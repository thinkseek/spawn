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
    Appended, EventKind, EventRecord, GlobalPosition, NewEvent, SessionSeq, Source,
};
use crate::id::{
    BranchName, CommandId, CommitId, IdempotencyKey, Principal, SessionId, TerminalName,
    WorkspaceId,
};
use crate::port::{
    CommandQueue, Commit, EventLog, Launch, StoreError, TerminalError, TerminalHost,
    WorkspaceError, WorkspaceStore,
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
}

/// All in-memory state behind one lock.
#[derive(Debug, Default)]
struct State {
    events: Vec<EventRecord>,
    sequences: HashMap<SessionId, u64>,
    next_position: u64,
    commands: Vec<CommandRow>,
    next_command: u64,
    branches: HashSet<BranchName>,
    mounts: HashMap<WorkspaceId, MountState>,
    commits: u64,
    terminals: HashSet<TerminalName>,
    panes: HashMap<TerminalName, String>,
    files: HashMap<String, Vec<u8>>,
}

impl State {
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

    /// Append to a file the terminal host can read, as an engine would.
    /// Test helper.
    pub fn append_file(&self, path: &str, bytes: &str) {
        self.lock()
            .files
            .entry(path.to_string())
            .or_default()
            .extend_from_slice(bytes.as_bytes());
    }

    /// Mark a workspace as holding uncommitted changes. Test helper.
    pub fn mark_dirty(&self, workspace: &WorkspaceId) {
        if let Some(mount) = self.lock().mounts.get_mut(workspace) {
            mount.dirty = true;
        }
    }
}

impl EventLog for MemoryStore {
    fn append(&self, event: NewEvent) -> Result<Appended, StoreError> {
        let now = self.clock.now();
        let (record, inserted) = self.lock().insert(event, now);
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
            Outcome::Failed(reason) => Some(reason.clone()),
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

    fn ensure_branch(&self, branch: &BranchName) -> Result<(), WorkspaceError> {
        self.lock().branches.insert(branch.clone());
        Ok(())
    }

    fn mount(&self, branch: &BranchName) -> Result<WorkspaceId, WorkspaceError> {
        let mut state = self.lock();
        if !state.branches.contains(branch) {
            return Err(WorkspaceError::Refused {
                code: "NoSuchBranch".to_string(),
                message: branch.to_string(),
            });
        }
        let number = state.mounts.len() + 1;
        let workspace = WorkspaceId::new(format!("{number}:/workspaces/{number}"))
            .map_err(|error| WorkspaceError::Unavailable(error.to_string()))?;
        state
            .mounts
            .insert(workspace.clone(), MountState { dirty: false });
        Ok(workspace)
    }

    fn commit(&self, workspace: &WorkspaceId, _message: &str) -> Result<Commit, WorkspaceError> {
        let mut state = self.lock();
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
        state.commits += 1;
        Ok(Commit::Created(
            CommitId::new(format!("c_{}", state.commits))
                .map_err(|error| WorkspaceError::Unavailable(error.to_string()))?,
        ))
    }

    fn unmount(&self, workspace: &WorkspaceId) -> Result<(), WorkspaceError> {
        self.lock().mounts.remove(workspace);
        Ok(())
    }
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
        state.panes.entry(launch.terminal.clone()).or_default();
        Ok(())
    }

    fn submit_text(
        &self,
        terminal: &TerminalName,
        _workspace: &WorkspaceId,
        text: &str,
    ) -> Result<(), TerminalError> {
        let mut state = self.lock();
        if !state.terminals.contains(terminal) {
            return Err(TerminalError(format!("terminal {terminal} is gone")));
        }
        let pane = state.panes.entry(terminal.clone()).or_default();
        pane.push_str(text);
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
        Ok(self.lock().terminals.contains(terminal))
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
            bytes[start..].to_vec()
        }))
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
