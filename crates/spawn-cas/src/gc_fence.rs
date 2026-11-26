//! D6 GC Fence (`<cas_root>/.gc-fence`) and 2-round / 24h-grace persistent
//! reachability garbage collection (`#113` Fine Item 7.2).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::gc::{is_persistent_namespace, read_dir_or_empty};
use crate::store::{is_shard, open_nofollow, remove_missing_ok};
use crate::{Algo, CasError, ChunkIndex, Digest, NS_BLOB, NS_TREE, Store, TreeEntryKind, is_hex64};

/// File binding a CAS root to its owning PostgreSQL database/schema.
pub(crate) const GC_FENCE_FILE: &str = ".gc-fence";

/// Persistent GC consecutive-unreachable round state under `tmp/`.
pub const GC_UNREACH_STATE_FILE: &str = ".gc-unreach-state.json";

/// Default minimum grace age before unreachable persistent objects are deleted (24 hours).
pub const DEFAULT_PERSISTENT_GC_GRACE: Duration = Duration::from_hours(24);

/// PostgreSQL cluster, database OID, and schema identity recorded in `.gc-fence`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcFenceBinding {
    /// Cluster `system_identifier` from `pg_control_system()`.
    pub db_system_id: String,
    /// Database `oid` from `pg_database`.
    pub db_oid: u32,
    /// PostgreSQL schema name holding `spawn_snapshots` / `spawn_refs`.
    pub schema_name: String,
}

impl GcFenceBinding {
    fn summary(&self) -> String {
        format!("{}:{}:{}", self.db_system_id, self.db_oid, self.schema_name)
    }

    fn validate(&self) -> Result<(), CasError> {
        if self.db_system_id.trim().is_empty() || self.schema_name.trim().is_empty() {
            return Err(CasError::BadFormat(
                "GC fence binding requires non-empty db_system_id and schema_name",
            ));
        }
        Ok(())
    }
}

/// Summary of one [`Store::gc_persistent`] pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PersistentGcReport {
    /// Number of root snapshots passed in.
    pub roots: usize,
    /// Total reachable persistent object digests (`snapshot`, `tree`, `blob`, `chunk`).
    pub reachable_objects: usize,
    /// Unreachable persistent object digests deleted in this pass.
    pub deleted_objects: Vec<Digest>,
    /// Unreachable chunk indices deleted in this pass.
    pub deleted_chunk_indices: Vec<Digest>,
    /// Unreachable persistent objects deferred (< 2 consecutive rounds or within grace).
    pub deferred_objects: usize,
    /// Unreachable chunk indices deferred (< 2 consecutive rounds or within grace).
    pub deferred_chunk_indices: usize,
    /// Bytes freed from deleted persistent objects and chunk indices.
    pub freed_bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct UnreachEntry {
    first_unreachable_ms: u64,
    #[serde(default)]
    first_unreachable_ns: u64,
    consecutive_rounds: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct UnreachState {
    objects: BTreeMap<Digest, UnreachEntry>,
    chunk_indices: BTreeMap<Digest, UnreachEntry>,
}

fn unix_ms(time: SystemTime) -> u64 {
    u64::try_from(
        time.duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

fn unix_ns(time: SystemTime) -> u64 {
    u64::try_from(
        time.duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    )
    .unwrap_or(u64::MAX)
}

/// Write `bytes` atomically to `target` (mode `0600`, `sync_all()` before rename).
fn write_atomic_0600(
    store: &Store,
    prefix: &str,
    target: &Path,
    bytes: &[u8],
) -> Result<(), CasError> {
    let (mut file, staged) = store.temp_file(prefix)?;
    let write_result = file
        .write_all(bytes)
        .and_then(|()| file.flush())
        .and_then(|()| file.sync_all())
        .and_then(|()| file.set_permissions(fs::Permissions::from_mode(0o600)));
    drop(file);
    if let Err(error) = write_result {
        let _ = remove_missing_ok(&staged);
        return Err(error.into());
    }
    fs::rename(&staged, target)?;
    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
    {
        open_nofollow(parent)?.sync_all()?;
    }
    Ok(())
}

/// Persist `.gc-fence` (`GcFenceBinding`) at `<cas_root>/.gc-fence` idempotently.
pub fn bind_gc_fence(store: &Store, binding: &GcFenceBinding) -> Result<(), CasError> {
    store.bind_gc_fence(binding)
}

/// Verify `<cas_root>/.gc-fence` against `live_binding` and confirm PG is not
/// lagging behind `refs.journal`.
pub fn verify_gc_fence(
    store: &Store,
    live_binding: &GcFenceBinding,
    pg_latest_ref_seq: u64,
    journal_latest_seq: u64,
) -> Result<(), CasError> {
    store.verify_gc_fence(live_binding, pg_latest_ref_seq, journal_latest_seq)
}

struct ReachableClosure {
    objects: BTreeSet<Digest>,
    chunk_indices: BTreeSet<Digest>,
}

fn collect_reachable(store: &Store, roots: &[Digest]) -> Result<ReachableClosure, CasError> {
    let mut reachable_objects = BTreeSet::new();
    let mut reachable_chunk_indices = BTreeSet::new();
    let mut visited_snapshots = BTreeSet::new();
    let mut visited_trees = BTreeSet::new();
    let mut snap_queue = VecDeque::new();
    let mut tree_queue = VecDeque::new();

    for root in roots {
        snap_queue.push_back(root.clone());
    }

    while let Some(snap_digest) = snap_queue.pop_front() {
        if !visited_snapshots.insert(snap_digest.clone()) {
            continue;
        }
        reachable_objects.insert(snap_digest.clone());
        let snapshot = store.read_merkle_snapshot(&snap_digest)?;
        tree_queue.push_back(snapshot.tree);
        for parent in snapshot.parents {
            reachable_objects.insert(parent.clone());
            if store.has(&parent) {
                snap_queue.push_back(parent);
            }
        }
    }

    while let Some(tree_digest) = tree_queue.pop_front() {
        if !visited_trees.insert(tree_digest.clone()) {
            continue;
        }
        reachable_objects.insert(tree_digest.clone());
        let tree = store.read_tree(&tree_digest)?;
        for entry in tree.entries {
            match entry.kind {
                TreeEntryKind::Dir => {
                    let child = Digest::from_raw_sha256(entry.id);
                    tree_queue.push_back(child);
                }
                TreeEntryKind::File | TreeEntryKind::Executable | TreeEntryKind::Symlink => {
                    let blob = Digest::from_raw_sha256(entry.id);
                    reachable_objects.insert(blob.clone());
                    let idx_path = store.chunk_index_path(&blob);
                    match fs::symlink_metadata(&idx_path) {
                        Ok(meta) => {
                            if !meta.is_file() {
                                return Err(CasError::NotPlain(idx_path));
                            }
                            reachable_chunk_indices.insert(blob.clone());
                            let index = store.read_chunk_index(&blob)?;
                            for chunk in index.chunks {
                                reachable_objects.insert(chunk.id);
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                TreeEntryKind::Gitlink => {}
            }
        }
    }

    Ok(ReachableClosure {
        objects: reachable_objects,
        chunk_indices: reachable_chunk_indices,
    })
}

fn next_unreach_entry(
    prev: Option<&UnreachEntry>,
    mtime: SystemTime,
    now: SystemTime,
    now_millis: u64,
    now_nanos: u64,
    grace: Duration,
    grace_millis: u64,
) -> (UnreachEntry, bool) {
    let mtime_millis = unix_ms(mtime);
    let mtime_nanos = unix_ns(mtime);
    let entry = match prev {
        Some(prior)
            if mtime_millis <= prior.first_unreachable_ms
                && (prior.first_unreachable_ns == 0
                    || mtime_nanos <= prior.first_unreachable_ns) =>
        {
            UnreachEntry {
                first_unreachable_ms: prior.first_unreachable_ms,
                first_unreachable_ns: prior.first_unreachable_ns,
                consecutive_rounds: prior.consecutive_rounds.saturating_add(1),
            }
        }
        _ => UnreachEntry {
            first_unreachable_ms: now_millis,
            first_unreachable_ns: now_nanos,
            consecutive_rounds: 1,
        },
    };
    let state_elapsed_ok = now_millis.saturating_sub(entry.first_unreachable_ms) >= grace_millis;
    let mtime_elapsed_ok = now.duration_since(mtime).is_ok_and(|age| age >= grace);
    let eligible = entry.consecutive_rounds >= 2 && state_elapsed_ok && mtime_elapsed_ok;
    (entry, eligible)
}

fn load_unreach_state(path: &Path) -> Result<UnreachState, CasError> {
    let mut file = match open_nofollow(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(UnreachState::default()),
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
            return Err(CasError::NotPlain(path.to_path_buf()));
        }
        Err(error) => return Err(error.into()),
    };
    if !file.metadata()?.is_file() {
        return Err(CasError::NotPlain(path.to_path_buf()));
    }
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut bytes)?;
    Ok(serde_json::from_slice(&bytes).unwrap_or_default())
}

fn list_chunk_indices(store: &Store) -> Result<Vec<(Digest, PathBuf, fs::Metadata)>, CasError> {
    let dir = store.root().join("chunk-index").join(Algo::Sha256.name());
    let mut out = Vec::new();
    for shard in read_dir_or_empty(&dir)? {
        let shard_name = shard.file_name().to_string_lossy().into_owned();
        if !is_shard(&shard_name) || !shard.file_type()?.is_dir() {
            continue;
        }
        for entry in read_dir_or_empty(&shard.path())? {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_hex64(&name) || !name.starts_with(&shard_name) {
                continue;
            }
            let path = entry.path();
            let meta = match fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if !meta.is_file() {
                return Err(CasError::NotPlain(path));
            }
            out.push((Digest::from_parts(Algo::Sha256, &name)?, path, meta));
        }
    }
    out.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(out)
}

fn ephemeral_digests(store: &Store) -> Result<BTreeSet<Digest>, CasError> {
    let mut out = BTreeSet::new();
    for namespace in read_dir_or_empty(&store.root().join("access"))? {
        if !namespace.file_type()?.is_dir() {
            continue;
        }
        let name = namespace.file_name().to_string_lossy().into_owned();
        if is_persistent_namespace(&name) {
            continue;
        }
        for algo_dir in read_dir_or_empty(&namespace.path())? {
            let Some(algo) = Algo::from_name(&algo_dir.file_name().to_string_lossy()) else {
                continue;
            };
            if !algo_dir.file_type()?.is_dir() {
                continue;
            }
            for record in read_dir_or_empty(&algo_dir.path())? {
                let hex = record.file_name().to_string_lossy().into_owned();
                if !is_hex64(&hex) {
                    continue;
                }
                if fs::symlink_metadata(record.path()).is_ok_and(|meta| meta.is_file()) {
                    out.insert(Digest::from_parts(algo, &hex)?);
                }
            }
        }
    }
    Ok(out)
}

impl Store {
    /// Path to `<cas_root>/.gc-fence`.
    pub fn gc_fence_path(&self) -> PathBuf {
        self.root().join(GC_FENCE_FILE)
    }

    /// Path to `<cas_root>/tmp/.gc-unreach-state.json`.
    pub fn gc_unreach_state_path(&self) -> PathBuf {
        self.tmp_dir().join(GC_UNREACH_STATE_FILE)
    }

    /// Read the stored `.gc-fence` binding if present.
    pub fn read_gc_fence(&self) -> Result<Option<GcFenceBinding>, CasError> {
        let path = self.gc_fence_path();
        match fs::symlink_metadata(&path) {
            Ok(meta) => {
                if !meta.is_file() {
                    return Err(CasError::NotPlain(path));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let bytes = self.read_plain_bytes(&path)?;
        let binding: GcFenceBinding = serde_json::from_slice(&bytes)
            .map_err(|_| CasError::BadFormat("corrupt .gc-fence JSON"))?;
        binding.validate()?;
        Ok(Some(binding))
    }

    /// Bind `<cas_root>/.gc-fence` to `binding`. Idempotent when the same
    /// binding is already stored; refuses a different binding.
    pub fn bind_gc_fence(&self, binding: &GcFenceBinding) -> Result<(), CasError> {
        binding.validate()?;
        let _lock = self.lock_gc()?;
        if let Some(existing) = self.read_gc_fence()? {
            if &existing == binding {
                return Ok(());
            }
            return Err(CasError::GcFenceMismatch {
                expected: existing.summary(),
                actual: binding.summary(),
            });
        }
        let bytes = serde_json::to_vec_pretty(binding)
            .map_err(|_| CasError::BadFormat("failed to serialize .gc-fence"))?;
        write_atomic_0600(self, "gc-fence", &self.gc_fence_path(), &bytes)
    }

    /// Verify `.gc-fence` matches `live_binding` and `pg_latest_ref_seq >= journal_latest_seq`.
    pub fn verify_gc_fence(
        &self,
        live_binding: &GcFenceBinding,
        pg_latest_ref_seq: u64,
        journal_latest_seq: u64,
    ) -> Result<(), CasError> {
        live_binding.validate()?;
        let Some(stored) = self.read_gc_fence()? else {
            return Err(CasError::GcFenceMissing(self.gc_fence_path()));
        };
        if &stored != live_binding {
            return Err(CasError::GcFenceMismatch {
                expected: stored.summary(),
                actual: live_binding.summary(),
            });
        }
        if pg_latest_ref_seq < journal_latest_seq {
            return Err(CasError::GcFenceBehindJournal {
                pg_latest_ref_seq,
                journal_latest_seq,
            });
        }
        Ok(())
    }

    /// Run one persistent D6 reachability GC sweep under exclusive `lock_gc()`
    /// and a verified GC Fence (`#113` Fine Item 7.2).
    ///
    /// Deletes an unpinned, unreachable persistent object (and its chunk index
    /// if chunked) ONLY when it has been unreachable for `>= 2` consecutive
    /// rounds AND `(now - first_unreachable_at) >= grace` AND its file `mtime`
    /// is at least `grace` old (refreshing under `LOCK_SH` on write reuse).
    pub fn gc_persistent(
        &self,
        roots: &[Digest],
        live_binding: &GcFenceBinding,
        pg_latest_ref_seq: u64,
        grace: Duration,
    ) -> Result<PersistentGcReport, CasError> {
        let _lock = self.lock_gc()?;
        let journal_latest_seq = self.latest_seq()?;
        self.verify_gc_fence(live_binding, pg_latest_ref_seq, journal_latest_seq)?;

        let pinned = self.pinned()?;
        let ephemeral = ephemeral_digests(self)?;
        let closure = collect_reachable(self, roots)?;

        let state_path = self.gc_unreach_state_path();
        let prev_state = load_unreach_state(&state_path)?;
        let mut next_state = UnreachState::default();

        let now = self.now();
        let now_millis = unix_ms(now);
        let now_nanos = unix_ns(now);
        let grace_millis = u64::try_from(grace.as_millis()).unwrap_or(u64::MAX);

        let mut report = PersistentGcReport {
            roots: roots.len(),
            reachable_objects: closure.objects.len(),
            ..PersistentGcReport::default()
        };

        let mut protected_by_retained_index: BTreeSet<Digest> = BTreeSet::new();
        for (file_digest, idx_path, idx_meta) in list_chunk_indices(self)? {
            if closure.chunk_indices.contains(&file_digest) || pinned.contains(&file_digest) {
                continue;
            }
            let index = ChunkIndex::decode(&self.read_plain_bytes(&idx_path)?)?;
            let (entry, eligible) = next_unreach_entry(
                prev_state.chunk_indices.get(&file_digest),
                idx_meta.modified()?,
                now,
                now_millis,
                now_nanos,
                grace,
                grace_millis,
            );
            if eligible {
                if remove_missing_ok(&idx_path)? {
                    report.freed_bytes += idx_meta.len();
                    report.deleted_chunk_indices.push(file_digest);
                }
            } else {
                for chunk in index.chunks {
                    protected_by_retained_index.insert(chunk.id);
                }
                next_state.chunk_indices.insert(file_digest, entry);
                report.deferred_chunk_indices += 1;
            }
        }

        for digest in self.persistent_digests()? {
            if closure.objects.contains(&digest) || pinned.contains(&digest) {
                continue;
            }
            let obj_path = self.object_path(&digest);
            let obj_meta = match fs::symlink_metadata(&obj_path) {
                Ok(meta) => {
                    if !meta.is_file() {
                        return Err(CasError::NotPlain(obj_path));
                    }
                    meta
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    self.remove_persistent_access(&digest)?;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            let (entry, grace_eligible) = next_unreach_entry(
                prev_state.objects.get(&digest),
                obj_meta.modified()?,
                now,
                now_millis,
                now_nanos,
                grace,
                grace_millis,
            );
            if grace_eligible && !protected_by_retained_index.contains(&digest) {
                self.remove_persistent_access(&digest)?;
                if !ephemeral.contains(&digest) && remove_missing_ok(&obj_path)? {
                    report.freed_bytes += obj_meta.len();
                }
                report.deleted_objects.push(digest);
            } else {
                next_state.objects.insert(digest, entry);
                report.deferred_objects += 1;
            }
        }

        let encoded = serde_json::to_vec(&next_state)
            .map_err(|_| CasError::BadFormat("failed to serialize persistent GC state"))?;
        write_atomic_0600(self, "gc-state", &state_path, &encoded)?;
        Ok(report)
    }

    fn remove_persistent_access(&self, digest: &Digest) -> Result<(), CasError> {
        for ns in [NS_TREE, NS_BLOB] {
            remove_missing_ok(&self.access_path(ns, digest))?;
        }
        Ok(())
    }
}
