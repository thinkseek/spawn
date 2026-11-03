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
    ArtifactRef, CheckpointReason, EndCause, Engine, EventKind, EventRecord, GlobalPosition,
    NewEvent, Source,
};
use crate::id::{AgentId, BranchName, ExecutionId, SessionId, TerminalName, WorkspaceId};
use crate::port::{
    CommandQueue, Commit, EventLog, Launch, StoreError, TerminalHost, WorkspaceError,
    WorkspaceStore,
};
use crate::screen::{ScreenState, TAIL_LINES, TRUST_KEYS, is_trust_dialog, read_claude_screen};
use crate::shell::shell_quote;

/// Engine choice and prompt of one launch.
///
/// Grouped so that [`Bridge::launch`] keeps a readable parameter list.
struct Run<'a> {
    engine: Engine,
    native: Option<&'a str>,
    prompt: &'a str,
}

/// Bridge settings.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    /// Absolute path of the `spawn` binary, used for hooks and for the runner.
    pub spawn_program: String,
    /// Claude Code executable.
    pub claude_program: String,
    /// Codex executable.
    pub codex_program: String,
    /// Extra environment for the engine process, for example the database
    /// URL that the hook and the runner need.
    pub environment: Vec<(String, String)>,
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
}

impl BridgeConfig {
    /// Defaults for one worker.
    pub fn new(worker: &str) -> Self {
        Self {
            spawn_program: "spawn".to_string(),
            claude_program: "claude".to_string(),
            codex_program: "codex".to_string(),
            environment: Vec::new(),
            worker: worker.to_string(),
            lease_ttl: Duration::from_mins(2),
            max_commands_per_tick: 16,
            ready_attempts: 40,
            ready_interval: Duration::from_millis(250),
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
    /// The runner reported the engine exit; cleanup is still due.
    reported_end: Option<(EndCause, Option<i32>)>,
}

/// What the Bridge knows about one session.
#[derive(Debug, Clone)]
struct SessionView {
    agent_id: AgentId,
    native: Option<(Engine, String)>,
    active: Option<Active>,
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
        }
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
            let outcome = self.handle(&claimed)?;
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
        let view = self
            .sessions
            .entry(event.session_id.clone())
            .or_insert_with(|| SessionView {
                agent_id: event.agent_id.clone(),
                native: None,
                active: None,
            });
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
                        reported_end: None,
                    });
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
            EventKind::NativeSessionStarted {
                engine,
                native_session_id,
                ..
            } => {
                view.native = Some((*engine, native_session_id.clone()));
            }
            EventKind::ArtifactCommitted { .. } => {
                if let Some(active) = view.active.as_mut() {
                    active.checkpoint = None;
                }
            }
            other => {
                if let Some(reason) = other.checkpoint_reason()
                    && let Some(active) = view.active.as_mut()
                {
                    active.checkpoint = Some(reason);
                }
            }
        }
    }

    fn act(&mut self, report: &mut TickReport) -> Result<(), StoreError> {
        let running: Vec<(SessionId, AgentId, Active)> = self
            .sessions
            .iter()
            .filter_map(|(session, view)| {
                let active = view.active.clone()?;
                Some((session.clone(), view.agent_id.clone(), active))
            })
            .collect();
        for (session, agent_id, active) in running {
            let ended = self.advance(&agent_id, &session, &active, report)?;
            if let Some(view) = self.sessions.get_mut(&session) {
                if ended {
                    view.active = None;
                } else if let Some(held) = view.active.as_mut() {
                    held.checkpoint = None;
                }
            }
        }
        Ok(())
    }

    /// Commit a pending checkpoint, or end an execution whose process is
    /// gone. Returns whether the execution ended.
    fn advance(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        active: &Active,
        report: &mut TickReport,
    ) -> Result<bool, StoreError> {
        let end = match active.reported_end {
            Some(end) => Some(end),
            None => match self.terminals.is_alive(&active.terminal, &active.workspace) {
                Ok(true) => None,
                Ok(false) => Some((EndCause::Lost, None)),
                Err(error) => {
                    report
                        .warnings
                        .push(format!("is_alive {}: {error}", active.terminal));
                    None
                }
            },
        };
        if let Some((cause, exit_code)) = end {
            return self.end(agent_id, session, active, cause, exit_code, report);
        }
        if let Some(reason) = active.checkpoint {
            self.checkpoint(agent_id, session, active, reason, report)?;
        }
        Ok(false)
    }

    /// Final checkpoint, kill, unmount, then `execution.ended`. Returns
    /// false when the terminal could not be killed; the execution then stays
    /// active and the next tick tries again.
    fn end(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        active: &Active,
        cause: EndCause,
        exit_code: Option<i32>,
        report: &mut TickReport,
    ) -> Result<bool, StoreError> {
        self.checkpoint(
            agent_id,
            session,
            active,
            CheckpointReason::ExecutionEnded,
            report,
        )?;
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

    fn checkpoint(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        active: &Active,
        reason: CheckpointReason,
        report: &mut TickReport,
    ) -> Result<(), StoreError> {
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
                    EventKind::ArtifactCommitted { reason, artifact },
                )?;
            }
            Err(WorkspaceError::Refused { code, message }) => {
                report
                    .warnings
                    .push(format!("commit refused {code}: {message}"));
            }
            Err(WorkspaceError::Unavailable(text)) => {
                report.warnings.push(format!("commit unavailable: {text}"));
            }
        }
        Ok(())
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

    fn handle(&mut self, claimed: &Claimed) -> Result<Outcome, StoreError> {
        let Ok(action) = claimed.action() else {
            return Ok(Outcome::Failed("stored action is not valid".to_string()));
        };
        match action {
            CommandAction::Start {
                engine,
                prompt,
                parent_session_id,
            } => self.start(claimed, engine, &prompt, parent_session_id.as_ref(), None),
            CommandAction::Input { text } => self.input(claimed, &text),
            CommandAction::Stop => self.stop(claimed),
            CommandAction::Resume { prompt } => self.resume(claimed, prompt.as_deref()),
        }
    }

    fn running(&self, session: &SessionId) -> Option<Active> {
        self.sessions.get(session)?.active.clone()
    }

    fn start(
        &mut self,
        claimed: &Claimed,
        engine: Engine,
        prompt: &str,
        parent: Option<&SessionId>,
        native: Option<&str>,
    ) -> Result<Outcome, StoreError> {
        if self.running(&claimed.session_id).is_some() {
            return Ok(Outcome::Failed(format!(
                "{} already runs an execution",
                claimed.session_id
            )));
        }
        let branch = BranchName::for_agent(&claimed.agent_id);
        if let Err(error) = self.workspaces.ensure_branch(&branch) {
            return Ok(Outcome::Failed(format!("ensure branch {branch}: {error}")));
        }
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
            reported_end: None,
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
            },
        )?;
        let launch = self.launch(
            claimed,
            &active,
            &Run {
                engine,
                native,
                prompt,
            },
        );
        if let Err(error) = self.terminals.start(&launch) {
            let mut report = TickReport::default();
            self.end(
                &claimed.agent_id,
                &claimed.session_id,
                &active,
                EndCause::Lost,
                None,
                &mut report,
            )?;
            report
                .warnings
                .insert(0, format!("start terminal: {error}"));
            return Ok(Outcome::Failed(report.warnings.join("; ")));
        }
        if engine == Engine::Claude && !prompt.is_empty() {
            return self.type_prompt(claimed, &active, prompt);
        }
        Ok(Outcome::Succeeded)
    }

    fn launch(&self, claimed: &Claimed, active: &Active, run: &Run<'_>) -> Launch {
        let Run {
            engine,
            native,
            prompt,
        } = *run;
        let mut arguments = vec!["run".to_string(), "--".to_string()];
        arguments.push(match engine {
            Engine::Claude => self.config.claude_program.clone(),
            Engine::Codex => self.config.codex_program.clone(),
        });
        arguments.extend(match engine {
            Engine::Claude => claude_arguments(&self.config.spawn_program, native),
            Engine::Codex => codex_arguments(native, prompt),
        });
        let mut environment = self.config.environment.clone();
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
        for attempt in 0..self.config.ready_attempts {
            if attempt > 0 {
                sleep(self.config.ready_interval);
            }
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
        match self
            .terminals
            .submit_text(&active.terminal, &active.workspace, text)
        {
            Ok(()) => Ok(Outcome::Succeeded),
            Err(error) => Ok(Outcome::Failed(format!("submit text: {error}"))),
        }
    }

    fn stop(&mut self, claimed: &Claimed) -> Result<Outcome, StoreError> {
        let Some(active) = self.running(&claimed.session_id) else {
            return Ok(Outcome::Succeeded);
        };
        let mut report = TickReport::default();
        let ended = self.end(
            &claimed.agent_id,
            &claimed.session_id,
            &active,
            EndCause::Stopped,
            None,
            &mut report,
        )?;
        self.catch_up()?;
        if ended {
            Ok(Outcome::Succeeded)
        } else {
            Ok(Outcome::Failed(report.warnings.join("; ")))
        }
    }

    fn resume(&mut self, claimed: &Claimed, prompt: Option<&str>) -> Result<Outcome, StoreError> {
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
        self.start(claimed, engine, prompt, None, Some(native.as_str()))
    }
}

/// Hook settings passed to Claude Code.
///
/// `--settings` takes inline JSON, and that JSON replaces the user's settings
/// file for the session. The permission keys must therefore be repeated here:
/// without them the engine stops on a startup dialog and never reads the
/// prompt the Bridge types into the pane.
///
/// The hook command is an absolute path, so a `PATH` change inside the sandbox
/// cannot break event capture.
pub fn claude_settings(spawn_program: &str) -> String {
    let command = format!("{} hook", shell_quote(spawn_program));
    let entry = json!([{ "hooks": [{ "type": "command", "command": command }] }]);
    let events: [&str; 7] = [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PostToolUse",
        "Notification",
        "Stop",
        "SessionEnd",
    ];
    let mut hooks = serde_json::Map::new();
    for event in events {
        hooks.insert(event.to_string(), entry.clone());
    }
    // `--settings` replaces the user settings file for this session, so the
    // first-run prompts have to be silenced here. A prompt in the pane blocks
    // the Bridge, which types the task prompt next and never answers it.
    json!({
        "permissions": { "defaultMode": "bypassPermissions" },
        "skipDangerousModePermissionPrompt": true,
        "hooks": hooks,
    })
    .to_string()
}

/// Claude arguments. The prompt is typed into the pane, never passed here.
pub fn claude_arguments(spawn_program: &str, native: Option<&str>) -> Vec<String> {
    // The engine runs in a container with no credentials of its own, so the
    // permission prompts have nowhere to go. The sandbox is the boundary.
    let mut arguments = vec![
        "--ax-screen-reader".to_string(),
        "--allow-dangerously-skip-permissions".to_string(),
        "--settings".to_string(),
        claude_settings(spawn_program),
    ];
    if let Some(native) = native {
        arguments.push("--resume".to_string());
        arguments.push(native.to_string());
    }
    arguments
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
