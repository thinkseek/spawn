//! `spawn backup restore-drill --only sync` (objective 217): the fleet's
//! notes folder, which each bundle holds since objective 216 (the record's
//! `sync` part, crate::backup::sync_dir), restored from the newest bundle
//! chain into the drill's own folder and compared with the folder, read
//! only.
//!
//! From each link of the chain only `manifest.json`, `sync/manifest.json`
//! and the files under `sync/files/` are unpacked ([`members`]), through
//! the same pipe as `--only transcripts`. Then ([`restore`]):
//!
//! - the newest record must hold the part: one with no `sync` (bundled with
//!   no notes folder set) or a dropped one (`sync.refused`) is a backup
//!   that cannot be restored;
//! - each link with a part: its `sync/manifest.json` must have the sha256
//!   its record names (`sync.manifest_sha256`); a link with no part, or a
//!   dropped one, holds no file;
//! - each file of the newest manifest comes from the newest link whose
//!   `taken` names it, into `<drill>/sync/<path>`; a file no link holds, or
//!   one restored at another size or sha256 than the newest manifest's,
//!   cannot be restored.
//!
//! Then the folder the newest record names (`sync.dir`) is compared with
//! the restored one ([`compare`]), read only: it is never written to,
//! created in or removed from; its regular files are walked and a symbolic
//! link is never followed (each file is opened `O_NOFOLLOW`). The bundle's
//! start is its `manifest.json`'s `created_at_ms`: a file the bundle read
//! was read at or after it, so any write since gives it such an mtime.
//!
//! - a file of the newest manifest: the same sha256 live, the same; other
//!   bytes with an mtime at or after the start, changed since (expected);
//!   other bytes with an earlier mtime, a mismatch (the backup holds older
//!   bytes than the folder had when the bundle began); no longer a regular
//!   file live (gone while the drill reads it included), removed since
//!   (expected); a file that cannot be read, a mismatch;
//! - a live file the manifest lacks: listed in the newest manifest's
//!   `skipped` or `changed_during_read`, said apart; else written at or
//!   after the start, made since (expected); else a mismatch (the backup
//!   lacks a file older than it);
//! - a folder that cannot be read is one mismatch naming it.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{BundleRecord, hex, members_of, plain_part, read_plain_file, relative};

/// The resource, recorded as `backup.drilled.sync`.
pub(super) const SYNC: &str = "sync";

/// One read: 64 KiB, so no file is held whole.
const CHUNK: usize = 64 * 1024;

/// What a bundle's listing names that `--only sync` extracts: the manifest,
/// `sync/manifest.json` and each file under `sync/files/`, at any depth, as
/// listed, once each; never a transcript slice, a repository bundle, a
/// directory (a name ending in `/`), a name with an empty, `.` or `..` part,
/// nor a name tar had to quote. A listing without the manifest is refused.
pub(super) fn members(listing: &str) -> Result<Vec<String>, String> {
    members_of(listing, |name| {
        name == "sync/manifest.json"
            || name
                .strip_prefix("sync/files/")
                .is_some_and(|file| file.split('/').all(plain_part))
    })
}

/// The bytes a sync drill writes, from the chain's records alone: each
/// link's ciphertext (`stored_bytes`) and the notes files it took
/// (`sync.taken_bytes`), and the newest record's notes folder
/// (`sync.bytes`), which they are restored into.
pub(super) fn bytes(chain: &[&BundleRecord]) -> u64 {
    let fetched = chain.iter().fold(0u64, |sum, link| {
        sum.saturating_add(link.record["stored_bytes"].as_u64().unwrap_or(0))
            .saturating_add(link.record["sync"]["taken_bytes"].as_u64().unwrap_or(0))
    });
    let restored = chain
        .last()
        .map_or(0, |newest| newest.record["sync"]["bytes"].as_u64().unwrap_or(0));
    fetched.saturating_add(restored)
}

/// The folder the newest record's notes part names, if it names one (a
/// dropped part names it too).
pub(super) fn named_dir(newest: &BundleRecord) -> Option<PathBuf> {
    newest.record["sync"]["dir"].as_str().map(PathBuf::from)
}

/// Why the newest record holds no notes folder to restore: none at all, or
/// a dropped part. `None` when it holds one.
pub(super) fn no_part(newest: &BundleRecord) -> Option<String> {
    let sync = &newest.record["sync"];
    if !sync.is_object() {
        return Some(format!(
            "the newest bundle (event {}) holds no notes folder: it was bundled with no \
             SPAWN_BACKUP_SYNC_DIR",
            newest.event_id
        ));
    }
    if let Some(reason) = sync["refused"].as_str() {
        return Some(format!(
            "the newest bundle (event {}) dropped its notes folder: {reason}",
            newest.event_id
        ));
    }
    None
}

/// One file of a notes manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct File_ {
    pub(super) path: String,
    pub(super) size: u64,
    pub(super) sha256: String,
}

/// The notes folder restored: its files (the newest manifest's), how many
/// links of the chain gave them, and what the newest manifest says apart.
#[derive(Debug, Default)]
pub(super) struct Restored {
    pub(super) files: Vec<File_>,
    pub(super) bytes: u64,
    pub(super) links: usize,
    /// The newest manifest's `skipped` and `changed_during_read`.
    pub(super) apart: BTreeSet<String>,
}

impl Restored {
    pub(super) fn line(&self) -> String {
        format!(
            "restore-drill: the notes folder: {} file(s), {} bytes, restored from {} bundle(s) \
             of the chain, each the newest manifest's sha256",
            self.files.len(),
            self.bytes,
            self.links
        )
    }
}

/// A link's notes manifest, read from its unpacked tree, its sha256 the
/// record's `sync.manifest_sha256`; `None` for a link with no part or a
/// dropped one (it holds no file).
fn link_manifest(record: &BundleRecord, unpacked: &Path) -> Result<Option<Value>, String> {
    let sync = &record.record["sync"];
    if !sync.is_object() || sync.get("refused").is_some() {
        return Ok(None);
    }
    let want = sync["manifest_sha256"].as_str().ok_or_else(|| {
        format!(
            "bundle {} names no sha256 of its notes manifest",
            record.event_id
        )
    })?;
    let bytes = read_plain_file(&unpacked.join("sync/manifest.json"))
        .map_err(|why| format!("bundle {}'s sync/manifest.json: {why}", record.event_id))?;
    let got = hex(&Sha256::digest(&bytes));
    if got != want {
        return Err(format!(
            "bundle {}'s sync/manifest.json has sha256 {got}, not its record's {want}",
            record.event_id
        ));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("bundle {}'s sync/manifest.json: {error}", record.event_id))
}

/// The names a manifest lists under `field`.
fn names(manifest: &Value, field: &str) -> BTreeSet<String> {
    manifest[field]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|name| name.as_str().map(str::to_string))
        .collect()
}

/// Copy `source` to `target` in [`CHUNK`]s: its length and sha256.
fn copy_hashed(source: &Path, target: &Path) -> Result<(u64, String), String> {
    let mut input = File::open(source).map_err(|error| format!("{}: {error}", source.display()))?;
    let mut output =
        File::create(target).map_err(|error| format!("{}: {error}", target.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; CHUNK];
    let mut length = 0u64;
    loop {
        let read = input
            .read(&mut buffer)
            .map_err(|error| format!("{}: {error}", source.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        output
            .write_all(&buffer[..read])
            .map_err(|error| format!("{}: {error}", target.display()))?;
        length += read as u64;
    }
    output
        .flush()
        .map_err(|error| format!("{}: {error}", target.display()))?;
    Ok((length, hex(&hasher.finalize())))
}

/// The newest manifest's files restored into `restored`, each from the
/// newest link (of `chain`, oldest first, unpacked as `unpacked`) whose
/// `taken` names it. Any error is a backup that cannot be restored.
pub(super) fn restore(
    chain: &[&BundleRecord],
    unpacked: &[(PathBuf, Value)],
    restored: &Path,
) -> Result<Restored, String> {
    let newest = chain.last().ok_or("the chain has no link")?;
    if let Some(why) = no_part(newest) {
        return Err(why);
    }
    let mut links: Vec<(&Path, BTreeSet<String>)> = Vec::new();
    let mut newest_manifest = None;
    for (index, (record, (directory, _))) in chain.iter().zip(unpacked).enumerate() {
        let manifest = link_manifest(record, directory)?;
        if let Some(manifest) = &manifest {
            links.push((directory.as_path(), names(manifest, "taken")));
        }
        if index + 1 == chain.len() {
            newest_manifest = manifest;
        }
    }
    let manifest = newest_manifest.ok_or("the newest bundle's notes manifest is missing")?;
    let mut apart = names(&manifest, "skipped");
    apart.extend(names(&manifest, "changed_during_read"));
    let mut out = Restored {
        apart,
        ..Restored::default()
    };
    let mut used = BTreeSet::new();
    for entry in manifest["files"].as_array().into_iter().flatten() {
        let named = entry["path"].as_str().unwrap_or_default();
        let path = relative(named)
            .ok_or_else(|| format!("the newest notes manifest names the path {named:?}"))?;
        let size = entry["size"].as_u64().unwrap_or(0);
        let sha256 = entry["sha256"].as_str().unwrap_or_default().to_string();
        let (link, (directory, _)) = links
            .iter()
            .enumerate()
            .rev()
            .find(|(_, (_, taken))| taken.contains(named))
            .ok_or_else(|| format!("sync/{named}: no bundle of the chain holds it"))?;
        used.insert(link);
        let target = restored.join(&path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
        }
        let source = directory.join("sync/files").join(&path);
        let (got_size, got_sha) = copy_hashed(&source, &target)
            .map_err(|_| format!("sync/{named}: the bundle that took it holds no copy"))?;
        if got_size != size || got_sha != sha256 {
            return Err(format!(
                "sync/{named}: restored at {got_size} bytes, sha256 {got_sha}; the newest \
                 manifest says {size} bytes, sha256 {sha256}"
            ));
        }
        out.bytes += size;
        out.files.push(File_ {
            path: named.to_string(),
            size,
            sha256,
        });
    }
    out.links = used.len();
    Ok(out)
}

/// The notes folder restored and live, compared.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Compared {
    pub(super) same: u64,
    pub(super) changed: u64,
    pub(super) removed: u64,
    pub(super) made: u64,
    pub(super) apart: u64,
    pub(super) mismatches: Vec<String>,
}

impl Compared {
    pub(super) fn line(&self, live: &Path) -> String {
        format!(
            "restore-drill: the notes folder against live {}: {} the same, {} changed since, {} \
             removed since, {} made since, {} skipped or changed while bundled, {} mismatch(es)",
            live.display(),
            self.same,
            self.changed,
            self.removed,
            self.made,
            self.apart,
            self.mismatches.len()
        )
    }
}

/// A file's mtime in ms.
fn mtime_ms(meta: &fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    meta.mtime()
        .saturating_mul(1000)
        .saturating_add(meta.mtime_nsec() / 1_000_000)
}

/// What the live folder holds: its regular files (relative path, mtime),
/// read without following anything, and each directory that cannot be read
/// (relative, why). An entry gone while it is read is left out.
type Listed = (BTreeMap<String, i64>, Vec<(String, String)>);

/// The live folder's regular files, or why the folder itself cannot be read.
fn list_live(live: &Path) -> Result<Listed, String> {
    let mut files = BTreeMap::new();
    let mut unreadable = Vec::new();
    let mut pending = vec![(String::new(), live.to_path_buf())];
    while let Some((relative, directory)) = pending.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if relative.is_empty() => return Err(error.to_string()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                unreadable.push((relative, error.to_string()));
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    unreadable.push((relative.clone(), error.to_string()));
                    break;
                }
            };
            let name = entry.file_name().to_string_lossy().to_string();
            let inner = if relative.is_empty() {
                name
            } else {
                format!("{relative}/{name}")
            };
            let path = entry.path();
            let meta = match fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    unreadable.push((inner, error.to_string()));
                    continue;
                }
            };
            if meta.is_dir() {
                pending.push((inner, path));
            } else if meta.is_file() {
                files.insert(inner, mtime_ms(&meta));
            }
        }
    }
    Ok((files, unreadable))
}

/// What reading a live file found.
enum Live {
    /// Its sha256 and mtime, read through the one open file.
    Read(String, i64),
    /// No longer a regular file there (not found, now a symlink or other).
    Gone,
    /// It cannot be read.
    Unreadable(String),
}

/// A live file read without following a symlink and without waiting.
fn read_live(path: &Path) -> Live {
    use rustix::fs::{Mode, OFlags};
    use rustix::io::Errno;
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd = match rustix::fs::open(path, flags, Mode::empty()) {
        Ok(fd) => fd,
        Err(Errno::NOENT | Errno::LOOP) => return Live::Gone,
        Err(error) => return Live::Unreadable(std::io::Error::from(error).to_string()),
    };
    let mut file = File::from(fd);
    let meta = match file.metadata() {
        Ok(meta) => meta,
        Err(error) => return Live::Unreadable(error.to_string()),
    };
    if !meta.is_file() {
        return Live::Gone;
    }
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; CHUNK];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => hasher.update(&buffer[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Live::Gone,
            Err(error) => return Live::Unreadable(error.to_string()),
        }
    }
    Live::Read(hex(&hasher.finalize()), mtime_ms(&meta))
}

/// An epoch in ms as UTC text.
fn when(ms: i64) -> String {
    format!("{} UTC", crate::alerts::time_text(ms))
}

/// The live folder `live` compared with the restored notes folder, read
/// only, against the newest bundle's start (`start_ms`).
pub(super) fn compare(restored: &Restored, live: &Path, start_ms: i64) -> Compared {
    let mut out = Compared::default();
    let (listed, unreadable) = match list_live(live) {
        Ok(listed) => listed,
        Err(why) => {
            out.mismatches.push(format!(
                "sync/: the notes folder {} cannot be read: {why}",
                live.display()
            ));
            return out;
        }
    };
    for (directory, why) in unreadable {
        out.mismatches
            .push(format!("sync/{directory}: this folder cannot be read: {why}"));
    }
    let manifest: BTreeSet<&str> = restored.files.iter().map(|file| file.path.as_str()).collect();
    for file in &restored.files {
        if !listed.contains_key(&file.path) {
            out.removed += 1;
            continue;
        }
        match read_live(&live.join(&file.path)) {
            Live::Gone => out.removed += 1,
            Live::Unreadable(why) => out
                .mismatches
                .push(format!("sync/{}: live cannot be read: {why}", file.path)),
            Live::Read(sha256, _) if sha256 == file.sha256 => out.same += 1,
            Live::Read(_, mtime) if mtime >= start_ms => out.changed += 1,
            Live::Read(_, mtime) => out.mismatches.push(format!(
                "sync/{}: live has other bytes than the backup, last written {}, before the \
                 bundle began ({}): the backup holds older bytes than the folder had then",
                file.path,
                when(mtime),
                when(start_ms)
            )),
        }
    }
    for (path, mtime) in &listed {
        if manifest.contains(path.as_str()) {
            continue;
        }
        if restored.apart.contains(path) {
            out.apart += 1;
        } else if *mtime >= start_ms {
            out.made += 1;
        } else {
            out.mismatches.push(format!(
                "sync/{path}: live, last written {}, before the bundle began ({}), is not in \
                 the backup",
                when(*mtime),
                when(start_ms)
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests;
