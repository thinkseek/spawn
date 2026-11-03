use spawn_types::clock::Timestamp;
use spawn_types::engine::Engine;
use spawn_types::event::EventKind;
use spawn_types::identifiers::{AgentId, BranchName, CommitId, ExecutionId, SessionId};
use spawn_types::observation::{Confidence, SemanticState, WaitReason};

/// Stable dotted name of an event kind, matching the wire protocol.
#[must_use]
pub fn event_kind_name(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::NativeSessionStarted { .. } => "session.native_started",
        EventKind::NativeSessionEnded { .. } => "session.native_ended",
        EventKind::MessageReceived { .. } => "message.received",
        EventKind::ToolStarted { .. } => "tool.started",
        EventKind::ToolCompleted { .. } => "tool.completed",
        EventKind::ToolFailed { .. } => "tool.failed",
        EventKind::TurnStopped { .. } => "turn.stopped",
        EventKind::TurnFailed { .. } => "turn.failed",
        EventKind::ExecutionStarted { .. } => "execution.started",
        EventKind::ExecutionExited { .. } => "execution.exited",
        EventKind::ExecutionLost { .. } => "execution.lost",
        EventKind::ArtifactCommitted { .. } => "artifact.committed",
        EventKind::CommandAccepted { .. } => "command.accepted",
        EventKind::CommandClaimed { .. } => "command.claimed",
        EventKind::CommandFinished { .. } => "command.finished",
        EventKind::ObservationRecorded { .. } => "observation.recorded",
        EventKind::RelationLinked { .. } => "relation.linked",
    }
}

/// Current semantic state of one session.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentStateRow {
    /// Owning session.
    pub session_id: SessionId,
    /// Latest semantic state.
    pub state: SemanticState,
    /// Structured reason.
    pub reason: Option<WaitReason>,
    /// Latest confidence.
    pub confidence: Confidence,
    /// Last update instant.
    pub updated_at: Timestamp,
}

/// One ordered timeline entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineRow {
    /// Global order position.
    pub global_position: spawn_types::event::GlobalPosition,
    /// Owning session.
    pub session_id: SessionId,
    /// Dotted event name.
    pub event_name: String,
    /// Event instant.
    pub occurred_at: Timestamp,
}

/// Open or closed tool task derived from tool lifecycle events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTaskRow {
    /// Owning session.
    pub session_id: SessionId,
    /// Tool or correlation context.
    pub correlation_context: String,
    /// Whether the task is still open.
    pub open: bool,
    /// Last update instant.
    pub updated_at: Timestamp,
}

/// One committed artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentArtifactRow {
    /// Owning session.
    pub session_id: SessionId,
    /// LayerFS project.
    pub project: String,
    /// Owning branch.
    pub branch: BranchName,
    /// Commit identifier.
    pub commit: CommitId,
    /// Commit instant.
    pub committed_at: Timestamp,
}

/// One execution lifecycle row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentExecutionRow {
    /// Execution identifier.
    pub execution_id: ExecutionId,
    /// Owning session.
    pub session_id: SessionId,
    /// Engine used.
    pub engine: Engine,
    /// Start instant.
    pub started_at: Timestamp,
    /// End instant when finished.
    pub ended_at: Option<Timestamp>,
    /// OS exit code when available.
    pub exit_code: Option<i32>,
}

/// Parent-child relation between sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRelationRow {
    /// Child session.
    pub child_session_id: SessionId,
    /// Parent agent.
    pub parent_agent_id: AgentId,
    /// Parent session.
    pub parent_session_id: SessionId,
}
