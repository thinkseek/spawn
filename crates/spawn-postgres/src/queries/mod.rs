//! Every SQL statement this crate sends, except the RSI controller's
//! (`crate::rsi`), grouped by responsibility.
//!
//! The Rust modules next to this one bind parameters and map rows; the text
//! here is what reaches the database. Writes call the SQL functions in
//! `sql/schema.sql`, reads go through its views and grant-filtered
//! `spawn_read_*` functions.

/// Columns of `spawn_events` that make one `EventRecord`
/// (see [`crate::row::event_record`]).
macro_rules! event_record_columns {
    () => {
        "id, agent_id, session_id, execution_id, source, source_event_id,
                        kind, session_seq, recorded_at_ms"
    };
}

/// Columns of `agent_state` that make one `SessionState`.
macro_rules! session_state_columns {
    () => {
        "session_id, agent_id, observation, session_seq, event_id, recorded_at_ms"
    };
}

pub(crate) mod commands;
pub(crate) mod events;
pub(crate) mod fleet;
pub(crate) mod gateway;
pub(crate) mod missions;
pub(crate) mod sessions;
