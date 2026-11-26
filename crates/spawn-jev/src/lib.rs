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
//!
//! # Security & Transport Invariants
//! - When configured with `out.sock` ([`JevConfig::new_out_sock`]), [`JevJudge`]
//!   validates the socket before connecting via [`validate_out_sock`]
//!   (rejecting symlinks, non-sockets, foreign UIDs, and group/world-writable or
//!   readable modes `mode & 0o077 != 0`).
//! - Implemented in pure safe Rust (`#![forbid(unsafe_code)]`) using plain
//!   `http://` or UNIX domain sockets without extra TLS/crypto dependencies.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

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
const OUT_JEV_CALL_PATH: &str = "/out/v1/jev/call";

/// Response read limit (matches the 4 MiB IPC payload cap plus HTTP headers).
const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024 + 64 * 1024;

/// Question name of the state choice.
const QUESTION: &str = "semantic_state";

/// Question name of the yes/no "a human must step in".
const NEEDS_HUMAN: &str = "needs_human";

/// Question name of the yes/no "the agent makes no progress".
const STUCK: &str = "stuck";

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

/// Return the System One endpoint URL for a relay or gateway `base_url`.
///
/// Strips any trailing `'/'` from `base_url` before appending `/v1/systemone`.
pub fn endpoint(base_url: &str) -> String {
    format!("{}/v1/systemone", base_url.trim_end_matches('/'))
}

/// Model the hosted service answers with when the caller names none.
///
/// The hosted endpoint rejects a request without `model` (422, read from
/// the live service on 2026-11-04).
const DEFAULT_MODEL: &str = "jev-latest";

/// Build the JSON request body for one [`JudgeInput`] snapshot using the
/// default `"jev-latest"` model.
///
/// Preserves [`SemanticState::ALL`] declaration order inside the serialized
/// `criteria` map because the remote service renders choices in field order.
pub fn build_request(input: &JudgeInput) -> String {
    build_request_for(input, DEFAULT_MODEL)
}

/// Build the JSON request body for one [`JudgeInput`] snapshot targeting an explicit `model`.
fn build_request_for(input: &JudgeInput, model: &str) -> String {
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

/// Parse one Jev JSON response body into a validated [`Judgement`].
///
/// Selects `choice` when present, otherwise picks the `probabilities` entry with
/// the highest score, clamps missing/invalid confidence values to
/// [`Confidence::ZERO`], and extracts finite `[0.0, 1.0]` probabilities for
/// `needs_human` and `stuck`.
///
/// # Errors
/// Returns [`JudgeError::Unavailable`] if the payload is not valid JSON, lacks
/// the `semantic_state` answer, has neither `choice` nor `probabilities`, or
/// names an unknown [`SemanticState`] key.
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

    /// Build an HTTP relay configuration for `base_url` (trailing `'/'` trimmed)
    /// with [`Self::DEFAULT_TIMEOUT`] and model `"jev-latest"`.
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            out_sock: None,
            timeout: Self::DEFAULT_TIMEOUT,
            model: DEFAULT_MODEL.to_string(),
        }
    }

    /// Build an outbound UDS configuration targeting `path` (`out.sock`)
    /// with [`Self::DEFAULT_TIMEOUT`] and model `"jev-latest"`.
    pub fn new_out_sock(path: impl Into<PathBuf>) -> Self {
        Self {
            base_url: String::new(),
            out_sock: Some(path.into()),
            timeout: Self::DEFAULT_TIMEOUT,
            model: DEFAULT_MODEL.to_string(),
        }
    }

    /// Return a copy of this configuration routed through local `out.sock` path `path`.
    #[must_use]
    pub fn with_out_sock(mut self, path: impl Into<PathBuf>) -> Self {
        self.out_sock = Some(path.into());
        self
    }

    /// Construct a `JevConfig` from [`OUT_SOCK_ENV`] (`SPAWN_OUT_SOCK`) if set and non-empty.
    pub fn from_out_sock_env() -> Option<Self> {
        Self::from_out_sock_lookup(|key| std::env::var(key).ok())
    }

    /// Construct a `JevConfig` using an injected environment lookup closure for [`OUT_SOCK_ENV`].
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

/// Validate `path` before dialing `out.sock`.
///
/// Enforces that `path` is non-empty, exists as an `AF_UNIX` socket (never a
/// symlink or regular file), is owned by the current effective UID, and grants
/// zero group/other permission bits (`mode & 0o077 == 0`).
///
/// # Errors
/// Returns an explanatory error string on empty paths, `lstat` failure,
/// symlinks, non-socket files, UID mismatches, or permissive file modes.
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

/// Default bounded timeout (`1.5 s`) for model and judge endpoint health probes.
///
/// Short enough to keep CLI startup and `spawn doctor` responsive even when an
/// endpoint is down or dropping packets.
pub const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_millis(1500);

/// Reachability classification returned by [`probe_http_endpoint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointStatus {
    /// Endpoint accepted a connection and answered with a non-5xx HTTP status (`200..500`).
    Ok,
    /// Endpoint could not be reached, timed out, returned malformed HTTP, or answered `5xx`.
    Unreachable,
    /// Endpoint URL was empty, malformed, or used an unsupported scheme.
    Invalid,
}

impl EndpointStatus {
    /// Stable lowercase identifier for logs and JSON diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Unreachable => "unreachable",
            Self::Invalid => "invalid",
        }
    }

    /// Return `true` when the endpoint responded as healthy ([`Self::Ok`]).
    #[must_use]
    pub const fn is_ok(self) -> bool {
        matches!(self, Self::Ok)
    }
}

/// Result of probing one HTTP or UNIX domain socket endpoint.
///
/// All string fields are sanitized so embedded userinfo, query parameters,
/// fragments, and response bodies are never exposed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointHealth {
    /// Outcome of the endpoint health probe.
    pub status: EndpointStatus,
    /// Sanitized target (`host:port` or `unix://<socket>`), or `"<invalid-endpoint>"`.
    pub target: String,
    /// HTTP status code received from the endpoint, when an HTTP response line arrived.
    pub http_status: Option<u16>,
    /// Elapsed round-trip probe duration in milliseconds.
    pub latency_ms: u64,
    /// Sanitized reason describing why the probe did not succeed, if unhealthy or invalid.
    pub detail: Option<String>,
}

/// Sanitize a model or judge endpoint URL into a safe display target.
///
/// Strips any embedded userinfo (`user:pass@`), path, query string, and
/// fragment. Returns `host:port` for `http://` and `https://`,
/// `unix://<socket>` for `unix://`, and `"<invalid-endpoint>"` for unparseable
/// or schemeless inputs so raw tokens can never leak into logs.
#[must_use]
pub fn sanitize_endpoint_target(url: &str) -> String {
    let trimmed = url.trim();
    if let Some(rest) = strip_scheme_ci(trimmed, "unix://") {
        let before_pipe = rest.split('|').next().unwrap_or("");
        let sock = strip_query_and_fragment(before_pipe).trim();
        if sock.is_empty() || sock.chars().any(|ch| ch.is_control() || ch.is_whitespace()) {
            return "<invalid-endpoint>".to_string();
        }
        return format!("unix://{sock}");
    }
    if let Some((rest, default_port)) = strip_scheme_ci(trimmed, "http://")
        .map(|rest| (rest, 80u16))
        .or_else(|| strip_scheme_ci(trimmed, "https://").map(|rest| (rest, 443u16)))
    {
        return parse_http_authority(rest, default_port).map_or_else(
            |_| "<invalid-endpoint>".to_string(),
            |(host, port)| format!("{host}:{port}"),
        );
    }
    "<invalid-endpoint>".to_string()
}

/// Probe a configured `http://host:port[/path]` or `unix://<socket>[|<path>]`
/// endpoint with a bounded HTTP/1.1 `GET` request.
///
/// Defaults the probe path to `/healthz` when the URL omits a path or specifies
/// only `"/"`, and strips query parameters and fragments before sending the
/// request. Treats HTTP `200..500` status codes as [`EndpointStatus::Ok`],
/// connection/validation/timeout/parse errors and HTTP `5xx` as
/// [`EndpointStatus::Unreachable`], and unsupported or malformed URLs as
/// [`EndpointStatus::Invalid`]. A zero `timeout` falls back to
/// [`DEFAULT_PROBE_TIMEOUT`].
///
/// Neither userinfo nor query strings nor HTTP response bodies appear in the
/// returned [`EndpointHealth`].
#[must_use]
pub fn probe_http_endpoint(url: &str, timeout: Duration) -> EndpointHealth {
    let started = std::time::Instant::now();
    let timeout = if timeout.is_zero() {
        DEFAULT_PROBE_TIMEOUT
    } else {
        timeout
    };

    let target = match parse_probe_target(url) {
        Ok(parsed) => parsed,
        Err((sanitized, reason)) => {
            return EndpointHealth {
                status: EndpointStatus::Invalid,
                target: sanitized,
                http_status: None,
                latency_ms: elapsed_ms(started),
                detail: Some(reason),
            };
        }
    };

    match target {
        ProbeTarget::Http {
            host,
            port,
            path,
            display_target,
        } => probe_tcp_http(&host, port, &path, &display_target, timeout, started),
        ProbeTarget::Unix {
            socket,
            path,
            display_target,
        } => probe_unix_http(&socket, &path, &display_target, timeout, started),
    }
}

enum ProbeTarget {
    Http {
        host: String,
        port: u16,
        path: String,
        display_target: String,
    },
    Unix {
        socket: PathBuf,
        path: String,
        display_target: String,
    },
}

fn elapsed_ms(started: std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn strip_scheme_ci<'a>(input: &'a str, scheme: &str) -> Option<&'a str> {
    let prefix = input.get(..scheme.len())?;
    prefix
        .eq_ignore_ascii_case(scheme)
        .then(|| &input[scheme.len()..])
}

fn strip_query_and_fragment(input: &str) -> &str {
    input.split(['?', '#']).next().unwrap_or("")
}

fn parse_http_authority(rest: &str, default_port: u16) -> Result<(String, u16), String> {
    let without_qf = strip_query_and_fragment(rest);
    let authority = without_qf
        .find('/')
        .map_or(without_qf, |index| &without_qf[..index]);
    if authority
        .chars()
        .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return Err(
            "endpoint authority contains invalid whitespace or control characters".to_string(),
        );
    }
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, right)| right);
    if host_port.is_empty() {
        return Err("endpoint URL has no host".to_string());
    }

    if let Some(after_open) = host_port.strip_prefix('[') {
        let Some(close_idx) = after_open.find(']') else {
            return Err("unclosed IPv6 host bracket in endpoint URL".to_string());
        };
        let host = &host_port[..=close_idx + 1];
        if close_idx == 0 {
            return Err("empty IPv6 host in endpoint URL".to_string());
        }
        let trailing = &after_open[close_idx + 1..];
        let port = if trailing.is_empty() {
            default_port
        } else if let Some(port_text) = trailing.strip_prefix(':') {
            parse_valid_port(port_text)?
        } else {
            return Err("invalid port syntax after IPv6 host".to_string());
        };
        return Ok((host.to_string(), port));
    }

    if host_port.contains(['[', ']']) {
        return Err("unexpected bracket in endpoint host".to_string());
    }

    let (host, port) = match host_port.split_once(':') {
        Some((host, port_text)) => {
            if port_text.contains(':') {
                return Err("bare IPv6 hosts must be wrapped in square brackets".to_string());
            }
            (host, parse_valid_port(port_text)?)
        }
        None => (host_port, default_port),
    };
    if host.is_empty() {
        return Err("endpoint URL has no host".to_string());
    }
    Ok((host.to_string(), port))
}

fn parse_valid_port(raw: &str) -> Result<u16, String> {
    let port = raw
        .parse::<u16>()
        .map_err(|_| "endpoint port is not a valid 1..=65535 integer".to_string())?;
    if port == 0 {
        return Err("endpoint port 0 is not valid".to_string());
    }
    Ok(port)
}

fn parse_probe_target(url: &str) -> Result<ProbeTarget, (String, String)> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err((
            "<invalid-endpoint>".to_string(),
            "endpoint URL is empty".to_string(),
        ));
    }

    if let Some(rest) = strip_scheme_ci(trimmed, "unix://") {
        let (raw_sock, raw_path) = match rest.split_once('|') {
            Some((sock, path)) => (sock, Some(path)),
            None => (rest, None),
        };
        let sock = strip_query_and_fragment(raw_sock).trim();
        if sock.is_empty() || sock.chars().any(|ch| ch.is_control() || ch.is_whitespace()) {
            return Err((
                "<invalid-endpoint>".to_string(),
                "invalid unix:// socket path".to_string(),
            ));
        }
        let probe_path = match raw_path {
            Some(raw) => {
                let clean = strip_query_and_fragment(raw).trim();
                if clean
                    .chars()
                    .any(|ch| ch.is_control() || ch.is_whitespace())
                {
                    return Err((
                        format!("unix://{sock}"),
                        "unix:// probe path contains invalid characters".to_string(),
                    ));
                }
                normalize_probe_path(clean)
            }
            None => "/healthz".to_string(),
        };
        let display_target = format!("unix://{sock}");
        return Ok(ProbeTarget::Unix {
            socket: PathBuf::from(sock),
            path: probe_path,
            display_target,
        });
    }

    if let Some(rest) = strip_scheme_ci(trimmed, "http://") {
        let (host, port) = parse_http_authority(rest, 80)
            .map_err(|reason| (sanitize_endpoint_target(trimmed), reason))?;
        let without_qf = strip_query_and_fragment(rest);
        let raw_path = without_qf
            .find('/')
            .map_or("", |index| &without_qf[index..]);
        if raw_path
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
        {
            return Err((
                format!("{host}:{port}"),
                "endpoint URL path contains invalid characters".to_string(),
            ));
        }
        let path = normalize_probe_path(raw_path);
        let display_target = format!("{host}:{port}");
        return Ok(ProbeTarget::Http {
            host,
            port,
            path,
            display_target,
        });
    }

    Err((
        sanitize_endpoint_target(trimmed),
        "only http:// and unix:// endpoints are supported for health probes".to_string(),
    ))
}

fn normalize_probe_path(raw_path: &str) -> String {
    if raw_path.is_empty() || raw_path == "/" {
        "/healthz".to_string()
    } else if raw_path.starts_with('/') {
        raw_path.to_string()
    } else {
        format!("/{raw_path}")
    }
}

fn probe_tcp_http(
    host: &str,
    port: u16,
    path: &str,
    display_target: &str,
    timeout: Duration,
    started: std::time::Instant,
) -> EndpointHealth {
    let connect_addr = format!("{host}:{port}");
    let addresses: Vec<std::net::SocketAddr> = match connect_addr.to_socket_addrs() {
        Ok(iter) => iter.collect(),
        Err(error) => {
            return EndpointHealth {
                status: EndpointStatus::Unreachable,
                target: display_target.to_string(),
                http_status: None,
                latency_ms: elapsed_ms(started),
                detail: Some(format!("resolve {display_target}: {error}")),
            };
        }
    };
    if addresses.is_empty() {
        return EndpointHealth {
            status: EndpointStatus::Unreachable,
            target: display_target.to_string(),
            http_status: None,
            latency_ms: elapsed_ms(started),
            detail: Some(format!("resolve {display_target}: no address")),
        };
    }

    let mut connected: Option<TcpStream> = None;
    let mut last_error: Option<std::io::Error> = None;
    for addr in addresses {
        let Some(remaining) = timeout
            .checked_sub(started.elapsed())
            .filter(|d| !d.is_zero())
        else {
            break;
        };
        match TcpStream::connect_timeout(&addr, remaining) {
            Ok(stream) => {
                connected = Some(stream);
                break;
            }
            Err(error) => last_error = Some(error),
        }
    }

    let Some(stream) = connected else {
        let detail = match last_error {
            Some(error) => format!("connect {display_target}: {error}"),
            None => format!("connect {display_target}: timed out"),
        };
        return EndpointHealth {
            status: EndpointStatus::Unreachable,
            target: display_target.to_string(),
            http_status: None,
            latency_ms: elapsed_ms(started),
            detail: Some(detail),
        };
    };

    let remaining = timeout
        .checked_sub(started.elapsed())
        .filter(|d| !d.is_zero())
        .unwrap_or_else(|| Duration::from_millis(1));
    if let Err(error) = stream
        .set_read_timeout(Some(remaining))
        .and_then(|()| stream.set_write_timeout(Some(remaining)))
    {
        return EndpointHealth {
            status: EndpointStatus::Unreachable,
            target: display_target.to_string(),
            http_status: None,
            latency_ms: elapsed_ms(started),
            detail: Some(format!(
                "configure socket timeout on {display_target}: {error}"
            )),
        };
    }

    probe_http_over_stream(stream, display_target, display_target, path, started)
}

#[cfg(unix)]
fn probe_unix_http(
    socket: &Path,
    path: &str,
    display_target: &str,
    timeout: Duration,
    started: std::time::Instant,
) -> EndpointHealth {
    if let Err(error) = validate_out_sock(socket) {
        return EndpointHealth {
            status: EndpointStatus::Unreachable,
            target: display_target.to_string(),
            http_status: None,
            latency_ms: elapsed_ms(started),
            detail: Some(error),
        };
    }
    let stream = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(error) => {
            return EndpointHealth {
                status: EndpointStatus::Unreachable,
                target: display_target.to_string(),
                http_status: None,
                latency_ms: elapsed_ms(started),
                detail: Some(format!("connect {display_target}: {error}")),
            };
        }
    };
    let remaining = timeout
        .checked_sub(started.elapsed())
        .filter(|d| !d.is_zero())
        .unwrap_or_else(|| Duration::from_millis(1));
    if let Err(error) = stream
        .set_read_timeout(Some(remaining))
        .and_then(|()| stream.set_write_timeout(Some(remaining)))
    {
        return EndpointHealth {
            status: EndpointStatus::Unreachable,
            target: display_target.to_string(),
            http_status: None,
            latency_ms: elapsed_ms(started),
            detail: Some(format!(
                "configure socket timeout on {display_target}: {error}"
            )),
        };
    }

    probe_http_over_stream(stream, "localhost", display_target, path, started)
}

#[cfg(not(unix))]
fn probe_unix_http(
    _socket: &Path,
    _path: &str,
    display_target: &str,
    _timeout: Duration,
    started: std::time::Instant,
) -> EndpointHealth {
    EndpointHealth {
        status: EndpointStatus::Unreachable,
        target: display_target.to_string(),
        http_status: None,
        latency_ms: elapsed_ms(started),
        detail: Some("unix:// endpoints require Unix".to_string()),
    }
}

fn probe_http_over_stream<S: Read + Write>(
    mut stream: S,
    host_header: &str,
    display_target: &str,
    path: &str,
    started: std::time::Instant,
) -> EndpointHealth {
    const MAX_STATUS_READ: usize = 4096;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host_header}\r\nConnection: close\r\nUser-Agent: spawn-preflight/1\r\n\r\n"
    );
    if let Err(error) = stream
        .write_all(request.as_bytes())
        .and_then(|()| stream.flush())
    {
        return EndpointHealth {
            status: EndpointStatus::Unreachable,
            target: display_target.to_string(),
            http_status: None,
            latency_ms: elapsed_ms(started),
            detail: Some(format!("write probe request to {display_target}: {error}")),
        };
    }

    let mut raw = Vec::new();
    let mut chunk = [0u8; 512];
    while raw.len() < MAX_STATUS_READ {
        let cap = chunk.len().min(MAX_STATUS_READ - raw.len());
        match stream.read(&mut chunk[..cap]) {
            Ok(0) => break,
            Ok(n) => {
                raw.extend_from_slice(&chunk[..n]);
                if raw.contains(&b'\n') {
                    break;
                }
            }
            Err(error) => {
                if raw.is_empty() {
                    return EndpointHealth {
                        status: EndpointStatus::Unreachable,
                        target: display_target.to_string(),
                        http_status: None,
                        latency_ms: elapsed_ms(started),
                        detail: Some(format!(
                            "read probe response from {display_target}: {error}"
                        )),
                    };
                }
                break;
            }
        }
    }

    let status_line = String::from_utf8_lossy(&raw);
    let first_line = status_line.lines().next().unwrap_or("").trim();
    let parsed_code = first_line
        .strip_prefix("HTTP/")
        .and_then(|_| first_line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .filter(|code| (100..600).contains(code));

    match parsed_code {
        Some(code) if (200..500).contains(&code) => EndpointHealth {
            status: EndpointStatus::Ok,
            target: display_target.to_string(),
            http_status: Some(code),
            latency_ms: elapsed_ms(started),
            detail: None,
        },
        Some(code) => EndpointHealth {
            status: EndpointStatus::Unreachable,
            target: display_target.to_string(),
            http_status: Some(code),
            latency_ms: elapsed_ms(started),
            detail: Some(format!("endpoint returned HTTP {code}")),
        },
        None => EndpointHealth {
            status: EndpointStatus::Unreachable,
            target: display_target.to_string(),
            http_status: None,
            latency_ms: elapsed_ms(started),
            detail: Some("endpoint closed or returned an invalid HTTP status line".to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{EndpointStatus, probe_http_endpoint, sanitize_endpoint_target};
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::thread;
    use std::time::Duration;

    fn respond_once(
        listener: TcpListener,
        status_line: &str,
        body: &str,
    ) -> thread::JoinHandle<String> {
        let status_line = status_line.to_string();
        let body = body.to_string();
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept client");
            let mut req_buf = [0u8; 2048];
            let read = stream.read(&mut req_buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&req_buf[..read]).to_string();
            let reply = format!(
                "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(reply.as_bytes());
            let _ = stream.flush();
            req
        })
    }

    #[test]
    fn probe_http_endpoint_healthy_for_200_and_404_and_uses_healthz_default() {
        let listener_200 = TcpListener::bind("127.0.0.1:0").expect("bind 200");
        let port_200 = listener_200.local_addr().expect("addr 200").port();
        let srv_200 = respond_once(listener_200, "HTTP/1.1 200 OK", "{\"ok\":true}");

        let health_200 = probe_http_endpoint(
            &format!("http://127.0.0.1:{port_200}"),
            Duration::from_secs(1),
        );
        let req_200 = srv_200.join().expect("join 200");
        assert!(req_200.starts_with("GET /healthz HTTP/1.1"), "{req_200}");
        assert_eq!(health_200.status, EndpointStatus::Ok);
        assert_eq!(health_200.http_status, Some(200));
        assert_eq!(health_200.target, format!("127.0.0.1:{port_200}"));
        assert_eq!(health_200.detail, None);

        let listener_404 = TcpListener::bind("127.0.0.1:0").expect("bind 404");
        let port_404 = listener_404.local_addr().expect("addr 404").port();
        let srv_404 = respond_once(
            listener_404,
            "HTTP/1.1 404 Not Found",
            "{\"error\":\"nope\"}",
        );

        let health_404 = probe_http_endpoint(
            &format!("http://127.0.0.1:{port_404}/custom?secret=NeverSent99"),
            Duration::from_secs(1),
        );
        let req_404 = srv_404.join().expect("join 404");
        assert!(req_404.starts_with("GET /custom HTTP/1.1"), "{req_404}");
        assert!(
            !req_404.contains("NeverSent99"),
            "query parameter must be stripped: {req_404}"
        );
        assert_eq!(health_404.status, EndpointStatus::Ok);
        assert_eq!(health_404.http_status, Some(404));
        assert_eq!(health_404.target, format!("127.0.0.1:{port_404}"));
        assert_eq!(health_404.detail, None);
    }

    #[test]
    fn probe_http_endpoint_unreachable_for_503_without_leaking_body() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 503");
        let port = listener.local_addr().expect("addr 503").port();
        let srv = respond_once(
            listener,
            "HTTP/1.1 503 Service Unavailable",
            "upstream-secret-body-LeakToken123",
        );

        let health =
            probe_http_endpoint(&format!("http://127.0.0.1:{port}"), Duration::from_secs(1));
        let _ = srv.join();
        assert_eq!(health.status, EndpointStatus::Unreachable);
        assert_eq!(health.http_status, Some(503));
        assert_eq!(health.target, format!("127.0.0.1:{port}"));
        let detail = health.detail.as_deref().unwrap_or("");
        assert!(detail.contains("503"), "{detail}");
        assert!(!detail.contains("LeakToken123"), "{detail}");
    }

    #[test]
    fn probe_http_endpoint_unreachable_for_closed_port() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind closed");
        let port = listener.local_addr().expect("addr closed").port();
        drop(listener);

        let health =
            probe_http_endpoint(&format!("http://127.0.0.1:{port}"), Duration::from_secs(1));
        assert_eq!(health.status, EndpointStatus::Unreachable);
        assert_eq!(health.http_status, None);
        assert_eq!(health.target, format!("127.0.0.1:{port}"));
        assert!(health.detail.is_some());
    }

    #[test]
    fn probe_http_endpoint_sanitizes_credentials_query_and_invalid_schemes() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind closed");
        let port = listener.local_addr().expect("addr closed").port();
        drop(listener);

        let http_secret = format!(
            "http://alice:RelayPassSecret99@127.0.0.1:{port}/v1?api_key=QueryApiKey88#FragSec77"
        );
        let health_http = probe_http_endpoint(&http_secret, Duration::from_secs(1));
        assert_eq!(health_http.status, EndpointStatus::Unreachable);
        assert_eq!(health_http.target, format!("127.0.0.1:{port}"));
        let rendered_http = format!("{health_http:?}");
        for forbidden in ["alice", "RelayPassSecret99", "QueryApiKey88", "FragSec77"] {
            assert!(
                !rendered_http.contains(forbidden),
                "forbidden credential fragment {forbidden:?} leaked into {rendered_http}"
            );
        }

        let https_url = "https://bob:HttpsSecret55@model.example.com:8443/v1?token=Tok44";
        assert_eq!(
            sanitize_endpoint_target(https_url),
            "model.example.com:8443"
        );
        let health_https = probe_http_endpoint(https_url, Duration::from_secs(1));
        assert_eq!(health_https.status, EndpointStatus::Invalid);
        assert_eq!(health_https.target, "model.example.com:8443");
        let rendered_https = format!("{health_https:?}");
        for forbidden in ["bob", "HttpsSecret55", "Tok44"] {
            assert!(!rendered_https.contains(forbidden), "{rendered_https}");
        }

        let raw_token = "sk-ant-raw-credential-never-print-me";
        assert_eq!(sanitize_endpoint_target(raw_token), "<invalid-endpoint>");
        let health_bad = probe_http_endpoint(raw_token, Duration::from_secs(1));
        assert_eq!(health_bad.status, EndpointStatus::Invalid);
        assert_eq!(health_bad.target, "<invalid-endpoint>");
        assert!(!format!("{health_bad:?}").contains(raw_token));
    }

    #[cfg(unix)]
    #[test]
    fn probe_http_endpoint_unix_socket_healthy_missing_and_insecure() {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixListener;

        struct SockDir(PathBuf);
        impl Drop for SockDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        let dir = SockDir(PathBuf::from(format!(
            "/tmp/spj-p-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_nanos())
        )));
        std::fs::create_dir_all(&dir.0).expect("mkdir");
        std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o700))
            .expect("chmod dir");

        let sock_path = dir.0.join("out.sock");
        let listener = UnixListener::bind(&sock_path).expect("bind unix");
        std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600))
            .expect("chmod sock");

        let srv = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept unix");
            let mut req_buf = [0u8; 1024];
            let n = stream.read(&mut req_buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&req_buf[..n]).to_string();
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
            );
            let _ = stream.flush();
            req
        });

        let url = format!("unix://{}|/healthz?token=UdsSecretTok", sock_path.display());
        let health = probe_http_endpoint(&url, Duration::from_secs(1));
        let req = srv.join().expect("join unix");
        assert!(req.starts_with("GET /healthz HTTP/1.1"), "{req}");
        assert!(!req.contains("UdsSecretTok"), "{req}");
        assert_eq!(health.status, EndpointStatus::Ok);
        assert_eq!(health.http_status, Some(200));
        assert_eq!(health.target, format!("unix://{}", sock_path.display()));
        assert!(!format!("{health:?}").contains("UdsSecretTok"));

        // Permissive permissions must fail validate_out_sock before connecting.
        std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o644))
            .expect("chmod 644");
        let insecure = probe_http_endpoint(
            &format!("unix://{}", sock_path.display()),
            Duration::from_secs(1),
        );
        assert_eq!(insecure.status, EndpointStatus::Unreachable);
        assert!(
            insecure
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("insecure permissions")),
            "{insecure:?}"
        );

        let missing = probe_http_endpoint(
            &format!("unix://{}", dir.0.join("missing.sock").display()),
            Duration::from_secs(1),
        );
        assert_eq!(missing.status, EndpointStatus::Unreachable);
    }
}
