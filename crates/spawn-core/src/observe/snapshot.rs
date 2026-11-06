//! The facts the Observer keeps for one session, folded from the log.

use std::collections::HashSet;

use super::SCREEN_STALE;
use super::judge::{JudgeInput, TurnFact};
use crate::event::{CommandStep, EndCause, EventKind, EventRecord, SessionSeq, Source};
use crate::id::{AgentId, ExecutionId, SessionId, WorkspaceId};
use crate::screen::ScreenState;
use crate::time::Timestamp;

/// Facts the Observer keeps for one session.
#[derive(Debug, Clone, Default)]
pub(super) struct Snapshot {
    engine: Option<String>,
    /// The newest execution started.
    execution: Option<ExecutionId>,
    /// Its workspace.
    workspace: Option<WorkspaceId>,
    /// The session's transcript, as its engine reported it.
    transcript: Option<String>,
    pub(super) execution_active: bool,
    /// The current execution's end, whose final checkpoint keeps failing:
    /// its workspace stays mounted until someone acts.
    stalled_end: Option<EndCause>,
    last_end: Option<EndCause>,
    open_tools: HashSet<String>,
    last_tool: Option<String>,
    permission_pending: bool,
    screen: Option<ScreenState>,
    screen_at: Option<Timestamp>,
    turn: Option<TurnFact>,
    last_error: Option<String>,
    exit_code: Option<i32>,
    pub(super) evidence_seq: SessionSeq,
    last_fact_at: Option<Timestamp>,
    /// When the engine last moved: its latest transcript fact, or the
    /// execution's start. Screen readings and commands do not move it.
    progress_at: Option<Timestamp>,
    /// The engine's latest move, as a position: what the model judge's
    /// fingerprint counts as new evidence beyond the facts shown.
    pub(super) progress_seq: SessionSeq,
}

impl Snapshot {
    /// When the facts were last added to, and the engine last moved.
    fn note_times(&mut self, record: &EventRecord) {
        self.evidence_seq = record.session_seq;
        self.last_fact_at = Some(record.recorded_at);
        if record.event.source == Source::Claude
            || matches!(record.event.kind, EventKind::ExecutionStarted { .. })
        {
            self.progress_at = Some(record.recorded_at);
            self.progress_seq = record.session_seq;
        }
    }

    /// A record of the session that adds nothing to its facts (an
    /// observation claimed by another source): judgements made after it
    /// still cover it.
    pub(super) fn note_seq(&mut self, record: &EventRecord) {
        self.evidence_seq = self.evidence_seq.max(record.session_seq);
    }

    /// Apply one event.
    pub(super) fn apply(&mut self, record: &EventRecord) {
        self.note_times(record);
        // A hook event is newer than any screen reading and means the engine
        // moved on, so an older screen state or permission prompt is stale.
        if matches!(
            record.event.kind,
            EventKind::ToolStarted { .. }
                | EventKind::ToolCompleted { .. }
                | EventKind::ToolFailed { .. }
                | EventKind::TurnStopped { .. }
                | EventKind::TurnFailed { .. }
                | EventKind::MessageReceived
        ) {
            self.screen = None;
            self.permission_pending = false;
        }
        match &record.event.kind {
            EventKind::ExecutionStarted {
                engine, workspace, ..
            } => self.started(engine, workspace, record.event.execution_id.as_ref()),
            // A late end report of an older execution says nothing about
            // the current one.
            EventKind::ExecutionEnded { .. }
                if record.event.execution_id.is_some()
                    && self.execution.is_some()
                    && record.event.execution_id != self.execution => {}
            EventKind::ExecutionEnded {
                cause, exit_code, ..
            } => {
                self.execution_active = false;
                self.stalled_end = None;
                self.last_end = Some(*cause);
                self.exit_code = *exit_code;
                self.open_tools.clear();
                // The pane is gone with the execution; nothing on it is a
                // current fact any more.
                self.screen = None;
                self.permission_pending = false;
            }
            EventKind::ExecutionEndStalled { cause, .. }
                if record.event.execution_id.is_none()
                    || record.event.execution_id == self.execution =>
            {
                self.stalled_end = Some(*cause);
                // The Bridge killed the terminal: nothing on it is current.
                self.screen = None;
                self.permission_pending = false;
            }
            EventKind::NativeSessionStarted {
                engine,
                transcript_path,
                ..
            } => {
                self.engine = Some(engine.clone());
                if let Some(path) = transcript_path {
                    self.transcript = Some(path.clone());
                }
            }
            EventKind::ToolStarted {
                tool_name,
                tool_use_id,
            } => {
                self.last_tool = Some(tool_name.clone());
                self.open_tools.insert(tool_use_id.clone());
            }
            EventKind::ToolCompleted { tool_use_id, .. }
            | EventKind::ToolFailed { tool_use_id, .. } => {
                self.open_tools.remove(tool_use_id);
                if let EventKind::ToolFailed { error, .. } = &record.event.kind {
                    self.last_error = Some(error.clone());
                }
            }
            EventKind::TurnStopped { .. } => {
                self.turn = Some(TurnFact::Stopped);
                self.open_tools.clear();
            }
            EventKind::TurnFailed { error } => {
                self.turn = Some(TurnFact::Failed);
                self.last_error = Some(error.clone());
                self.open_tools.clear();
            }
            EventKind::NotificationReceived { notification_type } => {
                self.permission_pending = notification_type.contains("permission");
            }
            EventKind::ScreenObserved { state, .. } => {
                self.screen = Some(*state);
                self.screen_at = Some(record.recorded_at);
                if *state == ScreenState::Permission {
                    self.permission_pending = true;
                }
                if *state == ScreenState::Ready {
                    self.permission_pending = false;
                }
            }
            EventKind::MessageReceived => {
                self.turn = None;
            }
            // The Bridge reads the pane as ready right before it types
            // input, and records the typing (`typed`, once per command). A
            // typing that comes after the latest ready reading delivered
            // that input: the ready screen is consumed and a new turn
            // begins, even though the engine's own hook for it may land a
            // moment later. Not the command's `command.finished`: a retried
            // command whose text was typed before the turn ended finishes
            // after the turn's end and a ready screen, and starts nothing.
            EventKind::CommandStepped {
                step: CommandStep::Typed,
                ..
            } if self.screen == Some(ScreenState::Ready) => {
                self.screen = None;
                self.turn = None;
            }
            _ => {}
        }
    }

    /// A new execution: what the previous one showed is gone.
    fn started(&mut self, engine: &str, workspace: &WorkspaceId, execution: Option<&ExecutionId>) {
        self.engine = Some(engine.to_string());
        self.workspace = Some(workspace.clone());
        self.execution = execution.cloned();
        self.execution_active = true;
        self.stalled_end = None;
        self.last_end = None;
        self.exit_code = None;
        self.open_tools.clear();
        self.permission_pending = false;
        self.screen = None;
        self.turn = None;
    }

    /// The running execution's workspace and the session's transcript:
    /// where the model's latest reply is read.
    pub(super) fn transcript(&self) -> Option<(&WorkspaceId, &str)> {
        if !self.execution_active {
            return None;
        }
        Some((self.workspace.as_ref()?, self.transcript.as_deref()?))
    }

    /// The judge input these facts make at `now`, before any context is
    /// attached.
    pub(super) fn input(
        &self,
        agent_id: &AgentId,
        session_id: &SessionId,
        now: Timestamp,
    ) -> JudgeInput {
        let stale = self
            .screen_at
            .is_none_or(|at| now.since(at) >= SCREEN_STALE);
        let screen = match self.screen {
            Some(ScreenState::Busy) if stale => None,
            other => other,
        };
        let since = |at: Timestamp| u64::try_from(now.since(at).as_millis()).unwrap_or(u64::MAX);
        JudgeInput {
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            engine: self.engine.clone(),
            execution_active: self.execution_active,
            stalled_end: self.stalled_end,
            last_end: self.last_end,
            exit_code: self.exit_code,
            open_tools: self.open_tools.len(),
            last_tool: self.last_tool.clone(),
            permission_pending: self.permission_pending,
            screen,
            turn: self.turn,
            last_error: self.last_error.clone(),
            evidence_seq: self.evidence_seq,
            ambiguity: None,
            quiet_ms: self.last_fact_at.map(since),
            progress_ms: self.progress_at.map(since),
            task: None,
            screen_tail: None,
            reply: None,
            perpetual: false,
        }
    }
}
