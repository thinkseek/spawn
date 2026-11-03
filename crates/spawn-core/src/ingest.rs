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

use std::borrow::Cow;
use std::fmt;
use std::marker::PhantomData;

use serde::Deserialize;
use serde::de::{Deserializer, Visitor};
use serde_json::Value;
use thiserror::Error;

use crate::event::{Engine, EventKind};

mod scan;

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

/// A JSON string borrowed from the payload when it has no escapes.
///
/// `Cow<str>` alone always allocates when deserialized inside an `Option`;
/// this wrapper borrows wherever the input allows, so fields that are only
/// matched or ignored cost no allocation.
#[derive(Debug)]
struct Str<'a>(Cow<'a, str>);

impl Str<'_> {
    fn as_str(&self) -> &str {
        &self.0
    }

    fn into_string(self) -> String {
        self.0.into_owned()
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for Str<'a> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct StrVisitor<'a>(PhantomData<Str<'a>>);

        impl<'de: 'a, 'a> Visitor<'de> for StrVisitor<'a> {
            type Value = Str<'a>;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a string")
            }

            fn visit_borrowed_str<E: serde::de::Error>(
                self,
                text: &'de str,
            ) -> Result<Self::Value, E> {
                Ok(Str(Cow::Borrowed(text)))
            }

            fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<Self::Value, E> {
                Ok(Str(Cow::Owned(text.to_owned())))
            }

            fn visit_string<E: serde::de::Error>(self, text: String) -> Result<Self::Value, E> {
                Ok(Str(Cow::Owned(text)))
            }
        }

        deserializer.deserialize_str(StrVisitor(PhantomData))
    }
}

fn owned(value: Option<Str<'_>>) -> Option<String> {
    value.map(Str::into_string)
}

#[derive(Debug, Deserialize)]
struct ClaudeHook<'a> {
    #[serde(borrow)]
    hook_event_name: Str<'a>,
    #[serde(borrow)]
    session_id: Option<Str<'a>>,
    #[serde(borrow)]
    transcript_path: Option<Str<'a>>,
    #[serde(borrow)]
    tool_name: Option<Str<'a>>,
    #[serde(borrow)]
    tool_use_id: Option<Str<'a>>,
    duration_ms: Option<u64>,
    error: Option<Value>,
    #[serde(borrow)]
    notification_type: Option<Str<'a>>,
    #[serde(borrow)]
    reason: Option<Str<'a>>,
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
    let hook = match scan::claude(payload) {
        Some(hook) => hook,
        None => serde_json::from_str(payload)?,
    };
    let name = hook.hook_event_name.as_str();
    // The tool key is `<tool_use_id>:<suffix>`; the id is required for every
    // event that has a key, so it is checked before the key is built.
    let tool_id = |suffix: &str| -> Result<(String, Option<String>), IngestError> {
        let id = required(hook.tool_use_id.as_ref(), name, "tool_use_id")?.as_str();
        // Built by hand: `format!` costs more than the rest of the mapping.
        let mut key = String::with_capacity(id.len() + 1 + suffix.len());
        key.push_str(id);
        key.push(':');
        key.push_str(suffix);
        Ok((id.to_owned(), Some(key)))
    };
    let (kind, source_event_id) = match name {
        "SessionStart" => (
            EventKind::NativeSessionStarted {
                engine: Engine::Claude,
                native_session_id: required(hook.session_id, name, "session_id")?.into_string(),
                transcript_path: owned(hook.transcript_path),
            },
            None,
        ),
        "SessionEnd" => (
            EventKind::NativeSessionEnded {
                reason: owned(hook.reason),
            },
            None,
        ),
        "UserPromptSubmit" => (EventKind::MessageReceived, None),
        "Notification" => (
            EventKind::NotificationReceived {
                notification_type: hook
                    .notification_type
                    .map_or_else(|| "unknown".to_string(), Str::into_string),
            },
            None,
        ),
        "PreToolUse" => {
            let tool_name = required(hook.tool_name.as_ref(), name, "tool_name")?
                .as_str()
                .to_owned();
            let (tool_use_id, key) = tool_id("started")?;
            (
                EventKind::ToolStarted {
                    tool_name,
                    tool_use_id,
                },
                key,
            )
        }
        "PostToolUse" => {
            let (tool_use_id, key) = tool_id("completed")?;
            (
                EventKind::ToolCompleted {
                    tool_use_id,
                    duration_ms: hook.duration_ms,
                },
                key,
            )
        }
        "PostToolUseFailure" => {
            let (tool_use_id, key) = tool_id("failed")?;
            (
                EventKind::ToolFailed {
                    tool_use_id,
                    error: error_text(hook.error.as_ref()),
                },
                key,
            )
        }
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
struct CodexLine<'a> {
    #[serde(rename = "type", borrow)]
    line_type: Str<'a>,
    #[serde(borrow)]
    thread_id: Option<Str<'a>>,
    usage: Option<CodexUsage>,
    error: Option<Value>,
    #[serde(borrow)]
    message: Option<Str<'a>>,
    #[serde(borrow)]
    item: Option<CodexItem<'a>>,
}

#[derive(Debug, Deserialize)]
struct CodexUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct CodexItem<'a> {
    #[serde(borrow)]
    id: Str<'a>,
    #[serde(rename = "type", borrow)]
    item_type: Str<'a>,
    #[serde(borrow)]
    status: Option<Str<'a>>,
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

fn codex_error(error: Option<&Value>, message: Option<&str>) -> String {
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

fn codex_item(line_type: &str, item: CodexItem<'_>) -> Option<EventKind> {
    if !CODEX_TOOL_ITEMS.contains(&item.item_type.as_str()) {
        return None;
    }
    match line_type {
        "item.started" => Some(EventKind::ToolStarted {
            tool_name: item.item_type.into_string(),
            tool_use_id: item.id.into_string(),
        }),
        "item.completed" if item.status.as_ref().map(Str::as_str) == Some("failed") => {
            Some(EventKind::ToolFailed {
                tool_use_id: item.id.into_string(),
                error: codex_error(item.error.as_ref(), None),
            })
        }
        "item.completed" => Some(EventKind::ToolCompleted {
            tool_use_id: item.id.into_string(),
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
    let parsed = match scan::codex(line) {
        Some(parsed) => parsed,
        None => serde_json::from_str(line)?,
    };
    let name = parsed.line_type.as_str();
    let kind = match name {
        "thread.started" => Some(EventKind::NativeSessionStarted {
            engine: Engine::Codex,
            native_session_id: required(parsed.thread_id, name, "thread_id")?.into_string(),
            transcript_path: None,
        }),
        "turn.started" => Some(EventKind::MessageReceived),
        "turn.completed" => Some(EventKind::TurnStopped {
            input_tokens: parsed.usage.as_ref().and_then(|usage| usage.input_tokens),
            output_tokens: parsed.usage.as_ref().and_then(|usage| usage.output_tokens),
        }),
        "turn.failed" | "error" => Some(EventKind::TurnFailed {
            error: codex_error(
                parsed.error.as_ref(),
                parsed.message.as_ref().map(Str::as_str),
            ),
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
