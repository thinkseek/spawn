//! The facts the Observer keeps for one session, folded from the log.

use std::collections::HashSet;

use super::SCREEN_STALE;
use super::judge::{JudgeInput, TurnFact};
use crate::event::{EndCause, Engine, EventKind, EventRecord, SessionSeq};
use crate::id::{AgentId, SessionId};
use crate::screen::ScreenState;
use crate::time::Timestamp;

/// Facts the Observer keeps for one session.
#[derive(Debug, Clone, Default)]
pub(super) struct Snapshot {
    engine: Option<Engine>,
    pub(super) execution_active: bool,
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
}

impl Snapshot {
    /// Apply one event.
    pub(super) fn apply(&mut self, record: &EventRecord) {
        self.evidence_seq = record.session_seq;
        self.last_fact_at = Some(record.recorded_at);
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
            EventKind::ExecutionStarted { engine, .. } => {
                self.engine = Some(*engine);
                self.execution_active = true;
                self.last_end = None;
                self.exit_code = None;
                self.open_tools.clear();
                self.permission_pending = false;
                self.screen = None;
                self.turn = None;
            }
            EventKind::ExecutionEnded { cause, exit_code } => {
                self.execution_active = false;
                self.last_end = Some(*cause);
                self.exit_code = *exit_code;
                self.open_tools.clear();
                // The pane is gone with the execution; nothing on it is a
                // current fact any more.
                self.screen = None;
                self.permission_pending = false;
            }
            EventKind::NativeSessionStarted { engine, .. } => {
                self.engine = Some(*engine);
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
            // The Bridge reads the pane as ready only right before it types
            // input. A command that then succeeds delivered that input: the
            // ready screen is consumed and a new turn begins, even though the
            // engine's own hook for it may land a moment later.
            EventKind::CommandFinished {
                succeeded: true, ..
            } if self.screen == Some(ScreenState::Ready) => {
                self.screen = None;
                self.turn = None;
            }
            _ => {}
        }
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
        JudgeInput {
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            engine: self.engine,
            execution_active: self.execution_active,
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
            quiet_ms: self
                .last_fact_at
                .map(|at| u64::try_from(now.since(at).as_millis()).unwrap_or(u64::MAX)),
            task: None,
            screen_tail: None,
            perpetual: false,
        }
    }
}
