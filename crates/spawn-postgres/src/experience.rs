//! How the fleet did over a window, from the experience views
//! (`spawn_decisions`, `spawn_interventions`, `spawn_outcomes`,
//! `spawn_escalations`; the schema) and the executions, judgements and
//! control records they sit beside: the answer of `/api/scorecard/fleet`
//! (spawn-cli's serve). Every figure is counted by SQL over `[since, until)`;
//! a ratio or a number of hours is rounded to 3 decimals, half away from
//! zero, as the scorecard rounds (docs/design.md §15). The reader login
//! (`spawn serve`) may run it.

use serde_json::Value;
use spawn_core::port::StoreError;

use crate::PostgresStore;

/// The window's figures, `$1` its start and `$2` its end in ms.
pub(crate) const FLEET_EXPERIENCE: &str = r"
with
  w as (select $1::bigint as since, $2::bigint as until),
  decisions as (
    select d.tier, count(*) as n
      from spawn_decisions d, w
     where d.at_ms >= w.since and d.at_ms < w.until
     group by d.tier),
  -- Every execution's running time inside the window; one still running
  -- runs to its end.
  live as (
    select coalesce(sum(least(coalesce(x.ended_at_ms, w.until), w.until)
                        - greatest(x.started_at_ms, w.since)), 0)::numeric as ms
      from agent_executions x, w
     where x.started_at_ms < w.until and coalesce(x.ended_at_ms, w.until) > w.since),
  interventions as (
    select i.kind, count(*) as n
      from spawn_interventions i, w
     where i.at_ms >= w.since and i.at_ms < w.until
     group by i.kind),
  outcomes as (
    select o.outcome, count(*) as n
      from spawn_outcomes o, w
     where o.at_ms >= w.since and o.at_ms < w.until
     group by o.outcome),
  judged as (
    select j.observation
      from agent_judgements j, w
     where j.recorded_at_ms >= w.since and j.recorded_at_ms < w.until),
  -- Each mission open in the window: from its `mission.added` (or its
  -- legacy row's creation) to its first `mission.closed` after that (or
  -- its legacy row's last change, closed), clipped to the window.
  opened as (
    select c.id as mission,
           min(case when c.type = 'mission.added' then c.at_ms
                    when jsonb_typeof(c.record -> 'row' -> 'created_at_ms') = 'number'
                    then (c.record -> 'row' ->> 'created_at_ms')::bigint
                    else c.at_ms end) as added_at_ms,
           min(case when c.type = 'legacy.imported'
                         and c.record -> 'row' ->> 'state' not in ('open', 'active')
                    then coalesce(case when jsonb_typeof(c.record -> 'row' -> 'updated_at_ms')
                                            = 'number'
                                       then (c.record -> 'row' ->> 'updated_at_ms')::bigint end,
                                  c.at_ms) end) as legacy_closed_at_ms
      from spawn_control_stream c
     where c.id is not null
       and (c.type = 'mission.added'
            or (c.type = 'legacy.imported' and c.legacy_table = 'mission_missions'))
     group by c.id),
  spans as (
    select o.mission,
           greatest(o.added_at_ms, w.since) as lo,
           least(coalesce(o.legacy_closed_at_ms,
                          (select min(c.at_ms) from spawn_control_stream c
                            where c.type = 'mission.closed' and c.id = o.mission
                              and c.at_ms >= o.added_at_ms)),
                 w.until) as hi
      from opened o, w),
  missions as (select * from spans where lo < hi),
  -- Its tasks' open intervals, assignment to close, clipped to its span,
  -- and their union (each run of overlapping intervals is one).
  intervals as (
    select m.mission, greatest(k.assigned_at_ms, m.lo) as lo,
           least(coalesce(k.closed_at_ms, m.hi), m.hi) as hi
      from missions m
      join spawn_task_spans k on k.mission_id = m.mission),
  clipped as (select * from intervals where lo < hi),
  runs as (
    select c.mission, c.lo, c.hi,
           max(c.hi) over (partition by c.mission order by c.lo, c.hi
                           rows between unbounded preceding and 1 preceding) as reach
      from clipped c),
  islands as (
    select r.mission, r.lo, r.hi,
           sum(case when r.reach >= r.lo then 0 else 1 end)
             over (partition by r.mission order by r.lo, r.hi) as island
      from runs r),
  busy as (
    select i.mission, sum(i.hi - i.lo) as ms
      from (select mission, island, min(lo) as lo, max(hi) as hi
              from islands group by mission, island) i
     group by i.mission),
  idle as (
    select m.mission, (m.hi - m.lo)::numeric as span_ms,
           (m.hi - m.lo - coalesce(b.ms, 0))::numeric as idle_ms
      from missions m left join busy b using (mission))
select jsonb_build_object(
  'since_ms', (select since from w),
  'until_ms', (select until from w),
  'ladder', jsonb_build_object(
    'rules', coalesce((select n from decisions where tier = 'rules'), 0),
    'model', coalesce((select n from decisions where tier = 'model'), 0),
    'dispatcher', coalesce((select n from decisions where tier = 'dispatcher'), 0),
    'supervisor', coalesce((select n from decisions where tier = 'supervisor'), 0),
    'human', coalesce((select n from decisions where tier = 'human'), 0),
    'liveness', coalesce((select n from decisions where tier = 'liveness'), 0)),
  'ladder_extra', (select coalesce(jsonb_agg(jsonb_build_array(tier, n) order by tier), '[]'::jsonb)
                     from decisions
                    where tier not in ('rules', 'model', 'dispatcher', 'supervisor', 'human',
                                       'liveness')),
  'agent_hours', (select round(ms / 3600000, 3)::float8 from live),
  'human_per_agent_hour',
    (select case when ms > 0 then round(coalesce((select n from decisions where tier = 'human'), 0)
                                        / (ms / 3600000), 3)::float8 end
       from live),
  'escalations', (select count(*) from spawn_escalations e, w
                   where e.at_ms >= w.since and e.at_ms < w.until),
  'interventions', jsonb_build_object(
    'total', coalesce((select sum(n) from interventions), 0),
    'by_kind', jsonb_build_object(
      'command', coalesce((select n from interventions where kind = 'command'), 0),
      'resume', coalesce((select n from interventions where kind = 'resume'), 0),
      'owner_task', coalesce((select n from interventions where kind = 'owner_task'), 0),
      'cancel_reassign', coalesce((select n from interventions where kind = 'cancel_reassign'), 0))),
  'outcomes', jsonb_build_object(
    'landed', coalesce((select n from outcomes where outcome = 'landed'), 0),
    'accepted', coalesce((select n from outcomes where outcome = 'accepted'), 0),
    'cancelled', coalesce((select n from outcomes where outcome = 'cancelled'), 0),
    'aborted', coalesce((select n from outcomes where outcome = 'aborted'), 0),
    'over_budget', coalesce((select n from outcomes where outcome = 'over_budget'), 0)),
  'landed_per_day',
    (select round(coalesce((select sum(n) from outcomes where outcome in ('landed', 'accepted')), 0)
                  / ((w.until - w.since)::numeric / 86400000), 3)::float8 from w),
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
    'model_latency_p50_ms', (select round((percentile_cont(0.5) within group
                               (order by (observation ->> 'latency_ms')::float8))::numeric, 3)::float8
                               from judged
                              where jsonb_typeof(observation -> 'latency_ms') = 'number')),
  'missions', (select coalesce(jsonb_agg(jsonb_build_object(
                  'mission', i.mission,
                  'text', left(m.text, 200),
                  'hours', round(i.span_ms / 3600000, 3)::float8,
                  'idle_hours', round(i.idle_ms / 3600000, 3)::float8,
                  'idle', i.idle_ms * 2 > i.span_ms)
                  order by i.mission), '[]'::jsonb)
                 from idle i left join mission_missions m on m.id = i.mission)
)";

impl PostgresStore {
    /// How the fleet did over `[since_ms, until_ms)` ([`FLEET_EXPERIENCE`]):
    /// `{since_ms, until_ms, ladder, ladder_extra, agent_hours,
    /// human_per_agent_hour, escalations, interventions, outcomes,
    /// landed_per_day, judgements, missions}`.
    pub fn fleet_experience(&self, since_ms: i64, until_ms: i64) -> Result<Value, StoreError> {
        self.query_json(FLEET_EXPERIENCE, &[&since_ms, &until_ms])
    }
}
