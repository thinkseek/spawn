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
//!   projects; `$XDG_CACHE_HOME`, else `$HOME/.cache`, with Bun's install
//!   cache and pnpm's store when present (`build_cache_roots`); the Core
//!   lines file's `storage.scratch_roots` less its `storage.scratch_exclude`.
//!   A source with no root is not measured, and says so;
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

/// The build caches' roots, from the environment `env` gives: the XDG
/// cache (`$XDG_CACHE_HOME`, else `$HOME/.cache`, counted whether or not it
/// is there yet), Bun's install cache (`$BUN_INSTALL_CACHE_DIR`, else
/// `$HOME/.bun/install/cache`), pnpm's store (`$PNPM_HOME/store`, else
/// `$HOME/.local/share/pnpm/store`), and any opt-in shared host build cache
/// resolved via [`crate::runtime::resolve_shared_build_cache`] when present.
/// Each of those optional roots is counted only when it is a directory, and
/// each root once by its canonical path (a cache reached two ways, through a
/// symlink or a variable naming another's root, is counted once), in that
/// order.
#[cfg(test)]
pub(crate) fn build_cache_roots(env: impl Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    build_cache_roots_for(None, env)
}

/// Like [`build_cache_roots`], using explicit `workspace_root` (when supplied)
/// before falling back to `SPAWN_WORKSPACE_ROOT` in `env` to resolve opt-in
/// `<workspace_root>/.caches`.
pub(crate) fn build_cache_roots_for(
    workspace_root: Option<&Path>,
    env: impl Fn(&str) -> Option<String>,
) -> Vec<PathBuf> {
    let var = |name: &str| {
        env(name)
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from)
    };
    let home = var("HOME");
    let under_home = |rest: &str| home.as_ref().map(|home| home.join(rest));
    let xdg = var("XDG_CACHE_HOME").or_else(|| under_home(".cache"));
    let bun = var("BUN_INSTALL_CACHE_DIR").or_else(|| under_home(".bun/install/cache"));
    let pnpm = var("PNPM_HOME")
        .map(|pnpm| pnpm.join("store"))
        .or_else(|| under_home(".local/share/pnpm/store"));
    let effective_ws = workspace_root
        .map(Path::to_path_buf)
        .or_else(|| var("SPAWN_WORKSPACE_ROOT"));
    let shared = crate::runtime::resolve_shared_build_cache(&env, effective_ws.as_deref())
        .ok()
        .flatten();
    let present = |path: Option<PathBuf>| path.filter(|path| path.is_dir());
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    for root in [xdg, present(bun), present(pnpm), present(shared)]
        .into_iter()
        .flatten()
    {
        let canonical = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
        if seen.contains(&canonical) {
            continue;
        }
        seen.push(canonical);
        roots.push(root);
    }
    roots
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
/// caches' roots `cache` ([`build_cache_roots`]), and the scratch roots
/// `settings` names.
pub(crate) fn sources(
    root: Option<&Path>,
    projects: PathBuf,
    cache: Vec<PathBuf>,
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
            roots: cache,
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
            build_cache_roots_for(root, crate::env_value),
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

#[cfg(test)]
mod shared_cache_tests {
    use super::build_cache_roots_for;
    use std::path::PathBuf;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name: &str| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, val)| (*val).to_string())
                .filter(|val| !val.is_empty())
        }
    }

    #[test]
    fn shared_build_cache_roots_opt_in_and_dedup() {
        let base = std::env::temp_dir().join(format!(
            "spawn-sb-shared-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let ws = base.join("workspaces");
        let xdg = base.join("xdg");
        let shared_default = ws.join(".caches");
        let custom = base.join("custom-caches");
        std::fs::create_dir_all(&ws).expect("ws");
        std::fs::create_dir_all(&xdg).expect("xdg");
        std::fs::create_dir_all(&shared_default).expect("shared_default");
        std::fs::create_dir_all(&custom).expect("custom");

        // Default off: `<ws>/.caches` is ignored even though it exists on disk.
        assert_eq!(
            build_cache_roots_for(
                Some(&ws),
                env_of(&[("XDG_CACHE_HOME", xdg.to_str().expect("utf8"))])
            ),
            vec![xdg.clone()]
        );

        // Opt-in via `SPAWN_SHARED_BUILD_CACHES=1` with explicit workspace root.
        assert_eq!(
            build_cache_roots_for(
                Some(&ws),
                env_of(&[
                    ("XDG_CACHE_HOME", xdg.to_str().expect("utf8")),
                    ("SPAWN_SHARED_BUILD_CACHES", "1"),
                ])
            ),
            vec![xdg.clone(), shared_default.clone()]
        );

        // Opt-in via `SPAWN_WORKSPACE_ROOT` fallback when `workspace_root` arg is None.
        assert_eq!(
            build_cache_roots_for(
                None,
                env_of(&[
                    ("XDG_CACHE_HOME", xdg.to_str().expect("utf8")),
                    ("SPAWN_WORKSPACE_ROOT", ws.to_str().expect("utf8")),
                    ("SPAWN_SHARED_BUILD_CACHES", "true"),
                ])
            ),
            vec![xdg.clone(), shared_default.clone()]
        );

        // Explicit `SPAWN_SHARED_BUILD_CACHE_DIR` picks the custom dir instead of `.caches`.
        assert_eq!(
            build_cache_roots_for(
                Some(&ws),
                env_of(&[
                    ("XDG_CACHE_HOME", xdg.to_str().expect("utf8")),
                    (
                        "SPAWN_SHARED_BUILD_CACHE_DIR",
                        custom.to_str().expect("utf8")
                    ),
                ])
            ),
            vec![xdg.clone(), custom.clone()]
        );

        // Dedup: identical path to `XDG_CACHE_HOME` or symlink targeting `XDG_CACHE_HOME` counts once.
        let link = base.join("shared-link");
        std::os::unix::fs::symlink(&xdg, &link).expect("symlink");
        assert_eq!(
            build_cache_roots_for(
                Some(&ws),
                env_of(&[
                    ("XDG_CACHE_HOME", xdg.to_str().expect("utf8")),
                    ("SPAWN_SHARED_BUILD_CACHE_DIR", link.to_str().expect("utf8")),
                ])
            ),
            vec![xdg.clone()]
        );

        // Missing shared cache directory is not counted; invalid path is safely skipped.
        let missing_ws = base.join("missing-ws");
        assert_eq!(
            build_cache_roots_for(
                Some(&missing_ws),
                env_of(&[
                    ("XDG_CACHE_HOME", xdg.to_str().expect("utf8")),
                    ("SPAWN_SHARED_BUILD_CACHES", "1"),
                ])
            ),
            vec![xdg.clone()]
        );
        assert_eq!(
            build_cache_roots_for(
                Some(&ws),
                env_of(&[
                    ("XDG_CACHE_HOME", xdg.to_str().expect("utf8")),
                    ("SPAWN_SHARED_BUILD_CACHE_DIR", "/etc"),
                ])
            ),
            vec![xdg]
        );

        let _ = std::fs::remove_dir_all(&base);
        let _: Vec<PathBuf> = Vec::new();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn shared_cache_roots_empty_missing_zero_byte_and_symlink_boundaries() {
        let base = std::env::temp_dir().join(format!(
            "spawn-sb-edge-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let ws = base.join("ws");
        let xdg = base.join("xdg");
        let empty_cache = base.join("empty-cache");
        let zero_byte_cache = base.join("zero-byte-cache");
        let outside_target = base.join("outside-target");
        let broken_link = base.join("broken-link");
        let file_as_cache = base.join("regular-file.txt");

        std::fs::create_dir_all(&ws).expect("mkdir ws");
        std::fs::create_dir_all(&xdg).expect("mkdir xdg");
        std::fs::create_dir_all(&empty_cache).expect("mkdir empty_cache");
        std::fs::create_dir_all(&zero_byte_cache).expect("mkdir zero_byte_cache");
        std::fs::create_dir_all(&outside_target).expect("mkdir outside_target");
        std::fs::write(zero_byte_cache.join("zero.bin"), b"").expect("write zero.bin");
        std::fs::write(outside_target.join("data.bin"), b"outside").expect("write data.bin");
        std::fs::write(&file_as_cache, b"not a directory").expect("write file");

        // Symlink pointing to a missing destination (broken symlink)
        std::os::unix::fs::symlink(base.join("non-existent-target"), &broken_link)
            .expect("broken symlink");

        // 1. Missing cache directory: SPAWN_SHARED_BUILD_CACHE_DIR pointing to non-existent path is omitted
        assert_eq!(
            build_cache_roots_for(
                Some(&ws),
                env_of(&[
                    ("XDG_CACHE_HOME", xdg.to_str().expect("utf8")),
                    (
                        "SPAWN_SHARED_BUILD_CACHE_DIR",
                        base.join("does-not-exist").to_str().expect("utf8")
                    ),
                ])
            ),
            vec![xdg.clone()]
        );

        // 2. Missing default .caches directory with SPAWN_SHARED_BUILD_CACHES=1 is omitted
        assert_eq!(
            build_cache_roots_for(
                Some(&ws),
                env_of(&[
                    ("XDG_CACHE_HOME", xdg.to_str().expect("utf8")),
                    ("SPAWN_SHARED_BUILD_CACHES", "1"),
                ])
            ),
            vec![xdg.clone()]
        );

        // 3. Empty cache directory: directory exists but has 0 files -> counted as a valid root
        assert_eq!(
            build_cache_roots_for(
                Some(&ws),
                env_of(&[
                    ("XDG_CACHE_HOME", xdg.to_str().expect("utf8")),
                    (
                        "SPAWN_SHARED_BUILD_CACHE_DIR",
                        empty_cache.to_str().expect("utf8")
                    ),
                ])
            ),
            vec![xdg.clone(), empty_cache.clone()]
        );

        // 4. Zero-byte files in cache directory: counted as valid root
        assert_eq!(
            build_cache_roots_for(
                Some(&ws),
                env_of(&[
                    ("XDG_CACHE_HOME", xdg.to_str().expect("utf8")),
                    (
                        "SPAWN_SHARED_BUILD_CACHE_DIR",
                        zero_byte_cache.to_str().expect("utf8")
                    ),
                ])
            ),
            vec![xdg.clone(), zero_byte_cache.clone()]
        );

        // 5. Broken symlink: is_dir() is false, safely omitted without error
        assert_eq!(
            build_cache_roots_for(
                Some(&ws),
                env_of(&[
                    ("XDG_CACHE_HOME", xdg.to_str().expect("utf8")),
                    (
                        "SPAWN_SHARED_BUILD_CACHE_DIR",
                        broken_link.to_str().expect("utf8")
                    ),
                ])
            ),
            vec![xdg.clone()]
        );

        // 6. Regular file passed as cache root: is_dir() is false, safely omitted
        assert_eq!(
            build_cache_roots_for(
                Some(&ws),
                env_of(&[
                    ("XDG_CACHE_HOME", xdg.to_str().expect("utf8")),
                    (
                        "SPAWN_SHARED_BUILD_CACHE_DIR",
                        file_as_cache.to_str().expect("utf8")
                    ),
                ])
            ),
            vec![xdg.clone()]
        );

        // 7. Symlink loop / cycle safety: recursive symlink
        let loop_link = base.join("loop-link");
        std::os::unix::fs::symlink(&loop_link, &loop_link).expect("symlink cycle");
        assert_eq!(
            build_cache_roots_for(
                Some(&ws),
                env_of(&[
                    ("XDG_CACHE_HOME", xdg.to_str().expect("utf8")),
                    (
                        "SPAWN_SHARED_BUILD_CACHE_DIR",
                        loop_link.to_str().expect("utf8")
                    ),
                ])
            ),
            vec![xdg.clone()]
        );

        // 8. Symlink pointing to outside target: resolved and deduplicated
        let symlink_to_outside = ws.join(".caches");
        std::os::unix::fs::symlink(&outside_target, &symlink_to_outside).expect("symlink outside");
        let roots = build_cache_roots_for(
            Some(&ws),
            env_of(&[
                ("XDG_CACHE_HOME", xdg.to_str().expect("utf8")),
                ("SPAWN_SHARED_BUILD_CACHES", "1"),
            ]),
        );
        assert_eq!(roots, vec![xdg.clone(), symlink_to_outside]);

        // If another cache names the same canonical directory, it is deduplicated
        let dup_roots = build_cache_roots_for(
            Some(&ws),
            env_of(&[
                ("XDG_CACHE_HOME", xdg.to_str().expect("utf8")),
                (
                    "BUN_INSTALL_CACHE_DIR",
                    outside_target.to_str().expect("utf8"),
                ),
                ("SPAWN_SHARED_BUILD_CACHES", "1"),
            ]),
        );
        assert_eq!(
            dup_roots,
            vec![xdg.clone(), outside_target.clone()],
            "symlink pointing to outside_target is dropped since bun already resolved to it"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn storage_bytes_json_field_stability_and_zero_byte_accounting() {
        use super::{
            BUILD_CACHE, LIVE_DATABASE, SCRATCH, SESSION_BACKUPS, Source, TEST_DATABASES,
            TRANSCRIPTS, live_database, script_input, script_sources, session_backups,
            test_databases,
        };
        use serde_json::json;

        let src_transcripts = Source {
            name: TRANSCRIPTS,
            roots: vec![PathBuf::from("/srv/transcripts")],
            exclude: Vec::new(),
        };
        let src_build_cache = Source {
            name: BUILD_CACHE,
            roots: vec![PathBuf::from("/srv/cache")],
            exclude: Vec::new(),
        };
        let src_scratch = Source {
            name: SCRATCH,
            roots: Vec::new(),
            exclude: Vec::new(),
        };
        let sources_list = vec![
            src_transcripts.clone(),
            src_build_cache.clone(),
            src_scratch.clone(),
        ];

        // 1. Script input schema stability
        let input = script_input(&sources_list, 1200);
        assert_eq!(input["bound_secs"], 1200);
        let src_arr = input["sources"].as_array().expect("sources array");
        assert_eq!(src_arr.len(), 2, "scratch with empty roots is omitted");
        assert_eq!(src_arr[0]["name"], "transcripts");
        assert_eq!(src_arr[0]["roots"], json!(["/srv/transcripts"]));
        assert_eq!(src_arr[0]["exclude"], json!([]));
        assert_eq!(src_arr[1]["name"], "build_cache");
        assert_eq!(src_arr[1]["roots"], json!(["/srv/cache"]));

        // 2. Script answer handling: zero-byte files & empty dirs
        let answer_zero = Ok(json!({
            "sources": [
                {
                    "name": "transcripts",
                    "bytes": 0,
                    "files": 0,
                    "unreadable": 0,
                    "seconds": 0.05
                },
                {
                    "name": "build_cache",
                    "bytes": 0,
                    "files": 4,
                    "unreadable": 0,
                    "seconds": 0.12
                }
            ]
        }));
        let sources_map = script_sources(&sources_list, &answer_zero);
        assert_eq!(
            sources_map["transcripts"],
            json!({"bytes": 0, "files": 0, "unreadable": 0, "seconds": 0.05})
        );
        assert_eq!(
            sources_map["build_cache"],
            json!({"bytes": 0, "files": 4, "unreadable": 0, "seconds": 0.12})
        );
        assert_eq!(
            sources_map["scratch"],
            json!({"not_measured": "no root to read"})
        );

        // 3. Script error & over_bound stability
        let answer_err = Err("killed by signal".to_string());
        let err_map = script_sources(&sources_list, &answer_err);
        assert_eq!(
            err_map["transcripts"],
            json!({"not_measured": "the extents script: killed by signal"})
        );
        assert_eq!(
            err_map["scratch"],
            json!({"not_measured": "no root to read"})
        );

        let answer_over_bound = Ok(json!({
            "sources": [
                {
                    "name": "transcripts",
                    "over_bound": true,
                    "files": 999
                }
            ]
        }));
        let ob_map = script_sources(&sources_list, &answer_over_bound);
        assert_eq!(
            ob_map["transcripts"],
            json!({"not_measured": "over its bound of 1200 s (999 files read)"})
        );
        assert_eq!(
            ob_map["build_cache"],
            json!({"not_measured": "the extents script gave no answer for it"})
        );

        // 4. Session backups & test databases & live database field stability
        let tmp = std::env::temp_dir().join(format!("spawn-sb-session-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let status_file = tmp.join("status.json");
        std::fs::write(&status_file, r#"{"estimated_bytes": 0}"#).expect("write status");

        let sb_zero = session_backups(Some(&status_file));
        assert_eq!(sb_zero, json!({"bytes": 0}));

        let sb_none = session_backups(None);
        assert_eq!(
            sb_none,
            json!({"not_measured": "no status file is configured"})
        );

        let db_entry = live_database(Ok(json!({"bytes": 0})));
        assert_eq!(db_entry, json!({"bytes": 0}));

        let test_db_entry = test_databases(
            &[
                ("role1".to_string(), "url1".to_string()),
                ("role2".to_string(), "url2".to_string()),
            ],
            |url| {
                if url == "url1" {
                    Ok(json!({"name": "db_test", "bytes": 0}))
                } else {
                    Ok(json!({"name": "db_test", "bytes": 100}))
                }
            },
        );
        assert_eq!(test_db_entry, json!({"bytes": 0, "databases": 1}));

        // 5. Complete host.storage_bytes record schema stability
        let mut full_sources = sources_map;
        full_sources.insert(SESSION_BACKUPS.to_string(), sb_zero);
        full_sources.insert(TEST_DATABASES.to_string(), test_db_entry);
        full_sources.insert(LIVE_DATABASE.to_string(), db_entry);

        let record = json!({
            "type": "host.storage_bytes",
            "sources": full_sources,
        });
        assert_eq!(record["type"], "host.storage_bytes");
        let rec_sources = record["sources"].as_object().expect("sources object");
        for expected_key in [
            TRANSCRIPTS,
            BUILD_CACHE,
            SCRATCH,
            SESSION_BACKUPS,
            TEST_DATABASES,
            LIVE_DATABASE,
        ] {
            assert!(
                rec_sources.contains_key(expected_key),
                "missing key: {expected_key}"
            );
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
