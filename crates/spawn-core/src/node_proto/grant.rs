//! Pure data model for job grants (`spawn-node-grant-v1`).
//!
//! Cryptographic signing and opening ([`SignedGrant`] / `open_grant`) remain
//! in `spawn-node-proto` so `spawn-core` stays free of `ring` and `base64`.

use serde::{Deserialize, Serialize};

use super::codec::{is_lower_hex, sha256_hex};
use super::json::{self, OrderedMap};
use super::spec::Holds;
use super::target::{check_target, is_job_id};
use super::{Error, VERSION};

/// The first line of the grant signing input.
pub const GRANT_PREFIX: &str = "spawn-node-grant-v1";

/// Variables a grant's `env` may not name: the node sets them itself, and
/// it never receives a database URL.
pub const NODE_OWNED_ENV: [&str; 8] = [
    "PATH",
    "HOME",
    "TMPDIR",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "SPAWN_TEST_DATABASE_URL",
    "SPAWN_GATE_PG_IMAGE",
    "SPAWN_GATE_PG_BIN",
];

/// The signed bytes of a grant.
pub fn grant_signing_input(grant: &[u8]) -> Vec<u8> {
    format!("{GRANT_PREFIX}\n{}\n", sha256_hex(grant)).into_bytes()
}

/// Validates `sha256:<64 lowercase hex>` digest syntax without requiring
/// `spawn-cas` inside `spawn-core`.
pub fn check_digest_syntax(text: &str) -> Result<(), Error> {
    let hex = text.strip_prefix("sha256:").ok_or(Error::Digest)?;
    if !is_lower_hex(hex, 64) {
        return Err(Error::Digest);
    }
    Ok(())
}

/// What a job is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// Gate run.
    Gate,
    /// Build job.
    Build,
    /// De-batch job.
    DeBatch,
}

/// The immutable inputs of a gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrantInputs {
    /// Repository slug.
    pub repo: String,
    /// Base commit.
    pub base: String,
    /// Candidate commit.
    pub candidate: String,
    /// Controller commit.
    pub controller: String,
    /// Scenarios tree digest.
    pub scenarios_sha256: String,
    /// Selected scenario name.
    pub scenario: Option<String>,
}

/// What an artifact is to the job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactRole {
    /// Delta pack over baseline.
    DeltaBundle,
    /// Full baseline pack.
    BaselineBundle,
    /// Controller pack.
    ControllerPack,
}

/// One artifact to fetch: a gateway path (`/node/v1/…`, with the envelope)
/// or an absolute `https://` URL (without it); believed only if the bytes
/// hash to `digest`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactUrl {
    /// Purpose of the artifact.
    pub role: ArtifactRole,
    /// Content digest (`sha256:<64 hex>` or `blake3:<64 hex>`).
    pub digest: String,
    /// Byte length.
    pub size: u64,
    /// Gateway-relative path or HTTPS URL.
    pub url: String,
}

impl ArtifactUrl {
    /// True when the URL is fetched from the gateway with the envelope.
    pub fn is_gateway_path(&self) -> bool {
        self.url.starts_with('/')
    }
}

/// A grant's JSON. Fields after `expires_ms` are absent from a minimal
/// grant and are not written when `None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    /// Protocol version.
    pub v: u64,
    /// Job identifier.
    pub job_id: String,
    /// Monotonic lease epoch.
    pub lease_epoch: u64,
    /// Assigned node identifier (64 lowercase hex).
    pub node_id: String,
    /// Job kind.
    pub kind: JobKind,
    /// Expiration timestamp (Unix ms).
    pub expires_ms: u64,
    /// Retry attempt counter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u64>,
    /// Capacity cost per pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<OrderedMap<u64>>,
    /// Hard wall timeout in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<u64>,
    /// Gate inputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inputs: Option<GrantInputs>,
    /// Extra environment variables.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<OrderedMap<String>>,
    /// Required node holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires: Option<Holds>,
    /// Artifacts to fetch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_urls: Option<Vec<ArtifactUrl>>,
}

impl Grant {
    /// The checks that do not depend on who reads the grant.
    pub fn validate(&self) -> Result<(), Error> {
        if self.v != VERSION {
            return Err(Error::Grant("v"));
        }
        if !is_job_id(&self.job_id) {
            return Err(Error::Grant("job_id"));
        }
        if !is_lower_hex(&self.node_id, 64) {
            return Err(Error::Grant("node_id"));
        }
        if let Some(name) = self
            .env
            .iter()
            .flat_map(OrderedMap::keys)
            .find(|name| NODE_OWNED_ENV.contains(name))
        {
            return Err(Error::ForbiddenEnv(name.to_string()));
        }
        for artifact in self.artifact_urls.iter().flatten() {
            check_digest_syntax(&artifact.digest).map_err(|_| Error::Grant("artifact digest"))?;
            let url_ok = if artifact.is_gateway_path() {
                check_target(&artifact.url).is_ok()
            } else {
                artifact.url.starts_with("https://")
            };
            if !url_ok {
                return Err(Error::Grant("artifact url"));
            }
        }
        Ok(())
    }

    /// The exact bytes the gateway signs.
    pub fn to_bytes(&self) -> Vec<u8> {
        json::to_vec(self)
    }
}
