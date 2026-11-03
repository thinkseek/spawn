//! Event-to-view projection.
//!
//! Folding is pure and IO-free. A [`ViewStore`] applies idempotent patches, so
//! restarting from the last committed global position never duplicates rows.

// Pedantic doc-bookkeeping lints: public items carry purposeful docs, but
// mandatory Errors/Panics sections and identifier backticks add noise.
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::doc_markdown
)]
#![allow(clippy::module_name_repetitions, clippy::must_use_candidate)]
mod rows;
mod store;

use std::sync::Arc;

use spawn_store::EventLog;
use spawn_types::event::{EventRecord, GlobalPosition};

pub use rows::{
    AgentArtifactRow, AgentExecutionRow, AgentRelationRow, AgentStateRow, AgentTaskRow,
    TimelineRow, event_kind_name,
};
pub use store::{InMemoryViewStore, ViewPatch, ViewStore, ViewTables};

/// Bounded number of events fetched per global scan batch.
const BATCH_LIMIT: usize = 512;

/// Pure fold of one event batch into view patches and the new cursor.
#[must_use]
pub fn fold_batch(
    records: &[EventRecord],
    previous_cursor: GlobalPosition,
) -> (Vec<ViewPatch>, GlobalPosition) {
    let mut patches = Vec::with_capacity(records.len());
    let mut cursor = previous_cursor;
    for record in records {
        patches.extend(project_record(record));
        cursor = record.global_position;
    }
    (patches, cursor)
}

fn project_record(record: &EventRecord) -> Vec<ViewPatch> {
    let event = &record.event;
    let session_id = event.session_id.clone();
    let timeline = TimelineRow {
        global_position: record.global_position,
        session_id: session_id.clone(),
        event_name: event_kind_name(&event.kind).to_string(),
        occurred_at: record.recorded_at,
    };
    let mut patches = vec![ViewPatch::UpsertTimeline(timeline)];

    match &event.kind {
        spawn_types::event::EventKind::ObservationRecorded { observation } => {
            patches.push(ViewPatch::UpsertState(AgentStateRow {
                session_id,
                state: observation.state,
                reason: observation.reason.clone(),
                confidence: observation.confidence,
                updated_at: record.recorded_at,
            }));
        }
        spawn_types::event::EventKind::ExecutionStarted { engine, .. } => {
            if let Some(execution_id) = event.execution_id.clone() {
                patches.push(ViewPatch::UpsertExecution(AgentExecutionRow {
                    execution_id,
                    session_id,
                    engine: *engine,
                    started_at: record.recorded_at,
                    ended_at: None,
                    exit_code: None,
                }));
            }
        }
        spawn_types::event::EventKind::ExecutionExited { exit_code } => {
            if let Some(execution_id) = &event.execution_id {
                patches.push(ViewPatch::CloseExecution {
                    execution_id: execution_id.clone(),
                    ended_at: record.recorded_at,
                    exit_code: *exit_code,
                });
            }
        }
        spawn_types::event::EventKind::ArtifactCommitted { .. } => {
            if let Some(artifact) = &event.artifact {
                patches.push(ViewPatch::AppendArtifact(AgentArtifactRow {
                    session_id,
                    project: artifact.project.clone(),
                    branch: artifact.branch.clone(),
                    commit: artifact.commit.clone(),
                    committed_at: record.recorded_at,
                }));
            }
        }
        spawn_types::event::EventKind::ToolStarted { tool_name, .. } => {
            patches.push(ViewPatch::UpsertTask(AgentTaskRow {
                session_id,
                correlation_context: tool_name.clone(),
                open: true,
                updated_at: record.recorded_at,
            }));
        }
        spawn_types::event::EventKind::ToolCompleted { .. }
        | spawn_types::event::EventKind::ToolFailed { .. } => {
            let name = match &event.kind {
                spawn_types::event::EventKind::ToolFailed { error_kind, .. } => error_kind.clone(),
                _ => "tool".to_string(),
            };
            patches.push(ViewPatch::UpsertTask(AgentTaskRow {
                session_id,
                correlation_context: name,
                open: false,
                updated_at: record.recorded_at,
            }));
        }
        spawn_types::event::EventKind::RelationLinked {
            parent_agent_id,
            parent_session_id,
        } => patches.push(ViewPatch::UpsertRelation(AgentRelationRow {
            child_session_id: session_id,
            parent_agent_id: parent_agent_id.clone(),
            parent_session_id: parent_session_id.clone(),
        })),
        _ => {}
    }
    patches
}

/// Drives projection over an [`EventLog`] into a [`ViewStore`].
pub struct Projector {
    log: Arc<dyn EventLog>,
    store: Arc<dyn ViewStore>,
    cursor: std::sync::Mutex<GlobalPosition>,
}

impl Projector {
    /// Create a projector at the given start position.
    #[must_use]
    pub fn new(
        log: Arc<dyn EventLog>,
        store: Arc<dyn ViewStore>,
        start_position: GlobalPosition,
    ) -> Self {
        Self {
            log,
            store,
            cursor: std::sync::Mutex::new(start_position),
        }
    }

    /// Process all available events and return the advanced cursor.
    ///
    /// # Errors
    /// Propagates storage failures.
    pub fn process_available(&self) -> Result<GlobalPosition, spawn_store::StoreError> {
        loop {
            let current = *self.cursor.lock().expect("cursor lock is not poisoned");
            let records = self.log.read_global(current, BATCH_LIMIT)?;
            if records.is_empty() {
                return Ok(current);
            }
            let (patches, next) = fold_batch(&records, current);
            for patch in patches {
                self.store
                    .apply(patch)
                    .map_err(|error| spawn_store::StoreError::Unavailable(error.to_string()))?;
            }
            *self.cursor.lock().expect("cursor lock is not poisoned") = next;
            if records.len() < BATCH_LIMIT {
                return Ok(next);
            }
        }
    }
}
