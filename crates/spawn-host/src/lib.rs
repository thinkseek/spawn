//! Host adapters: embedded `herdr-core` / legacy `herdr` terminals, the workspace stores (Git, the opt-in
//! copy-on-write and overlay stores, behind one [`LocalWorkspaceStore`]),
//! the artifact deduplicator, the local transcript archive, and the
//! filesystem view (`bwrap`) an engine without deny rules runs in.
//!
//! The Herdr and git adapters wrap external binaries (`spawn-hc` / `herdr`,
//! `git`) and apply uniform locale and error handling (`git` runs in the C
//! locale via [`git_command`]).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use spawn_core::id::{TerminalName, WorkspaceId};
use spawn_core::port::TerminalError;

pub mod agent_files;
pub mod archive_mask;
pub mod beneath;
pub mod cas_dedup;
pub mod cas_merkle;
pub mod container;
pub mod cow;
pub mod d6_cutover;
pub mod d6_layer0;
pub mod d6_shadow;
pub mod d6_workspace;
pub mod engine_view;
pub mod git;
pub mod git_border;
pub mod herdr;
pub mod herdr_core;
pub mod herdr_witness;
pub mod home;
pub mod merkle_merge;
#[cfg(target_os = "linux")]
pub mod overlay;
pub mod process;
pub mod root;
#[cfg(any(test, feature = "testing"))]
pub mod scratch;
mod terminal_support;
pub mod transcripts;
pub mod tree_path;
pub mod watch;
pub mod workspace;

#[cfg(test)]
mod audit;

pub use cas_dedup::{
    ArtifactRoot, DedupConfig, DedupError, DedupReport, PruneReport, dedup_dirs, prune_unused,
};
pub use cas_merkle::{
    BUILD_OUTPUT as D6_BUILD_OUTPUT, CasMerkleStore, D6RefKind, D6StoreError, InMemoryRefLedger,
    LedgerRef, LedgerSnapshot, MaterializeReport, PublishRequest, PublishedSnapshot, RefLedger,
    ScannedEntry, ScannedKind, TrackedSetPolicy, format_commit_body,
};
pub use container::{BwrapCapability, ContainerEnv, detect_container, probe_bwrap};
pub use cow::{
    CloneKind, CowStats, CowWorkspace, DirectoryOccupancy, WorkspaceLease, clone_or_copy,
    cow_clone_supported, directory_occupancy, live_users, reflink_file,
};
pub use d6_cutover::{
    BackfillReport, BundleDrillReport, CutoverEvent, CutoverParityReport, CutoverPhase,
    CutoverRefParity, GcFenceStatus, LocalOnlyLargeFile, PendingMergeAudit, PreCutoverCleanReport,
    ProjectCutover, ProjectStorageMode, ReconcileReport, RollbackReport,
    assert_zero_pending_merges, bind_store_gc_fence, check_gc_fence, is_readonly_border,
    reconcile_refs_journal, run_persistent_gc_for_ledger, verify_bundle_snapshot_roundtrip,
    verify_cutover_parity, verify_zero_pending_merges,
};
pub use d6_layer0::{
    EolPathFinding, ExistingDestPolicy, ExportAttrRuleFinding, ExportAttributesAudit,
    Layer0MaterializeReport, Layer0Options, audit_export_attributes, materialize_layer0,
    materialize_workspace_from_tree,
};
pub use d6_shadow::{
    SetAsideWhen, ShadowCheckpointOutcome, ShadowD6Workspace, ShadowOracleMetrics,
    ShadowTrackedSetMode,
};
pub use d6_workspace::D6Workspace;
pub use engine_view::{
    BwrapView, ContainerView, ExtraMount, HomeIsolation, HostInheritMode, ViewConfig,
};
#[cfg(any(test, feature = "testing"))]
pub use git::test_git_command;
pub use git::{
    DEFAULT_MAX_FILE_MB, ENGINE_RUNTIME_FILES, GitWorkspace, LARGE_FILE_TRAILER, MAIN_COPIES,
    SET_ASIDE_FILES, SET_ASIDE_WAITS, git_command, has_conflict_block, has_conflict_block_sized,
    max_file_bytes, names_files_outside_git,
};
pub use git_border::{BorderRefTarget, ExportReport, GitBorderAdapter, ImportReport, ImportedRef};
pub use herdr::{HerdrHost, HerdrPool};
pub use herdr_core::HerdrCoreHost;
pub use merkle_merge::{
    BuiltMergeSnapshot, ConflictKind, ConflictLabels, EolAttr, GitAttributes, MergeBaseError,
    MergeConflict, MergeDriver, MergeOptions, MergeReport, MergeShadowOutcome, MergeSnapshotSpec,
    PathAttributes, ShadowBlockerReason, ShadowOracleSummary, TextAttr, TextMergeResult,
    build_merge_snapshot, classify_shadow_merge, find_merge_bases, find_single_merge_base,
    is_binary_buffer, merge_snapshots, merge_text_3way, merge_trees, validate_single_merge_base,
};
#[cfg(target_os = "linux")]
pub use overlay::{OverlayWorkspace, Swept, overlay_supported, overlay_supported_in, sweep_probes};
pub use process::{current_uid, undumpable};
pub use root::canonical_root;
#[cfg(any(test, feature = "testing"))]
pub use scratch::Scratch;
pub use transcripts::{LocalTranscriptArchive, TRANSCRIPT_DIRECTORY};
pub use watch::InotifyWatch;
pub use workspace::{
    BACKEND_MARKER, CAS_DIRECTORY, LocalWorkspaceStore, WorkspaceBackend, claim_root, root_backend,
};

/// The terminal host of this machine Spawn runs on: embedded `herdr-core`
/// (`spawn-hc`), legacy `herdr`, or pooled shards, told apart only here.
pub enum LocalTerminalHost {
    /// Legacy external `herdr` daemon host.
    Herdr(HerdrHost),
    /// Embedded `herdr-core` daemon host (`spawn-hc`, default).
    HerdrCore(HerdrCoreHost),
    /// Sharded pool of `herdr` daemon hosts.
    HerdrPool(HerdrPool),
}

impl LocalTerminalHost {
    /// The same host, reading, restoring and replacing every file below
    /// `projects` through it.
    #[must_use]
    pub fn with_files_beneath(self, projects: beneath::Beneath) -> Self {
        match self {
            Self::Herdr(host) => Self::Herdr(host.with_files_beneath(projects)),
            Self::HerdrCore(host) => Self::HerdrCore(host.with_files_beneath(projects)),
            Self::HerdrPool(pool) => Self::HerdrPool(pool.with_files_beneath(projects)),
        }
    }

    fn inner(&self) -> &dyn spawn_core::port::TerminalHost {
        match self {
            Self::Herdr(host) => host,
            Self::HerdrCore(host) => host,
            Self::HerdrPool(pool) => pool,
        }
    }
}

impl spawn_core::port::TerminalHost for LocalTerminalHost {
    fn start(&self, launch: &spawn_core::port::Launch) -> Result<(), TerminalError> {
        self.inner().start(launch)
    }

    fn stage_text(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
        text: &str,
    ) -> Result<(), TerminalError> {
        self.inner().stage_text(terminal, workspace, text)
    }

    fn submit_staged(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
    ) -> Result<(), TerminalError> {
        self.inner().submit_staged(terminal, workspace)
    }

    fn submit_text(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
        text: &str,
    ) -> Result<(), TerminalError> {
        self.inner().submit_text(terminal, workspace, text)
    }

    fn send_keys(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
        keys: &[&str],
    ) -> Result<(), TerminalError> {
        self.inner().send_keys(terminal, workspace, keys)
    }

    fn is_alive(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
    ) -> Result<bool, TerminalError> {
        self.inner().is_alive(terminal, workspace)
    }

    fn server_alive(&self) -> bool {
        self.inner().server_alive()
    }

    fn server(&self) -> spawn_core::port::ServerSeen {
        self.inner().server()
    }

    fn runner(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
    ) -> Result<spawn_core::port::Runner, TerminalError> {
        self.inner().runner(terminal, workspace)
    }

    fn runners(
        &self,
        terminals: &[(TerminalName, WorkspaceId)],
    ) -> Result<Vec<(TerminalName, spawn_core::port::Runner)>, TerminalError> {
        self.inner().runners(terminals)
    }

    fn capture_tail(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
        lines: usize,
    ) -> Result<String, TerminalError> {
        self.inner().capture_tail(terminal, workspace, lines)
    }

    fn read_from(
        &self,
        workspace: &WorkspaceId,
        path: &str,
        offset: u64,
    ) -> Result<Option<Vec<u8>>, TerminalError> {
        self.inner().read_from(workspace, path, offset)
    }

    fn list_dirs(&self, workspace: &WorkspaceId, dir: &str) -> Result<Vec<String>, TerminalError> {
        self.inner().list_dirs(workspace, dir)
    }

    fn write_new(
        &self,
        workspace: &WorkspaceId,
        path: &str,
        bytes: &[u8],
    ) -> Result<bool, TerminalError> {
        self.inner().write_new(workspace, path, bytes)
    }

    fn replace_grown(
        &self,
        workspace: &WorkspaceId,
        path: &str,
        bytes: &[u8],
        current: &[u8],
    ) -> Result<bool, TerminalError> {
        self.inner().replace_grown(workspace, path, bytes, current)
    }

    fn terminate(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
    ) -> Result<(), TerminalError> {
        self.inner().terminate(terminal, workspace)
    }

    fn terminate_waits(&self) -> bool {
        self.inner().terminate_waits()
    }

    fn agent(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
    ) -> Result<Option<spawn_core::port::AgentReading>, TerminalError> {
        self.inner().agent(terminal, workspace)
    }

    fn agent_changes(&self) -> Vec<TerminalName> {
        self.inner().agent_changes()
    }
}
