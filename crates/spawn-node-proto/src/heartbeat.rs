//! `POST /node/v1/heartbeat` (section 3.1) and its answer.

use serde::{Deserialize, Serialize};

use crate::codec::is_lower_hex;
use crate::envelope::is_job_id;
use crate::grant::SignedGrant;
use crate::json::{self, OrderedMap};
use crate::spec::Holds;
use crate::{Error, Limit, VERSION};

/// `next_beat_ms` while the node holds a lease or a pending result, was
/// just granted one, or its cell has a queued job.
pub const NEXT_BEAT_BUSY_MS: u64 = 5_000;
/// `next_beat_ms` otherwise.
pub const NEXT_BEAT_IDLE_MS: u64 = 30_000;
/// The most grants one answer carries.
pub const MAX_GRANTS_PER_ANSWER: usize = 2;
/// The most `warm_bases` a beat lists.
pub const MAX_WARM_BASES: usize = 4;
/// Edges closer than this are coalesced into one beat.
pub const EDGE_COALESCE_MS: u64 = 500;

/// The node's own wish (`state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    Idle,
    Active,
    Draining,
}

/// Why the beat is sent now (`edge`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Edge {
    Boot,
    Timer,
    SlotFree,
    Completed,
    Grant,
    Sync,
}

/// A lease's informational phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Fetching,
    Running,
    Collecting,
}

/// A pool's free and total slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pool {
    pub free: u64,
    pub total: u64,
}

/// A lease the node holds and has not finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub job_id: String,
    pub lease_epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<Phase>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_s: Option<u64>,
}

/// A job at one lease epoch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseRef {
    pub job_id: String,
    pub lease_epoch: u64,
}

/// A heartbeat body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Beat {
    pub v: u64,
    pub cell_id: String,
    pub state: NodeState,
    pub spec_sha256: String,
    /// What is free after every lease the node holds, pool by pool.
    pub capacities: OrderedMap<Pool>,
    pub tmpfs_budget_mb: Pool,
    #[serde(default)]
    pub leases: Vec<Lease>,
    #[serde(default)]
    pub lease_results_pending: Vec<LeaseRef>,
    #[serde(default)]
    pub warm_bases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load1_milli: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_available_mb: Option<u64>,
    pub edge: Edge,
}

impl Beat {
    /// A received beat: within 4 KB, strict JSON, valid fields.
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > Limit::Beat.bytes() {
            return Err(Error::TooLarge {
                limit: Limit::Beat,
                size: bytes.len(),
            });
        }
        let beat: Self = json::parse(bytes)?;
        beat.validate()?;
        Ok(beat)
    }

    pub fn validate(&self) -> Result<(), Error> {
        if self.v != VERSION {
            return Err(Error::Body("v"));
        }
        if !is_lower_hex(&self.spec_sha256, 64) {
            return Err(Error::Body("spec_sha256"));
        }
        if self.warm_bases.len() > MAX_WARM_BASES
            || !self.warm_bases.iter().all(|base| is_lower_hex(base, 40))
        {
            return Err(Error::Body("warm_bases"));
        }
        let ids = self
            .leases
            .iter()
            .map(|lease| &lease.job_id)
            .chain(self.lease_results_pending.iter().map(|lease| &lease.job_id));
        if !ids.into_iter().all(|id| is_job_id(id)) {
            return Err(Error::Body("job_id"));
        }
        Ok(())
    }

    /// The bytes a node sends: the whole beat, or, when that passes 4 KB,
    /// the beat without the leases' `phase` and `elapsed_s`; a beat that
    /// still does not fit is refused locally.
    pub fn to_bytes(&self) -> Result<Vec<u8>, Error> {
        let full = json::to_vec(self);
        if full.len() <= Limit::Beat.bytes() {
            return Ok(full);
        }
        let mut lean = self.clone();
        for lease in &mut lean.leases {
            lease.phase = None;
            lease.elapsed_s = None;
        }
        let lean = json::to_vec(&lean);
        if lean.len() <= Limit::Beat.bytes() {
            Ok(lean)
        } else {
            Err(Error::TooLarge {
                limit: Limit::Beat,
                size: lean.len(),
            })
        }
    }
}

/// A lease the ledger does not hold for the node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fenced {
    pub job_id: String,
    pub lease_epoch: u64,
    pub reason: String,
}

/// The answer to a heartbeat (signed as every response is).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack {
    pub v: u64,
    pub server_time_ms: u64,
    pub next_beat_ms: u64,
    pub lease_ttl_s: u64,
    pub need_spec: bool,
    #[serde(default)]
    pub need_sync: Holds,
    #[serde(default)]
    pub renewed: Vec<LeaseRef>,
    #[serde(default)]
    pub fenced: Vec<Fenced>,
    #[serde(default)]
    pub acked_results: Vec<LeaseRef>,
    #[serde(default)]
    pub grants: Vec<SignedGrant>,
    pub drain: bool,
}

impl Ack {
    /// A received answer: strict JSON, version 1, at most two grants.
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        let ack: Self = json::parse(bytes)?;
        if ack.v != VERSION {
            return Err(Error::Body("v"));
        }
        if ack.grants.len() > MAX_GRANTS_PER_ANSWER {
            return Err(Error::Body("grants"));
        }
        Ok(ack)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        json::to_vec(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn beat(leases: usize) -> Beat {
        Beat {
            v: 1,
            cell_id: "c0".into(),
            state: NodeState::Active,
            spec_sha256: "0".repeat(64),
            capacities: [("gate_sandbox", Pool { free: 1, total: 4 })]
                .into_iter()
                .collect(),
            tmpfs_budget_mb: Pool { free: 1, total: 2 },
            leases: (0..leases)
                .map(|n| Lease {
                    job_id: format!("gate-0123456789ab-{n:08}"),
                    lease_epoch: 1,
                    phase: Some(Phase::Running),
                    elapsed_s: Some(120),
                })
                .collect(),
            lease_results_pending: vec![],
            warm_bases: vec![],
            load1_milli: Some(1250),
            mem_available_mb: None,
            edge: Edge::Timer,
        }
    }

    #[test]
    fn a_beat_over_four_kilobytes_drops_phases_first_then_is_refused() {
        let small = beat(2);
        let bytes = small.to_bytes().unwrap();
        assert!(bytes.windows(7).any(|w| w == b"\"phase\""));
        assert_eq!(Beat::parse(&bytes).unwrap(), small);

        let lean = beat(60).to_bytes().unwrap();
        assert!(lean.len() <= 4096);
        assert!(!lean.windows(7).any(|w| w == b"\"phase\""));

        assert!(matches!(
            beat(200).to_bytes(),
            Err(Error::TooLarge {
                limit: Limit::Beat,
                ..
            })
        ));
    }

    #[test]
    fn a_beat_with_bad_fields_is_refused() {
        let mut bad = beat(0);
        bad.warm_bases = vec!["a".repeat(40); 5];
        assert_eq!(bad.validate(), Err(Error::Body("warm_bases")));
        let mut bad = beat(1);
        bad.leases[0].job_id = "../x".into();
        assert_eq!(bad.validate(), Err(Error::Body("job_id")));
        let mut bad = beat(0);
        bad.v = 2;
        assert_eq!(bad.validate(), Err(Error::Body("v")));
        let bytes = String::from_utf8(beat(0).to_bytes().unwrap())
            .unwrap()
            .replace("\"timer\"", "\"sometime\"");
        assert!(matches!(
            Beat::parse(bytes.as_bytes()),
            Err(Error::Shape(_))
        ));
    }
}
