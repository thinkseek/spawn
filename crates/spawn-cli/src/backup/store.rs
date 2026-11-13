//! `spawn store put|get|list|verify`: the off-host copy of the
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
//! sent, the sealed bytes' sha256) before it counts; no object's metadata
//! holds anything of its plaintext (rev-g F1). A key found there is taken
//! as written (its name is its content's); `verify` finds one lost. A grown
//! file's old bytes are read and hashed again each run it grows (the prefix
//! check). Nothing here deletes or overwrites an object.
//!
//! A run's manifest lists every file of the area with its size, ns mtime,
//! whole-file sha256 and segments (offset, length, name, and the stored
//! object's size and MD5), under
//! `<area>/manifests/<host>/<account>/<generation>-<stamp>-<random>.json.gpg`,
//! read back the same way. The generation is the last manifest's plus one,
//! taken under the backup lock (runs of one area, host and account never
//! overlap), twelve digits, so the newest manifest is the greatest key
//! whatever the clock or the random part says (cc-astra A32). A file whose
//! size and mtime are the last manifest's is not read. The last manifest is
//! the newest one in the bucket (never a local guess). Only the newest
//! manifest can be read back for now: choosing an older one (an earlier
//! version of a file, a file a later run no longer saw) is not there yet;
//! the older manifests and their objects are kept.
//!
//! `verify` checks the stored size and MD5 (the bucket's ETag) of every
//! object the newest manifest names, from one listing (an ETag is the MD5
//! only of an object sent in one PUT: nothing here is ever sent multipart,
//! and `put` refuses an object whose ETag is not a plain MD5), and rebuilds and
//! hashes a sample (cc-astra A30). `get` writes into the directory it is
//! given through descriptors: no parent is followed through a symlink and
//! the file is created exclusively, never over anything (cc-astra A29). It
//! never removes anything (cc-astra A37): the bytes go to
//! `<file>.spawn-store-get.partial` and are named once whole; a write that
//! fails leaves that partial file, names it and exits non-zero, and a
//! restore into the same place refuses on it. An object found at a
//! segment's key by `put` enters a manifest only once its contents are
//! checked against the segment (length and HMAC). A
//! stored path with a `.git` component is put like any other but not
//! restored by `get` (the workspace's path rule refuses it); none of the
//! areas' files has one.
//!
//! Its bucket and credentials are the fleet's store credential,
//! `~/.config/spawn/r2-store.env` (`SPAWN_STORE_ENV_FILE`; mode 600; keys
//! `R2_ENDPOINT`, `R2_ACCESS_KEY_ID`, `R2_SECRET_ACCESS_KEY` and
//! `R2_BUCKETS`, a comma list), read as backup.rs reads r2.env: parsed,
//! never sourced, the keys only to curl on stdin, no value ever printed. The
//! bucket is `SPAWN_STORE_BUCKET`, which must be one `R2_BUCKETS` lists;
//! there is no default. For example (placeholders):
//!
//! ```text
//! R2_ENDPOINT=https://<account>.r2.cloudflarestorage.com
//! R2_BUCKETS=<name>-store,<name>-keep,<name>-work
//! SPAWN_STORE_BUCKET=<name>-work SPAWN_BACKUP_PREFIX=locktest/<run>/ spawn store put ...
//! ```
//!
//! What it may write is an allow-list ([`may_write`]): switched off (until
//! `SPAWN_STORE_ENABLED=1`), only a test object, under a `locktest/` or
//! `probe/` prefix (`SPAWN_BACKUP_PREFIX`), in the work bucket (a name
//! ending in `-work`); switched on, also the store's own keys in the keep
//! bucket; never anything in another bucket.
//! Anything written to the keep bucket is there for good (its lock has no
//! end), so a test is never let near it: the rule takes the work bucket by
//! its name, never by elimination, and any other bucket (the keep bucket,
//! the old store bucket, one of a name it does not know) is refused before
//! anything is read or sent. Originals are never moved or removed.
//!
//! Every listing is under `<prefix><area>/` (an area's manifests, its
//! segments), so an object outside it is never read or counted: the keep
//! bucket's permanent `probe/` lock-check object is not in `verify`'s
//! "not named" count, and that count (older versions', a failed run's
//! objects) never fails `verify` anyway; only a missing or changed object
//! does.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{Key, Lock, Scratch, Settings, Stamp, hex, random_hex};
use crate::Flags;
use crate::config::env_value;

const USAGE: &str = "usage: spawn store put --area transcripts|sessions [--from <dir>] [--root <dir>]\n       \
spawn store plan --area <area> [--from <dir>] [--root <dir>] [--list <file>]\n       \
spawn store get --area <area> --path <file> --into <dir> [--root <dir>]\n       \
spawn store list|verify --area <area> [--root <dir>]\n\
the bucket: SPAWN_STORE_BUCKET, one R2_BUCKETS lists in ~/.config/spawn/r2-store.env (SPAWN_STORE_ENV_FILE)";

/// The manifest's format: 2 since the generation and each segment's stored
/// size and MD5 (cc-astra A30, A32). A version-1 manifest (only ever under
/// `locktest/`) is never read: its key is not of the generation form, so it
/// is never the newest, and one that were is refused here by its version.
const MANIFEST_VERSION: u32 = 2;

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
    /// The stored (sealed) object's size and MD5, as the bucket lists them.
    pub(crate) stored: u64,
    pub(crate) etag: String,
}

/// A run's manifest: every file of the area as the run left it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub(crate) version: u32,
    /// The last manifest's plus one (1 for the first).
    pub(crate) generation: u64,
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

/// A key as the bucket lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Listed {
    pub(crate) key: String,
    pub(crate) size: u64,
    /// The ETag, lower case, unquoted (the MD5 of an object put whole).
    pub(crate) etag: String,
}

/// The bucket, read: HEAD, GET and LIST only. What `plan` is given, so
/// nothing it does can write.
pub(crate) trait Reader {
    fn head(&self, key: &str) -> Result<Option<Head>, String>;
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String>;
    /// Every key under `prefix`, with its size and ETag.
    fn list(&self, prefix: &str) -> Result<Vec<Listed>, String>;
    /// The key an object says sealed it (`x-amz-meta-spawn-seal`, read with
    /// [`super::key_name`]; absent: k1, as every object before objective
    /// 114). A bucket that keeps no headers (a test's) is k1's.
    fn sealed_with(&self, key: &str) -> Result<String, String> {
        let _ = key;
        super::key_name(None)
    }
}

/// The bucket, as this layer writes it: never a delete, never an overwrite.
pub(crate) trait Bucket: Reader {
    /// Write `file` at `key` unless the key exists (`If-None-Match: *`),
    /// with `sha256` (of `file`, the sealed bytes) as its metadata.
    fn put_new(&self, key: &str, file: &Path, sha256: &str) -> Result<Put, String>;
}

/// The bucket of the backups (`spawn backup`'s settings and client).
impl Bucket for Settings {
    fn put_new(&self, key: &str, file: &Path, sha256: &str) -> Result<Put, String> {
        super::put_new_object(self, key, file, sha256)
    }
}

impl Reader for Settings {
    fn head(&self, key: &str) -> Result<Option<Head>, String> {
        super::head_object(self, key)
    }
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        super::get_object_bytes(self, key).map(|found| found.map(|(bytes, _)| bytes))
    }
    fn list(&self, prefix: &str) -> Result<Vec<Listed>, String> {
        super::list_prefix(self, prefix)
    }
    fn sealed_with(&self, key: &str) -> Result<String, String> {
        let response = super::bucket_request(self, &self.r2.url(key), "head\n")?;
        if response.status != 200 {
            return Err(format!("HEAD {key} answered {}", response.status));
        }
        super::object_key(key, &response.headers)
    }
}

/// The backups' bucket with no way to write: `plan`'s. It is a [`Reader`]
/// and not a [`Bucket`], so no PUT is reachable from it.
struct ReadOnly<'a>(&'a Settings);

impl Reader for ReadOnly<'_> {
    fn head(&self, key: &str) -> Result<Option<Head>, String> {
        self.0.head(key)
    }
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        self.0.get(key)
    }
    fn list(&self, prefix: &str) -> Result<Vec<Listed>, String> {
        self.0.list(prefix)
    }
    fn sealed_with(&self, key: &str) -> Result<String, String> {
        self.0.sealed_with(key)
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
    name_key_of(&settings.key_file)
}

/// The names' key of the backup key in `file` (another key's, for a
/// manifest it sealed: objective 116).
fn name_key_of(file: &Path) -> Result<[u8; 32], String> {
    let secret =
        fs::read(file).map_err(|error| format!("the backup key {}: {error}", file.display()))?;
    Ok(Sha256::new()
        // FROZEN from the first live put on: every segment's name is made
        // with it, so a changed label names no object the bucket holds
        // (rev-g N1 on 8c0e3d77; it read "spawn backup store: ..." until
        // then, which only the locktest hand run used).
        .chain_update(b"spawn store: segment names\0")
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
    pub(crate) fn manifest(&self, generation: u64, stamp: &str, random: &str) -> String {
        format!(
            "{}{generation:012}-{stamp}-{random}.json.gpg",
            self.manifests()
        )
    }
}

/// Whether `bucket` is the keep bucket, every object of which is locked
/// for good: by its name's `-keep` ending. The one rule, for `spawn store`
/// ([`may_write`]) and `spawn backup` (which never writes there).
pub(crate) fn locked_forever(bucket: &str) -> bool {
    bucket.ends_with("-keep")
}

/// A bucket as the store sees it, by its name: the keep bucket
/// ([`locked_forever`]), the work bucket (a name ending in `-work`), or
/// another (the old bucket, which is `spawn backup`'s alone, or any name
/// it does not know).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Keep,
    Work,
    Other,
}

pub(crate) fn kind(bucket: &str) -> Kind {
    if locked_forever(bucket) {
        Kind::Keep
    } else if bucket.ends_with("-work") {
        Kind::Work
    } else {
        Kind::Other
    }
}

/// The areas the store has a writer for (there is no artifacts writer).
pub(crate) const AREAS: [&str; 2] = ["transcripts", "sessions"];

/// What the store writes in the keep bucket, all of it: with
/// `SPAWN_BACKUP_PREFIX` empty, an area's segments and manifests,
/// `transcripts/segments/…`, `transcripts/manifests/…`,
/// `sessions/segments/…` and `sessions/manifests/…` ([`Keys`]). The
/// (prefix, area) pairs, named, not matched.
pub(crate) const KEEP_WRITES: [(&str, &str); 2] = [("", "transcripts"), ("", "sessions")];

/// The test prefixes: a test run's `SPAWN_BACKUP_PREFIX` starts with one.
pub(crate) const TEST_PREFIXES: [&str; 2] = ["locktest/", "probe/"];

/// Whether a run may write an area's objects under `prefix` in `bucket`:
/// an allow-list, everything else refused (the lead's rule on rev-g F3/F4).
/// The cells, all of them:
/// - keep: switched on (`SPAWN_STORE_ENABLED=1`), the [`KEEP_WRITES`]
///   pairs only; switched off, none;
/// - work: a test prefix ([`TEST_PREFIXES`]) with one of [`AREAS`], in
///   every switch state; nothing else (no artifacts writer, so no
///   artifacts cell);
/// - any other bucket (the old one, a name of neither kind): none, in
///   every switch state.
///
/// The bucket's name is never in the message (it is a value of the
/// credential).
pub(crate) fn may_write(
    prefix: &str,
    area: &str,
    bucket: &str,
    enabled: Option<&str>,
) -> Result<(), String> {
    let on = enabled == Some("1");
    let test = TEST_PREFIXES.iter().any(|test| prefix.starts_with(test));
    let allowed = match kind(bucket) {
        Kind::Keep => on && KEEP_WRITES.contains(&(prefix, area)),
        Kind::Work => test && AREAS.contains(&area),
        Kind::Other => false,
    };
    if allowed {
        return Ok(());
    }
    Err(match kind(bucket) {
        Kind::Keep if test => "refused: a test object (a locktest/ or probe/ prefix) goes only to \
the work bucket, never the keep bucket, whose objects are there for good"
            .to_string(),
        Kind::Keep if !on => "refused: the store is switched off (SPAWN_STORE_ENABLED is not 1), \
so it writes nothing to the keep bucket"
            .to_string(),
        Kind::Keep => format!(
            "refused: the keep bucket takes only the store's own keys (no SPAWN_BACKUP_PREFIX; \
area transcripts or sessions), not {area} under {prefix:?}"
        ),
        Kind::Work => format!(
            "refused: the work bucket takes only a test object (a locktest/ or probe/ prefix, area \
transcripts or sessions), not {area} under {prefix:?}"
        ),
        Kind::Other => "refused: SPAWN_STORE_BUCKET names neither the keep nor the work bucket \
(a name ending in -keep or -work); the store writes nowhere else (the old bucket is spawn \
backup's)"
            .to_string(),
    })
}

/// The store's bucket and credentials, from the store credential `file`
/// (`R2_ENDPOINT`, `R2_ACCESS_KEY_ID`, `R2_SECRET_ACCESS_KEY`, `R2_BUCKETS`)
/// and `bucket` (`SPAWN_STORE_BUCKET`), which must be one `R2_BUCKETS`
/// lists. A missing key or a bucket not listed refuses, naming the key or
/// the setting, never a value.
pub(crate) fn store_r2(file: &Path, bucket: Option<&str>) -> Result<super::R2, String> {
    super::private_file(file, "the store credential")?;
    let vars = super::parse_env_file(
        &fs::read_to_string(file)
            .map_err(|error| format!("the store credential {}: {error}", file.display()))?,
    );
    let needed = |name: &str| -> Result<String, String> {
        vars.get(name)
            .filter(|value| !value.trim().is_empty())
            .cloned()
            .ok_or_else(|| format!("{name} is not set in {}", file.display()))
    };
    let bucket = bucket
        .map(str::trim)
        .filter(|bucket| !bucket.is_empty())
        .ok_or("SPAWN_STORE_BUCKET is required: the bucket the store reads and writes")?;
    if !needed("R2_BUCKETS")?
        .split(',')
        .any(|listed| listed.trim() == bucket)
    {
        return Err(format!(
            "SPAWN_STORE_BUCKET names a bucket R2_BUCKETS in {} does not list",
            file.display()
        ));
    }
    Ok(super::R2 {
        endpoint: needed("R2_ENDPOINT")?,
        bucket: bucket.to_string(),
        access_key: needed("R2_ACCESS_KEY_ID")?,
        secret_key: needed("R2_SECRET_ACCESS_KEY")?,
    })
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
            for root in ["projects", "file-history"] {
                let root = from.join(root);
                // The root itself is never followed either (cc-astra pass
                // 39): a symlinked projects or file-history is refused.
                match fs::symlink_metadata(&root) {
                    Ok(metadata) if !metadata.file_type().is_dir() => {
                        return Err(format!(
                            "{} is not a directory (a symlink is refused)",
                            root.display()
                        ));
                    }
                    Ok(_) => walk(from, &root, &mut out, said)?,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(format!("{}: {error}", root.display())),
                }
            }
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
pub(crate) struct Store<'a, B: Reader> {
    pub(crate) bucket: &'a B,
    pub(crate) keys: Keys,
    pub(crate) name_key: [u8; 32],
    pub(crate) seal: &'a dyn Fn(&Path, &Path) -> Result<(), String>,
    pub(crate) open: &'a dyn Fn(&Path, &Path) -> Result<(), String>,
    pub(crate) scratch: &'a Path,
    /// The random part of a manifest's key (uniqueness only, never order).
    pub(crate) nonce: &'a dyn Fn() -> Result<String, String>,
}

/// What a sent object is in the bucket: whether this run wrote it, and its
/// stored size and MD5.
struct Sent {
    wrote: bool,
    stored: u64,
    etag: String,
}

/// What `verify` found of the newest manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Verified {
    /// Objects the manifest names (distinct).
    pub(crate) named: u64,
    pub(crate) present: u64,
    pub(crate) missing: u64,
    /// There, but not of the size or MD5 the manifest says.
    pub(crate) wrong: u64,
    /// The area's objects it does not name (older versions', a failed run's).
    pub(crate) unnamed: u64,
    /// Files rebuilt from their objects and hashed against the manifest.
    pub(crate) hashed: u64,
}

impl Verified {
    pub(crate) fn ok(&self) -> bool {
        self.missing == 0 && self.wrong == 0
    }
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

/// What `plan` found: what `put` would do now, sending nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PlanCounts {
    pub(crate) files: u64,
    pub(crate) unchanged: u64,
    pub(crate) appended: u64,
    pub(crate) whole: u64,
    /// The selected files' bytes.
    pub(crate) bytes: u64,
    /// The bytes put would read and send, before encryption: a grown
    /// file's new bytes, a whole file's all (a segment already in the
    /// bucket is not sent again, so put may send less).
    pub(crate) send: u64,
    /// By group ([`group`]: the agent, or the sessions' root and project
    /// directory): files, bytes, bytes to send.
    pub(crate) by_group: BTreeMap<String, (u64, u64, u64)>,
    /// Every selected path and its size, in order.
    pub(crate) listed: Vec<(String, u64)>,
}

/// The group a plan counts a path in, never a file's name (the lead's
/// ruling on rev-g N2: a plan's printed lines are posted and read, so they
/// name directories only; the file names are in `--list` alone): the first
/// directory of a transcript (its agent), the first two directories of a
/// session file (`projects/<project>`, `file-history/<session>`), fewer
/// when the file is higher up; a file at the area's top is `(top)`, but for
/// the sessions' `history.jsonl`, a fixed name of the area.
pub(crate) fn group(area: &str, path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    let directories = &parts[..parts.len() - 1];
    let depth = if area == "transcripts" { 1 } else { 2 };
    if directories.is_empty() {
        return if area == "sessions" && path == "history.jsonl" {
            path.to_string()
        } else {
            "(top)".to_string()
        };
    }
    directories[..depth.min(directories.len())].join("/")
}

/// How `put` takes `source` against its entry in the last manifest
/// (`previous`), and the file opened to read it: one decision, shared by
/// `put` and `plan`, so the plan says what the put does.
fn classify(source: &Source, previous: Option<&Entry>) -> Result<(Plan, File), String> {
    let mut file =
        File::open(&source.file).map_err(|error| format!("{}: {error}", source.file.display()))?;
    let plan = plan(previous, source.size, source.mtime_ns, |last| {
        prefix_sha256(&mut file, last.size).map(|hash| hash == last.sha256)
    })?;
    Ok((plan, file))
}

impl<B: Reader> Store<'_, B> {
    /// The newest manifest of this area, host and account, and its key.
    pub(crate) fn newest(&self) -> Result<Option<(String, Manifest)>, String> {
        let Some(key) = self.newest_key()? else {
            return Ok(None);
        };
        let bytes = self.open_object(&key)?;
        let manifest: Manifest = serde_json::from_slice(&bytes)
            .map_err(|error| format!("the manifest {key}: {error}"))?;
        if manifest.version != MANIFEST_VERSION {
            return Err(format!(
                "the manifest {key} is of version {}, not {MANIFEST_VERSION}",
                manifest.version
            ));
        }
        Ok(Some((key, manifest)))
    }

    /// The newest manifest's key (one LIST), not opened.
    fn newest_key(&self) -> Result<Option<String>, String> {
        let listed = self.bucket.list(&self.keys.manifests())?;
        let prefix = self.keys.manifests();
        let Some(key) = listed
            .into_iter()
            .map(|listed| listed.key)
            .filter(|key| {
                // Only `<generation:012>-...` keys (A32); a key of another
                // form is never the newest.
                key.ends_with(".json.gpg")
                    && key
                        .strip_prefix(&prefix)
                        .and_then(|rest| rest.get(..13))
                        .is_some_and(|head| {
                            head.ends_with('-') && head[..12].bytes().all(|b| b.is_ascii_digit())
                        })
            })
            .max()
        else {
            return Ok(None);
        };
        Ok(Some(key))
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

    /// What `put` would do with the area's files `sources` now: the same
    /// comparison with the newest manifest (LIST and GET only), nothing
    /// sent, no manifest made.
    fn plan(&self, sources: &[Source]) -> Result<PlanCounts, String> {
        let last = self.newest()?.map(|(_, manifest)| manifest);
        self.plan_after(sources, last.as_ref(), true)
    }

    /// [`Self::plan`] against `last`; with `reuse` false (a last manifest
    /// of another key, objective 116) every file is to be sent whole, as
    /// [`Self::put_after`] sends it.
    fn plan_after(
        &self,
        sources: &[Source],
        last: Option<&Manifest>,
        reuse: bool,
    ) -> Result<PlanCounts, String> {
        let by_path: BTreeMap<&str, &Entry> = last
            .filter(|_| reuse)
            .iter()
            .flat_map(|manifest| &manifest.files)
            .map(|entry| (entry.path.as_str(), entry))
            .collect();
        let mut counts = PlanCounts::default();
        for source in sources {
            let (plan, _) = classify(source, by_path.get(source.path.as_str()).copied())?;
            let send = match plan {
                Plan::Unchanged => {
                    counts.unchanged += 1;
                    0
                }
                Plan::Append { from } => {
                    counts.appended += 1;
                    source.size - from
                }
                Plan::Whole => {
                    counts.whole += 1;
                    source.size
                }
            };
            counts.files += 1;
            counts.bytes += source.size;
            counts.send += send;
            let at = counts
                .by_group
                .entry(group(&self.keys.area, &source.path))
                .or_default();
            at.0 += 1;
            at.1 += source.size;
            at.2 += send;
            counts.listed.push((source.path.clone(), source.size));
        }
        Ok(counts)
    }
}

impl<B: Bucket> Store<'_, B> {
    /// Seal `bytes` and write them at `key` unless it exists; read back
    /// either way.
    fn send(&self, key: &str, bytes: &[u8]) -> Result<Sent, String> {
        // A key that is there is not written again; a segment found there
        // is adopted only once its contents are checked ([`Self::adopt`]),
        // a manifest found there is an error (the run's key is its own).
        if let Some(head) = self.bucket.head(key)? {
            single_put(key, &head.etag)?;
            return Ok(Sent {
                wrote: false,
                stored: head.length,
                etag: head.etag,
            });
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
            // The sealed bytes' sha256, never the plaintext's (rev-g F1): a
            // plaintext hash in the metadata would confirm a guessed
            // segment to anyone who can read the bucket.
            let sealed_sha256 = super::sha256_file(&sealed)?;
            let wrote = self.bucket.put_new(key, &sealed, &sealed_sha256)? == Put::Written;
            // Read back before it counts: what is there is what was sent.
            let head = self
                .bucket
                .head(key)?
                .ok_or_else(|| format!("{key}: not there after its PUT"))?;
            single_put(key, &head.etag)?;
            if wrote && (head.length != length || head.etag != md5 || head.sha256 != sealed_sha256)
            {
                return Err(format!(
                    "{key}: read back {} bytes, ETag {:?}, sha256 {}; sent {length} bytes, MD5 {md5:?}, sha256 {sealed_sha256}",
                    head.length, head.etag, head.sha256
                ));
            }
            Ok(Sent {
                wrote,
                stored: head.length,
                etag: head.etag,
            })
        })();
        let _ = fs::remove_file(&plain);
        let _ = fs::remove_file(&sealed);
        result
    }

    /// The segment `name` holding `bytes`: as `known` holds it (this run's
    /// or the last manifest's), else sent; its stored size and MD5.
    fn segment(
        &self,
        name: &str,
        bytes: &[u8],
        known: &mut BTreeMap<String, (u64, String)>,
        counts: &mut PutCounts,
    ) -> Result<(u64, String), String> {
        if let Some(held) = known.get(name) {
            return Ok(held.clone());
        }
        let key = self.keys.segment(name);
        let sent = self.send(&key, bytes)?;
        if sent.wrote {
            counts.segments_written += 1;
            counts.bytes_written += sent.stored;
        } else {
            self.adopt(&key, name, bytes)?;
            counts.segments_found += 1;
        }
        known.insert(name.to_string(), (sent.stored, sent.etag.clone()));
        Ok((sent.stored, sent.etag))
    }

    /// An object found at a segment's key (a HEAD, or the conditional
    /// PUT's "already there") enters the manifest only once its contents
    /// are this segment's: fetched, opened, and its length and HMAC
    /// compared with the bytes read here (cc-astra A30: never adopted on its
    /// own size and ETag). The other trusted source is the last manifest,
    /// ours and sealed, whose segments are taken without a request. A
    /// mismatch is an error that names the key: never a new baseline,
    /// never a write over it or a removal.
    fn adopt(&self, key: &str, name: &str, bytes: &[u8]) -> Result<(), String> {
        let found = self
            .open_object(key)
            .map_err(|error| format!("{key}: found, but cannot be read back: {error}"))?;
        if found.len() != bytes.len() || hex(&hmac_sha256(&self.name_key, &found)) != name {
            return Err(format!(
                "{key}: the object found there is not this segment ({} bytes against {}); \
not adopted, nothing written over it",
                found.len(),
                bytes.len()
            ));
        }
        Ok(())
    }

    /// One `put` run of the area's files `sources`.
    fn put(&self, sources: &[Source], made_ms: i64) -> Result<PutCounts, String> {
        let last = self.newest()?.map(|(_, manifest)| manifest);
        self.put_after(sources, made_ms, last.as_ref(), true)
    }

    /// One `put` run after `last`, the newest manifest: its generation is
    /// the new one's less one. With `reuse` false (`last` is another key's,
    /// objective 116) none of its segments is named again (their names are
    /// that key's HMACs): every file is put whole under this run's key.
    fn put_after(
        &self,
        sources: &[Source],
        made_ms: i64,
        last: Option<&Manifest>,
        reuse: bool,
    ) -> Result<PutCounts, String> {
        let generation = last.map_or(0, |manifest| manifest.generation) + 1;
        let by_path: BTreeMap<&str, &Entry> = last
            .filter(|_| reuse)
            .iter()
            .flat_map(|manifest| &manifest.files)
            .map(|entry| (entry.path.as_str(), entry))
            .collect();
        let mut known: BTreeMap<String, (u64, String)> = by_path
            .values()
            .flat_map(|entry| {
                entry
                    .segments
                    .iter()
                    .map(|segment| (segment.name.clone(), (segment.stored, segment.etag.clone())))
            })
            .collect();
        let mut counts = PutCounts::default();
        let mut files = Vec::new();
        for source in sources {
            counts.files += 1;
            let previous = by_path.get(source.path.as_str()).copied();
            let (plan, mut file) = classify(source, previous)?;
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
                let (stored, etag) = self.segment(&name, &bytes, &mut known, &mut counts)?;
                segments.push(Segment {
                    offset,
                    length,
                    name,
                    stored,
                    etag,
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
            version: MANIFEST_VERSION,
            generation,
            area: self.keys.area.clone(),
            host: self.keys.host.clone(),
            account: self.keys.account.clone(),
            made_ms,
            files,
        };
        let text = serde_json::to_vec(&manifest).map_err(|error| error.to_string())?;
        let key = self
            .keys
            .manifest(generation, &Stamp::at(made_ms).text(), &(self.nonce)()?);
        let sent = self.send(&key, &text)?;
        if !sent.wrote {
            return Err(format!("{key}: a manifest key that already existed"));
        }
        counts.bytes_written += sent.stored;
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

    /// The newest manifest checked (cc-astra A30): every object it names
    /// is in the bucket with the size and MD5 it recorded (one listing),
    /// and a sample of its files (the first two and the largest, each once)
    /// is rebuilt and hashed against it.
    pub(crate) fn verify(&self, manifest: &Manifest) -> Result<Verified, String> {
        let mut named: BTreeMap<String, (u64, &str)> = BTreeMap::new();
        for segment in manifest.files.iter().flat_map(|entry| &entry.segments) {
            named.insert(
                self.keys.segment(&segment.name),
                (segment.stored, segment.etag.as_str()),
            );
        }
        let held: BTreeMap<String, (u64, String)> = self
            .bucket
            .list(&format!("{}{}/segments/", self.keys.prefix, self.keys.area))?
            .into_iter()
            .map(|listed| (listed.key, (listed.size, listed.etag)))
            .collect();
        let mut found = Verified {
            named: named.len() as u64,
            ..Verified::default()
        };
        for (key, (stored, etag)) in &named {
            match held.get(key) {
                None => found.missing += 1,
                Some((size, held_etag)) if size == stored && held_etag == etag => {
                    found.present += 1;
                }
                Some(_) => found.wrong += 1,
            }
        }
        found.unnamed = held.keys().filter(|key| !named.contains_key(*key)).count() as u64;
        let mut sample: Vec<&Entry> = manifest.files.iter().take(2).collect();
        if let Some(largest) = manifest.files.iter().max_by_key(|entry| entry.size)
            && !sample.iter().any(|entry| entry.path == largest.path)
        {
            sample.push(largest);
        }
        if found.missing == 0 && found.wrong == 0 {
            for entry in sample {
                self.rebuild(entry)?;
                found.hashed += 1;
            }
        }
        Ok(found)
    }
}

/// An object's ETag is its body's MD5 only when it was sent in one request:
/// a multipart upload's is `<md5 of the parts' MD5s>-<parts>`. Every
/// segment and manifest is sent with one PUT ([`super::put_new_object`]),
/// so `verify`'s MD5 check holds; an object whose ETag is not a plain MD5
/// is refused here rather than recorded, so a change to multipart fails the
/// run (and `a_multipart_etag_is_refused`) instead of every later verify.
fn single_put(key: &str, etag: &str) -> Result<(), String> {
    if etag.len() == 32
        && etag
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        Ok(())
    } else {
        Err(format!(
            "{key}: ETag {etag:?} is not one PUT's MD5 (a multipart object?); not recorded"
        ))
    }
}

/// The partial name's mark: a restore whose write did not finish leaves
/// `<file>.spawn-store-get.partial` (cc-astra A37).
pub(crate) const PARTIAL_MARK: &str = ".spawn-store-get.partial";

/// Write a rebuilt file into `into` (`get`, cc-astra A29, A37 and A38):
/// through descriptors from `into` down, no parent followed through a
/// symlink, written to a file with no name, read back through its
/// descriptor and compared with `bytes`, and only then that file named,
/// never over anything, never removing anything (spawn-host's
/// `create_with`). Refused when anything is already at the path or at its
/// partial name; a failed write or comparison leaves the bytes at the
/// partial name and names it.
pub(crate) fn restore(into: &Path, path: &str, bytes: &[u8]) -> Result<(), String> {
    restore_with(into, path, bytes, |file| file.write_all(bytes))
}

/// [`restore`] with the write given (a test fails it midway, or changes the
/// file under it); what is read back must be `bytes`.
pub(crate) fn restore_with(
    into: &Path,
    path: &str,
    bytes: &[u8],
    write: impl FnOnce(&mut File) -> std::io::Result<()>,
) -> Result<(), String> {
    use spawn_host::agent_files::Created;
    use std::io::Read as _;
    let relative = spawn_host::tree_path::TreePath::from_bytes(path.as_bytes());
    // The bytes as the descriptor reads them, one byte past the expected
    // length so a longer file is seen.
    let confirm = |file: &mut File| {
        let mut held = Vec::with_capacity(bytes.len());
        file.take(bytes.len() as u64 + 1).read_to_end(&mut held)?;
        if held == bytes {
            Ok(())
        } else {
            Err(std::io::Error::other(format!(
                "the file holds {} bytes, not the {} verified (changed while written)",
                held.len(),
                bytes.len()
            )))
        }
    };
    match spawn_host::agent_files::create_with(into, &relative, PARTIAL_MARK, write, confirm) {
        Ok(Created::Written) => Ok(()),
        Ok(Created::Exists(taken)) => Err(format!(
            "refused: {} already holds something at {taken}; nothing written",
            into.display()
        )),
        Ok(Created::Partial { partial, why }) => Err(format!(
            "{path}: not restored ({why}); the bytes written are left at {} \
(a partial restore: nothing was removed, and a restore into the same place refuses until \
it is moved away)",
            into.join(partial.as_path()).display()
        )),
        Ok(Created::Unkept { why }) => Err(format!(
            "{path}: not restored ({why}); the bytes written have no name: nothing was \
named, moved or removed"
        )),
        Err(error) => Err(format!("refused: {path} under {}: {error}", into.display())),
    }
}

/// The entry point: `spawn store <verb> ...`.
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
    // The store's own bucket and credential; r2.env is never read (rev-g N5).
    let credential = env_value("SPAWN_STORE_ENV_FILE").map_or_else(
        || super::home().join(".config/spawn/r2-store.env"),
        PathBuf::from,
    );
    let settings = Settings::with_r2(&flags, || {
        store_r2(&credential, env_value("SPAWN_STORE_BUCKET").as_deref())
    })?;
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
        nonce: &|| random_hex(4),
    };
    // The newest manifest's own key (objective 116): when it is another
    // key's, that key opens it, and its file must be here. Without it the
    // run refuses before anything is written, never a fresh chain over a
    // manifest it could not read.
    let sealing = settings.seal.as_ref().map(|key| key.name.clone()).map_err(Clone::clone)?;
    let older_key = match older_key(&store, &sealing) {
        Ok(older_key) => older_key,
        Err(reason) => {
            eprintln!("spawn store {verb}: {reason}");
            return Ok(2);
        }
    };
    let open_older = |input: &Path, out: &Path| match &older_key {
        Some(key) => super::decrypt_with(key, input, out),
        None => Err("no other key".to_string()),
    };
    let older = match &older_key {
        Some(key) => Some(Store {
            bucket: &settings,
            keys: store.keys.clone(),
            name_key: name_key_of(&key.file)?,
            seal: &seal,
            open: &open_older,
            scratch: &scratch.path,
            nonce: &|| random_hex(4),
        }),
        None => None,
    };
    // get, list and verify read the newest manifest with its own key.
    let reading = older.as_ref().unwrap_or(&store);
    match verb.as_str() {
        "put" => put_verb(&store, older.as_ref(), &settings, &flags, &area, &sealing),
        "plan" => {
            let reader = ReadOnly(&settings);
            let planner = Store {
                bucket: &reader,
                keys: store.keys.clone(),
                name_key: store.name_key,
                seal: store.seal,
                open: store.open,
                scratch: store.scratch,
                nonce: store.nonce,
            };
            let other_last = older
                .as_ref()
                .map(|older| older.newest().map(|newest| newest.map(|(_, manifest)| manifest)))
                .transpose()?;
            plan_verb(&planner, other_last, &settings, &flags, &area)
        }
        "get" => get_verb(reading, &flags, &area),
        "list" => list_verb(reading, &area),
        "verify" => verify_verb(reading, &area),
        other => Err(format!("unknown store command {other}\n{USAGE}")),
    }
}

/// The key of the newest manifest when it is not `sealing` (objective
/// 116): its object's `x-amz-meta-spawn-seal` (absent: k1), and that key's
/// file on this host, or why there is none ("the last manifest is sealed
/// with <name>; its key file <path> is not on this host"). `None` when there
/// is no manifest, or the newest is the sealing key's.
fn older_key<B: Reader>(store: &Store<'_, B>, sealing: &str) -> Result<Option<Key>, String> {
    let Some(newest) = store.newest_key()? else {
        return Ok(None);
    };
    let named = store.bucket.sealed_with(&newest)?;
    if named == sealing {
        return Ok(None);
    }
    super::reading_key(&named, "the last manifest").map(Some)
}

/// `list`: the area's manifests counted, and the newest one's files.
fn list_verb(store: &Store<'_, Settings>, area: &str) -> Result<i32, String> {
    let listed = store.bucket.list(&store.keys.manifests())?;
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

/// `verify`: the newest manifest checked ([`Store::verify`]).
fn verify_verb(store: &Store<'_, Settings>, area: &str) -> Result<i32, String> {
    let (key, manifest) = store
        .newest()?
        .ok_or("no manifest in the bucket for this area, host and account")?;
    let found = store.verify(&manifest)?;
    println!(
        "store verify {area}: {key}: {} file(s), {} object(s) named: {} present with their size and MD5, {} missing, {} of another size or MD5; {} not named by it; {} file(s) rebuilt and hashed",
        manifest.files.len(),
        found.named,
        found.present,
        found.missing,
        found.wrong,
        found.unnamed,
        found.hashed
    );
    Ok(i32::from(!found.ok()))
}

/// `put`: one run over the area's files, under the backup lock.
fn put_verb(
    store: &Store<'_, Settings>,
    older: Option<&Store<'_, Settings>>,
    settings: &Settings,
    flags: &Flags,
    area: &str,
    sealing: &str,
) -> Result<i32, String> {
    may_write(
        &settings.prefix,
        area,
        &settings.r2.bucket,
        env_value("SPAWN_STORE_ENABLED").as_deref(),
    )?;
    let _lock = Lock::acquire(&settings.root)?;
    let from = from_dir(settings, flags, area);
    let mut said = Vec::new();
    let sources = sources(area, &from, &mut said)?;
    for line in &said {
        println!("store put {area}: {line}");
    }
    // After another key's manifest (`older` opens it): its generation, and
    // every file whole under the sealing key.
    let counts = match older {
        Some(older) => {
            let last = older.newest()?.map(|(_, manifest)| manifest);
            store.put_after(&sources, now_ms(), last.as_ref(), false)?
        }
        None => store.put(&sources, now_ms())?,
    };
    println!("store put {area}: {}", put_summary(&counts));
    // The run, as a `backup.stored` record (its manifest's key and counts,
    // nothing of a file's content). The copy is in the bucket already; a
    // record that cannot be written fails the run, and the next run reads
    // its last manifest from the bucket either way.
    let record = serde_json::json!({
        "type": "backup.stored",
        "sealed_with": sealing,
        "area": area,
        "host": store.keys.host,
        "account": store.keys.account,
        "manifest": counts.manifest,
        "files": counts.files,
        "unchanged": counts.unchanged,
        "grown": counts.appended,
        "whole": counts.whole,
        "segments_written": counts.segments_written,
        "segments_found": counts.segments_found,
        "bytes_written": counts.bytes_written,
    });
    crate::config::open_store()?
        .backup_record(&record)
        .map_err(|error| format!("backup.stored: {error}"))?;
    Ok(0)
}

/// The area's files' directory: `--from`, else the transcript archive
/// under the root, or the account's `.claude`.
fn from_dir(settings: &Settings, flags: &Flags, area: &str) -> PathBuf {
    match flags.get("from") {
        Some(from) => PathBuf::from(from),
        None if area == "transcripts" => settings.root.join(spawn_host::TRANSCRIPT_DIRECTORY),
        None => super::home().join(".claude"),
    }
}

/// What a run's files are, as `put` and `plan` both begin their line: the
/// two agree on it when nothing changed between them.
fn files_text(files: u64, unchanged: u64, grown: u64, whole: u64) -> String {
    format!("{files} file(s): {unchanged} unchanged, {grown} grown, {whole} whole")
}

/// `put`'s line after its area.
pub(crate) fn put_summary(counts: &PutCounts) -> String {
    format!(
        "{}; {} segment(s) written ({} bytes), {} already there; manifest {}",
        files_text(
            counts.files,
            counts.unchanged,
            counts.appended,
            counts.whole
        ),
        counts.segments_written,
        counts.bytes_written,
        counts.segments_found,
        counts.manifest
    )
}

/// `plan`'s line after its area.
pub(crate) fn plan_summary(counts: &PlanCounts) -> String {
    format!(
        "{}; {} bytes, {} of them to send (before encryption; a segment already in the bucket \
is not sent again)",
        files_text(
            counts.files,
            counts.unchanged,
            counts.appended,
            counts.whole
        ),
        counts.bytes,
        counts.send
    )
}

/// `plan`: what `put` would upload now, uploading nothing (lead-mac's go,
/// 2026-11-13, step 1). The same selection (`sources`; the entries put
/// would skip are counted by why, not named) and the same comparison with the newest
/// manifest (LIST and GET), over a bucket that cannot write ([`ReadOnly`]):
/// no PUT is reachable, no manifest is made, no `backup.stored` recorded,
/// no lock taken. `may_write` is not consulted: it guards writes, and a
/// plan makes none, so it runs with SPAWN_STORE_ENABLED unset and on a keep
/// bucket too. The bucket is named by its kind and prefix only (its name is
/// a credential value). `--list` writes each selected path and its size
/// (`<size>\t<path as a JSON string>`, one a line) to a new file, mode
/// 0600, created exclusively (`O_CREAT | O_EXCL`, which refuses any entry
/// at the name, a symlink included): it never writes over anything, and is
/// left (empty) if the plan fails.
fn plan_verb<R: Reader>(
    store: &Store<'_, R>,
    other_last: Option<Option<Manifest>>,
    settings: &Settings,
    flags: &Flags,
    area: &str,
) -> Result<i32, String> {
    let mut list = flags
        .get("list")
        .map(|path| create_list(Path::new(&path)))
        .transpose()?;
    let from = from_dir(settings, flags, area);
    let mut said = Vec::new();
    let sources = sources(area, &from, &mut said)?;
    // After another key's manifest, as put decides: every file whole.
    let counts = match other_last {
        Some(last) => store.plan_after(&sources, last.as_ref(), false)?,
        None => store.plan(&sources)?,
    };
    let bucket = match kind(&settings.r2.bucket) {
        Kind::Keep => "keep",
        Kind::Work => "work",
        Kind::Other => "other",
    };
    for line in plan_lines(area, bucket, &settings.prefix, &said, &counts) {
        println!("{line}");
    }
    if let Some(file) = list.as_mut() {
        write_list(file, &counts.listed)?;
    }
    Ok(0)
}

/// What `plan` prints: a summary that names no file (the lead's ruling:
/// it is posted and read). The entries put would skip, counted by why
/// (put's own lines name them); the bucket by kind and prefix; the totals;
/// a line per group ([`group`]) with its files, bytes and bytes to send.
pub(crate) fn plan_lines(
    area: &str,
    bucket: &str,
    prefix: &str,
    said: &[String],
    counts: &PlanCounts,
) -> Vec<String> {
    let mut skipped: BTreeMap<&str, u64> = BTreeMap::new();
    for line in said {
        let why = line
            .rfind(" (")
            .and_then(|at| line[at + 2..].strip_suffix(')'))
            .unwrap_or("other");
        *skipped.entry(why).or_default() += 1;
    }
    let mut lines: Vec<String> = skipped
        .into_iter()
        .map(|(why, count)| {
            format!("store plan {area}: {count} entry(ies) skipped as put skips them ({why})")
        })
        .collect();
    lines.push(format!(
        "store plan {area}: the {bucket} bucket, prefix {prefix:?}; nothing is sent"
    ));
    lines.push(format!("store plan {area}: {}", plan_summary(counts)));
    for (group, (files, bytes, send)) in &counts.by_group {
        lines.push(format!(
            "store plan {area}: {group}: {files} file(s), {bytes} bytes, {send} to send"
        ));
    }
    lines
}

/// `--list`'s file: new, mode 0600, `O_CREAT | O_EXCL` (any entry at the
/// name, a symlink included, is refused and left as it is).
pub(crate) fn create_list(path: &Path) -> Result<File, String> {
    use std::os::unix::fs::OpenOptionsExt as _;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("--list {}: {error} (never written over)", path.display()))
}

/// Each path and its size, `<size>\t<path as a JSON string>`, one a line.
pub(crate) fn write_list(file: &mut File, listed: &[(String, u64)]) -> Result<(), String> {
    for (path, size) in listed {
        let quoted = serde_json::to_string(path).map_err(|error| error.to_string())?;
        writeln!(file, "{size}\t{quoted}").map_err(|error| format!("--list: {error}"))?;
    }
    file.sync_all().map_err(|error| format!("--list: {error}"))
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
    restore(&into, &entry.path, &bytes)?;
    let target = into.join(&entry.path);
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
