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
         r.charter, r.name as resident
    from mission_tasks k
    join mission_missions m on m.id = k.mission_id
    left join mission_residents r on r.agent_id = k.agent_id
   where k.state in ('assigned', 'delivered', 'reported', 'revise')
      or (k.state in ('accepted', 'cancelled') and k.kind = 'ephemeral'
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
             m.created_at_ms, m.updated_at_ms,
             coalesce((select jsonb_agg(jsonb_build_object(
                 'id', k.id, 'kind', k.kind, 'agent_id', k.agent_id,
                 'session_id', k.session_id, 'brief', left(k.brief, 600),
                 'state', k.state, 'revisions', k.revisions,
                 'review_note', k.review_note, 'report', k.report,
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
