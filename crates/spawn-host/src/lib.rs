//! Host adapters: tmux terminals, the git workspace store and the local
//! transcript archive.
//!
//! The tmux and git adapters call an external program and read its exit
//! code, so they share the same failure rules as the programs they wrap.

pub mod git;
pub mod tmux;
pub mod transcripts;
pub mod tree_path;

#[cfg(test)]
mod audit_repro;

pub use git::{
    DEFAULT_MAX_FILE_MB, GitWorkspace, LARGE_FILE_TRAILER, has_conflict_block,
    has_conflict_block_sized, max_file_bytes, names_files_outside_git,
};
pub use tmux::TmuxHost;
pub use transcripts::{LocalTranscriptArchive, TRANSCRIPT_DIRECTORY};
