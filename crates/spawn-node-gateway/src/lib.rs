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

pub mod artifacts;
pub mod cache;
pub mod config;
pub mod gateway;
pub mod http;
pub mod limits;
pub mod primary;
pub mod results;
pub mod span;

pub use config::Config;
pub use gateway::{Clock, Connector, Gateway, ResultAnswer, Rid, Settings, SpecAnswer, spawn, system_clock};
