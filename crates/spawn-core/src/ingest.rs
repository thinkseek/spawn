//! What engines write, turned into [`EventKind`]s.
//!
//! The Bridge follows the files an engine writes anyway, through the
//! terminal host, so nothing depends on the engine reporting to Spawn:
//!
//! - Claude Code's session transcript, one JSON object per line
//!   ([`claude_line`]);
//! - Codex `exec --json` output, which the runner copies to a file
//!   ([`codex_line`]).
//!
//! Lines that carry no Spawn meaning give nothing, so a new engine release
//! adds no failure mode. Claude events carry a key the engine made unique
//! (the line's `uuid`, the `tool_use_id`), so reading a file again appends
//! nothing twice; Codex lines are keyed by their line number in a file that
//! belongs to one execution.

use serde::Deserialize;
use serde_json::Value;

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

fn truncate(text: &str) -> String {
    text.chars().take(MAX_ERROR_CHARS).collect()
}

fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => truncate(text),
        Value::Array(blocks) => truncate(
            &blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        Value::Null => String::new(),
        other => truncate(&other.to_string()),
    }
}

/// A time with a `+HH:MM`/`-HH:MM` zone: the time without it, and the
/// zone's offset from UTC in milliseconds.
fn numeric_zone(text: &str) -> Option<(&str, i64)> {
    let split = text.len().checked_sub(6)?;
    let (text, zone) = (text.get(..split)?, text.get(split..)?);
    let sign = match zone.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let (hours, minutes) = zone.get(1..)?.split_once(':')?;
    let minutes = hours.parse::<i64>().ok()? * 60 + minutes.parse::<i64>().ok()?;
    Some((text, sign * minutes * 60_000))
}

/// Milliseconds since the epoch of an RFC 3339 time such as
/// `2026-11-04T05:46:12.345Z` (the form Claude Code writes) or one with a
/// numeric offset, `2026-11-04T13:46:12.345+08:00`.
pub fn utc_ms(text: &str) -> Option<i64> {
    let (text, offset_ms) = match text.strip_suffix('Z') {
        Some(text) => (text, 0),
        None => numeric_zone(text)?,
    };
    let (date, time) = text.split_once('T')?;
    let mut date = date.splitn(3, '-').map(str::parse::<i64>);
    let (year, month, day) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let (clock, fraction) = time.split_once('.').unwrap_or((time, "0"));
    let mut clock = clock.splitn(3, ':').map(str::parse::<i64>);
    let (hour, minute, second) = (
        clock.next()?.ok()?,
        clock.next()?.ok()?,
        clock.next()?.ok()?,
    );
    let millis: i64 = format!("{fraction:0<3}").get(..3)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let shifted = if month <= 2 { year - 1 } else { year };
    let era = shifted.div_euclid(400);
    let year_of_era = shifted - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some((((days * 24 + hour) * 60 + minute) * 60 + second) * 1000 + millis - offset_ms)
}

fn block_type(block: &Value) -> Option<&str> {
    block.get("type").and_then(Value::as_str)
}

fn keyed(kind: EventKind, key: String) -> Normalized {
    Normalized {
        kind,
        source_event_id: Some(key),
    }
}

/// The tool ends in a user line's `tool_result` blocks.
fn tool_results(blocks: &[Value]) -> Vec<Normalized> {
    blocks
        .iter()
        .filter(|block| block_type(block) == Some("tool_result"))
        .filter_map(|block| {
            let id = block.get("tool_use_id").and_then(Value::as_str)?;
            Some(
                if block.get("is_error").and_then(Value::as_bool) == Some(true) {
                    keyed(
                        EventKind::ToolFailed {
                            tool_use_id: id.to_string(),
                            error: text_of(block.get("content").unwrap_or(&Value::Null)),
                        },
                        format!("{id}:failed"),
                    )
                } else {
                    keyed(
                        EventKind::ToolCompleted {
                            tool_use_id: id.to_string(),
                            duration_ms: None,
                        },
                        format!("{id}:completed"),
                    )
                },
            )
        })
        .collect()
}

/// What a user line that is not a tool result means: a prompt, an
/// interrupt (which stops the turn), or a slash command (nothing).
fn user_text(content: &Value, uuid: &str) -> Vec<Normalized> {
    let text = text_of(content);
    let text = text.trim_start();
    if text.starts_with("[Request interrupted") {
        vec![keyed(
            EventKind::TurnStopped {
                input_tokens: None,
                output_tokens: None,
            },
            uuid.to_string(),
        )]
    } else if text.is_empty()
        || text.starts_with("<command-")
        || text.starts_with("<local-command-")
    {
        Vec::new()
    } else {
        vec![keyed(EventKind::MessageReceived, uuid.to_string())]
    }
}

/// The events in one Claude Code transcript line written at or after
/// `since_ms` (the start of the execution being followed): a resumed
/// session's transcript holds the earlier executions' lines too.
///
/// - a prompt (a user line that is not a tool result) starts a turn;
/// - each `tool_use` block of an assistant line starts a tool, and each
///   `tool_result` block of a user line ends one, failed when `is_error`;
/// - `system`/`turn_duration`, written when a turn ends, and an interrupt
///   stop the turn; an API error line fails it.
///
/// Sub-agent (`isSidechain`) lines are the sub-agent's, not the session's.
pub fn claude_line(line: &str, since_ms: i64) -> Vec<Normalized> {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let field = |name: &str| value.get(name).and_then(Value::as_str);
    // A line older than the execution is an earlier execution's (a resumed
    // session appends to the same file). A line whose time cannot be read is
    // kept, not dropped: its `uuid` key stops a second append if it was.
    let written = field("timestamp").and_then(utc_ms);
    if written.is_some_and(|at| at < since_ms)
        || value.get("isSidechain").and_then(Value::as_bool) == Some(true)
    {
        return Vec::new();
    }
    let Some(uuid) = field("uuid") else {
        return Vec::new();
    };
    let content = value.pointer("/message/content").unwrap_or(&Value::Null);
    let blocks = content.as_array().map(Vec::as_slice).unwrap_or_default();
    match field("type") {
        Some("system") if field("subtype") == Some("turn_duration") => vec![keyed(
            EventKind::TurnStopped {
                input_tokens: None,
                output_tokens: None,
            },
            uuid.to_string(),
        )],
        Some("assistant")
            if value.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true) =>
        {
            vec![keyed(
                EventKind::TurnFailed {
                    error: text_of(content),
                },
                uuid.to_string(),
            )]
        }
        Some("assistant") => blocks
            .iter()
            .filter(|block| block_type(block) == Some("tool_use"))
            .filter_map(|block| {
                let id = block.get("id").and_then(Value::as_str)?;
                let name = block.get("name").and_then(Value::as_str)?;
                Some(keyed(
                    EventKind::ToolStarted {
                        tool_name: name.to_string(),
                        tool_use_id: id.to_string(),
                    },
                    format!("{id}:started"),
                ))
            })
            .collect(),
        Some("user")
            if blocks
                .iter()
                .any(|block| block_type(block) == Some("tool_result")) =>
        {
            tool_results(blocks)
        }
        Some("user") if value.get("isMeta").and_then(Value::as_bool) != Some(true) => {
            user_text(content, uuid)
        }
        _ => Vec::new(),
    }
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

fn codex_error(error: Option<&Value>, message: Option<&str>) -> String {
    match (error, message) {
        (Some(Value::Object(object)), _) => {
            object.get("message").and_then(Value::as_str).map_or_else(
                || truncate(&Value::Object(object.clone()).to_string()),
                truncate,
            )
        }
        (Some(other), _) => text_of(other),
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
                error: codex_error(item.error.as_ref(), None),
                tool_use_id: item.id,
            })
        }
        "item.completed" => Some(EventKind::ToolCompleted {
            tool_use_id: item.id,
            duration_ms: None,
        }),
        _ => None,
    }
}

/// The event in one Codex `exec --json` line, if it carries one.
pub fn codex_line(line: &str) -> Option<EventKind> {
    let parsed: CodexLine = serde_json::from_str(line.trim()).ok()?;
    match parsed.line_type.as_str() {
        "thread.started" => Some(EventKind::NativeSessionStarted {
            engine: Engine::Codex,
            native_session_id: parsed.thread_id?,
            transcript_path: None,
        }),
        "turn.started" => Some(EventKind::MessageReceived),
        "turn.completed" => Some(EventKind::TurnStopped {
            input_tokens: parsed.usage.as_ref().and_then(|usage| usage.input_tokens),
            output_tokens: parsed.usage.as_ref().and_then(|usage| usage.output_tokens),
        }),
        "turn.failed" | "error" => Some(EventKind::TurnFailed {
            error: codex_error(parsed.error.as_ref(), parsed.message.as_deref()),
        }),
        name @ ("item.started" | "item.completed") => codex_item(name, parsed.item?),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{claude_line, codex_line, utc_ms};
    use crate::event::EventKind;

    #[test]
    fn utc_times_parse_to_epoch_milliseconds() {
        assert_eq!(utc_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(utc_ms("2000-03-01T00:00:01.5Z"), Some(951_868_801_500));
        assert_eq!(utc_ms("2026-11-04T05:46:12.345Z"), Some(1_793_771_172_345));
        assert_eq!(utc_ms("2026-11-04 05:46:12"), None);
        // A numeric offset names the same instant.
        assert_eq!(
            utc_ms("2026-11-04T13:46:12.345+08:00"),
            Some(1_793_771_172_345)
        );
        assert_eq!(
            utc_ms("2026-11-04T00:46:12.345-05:00"),
            Some(1_793_771_172_345)
        );
        assert_eq!(
            utc_ms("2026-11-04T05:46:12.345+00:00"),
            Some(1_793_771_172_345)
        );
    }

    #[test]
    fn a_line_whose_time_cannot_be_read_is_kept() {
        let untimed = r#"{"type":"user","uuid":"u9","message":{"content":"fix it"}}"#;
        assert_eq!(
            claude_line(untimed, 1_793_771_172_346).len(),
            1,
            "kept, not judged older than the execution"
        );
        let odd =
            r#"{"type":"user","uuid":"u9","timestamp":"yesterday","message":{"content":"x"}}"#;
        assert_eq!(claude_line(odd, 1_793_771_172_346).len(), 1);
    }

    fn kinds(line: &str) -> Vec<(String, Option<String>)> {
        claude_line(line, 0)
            .into_iter()
            .map(|event| (event.kind.name().to_string(), event.source_event_id))
            .collect()
    }

    const AT: &str = r#""timestamp":"2026-11-04T05:46:12.345Z""#;

    #[test]
    fn a_transcript_gives_the_turn_and_its_tools() {
        let prompt = format!(
            r#"{{"type":"user","uuid":"u1",{AT},"message":{{"role":"user","content":"fix it"}}}}"#
        );
        assert_eq!(
            kinds(&prompt),
            vec![("message.received".into(), Some("u1".into()))]
        );
        let tool = format!(
            r#"{{"type":"assistant","uuid":"a1",{AT},"message":{{"content":[{{"type":"tool_use","id":"toolu_1","name":"Bash","input":{{}}}}],"stop_reason":"tool_use"}}}}"#
        );
        assert_eq!(
            kinds(&tool),
            vec![("tool.started".into(), Some("toolu_1:started".into()))]
        );
        let done = format!(
            r#"{{"type":"user","uuid":"u2",{AT},"message":{{"content":[{{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}}]}}}}"#
        );
        assert_eq!(
            kinds(&done),
            vec![("tool.completed".into(), Some("toolu_1:completed".into()))]
        );
        let failed = format!(
            r#"{{"type":"user","uuid":"u3",{AT},"message":{{"content":[{{"type":"tool_result","tool_use_id":"toolu_2","is_error":true,"content":[{{"type":"text","text":"exit 1"}}]}}]}}}}"#
        );
        let event = claude_line(&failed, 0).remove(0);
        assert_eq!(
            event.kind,
            EventKind::ToolFailed {
                tool_use_id: "toolu_2".into(),
                error: "exit 1".into()
            }
        );
        let end = format!(
            r#"{{"type":"system","subtype":"turn_duration","uuid":"s1",{AT},"durationMs":5}}"#
        );
        assert_eq!(
            kinds(&end),
            vec![("turn.stopped".into(), Some("s1".into()))]
        );
        let error = format!(
            r#"{{"type":"assistant","uuid":"a2",{AT},"isApiErrorMessage":true,"message":{{"content":[{{"type":"text","text":"API Error: 529 overloaded"}}]}}}}"#
        );
        assert_eq!(
            claude_line(&error, 0)[0].kind,
            EventKind::TurnFailed {
                error: "API Error: 529 overloaded".into()
            }
        );
    }

    #[test]
    fn old_side_meta_and_unknown_lines_give_nothing() {
        let prompt =
            format!(r#"{{"type":"user","uuid":"u1",{AT},"message":{{"content":"fix it"}}}}"#);
        assert!(
            claude_line(&prompt, 1_793_771_172_346).is_empty(),
            "before the execution"
        );
        let side = format!(
            r#"{{"type":"user","uuid":"u1",{AT},"isSidechain":true,"message":{{"content":"x"}}}}"#
        );
        assert!(kinds(&side).is_empty());
        let meta = format!(
            r#"{{"type":"user","uuid":"u1",{AT},"isMeta":true,"message":{{"content":"x"}}}}"#
        );
        assert!(kinds(&meta).is_empty());
        let command = format!(
            r#"{{"type":"user","uuid":"u1",{AT},"message":{{"content":"<command-name>/clear</command-name>"}}}}"#
        );
        assert!(kinds(&command).is_empty());
        assert!(kinds(r#"{"type":"mode","mode":"normal","sessionId":"x"}"#).is_empty());
        assert!(kinds("not json").is_empty());
        let interrupted = format!(
            r#"{{"type":"user","uuid":"u9",{AT},"message":{{"content":[{{"type":"text","text":"[Request interrupted by user]"}}]}}}}"#
        );
        assert_eq!(
            kinds(&interrupted),
            vec![("turn.stopped".into(), Some("u9".into()))]
        );
    }

    #[test]
    fn codex_lines_map_to_events() {
        let line = r#"{"type":"item.started","item":{"id":"i1","type":"command_execution"}}"#;
        assert!(matches!(
            codex_line(line),
            Some(EventKind::ToolStarted { .. })
        ));
        assert!(
            codex_line(r#"{"type":"item.started","item":{"id":"i2","type":"message"}}"#).is_none()
        );
        assert!(codex_line("  ").is_none());
        assert!(matches!(
            codex_line(r#"{"type":"thread.started","thread_id":"t1"}"#),
            Some(EventKind::NativeSessionStarted { .. })
        ));
    }
}
