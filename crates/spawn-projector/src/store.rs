use std::collections::HashMap;
use std::sync::Mutex;

use spawn_types::identifiers::{CommitId, ExecutionId, SessionId};
use thiserror::Error;

use crate::rows::{
    AgentArtifactRow, AgentExecutionRow, AgentRelationRow, AgentStateRow, AgentTaskRow, TimelineRow,
};

/// Errors raised while applying view patches.
#[derive(Debug, Error)]
pub enum ViewError {
    /// A patch referenced an unknown row.
    #[error("view row not found: {0}")]
    NotFound(String),
    /// Backend storage failed.
    #[error("view store unavailable: {0}")]
    Unavailable(String),
}

/// Idempotent patch applied to read-model tables.
#[derive(Debug, Clone, PartialEq)]
pub enum ViewPatch {
    /// Insert or replace the latest state row.
    UpsertState(AgentStateRow),
    /// Append one immutable timeline entry.
    UpsertTimeline(TimelineRow),
    /// Insert or replace a task row.
    UpsertTask(AgentTaskRow),
    /// Append one artifact row.
    AppendArtifact(AgentArtifactRow),
    /// Insert an execution row.
    UpsertExecution(AgentExecutionRow),
    /// Set terminal fields on an existing execution.
    CloseExecution {
        /// Execution to close.
        execution_id: ExecutionId,
        /// End instant.
        ended_at: spawn_types::clock::Timestamp,
        /// Exit code.
        exit_code: Option<i32>,
    },
    /// Insert or replace a relation row.
    UpsertRelation(AgentRelationRow),
}

/// All materialized read-model tables.
#[derive(Debug, Default, Clone)]
pub struct ViewTables {
    /// Latest state keyed by session.
    pub states: HashMap<SessionId, AgentStateRow>,
    /// Timeline in arrival order.
    pub timeline: Vec<TimelineRow>,
    /// Task rows keyed by session.
    pub tasks: HashMap<SessionId, AgentTaskRow>,
    /// Artifact rows in arrival order.
    pub artifacts: Vec<AgentArtifactRow>,
    /// Execution rows keyed by execution id.
    pub executions: HashMap<ExecutionId, AgentExecutionRow>,
    /// Relation rows keyed by child session.
    pub relations: HashMap<SessionId, AgentRelationRow>,
}

/// Sink that durably applies view patches.
pub trait ViewStore: Send + Sync {
    /// Apply one idempotent patch.
    fn apply(&self, patch: ViewPatch) -> Result<(), ViewError>;
}

/// In-memory [`ViewStore`] used by tests and local runs.
#[derive(Default)]
pub struct InMemoryViewStore {
    tables: Mutex<ViewTables>,
}

impl InMemoryViewStore {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Clone a consistent snapshot of every table.
    pub fn snapshot(&self) -> ViewTables {
        self.tables
            .lock()
            .expect("view lock is not poisoned")
            .clone()
    }
}

impl ViewStore for InMemoryViewStore {
    fn apply(&self, patch: ViewPatch) -> Result<(), ViewError> {
        let mut tables = self.tables.lock().expect("view lock is not poisoned");
        match patch {
            ViewPatch::UpsertState(row) => {
                tables.states.insert(row.session_id.clone(), row);
            }
            ViewPatch::UpsertTimeline(row) => tables.timeline.push(row),
            ViewPatch::UpsertTask(row) => {
                tables.tasks.insert(row.session_id.clone(), row);
            }
            ViewPatch::AppendArtifact(row) => tables.artifacts.push(row),
            ViewPatch::UpsertExecution(row) => {
                tables.executions.insert(row.execution_id.clone(), row);
            }
            ViewPatch::CloseExecution {
                execution_id,
                ended_at,
                exit_code,
            } => {
                let row = tables
                    .executions
                    .get_mut(&execution_id)
                    .ok_or_else(|| ViewError::NotFound(execution_id.as_str().to_string()))?;
                row.ended_at = Some(ended_at);
                row.exit_code = exit_code;
            }
            ViewPatch::UpsertRelation(row) => {
                tables.relations.insert(row.child_session_id.clone(), row);
            }
        }
        Ok(())
    }
}

/// Helper used by tests and SQL adapters to dedupe artifact commits.
#[allow(dead_code)]
#[must_use]
pub fn contains_commit(tables: &ViewTables, commit: &CommitId) -> bool {
    tables.artifacts.iter().any(|row| row.commit == *commit)
}
