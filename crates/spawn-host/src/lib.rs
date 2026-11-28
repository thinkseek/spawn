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
pub mod git_process;
pub mod herdr;
pub mod herdr_core;
pub mod herdr_global;
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
pub mod workspace_rules;

#[cfg(test)]
mod audit;

pub use archive_mask::CollectorMaskConfig;
pub use cas_dedup::{ArtifactRoot, DedupConfig, DedupError, DedupReport, dedup_dirs, prune_unused};
pub use cas_merkle::{
    CasMerkleStore, D6RefKind, D6StoreError, InMemoryRefLedger, LedgerRef, LedgerSnapshot,
    PublishRequest, PublishedSnapshot, RefLedger, RefsJournalLock, TrackedSetPolicy,
    format_commit_body,
};
pub use container::{BwrapCapability, ContainerEnv, detect_container, probe_bwrap};
pub use cow::{
    CloneKind, CowWorkspace, WorkspaceLease, clone_or_copy, cow_clone_supported, live_users,
    reflink_file,
};
pub use d6_cutover::{
    CutoverParityReport, CutoverPhase, GcFenceStatus, ProjectCutover, ProjectStorageMode,
    assert_zero_pending_merges, bind_store_gc_fence, check_gc_fence, is_readonly_border,
    reconcile_refs_journal, run_persistent_gc_for_ledger, verify_bundle_snapshot_roundtrip,
    verify_cutover_parity, verify_zero_pending_merges,
};
pub use d6_layer0::{
    ExistingDestPolicy, Layer0Options, audit_export_attributes, materialize_layer0,
    materialize_workspace_from_tree,
};
pub use d6_shadow::{
    ShadowCheckpointOutcome, ShadowD6Workspace, ShadowOracleMetrics, ShadowTrackedSetMode,
};
pub use d6_workspace::D6Workspace;
pub use engine_view::{
    BwrapView, ContainerView, ExtraMount, HomeIsolation, HostInheritMode, ViewConfig,
};
pub use git::GitWorkspace;
pub use git_border::{BorderRefTarget, GitBorderAdapter};
pub use git_process::git_command;
#[cfg(any(test, feature = "testing"))]
pub use git_process::test_git_command;
pub use herdr::{HerdrHost, HerdrPool};
pub use herdr_core::HerdrCoreHost;
pub use herdr_global::{HerdrDiscoveryEnv, HerdrGlobalClient};
pub use merkle_merge::{
    ConflictKind, ConflictLabels, MergeBaseError, MergeDriver, MergeOptions, MergeShadowOutcome,
    MergeSnapshotSpec, ShadowOracleSummary, build_merge_snapshot, classify_shadow_merge,
    merge_snapshots, merge_text_3way, merge_trees, validate_single_merge_base,
};
#[cfg(target_os = "linux")]
pub use overlay::{OverlayWorkspace, overlay_supported_in, sweep_probes};
pub use process::{current_uid, undumpable};
pub use root::canonical_root;
#[cfg(any(test, feature = "testing"))]
pub use scratch::Scratch;
pub use terminal_support::TerminalHostEnv;
pub use transcripts::{LocalTranscriptArchive, TRANSCRIPT_DIRECTORY};
pub use watch::InotifyWatch;
pub use workspace::{
    BACKEND_MARKER, CAS_DIRECTORY, LocalD6ShadowStore, LocalWorkspaceStore, WorkspaceBackend,
    claim_root, root_backend,
};
pub use workspace_rules::{
    DEFAULT_MAX_FILE_BYTES, ENGINE_RUNTIME_FILES, MAIN_COPIES, SET_ASIDE_FILES, SET_ASIDE_WAITS,
    has_conflict_block, has_conflict_block_sized, names_files_outside_git,
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

    fn list_dirs_up_to(
        &self,
        workspace: &WorkspaceId,
        dir: &str,
        limit: usize,
    ) -> Result<Vec<String>, TerminalError> {
        self.inner().list_dirs_up_to(workspace, dir, limit)
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

#[cfg(test)]
mod tests {
    use super::{HerdrCoreHost, LocalTerminalHost};
    use spawn_core::id::WorkspaceId;
    use spawn_core::port::{LIST_DIRS_LIMIT, TerminalHost};

    #[test]
    fn local_terminal_host_list_dirs_up_to_scales_past_64_preserving_newest_first() {
        let root =
            std::env::temp_dir().join(format!("spawn-host-list-dirs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create root");
        let base = std::time::UNIX_EPOCH + std::time::Duration::from_secs(10_000);
        for i in 0..120_u64 {
            let entry = root.join(format!("conv-{i:03}"));
            std::fs::create_dir(&entry).expect("create dir");
            let file = std::fs::File::open(&entry).expect("open dir");
            file.set_modified(base + std::time::Duration::from_secs(i))
                .expect("set mtime");
        }

        let host = LocalTerminalHost::HerdrCore(
            HerdrCoreHost::on_session(
                "spawn-hc",
                "spawn-hc-list-dirs",
                crate::TerminalHostEnv::default(),
            )
            .expect("herdr core host"),
        );
        let ws = WorkspaceId::new("local:/tmp").expect("workspace");
        let path = root.to_str().expect("utf8 path");

        let legacy = host.list_dirs(&ws, path).expect("legacy list_dirs");
        assert_eq!(legacy.len(), LIST_DIRS_LIMIT);
        assert_eq!(legacy.first().map(String::as_str), Some("conv-119"));

        let expanded = host
            .list_dirs_up_to(&ws, path, 4096)
            .expect("list_dirs_up_to 4096");
        assert_eq!(expanded.len(), 120);
        assert_eq!(expanded.first().map(String::as_str), Some("conv-119"));
        assert_eq!(expanded.last().map(String::as_str), Some("conv-000"));

        let _ = std::fs::remove_dir_all(&root);
    }
}
