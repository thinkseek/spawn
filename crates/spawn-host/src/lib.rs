//! Host adapters: tmux terminals, the git workspace store, the local chunk
//! store, with its collection of unreferenced objects ([`gc`]), and the
//! local transcript archive.
//!
//! The tmux and git adapters call an external program and read its exit
//! code, so they share the same failure rules as the programs they wrap.

pub mod chunks;
pub mod gc;
pub mod git;
pub mod tmux;
pub mod transcripts;
pub mod tree_path;

#[cfg(test)]
mod audit_repro;

pub use chunks::LocalChunkStore;
pub use git::{
    DEFAULT_CHUNK_DIRECTORY, GitWorkspace, LARGE_FILE_BYTES, has_conflict_block,
    has_conflict_block_sized,
};
pub use tmux::TmuxHost;
pub use transcripts::{LocalTranscriptArchive, TRANSCRIPT_DIRECTORY};
