//! Read model of the fleet for the dashboard.
//!
//! Every figure is computed by SQL from the log, the projection and the
//! command queue (the SQL below), so the page shows storage
//! and nothing a process remembers.

use serde_json::Value;
use spawn_core::port::StoreError;

use crate::PostgresStore;

/// One row per session with its judgement, its task and what it is doing.
pub(crate) const FLEET_BOARD: &str = r"
select coalesce(jsonb_agg(row_to_json(a) order by a.role_rank, a.session_id), '[]'::jsonb)
from (
  select
    s.session_id,
    s.agent_id,
    case when s.agent_id = 'agent_supervisor' then 'supervisor'
         when s.agent_id = 'agent_rsi' then 'rsi'
         else 'worker' end as role,
    case when s.agent_id = 'agent_supervisor' then 0
         when s.agent_id = 'agent_rsi' then 2 else 1 end as role_rank,
    s.engine,
    s.live,
    s.execution_id,
    s.started_at_ms,
    s.ended_at_ms,
    coalesce(s.state, 'UNKNOWN') as state,
    p.observation as judgement,
    p.observation_at_ms as judged_at_ms,
    coalesce((p.observation ->> 'attention')::boolean, false) as attention,
    -- Since the judgement after the latest one of another state. That one
    -- is found newest first (spawn_events_observation_time), so a session
    -- reads its current run of judgements, not every judgement it has.
    (select min(j.recorded_at_ms) from agent_judgements j
      where j.session_id = s.session_id
        and j.recorded_at_ms > coalesce((
              select k.recorded_at_ms from agent_judgements k
               where k.session_id = s.session_id and k.state is distinct from s.state
               order by k.recorded_at_ms desc limit 1), 0)
    ) as state_since_ms,
    case when coalesce((p.observation ->> 'attention')::boolean, false) then
      (select min(j.recorded_at_ms) from agent_judgements j
        where j.session_id = s.session_id
          and j.recorded_at_ms > coalesce((
                select k.recorded_at_ms from agent_judgements k
                 where k.session_id = s.session_id
                   and not coalesce((k.observation ->> 'attention')::boolean, false)
                 order by k.recorded_at_ms desc limit 1), 0))
    end as attention_since_ms,
    -- The assignment: the latest text a person or the supervisor gave, not
    -- a dispatcher digest or a liveness restart.
    (select left(coalesce(c.action ->> 'prompt', c.action ->> 'text'), 600)
       from spawn_commands c
      where c.session_id = s.session_id
        and c.principal not in ('spawn-dispatch', 'spawn-liveness')
        and coalesce(c.action ->> 'prompt', c.action ->> 'text') is not null
      order by c.id desc limit 1) as task,
    jsonb_build_object(
      'current_tool', (select o.tool_name from agent_open_tools o
                        where o.session_id = s.session_id
                          and o.execution_id = s.execution_id and s.live
                        order by o.session_seq desc limit 1),
      -- The live execution's open background jobs, oldest first, with their
      -- age on the database's clock.
      'jobs', (select coalesce(jsonb_agg(jsonb_build_object(
                 'id', j.job_id, 'kind', j.job_kind, 'started_at_ms', j.started_at_ms,
                 'age_ms', spawn_now_ms() - j.started_at_ms) order by j.session_seq), '[]'::jsonb)
                 from agent_open_jobs j
                where j.session_id = s.session_id
                  and j.execution_id = s.execution_id and s.live),
      'last_event_name', (select e.event_name from spawn_events e
                           where e.session_id = s.session_id and e.source <> 'observer'
                           order by e.session_seq desc limit 1),
      'last_event_at_ms', s.last_event_at_ms,
      'event_count', p.event_count,
      'tool_calls', (select count(*) from spawn_events e
                      where e.session_id = s.session_id and e.event_name = 'tool.started'),
      'executions', (select count(*) from agent_executions x where x.session_id = s.session_id)
    ) as activity
  from agent_status s
  join spawn_session_state p using (session_id)
) a";

/// How the fleet and the decision ladder did over the last hour and day,
/// and each gated mission's gate-room waits. Who needs a human is not
/// here: that is the dispatcher's own view (`GET /api/attention`), and a
/// judge's `attention` flag is only its opinion, on each judgement.
pub(crate) const FLEET_HEALTH: &str = r"
with
  now as (select spawn_now_ms() as ms),
  -- The day's judgements carry only their judge (the ladder counts them);
  -- the hour's carry the whole judgement. Both read their window by time
  -- (spawn_events_name_time).
  judged as (
    select j.observation ->> 'judge' as judge
      from agent_judgements j, now
     where j.recorded_at_ms > now.ms - 86400000),
  hour as (
    select j.observation
      from agent_judgements j, now
     where j.recorded_at_ms > now.ms - 3600000),
  commands as (
    select c.principal, c.created_at_ms from spawn_commands c, now
     where c.created_at_ms > now.ms - 86400000),
  live_ms as (
    select coalesce(sum(greatest(0,
             least(coalesce(x.ended_at_ms, now.ms), now.ms)
             - greatest(x.started_at_ms, now.ms - 86400000))), 0)::float8 as ms
      from agent_executions x, now
     where coalesce(x.ended_at_ms, now.ms) > now.ms - 86400000),
  events as (
    select e.event_name from spawn_events e, now
     where e.event_name in ('tool.started', 'tool.failed', 'turn.failed')
       and e.recorded_at_ms > now.ms - 3600000)
select jsonb_build_object(
  'generated_at_ms', (select ms from now),
  'agents_total', (select count(*) from agent_status),
  'live', (select count(*) from agent_status where live),
  'by_state', (select coalesce(jsonb_object_agg(state, n), '{}'::jsonb)
                 from (select coalesce(state, 'UNKNOWN') as state, count(*) as n
                         from agent_status group by 1) t),
  -- The run of `no_room` gate outcomes of each gated mission (one task at
  -- a time, as the store folds them): open or active missions, and any
  -- whose run goes on.
  'gate_room', (select coalesce(jsonb_agg(jsonb_build_object(
                   'mission', m.id, 'text', left(m.text, 200), 'task', m.gate_room_task,
                   'since_ms', m.gate_room_since_ms, 'last_ms', m.gate_room_last_ms,
                   'waits', m.gate_room_waits, 'escalated', m.gate_room_escalated)
                   order by m.id), '[]'::jsonb)
                  from mission_missions m
                 where m.gate is not null
                   and (m.state in ('open', 'active') or m.gate_room_task is not null)),
  'last_hour', jsonb_build_object(
    'judgements', (select count(*) from hour),
    'by_judge', (select coalesce(jsonb_object_agg(judge, n), '{}'::jsonb)
                   from (select observation ->> 'judge' as judge, count(*) as n
                           from hour group by 1) t),
    'judge_unavailable', (select count(*) from hour
                           where observation ->> 'reason' = 'JUDGE_UNAVAILABLE'),
    'model_latency_p50_ms', (select percentile_cont(0.5) within group
                               (order by (observation ->> 'latency_ms')::float8)
                               from hour where observation ? 'latency_ms'),
    'disagreements', (select count(*) from hour
                       where observation ? 'rules_state'
                         and observation ->> 'rules_state' is distinct from observation ->> 'state'),
    'tool_calls', (select count(*) from events where event_name = 'tool.started'),
    'failures', (select count(*) from events where event_name in ('tool.failed', 'turn.failed'))
  ),
  'ladder_24h', jsonb_build_object(
    'rules', (select count(*) from judged where judge = 'rules'),
    'model', (select count(*) from judged where judge <> 'rules'),
    'dispatcher', (select count(*) from commands where principal = 'spawn-dispatch'),
    'supervisor', (select count(*) from commands where principal = 'spawn_supervisor'),
    'liveness', (select count(*) from commands where principal = 'spawn-liveness'),
    'human', (select count(*) from commands where principal = 'human')
  ),
  'agent_hours_24h', (select ms / 3600000.0 from live_ms),
  'human_per_agent_hour', (select case when ms > 0
                              then (select count(*) from commands where principal = 'human')
                                   / (ms / 3600000.0) end from live_ms)
)";

/// What the dispatcher decides from: every worker session and the
/// supervisor, with the latest judgement, the episode (latest command from
/// someone other than the dispatcher, or a task it delivered), whether a command is in flight, the
/// model profile of its latest execution, what the dispatcher already sent
/// there in the last day, and however old what a decision still rests on:
/// its deliveries (a delivery is confirmed from its command), the nudges of
/// the current episode and the resume of the current execution (an old one
/// is not "not sent"); the tags of every input it delivered there
/// (`told`: the escalations its digests carried, however long ago);
/// `resumed_young` counts the executions its
/// resumes started that died young (`died_young!`) since the session's last
/// execution that did not; `failed_resume` is the latest execution one of
/// its resumes (`dispatch-resume-%`, whatever its command's outcome) started
/// that ended lost before its engine became ready (`lost_before_ready!`),
/// since the session's last execution that stayed up (neither died young
/// nor ended lost before ready), with its end's `evidence` and its
/// command's `error`, or `null`: what the log records about the session,
/// not the key of its latest execution, which a resume that started an
/// execution and failed never sends (docs/fleet-architecture.md §3, the
/// one-resume rule); `stops` are the Stops and Discards anyone sent
/// during the live execution (after the command that started it, so the
/// Bridge ran them on it), with their outcomes: a failed one is pursued.
/// `turn_ended`: the live execution's latest turn event (`message.received`,
/// `turn.stopped`, `turn.failed`) is a turn's end; the Bridge records a
/// turn's checkpoint before its end, so `checkpoint`, the agent's latest
/// `artifact.committed`, read in the same statement, is then that turn's
/// (or a later one). `jobs`: the live execution's open background jobs
/// (`agent_open_jobs`), oldest first, each with the id of its
/// `job.started` and its age on the database's clock.
///
/// Four of those — `evidence_seq`, `turn_ended`, `checkpoint`, `model` — are
/// one row per session or per agent over the log, so they are read once for
/// the whole fleet and joined, not once per session: the dispatcher reads
/// these facts on every pass it acts on, and four scans of the log do the
/// work of four per session (objective 023).
///
/// A delivery counts as the worker's (its `last_delivery`, and the episode)
/// only when it reached the worker or may have: it succeeded, has not
/// finished, or failed after typing may have begun (its error names `$1`,
/// [`spawn_core::bridge::MAY_HAVE_TYPED`]). A delivery refused before
/// anything was typed (a pane not ready, no execution, a source that did not
/// answer) told the worker nothing: the worker's last delivery is still the
/// one before it, and its own retry is not judged against itself.
///
/// `charter`, for the supervisor only (null for every other session), is
/// what decides which charter its running native session holds: `start`,
/// the command of the execution that first named its current native session
/// (the latest `session.native_started`'s id); `given`, that start's
/// `charter` (null for a start that recorded none); `inputs`, every input
/// the dispatcher sent into the session after that start that files a
/// `charter <hash>` tag, oldest first, with its outcome, however long ago.
pub(crate) const DISPATCH_FACTS: &str = concat!(
    r"
select coalesce(jsonb_agg(row_to_json(w) order by w.session_id), '[]'::jsonb)
from (
  select
    s.session_id,
    s.agent_id,
    s.live,
    s.execution_id,
    (select x.cause from agent_executions x where x.session_id = s.session_id
      order by x.started_at_ms desc, x.execution_id desc limit 1) as end_cause,
    p.observation,
    p.observation_event_id,
    p.observation_at_ms,
    ev.evidence_seq,
    coalesce(tn.ended, false) as turn_ended,
    ck.commit as checkpoint,
    coalesce(jb.jobs, '[]'::jsonb) as jobs,
    ep.id as episode,
    ep.action ->> 'type' as episode_action,
    oc.session_id is not null as open_command,
    coalesce(mo.model, 'default') as model,
    (select coalesce(jsonb_agg(jsonb_build_object(
              'id', c.id,
              'key', c.idempotency_key,
              'text', c.action ->> 'text',
              'created_at_ms', c.created_at_ms,
              'finished_at_ms', c.finished_at_ms,
              'succeeded', c.succeeded,
              'error', c.error) order by c.id), '[]'::jsonb)
       from spawn_commands c
      where c.session_id = s.session_id and c.principal = 'spawn-dispatch'
        and (c.created_at_ms > spawn_now_ms() - 86400000
             or c.idempotency_key like 'deliver-%'
             or c.idempotency_key = 'dispatch-resume-' || coalesce(s.execution_id, 'none')
             or c.idempotency_key = 'dispatch-nudge-' || s.session_id || '-' || coalesce(ep.id, 0)
             or starts_with(c.idempotency_key,
                            'dispatch-nudge-' || s.session_id || '-' || coalesce(ep.id, 0)
                            || '-retry-'))) as dispatched,
    (select coalesce(jsonb_agg(jsonb_build_object(
              'id', c.id,
              'key', c.idempotency_key,
              'principal', c.principal,
              'action', c.action ->> 'type',
              'created_at_ms', c.created_at_ms,
              'finished_at_ms', c.finished_at_ms,
              'succeeded', c.succeeded,
              'error', c.error) order by c.id), '[]'::jsonb)
       from spawn_commands c
      where s.live and c.session_id = s.session_id
        and c.action ->> 'type' in ('stop', 'discard')
        and c.id > (select x.command_id from agent_executions x
                     where x.session_id = s.session_id
                       and x.execution_id = s.execution_id)) as stops,
    -- Only the supervisor's told set is read (the tags its digests filed):
    -- every other session's is not gathered.
    case when s.session_id = 'session_supervisor' then
      (select coalesce(jsonb_agg(distinct tag), '[]'::jsonb)
         from spawn_commands c, jsonb_array_elements_text(c.action -> 'tags') tag
        where c.session_id = s.session_id and c.principal = 'spawn-dispatch'
          and c.succeeded and jsonb_typeof(c.action -> 'tags') = 'array')
    else '[]'::jsonb end as told,
    (select c.idempotency_key from spawn_commands c
      where c.session_id = s.session_id and c.principal = 'spawn-dispatch'
        and c.idempotency_key like 'deliver-%'
        and (c.succeeded is distinct from false or position($1 in coalesce(c.error, '')) > 0)
      order by c.id desc limit 1) as last_delivery,
    (select count(*) from agent_executions x
       join spawn_commands c on c.id = x.command_id
      where x.session_id = s.session_id and c.principal = 'spawn-dispatch'
        and c.idempotency_key like 'dispatch-resume-%' and c.succeeded
        and x.started_at_ms > (select coalesce(max(h.started_at_ms), 0)
                                 from agent_executions h
                                where h.session_id = s.session_id and not ",
    died_young!("h"),
    r")
        and ",
    died_young!("x"),
    r") as resumed_young,
    case when s.session_id = 'session_supervisor' then jsonb_build_object(
      'start', born.command_id,
      'given', born.charter,
      'inputs', (select coalesce(jsonb_agg(jsonb_build_object(
                     'id', c.id,
                     'key', c.idempotency_key,
                     'hash', t.hash,
                     'finished_at_ms', c.finished_at_ms,
                     'succeeded', c.succeeded,
                     'error', c.error) order by c.id), '[]'::jsonb)
                   from spawn_commands c
                   cross join lateral (
                     select min(substr(tag, length('charter ') + 1)) as hash
                       from jsonb_array_elements_text(
                              case when jsonb_typeof(c.action -> 'tags') = 'array'
                                   then c.action -> 'tags' else '[]'::jsonb end) tag
                      where starts_with(tag, 'charter ')) t
                  where c.session_id = s.session_id and c.principal = 'spawn-dispatch'
                    and c.action ->> 'type' = 'input' and t.hash is not null
                    and c.id > coalesce(born.command_id, 0)))
    end as charter,
    (select jsonb_build_object(
              'execution_id', x.execution_id,
              'command_id', c.id,
              'ended_at_ms', x.ended_at_ms,
              'evidence', (select e.kind -> 'detail' ->> 'evidence' from spawn_events e
                            where e.session_id = x.session_id
                              and e.execution_id = x.execution_id
                              and e.event_name = 'execution.ended' and e.source = 'bridge'
                            order by e.id limit 1),
              'error', c.error)
       from agent_executions x
       join spawn_commands c on c.id = x.command_id
      where x.session_id = s.session_id and c.principal = 'spawn-dispatch'
        and c.idempotency_key like 'dispatch-resume-%'
        and ",
    lost_before_ready!("x"),
    r"
        and x.started_at_ms > (select coalesce(max(h.started_at_ms), 0)
                                 from agent_executions h
                                where h.session_id = s.session_id and not ",
    died_young!("h"),
    r" and not ",
    lost_before_ready!("h"),
    r")
      order by x.started_at_ms desc, x.execution_id desc limit 1) as failed_resume
  from agent_status s
  join spawn_session_state p using (session_id)
  -- Four of the facts are one row per session or per agent over the log,
  -- read once for the whole fleet rather than once per session: the
  -- session's latest evidence, the latest event of its running execution
  -- (whether its turn ended), its agent's newest checkpoint, and the model
  -- its latest execution started with. Each is the same row the correlated
  -- read returned, and the loop reads these facts on every pass it acts on
  -- (objective 023: four reads of the log, not four per session).
  left join (
    select e.session_id, max(e.session_seq) as evidence_seq
      from spawn_events e
     where e.source <> 'observer'
       and e.event_name not in ('artifact.committed', 'merge.conflicted')
       and coalesce(e.kind -> 'detail' ->> 'step', '') <> 'waiting'
     group by e.session_id) ev on ev.session_id = s.session_id
  left join (
    select distinct on (e.session_id, e.execution_id)
           e.session_id, e.execution_id,
           e.event_name in ('turn.stopped', 'turn.failed') as ended
      from spawn_events e
     where e.event_name in ('message.received', 'turn.stopped', 'turn.failed')
     order by e.session_id, e.execution_id, e.session_seq desc) tn
    on tn.session_id = s.session_id and tn.execution_id = s.execution_id
  left join (
    select distinct on (e.agent_id) e.agent_id,
           e.kind -> 'detail' -> 'artifact' ->> 'commit' as commit
      from spawn_events e
     where e.event_name = 'artifact.committed'
     order by e.agent_id, e.id desc) ck on ck.agent_id = s.agent_id
  left join (
    select distinct on (e.session_id) e.session_id,
           e.kind -> 'detail' ->> 'model' as model
      from spawn_events e
     where e.event_name = 'execution.started'
     order by e.session_id, e.session_seq desc) mo on mo.session_id = s.session_id
  -- The live sessions' open background jobs, grouped once: the view is two
  -- reads of the log per job start, so it is evaluated for the fleet, not
  -- for each session.
  left join (
    select j.session_id, j.execution_id,
           jsonb_agg(jsonb_build_object(
             'id', j.job_id, 'kind', j.job_kind, 'event_id', j.event_id,
             'started_at_ms', j.started_at_ms,
             'age_ms', spawn_now_ms() - j.started_at_ms) order by j.session_seq) as jobs
      from agent_open_jobs j
     group by j.session_id, j.execution_id) jb
    on s.live and jb.session_id = s.session_id and jb.execution_id = s.execution_id
  -- The sessions with a command still in the queue, one read of the queue.
  left join (
    select distinct c.session_id from spawn_commands c where c.state <> 'finished') oc
    on oc.session_id = s.session_id
  -- The supervisor's current native session (the one its latest
  -- `session.native_started` names, which a resume resumes) and the start
  -- that began it: the command of the execution that named it first.
  left join lateral (select c.id as command_id, c.action ->> 'charter' as charter
     from spawn_events n
     join spawn_events x on x.session_id = n.session_id and x.event_name = 'execution.started'
                        and x.execution_id = n.execution_id
     join spawn_commands c on c.id = (x.kind -> 'detail' ->> 'command_id')::bigint
    where s.session_id = 'session_supervisor'
      and n.session_id = s.session_id and n.event_name = 'session.native_started'
      and n.kind -> 'detail' ->> 'native_session_id' =
          (select l.kind -> 'detail' ->> 'native_session_id' from spawn_events l
            where l.session_id = s.session_id and l.event_name = 'session.native_started'
            order by l.id desc limit 1)
    order by n.id limit 1) born on true
  left join lateral (select c.id, c.action from spawn_commands c
     where c.session_id = s.session_id
       and (c.principal <> 'spawn-dispatch'
            or (c.idempotency_key like 'deliver-%'
                and (c.succeeded is distinct from false
                     or position($1 in coalesce(c.error, '')) > 0)))
     order by c.id desc limit 1) ep on true
  where s.agent_id like 'agent\_worker\_%' or s.agent_id like 'agent\_resident\_%'
     or s.session_id = 'session_supervisor'
) w"
);

/// One session's judgements, newest first, with the evidence each saw.
/// `$1` session, `$2` limit.
pub(crate) const SESSION_JUDGEMENTS: &str =
    "select coalesce(jsonb_agg(row_to_json(j) order by j.session_seq desc), '[]'::jsonb)
                   from (select session_seq, recorded_at_ms, observation
                           from agent_judgements where session_id = $1
                          order by session_seq desc limit $2) j";

/// Commands into one session, newest first, with who sent them, and
/// `waiting`: whether the Bridge recorded the command's `command.step`
/// `waiting` (keyed `cmd_<id>:waiting`): an unfinished input that waits for
/// the session's turn, or a finished one that waited before it was typed.
/// `$1` session, `$2` limit.
pub(crate) const SESSION_COMMANDS: &str =
    "select coalesce(jsonb_agg(row_to_json(c) order by c.command_id desc), '[]'::jsonb)
                   from (select k.id as command_id, k.principal, k.action ->> 'type' as action,
                                left(coalesce(k.action ->> 'prompt', k.action ->> 'text'), 600) as text,
                                k.state, k.succeeded, k.error, k.created_at_ms, k.finished_at_ms,
                                exists (select 1 from spawn_events w
                                         where w.session_id = k.session_id and w.source = 'bridge'
                                           and w.source_event_id = 'cmd_' || k.id || ':waiting')
                                  as waiting
                           from spawn_commands k where k.session_id = $1
                          order by k.id desc limit $2) c";

/// The files main's merges into a session's branch changed that no
/// delivery has told the worker of yet: those of every `base_merged`
/// `artifact.committed` recorded after the latest `typed` step of a
/// dispatcher delivery (`deliver-%`) in the session. A delivery refused
/// before typing told nothing, so its retry still tells them. `$1` session.
pub(crate) const UNTOLD_MERGES: &str = "
select coalesce(jsonb_agg(m.file order by m.file), '[]'::jsonb)
  from (select distinct f.file
          from spawn_events e,
               jsonb_array_elements_text(coalesce(e.kind -> 'detail' -> 'files', '[]'::jsonb)) f(file)
         where e.session_id = $1 and e.event_name = 'artifact.committed'
           and e.kind -> 'detail' ->> 'reason' = 'base_merged'
           and e.session_seq > coalesce(
                 (select max(t.session_seq)
                    from spawn_events t
                    join spawn_commands c on c.id = (t.kind -> 'detail' ->> 'command_id')::bigint
                   where t.session_id = $1 and t.event_name = 'command.step'
                     and t.kind -> 'detail' ->> 'step' = 'typed'
                     and c.principal = 'spawn-dispatch'
                     and c.idempotency_key like 'deliver-%'), 0)) m";

/// When each of the tags `$2` was first delivered into session `$1`: the
/// earliest finish of a successful dispatcher command that filed it (for
/// the supervisor, the digest that told it of an escalation), as
/// `{"<tag>": <ms>}`. The same commands `told` reads in [`DISPATCH_FACTS`].
pub(crate) const TOLD_AT: &str = r"
select coalesce(jsonb_object_agg(t.tag, t.at_ms), '{}'::jsonb)
  from (select tag, min(coalesce(c.finished_at_ms, c.created_at_ms)) as at_ms
          from spawn_commands c
         cross join lateral jsonb_array_elements_text(
                 case when jsonb_typeof(c.action -> 'tags') = 'array'
                      then c.action -> 'tags' else '[]'::jsonb end) tag
         where c.session_id = $1 and c.principal = 'spawn-dispatch' and c.succeeded
           and tag = any($2)
         group by tag) t";

/// Where session `$1`'s engine writes its transcript and when its latest
/// turn ended: the `transcript_path` of its latest `session.native_started`
/// that names one, the `workspace` of its latest `execution.started`, and
/// the `recorded_at_ms` of its latest `turn.stopped` or `turn.failed`
/// (`turn_ended_at_ms`); each `null` when the log has none.
pub(crate) const LATEST_TURN: &str = r"
select jsonb_build_object(
  'transcript', (select e.kind -> 'detail' ->> 'transcript_path' from spawn_events e
                  where e.session_id = $1 and e.event_name = 'session.native_started'
                    and e.kind -> 'detail' ->> 'transcript_path' is not null
                  order by e.session_seq desc limit 1),
  'workspace', (select e.kind -> 'detail' ->> 'workspace' from spawn_events e
                 where e.session_id = $1 and e.event_name = 'execution.started'
                 order by e.session_seq desc limit 1),
  'turn_ended_at_ms', (select e.recorded_at_ms from spawn_events e
                        where e.session_id = $1
                          and e.event_name in ('turn.stopped', 'turn.failed')
                        order by e.session_seq desc limit 1))";

impl PostgresStore {
    /// When each of `tags` was first delivered into `session` ([`TOLD_AT`]).
    pub fn told_at(&self, session: &str, tags: &[String]) -> Result<Value, StoreError> {
        self.query_json(TOLD_AT, &[&session, &tags])
    }

    /// `session`'s transcript, workspace and latest turn's end
    /// ([`LATEST_TURN`]).
    pub fn latest_turn(&self, session: &str) -> Result<Value, StoreError> {
        self.query_json(LATEST_TURN, &[&session])
    }

    /// Files main's merges changed in `session`'s branch since the last
    /// delivery the worker was typed ([`UNTOLD_MERGES`]).
    pub fn untold_merges(&self, session: &str) -> Result<Vec<String>, StoreError> {
        let files = self.query_json(UNTOLD_MERGES, &[&session])?;
        Ok(serde_json::from_value(files).unwrap_or_default())
    }

    /// Facts for the dispatcher, as JSON rows.
    pub fn dispatch_facts(&self) -> Result<Value, StoreError> {
        self.query_json_prepared(DISPATCH_FACTS, &[&spawn_core::bridge::MAY_HAVE_TYPED])
    }

    /// The plan the dispatcher's facts read, as `EXPLAIN (FORMAT JSON)` gives
    /// it, without running it: the per-session reads of `spawn_commands` are
    /// index scans of `(session_id, id)` and `(session_id, principal, id)`,
    /// never a scan of the table (tests/wake.rs states it over 20,000 rows).
    pub fn dispatch_facts_plan(&self) -> Result<Value, StoreError> {
        let sql = format!("explain (format json, costs off) {DISPATCH_FACTS}");
        self.query_json(&sql, &[&spawn_core::bridge::MAY_HAVE_TYPED])
    }

    /// Every session with its judgement, task and activity, supervisor first.
    pub fn fleet_board(&self) -> Result<Value, StoreError> {
        self.query_json(FLEET_BOARD, &[])
    }

    /// Fleet health and the decision ladder over the last hour and day.
    pub fn fleet_health(&self) -> Result<Value, StoreError> {
        self.query_json(FLEET_HEALTH, &[])
    }

    /// One session's judgements, newest first, with the evidence each saw.
    pub fn judgements_json(&self, session: &str, limit: u32) -> Result<Value, StoreError> {
        self.query_json(SESSION_JUDGEMENTS, &[&session, &i64::from(limit)])
    }

    /// Commands into one session, newest first, with who sent them.
    pub fn commands_json(&self, session: &str, limit: u32) -> Result<Value, StoreError> {
        self.query_json(SESSION_COMMANDS, &[&session, &i64::from(limit)])
    }
}
