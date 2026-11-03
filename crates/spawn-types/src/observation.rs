use serde::{Deserialize, Serialize};

use crate::error::ValidationError;
use crate::event::SessionSeq;

/// Semantic state attributed to a session by a judge.
///
/// Execution facts live in the event log. These values describe what the facts
/// mean and must never be confused with process liveness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SemanticState {
    /// The agent is actively making progress.
    Working,
    /// The agent waits for an external condition.
    Waiting,
    /// Progress is blocked and requires intervention.
    Blocked,
    /// The business task reached a terminal success state.
    Completed,
    /// The judge cannot currently classify the session.
    Unknown,
}

/// Structured reason for a waiting, blocked or unknown state.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WaitReason {
    /// A permission prompt is open.
    AwaitingUserPermission,
    /// The agent asks for user input.
    AwaitingUserInput,
    /// The last turn stopped without a new command.
    IdleTurnStop,
    /// The decision judge could not be reached or returned an error.
    JudgeUnavailable,
    /// The newest observation is older than the freshness policy.
    ObservationExpired,
    /// A reason that does not have a dedicated variant.
    Other {
        /// Stable machine-readable detail.
        detail: String,
    },
}

/// Calibrated probability in the closed interval `[0, 1]`.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Confidence(f64);

impl Confidence {
    /// Construct after range validation.
    pub fn new(value: f64) -> Result<Self, ValidationError> {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            return Err(ValidationError::OutOfRange {
                value,
                minimum: 0.0,
                maximum: 1.0,
            });
        }
        Ok(Self(value))
    }

    /// Return the numeric probability.
    #[must_use]
    pub fn value(self) -> f64 {
        self.0
    }
}

/// One semantic judgement produced by the observer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    /// Judged semantic state.
    pub state: SemanticState,
    /// Structured reason when the state is not plain working or completed.
    pub reason: Option<WaitReason>,
    /// Session sequence used as evidence.
    pub based_on_seq: SessionSeq,
    /// Judge confidence.
    pub confidence: Confidence,
    /// Name of the judge implementation that produced the record.
    pub judge_name: String,
}
