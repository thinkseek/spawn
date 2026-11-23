//! Host adapters: tmux terminals, the git workspace store, the opt-in
//! copy-on-write workspace store and artifact deduplicator, and the local
//! transcript archive.
//!
//! The tmux and git adapters call an external program and read its exit
//! code, so they share the same failure rules as the programs they wrap.
//!
//! What either program answers is read the same on every host, whatever its
//! locale. git runs in the C locale ([`git_command`]). tmux is given none:
//! it has no translations (its messages and the system errors it quotes
//! are English under any `LANG`), and a locale given to the command that
//! starts its server would reach every pane, the engine's included.

pub mod agent_files;
pub mod archive_mask;
pub mod beneath;
pub mod cas_dedup;
pub mod cow;
pub mod git;
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
    SET_ASIDE_FILES, git_command, has_conflict_block, has_conflict_block_sized, max_file_bytes,
    names_files_outside_git,
};
#[cfg(target_os = "linux")]
pub use overlay::{OverlayWorkspace, overlay_supported, overlay_supported_in};
pub use process::{current_uid, undumpable};
pub use root::canonical_root;
#[cfg(any(test, feature = "testing"))]
pub use scratch::Scratch;
pub use tmux::TmuxHost;
pub use transcripts::{LocalTranscriptArchive, TRANSCRIPT_DIRECTORY};
pub use watch::InotifyWatch;
