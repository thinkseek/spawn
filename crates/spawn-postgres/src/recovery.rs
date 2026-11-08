//! What the recovery reads: the facts one interruption is told from, and
//! what the loop already recorded and sent about it
//! (docs/fleet-architecture.md §3.1, "the recovery"; crate
//! `spawn-cli::recover`).
//!
//! Every cause is read from a recorded fact, never from a guess: the end's
//! exit status or signal ([`crate::PostgresStore::recovery_facts`]'s
//! `execution`), the transcript's API error line by its event id (`turns`),
//! the command that stopped it (`stops`), the free space a reading took, the
//! terminal server's state at recovery (the host's, not here) and the
//! interruptions and revivals already in the log (`records`, `commands`).
//! A restarted loop reads the same facts and reaches the same decisions.

use serde_json::Value;
use spawn_core::id::SessionId;
use spawn_core::port::StoreError;

use crate::PostgresStore;

/// How far back a session whose latest execution ended is still read: a
/// resume's backoff caps at 640 s, a wait can hold longer (a login renewed
/// by hand, a disk filled for an hour), and both keep their session in the
/// log (the open-interruption clause below). Half an hour of ends is the
/// fresh work; older waits are found by their own records.
const RECENT_MS: i64 = 1_800_000;

/// The sessions the recovery looks at: the supervisor, the residents and
/// the ephemeral workers that have an execution worth reading — one that is
/// live, one that ended recently, or one whose loop recorded an interruption
/// whose wait is still open (however old: a wait ends on an observation, not
/// on time). A session with nothing recent and nothing open costs nothing.
pub(crate) const RECOVERY_SESSIONS: &str = r"
select coalesce(jsonb_agg(jsonb_build_object(
         'session_id', s.session_id, 'agent_id', s.agent_id) order by s.session_id), '[]'::jsonb)
from agent_status s
where (s.agent_id like 'agent\_worker\_%' or s.agent_id like 'agent\_resident\_%'
       or s.session_id = 'session_supervisor')
  and (
    s.live
    or coalesce(s.ended_at_ms, s.started_at_ms) > spawn_now_ms() - $1
    or exists (select 1 from spawn_events r
                where r.session_id = 'session_control' and r.event_name = 'control.recorded'
                  and r.kind -> 'detail' -> 'record' ->> 'type' = 'execution.interrupted'
                  and r.kind -> 'detail' -> 'record' ->> 'session_id' = s.session_id
                  and r.kind -> 'detail' -> 'record' ->> 'execution_id' = s.execution_id
                  and r.kind -> 'detail' -> 'record' ->> 'path' = 'wait'
                  and not exists (select 1 from spawn_events c
                                   where c.session_id = 'session_control'
                                     and c.event_name = 'control.recorded'
                                     and c.id > r.id
                                     and c.kind -> 'detail' -> 'record' ->> 'type' = 'execution.cleared'
                                     and c.kind -> 'detail' -> 'record' ->> 'execution_id' = s.execution_id))
  )";

/// Whether the log holds, above `$1` (an event id), a record the recovery
/// acts on by itself: a failed turn, an execution's end, one of its own
/// interruption or clear records, a task's over-budget Stop, or a loop's
/// start. Progress records (a tool call, a turn's end, a commit, a native
/// session's start) do not move it: a live execution needs no read, and a
/// wait is re-read every probe anyway. The loop asks this once before
/// reading every session's facts, so a pass whose watermark moved on some
/// other event (a screen, a delivery, a turn's progress) costs one index
/// range read and nothing more.
pub(crate) const RECOVERY_SIGNALS: &str = r"
select exists (
  select 1 from spawn_events e
   where e.id > $1
     and (e.event_name in ('turn.failed', 'tool.failed', 'execution.ended')
          or (e.event_name = 'control.recorded'
              and e.kind -> 'detail' -> 'record' ->> 'type' in
                  ('execution.interrupted', 'execution.cleared', 'task.over_budget',
                   'host.loop_started'))))";

/// One session's facts, as `recovery_facts` returns them. `$1` session.
///
/// - `execution`: the session's latest execution — its id, start, end, the
///   end's `cause`, `exit_code`, `signal` and `evidence`, its workspace —
///   or null for a session that never ran.
/// - `native_session_id`: the native session its latest
///   `session.native_started` names, which a resume resumes.
/// - `turns`: every `turn.failed` of the execution, oldest first, with the
///   engine's `status`, `error` tag (`kind`) and `text`, by event id.
/// - `failed_tools`: every `tool.failed` of the execution, oldest first,
///   with the engine's error `text`, by event id: a full disk is told here
///   (Claude Code's ENOSPC message) and nowhere else while the engine
///   waits at its prompt.
/// - `progress`: the execution's latest `tool.started`, `tool.completed` or
///   `turn.stopped` — the event id and its time; `0` when it made no
///   progress at all. A model answer after a failed turn is a progress event
///   newer than it, and a resume whose execution has none before it ended
///   died before it made any.
/// - `stops`: every Stop or Discard sent into the session since the
///   execution started, whoever sent it.
/// - `records`: the recovery's own records of this execution (interruptions
///   and clears), oldest first, each with its event id, its time and the
///   record.
/// - `commands`: every `recover-` command into the session, oldest first,
///   with its outcome: what a restarted loop reads to know what it sent.
/// - `executions`: every execution of the session, oldest first — when it
///   ran, how long, the key of the command that started it, and whether it
///   made progress (a tool call completed or a turn ended). The resume rule
///   counts the ones a revival started that died young ([`crate
///   `spawn-cli::recover`'s `resume_failed`]).
/// - `head`: the commit of the agent's latest `artifact.committed`, which a
///   handoff names as the branch's head.
pub(crate) const RECOVERY_FACTS: &str = r"
with x as (
  select * from agent_executions e
   where e.session_id = $1
   order by e.started_at_ms desc, e.execution_id desc limit 1
), turns as (
  select coalesce(jsonb_agg(jsonb_build_object(
            'event_id', e.id,
            'at_ms', e.recorded_at_ms,
            'status', (e.kind -> 'detail' ->> 'status')::bigint,
            'kind', e.kind -> 'detail' ->> 'error_kind',
            'text', e.kind -> 'detail' ->> 'error') order by e.id), '[]'::jsonb) as list
    from spawn_events e, x
   where e.session_id = $1 and e.execution_id = x.execution_id
     and e.event_name = 'turn.failed'
), tools as (
  select coalesce(jsonb_agg(jsonb_build_object(
            'event_id', e.id,
            'at_ms', e.recorded_at_ms,
            'text', e.kind -> 'detail' ->> 'error') order by e.id), '[]'::jsonb) as list
    from spawn_events e, x
   where e.session_id = $1 and e.execution_id = x.execution_id
     and e.event_name = 'tool.failed'
), progress as (
  select coalesce(max(e.id), 0) as last_id, coalesce(max(e.recorded_at_ms), 0) as last_at
    from spawn_events e, x
   where e.session_id = $1 and e.execution_id = x.execution_id
     and e.event_name in ('tool.started', 'tool.completed', 'turn.stopped')
), stops as (
  select coalesce(jsonb_agg(jsonb_build_object(
            'command_id', c.id,
            'principal', c.principal,
            'key', c.idempotency_key,
            'action', c.action ->> 'type',
            'created_at_ms', c.created_at_ms,
            'succeeded', c.succeeded,
            'error', c.error) order by c.id), '[]'::jsonb) as list
    from spawn_commands c, x
   where c.session_id = $1 and c.action ->> 'type' in ('stop', 'discard')
     and c.created_at_ms >= x.started_at_ms
), records as (
  select coalesce(jsonb_agg(jsonb_build_object(
            'event_id', e.id,
            'at_ms', e.recorded_at_ms,
            'record', e.kind -> 'detail' -> 'record') order by e.id), '[]'::jsonb) as list
    from spawn_events e, x
   where e.session_id = 'session_control' and e.event_name = 'control.recorded'
     and e.kind -> 'detail' -> 'record' ->> 'type'
         in ('execution.interrupted', 'execution.cleared')
     and e.kind -> 'detail' -> 'record' ->> 'execution_id' = x.execution_id
), commands as (
  select coalesce(jsonb_agg(jsonb_build_object(
            'id', c.id,
            'key', c.idempotency_key,
            'type', c.action ->> 'type',
            'created_at_ms', c.created_at_ms,
            'finished_at_ms', c.finished_at_ms,
            'succeeded', c.succeeded,
            'error', c.error) order by c.id), '[]'::jsonb) as list
    from spawn_commands c, x
   where c.session_id = $1 and starts_with(c.idempotency_key, 'recover-')
), ex as (
  select e.execution_id, e.started_at_ms, e.ended_at_ms, e.duration_ms, e.command_id,
         coalesce(c.idempotency_key, '') as key
    from agent_executions e left join spawn_commands c on c.id = e.command_id
   where e.session_id = $1
), executions as (
  select coalesce(jsonb_agg(jsonb_build_object(
            'id', e.execution_id,
            'started_at_ms', e.started_at_ms,
            'ended_at_ms', e.ended_at_ms,
            'duration_ms', e.duration_ms,
            'key', e.key,
            'progress', exists (select 1 from spawn_events p
                                 where p.session_id = $1
                                   and p.execution_id = e.execution_id
                                   and p.event_name in ('tool.completed', 'turn.stopped')))
            order by e.started_at_ms, e.execution_id), '[]'::jsonb) as list
    from ex e
)
select jsonb_build_object(
  'session_id', $1,
  'agent_id', (select agent_id from x),
  'execution', (select jsonb_build_object(
                  'id', x.execution_id,
                  'started_at_ms', x.started_at_ms,
                  'ended_at_ms', x.ended_at_ms,
                  'cause', x.cause,
                  'exit_code', x.exit_code,
                  'signal', x.signal,
                  'evidence', x.evidence,
                  'workspace', x.workspace,
                  'command_id', x.command_id,
                  'end_event_id', (select e.id from spawn_events e
                                    where e.session_id = $1
                                      and e.execution_id = x.execution_id
                                      and e.event_name = 'execution.ended'
                                      and e.source = 'bridge'
                                    order by e.id limit 1)) from x),
  'native_session_id', (select e.kind -> 'detail' ->> 'native_session_id'
                          from spawn_events e
                         where e.session_id = $1 and e.event_name = 'session.native_started'
                         order by e.id desc limit 1),
  'turns', (select list from turns),
  'failed_tools', (select list from tools),
  'progress', (select jsonb_build_object('event_id', last_id, 'at_ms', last_at) from progress),
  'stops', (select list from stops),
  'records', (select list from records),
  'commands', (select list from commands),
  'executions', (select list from executions),
  'over_budget', (select jsonb_build_object(
                     'event_id', e.id,
                     'at_ms', e.recorded_at_ms,
                     'record', e.kind -> 'detail' -> 'record')
                    from spawn_events e
                    join mission_tasks t
                      on t.id = (e.kind -> 'detail' -> 'record' ->> 'id')::bigint
                   where e.session_id = 'session_control'
                     and e.event_name = 'control.recorded'
                     and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.over_budget'
                     and t.session_id = $1
                   order by e.id desc limit 1),
  'head', (select e.kind -> 'detail' -> 'artifact' ->> 'commit'
             from spawn_events e
            where e.agent_id = (select agent_id from x)
              and e.event_name = 'artifact.committed'
            order by e.id desc limit 1)
)";

/// The latest `host.loop_started` record, with its `event_id` and `at_ms`:
/// which loop is running, and the boot before it (`RECOVERY_SESSIONS`'
/// `host_restart`). Null when no loop ever recorded one.
pub(crate) const LOOP_STARTED: &str = "
select e.kind -> 'detail' -> 'record'
       || jsonb_build_object('event_id', e.id, 'at_ms', e.recorded_at_ms)
  from spawn_events e
 where e.session_id = 'session_control' and e.event_name = 'control.recorded'
   and e.kind -> 'detail' -> 'record' ->> 'type' = 'host.loop_started'
 order by e.id desc limit 1";

/// Whether the recovery recorded an interruption of the session's latest
/// execution: from then on it, not the supervisor's liveness rule or the
/// dispatcher's one-resume rule, is what brings the agent back
/// (crate `spawn-cli`'s `recover`). `$1` session.
pub(crate) const RECOVERED: &str = "
select exists (
  select 1
    from spawn_events r
    join (select x.execution_id from agent_executions x
           where x.session_id = $1
           order by x.started_at_ms desc, x.execution_id desc limit 1) last
      on last.execution_id = r.kind -> 'detail' -> 'record' ->> 'execution_id'
   where r.session_id = 'session_control' and r.event_name = 'control.recorded'
     and r.kind -> 'detail' -> 'record' ->> 'type' = 'execution.interrupted'
     and r.kind -> 'detail' -> 'record' ->> 'session_id' = $1)";

impl PostgresStore {
    /// The sessions the recovery looks at ([`RECOVERY_SESSIONS`]).
    pub fn recovery_sessions(&self) -> Result<Value, StoreError> {
        self.query_json(RECOVERY_SESSIONS, &[&RECENT_MS])
    }

    /// Whether an event above `after` is one the recovery reads
    /// ([`RECOVERY_SIGNALS`]).
    pub fn recovery_signals(&self, after: i64) -> Result<bool, StoreError> {
        Ok(self.query_one(RECOVERY_SIGNALS, &[&after])?.get(0))
    }

    /// One session's recovery facts ([`RECOVERY_FACTS`]).
    pub fn recovery_facts(&self, session: &SessionId) -> Result<Value, StoreError> {
        self.query_json(RECOVERY_FACTS, &[&session.as_str()])
    }

    /// The latest `host.loop_started` record ([`LOOP_STARTED`]).
    pub fn loop_started(&self) -> Result<Option<Value>, StoreError> {
        self.query_opt_json(LOOP_STARTED, &[])
    }

    /// Whether the recovery recorded an interruption of the session's latest
    /// execution ([`RECOVERED`]).
    pub fn recovered(&self, session: &SessionId) -> Result<bool, StoreError> {
        Ok(self.query_one(RECOVERED, &[&session.as_str()])?.get(0))
    }
}
