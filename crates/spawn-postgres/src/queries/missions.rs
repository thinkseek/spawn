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
