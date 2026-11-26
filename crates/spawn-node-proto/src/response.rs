//! The response signature (section 2.3): every response the gateway itself
//! produces, success or refusal, carries `Spawn-Primary-Key` and
//! `Spawn-Primary-Sig` over seven lines that bind the request's
//! `(node_id, boot_id, seq)`. The daemon believes nothing unsigned; artifact
//! bodies are the one exception (their content address verifies them).

use serde::{Deserialize, Serialize};

use crate::codec::sha256_hex;
use crate::keys::{PublicKey, SigningKey, decode_signature};
use crate::refusal::Refusal;
use crate::{Envelope, Error, VERSION, json};

/// The first line of the response signing input.
pub const RESPONSE_PREFIX: &str = "spawn-node-resp-v1";
/// Header carrying the primary gateway key id (lowercase hex SHA-256).
pub const HEADER_PRIMARY_KEY: &str = "Spawn-Primary-Key";
/// Header carrying the base64url primary Ed25519 signature.
pub const HEADER_PRIMARY_SIG: &str = "Spawn-Primary-Sig";

/// The request a response answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestId<'a> {
    /// 64-char hex node identifier bound to this request.
    pub node_id: &'a str,
    /// 32-char lowercase hex boot identifier bound to this request.
    pub boot_id: &'a str,
    /// Request sequence number bound to this request.
    pub seq: u64,
}

impl Envelope {
    /// The `(node_id, boot_id, seq)` a response to this request binds.
    pub fn id(&self) -> RequestId<'_> {
        RequestId {
            node_id: &self.node_id,
            boot_id: &self.boot_id,
            seq: self.seq,
        }
    }
}

/// The seven signed lines of a response, each ended by `\n`.
pub fn response_signing_input(
    status: u16,
    request: RequestId<'_>,
    server_time_ms: u64,
    body: &[u8],
) -> Vec<u8> {
    format!(
        "{RESPONSE_PREFIX}\n{status}\n{}\n{}\n{}\n{server_time_ms}\n{}\n",
        request.node_id,
        request.boot_id,
        request.seq,
        sha256_hex(body)
    )
    .into_bytes()
}

/// The two response headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseSignature {
    /// `Spawn-Primary-Key`: the primary's key id.
    pub primary_key: String,
    /// `Spawn-Primary-Sig`: base64url.
    pub sig: String,
}

impl ResponseSignature {
    /// Returns both response signature headers in canonical pair order.
    pub fn to_headers(&self) -> [(&'static str, String); 2] {
        [
            (HEADER_PRIMARY_KEY, self.primary_key.clone()),
            (HEADER_PRIMARY_SIG, self.sig.clone()),
        ]
    }
}

/// The gateway's signature of a response. `server_time_ms` must be the
/// value the body carries.
pub fn sign_response(
    primary: &SigningKey,
    status: u16,
    request: RequestId<'_>,
    server_time_ms: u64,
    body: &[u8],
) -> ResponseSignature {
    ResponseSignature {
        primary_key: primary.public_key().key_id(),
        sig: primary.sign_b64u(&response_signing_input(
            status,
            request,
            server_time_ms,
            body,
        )),
    }
}

#[derive(Deserialize)]
struct ServerTime {
    server_time_ms: u64,
}

/// The daemon's check of a response: the key id is the pinned primary's,
/// the body is strict JSON with a `server_time_ms`, and the signature over
/// the seven lines verifies. Returns `server_time_ms`, the only clock a
/// node adapts to. Any failure is a transport failure (no state change).
pub fn verify_response(
    primary: &PublicKey,
    status: u16,
    request: RequestId<'_>,
    primary_key_header: &str,
    sig_header: &str,
    body: &[u8],
) -> Result<u64, Error> {
    if primary_key_header != primary.key_id() {
        return Err(Error::Signature);
    }
    let sig = decode_signature(sig_header)?;
    let ServerTime { server_time_ms } = json::parse(body)?;
    primary.verify(
        &response_signing_input(status, request, server_time_ms, body),
        &sig,
    )?;
    Ok(server_time_ms)
}

/// The body of every refusal: `{"v":1,"error":"…","server_time_ms":N}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefusalBody {
    /// Protocol version (`1`).
    pub v: u64,
    /// Wire error string mapped from [`Refusal`].
    pub error: String,
    /// Gateway clock timestamp in Unix milliseconds.
    pub server_time_ms: u64,
}

impl RefusalBody {
    /// Builds a [`RefusalBody`] from a typed [`Refusal`] and `server_time_ms`.
    pub fn new(refusal: Refusal, server_time_ms: u64) -> Self {
        Self {
            v: VERSION,
            error: refusal.error().to_string(),
            server_time_ms,
        }
    }

    /// The exact bytes the gateway sends.
    pub fn to_bytes(&self) -> Vec<u8> {
        json::to_vec(self)
    }

    /// The refusal this body and `status` name; `None` for a pair v1 does
    /// not have.
    pub fn refusal(&self, status: u16) -> Option<Refusal> {
        Refusal::from_wire(status, &self.error)
    }
}
