//! Terminal host port and tmux implementation.
//!
//! The port models named interactive sessions. The tmux adapter stays a thin
//! command builder and never interprets agent events.

// Pedantic doc-bookkeeping lints: public items carry purposeful docs, but
// mandatory Errors/Panics sections and identifier backticks add noise.
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::doc_markdown
)]
#![allow(clippy::module_name_repetitions, clippy::must_use_candidate)]
mod tmux;

pub use tmux::TmuxHost;

use spawn_process::ProcessError;
use spawn_types::identifiers::TerminalName;
use thiserror::Error;

/// Process launched inside a named terminal session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    /// Terminal session name.
    pub name: TerminalName,
    /// Program to execute.
    pub program: String,
    /// Program arguments.
    pub arguments: Vec<String>,
    /// Working directory for the process.
    pub working_directory: String,
    /// Optional prefix used to run inside a sandbox container.
    pub command_prefix: Vec<String>,
    /// Extra environment variables.
    pub environment: Vec<(String, String)>,
}

/// Snapshot of terminal pane contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneCapture {
    /// Visible pane text at capture time.
    pub text: String,
}

/// Errors raised by terminal operations.
#[derive(Debug, Error)]
pub enum TerminalError {
    /// The terminal command could not be executed.
    #[error("terminal process failure: {0}")]
    Process(#[from] ProcessError),
    /// The terminal returned an unexpected non-zero status.
    #[error("terminal command rejected: {0}")]
    Rejected(String),
}

/// Named interactive terminal lifecycle.
pub trait TerminalHost: Send + Sync {
    /// Start a detached named session.
    fn start(&self, spec: &LaunchSpec) -> Result<(), TerminalError>;

    /// Send keys as if typed by a user.
    fn send_keys(&self, name: &TerminalName, keys: &str, enter: bool) -> Result<(), TerminalError>;

    /// Capture the visible pane text.
    fn capture(&self, name: &TerminalName) -> Result<PaneCapture, TerminalError>;

    /// Report whether the session still exists.
    fn is_alive(&self, name: &TerminalName) -> Result<bool, TerminalError>;

    /// Terminate the session and its process group.
    fn terminate(&self, name: &TerminalName) -> Result<(), TerminalError>;
}
