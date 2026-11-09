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
//! restarted loop fires nothing twice and still knows what is open. A
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
//!   `gate_rejections`, `gate_aborts`, `gate_room_escalated` differs.
//! - `gate_base_disagrees`, `mission #<id>`, key
//!   `gate_base_disagrees:<event>`: a `task.gated` host fault with
//!   `base_disagrees` and `alert` true (objective 033).
//! - `command_failed`, `command #<id>`, key `command_failed:<command>`: a
//!   command that finished failed, an owner's note (`owner-note-…`, which a
//!   person sent) excepted.
//!
//! Conditions:
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
//!   later observation still stuck fires nothing; clears when the latest
//!   one is under 0.5 or its execution ended by a stop.
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

use std::collections::{BTreeMap, HashMap};
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
    /// An open (or pending) model-login alert key, by execution id.
    auth: HashMap<String, String>,
    /// The newest dump and bundle records.
    backups: BTreeMap<&'static str, Backup>,
    commands_after: i64,
    logs: Vec<LogWatch>,
    /// The first reading of the current run that found no integrator
    /// holding its lock while reported work waited (`integrator_down`).
    unlocked_since: Option<i64>,
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
            auth: HashMap::new(),
            backups: BTreeMap::new(),
            commands_after: 0,
            logs,
            unlocked_since: None,
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
        if watermark > self.watermark {
            let batch = store
                .alert_control(self.watermark, BATCH)
                .map_err(|error| error.to_string())?;
            let records = batch["records"].as_array().map_or(&[][..], Vec::as_slice);
            for entry in records {
                let id = entry["event_id"].as_i64().unwrap_or_default();
                let at = entry["at_ms"].as_i64().unwrap_or(now);
                self.feed(id, at, &entry["record"], &mut events);
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
            self.observe(&observations, &mut events);
            let commands = store
                .alert_failed_commands(self.commands_after)
                .map_err(|error| error.to_string())?;
            self.commands(&commands, &mut events);
            self.commands_after = now.saturating_sub(1000);
        }
        if reading_due {
            self.next_reading_ms = now.saturating_add(self.settings.check_ms);
            self.read_host(now, &mut events, &mut lines);
            self.read_integrator(store, now, &mut events, &mut lines);
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
        let history = store
            .alert_history(now.saturating_sub(HISTORY_MS), HISTORY_LIMIT)
            .map_err(|error| error.to_string())?;
        let mut events = Vec::new();
        for entry in history.as_array().into_iter().flatten() {
            let id = entry["event_id"].as_i64().unwrap_or_default();
            let at = entry["at_ms"].as_i64().unwrap_or(now);
            self.feed(id, at, &entry["record"], &mut events);
        }
        let observations = store
            .alert_observations()
            .map_err(|error| error.to_string())?;
        self.observe(&observations, &mut events);
        self.commands_after = now.saturating_sub(COMMAND_LOOKBACK_MS);
        let commands = store
            .alert_failed_commands(self.commands_after)
            .map_err(|error| error.to_string())?;
        self.commands(&commands, &mut events);
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
            _ => {}
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

    /// A gating's mission counters, and 033's base disagreement.
    fn gating(
        &mut self,
        event_id: i64,
        task: i64,
        mission_id: i64,
        record: &Value,
        out: &mut Vec<Event>,
    ) {
        if let Some(mission) = self.missions.get_mut(&mission_id) {
            let before = snapshot(mission);
            apply_gating(mission, record);
            let after = snapshot(mission);
            if before != after {
                out.push(Event::Fire {
                    key: format!("mission_state:{mission_id}:{event_id}"),
                    kind: "mission_state",
                    subject: format!("mission #{mission_id}"),
                    evidence: json!({
                        "mission": mission_id, "record": "task.gated", "event_id": event_id,
                        "before": before, "after": after,
                    }),
                    text: format!(
                        "mission #{mission_id}: {} (by task.gated)",
                        changed(&before, &after)
                    ),
                    notice: true,
                });
            }
        }
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
    /// clears, and so does a stop.
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
            let stopped = observation["ended_cause"].as_str() == Some("stopped");
            if open.is_empty() && stuck.is_some_and(|stuck| stuck >= LIKELY) {
                out.push(Event::Fire {
                    key: format!("agent_stuck:{session}:{event_id}"),
                    kind: "agent_stuck",
                    subject: session.clone(),
                    evidence: json!({
                        "agent_id": observation["agent_id"], "stuck": stuck,
                        "state": judgement["state"], "judge": judgement["judge"],
                        "event_id": event_id,
                    }),
                    text: format!(
                        "{session} is stuck ({:.2}): {}",
                        stuck.unwrap_or_default(),
                        judgement["state"].as_str().unwrap_or("unknown")
                    ),
                    notice: false,
                });
                continue;
            }
            if stopped {
                for key in open {
                    out.push(Event::Clear {
                        key,
                        evidence: json!({
                            "event_id": observation["ended_event"],
                            "execution": "stopped",
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

    /// The bounds that fall due: an undelivered task, an admission's wait, a
    /// backup's age, a crash line gone quiet.
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
                }),
                text: format!("MemAvailable is {measured} MiB, under {low}"),
                notice: false,
            }),
            Crossing::Clear => {
                for key in open {
                    out.push(Event::Clear {
                        key,
                        evidence: json!({"measured_mb": measured, "threshold_mb": ok}),
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
                    "measured_mb": measured, "threshold_mb": high,
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
                            "measured_mb": measured, "threshold_mb": ok,
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
                            "lines": matching,
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
                            "log": path.display().to_string(),
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
                    let Some(row) = self.known.get(&key) else {
                        continue;
                    };
                    if !row.open() || row.fired_event_id == 0 {
                        continue;
                    }
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

/// The fold's counter changes of a gating.
fn apply_gating(mission: &mut Mission, record: &Value) {
    match record["verdict"].as_str().unwrap_or_default() {
        "rejected" => mission.rejections += 1,
        "accepted" => mission.rejections = 0,
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
fn scrub(text: &str) -> String {
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
/// text.
fn line(alert: &Value) -> String {
    let state = alert["state"].as_str().unwrap_or("open");
    let at = alert
        .get("cleared_at_ms")
        .and_then(Value::as_i64)
        .or_else(|| alert["fired_at_ms"].as_i64())
        .unwrap_or_default();
    format!(
        "{state} {} {} {} {}",
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

#[cfg(test)]
mod tests {
    use super::{
        Crossing, Event, LIKELY, Monitor, Row, Settings, civil, crash_stamp, crossing,
        days_from_civil, line, parse_span, scrub, scrub_value, time_text, window_ms,
    };
    use serde_json::{Value, json};
    use std::path::{Path, PathBuf};

    fn settings() -> Settings {
        Settings {
            check_ms: 30_000,
            undelivered_ms: 300_000,
            admission_ms: 600_000,
            dump_max_age_ms: 7_200_000,
            bundle_max_age_ms: 93_600_000,
            memory_low_mb: 4096,
            memory_ok_mb: 8192,
            tmpfs_path: PathBuf::from("/tmp"),
            tmpfs_high_mb: 8000,
            tmpfs_ok_mb: 6000,
            quiet_ms: 900_000,
            integrator_down_ms: 300_000,
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
                Event::Clear { .. } => monitor.is_open(event.key()),
            })
            .collect()
    }

    /// The store's side of a flush: the keys it now holds.
    fn commit(monitor: &mut Monitor, events: &[Event]) {
        for event in events {
            match event {
                Event::Fire {
                    key, kind, subject, ..
                } => {
                    monitor.known.insert(
                        key.clone(),
                        Row {
                            kind: (*kind).to_string(),
                            subject: subject.clone(),
                            state: "open".to_string(),
                            evidence: Value::Null,
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
