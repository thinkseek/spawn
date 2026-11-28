//! Column decoders and parameter conversions shared by every reader, so each
//! identifier and position is decoded one way.
//!
//! A value the domain type rejects (malformed shape, invalid name, or corrupt
//! known-event payload) is a corrupt row, not a backend failure. An event whose
//! writer this build does not know (a source since removed), or whose `"name"`
//! is a valid dotted ASCII event name rejected by serde as an unknown variant
//! (such as retired `#113` or future expand-contract rows), is no corruption:
//! [`event_record`] skips it with one warning per name.

use std::collections::BTreeSet;
use std::sync::Mutex;

use postgres::Row;
use spawn_core::event::{EventKind, EventRecord, GlobalPosition, NewEvent, SessionSeq, Source};
use spawn_core::id::{AgentId, CommandId, ExecutionId, SessionId};
use spawn_core::port::StoreError;
use spawn_core::time::Timestamp;

pub(crate) fn corrupt(error: impl std::fmt::Display) -> StoreError {
    StoreError::Corrupt(error.to_string())
}

/// A count, limit or position as a `bigint` parameter, saturating at the top.
pub(crate) fn bigint(value: impl TryInto<i64>) -> i64 {
    value.try_into().unwrap_or(i64::MAX)
}

pub(crate) fn session_seq(row: &Row, column: &str) -> SessionSeq {
    SessionSeq(row.get::<_, i64>(column).unsigned_abs())
}

pub(crate) fn global_position(row: &Row, column: &str) -> GlobalPosition {
    GlobalPosition(row.get::<_, i64>(column).unsigned_abs())
}

pub(crate) fn command_id(row: &Row, column: &str) -> CommandId {
    CommandId(row.get::<_, i64>(column).unsigned_abs())
}

pub(crate) fn agent_id(row: &Row, column: &str) -> Result<AgentId, StoreError> {
    AgentId::new(row.get::<_, String>(column)).map_err(corrupt)
}

pub(crate) fn session_id(row: &Row, column: &str) -> Result<SessionId, StoreError> {
    SessionId::new(row.get::<_, String>(column)).map_err(corrupt)
}

/// The nullable `execution_id` column.
pub(crate) fn execution_id(row: &Row) -> Result<Option<ExecutionId>, StoreError> {
    row.get::<_, Option<String>>("execution_id")
        .map(ExecutionId::new)
        .transpose()
        .map_err(corrupt)
}

/// Source names this process has warned it skips.
static SKIPPED_SOURCES: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// Event names retired from [`EventKind`] (#113 item 4) that may still
/// exist in historical `spawn_events` rows or R2 archives. Kept for export and
/// active/retired disjointness guards; decoding skips them through the single
/// unknown dotted event path in [`decode_event_kind`].
pub const RETIRED_EVENTS: &[&str] = &[
    "session.native_ended",
    "notice.typed",
    "notice.received",
    "notification.received",
];

/// Unknown event names (retired or future expand-contract) this process has warned it skips.
static SKIPPED_EVENT_KINDS: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// Whether `name` is a non-empty dotted ASCII event name: at least two
/// non-empty segments separated by `.`, each containing only lowercase ASCII
/// letters, digits, or `_`.
fn is_valid_dotted_event_name(name: &str) -> bool {
    let mut segments = 0usize;
    for segment in name.split('.') {
        if segment.is_empty()
            || !segment
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return false;
        }
        segments += 1;
    }
    segments >= 2
}

/// Decode a `spawn_events.kind` JSON value via [`EventKind`]'s serde
/// deserializer first. Only if serde rejects it as an unknown variant with a
/// valid dotted lowercase ASCII `"name"` and `!EventKind::is_known_name(name)`
/// is it skipped once and returned as `Ok(None)`. All malformed shapes, invalid
/// `"name"` strings, and corrupt payloads for known events return [`StoreError::Corrupt`].
fn decode_event_kind(
    kind_value: &serde_json::Value,
    position: i64,
) -> Result<Option<EventKind>, StoreError> {
    match serde_json::from_value::<EventKind>(kind_value.clone()) {
        Ok(kind) => Ok(Some(kind)),
        Err(err) => {
            if let Some(name) = kind_value
                .as_object()
                .and_then(|object| object.get("name"))
                .and_then(serde_json::Value::as_str)
            {
                let unknown_prefix = format!("unknown variant `{name}`, ");
                if err.to_string().starts_with(&unknown_prefix)
                    && is_valid_dotted_event_name(name)
                    && !EventKind::is_known_name(name)
                {
                    let first = SKIPPED_EVENT_KINDS
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(name.to_string());
                    if first {
                        eprintln!(
                            "spawn: skipping unknown event kind {name}, which this build does not read \
                             (the first at position {position})"
                        );
                    }
                    return Ok(None);
                }
            }
            Err(corrupt(err))
        }
    }
}

/// One `spawn_events` row selected with `event_record_columns!`, or `None`
/// for an event whose source or valid dotted unknown event name this build skips:
/// the history may hold writers or event kinds from newer or removed releases,
/// and one of their records must not stop every reader of the log. The source
/// is checked first, then `kind` is deserialized serde-first into [`EventKind`].
pub(crate) fn event_record(row: &Row) -> Result<Option<EventRecord>, StoreError> {
    let position_raw: i64 = row.get("id");
    let text: String = row.get("source");
    let Some(source) = Source::parse(&text) else {
        let first = SKIPPED_SOURCES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(text.clone());
        if first {
            eprintln!(
                "spawn: skipping events from source {text}, which this build does not read \
                 (the first at position {position_raw})"
            );
        }
        return Ok(None);
    };
    let kind_value: serde_json::Value = row.try_get("kind").map_err(corrupt)?;
    let Some(kind) = decode_event_kind(&kind_value, position_raw)? else {
        return Ok(None);
    };
    Ok(Some(EventRecord {
        position: global_position(row, "id"),
        session_seq: session_seq(row, "session_seq"),
        recorded_at: Timestamp(row.get("recorded_at_ms")),
        event: NewEvent {
            agent_id: agent_id(row, "agent_id")?,
            session_id: session_id(row, "session_id")?,
            execution_id: execution_id(row)?,
            source,
            source_event_id: row.get("source_event_id"),
            kind,
        },
    }))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use spawn_core::event::EventKind;
    use spawn_core::port::StoreError;

    use super::{RETIRED_EVENTS, decode_event_kind, is_valid_dotted_event_name};

    #[test]
    fn valid_dotted_event_name_checks_segments_and_ascii() {
        for good in ["future.v99_event", "a.b", "a.b.c_1", "session.native_ended"] {
            assert!(is_valid_dotted_event_name(good), "{good} should be valid");
        }
        for bad in [
            "",
            "nodot",
            ".leading",
            "trailing.",
            "a..b",
            "Future.Event",
            "bad-name.event",
            "BAD NAME!",
            "future.event!",
        ] {
            assert!(!is_valid_dotted_event_name(bad), "{bad} should be rejected");
        }
    }

    #[test]
    fn decode_event_kind_tolerates_unknown_dotted_and_fails_closed_on_corrupt() {
        let skipped = decode_event_kind(
            &json!({"name": "future.row_unit_event", "detail": {"k": 1}}),
            42,
        )
        .expect("unknown dotted event returns Ok");
        assert!(skipped.is_none());

        for retired in RETIRED_EVENTS {
            let skipped = decode_event_kind(&json!({"name": retired, "detail": {}}), 42)
                .expect("retired dotted event returns Ok via unified unknown path");
            assert!(skipped.is_none());
        }

        let known = decode_event_kind(&json!({"name": "message.received"}), 43)
            .expect("known event returns Ok")
            .expect("known event is Some");
        assert_eq!(known, EventKind::MessageReceived);

        let grading = decode_event_kind(
            &json!({"name": "grading.recorded", "detail": {"record": {"type": "preset.created"}}}),
            43,
        )
        .expect("grading event returns Ok")
        .expect("grading event is Some");
        assert_eq!(grading.wire_name(), "grading.recorded");

        for corrupt_kind in [
            json!(42),
            json!(["message.received"]),
            json!("message.received"),
            json!(null),
            json!({"detail": {}}),
            json!({"name": 123, "detail": {}}),
            json!({"name": ""}),
            json!({"name": "nodot"}),
            json!({"name": ".leading"}),
            json!({"name": "trailing."}),
            json!({"name": "a..b"}),
            json!({"name": "BAD NAME!"}),
            json!({"name": "message.typed", "detail": {"command_id": "not-a-number"}}),
            json!({"name": "tool.started", "detail": {}}),
            json!({"name": "grading.recorded", "detail": {}}),
            json!({"name": "job.started", "detail": {"job_id": "j1", "kind": "bogus_kind"}}),
            json!({"name": "job.started", "detail": {"job_id": "j1", "kind": "job.started"}}),
        ] {
            let decoded = decode_event_kind(&corrupt_kind, 44);
            assert!(
                matches!(decoded, Err(StoreError::Corrupt(_))),
                "expected StoreError::Corrupt for {corrupt_kind}, got {decoded:?}"
            );
        }
    }
}
