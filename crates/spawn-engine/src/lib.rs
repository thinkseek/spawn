//! Native event normalization and launch-parameter construction.
//!
//! This crate is pure: it parses engine-specific payloads into the unified
//! [`EventKind`] vocabulary but never starts a process or touches storage.

// Pedantic doc-bookkeeping lints: public items carry purposeful docs, but
// mandatory Errors/Panics sections and identifier backticks add noise.
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::doc_markdown
)]
#![allow(clippy::module_name_repetitions, clippy::must_use_candidate)]
mod claude;
mod codex;
mod launch;

pub use claude::{ClaudeHookInput, normalize_claude_hook};
pub use codex::{CodexStreamEvent, normalize_codex_event};
pub use launch::{EngineLaunch, LaunchError, build_launch};

use spawn_types::event::EventKind;
use thiserror::Error;

/// One normalized native event before it is wrapped as a [`crate::NewEvent`].
#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedEvent {
    /// Unified event vocabulary entry.
    pub kind: EventKind,
    /// Stable native id used for idempotent appends.
    pub source_event_id: Option<String>,
}

/// Errors raised while parsing native payloads.
#[derive(Debug, Error)]
pub enum NormalizeError {
    /// The payload could not be parsed as JSON.
    #[error("invalid json payload: {0}")]
    Json(#[from] serde_json::Error),

    /// A required field was missing or empty.
    #[error("missing required field: {0}")]
    MissingField(&'static str),

    /// The native event name is not supported by the mapping table.
    #[error("unsupported native event: {0}")]
    UnsupportedEvent(String),
}
