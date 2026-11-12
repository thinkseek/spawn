//! `spawn backup store put|get|list|verify`: the off-host copy of the
//! transcript archive and the native sessions, upload-only (sync/plan-r2.md,
//! design D as the lead and rev-g settled it, 2026-11-13).
//!
//! Two areas, nothing more (the owner's cut of 11-05: no general store):
//! - `transcripts`: the workspace's transcript archive (`<root>/.transcripts`);
//! - `sessions`: the account's native sessions (`~/.claude/projects`,
//!   `~/.claude/history.jsonl`, `~/.claude/file-history`).
//!
//! A file's objects are its segments: one per run in which it changed, the
//! bytes from the last manifest's size to its size now (the archive is
//! written by appending), or, when its first bytes no longer hash to the
//! last manifest's sha256 (a rewritten memory file, a truncated or replaced
//! one), the whole file again from offset 0. A segment is named by the
//! HMAC-SHA256 of its bytes under a key derived from the backup key (a name
//! never confirms a guessed plaintext), compressed and encrypted with the
//! backup key (gpg), and PUT with `If-None-Match: *`, so a key that exists
//! is never written again; the same bytes are the same name, found by HEAD
//! and not sent. Each PUT is read back (HEAD: its length, its ETag the MD5
//! sent, its sha256) before it counts. Nothing here deletes or overwrites an
//! object.
//!
//! A run's manifest lists every file of the area with its size, ns mtime,
//! whole-file sha256 and segments (offset, length, name), under a key unique
//! to the run (`<area>/manifests/<host>/<account>/<stamp>-<random>.json.gpg`),
//! read back the same way. A file whose size and mtime are the last
//! manifest's is not read. The last manifest is the newest one in the bucket
//! (never a local guess).
//!
//! It ships switched off: until `SPAWN_STORE_ENABLED=1`, `put` writes only
//! under a `locktest/` prefix (`SPAWN_BACKUP_PREFIX`), the throwaway prefix
//! of the existing bucket. Originals are never moved or removed.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{Lock, Scratch, Settings, Stamp, hex, random_hex};
use crate::Flags;
use crate::config::env_value;

const USAGE: &str = "usage: spawn backup store put --area transcripts|sessions [--from <dir>] [--root <dir>]\n       \
spawn backup store get --area <area> --path <file> --into <dir> [--root <dir>]\n       \
spawn backup store list|verify --area <area> [--root <dir>]";

/// The largest segment: a file that grew by more is cut into several.
const SEGMENT_MAX: u64 = 64 * 1024 * 1024;

/// One file of a manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Entry {
    pub(crate) path: String,
    pub(crate) size: u64,
    pub(crate) mtime_ns: i128,
    pub(crate) sha256: String,
    pub(crate) segments: Vec<Segment>,
}

/// A run of a file's bytes, stored as one object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Segment {
    pub(crate) offset: u64,
    pub(crate) length: u64,
    pub(crate) name: String,
}

/// A run's manifest: every file of the area as the run left it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub(crate) version: u32,
    pub(crate) area: String,
    pub(crate) host: String,
    pub(crate) account: String,
    pub(crate) made_ms: i64,
    pub(crate) files: Vec<Entry>,
}

/// What a PUT found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Put {
    Written,
    Existed,
}

/// An object as HEAD reads it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Head {
    pub(crate) length: u64,
    pub(crate) etag: String,
    pub(crate) sha256: String,
}

/// The bucket, as this layer uses it: never a delete, never an overwrite.
pub(crate) trait Bucket {
    /// Write `file` at `key` unless the key exists (`If-None-Match: *`),
    /// with `sha256` (the plaintext's) as its metadata.
    fn put_new(&self, key: &str, file: &Path, sha256: &str) -> Result<Put, String>;
    fn head(&self, key: &str) -> Result<Option<Head>, String>;
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String>;
    /// Every key under `prefix`, with its size.
    fn list(&self, prefix: &str) -> Result<Vec<(String, u64)>, String>;
}

/// The bucket of the backups (`spawn backup`'s settings and client).
impl Bucket for Settings {
    fn put_new(&self, key: &str, file: &Path, sha256: &str) -> Result<Put, String> {
        super::put_new_object(self, key, file, sha256)
    }
    fn head(&self, key: &str) -> Result<Option<Head>, String> {
        super::head_object(self, key)
    }
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        super::get_object_bytes(self, key).map(|found| found.map(|(bytes, _)| bytes))
    }
    fn list(&self, prefix: &str) -> Result<Vec<(String, u64)>, String> {
        super::list_prefix(self, prefix)
    }
}

/// How a file is sent this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Plan {
    /// Its size and mtime are the last manifest's: not read.
    Unchanged,
    /// It grew from `from` and its first `from` bytes are the last
    /// manifest's: only the bytes from `from` on.
    Append { from: u64 },
    /// Anything else: the whole file, from 0.
    Whole,
}

/// The plan for a file of `size` bytes and `mtime_ns`, against its entry in
/// the last manifest; `prefix_matches` answers whether its first
/// `last.size` bytes hash to `last.sha256` (asked only when it grew).
pub(crate) fn plan(
    last: Option<&Entry>,
    size: u64,
    mtime_ns: i128,
    prefix_matches: impl FnOnce(&Entry) -> Result<bool, String>,
) -> Result<Plan, String> {
    let Some(last) = last else {
        return Ok(Plan::Whole);
    };
    if size == last.size && mtime_ns == last.mtime_ns {
        return Ok(Plan::Unchanged);
    }
    if size > last.size && prefix_matches(last)? {
        return Ok(Plan::Append { from: last.size });
    }
    Ok(Plan::Whole)
}

/// `[from, size)` as segment ranges of at most `max` bytes.
pub(crate) fn ranges(from: u64, size: u64, max: u64) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let mut at = from;
    while at < size {
        let length = (size - at).min(max.max(1));
        out.push((at, length));
        at += length;
    }
    out
}

/// HMAC-SHA256 (RFC 2104).
pub(crate) fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.map(|value| value ^ byte);
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(data)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

/// The key segment names are made with: from the backup key, never the key
/// itself.
fn name_key(settings: &Settings) -> Result<[u8; 32], String> {
    let secret = fs::read(&settings.key_file)
        .map_err(|error| format!("the backup key {}: {error}", settings.key_file.display()))?;
    Ok(Sha256::new()
        .chain_update(b"spawn backup store: segment names\0")
        .chain_update(&secret)
        .finalize()
        .into())
}

/// Where an area's objects are, under the run's prefix.
#[derive(Debug, Clone)]
pub(crate) struct Keys {
    pub(crate) prefix: String,
    pub(crate) area: String,
    pub(crate) host: String,
    pub(crate) account: String,
}

impl Keys {
    pub(crate) fn segment(&self, name: &str) -> String {
        format!(
            "{}{}/segments/{}/{name}",
            self.prefix,
            self.area,
            &name[..2.min(name.len())]
        )
    }
    pub(crate) fn manifests(&self) -> String {
        format!(
            "{}{}/manifests/{}/{}/",
            self.prefix, self.area, self.host, self.account
        )
    }
    pub(crate) fn manifest(&self, stamp: &str, random: &str) -> String {
        format!("{}{stamp}-{random}.json.gpg", self.manifests())
    }
}

/// Whether a run may write under `prefix`: anywhere once
/// `SPAWN_STORE_ENABLED=1` (the owner's buckets and token exist), else only
/// under `locktest/`.
pub(crate) fn may_write(prefix: &str, enabled: Option<&str>) -> Result<(), String> {
    if enabled == Some("1") || prefix.starts_with("locktest/") {
        return Ok(());
    }
    Err(format!(
        "refused: the store is switched off (SPAWN_STORE_ENABLED is not 1), so it writes only \
under a locktest/ prefix (SPAWN_BACKUP_PREFIX), not under {:?}",
        if prefix.is_empty() {
            "the bucket's root"
        } else {
            prefix
        }
    ))
}

/// A file of an area: its path in the manifest and on disk.
#[derive(Debug, Clone)]
struct Source {
    path: String,
    file: PathBuf,
    size: u64,
    mtime_ns: i128,
}

fn mtime_ns(metadata: &fs::Metadata) -> i128 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| {
            i128::try_from(since.as_nanos()).unwrap_or(i128::MAX)
        })
}

/// Every regular file under `dir` (a symlink is never followed: it is said
/// and skipped), its path relative to `base`.
fn walk(
    base: &Path,
    dir: &Path,
    out: &mut Vec<Source>,
    said: &mut Vec<String>,
) -> Result<(), String> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("{}: {error}", dir.display())),
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        let metadata =
            fs::symlink_metadata(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        let kind = metadata.file_type();
        if kind.is_symlink() {
            said.push(format!("skipped {} (a symlink)", path.display()));
        } else if kind.is_dir() {
            walk(base, &path, out, said)?;
        } else if kind.is_file() {
            let relative = path
                .strip_prefix(base)
                .map_err(|_| format!("{} is not under {}", path.display(), base.display()))?;
            let Some(text) = relative.to_str() else {
                said.push(format!("skipped {} (not UTF-8)", path.display()));
                continue;
            };
            out.push(Source {
                path: text.to_string(),
                file: path.clone(),
                size: metadata.len(),
                mtime_ns: mtime_ns(&metadata),
            });
        }
    }
    Ok(())
}

/// An area's files now: `transcripts` under `from` (the archive), or
/// `sessions` (`projects`, `history.jsonl` and `file-history` under `from`,
/// the account's `.claude`, which must not be a symlink).
fn sources(area: &str, from: &Path, said: &mut Vec<String>) -> Result<Vec<Source>, String> {
    let metadata =
        fs::symlink_metadata(from).map_err(|error| format!("{}: {error}", from.display()))?;
    if !metadata.file_type().is_dir() {
        return Err(format!(
            "{} is not a directory (a symlink is refused)",
            from.display()
        ));
    }
    let mut out = Vec::new();
    match area {
        "transcripts" => walk(from, from, &mut out, said)?,
        "sessions" => {
            walk(from, &from.join("projects"), &mut out, said)?;
            walk(from, &from.join("file-history"), &mut out, said)?;
            let history = from.join("history.jsonl");
            if let Ok(metadata) = fs::symlink_metadata(&history) {
                if metadata.file_type().is_file() {
                    out.push(Source {
                        path: "history.jsonl".to_string(),
                        file: history,
                        size: metadata.len(),
                        mtime_ns: mtime_ns(&metadata),
                    });
                } else {
                    said.push(format!(
                        "skipped {} (not a regular file)",
                        history.display()
                    ));
                }
            }
        }
        other => return Err(format!("unknown area {other} (transcripts or sessions)")),
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// The sha256 of a file's first `length` bytes.
fn prefix_sha256(file: &mut File, length: u64) -> Result<String, String> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| error.to_string())?;
    let mut hasher = Sha256::new();
    let copied = std::io::copy(&mut file.take(length), &mut HashWriter(&mut hasher))
        .map_err(|error| error.to_string())?;
    if copied != length {
        return Err(format!("read {copied} of {length} bytes"));
    }
    Ok(hex(&hasher.finalize()))
}

struct HashWriter<'a>(&'a mut Sha256);

impl Write for HashWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// What a run sends and how it encrypts: the bucket, the names' key, and
/// how to seal and open an object (gpg with the backup key; a test's own).
pub(crate) struct Store<'a, B: Bucket> {
    pub(crate) bucket: &'a B,
    pub(crate) keys: Keys,
    pub(crate) name_key: [u8; 32],
    pub(crate) seal: &'a dyn Fn(&Path, &Path) -> Result<(), String>,
    pub(crate) open: &'a dyn Fn(&Path, &Path) -> Result<(), String>,
    pub(crate) scratch: &'a Path,
}

/// What `put` did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PutCounts {
    pub(crate) files: u64,
    pub(crate) unchanged: u64,
    pub(crate) appended: u64,
    pub(crate) whole: u64,
    pub(crate) segments_written: u64,
    pub(crate) segments_found: u64,
    pub(crate) bytes_written: u64,
    pub(crate) manifest: String,
}

impl<B: Bucket> Store<'_, B> {
    /// The newest manifest of this area, host and account, and its key.
    pub(crate) fn newest(&self) -> Result<Option<(String, Manifest)>, String> {
        let listed = self.bucket.list(&self.keys.manifests())?;
        let Some((key, _)) = listed
            .into_iter()
            .filter(|(key, _)| key.ends_with(".json.gpg"))
            .max_by(|a, b| a.0.cmp(&b.0))
        else {
            return Ok(None);
        };
        let bytes = self.open_object(&key)?;
        let manifest: Manifest = serde_json::from_slice(&bytes)
            .map_err(|error| format!("the manifest {key}: {error}"))?;
        Ok(Some((key, manifest)))
    }

    fn open_object(&self, key: &str) -> Result<Vec<u8>, String> {
        let sealed = self
            .bucket
            .get(key)?
            .ok_or_else(|| format!("{key} is not in the bucket"))?;
        let at = self.scratch.join(format!("get-{}", random_hex(6)?));
        let opened = self.scratch.join(format!("open-{}", random_hex(6)?));
        fs::write(&at, &sealed).map_err(|error| format!("{}: {error}", at.display()))?;
        (self.open)(&at, &opened)?;
        let bytes = fs::read(&opened).map_err(|error| format!("{}: {error}", opened.display()));
        let _ = fs::remove_file(&at);
        let _ = fs::remove_file(&opened);
        bytes
    }

    /// Seal `bytes` and write them at `key` unless it exists; read back
    /// either way. True when this run wrote it.
    fn send(&self, key: &str, bytes: &[u8]) -> Result<(bool, u64), String> {
        let sha256 = hex(&Sha256::digest(bytes));
        if let Some(head) = self.bucket.head(key)? {
            if head.sha256 != sha256 {
                return Err(format!(
                    "{key} exists with another sha256 ({})",
                    head.sha256
                ));
            }
            return Ok((false, 0));
        }
        let plain = self.scratch.join(format!("plain-{}", random_hex(6)?));
        let sealed = self.scratch.join(format!("sealed-{}", random_hex(6)?));
        fs::write(&plain, bytes).map_err(|error| format!("{}: {error}", plain.display()))?;
        let result = (|| {
            (self.seal)(&plain, &sealed)?;
            let length = fs::metadata(&sealed)
                .map_err(|error| format!("{}: {error}", sealed.display()))?
                .len();
            let md5 = super::md5_file(&sealed)?;
            let wrote = self.bucket.put_new(key, &sealed, &sha256)? == Put::Written;
            // Read back before it counts: what is there is what was sent.
            let head = self
                .bucket
                .head(key)?
                .ok_or_else(|| format!("{key}: not there after its PUT"))?;
            if head.sha256 != sha256 {
                return Err(format!(
                    "{key}: read back with sha256 {}, sent {sha256}",
                    head.sha256
                ));
            }
            if wrote && (head.length != length || head.etag != md5) {
                return Err(format!(
                    "{key}: read back {} bytes, ETag {:?}; sent {length} bytes, MD5 {md5:?}",
                    head.length, head.etag
                ));
            }
            Ok((wrote, if wrote { length } else { 0 }))
        })();
        let _ = fs::remove_file(&plain);
        let _ = fs::remove_file(&sealed);
        result
    }

    /// One `put` run of the area's files `sources`.
    fn put(&self, sources: &[Source], made_ms: i64) -> Result<PutCounts, String> {
        let last = self.newest()?.map(|(_, manifest)| manifest);
        let by_path: BTreeMap<&str, &Entry> = last
            .iter()
            .flat_map(|manifest| &manifest.files)
            .map(|entry| (entry.path.as_str(), entry))
            .collect();
        let mut known: BTreeSet<String> = by_path
            .values()
            .flat_map(|entry| entry.segments.iter().map(|segment| segment.name.clone()))
            .collect();
        let mut counts = PutCounts::default();
        let mut files = Vec::new();
        for source in sources {
            counts.files += 1;
            let previous = by_path.get(source.path.as_str()).copied();
            let mut file = File::open(&source.file)
                .map_err(|error| format!("{}: {error}", source.file.display()))?;
            let plan = plan(previous, source.size, source.mtime_ns, |last| {
                prefix_sha256(&mut file, last.size).map(|hash| hash == last.sha256)
            })?;
            let (from, mut segments) = match (plan, previous) {
                (Plan::Unchanged, Some(entry)) => {
                    counts.unchanged += 1;
                    files.push(entry.clone());
                    continue;
                }
                (Plan::Append { from }, Some(entry)) => {
                    counts.appended += 1;
                    (from, entry.segments.clone())
                }
                _ => {
                    counts.whole += 1;
                    (0, Vec::new())
                }
            };
            // The whole file's sha256 now, over the bytes this run read
            // (its size when listed: a file still growing is taken up to
            // there, and the next run sees the rest).
            let mut whole = Sha256::new();
            file.seek(SeekFrom::Start(0))
                .map_err(|error| error.to_string())?;
            std::io::copy(&mut (&mut file).take(from), &mut HashWriter(&mut whole))
                .map_err(|error| format!("{}: {error}", source.file.display()))?;
            for (offset, length) in ranges(from, source.size, SEGMENT_MAX) {
                let mut bytes = Vec::with_capacity(usize::try_from(length).unwrap_or(0));
                (&mut file)
                    .take(length)
                    .read_to_end(&mut bytes)
                    .map_err(|error| format!("{}: {error}", source.file.display()))?;
                if bytes.len() as u64 != length {
                    return Err(format!(
                        "{}: shorter than listed ({} of {length} bytes at {offset}): changed while read",
                        source.file.display(),
                        bytes.len()
                    ));
                }
                whole.update(&bytes);
                let name = hex(&hmac_sha256(&self.name_key, &bytes));
                if known.insert(name.clone()) {
                    let (wrote, sent) = self.send(&self.keys.segment(&name), &bytes)?;
                    if wrote {
                        counts.segments_written += 1;
                        counts.bytes_written += sent;
                    } else {
                        counts.segments_found += 1;
                    }
                }
                segments.push(Segment {
                    offset,
                    length,
                    name,
                });
            }
            files.push(Entry {
                path: source.path.clone(),
                size: source.size,
                mtime_ns: source.mtime_ns,
                sha256: hex(&whole.finalize()),
                segments,
            });
        }
        let manifest = Manifest {
            version: 1,
            area: self.keys.area.clone(),
            host: self.keys.host.clone(),
            account: self.keys.account.clone(),
            made_ms,
            files,
        };
        let text = serde_json::to_vec(&manifest).map_err(|error| error.to_string())?;
        let key = self
            .keys
            .manifest(&Stamp::at(made_ms).text(), &random_hex(4)?);
        let (wrote, sent) = self.send(&key, &text)?;
        if !wrote {
            return Err(format!("{key}: a manifest key that already existed"));
        }
        counts.bytes_written += sent;
        counts.manifest = key;
        Ok(counts)
    }

    /// A file of the newest manifest, rebuilt from its segments, each
    /// checked by its name, the whole by its sha256.
    pub(crate) fn rebuild(&self, entry: &Entry) -> Result<Vec<u8>, String> {
        let mut bytes = Vec::with_capacity(usize::try_from(entry.size).unwrap_or(0));
        for segment in &entry.segments {
            if segment.offset != bytes.len() as u64 {
                return Err(format!(
                    "{}: a segment at {} after {} bytes",
                    entry.path,
                    segment.offset,
                    bytes.len()
                ));
            }
            let part = self.open_object(&self.keys.segment(&segment.name))?;
            if part.len() as u64 != segment.length
                || hex(&hmac_sha256(&self.name_key, &part)) != segment.name
            {
                return Err(format!(
                    "{}: the segment {} is not what was sent",
                    entry.path, segment.name
                ));
            }
            bytes.extend_from_slice(&part);
        }
        if bytes.len() as u64 != entry.size || hex(&Sha256::digest(&bytes)) != entry.sha256 {
            return Err(format!(
                "{}: rebuilt, it is not the file the manifest names",
                entry.path
            ));
        }
        Ok(bytes)
    }

    /// Every segment the newest manifest names is in the bucket; how many
    /// are, how many are missing, and how many of the area's objects it
    /// does not name (older versions' segments, a failed run's).
    pub(crate) fn check(&self, manifest: &Manifest) -> Result<(u64, u64, u64), String> {
        let named: BTreeSet<String> = manifest
            .files
            .iter()
            .flat_map(|entry| {
                entry
                    .segments
                    .iter()
                    .map(|segment| self.keys.segment(&segment.name))
            })
            .collect();
        let held: BTreeSet<String> = self
            .bucket
            .list(&format!("{}{}/segments/", self.keys.prefix, self.keys.area))?
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        let present = named.intersection(&held).count() as u64;
        let missing = named.difference(&held).count() as u64;
        let unnamed = held.difference(&named).count() as u64;
        Ok((present, missing, unnamed))
    }
}

/// The entry point: `spawn backup store <verb> ...`.
pub(crate) fn command(arguments: &[String]) -> Result<i32, String> {
    let Some(verb) = arguments.first() else {
        return Err(USAGE.to_string());
    };
    let flags = Flags::parse(&arguments[1..]).map_err(|error| format!("{error}\n{USAGE}"))?;
    let area = flags
        .get("area")
        .ok_or_else(|| format!("--area is required\n{USAGE}"))?;
    if area != "transcripts" && area != "sessions" {
        return Err(format!(
            "unknown area {area} (transcripts or sessions)\n{USAGE}"
        ));
    }
    let settings = Settings::from_env(&flags)?;
    let scratch = Scratch::new("store")?;
    let keys = Keys {
        prefix: settings.prefix.clone(),
        area: area.clone(),
        host: host_name(),
        account: env_value("USER").unwrap_or_else(|| "unknown".to_string()),
    };
    let seal = |input: &Path, out: &Path| super::seal(&settings, input, out);
    let open = |input: &Path, out: &Path| super::decrypt(&settings, input, out);
    let store = Store {
        bucket: &settings,
        keys,
        name_key: name_key(&settings)?,
        seal: &seal,
        open: &open,
        scratch: &scratch.path,
    };
    match verb.as_str() {
        "put" => put_verb(&store, &settings, &flags, &area),
        "get" => get_verb(&store, &flags, &area),
        "list" => {
            let listed = settings.list(&store.keys.manifests())?;
            println!("store list {area}: {} manifest(s)", listed.len());
            if let Some((key, manifest)) = store.newest()? {
                println!("newest {key}");
                for entry in &manifest.files {
                    println!(
                        "{} {} {} {}",
                        entry.path,
                        entry.size,
                        entry.sha256,
                        entry.segments.len()
                    );
                }
            }
            Ok(0)
        }
        "verify" => {
            let (key, manifest) = store
                .newest()?
                .ok_or("no manifest in the bucket for this area, host and account")?;
            let (present, missing, unnamed) = store.check(&manifest)?;
            let mut sample: Vec<&Entry> = manifest.files.iter().take(2).collect();
            if let Some(largest) = manifest.files.iter().max_by_key(|entry| entry.size) {
                sample.push(largest);
            }
            let mut rebuilt = 0;
            for entry in &sample {
                store.rebuild(entry)?;
                rebuilt += 1;
            }
            println!(
                "store verify {area}: {key}: {} file(s), {present} segment(s) present, {missing} missing, {unnamed} not named by it; {rebuilt} file(s) rebuilt and matched",
                manifest.files.len()
            );
            Ok(i32::from(missing > 0))
        }
        other => Err(format!("unknown store command {other}\n{USAGE}")),
    }
}

/// `put`: one run over the area's files, under the backup lock.
fn put_verb(
    store: &Store<'_, Settings>,
    settings: &Settings,
    flags: &Flags,
    area: &str,
) -> Result<i32, String> {
    may_write(
        &settings.prefix,
        env_value("SPAWN_STORE_ENABLED").as_deref(),
    )?;
    let _lock = Lock::acquire(&settings.root)?;
    let from = match flags.get("from") {
        Some(from) => PathBuf::from(from),
        None if area == "transcripts" => settings.root.join(spawn_host::TRANSCRIPT_DIRECTORY),
        None => super::home().join(".claude"),
    };
    let mut said = Vec::new();
    let sources = sources(area, &from, &mut said)?;
    for line in &said {
        println!("store put {area}: {line}");
    }
    let counts = store.put(&sources, now_ms())?;
    println!(
        "store put {area}: {} file(s): {} unchanged, {} grown, {} whole; {} segment(s) written ({} bytes), {} already there; manifest {}",
        counts.files,
        counts.unchanged,
        counts.appended,
        counts.whole,
        counts.segments_written,
        counts.bytes_written,
        counts.segments_found,
        counts.manifest
    );
    Ok(0)
}

/// `get`: one file of the newest manifest, rebuilt into `--into`.
fn get_verb(store: &Store<'_, Settings>, flags: &Flags, area: &str) -> Result<i32, String> {
    let path = flags
        .get("path")
        .ok_or_else(|| format!("--path is required\n{USAGE}"))?;
    let into = PathBuf::from(
        flags
            .get("into")
            .ok_or_else(|| format!("--into is required\n{USAGE}"))?,
    );
    let (_, manifest) = store
        .newest()?
        .ok_or("no manifest in the bucket for this area, host and account")?;
    let entry = manifest
        .files
        .iter()
        .find(|entry| entry.path == path)
        .ok_or_else(|| format!("{path} is not in the newest manifest"))?;
    let bytes = store.rebuild(entry)?;
    let target = into.join(&entry.path);
    if !into.is_dir()
        || target.exists()
        || entry
            .path
            .split('/')
            .any(|part| part == ".." || part.is_empty())
    {
        return Err(format!(
            "refused: {} (the directory must exist and the file must not)",
            target.display()
        ));
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    fs::write(&target, &bytes).map_err(|error| format!("{}: {error}", target.display()))?;
    println!(
        "store get {area}: {} ({} bytes, sha256 {})",
        target.display(),
        entry.size,
        entry.sha256
    );
    Ok(0)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

fn host_name() -> String {
    fs::read_to_string("/etc/hostname")
        .ok()
        .map(|name| name.trim().to_string())
        .filter(|name| {
            !name.is_empty()
                && name
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '.')
        })
        .unwrap_or_else(|| "host".to_string())
}

#[cfg(test)]
mod tests;
