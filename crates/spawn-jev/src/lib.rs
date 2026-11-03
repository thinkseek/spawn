//! Jev System One judge.
//!
//! The endpoint takes one `state` value and a map of typed questions, and
//! answers every question with a label or a probability. Spawn asks one
//! `choice` question per session, named `semantic_state`.
//!
//! The request and answer shapes were read from the running service, not
//! guessed: `{"state": …, "questions": {"<name>": {"type": "choice",
//! "instructions": …, "criteria": {"KEY": "description"}}}}` answers with
//! `{"answers": {"<name>": {"choice": "KEY", "confidence": 0.0..1.0}}}`.
//!
//! The criteria object is written in declaration order. The service renders
//! each criteria entry as a labelled option, so order decides the prompt.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde::Deserialize;
use spawn_core::observation::{Confidence, SemanticState};
use spawn_core::observe::{JudgeError, JudgeInput, Judgement, StateJudge};

/// Question name used in every request.
pub const QUESTION: &str = "semantic_state";

/// Instruction shown to the model.
const INSTRUCTIONS: &str = "What is the semantic state of this coding agent session? \
Use the evidence in state: an open tool or a running pane means progress, \
a permission prompt means a human must answer, and a stopped turn means the \
agent waits for the next prompt.";

/// Which state means what. Order is the wire order.
fn description(state: SemanticState) -> &'static str {
    match state {
        SemanticState::Working => "the agent is running a tool or a model turn",
        SemanticState::Waiting => "the agent stopped and waits for the next prompt",
        SemanticState::Blocked => "the agent waits for a permission answer from a human",
        SemanticState::Completed => "the task reached a terminal state",
        SemanticState::Unknown => "the evidence is not enough to decide",
    }
}

/// Endpoint of the decision service.
pub fn endpoint(base_url: &str) -> String {
    format!("{}/v1/systemone", base_url.trim_end_matches('/'))
}

/// Request body for one snapshot.
pub fn build_request(input: &JudgeInput) -> String {
    let state = serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string());
    let mut criteria = String::from("{");
    for (index, candidate) in SemanticState::ALL.iter().enumerate() {
        if index > 0 {
            criteria.push(',');
        }
        let key = serde_json::to_string(candidate.key()).unwrap_or_default();
        let text = serde_json::to_string(description(*candidate)).unwrap_or_default();
        criteria.push_str(&key);
        criteria.push(':');
        criteria.push_str(&text);
    }
    criteria.push('}');
    let instructions = serde_json::to_string(INSTRUCTIONS).unwrap_or_default();
    format!(
        "{{\"state\":{state},\"questions\":{{\"{QUESTION}\":{{\"type\":\"choice\",\"instructions\":{instructions},\"criteria\":{criteria}}}}}}}"
    )
}

#[derive(Debug, Deserialize)]
struct Reply {
    answers: BTreeMap<String, Answer>,
}

#[derive(Debug, Deserialize)]
struct Answer {
    #[serde(default)]
    choice: Option<String>,
    #[serde(default)]
    confidence: Option<f64>,
    #[serde(default)]
    probabilities: BTreeMap<String, f64>,
}

/// Parse one answer body.
pub fn parse_reply(body: &str) -> Result<Judgement, JudgeError> {
    let reply: Reply = serde_json::from_str(body)
        .map_err(|error| JudgeError::Unavailable(format!("answer is not JSON: {error}")))?;
    let answer = reply
        .answers
        .get(QUESTION)
        .ok_or_else(|| JudgeError::Unavailable(format!("answer lacks {QUESTION}")))?;
    let key = match answer.choice.as_deref() {
        Some(key) => key.to_string(),
        None => answer
            .probabilities
            .iter()
            .max_by(|left, right| left.1.total_cmp(right.1))
            .map(|(key, _)| key.clone())
            .ok_or_else(|| JudgeError::Unavailable("answer has no choice".to_string()))?,
    };
    let state = SemanticState::from_key(&key)
        .ok_or_else(|| JudgeError::Unavailable(format!("unknown state {key}")))?;
    let confidence = answer.confidence.map_or(Confidence::ZERO, |value| {
        Confidence::new(value).unwrap_or(Confidence::ZERO)
    });
    Ok(Judgement { state, confidence })
}

/// Settings of the decision service.
#[derive(Debug, Clone)]
pub struct JevConfig {
    /// Base URL, without a trailing slash.
    pub base_url: String,
    /// Whole request timeout.
    pub timeout: Duration,
}

impl JevConfig {
    /// Configuration for one base URL.
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            timeout: Duration::from_secs(10),
        }
    }
}

/// Judge that asks Jev over HTTP.
#[derive(Debug, Clone)]
pub struct JevJudge {
    config: JevConfig,
}

impl JevJudge {
    /// Judge for one configuration.
    pub fn new(config: JevConfig) -> Self {
        Self { config }
    }
}

impl StateJudge for JevJudge {
    fn name(&self) -> &'static str {
        "jev"
    }

    fn judge(&self, input: &JudgeInput) -> Result<Judgement, JudgeError> {
        let body = build_request(input);
        let text = post_json(&endpoint(&self.config.base_url), &body, self.config.timeout)
            .map_err(JudgeError::Unavailable)?;
        parse_reply(&text)
    }
}

fn split_url(url: &str) -> Result<(String, u16, String), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("only http:// endpoints are supported, not {url}"))?;
    let (authority, path) = rest
        .find('/')
        .map_or((rest, "/"), |index| (&rest[..index], &rest[index..]));
    let (host, port) = authority
        .rsplit_once(':')
        .map_or((authority, 80), |(host, text)| {
            (host, text.parse::<u16>().unwrap_or(80))
        });
    if host.is_empty() {
        return Err(format!("no host in {url}"));
    }
    Ok((host.to_string(), port, path.to_string()))
}

/// `Transfer-Encoding: chunked` body to plain text.
fn dechunk(body: &str) -> Result<String, String> {
    let mut output = String::new();
    let mut rest = body;
    loop {
        let (size_line, tail) = rest
            .split_once("\r\n")
            .ok_or_else(|| "chunked body ends early".to_string())?;
        let size = usize::from_str_radix(size_line.trim(), 16)
            .map_err(|_| format!("bad chunk size {size_line:?}"))?;
        if size == 0 {
            return Ok(output);
        }
        let (chunk, after) = tail
            .split_at_checked(size)
            .ok_or_else(|| "chunk is truncated".to_string())?;
        output.push_str(chunk);
        rest = after
            .strip_prefix("\r\n")
            .ok_or_else(|| "chunk misses its terminator".to_string())?;
    }
}

fn is_chunked(head: &str) -> bool {
    head.lines().any(|line| {
        let lower = line.to_ascii_lowercase();
        lower.starts_with("transfer-encoding:") && lower.contains("chunked")
    })
}

/// Minimal HTTP/1.1 POST over a plain socket.
///
/// The decision service runs inside the cluster and speaks plain HTTP. A
/// full client would add a TLS stack to the build for no gain.
fn post_json(url: &str, body: &str, timeout: Duration) -> Result<String, String> {
    let (host, port, path) = split_url(url)?;
    let address = format!("{host}:{port}");
    let stream = TcpStream::connect(address.as_str())
        .map_err(|error| format!("connect {address}: {error}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| format!("set read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| format!("set write timeout: {error}"))?;

    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut writer = stream
        .try_clone()
        .map_err(|error| format!("clone socket: {error}"))?;
    writer
        .write_all(request.as_bytes())
        .map_err(|error| format!("write request: {error}"))?;
    writer.flush().map_err(|error| format!("flush: {error}"))?;

    let mut raw = Vec::new();
    let mut reader = stream;
    reader
        .read_to_end(&mut raw)
        .map_err(|error| format!("read response: {error}"))?;
    let text = String::from_utf8_lossy(&raw);
    let (head, payload) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| "response has no header block".to_string())?;
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| "response has no status".to_string())?;
    let body_text = if is_chunked(head) {
        dechunk(payload)?
    } else {
        payload.to_string()
    };
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status}: {}", body_text.trim()));
    }
    Ok(body_text)
}
