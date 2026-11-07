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
//!   `postgres_crash:<subject>:<the line's stamp and [pid] as written>`: a
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
        Some(text) => match text.trim().parse() {
            Ok(value) => value,
            Err(_) => {
                warnings.push(format!(
                    "{name}: {text:?} is not a number of seconds or megabytes; using {default}"
                ));
                default
            }
        },
    }
}

impl Settings {
    pub(crate) fn from_env() -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let secs = |name: &str, default: u64, warnings: &mut Vec<String>| -> i64 {
            number(name, default, warnings).saturating_mul(1000).cast_signed()
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
    #[allow(dead_code)]
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
    said: Vec<&'static str>,
}

impl Monitor {
    /// The step with the environment's settings; the warnings are said once.
    pub(crate) fn from_env() -> (Self, Vec<String>) {
        let (settings, warnings) = Settings::from_env();
        (Self::with(settings), warnings)
    }

    fn with(settings: Settings) -> Self {
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
            said: Vec::new(),
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
            for entry in batch["records"].as_array().into_iter().flatten() {
                let id = entry["event_id"].as_i64().unwrap_or_default();
                let at = entry["at_ms"].as_i64().unwrap_or(now);
                self.feed(id, at, &entry["record"], &mut events);
            }
            self.watermark = batch["next"].as_i64().unwrap_or(self.watermark);
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
        }
        self.bounds(now, &mut events);
        self.flush(store, now, &events, &mut lines);
        self.plan(now);
        Ok(lines)
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
                .filter(|(_, row)| row.open() && row.kind == "postgres_crash" && row.subject == log.subject)
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
            "alert.fired" | "alert.cleared" => {}
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
                text: format!(
                    "mission #{id}: {} (by {kind})",
                    changed(&before, &after)
                ),
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
            self.gating(event_id, id, &task.mission, record, out);
        }
        let after = task_state_after(record, accept_gate, task.revisions);
        if let Some(after) = after {
            if bumps_revision(record) {
                if let Some(entry) = self.tasks.get_mut(&id) {
                    entry.revisions += 1;
                }
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
        {
            if let Some(assigned) = task.assigned_event {
                let key = format!("task_undelivered:{id}:{assigned}");
                if self.is_open(&key) {
                    out.push(Event::Clear {
                        key,
                        evidence: json!({"event_id": event_id, "record": kind}),
                    });
                }
            }
        }
    }

    /// A gating's mission counters, and 033's base disagreement.
    fn gating(
        &mut self,
        event_id: i64,
        task: i64,
        mission_id: &i64,
        record: &Value,
        out: &mut Vec<Event>,
    ) {
        if let Some(mission) = self.missions.get_mut(mission_id) {
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
    fn admission_record(&mut self, event_id: i64, at_ms: i64, record: &Value, out: &mut Vec<Event>) {
        let kind = record["kind"].as_str().unwrap_or("execution");
        let matched = if kind == "execution" {
            format!("execution:{}", record["agent_id"].as_str().unwrap_or_default())
        } else {
            format!("gate:{}", record["task"].as_i64().unwrap_or_default())
        };
        match record["type"].as_str().unwrap_or_default() {
            "admission.waiting" => {
                let subject = if kind == "execution" {
                    record["agent_id"].as_str().unwrap_or("an execution").to_string()
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
            if open.is_empty() {
                if let Some(stuck) = stuck {
                    if stuck >= LIKELY {
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
                                "{session} is stuck ({stuck:.2}): {}",
                                judgement["state"].as_str().unwrap_or("unknown")
                            ),
                            notice: false,
                        });
                        continue;
                    }
                }
            }
            if stopped && !open.is_empty() {
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
            if key.starts_with("owner-note-") || self.known.contains_key(&format!("command_failed:{id}"))
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
            let due = task.assigned_at.saturating_add(self.settings.undelivered_ms);
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
                    matched, now.saturating_sub(pending.at_ms) / 1000
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
        for log in &self.logs {
            let open = self.open_keys("postgres_crash", log.subject);
            if open.is_empty() {
                continue;
            }
            let quiet = now.saturating_sub(log.last_match_ms);
            if quiet < self.settings.quiet_ms {
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

    fn read_memory(&mut self, now: i64, out: &mut Vec<Event>, lines: &mut Vec<String>) {
        if self.settings.memory_low_mb == 0 {
            return;
        }
        match available_mb() {
            Ok(measured) => {
                if measured < self.settings.memory_low_mb && self.open_keys("memory_low", "host").is_empty()
                {
                    out.push(Event::Fire {
                        key: format!("memory_low:{now}"),
                        kind: "memory_low",
                        subject: "host".to_string(),
                        evidence: json!({
                            "measured_mb": measured,
                            "threshold_mb": self.settings.memory_low_mb,
                            "top": top_processes(),
                        }),
                        text: format!(
                            "MemAvailable is {measured} MiB, under {}",
                            self.settings.memory_low_mb
                        ),
                        notice: false,
                    });
                } else if measured >= self.settings.memory_ok_mb {
                    for key in self.open_keys("memory_low", "host") {
                        out.push(Event::Clear {
                            key,
                            evidence: json!({
                                "measured_mb": measured,
                                "threshold_mb": self.settings.memory_ok_mb,
                            }),
                        });
                    }
                }
            }
            Err(error) => lines.push(format!("alerts: {error}")),
        }
    }

    fn read_tmpfs(&mut self, now: i64, out: &mut Vec<Event>, lines: &mut Vec<String>) {
        if self.settings.tmpfs_high_mb == 0 {
            return;
        }
        match used_mb(&self.settings.tmpfs_path) {
            Ok(measured) => {
                if measured > self.settings.tmpfs_high_mb
                    && self.open_keys("tmpfs_high", "host").is_empty()
                {
                    out.push(Event::Fire {
                        key: format!("tmpfs_high:{now}"),
                        kind: "tmpfs_high",
                        subject: "host".to_string(),
                        evidence: json!({
                            "path": self.settings.tmpfs_path.display().to_string(),
                            "measured_mb": measured,
                            "threshold_mb": self.settings.tmpfs_high_mb,
                        }),
                        text: format!(
                            "{} holds {measured} MiB, over {}",
                            self.settings.tmpfs_path.display(),
                            self.settings.tmpfs_high_mb
                        ),
                        notice: false,
                    });
                } else if measured < self.settings.tmpfs_ok_mb {
                    for key in self.open_keys("tmpfs_high", "host") {
                        out.push(Event::Clear {
                            key,
                            evidence: json!({
                                "path": self.settings.tmpfs_path.display().to_string(),
                                "measured_mb": measured,
                                "threshold_mb": self.settings.tmpfs_ok_mb,
                            }),
                        });
                    }
                }
            }
            Err(error) => lines.push(format!("alerts: {error}")),
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
                self.say_once(index, &format!("alerts: {}: {error}", path.display()), lines);
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
    fn flush(&mut self, store: &PostgresStore, now: i64, events: &[Event], lines: &mut Vec<String>) {
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
            if let Some(assigned) = task.assigned_event {
                if !self
                    .known
                    .contains_key(&format!("task_undelivered:{id}:{assigned}"))
                {
                    next = next.min(task.assigned_at.saturating_add(self.settings.undelivered_ms));
                }
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
            if let Some(backup) = self.backups.get(subject) {
                if !self
                    .known
                    .contains_key(&format!("backup_stale:{subject}:{}", backup.event_id))
                {
                    next = next.min(backup.at_ms.saturating_add(max_age));
                }
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
        "task.assigned" => {
            if mission.state == "open" {
                mission.state = "active".to_string();
            }
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
            .map(|(rss_kb, pid, name)| {
                json!({"pid": pid, "rss_mb": rss_kb / 1024, "name": name})
            })
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
        Some(at) => (log.carry[..=at].to_string(), log.carry[at + 1..].to_string()),
        None => (String::new(), std::mem::take(&mut log.carry)),
    };
    log.carry = carry;
    Ok(Some((complete, truncated)))
}

/// The stamp and `[pid]` of a crash line stamped within the day, as written:
/// `2026-11-08 18:25:00.123 UTC [1]`. `None` for any other line.
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
    let prefix = line.get(..line.len() - rest.len() + end + 1)?.to_string();
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
    let yoe = (year - era * 400) as u64;
    let mp = u64::from(if month > 2 { month - 3 } else { month + 9 });
    let doy = (153 * mp + 2) / 5 + u64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

/// The civil date and time of an epoch in ms, in UTC.
fn civil(ms: i64) -> (i64, u32, u32, u32, u32, u32) {
    let seconds = ms.div_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
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
        rest = &rest[start + end..];
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
            println!(
                "{}",
                Value::Array(alerts.clone())
            );
        } else {
            for alert in &alerts {
                println!("{}", line(alert));
            }
        }
        return Ok(0);
    }
    let after = match since.as_deref() {
        Some(text) => text
            .trim()
            .parse::<i64>()
            .map_err(|_| format!("--since {text:?} is not an event id\n{USAGE}"))?,
        None => {
            let rows = store.alert_rows().map_err(|error| error.to_string())?;
            rows["cursor"].as_i64().unwrap_or_default()
        }
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
        let transitions = store
            .alert_transitions(cursor, BATCH)
            .map_err(|error| error.to_string())?;
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
