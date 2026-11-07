//! Read side of one session, over the `agent_*` views, so the SQL
//! definitions stay the one source of each fold.
//!
//! ponytail: methods on the store, no port; promote to a trait when a second
//! reader (for example an HTTP read service) appears.

use spawn_core::event::{GlobalPosition, SessionSeq};
use spawn_core::id::{AgentId, CommandId, ExecutionId, SessionId, TerminalName};
use spawn_core::observation::Observation;
use spawn_core::port::StoreError;
use spawn_core::time::Timestamp;

use postgres::Row;

use crate::PostgresStore;
use crate::row::{
    agent_id, bigint, corrupt, execution_id, global_position, session_id, session_seq,
};

/// Columns of `agent_state` that make one `SessionState`.
macro_rules! session_state_columns {
    () => {
        "session_id, agent_id, observation, session_seq, event_id, recorded_at_ms"
    };
}

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

/// Log entries after a position, store-wide, in log order. A screen
/// observation's evidence (the pane's text) is left out: the readers of
/// the whole log draw what happened, not what a pane said.
/// `$1` after, `$2` limit.
pub(crate) const LOG_AFTER: &str =
    "select id, agent_id, session_id, execution_id, source, event_name,
            case when event_name = 'screen.observed' then (kind->'detail') - 'evidence'
                 else kind->'detail' end as detail,
            recorded_at_ms
       from spawn_events
      where id > $1
      order by id
      limit $2";

/// The first event at or after a position. `$1` position.
pub(crate) const FIRST_FROM: &str =
    "select id, recorded_at_ms from spawn_events where id >= $1 order by id limit 1";

/// The last position in the log.
pub(crate) const LAST_POSITION: &str = "select coalesce(max(id), 0) as id from spawn_events";

/// Everything the supervisor's liveness rule decides from.
/// `$1` session, `$2` principal of the reviver.
///
/// `failures` counts, since the latest execution that did not die young
/// (`died_young!`), the reviver's failed commands and the executions its
/// succeeded commands started that died young: a revival is judged by
/// whether its engine stayed up, not by whether it launched.
pub(crate) const LIVENESS_FACTS: &str = concat!(
    // The session's executions once: every use below reads this row set, and
    // the view behind it (`agent_executions`) is a scan of the log with a
    // lateral read of each execution's end, so evaluating it once per call
    // rather than once per question keeps the loop's liveness read to one
    // scan of the log per session.
    "with execs as materialized (
                    select e.execution_id, e.started_at_ms, e.ended_at_ms, e.cause,
                           e.duration_ms, e.exit_code, e.command_id
                      from agent_executions e
                     where e.session_id = $1
                 ), cmds as materialized (
                    select c.id, c.principal, c.action, c.state, c.succeeded,
                           c.created_at_ms, c.finished_at_ms
                      from spawn_commands c where c.session_id = $1
                 ), last_exec as (
                    select execution_id, started_at_ms, ended_at_ms, cause from execs
                     order by started_at_ms desc, execution_id desc limit 1
                 ), last_cmd as (
                    select c.action ->> 'type' as kind
                      from cmds c
                     order by c.id desc limit 1
                 ), healthy as (
                    select coalesce(max(x.started_at_ms), 0) as since
                      from execs x
                     where not ",
    died_young!("x"),
    "
                 ), died as (
                    select x.ended_at_ms
                      from execs x
                      join cmds c on c.id = x.command_id
                     where c.principal = $2 and c.succeeded
                       and x.started_at_ms > (select since from healthy) and ",
    died_young!("x"),
    "
                 )
                 select
                    exists (select 1 from cmds c where c.state <> 'finished') as open_command,
                    (select execution_id from last_exec) as execution_id,
                    coalesce((select ended_at_ms is null from last_exec), false) as live,
                    (select cause from last_exec) as cause,
                    (select kind from last_cmd) as last_kind,
                    exists (select 1 from spawn_events e
                             where e.session_id = $1
                               and e.event_name = 'session.native_started') as native_bound,
                    (select count(*) from cmds c
                      where c.principal = $2 and c.succeeded = false
                        and c.created_at_ms > (select since from healthy)
                    ) + (select count(*) from died) as failures,
                    greatest(
                      (select max(c.finished_at_ms) from cmds c
                        where c.principal = $2 and c.succeeded = false),
                      (select max(ended_at_ms) from died)
                    ) as last_failure_ms"
);

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

/// The newest execution and its workspace. `$1` session.
pub(crate) const LATEST_TERMINAL: &str = "select execution_id,
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

/// Current judgement of one session, from `agent_state`.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionState {
    /// Session.
    pub session_id: SessionId,
    /// Agent that owns the session.
    pub agent_id: AgentId,
    /// Latest judgement.
    pub observation: Observation,
    /// Sequence of the observation event.
    pub session_seq: SessionSeq,
    /// Global position of the observation event.
    pub event_id: GlobalPosition,
    /// Time the observation was recorded.
    pub recorded_at: Timestamp,
}

/// One non-Observer event, from `agent_timeline`.
#[derive(Debug, Clone, PartialEq)]
pub struct TimelineEntry {
    /// Sequence in the session; the keyset cursor.
    pub session_seq: SessionSeq,
    /// Global position.
    pub event_id: GlobalPosition,
    /// Execution the event belongs to.
    pub execution_id: Option<ExecutionId>,
    /// Writer of the event, as stored: a source this build no longer knows
    /// still shows.
    pub source: String,
    /// Dotted event name.
    pub event_name: String,
    /// Event payload without the name.
    pub detail: Option<serde_json::Value>,
    /// Time the event was recorded.
    pub recorded_at: Timestamp,
}

/// One event of the whole log, from [`PostgresStore::log_after`].
#[derive(Debug, Clone, PartialEq)]
pub struct LogEntry {
    /// Global position; the keyset cursor.
    pub position: GlobalPosition,
    /// Owning agent, as stored.
    pub agent_id: String,
    /// Owning session, as stored.
    pub session_id: String,
    /// Owning execution, as stored.
    pub execution_id: Option<String>,
    /// Writer of the event, as stored.
    pub source: String,
    /// Dotted event name.
    pub event_name: String,
    /// Event payload without the name.
    pub detail: Option<serde_json::Value>,
    /// Time the event was recorded.
    pub recorded_at: Timestamp,
}

/// What the supervisor's liveness rule knows about its session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LivenessFacts {
    /// A command for the session is pending or leased.
    pub open_command: bool,
    /// Newest execution.
    pub execution_id: Option<String>,
    /// The newest execution has not ended.
    pub live: bool,
    /// How the newest execution ended.
    pub cause: Option<String>,
    /// Action type of the newest command.
    pub last_kind: Option<String>,
    /// The engine reported a native session at least once.
    pub native_bound: bool,
    /// Failed revivals since the newest execution started.
    pub failures: u32,
    /// When the latest failed revival finished.
    pub last_failure_ms: Option<i64>,
}

/// One execution, from `agent_executions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionRow {
    /// Execution.
    pub execution_id: ExecutionId,
    /// Command that created it.
    pub command_id: Option<CommandId>,
    /// Engine wire name.
    pub engine: Option<String>,
    /// Start time.
    pub started_at: Timestamp,
    /// Time the Bridge ended it; `None` while it runs.
    pub ended_at: Option<Timestamp>,
    /// End cause wire name.
    pub cause: Option<String>,
    /// Process exit code.
    pub exit_code: Option<i32>,
    /// The runner reported an engine exit.
    pub exit_reported: bool,
    /// `ended_at - started_at` in milliseconds.
    pub duration_ms: Option<i64>,
}

/// One tool call without a result, from `agent_open_tools`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenTool {
    /// Execution of the call.
    pub execution_id: Option<ExecutionId>,
    /// Engine tool call identifier.
    pub tool_use_id: String,
    /// Engine tool name.
    pub tool_name: Option<String>,
    /// Sequence of the start event.
    pub session_seq: SessionSeq,
    /// Start time.
    pub started_at: Timestamp,
}

/// Position of a session's log, from `agent_freshness`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Freshness {
    /// Highest session sequence.
    pub last_session_seq: SessionSeq,
    /// Highest global position.
    pub last_event_id: GlobalPosition,
    /// Time of the latest event.
    pub last_event_at: Timestamp,
    /// Number of events.
    pub event_count: u64,
}

/// One `agent_state` row selected with `session_state_columns!`.
fn session_state(row: &Row) -> Result<SessionState, StoreError> {
    Ok(SessionState {
        session_id: session_id(row, "session_id")?,
        agent_id: agent_id(row, "agent_id")?,
        observation: serde_json::from_value(row.get("observation")).map_err(corrupt)?,
        session_seq: session_seq(row, "session_seq"),
        event_id: global_position(row, "event_id"),
        recorded_at: Timestamp(row.get("recorded_at_ms")),
    })
}

impl PostgresStore {
    /// Current judgement of every session, ordered by session.
    pub fn states(&self) -> Result<Vec<SessionState>, StoreError> {
        self.query(STATES, &[])?.iter().map(session_state).collect()
    }

    /// Current judgement of one session.
    pub fn state(&self, session: &SessionId) -> Result<Option<SessionState>, StoreError> {
        self.query_opt(STATE, &[&session.as_str()])?
            .as_ref()
            .map(session_state)
            .transpose()
    }

    /// Up to `limit` timeline entries after `after`, in sequence order.
    pub fn timeline(
        &self,
        session: &SessionId,
        after: SessionSeq,
        limit: usize,
    ) -> Result<Vec<TimelineEntry>, StoreError> {
        self.query(
            TIMELINE,
            &[&session.as_str(), &bigint(after.0), &bigint(limit)],
        )?
        .iter()
        .map(|row| {
            Ok(TimelineEntry {
                session_seq: session_seq(row, "session_seq"),
                event_id: global_position(row, "id"),
                execution_id: execution_id(row)?,
                source: row.get("source"),
                event_name: row
                    .get::<_, Option<String>>("event_name")
                    .unwrap_or_default(),
                detail: row.get("detail"),
                recorded_at: Timestamp(row.get("recorded_at_ms")),
            })
        })
        .collect()
    }

    /// Up to `limit` events of the whole log after `after`, in log order.
    pub fn log_after(
        &self,
        after: GlobalPosition,
        limit: usize,
    ) -> Result<Vec<LogEntry>, StoreError> {
        self.query(LOG_AFTER, &[&bigint(after.0), &bigint(limit)])?
            .iter()
            .map(|row| {
                Ok(LogEntry {
                    position: global_position(row, "id"),
                    agent_id: row.get("agent_id"),
                    session_id: row.get("session_id"),
                    execution_id: row.get("execution_id"),
                    source: row.get("source"),
                    event_name: row
                        .get::<_, Option<String>>("event_name")
                        .unwrap_or_default(),
                    detail: row.get("detail"),
                    recorded_at: Timestamp(row.get("recorded_at_ms")),
                })
            })
            .collect()
    }

    /// The position just before the first event recorded at or after `at`:
    /// reading the log after it starts at that time. The last position when
    /// nothing is that recent.
    ///
    /// Positions are assigned in record order, so record times rise with
    /// them; a binary search over the primary key finds the boundary in a
    /// few dozen index reads, with no index on the time.
    pub fn position_before(&self, at: Timestamp) -> Result<GlobalPosition, StoreError> {
        let last = self.query_one(LAST_POSITION, &[])?.get::<_, i64>("id");
        // Every position below `low` was recorded before `at`; every one
        // from `high` on at or after it.
        let (mut low, mut high) = (0_i64, last.saturating_add(1));
        while low < high {
            let middle = low + (high - low) / 2;
            match self.query_opt(FIRST_FROM, &[&middle])? {
                Some(row)
                    if row.get::<_, i64>("id") < high
                        && row.get::<_, i64>("recorded_at_ms") < at.0 =>
                {
                    low = row.get::<_, i64>("id") + 1;
                }
                _ => high = middle,
            }
        }
        Ok(GlobalPosition((low - 1).max(0).unsigned_abs()))
    }

    /// Facts the supervisor's liveness rule decides from, all from storage.
    pub fn liveness_facts(
        &self,
        session: &SessionId,
        reviver: &str,
    ) -> Result<LivenessFacts, StoreError> {
        let row = self.query_one_prepared(LIVENESS_FACTS, &[&session.as_str(), &reviver])?;
        Ok(LivenessFacts {
            open_command: row.get("open_command"),
            execution_id: row.get("execution_id"),
            live: row.get("live"),
            cause: row.get("cause"),
            last_kind: row.get("last_kind"),
            native_bound: row.get("native_bound"),
            failures: u32::try_from(row.get::<_, i64>("failures")).unwrap_or(u32::MAX),
            last_failure_ms: row.get("last_failure_ms"),
        })
    }

    /// Commands one principal submitted, newest first, as JSON.
    pub fn commands_by(
        &self,
        principal: &str,
        limit: i64,
    ) -> Result<serde_json::Value, StoreError> {
        self.query_json(COMMANDS_BY, &[&principal, &limit])
    }

    /// The latest task typed into a session: the prompt of its newest start,
    /// resume or input command.
    pub fn latest_task(&self, session: &SessionId) -> Result<Option<String>, StoreError> {
        let row = self.query_opt(LATEST_TASK, &[&session.as_str()])?;
        Ok(row.and_then(|row| row.get("task")))
    }

    /// Terminal and workspace of a session's newest execution. The terminal
    /// is the execution's own ([`TerminalName::for_execution`]), whatever
    /// an older record also wrote.
    pub fn latest_terminal(
        &self,
        session: &SessionId,
    ) -> Result<Option<(TerminalName, String)>, StoreError> {
        let Some(row) = self.query_opt(LATEST_TERMINAL, &[&session.as_str()])? else {
            return Ok(None);
        };
        let Some(execution) = execution_id(&row)? else {
            return Ok(None);
        };
        Ok(row
            .get::<_, Option<String>>("workspace")
            .map(|workspace| (TerminalName::for_execution(&execution), workspace)))
    }

    /// Executions of one session, newest first.
    pub fn executions(&self, session: &SessionId) -> Result<Vec<ExecutionRow>, StoreError> {
        self.query(EXECUTIONS, &[&session.as_str()])?
            .iter()
            .map(|row| {
                Ok(ExecutionRow {
                    execution_id: execution_id(row)?
                        .ok_or_else(|| corrupt("execution.started without execution_id"))?,
                    command_id: row
                        .get::<_, Option<i64>>("command_id")
                        .map(|id| CommandId(id.unsigned_abs())),
                    engine: row.get("engine"),
                    started_at: Timestamp(row.get("started_at_ms")),
                    ended_at: row.get::<_, Option<i64>>("ended_at_ms").map(Timestamp),
                    cause: row.get("cause"),
                    exit_code: row.get("exit_code"),
                    exit_reported: row.get("exit_reported"),
                    duration_ms: row.get("duration_ms"),
                })
            })
            .collect()
    }

    /// Tool calls of one session that have no result, in sequence order.
    pub fn open_tools(&self, session: &SessionId) -> Result<Vec<OpenTool>, StoreError> {
        self.query(OPEN_TOOLS, &[&session.as_str()])?
            .iter()
            .map(|row| {
                Ok(OpenTool {
                    execution_id: execution_id(row)?,
                    tool_use_id: row
                        .get::<_, Option<String>>("tool_use_id")
                        .ok_or_else(|| corrupt("tool.started without tool_use_id"))?,
                    tool_name: row.get("tool_name"),
                    session_seq: session_seq(row, "session_seq"),
                    started_at: Timestamp(row.get("started_at_ms")),
                })
            })
            .collect()
    }

    /// Position of one session's log; `None` when it has no events.
    pub fn freshness(&self, session: &SessionId) -> Result<Option<Freshness>, StoreError> {
        let row = self.query_opt(FRESHNESS, &[&session.as_str()])?;
        Ok(row.map(|row| Freshness {
            last_session_seq: session_seq(&row, "last_session_seq"),
            last_event_id: global_position(&row, "last_event_id"),
            last_event_at: Timestamp(row.get("last_event_at_ms")),
            event_count: row.get::<_, i64>("event_count").unsigned_abs(),
        }))
    }
}
