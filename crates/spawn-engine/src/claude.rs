use serde::Deserialize;
use serde_json::Value;

use spawn_types::engine::Engine;
use spawn_types::event::EventKind;

use crate::{NormalizeError, NormalizedEvent};

/// Subset of the Claude Code hook input consumed by Spawn.
///
/// Unknown fields are ignored. Nested payloads stay as JSON because their
/// schema changes across Claude releases; only fields used by the mapping are
/// typed explicitly.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ClaudeHookInput {
    /// Hook discriminator such as `PreToolUse`.
    pub hook_event_name: String,
    /// Claude session identifier.
    pub session_id: Option<String>,
    /// Absolute transcript path.
    pub transcript_path: Option<String>,
    /// Working directory of the Claude process.
    pub cwd: Option<String>,
    /// Tool name for tool lifecycle hooks.
    pub tool_name: Option<String>,
    /// Raw tool input object.
    pub tool_input: Option<Value>,
    /// Raw tool response object.
    pub tool_response: Option<Value>,
    /// Reason supplied to session-end hooks.
    pub stop_hook_active: Option<bool>,
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn tool_correlation_id(input: &ClaudeHookInput) -> Option<String> {
    input
        .tool_input
        .as_ref()
        .and_then(|tool_input| string_field(tool_input, "tool_use_id"))
        .or_else(|| {
            input
                .tool_response
                .as_ref()
                .and_then(|response| string_field(response, "tool_use_id"))
        })
}

fn tool_failure_message(response: &Value) -> (String, String) {
    let kind = response
        .get("is_error")
        .map_or("tool_error", |_| "tool_error");
    let message = response
        .get("content")
        .and_then(Value::as_str)
        .map_or_else(|| response.to_string(), str::to_string);
    (kind.to_string(), message)
}

/// Map one Claude Code hook payload to a unified event.
///
/// # Errors
/// Returns [`NormalizeError::UnsupportedEvent`] for hooks Spawn does not map
/// and [`NormalizeError::MissingField`] when a mapped hook lacks required data.
pub fn normalize_claude_hook(input: &ClaudeHookInput) -> Result<NormalizedEvent, NormalizeError> {
    let correlation = || tool_correlation_id(input).unwrap_or_default();
    let make_id = |suffix: &str| {
        input
            .session_id
            .clone()
            .map(|session| format!("{session}:{suffix}"))
    };

    let kind = match input.hook_event_name.as_str() {
        "SessionStart" => EventKind::NativeSessionStarted {
            engine: Engine::ClaudeCode,
            native_session_id: input
                .session_id
                .clone()
                .ok_or(NormalizeError::MissingField("session_id"))?,
            transcript_path: input.transcript_path.clone(),
        },
        "SessionEnd" => EventKind::NativeSessionEnded {
            end_reason: input
                .transcript_path
                .clone()
                .map(|path| format!("session_end:{path}")),
        },
        "UserPromptSubmit" => EventKind::MessageReceived {
            prompt_ref: input.transcript_path.clone(),
        },
        "PreToolUse" => EventKind::ToolStarted {
            tool_name: input
                .tool_name
                .clone()
                .ok_or(NormalizeError::MissingField("tool_name"))?,
            tool_correlation_id: correlation(),
        },
        "PostToolUse" => EventKind::ToolCompleted {
            tool_correlation_id: correlation(),
            duration_ms: None,
        },
        "Notification" => EventKind::MessageReceived {
            prompt_ref: Some("permission_notification".to_string()),
        },
        "Stop" | "SubagentStop" => EventKind::TurnStopped {
            input_tokens: None,
            output_tokens: None,
        },
        "PostToolUseFailure" | "StopFailure" => {
            if input.hook_event_name == "StopFailure" {
                EventKind::TurnFailed {
                    failure_kind: "stop_failure".to_string(),
                }
            } else {
                let response = input.tool_response.clone().unwrap_or(Value::Null);
                let (error_kind, error_message) = tool_failure_message(&response);
                EventKind::ToolFailed {
                    tool_correlation_id: correlation(),
                    error_kind,
                    error_message,
                }
            }
        }
        other => return Err(NormalizeError::UnsupportedEvent(other.to_string())),
    };

    let source_event_id = match &kind {
        EventKind::NativeSessionStarted {
            native_session_id, ..
        } => Some(format!("claude:start:{native_session_id}")),
        EventKind::ToolStarted {
            tool_correlation_id,
            ..
        }
        | EventKind::ToolCompleted {
            tool_correlation_id,
            ..
        }
        | EventKind::ToolFailed {
            tool_correlation_id,
            ..
        } => Some(format!(
            "claude:tool:{tool_correlation_id}:{}",
            input.hook_event_name
        )),
        _ => make_id(&input.hook_event_name),
    };

    Ok(NormalizedEvent {
        kind,
        source_event_id,
    })
}
