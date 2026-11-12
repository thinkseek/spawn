//! How the fleet does over a window (objective 043; docs/fleet-architecture.md
//! §4 "Experience"): the experience views (`spawn_decisions`,
//! `spawn_interventions`, `spawn_outcomes`, `spawn_escalations`), the
//! judgements and the executions, counted over `[since, until)` in one
//! statement, one snapshot. Nothing here rounds or judges: spawn-cli's
//! `scorecard::fleet` turns these counts into `/api/scorecard/fleet`. The
//! reader login (`spawn serve`) may run it: it selects every table and view.

use serde_json::Value;
use spawn_core::port::StoreError;

use crate::PostgresStore;

/// The counts of `[$1, $2)`:
/// - `ladder`: decisions by tier; `interventions`: by kind; `outcomes`: by
///   outcome; `escalations`: how many;
/// - `agent_ms`: the executions' running time clipped to the window (one
///   still running, or ended at or after `$2`, runs to `$2`);
/// - `judgements`: `total`, `by_judge`, `judge_unavailable`,
///   `disagreements`, `model_latency_p50_ms`, as `/api/fleet`'s hour counts
///   them;
/// - `missions`: each mission open in the window (`spawn_mission_spans`),
///   by id, with its span and its tasks' open intervals as the scorecard's
///   throughput counts them (each task's latest assignment before `$2`, to
///   its close; `null` while open at `$2`), and `owner_work`: whether
///   its last configuration before `$2` (`mission.configured` or a legacy
///   mission row) left it with no objective backlog (`objectives` not an
///   object), so its work comes from the owner (objective 062). A mission
///   with no configuration record before `$2` (only its `mission.added`)
///   has no objective backlog either, so it is owner work too (objective
///   077); only a mission whose last configuration has a backlog is not.
///   `pauses`: the owner's pauses recorded before `$2` (objective 099),
///   oldest first, each `[paused_at_ms, unpaused_at_ms]`, the second `null`
///   while it lasts at `$2`.
const WINDOW: &str = r"
with
  decided as (
    select d.tier, count(*) as n from spawn_decisions d
     where d.at_ms >= $1 and d.at_ms < $2 group by d.tier),
  judged as (
    select j.observation from agent_judgements j
     where j.recorded_at_ms >= $1 and j.recorded_at_ms < $2),
  spans as (
    select distinct on (t.task_id) t.task_id, t.mission_id, t.assigned_at_ms,
           case when t.closed_at_ms < $2 then t.closed_at_ms end as closed_at_ms
      from spawn_task_spans t
     where t.assigned_at_ms < $2
     order by t.task_id, t.assigned_seq desc),
  missions as (
    select m.mission_id, m.text, m.opened_at_ms,
           case when m.closed_at_ms < $2 then m.closed_at_ms end as closed_at_ms
      from spawn_mission_spans m
     where m.opened_at_ms < $2 and (m.closed_at_ms is null or m.closed_at_ms > $1))
select jsonb_build_object(
  'ladder', (select coalesce(jsonb_object_agg(tier, n), '{}'::jsonb) from decided),
  'interventions', (select coalesce(jsonb_object_agg(kind, n), '{}'::jsonb)
                      from (select i.kind, count(*) as n from spawn_interventions i
                             where i.at_ms >= $1 and i.at_ms < $2 group by i.kind) t),
  'outcomes', (select coalesce(jsonb_object_agg(outcome, n), '{}'::jsonb)
                 from (select o.outcome, count(*) as n from spawn_outcomes o
                        where o.at_ms >= $1 and o.at_ms < $2 group by o.outcome) t),
  'escalations', (select count(*) from spawn_escalations e where e.at_ms >= $1 and e.at_ms < $2),
  'agent_ms', (select coalesce(sum(greatest(0,
                 least(coalesce(x.ended_at_ms, $2), $2) - greatest(x.started_at_ms, $1))), 0)::bigint
                 from agent_executions x
                where x.started_at_ms < $2 and coalesce(x.ended_at_ms, $2) > $1),
  'judgements', jsonb_build_object(
    'total', (select count(*) from judged),
    'by_judge', (select coalesce(jsonb_object_agg(judge, n), '{}'::jsonb)
                   from (select coalesce(observation ->> 'judge', 'unknown') as judge, count(*) as n
                           from judged group by 1) t),
    'judge_unavailable', (select count(*) from judged
                           where observation ->> 'reason' = 'JUDGE_UNAVAILABLE'),
    'disagreements', (select count(*) from judged
                       where observation ? 'rules_state'
                         and observation ->> 'rules_state' is distinct from observation ->> 'state'),
    'model_latency_p50_ms', (select percentile_cont(0.5) within group
                               (order by (observation ->> 'latency_ms')::float8)
                               from judged
                              where jsonb_typeof(observation -> 'latency_ms') = 'number')),
  'missions', (select coalesce(jsonb_agg(jsonb_build_object(
                  'mission', m.mission_id, 'text', left(m.text, 200),
                  'opened_at_ms', m.opened_at_ms, 'closed_at_ms', m.closed_at_ms,
                  'owner_work', coalesce((
                      select coalesce(jsonb_typeof(coalesce(r.record -> 'objectives',
                                                            r.record -> 'row' -> 'objectives')),
                                      'null') <> 'object'
                        from spawn_experience_records r
                       where r.at_ms < $2
                         and (r.type = 'mission.configured'
                              or (r.type = 'legacy.imported'
                                  and r.record ->> 'table' = 'mission_missions'))
                         and spawn_experience_int(coalesce(r.record -> 'id', r.record -> 'row' -> 'id'))
                             = m.mission_id
                       order by r.seq desc limit 1), true),
                  'tasks', (select coalesce(jsonb_agg(jsonb_build_array(s.assigned_at_ms, s.closed_at_ms)
                                                      order by s.assigned_at_ms, s.task_id), '[]'::jsonb)
                              from spans s where s.mission_id = m.mission_id),
                  'pauses', (select coalesce(jsonb_agg(jsonb_build_array(p.recorded_at_ms, (
                                 select min(u.recorded_at_ms) from spawn_events u
                                  where u.session_id = 'session_control'
                                    and u.event_name = 'control.recorded'
                                    and u.kind -> 'detail' -> 'record' ->> 'type' = 'mission.unpaused'
                                    and u.kind -> 'detail' -> 'record' ->> 'pause_event' = p.id::text
                                    and u.recorded_at_ms < $2))
                                 order by p.id), '[]'::jsonb)
                               from spawn_events p
                              where p.session_id = 'session_control'
                                and p.event_name = 'control.recorded'
                                and p.kind -> 'detail' -> 'record' ->> 'type' = 'mission.paused'
                                and p.kind -> 'detail' -> 'record' ->> 'id' = m.mission_id::text
                                and p.recorded_at_ms < $2))
                  order by m.mission_id), '[]'::jsonb)
                 from missions m))";

/// The interventions of `[$1, $2)` (objective 049), each command's outcome
/// as it stood before `$2`:
/// - `agent_ms`: the executions' running time clipped to the window, as
///   [`WINDOW`] counts it;
/// - `items`: newest first, one per intervention (`spawn_interventions`;
///   a person's commands as intents, `spawn_human_intents`, each taken as
///   it stood before `$2`: its commands created before it, its outcome its
///   last one's if that finished before it, else `pending`). Each item has
///   `kind`, `at_ms`, `principal`, `agent`, `session` (a command's; null
///   for the other kinds), `line` (an input's first line, at most 120
///   characters, else the action's type; "resumed mission #<id>",
///   "paused mission #<id>", "assigned task #<id>", "cancelled task #<id>, assigned task #<id>"),
///   `commands` (an intent's ids in order; `[]` otherwise), `attempts` and
///   `outcome` (null for the other kinds).
const INTERVENTIONS: &str = r"
with
  intents as (
    select i.at_ms, i.intent, i.principal, i.agent_id, i.session_id, i.action,
           k.n, i.commands[1:k.n] as commands,
           case when i.finished_at_ms[k.n] is null or i.finished_at_ms[k.n] >= $2 then 'pending'
                when i.succeeded[k.n] then 'taken'
                else 'failed' end as outcome
      from spawn_human_intents i
     cross join lateral (
           select count(*)::integer as n from unnest(i.created_at_ms) t(created)
            where t.created < $2) k
     where i.at_ms >= $1 and i.at_ms < $2),
  items as (
    select i.at_ms, 'command' as kind, i.intent as id,
           jsonb_build_object(
             'kind', 'command', 'at_ms', i.at_ms, 'principal', i.principal,
             'agent', i.agent_id, 'session', i.session_id,
             'line', case when i.action ->> 'type' = 'input'
                               and jsonb_typeof(i.action -> 'text') = 'string'
                          then left(split_part(i.action ->> 'text', E'\n', 1), 120)
                          else coalesce(i.action ->> 'type', 'command') end,
             'commands', to_jsonb(i.commands), 'attempts', i.n, 'outcome', i.outcome) as item
      from intents i
    union all
    select v.at_ms, v.kind, coalesce(v.task_id, v.mission_id),
           jsonb_build_object(
             'kind', v.kind, 'at_ms', v.at_ms, 'principal', null, 'agent', null,
             'session', null,
             'line', case v.kind
                       when 'resume' then 'resumed mission #' || v.mission_id
                       when 'pause' then 'paused mission #' || v.mission_id
                       when 'owner_task' then 'assigned task #' || v.task_id
                       else 'cancelled task #' || v.cancelled_task_id
                            || ', assigned task #' || v.task_id end,
             'commands', '[]'::jsonb, 'attempts', null, 'outcome', null)
      from spawn_interventions v
     where v.kind <> 'command' and v.at_ms >= $1 and v.at_ms < $2)
select jsonb_build_object(
  'agent_ms', (select coalesce(sum(greatest(0,
                 least(coalesce(x.ended_at_ms, $2), $2) - greatest(x.started_at_ms, $1))), 0)::bigint
                 from agent_executions x
                where x.started_at_ms < $2 and coalesce(x.ended_at_ms, $2) > $1),
  'items', (select coalesce(jsonb_agg(t.item order by t.at_ms desc, t.kind, t.id desc), '[]'::jsonb)
              from items t))";

impl PostgresStore {
    /// How the fleet did over `[since_ms, until_ms)`, from the experience
    /// views ([`WINDOW`]).
    ///
    /// A mission's `owner_work` reads its last configuration record, and
    /// that is its configuration as its fold has it (`mission_missions`,
    /// what `/api/rsi` shows): every configure records every setting
    /// (`spawn mission configure` merges its change into the row through
    /// `spawn_mission_reconfigure`), and the fold replaces them, so a later
    /// configure that names only `accept` or a budget keeps the backlog
    /// (S22; tests/experience.rs).
    pub fn experience_window(&self, since_ms: i64, until_ms: i64) -> Result<Value, StoreError> {
        self.query_json(WINDOW, &[&since_ms, &until_ms])
    }

    /// The interventions of `[since_ms, until_ms)` and the agent time
    /// beside them ([`INTERVENTIONS`]).
    pub fn interventions_window(&self, since_ms: i64, until_ms: i64) -> Result<Value, StoreError> {
        self.query_json(INTERVENTIONS, &[&since_ms, &until_ms])
    }
}
