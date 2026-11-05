//! The judge contract: what a judge is shown, what it answers, and the
//! deterministic stand-in used when no model is configured.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::event::{EndCause, SessionSeq};
use crate::observation::{Ambiguity, Confidence, SemanticState};
use crate::screen::ScreenState;

/// Judge failure.
#[derive(Debug, Error)]
pub enum JudgeError {
    /// The judge could not produce a judgement.
    #[error("judge unavailable: {0}")]
    Unavailable(String),
}

/// Facts one judgement uses. The shape is the judge's input contract.
///
/// It is stored as the evidence of every model judgement and read back for
/// replay, so every field defaults: a row written before a field existed
/// still replays.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct JudgeInput {
    /// Owning agent.
    pub agent_id: String,
    /// Judged session.
    pub session_id: String,
    /// Engine of the latest execution, as its record names it.
    pub engine: Option<String>,
    /// Whether an execution is running.
    pub execution_active: bool,
    /// The end under way whose final checkpoint keeps failing
    /// (`execution.end_stalled`): its terminal is down and its workspace
    /// stays mounted until the checkpoint commits or someone discards it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stalled_end: Option<EndCause>,
    /// How the latest execution ended.
    pub last_end: Option<EndCause>,
    /// Exit status of the latest execution, when its process reported one.
    pub exit_code: Option<i32>,
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
    /// Why the rules could not decide; set only for the model judge.
    pub ambiguity: Option<Ambiguity>,
    /// Milliseconds since the session's latest fact.
    pub quiet_ms: Option<u64>,
    /// The agent's task, as it was typed into the agent.
    pub task: Option<String>,
    /// Tail of the agent's terminal, as a person would see it.
    pub screen_tail: Option<String>,
    /// The session is meant never to end (the supervisor): it waits for
    /// instructions rather than completing.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub perpetual: bool,
}

/// How the latest turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    /// Distribution over [`SemanticState::ALL`], in that order, when the
    /// judge gives one.
    pub probabilities: Option<[f64; 5]>,
    /// Probability that a human must answer or decide.
    pub needs_human: Option<f64>,
    /// Probability that the agent loops or makes no progress.
    pub stuck: Option<f64>,
}

impl Judgement {
    /// A judgement with a state and a confidence only.
    pub fn of(state: SemanticState, confidence: Confidence) -> Self {
        Self {
            state,
            confidence,
            probabilities: None,
            needs_human: None,
            stuck: None,
        }
    }
}

/// Component that classifies a snapshot.
pub trait StateJudge: Send + Sync {
    /// Judge name stored in the observation.
    fn name(&self) -> &'static str;

    /// Classify one snapshot.
    fn judge(&self, input: &JudgeInput) -> Result<Judgement, JudgeError>;
}

/// Deterministic stand-in for the model judge.
///
/// It answers ambiguous cases when no Jev endpoint is configured, and keeps
/// the Observer testable without a model. It is not the rules tier: see
/// [`rules`](super::rules).
#[derive(Debug, Default, Clone, Copy)]
pub struct HeuristicJudge;

impl StateJudge for HeuristicJudge {
    fn name(&self) -> &'static str {
        "heuristic"
    }

    fn judge(&self, input: &JudgeInput) -> Result<Judgement, JudgeError> {
        let confidence = |value: f64| Confidence::new(value).unwrap_or(Confidence::ZERO);
        if input.permission_pending || input.screen == Some(ScreenState::Permission) {
            return Ok(Judgement::of(SemanticState::Blocked, confidence(0.9)));
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
            return Ok(Judgement::of(state, confidence(0.8)));
        }
        match input.last_end {
            Some(EndCause::Exited | EndCause::Stopped) => {
                Ok(Judgement::of(SemanticState::Completed, confidence(0.6)))
            }
            Some(EndCause::Lost) => Ok(Judgement::of(SemanticState::Unknown, confidence(0.3))),
            None => Ok(Judgement::of(SemanticState::Waiting, confidence(0.5))),
        }
    }
}
