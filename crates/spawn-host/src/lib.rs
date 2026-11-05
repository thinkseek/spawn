//! Host adapters: tmux terminals, the git workspace store and the local
//! chunk store, with its collection of unreferenced objects ([`gc`]).
//!
//! Both adapters call an external program and read its exit code, so they
//! share the same failure rules as the programs they wrap.

pub mod chunks;
pub mod gc;
pub mod git;
pub mod tmux;
pub mod tree_path;

pub use chunks::LocalChunkStore;
pub use git::{
    DEFAULT_CHUNK_DIRECTORY, GitWorkspace, LARGE_FILE_BYTES, container_name, docker_prefix,
    has_conflict_block,
};
pub use tmux::{PrefixBuilder, TmuxHost};
