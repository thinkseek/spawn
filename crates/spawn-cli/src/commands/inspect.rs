//! `fleet` and `tail`: read-only views of the log, as JSON.

use spawn_core::id::SessionId;

use crate::Flags;
use crate::config::open_store;

/// The fleet as JSON: one object per session the login may read, each live
/// execution with its resource envelope (its scope, its memory now, its
/// peak, its limits and its swap: objective 029) inside the slice's own.
pub(crate) fn fleet() -> Result<i32, String> {
    let store = open_store()?;
    let mut fleet = store.fleet_json().map_err(|error| error.to_string())?;
    // A bad envelope setting does not hide the fleet: the rows then show
    // what their cgroups say, without this process's settings.
    let envelopes = crate::envelope::Runtime::from_env().ok();
    let waits = store.admissions().map_err(|error| error.to_string())?;
    if let Some(agents) = fleet.as_array_mut() {
        crate::envelope::fleet(
            envelopes.as_ref().and_then(|runtime| runtime.envelope()),
            agents,
        );
        crate::envelope::admissions(agents, &waits);
    }
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
