//! Semantic judgements produced by the Observer.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::event::SessionSeq;

/// Meaning of the facts of a session. This is not process liveness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SemanticState {
    /// The agent makes progress.
    Working,
    /// The agent waits for new input.
    Waiting,
    /// Progress needs human intervention.
    Blocked,
    /// The task reached a terminal state.
    Completed,
    /// The state cannot be classified now.
    Unknown,
}

impl SemanticState {
    /// All states in declaration order.
    pub const ALL: [Self; 5] = [
        Self::Working,
        Self::Waiting,
        Self::Blocked,
        Self::Completed,
        Self::Unknown,
    ];

    /// Wire key, for example `WORKING`.
    pub fn key(self) -> &'static str {
        match self {
            Self::Working => "WORKING",
            Self::Waiting => "WAITING",
            Self::Blocked => "BLOCKED",
            Self::Completed => "COMPLETED",
            Self::Unknown => "UNKNOWN",
        }
    }

    /// Parse a wire key.
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|state| state.key() == key)
    }
}

/// Structured reason attached to a judgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum WaitReason {
    /// A permission prompt is open.
    WaitingForUserPermission,
    /// The engine waits for the next prompt.
    WaitingForUserInput,
    /// The judge failed or timed out; the agent may still be alive.
    JudgeUnavailable,
}

/// A probability outside `[0, 1]` or not finite.
#[derive(Debug, Clone, Copy, PartialEq, Error)]
#[error("confidence {0} is outside [0, 1]")]
pub struct InvalidConfidence(pub f64);

/// Calibrated probability in `[0, 1]`.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub struct Confidence(f64);

impl Confidence {
    /// Zero confidence.
    pub const ZERO: Self = Self(0.0);

    /// Validate a probability.
    pub fn new(value: f64) -> Result<Self, InvalidConfidence> {
        if value.is_finite() && (0.0..=1.0).contains(&value) {
            Ok(Self(value))
        } else {
            Err(InvalidConfidence(value))
        }
    }

    /// Numeric probability.
    pub fn value(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for Confidence {
    type Error = InvalidConfidence;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Confidence> for f64 {
    fn from(value: Confidence) -> Self {
        value.0
    }
}

/// One judgement, stored as an `observation.recorded` event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    /// Judged state.
    pub state: SemanticState,
    /// Structured reason.
    pub reason: Option<WaitReason>,
    /// Latest evidence sequence the judgement used.
    pub based_on_seq: SessionSeq,
    /// Judge confidence.
    pub confidence: Confidence,
    /// Judge implementation name.
    pub judge: String,
}
