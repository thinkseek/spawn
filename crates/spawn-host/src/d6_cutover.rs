//! D6 Phase 4 Step 4 (`#113` Comment #5 §三.3, 7.2.2–7.2.7):
//! Lossless 7-step per-project cutover & rollback state machine over generic
//! `L: RefLedger`, pre-cutover `.merge/*` / `pending` zeroing verification,
//! `refs.journal` crash-recovery fast-forward replay, GC Fence checks, and
//! offline R2 `git bundle` export -> blank-store import roundtrip drills.
//!
//! Safety invariants:
//! - Depends strictly on `spawn-host` + `spawn-cas` (zero database dependencies, zero forbidden OS code).
//! - All Git commands go exclusively through [`crate::git::git_command`].

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fmt::{self, Write as _};
use std::fs;
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use spawn_cas::{
    Digest, GIT_POINTER_THRESHOLD, GcFenceBinding, GitOid, NS_BLOB, PersistentGcReport, Snapshot,
    Store, Tree, TreeEntry, git_pointer_bytes,
};

use crate::cas_merkle::{CasMerkleStore, D6RefKind, D6StoreError, RefLedger, is_pointer_sized};
use crate::d6_layer0::{ExportAttributesAudit, audit_export_attributes};
use crate::git::git_command;
use crate::git_border::{GitBorderAdapter, ImportReport};
use crate::tree_path::{TreePath, nul_fields};

static CUTOVER_TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Active storage backing of a project across cutover (`#113` Comment #5 §三.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectStorageMode {
    /// Legacy bare Git + worktree backend.
    Git,
    /// D6 persistent CAS Merkle + PostgreSQL transactional ref ledger + read-only Git border.
    D6Cas,
}

/// Phase within the lossless 7-step per-project cutover and rollback workflow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CutoverPhase {
    /// Initial state before Step 1 backfill.
    NotStarted,
    /// Step 1 complete: reachable Git commits/trees/blobs backfilled into CAS + `spawn_snapshots`.
    BackfillImported,
    /// Step 2 complete: dispatch paused and zero `.merge/*` / `pending` markers verified.
    DispatchPausedAndDrained,
    /// Step 3 complete: post-Step-1 tail commits imported cleanly.
    TailImported,
    /// Step 4 complete: `spawn_refs` committed atomically, `refs.journal` updated, mode flipped to `D6Cas`.
    RefsCommitted,
    /// Step 5 complete: every ref verified `RefLedger git_oid == git rev-parse`.
    ParityVerified,
    /// Step 6 complete: original bare repository locked read-only (`pre-receive` + deny configs).
    BorderReadOnly,
    /// Step 7 complete: project dispatch resumed on `D6Cas`.
    Resumed,
    /// Rolled back to `Git` after exporting refs and restoring bare repo writability.
    RolledBack,
}

/// Recorded transition audit event emitted during [`ProjectCutover`] steps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CutoverEvent {
    /// Resulting phase after transition.
    pub phase: CutoverPhase,
    /// Project storage mode active at transition time.
    pub storage_mode: ProjectStorageMode,
    /// Human-readable audit summary.
    pub detail: String,
}

/// Pre-cutover audit of unresolved `.merge/*` and `pending` marker paths under a workspace root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PendingMergeAudit {
    /// Any regular files or symlinks inside `.merge/**`.
    pub pending_merge_files: Vec<PathBuf>,
    /// Any `*.merge` index markers or `*.pending` / `.pending/**` marker files.
    pub pending_markers: Vec<PathBuf>,
}

impl PendingMergeAudit {
    /// Whether zero pending merges or markers exist.
    pub fn is_clean(&self) -> bool {
        self.pending_merge_files.is_empty() && self.pending_markers.is_empty()
    }
}

/// Scan `workspace_root` for unresolved `.merge/**`, `.index/**/*.merge`, and `.pending/**` markers.
pub fn verify_zero_pending_merges(
    workspace_root: &Path,
) -> Result<PendingMergeAudit, D6StoreError> {
    let mut audit = PendingMergeAudit::default();
    match fs::symlink_metadata(workspace_root) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(D6StoreError::InvalidInput(format!(
                    "workspace_root {} must be a real non-symlink directory",
                    workspace_root.display()
                )));
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(audit),
        Err(err) => return Err(err.into()),
    }

    collect_tree_entries(
        &workspace_root.join(".merge"),
        &mut audit.pending_merge_files,
    )?;
    collect_matching_markers(
        &workspace_root.join(".index"),
        ".merge",
        &mut audit.pending_markers,
    )?;
    collect_tree_entries(&workspace_root.join(".pending"), &mut audit.pending_markers)?;
    collect_matching_markers(
        &workspace_root.join(".stale"),
        ".pending",
        &mut audit.pending_markers,
    )?;

    audit.pending_merge_files.sort();
    audit.pending_markers.sort();
    Ok(audit)
}

/// Fail closed unless [`verify_zero_pending_merges`] reports zero leftover merge markers.
pub fn assert_zero_pending_merges(workspace_root: &Path) -> Result<(), D6StoreError> {
    let audit = verify_zero_pending_merges(workspace_root)?;
    if audit.is_clean() {
        return Ok(());
    }
    let samples: Vec<String> = audit
        .pending_merge_files
        .iter()
        .chain(audit.pending_markers.iter())
        .take(8)
        .map(|p| p.display().to_string())
        .collect();
    Err(D6StoreError::InvalidInput(format!(
        "pre-cutover pending merge / marker check failed ({} .merge, {} marker): {}",
        audit.pending_merge_files.len(),
        audit.pending_markers.len(),
        samples.join(", ")
    )))
}

fn collect_tree_entries(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), D6StoreError> {
    let meta = match fs::symlink_metadata(dir) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        out.push(dir.to_path_buf());
        return Ok(());
    }
    for entry_res in fs::read_dir(dir)? {
        let entry = entry_res?;
        let path = entry.path();
        let entry_meta = fs::symlink_metadata(&path)?;
        if entry_meta.file_type().is_symlink() || !entry_meta.is_dir() {
            out.push(path);
        } else {
            collect_tree_entries(&path, out)?;
        }
    }
    Ok(())
}

fn collect_matching_markers(
    dir: &Path,
    suffix: &str,
    out: &mut Vec<PathBuf>,
) -> Result<(), D6StoreError> {
    let meta = match fs::symlink_metadata(dir) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Ok(());
    }
    for entry_res in fs::read_dir(dir)? {
        let entry = entry_res?;
        let path = entry.path();
        let entry_meta = fs::symlink_metadata(&path)?;
        if entry_meta.file_type().is_symlink() || !entry_meta.is_dir() {
            if path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(suffix))
            {
                out.push(path);
            }
        } else {
            collect_matching_markers(&path, suffix, out)?;
        }
    }
    Ok(())
}

/// Parity proof entry for a single ref across `RefLedger`, CAS, and bare Git repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CutoverRefParity {
    /// Full Git reference (`refs/heads/main`, `refs/heads/feat/x`, `refs/tags/v1`).
    pub git_ref: String,
    /// Name stored in `spawn_refs`.
    pub ledger_name: String,
    /// Ref classification in `spawn_refs`.
    pub kind: D6RefKind,
    /// Target `spawn-snapshot-v1` digest.
    pub snapshot: Digest,
    /// Verified 40-hex commit OID matching `git rev-parse` and CAS projection.
    pub git_oid: GitOid,
}

/// Complete ref-by-ref cutover parity report returned by [`verify_cutover_parity`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CutoverParityReport {
    /// Audited project name.
    pub project: String,
    /// Verified map keyed by full Git ref name.
    pub refs: BTreeMap<String, CutoverRefParity>,
}

/// Verify bijective ref parity (`RefLedger git_oid == CAS projection == git rev-parse`)
/// for every non-WIP ref in `project` against `bare_repo`.
pub fn verify_cutover_parity<L: RefLedger + ?Sized>(
    store: &CasMerkleStore,
    ledger: &L,
    project: &str,
    bare_repo: &Path,
) -> Result<CutoverParityReport, D6StoreError> {
    let bare_refs = list_bare_commit_refs(bare_repo)?;
    let mut bare_map: BTreeMap<String, (D6RefKind, String, GitOid)> = BTreeMap::new();
    for item in bare_refs {
        bare_map.insert(item.git_ref, (item.kind, item.ledger_name, item.commit_oid));
    }

    let ledger_refs = ledger.list_refs(Some(project))?;
    let mut oid_cache = HashMap::new();
    let mut verified = BTreeMap::new();

    for row in ledger_refs {
        if row.kind == D6RefKind::Wip {
            continue;
        }
        let git_ref = ledger_ref_to_git_ref(row.kind, &row.name);
        let Some((bare_kind, _, bare_oid)) = bare_map.remove(&git_ref) else {
            return Err(D6StoreError::InvalidInput(format!(
                "ref {git_ref} exists in RefLedger for {project} but is missing in {}",
                bare_repo.display()
            )));
        };
        if bare_kind != row.kind {
            return Err(D6StoreError::InvalidInput(format!(
                "ref {git_ref} kind mismatch: ledger {:?} vs bare {:?}",
                row.kind, bare_kind
            )));
        }

        let rev_oid = rev_parse_commit(bare_repo, &git_ref)?;
        if rev_oid != bare_oid {
            return Err(D6StoreError::OidMismatch {
                what: "ref rev-parse",
                expected: bare_oid.to_hex(),
                actual: rev_oid.to_hex(),
            });
        }

        let ledger_snap = ledger.get_snapshot(&row.snapshot)?.ok_or_else(|| {
            D6StoreError::Ledger(format!(
                "ref {git_ref} targets missing ledger snapshot {}",
                row.snapshot
            ))
        })?;
        if ledger_snap.git_oid != rev_oid {
            return Err(D6StoreError::OidMismatch {
                what: "ledger ref",
                expected: rev_oid.to_hex(),
                actual: ledger_snap.git_oid.to_hex(),
            });
        }

        let projected = resolve_cas_snapshot_oid(store, &row.snapshot, &mut oid_cache)?;
        if projected != rev_oid {
            return Err(D6StoreError::OidMismatch {
                what: "cas ref projection",
                expected: rev_oid.to_hex(),
                actual: projected.to_hex(),
            });
        }

        verified.insert(
            git_ref.clone(),
            CutoverRefParity {
                git_ref,
                ledger_name: row.name,
                kind: row.kind,
                snapshot: row.snapshot,
                git_oid: rev_oid,
            },
        );
    }

    if let Some((extra_ref, _)) = bare_map.into_iter().next() {
        return Err(D6StoreError::InvalidInput(format!(
            "ref {extra_ref} exists in {} but is missing in RefLedger for {project}",
            bare_repo.display()
        )));
    }

    Ok(CutoverParityReport {
        project: project.to_string(),
        refs: verified,
    })
}

/// Summary of Step 1 / Step 3 history backfill into CAS + `spawn_snapshots`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillReport {
    /// Total commits reachable from `refs/heads/*` and `refs/tags/*`.
    pub commits_scanned: usize,
    /// Commits newly recorded into `RefLedger::record_snapshot`.
    pub snapshots_newly_imported: usize,
    /// Commits already recorded in `RefLedger` (idempotently skipped/re-verified).
    pub snapshots_already_present: usize,
}

/// Output from Step 2 dispatch pause, `.merge/*` zeroing check, and `.gitattributes` audit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreCutoverCleanReport {
    /// Verified empty pending-merge / marker state under `workspace_root`.
    pub pending_audit: PendingMergeAudit,
    /// Pre-cutover `.gitattributes` export/EOL audit on the default branch tree.
    pub export_attributes: Option<ExportAttributesAudit>,
}

/// Summary returned by [`ProjectCutover::rollback_to_git`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackReport {
    /// Number of non-WIP refs exported back to the Git bare repository.
    pub exported_refs: usize,
    /// Ref parity report when non-empty refs were exported back.
    pub parity: Option<CutoverParityReport>,
}

/// Stateful coordinator implementing `#113` Comment #5 §三.3's 7-step lossless project cutover
/// and deterministic rollback back to Git.
#[derive(Debug, Clone)]
pub struct ProjectCutover {
    project: String,
    bare_repo: PathBuf,
    workspace_root: PathBuf,
    phase: CutoverPhase,
    storage_mode: ProjectStorageMode,
    dispatch_paused: bool,
    default_tree: Option<Digest>,
    export_attributes: Option<ExportAttributesAudit>,
    parity: Option<CutoverParityReport>,
    events: Vec<CutoverEvent>,
}

impl ProjectCutover {
    /// Construct a cutover coordinator in [`CutoverPhase::NotStarted`] on [`ProjectStorageMode::Git`].
    pub fn new(
        project: impl Into<String>,
        bare_repo: impl Into<PathBuf>,
        workspace_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            project: project.into(),
            bare_repo: bare_repo.into(),
            workspace_root: workspace_root.into(),
            phase: CutoverPhase::NotStarted,
            storage_mode: ProjectStorageMode::Git,
            dispatch_paused: false,
            default_tree: None,
            export_attributes: None,
            parity: None,
            events: Vec::new(),
        }
    }

    /// Resume a cutover coordinator from persisted cross-process state (e.g. CLI rollback).
    pub fn from_state(
        project: impl Into<String>,
        bare_repo: impl Into<PathBuf>,
        workspace_root: impl Into<PathBuf>,
        phase: CutoverPhase,
        storage_mode: ProjectStorageMode,
        dispatch_paused: bool,
    ) -> Self {
        Self {
            project: project.into(),
            bare_repo: bare_repo.into(),
            workspace_root: workspace_root.into(),
            phase,
            storage_mode,
            dispatch_paused,
            default_tree: None,
            export_attributes: None,
            parity: None,
            events: Vec::new(),
        }
    }

    /// Project identifier.
    pub fn project(&self) -> &str {
        &self.project
    }

    /// Current state-machine phase.
    pub const fn phase(&self) -> CutoverPhase {
        self.phase
    }

    /// Active project storage mode (`Git` or `D6Cas`).
    pub const fn storage_mode(&self) -> ProjectStorageMode {
        self.storage_mode
    }

    /// Whether project task dispatch is currently paused for cutover.
    pub const fn dispatch_paused(&self) -> bool {
        self.dispatch_paused
    }

    /// Ordered transition events recorded so far.
    pub fn events(&self) -> &[CutoverEvent] {
        &self.events
    }

    /// Latest cutover ref parity proof (populated after Step 5).
    pub fn parity_report(&self) -> Option<&CutoverParityReport> {
        self.parity.as_ref()
    }

    /// Pre-cutover `.gitattributes` audit captured in Step 2 (if default tree existed).
    pub fn export_attributes_audit(&self) -> Option<&ExportAttributesAudit> {
        self.export_attributes.as_ref()
    }

    fn require_phase(&self, expected: CutoverPhase, action: &str) -> Result<(), D6StoreError> {
        if self.phase != expected {
            return Err(D6StoreError::InvalidInput(format!(
                "{action} requires phase {expected:?}, current phase is {:?}",
                self.phase
            )));
        }
        Ok(())
    }

    fn push_event(&mut self, detail: impl Into<String>) {
        self.events.push(CutoverEvent {
            phase: self.phase,
            storage_mode: self.storage_mode,
            detail: detail.into(),
        });
    }

    /// **Step 1**: Backfill full commit/tree/blob history from `bare_repo` into `store`,
    /// verify 100% bijective tree/commit OIDs, sync CAS, and idempotently populate `spawn_snapshots`
    /// without advancing `spawn_refs` yet.
    pub fn step1_backfill_history<L: RefLedger + ?Sized>(
        &mut self,
        store: &CasMerkleStore,
        ledger: &L,
    ) -> Result<BackfillReport, D6StoreError> {
        self.require_phase(CutoverPhase::NotStarted, "step1_backfill_history")?;
        let (report, default_tree) =
            backfill_snapshots_only(store, ledger, &self.project, &self.bare_repo)?;
        self.default_tree = default_tree;
        self.phase = CutoverPhase::BackfillImported;
        self.push_event(format!(
            "backfilled {} commits ({} newly recorded)",
            report.commits_scanned, report.snapshots_newly_imported
        ));
        Ok(report)
    }

    /// **Step 2**: Pause project dispatch, fail closed if any `.merge/*` or `pending` markers exist
    /// under `workspace_root`, and run `.gitattributes` export/EOL audit on the default branch tree.
    pub fn step2_pause_dispatch_and_verify_clean(
        &mut self,
        store: &CasMerkleStore,
    ) -> Result<PreCutoverCleanReport, D6StoreError> {
        self.require_phase(
            CutoverPhase::BackfillImported,
            "step2_pause_dispatch_and_verify_clean",
        )?;
        self.dispatch_paused = true;
        let pending_audit = verify_zero_pending_merges(&self.workspace_root)?;
        if !pending_audit.is_clean() {
            assert_zero_pending_merges(&self.workspace_root)?;
        }
        let export_attributes = match &self.default_tree {
            Some(tree) => Some(audit_export_attributes(store, tree)?),
            None => None,
        };
        self.export_attributes.clone_from(&export_attributes);
        self.phase = CutoverPhase::DispatchPausedAndDrained;
        self.push_event("paused dispatch and verified zero pending merges");
        Ok(PreCutoverCleanReport {
            pending_audit,
            export_attributes,
        })
    }

    /// **Step 3**: Import any tail commits landed in `bare_repo` between Step 1 and Step 2 pause.
    pub fn step3_import_tail_commits<L: RefLedger + ?Sized>(
        &mut self,
        store: &CasMerkleStore,
        ledger: &L,
    ) -> Result<BackfillReport, D6StoreError> {
        self.require_phase(
            CutoverPhase::DispatchPausedAndDrained,
            "step3_import_tail_commits",
        )?;
        let (report, default_tree) =
            backfill_snapshots_only(store, ledger, &self.project, &self.bare_repo)?;
        if let Some(tree) = default_tree {
            self.export_attributes = Some(audit_export_attributes(store, &tree)?);
            self.default_tree = Some(tree);
        }
        self.phase = CutoverPhase::TailImported;
        self.push_event(format!(
            "tail scan {} commits ({} newly recorded)",
            report.commits_scanned, report.snapshots_newly_imported
        ));
        Ok(report)
    }

    /// **Step 4**: Atomically commit all frozen `spawn_refs` (`main`, branches, tags), append
    /// `refs.journal`, and flip the project storage flag to [`ProjectStorageMode::D6Cas`].
    pub fn step4_commit_refs_and_flip_mode<L: RefLedger + ?Sized>(
        &mut self,
        store: &CasMerkleStore,
        ledger: &L,
        adapter: &GitBorderAdapter,
        reason: &str,
    ) -> Result<ImportReport, D6StoreError> {
        self.require_phase(
            CutoverPhase::TailImported,
            "step4_commit_refs_and_flip_mode",
        )?;
        let bridge = DynLedgerBridge(ledger);
        let report =
            adapter.import_bare_repo(store, &bridge, &self.project, &self.bare_repo, reason)?;
        self.storage_mode = ProjectStorageMode::D6Cas;
        self.phase = CutoverPhase::RefsCommitted;
        self.push_event(format!(
            "committed {} refs to RefLedger and flipped mode to D6Cas",
            report.refs.len()
        ));
        Ok(report)
    }

    /// **Step 5**: Verify per-ref `RefLedger git_oid == CAS projection == git rev-parse`.
    pub fn step5_verify_parity<L: RefLedger + ?Sized>(
        &mut self,
        store: &CasMerkleStore,
        ledger: &L,
    ) -> Result<CutoverParityReport, D6StoreError> {
        self.require_phase(CutoverPhase::RefsCommitted, "step5_verify_parity")?;
        let parity = verify_cutover_parity(store, ledger, &self.project, &self.bare_repo)?;
        self.parity = Some(parity.clone());
        self.phase = CutoverPhase::ParityVerified;
        self.push_event(format!(
            "verified ref parity for {} refs",
            parity.refs.len()
        ));
        Ok(parity)
    }

    /// **Step 6**: Configure `bare_repo` read-only (`pre-receive` hook + deny configs) so it
    /// serves strictly as the [`GitBorderAdapter`] external mirror.
    pub fn step6_configure_readonly_border(
        &mut self,
        adapter: &GitBorderAdapter,
    ) -> Result<(), D6StoreError> {
        self.require_phase(
            CutoverPhase::ParityVerified,
            "step6_configure_readonly_border",
        )?;
        adapter.configure_readonly_receive(&self.bare_repo)?;
        self.phase = CutoverPhase::BorderReadOnly;
        self.push_event("configured read-only Git border mirror");
        Ok(())
    }

    /// **Step 7**: Resume project dispatch on [`ProjectStorageMode::D6Cas`].
    pub fn step7_resume_dispatch(&mut self) -> Result<(), D6StoreError> {
        self.require_phase(CutoverPhase::BorderReadOnly, "step7_resume_dispatch")?;
        self.dispatch_paused = false;
        self.phase = CutoverPhase::Resumed;
        self.push_event("resumed dispatch on D6Cas");
        Ok(())
    }

    /// Losslessly roll back to [`ProjectStorageMode::Git`]: export all live project refs back into
    /// `bare_repo`, remove the read-only `pre-receive` guard & deny configs, verify ref parity,
    /// flip storage mode to `Git`, and unpause dispatch.
    pub fn rollback_to_git<L: RefLedger + ?Sized>(
        &mut self,
        store: &CasMerkleStore,
        ledger: &L,
        adapter: &GitBorderAdapter,
    ) -> Result<RollbackReport, D6StoreError> {
        if matches!(
            self.phase,
            CutoverPhase::NotStarted | CutoverPhase::RolledBack
        ) {
            return Err(D6StoreError::InvalidInput(format!(
                "cannot roll back from phase {:?}",
                self.phase
            )));
        }

        let live_non_wip: Vec<_> = ledger
            .list_refs(Some(&self.project))?
            .into_iter()
            .filter(|r| r.kind != D6RefKind::Wip)
            .collect();

        let (exported_refs, parity) = if live_non_wip.is_empty() {
            (0, None)
        } else {
            let bridge = DynLedgerBridge(ledger);
            let export = adapter.export_project(store, &bridge, &self.project, &self.bare_repo)?;
            let count = export.refs.len();
            restore_writable_bare_repo(&self.bare_repo)?;
            let check = verify_cutover_parity(store, ledger, &self.project, &self.bare_repo)?;
            (count, Some(check))
        };
        if live_non_wip.is_empty() {
            restore_writable_bare_repo(&self.bare_repo)?;
        }

        self.storage_mode = ProjectStorageMode::Git;
        self.dispatch_paused = false;
        self.parity.clone_from(&parity);
        self.phase = CutoverPhase::RolledBack;
        self.push_event(format!("rolled back {exported_refs} refs to Git"));
        Ok(RollbackReport {
            exported_refs,
            parity,
        })
    }
}

struct DynLedgerBridge<'a, L: RefLedger + ?Sized>(&'a L);

impl<L: RefLedger + ?Sized> RefLedger for DynLedgerBridge<'_, L> {
    fn record_snapshot(
        &self,
        id: &Digest,
        git_oid: &GitOid,
        tree: &Digest,
        parents: &[Digest],
        project: &str,
        created_at_ms: i64,
    ) -> Result<Digest, D6StoreError> {
        self.0
            .record_snapshot(id, git_oid, tree, parents, project, created_at_ms)
    }

    fn get_snapshot(
        &self,
        id: &Digest,
    ) -> Result<Option<crate::cas_merkle::LedgerSnapshot>, D6StoreError> {
        self.0.get_snapshot(id)
    }

    fn get_snapshot_by_oid(
        &self,
        git_oid: &GitOid,
    ) -> Result<Option<crate::cas_merkle::LedgerSnapshot>, D6StoreError> {
        self.0.get_snapshot_by_oid(git_oid)
    }

    fn cas_ref(
        &self,
        project: &str,
        name: &str,
        kind: D6RefKind,
        expected: Option<&Digest>,
        new_snapshot: &Digest,
        reason: &str,
    ) -> Result<u64, D6StoreError> {
        self.0
            .cas_ref(project, name, kind, expected, new_snapshot, reason)
    }

    fn land_main_ref(
        &self,
        project: &str,
        name: &str,
        expected_base: Option<&Digest>,
        new_snapshot: &Digest,
        task_id: Option<i64>,
        reason: &str,
    ) -> Result<u64, D6StoreError> {
        self.0
            .land_main_ref(project, name, expected_base, new_snapshot, task_id, reason)
    }

    fn list_refs(
        &self,
        project: Option<&str>,
    ) -> Result<Vec<crate::cas_merkle::LedgerRef>, D6StoreError> {
        self.0.list_refs(project)
    }
}

fn restore_writable_bare_repo(bare_repo: &Path) -> Result<(), D6StoreError> {
    let hook_path = bare_repo.join("hooks/pre-receive");
    match fs::symlink_metadata(&hook_path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || meta.is_dir() {
                return Err(D6StoreError::InvalidInput(format!(
                    "{} must be a regular hook file during rollback",
                    hook_path.display()
                )));
            }
            fs::remove_file(&hook_path)?;
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    for key in [
        "receive.denyNonFastForwards",
        "receive.denyDeletes",
        "receive.denyCurrentBranch",
    ] {
        let out = git_bare_cmd(bare_repo)
            .args(["config", "--unset-all", key])
            .output()?;
        // Exit code 5 means key was not set, which is benign on rollback.
        if !out.status.success() && out.status.code() != Some(5) {
            ensure_git_cmd_ok("config --unset-all", &out)?;
        }
    }
    Ok(())
}

/// Inspect whether `bare_repo` currently has the D6 read-only border receive lock installed
/// (`hooks/pre-receive` regular file plus `receive.denyNonFastForwards=true`,
/// `receive.denyDeletes=true`, and `receive.denyCurrentBranch=refuse`).
pub fn is_readonly_border(bare_repo: &Path) -> Result<bool, D6StoreError> {
    let hook_path = bare_repo.join("hooks/pre-receive");
    match fs::symlink_metadata(&hook_path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_file() {
                return Ok(false);
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    }
    for (key, expected) in [
        ("receive.denyNonFastForwards", "true"),
        ("receive.denyDeletes", "true"),
        ("receive.denyCurrentBranch", "refuse"),
    ] {
        let out = git_bare_cmd(bare_repo)
            .args(["config", "--get", key])
            .output()?;
        if out.status.code() == Some(1) {
            return Ok(false);
        }
        if !out.status.success() {
            ensure_git_cmd_ok("config --get", &out)?;
        }
        if String::from_utf8_lossy(&out.stdout).trim() != expected {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Telemetry from [`reconcile_refs_journal`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Journal entries with `seq > pg_latest_ref_seq` inspected.
    pub entries_considered: usize,
    /// Journal entries applied to `ledger` during replay.
    pub entries_replayed: usize,
    /// Journal entries whose ref target already matched `ledger` (idempotent no-ops).
    pub already_applied: usize,
    /// Highest committed `seq` present in `<cas>/refs.journal` (`0` if empty).
    pub last_journal_seq: u64,
    /// Highest `RefLedger` log `seq` observed/committed at end of reconciliation.
    pub last_ledger_seq: u64,
}

/// Replay `refs.journal` entries with `seq > pg_latest_ref_seq` into `ledger` cleanly using
/// fast-forward validation (`#113` Fine Item 7.2.2). Does not re-append to `refs.journal`.
pub fn reconcile_refs_journal<L: RefLedger + ?Sized>(
    store: &CasMerkleStore,
    ledger: &L,
    pg_latest_ref_seq: u64,
    reason_prefix: &str,
) -> Result<ReconcileReport, D6StoreError> {
    let all_entries = store.cas().read_ref_journal()?;
    let last_journal_seq = all_entries.iter().map(|e| e.seq).max().unwrap_or(0);
    let journal_ts: HashMap<Digest, i64> = all_entries
        .into_iter()
        .map(|e| (e.new_snapshot, e.at_ms))
        .collect();

    let suffix = store.cas().reconcile_entries_after(pg_latest_ref_seq)?;
    if suffix.is_empty() {
        return Ok(ReconcileReport {
            entries_considered: 0,
            entries_replayed: 0,
            already_applied: 0,
            last_journal_seq,
            last_ledger_seq: pg_latest_ref_seq,
        });
    }

    let mut current_refs: HashMap<(String, String), Digest> = ledger
        .list_refs(None)?
        .into_iter()
        .map(|r| ((r.project, r.name), r.snapshot))
        .collect();

    let mut oid_cache: HashMap<Digest, GitOid> = HashMap::new();
    let mut last_ledger_seq = pg_latest_ref_seq;
    let mut entries_replayed = 0_usize;
    let mut already_applied = 0_usize;

    for entry in &suffix {
        let kind = parse_ledger_ref_kind(&entry.kind)?;
        let snap = store.cas().read_merkle_snapshot(&entry.new_snapshot)?;
        let expected_oid = ensure_snapshot_recorded_in_ledger(
            store,
            ledger,
            &entry.project,
            &entry.new_snapshot,
            Some(entry.at_ms),
            &journal_ts,
            &mut oid_cache,
        )?;
        if expected_oid != entry.git_oid {
            return Err(D6StoreError::OidMismatch {
                what: "refs.journal entry",
                expected: entry.git_oid.to_hex(),
                actual: expected_oid.to_hex(),
            });
        }

        let key = (entry.project.clone(), entry.name.clone());
        let existing = current_refs.get(&key).cloned();
        if existing.as_ref() == Some(&entry.new_snapshot) {
            already_applied += 1;
            continue;
        }
        if existing != entry.old_snapshot {
            return Err(D6StoreError::Ledger(format!(
                "non-fast-forward refs.journal replay on ({}, {}) seq {}: expected old={:?}, found={:?}",
                entry.project, entry.name, entry.seq, entry.old_snapshot, existing
            )));
        }

        let reason = format!("{reason_prefix}:seq={}", entry.seq);
        let applied_seq = match kind {
            D6RefKind::Main => {
                let task_id = if entry.old_snapshot.is_some() {
                    let parsed =
                        extract_spawn_task_trailer(&snap.commit_body).ok_or_else(|| {
                            D6StoreError::InvalidInput(format!(
                                "reconciling main advance seq={} missing Spawn-Task trailer",
                                entry.seq
                            ))
                        })?;
                    Some(parsed)
                } else {
                    None
                };
                ledger.land_main_ref(
                    &entry.project,
                    &entry.name,
                    entry.old_snapshot.as_ref(),
                    &entry.new_snapshot,
                    task_id,
                    &reason,
                )?
            }
            D6RefKind::Branch | D6RefKind::Wip | D6RefKind::Tag => ledger.cas_ref(
                &entry.project,
                &entry.name,
                kind,
                entry.old_snapshot.as_ref(),
                &entry.new_snapshot,
                &reason,
            )?,
        };
        current_refs.insert(key, entry.new_snapshot.clone());
        last_ledger_seq = last_ledger_seq.max(applied_seq);
        entries_replayed += 1;
    }

    Ok(ReconcileReport {
        entries_considered: suffix.len(),
        entries_replayed,
        already_applied,
        last_journal_seq,
        last_ledger_seq,
    })
}

fn ensure_snapshot_recorded_in_ledger<L: RefLedger + ?Sized>(
    store: &CasMerkleStore,
    ledger: &L,
    project: &str,
    snapshot_id: &Digest,
    override_at_ms: Option<i64>,
    journal_ts: &HashMap<Digest, i64>,
    oid_cache: &mut HashMap<Digest, GitOid>,
) -> Result<GitOid, D6StoreError> {
    if let Some(row) = ledger.get_snapshot(snapshot_id)? {
        oid_cache.insert(snapshot_id.clone(), row.git_oid);
        return Ok(row.git_oid);
    }
    let snap = store.cas().read_merkle_snapshot(snapshot_id)?;
    let mut parent_oids = Vec::with_capacity(snap.parents.len());
    for parent in &snap.parents {
        let p_oid = ensure_snapshot_recorded_in_ledger(
            store, ledger, project, parent, None, journal_ts, oid_cache,
        )?;
        parent_oids.push(p_oid);
    }
    let commit_oid = store.project_snapshot_oid(&snap, &parent_oids)?;
    let at_ms = override_at_ms
        .or_else(|| journal_ts.get(snapshot_id).copied())
        .unwrap_or_else(|| extract_committer_ms(&snap.commit_body));
    let _ = ledger.record_snapshot(
        snapshot_id,
        &commit_oid,
        &snap.tree,
        &snap.parents,
        project,
        at_ms,
    )?;
    oid_cache.insert(snapshot_id.clone(), commit_oid);
    Ok(commit_oid)
}

/// Validated GC Fence identity and log sequence alignment status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcFenceStatus {
    /// Active database identity verified against `<cas_root>/.gc-fence`.
    pub binding: GcFenceBinding,
    /// Live PostgreSQL `spawn_ref_log` sequence checked against journal.
    pub pg_latest_ref_seq: u64,
    /// Latest committed `seq` in `<cas_root>/refs.journal`.
    pub journal_latest_seq: u64,
}

/// Bind `store`'s CAS root to `binding` (`<cas_root>/.gc-fence`) idempotently.
pub fn bind_store_gc_fence(
    store: &CasMerkleStore,
    binding: &GcFenceBinding,
) -> Result<(), D6StoreError> {
    store.cas().bind_gc_fence(binding)?;
    Ok(())
}

/// Verify `<cas_root>/.gc-fence` against `live_binding` and assert `pg_latest_ref_seq >= journal_latest_seq`.
pub fn check_gc_fence(
    store: &CasMerkleStore,
    live_binding: &GcFenceBinding,
    pg_latest_ref_seq: u64,
) -> Result<GcFenceStatus, D6StoreError> {
    let journal_latest_seq = store.cas().latest_seq()?;
    store
        .cas()
        .verify_gc_fence(live_binding, pg_latest_ref_seq, journal_latest_seq)?;
    Ok(GcFenceStatus {
        binding: live_binding.clone(),
        pg_latest_ref_seq,
        journal_latest_seq,
    })
}

/// Collect live reachability roots across ALL projects in `ledger` and run one 2-round / grace-window
/// [`Store::gc_persistent`] pass protected by `.gc-fence`.
pub fn run_persistent_gc_for_ledger<L: RefLedger + ?Sized>(
    store: &CasMerkleStore,
    ledger: &L,
    live_binding: &GcFenceBinding,
    pg_latest_ref_seq: u64,
    grace: Duration,
) -> Result<PersistentGcReport, D6StoreError> {
    let all_refs = ledger.list_refs(None)?;
    let mut unique_roots = BTreeSet::new();
    for r in all_refs {
        unique_roots.insert(r.snapshot);
    }
    let roots: Vec<Digest> = unique_roots.into_iter().collect();
    let report = store
        .cas()
        .gc_persistent(&roots, live_binding, pg_latest_ref_seq, grace)?;
    Ok(report)
}

/// Description of one `> GIT_POINTER_THRESHOLD` (`100 MiB`) local-durable-only blob skipped from R2 bundles.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LocalOnlyLargeFile {
    /// Snapshot digest referencing this large blob.
    pub snapshot: Digest,
    /// Repository-relative path inside that snapshot's tree.
    pub path: TreePath,
    /// Canonical whole-file SHA-256 digest in host CAS.
    pub digest: Digest,
    /// Unchunked byte length (`> GIT_POINTER_THRESHOLD`).
    pub size: u64,
}

/// Result of an end-to-end R2 `git bundle` export -> blank-store import recovery drill
/// (`#113` Fine Items 7.2.6–7.2.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleDrillReport {
    /// Audited project name.
    pub project: String,
    /// Path to generated `.bundle` file.
    pub bundle_path: PathBuf,
    /// Bundle byte size on disk.
    pub bundle_bytes: u64,
    /// Number of reachable snapshots recomputed bit-for-bit in an isolated blank `CasMerkleStore`.
    pub snapshots_verified: usize,
    /// Recomputed `(snapshot_id, git_oid)` per exported Git ref.
    pub refs_verified: BTreeMap<String, (Digest, GitOid)>,
    /// All `> 100 MiB` (`GIT_POINTER_THRESHOLD`) local-only entries observed in source snapshots.
    pub local_only_large_files: Vec<LocalOnlyLargeFile>,
    /// Missing `> 100 MiB` local-only entries explicitly reported (not errored) during off-host restore.
    pub missing_local_only_large_files: Vec<LocalOnlyLargeFile>,
}

struct TempDirGuard(PathBuf);

impl TempDirGuard {
    fn create(tag: &str) -> Result<Self, D6StoreError> {
        let seq = CUTOVER_TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "spawn-d6-cutover-{tag}-{}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Execute an end-to-end R2 backup & disaster-recovery drill (`#113` Fine Items 7.2.6–7.2.7):
/// 1. Enumerate all reachable snapshots, refs, and `> 100 MiB` local-only files for `project`.
/// 2. Export `project` via `adapter` into a temporary bare mirror and write `bundle_path` (`git bundle create`).
/// 3. Verify and unbundle `bundle_path` into a fresh empty bare repo (`git bundle verify` + `git fsck --full --strict`).
/// 4. Import the unbundled repo into a blank temporary [`CasMerkleStore`] (tolerating missing
///    `> 100 MiB` local-only blobs via `spawn-pointer-v1` while listing them explicitly).
/// 5. Assert every `snapshot_id` and `git_oid` recomputes identically to the source store and ledger.
pub fn verify_bundle_snapshot_roundtrip<L: RefLedger + ?Sized>(
    store: &CasMerkleStore,
    ledger: &L,
    adapter: &GitBorderAdapter,
    project: &str,
    bundle_path: &Path,
) -> Result<BundleDrillReport, D6StoreError> {
    let source = collect_project_dag_inventory(store, ledger, project)?;
    if source.refs.is_empty() {
        return Err(D6StoreError::InvalidInput(format!(
            "project {project:?} has no non-WIP refs to bundle"
        )));
    }

    if let Some(parent) = bundle_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    let drill_dir = TempDirGuard::create("bundle-drill")?;
    let export_mirror = drill_dir.path().join("export_mirror.git");
    let restore_bare = drill_dir.path().join("restore_bare.git");
    let blank_cas_dir = drill_dir.path().join("blank_cas");

    let bridge = DynLedgerBridge(ledger);
    let _export_report = adapter.export_project(store, &bridge, project, &export_mirror)?;

    let create_out = git_bare_cmd(&export_mirror)
        .arg("bundle")
        .arg("create")
        .arg(bundle_path)
        .arg("--all")
        .output()?;
    ensure_git_cmd_ok("bundle create", &create_out)?;
    let bundle_bytes = fs::symlink_metadata(bundle_path)?.len();

    let init_out = git_command("git")
        .arg("init")
        .arg("--bare")
        .arg(&restore_bare)
        .output()?;
    ensure_git_cmd_ok("init --bare restore", &init_out)?;

    let verify_out = git_bare_cmd(&restore_bare)
        .arg("bundle")
        .arg("verify")
        .arg(bundle_path)
        .output()?;
    ensure_git_cmd_ok("bundle verify", &verify_out)?;

    let unbundle_out = git_bare_cmd(&restore_bare)
        .arg("bundle")
        .arg("unbundle")
        .arg(bundle_path)
        .output()?;
    ensure_git_cmd_ok("bundle unbundle", &unbundle_out)?;
    apply_unbundled_refs(&restore_bare, &unbundle_out.stdout)?;

    let fsck_out = git_bare_cmd(&restore_bare)
        .args(["fsck", "--full", "--strict"])
        .output()?;
    ensure_git_cmd_ok("fsck --full --strict", &fsck_out)?;

    let blank_cas = CasMerkleStore::open(&blank_cas_dir)?;
    let restored = import_bare_repo_into_blank_store(&blank_cas, &restore_bare)?;

    verify_restored_matches_source(&source, &restored)?;

    Ok(BundleDrillReport {
        project: project.to_string(),
        bundle_path: bundle_path.to_path_buf(),
        bundle_bytes,
        snapshots_verified: restored.snapshots.len(),
        refs_verified: restored.refs,
        local_only_large_files: source.local_only_large_files,
        missing_local_only_large_files: restored.missing_large_files,
    })
}

struct ProjectInventory {
    snapshots: BTreeMap<Digest, GitOid>,
    refs: BTreeMap<String, (Digest, GitOid)>,
    local_only_large_files: Vec<LocalOnlyLargeFile>,
}

fn collect_project_dag_inventory<L: RefLedger + ?Sized>(
    store: &CasMerkleStore,
    ledger: &L,
    project: &str,
) -> Result<ProjectInventory, D6StoreError> {
    let rows = ledger.list_refs(Some(project))?;
    let mut oid_cache = HashMap::new();
    let mut refs = BTreeMap::new();
    let mut queue = VecDeque::new();
    let mut snapshots = BTreeMap::new();
    let mut tree_large_cache: HashMap<Digest, Vec<(TreePath, Digest, u64)>> = HashMap::new();
    let mut large_files = BTreeSet::new();

    for row in rows {
        if row.kind == D6RefKind::Wip {
            continue;
        }
        let git_ref = ledger_ref_to_git_ref(row.kind, &row.name);
        let oid = resolve_cas_snapshot_oid(store, &row.snapshot, &mut oid_cache)?;
        if let Some(ledger_row) = ledger.get_snapshot(&row.snapshot)?
            && ledger_row.git_oid != oid
        {
            return Err(D6StoreError::OidMismatch {
                what: "source ledger snapshot",
                expected: oid.to_hex(),
                actual: ledger_row.git_oid.to_hex(),
            });
        }
        refs.insert(git_ref, (row.snapshot.clone(), oid));
        queue.push_back(row.snapshot);
    }

    while let Some(snap_id) = queue.pop_front() {
        if snapshots.contains_key(&snap_id) {
            continue;
        }
        let oid = resolve_cas_snapshot_oid(store, &snap_id, &mut oid_cache)?;
        let snap = store.cas().read_merkle_snapshot(&snap_id)?;
        let large_in_tree = if let Some(cached) = tree_large_cache.get(&snap.tree) {
            cached.clone()
        } else {
            let mut found = Vec::new();
            for (path_bytes, entry) in store.flatten_tree(&snap.tree)? {
                if is_pointer_sized(&entry)
                    && let Some(digest) = entry.digest()
                {
                    found.push((TreePath::from_bytes(path_bytes), digest, entry.size));
                }
            }
            tree_large_cache.insert(snap.tree.clone(), found.clone());
            found
        };
        for (path, digest, size) in large_in_tree {
            large_files.insert(LocalOnlyLargeFile {
                snapshot: snap_id.clone(),
                path,
                digest,
                size,
            });
        }
        for p in &snap.parents {
            if !snapshots.contains_key(p) {
                queue.push_back(p.clone());
            }
        }
        snapshots.insert(snap_id, oid);
    }

    Ok(ProjectInventory {
        snapshots,
        refs,
        local_only_large_files: large_files.into_iter().collect(),
    })
}

fn apply_unbundled_refs(restore_bare: &Path, unbundle_stdout: &[u8]) -> Result<(), D6StoreError> {
    let text = String::from_utf8_lossy(unbundle_stdout);
    let mut script = String::new();
    let mut main_present = false;
    let mut first_branch = None::<String>;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some((oid_hex, ref_name)) = trimmed.split_once(' ') else {
            continue;
        };
        if !ref_name.starts_with("refs/") {
            continue;
        }
        let oid = GitOid::parse_hex(oid_hex)?;
        let _ = writeln!(script, "update {ref_name} {oid}");
        if ref_name == "refs/heads/main" {
            main_present = true;
        } else if first_branch.is_none() && ref_name.starts_with("refs/heads/") {
            first_branch = Some(ref_name.to_string());
        }
    }

    if !script.is_empty() {
        let out = run_cmd_with_stdin(
            git_bare_cmd(restore_bare).args(["update-ref", "--stdin"]),
            script.as_bytes(),
        )?;
        ensure_git_cmd_ok("update-ref --stdin unbundle", &out)?;
    }
    let head_target = if main_present {
        Some("refs/heads/main")
    } else {
        first_branch.as_deref()
    };
    if let Some(target) = head_target {
        let out = git_bare_cmd(restore_bare)
            .args(["symbolic-ref", "HEAD", target])
            .output()?;
        ensure_git_cmd_ok("symbolic-ref HEAD", &out)?;
    }
    Ok(())
}

struct BlankRestoreResult {
    snapshots: BTreeMap<Digest, GitOid>,
    refs: BTreeMap<String, (Digest, GitOid)>,
    missing_large_files: Vec<LocalOnlyLargeFile>,
}

type DrillLargeEntry = (TreePath, Digest, u64);
type DrillBlobCache = HashMap<(GitOid, bool), (Digest, u64, bool)>;
type DrillTreeCache = HashMap<GitOid, (Digest, Vec<DrillLargeEntry>)>;

fn import_bare_repo_into_blank_store(
    store: &CasMerkleStore,
    bare_repo: &Path,
) -> Result<BlankRestoreResult, D6StoreError> {
    let raw_refs = list_bare_commit_refs(bare_repo)?;
    let commit_order = topo_rev_list(bare_repo, &raw_refs)?;
    let mut blob_cache: DrillBlobCache = HashMap::new();
    let mut tree_cache: DrillTreeCache = HashMap::new();
    let mut commit_map: HashMap<GitOid, (Digest, GitOid)> = HashMap::new();
    let mut snapshots = BTreeMap::new();
    let mut missing_large = BTreeSet::new();

    for commit_oid in &commit_order {
        let parsed = read_git_commit(bare_repo, *commit_oid)?;
        let (tree_digest, tree_missing_large) = import_tree_tolerating_missing_large(
            store,
            bare_repo,
            parsed.tree_oid,
            &mut blob_cache,
            &mut tree_cache,
        )?;
        let projected_tree = store.project_tree_oid(&tree_digest)?;
        if projected_tree != parsed.tree_oid {
            return Err(D6StoreError::OidMismatch {
                what: "restored bundle tree",
                expected: parsed.tree_oid.to_hex(),
                actual: projected_tree.to_hex(),
            });
        }

        let mut parent_digests = Vec::with_capacity(parsed.parent_oids.len());
        let mut parent_oids = Vec::with_capacity(parsed.parent_oids.len());
        for p_oid in &parsed.parent_oids {
            let (p_digest, _) = commit_map.get(p_oid).ok_or_else(|| {
                D6StoreError::InvalidInput(format!("missing restored parent commit {p_oid}"))
            })?;
            parent_digests.push(p_digest.clone());
            parent_oids.push(*p_oid);
        }

        let snap = Snapshot {
            tree: tree_digest,
            parents: parent_digests,
            commit_body: parsed.commit_body,
        };
        let snap_id = store.cas().put_merkle_snapshot(&snap)?;
        let projected_commit = store.project_snapshot_oid(&snap, &parent_oids)?;
        if projected_commit != *commit_oid {
            return Err(D6StoreError::OidMismatch {
                what: "restored bundle commit",
                expected: commit_oid.to_hex(),
                actual: projected_commit.to_hex(),
            });
        }
        for (path, digest, size) in tree_missing_large {
            missing_large.insert(LocalOnlyLargeFile {
                snapshot: snap_id.clone(),
                path,
                digest,
                size,
            });
        }
        commit_map.insert(*commit_oid, (snap_id.clone(), projected_commit));
        snapshots.insert(snap_id, projected_commit);
    }

    store.cas().sync_store()?;

    let mut refs = BTreeMap::new();
    for raw in raw_refs {
        let (snap_id, oid) = &commit_map[&raw.commit_oid];
        refs.insert(raw.git_ref, (snap_id.clone(), *oid));
    }

    Ok(BlankRestoreResult {
        snapshots,
        refs,
        missing_large_files: missing_large.into_iter().collect(),
    })
}

fn import_tree_tolerating_missing_large(
    store: &CasMerkleStore,
    repo: &Path,
    tree_oid: GitOid,
    blob_cache: &mut DrillBlobCache,
    tree_cache: &mut DrillTreeCache,
) -> Result<(Digest, Vec<DrillLargeEntry>), D6StoreError> {
    if let Some(cached) = tree_cache.get(&tree_oid) {
        return Ok(cached.clone());
    }
    let out = git_bare_cmd(repo)
        .args(["ls-tree", "-z", &tree_oid.to_hex()])
        .output()?;
    ensure_git_cmd_ok("ls-tree -z", &out)?;

    let mut entries = Vec::new();
    let mut missing_large = Vec::new();
    for record in nul_fields(&out.stdout) {
        let tab_pos = record.iter().position(|&b| b == b'\t').ok_or_else(|| {
            D6StoreError::InvalidInput("malformed ls-tree record missing tab".to_string())
        })?;
        let meta = std::str::from_utf8(&record[..tab_pos])
            .map_err(|_| D6StoreError::InvalidInput("non-utf8 ls-tree header".to_string()))?;
        let name = record[tab_pos + 1..].to_vec();
        let parts: Vec<&str> = meta.split(' ').collect();
        if parts.len() != 3 {
            return Err(D6StoreError::InvalidInput(format!(
                "unexpected ls-tree metadata: {meta:?}"
            )));
        }
        let (mode, obj_type, oid_hex) = (parts[0], parts[1], parts[2]);
        let obj_oid = GitOid::parse_hex(oid_hex)?;
        match (mode, obj_type) {
            ("040000", "tree") => {
                let (sub_digest, sub_missing) = import_tree_tolerating_missing_large(
                    store, repo, obj_oid, blob_cache, tree_cache,
                )?;
                for (sub_path, digest, size) in sub_missing {
                    let mut full = Vec::with_capacity(name.len() + 1 + sub_path.as_bytes().len());
                    full.extend_from_slice(&name);
                    full.push(b'/');
                    full.extend_from_slice(sub_path.as_bytes());
                    missing_large.push((TreePath::from_bytes(full), digest, size));
                }
                entries.push(TreeEntry::dir(name, &sub_digest));
            }
            ("160000", "commit") => {
                entries.push(TreeEntry::gitlink(name, obj_oid.as_bytes())?);
            }
            ("100644" | "100755" | "120000", "blob") => {
                let allow_ptr = mode != "120000";
                let key = (obj_oid, allow_ptr);
                let (digest, size, missing_local) = if let Some(hit) = blob_cache.get(&key) {
                    hit.clone()
                } else {
                    let imported = import_blob_for_drill(store.cas(), repo, obj_oid, allow_ptr)?;
                    blob_cache.insert(key, imported.clone());
                    imported
                };
                if missing_local {
                    missing_large.push((TreePath::from_bytes(name.clone()), digest.clone(), size));
                }
                let entry = match mode {
                    "100644" => TreeEntry::file(name, &digest, size),
                    "100755" => TreeEntry::executable(name, &digest, size),
                    "120000" => TreeEntry::symlink(name, &digest, size),
                    _ => unreachable!(),
                };
                entries.push(entry);
            }
            _ => {
                return Err(D6StoreError::InvalidInput(format!(
                    "unsupported ls-tree mode={mode} type={obj_type}"
                )));
            }
        }
    }

    let tree = Tree::from_entries(entries)?;
    let digest = store.put_tree_cached(&tree)?;
    let value = (digest.clone(), missing_large.clone());
    tree_cache.insert(tree_oid, value);
    Ok((digest, missing_large))
}

fn import_blob_for_drill(
    cas: &Store,
    repo: &Path,
    blob_oid: GitOid,
    allow_pointer_resolution: bool,
) -> Result<(Digest, u64, bool), D6StoreError> {
    let hex = blob_oid.to_hex();
    let size_out = git_bare_cmd(repo).args(["cat-file", "-s", &hex]).output()?;
    ensure_git_cmd_ok("cat-file -s", &size_out)?;
    let git_size: u64 = String::from_utf8_lossy(&size_out.stdout)
        .trim()
        .parse()
        .map_err(|_| D6StoreError::InvalidInput("invalid git blob size".to_string()))?;

    let mut child = git_bare_cmd(repo)
        .args(["cat-file", "blob", &hex])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| D6StoreError::InvalidInput("missing cat-file stdout".to_string()))?;

    if allow_pointer_resolution && git_size <= 256 {
        let mut small = Vec::new();
        stdout.read_to_end(&mut small)?;
        let out = child.wait_with_output()?;
        ensure_git_cmd_ok("cat-file blob", &out)?;
        if let Some((ptr_digest, ptr_size)) = parse_pointer_bytes(&small)
            && ptr_size > GIT_POINTER_THRESHOLD
        {
            if cas.has_blob(&ptr_digest)
                && cas.stream_blob(&ptr_digest, &mut io::sink())? == ptr_size
            {
                cas.touch(NS_BLOB, &ptr_digest)?;
                return Ok((ptr_digest, ptr_size, false));
            }
            return Ok((ptr_digest, ptr_size, true));
        }
        let stored = cas.put_blob(NS_BLOB, &mut small.as_slice())?;
        return Ok((stored.digest, stored.size, false));
    }

    let mut reader = BufReader::new(stdout);
    let stored_res = cas.put_blob(NS_BLOB, &mut reader);
    drop(reader);
    let out = child.wait_with_output()?;
    ensure_git_cmd_ok("cat-file blob", &out)?;
    let stored = stored_res?;
    if stored.size != git_size {
        return Err(D6StoreError::InvalidInput(format!(
            "blob {hex} streamed {} bytes vs git size {git_size}",
            stored.size
        )));
    }
    Ok((stored.digest, stored.size, false))
}

fn verify_restored_matches_source(
    source: &ProjectInventory,
    restored: &BlankRestoreResult,
) -> Result<(), D6StoreError> {
    if source.snapshots != restored.snapshots {
        return Err(D6StoreError::InvalidInput(format!(
            "restored bundle snapshot inventory diverged: expected {}, got {}",
            source.snapshots.len(),
            restored.snapshots.len()
        )));
    }
    if source.refs != restored.refs {
        return Err(D6StoreError::InvalidInput(format!(
            "restored bundle refs diverged: expected {}, got {}",
            source.refs.len(),
            restored.refs.len()
        )));
    }
    if source.local_only_large_files != restored.missing_large_files {
        return Err(D6StoreError::InvalidInput(format!(
            "restored bundle local-only large file report diverged: expected {}, got {}",
            source.local_only_large_files.len(),
            restored.missing_large_files.len()
        )));
    }
    Ok(())
}

fn backfill_snapshots_only<L: RefLedger + ?Sized>(
    store: &CasMerkleStore,
    ledger: &L,
    project: &str,
    bare_repo: &Path,
) -> Result<(BackfillReport, Option<Digest>), D6StoreError> {
    let raw_refs = list_bare_commit_refs(bare_repo)?;
    if raw_refs.is_empty() {
        return Ok((
            BackfillReport {
                commits_scanned: 0,
                snapshots_newly_imported: 0,
                snapshots_already_present: 0,
            },
            None,
        ));
    }

    let default_head_oid = raw_refs
        .iter()
        .find(|r| r.git_ref == "refs/heads/main")
        .or_else(|| {
            raw_refs
                .iter()
                .find(|r| r.git_ref.starts_with("refs/heads/"))
        })
        .map(|r| r.commit_oid);

    let commit_order = topo_rev_list(bare_repo, &raw_refs)?;
    let mut blob_cache: HashMap<(GitOid, bool), (Digest, u64)> = HashMap::new();
    let mut tree_cache: HashMap<GitOid, Digest> = HashMap::new();
    let mut commit_map: HashMap<GitOid, (Digest, Digest, Vec<Digest>, i64)> = HashMap::new();

    for commit_oid in &commit_order {
        let parsed = read_git_commit(bare_repo, *commit_oid)?;
        let tree_digest = import_git_tree_recursive(
            store,
            bare_repo,
            parsed.tree_oid,
            &mut blob_cache,
            &mut tree_cache,
        )?;
        let projected_tree = store.project_tree_oid(&tree_digest)?;
        if projected_tree != parsed.tree_oid {
            return Err(D6StoreError::OidMismatch {
                what: "backfill tree",
                expected: parsed.tree_oid.to_hex(),
                actual: projected_tree.to_hex(),
            });
        }

        let mut parent_digests = Vec::with_capacity(parsed.parent_oids.len());
        for p_oid in &parsed.parent_oids {
            let (p_digest, ..) = commit_map.get(p_oid).ok_or_else(|| {
                D6StoreError::InvalidInput(format!("missing backfill parent {p_oid}"))
            })?;
            parent_digests.push(p_digest.clone());
        }

        let snapshot = Snapshot {
            tree: tree_digest.clone(),
            parents: parent_digests.clone(),
            commit_body: parsed.commit_body,
        };
        let snap_id = store.cas().put_merkle_snapshot(&snapshot)?;
        let projected_commit = store.project_snapshot_oid(&snapshot, &parsed.parent_oids)?;
        if projected_commit != *commit_oid {
            return Err(D6StoreError::OidMismatch {
                what: "backfill commit",
                expected: commit_oid.to_hex(),
                actual: projected_commit.to_hex(),
            });
        }
        commit_map.insert(
            *commit_oid,
            (snap_id, tree_digest, parent_digests, parsed.committer_ms),
        );
    }

    store.cas().sync_store()?;

    let mut newly = 0_usize;
    let mut already = 0_usize;
    for commit_oid in &commit_order {
        let (snap_id, tree_digest, parents, at_ms) = &commit_map[commit_oid];
        let existed = ledger.get_snapshot(snap_id)?.is_some();
        let _ =
            ledger.record_snapshot(snap_id, commit_oid, tree_digest, parents, project, *at_ms)?;
        if existed {
            already += 1;
        } else {
            newly += 1;
        }
    }

    let default_tree = default_head_oid.and_then(|oid| commit_map.get(&oid).map(|v| v.1.clone()));
    Ok((
        BackfillReport {
            commits_scanned: commit_order.len(),
            snapshots_newly_imported: newly,
            snapshots_already_present: already,
        },
        default_tree,
    ))
}

fn resolve_cas_snapshot_oid(
    store: &CasMerkleStore,
    id: &Digest,
    cache: &mut HashMap<Digest, GitOid>,
) -> Result<GitOid, D6StoreError> {
    if let Some(&oid) = cache.get(id) {
        return Ok(oid);
    }
    let snap = store.cas().read_merkle_snapshot(id)?;
    let mut p_oids = Vec::with_capacity(snap.parents.len());
    for parent in &snap.parents {
        p_oids.push(resolve_cas_snapshot_oid(store, parent, cache)?);
    }
    let oid = store.project_snapshot_oid(&snap, &p_oids)?;
    cache.insert(id.clone(), oid);
    Ok(oid)
}

fn ledger_ref_to_git_ref(kind: D6RefKind, name: &str) -> String {
    match kind {
        D6RefKind::Main | D6RefKind::Branch => {
            if name.starts_with("refs/") {
                name.to_string()
            } else {
                format!("refs/heads/{name}")
            }
        }
        D6RefKind::Tag => {
            if name.starts_with("refs/") {
                name.to_string()
            } else if let Some(stripped) = name.strip_prefix("tags/") {
                format!("refs/tags/{stripped}")
            } else {
                format!("refs/tags/{name}")
            }
        }
        D6RefKind::Wip => format!("refs/wip/{name}"),
    }
}

fn parse_ledger_ref_kind(raw: &str) -> Result<D6RefKind, D6StoreError> {
    match raw {
        "main" => Ok(D6RefKind::Main),
        "branch" => Ok(D6RefKind::Branch),
        "wip" => Ok(D6RefKind::Wip),
        "tag" => Ok(D6RefKind::Tag),
        other => Err(D6StoreError::InvalidInput(format!(
            "unknown refs.journal ref kind {other:?}"
        ))),
    }
}

fn extract_spawn_task_trailer(commit_body: &[u8]) -> Option<i64> {
    let text = String::from_utf8_lossy(commit_body);
    let mut parsed = None;
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("Spawn-Task:")
            && let Ok(id) = rest.trim().parse::<i64>()
        {
            parsed = Some(id);
        }
    }
    parsed
}

fn git_bare_cmd(bare_dir: &Path) -> Command {
    let mut cmd = git_command("git");
    cmd.arg("--git-dir")
        .arg(bare_dir)
        .arg("-c")
        .arg("core.quotepath=false");
    cmd
}

fn ensure_git_cmd_ok(action: &str, out: &std::process::Output) -> Result<(), D6StoreError> {
    if out.status.success() {
        return Ok(());
    }
    Err(D6StoreError::GitBorder {
        command: action.to_string(),
        detail: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    })
}

fn run_cmd_with_stdin(
    cmd: &mut Command,
    stdin_bytes: &[u8],
) -> Result<std::process::Output, D6StoreError> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(stdin_bytes)?;
        stdin.flush()?;
    }
    Ok(child.wait_with_output()?)
}

fn rev_parse_commit(bare_dir: &Path, rev: &str) -> Result<GitOid, D6StoreError> {
    let arg = format!("{rev}^{{commit}}");
    let out = git_bare_cmd(bare_dir)
        .args(["rev-parse", "--verify", &arg])
        .output()?;
    ensure_git_cmd_ok("rev-parse commit", &out)?;
    GitOid::parse_hex(String::from_utf8_lossy(&out.stdout).trim()).map_err(D6StoreError::from)
}

struct BareRefItem {
    git_ref: String,
    ledger_name: String,
    kind: D6RefKind,
    commit_oid: GitOid,
}

fn list_bare_commit_refs(bare_dir: &Path) -> Result<Vec<BareRefItem>, D6StoreError> {
    let out = git_bare_cmd(bare_dir)
        .args([
            "for-each-ref",
            "--format=%(refname)%09%(objecttype)%09%(*objectname)%09%(objectname)%00",
            "refs/heads",
            "refs/tags",
        ])
        .output()?;
    ensure_git_cmd_ok("for-each-ref", &out)?;
    let mut items = Vec::new();
    for raw_rec in out.stdout.split(|&b| b == 0) {
        let rec = raw_rec.strip_prefix(b"\n").unwrap_or(raw_rec);
        if rec.is_empty() {
            continue;
        }
        let fields: Vec<&[u8]> = rec.splitn(4, |&b| b == b'\t').collect();
        if fields.len() != 4 {
            return Err(D6StoreError::InvalidInput(
                "malformed for-each-ref record".to_string(),
            ));
        }
        let git_ref = std::str::from_utf8(fields[0])
            .map_err(|_| D6StoreError::InvalidInput("non-utf8 git refname".to_string()))?
            .trim()
            .to_string();
        let obj_type = std::str::from_utf8(fields[1]).unwrap_or("").trim();
        let peeled = std::str::from_utf8(fields[2]).unwrap_or("").trim();
        let direct = std::str::from_utf8(fields[3]).unwrap_or("").trim();

        let commit_oid = if obj_type == "tag" {
            let oid = GitOid::parse_hex(peeled)?;
            let kind_out = git_bare_cmd(bare_dir)
                .args(["cat-file", "-t", peeled])
                .output()?;
            ensure_git_cmd_ok("cat-file -t", &kind_out)?;
            if String::from_utf8_lossy(&kind_out.stdout).trim() != "commit" {
                continue;
            }
            oid
        } else if obj_type == "commit" {
            GitOid::parse_hex(direct)?
        } else {
            continue;
        };

        let (ledger_name, kind) = if let Some(branch) = git_ref.strip_prefix("refs/heads/") {
            if branch == "main" {
                ("main".to_string(), D6RefKind::Main)
            } else {
                (branch.to_string(), D6RefKind::Branch)
            }
        } else if let Some(tag) = git_ref.strip_prefix("refs/tags/") {
            (format!("tags/{tag}"), D6RefKind::Tag)
        } else {
            continue;
        };

        items.push(BareRefItem {
            git_ref,
            ledger_name,
            kind,
            commit_oid,
        });
    }
    Ok(items)
}

fn topo_rev_list(repo: &Path, refs: &[BareRefItem]) -> Result<Vec<GitOid>, D6StoreError> {
    if refs.is_empty() {
        return Ok(Vec::new());
    }
    let mut tips = BTreeSet::new();
    for r in refs {
        tips.insert(r.commit_oid.to_hex());
    }
    let mut cmd = git_bare_cmd(repo);
    cmd.args(["rev-list", "--topo-order", "--reverse"]);
    for tip in &tips {
        cmd.arg(tip);
    }
    let out = cmd.output()?;
    ensure_git_cmd_ok("rev-list", &out)?;
    let mut commits = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            commits.push(GitOid::parse_hex(trimmed)?);
        }
    }
    Ok(commits)
}

struct ParsedCommitData {
    tree_oid: GitOid,
    parent_oids: Vec<GitOid>,
    commit_body: Vec<u8>,
    committer_ms: i64,
}

fn read_git_commit(repo: &Path, commit_oid: GitOid) -> Result<ParsedCommitData, D6StoreError> {
    let out = git_bare_cmd(repo)
        .args(["cat-file", "commit", &commit_oid.to_hex()])
        .output()?;
    ensure_git_cmd_ok("cat-file commit", &out)?;
    let raw = out.stdout;
    let mut pos = 0_usize;

    let tree_line = next_line(&raw, &mut pos)
        .ok_or_else(|| D6StoreError::InvalidInput("missing tree line".to_string()))?;
    let tree_hex = std::str::from_utf8(
        tree_line
            .strip_prefix(b"tree ")
            .ok_or_else(|| D6StoreError::InvalidInput("expected tree header".to_string()))?,
    )
    .map_err(|_| D6StoreError::InvalidInput("non-utf8 tree header".to_string()))?;
    let tree_oid = GitOid::parse_hex(tree_hex)?;

    let mut parent_oids = Vec::new();
    loop {
        let start = pos;
        let Some(line) = next_line(&raw, &mut pos) else {
            break;
        };
        if let Some(p_bytes) = line.strip_prefix(b"parent ") {
            let p_hex = std::str::from_utf8(p_bytes)
                .map_err(|_| D6StoreError::InvalidInput("non-utf8 parent header".to_string()))?;
            parent_oids.push(GitOid::parse_hex(p_hex)?);
        } else {
            pos = start;
            break;
        }
    }

    let commit_body = raw[pos..].to_vec();
    let committer_ms = extract_committer_ms(&commit_body);
    Ok(ParsedCommitData {
        tree_oid,
        parent_oids,
        commit_body,
        committer_ms,
    })
}

fn import_git_tree_recursive(
    store: &CasMerkleStore,
    repo: &Path,
    tree_oid: GitOid,
    blob_cache: &mut HashMap<(GitOid, bool), (Digest, u64)>,
    tree_cache: &mut HashMap<GitOid, Digest>,
) -> Result<Digest, D6StoreError> {
    if let Some(cached) = tree_cache.get(&tree_oid) {
        return Ok(cached.clone());
    }
    let out = git_bare_cmd(repo)
        .args(["ls-tree", "-z", &tree_oid.to_hex()])
        .output()?;
    ensure_git_cmd_ok("ls-tree -z", &out)?;

    let mut entries = Vec::new();
    for record in nul_fields(&out.stdout) {
        let tab_pos = record.iter().position(|&b| b == b'\t').ok_or_else(|| {
            D6StoreError::InvalidInput("malformed ls-tree record missing tab".to_string())
        })?;
        let meta = std::str::from_utf8(&record[..tab_pos])
            .map_err(|_| D6StoreError::InvalidInput("non-utf8 ls-tree header".to_string()))?;
        let name = record[tab_pos + 1..].to_vec();
        let parts: Vec<&str> = meta.split(' ').collect();
        if parts.len() != 3 {
            return Err(D6StoreError::InvalidInput(format!(
                "unexpected ls-tree metadata: {meta:?}"
            )));
        }
        let (mode, obj_type, oid_hex) = (parts[0], parts[1], parts[2]);
        let obj_oid = GitOid::parse_hex(oid_hex)?;

        match (mode, obj_type) {
            ("040000", "tree") => {
                let sub = import_git_tree_recursive(store, repo, obj_oid, blob_cache, tree_cache)?;
                entries.push(TreeEntry::dir(name, &sub));
            }
            ("160000", "commit") => {
                entries.push(TreeEntry::gitlink(name, obj_oid.as_bytes())?);
            }
            ("100644" | "100755" | "120000", "blob") => {
                let allow_ptr = mode != "120000";
                let key = (obj_oid, allow_ptr);
                let (digest, size) = if let Some(hit) = blob_cache.get(&key) {
                    hit.clone()
                } else {
                    let (d, s, missing) =
                        import_blob_for_drill(store.cas(), repo, obj_oid, allow_ptr)?;
                    if missing {
                        return Err(D6StoreError::InvalidInput(format!(
                            "backfill blob pointer {d} missing local target bytes in CAS"
                        )));
                    }
                    blob_cache.insert(key, (d.clone(), s));
                    (d, s)
                };
                let entry = match mode {
                    "100644" => TreeEntry::file(name, &digest, size),
                    "100755" => TreeEntry::executable(name, &digest, size),
                    "120000" => TreeEntry::symlink(name, &digest, size),
                    _ => unreachable!(),
                };
                entries.push(entry);
            }
            _ => {
                return Err(D6StoreError::InvalidInput(format!(
                    "unsupported ls-tree entry mode={mode} type={obj_type}"
                )));
            }
        }
    }

    let tree = Tree::from_entries(entries)?;
    let digest = store.put_tree_cached(&tree)?;
    tree_cache.insert(tree_oid, digest.clone());
    Ok(digest)
}

fn parse_pointer_bytes(bytes: &[u8]) -> Option<(Digest, u64)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let rest = text.strip_prefix("spawn-pointer-v1\n")?;
    let mut lines = rest.split('\n');
    let digest_line = lines.next()?;
    let size_line = lines.next()?;
    let trailing = lines.next()?;
    if !trailing.is_empty() || lines.next().is_some() {
        return None;
    }
    let digest = Digest::parse(digest_line).ok()?;
    let size_str = size_line.strip_prefix("size ")?;
    let size: u64 = size_str.parse().ok()?;
    if git_pointer_bytes(&digest, size) != bytes {
        return None;
    }
    Some((digest, size))
}

fn next_line<'a>(bytes: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    let rest = bytes.get(*pos..)?;
    if rest.is_empty() {
        return None;
    }
    let end = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
    let line = &rest[..end];
    *pos += if *pos + end < bytes.len() {
        end + 1
    } else {
        end
    };
    Some(line)
}

fn extract_committer_ms(commit_body: &[u8]) -> i64 {
    let text = String::from_utf8_lossy(commit_body);
    let headers = text.split("\n\n").next().unwrap_or("");
    for line in headers.lines() {
        if let Some(rest) = line.strip_prefix("committer ")
            && let Some(gt) = rest.rfind('>')
        {
            let mut parts = rest[gt + 1..].split_ascii_whitespace();
            if let Some(secs_str) = parts.next()
                && let Ok(secs) = secs_str.parse::<i64>()
            {
                return secs.saturating_mul(1000);
            }
        }
    }
    0
}

impl fmt::Display for ProjectStorageMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Git => f.write_str("git"),
            Self::D6Cas => f.write_str("d6_cas"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratch::Scratch;

    #[test]
    fn pending_merge_audit_catches_nested_merge_and_index_markers() {
        let scratch = Scratch::new("cutover_pending_unit");
        let ws = scratch.join("workspaces");
        fs::create_dir_all(&ws).expect("mkdir ws");
        assert!(verify_zero_pending_merges(&ws).expect("clean").is_clean());

        fs::create_dir_all(ws.join(".merge/feat")).expect("mkdir .merge/feat");
        fs::write(ws.join(".merge/feat/alpha"), b"pending-merge\0main").expect("write merge");
        fs::create_dir_all(ws.join(".index/nested")).expect("mkdir .index");
        fs::write(ws.join(".index/nested/branch.merge"), b"idx").expect("write idx");

        let audit = verify_zero_pending_merges(&ws).expect("dirty");
        assert!(!audit.is_clean());
        assert_eq!(audit.pending_merge_files.len(), 1);
        assert_eq!(audit.pending_markers.len(), 1);
        assert!(assert_zero_pending_merges(&ws).is_err());

        fs::remove_dir_all(ws.join(".merge")).expect("rm .merge");
        fs::remove_dir_all(ws.join(".index")).expect("rm .index");
        assert_zero_pending_merges(&ws).expect("clean after removal");
    }

    #[test]
    fn extract_spawn_task_trailer_reads_last_valid_task_line() {
        let body =
            b"author a 1 +0000\ncommitter b 2 +0000\n\nmsg\n\nSpawn-Task: 42\nSpawn-Head: abcdef\n";
        assert_eq!(extract_spawn_task_trailer(body), Some(42));
        let none = b"author a 1 +0000\ncommitter b 2 +0000\n\nplain commit\n";
        assert_eq!(extract_spawn_task_trailer(none), None);
    }

    #[test]
    fn readonly_border_inspection_and_from_state_roundtrip() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "spawn-host-ro-border-{}-{}",
            std::process::id(),
            stamp
        ));
        std::fs::create_dir_all(&root).expect("create temp root");
        let bare = root.join("bare.git");

        let init = crate::test_git_command()
            .arg("init")
            .arg("--bare")
            .arg(&bare)
            .output()
            .expect("run git init --bare");
        assert!(
            init.status.success(),
            "git init --bare failed: {}",
            String::from_utf8_lossy(&init.stderr)
        );

        assert!(
            !is_readonly_border(&bare).expect("check initial border"),
            "fresh bare repo should be writable"
        );

        let border = GitBorderAdapter::new();
        border
            .configure_readonly_receive(&bare)
            .expect("configure readonly receive");
        assert!(
            is_readonly_border(&bare).expect("check locked border"),
            "bare repo should report read-only after configure_readonly_receive"
        );

        restore_writable_bare_repo(&bare).expect("restore writable bare repo");
        assert!(
            !is_readonly_border(&bare).expect("check restored writable border"),
            "bare repo should report writable after restore_writable_bare_repo"
        );

        let initial = ProjectCutover::new("proj-cutover-roundtrip", &bare, &root);
        let restored = ProjectCutover::from_state(
            "proj-cutover-roundtrip",
            &bare,
            &root,
            initial.phase(),
            initial.storage_mode(),
            true,
        );
        assert_eq!(restored.phase(), initial.phase());
        assert_eq!(restored.storage_mode(), initial.storage_mode());
        assert!(restored.dispatch_paused());

        let _ = std::fs::remove_dir_all(&root);
    }
}
