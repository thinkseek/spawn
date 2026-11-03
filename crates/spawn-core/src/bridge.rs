//! Agent Bridge: turns commands into executions and persists what happens.
//!
//! The Bridge owns four jobs: start and resume engines, record events, commit
//! workspace state, and map native identifiers to Spawn sessions. It does not
//! run the agent loop and does not judge state.

use std::collections::HashMap;
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;

use serde_json::json;

use crate::command::{Claimed, CommandAction, Outcome};
use crate::event::{
    ArtifactRef, CheckpointReason, EndCause, Engine, EventKind, GlobalPosition, NewEvent, Source,
};
use crate::id::{AgentId, BranchName, ExecutionId, SessionId, TerminalName, WorkspaceId};
use crate::port::{
    CommandQueue, Commit, EventLog, Launch, StoreError, TerminalHost, WorkspaceError,
    WorkspaceStore,
};
use crate::screen::{ScreenState, TAIL_LINES, read_claude_screen};
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
    /// argv prefix that puts a terminal command inside the workspace.
    pub wrapper: Vec<String>,
    /// Extra environment for the engine process.
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
            wrapper: Vec::new(),
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
    checkpoint: Option<CheckpointReason>,
}

/// What the Bridge knows about one session.
#[derive(Debug, Clone)]
struct SessionView {
    agent_id: AgentId,
    native: Option<(Engine, String)>,
    active: Option<Active>,
    ended: Option<EndCause>,
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

    fn fold(&mut self, record: &crate::event::EventRecord) {
        let event = &record.event;
        let view = self
            .sessions
            .entry(event.session_id.clone())
            .or_insert_with(|| SessionView {
                agent_id: event.agent_id.clone(),
                native: None,
                active: None,
                ended: None,
            });
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
                    });
                    view.ended = None;
                }
            }
            EventKind::ExecutionEnded { cause, .. } => {
                view.ended = Some(*cause);
                if let Some(active) = view.active.as_mut() {
                    active.checkpoint = Some(CheckpointReason::ExecutionEnded);
                }
            }
            EventKind::NativeSessionStarted {
                engine,
                native_session_id,
                ..
            } => {
                view.native = Some((*engine, native_session_id.clone()));
            }
            other => {
                if let Some(reason) = other.checkpoint_reason()
                    && let Some(active) = view.active.as_mut()
                {
                    let escalates = active
                        .checkpoint
                        .is_none_or(|held| held != CheckpointReason::ExecutionEnded);
                    if escalates {
                        active.checkpoint = Some(reason);
                    }
                }
            }
        }
    }

    fn act(&mut self, report: &mut TickReport) -> Result<(), StoreError> {
        let running: Vec<SessionId> = self
            .sessions
            .iter()
            .filter(|(_, view)| view.active.is_some())
            .map(|(session, _)| session.clone())
            .collect();
        for session in running {
            let Some(mut view) = self.sessions.remove(&session) else {
                continue;
            };
            let result = self.advance(&session, &mut view, report);
            self.sessions.insert(session, view);
            result?;
        }
        Ok(())
    }

    fn advance(
        &mut self,
        session: &SessionId,
        view: &mut SessionView,
        report: &mut TickReport,
    ) -> Result<(), StoreError> {
        let Some(active) = view.active.clone() else {
            return Ok(());
        };
        if view.ended.is_none()
            && let Ok(false) = self.terminals.is_alive(&active.terminal)
        {
            self.append(
                &view.agent_id,
                session,
                Some(&active.execution_id),
                Source::Bridge,
                Some(format!("{}:ended", active.execution_id)),
                EventKind::ExecutionEnded {
                    cause: EndCause::Lost,
                    exit_code: None,
                },
            )?;
            view.ended = Some(EndCause::Lost);
        }
        let reason = active.checkpoint.or(if view.ended.is_some() {
            Some(CheckpointReason::ExecutionEnded)
        } else {
            None
        });
        if let Some(reason) = reason {
            self.checkpoint(session, view, &active, reason, report)?;
        }
        if view.ended.is_some() {
            if let Err(error) = self.workspaces.unmount(&active.workspace) {
                report
                    .warnings
                    .push(format!("unmount {}: {error}", active.workspace));
            }
            view.active = None;
            view.ended = None;
        }
        Ok(())
    }

    fn checkpoint(
        &mut self,
        session: &SessionId,
        view: &mut SessionView,
        active: &Active,
        reason: CheckpointReason,
        report: &mut TickReport,
    ) -> Result<(), StoreError> {
        let message = format!("spawn checkpoint {}", active.execution_id);
        let outcome = self.workspaces.commit(&active.workspace, &message);
        match outcome {
            Ok(Commit::Unchanged) => {}
            Ok(Commit::Created(commit)) => {
                let artifact = ArtifactRef {
                    project: self.workspaces.project().to_string(),
                    branch: BranchName::for_agent(&view.agent_id),
                    commit,
                };
                self.append(
                    &view.agent_id,
                    session,
                    Some(&active.execution_id),
                    Source::Layerfs,
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
        if let Some(held) = view.active.as_mut() {
            held.checkpoint = None;
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
        let terminal = TerminalName::for_execution(&execution);
        self.append(
            &claimed.agent_id,
            &claimed.session_id,
            Some(&execution),
            Source::Bridge,
            Some(format!("{execution}:started")),
            EventKind::ExecutionStarted {
                engine,
                terminal: terminal.clone(),
                workspace: workspace.clone(),
                command_id: claimed.command_id,
            },
        )?;
        let launch = self.launch(
            claimed,
            &execution,
            &terminal,
            &workspace,
            &Run {
                engine,
                native,
                prompt,
            },
        );
        if let Err(error) = self.terminals.start(&launch) {
            self.append(
                &claimed.agent_id,
                &claimed.session_id,
                Some(&execution),
                Source::Bridge,
                Some(format!("{execution}:ended")),
                EventKind::ExecutionEnded {
                    cause: EndCause::Lost,
                    exit_code: None,
                },
            )?;
            return Ok(Outcome::Failed(format!("start terminal: {error}")));
        }
        if engine == Engine::Claude && !prompt.is_empty() {
            return self.type_prompt(claimed, &execution, &terminal, prompt);
        }
        Ok(Outcome::Succeeded)
    }

    fn launch(
        &self,
        claimed: &Claimed,
        execution: &ExecutionId,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
        run: &Run<'_>,
    ) -> Launch {
        let Run {
            engine,
            native,
            prompt,
        } = *run;
        let mut arguments = vec!["run".to_string(), "--".to_string()];
        arguments.extend(self.config.wrapper.iter().cloned());
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
        environment.push(("SPAWN_EXECUTION_ID".to_string(), execution.to_string()));
        environment.push((
            "SPAWN_ENGINE".to_string(),
            match engine {
                Engine::Claude => "claude".to_string(),
                Engine::Codex => "codex".to_string(),
            },
        ));
        Launch {
            terminal: terminal.clone(),
            workspace: workspace.clone(),
            program: self.config.spawn_program.clone(),
            arguments,
            environment,
        }
    }

    fn type_prompt(
        &mut self,
        claimed: &Claimed,
        execution: &ExecutionId,
        terminal: &TerminalName,
        prompt: &str,
    ) -> Result<Outcome, StoreError> {
        let mut last = ScreenState::Unknown;
        for attempt in 0..self.config.ready_attempts {
            if attempt > 0 {
                sleep(self.config.ready_interval);
            }
            let Ok(pane) = self.terminals.capture_tail(terminal, TAIL_LINES) else {
                return Ok(Outcome::Failed("terminal disappeared".to_string()));
            };
            let reading = read_claude_screen(&pane);
            last = reading.state;
            if !reading.state.accepts_input() {
                continue;
            }
            self.append(
                &claimed.agent_id,
                &claimed.session_id,
                Some(execution),
                Source::Bridge,
                Some(format!("{execution}:screen:ready")),
                EventKind::ScreenObserved {
                    state: reading.state,
                    evidence: reading.evidence,
                },
            )?;
            return match self.terminals.submit_text(terminal, prompt) {
                Ok(()) => Ok(Outcome::Succeeded),
                Err(error) => Ok(Outcome::Failed(format!("submit prompt: {error}"))),
            };
        }
        Ok(Outcome::Failed(format!(
            "pane never became ready, last state {last:?}"
        )))
    }

    fn input(&mut self, claimed: &Claimed, text: &str) -> Result<Outcome, StoreError> {
        let Some((execution, terminal, engine)) = self.running(&claimed.session_id) else {
            return Ok(Outcome::Failed(format!(
                "{} runs no execution",
                claimed.session_id
            )));
        };
        if engine != Engine::Claude {
            return Ok(Outcome::Failed(
                "only a Claude execution accepts typed input".to_string(),
            ));
        }
        let Ok(pane) = self.terminals.capture_tail(&terminal, TAIL_LINES) else {
            return Ok(Outcome::Failed("terminal disappeared".to_string()));
        };
        let reading = read_claude_screen(&pane);
        self.append(
            &claimed.agent_id,
            &claimed.session_id,
            Some(&execution),
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
        match self.terminals.submit_text(&terminal, text) {
            Ok(()) => Ok(Outcome::Succeeded),
            Err(error) => Ok(Outcome::Failed(format!("submit text: {error}"))),
        }
    }

    fn running(&self, session: &SessionId) -> Option<(ExecutionId, TerminalName, Engine)> {
        let view = self.sessions.get(session)?;
        view.active.as_ref().map(|active| {
            (
                active.execution_id.clone(),
                active.terminal.clone(),
                active.engine,
            )
        })
    }

    fn stop(&mut self, claimed: &Claimed) -> Result<Outcome, StoreError> {
        let Some((execution, terminal, _)) = self.running(&claimed.session_id) else {
            return Ok(Outcome::Succeeded);
        };
        let Some(mut view) = self.sessions.remove(&claimed.session_id) else {
            return Ok(Outcome::Failed("session vanished".to_string()));
        };
        let active = view.active.clone();
        let result = if let Some(active) = &active {
            let mut report = TickReport::default();
            self.checkpoint(
                &claimed.session_id,
                &mut view,
                active,
                CheckpointReason::ExecutionEnded,
                &mut report,
            )
        } else {
            Ok(())
        };
        self.sessions.insert(claimed.session_id.clone(), view);
        result?;
        if let Err(error) = self.terminals.terminate(&terminal) {
            return Ok(Outcome::Failed(format!("terminate: {error}")));
        }
        self.append(
            &claimed.agent_id,
            &claimed.session_id,
            Some(&execution),
            Source::Bridge,
            Some(format!("{execution}:ended")),
            EventKind::ExecutionEnded {
                cause: EndCause::Stopped,
                exit_code: None,
            },
        )?;
        self.catch_up()?;
        Ok(Outcome::Succeeded)
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
/// `--settings` takes inline JSON. The hook command is an absolute path, so a
/// `PATH` change inside the sandbox cannot break event capture.
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
    json!({ "hooks": hooks }).to_string()
}

/// Claude arguments. The prompt is typed into the pane, never passed here.
pub fn claude_arguments(spawn_program: &str, native: Option<&str>) -> Vec<String> {
    let mut arguments = vec![
        "--ax-screen-reader".to_string(),
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
