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

/// The longest `archive.failed` reason, in characters: a longer one is cut
/// on a character boundary (objective 179).
pub const ARCHIVE_REASON_MAX: usize = 500;

/// Wire name of Claude Code, interactive in a terminal: the engine every
/// execution runs, and the default driver (`crate::driver::DEFAULT`, the
/// only one registered). Executions and native sessions record it in their
/// `engine` field, which the `agent_executions` view shows; the Bridge
/// resolves a driver from it (`crate::driver::driver_or_default`), so a
/// record naming another engine still reads, as Claude Code's.
pub const ENGINE: &str = "claude";

/// System that produced a fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The engine's transcript (Claude Code's, read through its driver,
    /// `crate::driver`), and the runner's exit report. The wire name stays
    /// `claude`.
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
    /// The Bridge is about to press a key command's key
    /// ([`crate::command::CommandAction::Key`]). A key's own steps, never
    /// `typing`/`typed`: those say an input was typed and waits for its
    /// answer (a turn), and an Esc starts no turn.
    Pressing,
    /// The key was sent to the pane.
    Pressed,
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

/// What an engine was launched with, as far as it is no secret (issue #79):
/// recorded on [`EventKind::ExecutionStarted`] so `spawn fleet` and
/// `/api/fleet` show where an engine's models come from. Names and an
/// address, never a key: the relay's address is its scheme, host and port
/// only ([`LaunchSummary::relay_origin`]), whatever the configured URL held.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LaunchSummary {
    /// The model relay the inline settings point the engine at
    /// (`ANTHROPIC_BASE_URL`): scheme, host and port. Unset when the engine
    /// runs on Claude Code's own settings: no relay configured, or a
    /// default-profile start whose relay was unreachable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_url: Option<String>,
    /// The main model the inline settings pin; unset for Claude Code's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The small, fast model (the haiku slot) the inline settings pin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub small_model: Option<String>,
    /// The Claude Code configuration directory the engine runs with
    /// (`CLAUDE_CONFIG_DIR`, from `SPAWN_CLAUDE_CONFIG_DIR`); unset for the
    /// user's own `~/.claude`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_dir: Option<String>,
}

impl LaunchSummary {
    /// Whether it says nothing: an engine on Claude Code's own settings in
    /// the user's own directory, which a record leaves out.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// `url`'s scheme, host and port, with no path, query or fragment.
    /// `None` for text with no `scheme://host`, for a URL that holds a user
    /// or a password anywhere (an `@`: nothing of it is shown, however the
    /// rest would parse), and for an authority that is not a host and an
    /// optional all-digit port (a second `:` outside IPv6 brackets, an
    /// empty or non-digit port).
    pub fn relay_origin(url: &str) -> Option<String> {
        let (scheme, rest) = url.split_once("://")?;
        if rest.contains('@') {
            return None;
        }
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        let scheme_ok = !scheme.is_empty()
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c));
        let (host, port) = if let Some(inside) = authority.strip_prefix('[') {
            let (address, after) = inside.split_once(']')?;
            let address_ok = !address.is_empty()
                && address
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.');
            if !address_ok {
                return None;
            }
            (&authority[..address.len() + 2], after)
        } else {
            let end = authority.find(':').unwrap_or(authority.len());
            let host = &authority[..end];
            let host_ok = !host.is_empty()
                && host
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
            if !host_ok {
                return None;
            }
            (host, &authority[end..])
        };
        let port_ok = port.is_empty()
            || port.strip_prefix(':').is_some_and(|digits| {
                !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
            });
        (scheme_ok && port_ok).then(|| format!("{scheme}://{host}{port}"))
    }
}

/// What runs as a background job of the engine: work the agent started
/// that goes on after its turn ends, and whose end the engine reports to
/// the agent by waking it (docs/fleet-architecture.md §2, "Waiting on its
/// own job").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// A shell command in the background: a Bash call with
    /// `run_in_background`, or one the engine moved there (its timeout ran
    /// out, or a message had to reach the agent).
    Bash,
    /// A Monitor: a command whose output lines the engine delivers as
    /// events. It expires by itself (within an hour).
    Monitor,
    /// An Agent launched in the background (`isAsync`). It can run again
    /// under the same id when a message resumes it; each run is a start
    /// and an end of the job.
    Agent,
}

impl JobKind {
    /// Wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Monitor => "monitor",
            Self::Agent => "agent",
        }
    }
}

/// How a background job ended, as the engine reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobEnd {
    /// It finished (exit status 0, or a Monitor's command ended).
    Completed,
    /// It finished with a failing status.
    Failed,
    /// The engine killed it (low memory, a stop from its UI).
    Killed,
    /// The agent stopped it (`TaskStop`, which succeeded), or the engine
    /// marked it stopped (a job of an earlier engine process).
    Stopped,
    /// A Monitor that ran out its time.
    Expired,
    /// An end the engine named in a way this build does not know.
    #[serde(other)]
    Ended,
}

impl JobEnd {
    /// The end a task notification's `<status>` names.
    pub fn from_status(status: &str) -> Self {
        match status.trim() {
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "killed" => Self::Killed,
            "stopped" => Self::Stopped,
            "expired" => Self::Expired,
            _ => Self::Ended,
        }
    }

    /// Wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Killed => "killed",
            Self::Stopped => "stopped",
            Self::Expired => "expired",
            Self::Ended => "ended",
        }
    }
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
    /// The session became a child of another session.
    #[serde(rename = "session.linked")]
    SessionLinked {
        /// Parent session.
        parent_session_id: SessionId,
    },
    /// A prompt reached the engine.
    #[serde(rename = "message.received")]
    MessageReceived,
    /// The Bridge typed a command's text with its marker
    /// ([`crate::bridge::message_marker`]): the command is a message whose
    /// taking the transcript says. Written after the text is staged and
    /// before its `typing` step, keyed `cmd_<id>:message`, and only by a
    /// Bridge that types the marker: a command typed by an older Spawn has
    /// none, and is never read as a message not taken.
    #[serde(rename = "message.typed")]
    MessageTyped {
        /// The command whose text carried the marker.
        command_id: CommandId,
    },
    /// A transcript's user line (a prompt, not a tool result) held the
    /// marker of a command: the engine took that message. Keyed by the
    /// line and the command, so a line read again records nothing; a line
    /// written twice is two. Beside the line's `message.received`.
    #[serde(rename = "message.taken")]
    MessageTaken {
        /// The command the marker names.
        command_id: CommandId,
    },
    /// The agent answered a message by its id (`spawn ack <id>
    /// done|declined [<why>]` in its own pane, read by the loop from its
    /// workspace), or a task's report answered the message that delivered
    /// it. In the message's session, keyed `<id>:acted`: the first answer
    /// stands, a later one records nothing.
    #[serde(rename = "message.acted")]
    MessageActed {
        /// The command whose message was answered.
        command_id: CommandId,
        /// `done` or `declined`.
        outcome: String,
        /// Why, required for `declined`; none for a plain `done`.
        why: Option<String>,
    },
    /// A tool call started.
    #[serde(rename = "tool.started")]
    ToolStarted {
        /// Engine tool name.
        tool_name: String,
        /// Engine tool call identifier.
        tool_use_id: String,
        /// The call's key input in a few words, for a person reading the
        /// trace: a command's first line, a file path, a pattern
        /// ([`crate::ingest::tool_summary`]). Credentials are redacted and it
        /// is at most [`crate::ingest::TOOL_SUMMARY_CHARS`] characters. None
        /// for a tool without one, and in every event recorded before it was.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        /// The summary is the call's whole input, unredacted and uncut (a
        /// one-line command, a Read's file and range, a Glob): two calls
        /// with the same summary are the same call, and a reader may fold
        /// them (`×N`). False when the summary leaves input out, and in
        /// every event recorded before it was.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        summary_whole: bool,
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
    /// One model call's usage, read from an assistant line of the
    /// execution's transcript (crate::ingest's `claude_usage`): the engine
    /// writes one line per content block, each repeating the message's id,
    /// model and usage, so the event is keyed by the message id and its four
    /// counts. A repeat appends nothing, and a line with a larger usage is
    /// one more event the cost step folds into the same message (its largest
    /// usage, its earliest time). The cost step prices it (crate spawn-cli,
    /// `cost`).
    #[serde(rename = "model.usage")]
    ModelUsage {
        /// The engine's `message.id`.
        message_id: String,
        /// The engine's `message.model`.
        model: String,
        /// The line's own time (`timestamp`), epoch milliseconds.
        at_ms: i64,
        /// `input_tokens`.
        input_tokens: u64,
        /// `output_tokens`.
        output_tokens: u64,
        /// `cache_creation_input_tokens`.
        cache_write_tokens: u64,
        /// `cache_read_input_tokens`.
        cache_read_tokens: u64,
    },
    /// The turn ended with an engine or API failure.
    ///
    /// Claude Code fails a turn by writing an API error line to its
    /// transcript and waiting at its prompt: nothing ends the execution,
    /// so this record is what says a turn failed with a model cause. It
    /// keeps the engine's own tags so a recovery can tell a model outage
    /// from an expired login from an exhausted context without reading the
    /// text (crate::ingest's `claude_line`).
    #[serde(rename = "turn.failed")]
    TurnFailed {
        /// The error text the engine showed, truncated (the line's
        /// `message.content[].text`).
        error: String,
        /// The engine's `apiErrorStatus`: the HTTP status of the failed
        /// request (529, 503, 429, 401, 400, ...).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<i64>,
        /// The engine's `error` tag: `overloaded`, `rate_limit`,
        /// `server_error`, `authentication_failed`, `invalid_request`,
        /// `unknown`, ...
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_kind: Option<String>,
    },
    /// The agent started a background job ([`JobKind`]), read from its
    /// transcript: the job's id as the engine named it. When it started is
    /// the record's time. A job is open only within its execution: the
    /// execution's end (or the start of the next, a resume) closes it. An
    /// Agent resumed under its id starts again: the job is open when its
    /// latest start has no later end.
    #[serde(rename = "job.started")]
    JobStarted {
        /// The engine's task id (`b8w03og7k`, an Agent's `a5e451ae390ad7923`).
        job_id: String,
        /// Bash, Monitor or Agent.
        kind: JobKind,
    },
    /// A background job ended, read from the transcript: the engine's
    /// notification of it, or the agent's `TaskStop` that succeeded. When it
    /// ended is the record's time. It ends the job's latest start before it.
    #[serde(rename = "job.ended")]
    JobEnded {
        /// The engine's task id.
        job_id: String,
        /// How it ended.
        how: JobEnd,
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
        /// What the engine was launched with, no secret in it
        /// ([`LaunchSummary`]). Left out when it says nothing, and by
        /// records made before it was kept.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        launch: Option<LaunchSummary>,
    },
    /// The execution's terminal started, on this terminal server: the
    /// server that ran the execution, by its identity
    /// ([`crate::port::TerminalServer`]). Recorded after the start, since
    /// the start may be what started the server. The recovery tells a host
    /// restart by it: the execution's server is gone when none answers or
    /// the one that answers is another (crate `spawn-cli`'s `recover`).
    /// Left out by a host that cannot tell its server, and by executions
    /// started before it was kept.
    #[serde(rename = "execution.terminal_started")]
    TerminalStarted {
        /// The terminal server.
        server: crate::port::TerminalServer,
    },
    /// The execution ended.
    #[serde(rename = "execution.ended")]
    ExecutionEnded {
        /// How it ended.
        cause: EndCause,
        /// Process exit code when the OS reported one.
        exit_code: Option<i32>,
        /// The signal that killed the engine, when one did (`9` for a
        /// SIGKILL): the runner writes it to the exit file beside the
        /// status (`crate::bridge`, "the end's cause and the signal"). An
        /// engine the OS killed has no exit code, so without this every
        /// kill reads as a clean exit with no code.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signal: Option<i32>,
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
    /// The end of an execution kept failing: its final checkpoint, or its
    /// kill, which the runner outlived (`error` says which). The workspace
    /// stays mounted with everything it holds, and the Bridge retries the
    /// end at a slow interval until it gets through or a `discard` command
    /// gives the work up. Written once per
    /// execution; it asks for attention until `execution.ended`.
    #[serde(rename = "execution.end_stalled")]
    ExecutionEndStalled {
        /// How the execution was ending.
        cause: EndCause,
        /// Process exit code when the OS reported one.
        exit_code: Option<i32>,
        /// End attempts that failed so far (final checkpoints, or kills the
        /// runner outlived).
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
    /// A checkpoint found the agent's branch gone from its store while its
    /// workspace was mounted, and made it again from the workspace
    /// (objective 205): `artifact` is the branch's new head, `parent` what it
    /// was made on (what was published of the branch, or the store's main;
    /// null with neither). The checkpoint's own `artifact.committed` of the
    /// same commit follows it.
    #[serde(rename = "artifact.branch_recreated")]
    ArtifactBranchRecreated {
        /// The made-again commit.
        artifact: ArtifactRef,
        /// What it was made on, if anything.
        parent: Option<CommitId>,
    },
    /// One transcript's pass to the transcript archive copied nothing
    /// (objective 179), for a reason other than the one recorded last: the
    /// error as the loop's warning says it (paths and errors, never a
    /// value), at most [`ARCHIVE_REASON_MAX`] characters. Recorded by the
    /// Bridge on a change only; objective 193's alert reads it.
    #[serde(rename = "archive.failed")]
    ArchiveFailed {
        /// The engine's native session id: which transcript.
        native: String,
        /// Why the pass copied nothing.
        reason: String,
    },
    /// The first pass that copied a transcript after an `archive.failed` of
    /// it (objective 179): `copied` is how far the transcript's offset
    /// moved in that pass.
    #[serde(rename = "archive.resumed")]
    ArchiveResumed {
        /// The engine's native session id: which transcript.
        native: String,
        /// How far that pass moved the transcript's archived offset.
        copied: u64,
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
    /// One fact of a node (multi-host): its join, drain, undrain, removal,
    /// or the loop's heartbeat, lost and back. `spawn_node_record` appends
    /// it to `session_control` under this name, outside the control fold
    /// (`spawn_control_refold` reads only `control.recorded`). It belongs
    /// to no agent, so nothing observes or bridges it; every global reader
    /// must still decode it, or one `spawn node add` stalls them all.
    #[serde(rename = "node.recorded")]
    NodeRecorded {
        /// The record, `{"type": "node.<fact>", "name": ..., ...}`.
        record: serde_json::Value,
    },
    /// One change to the grading catalog (docs/agent-grading.md): a
    /// `preset.*` record of the control session, outside the control fold
    /// (`spawn_control_refold` reads only `control.recorded`); the
    /// database folds it into `grading_presets` (`spawn_grading_refold`).
    /// Like a control record it belongs to no agent.
    #[serde(rename = "grading.recorded")]
    GradingRecorded {
        /// The record, `{"type": "preset.…", ...}`.
        record: serde_json::Value,
    },
}

/// Every active dotted wire name emitted by [`EventKind::name`].
pub const ACTIVE_EVENT_NAMES: &[&str] = &[
    "session.native_started",
    "session.linked",
    "message.received",
    "message.typed",
    "message.taken",
    "message.acted",
    "tool.started",
    "tool.completed",
    "tool.failed",
    "turn.stopped",
    "model.usage",
    "turn.failed",
    "job.started",
    "job.ended",
    "execution.started",
    "execution.terminal_started",
    "execution.ended",
    "execution.end_stalled",
    "execution.unmounting",
    "command.step",
    "artifact.committed",
    "artifact.branch_recreated",
    "merge.conflicted",
    "archive.failed",
    "archive.resumed",
    "command.accepted",
    "command.finished",
    "screen.observed",
    "observation.recorded",
    "control.recorded",
    "node.recorded",
    "grading.recorded",
];

impl EventKind {
    /// Stable dotted wire name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::NativeSessionStarted { .. } => "session.native_started",
            Self::SessionLinked { .. } => "session.linked",
            Self::MessageReceived => "message.received",
            Self::MessageTyped { .. } => "message.typed",
            Self::MessageTaken { .. } => "message.taken",
            Self::MessageActed { .. } => "message.acted",
            Self::ToolStarted { .. } => "tool.started",
            Self::ToolCompleted { .. } => "tool.completed",
            Self::ToolFailed { .. } => "tool.failed",
            Self::TurnStopped { .. } => "turn.stopped",
            Self::ModelUsage { .. } => "model.usage",
            Self::TurnFailed { .. } => "turn.failed",
            Self::JobStarted { .. } => "job.started",
            Self::JobEnded { .. } => "job.ended",
            Self::ExecutionStarted { .. } => "execution.started",
            Self::TerminalStarted { .. } => "execution.terminal_started",
            Self::ExecutionEnded { .. } => "execution.ended",
            Self::ExecutionEndStalled { .. } => "execution.end_stalled",
            Self::ExecutionUnmounting => "execution.unmounting",
            Self::CommandStepped { .. } => "command.step",
            Self::ArtifactCommitted { .. } => "artifact.committed",
            Self::ArtifactBranchRecreated { .. } => "artifact.branch_recreated",
            Self::MergeConflicted { .. } => "merge.conflicted",
            Self::ArchiveFailed { .. } => "archive.failed",
            Self::ArchiveResumed { .. } => "archive.resumed",
            Self::CommandAccepted { .. } => "command.accepted",
            Self::CommandFinished { .. } => "command.finished",
            Self::ScreenObserved { .. } => "screen.observed",
            Self::ObservationRecorded { .. } => "observation.recorded",
            Self::ControlRecorded { .. } => "control.recorded",
            Self::NodeRecorded { .. } => "node.recorded",
            Self::GradingRecorded { .. } => "grading.recorded",
        }
    }

    /// Whether this is a control-session record (a control-stream, node,
    /// or grading record) rather than a fact about an agent's session.
    pub fn is_control(&self) -> bool {
        matches!(
            self,
            Self::ControlRecorded { .. } | Self::NodeRecorded { .. } | Self::GradingRecorded { .. }
        )
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

#[cfg(test)]
mod tests {
    use super::EventKind;
    use serde_json::json;

    #[test]
    fn a_node_record_decodes_as_a_control_session_event() {
        let wire = json!({"name": "node.recorded",
                          "detail": {"record": {"type": "node.heartbeat", "name": "runner",
                                                "join": 7, "load1": 0.5}}});
        let kind: EventKind = serde_json::from_value(wire.clone()).expect("node.recorded decodes");
        assert!(
            matches!(&kind, EventKind::NodeRecorded { record } if record["type"] == "node.heartbeat")
        );
        assert_eq!(kind.name(), "node.recorded");
        assert!(kind.is_control());
        assert!(kind.checkpoint_reason().is_none());
        assert_eq!(serde_json::to_value(&kind).expect("encodes"), wire);
    }

    #[test]
    fn active_event_names_excludes_retired_events_and_is_unique() {
        let retired = [
            "session.native_ended",
            "notice.typed",
            "notice.received",
            "notification.received",
        ];
        let mut seen = std::collections::BTreeSet::new();
        for name in super::ACTIVE_EVENT_NAMES {
            assert!(
                !retired.contains(name),
                "ACTIVE_EVENT_NAMES must not contain retired event {name}"
            );
            assert!(seen.insert(*name), "duplicate active event name {name}");
        }
        for name in retired {
            let wire = json!({"name": name, "detail": {}});
            assert!(
                serde_json::from_value::<EventKind>(wire).is_err(),
                "retired event {name} must not decode as EventKind"
            );
        }
    }
}
