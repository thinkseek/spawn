//! LayerFS [`ArtifactStore`] port and `layerfs` CLI adapter.
//!
//! LayerFS publishes a JavaScript SDK and a native CLI. Spawn integrates at the
//! process boundary through documented single-line JSON output and keeps no
//! Node dependency.

// Pedantic doc-bookkeeping lints: public items carry purposeful docs, but
// mandatory Errors/Panics sections and identifier backticks add noise.
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::doc_markdown
)]
#![allow(clippy::module_name_repetitions, clippy::must_use_candidate)]
mod cli;
mod port;

pub use cli::LayerFsCli;
pub use port::{
    ArtifactError, ArtifactStore, BranchForkOutcome, CommitOutcome, CommitPage, CommitSummary,
    MergeOutcome, SandboxId, WorkspaceMount,
};
