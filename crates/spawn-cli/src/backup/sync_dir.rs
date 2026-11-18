//! The fleet's notes folder in the daily bundle (objective 216):
//! `SPAWN_BACKUP_SYNC_DIR` (the bundle unit sets `/var/tmp/spawn/sync`).
//!
//! The folder is walked read only: regular files are taken, a symbolic link
//! or a special file is listed in `skipped`, never followed, and nothing is
//! ever written to or cleaned in it. Each file is hashed in 64 KiB reads
//! (never held whole), and a file the run takes is copied, hashed again and
//! statted again; a file that changed while it was read is read once more,
//! then listed in `changed_during_read` and left out of the manifest, never
//! mixed.
//!
//! Inside the bundle: `sync/files` (a full: every file; an increment: the
//! files whose size, mtime or sha256 differ from the base bundle's
//! manifest, and the new ones) and `sync/manifest.json` (every file's path,
//! size, mtime_ms, mode and sha256; `taken`, `removed`, `skipped`,
//! `changed_during_read`). The base manifest is a local copy
//! ([`LOCAL_MANIFEST`] in the workspace root) written after each stored
//! bundle that holds the part, trusted only when its sha256 is the base
//! record's `sync.manifest_sha256` and that record stored its bundle;
//! otherwise the run takes every file and says why (`base_manifest:
//! "unusable: <why>"`).
//!
//! Before the tar, the staged part goes through the value scan
//! (`SPAWN_BACKUP_SYNC_SCAN`, default [`DEFAULT_SCAN`]), run as `python3 -I
//! <tool> <staged dir>`, its stderr dropped and its output read for places
//! only ([`judge_scan`]). A hit, a place it could not search (other than
//! one whose bytes it searched raw), a failed, killed or missing scan, or a
//! file or directory the run cannot read drops the part, and only it: the
//! rest of the bundle is stored as before. The record carries counts and a
//! hash, never the file list, a value or the path of a hit.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The local copy of the newest stored bundle's notes manifest, in the
/// workspace root: an increment's base, when the base record vouches for
/// it ([`base_manifest`]).
pub(super) const LOCAL_MANIFEST: &str = ".backup-sync-manifest.json";

/// The scan the fleet installs (`deploy/redact`), loading secret-values.py.
pub(super) const DEFAULT_SCAN: &str = "/usr/local/lib/spawn/redact/secret-scan-files.py";

/// Why a place was not seen into whose bytes the scan searched raw all the
/// same (secret-scan-files.py's own words): a part with only these places
/// unseen is kept, and the record names them.
const SEARCHED_RAW: [&str; 3] = ["binary (NUL bytes)", "not UTF-8 text", "high entropy"];

/// One read of a file: 64 KiB, so no file is ever held whole.
const CHUNK: usize = 64 * 1024;

/// One file of the folder as a run read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Entry {
    pub(super) path: String,
    pub(super) size: u64,
    pub(super) mtime_ms: i64,
    pub(super) mode: u32,
    pub(super) sha256: String,
}

impl Entry {
    fn json(&self) -> Value {
        json!({
            "path": self.path, "size": self.size, "mtime_ms": self.mtime_ms,
            "mode": self.mode, "sha256": self.sha256,
        })
    }

    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            path: value["path"].as_str()?.to_string(),
            size: value["size"].as_u64()?,
            mtime_ms: value["mtime_ms"].as_i64()?,
            mode: u32::try_from(value["mode"].as_u64()?).ok()?,
            sha256: value["sha256"].as_str()?.to_string(),
        })
    }
}

/// What the notes folder gave the bundle: its record part (`sync`), its
/// entry in the bundle's manifest, and, when the part is held, the bytes of
/// its manifest, written as the local copy once the bundle is stored.
pub(super) struct Part {
    pub(super) record: Value,
    pub(super) manifest: Value,
    pub(super) local: Option<Vec<u8>>,
}

/// The settings of one run: the folder, the staging folder the bundle tars
/// (the part goes in its `sync/`), whether the bundle is a full one, the
/// base bundle's record, the workspace root (the local manifest) and the
/// scan tool.
pub(super) struct Run<'a> {
    pub(super) dir: &'a Path,
    pub(super) stage: &'a Path,
    pub(super) full: bool,
    pub(super) base: Option<&'a Value>,
    pub(super) root: &'a Path,
    pub(super) scan: &'a str,
}

/// Take the notes folder into `run.stage`'s `sync/`, or drop the part with
/// the reason (the staged part removed): never an error of the bundle.
pub(super) fn take(run: &Run) -> Part {
    let dir = run.dir.display().to_string();
    match take_part(run) {
        Ok(part) => part,
        Err(reason) => {
            let _ = fs::remove_dir_all(run.stage.join("sync"));
            Part {
                record: json!({"dir": dir, "refused": reason}),
                manifest: json!({"refused": reason}),
                local: None,
            }
        }
    }
}

/// Write `bytes` as the local copy of the base manifest, atomically.
pub(super) fn write_local(root: &Path, bytes: &[u8]) -> Result<(), String> {
    let path = root.join(LOCAL_MANIFEST);
    let part = root.join(format!("{LOCAL_MANIFEST}.part"));
    fs::write(&part, bytes)
        .and_then(|()| fs::rename(&part, &path))
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// What the folder holds, read only: its regular files (relative path,
/// real path), in path order, and what was skipped (a symbolic link, a
/// special file), relative. A directory the run cannot read is an error
/// naming it.
fn walk(dir: &Path) -> Result<(Vec<(String, PathBuf)>, Vec<String>), String> {
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    let mut pending = vec![(String::new(), dir.to_path_buf())];
    while let Some((relative, path)) = pending.pop() {
        let shown = if relative.is_empty() { "." } else { &relative };
        let listing = fs::read_dir(&path).map_err(|error| format!("cannot read {shown}: {error}"))?;
        let mut names: Vec<_> = Vec::new();
        for entry in listing {
            let entry = entry.map_err(|error| format!("cannot read {shown}: {error}"))?;
            names.push(entry.file_name());
        }
        names.sort();
        for name in names.into_iter().rev() {
            let inner = path.join(&name);
            let name = name.to_string_lossy();
            let inner_relative = if relative.is_empty() {
                name.to_string()
            } else {
                format!("{relative}/{name}")
            };
            let meta = fs::symlink_metadata(&inner)
                .map_err(|error| format!("cannot read {inner_relative}: {error}"))?;
            if meta.is_dir() {
                pending.push((inner_relative, inner));
            } else if meta.is_file() {
                files.push((inner_relative, inner));
            } else {
                skipped.push(inner_relative);
            }
        }
    }
    files.sort();
    skipped.sort();
    Ok((files, skipped))
}

/// What a stat says of a file: its size, mtime (ms) and mode, or `None`
/// when it is no regular file now.
fn stat(path: &Path) -> std::io::Result<Option<(u64, i64, u32)>> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() {
        return Ok(None);
    }
    let mtime_ms = meta
        .mtime()
        .saturating_mul(1000)
        .saturating_add(meta.mtime_nsec() / 1_000_000);
    Ok(Some((meta.len(), mtime_ms, meta.mode() & 0o7777)))
}

/// Read `path` in [`CHUNK`]s: its sha256 and length, each chunk also
/// written to `copy` when given.
fn read_file(path: &Path, mut copy: Option<&mut File>) -> std::io::Result<(String, u64)> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; CHUNK];
    let mut length = 0u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        if let Some(copy) = copy.as_deref_mut() {
            copy.write_all(&buffer[..read])?;
        }
        length += read as u64;
    }
    Ok((super::hex(&hasher.finalize()), length))
}

/// One file read so that its bytes, size, mtime and mode belong together:
/// statted, hashed (and copied to `target`, then hashed again), statted
/// again; a file that changed meanwhile is read once more. `None`: it
/// changed both times (or stopped being a regular file). An error: it
/// cannot be read (named by `relative`).
fn read_stable(
    relative: &str,
    path: &Path,
    target: Option<&Path>,
) -> Result<Option<Entry>, String> {
    let cannot = |error: std::io::Error| format!("cannot read {relative}: {error}");
    for _ in 0..2 {
        let Some(before) = stat(path).map_err(cannot)? else {
            return Ok(None);
        };
        let (sha256, length) = match target {
            Some(target) => {
                let mut copy = File::create(target)
                    .map_err(|error| format!("{}: {error}", target.display()))?;
                let (copied, length) = read_file(path, Some(&mut copy)).map_err(cannot)?;
                let (again, _) = read_file(path, None).map_err(cannot)?;
                if again != copied {
                    continue;
                }
                (copied, length)
            }
            None => read_file(path, None).map_err(cannot)?,
        };
        let after = stat(path).map_err(cannot)?;
        if after == Some(before) && length == before.0 {
            return Ok(Some(Entry {
                path: relative.to_string(),
                size: before.0,
                mtime_ms: before.1,
                mode: before.2,
                sha256,
            }));
        }
    }
    if let Some(target) = target {
        let _ = fs::remove_file(target);
    }
    Ok(None)
}

/// The base bundle's manifest, by path, when it can be trusted: the base
/// record stored its bundle and names a notes manifest whose sha256 is the
/// local copy's ([`LOCAL_MANIFEST`]). Otherwise why not.
pub(super) fn base_manifest(
    root: &Path,
    base: Option<&Value>,
) -> Result<BTreeMap<String, Entry>, String> {
    let base = base.ok_or("no base bundle")?;
    if base["keys"].as_array().is_none_or(Vec::is_empty) {
        return Err("the base bundle was not stored".to_string());
    }
    let want = base["sync"]["manifest_sha256"]
        .as_str()
        .ok_or("the base bundle holds no notes folder")?;
    let path = root.join(LOCAL_MANIFEST);
    let bytes = fs::read(&path).map_err(|error| format!("no local copy of its manifest: {error}"))?;
    if super::hex(&Sha256::digest(&bytes)) != want {
        return Err("the local copy of its manifest is not the stored one".to_string());
    }
    let manifest: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("its manifest does not read: {error}"))?;
    manifest["files"]
        .as_array()
        .ok_or("its manifest names no files")?
        .iter()
        .map(|entry| {
            Entry::from_json(entry)
                .map(|entry| (entry.path.clone(), entry))
                .ok_or_else(|| "its manifest holds an entry it cannot read".to_string())
        })
        .collect()
}

/// Whether an increment takes `now`: new, or its size, mtime or sha256
/// differ from the base's entry.
pub(super) fn changed(now: &Entry, base: Option<&Entry>) -> bool {
    base.is_none_or(|base| {
        base.size != now.size || base.mtime_ms != now.mtime_ms || base.sha256 != now.sha256
    })
}

/// The part, or the reason it is dropped.
fn take_part(run: &Run) -> Result<Part, String> {
    let (files, skipped) = walk(run.dir)?;
    let (base, base_manifest) = if run.full {
        (None, "not used: a full bundle".to_string())
    } else {
        match base_manifest(run.root, run.base) {
            Ok(base) => (Some(base), "used".to_string()),
            Err(why) => (None, format!("unusable: {why}")),
        }
    };
    let sync = run.stage.join("sync");
    let staged_files = sync.join("files");
    fs::create_dir_all(&staged_files)
        .map_err(|error| format!("{}: {error}", staged_files.display()))?;
    let mut entries: Vec<Entry> = Vec::new();
    let mut taken: Vec<String> = Vec::new();
    let mut changed_during_read: Vec<String> = Vec::new();
    let (mut bytes, mut taken_bytes) = (0u64, 0u64);
    for (relative, path) in &files {
        let Some(read) = read_stable(relative, path, None)? else {
            changed_during_read.push(relative.clone());
            continue;
        };
        let held = base.as_ref().and_then(|base| base.get(relative));
        let entry = if base.is_none() || changed(&read, held) {
            let target = staged_files.join(relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("{}: {error}", parent.display()))?;
            }
            let Some(copied) = read_stable(relative, path, Some(&target))? else {
                changed_during_read.push(relative.clone());
                continue;
            };
            taken.push(relative.clone());
            taken_bytes += copied.size;
            copied
        } else {
            read
        };
        bytes += entry.size;
        entries.push(entry);
    }
    let present: BTreeSet<&str> = files.iter().map(|(path, _)| path.as_str()).collect();
    let removed: Vec<String> = base
        .as_ref()
        .map(|base| {
            base.keys()
                .filter(|path| !present.contains(path.as_str()))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let manifest = json!({
        "files": entries.iter().map(Entry::json).collect::<Vec<_>>(),
        "taken": taken,
        "removed": removed,
        "skipped": skipped,
        "changed_during_read": changed_during_read,
    });
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?;
    fs::write(sync.join("manifest.json"), &manifest_bytes)
        .map_err(|error| format!("sync/manifest.json: {error}"))?;
    let manifest_sha256 = super::hex(&Sha256::digest(&manifest_bytes));
    let scan = scan(run.scan, &sync)?;
    let kind = if run.full { "full" } else { "incremental" };
    Ok(Part {
        record: json!({
            "dir": run.dir.display().to_string(),
            "kind": kind,
            "base_manifest": base_manifest,
            "files": entries.len(),
            "bytes": bytes,
            "taken": taken.len(),
            "taken_bytes": taken_bytes,
            "removed": removed.len(),
            "skipped": skipped,
            "changed_during_read": changed_during_read,
            "manifest_sha256": manifest_sha256,
            "scan": scan,
        }),
        manifest: json!({
            "kind": kind,
            "files": "sync/files",
            "manifest": "sync/manifest.json",
            "manifest_sha256": manifest_sha256,
        }),
        local: Some(manifest_bytes),
    })
}

/// Run the value scan over the staged part `staged`: `python3 -I <tool>
/// <staged>`, stderr dropped, stdout read for places only. Its record
/// (`{exit, unseen: [{path, why}]}`) when the part is kept ([`judge_scan`]),
/// else why it is dropped.
fn scan(tool: &str, staged: &Path) -> Result<Value, String> {
    if !Path::new(tool).is_file() {
        return Err(format!("the value scan {tool} is missing"));
    }
    let output = Command::new("python3")
        .arg("-I")
        .arg(tool)
        .arg(staged)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map_err(|error| format!("the value scan did not run: {error}"))?;
    let prefix = format!("{}/", staged.display());
    judge_scan(
        output.status.code(),
        &String::from_utf8_lossy(&output.stdout),
        &prefix,
    )
}

/// The scan's answer read (its exit code, `None` when it was killed, and
/// its output; `prefix` the staged folder's path with its slash): exit 0
/// keeps the part; exit 3 keeps it only when every place not seen into was
/// searched raw ([`SEARCHED_RAW`]), naming those places (relative to the
/// staged files); exit 1 (a value or a key block), 2, any other code or a
/// kill drops it. A refusal never names the path of a hit.
pub(super) fn judge_scan(code: Option<i32>, stdout: &str, prefix: &str) -> Result<Value, String> {
    let unseen: Vec<(String, String)> = stdout
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("not seen into (")?;
            let (why, path) = rest.split_once("): ")?;
            let path = path
                .strip_prefix(&format!("{prefix}files/"))
                .or_else(|| path.strip_prefix(prefix))
                .unwrap_or(path);
            Some((path.to_string(), why.to_string()))
        })
        .collect();
    let places = || -> Vec<Value> {
        unseen
            .iter()
            .map(|(path, why)| json!({"path": path, "why": why}))
            .collect()
    };
    match code {
        Some(0) => Ok(json!({"exit": 0, "unseen": []})),
        Some(1) => Err("the value scan found a credential value or a private key block".to_string()),
        Some(3) => {
            let blind: BTreeSet<&str> = unseen
                .iter()
                .map(|(_, why)| why.as_str())
                .filter(|why| !SEARCHED_RAW.contains(why))
                .collect();
            if unseen.is_empty() || !blind.is_empty() {
                let whys = if blind.is_empty() {
                    "none named".to_string()
                } else {
                    blind.into_iter().collect::<Vec<_>>().join(", ")
                };
                return Err(format!(
                    "the value scan could not search {} place(s) ({whys})",
                    unseen.len()
                ));
            }
            Ok(json!({"exit": 3, "unseen": places()}))
        }
        Some(code) => Err(format!("the value scan failed (exit {code})")),
        None => Err("the value scan was killed".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Entry, LOCAL_MANIFEST, Run, base_manifest, changed, judge_scan, take, write_local,
    };
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use std::path::{Path, PathBuf};

    fn scratch(tag: &str) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("spawn-sync-dir-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("scratch");
        base
    }

    /// A scan that answers exit 0 (prints nothing).
    fn clean_scan(base: &Path) -> String {
        let tool = base.join("scan.py");
        std::fs::write(&tool, "import sys\nprint('files: 1')\nsys.exit(0)\n").expect("scan");
        tool.display().to_string()
    }

    fn entry(path: &str, size: u64, mtime_ms: i64, sha256: &str) -> Entry {
        Entry {
            path: path.to_string(),
            size,
            mtime_ms,
            mode: 0o644,
            sha256: sha256.to_string(),
        }
    }

    /// Exit 3 keeps the part only when every place not seen into was
    /// searched raw (binary, not UTF-8, high entropy), and names those
    /// places relative to the staged files; any other unseen reason, an
    /// exit 3 naming none, a hit, a failed scan and a kill drop it, and no
    /// refusal names a hit's path.
    #[test]
    fn the_scan_keeps_the_part_only_when_every_unseen_place_was_searched_raw() {
        let prefix = "/w/.spawn-backup-bundle-1-ab0/stage/sync/";
        let raw = format!(
            "files: 3; not seen into: 3\n\
             not seen into (binary (NUL bytes)): {prefix}files/obj-7/shot.png\n\
             not seen into (not UTF-8 text): {prefix}files/a.bin\n\
             not seen into (high entropy): {prefix}manifest.json\n"
        );
        let kept = judge_scan(Some(3), &raw, prefix).expect("kept");
        assert_eq!(
            kept,
            json!({"exit": 3, "unseen": [
                {"path": "obj-7/shot.png", "why": "binary (NUL bytes)"},
                {"path": "a.bin", "why": "not UTF-8 text"},
                {"path": "manifest.json", "why": "high entropy"},
            ]})
        );
        let blind = format!(
            "{raw}not seen into (damaged zstd (ZstdError)): {prefix}files/old.zst\n"
        );
        let refused = judge_scan(Some(3), &blind, prefix).expect_err("a blind place");
        assert!(refused.contains("damaged zstd"), "{refused}");
        assert!(!refused.contains("old.zst"), "{refused}");
        assert!(judge_scan(Some(3), "files: 1\n", prefix).is_err(), "none named");
        let hit = format!("value in {prefix}files/leak.md\n");
        let refused = judge_scan(Some(1), &hit, prefix).expect_err("a hit");
        assert!(!refused.contains("leak.md"), "{refused}");
        assert!(judge_scan(Some(2), "", prefix).is_err());
        assert!(judge_scan(Some(4), "", prefix).is_err());
        assert!(judge_scan(None, "", prefix).is_err(), "killed");
        assert_eq!(
            judge_scan(Some(0), "files: 2\n", prefix).expect("clean"),
            json!({"exit": 0, "unseen": []})
        );
    }

    /// The local base manifest is trusted only when the base record stored
    /// its bundle and names that manifest's sha256: no base, a base not
    /// stored, one without a notes part, a missing copy and an edited copy
    /// are each unusable, with why.
    #[test]
    fn the_base_manifest_is_trusted_only_when_the_stored_record_names_its_sha256() {
        let root = scratch("base");
        let manifest = json!({"files": [{"path": "a.md", "size": 2, "mtime_ms": 5, "mode": 420,
                                          "sha256": "ab"}]});
        let bytes = serde_json::to_vec_pretty(&manifest).expect("json");
        let sha = super::super::hex(&Sha256::digest(&bytes));
        let record = |keys: Value, sync: Value| json!({"keys": keys, "sync": sync});
        let stored = record(json!(["bundles/x"]), json!({"manifest_sha256": sha}));
        let why = |base: Option<&Value>| base_manifest(&root, base).expect_err("unusable");

        assert_eq!(why(None), "no base bundle");
        assert_eq!(
            why(Some(&record(json!([]), json!({"manifest_sha256": sha})))),
            "the base bundle was not stored"
        );
        assert_eq!(
            why(Some(&record(json!(["k"]), json!({"refused": "x"})))),
            "the base bundle holds no notes folder"
        );
        assert!(why(Some(&stored)).starts_with("no local copy of its manifest"));
        write_local(&root, &bytes).expect("write");
        let base = base_manifest(&root, Some(&stored)).expect("trusted");
        assert_eq!(base.get("a.md"), Some(&entry("a.md", 2, 5, "ab")));
        std::fs::write(
            root.join(LOCAL_MANIFEST),
            serde_json::to_vec(&manifest).expect("json"),
        )
        .expect("edit");
        assert_eq!(
            why(Some(&stored)),
            "the local copy of its manifest is not the stored one"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An increment takes a file that is new or whose size, mtime or sha256
    /// differ from the base's: equal bytes with a new mtime are taken, and
    /// so are other bytes at the same size and mtime; an equal one is not.
    #[test]
    fn an_increment_takes_a_file_by_size_mtime_and_sha256() {
        let base = entry("a.md", 2, 5, "ab");
        assert!(!changed(&base, Some(&base)), "equal");
        assert!(changed(&base, None), "new");
        assert!(changed(&entry("a.md", 2, 6, "ab"), Some(&base)), "equal bytes, new mtime");
        assert!(changed(&entry("a.md", 2, 5, "cd"), Some(&base)), "other bytes, same size and mtime");
        assert!(changed(&entry("a.md", 3, 5, "ab"), Some(&base)), "other size");
    }

    /// A full run takes every regular file, skips a symbolic link (never
    /// followed), writes the manifest whose sha256 the record names, and
    /// leaves the folder as it was; an increment on that base takes the
    /// changed and new files and names the removed one; an unreadable
    /// directory drops the part, named.
    #[test]
    fn the_folder_is_taken_read_only_and_an_increment_takes_what_changed() {
        let base = scratch("take");
        let dir = base.join("notes");
        std::fs::create_dir_all(dir.join("obj")).expect("notes");
        std::fs::write(dir.join("a.md"), "a\n").expect("a");
        std::fs::write(dir.join("obj/b.md"), "b\n").expect("b");
        std::fs::write(dir.join("gone.md"), "g\n").expect("gone");
        std::os::unix::fs::symlink("obj", dir.join("latest")).expect("link");
        let tool = clean_scan(&base);
        let root = base.join("ws");
        std::fs::create_dir_all(&root).expect("root");

        let stage = base.join("stage1");
        let part = take(&Run {
            dir: &dir,
            stage: &stage,
            full: true,
            base: None,
            root: &root,
            scan: &tool,
        });
        assert_eq!(part.record["kind"], "full", "{}", part.record);
        assert_eq!(part.record["files"], 3);
        assert_eq!(part.record["taken"], 3);
        assert_eq!(part.record["skipped"], json!(["latest"]));
        let manifest = std::fs::read(stage.join("sync/manifest.json")).expect("manifest");
        assert_eq!(
            part.record["manifest_sha256"],
            super::super::hex(&Sha256::digest(&manifest))
        );
        assert_eq!(
            std::fs::read(stage.join("sync/files/obj/b.md")).expect("b"),
            b"b\n"
        );
        assert!(!stage.join("sync/files/latest").exists(), "never followed");
        write_local(&root, part.local.as_deref().expect("held")).expect("local");

        std::fs::write(dir.join("a.md"), "a changed\n").expect("a");
        std::fs::write(dir.join("obj/c.md"), "c\n").expect("c");
        std::fs::remove_file(dir.join("gone.md")).expect("rm");
        let record = json!({"keys": ["bundles/x"], "sync": part.record});
        let stage = base.join("stage2");
        let part = take(&Run {
            dir: &dir,
            stage: &stage,
            full: false,
            base: Some(&record),
            root: &root,
            scan: &tool,
        });
        assert_eq!(part.record["base_manifest"], "used", "{}", part.record);
        assert_eq!(part.record["taken"], 2);
        assert_eq!(part.record["removed"], 1);
        let manifest: Value = serde_json::from_slice(
            &std::fs::read(stage.join("sync/manifest.json")).expect("manifest"),
        )
        .expect("json");
        assert_eq!(manifest["taken"], json!(["a.md", "obj/c.md"]));
        assert_eq!(manifest["removed"], json!(["gone.md"]));
        assert!(!stage.join("sync/files/obj/b.md").exists(), "unchanged");

        // An unreadable directory drops the part, named (not as root).
        std::fs::create_dir_all(dir.join("locked")).expect("locked");
        set_mode(&dir.join("locked"), 0o000);
        if std::fs::read_dir(dir.join("locked")).is_err() {
            let stage = base.join("stage3");
            let part = take(&Run {
                dir: &dir,
                stage: &stage,
                full: true,
                base: None,
                root: &root,
                scan: &tool,
            });
            let refused = part.record["refused"].as_str().expect("refused");
            assert!(refused.contains("locked"), "{refused}");
            assert!(!stage.join("sync").exists(), "the staged part is gone");
            assert_eq!(part.manifest["refused"], refused);
        }
        set_mode(&dir.join("locked"), 0o700);
        let _ = std::fs::remove_dir_all(&base);
    }

    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    }
}
