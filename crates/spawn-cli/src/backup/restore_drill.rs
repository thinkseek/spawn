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
//!   restored one is a mismatch.
//!
//! One summary line per resource, then the verdict: exit 0 when nothing
//! older than the bundle differs, 1 on any such mismatch (or a backup that
//! cannot be restored: a broken chain, a sha256 that is not the record's), 2
//! when the drill cannot run.
//!
//! It writes nothing live and records nothing: no lock, no record, git and
//! the files read only. The key is read as `spawn backup verify` reads it
//! (`SPAWN_BACKUP_KEY_FILE`, mode 600, given to gpg by path) and nothing of
//! it is printed. The scratch directory is made with mode 700 under `--into`
//! (default `/data/tmp`), which may not be under `$HOME`, `$SPAWN_LIVE` or
//! the workspace root; it is removed at the end (kept with `--keep`), and
//! only once it is checked to be the directory this run made.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{
    BundleRecord, Settings, bundle_records, decompress_bundle, decrypt, get_object_bytes,
    header_value, heads_of, hex, output, random_hex, sha256_file,
};
use crate::Flags;
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
            println!("restore-drill: FAILED: the backup cannot be restored: {reason}");
            return Ok(1);
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
    let drill = DrillDir::create(&parent)?;
    let result = run(&settings, &chain, drill.path());
    let code = match result {
        Ok(report) => {
            for line in &report.lines {
                println!("{line}");
            }
            for line in report.mismatches.iter().take(NAMED) {
                eprintln!("restore-drill: mismatch: {line}");
            }
            if report.mismatches.len() > NAMED {
                eprintln!(
                    "restore-drill: and {} more mismatches",
                    report.mismatches.len() - NAMED
                );
            }
            if report.mismatches.is_empty() {
                println!(
                    "restore-drill: passed: bundle {} (event {}) restored and matches live",
                    report.key, report.event_id
                );
                0
            } else {
                println!(
                    "restore-drill: FAILED: {} mismatch(es) older than bundle {} (event {})",
                    report.mismatches.len(),
                    report.key,
                    report.event_id
                );
                1
            }
        }
        Err(error) => {
            println!("restore-drill: FAILED: the backup cannot be restored: {error}");
            1
        }
    };
    if flags.switch("keep") {
        println!("restore-drill: kept {}", drill.path().display());
        drill.keep();
    } else {
        drill.remove()?;
    }
    Ok(code)
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
    let stored = |record: &BundleRecord| {
        record.record["keys"]
            .as_array()
            .is_some_and(|keys| !keys.is_empty())
    };
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
    mismatches: Vec<String>,
}

/// Fetch, check and unpack every bundle of the chain, restore, compare.
fn run(settings: &Settings, chain: &[&BundleRecord], drill: &Path) -> Result<Report, String> {
    let newest = chain[chain.len() - 1];
    let mut unpacked: Vec<(PathBuf, Value)> = Vec::new();
    for (index, record) in chain.iter().enumerate() {
        let here = drill.join(format!("bundle-{index}"));
        fs::create_dir(&here).map_err(|error| format!("{}: {error}", here.display()))?;
        let manifest = fetch_bundle(settings, record, &here)?;
        unpacked.push((here.join("unpacked"), manifest));
    }
    let key = newest.record["keys"][0]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let bundle_ms = newest.at_ms;
    let mut lines = Vec::new();
    let mut mismatches = Vec::new();
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
    let window_ms = settings.transcript_days.saturating_mul(86_400_000);
    let live = settings.root.join(".transcripts");
    let transcripts = compare_transcripts(&restored, &live, bundle_ms, window_ms)?;
    lines.push(transcripts.line(&live));
    mismatches.extend(transcripts.mismatches);

    restore_repos(newest, &unpacked, drill, &mut lines, &mut mismatches)?;
    Ok(Report {
        key,
        event_id: newest.event_id,
        lines,
        mismatches,
    })
}

/// Every mission source of the newest record restored under `drill/repos`
/// and compared with live: a line each, and the mismatches. A source the
/// newest bundle could not read (`missing`) is one: the backup holds none
/// of it.
fn restore_repos(
    newest: &BundleRecord,
    unpacked: &[(PathBuf, Value)],
    drill: &Path,
    lines: &mut Vec<String>,
    mismatches: &mut Vec<String>,
) -> Result<(), String> {
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
        let refs = compare_refs(&heads, &live_heads, |old, new| descends(source, old, new));
        lines.push(refs.line(source));
        mismatches.extend(
            refs.mismatches
                .iter()
                .map(|mismatch| format!("{source}: {mismatch}")),
        );
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
/// and unpacked into `here/unpacked`; its manifest's kind the record's.
fn fetch_bundle(settings: &Settings, record: &BundleRecord, here: &Path) -> Result<Value, String> {
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
    let encrypted = here.join("bundle.gpg");
    fs::write(&encrypted, &bytes).map_err(|error| format!("{}: {error}", encrypted.display()))?;
    drop(bytes);
    let raw = here.join("bundle.raw");
    decrypt(settings, &encrypted, &raw)?;
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

/// The sha256 of a file's first `bytes` bytes.
fn sha256_prefix(path: &Path, bytes: u64) -> Result<String, String> {
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

/// One source's refs restored and live, compared.
#[derive(Debug, Default)]
pub(super) struct Refs {
    pub(super) restored: usize,
    pub(super) same: usize,
    pub(super) newer: usize,
    pub(super) added: usize,
    pub(super) removed: usize,
    pub(super) mismatches: Vec<String>,
}

impl Refs {
    fn line(&self, source: &str) -> String {
        format!(
            "restore-drill: missions {source}: {} refs restored (clone --mirror, fsck connected); \
             against live: {} the same, {} moved on since (descendants), {} added since, {} \
             removed since, {} mismatch(es)",
            self.restored,
            self.same,
            self.newer,
            self.added,
            self.removed,
            self.mismatches.len()
        )
    }
}

/// The restored refs against live's (`refs/heads` and `refs/tags`), with
/// `descends(old, new)` asked of live.
pub(super) fn compare_refs(
    restored: &BTreeMap<String, String>,
    live: &BTreeMap<String, String>,
    descends: impl Fn(&str, &str) -> bool,
) -> Refs {
    let mut out = Refs {
        restored: restored.len(),
        ..Refs::default()
    };
    for (name, commit) in restored {
        match live.get(name) {
            None => out.removed += 1,
            Some(now) if now == commit => out.same += 1,
            Some(now) if descends(commit, now) => out.newer += 1,
            Some(now) => out.mismatches.push(format!(
                "{name}: restored at {commit}; live is at {now}, which does not descend from it"
            )),
        }
    }
    out.added = live
        .keys()
        .filter(|name| !restored.contains_key(*name))
        .count();
    out
}

#[cfg(test)]
mod tests {
    use super::{Chain, DrillDir, chain_of, compare_refs, compare_transcripts, drill_parent};
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
        let report = compare_refs(&restored, &live, |old, new| old == "c1" && new == "c2");
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
}
