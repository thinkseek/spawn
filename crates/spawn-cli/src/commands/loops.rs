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
//!   (`attempts()`: `SPAWN_DELIVERY_NOT_READY_MS`, default
//!   [`NOT_READY_RETRY_MS`], after a refusal, `retry_wait_ms()`
//!   after a failure), a stall's bound ([`dispatch::Bounds`]), a failed
//!   Stop's retry, a background job's bound, or a report note's retry;
//! - the liveness rule ([`supervise::keep_residents`],
//!   [`supervise::keep_alive`]), which waits for the end of a failed
//!   revival's backoff;
//! - the cost step ([`cost::record`]), which folds the Bridge's
//!   `model.usage` records into each message's priced cost, and waits for
//!   nothing but them (what only the transcript archive holds is
//!   counted once at start, beside the passes: [`cost::archive::start`];
//!   the other Claude Code sessions' transcripts, apart from the engines'
//!   record, at start and hourly: [`cost::sessions::start`]);
//! - the alerts ([`alerts::Monitor::check`]), which waits for its next host
//!   reading (`SPAWN_ALERT_CHECK_SECS`, 30 s) or a bound's due time.
//!
//! What does not follow this rule is what no append announces: the Bridge's
//! reading of each engine's screen and transcript runs every pass (objective
//! 025 gives that schedule its own bounds), the disk watch reads free
//! space on its own clock ([`disk::CHECK_EVERY`]), and the nodes' heartbeat
//! runs on a worker thread of its own ([`crate::nodes::Heartbeat`]).
//!
//! A pass where nothing moved and nothing is due reads the watermark and
//! runs no step: on an idle fleet that is the only statement it makes, and
//! it costs one scan. Between passes the loop waits on `LISTEN
//! spawn_events` and the Bridge's file watch, with a timeout of the earlier of
//! the pass interval and the next due time, so a change ends the wait at once
//! (both wakeups are hints; the watermark and drained watch are the records)
//! and a notification lost costs at most one interval.
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
use crate::{alerts, cost, disk, dispatch, supervise};

/// Most intervals a failing loop waits before its next pass.
const MAX_BACKOFF_INTERVALS: u32 = 30;

/// Most intervals an outage's pause grows to. The loop must notice the
/// database is back within a few seconds of its clearing (objective 032:
/// a restart of 8-20 s costs at most that), not half a minute.
const MAX_OUTAGE_INTERVALS: u32 = 4;

/// Pause after `failures` consecutive failures: one interval per failure,
/// at least one, at most [`MAX_BACKOFF_INTERVALS`].
fn backoff(interval: std::time::Duration, failures: u32) -> std::time::Duration {
    interval.saturating_mul(failures.clamp(1, MAX_BACKOFF_INTERVALS))
}

/// How long `spawn tick` goes on ticking for an end whose kill waits off
/// the Bridge's loop: longer than a host's kill takes at its bound (tmux:
/// 7 s) plus a waiting command's retry (1 s by default).
const TICK_ENDS_LIMIT: std::time::Duration = std::time::Duration::from_secs(12);

/// `spawn tick`: one Bridge and Observer pass. A command whose end waits for
/// its kill, which runs off the Bridge's loop (backlog P7), would finish
/// only at a later pass; this one-shot pass ticks the Bridge again, every
/// 50 ms, until no end waits (at most [`TICK_ENDS_LIMIT`]), so a stop it
/// claims is finished when it returns, as before the kill left the loop.
pub(crate) fn tick(flags: &Flags) -> Result<i32, String> {
    let (mut bridge, mut observer) = runtime(flags)?;
    let mut report = bridge.tick().map_err(|error| error.to_string())?;
    let deadline = std::time::Instant::now() + TICK_ENDS_LIMIT;
    while bridge.ends_pending() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
        // Its commands are the first pass's claimed again (a waiting stop),
        // not new ones: `commands=` says what this tick was given.
        let more = bridge.tick().map_err(|error| error.to_string())?;
        report.warnings.extend(more.warnings);
        report.notes.extend(more.notes);
    }
    for warning in &report.warnings {
        eprintln!("spawn tick warning: {warning}");
    }
    for note in &report.notes {
        eprintln!("spawn tick: {note}");
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
    /// A watched engine file may have changed: the next Bridge pass drains
    /// the watch and reads the files and watermarks as usual.
    FilesChanged,
    /// The stop flag is set.
    Stopped,
    /// The listening connection is gone: it proves nothing, and the loop
    /// waits out the interval and listens again on its next pass.
    Lost,
}

/// Wait for `pause`, or less once `stop` is set, a notification arrives on
/// `listener`, or `bridge`'s file watch becomes readable. Both wakeups are
/// hints, not records: the next pass reads the watermark and drains the watch.
/// A watch that cannot wait, or whose wait fails, leaves the original listener
/// wait in charge for the rest of this pause.
fn wait_unless_stopped(
    mut listener: Option<&mut spawn_postgres::Listener>,
    bridge: Option<&spawn_core::bridge::Bridge>,
    pause: Duration,
    stop: &AtomicBool,
) -> Waited {
    let deadline = std::time::Instant::now() + pause;
    let mut watch_files = bridge.is_some();
    loop {
        if stop.load(Ordering::SeqCst) {
            return Waited::Stopped;
        }
        if let Some(listener) = listener.as_deref_mut() {
            match listener.wait(Duration::ZERO) {
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
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Waited::TimedOut;
        }
        if watch_files && let Some(bridge) = bridge {
            // Bound each file wait so buffered PostgreSQL notifications and
            // the stop flag are still checked while both sources are quiet.
            match bridge.wait_for_change(remaining.min(Duration::from_millis(25))) {
                Ok(Some(true)) => return Waited::FilesChanged,
                Ok(Some(false)) => continue,
                Ok(None) => watch_files = false,
                Err(error) => {
                    eprintln!("spawn loop warning: {error}; file wakeup disabled for this wait");
                    // An instantly failing watch must never become a spin
                    // loop: only the ordinary listener/interval wait runs now.
                    watch_files = false;
                }
            }
        }
        match listener.as_deref_mut() {
            Some(listener) => match listener.wait(remaining) {
                Ok(1..) => return Waited::Notified,
                Ok(0) => {}
                Err(_) => return Waited::Lost,
            },
            None => {
                return if pause_unless_stopped(remaining, stop) {
                    Waited::Stopped
                } else {
                    Waited::TimedOut
                };
            }
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

    /// Record that the step saw `watermark` and had nothing to do: its own
    /// clock, an open wait's probe, keeps the time it had.
    fn advance(&mut self, watermark: i64) {
        self.mark = Some(watermark);
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
    cost: Step,
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
            cost: Step::new(),
            alerts: Step::new(),
        }
    }

    /// Every step, for the rules that hold for all of them.
    fn steps(&self) -> [&Step; 8] {
        [
            &self.bridge,
            &self.observer,
            &self.recovery,
            &self.dispatch,
            &self.residents,
            &self.alive,
            &self.cost,
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
            // MAX_FAILURES, and the pause grows to at most four intervals
            // ([`MAX_OUTAGE_INTERVALS`]): a restart is noticed within a few
            // seconds of its end.
            self.outages = self.outages.saturating_add(1);
            backoff(interval, self.outages.min(MAX_OUTAGE_INTERVALS))
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
/// spawn_events` and the Bridge's file watch, with a timeout of the earlier of
/// the pass interval and the next due time, so a change ends the wait at once
/// and is acted on sooner than a clock could, and a notification lost costs
/// one interval at most.
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
    // A recovery setting out of its range runs nothing.
    let config = crate::recover::Config::from_env()?;
    let stopping = stop_on_signal()?;
    let (bounds, mut warnings) = dispatch::Bounds::from_env();
    let (waits, more) = crate::deliver::Waits::from_env();
    warnings.extend(more);
    let (alerting, more) = alerts::Monitor::from_env();
    warnings.extend(more);
    let (core_lines, more) = crate::core_lines_sample::Sampler::from_env();
    warnings.extend(more);
    let (nodes, more) = crate::nodes::Heartbeat::from_env();
    warnings.extend(more);
    // A price the loop cannot read would record every cost wrong: it stops
    // here, naming the setting.
    let prices = cost::Prices::from_env()?;
    for warning in &warnings {
        eprintln!("spawn loop warning: {warning}");
    }
    let root = workspace_root(flags).map(std::path::PathBuf::from);
    // What only the transcript archive holds, counted beside the passes.
    cost::archive::start(root.as_deref(), &prices);
    // The other Claude Code sessions on this host, counted apart from the
    // engines' record: at start, then hourly, read only, appended bytes only.
    cost::sessions::start(root.as_deref(), &prices);
    let monitor = disk::Monitor::new(root.as_deref());
    let sampler = crate::storage::Sampler::new(root.as_deref());
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
        storage: sampler,
        storage_bytes: crate::storage_bytes::Sampler::default(),
        core_lines,
        nodes,
        terminals: crate::config::terminal_host()?,
        config,
        recovery: crate::recover::Recovery::new(),
        pace: Pace::default(),
        pause: loop_interval(),
        bounds,
        waits,
        alerting,
        prices,
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
    /// The `host.storage` samples: at the loop's start, then hourly
    /// ([`crate::storage::Sampler`]).
    storage: crate::storage::Sampler,
    /// The daily `host.storage_bytes` readings (objective 180): the real
    /// bytes on disk of each storage source, read beside the passes and
    /// outside every owner hold ([`crate::storage_bytes::Sampler`]).
    storage_bytes: crate::storage_bytes::Sampler,
    /// The `host.core_lines` samples (objective 090): at the loop's start,
    /// then every `SPAWN_CORE_LINES_SAMPLE_SECS`, read beside the passes
    /// ([`crate::core_lines_sample::Sampler`]).
    core_lines: crate::core_lines_sample::Sampler,
    /// The nodes' heartbeat (multi-host P1, [`crate::nodes::Heartbeat`]):
    /// its probes and its records run on a worker thread with a database
    /// connection of its own; a pass only takes its lines.
    nodes: crate::nodes::Heartbeat,
    /// The terminal host the recovery asks whether a native session's
    /// terminal server lives on (an end found with it gone is a host
    /// restart).
    terminals: spawn_host::LocalTerminalHost,
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
    /// The delivery waits `SPAWN_DELIVERY_RETRY_SECS`,
    /// `SPAWN_REISSUE_AFTER_SECS` and `SPAWN_DELIVERY_NOT_READY_MS` set
    /// ([`crate::deliver::Waits`]).
    waits: crate::deliver::Waits,
    /// The loop's own alerts ([`alerts::Monitor`]): its records decide
    /// whether this pass has anything to do, on the same watermark and
    /// due-time rule as the other steps.
    alerting: alerts::Monitor,
    /// The prices the cost step records each model call at
    /// (`SPAWN_MODEL_PRICES`, read once at start).
    prices: cost::Prices,
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
        // What the Bridge measured (each end's kill and its wait), logged.
        let mut notes: Vec<String> = Vec::new();
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
            self.bridge_work(
                &open,
                watermark,
                now,
                (&mut warnings, &mut notes),
                &mut errors,
            );
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
                Ok(report) => {
                    warnings.extend(report.warnings);
                    notes.extend(report.notes);
                }
                Err(error) => Fleet::fail(&mut errors, "bridge", &error.to_string()),
            }
        }
        for warning in &warnings {
            eprintln!("spawn loop warning: {warning}");
        }
        for note in &notes {
            eprintln!("spawn loop: {note}");
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
        (warnings, notes): (&mut Vec<String>, &mut Vec<String>),
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
                notes.extend(report.notes);
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
        // The storage sample never fails a pass either: one that could not
        // be written is tried again a minute later.
        match self.storage.check(open, now) {
            Ok(line) => lines.extend(line),
            Err(error) => lines.push(error),
        }
        // Nor the Core lines sample: its reading runs beside the passes and
        // one that could not be read or written is tried a minute later.
        match self.core_lines.check(open, now) {
            Ok(line) => lines.extend(line),
            Err(error) => lines.push(error),
        }
        // Nor the daily storage reading (objective 180): its script runs
        // beside the passes, and a reading held off, or one that could not
        // be taken or written, is looked at again ten minutes later.
        match self
            .storage_bytes
            .check(open, self.workspace_root.as_deref(), now)
        {
            Ok(line) => lines.extend(line),
            Err(error) => lines.push(error),
        }
        // Nor the nodes' heartbeat: its probes and its reads and writes run
        // on its own worker; a pass takes only the lines it said.
        lines.extend(self.nodes.check(open));
        // The recovery first of the rules that revive an agent: it records
        // every interruption once, with its cause and its evidence, and
        // takes the one way back that cause has. The dispatcher's
        // one-resume rule and the liveness rule's revival give way to what
        // it has recorded (docs/fleet-architecture.md §3.1).
        self.recovery_step(open, watermark, now, lines, errors);
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
        // The cost step before the alerts, so a budget's reading sees the
        // calls recorded so far. Only the Bridge's records give it work; it
        // never fails a pass: an error is said and tried again at the next
        // change of the log.
        if self.schedule.cost.runs(watermark, now) {
            match cost::record(open, &self.prices) {
                Ok(line) => lines.extend(line),
                Err(error) => lines.push(error),
            }
            self.schedule.cost.ran(watermark, None);
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

    /// The recovery's step: record every interruption once, with its cause
    /// and its evidence, and take the one way back that cause has. The
    /// dispatcher's one-resume rule and the liveness rule's revival give way
    /// to what it records (docs/fleet-architecture.md §3.1). It reads a few
    /// kinds of record, not every event: a pass whose watermark moved on a
    /// screen or a delivery asks once, cheaply, and skips every session's
    /// facts when none of them is here; an open wait's probe, and an outage
    /// the failed passes left, force it regardless.
    fn recovery_step(
        &mut self,
        open: &std::sync::Arc<PostgresStore>,
        watermark: i64,
        now: i64,
        lines: &mut Vec<String>,
        errors: &mut Vec<String>,
    ) {
        // The recovery reads a few kinds of record, not every event: a pass
        // whose watermark moved on a screen or a delivery asks once, cheaply,
        // and skips every session's facts when none of them is here. An open
        // wait's probe, and an outage the failed passes left, force it
        // regardless.
        let pending_outage = self.recovery.outage_pending();
        if !self.schedule.recovery.runs(watermark, now) && !pending_outage {
            return;
        }
        let forced = self.schedule.recovery.clock_due(now) || pending_outage;
        let signals = if forced {
            true
        } else {
            match open.recovery_signals(self.schedule.recovery.mark().unwrap_or(0)) {
                Ok(signals) => signals,
                Err(error) => {
                    Fleet::fail(errors, "recover", &error.to_string());
                    false
                }
            }
        };
        if signals {
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
                            let due = self.recovery.next_due(self.config.probe, now);
                            self.schedule.recovery.ran(watermark, due);
                        }
                        Err(error) => Fleet::fail(errors, "recover", &error),
                    }
                }
                Err(error) => Fleet::fail(errors, "recover", &error),
            }
        } else if self.schedule.recovery.runs(watermark, now) {
            // Nothing the recovery reads moved: it acts on the watermark
            // this pass and keeps the probe time an open wait already had.
            self.schedule.recovery.advance(watermark);
        }
    }

    /// Log a failed step and mark the pass failed.
    fn fail(errors: &mut Vec<String>, what: &str, error: &str) {
        eprintln!("spawn loop: {what}: {error}");
        errors.push(error.to_string());
    }

    /// Wait for the next pass: the pass interval at most (the Bridge's
    /// screens need it), the earliest due time among the steps, or a failed
    /// pass's backoff — ended early by a notification, a watched-file change,
    /// or a stop. True when the loop must stop.
    fn wait(&mut self, stopping: &AtomicBool) -> bool {
        let pause = if self.pace.failures > 0 || self.pace.outages > 0 {
            // The pause [`Pace`] put after the last failed pass: a database
            // outage paces the loop, the pass interval per pass at first and
            // at most four ([`MAX_OUTAGE_INTERVALS`]).
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
        let bridge = self.running.as_ref().map(|(bridge, _)| bridge);
        match wait_unless_stopped(self.listener.as_mut(), bridge, pause, stopping) {
            Waited::Stopped => true,
            Waited::Notified | Waited::FilesChanged | Waited::TimedOut => false,
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use spawn_core::bridge::{Bridge, BridgeConfig};
    use spawn_core::memory::MemoryStore;
    use spawn_core::port::{FileWatch, WatchError};
    use spawn_core::time::SystemClock;

    use super::{
        MAX_FAILURES, MAX_OUTAGE_INTERVALS, Pace, Schedule, Step, Waited, backoff,
        pause_unless_stopped, stop_on_signal, wait_unless_stopped,
    };

    /// A watch a unit test makes ready or permanently failing.
    #[derive(Debug)]
    struct StubWatch {
        fails: bool,
        waits: AtomicUsize,
    }

    impl StubWatch {
        fn new(fails: bool) -> Self {
            Self {
                fails,
                waits: AtomicUsize::new(0),
            }
        }
    }

    impl FileWatch for StubWatch {
        fn watch(&self, _key: &str, _path: &str) -> Result<(), WatchError> {
            Ok(())
        }

        fn unwatch(&self, _key: &str) {}

        fn drain(&self) -> Vec<String> {
            Vec::new()
        }

        fn wait(&self, _timeout: Duration) -> Result<Option<bool>, WatchError> {
            self.waits.fetch_add(1, Ordering::SeqCst);
            if self.fails {
                Err(WatchError("persistent test failure".to_string()))
            } else {
                Ok(Some(true))
            }
        }
    }

    /// A real Bridge with no executions, over memory ports and `watch`.
    fn memory_bridge(watch: Arc<dyn FileWatch>) -> Bridge {
        let store = Arc::new(MemoryStore::new("wait-test", Arc::new(SystemClock)));
        let mut config = BridgeConfig::new("wait-test");
        config.watch = Some(watch);
        Bridge::new(store.clone(), store.clone(), store.clone(), store, config)
    }

    /// PostgreSQL notifications use one database-wide channel, so the two
    /// integration cases cannot be allowed to wake each other's listener.
    #[cfg(target_os = "linux")]
    static DATABASE_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A transcript file of one integration test, removed on every exit.
    #[cfg(target_os = "linux")]
    struct ScratchFile(std::path::PathBuf);

    #[cfg(target_os = "linux")]
    impl ScratchFile {
        fn new(name: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos());
            let path = std::env::temp_dir().join(format!(
                "spawn-loop-{name}-{}-{nanos}.jsonl",
                std::process::id()
            ));
            std::fs::write(&path, "{\"type\":\"start\"}\n").expect("initial transcript");
            Self(path)
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for ScratchFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

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
            &mut schedule.cost,
            &mut schedule.alerts,
        ] {
            step.ran(watermark, None);
        }
        schedule
    }

    /// A step that saw a moved watermark and had nothing to do keeps the
    /// time it already waited for: an unrelated event never wipes an open
    /// wait's probe.
    #[test]
    fn a_step_that_had_nothing_to_do_keeps_its_due_time() {
        let mut step = Step::new();
        step.ran(7, Some(2_000));
        step.advance(9);
        assert_eq!(step.mark(), Some(9), "the watermark it saw");
        assert!(!step.runs(9, 1_999), "its probe is not due yet");
        assert!(step.runs(9, 2_000), "and is, when its time comes");
        step.ran(9, None);
        step.advance(10);
        assert!(
            !step.runs(10, 9_999),
            "a step with no clock waits for the log"
        );
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

    /// A ready file watch ends the wait with a hint for another Bridge pass.
    #[test]
    fn a_file_change_ends_the_wait() {
        let watch = Arc::new(StubWatch::new(false));
        let bridge = memory_bridge(watch.clone());
        let stop = AtomicBool::new(false);
        let started = Instant::now();
        let waited = wait_unless_stopped(None, Some(&bridge), Duration::from_secs(30), &stop);
        assert!(matches!(waited, Waited::FilesChanged));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(watch.waits.load(Ordering::SeqCst), 1);
    }

    /// The stop flag wins before a watch is polled.
    #[test]
    fn the_stop_flag_ends_a_file_watch_wait() {
        let watch = Arc::new(StubWatch::new(false));
        let bridge = memory_bridge(watch.clone());
        let stop = AtomicBool::new(true);
        let waited = wait_unless_stopped(None, Some(&bridge), Duration::from_secs(30), &stop);
        assert!(matches!(waited, Waited::Stopped));
        assert_eq!(watch.waits.load(Ordering::SeqCst), 0);
    }

    /// A permanently failing watch is tried once, then the ordinary pause
    /// waits out the remainder instead of spinning on the same error.
    #[test]
    fn a_watch_error_falls_back_without_spinning() {
        let watch = Arc::new(StubWatch::new(true));
        let bridge = memory_bridge(watch.clone());
        let stop = AtomicBool::new(false);
        let pause = Duration::from_millis(40);
        let started = Instant::now();
        let waited = wait_unless_stopped(None, Some(&bridge), pause, &stop);
        assert!(matches!(waited, Waited::TimedOut));
        assert_eq!(
            watch.waits.load(Ordering::SeqCst),
            1,
            "the persistent error is not retried in a spin loop"
        );
        assert!(
            started.elapsed() >= pause,
            "the fallback waited out the remainder"
        );
    }

    /// PostgreSQL still wakes the multiplexed wait while its file watch is
    /// quiet. Needs `SPAWN_TEST_DATABASE_URL`, like the store integration
    /// tests.
    #[cfg(target_os = "linux")] // the inotify side of the multiplexed wait
    #[test]
    fn a_postgresql_notification_still_ends_the_multiplexed_wait() {
        use spawn_core::port::CommandQueue;

        let _serial = DATABASE_TEST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let schema = spawn_postgres::testing::TestSchema::new("loop_pg_wake");
        let store = Arc::new(schema.store());
        let mut listener = store.listen().expect("listen");
        let bridge = memory_bridge(Arc::new(
            spawn_host::InotifyWatch::new().expect("inotify watch"),
        ));
        let stop = AtomicBool::new(false);
        assert_eq!(
            listener.wait(Duration::ZERO).expect("drain notifications"),
            0
        );
        let before = store.watermark().expect("watermark");
        let writer = {
            let store = store.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                let submission = spawn_core::command::Submission {
                    agent_id: spawn_core::id::AgentId::new("agent_loop_pg_wake").expect("agent"),
                    session_id: spawn_core::id::SessionId::new("session_loop_pg_wake")
                        .expect("session"),
                    idempotency_key: spawn_core::id::IdempotencyKey::new("loop-pg-wake")
                        .expect("key"),
                    action: spawn_core::command::CommandAction::Stop,
                };
                store
                    .submit(
                        &spawn_core::id::Principal::new("human").expect("principal"),
                        &submission,
                    )
                    .expect("submit");
            })
        };
        let started = Instant::now();
        let waited = wait_unless_stopped(
            Some(&mut listener),
            Some(&bridge),
            Duration::from_secs(10),
            &stop,
        );
        writer.join().expect("notification writer");
        assert!(matches!(waited, Waited::Notified));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the notification ended the wait, not its timeout"
        );
        assert!(store.watermark().expect("watermark") > before);
    }

    /// A real transcript append wakes the wait through inotify alone: no
    /// event is appended and no PostgreSQL notification arrives. Needs
    /// `SPAWN_TEST_DATABASE_URL` for the other half of the multiplexed wait.
    #[cfg(target_os = "linux")] // the inotify adapter
    #[test]
    fn a_transcript_write_wakes_without_a_postgresql_notification() {
        let _serial = DATABASE_TEST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let schema = spawn_postgres::testing::TestSchema::new("loop_file_wake");
        let store = schema.store();
        let mut listener = store.listen().expect("listen");
        assert_eq!(
            listener.wait(Duration::ZERO).expect("drain notifications"),
            0
        );
        let transcript = ScratchFile::new("file-wake");
        let watch = Arc::new(spawn_host::InotifyWatch::new().expect("inotify watch"));
        watch
            .watch("transcript", &transcript.0.display().to_string())
            .expect("watch transcript");
        assert!(watch.drain().is_empty(), "nothing changed yet");
        let bridge = memory_bridge(watch);
        let stop = AtomicBool::new(false);
        let before = store.watermark().expect("watermark");
        let writer = {
            let path = transcript.0.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(path)
                    .expect("open transcript");
                std::io::Write::write_all(&mut file, b"{\"type\":\"message\"}\n")
                    .expect("append transcript");
            })
        };
        let started = Instant::now();
        let waited = wait_unless_stopped(
            Some(&mut listener),
            Some(&bridge),
            Duration::from_secs(5),
            &stop,
        );
        writer.join().expect("transcript writer");
        assert!(matches!(waited, Waited::FilesChanged));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "inotify ended the wait far before its timeout"
        );
        assert_eq!(
            listener.wait(Duration::ZERO).expect("drain notifications"),
            0,
            "the file wakeup had no PostgreSQL notification"
        );
        assert_eq!(
            store.watermark().expect("watermark"),
            before,
            "the transcript append wrote no PostgreSQL event"
        );
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
            interval * MAX_OUTAGE_INTERVALS,
            "the outage's pause grows to at most four intervals, so a restart is noticed in seconds"
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

    /// A database outage's pause grows one interval per failed pass and
    /// stops at four: the loop notices a restart within a few seconds of
    /// its end (objective 032's soak), and an outage never ends it.
    #[test]
    fn an_outages_pause_grows_to_four_intervals_and_never_ends_the_loop() {
        let interval = Duration::from_millis(500);
        let dark = vec![format!(
            "{}: connection closed",
            spawn_core::port::UNREACHABLE
        )];
        let mut pace = Pace::default();
        let mut recovery = crate::recover::Recovery::new();
        let waits: Vec<Duration> = (0..8)
            .map(|_| {
                pace.after(&dark, interval, &mut recovery)
                    .expect("an outage never ends the loop")
                    .wait
            })
            .collect();
        assert_eq!(
            waits,
            vec![
                interval,
                interval * 2,
                interval * 3,
                interval * 4,
                interval * 4,
                interval * 4,
                interval * 4,
                interval * 4,
            ],
            "at most two seconds at the 500 ms interval"
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
