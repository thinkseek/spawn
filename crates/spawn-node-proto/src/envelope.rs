//! The request envelope (section 2.2): the target's grammar, the five
//! `Spawn-Node-*` headers, the eight-line signing input, the verbs and
//! their body caps. Everything structural is refused (`400 bad_request`)
//! before any signature is checked or any body parsed.

use crate::codec::{is_lower_hex, parse_decimal, sha256_hex};
use crate::keys::{PublicKey, SigningKey, decode_signature};
use crate::{Error, Limit, json, result};
use spawn_cas::Digest;

/// The first line of the request signing input.
pub const REQUEST_PREFIX: &str = "spawn-node-req-v1";
/// Every target starts with this.
pub use spawn_core::node_proto::target::{
    MAX_PATH_TAIL, MAX_QUERY, PATH_PREFIX, check_target, is_job_id, strip_public_prefix,
};
/// The largest `|ts_ms − server_time_ms|` the gateway takes (`409 clock_skew`).
pub const MAX_SKEW_MS: u64 = 120_000;

/// Header carrying the 64-char hex SHA-256 of the node public key.
pub const HEADER_NODE_ID: &str = "Spawn-Node-Id";
/// Header carrying the 32-char lowercase hex boot identifier.
pub const HEADER_BOOT: &str = "Spawn-Node-Boot";
/// Header carrying the strictly increasing per-boot sequence number.
pub const HEADER_SEQ: &str = "Spawn-Node-Seq";
/// Header carrying the node request timestamp in Unix milliseconds.
pub const HEADER_TS: &str = "Spawn-Node-Ts";
/// Header carrying the base64url-encoded 64-byte Ed25519 signature.
pub const HEADER_SIG: &str = "Spawn-Node-Sig";
/// The five request headers, in the order a client writes them.
pub const REQUEST_HEADERS: [&str; 5] = [
    HEADER_NODE_ID,
    HEADER_BOOT,
    HEADER_SEQ,
    HEADER_TS,
    HEADER_SIG,
];

/// The two methods v1 has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// HTTP `GET`.
    Get,
    /// HTTP `POST`.
    Post,
}

impl Method {
    /// Exactly `GET` or `POST`, upper case (rule 2).
    pub fn parse(text: &str) -> Result<Self, Error> {
        match text {
            "GET" => Ok(Self::Get),
            "POST" => Ok(Self::Post),
            _ => Err(Error::Method),
        }
    }

    /// Returns the uppercase HTTP method name (`GET` or `POST`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
        }
    }
}

/// What a request asks for, from its method and target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verb {
    /// `POST /node/v1/enroll` (section 2.5), seq 0.
    Enroll,
    /// `POST /node/v1/heartbeat`.
    Heartbeat,
    /// `POST /node/v1/spec`.
    Spec,
    /// `GET /node/v1/artifacts/<algo:hex>`.
    Artifact(Digest),
    /// `GET /node/v1/registry-manifest`.
    RegistryManifest,
    /// `POST /node/v1/jobs/<job_id>/result`.
    Result {
        /// Job identifier whose execution result is being reported.
        job_id: String,
    },
}

impl Verb {
    /// The verb of a checked target ([`check_target`] first); an unknown
    /// route or the wrong method is a bad request.
    pub fn route(method: Method, target: &str) -> Result<Self, Error> {
        check_target(target)?;
        let path = target.split_once('?').map_or(target, |(path, _)| path);
        let tail = &path[PATH_PREFIX.len()..];
        let verb = match tail {
            "enroll" => Self::Enroll,
            "heartbeat" => Self::Heartbeat,
            "spec" => Self::Spec,
            "registry-manifest" => Self::RegistryManifest,
            _ => {
                if let Some(digest) = tail.strip_prefix("artifacts/") {
                    Self::Artifact(Digest::parse(digest).map_err(|_| Error::Target)?)
                } else if let Some(job_id) = tail
                    .strip_prefix("jobs/")
                    .and_then(|rest| rest.strip_suffix("/result"))
                    .filter(|job_id| is_job_id(job_id))
                {
                    Self::Result {
                        job_id: job_id.to_string(),
                    }
                } else {
                    return Err(Error::Target);
                }
            }
        };
        if verb.method() == method {
            Ok(verb)
        } else {
            Err(Error::Method)
        }
    }

    /// Returns the HTTP method required by this verb.
    pub fn method(&self) -> Method {
        match self {
            Self::Artifact(_) | Self::RegistryManifest => Method::Get,
            _ => Method::Post,
        }
    }

    /// The largest body this verb may carry (section 3's table).
    pub fn body_cap(&self) -> usize {
        match self {
            Self::Enroll => Limit::Enroll.bytes(),
            Self::Heartbeat => Limit::Beat.bytes(),
            Self::Spec => Limit::Spec.bytes(),
            Self::Artifact(_) | Self::RegistryManifest => 0,
            Self::Result { .. } => Limit::ResultMeta.bytes() + 1 + Limit::ResultLog.bytes(),
        }
    }
}

/// The five header values of one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// 64 lowercase hex: SHA-256 of the node's raw public key.
    pub node_id: String,
    /// 32 lowercase hex.
    pub boot_id: String,
    /// 1.., strictly increasing within a boot; 0 for enroll.
    pub seq: u64,
    /// Unix ms, the node's clock plus its learned offset.
    pub ts_ms: u64,
    /// The raw 64-byte signature.
    pub sig: Vec<u8>,
}

impl Envelope {
    /// The envelope from its headers; `header` looks a name up
    /// (case-insensitively, as HTTP does) and gives its single value.
    pub fn from_headers<'h>(header: impl Fn(&str) -> Option<&'h str>) -> Result<Self, Error> {
        let get = |name: &'static str| header(name).ok_or(Error::MissingHeader(name));
        let node_id = get(HEADER_NODE_ID)?;
        let boot_id = get(HEADER_BOOT)?;
        if !is_lower_hex(node_id, 64) {
            return Err(Error::Header(HEADER_NODE_ID));
        }
        if !is_lower_hex(boot_id, 32) {
            return Err(Error::Header(HEADER_BOOT));
        }
        let seq = parse_decimal(get(HEADER_SEQ)?).map_err(|_| Error::Header(HEADER_SEQ))?;
        let ts_ms = parse_decimal(get(HEADER_TS)?).map_err(|_| Error::Header(HEADER_TS))?;
        let sig = decode_signature(get(HEADER_SIG)?)?;
        Ok(Self {
            node_id: node_id.to_string(),
            boot_id: boot_id.to_string(),
            seq,
            ts_ms,
            sig,
        })
    }

    /// The headers to send, in [`REQUEST_HEADERS`] order.
    pub fn to_headers(&self) -> [(&'static str, String); 5] {
        [
            (HEADER_NODE_ID, self.node_id.clone()),
            (HEADER_BOOT, self.boot_id.clone()),
            (HEADER_SEQ, self.seq.to_string()),
            (HEADER_TS, self.ts_ms.to_string()),
            (HEADER_SIG, crate::codec::b64u(&self.sig)),
        ]
    }

    /// The eight-line signing input of this envelope over `signed_body`.
    pub fn signing_input(&self, method: Method, target: &str, signed_body: &[u8]) -> Vec<u8> {
        request_signing_input(
            method,
            target,
            &self.node_id,
            &self.boot_id,
            self.seq,
            self.ts_ms,
            signed_body,
        )
    }

    /// Verifies the signature against `key`, which must also be the key
    /// `node_id` names.
    pub fn verify(
        &self,
        key: &PublicKey,
        method: Method,
        target: &str,
        signed_body: &[u8],
    ) -> Result<(), Error> {
        if key.key_id() != self.node_id {
            return Err(Error::Signature);
        }
        key.verify(&self.signing_input(method, target, signed_body), &self.sig)
    }
}

/// The signed bytes of a request: eight ASCII lines, each ended by `\n`.
pub fn request_signing_input(
    method: Method,
    target: &str,
    node_id: &str,
    boot_id: &str,
    seq: u64,
    ts_ms: u64,
    signed_body: &[u8],
) -> Vec<u8> {
    format!(
        "{REQUEST_PREFIX}\n{}\n{target}\n{node_id}\n{boot_id}\n{seq}\n{ts_ms}\n{}\n",
        method.as_str(),
        sha256_hex(signed_body)
    )
    .into_bytes()
}

/// A node's identity for one boot: its key and the boot id.
#[derive(Debug)]
pub struct NodeSigner {
    /// Signing key pair for this node.
    pub key: SigningKey,
    /// Lowercase hex SHA-256 node identifier derived from [`SigningKey`].
    pub node_id: String,
    /// 32-char lowercase hex identifier for the current boot.
    pub boot_id: String,
}

impl NodeSigner {
    /// Builds a [`NodeSigner`] after verifying `boot_id` is 32 lowercase hex digits.
    pub fn new(key: SigningKey, boot_id: &str) -> Result<Self, Error> {
        if !is_lower_hex(boot_id, 32) {
            return Err(Error::Header(HEADER_BOOT));
        }
        let node_id = key.public_key().key_id();
        Ok(Self {
            key,
            node_id,
            boot_id: boot_id.to_string(),
        })
    }

    /// The envelope of one request. `signed_body` is the whole body, or the
    /// metadata line of a result (rule 5).
    pub fn sign(
        &self,
        method: Method,
        target: &str,
        seq: u64,
        ts_ms: u64,
        signed_body: &[u8],
    ) -> Result<Envelope, Error> {
        check_target(target)?;
        let mut envelope = Envelope {
            node_id: self.node_id.clone(),
            boot_id: self.boot_id.clone(),
            seq,
            ts_ms,
            sig: Vec::new(),
        };
        envelope.sig = self
            .key
            .sign(&envelope.signing_input(method, target, signed_body))
            .to_vec();
        Ok(envelope)
    }
}

/// A request that passed every structural check: target, method, headers,
/// size cap, strict JSON. Its signature is not checked yet
/// ([`Checked::verify`]), because the key comes from the ledger (or, for
/// enroll, from the body).
#[derive(Debug)]
pub struct Checked<'a> {
    /// Validated HTTP method.
    pub method: Method,
    /// Validated request path and optional query string.
    pub target: &'a str,
    /// Routed protocol verb for this request.
    pub verb: Verb,
    /// Parsed request authentication headers.
    pub envelope: Envelope,
    /// The bytes the signature covers.
    pub signed_body: &'a [u8],
    /// A result's log bytes; empty for every other verb.
    pub log: &'a [u8],
}

impl Checked<'_> {
    /// Verifies the envelope's signature with `key`.
    pub fn verify(&self, key: &PublicKey) -> Result<(), Error> {
        self.envelope
            .verify(key, self.method, self.target, self.signed_body)
    }
}

/// Every check the gateway makes before any work: the target's grammar,
/// the route and method, the five headers, `seq` 0 for enroll only, the
/// verb's size cap, an empty GET body, strict JSON for every JSON body (a
/// result's metadata line). A failure here is `400 bad_request`, or
/// `413 too_large` for a result over its caps ([`Error::refusal`]).
pub fn check_request<'a, 'h>(
    method: &str,
    target: &'a str,
    header: impl Fn(&str) -> Option<&'h str>,
    body: &'a [u8],
) -> Result<Checked<'a>, Error> {
    let method = Method::parse(method)?;
    let verb = Verb::route(method, target)?;
    let envelope = Envelope::from_headers(header)?;
    if (verb == Verb::Enroll) != (envelope.seq == 0) {
        return Err(Error::Header(HEADER_SEQ));
    }
    let (signed_body, log) = match &verb {
        Verb::Result { .. } => {
            let (meta, log) = result::split_body(body)?;
            (meta, log)
        }
        other => {
            let cap = other.body_cap();
            if body.len() > cap {
                return Err(Error::TooLarge {
                    limit: match other {
                        Verb::Enroll => Limit::Enroll,
                        Verb::Heartbeat => Limit::Beat,
                        Verb::Spec => Limit::Spec,
                        _ => Limit::GetBody,
                    },
                    size: body.len(),
                });
            }
            (body, &body[..0])
        }
    };
    if method == Method::Post {
        json::check(signed_body)?;
    }
    Ok(Checked {
        method,
        target,
        verb,
        envelope,
        signed_body,
        log,
    })
}

/// True when a request's `ts_ms` is too far from the server's clock
/// (`409 clock_skew`).
pub fn is_clock_skewed(ts_ms: u64, server_time_ms: u64) -> bool {
    ts_ms.abs_diff(server_time_ms) > MAX_SKEW_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_target_grammar_takes_the_verbs_and_refuses_the_rest() {
        for good in [
            "/node/v1/heartbeat",
            "/node/v1/artifacts/sha256:c7c5c1d70c5dec4416ab6158afd0b223ef40c29b1dc1f97ed9428b94d4cadb1c",
            "/node/v1/jobs/gate-0123456789ab-k3x9q2ma/result",
            "/node/v1/registry-manifest?x=1&y=a.b",
            "/node/v1/a..b",
        ] {
            check_target(good).unwrap();
        }
        let long = format!("/node/v1/{}", "a".repeat(221));
        let long_query = format!("/node/v1/a?{}", "b".repeat(201));
        for bad in [
            "/node/v1/",
            "/node/v2/heartbeat",
            "/node/v1/artifacts/sha256%3Aab",
            "/node/v1//heartbeat",
            "/node/v1/a//b",
            "/node/v1/./spec",
            "/node/v1/a/../spec",
            "/node/v1/a/..",
            "/node/v1/heart beat",
            "/node/v1/a?",
            "/node/v1/a?b?c",
            "/node/v1/a?b/c",
            "/node/v1/a#b",
            "node/v1/heartbeat",
            long.as_str(),
            long_query.as_str(),
        ] {
            assert_eq!(check_target(bad), Err(Error::Target), "{bad}");
        }
        assert_eq!(
            check_target(&format!("/node/v1/{}", "a".repeat(220))),
            Ok(())
        );
    }

    #[test]
    fn routes_bind_their_method_and_job_ids_are_one_segment() {
        assert_eq!(
            Verb::route(Method::Post, "/node/v1/heartbeat").unwrap(),
            Verb::Heartbeat
        );
        assert_eq!(
            Verb::route(Method::Get, "/node/v1/heartbeat"),
            Err(Error::Method)
        );
        assert_eq!(
            Verb::route(Method::Post, "/node/v1/jobs/gate-1/result").unwrap(),
            Verb::Result {
                job_id: "gate-1".into()
            }
        );
        assert_eq!(
            Verb::route(Method::Post, "/node/v1/jobs/a/b/result"),
            Err(Error::Target)
        );
        assert_eq!(
            Verb::route(Method::Get, "/node/v1/artifacts/sha256:00"),
            Err(Error::Target)
        );
        assert_eq!(
            Verb::route(Method::Get, "/node/v1/unknown"),
            Err(Error::Target)
        );
        assert_eq!(Method::parse("get"), Err(Error::Method));
    }

    #[test]
    fn the_public_prefix_is_stripped_only_whole() {
        assert_eq!(
            strip_public_prefix("/x/node/v1/spec", "/x"),
            Some("/node/v1/spec")
        );
        assert_eq!(
            strip_public_prefix("/x/node/v1/spec", "/x/"),
            Some("/node/v1/spec")
        );
        assert_eq!(strip_public_prefix("/xy/node/v1/spec", "/x"), None);
        assert_eq!(
            strip_public_prefix("/node/v1/spec", ""),
            Some("/node/v1/spec")
        );
    }

    #[test]
    fn clock_skew_is_over_two_minutes_either_way() {
        assert!(!is_clock_skewed(1_000_000, 1_120_000));
        assert!(is_clock_skewed(1_000_000, 1_120_001));
        assert!(is_clock_skewed(1_120_001, 1_000_000));
    }
}
