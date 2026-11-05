//! Ports to external systems.
//!
//! Every port is synchronous: Spawn has no concurrent request path inside one
//! process, and blocking calls keep the dependency graph free of an async
//! runtime. Adapters live in `spawn-postgres` and `spawn-host`; in-memory
//! implementations live in `crate::memory` (feature `testing`).

use std::time::Duration;

use thiserror::Error;

use crate::command::{Claimed, Outcome, Submission};
use crate::event::{Appended, EventRecord, Fork, GlobalPosition, NewEvent};
use crate::id::{AgentId, BranchName, CommandId, CommitId, Principal, TerminalName, WorkspaceId};

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

/// The [`WorkspaceError::Refused`] code of a checkpoint of a workspace that
/// can no longer follow its branch (another writer moved the branch under a
/// merge, and the directory differs from both sides). The store has kept the
/// directory's state off the branch before refusing, so nothing is lost by
/// unmounting it; the execution cannot checkpoint again and is ended.
pub const WORKSPACE_STALE: &str = "WorkspaceStale";

/// Result of a workspace commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Commit {
    /// The workspace store published a new commit.
    Created(CommitId),
    /// The workspace had no changes.
    Unchanged,
}

/// Result of [`WorkspaceStore::ensure_branch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Branch {
    /// The branch was already there and stays where it is.
    Existing,
    /// The branch was created.
    Created(Fork),
}

/// What a start's `from` names: another agent's branch head, or a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkFrom {
    /// The head of this agent's branch.
    Agent(AgentId),
    /// A full commit id (40 or 64 hex digits).
    Commit(CommitId),
}

impl ForkFrom {
    /// Parse a `from`. Anything but an agent id or a full lowercase commit
    /// id is refused, so nothing that looks like an option reaches a backend.
    pub fn parse(from: &str) -> Result<Self, WorkspaceError> {
        let refused = || WorkspaceError::Refused {
            code: "BadFrom".to_string(),
            message: format!("from {from:?} is neither an agent id nor a full commit id"),
        };
        if from.starts_with('-') {
            return Err(refused());
        }
        if let Ok(agent) = AgentId::new(from) {
            return Ok(Self::Agent(agent));
        }
        let hex = from
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if hex && matches!(from.len(), 40 | 64) {
            return CommitId::new(from).map(Self::Commit).map_err(|_| refused());
        }
        Err(refused())
    }
}

/// Result of [`WorkspaceStore::merge_base`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum BaseMerge {
    /// The branch already contains the source's `main`.
    #[default]
    Current,
    /// `main` was merged in: the workspace directory and then the branch
    /// moved to `commit`; `files` changed in the directory.
    Merged {
        /// The merge commit, now the branch head.
        commit: CommitId,
        /// Paths the merge changed in the workspace.
        files: Vec<String>,
    },
    /// `main` conflicts with the branch in `files`. The branch did not move.
    Conflicted {
        /// Conflicting paths.
        files: Vec<String>,
        /// The workspace holds the merge with conflict markers in `files`
        /// (written now, or by an earlier call and not resolved yet); the
        /// checkpoint after they are resolved commits the merge. False: a
        /// conflict markers cannot show (a file deleted or renamed on one
        /// side), and nothing changed.
        marked: bool,
    },
}

/// Versioned file storage used by the Bridge.
pub trait WorkspaceStore: Send + Sync {
    /// Name of the store, recorded with every artifact.
    fn project(&self) -> &str;

    /// Create `branch` unless it exists: at `from` when given (see
    /// [`ForkFrom`]), else at the base. An existing branch ignores `from`.
    /// A `from` that does not resolve to a commit is refused.
    fn ensure_branch(
        &self,
        branch: &BranchName,
        from: Option<&str>,
    ) -> Result<Branch, WorkspaceError>;

    /// Materialize `branch` where the engine can run.
    fn mount(&self, branch: &BranchName) -> Result<WorkspaceId, WorkspaceError>;

    /// Commit the workspace. No changes is [`Commit::Unchanged`].
    fn commit(&self, workspace: &WorkspaceId, message: &str) -> Result<Commit, WorkspaceError>;

    /// Unmount the workspace. Committed state stays on the branch.
    fn unmount(&self, workspace: &WorkspaceId) -> Result<(), WorkspaceError>;

    /// Merge the base (the source's `main`) into the branch of a mounted,
    /// idle workspace whose state is committed: the directory first, then
    /// the branch. A content conflict writes the merge with conflict markers
    /// into the directory and leaves the branch where it is; the checkpoint
    /// after the markers are resolved commits the merge (see
    /// [`BaseMerge::Conflicted`]). A directory that differs from the branch,
    /// or a conflict markers cannot show, changes nothing. A store without a
    /// moving base is always current.
    fn merge_base(&self, workspace: &WorkspaceId) -> Result<BaseMerge, WorkspaceError> {
        let _ = workspace;
        Ok(BaseMerge::Current)
    }

    /// The commit `branch` points at, `None` when the branch does not exist
    /// (or the store cannot tell). The Bridge compares it at recovery with
    /// the last commit the log recorded for the branch.
    fn branch_head(&self, branch: &BranchName) -> Result<Option<CommitId>, WorkspaceError> {
        let _ = branch;
        Ok(None)
    }
}

/// Chunk store failure.
#[derive(Debug, Error)]
pub enum ChunkError {
    /// No object has this name.
    #[error("chunk {0} is missing")]
    Missing(String),
    /// The object's bytes do not hash to its name, or the name is not a hash.
    #[error("chunk {0} is corrupt")]
    Corrupt(String),
    /// The backend could not run.
    #[error("chunk store unavailable: {0}")]
    Unavailable(String),
}

/// One object a chunk store holds, as a collection sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredObject {
    /// Its name: the hash of its bytes.
    pub name: String,
    /// Time since it was written or last used (a `put` of the same content,
    /// a [`ChunkStore::touch`]).
    pub age: Duration,
    /// Bytes it takes in the store.
    pub bytes: u64,
}

/// Content-addressed object store for large files (docs/storage.md).
///
/// An object is named by the hash of its bytes, written once and never
/// changed. Every read is verified: bytes that do not hash to the name are an
/// error, never an answer. A local directory first, any S3-compatible bucket
/// later, behind this one port.
///
/// Objects are collected (`spawn chunks gc`) when no reachable commit names
/// them and they are older than a grace period, so every use restarts an
/// object's age: chunks are written before the commit that names them.
///
/// A commit that writes objects holds [`ChunkStore::writing`] from its first
/// object until its ref names them, and a collection holds
/// [`ChunkStore::collecting`] from before it reads the refs until its last
/// removal: a collection never sees the objects of a commit in flight
/// without its ref, however long the commit takes.
pub trait ChunkStore: Send + Sync + std::fmt::Debug {
    /// Hold off collections until the guard is dropped: taken by a writer
    /// before its first object and kept until a ref names them. Writers
    /// share it. A store no collection runs against holds nothing.
    fn writing(&self) -> Result<ChunkGuard, ChunkError> {
        Ok(Box::new(()))
    }

    /// Wait for every writer holding [`ChunkStore::writing`], then hold off
    /// new ones until the guard is dropped; [`ChunkStore::remove_if_older`]
    /// runs meanwhile.
    fn collecting(&self) -> Result<ChunkGuard, ChunkError> {
        Ok(Box::new(()))
    }

    /// Store `bytes` unless a sound object of the same content exists; its
    /// name. A corrupt object of that content is replaced; a sound one is
    /// made young again.
    fn put(&self, bytes: &[u8]) -> Result<String, ChunkError>;

    /// The bytes of the object `name`, verified against the name.
    fn get(&self, name: &str) -> Result<Vec<u8>, ChunkError>;

    /// Whether an object `name` is stored (not verified).
    fn contains(&self, name: &str) -> Result<bool, ChunkError>;

    /// Make the object `name` young again, as a `put` of its content would;
    /// false when the store does not hold it.
    fn touch(&self, name: &str) -> Result<bool, ChunkError>;

    /// Every object the store holds, with its age and size.
    fn list(&self) -> Result<Vec<StoredObject>, ChunkError>;

    /// Remove the object `name` only if it is still at least `older_than`
    /// old, read again just before it goes: the bytes freed, or `None` when
    /// it was kept (made young meanwhile, or already gone).
    fn remove_if_older(&self, name: &str, older_than: Duration) -> Result<Option<u64>, ChunkError>;
}

/// Held by [`ChunkStore::writing`] or [`ChunkStore::collecting`] until it is
/// dropped.
pub type ChunkGuard = Box<dyn std::any::Any + Send>;

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

/// The most bytes one [`TerminalHost::read_from`] returns: a file an agent
/// writes cannot make the Bridge hold an unbounded buffer.
pub const READ_LIMIT: usize = 8 << 20;

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

    /// The bytes of the file at `path`, as the terminal's process sees it,
    /// from byte `offset`, at most [`READ_LIMIT`] of them; `None` when there
    /// is no such file. How the Bridge follows what an engine writes (its
    /// transcript, the runner's exit status) without the engine reporting
    /// anything itself. The agent controls these paths: anything but a
    /// regular file (a FIFO, a device, a symlink) is an error, and a read
    /// must never block.
    fn read_from(
        &self,
        workspace: &WorkspaceId,
        path: &str,
        offset: u64,
    ) -> Result<Option<Vec<u8>>, TerminalError>;

    /// Create the file at `path`, as the terminal's process sees it, with
    /// its missing parent directories, holding `bytes`, unless something is
    /// at `path` already: whether it was written. Never replaces what is
    /// there, and the file appears whole or not at all. How the Bridge puts
    /// back an engine's transcript from the archive before a resume.
    fn write_new(
        &self,
        workspace: &WorkspaceId,
        path: &str,
        bytes: &[u8],
    ) -> Result<bool, TerminalError>;

    /// Kill the terminal and its process. A missing terminal is not an error.
    fn terminate(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
    ) -> Result<(), TerminalError>;
}

/// Transcript archive failure.
#[derive(Debug, Error)]
#[error("transcript archive: {0}")]
pub struct ArchiveError(pub String);

/// Durable copies of engines' native transcripts, beside the workspace
/// store ([`crate::transcripts`]). Each native session of an agent has
/// numbered copies, oldest first: copy 0 is the transcript as the Bridge
/// first saw it, and a later copy holds the whole file again after it
/// shrank or was replaced. A copy only grows; archived bytes never change.
pub trait TranscriptArchive: Send + Sync {
    /// The length of each copy of the transcript of `native`, oldest first;
    /// empty when none is archived.
    fn copies(&self, agent: &AgentId, native: &str) -> Result<Vec<u64>, ArchiveError>;

    /// At most `limit` bytes of copy `copy` from byte `offset`.
    fn read(
        &self,
        agent: &AgentId,
        native: &str,
        copy: usize,
        offset: u64,
        limit: usize,
    ) -> Result<Vec<u8>, ArchiveError>;

    /// Append `bytes` to copy `copy`, which must hold exactly `at` bytes (a
    /// new copy: the next number, at 0), and make them durable. Any other
    /// `at`, or a copy number past the next, is refused: nothing archived is
    /// overwritten.
    fn append(
        &self,
        agent: &AgentId,
        native: &str,
        copy: usize,
        at: u64,
        bytes: &[u8],
    ) -> Result<(), ArchiveError>;
}
