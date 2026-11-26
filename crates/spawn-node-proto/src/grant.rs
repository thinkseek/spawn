//! The grant (section 3.1): pure data model in `spawn_core::node_proto::grant`,
//! signed envelopes (`SignedGrant` / `open_grant`) here.

use crate::codec::b64u;
use crate::json;
use crate::keys::{PublicKey, SigningKey, decode_signature};
use crate::{Error, Limit};

pub use spawn_core::node_proto::grant::{
    ArtifactRole, ArtifactUrl, GRANT_PREFIX, Grant, GrantInputs, JobKind, NODE_OWNED_ENV,
    grant_signing_input,
};

/// A grant as an answer carries it: the base64url of its exact JSON bytes
/// and the primary's signature.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SignedGrant {
    /// Unpadded base64url encoding of the exact JSON grant bytes.
    pub grant: String,
    /// Unpadded base64url Ed25519 signature produced by the primary key.
    pub sig: String,
}

impl SignedGrant {
    /// The gateway's side: sign `grant` (its exact bytes).
    pub fn sign(primary: &SigningKey, grant: &[u8]) -> Self {
        Self {
            grant: b64u(grant),
            sig: primary.sign_b64u(&grant_signing_input(grant)),
        }
    }

    /// The grant's bytes, decoded strictly; refused past the 8 KB cap.
    pub fn bytes(&self) -> Result<Vec<u8>, Error> {
        decode_grant_bytes(&self.grant)
    }

    /// The node's side: verify, then parse those same bytes, then refuse a
    /// grant for another node or one that has expired at `now_ms`.
    pub fn open(
        &self,
        primary: &PublicKey,
        own_node_id: &str,
        now_ms: u64,
    ) -> Result<(Vec<u8>, Grant), Error> {
        let bytes = self.bytes()?;
        let grant = open_grant(primary, &bytes, &self.sig, own_node_id, now_ms)?;
        Ok((bytes, grant))
    }
}

/// Verifies stored grant bytes and their signature (base64url), then
/// parses and checks them as [`SignedGrant::open`] does.
pub fn open_grant(
    primary: &PublicKey,
    bytes: &[u8],
    sig: &str,
    own_node_id: &str,
    now_ms: u64,
) -> Result<Grant, Error> {
    if bytes.len() > Limit::Grant.bytes() {
        return Err(Error::TooLarge {
            limit: Limit::Grant,
            size: bytes.len(),
        });
    }
    primary.verify(&grant_signing_input(bytes), &decode_signature(sig)?)?;
    let grant: Grant = json::parse(bytes)?;
    grant.validate()?;
    if grant.node_id != own_node_id {
        return Err(Error::Grant("node_id is not this node's"));
    }
    if now_ms >= grant.expires_ms {
        return Err(Error::Grant("expired"));
    }
    Ok(grant)
}

fn decode_grant_bytes(text: &str) -> Result<Vec<u8>, Error> {
    let cap = Limit::Grant.bytes();
    // 4 characters per 3 bytes, unpadded.
    if text.len() > cap.div_ceil(3) * 4 {
        return Err(Error::TooLarge {
            limit: Limit::Grant,
            size: text.len() / 4 * 3,
        });
    }
    let len = text.len() / 4 * 3
        + match text.len() % 4 {
            0 => 0,
            2 => 1,
            3 => 2,
            _ => return Err(Error::Base64),
        };
    crate::codec::unb64u(text, len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::Holds;

    const NODE: &str = "56475aa75463474c0285df5dbf2bcab73da651358839e9b77481b2eab107708c";

    fn primary() -> SigningKey {
        SigningKey::from_seed(&[7; 32]).unwrap()
    }

    fn grant() -> Grant {
        Grant {
            v: 1,
            job_id: "gate-0123456789ab-k3x9q2ma".into(),
            lease_epoch: 1,
            node_id: NODE.into(),
            kind: JobKind::Gate,
            expires_ms: 1_000,
            attempt: Some(1),
            cost: Some(
                [("gate_sandbox", 1), ("tmpfs_mb", 20480)]
                    .into_iter()
                    .collect(),
            ),
            timeout_s: Some(7200),
            inputs: Some(GrantInputs {
                repo: "spawn".into(),
                base: "a".repeat(40),
                candidate: "b".repeat(40),
                controller: "c".repeat(40),
                scenarios_sha256: "d".repeat(64),
                scenario: None,
            }),
            env: Some([("SPAWN_GATE_JOBS", "8".to_string())].into_iter().collect()),
            requires: Some(Holds {
                pg: Some("16".into()),
                ..Holds::default()
            }),
            artifact_urls: Some(vec![ArtifactUrl {
                role: ArtifactRole::DeltaBundle,
                digest: format!("sha256:{}", "e".repeat(64)),
                size: 48211,
                url: format!("/node/v1/artifacts/sha256:{}", "e".repeat(64)),
            }]),
        }
    }

    #[test]
    fn a_full_grant_round_trips_signed_and_opens_for_its_node_only_before_expiry() {
        let key = primary();
        let bytes = grant().to_bytes();
        let signed = SignedGrant::sign(&key, &bytes);
        let (opened, parsed) = signed.open(&key.public_key(), NODE, 999).unwrap();
        assert_eq!(opened, bytes);
        assert_eq!(parsed, grant());
        assert_eq!(
            signed.open(&key.public_key(), NODE, 1_000).unwrap_err(),
            Error::Grant("expired")
        );
        assert_eq!(
            signed
                .open(&key.public_key(), &"0".repeat(64), 0)
                .unwrap_err(),
            Error::Grant("node_id is not this node's")
        );
        let other = SigningKey::from_seed(&[8; 32]).unwrap();
        assert_eq!(
            signed.open(&other.public_key(), NODE, 0).unwrap_err(),
            Error::Signature
        );
        let mut tampered = signed.clone();
        tampered.grant = b64u(&grant().to_bytes()[..bytes.len() - 1]);
        assert_eq!(
            tampered.open(&key.public_key(), NODE, 0).unwrap_err(),
            Error::Signature
        );
    }

    #[test]
    fn a_grant_naming_a_node_owned_variable_or_a_bad_url_is_refused() {
        let key = primary();
        for name in NODE_OWNED_ENV {
            let mut bad = grant();
            bad.env = Some([(name, "x".to_string())].into_iter().collect());
            let signed = SignedGrant::sign(&key, &bad.to_bytes());
            assert_eq!(
                signed.open(&key.public_key(), NODE, 0).unwrap_err(),
                Error::ForbiddenEnv(name.into())
            );
        }
        for url in ["http://x/y", "/node/v1/../x", "relative"] {
            let mut bad = grant();
            bad.artifact_urls.as_mut().unwrap()[0].url = url.into();
            assert_eq!(bad.validate(), Err(Error::Grant("artifact url")), "{url}");
        }
        let mut https = grant();
        https.artifact_urls.as_mut().unwrap()[0].url = "https://bucket.example/x".into();
        https.validate().unwrap();
        assert!(!https.artifact_urls.unwrap()[0].is_gateway_path());
    }

    #[test]
    fn a_grant_over_eight_kilobytes_is_refused_before_its_signature() {
        let key = primary();
        let mut big = grant();
        big.env = Some(
            [("SPAWN_GATE_JOBS", "x".repeat(9000))]
                .into_iter()
                .collect(),
        );
        let signed = SignedGrant::sign(&key, &big.to_bytes());
        assert!(matches!(
            signed.open(&key.public_key(), NODE, 0),
            Err(Error::TooLarge {
                limit: Limit::Grant,
                ..
            })
        ));
        let bad = SignedGrant {
            grant: "A".into(),
            sig: signed.sig,
        };
        assert_eq!(bad.bytes(), Err(Error::Base64));
    }
}
