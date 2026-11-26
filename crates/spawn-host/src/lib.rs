//! Host adapters: tmux terminals, the workspace stores (Git, the opt-in
//! copy-on-write and overlay stores, behind one [`LocalWorkspaceStore`]),
//! the artifact deduplicator, the local transcript archive, and the
//! filesystem view (`bwrap`) an engine without deny rules runs in.
//!
//! The tmux and git adapters call an external program and read its exit
//! code, so they share the same failure rules as the programs they wrap.
//!
//! What either program answers is read the same on every host, whatever its
//! locale. git runs in the C locale ([`git_command`]). tmux is given none:
//! it has no translations (its messages and the system errors it quotes
//! are English under any `LANG`), and a locale given to the command that
//! starts its server would reach every pane, the engine's included.

use spawn_core::id::{TerminalName, WorkspaceId};
use spawn_core::port::TerminalError;

pub mod agent_files;
pub mod archive_mask;
pub mod beneath;
pub mod cas_dedup;
pub mod cow;
pub mod engine_view;
pub mod git;
pub mod herdr;
pub mod home;
#[cfg(target_os = "linux")]
pub mod overlay;
pub mod process;
pub mod root;
#[cfg(any(test, feature = "testing"))]
pub mod scratch;
pub mod tmux;
pub mod transcripts;
pub mod tree_path;
pub mod watch;
pub mod workspace;

#[cfg(test)]
mod audit;

pub use cas_dedup::{
    ArtifactRoot, DedupConfig, DedupError, DedupReport, PruneReport, dedup_dirs, prune_unused,
};
pub use cow::{
    CloneKind, CowStats, CowWorkspace, DirectoryOccupancy, WorkspaceLease, clone_or_copy,
    cow_clone_supported, directory_occupancy, live_users, reflink_file,
};
#[cfg(any(test, feature = "testing"))]
pub use git::test_git_command;
pub use git::{
    DEFAULT_MAX_FILE_MB, ENGINE_RUNTIME_FILES, GitWorkspace, LARGE_FILE_TRAILER, MAIN_COPIES,
    SET_ASIDE_FILES, SET_ASIDE_WAITS, git_command, has_conflict_block, has_conflict_block_sized,
    max_file_bytes, names_files_outside_git,
};
pub use engine_view::{BwrapView, ViewConfig};
pub use herdr::HerdrHost;
#[cfg(target_os = "linux")]
pub use overlay::{OverlayWorkspace, Swept, overlay_supported, overlay_supported_in, sweep_probes};
pub use process::{current_uid, undumpable};
pub use root::canonical_root;
#[cfg(any(test, feature = "testing"))]
pub use scratch::Scratch;
pub use tmux::TmuxHost;
pub use transcripts::{LocalTranscriptArchive, TRANSCRIPT_DIRECTORY};
pub use watch::InotifyWatch;
pub use workspace::{
    BACKEND_MARKER, CAS_DIRECTORY, LocalWorkspaceStore, WorkspaceBackend, claim_root, root_backend,
};

/// The terminal host of this machine Spawn runs on: tmux (the default) or
/// Herdr (`SPAWN_TERMINAL_BACKEND=herdr`), told apart only here.
pub enum LocalTerminalHost {
    Tmux(TmuxHost),
    Herdr(HerdrHost),
}

impl LocalTerminalHost {
    /// The same host, reading, restoring and replacing every file below
    /// `projects` through it ([`TmuxHost::with_files_beneath`]).
    #[must_use]
    pub fn with_files_beneath(self, projects: beneath::Beneath) -> Self {
        match self {
            Self::Tmux(host) => Self::Tmux(host.with_files_beneath(projects)),
            Self::Herdr(host) => Self::Herdr(host.with_files_beneath(projects)),
        }
    }

    fn inner(&self) -> &dyn spawn_core::port::TerminalHost {
        match self {
            Self::Tmux(host) => host,
            Self::Herdr(host) => host,
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
