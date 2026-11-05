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
    (select min(j.recorded_at_ms) from agent_judgements j
      where j.session_id = s.session_id
        and j.recorded_at_ms > coalesce((
              select max(k.recorded_at_ms) from agent_judgements k
               where k.session_id = s.session_id and k.state is distinct from s.state), 0)
    ) as state_since_ms,
    case when coalesce((p.observation ->> 'attention')::boolean, false) then
      (select min(j.recorded_at_ms) from agent_judgements j
        where j.session_id = s.session_id
          and j.recorded_at_ms > coalesce((
                select max(k.recorded_at_ms) from agent_judgements k
                 where k.session_id = s.session_id
                   and not coalesce((k.observation ->> 'attention')::boolean, false)), 0))
    end as attention_since_ms,
    -- The assignment: the latest text a person or the supervisor gave, not
    -- a dispatcher digest or a liveness restart.
    (select left(coalesce(c.action ->> 'prompt', c.action ->> 'text'), 600)
       from spawn_commands c
      where c.session_id = s.session_id
        and c.principal not in ('spawn-dispatch', 'spawn-liveness')
        and coalesce(c.action ->> 'prompt', c.action ->> 'text') is not null
      order by c.id desc limit 1) as task,
    (select left(coalesce(c.action ->> 'prompt', c.action ->> 'text'), 600)
       from spawn_commands c
      where c.session_id = s.session_id
        and coalesce(c.action ->> 'prompt', c.action ->> 'text') is not null
      order by c.id desc limit 1) as last_input,
    jsonb_build_object(
      'current_tool', (select o.tool_name from agent_open_tools o
                        where o.session_id = s.session_id
                          and o.execution_id = s.execution_id and s.live
                        order by o.session_seq desc limit 1),
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

/// How the fleet and the decision ladder did over the last hour and day.
pub(crate) const FLEET_HEALTH: &str = r"
with
  now as (select spawn_now_ms() as ms),
  judged as (
    select j.observation, j.recorded_at_ms
      from agent_judgements j, now
     where j.recorded_at_ms > now.ms - 86400000),
  hour as (select * from judged, now where judged.recorded_at_ms > now.ms - 3600000),
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
     where e.recorded_at_ms > now.ms - 3600000)
select jsonb_build_object(
  'generated_at_ms', (select ms from now),
  'agents_total', (select count(*) from agent_status),
  'live', (select count(*) from agent_status where live),
  'by_state', (select coalesce(jsonb_object_agg(state, n), '{}'::jsonb)
                 from (select coalesce(state, 'UNKNOWN') as state, count(*) as n
                         from agent_status group by 1) t),
  'attention', (select count(*) from spawn_session_state
                 where coalesce((observation ->> 'attention')::boolean, false)),
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
    'rules', (select count(*) from judged where observation ->> 'judge' = 'rules'),
    'model', (select count(*) from judged where observation ->> 'judge' <> 'rules'),
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
/// there in the last day (its deliveries, however old: a delivery is
/// confirmed from its command), the tags of every input it delivered there
/// (`told`: the escalations its digests carried, however long ago);
/// `resumed_young` counts the executions its
/// resumes started that died young (`died_young!`) since the session's last
/// execution that did not.
///
/// A delivery counts as the worker's (its `last_delivery`, and the episode)
/// only when it reached the worker or may have: it succeeded, has not
/// finished, or failed after typing may have begun (its error names `$1`,
/// [`spawn_core::bridge::MAY_HAVE_TYPED`]). A delivery refused before
/// anything was typed (a pane not ready, no execution, a source that did not
/// answer) told the worker nothing: the worker's last delivery is still the
/// one before it, and its own retry is not judged against itself.
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
    (select max(e.session_seq) from spawn_events e
      where e.session_id = s.session_id and e.source <> 'observer'
        and e.event_name not in ('artifact.committed', 'merge.conflicted')) as evidence_seq,
    ep.id as episode,
    ep.action ->> 'type' as episode_action,
    exists (select 1 from spawn_commands c
             where c.session_id = s.session_id and c.state <> 'finished') as open_command,
    coalesce((select e.kind -> 'detail' ->> 'model' from spawn_events e
               where e.session_id = s.session_id and e.event_name = 'execution.started'
               order by e.session_seq desc limit 1), 'default') as model,
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
             or c.idempotency_key like 'deliver-%')) as dispatched,
    (select coalesce(jsonb_agg(distinct tag), '[]'::jsonb)
       from spawn_commands c, jsonb_array_elements_text(c.action -> 'tags') tag
      where c.session_id = s.session_id and c.principal = 'spawn-dispatch'
        and c.succeeded and jsonb_typeof(c.action -> 'tags') = 'array') as told,
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
    r") as resumed_young
  from agent_status s
  join spawn_session_state p using (session_id)
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

/// Commands into one session, newest first, with who sent them.
/// `$1` session, `$2` limit.
pub(crate) const SESSION_COMMANDS: &str =
    "select coalesce(jsonb_agg(row_to_json(c) order by c.command_id desc), '[]'::jsonb)
                   from (select id as command_id, principal, action ->> 'type' as action,
                                left(coalesce(action ->> 'prompt', action ->> 'text'), 600) as text,
                                state, succeeded, error, created_at_ms, finished_at_ms
                           from spawn_commands where session_id = $1
                          order by id desc limit $2) c";

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

impl PostgresStore {
    /// Files main's merges changed in `session`'s branch since the last
    /// delivery the worker was typed ([`UNTOLD_MERGES`]).
    pub fn untold_merges(&self, session: &str) -> Result<Vec<String>, StoreError> {
        let files = self.query_json(UNTOLD_MERGES, &[&session])?;
        Ok(serde_json::from_value(files).unwrap_or_default())
    }

    /// Facts for the dispatcher, as JSON rows.
    pub fn dispatch_facts(&self) -> Result<Value, StoreError> {
        self.query_json(DISPATCH_FACTS, &[&spawn_core::bridge::MAY_HAVE_TYPED])
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
