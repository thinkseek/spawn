//! Chunk store adapter and the large-file manifest (docs/storage.md).
//!
//! A large file is cut into content-defined chunks (a gear-hash FastCDC), each
//! named by the BLAKE3 hash of its bytes and stored zstd-compressed in a local
//! directory. The manifest of a workspace's large files (path, mode, size,
//! chunk list) is itself an object in the store; a checkpoint commit names it
//! in a `Spawn-Large: <hash>` trailer. Because cut points follow the content,
//! an in-place edit changes only the chunks it touches, and identical chunks
//! are stored once.

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use spawn_core::port::{ChunkError, ChunkStore, StoredObject};

use crate::tree_path::TreePath;

/// Smallest chunk the chunker cuts, except at the end of a file.
pub const MIN_CHUNK: usize = 128 << 10;
/// Size the chunker aims for.
pub const AVG_CHUNK: usize = 512 << 10;
/// Largest chunk the chunker cuts.
pub const MAX_CHUNK: usize = 2 << 20;

/// Trailer key that names the large-file manifest in a checkpoint commit.
pub const TRAILER: &str = "Spawn-Large";

/// Largest object a read decompresses, against a hostile store.
const MAX_OBJECT: u64 = 256 << 20;

/// zstd level: fast, and incompressible chunks fall back to raw blocks.
const ZSTD_LEVEL: i32 = 3;

/// One step of splitmix64, for the gear table.
const fn splitmix(state: u64) -> (u64, u64) {
    let state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    (state, z ^ (z >> 31))
}

/// Random value per byte, fixed forever: changing it moves every cut point.
const GEAR: [u64; 256] = {
    let mut table = [0u64; 256];
    let mut state = 0x5350_4157_4e00_0001_u64;
    let mut index = 0;
    while index < 256 {
        let (next, value) = splitmix(state);
        state = next;
        table[index] = value;
        index += 1;
    }
    table
};

/// Mask with the `bits` highest bits set: the gear hash's high bits depend on
/// the last 64 bytes, its low bits only on the last few.
const fn high_bits(bits: u32) -> u64 {
    !0u64 << (64 - bits)
}

/// Normalized chunking (FastCDC): a harder mask before the average size and
/// an easier one after it pull chunk sizes towards the average.
const MASK_HARD: u64 = high_bits(AVG_CHUNK.trailing_zeros() + 2);
const MASK_EASY: u64 = high_bits(AVG_CHUNK.trailing_zeros() - 2);

/// Length of the first chunk of `data`, which holds at most one chunk's worth
/// of the stream. `data` shorter than [`MAX_CHUNK`] is taken as the stream's
/// end, so it is cut whole when no cut point is found.
pub fn cut(data: &[u8]) -> usize {
    if data.len() <= MIN_CHUNK {
        return data.len();
    }
    let end = data.len().min(MAX_CHUNK);
    let middle = AVG_CHUNK.min(end);
    let mut hash = 0u64;
    let mut index = MIN_CHUNK;
    while index < middle {
        hash = (hash << 1).wrapping_add(GEAR[usize::from(data[index])]);
        if hash & MASK_HARD == 0 {
            return index + 1;
        }
        index += 1;
    }
    while index < end {
        hash = (hash << 1).wrapping_add(GEAR[usize::from(data[index])]);
        if hash & MASK_EASY == 0 {
            return index + 1;
        }
        index += 1;
    }
    end
}

/// Cut a stream into chunks, calling `each` with every chunk in order.
pub fn chunk_stream<E>(
    mut reader: impl Read,
    mut each: impl FnMut(&[u8]) -> Result<(), E>,
    io_error: impl Fn(std::io::Error) -> E,
) -> Result<(), E> {
    let mut buffer = vec![0u8; 2 * MAX_CHUNK];
    let mut filled = 0;
    let mut ended = false;
    loop {
        while !ended && filled < MAX_CHUNK {
            match reader.read(&mut buffer[filled..]) {
                Ok(0) => ended = true,
                Ok(read) => filled += read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(io_error(error)),
            }
        }
        if filled == 0 {
            return Ok(());
        }
        let length = cut(&buffer[..filled.min(MAX_CHUNK)]);
        each(&buffer[..length])?;
        buffer.copy_within(length..filled, 0);
        filled -= length;
    }
}

/// BLAKE3 hash of `bytes`, as lowercase hex: an object's name.
pub fn hash_of(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Whether `name` is an object name: 64 lowercase hex digits. Anything else
/// never reaches a path.
pub fn is_name(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Chunk store in a local directory: `<dir>/<first two hex digits>/<name>`,
/// each object zstd-compressed.
#[derive(Debug, Clone)]
pub struct LocalChunkStore {
    directory: PathBuf,
    /// Objects this process found sound: each is read back once, not on
    /// every `put` of the same content.
    verified: Arc<Mutex<HashSet<String>>>,
}

impl LocalChunkStore {
    /// Store rooted at `directory`, created on the first write.
    pub fn new(directory: impl AsRef<Path>) -> Self {
        Self {
            directory: directory.as_ref().to_path_buf(),
            verified: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Where the store lives.
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    fn path_of(&self, name: &str) -> PathBuf {
        self.directory.join(&name[..2]).join(name)
    }
}

fn unavailable(what: &str, path: &Path, error: &std::io::Error) -> ChunkError {
    ChunkError::Unavailable(format!("{what} {}: {error}", path.display()))
}

/// Distinguishes temporary files of concurrent writers in one process.
static TEMPORARY: AtomicU64 = AtomicU64::new(0);

impl LocalChunkStore {
    /// Whether the object at `path` decompresses to bytes named `name`.
    fn verifies(path: &Path, name: &str) -> bool {
        let Ok(file) = File::open(path) else {
            return false;
        };
        let Ok(decoder) = zstd::stream::read::Decoder::new(file) else {
            return false;
        };
        let mut hasher = blake3::Hasher::new();
        let copied = std::io::copy(&mut decoder.take(MAX_OBJECT + 1), &mut hasher);
        copied.is_ok_and(|length| length <= MAX_OBJECT)
            && hasher.finalize().to_hex().as_str() == name
    }

    /// Whether the object `name` exists and was found sound, by this process
    /// or now.
    fn sound(&self, name: &str, path: &Path) -> bool {
        let known = self
            .verified
            .lock()
            .is_ok_and(|verified| verified.contains(name));
        if known && path.is_file() {
            return true;
        }
        if !Self::verifies(path, name) {
            return false;
        }
        if let Ok(mut verified) = self.verified.lock() {
            verified.insert(name.to_string());
        }
        true
    }
}

/// Set the object file's modification time to now: a collection measures an
/// object's age from it. Owning the file is enough; it is opened read-only.
fn refresh(path: &Path) -> std::io::Result<()> {
    File::open(path)?.set_modified(std::time::SystemTime::now())
}

/// Age of a file from its modification time; a time in the future is age 0.
fn age_of(metadata: &std::fs::Metadata) -> std::time::Duration {
    metadata
        .modified()
        .ok()
        .and_then(|modified| std::time::SystemTime::now().duration_since(modified).ok())
        .unwrap_or_default()
}

/// Fsync a directory, so the entries just made in it survive a crash.
fn sync_directory(directory: &Path) -> std::io::Result<()> {
    File::open(directory)?.sync_all()
}

impl ChunkStore for LocalChunkStore {
    /// Store `bytes`. An object already there is verified first: a sound one
    /// is never rewritten, a corrupt one (a torn write, a disk error, a copy
    /// that died) is replaced atomically by a rename over it, which repairs
    /// every manifest that names it. File and directories are fsynced.
    fn put(&self, bytes: &[u8]) -> Result<String, ChunkError> {
        let name = hash_of(bytes);
        let path = self.path_of(&name);
        let exists = std::fs::symlink_metadata(&path).is_ok();
        if exists && self.sound(&name, &path) {
            // In use again: a collection must not take it before the commit
            // that names it.
            refresh(&path).map_err(|error| unavailable("refresh", &path, &error))?;
            return Ok(name);
        }
        let parent = path
            .parent()
            .ok_or_else(|| ChunkError::Unavailable(format!("{} has no parent", path.display())))?;
        if !parent.is_dir() {
            std::fs::create_dir_all(parent)
                .map_err(|error| unavailable("create", parent, &error))?;
            // The fan-out directory's own entry, and the root's, if new.
            sync_directory(&self.directory)
                .map_err(|error| unavailable("sync", &self.directory, &error))?;
            if let Some(above) = self
                .directory
                .parent()
                .filter(|above| !above.as_os_str().is_empty())
            {
                let _ = sync_directory(above);
            }
        }
        let compressed = zstd::bulk::compress(bytes, ZSTD_LEVEL)
            .map_err(|error| ChunkError::Unavailable(format!("compress {name}: {error}")))?;
        let temporary = parent.join(format!(
            ".{name}.tmp-{}-{}",
            std::process::id(),
            TEMPORARY.fetch_add(1, Ordering::Relaxed)
        ));
        let written = (|| {
            let mut file = File::create(&temporary)?;
            file.write_all(&compressed)?;
            file.sync_all()
        })();
        if let Err(error) = written {
            let _ = std::fs::remove_file(&temporary);
            return Err(unavailable("write", &temporary, &error));
        }
        let published = if exists {
            // A corrupt object: the rename replaces it in one step. A writer
            // racing this one renames the same bytes.
            std::fs::rename(&temporary, &path)
        } else {
            // A hard link never replaces an existing object; a writer that
            // lost the race wrote the same bytes, so its copy is dropped.
            let linked = match std::fs::hard_link(&temporary, &path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
                Err(_) if path.exists() => Ok(()),
                // A file system without hard links: rename is still atomic.
                Err(_) => std::fs::rename(&temporary, &path),
            };
            let _ = std::fs::remove_file(&temporary);
            linked
        };
        if published.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        published.map_err(|error| unavailable("publish", &path, &error))?;
        sync_directory(parent).map_err(|error| unavailable("sync", parent, &error))?;
        Ok(name)
    }

    fn get(&self, name: &str) -> Result<Vec<u8>, ChunkError> {
        if !is_name(name) {
            return Err(ChunkError::Corrupt(name.to_string()));
        }
        let path = self.path_of(name);
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(ChunkError::Missing(name.to_string()));
            }
            Err(error) => return Err(unavailable("open", &path, &error)),
        };
        let decoder = zstd::stream::read::Decoder::new(file)
            .map_err(|error| unavailable("decode", &path, &error))?;
        let mut bytes = Vec::new();
        decoder
            .take(MAX_OBJECT + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ChunkError::Corrupt(name.to_string()))?;
        if hash_of(&bytes) != name {
            return Err(ChunkError::Corrupt(name.to_string()));
        }
        Ok(bytes)
    }

    fn contains(&self, name: &str) -> Result<bool, ChunkError> {
        Ok(is_name(name) && self.path_of(name).is_file())
    }

    fn touch(&self, name: &str) -> Result<bool, ChunkError> {
        if !is_name(name) {
            return Ok(false);
        }
        let path = self.path_of(name);
        match refresh(&path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(unavailable("refresh", &path, &error)),
        }
    }

    /// Every file named by 64 hex digits in the fan-out directory of its
    /// first two; temporary files and anything else are not objects.
    fn list(&self) -> Result<Vec<StoredObject>, ChunkError> {
        let fan_outs = match std::fs::read_dir(&self.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(unavailable("list", &self.directory, &error)),
        };
        let mut objects = Vec::new();
        for fan_out in fan_outs {
            let fan_out = fan_out.map_err(|error| unavailable("list", &self.directory, &error))?;
            let prefix = fan_out.file_name().to_string_lossy().to_string();
            let is_fan_out = prefix.len() == 2
                && prefix
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
            let path = fan_out.path();
            if !is_fan_out || !path.is_dir() {
                continue;
            }
            let entries =
                std::fs::read_dir(&path).map_err(|error| unavailable("list", &path, &error))?;
            for entry in entries {
                let entry = entry.map_err(|error| unavailable("list", &path, &error))?;
                let name = entry.file_name().to_string_lossy().to_string();
                if !is_name(&name) || !name.starts_with(&prefix) {
                    continue;
                }
                let metadata = match entry.metadata() {
                    Ok(metadata) if metadata.is_file() => metadata,
                    Ok(_) => continue,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(unavailable("stat", &entry.path(), &error)),
                };
                objects.push(StoredObject {
                    name,
                    age: age_of(&metadata),
                    bytes: metadata.len(),
                });
            }
        }
        objects.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(objects)
    }

    fn remove_if_older(
        &self,
        name: &str,
        older_than: std::time::Duration,
    ) -> Result<Option<u64>, ChunkError> {
        if !is_name(name) {
            return Ok(None);
        }
        let path = self.path_of(name);
        // The time is read again here, just before the removal: a put since
        // the listing made the object young, and it stays.
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(unavailable("stat", &path, &error)),
        };
        if age_of(&metadata) < older_than {
            return Ok(None);
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(unavailable("remove", &path, &error)),
        }
        if let Ok(mut verified) = self.verified.lock() {
            verified.remove(name);
        }
        Ok(Some(metadata.len()))
    }
}

/// One large file in a manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LargeFile {
    /// Path relative to the workspace, `/`-separated, as the bytes of its
    /// name: not always UTF-8, and it may hold a newline.
    pub path: TreePath,
    /// git file mode: `0o100644` or `0o100755`.
    pub mode: u32,
    /// Size in bytes.
    pub size: u64,
    /// Chunk names, in order.
    pub chunks: Vec<String>,
}

/// The large files of one checkpoint, sorted by path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    /// Files, sorted by path.
    pub files: Vec<LargeFile>,
}

impl Manifest {
    /// Canonical bytes: the same files always give the same object.
    pub fn to_bytes(&self) -> Vec<u8> {
        let files: Vec<Value> = self
            .files
            .iter()
            .map(|file| {
                let mut entry = json!({"mode": file.mode, "size": file.size,
                                       "chunks": file.chunks});
                put_path(&mut entry, &file.path);
                entry
            })
            .collect();
        serde_json::to_vec(&json!({"version": 1, "files": files})).unwrap_or_default()
    }

    /// Parse and check a manifest: safe relative paths, object names, known
    /// modes. Anything else is refused.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let value: Value =
            serde_json::from_slice(bytes).map_err(|error| format!("manifest: {error}"))?;
        if value.get("version").and_then(Value::as_u64) != Some(1) {
            return Err("manifest: unknown version".to_string());
        }
        let listed = value
            .get("files")
            .and_then(Value::as_array)
            .ok_or("manifest: no files")?;
        let mut files = Vec::with_capacity(listed.len());
        for entry in listed {
            let path = path_of(entry)
                .filter(TreePath::is_safe)
                .ok_or("manifest: bad path")?;
            let mode = entry
                .get("mode")
                .and_then(Value::as_u64)
                .and_then(|mode| u32::try_from(mode).ok())
                .filter(|mode| [0o100_644, 0o100_755].contains(mode))
                .ok_or("manifest: bad mode")?;
            let size = entry
                .get("size")
                .and_then(Value::as_u64)
                .ok_or("manifest: bad size")?;
            let chunks = entry
                .get("chunks")
                .and_then(Value::as_array)
                .ok_or("manifest: no chunks")?
                .iter()
                .map(|chunk| {
                    chunk
                        .as_str()
                        .filter(|name| is_name(name))
                        .map(str::to_string)
                        .ok_or_else(|| "manifest: bad chunk name".to_string())
                })
                .collect::<Result<Vec<_>, _>>()?;
            files.push(LargeFile {
                path,
                mode,
                size,
                chunks,
            });
        }
        Ok(Self { files })
    }
}

/// Write `path` into a JSON `entry` so that any bytes round-trip: a UTF-8
/// path as `"path"` (JSON escapes a newline or a control character), any
/// other as `"path_hex"`, the bytes in lowercase hex. One path has one form,
/// so the same files always give the same manifest.
pub fn put_path(entry: &mut Value, path: &TreePath) {
    let Some(object) = entry.as_object_mut() else {
        return;
    };
    match path.as_str() {
        Some(text) => object.insert("path".to_string(), json!(text)),
        None => object.insert("path_hex".to_string(), json!(hex(path.as_bytes()))),
    };
}

/// The path [`put_path`] wrote into `entry`.
pub fn path_of(entry: &Value) -> Option<TreePath> {
    match (entry.get("path"), entry.get("path_hex")) {
        (Some(path), None) => path.as_str().map(TreePath::from),
        (None, Some(path)) => {
            let bytes = unhex(path.as_str()?)?;
            // A UTF-8 path has the other form.
            std::str::from_utf8(&bytes)
                .is_err()
                .then(|| TreePath::from_bytes(bytes))
        }
        _ => None,
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|byte| {
            [
                DIGITS[usize::from(byte >> 4)],
                DIGITS[usize::from(byte & 15)],
            ]
        })
        .map(char::from)
        .collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| {
            let pair = text.get(at..at + 2)?;
            pair.bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                .then(|| u8::from_str_radix(pair, 16).ok())?
        })
        .collect()
}

/// The commit message with the manifest named in a trailer, or unchanged
/// when there are no large files.
pub fn with_trailer(message: &str, manifest: Option<&str>) -> String {
    match manifest {
        Some(name) => format!("{}\n\n{TRAILER}: {name}\n", message.trim_end()),
        None => message.to_string(),
    }
}

/// The manifest a commit message names, if any: the last `Spawn-Large`
/// line of its last paragraph.
pub fn trailer_of(message: &str) -> Option<String> {
    let paragraph = message.trim_end().rsplit("\n\n").next()?;
    paragraph
        .lines()
        .filter_map(|line| line.strip_prefix(TRAILER)?.strip_prefix(':'))
        .map(str::trim)
        .filter(|name| is_name(name))
        .map(str::to_string)
        .next_back()
}

/// Chunk the file at `path` into `store`; the chunk names in order and the
/// number of bytes read.
pub fn store_file(store: &dyn ChunkStore, path: &Path) -> Result<(Vec<String>, u64), ChunkError> {
    let file = File::open(path).map_err(|error| unavailable("open", path, &error))?;
    let mut chunks = Vec::new();
    let mut size = 0u64;
    chunk_stream(
        file,
        |chunk| {
            size += chunk.len() as u64;
            chunks.push(store.put(chunk)?);
            Ok(())
        },
        |error| unavailable("read", path, &error),
    )?;
    Ok((chunks, size))
}

/// A directory held open, reached without following any symlink, and the
/// path that names its entries: through the open descriptor
/// (`/proc/self/fd/<n>/<entry>`) where /proc shows it, so a component
/// swapped for a symlink after it was opened is never followed.
struct Held {
    file: File,
    path: PathBuf,
}

impl Held {
    /// Open the directory `path` itself, refusing a symlink or anything that
    /// is not a directory.
    fn open(path: PathBuf) -> std::io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)?;
        Ok(Self { file, path })
    }

    /// The path of `name` inside this directory.
    fn entry(&self, name: &std::ffi::OsStr) -> PathBuf {
        use std::os::fd::AsRawFd;
        let through = PathBuf::from(format!("/proc/self/fd/{}", self.file.as_raw_fd()));
        if through.is_dir() {
            through.join(name)
        } else {
            self.path.join(name)
        }
    }

    /// The child directory `name`, made when missing. A symlink, a file or
    /// anything else that is not a directory is refused.
    fn child(&self, name: &std::ffi::OsStr) -> Result<Self, String> {
        let entry = self.entry(name);
        let shown = self.path.join(name);
        let refused = |error: std::io::Error| {
            format!(
                "{}: not a directory of the workspace (a symlink or a file is in the way): {error}",
                shown.display()
            )
        };
        match std::fs::symlink_metadata(&entry) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // mkdir never follows a symlink in its last component.
                match std::fs::create_dir(&entry) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(format!("create {}: {error}", shown.display())),
                }
            }
            Err(error) => return Err(format!("stat {}: {error}", shown.display())),
        }
        let held = Self::open(entry).map_err(refused)?;
        Ok(Self {
            path: shown,
            ..held
        })
    }
}

/// Write `file` under `directory` from `store`, atomically and inside the
/// workspace only: every chunk is verified and the size checked into a
/// temporary file beside the target, which is renamed into place only when
/// all of it is right. On any error the target is left untouched.
///
/// The parent directories are walked one component at a time, each opened
/// with `O_NOFOLLOW` and made with `mkdir` when missing, so a symlink (or a
/// file) where the manifest has a directory fails the write instead of
/// leading it out of the workspace. The temporary file is created
/// exclusively and without following a symlink, and renamed inside the
/// directory that was verified.
pub fn materialize(
    store: &dyn ChunkStore,
    directory: &Path,
    file: &LargeFile,
) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    if !file.path.is_safe() {
        return Err(format!("unsafe path {}", file.path));
    }
    let relative = file.path.as_path();
    let name = relative
        .file_name()
        .ok_or_else(|| format!("{} has no file name", file.path))?;
    let mut parent = Held::open(directory.to_path_buf())
        .map_err(|error| format!("open {}: {error}", directory.display()))?;
    if let Some(parents) = relative.parent() {
        for component in parents.components() {
            parent = parent.child(component.as_os_str())?;
        }
    }
    let temporary_name = format!(
        ".{}.spawn-{}-{}",
        name.to_string_lossy(),
        std::process::id(),
        TEMPORARY.fetch_add(1, Ordering::Relaxed)
    );
    let temporary = parent.entry(std::ffi::OsStr::new(&temporary_name));
    let shown = parent.path.join(&temporary_name);
    let target = parent.entry(name);
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&temporary)
        .map_err(|error| format!("create {}: {error}", shown.display()))?;
    let written = (|| -> Result<(), String> {
        let mut size = 0u64;
        for chunk in &file.chunks {
            let bytes = store.get(chunk).map_err(|error| error.to_string())?;
            size += bytes.len() as u64;
            output
                .write_all(&bytes)
                .map_err(|error| format!("write {}: {error}", shown.display()))?;
        }
        if size != file.size {
            return Err(format!(
                "{}: chunks hold {size} bytes, the manifest says {}",
                file.path, file.size
            ));
        }
        set_mode(&output, file.mode)
            .map_err(|error| format!("chmod {}: {error}", shown.display()))?;
        output
            .sync_all()
            .map_err(|error| format!("sync {}: {error}", shown.display()))?;
        // rename replaces a symlink at the target, never what it points to.
        std::fs::rename(&temporary, &target)
            .map_err(|error| format!("rename to {}: {error}", parent.path.join(name).display()))?;
        let _ = parent.file.sync_all();
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

fn set_mode(file: &File, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(if mode == 0o100_755 {
        0o755
    } else {
        0o644
    }))
}

/// git mode of a file from its permissions.
pub fn mode_of(metadata: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o111 != 0 {
        0o100_755
    } else {
        0o100_644
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic incompressible bytes (xorshift).
    fn noise(seed: u64, length: usize) -> Vec<u8> {
        let mut state = seed | 1;
        (0..length)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state.to_le_bytes()[0]
            })
            .collect()
    }

    fn chunks_of(data: &[u8]) -> Vec<Vec<u8>> {
        let mut chunks = Vec::new();
        chunk_stream::<std::io::Error>(
            data,
            |chunk| {
                chunks.push(chunk.to_vec());
                Ok(())
            },
            |error| error,
        )
        .expect("chunk from memory");
        chunks
    }

    fn scratch(tag: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("spawn-chunks-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("create scratch directory");
        directory
    }

    #[test]
    fn chunks_cover_the_input_within_the_size_bounds() {
        let data = noise(1, 9 << 20);
        let chunks = chunks_of(&data);
        assert_eq!(chunks.concat(), data);
        let (last, rest) = chunks.split_last().expect("chunks");
        assert!(
            rest.iter()
                .all(|chunk| (MIN_CHUNK..=MAX_CHUNK).contains(&chunk.len()))
        );
        assert!(last.len() <= MAX_CHUNK);
        assert!(
            chunks.len() > 4,
            "about {} chunks of {AVG_CHUNK}",
            chunks.len()
        );
        assert!(chunks_of(&[]).is_empty());
        assert_eq!(chunks_of(b"tiny"), vec![b"tiny".to_vec()]);
    }

    #[test]
    fn an_in_place_edit_changes_only_nearby_chunks() {
        let data = noise(2, 12 << 20);
        let mut edited = data.clone();
        edited[5 << 20..6 << 20].copy_from_slice(&noise(3, 1 << 20));
        let before: std::collections::HashSet<_> = chunks_of(&data)
            .iter()
            .map(|chunk| hash_of(chunk))
            .collect();
        let new_bytes: usize = chunks_of(&edited)
            .iter()
            .filter(|chunk| !before.contains(&hash_of(chunk)))
            .map(Vec::len)
            .sum();
        assert!(
            new_bytes < 5 << 20,
            "{new_bytes} new bytes for a 1 MiB edit"
        );
        assert!(new_bytes >= 1 << 20);
    }

    #[test]
    fn the_store_round_trips_and_stores_identical_chunks_once() {
        let directory = scratch("roundtrip");
        let store = LocalChunkStore::new(&directory);
        let bytes = noise(4, 300_000);
        let name = store.put(&bytes).expect("put");
        assert_eq!(name, hash_of(&bytes));
        assert!(store.contains(&name).expect("contains"));
        assert_eq!(store.get(&name).expect("get"), bytes);
        assert_eq!(store.put(&bytes).expect("put again"), name);
        let objects: Vec<_> = walk(&directory);
        assert_eq!(objects.len(), 1, "{objects:?}");
        assert!(matches!(
            store.get(&hash_of(b"absent")),
            Err(ChunkError::Missing(_))
        ));
        assert!(matches!(store.get("../x"), Err(ChunkError::Corrupt(_))));
        let _ = std::fs::remove_dir_all(&directory);
    }

    fn walk(directory: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(directory).expect("read store").flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(walk(&path));
            } else {
                files.push(path);
            }
        }
        files
    }

    #[test]
    fn a_corrupted_chunk_is_refused_and_never_materialized() {
        let directory = scratch("corrupt");
        let store = LocalChunkStore::new(directory.join("store"));
        let bytes = noise(5, 200_000);
        let name = store.put(&bytes).expect("put");
        let file = LargeFile {
            path: "data/big.bin".into(),
            mode: 0o100_644,
            size: bytes.len() as u64,
            chunks: vec![name.clone()],
        };
        let workspace = directory.join("ws");
        std::fs::create_dir_all(&workspace).expect("workspace");
        materialize(&store, &workspace, &file).expect("materialize");
        assert_eq!(
            std::fs::read(workspace.join("data/big.bin")).expect("read"),
            bytes
        );

        let object = store.directory().join(&name[..2]).join(&name);
        let mut stored = std::fs::read(&object).expect("read object");
        let middle = stored.len() / 2;
        stored[middle] ^= 0xff;
        std::fs::write(&object, &stored).expect("corrupt object");
        assert!(matches!(store.get(&name), Err(ChunkError::Corrupt(_))));

        std::fs::write(workspace.join("data/big.bin"), b"previous").expect("write previous");
        assert!(materialize(&store, &workspace, &file).is_err());
        assert_eq!(
            std::fs::read(workspace.join("data/big.bin")).expect("read"),
            b"previous",
            "a failed materialization leaves the target untouched"
        );
        let leftovers: Vec<_> = walk(&workspace.join("data"));
        assert_eq!(leftovers.len(), 1, "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_put_repairs_a_corrupt_object_and_never_rewrites_a_sound_one() {
        use std::os::unix::fs::MetadataExt;
        let directory = scratch("repair");
        let bytes = noise(6, 150_000);
        let name = LocalChunkStore::new(&directory).put(&bytes).expect("put");
        let object = directory.join(&name[..2]).join(&name);
        let inode = std::fs::metadata(&object).expect("object").ino();

        // Sound: a later process (a fresh store) verifies it and keeps it.
        let store = LocalChunkStore::new(&directory);
        assert_eq!(store.put(&bytes).expect("put sound"), name);
        assert_eq!(std::fs::metadata(&object).expect("object").ino(), inode);

        // Truncated, as a torn copy leaves it: the next put replaces it.
        let stored = std::fs::read(&object).expect("read object");
        std::fs::write(&object, &stored[..stored.len() / 2]).expect("truncate");
        let store = LocalChunkStore::new(&directory);
        assert!(matches!(store.get(&name), Err(ChunkError::Corrupt(_))));
        assert_eq!(store.put(&bytes).expect("put repairs"), name);
        assert_eq!(store.get(&name).expect("repaired"), bytes);
        assert_ne!(
            std::fs::metadata(&object).expect("object").ino(),
            inode,
            "replaced by a rename, not written in place"
        );
        assert_eq!(walk(&directory).len(), 1, "no temporary file left");
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn materialize_never_writes_through_a_symlink_or_past_a_file() {
        let directory = scratch("nofollow");
        let store = LocalChunkStore::new(directory.join("store"));
        let bytes = noise(7, 100_000);
        let name = store.put(&bytes).expect("put");
        let workspace = directory.join("ws");
        let outside = directory.join("outside");
        std::fs::create_dir_all(&workspace).expect("workspace");
        std::fs::create_dir_all(&outside).expect("outside");
        std::fs::write(outside.join("model.bin"), b"owner's file").expect("victim");
        let file = |path: &str| LargeFile {
            path: path.into(),
            mode: 0o100_644,
            size: bytes.len() as u64,
            chunks: vec![name.clone()],
        };

        // A symlinked directory, first or deeper in the path.
        std::os::unix::fs::symlink(&outside, workspace.join("assets")).expect("symlink");
        let refused = materialize(&store, &workspace, &file("assets/model.bin"))
            .expect_err("a symlinked parent is refused");
        assert!(
            refused.contains("not a directory of the workspace"),
            "{refused}"
        );
        std::fs::create_dir_all(workspace.join("deep")).expect("deep");
        std::os::unix::fs::symlink("../../outside", workspace.join("deep/link")).expect("symlink");
        assert!(materialize(&store, &workspace, &file("deep/link/model.bin")).is_err());
        assert_eq!(
            std::fs::read(outside.join("model.bin")).expect("victim"),
            b"owner's file"
        );
        assert_eq!(walk(&outside).len(), 1, "nothing written outside");

        // A file where a directory should be.
        std::fs::write(workspace.join("plain"), b"x").expect("file");
        assert!(materialize(&store, &workspace, &file("plain/model.bin")).is_err());

        // A symlink at the target itself is replaced, not written through.
        std::os::unix::fs::symlink(outside.join("model.bin"), workspace.join("top.bin"))
            .expect("symlink");
        materialize(&store, &workspace, &file("top.bin")).expect("materialize");
        assert_eq!(
            std::fs::read(workspace.join("top.bin")).expect("read"),
            bytes
        );
        assert!(
            !std::fs::symlink_metadata(workspace.join("top.bin"))
                .expect("stat")
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read(outside.join("model.bin")).expect("victim"),
            b"owner's file"
        );

        // Missing directories are made, inside the workspace.
        materialize(&store, &workspace, &file("new/dir/model.bin")).expect("materialize");
        assert_eq!(
            std::fs::read(workspace.join("new/dir/model.bin")).expect("read"),
            bytes
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn manifests_round_trip_and_refuse_unsafe_entries() {
        let manifest = Manifest {
            files: vec![LargeFile {
                path: "a/b.bin".into(),
                mode: 0o100_755,
                size: 3,
                chunks: vec![hash_of(b"abc")],
            }],
        };
        let bytes = manifest.to_bytes();
        assert_eq!(Manifest::from_bytes(&bytes).expect("parse"), manifest);
        assert_eq!(manifest.to_bytes(), bytes, "canonical");
        for path in ["../x", "/etc/passwd", "a/../../x", ".git/config", ""] {
            let mut bad = manifest.clone();
            bad.files[0].path = path.into();
            assert!(Manifest::from_bytes(&bad.to_bytes()).is_err(), "{path}");
        }
        let mut bad = manifest.clone();
        bad.files[0].chunks = vec!["../../x".to_string()];
        assert!(Manifest::from_bytes(&bad.to_bytes()).is_err());
    }

    #[test]
    fn a_manifest_path_of_any_bytes_round_trips() {
        let file = |path: TreePath| LargeFile {
            path,
            mode: 0o100_644,
            size: 3,
            chunks: vec![hash_of(b"abc")],
        };
        let manifest = Manifest {
            files: vec![
                file(TreePath::from_bytes(b"dir/bad\xffname.bin".to_vec())),
                file(TreePath::from("line\nbreak.bin")),
                file(TreePath::from("tab\tand \"quote\".bin")),
            ],
        };
        let bytes = manifest.to_bytes();
        assert_eq!(Manifest::from_bytes(&bytes).expect("parse"), manifest);
        let text = String::from_utf8(bytes).expect("JSON");
        assert!(
            text.contains(r#""path_hex":"6469722f626164ff6e616d652e62696e""#),
            "{text}"
        );
        assert!(text.contains(r#""path":"line\nbreak.bin""#), "{text}");
        // A UTF-8 path has one form only, and hex must be hex.
        for entry in [
            r#""path_hex":"6162""#,
            r#""path_hex":"zz""#,
            r#""path_hex":"616""#,
        ] {
            let forged = format!(
                r#"{{"version":1,"files":[{{"chunks":[],"mode":33188,"size":0,{entry}}}]}}"#
            );
            assert!(Manifest::from_bytes(forged.as_bytes()).is_err(), "{entry}");
        }
    }

    #[test]
    fn the_trailer_round_trips_through_a_message() {
        let name = hash_of(b"manifest");
        let message = with_trailer("spawn checkpoint x", Some(&name));
        assert_eq!(
            message,
            format!("spawn checkpoint x\n\nSpawn-Large: {name}\n")
        );
        assert_eq!(trailer_of(&message), Some(name));
        assert_eq!(with_trailer("plain", None), "plain");
        assert_eq!(trailer_of("plain"), None);
        assert_eq!(trailer_of("x\n\nSpawn-Large: not-a-hash\n"), None);
        assert_eq!(
            trailer_of("Spawn-Large: 0000\n\nbody without trailer"),
            None,
            "only the last paragraph holds trailers"
        );
    }
}
