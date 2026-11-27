//! D6 Phase 4 Step 5 (`#113` Comment #5 §三.2 & Safeguard #1):
//! Dual-track [`ShadowD6Workspace`] oracle wrapping an authoritative `W: WorkspaceStore`
//! (e.g. [`GitWorkspace`]) alongside a shadow [`D6Workspace`].
//!
//! Guarantees:
//! - Strictly `0` forbidden OS/FFI code (`forbid` lint enforced at workspace level).
//! - Strictly `0` database driver imports (operates over generic `L: RefLedger`).
//! - Every `checkpoint` / `commit` verifies byte-for-byte Git tree OID & commit OID
//!   equivalence (`Agree`) and enforces Fail-Closed [`ShadowCheckpointOutcome::TrackedSetLeak`]
//!   blocking shadow publish if any ignored file (e.g. `.env`) leaks into the D6 candidate tree.
//! - Every `merge_base` / `merge_main_clean` evaluates and records
//!   [`crate::merkle_merge::classify_shadow_merge`] outcomes in [`ShadowOracleMetrics`].

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use spawn_cas::{Digest, GIT_POINTER_THRESHOLD, GitOid, NS_BLOB, Snapshot, Store, Tree, TreeEntry};
use spawn_core::id::{BranchName, CommitId, WorkspaceId};
use spawn_core::port::{BaseMerge, Branch, Commit, Hold, SetAside, WorkspaceError, WorkspaceStore};

use crate::cas_merkle::{
    CasMerkleStore, D6StoreError, LedgerSnapshot, RefLedger, TrackedSetPolicy,
};
use crate::cow::CowWorkspace;
use crate::d6_workspace::D6Workspace;
use crate::git::GitWorkspace;
use crate::git_process::git_command;
use crate::merkle_merge::{
    MergeBaseError, MergeShadowOutcome, ShadowOracleSummary, classify_shadow_merge, merge_snapshots,
};
use crate::tree_path::{TreePath, nul_fields};
use crate::workspace::LocalWorkspaceStore;

/// Controls whether the shadow scan uses strict `.gitignore` enforcement or
/// intentionally simulates an unfiltered scan (negative test for fail-closed leak detection).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ShadowTrackedSetMode {
    /// Normal fail-closed tracked-set policy matching `.gitignore`, `info/exclude`,
    /// `BUILD_OUTPUT`, and `ENGINE_RUNTIME_FILES`.
    #[default]
    Strict,
    /// Negative-test mode bypassing `.gitignore` filtering in the shadow scanner to prove
    /// [`ShadowD6Workspace`] catches and blocks ignored leaks (`TrackedSetLeak`) fail-closed.
    SimulateUnfilteredScan,
}

/// Oracle verdict produced for each [`ShadowD6Workspace`] checkpoint / commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShadowCheckpointOutcome {
    /// Authoritative Git tree/commit OIDs agreed 1-to-1 with D6 CAS projections.
    Agree {
        /// Shared projected 40-hex root tree OID.
        tree_oid: Option<GitOid>,
        /// Authoritative Git commit OID (`None` on `Commit::Unchanged`).
        primary_commit: Option<CommitId>,
        /// Shadow D6 snapshot's projected commit OID (`None` on `Commit::Unchanged`).
        shadow_commit: Option<CommitId>,
    },
    /// Authoritative Git tree OID differed from D6's projected tree OID without an ignore leak.
    TreeOidMismatch {
        /// Native Git tree OID (`rev-parse <commit>^{tree}`).
        git_tree_oid: Option<GitOid>,
        /// Shadow D6 projected tree OID.
        d6_tree_oid: GitOid,
        /// Paths present in D6 tree but absent in Git tree.
        extra_in_d6: Vec<String>,
        /// Paths present in Git tree but absent in D6 tree.
        missing_in_d6: Vec<String>,
        /// Paths present in both with differing mode/type/content.
        differing_paths: Vec<String>,
    },
    /// Ignored file(s) (e.g. `.env`) entered D6's candidate tree while excluded by Git;
    /// shadow publishing was aborted fail-closed.
    TrackedSetLeak {
        /// Ignored path(s) caught by fail-closed leak guard / `git check-ignore`.
        leaked_paths: Vec<String>,
        /// Diagnostic detail.
        detail: String,
    },
    /// Non-parity error running the shadow path (never breaks authoritative primary unless configured).
    ShadowError {
        /// Lifecycle operation name.
        operation: String,
        /// Failure diagnostic.
        detail: String,
    },
}

/// Aggregated dual-track oracle telemetry snapshot across checkpoints and merges.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShadowOracleMetrics {
    /// Checkpoints where Git and D6 agreed cleanly.
    pub checkpoints_agreed: usize,
    /// Checkpoints with non-ignore-leak tree OID mismatches.
    pub tree_mismatches: usize,
    /// Checkpoints where an ignored file attempted to leak into D6 and was caught fail-closed.
    pub tracked_set_leaks: usize,
    /// Aggregated merge oracle summary from [`classify_shadow_merge`].
    pub merge_summary: ShadowOracleSummary,
    /// Shadow-only operational or commit-OID parity errors.
    pub shadow_errors: usize,
    /// Most recent checkpoint outcome.
    pub last_checkpoint: Option<ShadowCheckpointOutcome>,
    /// Most recent merge outcome.
    pub last_merge: Option<MergeShadowOutcome>,
    /// Ordered history of checkpoint outcomes.
    pub checkpoint_outcomes: Vec<ShadowCheckpointOutcome>,
    /// Ordered history of merge outcomes.
    pub merge_outcomes: Vec<MergeShadowOutcome>,
}

impl ShadowOracleMetrics {
    /// Whether any divergence (`tree_mismatches`, `tracked_set_leaks`, `shadow_errors`, or merge blocker) occurred.
    pub const fn has_divergences(&self) -> bool {
        self.tree_mismatches > 0
            || self.tracked_set_leaks > 0
            || self.shadow_errors > 0
            || self.merge_summary.blockers > 0
    }

    fn record_checkpoint(&mut self, outcome: ShadowCheckpointOutcome) {
        match &outcome {
            ShadowCheckpointOutcome::Agree { .. } => self.checkpoints_agreed += 1,
            ShadowCheckpointOutcome::TreeOidMismatch { .. } => self.tree_mismatches += 1,
            ShadowCheckpointOutcome::TrackedSetLeak { .. } => self.tracked_set_leaks += 1,
            ShadowCheckpointOutcome::ShadowError { .. } => self.shadow_errors += 1,
        }
        self.last_checkpoint = Some(outcome.clone());
        self.checkpoint_outcomes.push(outcome);
    }

    fn record_merge(&mut self, outcome: MergeShadowOutcome) {
        self.merge_summary.record(&outcome);
        self.last_merge = Some(outcome.clone());
        self.merge_outcomes.push(outcome);
    }
}

/// Uniform idle-guarded set-aside extension over [`WorkspaceStore`] backends so
/// [`ShadowD6Workspace::set_aside_when`] avoids re-entering a branch lock held by `primary`.
pub trait SetAsideWhen: WorkspaceStore {
    /// Call `set_aside` under the store's branch lock only while `ready()` permits it.
    fn set_aside_when_idle(
        &self,
        branch: &BranchName,
        start: Option<&str>,
        message: &str,
        ready: &dyn Fn() -> Result<(), String>,
    ) -> Result<SetAside, WorkspaceError>;
}

impl SetAsideWhen for GitWorkspace {
    fn set_aside_when_idle(
        &self,
        branch: &BranchName,
        start: Option<&str>,
        message: &str,
        ready: &dyn Fn() -> Result<(), String>,
    ) -> Result<SetAside, WorkspaceError> {
        self.set_aside_when(branch, start, message, ready)
    }
}

impl SetAsideWhen for CowWorkspace {
    fn set_aside_when_idle(
        &self,
        branch: &BranchName,
        start: Option<&str>,
        message: &str,
        ready: &dyn Fn() -> Result<(), String>,
    ) -> Result<SetAside, WorkspaceError> {
        self.set_aside_when(branch, start, message, ready)
    }
}

impl SetAsideWhen for LocalWorkspaceStore {
    fn set_aside_when_idle(
        &self,
        branch: &BranchName,
        start: Option<&str>,
        message: &str,
        ready: &dyn Fn() -> Result<(), String>,
    ) -> Result<SetAside, WorkspaceError> {
        self.set_aside_when(branch, start, message, ready)
    }
}

impl<L: RefLedger + Send + Sync> SetAsideWhen for D6Workspace<L> {
    fn set_aside_when_idle(
        &self,
        branch: &BranchName,
        start: Option<&str>,
        message: &str,
        ready: &dyn Fn() -> Result<(), String>,
    ) -> Result<SetAside, WorkspaceError> {
        self.set_aside_when(branch, start, message, ready)
    }
}

impl<W: SetAsideWhen, L: RefLedger + Send + Sync> SetAsideWhen for ShadowD6Workspace<W, L> {
    fn set_aside_when_idle(
        &self,
        branch: &BranchName,
        start: Option<&str>,
        message: &str,
        ready: &dyn Fn() -> Result<(), String>,
    ) -> Result<SetAside, WorkspaceError> {
        self.set_aside_when(branch, start, message, ready)
    }
}

/// Dual-track shadow-mode oracle running `primary` (e.g. [`GitWorkspace`]) alongside
/// a shadow [`D6Workspace`], verifying checkpoint/merge OID parity and aborting shadow writes
/// fail-closed on tracked-set leaks.
pub struct ShadowD6Workspace<W: WorkspaceStore, L: RefLedger> {
    primary: W,
    shadow: D6Workspace<L>,
    git_repo: Option<PathBuf>,
    tracked_set_mode: ShadowTrackedSetMode,
    strict_fail_closed: bool,
    metrics: Mutex<ShadowOracleMetrics>,
}

impl<W: WorkspaceStore, L: RefLedger> ShadowD6Workspace<W, L> {
    /// Wrap `primary` with shadow store `shadow`, auto-detecting `git_repo` when `primary.project()`
    /// names a bare/regular `.git` directory.
    pub fn new(primary: W, shadow: D6Workspace<L>) -> Self {
        let project_path = Path::new(primary.project());
        let git_repo = looks_like_git_dir(project_path).then(|| project_path.to_path_buf());
        Self {
            primary,
            shadow,
            git_repo,
            tracked_set_mode: ShadowTrackedSetMode::Strict,
            strict_fail_closed: false,
            metrics: Mutex::new(ShadowOracleMetrics::default()),
        }
    }

    /// Wrap `primary` and `shadow` with an explicit Git repository path `git_repo`.
    pub fn with_git_repo(primary: W, shadow: D6Workspace<L>, git_repo: impl Into<PathBuf>) -> Self {
        Self {
            primary,
            shadow,
            git_repo: Some(git_repo.into()),
            tracked_set_mode: ShadowTrackedSetMode::Strict,
            strict_fail_closed: false,
            metrics: Mutex::new(ShadowOracleMetrics::default()),
        }
    }

    /// Configure [`ShadowTrackedSetMode`] (`Strict` by default, or `SimulateUnfilteredScan`
    /// in negative fail-closed tests).
    #[must_use]
    pub const fn with_tracked_set_mode(mut self, mode: ShadowTrackedSetMode) -> Self {
        self.tracked_set_mode = mode;
        self
    }

    /// When `true`, `TrackedSetLeak` ALSO fails `checkpoint` / `commit` (`Refused { code: "TrackedSetLeak" }`)
    /// in addition to aborting shadow publishing and recording `metrics.tracked_set_leaks`.
    #[must_use]
    pub const fn with_strict_fail_closed(mut self, enabled: bool) -> Self {
        self.strict_fail_closed = enabled;
        self
    }

    /// Borrow the authoritative primary workspace store.
    pub const fn primary(&self) -> &W {
        &self.primary
    }

    /// Borrow the shadow [`D6Workspace`] store.
    pub const fn shadow(&self) -> &D6Workspace<L> {
        &self.shadow
    }

    /// Read a snapshot of accumulated oracle metrics.
    pub fn metrics(&self) -> ShadowOracleMetrics {
        self.metrics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Whether `TrackedSetLeak` strictly fails checkpoint/commit operations.
    pub const fn strict_fail_closed(&self) -> bool {
        self.strict_fail_closed
    }

    /// Configured tracked-set scan mode.
    pub const fn tracked_set_mode(&self) -> ShadowTrackedSetMode {
        self.tracked_set_mode
    }

    /// Bare/working Git repository path paired with this shadow store, if any.
    pub fn git_repo(&self) -> Option<&Path> {
        self.git_repo.as_deref()
    }
}

impl<W: WorkspaceStore, L: RefLedger> std::fmt::Debug for ShadowD6Workspace<W, L> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ShadowD6Workspace")
            .field("project", &self.primary.project())
            .field("git_repo", &self.git_repo)
            .field("tracked_set_mode", &self.tracked_set_mode)
            .field("strict_fail_closed", &self.strict_fail_closed)
            .finish_non_exhaustive()
    }
}

impl<W: WorkspaceStore, L: RefLedger + Send + Sync> ShadowD6Workspace<W, L> {
    /// Sync authoritative Git `refs/heads/main` into `shadow`'s CAS + `RefLedger` (`Main` ref),
    /// bootstrapping when `main` is not yet present or advancing via `task_id` when already live.
    pub fn sync_main_from_git(
        &self,
        task_id: Option<i64>,
    ) -> Result<Option<LedgerSnapshot>, WorkspaceError> {
        let Some(repo) = &self.git_repo else {
            return self.shadow.main_snapshot();
        };
        let Some(main_oid) = rev_parse_optional(repo, "refs/heads/main")? else {
            return Ok(None);
        };
        let imported = self.import_git_commit_graph(repo, main_oid)?;
        match self.shadow.main_snapshot()? {
            None => {
                self.shadow.land_main_snapshot(
                    None,
                    &imported.id,
                    None,
                    "d6:shadow_bootstrap_main",
                )?;
            }
            Some(existing) if existing.id == imported.id => {}
            Some(existing) => {
                self.shadow.land_main_snapshot(
                    Some(&existing.id),
                    &imported.id,
                    task_id,
                    "d6:shadow_sync_main",
                )?;
            }
        }
        Ok(Some(imported))
    }

    /// Run a dual-track checkpoint (`commit`), returning the authoritative primary [`Commit`].
    pub fn checkpoint(
        &self,
        workspace: &WorkspaceId,
        message: &str,
    ) -> Result<Commit, WorkspaceError> {
        let (commit, _) = self.checkpoint_with_outcome(workspace, message)?;
        Ok(commit)
    }

    /// Run a dual-track checkpoint (`commit`), returning both the authoritative [`Commit`] and
    /// the [`ShadowCheckpointOutcome`] verdict evaluated against the shadow store.
    pub fn checkpoint_with_outcome(
        &self,
        workspace: &WorkspaceId,
        message: &str,
    ) -> Result<(Commit, ShadowCheckpointOutcome), WorkspaceError> {
        let primary_res = self.primary.commit(workspace, message)?;
        let branch = workspace.sandbox();
        let primary_dir = Path::new(workspace.path());
        let outcome = self.evaluate_shadow_checkpoint(branch, primary_dir, &primary_res)?;
        Ok((primary_res, outcome))
    }

    /// Mirror `set_aside_when_idle` through `primary` and into `shadow`.
    pub fn set_aside_when(
        &self,
        branch: &BranchName,
        start: Option<&str>,
        message: &str,
        ready: &dyn Fn() -> Result<(), String>,
    ) -> Result<SetAside, WorkspaceError>
    where
        W: SetAsideWhen,
    {
        let res = self
            .primary
            .set_aside_when_idle(branch, start, message, ready)?;
        if let Err(err) = self.mirror_set_aside_to_shadow(branch.as_str(), &res) {
            self.record_shadow_error("set_aside_when", err.to_string());
        }
        Ok(res)
    }

    /// Discard workspace state in both `primary` (unmount) and `shadow` (`discard`).
    pub fn discard(&self, workspace: &WorkspaceId) -> Result<(), WorkspaceError> {
        self.primary.unmount(workspace)?;
        let shadow_ws = self.shadow.workspace_id_for_branch(workspace.sandbox())?;
        self.shadow.discard(&shadow_ws)
    }
}

impl<L: RefLedger + Send + Sync> ShadowD6Workspace<GitWorkspace, L> {
    /// Commit `workspace` through [`GitWorkspace::commit_or_take_back`] (taking a gone branch
    /// back from its source under branch lock when needed), then evaluate shadow parity.
    pub fn commit_or_take_back(
        &self,
        workspace: &WorkspaceId,
        message: &str,
        take_back: &dyn Fn() -> Result<(), WorkspaceError>,
    ) -> Result<Commit, WorkspaceError> {
        let primary_res = self
            .primary
            .commit_or_take_back(workspace, message, take_back)?;
        let branch = workspace.sandbox();
        let primary_dir = Path::new(workspace.path());
        let _ = self.evaluate_shadow_checkpoint(branch, primary_dir, &primary_res)?;
        Ok(primary_res)
    }
}

impl<W: WorkspaceStore, L: RefLedger + Send + Sync> WorkspaceStore for ShadowD6Workspace<W, L> {
    fn project(&self) -> &str {
        self.primary.project()
    }

    fn ensure_branch(
        &self,
        branch: &BranchName,
        from: Option<&str>,
    ) -> Result<Branch, WorkspaceError> {
        let primary_res = self.primary.ensure_branch(branch, from)?;
        if let Err(err) = self.mirror_ensure_branch(branch, from) {
            self.record_shadow_error("ensure_branch", err.to_string());
        }
        Ok(primary_res)
    }

    fn mount(&self, branch: &BranchName) -> Result<WorkspaceId, WorkspaceError> {
        self.mount_when(branch, &|| Ok(()))
    }

    fn mount_when(
        &self,
        branch: &BranchName,
        ready: &dyn Fn() -> Result<(), String>,
    ) -> Result<WorkspaceId, WorkspaceError> {
        let primary_ws = self.primary.mount_when(branch, ready)?;
        if let Err(err) = self.mirror_mount(branch) {
            self.record_shadow_error("mount", err.to_string());
        }
        Ok(primary_ws)
    }

    fn canonical(&self, workspace: &WorkspaceId) -> WorkspaceId {
        self.primary.canonical(workspace)
    }

    fn commit(&self, workspace: &WorkspaceId, message: &str) -> Result<Commit, WorkspaceError> {
        self.checkpoint(workspace, message)
    }

    fn unmount(&self, workspace: &WorkspaceId) -> Result<(), WorkspaceError> {
        self.unmount_when(workspace, &|| Ok(()))
    }

    fn unmount_when(
        &self,
        workspace: &WorkspaceId,
        ready: &dyn Fn() -> Result<(), WorkspaceError>,
    ) -> Result<(), WorkspaceError> {
        self.primary.unmount_when(workspace, ready)?;
        if let Ok(shadow_ws) = self.shadow.workspace_id_for_branch(workspace.sandbox())
            && self.shadow.is_mounted(&shadow_ws)
            && let Err(err) = self.shadow.unmount(&shadow_ws)
        {
            self.record_shadow_error("unmount", err.to_string());
        }
        Ok(())
    }

    fn is_mounted(&self, workspace: &WorkspaceId) -> bool {
        self.primary.is_mounted(workspace)
    }

    fn merge_main_clean(&self, branch: &BranchName) -> Result<BaseMerge, WorkspaceError> {
        let had_pending = self.shadow.has_pending_merge(branch.as_str());
        let pre_state = self.capture_pre_merge_snapshots(branch.as_str());
        let primary_res = self.primary.merge_main_clean(branch)?;
        self.evaluate_shadow_merge(branch.as_str(), had_pending, pre_state, &primary_res);
        Ok(primary_res)
    }

    fn merge_base(&self, workspace: &WorkspaceId) -> Result<BaseMerge, WorkspaceError> {
        self.merge_base_when(workspace, &|| Ok(()))
    }

    fn merge_base_when(
        &self,
        workspace: &WorkspaceId,
        ready: &dyn Fn() -> Result<(), String>,
    ) -> Result<BaseMerge, WorkspaceError> {
        let branch = workspace.sandbox();
        let had_pending = self.shadow.has_pending_merge(branch);
        let pre_state = self.capture_pre_merge_snapshots(branch);
        let primary_res = self.primary.merge_base_when(workspace, ready)?;
        self.evaluate_shadow_merge(branch, had_pending, pre_state, &primary_res);
        Ok(primary_res)
    }

    fn hold(&self, workspace: &WorkspaceId) -> Option<Hold> {
        let primary_hold = self.primary.hold(workspace);
        let shadow_hold = self
            .shadow
            .workspace_id_for_branch(workspace.sandbox())
            .ok()
            .and_then(|sw| self.shadow.hold(&sw));
        match (primary_hold, shadow_hold) {
            (None, None) => None,
            (p, s) => Some(Box::new((p, s)) as Hold),
        }
    }

    fn set_aside(
        &self,
        branch: &BranchName,
        start: Option<&str>,
        message: &str,
    ) -> Result<SetAside, WorkspaceError> {
        let res = self.primary.set_aside(branch, start, message)?;
        if let Err(err) = self.mirror_set_aside_to_shadow(branch.as_str(), &res) {
            self.record_shadow_error("set_aside", err.to_string());
        }
        Ok(res)
    }

    fn notices(&self, workspace: &WorkspaceId) -> Vec<String> {
        self.primary.notices(workspace)
    }

    fn notices_told(&self, workspace: &WorkspaceId, notices: &[String]) {
        self.primary.notices_told(workspace, notices);
        if let Ok(sw) = self.shadow.workspace_id_for_branch(workspace.sandbox()) {
            self.shadow.notices_told(&sw, notices);
        }
    }

    fn branch_head(&self, branch: &BranchName) -> Result<Option<CommitId>, WorkspaceError> {
        self.primary.branch_head(branch)
    }
}

type TreeComparison = (Vec<String>, Vec<String>, Vec<String>);

struct PreMergeSnapshots {
    main_oid: GitOid,
    branch_oid: GitOid,
    main_snap: LedgerSnapshot,
    branch_snap: LedgerSnapshot,
}

impl<W: WorkspaceStore, L: RefLedger + Send + Sync> ShadowD6Workspace<W, L> {
    fn record_checkpoint_outcome(
        &self,
        outcome: ShadowCheckpointOutcome,
    ) -> ShadowCheckpointOutcome {
        self.metrics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record_checkpoint(outcome.clone());
        outcome
    }

    fn record_shadow_error(&self, op: &str, detail: String) -> ShadowCheckpointOutcome {
        self.record_checkpoint_outcome(ShadowCheckpointOutcome::ShadowError {
            operation: op.to_string(),
            detail,
        })
    }

    fn record_merge_outcome(&self, outcome: MergeShadowOutcome) {
        self.metrics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record_merge(outcome);
    }

    fn record_multi_merge_base(&self) {
        self.metrics
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .merge_summary
            .rejected_multi_merge_base += 1;
    }

    fn git_exclude_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        if let Some(repo) = &self.git_repo {
            let exclude = repo.join("info/exclude");
            if fs::symlink_metadata(&exclude)
                .is_ok_and(|m| !m.file_type().is_symlink() && m.is_file())
            {
                files.push(exclude);
            }
        }
        files
    }

    fn mirror_ensure_branch(
        &self,
        branch: &BranchName,
        from: Option<&str>,
    ) -> Result<(), WorkspaceError> {
        if self.shadow.main_snapshot()?.is_none() {
            let _ = self.sync_main_from_git(None)?;
        }
        if self.shadow.branch_snapshot(branch.as_str())?.is_some() {
            return Ok(());
        }
        if let Some(repo) = &self.git_repo
            && let Some(branch_oid) = rev_parse_optional(repo, &format!("refs/heads/{branch}"))?
        {
            let snap = self.import_git_commit_graph(repo, branch_oid)?;
            let _ = self
                .shadow
                .ensure_branch(branch, Some(&snap.git_oid.to_hex()))?;
            return Ok(());
        }
        if let (Some(spec), Some(repo)) = (from, &self.git_repo)
            && let Ok(oid) = GitOid::parse_hex(spec)
            && self
                .shadow
                .ledger()
                .get_snapshot_by_oid(&oid)
                .map_err(|e| WorkspaceError::Unavailable(e.to_string()))?
                .is_none()
        {
            let _ = self.import_git_commit_graph(repo, oid)?;
        }
        let _ = self.shadow.ensure_branch(branch, from)?;
        Ok(())
    }

    fn mirror_mount(&self, branch: &BranchName) -> Result<(), WorkspaceError> {
        if self.shadow.branch_snapshot(branch.as_str())?.is_none()
            && let Some(repo) = &self.git_repo
            && let Some(branch_oid) = rev_parse_optional(repo, &format!("refs/heads/{branch}"))?
        {
            let snap = self.import_git_commit_graph(repo, branch_oid)?;
            if self.shadow.main_snapshot()?.is_none() {
                let _ = self.sync_main_from_git(None)?;
            }
            let _ = self
                .shadow
                .ensure_branch(branch, Some(&snap.git_oid.to_hex()))?;
        }
        if self.shadow.branch_snapshot(branch.as_str())?.is_some() {
            let _ = self.shadow.mount(branch)?;
        }
        Ok(())
    }

    fn evaluate_shadow_checkpoint(
        &self,
        branch: &str,
        primary_dir: &Path,
        primary_commit: &Commit,
    ) -> Result<ShadowCheckpointOutcome, WorkspaceError> {
        let extra_excludes = self.git_exclude_files();
        let simulate_unfiltered =
            self.tracked_set_mode == ShadowTrackedSetMode::SimulateUnfilteredScan;

        let scan_res = self.shadow.scan_directory_tree(
            branch,
            primary_dir,
            &extra_excludes,
            simulate_unfiltered,
        );
        let (candidate_tree, oversized, resolved_merge_main) = match scan_res {
            Ok(tuple) => tuple,
            Err(WorkspaceError::Refused { code, message }) if code == "TrackedSetLeak" => {
                let outcome =
                    self.record_checkpoint_outcome(ShadowCheckpointOutcome::TrackedSetLeak {
                        leaked_paths: extract_quoted_path(&message).into_iter().collect(),
                        detail: message.clone(),
                    });
                if self.strict_fail_closed {
                    return Err(WorkspaceError::Refused { code, message });
                }
                return Ok(outcome);
            }
            Err(err) => {
                return Ok(self.record_shadow_error("checkpoint_scan", err.to_string()));
            }
        };

        let d6_tree_oid = match self.shadow.store().project_tree_oid(&candidate_tree) {
            Ok(oid) => oid,
            Err(err) => {
                return Ok(self.record_shadow_error("project_tree_oid", err.to_string()));
            }
        };

        let Some(repo) = &self.git_repo else {
            return Ok(self.checkpoint_without_git_repo(
                branch,
                primary_dir,
                primary_commit,
                &candidate_tree,
                d6_tree_oid,
                &oversized,
                resolved_merge_main.as_ref(),
            ));
        };

        self.verify_shadow_against_git(
            branch,
            primary_dir,
            repo,
            primary_commit,
            &candidate_tree,
            d6_tree_oid,
            &oversized,
            resolved_merge_main.as_ref(),
            &extra_excludes,
        )
    }

    #[expect(clippy::too_many_arguments)]
    fn verify_shadow_against_git(
        &self,
        branch: &str,
        primary_dir: &Path,
        repo: &Path,
        primary_commit: &Commit,
        candidate_tree: &Digest,
        d6_tree_oid: GitOid,
        oversized: &[(TreePath, u64)],
        resolved_merge_main: Option<&Digest>,
        extra_excludes: &[PathBuf],
    ) -> Result<ShadowCheckpointOutcome, WorkspaceError> {
        let git_commit_oid = match primary_commit {
            Commit::Created(id) | Commit::Recreated { commit: id, .. } => {
                match GitOid::parse_hex(id.as_str()) {
                    Ok(oid) => oid,
                    Err(err) => {
                        return Ok(self.record_shadow_error("parse_git_commit", err.to_string()));
                    }
                }
            }
            Commit::Unchanged => match rev_parse_optional(repo, &format!("refs/heads/{branch}"))? {
                Some(oid) => oid,
                None => {
                    return Ok(self.record_shadow_error(
                        "resolve_unchanged_branch",
                        format!("missing git ref refs/heads/{branch}"),
                    ));
                }
            },
        };

        let git_tree_oid = match rev_parse_tree_oid(repo, git_commit_oid) {
            Ok(oid) => oid,
            Err(err) => {
                return Ok(self.record_shadow_error("rev_parse_tree", err.to_string()));
            }
        };

        let (extra_in_d6, missing_in_d6, differing_paths) =
            match self.compare_trees_vs_git(repo, candidate_tree, git_tree_oid) {
                Ok(diff) => diff,
                Err(err) => {
                    return Ok(self.record_shadow_error("compare_trees_vs_git", err.to_string()));
                }
            };

        if let Some(leak_outcome) = self.detect_tracked_set_leak_fail_closed(
            branch,
            primary_dir,
            repo,
            candidate_tree,
            &extra_in_d6,
            extra_excludes,
        )? {
            let detail = match &leak_outcome {
                ShadowCheckpointOutcome::TrackedSetLeak { detail, .. } => detail.clone(),
                _ => "tracked set leak detected".to_string(),
            };
            let recorded = self.record_checkpoint_outcome(leak_outcome);
            if self.strict_fail_closed {
                return Err(WorkspaceError::Refused {
                    code: "TrackedSetLeak".to_string(),
                    message: detail,
                });
            }
            return Ok(recorded);
        }

        if d6_tree_oid != git_tree_oid {
            return Ok(
                self.record_checkpoint_outcome(ShadowCheckpointOutcome::TreeOidMismatch {
                    git_tree_oid: Some(git_tree_oid),
                    d6_tree_oid,
                    extra_in_d6,
                    missing_in_d6,
                    differing_paths,
                }),
            );
        }

        match primary_commit {
            Commit::Unchanged => Ok(self.record_checkpoint_outcome(
                ShadowCheckpointOutcome::Agree {
                    tree_oid: Some(d6_tree_oid),
                    primary_commit: None,
                    shadow_commit: None,
                },
            )),
            Commit::Created(primary_id)
            | Commit::Recreated {
                commit: primary_id, ..
            } => self.publish_and_verify_shadow_commit(
                branch,
                primary_dir,
                repo,
                candidate_tree,
                oversized,
                resolved_merge_main,
                git_commit_oid,
                d6_tree_oid,
                primary_id,
            ),
        }
    }

    #[expect(clippy::too_many_arguments)]
    fn checkpoint_without_git_repo(
        &self,
        branch: &str,
        primary_dir: &Path,
        primary_commit: &Commit,
        candidate_tree: &Digest,
        d6_tree_oid: GitOid,
        oversized: &[(TreePath, u64)],
        resolved_merge_main: Option<&Digest>,
    ) -> ShadowCheckpointOutcome {
        let now_ms = 1_700_000_000_000_i64;
        let body = crate::cas_merkle::format_commit_body(
            "spawn <spawn@localhost> 1700000000 +0000",
            "spawn <spawn@localhost> 1700000000 +0000",
            "shadow checkpoint\n",
        );
        let shadow_commit = self.shadow.publish_branch_commit_with_body(
            branch,
            primary_dir,
            candidate_tree,
            oversized,
            resolved_merge_main,
            &body,
            now_ms,
            "d6:shadow_checkpoint",
        );
        match (primary_commit, shadow_commit) {
            (
                Commit::Created(p) | Commit::Recreated { commit: p, .. },
                Ok(Commit::Created(s) | Commit::Recreated { commit: s, .. }),
            ) => self.record_checkpoint_outcome(ShadowCheckpointOutcome::Agree {
                tree_oid: Some(d6_tree_oid),
                primary_commit: Some(p.clone()),
                shadow_commit: Some(s),
            }),
            (_, Ok(_)) => self.record_checkpoint_outcome(ShadowCheckpointOutcome::Agree {
                tree_oid: Some(d6_tree_oid),
                primary_commit: None,
                shadow_commit: None,
            }),
            (_, Err(err)) => self.record_shadow_error("shadow_publish", err.to_string()),
        }
    }

    #[expect(clippy::too_many_arguments)]
    fn publish_and_verify_shadow_commit(
        &self,
        branch: &str,
        primary_dir: &Path,
        repo: &Path,
        candidate_tree: &Digest,
        oversized: &[(TreePath, u64)],
        resolved_merge_main: Option<&Digest>,
        git_commit_oid: GitOid,
        d6_tree_oid: GitOid,
        primary_id: &CommitId,
    ) -> Result<ShadowCheckpointOutcome, WorkspaceError> {
        let parsed = match read_git_commit_raw(repo, git_commit_oid) {
            Ok(p) => p,
            Err(err) => {
                return Ok(self.record_shadow_error("read_git_commit", err.to_string()));
            }
        };
        if let Some(first_parent) = parsed.parent_oids.first()
            && self.shadow.branch_snapshot(branch)?.is_none()
            && let Err(err) = self.import_and_attach_branch(branch, repo, *first_parent)
        {
            return Ok(self.record_shadow_error("sync_missing_branch_head", err.to_string()));
        }
        let published = match self.shadow.publish_branch_commit_with_body(
            branch,
            primary_dir,
            candidate_tree,
            oversized,
            resolved_merge_main,
            &parsed.commit_body,
            parsed.committer_ms,
            "d6:shadow_checkpoint",
        ) {
            Ok(c) => c,
            Err(err) => {
                return Ok(self.record_shadow_error("publish_branch_commit", err.to_string()));
            }
        };

        let shadow_id = match &published {
            Commit::Created(id) | Commit::Recreated { commit: id, .. } => id.clone(),
            Commit::Unchanged => {
                return Ok(self.record_shadow_error(
                    "checkpoint_commit_parity",
                    format!(
                        "primary created commit {primary_id} while shadow reported Commit::Unchanged"
                    ),
                ));
            }
        };

        if shadow_id != *primary_id {
            return Ok(self.record_shadow_error(
                "checkpoint_commit_parity",
                format!("shadow commit OID {shadow_id} != primary Git commit OID {primary_id}"),
            ));
        }

        Ok(
            self.record_checkpoint_outcome(ShadowCheckpointOutcome::Agree {
                tree_oid: Some(d6_tree_oid),
                primary_commit: Some(primary_id.clone()),
                shadow_commit: Some(shadow_id),
            }),
        )
    }

    fn import_and_attach_branch(
        &self,
        branch: &str,
        repo: &Path,
        oid: GitOid,
    ) -> Result<(), WorkspaceError> {
        let snap = self.import_git_commit_graph(repo, oid)?;
        let _ = self.shadow.ensure_branch(
            &BranchName::new(branch).map_err(|e| WorkspaceError::Unavailable(e.to_string()))?,
            Some(&snap.git_oid.to_hex()),
        )?;
        Ok(())
    }

    fn detect_tracked_set_leak_fail_closed(
        &self,
        branch: &str,
        primary_dir: &Path,
        repo: &Path,
        candidate_tree: &Digest,
        extra_in_d6: &[String],
        extra_excludes: &[PathBuf],
    ) -> Result<Option<ShadowCheckpointOutcome>, WorkspaceError> {
        let head_tree = self.shadow.branch_snapshot(branch)?.map(|s| s.tree);
        let main_tree = self.shadow.main_snapshot()?.map(|s| s.tree);
        let policy = TrackedSetPolicy {
            base_tree: head_tree.as_ref(),
            main_tree: main_tree.as_ref(),
            extra_exclude_files: extra_excludes.to_vec(),
        };

        let mut leaked = BTreeSet::new();
        let mut details = Vec::new();
        if let Err(D6StoreError::IgnoredLeak { path, rule }) = self
            .shadow
            .store()
            .verify_no_ignored_untracked_leaks(primary_dir, candidate_tree, &policy)
        {
            details.push(format!(
                "verify_no_ignored_untracked_leaks: {path} ({rule})"
            ));
            leaked.insert(path);
        }

        if !extra_in_d6.is_empty() {
            let git_ignored = git_check_ignore_paths(repo, primary_dir, extra_in_d6)?;
            for p in git_ignored {
                details.push(format!("git check-ignore matched extra D6 path {p:?}"));
                leaked.insert(p);
            }
        }

        if leaked.is_empty() {
            return Ok(None);
        }
        let leaked_paths: Vec<String> = leaked.into_iter().collect();
        Ok(Some(ShadowCheckpointOutcome::TrackedSetLeak {
            leaked_paths,
            detail: details.join("; "),
        }))
    }

    fn compare_trees_vs_git(
        &self,
        repo: &Path,
        d6_tree: &Digest,
        git_tree_oid: GitOid,
    ) -> Result<TreeComparison, WorkspaceError> {
        let d6_flat = self
            .shadow
            .store()
            .flatten_tree(d6_tree)
            .map_err(|e| WorkspaceError::Unavailable(e.to_string()))?;
        let git_entries = git_ls_tree_recursive(repo, git_tree_oid)?;

        let mut extra_in_d6 = Vec::new();
        let mut missing_in_d6 = Vec::new();
        let mut differing_paths = Vec::new();

        for (path, entry) in &d6_flat {
            match git_entries.get(path) {
                None => extra_in_d6.push(TreePath::from_bytes(path.clone()).shown()),
                Some((git_mode, git_blob_oid)) => {
                    let mode_matches = match entry.kind {
                        spawn_cas::TreeEntryKind::File => git_mode == "100644",
                        spawn_cas::TreeEntryKind::Executable => git_mode == "100755",
                        spawn_cas::TreeEntryKind::Symlink => git_mode == "120000",
                        spawn_cas::TreeEntryKind::Gitlink => git_mode == "160000",
                        spawn_cas::TreeEntryKind::Dir => false,
                    };
                    let content_matches = match entry.kind {
                        spawn_cas::TreeEntryKind::Gitlink => {
                            entry.gitlink_oid() == Some(git_blob_oid.as_bytes().as_slice())
                        }
                        _ => entry
                            .digest()
                            .and_then(|d| {
                                spawn_cas::project_blob_oid(
                                    self.shadow.store().cas(),
                                    &d,
                                    entry.size,
                                )
                                .ok()
                            })
                            .is_some_and(|oid| oid == *git_blob_oid),
                    };
                    if !mode_matches || !content_matches {
                        differing_paths.push(TreePath::from_bytes(path.clone()).shown());
                    }
                }
            }
        }
        for path in git_entries.keys() {
            if !d6_flat.contains_key(path) {
                missing_in_d6.push(TreePath::from_bytes(path.clone()).shown());
            }
        }
        Ok((extra_in_d6, missing_in_d6, differing_paths))
    }

    fn capture_pre_merge_snapshots(&self, branch: &str) -> Option<PreMergeSnapshots> {
        let repo = self.git_repo.as_ref()?;
        let branch_oid = rev_parse_optional(repo, &format!("refs/heads/{branch}")).ok()??;
        let main_oid = rev_parse_optional(repo, "refs/heads/main").ok()??;
        let main_snap = self.import_git_commit_graph(repo, main_oid).ok()?;
        let branch_snap = self.import_git_commit_graph(repo, branch_oid).ok()?;
        if let Some(shadow_main) = self.shadow.main_snapshot().ok().flatten() {
            let _ = shadow_main;
        } else {
            let _ = self.shadow.land_main_snapshot(
                None,
                &main_snap.id,
                None,
                "d6:shadow_bootstrap_main",
            );
        }
        Some(PreMergeSnapshots {
            main_oid,
            branch_oid,
            main_snap,
            branch_snap,
        })
    }

    fn evaluate_shadow_merge(
        &self,
        branch: &str,
        had_pending: bool,
        pre_state: Option<PreMergeSnapshots>,
        primary_res: &BaseMerge,
    ) {
        let Some(repo) = &self.git_repo else {
            return;
        };
        let Some(pre) = pre_state else {
            return;
        };

        match primary_res {
            BaseMerge::Current => {}
            BaseMerge::Merged { commit, .. } => {
                self.handle_clean_shadow_merge(branch, repo, &pre, commit);
            }
            BaseMerge::Conflicted { files, marked } => {
                if !had_pending {
                    self.handle_conflicted_shadow_merge(branch, repo, &pre, files, *marked);
                }
            }
        }
    }

    fn handle_clean_shadow_merge(
        &self,
        branch: &str,
        repo: &Path,
        pre: &PreMergeSnapshots,
        commit: &CommitId,
    ) {
        let Ok(git_commit_oid) = GitOid::parse_hex(commit.as_str()) else {
            self.record_shadow_error(
                "shadow_merge_parse",
                format!("invalid merge commit id {commit}"),
            );
            return;
        };
        let Ok(git_tree_oid) = rev_parse_tree_oid(repo, git_commit_oid) else {
            self.record_shadow_error(
                "shadow_merge_tree",
                format!("rev-parse {git_commit_oid}^{{tree}} failed"),
            );
            return;
        };
        match merge_snapshots(
            self.shadow.store(),
            &pre.main_snap.id,
            &pre.branch_snap.id,
            None,
        ) {
            Ok((_base, report)) => {
                let outcome = classify_shadow_merge(&report, true, Some(git_tree_oid), 0, false);
                let blocker = outcome.is_blocker();
                self.record_merge_outcome(outcome);
                if !blocker
                    && let Err(err) = self.publish_shadow_git_merge_commit(
                        branch,
                        repo,
                        pre,
                        &report.tree,
                        git_commit_oid,
                    )
                {
                    self.record_shadow_error("shadow_merge_publish", err.to_string());
                }
            }
            Err(MergeBaseError::MultipleMergeBases { .. }) => {
                self.record_multi_merge_base();
            }
            Err(err) => {
                self.record_shadow_error("shadow_merge_snapshots", err.to_string());
            }
        }
    }

    fn handle_conflicted_shadow_merge(
        &self,
        branch: &str,
        repo: &Path,
        pre: &PreMergeSnapshots,
        files: &[String],
        marked: bool,
    ) {
        match merge_snapshots(
            self.shadow.store(),
            &pre.main_snap.id,
            &pre.branch_snap.id,
            None,
        ) {
            Ok((_base, report)) => {
                let (git_clean, git_tree_oid, git_conflict_paths) = git_merge_tree_probe(
                    repo,
                    pre.main_oid,
                    pre.branch_oid,
                )
                .unwrap_or((false, None, files.len()));
                let non_exact_rename = if git_clean && !report.is_clean() {
                    git_has_non_exact_rename_diff(repo, pre.main_oid, pre.branch_oid)
                } else {
                    false
                };
                let outcome = classify_shadow_merge(
                    &report,
                    git_clean,
                    git_tree_oid,
                    git_conflict_paths,
                    non_exact_rename,
                );
                let blocker = outcome.is_blocker();
                self.record_merge_outcome(outcome);
                if !blocker && marked {
                    let conflict_paths: Vec<TreePath> =
                        files.iter().map(|s| TreePath::from(s.as_str())).collect();
                    if let Err(err) = self.shadow.write_pending_merge_for_shadow(
                        branch,
                        &pre.main_snap.id,
                        &pre.branch_snap.id,
                        &conflict_paths,
                    ) {
                        self.record_shadow_error("shadow_write_pending", err.to_string());
                    }
                }
            }
            Err(MergeBaseError::MultipleMergeBases { .. }) => {
                self.record_multi_merge_base();
            }
            Err(err) => {
                self.record_shadow_error("shadow_merge_conflict", err.to_string());
            }
        }
    }

    fn publish_shadow_git_merge_commit(
        &self,
        branch: &str,
        repo: &Path,
        pre: &PreMergeSnapshots,
        merged_tree: &Digest,
        git_commit_oid: GitOid,
    ) -> Result<(), WorkspaceError> {
        let parsed = read_git_commit_raw(repo, git_commit_oid)?;
        let published = self.shadow.publish_merge_commit_with_body(
            branch,
            &pre.branch_snap.id,
            &pre.main_snap.id,
            merged_tree,
            &parsed.commit_body,
            parsed.committer_ms,
            "d6:shadow_merge_base",
        )?;
        if published.git_oid != git_commit_oid {
            return Err(WorkspaceError::Unavailable(format!(
                "shadow merge commit OID {} != git merge commit OID {}",
                published.git_oid, git_commit_oid
            )));
        }
        Ok(())
    }

    fn mirror_set_aside_to_shadow(
        &self,
        branch: &str,
        res: &SetAside,
    ) -> Result<(), WorkspaceError> {
        let SetAside::Created { commit, kept, .. } = res else {
            return Ok(());
        };
        let Some(repo) = &self.git_repo else {
            return Ok(());
        };
        let Some(shadow_head) = self.shadow.branch_snapshot(branch)? else {
            return Ok(());
        };
        let kept_oid = GitOid::parse_hex(kept.as_str())
            .map_err(|e| WorkspaceError::Unavailable(e.to_string()))?;
        let commit_oid = GitOid::parse_hex(commit.as_str())
            .map_err(|e| WorkspaceError::Unavailable(e.to_string()))?;
        if shadow_head.git_oid == commit_oid {
            return Ok(());
        }

        let mut expected_head = shadow_head.id;
        if shadow_head.git_oid != kept_oid {
            let kept_parsed = read_git_commit_raw(repo, kept_oid)?;
            let kept_tree = self.import_git_commit_tree_only(repo, kept_parsed.tree_oid)?;
            let pub_kept = self.shadow.publish_set_aside_commit_with_body(
                branch,
                &expected_head,
                &kept_tree,
                &kept_parsed.commit_body,
                kept_parsed.committer_ms,
                "d6:shadow_set_aside_kept",
            )?;
            expected_head = pub_kept.snapshot;
        }

        let sa_parsed = read_git_commit_raw(repo, commit_oid)?;
        let sa_tree = self.import_git_commit_tree_only(repo, sa_parsed.tree_oid)?;
        let pub_sa = self.shadow.publish_set_aside_commit_with_body(
            branch,
            &expected_head,
            &sa_tree,
            &sa_parsed.commit_body,
            sa_parsed.committer_ms,
            "d6:shadow_set_aside",
        )?;
        if pub_sa.git_oid != commit_oid {
            return Err(WorkspaceError::Unavailable(format!(
                "shadow set_aside commit OID {} != primary {}",
                pub_sa.git_oid, commit_oid
            )));
        }
        Ok(())
    }

    fn import_git_commit_tree_only(
        &self,
        repo: &Path,
        tree_oid: GitOid,
    ) -> Result<Digest, WorkspaceError> {
        let mut blob_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let digest = import_git_tree_recursive(
            self.shadow.store(),
            repo,
            tree_oid,
            &mut blob_cache,
            &mut tree_cache,
        )
        .map_err(|e| WorkspaceError::Unavailable(e.to_string()))?;
        self.shadow
            .store()
            .cas()
            .sync_store()
            .map_err(|e| WorkspaceError::Unavailable(e.to_string()))?;
        Ok(digest)
    }

    fn import_git_commit_graph(
        &self,
        repo: &Path,
        tip_oid: GitOid,
    ) -> Result<LedgerSnapshot, WorkspaceError> {
        if let Some(existing) = self
            .shadow
            .ledger()
            .get_snapshot_by_oid(&tip_oid)
            .map_err(|e| WorkspaceError::Unavailable(e.to_string()))?
            && self.shadow.store().cas().has(&existing.id)
        {
            return Ok(existing);
        }

        let order = topo_rev_list_single(repo, tip_oid)?;
        let mut blob_cache = HashMap::new();
        let mut tree_cache = HashMap::new();
        let mut resolved: HashMap<GitOid, LedgerSnapshot> = HashMap::new();

        for oid in &order {
            self.import_single_git_commit(
                repo,
                *oid,
                &mut blob_cache,
                &mut tree_cache,
                &mut resolved,
            )?;
        }

        self.shadow
            .store()
            .cas()
            .sync_store()
            .map_err(|e| WorkspaceError::Unavailable(e.to_string()))?;

        for oid in &order {
            let row = &resolved[oid];
            let _ = self
                .shadow
                .ledger()
                .record_snapshot(
                    &row.id,
                    &row.git_oid,
                    &row.tree,
                    &row.parents,
                    &row.project,
                    row.created_at_ms,
                )
                .map_err(|e| WorkspaceError::Unavailable(e.to_string()))?;
        }

        Ok(resolved.remove(&tip_oid).expect("tip commit resolved"))
    }

    fn import_single_git_commit(
        &self,
        repo: &Path,
        oid: GitOid,
        blob_cache: &mut HashMap<(GitOid, bool), (Digest, u64)>,
        tree_cache: &mut HashMap<GitOid, Digest>,
        resolved: &mut HashMap<GitOid, LedgerSnapshot>,
    ) -> Result<(), WorkspaceError> {
        if let Some(row) = self
            .shadow
            .ledger()
            .get_snapshot_by_oid(&oid)
            .map_err(|e| WorkspaceError::Unavailable(e.to_string()))?
            && self.shadow.store().cas().has(&row.id)
        {
            resolved.insert(oid, row);
            return Ok(());
        }

        let parsed = read_git_commit_raw(repo, oid)?;
        let tree_digest = import_git_tree_recursive(
            self.shadow.store(),
            repo,
            parsed.tree_oid,
            blob_cache,
            tree_cache,
        )
        .map_err(|e| WorkspaceError::Unavailable(e.to_string()))?;

        let mut parents = Vec::with_capacity(parsed.parent_oids.len());
        for p_oid in &parsed.parent_oids {
            let p_row = if let Some(hit) = resolved.get(p_oid) {
                hit.clone()
            } else {
                self.shadow
                    .ledger()
                    .get_snapshot_by_oid(p_oid)
                    .map_err(|e| WorkspaceError::Unavailable(e.to_string()))?
                    .ok_or_else(|| {
                        WorkspaceError::Unavailable(format!("missing parent snapshot for {p_oid}"))
                    })?
            };
            parents.push(p_row.id);
        }

        let snapshot = Snapshot {
            tree: tree_digest.clone(),
            parents: parents.clone(),
            commit_body: parsed.commit_body,
        };
        let snap_id = self
            .shadow
            .store()
            .cas()
            .put_merkle_snapshot(&snapshot)
            .map_err(|e| WorkspaceError::Unavailable(e.to_string()))?;
        let projected = self
            .shadow
            .store()
            .project_snapshot_oid(&snapshot, &parsed.parent_oids)
            .map_err(|e| WorkspaceError::Unavailable(e.to_string()))?;
        if projected != oid {
            return Err(WorkspaceError::Unavailable(format!(
                "imported git commit {oid} projected to mismatched OID {projected}"
            )));
        }
        resolved.insert(
            oid,
            LedgerSnapshot {
                id: snap_id,
                git_oid: oid,
                tree: tree_digest,
                parents,
                project: self.shadow.project_name().to_string(),
                created_at_ms: parsed.committer_ms,
            },
        );
        Ok(())
    }
}

struct RawGitCommit {
    tree_oid: GitOid,
    parent_oids: Vec<GitOid>,
    commit_body: Vec<u8>,
    committer_ms: i64,
}

fn looks_like_git_dir(path: &Path) -> bool {
    path.join("HEAD").is_file() && path.join("objects").is_dir()
}

fn git_bare(repo: &Path) -> Command {
    let mut cmd = git_command("git");
    cmd.arg("--git-dir")
        .arg(repo)
        .arg("-c")
        .arg("core.quotepath=false");
    cmd
}

fn rev_parse_optional(repo: &Path, spec: &str) -> Result<Option<GitOid>, WorkspaceError> {
    let out = git_bare(repo)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{spec}^{{commit}}"),
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|err| WorkspaceError::Unavailable(format!("run git rev-parse: {err}")))?;
    if !out.status.success() {
        return Ok(None);
    }
    let hex = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if hex.is_empty() {
        return Ok(None);
    }
    GitOid::parse_hex(&hex)
        .map(Some)
        .map_err(|err| WorkspaceError::Unavailable(err.to_string()))
}

fn rev_parse_tree_oid(repo: &Path, commit: GitOid) -> Result<GitOid, WorkspaceError> {
    let out = git_bare(repo)
        .args(["rev-parse", "--verify", &format!("{commit}^{{tree}}")])
        .stdin(Stdio::null())
        .output()
        .map_err(|err| WorkspaceError::Unavailable(format!("run git rev-parse: {err}")))?;
    if !out.status.success() {
        return Err(WorkspaceError::Unavailable(format!(
            "git rev-parse {commit}^{{tree}}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let hex = String::from_utf8_lossy(&out.stdout).trim().to_string();
    GitOid::parse_hex(&hex).map_err(|err| WorkspaceError::Unavailable(err.to_string()))
}

fn topo_rev_list_single(repo: &Path, tip: GitOid) -> Result<Vec<GitOid>, WorkspaceError> {
    let out = git_bare(repo)
        .args(["rev-list", "--topo-order", "--reverse", &tip.to_hex()])
        .stdin(Stdio::null())
        .output()
        .map_err(|err| WorkspaceError::Unavailable(format!("run git rev-list: {err}")))?;
    if !out.status.success() {
        return Err(WorkspaceError::Unavailable(format!(
            "git rev-list {tip}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let mut oids = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            oids.push(
                GitOid::parse_hex(trimmed)
                    .map_err(|err| WorkspaceError::Unavailable(err.to_string()))?,
            );
        }
    }
    Ok(oids)
}

fn read_git_commit_raw(repo: &Path, oid: GitOid) -> Result<RawGitCommit, WorkspaceError> {
    let out = git_bare(repo)
        .args(["cat-file", "commit", &oid.to_hex()])
        .stdin(Stdio::null())
        .output()
        .map_err(|err| WorkspaceError::Unavailable(format!("run git cat-file commit: {err}")))?;
    if !out.status.success() {
        return Err(WorkspaceError::Unavailable(format!(
            "git cat-file commit {oid}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let raw = out.stdout;
    let mut pos = 0_usize;

    let tree_line = next_raw_line(&raw, &mut pos).ok_or_else(|| {
        WorkspaceError::Unavailable(format!("missing tree header in commit {oid}"))
    })?;
    let tree_hex = std::str::from_utf8(tree_line.strip_prefix(b"tree ").ok_or_else(|| {
        WorkspaceError::Unavailable(format!("malformed tree header in commit {oid}"))
    })?)
    .map_err(|_| WorkspaceError::Unavailable("non-utf8 tree header".to_string()))?;
    let tree_oid =
        GitOid::parse_hex(tree_hex).map_err(|err| WorkspaceError::Unavailable(err.to_string()))?;

    let mut parent_oids = Vec::new();
    loop {
        let start = pos;
        let Some(line) = next_raw_line(&raw, &mut pos) else {
            break;
        };
        if let Some(p_bytes) = line.strip_prefix(b"parent ") {
            let hex = std::str::from_utf8(p_bytes)
                .map_err(|_| WorkspaceError::Unavailable("non-utf8 parent header".to_string()))?;
            parent_oids.push(
                GitOid::parse_hex(hex)
                    .map_err(|err| WorkspaceError::Unavailable(err.to_string()))?,
            );
        } else {
            pos = start;
            break;
        }
    }
    let commit_body = raw[pos..].to_vec();
    let committer_ms = parse_committer_timestamp_ms(&commit_body);
    Ok(RawGitCommit {
        tree_oid,
        parent_oids,
        commit_body,
        committer_ms,
    })
}

fn next_raw_line<'a>(bytes: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    if *pos >= bytes.len() {
        return None;
    }
    let rest = &bytes[*pos..];
    if let Some(rel_end) = rest.iter().position(|&b| b == b'\n') {
        let line = &rest[..rel_end];
        *pos += rel_end + 1;
        Some(line)
    } else {
        *pos = bytes.len();
        Some(rest)
    }
}

fn parse_committer_timestamp_ms(commit_body: &[u8]) -> i64 {
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

fn import_git_tree_recursive(
    store: &CasMerkleStore,
    repo: &Path,
    tree_oid: GitOid,
    blob_cache: &mut HashMap<(GitOid, bool), (Digest, u64)>,
    tree_cache: &mut HashMap<GitOid, Digest>,
) -> Result<Digest, D6StoreError> {
    if let Some(hit) = tree_cache.get(&tree_oid) {
        return Ok(hit.clone());
    }
    let out = git_bare(repo)
        .args(["ls-tree", "-z", &tree_oid.to_hex()])
        .stdin(Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(D6StoreError::GitBorder {
            command: format!("ls-tree -z {tree_oid}"),
            detail: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }

    let mut entries = Vec::new();
    for record in nul_fields(&out.stdout) {
        let tab_pos = record
            .iter()
            .position(|&b| b == b'\t')
            .ok_or_else(|| D6StoreError::InvalidInput("malformed ls-tree record".to_string()))?;
        let meta = std::str::from_utf8(&record[..tab_pos])
            .map_err(|_| D6StoreError::InvalidInput("non-utf8 ls-tree meta".to_string()))?;
        let name = record[tab_pos + 1..].to_vec();
        let parts: Vec<&str> = meta.split(' ').collect();
        if parts.len() != 3 {
            return Err(D6StoreError::InvalidInput(format!(
                "unexpected ls-tree meta: {meta:?}"
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
                    let imported = import_git_blob(store.cas(), repo, obj_oid, allow_ptr)?;
                    blob_cache.insert(key, imported.clone());
                    imported
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
                    "unsupported ls-tree entry {mode} {obj_type}"
                )));
            }
        }
    }

    let tree = Tree::from_entries(entries)?;
    let digest = store.put_tree_cached(&tree)?;
    let projected = store.project_tree_oid(&digest)?;
    if projected != tree_oid {
        return Err(D6StoreError::OidMismatch {
            what: "tree",
            expected: tree_oid.to_hex(),
            actual: projected.to_hex(),
        });
    }
    tree_cache.insert(tree_oid, digest.clone());
    Ok(digest)
}

fn import_git_blob(
    cas: &Store,
    repo: &Path,
    blob_oid: GitOid,
    allow_pointer_resolution: bool,
) -> Result<(Digest, u64), D6StoreError> {
    let hex = blob_oid.to_hex();
    let size_out = git_bare(repo)
        .args(["cat-file", "-s", &hex])
        .stdin(Stdio::null())
        .output()?;
    if !size_out.status.success() {
        return Err(D6StoreError::GitBorder {
            command: format!("cat-file -s {hex}"),
            detail: String::from_utf8_lossy(&size_out.stderr).trim().to_string(),
        });
    }
    let git_size: u64 = String::from_utf8_lossy(&size_out.stdout)
        .trim()
        .parse()
        .map_err(|_| D6StoreError::InvalidInput("invalid git blob size".to_string()))?;

    let mut child = git_bare(repo)
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
        if !out.status.success() {
            return Err(D6StoreError::GitBorder {
                command: format!("cat-file blob {hex}"),
                detail: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        if let Some((ptr_digest, ptr_size)) = parse_pointer_bytes(&small)
            && ptr_size > GIT_POINTER_THRESHOLD
            && cas.has_blob(&ptr_digest)
            && cas.stream_blob(&ptr_digest, &mut io::sink())? == ptr_size
        {
            cas.touch(NS_BLOB, &ptr_digest)?;
            return Ok((ptr_digest, ptr_size));
        }
        let stored = cas.put_blob(NS_BLOB, &mut small.as_slice())?;
        return Ok((stored.digest, stored.size));
    }

    let mut reader = BufReader::new(stdout);
    let stored_res = cas.put_blob(NS_BLOB, &mut reader);
    drop(reader);
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(D6StoreError::GitBorder {
            command: format!("cat-file blob {hex}"),
            detail: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    let stored = stored_res?;
    Ok((stored.digest, stored.size))
}

fn parse_pointer_bytes(bytes: &[u8]) -> Option<(Digest, u64)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let mut lines = text.lines();
    if lines.next()? != "spawn-pointer-v1" {
        return None;
    }
    let digest = Digest::parse(lines.next()?).ok()?;
    let size = lines.next()?.strip_prefix("size ")?.parse::<u64>().ok()?;
    if lines.next().is_some() {
        return None;
    }
    Some((digest, size))
}

fn git_ls_tree_recursive(
    repo: &Path,
    tree_oid: GitOid,
) -> Result<BTreeMap<Vec<u8>, (String, GitOid)>, WorkspaceError> {
    let out = git_bare(repo)
        .args(["ls-tree", "-r", "-z", "--full-tree", &tree_oid.to_hex()])
        .stdin(Stdio::null())
        .output()
        .map_err(|err| WorkspaceError::Unavailable(format!("run git ls-tree: {err}")))?;
    if !out.status.success() {
        return Err(WorkspaceError::Unavailable(format!(
            "git ls-tree {tree_oid}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let mut map = BTreeMap::new();
    for rec in nul_fields(&out.stdout) {
        let Some(tab) = rec.iter().position(|&b| b == b'\t') else {
            continue;
        };
        let meta = std::str::from_utf8(&rec[..tab])
            .map_err(|_| WorkspaceError::Unavailable("non-utf8 ls-tree meta".to_string()))?;
        let parts: Vec<&str> = meta.split(' ').collect();
        if parts.len() != 3 {
            continue;
        }
        let oid = GitOid::parse_hex(parts[2])
            .map_err(|err| WorkspaceError::Unavailable(err.to_string()))?;
        map.insert(rec[tab + 1..].to_vec(), (parts[0].to_string(), oid));
    }
    Ok(map)
}

fn git_check_ignore_paths(
    repo: &Path,
    work_tree: &Path,
    paths: &[String],
) -> Result<Vec<String>, WorkspaceError> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let mut input = Vec::new();
    for p in paths {
        input.extend_from_slice(p.as_bytes());
        input.push(0);
    }
    let mut child = git_command("git")
        .arg(format!("--git-dir={}", repo.display()))
        .arg(format!("--work-tree={}", work_tree.display()))
        .current_dir(work_tree)
        .args(["check-ignore", "--no-index", "-z", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| WorkspaceError::Unavailable(format!("run git check-ignore: {err}")))?;
    let stdin = child.stdin.take();
    let out = std::thread::scope(|scope| {
        scope.spawn(|| {
            use std::io::Write as _;
            stdin.map(|mut s| s.write_all(&input))
        });
        child.wait_with_output()
    })
    .map_err(|err| WorkspaceError::Unavailable(format!("wait git check-ignore: {err}")))?;
    // Exit status 0 = at least one ignored match, 1 = no match, >1 = error
    if !out.status.success() && out.status.code() != Some(1) {
        return Err(WorkspaceError::Unavailable(format!(
            "git check-ignore failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(nul_fields(&out.stdout)
        .map(|f| String::from_utf8_lossy(f).into_owned())
        .collect())
}

fn git_merge_tree_probe(
    repo: &Path,
    main_oid: GitOid,
    branch_oid: GitOid,
) -> Result<(bool, Option<GitOid>, usize), WorkspaceError> {
    let out = git_bare(repo)
        .arg(format!("--attr-source={main_oid}"))
        .args([
            "merge-tree",
            "--write-tree",
            "--name-only",
            "-z",
            &main_oid.to_hex(),
            &branch_oid.to_hex(),
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|err| WorkspaceError::Unavailable(format!("run git merge-tree: {err}")))?;
    let clean = out.status.success();
    let mut fields = out.stdout.split(|&b| b == 0);
    let tree_str = String::from_utf8_lossy(fields.next().unwrap_or_default())
        .trim()
        .to_string();
    let tree_oid = GitOid::parse_hex(&tree_str).ok();
    let mut conflict_files: HashSet<Vec<u8>> = HashSet::new();
    if !clean {
        for f in fields.take_while(|f| !f.is_empty()) {
            conflict_files.insert(f.to_vec());
        }
    }
    Ok((clean, tree_oid, conflict_files.len()))
}

fn git_has_non_exact_rename_diff(repo: &Path, main_oid: GitOid, branch_oid: GitOid) -> bool {
    let Ok(base_out) = git_bare(repo)
        .args(["merge-base", &main_oid.to_hex(), &branch_oid.to_hex()])
        .stdin(Stdio::null())
        .output()
    else {
        return false;
    };
    if !base_out.status.success() {
        return false;
    }
    let base_hex = String::from_utf8_lossy(&base_out.stdout).trim().to_string();
    for tip in [main_oid.to_hex(), branch_oid.to_hex()] {
        let Ok(out) = git_bare(repo)
            .args(["diff-tree", "-r", "-M", "--summary", &base_hex, &tip])
            .stdin(Stdio::null())
            .output()
        else {
            continue;
        };
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            if line.trim_start().starts_with("rename ")
                && let Some(pct_start) = line.rfind('(')
                && let Some(pct) = line[pct_start + 1..].strip_suffix("%)")
                && pct != "100"
            {
                return true;
            }
        }
    }
    false
}

fn extract_quoted_path(message: &str) -> Option<String> {
    let start = message.find('"')?;
    let rest = &message[start + 1..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::Scratch;
    use crate::cas_merkle::InMemoryRefLedger;
    use crate::test_git_command;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = test_git_command()
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn shadow_oracle_agrees_on_git_parity_and_blocks_ignored_leaks_fail_closed() {
        let scratch = Scratch::new("shadow-unit");
        let checkout = scratch.join("repo");
        fs::create_dir_all(&checkout).expect("repo");
        git(&checkout, &["init", "-q", "--initial-branch=main"]);
        fs::write(checkout.join(".gitignore"), ".env\n*.secret\n").expect("write");
        fs::write(checkout.join("README.md"), "base\n").expect("write");
        git(&checkout, &["add", "-A"]);
        git(&checkout, &["commit", "-qm", "base"]);

        let bare = checkout.join(".git");
        let primary = GitWorkspace::new(&bare, scratch.join("git-work"));
        let cas = Arc::new(CasMerkleStore::open(scratch.join("cas")).expect("cas"));
        let ledger = Arc::new(InMemoryRefLedger::new());
        let d6 = D6Workspace::new("proj", scratch.join("d6-work"), cas.clone(), ledger.clone());
        let shadow_strict = ShadowD6Workspace::new(primary.clone(), d6);

        let branch = BranchName::new("agent_a").expect("branch");
        shadow_strict
            .ensure_branch(&branch, None)
            .expect("ensure_branch");
        let ws = shadow_strict.mount(&branch).expect("mount");
        let ws_dir = PathBuf::from(ws.path());

        fs::write(ws_dir.join(".env"), "TOP_SECRET=1\n").expect("write ignored");
        fs::write(ws_dir.join("work.txt"), "hello d6\n").expect("write tracked");

        let (commit_res, outcome) = shadow_strict
            .checkpoint_with_outcome(&ws, "first checkpoint")
            .expect("checkpoint");
        assert!(matches!(commit_res, Commit::Created(_)));
        assert!(matches!(
            outcome,
            ShadowCheckpointOutcome::Agree {
                primary_commit: Some(ref p),
                shadow_commit: Some(ref s),
                ..
            } if p == s
        ));
        let metrics = shadow_strict.metrics();
        assert_eq!(metrics.checkpoints_agreed, 1);
        assert_eq!(metrics.tree_mismatches, 0);
        assert_eq!(metrics.tracked_set_leaks, 0);

        // Negative test: unfiltered shadow scan must fail-closed with TrackedSetLeak & never publish `.env`
        let head_before = shadow_strict
            .shadow()
            .branch_snapshot("agent_a")
            .expect("get")
            .expect("head")
            .id;

        let d6_leaky = D6Workspace::new("proj", scratch.join("d6-work"), cas, ledger.clone());
        let leaky_oracle = ShadowD6Workspace::new(primary, d6_leaky)
            .with_tracked_set_mode(ShadowTrackedSetMode::SimulateUnfilteredScan)
            .with_strict_fail_closed(true);

        fs::write(ws_dir.join("work.txt"), "second edit\n").expect("edit work");
        let err = leaky_oracle
            .checkpoint_with_outcome(&ws, "leaky checkpoint")
            .expect_err("strict fail-closed must refuse TrackedSetLeak");
        assert!(
            matches!(err, WorkspaceError::Refused { ref code, .. } if code == "TrackedSetLeak")
        );
        let leaky_metrics = leaky_oracle.metrics();
        assert_eq!(leaky_metrics.tracked_set_leaks, 1);
        assert_eq!(leaky_metrics.tree_mismatches, 0);
        assert!(matches!(
            leaky_metrics.last_checkpoint,
            Some(ShadowCheckpointOutcome::TrackedSetLeak { ref leaked_paths, .. })
                if leaked_paths.contains(&".env".to_string())
        ));
        // Confirm ledger did NOT advance when leak was detected
        let head_after = leaky_oracle
            .shadow()
            .branch_snapshot("agent_a")
            .expect("get")
            .expect("head")
            .id;
        assert_eq!(head_after, head_before);
    }
}
