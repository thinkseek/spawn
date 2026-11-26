//! Jev System One judge.
//!
//! The endpoint takes one `state` value and a map of typed questions, and
//! answers every question with a label or a probability. The Observer calls
//! it only for cases its rules mark ambiguous, and asks three questions in one
//! request: `semantic_state` (choice), `needs_human` and `stuck` (yes/no).
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
use std::net::{TcpStream, ToSocketAddrs};
#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use spawn_core::observation::{Confidence, SemanticState};
use spawn_core::observe::{JudgeError, JudgeInput, Judgement, StateJudge};

/// Environment variable naming the local outbound UDS (`out.sock`).
pub const OUT_SOCK_ENV: &str = "SPAWN_OUT_SOCK";

/// Fixed local RPC path exposed by `spawn-node front --serve outbound`.
pub const OUT_JEV_CALL_PATH: &str = "/out/v1/jev/call";

/// Response read limit (matches the 4 MiB IPC payload cap plus HTTP headers).
const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024 + 64 * 1024;

/// Question name of the state choice.
pub const QUESTION: &str = "semantic_state";

/// Question name of the yes/no "a human must step in".
pub const NEEDS_HUMAN: &str = "needs_human";

/// Question name of the yes/no "the agent makes no progress".
pub const STUCK: &str = "stuck";

/// Instruction shown to the model for the state choice.
const INSTRUCTIONS: &str = "What is the semantic state of this coding agent session? \
Deterministic rules could not decide it; `ambiguity` says why. Read \
`screen_tail`, the agent's terminal as a person would see it, and `task`, \
what the agent was asked to do. An agent that ended its turn with a question \
or a request for a decision is BLOCKED, not WAITING. An agent whose task is \
done and that says so is COMPLETED.";

/// Instruction for `needs_human`.
const NEEDS_HUMAN_INSTRUCTIONS: &str = "Does this agent need a human to answer, decide, \
approve or fix something before it can continue its task?";

/// Instruction for `stuck`.
const STUCK_INSTRUCTIONS: &str = "Is this agent stuck: repeating the same failing \
action, looping, or making no progress toward its task?";

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

/// Model the hosted service answers with when the caller names none.
///
/// The hosted endpoint rejects a request without `model` (422, read from
/// the live service on 2026-11-04).
pub const DEFAULT_MODEL: &str = "jev-latest";

/// Request body for one snapshot, naming [`DEFAULT_MODEL`].
pub fn build_request(input: &JudgeInput) -> String {
    build_request_for(input, DEFAULT_MODEL)
}

/// Request body for one snapshot, answered by `model`.
pub fn build_request_for(input: &JudgeInput, model: &str) -> String {
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
    let needs_human = serde_json::to_string(NEEDS_HUMAN_INSTRUCTIONS).unwrap_or_default();
    let stuck = serde_json::to_string(STUCK_INSTRUCTIONS).unwrap_or_default();
    let model = serde_json::to_string(model).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        "{{\"model\":{model},\"state\":{state},\"questions\":{{\"{QUESTION}\":{{\"type\":\"choice\",\"instructions\":{instructions},\"criteria\":{criteria}}},\"{NEEDS_HUMAN}\":{{\"type\":\"noul\",\"instructions\":{needs_human}}},\"{STUCK}\":{{\"type\":\"noul\",\"instructions\":{stuck}}}}}}}"
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
    /// Probability of "yes" for a `noul` question.
    #[serde(default)]
    noul: Option<f64>,
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
    let distribution = (!answer.probabilities.is_empty()).then(|| {
        SemanticState::ALL.map(|candidate| {
            answer
                .probabilities
                .get(candidate.key())
                .copied()
                .unwrap_or(0.0)
        })
    });
    let yes = |name: &str| {
        reply
            .answers
            .get(name)
            .and_then(|answer| answer.noul)
            .filter(|value| value.is_finite() && (0.0..=1.0).contains(value))
    };
    Ok(Judgement {
        state,
        confidence,
        probabilities: distribution,
        needs_human: yes(NEEDS_HUMAN),
        stuck: yes(STUCK),
    })
}

/// Settings of the decision service.
#[derive(Debug, Clone)]
pub struct JevConfig {
    /// Base URL, without a trailing slash.
    pub base_url: String,
    /// Optional local `out.sock` UNIX domain socket path for outbound Jev RPC.
    pub out_sock: Option<PathBuf>,
    /// The timeout of each connect, write and read
    /// ([`JevConfig::DEFAULT_TIMEOUT`] unless set).
    pub timeout: Duration,
    /// Model named in every request.
    pub model: String,
}

impl JevConfig {
    /// The client's timeout: longer than the relay's whole call
    /// (`scripts/jev-relay.py`, `DEFAULT_TIMEOUT_SECS`, 12 s), so a relay that
    /// gives up on the hosted API answers 502 with its reason before this
    /// client stops reading. With 10 s here and 20 s there, the client hung
    /// up first and every 502 ended as a BrokenPipe in the relay's log, its
    /// reason lost (2026-11-07..10: 309 of 349). The relay's slowest
    /// answer then was 8.5 s.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

    /// Configuration for one base URL.
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            out_sock: None,
            timeout: Self::DEFAULT_TIMEOUT,
            model: DEFAULT_MODEL.to_string(),
        }
    }

    /// Configuration targeting a local `out.sock` UNIX domain socket.
    pub fn new_out_sock(path: impl Into<PathBuf>) -> Self {
        Self {
            base_url: String::new(),
            out_sock: Some(path.into()),
            timeout: Self::DEFAULT_TIMEOUT,
            model: DEFAULT_MODEL.to_string(),
        }
    }

    /// Route requests through the given local `out.sock` UNIX domain socket.
    #[must_use]
    pub fn with_out_sock(mut self, path: impl Into<PathBuf>) -> Self {
        self.out_sock = Some(path.into());
        self
    }

    /// Read [`OUT_SOCK_ENV`] (`SPAWN_OUT_SOCK`) using the process environment.
    pub fn from_out_sock_env() -> Option<Self> {
        Self::from_out_sock_lookup(|key| std::env::var(key).ok())
    }

    /// Read [`OUT_SOCK_ENV`] (`SPAWN_OUT_SOCK`) using a custom lookup function.
    pub fn from_out_sock_lookup(lookup: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let raw = lookup(OUT_SOCK_ENV)?;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }
        Some(Self::new_out_sock(trimmed))
    }
}

/// Judge that asks Jev over HTTP or a local `out.sock` UNIX domain socket.
#[derive(Debug, Clone)]
pub struct JevJudge {
    config: JevConfig,
}

impl JevJudge {
    /// Judge for one configuration.
    pub fn new(config: JevConfig) -> Self {
        Self { config }
    }

    /// Judge using a local `out.sock` UNIX domain socket.
    pub fn from_out_sock(path: impl Into<PathBuf>) -> Self {
        Self::new(JevConfig::new_out_sock(path))
    }

    /// Judge configured from [`OUT_SOCK_ENV`] (`SPAWN_OUT_SOCK`), if non-empty.
    pub fn from_out_sock_env() -> Option<Self> {
        JevConfig::from_out_sock_env().map(Self::new)
    }
}

impl StateJudge for JevJudge {
    fn name(&self) -> &'static str {
        "jev"
    }

    fn judge(&self, input: &JudgeInput) -> Result<Judgement, JudgeError> {
        let body = build_request_for(input, &self.config.model);
        let text = match &self.config.out_sock {
            Some(path) => post_json_unix(path, &body, self.config.timeout),
            None => post_json(&endpoint(&self.config.base_url), &body, self.config.timeout),
        }
        .map_err(JudgeError::Unavailable)?;
        parse_reply(&text)
    }
}

/// Validate `path` before dialing `out.sock`: refuses symlinks, non-sockets,
/// mismatched effective UID, and group/world permissions (`mode & 0o077 != 0`).
#[cfg(unix)]
pub fn validate_out_sock(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err("out.sock path is empty".to_string());
    }
    let meta = std::fs::symlink_metadata(path)
        .map_err(|error| format!("stat out.sock {}: {error}", path.display()))?;
    if meta.file_type().is_symlink() {
        return Err(format!(
            "refusing out.sock {}: symlink is not allowed",
            path.display()
        ));
    }
    if !meta.file_type().is_socket() {
        return Err(format!(
            "refusing out.sock {}: not a UNIX domain socket",
            path.display()
        ));
    }
    let expected_uid = current_euid()?;
    if meta.uid() != expected_uid {
        return Err(format!(
            "refusing out.sock {}: uid {} does not match current user {expected_uid}",
            path.display(),
            meta.uid()
        ));
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "refusing out.sock {}: insecure permissions {mode:04o} (group/other bits set)",
            path.display()
        ));
    }
    Ok(())
}

/// Validate `path` before dialing `out.sock` (unsupported outside Unix).
#[cfg(not(unix))]
pub fn validate_out_sock(_path: &Path) -> Result<(), String> {
    Err("out.sock UDS transport requires Unix".to_string())
}

#[cfg(target_os = "linux")]
fn current_euid() -> Result<u32, String> {
    let status = std::fs::read_to_string("/proc/self/status")
        .map_err(|error| format!("read /proc/self/status: {error}"))?;
    let uid_line = status
        .lines()
        .find(|line| line.starts_with("Uid:"))
        .ok_or_else(|| "missing Uid line in /proc/self/status".to_string())?;
    let euid = uid_line
        .strip_prefix("Uid:")
        .and_then(|rest| rest.split_whitespace().nth(1))
        .ok_or_else(|| "malformed Uid line in /proc/self/status".to_string())?;
    euid.parse::<u32>()
        .map_err(|error| format!("parse effective uid {euid:?}: {error}"))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn current_euid() -> Result<u32, String> {
    let output = std::process::Command::new("id")
        .arg("-u")
        .output()
        .map_err(|error| format!("run id -u: {error}"))?;
    if !output.status.success() {
        return Err(format!("id -u exited with {}", output.status));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.trim()
        .parse::<u32>()
        .map_err(|error| format!("parse effective uid {text:?}: {error}"))
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

/// Minimal HTTP/1.1 POST over a plain TCP socket.
///
/// The client talks to the loopback relay (`scripts/jev-relay.py`), which
/// adds the API key and the TLS hop to the hosted service, so plain HTTP is
/// enough here. A full client would add a TLS stack to the build for no gain.
fn post_json(url: &str, body: &str, timeout: Duration) -> Result<String, String> {
    let (host, port, path) = split_url(url)?;
    let address = format!("{host}:{port}");
    let socket = address
        .to_socket_addrs()
        .map_err(|error| format!("resolve {address}: {error}"))?
        .next()
        .ok_or_else(|| format!("resolve {address}: no address"))?;
    let stream = TcpStream::connect_timeout(&socket, timeout)
        .map_err(|error| format!("connect {address}: {error}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| format!("set read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| format!("set write timeout: {error}"))?;
    post_json_over_stream(stream, &host, &path, body)
}

#[cfg(unix)]
fn post_json_unix(path: &Path, body: &str, timeout: Duration) -> Result<String, String> {
    validate_out_sock(path)?;
    let stream = UnixStream::connect(path)
        .map_err(|error| format!("connect out.sock {}: {error}", path.display()))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|error| format!("set read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|error| format!("set write timeout: {error}"))?;
    post_json_over_stream(stream, "spawn-out", OUT_JEV_CALL_PATH, body)
}

#[cfg(not(unix))]
fn post_json_unix(_path: &Path, _body: &str, _timeout: Duration) -> Result<String, String> {
    Err("out.sock UDS transport requires Unix".to_string())
}

/// Shared HTTP/1.1 unary POST over an established stream with explicit `Content-Length`.
fn post_json_over_stream<S: Read + Write>(
    mut stream: S,
    host: &str,
    path: &str,
    body: &str,
) -> Result<String, String> {
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|error| format!("write request: {error}"))?;
    stream.flush().map_err(|error| format!("flush: {error}"))?;

    let mut raw = Vec::new();
    (&mut stream)
        .take(MAX_RESPONSE_BYTES)
        .read_to_end(&mut raw)
        .map_err(|error| format!("read response: {error}"))?;
    if raw.len() as u64 == MAX_RESPONSE_BYTES {
        return Err("response exceeded maximum size".to_string());
    }
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
