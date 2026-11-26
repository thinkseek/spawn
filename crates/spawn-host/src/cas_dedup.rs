//! Language-agnostic deduplication of build artifacts across workspaces,
//! by content address and copy-on-write clone.
//!
//! Workers that build the same code in their own workspaces write the same
//! large files (`target/`, `node_modules/`, caches) again and again. This
//! module walks ordinary directories, hashes every eligible file (SHA-256),
//! keeps one canonical read-only blob per digest in a [`spawn_cas::Store`]
//! at `<cas_root>/objects/sha256/<first two hex digits>/<digest>`, and replaces
//! each duplicate in place with a copy-on-write clone of that blob: the
//! file keeps its path, mode and mtime (so `cargo`, `make` and `npm`
//! freshness checks see nothing change), but its data extents are now
//! shared on disk. No compiler wrapper, no agent awareness, no mount.
//!
//! # Contract: offline artifact roots only
//!
//! A replacement is a rename over the file. A process that holds the file
//! open (a log appender, a linker, an `mmap` writer) would go on writing to
//! the old, now unlinked inode, and its writes would be lost from the path;
//! no stat, age threshold or advisory lock taken by this pass can see such
//! a writer. So [`dedup_dirs`] runs only over [`ArtifactRoot`]s:
//!
//! - a disposable build-artifact directory ([`ARTIFACT_DIRECTORIES`]:
//!   `target`, `node_modules`), never a source tree or an arbitrary root;
//! - held offline: an [`ArtifactRoot`] borrows the exclusive side of its
//!   owning workspace's admission lock ([`WorkspaceLease::exclusive`], or
//!   [`crate::CowWorkspace::offline`]) for the whole pass, so no managed
//!   worker runs in it or starts meanwhile. Users outside that lifecycle
//!   are expected to be stopped; a process seen working in a root
//!   ([`crate::cow::directory_occupancy`]) still refuses it whole
//!   ([`DedupError::Live`]).
//!
//! A replacement is also refused, file by file, when:
//!
//! - the file is a symlink (never followed either) or a special file, or
//!   was swapped for one after it was listed (opened `O_NOFOLLOW |
//!   O_NONBLOCK`, so a FIFO never blocks the pass);
//! - it is below [`DedupConfig::min_size`] (default 64 KiB);
//! - it is a `*.jsonl` file or lies under a `.transcripts/` directory;
//! - it belongs to another user or carries extended attributes (an ACL
//!   included): the clone that replaces it gets only its mode and times;
//! - another process holds an `flock` on it, it changed within
//!   [`DedupConfig::min_age`], its stamp (inode, size, mtime, ctime) moved
//!   while it was hashed, or it has more than one hard link;
//! - the blob for its digest is not a plain single-link file, or the clone
//!   made from it does not hash to that digest (a tampered or planted blob
//!   is refused, never copied over a file).
//!
//! A directory or entry the walk cannot read is an error in the report and
//! marks its root incomplete ([`DedupReport::incomplete`]); nothing is
//! skipped silently.
//!
//! Without a clone-capable file system nothing is changed at all
//! ([`DedupReport::cow_supported`]): a hard link would share mutations, and
//! a copy saves nothing.
//!
//! A CAS root carries a marker file ([`CAS_MARKER`]) made by the first
//! pass; a non-empty directory without it is never used as one, and
//! [`prune_unused`] refuses to prune it. The index (`<cas_root>/index`)
//! remembers which files were already made clones of which blob, so a
//! second pass skips them unread, and [`prune_unused`] counts the blobs'
//! live references from it. A pass holds the CAS's publication lock
//! (shared) from its first blob to its last index entry, and a prune holds
//! it exclusively: no blob or temporary is pruned between its creation and
//! its index entry. Deleting a blob never touches a workspace file: the
//! file system refcounts the shared extents, so a pruned blob only stops
//! being a clone source.
//!
//! # The store is `spawn-cas`
//!
//! Blobs are `spawn-cas` objects (`sha256` namespace of `objects/`): the
//! marker, the layout, the publication of a blob (verified, mode 0444,
//! never replacing one) and its removal go through [`spawn_cas::Store`].
//! A CAS made before `spawn-cas` kept its blobs at `<cas_root>/<2 hex>/<64
//! hex>`; the first pass or prune that opens it moves them to
//! `objects/sha256/` by a rename ([`spawn_cas::Store::adopt_legacy_shards`]):
//! nothing is rehashed, and nothing that is not such a blob is touched.
//! The index keeps naming blobs by their 64 hex digits (all SHA-256), so an
//! existing index stays valid. [`prune_unused`] never removes a blob that
//! a `spawn-cas` pin names.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use spawn_cas::{Algo, CasError, Publish, Store, verify_reader};

use crate::cow::{
    WorkspaceLease, cow_clone_supported, directory_occupancy, hex, is_jsonl, reflink_new,
    unique_suffix,
};

/// The default smallest file worth deduplicating.
pub const DEFAULT_MIN_SIZE: u64 = 64 * 1024;

/// The file that marks a directory as a CAS root.
pub const CAS_MARKER: &str = spawn_cas::CAS_MARKER;

/// The names of the disposable build-artifact directories a pass may
/// deduplicate ([`ArtifactRoot`]).
pub const ARTIFACT_DIRECTORIES: [&str; 2] = ["target", "node_modules"];

/// A build-artifact directory held offline for a pass: see the module
/// documentation. It borrows the exclusive side of the admission lock of
/// the workspace it is the direct child of, taken for the directory that
/// is that workspace now.
#[derive(Debug)]
pub struct ArtifactRoot<'a> {
    /// The root, resolved through every link (none is followed later).
    directory: PathBuf,
    /// Its `(device, inode)` when it was checked.
    identity: (u64, u64),
    offline: &'a WorkspaceLease,
}

impl<'a> ArtifactRoot<'a> {
    /// `directory`, which must resolve to `<workspace>/<name>` of the
    /// workspace `offline` guards, with `<name>` one of
    /// [`ARTIFACT_DIRECTORIES`], and be a plain directory: a link to
    /// another workspace's artifacts, or a path through one, is refused
    /// ([`DedupError::NotArtifact`]). `offline` must be the exclusive side
    /// held for the workspace directory there now
    /// ([`DedupError::NotOffline`]).
    pub fn new(
        offline: &'a WorkspaceLease,
        directory: impl Into<PathBuf>,
    ) -> Result<Self, DedupError> {
        let given = directory.into();
        if !offline.holds_exclusive() {
            return Err(DedupError::NotOffline(given));
        }
        let not_artifact = || DedupError::NotArtifact(given.clone());
        let plain = fs::symlink_metadata(&given).is_ok_and(|meta| meta.is_dir());
        let resolved = fs::canonicalize(&given).map_err(|_| not_artifact())?;
        let named = resolved
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| ARTIFACT_DIRECTORIES.contains(&name));
        if !plain || !named || resolved.parent() != Some(offline.workspace()) {
            return Err(not_artifact());
        }
        let meta = fs::symlink_metadata(&resolved)?;
        Ok(Self {
            directory: resolved,
            identity: (meta.dev(), meta.ino()),
            offline,
        })
    }

    /// The directory (resolved).
    pub fn path(&self) -> &Path {
        &self.directory
    }

    /// Whether the root and its workspace are still what was checked.
    fn unchanged(&self) -> bool {
        self.offline.holds_exclusive()
            && fs::symlink_metadata(&self.directory)
                .is_ok_and(|meta| meta.is_dir() && (meta.dev(), meta.ino()) == self.identity)
    }
}

/// What [`dedup_dirs`] considers and where it keeps blobs.
#[derive(Debug, Clone)]
pub struct DedupConfig {
    /// The content-addressed store; on the same file system as the
    /// directories (a clone cannot cross file systems).
    pub cas_root: PathBuf,
    /// Files smaller than this are left alone.
    pub min_size: u64,
    /// Files whose mtime or ctime is younger than this are being written
    /// and left alone.
    pub min_age: Duration,
}

impl DedupConfig {
    /// Defaults: 64 KiB, two seconds of quiet.
    pub fn new(cas_root: impl Into<PathBuf>) -> Self {
        Self {
            cas_root: cas_root.into(),
            min_size: DEFAULT_MIN_SIZE,
            min_age: Duration::from_secs(2),
        }
    }
}

/// Why [`dedup_dirs`] did nothing.
#[derive(Debug)]
pub enum DedupError {
    /// A process works in `directory` (its working directory, an open file
    /// or a mapping is there): a live workspace is never deduplicated.
    Live {
        /// The directory refused.
        directory: PathBuf,
        /// The processes seen in it.
        pids: Vec<u32>,
    },
    /// The CAS root is a non-empty directory without [`CAS_MARKER`].
    NotCas(PathBuf),
    /// The directory is not a plain build-artifact directory
    /// ([`ARTIFACT_DIRECTORIES`]).
    NotArtifact(PathBuf),
    /// The admission given for the directory is a worker's lease, not the
    /// exclusive side: its workspace is not offline.
    NotOffline(PathBuf),
    /// The CAS root or the process table could not be read or written.
    Io(io::Error),
}

impl std::fmt::Display for DedupError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Live { directory, pids } => write!(
                formatter,
                "processes {pids:?} work in {}: only a stopped or unmounted workspace \
                 is deduplicated",
                directory.display()
            ),
            Self::NotCas(root) => write!(
                formatter,
                "{} is not a CAS root (no {CAS_MARKER}, and not empty)",
                root.display()
            ),
            Self::NotArtifact(directory) => write!(
                formatter,
                "{} is not a build-artifact directory ({})",
                directory.display(),
                ARTIFACT_DIRECTORIES.join(", ")
            ),
            Self::NotOffline(directory) => write!(
                formatter,
                "{} is not held offline (a lease is not the exclusive admission)",
                directory.display()
            ),
            Self::Io(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for DedupError {}

impl From<io::Error> for DedupError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// What one [`dedup_dirs`] pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DedupReport {
    /// Whether the CAS root can clone; when false nothing was changed.
    pub cow_supported: bool,
    /// Regular files at or above the size threshold that were considered.
    pub eligible: usize,
    /// Files replaced by a clone of an existing blob.
    pub deduplicated: usize,
    /// The sizes of those files: an upper bound of the bytes freed (a file
    /// whose extents were already shared frees less).
    pub reclaimed_bytes: u64,
    /// Blobs created (cloned from the first file of their content).
    pub blobs_created: usize,
    /// Files the index already knew as clones of their blob.
    pub already_shared: usize,
    /// Symlinks skipped.
    pub skipped_symlinks: usize,
    /// Files under the size threshold.
    pub skipped_small: usize,
    /// `*.jsonl` files and `.transcripts/` contents.
    pub skipped_transcripts: usize,
    /// Locked, recently changed, changing or swapped files.
    pub skipped_busy: usize,
    /// Special files and files with several hard links.
    pub skipped_other: usize,
    /// What failed, one line per file or directory; the pass went on.
    pub errors: Vec<String>,
    /// The roots some part of which could not be read: not wholly
    /// deduplicated.
    pub incomplete: Vec<PathBuf>,
    /// Processes the occupancy look could not inspect
    /// ([`crate::cow::DirectoryOccupancy::uninspected`]).
    pub uninspected: Vec<String>,
}

/// What [`prune_unused`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneReport {
    /// Index entries whose file is still the clone it was made.
    pub live_entries: usize,
    /// Index entries dropped (file gone or rewritten).
    pub dropped_entries: usize,
    /// Blobs some live entry references.
    pub blobs_kept: usize,
    /// Blobs removed (no live reference, older than the grace period).
    pub blobs_removed: usize,
    /// Bytes of the removed blobs.
    pub bytes_removed: u64,
    /// Leftover temporary files removed.
    pub temporaries_removed: usize,
    /// Blobs or temporaries that could not be removed, one line each.
    pub errors: Vec<String>,
}

/// One index line: a file made a clone of `hash`, and its stamp then.
#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexEntry {
    hash: String,
    dev: u64,
    ino: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    path: PathBuf,
}

impl IndexEntry {
    fn new(hash: &str, path: &Path, meta: &fs::Metadata) -> Self {
        Self {
            hash: hash.to_string(),
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
            ctime: (meta.ctime(), meta.ctime_nsec()),
            path: path.to_path_buf(),
        }
    }

    /// Whether `meta` is still the file this entry recorded.
    fn matches(&self, meta: &fs::Metadata) -> bool {
        self.dev == meta.dev()
            && self.ino == meta.ino()
            && self.size == meta.size()
            && self.mtime == (meta.mtime(), meta.mtime_nsec())
            && self.ctime == (meta.ctime(), meta.ctime_nsec())
    }

    fn render(&self) -> String {
        let path = hex(self.path.as_os_str().as_bytes());
        format!(
            "{} {} {} {} {} {} {} {} {path}\n",
            self.hash,
            self.dev,
            self.ino,
            self.size,
            self.mtime.0,
            self.mtime.1,
            self.ctime.0,
            self.ctime.1
        )
    }

    fn parse(line: &str) -> Option<Self> {
        let fields: Vec<&str> = line.split(' ').collect();
        let [hash, dev, ino, size, ms, mns, cs, cns, path] = fields.as_slice() else {
            return None;
        };
        if !path.len().is_multiple_of(2) || !is_hash(hash) {
            return None;
        }
        let bytes: Option<Vec<u8>> = (0..path.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(path.get(at..at + 2)?, 16).ok())
            .collect();
        Some(Self {
            hash: (*hash).to_string(),
            dev: dev.parse().ok()?,
            ino: ino.parse().ok()?,
            size: size.parse().ok()?,
            mtime: (ms.parse().ok()?, mns.parse().ok()?),
            ctime: (cs.parse().ok()?, cns.parse().ok()?),
            path: PathBuf::from(std::ffi::OsStr::from_bytes(&bytes?)),
        })
    }
}

/// Whether `name` is a blob name: 64 lowercase hex digits.
fn is_hash(name: &str) -> bool {
    spawn_cas::is_hex64(name)
}

/// The `sha256:` address of a 64-digit hash (checked by the caller).
fn sha256_digest(hash: &str) -> io::Result<spawn_cas::Digest> {
    spawn_cas::Digest::from_parts(Algo::Sha256, hash).map_err(io::Error::from)
}

/// Whether `name` is a temporary this module makes: `<hash>.<pid>-<n>`.
fn is_temporary(name: &str) -> bool {
    let Some((hash, suffix)) = name.split_once('.') else {
        return false;
    };
    let Some((pid, ordinal)) = suffix.split_once('-') else {
        return false;
    };
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    is_hash(hash) && digits(pid) && digits(ordinal)
}

/// A lock of the CAS: `index.lock`, held while the index is read or
/// written, or `publish.lock`, held shared by a pass from its first blob to
/// its last index entry and exclusively by a prune.
struct IndexLock {
    _file: fs::File,
}

const INDEX_LOCK: &str = "index.lock";
const PUBLISH_LOCK: &str = "publish.lock";

impl IndexLock {
    fn take(cas_root: &Path, name: &str, exclusive: bool) -> io::Result<Self> {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(cas_root.join(name))?;
        if exclusive {
            file.lock()?;
        } else {
            file.lock_shared()?;
        }
        Ok(Self { _file: file })
    }
}

/// The index; empty only when there is none. One that cannot be read, or a
/// line that does not parse, is an error (a prune never drops an entry on
/// a guess).
fn read_index(cas_root: &Path) -> io::Result<Vec<IndexEntry>> {
    let path = cas_root.join("index");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    text.lines()
        .map(|line| {
            IndexEntry::parse(line).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{} holds a line that is not an entry: {line:?}",
                        path.display()
                    ),
                )
            })
        })
        .collect()
}

/// A file opened for reading without following a symlink at its name and
/// without blocking on a FIFO.
fn open_nofollow(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

/// SHA-256 of a file's bytes, in lowercase hex.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = open_nofollow(path)?;
    sha256_reader(&mut file)
}

fn sha256_reader(reader: &mut impl Read) -> io::Result<String> {
    Ok(spawn_cas::sha256_reader(reader)?.hex().to_string())
}

fn same_stamp(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.size() == right.size()
        && (left.mtime(), left.mtime_nsec()) == (right.mtime(), right.mtime_nsec())
        && (left.ctime(), left.ctime_nsec()) == (right.ctime(), right.ctime_nsec())
}

/// Whether an open or a `stat` failed because the name is gone or is now a
/// link: the file moved, which is no error.
fn moved(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ELOOP)
}

/// Whether `path` is still the file `before` was: `false` when it is gone
/// or changed; an error when it cannot be told.
fn still(path: &Path, before: &fs::Metadata) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(same_stamp(before, &meta)),
        Err(error) if moved(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

fn young(meta: &fs::Metadata, min_age: Duration) -> bool {
    if min_age.is_zero() {
        return false;
    }
    let Some(cutoff) = SystemTime::now().checked_sub(min_age) else {
        return true;
    };
    let cutoff = cutoff
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let cutoff_ns = i128::try_from(cutoff.as_nanos()).unwrap_or(i128::MAX);
    let at = |seconds: i64, nanos: i64| i128::from(seconds) * 1_000_000_000 + i128::from(nanos);
    at(meta.mtime(), meta.mtime_nsec()) > cutoff_ns
        || at(meta.ctime(), meta.ctime_nsec()) > cutoff_ns
}

/// The `spawn-cas` store at `cas_root`, made (marker first) when missing
/// or empty, its legacy `<2 hex>/<64 hex>` blobs adopted; a non-empty
/// directory that is not one is refused ([`DedupError::NotCas`]).
fn open_store(cas_root: &Path) -> Result<Store, DedupError> {
    let store = Store::open(cas_root).map_err(|error| match error {
        CasError::NotCas(root) => DedupError::NotCas(root),
        other => DedupError::Io(other.into()),
    })?;
    store
        .adopt_legacy_shards()
        .map_err(|error| DedupError::Io(error.into()))?;
    Ok(store)
}

fn check_cas_root_claimable(cas_root: &Path) -> Result<(), DedupError> {
    if fs::symlink_metadata(cas_root.join(CAS_MARKER)).is_ok_and(|meta| meta.is_file()) {
        return Ok(());
    }
    match fs::symlink_metadata(cas_root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(DedupError::Io(error)),
        Ok(meta) if meta.is_dir() && fs::read_dir(cas_root)?.next().is_none() => Ok(()),
        Ok(_) => Err(DedupError::NotCas(cas_root.to_path_buf())),
    }
}

fn cow_probe_dir(cas_root: &Path) -> PathBuf {
    let tmp = cas_root.join("tmp");
    if tmp.is_dir() {
        return tmp;
    }
    let mut current = Some(cas_root);
    while let Some(dir) = current {
        if dir.is_dir() {
            return dir.to_path_buf();
        }
        current = dir.parent();
    }
    PathBuf::from(".")
}

/// One pass over `roots`: see the module documentation.
///
/// Refused whole, before anything is written, when a process is seen
/// working in one of the roots, or cannot be looked for
/// ([`DedupError::Live`], [`DedupError::Io`]), or the CAS root is a foreign
/// directory ([`DedupError::NotCas`]). What fails on one file or directory
/// is in [`DedupReport::errors`] (its root in
/// [`DedupReport::incomplete`]) and the pass goes on. Safe to run from
/// several processes (or threads) over the same CAS at once.
pub fn dedup_dirs(
    config: &DedupConfig,
    roots: &[ArtifactRoot<'_>],
) -> Result<DedupReport, DedupError> {
    let mut uninspected = Vec::new();
    for root in roots {
        let found = directory_occupancy(&root.directory)?;
        if !found.pids.is_empty() {
            return Err(DedupError::Live {
                directory: root.directory.clone(),
                pids: found.pids,
            });
        }
        uninspected.extend(found.uninspected);
    }
    let cas_root = crate::root::canonical_root(&config.cas_root);
    check_cas_root_claimable(&cas_root)?;
    if !cow_clone_supported(&cow_probe_dir(&cas_root)) {
        return Ok(DedupReport {
            cow_supported: false,
            uninspected,
            ..DedupReport::default()
        });
    }
    let store = open_store(&cas_root)?;
    let mut report = DedupReport {
        cow_supported: cow_clone_supported(&store.tmp_dir()),
        uninspected,
        ..DedupReport::default()
    };
    if !report.cow_supported {
        return Ok(report);
    }
    // From the first blob to the last index entry: no prune meanwhile.
    let _publishing = IndexLock::take(&cas_root, PUBLISH_LOCK, false)?;
    let known: BTreeMap<(u64, u64), IndexEntry> = {
        let _lock = IndexLock::take(&cas_root, INDEX_LOCK, false)?;
        read_index(&cas_root)?
            .into_iter()
            .map(|entry| ((entry.dev, entry.ino), entry))
            .collect()
    };
    let owner = crate::current_uid();
    let mut pass = Pass {
        config,
        cas_root: &cas_root,
        store: &store,
        known: &known,
        owner,
        report: &mut report,
        added: Vec::new(),
    };
    for root in roots {
        let errors = pass.report.errors.len();
        if root.unchanged() {
            pass.walk(&root.directory);
        } else {
            pass.report.errors.push(format!(
                "{} or its workspace changed since it was checked: not walked",
                root.directory.display()
            ));
        }
        if pass.report.errors.len() > errors {
            pass.report.incomplete.push(root.directory.clone());
        }
    }
    let added = std::mem::take(&mut pass.added);
    if !added.is_empty() {
        // The whole index, replaced in one rename: a reader never sees a
        // torn row.
        let _lock = IndexLock::take(&cas_root, INDEX_LOCK, true)?;
        let mut index = read_index(&cas_root)?;
        index.extend(added);
        publish_index(&cas_root, &index)?;
    }
    Ok(report)
}

struct Pass<'a> {
    config: &'a DedupConfig,
    cas_root: &'a Path,
    store: &'a Store,
    known: &'a BTreeMap<(u64, u64), IndexEntry>,
    /// This user: a file of anyone else is left alone.
    owner: u32,
    report: &'a mut DedupReport,
    added: Vec<IndexEntry>,
}

/// Why a file was left as it was.
enum Skip {
    Busy,
    Other,
}

/// What [`Pass::seed`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seeded {
    /// The blob was published from this file.
    Created,
    /// The file changed while it was cloned: left alone.
    Changed,
    /// Another pass published the blob first.
    Lost,
}

impl Pass<'_> {
    fn walk(&mut self, directory: &Path) {
        if directory.starts_with(self.cas_root) {
            return;
        }
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) => {
                self.report
                    .errors
                    .push(format!("read {}: {error}", directory.display()));
                return;
            }
        };
        let mut paths = Vec::new();
        for entry in entries {
            match entry {
                Ok(entry) => paths.push(entry.path()),
                Err(error) => self
                    .report
                    .errors
                    .push(format!("read {}: {error}", directory.display())),
            }
        }
        paths.sort();
        for path in paths {
            let meta = match fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(error) => {
                    self.report
                        .errors
                        .push(format!("stat {}: {error}", path.display()));
                    continue;
                }
            };
            let name = path.file_name().unwrap_or_default();
            let kind = meta.file_type();
            if kind.is_symlink() {
                self.report.skipped_symlinks += 1;
            } else if kind.is_dir() {
                if name == crate::TRANSCRIPT_DIRECTORY {
                    self.report.skipped_transcripts += 1;
                } else {
                    self.walk(&path);
                }
            } else if !kind.is_file() {
                self.report.skipped_other += 1;
            } else if is_jsonl(name) {
                self.report.skipped_transcripts += 1;
            } else if meta.size() < self.config.min_size {
                self.report.skipped_small += 1;
            } else {
                self.report.eligible += 1;
                match self.file(&path, &meta) {
                    Ok(Ok(())) => {}
                    Ok(Err(Skip::Busy)) => self.report.skipped_busy += 1,
                    Ok(Err(Skip::Other)) => self.report.skipped_other += 1,
                    Err(error) => self
                        .report
                        .errors
                        .push(format!("{}: {error}", path.display())),
                }
            }
        }
    }

    fn file(&mut self, path: &Path, before: &fs::Metadata) -> io::Result<Result<(), Skip>> {
        if before.nlink() > 1 || before.uid() != self.owner {
            return Ok(Err(Skip::Other));
        }
        if self
            .known
            .get(&(before.dev(), before.ino()))
            .is_some_and(|entry| entry.matches(before))
        {
            self.report.already_shared += 1;
            return Ok(Ok(()));
        }
        if young(before, self.config.min_age) {
            return Ok(Err(Skip::Busy));
        }
        // Gone, or swapped for a symlink (`ELOOP`) since it was listed: left
        // alone. Any other failure (`EACCES`, `EIO`) is an error.
        let mut file = match open_nofollow(path) {
            Ok(file) => file,
            Err(error) if moved(&error) => return Ok(Err(Skip::Busy)),
            Err(error) => return Err(error),
        };
        // Swapped for something else (a FIFO opens without blocking).
        let opened = file.metadata()?;
        if !opened.is_file() || !same_stamp(before, &opened) {
            return Ok(Err(Skip::Busy));
        }
        // Extended attributes (an ACL among them) would not follow the
        // clone that replaces the file.
        if has_xattrs(&file)? {
            return Ok(Err(Skip::Other));
        }
        // A writer that holds an flock keeps the file. Ours is held until
        // the replacement is done.
        match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(rustix::io::Errno::WOULDBLOCK) => return Ok(Err(Skip::Busy)),
            Err(error) => return Err(error.into()),
        }
        let hash = sha256_reader(&mut file)?;
        if !still(path, before)? {
            return Ok(Err(Skip::Busy));
        }
        let digest = sha256_digest(&hash)?;
        let blob = self.store.object_path(&digest);
        let source = match self.store.open_object(&digest) {
            Ok(source) => source,
            Err(error) if error.is_not_found() => {
                match self.seed(path, &file, before, &digest)? {
                    Seeded::Created => return Ok(Ok(())),
                    Seeded::Changed => return Ok(Err(Skip::Busy)),
                    // Someone else's blob: this file is a duplicate of it.
                    Seeded::Lost => self.store.open_object(&digest)?,
                }
            }
            Err(error) => {
                return Err(io::Error::other(format!(
                    "blob {} cannot be opened as a file: {error}",
                    blob.display()
                )));
            }
        };
        // A blob being published has a second link (the publisher's
        // temporary) for a moment; one that keeps it is left alone.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut blob_meta = source.metadata()?;
        while blob_meta.nlink() == 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
            blob_meta = source.metadata()?;
        }
        if blob_meta.is_file() && blob_meta.nlink() == 2 {
            return Ok(Err(Skip::Busy));
        }
        if !blob_meta.is_file() || blob_meta.nlink() != 1 || blob_meta.len() != before.size() {
            return Err(io::Error::other(format!(
                "blob {} is not a plain single-link file of {} bytes: refused",
                blob.display(),
                before.size()
            )));
        }
        // Clone the blob beside the file, check it holds the content its
        // name says, give it the file's mode and times, and rename it over
        // the file if nothing moved meanwhile.
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let staged = path.with_file_name(format!(".{name}.cas-{}", unique_suffix()));
        let mut clone = reflink_new(&source, &staged)?;
        let finish = (|| -> io::Result<Result<(), Skip>> {
            holds_its_content(&mut clone, &digest, &blob)?;
            take_owner(&clone, before, path)?;
            clone.set_permissions(fs::Permissions::from_mode(before.mode() & 0o7777))?;
            clone.set_times(
                fs::FileTimes::new()
                    .set_accessed(before.accessed()?)
                    .set_modified(before.modified()?),
            )?;
            if !still(path, before)? {
                return Ok(Err(Skip::Busy));
            }
            fs::rename(&staged, path)?;
            Ok(Ok(()))
        })();
        match finish {
            Ok(Ok(())) => {}
            other => return with_cleanup(other, &staged),
        }
        self.report.deduplicated += 1;
        self.report.reclaimed_bytes += before.size();
        let after = fs::symlink_metadata(path)?;
        self.added.push(IndexEntry::new(&hash, path, &after));
        Ok(Ok(()))
    }

    /// The first file of its content: the blob is cloned from it (so the
    /// file already shares the blob's extents), verified, and published
    /// through the store without replacing one a concurrent pass published
    /// first ([`Store::publish`]).
    fn seed(
        &mut self,
        path: &Path,
        file: &fs::File,
        before: &fs::Metadata,
        digest: &spawn_cas::Digest,
    ) -> io::Result<Seeded> {
        let staged = self
            .store
            .tmp_dir()
            .join(format!("{}.{}", digest.hex(), unique_suffix()));
        let mut clone = reflink_new(file, &staged)?;
        let published = (|| -> io::Result<Seeded> {
            // The clone is what was hashed, or the file changed under us.
            let verified = match verify_reader(&mut clone, digest) {
                Ok(verified) => verified,
                Err(CasError::Mismatch { .. }) => return Ok(Seeded::Changed),
                Err(error) => return Err(error.into()),
            };
            if !still(path, before)? {
                return Ok(Seeded::Changed);
            }
            // A hard link never replaces: a blob published meanwhile wins.
            match self.store.publish(&staged, &clone, &verified)? {
                Publish::Created => Ok(Seeded::Created),
                Publish::Exists => Ok(Seeded::Lost),
            }
        })();
        let published = with_cleanup(published, &staged)?;
        if published == Seeded::Created {
            self.report.blobs_created += 1;
            self.added.push(IndexEntry::new(digest.hex(), path, before));
        }
        Ok(published)
    }
}

/// Refuse a clone of `blob` that does not hash to `digest`: a tampered or
/// planted blob is never copied over a file.
fn holds_its_content(
    clone: &mut fs::File,
    digest: &spawn_cas::Digest,
    blob: &Path,
) -> io::Result<()> {
    match verify_reader(clone, digest) {
        Ok(_) => Ok(()),
        Err(CasError::Mismatch { .. }) => Err(io::Error::other(format!(
            "blob {} does not hold the content it is named for: refused",
            blob.display()
        ))),
        Err(error) => Err(error.into()),
    }
}

/// Drop the index entries whose file is gone or no longer the clone it was
/// made, then remove the blobs no live entry references and that are older
/// than `grace` (a concurrent pass may have published one and not yet
/// written its entry), and temporaries older than `grace`.
///
/// Refused (`InvalidInput`) unless `cas_root` carries [`CAS_MARKER`]. Legacy
/// blobs (`<hh>/<hash>`) are adopted into `objects/sha256/` first, without
/// rehashing. Only names this module makes are ever removed:
/// `objects/sha256/<hh>/<hash>` with a 64-digit lowercase hex hash in its
/// own shard, never one a `spawn-cas` pin names or a `spawn-cas` namespace
/// holds an access record of (`access/<ns>/sha256/<hash>`: that object is
/// the namespace's, collected by its own GC), and `tmp/<hash>.<pid>-<n>`;
/// anything else (a `*.jsonl` file included) is left where it is.
///
/// Safe at any time: a workspace file's data is refcounted by the file
/// system, never by the blob.
pub fn prune_unused(cas_root: &Path, grace: Duration) -> io::Result<PruneReport> {
    let mut report = PruneReport::default();
    if !fs::symlink_metadata(cas_root.join(CAS_MARKER)).is_ok_and(|meta| meta.is_file()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{} is not a CAS root (no {CAS_MARKER}): nothing pruned",
                cas_root.display()
            ),
        ));
    }
    let store = open_store(cas_root).map_err(|error| match error {
        DedupError::Io(error) => error,
        other => io::Error::new(io::ErrorKind::InvalidInput, other.to_string()),
    })?;
    // No pass is between a blob and its index entry while this runs.
    let _publishing = IndexLock::take(cas_root, PUBLISH_LOCK, true)?;
    let _lock = IndexLock::take(cas_root, INDEX_LOCK, true)?;
    let mut live = Vec::new();
    for entry in read_index(cas_root)? {
        match fs::symlink_metadata(&entry.path) {
            Ok(meta) if entry.matches(&meta) => live.push(entry),
            // Gone or rewritten: the entry no longer holds its blob.
            Ok(_) => report.dropped_entries += 1,
            Err(error) if error.kind() == io::ErrorKind::NotFound => report.dropped_entries += 1,
            // Cannot tell: kept (and its blob with it), and said.
            Err(error) => {
                report
                    .errors
                    .push(format!("stat {}: {error}", entry.path.display()));
                live.push(entry);
            }
        }
    }
    report.live_entries = live.len();
    let referenced: BTreeSet<&str> = live.iter().map(|entry| entry.hash.as_str()).collect();
    publish_index(cas_root, &live)?;

    let old_enough = |meta: &fs::Metadata| {
        let changed = SystemTime::UNIX_EPOCH + Duration::new(meta.ctime().max(0).unsigned_abs(), 0);
        SystemTime::now()
            .duration_since(changed)
            .is_ok_and(|age| age >= grace)
    };
    for temporary in fs::read_dir(store.tmp_dir())? {
        let temporary = temporary?;
        let name = temporary.file_name().to_string_lossy().into_owned();
        let removable = fs::symlink_metadata(temporary.path())
            .is_ok_and(|meta| meta.is_file() && old_enough(&meta));
        if is_temporary(&name) && removable {
            match fs::remove_file(temporary.path()) {
                Ok(()) => report.temporaries_removed += 1,
                Err(error) => report
                    .errors
                    .push(format!("remove {}: {error}", temporary.path().display())),
            }
        }
    }
    // Pins and access records are read, and blobs removed, with no pin
    // written and no GC pass meanwhile.
    let _pins = store.lock_gc().map_err(io::Error::from)?;
    let pinned = store.pinned().map_err(io::Error::from)?;
    for object in store.objects(Algo::Sha256).map_err(io::Error::from)? {
        // An object some spawn-cas namespace records (`artifacts`,
        // `base-cache`, `backup`, `data`) is that namespace's to collect,
        // by its own LRU GC; it is never a dedup blob to prune. One whose
        // records cannot be read is kept, and said.
        let namespaced = match store.namespaces_of(&object.digest) {
            Ok(namespaces) => !namespaces.is_empty(),
            Err(error) => {
                report.errors.push(format!(
                    "access records of {}: {error}",
                    object.path.display()
                ));
                true
            }
        };
        if namespaced
            || referenced.contains(object.digest.hex())
            || pinned.contains(&object.digest)
            || !old_enough(&object.meta)
        {
            report.blobs_kept += 1;
            continue;
        }
        // The object file only: no access record is ever removed here.
        match store.remove_object_file(&object.digest) {
            Ok(_) => {
                report.blobs_removed += 1;
                report.bytes_removed += object.meta.size();
            }
            Err(error) => {
                report.blobs_kept += 1;
                report
                    .errors
                    .push(format!("remove {}: {error}", object.path.display()));
            }
        }
    }
    Ok(report)
}

/// Give the staged `clone` of `path` the file's group, and refuse it unless
/// it then has the file's owner and group and no extended attribute (no
/// default ACL of the directory): no one gains or loses access through the
/// replacement.
fn take_owner(clone: &fs::File, before: &fs::Metadata, path: &Path) -> io::Result<()> {
    if clone.metadata()?.gid() != before.gid() {
        rustix::fs::fchown(clone, None, Some(rustix::fs::Gid::from_raw(before.gid()))).map_err(
            |error| {
                io::Error::other(format!(
                    "the clone of {} cannot take its group {}: {error}",
                    path.display(),
                    before.gid()
                ))
            },
        )?;
    }
    let made = clone.metadata()?;
    if made.uid() != before.uid() || made.gid() != before.gid() || has_xattrs(clone)? {
        return Err(io::Error::other(format!(
            "the clone of {} would not keep its owner, group or attributes: refused",
            path.display()
        )));
    }
    Ok(())
}

/// Replace the index with `entries`: written whole beside it, synced, and
/// renamed over it (the caller holds `index.lock` exclusively).
fn publish_index(cas_root: &Path, entries: &[IndexEntry]) -> io::Result<()> {
    let staged = cas_root.join(format!("index.{}", unique_suffix()));
    let text: String = entries.iter().map(IndexEntry::render).collect();
    let written = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&staged)
        .and_then(|mut file| {
            file.write_all(text.as_bytes())?;
            file.sync_all()
        })
        .and_then(|()| fs::rename(&staged, cas_root.join("index")));
    with_cleanup(written, &staged)
}

/// `outcome`, after `staged` (a clone of this pass) is removed; a removal
/// that fails is part of the error, never dropped.
fn with_cleanup<T>(outcome: io::Result<T>, staged: &Path) -> io::Result<T> {
    let removed = match fs::remove_file(staged) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    };
    match (outcome, removed) {
        (outcome, Ok(())) => outcome,
        (Ok(_), Err(error)) => Err(io::Error::other(format!(
            "remove {}: {error}",
            staged.display()
        ))),
        (Err(error), Err(also)) => Err(io::Error::other(format!(
            "{error}; and remove {}: {also}",
            staged.display()
        ))),
    }
}

/// Whether the open file carries extended attributes; a file system
/// without them answers no.
fn has_xattrs(file: &fs::File) -> io::Result<bool> {
    use rustix::io::Errno;
    match rustix::fs::flistxattr_query(file) {
        Ok(length) => Ok(length > 0),
        Err(errno) if [Errno::NOTSUP, Errno::OPNOTSUPP].contains(&errno) => Ok(false),
        Err(errno) => Err(errno.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_entries_round_trip() {
        let entry = IndexEntry {
            hash: "ab".repeat(32),
            dev: 1,
            ino: 2,
            size: 3,
            mtime: (4, 5),
            ctime: (6, 7),
            path: PathBuf::from(std::ffi::OsStr::from_bytes(b"/w/t a\n\xff")),
        };
        let line = entry.render();
        assert_eq!(IndexEntry::parse(line.trim_end_matches('\n')), Some(entry));
    }

    #[test]
    fn sha256_matches_the_known_digest() {
        let digest = sha256_reader(&mut &b"abc"[..]).expect("hash");
        assert_eq!(
            digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn only_names_this_module_makes_are_prunable() {
        let hash = "0f".repeat(32);
        assert!(is_hash(&hash));
        assert!(!is_hash(&"0F".repeat(32)));
        assert!(!is_hash("session.jsonl"));
        assert!(is_temporary(&format!("{hash}.123-4")));
        for name in [
            "notes.txt",
            "x.jsonl",
            &format!("{hash}.jsonl"),
            &format!("{hash}.1-"),
            &format!("{hash}.a-1"),
        ] {
            assert!(!is_temporary(name), "{name}");
        }
    }

    #[test]
    fn a_fifo_never_blocks_the_open() {
        let scratch = crate::scratch::Scratch::new("cas-fifo");
        let fifo = scratch.join("pipe");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .expect("mkfifo")
                .success()
        );
        let (done, finished) = std::sync::mpsc::channel();
        let path = fifo.clone();
        std::thread::spawn(move || {
            let opened = open_nofollow(&path).and_then(|file| file.metadata());
            let _ = done.send(opened.map(|meta| meta.is_file()).ok());
        });
        let answer = finished
            .recv_timeout(Duration::from_secs(5))
            .expect("the open returns at once");
        assert_ne!(answer, Some(true), "a FIFO is never taken for a file");
    }

    #[test]
    fn a_prune_waits_for_a_pass_that_is_publishing() {
        let scratch = crate::scratch::Scratch::new("cas-publish");
        let root = scratch.join("cas");
        let shard = root.join("objects/sha256/ab");
        fs::create_dir_all(&shard).expect("mkdir");
        fs::write(root.join(CAS_MARKER), "spawn-cas v1\n").expect("marker");
        let hash = format!("ab{}", "0".repeat(62));
        fs::write(shard.join(&hash), "blob").expect("write");
        // A pass between its blob and its index entry.
        let publishing = IndexLock::take(&root, PUBLISH_LOCK, false).expect("lock");
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let _ = done.send(prune_unused(&root, Duration::ZERO).map(|r| r.blobs_removed));
            });
            assert!(
                finished.recv_timeout(Duration::from_millis(300)).is_err(),
                "the prune waits"
            );
            assert!(shard.join(&hash).exists());
            drop(publishing);
            let removed = finished
                .recv_timeout(Duration::from_secs(10))
                .expect("the prune goes on")
                .expect("prune");
            assert_eq!(removed, 1);
        });
    }

    #[test]
    fn dedup_dirs_probes_cow_before_creating_marker_and_still_refuses_foreign_root() {
        let scratch = crate::scratch::Scratch::new("cas-cow-probe-order");

        let foreign = scratch.join("foreign");
        fs::create_dir_all(&foreign).expect("mkdir foreign");
        fs::write(foreign.join("note.txt"), b"keep me").expect("write foreign file");
        let err = dedup_dirs(&DedupConfig::new(&foreign), &[])
            .expect_err("non-empty dir without CAS marker must return NotCas");
        assert!(matches!(err, DedupError::NotCas(_)), "{err:?}");
        assert!(!foreign.join(CAS_MARKER).exists());

        let uncreated_cas = scratch.join("uncreated/.cas");
        let report = dedup_dirs(&DedupConfig::new(&uncreated_cas), &[]).expect("dedup_dirs");
        if !cow_clone_supported(scratch.path()) {
            assert!(!report.cow_supported);
            assert!(
                !uncreated_cas.exists(),
                "non-CoW filesystems must not create .cas directory eagerly"
            );
            assert!(!uncreated_cas.join(CAS_MARKER).exists());
        }
    }
}
