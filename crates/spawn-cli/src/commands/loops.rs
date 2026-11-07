//! `tick` and `loop`: one pass, and the resident loop. Landing and RSI
//! objectives run in `spawn integrate` (crate::integrate).
//!
//! The loop's rule, in one sentence: **the loop acts on records and on the
//! times records name, never on the clock.** Every pass reads the log's
//! watermark (`max(spawn_events.id)`, one index-only read of the primary
//! key); a step runs when that watermark has moved since it last ran, or
//! when a time its own records named has come. A step is:
//!
//! - the Bridge's reading of the log and its claim of the command queue
//!   ([`Bridge::work`]), which waits for the earliest lease still running
//!   (`lease_until_ms`), or for the next pass when its claim stopped at the
//!   per-tick limit;
//! - the Observer's read of new events ([`Observer::tick`]), which waits for
//!   its own schedule (a fact settling, a recheck of a running execution, a
//!   judge's backoff);
//! - the dispatcher ([`dispatch::step`]), which waits for a delivery's retry
//!   (`attempts()`: [`NOT_READY_RETRY_MS`] after a refusal, `retry_wait_ms()`
//!   after a failure), a stall's bound ([`dispatch::Bounds`]), a failed
//!   Stop's retry, a background job's bound, or a report note's retry;
//! - the liveness rule ([`supervise::keep_residents`],
//!   [`supervise::keep_alive`]), which waits for the end of a failed
//!   revival's backoff.
//!
//! What does not follow this rule is what no append announces: the Bridge's
//! reading of each engine's screen and transcript runs every pass (objective
//! 025 gives that schedule its own bounds), and the disk watch reads free
//! space on its own clock ([`disk::CHECK_EVERY`]).
//!
//! A pass where nothing moved and nothing is due reads the watermark and
//! runs no step: on an idle fleet that is the only statement it makes, and
//! it costs one scan. Between passes the loop waits on `LISTEN
//! spawn_events` with a timeout of the earlier of the pass interval and the
//! next due time, so a change ends the wait at once (a notification is a
//! hint; the watermark is the record) and a notification lost costs at most
//! one interval.
//!
//! Every wait a step returns is checked by a unit test against the rule it
//! belongs to, and none of them moves a decision: given the same records and
//! the same clock, a step run on every pass and a step run on change make
//! the same orders, deliveries, escalations and digests.

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

/// Wait for `pause`, or less once `stop` is set, or at once when a
/// notification arrives on `listener` (the log's change signal: a hint, not
/// the record — the next pass reads the watermark anyway). True when the
/// stop is set. Without a listener it is [`pause_unless_stopped`]: the
/// watermark alone wakes the loop, one interval late at worst.
fn wait_unless_stopped(
    listener: Option<&mut spawn_postgres::Listener>,
    pause: Duration,
    stop: &AtomicBool,
) -> bool {
    let Some(listener) = listener else {
        return pause_unless_stopped(pause, stop);
    };
    let until = std::time::Instant::now() + pause;
    loop {
        if stop.load(Ordering::SeqCst) {
            return true;
        }
        let left = until.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return false;
        }
        match listener.wait(left) {
            // A notification (or a burst of them) ends the wait: one pass
            // acts on all of it.
            Ok(1..) | Err(_) => return false,
            Ok(0) => {}
        }
    }
}

/// Consecutive failed passes after which the loop exits, so its unit
/// restarts it with fresh connections (the store does not reconnect).
const MAX_FAILURES: u32 = 10;

/// One step of the loop, and what decides whether this pass runs it: the
/// watermark it last acted on, and the earliest time it waits for.
///
/// A step runs when the log's watermark has moved since it last ran (a
/// record it decides from may be new), or when its own due time has come (a
/// retry, a bound, a lease, a backoff the records it read named). A pass
/// where neither holds reads the watermark and runs no step: on an idle
/// fleet that is the only statement it makes.
#[derive(Debug, Default)]
struct Step {
    /// The watermark (`max(spawn_events.id)`) the step last acted on, read
    /// before it ran: every event at or below it was in the records the step
    /// read, and one above it may not have been. A step that has never run
    /// has no mark and runs at once.
    mark: Option<i64>,
    /// When this step is due even if the watermark does not move, on this
    /// host's clock; `None` while no clock is waited for.
    due: Option<i64>,
}

impl Step {
    /// A step that has not run yet.
    fn new() -> Self {
        Self {
            mark: None,
            due: Some(0),
        }
    }

    /// Whether this pass runs the step.
    fn runs(&self, watermark: i64, now_ms: i64) -> bool {
        self.mark != Some(watermark) || self.due.is_some_and(|due| due <= now_ms)
    }

    /// Record that the step ran having acted on `watermark`, and when it is
    /// next due (`None`: no clock).
    fn ran(&mut self, watermark: i64, due: Option<i64>) {
        self.mark = Some(watermark);
        self.due = due;
    }
}

/// The earliest due time among the steps; `None` when none waits on a clock.
fn earliest(steps: &[&Step]) -> Option<i64> {
    steps.iter().filter_map(|step| step.due).min()
}

/// This host's clock in epoch milliseconds.
fn now_ms() -> i64 {
    spawn_core::time::Clock::now(&spawn_core::time::SystemClock).0
}

/// Run the Bridge, the Observer, the disk watch, the dispatcher and the
/// liveness rule, until SIGTERM or SIGINT ends it after its pass.
///
/// What the loop reads follows what happens, not the clock
/// (docs/fleet-architecture.md §3, docs/design.md §14): the log's watermark
/// is read once a pass — one index-only read of `spawn_events`' primary
/// key — and the Bridge's reading of the log and the queue, the Observer,
/// the dispatcher and the liveness rule run only when that watermark has
/// moved since they last ran, or when a time one of their records named has
/// come. Between passes the loop waits on `LISTEN spawn_events` with a
/// timeout of the earliest of the pass interval and the next due time, so a
/// change ends the wait at once and is acted on sooner than a clock could,
/// and a notification lost costs one interval at most. The Bridge's own
/// reading of screens and transcripts is not on this schedule: neither is
/// announced by an append, so it runs every pass.
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
    let mut listener: Option<spawn_postgres::Listener> = None;
    let mut bridge_step = Step::new();
    let mut observer_step = Step::new();
    let mut dispatch_step = Step::new();
    let mut residents_step = Step::new();
    let mut alive_step = Step::new();
    let workspace_root = workspace_root(flags).map(std::path::PathBuf::from);
    let mut disk = disk::Monitor::new(workspace_root.as_deref());
    let (bounds, warnings) = dispatch::Bounds::from_env();
    for warning in &warnings {
        eprintln!("spawn loop warning: {warning}");
    }
    let mut failures: u32 = 0;
    loop {
        let now = now_ms();
        let mut failed = false;
        let mut fail = |what: &str, error: &str| {
            failed = true;
            eprintln!("spawn loop: {what}: {error}");
        };
        let mut lines: Vec<String> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        if store.is_none() {
            store = open_store()
                .map(std::sync::Arc::new)
                .map_err(|error| fail("store", &error))
                .ok();
            listener = None;
        }
        let watermark = match &store {
            Some(open) => match open.watermark() {
                Ok(watermark) => Some(watermark),
                Err(error) => {
                    fail("watermark", &error.to_string());
                    None
                }
            },
            None => None,
        };
        if let Some(open) = store.clone() {
            let watermark = watermark.unwrap_or_default();
            // The Bridge's reading of the log and its claims of the queue:
            // its input is the records, so it runs on change, and its own
            // wait is the earliest lease still running.
            if bridge_step.runs(watermark, now) {
                match bridge.work() {
                    Ok((report, more)) => {
                        warnings.extend(report.warnings);
                        let due = if more {
                            // The claim stopped at its per-tick limit: a
                            // command may be waiting for one now.
                            Some(now)
                        } else {
                            match open.next_lease_ms() {
                                Ok(due) => due,
                                Err(error) => {
                                    fail("bridge", &error.to_string());
                                    None
                                }
                            }
                        };
                        bridge_step.ran(watermark, due);
                    }
                    Err(error) => fail("bridge", &error.to_string()),
                }
            }
            // The Observer's own read of the log, and the judgements its
            // schedule waits for (facts settling, a recheck, a backoff).
            if observer_step.runs(watermark, now) {
                match observer.tick() {
                    Ok(_) => observer_step.ran(watermark, observer.next_due_ms()),
                    Err(error) => fail("observer", &error.to_string()),
                }
            }
            // The disk watch never fails a pass: a fleet must not stop
            // because it cannot record a warning. It reads free space on its
            // own clock ([`disk::CHECK_EVERY`]) and the log once.
            lines.extend(
                disk.check(&open)
                    .unwrap_or_else(|error| vec![format!("disk: {error}")]),
            );
            if dispatching && dispatch_step.runs(watermark, now) {
                match dispatch::step(
                    &open,
                    supervised,
                    &spawn,
                    workspace_root.as_deref(),
                    &disk,
                    &bounds,
                ) {
                    Ok((more, due)) => {
                        lines.extend(more);
                        dispatch_step.ran(watermark, due);
                    }
                    Err(error) => fail("dispatch", &error),
                }
            }
            // Under the disk floor no resident is revived (a new execution);
            // the supervisor still is, to act on the warning.
            if !disk.below_floor() && residents_step.runs(watermark, now) {
                match supervise::keep_residents(&open) {
                    Ok((more, due)) => {
                        lines.extend(more);
                        residents_step.ran(watermark, due);
                    }
                    Err(error) => fail("residents", &error),
                }
            }
            if supervised && alive_step.runs(watermark, now) {
                match supervise::keep_alive(&open) {
                    Ok((line, due)) => {
                        lines.extend(line);
                        alive_step.ran(watermark, due);
                    }
                    Err(error) => fail("supervise", &error),
                }
            }
        }
        // The screens and transcripts the Bridge reads are not announced by
        // an append: this is what keeps them on the pass interval, and what
        // keeps an engine's turn end seen as soon as it is written.
        match bridge.poll() {
            Ok(report) => warnings.extend(report.warnings),
            Err(error) => fail("bridge", &error.to_string()),
        }
        for warning in &warnings {
            eprintln!("spawn loop warning: {warning}");
        }
        for line in &lines {
            println!("{line}");
        }
        if failed {
            // A broken connection is reopened on the next pass.
            store = None;
            listener = None;
            failures = failures.saturating_add(1);
            if failures >= MAX_FAILURES {
                return Err(format!("{failures} failed passes in a row; exiting"));
            }
        } else {
            failures = 0;
        }
        // What the next pass waits for: the pass interval at most (the
        // Bridge's screens need it), or the earliest due time among the
        // steps, or a failed pass's backoff.
        let wait = if failures > 0 {
            backoff(interval, failures)
        } else {
            let next = earliest(&[
                &bridge_step,
                &observer_step,
                &dispatch_step,
                &residents_step,
                &alive_step,
            ]);
            next.map_or(interval, |due| {
                interval.min(std::time::Duration::from_millis(
                    u64::try_from(due.saturating_sub(now_ms())).unwrap_or(0),
                ))
            })
        };
        // A notification on the log ends the wait at once: it is a hint that
        // the watermark moved, never the record that it did. Without a
        // listener the wait is the interval and the watermark is still read
        // every pass.
        if listener.is_none()
            && let Some(open) = &store
        {
            match open.listen() {
                Ok(open) => listener = Some(open),
                Err(error) => eprintln!("spawn loop: listen: {error}; the watermark alone wakes it"),
            }
        }
        if wait_unless_stopped(listener.as_mut(), wait, &stopping) {
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
