//! Agent Bridge: turns commands into executions and persists what happens.
//!
//! The Bridge owns four jobs: start and resume engines, record events, commit
//! workspace state, and map native identifiers to Spawn sessions. It does not
//! run the agent loop and does not judge state.
//!
//! The Bridge is the only writer of a Bridge-sourced `execution.ended`, and
//! it writes that event only after the final checkpoint, the terminal kill
//! and the unmount. A refold of the log therefore never repeats cleanup.

use std::collections::HashMap;
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;

use serde_json::json;

use crate::command::{Claimed, CommandAction, Outcome};
use crate::event::{
    ArtifactRef, CheckpointReason, EndCause, Engine, EventKind, EventRecord, Fork, GlobalPosition,
    NewEvent, Source,
};
use crate::id::{AgentId, BranchName, ExecutionId, SessionId, TerminalName, WorkspaceId};
use crate::ingest::{claude_line, codex_line};
use crate::port::{
    BaseMerge, Branch, CommandQueue, Commit, EventLog, Launch, READ_LIMIT, StoreError,
    TerminalHost, WorkspaceError, WorkspaceStore,
};
use crate::screen::{ScreenState, TAIL_LINES, TRUST_KEYS, is_trust_dialog, read_claude_screen};
use crate::time::{Clock, SystemClock};

/// Transcript lines this much older than the execution's start are still
/// read as its own.
const CLOCK_SLACK_MS: i64 = 60_000;

/// An idle execution's workspace is offered the source's main at most this
/// often, by the Bridge's clock, besides right after each turn ends: an agent
/// already idle when main moves gets the merge (or the conflict markers)
/// without waiting for a turn.
pub const IDLE_MERGE_MS: i64 = 30_000;

/// Final checkpoints an ending execution tries on consecutive ticks before
/// its end counts as stalled: the terminal is killed, the workspace stays
/// mounted, `execution.end_stalled` asks for attention, and the checkpoint is
/// retried every [`END_RETRY_MS`]. A workspace is unmounted only after its
/// final checkpoint commits, or on a `discard` command.
pub const END_ATTEMPTS: u32 = 20;

/// How often a stalled end retries its final checkpoint, by the Bridge's
/// clock.
pub const END_RETRY_MS: i64 = 30_000;

/// Engine choice and prompt of one launch.
///
/// Grouped so that [`Bridge::launch`] keeps a readable parameter list.
struct Run<'a> {
    engine: Engine,
    native: Option<&'a str>,
    /// `native` names an earlier session to resume, not a new one.
    resume: bool,
    prompt: &'a str,
}

/// What one start asks for. Grouped so that [`Bridge::start`] keeps a
/// readable parameter list.
struct Begin<'a> {
    engine: Engine,
    prompt: &'a str,
    parent: Option<&'a SessionId>,
    /// A session to resume.
    native: Option<&'a str>,
    /// Where a new branch starts (`start {from}`).
    from: Option<&'a str>,
}

/// Bridge settings.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    /// Absolute path of the `spawn` binary, which runs the engine (`spawn run`).
    pub spawn_program: String,
    /// Claude Code executable.
    pub claude_program: String,
    /// Codex executable.
    pub codex_program: String,
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
}

impl BridgeConfig {
    /// Defaults for one worker.
    pub fn new(worker: &str) -> Self {
        Self {
            spawn_program: "spawn".to_string(),
            claude_program: "claude".to_string(),
            codex_program: "codex".to_string(),
            environment: Vec::new(),
            session_environment: Vec::new(),
            worker: worker.to_string(),
            lease_ttl: Duration::from_mins(2),
            max_commands_per_tick: 16,
            ready_attempts: 40,
            ready_interval: Duration::from_millis(250),
            claude_projects: "/root/.claude/projects".to_string(),
            screen_refresh: Duration::from_secs(30),
        }
    }
}

/// Running execution of one session.
#[derive(Debug, Clone)]
struct Active {
    execution_id: ExecutionId,
    terminal: TerminalName,
    workspace: WorkspaceId,
    engine: Engine,
    /// A tool or turn finished after the last commit.
    // ponytail: an `Unchanged` commit writes no event, so after a refold an
    // idle session repeats one no-op commit per tick; record attempts if
    // Workspace calls get expensive.
    checkpoint: Option<CheckpointReason>,
    /// A turn runs: a prompt reached the engine (or the Bridge typed one), or
    /// a tool started, and no turn end followed. The base is never merged
    /// into a workspace while a turn runs.
    turn_open: bool,
    /// The runner reported the engine exit; cleanup is still due.
    reported_end: Option<(EndCause, Option<i32>)>,
    /// When the execution started: transcript lines before it belong to
    /// earlier executions of the session.
    started_ms: i64,
    /// What the Bridge has read of the engine's files and screen.
    follow: Follow,
}

/// The Bridge's own view of a running engine, rebuilt after a restart: read
/// positions start at zero (event keys make a second read a no-op), and the
/// last recorded screen is folded from the log.
#[derive(Debug, Clone, Default)]
struct Follow {
    /// Bytes of the transcript (Claude) or output copy (Codex) read so far.
    offset: u64,
    /// Codex output lines read so far, which key their events.
    lines: u64,
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
    /// When `merge_base` last ran for this execution, by the Bridge's clock.
    merge_at: Option<i64>,
    /// The last merge warning reported: the same conflict (or failure) is
    /// reported once, not every [`IDLE_MERGE_MS`].
    merge_warning: Option<String>,
    /// This tick read an event that asks for a checkpoint, not folded yet:
    /// the workspace may hold work not on its branch.
    checkpoint_read: bool,
    /// Final checkpoints of this execution's end that failed. The
    /// workspace stays mounted until one succeeds (or a `discard`); from
    /// [`END_ATTEMPTS`] on the end is stalled.
    end_failures: u32,
    /// When the end's final checkpoint was last tried, by the Bridge's
    /// clock: a stalled end retries every [`END_RETRY_MS`].
    end_tried_at: Option<i64>,
}

/// What the Bridge knows about one session.
#[derive(Debug, Clone)]
struct SessionView {
    agent_id: AgentId,
    native: Option<(Engine, String)>,
    active: Option<Active>,
    /// What merges of main changed under the agent since the last prompt it
    /// received: told with the next prompt the Bridge types.
    notice: Notice,
}

/// Files merges of main changed in an agent's workspace that the agent has
/// not been told of yet. Folded from the log: a merge adds its files, a
/// prompt reaching the engine clears them (the Bridge put the notice in it).
#[derive(Debug, Clone, Default)]
struct Notice {
    merged: std::collections::BTreeSet<String>,
    conflicted: std::collections::BTreeSet<String>,
}

impl Notice {
    /// The text that goes before the next prompt, if anything is to be told.
    fn text(&self) -> Option<String> {
        let list = |files: &std::collections::BTreeSet<String>| {
            files.iter().cloned().collect::<Vec<_>>().join(", ")
        };
        let merged: std::collections::BTreeSet<String> =
            self.merged.difference(&self.conflicted).cloned().collect();
        match (merged.is_empty(), self.conflicted.is_empty()) {
            (true, true) => None,
            (false, true) => Some(format!(
                "Spawn merged main into your workspace since your last turn: {}. Re-read them \
                 before editing.",
                list(&merged)
            )),
            (true, false) => Some(format!(
                "Spawn merged main into your workspace since your last turn; these files carry \
                 conflict markers: {}. Resolve them by editing the marked regions; do not \
                 rewrite the files from memory.",
                list(&self.conflicted)
            )),
            (false, false) => Some(format!(
                "Spawn merged main into your workspace since your last turn: {}. Re-read them \
                 before editing. These files carry conflict markers: {}. Resolve them by \
                 editing the marked regions; do not rewrite the files from memory.",
                list(&merged),
                list(&self.conflicted)
            )),
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
        }
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
        self.catch_up()
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
                active: None,
                notice: Notice::default(),
            });
        match &event.kind {
            EventKind::ArtifactCommitted {
                reason: CheckpointReason::BaseMerged,
                files,
                ..
            } => view.notice.merged.extend(files.iter().cloned()),
            EventKind::MergeConflicted {
                files,
                marked: true,
            } => view.notice.conflicted.extend(files.iter().cloned()),
            EventKind::MessageReceived => view.notice = Notice::default(),
            _ => {}
        }
        let current = view
            .active
            .as_ref()
            .is_some_and(|active| event.execution_id.as_ref() == Some(&active.execution_id));
        match &event.kind {
            EventKind::ExecutionStarted {
                engine,
                terminal,
                workspace,
                ..
            } => {
                if let Some(execution_id) = event.execution_id.clone() {
                    view.active = Some(Active {
                        execution_id,
                        terminal: terminal.clone(),
                        workspace: workspace.clone(),
                        engine: *engine,
                        checkpoint: None,
                        turn_open: false,
                        reported_end: None,
                        started_ms: record.recorded_at.0,
                        follow: Follow::default(),
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
                engine,
                native_session_id,
                ..
            } => {
                view.native = Some((*engine, native_session_id.clone()));
            }
            // A merge of main is no checkpoint of the agent's work: one
            // still due stays due.
            EventKind::ArtifactCommitted { reason, .. }
                if *reason != CheckpointReason::BaseMerged =>
            {
                if let Some(active) = view.active.as_mut() {
                    active.checkpoint = None;
                }
            }
            other => {
                if let Some(active) = view.active.as_mut() {
                    if let Some(open) = opens_turn(other) {
                        active.turn_open = open;
                    }
                    if let Some(reason) = other.checkpoint_reason() {
                        active.checkpoint = Some(reason);
                    }
                }
            }
        }
    }

    fn act(&mut self, report: &mut TickReport) -> Result<(), StoreError> {
        let running: Vec<(SessionId, AgentId, Option<String>, Active)> = self
            .sessions
            .iter()
            .filter_map(|(session, view)| {
                let active = view.active.clone()?;
                let native = view.native.as_ref().map(|(_, id)| id.clone());
                Some((session.clone(), view.agent_id.clone(), native, active))
            })
            .collect();
        for (session, agent_id, native, mut active) in running {
            let ended =
                self.advance(&agent_id, &session, native.as_deref(), &mut active, report)?;
            if let Some(view) = self.sessions.get_mut(&session) {
                if ended {
                    view.active = None;
                } else if let Some(held) = view.active.as_mut() {
                    held.checkpoint = None;
                    held.turn_open = active.turn_open;
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
        active.follow.checkpoint_read = false;
        self.follow_output(agent_id, session, native, active, report)?;
        let end = match active
            .reported_end
            .or_else(|| self.exit_status(session, active, report))
        {
            Some(end) => Some(end),
            None => match self.terminals.is_alive(&active.terminal, &active.workspace) {
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
                        .push(format!("is_alive {}: {error}", active.terminal));
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
            let committed = self.checkpoint(agent_id, session, active, reason, report)?;
            // An idle workspace whose state is on its branch takes the
            // source's main. `turn_open` includes what was read just now.
            if committed && reason == CheckpointReason::TurnStopped && !active.turn_open {
                self.merge_base(agent_id, session, active, report)?;
            }
        } else if self.idle_merge_due(active) {
            self.merge_base(agent_id, session, active, report)?;
        }
        Ok(false)
    }

    /// Whether an execution with nothing to checkpoint is offered main now:
    /// no turn open, nothing read this tick that asks for a checkpoint, the
    /// screen last read ready, and the last offer at least [`IDLE_MERGE_MS`]
    /// ago.
    fn idle_merge_due(&self, active: &Active) -> bool {
        let ready = active
            .follow
            .screen
            .is_some_and(|(state, _)| state == ScreenState::Ready);
        let now = self.clock.now().0;
        !active.turn_open
            && !active.follow.checkpoint_read
            && ready
            && active
                .follow
                .merge_at
                .is_none_or(|at| now.saturating_sub(at) >= IDLE_MERGE_MS)
    }

    /// Merge the source's main into the branch of an idle workspace, and
    /// record the merge as a checkpoint. A conflict is reported, once per
    /// conflict: landing tells the agent, and its files carry the markers.
    fn merge_base(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        active: &mut Active,
        report: &mut TickReport,
    ) -> Result<(), StoreError> {
        active.follow.merge_at = Some(self.clock.now().0);
        let conflict = match self.workspaces.merge_base(&active.workspace) {
            Ok(BaseMerge::Current) => None,
            Ok(BaseMerge::Merged { commit, files }) => {
                let artifact = ArtifactRef {
                    project: self.workspaces.project().to_string(),
                    branch: BranchName::for_agent(agent_id),
                    commit,
                };
                self.append(
                    agent_id,
                    session,
                    Some(&active.execution_id),
                    Source::Bridge,
                    Some(format!("commit:{}", artifact.commit)),
                    EventKind::ArtifactCommitted {
                        reason: CheckpointReason::BaseMerged,
                        artifact,
                        files,
                    },
                )?;
                None
            }
            Ok(BaseMerge::Conflicted { files, marked }) => Some((files, marked)),
            Err(error) => {
                let text = format!("{session}: merge main: {error}");
                if active.follow.merge_warning.as_ref() != Some(&text) {
                    report.warnings.push(text.clone());
                }
                active.follow.merge_warning = Some(text);
                return Ok(());
            }
        };
        let Some((files, marked)) = conflict else {
            active.follow.merge_warning = None;
            return Ok(());
        };
        let text = if marked {
            format!(
                "{session}: main conflicts with the branch in {}; the workspace carries \
                     conflict markers there until they are resolved",
                files.join(", ")
            )
        } else {
            format!(
                "{session}: main conflicts with the branch in {} in a way conflict markers \
                 cannot show (a file deleted or renamed on one side); left unmerged",
                files.join(", ")
            )
        };
        // Once per conflict: reported, and recorded with its files.
        if active.follow.merge_warning.as_ref() != Some(&text) {
            report.warnings.push(text.clone());
            self.append(
                agent_id,
                session,
                Some(&active.execution_id),
                Source::Bridge,
                None,
                EventKind::MergeConflicted { files, marked },
            )?;
        }
        active.follow.merge_warning = Some(text);
        Ok(())
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

    /// Append the events in what the engine wrote since the last read: the
    /// Claude transcript, or the runner's copy of the Codex output. Only
    /// whole lines are read; a line being written waits for the next tick.
    fn follow_output(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        native: Option<&str>,
        active: &mut Active,
        report: &mut TickReport,
    ) -> Result<(), StoreError> {
        let path = match active.engine {
            Engine::Claude => match native {
                Some(native) => {
                    transcript_path(&self.config.claude_projects, &active.workspace, native)
                }
                None => return Ok(()),
            },
            Engine::Codex => codex_output(active),
        };
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
            let events = match active.engine {
                // A minute of slack for a log clock ahead of this host's:
                // lines already recorded are dropped by their keys anyway.
                Engine::Claude => claude_line(&line, active.started_ms - CLOCK_SLACK_MS)
                    .into_iter()
                    .map(|event| (event.source_event_id, event.kind))
                    .collect(),
                Engine::Codex => {
                    active.follow.lines += 1;
                    let key = format!("{}:codex:{}", active.execution_id, active.follow.lines);
                    codex_line(&line)
                        .map(|kind| (Some(key), kind))
                        .into_iter()
                        .collect::<Vec<_>>()
                }
            };
            let source = match active.engine {
                Engine::Claude => Source::Claude,
                Engine::Codex => Source::Codex,
            };
            for (key, kind) in events {
                if let Some(open) = opens_turn(&kind) {
                    active.turn_open = open;
                }
                active.follow.checkpoint_read |= kind.checkpoint_reason().is_some();
                self.append(
                    agent_id,
                    session,
                    Some(&active.execution_id),
                    source,
                    key,
                    kind,
                )?;
            }
        }
        active.follow.offset += u64::try_from(end + 1).unwrap_or(u64::MAX);
        Ok(())
    }

    /// Read the pane and record its state when it changes, or again after
    /// [`BridgeConfig::screen_refresh`] while a turn keeps it busy. Only a
    /// Claude pane has a layout the Bridge knows.
    fn follow_screen(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        active: &mut Active,
    ) -> Result<(), StoreError> {
        if active.engine != Engine::Claude {
            return Ok(());
        }
        let Ok(pane) = self
            .terminals
            .capture_tail(&active.terminal, &active.workspace, TAIL_LINES)
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
            .terminate(&active.terminal, &active.workspace)
        {
            report
                .warnings
                .push(format!("terminate {}: {error}", active.terminal));
            return Ok(false);
        }
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
            .terminate(&active.terminal, &active.workspace)
        {
            report
                .warnings
                .push(format!("terminate {}: {error}", active.terminal));
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
    /// workspace's state is now on its branch (false: the commit failed and
    /// was reported).
    fn checkpoint(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        active: &Active,
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
                self.append(
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

    fn handle(
        &mut self,
        claimed: &Claimed,
        report: &mut TickReport,
    ) -> Result<Outcome, StoreError> {
        let Ok(action) = claimed.action() else {
            return Ok(Outcome::Failed("stored action is not valid".to_string()));
        };
        match action {
            CommandAction::Start {
                engine,
                prompt,
                parent_session_id,
                from,
            } => self.start(
                claimed,
                &Begin {
                    engine,
                    prompt: &prompt,
                    parent: parent_session_id.as_ref(),
                    native: None,
                    from: from.as_deref(),
                },
                report,
            ),
            CommandAction::Input { text } => self.input(claimed, &text),
            CommandAction::Stop => self.stop(claimed, false, report),
            CommandAction::Discard => self.stop(claimed, true, report),
            CommandAction::Resume { prompt } => self.resume(claimed, prompt.as_deref(), report),
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
            engine,
            prompt,
            parent,
            native,
            from,
        } = *begin;
        if self.running(&claimed.session_id).is_some() {
            return Ok(Outcome::Failed(format!(
                "{} already runs an execution",
                claimed.session_id
            )));
        }
        let branch = BranchName::for_agent(&claimed.agent_id);
        let fork = match self.ensure_branch(claimed, &branch, from, report) {
            Ok(fork) => fork,
            Err(why) => return Ok(Outcome::Failed(why)),
        };
        // ponytail: a crash between mount and `execution.started` leaks one
        // mount; a startup sweep of unrecorded mounts fixes it if it shows up.
        let workspace = match self.workspaces.mount(&branch) {
            Ok(workspace) => workspace,
            Err(error) => return Ok(Outcome::Failed(format!("mount {branch}: {error}"))),
        };
        if let Some(parent_session) = parent {
            self.append(
                &claimed.agent_id,
                &claimed.session_id,
                None,
                Source::Bridge,
                Some(format!("{}:linked:{parent_session}", claimed.session_id)),
                EventKind::SessionLinked {
                    parent_session_id: parent_session.clone(),
                },
            )?;
        }
        let execution = ExecutionId::for_command(claimed.command_id);
        let active = Active {
            terminal: TerminalName::for_execution(&execution),
            execution_id: execution,
            workspace,
            engine,
            checkpoint: None,
            turn_open: false,
            reported_end: None,
            started_ms: self.clock.now().0,
            follow: Follow::default(),
        };
        // The event goes first: a crash after it leaves a recorded execution
        // whose missing terminal the next tick ends as lost.
        self.append(
            &claimed.agent_id,
            &claimed.session_id,
            Some(&active.execution_id),
            Source::Bridge,
            Some(format!("{}:started", active.execution_id)),
            EventKind::ExecutionStarted {
                engine,
                terminal: active.terminal.clone(),
                workspace: active.workspace.clone(),
                command_id: claimed.command_id,
                fork,
            },
        )?;
        let fresh = self.name_session(claimed, engine, native, &active)?;
        let launch = self.launch(
            claimed,
            &active,
            &Run {
                engine,
                native: fresh.as_deref().or(native),
                resume: native.is_some(),
                prompt,
            },
        );
        if let Err(error) = self.terminals.start(&launch) {
            return self.abandon(claimed, &active, format!("start terminal: {error}"));
        }
        if engine == Engine::Claude && !prompt.is_empty() {
            let outcome = self.type_prompt(claimed, &active, prompt)?;
            if let Outcome::Failed(why) = outcome {
                // An engine that never got its prompt must not look alive:
                // whoever started it would wait on it forever.
                return self.abandon(claimed, &active, why);
            }
            // The prompt starts a turn before the transcript says so.
            self.catch_up()?;
            self.open_turn(&claimed.session_id);
            return Ok(outcome);
        }
        Ok(Outcome::Succeeded)
    }

    /// End a start that failed as lost; the failure names `why` first.
    fn abandon(
        &self,
        claimed: &Claimed,
        active: &Active,
        why: String,
    ) -> Result<Outcome, StoreError> {
        let mut report = TickReport::default();
        // Nothing ran in it yet: an unmount loses nothing.
        self.end(
            &claimed.agent_id,
            &claimed.session_id,
            &mut active.clone(),
            EndCause::Lost,
            None,
            true,
            &mut report,
        )?;
        report.warnings.insert(0, why);
        Ok(Outcome::Failed(report.warnings.join("; ")))
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
        engine: Engine,
        native: Option<&str>,
        active: &Active,
    ) -> Result<Option<String>, StoreError> {
        if engine != Engine::Claude || native.is_some() {
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
                engine,
                native_session_id: id.clone(),
                transcript_path: Some(transcript_path(
                    &self.config.claude_projects,
                    &active.workspace,
                    &id,
                )),
            },
        )?;
        self.catch_up()?;
        Ok(Some(id))
    }

    fn launch(&self, claimed: &Claimed, active: &Active, run: &Run<'_>) -> Launch {
        let Run {
            engine,
            native,
            resume,
            prompt,
        } = *run;
        let mut arguments = vec!["run".to_string(), "--".to_string()];
        arguments.push(match engine {
            Engine::Claude => self.config.claude_program.clone(),
            Engine::Codex => self.config.codex_program.clone(),
        });
        arguments.extend(match engine {
            Engine::Claude => claude_arguments(native, resume),
            Engine::Codex => codex_arguments(native.filter(|_| resume), prompt),
        });
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
        environment.push(("SPAWN_ENGINE".to_string(), engine.as_str().to_string()));
        // The runner writes the exit status, and a copy of Codex output,
        // where the Bridge reads them: the pane needs no login of its own.
        environment.push(("SPAWN_EXIT_FILE".to_string(), exit_file(active)));
        if engine == Engine::Codex {
            environment.push(("SPAWN_ENGINE_OUTPUT".to_string(), codex_output(active)));
        }
        Launch {
            terminal: active.terminal.clone(),
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
    ) -> Result<Outcome, StoreError> {
        let mut last = ScreenState::Unknown;
        let mut trusted = false;
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
                    .capture_tail(&active.terminal, &active.workspace, TAIL_LINES)
            else {
                return Ok(Outcome::Failed("terminal disappeared".to_string()));
            };
            // The Bridge exported this directory itself, so it trusts it.
            // Answer once: a second answer would land in the input box.
            if !trusted && is_trust_dialog(&pane) {
                trusted = true;
                if let Err(error) =
                    self.terminals
                        .send_keys(&active.terminal, &active.workspace, &TRUST_KEYS)
                {
                    return Ok(Outcome::Failed(format!("answer trust dialog: {error}")));
                }
                attempt = 1;
                continue;
            }
            let reading = read_claude_screen(&pane);
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
            return match self
                .terminals
                .submit_text(&active.terminal, &active.workspace, prompt)
            {
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
        if active.engine != Engine::Claude {
            return Ok(Outcome::Failed(
                "only a Claude execution accepts typed input".to_string(),
            ));
        }
        let Ok(pane) = self
            .terminals
            .capture_tail(&active.terminal, &active.workspace, TAIL_LINES)
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
                "pane state {:?} does not accept input",
                reading.state
            )));
        }
        let typed = self.with_notice(&claimed.session_id, text);
        match self
            .terminals
            .submit_text(&active.terminal, &active.workspace, &typed)
        {
            Ok(()) => {
                // A turn starts now, before its transcript says so.
                self.open_turn(&claimed.session_id);
                self.notice_delivered(&claimed.session_id);
                Ok(Outcome::Succeeded)
            }
            Err(error) => Ok(Outcome::Failed(format!("submit text: {error}"))),
        }
    }

    /// The Bridge typed a prompt: the session's turn is open from now on, so
    /// no merge touches the workspace until the turn ends.
    fn open_turn(&mut self, session: &SessionId) {
        if let Some(held) = self
            .sessions
            .get_mut(session)
            .and_then(|view| view.active.as_mut())
        {
            held.turn_open = true;
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
        report: &mut TickReport,
    ) -> Result<Outcome, StoreError> {
        if self.running(&claimed.session_id).is_some() {
            return Ok(Outcome::Failed(format!(
                "{} still runs an execution",
                claimed.session_id
            )));
        }
        let Some((engine, native)) = self
            .sessions
            .get(&claimed.session_id)
            .and_then(|view| view.native.clone())
        else {
            return Ok(Outcome::Failed("no native session to resume".to_string()));
        };
        let prompt = prompt.unwrap_or("");
        if engine == Engine::Codex && prompt.is_empty() {
            return Ok(Outcome::Failed("codex resume needs a prompt".to_string()));
        }
        // What merges changed goes with the prompt; nothing is typed
        // unprompted.
        let prompt = if prompt.is_empty() {
            String::new()
        } else {
            self.with_notice(&claimed.session_id, prompt)
        };
        let outcome = self.start(
            claimed,
            &Begin {
                engine,
                prompt: &prompt,
                parent: None,
                native: Some(native.as_str()),
                from: None,
            },
            report,
        )?;
        if outcome == Outcome::Succeeded && !prompt.is_empty() {
            self.notice_delivered(&claimed.session_id);
        }
        Ok(outcome)
    }

    /// `prompt`, after what merges of main changed under the agent since
    /// its last prompt, if anything did.
    fn with_notice(&self, session: &SessionId, prompt: &str) -> String {
        match self
            .sessions
            .get(session)
            .and_then(|view| view.notice.text())
        {
            Some(notice) => format!("{notice}\n\n{prompt}"),
            None => prompt.to_string(),
        }
    }

    /// The notice went out with a typed prompt (which the command records).
    fn notice_delivered(&mut self, session: &SessionId) {
        if let Some(view) = self.sessions.get_mut(session) {
            view.notice = Notice::default();
        }
    }
}

/// Whether an event opens a turn (`Some(true)`), ends one (`Some(false)`),
/// or says nothing about turns.
fn opens_turn(kind: &EventKind) -> Option<bool> {
    match kind {
        EventKind::MessageReceived | EventKind::ToolStarted { .. } => Some(true),
        EventKind::TurnStopped { .. } | EventKind::TurnFailed { .. } => Some(false),
        _ => None,
    }
}

/// Settings passed to Claude Code.
///
/// `--settings` takes inline JSON, and that JSON replaces the user's settings
/// file for the session. The permission keys must therefore be repeated here:
/// without them the engine stops on a startup dialog and never reads the
/// prompt the Bridge types into the pane. No hooks: the Bridge reads what
/// the engine does from its transcript and its screen.
pub fn claude_settings() -> String {
    json!({
        "permissions": { "defaultMode": "bypassPermissions" },
        "skipDangerousModePermissionPrompt": true,
    })
    .to_string()
}

/// Claude arguments for the session `native`: a new one with that id, or
/// the earlier one resumed. The prompt is typed into the pane, never
/// passed here.
pub fn claude_arguments(native: Option<&str>, resume: bool) -> Vec<String> {
    // The engine runs in a container with no credentials of its own, so the
    // permission prompts have nowhere to go. The sandbox is the boundary.
    let mut arguments = vec![
        "--ax-screen-reader".to_string(),
        "--allow-dangerously-skip-permissions".to_string(),
        "--settings".to_string(),
        claude_settings(),
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

/// Where the runner copies Codex's `exec --json` output.
fn codex_output(active: &Active) -> String {
    format!(
        "{}/.spawn/codex-{}.jsonl",
        active.workspace.path(),
        active.execution_id
    )
}

/// Codex arguments. `exec --json` prints one JSON object per line.
pub fn codex_arguments(native: Option<&str>, prompt: &str) -> Vec<String> {
    let mut arguments = vec!["exec".to_string(), "--json".to_string()];
    if let Some(native) = native {
        arguments.push("resume".to_string());
        arguments.push(native.to_string());
    }
    arguments.push("--".to_string());
    arguments.push(prompt.to_string());
    arguments
}
