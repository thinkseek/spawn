//! D6 Phase 4 Step 5 (`#113` Comment #5 & `#95`):
//! Pure-Rust [`D6Workspace`] adapter implementing [`spawn_core::port::WorkspaceStore`]
//! over `CasMerkleStore`, `d6_layer0`, `merkle_merge`, and a generic `L: RefLedger`.
//!
//! Safety invariants:
//! - Strictly `0` forbidden OS/FFI code (`forbid` lint enforced at workspace level).
//! - Strictly `0` database driver imports (depends only on [`RefLedger`]).
//! - Strictly **ZERO `git` subprocess invocations** (`Command` is never imported or called).
//! - Warm Layer 0 baselines under `<root>/.d6/layers/<tree_hex>` never hard-link against
//!   `<cas>/objects`, and mounted writable workspaces are isolated via `clone_or_copy`.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

pub(crate) type ScannedWorkspaceTree = (Digest, Vec<(TreePath, u64)>, Option<Digest>);
type FilteredScan = (Vec<ScannedEntry>, Vec<(TreePath, u64)>);

use spawn_cas::{
    Algo, Digest, GitOid, JournalEntry, Snapshot, TreeEntry, TreeEntryKind, TreeMutation,
    empty_tree_digest, is_valid_entry_name, sha256_of, sha256_reader,
};
use spawn_core::event::Fork;
use spawn_core::id::{BranchName, CommitId, WorkspaceId};
use spawn_core::port::{
    BaseMerge, Branch, Commit, ForkFrom, Hold, MERGE_WAITS, MOUNT_CANCELLED, SetAside,
    WORKSPACE_STALE, WorkspaceError, WorkspaceStore,
};

use crate::agent_files::{AgentFileError, open_regular, read_regular, rename_within, replace_with};
use crate::cas_merkle::{
    BUILD_OUTPUT as D6_BUILD_OUTPUT, CasMerkleStore, D6RefKind, D6StoreError, LedgerSnapshot,
    PublishRequest, PublishedSnapshot, RefLedger, ScannedEntry, ScannedKind, TrackedSetPolicy,
    format_commit_body,
};
use crate::cow::clone_or_copy;
use crate::d6_layer0::{
    ExistingDestPolicy, Layer0MaterializeReport, Layer0Options, materialize_layer0,
};
use crate::git::{
    CONFLICT_MARKER_SIZE, ENGINE_RUNTIME_FILES, LARGE_FILE_TRAILER, MAIN_COPIES, SET_ASIDE_FILES,
    SET_ASIDE_WAITS, has_conflict_block_sized, lock_file, max_file_bytes, names_files_outside_git,
};
use crate::merkle_merge::{
    ConflictKind, GitAttributes, MergeBaseError, MergeSnapshotSpec, build_merge_snapshot,
    find_single_merge_base, merge_snapshots,
};
use crate::root::{canonical_root, legacy_workspace};
use crate::tree_path::{TreePath, nul_fields};

const BASE_BRANCH: &str = "main";
const D6_META_DIR: &str = ".d6";
const LAYERS_SUBDIR: &str = "layers";
const LOCKS_SUBDIR: &str = "locks";
const MOUNT_SUBDIR: &str = "mount";
const NEAREST_POINTER_FILE: &str = ".nearest";
const PENDING_TAG: &str = "pending-merge";
const COPIES_TAG: &str = "main-copies";
const OVERSIZED_RECORD_LIMIT: u64 = 64 << 20;
const CONFLICT_READ_LIMIT: u64 = 64 << 20;

static D6_STAGE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Pure-Rust CAS + transactional [`RefLedger`] workspace backend (`#113` Phase 4 Step 5).
///
/// Implements [`WorkspaceStore`] with zero `git` subprocess calls, warm incremental
/// Layer 0 caching under `<root>/.d6/layers/<tree_hex>`, per-branch `flock` locking,
/// fail-closed `TrackedSetPolicy` scanning, pure-Rust three-way Merkle merges, and
/// `sync_store()` -> `RefLedger` -> `refs.journal` crash-safe publishing.
pub struct D6Workspace<L: RefLedger> {
    project: String,
    root: PathBuf,
    store: Arc<CasMerkleStore>,
    ledger: Arc<L>,
    max_file_bytes: u64,
    author_name: String,
    author_email: String,
    extra_exclude_files: Vec<PathBuf>,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    last_layer0: Mutex<Option<Layer0MaterializeReport>>,
}

impl<L: RefLedger> D6Workspace<L> {
    /// Construct a `D6Workspace` backed by `store` and `ledger` with workspaces rooted at `root`.
    pub fn new(
        project: impl Into<String>,
        root: impl AsRef<Path>,
        store: Arc<CasMerkleStore>,
        ledger: Arc<L>,
    ) -> Self {
        Self {
            project: project.into(),
            root: canonical_root(root.as_ref()),
            store,
            ledger,
            max_file_bytes: max_file_bytes(std::env::var("SPAWN_MAX_FILE_MB").ok().as_deref()),
            author_name: "spawn".to_string(),
            author_email: "spawn@localhost".to_string(),
            extra_exclude_files: Vec::new(),
            clock: Arc::new(system_now_ms),
            last_layer0: Mutex::new(None),
        }
    }

    /// Open a CAS store at `cas_root` and construct a [`D6Workspace`].
    pub fn open(
        project: impl Into<String>,
        root: impl AsRef<Path>,
        cas_root: impl AsRef<Path>,
        ledger: Arc<L>,
    ) -> Result<Self, D6StoreError> {
        let store = Arc::new(CasMerkleStore::open(cas_root.as_ref().to_path_buf())?);
        Ok(Self::new(project, root, store, ledger))
    }

    /// Override the per-file size limit (in bytes) above which untracked files are left out
    /// of checkpoints and surfaced via [`WorkspaceStore::notices`].
    #[must_use]
    pub fn with_max_file_bytes(mut self, bytes: u64) -> Self {
        self.max_file_bytes = bytes;
        self
    }

    /// Override the author/committer name and email used for synthetic commit envelopes.
    #[must_use]
    pub fn with_identity(mut self, name: impl Into<String>, email: impl Into<String>) -> Self {
        self.author_name = name.into();
        self.author_email = email.into();
        self
    }

    /// Configure extra ignore files evaluated alongside `.gitignore` during checkpoint scans.
    #[must_use]
    pub fn with_extra_exclude_files(mut self, files: Vec<PathBuf>) -> Self {
        self.extra_exclude_files = files;
        self
    }

    /// Override the millisecond wall-clock function used for commit timestamps and journal entries.
    #[must_use]
    pub fn with_clock_ms(mut self, clock: Arc<dyn Fn() -> i64 + Send + Sync>) -> Self {
        self.clock = clock;
        self
    }

    /// Canonicalized workspace root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Configured project identifier.
    pub fn project_name(&self) -> &str {
        &self.project
    }

    /// Shared content-addressed Merkle store.
    pub const fn store(&self) -> &Arc<CasMerkleStore> {
        &self.store
    }

    /// Shared transactional snapshot/ref ledger.
    pub const fn ledger(&self) -> &Arc<L> {
        &self.ledger
    }

    /// Directory holding immutable warm Layer 0 baseline trees (`<root>/.d6/layers`).
    pub fn layers_dir(&self) -> PathBuf {
        self.root.join(D6_META_DIR).join(LAYERS_SUBDIR)
    }

    /// Warm Layer 0 cache path (`<root>/.d6/layers/<tree_hex>`) for `tree`.
    pub fn layer_path(&self, tree: &Digest) -> PathBuf {
        self.layers_dir().join(tree.hex())
    }

    /// Effective checkpoint single-file byte cap.
    pub const fn max_file_bytes(&self) -> u64 {
        self.max_file_bytes
    }

    /// Extra exclude files evaluated by workspace scans.
    pub fn extra_exclude_files(&self) -> &[PathBuf] {
        &self.extra_exclude_files
    }

    /// Most recent Layer 0 materialization report recorded during `mount` / `ensure_warm_layer`.
    pub fn last_layer0_report(&self) -> Option<Layer0MaterializeReport> {
        self.last_layer0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Whether a merge of `main` into `branch` is waiting on conflict marker resolution.
    pub fn has_pending_merge(&self, branch: &str) -> bool {
        self.pending_merge(branch).is_some()
    }

    /// Non-marker conflict `main` copies recorded for `branch`, if any.
    pub fn main_copies(&self, branch: &str) -> Option<(String, Vec<String>)> {
        self.read_main_copies(branch)
            .map(|(main, files)| (main, shown_paths(&files)))
    }

    /// Look up the current [`LedgerSnapshot`] of `branch` (any ref matching `name = branch`).
    pub fn branch_snapshot(&self, branch: &str) -> Result<Option<LedgerSnapshot>, WorkspaceError> {
        let Some(r) = self.find_ref(branch)? else {
            return Ok(None);
        };
        let snap = self
            .ledger
            .get_snapshot(&r.snapshot)
            .map_err(map_store_err)?
            .ok_or_else(|| {
                WorkspaceError::Unavailable(format!(
                    "ref {branch} points to missing snapshot {}",
                    r.snapshot
                ))
            })?;
        Ok(Some(snap))
    }

    /// Look up the current [`LedgerSnapshot`] of project `main`.
    pub fn main_snapshot(&self) -> Result<Option<LedgerSnapshot>, WorkspaceError> {
        self.branch_snapshot(BASE_BRANCH)
    }

    /// Materialize or reuse an immutable warm Layer 0 cache directory `<root>/.d6/layers/<tree_hex>`.
    ///
    /// Incremental hard-link reuse via `Layer0Options::nearest` is strictly limited to an already
    /// cached non-CAS warm layer directory under `<root>/.d6/layers/`, never `<cas>/objects`.
    pub fn ensure_warm_layer(
        &self,
        tree: &Digest,
        preferred_nearest: Option<&Digest>,
    ) -> Result<(PathBuf, Layer0MaterializeReport), WorkspaceError> {
        let layers_root = self.layers_dir();
        fs::create_dir_all(&layers_root).map_err(|err| {
            WorkspaceError::Unavailable(format!("create {}: {err}", layers_root.display()))
        })?;
        let layer_lock_path = self.locks_dir().join(format!("layer-{}.lock", tree.hex()));
        let _layer_lock = lock_file(&layer_lock_path)?;

        let dest_dir = self.layer_path(tree);
        let nearest_choice = self.pick_nearest_layer(tree, preferred_nearest);
        let options = Layer0Options {
            nearest: nearest_choice
                .as_ref()
                .map(|(digest, path)| (digest, path.as_path())),
            if_exists: ExistingDestPolicy::ReuseIfPresent,
        };
        let report =
            materialize_layer0(&self.store, tree, &dest_dir, &options).map_err(map_store_err)?;
        if !report.reused_existing_dest {
            let _ = self.write_nearest_pointer(tree);
        }
        *self
            .last_layer0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(report.clone());
        Ok((dest_dir, report))
    }

    /// Publish a new snapshot onto project `main` (`kind = Main`) through the D6 4-step
    /// durability sequence (`sync_store` -> `spawn_ref_land_main` -> `refs.journal`).
    pub fn land_main_snapshot(
        &self,
        expected_base: Option<&Digest>,
        new_snapshot: &Digest,
        task_id: Option<i64>,
        reason: &str,
    ) -> Result<u64, WorkspaceError> {
        let snap = self
            .store
            .cas()
            .read_merkle_snapshot(new_snapshot)
            .map_err(map_cas_err)?;
        let parent_oids = self
            .store
            .resolve_parent_oids(self.ledger.as_ref(), &snap.parents)
            .map_err(map_store_err)?;
        let git_oid = self
            .store
            .project_snapshot_oid(&snap, &parent_oids)
            .map_err(map_store_err)?;
        self.store.cas().sync_store().map_err(map_cas_err)?;
        let at_ms = if let Some(existing) = self
            .ledger
            .get_snapshot(new_snapshot)
            .map_err(map_store_err)?
        {
            existing.created_at_ms
        } else {
            let parsed_ms = parse_committer_ms(&snap.commit_body);
            let ts = if parsed_ms > 0 {
                parsed_ms
            } else {
                self.now_ms()
            };
            let _ = self
                .ledger
                .record_snapshot(
                    new_snapshot,
                    &git_oid,
                    &snap.tree,
                    &snap.parents,
                    &self.project,
                    ts,
                )
                .map_err(map_store_err)?;
            ts
        };
        let seq = self
            .ledger
            .land_main_ref(
                &self.project,
                BASE_BRANCH,
                expected_base,
                new_snapshot,
                task_id,
                reason,
            )
            .map_err(map_store_err)?;
        self.store
            .cas()
            .append_ref_journal(&JournalEntry {
                seq,
                project: self.project.clone(),
                name: BASE_BRANCH.to_string(),
                kind: D6RefKind::Main.as_str().to_string(),
                old_snapshot: expected_base.cloned(),
                new_snapshot: new_snapshot.clone(),
                git_oid,
                at_ms,
            })
            .map_err(map_cas_err)?;
        Ok(seq)
    }

    /// Checkpoint the mounted `workspace` directory cleanly under `WorkspaceStore::commit`.
    pub fn checkpoint(
        &self,
        workspace: &WorkspaceId,
        message: &str,
    ) -> Result<Commit, WorkspaceError> {
        self.commit_or_take_back(workspace, message, &|| Ok(()))
    }

    /// Commit `workspace` with an explicit caller-supplied raw commit body and timestamp.
    pub fn commit_with_body(
        &self,
        workspace: &WorkspaceId,
        commit_body: &[u8],
        created_at_ms: i64,
        reason: &str,
    ) -> Result<Commit, WorkspaceError> {
        let branch = workspace.sandbox().to_string();
        let directory = self.checked_path(workspace)?;
        let _lock = self.lock_branch(&branch)?;
        self.commit_locked_inner(
            &branch,
            &directory,
            &|| Ok(()),
            commit_body,
            created_at_ms,
            reason,
        )
    }

    /// Commit `workspace`, asking `take_back` first under the branch's lock when the branch
    /// ref is missing before falling back to recreating it from `main` or an empty root.
    pub fn commit_or_take_back(
        &self,
        workspace: &WorkspaceId,
        message: &str,
        take_back: &dyn Fn() -> Result<(), WorkspaceError>,
    ) -> Result<Commit, WorkspaceError> {
        let branch = workspace.sandbox().to_string();
        let directory = self.checked_path(workspace)?;
        let _lock = self.lock_branch(&branch)?;
        let created_at_ms = self.now_ms();
        let commit_body = self.format_body_ms(created_at_ms, message);
        self.commit_locked_inner(
            &branch,
            &directory,
            take_back,
            &commit_body,
            created_at_ms,
            "d6:checkpoint",
        )
    }

    /// [`WorkspaceStore::set_aside`] guarded by `ready()` asked under the branch's lock
    /// right before any read or mutation (prevents racing with a live agent turn).
    pub fn set_aside_when(
        &self,
        branch: &BranchName,
        start: Option<&str>,
        message: &str,
        ready: &dyn Fn() -> Result<(), String>,
    ) -> Result<SetAside, WorkspaceError> {
        self.set_aside_since(branch.as_str(), start, message, ready)
    }

    /// Discard a branch's local mounted workspace directory and sidecar state under its lock,
    /// leaving committed ledger snapshots/history intact.
    pub fn discard(&self, workspace: &WorkspaceId) -> Result<(), WorkspaceError> {
        let branch = workspace.sandbox();
        let directory = self.checked_path(workspace)?;
        let _lock = self.lock_branch(branch)?;
        match fs::remove_dir_all(&directory) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(WorkspaceError::Unavailable(format!(
                    "discard remove {}: {err}",
                    directory.display()
                )));
            }
        }
        self.clear_pending(branch);
        self.clear_main_copies(branch);
        self.clear_stale(branch);
        let _ = fs::remove_file(self.oversized_path(branch));
        Ok(())
    }

    /// Scan `directory` into CAS (`NS_BLOB` + `NS_TREE`) for `branch` against `TrackedSetPolicy`,
    /// applying size filtering and optional fail-closed leak verification.
    pub(crate) fn scan_directory_tree(
        &self,
        branch: &str,
        directory: &Path,
        extra_excludes: &[PathBuf],
        simulate_unfiltered: bool,
    ) -> Result<ScannedWorkspaceTree, WorkspaceError> {
        let head = self.branch_snapshot(branch)?;
        let pending = self.pending_merge(branch);
        let pending_main_id = pending.as_ref().map(|p| p.main.clone());
        let resolved_merge_main = pending.as_ref().and_then(|p| {
            let main_snap = p.main.clone();
            if let Some(head_snap) = &head
                && matches!(self.is_ancestor(&main_snap, &head_snap.id), Ok(true))
            {
                return None;
            }
            let shortest = self
                .shortest_markers(std::slice::from_ref(&main_snap), &p.files)
                .ok()?;
            (!unresolved_conflicts(directory, &shortest)).then_some(main_snap)
        });

        let main_tree = self.main_snapshot()?.map(|s| s.tree);
        let pending_main_tree = match pending_main_id.as_ref() {
            Some(id) => Some(self.snapshot_tree(id)?),
            None => None,
        };
        let base_tree = head.as_ref().map(|s| s.tree.clone());

        let mut merged_excludes = self.extra_exclude_files.clone();
        for path in extra_excludes {
            if !merged_excludes.contains(path) {
                merged_excludes.push(path.clone());
            }
        }

        let policy = TrackedSetPolicy {
            base_tree: base_tree.as_ref(),
            main_tree: main_tree.as_ref(),
            extra_exclude_files: merged_excludes,
        };

        let scanned = if simulate_unfiltered {
            scan_without_gitignore(
                directory,
                base_tree.as_ref(),
                main_tree.as_ref(),
                &self.store,
            )?
        } else {
            self.store
                .scan_workspace_tree(directory, &policy)
                .map_err(map_store_err)?
        };

        let mut kept_in_source = Vec::new();
        if let Some(ref mt) = main_tree {
            kept_in_source.push(mt.clone());
        }
        if let Some(ref pt) = pending_main_tree
            && !kept_in_source.contains(pt)
        {
            kept_in_source.push(pt.clone());
        }
        let mut source_maps = Vec::with_capacity(kept_in_source.len());
        for t in &kept_in_source {
            source_maps.push(self.store.flatten_tree(t).map_err(map_store_err)?);
        }

        let mut oversized = Vec::new();
        let mut filtered = Vec::with_capacity(scanned.len());
        for entry in scanned {
            if matches!(entry.kind, ScannedKind::File | ScannedKind::Executable) {
                let full_path = directory.join(entry.path.as_path());
                let meta = fs::symlink_metadata(&full_path).map_err(|err| {
                    WorkspaceError::Unavailable(format!("stat {}: {err}", full_path.display()))
                })?;
                if meta.file_type().is_file() && meta.len() > self.max_file_bytes {
                    let key = entry.path.as_bytes();
                    let in_source = source_maps.iter().any(|m| {
                        m.get(key).is_some_and(|te| {
                            matches!(te.kind, TreeEntryKind::File | TreeEntryKind::Executable)
                        })
                    });
                    if !in_source {
                        oversized.push((entry.path, meta.len()));
                        continue;
                    }
                }
            }
            filtered.push(entry);
        }
        oversized.sort();
        oversized.dedup_by(|a, b| a.0 == b.0);

        let base_digest = base_tree.clone().unwrap_or_else(empty_tree_digest);
        let mutations = self.build_mutations_from_scanned(directory, &base_digest, &filtered)?;
        let next_tree = self
            .store
            .apply_mutations(&base_digest, &mutations)
            .map_err(map_store_err)?;

        if !simulate_unfiltered {
            self.store
                .verify_no_ignored_untracked_leaks(directory, &next_tree, &policy)
                .map_err(map_store_err)?;
        }

        Ok((next_tree, oversized, resolved_merge_main))
    }

    #[expect(clippy::too_many_arguments)]
    pub(crate) fn publish_branch_commit_with_body(
        &self,
        branch: &str,
        directory: &Path,
        next_tree: &Digest,
        oversized: &[(TreePath, u64)],
        resolved_merge_main: Option<&Digest>,
        commit_body: &[u8],
        created_at_ms: i64,
        reason: &str,
    ) -> Result<Commit, WorkspaceError> {
        let _lock = self.lock_branch(branch)?;
        self.record_oversized(branch, directory, oversized);
        let head = self.branch_snapshot(branch)?;
        let Some(head) = head else {
            let main_snap = self.main_snapshot()?;
            let mut parents = Vec::new();
            if let Some(m) = &main_snap {
                parents.push(m.id.clone());
            }
            let published = self
                .store
                .publish_snapshot(
                    self.ledger.as_ref(),
                    &PublishRequest {
                        project: &self.project,
                        ref_name: branch,
                        kind: D6RefKind::Branch,
                        expected_ref: None,
                        tree: next_tree,
                        parents: &parents,
                        commit_body,
                        created_at_ms,
                        reason,
                        landing_task_id: None,
                    },
                )
                .map_err(map_store_err)?;
            self.clear_pending(branch);
            let commit = CommitId::new(published.git_oid.to_hex())
                .map_err(|err| WorkspaceError::Unavailable(err.to_string()))?;
            let parent = main_snap
                .map(|m| CommitId::new(m.git_oid.to_hex()))
                .transpose()
                .map_err(|err| WorkspaceError::Unavailable(err.to_string()))?;
            return Ok(Commit::Recreated { commit, parent });
        };

        let merged_second = resolved_merge_main.filter(|m| *m != &head.id).cloned();
        if *next_tree == head.tree && merged_second.is_none() {
            if resolved_merge_main.is_some() {
                self.clear_pending(branch);
            }
            return Ok(Commit::Unchanged);
        }

        let mut parents = vec![head.id.clone()];
        if let Some(m) = merged_second {
            parents.push(m);
        }
        let published = self
            .store
            .publish_snapshot(
                self.ledger.as_ref(),
                &PublishRequest {
                    project: &self.project,
                    ref_name: branch,
                    kind: D6RefKind::Branch,
                    expected_ref: Some(&head.id),
                    tree: next_tree,
                    parents: &parents,
                    commit_body,
                    created_at_ms,
                    reason,
                    landing_task_id: None,
                },
            )
            .map_err(map_store_err)?;
        if resolved_merge_main.is_some() {
            self.clear_pending(branch);
        }
        CommitId::new(published.git_oid.to_hex())
            .map(Commit::Created)
            .map_err(|err| WorkspaceError::Unavailable(err.to_string()))
    }

    pub(crate) fn write_pending_merge_for_shadow(
        &self,
        branch: &str,
        main_snap: &Digest,
        written_over: &Digest,
        files: &[TreePath],
    ) -> Result<(), WorkspaceError> {
        let _lock = self.lock_branch(branch)?;
        self.write_pending(branch, main_snap, written_over, files)
    }

    #[expect(clippy::too_many_arguments)]
    pub(crate) fn publish_merge_commit_with_body(
        &self,
        branch: &str,
        expected_head: &Digest,
        main_snap: &Digest,
        merged_tree: &Digest,
        commit_body: &[u8],
        created_at_ms: i64,
        reason: &str,
    ) -> Result<PublishedSnapshot, WorkspaceError> {
        let _lock = self.lock_branch(branch)?;
        let published = self
            .store
            .publish_snapshot(
                self.ledger.as_ref(),
                &PublishRequest {
                    project: &self.project,
                    ref_name: branch,
                    kind: D6RefKind::Branch,
                    expected_ref: Some(expected_head),
                    tree: merged_tree,
                    parents: &[expected_head.clone(), main_snap.clone()],
                    commit_body,
                    created_at_ms,
                    reason,
                    landing_task_id: None,
                },
            )
            .map_err(map_store_err)?;
        self.clear_pending(branch);
        self.clear_main_copies(branch);
        Ok(published)
    }

    pub(crate) fn publish_set_aside_commit_with_body(
        &self,
        branch: &str,
        expected_head: &Digest,
        tree: &Digest,
        commit_body: &[u8],
        created_at_ms: i64,
        reason: &str,
    ) -> Result<PublishedSnapshot, WorkspaceError> {
        let _lock = self.lock_branch(branch)?;
        let published = self
            .store
            .publish_snapshot(
                self.ledger.as_ref(),
                &PublishRequest {
                    project: &self.project,
                    ref_name: branch,
                    kind: D6RefKind::Branch,
                    expected_ref: Some(expected_head),
                    tree,
                    parents: std::slice::from_ref(expected_head),
                    commit_body,
                    created_at_ms,
                    reason,
                    landing_task_id: None,
                },
            )
            .map_err(map_store_err)?;
        self.clear_pending(branch);
        Ok(published)
    }

    pub(crate) fn workspace_id_for_branch(
        &self,
        branch: &str,
    ) -> Result<WorkspaceId, WorkspaceError> {
        let dir = self.root.join(branch);
        WorkspaceId::new(format!("{branch}:{}", dir.display()))
            .map_err(|err| WorkspaceError::Unavailable(err.to_string()))
    }
}

impl<L: RefLedger + Send + Sync> WorkspaceStore for D6Workspace<L> {
    fn project(&self) -> &str {
        &self.project
    }

    fn ensure_branch(
        &self,
        branch: &BranchName,
        from: Option<&str>,
    ) -> Result<Branch, WorkspaceError> {
        if branch.as_str().starts_with('-') || !is_plain_name(branch.as_str()) {
            return Err(WorkspaceError::Unavailable(format!(
                "invalid branch name {branch}"
            )));
        }
        if branch.as_str() == BASE_BRANCH {
            return Err(WorkspaceError::Refused {
                code: "ReservedBranch".to_string(),
                message: "main cannot be created as an agent branch".to_string(),
            });
        }
        let _lock = self.lock_branch(branch.as_str())?;
        let main_snap = self.ensure_base_seeded()?;
        if self.find_ref(branch.as_str())?.is_some() {
            return Ok(Branch::Existing);
        }

        let (label, target) = match from {
            None => (BASE_BRANCH.to_string(), main_snap),
            Some(spec) => (spec.to_string(), self.resolve_from(spec)?),
        };

        let at_ms = self.now_ms();
        self.store.cas().sync_store().map_err(map_cas_err)?;
        let seq = self
            .ledger
            .cas_ref(
                &self.project,
                branch.as_str(),
                D6RefKind::Branch,
                None,
                &target.id,
                "d6:ensure_branch",
            )
            .map_err(map_store_err)?;
        self.store
            .cas()
            .append_ref_journal(&JournalEntry {
                seq,
                project: self.project.clone(),
                name: branch.as_str().to_string(),
                kind: D6RefKind::Branch.as_str().to_string(),
                old_snapshot: None,
                new_snapshot: target.id.clone(),
                git_oid: target.git_oid,
                at_ms,
            })
            .map_err(map_cas_err)?;

        Ok(Branch::Created(Fork {
            from: label,
            commit: CommitId::new(target.git_oid.to_hex())
                .map_err(|err| WorkspaceError::Unavailable(err.to_string()))?,
        }))
    }

    fn mount(&self, branch: &BranchName) -> Result<WorkspaceId, WorkspaceError> {
        self.mount_when(branch, &|| Ok(()))
    }

    fn mount_when(
        &self,
        branch: &BranchName,
        ready: &dyn Fn() -> Result<(), String>,
    ) -> Result<WorkspaceId, WorkspaceError> {
        if !is_plain_name(branch.as_str()) {
            return Err(WorkspaceError::Unavailable(format!(
                "invalid branch name {branch}"
            )));
        }
        let head = self.branch_snapshot(branch.as_str())?.ok_or_else(|| {
            WorkspaceError::Unavailable(format!("branch {branch} has no snapshot"))
        })?;
        self.refuse_files_outside_d6(&head.id)?;

        let parent_tree = head
            .parents
            .first()
            .and_then(|pid| self.snapshot_tree(pid).ok());
        let (layer_dir, _) = self.ensure_warm_layer(&head.tree, parent_tree.as_ref())?;
        let staged = self.staging_path(branch.as_str(), "staged")?;
        if let Err(err) = clone_layer_to_writable(&layer_dir, &staged) {
            let _ = fs::remove_dir_all(&staged);
            return Err(err);
        }

        let res = self.install_staged_mount(branch.as_str(), &head.id, &staged, ready);
        if res.is_err() {
            let _ = fs::remove_dir_all(&staged);
        }
        res
    }

    fn canonical(&self, workspace: &WorkspaceId) -> WorkspaceId {
        let directory = Path::new(workspace.path());
        if directory.starts_with(&self.root) {
            return workspace.clone();
        }
        legacy_workspace(&self.root, workspace.sandbox(), directory)
            .and_then(|dir| {
                WorkspaceId::new(format!("{}:{}", workspace.sandbox(), dir.display())).ok()
            })
            .unwrap_or_else(|| workspace.clone())
    }

    fn commit(&self, workspace: &WorkspaceId, message: &str) -> Result<Commit, WorkspaceError> {
        self.commit_or_take_back(workspace, message, &|| Ok(()))
    }

    fn unmount(&self, workspace: &WorkspaceId) -> Result<(), WorkspaceError> {
        self.unmount_when(workspace, &|| Ok(()))
    }

    fn unmount_when(
        &self,
        workspace: &WorkspaceId,
        ready: &dyn Fn() -> Result<(), WorkspaceError>,
    ) -> Result<(), WorkspaceError> {
        let branch = workspace.sandbox();
        let directory = self.checked_path(workspace)?;
        let _lock = self.lock_branch(branch)?;
        ready()?;
        self.keep_stale_locked(branch, &directory)?;
        self.settle_pending(branch);
        match fs::remove_dir_all(&directory) {
            Ok(()) => {
                self.clear_stale(branch);
                Ok(())
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                self.clear_stale(branch);
                Ok(())
            }
            Err(err) => Err(WorkspaceError::Unavailable(format!(
                "remove {}: {err}",
                directory.display()
            ))),
        }
    }

    fn is_mounted(&self, workspace: &WorkspaceId) -> bool {
        self.checked_path(workspace)
            .is_ok_and(|directory| directory.is_dir())
    }

    fn merge_main_clean(&self, branch: &BranchName) -> Result<BaseMerge, WorkspaceError> {
        let _lock = self.lock_branch(branch.as_str())?;
        let head = self.branch_snapshot(branch.as_str())?.ok_or_else(|| {
            WorkspaceError::Unavailable(format!("branch {branch} has no snapshot"))
        })?;
        self.refuse_files_outside_d6(&head.id)?;
        let Some(main_snap) = self.main_snapshot()? else {
            return Ok(BaseMerge::Current);
        };
        if self.is_ancestor(&main_snap.id, &head.id)? {
            return Ok(BaseMerge::Current);
        }

        let report = match merge_snapshots(&self.store, &main_snap.id, &head.id, None) {
            Ok((_base, report)) => report,
            Err(MergeBaseError::NoCommonAncestor { .. }) => return Ok(BaseMerge::Current),
            Err(err) => return Err(WorkspaceError::Unavailable(err.to_string())),
        };

        if !report.is_clean() {
            let files: Vec<String> = report.conflicts.iter().map(|c| c.path.shown()).collect();
            let marked = is_pure_content_conflicts(&report.conflicts);
            self.follow_main_copies(branch.as_str(), &main_snap.id, &report.conflicts, marked)?;
            return Ok(BaseMerge::Conflicted {
                files,
                marked: false,
            });
        }

        let files = self.diff_tree_paths(&head.tree, &report.tree)?;
        let now_ms = self.now_ms();
        let ident = self.git_ident_line_ms(now_ms);
        let msg = format!("spawn: merge main {} into {branch}", main_snap.git_oid);
        let built = build_merge_snapshot(
            &self.store,
            &MergeSnapshotSpec {
                main_snapshot: &head.id,
                branch_snapshot: &main_snap.id,
                merged_tree: &report.tree,
                main_git_oid: Some(head.git_oid),
                branch_git_oid: Some(main_snap.git_oid),
                author_line: &ident,
                committer_line: &ident,
                message: &msg,
                spawn_task: None,
                spawn_objective: None,
                extra_trailers: &[],
            },
        )
        .map_err(map_store_err)?;

        self.commit_snapshot_cas(
            branch.as_str(),
            &head.id,
            &built.snapshot_id,
            &built.snapshot.tree,
            &built.snapshot.parents,
            &built.git_oid,
            now_ms,
            "d6:merge_main_clean",
        )?;

        Ok(BaseMerge::Merged {
            commit: CommitId::new(built.git_oid.to_hex())
                .map_err(|err| WorkspaceError::Unavailable(err.to_string()))?,
            files,
        })
    }

    fn merge_base(&self, workspace: &WorkspaceId) -> Result<BaseMerge, WorkspaceError> {
        self.merge_main(workspace, &|| Ok(()))
    }

    fn merge_base_when(
        &self,
        workspace: &WorkspaceId,
        ready: &dyn Fn() -> Result<(), String>,
    ) -> Result<BaseMerge, WorkspaceError> {
        self.merge_main(workspace, ready)
    }

    fn hold(&self, workspace: &WorkspaceId) -> Option<Hold> {
        self.lock_branch(workspace.sandbox())
            .ok()
            .map(|lock| Box::new(lock) as Hold)
    }

    fn set_aside(
        &self,
        branch: &BranchName,
        start: Option<&str>,
        message: &str,
    ) -> Result<SetAside, WorkspaceError> {
        self.set_aside_since(branch.as_str(), start, message, &|| Ok(()))
    }

    fn notices(&self, workspace: &WorkspaceId) -> Vec<String> {
        self.untold_oversized(workspace.sandbox())
    }

    fn notices_told(&self, workspace: &WorkspaceId, notices: &[String]) {
        self.oversized_told(workspace.sandbox(), notices);
    }

    fn branch_head(&self, branch: &BranchName) -> Result<Option<CommitId>, WorkspaceError> {
        let Some(snap) = self.branch_snapshot(branch.as_str())? else {
            return Ok(None);
        };
        CommitId::new(snap.git_oid.to_hex())
            .map(Some)
            .map_err(|err| WorkspaceError::Unavailable(err.to_string()))
    }
}

impl<L: RefLedger> D6Workspace<L> {
    fn now_ms(&self) -> i64 {
        (self.clock)()
    }

    fn git_ident_line_ms(&self, at_ms: i64) -> String {
        let secs = at_ms.div_euclid(1000);
        format!("{} <{}> {secs} +0000", self.author_name, self.author_email)
    }

    fn format_body_ms(&self, at_ms: i64, message: &str) -> Vec<u8> {
        let ident = self.git_ident_line_ms(at_ms);
        format_commit_body(&ident, &ident, message)
    }

    fn d6_meta_dir(&self) -> PathBuf {
        self.root.join(D6_META_DIR)
    }

    fn locks_dir(&self) -> PathBuf {
        self.d6_meta_dir().join(LOCKS_SUBDIR)
    }

    fn lock_branch(&self, branch: &str) -> Result<fs::File, WorkspaceError> {
        if !is_plain_name(branch) {
            return Err(WorkspaceError::Unavailable(format!(
                "invalid branch lock name {branch:?}"
            )));
        }
        lock_file(&self.locks_dir().join(format!("{branch}.lock")))
    }

    fn checked_path(&self, workspace: &WorkspaceId) -> Result<PathBuf, WorkspaceError> {
        let directory = PathBuf::from(workspace.path());
        if directory.starts_with(&self.root) {
            return Ok(directory);
        }
        if let Some(dir) = legacy_workspace(&self.root, workspace.sandbox(), &directory) {
            return Ok(dir);
        }
        Err(WorkspaceError::Unavailable(format!(
            "workspace {} is outside {}",
            directory.display(),
            self.root.display()
        )))
    }

    fn find_ref(
        &self,
        branch: &str,
    ) -> Result<Option<crate::cas_merkle::LedgerRef>, WorkspaceError> {
        let refs = self
            .ledger
            .list_refs(Some(&self.project))
            .map_err(map_store_err)?;
        Ok(refs.into_iter().find(|r| r.name == branch))
    }

    fn ensure_base_seeded(&self) -> Result<LedgerSnapshot, WorkspaceError> {
        if let Some(existing) = self.main_snapshot()? {
            return Ok(existing);
        }
        let empty_tree = self.store.cas().put_empty_tree().map_err(map_cas_err)?;
        let at_ms = self.now_ms();
        let body = self.format_body_ms(at_ms, "spawn: base");
        let published = self
            .store
            .publish_snapshot(
                self.ledger.as_ref(),
                &PublishRequest {
                    project: &self.project,
                    ref_name: BASE_BRANCH,
                    kind: D6RefKind::Main,
                    expected_ref: None,
                    tree: &empty_tree,
                    parents: &[],
                    commit_body: &body,
                    created_at_ms: at_ms,
                    reason: "d6:seed_base",
                    landing_task_id: None,
                },
            )
            .map_err(map_store_err)?;
        self.ledger
            .get_snapshot(&published.snapshot)
            .map_err(map_store_err)?
            .ok_or_else(|| {
                WorkspaceError::Unavailable("newly seeded main snapshot vanished".to_string())
            })
    }

    fn resolve_from(&self, from: &str) -> Result<LedgerSnapshot, WorkspaceError> {
        let refused = |message: String| WorkspaceError::Refused {
            code: "NoSuchFrom".to_string(),
            message,
        };
        match ForkFrom::parse(from)? {
            ForkFrom::Agent(agent) => self
                .branch_snapshot(agent.as_str())?
                .ok_or_else(|| refused(format!("{agent} has no branch in {}", self.project))),
            ForkFrom::Commit(commit) => {
                if let Ok(oid) = GitOid::parse_hex(commit.as_str()) {
                    let found = self
                        .ledger
                        .get_snapshot_by_oid(&oid)
                        .map_err(map_store_err)?;
                    if let Some(snap) = found
                        && snap.project == self.project
                    {
                        return Ok(snap);
                    }
                    return Err(refused(format!("no commit {commit} in {}", self.project)));
                }
                if let Ok(digest) = Digest::from_parts(Algo::Sha256, commit.as_str()) {
                    let found = self.ledger.get_snapshot(&digest).map_err(map_store_err)?;
                    if let Some(snap) = found
                        && snap.project == self.project
                    {
                        return Ok(snap);
                    }
                }
                Err(refused(format!("no commit {commit} in {}", self.project)))
            }
        }
    }

    fn snapshot_tree(&self, snap_id: &Digest) -> Result<Digest, WorkspaceError> {
        if let Some(row) = self.ledger.get_snapshot(snap_id).map_err(map_store_err)? {
            return Ok(row.tree);
        }
        let snap = self
            .store
            .cas()
            .read_merkle_snapshot(snap_id)
            .map_err(map_cas_err)?;
        Ok(snap.tree)
    }

    fn refuse_files_outside_d6(&self, snap_id: &Digest) -> Result<(), WorkspaceError> {
        let snap = self
            .store
            .cas()
            .read_merkle_snapshot(snap_id)
            .map_err(map_cas_err)?;
        let msg = extract_commit_message(&snap.commit_body);
        if names_files_outside_git(&msg) {
            return Err(WorkspaceError::Refused {
                code: "FilesOutsideGit".to_string(),
                message: format!(
                    "snapshot {snap_id} names large files outside git ({LARGE_FILE_TRAILER})"
                ),
            });
        }
        Ok(())
    }

    fn pick_nearest_layer(
        &self,
        target_tree: &Digest,
        preferred: Option<&Digest>,
    ) -> Option<(Digest, PathBuf)> {
        if let Some(pref) = preferred
            && pref != target_tree
        {
            let dir = self.layer_path(pref);
            if fs::symlink_metadata(&dir).is_ok_and(|m| !m.file_type().is_symlink() && m.is_dir()) {
                return Some((pref.clone(), dir));
            }
        }
        let pointer = self.layers_dir().join(NEAREST_POINTER_FILE);
        let raw = fs::read_to_string(pointer).ok()?;
        let digest = Digest::parse(raw.trim()).ok()?;
        if &digest == target_tree {
            return None;
        }
        let dir = self.layer_path(&digest);
        if fs::symlink_metadata(&dir).is_ok_and(|m| !m.file_type().is_symlink() && m.is_dir()) {
            Some((digest, dir))
        } else {
            None
        }
    }

    fn write_nearest_pointer(&self, tree: &Digest) -> Result<(), WorkspaceError> {
        let layers = self.layers_dir();
        let tmp = layers.join(format!(
            ".nearest.{}.{}",
            std::process::id(),
            D6_STAGE_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&tmp, format!("{tree}\n")).map_err(|err| {
            WorkspaceError::Unavailable(format!("write {}: {err}", tmp.display()))
        })?;
        fs::rename(&tmp, layers.join(NEAREST_POINTER_FILE))
            .map_err(|err| WorkspaceError::Unavailable(format!("rename nearest pointer: {err}")))
    }

    fn staging_path(&self, branch: &str, what: &str) -> Result<PathBuf, WorkspaceError> {
        let staging = self.d6_meta_dir().join(MOUNT_SUBDIR);
        fs::create_dir_all(&staging).map_err(|err| {
            WorkspaceError::Unavailable(format!("create {}: {err}", staging.display()))
        })?;
        let prefix = format!("{branch}.");
        let ours = format!("{branch}.{}.", std::process::id());
        if let Ok(entries) = fs::read_dir(&staging) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with(&prefix) && !name.starts_with(&ours) {
                    if entry.file_type().is_ok_and(|k| k.is_dir()) {
                        let _ = fs::remove_dir_all(entry.path());
                    } else {
                        let _ = fs::remove_file(entry.path());
                    }
                }
            }
        }
        Ok(staging.join(format!(
            "{ours}{what}.{}",
            D6_STAGE_SEQ.fetch_add(1, Ordering::Relaxed)
        )))
    }

    fn install_staged_mount(
        &self,
        branch: &str,
        expected_head: &Digest,
        staged: &Path,
        ready: &dyn Fn() -> Result<(), String>,
    ) -> Result<WorkspaceId, WorkspaceError> {
        let _lock = self.lock_branch(branch)?;
        ready().map_err(|why| WorkspaceError::Refused {
            code: MOUNT_CANCELLED.to_string(),
            message: why,
        })?;
        let current = self.branch_snapshot(branch)?.ok_or_else(|| {
            WorkspaceError::Unavailable(format!("branch {branch} has no snapshot"))
        })?;
        if &current.id != expected_head {
            return Err(WorkspaceError::Refused {
                code: MOUNT_CANCELLED.to_string(),
                message: format!("branch {branch} moved during mount"),
            });
        }

        let directory = self.root.join(branch);
        self.keep_stale_locked(branch, &directory)?;
        self.settle_pending(branch);
        self.restore_main_copies(branch, staged)?;
        self.replace_workspace(branch, staged, &directory)?;
        self.clear_stale(branch);

        WorkspaceId::new(format!("{branch}:{}", directory.display()))
            .map_err(|err| WorkspaceError::Unavailable(err.to_string()))
    }

    fn replace_workspace(
        &self,
        branch: &str,
        staged: &Path,
        directory: &Path,
    ) -> Result<(), WorkspaceError> {
        let replaced = match fs::symlink_metadata(directory) {
            Ok(_) => {
                let aside = self.staging_path(branch, "replaced")?;
                fs::rename(directory, &aside).map_err(|err| {
                    WorkspaceError::Unavailable(format!(
                        "move {} aside: {err}",
                        directory.display()
                    ))
                })?;
                Some(aside)
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => None,
            Err(err) => {
                return Err(WorkspaceError::Unavailable(format!(
                    "stat {}: {err}",
                    directory.display()
                )));
            }
        };
        if let Err(err) = fs::rename(staged, directory) {
            if let Some(aside) = &replaced {
                let _ = fs::rename(aside, directory);
            }
            return Err(WorkspaceError::Unavailable(format!(
                "move staged mount to {}: {err}",
                directory.display()
            )));
        }
        if let Some(aside) = replaced {
            let _ = fs::remove_dir_all(aside);
        }
        Ok(())
    }

    fn commit_locked_inner(
        &self,
        branch: &str,
        directory: &Path,
        take_back: &dyn Fn() -> Result<(), WorkspaceError>,
        commit_body: &[u8],
        created_at_ms: i64,
        reason: &str,
    ) -> Result<Commit, WorkspaceError> {
        if self.is_stale(branch) {
            let kept = self.keep_stale(branch, directory)?;
            return Err(stale_refusal(branch, Some(&kept)));
        }
        if self.branch_snapshot(branch)?.is_none() {
            take_back()?;
        }
        let head = self.branch_snapshot(branch)?;
        if head.is_none() {
            return self.recreate(branch, directory, commit_body, created_at_ms, reason);
        }
        let head = head.expect("checked Some");

        let (next_tree, oversized, resolved_merge_main) =
            self.scan_directory_tree(branch, directory, &[], false)?;
        self.record_oversized(branch, directory, &oversized);

        let merged_second = resolved_merge_main.clone().filter(|m| m != &head.id);
        if next_tree == head.tree && merged_second.is_none() {
            if resolved_merge_main.is_some() {
                self.clear_pending(branch);
            }
            return Ok(Commit::Unchanged);
        }

        let mut parents = vec![head.id.clone()];
        if let Some(m) = merged_second {
            parents.push(m);
        }
        let published = self
            .store
            .publish_snapshot(
                self.ledger.as_ref(),
                &PublishRequest {
                    project: &self.project,
                    ref_name: branch,
                    kind: D6RefKind::Branch,
                    expected_ref: Some(&head.id),
                    tree: &next_tree,
                    parents: &parents,
                    commit_body,
                    created_at_ms,
                    reason,
                    landing_task_id: None,
                },
            )
            .map_err(map_store_err)?;

        if resolved_merge_main.is_some() {
            self.clear_pending(branch);
        }
        CommitId::new(published.git_oid.to_hex())
            .map(Commit::Created)
            .map_err(|err| WorkspaceError::Unavailable(err.to_string()))
    }

    fn recreate(
        &self,
        branch: &str,
        directory: &Path,
        commit_body: &[u8],
        created_at_ms: i64,
        reason: &str,
    ) -> Result<Commit, WorkspaceError> {
        let main_snap = self.main_snapshot()?;
        let base_tree = main_snap
            .as_ref()
            .map_or_else(empty_tree_digest, |m| m.tree.clone());
        let policy = TrackedSetPolicy {
            base_tree: main_snap.as_ref().map(|m| &m.tree),
            main_tree: main_snap.as_ref().map(|m| &m.tree),
            extra_exclude_files: self.extra_exclude_files.clone(),
        };
        let scanned = self
            .store
            .scan_workspace_tree(directory, &policy)
            .map_err(map_store_err)?;
        let (filtered, oversized) = self.filter_oversized(
            directory,
            scanned,
            main_snap.as_ref().map(|m| &m.tree),
            None,
        )?;
        self.record_oversized(branch, directory, &oversized);

        let mutations = self.build_mutations_from_scanned(directory, &base_tree, &filtered)?;
        let next_tree = self
            .store
            .apply_mutations(&base_tree, &mutations)
            .map_err(map_store_err)?;
        self.store
            .verify_no_ignored_untracked_leaks(directory, &next_tree, &policy)
            .map_err(map_store_err)?;

        let mut parents = Vec::new();
        if let Some(m) = &main_snap {
            parents.push(m.id.clone());
        }
        let published = self
            .store
            .publish_snapshot(
                self.ledger.as_ref(),
                &PublishRequest {
                    project: &self.project,
                    ref_name: branch,
                    kind: D6RefKind::Branch,
                    expected_ref: None,
                    tree: &next_tree,
                    parents: &parents,
                    commit_body,
                    created_at_ms,
                    reason,
                    landing_task_id: None,
                },
            )
            .map_err(map_store_err)?;
        self.clear_pending(branch);
        let commit = CommitId::new(published.git_oid.to_hex())
            .map_err(|err| WorkspaceError::Unavailable(err.to_string()))?;
        let parent = main_snap
            .map(|m| CommitId::new(m.git_oid.to_hex()))
            .transpose()
            .map_err(|err| WorkspaceError::Unavailable(err.to_string()))?;
        Ok(Commit::Recreated { commit, parent })
    }

    fn filter_oversized(
        &self,
        directory: &Path,
        scanned: Vec<ScannedEntry>,
        main_tree: Option<&Digest>,
        pending_main_tree: Option<&Digest>,
    ) -> Result<FilteredScan, WorkspaceError> {
        let mut source_maps = Vec::new();
        if let Some(mt) = main_tree {
            source_maps.push(self.store.flatten_tree(mt).map_err(map_store_err)?);
        }
        if let Some(pt) = pending_main_tree
            && main_tree != Some(pt)
        {
            source_maps.push(self.store.flatten_tree(pt).map_err(map_store_err)?);
        }
        let mut oversized = Vec::new();
        let mut filtered = Vec::with_capacity(scanned.len());
        for entry in scanned {
            if matches!(entry.kind, ScannedKind::File | ScannedKind::Executable) {
                let full_path = directory.join(entry.path.as_path());
                let meta = fs::symlink_metadata(&full_path).map_err(|err| {
                    WorkspaceError::Unavailable(format!("stat {}: {err}", full_path.display()))
                })?;
                if meta.file_type().is_file() && meta.len() > self.max_file_bytes {
                    let key = entry.path.as_bytes();
                    let in_source = source_maps.iter().any(|m| {
                        m.get(key).is_some_and(|te| {
                            matches!(te.kind, TreeEntryKind::File | TreeEntryKind::Executable)
                        })
                    });
                    if !in_source {
                        oversized.push((entry.path, meta.len()));
                        continue;
                    }
                }
            }
            filtered.push(entry);
        }
        oversized.sort();
        oversized.dedup_by(|a, b| a.0 == b.0);
        Ok((filtered, oversized))
    }

    fn build_mutations_from_scanned(
        &self,
        directory: &Path,
        base_tree: &Digest,
        scanned: &[ScannedEntry],
    ) -> Result<Vec<TreeMutation>, WorkspaceError> {
        let base_map = self.store.flatten_tree(base_tree).map_err(map_store_err)?;
        let mut seen: BTreeSet<Vec<u8>> = BTreeSet::new();
        let mut mutations = Vec::new();

        for item in scanned {
            let rel = item.path.as_bytes().to_vec();
            seen.insert(rel.clone());
            if let Some(mutation) =
                self.mutation_for_scanned_item(directory, &rel, item, base_map.get(&rel))?
            {
                mutations.push(mutation);
            }
        }

        for existing_path in base_map.keys() {
            if !seen.contains(existing_path) && !has_ancestor_path(&seen, existing_path) {
                mutations.push(TreeMutation::Remove {
                    path: split_rel_segments(existing_path),
                });
            }
        }

        Ok(mutations)
    }

    fn mutation_for_scanned_item(
        &self,
        directory: &Path,
        rel: &[u8],
        item: &ScannedEntry,
        prev: Option<&TreeEntry>,
    ) -> Result<Option<TreeMutation>, WorkspaceError> {
        let disk_path = directory.join(item.path.as_path());
        match &item.kind {
            ScannedKind::File | ScannedKind::Executable => {
                let want_kind = if item.kind == ScannedKind::Executable {
                    TreeEntryKind::Executable
                } else {
                    TreeEntryKind::File
                };
                let mut file = open_regular_file_nofollow(&disk_path)?;
                let size = file
                    .metadata()
                    .map_err(|err| {
                        WorkspaceError::Unavailable(format!("stat {}: {err}", disk_path.display()))
                    })?
                    .len();
                let reuse = prev.is_some_and(|e| {
                    e.kind == want_kind
                        && e.size == size
                        && e.digest().is_some_and(|d| self.store.cas().has_blob(&d))
                });
                if reuse {
                    let digest = sha256_reader(&mut file).map_err(|err| {
                        WorkspaceError::Unavailable(format!("hash {}: {err}", disk_path.display()))
                    })?;
                    if prev.is_some_and(|e| e.id == digest.as_bytes()) {
                        return Ok(None);
                    }
                }
                drop(file);
                let (digest, stored_size) = self
                    .store
                    .put_file_blob(&disk_path)
                    .map_err(map_store_err)?;
                Ok(Some(TreeMutation::Upsert {
                    path: split_rel_segments(rel),
                    kind: want_kind,
                    id: digest.as_bytes(),
                    size: stored_size,
                }))
            }
            ScannedKind::Symlink => {
                let target = fs::read_link(&disk_path).map_err(|err| {
                    WorkspaceError::Unavailable(format!("readlink {}: {err}", disk_path.display()))
                })?;
                let target_bytes = target.as_os_str().as_bytes();
                let size = u64::try_from(target_bytes.len()).map_err(|_| {
                    WorkspaceError::Unavailable("symlink target length overflow".to_string())
                })?;
                let digest = sha256_of(target_bytes);
                let same = prev.is_some_and(|e| {
                    e.kind == TreeEntryKind::Symlink
                        && e.id == digest.as_bytes()
                        && e.size == size
                        && self.store.cas().has_blob(&digest)
                });
                if same {
                    return Ok(None);
                }
                let (stored_digest, stored_size) = self
                    .store
                    .put_bytes_blob(target_bytes)
                    .map_err(map_store_err)?;
                Ok(Some(TreeMutation::Upsert {
                    path: split_rel_segments(rel),
                    kind: TreeEntryKind::Symlink,
                    id: stored_digest.as_bytes(),
                    size: stored_size,
                }))
            }
            ScannedKind::Gitlink { oid } => {
                let entry = TreeEntry::gitlink(b"x".to_vec(), oid).map_err(map_cas_err)?;
                let same = prev.is_some_and(|e| {
                    e.kind == TreeEntryKind::Gitlink && e.id == entry.id && e.size == entry.size
                });
                if same {
                    return Ok(None);
                }
                Ok(Some(TreeMutation::Upsert {
                    path: split_rel_segments(rel),
                    kind: TreeEntryKind::Gitlink,
                    id: entry.id,
                    size: entry.size,
                }))
            }
        }
    }

    fn merge_main(
        &self,
        workspace: &WorkspaceId,
        ready: &dyn Fn() -> Result<(), String>,
    ) -> Result<BaseMerge, WorkspaceError> {
        let branch = workspace.sandbox().to_string();
        let directory = self.checked_path(workspace)?;
        let _lock = self.lock_branch(&branch)?;
        self.refuse_stale(&branch)?;

        if let Some(pending) = self.pending_merge(&branch) {
            let shortest = self.shortest_markers(&[pending.main], &pending.files)?;
            if unresolved_conflicts(&directory, &shortest) {
                return Ok(BaseMerge::Conflicted {
                    files: shown_paths(&pending.files),
                    marked: true,
                });
            }
            return Ok(BaseMerge::Current);
        }

        let head = self.branch_snapshot(&branch)?.ok_or_else(|| {
            WorkspaceError::Unavailable(format!("branch {branch} has no snapshot"))
        })?;
        let Some(main_snap) = self.main_snapshot()? else {
            return Ok(BaseMerge::Current);
        };
        if self.is_ancestor(&main_snap.id, &head.id)? {
            self.clear_main_copies(&branch);
            return Ok(BaseMerge::Current);
        }

        let report = match merge_snapshots(&self.store, &main_snap.id, &head.id, None) {
            Ok((_base, report)) => report,
            Err(MergeBaseError::NoCommonAncestor { .. }) => return Ok(BaseMerge::Current),
            Err(err) => return Err(WorkspaceError::Unavailable(err.to_string())),
        };

        ready().map_err(|why| WorkspaceError::Refused {
            code: MERGE_WAITS.to_string(),
            message: why,
        })?;

        if !report.is_clean() {
            let mut conflict_paths: Vec<TreePath> =
                report.conflicts.iter().map(|c| c.path.clone()).collect();
            conflict_paths.dedup();
            let marked = is_pure_content_conflicts(&report.conflicts);
            if marked {
                apply_two_tree_update(&self.store, &directory, &head.tree, &report.tree)?;
                if let Err(err) =
                    self.write_pending(&branch, &main_snap.id, &head.id, &conflict_paths)
                {
                    let _ =
                        apply_two_tree_update(&self.store, &directory, &report.tree, &head.tree);
                    return Err(err);
                }
                self.clear_main_copies(&branch);
            } else {
                self.copy_mains_versions(
                    &branch,
                    &directory,
                    &main_snap.id,
                    &main_snap.tree,
                    &conflict_paths,
                )?;
            }
            return Ok(BaseMerge::Conflicted {
                files: shown_paths(&conflict_paths),
                marked,
            });
        }

        self.apply_and_publish_clean_merge(&branch, &directory, &head, &main_snap, &report.tree)
    }

    fn apply_and_publish_clean_merge(
        &self,
        branch: &str,
        directory: &Path,
        head: &LedgerSnapshot,
        main_snap: &LedgerSnapshot,
        merged_tree: &Digest,
    ) -> Result<BaseMerge, WorkspaceError> {
        let changed_files = apply_two_tree_update(&self.store, directory, &head.tree, merged_tree)?;

        let now_ms = self.now_ms();
        let ident = self.git_ident_line_ms(now_ms);
        let msg = format!("spawn: merge main {} into {branch}", main_snap.git_oid);
        let built = build_merge_snapshot(
            &self.store,
            &MergeSnapshotSpec {
                main_snapshot: &head.id,
                branch_snapshot: &main_snap.id,
                merged_tree,
                main_git_oid: Some(head.git_oid),
                branch_git_oid: Some(main_snap.git_oid),
                author_line: &ident,
                committer_line: &ident,
                message: &msg,
                spawn_task: None,
                spawn_objective: None,
                extra_trailers: &[],
            },
        )
        .map_err(map_store_err)?;

        if let Err(err) = self.commit_snapshot_cas(
            branch,
            &head.id,
            &built.snapshot_id,
            &built.snapshot.tree,
            &built.snapshot.parents,
            &built.git_oid,
            now_ms,
            "d6:merge_base",
        ) {
            return Err(self.follow_moved_branch(branch, directory, merged_tree, &err));
        }

        self.clear_main_copies(branch);
        Ok(BaseMerge::Merged {
            commit: CommitId::new(built.git_oid.to_hex())
                .map_err(|err| WorkspaceError::Unavailable(err.to_string()))?,
            files: changed_files,
        })
    }

    fn set_aside_since(
        &self,
        branch: &str,
        start: Option<&str>,
        message: &str,
        ready: &dyn Fn() -> Result<(), String>,
    ) -> Result<SetAside, WorkspaceError> {
        let _lock = self.lock_branch(branch)?;
        ready().map_err(|why| WorkspaceError::Refused {
            code: SET_ASIDE_WAITS.to_string(),
            message: why,
        })?;
        self.refuse_stale(branch)?;

        let mut head = self.branch_snapshot(branch)?.ok_or_else(|| {
            WorkspaceError::Unavailable(format!("branch {branch} has no snapshot"))
        })?;
        let start_snap = self.set_aside_start(branch, &head, start)?;
        let directory = self.root.join(branch);
        let mounted = directory.is_dir();

        if mounted {
            head = self.scan_and_keep_mounted_for_set_aside(
                branch,
                &directory,
                head,
                &start_snap.tree,
            )?;
        }

        if head.tree == start_snap.tree {
            if head.id != start_snap.id
                && let Some(done) = self.set_aside_made(&head, &start_snap.tree, message)?
            {
                return Ok(done);
            }
            return Ok(SetAside::Unchanged);
        }

        let files = if mounted {
            apply_two_tree_update(&self.store, &directory, &head.tree, &start_snap.tree)?
        } else {
            self.diff_tree_paths(&head.tree, &start_snap.tree)?
        };

        let now_ms = self.now_ms();
        let body = self.format_body_ms(now_ms, message);
        let snapshot = Snapshot {
            tree: start_snap.tree.clone(),
            parents: vec![head.id.clone()],
            commit_body: body,
        };
        let snap_id = self
            .store
            .cas()
            .put_merkle_snapshot(&snapshot)
            .map_err(map_cas_err)?;
        let commit_oid = self
            .store
            .project_snapshot_oid(&snapshot, &[head.git_oid])
            .map_err(map_store_err)?;

        if let Err(err) = self.commit_snapshot_cas(
            branch,
            &head.id,
            &snap_id,
            &snapshot.tree,
            &snapshot.parents,
            &commit_oid,
            now_ms,
            "d6:set_aside",
        ) {
            if mounted {
                return Err(self.follow_moved_branch(branch, &directory, &start_snap.tree, &err));
            }
            return Err(err);
        }

        self.clear_pending(branch);
        Ok(SetAside::Created {
            commit: CommitId::new(commit_oid.to_hex())
                .map_err(|err| WorkspaceError::Unavailable(err.to_string()))?,
            kept: CommitId::new(head.git_oid.to_hex())
                .map_err(|err| WorkspaceError::Unavailable(err.to_string()))?,
            files,
        })
    }

    fn scan_and_keep_mounted_for_set_aside(
        &self,
        branch: &str,
        directory: &Path,
        mut head: LedgerSnapshot,
        start_tree: &Digest,
    ) -> Result<LedgerSnapshot, WorkspaceError> {
        let main_tree = self.main_snapshot()?.map(|m| m.tree);
        let policy = TrackedSetPolicy {
            base_tree: Some(&head.tree),
            main_tree: main_tree.as_ref(),
            extra_exclude_files: self.extra_exclude_files.clone(),
        };
        let scanned = self
            .store
            .scan_workspace_tree(directory, &policy)
            .map_err(map_store_err)?;
        let (filtered, oversized) =
            self.filter_oversized(directory, scanned, main_tree.as_ref(), None)?;
        let mutations = self.build_mutations_from_scanned(directory, &head.tree, &filtered)?;
        let work_tree = self
            .store
            .apply_mutations(&head.tree, &mutations)
            .map_err(map_store_err)?;
        self.store
            .verify_no_ignored_untracked_leaks(directory, &work_tree, &policy)
            .map_err(map_store_err)?;

        if work_tree != head.tree {
            let now_ms = self.now_ms();
            let kept_body =
                self.format_body_ms(now_ms, &crate::git::set_aside_kept_message(branch));
            let published = self
                .store
                .publish_snapshot(
                    self.ledger.as_ref(),
                    &PublishRequest {
                        project: &self.project,
                        ref_name: branch,
                        kind: D6RefKind::Branch,
                        expected_ref: Some(&head.id),
                        tree: &work_tree,
                        parents: std::slice::from_ref(&head.id),
                        commit_body: &kept_body,
                        created_at_ms: now_ms,
                        reason: "d6:set_aside_kept",
                        landing_task_id: None,
                    },
                )
                .map_err(map_store_err)?;
            head = self
                .ledger
                .get_snapshot(&published.snapshot)
                .map_err(map_store_err)?
                .ok_or_else(|| WorkspaceError::Unavailable("kept snapshot vanished".to_string()))?;
        }
        self.make_way_for_start(branch, directory, start_tree, &head, &oversized)?;
        Ok(head)
    }

    fn set_aside_start(
        &self,
        branch: &str,
        head: &LedgerSnapshot,
        start: Option<&str>,
    ) -> Result<LedgerSnapshot, WorkspaceError> {
        let resolved = if let Some(spec) = start {
            if let Ok(digest) = Digest::parse(spec) {
                self.ledger
                    .get_snapshot(&digest)
                    .map_err(map_store_err)?
                    .ok_or_else(|| WorkspaceError::Refused {
                        code: "NoSuchFrom".to_string(),
                        message: format!("unknown snapshot {spec}"),
                    })?
            } else if let Ok(oid) = GitOid::parse_hex(spec) {
                self.ledger
                    .get_snapshot_by_oid(&oid)
                    .map_err(map_store_err)?
                    .ok_or_else(|| WorkspaceError::Refused {
                        code: "NoSuchFrom".to_string(),
                        message: format!("unknown commit {spec}"),
                    })?
            } else {
                return Err(WorkspaceError::Refused {
                    code: "NoSuchFrom".to_string(),
                    message: format!("invalid set_aside start {spec:?}"),
                });
            }
        } else {
            let main_snap = self.main_snapshot()?.ok_or_else(|| {
                WorkspaceError::Unavailable("project main snapshot missing".to_string())
            })?;
            let base_id = find_single_merge_base(&self.store, &main_snap.id, &head.id)
                .map_err(|err| WorkspaceError::Unavailable(err.to_string()))?;
            self.ledger
                .get_snapshot(&base_id)
                .map_err(map_store_err)?
                .ok_or_else(|| {
                    WorkspaceError::Unavailable(format!("missing merge-base snapshot {base_id}"))
                })?
        };

        if !self.is_ancestor(&resolved.id, &head.id)? {
            return Err(WorkspaceError::Refused {
                code: "NotAncestor".to_string(),
                message: format!(
                    "{} is not a commit of branch {branch} (head {})",
                    resolved.git_oid, head.git_oid
                ),
            });
        }
        Ok(resolved)
    }

    fn set_aside_made(
        &self,
        head: &LedgerSnapshot,
        start_tree: &Digest,
        message: &str,
    ) -> Result<Option<SetAside>, WorkspaceError> {
        if head.parents.len() != 1 {
            return Ok(None);
        }
        let kept_id = &head.parents[0];
        let kept_snap = self
            .ledger
            .get_snapshot(kept_id)
            .map_err(map_store_err)?
            .ok_or_else(|| {
                WorkspaceError::Unavailable(format!("missing kept snapshot {kept_id}"))
            })?;
        if kept_snap.tree == *start_tree {
            return Ok(None);
        }
        let raw_snap = self
            .store
            .cas()
            .read_merkle_snapshot(&head.id)
            .map_err(map_cas_err)?;
        let actual_msg = extract_commit_message(&raw_snap.commit_body);
        if actual_msg.trim() != message.trim() {
            return Ok(None);
        }
        let files = self.diff_tree_paths(&kept_snap.tree, start_tree)?;
        Ok(Some(SetAside::Created {
            commit: CommitId::new(head.git_oid.to_hex())
                .map_err(|err| WorkspaceError::Unavailable(err.to_string()))?,
            kept: CommitId::new(kept_snap.git_oid.to_hex())
                .map_err(|err| WorkspaceError::Unavailable(err.to_string()))?,
            files,
        }))
    }

    fn make_way_for_start(
        &self,
        branch: &str,
        directory: &Path,
        start_tree: &Digest,
        kept: &LedgerSnapshot,
        oversized: &[(TreePath, u64)],
    ) -> Result<(), WorkspaceError> {
        if oversized.is_empty() {
            self.record_oversized(branch, directory, oversized);
            return Ok(());
        }
        let unavailable = |err: AgentFileError| {
            WorkspaceError::Unavailable(format!(
                "make way for files of {} in {branch}: {err}",
                kept.git_oid
            ))
        };
        let start_flat = self.store.flatten_tree(start_tree).map_err(map_store_err)?;
        let mut stay = Vec::new();
        for (path, size) in oversized {
            let in_way = start_flat.get(path.as_bytes()).is_some_and(|e| {
                matches!(
                    e.kind,
                    TreeEntryKind::File | TreeEntryKind::Executable | TreeEntryKind::Symlink
                )
            });
            if !in_way {
                stay.push((path.clone(), *size));
                continue;
            }
            let mut aside = format!("{SET_ASIDE_FILES}/{}/", kept.git_oid).into_bytes();
            aside.extend_from_slice(path.as_bytes());
            rename_within(directory, path, &TreePath::from_bytes(aside)).map_err(unavailable)?;
        }
        self.record_oversized(branch, directory, &stay);
        Ok(())
    }

    #[expect(clippy::too_many_arguments)]
    fn commit_snapshot_cas(
        &self,
        branch: &str,
        expected_old: &Digest,
        snap_id: &Digest,
        tree: &Digest,
        parents: &[Digest],
        git_oid: &GitOid,
        at_ms: i64,
        reason: &str,
    ) -> Result<u64, WorkspaceError> {
        self.store.cas().sync_store().map_err(map_cas_err)?;
        let _ = self
            .ledger
            .record_snapshot(snap_id, git_oid, tree, parents, &self.project, at_ms)
            .map_err(map_store_err)?;
        let seq = self
            .ledger
            .cas_ref(
                &self.project,
                branch,
                D6RefKind::Branch,
                Some(expected_old),
                snap_id,
                reason,
            )
            .map_err(map_store_err)?;
        self.store
            .cas()
            .append_ref_journal(&JournalEntry {
                seq,
                project: self.project.clone(),
                name: branch.to_string(),
                kind: D6RefKind::Branch.as_str().to_string(),
                old_snapshot: Some(expected_old.clone()),
                new_snapshot: snap_id.clone(),
                git_oid: *git_oid,
                at_ms,
            })
            .map_err(map_cas_err)?;
        Ok(seq)
    }

    fn follow_moved_branch(
        &self,
        branch: &str,
        directory: &Path,
        merged_tree: &Digest,
        lost: &WorkspaceError,
    ) -> WorkspaceError {
        let followed = self.branch_snapshot(branch).and_then(|opt| {
            let now = opt.ok_or_else(|| {
                WorkspaceError::Unavailable(format!("branch {branch} disappeared"))
            })?;
            apply_two_tree_update(&self.store, directory, merged_tree, &now.tree)?;
            Ok(now.git_oid)
        });
        match followed {
            Ok(now_oid) => WorkspaceError::Unavailable(format!(
                "{lost} (branch moved under the merge; workspace follows head {now_oid})"
            )),
            Err(err) => {
                let marked = self.mark_stale(branch);
                WorkspaceError::Unavailable(format!(
                    "{lost} (branch moved and workspace cannot follow head: {err}; marked stale{})",
                    marked.err().map_or(String::new(), |e| format!(", {e}"))
                ))
            }
        }
    }

    fn is_ancestor(&self, ancestor: &Digest, desc: &Digest) -> Result<bool, WorkspaceError> {
        if ancestor == desc {
            return Ok(true);
        }
        let mut seen = HashSet::new();
        let mut queue = VecDeque::new();
        seen.insert(desc.clone());
        queue.push_back(desc.clone());
        while let Some(id) = queue.pop_front() {
            if &id == ancestor {
                return Ok(true);
            }
            let parents = if let Some(row) = self.ledger.get_snapshot(&id).map_err(map_store_err)? {
                row.parents
            } else {
                self.store
                    .cas()
                    .read_merkle_snapshot(&id)
                    .map_err(map_cas_err)?
                    .parents
            };
            for p in parents {
                if &p == ancestor {
                    return Ok(true);
                }
                if seen.insert(p.clone()) {
                    queue.push_back(p);
                }
            }
        }
        Ok(false)
    }

    fn diff_tree_paths(&self, a: &Digest, b: &Digest) -> Result<Vec<String>, WorkspaceError> {
        let a_map = self.store.flatten_tree(a).map_err(map_store_err)?;
        let b_map = self.store.flatten_tree(b).map_err(map_store_err)?;
        let mut out = Vec::new();
        for key in a_map.keys().chain(b_map.keys()).collect::<BTreeSet<_>>() {
            if a_map.get(key) != b_map.get(key) {
                out.push(TreePath::from_bytes(key.clone()).shown());
            }
        }
        Ok(out)
    }

    // ---- Stale tracking ----

    fn stale_path(&self, branch: &str) -> PathBuf {
        self.root.join(".stale").join(branch)
    }

    fn mark_stale(&self, branch: &str) -> Result<(), WorkspaceError> {
        let path = self.stale_path(branch);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                WorkspaceError::Unavailable(format!("create {}: {err}", parent.display()))
            })?;
        }
        fs::write(&path, b"")
            .map_err(|err| WorkspaceError::Unavailable(format!("write {}: {err}", path.display())))
    }

    fn clear_stale(&self, branch: &str) {
        let _ = fs::remove_file(self.stale_path(branch));
    }

    fn is_stale(&self, branch: &str) -> bool {
        self.stale_path(branch).exists()
    }

    fn refuse_stale(&self, branch: &str) -> Result<(), WorkspaceError> {
        if !self.is_stale(branch) {
            return Ok(());
        }
        Err(stale_refusal(branch, None))
    }

    fn keep_stale_locked(&self, branch: &str, directory: &Path) -> Result<(), WorkspaceError> {
        if !self.is_stale(branch) || fs::symlink_metadata(directory).is_err() {
            return Ok(());
        }
        self.keep_stale(branch, directory).map(|_| ())
    }

    fn keep_stale(&self, branch: &str, directory: &Path) -> Result<String, WorkspaceError> {
        let head = self.branch_snapshot(branch)?.ok_or_else(|| {
            WorkspaceError::Unavailable(format!("branch {branch} has no snapshot"))
        })?;
        let main_tree = self.main_snapshot()?.map(|m| m.tree);
        let policy = TrackedSetPolicy {
            base_tree: Some(&head.tree),
            main_tree: main_tree.as_ref(),
            extra_exclude_files: self.extra_exclude_files.clone(),
        };
        let scanned = self
            .store
            .scan_workspace_tree(directory, &policy)
            .map_err(map_store_err)?;
        let (filtered, oversized) =
            self.filter_oversized(directory, scanned, main_tree.as_ref(), None)?;
        self.record_oversized(branch, directory, &oversized);
        let mutations = self.build_mutations_from_scanned(directory, &head.tree, &filtered)?;
        let stale_tree = self
            .store
            .apply_mutations(&head.tree, &mutations)
            .map_err(map_store_err)?;

        let stale_ref_name = format!("wip/stale/{branch}");
        let prev = self.branch_snapshot(&stale_ref_name)?;
        if let Some(prev_snap) = &prev
            && prev_snap.tree == stale_tree
        {
            return Ok(prev_snap.git_oid.to_hex());
        }
        let mut parents = vec![head.id.clone()];
        if let Some(prev_snap) = &prev
            && prev_snap.id != head.id
        {
            parents.push(prev_snap.id.clone());
        }
        let now_ms = self.now_ms();
        let body = self.format_body_ms(
            now_ms,
            &format!("spawn: stale workspace of {branch}, kept off the branch"),
        );
        let published = self
            .store
            .publish_snapshot(
                self.ledger.as_ref(),
                &PublishRequest {
                    project: &self.project,
                    ref_name: &stale_ref_name,
                    kind: D6RefKind::Wip,
                    expected_ref: prev.as_ref().map(|p| &p.id),
                    tree: &stale_tree,
                    parents: &parents,
                    commit_body: &body,
                    created_at_ms: now_ms,
                    reason: "d6:keep_stale",
                    landing_task_id: None,
                },
            )
            .map_err(map_store_err)?;
        Ok(published.git_oid.to_hex())
    }

    // ---- Pending merge & main copies ----

    fn pending_path(&self, branch: &str) -> PathBuf {
        self.root.join(".merge").join(branch)
    }

    fn pending_merge(&self, branch: &str) -> Option<D6PendingMerge> {
        let bytes = fs::read(self.pending_path(branch)).ok()?;
        let mut fields = nul_fields(&bytes);
        if fields.next()? != PENDING_TAG.as_bytes() {
            return None;
        }
        let main = Digest::parse(std::str::from_utf8(fields.next()?).ok()?).ok()?;
        let written_over = Digest::parse(std::str::from_utf8(fields.next()?).ok()?).ok()?;
        Some(D6PendingMerge {
            main,
            written_over,
            files: fields.map(TreePath::from_bytes).collect(),
        })
    }

    fn write_pending(
        &self,
        branch: &str,
        main: &Digest,
        head: &Digest,
        files: &[TreePath],
    ) -> Result<(), WorkspaceError> {
        let path = self.pending_path(branch);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                WorkspaceError::Unavailable(format!("create {}: {err}", parent.display()))
            })?;
        }
        let mut payload = format!("{PENDING_TAG}\0{main}\0{head}\0").into_bytes();
        for f in files {
            payload.extend_from_slice(f.as_bytes());
            payload.push(0);
        }
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        fs::write(&tmp, payload).map_err(|err| {
            WorkspaceError::Unavailable(format!("write {}: {err}", tmp.display()))
        })?;
        fs::rename(&tmp, &path)
            .map_err(|err| WorkspaceError::Unavailable(format!("rename {}: {err}", path.display())))
    }

    fn clear_pending(&self, branch: &str) {
        let _ = fs::remove_file(self.pending_path(branch));
    }

    fn settle_pending(&self, branch: &str) {
        let carried = self.pending_merge(branch).is_some_and(|pending| {
            let Ok(Some(head)) = self.branch_snapshot(branch) else {
                return false;
            };
            head.id != pending.written_over
                && matches!(self.is_ancestor(&pending.written_over, &head.id), Ok(true))
                && matches!(self.is_ancestor(&pending.main, &head.id), Ok(false))
        });
        if !carried {
            self.clear_pending(branch);
        }
    }

    fn shortest_markers(
        &self,
        sources: &[Digest],
        paths: &[TreePath],
    ) -> Result<Vec<(TreePath, usize)>, WorkspaceError> {
        let mut out: Vec<(TreePath, usize)> = paths
            .iter()
            .map(|p| (p.clone(), CONFLICT_MARKER_SIZE))
            .collect();
        if paths.is_empty() {
            return Ok(out);
        }
        for source_snap in sources {
            let tree = self.snapshot_tree(source_snap)?;
            let flat = self.store.flatten_tree(&tree).map_err(map_store_err)?;
            let root_attrs = if let Some(entry) = flat.get(b".gitattributes".as_slice())
                && let Some(digest) = entry.digest()
            {
                let raw = self.store.cas().read_blob(&digest).map_err(map_cas_err)?;
                Some(
                    GitAttributes::from_root_str(&String::from_utf8_lossy(&raw), &[])
                        .map_err(map_store_err)?,
                )
            } else {
                None
            };
            if let Some(attrs) = &root_attrs {
                for (path, shortest) in &mut out {
                    let pa = attrs.resolve_path(path.as_bytes());
                    *shortest = (*shortest).min(pa.conflict_marker_size);
                }
            }
            for (path, shortest) in &mut out {
                // Evaluate nested ancestor `.gitattributes` in `main` tree if any exist.
                let bytes = path.as_bytes();
                let mut offset = 0;
                while let Some(pos) = bytes[offset..].iter().position(|&b| b == b'/') {
                    let dir = &bytes[..offset + pos];
                    let mut attr_path = Vec::with_capacity(dir.len() + 15);
                    attr_path.extend_from_slice(dir);
                    attr_path.extend_from_slice(b"/.gitattributes");
                    if let Some(entry) = flat.get(&attr_path)
                        && let Some(digest) = entry.digest()
                    {
                        let raw = self.store.cas().read_blob(&digest).map_err(map_cas_err)?;
                        let nested =
                            GitAttributes::from_root_str(&String::from_utf8_lossy(&raw), &[])
                                .map_err(map_store_err)?;
                        let scoped = &bytes[offset + pos + 1..];
                        let pa = nested.resolve_path(scoped);
                        *shortest = (*shortest).min(pa.conflict_marker_size);
                    }
                    offset += pos + 1;
                }
            }
        }
        Ok(out)
    }

    fn copies_path(&self, branch: &str) -> PathBuf {
        self.root.join(".main-copies").join(branch)
    }

    fn read_main_copies(&self, branch: &str) -> Option<(String, Vec<TreePath>)> {
        let bytes = fs::read(self.copies_path(branch)).ok()?;
        let mut fields = nul_fields(&bytes);
        if fields.next()? != COPIES_TAG.as_bytes() {
            return None;
        }
        let main = std::str::from_utf8(fields.next()?).ok()?.to_string();
        if Digest::parse(&main).is_err() {
            return None;
        }
        Some((main, fields.map(TreePath::from_bytes).collect()))
    }

    fn clear_main_copies(&self, branch: &str) {
        let _ = fs::remove_file(self.copies_path(branch));
    }

    fn copy_mains_versions(
        &self,
        branch: &str,
        directory: &Path,
        main_id: &Digest,
        main_tree: &Digest,
        files: &[TreePath],
    ) -> Result<(), WorkspaceError> {
        let flat = self.store.flatten_tree(main_tree).map_err(map_store_err)?;
        let mut copied = Vec::new();
        for file in files {
            if let Some(entry) = flat.get(file.as_bytes())
                && matches!(entry.kind, TreeEntryKind::File | TreeEntryKind::Executable)
                && let Some(digest) = entry.digest()
            {
                let bytes = self.store.cas().read_blob(&digest).map_err(map_cas_err)?;
                let mut copy = format!("{MAIN_COPIES}/").into_bytes();
                copy.extend_from_slice(file.as_bytes());
                replace_with(directory, &TreePath::from_bytes(copy), &bytes).map_err(|err| {
                    WorkspaceError::Unavailable(format!("copy main {file}: {err}"))
                })?;
                copied.push(file);
            }
        }
        self.record_main_copies(branch, main_id, &copied)
    }

    fn record_main_copies(
        &self,
        branch: &str,
        main_id: &Digest,
        copied: &[&TreePath],
    ) -> Result<(), WorkspaceError> {
        let path = self.copies_path(branch);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                WorkspaceError::Unavailable(format!("create {}: {err}", parent.display()))
            })?;
        }
        let mut text = format!("{COPIES_TAG}\0{main_id}\0").into_bytes();
        for f in copied {
            text.extend_from_slice(f.as_bytes());
            text.push(0);
        }
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        fs::write(&tmp, text).map_err(|err| {
            WorkspaceError::Unavailable(format!("write {}: {err}", tmp.display()))
        })?;
        fs::rename(&tmp, &path)
            .map_err(|err| WorkspaceError::Unavailable(format!("rename {}: {err}", path.display())))
    }

    fn follow_main_copies(
        &self,
        branch: &str,
        main_id: &Digest,
        conflicts: &[crate::merkle_merge::MergeConflict],
        marked: bool,
    ) -> Result<(), WorkspaceError> {
        let Some((recorded, _)) = self.read_main_copies(branch) else {
            return Ok(());
        };
        if recorded != main_id.to_string() || marked {
            self.clear_main_copies(branch);
        }
        if marked {
            return Ok(());
        }
        let main_tree = self.snapshot_tree(main_id)?;
        let flat = self.store.flatten_tree(&main_tree).map_err(map_store_err)?;
        let mut copied = Vec::new();
        for c in conflicts {
            if let Some(entry) = flat.get(c.path.as_bytes())
                && matches!(entry.kind, TreeEntryKind::File | TreeEntryKind::Executable)
            {
                copied.push(&c.path);
            }
        }
        self.record_main_copies(branch, main_id, &copied)
    }

    fn restore_main_copies(&self, branch: &str, directory: &Path) -> Result<(), WorkspaceError> {
        let Some((main_str, files)) = self.read_main_copies(branch) else {
            return Ok(());
        };
        let Ok(main_id) = Digest::parse(&main_str) else {
            self.clear_main_copies(branch);
            return Ok(());
        };
        if let Ok(Some(head)) = self.branch_snapshot(branch)
            && matches!(self.is_ancestor(&main_id, &head.id), Ok(true))
        {
            self.clear_main_copies(branch);
            return Ok(());
        }
        let main_tree = self.snapshot_tree(&main_id)?;
        let flat = self.store.flatten_tree(&main_tree).map_err(map_store_err)?;
        for file in &files {
            if let Some(entry) = flat.get(file.as_bytes())
                && matches!(entry.kind, TreeEntryKind::File | TreeEntryKind::Executable)
                && let Some(digest) = entry.digest()
            {
                let bytes = self.store.cas().read_blob(&digest).map_err(map_cas_err)?;
                let mut copy = format!("{MAIN_COPIES}/").into_bytes();
                copy.extend_from_slice(file.as_bytes());
                replace_with(directory, &TreePath::from_bytes(copy), &bytes)
                    .map_err(|err| WorkspaceError::Unavailable(format!("restore {file}: {err}")))?;
            }
        }
        Ok(())
    }

    // ---- Oversized tracking & notices ----

    fn oversized_path(&self, branch: &str) -> PathBuf {
        self.root.join(".oversized").join(branch)
    }

    fn read_oversized(&self, branch: &str) -> BTreeMap<TreePath, OversizedEntry> {
        if !is_plain_name(branch) {
            return BTreeMap::new();
        }
        let Ok(bytes) = fs::read(self.oversized_path(branch)) else {
            return BTreeMap::new();
        };
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > OVERSIZED_RECORD_LIMIT {
            return BTreeMap::new();
        }
        parse_oversized(&bytes)
    }

    fn write_oversized(&self, branch: &str, record: &BTreeMap<TreePath, OversizedEntry>) {
        let path = self.oversized_path(branch);
        if record.is_empty() {
            let _ = fs::remove_file(&path);
            return;
        }
        let mut bytes = Vec::new();
        for (file, entry) in record {
            bytes.extend_from_slice(if entry.told { b"told" } else { b"untold" });
            bytes.push(0);
            bytes.extend_from_slice(entry.size.to_string().as_bytes());
            if let Some(digest) = &entry.digest {
                bytes.push(b' ');
                bytes.extend_from_slice(digest.as_bytes());
            }
            bytes.push(0);
            bytes.extend_from_slice(file.as_bytes());
            bytes.push(0);
        }
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        if fs::write(&tmp, bytes).is_ok() {
            let _ = fs::rename(&tmp, &path);
        }
    }

    fn record_oversized(&self, branch: &str, directory: &Path, oversized: &[(TreePath, u64)]) {
        let before = self.read_oversized(branch);
        let written = fs::metadata(self.oversized_path(branch))
            .and_then(|meta| meta.modified())
            .ok();
        let record: BTreeMap<TreePath, OversizedEntry> = oversized
            .iter()
            .map(|(path, size)| {
                let earlier = before.get(path);
                let told = earlier.is_some_and(|e| e.told);
                let digest = compute_oversized_digest(directory, path, *size, earlier, written);
                (
                    path.clone(),
                    OversizedEntry {
                        size: *size,
                        told,
                        digest,
                    },
                )
            })
            .collect();
        if record != before {
            self.write_oversized(branch, &record);
        }
    }

    fn oversized_notice(&self, path: &TreePath, size: u64) -> String {
        format!(
            "{path} ({} MiB) is over the {} MiB cap for one file in a checkpoint \
             (SPAWN_MAX_FILE_MB), so checkpoints leave it out: it is not on your branch, \
             and it is lost when this workspace is unmounted. Make it smaller, split it, \
             or make sure it can be generated again from what is committed.",
            size.div_ceil(1 << 20),
            self.max_file_bytes >> 20
        )
    }

    fn untold_oversized(&self, branch: &str) -> Vec<String> {
        self.read_oversized(branch)
            .iter()
            .filter(|(_, e)| !e.told)
            .map(|(path, e)| self.oversized_notice(path, e.size))
            .collect()
    }

    fn oversized_told(&self, branch: &str, notices: &[String]) {
        let Ok(_lock) = self.lock_branch(branch) else {
            return;
        };
        let mut record = self.read_oversized(branch);
        let mut changed = false;
        for (path, entry) in &mut record {
            if !entry.told && notices.contains(&self.oversized_notice(path, entry.size)) {
                entry.told = true;
                changed = true;
            }
        }
        if changed {
            self.write_oversized(branch, &record);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct D6PendingMerge {
    main: Digest,
    written_over: Digest,
    files: Vec<TreePath>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OversizedEntry {
    size: u64,
    told: bool,
    digest: Option<String>,
}

fn system_now_ms() -> i64 {
    let dur = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(dur.as_millis()).unwrap_or(i64::MAX)
}

fn map_store_err(err: D6StoreError) -> WorkspaceError {
    match err {
        D6StoreError::IgnoredLeak { path, rule } => WorkspaceError::Refused {
            code: "TrackedSetLeak".to_string(),
            message: format!("untracked ignored file {path:?} matched {rule:?}"),
        },
        other => WorkspaceError::Unavailable(other.to_string()),
    }
}

#[expect(clippy::needless_pass_by_value)]
fn map_cas_err(err: spawn_cas::CasError) -> WorkspaceError {
    WorkspaceError::Unavailable(err.to_string())
}

fn is_plain_name(name: &str) -> bool {
    use std::path::Component;
    if name.is_empty() || name.contains(['/', '\\', '\0']) {
        return false;
    }
    let mut comps = Path::new(name).components();
    matches!(
        (comps.next(), comps.next()),
        (Some(Component::Normal(part)), None) if part == OsStr::new(name)
    )
}

fn extract_commit_message(commit_body: &[u8]) -> String {
    if let Some(pos) = commit_body.windows(2).position(|w| w == b"\n\n") {
        String::from_utf8_lossy(&commit_body[pos + 2..]).into_owned()
    } else {
        String::new()
    }
}

fn parse_committer_ms(commit_body: &[u8]) -> i64 {
    for raw_line in commit_body.split(|&b| b == b'\n') {
        if raw_line.is_empty() {
            break;
        }
        if let Some(rest) = raw_line.strip_prefix(b"committer ")
            && let Ok(text) = std::str::from_utf8(rest)
        {
            let mut parts = text.split_whitespace().rev();
            let _tz = parts.next();
            if let Some(secs_str) = parts.next()
                && let Ok(secs) = secs_str.parse::<i64>()
            {
                return secs.saturating_mul(1000);
            }
        }
    }
    0
}

fn split_rel_segments(rel_path: &[u8]) -> Vec<Vec<u8>> {
    rel_path.split(|&b| b == b'/').map(<[u8]>::to_vec).collect()
}

fn has_ancestor_path(seen: &BTreeSet<Vec<u8>>, rel_path: &[u8]) -> bool {
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

fn shown_paths(paths: &[TreePath]) -> Vec<String> {
    paths.iter().map(TreePath::shown).collect()
}

fn stale_refusal(branch: &str, kept: Option<&str>) -> WorkspaceError {
    let kept = kept.map_or_else(String::new, |commit| {
        format!(" Its state is kept off branch as {commit}.")
    });
    WorkspaceError::Refused {
        code: WORKSPACE_STALE.to_string(),
        message: format!(
            "workspace of {branch} could not follow moved branch.{kept} Re-mount to continue."
        ),
    }
}

fn is_pure_content_conflicts(conflicts: &[crate::merkle_merge::MergeConflict]) -> bool {
    !conflicts.is_empty()
        && conflicts
            .iter()
            .all(|c| matches!(c.kind, ConflictKind::Content | ConflictKind::AddAdd))
}

fn unresolved_conflicts(directory: &Path, files: &[(TreePath, usize)]) -> bool {
    files.iter().any(
        |(file, shortest)| match read_regular(directory, file, CONFLICT_READ_LIMIT) {
            Ok(Some((bytes, _, more))) => more || has_conflict_block_sized(&bytes, *shortest),
            Ok(None) | Err(AgentFileError::NotRegular(_) | AgentFileError::Unsafe(_)) => false,
            Err(AgentFileError::Failed(_)) => true,
        },
    )
}

fn parse_oversized(bytes: &[u8]) -> BTreeMap<TreePath, OversizedEntry> {
    let bytes = bytes.strip_suffix(b"\0").unwrap_or(bytes);
    if bytes.is_empty() {
        return BTreeMap::new();
    }
    let fields: Vec<&[u8]> = bytes.split(|b| *b == 0).collect();
    fields
        .chunks_exact(3)
        .filter_map(|entry| {
            let told = match entry[0] {
                b"told" => true,
                b"untold" => false,
                _ => return None,
            };
            let sized = std::str::from_utf8(entry[1]).ok()?;
            let (size, digest) = match sized.split_once(' ') {
                Some((s, d)) => (s, is_sha256_str(d).then_some(d)),
                None => (sized, None),
            };
            let size = size.parse().ok()?;
            if entry[2].is_empty() {
                return None;
            }
            Some((
                TreePath::from_bytes(entry[2]),
                OversizedEntry {
                    size,
                    told,
                    digest: digest.map(str::to_string),
                },
            ))
        })
        .collect()
}

fn is_sha256_str(text: &str) -> bool {
    text.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

fn compute_oversized_digest(
    directory: &Path,
    path: &TreePath,
    size: u64,
    earlier: Option<&OversizedEntry>,
    written: Option<SystemTime>,
) -> Option<String> {
    let opened = open_regular(directory, path).ok()??;
    if opened.metadata.len() != size {
        return None;
    }
    if let (Some(earlier), Some(written), Ok(modified)) =
        (earlier, written, opened.metadata.modified())
        && earlier.size == size
        && earlier.digest.is_some()
        && modified < written
    {
        return earlier.digest.clone();
    }
    let mut file = opened.file;
    let digest = sha256_reader(&mut file).ok()?;
    Some(format!("sha256:{}", digest.hex()))
}

fn clone_layer_to_writable(layer_dir: &Path, staged_dir: &Path) -> Result<(), WorkspaceError> {
    fs::create_dir(staged_dir).map_err(|err| {
        WorkspaceError::Unavailable(format!("create {}: {err}", staged_dir.display()))
    })?;
    fs::set_permissions(staged_dir, fs::Permissions::from_mode(0o755)).map_err(|err| {
        WorkspaceError::Unavailable(format!("chmod {}: {err}", staged_dir.display()))
    })?;
    copy_layer_recursive(layer_dir, staged_dir)
}

fn copy_layer_recursive(src_dir: &Path, dst_dir: &Path) -> Result<(), WorkspaceError> {
    let mut entries: Vec<fs::DirEntry> = Vec::new();
    for entry_res in fs::read_dir(src_dir)
        .map_err(|err| WorkspaceError::Unavailable(format!("read {}: {err}", src_dir.display())))?
    {
        let entry = entry_res.map_err(|err| {
            WorkspaceError::Unavailable(format!("read entry {}: {err}", src_dir.display()))
        })?;
        entries.push(entry);
    }
    entries.sort_by_key(fs::DirEntry::file_name);

    for entry in entries {
        let name = entry.file_name();
        let src = src_dir.join(&name);
        let dst = dst_dir.join(&name);
        let meta = fs::symlink_metadata(&src)
            .map_err(|err| WorkspaceError::Unavailable(format!("stat {}: {err}", src.display())))?;
        let ftype = meta.file_type();
        if ftype.is_dir() {
            fs::create_dir(&dst).map_err(|err| {
                WorkspaceError::Unavailable(format!("mkdir {}: {err}", dst.display()))
            })?;
            fs::set_permissions(&dst, fs::Permissions::from_mode(0o755)).map_err(|err| {
                WorkspaceError::Unavailable(format!("chmod {}: {err}", dst.display()))
            })?;
            copy_layer_recursive(&src, &dst)?;
        } else if ftype.is_symlink() {
            let target = fs::read_link(&src).map_err(|err| {
                WorkspaceError::Unavailable(format!("readlink {}: {err}", src.display()))
            })?;
            std::os::unix::fs::symlink(&target, &dst).map_err(|err| {
                WorkspaceError::Unavailable(format!("symlink {}: {err}", dst.display()))
            })?;
        } else if ftype.is_file() {
            clone_or_copy(&src, &dst).map_err(|err| {
                WorkspaceError::Unavailable(format!(
                    "clone {} -> {}: {err}",
                    src.display(),
                    dst.display()
                ))
            })?;
            let mode = meta.permissions().mode() & 0o777;
            fs::set_permissions(&dst, fs::Permissions::from_mode(mode)).map_err(|err| {
                WorkspaceError::Unavailable(format!("chmod {}: {err}", dst.display()))
            })?;
        }
    }
    Ok(())
}

/// Fail-closed two-tree working directory updater matching `git read-tree -m -u from to`:
/// refuses with `WorkspaceDiffers` and zero disk writes if any path modified/removed between
/// `from` and `to` diverges on disk, or if any newly added path collides on disk.
fn apply_two_tree_update(
    store: &CasMerkleStore,
    directory: &Path,
    from_tree: &Digest,
    to_tree: &Digest,
) -> Result<Vec<String>, WorkspaceError> {
    if from_tree == to_tree {
        return Ok(Vec::new());
    }
    let from_map = store.flatten_tree(from_tree).map_err(map_store_err)?;
    let to_map = store.flatten_tree(to_tree).map_err(map_store_err)?;

    let mut diff_paths = Vec::new();
    for key in from_map
        .keys()
        .chain(to_map.keys())
        .collect::<BTreeSet<_>>()
    {
        let f_ent = from_map.get(key).cloned();
        let t_ent = to_map.get(key).cloned();
        if f_ent != t_ent {
            let path = TreePath::from_bytes(key.clone());
            if !path.is_safe() {
                return Err(WorkspaceError::Unavailable(format!(
                    "disallowed path {path} in tree update"
                )));
            }
            diff_paths.push((path, f_ent, t_ent));
        }
    }
    if diff_paths.is_empty() {
        return Ok(Vec::new());
    }

    let removed_set: HashSet<Vec<u8>> = diff_paths
        .iter()
        .filter(|(_, f, t)| f.is_some() && t.is_none())
        .map(|(p, _, _)| p.as_bytes().to_vec())
        .collect();

    // Phase 1: verify clean workspace preconditions without mutating a single byte.
    for (path, f_opt, t_opt) in &diff_paths {
        let full = directory.join(path.as_path());
        match f_opt {
            Some(expected) => verify_disk_matches_entry(store, directory, path, &full, expected)?,
            None => {
                // Newly added by merge/set-aside: must not overwrite an untracked entry on disk,
                // except an existing directory whose entire contents are scheduled for removal.
                if let Ok(meta) = fs::symlink_metadata(&full) {
                    let replacing_cleared_dir = !meta.file_type().is_symlink()
                        && meta.is_dir()
                        && dir_contains_only_removed(&full, path.as_bytes(), &removed_set);
                    if !replacing_cleared_dir {
                        return Err(workspace_differs(format!(
                            "untracked entry already exists at {path}"
                        )));
                    }
                }
            }
        }
        if t_opt.is_some() {
            verify_parent_chain_clear_for_write(directory, path, &removed_set)?;
        }
    }

    // Phase 2: apply removals + empty parent pruning first, then creations/updates.
    for (path, f_opt, t_opt) in &diff_paths {
        if f_opt.is_some() && t_opt.is_none() {
            let full = directory.join(path.as_path());
            remove_path_nofollow(&full)?;
            prune_empty_parents(directory, &full);
        }
    }

    for (path, _, t_opt) in &diff_paths {
        let Some(target) = t_opt else {
            continue;
        };
        let full = directory.join(path.as_path());
        write_entry_from_cas(store, directory, &full, path, target)?;
    }

    Ok(diff_paths.into_iter().map(|(p, _, _)| p.shown()).collect())
}

fn dir_contains_only_removed(
    dir: &Path,
    rel_prefix: &[u8],
    removed_set: &HashSet<Vec<u8>>,
) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    for entry_res in entries {
        let Ok(entry) = entry_res else {
            return false;
        };
        let os_name = entry.file_name();
        let mut child_rel = Vec::with_capacity(rel_prefix.len() + 1 + os_name.len());
        child_rel.extend_from_slice(rel_prefix);
        child_rel.push(b'/');
        child_rel.extend_from_slice(os_name.as_bytes());
        let Ok(meta) = fs::symlink_metadata(entry.path()) else {
            return false;
        };
        if meta.is_dir() && !meta.file_type().is_symlink() {
            if !removed_set.contains(&child_rel)
                && !dir_contains_only_removed(&entry.path(), &child_rel, removed_set)
            {
                return false;
            }
        } else if !removed_set.contains(&child_rel) {
            return false;
        }
    }
    true
}

fn workspace_differs(detail: String) -> WorkspaceError {
    WorkspaceError::Refused {
        code: "WorkspaceDiffers".to_string(),
        message: detail,
    }
}

fn verify_disk_matches_entry(
    _store: &CasMerkleStore,
    directory: &Path,
    rel: &TreePath,
    full: &Path,
    expected: &TreeEntry,
) -> Result<(), WorkspaceError> {
    // Reject any symlink on parent components from `directory`.
    let mut prefix = PathBuf::new();
    let comps: Vec<&[u8]> = rel.components().collect();
    for seg in &comps[..comps.len().saturating_sub(1)] {
        prefix.push(OsStr::from_bytes(seg));
        let p = directory.join(&prefix);
        match fs::symlink_metadata(&p) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                return Err(workspace_differs(format!(
                    "parent component {} differs from tracked tree",
                    p.display()
                )));
            }
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Err(workspace_differs(format!("tracked path {rel} was deleted")));
            }
            Err(err) => {
                return Err(WorkspaceError::Unavailable(format!(
                    "stat {}: {err}",
                    p.display()
                )));
            }
        }
    }

    let meta = match fs::symlink_metadata(full) {
        Ok(m) => m,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(workspace_differs(format!("tracked path {rel} was deleted")));
        }
        Err(err) => {
            return Err(WorkspaceError::Unavailable(format!(
                "stat {}: {err}",
                full.display()
            )));
        }
    };

    match expected.kind {
        TreeEntryKind::Gitlink => {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(workspace_differs(format!(
                    "gitlink directory {rel} is no longer a directory"
                )));
            }
        }
        TreeEntryKind::Symlink => {
            if !meta.file_type().is_symlink() {
                return Err(workspace_differs(format!(
                    "symlink {rel} was replaced on disk"
                )));
            }
            let target = fs::read_link(full).map_err(|err| {
                WorkspaceError::Unavailable(format!("readlink {}: {err}", full.display()))
            })?;
            let digest = sha256_of(target.as_os_str().as_bytes());
            if digest.as_bytes() != expected.id {
                return Err(workspace_differs(format!(
                    "symlink {rel} has uncommitted edits"
                )));
            }
        }
        TreeEntryKind::File | TreeEntryKind::Executable => {
            if !meta.file_type().is_file() || meta.len() != expected.size {
                return Err(workspace_differs(format!(
                    "file {rel} has uncommitted edits"
                )));
            }
            let is_exec = meta.permissions().mode() & 0o111 != 0;
            if is_exec != (expected.kind == TreeEntryKind::Executable) {
                return Err(workspace_differs(format!(
                    "file {rel} mode changed in workspace"
                )));
            }
            let mut f = open_regular_file_nofollow(full)?;
            let digest = sha256_reader(&mut f).map_err(|err| {
                WorkspaceError::Unavailable(format!("hash {}: {err}", full.display()))
            })?;
            if digest.as_bytes() != expected.id {
                return Err(workspace_differs(format!(
                    "file {rel} has uncommitted edits"
                )));
            }
        }
        TreeEntryKind::Dir => {}
    }
    Ok(())
}

fn verify_parent_chain_clear_for_write(
    directory: &Path,
    rel: &TreePath,
    removed_set: &HashSet<Vec<u8>>,
) -> Result<(), WorkspaceError> {
    let comps: Vec<&[u8]> = rel.components().collect();
    let mut cur = Vec::new();
    for seg in &comps[..comps.len().saturating_sub(1)] {
        if !cur.is_empty() {
            cur.push(b'/');
        }
        cur.extend_from_slice(seg);
        let p = directory.join(OsStr::from_bytes(&cur));
        match fs::symlink_metadata(&p) {
            Ok(meta) => {
                if !meta.file_type().is_symlink() && meta.is_dir() {
                    continue;
                }
                if removed_set.contains(&cur) {
                    continue;
                }
                return Err(workspace_differs(format!(
                    "non-directory path at ancestor {} blocks writing {rel}",
                    String::from_utf8_lossy(&cur)
                )));
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(WorkspaceError::Unavailable(format!(
                    "stat {}: {err}",
                    p.display()
                )));
            }
        }
    }
    Ok(())
}

fn write_entry_from_cas(
    store: &CasMerkleStore,
    directory: &Path,
    full: &Path,
    rel: &TreePath,
    entry: &TreeEntry,
) -> Result<(), WorkspaceError> {
    if let Some(parent) = full.parent()
        && parent != directory
    {
        fs::create_dir_all(parent).map_err(|err| {
            WorkspaceError::Unavailable(format!("mkdir {}: {err}", parent.display()))
        })?;
    }
    remove_path_nofollow(full)?;
    match entry.kind {
        TreeEntryKind::Gitlink => {
            fs::create_dir_all(full)
                .map_err(|err| WorkspaceError::Unavailable(format!("mkdir {rel}: {err}")))?;
            fs::set_permissions(full, fs::Permissions::from_mode(0o755))
                .map_err(|err| WorkspaceError::Unavailable(format!("chmod {rel}: {err}")))?;
        }
        TreeEntryKind::Symlink => {
            let digest = entry.digest().ok_or_else(|| {
                WorkspaceError::Unavailable(format!("symlink {rel} missing digest"))
            })?;
            let bytes = store.cas().read_blob(&digest).map_err(map_cas_err)?;
            std::os::unix::fs::symlink(OsStr::from_bytes(&bytes), full)
                .map_err(|err| WorkspaceError::Unavailable(format!("symlink {rel}: {err}")))?;
        }
        TreeEntryKind::File | TreeEntryKind::Executable => {
            let mode = if entry.kind == TreeEntryKind::Executable {
                0o755
            } else {
                0o644
            };
            let digest = entry
                .digest()
                .ok_or_else(|| WorkspaceError::Unavailable(format!("file {rel} missing digest")))?;
            let mut out = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(full)
                .map_err(|err| WorkspaceError::Unavailable(format!("create {rel}: {err}")))?;
            store
                .cas()
                .stream_blob(&digest, &mut out)
                .map_err(map_cas_err)?;
            out.set_permissions(fs::Permissions::from_mode(mode))
                .map_err(|err| WorkspaceError::Unavailable(format!("chmod {rel}: {err}")))?;
        }
        TreeEntryKind::Dir => {}
    }
    Ok(())
}

fn remove_path_nofollow(path: &Path) -> Result<(), WorkspaceError> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.is_dir() && !meta.file_type().is_symlink() {
                fs::remove_dir_all(path).map_err(|err| {
                    WorkspaceError::Unavailable(format!("rmdir {}: {err}", path.display()))
                })?;
            } else {
                fs::remove_file(path).map_err(|err| {
                    WorkspaceError::Unavailable(format!("rm {}: {err}", path.display()))
                })?;
            }
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(WorkspaceError::Unavailable(format!(
            "stat {}: {err}",
            path.display()
        ))),
    }
}

fn prune_empty_parents(root: &Path, file_path: &Path) {
    let mut cur = file_path.parent();
    while let Some(dir) = cur {
        if dir == root {
            break;
        }
        if fs::remove_dir(dir).is_err() {
            break;
        }
        cur = dir.parent();
    }
}

fn open_regular_file_nofollow(path: &Path) -> Result<fs::File, WorkspaceError> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(|err| WorkspaceError::Unavailable(format!("open {}: {err}", path.display())))?;
    let meta = file
        .metadata()
        .map_err(|err| WorkspaceError::Unavailable(format!("stat {}: {err}", path.display())))?;
    if !meta.is_file() {
        return Err(WorkspaceError::Unavailable(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    Ok(file)
}

/// Intentional negative-test helper: scans a workspace while bypassing `.gitignore`
/// filtering (still excluding `.git` internal components, `.spawn/`, `target/`, and
/// `ENGINE_RUNTIME_FILES` unless tracked in both `base_tree` and `main_tree`) so the
/// shadow oracle can prove fail-closed detection of ignored untracked leaks.
fn scan_without_gitignore(
    workspace_dir: &Path,
    base_tree: Option<&Digest>,
    main_tree: Option<&Digest>,
    store: &CasMerkleStore,
) -> Result<Vec<ScannedEntry>, WorkspaceError> {
    let base_map = match base_tree {
        Some(t) => store.flatten_tree(t).map_err(map_store_err)?,
        None => BTreeMap::new(),
    };
    let main_map = match main_tree {
        Some(t) => store.flatten_tree(t).map_err(map_store_err)?,
        None => BTreeMap::new(),
    };
    let mut entries = Vec::new();
    walk_unfiltered(workspace_dir, b"", &base_map, &main_map, &mut entries)?;
    entries.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    Ok(entries)
}

fn walk_unfiltered(
    dir: &Path,
    rel_dir: &[u8],
    base_map: &BTreeMap<Vec<u8>, TreeEntry>,
    main_map: &BTreeMap<Vec<u8>, TreeEntry>,
    out: &mut Vec<ScannedEntry>,
) -> Result<(), WorkspaceError> {
    let mut children = Vec::new();
    for entry_res in fs::read_dir(dir)
        .map_err(|err| WorkspaceError::Unavailable(format!("read_dir {}: {err}", dir.display())))?
    {
        children.push(entry_res.map_err(|err| {
            WorkspaceError::Unavailable(format!("read_dir entry {}: {err}", dir.display()))
        })?);
    }
    children.sort_by_key(fs::DirEntry::file_name);

    for entry in children {
        let os_name = entry.file_name();
        let name = os_name.as_bytes();
        if !is_valid_entry_name(name) {
            continue;
        }
        let mut rel = Vec::with_capacity(rel_dir.len() + 1 + name.len());
        if !rel_dir.is_empty() {
            rel.extend_from_slice(rel_dir);
            rel.push(b'/');
        }
        rel.extend_from_slice(name);

        let meta = fs::symlink_metadata(entry.path()).map_err(|err| {
            WorkspaceError::Unavailable(format!("stat {}: {err}", entry.path().display()))
        })?;
        let ftype = meta.file_type();
        if ftype.is_dir() {
            if let Some(base_ent) = base_map.get(&rel)
                && base_ent.kind == TreeEntryKind::Gitlink
                && let Some(oid) = base_ent.gitlink_oid()
            {
                out.push(ScannedEntry {
                    path: TreePath::from_bytes(rel),
                    kind: ScannedKind::Gitlink { oid: oid.to_vec() },
                });
                continue;
            }
            let in_both = has_descendant(base_map, &rel) && has_descendant(main_map, &rel);
            if (is_build_output(&rel) || is_engine_runtime(&rel)) && !in_both {
                continue;
            }
            walk_unfiltered(&entry.path(), &rel, base_map, main_map, out)?;
        } else if ftype.is_symlink() {
            let in_both = base_map.contains_key(&rel) && main_map.contains_key(&rel);
            if (is_build_output(&rel) || is_engine_runtime(&rel)) && !in_both {
                continue;
            }
            out.push(ScannedEntry {
                path: TreePath::from_bytes(rel),
                kind: ScannedKind::Symlink,
            });
        } else if ftype.is_file() {
            let in_both = base_map.contains_key(&rel) && main_map.contains_key(&rel);
            if (is_build_output(&rel) || is_engine_runtime(&rel)) && !in_both {
                continue;
            }
            let kind = if meta.permissions().mode() & 0o111 != 0 {
                ScannedKind::Executable
            } else {
                ScannedKind::File
            };
            out.push(ScannedEntry {
                path: TreePath::from_bytes(rel),
                kind,
            });
        }
    }
    Ok(())
}

fn has_descendant(map: &BTreeMap<Vec<u8>, TreeEntry>, rel_dir: &[u8]) -> bool {
    let mut prefix = Vec::with_capacity(rel_dir.len() + 1);
    prefix.extend_from_slice(rel_dir);
    prefix.push(b'/');
    map.range(prefix.clone()..)
        .next()
        .is_some_and(|(k, _)| k.starts_with(&prefix))
}

fn is_build_output(rel: &[u8]) -> bool {
    D6_BUILD_OUTPUT.iter().any(|root| {
        let p = root.as_bytes();
        rel == p || (rel.starts_with(p) && rel.get(p.len()) == Some(&b'/'))
    })
}

fn is_engine_runtime(rel: &[u8]) -> bool {
    ENGINE_RUNTIME_FILES.iter().any(|entry| {
        let p = entry.strip_suffix('/').unwrap_or(entry).as_bytes();
        if rel == p || (rel.starts_with(p) && rel.get(p.len()) == Some(&b'/')) {
            return true;
        }
        let mut offset = 0;
        while let Some(pos) = rel[offset..].iter().position(|&b| b == b'/') {
            let tail = &rel[offset + pos + 1..];
            if tail == p || (tail.starts_with(p) && tail.get(p.len()) == Some(&b'/')) {
                return true;
            }
            offset += pos + 1;
        }
        false
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;
    use std::sync::atomic::{AtomicI64, Ordering};

    use spawn_cas::Tree;

    use super::*;
    use crate::Scratch;
    use crate::cas_merkle::InMemoryRefLedger;

    fn test_workspace() -> (
        Scratch,
        Arc<CasMerkleStore>,
        Arc<InMemoryRefLedger>,
        D6Workspace<InMemoryRefLedger>,
        Arc<AtomicI64>,
    ) {
        let scratch = Scratch::new("d6-ws-unit");
        let cas = Arc::new(CasMerkleStore::open(scratch.join("cas")).expect("open cas"));
        let ledger = Arc::new(InMemoryRefLedger::new());
        let clock_ms = Arc::new(AtomicI64::new(1_700_000_000_000));
        let c = clock_ms.clone();
        let ws = D6Workspace::new("proj", scratch.join("work"), cas.clone(), ledger.clone())
            .with_clock_ms(Arc::new(move || c.fetch_add(1_000, Ordering::Relaxed)));
        (scratch, cas, ledger, ws, clock_ms)
    }

    #[test]
    fn pure_rust_d6_workspace_lifecycle_warm_layer0_merge_set_aside_and_discard() {
        let (_scratch, cas, _ledger, store, clock) = test_workspace();
        let b1 = BranchName::new("agent_1").expect("branch");
        let made = store.ensure_branch(&b1, None).expect("ensure_branch");
        assert!(matches!(made, Branch::Created(_)));
        assert_eq!(
            store.ensure_branch(&b1, None).expect("idempotent"),
            Branch::Existing
        );

        // Seed main with initial files using land_main_snapshot
        let (readme_d, readme_len) = cas.put_bytes_blob(b"base\n").expect("put blob");
        let (ignore_d, ignore_len) = cas.put_bytes_blob(b".env\n").expect("put gitignore");
        let t0 = cas
            .put_tree_cached(
                &Tree::from_entries(vec![
                    TreeEntry::file(b".gitignore".to_vec(), &ignore_d, ignore_len),
                    TreeEntry::file(b"README.md".to_vec(), &readme_d, readme_len),
                ])
                .expect("tree"),
            )
            .expect("put tree");
        let main0 = store.main_snapshot().expect("main0").expect("present");
        let body = format_commit_body(
            "spawn <s@l> 1700000010 +0000",
            "spawn <s@l> 1700000010 +0000",
            "seed files\n",
        );
        let snap1 = cas
            .cas()
            .put_merkle_snapshot(&Snapshot {
                tree: t0.clone(),
                parents: vec![main0.id.clone()],
                commit_body: body,
            })
            .expect("put snap");
        store
            .land_main_snapshot(Some(&main0.id), &snap1, Some(1), "test seed")
            .expect("land main");

        let b2 = BranchName::new("agent_2").expect("branch 2");
        store.ensure_branch(&b2, None).expect("ensure branch 2");
        let ws2 = store.mount(&b2).expect("mount b2");
        assert!(store.is_mounted(&ws2));
        let ws2_dir = PathBuf::from(ws2.path());

        // Check warm layer materialized & never hard-linked to CAS objects
        let layer_file = store.layer_path(&t0).join("README.md");
        let cas_obj = cas.cas().object_path(&readme_d);
        assert_ne!(
            fs::symlink_metadata(&layer_file).expect("layer meta").ino(),
            fs::symlink_metadata(&cas_obj).expect("cas meta").ino()
        );

        // Write ignored .env + tracked executable + symlink, verify commit omits .env
        fs::write(ws2_dir.join(".env"), "SECRET=1\n").expect("write .env");
        fs::write(ws2_dir.join("run.sh"), "#!/bin/sh\necho hi\n").expect("write sh");
        fs::set_permissions(ws2_dir.join("run.sh"), fs::Permissions::from_mode(0o755))
            .expect("chmod");
        std::os::unix::fs::symlink("README.md", ws2_dir.join("link.txt")).expect("symlink");
        fs::write(ws2_dir.join("README.md"), "branch edit\n").expect("edit");

        let c1 = store.checkpoint(&ws2, "work 1").expect("checkpoint");
        assert!(matches!(c1, Commit::Created(_)));
        assert_eq!(store.commit(&ws2, "noop").expect("noop"), Commit::Unchanged);
        let head2 = store
            .branch_snapshot("agent_2")
            .expect("get")
            .expect("head");
        let flat2 = cas.flatten_tree(&head2.tree).expect("flatten");
        assert!(!flat2.contains_key(b".env".as_slice()));
        assert_eq!(flat2[b"run.sh".as_slice()].kind, TreeEntryKind::Executable);
        assert_eq!(flat2[b"link.txt".as_slice()].kind, TreeEntryKind::Symlink);

        // Next mount reuses incremental nearest warm layer
        let ws3_branch = BranchName::new("agent_3").expect("b3");
        store
            .ensure_branch(&ws3_branch, Some("agent_2"))
            .expect("fork from agent_2");
        let ws3 = store.mount(&ws3_branch).expect("mount b3");
        let report3 = store.last_layer0_report().expect("layer0 report");
        assert!(report3.hardlinked_from_nearest >= 1 || report3.reused_existing_dest);
        store.discard(&ws3).expect("discard ws3");
        assert!(!store.is_mounted(&ws3));

        // Advance main conflictingly and test merge_base -> diff3 markers -> resolution
        verify_conflict_merge_hold_and_set_aside(
            &cas, &store, &clock, &b2, &ws2, &ws2_dir, &t0, &snap1,
        );
    }

    #[expect(clippy::too_many_arguments)]
    fn verify_conflict_merge_hold_and_set_aside(
        cas: &CasMerkleStore,
        store: &D6Workspace<InMemoryRefLedger>,
        clock: &AtomicI64,
        b2: &BranchName,
        ws2: &WorkspaceId,
        ws2_dir: &Path,
        t0: &Digest,
        snap1: &Digest,
    ) {
        let (main_edit_d, main_edit_len) = cas.put_bytes_blob(b"main edit\n").expect("blob");
        let t_main2 = cas
            .apply_mutations(
                t0,
                &[TreeMutation::Upsert {
                    path: vec![b"README.md".to_vec()],
                    kind: TreeEntryKind::File,
                    id: main_edit_d.as_bytes(),
                    size: main_edit_len,
                }],
            )
            .expect("apply");
        let ts = clock.fetch_add(1_000, Ordering::Relaxed);
        let body2 = format_commit_body(
            &format!("spawn <s@l> {} +0000", ts / 1000),
            &format!("spawn <s@l> {} +0000", ts / 1000),
            "main conflict\n",
        );
        let snap_main2 = cas
            .cas()
            .put_merkle_snapshot(&Snapshot {
                tree: t_main2,
                parents: vec![snap1.clone()],
                commit_body: body2,
            })
            .expect("put main2");
        store
            .land_main_snapshot(Some(snap1), &snap_main2, Some(2), "main2")
            .expect("land main2");

        let merge_res = store.merge_base(ws2).expect("merge_base");
        assert!(matches!(
            merge_res,
            BaseMerge::Conflicted { marked: true, .. }
        ));
        assert!(store.has_pending_merge("agent_2"));
        let marked_txt = fs::read_to_string(ws2_dir.join("README.md")).expect("read conflict");
        assert!(
            marked_txt.contains("<<<<<<<")
                && marked_txt.contains("|||||||")
                && marked_txt.contains(">>>>>>>")
        );

        // Hold lock blocks concurrent merge_base_when until dropped
        let guard = store.hold(ws2);
        assert!(guard.is_some());
        drop(guard);

        fs::write(ws2_dir.join("README.md"), "resolved both\n").expect("resolve");
        let resolved = store.commit(ws2, "resolved").expect("commit resolved");
        assert!(matches!(resolved, Commit::Created(_)));
        assert!(!store.has_pending_merge("agent_2"));
        let resolved_snap = store
            .branch_snapshot("agent_2")
            .expect("get")
            .expect("head");
        assert_eq!(resolved_snap.parents.len(), 2);

        // Set aside back to snap_main2 (merge base with main) and verify idempotency
        let sa1 = store
            .set_aside_when(b2, None, "cancel task", &|| Ok(()))
            .expect("set_aside");
        assert!(matches!(sa1, SetAside::Created { .. }));
        let sa2 = store
            .set_aside_when(b2, None, "cancel task", &|| Ok(()))
            .expect("idempotent retry");
        assert_eq!(sa1, sa2);

        store.unmount(ws2).expect("unmount ws2");
        assert!(!store.is_mounted(ws2));
    }
}
