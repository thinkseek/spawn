//! OpenAI Codex CLI driver (`codex`), interactive in a terminal under Herdr.
//!
//! Pure domain rules (#91 P2 / #113 D1): launch planning, incremental JSONL
//! parsing, deterministic terminal-tail classification, static dialog and
//! hard-block rule tables, safe session identifiers, and non-secret env
//! allowlisting. Performs no IO and reads no clock.

use serde_json::{Value, json};

use crate::command::is_model_label;
use crate::driver::{
    AgentDriver, DialogGuard, DialogOutcome, DialogRule, DriverTraits, EngineHome, EngineLaunch,
    Guard, HardBlockRule, LaunchPlan, LineCursor, LineFacts, RuleMatch, SessionBinding, TurnSource,
    plain_session_id, sanitize_worker_key,
};
use crate::event::EventKind;
use crate::id::{ExecutionId, WorkspaceId};
use crate::ingest::{Normalized, tool_summary, utc_ms};
use crate::message::markers;
use crate::redact::redact;
use crate::screen::{ScreenReading, ScreenState, TAIL_LINES, drop_agy_account_lines, strip_ansi};

/// Wire identifier for the Codex CLI driver.
pub const CODEX_ID: &str = "codex";

/// Human-readable title for the Codex CLI driver.
pub const CODEX_TITLE: &str = "Codex CLI";

/// Default Codex home directory relative to `$HOME` (`SPAWN_CODEX_HOME` overrides).
pub const CODEX_HOME: &str = ".codex";

/// Codex CLI `major.minor` releases verified against Spawn's parser and screen rules.
pub const CODEX_TESTED: [&str; 1] = ["0.171"];

/// Explicit non-secret environment variable allowlist forwarded to Codex launches.
///
/// API credentials (`OPENAI_API_KEY`, etc.) are deliberately excluded; authentication
/// lives inside the host's configured `$CODEX_HOME` or sandboxed relay environment.
pub const CODEX_ENV_ALLOWLIST: &[&str] = &[
    "CODEX_HOME",
    "OPENAI_BASE_URL",
    "NO_COLOR",
    "TERM",
    "LANG",
    "LC_ALL",
    "PATH",
    "HOME",
    "TMPDIR",
];

/// Maximum error message characters retained in normalized events.
const MAX_ERROR_CHARS: usize = 2000;

/// Maximum terminal excerpt evidence characters retained in [`ScreenReading`].
const MAX_EVIDENCE_CHARS: usize = 200;

/// Minimum token length for plain-text reported session IDs.
const MIN_PLAIN_SESSION_LEN: usize = 8;

/// Uncaptured dialog escalation reason (fail-closed until live pane verified).
const CODEX_UNCAPTURED: &str = "no captured screen of this dialog yet: a person answers it";

/// Declarative dialog rules for Codex (`#113` D1). All currently fail-closed to human escalation.
pub const CODEX_DIALOG_RULES: &[DialogRule] = &[
    DialogRule {
        engine: CODEX_ID,
        rule: RuleMatch::Exact("trust_directory"),
        kind: None,
        guard: DialogGuard::Always,
        outcome: DialogOutcome::Escalate(CODEX_UNCAPTURED),
    },
    DialogRule {
        engine: CODEX_ID,
        rule: RuleMatch::Exact("startup_update"),
        kind: None,
        guard: DialogGuard::Always,
        outcome: DialogOutcome::Escalate(CODEX_UNCAPTURED),
    },
    DialogRule {
        engine: CODEX_ID,
        rule: RuleMatch::Exact("live_strong_blocker"),
        kind: None,
        guard: DialogGuard::Always,
        outcome: DialogOutcome::Escalate(CODEX_UNCAPTURED),
    },
    DialogRule {
        engine: CODEX_ID,
        rule: RuleMatch::Exact("permission_prompt"),
        kind: None,
        guard: DialogGuard::Always,
        outcome: DialogOutcome::Escalate(CODEX_UNCAPTURED),
    },
    DialogRule {
        engine: CODEX_ID,
        rule: RuleMatch::Exact("approval_prompt"),
        kind: None,
        guard: DialogGuard::Always,
        outcome: DialogOutcome::Escalate(CODEX_UNCAPTURED),
    },
];

/// Declarative hard-block tail rules evaluated on Codex panes (`#113` D1).
pub const CODEX_HARD_BLOCK_RULES: &[HardBlockRule] = &[
    HardBlockRule {
        needle: "please run /login",
        reason: "login required",
    },
    HardBlockRule {
        needle: "login required",
        reason: "login required",
    },
    HardBlockRule {
        needle: "not logged in",
        reason: "login required",
    },
    HardBlockRule {
        needle: "codex login",
        reason: "login required",
    },
    HardBlockRule {
        needle: "sign in with chatgpt",
        reason: "login required",
    },
    HardBlockRule {
        needle: "invalid_api_key",
        reason: "login required",
    },
    HardBlockRule {
        needle: "incorrect api key",
        reason: "login required",
    },
    HardBlockRule {
        needle: "oauth token has expired",
        reason: "login expired",
    },
    HardBlockRule {
        needle: "authentication_error",
        reason: "login expired",
    },
    HardBlockRule {
        needle: "token_expired",
        reason: "login expired",
    },
    HardBlockRule {
        needle: "rate limit reached",
        reason: "rate limit",
    },
    HardBlockRule {
        needle: "rate_limit_error",
        reason: "rate limit",
    },
    HardBlockRule {
        needle: "rate_limit_exceeded",
        reason: "rate limit",
    },
    HardBlockRule {
        needle: "too many requests",
        reason: "rate limit",
    },
    HardBlockRule {
        needle: "usage limit reached",
        reason: "quota reached",
    },
    HardBlockRule {
        needle: "quota exceeded",
        reason: "quota reached",
    },
    HardBlockRule {
        needle: "insufficient_quota",
        reason: "quota reached",
    },
    HardBlockRule {
        needle: "resource_exhausted",
        reason: "quota reached",
    },
    HardBlockRule {
        needle: "billing hard limit",
        reason: "quota reached",
    },
    HardBlockRule {
        needle: "update required",
        reason: "update required",
    },
    HardBlockRule {
        needle: "please update to continue",
        reason: "update required",
    },
];

/// Fatal startup refusal phrases printed when Codex declines to run as configured.
const CODEX_START_REFUSALS: [&str; 7] = [
    "model is not supported",
    "does not exist or you do not have access",
    "model_not_found",
    "unknown model",
    "invalid model",
    "missing openai_api_key",
    "please run `codex login`",
];

/// Codex idle placeholder strings that appear inside an empty prompt box.
const CODEX_PLACEHOLDERS: [&str; 8] = [
    "ask codex to do anything",
    "improve documentation in @",
    "explain this codebase",
    "find and fix a bug",
    "write tests for",
    "summarize recent commits",
    "type a message",
    "send a message",
];

/// Footer tokens identifying a real Codex TUI status bar.
const CODEX_FOOTER_HINTS: [&str; 10] = [
    "? for shortcuts",
    "context left",
    "shift+tab",
    "tab to queue",
    "workspace-write",
    "danger-full-access",
    "read-only",
    "sandbox",
    "approvals",
    "ctrl+",
];

/// OpenAI Codex CLI (`codex`) driver registered alongside Claude Code and agy.
#[derive(Debug, Clone, Copy, Default)]
pub struct CodexDriver;

/// Validate that `native` is safe to embed in paths and CLI positional resume arguments.
pub fn is_safe_codex_session_id(native: &str) -> bool {
    plain_session_id(native) && !native.starts_with('-') && !native.starts_with('.')
}

/// Validate that `model` is a safe Codex model identifier for `--model`.
pub fn is_safe_codex_model(model: &str) -> bool {
    is_model_label(model.trim())
}

/// Filter `(key, value)` environment pairs to [`CODEX_ENV_ALLOWLIST`], dropping empty
/// values and any string carrying NUL/CR/LF control characters.
pub fn filter_codex_env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut allowed = Vec::new();
    for &(key, value) in pairs {
        if !CODEX_ENV_ALLOWLIST.contains(&key) {
            continue;
        }
        let clean = value.trim();
        if clean.is_empty() || clean.contains(['\0', '\r', '\n']) {
            continue;
        }
        if let Some(existing) = allowed
            .iter_mut()
            .find(|(existing_key, _): &&mut (String, String)| existing_key == key)
        {
            existing.1 = clean.to_string();
        } else {
            allowed.push((key.to_string(), clean.to_string()));
        }
    }
    allowed
}

/// Build safe CLI arguments for Codex CLI (`codex` or `codex resume`), keeping prompt
/// delivery strictly separate (Bridge types prompts into the Herdr terminal pane).
pub fn codex_arguments(
    model: Option<&str>,
    cwd: Option<&str>,
    resume: Option<&str>,
) -> Vec<String> {
    let resume_id = resume
        .map(str::trim)
        .filter(|id| is_safe_codex_session_id(id));
    let mut args = Vec::new();
    if resume_id.is_some() {
        args.push("resume".to_string());
    }
    if let Some(label) = model.map(str::trim).filter(|m| is_safe_codex_model(m)) {
        args.push("--model".to_string());
        args.push(label.to_string());
    }
    if let Some(dir) = cwd
        .map(str::trim)
        .filter(|d| !d.is_empty() && !d.contains(['\0', '\r', '\n']) && !d.starts_with('-'))
    {
        args.push("--cd".to_string());
        args.push(dir.to_string());
    }
    args.extend([
        "--dangerously-bypass-approvals-and-sandbox".to_string(),
        "--no-alt-screen".to_string(),
        "--no-daemon".to_string(),
    ]);
    if let Some(id) = resume_id {
        args.push(id.to_string());
    }
    args
}

impl AgentDriver for CodexDriver {
    fn id(&self) -> &'static str {
        CODEX_ID
    }

    fn title(&self) -> &'static str {
        CODEX_TITLE
    }

    fn traits(&self) -> DriverTraits {
        DriverTraits {
            session: SessionBinding::Reported,
            turns: TurnSource::Transcript,
            guard: Guard::Sandbox,
        }
    }

    fn launch(&self, home: &EngineHome, launch: &EngineLaunch<'_>) -> LaunchPlan {
        let model = launch.model.or(home.model.as_deref());
        let resume = launch.native.filter(|_| launch.resume);
        let codex_home = home.sessions.trim().trim_end_matches('/');
        let environment = if codex_home.is_empty() {
            Vec::new()
        } else {
            filter_codex_env(&[("CODEX_HOME", codex_home)])
        };
        LaunchPlan {
            program: home.program.clone(),
            arguments: codex_arguments(model, launch.cwd, resume),
            environment,
            settings: String::new(),
        }
    }

    fn new_session_id(&self, _execution: &ExecutionId) -> Option<String> {
        None
    }

    fn transcript_path(&self, home: &EngineHome, _workspace: &WorkspaceId, native: &str) -> String {
        let safe = if is_safe_codex_session_id(native) {
            native.to_string()
        } else {
            let cleaned: String = sanitize_worker_key(native)
                .chars()
                .map(|c| if c == '.' { '_' } else { c })
                .collect();
            if cleaned.is_empty() {
                "invalid".to_string()
            } else {
                cleaned
            }
        };
        let base = home.sessions.trim().trim_end_matches('/');
        format!("{base}/sessions/{safe}.jsonl")
    }

    fn reported_session(&self, line: &str) -> Option<String> {
        reported_session_from_line(line)
    }

    fn session_scan_dir(&self, home: &EngineHome) -> Option<String> {
        let base = home.sessions.trim().trim_end_matches('/');
        (!base.is_empty()).then(|| format!("{base}/sessions"))
    }

    fn read_line(&self, line: &str, cursor: &mut LineCursor) -> LineFacts {
        codex_line(line, cursor)
    }

    fn read_screen(&self, pane: &str) -> ScreenReading {
        read_codex_screen(pane)
    }

    fn startup_answer(&self, _pane: &str) -> Option<&'static [&'static str]> {
        None
    }

    fn excerpt_source(&self, pane: &str) -> String {
        mask_codex_pane(pane)
    }

    fn start_refusal(&self, pane: &str) -> Option<String> {
        let masked = mask_codex_pane(pane);
        let lines = tail_non_empty_lines(&masked);
        lines
            .into_iter()
            .find(|line| {
                let lower = line.to_lowercase();
                CODEX_START_REFUSALS
                    .iter()
                    .any(|needle| lower.contains(needle))
            })
            .map(|line| {
                let trimmed: String = line.trim().chars().take(MAX_EVIDENCE_CHARS).collect();
                format!("codex refused to start: {trimmed}")
            })
    }

    fn dialog_rules(&self) -> &'static [DialogRule] {
        CODEX_DIALOG_RULES
    }

    fn hard_block_rules(&self) -> &'static [HardBlockRule] {
        CODEX_HARD_BLOCK_RULES
    }
}

impl CodexDriver {
    /// Parse a version from `codex --version` output (`codex-cli 0.171.0`).
    pub fn version(answer: &str) -> Option<String> {
        answer
            .split_whitespace()
            .map(|word| word.trim_start_matches('v'))
            .find(|word| {
                word.contains('.')
                    && word
                        .split('.')
                        .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
            })
            .map(str::to_string)
    }

    /// Whether `version`'s `major.minor` matches [`CODEX_TESTED`].
    pub fn version_tested(version: &str) -> bool {
        let mut parts = version.split('.');
        let (Some(major), Some(minor)) = (parts.next(), parts.next()) else {
            return false;
        };
        CODEX_TESTED.contains(&format!("{major}.{minor}").as_str())
    }
}

/// Extract a reported native Codex session id from a JSONL line or plain pane line.
fn reported_session_from_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        let kind = value.get("type").and_then(Value::as_str);
        let candidate = match kind {
            Some("thread.started") => value
                .get("thread_id")
                .or_else(|| value.get("session_id"))
                .or_else(|| value.pointer("/payload/id"))
                .and_then(Value::as_str),
            Some("session_meta") => value
                .pointer("/payload/id")
                .or_else(|| value.get("session_id"))
                .or_else(|| value.get("thread_id"))
                .and_then(Value::as_str),
            Some("session.started") => value
                .get("session_id")
                .or_else(|| value.get("thread_id"))
                .or_else(|| value.pointer("/payload/id"))
                .and_then(Value::as_str),
            _ => None,
        };
        return candidate
            .filter(|id| is_safe_codex_session_id(id))
            .map(str::to_string);
    }

    let plain = strip_ansi(line);
    let lower = plain.to_lowercase();
    for needle in ["session id:", "thread id:", "codex resume "] {
        if let Some(at) = lower.find(needle) {
            let after = &plain[at + needle.len()..];
            if let Some(token) = after.split_whitespace().next() {
                let id = token.trim_end_matches([',', '.', ';', ')', ']', '"', '\'']);
                if is_plain_reported_token(id) {
                    return Some(id.to_string());
                }
            }
        }
    }
    None
}

fn is_plain_reported_token(token: &str) -> bool {
    is_safe_codex_session_id(token)
        && token.len() >= MIN_PLAIN_SESSION_LEN
        && (token.contains('-') || token.bytes().any(|b| b.is_ascii_digit()))
}

fn keyed(kind: EventKind, source_event_id: String) -> Normalized {
    Normalized {
        kind,
        source_event_id: Some(source_event_id),
    }
}

fn fnv1a_hex(text: &str) -> String {
    let digest = text.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{digest:016x}")
}

fn clean_error(raw: &str) -> String {
    redact(raw).chars().take(MAX_ERROR_CHARS).collect()
}

fn reading(state: ScreenState, evidence: &str) -> ScreenReading {
    ScreenReading {
        state,
        evidence: evidence.trim().chars().take(MAX_EVIDENCE_CHARS).collect(),
    }
}

/// Parse one Codex JSONL line (either `codex exec --json` or interactive rollout schema)
/// into normalized Spawn domain events.
pub fn codex_line(line: &str, cursor: &mut LineCursor) -> LineFacts {
    cursor.lines = cursor.lines.saturating_add(1);
    let Ok(value) = serde_json::from_str::<Value>(line.trim()) else {
        return LineFacts::default();
    };
    let line_key = format!("codex:{}:{}", cursor.lines, fnv1a_hex(line));
    let mut facts = LineFacts::default();
    match value.get("type").and_then(Value::as_str) {
        Some("session_meta") => {
            facts.version = value
                .pointer("/payload/cli_version")
                .or_else(|| value.get("cli_version"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty() && v.len() <= 64)
                .map(str::to_string);
        }
        Some("item.started") => {
            if let Some(event) = parse_exec_item_started(&value) {
                facts.events.push(event);
            }
        }
        Some("item.completed") => {
            if let Some(event) = parse_exec_item_completed(&value, &line_key) {
                facts.events.push(event);
            }
        }
        Some("turn.completed") => {
            parse_exec_turn_completed(&value, &line_key, &mut facts.events);
        }
        Some("turn.failed" | "error") => {
            facts.events.push(parse_turn_failed(&value, &line_key));
        }
        Some("event_msg") => {
            parse_rollout_event_msg(&value, &line_key, &mut facts.events);
        }
        Some("response_item") => {
            parse_rollout_response_item(&value, &line_key, &mut facts.events);
        }
        _ => {}
    }
    facts
}

fn parse_exec_item_started(value: &Value) -> Option<Normalized> {
    let item = value.get("item")?;
    let id = item
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    let item_type = item.get("type").and_then(Value::as_str)?;
    let (tool_name, input) = match item_type {
        "command_execution" => (
            "Bash".to_string(),
            item.get("command")
                .and_then(Value::as_str)
                .map(|cmd| json!({ "command": cmd })),
        ),
        "file_change" | "apply_patch" => {
            let path = item
                .get("path")
                .or_else(|| item.get("file_path"))
                .and_then(Value::as_str)
                .or_else(|| {
                    item.get("changes")
                        .and_then(Value::as_array)
                        .and_then(|arr| arr.first())
                        .and_then(|c| c.get("path"))
                        .and_then(Value::as_str)
                });
            ("Edit".to_string(), path.map(|p| json!({ "file_path": p })))
        }
        "mcp_tool_call" => {
            let name = item
                .get("tool")
                .or_else(|| item.get("name"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("mcp")
                .to_string();
            (name, decode_args_value(item.get("arguments")))
        }
        "web_search" => (
            "WebSearch".to_string(),
            item.get("query")
                .and_then(Value::as_str)
                .map(|q| json!({ "query": q })),
        ),
        _ => return None,
    };
    Some(keyed(
        EventKind::ToolStarted {
            summary: tool_summary(&tool_name, input.as_ref()),
            tool_name,
            tool_use_id: id.to_string(),
            summary_whole: false,
        },
        format!("{id}:started"),
    ))
}

fn parse_exec_item_completed(value: &Value, line_key: &str) -> Option<Normalized> {
    let item = value.get("item")?;
    let item_type = item.get("type").and_then(Value::as_str)?;
    if item_type == "error" {
        let msg = item
            .get("message")
            .or_else(|| item.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("codex item error");
        let err_id = item
            .get("id")
            .and_then(Value::as_str)
            .map_or_else(|| format!("{line_key}:err"), |id| format!("{id}:err"));
        return Some(keyed(
            EventKind::TurnFailed {
                error: clean_error(msg),
                status: None,
                error_kind: None,
            },
            err_id,
        ));
    }
    if !matches!(
        item_type,
        "command_execution" | "file_change" | "apply_patch" | "mcp_tool_call" | "web_search"
    ) {
        return None;
    }
    let id = item
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    let status = item.get("status").and_then(Value::as_str);
    let exit_code = item.get("exit_code").and_then(Value::as_i64);
    let err_text = item
        .pointer("/error/message")
        .or_else(|| item.get("error"))
        .and_then(Value::as_str)
        .or_else(|| item.get("aggregated_output").and_then(Value::as_str));
    let failed = status == Some("failed")
        || exit_code.is_some_and(|code| code != 0)
        || item.get("error").is_some_and(|e| !e.is_null());
    let event = if failed {
        let reason = err_text.unwrap_or("codex tool failed");
        EventKind::ToolFailed {
            tool_use_id: id.to_string(),
            error: clean_error(reason),
        }
    } else {
        EventKind::ToolCompleted {
            tool_use_id: id.to_string(),
            duration_ms: item.get("duration_ms").and_then(Value::as_u64),
        }
    };
    Some(keyed(event, format!("{id}:ended")))
}

fn parse_exec_turn_completed(value: &Value, line_key: &str, events: &mut Vec<Normalized>) {
    let usage = value.get("usage");
    let count = |key: &str| usage.and_then(|u| u.get(key)).and_then(Value::as_u64);
    let input = count("input_tokens");
    let output = count("output_tokens");
    let cached = count("cached_input_tokens").unwrap_or(0);
    if let (Some(in_tok), Some(out_tok), Some(at_ms)) = (
        input,
        output,
        value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(utc_ms),
    ) && (in_tok > 0 || out_tok > 0 || cached > 0)
    {
        let model = value
            .get("model")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
            .unwrap_or(CODEX_ID);
        let msg_id = format!("{line_key}:usage");
        events.push(keyed(
            EventKind::ModelUsage {
                message_id: msg_id.clone(),
                model: model.to_string(),
                at_ms,
                input_tokens: in_tok,
                output_tokens: out_tok,
                cache_write_tokens: 0,
                cache_read_tokens: cached,
            },
            msg_id,
        ));
    }
    events.push(keyed(
        EventKind::TurnStopped {
            input_tokens: input,
            output_tokens: output,
        },
        format!("{line_key}:stopped"),
    ));
}

fn parse_turn_failed(value: &Value, line_key: &str) -> Normalized {
    let raw = value
        .pointer("/error/message")
        .or_else(|| value.get("message"))
        .or_else(|| value.get("error"))
        .and_then(Value::as_str)
        .unwrap_or("codex turn failed");
    let status = value
        .pointer("/error/status")
        .or_else(|| value.get("status"))
        .and_then(Value::as_i64);
    let error_kind = value
        .pointer("/error/code")
        .or_else(|| value.pointer("/error/type"))
        .or_else(|| value.get("error_kind"))
        .and_then(Value::as_str)
        .map(str::to_string);
    keyed(
        EventKind::TurnFailed {
            error: clean_error(raw),
            status,
            error_kind,
        },
        format!("{line_key}:failed"),
    )
}

fn parse_rollout_event_msg(value: &Value, line_key: &str, events: &mut Vec<Normalized>) {
    let Some(payload) = value.get("payload") else {
        return;
    };
    match payload.get("type").and_then(Value::as_str) {
        Some("user_message") => {
            let text = payload
                .get("message")
                .or_else(|| payload.get("text"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            push_user_message_events(text, line_key, events);
        }
        Some("task_complete" | "turn_complete") => {
            let input_tokens = payload.get("input_tokens").and_then(Value::as_u64);
            let output_tokens = payload.get("output_tokens").and_then(Value::as_u64);
            events.push(keyed(
                EventKind::TurnStopped {
                    input_tokens,
                    output_tokens,
                },
                format!("{line_key}:stopped"),
            ));
        }
        Some("turn_aborted") => {
            let reason = payload
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("turn aborted");
            let lower = reason.to_lowercase();
            if lower.contains("interrupt") || lower.contains("cancel") {
                events.push(keyed(
                    EventKind::TurnStopped {
                        input_tokens: None,
                        output_tokens: None,
                    },
                    format!("{line_key}:aborted"),
                ));
            } else {
                events.push(keyed(
                    EventKind::TurnFailed {
                        error: clean_error(reason),
                        status: None,
                        error_kind: Some("turn_aborted".to_string()),
                    },
                    format!("{line_key}:aborted"),
                ));
            }
        }
        Some("error" | "stream_error") => {
            let raw = payload
                .get("message")
                .or_else(|| payload.get("error"))
                .and_then(Value::as_str)
                .unwrap_or("codex stream error");
            events.push(keyed(
                EventKind::TurnFailed {
                    error: clean_error(raw),
                    status: payload.get("status").and_then(Value::as_i64),
                    error_kind: payload
                        .get("error_kind")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                },
                format!("{line_key}:error"),
            ));
        }
        Some("token_count") => parse_rollout_token_count(value, payload, line_key, events),
        _ => {}
    }
}

fn parse_rollout_token_count(
    value: &Value,
    payload: &Value,
    line_key: &str,
    events: &mut Vec<Normalized>,
) {
    let Some(usage) = payload
        .pointer("/info/last_token_usage")
        .or_else(|| payload.get("last_token_usage"))
    else {
        return;
    };
    let Some(at_ms) = value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(utc_ms)
    else {
        return;
    };
    let input = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = usage
        .get("cached_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if (input, output, cached) == (0, 0, 0) {
        return;
    }
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| !m.is_empty())
        .unwrap_or(CODEX_ID);
    let msg_id = format!("{line_key}:tokens");
    events.push(keyed(
        EventKind::ModelUsage {
            message_id: msg_id.clone(),
            model: model.to_string(),
            at_ms,
            input_tokens: input,
            output_tokens: output,
            cache_write_tokens: 0,
            cache_read_tokens: cached,
        },
        msg_id,
    ));
}

fn parse_rollout_response_item(value: &Value, line_key: &str, events: &mut Vec<Normalized>) {
    let Some(payload) = value.get("payload") else {
        return;
    };
    match payload.get("type").and_then(Value::as_str) {
        Some("message") if payload.get("role").and_then(Value::as_str) == Some("user") => {
            let text = extract_content_text(payload.get("content"));
            push_user_message_events(&text, line_key, events);
        }
        Some(kind @ ("function_call" | "custom_tool_call" | "local_shell_call")) => {
            let Some(call_id) = payload
                .get("call_id")
                .or_else(|| payload.get("id"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            else {
                return;
            };
            let raw_name = if kind == "local_shell_call" {
                "local_shell_call"
            } else {
                payload
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("unknown")
            };
            let tool_name = normalize_codex_tool_name(raw_name);
            let input = extract_call_input(kind, payload);
            events.push(keyed(
                EventKind::ToolStarted {
                    summary: tool_summary(tool_name, input.as_ref()),
                    tool_name: tool_name.to_string(),
                    tool_use_id: call_id.to_string(),
                    summary_whole: false,
                },
                format!("{call_id}:started"),
            ));
        }
        Some("function_call_output" | "custom_tool_call_output") => {
            let Some(call_id) = payload
                .get("call_id")
                .or_else(|| payload.get("id"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            else {
                return;
            };
            let event = classify_call_output(call_id, payload);
            events.push(keyed(event, format!("{call_id}:ended")));
        }
        _ => {}
    }
}

fn push_user_message_events(text: &str, line_key: &str, events: &mut Vec<Normalized>) {
    events.push(keyed(
        EventKind::MessageReceived,
        format!("{line_key}:received"),
    ));
    for command_id in markers(text) {
        events.push(keyed(
            EventKind::MessageTaken { command_id },
            format!("{line_key}:taken:{}", command_id.0),
        ));
    }
}

fn extract_content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .or_else(|| item.get("input_text"))
                    .and_then(Value::as_str)
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn normalize_codex_tool_name(raw: &str) -> &str {
    match raw {
        "shell" | "shell_command" | "exec_command" | "local_shell_call" => "Bash",
        "apply_patch" => "Edit",
        other => other,
    }
}

fn decode_args_value(raw: Option<&Value>) -> Option<Value> {
    match raw? {
        obj @ Value::Object(_) => Some(obj.clone()),
        Value::String(text) => serde_json::from_str::<Value>(text)
            .ok()
            .filter(Value::is_object),
        _ => None,
    }
}

fn extract_call_input(kind: &str, payload: &Value) -> Option<Value> {
    if kind == "local_shell_call"
        && let Some(parts) = payload.pointer("/action/command").and_then(Value::as_array)
    {
        let words: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
        if !words.is_empty() {
            return Some(json!({ "command": words.join(" ") }));
        }
    }
    let parsed = decode_args_value(payload.get("arguments").or_else(|| payload.get("input")))?;
    if let Some(cmd) = parsed
        .get("cmd")
        .or_else(|| parsed.get("command"))
        .and_then(Value::as_str)
    {
        return Some(json!({ "command": cmd }));
    }
    if let Some(parts) = parsed.get("command").and_then(Value::as_array) {
        let words: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
        if !words.is_empty() {
            return Some(json!({ "command": words.join(" ") }));
        }
    }
    Some(parsed)
}

fn classify_call_output(call_id: &str, payload: &Value) -> EventKind {
    let explicit_err = payload.get("is_error").and_then(Value::as_bool) == Some(true);
    let output_val = payload.get("output");
    let parsed_output = output_val.and_then(|v| match v {
        Value::Object(_) => Some(v.clone()),
        Value::String(s) => serde_json::from_str::<Value>(s).ok(),
        _ => None,
    });
    let exit_nonzero = parsed_output
        .as_ref()
        .and_then(|o| {
            o.pointer("/metadata/exit_code")
                .or_else(|| o.get("exit_code"))
                .and_then(Value::as_i64)
        })
        .is_some_and(|code| code != 0);
    let status_failed = parsed_output
        .as_ref()
        .and_then(|o| o.get("status").and_then(Value::as_str))
        .is_some_and(|s| matches!(s, "failed" | "error"));
    if explicit_err || exit_nonzero || status_failed {
        let raw_err = parsed_output
            .as_ref()
            .and_then(|o| {
                o.get("output")
                    .or_else(|| o.get("error"))
                    .and_then(Value::as_str)
            })
            .or_else(|| output_val.and_then(Value::as_str))
            .unwrap_or("codex tool failed");
        EventKind::ToolFailed {
            tool_use_id: call_id.to_string(),
            error: clean_error(raw_err),
        }
    } else {
        EventKind::ToolCompleted {
            tool_use_id: call_id.to_string(),
            duration_ms: payload.get("duration_ms").and_then(Value::as_u64),
        }
    }
}

/// Mask e-mail account lines and API-key-bearing lines from Codex terminal excerpts.
fn mask_codex_pane(pane: &str) -> String {
    let no_email = drop_agy_account_lines(pane);
    let mut kept = String::with_capacity(no_email.len());
    for line in no_email.lines() {
        let stripped = strip_ansi(line);
        if contains_sensitive_key_line(&stripped) {
            continue;
        }
        kept.push_str(line);
        kept.push('\n');
    }
    kept
}

fn contains_sensitive_key_line(line: &str) -> bool {
    let lower = line.to_lowercase();
    if has_key_assignment(&lower, "openai_api_key") {
        return true;
    }
    // Check token-boundary `sk-...` secrets (`sk-` preceded by start/delimiter and
    // followed by at least 8 key characters) so normal hyphenated words (`disk-usage`) match nothing.
    let bytes = line.as_bytes();
    for (idx, window) in bytes.windows(3).enumerate() {
        if window != b"sk-" {
            continue;
        }
        let boundary_ok = idx == 0 || !bytes[idx - 1].is_ascii_alphanumeric();
        let rest_len = bytes[idx + 3..]
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            .count();
        if boundary_ok && rest_len >= 8 {
            return true;
        }
    }
    false
}

fn has_key_assignment(lower: &str, needle: &str) -> bool {
    let mut rest = lower;
    while let Some(at) = rest.find(needle) {
        let after = rest[at + needle.len()..].trim_start_matches([' ', '\t']);
        if after.starts_with('=') || after.starts_with(':') {
            return true;
        }
        rest = &rest[at + needle.len()..];
    }
    false
}

fn tail_non_empty_lines(pane: &str) -> Vec<String> {
    let all: Vec<String> = pane
        .lines()
        .map(|line| strip_ansi(line).trim_end().to_string())
        .collect();
    let end = all
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .map_or(0, |at| at + 1);
    let start = end.saturating_sub(TAIL_LINES);
    all[start..end]
        .iter()
        .filter(|line| !line.trim().is_empty())
        .cloned()
        .collect()
}

/// Whether the pane shows Codex's interactive directory trust prompt with no prompt below.
pub fn is_codex_trust_dialog(pane: &str) -> bool {
    let lines = tail_non_empty_lines(pane);
    let bottom = &lines[lines.len().saturating_sub(10)..];
    let text = bottom.join("\n").to_lowercase();
    let asks_trust = text.contains("do you trust the contents of this directory")
        || text.contains("trust this directory");
    let has_yes = text.contains("1. yes");
    asks_trust && has_yes && find_codex_prompt(&lines).is_none()
}

/// Read the live Codex terminal tail deterministically into [`ScreenReading`].
pub fn read_codex_screen(pane: &str) -> ScreenReading {
    let masked = mask_codex_pane(pane);
    let lines = tail_non_empty_lines(&masked);
    if lines.is_empty() {
        return ScreenReading::unknown();
    }
    if let Some(perm) = detect_permission_prompt(&lines) {
        return reading(ScreenState::Permission, perm);
    }
    if is_codex_trust_dialog(&masked) {
        return ScreenReading::unknown();
    }
    if let Some(busy) = detect_busy_tail(&lines) {
        return reading(ScreenState::Busy, busy);
    }
    let Some((prompt_idx, body)) = find_codex_prompt(&lines) else {
        return ScreenReading::unknown();
    };
    if let Some(busy_line) = activity_before(&lines, prompt_idx)
        .iter()
        .find(|line| is_codex_busy_line(line))
    {
        return reading(ScreenState::Busy, busy_line);
    }
    if let Some(err_line) = activity_before(&lines, prompt_idx)
        .iter()
        .rev()
        .find(|line| is_codex_error_line(line))
        && is_codex_idle_body(body)
    {
        return reading(ScreenState::Error, err_line);
    }
    if is_codex_idle_body(body) {
        reading(ScreenState::Ready, &lines[prompt_idx])
    } else {
        reading(ScreenState::Drafted, &lines[prompt_idx])
    }
}

fn detect_permission_prompt(lines: &[String]) -> Option<&str> {
    let bottom = &lines[lines.len().saturating_sub(8)..];
    let joined = bottom.join("\n").to_lowercase();
    if let Some(confirm) = bottom.iter().rev().find(|line| {
        line.to_lowercase()
            .contains("press enter to confirm or esc to cancel")
    }) {
        return Some(confirm.as_str());
    }
    let asks = joined.contains("allow command?")
        || joined.contains("approve this command?")
        || joined.contains("would you like to run");
    let has_numbered = bottom.iter().any(|line| {
        let trimmed = line.trim_start_matches([' ', '▌', '›', '>']);
        trimmed.trim_start().starts_with("1.")
    });
    if asks && has_numbered {
        return bottom.last().map(String::as_str);
    }
    None
}

fn detect_busy_tail(lines: &[String]) -> Option<&str> {
    let bottom = &lines[lines.len().saturating_sub(4)..];
    bottom
        .iter()
        .rev()
        .find(|line| is_codex_busy_line(line))
        .map(String::as_str)
}

fn is_codex_busy_line(line: &str) -> bool {
    let lower = line.trim().to_lowercase();
    if lower.contains("esc to interrupt")
        || lower.starts_with("• working")
        || lower.starts_with("• thinking")
        || lower.starts_with("• running")
    {
        return true;
    }
    line.trim()
        .chars()
        .next()
        .is_some_and(|ch| ('\u{2800}'..='\u{28ff}').contains(&ch))
}

fn is_codex_error_line(line: &str) -> bool {
    let trimmed = line.trim();
    let lower = trimmed.to_lowercase();
    trimmed.starts_with('■')
        || lower.starts_with("error:")
        || lower.contains("api error")
        || lower.contains("stream error")
        || lower.contains("rate limit")
        || lower.contains("unexpected status")
}

fn activity_before(lines: &[String], prompt_idx: usize) -> &[String] {
    let start = prompt_idx.saturating_sub(4);
    &lines[start..prompt_idx]
}

fn find_codex_prompt(lines: &[String]) -> Option<(usize, &str)> {
    let has_footer_below = |idx: usize| {
        lines[idx + 1..].iter().any(|line| {
            let lower = line.to_lowercase();
            CODEX_FOOTER_HINTS.iter().any(|hint| lower.contains(hint))
        })
    };
    for (idx, line) in lines.iter().enumerate().rev() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix('›')
            && (rest.is_empty() || rest.starts_with(char::is_whitespace))
            && has_footer_below(idx)
        {
            return Some((idx, rest.trim()));
        }
        if let Some(rest) = trimmed.strip_prefix('>')
            && (rest.is_empty() || rest.starts_with(char::is_whitespace))
            && has_footer_below(idx)
        {
            return Some((idx, rest.trim()));
        }
    }
    None
}

fn is_codex_idle_body(body: &str) -> bool {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return true;
    }
    let lower = trimmed.to_lowercase();
    CODEX_PLACEHOLDERS
        .iter()
        .any(|hint| lower.starts_with(hint))
}
