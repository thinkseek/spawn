//! The judge contract: what a judge is shown and what it answers.

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
/// It is stored as the evidence of every model judgement, and every field
/// defaults, so a row written before a field existed still reads.
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
    /// The end under way whose final checkpoint (or the kill of its
    /// runner) keeps failing (`execution.end_stalled`): its workspace stays
    /// mounted until the end gets through or someone discards it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stalled_end: Option<EndCause>,
    /// How the latest execution ended.
    pub last_end: Option<EndCause>,
    /// Exit status of the latest execution, when its process reported one.
    pub exit_code: Option<i32>,
    /// Number of tool calls still open.
    pub open_tools: usize,
    /// Number of background jobs the running execution started that have
    /// not ended (`job.started` without its `job.ended`): work the engine
    /// wakes the agent for when it reports. Left out when none.
    #[serde(skip_serializing_if = "is_zero")]
    pub open_jobs: usize,
    /// Milliseconds since the oldest of those jobs started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_job_ms: Option<u64>,
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
    /// Milliseconds since the engine last moved: its latest transcript fact
    /// (a tool, a message, a turn), or the execution's start. Screen
    /// readings and commands are not progress: a spinner redrawn over a
    /// hung tool keeps refreshing the screen, never this.
    pub progress_ms: Option<u64>,
    /// The agent's task, as it was typed into the agent.
    pub task: Option<String>,
    /// Tail of the agent's terminal, as a person would see it.
    pub screen_tail: Option<String>,
    /// The model's latest reply: its own text since the last prompt, from the
    /// transcript. Where a question for a person is read (`HUMAN:`); the
    /// question itself is kept on the observation, so this stays out of the
    /// stored evidence and of what the judge is shown.
    #[serde(skip)]
    pub reply: Option<String>,
    /// The session is meant never to end (the supervisor): it waits for
    /// instructions rather than completing.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub perpetual: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde's skip_serializing_if passes a reference
fn is_zero(count: &usize) -> bool {
    *count == 0
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
