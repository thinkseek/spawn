use serde::{Deserialize, Serialize};

use crate::clock::Timestamp;
use crate::command::{CommandId, LeaseAttempt};
use crate::engine::Engine;
use crate::identifiers::{AgentId, BranchName, CommitId, ExecutionId, SessionId};
use crate::observation::Observation;

/// Per-session monotonic sequence number, starting at one.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct SessionSeq(pub u64);

/// Store-wide monotonic position used by global consumers and CDC cursors.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct GlobalPosition(pub u64);

/// Unique identifier of an immutable stored event.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventId(String);

impl EventId {
    /// Construct from a store-generated wire value.
    pub fn new(raw: impl Into<String>) -> Result<Self, crate::error::ValidationError> {
        let raw = raw.into();
        crate::identifiers::validate_opaque_label(&raw)?;
        Ok(Self(raw))
    }

    /// Borrow the wire representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// System that produced a raw fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSource {
    /// Structured Claude Code hook payloads.
    ClaudeHooks,
    /// Codex `exec --json` stream events.
    CodexStream,
    /// LayerFS commit and workspace facts.
    LayerFs,
    /// Agent Bridge lifecycle facts.
    Bridge,
    /// Observer semantic judgements.
    Observer,
    /// SQL Gateway command intake.
    Gateway,
    /// Process and heartbeat runtime facts.
    Runtime,
}

/// Reference to a committed LayerFS state.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArtifactRef {
    /// LayerFS project name.
    pub project: String,
    /// Branch that owns the commit.
    pub branch: BranchName,
    /// Commit identifier returned by LayerFS.
    pub commit: CommitId,
}

/// Why a checkpoint commit was created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointReason {
    /// A file-mutating tool call finished.
    ToolCompleted,
    /// The model stopped for the turn.
    TurnFinished,
    /// A periodic sweep detected uncommitted changes.
    PeriodicSweep,
    /// The execution shut down.
    ExecutionShutdown,
}

/// Unified, source-independent event vocabulary.
///
/// Serialized names match the dotted protocol names in the architecture
/// document. New variants must keep their explicit wire name stable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "name", content = "detail")]
pub enum EventKind {
    /// A native engine session started or resumed.
    #[serde(rename = "session.native_started")]
    NativeSessionStarted {
        /// Engine that owns the native session.
        engine: Engine,
        /// Native session identifier used for resume.
        native_session_id: String,
        /// Transcript or rollout file path when available.
        transcript_path: Option<String>,
    },
    /// A native engine session ended.
    #[serde(rename = "session.native_ended")]
    NativeSessionEnded {
        /// Engine-specific end reason text.
        end_reason: Option<String>,
    },
    /// A user prompt reached the engine.
    #[serde(rename = "message.received")]
    MessageReceived {
        /// Reference to the prompt body; inline text stays short.
        prompt_ref: Option<String>,
    },
    /// A tool call started.
    #[serde(rename = "tool.started")]
    ToolStarted {
        /// Engine-specific tool name.
        tool_name: String,
        /// Correlation id shared with the completion event.
        tool_correlation_id: String,
    },
    /// A tool call completed successfully.
    #[serde(rename = "tool.completed")]
    ToolCompleted {
        /// Correlation id from the started event.
        tool_correlation_id: String,
        /// Reported duration in milliseconds when present.
        duration_ms: Option<u64>,
    },
    /// A tool call failed.
    #[serde(rename = "tool.failed")]
    ToolFailed {
        /// Correlation id from the started event.
        tool_correlation_id: String,
        /// Stable error category.
        error_kind: String,
        /// Raw error message retained for diagnosis.
        error_message: String,
    },
    /// The model finished one turn; this is not task completion.
    #[serde(rename = "turn.stopped")]
    TurnStopped {
        /// Input tokens reported by the engine when available.
        input_tokens: Option<u64>,
        /// Output tokens reported by the engine when available.
        output_tokens: Option<u64>,
    },
    /// A turn ended with an engine failure.
    #[serde(rename = "turn.failed")]
    TurnFailed {
        /// Engine-reported failure category.
        failure_kind: String,
    },
    /// A new execution began.
    #[serde(rename = "execution.started")]
    ExecutionStarted {
        /// Engine used by this execution.
        engine: Engine,
        /// Terminal that hosts the process.
        terminal_name: String,
        /// Mounted workspace the process runs against.
        mount_id: String,
    },
    /// The execution process exited.
    #[serde(rename = "execution.exited")]
    ExecutionExited {
        /// Process exit code when the OS supplied one.
        exit_code: Option<i32>,
    },
    /// The execution process disappeared without a clean exit.
    #[serde(rename = "execution.lost")]
    ExecutionLost {
        /// Observed loss reason.
        reason: String,
    },
    /// LayerFS persisted a checkpoint.
    #[serde(rename = "artifact.committed")]
    ArtifactCommitted {
        /// Trigger for the checkpoint.
        reason: CheckpointReason,
    },
    /// Gateway accepted a consumer command.
    #[serde(rename = "command.accepted")]
    CommandAccepted {
        /// Accepted command identifier.
        command_id: CommandId,
    },
    /// A worker leased a command.
    #[serde(rename = "command.claimed")]
    CommandClaimed {
        /// Leased command identifier.
        command_id: CommandId,
        /// Fencing attempt of the lease.
        attempt: LeaseAttempt,
    },
    /// A leased command reached a terminal state.
    #[serde(rename = "command.finished")]
    CommandFinished {
        /// Finished command identifier.
        command_id: CommandId,
        /// Whether the worker reported success.
        succeeded: bool,
    },
    /// Observer recorded a semantic judgement.
    #[serde(rename = "observation.recorded")]
    ObservationRecorded {
        /// The judgement payload.
        observation: Observation,
    },
    /// A parent relation was recorded.
    #[serde(rename = "relation.linked")]
    RelationLinked {
        /// Parent agent identifier.
        parent_agent_id: AgentId,
        /// Parent session identifier.
        parent_session_id: SessionId,
    },
}

/// Event supplied by a producer before the store assigns ordering.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewEvent {
    /// Owning agent.
    pub agent_id: AgentId,
    /// Owning logical session.
    pub session_id: SessionId,
    /// Owning execution when the fact is execution-scoped.
    pub execution_id: Option<ExecutionId>,
    /// Producing system.
    pub source: EventSource,
    /// Native identifier used for idempotent insertion.
    pub source_event_id: Option<String>,
    /// Unified event vocabulary entry.
    pub kind: EventKind,
    /// Artifact reference for file events.
    pub artifact: Option<ArtifactRef>,
    /// Producer timestamp; the store fills a default when absent.
    pub occurred_at: Option<Timestamp>,
}

/// Stored event with store-assigned identity and ordering.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventRecord {
    /// Store-wide event identifier.
    pub event_id: EventId,
    /// Event content supplied by the producer.
    #[serde(flatten)]
    pub event: NewEvent,
    /// Per-session sequence.
    pub session_seq: SessionSeq,
    /// Store-wide position.
    pub global_position: GlobalPosition,
    /// Persisted timestamp.
    pub recorded_at: Timestamp,
}

/// Result of an append attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum AppendOutcome {
    /// The store inserted the event and assigned ordering.
    Recorded(EventRecord),
    /// An event with the same source key already existed.
    Deduplicated(EventRecord),
}

impl AppendOutcome {
    /// Borrow the stored record in either outcome.
    #[must_use]
    pub fn record(&self) -> &EventRecord {
        match self {
            AppendOutcome::Recorded(record) | AppendOutcome::Deduplicated(record) => record,
        }
    }
}

/// Per-session cursor exposed to readers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCursor {
    /// Session the cursor describes.
    pub session_id: SessionId,
    /// Last assigned session sequence.
    pub last_seq: SessionSeq,
    /// Timestamp of the last stored event.
    pub last_occurred_at: Timestamp,
}
