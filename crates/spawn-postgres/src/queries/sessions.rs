//! SQL of the per-session read side ([`crate::sessions`]): the `agent_*`
//! views, so the SQL definitions stay the one source of each fold, plus the
//! few facts read straight from the log and the command queue.

/// Current judgement of every session, ordered by session.
pub(crate) const STATES: &str = concat!(
    "select ",
    session_state_columns!(),
    "
                   from agent_state order by session_id"
);

/// Current judgement of one session. `$1` session.
pub(crate) const STATE: &str = concat!(
    "select ",
    session_state_columns!(),
    "
                   from agent_state where session_id = $1"
);

/// Timeline entries after a sequence. `$1` session, `$2` after, `$3` limit.
pub(crate) const TIMELINE: &str =
    "select session_seq, id, execution_id, source, event_name, detail, recorded_at_ms
                   from agent_timeline
                  where session_id = $1 and session_seq > $2
                  order by session_seq
                  limit $3";

/// Everything the supervisor's liveness rule decides from.
/// `$1` session, `$2` principal of the reviver.
pub(crate) const LIVENESS_FACTS: &str = "with last_exec as (
                    select e.execution_id, e.started_at_ms, e.ended_at_ms, e.cause
                      from agent_executions e
                     where e.session_id = $1
                     order by e.started_at_ms desc, e.execution_id desc limit 1
                 ), last_cmd as (
                    select c.principal, c.action ->> 'type' as kind
                      from spawn_commands c where c.session_id = $1
                     order by c.id desc limit 1
                 )
                 select
                    exists (select 1 from spawn_commands c
                             where c.session_id = $1 and c.state <> 'finished') as open_command,
                    (select execution_id from last_exec) as execution_id,
                    coalesce((select ended_at_ms is null from last_exec), false) as live,
                    (select cause from last_exec) as cause,
                    (select principal from last_cmd) as last_principal,
                    (select kind from last_cmd) as last_kind,
                    exists (select 1 from spawn_events e
                             where e.session_id = $1
                               and e.event_name = 'session.native_started') as native_bound,
                    (select count(*) from spawn_commands c
                      where c.session_id = $1 and c.principal = $2 and c.succeeded = false
                        and c.created_at_ms > coalesce((select started_at_ms from last_exec), 0)
                    ) as failures,
                    (select max(c.finished_at_ms) from spawn_commands c
                      where c.session_id = $1 and c.principal = $2 and c.succeeded = false
                    ) as last_failure_ms";

/// Commands one principal submitted, newest first, as one JSON array.
/// `$1` principal, `$2` limit.
pub(crate) const COMMANDS_BY: &str = "select coalesce(jsonb_agg(c order by c.id desc), '[]'::jsonb) as commands
                   from (select id, agent_id, session_id, action, state, succeeded, error, created_at_ms
                           from spawn_commands where principal = $1
                          order by id desc limit $2) c";

/// Prompt of the newest start, resume or input command. `$1` session.
pub(crate) const LATEST_TASK: &str =
    "select coalesce(action ->> 'prompt', action ->> 'text') as task
                   from spawn_commands
                  where session_id = $1
                    and action ->> 'type' in ('start', 'resume', 'input')
                    and coalesce(action ->> 'prompt', action ->> 'text') is not null
                  order by id desc limit 1";

/// Terminal and workspace of the newest execution. `$1` session.
pub(crate) const LATEST_TERMINAL: &str = "select kind -> 'detail' ->> 'terminal' as terminal,
                        kind -> 'detail' ->> 'workspace' as workspace
                   from spawn_events
                  where session_id = $1 and event_name = 'execution.started'
                  order by id desc limit 1";

/// Executions of one session, newest first. `$1` session.
pub(crate) const EXECUTIONS: &str =
    "select execution_id, command_id, engine, started_at_ms, ended_at_ms, cause,
                        exit_code, exit_reported, duration_ms
                   from agent_executions
                  where session_id = $1
                  order by started_at_ms desc, execution_id";

/// Tool calls without a result, in sequence order. `$1` session.
pub(crate) const OPEN_TOOLS: &str =
    "select execution_id, tool_use_id, tool_name, session_seq, started_at_ms
                   from agent_open_tools
                  where session_id = $1
                  order by session_seq";

/// Position of one session's log. `$1` session.
pub(crate) const FRESHNESS: &str =
    "select last_session_seq, last_event_id, last_event_at_ms, event_count
                   from agent_freshness where session_id = $1";
