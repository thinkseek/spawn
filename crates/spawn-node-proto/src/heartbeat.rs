//! `POST /node/v1/heartbeat` (section 3.1) and its answer.

use serde::{Deserialize, Serialize};

use crate::codec::{is_build_sha, is_lower_hex, is_node_version};
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
/// Maximum valid battery charge percentage (`0..=100`).
pub const MAX_BATTERY_PCT: u8 = 100;
/// Maximum remaining battery estimate in minutes (matches PG 9-digit limit).
pub const MAX_REMAINING_MINS: u32 = 999_999_999;

/// The node's own wish (`state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeState {
    /// Node holds no active work and is available for grants.
    Idle,
    /// Node is running or holding active leases.
    Active,
    /// Node is finishing existing leases and refuses new grants.
    Draining,
}

/// Why the beat is sent now (`edge`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Edge {
    /// Initial beat sent right after node boot.
    Boot,
    /// Periodic scheduled beat.
    Timer,
    /// Beat triggered immediately because an execution slot freed up.
    SlotFree,
    /// Beat triggered immediately upon finishing a job result.
    Completed,
    /// Beat triggered upon accepting or acting on a grant.
    Grant,
    /// Beat triggered to deliver sync or artifact state.
    Sync,
}

/// A lease's informational phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Node is fetching workspace inputs or artifacts.
    Fetching,
    /// Node is executing the job workload.
    Running,
    /// Node is collecting and hashing job outputs.
    Collecting,
}

/// A pool's free and total slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pool {
    /// Available capacity units in the pool.
    pub free: u64,
    /// Total configured capacity units in the pool.
    pub total: u64,
}

/// A lease the node holds and has not finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    /// Identifier of the leased job.
    pub job_id: String,
    /// Monotonic epoch identifying this lease assignment.
    pub lease_epoch: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Optional execution phase; stripped first when trimming past 4 KB.
    pub phase: Option<Phase>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Elapsed runtime in seconds; stripped with `phase` under 4 KB trim.
    pub elapsed_s: Option<u64>,
}

/// A job at one lease epoch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseRef {
    /// Identifier of the referenced job.
    pub job_id: String,
    /// Lease epoch of the referenced job.
    pub lease_epoch: u64,
}

/// Power and battery telemetry reported optionally by a worker node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerTelemetry {
    /// Whether the worker is connected to AC power.
    pub on_ac: bool,
    #[serde(default)]
    /// Remaining battery percentage (`0..=100`), when running on battery.
    pub battery_pct: Option<u8>,
    /// Whether the battery is currently charging.
    pub charging: bool,
    #[serde(default)]
    /// Estimated remaining battery runtime in minutes, if available.
    pub remaining_mins: Option<u32>,
}

impl PowerTelemetry {
    /// Validate power telemetry bounds (`battery_pct <= 100`, `remaining_mins <= 999_999_999`).
    pub fn validate(&self) -> Result<(), Error> {
        if self.battery_pct.is_some_and(|pct| pct > MAX_BATTERY_PCT) {
            return Err(Error::Body("power.battery_pct"));
        }
        if self
            .remaining_mins
            .is_some_and(|mins| mins > MAX_REMAINING_MINS)
        {
            return Err(Error::Body("power.remaining_mins"));
        }
        Ok(())
    }
}

/// A heartbeat body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Beat {
    /// Protocol version (must equal [`VERSION`]).
    pub v: u64,
    /// Logical cell identifier assigned to this node.
    pub cell_id: String,
    /// Node's current scheduling state.
    pub state: NodeState,
    /// 64-char lowercase hex SHA-256 of the node's active [`NodeSpec`](crate::spec::NodeSpec).
    pub spec_sha256: String,
    /// What is free after every lease the node holds, pool by pool.
    pub capacities: OrderedMap<Pool>,
    /// Free and total ephemeral tmpfs budget in MiB.
    pub tmpfs_budget_mb: Pool,
    #[serde(default)]
    /// Active job leases currently held by the node.
    pub leases: Vec<Lease>,
    #[serde(default)]
    /// Finished job leases awaiting result upload or acknowledgment.
    pub lease_results_pending: Vec<LeaseRef>,
    #[serde(default)]
    /// Up to [`MAX_WARM_BASES`] 40-char git base object IDs kept warm locally.
    pub warm_bases: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// 1-minute host load average scaled by 1000 (milli-load).
    pub load1_milli: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Available host memory in MiB.
    pub mem_available_mb: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// Optional host power and battery telemetry.
    pub power: Option<PowerTelemetry>,
    /// Event that triggered this beat.
    pub edge: Edge,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The commit the daemon was built at: 7 to 64 lowercase hex digits
    /// (absent: an older daemon, or a build that does not know it).
    pub build_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    /// The daemon's version string: 1 to 64 bytes of printable ASCII.
    pub node_version: Option<String>,
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

    /// Validates beat version, hashes, warm bases, job ids, power telemetry,
    /// and the optional `build_sha` and `node_version`.
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
        if let Some(power) = &self.power {
            power.validate()?;
        }
        if self
            .build_sha
            .as_deref()
            .is_some_and(|sha| !is_build_sha(sha))
        {
            return Err(Error::Body("build_sha"));
        }
        if self
            .node_version
            .as_deref()
            .is_some_and(|version| !is_node_version(version))
        {
            return Err(Error::Body("node_version"));
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
    /// Identifier of the fenced job.
    pub job_id: String,
    /// Lease epoch rejected by the ledger.
    pub lease_epoch: u64,
    /// Human-readable reason the lease was fenced.
    pub reason: String,
}

/// The answer to a heartbeat (signed as every response is).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack {
    /// Protocol version (`1`).
    pub v: u64,
    /// Gateway clock timestamp in Unix milliseconds.
    pub server_time_ms: u64,
    /// Suggested delay before the next timer beat in milliseconds.
    pub next_beat_ms: u64,
    /// Lease expiration TTL in seconds.
    pub lease_ttl_s: u64,
    /// Whether the gateway requests a fresh `POST /node/v1/spec`.
    pub need_spec: bool,
    #[serde(default)]
    /// Cache/store holds the gateway instructs the node to sync.
    pub need_sync: Holds,
    #[serde(default)]
    /// Active leases renewed by this heartbeat.
    pub renewed: Vec<LeaseRef>,
    #[serde(default)]
    /// Leases the gateway no longer considers valid for this node.
    pub fenced: Vec<Fenced>,
    #[serde(default)]
    /// Pending job results acknowledged by the ledger.
    pub acked_results: Vec<LeaseRef>,
    #[serde(default)]
    /// Signed work grants allocated to the node.
    pub grants: Vec<SignedGrant>,
    /// Whether the node should transition to [`NodeState::Draining`].
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

    /// Serializes the heartbeat acknowledgment to JSON bytes.
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
            power: None,
            edge: Edge::Timer,
            build_sha: None,
            node_version: None,
        }
    }

    #[test]
    fn a_beat_without_build_fields_omits_them_and_parses() {
        let bytes = beat(0).to_bytes().unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(!text.contains("build_sha") && !text.contains("node_version"));
        let parsed = Beat::parse(&bytes).unwrap();
        assert_eq!(parsed.build_sha, None);
        assert_eq!(parsed.node_version, None);
    }

    #[test]
    fn a_beat_build_sha_and_node_version_roundtrip() {
        let mut with = beat(0);
        with.build_sha = Some("0123456789abcdef0123456789abcdef01234567".into());
        with.node_version = Some("0.1.0".into());
        let bytes = with.to_bytes().unwrap();
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.ends_with(
            r#""edge":"timer","build_sha":"0123456789abcdef0123456789abcdef01234567","node_version":"0.1.0"}"#
        ));
        assert_eq!(Beat::parse(&bytes).unwrap(), with);
    }

    #[test]
    fn a_beat_with_a_bad_build_sha_or_node_version_is_refused() {
        for sha in ["abc12", "ABCDEF0123", &"a".repeat(65), "xyz1234", ""] {
            let mut bad = beat(0);
            bad.build_sha = Some(sha.into());
            assert_eq!(bad.validate(), Err(Error::Body("build_sha")), "{sha:?}");
            let bytes = json::to_vec(&bad);
            assert_eq!(
                Beat::parse(&bytes),
                Err(Error::Body("build_sha")),
                "{sha:?}"
            );
        }
        for version in ["0.1\u{0}", &"1".repeat(65), "ver\u{e9}", "", "a\nb"] {
            let mut bad = beat(0);
            bad.node_version = Some(version.into());
            assert_eq!(
                bad.validate(),
                Err(Error::Body("node_version")),
                "{version:?}"
            );
        }
        // A non-string value is a shape error, not a silent drop.
        let text = String::from_utf8(beat(0).to_bytes().unwrap())
            .unwrap()
            .replace(r#""edge":"timer""#, r#""edge":"timer","build_sha":1234567"#);
        assert!(matches!(Beat::parse(text.as_bytes()), Err(Error::Shape(_))));
    }

    #[test]
    fn a_beat_power_field_roundtrips_and_enforces_bounds() {
        let mut with_power = beat(0);
        with_power.power = Some(PowerTelemetry {
            on_ac: false,
            battery_pct: Some(100),
            charging: false,
            remaining_mins: Some(MAX_REMAINING_MINS),
        });
        let bytes = with_power.to_bytes().unwrap();
        assert!(bytes.windows(7).any(|w| w == b"\"power\""));
        assert_eq!(Beat::parse(&bytes).unwrap(), with_power);

        let no_power = beat(0).to_bytes().unwrap();
        assert!(!no_power.windows(7).any(|w| w == b"\"power\""));

        let mut bad_pct = beat(0);
        bad_pct.power = Some(PowerTelemetry {
            on_ac: true,
            battery_pct: Some(101),
            charging: true,
            remaining_mins: None,
        });
        assert_eq!(bad_pct.validate(), Err(Error::Body("power.battery_pct")));

        let mut bad_mins = beat(0);
        bad_mins.power = Some(PowerTelemetry {
            on_ac: false,
            battery_pct: Some(50),
            charging: false,
            remaining_mins: Some(MAX_REMAINING_MINS + 1),
        });
        assert_eq!(
            bad_mins.validate(),
            Err(Error::Body("power.remaining_mins"))
        );
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
