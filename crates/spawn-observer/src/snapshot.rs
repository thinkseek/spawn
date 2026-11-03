use serde::{Deserialize, Serialize};
use spawn_types::clock::Timestamp;
use spawn_types::event::{EventKind, EventRecord, SessionSeq};
use spawn_types::identifiers::SessionId;

/// Fact-only view of one session at one point in time.
///
/// The boolean fields are independent fact flags, not a state enum; keeping
/// them separate lets the fold update one dimension without coupling.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SessionSnapshot {
    /// Session this snapshot describes.
    pub session_id: Option<SessionId>,
    /// Sequence of the latest evidence event.
    pub based_on_seq: SessionSeq,
    /// Number of tool calls started without a matching completion.
    pub open_tool_count: u32,
    /// Name of the most recently started tool.
    pub last_tool_name: Option<String>,
    /// Whether an execution is currently considered active.
    pub execution_active: bool,
    /// Whether the process already exited cleanly.
    pub execution_exited: bool,
    /// Whether a permission notification was observed and not cleared.
    pub permission_pending: bool,
    /// Whether the latest turn stopped without new activity.
    pub turn_stopped: bool,
    /// Timestamp of the latest evidence event.
    pub last_activity_at: Timestamp,
}

impl SessionSnapshot {
    /// Fold a contiguous slice of non-observer event records.
    pub fn fold<'a>(records: impl IntoIterator<Item = &'a EventRecord>) -> Self {
        let mut snapshot = Self::default();
        for record in records {
            snapshot.session_id = Some(record.event.session_id.clone());
            snapshot.based_on_seq = record.session_seq;
            snapshot.last_activity_at = record.recorded_at;
            match &record.event.kind {
                EventKind::ToolStarted { tool_name, .. } => {
                    snapshot.open_tool_count = snapshot.open_tool_count.saturating_add(1);
                    snapshot.last_tool_name = Some(tool_name.clone());
                    snapshot.turn_stopped = false;
                    snapshot.permission_pending = false;
                }
                EventKind::ToolCompleted { .. } | EventKind::ToolFailed { .. } => {
                    snapshot.open_tool_count = snapshot.open_tool_count.saturating_sub(1);
                }
                EventKind::ExecutionStarted { .. } => {
                    snapshot.execution_active = true;
                    snapshot.execution_exited = false;
                    snapshot.turn_stopped = false;
                }
                EventKind::ExecutionExited { .. } | EventKind::ExecutionLost { .. } => {
                    snapshot.execution_active = false;
                    snapshot.execution_exited = true;
                }
                EventKind::MessageReceived { prompt_ref } => {
                    if prompt_ref.as_deref() == Some("permission_notification") {
                        snapshot.permission_pending = true;
                    }
                    snapshot.turn_stopped = false;
                }
                EventKind::TurnStopped { .. } | EventKind::TurnFailed { .. } => {
                    snapshot.turn_stopped = true;
                }
                EventKind::NativeSessionEnded { .. } => {
                    snapshot.execution_active = false;
                }
                _ => {}
            }
        }
        snapshot
    }
}
