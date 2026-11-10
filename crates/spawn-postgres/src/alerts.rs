//! Alerts: the loop's `alert.fired` and `alert.cleared` records, folded into
//! `spawn_alerts` (docs/fleet-architecture.md, "Alerts"; docs/storage.md,
//! "Alerts").
//!
//! The loop computes each transition from records and readings and records it
//! through the owner-only `spawn_alert_record`; the store refuses a second
//! `alert.fired` for a key, an `alert.cleared` without its `alert.fired`, and
//! a second `alert.cleared`. The reads below are the loop's way back after a
//! restart (its alerts and the facts it evaluates), and what
//! `spawn alerts --once`, `spawn alerts --follow` and `/api/alerts` show.

use serde_json::Value;
use spawn_core::port::StoreError;

use crate::PostgresStore;

/// The open and active missions' sources, for the `source_away` reading
/// (P12): `[{mission, sources}]`, by mission.
pub(crate) const MISSION_SOURCES: &str = "select coalesce(jsonb_agg(jsonb_build_object(
    'mission', id, 'sources', sources) order by id), '[]'::jsonb)
  from mission_missions where state in ('open', 'active')";

/// The event watermark and the database's clock, in one index-only read:
/// an idle pass costs the loop one statement (objective 023).
pub(crate) const CLOCK: &str = "select coalesce(max(id), 0), spawn_now_ms() from spawn_events";

/// The messages the `message_not_taken` alert reads (objective 045;
/// `spawn_command_message`), on the database's clock: `{now_ms, max_id,
/// messages}`, `max_id` the newest command read (or `$2`), `messages` each
/// read command that types a message, with its agent, session, action,
/// creation and `spawn_command_message`'s facts (`tracked`, `taken`,
/// `not_taken`, `due_ms`, `typed`, `ended`, `state`). `$1` the bound in
/// milliseconds (SPAWN_MESSAGE_TAKEN_SECS). This one reads what the step
/// needs when the log moved, a scan or two when nothing is new (objective
/// 023): the commands after `$2`; the pending messages `$3` that fell due;
/// and the open alerts' `$4` whose session recorded a `message.taken` or an
/// `execution.ended` at or after `$5` (what clears one).
pub(crate) const MESSAGES_SINCE: &str = r"
with candidates as (
  select k.* from spawn_commands k where k.id > $2
  union
  select k.* from spawn_commands k where cardinality($3::bigint[]) > 0 and k.id = any($3)
  union
  select k.* from spawn_commands k
   where cardinality($4::bigint[]) > 0 and k.id = any($4)
     and k.session_id in (select e.session_id from spawn_events e
                           where e.event_name in ('message.taken', 'execution.ended')
                             and e.recorded_at_ms >= $5))
select jsonb_build_object(
  'now_ms', spawn_now_ms(),
  'max_id', greatest($2, coalesce((select max(id) from candidates), $2)),
  'messages', coalesce((select jsonb_agg(x.row order by x.id) from (
      select k.id, jsonb_build_object(
               'command_id', k.id, 'agent_id', k.agent_id, 'session_id', k.session_id,
               'action', k.action ->> 'type', 'created_at_ms', k.created_at_ms) || m.message as row
        from candidates k
        cross join lateral (
          select spawn_command_message(k.id, k.session_id, k.action, k.state, k.succeeded,
                                       k.created_at_ms, k.finished_at_ms, $1,
                                       (select spawn_messages_marked_from()),
                                       spawn_now_ms()) as message) m
       where m.message is not null) x), '[]'::jsonb))";

/// [`MESSAGES_SINCE`] as the step's start reads them: every command that
/// can be a message not taken (each live session's commands from its
/// execution's own start on: an input typed into that execution came after
/// it, one before it was typed into an execution that ended or failed; and
/// every unfinished command), with those of `$2` (the open alerts'), and the
/// newest command id.
pub(crate) const MESSAGES_ALL: &str = r"
with live as (
  select l.session_id, l.from_command
    from spawn_session_state t
    cross join lateral (
      select s.session_id, (s.kind -> 'detail' ->> 'command_id')::bigint as from_command,
             s.execution_id
        from spawn_events s
       where s.session_id = t.session_id and s.event_name = 'execution.started'
       order by s.id desc limit 1) l
   where not exists (select 1 from spawn_events x
                      where x.session_id = l.session_id and x.event_name = 'execution.ended'
                        and x.source = 'bridge' and x.execution_id = l.execution_id)),
candidates as (
  select k.* from spawn_commands k join live l
      on k.session_id = l.session_id and k.id >= l.from_command
  union
  select k.* from spawn_commands k where k.state <> 'finished'
  union
  select k.* from spawn_commands k where k.id = any($2))
select jsonb_build_object(
  'now_ms', spawn_now_ms(),
  'max_id', coalesce((select max(id) from spawn_commands), 0),
  'messages', coalesce((select jsonb_agg(x.row order by x.id) from (
      select k.id, jsonb_build_object(
               'command_id', k.id, 'agent_id', k.agent_id, 'session_id', k.session_id,
               'action', k.action ->> 'type', 'created_at_ms', k.created_at_ms) || m.message as row
        from candidates k
        cross join lateral (
          select spawn_command_message(k.id, k.session_id, k.action, k.state, k.succeeded,
                                       k.created_at_ms, k.finished_at_ms, $1,
                                       (select spawn_messages_marked_from()),
                                       spawn_now_ms()) as message) m
       where m.message is not null) x), '[]'::jsonb))";

/// The database's clock alone, the one every bound is measured by.
pub(crate) const NOW: &str = "select spawn_now_ms()";

/// The missions and tasks the loop compares a record against, with what a
/// task's delivery bound needs (its `task.assigned` record, and whether its
/// agent holds a delivered or reported task).
pub(crate) const MIRROR: &str = r"
select jsonb_build_object(
  'now_ms', spawn_now_ms(),
  'missions', coalesce((select jsonb_agg(jsonb_build_object(
        'id', m.id, 'state', m.state, 'halted', m.halted, 'accept', m.accept,
        'gated', m.gate is not null, 'gate_rejections', m.gate_rejections,
        'gate_aborts', m.gate_aborts, 'gate_room_escalated', m.gate_room_escalated)
      order by m.id) from mission_missions m), '[]'::jsonb),
  'tasks', coalesce((select jsonb_agg(jsonb_build_object(
        'id', k.id, 'mission', k.mission_id, 'state', k.state, 'agent_id', k.agent_id,
        'session_id', k.session_id, 'objective', k.objective, 'revisions', k.revisions,
        'assigned_event', (select max(e.id) from spawn_events e
                            where e.session_id = 'session_control'
                              and e.event_name = 'control.recorded'
                              and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.assigned'
                              and e.kind -> 'detail' -> 'record' ->> 'id' = k.id::text),
        'assigned_at_ms', (select max(e.recorded_at_ms) from spawn_events e
                            where e.session_id = 'session_control'
                              and e.event_name = 'control.recorded'
                              and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.assigned'
                              and e.kind -> 'detail' -> 'record' ->> 'id' = k.id::text),
        'agent_free', not exists (select 1 from mission_tasks o
                                   where o.agent_id = k.agent_id
                                     and o.state in ('delivered', 'reported')))
      order by k.id) from mission_tasks k), '[]'::jsonb))";

/// Every alert row, and the newest transition event id: the loop reads its
/// alerts back after a restart, and fires nothing twice.
pub(crate) const ROWS: &str = r"
select jsonb_build_object(
  'cursor', coalesce((select greatest(coalesce(max(fired_event_id), 0),
                                      coalesce(max(cleared_event_id), 0))
                        from spawn_alerts), 0),
  'rows', coalesce((select jsonb_agg(jsonb_build_object(
        'key', a.key, 'kind', a.kind, 'subject', a.subject, 'state', a.state,
        'notice', a.notice, 'text', a.text, 'evidence', a.evidence,
        'fired_event_id', a.fired_event_id, 'fired_at_ms', a.fired_at_ms,
        'cleared_event_id', a.cleared_event_id, 'cleared_at_ms', a.cleared_at_ms,
        'cleared_evidence', a.cleared_evidence)
      order by a.fired_at_ms, a.key) from spawn_alerts a), '[]'::jsonb))";

/// The control records after `$1`, oldest first, at most `$2`: what the loop
/// has not looked at yet. `next` is the event id it continues from (the
/// batch's last one, or `$1` when there was none).
/// The batch is read once: `next` and `records` are that one read's last id
/// and rows, not two reads of the control stream (the loop reads it on every
/// pass it acts on).
pub(crate) const CONTROL: &str = r"
with batch as (
  select e.id, e.recorded_at_ms, e.kind -> 'detail' -> 'record' as record
    from spawn_events e
   where e.session_id = 'session_control' and e.event_name = 'control.recorded'
     and e.id > $1
   order by e.id limit $2)
select jsonb_build_object(
  'next', coalesce(max(b.id), $1),
  'records', coalesce(jsonb_agg(jsonb_build_object(
        'event_id', b.id, 'at_ms', b.recorded_at_ms, 'record', b.record)
      order by b.id), '[]'::jsonb))
from batch b";

/// The records the alert conditions read, newest last, since `$1`: the ones
/// objectives 028, 029, 031 and 033 define, and the halt and resume, so a
/// loop that restarted still knows what is pending.
pub(crate) const HISTORY: &str = r"
select coalesce(jsonb_agg(jsonb_build_object(
      'event_id', x.id, 'at_ms', x.recorded_at_ms, 'record', x.record)
    order by x.id), '[]'::jsonb)
from (select e.id, e.recorded_at_ms, e.kind -> 'detail' -> 'record' as record
        from spawn_events e
       where e.session_id = 'session_control' and e.event_name = 'control.recorded'
         and e.recorded_at_ms >= $1
         and e.kind -> 'detail' -> 'record' ->> 'type' in
             ('mission.halted', 'mission.resumed', 'admission.waiting', 'admission.admitted',
              'backup.dumped', 'backup.bundled', 'backup.verified',
              'execution.interrupted', 'execution.cleared', 'host.judge')
       order by e.id
       limit $2) x";

/// Every session's latest judgement, with its execution's end when the
/// session's current execution has ended (an end clears a stuck alert,
/// whatever its cause). The Observer keeps the latest judgement covering
/// the latest fact; the alert reads the same one.
///
/// The end is the session's latest `execution.started` or `execution.ended`
/// when that is an end, not an end after the latest judgement: the Observer
/// judges once more after every end (a rules judgement with no `stuck`), so
/// an end is never after the latest judgement of a session that ended. A
/// later `execution.started` (a resume) means the end is not current. Only
/// the Bridge's `execution.ended` is an end, as the store's own fold has it
/// (schema.sql: "a runner's exit report is no end"): a runner's late report
/// of an older execution, after the resume's start, would otherwise read a
/// live session as ended.
///
/// Two reads of the log whatever the number of sessions: the judgements,
/// and each session's latest start or end, joined once. (The live log's ten
/// judgements cost 21 scans a pass read as correlated subqueries, one per
/// judgement and one per end.)
pub(crate) const OBSERVATIONS: &str = r"
with latest as (
  select distinct on (e.session_id)
         e.session_id, e.agent_id, e.id, e.recorded_at_ms,
         e.kind -> 'detail' -> 'observation' as observation
    from spawn_events e
   where e.event_name = 'observation.recorded'
   order by e.session_id, e.id desc),
current as (
  select distinct on (e.session_id) e.session_id, e.id, e.event_name, e.kind
    from spawn_events e
    join latest l using (session_id)
   where (e.event_name = 'execution.started'
          or (e.event_name = 'execution.ended' and e.source = 'bridge'))
   order by e.session_id, e.id desc)
select coalesce(jsonb_agg(jsonb_build_object(
      'session_id', o.session_id, 'agent_id', o.agent_id, 'event_id', o.id,
      'at_ms', o.recorded_at_ms, 'observation', o.observation,
      'ended_event', o.ended_event, 'ended_cause', o.ended_cause)
    order by o.session_id), '[]'::jsonb)
from (select l.session_id, l.agent_id, l.id, l.recorded_at_ms, l.observation,
             case when x.event_name = 'execution.ended' then x.id end as ended_event,
             case when x.event_name = 'execution.ended'
                  then x.kind -> 'detail' ->> 'cause' end as ended_cause
        from latest l
        left join current x using (session_id)
       order by l.session_id) o";

/// Commands that finished failed after `$1`, oldest first: a notice each,
/// an owner's note excepted (the alert step skips those).
pub(crate) const FAILED_COMMANDS: &str = r"
select coalesce(jsonb_agg(jsonb_build_object(
      'command', c.id, 'agent_id', c.agent_id, 'session_id', c.session_id,
      'principal', c.principal, 'action', c.action ->> 'type',
      'key', c.idempotency_key, 'error', left(coalesce(c.error, ''), 300),
      'finished_at_ms', c.finished_at_ms)
    order by c.id), '[]'::jsonb)
from spawn_commands c
where c.succeeded = false and c.finished_at_ms > $1";

/// The alerts the windows show: every open one, and every one fired or
/// cleared since `$1` (a notice is news within the window only).
pub(crate) const ALERTS: &str = r"
select coalesce(jsonb_agg(jsonb_build_object(
      'key', a.key, 'kind', a.kind, 'subject', a.subject, 'state', a.state,
      'fired_event_id', a.fired_event_id, 'fired_at_ms', a.fired_at_ms,
      'evidence', a.evidence,
      'cleared_event_id', a.cleared_event_id, 'cleared_at_ms', a.cleared_at_ms,
      'cleared_evidence', a.cleared_evidence, 'text', a.text,
      'scored_at_ms', (select s.scored_at_ms from spawn_alert_scores s where s.key = a.key))
    order by a.fired_at_ms, a.key), '[]'::jsonb)
from spawn_alerts a
where a.state = 'open' or a.fired_at_ms >= $1
   or coalesce(a.cleared_at_ms, 0) >= $1";

/// The transitions recorded after event `$1`, oldest first, at most `$2`:
/// what `spawn alerts --follow` prints as they are recorded.
pub(crate) const TRANSITIONS: &str = r"
select coalesce(jsonb_agg(jsonb_build_object(
      'transition', t.transition, 'key', t.key, 'kind', t.kind, 'subject', t.subject,
      'event_id', t.event_id, 'at_ms', t.at_ms, 'evidence', t.evidence, 'notice', t.notice)
    order by t.event_id, t.transition), '[]'::jsonb)
from (select 'fired' as transition, a.key, a.kind, a.subject,
             a.fired_event_id as event_id, a.fired_at_ms as at_ms,
             a.evidence, a.notice
        from spawn_alerts a where a.fired_event_id > $1
      union all
      select 'cleared', a.key, a.kind, a.subject,
             a.cleared_event_id, a.cleared_at_ms,
             coalesce(a.cleared_evidence, '{}'::jsonb), a.notice
        from spawn_alerts a where a.cleared_event_id > $1) t
limit $2";

/// Whether an integrator runs, and the work that waits for one: `locked`,
/// some backend holds the integrator's advisory lock on this store
/// (`integrator_try_lock`: the two-key form, `$1` and the schema's
/// `hashtext`, in this database); `waiting`, every reported task of a gated
/// mission that is open or active, not halted, and whose source no owner
/// holds now (a deploy restarting the integrator holds it, and so does an
/// owner's push). pg_locks is no table: a reading with no waiting work
/// scans nothing but the tasks' index.
pub(crate) const INTEGRATOR: &str = r"
select jsonb_build_object(
  'now_ms', spawn_now_ms(),
  'locked', exists (select 1 from pg_locks l
                     where l.locktype = 'advisory' and l.granted
                       and l.database = (select d.oid from pg_database d
                                          where d.datname = current_database())
                       and l.classid = $1::int4::oid
                       and l.objid = hashtext(current_schema())::oid
                       and l.objsubid = 2),
  'waiting', coalesce((select jsonb_agg(jsonb_build_object('mission', m.id, 'task', k.id)
                                        order by k.id)
                         from mission_tasks k
                         join mission_missions m on m.id = k.mission_id
                        where k.state = 'reported' and m.gate is not null
                          and m.state in ('open', 'active') and m.halted is null
                          and not exists (select 1 from mission_source_holds h
                                           where h.source = m.sources ->> 0
                                             and h.expires_ms > spawn_now_ms())),
                      '[]'::jsonb))";

impl PostgresStore {
    /// Record an `alert.fired` or `alert.cleared` record (owner only).
    /// Returns the event's id; a second `fired` for a key, a `cleared`
    /// without its `fired` and a second `cleared` are refused.
    pub fn alert_record(&self, record: &Value) -> Result<i64, StoreError> {
        Ok(self
            .query_one("select spawn_alert_record($1)", &[record])?
            .get(0))
    }

    /// The newest event id and the database's clock, in one statement
    /// ([`CLOCK`]).
    pub fn alert_clock(&self) -> Result<(i64, i64), StoreError> {
        let row = self.query_one(CLOCK, &[])?;
        Ok((row.get(0), row.get(1)))
    }

    /// The database's clock in epoch ms ([`NOW`]).
    pub fn now_ms(&self) -> Result<i64, StoreError> {
        Ok(self.query_one(NOW, &[])?.get(0))
    }

    /// The open and active missions' sources ([`MISSION_SOURCES`]).
    pub fn alert_mission_sources(&self) -> Result<Value, StoreError> {
        self.query_json(MISSION_SOURCES, &[])
    }

    /// The missions and tasks the loop compares records against ([`MIRROR`]).
    pub fn alert_mirror(&self) -> Result<Value, StoreError> {
        self.query_json(MIRROR, &[])
    }

    /// Every alert row and the newest transition's event id ([`ROWS`]).
    pub fn alert_rows(&self) -> Result<Value, StoreError> {
        self.query_json(ROWS, &[])
    }

    /// The control records after `after` ([`CONTROL`]).
    pub fn alert_control(&self, after: i64, limit: i64) -> Result<Value, StoreError> {
        self.query_json(CONTROL, &[&after, &limit])
    }

    /// The alert-family records since `since_ms` ([`HISTORY`]).
    pub fn alert_history(&self, since_ms: i64, limit: i64) -> Result<Value, StoreError> {
        self.query_json(HISTORY, &[&since_ms, &limit])
    }

    /// Every session's latest judgement ([`OBSERVATIONS`]).
    pub fn alert_observations(&self) -> Result<Value, StoreError> {
        self.query_json(OBSERVATIONS, &[])
    }

    /// The messages the `message_not_taken` alert's start reads
    /// ([`MESSAGES_ALL`]): `bound_ms` is SPAWN_MESSAGE_TAKEN_SECS, `open` the
    /// commands of the open alerts.
    pub fn alert_messages_all(&self, bound_ms: i64, open: &[i64]) -> Result<Value, StoreError> {
        self.query_json(MESSAGES_ALL, &[&bound_ms, &open])
    }

    /// The messages of the commands after `after`, of the pending `due`
    /// ones, and of the `open` alerts' whose session took a message or
    /// ended an execution at or after `since_ms` ([`MESSAGES_SINCE`]).
    pub fn alert_messages_since(
        &self,
        bound_ms: i64,
        after: i64,
        due: &[i64],
        open: &[i64],
        since_ms: i64,
    ) -> Result<Value, StoreError> {
        self.query_json_prepared(MESSAGES_SINCE, &[&bound_ms, &after, &due, &open, &since_ms])
    }

    /// Commands that finished failed after `since_ms` ([`FAILED_COMMANDS`]).
    pub fn alert_failed_commands(&self, since_ms: i64) -> Result<Value, StoreError> {
        self.query_json(FAILED_COMMANDS, &[&since_ms])
    }

    /// Whether an integrator holds its lock, and the reported work that
    /// waits for one ([`INTEGRATOR`]).
    pub fn alert_integrator(&self) -> Result<Value, StoreError> {
        self.query_json(INTEGRATOR, &[&crate::missions::INTEGRATOR_LOCK_KEY])
    }

    /// The alerts the window shows ([`ALERTS`]).
    pub fn alerts(&self, since_ms: i64) -> Result<Value, StoreError> {
        self.query_json(ALERTS, &[&since_ms])
    }

    /// The transitions recorded after event `since` ([`TRANSITIONS`]).
    pub fn alert_transitions(&self, since: i64, limit: i64) -> Result<Value, StoreError> {
        self.query_json(TRANSITIONS, &[&since, &limit])
    }
}
