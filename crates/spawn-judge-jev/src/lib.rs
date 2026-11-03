//! HTTP client for System One compatible decision services.
//!
//! The same request/response contract is served by TypeSafe Jev and by a
//! self-hosted StartLux decision service. Only the endpoint differs.

// Pedantic doc-bookkeeping lints: public items carry purposeful docs, but
// mandatory Errors/Panics sections and identifier backticks add noise.
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::doc_markdown
)]
#![allow(clippy::module_name_repetitions, clippy::must_use_candidate)]
use std::time::Duration;

use serde::{Deserialize, Serialize, Serializer};

use spawn_observer::{Judgement, SessionSnapshot, StateJudge};
use spawn_types::clock::Timestamp;
use spawn_types::observation::{Confidence, SemanticState, WaitReason};

const STATE_QUESTION_ID: &str = "semantic_state";

/// Ordered option map for a choice question.
///
/// System One identifies choice options by the keys of a JSON object
/// `{option: description}`. A plain array of pairs is read by the service as
/// option *names*, so the pair would be stringified into one key. This type
/// keeps the declared option order while serializing to the required object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoiceCriteria(pub Vec<(String, String)>);

impl Serialize for ChoiceCriteria {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (option, description) in &self.0 {
            map.serialize_entry(option, description)?;
        }
        map.end()
    }
}

/// One typed question in the System One contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SystemOneQuestion {
    /// Choose one listed option.
    Choice {
        /// Question text.
        instructions: String,
        /// Option key to description map, emitted as a JSON object.
        criteria: ChoiceCriteria,
    },
}

/// Request body for `POST /v1/systemone`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SystemOneRequest {
    /// Free-form state evidence.
    pub state: serde_json::Value,
    /// Optional model override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Typed questions keyed by caller id.
    pub questions: std::collections::HashMap<String, SystemOneQuestion>,
}

/// One answer returned by the decision service.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ChoiceAnswer {
    /// Always `choice` for this struct.
    #[serde(rename = "type")]
    pub answer_type: String,
    /// Selected option key.
    pub choice: String,
    /// Reported confidence.
    pub confidence: f64,
    /// Probability for every option.
    #[serde(default)]
    pub probabilities: std::collections::HashMap<String, f64>,
}

/// Response body for `POST /v1/systemone`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SystemOneResponse {
    /// Model name echoed by the service.
    #[serde(default)]
    pub model: String,
    /// Answers keyed by question id.
    pub answers: std::collections::HashMap<String, ChoiceAnswer>,
}

/// Configuration for [`JevJudge`].
#[derive(Debug, Clone)]
pub struct JevConfig {
    /// Base URL without trailing slash, for example `http://host:8090`.
    pub base_url: String,
    /// Model name when the service requires one.
    pub model: Option<String>,
    /// Optional bearer token.
    pub bearer_token: Option<String>,
    /// Request timeout.
    pub timeout: Duration,
    /// Stable judge name recorded in observations.
    pub judge_name: String,
}

impl JevConfig {
    /// Create config with sensible defaults for an internal service.
    #[must_use]
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            model: None,
            bearer_token: None,
            timeout: Duration::from_secs(5),
            judge_name: "jev".to_string(),
        }
    }
}

/// HTTP-backed [`StateJudge`].
pub struct JevJudge {
    config: JevConfig,
    agent: ureq::Agent,
}

/// Stable wire key expected by the decision question.
#[must_use]
pub fn state_wire_key(state: SemanticState) -> &'static str {
    match state {
        SemanticState::Working => "WORKING",
        SemanticState::Waiting => "WAITING",
        SemanticState::Blocked => "BLOCKED",
        SemanticState::Completed => "COMPLETED",
        SemanticState::Unknown => "UNKNOWN",
    }
}

/// Parse a wire key returned by the service.
#[must_use]
pub fn state_from_wire_key(value: &str) -> Option<SemanticState> {
    match value {
        "WORKING" => Some(SemanticState::Working),
        "WAITING" => Some(SemanticState::Waiting),
        "BLOCKED" => Some(SemanticState::Blocked),
        "COMPLETED" => Some(SemanticState::Completed),
        "UNKNOWN" => Some(SemanticState::Unknown),
        _ => None,
    }
}

impl JevJudge {
    /// Construct a client from config.
    #[must_use]
    pub fn new(config: JevConfig) -> Self {
        let agent_config = ureq::Agent::config_builder()
            .timeout_connect(Some(config.timeout))
            .timeout_recv_response(Some(config.timeout))
            .build();
        let agent = ureq::Agent::new_with_config(agent_config);
        Self { config, agent }
    }

    fn build_request(snapshot: &SessionSnapshot, model: Option<String>) -> SystemOneRequest {
        let criteria_rows: Vec<(String, String)> = vec![
            (
                state_wire_key(SemanticState::Working).to_string(),
                "The agent is actively making progress.".to_string(),
            ),
            (
                state_wire_key(SemanticState::Waiting).to_string(),
                "The agent waits for an external condition or new input.".to_string(),
            ),
            (
                state_wire_key(SemanticState::Blocked).to_string(),
                "Progress needs human intervention or permission.".to_string(),
            ),
            (
                state_wire_key(SemanticState::Completed).to_string(),
                "The task reached a terminal success state.".to_string(),
            ),
            (
                state_wire_key(SemanticState::Unknown).to_string(),
                "The evidence is insufficient for a classification.".to_string(),
            ),
        ];
        let state = serde_json::to_value(snapshot).unwrap_or(serde_json::Value::Null);
        let mut questions = std::collections::HashMap::new();
        questions.insert(
            STATE_QUESTION_ID.to_string(),
            SystemOneQuestion::Choice {
                instructions: "Classify the current agent session state.".to_string(),
                criteria: ChoiceCriteria(criteria_rows),
            },
        );
        SystemOneRequest {
            state,
            model,
            questions,
        }
    }

    fn map_state(choice: &str, confidence: f64) -> Result<Judgement, String> {
        let state =
            state_from_wire_key(choice).ok_or_else(|| format!("unknown state choice {choice}"))?;
        let confidence = Confidence::new(confidence).map_err(|error| error.to_string())?;
        let reason = match state {
            SemanticState::Blocked => Some(WaitReason::AwaitingUserPermission),
            SemanticState::Waiting => Some(WaitReason::IdleTurnStop),
            SemanticState::Unknown => Some(WaitReason::JudgeUnavailable),
            SemanticState::Working | SemanticState::Completed => None,
        };
        Ok(Judgement {
            state,
            reason,
            confidence,
        })
    }
}

impl StateJudge for JevJudge {
    fn judge(&self, snapshot: &SessionSnapshot, _now: Timestamp) -> Result<Judgement, String> {
        let request = Self::build_request(snapshot, self.config.model.clone());
        let url = format!("{}/v1/systemone", self.config.base_url);
        let mut post = self
            .agent
            .post(&url)
            .header("Content-Type", "application/json");
        if let Some(token) = &self.config.bearer_token {
            post = post.header("Authorization", format!("Bearer {token}"));
        }
        let response: SystemOneResponse = post
            .send_json(&request)
            .map_err(|error| format!("decision service call failed: {error}"))?
            .body_mut()
            .read_json()
            .map_err(|error| format!("invalid decision response: {error}"))?;
        let answer = response
            .answers
            .get(STATE_QUESTION_ID)
            .ok_or_else(|| "decision response missing semantic_state".to_string())?;
        Self::map_state(&answer.choice, answer.confidence)
    }

    fn name(&self) -> &str {
        &self.config.judge_name
    }
}
