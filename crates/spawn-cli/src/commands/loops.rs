//! `tick`, `loop` and `rsi-loop`: one pass, the resident loop, and the RSI
//! controller's own loop.

use spawn_postgres::PostgresStore;

use crate::config::{
    env_value, loop_interval, open_store, spawn_program, supervised, workspace_root,
};
use crate::flags::Flags;
use crate::runtime::runtime;
use crate::{dispatch, evolve, supervise, trace};

/// Most intervals a failing loop waits before its next pass.
const MAX_BACKOFF_INTERVALS: u32 = 30;

/// Pause after `failures` consecutive failures: one interval per failure,
/// at least one, at most [`MAX_BACKOFF_INTERVALS`].
fn backoff(interval: std::time::Duration, failures: u32) -> std::time::Duration {
    interval.saturating_mul(failures.clamp(1, MAX_BACKOFF_INTERVALS))
}

/// `spawn tick`: one Bridge and Observer pass.
pub(crate) fn tick(flags: &Flags) -> Result<i32, String> {
    let (mut bridge, mut observer) = runtime(flags)?;
    let report = trace::time("bridge.tick", || bridge.tick()).map_err(|error| error.to_string())?;
    for warning in &report.warnings {
        eprintln!("spawn tick warning: {warning}");
    }
    let observations =
        trace::time("observer.tick", || observer.tick()).map_err(|error| error.to_string())?;
    println!(
        "commands={} observations={} sessions={}",
        report.commands,
        observations,
        bridge.session_count()
    );
    Ok(0)
}

/// Run the Bridge, the Observer and the RSI controller until killed.
///
/// The loop holds one connection and one in-memory cursor, so its cost does
/// not grow with the event log. A transient store error is logged and
/// retried with a growing pause. The one exit is a halt from the RSI
/// controller, which means three rejections in a row; that is a decision for
/// a human, so the process stops instead of spinning.
pub(crate) fn resident_loop(flags: &Flags) -> Result<i32, String> {
    let interval = loop_interval();
    let (mut bridge, mut observer) = runtime(flags)?;
    let supervised = supervised();
    let dispatching = env_value("SPAWN_DISPATCH").is_none_or(|value| value != "0");
    let spawn = spawn_program();
    let mut dispatch_store: Option<PostgresStore> = None;
    let workspace_root = workspace_root(flags).map(std::path::PathBuf::from);
    let mut failures: u32 = 0;
    loop {
        match bridge.tick() {
            Ok(report) => {
                failures = 0;
                for warning in &report.warnings {
                    eprintln!("spawn loop warning: {warning}");
                }
            }
            Err(error) => {
                failures = failures.saturating_add(1);
                eprintln!("spawn loop: bridge: {error}");
            }
        }
        if let Err(error) = observer.tick() {
            eprintln!("spawn loop: observer: {error}");
        }
        if dispatching {
            if dispatch_store.is_none() {
                dispatch_store = open_store()
                    .map_err(|error| eprintln!("spawn loop: dispatch: {error}"))
                    .ok();
            }
            if let Some(store) = &dispatch_store {
                match dispatch::step(store, supervised, &spawn, workspace_root.as_deref()) {
                    Ok(lines) => lines.iter().for_each(|line| println!("{line}")),
                    Err(error) => {
                        // A broken connection is reopened on the next step.
                        dispatch_store = None;
                        failures = failures.saturating_add(1);
                        eprintln!("spawn loop: dispatch: {error}");
                    }
                }
            }
        }
        if let Some(store) = &dispatch_store {
            match supervise::keep_residents(store) {
                Ok(lines) => lines.iter().for_each(|line| println!("{line}")),
                Err(error) => eprintln!("spawn loop: residents: {error}"),
            }
        }
        if supervised {
            match supervise::keep_alive() {
                Ok(Some(line)) => println!("{line}"),
                Ok(None) => {}
                Err(error) => {
                    failures = failures.saturating_add(1);
                    eprintln!("spawn loop: supervise: {error}");
                }
            }
        }
        // ponytail: fixed backoff, cap at 30 intervals. A per-error-kind
        // schedule only matters once a failure class is slow to clear.
        std::thread::sleep(backoff(interval, failures));
    }
}

/// `spawn evolve`: one RSI step; idle counts as success.
pub(crate) fn evolve_once(flags: &Flags) -> Result<i32, String> {
    evolve::evolve(flags).map(|code| if code == evolve::IDLE { 0 } else { code })
}

/// Run the RSI controller until it halts.
///
/// RSI is one workload of the fleet, not the fleet: it runs in its own
/// process, so a 20-minute gate does not stall the Bridge, and its halt (exit
/// 2, a decision for a human) stops only the RSI loop, never the supervisor.
pub(crate) fn rsi_loop(flags: &Flags) -> Result<i32, String> {
    let interval = loop_interval();
    let mut failures: u32 = 0;
    loop {
        match evolve::evolve(flags) {
            Ok(2) => {
                eprintln!("spawn rsi-loop: RSI halted; a person must clear it");
                return Ok(2);
            }
            // Nothing to do until the backlog changes; look again in a minute.
            Ok(evolve::IDLE) => {
                failures = 0;
                std::thread::sleep(std::time::Duration::from_mins(1));
                continue;
            }
            Ok(_) => failures = 0,
            Err(error) => {
                failures = failures.saturating_add(1);
                eprintln!("spawn rsi-loop: evolve: {error}");
            }
        }
        std::thread::sleep(backoff(interval, failures));
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::backoff;

    #[test]
    fn backoff_waits_one_interval_per_failure_between_one_and_thirty() {
        let interval = Duration::from_millis(100);
        assert_eq!(backoff(interval, 0), interval);
        assert_eq!(backoff(interval, 3), interval * 3);
        assert_eq!(backoff(interval, 500), interval * 30);
    }
}
