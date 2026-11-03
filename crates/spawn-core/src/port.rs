//! Ports to external systems.
//!
//! Every port is synchronous: Spawn has no concurrent request path inside one
//! process, and blocking calls keep the dependency graph free of an async
//! runtime. Adapters live in `spawn-postgres` and `spawn-host`; in-memory
//! implementations live in [`crate::memory`].

use std::time::Duration;

use thiserror::Error;

use crate::command::{Claimed, Outcome, Submission};
use crate::event::{Appended, EventRecord, GlobalPosition, NewEvent, SessionSeq};
use crate::id::{BranchName, CommandId, CommitId, Principal, SessionId, TerminalName, WorkspaceId};

/// Storage failure.
#[derive(Debug, Error)]
pub enum StoreError {
    /// The attempt no longer holds the lease of the command.
    #[error("lease of {command} attempt {attempt} is no longer held")]
    LeaseLost {
        /// Command.
        command: CommandId,
        /// Rejected attempt.
        attempt: u32,
    },
    /// The store refused the input, for example an unknown action type.
    #[error("rejected: {0}")]
    Rejected(String),
    /// Stored data does not match the schema.
    #[error("corrupt stored data: {0}")]
    Corrupt(String),
    /// The backend failed.
    #[error("storage backend failure: {0}")]
    Backend(String),
}

/// Append-only event log with per-session and global order.
pub trait EventLog: Send + Sync {
    /// Append one event. A repeated `(session, source, source_event_id)`
    /// returns the stored record with `inserted = false`.
    fn append(&self, event: NewEvent) -> Result<Appended, StoreError>;

    /// Session events with `session_seq > after`, in order, at most `limit`.
    fn read_session(
        &self,
        session: &SessionId,
        after: SessionSeq,
        limit: usize,
    ) -> Result<Vec<EventRecord>, StoreError>;

    /// All events with `position > after`, in order, at most `limit`.
    ///
    /// A position is visible only after every lower position is visible, so a
    /// reader that stores its last position never skips an event.
    fn read_global(
        &self,
        after: GlobalPosition,
        limit: usize,
    ) -> Result<Vec<EventRecord>, StoreError>;
}

/// Leased command queue.
///
/// Commands of one session run in submission order, one at a time. A lease
/// expires after its ttl; the next claim takes the same command with a higher
/// attempt.
pub trait CommandQueue: Send + Sync {
    /// Insert a command and its `command.accepted` event atomically. A
    /// repeated `(principal, idempotency_key)` returns the first command and
    /// writes nothing.
    fn submit(
        &self,
        principal: &Principal,
        submission: &Submission,
    ) -> Result<CommandId, StoreError>;

    /// Lease the oldest unfinished command of a session that has no live
    /// lease, or `None`.
    fn claim(&self, worker: &str, ttl: Duration) -> Result<Option<Claimed>, StoreError>;

    /// Record the outcome and its `command.finished` event atomically.
    fn finish(&self, claimed: &Claimed, outcome: &Outcome) -> Result<(), StoreError>;
}

/// Workspace store refusal or failure.
#[derive(Debug, Error)]
pub enum WorkspaceError {
    /// The store refused the call with a structured code.
    #[error("workspace refused: {code}: {message}")]
    Refused {
        /// Backend error code, for example `NoCommit`.
        code: String,
        /// Backend message.
        message: String,
    },
    /// The backend could not run or printed an unexpected answer.
    #[error("workspace unavailable: {0}")]
    Unavailable(String),
}

/// Result of a workspace commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Commit {
    /// The workspace store published a new commit.
    Created(CommitId),
    /// The workspace had no changes.
    Unchanged,
}

/// Versioned file storage used by the Bridge.
pub trait WorkspaceStore: Send + Sync {
    /// Name of the store, recorded with every artifact.
    fn project(&self) -> &str;

    /// Create `branch` from the base unless it exists.
    fn ensure_branch(&self, branch: &BranchName) -> Result<(), WorkspaceError>;

    /// Materialize `branch` where the engine can run.
    fn mount(&self, branch: &BranchName) -> Result<WorkspaceId, WorkspaceError>;

    /// Commit the workspace. No changes is [`Commit::Unchanged`].
    fn commit(&self, workspace: &WorkspaceId, message: &str) -> Result<Commit, WorkspaceError>;

    /// Unmount the workspace. Committed state stays on the branch.
    fn unmount(&self, workspace: &WorkspaceId) -> Result<(), WorkspaceError>;
}

/// Terminal failure.
#[derive(Debug, Error)]
#[error("terminal failure: {0}")]
pub struct TerminalError(pub String);

/// Process to start in a new terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// Terminal session name.
    pub terminal: TerminalName,
    /// Workspace the process runs in.
    pub workspace: WorkspaceId,
    /// Directory the process starts in, as seen by the terminal host.
    pub cwd: String,
    /// Executable.
    pub program: String,
    /// Arguments, passed without shell interpretation.
    pub arguments: Vec<String>,
    /// Environment of the process.
    pub environment: Vec<(String, String)>,
}

/// Named interactive terminals.
pub trait TerminalHost: Send + Sync {
    /// Start a detached terminal that runs the launch.
    fn start(&self, launch: &Launch) -> Result<(), TerminalError>;

    // Every call after `start` names the workspace again, so a host that
    // runs inside the workspace needs no memory between processes.

    /// Type `text` literally, then press Enter.
    fn submit_text(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
        text: &str,
    ) -> Result<(), TerminalError>;

    /// Press keys one at a time, without a paste. `keys` uses tmux key
    /// names (`y`, `Enter`). For dialogs that ignore pasted text.
    fn send_keys(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
        keys: &[&str],
    ) -> Result<(), TerminalError>;

    /// Whether the terminal still exists.
    fn is_alive(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
    ) -> Result<bool, TerminalError>;

    /// Last `lines` lines of the terminal screen, newest last.
    fn capture_tail(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
        lines: usize,
    ) -> Result<String, TerminalError>;

    /// Kill the terminal and its process. A missing terminal is not an error.
    fn terminate(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
    ) -> Result<(), TerminalError>;
}
