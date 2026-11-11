//! Alerts: what Spawn tells about itself.
//!
//! The supervisor once ran a hand-written monitor that printed into one
//! pane; nothing of it was recorded, its state lived in its process, and no
//! view of Spawn showed an alert. The loop now raises its own
//! (docs/fleet-architecture.md, "Alerts"): each transition is a control
//! record, `alert.fired` or `alert.cleared`, written through the owner-only
//! `spawn_alert_record`, folded into `spawn_alerts` and shown by
//! `spawn alerts --once`, `spawn alerts --follow` and `GET /api/alerts`.
//!
//! An alert fires once per occurrence and clears at most once, whatever
//! restarts: the key names the occurrence, the store refuses a second
//! `alert.fired` for a key, and the step reads the keys back on start, so a
//! restarted loop fires nothing twice and still knows what is open. It
//! clears only on evidence after what it fired on (objective 050; [`after`]):
//! a record later in the log than the alert's own `event_id`, or a reading
//! at or after its `reading_ms`. A restart reads the last week of records
//! again, and an older one clears nothing; the records a pass reads are
//! recorded one at a time, so a halt and its resume in one read end the
//! alert cleared, naming the resume. A
//! notice is an `alert.fired` with `notice: true`: it is news, never a
//! condition, and never clears.
//!
//! Every alert rests on a named record (its `event_id`) or on a recorded
//! reading (its measure and threshold): a task's state change, a mission's
//! counters, a judgement, an admission's wait, a backup's age, a crash line
//! in the database's log, MemAvailable under its threshold, the used space
//! of the filesystem holding `SPAWN_ALERT_TMPFS_PATH`. Nothing rests on the
//! step's memory (it is rebuilt from the record on start), on absence (no
//! backup record means no alert, never "no backup"), or on time without a
//! record.
//!
//! The kinds, their keys, when they fire and clear:
//!
//! Notices (`notice: true`, never cleared), for every mission:
//! - `task_state`, `task #<id>`, key `task_state:<task>:<event>`: a control
//!   record after which the task's state differs from before it (its
//!   `task.assigned`: from none). A record that leaves it as it was (an
//!   `aborted` or `stale` gating) is no notice.
//! - `mission_state`, `mission #<id>`, key `mission_state:<mission>:<event>`:
//!   a record after which any of the mission's `state`, `halted`,
//!   `gate_rejections`, `gate_aborts`, `gate_room_escalated` differs. The
//!   step folds `gate_rejections` as the store does, with the run's
//!   objective (`gate_rejection_objective`, read back on start): a cancel of
//!   a task of the objective every rejection of the run was ends the run
//!   (objective 053).
//! - `gate_base_disagrees`, `mission #<id>`, key
//!   `gate_base_disagrees:<event>`: a `task.gated` host fault with
//!   `base_disagrees` and `alert` true (objective 033).
//! - `command_failed`, `command #<id>`, key `command_failed:<command>`: a
//!   command that finished failed, an owner's note (`owner-note-…`, which a
//!   person sent) excepted.
//!
//! Conditions:
//! - `scenario_red`, `mission #<id>`, key
//!   `scenario_red:<mission>:<scenario>:<the first task.gated naming it>`:
//!   a gate of the mission found main red on the scenario (the record's
//!   `main_red_scenarios`: solved, as the same scenario, on an ancestor of
//!   the base, and unsolved on it; scripts/rsi/gate.sh, "Main red on a
//!   scenario"), where an unsolved objective says nothing. Once per mission
//!   and scenario while it stays red; clears at the first later `accepted`
//!   gating of the mission that does not name it (an accepted gate judged
//!   every scenario on the base; a rejection may come before the base, so
//!   it clears nothing).
//! - `validator_defect`, `mission #<id>`, key
//!   `validator_defect:<mission>:<scenario>:<event>`: a validator (a
//!   regression) that fails on the base too, run again in the gate
//!   (scripts/rsi/gate.sh, "Three classes of a failing regression"), in
//!   either of two records, each its own alert (evidence `owner`):
//!   - a task's `task.gated` `host_fault` with evidence `validator_defect`:
//!     no candidate that fails it gets a verdict. Clears at the mission's
//!     next `accepted` or `rejected` gating, or once a gate names it
//!     `quarantined`;
//!   - an owner's accepted landing (`source.moved` with
//!     `validator_defects`), for each mission it names: nothing checks the
//!     scenario until it is fixed or quarantined. Clears only once a gate of
//!     the mission names it `quarantined`, the owner's decision: a later
//!     gate that does not name it may have read it unsolved on the base
//!     rather than passed (review-fix-gate.md F1).
//! - `task_undelivered`, `task #<id>`, key
//!   `task_undelivered:<task>:<its task.assigned event>`: in `assigned`
//!   longer than `SPAWN_ALERT_UNDELIVERED_SECS` (default 300) while its
//!   agent holds no delivered or reported task; clears at the record that
//!   moves it.
//! - `mission_halted`, `mission #<id>`, key
//!   `mission_halted:<mission>:<its mission.halted event>`: clears at the
//!   mission's `mission.resumed`.
//! - `postgres_crash`, `test` or `live`, key
//!   `postgres_crash:<subject>:<the line's stamp and [pid] as written, its
//!   spaces folded out so the key is one word>`: a
//!   log line holding `terminated by signal`, `No space left` or `could not
//!   resize`, stamped within the day; clears at the first `database system
//!   is ready to accept connections` line after it, or after
//!   `SPAWN_ALERT_PG_QUIET_SECS` (default 900) with no such line.
//! - `source_away`, `mission #<id>`, key `source_away:<mission>:<reading
//!   ms>`, evidence its `source`: at two readings in a row, a source of an
//!   open or active mission that is a path on this host (absolute) is not a
//!   directory: an unmounted volume, a moved checkout (one reading only
//!   notes it, so a path that comes and goes does not flap). Its agents' branches
//!   are not published to it until it is back (the pusher retries, and says
//!   so only once, in its log: fix-core-b item 6). One per mission and
//!   source while it is away; clears at the first reading that finds the
//!   directory, or once the mission no longer has the source or is no
//!   longer open or active. The supervisor hears of each, and of its end
//!   (dispatch's digest, `source_away_news`).
//! - `memory_low`, `host`, key `memory_low:<reading ms>`: MemAvailable
//!   under `SPAWN_ALERT_MEMORY_LOW_MB` (default 4096); clears at or above
//!   `SPAWN_ALERT_MEMORY_OK_MB` (default 8192), the margin that keeps a host
//!   hovering at the threshold from flapping.
//! - `tmpfs_high`, `host`, key `tmpfs_high:<reading ms>`: the used space of
//!   the filesystem holding `SPAWN_ALERT_TMPFS_PATH` (default `/tmp`) over
//!   `SPAWN_ALERT_TMPFS_HIGH_MB` (default 8000); clears under
//!   `SPAWN_ALERT_TMPFS_OK_MB` (default 6000). A threshold of 0 disables its
//!   check.
//! - `agent_stuck`, the session id, key `agent_stuck:<session>:<its
//!   observation>`: the session's latest `observation.recorded` says `stuck`
//!   at or above [`LIKELY`] (0.5), whatever the dispatcher does with it; a
//!   later observation still stuck fires nothing, and a session whose
//!   current execution has ended fires nothing; clears when the latest
//!   one is under 0.5, or when the session's current execution has ended,
//!   whatever the cause (stopped, exited, lost), with that end as evidence.
//! - `judge_down`, the judge's name (`jev`), key `judge_down:<its
//!   host.judge down event>`: the model judge's watcher recorded `down` (two
//!   calls in a row failed after its last answer, crate judge_watch.rs) and
//!   no `up` after it, `SPAWN_ALERT_JUDGE_DOWN_SECS` (default 900) after the
//!   first of them failed (`first_failed_ms`): no call answered since. It
//!   rests on that record, not on the absence of judgements (a judge that
//!   is down writes few: the Observer records a failure once, not each
//!   retry). Clears at the `up` record, the first call answered, with its
//!   `failed_calls`; an `up` whose `down` is older than the history (a loop
//!   restarted during a week-long outage) clears the judge's open alert.
//! - `admission_wait`, an execution's `agent_id` or a gate's `task #<id>`,
//!   key `admission_wait:<its admission.waiting event>`: an
//!   `admission.waiting` (029) older than `SPAWN_ALERT_ADMISSION_SECS`
//!   (default 600) with no `admission.admitted` of the same kind and agent
//!   or task after it; clears at that admitted.
//! - `backup_stale`, `dump` or `bundle`, key
//!   `backup_stale:<subject>:<the newest backup's event>`: the newest
//!   `backup.dumped` or `backup.bundled` older than
//!   `SPAWN_ALERT_BACKUP_DUMP_MAX_AGE_SECS` (default 7200, the hourly dump's
//!   schedule twice) or `SPAWN_ALERT_BACKUP_BUNDLE_MAX_AGE_SECS` (default
//!   93600, the daily bundle's day plus two hours); with no such record at
//!   all there is no alert; clears at a newer record, which growing old is a
//!   new occurrence.
//! - `backup_budget`, `r2`, key `backup_budget:<its record>`: a `backup.*`
//!   record with `over_alert` true (031) while none is open; clears at the
//!   first later one with `over_alert` false.
//! - `drill_failed`, `drill`, key `drill_failed:<its record>`: a
//!   `backup.verified` with `ok` false; clears at the next with `ok` true.
//! - `integrator_down`, `spawn-integrate`, key `integrator_down:<the first
//!   reading's ms>`: no backend holds the integrator's lock on this store
//!   (`pg_locks`, the lock `integrator_try_lock` takes) at every reading for
//!   `SPAWN_ALERT_INTEGRATOR_DOWN_SECS` (default 300, longer than a deploy's
//!   restart; 0 disables it) while a gated mission has a reported task that
//!   waits for a gate: open or active, not halted, its source held by no
//!   owner (a deploy and an owner's push hold it). Clears at the first
//!   reading that finds the lock held. It rests on readings: the first one
//!   that found the lock free is kept by the step, so a restarted loop
//!   starts the bound again (later, never sooner, never twice). The lock is
//!   read only while the step's fold has a reported task of a mission that
//!   is open and not halted, or this alert is open: an idle loop reads
//!   nothing more (objective 023).
//! - `controller_behind`, `mission #<id>`, key `controller_behind:<mission>:
//!   <the first reading's ms>`: at every reading for
//!   `SPAWN_ALERT_CONTROLLER_BEHIND_SECS` (default 1800; 0 disables it), the
//!   owner's latest landing on a gated mission's source (the latest
//!   `source.moved` record's `to`; the mission open or active, not halted,
//!   with a reported task) is newer than the controller its gate's
//!   scenarios come from (`controllers/<name>-<commit>`): a descendant of
//!   that commit. The integrator holds such a mission's gates until the
//!   deploy switches the controller (P22), and every owner's landing is
//!   followed by its deploy within a minute or so: a wait this long is a
//!   landing not deployed (deploy.sh's exit 5). A task's landing needs no
//!   deploy and counts for nothing. Clears at the first reading that finds
//!   the controller has the landing, or the mission no longer waiting
//!   (closed, halted, nothing reported). Read on the step's readings, only
//!   while its fold has such a reported task or this alert is open; the
//!   first reading behind is kept by the step, so a restarted loop starts
//!   the bound again (later, never sooner).
//! - `cost_budget`, `mission #<id>`, key `cost_budget:<mission>:<the UTC
//!   day, YYYY-MM-DD>`: at a reading (every `SPAWN_ALERT_CHECK_SECS`), the
//!   mission's cost of the model calls timed in the current UTC day
//!   (crate::cost, priced when recorded) has reached its daily budget
//!   (`budget.daily_usd`, `spawn mission configure --daily-usd`). Once per
//!   mission and day, whatever restarts and however much more it spends;
//!   clears when the day ends, or at a reading that finds the budget removed
//!   or above the day's spend. A mission with no daily budget, or under it,
//!   raises none. It rests on the readings of the recorded costs.
//! - `message_not_taken`, the session id, key `message_not_taken:<command>`,
//!   evidence the command, agent, session and whether it was typed
//!   (objective 045): a message (what the Bridge types for a command, with
//!   its marker) that no `message.taken` names `SPAWN_MESSAGE_TAKEN_SECS`
//!   (default 120) after it was recorded (`spawn_command_message`): typed
//!   and never taken, or never typed. A command an older Spawn typed carries
//!   no marker (no `message.typed`) and is never one, nor is one sent while
//!   its session's transcript had held no marker yet. Once per message;
//!   clears when it is taken, or when its session ends with it untaken (the
//!   execution it was typed into, or its own start's, ended). A message
//!   whose session ended first fires nothing. Read on start (every message
//!   that can be one, and the open alerts), then when the log moved (the
//!   new commands, the pending messages due, the open alerts whose session
//!   took a message or ended since) and when a pending one falls due.
//! - `model_auth`, the session id, key
//!   `model_auth:<its execution.interrupted>`: an `execution.interrupted`
//!   (028) with cause `model_auth`; clears at that execution's
//!   `execution.cleared`.
//!
//! When: a record-borne alert is recorded within one pass of its record; a
//! host reading or a log line within `SPAWN_ALERT_CHECK_SECS` (default 30)
//! and one pass; a bound within one pass of falling due, never before,
//! measured on the database's clock from the record's `recorded_at_ms`. The
//! step runs when the event watermark moved or one of its timers is due, and
//! an idle pass reads the watermark once and nothing else (objective 023).
//! Like the disk watch it never fails a pass: an error is said and tried at
//! the next check.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use spawn_core::port::StoreError;
use spawn_postgres::PostgresStore;

use crate::config::{env_value, open_store};

/// The Observer's own attention threshold: a `stuck` judgement at or above
/// it says the agent loops or makes no progress.
pub(crate) const LIKELY: f64 = 0.5;

/// A day in milliseconds: the daily budget's day is the UTC day.
const DAY_MS: i64 = 86_400_000;

/// The UTC day of `ms`, `YYYY-MM-DD`.
fn utc_day(ms: i64) -> String {
    let (year, month, day, ..) = civil(ms);
    format!("{year:04}-{month:02}-{day:02}")
}

/// The integrator-down alert's subject: the unit that runs the integrator.
const INTEGRATOR: &str = "spawn-integrate";

/// The most control records one pass handles; the next pass goes on.
const BATCH: i64 = 500;

/// How far back a restarted loop reads the alert-family records: a condition
/// older than this has long been recorded (or is a new one).
const HISTORY_MS: i64 = 7 * 86_400_000;
const HISTORY_LIMIT: i64 = 5_000;

/// How far back a restarted loop looks for commands that finished failed.
const COMMAND_LOOKBACK_MS: i64 = 600_000;

/// How far before the last reading of the messages the next looks for a
/// taking or an end: an append that committed after that reading may carry
/// an earlier time.
const MESSAGES_MARGIN_MS: i64 = 60_000;

/// On start, the last part of a log is read, not all of it.
const LOG_TAIL_BYTES: u64 = 1024 * 1024;
/// The most one reading takes from a log that grew a lot.
const LOG_READ_LIMIT: u64 = 4 * 1024 * 1024;
/// The most lines one crash occurrence quotes.
const CRASH_LINES: usize = 10;
/// A crash line older than this is history, not a reading.
const CRASH_MAX_AGE_MS: i64 = 86_400_000;

/// The line a recovered server writes.
const READY_LINE: &str = "database system is ready to accept connections";

/// The three lines the fleet's monitor called a crash.
const CRASH_PATTERNS: [&str; 3] = ["terminated by signal", "No space left", "could not resize"];

/// The thresholds, bounds and paths the step runs with (`SPAWN_ALERT_*`).
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    /// How often the host and the logs are read.
    pub(crate) check_ms: i64,
    /// How long a task may stay assigned with its agent free.
    pub(crate) undelivered_ms: i64,
    /// How long an admission may wait.
    pub(crate) admission_ms: i64,
    /// How long the model judge may answer no call.
    pub(crate) judge_down_ms: i64,
    /// How long a message may go untaken (SPAWN_MESSAGE_TAKEN_SECS).
    pub(crate) message_taken_ms: i64,
    /// The dump's and the bundle's schedules, with room for one late run.
    pub(crate) dump_max_age_ms: i64,
    pub(crate) bundle_max_age_ms: i64,
    /// MemAvailable under this, and the margin it clears at.
    pub(crate) memory_low_mb: u64,
    pub(crate) memory_ok_mb: u64,
    /// The filesystem watched, over this, and the margin it clears under.
    pub(crate) tmpfs_path: PathBuf,
    pub(crate) tmpfs_high_mb: u64,
    pub(crate) tmpfs_ok_mb: u64,
    /// How long a crash line clears itself by silence.
    pub(crate) quiet_ms: i64,
    /// How long no integrator may hold its lock while reported work waits
    /// (0: never alerted).
    pub(crate) integrator_down_ms: i64,
    /// How long a mission's main may be ahead of its controller while a
    /// task waits for it (0: never alerted).
    pub(crate) controller_behind_ms: i64,
    /// The window `spawn alerts --once` shows.
    pub(crate) window_ms: i64,
    /// The two database logs, when named.
    pub(crate) test_log: Option<PathBuf>,
    pub(crate) live_log: Option<PathBuf>,
}

/// A number from `SPAWN_ALERT_*`, its default when unset, or its default
/// and one line saying it cannot be read (said once, at start).
fn number(name: &str, default: u64, warnings: &mut Vec<String>) -> u64 {
    match env_value(name) {
        None => default,
        Some(text) => {
            if let Ok(value) = text.trim().parse() {
                value
            } else {
                warnings.push(format!(
                    "{name}: {text:?} is not a number of seconds or megabytes; using {default}"
                ));
                default
            }
        }
    }
}

impl Settings {
    pub(crate) fn from_env() -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let secs = |name: &str, default: u64, warnings: &mut Vec<String>| -> i64 {
            number(name, default, warnings)
                .saturating_mul(1000)
                .cast_signed()
        };
        let settings = Self {
            check_ms: secs("SPAWN_ALERT_CHECK_SECS", 30, &mut warnings).max(1),
            undelivered_ms: secs("SPAWN_ALERT_UNDELIVERED_SECS", 300, &mut warnings),
            admission_ms: secs("SPAWN_ALERT_ADMISSION_SECS", 600, &mut warnings),
            judge_down_ms: secs("SPAWN_ALERT_JUDGE_DOWN_SECS", 900, &mut warnings),
            message_taken_ms: {
                let (bound, warning) = crate::config::message_taken_bound(
                    env_value("SPAWN_MESSAGE_TAKEN_SECS").as_deref(),
                );
                warnings.extend(warning);
                bound
            },
            dump_max_age_ms: secs("SPAWN_ALERT_BACKUP_DUMP_MAX_AGE_SECS", 7200, &mut warnings),
            bundle_max_age_ms: secs(
                "SPAWN_ALERT_BACKUP_BUNDLE_MAX_AGE_SECS",
                93_600,
                &mut warnings,
            ),
            memory_low_mb: number("SPAWN_ALERT_MEMORY_LOW_MB", 4096, &mut warnings),
            memory_ok_mb: number("SPAWN_ALERT_MEMORY_OK_MB", 8192, &mut warnings),
            tmpfs_path: env_value("SPAWN_ALERT_TMPFS_PATH")
                .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from),
            tmpfs_high_mb: number("SPAWN_ALERT_TMPFS_HIGH_MB", 8000, &mut warnings),
            tmpfs_ok_mb: number("SPAWN_ALERT_TMPFS_OK_MB", 6000, &mut warnings),
            quiet_ms: secs("SPAWN_ALERT_PG_QUIET_SECS", 900, &mut warnings),
            integrator_down_ms: secs("SPAWN_ALERT_INTEGRATOR_DOWN_SECS", 300, &mut warnings),
            controller_behind_ms: secs("SPAWN_ALERT_CONTROLLER_BEHIND_SECS", 1800, &mut warnings),
            window_ms: secs("SPAWN_ALERT_WINDOW_SECS", 86_400, &mut warnings),
            test_log: env_value("SPAWN_TEST_DATABASE_LOG").map(PathBuf::from),
            live_log: env_value("SPAWN_DATABASE_LOG").map(PathBuf::from),
        };
        (settings, warnings)
    }
}

/// One alert transition the step decided on, before it is recorded.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Event {
    Fire {
        key: String,
        kind: &'static str,
        subject: String,
        evidence: Value,
        text: String,
        notice: bool,
    },
    Clear {
        key: String,
        evidence: Value,
    },
}

impl Event {
    fn key(&self) -> &str {
        match self {
            Self::Fire { key, .. } | Self::Clear { key, .. } => key,
        }
    }
}

/// One recorded alert, as the store has it.
#[derive(Debug, Clone)]
struct Row {
    kind: String,
    subject: String,
    state: String,
    evidence: Value,
    fired_event_id: i64,
    fired_at_ms: i64,
}

impl Row {
    fn open(&self) -> bool {
        self.state == "open"
    }

    fn from_value(value: &Value) -> Option<(String, Self)> {
        let key = value["key"].as_str()?.to_string();
        Some((
            key,
            Self {
                kind: value["kind"].as_str().unwrap_or("alert").to_string(),
                subject: value["subject"].as_str().unwrap_or("").to_string(),
                state: value["state"].as_str().unwrap_or("open").to_string(),
                evidence: value["evidence"].clone(),
                fired_event_id: value["fired_event_id"].as_i64().unwrap_or(0),
                fired_at_ms: value["fired_at_ms"].as_i64().unwrap_or(0),
            },
        ))
    }
}

/// A task, as the fold would have it: the state the records left, and what
/// the delivery bound needs.
#[derive(Debug, Clone)]
struct Task {
    mission: i64,
    state: Option<String>,
    agent: String,
    objective: Option<i64>,
    revisions: i64,
    /// Its `task.assigned` record and time.
    assigned_event: Option<i64>,
    assigned_at: i64,
    /// Whether its agent holds no delivered or reported task.
    agent_free: bool,
}

/// A mission's five watched fields, as the fold would have them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Mission {
    state: String,
    halted: Option<String>,
    rejections: i64,
    /// The objective of every rejection in the run of `rejections`, as the
    /// fold keeps it (`gate_rejection_objective`): a cancel of a task of it
    /// ends the run.
    run_objective: Option<i64>,
    aborts: i64,
    escalated: bool,
    accept: Option<String>,
}

/// An `admission.waiting` with no `admission.admitted` after it yet.
#[derive(Debug, Clone)]
struct Pending {
    event_id: i64,
    at_ms: i64,
    subject: String,
    evidence: Value,
}

/// The newest backup record of one subject (`dump` or `bundle`).
#[derive(Debug, Clone)]
struct Backup {
    event_id: i64,
    at_ms: i64,
    key: Option<String>,
}

/// One database log the crash alert reads.
#[derive(Debug, Clone)]
struct LogWatch {
    subject: &'static str,
    path: Option<PathBuf>,
    offset: u64,
    seated: bool,
    carry: String,
    said: bool,
    last_match_ms: i64,
    last_line: Option<String>,
}

/// The loop's alert step: what it knows, and what it records.
pub(crate) struct Monitor {
    settings: Settings,
    started: bool,
    /// The last event id the step looked at.
    watermark: i64,
    /// When the host and the logs are read next, on the database's clock.
    next_reading_ms: i64,
    /// The earliest bound that has not fired yet.
    next_bound_ms: Option<i64>,
    /// Every alert the store holds, by key: the record a restart reads back.
    known: BTreeMap<String, Row>,
    tasks: HashMap<i64, Task>,
    missions: HashMap<i64, Mission>,
    /// The pending admission waits, by kind and agent or task.
    admitted: HashMap<String, Pending>,
    /// The model judge's `down` records with no `up` after them, by judge.
    judge_down: HashMap<String, Pending>,
    /// An open (or pending) model-login alert key, by execution id.
    auth: HashMap<String, String>,
    /// The open (or pending) `scenario_red` key, by mission and scenario.
    red: HashMap<(i64, String), String>,
    /// The open (or pending) `validator_defect` key, by mission, scenario
    /// and whether an owner's landing was accepted without it (`owner`).
    defects: HashMap<(i64, String, bool), String>,
    /// The newest dump and bundle records.
    backups: BTreeMap<&'static str, Backup>,
    commands_after: i64,
    logs: Vec<LogWatch>,
    /// The first reading of the current run that found no integrator
    /// holding its lock while reported work waited (`integrator_down`).
    unlocked_since: Option<i64>,
    /// The first reading of the current run that found each mission's main
    /// ahead of its controller (`controller_behind`).
    behind_since: BTreeMap<i64, i64>,
    /// The (mission, source) pairs the last reading found away with no
    /// alert yet, and when it did (`source_away` fires on the second
    /// reading in a row, so a path that comes and goes does not flap). A
    /// restart forgets them: its first reading starts the count again.
    away_once: BTreeMap<(i64, String), i64>,
    /// When the earliest pending message reaches its bound, as the last
    /// reading of the messages found (`message_not_taken`).
    messages_due: Option<i64>,
    /// The newest command the messages' reading has seen: the next reads
    /// the commands after it.
    messages_after: i64,
    /// The tracked messages not taken, not alerted and whose session had not
    /// ended with them when last read, by the time each falls due: read
    /// again then.
    pending: BTreeMap<i64, i64>,
    /// From when the next reading looks for a `message.taken` or an
    /// `execution.ended` that may clear an open alert: the last reading's
    /// time, less a margin for an append that committed late.
    messages_since: i64,
    /// What the supervisor had to dispatch when last read
    /// ([`crate::serve::attention::supervisor_dispatch`]), read only while
    /// its latest judgement is stuck or its `agent_stuck` is open; `None`
    /// before a read (or after one that failed): its judgement alone
    /// decides, as before objective 057.
    supervisor: Option<Dispatch>,
    /// When the supervisor's dispatch may be read again: at most once per
    /// `SPAWN_ALERT_CHECK_SECS`.
    supervisor_due_ms: i64,
}

/// What the supervisor has to dispatch, as one read of the attention view
/// found it (`agent_stuck`, objective 057).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Dispatch {
    /// It asks the owner a question (the attention view's `question`).
    pub(crate) question: bool,
    /// The plan's pending and told escalations.
    pub(crate) items: Vec<DispatchItem>,
}

/// One escalation of the supervisor's plan.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DispatchItem {
    pub(crate) tag: String,
    /// The agent it is about, when it is about one.
    pub(crate) agent: Option<String>,
    /// It is a worker's own judgement (attention, idle, a nudge that did
    /// not help), not a record the supervisor must act on.
    pub(crate) judgement: bool,
}

impl Dispatch {
    /// From [`crate::serve::attention::supervisor_dispatch`]'s answer.
    pub(crate) fn from_value(value: &Value) -> Self {
        Self {
            question: value["question"].as_bool().unwrap_or(false),
            items: value["items"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|item| DispatchItem {
                    tag: item["tag"].as_str().unwrap_or_default().to_string(),
                    agent: item["agent_id"].as_str().map(str::to_string),
                    judgement: item["judgement"].as_bool().unwrap_or(false),
                })
                .collect(),
        }
    }
}

/// A worker's task states in which its part is done: what it waits on is
/// the gate, a reviewer, the landing or the owner's hold (objective 057). A
/// `reported` task waits for its gate or its review, `accepted` for its
/// landing, `landing` for its push (or behind a hold), `landed` for nothing.
const PART_DONE: [&str; 4] = ["reported", "accepted", "landing", "landed"];

/// The task states in which a worker holds a task: delivered or later, its
/// work (or done), never the supervisor's to assign.
const HELD: [&str; 6] = [
    "delivered",
    "revise",
    "reported",
    "accepted",
    "landing",
    "landed",
];

/// The one command that acts on a worker stuck in its task.
fn restart_command(task: i64) -> String {
    format!("spawn task restart --id {task}")
}

impl Monitor {
    /// The step with the environment's settings; the warnings are said once.
    pub(crate) fn from_env() -> (Self, Vec<String>) {
        let (settings, warnings) = Settings::from_env();
        (Self::with(settings), warnings)
    }

    pub(crate) fn with(settings: Settings) -> Self {
        let logs = vec![
            LogWatch::new("test", settings.test_log.clone()),
            LogWatch::new("live", settings.live_log.clone()),
        ];
        Self {
            settings,
            started: false,
            watermark: 0,
            next_reading_ms: 0,
            next_bound_ms: None,
            known: BTreeMap::new(),
            tasks: HashMap::new(),
            missions: HashMap::new(),
            admitted: HashMap::new(),
            judge_down: HashMap::new(),
            auth: HashMap::new(),
            red: HashMap::new(),
            defects: HashMap::new(),
            backups: BTreeMap::new(),
            commands_after: 0,
            logs,
            unlocked_since: None,
            behind_since: BTreeMap::new(),
            away_once: BTreeMap::new(),
            messages_due: None,
            messages_after: 0,
            pending: BTreeMap::new(),
            messages_since: 0,
            supervisor: None,
            supervisor_due_ms: 0,
        }
    }

    /// One pass: read what is new, decide, record. Returns lines for the
    /// loop's log. Never fails the loop: the caller says the error and tries
    /// again at the next check.
    pub(crate) fn check(&mut self, store: &PostgresStore) -> Result<Vec<String>, String> {
        let mut lines = Vec::new();
        if !self.started {
            self.start(store, &mut lines)?;
        }
        let (watermark, now) = store.alert_clock().map_err(|error| error.to_string())?;
        let reading_due = now >= self.next_reading_ms;
        let bound_due = self.next_bound_ms.is_some_and(|due| now >= due);
        if watermark <= self.watermark && !reading_due && !bound_due {
            return Ok(lines);
        }
        let mut events = Vec::new();
        let moved = watermark > self.watermark;
        if watermark > self.watermark {
            let batch = store
                .alert_control(self.watermark, BATCH)
                .map_err(|error| error.to_string())?;
            let records = batch["records"].as_array().map_or(&[][..], Vec::as_slice);
            // One record at a time, as at the start: a halt and its resume
            // in one batch end cleared, naming the resume.
            for entry in records {
                let id = entry["event_id"].as_i64().unwrap_or_default();
                let at = entry["at_ms"].as_i64().unwrap_or(now);
                let mut fed = Vec::new();
                self.feed(id, at, &entry["record"], &mut fed);
                self.flush(store, now, &fed, &mut lines);
            }
            // What the step has read: the last control record when the
            // batch was cut at its limit (the rest waits for the next
            // check), and otherwise everything up to the clock, which is
            // what makes an idle check one statement (objective 023: the
            // log's watermark, never the clock, says whether there is work).
            self.watermark = if i64::try_from(records.len()).unwrap_or(i64::MAX) < BATCH {
                watermark
            } else {
                batch["next"].as_i64().unwrap_or(self.watermark)
            };
            let observations = store
                .alert_observations()
                .map_err(|error| error.to_string())?;
            self.read_supervisor(store, now, &observations, &mut lines);
            self.observe(&observations, &mut events);
            let commands = store
                .alert_failed_commands(self.commands_after)
                .map_err(|error| error.to_string())?;
            self.commands(&commands, &mut events);
            self.commands_after = now.saturating_sub(1000);
        }
        // The messages are read when the log moved (a new command, a taking,
        // an end all append) and when a pending one falls due: the new
        // commands, the pending ones due, and the open alerts' whose session
        // took a message or ended since, so an idle pass reads next to
        // nothing (objective 023).
        if moved || self.messages_due.is_some_and(|due| now >= due) {
            self.read_messages(store, now, &mut events, &mut lines);
        }
        if reading_due {
            self.next_reading_ms = now.saturating_add(self.settings.check_ms);
            self.read_host(now, &mut events, &mut lines);
            self.read_integrator(store, now, &mut events, &mut lines);
            self.read_controllers(store, now, &mut events, &mut lines);
            self.read_sources(store, now, &mut events, &mut lines);
            self.read_budgets(store, now, &mut events, &mut lines);
        }
        self.bounds(now, &mut events);
        self.flush(store, now, &events, &mut lines);
        self.plan(now);
        Ok(lines)
    }

    /// The earliest time the step waits for even when no record is
    /// written: its next host reading (`SPAWN_ALERT_CHECK_SECS`), or a
    /// bound's due time. Before the first check it is `0`, so a fresh loop
    /// runs it once.
    pub(crate) fn due_ms(&self) -> i64 {
        self.next_bound_ms.map_or(self.next_reading_ms, |bound| {
            bound.min(self.next_reading_ms)
        })
    }

    /// Read what the step needs back: its alerts, the missions and tasks, the
    /// pending records, and where the event log stands.
    fn start(&mut self, store: &PostgresStore, lines: &mut Vec<String>) -> Result<(), String> {
        let mirror = store.alert_mirror().map_err(|error| error.to_string())?;
        let now = mirror["now_ms"]
            .as_i64()
            .unwrap_or_else(|| store.now_ms().unwrap_or_default());
        for mission in mirror["missions"].as_array().into_iter().flatten() {
            let id = mission["id"].as_i64().unwrap_or_default();
            self.missions.insert(
                id,
                Mission {
                    state: mission["state"].as_str().unwrap_or("open").to_string(),
                    halted: mission["halted"].as_str().map(str::to_string),
                    rejections: mission["gate_rejections"].as_i64().unwrap_or_default(),
                    run_objective: mission["gate_rejection_objective"].as_i64(),
                    aborts: mission["gate_aborts"].as_i64().unwrap_or_default(),
                    escalated: mission["gate_room_escalated"].as_bool().unwrap_or(false),
                    accept: mission["accept"].as_str().map(str::to_string),
                },
            );
        }
        for task in mirror["tasks"].as_array().into_iter().flatten() {
            let id = task["id"].as_i64().unwrap_or_default();
            self.tasks.insert(
                id,
                Task {
                    mission: task["mission"].as_i64().unwrap_or_default(),
                    state: task["state"].as_str().map(str::to_string),
                    agent: task["agent_id"].as_str().unwrap_or("").to_string(),
                    objective: task["objective"].as_i64(),
                    revisions: task["revisions"].as_i64().unwrap_or_default(),
                    assigned_event: task["assigned_event"].as_i64(),
                    assigned_at: task["assigned_at_ms"].as_i64().unwrap_or_default(),
                    agent_free: task["agent_free"].as_bool().unwrap_or(true),
                },
            );
        }
        let rows = store.alert_rows().map_err(|error| error.to_string())?;
        for row in rows["rows"].as_array().into_iter().flatten() {
            if let Some((key, row)) = Row::from_value(row) {
                self.known.insert(key, row);
            }
        }
        self.seed_red();
        self.seed_defects();
        let history = store
            .alert_history(now.saturating_sub(HISTORY_MS), HISTORY_LIMIT)
            .map_err(|error| error.to_string())?;
        // Each record is recorded before the next is read, so a later record
        // in the same history (a halt's resume, a good drill after a failed
        // one) finds the alert its predecessor fired and clears it, naming
        // itself; a record older than an open alert's own clears nothing
        // ([`after`]).
        for entry in history.as_array().into_iter().flatten() {
            let id = entry["event_id"].as_i64().unwrap_or_default();
            let at = entry["at_ms"].as_i64().unwrap_or(now);
            let mut fed = Vec::new();
            self.feed(id, at, &entry["record"], &mut fed);
            self.flush(store, now, &fed, lines);
        }
        let mut events = Vec::new();
        let observations = store
            .alert_observations()
            .map_err(|error| error.to_string())?;
        self.read_supervisor(store, now, &observations, lines);
        self.observe(&observations, &mut events);
        self.commands_after = now.saturating_sub(COMMAND_LOOKBACK_MS);
        let commands = store
            .alert_failed_commands(self.commands_after)
            .map_err(|error| error.to_string())?;
        self.commands(&commands, &mut events);
        // The messages, read back as they stand: an alert the store holds
        // fires nothing again, and one due while the loop was down fires.
        let messages = store
            .alert_messages_all(self.settings.message_taken_ms, &self.open_messages())
            .map_err(|error| error.to_string())?;
        self.messages(&messages, &mut events);
        self.watermark = store.alert_clock().map_err(|error| error.to_string())?.0;
        for log in &mut self.logs {
            let open = self
                .known
                .iter()
                .filter(|(_, row)| {
                    row.open() && row.kind == "postgres_crash" && row.subject == log.subject
                })
                .max_by_key(|(_, row)| row.fired_at_ms);
            if let Some((_, row)) = open {
                log.last_match_ms = row.fired_at_ms;
                log.last_line = row.evidence["lines"]
                    .as_array()
                    .and_then(|lines| lines.last())
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
        }
        self.flush(store, now, &events, lines);
        self.started = true;
        self.next_reading_ms = now;
        self.plan(now);
        Ok(())
    }

    /// One control record the step has not seen: fold it into what the step
    /// knows and decide the alerts it makes.
    fn feed(&mut self, event_id: i64, at_ms: i64, record: &Value, out: &mut Vec<Event>) {
        let Some(kind) = record["type"].as_str() else {
            return;
        };
        match kind {
            "task.assigned" | "task.delivered" | "task.reported" | "task.gated"
            | "task.reviewed" | "task.landed" | "task.aborted" | "task.over_budget"
            | "task.cancelled" => self.task_record(event_id, at_ms, record, out),
            "mission.added" | "mission.closed" | "mission.halted" | "mission.resumed"
            | "mission.configured" => self.mission_record(event_id, record, out),
            "admission.waiting" | "admission.admitted" => {
                self.admission_record(event_id, at_ms, record, out);
            }
            "backup.dumped" | "backup.bundled" => self.backup_record(event_id, at_ms, record, out),
            "backup.verified" => self.verified_record(event_id, record, out),
            "execution.interrupted" | "execution.cleared" => {
                self.auth_record(event_id, record, out);
            }
            "host.judge" => self.judge_record(event_id, at_ms, record, out),
            "source.moved" => self.owner_landing(event_id, record, out),
            _ => {}
        }
    }

    /// The open `validator_defect` alerts the store holds, by mission,
    /// scenario and origin.
    fn seed_defects(&mut self) {
        for (key, row) in &self.known {
            if row.open() && row.kind == "validator_defect" {
                let mission = row.evidence["mission"].as_i64();
                let scenario = row.evidence["scenario"].as_str();
                let owner = row.evidence["owner"].as_bool().unwrap_or(false);
                if let (Some(mission), Some(scenario)) = (mission, scenario) {
                    self.defects
                        .insert((mission, scenario.to_string(), owner), key.clone());
                }
            }
        }
    }

    /// `validator_defect` from an owner's accepted landing that the gate
    /// judged without a validator failing on the base too (its
    /// `validator_defects`): one per mission the move names and scenario,
    /// cleared only once a gate of the mission names it `quarantined` (the
    /// owner's decision). Nothing else clears it: a later gate that does not
    /// name it may have judged the scenario unsolved on the base, not
    /// passed (review-fix-gate.md F1). A `quarantined` the move names clears
    /// the missions' alerts for it.
    fn owner_landing(&mut self, event_id: i64, record: &Value, out: &mut Vec<Event>) {
        let missions: Vec<i64> = record["missions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_i64)
            .collect();
        let request = record["request"].as_i64();
        for mission in &missions {
            self.quarantine_clears(*mission, record, event_id, out);
            for scenario in names(&record["validator_defects"]) {
                let at = (*mission, scenario.clone(), true);
                if self.defects.contains_key(&at) {
                    continue;
                }
                let key = format!("validator_defect:{mission}:{scenario}:{event_id}");
                self.defects.insert(at, key.clone());
                out.push(Event::Fire {
                    key,
                    kind: "validator_defect",
                    subject: format!("mission #{mission}"),
                    evidence: json!({
                        "mission": mission, "scenario": scenario, "owner": true,
                        "request": request, "event_id": event_id, "to": record["to"],
                    }),
                    text: format!(
                        "mission #{mission}: the owner's landing (request #{}) was accepted \
                         without {scenario}, which failed on main too: until it is repaired, \
                         nothing checks what it checks. Today only a gate naming it \
                         `quarantined` clears this alert: the gate's verdict cannot yet tell a \
                         repaired validator from one it skipped",
                        request.unwrap_or_default()
                    ),
                    notice: false,
                });
            }
        }
    }

    /// `validator_defect` from a task's gate that ended on one (`host_fault`,
    /// evidence `validator_defect`): one per mission and scenario, cleared
    /// at the mission's next gate verdict (`accepted` or `rejected`: its
    /// gates judge again) or once a gate names it `quarantined`.
    fn task_defects(
        &mut self,
        event_id: i64,
        task: i64,
        mission: i64,
        record: &Value,
        out: &mut Vec<Event>,
    ) {
        self.quarantine_clears(mission, record, event_id, out);
        if matches!(record["verdict"].as_str(), Some("accepted" | "rejected")) {
            let judged: Vec<(i64, String, bool)> = self
                .defects
                .keys()
                .filter(|(at, _, owner)| *at == mission && !owner)
                .cloned()
                .collect();
            for at in judged {
                if let Some(key) = self.defects.remove(&at) {
                    out.push(Event::Clear {
                        key,
                        evidence: json!({"event_id": event_id, "record": "task.gated",
                                         "task": task, "verdict": record["verdict"]}),
                    });
                }
            }
        }
        if record["verdict"].as_str() != Some("host_fault")
            || record["evidence"].as_str() != Some("validator_defect")
        {
            return;
        }
        for scenario in names(&record["validator_defects"]) {
            let at = (mission, scenario.clone(), false);
            if self.defects.contains_key(&at) {
                continue;
            }
            let key = format!("validator_defect:{mission}:{scenario}:{event_id}");
            self.defects.insert(at, key.clone());
            out.push(Event::Fire {
                key,
                kind: "validator_defect",
                subject: format!("mission #{mission}"),
                evidence: json!({
                    "mission": mission, "scenario": scenario, "owner": false,
                    "task": task, "event_id": event_id,
                }),
                text: format!(
                    "mission #{mission}: {scenario} fails on the base too (task #{task}'s gate): \
                     no task's candidate that fails it gets a verdict until it is fixed or \
                     quarantined"
                ),
                notice: false,
            });
        }
    }

    /// Clear the mission's `validator_defect` alerts, of either origin, for
    /// each scenario the record names `quarantined`.
    fn quarantine_clears(
        &mut self,
        mission: i64,
        record: &Value,
        event_id: i64,
        out: &mut Vec<Event>,
    ) {
        for scenario in names(&record["quarantined"]) {
            for owner in [false, true] {
                if let Some(key) = self.defects.remove(&(mission, scenario.clone(), owner)) {
                    out.push(Event::Clear {
                        key,
                        evidence: json!({"event_id": event_id, "record": record["type"],
                                         "quarantined": scenario}),
                    });
                }
            }
        }
    }

    /// The notices and the halt of one mission's record.
    fn mission_record(&mut self, event_id: i64, record: &Value, out: &mut Vec<Event>) {
        let kind = record["type"].as_str().unwrap_or_default();
        let Some(id) = record["id"].as_i64() else {
            return;
        };
        if kind == "mission.added" {
            self.missions.entry(id).or_insert_with(|| Mission {
                state: "open".to_string(),
                halted: None,
                rejections: 0,
                run_objective: None,
                aborts: 0,
                escalated: false,
                accept: None,
            });
        }
        let Some(mission) = self.missions.get_mut(&id) else {
            return;
        };
        let before = snapshot(mission);
        apply_mission(mission, record);
        let after = snapshot(mission);
        let counters = (mission.rejections, mission.aborts);
        if before != after {
            out.push(Event::Fire {
                key: format!("mission_state:{id}:{event_id}"),
                kind: "mission_state",
                subject: format!("mission #{id}"),
                evidence: json!({
                    "mission": id, "record": kind, "event_id": event_id,
                    "before": before, "after": after,
                }),
                text: format!("mission #{id}: {} (by {kind})", changed(&before, &after)),
                notice: true,
            });
        }
        match kind {
            "mission.halted" => {
                let reason = record["reason"].as_str().unwrap_or("halted");
                out.push(Event::Fire {
                    key: format!("mission_halted:{id}:{event_id}"),
                    kind: "mission_halted",
                    subject: format!("mission #{id}"),
                    evidence: json!({
                        "reason": reason, "gate_rejections": counters.0,
                        "gate_aborts": counters.1, "event_id": event_id,
                    }),
                    text: format!("mission #{id} halted: {reason}"),
                    notice: false,
                });
            }
            "mission.resumed" => {
                for key in self.open_keys("mission_halted", &format!("mission #{id}")) {
                    out.push(Event::Clear {
                        key,
                        evidence: json!({"event_id": event_id, "record": "mission.resumed"}),
                    });
                }
            }
            _ => {}
        }
    }

    /// The open `scenario_red` alerts the store holds, by mission and
    /// scenario: a main still red on a scenario stays one alert, however old
    /// its first record is (the history read back on start is a week).
    fn seed_red(&mut self) {
        for (key, row) in &self.known {
            if row.open() && row.kind == "scenario_red" {
                let mission = row.evidence["mission"].as_i64();
                let scenario = row.evidence["scenario"].as_str();
                if let (Some(mission), Some(scenario)) = (mission, scenario) {
                    self.red
                        .insert((mission, scenario.to_string()), key.clone());
                }
            }
        }
    }

    /// `scenario_red`: opened by a gating that names a scenario main is red
    /// on, once per mission and scenario; cleared by a later accepted gating
    /// of the mission that does not name it.
    fn red_scenarios(
        &mut self,
        event_id: i64,
        task: i64,
        mission_id: i64,
        record: &Value,
        out: &mut Vec<Event>,
    ) {
        let named: Vec<String> = record["main_red_scenarios"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        for scenario in &named {
            let at = (mission_id, scenario.clone());
            if self.red.contains_key(&at) {
                continue;
            }
            let key = format!("scenario_red:{mission_id}:{scenario}:{event_id}");
            self.red.insert(at, key.clone());
            out.push(Event::Fire {
                key,
                kind: "scenario_red",
                subject: format!("mission #{mission_id}"),
                evidence: json!({
                    "mission": mission_id, "scenario": scenario, "task": task,
                    "base": record["base"], "event_id": event_id,
                }),
                text: format!(
                    "mission #{mission_id}: main is red on {scenario}: solved on an ancestor of \
the base, unsolved on it (task #{task}'s gate)"
                ),
                notice: false,
            });
        }
        if record["verdict"].as_str() != Some("accepted") {
            return;
        }
        let green: Vec<(i64, String)> = self
            .red
            .keys()
            .filter(|(mission, scenario)| *mission == mission_id && !named.contains(scenario))
            .cloned()
            .collect();
        for at in green {
            if let Some(key) = self.red.remove(&at) {
                out.push(Event::Clear {
                    key,
                    evidence: json!({
                        "event_id": event_id, "record": "task.gated", "task": task,
                        "base": record["base"],
                    }),
                });
            }
        }
    }

    /// A task record: its state notice, its mission's counters when it was
    /// gated, 033's notice, and the delivery bound's clearing.
    fn task_record(&mut self, event_id: i64, at_ms: i64, record: &Value, out: &mut Vec<Event>) {
        let kind = record["type"].as_str().unwrap_or_default();
        let Some(id) = record["id"].as_i64() else {
            return;
        };
        if kind == "task.assigned" {
            self.tasks.entry(id).or_insert_with(|| Task {
                mission: record["mission_id"].as_i64().unwrap_or_default(),
                state: None,
                agent: record["agent_id"].as_str().unwrap_or("").to_string(),
                objective: record["objective"].as_i64(),
                revisions: 0,
                assigned_event: Some(event_id),
                assigned_at: at_ms,
                agent_free: true,
            });
        }
        let Some(task) = self.tasks.get(&id).cloned() else {
            return;
        };
        let accept_gate = self
            .missions
            .get(&task.mission)
            .is_some_and(|mission| mission.accept.as_deref() == Some("gate"));
        let guarded = record["revision"]
            .as_i64()
            .is_none_or(|revision| revision == task.revisions);
        if kind == "task.gated" && guarded {
            self.gating(event_id, id, task.mission, record, out);
        }
        if kind == "task.cancelled" {
            // Whoever cancels it (a yield, the owner, an objective solved on
            // the base), as the fold: a cancel of the run's objective ends it.
            self.mission_change(event_id, task.mission, kind, out, |mission| {
                apply_cancel(mission, task.objective);
            });
        }
        let after = task_state_after(record, accept_gate, task.revisions);
        if let Some(after) = after {
            if bumps_revision(record)
                && let Some(entry) = self.tasks.get_mut(&id)
            {
                entry.revisions += 1;
            }
            if task.state.as_deref() != Some(after.as_str()) {
                if let Some(entry) = self.tasks.get_mut(&id) {
                    entry.state = Some(after.clone());
                }
                out.push(Event::Fire {
                    key: format!("task_state:{id}:{event_id}"),
                    kind: "task_state",
                    subject: format!("task #{id}"),
                    evidence: json!({
                        "task": id, "mission": task.mission, "objective": task.objective,
                        "agent_id": task.agent.clone(), "from": task.state.clone(),
                        "to": after, "record": kind, "event_id": event_id,
                    }),
                    text: format!(
                        "task #{id}: {} -> {after} (by {kind})",
                        task.state.as_deref().unwrap_or("none")
                    ),
                    notice: true,
                });
                if PART_DONE.contains(&after.as_str()) {
                    self.part_done(event_id, id, &task.agent, kind, &after, out);
                }
            }
        }
        if self
            .tasks
            .get(&id)
            .is_some_and(|task| task.state.as_deref() != Some("assigned"))
            && let Some(assigned) = task.assigned_event
            && self.is_open(&format!("task_undelivered:{id}:{assigned}"))
        {
            out.push(Event::Clear {
                key: format!("task_undelivered:{id}:{assigned}"),
                evidence: json!({"event_id": event_id, "record": kind}),
            });
        }
    }

    /// A worker's task moved past its part ([`PART_DONE`]) by the record
    /// `event_id`: the worker's open `agent_stuck` (on this task, or one
    /// that names none) clears at it, naming it (objective 057).
    fn part_done(
        &mut self,
        event_id: i64,
        task: i64,
        agent: &str,
        record: &str,
        state: &str,
        out: &mut Vec<Event>,
    ) {
        let open: Vec<String> = self
            .known
            .iter()
            .filter(|(_, row)| {
                row.open()
                    && row.kind == "agent_stuck"
                    && row.evidence["agent_id"].as_str() == Some(agent)
                    && row.evidence["task"]
                        .as_i64()
                        .is_none_or(|named| named == task)
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in open {
            out.push(Event::Clear {
                key,
                evidence: json!({
                    "event_id": event_id, "record": record, "task": task, "task_state": state,
                }),
            });
        }
    }

    /// The `mission_state` notice of a record after which a mission's
    /// watched fields differ (`change` applies the record's fold).
    fn mission_change(
        &mut self,
        event_id: i64,
        mission_id: i64,
        kind: &str,
        out: &mut Vec<Event>,
        change: impl FnOnce(&mut Mission),
    ) {
        let Some(mission) = self.missions.get_mut(&mission_id) else {
            return;
        };
        let before = snapshot(mission);
        change(mission);
        let after = snapshot(mission);
        if before != after {
            out.push(Event::Fire {
                key: format!("mission_state:{mission_id}:{event_id}"),
                kind: "mission_state",
                subject: format!("mission #{mission_id}"),
                evidence: json!({
                    "mission": mission_id, "record": kind, "event_id": event_id,
                    "before": before, "after": after,
                }),
                text: format!(
                    "mission #{mission_id}: {} (by {kind})",
                    changed(&before, &after)
                ),
                notice: true,
            });
        }
    }

    /// A gating's mission counters, and 033's base disagreement.
    fn gating(
        &mut self,
        event_id: i64,
        task: i64,
        mission_id: i64,
        record: &Value,
        out: &mut Vec<Event>,
    ) {
        let objective = self.tasks.get(&task).and_then(|task| task.objective);
        self.mission_change(event_id, mission_id, "task.gated", out, |mission| {
            apply_gating(mission, record, objective);
        });
        self.red_scenarios(event_id, task, mission_id, record, out);
        self.task_defects(event_id, task, mission_id, record, out);
        if base_disagrees(record).is_some() {
            out.push(Event::Fire {
                key: format!("gate_base_disagrees:{event_id}"),
                kind: "gate_base_disagrees",
                subject: format!("mission #{mission_id}"),
                evidence: json!({
                    "task": task, "mission": mission_id,
                    "scenario": record["scenario"], "exits": record["exits"],
                    "base_disagrees": record["base_disagrees"],
                    "setup_s": record.get("setup_s").cloned().unwrap_or(Value::Null),
                    "due_ms": record["due_ms"], "event_id": event_id,
                }),
                text: format!(
                    "mission #{mission_id}: the base disagrees with itself (task #{task}, {} \
times in a row)",
                    record["base_disagrees"].as_i64().unwrap_or_default()
                ),
                notice: true,
            });
        }
    }

    /// An admission's wait and its clear.
    fn admission_record(
        &mut self,
        event_id: i64,
        at_ms: i64,
        record: &Value,
        out: &mut Vec<Event>,
    ) {
        let kind = record["kind"].as_str().unwrap_or("execution");
        let matched = if kind == "execution" {
            format!(
                "execution:{}",
                record["agent_id"].as_str().unwrap_or_default()
            )
        } else {
            format!("gate:{}", record["task"].as_i64().unwrap_or_default())
        };
        match record["type"].as_str().unwrap_or_default() {
            "admission.waiting" => {
                let subject = if kind == "execution" {
                    record["agent_id"]
                        .as_str()
                        .unwrap_or("an execution")
                        .to_string()
                } else {
                    record["task"]
                        .as_i64()
                        .map_or_else(|| "a gate".to_string(), |task| format!("task #{task}"))
                };
                self.admitted.insert(
                    matched,
                    Pending {
                        event_id,
                        at_ms,
                        subject,
                        evidence: json!({
                            "kind": kind, "envelope_bytes": record["envelope_bytes"],
                            "measure": record["measure"],
                            "measured_bytes": record["measured_bytes"],
                            "threshold_bytes": record["threshold_bytes"],
                            "event_id": event_id,
                        }),
                    },
                );
            }
            "admission.admitted" => {
                if let Some(pending) = self.admitted.remove(&matched) {
                    out.push(Event::Clear {
                        key: format!("admission_wait:{}", pending.event_id),
                        evidence: json!({
                            "waited_ms": record["waited_ms"], "event_id": event_id,
                        }),
                    });
                }
            }
            _ => {}
        }
    }

    /// The model judge's crossing: a `down` waits for its bound, an `up`
    /// clears it (or forgets the `down` before it fired).
    fn judge_record(&mut self, event_id: i64, at_ms: i64, record: &Value, out: &mut Vec<Event>) {
        let judge = record["judge"].as_str().unwrap_or("judge").to_string();
        match record["state"].as_str() {
            Some("down") => {
                // A second `down` with no `up` between keeps the first: the
                // judge has answered nothing since it.
                // The bound runs from the first failed call, when known.
                let first = record["first_failed_ms"]
                    .as_i64()
                    .unwrap_or(at_ms)
                    .min(at_ms);
                self.judge_down.entry(judge.clone()).or_insert(Pending {
                    event_id,
                    at_ms: first,
                    subject: judge,
                    evidence: json!({"error": scrub(record["error"].as_str().unwrap_or_default())}),
                });
            }
            Some("up") => {
                if let Some(pending) = self.judge_down.remove(&judge) {
                    out.push(Event::Clear {
                        key: format!("judge_down:{}", pending.event_id),
                        evidence: json!({
                            "event_id": event_id,
                            "failed_calls": record["failed_calls"],
                            "down_ms": at_ms.saturating_sub(pending.at_ms),
                        }),
                    });
                } else {
                    // Its `down` fell out of the history: the open alert
                    // still clears, without a span.
                    for key in self.open_keys("judge_down", &judge) {
                        out.push(Event::Clear {
                            key,
                            evidence: json!({
                                "event_id": event_id,
                                "failed_calls": record["failed_calls"],
                            }),
                        });
                    }
                }
            }
            _ => {}
        }
    }

    /// A backup record: the stale one it clears, and 031's over-alert budget.
    fn backup_record(&mut self, event_id: i64, at_ms: i64, record: &Value, out: &mut Vec<Event>) {
        let subject = if record["type"] == "backup.dumped" {
            "dump"
        } else {
            "bundle"
        };
        let key = record["keys"]
            .as_array()
            .and_then(|keys| keys.first())
            .and_then(Value::as_str)
            .map(str::to_string);
        self.backups.insert(
            subject,
            Backup {
                event_id,
                at_ms,
                key: key.clone(),
            },
        );
        for open in self.open_keys("backup_stale", subject) {
            out.push(Event::Clear {
                key: open,
                evidence: json!({"key": key, "event_id": event_id}),
            });
        }
        match record["over_alert"].as_bool() {
            Some(true) if self.open_keys("backup_budget", "r2").is_empty() => {
                out.push(Event::Fire {
                    key: format!("backup_budget:{event_id}"),
                    kind: "backup_budget",
                    subject: "r2".to_string(),
                    evidence: json!({
                        "held_bytes": record["held_bytes"],
                        "written_bytes": record["written_bytes"],
                        "alert_bytes": record["alert_bytes"],
                        "limit_bytes": record["limit_bytes"],
                        "skipped": record["skipped"], "event_id": event_id,
                    }),
                    text: format!(
                        "R2 holds {} bytes, over its alert threshold of {} (031)",
                        record["held_bytes"].as_i64().unwrap_or_default(),
                        record["alert_bytes"].as_i64().unwrap_or_default()
                    ),
                    notice: false,
                });
            }
            Some(false) => {
                for open in self.open_keys("backup_budget", "r2") {
                    out.push(Event::Clear {
                        key: open,
                        evidence: json!({
                            "held_bytes": record["held_bytes"], "event_id": event_id,
                        }),
                    });
                }
            }
            _ => {}
        }
    }

    /// A restore drill's verdict.
    fn verified_record(&mut self, event_id: i64, record: &Value, out: &mut Vec<Event>) {
        if record["ok"].as_bool() == Some(false) {
            out.push(Event::Fire {
                key: format!("drill_failed:{event_id}"),
                kind: "drill_failed",
                subject: "drill".to_string(),
                evidence: json!({
                    "key": record["key"], "reason": record["reason"],
                    "events": record["events"], "max_event_id": record["max_event_id"],
                    "expected": record["expected"], "event_id": event_id,
                }),
                text: format!(
                    "the restore drill failed: {}",
                    record["reason"].as_str().unwrap_or("no reason given")
                ),
                notice: false,
            });
        } else {
            for open in self.open_keys("drill_failed", "drill") {
                out.push(Event::Clear {
                    key: open,
                    evidence: json!({"key": record["key"], "event_id": event_id}),
                });
            }
        }
    }

    /// A model login that expired, and the clear that a person fixed it.
    fn auth_record(&mut self, event_id: i64, record: &Value, out: &mut Vec<Event>) {
        let execution = record["execution_id"].as_str().unwrap_or_default();
        match record["type"].as_str().unwrap_or_default() {
            "execution.interrupted" if record["cause"].as_str() == Some("model_auth") => {
                let key = format!("model_auth:{event_id}");
                self.auth.insert(execution.to_string(), key.clone());
                out.push(Event::Fire {
                    key,
                    kind: "model_auth",
                    subject: record["session_id"]
                        .as_str()
                        .unwrap_or("an execution")
                        .to_string(),
                    evidence: json!({
                        "agent_id": record["agent_id"], "execution_id": record["execution_id"],
                        "evidence": record["evidence"], "event_id": event_id,
                    }),
                    text: "the model login expired: a person must log the engine in again"
                        .to_string(),
                    notice: false,
                });
            }
            "execution.cleared" => {
                if let Some(key) = self.auth.remove(execution) {
                    out.push(Event::Clear {
                        key,
                        evidence: json!({
                            "waited_ms": record["waited_ms"], "event_id": event_id,
                        }),
                    });
                }
            }
            _ => {}
        }
    }

    /// Every session's latest judgement: a stuck one fires once, a calm one
    /// clears, and so does the end of the session's current execution,
    /// whatever its cause. An ended session fires nothing: the Observer
    /// judges once more after every end, so a judgement can be newer than
    /// the end it follows, and a restart that reads an old stuck judgement
    /// of a session that has ended must not open it again.
    ///
    /// A stuck judgement is a claim that someone must act (objective 057),
    /// so it fires nothing for an agent that waits because nothing is its
    /// to do: a worker whose task is past its part ([`PART_DONE`]: it waits
    /// for the gate, a reviewer, its landing or the owner's hold; its open
    /// alert cleared at the task record that moved it, [`Self::task_record`])
    /// and the supervisor with nothing to dispatch ([`Self::supervisor_work`];
    /// its open alert clears then). A worker's alert names its task and the
    /// one command that acts on it, `spawn task restart --id <task>` (a
    /// delivered task: the only one a restart takes).
    fn observe(&mut self, observations: &Value, out: &mut Vec<Event>) {
        for observation in observations.as_array().into_iter().flatten() {
            let session = observation["session_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let event_id = observation["event_id"].as_i64().unwrap_or_default();
            let judgement = &observation["observation"];
            let stuck = judgement["stuck"].as_f64();
            let open = self.open_keys("agent_stuck", &session);
            let ended = observation["ended_event"].as_i64();
            let is_supervisor = session == crate::supervise::SUPERVISOR_SESSION;
            let held = if is_supervisor {
                None
            } else {
                observation["agent_id"]
                    .as_str()
                    .and_then(|agent| self.held_task(agent))
            };
            let done = held
                .as_ref()
                .is_some_and(|(_, state)| PART_DONE.contains(&state.as_str()));
            let dispatch = if is_supervisor {
                self.supervisor_work()
            } else {
                None
            };
            let idle_supervisor = dispatch.as_ref().is_some_and(Vec::is_empty);
            if let Some(ended) = ended {
                for key in open {
                    out.push(Event::Clear {
                        key,
                        evidence: json!({
                            "event_id": ended,
                            "cause": observation["ended_cause"],
                        }),
                    });
                }
            } else if stuck.is_some_and(|stuck| stuck < LIKELY) {
                for key in open {
                    out.push(Event::Clear {
                        key,
                        evidence: json!({"stuck": stuck, "event_id": event_id}),
                    });
                }
            } else if done {
                // Its task's record cleared it; one still open (a record
                // read before a restart) clears at a later judgement.
                let (task, state) = held.clone().unwrap_or_default();
                for key in open {
                    out.push(Event::Clear {
                        key,
                        evidence: json!({"event_id": event_id, "task": task, "task_state": state}),
                    });
                }
            } else if idle_supervisor {
                for key in open {
                    out.push(Event::Clear {
                        key,
                        evidence: json!({
                            "event_id": self.watermark, "nothing_to_dispatch": true,
                        }),
                    });
                }
            } else if open.is_empty() && stuck.is_some_and(|stuck| stuck >= LIKELY) {
                let mut evidence = json!({
                    "agent_id": observation["agent_id"], "stuck": stuck,
                    "state": judgement["state"], "judge": judgement["judge"],
                    "event_id": event_id,
                });
                let mut text = format!(
                    "{session} is stuck ({:.2}): {}",
                    stuck.unwrap_or_default(),
                    judgement["state"].as_str().unwrap_or("unknown")
                );
                if let Some((task, state)) = &held {
                    let command = (state == "delivered").then(|| restart_command(*task));
                    evidence["task"] = json!(task);
                    evidence["task_state"] = json!(state);
                    evidence["command"] = json!(command);
                    text.push_str(&format!(" on task #{task} ({state})"));
                    if let Some(command) = &command {
                        text.push_str(&format!("; `{command}` gives it a fresh execution"));
                    }
                }
                if let Some(dispatch) = &dispatch {
                    evidence["dispatch"] = json!(dispatch);
                    text.push_str(&format!("; it has to dispatch: {}", dispatch.join(", ")));
                }
                out.push(Event::Fire {
                    key: format!("agent_stuck:{session}:{event_id}"),
                    kind: "agent_stuck",
                    subject: session.clone(),
                    evidence,
                    text,
                    notice: false,
                });
            }
        }
    }

    /// The task `agent` holds: its latest delivered or later ([`HELD`]),
    /// with its state. A task only assigned is not with the worker yet.
    fn held_task(&self, agent: &str) -> Option<(i64, String)> {
        self.tasks
            .iter()
            .filter(|(_, task)| task.agent == agent)
            .filter_map(|(id, task)| {
                task.state
                    .as_deref()
                    .filter(|state| HELD.contains(state))
                    .map(|state| (*id, state.to_string()))
            })
            .max_by_key(|(id, _)| *id)
    }

    /// What the supervisor has to dispatch, from the last read of its plan
    /// ([`Self::read_supervisor`]) and the step's own fold: a question to
    /// the owner, a pending or told escalation (a worker's own judgement
    /// excepted while that worker holds a task: its work, which its own
    /// alert names), and a mission waiting for it to assign its first task
    /// (open or active, not halted, not the integrator's gate, no task
    /// yet). Empty: nothing to dispatch. `None` when its plan was not read.
    fn supervisor_work(&self) -> Option<Vec<String>> {
        let read = self.supervisor.as_ref()?;
        let mut work = Vec::new();
        if read.question {
            work.push("a question to the owner".to_string());
        }
        for item in &read.items {
            let workers = item.judgement
                && item
                    .agent
                    .as_deref()
                    .is_some_and(|agent| self.held_task(agent).is_some());
            if !workers {
                work.push(format!("escalation {}", item.tag));
            }
        }
        let mut missions: Vec<(&i64, &Mission)> = self.missions.iter().collect();
        missions.sort_by_key(|(id, _)| **id);
        for (id, mission) in missions {
            let waiting = matches!(mission.state.as_str(), "open" | "active")
                && mission.halted.is_none()
                && mission.accept.as_deref() != Some("gate")
                && !self.tasks.values().any(|task| task.mission == *id);
            if waiting {
                work.push(format!("mission #{id} waits for its first task"));
            }
        }
        Some(work)
    }

    /// Read what the supervisor has to dispatch, when its `agent_stuck`
    /// depends on it: its latest judgement (in `observations`) is stuck and
    /// its execution has not ended, or its alert is open. At most once per
    /// `SPAWN_ALERT_CHECK_SECS`; a read that fails is said, and its
    /// judgement alone decides until the next.
    fn read_supervisor(
        &mut self,
        store: &PostgresStore,
        now: i64,
        observations: &Value,
        lines: &mut Vec<String>,
    ) {
        let session = crate::supervise::SUPERVISOR_SESSION;
        let stuck = observations
            .as_array()
            .into_iter()
            .flatten()
            .filter(|observation| observation["session_id"] == session)
            .any(|observation| {
                observation["ended_event"].is_null()
                    && observation["observation"]["stuck"]
                        .as_f64()
                        .is_some_and(|stuck| stuck >= LIKELY)
            });
        if !stuck && self.open_keys("agent_stuck", session).is_empty() {
            self.supervisor_due_ms = 0;
            return;
        }
        if now < self.supervisor_due_ms {
            return;
        }
        self.supervisor_due_ms = now.saturating_add(self.settings.check_ms);
        match crate::serve::attention::supervisor_dispatch(store) {
            Ok(value) => self.supervisor = Some(Dispatch::from_value(&value)),
            Err(error) => {
                self.supervisor = None;
                lines.push(format!("alerts: the supervisor's dispatch: {error}"));
            }
        }
    }

    /// Commands that finished failed, an owner's note excepted.
    fn commands(&mut self, commands: &Value, out: &mut Vec<Event>) {
        for command in commands.as_array().into_iter().flatten() {
            let id = command["command"].as_i64().unwrap_or_default();
            let key = command["key"].as_str().unwrap_or_default();
            if key.starts_with("owner-note-")
                || self.known.contains_key(&format!("command_failed:{id}"))
            {
                continue;
            }
            let error = scrub(command["error"].as_str().unwrap_or_default());
            out.push(Event::Fire {
                key: format!("command_failed:{id}"),
                kind: "command_failed",
                subject: format!("command #{id}"),
                evidence: json!({
                    "command": id, "agent_id": command["agent_id"],
                    "session_id": command["session_id"],
                    "principal": command["principal"],
                    "action": command["action"], "key": command["key"], "error": error,
                }),
                text: format!("command #{id} failed: {error}"),
                notice: true,
            });
        }
    }

    /// The commands of the open `message_not_taken` alerts.
    fn open_messages(&self) -> Vec<i64> {
        self.known
            .iter()
            .filter(|(_, row)| row.open() && row.kind == "message_not_taken")
            .filter_map(|(key, _)| key.strip_prefix("message_not_taken:")?.parse().ok())
            .collect()
    }

    /// Read the commands after the last one seen, the pending messages due
    /// at `now`, and the open alerts' that may have cleared
    /// ([`PostgresStore::alert_messages_since`]), and decide. An error is
    /// said and read again at the next pass.
    fn read_messages(
        &mut self,
        store: &PostgresStore,
        now: i64,
        out: &mut Vec<Event>,
        lines: &mut Vec<String>,
    ) {
        let due: Vec<i64> = self
            .pending
            .iter()
            .filter(|(_, due)| **due <= now)
            .map(|(id, _)| *id)
            .collect();
        match store.alert_messages_since(
            self.settings.message_taken_ms,
            self.messages_after,
            &due,
            &self.open_messages(),
            self.messages_since,
        ) {
            Ok(messages) => self.messages(&messages, out),
            Err(error) => lines.push(format!("alerts: messages: {error}")),
        }
    }

    /// `message_not_taken`: a tracked message no `message.taken` names past
    /// its bound, its session not ended with it, fires once; an open one
    /// clears once it is taken, or once its session ended with it untaken.
    /// One not due yet is pending, read again until it is taken, ends or
    /// fires; the next bound is the earliest pending one's.
    fn messages(&mut self, messages: &Value, out: &mut Vec<Event>) {
        for message in messages["messages"].as_array().into_iter().flatten() {
            let Some(id) = message["command_id"].as_i64() else {
                continue;
            };
            let key = format!("message_not_taken:{id}");
            let taken = message["taken"].as_i64().unwrap_or_default();
            let ended = message["ended"].as_bool() != Some(false);
            if self.known.contains_key(&key) {
                self.pending.remove(&id);
                if !self.is_open(&key) {
                    continue;
                }
                if taken > 0 {
                    out.push(Event::Clear {
                        key,
                        evidence: json!({"taken": taken, "taken_at_ms": message["taken_at_ms"]}),
                    });
                } else if ended {
                    out.push(Event::Clear {
                        key,
                        evidence: json!({"ended": true, "state": message["state"]}),
                    });
                }
                continue;
            }
            if message["tracked"].as_bool() != Some(true) || taken > 0 || ended {
                self.pending.remove(&id);
                continue;
            }
            // Pending until its alert is in the store: a fire the store
            // refused for an error is tried again at the next reading.
            let now = messages["now_ms"].as_i64().unwrap_or_default();
            let due = message["due_ms"].as_i64().unwrap_or(now);
            if message["not_taken"].as_bool() != Some(true) {
                self.pending.insert(id, due);
                continue;
            }
            self.pending.insert(id, now);
            let typed = message["typed"].as_bool() == Some(true);
            let agent = message["agent_id"].as_str().unwrap_or_default();
            let action = message["action"].as_str().unwrap_or("input");
            let secs = self.settings.message_taken_ms / 1000;
            out.push(Event::Fire {
                key,
                kind: "message_not_taken",
                subject: message["session_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                evidence: json!({
                    "command": id, "agent_id": agent, "session_id": message["session_id"],
                    "action": action, "typed": typed,
                    "recorded_at_ms": message["created_at_ms"],
                    "bound_ms": self.settings.message_taken_ms,
                }),
                text: if typed {
                    format!(
                        "{agent} has not taken message #{id} (its {action}): typed, and no \
                         transcript line holds its marker {secs} s after it was sent"
                    )
                } else {
                    format!(
                        "{agent} has not taken message #{id} (its {action}): never typed \
                         {secs} s after it was sent"
                    )
                },
                notice: false,
            });
        }
        if let Some(max) = messages["max_id"].as_i64() {
            self.messages_after = self.messages_after.max(max);
        }
        if let Some(now) = messages["now_ms"].as_i64() {
            self.messages_since = now.saturating_sub(MESSAGES_MARGIN_MS);
        }
        self.messages_due = self.pending.values().copied().min();
    }

    /// The model judge's `down` records that reached their bound
    /// (`judge_down`), once each.
    fn judge_bounds(&self, now: i64, out: &mut Vec<Event>) {
        for pending in self.judge_down.values() {
            let due = pending.at_ms.saturating_add(self.settings.judge_down_ms);
            let key = format!("judge_down:{}", pending.event_id);
            if now < due || self.known.contains_key(&key) {
                continue;
            }
            let error = pending.evidence["error"].as_str().unwrap_or_default();
            out.push(Event::Fire {
                key,
                kind: "judge_down",
                subject: pending.subject.clone(),
                evidence: json!({
                    "judge": pending.subject, "error": error,
                    "down_ms": now.saturating_sub(pending.at_ms),
                    "threshold_ms": self.settings.judge_down_ms,
                    "event_id": pending.event_id,
                }),
                text: format!(
                    "the model judge {} failed two calls in a row, the first {} s ago, \
                     and has answered none since: {error}",
                    pending.subject,
                    now.saturating_sub(pending.at_ms) / 1000
                ),
                notice: false,
            });
        }
    }

    /// The bounds that fall due: an undelivered task, an admission's wait, a
    /// judge that answers nothing, a backup's age, a crash line gone quiet.
    fn bounds(&mut self, now: i64, out: &mut Vec<Event>) {
        for (id, task) in &self.tasks {
            let Some(assigned) = task.assigned_event else {
                continue;
            };
            if task.state.as_deref() != Some("assigned") || !task.agent_free {
                continue;
            }
            let due = task
                .assigned_at
                .saturating_add(self.settings.undelivered_ms);
            if now < due {
                continue;
            }
            let key = format!("task_undelivered:{id}:{assigned}");
            if self.known.contains_key(&key) {
                continue;
            }
            out.push(Event::Fire {
                key,
                kind: "task_undelivered",
                subject: format!("task #{id}"),
                evidence: json!({
                    "task": id, "mission": task.mission, "agent_id": task.agent,
                    "assigned_event_id": assigned, "assigned_at_ms": task.assigned_at,
                    "waited_ms": now.saturating_sub(task.assigned_at),
                    "threshold_ms": self.settings.undelivered_ms,
                    "event_id": assigned,
                }),
                text: format!(
                    "task #{id} has been assigned for {} s with its agent free",
                    now.saturating_sub(task.assigned_at) / 1000
                ),
                notice: false,
            });
        }
        for (matched, pending) in &self.admitted {
            let due = pending.at_ms.saturating_add(self.settings.admission_ms);
            let key = format!("admission_wait:{}", pending.event_id);
            if now < due || self.known.contains_key(&key) {
                continue;
            }
            out.push(Event::Fire {
                key,
                kind: "admission_wait",
                subject: pending.subject.clone(),
                evidence: json!({
                    "kind": pending.evidence["kind"],
                    "envelope_bytes": pending.evidence["envelope_bytes"],
                    "measure": pending.evidence["measure"],
                    "measured_bytes": pending.evidence["measured_bytes"],
                    "threshold_bytes": pending.evidence["threshold_bytes"],
                    "waited_ms": now.saturating_sub(pending.at_ms),
                    "threshold_ms": self.settings.admission_ms,
                    "event_id": pending.event_id,
                }),
                text: format!(
                    "{} has waited {} s to be admitted",
                    matched,
                    now.saturating_sub(pending.at_ms) / 1000
                ),
                notice: false,
            });
        }
        self.judge_bounds(now, out);
        for (subject, max_age) in [
            ("dump", self.settings.dump_max_age_ms),
            ("bundle", self.settings.bundle_max_age_ms),
        ] {
            let Some(backup) = self.backups.get(subject) else {
                continue;
            };
            let due = backup.at_ms.saturating_add(max_age);
            let key = format!("backup_stale:{subject}:{}", backup.event_id);
            if now < due || self.known.contains_key(&key) {
                continue;
            }
            let age = now.saturating_sub(backup.at_ms);
            out.push(Event::Fire {
                key,
                kind: "backup_stale",
                subject: subject.to_string(),
                evidence: json!({
                    "key": backup.key, "age_secs": age / 1000,
                    "max_age_secs": max_age / 1000, "event_id": backup.event_id,
                }),
                text: format!(
                    "the newest {subject} is {} s old (its schedule allows {} s)",
                    age / 1000,
                    max_age / 1000
                ),
                notice: false,
            });
        }
        self.quiet_crashes(now, out);
    }

    /// A crash line that no ready line followed, and that has been silent
    /// for `SPAWN_ALERT_PG_QUIET_SECS`, clears itself.
    fn quiet_crashes(&self, now: i64, out: &mut Vec<Event>) {
        for log in &self.logs {
            let open = self.open_keys("postgres_crash", log.subject);
            let quiet = now.saturating_sub(log.last_match_ms);
            if open.is_empty() || quiet < self.settings.quiet_ms {
                continue;
            }
            for key in open {
                out.push(Event::Clear {
                    key,
                    evidence: json!({
                        "quiet_secs": quiet / 1000, "last_line": log.last_line,
                        "reading_ms": now,
                    }),
                });
            }
        }
    }

    /// The host and the logs, read every `SPAWN_ALERT_CHECK_SECS`.
    fn read_host(&mut self, now: i64, out: &mut Vec<Event>, lines: &mut Vec<String>) {
        self.read_memory(now, out, lines);
        self.read_tmpfs(now, out, lines);
        for index in 0..self.logs.len() {
            self.read_log(index, now, out, lines);
        }
    }

    /// The integrator-down reading (`integrator_down`): read only while
    /// the fold has a reported task of a mission open and not halted, or the
    /// alert is open.
    /// The open and active missions' sources that are paths on this host,
    /// and whether each is a directory now (`source_away`).
    fn read_sources(
        &mut self,
        store: &PostgresStore,
        now: i64,
        out: &mut Vec<Event>,
        lines: &mut Vec<String>,
    ) {
        let listed = match store.alert_mission_sources() {
            Ok(listed) => listed,
            Err(error) => {
                lines.push(format!("alerts: mission sources: {error}"));
                return;
            }
        };
        let watched = host_sources(&listed);
        out.extend(self.source_events(&watched, |path| Path::new(path).is_dir(), now));
    }

    /// What a reading of `watched` (mission, source) decides, given what is
    /// a directory: fire for each source away at this reading and the one
    /// before with none open, clear each open one whose source is back or
    /// no longer watched.
    fn source_events(
        &mut self,
        watched: &BTreeSet<(i64, String)>,
        present: impl Fn(&str) -> bool,
        now: i64,
    ) -> Vec<Event> {
        let mut out = Vec::new();
        let mut open = BTreeSet::new();
        for (key, row) in &self.known {
            if !row.open() || row.kind != "source_away" {
                continue;
            }
            let mission = row
                .subject
                .strip_prefix("mission #")
                .and_then(|id| id.parse::<i64>().ok());
            let source = row.evidence["source"].as_str().map(str::to_string);
            let (Some(mission), Some(source)) = (mission, source) else {
                continue;
            };
            let entry = (mission, source);
            if !watched.contains(&entry) {
                out.push(Event::Clear {
                    key: key.clone(),
                    evidence: json!({
                        "source": entry.1, "reason": "no longer watched", "reading_ms": now,
                    }),
                });
            } else if present(&entry.1) {
                out.push(Event::Clear {
                    key: key.clone(),
                    evidence: json!({"source": entry.1, "present": true, "reading_ms": now}),
                });
            } else {
                open.insert(entry);
            }
        }
        let mut away = BTreeMap::new();
        for (mission, source) in watched {
            let entry = (*mission, source.clone());
            if open.contains(&entry) || present(source) {
                continue;
            }
            // The first reading that finds it away only notes it.
            let Some(since) = self.away_once.get(&entry).copied() else {
                away.insert(entry, now);
                continue;
            };
            out.push(Event::Fire {
                key: format!("source_away:{mission}:{now}"),
                kind: "source_away",
                subject: format!("mission #{mission}"),
                evidence: json!({
                    "source": source, "present": false, "away_since_ms": since,
                    "reading_ms": now,
                }),
                text: format!(
                    "{source} is not a directory on this host: mission #{mission}'s \
                     branches are not published to it until it is back"
                ),
                notice: false,
            });
        }
        self.away_once = away;
        out
    }

    fn read_integrator(
        &mut self,
        store: &PostgresStore,
        now: i64,
        out: &mut Vec<Event>,
        lines: &mut Vec<String>,
    ) {
        if self.settings.integrator_down_ms <= 0 {
            return;
        }
        let waits = self.tasks.values().any(|task| {
            task.state.as_deref() == Some("reported")
                && self.missions.get(&task.mission).is_some_and(|mission| {
                    mission.halted.is_none() && matches!(mission.state.as_str(), "open" | "active")
                })
        });
        if !waits && self.open_keys("integrator_down", INTEGRATOR).is_empty() {
            self.unlocked_since = None;
            return;
        }
        match store.alert_integrator() {
            Ok(reading) => self.integrator(now, &reading, out),
            Err(error) => lines.push(format!("alerts: the integrator's lock: {error}")),
        }
    }

    /// One reading of the integrator's lock and the work waiting for it
    /// (the store's `INTEGRATOR`): fire after the bound of readings with
    /// the lock free and work waiting, clear when the lock is held.
    fn integrator(&mut self, now: i64, reading: &Value, out: &mut Vec<Event>) {
        let Some(locked) = reading["locked"].as_bool() else {
            return;
        };
        let at = reading["now_ms"].as_i64().unwrap_or(now);
        let open = self.open_keys("integrator_down", INTEGRATOR);
        if locked {
            self.unlocked_since = None;
            for key in open {
                out.push(Event::Clear {
                    key,
                    evidence: json!({"locked": true, "reading_ms": at}),
                });
            }
            return;
        }
        let waiting = reading["waiting"].as_array().map_or(&[][..], Vec::as_slice);
        if waiting.is_empty() {
            // Nothing waits: no bound runs (an open alert stays open until
            // the integrator runs again).
            self.unlocked_since = None;
            return;
        }
        let since = *self.unlocked_since.get_or_insert(at);
        let down = at.saturating_sub(since);
        if !open.is_empty() || down < self.settings.integrator_down_ms {
            return;
        }
        let tasks: Vec<String> = waiting
            .iter()
            .take(5)
            .map(|entry| {
                format!(
                    "task #{} of mission #{}",
                    entry["task"].as_i64().unwrap_or_default(),
                    entry["mission"].as_i64().unwrap_or_default()
                )
            })
            .collect();
        out.push(Event::Fire {
            key: format!("integrator_down:{since}"),
            kind: "integrator_down",
            subject: INTEGRATOR.to_string(),
            evidence: json!({
                "since_ms": since, "reading_ms": at, "down_ms": down,
                "threshold_ms": self.settings.integrator_down_ms,
                "waiting": waiting.iter().take(20).collect::<Vec<_>>(),
                "waiting_count": waiting.len(),
            }),
            text: format!(
                "spawn-integrate is not running: no integrator has held its lock for {}s while \
                 {} reported task{} wait{} for a gate ({}{})",
                down / 1000,
                waiting.len(),
                if waiting.len() == 1 { "" } else { "s" },
                if waiting.len() == 1 { "s" } else { "" },
                tasks.join(", "),
                if waiting.len() > tasks.len() {
                    ", …"
                } else {
                    ""
                }
            ),
            notice: false,
        });
    }

    /// The controller-behind reading (`controller_behind`): read only while
    /// the fold has a reported task of a mission open and not halted, or the
    /// alert is open. Each listed mission's owner's latest landing is
    /// compared with its controller in its source.
    fn read_controllers(
        &mut self,
        store: &PostgresStore,
        now: i64,
        out: &mut Vec<Event>,
        lines: &mut Vec<String>,
    ) {
        if self.settings.controller_behind_ms <= 0 {
            return;
        }
        let waits = self.tasks.values().any(|task| {
            task.state.as_deref() == Some("reported")
                && self.missions.get(&task.mission).is_some_and(|mission| {
                    mission.halted.is_none() && matches!(mission.state.as_str(), "open" | "active")
                })
        });
        if !waits
            && !self
                .known
                .values()
                .any(|row| row.open() && row.kind == "controller_behind")
        {
            self.behind_since.clear();
            return;
        }
        let reading = match store.alert_controllers() {
            Ok(reading) => reading,
            Err(error) => {
                lines.push(format!("alerts: the missions' controllers: {error}"));
                return;
            }
        };
        let at = reading["now_ms"].as_i64().unwrap_or(now);
        let mut behind = BTreeMap::new();
        for entry in reading["missions"].as_array().into_iter().flatten() {
            let (Some(mission), Some(gate), Some(source), Some(landed)) = (
                entry["mission"].as_i64(),
                entry["gate"].as_str(),
                entry["source"].as_str(),
                entry["landed"].as_str(),
            ) else {
                continue;
            };
            if !Path::new(source).is_absolute() {
                continue;
            }
            if let Some(controller) =
                crate::integrate::controller_behind(Path::new(source), Path::new(gate), landed)
            {
                behind.insert(mission, (controller, landed.to_string()));
            }
        }
        out.extend(self.controller_events(&behind, at));
    }

    /// What a reading decides, given each waiting mission whose owner's
    /// latest landing is newer than its controller (`behind`: mission to
    /// controller and landing):
    /// fire for each behind for the bound with none open, clear each open
    /// one whose mission is no longer behind.
    fn controller_events(
        &mut self,
        behind: &BTreeMap<i64, (String, String)>,
        now: i64,
    ) -> Vec<Event> {
        let mut out = Vec::new();
        let mut open = BTreeSet::new();
        for (key, row) in &self.known {
            if !row.open() || row.kind != "controller_behind" {
                continue;
            }
            let Some(mission) = row
                .subject
                .strip_prefix("mission #")
                .and_then(|id| id.parse::<i64>().ok())
            else {
                continue;
            };
            if behind.contains_key(&mission) {
                open.insert(mission);
            } else {
                out.push(Event::Clear {
                    key: key.clone(),
                    evidence: json!({"behind": false, "reading_ms": now}),
                });
            }
        }
        self.behind_since
            .retain(|mission, _| behind.contains_key(mission));
        for (mission, (controller, landed)) in behind {
            let since = *self.behind_since.entry(*mission).or_insert(now);
            let behind_ms = now.saturating_sub(since);
            if open.contains(mission) || behind_ms < self.settings.controller_behind_ms {
                continue;
            }
            out.push(Event::Fire {
                key: format!("controller_behind:{mission}:{since}"),
                kind: "controller_behind",
                subject: format!("mission #{mission}"),
                evidence: json!({
                    "controller": controller, "landed": landed, "since_ms": since,
                    "reading_ms": now, "behind_ms": behind_ms,
                    "threshold_ms": self.settings.controller_behind_ms,
                }),
                text: format!(
                    "mission #{mission}'s controller {} has not had the owner's landing {} \
                     for {}s: its gates wait for the deploy that switches the controller (a \
                     landing not deployed?)",
                    &controller[..controller.len().min(8)],
                    &landed[..landed.len().min(8)],
                    behind_ms / 1000
                ),
                notice: false,
            });
        }
        out
    }

    /// The daily budgets' reading (`cost_budget`): each budgeted mission's
    /// spend of the UTC day so far, on the database's clock.
    fn read_budgets(
        &mut self,
        store: &PostgresStore,
        now: i64,
        out: &mut Vec<Event>,
        lines: &mut Vec<String>,
    ) {
        let day_start = now - now.rem_euclid(DAY_MS);
        match store.cost_budgets(day_start, now.saturating_add(1)) {
            Ok(reading) => self.budgets(now, &reading, out),
            Err(error) => lines.push(format!("alerts: the daily budgets: {error}")),
        }
    }

    /// One reading of the daily budgets (`[{mission, budget_usd,
    /// spent_usd}]`): fire for a mission whose day's spend reached its
    /// budget, once a day; clear an open one of another day, of a mission
    /// with no budget now, or of one back under it.
    fn budgets(&mut self, now: i64, reading: &Value, out: &mut Vec<Event>) {
        let day = utc_day(now);
        let rows = reading.as_array().map_or(&[][..], Vec::as_slice);
        let mut over: HashMap<String, (f64, f64)> = HashMap::new();
        let mut read: HashMap<String, (f64, f64)> = HashMap::new();
        for row in rows {
            let (Some(mission), Some(budget)) =
                (row["mission"].as_i64(), row["budget_usd"].as_f64())
            else {
                continue;
            };
            let spent = row["spent_usd"].as_f64().unwrap_or_default();
            let subject = format!("mission #{mission}");
            read.insert(subject.clone(), (spent, budget));
            if budget > 0.0 && spent >= budget {
                over.insert(subject.clone(), (spent, budget));
                let key = format!("cost_budget:{mission}:{day}");
                if !self.known.contains_key(&key) {
                    out.push(Event::Fire {
                        key,
                        kind: "cost_budget",
                        subject: subject.clone(),
                        evidence: json!({
                            "day": day, "spent_usd": spent, "budget_usd": budget,
                            "reading_ms": now,
                        }),
                        text: format!(
                            "{subject} spent {} today (UTC), over its {} daily budget",
                            crate::cost::dollars(spent),
                            crate::cost::dollars(budget)
                        ),
                        notice: false,
                    });
                }
            }
        }
        let open: Vec<(String, String)> = self
            .known
            .iter()
            .filter(|(_, row)| row.open() && row.kind == "cost_budget")
            .map(|(key, row)| (key.clone(), row.subject.clone()))
            .collect();
        for (key, subject) in open {
            let today = key.ends_with(&format!(":{day}"));
            if today && over.contains_key(&subject) {
                continue;
            }
            let evidence = match read.get(&subject) {
                _ if !today => json!({"day": day, "reading_ms": now, "why": "the day ended"}),
                Some((spent, budget)) => json!({
                    "day": day, "spent_usd": spent, "budget_usd": budget, "reading_ms": now,
                    "why": "under the daily budget",
                }),
                None => json!({"day": day, "reading_ms": now, "why": "no daily budget"}),
            };
            out.push(Event::Clear { key, evidence });
        }
    }

    fn read_memory(&mut self, now: i64, out: &mut Vec<Event>, lines: &mut Vec<String>) {
        if self.settings.memory_low_mb == 0 {
            return;
        }
        let measured = match available_mb() {
            Ok(measured) => measured,
            Err(error) => {
                lines.push(format!("alerts: {error}"));
                return;
            }
        };
        let open = self.open_keys("memory_low", "host");
        let (low, ok) = (self.settings.memory_low_mb, self.settings.memory_ok_mb);
        match crossing(!open.is_empty(), measured, low, ok, true) {
            Crossing::Fire => out.push(Event::Fire {
                key: format!("memory_low:{now}"),
                kind: "memory_low",
                subject: "host".to_string(),
                evidence: json!({
                    "measured_mb": measured, "threshold_mb": low, "top": top_processes(),
                    "reading_ms": now,
                }),
                text: format!("MemAvailable is {measured} MiB, under {low}"),
                notice: false,
            }),
            Crossing::Clear => {
                for key in open {
                    out.push(Event::Clear {
                        key,
                        evidence: json!({
                            "measured_mb": measured, "threshold_mb": ok, "reading_ms": now,
                        }),
                    });
                }
            }
            Crossing::Nothing => {}
        }
    }

    fn read_tmpfs(&mut self, now: i64, out: &mut Vec<Event>, lines: &mut Vec<String>) {
        if self.settings.tmpfs_high_mb == 0 {
            return;
        }
        let path = self.settings.tmpfs_path.clone();
        let measured = match used_mb(&path) {
            Ok(measured) => measured,
            Err(error) => {
                lines.push(format!("alerts: {error}"));
                return;
            }
        };
        let open = self.open_keys("tmpfs_high", "host");
        let (high, ok) = (self.settings.tmpfs_high_mb, self.settings.tmpfs_ok_mb);
        match crossing(!open.is_empty(), measured, high, ok, false) {
            Crossing::Fire => out.push(Event::Fire {
                key: format!("tmpfs_high:{now}"),
                kind: "tmpfs_high",
                subject: "host".to_string(),
                evidence: json!({
                    "path": path.display().to_string(),
                    "measured_mb": measured, "threshold_mb": high, "reading_ms": now,
                }),
                text: format!("{} holds {measured} MiB, over {high}", path.display()),
                notice: false,
            }),
            Crossing::Clear => {
                for key in open {
                    out.push(Event::Clear {
                        key,
                        evidence: json!({
                            "path": path.display().to_string(),
                            "measured_mb": measured, "threshold_mb": ok, "reading_ms": now,
                        }),
                    });
                }
            }
            Crossing::Nothing => {}
        }
    }

    /// Read what the log `index` appended since the last reading.
    fn read_log(&mut self, index: usize, now: i64, out: &mut Vec<Event>, lines: &mut Vec<String>) {
        let subject = self.logs[index].subject;
        let Some(path) = self.logs[index].path.clone() else {
            let message = if subject == "test" {
                "alerts: SPAWN_TEST_DATABASE_LOG names no log: the test database's crash lines are not read"
            } else {
                "alerts: SPAWN_DATABASE_LOG names no log: the live database's crash lines are not read"
            };
            self.say_once(index, message, lines);
            return;
        };
        let read = read_appended(&path, &mut self.logs[index]);
        let (text, truncated) = match read {
            Ok(Some(read)) => read,
            Ok(None) => {
                self.logs[index].said = false;
                return;
            }
            Err(error) => {
                self.say_once(
                    index,
                    &format!("alerts: {}: {error}", path.display()),
                    lines,
                );
                return;
            }
        };
        self.logs[index].said = false;
        self.lines_of_log(index, &path, now, &text, out);
        if truncated {
            lines.push(format!(
                "alerts: {} grew more than the step reads at once; the rest is read next time",
                path.display()
            ));
        }
    }

    /// Say `message` once per log, until it reads again.
    fn say_once(&mut self, index: usize, message: &str, lines: &mut Vec<String>) {
        if !self.logs[index].said {
            self.logs[index].said = true;
            lines.push(message.to_string());
        }
    }

    /// The crash lines of one reading: the occurrences that fire, and the
    /// ready line that clears an open one.
    fn lines_of_log(
        &mut self,
        index: usize,
        path: &Path,
        now: i64,
        text: &str,
        out: &mut Vec<Event>,
    ) {
        let subject = self.logs[index].subject;
        let mut matching: Vec<String> = Vec::new();
        for line in text.lines() {
            if let Some((_stamp, prefix)) = crash_stamp(line, now) {
                self.logs[index].last_match_ms = now;
                self.logs[index].last_line = Some(line.to_string());
                if matching.len() < CRASH_LINES && matching.last() != Some(&line.to_string()) {
                    matching.push(line.to_string());
                }
                let key = format!("postgres_crash:{subject}:{prefix}");
                if !self.known.contains_key(&key) {
                    out.push(Event::Fire {
                        key,
                        kind: "postgres_crash",
                        subject: subject.to_string(),
                        evidence: json!({
                            "log": path.display().to_string(),
                            "lines": matching, "reading_ms": now,
                        }),
                        text: format!("the {subject} database's log: {}", line.trim()),
                        notice: false,
                    });
                }
            } else if line.contains(READY_LINE) {
                for key in self.open_keys("postgres_crash", subject) {
                    out.push(Event::Clear {
                        key,
                        evidence: json!({
                            "line": line.trim(),
                            "log": path.display().to_string(), "reading_ms": now,
                        }),
                    });
                }
            }
        }
    }

    /// Record `events`, in order, each once: the store refuses a second
    /// `alert.fired` for a key, so two loops at once still fire once.
    fn flush(
        &mut self,
        store: &PostgresStore,
        now: i64,
        events: &[Event],
        lines: &mut Vec<String>,
    ) {
        for event in events {
            let key = event.key().to_string();
            let record = match event {
                Event::Fire {
                    kind,
                    subject,
                    evidence,
                    text,
                    notice,
                    ..
                } => {
                    if self.known.contains_key(&key) {
                        continue;
                    }
                    json!({
                        "type": "alert.fired", "key": key, "kind": kind,
                        "subject": subject, "evidence": scrub_value(evidence),
                        "text": text, "notice": notice,
                    })
                }
                Event::Clear { evidence, .. } => {
                    if !self.clears(&key, evidence, now) {
                        continue;
                    }
                    let Some(row) = self.known.get(&key) else {
                        continue;
                    };
                    json!({
                        "type": "alert.cleared", "key": key, "kind": row.kind,
                        "subject": row.subject, "evidence": scrub_value(evidence),
                    })
                }
            };
            match store.alert_record(&record) {
                Ok(id) => {
                    let kind = record["kind"].as_str().unwrap_or_default().to_string();
                    let subject = record["subject"].as_str().unwrap_or_default().to_string();
                    let fired = record["type"] == "alert.fired";
                    if fired {
                        self.known.insert(
                            key.clone(),
                            Row {
                                kind,
                                subject: subject.clone(),
                                state: if record["notice"].as_bool() == Some(true) {
                                    "notice".to_string()
                                } else {
                                    "open".to_string()
                                },
                                evidence: record["evidence"].clone(),
                                fired_event_id: id,
                                fired_at_ms: now,
                            },
                        );
                        lines.push(format!("alerts: fired {key} ({subject})"));
                    } else if let Some(row) = self.known.get_mut(&key) {
                        row.state = "cleared".to_string();
                        lines.push(format!("alerts: cleared {key} ({subject})"));
                    }
                }
                Err(StoreError::Rejected(why)) => {
                    // Another loop recorded it (or the record was refused):
                    // the store's word stands, and the step goes on.
                    let fired = record["type"] == "alert.fired";
                    if fired {
                        self.known.entry(key.clone()).or_insert_with(|| Row {
                            kind: record["kind"].as_str().unwrap_or_default().to_string(),
                            subject: record["subject"].as_str().unwrap_or_default().to_string(),
                            state: if record["notice"].as_bool() == Some(true) {
                                "notice".to_string()
                            } else {
                                "open".to_string()
                            },
                            evidence: record["evidence"].clone(),
                            fired_event_id: 0,
                            fired_at_ms: 0,
                        });
                    }
                    lines.push(format!("alerts: {key}: {why}"));
                }
                Err(error) => {
                    lines.push(format!("alerts: {key}: {error}"));
                }
            }
        }
    }

    /// The earliest bound that has not fired yet, for the next due time.
    fn plan(&mut self, _now: i64) {
        let mut next = i64::MAX;
        for (id, task) in &self.tasks {
            if task.state.as_deref() != Some("assigned") || !task.agent_free {
                continue;
            }
            if let Some(assigned) = task.assigned_event
                && !self
                    .known
                    .contains_key(&format!("task_undelivered:{id}:{assigned}"))
            {
                next = next.min(
                    task.assigned_at
                        .saturating_add(self.settings.undelivered_ms),
                );
            }
        }
        for pending in self.admitted.values() {
            if !self
                .known
                .contains_key(&format!("admission_wait:{}", pending.event_id))
            {
                next = next.min(pending.at_ms.saturating_add(self.settings.admission_ms));
            }
        }
        for pending in self.judge_down.values() {
            if !self
                .known
                .contains_key(&format!("judge_down:{}", pending.event_id))
            {
                next = next.min(pending.at_ms.saturating_add(self.settings.judge_down_ms));
            }
        }
        for (subject, max_age) in [
            ("dump", self.settings.dump_max_age_ms),
            ("bundle", self.settings.bundle_max_age_ms),
        ] {
            if let Some(backup) = self.backups.get(subject)
                && !self
                    .known
                    .contains_key(&format!("backup_stale:{subject}:{}", backup.event_id))
            {
                next = next.min(backup.at_ms.saturating_add(max_age));
            }
        }
        for log in &self.logs {
            if !self.open_keys("postgres_crash", log.subject).is_empty() && log.last_match_ms > 0 {
                next = next.min(log.last_match_ms.saturating_add(self.settings.quiet_ms));
            }
        }
        if let Some(due) = self.messages_due {
            next = next.min(due);
        }
        self.next_bound_ms = (next != i64::MAX).then_some(next);
    }

    /// The keys of the open alerts of one kind and subject.
    fn open_keys(&self, kind: &str, subject: &str) -> Vec<String> {
        self.known
            .iter()
            .filter(|(_, row)| row.open() && row.kind == kind && row.subject == subject)
            .map(|(key, _)| key.clone())
            .collect()
    }

    fn is_open(&self, key: &str) -> bool {
        self.known.get(key).is_some_and(Row::open)
    }

    /// Whether a clear of `key` with `evidence`, decided at `now`, is
    /// recorded: the alert is open in the store, and the evidence comes
    /// after what it fired on ([`after`]). One that does not clears
    /// nothing, and is no error.
    fn clears(&self, key: &str, evidence: &Value, now: i64) -> bool {
        self.known
            .get(key)
            .is_some_and(|row| row.open() && row.fired_event_id != 0 && after(row, evidence, now))
    }
}

/// Whether a clear's `evidence` comes after what the alert `row` fired on,
/// read from the alert's own evidence (what the store holds, never the
/// step's memory, which a restart empties): a record-borne clear names a
/// control record or event (`event_id`) later in the log than the one the
/// alert fired on; any other (a reading, `reading_ms`, or else the time it
/// is decided, `now`) is taken at or after the alert's reading, or its
/// firing.
fn after(row: &Row, evidence: &Value, now: i64) -> bool {
    if let (Some(by), Some(on)) = (
        evidence["event_id"].as_i64(),
        row.evidence["event_id"].as_i64(),
    ) {
        return by > on;
    }
    let read = evidence["reading_ms"].as_i64().unwrap_or(now);
    let fired = row.evidence["reading_ms"]
        .as_i64()
        .unwrap_or(row.fired_at_ms);
    read >= fired
}

impl LogWatch {
    fn new(subject: &'static str, path: Option<PathBuf>) -> Self {
        Self {
            subject,
            path,
            offset: 0,
            seated: false,
            carry: String::new(),
            said: false,
            last_match_ms: 0,
            last_line: None,
        }
    }
}

/// What one reading of a condition does: the crossing in, the crossing out
/// past the margin, or nothing (a reading between the two thresholds does
/// neither, so a host hovering at the threshold does not flap).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Crossing {
    Fire,
    Clear,
    Nothing,
}

/// Where a reading leaves a condition: `turn_on` is the threshold it fires
/// over (or under, when `under`), `turn_off` the one it clears at, on the
/// other side of the margin.
/// The strings of a record's list, in order.
fn names(list: &Value) -> Vec<String> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn crossing(open: bool, measured: u64, turn_on: u64, turn_off: u64, under: bool) -> Crossing {
    let bad = if under {
        measured < turn_on
    } else {
        measured > turn_on
    };
    let good = if under {
        measured >= turn_off
    } else {
        measured < turn_off
    };
    match (open, bad, good) {
        (false, true, _) => Crossing::Fire,
        (true, _, true) => Crossing::Clear,
        _ => Crossing::Nothing,
    }
}

/// The five fields a mission's notices compare.
fn snapshot(mission: &Mission) -> Value {
    json!({
        "state": mission.state,
        "halted": mission.halted,
        "gate_rejections": mission.rejections,
        "gate_aborts": mission.aborts,
        "gate_room_escalated": mission.escalated,
    })
}

/// What changed between two snapshots, in one phrase for a person.
fn changed(before: &Value, after: &Value) -> String {
    let mut parts = Vec::new();
    for field in [
        "state",
        "halted",
        "gate_rejections",
        "gate_aborts",
        "gate_room_escalated",
    ] {
        if before[field] != after[field] {
            parts.push(format!(
                "{field} {} -> {}",
                text_of(&before[field]),
                text_of(&after[field])
            ));
        }
    }
    parts.join(", ")
}

fn text_of(value: &Value) -> String {
    match value {
        Value::Null => "none".to_string(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// The mission's fold of one record, as far as the five watched fields go.
fn apply_mission(mission: &mut Mission, record: &Value) {
    match record["type"].as_str().unwrap_or_default() {
        "mission.closed" => {
            if let Some(state) = record["state"].as_str() {
                mission.state = state.to_string();
            }
        }
        "mission.halted" => {
            mission.halted = Some(record["reason"].as_str().unwrap_or("halted").to_string());
        }
        "mission.resumed" => {
            mission.halted = None;
            mission.rejections = 0;
            mission.run_objective = None;
            mission.aborts = 0;
            mission.escalated = false;
        }
        "mission.configured" => {
            mission.accept = record["accept"].as_str().map(str::to_string);
        }
        "task.assigned" if mission.state == "open" => {
            mission.state = "active".to_string();
        }
        "task.aborted" => mission.aborts += 1,
        _ => {}
    }
}

/// The fold's counter changes of a gating of a task of `objective`.
fn apply_gating(mission: &mut Mission, record: &Value, objective: Option<i64>) {
    match record["verdict"].as_str().unwrap_or_default() {
        "rejected" => {
            // The run's objective: the first rejection's task's, kept while
            // every later one is a task of the same objective.
            mission.run_objective = if mission.rejections == 0 {
                objective
            } else {
                mission.run_objective.filter(|run| Some(*run) == objective)
            };
            mission.rejections += 1;
        }
        "accepted" => {
            mission.rejections = 0;
            mission.run_objective = None;
        }
        _ => {}
    }
    match record["verdict"].as_str().unwrap_or_default() {
        "aborted" => mission.aborts += 1,
        "stale" | "no_room" | "host_fault" => {}
        _ => mission.aborts = 0,
    }
    match record["verdict"].as_str().unwrap_or_default() {
        "no_room" => {
            mission.escalated |= record["escalate"].as_bool().unwrap_or(false);
        }
        "stale" => {}
        _ => mission.escalated = false,
    }
}

/// The fold's change of a cancel of a task of `objective`: one of the
/// objective every rejection of the mission's run was ends that run.
fn apply_cancel(mission: &mut Mission, objective: Option<i64>) {
    if objective.is_some() && mission.run_objective == objective {
        mission.rejections = 0;
        mission.run_objective = None;
    }
}

/// The task state a record leaves, or `None` when it leaves it as it was.
fn task_state_after(record: &Value, accept_gate: bool, revisions: i64) -> Option<String> {
    if record["revision"]
        .as_i64()
        .is_some_and(|revision| revision != revisions)
    {
        return None;
    }
    let state = match record["type"].as_str()? {
        "task.assigned" => "assigned",
        "task.delivered" => "delivered",
        "task.reported" => "reported",
        "task.gated" => match record["verdict"].as_str()? {
            "accepted" => "landing",
            "rejected" | "conflict" | "incompatible" => "revise",
            "stale" => {
                if accept_gate {
                    "reported"
                } else {
                    "accepted"
                }
            }
            _ => return None,
        },
        "task.reviewed" => {
            if record["accepted"].as_bool()? {
                "accepted"
            } else {
                "revise"
            }
        }
        "task.landed" => "landed",
        "task.aborted" | "task.over_budget" | "task.cancelled" => "cancelled",
        _ => return None,
    };
    Some(state.to_string())
}

/// Whether the record moves the task's revision count (a rejection, a
/// conflict, an incompatible head or a send-back).
fn bumps_revision(record: &Value) -> bool {
    match record["type"].as_str().unwrap_or_default() {
        "task.gated" => matches!(
            record["verdict"].as_str(),
            Some("rejected" | "conflict" | "incompatible")
        ),
        "task.reviewed" => record["accepted"].as_bool() == Some(false),
        _ => false,
    }
}

/// 033's record: a host fault that says the base disagrees with itself, with
/// `alert` true.
fn base_disagrees(record: &Value) -> Option<()> {
    (record["type"] == "task.gated"
        && record["verdict"] == "host_fault"
        && record["evidence"] == "base_disagrees"
        && record["alert"].as_bool() == Some(true))
    .then_some(())
}

/// MemAvailable of `/proc/meminfo`, in MiB.
fn available_mb() -> Result<u64, String> {
    let text = std::fs::read_to_string("/proc/meminfo")
        .map_err(|error| format!("/proc/meminfo: {error}"))?;
    let line = text
        .lines()
        .find(|line| line.starts_with("MemAvailable:"))
        .ok_or_else(|| "/proc/meminfo: no MemAvailable".to_string())?;
    let kb: u64 = line
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| format!("/proc/meminfo: cannot read {line:?}"))?;
    Ok(kb / 1024)
}

/// The used MiB of the filesystem that holds `path`, as `df -Pk` reports it.
fn used_mb(path: &Path) -> Result<u64, String> {
    let output = std::process::Command::new("df")
        .arg("-Pk")
        .arg("--")
        .arg(path)
        .output()
        .map_err(|error| format!("df {}: {error}", path.display()))?;
    if !output.status.success() {
        return Err(format!(
            "df {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text
        .lines()
        .nth(1)
        .ok_or_else(|| format!("df {}: no filesystem in its output", path.display()))?;
    let kb: u64 = line
        .split_whitespace()
        .nth(2)
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| format!("df {}: cannot read {line:?}", path.display()))?;
    Ok(kb / 1024)
}

/// The three largest processes by RSS: `pid`, `rss_mb`, and the command name
/// only (never its arguments).
fn top_processes() -> Value {
    let mut processes: Vec<(u64, u64, String)> = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Value::Array(Vec::new());
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|name| name.parse::<u64>().ok()) else {
            continue;
        };
        let Ok(status) = std::fs::read_to_string(entry.path().join("status")) else {
            continue;
        };
        let mut rss_kb = 0;
        let mut command = String::new();
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                rss_kb = rest
                    .split_whitespace()
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0);
            } else if let Some(rest) = line.strip_prefix("Name:") {
                command = rest.trim().to_string();
            }
        }
        processes.push((rss_kb, pid, command));
    }
    processes.sort_unstable_by(|left, right| right.cmp(left));
    Value::Array(
        processes
            .into_iter()
            .take(3)
            .map(|(rss_kb, pid, name)| json!({"pid": pid, "rss_mb": rss_kb / 1024, "name": name}))
            .collect(),
    )
}

/// The bytes appended to `log`'s file since its last reading, as text, and
/// whether more than one reading takes was left. On start, the last
/// [`LOG_TAIL_BYTES`] only.
fn read_appended(path: &Path, log: &mut LogWatch) -> Result<Option<(String, bool)>, String> {
    let mut file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let length = file.metadata().map_err(|error| error.to_string())?.len();
    if !log.seated {
        log.offset = length.saturating_sub(LOG_TAIL_BYTES);
        log.seated = true;
    }
    if length < log.offset {
        // Rotated or truncated: read the new file from its start.
        log.offset = 0;
        log.carry.clear();
    }
    if length == log.offset {
        return Ok(None);
    }
    let start = length.saturating_sub(LOG_READ_LIMIT).max(log.offset);
    if start > log.offset {
        // Too far behind: skip ahead, and say nothing about what was skipped.
        log.offset = start;
        log.carry.clear();
    }
    file.seek(SeekFrom::Start(log.offset))
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    file.take(LOG_READ_LIMIT)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    log.offset = log.offset.saturating_add(bytes.len() as u64);
    let truncated = log.offset < length;
    log.carry.push_str(&String::from_utf8_lossy(&bytes));
    // A line the reading cut waits for its end in the next one.
    let (complete, carry) = match log.carry.rfind('\n') {
        Some(at) => (
            log.carry[..=at].to_string(),
            log.carry[at + 1..].to_string(),
        ),
        None => (String::new(), std::mem::take(&mut log.carry)),
    };
    log.carry = carry;
    Ok(Some((complete, truncated)))
}

/// The stamp and `[pid]` of a crash line stamped within the day, as written,
/// with the stamp's spaces folded out so the key is one word:
/// `2026-11-08T18:25:00.123UTC[1]`. `None` for any other line.
fn crash_stamp(line: &str, now: i64) -> Option<(i64, String)> {
    if !CRASH_PATTERNS.iter().any(|pattern| line.contains(pattern)) {
        return None;
    }
    let bytes = line.as_bytes();
    if bytes.len() < 26 {
        return None;
    }
    let date = line.get(0..10)?;
    let time = line.get(11..19)?;
    if date.as_bytes()[4] != b'-'
        || date.as_bytes()[7] != b'-'
        || time.as_bytes()[2] != b':'
        || time.as_bytes()[5] != b':'
        || !date
            .chars()
            .chain(time.chars())
            .all(|c| c.is_ascii_digit() || c == '-' || c == ':')
    {
        return None;
    }
    let year: i64 = date.get(0..4)?.parse().ok()?;
    let month: u32 = date.get(5..7)?.parse().ok()?;
    let day: u32 = date.get(8..10)?.parse().ok()?;
    let hour: i64 = time.get(0..2)?.parse().ok()?;
    let minute: i64 = time.get(3..5)?.parse().ok()?;
    let second: i64 = time.get(6..8)?.parse().ok()?;
    let mut rest = line.get(19..)?;
    let mut millis: i64 = 0;
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits: String = fraction.chars().take_while(char::is_ascii_digit).collect();
        if digits.len() != 3 {
            return None;
        }
        millis = digits.parse().ok()?;
        rest = fraction.get(3..)?;
    }
    let rest = rest.strip_prefix(' ')?;
    let (offset_min, rest) = zone(rest)?;
    let rest = rest.strip_prefix(' ')?;
    if !rest.starts_with('[') {
        return None;
    }
    let end = rest.find(']')?;
    if !rest.get(1..end)?.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let written = line.get(..line.len() - rest.len() + end + 1)?;
    // The key is one word: the stamp's spaces are folded out, its text (the
    // date, the time, the zone, the pid) as the line wrote it.
    let prefix = format!(
        "{}T{}",
        written.get(..10)?,
        written.get(11..)?.replace(' ', "")
    );
    let stamp = days_from_civil(year, month, day)
        .checked_mul(86_400)?
        .checked_add(hour * 3600 + minute * 60 + second)?;
    let stamp = stamp * 1000 + millis - i64::from(offset_min) * 60_000;
    (stamp >= now - CRASH_MAX_AGE_MS).then_some((stamp, prefix))
}

/// A postgres log's time zone (`UTC`, `+08`, `-05:30`, `+0800`), as minutes
/// east of UTC, and what follows.
fn zone(text: &str) -> Option<(i32, &str)> {
    if let Some(rest) = text.strip_prefix("UTC") {
        return Some((0, rest));
    }
    let sign = match text.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let digits: String = text
        .get(1..)?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let (hours, minutes, rest) = match digits.len() {
        2 => (digits.parse::<i32>().ok()?, 0, text.get(3..)?),
        4 => (
            digits.get(..2)?.parse::<i32>().ok()?,
            digits.get(2..4)?.parse::<i32>().ok()?,
            text.get(5..)?,
        ),
        _ => return None,
    };
    let (minutes, rest) = if let Some(rest) = rest.strip_prefix(':') {
        let more: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if more.len() != 2 {
            return None;
        }
        (more.parse::<i32>().ok()?, rest.get(2..)?)
    } else {
        (minutes, rest)
    };
    (hours <= 23 && minutes < 60).then_some((sign * (hours * 60 + minutes), rest))
}

/// Days since 1970-01-01 of a civil date (proleptic Gregorian).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = (year - era * 400).cast_unsigned();
    let mp = u64::from(if month > 2 { month - 3 } else { month + 9 });
    let doy = (153 * mp + 2) / 5 + u64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe.cast_signed() - 719_468
}

/// The civil date and time of an epoch in ms, in UTC.
fn civil(ms: i64) -> (i64, u32, u32, u32, u32, u32) {
    let seconds = ms.div_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097).cast_unsigned();
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe.cast_signed() + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (
        year,
        u32::try_from(month).unwrap_or(1),
        u32::try_from(day).unwrap_or(1),
        u32::try_from(rest / 3600).unwrap_or(0),
        u32::try_from(rest % 3600 / 60).unwrap_or(0),
        u32::try_from(rest % 60).unwrap_or(0),
    )
}

/// An epoch in ms as `2026-11-08 18:25:00` (UTC).
pub(crate) fn time_text(ms: i64) -> String {
    let (year, month, day, hour, minute, second) = civil(ms);
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}")
}

/// A password in a URL is a secret: evidence never holds one. `scheme://
/// user:password@host` becomes `scheme://user:***@host`.
pub(crate) fn scrub(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find("://") {
        let before = &rest[..index];
        let start = before
            .rfind(|c: char| c.is_whitespace() || c == '"' || c == '\'')
            .map_or(0, |at| at + 1);
        let after = &rest[index..];
        let end = after
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\'')
            .unwrap_or(after.len());
        out.push_str(&rest[..start]);
        out.push_str(&redact(&rest[start..index + end]));
        rest = &rest[index + end..];
    }
    out.push_str(rest);
    out
}

fn redact(token: &str) -> String {
    let Some((scheme, rest)) = token.split_once("://") else {
        return token.to_string();
    };
    let Some(at) = rest.rfind('@') else {
        return token.to_string();
    };
    let Some(colon) = rest[..at].find(':') else {
        return token.to_string();
    };
    format!("{scheme}://{}:***{}", &rest[..colon], &rest[at..])
}

/// [`scrub`] applied to every string an evidence object holds.
fn scrub_value(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(scrub(text)),
        Value::Array(items) => Value::Array(items.iter().map(scrub_value).collect()),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, value)| (key.clone(), scrub_value(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// A span (`90s`, `30m`, `2h`, `1d`, or a number of ms) in ms.
fn parse_span(text: &str) -> Option<i64> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let value: i64 = number.parse().ok()?;
    let factor = match unit {
        "" | "ms" => 1,
        "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    Some(value.saturating_mul(factor))
}

/// The window `spawn alerts --once` and `/api/alerts` show, in ms: `since`
/// is a span (`90s`, `30m`, `2h`, `1d`, or a number of ms), else the
/// configured window.
pub(crate) fn window_ms(since: Option<&str>) -> Result<i64, String> {
    match since {
        None => Ok(Settings::from_env().0.window_ms),
        Some(text) if text.trim().is_empty() => Ok(Settings::from_env().0.window_ms),
        Some(text) => parse_span(text)
            .ok_or_else(|| format!("--since {text:?} is not a span (90s, 30m, 2h, 1d) or ms")),
    }
}

/// The alert window as `spawn alerts --once --json` and `/api/alerts`
/// answer it: the one array both show.
pub(crate) fn window(store: &PostgresStore, since: Option<&str>) -> Result<Value, String> {
    let span = window_ms(since)?;
    let now = store.now_ms().map_err(|error| error.to_string())?;
    store
        .alerts(now.saturating_sub(span))
        .map_err(|error| error.to_string())
}

const USAGE: &str = "usage: spawn alerts --once [--json] [--since <90s|30m|2h|1d|ms>]\n       spawn alerts --follow [--json] [--since <event id>]";

/// `spawn alerts`: the alerts now, or each transition as it is recorded.
pub(crate) fn command(arguments: &[String]) -> Result<i32, String> {
    let mut once = false;
    let mut follow = false;
    let mut json = false;
    let mut since: Option<String> = None;
    let mut words = arguments.iter();
    while let Some(word) = words.next() {
        match word.as_str() {
            "--once" => once = true,
            "--follow" => follow = true,
            "--json" => json = true,
            "--since" => {
                since = Some(
                    words
                        .next()
                        .ok_or_else(|| format!("--since needs a value\n{USAGE}"))?
                        .clone(),
                );
            }
            other => return Err(format!("unexpected argument {other}\n{USAGE}")),
        }
    }
    if once == follow {
        return Err(USAGE.to_string());
    }
    let store = open_store()?;
    if once {
        let alerts = window(&store, since.as_deref())?;
        let alerts = alerts.as_array().cloned().unwrap_or_default();
        if json {
            println!("{}", Value::Array(alerts.clone()));
        } else {
            for alert in &alerts {
                println!("{}", line(alert));
            }
        }
        return Ok(0);
    }
    let after = if let Some(text) = since.as_deref() {
        text.trim()
            .parse::<i64>()
            .map_err(|_| format!("--since {text:?} is not an event id\n{USAGE}"))?
    } else {
        let rows = store.alert_rows().map_err(|error| error.to_string())?;
        rows["cursor"].as_i64().unwrap_or_default()
    };
    follow_stream(&store, after, json)
}

/// One alert as a line for a person: its state, kind, subject, time and
/// text; an open `agent_stuck` also says when the session was last scored
/// (`scored_at_ms`, read with the window): a judge that has been down
/// since leaves the alert on an old score.
fn line(alert: &Value) -> String {
    let state = alert["state"].as_str().unwrap_or("open");
    let at = alert
        .get("cleared_at_ms")
        .and_then(Value::as_i64)
        .or_else(|| alert["fired_at_ms"].as_i64())
        .unwrap_or_default();
    let scored = alert["scored_at_ms"]
        .as_i64()
        .map(|scored| format!(" (last scored {})", time_text(scored)))
        .unwrap_or_default();
    format!(
        "{state} {} {} {} {}{scored}",
        alert["kind"].as_str().unwrap_or("alert"),
        alert["subject"].as_str().unwrap_or(""),
        time_text(at),
        alert["text"].as_str().unwrap_or("")
    )
}

/// Print each transition after `after` as it is recorded, until stopped.
fn follow_stream(store: &PostgresStore, after: i64, json: bool) -> Result<i32, String> {
    let mut cursor = after;
    let mut out = std::io::stdout();
    loop {
        let transitions = match store.alert_transitions(cursor, BATCH) {
            Ok(transitions) => transitions,
            Err(error) => {
                // A read that failed is tried again: a stream that ends on
                // one glitch tells nobody what happened after it.
                eprintln!("spawn alerts: {error}");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        for transition in transitions.as_array().into_iter().flatten() {
            let event_id = transition["event_id"].as_i64().unwrap_or_default();
            cursor = cursor.max(event_id);
            if json {
                let _ = writeln!(out, "{transition}");
            } else {
                let _ = writeln!(
                    out,
                    "{} {} {} {} {}",
                    transition["transition"].as_str().unwrap_or("fired"),
                    transition["kind"].as_str().unwrap_or("alert"),
                    transition["subject"].as_str().unwrap_or(""),
                    time_text(transition["at_ms"].as_i64().unwrap_or_default()),
                    transition["key"].as_str().unwrap_or("")
                );
            }
            let _ = out.flush();
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The (mission, source) pairs of the store's `alert_mission_sources` whose
/// source is a path on this host: absolute (a URL or `host:path` never is,
/// and is another host's to check).
fn host_sources(listed: &Value) -> BTreeSet<(i64, String)> {
    let mut out = BTreeSet::new();
    for entry in listed.as_array().into_iter().flatten() {
        let Some(mission) = entry["mission"].as_i64() else {
            continue;
        };
        for source in entry["sources"].as_array().into_iter().flatten() {
            if let Some(source) = source.as_str()
                && Path::new(source).is_absolute()
            {
                out.insert((mission, source.to_string()));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        Crossing, Event, LIKELY, Monitor, Row, Settings, civil, crash_stamp, crossing,
        days_from_civil, host_sources, line, parse_span, scrub, scrub_value, time_text, window_ms,
    };
    use serde_json::{Value, json};
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::{Path, PathBuf};

    fn settings() -> Settings {
        Settings {
            check_ms: 30_000,
            undelivered_ms: 300_000,
            admission_ms: 600_000,
            judge_down_ms: 900_000,
            message_taken_ms: 120_000,
            dump_max_age_ms: 7_200_000,
            bundle_max_age_ms: 93_600_000,
            memory_low_mb: 4096,
            memory_ok_mb: 8192,
            tmpfs_path: PathBuf::from("/tmp"),
            tmpfs_high_mb: 8000,
            tmpfs_ok_mb: 6000,
            quiet_ms: 900_000,
            integrator_down_ms: 300_000,
            controller_behind_ms: 1_800_000,
            window_ms: 86_400_000,
            test_log: None,
            live_log: None,
        }
    }

    fn monitor() -> Monitor {
        Monitor::with(settings())
    }

    /// What `flush` would record: the events whose key is not already there
    /// (and the clearings of open alerts only).
    fn decided(monitor: &Monitor, events: Vec<Event>) -> Vec<Event> {
        events
            .into_iter()
            .filter(|event| match event {
                Event::Fire { .. } => !monitor.known.contains_key(event.key()),
                Event::Clear { key, evidence } => monitor.clears(key, evidence, i64::MAX),
            })
            .collect()
    }

    /// The store's side of a flush: the keys it now holds.
    fn commit(monitor: &mut Monitor, events: &[Event]) {
        for event in events {
            match event {
                Event::Fire {
                    key,
                    kind,
                    subject,
                    evidence,
                    ..
                } => {
                    monitor.known.insert(
                        key.clone(),
                        Row {
                            kind: (*kind).to_string(),
                            subject: subject.clone(),
                            state: "open".to_string(),
                            evidence: evidence.clone(),
                            fired_event_id: 1,
                            fired_at_ms: 0,
                        },
                    );
                }
                Event::Clear { key, .. } => {
                    if let Some(row) = monitor.known.get_mut(key) {
                        row.state = "cleared".to_string();
                    }
                }
            }
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    fn feed(monitor: &mut Monitor, event_id: i64, at_ms: i64, record: Value) -> Vec<Event> {
        let mut out = Vec::new();
        monitor.feed(event_id, at_ms, &record, &mut out);
        out
    }

    fn fires(events: &[Event]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Fire { key, .. } => Some(key.as_str()),
                Event::Clear { .. } => None,
            })
            .collect()
    }

    #[test]
    fn a_task_records_each_state_change_once_with_from_and_to() {
        let mut monitor = monitor();
        let assigned = feed(
            &mut monitor,
            10,
            1000,
            json!({"type": "task.assigned", "id": 7, "mission_id": 4,
                   "agent_id": "agent_x", "objective": 26, "revision": 0}),
        );
        assert_eq!(fires(&assigned), ["task_state:7:10"]);
        let Event::Fire {
            kind,
            subject,
            evidence,
            notice,
            ..
        } = &assigned[0]
        else {
            panic!("a fire");
        };
        assert_eq!(*kind, "task_state");
        assert_eq!(subject, "task #7");
        assert!(notice);
        assert_eq!(evidence["from"], json!(null));
        assert_eq!(evidence["to"], "assigned");
        assert_eq!(evidence["record"], "task.assigned");
        assert_eq!(evidence["event_id"], 10);
        assert_eq!(evidence["mission"], 4);
        commit(&mut monitor, &assigned);

        let delivered = feed(
            &mut monitor,
            11,
            2000,
            json!({"type": "task.delivered", "id": 7, "command_id": 3}),
        );
        assert_eq!(fires(&delivered), ["task_state:7:11"]);
        let Event::Fire { evidence, .. } = &delivered[0] else {
            panic!("a fire");
        };
        assert_eq!(evidence["from"], "assigned");
        assert_eq!(evidence["to"], "delivered");
        commit(&mut monitor, &delivered);

        let again = feed(
            &mut monitor,
            11,
            2000,
            json!({"type": "task.delivered", "id": 7, "command_id": 3}),
        );
        assert!(
            decided(&monitor, again).is_empty(),
            "a replay records nothing"
        );
    }

    #[test]
    fn a_gating_that_leaves_the_task_is_no_notice_and_moves_the_missions_counters() {
        let mut monitor = monitor();
        feed(
            &mut monitor,
            1,
            0,
            json!({"type": "mission.added", "id": 4, "text": "m"}),
        );
        feed(
            &mut monitor,
            2,
            0,
            json!({"type": "task.assigned", "id": 7, "mission_id": 4,
                   "agent_id": "agent_x", "revision": 0}),
        );
        for (event, aborts) in [(3, 1), (4, 2), (5, 3)] {
            let events = feed(
                &mut monitor,
                event,
                0,
                json!({"type": "task.gated", "id": 7, "verdict": "aborted", "revision": 0}),
            );
            assert!(
                !fires(&events)
                    .iter()
                    .any(|key| key.starts_with("task_state:")),
                "an aborted gating leaves the task as it was"
            );
            let notice = events
                .iter()
                .find(|event| {
                    matches!(
                        event,
                        Event::Fire {
                            kind: "mission_state",
                            ..
                        }
                    )
                })
                .expect("the mission's abort count moved");
            let Event::Fire { evidence, .. } = notice else {
                panic!("a fire");
            };
            assert_eq!(evidence["record"], "task.gated");
            assert_eq!(evidence["after"]["gate_aborts"], aborts);
            assert_eq!(evidence["before"]["gate_aborts"], aborts - 1);
        }
    }

    /// The `gate_rejections` change a record's notices say: "<before> ->
    /// <after> (by <record>)", or `None` when none says it.
    fn rejections_change(events: &[Event]) -> Option<String> {
        events.iter().find_map(|event| match event {
            Event::Fire {
                kind: "mission_state",
                evidence,
                ..
            } if evidence["before"]["gate_rejections"] != evidence["after"]["gate_rejections"] => {
                Some(format!(
                    "{} -> {} (by {})",
                    evidence["before"]["gate_rejections"],
                    evidence["after"]["gate_rejections"],
                    evidence["record"].as_str().unwrap_or_default()
                ))
            }
            _ => None,
        })
    }

    /// Objective 053: a cancel of a task of the objective every rejection
    /// of the run was ends the run, with its notice; a cancel outside the
    /// run (another objective, a run of two, a task with none) changes
    /// nothing, as the store's fold.
    #[test]
    fn a_cancel_that_ends_the_run_of_rejections_is_a_notice_as_the_fold() {
        let mut monitor = monitor();
        feed(
            &mut monitor,
            1,
            0,
            json!({"type": "mission.added", "id": 4, "text": "m"}),
        );
        for (task, objective) in [
            (1, json!(1)),
            (2, json!(2)),
            (3, json!(3)),
            (4, Value::Null),
        ] {
            feed(
                &mut monitor,
                1 + task,
                0,
                json!({"type": "task.assigned", "id": task, "mission_id": 4,
                       "agent_id": format!("agent_{task}"), "objective": objective}),
            );
        }
        let mut event = 10;
        let mut gate = |monitor: &mut Monitor, task: i64, verdict: &str| {
            event += 1;
            feed(
                monitor,
                event,
                0,
                json!({"type": "task.gated", "id": task, "verdict": verdict}),
            )
        };
        let cancel = |monitor: &mut Monitor, event: i64, task: i64| {
            feed(
                monitor,
                event,
                0,
                json!({"type": "task.cancelled", "id": task}),
            )
        };
        for want in ["0 -> 1", "1 -> 2", "2 -> 3"] {
            let events = gate(&mut monitor, 1, "rejected");
            assert_eq!(
                rejections_change(&events),
                Some(format!("{want} (by task.gated)"))
            );
        }
        // A cancel of another objective's task: nothing.
        assert_eq!(rejections_change(&cancel(&mut monitor, 20, 2)), None);
        // The yield of objective 1: the run ends.
        let yielded = cancel(&mut monitor, 21, 1);
        assert_eq!(
            rejections_change(&yielded),
            Some("3 -> 0 (by task.cancelled)".into())
        );
        let Some(Event::Fire { text, .. }) = yielded.first() else {
            panic!("a notice: {yielded:?}");
        };
        assert_eq!(
            text,
            "mission #4: gate_rejections 3 -> 0 (by task.cancelled)"
        );
        // Cancelled again: the run is gone, nothing more.
        assert_eq!(rejections_change(&cancel(&mut monitor, 22, 1)), None);

        // A run of two objectives: a cancel of either ends nothing.
        gate(&mut monitor, 2, "rejected");
        gate(&mut monitor, 3, "rejected");
        assert_eq!(rejections_change(&cancel(&mut monitor, 23, 3)), None);
        assert_eq!(rejections_change(&cancel(&mut monitor, 24, 2)), None);
        let events = gate(&mut monitor, 3, "rejected");
        assert_eq!(
            rejections_change(&events),
            Some("2 -> 3 (by task.gated)".into())
        );
        // An accept ends it; a run of a task with no objective is no cancel's.
        gate(&mut monitor, 3, "accepted");
        gate(&mut monitor, 4, "rejected");
        assert_eq!(rejections_change(&cancel(&mut monitor, 25, 4)), None);
        // A resume ends the run and its objective: a cancel after says nothing.
        gate(&mut monitor, 4, "accepted");
        gate(&mut monitor, 3, "rejected");
        feed(
            &mut monitor,
            26,
            0,
            json!({"type": "mission.resumed", "id": 4}),
        );
        assert_eq!(rejections_change(&cancel(&mut monitor, 27, 3)), None);
    }

    #[test]
    fn a_halt_is_a_notice_and_an_alert_and_a_resume_clears_it() {
        let mut monitor = monitor();
        feed(
            &mut monitor,
            1,
            0,
            json!({"type": "mission.added", "id": 4, "text": "m"}),
        );
        let halted = feed(
            &mut monitor,
            2,
            0,
            json!({"type": "mission.halted", "id": 4, "reason": "three rejections"}),
        );
        assert_eq!(fires(&halted), ["mission_state:4:2", "mission_halted:4:2"]);
        let Event::Fire {
            evidence, notice, ..
        } = &halted[1]
        else {
            panic!("a fire");
        };
        assert_eq!(evidence["reason"], "three rejections");
        assert!(!notice);
        commit(&mut monitor, &halted);

        let resumed = feed(
            &mut monitor,
            3,
            0,
            json!({"type": "mission.resumed", "id": 4}),
        );
        let cleared = decided(&monitor, resumed);
        assert_eq!(cleared.len(), 2, "{cleared:?}");
        assert!(matches!(cleared[1], Event::Clear { ref key, .. } if key == "mission_halted:4:2"));
        let Event::Clear { evidence, .. } = &cleared[1] else {
            panic!("a clear");
        };
        assert_eq!(evidence["event_id"], 3);
    }

    /// What a restart reads again: an open halt of record 20, then the
    /// records of the history in order. A resume older than the halt, a
    /// good drill older than the failed one, an `up` older than its `down`
    /// clear nothing; a later one clears.
    #[test]
    fn a_record_older_than_an_alerts_own_clears_nothing() {
        let mut monitor = monitor();
        let open = |kind: &str, subject: &str, evidence: Value| super::Row {
            kind: kind.to_string(),
            subject: subject.to_string(),
            state: "open".to_string(),
            evidence,
            fired_event_id: 100,
            fired_at_ms: 0,
        };
        monitor.known.insert(
            "mission_halted:4:20".into(),
            open("mission_halted", "mission #4", json!({"event_id": 20})),
        );
        monitor.known.insert(
            "drill_failed:30".into(),
            open("drill_failed", "drill", json!({"event_id": 30})),
        );
        monitor.known.insert(
            "judge_down:40".into(),
            open("judge_down", "jev", json!({"event_id": 40})),
        );
        feed(
            &mut monitor,
            1,
            0,
            json!({"type": "mission.added", "id": 4, "text": "m"}),
        );
        for (event_id, record) in [
            (15, json!({"type": "mission.resumed", "id": 4})),
            (
                25,
                json!({"type": "backup.verified", "ok": true, "key": "old"}),
            ),
            (
                30,
                json!({"type": "backup.verified", "ok": false, "key": "same"}),
            ),
            (
                35,
                json!({"type": "host.judge", "judge": "jev", "state": "up"}),
            ),
        ] {
            let events = feed(&mut monitor, event_id, 0, record);
            let cleared: Vec<Event> = decided(&monitor, events)
                .into_iter()
                .filter(|event| matches!(event, Event::Clear { .. }))
                .collect();
            assert!(cleared.is_empty(), "record {event_id} cleared {cleared:?}");
        }
        for (event_id, record, key) in [
            (
                50,
                json!({"type": "mission.resumed", "id": 4}),
                "mission_halted:4:20",
            ),
            (
                51,
                json!({"type": "backup.verified", "ok": true, "key": "new"}),
                "drill_failed:30",
            ),
            (
                52,
                json!({"type": "host.judge", "judge": "jev", "state": "up"}),
                "judge_down:40",
            ),
        ] {
            let events = feed(&mut monitor, event_id, 0, record);
            let cleared = decided(&monitor, events);
            assert!(
                cleared
                    .iter()
                    .any(|event| matches!(event, Event::Clear { key: at, evidence }
                        if at == key && evidence["event_id"] == event_id)),
                "record {event_id} clears {key}: {cleared:?}"
            );
        }
    }

    /// A reading-borne clear: a reading at or after the one the alert fired
    /// on, or, when the alert names none, after its firing.
    #[test]
    fn a_reading_clears_only_after_the_reading_it_fired_on() {
        let row = |evidence: Value, fired_at_ms: i64| super::Row {
            kind: "memory_low".to_string(),
            subject: "host".to_string(),
            state: "open".to_string(),
            evidence,
            fired_event_id: 7,
            fired_at_ms,
        };
        let fired = row(json!({"reading_ms": 1_000}), 1_200);
        assert!(!super::after(&fired, &json!({"reading_ms": 900}), 5_000));
        assert!(super::after(&fired, &json!({"reading_ms": 1_500}), 5_000));
        let unnamed = row(json!({}), 1_200);
        assert!(!super::after(&unnamed, &json!({}), 1_100));
        assert!(super::after(&unnamed, &json!({}), 1_300));
        // An event names the order whatever the clocks say.
        let record = row(json!({"event_id": 9}), 0);
        assert!(!super::after(
            &record,
            &json!({"event_id": 9, "reading_ms": 9_999}),
            0
        ));
        assert!(super::after(&record, &json!({"event_id": 10}), 0));
    }

    #[test]
    fn a_restarted_loop_reads_its_alerts_back_and_fires_nothing_twice() {
        let mut monitor = monitor();
        feed(
            &mut monitor,
            1,
            0,
            json!({"type": "mission.added", "id": 4, "text": "m"}),
        );
        let events = feed(
            &mut monitor,
            2,
            0,
            json!({"type": "mission.halted", "id": 4, "reason": "halted"}),
        );
        let recorded = decided(&monitor, events);
        commit(&mut monitor, &recorded);
        // The restart: the same records are read again with the keys the
        // store holds; the decision may repeat, but nothing is recorded.
        let replayed = feed(
            &mut monitor,
            2,
            0,
            json!({"type": "mission.halted", "id": 4, "reason": "halted"}),
        );
        assert!(decided(&monitor, replayed).is_empty());
        // And the open alert is still known, so a quiet re-reading of a
        // threshold does not fire it again.
        assert!(monitor.is_open("mission_halted:4:2"));
    }

    /// A `task.gated` of task 7 (mission 4), at the revision the integrator
    /// writes (a rejection bumps the task's), naming the scenarios main is
    /// red on.
    fn red_gated(verdict: &str, revision: i64, red: &[&str]) -> Value {
        let mut record = json!({"type": "task.gated", "id": 7, "verdict": verdict,
                                "revision": revision, "base": "b"});
        if !red.is_empty() {
            record["main_red_scenarios"] = json!(red);
        }
        record
    }

    /// `scenario_red`: once per mission and scenario while red, even when
    /// two gatings naming it are read in one batch; a rejection clears
    /// nothing (it may come before the base phase), nor does another
    /// mission's acceptance; an accepted gating that no longer names it
    /// clears it, and only it.
    #[test]
    fn main_red_on_a_scenario_is_one_alert_until_an_accepted_gate_clears_it() {
        let mut monitor = monitor();
        feed(
            &mut monitor,
            10,
            0,
            json!({"type": "task.assigned", "id": 7, "mission_id": 4, "agent_id": "a"}),
        );
        let first = feed(&mut monitor, 11, 0, red_gated("accepted", 0, &["046.sh"]));
        assert!(
            fires(&first).contains(&"scenario_red:4:046.sh:11"),
            "{first:?}"
        );
        // Not committed yet: the same batch reads a second gating.
        let second = feed(
            &mut monitor,
            12,
            0,
            red_gated("rejected", 0, &["046.sh", "047.sh"]),
        );
        let red: Vec<&str> = fires(&second)
            .into_iter()
            .filter(|key| key.starts_with("scenario_red:"))
            .collect();
        assert_eq!(red, ["scenario_red:4:047.sh:12"], "{second:?}");
        commit(&mut monitor, &first);
        commit(&mut monitor, &second);
        let Event::Fire {
            evidence, notice, ..
        } = first
            .iter()
            .find(|event| event.key() == "scenario_red:4:046.sh:11")
            .expect("the fire")
        else {
            panic!("a fire")
        };
        assert!(!notice, "a condition, not a notice");
        assert_eq!(evidence["scenario"], "046.sh");
        assert_eq!(evidence["mission"], 4);
        // Another mission's accepted gate is no word on mission 4's main.
        feed(
            &mut monitor,
            30,
            0,
            json!({"type": "task.assigned", "id": 8, "mission_id": 5, "agent_id": "b"}),
        );
        let elsewhere = feed(
            &mut monitor,
            31,
            0,
            json!({"type": "task.gated", "id": 8, "verdict": "accepted", "revision": 0, "base": "c"}),
        );
        assert!(
            !elsewhere
                .iter()
                .any(|event| matches!(event, Event::Clear { .. })),
            "{elsewhere:?}"
        );
        let rejected = feed(&mut monitor, 13, 0, red_gated("rejected", 1, &[]));
        assert!(
            !rejected
                .iter()
                .any(|event| matches!(event, Event::Clear { .. })),
            "{rejected:?}"
        );
        let accepted = feed(&mut monitor, 14, 0, red_gated("accepted", 2, &["047.sh"]));
        let cleared: Vec<&str> = accepted
            .iter()
            .filter(|event| matches!(event, Event::Clear { .. }))
            .map(Event::key)
            .collect();
        assert_eq!(cleared, ["scenario_red:4:046.sh:11"], "{accepted:?}");
        commit(&mut monitor, &accepted);
        assert!(monitor.is_open("scenario_red:4:047.sh:12"));
    }

    /// `validator_defect` (review-fix-gate.md F1 and N1): a task's gate that
    /// ended on one opens it once per mission and scenario, and the
    /// mission's next verdict clears it; an owner's accepted landing that
    /// names one opens it for each mission it moves, and only a gate naming
    /// the scenario `quarantined` clears that one, whatever verdicts come.
    #[test]
    fn a_validator_defect_is_an_alert_until_judged_again_or_quarantined() {
        let mut monitor = monitor();
        feed(
            &mut monitor,
            10,
            0,
            json!({"type": "task.assigned", "id": 7, "mission_id": 4, "agent_id": "a"}),
        );
        let defect = |revision: i64| {
            json!({"type": "task.gated", "id": 7, "verdict": "host_fault", "revision": revision,
                   "base": "b", "evidence": "validator_defect",
                   "validator_defects": ["028.sh"],
                   "fact": {"scenario": "028.sh", "on": "candidate and base"}})
        };
        let first = feed(&mut monitor, 11, 0, defect(0));
        assert_eq!(
            fires(&first)
                .into_iter()
                .filter(|key| key.starts_with("validator_defect:"))
                .collect::<Vec<_>>(),
            ["validator_defect:4:028.sh:11"],
            "{first:?}"
        );
        commit(&mut monitor, &first);
        let again = feed(&mut monitor, 12, 0, defect(0));
        assert!(
            !fires(&again)
                .iter()
                .any(|key| key.starts_with("validator_defect:")),
            "once while open: {again:?}"
        );
        commit(&mut monitor, &again);
        // The owner's landing accepted without 028.sh and 031.sh: its own
        // alert for mission 4 (and mission 9, which it moved too).
        let moved = feed(
            &mut monitor,
            13,
            0,
            json!({"type": "source.moved", "request": 77, "from": "b", "to": "c",
                   "missions": [4, 9], "verdict": "accepted",
                   "validator_defects": ["028.sh"]}),
        );
        let mut owner: Vec<&str> = fires(&moved);
        owner.sort_unstable();
        assert_eq!(
            owner,
            [
                "validator_defect:4:028.sh:13",
                "validator_defect:9:028.sh:13"
            ],
            "{moved:?}"
        );
        commit(&mut monitor, &moved);
        // The mission's next verdict clears the task's, not the owner's.
        let judged = feed(
            &mut monitor,
            14,
            0,
            json!({"type": "task.gated", "id": 7, "verdict": "accepted", "revision": 0,
                   "base": "c"}),
        );
        let cleared: Vec<&str> = judged
            .iter()
            .filter(|event| matches!(event, Event::Clear { .. }))
            .map(Event::key)
            .collect();
        assert_eq!(cleared, ["validator_defect:4:028.sh:11"], "{judged:?}");
        commit(&mut monitor, &judged);
        assert!(monitor.is_open("validator_defect:4:028.sh:13"));
        // A gate of mission 4 that names 028.sh quarantined: the owner chose.
        let quarantined = feed(
            &mut monitor,
            15,
            0,
            json!({"type": "task.gated", "id": 7, "verdict": "rejected", "revision": 0,
                   "base": "c", "quarantined": ["028.sh"]}),
        );
        let cleared: Vec<&str> = quarantined
            .iter()
            .filter(|event| matches!(event, Event::Clear { .. }))
            .map(Event::key)
            .collect();
        assert_eq!(cleared, ["validator_defect:4:028.sh:13"], "{quarantined:?}");
        commit(&mut monitor, &quarantined);
        assert!(
            monitor.is_open("validator_defect:9:028.sh:13"),
            "mission 9's stays"
        );
    }

    /// A restart that reads an open `scenario_red` back fires nothing again
    /// for it, and still clears it at the accepted gating that no longer
    /// names it.
    #[test]
    fn a_restart_keeps_main_red_one_alert() {
        // The restart: the open alert read back, with its evidence.
        let mut restarted = super::Monitor::with(settings());
        restarted.known.insert(
            "scenario_red:4:047.sh:12".to_string(),
            Row {
                kind: "scenario_red".to_string(),
                subject: "mission #4".to_string(),
                state: "open".to_string(),
                evidence: json!({"mission": 4, "scenario": "047.sh"}),
                fired_event_id: 12,
                fired_at_ms: 0,
            },
        );
        restarted.seed_red();
        feed(
            &mut restarted,
            20,
            0,
            json!({"type": "task.assigned", "id": 7, "mission_id": 4, "agent_id": "a"}),
        );
        let again = feed(&mut restarted, 21, 0, red_gated("rejected", 0, &["047.sh"]));
        assert!(
            !fires(&again)
                .iter()
                .any(|key| key.starts_with("scenario_red:")),
            "{again:?}"
        );
        let green = feed(&mut restarted, 22, 0, red_gated("accepted", 1, &[]));
        assert!(
            green
                .iter()
                .any(|event| matches!(event, Event::Clear { key, .. } if key == "scenario_red:4:047.sh:12")),
            "{green:?}"
        );
    }

    /// A restart that reads an open `message_not_taken` alert back: the
    /// message, still not taken, fires nothing again; taken, it clears,
    /// once. A message not due yet is pending, sets the step's next bound,
    /// and fires once at it.
    #[test]
    fn a_restart_keeps_a_message_not_taken_one_alert() {
        let message = |id: i64, due: i64, not_taken: bool, taken: i64| {
            json!({"command_id": id, "agent_id": "agent_worker_a",
                   "session_id": "session_worker_a", "action": "input",
                   "created_at_ms": due - 120_000, "due_ms": due, "tracked": true,
                   "not_taken": not_taken, "typed": true, "taken": taken,
                   "taken_at_ms": if taken > 0 { json!(due) } else { Value::Null },
                   "ended": false, "state": if taken > 0 { "taken" } else { "typed" }})
        };
        let mut restarted = super::Monitor::with(settings());
        restarted.known.insert(
            "message_not_taken:41".to_string(),
            Row {
                kind: "message_not_taken".to_string(),
                subject: "session_worker_a".to_string(),
                state: "open".to_string(),
                evidence: json!({"command": 41, "typed": true}),
                fired_event_id: 12,
                fired_at_ms: 0,
            },
        );
        assert_eq!(restarted.open_messages(), [41]);
        let still = json!({"now_ms": 500_000, "max_id": 42, "messages": [
            message(41, 121_000, true, 0), message(42, 610_000, false, 0)]});
        let mut out = Vec::new();
        restarted.messages(&still, &mut out);
        assert!(decided(&restarted, out).is_empty(), "not fired again");
        assert_eq!(restarted.messages_due, Some(610_000));
        assert_eq!(restarted.messages_after, 42);
        assert_eq!(
            restarted
                .pending
                .iter()
                .map(|(id, due)| (*id, *due))
                .collect::<Vec<_>>(),
            [(42, 610_000)]
        );
        assert_eq!(restarted.messages_since, 440_000);
        restarted.plan(500_000);
        assert_eq!(restarted.next_bound_ms, Some(610_000));

        let due = json!({"now_ms": 610_000, "max_id": 42, "messages": [
            message(41, 121_000, true, 1), message(42, 610_000, true, 0)]});
        let mut out = Vec::new();
        restarted.messages(&due, &mut out);
        let events = decided(&restarted, out);
        assert!(
            matches!(&events[..], [Event::Clear { key, evidence }, Event::Fire { key: fired, .. }]
                if key == "message_not_taken:41" && evidence["taken"] == 1
                    && fired == "message_not_taken:42"),
            "{events:?}"
        );
        commit(&mut restarted, &events);
        let mut out = Vec::new();
        restarted.messages(&due, &mut out);
        assert!(decided(&restarted, out).is_empty(), "each once");
        assert!(restarted.pending.is_empty());
        assert_eq!(restarted.messages_due, None);
    }

    /// A restart that reads open `validator_defect` alerts back (rev-b F1),
    /// one of each origin: a repeat of the task's defect fires nothing, the
    /// mission's next verdict clears only the task's, and a gate naming the
    /// scenario quarantined clears the owner's.
    #[test]
    fn a_restart_keeps_each_validator_defect_one_alert() {
        let mut restarted = super::Monitor::with(settings());
        for (key, owner, event) in [
            ("validator_defect:4:028.sh:11", false, 11),
            ("validator_defect:4:028.sh:13", true, 13),
        ] {
            restarted.known.insert(
                key.to_string(),
                Row {
                    kind: "validator_defect".to_string(),
                    subject: "mission #4".to_string(),
                    state: "open".to_string(),
                    evidence: json!({"mission": 4, "scenario": "028.sh", "owner": owner}),
                    fired_event_id: event,
                    fired_at_ms: 0,
                },
            );
        }
        restarted.seed_defects();
        feed(
            &mut restarted,
            20,
            0,
            json!({"type": "task.assigned", "id": 7, "mission_id": 4, "agent_id": "a"}),
        );
        let again = feed(
            &mut restarted,
            21,
            0,
            json!({"type": "task.gated", "id": 7, "verdict": "host_fault", "revision": 0,
                   "base": "b", "evidence": "validator_defect",
                   "validator_defects": ["028.sh"],
                   "fact": {"scenario": "028.sh", "on": "candidate and base"}}),
        );
        assert!(
            !fires(&again)
                .iter()
                .any(|key| key.starts_with("validator_defect:")),
            "{again:?}"
        );
        let cleared = |events: &[Event]| -> Vec<String> {
            events
                .iter()
                .filter(|event| matches!(event, Event::Clear { .. }))
                .map(|event| event.key().to_string())
                .collect()
        };
        let judged = feed(
            &mut restarted,
            22,
            0,
            json!({"type": "task.gated", "id": 7, "verdict": "accepted", "revision": 0,
                   "base": "c"}),
        );
        assert_eq!(
            cleared(&judged),
            ["validator_defect:4:028.sh:11"],
            "{judged:?}"
        );
        commit(&mut restarted, &judged);
        let quarantined = feed(
            &mut restarted,
            23,
            0,
            json!({"type": "task.gated", "id": 7, "verdict": "accepted", "revision": 0,
                   "base": "c", "quarantined": ["028.sh"]}),
        );
        assert_eq!(
            cleared(&quarantined),
            ["validator_defect:4:028.sh:13"],
            "{quarantined:?}"
        );
    }

    #[test]
    fn the_keys_name_the_occurrence() {
        let mut monitor = monitor();
        let events = feed(
            &mut monitor,
            12,
            0,
            json!({"type": "task.assigned", "id": 7, "mission_id": 4, "agent_id": "a"}),
        );
        assert_eq!(fires(&events), ["task_state:7:12"]);
        feed(
            &mut monitor,
            13,
            0,
            json!({"type": "mission.added", "id": 4}),
        );
        let gated = feed(
            &mut monitor,
            14,
            0,
            json!({"type": "task.gated", "id": 7, "verdict": "host_fault",
                   "evidence": "base_disagrees", "alert": true, "revision": 0,
                   "scenario": "011.sh", "base_disagrees": 3}),
        );
        assert!(
            fires(&gated).contains(&"gate_base_disagrees:14"),
            "{gated:?}"
        );
        let drilled = feed(
            &mut monitor,
            15,
            0,
            json!({"type": "backup.verified", "key": "k", "ok": false, "reason": "r"}),
        );
        assert_eq!(fires(&drilled), ["drill_failed:15"]);
        let auth = feed(
            &mut monitor,
            16,
            0,
            json!({"type": "execution.interrupted", "cause": "model_auth",
                   "execution_id": "exec_1", "session_id": "session_x", "agent_id": "a"}),
        );
        assert_eq!(fires(&auth), ["model_auth:16"]);
    }

    /// A mission whose controller lacks the owner's latest landing (P22) fires one
    /// `controller_behind` once it has been behind for the bound, from the
    /// first reading that found it so; nothing more while it stays behind;
    /// clears when the controller catches up (or the mission no longer
    /// waits); a mission caught up before the bound fires nothing.
    #[test]
    fn a_controller_behind_main_fires_after_the_bound_and_clears_when_caught_up() {
        let mut monitor = monitor();
        let c1 = "1".repeat(40);
        let m2 = "2".repeat(40);
        let behind = |missions: &[i64]| -> BTreeMap<i64, (String, String)> {
            missions
                .iter()
                .map(|mission| (*mission, (c1.clone(), m2.clone())))
                .collect()
        };
        assert!(monitor.controller_events(&behind(&[4, 6]), 1000).is_empty());
        assert!(
            monitor
                .controller_events(&behind(&[4, 6]), 900_000)
                .is_empty()
        );
        // Mission 6 caught up before the bound (its deploy came): it starts
        // again from its next reading behind.
        assert!(
            monitor
                .controller_events(&behind(&[4]), 1_000_000)
                .is_empty()
        );
        assert!(
            monitor
                .controller_events(&behind(&[4, 6]), 1_200_000)
                .is_empty()
        );
        let fired = monitor.controller_events(&behind(&[4, 6]), 1_801_000);
        assert_eq!(fires(&fired), ["controller_behind:4:1000"]);
        let Event::Fire {
            subject,
            evidence,
            text,
            notice,
            ..
        } = &fired[0]
        else {
            panic!("a fire: {fired:?}");
        };
        assert_eq!(subject, "mission #4");
        assert_eq!(evidence["controller"], c1.as_str());
        assert_eq!(evidence["landed"], m2.as_str());
        assert_eq!(evidence["since_ms"], 1000);
        assert_eq!(evidence["behind_ms"], 1_800_000);
        assert!(
            text.contains("controller 11111111 has not had the owner's landing 22222222 for 1800s")
                && !notice,
            "{text}"
        );
        commit(&mut monitor, &fired);
        assert!(
            monitor
                .controller_events(&behind(&[4, 6]), 2_000_000)
                .is_empty(),
            "once per occurrence; 6 is not yet behind for the bound"
        );
        let caught_up = monitor.controller_events(&behind(&[6]), 2_100_000);
        assert_eq!(
            caught_up.iter().map(Event::key).collect::<Vec<_>>(),
            ["controller_behind:4:1000"]
        );
        let Event::Clear { evidence, .. } = &caught_up[0] else {
            panic!("a clear: {caught_up:?}");
        };
        assert_eq!(evidence["behind"], false);
        commit(&mut monitor, &caught_up);
        let six = monitor.controller_events(&behind(&[4, 6]), 3_000_000);
        assert_eq!(fires(&six), ["controller_behind:6:1200000"]);
    }

    /// P12: a host-path source of an open or active mission that is not a
    /// directory fires one `source_away` per mission and source while it is
    /// away, clears when it is back or no longer watched, and fires anew
    /// when it goes again.
    #[test]
    fn a_source_away_fires_once_and_clears_when_back_or_unwatched() {
        let mut monitor = monitor();
        let watched: BTreeSet<(i64, String)> = [
            (4, "/mnt/vol/a.git".to_string()),
            (4, "/srv/b.git".to_string()),
            (6, "/mnt/vol/a.git".to_string()),
        ]
        .into();
        let gone = |path: &str| !path.starts_with("/mnt/vol");
        // The first reading that finds them away only notes it.
        assert!(monitor.source_events(&watched, gone, 500).is_empty());
        let first = monitor.source_events(&watched, gone, 1000);
        assert_eq!(fires(&first), ["source_away:4:1000", "source_away:6:1000"]);
        let Event::Fire {
            subject,
            evidence,
            text,
            notice,
            ..
        } = &first[0]
        else {
            panic!("a fire: {first:?}");
        };
        assert_eq!(subject, "mission #4");
        assert_eq!(evidence["source"], "/mnt/vol/a.git");
        assert_eq!(
            evidence["away_since_ms"], 500,
            "since the first reading that found it"
        );
        assert!(
            text.contains("/mnt/vol/a.git is not a directory") && !notice,
            "{text}"
        );
        commit(&mut monitor, &first);
        // Still away: nothing more, however many readings.
        assert!(monitor.source_events(&watched, gone, 2000).is_empty());
        // Back for mission 4's and 6's, which share the path: both clear.
        let back = monitor.source_events(&watched, |_| true, 3000);
        assert_eq!(
            back.iter().map(Event::key).collect::<Vec<_>>(),
            ["source_away:4:1000", "source_away:6:1000"]
        );
        let Event::Clear { evidence, .. } = &back[0] else {
            panic!("a clear: {back:?}");
        };
        assert_eq!(evidence["present"], true);
        commit(&mut monitor, &back);
        // Away again: a new occurrence, a new key, after two readings again.
        assert!(monitor.source_events(&watched, gone, 3500).is_empty());
        let again = monitor.source_events(&watched, gone, 4000);
        assert_eq!(fires(&again), ["source_away:4:4000", "source_away:6:4000"]);
        commit(&mut monitor, &again);
        // Mission 6 closed (or dropped the source): its alert clears, still
        // away or not; mission 4's stays.
        let fewer: BTreeSet<(i64, String)> = watched
            .iter()
            .filter(|(mission, _)| *mission == 4)
            .cloned()
            .collect();
        let closed = monitor.source_events(&fewer, gone, 5000);
        assert_eq!(
            closed.iter().map(Event::key).collect::<Vec<_>>(),
            ["source_away:6:4000"]
        );
        let Event::Clear { evidence, .. } = &closed[0] else {
            panic!("a clear: {closed:?}");
        };
        assert_eq!(evidence["reason"], "no longer watched");
    }

    /// rev-f's N2: a path that is away at one reading and back at the next,
    /// again and again, fires nothing; one away at two readings in a row
    /// fires once.
    #[test]
    fn a_source_that_comes_and_goes_does_not_flap() {
        let mut flapping = monitor();
        let watched: BTreeSet<(i64, String)> = [(4, "/mnt/vol/a.git".to_string())].into();
        for reading in 0..6 {
            let away = reading % 2 == 0;
            let events = flapping.source_events(&watched, |_| !away, reading * 1000);
            assert!(events.is_empty(), "reading {reading}: {events:?}");
        }
        assert!(flapping.source_events(&watched, |_| false, 6000).is_empty());
        let fired = flapping.source_events(&watched, |_| false, 7000);
        assert_eq!(fires(&fired), ["source_away:4:7000"]);
        // Unwatched in between: the count starts again.
        let mut other = monitor();
        assert!(other.source_events(&watched, |_| false, 0).is_empty());
        assert!(
            other
                .source_events(&BTreeSet::new(), |_| false, 1000)
                .is_empty()
        );
        assert!(other.source_events(&watched, |_| false, 2000).is_empty());
        assert_eq!(
            fires(&other.source_events(&watched, |_| false, 3000)),
            ["source_away:4:3000"]
        );
    }

    #[test]
    fn only_a_path_on_this_host_is_a_watched_source() {
        let listed = json!([
            {"mission": 4, "sources": ["/srv/a.git", "https://example.com/b.git", "relative/c",
                                       "git@example.com:d.git", 7]},
            {"mission": 6, "sources": ["/srv/a.git"]},
            {"mission": null, "sources": ["/srv/e.git"]},
        ]);
        let watched: Vec<(i64, String)> = host_sources(&listed).into_iter().collect();
        assert_eq!(
            watched,
            [(4, "/srv/a.git".to_string()), (6, "/srv/a.git".to_string())]
        );
    }

    #[test]
    fn memory_and_tmpfs_do_not_flap_between_their_thresholds() {
        // MemAvailable: fire under 4096, clear at 8192; 5000 in between is
        // neither, whichever side it comes from.
        assert_eq!(crossing(false, 2000, 4096, 8192, true), Crossing::Fire);
        assert_eq!(crossing(true, 5000, 4096, 8192, true), Crossing::Nothing);
        assert_eq!(crossing(true, 9000, 4096, 8192, true), Crossing::Clear);
        assert_eq!(crossing(false, 9000, 4096, 8192, true), Crossing::Nothing);
        assert_eq!(crossing(true, 4096, 4096, 8192, true), Crossing::Nothing);
        // The filesystem: fire over 8000, clear under 6000.
        assert_eq!(crossing(false, 9000, 8000, 6000, false), Crossing::Fire);
        assert_eq!(crossing(true, 7000, 8000, 6000, false), Crossing::Nothing);
        assert_eq!(crossing(true, 5000, 8000, 6000, false), Crossing::Clear);
        assert_eq!(crossing(false, 5000, 8000, 6000, false), Crossing::Nothing);
        // A threshold the host never crosses: both equal, the usual run's
        // 1/1 and 100000000/100000000.
        assert_eq!(crossing(false, 19_000, 1, 1, true), Crossing::Nothing);
        assert_eq!(crossing(true, 19_000, 1, 1, true), Crossing::Clear);
        assert_eq!(
            crossing(false, 4000, 100_000_000, 100_000_000, false),
            Crossing::Nothing
        );
    }

    #[test]
    fn a_command_failure_is_a_notice_and_an_owner_note_is_not() {
        let mut monitor = monitor();
        let commands = json!([
            {"command": 5, "agent_id": "agent_a", "session_id": "session_a",
             "principal": "h", "action": "input", "key": "k-5", "error": "no execution"},
            {"command": 6, "agent_id": "owner", "session_id": "session_owner",
             "principal": "owner", "action": "input", "key": "owner-note-1", "error": "sent"},
        ]);
        let mut out = Vec::new();
        monitor.commands(&commands, &mut out);
        assert_eq!(fires(&out), ["command_failed:5"]);
        let Event::Fire {
            subject,
            evidence,
            notice,
            ..
        } = &out[0]
        else {
            panic!("a fire");
        };
        assert_eq!(subject, "command #5");
        assert!(notice);
        assert_eq!(evidence["error"], "no execution");
        assert_eq!(evidence["principal"], "h");
    }

    #[test]
    fn a_crash_line_is_parsed_with_utc_and_offset_stamps() {
        let base = days_from_civil(2026, 11, 8) * 86_400_000 + (18 * 3600 + 25 * 60) * 1000 + 123;
        let now = base + 5_000;
        let (stamp, prefix) = crash_stamp(
            "2026-11-08 18:25:00.123 UTC [1] LOG:  server process (PID 4242) was \
terminated by signal 11: Segmentation fault",
            now,
        )
        .expect("a crash stamp");
        assert_eq!(stamp, base);
        assert_eq!(prefix, "2026-11-08T18:25:00.123UTC[1]");
        // +08:00 is eight hours ahead of UTC.
        let (stamp, prefix) = crash_stamp(
            "2026-11-09 02:25:00.123 +08:00 [77] ERROR:  could not resize shared memory \
segment \"/PostgreSQL.2051\" to 8388608 bytes: No space left on device",
            now,
        )
        .expect("an offset stamp");
        assert_eq!(stamp, base);
        assert_eq!(prefix, "2026-11-09T02:25:00.123+08:00[77]");
        // The third pattern, and a line that is not one at all.
        assert!(
            crash_stamp(
                "2026-11-08 18:25:00.123 UTC [3] FATAL:  No space left on device",
                now
            )
            .is_some()
        );
        assert!(
            crash_stamp(
                "2026-11-08 18:25:00.123 UTC [3] LOG:  checkpoint complete",
                now
            )
            .is_none()
        );
        assert!(crash_stamp("not a log line", now).is_none());
        // Older than the day: history, not a reading.
        assert!(
            crash_stamp(
                "2026-11-06 18:25:00.123 UTC [1] LOG:  terminated by signal 11",
                now
            )
            .is_none()
        );
    }

    #[test]
    fn a_crash_clears_at_the_ready_line_or_by_quiet() {
        let mut monitor = monitor();
        let now = 1_000_000;
        let crash = "2026-11-08 18:25:00.123 UTC [1] LOG:  server process (PID 4242) was \
terminated by signal 11: Segmentation fault\n";
        let mut out = Vec::new();
        monitor.lines_of_log(0, Path::new("/var/log/pg.log"), now, crash, &mut out);
        assert_eq!(
            fires(&out),
            ["postgres_crash:test:2026-11-08T18:25:00.123UTC[1]"]
        );
        let recorded = decided(&monitor, out);
        commit(&mut monitor, &recorded);

        // The same lines read again: nothing fires twice.
        let mut again = Vec::new();
        monitor.lines_of_log(0, Path::new("/var/log/pg.log"), now, crash, &mut again);
        assert!(decided(&monitor, again).is_empty());

        // Quiet: no ready line for the bound clears it, with the last line.
        monitor.logs[0].last_match_ms = now;
        let mut quiet = Vec::new();
        monitor.quiet_crashes(now + settings().quiet_ms + 1, &mut quiet);
        assert_eq!(quiet.len(), 1);
        assert!(matches!(&quiet[0], Event::Clear { evidence, .. }
            if evidence["quiet_secs"] == 900));

        // A ready line clears it at once, quoting the line.
        let ready = "2026-11-08 18:26:00.000 UTC [1] LOG:  database system is ready to accept \
connections\n";
        let mut cleared = Vec::new();
        monitor.lines_of_log(0, Path::new("/var/log/pg.log"), now, ready, &mut cleared);
        assert_eq!(cleared.len(), 1);
        assert!(matches!(&cleared[0], Event::Clear { evidence, .. }
            if evidence["line"].as_str().is_some_and(|line| line.contains("ready to accept"))));
    }

    #[test]
    fn backup_ages_fire_without_a_record_not_at_all_and_again_after_the_new_one_ages() {
        let mut monitor = monitor();
        let mut out = Vec::new();
        monitor.bounds(10_000_000, &mut out);
        assert!(out.is_empty(), "no backup record at all is no alert");

        let dumped = feed(
            &mut monitor,
            20,
            1000,
            json!({"type": "backup.dumped", "keys": ["postgres/hourly/a.dump"],
                   "bytes": 1, "events": 2}),
        );
        assert!(dumped.is_empty(), "a fresh dump is no alert");
        let mut early = Vec::new();
        monitor.bounds(1000 + 7_199_999, &mut early);
        assert!(early.is_empty(), "never before its age");

        let mut due = Vec::new();
        monitor.bounds(1000 + 7_200_000, &mut due);
        assert_eq!(fires(&due), ["backup_stale:dump:20"]);
        let Event::Fire {
            evidence, subject, ..
        } = &due[0]
        else {
            panic!("a fire");
        };
        assert_eq!(subject, "dump");
        assert_eq!(evidence["age_secs"], 7200);
        assert_eq!(evidence["max_age_secs"], 7200);
        assert_eq!(evidence["key"], "postgres/hourly/a.dump");
        let recorded = decided(&monitor, due);
        commit(&mut monitor, &recorded);

        let newer = feed(
            &mut monitor,
            21,
            1000 + 7_200_000,
            json!({"type": "backup.dumped", "keys": ["postgres/hourly/b.dump"], "bytes": 1}),
        );
        let cleared = decided(&monitor, newer);
        assert_eq!(cleared.len(), 1);
        assert!(matches!(&cleared[0], Event::Clear { key, evidence }
            if key == "backup_stale:dump:20" && evidence["event_id"] == 21));
        commit(&mut monitor, &cleared);

        // The newer dump growing old is a new occurrence with its own key.
        let mut later = Vec::new();
        monitor.bounds(1000 + 2 * 7_200_000, &mut later);
        assert_eq!(fires(&later), ["backup_stale:dump:21"]);
    }

    #[test]
    fn a_missions_daily_budget_alerts_once_a_day_and_clears_with_the_day_or_the_budget() {
        let mut monitor = monitor();
        // 2026-11-10 12:00 UTC.
        let noon = 1_794_312_000_000_i64;
        let reading = |spent: f64, budget: f64| {
            json!([{"mission": 4, "budget_usd": budget, "spent_usd": spent},
                   {"mission": 5, "budget_usd": 1.0, "spent_usd": 0.06}])
        };
        let read = |monitor: &mut Monitor, now: i64, value: Value| {
            let mut out = Vec::new();
            monitor.budgets(now, &value, &mut out);
            let out = decided(monitor, out);
            commit(monitor, &out);
            out
        };
        assert!(
            read(&mut monitor, noon, reading(0.4855, 0.5)).is_empty(),
            "under: none"
        );
        let fired = read(&mut monitor, noon + 2000, reading(0.9355, 0.5));
        assert_eq!(fires(&fired), ["cost_budget:4:2026-11-10"]);
        let Event::Fire {
            subject,
            text,
            evidence,
            ..
        } = &fired[0]
        else {
            panic!("a fire");
        };
        assert_eq!(subject, "mission #4");
        assert_eq!(
            text,
            "mission #4 spent $0.94 today (UTC), over its $0.50 daily budget"
        );
        assert_eq!(evidence["day"], "2026-11-10");
        // More spending the same day: nothing more.
        assert!(read(&mut monitor, noon + 4000, reading(1.2355, 0.5)).is_empty());
        // The budget raised above the spend clears it, and the day's key is
        // spent: going over again that day fires nothing.
        let raised = read(&mut monitor, noon + 6000, reading(1.2355, 2.0));
        assert!(
            matches!(&raised[..], [Event::Clear { key, .. }] if key == "cost_budget:4:2026-11-10")
        );
        assert!(read(&mut monitor, noon + 8000, reading(2.5, 2.0)).is_empty());
        // The next UTC day is a new occurrence.
        let tomorrow = noon + super::DAY_MS;
        let next = read(&mut monitor, tomorrow, reading(0.6, 0.5));
        assert_eq!(fires(&next), ["cost_budget:4:2026-11-11"]);
        // Its day over: cleared at the next day's first reading.
        let after = read(&mut monitor, tomorrow + super::DAY_MS, reading(0.1, 0.5));
        assert!(
            matches!(&after[..], [Event::Clear { key, .. }] if key == "cost_budget:4:2026-11-11")
        );
        // A budget removed clears its open alert.
        let third = tomorrow + super::DAY_MS + 1000;
        assert_eq!(
            fires(&read(&mut monitor, third, reading(0.7, 0.5))),
            ["cost_budget:4:2026-11-12"]
        );
        let removed = read(&mut monitor, third + 1000, json!([]));
        assert!(
            matches!(&removed[..], [Event::Clear { key, .. }] if key == "cost_budget:4:2026-11-12")
        );
    }

    #[test]
    fn a_budget_over_alert_fires_once_and_clears_under_it() {
        let mut monitor = monitor();
        let over = feed(
            &mut monitor,
            30,
            1000,
            json!({"type": "backup.dumped", "keys": ["k1"], "over_alert": true,
                   "held_bytes": 8_100_000_000_i64, "written_bytes": 2_400_100_i64,
                   "alert_bytes": 8_000_000_000_i64, "limit_bytes": 10_000_000_000_i64,
                   "skipped": []}),
        );
        assert_eq!(fires(&over), ["backup_budget:30"]);
        let Event::Fire {
            subject, evidence, ..
        } = &over[0]
        else {
            panic!("a fire");
        };
        assert_eq!(subject, "r2");
        assert_eq!(evidence["held_bytes"], 8_100_000_000_i64);
        assert_eq!(evidence["alert_bytes"], 8_000_000_000_i64);
        let recorded = decided(&monitor, over);
        commit(&mut monitor, &recorded);

        let still = feed(
            &mut monitor,
            31,
            2000,
            json!({"type": "backup.dumped", "keys": ["k2"], "over_alert": true,
                   "held_bytes": 8_200_000_000_i64}),
        );
        assert!(
            !fires(&still)
                .iter()
                .any(|key| key.starts_with("backup_budget:")),
            "one alert while it is open"
        );

        let under = feed(
            &mut monitor,
            32,
            3000,
            json!({"type": "backup.dumped", "keys": ["k3"], "over_alert": false,
                   "held_bytes": 7_900_000_000_i64}),
        );
        let cleared = decided(&monitor, under);
        assert!(cleared.iter().any(|event| matches!(event,
            Event::Clear { key, evidence } if key == "backup_budget:30"
                && evidence["event_id"] == 32)));
    }

    #[test]
    fn a_base_disagreement_is_a_notice_only_when_its_record_says_alert() {
        let mut monitor = monitor();
        feed(
            &mut monitor,
            39,
            0,
            json!({"type": "task.assigned", "id": 7, "mission_id": 4, "agent_id": "a"}),
        );
        let record = json!({"type": "task.gated", "id": 7, "verdict": "host_fault",
                            "evidence": "base_disagrees", "alert": true, "revision": 0,
                            "scenario": "011.sh", "exits": [1, 0], "setup_s": 244,
                            "base_disagrees": 3, "due_ms": 5});
        let events = feed(&mut monitor, 40, 0, record.clone());
        assert!(fires(&events).contains(&"gate_base_disagrees:40"));
        let quiet = feed(
            &mut monitor,
            41,
            0,
            json!({"type": "task.gated", "id": 7, "verdict": "host_fault",
                   "evidence": "base_disagrees", "revision": 0}),
        );
        assert!(
            !fires(&quiet)
                .iter()
                .any(|key| key.starts_with("gate_base_disagrees:")),
            "no alert without the record's own flag"
        );
    }

    #[test]
    fn an_admission_fires_after_its_bound_and_clears_at_its_admission() {
        let mut monitor = monitor();
        let waiting = feed(
            &mut monitor,
            50,
            1000,
            json!({"type": "admission.waiting", "kind": "execution",
                   "agent_id": "agent_worker_bb_wait", "session_id": "s",
                   "envelope_bytes": 6_442_450_944_i64, "measure": "host",
                   "measured_bytes": 2_147_483_648_i64,
                   "threshold_bytes": 6_442_450_944_i64}),
        );
        assert!(waiting.is_empty(), "a wait is not yet an alert");
        let mut early = Vec::new();
        monitor.bounds(1000 + 599_999, &mut early);
        assert!(early.is_empty(), "never before its bound");
        let mut due = Vec::new();
        monitor.bounds(1000 + 600_000, &mut due);
        assert_eq!(fires(&due), ["admission_wait:50"]);
        let Event::Fire {
            subject, evidence, ..
        } = &due[0]
        else {
            panic!("a fire");
        };
        assert_eq!(subject, "agent_worker_bb_wait");
        assert_eq!(evidence["measure"], "host");
        let recorded = decided(&monitor, due);
        commit(&mut monitor, &recorded);

        let admitted = feed(
            &mut monitor,
            51,
            1000 + 610_000,
            json!({"type": "admission.admitted", "kind": "execution",
                   "agent_id": "agent_worker_bb_wait", "waited_ms": 610_000}),
        );
        let cleared = decided(&monitor, admitted);
        assert_eq!(cleared.len(), 1);
        assert!(matches!(&cleared[0], Event::Clear { key, evidence }
            if key == "admission_wait:50" && evidence["waited_ms"] == 610_000));
    }

    #[test]
    fn a_stuck_judgement_fires_once_and_a_calm_one_clears_it() {
        let mut monitor = monitor();
        let stuck = json!([{"session_id": "session_resident_r1", "agent_id": "agent_resident_r1",
                            "event_id": 60, "at_ms": 0,
                            "observation": {"stuck": 0.9, "state": "WAITING", "judge": "jev"}}]);
        let mut out = Vec::new();
        monitor.observe(&stuck, &mut out);
        assert_eq!(fires(&out), ["agent_stuck:session_resident_r1:60"]);
        let Event::Fire { evidence, .. } = &out[0] else {
            panic!("a fire");
        };
        assert_eq!(evidence["stuck"], 0.9);
        assert!(evidence["stuck"].as_f64().unwrap_or_default() >= LIKELY);
        let recorded = decided(&monitor, out);
        commit(&mut monitor, &recorded);

        let still = json!([{"session_id": "session_resident_r1", "agent_id": "agent_resident_r1",
                            "event_id": 61, "at_ms": 0,
                            "observation": {"stuck": 0.8, "state": "WAITING", "judge": "jev"}}]);
        let mut again = Vec::new();
        monitor.observe(&still, &mut again);
        assert!(
            decided(&monitor, again).is_empty(),
            "a later stuck fires nothing"
        );

        let calm = json!([{"session_id": "session_resident_r1", "agent_id": "agent_resident_r1",
                           "event_id": 62, "at_ms": 0,
                           "observation": {"stuck": 0.05, "state": "WAITING", "judge": "jev"}}]);
        let mut cleared = Vec::new();
        monitor.observe(&calm, &mut cleared);
        let cleared = decided(&monitor, cleared);
        assert!(matches!(&cleared[0], Event::Clear { evidence, .. }
            if evidence["stuck"] == 0.05 && evidence["event_id"] == 62));
    }

    /// One mission (`accept`: `gate` or `review`) and its task #7 with
    /// `agent_worker_w`, moved by `records` (each a control record, its
    /// event id 10, 11, ...).
    fn worker_monitor(accept: &str, records: &[Value]) -> Monitor {
        let mut monitor = monitor();
        let mut all = vec![
            (1, json!({"type": "mission.added", "id": 4, "text": "m", "origin": "human"})),
            (2, json!({"type": "mission.configured", "id": 4, "accept": accept})),
            (
                10,
                json!({"type": "task.assigned", "id": 7, "mission_id": 4,
                       "agent_id": "agent_worker_w", "objective": 26}),
            ),
            (11, json!({"type": "task.delivered", "id": 7, "command_id": 3})),
        ];
        all.extend((12..).zip(records.iter().cloned()));
        for (id, record) in all {
            let events = feed(&mut monitor, id, id * 1000, record);
            commit(&mut monitor, &events);
        }
        monitor
    }

    /// A judgement of `session` (`agent`) at event 100, stuck 0.9, WAITING.
    fn stuck_of(session: &str, agent: &str) -> Value {
        json!([{"session_id": session, "agent_id": agent, "event_id": 100, "at_ms": 0,
                "observation": {"stuck": 0.9, "state": "WAITING", "judge": "jev"}}])
    }

    fn observed(monitor: &mut Monitor, observations: &Value) -> Vec<Event> {
        let mut out = Vec::new();
        monitor.observe(observations, &mut out);
        decided(monitor, out)
    }

    /// Objective 057 (a): a worker whose task is past its part fires
    /// nothing, whatever the judge's score: reported (waiting for its gate,
    /// or for a review), gated stale (reported again), accepted by its
    /// review (waiting for its landing), landing after an accepted gate (or
    /// held behind the owner's hold: `task.held` leaves it landing), landed.
    #[test]
    fn a_worker_whose_part_is_done_is_never_alerted_stuck() {
        let reported = json!({"type": "task.reported", "id": 7, "commit": "c"});
        let cases: Vec<(&str, &str, Vec<Value>)> = vec![
            ("reported, waiting for its gate", "gate", vec![reported.clone()]),
            ("reported, waiting for a review", "review", vec![reported.clone()]),
            (
                "gated stale: reported again",
                "gate",
                vec![
                    reported.clone(),
                    json!({"type": "task.gated", "id": 7, "verdict": "stale"}),
                ],
            ),
            (
                "gated accepted: landing",
                "gate",
                vec![
                    reported.clone(),
                    json!({"type": "task.gated", "id": 7, "verdict": "accepted"}),
                ],
            ),
            (
                "landing, held behind the owner's hold",
                "gate",
                vec![
                    reported.clone(),
                    json!({"type": "task.gated", "id": 7, "verdict": "accepted"}),
                    json!({"type": "task.held", "id": 7, "hold": 2, "source": "/s"}),
                ],
            ),
            (
                "reviewed accepted",
                "review",
                vec![
                    reported.clone(),
                    json!({"type": "task.reviewed", "id": 7, "accepted": true}),
                ],
            ),
            (
                "landed",
                "gate",
                vec![
                    reported.clone(),
                    json!({"type": "task.gated", "id": 7, "verdict": "accepted"}),
                    json!({"type": "task.landed", "id": 7}),
                ],
            ),
        ];
        for (what, accept, records) in cases {
            let mut monitor = worker_monitor(accept, &records);
            let out = observed(&mut monitor, &stuck_of("session_worker_w", "agent_worker_w"));
            assert!(out.is_empty(), "{what}: {out:?}");
        }
    }

    /// Objective 057 (b): a worker stuck in its own work (its task
    /// delivered) fires once, naming its task and the one command that acts
    /// on it; one whose task went back for revision names its task, and no
    /// command (a restart takes a delivered task only).
    #[test]
    fn a_worker_stuck_in_its_task_names_the_task_and_its_command() {
        let mut monitor = worker_monitor("gate", &[]);
        let out = observed(&mut monitor, &stuck_of("session_worker_w", "agent_worker_w"));
        assert_eq!(fires(&out), ["agent_stuck:session_worker_w:100"]);
        let Event::Fire { evidence, text, .. } = &out[0] else {
            panic!("a fire");
        };
        assert_eq!(evidence["task"], 7);
        assert_eq!(evidence["task_state"], "delivered");
        assert_eq!(evidence["command"], "spawn task restart --id 7");
        assert_eq!(evidence["event_id"], 100);
        assert!(text.contains("spawn task restart --id 7"), "{text}");
        commit(&mut monitor, &out);
        let again = observed(&mut monitor, &stuck_of("session_worker_w", "agent_worker_w"));
        assert!(again.is_empty(), "one alert: {again:?}");

        let mut revising = worker_monitor(
            "gate",
            &[
                json!({"type": "task.reported", "id": 7, "commit": "c"}),
                json!({"type": "task.gated", "id": 7, "verdict": "rejected", "revision": 0}),
            ],
        );
        let out = observed(&mut revising, &stuck_of("session_worker_w", "agent_worker_w"));
        let Event::Fire { evidence, .. } = &out[0] else {
            panic!("a fire: {out:?}");
        };
        assert_eq!(evidence["task"], 7);
        assert_eq!(evidence["task_state"], "revise");
        assert_eq!(evidence["command"], json!(null));
    }

    /// Objective 057 (a): an open stuck alert of a worker clears at the
    /// record that moved its task past its part, that record's id its
    /// `event_id`: its `task.reported`, and as well a `task.gated` or a
    /// `task.landed` for one still open.
    #[test]
    fn a_workers_stuck_alert_clears_at_its_tasks_report() {
        for (records, clearing) in [
            (
                vec![json!({"type": "task.reported", "id": 7, "commit": "c"})],
                "task.reported",
            ),
            (
                vec![
                    json!({"type": "task.reported", "id": 7, "commit": "c"}),
                    json!({"type": "task.gated", "id": 7, "verdict": "stale"}),
                ],
                "task.reported",
            ),
        ] {
            let mut monitor = worker_monitor("gate", &[]);
            let fired = observed(&mut monitor, &stuck_of("session_worker_w", "agent_worker_w"));
            commit(&mut monitor, &fired);
            let mut cleared = Vec::new();
            for (offset, record) in records.iter().enumerate() {
                let id = 200 + i64::try_from(offset).unwrap_or_default();
                let events = feed(&mut monitor, id, 0, record.clone());
                let events = decided(&monitor, events);
                commit(&mut monitor, &events);
                cleared.extend(events.into_iter().filter(|event| matches!(event, Event::Clear { .. })));
            }
            assert_eq!(cleared.len(), 1, "{cleared:?}");
            assert!(matches!(&cleared[0], Event::Clear { key, evidence }
                if key == "agent_stuck:session_worker_w:100" && evidence["event_id"] == 200
                    && evidence["record"] == clearing && evidence["task"] == 7));
            let after = observed(&mut monitor, &stuck_of("session_worker_w", "agent_worker_w"));
            assert!(after.is_empty(), "nothing fires again: {after:?}");
        }
        // Landed by its gate's landing: cleared at the gated accepted.
        let mut monitor = worker_monitor(
            "gate",
            &[json!({"type": "task.reported", "id": 7, "commit": "c"})],
        );
        monitor.known.insert(
            "agent_stuck:session_worker_w:100".to_string(),
            Row {
                kind: "agent_stuck".to_string(),
                subject: "session_worker_w".to_string(),
                state: "open".to_string(),
                evidence: json!({"agent_id": "agent_worker_w", "event_id": 100}),
                fired_event_id: 101,
                fired_at_ms: 0,
            },
        );
        let events = feed(
            &mut monitor,
            300,
            0,
            json!({"type": "task.gated", "id": 7, "verdict": "accepted"}),
        );
        let cleared: Vec<Event> = decided(&monitor, events)
            .into_iter()
            .filter(|event| matches!(event, Event::Clear { .. }))
            .collect();
        assert!(matches!(&cleared[..], [Event::Clear { evidence, .. }]
            if evidence["event_id"] == 300 && evidence["record"] == "task.gated"
                && evidence["task_state"] == "landing"));
    }

    fn supervisor_stuck() -> Value {
        stuck_of("session_supervisor", "agent_supervisor")
    }

    fn item(tag: &str, agent: Option<&str>, judgement: bool) -> super::DispatchItem {
        super::DispatchItem {
            tag: tag.to_string(),
            agent: agent.map(str::to_string),
            judgement,
        }
    }

    /// Objective 057 (a): the supervisor with nothing to dispatch fires
    /// nothing, whatever the judge's score; each of its three conditions
    /// fires one: a question to the owner, a pending or told escalation
    /// (a worker's own judgement excepted while it holds its task), a
    /// mission waiting for its first task. Its plan unread: its judgement
    /// alone, as before.
    #[test]
    fn the_supervisor_is_alerted_stuck_only_with_something_to_dispatch() {
        // Its one mission's task with a worker: nothing to dispatch.
        let mut monitor = worker_monitor("gate", &[]);
        monitor.supervisor = Some(super::Dispatch::default());
        assert!(observed(&mut monitor, &supervisor_stuck()).is_empty());
        // A worker's judgement about its task is the worker's (its own alert).
        monitor.supervisor = Some(super::Dispatch {
            question: false,
            items: vec![item("session_worker_w 99", Some("agent_worker_w"), true)],
        });
        assert!(observed(&mut monitor, &supervisor_stuck()).is_empty());

        let fired = |dispatch: super::Dispatch, monitor: &mut Monitor| {
            monitor.supervisor = Some(dispatch);
            let out = observed(monitor, &supervisor_stuck());
            assert_eq!(fires(&out), ["agent_stuck:session_supervisor:100"]);
            let Event::Fire { evidence, .. } = &out[0] else {
                panic!("a fire");
            };
            evidence["dispatch"].clone()
        };
        // 1. A question to the owner.
        let mut asking = worker_monitor("gate", &[]);
        let why = fired(
            super::Dispatch {
                question: true,
                items: Vec::new(),
            },
            &mut asking,
        );
        assert_eq!(why, json!(["a question to the owner"]));
        // 2. A pending or told escalation: a report to review, a stall, a
        // worker's judgement while it holds no task.
        for escalation in [
            item("session_worker_w report-7-0", Some("agent_worker_w"), false),
            item("missions stall-7-halted-3", None, false),
            item("session_worker_x 98", Some("agent_worker_x"), true),
        ] {
            let mut escalated = worker_monitor("gate", &[]);
            let tag = escalation.tag.clone();
            let why = fired(
                super::Dispatch {
                    question: false,
                    items: vec![escalation],
                },
                &mut escalated,
            );
            assert_eq!(why, json!([format!("escalation {tag}")]));
        }
        // 3. A mission of its own waiting for its first task (a gated
        // mission's objectives are the integrator's).
        let mut waiting = worker_monitor("gate", &[]);
        let mut events = Vec::new();
        waiting.feed(
            50,
            0,
            &json!({"type": "mission.added", "id": 5, "text": "n", "origin": "human"}),
            &mut events,
        );
        waiting.feed(
            51,
            0,
            &json!({"type": "mission.added", "id": 6, "text": "g", "origin": "human"}),
            &mut events,
        );
        waiting.feed(
            52,
            0,
            &json!({"type": "mission.configured", "id": 6, "accept": "gate"}),
            &mut events,
        );
        let why = fired(super::Dispatch::default(), &mut waiting);
        assert_eq!(why, json!(["mission #5 waits for its first task"]));
        // Unread: the judgement alone.
        let mut unread = worker_monitor("gate", &[]);
        unread.supervisor = None;
        assert_eq!(
            fires(&observed(&mut unread, &supervisor_stuck())),
            ["agent_stuck:session_supervisor:100"]
        );
    }

    /// Objective 057 (a): the supervisor's open stuck alert clears once it
    /// has nothing to dispatch, at the latest record the step read.
    #[test]
    fn the_supervisors_stuck_alert_clears_once_nothing_is_to_dispatch() {
        let mut monitor = worker_monitor("gate", &[]);
        monitor.supervisor = Some(super::Dispatch {
            question: true,
            items: Vec::new(),
        });
        let fired = observed(&mut monitor, &supervisor_stuck());
        commit(&mut monitor, &fired);
        monitor.supervisor = Some(super::Dispatch::default());
        monitor.watermark = 120;
        let cleared = observed(&mut monitor, &supervisor_stuck());
        assert!(matches!(&cleared[..], [Event::Clear { key, evidence }]
            if key == "agent_stuck:session_supervisor:100" && evidence["event_id"] == 120
                && evidence["nothing_to_dispatch"] == true));
    }

    /// The stuck alert against the real store: the session's events are
    /// appended to a schema of its own in the test database
    /// (`SPAWN_TEST_DATABASE_URL`, never the live one) and read back by
    /// `alert_observations`, as the loop's step reads them.
    /// An alert clears only on evidence after what it fired on, whatever
    /// restarts (objective 050): the step meets a history with a halt, its
    /// resume and a second halt, two dumps and a good then a failed drill.
    mod restarts {
        use super::super::Monitor;
        use super::settings;
        use serde_json::Value;
        use spawn_postgres::PostgresStore;

        const HOUR: i64 = 3_600_000;

        /// Run `case` on a migrated schema of its own, with an owner
        /// connection to it; dropped afterwards.
        fn in_schema(name: &str, case: impl FnOnce(&PostgresStore, &mut postgres::Client)) {
            let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
                "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
            );
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.subsec_nanos());
            let schema = format!("spawn_test_{}_{nanos:x}_{name}", std::process::id());
            let store = PostgresStore::connect_in_schema(&url, &schema)
                .expect("a schema of its own, migrated");
            let separator = if url.contains('?') { '&' } else { '?' };
            let owner = format!("{url}{separator}options=-csearch_path%3D{schema}");
            let mut client =
                postgres::Client::connect(&owner, postgres::NoTls).expect("the owner connects");
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                case(&store, &mut client);
            }));
            drop(client);
            drop(store);
            let _ = postgres::Client::connect(&url, postgres::NoTls).and_then(|mut client| {
                client.batch_execute(&format!("drop schema {schema} cascade"))
            });
            if let Err(panic) = result {
                std::panic::resume_unwind(panic);
            }
        }

        /// A control record `hours` before now, as the store holds it; its
        /// event id.
        fn record(client: &mut postgres::Client, hours: i64, record: &str) -> i64 {
            let record: Value = serde_json::from_str(record).expect("a record");
            client
                .query_one(
                    "insert into spawn_events (agent_id, session_id, execution_id, source,
                                               source_event_id, kind, session_seq, recorded_at_ms)
                     select 'agent_control', 'session_control', null, 'control', null,
                            jsonb_build_object('name', 'control.recorded',
                                               'detail', jsonb_build_object('record', $1::jsonb)),
                            coalesce((select max(session_seq) from spawn_events
                                       where session_id = 'session_control'), 0) + 1,
                            spawn_now_ms() - $2::bigint
                     returning id",
                    &[&record, &(hours * HOUR)],
                )
                .expect("a control record")
                .get(0)
        }

        fn refold(client: &mut postgres::Client) {
            client
                .batch_execute("select spawn_control_refold()")
                .expect("refold");
        }

        /// The alert `key`'s state and the event its clear named.
        fn alert(client: &mut postgres::Client, key: &str) -> (String, Option<i64>) {
            client
                .query_opt(
                    "select state, (cleared_evidence ->> 'event_id')::bigint
                       from spawn_alerts where key = $1",
                    &[&key],
                )
                .expect("the alert reads")
                .map_or(("none".to_string(), None), |row| (row.get(0), row.get(1)))
        }

        fn count(client: &mut postgres::Client) -> i64 {
            client
                .query_one(
                    "select count(*) from spawn_alerts
                      where kind in ('mission_halted', 'backup_stale', 'drill_failed')",
                    &[],
                )
                .expect("count")
                .get(0)
        }

        /// A loop's step, started fresh, after one check.
        fn started(store: &PostgresStore) -> Monitor {
            let mut monitor = Monitor::with(settings());
            monitor.check(store).expect("the step checks");
            monitor
        }

        /// Objective 053: a step started on a run of rejections (a restart)
        /// knows the run's objective from the store, so the cancel of its
        /// task fires the notice the store's fold agrees with.
        #[test]
        fn a_restarted_step_knows_the_runs_objective_and_its_cancel() {
            in_schema("alerts_run_cancel", |store, client| {
                record(
                    client,
                    3,
                    r#"{"type":"mission.added","id":1,"text":"m","origin":"human","sources":[]}"#,
                );
                record(
                    client,
                    3,
                    r#"{"type":"task.assigned","id":5,"mission_id":1,"kind":"ephemeral",
                        "agent_id":"agent_w","session_id":"session_w","brief":"b",
                        "created_by":"spawn-integrate","objective":5}"#,
                );
                for _ in 0..2 {
                    record(
                        client,
                        2,
                        r#"{"type":"task.gated","id":5,"verdict":"rejected"}"#,
                    );
                }
                refold(client);
                let mut monitor = started(store);
                let cancelled = record(client, 0, r#"{"type":"task.cancelled","id":5}"#);
                refold(client);
                monitor.check(store).expect("the step checks");
                let (text, folded): (String, i32) = client
                    .query_one(
                        "select a.text, m.gate_rejections
                           from spawn_alerts a, mission_missions m
                          where a.key = $1 and m.id = 1",
                        &[&format!("mission_state:1:{cancelled}")],
                    )
                    .map(|row| (row.get(0), row.get(1)))
                    .expect("the cancel's notice");
                assert_eq!(
                    text,
                    "mission #1: gate_rejections 2 -> 0 (by task.cancelled)"
                );
                assert_eq!(folded, 0, "the store's count");
            });
        }

        #[test]
        fn an_older_record_clears_nothing_at_a_first_start_or_after_a_restart() {
            in_schema("alerts_restarts", |store, client| {
                record(
                    client,
                    5,
                    r#"{"type":"mission.added","id":1,"text":"m","origin":"human","sources":[]}"#,
                );
                let d1 = record(client, 5, r#"{"type":"backup.dumped","keys":["d1"]}"#);
                let v1 = record(
                    client,
                    4,
                    r#"{"type":"backup.verified","ok":true,"key":"v1"}"#,
                );
                let h1 = record(
                    client,
                    3,
                    r#"{"type":"mission.halted","id":1,"reason":"first"}"#,
                );
                let d2 = record(client, 3, r#"{"type":"backup.dumped","keys":["d2"]}"#);
                let r1 = record(client, 2, r#"{"type":"mission.resumed","id":1}"#);
                let h2 = record(
                    client,
                    1,
                    r#"{"type":"mission.halted","id":1,"reason":"second"}"#,
                );
                let v2 = record(
                    client,
                    1,
                    r#"{"type":"backup.verified","ok":false,"key":"v2","reason":"bad"}"#,
                );
                refold(client);
                assert!(d1 < d2 && v1 < v2 && h1 < r1 && r1 < h2);
                let halt = format!("mission_halted:1:{h2}");
                let stale = format!("backup_stale:dump:{d2}");
                let drill = format!("drill_failed:{v2}");

                // The first start: the second halt, the newer dump and the
                // failed drill are open; the first halt is cleared on its
                // resume, which the same history holds.
                let mut monitor = started(store);
                for key in [&halt, &stale, &drill] {
                    assert_eq!(alert(client, key), ("open".to_string(), None), "{key}");
                }
                assert_eq!(
                    alert(client, &format!("mission_halted:1:{h1}")),
                    ("cleared".to_string(), Some(r1)),
                    "fired and cleared naming its resume"
                );

                // Restarts: the older resume, dump and drill clear nothing.
                for _ in 0..2 {
                    monitor = started(store);
                    for key in [&halt, &stale, &drill] {
                        assert_eq!(alert(client, key).0, "open", "{key} after a restart");
                    }
                }

                // Later evidence clears each, naming it.
                let r2 = record(client, 0, r#"{"type":"mission.resumed","id":1}"#);
                let d3 = record(client, 0, r#"{"type":"backup.dumped","keys":["d3"]}"#);
                let v3 = record(
                    client,
                    0,
                    r#"{"type":"backup.verified","ok":true,"key":"v3"}"#,
                );
                refold(client);
                monitor.check(store).expect("the step checks");
                assert_eq!(alert(client, &halt), ("cleared".to_string(), Some(r2)));
                assert_eq!(alert(client, &stale), ("cleared".to_string(), Some(d3)));
                assert_eq!(alert(client, &drill), ("cleared".to_string(), Some(v3)));

                // A further restart fires nothing again.
                let before = count(client);
                drop(started(store));
                assert_eq!(count(client), before);

                // A halt and its resume, and a failed drill and a good one,
                // written while the loop was down: neither opens.
                let h3 = record(
                    client,
                    0,
                    r#"{"type":"mission.halted","id":1,"reason":"third"}"#,
                );
                let r3 = record(client, 0, r#"{"type":"mission.resumed","id":1}"#);
                let v4 = record(
                    client,
                    0,
                    r#"{"type":"backup.verified","ok":false,"key":"v4","reason":"bad"}"#,
                );
                let v5 = record(
                    client,
                    0,
                    r#"{"type":"backup.verified","ok":true,"key":"v5"}"#,
                );
                refold(client);
                drop(started(store));
                assert_eq!(
                    alert(client, &format!("mission_halted:1:{h3}")),
                    ("cleared".to_string(), Some(r3))
                );
                assert_eq!(
                    alert(client, &format!("drill_failed:{v4}")),
                    ("cleared".to_string(), Some(v5))
                );
            });
        }
    }

    mod through_the_store {
        use super::super::{Event, Monitor};
        use super::{commit, decided, fires, monitor};
        use serde_json::{Value, json};
        use spawn_core::command::Outcome;
        use spawn_core::event::{
            ENGINE, EndCause, EventKind, ModelProfile, NewEvent, SessionSeq, Source,
        };
        use spawn_core::id::{AgentId, CommandId, ExecutionId, SessionId, WorkspaceId};
        use spawn_core::observation::{Confidence, Observation, SemanticState};
        use spawn_core::port::EventLog;
        use spawn_postgres::PostgresStore;

        /// Run `case` on a migrated schema of its own, dropped afterwards.
        fn in_schema(name: &str, case: impl FnOnce(&PostgresStore)) {
            let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
                "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
            );
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.subsec_nanos());
            let schema = format!("spawn_test_{}_{nanos:x}_{name}", std::process::id());
            let store = PostgresStore::connect_in_schema(&url, &schema)
                .expect("a schema of its own, migrated");
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| case(&store)));
            drop(store);
            let _ = postgres::Client::connect(&url, postgres::NoTls).and_then(|mut client| {
                client.batch_execute(&format!("drop schema {schema} cascade"))
            });
            if let Err(panic) = result {
                std::panic::resume_unwind(panic);
            }
        }

        /// One worker session's records, appended in order; each start
        /// begins an execution of its own (`exec_<name>_<n>`), as the
        /// Bridge's do.
        struct Session {
            name: &'static str,
            next: u64,
            execution: u32,
        }

        impl Session {
            fn new(name: &'static str) -> Self {
                Self {
                    name,
                    next: 0,
                    execution: 0,
                }
            }

            fn append(&mut self, store: &PostgresStore, source: Source, kind: EventKind) -> i64 {
                let execution = self.execution;
                self.append_to(store, execution, source, kind)
            }

            /// A record of execution `n` of this session.
            fn append_to(
                &mut self,
                store: &PostgresStore,
                n: u32,
                source: Source,
                kind: EventKind,
            ) -> i64 {
                self.next += 1;
                let execution = ExecutionId::new(format!("exec_{}_{n}", self.name)).expect("valid");
                let appended = store
                    .append(NewEvent {
                        agent_id: AgentId::new(format!("agent_{}", self.name)).expect("valid"),
                        session_id: SessionId::new(format!("session_{}", self.name))
                            .expect("valid"),
                        execution_id: Some(execution),
                        source,
                        source_event_id: Some(format!("{}:{}", self.name, self.next)),
                        kind,
                    })
                    .expect("append succeeds");
                i64::try_from(appended.record.position.0).expect("an id")
            }

            fn started(&mut self, store: &PostgresStore) -> i64 {
                self.execution += 1;
                self.append(
                    store,
                    Source::Bridge,
                    EventKind::ExecutionStarted {
                        engine: ENGINE.to_string(),
                        workspace: WorkspaceId::new("1:/workspaces/1").expect("valid"),
                        command_id: CommandId(1),
                        fork: None,
                        model: ModelProfile::Default,
                        transcript_from: None,
                    },
                )
            }

            fn ended(&mut self, store: &PostgresStore, cause: EndCause) -> i64 {
                self.append(
                    store,
                    Source::Bridge,
                    EventKind::ExecutionEnded {
                        cause,
                        exit_code: None,
                        signal: None,
                        evidence: None,
                    },
                )
            }

            /// A judgement: the model judge's with a `stuck` score, or the
            /// rules' (no `stuck`), as the Observer records after an end.
            fn judged(&mut self, store: &PostgresStore, stuck: Option<f64>) -> i64 {
                let mut observation = Observation::basic(
                    if stuck.is_some() {
                        SemanticState::Unknown
                    } else {
                        SemanticState::Completed
                    },
                    None,
                    SessionSeq(self.next),
                    Confidence::new(1.0).expect("valid confidence"),
                    if stuck.is_some() { "jev" } else { "rules" },
                );
                observation.stuck = stuck;
                self.append(
                    store,
                    Source::Observer,
                    EventKind::ObservationRecorded { observation },
                )
            }
        }

        fn observe(monitor: &mut Monitor, store: &PostgresStore) -> Vec<Event> {
            let observations: Value = store.alert_observations().expect("the judgements");
            let mut out = Vec::new();
            monitor.observe(&observations, &mut out);
            decided(monitor, out)
        }

        fn cleared_by(events: &[Event]) -> Vec<(String, Value)> {
            events
                .iter()
                .filter_map(|event| match event {
                    Event::Clear { key, evidence } => Some((key.clone(), evidence.clone())),
                    Event::Fire { .. } => None,
                })
                .collect()
        }

        /// 106's shape on the live log: stuck, stopped, then the rules'
        /// COMPLETED judgement with no `stuck` after the end. The end is the
        /// session's current state, so it clears, with the end's id; and a
        /// lost execution clears the same way, its cause named.
        #[test]
        fn a_stuck_alert_clears_at_its_executions_end_whatever_judgement_follows() {
            in_schema("stuck_end", |store| {
                let mut monitor = monitor();
                let mut stopped = Session::new("worker_stop");
                let mut lost = Session::new("worker_lost");
                stopped.started(store);
                lost.started(store);
                let stuck = stopped.judged(store, Some(0.64));
                let stuck_lost = lost.judged(store, Some(0.7));
                let fired = observe(&mut monitor, store);
                assert_eq!(
                    fires(&fired),
                    [
                        format!("agent_stuck:session_worker_lost:{stuck_lost}"),
                        format!("agent_stuck:session_worker_stop:{stuck}"),
                    ]
                );
                commit(&mut monitor, &fired);

                let end = stopped.ended(store, EndCause::Stopped);
                let end_lost = lost.ended(store, EndCause::Lost);
                stopped.judged(store, None);
                lost.judged(store, None);
                let cleared = observe(&mut monitor, store);
                assert_eq!(
                    cleared_by(&cleared),
                    [
                        (
                            format!("agent_stuck:session_worker_lost:{stuck_lost}"),
                            json!({"event_id": end_lost, "cause": "lost"})
                        ),
                        (
                            format!("agent_stuck:session_worker_stop:{stuck}"),
                            json!({"event_id": end, "cause": "stopped"})
                        ),
                    ]
                );
                commit(&mut monitor, &cleared);
                assert!(observe(&mut monitor, store).is_empty(), "cleared once");
            });
        }

        /// An end and then a start (a resume, as the supervisor's lost
        /// execution and the next): the end is not current, so a stuck
        /// alert of the new execution stays open, and an unscored judgement
        /// changes nothing.
        #[test]
        fn an_end_followed_by_a_start_clears_nothing() {
            in_schema("stuck_resume", |store| {
                let mut monitor = monitor();
                let mut session = Session::new("supervisor");
                session.started(store);
                session.ended(store, EndCause::Lost);
                session.started(store);
                let stuck = session.judged(store, Some(0.52));
                let fired = observe(&mut monitor, store);
                assert_eq!(
                    fires(&fired),
                    [format!("agent_stuck:session_supervisor:{stuck}")]
                );
                commit(&mut monitor, &fired);
                session.judged(store, None);
                assert!(observe(&mut monitor, store).is_empty(), "nothing clears");
            });
        }

        /// A runner's late report of the older execution's end, recorded
        /// after the resume's start (only old logs hold one: bridge.rs), is
        /// no end: the store counts the Bridge's alone (schema.sql's "a
        /// runner's exit report is no end"). The live execution's stuck
        /// judgement fires, and nothing clears it. rev-f's probe on
        /// 4897d444, which read that report as the current end.
        #[test]
        fn a_runners_late_end_report_of_an_older_execution_is_no_end() {
            in_schema("stuck_late_report", |store| {
                let mut monitor = monitor();
                let mut session = Session::new("late");
                session.started(store);
                session.ended(store, EndCause::Lost);
                session.started(store);
                session.append_to(
                    store,
                    1,
                    Source::Claude,
                    EventKind::ExecutionEnded {
                        cause: EndCause::Exited,
                        exit_code: Some(0),
                        signal: None,
                        evidence: None,
                    },
                );
                let stuck = session.judged(store, Some(0.8));
                let fired = observe(&mut monitor, store);
                assert_eq!(fires(&fired), [format!("agent_stuck:session_late:{stuck}")]);
                commit(&mut monitor, &fired);
                session.judged(store, None);
                assert!(observe(&mut monitor, store).is_empty(), "nothing clears it");
            });
        }

        /// The model judge's `down` record (crate judge_watch.rs), read back
        /// through the store's history as a restarted loop reads it: no
        /// alert before SPAWN_ALERT_JUDGE_DOWN_SECS after its first failed
        /// call (30 s before the record), `judge_down` at it, once, and its
        /// `up` record, read as the next pass reads the control stream,
        /// clears it with `failed_calls`. A loop whose history no longer
        /// holds the `down` (restarted a week into the outage) clears the
        /// open alert at the `up` all the same.
        #[test]
        fn a_judge_down_for_its_bound_fires_once_and_its_up_clears_it() {
            in_schema("judge_down", |store| {
                let mut monitor = monitor();
                let first = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(0))
                    - 30_000;
                let down = store
                    .host_record(&json!({"type": "host.judge", "judge": "jev",
                                         "state": "down", "error": "judge unavailable: 502",
                                         "first_failed_ms": first}))
                    .expect("record down");
                let history = store.alert_history(0, 100).expect("history");
                let mut fed = Vec::new();
                for entry in history.as_array().into_iter().flatten() {
                    monitor.feed(
                        entry["event_id"].as_i64().unwrap_or_default(),
                        entry["at_ms"].as_i64().unwrap_or_default(),
                        &entry["record"],
                        &mut fed,
                    );
                }
                assert!(fed.is_empty(), "a down is not yet an alert");
                assert!(history[0]["at_ms"].as_i64().is_some_and(|at| at > first));
                let at = first;
                let mut early = Vec::new();
                monitor.bounds(at + 899_999, &mut early);
                assert!(early.is_empty(), "never before its bound");
                let mut due = Vec::new();
                monitor.bounds(at + 900_000, &mut due);
                let key = format!("judge_down:{down}");
                assert_eq!(fires(&due), [key.as_str()]);
                let Event::Fire {
                    subject,
                    evidence,
                    text,
                    ..
                } = &due[0]
                else {
                    panic!("a fire");
                };
                assert_eq!(subject, "jev");
                assert_eq!(evidence["down_ms"], 900_000);
                assert_eq!(evidence["error"], "judge unavailable: 502");
                assert!(
                    text.contains("failed two calls in a row, the first 900 s ago"),
                    "{text}"
                );
                let recorded = decided(&monitor, due);
                commit(&mut monitor, &recorded);
                let mut restarted = super::monitor();
                commit(&mut restarted, &recorded);
                let mut again = Vec::new();
                monitor.bounds(at + 1_800_000, &mut again);
                assert!(decided(&monitor, again).is_empty(), "once per occurrence");

                let up = store
                    .host_record(&json!({"type": "host.judge", "judge": "jev",
                                         "state": "up", "failed_calls": 41}))
                    .expect("record up");
                let batch = store.alert_control(down, 100).expect("the control stream");
                let mut cleared = Vec::new();
                for entry in batch["records"].as_array().into_iter().flatten() {
                    monitor.feed(
                        entry["event_id"].as_i64().unwrap_or_default(),
                        entry["at_ms"].as_i64().unwrap_or_default(),
                        &entry["record"],
                        &mut cleared,
                    );
                }
                let cleared = cleared_by(&decided(&monitor, cleared));
                assert_eq!(cleared.len(), 1, "{cleared:?}");
                assert_eq!(cleared[0].0, key);
                assert_eq!(cleared[0].1["event_id"], up);
                assert_eq!(cleared[0].1["failed_calls"], 41);
                assert!(
                    cleared[0].1["down_ms"]
                        .as_i64()
                        .is_some_and(|ms| ms >= 30_000)
                );

                let mut orphan = Vec::new();
                for entry in batch["records"].as_array().into_iter().flatten() {
                    restarted.feed(
                        entry["event_id"].as_i64().unwrap_or_default(),
                        entry["at_ms"].as_i64().unwrap_or_default(),
                        &entry["record"],
                        &mut orphan,
                    );
                }
                let orphan = cleared_by(&decided(&restarted, orphan));
                assert_eq!(orphan.len(), 1, "the up with no down known: {orphan:?}");
                assert_eq!(orphan[0].0, key);
                assert_eq!(orphan[0].1["event_id"], up);
            });
        }

        /// An open agent_stuck says when its session was last scored, read
        /// with the window: an unscored judgement after it (a judge that is
        /// down) leaves that time where it was.
        #[test]
        fn an_open_stuck_alert_carries_the_time_of_its_sessions_last_score() {
            in_schema("stuck_scored_at", |store| {
                let mut session = Session::new("scored");
                session.started(store);
                let stuck = session.judged(store, Some(0.7));
                let scored_at = store.alert_observations().expect("the judgements")[0]["at_ms"]
                    .as_i64()
                    .expect("its time");
                store
                    .alert_record(&json!({
                        "type": "alert.fired", "key": format!("agent_stuck:session_scored:{stuck}"),
                        "kind": "agent_stuck", "subject": "session_scored",
                        "evidence": {"stuck": 0.7, "event_id": stuck},
                        "text": "session_scored is stuck (0.70): UNKNOWN", "notice": false,
                    }))
                    .expect("fire");
                std::thread::sleep(std::time::Duration::from_millis(5));
                session.judged(store, None);
                let window = store.alerts(0).expect("the window");
                assert_eq!(window[0]["scored_at_ms"], scored_at, "{window}");
                assert!(super::super::line(&window[0]).contains("(last scored "));
            });
        }

        /// A loop that starts after the session ended, its latest scored
        /// judgement stuck: nothing fires. And an alert already open on a
        /// judgement made after the end (one fired before this rule) clears
        /// at that end.
        #[test]
        fn an_ended_session_fires_nothing_and_its_open_alert_clears() {
            in_schema("stuck_ended", |store| {
                let mut session = Session::new("worker_done");
                session.started(store);
                let end = session.ended(store, EndCause::Exited);
                let stuck = session.judged(store, Some(0.9));
                let mut fresh = monitor();
                assert!(
                    observe(&mut fresh, store).is_empty(),
                    "an ended session fires nothing"
                );

                let key = format!("agent_stuck:session_worker_done:{stuck}");
                let mut monitor = monitor();
                commit(
                    &mut monitor,
                    &[Event::Fire {
                        key: key.clone(),
                        kind: "agent_stuck",
                        subject: "session_worker_done".to_string(),
                        evidence: json!({}),
                        text: String::new(),
                        notice: false,
                    }],
                );
                assert_eq!(
                    cleared_by(&observe(&mut monitor, store)),
                    [(key, json!({"event_id": end, "cause": "exited"}))]
                );
            });
        }

        /// One record with the key its producer gives it.
        fn keyed(
            store: &PostgresStore,
            session: &str,
            execution: &str,
            source: Source,
            key: &str,
            kind: EventKind,
        ) -> i64 {
            let appended = store
                .append(NewEvent {
                    agent_id: AgentId::new(format!("agent_{session}")).expect("valid"),
                    session_id: SessionId::new(format!("session_{session}")).expect("valid"),
                    execution_id: Some(ExecutionId::new(execution.to_string()).expect("valid")),
                    source,
                    source_event_id: Some(key.to_string()),
                    kind,
                })
                .expect("append succeeds");
            i64::try_from(appended.record.position.0).expect("an id")
        }

        /// A person's input into `session` (`key` its idempotency key).
        fn input(store: &PostgresStore, session: &str, key: &str, text: &str) -> CommandId {
            use spawn_core::command::{CommandAction, Submission};
            use spawn_core::id::{IdempotencyKey, Principal};
            use spawn_core::port::CommandQueue;
            store
                .submit(
                    &Principal::new("human").expect("principal"),
                    &Submission {
                        agent_id: AgentId::new(format!("agent_{session}")).expect("agent"),
                        session_id: SessionId::new(format!("session_{session}")).expect("session"),
                        idempotency_key: IdempotencyKey::new(key).expect("key"),
                        action: CommandAction::Input {
                            text: text.to_string(),
                            tags: Vec::new(),
                        },
                    },
                )
                .expect("submit")
        }

        /// The Bridge's records of typing `command` into `execution`:
        /// `message.typed` when the text carried the marker, then `typing`
        /// and `typed`.
        fn typed(
            store: &PostgresStore,
            session: &str,
            execution: &str,
            command: CommandId,
            marked: bool,
        ) {
            use spawn_core::event::CommandStep;
            let id = command.0;
            if marked {
                keyed(
                    store,
                    session,
                    execution,
                    Source::Bridge,
                    &format!("cmd_{id}:message"),
                    EventKind::MessageTyped {
                        command_id: command,
                    },
                );
            }
            for (step, name) in [
                (CommandStep::Typing, "typing"),
                (CommandStep::Typed, "typed"),
            ] {
                keyed(
                    store,
                    session,
                    execution,
                    Source::Bridge,
                    &format!("cmd_{id}:{name}"),
                    EventKind::CommandStepped {
                        command_id: command,
                        step,
                        workspace: None,
                    },
                );
            }
        }

        /// The transcript's line `line` took `command`.
        fn taken(store: &PostgresStore, session: &str, execution: &str, line: &str, id: u64) {
            keyed(
                store,
                session,
                execution,
                Source::Claude,
                &format!("{line}:taken:{id}"),
                EventKind::MessageTaken {
                    command_id: CommandId(id),
                },
            );
        }

        fn turn_ended(store: &PostgresStore, session: &str, execution: &str, line: &str) -> i64 {
            keyed(
                store,
                session,
                execution,
                Source::Claude,
                line,
                EventKind::TurnStopped {
                    input_tokens: None,
                    output_tokens: None,
                },
            )
        }

        /// A monitor whose messages are due at once.
        fn prompt_monitor() -> Monitor {
            let mut settings = super::settings();
            settings.message_taken_ms = 0;
            Monitor::with(settings)
        }

        /// The step's own reading of the messages: the commands after the
        /// last it saw, its pending messages and its open alerts'.
        fn read(monitor: &mut Monitor, store: &PostgresStore) -> Vec<Event> {
            let mut out = Vec::new();
            let mut lines = Vec::new();
            let now = store.now_ms().expect("the clock");
            monitor.read_messages(store, now, &mut out, &mut lines);
            assert!(lines.is_empty(), "{lines:?}");
            decided(monitor, out)
        }

        /// The commands of one session, as an older Spawn and this one left
        /// them: `old`, typed with no marker before any loop typed markers;
        /// `answered`, taken twice and answered after its first taking;
        /// `deaf`, typed and never taken; `untyped`, never typed. All four
        /// carry the same text.
        struct Messages {
            worker: Session,
            old: CommandId,
            answered: CommandId,
            deaf: CommandId,
            untyped: CommandId,
            refused: CommandId,
        }

        const MSG_EXECUTION: &str = "exec_msg_1";

        fn messages_world(store: &PostgresStore) -> Messages {
            let session = "msg";
            let mut worker = Session::new(session);
            let old = input(store, session, "old", "same text");
            worker.started(store);
            typed(store, session, MSG_EXECUTION, old, false);
            store
                .host_record(&json!({"type": "host.messages_marked", "started_at_ms": 1}))
                .expect("the first marking loop");
            finish(store, old, &Outcome::Succeeded);
            // The first message the transcript holds: from then on the
            // session's messages are tracked.
            let answered = input(store, session, "answered", "same text");
            typed(store, session, MSG_EXECUTION, answered, true);
            taken(store, session, MSG_EXECUTION, "u1", answered.0);
            taken(store, session, MSG_EXECUTION, "u1-again", answered.0);
            turn_ended(store, session, MSG_EXECUTION, "t1");
            finish(store, answered, &Outcome::Succeeded);
            // One the Bridge refused before typing it, which a loop sends
            // again as a new command.
            let refused = input(store, session, "refused", "same text");
            finish(
                store,
                refused,
                &Outcome::Failed("pane state Busy does not accept input".to_string()),
            );
            let deaf = input(store, session, "deaf", "same text");
            let untyped = input(store, session, "untyped", "same text");
            typed(store, session, MSG_EXECUTION, deaf, true);
            Messages {
                worker,
                old,
                answered,
                deaf,
                untyped,
                refused,
            }
        }

        /// Claim `command` (the oldest unfinished one) and finish it.
        fn finish(store: &PostgresStore, command: CommandId, outcome: &Outcome) {
            use spawn_core::port::CommandQueue;
            let claimed = store
                .claim("tester", std::time::Duration::from_mins(1))
                .expect("claim")
                .expect("a command");
            assert_eq!(claimed.command_id, command);
            store.finish(&claimed, outcome).expect("finish");
        }

        /// Each command's message read by its id from the records, by the
        /// commands API: answered and taken twice; typed and not taken; never
        /// typed and not taken; an older Spawn's, typed and never not taken.
        /// The first marking loop's record is written once.
        #[test]
        fn each_command_reads_its_message_by_its_id() {
            in_schema("messages_read", |store| {
                let world = messages_world(store);
                let again = store
                    .host_record(&json!({"type": "host.messages_marked", "started_at_ms": 2}))
                    .expect("a later loop");
                let first = store
                    .host_record(&json!({"type": "host.messages_marked", "started_at_ms": 3}))
                    .expect("and another");
                assert_eq!(first, again, "the first record stands");
                let commands = store
                    .commands_json("session_msg", 50, 0)
                    .expect("the commands");
                let message = |id: CommandId| {
                    commands
                        .as_array()
                        .expect("an array")
                        .iter()
                        .find(|command| command["command_id"] == json!(id.0))
                        .expect("the command")["message"]
                        .clone()
                };
                let answer = message(world.answered);
                assert_eq!(answer["state"], "answered", "{answer}");
                assert_eq!(answer["taken"], 2);
                assert_eq!(answer["not_taken"], false);
                assert!(answer["typed_at_ms"].as_i64() <= answer["taken_at_ms"].as_i64());
                assert!(answer["taken_at_ms"].as_i64() <= answer["answered_at_ms"].as_i64());
                let mut names: Vec<String> = answer
                    .as_object()
                    .map(|fields| fields.keys().cloned().collect())
                    .unwrap_or_default();
                names.sort_unstable();
                assert_eq!(
                    names,
                    [
                        "acted",
                        "acted_at_ms",
                        "answered_at_ms",
                        "not_taken",
                        "state",
                        "taken",
                        "taken_at_ms",
                        "typed_at_ms"
                    ]
                );
                assert_eq!(answer["acted"], Value::Null, "never acked: {answer}");
                let deaf = message(world.deaf);
                assert_eq!(
                    (&deaf["state"], &deaf["not_taken"], &deaf["taken"]),
                    (&json!("typed"), &json!(true), &json!(0)),
                    "{deaf}"
                );
                assert_eq!(deaf["answered_at_ms"], Value::Null);
                let never = message(world.untyped);
                assert_eq!(
                    (&never["state"], &never["not_taken"], &never["typed_at_ms"]),
                    (&json!("recorded"), &json!(true), &Value::Null),
                    "{never}"
                );
                let refused = message(world.refused);
                assert_eq!(
                    (&refused["state"], &refused["not_taken"]),
                    (&json!("recorded"), &json!(false)),
                    "one refused before typing is no message not taken: {refused}"
                );
                let older = message(world.old);
                assert_eq!(
                    (&older["state"], &older["not_taken"]),
                    (&json!("typed"), &json!(false)),
                    "an older Spawn's input is never not taken: {older}"
                );
                // Within its bound, none is not taken yet.
                let patient = store
                    .commands_json("session_msg", 50, 3_600_000)
                    .expect("the commands");
                assert!(
                    patient
                        .as_array()
                        .into_iter()
                        .flatten()
                        .all(|command| command["message"]["not_taken"] == false),
                    "{patient}"
                );
            });
        }

        /// The alert fires for the two not taken (whether typed in its
        /// evidence), once; the one taken later clears, and the session's
        /// end clears the other; a fresh step then fires nothing.
        #[test]
        fn a_message_not_taken_is_one_alert_until_taken_or_its_session_ends() {
            in_schema("messages_alert", |store| {
                let mut world = messages_world(store);
                let (deaf, untyped) = (world.deaf, world.untyped);
                let mut monitor = prompt_monitor();
                let fired = read(&mut monitor, store);
                assert_eq!(
                    fires(&fired),
                    [
                        format!("message_not_taken:{}", deaf.0),
                        format!("message_not_taken:{}", untyped.0)
                    ]
                );
                let evidence = |id: CommandId| {
                    fired
                        .iter()
                        .find_map(|event| match event {
                            Event::Fire {
                                key,
                                evidence,
                                subject,
                                ..
                            } if *key == format!("message_not_taken:{}", id.0) => {
                                Some((subject.clone(), evidence.clone()))
                            }
                            _ => None,
                        })
                        .expect("its alert")
                };
                let (subject, deaf_evidence) = evidence(deaf);
                assert_eq!(subject, "session_msg");
                assert_eq!(deaf_evidence["command"], json!(deaf.0));
                assert_eq!(deaf_evidence["agent_id"], "agent_msg");
                assert_eq!(deaf_evidence["typed"], true);
                assert_eq!(evidence(untyped).1["typed"], false);
                commit(&mut monitor, &fired);
                assert!(read(&mut monitor, store).is_empty(), "once");

                // The untyped one is typed and taken: its alert clears.
                typed(store, "msg", MSG_EXECUTION, untyped, true);
                taken(store, "msg", MSG_EXECUTION, "u2", untyped.0);
                let cleared = read(&mut monitor, store);
                assert!(
                    matches!(&cleared[..], [Event::Clear { key, evidence }]
                        if *key == format!("message_not_taken:{}", untyped.0)
                            && evidence["taken"] == 1),
                    "{cleared:?}"
                );
                commit(&mut monitor, &cleared);

                // The session ends with the deaf one untaken: it clears.
                world.worker.ended(store, EndCause::Stopped);
                let ended = read(&mut monitor, store);
                assert!(
                    matches!(&ended[..], [Event::Clear { key, evidence }]
                        if *key == format!("message_not_taken:{}", deaf.0)
                            && evidence["ended"] == true),
                    "{ended:?}"
                );
                commit(&mut monitor, &ended);
                assert!(read(&mut monitor, store).is_empty());
                // A fresh step finds nothing to fire for an ended session.
                assert!(read(&mut prompt_monitor(), store).is_empty());
            });
        }

        /// The loop's own pass, twice, the second a restarted loop (a
        /// session whose transcript held no marker yet tracks nothing): the
        /// first records the alert of a message never taken, the restarted
        /// one reads it back on start, fires nothing again, and clears it
        /// when the message is taken.
        #[test]
        fn a_restarted_loop_reads_its_message_alert_back_and_clears_it_once() {
            in_schema("messages_restart", |store| {
                let session = "msg_restart";
                let mut worker = Session::new(session);
                worker.started(store);
                let execution = "exec_msg_restart_1";
                store
                    .host_record(&json!({"type": "host.messages_marked", "started_at_ms": 1}))
                    .expect("the first marking loop");
                // Its task, typed: until the transcript holds a marker, the
                // session's silence says nothing.
                let task = input(store, session, "task", "hello");
                typed(store, session, execution, task, true);
                let silent = store
                    .commands_json(&format!("session_{session}"), 10, 0)
                    .expect("the commands");
                assert!(
                    silent
                        .as_array()
                        .into_iter()
                        .flatten()
                        .all(|command| command["message"]["not_taken"] == false),
                    "{silent}"
                );
                taken(store, session, execution, "u0", task.0);
                // The next input is tracked, and the engine never takes it.
                let deaf = input(store, session, "deaf", "hello");
                typed(store, session, execution, deaf, true);
                let key = format!("message_not_taken:{}", deaf.0);
                let rows = |store: &PostgresStore| {
                    store.alert_rows().expect("the alerts")["rows"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|row| row["key"] == json!(key))
                        .map(|row| row["state"].as_str().unwrap_or_default().to_string())
                        .collect::<Vec<_>>()
                };

                let mut first = prompt_monitor();
                first.check(store).expect("a pass");
                assert_eq!(rows(store), ["open"]);

                let mut restarted = prompt_monitor();
                restarted.check(store).expect("a restarted pass");
                assert!(restarted.is_open(&key), "read back on start");
                assert_eq!(rows(store), ["open"], "not fired again");

                taken(store, session, execution, "u1", deaf.0);
                restarted.next_reading_ms = 0;
                restarted.check(store).expect("a pass");
                assert_eq!(rows(store), ["cleared"]);
                restarted.next_reading_ms = 0;
                restarted.check(store).expect("a pass");
                assert_eq!(rows(store), ["cleared"], "cleared once");
            });
        }
    }

    #[test]
    fn a_model_login_that_expired_clears_when_its_execution_does() {
        let mut monitor = monitor();
        let interrupted = feed(
            &mut monitor,
            70,
            0,
            json!({"type": "execution.interrupted", "agent_id": "agent_worker_a",
                   "session_id": "session_worker_a", "execution_id": "exec_auth",
                   "cause": "model_auth",
                   "evidence": {"error": "authentication_failed", "status": 401}}),
        );
        assert_eq!(fires(&interrupted), ["model_auth:70"]);
        let Event::Fire {
            subject, evidence, ..
        } = &interrupted[0]
        else {
            panic!("a fire");
        };
        assert_eq!(subject, "session_worker_a");
        assert_eq!(evidence["execution_id"], "exec_auth");
        let recorded = decided(&monitor, interrupted);
        commit(&mut monitor, &recorded);

        let cleared = feed(
            &mut monitor,
            71,
            0,
            json!({"type": "execution.cleared", "execution_id": "exec_auth",
                   "session_id": "session_worker_a", "waited_ms": 9000}),
        );
        let cleared = decided(&monitor, cleared);
        assert!(matches!(&cleared[0], Event::Clear { key, evidence }
            if key == "model_auth:70" && evidence["waited_ms"] == 9000));
    }

    #[test]
    fn evidence_never_holds_a_url_password() {
        let secret = "postgres://spawn:pa55w0rd@db:5432/spawn_test";
        let scrubbed = scrub(&format!("cannot connect to {secret}: refused"));
        assert!(!scrubbed.contains("pa55w0rd"), "{scrubbed}");
        assert!(scrubbed.contains("postgres://spawn:***@db:5432/spawn_test"));
        let value = scrub_value(&json!({"error": format!("{secret} refused"), "n": 3}));
        assert!(!value.to_string().contains("pa55w0rd"), "{value}");
        assert_eq!(value["n"], 3);
        // A URL without a password, and plain text, are left as they are.
        assert_eq!(
            scrub("see postgres://db:5432/x"),
            "see postgres://db:5432/x"
        );
        assert_eq!(scrub("no url here"), "no url here");
    }

    #[test]
    fn the_text_form_names_the_kind_and_the_subject_of_each_alert() {
        let alerts = json!([
            {"key": "mission_halted:4:9", "kind": "mission_halted", "subject": "mission #4",
             "state": "open", "fired_at_ms": 1_760_000_000_000_i64,
             "text": "mission #4 halted: three rejections"},
            {"key": "task_state:7:10", "kind": "task_state", "subject": "task #7",
             "state": "notice", "fired_at_ms": 1_760_000_000_000_i64,
             "text": "task #7: none -> assigned (by task.assigned)"},
        ]);
        let lines: Vec<String> = alerts.as_array().unwrap().iter().map(line).collect();
        assert!(lines[0].contains("mission_halted") && lines[0].contains("mission #4"));
        assert!(lines[0].starts_with("open "));
        assert!(lines[1].contains("task_state") && lines[1].contains("task #7"));
        assert!(lines[1].starts_with("notice "));
        // One line per alert, and the same array the API answers: both are
        // `window`'s answer, shaped by the store's one method.
        assert_eq!(lines.len(), alerts.as_array().unwrap().len());
    }

    #[test]
    fn the_window_is_a_span_or_the_configured_default() {
        assert_eq!(window_ms(Some("90s")), Ok(90_000));
        assert_eq!(window_ms(Some("30m")), Ok(1_800_000));
        assert_eq!(window_ms(Some("2h")), Ok(7_200_000));
        assert_eq!(window_ms(Some("1d")), Ok(86_400_000));
        assert_eq!(window_ms(Some("1500")), Ok(1500));
        assert_eq!(parse_span("500ms"), Some(500));
        assert!(window_ms(Some("soon")).is_err());
    }

    #[test]
    fn a_time_is_rendered_in_utc() {
        let (year, month, day, hour, minute, second) = civil(1_760_000_000_000);
        assert_eq!((year, month, day), (2025, 10, 9));
        assert_eq!((hour, minute, second), (8, 53, 20));
        assert_eq!(time_text(0), "1970-01-01 00:00:00");
    }
}
