//! Observer: turns stored facts into semantic state.
//!
//! The Observer never decides whether to wake an agent. It reads the event
//! stream, keeps one snapshot per session, asks a [`StateJudge`] for a
//! judgement, and appends an `observation.recorded` event when the judgement
//! changes. A judge failure produces `UNKNOWN`, never "dead".

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde::Serialize;
use thiserror::Error;

use crate::event::{
    EndCause, Engine, EventKind, EventRecord, GlobalPosition, NewEvent, SessionSeq, Source,
};
use crate::id::{AgentId, SessionId};
use crate::observation::{Confidence, Observation, SemanticState, WaitReason};
use crate::port::{EventLog, StoreError};
use crate::screen::ScreenState;

/// Judge failure.
#[derive(Debug, Error)]
pub enum JudgeError {
    /// The judge could not produce a judgement.
    #[error("judge unavailable: {0}")]
    Unavailable(String),
}

/// Facts one judgement uses. The shape is the judge's input contract.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct JudgeInput {
    /// Owning agent.
    pub agent_id: String,
    /// Judged session.
    pub session_id: String,
    /// Engine of the latest execution.
    pub engine: Option<Engine>,
    /// Whether an execution is running.
    pub execution_active: bool,
    /// How the latest execution ended.
    pub last_end: Option<EndCause>,
    /// Number of tool calls still open.
    pub open_tools: usize,
    /// Tool that started last.
    pub last_tool: Option<String>,
    /// Whether a permission dialog is open.
    pub permission_pending: bool,
    /// Latest state read from the engine pane.
    pub screen: Option<ScreenState>,
    /// State of the latest turn.
    pub turn: Option<TurnFact>,
    /// Latest engine error, truncated.
    pub last_error: Option<String>,
    /// Evidence sequence of the snapshot.
    pub evidence_seq: SessionSeq,
}

/// How the latest turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnFact {
    /// The model finished a turn.
    Stopped,
    /// The turn failed.
    Failed,
}

/// One judgement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Judgement {
    /// Judged state.
    pub state: SemanticState,
    /// Judge confidence.
    pub confidence: Confidence,
}

/// Component that classifies a snapshot.
pub trait StateJudge: Send + Sync {
    /// Judge name stored in the observation.
    fn name(&self) -> &'static str;

    /// Classify one snapshot.
    fn judge(&self, input: &JudgeInput) -> Result<Judgement, JudgeError>;
}

/// Deterministic rules over the visible facts.
///
/// It is the fallback when no Jev endpoint is configured, and it keeps the
/// Observer testable without a model.
#[derive(Debug, Default, Clone, Copy)]
pub struct HeuristicJudge;

impl StateJudge for HeuristicJudge {
    fn name(&self) -> &'static str {
        "heuristic"
    }

    fn judge(&self, input: &JudgeInput) -> Result<Judgement, JudgeError> {
        let confidence = |value: f64| Confidence::new(value).unwrap_or(Confidence::ZERO);
        if input.permission_pending || input.screen == Some(ScreenState::Permission) {
            return Ok(Judgement {
                state: SemanticState::Blocked,
                confidence: confidence(0.9),
            });
        }
        if input.execution_active {
            let state = if input.open_tools > 0 || input.screen == Some(ScreenState::Busy) {
                SemanticState::Working
            } else if input.turn == Some(TurnFact::Stopped)
                || input.screen == Some(ScreenState::Ready)
            {
                SemanticState::Waiting
            } else {
                SemanticState::Working
            };
            return Ok(Judgement {
                state,
                confidence: confidence(0.8),
            });
        }
        match input.last_end {
            Some(EndCause::Exited | EndCause::Stopped) => Ok(Judgement {
                state: SemanticState::Completed,
                confidence: confidence(0.6),
            }),
            Some(EndCause::Lost) => Ok(Judgement {
                state: SemanticState::Unknown,
                confidence: confidence(0.3),
            }),
            None => Ok(Judgement {
                state: SemanticState::Waiting,
                confidence: confidence(0.5),
            }),
        }
    }
}

/// Facts the Observer keeps for one session.
#[derive(Debug, Clone, Default)]
struct Snapshot {
    engine: Option<Engine>,
    execution_active: bool,
    last_end: Option<EndCause>,
    open_tools: HashSet<String>,
    last_tool: Option<String>,
    permission_pending: bool,
    screen: Option<ScreenState>,
    turn: Option<TurnFact>,
    last_error: Option<String>,
    evidence_seq: SessionSeq,
}

impl Snapshot {
    /// Apply one event.
    fn apply(&mut self, record: &EventRecord) {
        self.evidence_seq = record.session_seq;
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
                self.open_tools.clear();
                self.permission_pending = false;
                self.screen = None;
                self.turn = None;
            }
            EventKind::ExecutionEnded { cause, .. } => {
                self.execution_active = false;
                self.last_end = Some(*cause);
                self.open_tools.clear();
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
            _ => {}
        }
    }

    fn input(&self, agent_id: &AgentId, session_id: &SessionId) -> JudgeInput {
        JudgeInput {
            agent_id: agent_id.to_string(),
            session_id: session_id.to_string(),
            engine: self.engine,
            execution_active: self.execution_active,
            last_end: self.last_end,
            open_tools: self.open_tools.len(),
            last_tool: self.last_tool.clone(),
            permission_pending: self.permission_pending,
            screen: self.screen,
            turn: self.turn,
            last_error: self.last_error.clone(),
            evidence_seq: self.evidence_seq,
        }
    }
}

/// Per-session observation state.
#[derive(Debug)]
struct Tracked {
    agent_id: AgentId,
    snapshot: Snapshot,
    last: Option<(SemanticState, Option<WaitReason>)>,
    judged_seq: SessionSeq,
    retry: bool,
}

/// 1Hz state observer.
pub struct Observer {
    log: Arc<dyn EventLog>,
    judge: Arc<dyn StateJudge>,
    cursor: GlobalPosition,
    sessions: HashMap<SessionId, Tracked>,
}

impl Observer {
    /// Observer over one log and one judge.
    pub fn new(log: Arc<dyn EventLog>, judge: Arc<dyn StateJudge>) -> Self {
        Self {
            log,
            judge,
            cursor: GlobalPosition(0),
            sessions: HashMap::new(),
        }
    }

    /// Sessions the Observer tracks.
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Rebuild snapshots from the whole log.
    pub fn recover(&mut self) -> Result<(), StoreError> {
        self.cursor = GlobalPosition(0);
        self.sessions.clear();
        self.catch_up()
    }

    /// Read new events and judge changed sessions.
    ///
    /// Returns the number of observations written.
    pub fn tick(&mut self) -> Result<usize, StoreError> {
        self.catch_up()?;
        let pending: Vec<SessionId> = self
            .sessions
            .iter()
            .filter(|(_, tracked)| {
                tracked.retry || tracked.snapshot.evidence_seq > tracked.judged_seq
            })
            .map(|(session, _)| session.clone())
            .collect();
        let mut written = 0;
        for session in pending {
            written += self.observe(&session)?;
        }
        Ok(written)
    }

    fn catch_up(&mut self) -> Result<(), StoreError> {
        const BATCH: usize = 200;
        loop {
            let batch = self.log.read_global(self.cursor, BATCH)?;
            if batch.is_empty() {
                return Ok(());
            }
            for record in &batch {
                let tracked = self
                    .sessions
                    .entry(record.event.session_id.clone())
                    .or_insert_with(|| Tracked {
                        agent_id: record.event.agent_id.clone(),
                        snapshot: Snapshot::default(),
                        last: None,
                        judged_seq: SessionSeq(0),
                        retry: false,
                    });
                // The Observer's own output restores what it last wrote, so a
                // new process does not write the same observation again.
                if let EventKind::ObservationRecorded { observation } = &record.event.kind {
                    tracked.last = Some((observation.state, observation.reason));
                    tracked.judged_seq = observation.based_on_seq;
                    tracked.retry = observation.reason == Some(WaitReason::JudgeUnavailable);
                } else if record.event.source != Source::Observer {
                    tracked.snapshot.apply(record);
                }
            }
            if let Some(last) = batch.last() {
                self.cursor = last.position;
            }
            if batch.len() < BATCH {
                return Ok(());
            }
        }
    }

    fn observe(&mut self, session: &SessionId) -> Result<usize, StoreError> {
        let Some(tracked) = self.sessions.get(session) else {
            return Ok(0);
        };
        let input = tracked.snapshot.input(&tracked.agent_id, session);
        let agent = tracked.agent_id.clone();
        let seq = tracked.snapshot.evidence_seq;
        if let Ok(judgement) = self.judge.judge(&input) {
            let reason = reason_for(judgement.state, &input);
            let key = (judgement.state, reason);
            let changed = tracked.last != Some(key);
            let observation = Observation {
                state: judgement.state,
                reason,
                based_on_seq: seq,
                confidence: judgement.confidence,
                judge: self.judge.name().to_string(),
            };
            let written = if changed {
                self.append_observation(&agent, session, observation)?;
                1
            } else {
                0
            };
            let Some(tracked) = self.sessions.get_mut(session) else {
                return Ok(written);
            };
            tracked.last = Some(key);
            tracked.judged_seq = seq;
            tracked.retry = false;
            return Ok(written);
        }

        let key = (SemanticState::Unknown, Some(WaitReason::JudgeUnavailable));
        let changed = tracked.last != Some(key);
        let observation = Observation {
            state: SemanticState::Unknown,
            reason: Some(WaitReason::JudgeUnavailable),
            based_on_seq: seq,
            confidence: Confidence::ZERO,
            judge: self.judge.name().to_string(),
        };
        let written = if changed {
            self.append_observation(&agent, session, observation)?;
            1
        } else {
            0
        };
        let Some(tracked) = self.sessions.get_mut(session) else {
            return Ok(written);
        };
        tracked.last = Some(key);
        tracked.retry = true;
        Ok(written)
    }

    fn append_observation(
        &self,
        agent_id: &AgentId,
        session: &SessionId,
        observation: Observation,
    ) -> Result<(), StoreError> {
        self.log.append(NewEvent {
            agent_id: agent_id.clone(),
            session_id: session.clone(),
            execution_id: None,
            source: Source::Observer,
            source_event_id: None,
            kind: EventKind::ObservationRecorded { observation },
        })?;
        Ok(())
    }
}

fn reason_for(state: SemanticState, input: &JudgeInput) -> Option<WaitReason> {
    match state {
        SemanticState::Blocked if input.permission_pending => {
            Some(WaitReason::WaitingForUserPermission)
        }
        SemanticState::Waiting => Some(WaitReason::WaitingForUserInput),
        _ => None,
    }
}
