//! Agent Bridge: turns commands into executions and persists what happens.
//!
//! The Bridge owns four jobs: start and resume engines, record events, commit
//! workspace state, and map native identifiers to Spawn sessions. It does not
//! run the agent loop and does not judge state.
//!
//! The Bridge is the only writer of a Bridge-sourced `execution.ended`, and
//! it writes that event only after the final checkpoint, the terminal kill
//! and the unmount. A refold of the log therefore never repeats cleanup.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;

use serde_json::json;

use crate::command::{Claimed, CommandAction, Outcome};
use crate::event::{
    ArtifactRef, CheckpointReason, CommandStep, ENGINE, EndCause, EventKind, EventRecord, Fork,
    GlobalPosition, ModelProfile, NewEvent, Source,
};
use crate::id::{
    AgentId, BranchName, CommandId, CommitId, ExecutionId, SessionId, TerminalName, WorkspaceId,
};
use crate::ingest::{claude_line, claude_version};
use crate::port::WORKSPACE_STALE;
use crate::port::{
    BaseMerge, Branch, CommandQueue, Commit, EventLog, Launch, READ_LIMIT, StoreError,
    TerminalError, TerminalHost, TranscriptArchive, WorkspaceError, WorkspaceStore,
};
use crate::screen::{ScreenState, TAIL_LINES, TRUST_KEYS, is_trust_dialog, read_claude_screen};
use crate::time::{Clock, SystemClock};
use crate::transcripts;

/// Final checkpoints an ending execution tries on consecutive ticks before
/// its end counts as stalled: the terminal is killed, the workspace stays
/// mounted, `execution.end_stalled` asks for attention, and the checkpoint is
/// retried every [`END_RETRY_MS`]. A workspace is unmounted only after its
/// final checkpoint commits, or on a `discard` command.
pub const END_ATTEMPTS: u32 = 20;

/// How often a stalled end retries its final checkpoint, by the Bridge's
/// clock.
pub const END_RETRY_MS: i64 = 30_000;

/// The session and settings of one launch.
///
/// Grouped so that [`Bridge::launch`] keeps a readable parameter list.
struct Run<'a> {
    native: Option<&'a str>,
    /// `native` names an earlier session to resume, not a new one.
    resume: bool,
    /// Claude Code's inline settings ([`claude_settings`]).
    settings: &'a str,
}

/// What one start asks for. Grouped so that [`Bridge::start`] keeps a
/// readable parameter list.
struct Begin<'a> {
    prompt: &'a str,
    parent: Option<&'a SessionId>,
    /// A session to resume.
    native: Option<&'a str>,
    /// Where a new branch starts (`start {from}`).
    from: Option<&'a str>,
    /// Models the engine runs on; unset keeps the session's last profile.
    model: Option<ModelProfile>,
}

/// The loopback relay engines reach their models through
/// (`scripts/model-relay.py`), and the models each profile names. The relay
/// routes `deepseek-*` models to DeepSeek with its own key and everything
/// else to Claude Code's gateway with the engine's own credentials, so
/// Spawn holds neither.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRelay {
    /// Base URL of the relay, `http://127.0.0.1:<port>`.
    pub url: String,
    /// The cheap profile's main model, also in its sonnet and opus slots.
    pub cheap_model: String,
    /// The small, fast model (the haiku slot) of every profile.
    pub small_model: String,
}

impl ModelRelay {
    /// Whether the relay accepts connections now. Checked at every start,
    /// so a relay that comes back is used again without a restart.
    pub fn reachable(&self) -> bool {
        let address = self
            .url
            .split_once("://")
            .map_or(self.url.as_str(), |(_, rest)| rest);
        let address = address.split('/').next().unwrap_or_default();
        let Ok(mut addresses) = std::net::ToSocketAddrs::to_socket_addrs(address) else {
            return false;
        };
        addresses.any(|address| {
            std::net::TcpStream::connect_timeout(&address, Duration::from_millis(500)).is_ok()
        })
    }
}

/// Bridge settings.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    /// Absolute path of the `spawn` binary, which runs the engine (`spawn run`).
    pub spawn_program: String,
    /// Claude Code executable.
    pub claude_program: String,
    /// Extra environment for the engine process, for example the database
    /// URL that the hook and the runner need.
    pub environment: Vec<(String, String)>,
    /// Environment that replaces entries of `environment` for one session,
    /// for example a different database login for the supervisor.
    pub session_environment: Vec<(String, Vec<(String, String)>)>,
    /// Worker name recorded in the lease.
    pub worker: String,
    /// Lease duration of one command.
    pub lease_ttl: Duration,
    /// Commands handled per tick.
    pub max_commands_per_tick: usize,
    /// Attempts to find a ready input box before typing.
    pub ready_attempts: usize,
    /// Delay between ready attempts.
    pub ready_interval: Duration,
    /// Where Claude Code writes session transcripts (its `projects`
    /// directory), as the terminal host sees it.
    pub claude_projects: String,
    /// A busy screen is recorded again this often while it stays busy, so
    /// the Observer keeps a current fact for a long turn.
    pub screen_refresh: Duration,
    /// The model relay, when Claude Code engines go through one.
    pub model_relay: Option<ModelRelay>,
}

impl BridgeConfig {
    /// Defaults for one worker.
    pub fn new(worker: &str) -> Self {
        Self {
            spawn_program: "spawn".to_string(),
            claude_program: "claude".to_string(),
            environment: Vec::new(),
            session_environment: Vec::new(),
            worker: worker.to_string(),
            lease_ttl: Duration::from_mins(2),
            max_commands_per_tick: 16,
            ready_attempts: 40,
            ready_interval: Duration::from_millis(250),
            claude_projects: "/root/.claude/projects".to_string(),
            screen_refresh: Duration::from_secs(30),
            model_relay: None,
        }
    }
}

/// Running execution of one session.
#[derive(Debug, Clone)]
struct Active {
    execution_id: ExecutionId,
    workspace: WorkspaceId,
    /// A tool or turn finished after the last commit.
    // ponytail: an `Unchanged` commit writes no event, so after a refold an
    // idle session repeats one no-op commit per tick; record attempts if
    // Workspace calls get expensive.
    checkpoint: Option<CheckpointReason>,
    /// The runner reported the engine exit; cleanup is still due.
    reported_end: Option<(EndCause, Option<i32>)>,
    /// The end committed its final checkpoint and killed the terminal
    /// (`execution.unmounting`): what is left is the unmount and the record.
    unmounting: bool,
    /// What the Bridge has read of the engine's files and screen.
    follow: Follow,
}

impl Active {
    /// The terminal that hosts the execution: always `spawn-<execution>`,
    /// so it is derived, never stored.
    fn terminal(&self) -> TerminalName {
        TerminalName::for_execution(&self.execution_id)
    }
}

/// The Bridge's own view of a running engine, rebuilt after a restart: read
/// positions start at zero (event keys make a second read a no-op), and the
/// last recorded screen is folded from the log.
#[derive(Debug, Clone, Default)]
struct Follow {
    /// Bytes of the transcript read so far.
    offset: u64,
    /// The last screen state recorded, and when.
    screen: Option<(ScreenState, i64)>,
    /// A state read once and not recorded yet: a screen is recorded when
    /// two readings in a row agree, so a frame mid-redraw is not a fact.
    seen: Option<ScreenState>,
    /// When this Bridge first followed the execution, by its own clock.
    first_seen: Option<i64>,
    /// The engine's file is missing after the start grace, or unreadable;
    /// warned once.
    warned: bool,
    /// The runner's exit file is unreadable; warned once.
    exit_warned: bool,
    /// Final checkpoints of this execution's end that failed. The
    /// workspace stays mounted until one succeeds (or a `discard`); from
    /// [`END_ATTEMPTS`] on the end is stalled.
    end_failures: u32,
    /// When the end's final checkpoint was last tried, by the Bridge's
    /// clock: a stalled end retries every [`END_RETRY_MS`].
    end_tried_at: Option<i64>,
    /// The workspace answered [`WORKSPACE_STALE`] with this message: it
    /// cannot checkpoint to its branch again, and its state is kept off it,
    /// so the execution is ended (and its workspace unmounted) instead of
    /// retried.
    stale: Option<String>,
    /// The engine's version, once read from this execution's transcript
    /// and recorded.
    version: Option<String>,
}

/// What the Bridge knows about one session.
#[derive(Debug, Clone)]
struct SessionView {
    agent_id: AgentId,
    /// The native session the latest execution ran, which a resume
    /// continues.
    native: Option<String>,
    /// The log holds events read from that session's transcript: it
    /// existed, and a resume must not start without it.
    transcript_read: bool,
    /// The profile the session's last execution started with: what a start
    /// or resume that names none runs on.
    model: ModelProfile,
    active: Option<Active>,
    /// What the log last says the agent's branch points at: the commit of
    /// its last `artifact.committed`, or the fork its branch was created at.
    /// The agent's branch is shared by its sessions: recovery takes the
    /// latest of these over all of them, by log position.
    recorded_head: Option<(CommitId, GlobalPosition)>,
    /// The session's latest execution, where it started in the log, and
    /// whether it has ended.
    latest: Option<(ExecutionId, GlobalPosition, bool)>,
    /// What the log says each command's handling already did
    /// ([`CommandStep`]): a retried command and a restarted Bridge go on from
    /// there instead of doing an effect twice.
    steps: HashMap<CommandId, Steps>,
    /// The latest prompt the engine's transcript recorded: its execution and
    /// where it is in the log.
    last_message: Option<(ExecutionId, GlobalPosition)>,
}

/// The recorded steps of one command ([`CommandStep`]).
#[derive(Debug, Clone, Default)]
struct Steps {
    /// The start mounted this workspace.
    mounted: Option<WorkspaceId>,
    /// That mount went again before an execution used it.
    released: bool,
    /// The command's execution started.
    started: bool,
    /// Where the Bridge recorded that it was about to type.
    typing_at: Option<GlobalPosition>,
    /// The typing succeeded.
    typed: bool,
}

/// What a retried command's typing comes to ([`SessionView::typing`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Typing {
    /// Nothing was typed yet.
    Due,
    /// It was typed: the record says so, or the engine's transcript has a
    /// prompt after the record that typing began.
    Done,
    /// Typing began and nothing says whether it reached the engine; it is
    /// not typed again.
    Unknown,
}

impl SessionView {
    /// Fold the command steps and the latest prompt.
    fn fold_steps(&mut self, record: &EventRecord) {
        let event = &record.event;
        match &event.kind {
            EventKind::CommandStepped {
                command_id,
                step,
                workspace,
            } if event.source == Source::Bridge => {
                let steps = self.steps.entry(*command_id).or_default();
                match step {
                    CommandStep::Mounted => steps.mounted.clone_from(workspace),
                    CommandStep::MountReleased => steps.released = true,
                    CommandStep::Typing => steps.typing_at = Some(record.position),
                    CommandStep::Typed => steps.typed = true,
                }
            }
            EventKind::ExecutionStarted { command_id, .. } => {
                self.steps.entry(*command_id).or_default().started = true;
            }
            EventKind::MessageReceived => {
                if let Some(execution) = event.execution_id.clone() {
                    self.last_message = Some((execution, record.position));
                }
            }
            _ => {}
        }
    }

    /// Whether the typing of `command` into `execution` is due, done or
    /// unknown, from the log.
    fn typing(&self, command: CommandId, execution: &ExecutionId) -> Typing {
        let Some(steps) = self.steps.get(&command) else {
            return Typing::Due;
        };
        match steps.typing_at {
            _ if steps.typed => Typing::Done,
            None => Typing::Due,
            Some(at) => {
                let arrived = self
                    .last_message
                    .as_ref()
                    .is_some_and(|(seen, position)| seen == execution && *position > at);
                if arrived {
                    Typing::Done
                } else {
                    Typing::Unknown
                }
            }
        }
    }

    /// Fold what recovery needs to compare the agent's branch with the log:
    /// the commit last recorded for it and the latest execution.
    fn fold_branch(&mut self, record: &EventRecord) {
        let event = &record.event;
        match &event.kind {
            EventKind::ExecutionStarted { fork, .. } => {
                if let Some(execution) = event.execution_id.clone() {
                    self.latest = Some((execution, record.position, false));
                }
                if let Some(fork) = fork {
                    self.recorded_head = Some((fork.commit.clone(), record.position));
                }
            }
            EventKind::ExecutionEnded { .. } if event.source == Source::Bridge => {
                if let Some((execution, _, ended)) = self.latest.as_mut()
                    && event.execution_id.as_ref() == Some(execution)
                {
                    *ended = true;
                }
            }
            EventKind::ArtifactCommitted { artifact, .. } => {
                self.recorded_head = Some((artifact.commit.clone(), record.position));
            }
            _ => {}
        }
    }
}

/// Result of one tick.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TickReport {
    /// Commands handled.
    pub commands: usize,
    /// Non-fatal problems, for example a failed commit.
    pub warnings: Vec<String>,
}

/// Agent Bridge.
pub struct Bridge {
    log: Arc<dyn EventLog>,
    queue: Arc<dyn CommandQueue>,
    workspaces: Arc<dyn WorkspaceStore>,
    terminals: Arc<dyn TerminalHost>,
    config: BridgeConfig,
    cursor: GlobalPosition,
    sessions: HashMap<SessionId, SessionView>,
    clock: Arc<dyn Clock>,
    /// Per session, the `artifact.committed` of a commit the branch already
    /// took whose append failed: appended again first at the next tick
    /// ([`Bridge::record_commit`]).
    unrecorded_commits: std::sync::Mutex<HashMap<SessionId, Vec<NewEvent>>>,
    /// Per session, the store's notices this Bridge already logged as a
    /// warning. Only the log: whether the agent was told is the store's
    /// record ([`WorkspaceStore::notices_told`]).
    warned: std::sync::Mutex<HashMap<SessionId, HashSet<String>>>,
    /// Where the engines' transcripts are copied ([`crate::transcripts`]).
    transcripts: Option<Arc<dyn TranscriptArchive>>,
}

impl Bridge {
    /// Bridge over one store and one worker name.
    pub fn new(
        log: Arc<dyn EventLog>,
        queue: Arc<dyn CommandQueue>,
        workspaces: Arc<dyn WorkspaceStore>,
        terminals: Arc<dyn TerminalHost>,
        config: BridgeConfig,
    ) -> Self {
        Self {
            log,
            queue,
            workspaces,
            terminals,
            config,
            cursor: GlobalPosition(0),
            sessions: HashMap::new(),
            clock: Arc::new(SystemClock),
            unrecorded_commits: std::sync::Mutex::default(),
            warned: std::sync::Mutex::default(),
            transcripts: None,
        }
    }

    /// Copy every engine's transcript into `archive` at turn ends and
    /// execution ends, and restore a missing one from it before a resume.
    #[must_use]
    pub fn with_transcript_archive(mut self, archive: Arc<dyn TranscriptArchive>) -> Self {
        self.transcripts = Some(archive);
        self
    }

    /// Use `clock` for screen refreshes instead of the system clock.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Sessions the Bridge tracks.
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Rebuild state from the whole log.
    pub fn recover(&mut self) -> Result<(), StoreError> {
        self.cursor = GlobalPosition(0);
        self.sessions.clear();
        self.catch_up()?;
        self.rederive_unrecorded();
        self.release_orphan_mounts()?;
        Ok(())
    }

    /// A start that mounted its workspace and died before its execution was
    /// recorded (or before the mount's release was) left a mount no
    /// execution owns. After a restart, each recorded mount with no
    /// execution started and no release goes, unless an execution runs in
    /// that workspace, and its release is recorded. An unmount that fails
    /// is left for the next restart.
    fn release_orphan_mounts(&mut self) -> Result<(), StoreError> {
        let used: HashSet<WorkspaceId> = self
            .sessions
            .values()
            .filter_map(|view| view.active.as_ref())
            .map(|active| active.workspace.clone())
            .collect();
        let mut orphans = Vec::new();
        for (session, view) in &self.sessions {
            for (command, steps) in &view.steps {
                if let Some(workspace) = steps.mounted.as_ref()
                    && !steps.released
                    && !steps.started
                    && !used.contains(workspace)
                {
                    orphans.push((
                        view.agent_id.clone(),
                        session.clone(),
                        *command,
                        workspace.clone(),
                    ));
                }
            }
        }
        for (agent_id, session, command, workspace) in orphans {
            if self.workspaces.unmount(&workspace).is_err() {
                continue;
            }
            self.append(
                &agent_id,
                &session,
                None,
                Source::Bridge,
                Some(format!("{command}:mount_released")),
                EventKind::CommandStepped {
                    command_id: command,
                    step: CommandStep::MountReleased,
                    workspace: Some(workspace),
                },
            )?;
        }
        self.catch_up()
    }

    /// After a restart, the `artifact.committed` records that were waiting
    /// in memory are gone. Re-derive them: an agent whose branch head is not
    /// the commit the log last recorded for it (its last checkpoint or
    /// merge, or the fork it was created at) took a commit whose record
    /// never reached the log. It waits, as a failed append would, under the
    /// agent's latest session and execution, and the next tick appends it
    /// before anything else of that session.
    ///
    /// The record says what the branch holds, not how it got there: a merge
    /// of main re-derived this way is recorded as a checkpoint, without the
    /// files the merge changed.
    fn rederive_unrecorded(&self) {
        let mut latest: HashMap<&AgentId, (&SessionId, &SessionView, GlobalPosition)> =
            HashMap::new();
        let mut recorded: HashMap<&AgentId, (&CommitId, GlobalPosition)> = HashMap::new();
        for (session, view) in &self.sessions {
            if let Some((commit, at)) = view.recorded_head.as_ref()
                && recorded
                    .get(&view.agent_id)
                    .is_none_or(|(_, known)| at > known)
            {
                recorded.insert(&view.agent_id, (commit, *at));
            }
            let Some((_, started, _)) = view.latest.as_ref() else {
                continue;
            };
            let newer = latest
                .get(&view.agent_id)
                .is_none_or(|(_, _, known)| *started > *known);
            if newer {
                latest.insert(&view.agent_id, (session, view, *started));
            }
        }
        for (agent_id, (session, view, _)) in latest {
            let (Some((recorded, _)), Some((execution, _, ended))) =
                (recorded.get(agent_id), view.latest.as_ref())
            else {
                continue;
            };
            let branch = BranchName::for_agent(agent_id);
            let Ok(Some(head)) = self.workspaces.branch_head(&branch) else {
                continue;
            };
            if &head == *recorded {
                continue;
            }
            let reason = if *ended {
                CheckpointReason::ExecutionEnded
            } else {
                CheckpointReason::TurnStopped
            };
            self.unrecorded_commits_lock()
                .entry(session.clone())
                .or_default()
                .push(NewEvent {
                    agent_id: agent_id.clone(),
                    session_id: session.clone(),
                    execution_id: Some(execution.clone()),
                    source: Source::Bridge,
                    source_event_id: Some(format!("commit:{head}")),
                    kind: EventKind::ArtifactCommitted {
                        reason,
                        artifact: ArtifactRef {
                            project: self.workspaces.project().to_string(),
                            branch,
                            commit: head,
                        },
                        files: Vec::new(),
                    },
                });
        }
    }

    /// Append the waiting commit records of every session, each on its own:
    /// one that the log keeps refusing is warned and stays waiting, and the
    /// other sessions go on.
    fn flush_unrecorded(&self, report: &mut TickReport) {
        let waiting: Vec<SessionId> = self.unrecorded_commits_lock().keys().cloned().collect();
        for session in waiting {
            if let Err(error) = self.record_unrecorded_commit(&session) {
                report.warnings.push(format!(
                    "{session}: a commit record waits, its append failed: {error}"
                ));
            }
        }
    }

    /// Handle queued commands and advance running executions.
    pub fn tick(&mut self) -> Result<TickReport, StoreError> {
        self.catch_up()?;
        let mut report = TickReport::default();
        while report.commands < self.config.max_commands_per_tick {
            let Some(claimed) = self
                .queue
                .claim(&self.config.worker, self.config.lease_ttl)?
            else {
                break;
            };
            let outcome = self.handle(&claimed, &mut report)?;
            self.queue.finish(&claimed, &outcome)?;
            report.commands += 1;
            self.catch_up()?;
        }
        self.act(&mut report)?;
        self.catch_up()?;
        Ok(report)
    }

    fn catch_up(&mut self) -> Result<(), StoreError> {
        const BATCH: usize = 200;
        loop {
            let batch = self.log.read_global(self.cursor, BATCH)?;
            if batch.is_empty() {
                return Ok(());
            }
            for record in &batch {
                self.fold(record);
            }
            if let Some(last) = batch.last() {
                self.cursor = last.position;
            }
            if batch.len() < BATCH {
                return Ok(());
            }
        }
    }

    fn fold(&mut self, record: &EventRecord) {
        let event = &record.event;
        // The control stream is not an agent's session.
        if event.kind.is_control() {
            return;
        }
        let view = self
            .sessions
            .entry(event.session_id.clone())
            .or_insert_with(|| SessionView {
                agent_id: event.agent_id.clone(),
                native: None,
                transcript_read: false,
                model: ModelProfile::Default,
                active: None,
                recorded_head: None,
                latest: None,
                steps: HashMap::new(),
                last_message: None,
            });
        view.fold_branch(record);
        view.fold_steps(record);
        view.transcript_read |= transcripts::read_from_transcript(event);
        let current = view
            .active
            .as_ref()
            .is_some_and(|active| event.execution_id.as_ref() == Some(&active.execution_id));
        match &event.kind {
            EventKind::ExecutionStarted {
                workspace,
                model,
                transcript_from,
                ..
            } => {
                view.model = *model;
                if let Some(execution_id) = event.execution_id.clone() {
                    view.active = Some(Active {
                        execution_id,
                        workspace: workspace.clone(),
                        checkpoint: None,
                        reported_end: None,
                        unmounting: false,
                        // The execution's lines are the ones after where the
                        // transcript ended when it started, after a restart too.
                        follow: Follow {
                            offset: transcript_from.unwrap_or(0),
                            ..Follow::default()
                        },
                    });
                }
            }
            // After a restart, the last recorded screen. The Bridge's own
            // readings keep its own clock, which the refresh is timed by.
            EventKind::ScreenObserved { state, .. } if current => {
                if let Some(active) = view.active.as_mut()
                    && active.follow.screen.map(|(known, _)| known) != Some(*state)
                {
                    active.follow.screen = Some((*state, record.recorded_at.0));
                }
            }
            EventKind::ExecutionEnded { .. } if current && event.source == Source::Bridge => {
                view.active = None;
            }
            EventKind::ExecutionEnded { cause, exit_code } if current => {
                if let Some(active) = view.active.as_mut() {
                    active.reported_end = Some((*cause, *exit_code));
                }
            }
            // A recovered end goes straight to the unmount.
            EventKind::ExecutionUnmounting if current => {
                if let Some(active) = view.active.as_mut() {
                    active.unmounting = true;
                }
            }
            // A stalled end stays due after a restart, slow from the start.
            EventKind::ExecutionEndStalled {
                cause, exit_code, ..
            } if current => {
                if let Some(active) = view.active.as_mut() {
                    active.reported_end = Some((*cause, *exit_code));
                    active.follow.end_failures = active.follow.end_failures.max(END_ATTEMPTS);
                }
            }
            EventKind::NativeSessionStarted {
                native_session_id, ..
            } => {
                if view.native.as_ref() != Some(native_session_id) {
                    view.transcript_read = false;
                }
                view.native = Some(native_session_id.clone());
            }
            // A merge of main is no checkpoint of the agent's work: one
            // still due stays due. Nor is a commit of an earlier execution.
            EventKind::ArtifactCommitted { reason, .. }
                if current && *reason != CheckpointReason::BaseMerged =>
            {
                if let Some(active) = view.active.as_mut() {
                    active.checkpoint = None;
                }
            }
            // Checkpoints are the current execution's own: an event of an
            // earlier one that reaches the log late (or one with no
            // execution) asks for none.
            other if current => {
                if let Some(active) = view.active.as_mut()
                    && let Some(reason) = other.checkpoint_reason()
                {
                    active.checkpoint = Some(reason);
                }
            }
            _ => {}
        }
    }

    fn act(&mut self, report: &mut TickReport) -> Result<(), StoreError> {
        // Sessions with no running execution have no `advance` to do it.
        self.flush_unrecorded(report);
        let running: Vec<(SessionId, AgentId, Option<String>, Active)> = self
            .sessions
            .iter()
            .filter_map(|(session, view)| {
                let active = view.active.clone()?;
                Some((
                    session.clone(),
                    view.agent_id.clone(),
                    view.native.clone(),
                    active,
                ))
            })
            .collect();
        for (session, agent_id, native, mut active) in running {
            let ended =
                self.advance(&agent_id, &session, native.as_deref(), &mut active, report)?;
            if let Some(view) = self.sessions.get_mut(&session) {
                if ended {
                    view.active = None;
                } else if let Some(held) = view.active.as_mut() {
                    // Cleared by `advance` only once committed: a failed
                    // checkpoint stays due and is tried again next tick.
                    held.checkpoint = active.checkpoint;
                    held.reported_end = active.reported_end;
                    held.follow = active.follow;
                }
            }
        }
        Ok(())
    }

    /// Record what the engine did since the last tick, commit a pending
    /// checkpoint, or end an execution whose process is gone. Returns
    /// whether the execution ended.
    fn advance(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        native: Option<&str>,
        active: &mut Active,
        report: &mut TickReport,
    ) -> Result<bool, StoreError> {
        // Nothing of this session goes into the log before its waiting
        // commit records; one that keeps failing holds up only this session.
        if let Err(error) = self.record_unrecorded_commit(session) {
            report.warnings.push(format!(
                "{session}: a commit record waits, its append failed: {error}"
            ));
            return Ok(false);
        }
        self.follow_output(agent_id, session, native, active, report)?;
        let end = match active
            .reported_end
            .or_else(|| self.exit_status(session, active, report))
        {
            Some(end) => Some(end),
            None => match self
                .terminals
                .is_alive(&active.terminal(), &active.workspace)
            {
                Ok(true) => {
                    self.follow_screen(agent_id, session, active)?;
                    None
                }
                // The engine may have written its exit status and closed the
                // pane between the read above and this check: read it again
                // before calling the execution lost.
                Ok(false) => Some(
                    self.exit_status(session, active, report)
                        .unwrap_or((EndCause::Lost, None)),
                ),
                Err(error) => {
                    report
                        .warnings
                        .push(format!("is_alive {}: {error}", active.terminal()));
                    None
                }
            },
        };
        if let Some((cause, exit_code)) = end {
            // What the engine wrote last comes before its end.
            self.follow_output(agent_id, session, native, active, report)?;
            return self.end(agent_id, session, active, cause, exit_code, false, report);
        }
        if let Some(reason) = active.checkpoint {
            // A commit that failed was reported; the checkpoint stays due.
            if self.checkpoint(agent_id, session, active, reason, report)? {
                active.checkpoint = None;
                if reason == CheckpointReason::TurnStopped {
                    self.archive_transcript(agent_id, session, active, report);
                }
            }
        }
        if active.follow.stale.is_some() {
            return self.end(
                agent_id,
                session,
                active,
                EndCause::Stopped,
                None,
                false,
                report,
            );
        }
        Ok(false)
    }

    /// The exit status the runner wrote when the engine exited, if it has.
    /// An unreadable file (not a regular file, a host error) is warned once
    /// and the engine counts as running: the pane's liveness still ends it.
    fn exit_status(
        &self,
        session: &SessionId,
        active: &mut Active,
        report: &mut TickReport,
    ) -> Option<(EndCause, Option<i32>)> {
        let bytes = match self
            .terminals
            .read_from(&active.workspace, &exit_file(active), 0)
        {
            Ok(bytes) => bytes?,
            Err(error) => {
                if !active.follow.exit_warned {
                    active.follow.exit_warned = true;
                    report
                        .warnings
                        .push(format!("{session}: exit file: {error}"));
                }
                return None;
            }
        };
        let text = String::from_utf8_lossy(&bytes);
        Some((EndCause::Exited, text.trim().parse().ok()))
    }

    /// Append the events in what the engine wrote to its transcript since
    /// the last read. Only whole lines are read; a line being written waits
    /// for the next tick.
    fn follow_output(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        native: Option<&str>,
        active: &mut Active,
        report: &mut TickReport,
    ) -> Result<(), StoreError> {
        let Some(native) = native else {
            return Ok(());
        };
        let path = transcript_path(&self.config.claude_projects, &active.workspace, native);
        let bytes = match self
            .terminals
            .read_from(&active.workspace, &path, active.follow.offset)
        {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                let now = self.clock.now().0;
                let since = *active.follow.first_seen.get_or_insert(now);
                let grace = now.saturating_sub(since) > 60_000;
                if grace && !active.follow.warned {
                    active.follow.warned = true;
                    report
                        .warnings
                        .push(format!("{session}: no engine output at {path}"));
                }
                return Ok(());
            }
            Err(error) => {
                if !active.follow.warned {
                    active.follow.warned = true;
                    report
                        .warnings
                        .push(format!("{session}: read {path}: {error}"));
                }
                return Ok(());
            }
        };
        let Some(end) = bytes.iter().rposition(|byte| *byte == b'\n') else {
            // A full read without a newline is one line longer than the
            // limit: skip what was read rather than stall on it forever (the
            // rest of it parses as nothing).
            if bytes.len() >= READ_LIMIT {
                active.follow.offset += u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                report.warnings.push(format!(
                    "{session}: skipped a line over {READ_LIMIT} bytes in {path}"
                ));
            }
            return Ok(());
        };
        for line in bytes[..end].split(|byte| *byte == b'\n') {
            let line = String::from_utf8_lossy(line);
            if active.follow.version.is_none()
                && let Some(version) = claude_version(&line)
            {
                self.record_version(agent_id, session, native, active, &path, version)?;
            }
            for event in claude_line(&line) {
                self.log.append(NewEvent {
                    agent_id: agent_id.clone(),
                    session_id: session.clone(),
                    execution_id: Some(active.execution_id.clone()),
                    source: Source::Claude,
                    source_event_id: event.source_event_id,
                    kind: event.kind,
                })?;
            }
        }
        active.follow.offset += u64::try_from(end + 1).unwrap_or(u64::MAX);
        Ok(())
    }

    /// The engine's version, read from the execution's transcript: the
    /// native session is recorded again with it, once per execution.
    fn record_version(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        native: &str,
        active: &mut Active,
        path: &str,
        version: String,
    ) -> Result<(), StoreError> {
        active.follow.version = Some(version.clone());
        self.append(
            agent_id,
            session,
            Some(&active.execution_id),
            Source::Claude,
            Some(format!("{}:engine_version", active.execution_id)),
            EventKind::NativeSessionStarted {
                engine: ENGINE.to_string(),
                native_session_id: native.to_string(),
                transcript_path: Some(path.to_string()),
                engine_version: Some(version),
            },
        )
    }

    /// Read the pane and record its state when it changes, or again after
    /// [`BridgeConfig::screen_refresh`] while a turn keeps it busy.
    fn follow_screen(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        active: &mut Active,
    ) -> Result<(), StoreError> {
        let Ok(pane) =
            self.terminals
                .capture_tail(&active.terminal(), &active.workspace, TAIL_LINES)
        else {
            return Ok(());
        };
        let reading = read_claude_screen(&pane);
        let confirmed = active.follow.seen == Some(reading.state);
        active.follow.seen = Some(reading.state);
        if !confirmed || reading.state == ScreenState::Unknown {
            return Ok(());
        }
        let now = self.clock.now().0;
        let due = match active.follow.screen {
            None => true,
            Some((state, _)) if state != reading.state => true,
            Some((state, at)) => {
                state == ScreenState::Busy
                    && now.saturating_sub(at)
                        >= i64::try_from(self.config.screen_refresh.as_millis()).unwrap_or(i64::MAX)
            }
        };
        if due {
            active.follow.screen = Some((reading.state, now));
            self.append(
                agent_id,
                session,
                Some(&active.execution_id),
                Source::Bridge,
                None,
                EventKind::ScreenObserved {
                    state: reading.state,
                    evidence: reading.evidence,
                },
            )?;
        }
        Ok(())
    }

    /// Final checkpoint, kill, unmount, then `execution.ended`. Returns
    /// false when the final checkpoint failed or the terminal could not be
    /// killed; the execution then stays active, its workspace mounted, and
    /// the next tick tries the same end again.
    ///
    /// A failed final checkpoint never leads to an unmount: its workspace
    /// holds the only copy of what the execution did not commit. After
    /// [`END_ATTEMPTS`] failures the end is stalled: the terminal is killed,
    /// `execution.end_stalled` is recorded and warned once, and the
    /// checkpoint is retried every [`END_RETRY_MS`] until it commits. Only
    /// `discard` (a person's or the supervisor's decision) unmounts a
    /// workspace whose final checkpoint failed.
    #[allow(clippy::too_many_arguments)]
    fn end(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        active: &mut Active,
        cause: EndCause,
        exit_code: Option<i32>,
        discard: bool,
        report: &mut TickReport,
    ) -> Result<bool, StoreError> {
        active.reported_end = Some((cause, exit_code));
        // The checkpoint and the terminal are behind an end that recorded
        // its unmount: what is left is the unmount and the record.
        if active.unmounting {
            // A restart may find the engine's terminal still there.
            if let Err(error) = self
                .terminals
                .terminate(&active.terminal(), &active.workspace)
            {
                report
                    .warnings
                    .push(format!("terminate {}: {error}", active.terminal()));
                return Ok(false);
            }
            return self.finish_end(agent_id, session, active, cause, exit_code, report);
        }
        // A checkpoint whose record waits is recorded before the end.
        if let Err(error) = self.record_unrecorded_commit(session) {
            report.warnings.push(format!(
                "{session}: a commit record waits, its append failed: {error}; the end is \
                 tried again"
            ));
            return Ok(false);
        }
        let now = self.clock.now().0;
        let stalled = active.follow.end_failures >= END_ATTEMPTS;
        if stalled
            && !discard
            && active
                .follow
                .end_tried_at
                .is_some_and(|at| now.saturating_sub(at) < END_RETRY_MS)
        {
            return Ok(false);
        }
        active.follow.end_tried_at = Some(now);
        let committed = self.checkpoint(
            agent_id,
            session,
            active,
            CheckpointReason::ExecutionEnded,
            report,
        )?;
        if !committed && discard {
            report.warnings.push(format!(
                "{session}: final checkpoint failed; discarded: unmounting {} with what it \
                 did not commit",
                active.workspace
            ));
        } else if !committed {
            let error = report.warnings.last().cloned().unwrap_or_default();
            active.follow.end_failures = active.follow.end_failures.saturating_add(1);
            let failures = active.follow.end_failures;
            if failures < END_ATTEMPTS {
                report.warnings.push(format!(
                    "{session}: final checkpoint failed ({failures} of {END_ATTEMPTS}); the \
                     workspace stays mounted and the end is tried again"
                ));
            } else if failures == END_ATTEMPTS {
                self.stall(agent_id, session, active, error, report)?;
            }
            return Ok(false);
        }
        if let Err(error) = self
            .terminals
            .terminate(&active.terminal(), &active.workspace)
        {
            report
                .warnings
                .push(format!("terminate {}: {error}", active.terminal()));
            return Ok(false);
        }
        // The transcript is whole now, and a container's copy goes next.
        self.archive_transcript(agent_id, session, active, report);
        self.append(
            agent_id,
            session,
            Some(&active.execution_id),
            Source::Bridge,
            Some(format!("{}:unmounting", active.execution_id)),
            EventKind::ExecutionUnmounting,
        )?;
        active.unmounting = true;
        self.finish_end(agent_id, session, active, cause, exit_code, report)
    }

    /// The last of an end, from its recorded `execution.unmounting` (the
    /// terminal is gone): the workspace unmounted (an absent one is no
    /// failure) and `execution.ended` recorded.
    fn finish_end(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        active: &Active,
        cause: EndCause,
        exit_code: Option<i32>,
        report: &mut TickReport,
    ) -> Result<bool, StoreError> {
        if let Err(error) = self.workspaces.unmount(&active.workspace) {
            report
                .warnings
                .push(format!("unmount {}: {error}", active.workspace));
        }
        self.append(
            agent_id,
            session,
            Some(&active.execution_id),
            Source::Bridge,
            Some(format!("{}:ended", active.execution_id)),
            EventKind::ExecutionEnded { cause, exit_code },
        )?;
        Ok(true)
    }

    /// The end's final checkpoint failed [`END_ATTEMPTS`] times: kill the
    /// terminal (the engine's work is all in the workspace, which stays),
    /// record `execution.end_stalled` and warn, once.
    fn stall(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        active: &Active,
        error: String,
        report: &mut TickReport,
    ) -> Result<(), StoreError> {
        if let Err(error) = self
            .terminals
            .terminate(&active.terminal(), &active.workspace)
        {
            report
                .warnings
                .push(format!("terminate {}: {error}", active.terminal()));
        }
        report.warnings.push(format!(
            "{session}: FINAL CHECKPOINT FAILED {END_ATTEMPTS} TIMES; the terminal is down, \
             {} STAYS MOUNTED with everything it holds and the checkpoint is retried every \
             {} s. Fix the cause ({error}); a discard command gives the uncommitted work up",
            active.workspace,
            END_RETRY_MS / 1000
        ));
        let (cause, exit_code) = active.reported_end.unwrap_or((EndCause::Lost, None));
        self.append(
            agent_id,
            session,
            Some(&active.execution_id),
            Source::Bridge,
            Some(format!("{}:end_stalled", active.execution_id)),
            EventKind::ExecutionEndStalled {
                cause,
                exit_code,
                failures: END_ATTEMPTS,
                error,
            },
        )
    }

    /// Commit the workspace and record a new commit. Returns whether the
    /// workspace's state is now on its branch, or kept off it by a store
    /// that answered [`WORKSPACE_STALE`] (`follow.stale` then holds why and the
    /// execution ends); false: the commit failed and was reported.
    fn checkpoint(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        active: &mut Active,
        reason: CheckpointReason,
        report: &mut TickReport,
    ) -> Result<bool, StoreError> {
        let message = format!("spawn checkpoint {}", active.execution_id);
        match self.workspaces.commit(&active.workspace, &message) {
            Ok(Commit::Unchanged) => {}
            Ok(Commit::Created(commit)) => {
                let artifact = ArtifactRef {
                    project: self.workspaces.project().to_string(),
                    branch: BranchName::for_agent(agent_id),
                    commit,
                };
                self.record_commit(
                    agent_id,
                    session,
                    Some(&active.execution_id),
                    Source::Bridge,
                    Some(format!("commit:{}", artifact.commit)),
                    EventKind::ArtifactCommitted {
                        reason,
                        artifact,
                        files: Vec::new(),
                    },
                )?;
            }
            // The store kept the workspace's state off its branch: an
            // unmount loses nothing, and the execution ends.
            Err(WorkspaceError::Refused { code, message }) if code == WORKSPACE_STALE => {
                if active.follow.stale.is_none() {
                    report.warnings.push(format!(
                        "{session}: commit refused {code}: {message} The execution ends."
                    ));
                }
                active.follow.stale = Some(message);
            }
            Err(WorkspaceError::Refused { code, message }) => {
                report
                    .warnings
                    .push(format!("commit refused {code}: {message}"));
                return Ok(false);
            }
            Err(WorkspaceError::Unavailable(text)) => {
                report.warnings.push(format!("commit unavailable: {text}"));
                return Ok(false);
            }
        }
        // Logged once, and carried by the next prompt the Bridge types.
        let notices = self.workspaces.notices(&active.workspace);
        if !notices.is_empty()
            && let Ok(mut warned) = self.warned.lock()
        {
            let seen = warned.entry(session.clone()).or_default();
            for notice in notices {
                if !seen.contains(&notice) {
                    report.warnings.push(format!("{session}: {notice}"));
                    seen.insert(notice);
                }
            }
        }
        Ok(true)
    }

    fn append(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        execution: Option<&ExecutionId>,
        source: Source,
        key: Option<String>,
        kind: EventKind,
    ) -> Result<(), StoreError> {
        self.log.append(NewEvent {
            agent_id: agent_id.clone(),
            session_id: session.clone(),
            execution_id: execution.cloned(),
            source,
            source_event_id: key,
            kind,
        })?;
        Ok(())
    }

    /// Append the `artifact.committed` of a commit the branch already took.
    /// The branch moved first, so an append that fails must not lose the
    /// record: it waits in `unrecorded_commits` and the session's next tick
    /// appends it before anything else ([`Self::record_unrecorded_commit`]).
    /// Its key (`commit:<id>`) makes a second append of one that did land
    /// a no-op.
    fn record_commit(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        execution: Option<&ExecutionId>,
        source: Source,
        key: Option<String>,
        kind: EventKind,
    ) -> Result<(), StoreError> {
        let event = NewEvent {
            agent_id: agent_id.clone(),
            session_id: session.clone(),
            execution_id: execution.cloned(),
            source,
            source_event_id: key,
            kind,
        };
        self.unrecorded_commits_lock()
            .entry(session.clone())
            .or_default()
            .push(event.clone());
        self.record_unrecorded_commit(session)
    }

    /// Append, in order, the commit records of `session` whose append
    /// failed; each leaves the list once the log took it.
    fn record_unrecorded_commit(&self, session: &SessionId) -> Result<(), StoreError> {
        loop {
            let Some(event) = self
                .unrecorded_commits_lock()
                .get(session)
                .and_then(|waiting| waiting.first().cloned())
            else {
                return Ok(());
            };
            self.log.append(event)?;
            let mut unrecorded = self.unrecorded_commits_lock();
            if let Some(waiting) = unrecorded.get_mut(session) {
                waiting.remove(0);
                if waiting.is_empty() {
                    unrecorded.remove(session);
                }
            }
        }
    }

    fn unrecorded_commits_lock(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<SessionId, Vec<NewEvent>>> {
        self.unrecorded_commits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn handle(
        &mut self,
        claimed: &Claimed,
        report: &mut TickReport,
    ) -> Result<Outcome, StoreError> {
        let Ok(action) = claimed.action() else {
            return Ok(Outcome::Failed("stored action is not valid".to_string()));
        };
        // What a command appends comes after the session's waiting commit
        // records. While the log refuses them the command is not handled
        // (the error ends the tick; the lease brings the command back).
        self.record_unrecorded_commit(&claimed.session_id)?;
        match action {
            CommandAction::Start {
                engine: _,
                prompt,
                parent_session_id,
                from,
                model,
            } => self.start(
                claimed,
                &Begin {
                    prompt: &prompt,
                    parent: parent_session_id.as_ref(),
                    native: None,
                    from: from.as_deref(),
                    model,
                },
                report,
            ),
            CommandAction::Input { text, .. } => self.input(claimed, &text),
            CommandAction::Stop => self.stop(claimed, false, report),
            CommandAction::Discard => self.stop(claimed, true, report),
            CommandAction::Resume { prompt, model } => {
                self.resume(claimed, prompt.as_deref(), model, report)
            }
        }
    }

    fn running(&self, session: &SessionId) -> Option<Active> {
        self.sessions.get(session)?.active.clone()
    }

    fn start(
        &mut self,
        claimed: &Claimed,
        begin: &Begin<'_>,
        report: &mut TickReport,
    ) -> Result<Outcome, StoreError> {
        let Begin {
            prompt,
            parent,
            native,
            from,
            model,
        } = *begin;
        // Another Bridge may have started the session since this tick read
        // the log; the log refuses a second start either way.
        self.catch_up()?;
        if let Some(outcome) = self.started_before(claimed, prompt, native.is_none()) {
            return outcome;
        }
        let model = model.unwrap_or_else(|| {
            self.sessions
                .get(&claimed.session_id)
                .map_or(ModelProfile::Default, |view| view.model)
        });
        let settings = match self.engine_settings(claimed, model, report) {
            Ok(settings) => settings,
            Err(why) => return Ok(Outcome::Failed(why)),
        };
        let branch = BranchName::for_agent(&claimed.agent_id);
        let fork = match self.ensure_branch(claimed, &branch, from, report) {
            Ok(fork) => fork,
            Err(why) => return Ok(Outcome::Failed(why)),
        };
        self.merge_main_at_start(claimed, &branch, report)?;
        let workspace = match self.workspaces.mount(&branch) {
            Ok(workspace) => workspace,
            Err(error) => return Ok(Outcome::Failed(format!("mount {branch}: {error}"))),
        };
        // Recorded, so a restart that finds no execution using it unmounts it
        // ([`Self::release_orphan_mounts`]).
        if let Err(error) = self.step(claimed, None, CommandStep::Mounted, Some(&workspace)) {
            self.give_up_mount(claimed, &workspace, error.to_string());
            return Err(error);
        }
        // A resumed session's transcript holds the earlier executions' lines:
        // this execution's are the ones after its length now.
        let transcript_from = match native
            .map(|native| self.resumed_transcript(claimed, &workspace, native, report))
            .transpose()
        {
            Ok(length) => length.flatten(),
            Err(why) => return Ok(self.give_up_mount(claimed, &workspace, why)),
        };
        if let Some(parent_session) = parent {
            let linked = self.append(
                &claimed.agent_id,
                &claimed.session_id,
                None,
                Source::Bridge,
                Some(format!("{}:linked:{parent_session}", claimed.session_id)),
                EventKind::SessionLinked {
                    parent_session_id: parent_session.clone(),
                },
            );
            if let Err(error) = linked {
                self.give_up_mount(claimed, &workspace, error.to_string());
                return Err(error);
            }
        }
        let active = Active {
            execution_id: ExecutionId::for_command(claimed.command_id),
            workspace,
            checkpoint: None,
            reported_end: None,
            unmounting: false,
            follow: Follow::default(),
        };
        // The event goes first: a crash after it leaves a recorded execution
        // whose missing terminal the next tick ends as lost.
        let recorded = self.append(
            &claimed.agent_id,
            &claimed.session_id,
            Some(&active.execution_id),
            Source::Bridge,
            Some(format!("{}:started", active.execution_id)),
            EventKind::ExecutionStarted {
                engine: ENGINE.to_string(),
                workspace: active.workspace.clone(),
                command_id: claimed.command_id,
                fork,
                model,
                transcript_from,
            },
        );
        match recorded {
            Ok(()) => {}
            Err(StoreError::Rejected(why)) => {
                return self.refused_start(claimed, &active.workspace, why);
            }
            // Whether it reached the log is unknown: the mount goes either
            // way. A recorded execution without its terminal or workspace
            // ends as lost on the next tick.
            Err(error) => {
                self.give_up_mount(claimed, &active.workspace, error.to_string());
                return Err(error);
            }
        }
        self.launch_and_prompt(claimed, &active, native, &settings, prompt)
    }

    /// A retried Start or Resume whose execution the log already records
    /// (the tick that handled it died, or its finish failed): go on from
    /// what the log says it did. An engine whose terminal is gone ends as
    /// lost on the next tick; a prompt is typed only when the log shows it
    /// was not ([`SessionView::typing`]), and an engine whose prompt may or
    /// may not have arrived is ended rather than typed into again or left
    /// waiting at an empty prompt.
    fn go_on_starting(
        &mut self,
        claimed: &Claimed,
        active: &Active,
        prompt: &str,
        fresh: bool,
    ) -> Result<Outcome, StoreError> {
        match self
            .terminals
            .is_alive(&active.terminal(), &active.workspace)
        {
            Ok(true) => {}
            Ok(false) => {
                return Ok(Outcome::Failed(format!(
                    "{}: the engine's terminal is gone; the execution ends as lost",
                    claimed.session_id
                )));
            }
            Err(error) => {
                return Ok(Outcome::Failed(format!(
                    "is_alive {}: {error}",
                    active.terminal()
                )));
            }
        }
        if prompt.is_empty() {
            return Ok(Outcome::Succeeded);
        }
        match self.typing_of(claimed, &active.execution_id) {
            Typing::Done => Ok(Outcome::Succeeded),
            Typing::Unknown => {
                self.abandon(claimed, active, format!("the prompt {MAY_HAVE_TYPED}"), false)
            }
            Typing::Due => self.prompt_engine(claimed, active, prompt, fresh),
        }
    }

    /// What a Start or Resume does when the session already runs an
    /// execution or this command's ran: it goes on with its own execution
    /// ([`Self::go_on_starting`]) and refuses otherwise. `None` when it
    /// starts afresh.
    fn started_before(
        &mut self,
        claimed: &Claimed,
        prompt: &str,
        fresh: bool,
    ) -> Option<Result<Outcome, StoreError>> {
        if let Some(active) = self.running(&claimed.session_id) {
            if active.execution_id == ExecutionId::for_command(claimed.command_id) {
                return Some(self.go_on_starting(claimed, &active, prompt, fresh));
            }
            return Some(Ok(Outcome::Failed(format!(
                "{} already runs an execution",
                claimed.session_id
            ))));
        }
        self.recorded_steps(claimed).started.then(|| {
            Ok(Outcome::Failed(format!(
                "{}: this command's execution already ran and ended",
                claimed.session_id
            )))
        })
    }

    /// The command's recorded steps.
    fn recorded_steps(&self, claimed: &Claimed) -> Steps {
        self.sessions
            .get(&claimed.session_id)
            .and_then(|view| view.steps.get(&claimed.command_id))
            .cloned()
            .unwrap_or_default()
    }

    /// Whether the command's typing into `execution` is due, done or unknown.
    fn typing_of(&self, claimed: &Claimed, execution: &ExecutionId) -> Typing {
        self.sessions
            .get(&claimed.session_id)
            .map_or(Typing::Due, |view| {
                view.typing(claimed.command_id, execution)
            })
    }

    /// Record a step of the command being handled, once (keyed by command
    /// and step).
    fn step(
        &self,
        claimed: &Claimed,
        execution: Option<&ExecutionId>,
        step: CommandStep,
        workspace: Option<&WorkspaceId>,
    ) -> Result<(), StoreError> {
        let name = match step {
            CommandStep::Mounted => "mounted",
            CommandStep::MountReleased => "mount_released",
            CommandStep::Typing => "typing",
            CommandStep::Typed => "typed",
        };
        self.append(
            &claimed.agent_id,
            &claimed.session_id,
            execution,
            Source::Bridge,
            Some(format!("{}:{name}", claimed.command_id)),
            EventKind::CommandStepped {
                command_id: claimed.command_id,
                step,
                workspace: workspace.cloned(),
            },
        )
    }

    /// Text into the engine's input box, at most once: the intent is
    /// recorded before the keys go and the completion after, so a retried
    /// command or a restarted Bridge can tell whether it was typed. What the
    /// workspace store has to tell and no prompt carried yet goes before
    /// it, and is marked told in the store once the typing is recorded.
    fn type_once(
        &self,
        claimed: &Claimed,
        active: &Active,
        text: &str,
    ) -> Result<Result<(), TerminalError>, StoreError> {
        // What the workspace store has to tell the agent goes first. It is
        // the store's record, not this process's memory, so a restart
        // between the checkpoint and this prompt loses none of it.
        let notices = self.workspaces.notices(&active.workspace);
        let text = if notices.is_empty() {
            text.to_string()
        } else {
            format!("{}\n\n{text}", notices.join("\n\n"))
        };
        self.step(
            claimed,
            Some(&active.execution_id),
            CommandStep::Typing,
            None,
        )?;
        let typed = self
            .terminals
            .submit_text(&active.terminal(), &active.workspace, &text);
        if typed.is_ok() {
            self.step(
                claimed,
                Some(&active.execution_id),
                CommandStep::Typed,
                None,
            )?;
            if !notices.is_empty() {
                self.workspaces.notices_told(&active.workspace, &notices);
            }
        }
        Ok(typed)
    }

    /// The engine of a recorded execution started in its terminal, then its
    /// prompt typed (an engine that never got its prompt is abandoned).
    fn launch_and_prompt(
        &mut self,
        claimed: &Claimed,
        active: &Active,
        native: Option<&str>,
        settings: &str,
        prompt: &str,
    ) -> Result<Outcome, StoreError> {
        let fresh = self.name_session(claimed, native, active)?;
        let launch = self.launch(
            claimed,
            active,
            &Run {
                native: fresh.as_deref().or(native),
                resume: native.is_some(),
                settings,
            },
        );
        if let Err(error) = self.terminals.start(&launch) {
            return self.abandon(claimed, active, format!("start terminal: {error}"), true);
        }
        if prompt.is_empty() {
            return Ok(Outcome::Succeeded);
        }
        self.prompt_engine(claimed, active, prompt, native.is_none())
    }

    /// Type a started engine's prompt (`fresh`: a new native session, whose
    /// directory may be new to the engine). An engine that never got it must
    /// not look alive (whoever started it would wait on it forever): it is
    /// abandoned.
    fn prompt_engine(
        &mut self,
        claimed: &Claimed,
        active: &Active,
        prompt: &str,
        fresh: bool,
    ) -> Result<Outcome, StoreError> {
        let outcome = self.type_prompt(claimed, active, prompt, fresh)?;
        if let Outcome::Failed(why) = outcome {
            let discard = self.never_typed(claimed, active)?;
            return self.abandon(claimed, active, why, discard);
        }
        self.catch_up()?;
        Ok(outcome)
    }

    /// Before a resume of `native`: its transcript is where the engine
    /// reads it, put back from the archive when it went missing, or why the
    /// resume cannot go on ([`transcripts::restore`]).
    fn ready_transcript(
        &self,
        claimed: &Claimed,
        workspace: &WorkspaceId,
        native: &str,
        report: &mut TickReport,
    ) -> Result<(), String> {
        let path = transcript_path(&self.config.claude_projects, workspace, native);
        let read_before = self
            .sessions
            .get(&claimed.session_id)
            .is_some_and(|view| view.transcript_read);
        let restored = transcripts::restore(
            self.terminals.as_ref(),
            self.transcripts.as_deref(),
            workspace,
            &path,
            &claimed.agent_id,
            native,
            read_before,
        )?;
        if let Some(bytes) = restored {
            report.warnings.push(format!(
                "{}: {path} was missing; restored {bytes} bytes from the transcript archive",
                claimed.session_id
            ));
        }
        Ok(())
    }

    /// Copy what the engine appended to its transcript into the archive,
    /// if there is one. A failure is warned; the next copy tries again.
    fn archive_transcript(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        active: &Active,
        report: &mut TickReport,
    ) {
        let native = self
            .sessions
            .get(session)
            .and_then(|view| view.native.as_deref());
        let (Some(archive), Some(native)) = (self.transcripts.as_deref(), native) else {
            return;
        };
        let path = transcript_path(&self.config.claude_projects, &active.workspace, native);
        if let Err(error) = transcripts::archive(
            self.terminals.as_ref(),
            archive,
            &active.workspace,
            &path,
            agent_id,
            native,
        ) {
            report
                .warnings
                .push(format!("{session}: archive the transcript: {error}"));
        }
    }

    /// The log refused `execution.started`: the session runs an execution
    /// another Bridge started. The mount goes, unless it is that
    /// execution's own workspace.
    fn refused_start(
        &mut self,
        claimed: &Claimed,
        workspace: &WorkspaceId,
        why: String,
    ) -> Result<Outcome, StoreError> {
        self.catch_up()?;
        let theirs = self
            .running(&claimed.session_id)
            .is_some_and(|active| active.workspace == *workspace);
        if theirs {
            return Ok(Outcome::Failed(why));
        }
        Ok(self.give_up_mount(claimed, workspace, why))
    }

    /// Main reaches an agent's branch only here, when an execution starts or
    /// resumes, and only when it merges cleanly: the merge is recorded as a
    /// `base_merged` commit. A conflict leaves the branch as it is and is
    /// recorded (`merge.conflicted`, `marked: false`, keyed by the branch
    /// head and the files, so a retried start records it once) for the
    /// supervisor and the integrator; the execution starts on the branch as
    /// it is. A store that cannot merge is warned of, and changes nothing.
    fn merge_main_at_start(
        &self,
        claimed: &Claimed,
        branch: &BranchName,
        report: &mut TickReport,
    ) -> Result<(), StoreError> {
        let session = &claimed.session_id;
        match self.workspaces.merge_main_clean(branch) {
            Ok(BaseMerge::Current) => Ok(()),
            Ok(BaseMerge::Merged { commit, files }) => {
                let artifact = ArtifactRef {
                    project: self.workspaces.project().to_string(),
                    branch: branch.clone(),
                    commit,
                };
                self.record_commit(
                    &claimed.agent_id,
                    session,
                    None,
                    Source::Bridge,
                    Some(format!("commit:{}", artifact.commit)),
                    EventKind::ArtifactCommitted {
                        reason: CheckpointReason::BaseMerged,
                        artifact,
                        files,
                    },
                )
            }
            Ok(BaseMerge::Conflicted { files, .. }) => {
                report.warnings.push(format!(
                    "{session}: main conflicts with {branch} in {}; the execution starts on \
                     the branch as it is",
                    files.join(", ")
                ));
                let head = self
                    .workspaces
                    .branch_head(branch)
                    .ok()
                    .flatten()
                    .map_or_else(|| "-".to_string(), |head| head.to_string());
                self.append(
                    &claimed.agent_id,
                    session,
                    None,
                    Source::Bridge,
                    Some(format!("conflict:{head}:{:016x}", files_digest(&files))),
                    EventKind::MergeConflicted {
                        files,
                        marked: false,
                    },
                )
            }
            Err(error) => {
                report.warnings.push(format!(
                    "{session}: merge main into {branch}: {error}; the execution starts on the \
                     branch as it is"
                ));
                Ok(())
            }
        }
    }

    /// Before a resume of `native`: its transcript ready
    /// ([`Self::ready_transcript`]), and its length, which the execution's
    /// own lines come after.
    fn resumed_transcript(
        &self,
        claimed: &Claimed,
        workspace: &WorkspaceId,
        native: &str,
        report: &mut TickReport,
    ) -> Result<Option<u64>, String> {
        self.ready_transcript(claimed, workspace, native, report)?;
        self.transcript_length(workspace, native)
    }

    /// How long the transcript of `native` is, as the engine will see it;
    /// `None` when it does not exist (yet).
    fn transcript_length(
        &self,
        workspace: &WorkspaceId,
        native: &str,
    ) -> Result<Option<u64>, String> {
        let path = transcript_path(&self.config.claude_projects, workspace, native);
        let mut length = 0u64;
        loop {
            match self.terminals.read_from(workspace, &path, length) {
                Ok(None) => return Ok((length > 0).then_some(length)),
                Ok(Some(bytes)) if bytes.is_empty() => return Ok(Some(length)),
                Ok(Some(bytes)) => {
                    length = length.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
                }
                Err(error) => return Err(format!("read {path}: {error}")),
            }
        }
    }

    /// A start that fails after its mount, before anything ran in it: the
    /// mount goes, and the start fails with `why`. The release is recorded
    /// when the log takes it; when it does not, a restart unmounts again
    /// ([`Self::release_orphan_mounts`]).
    fn give_up_mount(&self, claimed: &Claimed, workspace: &WorkspaceId, why: String) -> Outcome {
        if let Err(error) = self.workspaces.unmount(workspace) {
            return Outcome::Failed(format!("{why}; unmount {workspace}: {error}"));
        }
        match self.step(claimed, None, CommandStep::MountReleased, Some(workspace)) {
            Ok(()) => Outcome::Failed(why),
            Err(error) => Outcome::Failed(format!("{why}; record the unmount: {error}")),
        }
    }

    /// Claude Code's inline settings for an engine on `model`, or why it
    /// cannot start. The default profile falls back on Claude Code's own
    /// settings (the gateway, its own small model) when the relay is down;
    /// the cheap profile has no fallback, and fails where its sender sees
    /// it.
    fn engine_settings(
        &self,
        claimed: &Claimed,
        model: ModelProfile,
        report: &mut TickReport,
    ) -> Result<String, String> {
        let relay = self.config.model_relay.as_ref();
        let reachable = relay.filter(|relay| relay.reachable());
        match (model, relay, reachable) {
            (_, _, Some(relay)) => Ok(claude_settings(Some(relay), model)),
            (ModelProfile::Default, Some(relay), None) => {
                report.warnings.push(format!(
                    "{}: model relay {} unreachable; the default profile runs on Claude Code's \
                     own settings",
                    claimed.session_id, relay.url
                ));
                Ok(claude_settings(None, model))
            }
            (ModelProfile::Default, None, None) => Ok(claude_settings(None, model)),
            (ModelProfile::Cheap, Some(relay), None) => Err(format!(
                "the cheap profile needs the model relay, and {} is unreachable",
                relay.url
            )),
            (ModelProfile::Cheap, None, None) => Err(
                "the cheap profile needs the model relay, and none is configured \
                 (SPAWN_MODEL_RELAY)"
                    .to_string(),
            ),
        }
    }

    /// End a start that failed as lost; the failure names `why` first.
    /// `discard` only when the log shows its prompt was never typed
    /// (nothing ran in it, so an unmount loses nothing). Otherwise the
    /// prompt may have reached the engine: a final checkpoint that fails
    /// keeps the workspace, and the end stays due, as a Stop's does.
    fn abandon(
        &mut self,
        claimed: &Claimed,
        active: &Active,
        why: String,
        discard: bool,
    ) -> Result<Outcome, StoreError> {
        let mut report = TickReport::default();
        let mut ending = active.clone();
        let ended = self.end(
            &claimed.agent_id,
            &claimed.session_id,
            &mut ending,
            EndCause::Lost,
            None,
            discard,
            &mut report,
        )?;
        if !ended {
            self.catch_up()?;
            if let Some(held) = self
                .sessions
                .get_mut(&claimed.session_id)
                .and_then(|view| view.active.as_mut())
                .filter(|held| held.execution_id == ending.execution_id)
            {
                held.reported_end = ending.reported_end;
                held.follow.end_failures = ending.follow.end_failures;
                held.follow.end_tried_at = ending.follow.end_tried_at;
            }
        }
        report.warnings.insert(0, why);
        Ok(Outcome::Failed(report.warnings.join("; ")))
    }

    /// Whether the log shows this command's typing never began, after
    /// folding what this tick appended: only then may its start be
    /// discarded.
    fn never_typed(&mut self, claimed: &Claimed, active: &Active) -> Result<bool, StoreError> {
        self.catch_up()?;
        Ok(matches!(
            self.typing_of(claimed, &active.execution_id),
            Typing::Due
        ))
    }

    /// The agent's branch, created at `from` (else at the base) unless it
    /// exists. Returns where a new branch began; an existing branch ignores
    /// `from`, with a warning.
    fn ensure_branch(
        &self,
        claimed: &Claimed,
        branch: &BranchName,
        from: Option<&str>,
        report: &mut TickReport,
    ) -> Result<Option<Fork>, String> {
        let fork = match self.workspaces.ensure_branch(branch, from) {
            Ok(Branch::Created(fork)) => Some(fork),
            Ok(Branch::Existing) => None,
            Err(error) => return Err(format!("ensure branch {branch}: {error}")),
        };
        if let Some(from) = from
            && fork.as_ref().is_none_or(|fork| fork.from != from)
        {
            report.warnings.push(format!(
                "{}: branch {branch} exists; from {from} ignored",
                claimed.session_id
            ));
        }
        Ok(fork)
    }

    /// A new Claude session gets its id from the Bridge, recorded before the
    /// engine starts, so the transcript it will write is known. Returns the
    /// id, or `None` when the engine names its own session or one is resumed.
    fn name_session(
        &mut self,
        claimed: &Claimed,
        native: Option<&str>,
        active: &Active,
    ) -> Result<Option<String>, StoreError> {
        if native.is_some() {
            return Ok(None);
        }
        let id = claude_session_id(&active.execution_id);
        self.append(
            &claimed.agent_id,
            &claimed.session_id,
            Some(&active.execution_id),
            Source::Bridge,
            Some(format!("{}:native", active.execution_id)),
            EventKind::NativeSessionStarted {
                engine: ENGINE.to_string(),
                native_session_id: id.clone(),
                transcript_path: Some(transcript_path(
                    &self.config.claude_projects,
                    &active.workspace,
                    &id,
                )),
                engine_version: None,
            },
        )?;
        self.catch_up()?;
        Ok(Some(id))
    }

    fn launch(&self, claimed: &Claimed, active: &Active, run: &Run<'_>) -> Launch {
        let Run {
            native,
            resume,
            settings,
        } = *run;
        let mut arguments = vec![
            "run".to_string(),
            "--".to_string(),
            self.config.claude_program.clone(),
        ];
        arguments.extend(claude_arguments(native, resume, settings));
        let mut environment = self.config.environment.clone();
        for (session, overrides) in &self.config.session_environment {
            if session != claimed.session_id.as_str() {
                continue;
            }
            for (name, value) in overrides {
                environment.retain(|(known, _)| known != name);
                environment.push((name.clone(), value.clone()));
            }
        }
        environment.push(("SPAWN_AGENT_ID".to_string(), claimed.agent_id.to_string()));
        environment.push((
            "SPAWN_SESSION_ID".to_string(),
            claimed.session_id.to_string(),
        ));
        environment.push((
            "SPAWN_EXECUTION_ID".to_string(),
            active.execution_id.to_string(),
        ));
        // The runner writes the exit status where the Bridge reads it: the
        // pane needs no login of its own.
        environment.push(("SPAWN_EXIT_FILE".to_string(), exit_file(active)));
        Launch {
            terminal: active.terminal(),
            workspace: active.workspace.clone(),
            cwd: active.workspace.path().to_string(),
            program: self.config.spawn_program.clone(),
            arguments,
            environment,
        }
    }

    fn type_prompt(
        &mut self,
        claimed: &Claimed,
        active: &Active,
        prompt: &str,
        fresh: bool,
    ) -> Result<Outcome, StoreError> {
        let mut last = ScreenState::Unknown;
        // Only a fresh session's directory can be new to the engine: a
        // resume runs where its session ran, which was trusted then.
        let mut trusted = !fresh;
        // The trust dialog comes before the engine draws any prompt: once a
        // prompt was seen, nothing on the pane is that dialog any more.
        let mut prompt_seen = false;
        // A dialog frame is answered only when the next read shows it again.
        let mut dialog: Option<String> = None;
        // The trust dialog comes before the engine starts up, so answering it
        // restarts the budget instead of spending it.
        let mut attempt = 0;
        while attempt < self.config.ready_attempts {
            if attempt > 0 {
                sleep(self.config.ready_interval);
            }
            attempt += 1;
            let Ok(pane) =
                self.terminals
                    .capture_tail(&active.terminal(), &active.workspace, TAIL_LINES)
            else {
                return Ok(Outcome::Failed("terminal disappeared".to_string()));
            };
            let reading = read_claude_screen(&pane);
            prompt_seen |= reading.state != ScreenState::Unknown;
            // The Bridge exported this directory itself, so it trusts it.
            // Answer once (a second answer would land in the input box), and
            // only the dialog itself: on two equal reads, before any prompt.
            if !trusted && !prompt_seen && is_trust_dialog(&pane) {
                if dialog.as_deref() != Some(pane.as_str()) {
                    dialog = Some(pane);
                    continue;
                }
                trusted = true;
                if let Err(error) =
                    self.terminals
                        .send_keys(&active.terminal(), &active.workspace, &TRUST_KEYS)
                {
                    return Ok(Outcome::Failed(format!("answer trust dialog: {error}")));
                }
                attempt = 1;
                continue;
            }
            dialog = None;
            last = reading.state;
            if !reading.state.accepts_input() {
                continue;
            }
            self.append(
                &claimed.agent_id,
                &claimed.session_id,
                Some(&active.execution_id),
                Source::Bridge,
                Some(format!("{}:screen:ready", active.execution_id)),
                EventKind::ScreenObserved {
                    state: reading.state,
                    evidence: reading.evidence,
                },
            )?;
            return match self.type_once(claimed, active, prompt)? {
                Ok(()) => Ok(Outcome::Succeeded),
                Err(error) => Ok(Outcome::Failed(format!("submit prompt: {error}"))),
            };
        }
        Ok(Outcome::Failed(format!(
            "pane never became ready, last state {last:?}"
        )))
    }

    fn input(&mut self, claimed: &Claimed, text: &str) -> Result<Outcome, StoreError> {
        let Some(active) = self.running(&claimed.session_id) else {
            return Ok(Outcome::Failed(format!(
                "{} runs no execution",
                claimed.session_id
            )));
        };
        // A retried input is never typed twice.
        match self.typing_of(claimed, &active.execution_id) {
            Typing::Due => {}
            Typing::Done => return Ok(Outcome::Succeeded),
            Typing::Unknown => {
                return Ok(Outcome::Failed(format!("the text {MAY_HAVE_TYPED}")));
            }
        }
        let Ok(pane) =
            self.terminals
                .capture_tail(&active.terminal(), &active.workspace, TAIL_LINES)
        else {
            return Ok(Outcome::Failed("terminal disappeared".to_string()));
        };
        let reading = read_claude_screen(&pane);
        self.append(
            &claimed.agent_id,
            &claimed.session_id,
            Some(&active.execution_id),
            Source::Bridge,
            None,
            EventKind::ScreenObserved {
                state: reading.state,
                evidence: reading.evidence,
            },
        )?;
        if !reading.state.accepts_input() {
            return Ok(Outcome::Failed(format!(
                "pane state {:?} {PANE_NOT_READY}",
                reading.state
            )));
        }
        match self.type_once(claimed, &active, text)? {
            Ok(()) => Ok(Outcome::Succeeded),
            Err(error) => Ok(Outcome::Failed(format!("submit text: {error}"))),
        }
    }

    /// `Stop`, or with `discard` the `Discard` that unmounts even when the
    /// final checkpoint fails.
    fn stop(
        &mut self,
        claimed: &Claimed,
        discard: bool,
        tick: &mut TickReport,
    ) -> Result<Outcome, StoreError> {
        let Some(mut active) = self.running(&claimed.session_id) else {
            return Ok(Outcome::Succeeded);
        };
        let mut report = TickReport::default();
        // A stop of an end already under way keeps its cause.
        let (cause, exit_code) = active.reported_end.unwrap_or((EndCause::Stopped, None));
        let ended = self.end(
            &claimed.agent_id,
            &claimed.session_id,
            &mut active,
            cause,
            exit_code,
            discard,
            &mut report,
        )?;
        self.catch_up()?;
        if ended {
            // A discard that lost work says so in the tick's report too.
            tick.warnings.append(&mut report.warnings);
            return Ok(Outcome::Succeeded);
        }
        // The stop stays due: each tick tries the same end again.
        if let Some(held) = self
            .sessions
            .get_mut(&claimed.session_id)
            .and_then(|view| view.active.as_mut())
        {
            held.reported_end = active.reported_end;
            held.follow.end_failures = active.follow.end_failures;
            held.follow.end_tried_at = active.follow.end_tried_at;
        }
        if report.warnings.is_empty() {
            report.warnings.push(format!(
                "{}: the end is stalled and its final checkpoint retried every {} s; a \
                 discard command gives the uncommitted work up",
                claimed.session_id,
                END_RETRY_MS / 1000
            ));
        }
        Ok(Outcome::Failed(report.warnings.join("; ")))
    }

    fn resume(
        &mut self,
        claimed: &Claimed,
        prompt: Option<&str>,
        model: Option<ModelProfile>,
        report: &mut TickReport,
    ) -> Result<Outcome, StoreError> {
        // A retried resume whose execution started goes on in `start`.
        let ours = ExecutionId::for_command(claimed.command_id);
        if self
            .running(&claimed.session_id)
            .is_some_and(|active| active.execution_id != ours)
        {
            return Ok(Outcome::Failed(format!(
                "{} still runs an execution",
                claimed.session_id
            )));
        }
        let Some(native) = self
            .sessions
            .get(&claimed.session_id)
            .and_then(|view| view.native.clone())
        else {
            return Ok(Outcome::Failed("no native session to resume".to_string()));
        };
        self.start(
            claimed,
            &Begin {
                prompt: prompt.unwrap_or(""),
                parent: None,
                native: Some(native.as_str()),
                from: None,
                model,
            },
            report,
        )
    }
}

/// How an input fails when the pane does not show a prompt that takes
/// text (`pane state <state> does not accept input`): refused before
/// anything was typed, so nothing reached the engine. The pane may be a
/// moment from ready: a turn's end reaches the transcript before the screen
/// settles at the prompt.
pub const PANE_NOT_READY: &str = "does not accept input";

/// How a command that may or may not have typed its text fails (the log
/// shows typing began and nothing shows it arrived): it is not typed again,
/// and whoever sent it must not send it again blindly either.
pub const MAY_HAVE_TYPED: &str =
    "may have been typed and the log cannot tell; it is not typed again";

/// A digest of a list of paths that stays the same across builds and hosts
/// (FNV-1a over the paths, each ended by a NUL): part of a record's key.
fn files_digest(files: &[String]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for file in files {
        for byte in file.bytes().chain(std::iter::once(0)) {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    hash
}

/// Settings passed to Claude Code for an engine on `profile`, through
/// `relay` when there is one.
///
/// `--settings` takes inline JSON, which Claude Code merges over the user's
/// settings files: a key set here wins, and `env` merges name by name
/// (checked with Claude Code 2.1.318: an inline `env` naming only the base
/// URL and the haiku slot kept the user's main model and auth token). So
/// the gateway's token reaches the engine from the user's own settings, as
/// it would without Spawn, and Spawn never holds it.
///
/// The permission keys are repeated so the engine never depends on the
/// user's file for them: without them it stops on a startup dialog and
/// never reads the prompt the Bridge types into the pane. No hooks: the
/// Bridge reads what the engine does from its transcript and its screen.
///
/// With a relay, the engine calls it instead of the gateway, and the haiku
/// slot (Claude Code's small model for background work) is the relay's
/// small model; the cheap profile also puts its main model in the main,
/// sonnet and opus slots. Without a relay nothing about models is set: the
/// cheap profile needs one, and the Bridge refuses to start it without.
pub fn claude_settings(relay: Option<&ModelRelay>, profile: ModelProfile) -> String {
    let mut settings = json!({
        "permissions": { "defaultMode": "bypassPermissions" },
        "skipDangerousModePermissionPrompt": true,
    });
    if let Some(relay) = relay {
        let mut env = serde_json::Map::new();
        env.insert("ANTHROPIC_BASE_URL".to_string(), json!(relay.url));
        if profile == ModelProfile::Cheap {
            for slot in [
                "ANTHROPIC_MODEL",
                "ANTHROPIC_DEFAULT_OPUS_MODEL",
                "ANTHROPIC_DEFAULT_SONNET_MODEL",
            ] {
                env.insert(slot.to_string(), json!(relay.cheap_model));
            }
        }
        env.insert(
            "ANTHROPIC_DEFAULT_HAIKU_MODEL".to_string(),
            json!(relay.small_model),
        );
        settings["env"] = serde_json::Value::Object(env);
    }
    settings.to_string()
}

/// Claude arguments for the session `native`: a new one with that id, or
/// the earlier one resumed, with `settings` from [`claude_settings`]. The
/// prompt is typed into the pane, never passed here.
pub fn claude_arguments(native: Option<&str>, resume: bool, settings: &str) -> Vec<String> {
    // The engine runs in a container with no credentials of its own, so the
    // permission prompts have nowhere to go. The sandbox is the boundary.
    let mut arguments = vec![
        "--ax-screen-reader".to_string(),
        "--allow-dangerously-skip-permissions".to_string(),
        "--settings".to_string(),
        settings.to_string(),
    ];
    if let Some(native) = native {
        arguments.push(if resume { "--resume" } else { "--session-id" }.to_string());
        arguments.push(native.to_string());
    }
    arguments
}

/// The id of the Claude session a new execution starts: a UUID derived from
/// the execution, so the Bridge knows the transcript before the engine runs.
pub fn claude_session_id(execution: &ExecutionId) -> String {
    let hash = |seed: u64| {
        execution.as_str().bytes().fold(seed, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        })
    };
    let (high, low) = (hash(0xcbf2_9ce4_8422_2325), hash(0x8422_2325_cbf2_9ce4));
    // Version 4 and the RFC 4122 variant, so the engine accepts it.
    let high = (high & !0xf000) | 0x4000;
    let low = (low & !(0b11 << 62)) | (0b10 << 62);
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        high >> 32,
        (high >> 16) & 0xffff,
        high & 0xffff,
        low >> 48,
        low & 0xffff_ffff_ffff
    )
}

/// Where Claude Code writes the transcript of `native` for an engine started
/// in `workspace`: its projects directory, one directory per working
/// directory with every character but ASCII letters and digits replaced by
/// `-`, one file per session.
pub fn transcript_path(projects: &str, workspace: &WorkspaceId, native: &str) -> String {
    let directory: String = workspace
        .path()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!(
        "{}/{directory}/{native}.jsonl",
        projects.trim_end_matches('/')
    )
}

/// Where the runner writes the engine's exit status. `.spawn/` is never
/// committed.
fn exit_file(active: &Active) -> String {
    format!(
        "{}/.spawn/exit-{}",
        active.workspace.path(),
        active.execution_id
    )
}
