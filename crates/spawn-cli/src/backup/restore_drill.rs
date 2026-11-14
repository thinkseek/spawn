//! `spawn backup restore-drill`: the transcript archive and the mission
//! repositories restored from R2 into scratch, and compared with live
//! (core (ii); sync/recovery.md's two "NONE" rows).
//!
//! `spawn backup verify` proves the newest bundle opens: its manifest, its
//! file sizes, its git bundles' heads. This proves the backup gives the
//! resources back. It takes the newest stored bundle and the chain of
//! increments back to its full bundle (each record's `base_event_id`), and
//! applies them in order into a scratch directory of its own:
//!
//! - every transcript slice at its offset (a whole file at offset 0, an
//!   append at the size the chain has restored so far), each file then the
//!   size the newest record names for it;
//! - every mission source as a `git clone --mirror` of its full bundle, each
//!   increment's bundle fetched on top, its refs set to the newest record's
//!   heads and `git fsck --connectivity-only` run on it.
//!
//! Each restored transcript is first judged against the newest record
//! itself (objective 149): `transcripts.sha256` names the sha256 of every
//! window file as bundled, so a restored file whose sha256 is not the
//! record's, or a file the record sizes without one, is a mismatch, and the
//! drill says "<n> of <m> files as the newest record's sha256". A record
//! bundled before 149 names no map: that is said, and its files are judged
//! against live only, with no mismatch of the record's own.
//!
//! Then each is compared with live, read-only:
//!
//! - a transcript file is the same size and sha256 as the live one, or the
//!   live one is longer and its first bytes are the restored file (it grew
//!   since the bundle: expected). A live file the backup lacks was made
//!   after the bundle (expected), or was last written before the bundle's
//!   window (`SPAWN_BACKUP_TRANSCRIPT_DAYS`): in no bundle, so with no
//!   off-host copy once the bundles that held it expire, said apart and
//!   counted, not a failure (the window is the owner's question). Anything
//!   else is a mismatch: a live file shorter than its restored copy, with
//!   other bytes, gone, or written within the window and not in the backup;
//! - a ref is at the same commit as live, or live moved it to a descendant
//!   (newer: expected); a live ref the backup lacks was made since, and a
//!   restored ref live no longer has was removed since (both said, both
//!   expected). A live ref at a commit that does not descend from the
//!   restored one is a mismatch. Each restored head is first read through
//!   the history rewrite's commit map (objective 178), so a head bundled
//!   before the rewrite is compared by its new id; a head the rewrite
//!   dropped is said on its own line and counted on the source's, never a
//!   failure; a head in neither the map nor live is an "unknown commit": it
//!   fails the drill (counted in the record's `mismatches`), never as a
//!   mismatch.
//!
//! One summary line per resource, then the verdict: exit 0 when nothing
//! older than the bundle differs, 1 on any such mismatch (or a backup that
//! cannot be restored: a broken chain, a sha256 that is not the record's), 2
//! when the drill cannot run.
//!
//! Once it ran (exit 0 or 1) it records its verdict per resource through
//! `spawn_backup_record` (objective 104): `backup.drilled.transcripts` and
//! `backup.drilled.missions`, each exactly `{type, ok, reason, bundle,
//! bundle_event_id, mismatches, duration_ms}`: the newest stored bundle's
//! key and record, that resource's count of mismatches older than it, ok
//! with reason "" when it has none, else a reason of at most 500
//! characters. A backup that cannot be restored (a broken chain included)
//! fails both, with its reason and no count. A drill that cannot run (exit
//! 2) records nothing; a record the store refuses is said and exits 2. The
//! Core lines recovery rows read them (`record`). Otherwise it writes
//! nothing live: no lock, git and the files read only.
//!
//! The chain is read with the key its records name (objective 114): every
//! link must name the newest link's key (a chain that crosses keys cannot
//! be restored, exit 1), and each object's `x-amz-meta-spawn-seal` must say
//! it too (absent: k1). A link whose `sealed_with` is not a key name, or a
//! chain key whose file is not on this host, fails both resources with that
//! reason, exit 2. The key file is given to gpg by path (mode 600) and
//! nothing of it is printed; a refused name is never echoed. The scratch
//! directory is made with mode 700 under `--into`
//! (default `/data/tmp`), which may not be under `$HOME`, `$SPAWN_LIVE` or
//! the workspace root; it is removed at the end (kept with `--keep`), and
//! only once it is checked to be the directory this run made.
//!
//! deploy/systemd/spawn-backup-restore-drill.{service,timer} run it daily
//! (05:30) with the bundle unit's environment, so it reads the bundle
//! records from, and records into, the store the bundle writes to. The
//! removal above is by name after a check: until objective 149b empties
//! the directory through the fd held from its creation, the drill runs
//! only by hand, attended, and the timer is not installed
//! (deploy/backup/README.md).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Instant, SystemTime};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{
    BundleRecord, Key, Settings, bundle_records, decompress_bundle, decrypt_with, get_object_bytes,
    header_value, heads_of, hex, object_key, output, random_hex, reading_key, record_key, same_key,
    sha256_file,
};
use crate::Flags;
use crate::commit_now::CommitNow;
use crate::config::{env_value, open_store};

/// Where the scratch directory goes when `--into` is not given.
const DEFAULT_PARENT: &str = "/data/tmp";

/// The scratch directory's name starts with this.
const DRILL_PREFIX: &str = "spawn-restore-drill-";

/// How many mismatches are named one by one.
const NAMED: usize = 20;

/// `spawn backup restore-drill [--root <dir>] [--into <dir>] [--keep]`.
pub(super) fn restore_drill(flags: &Flags) -> Result<i32, String> {
    let settings = match Settings::from_env(flags) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("spawn backup restore-drill: {error}");
            return Ok(2);
        }
    };
    let parent = flags
        .get("into")
        .map_or_else(|| PathBuf::from(DEFAULT_PARENT), PathBuf::from);
    let guards: Vec<(&str, PathBuf)> = [
        ("$HOME", env_value("HOME").map(PathBuf::from)),
        ("$SPAWN_LIVE", env_value("SPAWN_LIVE").map(PathBuf::from)),
        ("the workspace root", Some(settings.root.clone())),
    ]
    .into_iter()
    .filter_map(|(name, path)| Some((name, path?)))
    .collect();
    let parent = match drill_parent(&parent, &guards) {
        Ok(parent) => parent,
        Err(error) => {
            eprintln!("spawn backup restore-drill: {error}");
            return Ok(2);
        }
    };
    let started = Instant::now();
    let store = open_store()?;
    let now_ms = store.now_ms().map_err(|error| error.to_string())?;
    let records = bundle_records(&store, &settings.prefix, now_ms)?;
    let chain = match chain_of(&records) {
        Ok(chain) => chain,
        Err(Chain::None) => {
            eprintln!("spawn backup restore-drill: no backup.bundled record with a stored key");
            return Ok(2);
        }
        Err(Chain::Broken(reason)) => {
            return Ok(unrestorable(&store, &records, &reason, started));
        }
    };
    if let Some(newest) = records.last()
        && newest.event_id != chain[chain.len() - 1].event_id
    {
        println!(
            "restore-drill: the newest bundle record ({}) stored nothing; drilling the newest \
             stored one ({})",
            newest.event_id,
            chain[chain.len() - 1].event_id
        );
    }
    // One key reads the whole chain, and its file must be here.
    let seal = match chain_key(&chain) {
        Ok(name) => match reading_key(&name, "the bundle chain") {
            Ok(seal) => seal,
            Err(reason) => return Ok(fail_both(&store, &records, &reason, started, 2)),
        },
        Err(ChainKey::Refused(reason)) => {
            return Ok(fail_both(&store, &records, &reason, started, 2));
        }
        Err(ChainKey::Crossed(reason)) => {
            return Ok(unrestorable(&store, &records, &reason, started));
        }
    };
    let drill = DrillDir::create(&parent)?;
    let result = run(&store, &settings, &chain, &seal, drill.path());
    let newest = chain[chain.len() - 1];
    let (code, outcomes) = match result {
        Ok(report) => (said(&report), outcomes(&report)),
        Err(error) => {
            let reason = format!("the backup cannot be restored: {error}");
            println!("restore-drill: FAILED: {reason}");
            (
                1,
                [
                    (TRANSCRIPTS, Outcome::Unrestorable(reason.clone())),
                    (MISSIONS, Outcome::Unrestorable(reason)),
                ],
            )
        }
    };
    if flags.switch("keep") {
        println!("restore-drill: kept {}", drill.path().display());
        drill.keep();
    } else {
        drill.remove()?;
    }
    let key = bundle_key(newest);
    let verdict = Verdict {
        key: &key,
        event_id: newest.event_id,
        duration_ms: elapsed_ms(started),
    };
    Ok(record_verdicts(&store, &verdict, &outcomes).unwrap_or(code))
}

/// The two resources a drill proves, each recorded as `backup.drilled.<name>`.
const TRANSCRIPTS: &str = "transcripts";
const MISSIONS: &str = "missions";

/// The longest reason a drill record takes (`spawn_backup_record`).
const REASON_CHARS: usize = 500;

/// One resource's outcome.
#[derive(Debug, Clone)]
enum Outcome {
    /// Restored, and nothing older than the bundle differs.
    Passed,
    /// Restored, with this many mismatches; the reason names the first.
    Mismatched(usize, String),
    /// The backup cannot be restored, for this reason.
    Unrestorable(String),
}

/// What every record of one drill names.
struct Verdict<'a> {
    key: &'a str,
    event_id: i64,
    duration_ms: i64,
}

/// A chain that lacks a link: said, and both resources recorded failed
/// against the newest stored record (`chain_of` found one); exit 1, or 2
/// when the store refused a record.
fn unrestorable(
    store: &spawn_postgres::PostgresStore,
    records: &[BundleRecord],
    reason: &str,
    started: Instant,
) -> i32 {
    let reason = format!("the backup cannot be restored: {reason}");
    fail_both(store, records, &reason, started, 1)
}

/// Both resources recorded failed with `reason`, as it is, against the
/// newest stored record: exit `code`, or 2 when the store refused a record.
/// A drill that cannot read its chain (objective 114: a link's key name
/// refused, the chain key's file not on this host) exits 2 this way.
fn fail_both(
    store: &spawn_postgres::PostgresStore,
    records: &[BundleRecord],
    reason: &str,
    started: Instant,
    code: i32,
) -> i32 {
    let reason = reason.to_string();
    println!("restore-drill: FAILED: {reason}");
    let newest = records.iter().rev().find(|record| stored(record));
    let (key, event_id) = newest.map_or((String::new(), 0), |record| {
        (bundle_key(record), record.event_id)
    });
    let verdict = Verdict {
        key: &key,
        event_id,
        duration_ms: elapsed_ms(started),
    };
    let both = [
        (TRANSCRIPTS, Outcome::Unrestorable(reason.clone())),
        (MISSIONS, Outcome::Unrestorable(reason)),
    ];
    record_verdicts(store, &verdict, &both).unwrap_or(code)
}

/// Why a chain has no one key to be read with.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ChainKey {
    /// A link's `sealed_with` is not a key name: the reason names the
    /// record, never the value.
    Refused(String),
    /// Two links name two keys.
    Crossed(String),
}

/// The one key every link of `chain` (oldest first) names, the newest
/// link's (objective 114).
pub(super) fn chain_key(chain: &[&BundleRecord]) -> Result<String, ChainKey> {
    let names = chain
        .iter()
        .map(|link| record_key("backup.bundled", link.event_id, &link.record))
        .collect::<Result<Vec<String>, String>>()
        .map_err(ChainKey::Refused)?;
    let (Some(newest), Some(last)) = (names.last(), chain.last()) else {
        return Err(ChainKey::Crossed("the chain has no link".to_string()));
    };
    match chain.iter().zip(&names).find(|(_, name)| *name != newest) {
        None => Ok(newest.clone()),
        Some((link, name)) => Err(ChainKey::Crossed(format!(
            "the chain crosses keys: bundle {} is sealed with {name}, the newest ({}) with {newest}",
            link.event_id, last.event_id
        ))),
    }
}

/// The report said: its lines, its first mismatches, the verdict; the exit.
fn said(report: &Report) -> i32 {
    for line in &report.lines {
        println!("{line}");
    }
    let mismatches: Vec<&String> = report.transcripts.iter().chain(&report.missions).collect();
    for line in mismatches.iter().take(NAMED) {
        eprintln!("restore-drill: mismatch: {line}");
    }
    if mismatches.len() > NAMED {
        eprintln!(
            "restore-drill: and {} more mismatches",
            mismatches.len() - NAMED
        );
    }
    for line in report.unknown.iter().take(NAMED) {
        eprintln!("restore-drill: unknown commit: {line}");
    }
    if report.unknown.len() > NAMED {
        eprintln!(
            "restore-drill: and {} more unknown commits",
            report.unknown.len() - NAMED
        );
    }
    if mismatches.is_empty() && report.unknown.is_empty() {
        println!(
            "restore-drill: passed: bundle {} (event {}) restored and matches live",
            report.key, report.event_id
        );
        0
    } else {
        println!(
            "restore-drill: FAILED: {} older than bundle {} (event {})",
            counted(mismatches.len(), report.unknown.len()),
            report.key,
            report.event_id
        );
        1
    }
}

/// "N mismatch(es)", and " and M unknown commit(s)" when there is one.
fn counted(mismatches: usize, unknown: usize) -> String {
    if unknown == 0 {
        format!("{mismatches} mismatch(es)")
    } else {
        format!("{mismatches} mismatch(es) and {unknown} unknown commit(s)")
    }
}

/// Each resource's outcome from a drill that ran.
fn outcomes(report: &Report) -> [(&'static str, Outcome); 2] {
    [
        (
            TRANSCRIPTS,
            outcome(TRANSCRIPTS, &report.transcripts, &[], &report.key),
        ),
        (
            MISSIONS,
            outcome(MISSIONS, &report.missions, &report.unknown, &report.key),
        ),
    ]
}

/// A resource's outcome from its mismatches and its unknown commits
/// (objective 178: a restored head in neither the commit map nor live):
/// none passes; any fails, its count both, its reason each count (an
/// unknown commit never called a mismatch) and the first.
fn outcome(resource: &str, mismatches: &[String], unknown: &[String], key: &str) -> Outcome {
    match mismatches.first().or_else(|| unknown.first()) {
        None => Outcome::Passed,
        Some(first) => Outcome::Mismatched(
            mismatches.len() + unknown.len(),
            format!(
                "{resource}: {} older than bundle {key}; the first: {first}",
                counted(mismatches.len(), unknown.len())
            ),
        ),
    }
}

/// One `backup.drilled.<resource>` record per outcome (objective 104).
/// `Some(2)` when the store refused one (said on stderr), else `None`.
fn record_verdicts(
    store: &spawn_postgres::PostgresStore,
    verdict: &Verdict<'_>,
    outcomes: &[(&str, Outcome)],
) -> Option<i32> {
    let mut refused = None;
    for (resource, outcome) in outcomes {
        let record = drill_record(resource, outcome, verdict);
        if let Err(error) = store.backup_record(&record) {
            eprintln!(
                "spawn backup restore-drill: backup.drilled.{resource} was not recorded: {error}"
            );
            refused = Some(2);
        }
    }
    refused
}

/// A drill record, exactly `{type, ok, reason, bundle, bundle_event_id,
/// mismatches, duration_ms}`: ok with reason "" when the resource has no
/// mismatch; a backup that cannot be restored counts none.
fn drill_record(resource: &str, outcome: &Outcome, verdict: &Verdict<'_>) -> Value {
    let (ok, reason, mismatches) = match outcome {
        Outcome::Passed => (true, "", 0),
        Outcome::Mismatched(count, reason) => (false, reason.as_str(), *count),
        Outcome::Unrestorable(reason) => (false, reason.as_str(), 0),
    };
    json!({
        "type": format!("backup.drilled.{resource}"),
        "ok": ok,
        "reason": reason.chars().take(REASON_CHARS).collect::<String>(),
        "bundle": verdict.key,
        "bundle_event_id": verdict.event_id,
        "mismatches": mismatches,
        "duration_ms": verdict.duration_ms,
    })
}

/// The parent of the scratch directory, resolved: an existing directory
/// under none of `guards` (each resolved too, so a symlink into one is
/// refused). A guard that does not exist guards nothing.
pub(super) fn drill_parent(parent: &Path, guards: &[(&str, PathBuf)]) -> Result<PathBuf, String> {
    if !parent.is_absolute() {
        return Err(format!(
            "--into {} is not an absolute path",
            parent.display()
        ));
    }
    let resolved = parent
        .canonicalize()
        .map_err(|error| format!("--into {}: {error}", parent.display()))?;
    if !resolved.is_dir() {
        return Err(format!("--into {} is not a directory", parent.display()));
    }
    for (name, guard) in guards {
        let Ok(guard) = guard.canonicalize() else {
            continue;
        };
        if resolved.starts_with(&guard) {
            return Err(format!(
                "--into {} is under {name} ({}): a drill restores into scratch, never there",
                parent.display(),
                guard.display()
            ));
        }
    }
    Ok(resolved)
}

/// The directory this run made, and what it was when made: it is removed
/// only while it is still that directory.
pub(super) struct DrillDir {
    path: PathBuf,
    parent: PathBuf,
    #[cfg(unix)]
    identity: (u64, u64),
    done: bool,
}

impl DrillDir {
    /// A new directory `<parent>/spawn-restore-drill-<pid>-<hex>`, mode 700,
    /// made here (never one that was there).
    pub(super) fn create(parent: &Path) -> Result<Self, String> {
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        for _ in 0..10 {
            let path = parent.join(format!(
                "{DRILL_PREFIX}{}-{}",
                std::process::id(),
                random_hex(6)?
            ));
            match builder.create(&path) {
                Ok(()) => {
                    let meta = fs::symlink_metadata(&path)
                        .map_err(|error| format!("{}: {error}", path.display()))?;
                    #[cfg(unix)]
                    let identity = {
                        use std::os::unix::fs::MetadataExt;
                        (meta.dev(), meta.ino())
                    };
                    #[cfg(not(unix))]
                    let _ = meta;
                    return Ok(Self {
                        path,
                        parent: parent.to_path_buf(),
                        #[cfg(unix)]
                        identity,
                        done: false,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(format!("{}: {error}", path.display())),
            }
        }
        Err(format!(
            "cannot make a scratch directory in {}",
            parent.display()
        ))
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    /// Left as it is, for a person to look at.
    fn keep(mut self) {
        self.done = true;
    }

    /// Removed, once checked: its parent is the one it was made in, its
    /// name is a drill's, and it is still the directory made then (not a
    /// symlink, the same device and inode). Anything else is left, and
    /// said.
    pub(super) fn remove(mut self) -> Result<(), String> {
        self.done = true;
        let named = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(DRILL_PREFIX));
        let meta = fs::symlink_metadata(&self.path)
            .map_err(|error| format!("{}: {error}", self.path.display()))?;
        #[cfg(unix)]
        let same = {
            use std::os::unix::fs::MetadataExt;
            (meta.dev(), meta.ino()) == self.identity
        };
        #[cfg(not(unix))]
        let same = true;
        if !named || self.path.parent() != Some(self.parent.as_path()) || !meta.is_dir() || !same {
            return Err(format!(
                "{} is not the scratch directory this drill made: left as it is",
                self.path.display()
            ));
        }
        fs::remove_dir_all(&self.path).map_err(|error| format!("{}: {error}", self.path.display()))
    }
}

impl Drop for DrillDir {
    fn drop(&mut self) {
        if !self.done {
            // An error left it before the end: removed with the same checks.
            let left = Self {
                path: self.path.clone(),
                parent: self.parent.clone(),
                #[cfg(unix)]
                identity: self.identity,
                done: false,
            };
            self.done = true;
            if let Err(error) = left.remove() {
                eprintln!("restore-drill: {error}");
            }
        }
    }
}

/// Whether a bundle record stored something (a key in the bucket).
fn stored(record: &BundleRecord) -> bool {
    record.record["keys"]
        .as_array()
        .is_some_and(|keys| !keys.is_empty())
}

/// A bundle record's key in the bucket (its first).
fn bundle_key(record: &BundleRecord) -> String {
    record.record["keys"][0]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// The milliseconds since `started`.
fn elapsed_ms(started: Instant) -> i64 {
    i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX)
}

/// Why there is no chain to restore.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Chain {
    /// No record stored anything.
    None,
    /// The newest stored bundle's chain lacks a link.
    Broken(String),
}

/// The newest stored bundle and its bases back to a full one, oldest first.
pub(super) fn chain_of(records: &[BundleRecord]) -> Result<Vec<&BundleRecord>, Chain> {
    let Some(newest) = records.iter().rev().find(|record| stored(record)) else {
        return Err(Chain::None);
    };
    let by_id: BTreeMap<i64, &BundleRecord> = records
        .iter()
        .map(|record| (record.event_id, record))
        .collect();
    let mut chain = vec![newest];
    let mut at = newest;
    while at.record["kind"].as_str() != Some("full") {
        let Some(base) = at.record["base_event_id"].as_i64() else {
            return Err(Chain::Broken(format!(
                "the increment {} names no base",
                at.event_id
            )));
        };
        let Some(next) = by_id.get(&base) else {
            return Err(Chain::Broken(format!(
                "the increment {} has its base {base}, which no record of this prefix is",
                at.event_id
            )));
        };
        if !stored(next) {
            return Err(Chain::Broken(format!(
                "the increment {} has its base {base}, which stored nothing (skipped: {})",
                at.event_id, next.record["skipped"]
            )));
        }
        if chain.len() > records.len() {
            return Err(Chain::Broken("the bases go round in a circle".to_string()));
        }
        chain.push(next);
        at = next;
    }
    chain.reverse();
    Ok(chain)
}

/// What the drill found.
struct Report {
    key: String,
    event_id: i64,
    lines: Vec<String>,
    /// The transcript archive's mismatches.
    transcripts: Vec<String>,
    /// The mission repositories' mismatches.
    missions: Vec<String>,
    /// The mission repositories' restored heads in neither the commit map
    /// nor live (objective 178): they fail the drill, never as mismatches.
    unknown: Vec<String>,
}

/// Fetch, check and unpack every bundle of the chain, restore, compare.
fn run(
    store: &spawn_postgres::PostgresStore,
    settings: &Settings,
    chain: &[&BundleRecord],
    seal: &Key,
    drill: &Path,
) -> Result<Report, String> {
    let newest = chain[chain.len() - 1];
    let mut unpacked: Vec<(PathBuf, Value)> = Vec::new();
    for (index, record) in chain.iter().enumerate() {
        let here = drill.join(format!("bundle-{index}"));
        fs::create_dir(&here).map_err(|error| format!("{}: {error}", here.display()))?;
        let manifest = fetch_bundle(settings, record, seal, &here)?;
        unpacked.push((here.join("unpacked"), manifest));
    }
    let key = bundle_key(newest);
    let bundle_ms = newest.at_ms;
    let mut lines = Vec::new();
    let mut mismatches = Vec::new();
    let mut missions = Vec::new();
    let chain_text = chain
        .iter()
        .map(|record| {
            format!(
                "{} {}",
                record.record["kind"].as_str().unwrap_or("?"),
                record.event_id
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    lines.push(format!(
        "restore-drill: chain {chain_text}: {} bundle(s) fetched, sha256 as recorded, decrypted",
        chain.len()
    ));

    let restored = drill.join("transcripts");
    fs::create_dir(&restored).map_err(|error| format!("{}: {error}", restored.display()))?;
    restore_transcripts(&unpacked, &restored)?;
    let sizes = newest.record["transcripts"]["sizes"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    for (path, size) in &sizes {
        let want = size.as_u64().unwrap_or(0);
        match fs::metadata(restored.join(path)) {
            Ok(meta) if meta.len() == want => {}
            Ok(meta) => mismatches.push(format!(
                "transcripts/{path}: the chain restores {} bytes; the newest record says {want}",
                meta.len()
            )),
            Err(_) => mismatches.push(format!(
                "transcripts/{path}: the newest record names it ({want} bytes); the chain restores none"
            )),
        }
    }
    let (record_line, against_record) = judge_by_record(&restored, &newest.record["transcripts"])?;
    lines.push(record_line);
    mismatches.extend(against_record);
    let window_ms = settings.transcript_days.saturating_mul(86_400_000);
    let live = settings.root.join(".transcripts");
    let transcripts = compare_transcripts(&restored, &live, bundle_ms, window_ms)?;
    lines.push(transcripts.line(&live));
    mismatches.extend(transcripts.mismatches);

    let mut unknown = Vec::new();
    let found = Found {
        lines: &mut lines,
        mismatches: &mut missions,
        unknown: &mut unknown,
    };
    restore_repos(store, newest, &unpacked, drill, found)?;
    Ok(Report {
        key,
        event_id: newest.event_id,
        lines,
        transcripts: mismatches,
        missions,
        unknown,
    })
}

/// Every mission source of the newest record restored under `drill/repos`
/// and compared with live: a line each, and the mismatches. A source the
/// newest bundle could not read (`missing`) is one: the backup holds none
/// of it. Each restored head is read through the commit map (objective
/// 178): one the rewrite dropped is said on its own line, one in neither
/// the map nor live is an unknown commit.
fn restore_repos(
    store: &spawn_postgres::PostgresStore,
    newest: &BundleRecord,
    unpacked: &[(PathBuf, Value)],
    drill: &Path,
    found: Found<'_>,
) -> Result<(), String> {
    let Found {
        lines,
        mismatches,
        unknown,
    } = found;
    let repos = drill.join("repos");
    fs::create_dir(&repos).map_err(|error| format!("{}: {error}", repos.display()))?;
    let newest_repos = newest.record["repos"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for (index, repo) in newest_repos.iter().enumerate() {
        let Some(source) = repo["source"].as_str() else {
            continue;
        };
        let heads: BTreeMap<String, String> = repo["heads"]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(name, commit)| Some((name.clone(), commit.as_str()?.to_string())))
            .collect();
        let mirror = repos.join(format!("repo-{index}.git"));
        if let Err(error) = restore_repo(source, unpacked, &heads, &mirror) {
            lines.push(format!(
                "restore-drill: missions {source}: not restored: {error}"
            ));
            mismatches.push(format!("{source}: not restored: {error}"));
            continue;
        }
        let live_heads = match heads_of(source) {
            Ok(live_heads) => live_heads,
            Err(error) => {
                lines.push(format!(
                    "restore-drill: missions {source}: {} refs restored; live cannot be read: {error}",
                    heads.len()
                ));
                mismatches.push(format!("{source}: live cannot be read: {error}"));
                continue;
            }
        };
        let refs = compare_refs(
            &heads,
            &live_heads,
            |commit| {
                crate::commit_now::commit_now(store, Path::new(source), commit)
                    .map_err(|error| error.to_string())
            },
            |old, new| descends(source, old, new),
        );
        let refs = match refs {
            Ok(refs) => refs,
            Err(error) => {
                lines.push(format!(
                    "restore-drill: missions {source}: {} refs restored; the commit map cannot \
                     be read: {error}",
                    heads.len()
                ));
                mismatches.push(format!("{source}: the commit map cannot be read: {error}"));
                continue;
            }
        };
        lines.push(refs.line(source));
        lines.extend(
            refs.dropped
                .iter()
                .map(|dropped| format!("restore-drill: missions {source}: {dropped}")),
        );
        mismatches.extend(
            refs.mismatches
                .iter()
                .map(|mismatch| format!("{source}: {mismatch}")),
        );
        unknown.extend(refs.unknown.iter().map(|head| format!("{source}: {head}")));
    }
    for source in newest.record["missing"].as_array().into_iter().flatten() {
        mismatches.push(format!(
            "{}: the bundle could not read it, so it holds none of it",
            source.as_str().unwrap_or("?")
        ));
    }
    Ok(())
}

/// One stored bundle: fetched into `here`, its sha256 the record's (the
/// object's metadata and the decrypted bytes), decompressed by its magic
/// and unpacked into `here/unpacked`; its manifest's kind the record's. It
/// is opened with `seal`, the chain's key, which its object must say too.
fn fetch_bundle(
    settings: &Settings,
    record: &BundleRecord,
    seal: &Key,
    here: &Path,
) -> Result<Value, String> {
    let key = record.record["keys"][0].as_str().unwrap_or_default();
    let expected = record.record["sha256"].as_str().unwrap_or_default();
    let Some((bytes, headers)) = get_object_bytes(settings, key)? else {
        return Err(format!("the bucket does not hold {key}"));
    };
    if let Some(meta) = header_value(&headers, "x-amz-meta-sha256")
        && meta != expected
    {
        return Err(format!(
            "{key}'s x-amz-meta-sha256 {meta} is not the record's {expected}"
        ));
    }
    same_key(&seal.name, &object_key(key, &headers)?)?;
    let encrypted = here.join("bundle.gpg");
    fs::write(&encrypted, &bytes).map_err(|error| format!("{}: {error}", encrypted.display()))?;
    drop(bytes);
    let raw = here.join("bundle.raw");
    decrypt_with(seal, &encrypted, &raw)?;
    let found = sha256_file(&raw)?;
    if found != expected {
        return Err(format!(
            "{key} decrypts to sha256 {found}, not the record's {expected}"
        ));
    }
    fs::remove_file(&encrypted).map_err(|error| format!("{}: {error}", encrypted.display()))?;
    let (tar, compression) = decompress_bundle(here, &raw)?;
    fs::remove_file(&raw).map_err(|error| format!("{}: {error}", raw.display()))?;
    let unpacked = here.join("unpacked");
    fs::create_dir(&unpacked).map_err(|error| format!("{}: {error}", unpacked.display()))?;
    let (ok, _stdout, stderr) = output(
        {
            let mut command = Command::new("tar");
            command.arg("-xf").arg(&tar).arg("-C").arg(&unpacked);
            command
        },
        "tar -xf",
    )?;
    if !ok {
        return Err(format!(
            "{key} is no tar archive ({compression}): {}",
            stderr.trim()
        ));
    }
    fs::remove_file(&tar).map_err(|error| format!("{}: {error}", tar.display()))?;
    let manifest: Value = serde_json::from_slice(
        &fs::read(unpacked.join("manifest.json"))
            .map_err(|error| format!("{key} holds no manifest.json: {error}"))?,
    )
    .map_err(|error| format!("{key}'s manifest.json: {error}"))?;
    let kind = record.record["kind"].as_str().unwrap_or_default();
    if manifest["kind"].as_str() != Some(kind) {
        return Err(format!(
            "{key}'s manifest says {}; its record says {kind:?}",
            manifest["kind"]
        ));
    }
    Ok(manifest)
}

/// A path the manifest names, as a relative path with no `..`: what a
/// restore may write under its own directory.
fn relative(path: &str) -> Option<PathBuf> {
    let path = Path::new(path);
    path.components()
        .all(|component| matches!(component, Component::Normal(_)))
        .then(|| path.to_path_buf())
        .filter(|path| !path.as_os_str().is_empty())
}

/// Every transcript slice of the chain, in order, into `restored`: a slice
/// at offset 0 is the whole file, any other an append at exactly the size
/// restored so far.
fn restore_transcripts(unpacked: &[(PathBuf, Value)], restored: &Path) -> Result<(), String> {
    for (directory, manifest) in unpacked {
        for entry in manifest["transcripts"].as_array().into_iter().flatten() {
            let named = entry["path"].as_str().unwrap_or_default();
            let path = relative(named)
                .ok_or_else(|| format!("the manifest names the transcript path {named:?}"))?;
            let offset = entry["offset"].as_u64().unwrap_or(0);
            let source = directory.join("transcripts").join(&path);
            let target = restored.join(&path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("{}: {error}", parent.display()))?;
            }
            let mut out = if offset == 0 {
                File::create(&target)
            } else {
                let held = fs::metadata(&target).map_or(0, |meta| meta.len());
                if held != offset {
                    return Err(format!(
                        "transcripts/{named}: an increment appends at {offset}, but the chain \
                         restored {held} bytes before it"
                    ));
                }
                OpenOptions::new().append(true).open(&target)
            }
            .map_err(|error| format!("{}: {error}", target.display()))?;
            let mut input = File::open(&source)
                .map_err(|_| format!("the bundle holds no transcripts/{named}"))?;
            std::io::copy(&mut input, &mut out)
                .map_err(|error| format!("{}: {error}", target.display()))?;
            out.flush()
                .map_err(|error| format!("{}: {error}", target.display()))?;
        }
    }
    Ok(())
}

/// The transcript archive restored and live, compared.
#[derive(Debug, Default)]
pub(super) struct Transcripts {
    pub(super) restored: u64,
    pub(super) restored_bytes: u64,
    /// Same size and sha256.
    pub(super) same: u64,
    /// Live is longer, and begins with the restored bytes.
    pub(super) grown: u64,
    /// Live only, made since the bundle.
    pub(super) new: u64,
    /// Live only, last written before the bundle's window: in no bundle.
    pub(super) outside: u64,
    pub(super) outside_bytes: u64,
    pub(super) mismatches: Vec<String>,
}

impl Transcripts {
    fn line(&self, live: &Path) -> String {
        format!(
            "restore-drill: transcripts: {} files ({} bytes) restored; against {}: {} the same, \
             {} grown since (their start the same), {} new since the bundle, {} in no bundle \
             (last written before its window, {} bytes: no off-host copy once older bundles \
             expire), {} mismatch(es)",
            self.restored,
            self.restored_bytes,
            live.display(),
            self.same,
            self.grown,
            self.new,
            self.outside,
            self.outside_bytes,
            self.mismatches.len()
        )
    }
}

/// Every regular file under `root`, relative, sorted.
fn files_under(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("{}: {error}", directory.display())),
        };
        for entry in entries {
            let entry = entry.map_err(|error| format!("{}: {error}", directory.display()))?;
            let kind = entry
                .file_type()
                .map_err(|error| format!("{}: {error}", entry.path().display()))?;
            if kind.is_dir() {
                stack.push(entry.path());
            } else if kind.is_file()
                && let Ok(path) = entry.path().strip_prefix(root)
            {
                files.push(path.to_path_buf());
            }
        }
    }
    files.sort();
    Ok(files)
}

/// The restored transcripts judged against the newest record's own
/// `transcripts.sha256` (objective 149), before live is looked at: its
/// line, and a mismatch for each restored file whose sha256 is not the one
/// the record names, and for each file the record sizes without a sha256.
/// A file the chain did not restore is the size check's mismatch, not this
/// one's. A record bundled before 149 names no map: said, no mismatch, and
/// the files are judged against live only.
pub(super) fn judge_by_record(
    restored: &Path,
    transcripts: &Value,
) -> Result<(String, Vec<String>), String> {
    let Some(hashes) = transcripts["sha256"].as_object() else {
        return Ok((
            "restore-drill: transcripts: the newest record names no per-file sha256 \
             (bundled before objective 149): judged against live only"
                .to_string(),
            Vec::new(),
        ));
    };
    let sizes = transcripts["sizes"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let mut same = 0usize;
    let mut mismatches = Vec::new();
    for (path, size) in &sizes {
        let Some(want) = hashes.get(path).and_then(Value::as_str) else {
            mismatches.push(format!(
                "transcripts/{path}: the newest record sizes it ({} bytes) and names no sha256",
                size.as_u64().unwrap_or(0)
            ));
            continue;
        };
        let file = restored.join(path);
        if !file.is_file() {
            continue;
        }
        if sha256_file(&file)? == want {
            same += 1;
        } else {
            mismatches.push(format!(
                "transcripts/{path}: the restored bytes are not the sha256 the newest record names"
            ));
        }
    }
    Ok((
        format!(
            "restore-drill: transcripts: {same} of {} files as the newest record's sha256",
            sizes.len()
        ),
        mismatches,
    ))
}

/// The sha256 of a file's first `bytes` bytes: the drill's look at live, and
/// the bundle's record of each transcript as bundled (objective 149).
pub(super) fn sha256_prefix(path: &Path, bytes: u64) -> Result<String, String> {
    let file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut reader = file.take(bytes);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

fn millis(time: std::io::Result<SystemTime>) -> Option<i64> {
    let elapsed = time.ok()?.duration_since(SystemTime::UNIX_EPOCH).ok()?;
    i64::try_from(elapsed.as_millis()).ok()
}

/// The restored archive against live (read only). `bundle_ms` is when the
/// newest bundle was recorded, `window_ms` its transcript window.
pub(super) fn compare_transcripts(
    restored: &Path,
    live: &Path,
    bundle_ms: i64,
    window_ms: i64,
) -> Result<Transcripts, String> {
    let mut out = Transcripts::default();
    let restored_files = files_under(restored)?;
    let held: BTreeSet<&PathBuf> = restored_files.iter().collect();
    for path in &restored_files {
        let ours = restored.join(path);
        let size = fs::metadata(&ours)
            .map_err(|error| format!("{}: {error}", ours.display()))?
            .len();
        out.restored += 1;
        out.restored_bytes = out.restored_bytes.saturating_add(size);
        let name = path.display();
        let theirs = live.join(path);
        let Ok(meta) = fs::metadata(&theirs) else {
            out.mismatches.push(format!(
                "transcripts/{name}: restored ({size} bytes), gone from live"
            ));
            continue;
        };
        let live_size = meta.len();
        if live_size < size {
            out.mismatches.push(format!(
                "transcripts/{name}: live is {live_size} bytes, shorter than its restored {size}"
            ));
            continue;
        }
        let want = sha256_file(&ours)?;
        if sha256_prefix(&theirs, size)? != want {
            out.mismatches.push(format!(
                "transcripts/{name}: live's first {size} bytes are not the restored file's"
            ));
        } else if live_size == size {
            out.same += 1;
        } else {
            out.grown += 1;
        }
    }
    for path in files_under(live)? {
        if held.contains(&path) {
            continue;
        }
        let meta = fs::metadata(live.join(&path))
            .map_err(|error| format!("{}: {error}", live.join(&path).display()))?;
        let modified = millis(meta.modified()).unwrap_or(i64::MAX);
        // Its birth when the filesystem has one: a file made before the
        // bundle and written since is no new one.
        let born = millis(meta.created()).unwrap_or(modified);
        if born >= bundle_ms {
            out.new += 1;
        } else if modified < bundle_ms.saturating_sub(window_ms) || modified >= bundle_ms {
            // Not written within the window when the bundle ran (or written
            // only since, after a pause longer than the window): no bundle
            // of the chain was to hold it.
            out.outside += 1;
            out.outside_bytes = out.outside_bytes.saturating_add(meta.len());
        } else {
            out.mismatches.push(format!(
                "transcripts/{}: live, written within the bundle's window, and not in the backup",
                path.display()
            ));
        }
    }
    Ok(out)
}

/// A git command for the drill's own repositories and the read-only looks
/// at live ones: the controller's (the C locale, the allowed protocols).
fn git() -> Command {
    crate::sources::controller_git()
}

fn run_git(mut command: Command, what: &str) -> Result<String, String> {
    let (ok, stdout, stderr) = output(
        {
            command.env("GIT_TERMINAL_PROMPT", "0");
            command
        },
        what,
    )?;
    if ok {
        Ok(stdout)
    } else {
        Err(format!("{what}: {}", stderr.trim()))
    }
}

/// One source restored into `mirror`: a `git clone --mirror` of its bundle
/// in the chain's full bundle, each increment's bundle fetched on top, its
/// refs then exactly `heads` (the newest record's), and its objects
/// connected.
fn restore_repo(
    source: &str,
    unpacked: &[(PathBuf, Value)],
    heads: &BTreeMap<String, String>,
    mirror: &Path,
) -> Result<(), String> {
    let mut bundles: Vec<PathBuf> = Vec::new();
    for (directory, manifest) in unpacked {
        let entry = manifest["repos"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|repo| repo["source"].as_str() == Some(source));
        if let Some(file) = entry.and_then(|repo| repo["file"].as_str()) {
            let file = relative(file)
                .ok_or_else(|| format!("the manifest names the bundle file {file:?}"))?;
            bundles.push(directory.join(file));
        }
    }
    if heads.is_empty() {
        let mut init = git();
        init.args(["init", "-q", "--bare"]).arg(mirror);
        run_git(init, "git init --bare")?;
        return Ok(());
    }
    let Some((first, rest)) = bundles.split_first() else {
        return Err("no bundle of the chain holds it".to_string());
    };
    let mut clone = git();
    clone
        .args(["clone", "-q", "--mirror"])
        .arg(first)
        .arg(mirror);
    run_git(clone, "git clone --mirror")?;
    for bundle in rest {
        let mut fetch = git();
        fetch
            .arg("--git-dir")
            .arg(mirror)
            .args(["fetch", "-q"])
            .arg(bundle)
            .arg("+refs/*:refs/*");
        run_git(fetch, "git fetch (an increment's bundle)")?;
    }
    let listed = run_git(
        {
            let mut command = git();
            command
                .arg("--git-dir")
                .arg(mirror)
                .args(["for-each-ref", "--format=%(refname)"]);
            command
        },
        "git for-each-ref",
    )?;
    for name in listed.lines().filter(|name| !heads.contains_key(*name)) {
        let mut delete = git();
        delete
            .arg("--git-dir")
            .arg(mirror)
            .args(["update-ref", "-d", name]);
        run_git(delete, "git update-ref -d")?;
    }
    for (name, commit) in heads {
        let mut update = git();
        update
            .arg("--git-dir")
            .arg(mirror)
            .args(["update-ref", name, commit]);
        run_git(update, "git update-ref").map_err(|error| {
            format!("{name} at {commit}: the restored objects lack it ({error})")
        })?;
    }
    let mut fsck = git();
    fsck.arg("--git-dir").arg(mirror).args([
        "fsck",
        "--connectivity-only",
        "--no-dangling",
        "--no-progress",
    ]);
    run_git(fsck, "git fsck --connectivity-only")?;
    Ok(())
}

/// Whether live's `new` descends from `old` (or is it), asked of the live
/// repository read-only.
fn descends(source: &str, old: &str, new: &str) -> bool {
    git()
        .args(["-C", source, "merge-base", "--is-ancestor", old, new])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Where [`restore_repos`] puts what it finds: the report's lines, the
/// mismatches and the unknown commits.
struct Found<'a> {
    lines: &'a mut Vec<String>,
    mismatches: &'a mut Vec<String>,
    unknown: &'a mut Vec<String>,
}

/// One source's refs restored and live, compared.
#[derive(Debug, Default)]
pub(super) struct Refs {
    pub(super) restored: usize,
    pub(super) same: usize,
    pub(super) newer: usize,
    pub(super) added: usize,
    pub(super) removed: usize,
    pub(super) mismatches: Vec<String>,
    /// Heads whose commit the history rewrite dropped (the commit map names
    /// no new commit): said, never a failure; nothing of an empty commit is
    /// lost.
    pub(super) dropped: Vec<String>,
    /// Heads in neither the commit map nor live: a failure, as "unknown
    /// commit", never a mismatch.
    pub(super) unknown: Vec<String>,
}

impl Refs {
    fn line(&self, source: &str) -> String {
        let mut line = format!(
            "restore-drill: missions {source}: {} refs restored (clone --mirror, fsck connected); \
             against live: {} the same, {} moved on since (descendants), {} added since, {} \
             removed since, {} mismatch(es)",
            self.restored,
            self.same,
            self.newer,
            self.added,
            self.removed,
            self.mismatches.len()
        );
        if !self.dropped.is_empty() || !self.unknown.is_empty() {
            let _ = write!(
                line,
                ", {} dropped by the history rewrite, {} unknown commit(s)",
                self.dropped.len(),
                self.unknown.len()
            );
        }
        line
    }
}

/// The restored refs against live's (`refs/heads` and `refs/tags`): each
/// restored head read through `now` (the commit map, then live:
/// [`crate::commit_now::commit_now`]; objective 178), then compared with
/// live, `descends(old, new)` asked of live. Mapped to live's commit is the
/// same, one live moved past is newer; only a commit live holds can be a
/// mismatch. A dropped head is listed in `dropped`, one in neither the map
/// nor live in `unknown`. An error of `now` (the store's) is the error.
pub(super) fn compare_refs(
    restored: &BTreeMap<String, String>,
    live: &BTreeMap<String, String>,
    now: impl Fn(&str) -> Result<CommitNow, String>,
    descends: impl Fn(&str, &str) -> bool,
) -> Result<Refs, String> {
    let mut out = Refs {
        restored: restored.len(),
        ..Refs::default()
    };
    for (name, commit) in restored {
        let Some(live_at) = live.get(name) else {
            out.removed += 1;
            continue;
        };
        let current = match now(commit)? {
            CommitNow::Commit(current) => current,
            CommitNow::Dropped => {
                out.dropped
                    .push(format!("{name}: {}", why(CommitNow::Dropped, commit)));
                continue;
            }
            CommitNow::Unknown => {
                out.unknown
                    .push(format!("{name}: {}", why(CommitNow::Unknown, commit)));
                continue;
            }
        };
        if *live_at == current {
            out.same += 1;
        } else if descends(&current, live_at) {
            out.newer += 1;
        } else if current == *commit {
            out.mismatches.push(format!(
                "{name}: restored at {commit}; live is at {live_at}, which does not descend from it"
            ));
        } else {
            out.mismatches.push(format!(
                "{name}: restored at {commit} (now {current}); live is at {live_at}, which does \
                 not descend from it"
            ));
        }
    }
    out.added = live
        .keys()
        .filter(|name| !restored.contains_key(*name))
        .count();
    Ok(out)
}

/// The sentence of a restored head with no commit now.
fn why(now: CommitNow, commit: &str) -> String {
    now.id_or_why("restored at", commit).unwrap_or_else(|why| why)
}

#[cfg(test)]
mod tests {
    use super::{
        Chain, DrillDir, chain_of, compare_refs, compare_transcripts, drill_parent,
        judge_by_record, outcome, sha256_prefix, Outcome,
    };
    use crate::commit_now::CommitNow;
    use crate::backup::BundleRecord;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// A directory of the test's own under the temp dir, removed at the end.
    struct Own(PathBuf);

    impl Own {
        /// A directory made fresh here (a random name, `create_dir`, never
        /// one that was there): its Drop removes only what this made.
        fn new(name: &str) -> Self {
            for _ in 0..10 {
                let path = std::env::temp_dir().join(format!(
                    "spawn-restore-drill-test-{name}-{}-{}",
                    std::process::id(),
                    crate::backup::random_hex(6).expect("random")
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path.canonicalize().expect("resolved")),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => panic!("{}: {error}", path.display()),
                }
            }
            panic!("no fresh directory for {name}");
        }
    }

    impl Drop for Own {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().expect("a parent")).expect("parent");
        fs::write(path, bytes).expect("write");
    }

    fn record(event_id: i64, kind: &str, base: Option<i64>, stored: bool) -> BundleRecord {
        BundleRecord {
            event_id,
            at_ms: event_id * 1000,
            record: json!({
                "kind": kind, "base_event_id": base,
                "keys": if stored { json!([format!("bundles/k{event_id}")]) } else { json!([]) },
                "skipped": [],
            }),
        }
    }

    /// Objective 114: a chain is read with one key, its newest link's (no
    /// `sealed_with`: k1); links of two keys cross and are refused; a link
    /// whose name is no key name is refused by its record, never echoed.
    #[test]
    fn a_chain_is_read_with_one_key_and_never_across_keys() {
        use super::{ChainKey, chain_key};
        let sealed = |event_id: i64, base: Option<i64>, name: Option<&str>| {
            let mut link = record(
                event_id,
                if base.is_some() {
                    "incremental"
                } else {
                    "full"
                },
                base,
                true,
            );
            if let Some(name) = name {
                link.record["sealed_with"] = json!(name);
            }
            link
        };
        let (old, incr) = (sealed(1, None, None), sealed(2, Some(1), Some("k1")));
        assert_eq!(chain_key(&[&old, &incr]), Ok("k1".to_string()));
        let (full2, incr2) = (sealed(3, None, Some("k2")), sealed(4, Some(3), Some("k2")));
        assert_eq!(chain_key(&[&full2, &incr2]), Ok("k2".to_string()));
        let crossed = sealed(5, Some(2), Some("k2"));
        assert!(matches!(chain_key(&[&old, &incr, &crossed]),
                Err(ChainKey::Crossed(reason)) if reason.starts_with("the chain crosses keys")),);
        let made_up = "bb-made-up-passphrase-0123456789";
        let planted = sealed(6, None, Some(made_up));
        assert_eq!(
            chain_key(&[&planted]),
            Err(ChainKey::Refused(
                "the sealed_with of backup.bundled record 6 is not a backup key name (k and a number from 1, as k2)"
                    .to_string()
            ))
        );
    }

    /// The parent is refused under a fake home, a fake live directory or the
    /// root, through a symlink too, and when relative or missing; any other
    /// directory is taken, resolved. The fake home is never the real one.
    #[test]
    fn the_scratch_parent_is_never_under_home_live_or_the_root() {
        let own = Own::new("parent");
        let home = own.0.join("home");
        let live = home.join(".local/share/spawn-rsi");
        let root = own.0.join("ws");
        let elsewhere = own.0.join("data-tmp");
        for directory in [&live, &root, &elsewhere] {
            fs::create_dir_all(directory).expect("dir");
        }
        assert_ne!(
            Some(home.clone()),
            std::env::var_os("HOME").map(PathBuf::from),
            "a fake home, never the real one"
        );
        let guards = vec![
            ("$HOME", home.clone()),
            ("$SPAWN_LIVE", live.clone()),
            ("the workspace root", root.clone()),
        ];
        for refused in [&home, &live, &live.join(".."), &root] {
            let error = drill_parent(refused, &guards).expect_err("refused");
            assert!(error.contains("is under"), "{error}");
        }
        std::os::unix::fs::symlink(&home, own.0.join("into-home")).expect("symlink");
        assert!(
            drill_parent(&own.0.join("into-home"), &guards).is_err(),
            "a symlink into home"
        );
        assert!(drill_parent(Path::new("relative"), &guards).is_err());
        assert!(drill_parent(&own.0.join("missing"), &guards).is_err());
        assert_eq!(drill_parent(&elsewhere, &guards), Ok(elsewhere.clone()));
    }

    /// The scratch directory is made fresh with mode 700 and removed at the
    /// end; one replaced in the meantime (a symlink to a directory holding
    /// a file) is left, and so is what the symlink points at.
    #[test]
    fn the_drill_removes_only_the_directory_it_made() {
        use std::os::unix::fs::PermissionsExt;
        let own = Own::new("dir");
        let drill = DrillDir::create(&own.0).expect("made");
        let path = drill.path().to_path_buf();
        let mode = fs::metadata(&path).expect("there").permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        write(&path.join("a/b"), b"x");
        drill.remove().expect("removed");
        assert!(!path.exists());

        let drill = DrillDir::create(&own.0).expect("made");
        let path = drill.path().to_path_buf();
        let precious = own.0.join("precious");
        write(&precious.join("keep"), b"keep");
        fs::remove_dir(&path).expect("emptied");
        std::os::unix::fs::symlink(&precious, &path).expect("swapped for a symlink");
        let error = drill.remove().expect_err("not the directory it made");
        assert!(error.contains("left as it is"), "{error}");
        assert!(
            precious.join("keep").exists(),
            "what it pointed at is untouched"
        );
    }

    /// The chain: the newest stored record back through its bases to a
    /// full one, oldest first; a base that stored nothing breaks it; no
    /// stored record is none.
    #[test]
    fn the_chain_runs_from_the_newest_stored_bundle_back_to_its_full_one() {
        let records = vec![
            record(1, "full", None, true),
            record(2, "incremental", Some(1), true),
            record(3, "incremental", Some(2), true),
            record(4, "incremental", Some(3), false),
        ];
        let ids: Vec<i64> = chain_of(&records)
            .expect("a chain")
            .iter()
            .map(|record| record.event_id)
            .collect();
        assert_eq!(ids, [1, 2, 3], "the newest stored, not the skipped one");
        let broken = vec![
            record(1, "full", None, true),
            record(2, "incremental", Some(1), false),
            record(3, "incremental", Some(2), true),
        ];
        assert!(
            matches!(chain_of(&broken), Err(Chain::Broken(reason)) if reason.contains("stored nothing"))
        );
        let orphan = vec![record(5, "incremental", Some(4), true)];
        assert!(matches!(chain_of(&orphan), Err(Chain::Broken(_))));
        assert!(matches!(
            chain_of(&[record(1, "full", None, false)]),
            Err(Chain::None)
        ));
    }

    /// The sha256 of these bytes, hex.
    fn digest(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        crate::backup::hex(&Sha256::digest(bytes))
    }

    /// Restored transcripts against the newest record's own sha256
    /// (objective 149): the bytes as bundled pass; the same size with other
    /// bytes fails, named; a file sized without a sha256 fails; a record
    /// bundled before 149 (no map) is said, with no mismatch.
    #[test]
    fn restored_transcripts_are_judged_against_the_records_sha256() {
        let own = Own::new("record");
        let restored = own.0.join("restored");
        write(&restored.join("a/one.jsonl"), b"one\n");
        write(&restored.join("b/two.jsonl"), b"two\ntwo\n");
        let sizes = json!({"a/one.jsonl": 4, "b/two.jsonl": 8});

        let good = json!({"sizes": sizes, "sha256": {
            "a/one.jsonl": digest(b"one\n"), "b/two.jsonl": digest(b"two\ntwo\n"),
        }});
        let (line, mismatches) = judge_by_record(&restored, &good).expect("judged");
        assert_eq!(
            line,
            "restore-drill: transcripts: 2 of 2 files as the newest record's sha256"
        );
        assert!(mismatches.is_empty(), "{mismatches:?}");

        let other = json!({"sizes": sizes, "sha256": {
            "a/one.jsonl": digest(b"ONE\n"), "b/two.jsonl": digest(b"two\ntwo\n"),
        }});
        let (line, mismatches) = judge_by_record(&restored, &other).expect("judged");
        assert!(line.contains(" 1 of 2 files "), "{line}");
        assert_eq!(
            mismatches,
            [
                "transcripts/a/one.jsonl: the restored bytes are not the sha256 the newest record names"
            ]
        );

        let unhashed = json!({"sizes": sizes, "sha256": {"a/one.jsonl": digest(b"one\n")}});
        let (line, mismatches) = judge_by_record(&restored, &unhashed).expect("judged");
        assert!(line.contains(" 1 of 2 files "), "{line}");
        assert_eq!(mismatches.len(), 1, "{mismatches:?}");
        assert!(
            mismatches[0].starts_with("transcripts/b/two.jsonl:")
                && mismatches[0].contains("names no sha256"),
            "{mismatches:?}"
        );

        let before = json!({"files": 2, "bytes": 12, "sizes": sizes});
        let (line, mismatches) = judge_by_record(&restored, &before).expect("judged");
        assert!(
            line.contains("names no per-file sha256 (bundled before objective 149)"),
            "{line}"
        );
        assert!(mismatches.is_empty(), "{mismatches:?}");
    }

    /// The hash the bundle records is of a file's first `size` bytes: a file
    /// grown since its size was read is recorded as its bundled bytes.
    #[test]
    fn a_prefix_hash_is_of_the_first_bytes_only() {
        let own = Own::new("prefix");
        let file = own.0.join("grown.jsonl");
        write(&file, b"bundled\nappended since\n");
        assert_eq!(
            sha256_prefix(&file, 8).expect("hashed"),
            digest(b"bundled\n")
        );
        assert_ne!(
            sha256_prefix(&file, 8).expect("hashed"),
            crate::backup::sha256_file(&file).expect("hashed")
        );
        assert_eq!(
            sha256_prefix(&file, 1 << 20).expect("hashed"),
            digest(b"bundled\nappended since\n"),
            "a size past the end is the whole file"
        );
    }

    /// Restored transcripts against live: the same file, one grown since,
    /// one new since the bundle and one last written before its window are
    /// no mismatch; one changed, one shorter, one gone and one written in
    /// the window but not restored are.
    #[test]
    fn transcripts_compare_by_size_and_sha256_allowing_what_came_since() {
        let own = Own::new("transcripts");
        let restored = own.0.join("restored");
        let live = own.0.join("live");
        write(&restored.join("a/same.jsonl"), b"one\n");
        write(&live.join("a/same.jsonl"), b"one\n");
        write(&restored.join("a/grown.jsonl"), b"one\n");
        write(&live.join("a/grown.jsonl"), b"one\ntwo\n");
        write(&restored.join("changed.jsonl"), b"one\n");
        write(&live.join("changed.jsonl"), b"ONE\n");
        write(&restored.join("shorter.jsonl"), b"one\ntwo\n");
        write(&live.join("shorter.jsonl"), b"one\n");
        write(&restored.join("gone.jsonl"), b"one\n");
        write(&live.join("new.jsonl"), b"new\n");
        write(&live.join("old.jsonl"), b"old\n");
        write(&live.join("missed.jsonl"), b"missed\n");
        let now = std::time::SystemTime::now();
        let set = |path: &Path, ago_days: u64| {
            let file = fs::File::options().write(true).open(path).expect("open");
            file.set_modified(now - std::time::Duration::from_secs(ago_days * 86_400))
                .expect("mtime");
        };
        set(&live.join("old.jsonl"), 30);
        set(&live.join("missed.jsonl"), 3);
        let now_ms = i64::try_from(
            now.duration_since(std::time::UNIX_EPOCH)
                .expect("after 1970")
                .as_millis(),
        )
        .expect("ms");
        // The files were born just now: a bundle "an hour from now" makes
        // every live-only file one that was there before it.
        let bundle_ms = now_ms + 3_600_000;
        let report =
            compare_transcripts(&restored, &live, bundle_ms, 14 * 86_400_000).expect("compared");
        assert_eq!(report.restored, 5);
        assert_eq!((report.same, report.grown, report.new), (1, 1, 0));
        assert_eq!(report.outside, 1, "old.jsonl: in no bundle, said apart");
        let named = report.mismatches.join("\n");
        // new.jsonl and missed.jsonl were there (born) before such a bundle
        // and written within its window: the backup should hold them.
        for name in [
            "changed.jsonl",
            "shorter.jsonl",
            "gone.jsonl",
            "missed.jsonl",
            "new.jsonl",
        ] {
            assert!(named.contains(name), "{name} is a mismatch: {named}");
        }
        assert_eq!(report.mismatches.len(), 5, "{named}");

        // A bundle before the files were born (where the filesystem keeps a
        // birth time): live-only files are new since it.
        if fs::metadata(live.join("new.jsonl"))
            .and_then(|meta| meta.created())
            .is_ok()
        {
            let early = compare_transcripts(&restored, &live, now_ms - 3_600_000, 14 * 86_400_000)
                .expect("compared");
            assert_eq!((early.new, early.outside), (3, 0), "{:?}", early.mismatches);
        }
    }

    /// Refs: the same, moved on to a descendant, added and removed since
    /// are no mismatch; a ref live moved to a commit that does not descend
    /// from the restored one is.
    #[test]
    fn refs_compare_allowing_descendants_and_refs_made_or_removed_since() {
        let restored: BTreeMap<String, String> = [
            ("refs/heads/main", "c1"),
            ("refs/heads/moved", "c1"),
            ("refs/heads/gone", "c1"),
            ("refs/heads/rewritten", "c1"),
        ]
        .into_iter()
        .map(|(name, commit)| (name.to_string(), commit.to_string()))
        .collect();
        let live: BTreeMap<String, String> = [
            ("refs/heads/main", "c1"),
            ("refs/heads/moved", "c2"),
            ("refs/heads/rewritten", "x9"),
            ("refs/tags/v1", "c2"),
        ]
        .into_iter()
        .map(|(name, commit)| (name.to_string(), commit.to_string()))
        .collect();
        let held = |commit: &str| Ok(CommitNow::Commit(commit.to_string()));
        let report = compare_refs(&restored, &live, held, |old, new| old == "c1" && new == "c2")
            .expect("compared");
        assert_eq!(
            (
                report.restored,
                report.same,
                report.newer,
                report.added,
                report.removed
            ),
            (4, 1, 1, 1, 1)
        );
        assert_eq!(report.mismatches.len(), 1);
        assert!(
            report.mismatches[0].starts_with("refs/heads/rewritten"),
            "{:?}",
            report.mismatches
        );
    }

    /// Objective 178: each restored head is read through the commit map,
    /// then live. A head bundled before the rewrite mapped to live's commit
    /// is the same, one live moved past is newer; a dropped head is listed
    /// apart and fails nothing; a head in neither the map nor live is an
    /// unknown commit, never a mismatch; a mapped head live does not descend
    /// from is a mismatch naming both ids. The source's line gains the
    /// counts; the outcome counts the unknown without calling it a
    /// mismatch; a store error is the error.
    #[test]
    fn restored_heads_are_compared_with_live_through_the_commit_map() {
        let restored: BTreeMap<String, String> = [
            ("refs/heads/same", "o1"),
            ("refs/heads/newer", "o1"),
            ("refs/heads/e", "oe"),
            ("refs/heads/lost", "ol"),
            ("refs/heads/rewritten", "o2"),
        ]
        .into_iter()
        .map(|(name, commit)| (name.to_string(), commit.to_string()))
        .collect();
        let live: BTreeMap<String, String> = [
            ("refs/heads/same", "n1"),
            ("refs/heads/newer", "n2"),
            ("refs/heads/e", "n1"),
            ("refs/heads/lost", "n1"),
            ("refs/heads/rewritten", "n9"),
        ]
        .into_iter()
        .map(|(name, commit)| (name.to_string(), commit.to_string()))
        .collect();
        let map = |commit: &str| {
            let entry = match commit {
                "o1" => Some(Some("n1".to_string())),
                "o2" => Some(Some("n3".to_string())),
                "oe" => Some(None),
                _ => None,
            };
            Ok(CommitNow::of(commit, entry.as_ref(), || {
                commit.starts_with('n')
            }))
        };
        let descends = |old: &str, new: &str| old == "n1" && new == "n2";
        let report = compare_refs(&restored, &live, map, descends).expect("compared");
        assert_eq!(
            (report.restored, report.same, report.newer, report.added, report.removed),
            (5, 1, 1, 0, 0)
        );
        assert_eq!(
            report.dropped,
            ["refs/heads/e: restored at oe was dropped by the history rewrite (the commit map \
              names no new commit for it)"]
        );
        assert_eq!(
            report.unknown,
            ["refs/heads/lost: restored at ol is an unknown commit (in neither the commit map \
              nor the source)"]
        );
        assert_eq!(
            report.mismatches,
            ["refs/heads/rewritten: restored at o2 (now n3); live is at n9, which does not \
              descend from it"]
        );
        assert!(
            report.line("/src").ends_with(
                "against live: 1 the same, 1 moved on since (descendants), 0 added since, 0 \
                 removed since, 1 mismatch(es), 1 dropped by the history rewrite, 1 unknown \
                 commit(s)"
            ),
            "{}",
            report.line("/src")
        );
        // Neither dropped nor unknown: the line as it was.
        let plain = compare_refs(
            &restored,
            &BTreeMap::new(),
            |_: &str| Err("never asked".to_string()),
            descends,
        )
        .expect("all removed");
        assert!(plain.line("/src").ends_with("5 removed since, 0 mismatch(es)"));
        // The outcome: both counted, the unknown never called a mismatch.
        match outcome("missions", &[], &report.unknown, "k") {
            Outcome::Mismatched(count, reason) => {
                assert_eq!(count, 1);
                assert!(
                    reason.starts_with(
                        "missions: 0 mismatch(es) and 1 unknown commit(s) older than bundle k; \
                         the first: refs/heads/lost: restored at ol is an unknown commit"
                    ),
                    "{reason}"
                );
            }
            other => panic!("not failed: {other:?}"),
        }
        match outcome("missions", &report.mismatches, &[], "k") {
            Outcome::Mismatched(1, reason) => assert!(
                reason.starts_with("missions: 1 mismatch(es) older than bundle k; the first:"),
                "{reason}"
            ),
            other => panic!("not one mismatch: {other:?}"),
        }
        assert!(matches!(outcome("missions", &[], &[], "k"), Outcome::Passed));
        // A store error is the error, not a verdict.
        let failed = compare_refs(
            &restored,
            &live,
            |_: &str| Err("the store is gone".to_string()),
            descends,
        );
        assert_eq!(failed.err(), Some("the store is gone".to_string()));
    }
}
