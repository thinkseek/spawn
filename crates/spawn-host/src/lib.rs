//! Host adapters: tmux terminals and the git workspace store.
//!
//! Both adapters call an external program and read its exit code, so they
//! share the same failure rules as the programs they wrap.

pub mod git;
pub mod tmux;

pub use git::{GitWorkspace, LARGE_FILE_BYTES, container_name, docker_prefix, has_conflict_block};
pub use tmux::{PrefixBuilder, TmuxHost};
