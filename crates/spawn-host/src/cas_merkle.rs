//! D6 Phase 4 Step 2 (`#113` Fine Item 7.1–7.3 & Blind-Spot Safeguards #1, #2, #4, #5, #6):
//! `CasMerkleStore` — fail-closed workspace tracked-set scanner, in-memory cached
//! `O(|diff| + depth)` Merkle updates, 4-step atomic durability publishing
//! (`cas.sync_store` -> PG `spawn_snapshots`/`spawn_refs` -> `fsync` `refs.journal`),
//! and Layer 0 materialization without `0444` CAS hard-links.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::error::Error;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{self, BufReader, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use spawn_cas::{
    CasError, Digest, GIT_POINTER_THRESHOLD, GitOid, GitOidCache, JournalEntry, NS_BLOB, Snapshot,
    Store, Tree, TreeEntry, TreeEntryKind, TreeMutation, empty_tree_digest, is_valid_entry_name,
    project_snapshot_oid, project_tree_oid, sha256_of, sha256_reader,
};

use crate::tree_path::TreePath;
use crate::{ENGINE_RUNTIME_FILES, LARGE_FILE_TRAILER, canonical_root};

/// Root-level build directories excluded from D6 workspace checkpoints
/// unless tracked in both `base_tree` and `main_tree`.
pub const BUILD_OUTPUT: [&str; 2] = ["target", ".spawn"];

/// Errors returned by [`CasMerkleStore`] and [`crate::GitBorderAdapter`].
#[derive(Debug)]
pub enum D6StoreError {
    /// Content-addressed store failure.
    Cas(CasError),
    /// Filesystem or OS I/O failure.
    Io(io::Error),
    /// Persistent metadata ledger failure (PostgreSQL or in-memory ledger).
    Ledger(String),
    /// Invalid input, symlinked control file, malformed ignore syntax, or disallowed path.
    InvalidInput(String),
    /// Fail-closed detection of an untracked ignored/sensitive file inside a tree snapshot.
    IgnoredLeak {
        /// Repository-relative path that triggered the guard.
        path: String,
        /// Rule source/pattern that matched.
        rule: String,
    },
    /// Border OID projection disagreed with native Git OID computation.
    OidMismatch {
        /// Object kind (`tree`, `commit`, `ref`).
        what: &'static str,
        /// Expected deterministic 40-hex OID.
        expected: String,
        /// Actual OID returned by Git.
        actual: String,
    },
    /// External Git boundary adapter command failed.
    GitBorder {
        /// Command summary.
        command: String,
        /// Captured stderr or diagnostic.
        detail: String,
    },
}

impl fmt::Display for D6StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cas(err) => write!(f, "CAS error: {err}"),
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::Ledger(msg) => write!(f, "ledger error: {msg}"),
            Self::InvalidInput(msg) => write!(f, "invalid input: {msg}"),
            Self::IgnoredLeak { path, rule } => {
                write!(f, "ignored untracked file {path:?} matched {rule:?}")
            }
            Self::OidMismatch {
                what,
                expected,
                actual,
            } => write!(
                f,
                "Git OID mismatch for {what}: expected {expected}, got {actual}"
            ),
            Self::GitBorder { command, detail } => {
                write!(f, "git border {command} failed: {detail}")
            }
        }
    }
}

impl Error for D6StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Cas(err) => Some(err),
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<CasError> for D6StoreError {
    fn from(err: CasError) -> Self {
        Self::Cas(err)
    }
}

impl From<io::Error> for D6StoreError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// Classification of a ref recorded in [`RefLedger`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum D6RefKind {
    /// Protected project main branch (advances strictly via landing gate / bootstrap).
    Main,
    /// Task or feature branch.
    Branch,
    /// Volatile background WIP flush ref.
    Wip,
    /// Immutable tag ref.
    Tag,
}

impl D6RefKind {
    /// Wire/database lowercase representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Branch => "branch",
            Self::Wip => "wip",
            Self::Tag => "tag",
        }
    }
}

/// Recorded snapshot metadata stored in [`RefLedger`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerSnapshot {
    /// Canonical `sha256:<64 hex>` snapshot digest.
    pub id: Digest,
    /// Deterministic 40-hex projected Git commit OID.
    pub git_oid: GitOid,
    /// Canonical `spawn-tree-v1` root digest.
    pub tree: Digest,
    /// Parent snapshot digests in Git first-parent order.
    pub parents: Vec<Digest>,
    /// Project name.
    pub project: String,
    /// Creation timestamp in Unix epoch milliseconds.
    pub created_at_ms: i64,
}

/// Recorded ref row in [`RefLedger`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerRef {
    /// Project name.
    pub project: String,
    /// Ref name (`main`, `feat/x`, `tags/v1`, `wip/...`).
    pub name: String,
    /// Classification.
    pub kind: D6RefKind,
    /// Target `spawn-snapshot-v1` digest.
    pub snapshot: Digest,
    /// Update timestamp in Unix epoch milliseconds.
    pub updated_at_ms: i64,
}

/// Hexagonal transactional ref & snapshot ledger abstraction (implemented over
/// `spawn-postgres` in callers/tests, keeping `spawn-host` decoupled from PG drivers).
pub trait RefLedger {
    /// Idempotently record a snapshot row (`spawn_snapshots`).
    fn record_snapshot(
        &self,
        id: &Digest,
        git_oid: &GitOid,
        tree: &Digest,
        parents: &[Digest],
        project: &str,
        created_at_ms: i64,
    ) -> Result<Digest, D6StoreError>;

    /// Look up snapshot metadata by its canonical `sha256:` digest.
    fn get_snapshot(&self, id: &Digest) -> Result<Option<LedgerSnapshot>, D6StoreError>;

    /// Look up snapshot metadata by its projected 40-hex Git OID.
    fn get_snapshot_by_oid(&self, git_oid: &GitOid)
    -> Result<Option<LedgerSnapshot>, D6StoreError>;

    /// Atomically compare-and-swap a non-main ref (`branch`, `wip`, `tag`), returning log `seq`.
    fn cas_ref(
        &self,
        project: &str,
        name: &str,
        kind: D6RefKind,
        expected: Option<&Digest>,
        new_snapshot: &Digest,
        reason: &str,
    ) -> Result<u64, D6StoreError>;

    /// Atomically advance or bootstrap a `kind = 'main'` ref, returning log `seq`.
    fn land_main_ref(
        &self,
        project: &str,
        name: &str,
        expected_base: Option<&Digest>,
        new_snapshot: &Digest,
        task_id: Option<i64>,
        reason: &str,
    ) -> Result<u64, D6StoreError>;

    /// List live refs, optionally scoped to `project`.
    fn list_refs(&self, project: Option<&str>) -> Result<Vec<LedgerRef>, D6StoreError>;
}

#[derive(Default)]
struct InMemoryState {
    snapshots: HashMap<Digest, LedgerSnapshot>,
    by_oid: HashMap<GitOid, Digest>,
    refs: BTreeMap<(String, String), LedgerRef>,
    next_seq: u64,
}

/// Pure in-memory [`RefLedger`] mirroring SQL CAS/idempotency/main-protection semantics
/// for deterministic unit testing without PostgreSQL.
#[derive(Default)]
pub struct InMemoryRefLedger {
    state: Mutex<InMemoryState>,
}

impl InMemoryRefLedger {
    /// Construct an empty in-memory ref ledger.
    pub fn new() -> Self {
        Self::default()
    }
}

impl RefLedger for InMemoryRefLedger {
    fn record_snapshot(
        &self,
        id: &Digest,
        git_oid: &GitOid,
        tree: &Digest,
        parents: &[Digest],
        project: &str,
        created_at_ms: i64,
    ) -> Result<Digest, D6StoreError> {
        if project.trim().is_empty() {
            return Err(D6StoreError::Ledger(
                "project must be non-empty".to_string(),
            ));
        }
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = guard.snapshots.get(id) {
            if existing.git_oid == *git_oid
                && existing.tree == *tree
                && existing.parents == parents
                && existing.project == project
                && existing.created_at_ms == created_at_ms
            {
                return Ok(existing.id.clone());
            }
            return Err(D6StoreError::Ledger(format!(
                "snapshot {id} already recorded with different metadata"
            )));
        }
        if let Some(other) = guard.by_oid.get(git_oid) {
            return Err(D6StoreError::Ledger(format!(
                "git_oid {git_oid} already mapped to snapshot {other}"
            )));
        }
        let row = LedgerSnapshot {
            id: id.clone(),
            git_oid: *git_oid,
            tree: tree.clone(),
            parents: parents.to_vec(),
            project: project.to_string(),
            created_at_ms,
        };
        guard.by_oid.insert(*git_oid, id.clone());
        guard.snapshots.insert(id.clone(), row);
        Ok(id.clone())
    }

    fn get_snapshot(&self, id: &Digest) -> Result<Option<LedgerSnapshot>, D6StoreError> {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(guard.snapshots.get(id).cloned())
    }

    fn get_snapshot_by_oid(
        &self,
        git_oid: &GitOid,
    ) -> Result<Option<LedgerSnapshot>, D6StoreError> {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(guard
            .by_oid
            .get(git_oid)
            .and_then(|id| guard.snapshots.get(id))
            .cloned())
    }

    fn cas_ref(
        &self,
        project: &str,
        name: &str,
        kind: D6RefKind,
        expected: Option<&Digest>,
        new_snapshot: &Digest,
        _reason: &str,
    ) -> Result<u64, D6StoreError> {
        if kind == D6RefKind::Main {
            return Err(D6StoreError::Ledger(
                "main refs must advance via land_main_ref".to_string(),
            ));
        }
        if project.trim().is_empty() || name.trim().is_empty() {
            return Err(D6StoreError::Ledger(
                "project and name must be non-empty".to_string(),
            ));
        }
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !guard.snapshots.contains_key(new_snapshot) {
            return Err(D6StoreError::Ledger(format!(
                "snapshot {new_snapshot} does not exist"
            )));
        }
        let key = (project.to_string(), name.to_string());
        if let Some(existing) = guard.refs.get(&key) {
            if existing.kind == D6RefKind::Main {
                return Err(D6StoreError::Ledger(
                    "existing main ref cannot move via cas_ref".to_string(),
                ));
            }
            if existing.kind != kind {
                return Err(D6StoreError::Ledger(format!(
                    "ref kind mismatch: existing {:?}, requested {:?}",
                    existing.kind, kind
                )));
            }
            if expected != Some(&existing.snapshot) {
                return Err(D6StoreError::Ledger(format!(
                    "ref CAS mismatch on ({project}, {name})"
                )));
            }
        } else if expected.is_some() {
            return Err(D6StoreError::Ledger(format!(
                "ref ({project}, {name}) does not exist"
            )));
        }
        guard.next_seq += 1;
        let seq = guard.next_seq;
        guard.refs.insert(
            key,
            LedgerRef {
                project: project.to_string(),
                name: name.to_string(),
                kind,
                snapshot: new_snapshot.clone(),
                updated_at_ms: i64::try_from(seq).unwrap_or(i64::MAX),
            },
        );
        Ok(seq)
    }

    fn land_main_ref(
        &self,
        project: &str,
        name: &str,
        expected_base: Option<&Digest>,
        new_snapshot: &Digest,
        task_id: Option<i64>,
        _reason: &str,
    ) -> Result<u64, D6StoreError> {
        if project.trim().is_empty() || name.trim().is_empty() {
            return Err(D6StoreError::Ledger(
                "project and name must be non-empty".to_string(),
            ));
        }
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !guard.snapshots.contains_key(new_snapshot) {
            return Err(D6StoreError::Ledger(format!(
                "snapshot {new_snapshot} does not exist"
            )));
        }
        let key = (project.to_string(), name.to_string());
        match expected_base {
            None => {
                if task_id.is_some() {
                    return Err(D6StoreError::Ledger(
                        "bootstrapping main ref requires task_id = None".to_string(),
                    ));
                }
                if guard.refs.contains_key(&key) {
                    return Err(D6StoreError::Ledger(format!(
                        "main ref ({project}, {name}) already exists"
                    )));
                }
            }
            Some(base) => {
                if task_id.is_none() {
                    return Err(D6StoreError::Ledger(
                        "advancing main ref requires accepted task_id".to_string(),
                    ));
                }
                if !guard.snapshots.contains_key(base) {
                    return Err(D6StoreError::Ledger(format!(
                        "expected base snapshot {base} does not exist"
                    )));
                }
                let existing = guard.refs.get(&key).ok_or_else(|| {
                    D6StoreError::Ledger(format!("main ref ({project}, {name}) does not exist"))
                })?;
                if existing.kind != D6RefKind::Main {
                    return Err(D6StoreError::Ledger("ref kind is not main".to_string()));
                }
                if &existing.snapshot != base {
                    return Err(D6StoreError::Ledger(format!(
                        "main ref CAS mismatch on ({project}, {name})"
                    )));
                }
            }
        }
        guard.next_seq += 1;
        let seq = guard.next_seq;
        guard.refs.insert(
            key,
            LedgerRef {
                project: project.to_string(),
                name: name.to_string(),
                kind: D6RefKind::Main,
                snapshot: new_snapshot.clone(),
                updated_at_ms: i64::try_from(seq).unwrap_or(i64::MAX),
            },
        );
        Ok(seq)
    }

    fn list_refs(&self, project: Option<&str>) -> Result<Vec<LedgerRef>, D6StoreError> {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(guard
            .refs
            .values()
            .filter(|r| project.is_none_or(|p| r.project == p))
            .cloned()
            .collect())
    }
}

/// Request parameters for [`CasMerkleStore::publish_snapshot`].
#[derive(Debug, Clone)]
pub struct PublishRequest<'a> {
    /// Target project identifier.
    pub project: &'a str,
    /// Target ref name (e.g. `main`, `feat/x`, `wip/feat/x`, `tags/v1`).
    pub ref_name: &'a str,
    /// Ref classification.
    pub kind: D6RefKind,
    /// Expected prior snapshot on this ref (`None` when creating or bootstrapping).
    pub expected_ref: Option<&'a Digest>,
    /// Canonical `spawn-tree-v1` root digest.
    pub tree: &'a Digest,
    /// Parent snapshot digests in Git first-parent order.
    pub parents: &'a [Digest],
    /// Verbatim raw Git commit body (headers after `tree`/`parent` + blank line + message).
    pub commit_body: &'a [u8],
    /// Transition timestamp in Unix epoch milliseconds.
    pub created_at_ms: i64,
    /// Transition log reason stored in `spawn_ref_log`.
    pub reason: &'a str,
    /// Landing task ID required when advancing an existing `main` ref.
    pub landing_task_id: Option<i64>,
}

/// Summary returned by [`CasMerkleStore::publish_snapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedSnapshot {
    /// Canonical `spawn-snapshot-v1` SHA-256 digest.
    pub snapshot: Digest,
    /// Root `spawn-tree-v1` SHA-256 digest.
    pub tree: Digest,
    /// Deterministic 40-hex projected commit OID.
    pub git_oid: GitOid,
    /// Deterministic 40-hex projected root tree OID.
    pub tree_git_oid: GitOid,
    /// Monotonic `spawn_ref_log.seq` written to both PG and `refs.journal`.
    pub ref_seq: u64,
}

/// Workspace tracking & ignore policy enforcing D6 Safeguard #1 (fail-closed tracked set).
#[derive(Debug, Clone, Default)]
pub struct TrackedSetPolicy<'a> {
    /// Base snapshot tree (paths tracked here survive `.gitignore` unless excluded).
    pub base_tree: Option<&'a Digest>,
    /// Mainline tree (required alongside `base_tree` to retain `ENGINE_RUNTIME_FILES` / `BUILD_OUTPUT`).
    pub main_tree: Option<&'a Digest>,
    /// Optional additional ignore files evaluated alongside `.git/info/exclude` (e.g. global excludes).
    pub extra_exclude_files: Vec<PathBuf>,
}

/// Classification of a scanned file system entry in a workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScannedKind {
    /// Regular non-executable file (`0644`).
    File,
    /// Regular executable file (`0755`).
    Executable,
    /// Symbolic link (`120000`).
    Symlink,
    /// Preserved submodule gitlink (`160000`) carried forward from `base_tree`.
    Gitlink {
        /// 20-byte (or 32-byte) commit OID.
        oid: Vec<u8>,
    },
}

/// One workspace entry admitted by [`CasMerkleStore::scan_workspace_tree`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedEntry {
    /// Tree-relative path.
    pub path: TreePath,
    /// Entry classification.
    pub kind: ScannedKind,
}

/// Summary returned by [`CasMerkleStore::materialize_tree`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaterializeReport {
    /// Regular and executable files streamed fresh from CAS objects.
    pub files_written: usize,
    /// Unchanged files hard-linked from an existing non-CAS layer directory.
    pub hardlinked_from_prev: usize,
    /// Symbolic links created.
    pub symlinks_written: usize,
    /// Non-root directories created.
    pub dirs_created: usize,
    /// Gitlink entries materialized as empty submodule mount directories.
    pub gitlinks_skipped: usize,
}

/// Format standard raw Git commit body bytes (`author` + `committer` + `\n` + message)
/// matching `git commit-tree` and `spawn-snapshot-v1`.
pub fn format_commit_body(author_line: &str, committer_line: &str, message: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(author_line.len() + committer_line.len() + message.len() + 24);
    out.extend_from_slice(b"author ");
    out.extend_from_slice(author_line.as_bytes());
    out.extend_from_slice(b"\ncommitter ");
    out.extend_from_slice(committer_line.as_bytes());
    out.extend_from_slice(b"\n\n");
    out.extend_from_slice(message.as_bytes());
    out
}

/// In-memory Merkle tree cache + `CasMerkleStore` facade over [`spawn_cas::Store`].
pub struct CasMerkleStore {
    cas: Store,
    oid_cache: Mutex<GitOidCache>,
    tree_cache: Mutex<HashMap<Digest, Tree>>,
}

impl CasMerkleStore {
    /// Open or initialize a `CasMerkleStore` at `cas_root`.
    pub fn open(cas_root: impl Into<PathBuf>) -> Result<Self, D6StoreError> {
        Ok(Self::from_store(Store::open(cas_root)?))
    }

    /// Wrap an existing [`spawn_cas::Store`].
    pub fn from_store(cas: Store) -> Self {
        Self {
            cas,
            oid_cache: Mutex::new(GitOidCache::new()),
            tree_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Borrow the underlying content-addressed store.
    pub const fn cas(&self) -> &Store {
        &self.cas
    }

    /// Store a byte slice into the persistent `blob` namespace (`NS_BLOB`),
    /// transparently FastCDC-chunking payloads `> 4 MiB`.
    pub fn put_bytes_blob(&self, bytes: &[u8]) -> Result<(Digest, u64), D6StoreError> {
        let mut cursor = bytes;
        let stored = self.cas.put_blob(NS_BLOB, &mut cursor)?;
        Ok((stored.digest, stored.size))
    }

    /// Store a workspace file opened `O_NOFOLLOW | O_NONBLOCK` into `NS_BLOB`.
    pub(crate) fn put_file_blob(&self, path: &Path) -> Result<(Digest, u64), D6StoreError> {
        let file = open_regular_nofollow(path)?;
        let mut reader = BufReader::new(file);
        let stored = self.cas.put_blob(NS_BLOB, &mut reader)?;
        Ok((stored.digest, stored.size))
    }

    /// Read and decode a canonical `spawn-tree-v1` object through the immutable in-memory cache.
    pub fn read_tree_cached(&self, digest: &Digest) -> Result<Tree, D6StoreError> {
        if let Some(cached) = self
            .tree_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(digest)
            .cloned()
        {
            return Ok(cached);
        }
        let tree = if *digest == empty_tree_digest() && !self.cas.has(digest) {
            Tree::default()
        } else {
            self.cas.read_tree(digest)?
        };
        let mut guard = self
            .tree_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.insert(digest.clone(), tree.clone());
        Ok(tree)
    }

    /// Put a canonical `spawn-tree-v1` node into `NS_TREE` and populate the tree cache.
    pub fn put_tree_cached(&self, tree: &Tree) -> Result<Digest, D6StoreError> {
        let digest = self.cas.put_tree(tree)?;
        let mut guard = self
            .tree_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.insert(digest.clone(), tree.clone());
        Ok(digest)
    }

    /// Project a tree digest to its deterministic 40-hex Git tree OID.
    pub fn project_tree_oid(&self, tree: &Digest) -> Result<GitOid, D6StoreError> {
        let mut cache = self
            .oid_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(project_tree_oid(&self.cas, tree, &mut cache)?)
    }

    /// Project a snapshot and parent digests to its deterministic 40-hex Git commit OID.
    pub fn project_snapshot_oid(
        &self,
        snapshot: &Snapshot,
        parent_oids: &[GitOid],
    ) -> Result<GitOid, D6StoreError> {
        let mut cache = self
            .oid_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(project_snapshot_oid(
            &self.cas,
            snapshot,
            parent_oids,
            &mut cache,
        )?)
    }

    /// Flatten a canonical `spawn-tree-v1` root into sorted `(path_bytes, TreeEntry)` leaf entries.
    pub fn flatten_tree(
        &self,
        root: &Digest,
    ) -> Result<BTreeMap<Vec<u8>, TreeEntry>, D6StoreError> {
        let mut out = BTreeMap::new();
        self.flatten_recursive(root, &[], &mut out)?;
        Ok(out)
    }

    fn flatten_recursive(
        &self,
        tree_digest: &Digest,
        prefix: &[u8],
        out: &mut BTreeMap<Vec<u8>, TreeEntry>,
    ) -> Result<(), D6StoreError> {
        let tree = self.read_tree_cached(tree_digest)?;
        for entry in tree.entries {
            let mut path = Vec::with_capacity(prefix.len() + 1 + entry.name.len());
            if !prefix.is_empty() {
                path.extend_from_slice(prefix);
                path.push(b'/');
            }
            path.extend_from_slice(&entry.name);
            if entry.kind == TreeEntryKind::Dir {
                let sub = entry
                    .digest()
                    .ok_or_else(|| D6StoreError::InvalidInput("dir without digest".to_string()))?;
                self.flatten_recursive(&sub, &path, out)?;
            } else {
                out.insert(path, entry);
            }
        }
        Ok(())
    }

    /// Apply leaf mutations in `O(|diff| + depth)` over persistent CAS trees.
    pub fn apply_mutations(
        &self,
        base_tree: &Digest,
        mutations: &[TreeMutation],
    ) -> Result<Digest, D6StoreError> {
        let next = self.cas.apply_tree_mutations(base_tree, mutations)?;
        let _ = self.read_tree_cached(&next)?;
        Ok(next)
    }

    /// Scan `workspace_dir` under `policy`, failing closed on malformed ignore files or special files.
    pub fn scan_workspace_tree(
        &self,
        workspace_dir: &Path,
        policy: &TrackedSetPolicy<'_>,
    ) -> Result<Vec<ScannedEntry>, D6StoreError> {
        let base_map = match policy.base_tree {
            Some(tree) => self.flatten_tree(tree)?,
            None => BTreeMap::new(),
        };
        let main_map = match policy.main_tree {
            Some(tree) => self.flatten_tree(tree)?,
            None => BTreeMap::new(),
        };
        let matcher = IgnoreMatcher::for_workspace(workspace_dir, &policy.extra_exclude_files)?;
        let mut scanner = WorkspaceScanner {
            base_map: &base_map,
            main_map: &main_map,
            matcher,
            entries: Vec::new(),
        };
        scanner.walk_dir(workspace_dir, b"", false)?;
        scanner
            .entries
            .sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        Ok(scanner.entries)
    }

    /// Check that a candidate tree contains zero untracked files/dirs matched by workspace
    /// `.gitignore`, `.git/info/exclude`, `BUILD_OUTPUT`, or `ENGINE_RUNTIME_FILES` (fail-closed).
    pub fn verify_no_ignored_untracked_leaks(
        &self,
        workspace_dir: &Path,
        candidate_tree: &Digest,
        policy: &TrackedSetPolicy<'_>,
    ) -> Result<(), D6StoreError> {
        let base_map = match policy.base_tree {
            Some(tree) => self.flatten_tree(tree)?,
            None => BTreeMap::new(),
        };
        let main_map = match policy.main_tree {
            Some(tree) => self.flatten_tree(tree)?,
            None => BTreeMap::new(),
        };
        let matcher = IgnoreMatcher::for_workspace(workspace_dir, &policy.extra_exclude_files)?;
        let candidate_map = self.flatten_tree(candidate_tree)?;
        for rel_bytes in candidate_map.keys() {
            let rel_str = String::from_utf8_lossy(rel_bytes).into_owned();
            if is_disallowed_git_path(rel_bytes) {
                return Err(D6StoreError::IgnoredLeak {
                    path: rel_str,
                    rule: ".git internal component".to_string(),
                });
            }
            let in_both = base_map.contains_key(rel_bytes) && main_map.contains_key(rel_bytes);
            if matches_engine_runtime(rel_bytes) && !in_both {
                return Err(D6StoreError::IgnoredLeak {
                    path: rel_str,
                    rule: "ENGINE_RUNTIME_FILES".to_string(),
                });
            }
            if matches_build_output(rel_bytes) && !in_both {
                return Err(D6StoreError::IgnoredLeak {
                    path: rel_str,
                    rule: "BUILD_OUTPUT".to_string(),
                });
            }
            if !base_map.contains_key(rel_bytes)
                && let Some(rule) =
                    matcher.check_with_ancestors_on_disk(workspace_dir, rel_bytes)?
            {
                return Err(D6StoreError::IgnoredLeak {
                    path: rel_str,
                    rule,
                });
            }
        }
        Ok(())
    }

    /// Compute minimal `O(|diff|)` [`TreeMutation`] operations transforming `base_tree` into the
    /// tracked-set view of `workspace_dir`, uploading only changed or new blobs.
    pub fn diff_workspace_mutations(
        &self,
        workspace_dir: &Path,
        policy: &TrackedSetPolicy<'_>,
    ) -> Result<Vec<TreeMutation>, D6StoreError> {
        let base_digest = policy.base_tree.cloned().unwrap_or_else(empty_tree_digest);
        let base_map = self.flatten_tree(&base_digest)?;
        let scanned = self.scan_workspace_tree(workspace_dir, policy)?;

        let mut seen: BTreeSet<Vec<u8>> = BTreeSet::new();
        let mut mutations = Vec::new();
        for item in scanned {
            let rel = item.path.as_bytes().to_vec();
            seen.insert(rel.clone());
            let disk_path = workspace_dir.join(item.path.as_path());
            let prev = base_map.get(&rel);

            match item.kind {
                ScannedKind::File | ScannedKind::Executable => {
                    let want_kind = if item.kind == ScannedKind::Executable {
                        TreeEntryKind::Executable
                    } else {
                        TreeEntryKind::File
                    };
                    let (digest, size) = self.hash_or_store_file(&disk_path, prev)?;
                    let same = prev.is_some_and(|e| {
                        e.kind == want_kind
                            && e.id == digest.as_bytes()
                            && e.size == size
                            && self.cas.has_blob(&digest)
                    });
                    if !same {
                        let stored = self.put_file_blob(&disk_path)?;
                        mutations.push(TreeMutation::Upsert {
                            path: split_path(&rel),
                            kind: want_kind,
                            id: stored.0.as_bytes(),
                            size: stored.1,
                        });
                    }
                }
                ScannedKind::Symlink => {
                    let target = fs::read_link(&disk_path)?;
                    let target_bytes = target.as_os_str().as_bytes();
                    let size = u64::try_from(target_bytes.len())
                        .map_err(|_| D6StoreError::InvalidInput("symlink overflow".to_string()))?;
                    let digest = sha256_of(target_bytes);
                    let same = prev.is_some_and(|e| {
                        e.kind == TreeEntryKind::Symlink
                            && e.id == digest.as_bytes()
                            && e.size == size
                            && self.cas.has_blob(&digest)
                    });
                    if !same {
                        let (stored_digest, stored_size) = self.put_bytes_blob(target_bytes)?;
                        mutations.push(TreeMutation::Upsert {
                            path: split_path(&rel),
                            kind: TreeEntryKind::Symlink,
                            id: stored_digest.as_bytes(),
                            size: stored_size,
                        });
                    }
                }
                ScannedKind::Gitlink { oid } => {
                    let entry = TreeEntry::gitlink(b"x".to_vec(), &oid)?;
                    let same = prev.is_some_and(|e| {
                        e.kind == TreeEntryKind::Gitlink && e.id == entry.id && e.size == entry.size
                    });
                    if !same {
                        mutations.push(TreeMutation::Upsert {
                            path: split_path(&rel),
                            kind: TreeEntryKind::Gitlink,
                            id: entry.id,
                            size: entry.size,
                        });
                    }
                }
            }
        }

        for existing_path in base_map.keys() {
            if !seen.contains(existing_path) && !has_ancestor_in(&seen, existing_path) {
                mutations.push(TreeMutation::Remove {
                    path: split_path(existing_path),
                });
            }
        }

        Ok(mutations)
    }

    fn hash_or_store_file(
        &self,
        disk_path: &Path,
        prev: Option<&TreeEntry>,
    ) -> Result<(Digest, u64), D6StoreError> {
        let mut file = open_regular_nofollow(disk_path)?;
        let size = file.metadata()?.len();
        if let Some(entry) = prev
            && entry.size == size
            && matches!(entry.kind, TreeEntryKind::File | TreeEntryKind::Executable)
        {
            let digest = sha256_reader(&mut file)?;
            return Ok((digest, size));
        }
        drop(file);
        self.put_file_blob(disk_path)
    }

    /// Snapshot `workspace_dir` relative to `policy` into CAS (`NS_BLOB` + `NS_TREE`),
    /// returning the new root tree digest and verifying zero ignored leaks.
    pub fn snapshot_workspace(
        &self,
        workspace_dir: &Path,
        policy: &TrackedSetPolicy<'_>,
    ) -> Result<Digest, D6StoreError> {
        let base_digest = policy.base_tree.cloned().unwrap_or_else(empty_tree_digest);
        let mutations = self.diff_workspace_mutations(workspace_dir, policy)?;
        let next_tree = self.apply_mutations(&base_digest, &mutations)?;
        self.verify_no_ignored_untracked_leaks(workspace_dir, &next_tree, policy)?;
        Ok(next_tree)
    }

    /// Execute the 4-step atomic D6 publish sequence (`#113` Fine Item 7.2):
    /// 1. Validate envelope & reject tombstoned `Spawn-Large:` trailer.
    /// 2. Write canonical `spawn-snapshot-v1` object & project deterministic Git OIDs.
    /// 3. **`Store::sync_store()`** durability barrier before any PG transaction.
    /// 4. Record snapshot + CAS/land ref in `RefLedger` (PostgreSQL).
    /// 5. Append and `fsync` entry to `<cas_root>/refs.journal`.
    pub fn publish_snapshot(
        &self,
        ledger: &dyn RefLedger,
        req: &PublishRequest<'_>,
    ) -> Result<PublishedSnapshot, D6StoreError> {
        if has_tombstoned_large_trailer(req.commit_body) {
            return Err(D6StoreError::InvalidInput(format!(
                "tombstoned {LARGE_FILE_TRAILER} commit trailer is forbidden in D6 snapshots"
            )));
        }

        let parent_oids = self.resolve_parent_oids(ledger, req.parents)?;
        let snapshot = Snapshot {
            tree: req.tree.clone(),
            parents: req.parents.to_vec(),
            commit_body: req.commit_body.to_vec(),
        };
        let snapshot_id = self.cas.put_merkle_snapshot(&snapshot)?;
        let tree_git_oid = self.project_tree_oid(req.tree)?;
        let git_oid = self.project_snapshot_oid(&snapshot, &parent_oids)?;

        // Durability barrier before publishing roots in PostgreSQL.
        self.cas.sync_store()?;

        self.record_and_advance_ref(ledger, req, &snapshot_id, &git_oid)
            .map(|ref_seq| PublishedSnapshot {
                snapshot: snapshot_id,
                tree: req.tree.clone(),
                git_oid,
                tree_git_oid,
                ref_seq,
            })
    }

    /// Resolve Git commit OIDs for an ordered parent slice using the ledger first,
    /// falling back recursively to [`Store::read_merkle_snapshot`].
    pub fn resolve_parent_oids(
        &self,
        ledger: &dyn RefLedger,
        parents: &[Digest],
    ) -> Result<Vec<GitOid>, D6StoreError> {
        let mut cache = HashMap::new();
        let mut out = Vec::with_capacity(parents.len());
        for parent in parents {
            out.push(self.resolve_snapshot_oid_cached(ledger, parent, &mut cache)?);
        }
        Ok(out)
    }

    fn resolve_snapshot_oid_cached(
        &self,
        ledger: &dyn RefLedger,
        id: &Digest,
        seen: &mut HashMap<Digest, GitOid>,
    ) -> Result<GitOid, D6StoreError> {
        if let Some(oid) = seen.get(id) {
            return Ok(*oid);
        }
        if let Some(row) = ledger.get_snapshot(id)? {
            seen.insert(id.clone(), row.git_oid);
            return Ok(row.git_oid);
        }
        let snap = self.cas.read_merkle_snapshot(id)?;
        let mut parent_oids = Vec::with_capacity(snap.parents.len());
        for parent in &snap.parents {
            parent_oids.push(self.resolve_snapshot_oid_cached(ledger, parent, seen)?);
        }
        let oid = self.project_snapshot_oid(&snap, &parent_oids)?;
        seen.insert(id.clone(), oid);
        Ok(oid)
    }

    fn record_and_advance_ref(
        &self,
        ledger: &dyn RefLedger,
        req: &PublishRequest<'_>,
        snapshot_id: &Digest,
        git_oid: &GitOid,
    ) -> Result<u64, D6StoreError> {
        let _ = ledger.record_snapshot(
            snapshot_id,
            git_oid,
            req.tree,
            req.parents,
            req.project,
            req.created_at_ms,
        )?;
        let ref_seq = match req.kind {
            D6RefKind::Main => ledger.land_main_ref(
                req.project,
                req.ref_name,
                req.expected_ref,
                snapshot_id,
                req.landing_task_id,
                req.reason,
            )?,
            D6RefKind::Branch | D6RefKind::Wip | D6RefKind::Tag => ledger.cas_ref(
                req.project,
                req.ref_name,
                req.kind,
                req.expected_ref,
                snapshot_id,
                req.reason,
            )?,
        };
        let entry = JournalEntry {
            seq: ref_seq,
            project: req.project.to_string(),
            name: req.ref_name.to_string(),
            kind: req.kind.as_str().to_string(),
            old_snapshot: req.expected_ref.cloned(),
            new_snapshot: snapshot_id.clone(),
            git_oid: *git_oid,
            at_ms: req.created_at_ms,
        };
        self.cas.append_ref_journal(&entry)?;
        Ok(ref_seq)
    }

    /// Materialize a canonical `spawn-tree-v1` root into `dest_dir` with proper
    /// `0644` / `0755` permissions and optional `prev_layer` hard-link reuse.
    ///
    /// **Safety rule (D6 Safeguard #4)**: NEVER creates hard links against `0444`
    /// objects inside `cas.root()`; `prev_layer` inside `cas.root()` is refused.
    pub fn materialize_tree(
        &self,
        tree_digest: &Digest,
        dest_dir: &Path,
        prev_layer: Option<&Path>,
        prev_tree: Option<&Digest>,
    ) -> Result<MaterializeReport, D6StoreError> {
        let cas_canon = canonical_root(self.cas.root());
        let validated_prev = match prev_layer {
            Some(prev) => {
                let meta = fs::symlink_metadata(prev)?;
                if !meta.is_dir() {
                    return Err(D6StoreError::InvalidInput(
                        "prev_layer must be a real directory".to_string(),
                    ));
                }
                let prev_canon = canonical_root(prev);
                if prev_canon == cas_canon || prev_canon.starts_with(&cas_canon) {
                    return Err(D6StoreError::InvalidInput(
                        "prev_layer must not reside inside CAS root".to_string(),
                    ));
                }
                Some(prev_canon)
            }
            None => None,
        };

        if let Ok(dest_meta) = fs::symlink_metadata(dest_dir)
            && dest_meta.file_type().is_symlink()
        {
            return Err(D6StoreError::InvalidInput(
                "destination directory cannot be a symlink".to_string(),
            ));
        }
        fs::create_dir_all(dest_dir)?;
        let dest_canon = canonical_root(dest_dir);
        if dest_canon == cas_canon || dest_canon.starts_with(&cas_canon) {
            return Err(D6StoreError::InvalidInput(
                "destination directory must not reside inside CAS root".to_string(),
            ));
        }

        let prev_entries = match prev_tree {
            Some(digest) => Some(self.flatten_tree(digest)?),
            None => None,
        };
        let mut report = MaterializeReport::default();
        self.materialize_recursive(
            tree_digest,
            &dest_canon,
            b"",
            validated_prev.as_deref(),
            prev_entries.as_ref(),
            &mut report,
        )?;
        Ok(report)
    }

    fn materialize_recursive(
        &self,
        tree_digest: &Digest,
        dest_root: &Path,
        prefix: &[u8],
        prev_layer: Option<&Path>,
        prev_entries: Option<&BTreeMap<Vec<u8>, TreeEntry>>,
        report: &mut MaterializeReport,
    ) -> Result<(), D6StoreError> {
        let tree = self.read_tree_cached(tree_digest)?;
        for entry in tree.entries {
            if !is_valid_entry_name(&entry.name) {
                return Err(D6StoreError::InvalidInput(
                    "invalid tree entry name during materialization".to_string(),
                ));
            }
            let rel_bytes = join_rel(prefix, &entry.name);
            let rel_path = Path::new(OsStr::from_bytes(&rel_bytes));
            let target_path = dest_root.join(rel_path);

            match entry.kind {
                TreeEntryKind::Dir => {
                    if let Ok(m) = fs::symlink_metadata(&target_path)
                        && (m.file_type().is_symlink() || !m.is_dir())
                    {
                        remove_existing_entry(&target_path)?;
                    }
                    fs::create_dir_all(&target_path)?;
                    fs::set_permissions(&target_path, fs::Permissions::from_mode(0o755))?;
                    report.dirs_created += 1;
                    let sub_digest = entry.digest().ok_or_else(|| {
                        D6StoreError::InvalidInput("dir entry missing digest".to_string())
                    })?;
                    self.materialize_recursive(
                        &sub_digest,
                        dest_root,
                        &rel_bytes,
                        prev_layer,
                        prev_entries,
                        report,
                    )?;
                }
                TreeEntryKind::File | TreeEntryKind::Executable => {
                    let mode = if entry.kind == TreeEntryKind::Executable {
                        0o755
                    } else {
                        0o644
                    };
                    let blob_digest = entry.digest().ok_or_else(|| {
                        D6StoreError::InvalidInput("file entry missing digest".to_string())
                    })?;
                    if try_hardlink_from_prev(
                        &self.cas,
                        prev_layer,
                        prev_entries,
                        &rel_bytes,
                        rel_path,
                        &target_path,
                        &entry,
                        &blob_digest,
                        mode,
                    )? {
                        report.hardlinked_from_prev += 1;
                    } else {
                        self.write_fresh_blob_file(&target_path, &blob_digest, entry.size, mode)?;
                        report.files_written += 1;
                    }
                }
                TreeEntryKind::Symlink => {
                    let blob_digest = entry.digest().ok_or_else(|| {
                        D6StoreError::InvalidInput("symlink entry missing digest".to_string())
                    })?;
                    let target_bytes = self.cas.read_blob(&blob_digest)?;
                    if u64::try_from(target_bytes.len()).ok() != Some(entry.size) {
                        return Err(D6StoreError::InvalidInput(
                            "symlink target length mismatches entry size".to_string(),
                        ));
                    }
                    remove_existing_entry(&target_path)?;
                    symlink(OsStr::from_bytes(&target_bytes), &target_path)?;
                    report.symlinks_written += 1;
                }
                TreeEntryKind::Gitlink => {
                    if let Ok(m) = fs::symlink_metadata(&target_path)
                        && (m.file_type().is_symlink() || !m.is_dir())
                    {
                        remove_existing_entry(&target_path)?;
                    }
                    fs::create_dir_all(&target_path)?;
                    fs::set_permissions(&target_path, fs::Permissions::from_mode(0o755))?;
                    report.gitlinks_skipped += 1;
                }
            }
        }
        Ok(())
    }

    fn write_fresh_blob_file(
        &self,
        target_path: &Path,
        blob_digest: &Digest,
        expected_size: u64,
        mode: u32,
    ) -> Result<(), D6StoreError> {
        remove_existing_entry(target_path)?;
        let mut out = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(target_path)?;
        let write_res = self.cas.stream_blob(blob_digest, &mut out);
        match write_res {
            Ok(written) if written == expected_size => {
                out.set_permissions(fs::Permissions::from_mode(mode))?;
                Ok(())
            }
            Ok(written) => {
                drop(out);
                let _ = fs::remove_file(target_path);
                Err(D6StoreError::InvalidInput(format!(
                    "blob {blob_digest} wrote {written} bytes, expected {expected_size}"
                )))
            }
            Err(err) => {
                drop(out);
                let _ = fs::remove_file(target_path);
                Err(err.into())
            }
        }
    }
}

#[expect(clippy::too_many_arguments)]
fn try_hardlink_from_prev(
    cas: &Store,
    prev_layer: Option<&Path>,
    prev_entries: Option<&BTreeMap<Vec<u8>, TreeEntry>>,
    rel_bytes: &[u8],
    rel_path: &Path,
    target_path: &Path,
    entry: &TreeEntry,
    blob_digest: &Digest,
    want_mode: u32,
) -> Result<bool, D6StoreError> {
    let Some(prev_root) = prev_layer else {
        return Ok(false);
    };
    if let Some(prev_map) = prev_entries
        && prev_map.get(rel_bytes) != Some(entry)
    {
        return Ok(false);
    }
    let prev_file = prev_root.join(rel_path);
    let Some(parent) = prev_file.parent() else {
        return Ok(false);
    };
    if !canonical_root(parent).starts_with(prev_root) {
        return Ok(false);
    }
    let Ok(meta) = fs::symlink_metadata(&prev_file) else {
        return Ok(false);
    };
    if !meta.file_type().is_file() || meta.len() != entry.size {
        return Ok(false);
    }
    if meta.permissions().mode() & 0o777 != want_mode {
        return Ok(false);
    }
    if let Ok(cas_meta) = fs::symlink_metadata(cas.object_path(blob_digest))
        && cas_meta.dev() == meta.dev()
        && cas_meta.ino() == meta.ino()
    {
        return Ok(false);
    }
    let mut handle = open_regular_nofollow(&prev_file)?;
    if sha256_reader(&mut handle)? != *blob_digest {
        return Ok(false);
    }
    drop(handle);
    remove_existing_entry(target_path)?;
    fs::hard_link(&prev_file, target_path)?;
    Ok(true)
}

/// Reject tombstoned `Spawn-Large:` trailers while leaving legacy `Spawn-Large` references out.
fn has_tombstoned_large_trailer(commit_body: &[u8]) -> bool {
    let prefix = format!("{LARGE_FILE_TRAILER}:");
    let text = String::from_utf8_lossy(commit_body);
    text.lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed.len() >= prefix.len() && trimmed[..prefix.len()].eq_ignore_ascii_case(&prefix)
    })
}

fn open_regular_nofollow(path: &Path) -> Result<fs::File, D6StoreError> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(|err| {
            if err.raw_os_error() == Some(libc::ELOOP) {
                D6StoreError::InvalidInput(format!("{} is a symlink", path.display()))
            } else {
                D6StoreError::Io(err)
            }
        })?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(D6StoreError::InvalidInput(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    Ok(file)
}

fn remove_existing_entry(path: &Path) -> Result<(), D6StoreError> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.is_dir() && !meta.file_type().is_symlink() {
                fs::remove_dir_all(path)?;
            } else {
                fs::remove_file(path)?;
            }
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

fn join_rel(prefix: &[u8], name: &[u8]) -> Vec<u8> {
    if prefix.is_empty() {
        name.to_vec()
    } else {
        let mut out = Vec::with_capacity(prefix.len() + 1 + name.len());
        out.extend_from_slice(prefix);
        out.push(b'/');
        out.extend_from_slice(name);
        out
    }
}

fn split_path(rel_path: &[u8]) -> Vec<Vec<u8>> {
    rel_path.split(|&b| b == b'/').map(<[u8]>::to_vec).collect()
}

fn is_disallowed_git_path(rel_bytes: &[u8]) -> bool {
    rel_bytes
        .split(|&b| b == b'/')
        .any(|comp| !is_valid_entry_name(comp))
}

fn matches_build_output(rel_bytes: &[u8]) -> bool {
    BUILD_OUTPUT.iter().any(|root_dir| {
        let pat = root_dir.as_bytes();
        rel_bytes == pat || (rel_bytes.starts_with(pat) && rel_bytes.get(pat.len()) == Some(&b'/'))
    })
}

fn matches_engine_runtime(rel_bytes: &[u8]) -> bool {
    ENGINE_RUNTIME_FILES.iter().any(|entry| {
        let pat = entry.strip_suffix('/').unwrap_or(entry).as_bytes();
        rel_bytes == pat
            || (rel_bytes.starts_with(pat) && rel_bytes.get(pat.len()) == Some(&b'/'))
            || contains_subpath(rel_bytes, pat)
    })
}

fn contains_subpath(rel_bytes: &[u8], pat: &[u8]) -> bool {
    let mut offset = 0;
    while let Some(pos) = rel_bytes[offset..].iter().position(|&b| b == b'/') {
        let tail = &rel_bytes[offset + pos + 1..];
        if tail == pat || (tail.starts_with(pat) && tail.get(pat.len()) == Some(&b'/')) {
            return true;
        }
        offset += pos + 1;
    }
    false
}

struct WorkspaceScanner<'a> {
    base_map: &'a BTreeMap<Vec<u8>, TreeEntry>,
    main_map: &'a BTreeMap<Vec<u8>, TreeEntry>,
    matcher: IgnoreMatcher,
    entries: Vec<ScannedEntry>,
}

impl WorkspaceScanner<'_> {
    fn walk_dir(
        &mut self,
        dir: &Path,
        rel_dir: &[u8],
        parent_ignored: bool,
    ) -> Result<(), D6StoreError> {
        let pushed = if parent_ignored {
            false
        } else {
            self.matcher.push_dir_gitignore(dir, rel_dir)?
        };

        let mut dir_entries = Vec::new();
        for entry_res in fs::read_dir(dir)? {
            let entry = entry_res?;
            dir_entries.push(entry);
        }
        dir_entries.sort_by_key(fs::DirEntry::file_name);

        for entry in dir_entries {
            let name_os = entry.file_name();
            let name = name_os.as_bytes();
            if !is_valid_entry_name(name) {
                continue;
            }
            let rel_path = join_rel(rel_dir, name);
            let meta = fs::symlink_metadata(entry.path())?;
            let ftype = meta.file_type();

            if ftype.is_dir() {
                if let Some(TreeEntry {
                    kind: TreeEntryKind::Gitlink,
                    ..
                }) = self.base_map.get(&rel_path)
                {
                    let base_entry = &self.base_map[&rel_path];
                    if let Some(oid) = base_entry.gitlink_oid() {
                        self.entries.push(ScannedEntry {
                            path: TreePath::from_bytes(rel_path),
                            kind: ScannedKind::Gitlink { oid: oid.to_vec() },
                        });
                    }
                    continue;
                }

                let in_both_dir = has_descendant(self.base_map, &rel_path)
                    && has_descendant(self.main_map, &rel_path);
                if (matches_engine_runtime(&rel_path) || matches_build_output(&rel_path))
                    && !in_both_dir
                {
                    continue;
                }
                let dir_ignored =
                    parent_ignored || self.matcher.is_ignored(&rel_path, true).is_some();
                if dir_ignored && !has_descendant(self.base_map, &rel_path) {
                    continue;
                }
                self.walk_dir(&entry.path(), &rel_path, dir_ignored)?;
            } else if ftype.is_symlink() {
                let in_base = self.base_map.contains_key(&rel_path);
                let in_both = in_base && self.main_map.contains_key(&rel_path);
                if self.should_skip_leaf(&rel_path, in_base, in_both, parent_ignored) {
                    continue;
                }
                self.entries.push(ScannedEntry {
                    path: TreePath::from_bytes(rel_path),
                    kind: ScannedKind::Symlink,
                });
            } else if ftype.is_file() {
                let in_base = self.base_map.contains_key(&rel_path);
                let in_both = in_base && self.main_map.contains_key(&rel_path);
                if self.should_skip_leaf(&rel_path, in_base, in_both, parent_ignored) {
                    continue;
                }
                let kind = if meta.permissions().mode() & 0o111 != 0 {
                    ScannedKind::Executable
                } else {
                    ScannedKind::File
                };
                self.entries.push(ScannedEntry {
                    path: TreePath::from_bytes(rel_path),
                    kind,
                });
            } else {
                return Err(D6StoreError::InvalidInput(format!(
                    "unexpected non-regular filesystem object at {}",
                    entry.path().display()
                )));
            }
        }

        if pushed {
            self.matcher.pop();
        }
        Ok(())
    }

    fn should_skip_leaf(
        &self,
        rel_path: &[u8],
        in_base: bool,
        in_both: bool,
        parent_ignored: bool,
    ) -> bool {
        if (matches_engine_runtime(rel_path) || matches_build_output(rel_path)) && !in_both {
            return true;
        }
        if in_base {
            return false;
        }
        parent_ignored || self.matcher.is_ignored(rel_path, false).is_some()
    }
}

fn has_ancestor_in(seen: &BTreeSet<Vec<u8>>, rel_path: &[u8]) -> bool {
    let mut offset = 0;
    while let Some(pos) = rel_path[offset..].iter().position(|&b| b == b'/') {
        let cut = offset + pos;
        if seen.contains(&rel_path[..cut]) {
            return true;
        }
        offset = cut + 1;
    }
    false
}

fn has_descendant(map: &BTreeMap<Vec<u8>, TreeEntry>, rel_dir: &[u8]) -> bool {
    let mut prefix = Vec::with_capacity(rel_dir.len() + 1);
    prefix.extend_from_slice(rel_dir);
    prefix.push(b'/');
    map.range(prefix.clone()..)
        .next()
        .is_some_and(|(k, _)| k.starts_with(&prefix))
}

#[derive(Debug, Clone)]
struct IgnoreLayer {
    base_dir: Vec<u8>,
    source_name: String,
    rules: Vec<IgnoreRule>,
}

#[derive(Debug, Clone)]
struct IgnoreRule {
    raw: String,
    negated: bool,
    dir_only: bool,
    anchored: bool,
    segments: Vec<Vec<u8>>,
}

struct IgnoreMatcher {
    layers: Vec<IgnoreLayer>,
}

impl IgnoreMatcher {
    fn for_workspace(
        workspace_dir: &Path,
        extra_excludes: &[PathBuf],
    ) -> Result<Self, D6StoreError> {
        let mut layers = Vec::new();
        for extra in extra_excludes {
            if let Some(layer) = load_optional_ignore_file(extra, b"")? {
                layers.push(layer);
            }
        }
        let dot_git = workspace_dir.join(".git");
        match fs::symlink_metadata(&dot_git) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(D6StoreError::InvalidInput(format!(
                        "{} cannot be a symlink",
                        dot_git.display()
                    )));
                }
                if meta.is_dir() {
                    let info_exclude = dot_git.join("info/exclude");
                    if let Some(layer) = load_optional_ignore_file(&info_exclude, b"")? {
                        layers.push(layer);
                    }
                }
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        Ok(Self { layers })
    }

    fn push_dir_gitignore(&mut self, dir: &Path, rel_dir: &[u8]) -> Result<bool, D6StoreError> {
        let path = dir.join(".gitignore");
        if let Some(layer) = load_optional_ignore_file(&path, rel_dir)? {
            self.layers.push(layer);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn pop(&mut self) {
        let _ = self.layers.pop();
    }

    fn is_ignored(&self, rel_path: &[u8], is_dir: bool) -> Option<String> {
        let mut matched: Option<(bool, String)> = None;
        for layer in &self.layers {
            let scoped = if layer.base_dir.is_empty() {
                rel_path
            } else if let Some(after) = rel_path.strip_prefix(layer.base_dir.as_slice())
                && let Some(rest) = after.strip_prefix(b"/")
            {
                rest
            } else {
                continue;
            };
            for rule in &layer.rules {
                if rule.matches(scoped, is_dir) {
                    matched = Some((!rule.negated, format!("{}:{}", layer.source_name, rule.raw)));
                }
            }
        }
        match matched {
            Some((true, desc)) => Some(desc),
            _ => None,
        }
    }

    fn check_with_ancestors_on_disk(
        &self,
        workspace_dir: &Path,
        rel_path: &[u8],
    ) -> Result<Option<String>, D6StoreError> {
        // Load all nested `.gitignore` layers along the path's directory prefixes.
        let components = split_path(rel_path);
        let mut local = IgnoreMatcher {
            layers: self.layers.clone(),
        };
        let mut cur_dir = workspace_dir.to_path_buf();
        let mut cur_rel = Vec::new();
        local.push_dir_gitignore(&cur_dir, b"")?;
        for idx in 0..components.len() {
            let is_leaf = idx + 1 == components.len();
            let prefix = join_rel(&cur_rel, &components[idx]);
            if !is_leaf {
                if let Some(rule) = local.is_ignored(&prefix, true) {
                    return Ok(Some(rule));
                }
                cur_dir = cur_dir.join(OsStr::from_bytes(&components[idx]));
                cur_rel = prefix;
                if fs::symlink_metadata(&cur_dir).is_ok_and(|m| m.is_dir()) {
                    local.push_dir_gitignore(&cur_dir, &cur_rel)?;
                }
            } else if let Some(rule) = local.is_ignored(&prefix, false) {
                return Ok(Some(rule));
            }
        }
        Ok(None)
    }
}

fn load_optional_ignore_file(
    path: &Path,
    base_dir: &[u8],
) -> Result<Option<IgnoreLayer>, D6StoreError> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_file() {
                return Err(D6StoreError::InvalidInput(format!(
                    "ignore file {} must be a plain non-symlink file",
                    path.display()
                )));
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    }
    let mut file = open_regular_nofollow(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let mut rules = Vec::new();
    for raw_line in bytes.split(|&b| b == b'\n') {
        let line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
        if let Some(rule) = parse_ignore_line(line, path)? {
            rules.push(rule);
        }
    }
    Ok(Some(IgnoreLayer {
        base_dir: base_dir.to_vec(),
        source_name: path.display().to_string(),
        rules,
    }))
}

fn parse_ignore_line(raw: &[u8], source: &Path) -> Result<Option<IgnoreRule>, D6StoreError> {
    if raw.is_empty() || raw[0] == b'#' {
        return Ok(None);
    }
    let raw_str = String::from_utf8_lossy(raw).into_owned();
    let trimmed = strip_unescaped_trailing_spaces(raw);
    if trimmed.is_empty() {
        return Ok(None);
    }
    let (negated, mut pat) = if trimmed[0] == b'!' {
        (true, trimmed[1..].to_vec())
    } else {
        (false, trimmed)
    };
    if pat.is_empty() {
        return Ok(None);
    }
    let dir_only = pat.ends_with(b"/");
    if dir_only {
        pat.pop();
    }
    if pat.is_empty() {
        return Ok(None);
    }
    let anchored = if let Some(stripped) = pat.strip_prefix(b"/") {
        pat = stripped.to_vec();
        true
    } else {
        pat.contains(&b'/')
    };
    if pat.is_empty() {
        return Ok(None);
    }
    let mut segments = Vec::new();
    for seg in pat.split(|&b| b == b'/') {
        validate_glob_segment(seg, source)?;
        segments.push(seg.to_vec());
    }
    Ok(Some(IgnoreRule {
        raw: raw_str,
        negated,
        dir_only,
        anchored,
        segments,
    }))
}

fn strip_unescaped_trailing_spaces(line: &[u8]) -> Vec<u8> {
    let mut end = line.len();
    while end > 0 && line[end - 1] == b' ' {
        if end >= 2 && line[end - 2] == b'\\' {
            break;
        }
        end -= 1;
    }
    let slice = &line[..end];
    if slice.iter().all(|&b| b == b' ') {
        return Vec::new();
    }
    slice.to_vec()
}

fn validate_glob_segment(seg: &[u8], source: &Path) -> Result<(), D6StoreError> {
    let mut i = 0;
    while i < seg.len() {
        match seg[i] {
            b'\\' => {
                if i + 1 >= seg.len() {
                    return Err(D6StoreError::InvalidInput(format!(
                        "dangling escape in {}",
                        source.display()
                    )));
                }
                i += 2;
            }
            b'[' => {
                let mut j = i + 1;
                if j < seg.len() && matches!(seg[j], b'!' | b'^') {
                    j += 1;
                }
                if j < seg.len() && seg[j] == b']' {
                    j += 1;
                }
                let mut closed = false;
                while j < seg.len() {
                    if seg[j] == b'[' && seg.get(j + 1) == Some(&b':') {
                        return Err(D6StoreError::InvalidInput(format!(
                            "unsupported POSIX character class in {}",
                            source.display()
                        )));
                    }
                    if seg[j] == b']' {
                        closed = true;
                        i = j + 1;
                        break;
                    }
                    j += 1;
                }
                if !closed {
                    return Err(D6StoreError::InvalidInput(format!(
                        "unclosed character class in {}",
                        source.display()
                    )));
                }
            }
            _ => i += 1,
        }
    }
    Ok(())
}

impl IgnoreRule {
    fn matches(&self, scoped_rel: &[u8], is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        if !self.anchored && self.segments.len() == 1 {
            let basename = scoped_rel
                .rsplit(|&b| b == b'/')
                .next()
                .unwrap_or(scoped_rel);
            return match_segment(&self.segments[0], basename);
        }
        let path_segs: Vec<&[u8]> = scoped_rel.split(|&b| b == b'/').collect();
        match_segments(&self.segments, &path_segs)
    }
}

fn match_segments(pat: &[Vec<u8>], path: &[&[u8]]) -> bool {
    if pat.is_empty() {
        return path.is_empty();
    }
    if pat[0].as_slice() == b"**" {
        if pat.len() == 1 {
            return !path.is_empty();
        }
        for split in 0..=path.len() {
            if match_segments(&pat[1..], &path[split..]) {
                return true;
            }
        }
        return false;
    }
    if path.is_empty() {
        return false;
    }
    match_segment(&pat[0], path[0]) && match_segments(&pat[1..], &path[1..])
}

fn match_segment(pat: &[u8], text: &[u8]) -> bool {
    let mut pi = 0;
    let mut ti = 0;
    let mut wildcard_pat_idx: Option<usize> = None;
    let mut backtrack_text_idx = 0;

    while ti < text.len() {
        if pi < pat.len() && pat[pi] == b'*' {
            wildcard_pat_idx = Some(pi);
            pi += 1;
            backtrack_text_idx = ti;
        } else if let Some(next_pi) = match_single_token(pat, pi, text[ti]) {
            pi = next_pi;
            ti += 1;
        } else if let Some(sp) = wildcard_pat_idx {
            pi = sp + 1;
            backtrack_text_idx += 1;
            ti = backtrack_text_idx;
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }
    pi == pat.len()
}

fn match_single_token(pat: &[u8], pi: usize, ch: u8) -> Option<usize> {
    if pi >= pat.len() {
        return None;
    }
    match pat[pi] {
        b'?' => Some(pi + 1),
        b'\\' => (pi + 1 < pat.len() && pat[pi + 1] == ch).then_some(pi + 2),
        b'[' => match_char_class(pat, pi, ch),
        other => (other == ch).then_some(pi + 1),
    }
}

fn match_char_class(pat: &[u8], start: usize, ch: u8) -> Option<usize> {
    let mut i = start + 1;
    let mut negated = false;
    if i < pat.len() && matches!(pat[i], b'!' | b'^') {
        negated = true;
        i += 1;
    }
    let mut hit = false;
    let mut first = true;
    while i < pat.len() {
        if pat[i] == b']' && !first {
            let ok = if negated { !hit } else { hit };
            return ok.then_some(i + 1);
        }
        first = false;
        let lo = pat[i];
        if i + 2 < pat.len() && pat[i + 1] == b'-' && pat[i + 2] != b']' {
            let hi = pat[i + 2];
            if lo <= ch && ch <= hi {
                hit = true;
            }
            i += 3;
        } else {
            if ch == lo {
                hit = true;
            }
            i += 1;
        }
    }
    None
}

/// Inspect pointer blob cap constant alignment (`D6 Fine Item 7.3`).
pub(crate) fn is_pointer_sized(entry: &TreeEntry) -> bool {
    entry.kind != TreeEntryKind::Symlink
        && entry.kind != TreeEntryKind::Dir
        && entry.kind != TreeEntryKind::Gitlink
        && entry.size > GIT_POINTER_THRESHOLD
}
