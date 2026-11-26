//! Pure data model, validation and wire codecs for `spawn-node` v1.
//!
//! These types carry no transport or cryptographic-signing dependencies (`ring`,
//! `rustls`, `base64`), so `spawn-core` and control-plane callers such as
//! `spawn-cli`'s `integrate` pipeline can parse, inspect and validate grants,
//! specs, results, refusals and targets without pulling TLS or key libraries
//! into their dependency closure.

pub mod codec;
pub mod grant;
pub mod json;
pub mod refusal;
pub mod result;
pub mod spec;
pub mod target;

pub use refusal::{Reaction, Refusal};
pub use target::{artifact_path, check_target, is_job_id, strip_public_prefix};

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
    /// Field was not valid unpadded base64url.
    #[error("not strict unpadded base64url")]
    Base64,
    /// Integer encoding was not canonical decimal strictly below `2^53`.
    #[error("not a canonical decimal below 2^53")]
    Decimal,
    /// Malformed strict JSON syntax or unsupported literal (`{0}` details the failure).
    #[error("json: {0}")]
    Json(&'static str),
    /// Parsed JSON did not match the expected object/type shape.
    #[error("json shape: {0}")]
    Shape(String),
    /// Request target path did not match any `spawn-node` v1 route grammar.
    #[error("bad request target")]
    Target,
    /// HTTP method is not valid for the route.
    #[error("bad method")]
    Method,
    /// Header value failed syntax or bounds validation.
    #[error("bad header {0}")]
    Header(&'static str),
    /// Required protocol header was omitted.
    #[error("missing header {0}")]
    MissingHeader(&'static str),
    /// Body exceeded the [`Limit`] cap for this message kind.
    #[error("{size} bytes, over the {limit:?} cap")]
    TooLarge {
        /// Which protocol limit was exceeded.
        limit: Limit,
        /// Actual byte length observed.
        size: usize,
    },
    /// Body payload failed schema or semantic validation.
    #[error("bad body: {0}")]
    Body(&'static str),
    /// Ed25519 request or grant signature failed verification.
    #[error("signature does not verify")]
    Signature,
    /// Public or signing key was malformed or not on the expected curve.
    #[error("bad Ed25519 key")]
    Key,
    /// Grant semantic check failed.
    #[error("grant: {0}")]
    Grant(&'static str),
    /// Grant `env` attempted to set a node-reserved variable name.
    #[error("grant env names {0}, which the node sets itself")]
    ForbiddenEnv(String),
    /// Content digest string did not match `sha256:<64 lowercase hex>`.
    #[error("bad content digest")]
    Digest,
    /// Artifact or payload bytes did not match the expected digest.
    #[error("bytes do not hash to the content digest")]
    DigestMismatch,
    /// Enrollment token was malformed or unparseable.
    #[error("bad enroll token")]
    Token,
    /// Node primary key did not match the enrollment token's pin.
    #[error("primary key does not match the token's pin")]
    PinMismatch,
    /// Uploaded result log bytes or digest mismatched metadata `log_bytes` / `log_sha256`.
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
