//! SQL behind missions, tasks and residents ([`crate::missions`]).
//!
//! Every decision is a call to a checked SQL function in `sql/schema.sql`,
//! so the supervisor's restricted login and the owner's CLI share one set of
//! rules. The delivery bookkeeping updates run as the owner.

/// One task the dispatcher has to look at, with what it needs to deliver it.
pub(crate) const DELIVERY_TASKS: &str = r"
select coalesce(jsonb_agg(row_to_json(t) order by t.id), '[]'::jsonb)
from (
  select k.id, k.mission_id, k.kind, k.agent_id, k.session_id, k.brief, k.state,
         k.revisions, k.review_note, k.command_id,
         m.text as mission_text, m.sources as mission_sources,
         m.gate is not null as gated,
         r.charter, r.name as resident
    from mission_tasks k
    join mission_missions m on m.id = k.mission_id
    left join mission_residents r on r.agent_id = k.agent_id
   where k.state in ('assigned', 'delivered', 'reported', 'revise', 'landing')
      or (k.state = 'accepted' and m.gate is not null)
      or (k.state in ('accepted', 'cancelled', 'landed') and k.kind = 'ephemeral'
          and k.updated_at_ms > spawn_now_ms() - 86400000)
) t";

/// What the supervisor should hear about, beyond worker states: open
/// missions, reports, residents approved, and human messages to workers.
pub(crate) const MISSION_NEWS: &str = r"
select jsonb_build_object(
  'missions', coalesce((select jsonb_agg(jsonb_build_object(
                 'id', m.id, 'text', left(m.text, 400), 'origin', m.origin,
                 'sources', m.sources) order by m.id)
               from mission_missions m where m.state = 'open'), '[]'::jsonb),
  'reports', coalesce((select jsonb_agg(jsonb_build_object(
                 'id', k.id, 'mission_id', k.mission_id, 'kind', k.kind,
                 'agent_id', k.agent_id, 'session_id', k.session_id,
                 'report', left(k.report, 600)) order by k.id)
               from mission_tasks k where k.state = 'reported'), '[]'::jsonb),
  'approved', coalesce((select jsonb_agg(jsonb_build_object('name', r.name) order by r.name)
               from mission_residents r
              where r.state = 'approved'
                and r.updated_at_ms > spawn_now_ms() - 86400000), '[]'::jsonb),
  'proposed', coalesce((select jsonb_agg(jsonb_build_object(
                 'name', r.name, 'charter', left(r.charter, 400)) order by r.name)
               from mission_residents r where r.state = 'proposed'), '[]'::jsonb),
  'human', coalesce((select jsonb_agg(jsonb_build_object(
                 'command_id', c.id, 'session_id', c.session_id,
                 'text', left(coalesce(c.action ->> 'text', c.action ->> 'prompt', c.action ->> 'type'), 200))
                 order by c.id)
               from spawn_commands c
              where c.principal = 'human'
                and (c.agent_id like 'agent\_worker\_%' or c.agent_id like 'agent\_resident\_%')
                and c.created_at_ms > spawn_now_ms() - 86400000), '[]'::jsonb)
)";

/// The board of missions with their tasks, newest first.
pub(crate) const MISSION_BOARD: &str = r"
select jsonb_build_object(
  'missions', coalesce((select jsonb_agg(row_to_json(x) order by x.id desc) from (
      select m.id, m.text, m.origin, m.sources, m.state, m.summary,
             m.gate, m.accept, m.budget, m.baseline, m.halted, m.objectives,
             m.created_at_ms, m.updated_at_ms,
             coalesce((select jsonb_agg(jsonb_build_object(
                 'id', k.id, 'kind', k.kind, 'agent_id', k.agent_id,
                 'session_id', k.session_id, 'brief', left(k.brief, 600),
                 'state', k.state, 'revisions', k.revisions,
                 'review_note', k.review_note, 'report', k.report,
                 'candidate', k.candidate, 'objective', k.objective, 'metric', k.metric,
                 'created_by', k.created_by, 'created_at_ms', k.created_at_ms,
                 'updated_at_ms', k.updated_at_ms) order by k.id)
               from mission_tasks k where k.mission_id = m.id), '[]'::jsonb) as tasks
        from mission_missions m
       order by m.id desc limit $1) x), '[]'::jsonb),
  'residents', coalesce((select jsonb_agg(row_to_json(r) order by r.name) from (
      select name, agent_id, session_id, charter, state, proposed_by, approved_by,
             created_at_ms, updated_at_ms from mission_residents) r), '[]'::jsonb)
)";

// Checked decisions, one SQL function each.

/// `$1` text, `$2` sources, `$3` origin; returns the mission id.
pub(crate) const MISSION_ADD: &str = "select spawn_mission_add($1, $2, $3)";
/// `$1` mission, `$2` final state, `$3` summary.
pub(crate) const MISSION_CLOSE: &str = "select spawn_mission_close($1, $2, $3)";
/// `$1` mission, `$2` gate (absolute path or null), `$3` accept, `$4` budget.
/// `$5` objectives (`{backlog, resident}` or null).
pub(crate) const MISSION_CONFIGURE: &str = "select spawn_mission_configure($1, $2, $3, $4, $5)";
/// `$1` mission, `$2` objective, `$3` scenario, `$4` brief; returns the task.
pub(crate) const OBJECTIVE_ASSIGN: &str = "select spawn_objective_assign($1, $2, $3, $4)";
/// `$1` mission, `$2` objective, `$3` metric line, `$4` note, `$5` time.
pub(crate) const OBJECTIVE_IMPORT: &str = "select spawn_objective_import($1, $2, $3, $4, $5)";

/// An objective mission's landed tasks: `[{objective, metric}]`, newest
/// first. `$1` mission.
pub(crate) const OBJECTIVE_LANDINGS: &str = r"
select coalesce(jsonb_agg(jsonb_build_object('objective', k.objective, 'metric', k.metric)
                          order by k.updated_at_ms desc, k.id desc), '[]'::jsonb)
  from mission_tasks k
 where k.mission_id = $1 and k.state = 'landed' and k.objective is not null";

/// Gate verdicts on one objective's tasks, newest first, from the control
/// stream: `[{task, verdict, reason, metric, at_ms}]`. `$1` mission,
/// `$2` objective, `$3` limit.
pub(crate) const OBJECTIVE_HISTORY: &str = r"
select coalesce(jsonb_agg(z.h order by z.seq desc), '[]'::jsonb)
  from (select e.session_seq as seq,
               jsonb_build_object('task', (r ->> 'id')::bigint, 'verdict', r ->> 'verdict',
                                  'reason', r ->> 'reason', 'metric', r ->> 'metric',
                                  'at_ms', e.recorded_at_ms) as h
          from spawn_events e
         cross join lateral (select e.kind -> 'detail' -> 'record' as r) x
         where e.session_id = 'session_control' and e.event_name = 'control.recorded'
           and r ->> 'type' = 'task.gated' and r ->> 'verdict' in ('accepted', 'rejected', 'aborted')
           and (r ->> 'id')::bigint in (select id from mission_tasks
                                         where mission_id = $1 and objective = $2)
         order by e.session_seq desc
         limit $3) z";
/// The last `task.gated` record of every task, keyed by task id:
/// `{"<id>": {verdict, reason, metric, candidate, at_ms}}`. Kept out of
/// [`MISSION_BOARD`]: the supervisor's login reads the board and may not
/// read `spawn_events`.
pub(crate) const LAST_GATES: &str = r"
select coalesce(jsonb_object_agg(z.task, z.gate), '{}'::jsonb)
  from (select distinct on (r ->> 'id') r ->> 'id' as task,
               jsonb_build_object('verdict', r ->> 'verdict', 'reason', r ->> 'reason',
                                  'metric', r ->> 'metric', 'candidate', r ->> 'candidate',
                                  'at_ms', e.recorded_at_ms) as gate
          from spawn_events e
         cross join lateral (select e.kind -> 'detail' -> 'record' as r) g
         where e.session_id = 'session_control' and e.event_name = 'control.recorded'
           and r ->> 'type' = 'task.gated'
         order by r ->> 'id', e.session_seq desc) z";

/// Open objective missions (`objectives` = `{backlog, resident}`), for the
/// RSI page: settings, the resident, and every task newest first with its
/// gate verdicts from the control stream (oldest first, without logs).
pub(crate) const OBJECTIVE_MISSIONS: &str = r"
select coalesce(jsonb_agg(row_to_json(x) order by x.id), '[]'::jsonb)
from (
  select m.id, m.text, m.state, m.sources, m.gate, m.accept, m.budget, m.baseline, m.halted,
         m.scenario_floor, m.gate_rejections, m.gate_aborts, m.objectives,
         m.created_at_ms, m.updated_at_ms,
         (select jsonb_build_object('name', r.name, 'agent_id', r.agent_id,
                                    'session_id', r.session_id, 'state', r.state)
            from mission_residents r where r.name = m.objectives ->> 'resident') as resident,
         coalesce((select jsonb_agg(jsonb_build_object(
                     'id', k.id, 'kind', k.kind, 'agent_id', k.agent_id,
                     'session_id', k.session_id, 'state', k.state, 'revisions', k.revisions,
                     'objective', k.objective, 'candidate', k.candidate, 'base', k.base,
                     'metric', k.metric, 'scenario', k.scenario,
                     'brief', left(k.brief, 600), 'review_note', left(k.review_note, 2000),
                     'created_by', k.created_by, 'created_at_ms', k.created_at_ms,
                     'delivered_at_ms', k.delivered_at_ms, 'updated_at_ms', k.updated_at_ms,
                     'gates', coalesce((
                        select jsonb_agg(jsonb_build_object(
                                 'verdict', r ->> 'verdict', 'reason', r ->> 'reason',
                                 'metric', r ->> 'metric', 'candidate', r ->> 'candidate',
                                 'at_ms', e.recorded_at_ms) order by e.session_seq)
                          from spawn_events e
                         cross join lateral (select e.kind -> 'detail' -> 'record' as r) g
                         where e.session_id = 'session_control'
                           and e.event_name = 'control.recorded'
                           and r ->> 'type' = 'task.gated'
                           and r ->> 'id' = k.id::text), '[]'::jsonb))
                     order by k.id desc)
                   from mission_tasks k where k.mission_id = m.id), '[]'::jsonb) as tasks
    from mission_missions m
   where m.objectives is not null and m.state in ('open', 'active')
) x";

/// One task of an objective mission with its full brief, report, last gate
/// log, and every gate verdict with its log, oldest first; null when there
/// is no such task. `$1` task.
pub(crate) const OBJECTIVE_TASK: &str = r"
select (select jsonb_build_object(
          'id', k.id, 'mission_id', k.mission_id, 'kind', k.kind, 'agent_id', k.agent_id,
          'session_id', k.session_id, 'state', k.state, 'revisions', k.revisions,
          'objective', k.objective, 'candidate', k.candidate, 'base', k.base,
          'metric', k.metric, 'scenario', k.scenario, 'brief', k.brief,
          'review_note', k.review_note, 'report', k.report, 'gate_log', k.gate_log,
          'created_by', k.created_by, 'created_at_ms', k.created_at_ms,
          'delivered_at_ms', k.delivered_at_ms, 'updated_at_ms', k.updated_at_ms,
          'gates', coalesce((
             select jsonb_agg(jsonb_build_object(
                      'verdict', r ->> 'verdict', 'reason', r ->> 'reason',
                      'metric', r ->> 'metric', 'candidate', r ->> 'candidate',
                      'log', r ->> 'log', 'at_ms', e.recorded_at_ms) order by e.session_seq)
               from spawn_events e
              cross join lateral (select e.kind -> 'detail' -> 'record' as r) g
              where e.session_id = 'session_control'
                and e.event_name = 'control.recorded'
                and r ->> 'type' = 'task.gated'
                and r ->> 'id' = k.id::text), '[]'::jsonb))
          from mission_tasks k where k.id = $1)";

/// The integrator's advisory lock: one integrator per database. The key is
/// the one the retired RSI controller held, so an old controller and an
/// integrator can never run at once.
pub(crate) const INTEGRATOR_LOCK_KEY: i64 = 1_399_957_336;

/// `$1` the lock key; true when this connection now holds it.
pub(crate) const INTEGRATOR_TRY_LOCK: &str = "select pg_try_advisory_lock($1) as held";

/// `$1` mission.
pub(crate) const MISSION_RESUME: &str = "select spawn_mission_resume($1)";
/// `$1` the record; returns its event id.
pub(crate) const INTEGRATOR_RECORD: &str = "select spawn_integrator_record($1)";

/// Missions with a gate that are still open, with their tasks the
/// integrator may have to act on.
pub(crate) const GATED: &str = r"
select coalesce(jsonb_agg(row_to_json(x) order by x.id), '[]'::jsonb)
from (
  select m.id, m.gate, m.accept, m.budget, m.baseline, m.halted, m.scenario_floor,
         m.gate_rejections, m.gate_aborts, m.objectives, m.sources ->> 0 as source,
         coalesce((select jsonb_agg(jsonb_build_object(
                     'id', k.id, 'kind', k.kind, 'agent_id', k.agent_id,
                     'session_id', k.session_id, 'state', k.state, 'base', k.base,
                     'candidate', k.candidate, 'scenario', k.scenario,
                     'objective', k.objective, 'delivered_at_ms', k.delivered_at_ms)
                     order by k.id)
                   from mission_tasks k
                  where k.mission_id = m.id
                    and k.state in ('assigned', 'delivered', 'reported', 'revise',
                                    'accepted', 'landing')), '[]'::jsonb) as tasks
    from mission_missions m
   where m.gate is not null and m.state in ('open', 'active')
) x";

/// Tool calls a session started after a moment. `$1` session, `$2` ms.
pub(crate) const TOOL_CALLS_SINCE: &str = "select count(*) from spawn_events
                  where session_id = $1 and event_name = 'tool.started' and recorded_at_ms > $2";

/// `$1` mission, `$2` kind, `$3` worker, `$4` brief; returns the task id.
pub(crate) const TASK_ASSIGN: &str = "select spawn_task_assign($1, $2, $3, $4)";
/// `$1` task, `$2` accept, `$3` note.
pub(crate) const TASK_REVIEW: &str = "select spawn_task_review($1, $2, $3)";
/// `$1` task.
pub(crate) const TASK_CANCEL: &str = "select spawn_task_cancel($1)";
/// `$1` name, `$2` charter.
pub(crate) const RESIDENT_PROPOSE: &str = "select spawn_resident_propose($1, $2)";
/// `$1` name.
pub(crate) const RESIDENT_APPROVE: &str = "select spawn_resident_approve($1)";
/// `$1` name.
pub(crate) const RESIDENT_RETIRE: &str = "select spawn_resident_retire($1)";

// Residents and workspaces.

/// Approved residents that have run before, as (agent, session, charter).
pub(crate) const RESIDENTS_TO_KEEP: &str = "select r.agent_id, r.session_id, r.charter
                   from mission_residents r
                  where r.state = 'approved'
                    and exists (select 1 from spawn_events e
                                 where e.session_id = r.session_id
                                   and e.event_name = 'execution.started')
                  order by r.name";

/// Retired residents, as (agent, session).
pub(crate) const RETIRED_RESIDENTS: &str =
    "select agent_id, session_id from mission_residents where state = 'retired'";

/// First source of the mission behind an agent's first task. `$1` agent.
pub(crate) const AGENT_SOURCE: &str = "select m.sources ->> 0
                   from mission_tasks k join mission_missions m on m.id = k.mission_id
                  where k.agent_id = $1
                  order by k.id limit 1";

// Delivery bookkeeping, run by the dispatcher as the owner.

/// `$1` task, `$2` delivery command.
pub(crate) const TASK_DELIVERED: &str = "select spawn_task_delivered($1, $2)";

/// `$1` task, `$2` report. True when the report was recorded.
pub(crate) const TASK_REPORTED: &str = "select spawn_task_reported($1, $2)";
