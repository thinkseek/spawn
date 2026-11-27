//! The recoverability north star (objective 093): the critical resources
//! whose restore drill passed in the last 7 days, with the age of the
//! newest session backup and of the oldest passed drill.
//!
//! Which resources count is the recovery table's (sync/recovery.md), so the
//! rows live in the Core lines file beside the ranges, read at every
//! request:
//!
//! ```text
//! "recoverability": {
//!   "drills": [{"name", "record"?: <type>, "unit"?: <name>.service}],
//!   "session_backup_status"?: <path>
//! }
//! ```
//!
//! A row with `record` passed at the newest control record of that type
//! with `ok` true (the database drill's `backup.verified`, backup.rs
//! `record_verified`); its newest drill (`last`, objective 105) is the
//! newest record of that type whatever its `ok`, both read in one statement
//! (`drill_of_type`). A row with `unit` is read without root through
//! `$SPAWN_SYSTEMCTL show -p
//! LoadState,Result,ExecMainStatus,ExecMainExitTimestamp --timestamp=unix --
//! <unit>` (default `systemctl`), its arguments as argv
//! and no shell, each call bounded ([`UNIT_LIMIT`]; one that runs over is no
//! pass) and its duration logged: it passed when `Result` is `success` and
//! `ExecMainStatus` 0, at its exit timestamp, and the same output gives its
//! newest run (`last`: not ok, with "Result=<Result>,
//! ExecMainStatus=<status>", otherwise). A reading is kept for
//! [`UNIT_KEPT_MS`]. The system manager is asked first; when it does not
//! know the unit (`LoadState=not-found`, where it also answers
//! `Result=success`, `ExecMainStatus=0` and no exit: no pass), the user
//! manager is asked once (`--user show ...`, the same bound, logged, kept
//! the same), since a drill can be a `--user` unit (objective 231: the
//! controllers' drill, installed in ~/.config/systemd/user, read as never
//! passed). A unit the user manager knew is remembered as the user's
//! ([`USER_UNITS`]) and asked of it first from then on, so the system
//! manager's "not found" is not asked (and logged) again on every reading;
//! once the user manager no longer knows it, it is forgotten and the system
//! manager asked again. A unit both managers know is the system manager's, and so is
//! an answer with no `LoadState` line (a stand-in, an older systemd); a
//! system call that fails is no reading, never asked again. A unit name other than
//! `[A-Za-z0-9][A-Za-z0-9@._-]*.service` is never run ([`unit_allowed`]) and
//! the log names it as refused: systemctl reads a leading `-` as an option
//! (`-H<host>` is an SSH connection to that host), and a `.timer` has no
//! `ExecMain*` result, so it could never pass. A row with neither has no
//! drill. The north star is out of the rows (objective 181,
//! [`out_of_rows`]). The session backups' status file (`at`, epoch seconds) gives the
//! newest backup's age.
//!
//! Monitoring only: no gate, integrator or deploy step reads any of it.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use spawn_core::control::{ControlRecord, ControllersDrillOutcome, DrillOutcome};

use super::Figure;

/// How long one `systemctl show` may run before it counts as no pass.
pub(super) const UNIT_LIMIT: Duration = Duration::from_secs(3);

/// How long a unit's reading is kept, so a Home poll does not call
/// systemctl again at every request.
const UNIT_KEPT_MS: i64 = 60_000;

/// The window of the north star: 7 days.
const WEEK_MS: i64 = 7 * 86_400_000;

/// Where a row's drill reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Source {
    /// The newest control record of this type with `ok` true.
    Record(String),
    /// A systemd unit's last run.
    Unit(String),
    /// No drill.
    None,
}

/// One row of the recovery table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Drill {
    pub(super) name: String,
    pub(super) source: Source,
}

/// The file's `recoverability`: whether it names one, its rows, in order,
/// and the session backups' status file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Config {
    pub(super) present: bool,
    pub(super) drills: Vec<Drill>,
    pub(super) status: Option<PathBuf>,
}

/// The `recoverability` of the Core lines file's `text` (`None`: no file).
/// A file that is not JSON, or without the object, gives no rows; a row
/// without a name is passed over.
pub(super) fn config(text: Option<&str>) -> Config {
    let Some(value) = text.and_then(|text| serde_json::from_str::<Value>(text).ok()) else {
        return Config::default();
    };
    let section = &value["recoverability"];
    if !section.is_object() {
        return Config::default();
    }
    let drills = section["drills"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let name = row["name"].as_str()?.to_string();
            let source = if let Some(record) = row["record"].as_str() {
                Source::Record(record.to_string())
            } else if let Some(unit) = row["unit"].as_str() {
                Source::Unit(unit.to_string())
            } else {
                Source::None
            };
            Some(Drill { name, source })
        })
        .collect();
    let status = section["session_backup_status"]
        .as_str()
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    Config {
        present: true,
        drills,
        status,
    }
}

/// Whether systemctl may be asked about `unit`: a letter or digit first
/// (never `-`, which systemctl reads as an option), then letters, digits
/// and `@._-`, ending in `.service` (a timer has no `ExecMain*` result).
pub(super) fn unit_allowed(unit: &str) -> bool {
    let Some(stem) = unit.strip_suffix(".service") else {
        return false;
    };
    let mut chars = stem.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '.' | '_' | '-'))
}

/// The last pass in `systemctl show`'s output: `Result=success`,
/// `ExecMainStatus=0` and `ExecMainExitTimestamp=@<epoch seconds>`, in ms;
/// `None` otherwise (a failure, or a unit that has not run).
pub(super) fn parse_show(text: &str) -> Option<i64> {
    let mut properties = BTreeMap::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            properties.insert(key.trim(), value.trim());
        }
    }
    if properties.get("Result") != Some(&"success")
        || properties.get("ExecMainStatus") != Some(&"0")
    {
        return None;
    }
    let seconds: i64 = properties
        .get("ExecMainExitTimestamp")?
        .trim_start_matches('@')
        .parse()
        .ok()?;
    (seconds > 0).then(|| seconds.saturating_mul(1000))
}

/// A row's newest drill, whatever it gave (objective 105).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Last {
    /// When it ended; `None` when that is not known (a unit's run with no
    /// exit timestamp).
    pub(super) at_ms: Option<i64>,
    pub(super) ok: bool,
    /// "" when it passed.
    pub(super) reason: String,
}

/// What a row's drill gave: its last pass and its newest drill.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Reading {
    pub(super) passed_at_ms: Option<i64>,
    pub(super) last: Option<Last>,
}

impl Reading {
    /// Counted by the north star at `since`: a pass at or after it, and a
    /// newest drill that did not fail.
    fn counts(&self, since: i64) -> bool {
        self.passed_at_ms.is_some_and(|at| at >= since)
            && self.last.as_ref().is_none_or(|last| last.ok)
    }
}

/// The newest run in `systemctl show`'s output: ok exactly when `Result`
/// is `success` and `ExecMainStatus` 0, at its `ExecMainExitTimestamp`
/// (`None` without one), its reason "Result=<Result>,
/// ExecMainStatus=<status>" when not ok. `None` for a unit that has not
/// run: no `Result`, or a success with no exit timestamp (what systemd
/// shows for a unit never started).
pub(super) fn parse_last(text: &str) -> Option<Last> {
    let mut properties = BTreeMap::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            properties.insert(key.trim(), value.trim());
        }
    }
    let result = *properties.get("Result")?;
    let status = properties.get("ExecMainStatus").copied().unwrap_or("");
    let at_ms = properties
        .get("ExecMainExitTimestamp")
        .and_then(|at| at.trim_start_matches('@').parse::<i64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(|seconds| seconds.saturating_mul(1000));
    let ok = result == "success" && status == "0";
    if ok && at_ms.is_none() {
        return None;
    }
    Some(Last {
        at_ms,
        ok,
        reason: if ok {
            String::new()
        } else {
            format!("Result={result}, ExecMainStatus={status}")
        },
    })
}

/// A record type's reading from [`spawn_postgres::PostgresStore::drill_of_type`]'s
/// `{at_ms, record, passed_at_ms}`: the newest record's verdict. A restore
/// drill's (`backup.drilled.*`) is its [`DrillOutcome`] ([`drill_verdict`]);
/// any other type's (the database drill's `backup.verified`) is ok exactly
/// when its `ok` is JSON true, its reason its `reason` when a string, else "".
pub(super) fn record_reading(answer: Option<&Value>) -> Reading {
    let Some(answer) = answer else {
        return Reading::default();
    };
    let record = &answer["record"];
    let (ok, reason) = drill_verdict(record).unwrap_or_else(|| {
        (
            record["ok"] == Value::Bool(true),
            record["reason"].as_str().unwrap_or("").to_string(),
        )
    });
    Reading {
        passed_at_ms: answer["passed_at_ms"].as_i64(),
        last: Some(Last {
            at_ms: answer["at_ms"].as_i64(),
            ok,
            reason,
        }),
    }
}

/// A restore drill record's verdict, read through its closed outcome: ok
/// with reason "" when it passed, its reason when it failed (a controllers
/// run that could not restore the backup failed with its reason). A drill
/// record no writer could have written (one `spawn_backup_record` refuses)
/// is no pass, and says why. `None` for a record of another type.
fn drill_verdict(record: &Value) -> Option<(bool, String)> {
    if !record["type"].as_str()?.starts_with("backup.drilled.") {
        return None;
    }
    let unwritten = |why: &dyn std::fmt::Display| {
        (false, format!("not a drill record any writer wrote: {why}"))
    };
    let verdict = |outcome: Result<DrillOutcome<'_>, _>| match outcome {
        Ok(DrillOutcome::Passed) => (true, String::new()),
        Ok(DrillOutcome::Failed { reason }) => (false, reason.to_string()),
        Err(error) => unwritten(&error),
    };
    Some(match ControlRecord::from_value_strict(record) {
        Ok(
            ControlRecord::BackupDrilledTranscripts(drill)
            | ControlRecord::BackupDrilledMissions(drill)
            | ControlRecord::BackupDrilledSync(drill),
        ) => verdict(drill.outcome()),
        Ok(ControlRecord::BackupDrilledControllers(drill)) => {
            verdict(drill.outcome().map(|outcome| match outcome {
                ControllersDrillOutcome::Compared { outcome, .. } => outcome,
                ControllersDrillOutcome::Unrestorable { reason } => DrillOutcome::Failed { reason },
            }))
        }
        Ok(other) => unwritten(&other.kind().as_str()),
        Err(error) => unwritten(&error),
    })
}

/// `<program> [--user] show -p
/// LoadState,Result,ExecMainStatus,ExecMainExitTimestamp --timestamp=unix --
/// <unit>`, as argv, no stdin, its output read: the system manager's, or
/// the user manager's with `user` (objective 231). `Err` when it could not
/// start, ran over `limit` (killed, with what it started) or failed. A name
/// [`unit_allowed`] refuses is never run.
pub(super) fn show(
    program: &str,
    unit: &str,
    user: bool,
    limit: Duration,
) -> Result<String, String> {
    use std::os::unix::process::CommandExt as _;

    if !unit_allowed(unit) {
        return Err(format!("unit {unit:?} refused"));
    }
    let mut child = Command::new(program)
        .args(user.then_some("--user"))
        .args([
            "show",
            "-p",
            "LoadState,Result,ExecMainStatus,ExecMainExitTimestamp",
            "--timestamp=unix",
            "--",
            unit,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|error| format!("{program}: {error}"))?;
    // The output is read on a thread of its own, so a child that fills its
    // pipe cannot hold this one past its limit. Its text comes back on a
    // channel: one deadline bounds both the child's exit and the end of its
    // output (cc-astra A25), since a process the child started can hold the
    // pipe open after the child itself has exited.
    let mut stdout = child.stdout.take();
    let (send, output) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(stdout) = stdout.as_mut() {
            let _ = stdout.read_to_string(&mut text);
        }
        let _ = send.send(text);
    });
    let group = child.id();
    let kill_group = || {
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{group}")])
            .stderr(Stdio::null())
            .status();
    };
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                kill_group();
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("ran over {} ms", limit.as_millis()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => return Err(error.to_string()),
        }
    };
    let Ok(text) = output.recv_timeout(deadline.saturating_duration_since(Instant::now())) else {
        // What it started still holds the pipe: its group is killed and the
        // reader is left to end with the pipe, never joined here.
        kill_group();
        return Err(format!(
            "its output did not end within {} ms (a process it started holds it)",
            limit.as_millis()
        ));
    };
    let _ = reader.join();
    if status.success() {
        Ok(text)
    } else {
        Err(format!("exited {status}"))
    }
}

/// The units' readings kept: `(read at, reading)` by unit.
static UNITS: Mutex<BTreeMap<String, (i64, Reading)>> = Mutex::new(BTreeMap::new());

/// Held across a unit's cache check, call and keeping: a reading that runs
/// beside another (serve's warm reading and the first request) waits for
/// its call and takes what it kept, so each unit is asked once.
static SHOWING: Mutex<()> = Mutex::new(());

/// The units the user manager knew when the system manager did not, by
/// `(program, unit)`: asked of the user manager first ([`read_unit`]).
static USER_UNITS: Mutex<BTreeSet<(String, String)>> = Mutex::new(BTreeSet::new());

/// The refused names already in the log: each is named once.
static REFUSED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// A unit's last pass and last run at `now_ms`, from one `systemctl show`,
/// kept for [`UNIT_KEPT_MS`] (one call at a time, [`SHOWING`]); each call
/// is logged with its duration. A
/// refused name is logged (once) and not run.
fn unit_reading(unit: &str, now_ms: i64) -> Reading {
    if !unit_allowed(unit) {
        let mut refused = REFUSED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if refused.insert(unit.to_string()) {
            eprintln!(
                "core-lines: drill unit {unit:?} refused: not [A-Za-z0-9][A-Za-z0-9@._-]*.service"
            );
        }
        return Reading::default();
    }
    let _showing = SHOWING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((at, reading)) = UNITS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(unit)
        .cloned()
        && now_ms.saturating_sub(at) < UNIT_KEPT_MS
    {
        return reading;
    }
    let program = crate::env_value("SPAWN_SYSTEMCTL").unwrap_or_else(|| "systemctl".to_string());
    let reading = read_unit(&program, unit, UNIT_LIMIT);
    UNITS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(unit.to_string(), (now_ms, reading.clone()));
    reading
}

/// Whether a `systemctl show` answer says the manager does not know the
/// unit: a `LoadState=not-found` line, and no other load state (masked,
/// error, bad-setting, merged, stub are known units).
pub(super) fn not_found(text: &str) -> bool {
    text.lines()
        .any(|line| line.trim_end() == "LoadState=not-found")
}

/// A unit's last pass and last run from `program`, each call logged with
/// its duration: the system manager's answer, or, when it says the unit is
/// not found, the user manager's (objective 231), asked once with the same
/// `limit`. A system call that fails is no reading and is not asked again.
/// A unit the user manager knew is remembered ([`USER_UNITS`]) and asked of
/// it first on the next reading, without the system manager's probe; when
/// the user manager no longer knows it, it is forgotten and the system
/// manager is asked as above (a unit moved back to the system's).
fn read_unit(program: &str, unit: &str, limit: Duration) -> Reading {
    let key = (program.to_string(), unit.to_string());
    let user_units = || {
        USER_UNITS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    };
    if user_units().contains(&key) {
        let started = Instant::now();
        let shown = show(program, unit, true, limit);
        let took = started.elapsed().as_millis();
        match shown {
            Ok(text) if not_found(&text) => {
                eprintln!(
                    "core-lines: {program} --user show {unit}: no longer a user unit ({took} ms); asking the system manager"
                );
                user_units().remove(&key);
            }
            Ok(text) => {
                eprintln!("core-lines: {program} --user show {unit}: {took} ms");
                return unit_of(&text);
            }
            Err(why) => {
                eprintln!("core-lines: {program} --user show {unit}: {why} ({took} ms)");
                return Reading::default();
            }
        }
    }
    let started = Instant::now();
    let shown = show(program, unit, false, limit);
    let took = started.elapsed().as_millis();
    let text = match shown {
        Ok(text) if not_found(&text) => {
            eprintln!(
                "core-lines: {program} show {unit}: not a system unit ({took} ms); asking the user manager"
            );
            let started = Instant::now();
            let shown = show(program, unit, true, limit);
            let took = started.elapsed().as_millis();
            match shown {
                Ok(text) => {
                    eprintln!("core-lines: {program} --user show {unit}: {took} ms");
                    if !not_found(&text) {
                        user_units().insert(key);
                    }
                    text
                }
                Err(why) => {
                    eprintln!("core-lines: {program} --user show {unit}: {why} ({took} ms)");
                    return Reading::default();
                }
            }
        }
        Ok(text) => {
            eprintln!("core-lines: {program} show {unit}: {took} ms");
            text
        }
        Err(why) => {
            eprintln!("core-lines: {program} show {unit}: {why} ({took} ms)");
            return Reading::default();
        }
    };
    unit_of(&text)
}

/// A unit's last pass and last run from its `systemctl show` answer.
fn unit_of(text: &str) -> Reading {
    Reading {
        passed_at_ms: parse_show(text),
        last: parse_last(text),
    }
}

/// Each row's reading, in order: a record's from `record` (each type read
/// once however many rows name it), a unit's from `unit` (only for a name
/// [`unit_allowed`] takes), none without a drill.
pub(super) fn readings(
    config: &Config,
    mut record: impl FnMut(&str) -> Reading,
    mut unit: impl FnMut(&str) -> Reading,
) -> Vec<Reading> {
    let mut types: BTreeMap<&str, Reading> = BTreeMap::new();
    config
        .drills
        .iter()
        .map(|drill| match &drill.source {
            Source::Record(kind) => types
                .entry(kind.as_str())
                .or_insert_with(|| record(kind))
                .clone(),
            Source::Unit(name) if unit_allowed(name) => unit(name),
            Source::Unit(_) | Source::None => Reading::default(),
        })
        .collect()
}

/// The answer's `drills`: `{name, source, passed_at_ms, last}` in the
/// file's order, `source` "record <type>", "unit <name>" or null, `last`
/// `{at_ms, ok, reason}` or null.
pub(super) fn drills_json(config: &Config, readings: &[Reading]) -> Value {
    json!(
        config
            .drills
            .iter()
            .zip(readings)
            .map(|(drill, reading)| {
                let source = match &drill.source {
                    Source::Record(kind) => json!(format!("record {kind}")),
                    Source::Unit(unit) => json!(format!("unit {unit}")),
                    Source::None => Value::Null,
                };
                let last = reading.last.as_ref().map_or(
                    Value::Null,
                    |last| json!({"at_ms": last.at_ms, "ok": last.ok, "reason": last.reason}),
                );
                json!({
                    "name": drill.name, "source": source,
                    "passed_at_ms": reading.passed_at_ms, "last": last,
                })
            })
            .collect::<Vec<_>>()
    )
}

/// The status file's `at` (epoch seconds), in ms; `None` without a
/// readable one.
pub(super) fn status_at_ms(path: Option<&Path>) -> Option<i64> {
    let text = std::fs::read_to_string(path?).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let seconds = value["at"].as_i64()?;
    Some(seconds.saturating_mul(1000))
}

/// The north star and the two ages at `now_ms`, from the rows' readings
/// and the status file's moment:
/// - `recoverability.restores_passed`: the rows whose last pass is within
///   7 days and whose newest drill did not fail (none, or ok: objective
///   105); null with no rows;
/// - `recoverability.session_backup_age`: `now_ms` less the status file's
///   moment; null without one;
/// - `recoverability.oldest_drill_age`: `now_ms` less the oldest last pass,
///   rows that never passed left out; null with none.
pub(super) fn figures(
    readings: &[Reading],
    status_at_ms: Option<i64>,
    now_ms: i64,
) -> (Figure, Vec<Figure>) {
    let age =
        |at: Option<i64>| at.map_or(Value::Null, |at| json!(now_ms.saturating_sub(at).max(0)));
    let since = now_ms.saturating_sub(WEEK_MS);
    let within = if readings.is_empty() {
        Value::Null
    } else {
        json!(
            readings
                .iter()
                .filter(|reading| reading.counts(since))
                .count()
        )
    };
    (
        Figure::new(
            "recoverability.restores_passed",
            "Critical resources with a restore passed in 7 days",
            "count",
            within,
        ),
        vec![
            Figure::new(
                "recoverability.session_backup_age",
                "Age of the newest session backup",
                "ms",
                age(status_at_ms),
            ),
            Figure::new(
                "recoverability.oldest_drill_age",
                "Age of the oldest passed drill",
                "ms",
                age(readings
                    .iter()
                    .filter_map(|reading| reading.passed_at_ms)
                    .min()),
            ),
        ],
    )
}

/// The north star out of the recovery table's rows (objective 181): `of`
/// the row count and the note "rows: <name>, ..." in the file's order, rows
/// without a drill among them; neither with no rows. What counts is
/// [`figures`]'s, unchanged.
pub(super) fn out_of_rows(star: &mut Figure, config: &Config) {
    if config.drills.is_empty() {
        return;
    }
    star.of = Some(config.drills.len());
    star.note = Some(format!(
        "rows: {}",
        config
            .drills
            .iter()
            .map(|drill| drill.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    ));
}

/// The drills read now: the north star, the two ages and the answer's
/// `drills`, from the Core lines file's `text` and the store.
pub(super) struct Drills {
    pub(super) north_star: Figure,
    pub(super) guardrails: Vec<Figure>,
    pub(super) list: Value,
}

/// Read the drills of the file's `text` at `now_ms`: a record type's newest
/// record and last pass from the store in one statement (a failed read is
/// no reading, and logged), a unit's through systemctl
/// ([`unit_reading`]). The two ages join the line only when the file
/// names its recovery table (`recoverability`): without one both would be
/// null by construction, and objective 087's line stays its four
/// guardrails. The north star is always there (null with no rows).
pub(super) fn read(
    store: &spawn_postgres::PostgresStore,
    text: Option<&str>,
    now_ms: i64,
) -> Drills {
    let config = config(text);
    let readings = readings(
        &config,
        |kind| match store.drill_of_type(kind) {
            Ok(answer) => record_reading(answer.as_ref()),
            Err(error) => {
                eprintln!("core-lines: the newest {kind}: {error}");
                Reading::default()
            }
        },
        |unit| unit_reading(unit, now_ms),
    );
    // A refused name is named in the log too.
    for drill in &config.drills {
        if let Source::Unit(unit) = &drill.source
            && !unit_allowed(unit)
        {
            unit_reading(unit, now_ms);
        }
    }
    let (mut north_star, mut guardrails) =
        figures(&readings, status_at_ms(config.status.as_deref()), now_ms);
    out_of_rows(&mut north_star, &config);
    if !config.present {
        guardrails.clear();
    }
    Drills {
        north_star,
        guardrails,
        list: drills_json(&config, &readings),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Config, Drill, Last, Reading, Source, config, drills_json, figures, not_found, out_of_rows,
        parse_last, parse_show, read_unit, readings, record_reading, show, status_at_ms,
        unit_allowed,
    };
    use serde_json::{Value, json};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// A scratch directory removed when the test ends, passed or not:
    /// a test leaves nothing in the temporary directory.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new() -> Self {
            Scratch(crate::storage::tests::scratch("drills"))
        }

        fn as_path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const HOUR: i64 = 3_600_000;
    const DAY: i64 = 24 * HOUR;

    #[test]
    fn the_rows_come_from_the_files_recoverability_in_order() {
        let text = r#"{"ranges": {}, "recoverability": {
            "session_backup_status": "/var/backups/spawn-sessions/status.json",
            "drills": [
              {"name": "Database", "record": "backup.verified"},
              {"name": "Session files", "unit": "spawn-session-backup-drill.service"},
              {"record": "nameless"},
              {"name": "Configuration and credentials"}]}}"#;
        assert_eq!(
            config(Some(text)),
            Config {
                present: true,
                drills: vec![
                    Drill {
                        name: "Database".into(),
                        source: Source::Record("backup.verified".into())
                    },
                    Drill {
                        name: "Session files".into(),
                        source: Source::Unit("spawn-session-backup-drill.service".into())
                    },
                    Drill {
                        name: "Configuration and credentials".into(),
                        source: Source::None
                    },
                ],
                status: Some("/var/backups/spawn-sessions/status.json".into()),
            }
        );
        assert_eq!(config(None), Config::default());
        assert_eq!(config(Some("not json")), Config::default());
        assert_eq!(config(Some(r#"{"ranges": {}}"#)), Config::default());
        assert!(!Config::default().present, "no table named");
        // A table with no rows is named, and has none.
        let empty = config(Some(r#"{"recoverability": {}}"#));
        assert!(empty.present && empty.drills.is_empty() && empty.status.is_none());
    }

    #[test]
    fn only_a_plain_service_name_is_ever_asked_about() {
        for good in [
            "spawn-session-backup-drill.service",
            "bb-drill-a.service",
            "getty@tty1.service",
            "a.b_c.service",
            "0x.service",
        ] {
            assert!(unit_allowed(good), "{good}");
        }
        for bad in [
            "-Hbb.invalid.service",
            "--host=bb.invalid.service",
            "bb drill; touch /tmp/ran",
            "bb-drill-a.timer",
            "bb-drill-a",
            ".service",
            "_x.service",
            "a/b.service",
            "a$(id).service",
            "",
        ] {
            assert!(!unit_allowed(bad), "{bad}");
        }
    }

    #[test]
    fn a_unit_passed_on_success_and_status_0_at_its_exit() {
        assert_eq!(
            parse_show("Result=success\nExecMainStatus=0\nExecMainExitTimestamp=@1731400000\n"),
            Some(1_731_400_000_000)
        );
        assert_eq!(
            parse_show("Result=exit-code\nExecMainStatus=1\nExecMainExitTimestamp=@1731400000\n"),
            None,
            "a failed run"
        );
        assert_eq!(
            parse_show("Result=success\nExecMainStatus=2\nExecMainExitTimestamp=@1731400000\n"),
            None
        );
        assert_eq!(
            parse_show("Result=success\nExecMainStatus=0\nExecMainExitTimestamp=\n"),
            None,
            "never run: no exit timestamp"
        );
        assert_eq!(parse_show(""), None);
    }

    /// The rows of [`the_rows_last_passes_count_within_7_days_and_give_both_ages`].
    fn seven_rows() -> Config {
        Config {
            present: true,
            drills: vec![
                Drill {
                    name: "Database".into(),
                    source: Source::Record("backup.verified".into()),
                },
                Drill {
                    name: "Failing record".into(),
                    source: Source::Record("backup.other".into()),
                },
                Drill {
                    name: "Passed".into(),
                    source: Source::Unit("a.service".into()),
                },
                Drill {
                    name: "Failed".into(),
                    source: Source::Unit("b.service".into()),
                },
                Drill {
                    name: "Old".into(),
                    source: Source::Unit("c.service".into()),
                },
                Drill {
                    name: "Refused".into(),
                    source: Source::Unit("-Hhost.service".into()),
                },
                Drill {
                    name: "Configuration".into(),
                    source: Source::None,
                },
            ],
            status: None,
        }
    }

    /// The drill rows: a passing and a failing record (the store gives the
    /// newest passing one), a unit's three results, a refused name, no
    /// drill, one outside 7 days; and both ages.
    #[test]
    fn the_rows_last_passes_count_within_7_days_and_give_both_ages() {
        let now = 100 * DAY;
        let config = seven_rows();
        let mut asked = Vec::new();
        let passed = |at: i64| Reading {
            passed_at_ms: Some(at),
            last: Some(Last {
                at_ms: Some(at),
                ok: true,
                reason: String::new(),
            }),
        };
        let readings = readings(
            &config,
            |kind| {
                if kind == "backup.verified" {
                    passed(now - 3 * HOUR)
                } else {
                    Reading::default()
                }
            },
            |unit| {
                asked.push(unit.to_string());
                match unit {
                    "a.service" => passed(now - 10 * HOUR),
                    "c.service" => passed(now - 8 * DAY),
                    _ => Reading::default(),
                }
            },
        );
        assert_eq!(asked, ["a.service", "b.service", "c.service"]);
        assert_eq!(
            readings
                .iter()
                .map(|reading| reading.passed_at_ms)
                .collect::<Vec<_>>(),
            [
                Some(now - 3 * HOUR),
                None,
                Some(now - 10 * HOUR),
                None,
                Some(now - 8 * DAY),
                None,
                None
            ]
        );
        let list = drills_json(&config, &readings);
        assert_eq!(
            list[0],
            json!({"name": "Database", "source": "record backup.verified",
                   "passed_at_ms": now - 3 * HOUR,
                   "last": {"at_ms": now - 3 * HOUR, "ok": true, "reason": ""}})
        );
        assert_eq!(
            list[5],
            json!({"name": "Refused", "source": "unit -Hhost.service", "passed_at_ms": null, "last": null})
        );
        assert_eq!(
            list[6],
            json!({"name": "Configuration", "source": null, "passed_at_ms": null, "last": null})
        );

        let (star, ages) = figures(&readings, Some(now - 10 * 60_000), now);
        assert_eq!(star.key, "recoverability.restores_passed");
        assert_eq!(star.value, json!(2), "the old drill is 8 days old");
        assert_eq!(ages[0].key, "recoverability.session_backup_age");
        assert_eq!(ages[0].value, json!(10 * 60_000));
        assert_eq!(ages[1].key, "recoverability.oldest_drill_age");
        assert_eq!(ages[1].value, json!(8 * DAY), "never passed is left out");
        // Objective 181: out of the rows, each named, those without a drill
        // (and the refused one) among them; what counts is unchanged.
        let mut star = star;
        out_of_rows(&mut star, &config);
        assert_eq!((star.value.clone(), star.of), (json!(2), Some(7)));
        assert_eq!(
            star.note.as_deref(),
            Some("rows: Database, Failing record, Passed, Failed, Old, Refused, Configuration")
        );
        let shown = super::super::figure_json(&star, None);
        assert_eq!(shown["of"], json!(7));
        assert_eq!(list[6]["last"], Value::Null, "no drill, never counted");

        // No rows: no north star, neither `of` nor a note; none passed: 0
        // and no oldest age.
        let (mut star, ages) = figures(&[], None, now);
        out_of_rows(&mut star, &Config::default());
        assert_eq!((star.of, star.note.clone()), (None, None));
        let shown = super::super::figure_json(&star, None);
        assert!(shown.get("of").is_none() && shown.get("note").is_none());
        assert_eq!(star.value, Value::Null);
        assert_eq!(ages[0].value, Value::Null);
        assert_eq!(ages[1].value, Value::Null);
        let (star, ages) = figures(&[Reading::default(), Reading::default()], None, now);
        assert_eq!(star.value, json!(0));
        assert_eq!(ages[1].value, Value::Null);
    }

    /// Objective 105: a row whose newest drill failed after a pass within
    /// 7 days is not counted; its pass still gives the oldest age. A pass
    /// with no newest drill read (`last` null) still counts.
    #[test]
    fn a_row_whose_newest_drill_failed_is_not_counted() {
        let now = 100 * DAY;
        let failed = Reading {
            passed_at_ms: Some(now - 3 * HOUR),
            last: Some(Last {
                at_ms: Some(now - HOUR),
                ok: false,
                reason: "the restore failed".into(),
            }),
        };
        let unread = Reading {
            passed_at_ms: Some(now - 5 * HOUR),
            last: None,
        };
        let (star, ages) = figures(&[failed, unread], None, now);
        assert_eq!(star.value, json!(1));
        assert_eq!(
            ages[1].value,
            json!(5 * HOUR),
            "the failed row's pass is newer"
        );
    }

    /// Each record type is read once per reading, however many rows name it.
    #[test]
    fn a_record_type_is_read_once_per_reading() {
        let row = |name: &str| Drill {
            name: name.into(),
            source: Source::Record("backup.verified".into()),
        };
        let config = Config {
            present: true,
            drills: vec![row("One"), row("Two")],
            status: None,
        };
        let mut reads = 0;
        let got = readings(
            &config,
            |_| {
                reads += 1;
                Reading::default()
            },
            |_| Reading::default(),
        );
        assert_eq!(reads, 1);
        assert_eq!(got.len(), 2);
    }

    /// A record type's newest record: ok exactly when `ok` is JSON true,
    /// its reason a string's or ""; no record, no reading.
    #[test]
    fn a_records_newest_drill_is_its_own_ok_and_reason() {
        let reading = record_reading(Some(&json!({
            "at_ms": 7, "passed_at_ms": 3,
            "record": {"type": "backup.verified", "ok": false, "reason": "pg_restore exited 1"},
        })));
        assert_eq!(reading.passed_at_ms, Some(3));
        assert_eq!(
            reading.last,
            Some(Last {
                at_ms: Some(7),
                ok: false,
                reason: "pg_restore exited 1".into()
            })
        );
        let odd = record_reading(Some(&json!({
            "at_ms": 9, "passed_at_ms": null,
            "record": {"type": "backup.verified", "ok": "true", "reason": 5},
        })));
        assert_eq!(
            odd.last,
            Some(Last {
                at_ms: Some(9),
                ok: false,
                reason: String::new()
            }),
            "ok is JSON true only; a reason that is no string is \"\""
        );
        assert_eq!(record_reading(None), Reading::default());
    }

    /// A restore drill's newest record reads through its closed outcome: a
    /// pass, a failure with its reason, a controllers run that could not
    /// restore the backup; a record no writer could have written (a pass
    /// that names a reason, a failure without one) is no pass and says why.
    #[test]
    fn a_drill_records_verdict_is_its_closed_outcome() {
        let last = |record: Value| {
            record_reading(Some(
                &json!({"at_ms": 7, "passed_at_ms": 3, "record": record}),
            ))
            .last
            .expect("a newest drill")
        };
        let drill = |kind: &str, ok: bool, reason: &str| {
            json!({"type": kind, "ok": ok, "reason": reason, "bundle": "b",
                   "bundle_event_id": 1, "mismatches": u64::from(!ok), "duration_ms": 5})
        };
        let passed = last(drill("backup.drilled.transcripts", true, ""));
        assert!(passed.ok);
        assert_eq!(passed.reason, "");
        let failed = last(drill("backup.drilled.missions", false, "missions: 1 older"));
        assert!(!failed.ok);
        assert_eq!(failed.reason, "missions: 1 older");
        let unrestorable = last(drill("backup.drilled.controllers", false, "no bundle"));
        assert!(!unrestorable.ok);
        assert_eq!(unrestorable.reason, "no bundle");
        for (ok, reason) in [(true, "why"), (false, "")] {
            let odd = last(drill("backup.drilled.sync", ok, reason));
            assert!(!odd.ok, "{ok} {reason:?}");
            assert!(
                odd.reason
                    .starts_with("not a drill record any writer wrote"),
                "{}",
                odd.reason
            );
        }
    }

    /// A unit's newest run from the same `systemctl show` output.
    #[test]
    fn a_units_newest_run_is_read_from_its_show_output() {
        assert_eq!(
            parse_last("Result=success\nExecMainStatus=0\nExecMainExitTimestamp=@5\n"),
            Some(Last {
                at_ms: Some(5000),
                ok: true,
                reason: String::new()
            })
        );
        assert_eq!(
            parse_last("Result=exit-code\nExecMainStatus=1\nExecMainExitTimestamp=@5\n"),
            Some(Last {
                at_ms: Some(5000),
                ok: false,
                reason: "Result=exit-code, ExecMainStatus=1".into()
            })
        );
        assert_eq!(
            parse_last("Result=timeout\nExecMainStatus=0\nExecMainExitTimestamp=\n"),
            Some(Last {
                at_ms: None,
                ok: false,
                reason: "Result=timeout, ExecMainStatus=0".into()
            }),
            "a failure with no exit timestamp"
        );
        assert_eq!(
            parse_last("Result=success\nExecMainStatus=0\nExecMainExitTimestamp=\n"),
            None,
            "never run"
        );
        assert_eq!(parse_last(""), None);
    }

    #[test]
    fn the_status_files_at_is_in_seconds() {
        let dir = Scratch::new();
        let path = dir.as_path().join("status.json");
        std::fs::write(&path, r#"{"at": 1731400000, "accounts": []}"#).expect("write");
        assert_eq!(status_at_ms(Some(&path)), Some(1_731_400_000_000));
        std::fs::write(&path, "nope").expect("write");
        assert_eq!(status_at_ms(Some(&path)), None);
        assert_eq!(status_at_ms(Some(&dir.as_path().join("none.json"))), None);
        assert_eq!(status_at_ms(None), None);
    }

    /// The fake systemctl, written and made executable by a child `sh`
    /// (objective 107): no descriptor of this process is ever open for
    /// writing on it, so another test's child, between its fork and its
    /// exec, cannot hold it busy (ETXTBSY) when a test runs it.
    fn script(dir: &std::path::Path, body: &str) -> String {
        let path = dir.join("systemctl");
        let status = Command::new("sh")
            .args([
                "-c",
                "printf '%s\\n' \"$1\" >\"$2\" && chmod 755 \"$2\"",
                "sh",
            ])
            .arg(format!("#!/bin/sh\n{body}"))
            .arg(&path)
            .stdin(Stdio::null())
            .status()
            .expect("run sh to write the fake");
        assert!(status.success(), "writing the fake: {status}");
        path.display().to_string()
    }

    #[test]
    fn a_unit_call_is_argv_with_the_unit_after_the_double_dash() {
        let dir = Scratch::new();
        let calls = dir.as_path().join("calls");
        let program = script(
            dir.as_path(),
            &format!(
                "printf '%s\\n' \"$@\" >{}\nprintf 'Result=success\\nExecMainStatus=0\\nExecMainExitTimestamp=@5\\n'",
                calls.display()
            ),
        );
        let text = show(&program, "a.service", false, Duration::from_secs(5)).expect("shown");
        assert_eq!(parse_show(&text), Some(5000));
        assert_eq!(
            std::fs::read_to_string(&calls).expect("the call"),
            "show\n-p\nLoadState,Result,ExecMainStatus,ExecMainExitTimestamp\n--timestamp=unix\n--\na.service\n"
        );
    }

    #[test]
    fn a_unit_call_that_runs_over_is_killed_and_no_pass() {
        let dir = Scratch::new();
        let program = script(dir.as_path(), "sleep 30\necho Result=success");
        let started = Instant::now();
        let shown = show(&program, "a.service", false, Duration::from_millis(300));
        assert!(started.elapsed() < Duration::from_secs(5), "bounded");
        assert!(
            shown.as_ref().is_err_and(|why| why.contains("ran over")),
            "{shown:?}"
        );
    }

    /// cc-astra A25: the direct child exits at once, but a process it
    /// started keeps its stdout open. The call still ends at its limit,
    /// with no pass.
    #[test]
    fn a_unit_call_whose_child_holds_the_output_ends_at_its_limit() {
        let dir = Scratch::new();
        let program = script(dir.as_path(), "echo Result=success\nsleep 30 &\nexit 0");
        let started = Instant::now();
        let shown = show(&program, "a.service", false, Duration::from_millis(300));
        assert!(started.elapsed() < Duration::from_secs(5), "bounded");
        assert!(
            shown
                .as_ref()
                .is_err_and(|why| why.contains("did not end within 300 ms")),
            "{shown:?}"
        );
    }

    #[test]
    fn a_refused_name_is_never_run() {
        let dir = Scratch::new();
        let ran = dir.as_path().join("ran");
        let program = script(dir.as_path(), &format!("touch {}", ran.display()));
        for bad in [
            "-Hbb.invalid.service",
            "--host=bb.invalid.service",
            "-x.service",
            "bb drill; touch x",
            "bb-drill-a.timer",
        ] {
            let shown = show(&program, bad, false, Duration::from_secs(5));
            assert!(
                shown.as_ref().is_err_and(|why| why.contains("refused")),
                "{bad}: {shown:?}"
            );
        }
        assert!(!ran.exists(), "systemctl was never run");
        // The good name runs it.
        let shown = show(&program, "a.service", false, Duration::from_secs(5));
        assert!(shown.is_ok(), "a.service: {shown:?}");
        assert!(ran.exists());
    }

    /// A fake systemctl answering `system` as the system manager and
    /// `user` as the user manager (its first argument `--user`), each
    /// call's argv appended to `calls`, one line a call.
    fn managers(dir: &std::path::Path, system: &str, user: &str) -> (String, std::path::PathBuf) {
        let calls = dir.join("calls");
        let program = script(
            dir,
            &format!(
                "echo \"$*\" >>{calls}\nif [ \"$1\" = --user ]; then printf '{user}'; else printf '{system}'; fi",
                calls = calls.display()
            ),
        );
        (program, calls)
    }

    const NOT_FOUND: &str =
        "LoadState=not-found\\nResult=success\\nExecMainStatus=0\\nExecMainExitTimestamp=\\n";
    const SYSTEM_CALL: &str = "show -p LoadState,Result,ExecMainStatus,ExecMainExitTimestamp --timestamp=unix -- a.service";

    /// Objective 231: a unit the system manager does not know is asked of
    /// the user manager once, and its answer is the reading.
    #[test]
    fn a_unit_the_system_manager_does_not_know_is_read_from_the_user_manager() {
        let dir = Scratch::new();
        let (program, calls) = managers(
            dir.as_path(),
            NOT_FOUND,
            "LoadState=loaded\\nResult=success\\nExecMainStatus=0\\nExecMainExitTimestamp=@7\\n",
        );
        let reading = read_unit(&program, "a.service", Duration::from_secs(5));
        assert_eq!(reading.passed_at_ms, Some(7000));
        assert_eq!(
            reading.last,
            Some(Last {
                at_ms: Some(7000),
                ok: true,
                reason: String::new()
            })
        );
        assert_eq!(
            std::fs::read_to_string(&calls).expect("the calls"),
            format!("{SYSTEM_CALL}\n--user {SYSTEM_CALL}\n")
        );
    }

    #[test]
    fn a_failed_user_unit_says_its_result() {
        let dir = Scratch::new();
        let (program, _) = managers(
            dir.as_path(),
            NOT_FOUND,
            "LoadState=loaded\\nResult=exit-code\\nExecMainStatus=1\\nExecMainExitTimestamp=@7\\n",
        );
        let reading = read_unit(&program, "a.service", Duration::from_secs(5));
        assert_eq!(reading.passed_at_ms, None);
        assert_eq!(
            reading.last,
            Some(Last {
                at_ms: Some(7000),
                ok: false,
                reason: "Result=exit-code, ExecMainStatus=1".to_string()
            })
        );
    }

    /// A system unit, failed, passed or answering with no LoadState line,
    /// is the system manager's: the user manager (which would say another
    /// pass) is never asked.
    #[test]
    fn a_system_unit_is_never_asked_of_the_user_manager() {
        let user =
            "LoadState=loaded\\nResult=success\\nExecMainStatus=0\\nExecMainExitTimestamp=@99\\n";
        for (system, passed) in [
            (
                "LoadState=loaded\\nResult=exit-code\\nExecMainStatus=1\\nExecMainExitTimestamp=@5\\n",
                None,
            ),
            (
                "LoadState=loaded\\nResult=success\\nExecMainStatus=0\\nExecMainExitTimestamp=@5\\n",
                Some(5000),
            ),
            (
                "Result=success\\nExecMainStatus=0\\nExecMainExitTimestamp=@5\\n",
                Some(5000),
            ),
            (
                "LoadState=masked\\nResult=success\\nExecMainStatus=0\\nExecMainExitTimestamp=@5\\n",
                Some(5000),
            ),
        ] {
            let dir = Scratch::new();
            let (program, calls) = managers(dir.as_path(), system, user);
            let reading = read_unit(&program, "a.service", Duration::from_secs(5));
            assert_eq!(reading.passed_at_ms, passed, "{system}");
            assert_eq!(
                std::fs::read_to_string(&calls).expect("the calls"),
                format!("{SYSTEM_CALL}\n"),
                "{system}"
            );
        }
    }

    #[test]
    fn a_unit_neither_manager_knows_has_no_pass() {
        let dir = Scratch::new();
        let (program, calls) = managers(dir.as_path(), NOT_FOUND, NOT_FOUND);
        assert_eq!(
            read_unit(&program, "a.service", Duration::from_secs(5)),
            Reading::default()
        );
        assert_eq!(
            std::fs::read_to_string(&calls)
                .expect("the calls")
                .lines()
                .count(),
            2
        );
    }

    /// A system call that fails (exits 1) is no reading, and the user
    /// manager is not asked.
    #[test]
    fn a_system_call_that_fails_is_not_asked_of_the_user_manager() {
        let dir = Scratch::new();
        let calls = dir.as_path().join("calls");
        let program = script(
            dir.as_path(),
            &format!(
                "echo \"$*\" >>{}\nif [ \"$1\" = --user ]; then printf 'Result=success\\nExecMainStatus=0\\nExecMainExitTimestamp=@9\\n'; exit 0; fi\nprintf 'LoadState=not-found\\n'\nexit 1",
                calls.display()
            ),
        );
        assert_eq!(
            read_unit(&program, "a.service", Duration::from_secs(5)),
            Reading::default()
        );
        assert_eq!(
            std::fs::read_to_string(&calls).expect("the calls"),
            format!("{SYSTEM_CALL}\n")
        );
    }

    /// A unit the user manager knew is asked of it first on the next
    /// reading: the system manager's "not found" is not probed (and logged)
    /// again on every tick. Once the user manager no longer knows it, the
    /// system manager is asked again and its answer is the reading, and the
    /// unit is the system's from then on.
    #[test]
    fn a_known_user_unit_is_asked_of_the_user_manager_first() {
        let dir = Scratch::new();
        let calls = dir.as_path().join("calls");
        let system = dir.as_path().join("system");
        let user = dir.as_path().join("user");
        let answer = |path: &std::path::Path, text: &str| {
            std::fs::write(path, text).expect("write the answer");
        };
        let program = script(
            dir.as_path(),
            &format!(
                "echo \"$*\" >>{calls}\nif [ \"$1\" = --user ]; then cat {user}; else cat {system}; fi",
                calls = calls.display(),
                user = user.display(),
                system = system.display()
            ),
        );
        let calls_made = || {
            let made = std::fs::read_to_string(&calls).expect("the calls");
            std::fs::remove_file(&calls).expect("clear the calls");
            made
        };
        let not_found =
            "LoadState=not-found\nResult=success\nExecMainStatus=0\nExecMainExitTimestamp=\n";
        answer(&system, not_found);
        answer(
            &user,
            "LoadState=loaded\nResult=success\nExecMainStatus=0\nExecMainExitTimestamp=@7\n",
        );
        let first = read_unit(&program, "a.service", Duration::from_secs(5));
        assert_eq!(first.passed_at_ms, Some(7000));
        assert_eq!(
            calls_made(),
            format!("{SYSTEM_CALL}\n--user {SYSTEM_CALL}\n")
        );
        // The second reading asks the user manager alone.
        let second = read_unit(&program, "a.service", Duration::from_secs(5));
        assert_eq!(second, first);
        assert_eq!(calls_made(), format!("--user {SYSTEM_CALL}\n"));
        // Another unit of the same program is not remembered by it.
        read_unit(&program, "b.service", Duration::from_secs(5));
        assert!(calls_made().starts_with("show "), "b.service: system first");

        // Moved back to the system manager: asked there once the user
        // manager no longer knows it, and there first from then on.
        answer(&user, not_found);
        answer(
            &system,
            "LoadState=loaded\nResult=success\nExecMainStatus=0\nExecMainExitTimestamp=@9\n",
        );
        let moved = read_unit(&program, "a.service", Duration::from_secs(5));
        assert_eq!(moved.passed_at_ms, Some(9000));
        assert_eq!(
            calls_made(),
            format!("--user {SYSTEM_CALL}\n{SYSTEM_CALL}\n")
        );
        let again = read_unit(&program, "a.service", Duration::from_secs(5));
        assert_eq!(again.passed_at_ms, Some(9000));
        assert_eq!(calls_made(), format!("{SYSTEM_CALL}\n"));
    }

    /// Only `LoadState=not-found` is not found; every other load state is
    /// a unit the manager knows, and other properties are never read for it.
    #[test]
    fn not_found_reads_only_the_load_state() {
        assert!(not_found("LoadState=not-found\nResult=success\n"));
        assert!(not_found("Result=success\nLoadState=not-found"));
        for known in ["loaded", "masked", "error", "bad-setting", "merged", "stub"] {
            assert!(!not_found(&format!("LoadState={known}\n")), "{known}");
        }
        assert!(!not_found("Result=not-found\nExecMainStatus=0\n"));
        assert!(!not_found("SubState=not-found\n"));
        assert!(!not_found(""));
    }
}
