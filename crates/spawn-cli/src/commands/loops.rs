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
//!   revival's backoff;
//! - the alerts ([`alerts::Monitor::check`]), which waits for its next host
//!   reading (`SPAWN_ALERT_CHECK_SECS`, 30 s) or a bound's due time.
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
use crate::{alerts, disk, dispatch, supervise};

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

/// What a pass's wait ended for.
enum Waited {
    /// The wait timed out with nothing announced.
    TimedOut,
    /// A notification, or a burst of them, arrived: one pass acts on all.
    Notified,
    /// The stop flag is set.
    Stopped,
    /// The listening connection is gone: it proves nothing, and the loop
    /// waits out the interval and listens again on its next pass.
    Lost,
}

/// Wait for `pause`, or less once `stop` is set, or at once when a
/// notification arrives on `listener` (the log's change signal: a hint, not
/// the record — the next pass reads the watermark anyway). Without a
/// listener it is [`pause_unless_stopped`]: the watermark alone wakes the
/// loop, one interval late at worst.
fn wait_unless_stopped(
    listener: Option<&mut spawn_postgres::Listener>,
    pause: Duration,
    stop: &AtomicBool,
) -> Waited {
    let Some(listener) = listener else {
        return if pause_unless_stopped(pause, stop) {
            Waited::Stopped
        } else {
            Waited::TimedOut
        };
    };
    let until = std::time::Instant::now() + pause;
    loop {
        if stop.load(Ordering::SeqCst) {
            return Waited::Stopped;
        }
        let left = until.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Waited::TimedOut;
        }
        match listener.wait(left) {
            // A notification (or a burst of them) ends the wait: one pass
            // acts on all of it.
            Ok(1..) => return Waited::Notified,
            Ok(0) => {}
            // A broken connection, never a hint: returning at once would
            // spin the loop on a wait that fails instantly, so it ends the
            // wait for good and the caller listens again with a fresh one.
            Err(_) => return Waited::Lost,
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

    /// The watermark this step last acted on (`None`: never).
    fn mark(&self) -> Option<i64> {
        self.mark
    }

    /// Whether the step's own clock has reached its due time, whatever the
    /// log says: an open wait's probe, or a retry a record named.
    fn clock_due(&self, now_ms: i64) -> bool {
        self.due.is_some_and(|due| due <= now_ms)
    }
}

/// The steps of one loop, each with what decides whether a pass runs it.
#[derive(Debug, Default)]
struct Schedule {
    bridge: Step,
    observer: Step,
    recovery: Step,
    dispatch: Step,
    residents: Step,
    alive: Step,
    alerts: Step,
}

impl Schedule {
    fn new() -> Self {
        Self {
            bridge: Step::new(),
            observer: Step::new(),
            recovery: Step::new(),
            dispatch: Step::new(),
            residents: Step::new(),
            alive: Step::new(),
            alerts: Step::new(),
        }
    }

    /// Every step, for the rules that hold for all of them.
    fn steps(&self) -> [&Step; 7] {
        [
            &self.bridge,
            &self.observer,
            &self.recovery,
            &self.dispatch,
            &self.residents,
            &self.alive,
            &self.alerts,
        ]
    }

    /// Whether one of the steps waits for no time still to come: its due
    /// time has been reached, and it is the step's to act on now.
    fn due(&self, now_ms: i64) -> bool {
        self.steps()
            .into_iter()
            .any(|step| step.due.is_some_and(|due| due <= now_ms))
    }

    /// The earliest due time among the steps; `None` when none waits on a
    /// clock.
    fn earliest(&self) -> Option<i64> {
        self.steps().into_iter().filter_map(|step| step.due).min()
    }
}

/// This host's clock in epoch milliseconds.
fn now_ms() -> i64 {
    spawn_core::time::Clock::now(&spawn_core::time::SystemClock).0
}

/// How the loop counts failed passes. A pass that failed only because the
/// database could not be reached paces the loop but never ends it: the
/// engines run on untouched, and the recovery records the outage's
/// interruptions at the first pass that gets through. Every other failure
/// counts toward [`MAX_FAILURES`].
#[derive(Default)]
struct Pace {
    failures: u32,
    outages: u32,
}

/// The pause after a pass, and whether the pass could not reach the
/// database at all.
struct Paused {
    wait: Duration,
    unreachable: bool,
}

impl Pace {
    /// The pause before the next pass, or an error once too many passes in a
    /// row failed.
    fn after(
        &mut self,
        errors: &[String],
        interval: Duration,
        recovery: &mut crate::recover::Recovery,
    ) -> Result<Paused, String> {
        let unreachable = errors
            .iter()
            .find(|error| spawn_core::port::is_unreachable(error));
        if let Some(error) = unreachable {
            recovery.failed(
                spawn_core::time::Clock::now(&spawn_core::time::SystemClock).0,
                error,
            );
        }
        let wait = if errors.is_empty() {
            self.failures = 0;
            self.outages = 0;
            backoff(interval, self.failures)
        } else if crate::recover::counts_as_failure(errors) {
            self.failures = self.failures.saturating_add(1);
            self.outages = 0;
            if self.failures >= MAX_FAILURES {
                return Err(format!("{} failed passes in a row; exiting", self.failures));
            }
            backoff(interval, self.failures)
        } else {
            // Only the database was unreachable: it does not count toward
            // MAX_FAILURES, and the pause grows to at most 30 intervals.
            self.outages = self.outages.saturating_add(1);
            backoff(interval, self.outages)
        };
        Ok(Paused {
            wait,
            unreachable: unreachable.is_some(),
        })
    }
}

/// Run the Bridge, the Observer, the disk watch, the recovery, the
/// dispatcher and the liveness rule, until SIGTERM or SIGINT ends it after
/// its pass.
///
/// What the loop reads follows what happens, not the clock
/// (docs/fleet-architecture.md §3, docs/design.md §14): the log's watermark
/// is read once a pass — one index-only read of `spawn_events`' primary
/// key — and the Bridge's reading of the log and the queue, the Observer,
/// the recovery, the dispatcher and the liveness rule run only when that
/// watermark has moved since they last ran, or when a time one of their
/// records named has come. Between passes the loop waits on `LISTEN
/// spawn_events` with a timeout of the earlier of the pass interval and the
/// next due time, so a change ends the wait at once and is acted on sooner
/// than a clock could, and a notification lost costs one interval at most.
/// The Bridge's own reading of screens and transcripts is not on this
/// schedule: neither is announced by an append, so it runs every pass.
///
/// The recovery ([`crate::recover`]) runs before the dispatcher and the
/// liveness rule, and both give way to it: every way an execution ends — its
/// end by exit, signal or a vanished terminal server, a turn the model
/// failed, an outage of Spawn's own database it lived through — is one
/// `execution.interrupted` record naming its cause and the path it takes,
/// and each path brings the agent back without throwing the work away
/// (docs/fleet-architecture.md §3.1: resume at once, then a doubling
/// backoff, a handoff, a wait cleared by a recorded observation, or final).
///
/// The loop holds its connections and one in-memory cursor, so its cost does
/// not grow with the event log. A failed pass is logged and retried with a
/// growing pause; the store, and with it the Bridge and the Observer, are
/// rebuilt after a pass that could not reach the database. After
/// [`MAX_FAILURES`] failed passes in a row the loop exits with an error and
/// systemd starts it again — but a pass that failed only because the
/// database could not be reached is [`Pace`]d, never counted: the engines
/// run on untouched, and the loop outlives the outage.
pub(crate) fn resident_loop(flags: &Flags) -> Result<i32, String> {
    let stopping = stop_on_signal()?;
    let (bounds, mut warnings) = dispatch::Bounds::from_env();
    let (waits, more) = crate::deliver::Waits::from_env();
    warnings.extend(more);
    let (alerting, more) = alerts::Monitor::from_env();
    warnings.extend(more);
    for warning in &warnings {
        eprintln!("spawn loop warning: {warning}");
    }
    let root = workspace_root(flags).map(std::path::PathBuf::from);
    let monitor = disk::Monitor::new(root.as_deref());
    let mut fleet = Fleet {
        flags,
        running: Some(runtime(flags)?),
        store: None,
        listener: None,
        schedule: Schedule::new(),
        supervised: supervised(),
        dispatching: env_value("SPAWN_DISPATCH").is_none_or(|value| value != "0"),
        spawn: spawn_program(),
        workspace_root: root,
        disk: monitor,
        terminals: crate::config::terminal_host(),
        config: crate::recover::Config::from_env(),
        recovery: crate::recover::Recovery::new(),
        pace: Pace::default(),
        pause: loop_interval(),
        bounds,
        waits,
        alerting,
        interval: loop_interval(),
        steps_at: None,
    };
    loop {
        fleet.pass()?;
        if fleet.wait(&stopping) {
            eprintln!("spawn loop: stopping; pending pushes are made first");
            return Ok(0);
        }
    }
}

/// The loop between passes: the Bridge, the Observer and the reads over the
/// store, and the marks that say what each has acted on.
struct Fleet<'a> {
    /// The command line, for the Bridge and the Observer rebuilt after an
    /// outage of the database.
    flags: &'a Flags,
    /// The Bridge and the Observer, built again after such an outage: each
    /// holds its own connection, and the store does not reconnect.
    running: Option<(spawn_core::bridge::Bridge, spawn_core::observe::Observer)>,
    /// The store's connection, reopened after a failed pass.
    store: Option<std::sync::Arc<PostgresStore>>,
    /// The connection that listens for `spawn_events`, on its own session.
    listener: Option<spawn_postgres::Listener>,
    schedule: Schedule,
    supervised: bool,
    dispatching: bool,
    spawn: String,
    workspace_root: Option<std::path::PathBuf>,
    disk: disk::Monitor,
    /// The terminal host the recovery asks whether a native session's
    /// terminal server lives on (an end found with it gone is a host
    /// restart).
    terminals: spawn_host::TmuxHost,
    /// The recovery's settings (`SPAWN_RECOVER_*`).
    config: crate::recover::Config,
    /// The recovery's own state: this loop's start, and the database outage
    /// its failed passes made, cleared at its first good pass.
    recovery: crate::recover::Recovery,
    /// How many passes in a row failed, and how many in a row failed only
    /// because the database could not be reached ([`Pace`]).
    pace: Pace,
    /// The pause [`Pace`] put after the last failed pass.
    pause: Duration,
    bounds: dispatch::Bounds,
    /// The delivery waits `SPAWN_DELIVERY_RETRY_SECS` and
    /// `SPAWN_REISSUE_AFTER_SECS` set ([`crate::deliver::Waits`]).
    waits: crate::deliver::Waits,
    /// The loop's own alerts ([`alerts::Monitor`]): its records decide
    /// whether this pass has anything to do, on the same watermark and
    /// due-time rule as the other steps.
    alerting: alerts::Monitor,
    interval: Duration,
    /// When the steps last ran over the store. They run at most once a pass
    /// interval, which is the resolution the loop always had: a burst of
    /// notifications inside one interval is one pass's work, not one run per
    /// event.
    steps_at: Option<std::time::Instant>,
}

impl Fleet<'_> {
    /// One pass: open the store and the runtime if they were closed, read
    /// the watermark once, run the steps it names, and read the Bridge's
    /// screens and transcripts — which no append announces, so they are read
    /// every pass. The pass's failures are [`Pace`]d: one that could only
    /// not reach the database never ends the loop.
    fn pass(&mut self) -> Result<(), String> {
        let now = now_ms();
        let mut lines: Vec<String> = Vec::new();
        let mut warnings: Vec<String> = Vec::new();
        let mut errors: Vec<String> = Vec::new();
        if self.running.is_none() {
            match runtime(self.flags) {
                Ok(built) => self.running = Some(built),
                Err(error) => Fleet::fail(&mut errors, "runtime", &error),
            }
        }
        if self.store.is_none() {
            match open_store().map(std::sync::Arc::new) {
                Ok(open) => self.store = Some(open),
                Err(error) => {
                    Fleet::fail(&mut errors, "store", &error);
                }
            }
            self.listener = None;
        }
        let watermark = match &self.store {
            Some(open) => match open.watermark() {
                Ok(watermark) => Some(watermark),
                Err(error) => {
                    Fleet::fail(&mut errors, "watermark", &error.to_string());
                    None
                }
            },
            None => None,
        };
        // A time the records named runs its step now, never behind the pass
        // interval; a change of the log may wait for that interval, so a
        // burst of appends inside one interval is one pass's work.
        let due =
            self.schedule.due(now) || self.steps_at.is_none_or(|at| at.elapsed() >= self.interval);
        if let (Some(open), Some(watermark)) = (self.store.clone(), watermark) {
            self.bridge_work(&open, watermark, now, &mut warnings, &mut errors);
            if due {
                self.steps_at = Some(std::time::Instant::now());
                self.steps(&open, watermark, now, &mut lines, &mut errors);
            }
        }
        // The screens and transcripts the Bridge reads are not announced by
        // an append: this is what keeps them on the pass interval, and what
        // keeps an engine's turn end seen as soon as it is written.
        if let Some((bridge, _)) = self.running.as_mut() {
            match bridge.poll() {
                Ok(report) => warnings.extend(report.warnings),
                Err(error) => Fleet::fail(&mut errors, "bridge", &error.to_string()),
            }
        }
        for warning in &warnings {
            eprintln!("spawn loop warning: {warning}");
        }
        for line in &lines {
            println!("{line}");
        }
        let paused = self
            .pace
            .after(&errors, self.interval, &mut self.recovery)?;
        self.pause = paused.wait;
        if !errors.is_empty() {
            // A broken connection is reopened on the next pass, and the
            // Bridge and the Observer are built again when it was the
            // database that could not be reached.
            self.store = None;
            self.listener = None;
            if paused.unreachable {
                self.running = None;
            }
        }
        Ok(())
    }

    /// The Bridge's reading of the log and its claims of the queue: a few
    /// reads, and the one step that turns a submitted command into work, so
    /// it runs whenever the watermark moved or its lease says so, never
    /// behind the pass interval the heavier steps keep to.
    fn bridge_work(
        &mut self,
        open: &std::sync::Arc<PostgresStore>,
        watermark: i64,
        now: i64,
        warnings: &mut Vec<String>,
        errors: &mut Vec<String>,
    ) {
        if !self.schedule.bridge.runs(watermark, now) {
            return;
        }
        // `None`: the runtime is built again on the next pass.
        let worked = self.running.as_mut().map(|(bridge, _)| bridge.work());
        match worked {
            Some(Ok((report, more))) => {
                warnings.extend(report.warnings);
                // Its own wait is the earliest lease still running, or the
                // next pass when the claim stopped at its per-tick limit and
                // a command may still be waiting for one.
                let due = if more {
                    Some(now)
                } else {
                    open.next_lease_ms().unwrap_or_else(|error| {
                        Fleet::fail(errors, "bridge", &error.to_string());
                        None
                    })
                };
                self.schedule.bridge.ran(watermark, due);
            }
            Some(Err(error)) => Fleet::fail(errors, "bridge", &error.to_string()),
            None => {}
        }
    }

    /// The steps whose input is the store besides the Bridge's own reads:
    /// the Observer's judgements, the disk watch, the recovery, the
    /// dispatcher and the liveness rule. Each runs when the watermark moved
    /// or its own time came, and records both.
    fn steps(
        &mut self,
        open: &std::sync::Arc<PostgresStore>,
        watermark: i64,
        now: i64,
        lines: &mut Vec<String>,
        errors: &mut Vec<String>,
    ) {
        if self.schedule.observer.runs(watermark, now)
            && let Some((_, observer)) = self.running.as_mut()
        {
            match observer.tick() {
                Ok(_) => self
                    .schedule
                    .observer
                    .ran(watermark, observer.next_due_ms()),
                Err(error) => Fleet::fail(errors, "observer", &error.to_string()),
            }
        }
        // The disk watch never fails a pass: a fleet must not stop because it
        // cannot record a warning. It reads free space on its own clock
        // ([`disk::CHECK_EVERY`]) and the log once.
        lines.extend(
            self.disk
                .check(open)
                .unwrap_or_else(|error| vec![format!("disk: {error}")]),
        );
        // The recovery first of the rules that revive an agent: it records
        // every interruption once, with its cause and its evidence, and
        // takes the one way back that cause has. The dispatcher's
        // one-resume rule and the liveness rule's revival give way to what
        // it has recorded (docs/fleet-architecture.md §3.1).
        // The recovery reads a few kinds of record, not every event: a pass
        // whose watermark moved on a screen or a delivery asks once, cheaply,
        // and skips every session's facts when none of them is here. A wait
        // that is open and an outage that just ended force it regardless.
        let recovery_signals = self.schedule.recovery.clock_due(now)
            || match open.recovery_signals(self.schedule.recovery.mark().unwrap_or(0)) {
                Ok(signals) => signals,
                Err(error) => {
                    Fleet::fail(errors, "recover", &error.to_string());
                    false
                }
            };
        if self.schedule.recovery.runs(watermark, now) {
            if !recovery_signals {
                self.schedule.recovery.ran(watermark, None);
            } else {
            match crate::deliver::Work::load(open) {
                Ok(work) => {
                    let world = crate::recover::World {
                        store: open,
                        work: &work,
                        terminals: &self.terminals,
                        monitor: &self.disk,
                        root: self.workspace_root.as_deref(),
                        config: &self.config,
                        now_ms: now,
                        spawn: &self.spawn,
                    };
                    match crate::recover::step(&mut self.recovery, &world) {
                        Ok(more) => {
                            lines.extend(more);
                            // An open wait is re-read every probe (the
                            // credentials file, the free space, a retry's
                            // due time: [`crate::recover::Config::probe`]);
                            // with none, the step runs again when the log
                            // moves, never before — an idle pass then costs
                            // the watermark read alone.
                            let due = self
                                .recovery
                                .probing()
                                .then(|| now.saturating_add(self.config.probe));
                            self.schedule.recovery.ran(watermark, due);
                        }
                        Err(error) => Fleet::fail(errors, "recover", &error),
                    }
                }
                Err(error) => Fleet::fail(errors, "recover", &error),
            }
            }
        }
        if self.dispatching && self.schedule.dispatch.runs(watermark, now) {
            match dispatch::step(
                open,
                self.supervised,
                &self.spawn,
                self.workspace_root.as_deref(),
                &self.disk,
                &self.bounds,
                self.waits,
            ) {
                Ok((more, due)) => {
                    lines.extend(more);
                    self.schedule.dispatch.ran(watermark, due);
                }
                Err(error) => Fleet::fail(errors, "dispatch", &error),
            }
        }
        // Under the disk floor no resident is revived (a new execution); the
        // supervisor still is, to act on the warning.
        if !self.disk.below_floor() && self.schedule.residents.runs(watermark, now) {
            match supervise::keep_residents(open) {
                Ok((more, due)) => {
                    lines.extend(more);
                    self.schedule.residents.ran(watermark, due);
                }
                Err(error) => Fleet::fail(errors, "residents", &error),
            }
        }
        if self.supervised && self.schedule.alive.runs(watermark, now) {
            match supervise::keep_alive(open) {
                Ok((line, due)) => {
                    lines.extend(line);
                    self.schedule.alive.ran(watermark, due);
                }
                Err(error) => Fleet::fail(errors, "supervise", &error),
            }
        }
        // The alerts never fail a pass either: an error is said and tried
        // again at the next check, like the disk watch's.
        if self.schedule.alerts.runs(watermark, now) {
            match self.alerting.check(open) {
                Ok(more) => {
                    lines.extend(more);
                    self.schedule
                        .alerts
                        .ran(watermark, Some(self.alerting.due_ms()));
                }
                Err(error) => {
                    lines.push(format!("alerts: {error}"));
                    self.schedule
                        .alerts
                        .ran(watermark, Some(self.alerting.due_ms()));
                }
            }
        }
    }

    /// Log a failed step and mark the pass failed.
    fn fail(errors: &mut Vec<String>, what: &str, error: &str) {
        eprintln!("spawn loop: {what}: {error}");
        errors.push(error.to_string());
    }

    /// Wait for the next pass: the pass interval at most (the Bridge's
    /// screens need it), the earliest due time among the steps, or a failed
    /// pass's backoff — ended early by a notification or a stop. True when
    /// the loop must stop.
    fn wait(&mut self, stopping: &AtomicBool) -> bool {
        let pause = if self.pace.failures > 0 || self.pace.outages > 0 {
            // The pause [`Pace`] put after the last failed pass: a database
            // outage paces the loop, the pass interval per pass at first and
            // at most 30 intervals ([`MAX_BACKOFF_INTERVALS`]).
            self.pause
        } else {
            self.schedule.earliest().map_or(self.interval, |due| {
                self.interval.min(Duration::from_millis(
                    u64::try_from(due.saturating_sub(now_ms())).unwrap_or(0),
                ))
            })
        };
        // A notification on the log ends the wait at once: a hint that the
        // watermark moved, never the record that it did. Without a listener
        // the wait is the interval, and the watermark is still read every
        // pass.
        if self.listener.is_none()
            && let Some(open) = &self.store
        {
            match open.listen() {
                Ok(listener) => self.listener = Some(listener),
                Err(error) => {
                    eprintln!("spawn loop: listen: {error}; the watermark alone wakes it");
                }
            }
        }
        match wait_unless_stopped(self.listener.as_mut(), pause, stopping) {
            Waited::Stopped => true,
            Waited::Notified | Waited::TimedOut => false,
            // A dead listening connection is no hint and no reason to spin:
            // the loop waits out the interval (its watermark read is the
            // record either way) and opens a fresh listener next pass.
            Waited::Lost => {
                eprintln!("spawn loop: listen: the notification connection is gone; reopening");
                self.listener = None;
                pause_unless_stopped(pause, stopping)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use super::{
        MAX_BACKOFF_INTERVALS, MAX_FAILURES, Pace, Schedule, Step, backoff, pause_unless_stopped,
        stop_on_signal,
    };

    /// A schedule whose steps have all run on `watermark` and wait for no
    /// clock, for the cases that set one of them.
    fn settled(watermark: i64) -> Schedule {
        let mut schedule = Schedule::new();
        for step in [
            &mut schedule.bridge,
            &mut schedule.observer,
            &mut schedule.recovery,
            &mut schedule.dispatch,
            &mut schedule.residents,
            &mut schedule.alive,
            &mut schedule.alerts,
        ] {
            step.ran(watermark, None);
        }
        schedule
    }

    /// A step runs on a record or on a time, never on the clock: a pass
    /// whose watermark is unchanged and whose due times are in the future
    /// runs nothing, one whose watermark moved runs it, and one whose due
    /// time has come runs it with the watermark where it was.
    #[test]
    fn a_step_runs_when_the_watermark_moves_or_when_its_time_comes() {
        let mut step = Step::new();
        assert!(step.runs(7, 1_000), "a step that has not run runs at once");
        step.ran(7, None);
        assert!(!step.runs(7, 1_000), "nothing moved, nothing waits on it");
        step.ran(7, Some(2_000));
        assert!(!step.runs(7, 1_999), "its due time has not come");
        assert!(step.runs(8, 1_999), "the watermark moved: it runs");
        assert!(
            step.runs(7, 2_000),
            "its due time came with the watermark unmoved"
        );
        assert!(
            step.runs(7, 9_000),
            "a due time already reached is due now, never skipped"
        );
    }

    /// The pass is what its steps say: with nothing moved and nothing due,
    /// no step runs and the pass reads the watermark only; with the
    /// watermark moved every step runs exactly once; and a pass that runs
    /// the same watermark twice never runs a step twice for it.
    #[test]
    fn a_pass_runs_each_step_once_per_moved_watermark() {
        let mut steps = [Step::new(), Step::new(), Step::new()];
        let mut runs = [0_u32; 3];
        let pass = |steps: &mut [Step], runs: &mut [u32; 3], watermark: i64, now: i64| {
            for (step, count) in steps.iter_mut().zip(runs.iter_mut()) {
                if step.runs(watermark, now) {
                    *count += 1;
                    step.ran(watermark, None);
                }
            }
        };
        pass(&mut steps, &mut runs, 10, 1_000);
        assert_eq!(runs, [1, 1, 1], "the first pass runs every step");
        pass(&mut steps, &mut runs, 10, 1_500);
        assert_eq!(runs, [1, 1, 1], "an idle pass runs no step");
        pass(&mut steps, &mut runs, 11, 2_000);
        assert_eq!(runs, [2, 2, 2], "a moved watermark runs each step once");
        pass(&mut steps, &mut runs, 11, 2_500);
        assert_eq!(runs, [2, 2, 2], "a pass with nothing new runs nothing");
    }

    /// A pass where the watermark does not move still runs the step whose
    /// own time came, and only that one.
    #[test]
    fn a_due_time_reached_runs_its_step_with_the_watermark_unmoved() {
        let mut schedule = settled(5);
        schedule.dispatch.ran(5, Some(9_000));
        schedule.alive.ran(5, Some(3_000));
        let now = 5_000;
        assert!(
            !schedule.dispatch.runs(5, now),
            "the dispatcher waits for a time not come"
        );
        assert!(
            schedule.alive.runs(5, now),
            "the liveness rule's time has come"
        );
        assert_eq!(
            schedule.earliest(),
            Some(3_000),
            "the loop waits for the earliest of them"
        );
    }

    /// The wait is never longer than the next due time, and the loop has no
    /// due time when no step waits on a clock.
    #[test]
    fn the_earliest_due_time_is_the_loops_wait() {
        let mut schedule = settled(1);
        schedule.dispatch.ran(1, Some(4_000));
        schedule.alive.ran(1, None);
        assert_eq!(schedule.earliest(), Some(4_000));
        schedule.dispatch.ran(1, None);
        assert_eq!(schedule.earliest(), None);
    }

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

    /// A pass that failed only because the database could not be reached is
    /// no failed pass: the loop outlives the outage (it paces itself on the
    /// outage's own count, at most thirty intervals) and ends only after
    /// [`MAX_FAILURES`] passes that failed for another reason.
    #[test]
    fn only_a_pass_that_failed_for_more_than_the_database_counts_toward_the_exit() {
        let interval = Duration::from_millis(100);
        let dark = vec![format!(
            "{}: connection closed",
            spawn_core::port::UNREACHABLE
        )];
        let other = vec!["storage backend failure: permission denied".to_string()];
        let mut pace = Pace::default();
        let mut recovery = crate::recover::Recovery::new();
        // Ten failed passes in a row would end the loop; ten passes that
        // could not reach the database do not.
        for _ in 0..(MAX_FAILURES * 3) {
            let paused = pace
                .after(&dark, interval, &mut recovery)
                .expect("an outage never ends the loop");
            assert!(paused.unreachable);
        }
        assert_eq!(pace.failures, 0, "no outage pass counts as a failure");
        let paused = pace
            .after(&dark, interval, &mut recovery)
            .expect("still an outage");
        assert_eq!(
            paused.wait,
            interval * MAX_BACKOFF_INTERVALS,
            "the outage's pause grows to at most thirty intervals"
        );
        // Any other failure counts, and ten of them end the loop.
        for _ in 1..MAX_FAILURES {
            pace.after(&other, interval, &mut recovery)
                .expect("under the bound");
        }
        assert_eq!(pace.failures, MAX_FAILURES - 1);
        assert!(
            pace.after(&other, interval, &mut recovery).is_err(),
            "{MAX_FAILURES} failed passes end the loop"
        );
    }

    #[test]
    fn backoff_waits_one_interval_per_failure_between_one_and_thirty() {
        let interval = Duration::from_millis(100);
        assert_eq!(backoff(interval, 0), interval);
        assert_eq!(backoff(interval, 3), interval * 3);
        assert_eq!(backoff(interval, 500), interval * 30);
    }
}
