//! spawn-node v1's gateway (design §2, docs/node-protocol-v1.md): the only
//! door between a node and the ledger. It checks every request's shape,
//! pace, clock and Ed25519 signature, calls one of the four
//! `spawn_node_gw_*` functions as the restricted `spawn_node_gateway`
//! login (verify-then-commit), streams results to disk, serves artifacts,
//! and signs everything it answers with the primary's key.
//!
//! Synchronous throughout: a std-only HTTP/1.1 server ([`http`]) with a
//! fixed pool of worker threads, each holding its own PostgreSQL
//! connection. No async runtime, no unsafe code.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// Content-addressed artifact verification and disk streaming.
pub mod artifacts;
/// In-memory node public-key cache with TTL and capacity eviction.
pub mod cache;
/// Environment and CLI configuration parsing.
pub mod config;
/// Request validation, signature checks, and PostgreSQL gateway handlers.
pub mod gateway;
/// Synchronous HTTP/1.1 listener and connection loop.
pub mod http;
/// Request byte/rate limits and per-node concurrency guards.
pub mod limits;
/// Primary Ed25519 signing key loading and creation.
pub mod primary;
/// Result log and metadata storage helpers.
pub mod results;
/// Bounded span/size parsing helpers.
pub mod span;

pub use config::Config;
pub use gateway::{
    Clock, Connector, Gateway, ResultAnswer, Rid, Settings, SpecAnswer, spawn, spawn_with,
    system_clock,
};
