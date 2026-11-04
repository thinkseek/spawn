//! `status`, `fleet` and `tail`: read-only views of the log, as text or JSON.

use spawn_core::event::EventKind;
use spawn_core::id::SessionId;

use crate::Flags;
use crate::config::open_store;

/// `spawn status`: the latest observation per session.
pub(crate) fn status(_flags: &Flags) -> Result<i32, String> {
    let store = open_store()?;
    for state in store.states().map_err(|error| error.to_string())? {
        let kind = EventKind::ObservationRecorded {
            observation: state.observation,
        };
        let line = serde_json::to_string(&kind).unwrap_or_else(|_| "{}".to_string());
        println!("{}\t{line}", state.session_id);
    }
    Ok(0)
}

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
