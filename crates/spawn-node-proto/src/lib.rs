//! spawn-node v1's wire protocol, shared by the gateway (cell 2) and every
//! Rust caller of the protocol; the Go daemon implements the same bytes
//! and both sides check them against the golden fixtures in
//! `testdata/node-protocol-v1/` (design §§2–3, §7).
//!
//! - [`envelope`]: the request target's grammar, the five `Spawn-Node-*`
//!   headers, the eight-line signing input, the verbs and their caps;
//! - [`response`]: the seven-line response signature and the refusal body;
//! - [`refusal`]: section 2.4's statuses, `error` codes and reactions;
//! - [`heartbeat`], [`spec`], [`grant`], [`result`], [`artifact`],
//!   [`enroll`]: the verbs' typed bodies, which write the fixtures' exact
//!   bytes back;
//! - [`tree`]: `tree_sha256_v1`, the digest of `scenarios_sha256` and of
//!   the cargo registry;
//! - [`json`], [`codec`], [`keys`]: strict JSON, the scalar encodings and
//!   Ed25519 (ring).
//!
//! No JSON is canonicalised anywhere: every signature covers the bytes as
//! sent, and a receiver hashes the bytes it received and parses those same
//! bytes afterwards.

pub mod artifact;
pub mod codec;
pub mod enroll;
pub mod envelope;
pub mod grant;
pub mod heartbeat;
pub mod json;
pub mod keys;
pub mod refusal;
pub mod response;
pub mod result;
pub mod spec;
pub mod tree;

pub use artifact::ContentDigest;
pub use envelope::{Checked, Envelope, Method, NodeSigner, Verb, check_request, check_target};
pub use keys::{PublicKey, SigningKey};
pub use refusal::{Reaction, Refusal};
pub use tree::{TREE_SKIP_REGISTRY, registry_sha256, scenarios_sha256, tree_sha256_v1};

/// The protocol version every body carries as `"v"`.
pub const VERSION: u64 = 1;

/// The size caps of the protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// A heartbeat body: hard cap 4 KB.
    Beat,
    /// A spec body: 64 KB.
    Spec,
    /// An enroll body: a spec plus the token and the key.
    Enroll,
    /// A GET carries no body.
    GetBody,
    /// A grant's JSON bytes: under 8 KB.
    Grant,
    /// A result's metadata line: 256 KiB.
    ResultMeta,
    /// A result's log: 32 MiB.
    ResultLog,
    /// A result's `verdict_raw`: 64 KiB.
    Verdict,
}

impl Limit {
    /// The largest size allowed, in bytes.
    pub const fn bytes(self) -> usize {
        match self {
            Self::Beat => 4096,
            Self::Spec | Self::Verdict => 64 * 1024,
            Self::Enroll => 64 * 1024 + 1024,
            Self::GetBody => 0,
            Self::Grant => 8 * 1024,
            Self::ResultMeta => 256 * 1024,
            Self::ResultLog => 32 * 1024 * 1024,
        }
    }
}

/// Why the protocol refused some bytes. [`Error::refusal`] gives the
/// answer the gateway sends for it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("not strict unpadded base64url")]
    Base64,
    #[error("not a canonical decimal below 2^53")]
    Decimal,
    #[error("json: {0}")]
    Json(&'static str),
    #[error("json shape: {0}")]
    Shape(String),
    #[error("bad request target")]
    Target,
    #[error("bad method")]
    Method,
    #[error("bad header {0}")]
    Header(&'static str),
    #[error("missing header {0}")]
    MissingHeader(&'static str),
    #[error("{size} bytes, over the {limit:?} cap")]
    TooLarge { limit: Limit, size: usize },
    #[error("bad body: {0}")]
    Body(&'static str),
    #[error("signature does not verify")]
    Signature,
    #[error("bad Ed25519 key")]
    Key,
    #[error("grant: {0}")]
    Grant(&'static str),
    #[error("grant env names {0}, which the node sets itself")]
    ForbiddenEnv(String),
    #[error("bad content digest")]
    Digest,
    #[error("bytes do not hash to the content digest")]
    DigestMismatch,
    #[error("bad enroll token")]
    Token,
    #[error("primary key does not match the token's pin")]
    PinMismatch,
    #[error("log does not match log_bytes / log_sha256")]
    LogMismatch,
}

impl Error {
    /// The refusal the gateway answers for this error.
    pub fn refusal(&self) -> Refusal {
        match self {
            Self::Signature | Self::Key => Refusal::BadSignature,
            Self::TooLarge {
                limit: Limit::ResultMeta | Limit::ResultLog,
                ..
            } => Refusal::TooLarge,
            _ => Refusal::BadRequest,
        }
    }
}
