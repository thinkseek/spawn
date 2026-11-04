//! Unified event vocabulary and stored event records.
//!
//! Every producer (engine hooks, Codex stream, Bridge, Observer,
//! SQL gateway) writes the same [`EventKind`] values. The store assigns the
//! per-session sequence, the global position and the record time; producer
//! clocks never decide order.

use serde::{Deserialize, Serialize};

use crate::id::{
    AgentId, BranchName, CommandId, CommitId, ExecutionId, SessionId, TerminalName, WorkspaceId,
};
use crate::observation::Observation;
use crate::screen::ScreenState;
use crate::time::Timestamp;

/// Native agent CLI behind an execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Engine {
    /// Claude Code, interactive in a terminal, events through hooks.
    Claude,
    /// Codex `exec --json`, events through its JSONL standard output.
    Codex,
}

/// System that produced a fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Claude Code hooks.
    Claude,
    /// Codex JSONL stream.
    Codex,
    /// Agent Bridge lifecycle facts.
    Bridge,
    /// Observer judgements.
    Observer,
    /// SQL gateway command intake and completion.
    Gateway,
    /// The control stream: missions, tasks, residents and landings
    /// (docs/fleet-architecture.md §10.1).
    Control,
}

impl Source {
    /// Wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Bridge => "bridge",
            Self::Observer => "observer",
            Self::Gateway => "gateway",
            Self::Control => "control",
        }
    }

    /// Parse a wire name.
    pub fn parse(text: &str) -> Option<Self> {
        [
            Self::Claude,
            Self::Codex,
            Self::Bridge,
            Self::Observer,
            Self::Gateway,
            Self::Control,
        ]
        .into_iter()
        .find(|source| source.as_str() == text)
    }
}

impl Engine {
    /// Wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    /// Parse a wire name.
    pub fn parse(text: &str) -> Option<Self> {
        [Self::Claude, Self::Codex]
            .into_iter()
            .find(|engine| engine.as_str() == text)
    }
}

/// Why a checkpoint commit was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointReason {
    /// A tool call finished.
    ToolCompleted,
    /// The model finished a turn.
    TurnStopped,
    /// The execution ended.
    ExecutionEnded,
}

/// How an execution ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndCause {
    /// The engine process exited by itself.
    Exited,
    /// A stop command terminated the execution.
    Stopped,
    /// The terminal disappeared without a recorded exit.
    Lost,
}

/// One workspace commit linked to the event that records why it happened.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArtifactRef {
    /// Store name.
    pub project: String,
    /// Branch that received the commit.
    pub branch: BranchName,
    /// Commit identifier.
    pub commit: CommitId,
}

/// Source-independent event vocabulary. Wire names are stable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "name", content = "detail")]
pub enum EventKind {
    /// A native engine session started or resumed.
    #[serde(rename = "session.native_started")]
    NativeSessionStarted {
        /// Engine that owns the native session.
        engine: Engine,
        /// Native session identifier, used for resume.
        native_session_id: String,
        /// Transcript path when the engine reports one.
        transcript_path: Option<String>,
    },
    /// A native engine session ended.
    #[serde(rename = "session.native_ended")]
    NativeSessionEnded {
        /// Engine-reported reason.
        reason: Option<String>,
    },
    /// The session became a child of another session.
    #[serde(rename = "session.linked")]
    SessionLinked {
        /// Parent session.
        parent_session_id: SessionId,
    },
    /// A prompt reached the engine.
    #[serde(rename = "message.received")]
    MessageReceived,
    /// The engine asked for attention, for example a permission prompt.
    #[serde(rename = "notification.received")]
    NotificationReceived {
        /// Engine notification type, for example `permission_prompt`.
        notification_type: String,
    },
    /// A tool call started.
    #[serde(rename = "tool.started")]
    ToolStarted {
        /// Engine tool name.
        tool_name: String,
        /// Engine tool call identifier.
        tool_use_id: String,
    },
    /// A tool call succeeded.
    #[serde(rename = "tool.completed")]
    ToolCompleted {
        /// Engine tool call identifier.
        tool_use_id: String,
        /// Reported duration.
        duration_ms: Option<u64>,
    },
    /// A tool call failed.
    #[serde(rename = "tool.failed")]
    ToolFailed {
        /// Engine tool call identifier.
        tool_use_id: String,
        /// Engine error text, truncated.
        error: String,
    },
    /// The model finished one turn. This is not task completion.
    #[serde(rename = "turn.stopped")]
    TurnStopped {
        /// Input tokens when reported.
        input_tokens: Option<u64>,
        /// Output tokens when reported.
        output_tokens: Option<u64>,
    },
    /// The turn ended with an engine or API failure.
    #[serde(rename = "turn.failed")]
    TurnFailed {
        /// Engine error text, truncated.
        error: String,
    },
    /// An execution started in a terminal against a workspace.
    #[serde(rename = "execution.started")]
    ExecutionStarted {
        /// Engine of the execution.
        engine: Engine,
        /// Terminal session that hosts the process.
        terminal: TerminalName,
        /// Mounted workspace.
        workspace: WorkspaceId,
        /// Command that created the execution.
        command_id: CommandId,
    },
    /// The execution ended.
    #[serde(rename = "execution.ended")]
    ExecutionEnded {
        /// How it ended.
        cause: EndCause,
        /// Process exit code when the OS reported one.
        exit_code: Option<i32>,
    },
    /// The workspace store persisted a checkpoint.
    #[serde(rename = "artifact.committed")]
    ArtifactCommitted {
        /// Trigger of the checkpoint.
        reason: CheckpointReason,
        /// Commit reference.
        artifact: ArtifactRef,
    },
    /// The SQL gateway accepted a command.
    #[serde(rename = "command.accepted")]
    CommandAccepted {
        /// Accepted command.
        command_id: CommandId,
    },
    /// A worker finished a command.
    #[serde(rename = "command.finished")]
    CommandFinished {
        /// Finished command.
        command_id: CommandId,
        /// Whether the command succeeded.
        succeeded: bool,
        /// Failure text when it did not.
        error: Option<String>,
    },
    /// The Bridge read the tail of a Claude pane.
    #[serde(rename = "screen.observed")]
    ScreenObserved {
        /// Detected pane state.
        state: ScreenState,
        /// Line that decided the state.
        evidence: String,
    },
    /// The Observer recorded a semantic judgement.
    #[serde(rename = "observation.recorded")]
    ObservationRecorded {
        /// Judgement.
        observation: Observation,
    },
    /// One record of the control stream: a mission, task, resident or
    /// landing fact. The record's `type` names it; the database folds the
    /// records into the `mission_*` tables (docs/fleet-architecture.md
    /// §10.1). It belongs to no agent, so nothing observes or bridges it.
    #[serde(rename = "control.recorded")]
    ControlRecorded {
        /// The record, `{"type": ..., ...}`.
        record: serde_json::Value,
    },
}

impl EventKind {
    /// Stable dotted wire name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::NativeSessionStarted { .. } => "session.native_started",
            Self::NativeSessionEnded { .. } => "session.native_ended",
            Self::SessionLinked { .. } => "session.linked",
            Self::MessageReceived => "message.received",
            Self::NotificationReceived { .. } => "notification.received",
            Self::ToolStarted { .. } => "tool.started",
            Self::ToolCompleted { .. } => "tool.completed",
            Self::ToolFailed { .. } => "tool.failed",
            Self::TurnStopped { .. } => "turn.stopped",
            Self::TurnFailed { .. } => "turn.failed",
            Self::ExecutionStarted { .. } => "execution.started",
            Self::ExecutionEnded { .. } => "execution.ended",
            Self::ArtifactCommitted { .. } => "artifact.committed",
            Self::CommandAccepted { .. } => "command.accepted",
            Self::CommandFinished { .. } => "command.finished",
            Self::ScreenObserved { .. } => "screen.observed",
            Self::ObservationRecorded { .. } => "observation.recorded",
            Self::ControlRecorded { .. } => "control.recorded",
        }
    }

    /// Whether this is a control-stream record rather than a fact about an
    /// agent's session.
    pub fn is_control(&self) -> bool {
        matches!(self, Self::ControlRecorded { .. })
    }

    /// Checkpoint the Bridge must take after this event, if any.
    pub fn checkpoint_reason(&self) -> Option<CheckpointReason> {
        match self {
            Self::ToolCompleted { .. } | Self::ToolFailed { .. } => {
                Some(CheckpointReason::ToolCompleted)
            }
            Self::TurnStopped { .. } | Self::TurnFailed { .. } => {
                Some(CheckpointReason::TurnStopped)
            }
            _ => None,
        }
    }
}

/// Per-session sequence, starting at one.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct SessionSeq(pub u64);

/// Store-wide position, used by global readers and CDC cursors.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct GlobalPosition(pub u64);

/// Event content before the store assigns order and time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewEvent {
    /// Owning agent.
    pub agent_id: AgentId,
    /// Owning logical session.
    pub session_id: SessionId,
    /// Owning execution for execution-scoped facts.
    pub execution_id: Option<ExecutionId>,
    /// Producer.
    pub source: Source,
    /// Producer key. A repeated `(session, source, key)` is stored once.
    pub source_event_id: Option<String>,
    /// Event content.
    pub kind: EventKind,
}

/// Stored, immutable event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventRecord {
    /// Store-wide position. The textual event id is `evt_<position>`.
    pub position: GlobalPosition,
    /// Per-session sequence.
    pub session_seq: SessionSeq,
    /// Store-assigned record time.
    pub recorded_at: Timestamp,
    /// Event content.
    #[serde(flatten)]
    pub event: NewEvent,
}

/// Result of an append.
#[derive(Debug, Clone, PartialEq)]
pub struct Appended {
    /// Stored record: the new one, or the existing one for a repeated key.
    pub record: EventRecord,
    /// False when the source key already existed.
    pub inserted: bool,
}
