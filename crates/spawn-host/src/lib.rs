//! Host adapters: tmux terminals and the LayerFS command line.
//!
//! Both adapters call an external program and read its exit code, so they
//! share the same failure rules as the programs they wrap.

pub mod layerfs;
pub mod tmux;

pub use layerfs::LayerFsCli;
pub use tmux::{PrefixBuilder, TmuxHost};
