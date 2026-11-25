//! `POST /node/v1/spec` (section 3.3): what a node is and holds. The
//! gateway hashes the body bytes itself ([`spec_sha256`]); it never takes
//! the node's word for `spec_sha256`.

use serde::{Deserialize, Serialize};

use crate::codec::sha256_hex;
use crate::json::{self, OrderedMap};
use crate::{Error, Limit, VERSION};

/// What a node holds (`spec.holds`), what a job requires
/// (`grant.requires`) and what an answer says to sync (`need_sync`).
/// Absent keys are not written.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holds {
    /// `tree_sha256_v1` of the cargo registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry: Option<String>,
    /// SHA-256 of `web/package-lock.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_lock: Option<String>,
    /// The `channel` of `rust-toolchain.toml`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolchain: Option<String>,
    /// The gate's PostgreSQL major version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pg: Option<String>,
}

impl Holds {
    fn pairs(&self) -> [(&'static str, Option<&str>); 4] {
        [
            ("registry", self.registry.as_deref()),
            ("web_lock", self.web_lock.as_deref()),
            ("toolchain", self.toolchain.as_deref()),
            ("pg", self.pg.as_deref()),
        ]
    }

    /// `requires <@ holds`: every key `requires` names, `self` holds at
    /// exactly that value (section 3.4 rule 3).
    pub fn satisfies(&self, requires: &Self) -> bool {
        self.pairs()
            .into_iter()
            .zip(requires.pairs())
            .all(|((_, held), (_, wanted))| wanted.is_none() || held == wanted)
    }

    pub fn is_empty(&self) -> bool {
        self.pairs().iter().all(|(_, value)| value.is_none())
    }
}

/// `resources_total`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resources {
    pub cores: u64,
    pub mem_mb: u64,
    pub disk_mb: u64,
}

/// A spec body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeSpec {
    pub v: u64,
    pub name: String,
    pub cell_id: String,
    pub labels: Vec<String>,
    pub resources_total: Resources,
    /// Pool name → total slots, in the node's order.
    pub capacities_total: OrderedMap<u64>,
    pub tmpfs_budget_mb: u64,
    pub holds: Holds,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub versions: Option<OrderedMap<String>>,
}

impl NodeSpec {
    /// A received spec body: within 64 KB, strict JSON, version 1.
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > Limit::Spec.bytes() {
            return Err(Error::TooLarge {
                limit: Limit::Spec,
                size: bytes.len(),
            });
        }
        let spec: Self = json::parse(bytes)?;
        spec.validate()?;
        Ok(spec)
    }

    pub fn validate(&self) -> Result<(), Error> {
        if self.v != VERSION {
            return Err(Error::Body("v"));
        }
        if self.name.is_empty() || self.cell_id.is_empty() {
            return Err(Error::Body("name"));
        }
        Ok(())
    }

    /// The bytes a node sends.
    pub fn to_bytes(&self) -> Vec<u8> {
        json::to_vec(self)
    }
}

/// The spec's SHA-256: over the body bytes as received.
pub fn spec_sha256(body: &[u8]) -> String {
    sha256_hex(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_is_contained_in_holds_key_by_key() {
        let holds = Holds {
            registry: Some("r".into()),
            pg: Some("16".into()),
            ..Holds::default()
        };
        assert!(holds.satisfies(&Holds::default()));
        assert!(holds.satisfies(&Holds {
            pg: Some("16".into()),
            ..Holds::default()
        }));
        assert!(!holds.satisfies(&Holds {
            pg: Some("17".into()),
            ..Holds::default()
        }));
        assert!(!holds.satisfies(&Holds {
            web_lock: Some("w".into()),
            ..Holds::default()
        }));
        assert!(Holds::default().is_empty());
        assert_eq!(json::to_vec(&Holds::default()), b"{}");
    }
}
