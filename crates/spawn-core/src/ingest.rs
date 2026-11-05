//! What engines write, turned into [`EventKind`]s.
//!
//! The Bridge follows the files an engine writes anyway, through the
//! terminal host, so nothing depends on the engine reporting to Spawn:
//!
//! Claude Code's session transcript, one JSON object per line
//! ([`claude_line`]).
//!
//! Lines that carry no Spawn meaning give nothing, so a new engine release
//! adds no failure mode. Events carry a key the engine made unique (the
//! line's `uuid`, the `tool_use_id`), so reading a file again appends
//! nothing twice.

use serde_json::Value;

use crate::event::EventKind;

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
    let (hours, minutes) = (hours.parse::<i64>().ok()?, minutes.parse::<i64>().ok()?);
    if !(0..=23).contains(&hours) || !(0..=59).contains(&minutes) {
        return None;
    }
    Some((text, sign * (hours * 60 + minutes) * 60_000))
}

/// Milliseconds since the epoch of an RFC 3339 time such as
/// `2026-11-04T05:46:12.345Z` (the form Claude Code writes) or one with a
/// numeric offset, `2026-11-04T13:46:12.345+08:00`.
///
/// Total: any text that is not such a time, a field out of its range
/// (years 1 to 9999), gives `None`, never a panic or a wrapped value; the
/// text may come from a file an engine writes.
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
    if !(1..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&second)
    {
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

/// The events in one Claude Code transcript line. Which lines are an
/// execution's is decided by where they are in the file (the Bridge reads
/// from the transcript's length when the execution started), never by the
/// time a line claims: that time is written by the engine.
///
/// - a prompt (a user line that is not a tool result) starts a turn;
/// - each `tool_use` block of an assistant line starts a tool, and each
///   `tool_result` block of a user line ends one, failed when `is_error`;
/// - `system`/`turn_duration`, written when a turn ends, and an interrupt
///   stop the turn; an API error line fails it.
///
/// Sub-agent (`isSidechain`) lines are the sub-agent's, not the session's.
pub fn claude_line(line: &str) -> Vec<Normalized> {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let field = |name: &str| value.get(name).and_then(Value::as_str);
    if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
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

/// The engine version a Claude Code transcript line names (its `version`
/// field). Like [`claude_line`], the caller reads only the execution's part
/// of the file.
pub fn claude_version(line: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    let version = value.get("version").and_then(Value::as_str)?.trim();
    (!version.is_empty() && version.len() <= 64).then(|| version.to_string())
}

#[cfg(test)]
mod tests {
    use super::{claude_line, claude_version, utc_ms};

    #[test]
    fn a_line_names_the_engine_version_it_was_written_by() {
        let line = r#"{"type":"user","uuid":"u1","timestamp":"2026-11-04T05:46:12.345Z","version":"2.1.318","message":{"content":"x"}}"#;
        assert_eq!(claude_version(line).as_deref(), Some("2.1.318"));
        assert_eq!(claude_version(r#"{"type":"mode","version":""}"#), None);
        assert_eq!(claude_version(r#"{"type":"mode"}"#), None);
        assert_eq!(claude_version("not json"), None);
    }
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
    fn a_line_is_kept_whatever_time_it_claims() {
        for timestamp in [
            None,
            Some("yesterday"),
            Some("1970-01-01T00:00:00.000Z"),
            Some("99999999999999999-11-04T05:46:12.000Z"),
            Some("2026-11-04T99999999999999999:46:12.000Z"),
        ] {
            let at = timestamp.map_or(String::new(), |at| format!(r#""timestamp":"{at}","#));
            let line = format!(r#"{{"type":"user","uuid":"u9",{at}"message":{{"content":"x"}}}}"#);
            assert_eq!(claude_line(&line).len(), 1, "{timestamp:?}");
        }
    }

    #[test]
    fn utc_ms_is_total() {
        for text in [
            "99999999999999999-11-04T05:46:12.000Z",
            "2026-11-04T99999999999999999:46:12.000Z",
            "2026-11-04T05:99999999999999999:12.000Z",
            "2026-11-04T05:46:99999999999999999.000Z",
            "-99999999999999999-11-04T05:46:12.000Z",
            "10000-01-01T00:00:00.000Z",
            "0-01-01T00:00:00.000Z",
            "2026-11-04T24:00:00.000Z",
            "2026-11-04T05:60:00.000Z",
            "2026-11-04T05:46:12.000+99999999999999999:00",
            "2026-11-04T05:46:12.000+24:00",
        ] {
            assert_eq!(utc_ms(text), None, "{text}");
        }
        // Every byte string of a few shapes: never a panic.
        let pieces = [
            "9",
            "99999999999999999999",
            "-",
            "T",
            ":",
            ".",
            "Z",
            "+",
            "0",
        ];
        for a in pieces {
            for b in pieces {
                for c in pieces {
                    let _ = utc_ms(&format!("{a}-{b}-{c}T{a}:{b}:{c}.{a}Z"));
                    let _ = utc_ms(&format!("2026-01-01T00:00:00{a}{b}:{c}"));
                }
            }
        }
        assert_eq!(
            utc_ms("9999-12-31T23:59:60.999Z"),
            Some(253_402_300_800_999)
        );
    }

    fn kinds(line: &str) -> Vec<(String, Option<String>)> {
        claude_line(line)
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
        let event = claude_line(&failed).remove(0);
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
            claude_line(&error)[0].kind,
            EventKind::TurnFailed {
                error: "API Error: 529 overloaded".into()
            }
        );
    }

    #[test]
    fn old_side_meta_and_unknown_lines_give_nothing() {
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
}
