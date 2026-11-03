//! Semantic judgements produced by the Observer.

use std::collections::BTreeMap;

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

/// Why the rules could not decide a state from the facts alone.
///
/// Only an ambiguous case reaches the model judge. Each kind limits the
/// states the judge may choose: the facts already rule the others out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ambiguity {
    /// The turn stopped at an idle prompt: finished, asking a human, or
    /// waiting for the next prompt.
    TurnStoppedIdle,
    /// The execution runs with no open tool and no event for a while:
    /// thinking, waiting, or hung.
    QuietLive,
    /// The latest turn failed or the engine printed an error: recovering or
    /// stuck.
    EngineError,
    /// The engine exited on its own: done, or failed.
    ExitedOnItsOwn,
    /// The terminal disappeared without a recorded exit.
    Lost,
}

impl Ambiguity {
    /// States the facts still allow.
    pub fn allowed(self) -> &'static [SemanticState] {
        match self {
            Self::TurnStoppedIdle => &[
                SemanticState::Waiting,
                SemanticState::Blocked,
                SemanticState::Completed,
            ],
            Self::QuietLive | Self::EngineError => &[
                SemanticState::Working,
                SemanticState::Waiting,
                SemanticState::Blocked,
            ],
            Self::ExitedOnItsOwn => &[SemanticState::Completed, SemanticState::Blocked],
            Self::Lost => &[
                SemanticState::Unknown,
                SemanticState::Blocked,
                SemanticState::Completed,
            ],
        }
    }
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
    /// Who decided: `rules`, or the model judge's name (`jev`, `heuristic`).
    pub judge: String,
    /// Why the rules could not decide. `None` when the rules decided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ambiguity: Option<Ambiguity>,
    /// The rules' best guess for an ambiguous case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules_state: Option<SemanticState>,
    /// The model judge's distribution over states, before the facts narrowed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probabilities: Option<BTreeMap<String, f64>>,
    /// Probability that a human must answer or decide before work continues.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs_human: Option<f64>,
    /// Probability that the agent loops or makes no progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stuck: Option<f64>,
    /// Model judge round trip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    /// The observation layer's flag that a person or a higher tier should
    /// look. Acting on it is the control layer's decision, not this one's.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub attention: bool,
    /// The input the model judge saw, for audit and replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<serde_json::Value>,
    /// Why `attention` is set, in one word: `permission`, `blocked`,
    /// `unknown`, `needs_human`, `stuck`, `risky`, `disagreement`,
    /// `low_confidence`, `judge_unavailable`, `failing_exit`, or
    /// `human_question`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention_reason: Option<String>,
    /// The question for a person the agent's screen ends with (a line that
    /// starts with `HUMAN:` after the last input). A rule over evidence: it
    /// asks for attention whatever the model says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_question: Option<String>,
    /// Stable digest of that input. A restarted Observer does not ask the
    /// model again about evidence it already judged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

impl Observation {
    /// An observation with only the original fields set.
    pub fn basic(
        state: SemanticState,
        reason: Option<WaitReason>,
        based_on_seq: SessionSeq,
        confidence: Confidence,
        judge: &str,
    ) -> Self {
        Self {
            state,
            reason,
            based_on_seq,
            confidence,
            judge: judge.to_string(),
            ambiguity: None,
            rules_state: None,
            probabilities: None,
            needs_human: None,
            stuck: None,
            latency_ms: None,
            attention: false,
            evidence: None,
            attention_reason: None,
            human_question: None,
            fingerprint: None,
        }
    }
}
