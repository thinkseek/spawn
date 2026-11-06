//! Unified event vocabulary and stored event records.
//!
//! Every producer (the engine's transcript, Bridge, Observer, SQL gateway)
//! writes the same [`EventKind`] values. The store assigns the
//! per-session sequence, the global position and the record time; producer
//! clocks never decide order.

use serde::{Deserialize, Serialize};

use crate::id::{AgentId, BranchName, CommandId, CommitId, ExecutionId, SessionId, WorkspaceId};
use crate::observation::Observation;
use crate::screen::ScreenState;
use crate::time::Timestamp;

/// Wire name of the engine every execution runs: Claude Code, interactive
/// in a terminal. Executions and native sessions record it in their
/// `engine` field, which the `agent_executions` view shows; nothing
/// branches on it, and a record naming another engine still reads.
pub const ENGINE: &str = "claude";

/// System that produced a fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The Claude Code transcript, and the runner's exit report.
    Claude,
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
            Self::Bridge,
            Self::Observer,
            Self::Gateway,
            Self::Control,
        ]
        .into_iter()
        .find(|source| source.as_str() == text)
    }
}

/// Which models a Claude Code engine runs on (docs/fleet-architecture.md,
/// "Engine models"). A session keeps the profile it last started with
/// until a start or resume names another.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelProfile {
    /// The main model of Claude Code's own settings; the small model (the
    /// haiku slot, for background work) through the model relay.
    #[default]
    Default,
    /// Every model through the model relay, on the cheap provider: for
    /// simple, well-specified work.
    Cheap,
}

impl ModelProfile {
    /// Wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Cheap => "cheap",
        }
    }

    /// Parse a wire name.
    pub fn parse(text: &str) -> Option<Self> {
        [Self::Default, Self::Cheap]
            .into_iter()
            .find(|profile| profile.as_str() == text)
    }

    /// The default profile, which a record leaves out.
    #[allow(clippy::trivially_copy_pass_by_ref)] // serde's skip_serializing_if passes a reference
    pub fn is_default(&self) -> bool {
        *self == Self::Default
    }
}

/// A step of a command's handling the Bridge records, so that a retried
/// command (its lease expired, its tick failed) or a restarted Bridge knows
/// which irreversible effects already happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandStep {
    /// The start mounted its workspace (`workspace` on the record). A mount
    /// with no `execution.started` of the command and no
    /// [`CommandStep::MountReleased`] is unmounted when the Bridge recovers.
    Mounted,
    /// That mount went again before an execution used it.
    MountReleased,
    /// The Bridge is about to type the command's prompt or input.
    Typing,
    /// The typing succeeded.
    Typed,
    /// An input that waits for its turn: the pane did not take text when
    /// the Bridge read it (a turn runs, a dialog shows, the capture keeps
    /// failing), or an earlier input of the session still waits, or the
    /// session's last typed input has not been answered yet. Nothing was
    /// typed and the command stays unfinished; the Bridge types it once the
    /// pane reads ready, or fails it when the session's execution ends
    /// first. Only an input from outside Spawn's own loops waits
    /// (`crate::bridge::waits`). Recorded once per command.
    Waiting,
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
    /// `main` was merged into the agent's branch: cleanly when an execution
    /// starts or resumes, or into its mounted workspace between turns (a
    /// `conflict` verdict, a task's delivery).
    BaseMerged,
    /// The work of a cancelled task was set aside: a commit on top of the
    /// branch whose tree is the checkpoint the task started from, written by
    /// the dispatcher while nothing of the agent ran
    /// ([`crate::port::WorkspaceStore::set_aside`]). The work stays in its
    /// parent.
    SetAside,
}

/// How an execution ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndCause {
    /// The engine process exited by itself.
    Exited,
    /// A stop command terminated the execution.
    Stopped,
    /// No runner the Bridge started runs the execution any more, and none
    /// left an exit status: its terminal disappeared, or is there and hosts
    /// no runner (a dead pane, a shell restored after a restart). The end
    /// record's `evidence` says which.
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

/// Where an agent's branch started, recorded by the start that created it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Fork {
    /// What it was created from: `main`, or the start's `from` (another
    /// agent's id, meaning its branch head, or a full commit id). Records
    /// from before the cross-host cut may say `spawn/<agent>` (adopted from
    /// another host's push).
    pub from: String,
    /// The commit the branch was created at.
    pub commit: CommitId,
}

/// Source-independent event vocabulary. Wire names are stable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "name", content = "detail")]
pub enum EventKind {
    /// A native engine session started or resumed. The Bridge records a
    /// new session before its engine starts, without a version; each
    /// execution's first transcript line that names the engine's version
    /// records the session again with it (a resume included).
    #[serde(rename = "session.native_started")]
    NativeSessionStarted {
        /// Engine that owns the native session: [`ENGINE`].
        engine: String,
        /// Native session identifier, used for resume.
        native_session_id: String,
        /// Transcript path when the engine reports one.
        transcript_path: Option<String>,
        /// The engine's version, as its transcript names it (Claude Code's
        /// `version` field); unset when not read yet, and in records from
        /// before it was kept.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        engine_version: Option<String>,
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
    /// Read only, in records written before main stopped being merged into
    /// running workspaces: nothing writes it, and folds ignore it. The
    /// Bridge typed a prompt that opened with a merge notice, telling of
    /// the merges up to and including the event at `through`.
    #[serde(rename = "notice.typed")]
    NoticeTyped {
        /// The last merge event the typed notice told of.
        through: GlobalPosition,
    },
    /// Read only, like [`EventKind::NoticeTyped`]: nothing writes it, and
    /// folds ignore it. A prompt that opened with the merge notice reached
    /// the engine: the notice the Bridge typed last was delivered.
    #[serde(rename = "notice.received")]
    NoticeReceived,
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
    /// An execution started in a terminal against a workspace. The
    /// terminal is [`crate::id::TerminalName::for_execution`] of the event's
    /// execution, so it is not recorded; a record that still names it reads
    /// as before.
    #[serde(rename = "execution.started")]
    ExecutionStarted {
        /// Engine of the execution: [`ENGINE`].
        engine: String,
        /// Mounted workspace.
        workspace: WorkspaceId,
        /// Command that created the execution.
        command_id: CommandId,
        /// Set when this start created the agent's branch: where it began.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fork: Option<Fork>,
        /// Models the engine runs on; left out for the default profile.
        #[serde(default, skip_serializing_if = "ModelProfile::is_default")]
        model: ModelProfile,
        /// The length of the engine's transcript when the execution started
        /// (a resumed session's file holds the earlier executions' lines):
        /// the execution's lines are the ones after it. Left out for a
        /// transcript that did not exist yet, and by records made before it
        /// was kept (read from the start of the file).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        transcript_from: Option<u64>,
    },
    /// The execution ended.
    #[serde(rename = "execution.ended")]
    ExecutionEnded {
        /// How it ended.
        cause: EndCause,
        /// Process exit code when the OS reported one.
        exit_code: Option<i32>,
        /// What the Bridge saw when it ended an execution no runner of its
        /// own ran any more: the terminal gone, or there and hosting no
        /// runner ([`crate::port::Runner`]), and a stop or start that found
        /// it so. Left out when there is nothing to tell (an exit, a stop of
        /// a running engine), and by records made before it was kept.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence: Option<String>,
    },
    /// An ending execution's final checkpoint committed and its terminal is
    /// gone; the unmount comes next. An end recovered after this record
    /// does not checkpoint again (the workspace may be gone already): it
    /// unmounts and records `execution.ended`.
    #[serde(rename = "execution.unmounting")]
    ExecutionUnmounting,
    /// A step of a command's handling ([`CommandStep`]), keyed
    /// `cmd_<id>:<step>`: written before the effect for an intent
    /// (`typing`), after it for a completion.
    #[serde(rename = "command.step")]
    CommandStepped {
        /// The command.
        command_id: CommandId,
        /// The step.
        step: CommandStep,
        /// For [`CommandStep::Mounted`] and [`CommandStep::MountReleased`]:
        /// the workspace.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<WorkspaceId>,
    },
    /// The final checkpoint of an ending execution kept failing: the
    /// terminal is down, the workspace stays mounted with everything it
    /// holds, and the Bridge retries the checkpoint at a slow interval until
    /// it commits or a `discard` command gives the work up. Written once per
    /// execution; it asks for attention until `execution.ended`.
    #[serde(rename = "execution.end_stalled")]
    ExecutionEndStalled {
        /// How the execution was ending.
        cause: EndCause,
        /// Process exit code when the OS reported one.
        exit_code: Option<i32>,
        /// Final checkpoints that failed so far.
        failures: u32,
        /// The last failure.
        error: String,
    },
    /// The workspace store persisted a checkpoint.
    #[serde(rename = "artifact.committed")]
    ArtifactCommitted {
        /// Trigger of the checkpoint.
        reason: CheckpointReason,
        /// Commit reference.
        artifact: ArtifactRef,
        /// For a merge of main (`base_merged`): the files it changed.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        files: Vec<String>,
    },
    /// Merging the source's `main` into an agent's branch conflicted: the
    /// branch stayed as it was.
    #[serde(rename = "merge.conflicted")]
    MergeConflicted {
        /// Conflicting paths.
        files: Vec<String>,
        /// The agent's mounted workspace holds the merge with conflict
        /// markers in `files`, `main` pending as the second parent of the
        /// checkpoint that resolves them (a merge between turns: a `conflict`
        /// verdict or a task's delivery). False: nothing changed (a start or
        /// resume, which merges only cleanly, or a conflict markers cannot
        /// show).
        marked: bool,
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
            Self::NoticeTyped { .. } => "notice.typed",
            Self::NoticeReceived => "notice.received",
            Self::NotificationReceived { .. } => "notification.received",
            Self::ToolStarted { .. } => "tool.started",
            Self::ToolCompleted { .. } => "tool.completed",
            Self::ToolFailed { .. } => "tool.failed",
            Self::TurnStopped { .. } => "turn.stopped",
            Self::TurnFailed { .. } => "turn.failed",
            Self::ExecutionStarted { .. } => "execution.started",
            Self::ExecutionEnded { .. } => "execution.ended",
            Self::ExecutionEndStalled { .. } => "execution.end_stalled",
            Self::ExecutionUnmounting => "execution.unmounting",
            Self::CommandStepped { .. } => "command.step",
            Self::ArtifactCommitted { .. } => "artifact.committed",
            Self::MergeConflicted { .. } => "merge.conflicted",
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
