//! Chunk store adapter and the large-file manifest (docs/storage.md, phase 1).
//!
//! A large file is cut into content-defined chunks (a gear-hash FastCDC), each
//! named by the BLAKE3 hash of its bytes and stored zstd-compressed in a local
//! directory. The manifest of a workspace's large files (path, mode, size,
//! chunk list) is itself an object in the store; a checkpoint commit names it
//! in a `Spawn-Large: <hash>` trailer. Because cut points follow the content,
//! an in-place edit changes only the chunks it touches, and identical chunks
//! are stored once.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use spawn_core::port::{ChunkError, ChunkStore};

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
}

impl LocalChunkStore {
    /// Store rooted at `directory`, created on the first write.
    pub fn new(directory: impl AsRef<Path>) -> Self {
        Self {
            directory: directory.as_ref().to_path_buf(),
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

impl ChunkStore for LocalChunkStore {
    fn put(&self, bytes: &[u8]) -> Result<String, ChunkError> {
        let name = hash_of(bytes);
        let path = self.path_of(&name);
        if path.exists() {
            return Ok(name);
        }
        let parent = path
            .parent()
            .ok_or_else(|| ChunkError::Unavailable(format!("{} has no parent", path.display())))?;
        std::fs::create_dir_all(parent).map_err(|error| unavailable("create", parent, &error))?;
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
        // A hard link never replaces an existing object; a writer that lost
        // the race wrote the same bytes, so its copy is simply dropped.
        let linked = match std::fs::hard_link(&temporary, &path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(_) if path.exists() => Ok(()),
            // A file system without hard links: rename is still atomic.
            Err(_) => std::fs::rename(&temporary, &path),
        };
        let _ = std::fs::remove_file(&temporary);
        linked.map_err(|error| unavailable("publish", &path, &error))?;
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
}

/// One large file in a manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LargeFile {
    /// Path relative to the workspace, `/`-separated.
    pub path: String,
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
                json!({"path": file.path, "mode": file.mode, "size": file.size,
                       "chunks": file.chunks})
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
            let path = entry
                .get("path")
                .and_then(Value::as_str)
                .filter(|path| is_safe_path(path))
                .ok_or("manifest: bad path")?
                .to_string();
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

/// A relative path of normal components only: no root, no `..`, no `.git`.
pub fn is_safe_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\0')
        && Path::new(path).components().all(|component| match component {
            Component::Normal(name) => name != ".git",
            _ => false,
        })
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

/// Write `file` under `directory` from `store`, atomically: every chunk is
/// verified and the size checked into a temporary file beside the target,
/// which is renamed into place only when all of it is right. On any error
/// the target is left untouched.
pub fn materialize(
    store: &dyn ChunkStore,
    directory: &Path,
    file: &LargeFile,
) -> Result<(), String> {
    if !is_safe_path(&file.path) {
        return Err(format!("unsafe path {}", file.path));
    }
    let target = directory.join(&file.path);
    let parent = target
        .parent()
        .ok_or_else(|| format!("{} has no parent", target.display()))?;
    std::fs::create_dir_all(parent).map_err(|error| format!("create {}: {error}", parent.display()))?;
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let temporary = parent.join(format!(
        ".{name}.spawn-{}-{}",
        std::process::id(),
        TEMPORARY.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| -> Result<(), String> {
        let mut output = File::create(&temporary)
            .map_err(|error| format!("create {}: {error}", temporary.display()))?;
        let mut size = 0u64;
        for chunk in &file.chunks {
            let bytes = store.get(chunk).map_err(|error| error.to_string())?;
            size += bytes.len() as u64;
            output
                .write_all(&bytes)
                .map_err(|error| format!("write {}: {error}", temporary.display()))?;
        }
        if size != file.size {
            return Err(format!(
                "{}: chunks hold {size} bytes, the manifest says {}",
                file.path, file.size
            ));
        }
        output
            .sync_all()
            .map_err(|error| format!("sync {}: {error}", temporary.display()))?;
        set_mode(&temporary, file.mode)?;
        std::fs::rename(&temporary, &target)
            .map_err(|error| format!("rename to {}: {error}", target.display()))
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let permissions = std::fs::Permissions::from_mode(if mode == 0o100_755 { 0o755 } else { 0o644 });
    std::fs::set_permissions(path, permissions)
        .map_err(|error| format!("chmod {}: {error}", path.display()))
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
        assert!(rest.iter().all(|chunk| (MIN_CHUNK..=MAX_CHUNK).contains(&chunk.len())));
        assert!(last.len() <= MAX_CHUNK);
        assert!(chunks.len() > 4, "about {} chunks of {AVG_CHUNK}", chunks.len());
        assert!(chunks_of(&[]).is_empty());
        assert_eq!(chunks_of(b"tiny"), vec![b"tiny".to_vec()]);
    }

    #[test]
    fn an_in_place_edit_changes_only_nearby_chunks() {
        let data = noise(2, 12 << 20);
        let mut edited = data.clone();
        edited[5 << 20..6 << 20].copy_from_slice(&noise(3, 1 << 20));
        let before: std::collections::HashSet<_> =
            chunks_of(&data).iter().map(|chunk| hash_of(chunk)).collect();
        let new_bytes: usize = chunks_of(&edited)
            .iter()
            .filter(|chunk| !before.contains(&hash_of(chunk)))
            .map(Vec::len)
            .sum();
        assert!(new_bytes < 5 << 20, "{new_bytes} new bytes for a 1 MiB edit");
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
            path: "data/big.bin".to_string(),
            mode: 0o100_644,
            size: bytes.len() as u64,
            chunks: vec![name.clone()],
        };
        let workspace = directory.join("ws");
        materialize(&store, &workspace, &file).expect("materialize");
        assert_eq!(std::fs::read(workspace.join("data/big.bin")).expect("read"), bytes);

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
    fn manifests_round_trip_and_refuse_unsafe_entries() {
        let manifest = Manifest {
            files: vec![LargeFile {
                path: "a/b.bin".to_string(),
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
            bad.files[0].path = path.to_string();
            assert!(Manifest::from_bytes(&bad.to_bytes()).is_err(), "{path}");
        }
        let mut bad = manifest.clone();
        bad.files[0].chunks = vec!["../../x".to_string()];
        assert!(Manifest::from_bytes(&bad.to_bytes()).is_err());
    }

    #[test]
    fn the_trailer_round_trips_through_a_message() {
        let name = hash_of(b"manifest");
        let message = with_trailer("spawn checkpoint x", Some(&name));
        assert_eq!(message, format!("spawn checkpoint x\n\nSpawn-Large: {name}\n"));
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
