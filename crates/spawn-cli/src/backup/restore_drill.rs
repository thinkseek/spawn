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
//!   heads and `git fsck --full` run on it (every object read whole).
//!
//! `--only missions` (objective 186) drills the mission repositories alone:
//! from each bundle it unpacks by name only `manifest.json` and the files
//! directly under `repos/`, never a transcript, and records
//! `backup.drilled.missions` alone. The bundle is never written in the
//! clear: gpg decrypts the stored ciphertext into a pipe, and the stream is
//! hashed on its way through the decompressor its magic names into tar,
//! twice: `tar -t` first, whose sha256 must be the record's before anything
//! is extracted, then `tar -x` of the names chosen from that listing, whose
//! sha256 must be the same. A run killed part way leaves the ciphertext,
//! the list of names and what it has extracted: no transcript.
//!
<<<<<<< 40293466db32cb4376fe20ba8af611199fea6eaf
//! `--only controllers` (objective 198, [`controllers`]) reads no bundle: it
//! checks that each live controller an open or active mission's gate names,
//! current and previous, is `git archive` of its commit from the live
//! source, and records one `backup.drilled.controllers` per controller.
=======
//! `--only transcripts` (objective 200) drills the transcript archive
//! alone, through the same pipe: from each bundle it unpacks by name only
//! `manifest.json` and the files under `transcripts/` (no directory, no
//! empty, `.` or `..` part, no name tar had to quote), never a repository
//! bundle, and each unpacked tree must hold only directories and regular
//! files (a symlink in a bundle is never followed). The chain's slices are
//! restored into the drill's folder and each file is judged by the newest
//! record alone: the size it names and its sha256 in `transcripts.sha256`
//! (a record bundled before objective 149 names none, and fails). Nothing
//! live is read: the archive, `~/.claude` and every path outside the folder
//! stay unopened. Before anything is fetched it refuses (exit 2, nothing
//! recorded, nothing made) when the chain's ciphertext and slices and the
//! newest record's files, counted from the records, would leave the
//! folder's file system under `SPAWN_BACKUP_DRILL_FLOOR_MB` (default 40960,
//! 40 GB). It records `backup.drilled.transcripts` alone; its folder is
//! removed, after the identity check below, only when everything in it is
//! a directory or a regular file, else the drill fails and the folder is
//! left, said.
>>>>>>> b822c635abac0fae9289692121a5f4d6d49cbb6e
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
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Instant, SystemTime};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{
    BundleRecord, Key, Settings, bundle_compression, bundle_records, decompress_bundle,
    decrypt_with, get_object_bytes, header_value, heads_of, hex, object_key, output, random_hex,
    reading_key, record_key, same_key, sha256_file,
};
use crate::Flags;
use crate::commit_now::CommitNow;
use crate::config::{env_value, open_store};

mod controllers;

/// Where the scratch directory goes when `--into` is not given.
const DEFAULT_PARENT: &str = "/data/tmp";

/// The scratch directory's name starts with this.
const DRILL_PREFIX: &str = "spawn-restore-drill-";

/// How many mismatches are named one by one.
const NAMED: usize = 20;

/// `spawn backup restore-drill [--root <dir>] [--into <dir>] [--keep]
<<<<<<< 40293466db32cb4376fe20ba8af611199fea6eaf
/// [--only missions|controllers]`.
pub(super) fn restore_drill(flags: &Flags) -> Result<i32, String> {
    let only_controllers = flags.get("only").as_deref() == Some(controllers::CONTROLLERS);
    let only = if only_controllers {
        false
    } else {
        match only_missions(flags.get("only").as_deref()) {
            Ok(only) => only,
            Err(error) => {
                eprintln!("spawn backup restore-drill: {error}");
                return Ok(2);
            }
        }
=======
/// [--only missions|transcripts]`.
pub(super) fn restore_drill(flags: &Flags) -> Result<i32, String> {
    let Some((only, resources)) = resources_of(flags) else {
        return Ok(2);
>>>>>>> b822c635abac0fae9289692121a5f4d6d49cbb6e
    };
    let settings = match Settings::from_env(flags) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("spawn backup restore-drill: {error}");
            return Ok(2);
        }
    };
<<<<<<< 40293466db32cb4376fe20ba8af611199fea6eaf
    let parent = match parent_of(flags, &settings) {
        Ok(parent) => parent,
        Err(error) => {
            eprintln!("spawn backup restore-drill: {error}");
            return Ok(2);
        }
=======
    let Some(parent) = parent_of(flags, &settings) else {
        return Ok(2);
>>>>>>> b822c635abac0fae9289692121a5f4d6d49cbb6e
    };
    if only_controllers {
        let live = env_value("SPAWN_LIVE").map_or_else(
            || super::home().join(".local/share/spawn-rsi"),
            PathBuf::from,
        );
        let store = open_store()?;
        return controllers::drill(&store, &parent, &live);
    }
    let resources: &[&'static str] = if only {
        println!(
            "restore-drill: --only missions: the mission repositories alone; no transcript \
             unpacked or restored"
        );
        &[MISSIONS]
    } else {
        &[TRANSCRIPTS, MISSIONS]
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
            return Ok(unrestorable(&store, &records, &reason, started, resources));
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
            Err(reason) => {
                return Ok(fail_both(&store, &records, &reason, started, 2, resources));
            }
        },
        Err(ChainKey::Refused(reason)) => {
            return Ok(fail_both(&store, &records, &reason, started, 2, resources));
        }
        Err(ChainKey::Crossed(reason)) => {
            return Ok(unrestorable(&store, &records, &reason, started, resources));
        }
    };
    // Before anything is fetched: the transcripts drill's bytes above the
    // floor, or no drill (exit 2, nothing recorded, nothing made).
    if only == Only::Transcripts
        && let Err(refusal) = above_floor(&chain, &parent)
    {
        eprintln!("spawn backup restore-drill: {refusal}");
        return Ok(2);
    }
    let mut drill = DrillDir::create(&parent)?;
    if only == Only::Transcripts {
        drill.plain_only();
    }
    let result = run(&store, &settings, &chain, &seal, drill.path(), only);
    let newest = chain[chain.len() - 1];
    let (mut code, mut outcomes) = verdicts(result, resources);
    let removed = if flags.switch("keep") {
        println!("restore-drill: kept {}", drill.path().display());
        drill.keep();
        Ok(())
    } else {
        drill.remove()
    };
    if let Err(error) = removed {
        if only != Only::Transcripts {
            return Err(error);
        }
        // The folder left (not plain, or no longer the one made): said, and
        // the drill fails.
        println!("restore-drill: FAILED: {error}");
        code = code.max(1);
        for (_, outcome) in &mut outcomes {
            if matches!(outcome, Outcome::Passed) {
                *outcome = Outcome::Unrestorable(error.clone());
            }
        }
    }
    let key = bundle_key(newest);
    let verdict = Verdict {
        key: &key,
        event_id: newest.event_id,
        duration_ms: elapsed_ms(started),
    };
    Ok(record_verdicts(&store, &verdict, &outcomes).unwrap_or(code))
}

/// `--only`'s drill and the resources it proves, its line said; `None`
/// (said) when `--only`'s value is refused: exit 2, nothing recorded.
fn resources_of(flags: &Flags) -> Option<(Only, &'static [&'static str])> {
    let only = match only_of(flags.get("only").as_deref()) {
        Ok(only) => only,
        Err(error) => {
            eprintln!("spawn backup restore-drill: {error}");
            return None;
        }
    };
    let resources: &'static [&'static str] = match only {
        Only::Both => &[TRANSCRIPTS, MISSIONS],
        Only::Missions => {
            println!(
                "restore-drill: --only missions: the mission repositories alone; no transcript \
                 unpacked or restored"
            );
            &[MISSIONS]
        }
        Only::Transcripts => {
            println!(
                "restore-drill: --only transcripts: the transcript archive alone, judged by the \
                 newest record; no repository bundle unpacked, nothing live read"
            );
            &[TRANSCRIPTS]
        }
    };
    Some((only, resources))
}

/// The drill folder's parent (`--into`, default [`DEFAULT_PARENT`]),
/// resolved and under none of `$HOME`, `$SPAWN_LIVE` and the workspace
/// root; `None` (said) when refused.
fn parent_of(flags: &Flags, settings: &Settings) -> Option<PathBuf> {
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
    match drill_parent(&parent, &guards) {
        Ok(parent) => Some(parent),
        Err(error) => {
            eprintln!("spawn backup restore-drill: {error}");
            None
        }
    }
}

/// The free space a transcripts drill must leave on its folder's file
/// system: `SPAWN_BACKUP_DRILL_FLOOR_MB`, in MiB.
const FLOOR_MB: u64 = 40_960;

/// The bytes a transcripts drill writes, from the chain's records alone:
/// each link's ciphertext (`stored_bytes`) and transcript slices
/// (`transcripts.bytes`), and the newest record's files
/// (`transcripts.sizes`), which the chain's slices are restored into.
pub(super) fn transcripts_bytes(chain: &[&BundleRecord]) -> u64 {
    let fetched = chain.iter().fold(0u64, |sum, link| {
        sum.saturating_add(link.record["stored_bytes"].as_u64().unwrap_or(0))
            .saturating_add(link.record["transcripts"]["bytes"].as_u64().unwrap_or(0))
    });
    let restored = chain.last().map_or(0, |newest| {
        newest.record["transcripts"]["sizes"]
            .as_object()
            .into_iter()
            .flatten()
            .fold(0u64, |sum, (_, size)| {
                sum.saturating_add(size.as_u64().unwrap_or(0))
            })
    });
    fetched.saturating_add(restored)
}

/// The refusal, one line naming the floor, when writing `needed` bytes
/// into `parent` with `free` bytes free would leave under `floor_mb` MiB.
pub(super) fn floor_refusal(
    needed: u64,
    free: u64,
    floor_mb: u64,
    parent: &Path,
) -> Result<(), String> {
    let floor = floor_mb.saturating_mul(1024 * 1024);
    if free.saturating_sub(needed) >= floor && free >= needed {
        return Ok(());
    }
    Err(format!(
        "refused: the chain's ciphertext and slices and the newest record's files are {needed} \
         bytes, and {} has {free} bytes free: that would leave it under the floor of {floor_mb} \
         MB (SPAWN_BACKUP_DRILL_FLOOR_MB); nothing fetched",
        parent.display()
    ))
}

/// The transcripts drill's bytes (`transcripts_bytes`) against the free
/// space of `parent`'s file system and the floor.
fn above_floor(chain: &[&BundleRecord], parent: &Path) -> Result<(), String> {
    let floor_mb = match env_value("SPAWN_BACKUP_DRILL_FLOOR_MB") {
        None => FLOOR_MB,
        Some(value) => value.trim().parse::<u64>().map_err(|_| {
            "refused: SPAWN_BACKUP_DRILL_FLOOR_MB, the floor, is not a whole number of MB"
                .to_string()
        })?,
    };
    let free = crate::disk::free_bytes(parent)
        .map_err(|error| format!("refused: the floor cannot be checked: {error}"))?;
    floor_refusal(transcripts_bytes(chain), free, floor_mb, parent)
}

/// A drill's result said, and its exit and the outcome of each resource
/// it proves (`resources`): a report's own, or each unrestorable.
fn verdicts(
    result: Result<Report, String>,
    resources: &[&'static str],
) -> (i32, Vec<(&'static str, Outcome)>) {
    match result {
        Ok(report) => (
            said(&report),
            outcomes(&report)
                .into_iter()
                .filter(|(resource, _)| resources.contains(resource))
                .collect(),
        ),
        Err(error) => {
            let reason = format!("the backup cannot be restored: {error}");
            println!("restore-drill: FAILED: {reason}");
            (1, unrestorable_all(resources, &reason))
        }
    }
}

/// Each of `resources` unrestorable, for `reason`.
fn unrestorable_all(resources: &[&'static str], reason: &str) -> Vec<(&'static str, Outcome)> {
    resources
        .iter()
        .map(|resource| (*resource, Outcome::Unrestorable(reason.to_string())))
        .collect()
}

/// The two resources a drill proves, each recorded as `backup.drilled.<name>`.
const TRANSCRIPTS: &str = "transcripts";
const MISSIONS: &str = "missions";

<<<<<<< 40293466db32cb4376fe20ba8af611199fea6eaf
/// `--only`'s value: absent, the drill proves both resources (`false`);
/// `missions`, the mission repositories alone (`true`, objective 186).
/// `controllers` (objective 198) is taken before this; anything else
/// (`transcripts`, an empty value) is refused.
pub(super) fn only_missions(value: Option<&str>) -> Result<bool, String> {
    match value {
        None => Ok(false),
        Some(MISSIONS) => Ok(true),
        Some(_) => Err(
            "--only takes missions (the mission repositories) or controllers (the live \
             controllers)"
=======
/// What a drill proves: both resources, or one alone (`--only`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Only {
    /// No `--only`: the transcripts and the missions, against live.
    Both,
    /// `--only missions` (objective 186): the mission repositories alone.
    Missions,
    /// `--only transcripts` (objective 200): the transcript archive alone,
    /// judged by the newest record, nothing live read.
    Transcripts,
}

/// `--only`'s value: absent, both resources; `missions` or `transcripts`,
/// that one alone. Anything else (`repos`, an empty value) is refused.
pub(super) fn only_of(value: Option<&str>) -> Result<Only, String> {
    match value {
        None => Ok(Only::Both),
        Some(MISSIONS) => Ok(Only::Missions),
        Some(TRANSCRIPTS) => Ok(Only::Transcripts),
        Some(_) => Err(
            "--only takes missions or transcripts (the mission repositories, the transcript \
             archive)"
>>>>>>> b822c635abac0fae9289692121a5f4d6d49cbb6e
                .to_string(),
        ),
    }
}

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

/// A chain that lacks a link: said, and the drilled resources recorded
/// failed against the newest stored record (`chain_of` found one); exit 1,
/// or 2 when the store refused a record.
fn unrestorable(
    store: &spawn_postgres::PostgresStore,
    records: &[BundleRecord],
    reason: &str,
    started: Instant,
    resources: &[&'static str],
) -> i32 {
    let reason = format!("the backup cannot be restored: {reason}");
    fail_both(store, records, &reason, started, 1, resources)
}

/// Each resource the drill proves (both, or the one `--only` names)
/// recorded failed with `reason`, as it is, against the
/// newest stored record; said once. Exit `code`, or 2 when the store
/// refused a record. Called for a chain that cannot be restored (exit 1,
/// through `unrestorable`) and for one that cannot be read (exit 2;
/// objective 114: a link's key name refused, the chain key's file not on
/// this host).
fn fail_both(
    store: &spawn_postgres::PostgresStore,
    records: &[BundleRecord],
    reason: &str,
    started: Instant,
    code: i32,
    resources: &[&'static str],
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
    record_verdicts(store, &verdict, &unrestorable_all(resources, &reason)).unwrap_or(code)
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
            "restore-drill: passed: bundle {} (event {}) restored and matches {}",
            report.key, report.event_id, report.against
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

/// The scratch directory's parent (`--into`, else [`DEFAULT_PARENT`]),
/// resolved and checked to be under none of `$HOME`, `$SPAWN_LIVE` and the
/// workspace root ([`drill_parent`]).
fn parent_of(flags: &Flags, settings: &Settings) -> Result<PathBuf, String> {
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
    drill_parent(&parent, &guards)
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
    /// Removed only when every entry under it is a directory or a regular
    /// file (the transcripts drill's, objective 200).
    plain: bool,
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
                        plain: false,
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

    /// Removed only when it holds nothing but directories and regular
    /// files ([`plain_tree`]): anything else is left, and said.
    pub(super) fn plain_only(&mut self) {
        self.plain = true;
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
        if self.plain {
            plain_tree(&self.path)
                .map_err(|error| format!("{error}: the drill folder is left as it is"))?;
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
                plain: self.plain,
                done: false,
            };
            self.done = true;
            if let Err(error) = left.remove() {
                eprintln!("restore-drill: {error}");
            }
        }
    }
}

/// `root` holds only directories and regular files, read without
/// following anything: else the first other entry, named (a symlink in a
/// bundle is never followed).
pub(super) fn plain_tree(root: &Path) -> Result<(), String> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let entries = fs::read_dir(&directory)
            .map_err(|error| format!("{}: {error}", directory.display()))?;
        for entry in entries {
            let entry = entry.map_err(|error| format!("{}: {error}", directory.display()))?;
            let path = entry.path();
            let kind = entry
                .file_type()
                .map_err(|error| format!("{}: {error}", path.display()))?;
            if kind.is_dir() {
                stack.push(path);
            } else if !kind.is_file() {
                let what = if kind.is_symlink() {
                    "a symlink"
                } else {
                    "neither a directory nor a regular file"
                };
                return Err(format!("{} is {what}", path.display()));
            }
        }
    }
    Ok(())
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
    /// What a passed drill matches: live, or the newest record alone (the
    /// transcripts drill).
    against: &'static str,
}

/// Fetch, check and unpack every bundle of the chain, restore, compare.
/// With `--only missions`, each bundle's manifest and repository bundles
/// alone are unpacked, through a pipe, and the transcripts are neither
/// unpacked nor restored nor compared. With `--only transcripts`, each
/// bundle's manifest and transcript slices alone are unpacked the same way
/// (each unpacked tree plain: [`plain_tree`]), restored into the drill's
/// folder and judged by the newest record alone ([`judge_alone`]); nothing
/// live is read.
fn run(
    store: &spawn_postgres::PostgresStore,
    settings: &Settings,
    chain: &[&BundleRecord],
    seal: &Key,
    drill: &Path,
    only: Only,
) -> Result<Report, String> {
    let newest = chain[chain.len() - 1];
    let mut unpacked: Vec<(PathBuf, Value)> = Vec::new();
    for (index, record) in chain.iter().enumerate() {
        let here = drill.join(format!("bundle-{index}"));
        fs::create_dir(&here).map_err(|error| format!("{}: {error}", here.display()))?;
        let manifest = match only {
            Only::Both => fetch_bundle(settings, record, seal, &here)?,
            Only::Missions => fetch_members(settings, record, seal, &here, missions_members)?,
            Only::Transcripts => {
                let manifest = fetch_members(settings, record, seal, &here, transcripts_members)?;
                plain_tree(&here.join("unpacked"))?;
                manifest
            }
        };
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
    if only == Only::Transcripts {
        lines.push(format!(
            "restore-drill: chain {chain_text}: {} bundle(s) fetched, decrypted in a pipe, \
             sha256 as recorded; manifest.json and transcripts/* alone unpacked",
            chain.len()
        ));
        let restored = drill.join("transcripts");
        fs::create_dir(&restored).map_err(|error| format!("{}: {error}", restored.display()))?;
        restore_transcripts(&unpacked, &restored)?;
        let (judged, found) = judge_alone(&restored, &newest.record["transcripts"])?;
        lines.extend(judged);
        // Everything the drill wrote is plain, or it fails (and its folder
        // is left).
        plain_tree(drill)?;
        return Ok(Report {
            key,
            event_id: newest.event_id,
            lines,
            transcripts: found,
            missions,
            unknown: Vec::new(),
            against: "the newest record",
        });
    }
    if only == Only::Missions {
        lines.push(format!(
            "restore-drill: chain {chain_text}: {} bundle(s) fetched, decrypted in a pipe, \
             sha256 as recorded; manifest.json and repos/* alone unpacked",
            chain.len()
        ));
        let mut unknown = Vec::new();
        let found = Found {
            lines: &mut lines,
            mismatches: &mut missions,
            unknown: &mut unknown,
        };
        restore_repos(store, newest, &unpacked, drill, found)?;
        return Ok(Report {
            key,
            event_id: newest.event_id,
            lines,
            transcripts: mismatches,
            missions,
            unknown,
            against: "live",
        });
    }
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
        against: "live",
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
    let encrypted = fetch_ciphertext(settings, record, seal, here)?;
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
    read_manifest(key, record, &unpacked)
}

/// One stored bundle's ciphertext fetched into `here/bundle.gpg`: the
/// object's `x-amz-meta-sha256` the record's, its seal `seal`'s. Only the
/// ciphertext is written; the caller decrypts it.
fn fetch_ciphertext(
    settings: &Settings,
    record: &BundleRecord,
    seal: &Key,
    here: &Path,
) -> Result<PathBuf, String> {
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
    Ok(encrypted)
}

/// The manifest unpacked into `unpacked`, its kind the record's.
fn read_manifest(key: &str, record: &BundleRecord, unpacked: &Path) -> Result<Value, String> {
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

/// One stored bundle for `--only missions` (objective 186) or `--only
/// transcripts` (objective 200): its ciphertext fetched into `here` as
/// `fetch_bundle` does, then never decrypted to a file. The decrypted
/// stream goes through a pipe into `tar -t`, and its sha256 must be the
/// record's before anything is extracted; the names `choose` takes from
/// that listing (`missions_members`: the manifest and the files directly
/// under `repos/`; `transcripts_members`: the manifest and the files under
/// `transcripts/`) are written to `here/names`, and a second
/// stream goes into `tar -x` of those names alone into `here/unpacked`, its
/// sha256 the same. The manifest's kind is then the record's.
fn fetch_members(
    settings: &Settings,
    record: &BundleRecord,
    seal: &Key,
    here: &Path,
    choose: fn(&str) -> Result<Vec<String>, String>,
) -> Result<Value, String> {
    let key = record.record["keys"][0].as_str().unwrap_or_default();
    let expected = record.record["sha256"].as_str().unwrap_or_default();
    let encrypted = fetch_ciphertext(settings, record, seal, here)?;
    let mut list = Command::new("tar");
    list.args(["-tf", "-"]);
    let listed = stream_into_tar(seal, &encrypted, list)?;
    listed.judge(key, expected, "tar -t")?;
    let members = choose(&listed.stdout).map_err(|error| format!("{key}: {error}"))?;
    let names = here.join("names");
    let mut text = String::new();
    for member in &members {
        text.push_str(member);
        text.push('\n');
    }
    fs::write(&names, text).map_err(|error| format!("{}: {error}", names.display()))?;
    let unpacked = here.join("unpacked");
    fs::create_dir(&unpacked).map_err(|error| format!("{}: {error}", unpacked.display()))?;
    let mut extract = Command::new("tar");
    extract
        .args(["-xf", "-", "-C"])
        .arg(&unpacked)
        .args(["--no-wildcards", "--verbatim-files-from", "-T"])
        .arg(&names);
    let extracted = stream_into_tar(seal, &encrypted, extract)?;
    extracted.judge(key, expected, "tar -x")?;
    fs::remove_file(&encrypted).map_err(|error| format!("{}: {error}", encrypted.display()))?;
    fs::remove_file(&names).map_err(|error| format!("{}: {error}", names.display()))?;
    read_manifest(key, record, &unpacked)
}

/// What a bundle's listing names that `--only missions` extracts: the
/// manifest and each file directly under `repos/`, as listed (`./` and
/// all), once each; never a transcript slice, a deeper path, a directory,
/// `.` or `..`, nor a name tar had to quote. A listing without the manifest
/// is refused: an empty list of names would make `tar -x` extract every
/// member.
pub(super) fn missions_members(listing: &str) -> Result<Vec<String>, String> {
    members_of(listing, |name| {
        name.strip_prefix("repos/")
            .is_some_and(|file| plain_part(file) && !file.contains('/'))
    })
}

/// What a bundle's listing names that `--only transcripts` extracts: the
/// manifest and each file under `transcripts/`, at any depth, as listed,
/// once each; never a repository bundle, a directory (a name ending in
/// `/`), a name with an empty, `.` or `..` part, nor a name tar had to
/// quote. A listing without the manifest is refused.
pub(super) fn transcripts_members(listing: &str) -> Result<Vec<String>, String> {
    members_of(listing, |name| {
        name.strip_prefix("transcripts/")
            .is_some_and(|file| file.split('/').all(plain_part))
    })
}

/// A part of a member's name as taken: not empty, `.` or `..`, and nothing
/// tar quotes (a backslash, a control character).
fn plain_part(part: &str) -> bool {
    !part.is_empty()
        && part != "."
        && part != ".."
        && !part.contains('\\')
        && !part.chars().any(char::is_control)
}

/// The manifest and the names `wanted` takes (each without its leading
/// `./`), as listed, once each; refused without the manifest.
fn members_of(listing: &str, wanted: impl Fn(&str) -> bool) -> Result<Vec<String>, String> {
    let mut members: Vec<String> = Vec::new();
    let mut manifest = false;
    for line in listing.lines() {
        let name = line.strip_prefix("./").unwrap_or(line);
        let wanted = if name == "manifest.json" {
            manifest = true;
            true
        } else {
            wanted(name)
        };
        if wanted && !members.iter().any(|member| member == line) {
            members.push(line.to_string());
        }
    }
    if !manifest {
        return Err("the bundle lists no manifest.json".to_string());
    }
    Ok(members)
}

/// One step of a stream: whether it exited 0, and its stderr.
struct Ran {
    ok: bool,
    stderr: String,
}

/// A decrypted stream run through tar: its sha256, what its magic said it
/// is, what each step said, and tar's stdout.
struct Streamed {
    sha256: String,
    compression: &'static str,
    gpg: Ran,
    decompressor: Option<(&'static str, Ran)>,
    tar: Ran,
    stdout: String,
}

impl Streamed {
    /// The stream judged: gpg opened it, its sha256 is `expected`, then the
    /// decompressor and tar (`what`) read it.
    fn judge(&self, key: &str, expected: &str, what: &str) -> Result<(), String> {
        if !self.gpg.ok {
            return Err(format!("gpg: {}", self.gpg.stderr.trim()));
        }
        if self.sha256 != expected {
            return Err(format!(
                "{key} decrypts to sha256 {}, not the record's {expected}",
                self.sha256
            ));
        }
        if let Some((program, ran)) = &self.decompressor
            && !ran.ok
        {
            return Err(format!("{program}: {}", ran.stderr.trim()));
        }
        if !self.tar.ok {
            return Err(format!(
                "{key}: {what} ({}): {}",
                self.compression,
                self.tar.stderr.trim()
            ));
        }
        Ok(())
    }
}

/// A reader drained on a thread of its own.
fn drain<R: Read + Send + 'static>(reader: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut reader) = reader {
            let _ = reader.read_to_end(&mut bytes);
        }
        bytes
    })
}

/// A drained reader's bytes, as text.
fn drained(handle: std::thread::JoinHandle<Vec<u8>>) -> String {
    String::from_utf8_lossy(&handle.join().unwrap_or_default()).into_owned()
}

/// The children of one stream, killed and reaped if it stops part way.
struct Running(Vec<Child>);

impl Drop for Running {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// gpg decrypting `encrypted` to its stdout, a pipe: never to a file.
fn start_gpg(seal: &Key, encrypted: &Path) -> Result<Child, String> {
    Command::new("gpg")
        .args([
            "--batch",
            "--quiet",
            "--no-tty",
            "--pinentry-mode",
            "loopback",
            "--no-symkey-cache",
        ])
        .arg("--passphrase-file")
        .arg(&seal.file)
        .arg("--decrypt")
        .arg(encrypted)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("gpg: {error}"))
}

/// The decompressor `compression` names (none for a plain tar) and `tar`
/// after it, started into `running`: the pipe the stream is written to,
/// and the drained stderr of each and tar's stdout.
struct TarPipes {
    sink: Option<ChildStdin>,
    decompressor: Option<(&'static str, std::thread::JoinHandle<Vec<u8>>)>,
    stdout: std::thread::JoinHandle<Vec<u8>>,
    stderr: std::thread::JoinHandle<Vec<u8>>,
}

fn start_tar(
    compression: &str,
    mut tar: Command,
    running: &mut Running,
) -> Result<TarPipes, String> {
    let program = match compression {
        "gzip" => Some(("gzip", &["-dc"][..])),
        "zstd" => Some(("zstd", &["-q", "-dc"][..])),
        _ => None,
    };
    let mut sink = None;
    let mut decompressor = None;
    if let Some((name, arguments)) = program {
        let mut child = Command::new(name)
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("{name}: {error}"))?;
        sink = child.stdin.take();
        let out = child.stdout.take();
        decompressor = Some((name, drain(child.stderr.take())));
        running.0.push(child);
        tar.stdin(out.map_or_else(Stdio::null, Stdio::from));
    } else {
        tar.stdin(Stdio::piped());
    }
    let mut child = tar
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("tar: {error}"))?;
    if sink.is_none() {
        sink = child.stdin.take();
    }
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    running.0.push(child);
    Ok(TarPipes {
        sink,
        decompressor,
        stdout,
        stderr,
    })
}

/// Bytes written on to the next step while it reads; once it stops (a
/// broken pipe) nothing more is written to it, and the stream is still
/// read through and hashed.
fn feed(sink: &mut Option<ChildStdin>, bytes: &[u8]) {
    if let Some(pipe) = sink
        && pipe.write_all(bytes).is_err()
    {
        *sink = None;
    }
}

/// `encrypted` decrypted by gpg into a pipe, never a file: the stream
/// hashed as it passes, decompressed by the program its magic names (zstd,
/// gzip, or none) and fed into `tar`, whose stdout is kept. Nothing is
/// judged here (`Streamed::judge`).
fn stream_into_tar(seal: &Key, encrypted: &Path, tar: Command) -> Result<Streamed, String> {
    let mut running = Running(Vec::new());
    let mut gpg = start_gpg(seal, encrypted)?;
    let plain = gpg.stdout.take();
    let gpg_err = drain(gpg.stderr.take());
    running.0.push(gpg);
    let mut plain = plain.ok_or("gpg: no stdout")?;
    let mut head = Vec::with_capacity(4);
    (&mut plain)
        .take(4)
        .read_to_end(&mut head)
        .map_err(|error| format!("gpg: {error}"))?;
    let compression = bundle_compression(&head);
    let pipes = start_tar(compression, tar, &mut running)?;
    let mut sink = pipes.sink;
    let mut hasher = Sha256::new();
    hasher.update(&head);
    feed(&mut sink, &head);
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = match plain.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("gpg: {error}")),
        };
        hasher.update(&buffer[..read]);
        feed(&mut sink, &buffer[..read]);
    }
    drop(sink);
    drop(plain);
    let mut statuses = Vec::new();
    for mut child in std::mem::take(&mut running.0) {
        statuses.push(
            child
                .wait()
                .map_err(|error| format!("wait: {error}"))?
                .success(),
        );
    }
    let ok = |index: usize| statuses.get(index).copied().unwrap_or(false);
    let decompressor = pipes.decompressor.map(|(name, stderr)| {
        (
            name,
            Ran {
                ok: ok(1),
                stderr: drained(stderr),
            },
        )
    });
    Ok(Streamed {
        sha256: hex(&hasher.finalize()),
        compression,
        gpg: Ran {
            ok: ok(0),
            stderr: drained(gpg_err),
        },
        decompressor,
        tar: Ran {
            ok: ok(statuses.len().saturating_sub(1)),
            stderr: drained(pipes.stderr),
        },
        stdout: drained(pipes.stdout),
    })
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

/// The restored transcripts judged by the newest record alone (`--only
/// transcripts`, objective 200), nothing live read: its lines (the files
/// and bytes restored, and "<k> of <n> files as the newest record's
/// sha256") and a mismatch for each file the record names that is not
/// restored at its size and its sha256 in `transcripts.sha256`. A record
/// bundled before objective 149 names no sha256: that fails each file. A
/// path the record names that is not a plain relative one is a mismatch,
/// never opened.
pub(super) fn judge_alone(
    restored: &Path,
    transcripts: &Value,
) -> Result<(Vec<String>, Vec<String>), String> {
    let files = files_under(restored)?;
    let mut bytes = 0u64;
    for file in &files {
        let path = restored.join(file);
        let meta = fs::metadata(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        bytes = bytes.saturating_add(meta.len());
    }
    let mut lines = vec![format!(
        "restore-drill: transcripts: {} files ({bytes} bytes) restored into the drill's own \
         folder; judged by the newest record alone, nothing live read",
        files.len()
    )];
    let sizes = transcripts["sizes"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    let hashes = transcripts["sha256"].as_object();
    if hashes.is_none() {
        lines.push(
            "restore-drill: transcripts: the newest record names no per-file sha256 (bundled \
             before objective 149): no file can be judged by it"
                .to_string(),
        );
    }
    let mut same = 0usize;
    let mut mismatches = Vec::new();
    for (named, size) in &sizes {
        let want_size = size.as_u64().unwrap_or(0);
        let Some(path) = relative(named) else {
            mismatches.push(format!(
                "the newest record names the transcript path {named:?}"
            ));
            continue;
        };
        let Some(want) = hashes
            .and_then(|hashes| hashes.get(named))
            .and_then(Value::as_str)
        else {
            mismatches.push(format!(
                "transcripts/{named}: the newest record sizes it ({want_size} bytes) and names \
                 no sha256"
            ));
            continue;
        };
        let file = restored.join(path);
        match fs::symlink_metadata(&file) {
            Ok(meta) if meta.is_file() && meta.len() == want_size => {}
            Ok(meta) if meta.is_file() => {
                mismatches.push(format!(
                    "transcripts/{named}: the chain restores {} bytes; the newest record says \
                     {want_size}",
                    meta.len()
                ));
                continue;
            }
            _ => {
                mismatches.push(format!(
                    "transcripts/{named}: the newest record names it ({want_size} bytes); the \
                     chain restores none"
                ));
                continue;
            }
        }
        if sha256_file(&file)? == want {
            same += 1;
        } else {
            mismatches.push(format!(
                "transcripts/{named}: the restored bytes are not the sha256 the newest record names"
            ));
        }
    }
    lines.push(format!(
        "restore-drill: transcripts: {same} of {} files as the newest record's sha256",
        sizes.len()
    ));
    Ok((lines, mismatches))
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
/// whole and connected (`git fsck --full`: every object read).
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
    // --full: every object read whole, not only the links between them.
    fsck.arg("--git-dir")
        .arg(mirror)
        .args(["fsck", "--full", "--no-dangling", "--no-progress"]);
    run_git(fsck, "git fsck --full")?;
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
            "restore-drill: missions {source}: {} refs restored (clone --mirror, fsck --full clean); \
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
    now.id_or_why("restored at", commit)
        .unwrap_or_else(|why| why)
}

#[cfg(test)]
mod tests {
    use super::{
        Chain, DrillDir, Only, Outcome, chain_of, compare_refs, compare_transcripts, drill_parent,
        judge_by_record, missions_members, only_of, outcome, sha256_prefix,
    };
    use crate::backup::BundleRecord;
    use crate::commit_now::CommitNow;
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
        let report = compare_refs(&restored, &live, held, |old, new| {
            old == "c1" && new == "c2"
        })
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

    /// The outcome of `report`'s heads: both counted, the unknown never
    /// called a mismatch.
    fn outcomes_count_the_unknown_apart(report: &super::Refs) {
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
        assert!(matches!(
            outcome("missions", &[], &[], "k"),
            Outcome::Passed
        ));
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
            (
                report.restored,
                report.same,
                report.newer,
                report.added,
                report.removed
            ),
            (5, 1, 1, 0, 0)
        );
        assert_eq!(
            report.dropped,
            [
                "refs/heads/e: restored at oe was dropped by the history rewrite (the commit map \
              names no new commit for it)"
            ]
        );
        assert_eq!(
            report.unknown,
            [
                "refs/heads/lost: restored at ol is an unknown commit (in neither the commit map \
              nor the source)"
            ]
        );
        assert_eq!(
            report.mismatches,
            [
                "refs/heads/rewritten: restored at o2 (now n3); live is at n9, which does not \
              descend from it"
            ]
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
        assert!(
            plain
                .line("/src")
                .ends_with("5 removed since, 0 mismatch(es)")
        );
        outcomes_count_the_unknown_apart(&report);
        // A store error is the error, not a verdict.
        let failed = compare_refs(
            &restored,
            &live,
            |_: &str| Err("the store is gone".to_string()),
            descends,
        );
        assert_eq!(failed.err(), Some("the store is gone".to_string()));
    }

    #[test]
    fn a_missions_only_drill_extracts_the_manifest_and_the_repository_bundles_alone() {
        let listing = "./\n./manifest.json\n./repos/\n./repos/repo-0.bundle\n\
                       ./repos/repo-1.bundle\n./repos/deeper/x.bundle\n./repos/..\n./repos/.\n\
                       ./repos/../transcripts/a.jsonl\n./repos/odd\\nname\n./transcripts/\n\
                       ./transcripts/agent_supervisor/0001.jsonl\n../repos/escape.bundle\n\
                       ./repos/repo-0.bundle\n";
        assert_eq!(
            missions_members(listing),
            Ok(vec![
                "./manifest.json".to_string(),
                "./repos/repo-0.bundle".to_string(),
                "./repos/repo-1.bundle".to_string(),
            ])
        );
        // Names without the leading ./ are taken as listed.
        assert_eq!(
            missions_members("manifest.json\nrepos/a.bundle\ntranscripts/b.jsonl\n"),
            Ok(vec![
                "manifest.json".to_string(),
                "repos/a.bundle".to_string()
            ])
        );
        // No manifest: refused, never an empty list of names (tar -x would
        // extract every member).
        assert!(missions_members("./transcripts/a.jsonl\n./repos/a.bundle\n").is_err());
        assert!(missions_members("").is_err());

        assert_eq!(only_of(None), Ok(Only::Both));
        assert_eq!(only_of(Some("missions")), Ok(Only::Missions));
        assert_eq!(only_of(Some("transcripts")), Ok(Only::Transcripts));
        for refused in [
            "repos",
            "",
            "Missions",
            "missions,transcripts",
            "Transcripts",
        ] {
            assert!(only_of(Some(refused)).is_err(), "{refused:?}");
        }
    }

    /// Objective 200: `--only transcripts` takes from a bundle's listing the
    /// manifest and the files under `transcripts/` alone, as listed; never a
    /// repository bundle, a directory, a `.` or `..` part, nor a quoted name.
    /// A listing without the manifest is refused.
    #[test]
    fn a_transcripts_only_drill_extracts_the_manifest_and_the_transcript_slices_alone() {
        use super::transcripts_members;
        let listing = "./\n./manifest.json\n./repos/\n./repos/repo-0.bundle\n./transcripts/\n\
                       ./transcripts/agent/\n./transcripts/agent/a.jsonl\n\
                       ./transcripts/agent/sub/b.jsonl\n./transcripts/../escape\n\
                       ./transcripts/agent/./c.jsonl\n./transcripts//d.jsonl\n\
                       ./transcripts/agent/odd\\nname.jsonl\n./transcripts/agent/a.jsonl\n\
                       ./transcripts\n./other/transcripts/e.jsonl\n";
        assert_eq!(
            transcripts_members(listing),
            Ok(vec![
                "./manifest.json".to_string(),
                "./transcripts/agent/a.jsonl".to_string(),
                "./transcripts/agent/sub/b.jsonl".to_string(),
            ])
        );
        assert_eq!(
            transcripts_members("manifest.json\nrepos/a.bundle\ntranscripts/x/b.jsonl\n"),
            Ok(vec![
                "manifest.json".to_string(),
                "transcripts/x/b.jsonl".to_string()
            ])
        );
        // An increment with no transcript: the manifest alone.
        assert_eq!(
            transcripts_members("./manifest.json\n./repos/a.bundle\n"),
            Ok(vec!["./manifest.json".to_string()])
        );
        assert!(transcripts_members("./transcripts/a/b.jsonl\n./repos/a.bundle\n").is_err());
        assert!(transcripts_members("").is_err());
    }

    /// Objective 200: a drill folder is plain when it holds only directories
    /// and regular files; a symlink, even under a transcript's name, is
    /// refused and named, never followed, and a plain-only DrillDir with one
    /// is left, not removed.
    #[cfg(unix)]
    #[test]
    fn the_drill_folder_is_plain_only_with_directories_and_regular_files() {
        use super::plain_tree;
        let own = Own::new("plain");
        let tree = own.0.join("tree");
        write(&tree.join("transcripts/agent/a.jsonl"), b"{}\n");
        fs::create_dir_all(tree.join("transcripts/empty")).expect("dir");
        assert_eq!(plain_tree(&tree), Ok(()));

        let outside = own.0.join("outside.jsonl");
        write(&outside, b"never to be opened\n");
        let link = tree.join("transcripts/agent/b.jsonl");
        std::os::unix::fs::symlink(&outside, &link).expect("symlink");
        let refused = plain_tree(&tree).expect_err("a symlink is refused");
        assert!(refused.contains("transcripts/agent/b.jsonl"), "{refused}");
        assert!(refused.contains("a symlink"), "{refused}");

        // A plain-only drill folder holding a symlink is left, said.
        let mut drill = DrillDir::create(&own.0).expect("drill");
        drill.plain_only();
        let path = drill.path().to_path_buf();
        std::os::unix::fs::symlink(&outside, path.join("c.jsonl")).expect("symlink");
        let left = drill.remove().expect_err("not removed");
        assert!(left.contains("c.jsonl") && left.contains("left"), "{left}");
        assert!(path.is_dir());
        assert_eq!(
            fs::read(&outside).expect("outside"),
            b"never to be opened\n"
        );
        fs::remove_file(path.join("c.jsonl")).expect("unlink");

        // Plain again: removed.
        let mut drill = DrillDir::create(&own.0).expect("drill");
        drill.plain_only();
        let path = drill.path().to_path_buf();
        write(&path.join("transcripts/a.jsonl"), b"{}\n");
        drill.remove().expect("removed");
        assert!(!path.exists());
    }

    /// Objective 200: the bytes a transcripts drill writes are counted from
    /// the chain's records (each link's ciphertext and slices, the newest
    /// record's files), and the drill refuses, naming the floor, when they
    /// would leave the file system under it.
    #[test]
    fn a_transcripts_only_drill_needs_its_bytes_above_the_floor() {
        use super::{floor_refusal, transcripts_bytes};
        let mut full = record(1, "full", None, true);
        full.record["stored_bytes"] = json!(1000);
        full.record["transcripts"] = json!({"bytes": 300, "sizes": {"a": 100, "b": 200}});
        let mut incr = record(2, "incremental", Some(1), true);
        incr.record["stored_bytes"] = json!(50);
        incr.record["transcripts"] = json!({"bytes": 20, "sizes": {"a": 120, "b": 200}});
        assert_eq!(
            transcripts_bytes(&[&full, &incr]),
            1000 + 300 + 50 + 20 + 120 + 200
        );
        assert_eq!(transcripts_bytes(&[&full]), 1000 + 300 + 300);

        let mib = 1024 * 1024;
        let parent = Path::new("/data/tmp");
        assert_eq!(floor_refusal(1_000, 40 * mib + 1_000, 40, parent), Ok(()));
        let refused = floor_refusal(1_001, 40 * mib + 1_000, 40, parent).expect_err("under");
        assert!(refused.contains("floor of 40 MB"), "{refused}");
        assert!(refused.contains("SPAWN_BACKUP_DRILL_FLOOR_MB"), "{refused}");
        assert!(!refused.contains('\n'), "one line: {refused}");
        // More than is free: refused, even with no floor.
        assert!(floor_refusal(2_000, 1_000, 0, parent).is_err());
        // The default floor, 40 GB.
        assert!(floor_refusal(0, 40_960 * mib - 1, super::FLOOR_MB, parent).is_err());
        assert_eq!(
            floor_refusal(0, 40_960 * mib, super::FLOOR_MB, parent),
            Ok(())
        );
    }
}
