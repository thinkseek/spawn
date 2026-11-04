//! Read side of one session, over the `agent_*` views, so the SQL
//! definitions stay the one source of each fold.
//!
//! ponytail: methods on the store, no port; promote to a trait when a second
//! reader (for example an HTTP read service) appears.

use spawn_core::event::{GlobalPosition, SessionSeq, Source};
use spawn_core::id::{AgentId, CommandId, ExecutionId, SessionId};
use spawn_core::observation::Observation;
use spawn_core::port::StoreError;
use spawn_core::time::Timestamp;

use postgres::Row;

use crate::PostgresStore;
use crate::queries::sessions;
use crate::row::{
    agent_id, bigint, corrupt, execution_id, global_position, session_id, session_seq, source,
};

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
    /// Writer of the event.
    pub source: Source,
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
    /// Principal of the newest command.
    pub last_principal: Option<String>,
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
        self.query(sessions::STATES, &[])?
            .iter()
            .map(session_state)
            .collect()
    }

    /// Current judgement of one session.
    pub fn state(&self, session: &SessionId) -> Result<Option<SessionState>, StoreError> {
        self.query_opt(sessions::STATE, &[&session.as_str()])?
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
            sessions::TIMELINE,
            &[&session.as_str(), &bigint(after.0), &bigint(limit)],
        )?
        .iter()
        .map(|row| {
            Ok(TimelineEntry {
                session_seq: session_seq(row, "session_seq"),
                event_id: global_position(row, "id"),
                execution_id: execution_id(row)?,
                source: source(row)?,
                event_name: row
                    .get::<_, Option<String>>("event_name")
                    .unwrap_or_default(),
                detail: row.get("detail"),
                recorded_at: Timestamp(row.get("recorded_at_ms")),
            })
        })
        .collect()
    }

    /// Facts the supervisor's liveness rule decides from, all from storage.
    pub fn liveness_facts(
        &self,
        session: &SessionId,
        reviver: &str,
    ) -> Result<LivenessFacts, StoreError> {
        let row = self.query_one(sessions::LIVENESS_FACTS, &[&session.as_str(), &reviver])?;
        Ok(LivenessFacts {
            open_command: row.get("open_command"),
            execution_id: row.get("execution_id"),
            live: row.get("live"),
            cause: row.get("cause"),
            last_principal: row.get("last_principal"),
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
        self.query_json(sessions::COMMANDS_BY, &[&principal, &limit])
    }

    /// The latest task typed into a session: the prompt of its newest start,
    /// resume or input command.
    pub fn latest_task(&self, session: &SessionId) -> Result<Option<String>, StoreError> {
        let row = self.query_opt(sessions::LATEST_TASK, &[&session.as_str()])?;
        Ok(row.and_then(|row| row.get("task")))
    }

    /// Terminal and workspace of a session's newest execution.
    pub fn latest_terminal(
        &self,
        session: &SessionId,
    ) -> Result<Option<(String, String)>, StoreError> {
        let row = self.query_opt(sessions::LATEST_TERMINAL, &[&session.as_str()])?;
        Ok(row.and_then(|row| {
            Some((
                row.get::<_, Option<String>>("terminal")?,
                row.get::<_, Option<String>>("workspace")?,
            ))
        }))
    }

    /// Executions of one session, newest first.
    pub fn executions(&self, session: &SessionId) -> Result<Vec<ExecutionRow>, StoreError> {
        self.query(sessions::EXECUTIONS, &[&session.as_str()])?
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
        self.query(sessions::OPEN_TOOLS, &[&session.as_str()])?
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
        let row = self.query_opt(sessions::FRESHNESS, &[&session.as_str()])?;
        Ok(row.map(|row| Freshness {
            last_session_seq: session_seq(&row, "last_session_seq"),
            last_event_id: global_position(&row, "last_event_id"),
            last_event_at: Timestamp(row.get("last_event_at_ms")),
            event_count: row.get::<_, i64>("event_count").unsigned_abs(),
        }))
    }
}
