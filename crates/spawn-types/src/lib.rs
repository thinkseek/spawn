//! Domain algebraic data types for Spawn.
//!
//! This crate has no IO dependency. Every identifier and bounded value is a
//! validated newtype. Construction failures return [`ValidationError`].

// Pedantic doc-bookkeeping lints: public items carry purposeful docs, but
// mandatory Errors/Panics sections and identifier backticks add noise.
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::doc_markdown
)]
#![allow(clippy::module_name_repetitions, clippy::must_use_candidate)]
pub mod clock;
pub mod command;
pub mod engine;
pub mod error;
pub mod event;
pub mod identifiers;
pub mod observation;

pub use clock::{Clock, DeterministicClock, SystemClock, Timestamp};
pub use command::{
    CommandAction, CommandId, CommandRecord, CommandStatus, IdempotencyKey, Lease, LeaseAttempt,
    Principal,
};
pub use engine::{Engine, EngineKind};
pub use error::{SpawnError, ValidationError};
pub use event::{
    AppendOutcome, ArtifactRef, EventId, EventKind, EventRecord, EventSource, GlobalPosition,
    NewEvent, SessionSeq,
};
pub use identifiers::{
    AgentId, BranchName, CommitId, ExecutionId, IdGenerator, MountId, SessionId, SystemIdGenerator,
    TerminalName, current_process_tag, validate_opaque_label,
};
pub use observation::{Confidence, Observation, SemanticState, WaitReason};
