//! `tick` and `loop`: one pass, and the resident loop. Landing and RSI
//! objectives run in `spawn integrate` (crate::integrate).

use spawn_postgres::PostgresStore;

use crate::Flags;
use crate::config::{
    env_value, loop_interval, open_store, spawn_program, supervised, workspace_root,
};
use crate::runtime::runtime;
use crate::{dispatch, supervise, trace};

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

/// Consecutive failed passes after which the loop exits, so its unit
/// restarts it with fresh connections (the store does not reconnect).
const MAX_FAILURES: u32 = 10;

/// Run the Bridge, the Observer, the dispatcher and the liveness rule.
///
/// The loop holds its connections and one in-memory cursor, so its cost does
/// not grow with the event log. A failed pass is logged and retried with a
/// growing pause; the dispatcher's connection is reopened. After
/// [`MAX_FAILURES`] failed passes in a row the loop exits with an error and
/// systemd starts it again.
pub(crate) fn resident_loop(flags: &Flags) -> Result<i32, String> {
    let interval = loop_interval();
    let (mut bridge, mut observer) = runtime(flags)?;
    let supervised = supervised();
    let dispatching = env_value("SPAWN_DISPATCH").is_none_or(|value| value != "0");
    let spawn = spawn_program();
    let mut store: Option<PostgresStore> = None;
    let workspace_root = workspace_root(flags).map(std::path::PathBuf::from);
    let mut failures: u32 = 0;
    loop {
        let mut failed = false;
        let mut fail = |what: &str, error: &str| {
            failed = true;
            eprintln!("spawn loop: {what}: {error}");
        };
        match bridge.tick() {
            Ok(report) => {
                for warning in &report.warnings {
                    eprintln!("spawn loop warning: {warning}");
                }
            }
            Err(error) => fail("bridge", &error.to_string()),
        }
        if let Err(error) = observer.tick() {
            fail("observer", &error.to_string());
        }
        if store.is_none() {
            store = open_store().map_err(|error| fail("store", &error)).ok();
        }
        if let Some(open) = &store {
            let mut lines = Vec::new();
            let mut step = |what: &str, result: Result<Vec<String>, String>| match result {
                Ok(more) => lines.extend(more),
                Err(error) => fail(what, &error),
            };
            if dispatching {
                step(
                    "dispatch",
                    dispatch::step(open, supervised, &spawn, workspace_root.as_deref()),
                );
            }
            step("residents", supervise::keep_residents(open));
            if supervised {
                step(
                    "supervise",
                    supervise::keep_alive(open).map(|line| line.into_iter().collect()),
                );
            }
            for line in &lines {
                println!("{line}");
            }
        }
        if failed {
            // A broken connection is reopened on the next pass.
            store = None;
            failures = failures.saturating_add(1);
            if failures >= MAX_FAILURES {
                return Err(format!("{failures} failed passes in a row; exiting"));
            }
        } else {
            failures = 0;
        }
        // ponytail: fixed backoff, cap at 30 intervals. A per-error-kind
        // schedule only matters once a failure class is slow to clear.
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
