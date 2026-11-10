//! What the fleet's model calls cost (objective 041): the loop's cost step
//! folds the Bridge's `model.usage` events into `spawn_cost_messages`,
//! priced when recorded (`spawn_cost_record`, owner only); `/api/cost` reads
//! the totals of a window, and the alerts step each budgeted mission's spend
//! of the UTC day. Every sum is the database's (`numeric`): exact, never
//! rounded to cents.

use serde_json::Value;
use spawn_core::port::StoreError;

use crate::PostgresStore;

/// The messages timed in `[$1, $2)`, summed by model, mission, task, agent
/// and execution (an execution with its models). `usd` is the sum of the
/// priced messages' costs, 0 for an entry with only unpriced ones (`null`
/// for a model that is priced nowhere in the window); `unpriced_tokens` the
/// four counts of the messages with no price. Most costly first.
const WINDOW: &str = r"
with m as materialized (
  select c.*, c.input_tokens + c.output_tokens + c.cache_write_tokens
              + c.cache_read_tokens as total
    from spawn_cost_messages c
   where c.at_ms >= $1 and c.at_ms < $2
), by_execution_model as (
  select execution_id, model, sum(usd) as usd,
         jsonb_build_object('input', sum(input_tokens), 'output', sum(output_tokens),
                            'cache_write', sum(cache_write_tokens),
                            'cache_read', sum(cache_read_tokens)) as tokens
    from m group by execution_id, model
)
select jsonb_build_object(
  'total', (select jsonb_build_object(
              'usd', coalesce(sum(usd), 0),
              'tokens', jsonb_build_object(
                  'input', coalesce(sum(input_tokens), 0),
                  'output', coalesce(sum(output_tokens), 0),
                  'cache_write', coalesce(sum(cache_write_tokens), 0),
                  'cache_read', coalesce(sum(cache_read_tokens), 0)),
              'unpriced_tokens', coalesce(sum(total) filter (where usd is null), 0))
              from m),
  'models', coalesce((select jsonb_agg(jsonb_build_object(
                'model', model, 'usd', usd, 'tokens', tokens)
                order by usd desc nulls last, model)
              from (select model, sum(usd) as usd,
                           jsonb_build_object('input', sum(input_tokens),
                                              'output', sum(output_tokens),
                                              'cache_write', sum(cache_write_tokens),
                                              'cache_read', sum(cache_read_tokens)) as tokens
                      from m group by model) g), '[]'::jsonb),
  'missions', coalesce((select jsonb_agg(jsonb_build_object(
                'mission', mission_id, 'usd', usd, 'tokens', tokens,
                'unpriced_tokens', unpriced_tokens)
                order by usd desc, unpriced_tokens desc, mission_id)
              from (select mission_id, coalesce(sum(usd), 0) as usd,
                           jsonb_build_object('input', sum(input_tokens),
                                              'output', sum(output_tokens),
                                              'cache_write', sum(cache_write_tokens),
                                              'cache_read', sum(cache_read_tokens)) as tokens,
                           coalesce(sum(total) filter (where usd is null), 0) as unpriced_tokens
                      from m where mission_id is not null group by mission_id) g), '[]'::jsonb),
  'tasks', coalesce((select jsonb_agg(jsonb_build_object(
                'task', task_id, 'mission', mission_id, 'usd', usd, 'tokens', tokens,
                'unpriced_tokens', unpriced_tokens)
                order by usd desc, unpriced_tokens desc, task_id)
              from (select task_id, max(mission_id) as mission_id, coalesce(sum(usd), 0) as usd,
                           jsonb_build_object('input', sum(input_tokens),
                                              'output', sum(output_tokens),
                                              'cache_write', sum(cache_write_tokens),
                                              'cache_read', sum(cache_read_tokens)) as tokens,
                           coalesce(sum(total) filter (where usd is null), 0) as unpriced_tokens
                      from m where task_id is not null group by task_id) g), '[]'::jsonb),
  'agents', coalesce((select jsonb_agg(jsonb_build_object(
                'agent', agent_id, 'usd', usd, 'tokens', tokens,
                'unpriced_tokens', unpriced_tokens)
                order by usd desc, unpriced_tokens desc, agent_id)
              from (select agent_id, coalesce(sum(usd), 0) as usd,
                           jsonb_build_object('input', sum(input_tokens),
                                              'output', sum(output_tokens),
                                              'cache_write', sum(cache_write_tokens),
                                              'cache_read', sum(cache_read_tokens)) as tokens,
                           coalesce(sum(total) filter (where usd is null), 0) as unpriced_tokens
                      from m group by agent_id) g), '[]'::jsonb),
  'executions', coalesce((select jsonb_agg(jsonb_build_object(
                'execution', execution_id, 'agent', agent_id, 'task', task_id, 'usd', usd,
                'tokens', tokens, 'unpriced_tokens', unpriced_tokens,
                'models', (select coalesce(jsonb_agg(jsonb_build_object(
                                     'model', b.model, 'usd', b.usd, 'tokens', b.tokens)
                                     order by b.usd desc nulls last, b.model), '[]'::jsonb)
                             from by_execution_model b where b.execution_id = g.execution_id))
                order by usd desc, unpriced_tokens desc, execution_id)
              from (select execution_id, max(agent_id) as agent_id,
                           (array_agg(task_id order by at_ms desc, message_id))[1] as task_id,
                           coalesce(sum(usd), 0) as usd,
                           jsonb_build_object('input', sum(input_tokens),
                                              'output', sum(output_tokens),
                                              'cache_write', sum(cache_write_tokens),
                                              'cache_read', sum(cache_read_tokens)) as tokens,
                           coalesce(sum(total) filter (where usd is null), 0) as unpriced_tokens
                      from m group by execution_id) g), '[]'::jsonb))";

/// Every mission with a daily budget (`budget.daily_usd`) and its cost of
/// the messages timed in `[$1, $2)` (the UTC day so far).
const BUDGETS: &str = r"
select coalesce(jsonb_agg(jsonb_build_object(
         'mission', m.id,
         'budget_usd', m.budget -> 'daily_usd',
         'spent_usd', coalesce((select sum(c.usd) from spawn_cost_messages c
                                 where c.mission_id = m.id
                                   and c.at_ms >= $1 and c.at_ms < $2), 0))
         order by m.id), '[]'::jsonb)
  from mission_missions m
 where jsonb_typeof(m.budget -> 'daily_usd') = 'number'";

impl PostgresStore {
    /// Fold the `model.usage` events not yet folded, pricing each new
    /// message by `prices` (`spawn_cost_record`, owner only): how many
    /// messages it recorded or changed.
    pub fn cost_record(&self, prices: &Value) -> Result<i64, StoreError> {
        let count: i32 = self
            .query_one("select spawn_cost_record($1)", &[prices])?
            .get(0);
        Ok(i64::from(count))
    }

    /// The cost of the messages timed in `[since_ms, until_ms)` ([`WINDOW`]):
    /// `{total, models, missions, tasks, agents, executions}`.
    pub fn cost_window(&self, since_ms: i64, until_ms: i64) -> Result<Value, StoreError> {
        self.query_json(WINDOW, &[&since_ms, &until_ms])
    }

    /// Each mission with a daily budget and its spend in `[since_ms,
    /// until_ms)` ([`BUDGETS`]): `[{mission, budget_usd, spent_usd}]`.
    pub fn cost_budgets(&self, since_ms: i64, until_ms: i64) -> Result<Value, StoreError> {
        self.query_json(BUDGETS, &[&since_ms, &until_ms])
    }
}
