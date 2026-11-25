//! `fleet` and `tail`: read-only views of the log, as JSON.

use spawn_core::id::SessionId;

use crate::Flags;
use crate::config::open_store;

/// The usage of `spawn fleet`, the only copy (main's help reads it).
pub(crate) const FLEET_USAGE: &str = "usage: spawn fleet\n      \
                                      (every session the login may read, with its judgement, as JSON)";

/// The usage of `spawn tail`, the only copy (main's help reads it).
pub(crate) const TAIL_USAGE: &str = "usage: spawn tail --session <session id> [--events N]\n      \
                                     (one session's latest judgement, newest events, judgements and\n       \
                                     commands, as JSON; --events: 1 to 200, default 20)";

/// The fleet as JSON: one object per session the login may read.
pub(crate) fn fleet() -> Result<i32, String> {
    let store = open_store()?;
    let fleet = store.fleet_json().map_err(|error| error.to_string())?;
    println!(
        "{}",
        serde_json::to_string_pretty(&fleet).unwrap_or_else(|_| "[]".to_string())
    );
    Ok(0)
}

/// One session's latest judgement and newest events as JSON.
pub(crate) fn tail(flags: &Flags) -> Result<i32, String> {
    let store = open_store()?;
    let session = SessionId::new(flags.required("session")?).map_err(|error| error.to_string())?;
    let events = flags
        .get("events")
        .and_then(|text| text.parse::<i64>().ok())
        .unwrap_or(20)
        .clamp(1, 200);
    let tail = store
        .tail_json(&session, events)
        .map_err(|error| error.to_string())?;
    println!(
        "{}",
        serde_json::to_string_pretty(&tail).unwrap_or_else(|_| "{}".to_string())
    );
    Ok(0)
}
