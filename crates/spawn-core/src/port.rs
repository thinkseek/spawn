//! Ports to external systems.
//!
//! Every port is synchronous: Spawn has no concurrent request path inside one
//! process, and blocking calls keep the dependency graph free of an async
//! runtime. Adapters live in `spawn-postgres` and `spawn-host`; in-memory
//! implementations live in `crate::memory` (feature `testing`).

use std::time::Duration;

use thiserror::Error;

use crate::command::{Claimed, Outcome, Submission};
use crate::event::{Appended, EventKind, EventRecord, Fork, GlobalPosition, NewEvent, Source};
use crate::id::{
    AgentId, BranchName, CommandId, CommitId, Principal, SessionId, TerminalName, WorkspaceId,
};

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
    /// The database could not be reached: no server answered, the
    /// connection was cut, or the connect itself failed. It is not a
    /// verdict on the work: the engines run on, and the loop that reads the
    /// store waits for it instead of ending ([`UNREACHABLE`]).
    #[error("{UNREACHABLE}: {0}")]
    Unreachable(String),
}

/// The prefix of every [`StoreError::Unreachable`]: what a caller holding
/// only the error's text reads to tell "the database is not there" from
/// "the database said no" (`is_unreachable`).
pub const UNREACHABLE: &str = "database unreachable";

/// Whether an error's text is a [`StoreError::Unreachable`]: the store
/// could not be reached, so the pass that failed says nothing about the
/// fleet and must be retried rather than counted as a failed one.
pub fn is_unreachable(error: &str) -> bool {
    error.contains(UNREACHABLE)
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

    /// The session's open execution as the log has it: the record of its
    /// latest `execution.started` when no `execution.ended` the Bridge wrote
    /// follows it, the rule by which the log refuses a second start. The
    /// Bridge asks when its own view of the session runs nothing, so a view
    /// that diverged from the log (whatever made it) cannot leave the log
    /// waiting forever for an end the Bridge sees no reason to write. The
    /// default reads the whole log.
    fn open_execution(&self, session: &SessionId) -> Result<Option<EventRecord>, StoreError> {
        const BATCH: usize = 1000;
        let mut after = GlobalPosition(0);
        let mut open: Option<EventRecord> = None;
        loop {
            let batch = self.read_global(after, BATCH)?;
            for record in &batch {
                let event = &record.event;
                if event.session_id != *session || event.execution_id.is_none() {
                    continue;
                }
                match &event.kind {
                    EventKind::ExecutionStarted { .. } => open = Some(record.clone()),
                    EventKind::ExecutionEnded { .. }
                        if event.source == Source::Bridge
                            && open.as_ref().is_some_and(|started| {
                                started.event.execution_id == event.execution_id
                            }) =>
                    {
                        open = None;
                    }
                    _ => {}
                }
            }
            match batch.last() {
                Some(last) if batch.len() == BATCH => after = last.position,
                _ => return Ok(open),
            }
        }
    }
}

/// Exclusive ownership of one named Bridge worker.
///
/// The backend releases this ownership when its process dies. Holding the
/// guard is what makes [`CommandQueue::recover_worker`] safe: a replacement
/// cannot take a still-live worker's leases. Every same-named Bridge in a
/// deployment must participate; a pre-protocol process holds no lock, so old
/// and new binaries must not run the same worker concurrently.
pub trait CommandOwnership: Send + Sync {}

/// Leased command queue.
///
/// Commands of one session run in submission order, one at a time. A lease
/// expires after its ttl; the next claim takes the same command with a higher
/// attempt.
pub trait CommandQueue: Send + Sync {
    /// Exclusively own `worker` for the returned guard's life, then make its
    /// unfinished leases immediately claimable without changing their fencing
    /// attempts. The count says how many live leases were recovered. A second
    /// live Bridge with the same name is refused, without changing its leases.
    /// Combining acquisition and recovery makes an unowned recovery impossible.
    fn recover_worker(
        &self,
        worker: &str,
    ) -> Result<(Box<dyn CommandOwnership>, usize), StoreError>;

    /// Insert a command and its `command.accepted` event atomically. A
    /// repeated `(principal, idempotency_key)` returns the first command and
    /// writes nothing.
    fn submit(
        &self,
        principal: &Principal,
        submission: &Submission,
    ) -> Result<CommandId, StoreError>;

    /// Lease the oldest unfinished command of a session that has no live
    /// lease, or `None`. An unfinished input the Bridge recorded `waiting`
    /// (it waits for the session's turn) does not hold the commands behind
    /// it; any other unfinished command does.
    fn claim(&self, worker: &str, ttl: Duration) -> Result<Option<Claimed>, StoreError>;

    /// Record the outcome and its `command.finished` event atomically.
    fn finish(&self, claimed: &Claimed, outcome: &Outcome) -> Result<(), StoreError>;

    /// Leave a claimed command unfinished and make it claimable again after
    /// `retry_in`: the Bridge's input that waits for its turn
    /// (`CommandStep::Waiting`) is claimed again then, by this Bridge or a
    /// restarted one. A lease that is no longer this claim's changes
    /// nothing. Without it the lease runs out on its own.
    fn release(&self, claimed: &Claimed, retry_in: Duration) -> Result<(), StoreError> {
        let _ = (claimed, retry_in);
        Ok(())
    }
}

/// Workspace store refusal or failure.
#[derive(Debug, Clone, Error)]
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

/// The [`WorkspaceError::Refused`] code of a merge of main into a workspace
/// that did not happen because the agent was not idle
/// ([`WorkspaceStore::merge_base_when`]): nothing changed, and the merge is
/// tried again later. The message says why.
pub const MERGE_WAITS: &str = "MergeWaits";

/// The [`WorkspaceError::Refused`] code of a background mount that was
/// cancelled before installing or replacing the workspace
/// ([`WorkspaceStore::mount_when`]).
pub const MOUNT_CANCELLED: &str = "MountCancelled";

/// The [`WorkspaceError::Refused`] code of a delayed/abandoned unmount that
/// observed a newer mount or live execution under the branch lock
/// ([`WorkspaceStore::unmount_when`]).
pub const UNMOUNT_IN_USE: &str = "UnmountInUse";

/// What [`WorkspaceStore::hold`] returns: the workspace is held until it is
/// dropped.
pub type Hold = Box<dyn std::any::Any + Send>;

/// Result of a workspace commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Commit {
    /// The workspace store published a new commit.
    Created(CommitId),
    /// The workspace had no changes.
    Unchanged,
    /// The branch was gone from the store while its workspace was mounted
    /// (objective 205, incident I-006: deleted under it), and the checkpoint
    /// made it again from the workspace: `commit` holds the workspace's
    /// state and is the branch's head now. `parent` is the commit it was
    /// made on: what this host had published of the branch (taken back
    /// from the source; `commit` is that head itself when the workspace
    /// held nothing new), else the store's main; `None` with neither (a
    /// commit with no parent).
    Recreated {
        /// The branch's new head.
        commit: CommitId,
        /// What it was made on, if anything.
        parent: Option<CommitId>,
    },
}

/// Result of [`WorkspaceStore::set_aside`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetAside {
    /// The branch already holds the start's tree: nothing to set aside.
    Unchanged,
    /// `commit`, whose first parent is `kept`, has the start's tree; `kept`
    /// holds the work set aside; `files` are the paths the two trees differ
    /// in.
    Created {
        commit: CommitId,
        kept: CommitId,
        files: Vec<String>,
    },
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

/// Result of [`WorkspaceStore::merge_main_clean`] and of the mounted-workspace
/// [`WorkspaceStore::merge_base`].
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

    /// Materialize `branch` where the engine can run. The id names the
    /// directory as the engine's process sees it (every symlink on the way
    /// resolved): the pane starts there, and the engine files its transcript
    /// under that path.
    fn mount(&self, branch: &BranchName) -> Result<WorkspaceId, WorkspaceError>;

    /// Materialize `branch` only while `ready` still wants the mount.
    ///
    /// Production stores check `ready` under the branch's lock right before
    /// creating or replacing any workspace tree; if `ready` returns an error,
    /// nothing is modified and [`MOUNT_CANCELLED`] is returned.
    fn mount_when(
        &self,
        branch: &BranchName,
        ready: &dyn Fn() -> Result<(), String>,
    ) -> Result<WorkspaceId, WorkspaceError> {
        ready().map_err(|why| WorkspaceError::Refused {
            code: MOUNT_CANCELLED.to_string(),
            message: why,
        })?;
        self.mount(branch)
    }

    /// `workspace` as this store names it now ([`Self::mount`]). An id
    /// recorded under another spelling of the store's root (the root as it
    /// was configured, before a version that resolves it through its
    /// symlinks) names the same directory, and comes back under the resolved
    /// root; any other id comes back as it is. The Bridge reads the
    /// workspace of every execution in the log through it, so an execution
    /// started before the upgrade is followed where its engine writes.
    fn canonical(&self, workspace: &WorkspaceId) -> WorkspaceId {
        workspace.clone()
    }

    /// Commit the workspace. No changes is [`Commit::Unchanged`]. A branch
    /// gone from the store while its workspace is mounted is made again
    /// from the workspace ([`Commit::Recreated`], objective 205), never over
    /// a ref someone made meanwhile.
    fn commit(&self, workspace: &WorkspaceId, message: &str) -> Result<Commit, WorkspaceError>;

    /// Unmount the workspace. Committed state stays on the branch.
    fn unmount(&self, workspace: &WorkspaceId) -> Result<(), WorkspaceError>;

    /// Unmount `workspace` only while `ready` still allows it.
    ///
    /// Production stores check `ready` under the branch's lock before
    /// removing or stopping the workspace; if `ready` returns an error
    /// (such as [`UNMOUNT_IN_USE`] or a transient log read error), nothing
    /// is removed and that error is returned.
    fn unmount_when(
        &self,
        workspace: &WorkspaceId,
        ready: &dyn Fn() -> Result<(), WorkspaceError>,
    ) -> Result<(), WorkspaceError> {
        ready()?;
        self.unmount(workspace)
    }

    /// Whether `workspace` is currently mounted in this store. Used when a
    /// reclaimed start considers reusing a recorded `CommandStep::Mounted`
    /// workspace whose `CommandStep::MountReleased` may have failed to append
    /// after an unmount succeeded.
    fn is_mounted(&self, workspace: &WorkspaceId) -> bool {
        let _ = workspace;
        true
    }

    /// Merge `main` into `branch`, which is not mounted, only when the merge
    /// is clean: the branch moves to the merge commit
    /// ([`BaseMerge::Merged`]). A conflict of any kind changes nothing and
    /// is answered as [`BaseMerge::Conflicted`] with `marked: false`. The
    /// Bridge calls it when an execution starts or resumes, before the
    /// mount: that is the only way main reaches an agent's branch. A store
    /// without a moving base is always current.
    fn merge_main_clean(&self, branch: &BranchName) -> Result<BaseMerge, WorkspaceError> {
        let _ = branch;
        Ok(BaseMerge::Current)
    }

    /// Not called by the Bridge, which merges main only at a start or resume
    /// ([`Self::merge_main_clean`]): the integrator calls it on a `conflict`
    /// verdict and the dispatcher before it types a task into a live
    /// worker, both while the agent is idle between turns, and both through
    /// [`Self::merge_base_when`], which checks that it is. Merge the base
    /// into the branch of a mounted workspace whose state is committed: the
    /// directory first, then the branch. A content conflict writes the merge with conflict markers
    /// into the directory and leaves the branch where it is; the checkpoint
    /// after the markers are resolved commits the merge (see
    /// [`BaseMerge::Conflicted`]). A directory that differs from the branch,
    /// or a conflict markers cannot show, changes nothing. A store without a
    /// moving base is always current.
    fn merge_base(&self, workspace: &WorkspaceId) -> Result<BaseMerge, WorkspaceError> {
        let _ = workspace;
        Ok(BaseMerge::Current)
    }

    /// [`Self::merge_base`], only while `ready` says the agent is idle (no
    /// turn running, no command in flight). A store asks it as late as it
    /// can: after its own preparation (a fetch of `main`), under the
    /// branch's lock, before anything is written, so a turn typed in while
    /// it prepared is seen (the Bridge types under [`Self::hold`]). When
    /// `ready` refuses, nothing changes: the merge is refused with
    /// [`MERGE_WAITS`] and `ready`'s reason. A merge with nothing to write
    /// (current, or markers still unresolved) need not ask.
    fn merge_base_when(
        &self,
        workspace: &WorkspaceId,
        ready: &dyn Fn() -> Result<(), String>,
    ) -> Result<BaseMerge, WorkspaceError> {
        ready().map_err(|why| WorkspaceError::Refused {
            code: MERGE_WAITS.to_string(),
            message: why,
        })?;
        self.merge_base(workspace)
    }

    /// Hold `workspace` against [`Self::merge_base_when`] until the returned
    /// guard is dropped: the Bridge holds it while it types into the agent's
    /// engine, so a merge that found the agent idle cannot write the
    /// workspace under a turn typed in meanwhile, and one that waited for
    /// the typing sees it. `None`: nothing to hold (or it could not be
    /// taken; the typing goes on).
    fn hold(&self, workspace: &WorkspaceId) -> Option<Hold> {
        let _ = workspace;
        None
    }

    /// Set aside the work on `branch` since the commit `start` (a checkpoint
    /// of the branch, so its ancestor; `None`: where the branch forked from
    /// `main`), for a task that was cancelled. Called by the dispatcher
    /// while nothing of the agent writes the branch, under the branch's
    /// lock: its execution has ended, or it runs idle at its prompt with
    /// nothing in flight, which a store with branches asks again under the
    /// lock before it writes (the Bridge types under [`Self::hold`]), so a
    /// workspace is never reset under a turn. Nothing is rewound: a mounted
    /// workspace whose state no checkpoint took is committed first, then a
    /// new commit with `start`'s tree goes on top of the branch head (by
    /// compare-and-swap) and the workspace follows it. The work stays in the
    /// branch's history ([`SetAside::Created`]'s `kept`). A store without
    /// branches has nothing to set aside.
    ///
    /// `message` names the task, and the branch moves before the caller
    /// records the set-aside: asked again with the same `message` while the
    /// branch head is that set-aside, the store answers the same
    /// [`SetAside::Created`] and writes nothing, so a record that failed is
    /// made by the retry (a checkpoint taken on top of it meanwhile is set
    /// aside by a set-aside of its own). What the workspace's checkpoints
    /// leave out (a file over the size cap, a special file) is in no commit
    /// and stays in a mounted workspace as it is; only one in the way of a
    /// file of `start` makes way for it, and a file over the cap is then
    /// moved aside inside the workspace, never deleted.
    fn set_aside(
        &self,
        branch: &BranchName,
        start: Option<&str>,
        message: &str,
    ) -> Result<SetAside, WorkspaceError> {
        let _ = (branch, start, message);
        Ok(SetAside::Unchanged)
    }

    /// What the store must tell the agent of the workspace and has not
    /// told yet (a file a checkpoint left out). Reading changes nothing: a
    /// notice is returned until [`Self::notices_told`] says a prompt
    /// carried it, so one no prompt carried yet outlives a restart of the
    /// Bridge. No default: a store that wraps another must pass it on.
    fn notices(&self, workspace: &WorkspaceId) -> Vec<String>;

    /// A prompt typed into the agent's engine carried `notices` (as
    /// [`Self::notices`] returned them): they are not returned again.
    fn notices_told(&self, workspace: &WorkspaceId, notices: &[String]);

    /// The commit `branch` points at, `None` when the branch does not exist
    /// (or the store cannot tell). The Bridge compares it at recovery with
    /// the last commit the log recorded for the branch.
    fn branch_head(&self, branch: &BranchName) -> Result<Option<CommitId>, WorkspaceError> {
        let _ = branch;
        Ok(None)
    }
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

/// One terminal server, told as a process is: its pid and when it started
/// (wall clock, in ms). Two readings name the same server when both are
/// equal; a pid alone is reused, after a reboot first of all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TerminalServer {
    /// The server's process id.
    pub pid: u32,
    /// When it started, ms since the epoch (tmux keeps whole seconds).
    pub started_at_ms: i64,
}

/// What [`TerminalHost::server`] saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerSeen {
    /// No terminal server answers: every terminal is gone with it.
    Gone,
    /// A server answers (or the host cannot tell that none does), and is
    /// this one when the host can tell which.
    Answers(Option<TerminalServer>),
}

/// What a terminal hosts ([`TerminalHost::runner`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Runner {
    /// The runner the host started there, alive.
    Running,
    /// No terminal of that name.
    Gone,
    /// A terminal of that name that does not host that runner: why, as the
    /// host saw it (a dead pane and its status, or what the pane was started
    /// with and runs now).
    Untracked(String),
}

/// The most bytes one [`TerminalHost::read_from`] returns: a file an agent
/// writes cannot make the Bridge hold an unbounded buffer.
pub const READ_LIMIT: usize = 8 << 20;

/// What a terminal host that watches agents itself (Herdr) says an agent
/// is doing ([`TerminalHost::agent`]). Herdr's `done` (idle after work
/// nobody has looked at yet) is [`AgentStatus::Idle`]: whether anyone saw
/// the work is no state of the agent's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentStatus {
    /// At its prompt, nothing running that the host can see.
    Idle,
    /// A turn runs.
    Working,
    /// A dialog of the agent's waits for an answer.
    Blocked,
    /// The host cannot tell (starting up, an unknown screen).
    Unknown,
}

impl AgentStatus {
    /// The status as Herdr names it (`idle`, `done`, `working`, `blocked`,
    /// `unknown`); anything else is [`AgentStatus::Unknown`].
    pub fn from_herdr(status: &str) -> Self {
        match status {
            "idle" | "done" => Self::Idle,
            "working" => Self::Working,
            "blocked" => Self::Blocked,
            _ => Self::Unknown,
        }
    }
}

/// One reading of the agent a terminal hosts, as the terminal host's own
/// agent detection has it: a second witness beside the Bridge's own reading
/// of the pane, never the state machine itself (crate::bridge).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentReading {
    /// The agent the host detected in the pane (`claude`, `agy`); `None`
    /// when it detects none there (a shell, an engine that exited).
    pub agent: Option<String>,
    /// What it is doing.
    pub status: AgentStatus,
    /// The host's rule that decided the status (`permission_prompt`);
    /// `None` when the status is the host's fallback (idle for a known agent
    /// whose screen matched no rule).
    pub rule: Option<String>,
    /// The rule saw a dialog on screen (Herdr's `visible_blocker`).
    pub blocker_visible: bool,
    /// The native session the agent reported to the host, when it did.
    pub session: Option<String>,
    /// The host's count of the agent's status changes: the same number is
    /// the same reading.
    pub seq: u64,
    /// The version of the detection rules the host read the screen with.
    pub manifest: Option<String>,
}

/// Named interactive terminals.
pub trait TerminalHost: Send + Sync {
    /// Start a detached terminal that runs the launch.
    fn start(&self, launch: &Launch) -> Result<(), TerminalError>;

    // Every call after `start` names the workspace again, so a host that
    // runs inside the workspace needs no memory between processes.

    /// Hold `text` for the terminal's next [`Self::submit_staged`]. Nothing
    /// reaches the pane: a failure here typed nothing, so the caller may try
    /// again. Staging again replaces what was held.
    fn stage_text(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
        text: &str,
    ) -> Result<(), TerminalError>;

    /// Type what [`Self::stage_text`] held, literally, then press Enter. A
    /// failure here may have typed some or all of it.
    fn submit_staged(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
    ) -> Result<(), TerminalError>;

    /// Type `text` literally, then press Enter: [`Self::stage_text`], then
    /// [`Self::submit_staged`]. A caller that records what it typed calls
    /// the two itself, so a failure before any key went is known as one.
    fn submit_text(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
        text: &str,
    ) -> Result<(), TerminalError> {
        self.stage_text(terminal, workspace, text)?;
        self.submit_staged(terminal, workspace)
    }

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

    /// Whether the terminal server that holds the terminals is there. A
    /// host whose terminals share one server (tmux) that has gone took
    /// every terminal in it with it, which is how a host restart is told
    /// from one terminal that vanished ([`crate::event::EndCause`], and the
    /// recovery's `host_restart`). A host that cannot tell answers true:
    /// an execution then reads as crashed, never as a host restart.
    fn server_alive(&self) -> bool {
        !matches!(self.server(), ServerSeen::Gone)
    }

    /// The terminal server that answers now, and which one it is: a server
    /// is told from another as a process is, by its pid and when it
    /// started, never by whether something answers. A server that answered
    /// when an execution's terminal started and answers now under another
    /// identity is a new one: the one that ran the execution is gone, and
    /// every terminal with it (the recovery's `host_restart`, whatever
    /// started the new server since). A host that cannot tell answers
    /// [`ServerSeen::Answers`] with no identity: an execution then reads as
    /// crashed, never as a host restart it cannot show.
    fn server(&self) -> ServerSeen {
        ServerSeen::Answers(None)
    }

    /// Whether the terminal hosts the runner this host started in it
    /// ([`Self::start`]), alive: what the Bridge asks before it calls an
    /// execution running or types into it. A terminal of that name can
    /// outlive its runner: a pane left dead (`remain-on-exit`), or a session
    /// a terminal manager restored after a restart, at a shell prompt. Such
    /// a terminal runs no engine of Spawn's, whatever its screen shows, so
    /// its execution is lost ([`Runner::Untracked`], with what the host saw).
    /// A host that cannot tell answers from [`Self::is_alive`].
    fn runner(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
    ) -> Result<Runner, TerminalError> {
        Ok(if self.is_alive(terminal, workspace)? {
            Runner::Running
        } else {
            Runner::Gone
        })
    }

    /// [`Self::runner`] for a whole fleet in one call, in the order given: a
    /// host that can ask its terminal server once for every pane does, so a
    /// pass over a fleet costs one call and not one per terminal. Each
    /// terminal not there is [`Runner::Gone`]; an error says nothing about
    /// any of them. The default asks [`Self::runner`] for each.
    fn runners(
        &self,
        terminals: &[(TerminalName, WorkspaceId)],
    ) -> Result<Vec<(TerminalName, Runner)>, TerminalError> {
        let mut states = Vec::with_capacity(terminals.len());
        for (terminal, workspace) in terminals {
            states.push((terminal.clone(), self.runner(terminal, workspace)?));
        }
        Ok(states)
    }

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

    /// Replace the regular file at `path`, as the terminal's process sees
    /// it, with `bytes`, if it still holds exactly `current`: whether it was
    /// replaced (`false`: it changed, or is gone, since it was read). Never
    /// shorter: `bytes` shorter than `current` is an error and nothing is
    /// written. The bytes go to a new file beside it first, made durable,
    /// and a rename puts it in place: the file is whole, old or new, at
    /// every moment. How the Bridge puts back a transcript that lost its
    /// head (S16). A host that cannot is an error, never a silent no-op.
    fn replace_grown(
        &self,
        _workspace: &WorkspaceId,
        path: &str,
        _bytes: &[u8],
        _current: &[u8],
    ) -> Result<bool, TerminalError> {
        Err(TerminalError(format!(
            "this terminal host cannot replace {path}"
        )))
    }

    /// Kill the terminal and its process, and return only once the runner
    /// and every process it started have exited, so nothing of the engine's
    /// can write to the workspace after it (the Bridge's final checkpoint
    /// comes next). A process that outlives a bounded wait and a kill is an
    /// error. A missing terminal is not an error.
    fn terminate(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
    ) -> Result<(), TerminalError>;

    /// Whether [`Self::terminate`] waits for processes to exit (a real
    /// host's bounded wait, seconds): the Bridge then runs it off its loop,
    /// so one end does not hold every other execution's turn for that long.
    /// A host whose terminate returns at once (in memory) says no, and the
    /// Bridge calls it in line.
    fn terminate_waits(&self) -> bool {
        false
    }

    /// What the host's own agent detection says the terminal's agent is
    /// doing: `None` from a host that has none (tmux, in memory), which
    /// leaves the Bridge on its own reading of the pane, exactly as before.
    /// A host that has one answers for every terminal, a missing one too
    /// (no agent there).
    fn agent(
        &self,
        _terminal: &TerminalName,
        _workspace: &WorkspaceId,
    ) -> Result<Option<AgentReading>, TerminalError> {
        Ok(None)
    }

    /// The terminals whose agent changed since the last call, as the host
    /// was told (never blocks): what makes the Bridge read a pane sooner
    /// than its backstop. Empty from a host with no agent detection.
    fn agent_changes(&self) -> Vec<TerminalName> {
        Vec::new()
    }
}

/// Why a watch could not be set: the platform has none, the filesystem does
/// not support one, or the platform's watch limit is reached.
#[derive(Debug, Error)]
#[error("watch: {0}")]
pub struct WatchError(pub String);

/// A change watch on files a running engine writes (its transcript and the
/// exit status its runner leaves), so the Bridge reads a file when it
/// changed instead of on every pass.
///
/// The watch names no process and holds nothing of the Bridge's state: a
/// key is a name the caller chooses, and every path watched under it is the
/// caller's interest. A path that does not exist yet may be watched,
/// including one whose parent directory does not exist yet: what is watched
/// then is the deepest directory above it that does, and the watch moves
/// down as the directories appear.
pub trait FileWatch: Send + Sync + std::fmt::Debug {
    /// Watch `path` for `key`: a change to it (a write, a creation, a
    /// removal, a replacement) is reported by the next [`Self::drain`].
    /// Watching a path twice under one key changes nothing. A path that
    /// cannot be watched is an error, and the caller reads that file on
    /// every pass instead.
    fn watch(&self, key: &str, path: &str) -> Result<(), WatchError>;

    /// Watch nothing more for `key`, and forget every change it reported
    /// that no [`Self::drain`] took yet.
    fn unwatch(&self, key: &str);

    /// The keys whose paths changed since the last call, each at most once.
    fn drain(&self) -> Vec<String>;

    /// Wait until this watch may have changes. `Ok(None)` means the adapter
    /// cannot wait; `Ok(Some(false))` means timeout.
    fn wait(&self, _timeout: Duration) -> Result<Option<bool>, WatchError> {
        Ok(None)
    }
}

/// One engine launch a filesystem view is asked to wrap ([`EngineView`]).
#[derive(Debug, Clone, Copy)]
pub struct ViewedLaunch<'a> {
    /// The engine's driver id (`agy`).
    pub engine: &'a str,
    /// The workspace the engine runs in, bound writable.
    pub workspace: &'a str,
    /// The engine's own home (its transcripts), bound writable.
    pub engine_home: &'a str,
    /// The `TMPDIR` the launch's environment names, if any.
    pub tmpdir: Option<&'a str>,
}

/// The filesystem view an engine with no deny rules of its own
/// ([`crate::driver::Guard::Sandbox`]) runs in (decision C2, N6): the host's
/// filesystem read-only, its credentials, the other engines' homes and the
/// terminal servers' sockets masked, and only the workspace, the engine's
/// home and `TMPDIR` writable. The host's is `bwrap`
/// (`spawn_host::engine_view`).
pub trait EngineView: Send + Sync + std::fmt::Debug {
    /// Its mechanism's name (`bwrap`).
    fn name(&self) -> &'static str;

    /// The words that run a program inside the view for `launch`: the
    /// view's program and its arguments, ending in `--`, after which the
    /// launch's own program and arguments go. Resolved again at every
    /// launch. An error says why this launch cannot have its view: the
    /// engine is not started.
    fn wrap(&self, launch: &ViewedLaunch<'_>) -> Result<Vec<String>, String>;
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

    /// Record where the archive stands with one native session
    /// ([`crate::transcripts::ArchiveProgress`]), in place of what was
    /// recorded for it before; `durable` makes it survive a crash of the
    /// host (an ended execution's, which is not written again).
    fn put_progress(
        &self,
        progress: &crate::transcripts::ArchiveProgress,
        durable: bool,
    ) -> Result<(), ArchiveError>;

    /// Every native session's latest [`Self::put_progress`]. One that
    /// cannot be read is left out.
    fn progress(&self) -> Result<Vec<crate::transcripts::ArchiveProgress>, ArchiveError>;

    /// Copy `copy`'s record ([`crate::transcripts::CopySource`], objective
    /// 175): where in the transcript it stands. `None` when none was put
    /// (a copy made before 175, or none at all).
    fn source(
        &self,
        agent: &AgentId,
        native: &str,
        copy: usize,
    ) -> Result<Option<crate::transcripts::CopySource>, ArchiveError>;

    /// Replace copy `copy`'s record whole, durably: after each append, so a
    /// copy whose length is not its record's was cut by a crash between the
    /// two.
    fn put_source(
        &self,
        agent: &AgentId,
        native: &str,
        copy: usize,
        source: &crate::transcripts::CopySource,
    ) -> Result<(), ArchiveError>;
}

/// The credential mask the transcript archive copies through (objective
/// 175): built from the places the value collector gives, never from a
/// value passed through a file, a pipe or an argument.
pub trait ArchiveMask: Send + Sync {
    /// The mask for this pass, or why it cannot be had (paths only, never
    /// a value): without it nothing is copied.
    ///
    /// # Errors
    ///
    /// Why the mask cannot be built now.
    fn mask(&self) -> Result<std::sync::Arc<crate::mask::Mask>, String>;

    /// Whether `mask`, given by [`Self::mask`], is still the one its
    /// sources give: false once a credential file, a watched directory or a
    /// probed path changed since it was built.
    fn current(&self, mask: &std::sync::Arc<crate::mask::Mask>) -> bool;
}
