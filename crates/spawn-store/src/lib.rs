//! Storage ports and an in-memory implementation.
//!
//! The ports define durability and leasing contracts. The in-memory store is a
//! first-class implementation used by local runs and public API tests; it
//! enforces the same invariants as the PostgreSQL adapter.

// Pedantic doc-bookkeeping lints: public items carry purposeful docs, but
// mandatory Errors/Panics sections and identifier backticks add noise.
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::doc_markdown
)]
#![allow(clippy::module_name_repetitions, clippy::must_use_candidate)]
mod memory;
mod ports;

pub use memory::InMemoryStore;
pub use ports::{CommandQueue, EventLog, StoreError};
