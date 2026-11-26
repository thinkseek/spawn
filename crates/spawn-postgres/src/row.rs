//! Column decoders and parameter conversions shared by every reader, so each
//! identifier and position is decoded one way.
//!
//! A value the domain type rejects is a corrupt row, not a backend failure.
//! An event whose writer this build does not know (a source since removed)
//! is no corruption: [`event_record`] skips it, with one warning per name.

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
/// exist in historical `spawn_events` rows or R2 archives. [`event_record`]
/// skips them cleanly with one warning per name instead of deleting rows or
/// stalling readers; unlisted unknown names remain [`StoreError::Corrupt`].
pub const RETIRED_EVENTS: &[&str] = &[
    "session.native_ended",
    "notice.typed",
    "notice.received",
    "notification.received",
];

/// Retired event names this process has warned it skips.
static SKIPPED_EVENTS: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// One `spawn_events` row selected with `event_record_columns!`, or `None`
/// for an event whose source or retired name this build skips: the history
/// may hold writers or event kinds since removed, and one of their records
/// must not stop every reader of the log. The source is read first, then
/// retired event names are checked before deserializing [`EventKind`].
pub(crate) fn event_record(row: &Row) -> Result<Option<EventRecord>, StoreError> {
    let text: String = row.get("source");
    let Some(source) = Source::parse(&text) else {
        let first = SKIPPED_SOURCES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(text.clone());
        if first {
            eprintln!(
                "spawn: skipping events from source {text}, which this build does not read \
                 (the first at position {})",
                row.get::<_, i64>("id")
            );
        }
        return Ok(None);
    };
    let kind_value: serde_json::Value = row.get("kind");
    if let Some(name) = kind_value.get("name").and_then(serde_json::Value::as_str)
        && RETIRED_EVENTS.contains(&name)
    {
        let first = SKIPPED_EVENTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(name.to_string());
        if first {
            eprintln!(
                "spawn: skipping retired event kind {name}, which this build does not read \
                 (the first at position {})",
                row.get::<_, i64>("id")
            );
        }
        return Ok(None);
    }
    let kind: EventKind = serde_json::from_value(kind_value).map_err(corrupt)?;
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
