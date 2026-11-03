//! Normalization of native engine events into [`EventKind`].
//!
//! Claude Code delivers one JSON object per hook on standard input. Codex
//! `exec --json` prints one JSON object per line. Events that carry no Spawn
//! meaning (assistant text, reasoning, progress updates, unknown hooks)
//! return `Ok(None)` so new engine releases never break ingestion.
//!
//! Source keys are set only where the engine supplies a globally unique id
//! (Claude `tool_use_id`). Turn and prompt events have no such id; giving them
//! a constant key would drop every event after the first.

use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

use crate::event::{Engine, EventKind};

/// Maximum characters kept from engine error text.
const MAX_ERROR_CHARS: usize = 2000;

/// Normalized engine event.
#[derive(Debug, Clone, PartialEq)]
pub struct Normalized {
    /// Unified event.
    pub kind: EventKind,
    /// Producer key for idempotent appends.
    pub source_event_id: Option<String>,
}

/// Malformed engine payload.
#[derive(Debug, Error)]
pub enum IngestError {
    /// The payload is not valid JSON or has a wrong field type.
    #[error("invalid payload: {0}")]
    Json(#[from] serde_json::Error),
    /// A field that the event requires is missing.
    #[error("{event} payload lacks `{field}`")]
    MissingField {
        /// Native event name.
        event: String,
        /// Missing field.
        field: &'static str,
    },
}

fn truncate(text: &str) -> String {
    text.chars().take(MAX_ERROR_CHARS).collect()
}

fn required<T>(value: Option<T>, event: &str, field: &'static str) -> Result<T, IngestError> {
    value.ok_or_else(|| IngestError::MissingField {
        event: event.to_string(),
        field,
    })
}

#[derive(Debug, Deserialize)]
struct ClaudeHook {
    hook_event_name: String,
    session_id: Option<String>,
    transcript_path: Option<String>,
    tool_name: Option<String>,
    tool_use_id: Option<String>,
    duration_ms: Option<u64>,
    error: Option<Value>,
    notification_type: Option<String>,
    reason: Option<String>,
}

fn error_text(error: Option<&Value>) -> String {
    match error {
        Some(Value::String(text)) => truncate(text),
        Some(other) => truncate(&other.to_string()),
        None => String::new(),
    }
}

/// Map one Claude Code hook payload.
pub fn claude_hook(payload: &str) -> Result<Option<Normalized>, IngestError> {
    let hook: ClaudeHook = serde_json::from_str(payload)?;
    let name = hook.hook_event_name.as_str();
    let tool_key = |suffix: &str| hook.tool_use_id.as_ref().map(|id| format!("{id}:{suffix}"));
    let (kind, source_event_id) = match name {
        "SessionStart" => (
            EventKind::NativeSessionStarted {
                engine: Engine::Claude,
                native_session_id: required(hook.session_id.clone(), name, "session_id")?,
                transcript_path: hook.transcript_path.clone(),
            },
            None,
        ),
        "SessionEnd" => (
            EventKind::NativeSessionEnded {
                reason: hook.reason.clone(),
            },
            None,
        ),
        "UserPromptSubmit" => (EventKind::MessageReceived, None),
        "Notification" => (
            EventKind::NotificationReceived {
                notification_type: hook
                    .notification_type
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string()),
            },
            None,
        ),
        "PreToolUse" => (
            EventKind::ToolStarted {
                tool_name: required(hook.tool_name.clone(), name, "tool_name")?,
                tool_use_id: required(hook.tool_use_id.clone(), name, "tool_use_id")?,
            },
            tool_key("started"),
        ),
        "PostToolUse" => (
            EventKind::ToolCompleted {
                tool_use_id: required(hook.tool_use_id.clone(), name, "tool_use_id")?,
                duration_ms: hook.duration_ms,
            },
            tool_key("completed"),
        ),
        "PostToolUseFailure" => (
            EventKind::ToolFailed {
                tool_use_id: required(hook.tool_use_id.clone(), name, "tool_use_id")?,
                error: error_text(hook.error.as_ref()),
            },
            tool_key("failed"),
        ),
        "Stop" => (
            EventKind::TurnStopped {
                input_tokens: None,
                output_tokens: None,
            },
            None,
        ),
        "StopFailure" => (
            EventKind::TurnFailed {
                error: error_text(hook.error.as_ref()),
            },
            None,
        ),
        _ => return Ok(None),
    };
    Ok(Some(Normalized {
        kind,
        source_event_id,
    }))
}

#[derive(Debug, Deserialize)]
struct CodexLine {
    #[serde(rename = "type")]
    line_type: String,
    thread_id: Option<String>,
    usage: Option<CodexUsage>,
    error: Option<Value>,
    message: Option<String>,
    item: Option<CodexItem>,
}

#[derive(Debug, Deserialize)]
struct CodexUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct CodexItem {
    id: String,
    #[serde(rename = "type")]
    item_type: String,
    status: Option<String>,
    error: Option<Value>,
}

/// Codex item types that are tool calls. Messages, reasoning and plan
/// updates are not tools.
const CODEX_TOOL_ITEMS: [&str; 4] = [
    "command_execution",
    "file_change",
    "mcp_tool_call",
    "web_search",
];

fn codex_error(error: Option<&Value>, message: Option<&String>) -> String {
    match (error, message) {
        (Some(Value::Object(object)), _) => {
            object.get("message").and_then(Value::as_str).map_or_else(
                || truncate(&Value::Object(object.clone()).to_string()),
                truncate,
            )
        }
        (Some(other), _) => error_text(Some(other)),
        (None, Some(text)) => truncate(text),
        (None, None) => String::new(),
    }
}

fn codex_item(line_type: &str, item: CodexItem) -> Option<EventKind> {
    if !CODEX_TOOL_ITEMS.contains(&item.item_type.as_str()) {
        return None;
    }
    match line_type {
        "item.started" => Some(EventKind::ToolStarted {
            tool_name: item.item_type,
            tool_use_id: item.id,
        }),
        "item.completed" if item.status.as_deref() == Some("failed") => {
            Some(EventKind::ToolFailed {
                tool_use_id: item.id,
                error: codex_error(item.error.as_ref(), None),
            })
        }
        "item.completed" => Some(EventKind::ToolCompleted {
            tool_use_id: item.id,
            duration_ms: None,
        }),
        _ => None,
    }
}

/// Map one Codex `exec --json` line. Blank lines return `Ok(None)`.
pub fn codex_line(line: &str) -> Result<Option<Normalized>, IngestError> {
    if line.trim().is_empty() {
        return Ok(None);
    }
    let parsed: CodexLine = serde_json::from_str(line)?;
    let name = parsed.line_type.as_str();
    let kind = match name {
        "thread.started" => Some(EventKind::NativeSessionStarted {
            engine: Engine::Codex,
            native_session_id: required(parsed.thread_id.clone(), name, "thread_id")?,
            transcript_path: None,
        }),
        "turn.started" => Some(EventKind::MessageReceived),
        "turn.completed" => Some(EventKind::TurnStopped {
            input_tokens: parsed.usage.as_ref().and_then(|usage| usage.input_tokens),
            output_tokens: parsed.usage.as_ref().and_then(|usage| usage.output_tokens),
        }),
        "turn.failed" | "error" => Some(EventKind::TurnFailed {
            error: codex_error(parsed.error.as_ref(), parsed.message.as_ref()),
        }),
        "item.started" | "item.completed" => {
            let item = required(parsed.item, name, "item")?;
            codex_item(name, item)
        }
        _ => None,
    };
    Ok(kind.map(|kind| Normalized {
        kind,
        source_event_id: None,
    }))
}
