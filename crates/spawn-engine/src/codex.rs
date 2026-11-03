use serde::Deserialize;
use serde_json::Value;

use spawn_types::engine::Engine;
use spawn_types::event::EventKind;

use crate::{NormalizeError, NormalizedEvent};

/// One JSONL record emitted by `codex exec --json`.
///
/// Codex wraps every line in `{type, ...payload}`. Payload fields are kept
/// loose because the CLI adds fields between releases.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CodexStreamEvent {
    /// Top-level record discriminator.
    #[serde(rename = "type")]
    pub record_type: String,
    /// Remaining record fields.
    #[serde(flatten)]
    pub payload: Value,
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn item_id(payload: &Value) -> String {
    text(payload, "id")
        .or_else(|| payload.get("item").and_then(|item| text(item, "id")))
        .unwrap_or_else(|| "unknown_item".to_string())
}

fn item_name(payload: &Value) -> String {
    text(payload, "name")
        .or_else(|| payload.get("item").and_then(|item| text(item, "name")))
        .unwrap_or_else(|| "codex_item".to_string())
}

fn is_failed_item(payload: &Value) -> bool {
    let status = text(payload, "status")
        .or_else(|| payload.get("item").and_then(|item| text(item, "status")))
        .unwrap_or_default();
    status.eq_ignore_ascii_case("failed") || status.eq_ignore_ascii_case("error")
}

fn item_error(payload: &Value) -> (String, String) {
    let message = payload
        .get("error")
        .map_or_else(|| payload.to_string(), Value::to_string);
    ("codex_item_error".to_string(), message)
}

/// Map one Codex JSONL record to a unified event.
///
/// Records without a Spawn semantic equivalent (for example progress pings)
/// return [`NormalizeError::UnsupportedEvent`]; callers skip them explicitly.
///
/// # Errors
/// Propagates malformed required fields and unknown record types.
pub fn normalize_codex_event(event: &CodexStreamEvent) -> Result<NormalizedEvent, NormalizeError> {
    let payload = &event.payload;
    let kind = match event.record_type.as_str() {
        "thread.started" => EventKind::NativeSessionStarted {
            engine: Engine::Codex,
            native_session_id: text(payload, "thread_id")
                .or_else(|| text(payload, "id"))
                .ok_or(NormalizeError::MissingField("thread_id"))?,
            transcript_path: text(payload, "rollout_path"),
        },
        "thread.finished" | "thread.exited" => EventKind::NativeSessionEnded {
            end_reason: text(payload, "reason"),
        },
        "turn.started" => EventKind::MessageReceived {
            prompt_ref: text(payload, "input"),
        },
        "turn.completed" => EventKind::TurnStopped {
            input_tokens: payload
                .get("usage")
                .and_then(|usage| usage.get("input_tokens"))
                .and_then(Value::as_u64),
            output_tokens: payload
                .get("usage")
                .and_then(|usage| usage.get("output_tokens"))
                .and_then(Value::as_u64),
        },
        "turn.failed" => EventKind::TurnFailed {
            failure_kind: text(payload, "error_kind").unwrap_or_else(|| "turn_failed".into()),
        },
        "item.started" => EventKind::ToolStarted {
            tool_name: item_name(payload),
            tool_correlation_id: item_id(payload),
        },
        "item.completed" => {
            if is_failed_item(payload) {
                let (error_kind, error_message) = item_error(payload);
                EventKind::ToolFailed {
                    tool_correlation_id: item_id(payload),
                    error_kind,
                    error_message,
                }
            } else {
                EventKind::ToolCompleted {
                    tool_correlation_id: item_id(payload),
                    duration_ms: payload.get("duration_ms").and_then(Value::as_u64),
                }
            }
        }
        "error" => EventKind::TurnFailed {
            failure_kind: text(payload, "kind").unwrap_or_else(|| "stream_error".into()),
        },
        other => return Err(NormalizeError::UnsupportedEvent(other.to_string())),
    };

    let source_event_id = Some(format!("codex:{}:{}", event.record_type, item_id(payload)));
    Ok(NormalizedEvent {
        kind,
        source_event_id,
    })
}
