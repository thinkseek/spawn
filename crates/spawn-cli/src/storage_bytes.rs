//! The real bytes on disk of each storage source (objective 180): the
//! reading the storage north star ("Bytes a landing leaves behind") and S1
//! ("Rebuildable bytes on disk") are measured from (objective 185).
//!
//! An apparent size says nothing on btrfs, where a reflinked copy shares its
//! data, so the loop asks the file system for each file's extents and counts
//! each physical extent once, in the first source that holds it
//! (scripts/storage-extents.py). Once a day, by the database's clock, it
//! records a `host.storage_bytes` reading of six sources:
//!
//! - `transcripts`, `build_cache` and `scratch`, through the script, their
//!   extents counted in that order: the transcript archive and Claude Code's
//!   projects; `$XDG_CACHE_HOME`, else `$HOME/.cache`; the Core lines file's
//!   `storage.scratch_roots` less its `storage.scratch_exclude`. A source
//!   with no root is not measured, and says so;
//! - `session_backups`: the status file's `estimated_bytes` (objective 097);
//! - `test_databases`: each configured test database's size, each database
//!   once by name;
//! - `live_database`: the live database's size.
//!
//! The script runs at `nice -n 19`, on a thread of its own, never inside a
//! pass; it is stopped after [`SCRIPT_LIMIT`], and each source has
//! [`BOUND_SECS`]. No reading starts while the owner holds any source
//! (`spawn hold`) or while the quiet-hold file (the Core lines file's
//! `storage.quiet_hold_file`, else `SPAWN_QUIET_HOLD_FILE`) cannot be read
//! or its first line does not say `ENDED`; a hold that begins while the
//! script runs stops it. Nothing is recorded during a hold: a reading of
//! sources all not measured must never take the day's slot. A reading that
//! cannot be taken or written is a line in the loop's log, never a failed
//! pass, and is tried again [`RETRY_MS`] later. No gate, integrator or
//! deploy step reads the readings: monitoring only.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use spawn_postgres::PostgresStore;

use crate::core_lines_sample::Reading;
use crate::serve::core_lines::StorageBytesSettings;

/// How old the newest reading is when another is due.
pub(crate) const DAY_MS: i64 = 86_400_000;

/// How soon a reading put off by a hold, or one that could not be taken or
/// written, is looked at again.
pub(crate) const RETRY_MS: i64 = 600_000;

/// How long the extents script may run before it is stopped.
pub(crate) const SCRIPT_LIMIT: Duration = Duration::from_hours(1);

/// How long the script may take over one source.
pub(crate) const BOUND_SECS: u64 = 1200;

/// How many characters of the extents script's own reason a source not
/// measured keeps (objective 202).
pub(crate) const WHY_MAX_CHARS: usize = 300;

/// How often the holds are looked at while a reading is in flight.
const HOLD_LOOK_MS: i64 = 5_000;

/// The script's name beside the binary.
pub(crate) const SCRIPT: &str = "storage-extents.py";

pub(crate) const TRANSCRIPTS: &str = "transcripts";
pub(crate) const BUILD_CACHE: &str = "build_cache";
pub(crate) const SCRATCH: &str = "scratch";
pub(crate) const SESSION_BACKUPS: &str = "session_backups";
pub(crate) const TEST_DATABASES: &str = "test_databases";
pub(crate) const LIVE_DATABASE: &str = "live_database";

/// One source the script reads: its roots and what it never enters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Source {
    pub(crate) name: &'static str,
    pub(crate) roots: Vec<PathBuf>,
    pub(crate) exclude: Vec<PathBuf>,
}

/// What one reading reads.
#[derive(Clone)]
pub(crate) struct Plan {
    /// The script's sources, in their counting order.
    pub(crate) sources: Vec<Source>,
    /// The session backups' status file (objective 097).
    pub(crate) session_status: Option<PathBuf>,
    /// The configured test databases with their URLs: never printed.
    pub(crate) test_databases: Vec<(String, String)>,
    pub(crate) script: PathBuf,
}

/// The build cache: `$XDG_CACHE_HOME`, else `$HOME/.cache`.
fn build_cache() -> Option<PathBuf> {
    crate::env_value("XDG_CACHE_HOME")
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            crate::env_value("HOME")
                .filter(|path| !path.trim().is_empty())
                .map(|home| PathBuf::from(home).join(".cache"))
        })
}

/// The script `SPAWN_STORAGE_EXTENTS` names, else the one beside the binary.
pub(crate) fn script_path() -> PathBuf {
    crate::env_value("SPAWN_STORAGE_EXTENTS")
        .filter(|path| !path.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|binary| binary.parent().map(|dir| dir.join(SCRIPT)))
        })
        .unwrap_or_else(|| PathBuf::from(SCRIPT))
}

/// The script's three sources, in their counting order: the transcripts
/// (the archive under `root`, and Claude Code's `projects`), the build
/// cache `cache`, and the scratch roots `settings` names.
pub(crate) fn sources(
    root: Option<&Path>,
    projects: PathBuf,
    cache: Option<PathBuf>,
    settings: &StorageBytesSettings,
) -> Vec<Source> {
    let mut transcripts: Vec<PathBuf> =
        root.map(crate::storage::archive_path).into_iter().collect();
    transcripts.push(projects);
    vec![
        Source {
            name: TRANSCRIPTS,
            roots: transcripts,
            exclude: Vec::new(),
        },
        Source {
            name: BUILD_CACHE,
            roots: cache.into_iter().collect(),
            exclude: Vec::new(),
        },
        Source {
            name: SCRATCH,
            roots: settings.scratch_roots.clone(),
            exclude: settings.scratch_exclude.clone(),
        },
    ]
}

/// The plan of a reading now, for the workspace root `root`.
pub(crate) fn plan(root: Option<&Path>, settings: &StorageBytesSettings) -> Plan {
    Plan {
        sources: sources(
            root,
            crate::storage::projects_path(),
            build_cache(),
            settings,
        ),
        session_status: crate::storage::session_backup_status(),
        test_databases: settings.test_databases.clone(),
        script: script_path(),
    }
}

/// The script's input: each source with a root, in the counting order.
pub(crate) fn script_input(sources: &[Source], bound_secs: u64) -> Value {
    let paths = |paths: &[PathBuf]| -> Vec<String> {
        paths
            .iter()
            .map(|path| path.display().to_string())
            .collect()
    };
    json!({
        "bound_secs": bound_secs,
        "sources": sources
            .iter()
            .filter(|source| !source.roots.is_empty())
            .map(|source| json!({
                "name": source.name,
                "roots": paths(&source.roots),
                "exclude": paths(&source.exclude),
            }))
            .collect::<Vec<_>>(),
    })
}

/// A source's entry: not measured, and why.
fn not_measured(why: impl Into<String>) -> Value {
    json!({"not_measured": why.into()})
}

/// The script's sources' entries from its answer (or why it gave none): a
/// source with no root, one over its bound, and one the answer leaves out
/// are not measured, each with why.
pub(crate) fn script_sources(
    sources: &[Source],
    answer: &Result<Value, String>,
) -> Map<String, Value> {
    let mut out = Map::new();
    for source in sources {
        let entry = if source.roots.is_empty() {
            not_measured("no root to read")
        } else {
            match answer {
                Err(error) => not_measured(format!("the extents script: {error}")),
                Ok(answer) => answer["sources"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|entry| entry["name"] == source.name)
                    .map_or_else(
                        || not_measured("the extents script gave no answer for it"),
                        script_entry,
                    ),
            }
        };
        out.insert(source.name.to_string(), entry);
    }
    out
}

/// One source's entry of the script's answer: its bytes, files, unreadable
/// count and seconds, or not measured when the script says so (with its
/// reason, control characters dropped and cut at [`WHY_MAX_CHARS`]), when
/// over its bound, or when malformed.
fn script_entry(entry: &Value) -> Value {
    if let Some(why) = entry["not_measured"].as_str() {
        let why: String = why
            .chars()
            .filter(|c| !c.is_control())
            .take(WHY_MAX_CHARS)
            .collect();
        if !why.trim().is_empty() {
            return not_measured(why);
        }
    }
    if entry["over_bound"] == true {
        return not_measured(format!(
            "over its bound of {BOUND_SECS} s ({} files read)",
            entry["files"].as_u64().unwrap_or_default()
        ));
    }
    match entry["bytes"].as_u64() {
        Some(bytes) => json!({
            "bytes": bytes,
            "files": entry["files"].as_u64().unwrap_or_default(),
            "unreadable": entry["unreadable"].as_u64().unwrap_or_default(),
            "seconds": entry["seconds"].as_f64().unwrap_or_default(),
        }),
        None => not_measured("the extents script's answer for it has no bytes"),
    }
}

/// The session backups' entry: the status file's `estimated_bytes`.
pub(crate) fn session_backups(status: Option<&Path>) -> Value {
    match (status, crate::storage::session_backup_bytes(status)) {
        (_, Some(bytes)) => json!({"bytes": bytes}),
        (None, None) => not_measured("no status file is configured"),
        (Some(_), None) => not_measured("the status file gives no estimated_bytes"),
    }
}

/// The test databases' entry: their sizes summed, each database once by its
/// name (two URLs of one database count it once). One that cannot be read
/// leaves the source not measured, naming its role, never its URL.
pub(crate) fn test_databases(
    databases: &[(String, String)],
    read: impl Fn(&str) -> Result<Value, String>,
) -> Value {
    let mut counted: Vec<String> = Vec::new();
    let mut bytes: u64 = 0;
    for (role, url) in databases {
        let size = match read(url) {
            Ok(size) => size,
            Err(error) => {
                return not_measured(format!(
                    "the test database {role} could not be read: {error}"
                ));
            }
        };
        let name = size["name"].as_str().unwrap_or_default().to_string();
        if counted.contains(&name) {
            continue;
        }
        let Some(size_bytes) = size["bytes"].as_u64() else {
            return not_measured(format!("the test database {role} gave no size"));
        };
        bytes = bytes.saturating_add(size_bytes);
        counted.push(name);
    }
    json!({"bytes": bytes, "databases": counted.len()})
}

/// The live database's entry, from its size reading (`database_size`).
pub(crate) fn live_database(size: Result<Value, String>) -> Value {
    match size {
        Ok(size) => size["bytes"].as_u64().map_or_else(
            || not_measured("the live database gave no size"),
            |bytes| json!({"bytes": bytes}),
        ),
        Err(error) => not_measured(format!("the live database could not be read: {error}")),
    }
}

/// The last line of `text`, at most 200 characters: what a failed script
/// said, without the rest of its output.
fn last_line(text: &str) -> String {
    text.lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .chars()
        .take(200)
        .collect()
}

/// A pipe drained on a thread of its own, so a child never blocks on it.
fn drain(pipe: Option<impl Read + Send + 'static>) -> Option<std::thread::JoinHandle<String>> {
    pipe.map(|mut pipe| {
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = pipe.read_to_string(&mut text);
            text
        })
    })
}

/// Run the extents script `script` under `nice -n 19 python3 -I -B` with
/// `input` on its stdin; its JSON answer. It is stopped (killed) after
/// `limit`, or as soon as `stop` is set.
pub(crate) fn run_script(
    script: &Path,
    input: &Value,
    limit: Duration,
    stop: &AtomicBool,
) -> Result<Value, String> {
    let mut child = Command::new("nice")
        .args(["-n", "19", "python3", "-I", "-B"])
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not start it: {error}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.to_string().as_bytes());
    }
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let begun = Instant::now();
    let stopped = |child: &mut std::process::Child, why: String| {
        let _ = child.kill();
        let _ = child.wait();
        Err(why)
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => return stopped(&mut child, format!("could not wait for it: {error}")),
        }
        if stop.load(Ordering::SeqCst) {
            return stopped(&mut child, "stopped by a hold".to_string());
        }
        if begun.elapsed() >= limit {
            return stopped(
                &mut child,
                format!("stopped after its limit of {} s", limit.as_secs()),
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let joined = |handle: Option<std::thread::JoinHandle<String>>| {
        handle
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default()
    };
    let (stdout, stderr) = (joined(stdout), joined(stderr));
    if !status.success() {
        return Err(format!("it failed ({status}): {}", last_line(&stderr)));
    }
    serde_json::from_str(stdout.trim()).map_err(|_| "it gave no JSON answer".to_string())
}

/// One reading's work, on the reading's thread: the script's sources (the
/// script stopped by `stop` or its limit), the session backups and the test
/// databases. The live database is read when the reading is written.
pub(crate) fn read(plan: &Plan, stop: &AtomicBool) -> Map<String, Value> {
    let answer = if plan.sources.iter().any(|source| !source.roots.is_empty()) {
        run_script(
            &plan.script,
            &script_input(&plan.sources, BOUND_SECS),
            SCRIPT_LIMIT,
            stop,
        )
    } else {
        Ok(json!({"sources": []}))
    };
    let mut sources = script_sources(&plan.sources, &answer);
    sources.insert(
        SESSION_BACKUPS.to_string(),
        session_backups(plan.session_status.as_deref()),
    );
    sources.insert(
        TEST_DATABASES.to_string(),
        test_databases(&plan.test_databases, |url| {
            spawn_postgres::test_database_size(url).map_err(|error| error.to_string())
        }),
    );
    sources
}

/// A reading in flight: its answer, and the flag that stops its script.
pub(crate) struct Flight {
    pub(crate) reading: Reading<Map<String, Value>>,
    pub(crate) stop: Arc<AtomicBool>,
}

/// Start a reading of `plan` on a thread of its own.
pub(crate) fn start(plan: Plan) -> Flight {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    Flight {
        reading: Reading::spawn(move || Ok(read(&plan, &flag))),
        stop,
    }
}

/// Why the quiet-hold file `file` holds a reading: `None` when no file is
/// set or its first line says `ENDED`.
pub(crate) fn quiet_hold(file: Option<&Path>) -> Option<String> {
    let file = file?;
    match std::fs::read_to_string(file) {
        Ok(text) => {
            let first = text.lines().next().unwrap_or_default();
            if first.split_whitespace().next() == Some("ENDED") {
                None
            } else {
                Some("the quiet-hold file does not say ENDED".to_string())
            }
        }
        Err(error) => Some(format!(
            "the quiet-hold file cannot be read ({})",
            error.kind()
        )),
    }
}

/// Why the owner's holds (`source_holds`: `{now_ms, holds: [{source, ...}]}`)
/// hold a reading: `None` when there is none.
pub(crate) fn source_hold(holds: &Value) -> Option<String> {
    let held: Vec<&str> = holds["holds"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|hold| hold["source"].as_str().unwrap_or("a source"))
        .collect();
    (!held.is_empty()).then(|| format!("the owner holds {}", held.join(", ")))
}

/// What the sampler reads and writes, through the store in the loop
/// ([`StoreWorld`]) and made up in the tests.
pub(crate) trait World {
    /// The newest reading and the database's clock (`host_storage_bytes`).
    fn readings(&mut self) -> Result<Value, String>;
    /// The owner's active holds (`source_holds`).
    fn holds(&mut self) -> Result<Value, String>;
    /// The Core lines file's settings.
    fn settings(&mut self) -> Result<StorageBytesSettings, String>;
    /// Start a reading with these settings.
    fn start(&mut self, settings: &StorageBytesSettings) -> Flight;
    /// The live database's entry.
    fn live_database(&mut self) -> Value;
    /// Record a reading; its event's id.
    fn write(&mut self, record: &Value) -> Result<i64, String>;
}

/// The loop's world: the store, and the workspace root.
pub(crate) struct StoreWorld<'a> {
    pub(crate) store: &'a PostgresStore,
    pub(crate) root: Option<&'a Path>,
}

impl World for StoreWorld<'_> {
    fn readings(&mut self) -> Result<Value, String> {
        self.store
            .host_storage_bytes()
            .map_err(|error| error.to_string())
    }

    fn holds(&mut self) -> Result<Value, String> {
        self.store.source_holds().map_err(|error| error.to_string())
    }

    fn settings(&mut self) -> Result<StorageBytesSettings, String> {
        crate::serve::core_lines::storage_bytes_settings()
    }

    fn start(&mut self, settings: &StorageBytesSettings) -> Flight {
        start(plan(self.root, settings))
    }

    fn live_database(&mut self) -> Value {
        live_database(
            self.store
                .database_size()
                .map_err(|error| error.to_string()),
        )
    }

    fn write(&mut self, record: &Value) -> Result<i64, String> {
        self.store
            .host_record(record)
            .map_err(|error| error.to_string())
    }
}

/// Why a reading may not start or go on now, from the quiet-hold file and
/// the owner's holds: `Ok(None)` when nothing holds it, `Err` when the holds
/// could not be read.
fn gate(world: &mut impl World, settings: &StorageBytesSettings) -> Result<Option<String>, String> {
    if let Some(why) = quiet_hold(settings.quiet_hold_file.as_deref()) {
        return Ok(Some(why));
    }
    Ok(source_hold(&world.holds()?))
}

/// A reading in flight, with when it started and when the holds were last
/// looked at, and the settings it started with.
struct InFlight {
    flight: Flight,
    started_ms: i64,
    looked_ms: i64,
    settings: StorageBytesSettings,
}

/// The loop's daily `host.storage_bytes` readings.
#[derive(Default)]
pub(crate) struct Sampler {
    /// When the sampler next looks whether a reading is due (this loop's
    /// clock); the first pass looks at once.
    next_ms: i64,
    reading: Option<InFlight>,
    /// A reading stopped or given up whose thread may still run: no other
    /// starts before it ends.
    abandoned: Option<Reading<Map<String, Value>>>,
}

impl Sampler {
    /// Looked at again [`RETRY_MS`] after `now_ms`.
    fn retry(&mut self, now_ms: i64) {
        self.next_ms = now_ms.saturating_add(RETRY_MS);
    }

    /// One pass's look: start a reading when one is due and nothing holds
    /// it; while one is in flight, stop it when a hold begins, and record it
    /// when it has come back. Returns the line for the loop's log; an error
    /// is a line too, never a failed pass.
    pub(crate) fn check_with(
        &mut self,
        now_ms: i64,
        world: &mut impl World,
    ) -> Result<Option<String>, String> {
        if self.abandoned.as_ref().is_some_and(|old| !old.running()) {
            self.abandoned = None;
        }
        if self.reading.is_some() {
            return self.in_flight(now_ms, world);
        }
        if now_ms < self.next_ms || self.abandoned.is_some() {
            return Ok(None);
        }
        let readings = match world.readings() {
            Ok(readings) => readings,
            Err(error) => {
                self.retry(now_ms);
                return Err(format!(
                    "storage bytes: the readings could not be read: {error}; trying again in {} s",
                    RETRY_MS / 1000
                ));
            }
        };
        let db_now = readings["now_ms"].as_i64().unwrap_or(now_ms);
        if let Some(at) = readings["newest"]["at_ms"].as_i64() {
            let left = at.saturating_add(DAY_MS).saturating_sub(db_now);
            if left > 0 {
                self.next_ms = now_ms.saturating_add(left);
                return Ok(None);
            }
        }
        let settings = match world.settings() {
            Ok(settings) => settings,
            Err(error) => {
                self.retry(now_ms);
                return Ok(Some(format!(
                    "storage bytes: not measured (owner gate): the Core lines file could not be read: {error}; looking again in {} s",
                    RETRY_MS / 1000
                )));
            }
        };
        match gate(world, &settings) {
            Ok(None) => {}
            Ok(Some(why)) => {
                self.retry(now_ms);
                return Ok(Some(format!(
                    "storage bytes: not measured (owner gate): {why}; looking again in {} s",
                    RETRY_MS / 1000
                )));
            }
            Err(error) => {
                self.retry(now_ms);
                return Err(format!(
                    "storage bytes: the holds could not be read: {error}; trying again in {} s",
                    RETRY_MS / 1000
                ));
            }
        }
        let flight = world.start(&settings);
        self.reading = Some(InFlight {
            flight,
            started_ms: now_ms,
            looked_ms: now_ms,
            settings,
        });
        Ok(Some(
            "storage bytes: reading the real bytes on disk of each source".to_string(),
        ))
    }

    /// Stop the reading in flight, keep its thread until it ends, and look
    /// again [`RETRY_MS`] later; the line saying why.
    fn stop(&mut self, now_ms: i64, why: &str) -> Result<Option<String>, String> {
        if let Some(stopped) = self.reading.take() {
            stopped.flight.stop.store(true, Ordering::SeqCst);
            self.abandoned = Some(stopped.flight.reading);
        }
        self.retry(now_ms);
        Err(format!(
            "storage bytes: stopped the reading: {why}; trying again in {} s",
            RETRY_MS / 1000
        ))
    }

    /// The reading in flight: stopped by a hold or holds that cannot be
    /// read, given up past the script's limit, recorded when it is back.
    fn in_flight(&mut self, now_ms: i64, world: &mut impl World) -> Result<Option<String>, String> {
        let Some(mut flight) = self.reading.take() else {
            return Ok(None);
        };
        if now_ms.saturating_sub(flight.looked_ms) >= HOLD_LOOK_MS {
            flight.looked_ms = now_ms;
            let gated = gate(world, &flight.settings);
            self.reading = Some(flight);
            match gated {
                Ok(None) => {}
                Ok(Some(why)) => return self.stop(now_ms, &format!("(owner gate) {why}")),
                Err(error) => {
                    return self.stop(now_ms, &format!("the holds could not be read: {error}"));
                }
            }
        } else {
            self.reading = Some(flight);
        }
        let Some(flight) = self.reading.as_ref() else {
            return Ok(None);
        };
        let sources = match flight.flight.reading.try_answer() {
            Err(TryRecvError::Empty) => {
                let limit = i64::try_from(SCRIPT_LIMIT.as_millis()).unwrap_or(i64::MAX);
                if now_ms.saturating_sub(flight.started_ms) > limit.saturating_add(RETRY_MS) {
                    return self.stop(now_ms, "it ran past its limit");
                }
                return Ok(None);
            }
            Err(TryRecvError::Disconnected) => {
                self.reading = None;
                self.retry(now_ms);
                return Err(format!(
                    "storage bytes: the reading ended without an answer; trying again in {} s",
                    RETRY_MS / 1000
                ));
            }
            Ok(Err(error)) => {
                self.reading = None;
                self.retry(now_ms);
                return Err(format!(
                    "storage bytes: {error}; trying again in {} s",
                    RETRY_MS / 1000
                ));
            }
            Ok(Ok(sources)) => sources,
        };
        self.reading = None;
        self.write(now_ms, sources, world)
    }

    /// Record the reading `sources` with the live database's size.
    fn write(
        &mut self,
        now_ms: i64,
        mut sources: Map<String, Value>,
        world: &mut impl World,
    ) -> Result<Option<String>, String> {
        sources.insert(LIVE_DATABASE.to_string(), world.live_database());
        let summary = summary(&sources);
        let record = json!({"type": "host.storage_bytes", "sources": sources});
        if let Err(error) = world.write(&record) {
            self.retry(now_ms);
            return Err(format!(
                "storage bytes: the reading could not be recorded: {error}; trying again in {} s",
                RETRY_MS / 1000
            ));
        }
        self.next_ms = now_ms.saturating_add(DAY_MS);
        Ok(Some(format!("storage bytes: recorded {summary}")))
    }

    /// [`Sampler::check_with`] through the store.
    pub(crate) fn check(
        &mut self,
        store: &PostgresStore,
        root: Option<&Path>,
        now_ms: i64,
    ) -> Result<Option<String>, String> {
        self.check_with(now_ms, &mut StoreWorld { store, root })
    }
}

/// The reading in words: each source's bytes, or that it is not measured.
fn summary(sources: &Map<String, Value>) -> String {
    sources
        .iter()
        .map(|(name, source)| match source["bytes"].as_u64() {
            Some(bytes) => format!("{name} {}", crate::disk::gib(bytes)),
            None => format!("{name} not measured"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests;
