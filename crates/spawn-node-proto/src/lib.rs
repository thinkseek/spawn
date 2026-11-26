//! spawn-node v1's wire protocol, shared by the gateway (cell 2) and every
//! Rust caller of the protocol; the Go daemon implements the same bytes
//! and both sides check them against the golden fixtures in
//! `testdata/node-protocol-v1/` (design §§2–3, §7).
//!
//! Pure data types, validation, strict JSON, target grammar and refusals live in
//! `spawn_core::node_proto`; content digests and workspace tree hashing live in
//! `spawn_cas`; this crate layers the Ed25519 signing/verification (`ring`),
//! strict base64url (`base64`), request envelope and response signatures on top.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod codec;
pub mod enroll;
pub mod envelope;
pub mod grant;
pub mod heartbeat;
pub mod keys;
pub mod response;

pub use spawn_cas::Digest;
pub use spawn_core::node_proto::{
    Error, Limit, Reaction, Refusal, VERSION, artifact_path, check_target, is_job_id, json,
    refusal, result, spec, strip_public_prefix,
};

pub use envelope::{Checked, Envelope, Method, NodeSigner, Verb, check_request};
pub use keys::{PublicKey, SigningKey};
