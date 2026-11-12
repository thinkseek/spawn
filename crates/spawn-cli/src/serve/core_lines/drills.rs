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
//! `record_verified`). A row with `unit` is read without root through
//! `$SPAWN_SYSTEMCTL show -p Result,ExecMainStatus,ExecMainExitTimestamp
//! --timestamp=unix -- <unit>` (default `systemctl`), its arguments as argv
//! and no shell, each call bounded ([`UNIT_LIMIT`]; one that runs over is no
//! pass) and its duration logged: it passed when `Result` is `success` and
//! `ExecMainStatus` 0, at its exit timestamp. A reading is kept for
//! [`UNIT_KEPT_MS`]. A unit name other than
//! `[A-Za-z0-9][A-Za-z0-9@._-]*.service` is never run ([`unit_allowed`]) and
//! the log names it as refused: systemctl reads a leading `-` as an option
//! (`-H<host>` is an SSH connection to that host), and a `.timer` has no
//! `ExecMain*` result, so it could never pass. A row with neither has no
//! drill. The session backups' status file (`at`, epoch seconds) gives the
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

/// `<program> show -p Result,ExecMainStatus,ExecMainExitTimestamp
/// --timestamp=unix -- <unit>`, as argv, no stdin, its output read: `Err`
/// when it could not start, ran over `limit` (killed, with what it
/// started) or failed. A name [`unit_allowed`] refuses is never run.
pub(super) fn show(program: &str, unit: &str, limit: Duration) -> Result<String, String> {
    use std::os::unix::process::CommandExt as _;

    if !unit_allowed(unit) {
        return Err(format!("unit {unit:?} refused"));
    }
    let mut child = Command::new(program)
        .args([
            "show",
            "-p",
            "Result,ExecMainStatus,ExecMainExitTimestamp",
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

/// The units' readings kept: `(read at, last pass)` by unit.
static UNITS: Mutex<BTreeMap<String, (i64, Option<i64>)>> = Mutex::new(BTreeMap::new());

/// The refused names already in the log: each is named once.
static REFUSED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// A unit's last pass at `now_ms`, kept for [`UNIT_KEPT_MS`]; each call is
/// logged with its duration. A refused name is logged (once) and not run.
fn unit_pass(unit: &str, now_ms: i64) -> Option<i64> {
    if !unit_allowed(unit) {
        let mut refused = REFUSED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if refused.insert(unit.to_string()) {
            eprintln!(
                "core-lines: drill unit {unit:?} refused: not [A-Za-z0-9][A-Za-z0-9@._-]*.service"
            );
        }
        return None;
    }
    if let Some((at, pass)) = UNITS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(unit)
        .copied()
        && now_ms.saturating_sub(at) < UNIT_KEPT_MS
    {
        return pass;
    }
    let program = crate::env_value("SPAWN_SYSTEMCTL").unwrap_or_else(|| "systemctl".to_string());
    let started = Instant::now();
    let shown = show(&program, unit, UNIT_LIMIT);
    let took = started.elapsed().as_millis();
    let pass = match shown {
        Ok(text) => {
            eprintln!("core-lines: {program} show {unit}: {took} ms");
            parse_show(&text)
        }
        Err(why) => {
            eprintln!("core-lines: {program} show {unit}: {why} ({took} ms)");
            None
        }
    };
    UNITS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(unit.to_string(), (now_ms, pass));
    pass
}

/// Each row's last pass, in order: a record's from `record`, a unit's from
/// `unit` (only for a name [`unit_allowed`] takes), none without a drill.
pub(super) fn last_passes(
    config: &Config,
    mut record: impl FnMut(&str) -> Option<i64>,
    mut unit: impl FnMut(&str) -> Option<i64>,
) -> Vec<Option<i64>> {
    config
        .drills
        .iter()
        .map(|drill| match &drill.source {
            Source::Record(kind) => record(kind),
            Source::Unit(name) if unit_allowed(name) => unit(name),
            Source::Unit(_) | Source::None => None,
        })
        .collect()
}

/// The answer's `drills`: `{name, source, passed_at_ms}` in the file's
/// order, `source` "record <type>", "unit <name>" or null.
pub(super) fn drills_json(config: &Config, passes: &[Option<i64>]) -> Value {
    json!(
        config
            .drills
            .iter()
            .zip(passes)
            .map(|(drill, pass)| {
                let source = match &drill.source {
                    Source::Record(kind) => json!(format!("record {kind}")),
                    Source::Unit(unit) => json!(format!("unit {unit}")),
                    Source::None => Value::Null,
                };
                json!({"name": drill.name, "source": source, "passed_at_ms": pass})
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

/// The north star and the two ages at `now_ms`, from the rows' last passes
/// and the status file's moment:
/// - `recoverability.restores_passed`: the rows whose last pass is within
///   7 days; null with no rows;
/// - `recoverability.session_backup_age`: `now_ms` less the status file's
///   moment; null without one;
/// - `recoverability.oldest_drill_age`: `now_ms` less the oldest last pass,
///   rows that never passed left out; null with none.
pub(super) fn figures(
    passes: &[Option<i64>],
    status_at_ms: Option<i64>,
    now_ms: i64,
) -> (Figure, Vec<Figure>) {
    let age =
        |at: Option<i64>| at.map_or(Value::Null, |at| json!(now_ms.saturating_sub(at).max(0)));
    let since = now_ms.saturating_sub(WEEK_MS);
    let within = if passes.is_empty() {
        Value::Null
    } else {
        json!(passes.iter().flatten().filter(|at| **at >= since).count())
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
                age(passes.iter().flatten().min().copied()),
            ),
        ],
    )
}

/// The drills read now: the north star, the two ages and the answer's
/// `drills`, from the Core lines file's `text` and the store.
pub(super) struct Drills {
    pub(super) north_star: Figure,
    pub(super) guardrails: Vec<Figure>,
    pub(super) list: Value,
}

/// Read the drills of the file's `text` at `now_ms`: a record's last pass
/// from the store (a failed read is no pass, and logged), a unit's through
/// systemctl ([`unit_pass`]). The two ages join the line only when the file
/// names its recovery table (`recoverability`): without one both would be
/// null by construction, and objective 087's line stays its four
/// guardrails. The north star is always there (null with no rows).
pub(super) fn read(
    store: &spawn_postgres::PostgresStore,
    text: Option<&str>,
    now_ms: i64,
) -> Drills {
    let config = config(text);
    let passes = last_passes(
        &config,
        |kind| match store.newest_passed_record(kind) {
            Ok(at) => at,
            Err(error) => {
                eprintln!("core-lines: the newest passing {kind}: {error}");
                None
            }
        },
        |unit| unit_pass(unit, now_ms),
    );
    // A refused name is named in the log too.
    for drill in &config.drills {
        if let Source::Unit(unit) = &drill.source
            && !unit_allowed(unit)
        {
            unit_pass(unit, now_ms);
        }
    }
    let (north_star, mut guardrails) =
        figures(&passes, status_at_ms(config.status.as_deref()), now_ms);
    if !config.present {
        guardrails.clear();
    }
    Drills {
        north_star,
        guardrails,
        list: drills_json(&config, &passes),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Config, Drill, Source, config, drills_json, figures, last_passes, parse_show, show,
        status_at_ms, unit_allowed,
    };
    use serde_json::{Value, json};
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

    /// The drill rows: a passing and a failing record (the store gives the
    /// newest passing one), a unit's three results, a refused name, no
    /// drill, one outside 7 days; and both ages.
    #[test]
    fn the_rows_last_passes_count_within_7_days_and_give_both_ages() {
        let now = 100 * DAY;
        let config = Config {
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
        };
        let mut asked = Vec::new();
        let passes = last_passes(
            &config,
            |kind| (kind == "backup.verified").then_some(now - 3 * HOUR),
            |unit| {
                asked.push(unit.to_string());
                match unit {
                    "a.service" => Some(now - 10 * HOUR),
                    "c.service" => Some(now - 8 * DAY),
                    _ => None,
                }
            },
        );
        assert_eq!(asked, ["a.service", "b.service", "c.service"]);
        assert_eq!(
            passes,
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
        let list = drills_json(&config, &passes);
        assert_eq!(
            list[0],
            json!({"name": "Database", "source": "record backup.verified", "passed_at_ms": now - 3 * HOUR})
        );
        assert_eq!(
            list[5],
            json!({"name": "Refused", "source": "unit -Hhost.service", "passed_at_ms": null})
        );
        assert_eq!(
            list[6],
            json!({"name": "Configuration", "source": null, "passed_at_ms": null})
        );

        let (star, ages) = figures(&passes, Some(now - 10 * 60_000), now);
        assert_eq!(star.key, "recoverability.restores_passed");
        assert_eq!(star.value, json!(2), "the old drill is 8 days old");
        assert_eq!(ages[0].key, "recoverability.session_backup_age");
        assert_eq!(ages[0].value, json!(10 * 60_000));
        assert_eq!(ages[1].key, "recoverability.oldest_drill_age");
        assert_eq!(ages[1].value, json!(8 * DAY), "never passed is left out");

        // No rows: no north star; none passed: 0 and no oldest age.
        let (star, ages) = figures(&[], None, now);
        assert_eq!(star.value, Value::Null);
        assert_eq!(ages[0].value, Value::Null);
        assert_eq!(ages[1].value, Value::Null);
        let (star, ages) = figures(&[None, None], None, now);
        assert_eq!(star.value, json!(0));
        assert_eq!(ages[1].value, Value::Null);
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

    fn script(dir: &std::path::Path, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join("systemctl");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write the fake");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
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
        let text = show(&program, "a.service", Duration::from_secs(5)).expect("shown");
        assert_eq!(parse_show(&text), Some(5000));
        assert_eq!(
            std::fs::read_to_string(&calls).expect("the call"),
            "show\n-p\nResult,ExecMainStatus,ExecMainExitTimestamp\n--timestamp=unix\n--\na.service\n"
        );
    }

    #[test]
    fn a_unit_call_that_runs_over_is_killed_and_no_pass() {
        let dir = Scratch::new();
        let program = script(dir.as_path(), "sleep 30\necho Result=success");
        let started = Instant::now();
        let shown = show(&program, "a.service", Duration::from_millis(300));
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
        let shown = show(&program, "a.service", Duration::from_millis(300));
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
            let shown = show(&program, bad, Duration::from_secs(5));
            assert!(
                shown.as_ref().is_err_and(|why| why.contains("refused")),
                "{bad}: {shown:?}"
            );
        }
        assert!(!ran.exists(), "systemctl was never run");
        // The good name runs it.
        let _ = show(&program, "a.service", Duration::from_secs(5));
        assert!(ran.exists());
    }
}
