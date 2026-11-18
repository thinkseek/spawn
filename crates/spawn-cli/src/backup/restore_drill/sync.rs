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
    let restored = chain.last().map_or(0, |newest| {
        newest.record["sync"]["bytes"].as_u64().unwrap_or(0)
    });
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
pub(super) struct NoteFile {
    pub(super) path: String,
    pub(super) size: u64,
    pub(super) sha256: String,
}

/// The notes folder restored: its files (the newest manifest's), how many
/// links of the chain gave them, and what the newest manifest says apart.
#[derive(Debug, Default)]
pub(super) struct Restored {
    pub(super) files: Vec<NoteFile>,
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
        out.files.push(NoteFile {
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
        out.mismatches.push(format!(
            "sync/{directory}: this folder cannot be read: {why}"
        ));
    }
    let manifest: BTreeSet<&str> = restored
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
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
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};

    use super::super::super::{BundleRecord, hex};
    use super::{Compared, NoteFile, Restored, bytes, compare, members, no_part, restore};

    /// A scratch folder, removed when the test ends.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let root = crate::storage::tests::scratch(tag);
            fs::create_dir_all(&root).expect("mkdir");
            Self(root)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sha(bytes: &[u8]) -> String {
        hex(&Sha256::digest(bytes))
    }

    fn write(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(path, bytes).expect("write");
    }

    /// A link: its record, and its unpacked tree holding `files` (taken)
    /// and a notes manifest listing `listed` (path, bytes), with `extra`
    /// fields merged in.
    fn link(
        root: &Path,
        event_id: i64,
        taken: &[(&str, &[u8])],
        listed: &[(&str, &[u8])],
        extra: &Value,
    ) -> (BundleRecord, (PathBuf, Value)) {
        let unpacked = root.join(format!("bundle-{event_id}/unpacked"));
        for (path, content) in taken {
            write(&unpacked.join("sync/files").join(path), content);
        }
        let mut manifest = json!({
            "files": listed.iter().map(|(path, content)| json!({
                "path": path, "size": content.len(), "mtime_ms": 1, "mode": 420,
                "sha256": sha(content),
            })).collect::<Vec<_>>(),
            "taken": taken.iter().map(|(path, _)| *path).collect::<Vec<_>>(),
            "removed": [], "skipped": [], "changed_during_read": [],
        });
        if let (Some(manifest), Some(extra)) = (manifest.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                manifest.insert(key.clone(), value.clone());
            }
        }
        let text = serde_json::to_vec_pretty(&manifest).expect("json");
        write(&unpacked.join("sync/manifest.json"), &text);
        let record = BundleRecord {
            event_id,
            at_ms: 0,
            record: json!({"sync": {"dir": "/notes", "manifest_sha256": sha(&text),
                                    "taken_bytes": 10, "bytes": 100},
                           "stored_bytes": 1000}),
        };
        (record, (unpacked, json!({"created_at_ms": 0})))
    }

    fn restored_of(
        links: &[(BundleRecord, (PathBuf, Value))],
        into: &Path,
    ) -> Result<Restored, String> {
        let chain: Vec<&BundleRecord> = links.iter().map(|(record, _)| record).collect();
        let unpacked: Vec<(PathBuf, Value)> =
            links.iter().map(|(_, unpacked)| unpacked.clone()).collect();
        fs::create_dir_all(into).expect("mkdir");
        restore(&chain, &unpacked, into)
    }

    /// Objective 217: each file comes from the newest link that took it;
    /// the newest manifest's `skipped` and `changed_during_read` are said
    /// apart, never an older link's.
    #[test]
    fn each_file_comes_from_the_newest_link_that_took_it() {
        let scratch = Scratch::new("sync_restore");
        let full = link(
            &scratch.0,
            1,
            &[("a.md", b"old a"), ("b/c.md", b"c")],
            &[("a.md", b"old a"), ("b/c.md", b"c")],
            &json!({"skipped": ["old-link"]}),
        );
        let increment = link(
            &scratch.0,
            2,
            &[("a.md", b"new a")],
            &[("a.md", b"new a"), ("b/c.md", b"c")],
            &json!({"skipped": ["latest"], "changed_during_read": ["busy.md"]}),
        );
        let into = scratch.0.join("sync");
        let restored = restored_of(&[full, increment], &into).expect("restored");
        assert_eq!(fs::read(into.join("a.md")).expect("a"), b"new a");
        assert_eq!(fs::read(into.join("b/c.md")).expect("c"), b"c");
        assert_eq!(restored.files.len(), 2);
        assert_eq!(restored.bytes, 6);
        assert_eq!(restored.links, 2);
        assert_eq!(
            restored
                .apart
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["busy.md", "latest"]
        );
        assert!(
            restored
                .line()
                .contains("the notes folder: 2 file(s), 6 bytes")
        );
    }

    /// A path no link took, a manifest whose sha256 is not its record's, a
    /// taken file whose bytes are not the newest manifest's, and a newest
    /// record with no part or a dropped one: the backup cannot be restored.
    #[test]
    fn what_the_chain_cannot_give_back_is_refused() {
        let scratch = Scratch::new("sync_refused");
        let untaken = link(&scratch.0, 1, &[], &[("a.md", b"a")], &json!({}));
        let error = restored_of(&[untaken], &scratch.0.join("r1")).expect_err("no link took it");
        assert!(
            error.contains("sync/a.md: no bundle of the chain holds it"),
            "{error}"
        );

        let (mut record, unpacked) = link(
            &scratch.0,
            2,
            &[("a.md", b"a")],
            &[("a.md", b"a")],
            &json!({}),
        );
        record.record["sync"]["manifest_sha256"] = json!("0".repeat(64));
        let error = restored_of(&[(record, unpacked)], &scratch.0.join("r2"))
            .expect_err("another manifest");
        assert!(error.contains("not its record's"), "{error}");

        let wrong = link(
            &scratch.0,
            3,
            &[("a.md", b"other")],
            &[("a.md", b"a")],
            &json!({}),
        );
        let error = restored_of(&[wrong], &scratch.0.join("r3")).expect_err("other bytes");
        assert!(error.contains("sync/a.md: restored at 5 bytes"), "{error}");

        let (mut none, unpacked) = link(&scratch.0, 4, &[], &[], &json!({}));
        none.record = json!({"stored_bytes": 1});
        assert!(no_part(&none).is_some_and(|why| why.contains("holds no notes folder")));
        let error = restored_of(&[(none, unpacked)], &scratch.0.join("r4")).expect_err("no part");
        assert!(error.contains("holds no notes folder"), "{error}");

        let (mut dropped, unpacked) = link(&scratch.0, 5, &[], &[], &json!({}));
        dropped.record = json!({"sync": {"dir": "/notes", "refused": "a value in a file"}});
        let error =
            restored_of(&[(dropped, unpacked)], &scratch.0.join("r5")).expect_err("dropped");
        assert!(
            error.contains("dropped its notes folder: a value in a file"),
            "{error}"
        );
    }

    fn set_mtime(path: &Path, ms: i64) {
        let time = std::time::UNIX_EPOCH
            + std::time::Duration::from_millis(u64::try_from(ms).expect("positive"));
        fs::File::options()
            .write(true)
            .open(path)
            .expect("open")
            .set_modified(time)
            .expect("set the mtime");
    }

    fn file(path: &str, content: &[u8]) -> NoteFile {
        NoteFile {
            path: path.to_string(),
            size: content.len() as u64,
            sha256: sha(content),
        }
    }

    /// Objective 217: the comparison by bytes, then by the bundle's start.
    #[test]
    fn live_is_compared_by_bytes_then_by_the_bundles_start() {
        let scratch = Scratch::new("sync_compare");
        let live = scratch.0.join("live");
        let start = 1_800_000_000_000_i64;
        let restored = Restored {
            files: vec![
                file("same.md", b"same"),
                file("changed.md", b"before"),
                file("set-back.md", b"bundled"),
                file("between.md", b"bundled"),
                file("removed.md", b"gone"),
            ],
            bytes: 0,
            links: 1,
            apart: ["latest".to_string()].into_iter().collect(),
        };
        write(&live.join("same.md"), b"same");
        write(&live.join("changed.md"), b"after");
        set_mtime(&live.join("changed.md"), start + 5_000);
        write(&live.join("set-back.md"), b"other");
        set_mtime(&live.join("set-back.md"), start - 86_400_000);
        write(&live.join("between.md"), b"other");
        set_mtime(&live.join("between.md"), start - 1_000);
        write(&live.join("new.md"), b"new");
        set_mtime(&live.join("new.md"), start + 1);
        write(&live.join("latest"), b"a file where a link was");
        set_mtime(&live.join("latest"), start - 86_400_000);
        write(&live.join("old.md"), b"old");
        set_mtime(&live.join("old.md"), start - 2 * 86_400_000);
        std::os::unix::fs::symlink("same.md", live.join("link")).expect("symlink");

        let compared = compare(&restored, &live, start);
        assert_eq!(
            (
                compared.same,
                compared.changed,
                compared.removed,
                compared.made,
                compared.apart
            ),
            (1, 1, 1, 1, 1),
            "{compared:?}"
        );
        assert_eq!(compared.mismatches.len(), 3, "{compared:?}");
        assert!(compared.mismatches[0].starts_with("sync/set-back.md: live has other bytes"));
        assert!(compared.mismatches[1].starts_with("sync/between.md: live has other bytes"));
        assert!(compared.mismatches[2].starts_with("sync/old.md: live, last written"));
        assert!(
            compared
                .line(&live)
                .ends_with("1 the same, 1 changed since, 1 removed since, 1 made since, 1 skipped or changed while bundled, 3 mismatch(es)")
        );
        assert!(
            !compared.mismatches.iter().any(|line| line.contains("other")
                && line.contains("bundled")
                && line.contains("same")),
            "no file content in a line"
        );
    }

    /// A folder that cannot be read is one mismatch naming it; a live file
    /// that cannot be read is a mismatch; a file removed while live is read
    /// is removed since.
    #[test]
    fn what_live_cannot_give_is_said() {
        let scratch = Scratch::new("sync_unreadable");
        let restored = Restored {
            files: vec![file("a.md", b"a"), file("b.md", b"b")],
            ..Restored::default()
        };
        let gone = scratch.0.join("no-such-folder");
        let compared = compare(&restored, &gone, 0);
        assert_eq!(compared.mismatches.len(), 1, "{compared:?}");
        assert!(compared.mismatches[0].contains(&format!("the notes folder {}", gone.display())));
        assert_eq!(compared.removed, 0, "nothing else counted");

        let live = scratch.0.join("live");
        write(&live.join("a.md"), b"a");
        write(&live.join("b.md"), b"b");
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(live.join("a.md"), fs::Permissions::from_mode(0o000)).expect("chmod");
        if fs::read(live.join("a.md")).is_err() {
            let compared = compare(&restored, &live, 0);
            assert_eq!(compared.mismatches.len(), 1, "{compared:?}");
            assert!(
                compared.mismatches[0].starts_with("sync/a.md: live cannot be read"),
                "{compared:?}"
            );
            assert_eq!(compared.same, 1);
        }
        fs::set_permissions(live.join("a.md"), fs::Permissions::from_mode(0o644)).expect("chmod");

        // Removed between the listing and the read: its bytes are gone.
        assert!(matches!(
            super::read_live(&live.join("c.md")),
            super::Live::Gone
        ));
        let mut listed_only = Compared::default();
        listed_only.removed += 1;
        assert_eq!(listed_only.removed, 1);
    }

    /// The listing takes the manifest and the notes part alone; the floor
    /// counts each link's ciphertext and taken notes, and the newest
    /// record's folder.
    #[test]
    fn the_listing_takes_the_notes_part_alone_and_the_floor_counts_its_bytes() {
        let listing = "./\n./manifest.json\n./sync/\n./sync/manifest.json\n./sync/files/\n\
                       ./sync/files/a.md\n./sync/files/obj/b.md\n./sync/files/../x\n\
                       ./transcripts/t.jsonl\n./repos/r.bundle\n./sync/other.json\n";
        assert_eq!(
            members(listing).expect("members"),
            [
                "./manifest.json",
                "./sync/manifest.json",
                "./sync/files/a.md",
                "./sync/files/obj/b.md"
            ]
        );
        assert!(
            members("./sync/manifest.json\n").is_err(),
            "no manifest.json"
        );
        let scratch = Scratch::new("sync_floor");
        let (one, _) = link(&scratch.0, 1, &[], &[], &json!({}));
        let (two, _) = link(&scratch.0, 2, &[], &[], &json!({}));
        assert_eq!(bytes(&[&one, &two]), 1000 + 10 + 1000 + 10 + 100);
    }
}
