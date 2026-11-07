//! `tick` and `loop`: one pass, and the resident loop. Landing and RSI
//! objectives run in `spawn integrate` (crate::integrate).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use spawn_postgres::PostgresStore;

use crate::Flags;
use crate::config::{
    env_value, loop_interval, open_store, spawn_program, supervised, workspace_root,
};
use crate::runtime::runtime;
use crate::{disk, dispatch, supervise};

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
    let report = bridge.tick().map_err(|error| error.to_string())?;
    for warning in &report.warnings {
        eprintln!("spawn tick warning: {warning}");
    }
    let observations = observer.tick().map_err(|error| error.to_string())?;
    println!(
        "commands={} observations={} sessions={}",
        report.commands,
        observations,
        bridge.session_count()
    );
    Ok(0)
}

/// A flag SIGTERM and SIGINT set instead of killing the process: the loop
/// ends after its pass, and dropping the Bridge makes the pushes still
/// queued in memory (`MissionWorkspaces`' drop flushes its pusher). Without
/// it a deploy's restart lost them (causality audit #4).
fn stop_on_signal() -> Result<Arc<AtomicBool>, String> {
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        signal_hook::flag::register(signal, Arc::clone(&stop))
            .map_err(|error| format!("handle signal {signal}: {error}"))?;
    }
    Ok(stop)
}

/// Sleep for `pause`, or less once `stop` is set. True when it is.
fn pause_unless_stopped(pause: Duration, stop: &AtomicBool) -> bool {
    let until = std::time::Instant::now() + pause;
    while !stop.load(Ordering::SeqCst) {
        let left = until.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return false;
        }
        std::thread::sleep(left.min(Duration::from_millis(100)));
    }
    true
}

/// Consecutive failed passes after which the loop exits, so its unit
/// restarts it with fresh connections (the store does not reconnect).
const MAX_FAILURES: u32 = 10;

/// Run the Bridge, the Observer, the disk watch, the dispatcher and the
/// liveness rule, until SIGTERM or SIGINT ends it after its pass.
///
/// The loop holds its connections and one in-memory cursor, so its cost does
/// not grow with the event log. A failed pass is logged and retried with a
/// growing pause; the dispatcher's connection is reopened. After
/// [`MAX_FAILURES`] failed passes in a row the loop exits with an error and
/// systemd starts it again.
pub(crate) fn resident_loop(flags: &Flags) -> Result<i32, String> {
    let stopping = stop_on_signal()?;
    let interval = loop_interval();
    let (mut bridge, mut observer) = runtime(flags)?;
    let supervised = supervised();
    let dispatching = env_value("SPAWN_DISPATCH").is_none_or(|value| value != "0");
    let spawn = spawn_program();
    let mut store: Option<std::sync::Arc<PostgresStore>> = None;
    let workspace_root = workspace_root(flags).map(std::path::PathBuf::from);
    let mut disk = disk::Monitor::new(workspace_root.as_deref());
    let (bounds, warnings) = dispatch::Bounds::from_env();
    for warning in &warnings {
        eprintln!("spawn loop warning: {warning}");
    }
    let mut failures: u32 = 0;
    loop {
        // A pass takes its interval, the work included: the pause is what is
        // left of it, so a command submitted just after a pass waits about
        // one interval for its claim, not an interval plus the last pass.
        let started = std::time::Instant::now();
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
            store = open_store()
                .map(std::sync::Arc::new)
                .map_err(|error| fail("store", &error))
                .ok();
        }
        if let Some(open) = &store {
            let mut lines = Vec::new();
            let mut step = |what: &str, result: Result<Vec<String>, String>| match result {
                Ok(more) => lines.extend(more),
                Err(error) => fail(what, &error),
            };
            // The disk watch never fails a pass: a fleet must not stop
            // because it cannot record a warning.
            step(
                "disk",
                Ok(disk
                    .check(open)
                    .unwrap_or_else(|error| vec![format!("disk: {error}")])),
            );
            if dispatching {
                step(
                    "dispatch",
                    dispatch::step(
                        open,
                        supervised,
                        &spawn,
                        workspace_root.as_deref(),
                        &disk,
                        &bounds,
                    ),
                );
            }
            // Under the disk floor no resident is revived (a new execution);
            // the supervisor still is, to act on the warning.
            if !disk.below_floor() {
                step("residents", supervise::keep_residents(open));
            }
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
        let pause = backoff(interval, failures).saturating_sub(started.elapsed());
        if pause_unless_stopped(pause, &stopping) {
            eprintln!("spawn loop: stopping; pending pushes are made first");
            return Ok(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use super::{backoff, pause_unless_stopped, stop_on_signal};

    /// A deploy's SIGTERM ends the loop's pause instead of the process, so
    /// the loop returns and its pending pushes are made.
    #[test]
    fn sigterm_ends_the_pause_not_the_process() {
        let stop = stop_on_signal().expect("handlers");
        let started = Instant::now();
        signal_hook::low_level::raise(signal_hook::consts::SIGTERM).expect("raise");
        assert!(pause_unless_stopped(Duration::from_secs(30), &stop));
        assert!(started.elapsed() < Duration::from_secs(5));
        stop.store(false, Ordering::SeqCst);
        assert!(!pause_unless_stopped(Duration::from_millis(10), &stop));
    }

    #[test]
    fn backoff_waits_one_interval_per_failure_between_one_and_thirty() {
        let interval = Duration::from_millis(100);
        assert_eq!(backoff(interval, 0), interval);
        assert_eq!(backoff(interval, 3), interval * 3);
        assert_eq!(backoff(interval, 500), interval * 30);
    }
}
