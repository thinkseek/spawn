//! Host adapters: tmux terminals, the git workspace store and the local
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
pub mod git;
pub mod root;
pub mod tmux;
pub mod transcripts;
pub mod tree_path;

#[cfg(test)]
mod audit;

pub use git::{
    DEFAULT_MAX_FILE_MB, ENGINE_RUNTIME_FILES, GitWorkspace, LARGE_FILE_TRAILER, MAIN_COPIES,
    git_command, has_conflict_block, has_conflict_block_sized, max_file_bytes,
    names_files_outside_git,
};
pub use root::canonical_root;
pub use tmux::TmuxHost;
pub use transcripts::{LocalTranscriptArchive, TRANSCRIPT_DIRECTORY};
