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
///   its close; `null` while open at `$2`).
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
                  'tasks', (select coalesce(jsonb_agg(jsonb_build_array(s.assigned_at_ms, s.closed_at_ms)
                                                      order by s.assigned_at_ms, s.task_id), '[]'::jsonb)
                              from spans s where s.mission_id = m.mission_id))
                  order by m.mission_id), '[]'::jsonb)
                 from missions m))";

impl PostgresStore {
    /// How the fleet did over `[since_ms, until_ms)`, from the experience
    /// views ([`WINDOW`]).
    pub fn experience_window(&self, since_ms: i64, until_ms: i64) -> Result<Value, StoreError> {
        self.query_json(WINDOW, &[&since_ms, &until_ms])
    }
}
