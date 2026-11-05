//! In-memory implementations of every port.
//!
//! These backends follow the same rules as the PostgreSQL adapters: a
//! repeated source key is stored once, session sequences are dense and
//! monotonic, and one session never holds two live leases. The shared
//! conformance suite in [`crate::contract`] runs against both.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::command::{Claimed, Outcome, Submission};
use crate::event::{
    Appended, EventKind, EventRecord, Fork, GlobalPosition, NewEvent, SessionSeq, Source,
};
use crate::id::{
    AgentId, BranchName, CommandId, CommitId, ExecutionId, IdempotencyKey, Principal, SessionId,
    TerminalName, WorkspaceId,
};
use crate::port::{
    ArchiveError, BaseMerge, Branch, CommandQueue, Commit, EventLog, ForkFrom, Launch, READ_LIMIT,
    StoreError, TerminalError, TerminalHost, TranscriptArchive, WorkspaceError, WorkspaceStore,
};
use crate::time::{Clock, Timestamp};

/// A command inside the queue.
#[derive(Debug, Clone)]
struct CommandRow {
    id: CommandId,
    principal: Principal,
    key: IdempotencyKey,
    submission: Submission,
    finished: bool,
    lease: Option<Lease>,
}

/// One worker holding one command.
#[derive(Debug, Clone)]
struct Lease {
    attempt: u32,
    expires: Timestamp,
}

/// A mounted workspace and its uncommitted state.
#[derive(Debug, Clone)]
struct MountState {
    dirty: bool,
    branch: BranchName,
}

/// All in-memory state behind one lock.
#[derive(Debug, Default)]
struct State {
    events: Vec<EventRecord>,
    sequences: HashMap<SessionId, u64>,
    next_position: u64,
    commands: Vec<CommandRow>,
    next_command: u64,
    /// Branch heads.
    heads: HashMap<BranchName, CommitId>,
    /// Every commit the store issued.
    known: HashSet<CommitId>,
    /// Every commit each branch contains (the head and its ancestors), so a
    /// merge the branch already holds is current, like `merge-base
    /// --is-ancestor main head`.
    history: HashMap<BranchName, HashSet<CommitId>>,
    mounts: HashMap<WorkspaceId, MountState>,
    /// Mounts issued so far: each mount's id is new, never one a mounted
    /// or an unmounted workspace had.
    mounts_issued: u64,
    commits: u64,
    /// What `merge_base` answers.
    base_merge: BaseMerge,
    /// Workspace calls in order, for tests of the call order.
    workspace_calls: Vec<String>,
    /// Commits fail while set.
    commit_fails: bool,
    /// How many of the next `submit_text` calls fail, after their text may
    /// have reached the engine.
    submit_failures: u32,
    /// Commits and merges answer `WorkspaceStale` while set.
    stale: bool,
    /// The next append of an `artifact.committed` event fails.
    branch_record_fails: bool,
    /// Sessions whose `artifact.committed` and `merge.conflicted` appends
    /// all fail, as a record the log can never take would.
    branch_records_refused: HashSet<SessionId>,
    terminals: HashSet<TerminalName>,
    /// Every terminal start, in order.
    launches: Vec<Launch>,
    panes: HashMap<TerminalName, String>,
    /// Text staged for a terminal's next submit.
    staged: HashMap<TerminalName, String>,
    files: HashMap<String, Vec<u8>>,
    /// Engines that exit at the next liveness check: the file is written
    /// and the terminal closes then, between the Bridge's two reads.
    exits_on_check: HashMap<TerminalName, (String, Vec<u8>)>,
    /// The transcript archive: per agent and native session, its copies.
    transcripts: HashMap<(AgentId, String), Vec<Vec<u8>>>,
}

impl State {
    /// The session's latest execution when it has no end the Bridge wrote:
    /// a start of another is refused, as `spawn_append_trusted` refuses it.
    fn open_execution(&self, session: &SessionId) -> Option<ExecutionId> {
        let (index, started) =
            self.events
                .iter()
                .enumerate()
                .rev()
                .find_map(|(index, record)| {
                    let event = &record.event;
                    let started = event.execution_id.as_ref()?;
                    (event.session_id == *session
                        && matches!(event.kind, EventKind::ExecutionStarted { .. }))
                    .then_some((index, started))
                })?;
        let ended = self.events[index + 1..].iter().any(|record| {
            record.event.session_id == *session
                && record.event.source == Source::Bridge
                && record.event.execution_id.as_ref() == Some(started)
                && matches!(record.event.kind, EventKind::ExecutionEnded { .. })
        });
        (!ended).then(|| started.clone())
    }

    /// Whether `event` repeats a stored `(session, source, key)`.
    fn repeats(&self, event: &NewEvent) -> bool {
        event.source_event_id.as_deref().is_some_and(|key| {
            self.events.iter().any(|record| {
                record.event.session_id == event.session_id
                    && record.event.source == event.source
                    && record.event.source_event_id.as_deref() == Some(key)
            })
        })
    }

    fn insert(&mut self, event: NewEvent, now: Timestamp) -> (EventRecord, bool) {
        if let Some(key) = event.source_event_id.as_deref() {
            let repeated = self.events.iter().find(|record| {
                record.event.session_id == event.session_id
                    && record.event.source == event.source
                    && record.event.source_event_id.as_deref() == Some(key)
            });
            if let Some(record) = repeated {
                return (record.clone(), false);
            }
        }
        self.next_position += 1;
        let position = GlobalPosition(self.next_position);
        let sequence = self.sequences.entry(event.session_id.clone()).or_insert(0);
        *sequence += 1;
        let record = EventRecord {
            position,
            session_seq: SessionSeq(*sequence),
            recorded_at: now,
            event,
        };
        self.events.push(record.clone());
        (record, true)
    }
}

/// Memory backend for every port.
pub struct MemoryStore {
    project: String,
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
}

impl MemoryStore {
    /// Store for one project.
    pub fn new(project: &str, clock: Arc<dyn Clock>) -> Self {
        Self {
            project: project.to_string(),
            clock,
            state: Mutex::new(State::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Replace the visible pane of a terminal. Test helper.
    pub fn set_pane(&self, terminal: &TerminalName, pane: &str) {
        self.lock().panes.insert(terminal.clone(), pane.to_string());
    }

    /// Make the engine of `terminal` exit at the next liveness check:
    /// `path` gets `bytes` and the terminal closes in that same moment.
    /// Test helper for the race between reading the exit file and asking
    /// whether the terminal lives.
    pub fn exit_on_next_check(&self, terminal: &TerminalName, path: &str, bytes: &str) {
        self.lock().exits_on_check.insert(
            terminal.clone(),
            (path.to_string(), bytes.as_bytes().to_vec()),
        );
    }

    /// Append to a file the terminal host can read, as an engine would.
    /// Test helper.
    pub fn append_file(&self, path: &str, bytes: &str) {
        self.lock()
            .files
            .entry(path.to_string())
            .or_default()
            .extend_from_slice(bytes.as_bytes());
    }

    /// Replace a file the terminal host can read, as an engine that
    /// rewrites or truncates it would. Test helper.
    pub fn replace_file(&self, path: &str, bytes: &[u8]) {
        self.lock().files.insert(path.to_string(), bytes.to_vec());
    }

    /// Delete a file the terminal host can read, as Claude Code's cleanup
    /// would. Test helper.
    pub fn remove_file(&self, path: &str) {
        self.lock().files.remove(path);
    }

    /// What `merge_base` answers from now on (a merge moves the branch to
    /// its commit). Test helper.
    pub fn set_base_merge(&self, merge: BaseMerge) {
        self.lock().base_merge = merge;
    }

    /// The workspace calls so far, in order: `ensure_branch <branch>
    /// <from or ->`, `mount <branch>`, `commit`, `merge_base`, `unmount`.
    /// Test helper.
    pub fn workspace_calls(&self) -> Vec<String> {
        self.lock().workspace_calls.clone()
    }

    /// Every terminal start so far, in order. Test helper.
    pub fn launches(&self) -> Vec<Launch> {
        self.lock().launches.clone()
    }

    /// Make the next text submitted to a terminal fail, as a send whose
    /// keys may have gone before it broke. Test helper.
    pub fn fail_next_submit(&self) {
        self.lock().submit_failures = 1;
    }

    /// Make every commit fail (`true`) or work again. Test helper.
    pub fn fail_commits(&self, fail: bool) {
        self.lock().commit_fails = fail;
    }

    /// Make every commit and merge answer `WorkspaceStale` (`true`), as a
    /// store does that kept the workspace's state off its branch, or work
    /// again. Test helper.
    pub fn make_stale(&self, stale: bool) {
        self.lock().stale = stale;
    }

    /// Make the next append of an `artifact.committed` or `merge.conflicted`
    /// event fail, as a log that is down would. Test helper.
    pub fn fail_next_branch_record(&self) {
        self.lock().branch_record_fails = true;
    }

    /// Make every append of an `artifact.committed` or `merge.conflicted`
    /// event of `session` fail (`true`) or work again. Test helper.
    pub fn refuse_branch_records_of(&self, session: &SessionId, refuse: bool) {
        let mut state = self.lock();
        if refuse {
            state.branch_records_refused.insert(session.clone());
        } else {
            state.branch_records_refused.remove(session);
        }
    }

    /// Mark a workspace as holding uncommitted changes. Test helper.
    pub fn mark_dirty(&self, workspace: &WorkspaceId) {
        if let Some(mount) = self.lock().mounts.get_mut(workspace) {
            mount.dirty = true;
        }
    }
}

impl EventLog for MemoryStore {
    fn append(&self, mut event: NewEvent) -> Result<Appended, StoreError> {
        crate::redact::redact_event(&mut event.kind);
        let now = self.clock.now();
        let mut state = self.lock();
        let branch_record = matches!(
            event.kind,
            EventKind::ArtifactCommitted { .. } | EventKind::MergeConflicted { .. }
        );
        if state.branch_record_fails && branch_record {
            state.branch_record_fails = false;
            return Err(StoreError::Backend("the log is down".to_string()));
        }
        if branch_record && state.branch_records_refused.contains(&event.session_id) {
            return Err(StoreError::Backend(
                "the log refuses this record".to_string(),
            ));
        }
        if matches!(event.kind, EventKind::ExecutionStarted { .. })
            && !state.repeats(&event)
            && let Some(open) = state.open_execution(&event.session_id)
        {
            return Err(StoreError::Rejected(format!(
                "session {} already runs execution {open}",
                event.session_id
            )));
        }
        let (record, inserted) = state.insert(event, now);
        Ok(Appended { record, inserted })
    }

    fn read_global(
        &self,
        after: GlobalPosition,
        limit: usize,
    ) -> Result<Vec<EventRecord>, StoreError> {
        Ok(self
            .lock()
            .events
            .iter()
            .filter(|record| record.position > after)
            .take(limit)
            .cloned()
            .collect())
    }
}

impl CommandQueue for MemoryStore {
    fn submit(
        &self,
        principal: &Principal,
        submission: &Submission,
    ) -> Result<CommandId, StoreError> {
        let mut state = self.lock();
        if let Some(row) = state
            .commands
            .iter()
            .find(|row| &row.principal == principal && row.key == submission.idempotency_key)
        {
            return Ok(row.id);
        }
        state.next_command += 1;
        let id = CommandId(state.next_command);
        state.commands.push(CommandRow {
            id,
            principal: principal.clone(),
            key: submission.idempotency_key.clone(),
            submission: submission.clone(),
            finished: false,
            lease: None,
        });
        let event = NewEvent {
            agent_id: submission.agent_id.clone(),
            session_id: submission.session_id.clone(),
            execution_id: None,
            source: Source::Gateway,
            source_event_id: Some(format!("{id}:accepted")),
            kind: EventKind::CommandAccepted { command_id: id },
        };
        let now = self.clock.now();
        state.insert(event, now);
        Ok(id)
    }

    fn claim(&self, _worker: &str, ttl: Duration) -> Result<Option<Claimed>, StoreError> {
        let mut state = self.lock();
        let now = self.clock.now();
        let candidate = {
            let commands = &state.commands;
            commands
                .iter()
                .enumerate()
                .filter(|(_, row)| !row.finished)
                .filter(|(index, row)| {
                    !commands[..*index].iter().any(|older| {
                        older.submission.session_id == row.submission.session_id && !older.finished
                    })
                })
                .filter(|(_, row)| row.lease.as_ref().is_none_or(|lease| lease.expires <= now))
                .map(|(index, _)| index)
                .min()
        };
        let Some(index) = candidate else {
            return Ok(None);
        };
        let attempt = state.commands[index]
            .lease
            .as_ref()
            .map_or(1, |lease| lease.attempt + 1);
        let submission = state.commands[index].submission.clone();
        let id = state.commands[index].id;
        state.commands[index].lease = Some(Lease {
            attempt,
            expires: now.plus(ttl),
        });
        let action = serde_json::to_value(&submission.action)
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        Ok(Some(Claimed {
            command_id: id,
            attempt,
            agent_id: submission.agent_id,
            session_id: submission.session_id,
            action,
        }))
    }

    fn finish(&self, claimed: &Claimed, outcome: &Outcome) -> Result<(), StoreError> {
        let mut state = self.lock();
        let failure = match outcome {
            Outcome::Succeeded => None,
            Outcome::Failed(reason) => Some(crate::redact::redact(reason)),
        };
        let (agent_id, session_id) = {
            let row = state
                .commands
                .iter_mut()
                .find(|row| row.id == claimed.command_id)
                .ok_or_else(|| {
                    StoreError::Backend(format!("unknown command {}", claimed.command_id))
                })?;
            if row.finished {
                return Ok(());
            }
            let attempt = row.lease.as_ref().map_or(0, |lease| lease.attempt);
            if attempt != claimed.attempt {
                return Err(StoreError::LeaseLost {
                    command: claimed.command_id,
                    attempt: claimed.attempt,
                });
            }
            row.finished = true;
            (
                row.submission.agent_id.clone(),
                row.submission.session_id.clone(),
            )
        };
        let event = NewEvent {
            agent_id,
            session_id,
            execution_id: None,
            source: Source::Gateway,
            source_event_id: Some(format!("{}:finished", claimed.command_id)),
            kind: EventKind::CommandFinished {
                command_id: claimed.command_id,
                succeeded: failure.is_none(),
                error: failure,
            },
        };
        let now = self.clock.now();
        state.insert(event, now);
        Ok(())
    }
}

impl WorkspaceStore for MemoryStore {
    fn project(&self) -> &str {
        &self.project
    }

    fn ensure_branch(
        &self,
        branch: &BranchName,
        from: Option<&str>,
    ) -> Result<Branch, WorkspaceError> {
        let mut state = self.lock();
        state
            .workspace_calls
            .push(format!("ensure_branch {branch} {}", from.unwrap_or("-")));
        if state.heads.contains_key(branch) {
            return Ok(Branch::Existing);
        }
        let (commit, mut history) = match from.map(ForkFrom::parse).transpose()? {
            None => (base_commit(), HashSet::new()),
            Some(ForkFrom::Agent(agent)) => {
                let fork = BranchName::for_agent(&agent);
                let head =
                    state
                        .heads
                        .get(&fork)
                        .cloned()
                        .ok_or_else(|| WorkspaceError::Refused {
                            code: "NoSuchFrom".to_string(),
                            message: format!("{agent} has no branch"),
                        })?;
                (head, state.history.get(&fork).cloned().unwrap_or_default())
            }
            Some(ForkFrom::Commit(commit)) => {
                if commit != base_commit() && !state.known.contains(&commit) {
                    return Err(WorkspaceError::Refused {
                        code: "NoSuchFrom".to_string(),
                        message: format!("no commit {commit}"),
                    });
                }
                (commit, HashSet::new())
            }
        };
        history.insert(commit.clone());
        state.history.insert(branch.clone(), history);
        state.heads.insert(branch.clone(), commit.clone());
        Ok(Branch::Created(Fork {
            from: from.unwrap_or("main").to_string(),
            commit,
        }))
    }

    fn mount(&self, branch: &BranchName) -> Result<WorkspaceId, WorkspaceError> {
        let mut state = self.lock();
        state.workspace_calls.push(format!("mount {branch}"));
        if !state.heads.contains_key(branch) {
            return Err(WorkspaceError::Refused {
                code: "NoSuchBranch".to_string(),
                message: branch.to_string(),
            });
        }
        state.mounts_issued += 1;
        let number = state.mounts_issued;
        let workspace = WorkspaceId::new(format!("{number}:/workspaces/{number}"))
            .map_err(|error| WorkspaceError::Unavailable(error.to_string()))?;
        state.mounts.insert(
            workspace.clone(),
            MountState {
                dirty: false,
                branch: branch.clone(),
            },
        );
        Ok(workspace)
    }

    fn commit(&self, workspace: &WorkspaceId, _message: &str) -> Result<Commit, WorkspaceError> {
        let mut state = self.lock();
        state.workspace_calls.push("commit".to_string());
        if state.commit_fails {
            return Err(WorkspaceError::Unavailable("commit fails".to_string()));
        }
        if state.stale {
            return Err(stale_refusal());
        }
        let Some(mount) = state.mounts.get_mut(workspace) else {
            return Err(WorkspaceError::Refused {
                code: "NoSuchWorkspace".to_string(),
                message: workspace.to_string(),
            });
        };
        if !mount.dirty {
            return Ok(Commit::Unchanged);
        }
        mount.dirty = false;
        let branch = mount.branch.clone();
        state.commits += 1;
        let commit = CommitId::new(format!("{:040x}", state.commits))
            .map_err(|error| WorkspaceError::Unavailable(error.to_string()))?;
        state.known.insert(commit.clone());
        state
            .history
            .entry(branch.clone())
            .or_default()
            .insert(commit.clone());
        state.heads.insert(branch, commit.clone());
        Ok(Commit::Created(commit))
    }

    fn unmount(&self, workspace: &WorkspaceId) -> Result<(), WorkspaceError> {
        let mut state = self.lock();
        state.workspace_calls.push("unmount".to_string());
        state.mounts.remove(workspace);
        Ok(())
    }

    fn merge_base(&self, workspace: &WorkspaceId) -> Result<BaseMerge, WorkspaceError> {
        let mut state = self.lock();
        state.workspace_calls.push("merge_base".to_string());
        if state.stale {
            return Err(stale_refusal());
        }
        let Some(branch) = state
            .mounts
            .get(workspace)
            .map(|mount| mount.branch.clone())
        else {
            return Err(WorkspaceError::Refused {
                code: "NoSuchWorkspace".to_string(),
                message: workspace.to_string(),
            });
        };
        let merge = state.base_merge.clone();
        if let BaseMerge::Merged { commit, .. } = &merge {
            // A merge the branch already holds is current: asked again, it
            // never moves a branch that went on back to the merge.
            let history = state.history.entry(branch.clone()).or_default();
            if !history.insert(commit.clone()) {
                return Ok(BaseMerge::Current);
            }
            state.known.insert(commit.clone());
            state.heads.insert(branch, commit.clone());
        }
        Ok(merge)
    }

    fn merge_main_clean(&self, branch: &BranchName) -> Result<BaseMerge, WorkspaceError> {
        let mut state = self.lock();
        state.workspace_calls.push(format!("merge_main {branch}"));
        if !state.heads.contains_key(branch) {
            return Err(WorkspaceError::Refused {
                code: "NoSuchBranch".to_string(),
                message: branch.to_string(),
            });
        }
        match state.base_merge.clone() {
            BaseMerge::Current => Ok(BaseMerge::Current),
            BaseMerge::Merged { commit, files } => {
                // A merge the branch already holds is current.
                let history = state.history.entry(branch.clone()).or_default();
                if !history.insert(commit.clone()) {
                    return Ok(BaseMerge::Current);
                }
                state.known.insert(commit.clone());
                state.heads.insert(branch.clone(), commit.clone());
                Ok(BaseMerge::Merged { commit, files })
            }
            // A conflict leaves the branch as it is.
            BaseMerge::Conflicted { files, .. } => Ok(BaseMerge::Conflicted {
                files,
                marked: false,
            }),
        }
    }

    /// The memory store leaves no file out of a checkpoint.
    fn notices(&self, _workspace: &WorkspaceId) -> Vec<String> {
        Vec::new()
    }

    fn notices_told(&self, _workspace: &WorkspaceId, _notices: &[String]) {}

    fn branch_head(&self, branch: &BranchName) -> Result<Option<CommitId>, WorkspaceError> {
        Ok(self.lock().heads.get(branch).cloned())
    }
}

/// The base commit of every memory branch.
fn base_commit() -> CommitId {
    CommitId::new("b".repeat(40)).unwrap_or_else(|_| unreachable!("a valid label"))
}

impl TerminalHost for MemoryStore {
    fn start(&self, launch: &Launch) -> Result<(), TerminalError> {
        let mut state = self.lock();
        if state.terminals.contains(&launch.terminal) {
            return Err(TerminalError(format!(
                "terminal {} already exists",
                launch.terminal
            )));
        }
        state.terminals.insert(launch.terminal.clone());
        state.launches.push(launch.clone());
        state.panes.entry(launch.terminal.clone()).or_default();
        Ok(())
    }

    fn stage_text(
        &self,
        terminal: &TerminalName,
        _workspace: &WorkspaceId,
        text: &str,
    ) -> Result<(), TerminalError> {
        // Like a tmux buffer: held by the server, whatever the pane.
        self.lock()
            .staged
            .insert(terminal.clone(), text.to_string());
        Ok(())
    }

    fn submit_staged(
        &self,
        terminal: &TerminalName,
        _workspace: &WorkspaceId,
    ) -> Result<(), TerminalError> {
        let mut state = self.lock();
        if !state.terminals.contains(terminal) {
            return Err(TerminalError(format!("terminal {terminal} is gone")));
        }
        if state.submit_failures > 0 {
            state.submit_failures -= 1;
            return Err(TerminalError("send-keys broke midway".to_string()));
        }
        let Some(text) = state.staged.remove(terminal) else {
            return Err(TerminalError(format!("nothing staged for {terminal}")));
        };
        let pane = state.panes.entry(terminal.clone()).or_default();
        pane.push_str(&text);
        pane.push('\n');
        Ok(())
    }

    fn send_keys(
        &self,
        terminal: &TerminalName,
        _workspace: &WorkspaceId,
        keys: &[&str],
    ) -> Result<(), TerminalError> {
        let mut state = self.lock();
        if !state.terminals.contains(terminal) {
            return Err(TerminalError(format!("terminal {terminal} is gone")));
        }
        let pane = state.panes.entry(terminal.clone()).or_default();
        for key in keys {
            pane.push_str(if *key == "Enter" { "\n" } else { key });
        }
        Ok(())
    }

    fn is_alive(
        &self,
        terminal: &TerminalName,
        _workspace: &WorkspaceId,
    ) -> Result<bool, TerminalError> {
        let mut state = self.lock();
        if let Some((path, bytes)) = state.exits_on_check.remove(terminal) {
            state.files.insert(path, bytes);
            state.terminals.remove(terminal);
        }
        Ok(state.terminals.contains(terminal))
    }

    fn capture_tail(
        &self,
        terminal: &TerminalName,
        _workspace: &WorkspaceId,
        lines: usize,
    ) -> Result<String, TerminalError> {
        let state = self.lock();
        if !state.terminals.contains(terminal) {
            return Err(TerminalError(format!("terminal {terminal} is gone")));
        }
        let pane = state.panes.get(terminal).map_or("", String::as_str);
        let all: Vec<&str> = pane.lines().collect();
        let start = all.len().saturating_sub(lines);
        Ok(all[start..].join("\n"))
    }

    fn read_from(
        &self,
        _workspace: &WorkspaceId,
        path: &str,
        offset: u64,
    ) -> Result<Option<Vec<u8>>, TerminalError> {
        let state = self.lock();
        Ok(state.files.get(path).map(|bytes| {
            let start = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(bytes.len());
            bytes[start..bytes.len().min(start.saturating_add(READ_LIMIT))].to_vec()
        }))
    }

    fn write_new(
        &self,
        _workspace: &WorkspaceId,
        path: &str,
        bytes: &[u8],
    ) -> Result<bool, TerminalError> {
        let mut state = self.lock();
        if state.files.contains_key(path) {
            return Ok(false);
        }
        state.files.insert(path.to_string(), bytes.to_vec());
        Ok(true)
    }

    fn terminate(
        &self,
        terminal: &TerminalName,
        _workspace: &WorkspaceId,
    ) -> Result<(), TerminalError> {
        let mut state = self.lock();
        state.terminals.remove(terminal);
        state.panes.remove(terminal);
        Ok(())
    }
}

/// What a stale workspace answers ([`crate::port::WORKSPACE_STALE`]).
fn stale_refusal() -> WorkspaceError {
    WorkspaceError::Refused {
        code: crate::port::WORKSPACE_STALE.to_string(),
        message: "kept off the branch".to_string(),
    }
}

impl TranscriptArchive for MemoryStore {
    fn copies(&self, agent: &AgentId, native: &str) -> Result<Vec<u64>, ArchiveError> {
        Ok(self
            .lock()
            .transcripts
            .get(&(agent.clone(), native.to_string()))
            .map(|copies| {
                copies
                    .iter()
                    .map(|copy| u64::try_from(copy.len()).unwrap_or(u64::MAX))
                    .collect()
            })
            .unwrap_or_default())
    }

    fn read(
        &self,
        agent: &AgentId,
        native: &str,
        copy: usize,
        offset: u64,
        limit: usize,
    ) -> Result<Vec<u8>, ArchiveError> {
        let state = self.lock();
        let bytes = state
            .transcripts
            .get(&(agent.clone(), native.to_string()))
            .and_then(|copies| copies.get(copy))
            .ok_or_else(|| ArchiveError(format!("no copy {copy} of {native}")))?;
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(bytes.len());
        Ok(bytes[start..bytes.len().min(start.saturating_add(limit))].to_vec())
    }

    fn append(
        &self,
        agent: &AgentId,
        native: &str,
        copy: usize,
        at: u64,
        bytes: &[u8],
    ) -> Result<(), ArchiveError> {
        let mut state = self.lock();
        let copies = state
            .transcripts
            .entry((agent.clone(), native.to_string()))
            .or_default();
        if copy == copies.len() {
            copies.push(Vec::new());
        }
        let held = copies
            .get_mut(copy)
            .ok_or_else(|| ArchiveError(format!("copy {copy} of {native} is past the next")))?;
        if u64::try_from(held.len()).unwrap_or(u64::MAX) != at {
            return Err(ArchiveError(format!(
                "copy {copy} of {native} holds {} bytes, not {at}",
                held.len()
            )));
        }
        held.extend_from_slice(bytes);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::MemoryStore;
    use crate::id::{BranchName, CommitId};
    use crate::port::{BaseMerge, Commit, WorkspaceStore};
    use crate::time::SystemClock;

    fn store_with(branches: &[&str]) -> MemoryStore {
        let store = MemoryStore::new("shop", Arc::new(SystemClock));
        for branch in branches {
            let branch = BranchName::new(*branch).expect("valid branch");
            store.ensure_branch(&branch, None).expect("branch");
        }
        store
    }

    #[test]
    fn a_remount_never_takes_the_id_of_another_branchs_mount() {
        let store = store_with(&["agent_a", "agent_b", "agent_c"]);
        let branch = |name: &str| BranchName::new(name).expect("valid branch");
        let a = store.mount(&branch("agent_a")).expect("mount a");
        let b = store.mount(&branch("agent_b")).expect("mount b");
        store.unmount(&a).expect("unmount a");
        let c = store.mount(&branch("agent_c")).expect("mount c");
        assert_ne!(c, b, "a new mount has a new id");
        assert_ne!(c, a, "not even the id of an unmounted one");

        // b's work still goes to b's branch, c's to c's.
        store.mark_dirty(&b);
        let Commit::Created(on_b) = store.commit(&b, "b").expect("commit b") else {
            panic!("b is dirty");
        };
        store.mark_dirty(&c);
        let Commit::Created(on_c) = store.commit(&c, "c").expect("commit c") else {
            panic!("c is dirty");
        };
        let heads = &store.lock().heads;
        assert_eq!(heads.get(&branch("agent_b")), Some(&on_b));
        assert_eq!(heads.get(&branch("agent_c")), Some(&on_c));
    }

    #[test]
    fn a_repeated_merge_never_moves_a_branch_back() {
        let store = store_with(&["agent_a"]);
        let branch = BranchName::new("agent_a").expect("valid branch");
        let workspace = store.mount(&branch).expect("mount");
        let merged = CommitId::new("e".repeat(40)).expect("valid commit");
        store.set_base_merge(BaseMerge::Merged {
            commit: merged.clone(),
            files: Vec::new(),
        });
        assert!(matches!(
            store.merge_base(&workspace).expect("merge"),
            BaseMerge::Merged { .. }
        ));
        store.mark_dirty(&workspace);
        let Commit::Created(after) = store.commit(&workspace, "work").expect("commit") else {
            panic!("dirty");
        };
        // The same main again: the branch holds it, so it is current.
        assert_eq!(
            store.merge_base(&workspace).expect("again"),
            BaseMerge::Current
        );
        assert_eq!(store.lock().heads.get(&branch), Some(&after));
    }
}
