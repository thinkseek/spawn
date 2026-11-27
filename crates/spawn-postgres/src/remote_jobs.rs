//! spawn-node v1's ledger (issue #96; design "spawn-node v1 and
//! host-adaptive storage" §§4–5): the v1 nodes (`spawn_node_state`) and the
//! remote jobs they are leased (`spawn_remote_jobs`), both written only
//! through the SQL functions of `sql/schema.sql`.
//!
//! Two callers, two logins:
//!
//! - the gateway (`spawn-node-gateway`), logged in as this database's
//!   `spawn_node_gateway` login, calls the four `spawn_node_gw_*` functions
//!   and nothing else. Each call runs in a transaction of its own and is
//!   *verify-then-commit*: the function's answer carries the node's public
//!   key, the gateway checks the request's signature against it, and the
//!   transaction commits only when the check passes
//!   ([`PostgresStore::node_gw_beat`] and its kin take that check as a
//!   closure);
//! - the owner (the integrator, `spawn node …`) enrolls, drains, sweeps,
//!   enqueues, takes a job back and judges a node's fault.
//!
//! A beat is a row update and never an event; transitions are recorded as
//! `node.recorded` records of the control session with `"proto":"node-v1"`,
//! outside every fold.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use spawn_core::port::StoreError;

use crate::{PostgresStore, backend};

/// The base name of the gateway's login (`spawn_login_role` makes this
/// database's own of it).
pub(crate) const GATEWAY_LOGIN: &str = "spawn_node_gateway";

/// The gateway login's defaults (design §4.2 step 6): one call never holds
/// a row, or the append lock, for long.
const GATEWAY_TIMEOUTS: [(&str, &str); 3] = [
    ("statement_timeout", "5s"),
    ("lock_timeout", "2s"),
    ("idle_in_transaction_session_timeout", "5s"),
];

/// This database's gateway login, by name.
fn gateway_role(client: &mut impl postgres::GenericClient) -> Result<String, postgres::Error> {
    Ok(client
        .query_one("select spawn_login_role($1)", &[&GATEWAY_LOGIN])?
        .get(0))
}

/// A migration without the owner's password turns the gateway's login off
/// too (`spawn_logins_off` does the other three).
pub(crate) fn gateway_login_off(
    client: &mut impl postgres::GenericClient,
) -> Result<(), postgres::Error> {
    let role = gateway_role(client)?;
    // The role is spawn_login_role's: [a-z0-9_], nothing to quote.
    client.batch_execute(&format!("alter role \"{role}\" with nologin password null"))
}

/// Set the gateway login's timeouts as its role defaults, on every
/// migration (idempotent).
pub(crate) fn gateway_login_timeouts(
    client: &mut impl postgres::GenericClient,
) -> Result<(), postgres::Error> {
    let role = gateway_role(client)?;
    for (setting, value) in GATEWAY_TIMEOUTS {
        client.batch_execute(&format!("alter role \"{role}\" set {setting} = '{value}'"))?;
    }
    Ok(())
}

/// What a remote job runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteJobKind {
    /// A gate whose verdict may be believed (M3); never dead-lettered.
    Gate,
    /// A gate judged again on a node, deciding nothing (M2).
    GateShadow,
    /// A build.
    Build,
    /// A batch of DE work.
    DeBatch,
}

/// Where a remote job is (design §6.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteJobState {
    /// Waiting for a node.
    Queued,
    /// Leased to a node at its `lease_epoch`.
    Leased,
    /// Its lease ended (finished, or past its time); a result may still come
    /// within the grace.
    ResultPending,
    /// A node's result is stored.
    Completed,
    /// The primary runs it itself.
    FallbackLocal,
    /// A build or batch that ran out of attempts.
    DeadLetter,
    /// A shadow gate that ran out of attempts.
    Cancelled,
}

/// A job to queue (`spawn_remote_job_enqueue`). `requires` is what a node
/// must hold, key by key (`registry`, `web_lock`, `toolchain`, `pg`);
/// `cost` what it takes of the node's free capacity pools and `tmpfs_mb`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RemoteJobSpec {
    /// `^[a-z0-9][a-z0-9._-]{7,63}$`.
    pub job_id: String,
    /// What it runs.
    pub kind: RemoteJobKind,
    /// The cell (`c0` when none).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cell_id: Option<String>,
    /// Higher first (0 when none).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<i32>,
    /// The judging it serves, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub judging_id: Option<String>,
    /// The mission, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mission_id: Option<i64>,
    /// The task, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<i64>,
    /// What the node is granted (a JSON object, at most 64 KiB).
    pub spec: Value,
    /// What a node must hold to be leased it.
    pub requires: BTreeMap<String, String>,
    /// What it takes of a node.
    pub cost: BTreeMap<String, i64>,
    /// The base commit (40 hex), when a node may need its baseline.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    /// Leases it may take (2 when none).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<i32>,
    /// How long it may wait for a lease (30 000 when none).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_wait_ms: Option<i64>,
}

/// Stored node enrollment/lifecycle state in `spawn_node_state.status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    /// Enrolled by token only; waiting for node key registration.
    Pending,
    /// Key pinned and eligible for heartbeats and grants (subject to drain/degraded flags).
    Active,
    /// Permanently revoked by owner admin action.
    Revoked,
}

impl NodeStatus {
    /// Canonical lowercase wire/SQL representation (`pending`, `active`, `revoked`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Revoked => "revoked",
        }
    }
}

/// Completion artifacts and exit metadata recorded once a remote job finishes on a node.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteJobResult {
    /// Completion timestamp in epoch milliseconds.
    pub finished_at_ms: i64,
    /// Remote process exit code (`None` when terminated by signal or timeout without exit status).
    pub exit_code: Option<i32>,
    /// The node's result metadata object, as uploaded.
    pub result: Value,
    /// SHA-256 hex digest of the result artifact.
    pub result_sha256: String,
    /// SHA-256 hex digest of the captured log artifact.
    pub log_sha256: String,
    /// Byte length of the captured remote log (`0..=33_554_432`).
    pub log_bytes: i64,
}

/// Closed lifecycle progress and stage-specific fields of a [`RemoteJob`].
#[derive(Clone, Debug, PartialEq)]
pub enum RemoteJobProgress {
    /// Waiting in the queue for a node lease.
    Queued {
        /// Queue deadline timestamp in epoch milliseconds, if bounded.
        queue_deadline_ms: Option<i64>,
    },
    /// Actively leased to a node until `lease_expires_ms`.
    Leased {
        /// Node holding the current lease.
        node_id: String,
        /// Timestamp when the current attempt was leased, in epoch milliseconds.
        leased_at_ms: i64,
        /// Timestamp when the current lease expires, in epoch milliseconds.
        lease_expires_ms: i64,
    },
    /// Lease expired or node finished execution; awaiting late result upload until `grace_until_ms`.
    ResultPending {
        /// Node holding the expired lease within grace.
        node_id: String,
        /// Timestamp when the current attempt was leased, in epoch milliseconds.
        leased_at_ms: i64,
        /// Original lease expiry timestamp in epoch milliseconds.
        lease_expires_ms: i64,
        /// Grace deadline timestamp in epoch milliseconds.
        grace_until_ms: i64,
    },
    /// Completed on a node with verified result artifacts.
    Completed {
        /// Node that finished the run.
        node_id: String,
        /// Timestamp when the completed attempt was leased, if recorded.
        leased_at_ms: Option<i64>,
        /// Uploaded result metadata and artifact digests.
        completion: RemoteJobResult,
    },
    /// Taken back or routed to run locally on the primary host.
    FallbackLocal,
    /// Exhausted all attempts for a build or batch job.
    DeadLetter,
    /// Cancelled after exhausting shadow attempts.
    Cancelled,
}

impl RemoteJobProgress {
    /// Projection back to the flat [`RemoteJobState`] tag.
    pub const fn state(&self) -> RemoteJobState {
        match self {
            Self::Queued { .. } => RemoteJobState::Queued,
            Self::Leased { .. } => RemoteJobState::Leased,
            Self::ResultPending { .. } => RemoteJobState::ResultPending,
            Self::Completed { .. } => RemoteJobState::Completed,
            Self::FallbackLocal => RemoteJobState::FallbackLocal,
            Self::DeadLetter => RemoteJobState::DeadLetter,
            Self::Cancelled => RemoteJobState::Cancelled,
        }
    }

    /// Active holder or finishing `node_id` for states that hold one.
    pub fn node_id(&self) -> Option<&str> {
        match self {
            Self::Leased { node_id, .. }
            | Self::ResultPending { node_id, .. }
            | Self::Completed { node_id, .. } => Some(node_id.as_str()),
            Self::Queued { .. } | Self::FallbackLocal | Self::DeadLetter | Self::Cancelled => None,
        }
    }

    /// Uploaded completion details when in [`RemoteJobProgress::Completed`].
    pub const fn completion(&self) -> Option<&RemoteJobResult> {
        match self {
            Self::Completed { completion, .. } => Some(completion),
            _ => None,
        }
    }
}

/// A remote job as the ledger holds it.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(try_from = "RawRemoteJob")]
pub struct RemoteJob {
    /// Unique job identifier (`spawn_remote_jobs.job_id`).
    pub job_id: String,
    /// Scheduling cell identifier (`c0` by default).
    pub cell_id: String,
    /// Kind of work performed (`gate`, `gate_shadow`, `build`, `de_batch`).
    pub kind: RemoteJobKind,
    /// Dispatch priority (higher executes first).
    pub priority: i32,
    /// Associated judging identifier, if any.
    pub judging_id: Option<String>,
    /// Associated mission id, if any.
    pub mission_id: Option<i64>,
    /// Associated task id, if any.
    pub task_id: Option<i64>,
    /// Job specification payload passed to the worker node.
    pub spec: Value,
    /// Required node capabilities/tags map.
    pub requires: Value,
    /// Estimated resource cost deducted from node capacity pools.
    pub cost: Value,
    /// Baseline Git commit OID, if needed by the remote job.
    pub base_commit: Option<String>,
    /// Nodes that may not be leased it again.
    pub excluded_nodes: Vec<String>,
    /// Nodes it faulted on.
    pub fault_nodes: Vec<String>,
    /// Moves at every grant and every fence: only the current epoch's
    /// result is believed.
    pub lease_epoch: i64,
    /// Total execution attempts used so far.
    pub attempts: i32,
    /// Maximum attempts permitted before fallback or dead-lettering.
    pub max_attempts: i32,
    /// Timestamp when the job was first enqueued, in epoch milliseconds.
    pub enqueued_at_ms: i64,
    /// Closed lifecycle stage and stage-owned timestamps, node holder, and result.
    pub progress: RemoteJobProgress,
}

impl RemoteJob {
    /// Current lifecycle state tag derived from [`Self::progress`].
    pub const fn state(&self) -> RemoteJobState {
        self.progress.state()
    }

    /// Holder or finishing `node_id` derived from [`Self::progress`].
    pub fn node_id(&self) -> Option<&str> {
        self.progress.node_id()
    }

    /// Completion artifacts when [`Self::progress`] is [`RemoteJobProgress::Completed`].
    pub const fn completion(&self) -> Option<&RemoteJobResult> {
        self.progress.completion()
    }
}

#[derive(Deserialize)]
struct RawRemoteJob {
    job_id: String,
    cell_id: String,
    kind: RemoteJobKind,
    state: RemoteJobState,
    priority: i32,
    judging_id: Option<String>,
    mission_id: Option<i64>,
    task_id: Option<i64>,
    spec: Value,
    requires: Value,
    cost: Value,
    base_commit: Option<String>,
    excluded_nodes: Vec<String>,
    fault_nodes: Vec<String>,
    node_id: Option<String>,
    lease_epoch: i64,
    lease_expires_ms: Option<i64>,
    grace_until_ms: Option<i64>,
    attempts: i32,
    max_attempts: i32,
    enqueued_at_ms: i64,
    queue_deadline_ms: Option<i64>,
    leased_at_ms: Option<i64>,
    finished_at_ms: Option<i64>,
    exit_code: Option<i32>,
    result: Option<Value>,
    result_sha256: Option<String>,
    log_sha256: Option<String>,
    log_bytes: Option<i64>,
}

const MAX_REMOTE_LOG_BYTES: i64 = 33_554_432;

fn raw_has_completion_fields(raw: &RawRemoteJob) -> bool {
    raw.finished_at_ms.is_some()
        || raw.exit_code.is_some()
        || raw.result.is_some()
        || raw.result_sha256.is_some()
        || raw.log_sha256.is_some()
        || raw.log_bytes.is_some()
}

fn require_non_empty<'a>(
    opt: Option<&'a str>,
    field: &str,
    state: &str,
) -> Result<&'a str, String> {
    let val = opt.ok_or_else(|| format!("remote_job state {state} requires non-null {field}"))?;
    if val.is_empty() {
        return Err(format!(
            "remote_job state {state} requires non-empty {field}"
        ));
    }
    Ok(val)
}

fn forbid_active_lease_timers(raw: &RawRemoteJob, state: &str) -> Result<(), String> {
    if raw.lease_expires_ms.is_some() {
        return Err(format!(
            "remote_job state {state} must not have lease_expires_ms"
        ));
    }
    if raw.grace_until_ms.is_some() {
        return Err(format!(
            "remote_job state {state} must not have grace_until_ms"
        ));
    }
    Ok(())
}

fn decode_remote_job_progress(raw: &mut RawRemoteJob) -> Result<RemoteJobProgress, String> {
    let prior_exclusions = !raw.excluded_nodes.is_empty();
    let has_completion = raw_has_completion_fields(raw);
    match raw.state {
        RemoteJobState::Queued => {
            if raw.node_id.is_some() {
                return Err("remote_job state queued must not have node_id".to_string());
            }
            forbid_active_lease_timers(raw, "queued")?;
            if !prior_exclusions && has_completion {
                return Err(
                    "remote_job state queued without excluded_nodes must not have completion fields"
                        .to_string(),
                );
            }
            Ok(RemoteJobProgress::Queued {
                queue_deadline_ms: raw.queue_deadline_ms,
            })
        }
        RemoteJobState::Leased => {
            if raw.grace_until_ms.is_some() {
                return Err("remote_job state leased must not have grace_until_ms".to_string());
            }
            if !prior_exclusions && has_completion {
                return Err(
                    "remote_job state leased without excluded_nodes must not have completion fields"
                        .to_string(),
                );
            }
            let node_id =
                require_non_empty(raw.node_id.as_deref(), "node_id", "leased")?.to_string();
            let leased_at_ms = raw
                .leased_at_ms
                .ok_or_else(|| "remote_job state leased requires leased_at_ms".to_string())?;
            let lease_expires_ms = raw
                .lease_expires_ms
                .ok_or_else(|| "remote_job state leased requires lease_expires_ms".to_string())?;
            Ok(RemoteJobProgress::Leased {
                node_id,
                leased_at_ms,
                lease_expires_ms,
            })
        }
        RemoteJobState::ResultPending => {
            if !prior_exclusions && has_completion {
                return Err(
                    "remote_job state result_pending without excluded_nodes must not have completion fields"
                        .to_string(),
                );
            }
            let node_id =
                require_non_empty(raw.node_id.as_deref(), "node_id", "result_pending")?.to_string();
            let leased_at_ms = raw.leased_at_ms.ok_or_else(|| {
                "remote_job state result_pending requires leased_at_ms".to_string()
            })?;
            let lease_expires_ms = raw.lease_expires_ms.ok_or_else(|| {
                "remote_job state result_pending requires lease_expires_ms".to_string()
            })?;
            let grace_until_ms = raw.grace_until_ms.ok_or_else(|| {
                "remote_job state result_pending requires grace_until_ms".to_string()
            })?;
            Ok(RemoteJobProgress::ResultPending {
                node_id,
                leased_at_ms,
                lease_expires_ms,
                grace_until_ms,
            })
        }
        RemoteJobState::Completed => decode_completed_progress(raw),
        RemoteJobState::FallbackLocal | RemoteJobState::DeadLetter => {
            let label = match raw.state {
                RemoteJobState::FallbackLocal => "fallback_local",
                _ => "dead_letter",
            };
            forbid_active_lease_timers(raw, label)?;
            if raw.node_id.is_some() && !prior_exclusions {
                return Err(format!(
                    "remote_job state {label} with node_id requires non-empty excluded_nodes"
                ));
            }
            Ok(if raw.state == RemoteJobState::FallbackLocal {
                RemoteJobProgress::FallbackLocal
            } else {
                RemoteJobProgress::DeadLetter
            })
        }
        RemoteJobState::Cancelled => {
            if raw.node_id.is_some() {
                return Err("remote_job state cancelled must not have node_id".to_string());
            }
            forbid_active_lease_timers(raw, "cancelled")?;
            Ok(RemoteJobProgress::Cancelled)
        }
    }
}

fn decode_completed_progress(raw: &mut RawRemoteJob) -> Result<RemoteJobProgress, String> {
    forbid_active_lease_timers(raw, "completed")?;
    let node_id = require_non_empty(raw.node_id.as_deref(), "node_id", "completed")?.to_string();
    let finished_at_ms = raw
        .finished_at_ms
        .ok_or_else(|| "remote_job state completed requires finished_at_ms".to_string())?;
    let result = raw
        .result
        .take()
        .ok_or_else(|| "remote_job state completed requires result".to_string())?;
    if !result.is_object() {
        return Err("remote_job state completed requires result JSON object".to_string());
    }
    let result_sha256 =
        require_non_empty(raw.result_sha256.as_deref(), "result_sha256", "completed")?.to_string();
    let log_sha256 =
        require_non_empty(raw.log_sha256.as_deref(), "log_sha256", "completed")?.to_string();
    let log_bytes = raw
        .log_bytes
        .ok_or_else(|| "remote_job state completed requires log_bytes".to_string())?;
    if !(0..=MAX_REMOTE_LOG_BYTES).contains(&log_bytes) {
        return Err(format!(
            "remote_job state completed log_bytes {log_bytes} out of range 0..={MAX_REMOTE_LOG_BYTES}"
        ));
    }
    Ok(RemoteJobProgress::Completed {
        node_id,
        leased_at_ms: raw.leased_at_ms,
        completion: RemoteJobResult {
            finished_at_ms,
            exit_code: raw.exit_code,
            result,
            result_sha256,
            log_sha256,
            log_bytes,
        },
    })
}

impl TryFrom<RawRemoteJob> for RemoteJob {
    type Error = String;

    fn try_from(mut raw: RawRemoteJob) -> Result<Self, String> {
        if raw.attempts < 0 {
            return Err(format!(
                "remote_job attempts {} must be non-negative",
                raw.attempts
            ));
        }
        if raw.max_attempts < 1 {
            return Err(format!(
                "remote_job max_attempts {} must be at least 1",
                raw.max_attempts
            ));
        }
        if raw.lease_epoch < 0 {
            return Err(format!(
                "remote_job lease_epoch {} must be non-negative",
                raw.lease_epoch
            ));
        }
        if raw.node_id.as_deref() == Some("") {
            return Err("remote_job node_id must not be empty".to_string());
        }
        let progress = decode_remote_job_progress(&mut raw)?;
        Ok(Self {
            job_id: raw.job_id,
            cell_id: raw.cell_id,
            kind: raw.kind,
            priority: raw.priority,
            judging_id: raw.judging_id,
            mission_id: raw.mission_id,
            task_id: raw.task_id,
            spec: raw.spec,
            requires: raw.requires,
            cost: raw.cost,
            base_commit: raw.base_commit,
            excluded_nodes: raw.excluded_nodes,
            fault_nodes: raw.fault_nodes,
            lease_epoch: raw.lease_epoch,
            attempts: raw.attempts,
            max_attempts: raw.max_attempts,
            enqueued_at_ms: raw.enqueued_at_ms,
            progress,
        })
    }
}

/// A v1 node as the ledger holds it (its key and token digest left out).
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(try_from = "RawNodeV1")]
pub struct NodeV1 {
    /// Human-readable unique node name.
    pub name: String,
    /// SHA-256 of its Ed25519 key, in hex; none before enrollment.
    pub node_id: Option<String>,
    /// Scheduling cell identifier (`c0` by default).
    pub cell_id: String,
    /// `pending`, `active` or `revoked`.
    pub status: NodeStatus,
    /// Whether an administrator drained this node from receiving new leases.
    pub drained: bool,
    /// Three strikes: no grants until an undrain.
    pub degraded: bool,
    /// Whether the sweeper recorded a `node.lost` alert transition for the current outage.
    pub lost_recorded: bool,
    /// Consecutive host-fault strikes accumulated by this node.
    pub host_fault_strikes: i32,
    /// Node boot identifier sent on enrollment/heartbeat.
    pub boot_id: Option<String>,
    /// Last monotonic heartbeat sequence number observed.
    pub beat_seq: i64,
    /// Timestamp of the last heartbeat in epoch milliseconds.
    pub last_beat_ms: Option<i64>,
    /// SHA-256 hex digest of the node's advertised capability specification.
    pub spec_sha256: Option<String>,
    /// Node capability specification JSON object.
    pub spec: Option<Value>,
    /// Base commits cached warmly on this node.
    pub warm_bases: Vec<String>,
    /// Power/thermal telemetry reported in heartbeats, if available.
    #[serde(default)]
    pub power: Option<Value>,
    /// The commit the node's daemon was built at, from its last beat's
    /// `build_sha` (7 to 64 lowercase hex); none from a daemon that does
    /// not send it, or before its first beat.
    #[serde(default)]
    pub build_sha: Option<String>,
    /// The node's daemon version, from its last beat's `node_version`
    /// (printable ASCII, at most 64 bytes); none when not sent.
    #[serde(default)]
    pub node_version: Option<String>,
}

#[derive(Deserialize)]
struct RawNodeV1 {
    name: String,
    node_id: Option<String>,
    cell_id: String,
    status: NodeStatus,
    drained: bool,
    degraded: bool,
    lost_recorded: bool,
    host_fault_strikes: i32,
    boot_id: Option<String>,
    beat_seq: i64,
    last_beat_ms: Option<i64>,
    spec_sha256: Option<String>,
    spec: Option<Value>,
    warm_bases: Vec<String>,
    #[serde(default)]
    power: Option<Value>,
    #[serde(default)]
    build_sha: Option<String>,
    #[serde(default)]
    node_version: Option<String>,
}

impl TryFrom<RawNodeV1> for NodeV1 {
    type Error = String;

    fn try_from(raw: RawNodeV1) -> Result<Self, String> {
        if raw.status == NodeStatus::Active && raw.node_id.is_none() {
            return Err("active node missing node_id".to_string());
        }
        if raw.status == NodeStatus::Pending && raw.node_id.is_some() {
            return Err("pending node must not hold node_id".to_string());
        }
        if raw.host_fault_strikes < 0 {
            return Err(format!(
                "node host_fault_strikes {} must be non-negative",
                raw.host_fault_strikes
            ));
        }
        Ok(Self {
            name: raw.name,
            node_id: raw.node_id,
            cell_id: raw.cell_id,
            status: raw.status,
            drained: raw.drained,
            degraded: raw.degraded,
            lost_recorded: raw.lost_recorded,
            host_fault_strikes: raw.host_fault_strikes,
            boot_id: raw.boot_id,
            beat_seq: raw.beat_seq,
            last_beat_ms: raw.last_beat_ms,
            spec_sha256: raw.spec_sha256,
            spec: raw.spec,
            warm_bases: raw.warm_bases,
            power: raw.power,
            build_sha: raw.build_sha,
            node_version: raw.node_version,
        })
    }
}

/// The fields `nodes_v1`/`node_v1` read out of a node's last beat
/// (`spawn_node_state.beat`), each null unless well formed: `power` an
/// object, `build_sha` 7 to 64 lowercase hex, `node_version` 1 to 64
/// printable ASCII characters. The gateway refuses a beat with a malformed
/// one; the checks here keep a row written another way from reaching the
/// readers.
const NODE_V1_BEAT_FIELDS: &str = "jsonb_build_object(
        'power',
        case when jsonb_typeof(n.beat -> 'power') = 'object' then n.beat -> 'power' end,
        'build_sha',
        case when jsonb_typeof(n.beat -> 'build_sha') = 'string'
                  and n.beat ->> 'build_sha' ~ '^[0-9a-f]{7,64}$'
             then n.beat -> 'build_sha' end,
        'node_version',
        case when jsonb_typeof(n.beat -> 'node_version') = 'string'
                  and n.beat ->> 'node_version' ~ '^[\x20-\x7e]{1,64}$'
             then n.beat -> 'node_version' end)";

/// `POST /node/v1/enroll`'s call: the token's digest, the node's new key,
/// its boot id and its spec (hashed by the gateway).
#[derive(Clone, Copy, Debug)]
pub struct NodeEnroll<'a> {
    /// SHA-256 digest of the single-use enrollment token presented by the node.
    pub token_sha256: &'a [u8; 32],
    /// 32-byte Ed25519 public key to bind to the enrolled node.
    pub pubkey: &'a [u8; 32],
    /// Boot identifier reported by the enrolling node process.
    pub boot_id: &'a str,
    /// SHA-256 hex digest of `spec`.
    pub spec_sha256: &'a str,
    /// Node capability specification JSON payload.
    pub spec: &'a Value,
}

/// A gateway call's answer, and whether its transaction committed (the
/// caller's check of the answer passed).
#[derive(Clone, Debug, PartialEq)]
pub struct Committed<T> {
    /// Decoded SQL function response returned prior to signature verification.
    pub answer: T,
    /// True if the verify closure passed and the database transaction committed.
    pub committed: bool,
}

/// An owner's change of a v1 node (`spawn_node_admin`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeAction {
    /// Stop granting new remote jobs while allowing in-flight leases to drain.
    Drain,
    /// Also clears `degraded` and the strikes.
    Undrain,
    /// Fences everything it holds; the name is never enrolled again.
    Revoke,
}

impl NodeAction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Drain => "drain",
            Self::Undrain => "undrain",
            Self::Revoke => "revoke",
        }
    }
}

/// Where a job judged a node's fault goes (`spawn_remote_job_fault`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultNext {
    /// To another node (this one excluded), while attempts remain.
    Queued,
    /// Reassigned to run locally on the primary host.
    FallbackLocal,
    /// Exhausted all retry attempts; moved to terminal dead-letter state.
    DeadLetter,
    /// The row stays completed.
    Keep,
}

impl FaultNext {
    fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::FallbackLocal => "fallback_local",
            Self::DeadLetter => "dead_letter",
            Self::Keep => "keep",
        }
    }
}

/// Strike count recorded by a non-poison applied [`FaultOutcome`] (`>= 1`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FaultStrike {
    strikes: i32,
}

impl FaultStrike {
    /// Consecutive strikes required to mark a node degraded.
    pub const DEGRADED_THRESHOLD: i32 = 3;

    /// Construct a validated strike count (`strikes >= 1`).
    pub const fn new(strikes: i32) -> Option<Self> {
        if strikes >= 1 {
            Some(Self { strikes })
        } else {
            None
        }
    }

    /// Positive strike count after this fault was applied.
    pub const fn strikes(self) -> i32 {
        self.strikes
    }

    /// Whether this strike count crossed [`Self::DEGRADED_THRESHOLD`].
    pub const fn degraded(self) -> bool {
        self.strikes >= Self::DEGRADED_THRESHOLD
    }
}

/// Closed outcome of judging a completed remote job as a host fault (`spawn_remote_job_fault`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawFaultOutcome")]
pub enum FaultOutcome {
    /// Job was not completed at that lease epoch or had already been judged.
    NotApplied,
    /// Fault judging applied cleanly at the requested epoch.
    Applied {
        /// Where the job transitioned next (`queued`, `fallback_local`, `dead_letter`, or `keep`).
        next: FaultNext,
        /// True when a prior node already faulted on the same job (earlier strikes refunded).
        poison: bool,
        /// Strike state if this fault recorded a strike (`None` when `strike == false` or `poison`).
        strike: Option<FaultStrike>,
    },
}

impl FaultOutcome {
    /// Whether this fault judgement applied at the requested lease epoch.
    pub const fn is_applied(self) -> bool {
        matches!(self, Self::Applied { .. })
    }

    /// Destination state after an applied fault, or `None` when not applied.
    pub const fn next(self) -> Option<FaultNext> {
        match self {
            Self::NotApplied => None,
            Self::Applied { next, .. } => Some(next),
        }
    }

    /// Whether the fault was classified as a poison-pill job.
    pub const fn is_poison(self) -> bool {
        matches!(self, Self::Applied { poison: true, .. })
    }

    /// Strike state recorded for the faulted node, if any.
    pub const fn strike(self) -> Option<FaultStrike> {
        match self {
            Self::NotApplied => None,
            Self::Applied { strike, .. } => strike,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFaultOutcome {
    ok: bool,
    #[serde(default)]
    next: Option<FaultNext>,
    #[serde(default)]
    poison: Option<bool>,
    #[serde(default)]
    strikes: Option<i32>,
    #[serde(default)]
    degraded: Option<bool>,
}

impl TryFrom<RawFaultOutcome> for FaultOutcome {
    type Error = String;

    fn try_from(raw: RawFaultOutcome) -> Result<Self, String> {
        if !raw.ok {
            if raw.next.is_some()
                || raw.poison.is_some()
                || raw.strikes.is_some()
                || raw.degraded.is_some()
            {
                return Err(
                    "fault outcome ok=false must not carry next, poison, strikes, or degraded"
                        .to_string(),
                );
            }
            return Ok(Self::NotApplied);
        }

        let next = raw
            .next
            .ok_or_else(|| "fault outcome ok=true missing next".to_string())?;
        let poison = raw
            .poison
            .ok_or_else(|| "fault outcome ok=true missing poison".to_string())?;
        let degraded = raw
            .degraded
            .ok_or_else(|| "fault outcome ok=true missing degraded".to_string())?;

        if poison {
            if next == FaultNext::Queued {
                return Err("poison fault outcome must not transition to queued".to_string());
            }
            if raw.strikes.is_some() || degraded {
                return Err("poison fault outcome must not carry strikes or degraded".to_string());
            }
            return Ok(Self::Applied {
                next,
                poison: true,
                strike: None,
            });
        }

        if let Some(count) = raw.strikes {
            let strike = FaultStrike::new(count)
                .ok_or_else(|| format!("applied fault outcome strikes {count} must be positive"))?;
            if degraded != strike.degraded() {
                return Err(format!(
                    "fault outcome degraded={degraded} inconsistent with strikes={count}"
                ));
            }
            Ok(Self::Applied {
                next,
                poison: false,
                strike: Some(strike),
            })
        } else {
            if degraded {
                return Err(
                    "applied fault outcome without strikes must not report degraded=true"
                        .to_string(),
                );
            }
            Ok(Self::Applied {
                next,
                poison: false,
                strike: None,
            })
        }
    }
}

/// What a sweep did: leases now waiting for a result, leases fenced after
/// their grace, nodes recorded lost.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub struct SweepOutcome {
    /// Leases transitioned to `result_pending` awaiting grace completion.
    pub waiting: i64,
    /// Expired leases fenced after their grace period elapsed.
    pub fenced: i64,
    /// Nodes marked `lost` after missing their heartbeat threshold.
    pub lost: i64,
}

fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, StoreError> {
    serde_json::from_value(value).map_err(|error| StoreError::Backend(error.to_string()))
}

impl PostgresStore {
    /// One gateway function in a transaction of its own, verify-then-commit
    /// (design §4.2 step 7): `commit` sees the answer; true commits, false
    /// (a bad signature) rolls everything the call did back.
    fn gateway_call<T>(
        &self,
        sql: &str,
        params: &crate::Params<'_>,
        commit: impl FnOnce(&T) -> bool,
    ) -> Result<Committed<T>, StoreError>
    where
        T: for<'a> postgres::types::FromSql<'a>,
    {
        let mut client = self.lock();
        let mut transaction = client.transaction().map_err(|error| backend(&error))?;
        let answer: T = transaction
            .query_one(sql, params)
            .map_err(|error| backend(&error))?
            .get(0);
        let committed = commit(&answer);
        if committed {
            transaction.commit()
        } else {
            transaction.rollback()
        }
        .map_err(|error| backend(&error))?;
        Ok(Committed { answer, committed })
    }

    /// `POST /node/v1/enroll` (`spawn_node_gw_enroll`): consumes the token
    /// and pins the key. The answer: `{ok, name, node_id, cell_id,
    /// server_time_ms}` or `{ok: false, error}` (`bad_request`,
    /// `bad_token`, `key_in_use`).
    pub fn node_gw_enroll(
        &self,
        enroll: &NodeEnroll<'_>,
        commit: impl FnOnce(&Value) -> bool,
    ) -> Result<Committed<Value>, StoreError> {
        self.gateway_call(
            "select spawn_node_gw_enroll($1, $2, $3, $4, $5)",
            &[
                &enroll.token_sha256.as_slice(),
                &enroll.pubkey.as_slice(),
                &enroll.boot_id,
                &enroll.spec_sha256,
                enroll.spec,
            ],
            commit,
        )
    }

    /// `POST /node/v1/heartbeat` (`spawn_node_gw_beat`): renews, fences and
    /// grants (at most two a beat). The answer carries `pubkey` (standard
    /// base64) whenever the node is known, for the signature check.
    pub fn node_gw_beat(
        &self,
        node_id: &str,
        boot_id: &str,
        seq: i64,
        beat: &Value,
        commit: impl FnOnce(&Value) -> bool,
    ) -> Result<Committed<Value>, StoreError> {
        self.gateway_call(
            "select spawn_node_gw_beat($1, $2, $3, $4)",
            &[&node_id, &boot_id, &seq, beat],
            commit,
        )
    }

    /// `POST /node/v1/spec` (`spawn_node_gw_spec`): `{ok, changed, pubkey}`
    /// or `{ok: false, error}` (`beat_first` when the boot is not the
    /// node's current one).
    pub fn node_gw_spec(
        &self,
        node_id: &str,
        boot_id: &str,
        spec_sha256: &str,
        spec: &Value,
        commit: impl FnOnce(&Value) -> bool,
    ) -> Result<Committed<Value>, StoreError> {
        self.gateway_call(
            "select spawn_node_gw_spec($1, $2, $3, $4)",
            &[&node_id, &boot_id, &spec_sha256, spec],
            commit,
        )
    }

    /// `POST /node/v1/jobs/<job_id>/result` (`spawn_node_gw_result`): true
    /// when this call, or an identical earlier one, completed the job at
    /// `lease_epoch`; false when the lease moved (`409 lease_fenced`).
    pub fn node_gw_result(
        &self,
        node_id: &str,
        job_id: &str,
        lease_epoch: i64,
        result: &Value,
        commit: impl FnOnce(&bool) -> bool,
    ) -> Result<Committed<bool>, StoreError> {
        self.gateway_call(
            "select spawn_node_gw_result($1, $2, $3, $4)",
            &[&node_id, &job_id, &lease_epoch, result],
            commit,
        )
    }

    /// `spawn node enroll-token <name>` (owner only): store a one-time
    /// token's digest, good `ttl_ms` (1 s to 10 min). Returns when it
    /// expires. Refused for a P1-joined or a revoked name.
    pub fn node_enroll_token(
        &self,
        name: &str,
        cell_id: Option<&str>,
        token_sha256: &[u8; 32],
        ttl_ms: i64,
    ) -> Result<i64, StoreError> {
        Ok(self
            .query_one(
                "select spawn_node_enroll_token($1, $2, $3, $4)",
                &[&name, &cell_id, &token_sha256.as_slice(), &ttl_ms],
            )?
            .get(0))
    }

    /// `spawn node drain|undrain|revoke` of a v1 node (owner only): false
    /// when it was so already.
    pub fn node_admin(&self, name: &str, action: NodeAction) -> Result<bool, StoreError> {
        Ok(self
            .query_one(
                "select spawn_node_admin($1, $2)",
                &[&name, &action.as_str()],
            )?
            .get(0))
    }

    /// The level-triggered sweep (owner only), from the integrator's pass.
    pub fn node_sweep(&self) -> Result<SweepOutcome, StoreError> {
        decode(self.query_json("select spawn_node_sweep()", &[])?)
    }

    /// Queue a remote job (owner only). Returns its id.
    pub fn remote_job_enqueue(&self, job: &RemoteJobSpec) -> Result<String, StoreError> {
        let job =
            serde_json::to_value(job).map_err(|error| StoreError::Backend(error.to_string()))?;
        Ok(self
            .query_one("select spawn_remote_job_enqueue($1)", &[&job])?
            .get(0))
    }

    /// `FALLBACK_LOCAL` (owner only): take back a job still queued at
    /// `lease_epoch`. Of this and a node's grant, exactly one wins.
    pub fn remote_job_take_local(
        &self,
        job_id: &str,
        lease_epoch: i64,
    ) -> Result<bool, StoreError> {
        Ok(self
            .query_one(
                "select spawn_remote_job_take_local($1, $2)",
                &[&job_id, &lease_epoch],
            )?
            .get(0))
    }

    /// Judge a completed job's run the node's fault (owner only): a strike
    /// when `strike`, three make the node degraded; the poison rule gives
    /// strikes back. Judged once per completion.
    pub fn remote_job_fault(
        &self,
        job_id: &str,
        lease_epoch: i64,
        evidence: &str,
        strike: bool,
        next: FaultNext,
    ) -> Result<FaultOutcome, StoreError> {
        decode(self.query_json(
            "select spawn_remote_job_fault($1, $2, $3, $4, $5)",
            &[&job_id, &lease_epoch, &evidence, &strike, &next.as_str()],
        )?)
    }

    /// Judge a completed job's run valid (owner only): the node's strikes
    /// go back to 0, `degraded` stays (only an undrain clears it). Judged
    /// once per completion: false when the job is not completed at
    /// `lease_epoch`, was judged already, or its node was excluded.
    pub fn remote_job_valid(&self, job_id: &str, lease_epoch: i64) -> Result<bool, StoreError> {
        let outcome = self.query_json(
            "select spawn_remote_job_valid($1, $2)",
            &[&job_id, &lease_epoch],
        )?;
        Ok(outcome.get("ok").and_then(Value::as_bool).unwrap_or(false))
    }

    /// One remote job, or `None`.
    pub fn remote_job(&self, job_id: &str) -> Result<Option<RemoteJob>, StoreError> {
        self.query_opt_json(
            "select to_jsonb(j) from spawn_remote_jobs j where j.job_id = $1",
            &[&job_id],
        )?
        .map(decode)
        .transpose()
    }

    /// Every v1 node, by name (their keys and token digests left out):
    /// `spawn node list` beside the P1 nodes (PR S4).
    pub fn nodes_v1(&self) -> Result<Vec<NodeV1>, StoreError> {
        decode(self.query_json(
            &format!(
                "select coalesce(jsonb_agg(
                                  (to_jsonb(n) - 'pubkey' - 'enroll_token_sha256') || {NODE_V1_BEAT_FIELDS}
                                  order by n.name), '[]'::jsonb)
                   from spawn_node_state n"
            ),
            &[],
        )?)
    }

    /// The jobs of `kind` still queued, oldest first: the integrator takes
    /// back the shadows a predecessor queued and no longer waits for
    /// (PR S4).
    pub fn remote_jobs_queued(&self, kind: RemoteJobKind) -> Result<Vec<RemoteJob>, StoreError> {
        let kind =
            serde_json::to_value(kind).map_err(|error| StoreError::Backend(error.to_string()))?;
        let kind = kind.as_str().unwrap_or_default().to_string();
        decode(self.query_json(
            "select coalesce(jsonb_agg(to_jsonb(j) order by j.enqueued_at_ms, j.job_id),
                             '[]'::jsonb)
               from spawn_remote_jobs j where j.state = 'queued' and j.kind = $1",
            &[&kind],
        )?)
    }

    /// One v1 node by name, or `None`.
    pub fn node_v1(&self, name: &str) -> Result<Option<NodeV1>, StoreError> {
        self.query_opt_json(
            &format!(
                "select (to_jsonb(n) - 'pubkey' - 'enroll_token_sha256') || {NODE_V1_BEAT_FIELDS}
                   from spawn_node_state n where n.name = $1"
            ),
            &[&name],
        )?
        .map(decode)
        .transpose()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{
        FaultNext, FaultOutcome, FaultStrike, NodeStatus, NodeV1, RemoteJob, RemoteJobProgress,
        RemoteJobState,
    };

    fn base_node_json() -> Value {
        json!({
            "name": "n252",
            "node_id": "a".repeat(64),
            "cell_id": "c0",
            "status": "active",
            "drained": false,
            "degraded": false,
            "lost_recorded": false,
            "host_fault_strikes": 0,
            "boot_id": "0".repeat(32),
            "beat_seq": 1,
            "last_beat_ms": 1_000_000,
            "spec_sha256": "b".repeat(64),
            "spec": null,
            "warm_bases": [],
            "power": null,
            "build_sha": null,
            "node_version": null,
            "seq_hi": 1
        })
    }

    fn base_job_json() -> Value {
        json!({
            "job_id": "gate-00000001",
            "cell_id": "c0",
            "kind": "gate",
            "state": "queued",
            "priority": 0,
            "judging_id": null,
            "mission_id": 1,
            "task_id": 2,
            "spec": {},
            "requires": {},
            "cost": {},
            "base_commit": null,
            "excluded_nodes": [],
            "fault_nodes": [],
            "node_id": null,
            "boot_id": null,
            "lease_epoch": 0,
            "lease_expires_ms": null,
            "grace_until_ms": null,
            "grant_seq": null,
            "attempts": 0,
            "max_attempts": 2,
            "enqueued_at_ms": 1_000,
            "queue_deadline_ms": 31_000,
            "leased_at_ms": null,
            "finished_at_ms": null,
            "exit_code": null,
            "result": null,
            "result_sha256": null,
            "log_sha256": null,
            "log_bytes": null
        })
    }

    #[test]
    fn node_status_serde_and_node_v1_invariants() {
        for (status, wire) in [
            (NodeStatus::Pending, "\"pending\""),
            (NodeStatus::Active, "\"active\""),
            (NodeStatus::Revoked, "\"revoked\""),
        ] {
            assert_eq!(serde_json::to_string(&status).unwrap(), wire);
            assert_eq!(serde_json::from_str::<NodeStatus>(wire).unwrap(), status);
            assert_eq!(format!("\"{}\"", status.as_str()), wire);
        }

        let active: NodeV1 = serde_json::from_value(base_node_json()).unwrap();
        assert_eq!(active.status, NodeStatus::Active);

        let mut pending_json = base_node_json();
        pending_json["status"] = json!("pending");
        pending_json["node_id"] = Value::Null;
        assert_eq!(
            serde_json::from_value::<NodeV1>(pending_json)
                .unwrap()
                .status,
            NodeStatus::Pending
        );

        let mut revoked_unenrolled = base_node_json();
        revoked_unenrolled["status"] = json!("revoked");
        revoked_unenrolled["node_id"] = Value::Null;
        assert_eq!(
            serde_json::from_value::<NodeV1>(revoked_unenrolled)
                .unwrap()
                .status,
            NodeStatus::Revoked
        );

        for (field, val) in [
            ("status", json!("lost")),
            ("node_id", Value::Null),
            ("host_fault_strikes", json!(-1)),
        ] {
            let mut bad = base_node_json();
            bad[field] = val;
            assert!(serde_json::from_value::<NodeV1>(bad).is_err(), "{field}");
        }

        let mut pending_with_id = base_node_json();
        pending_with_id["status"] = json!("pending");
        assert!(serde_json::from_value::<NodeV1>(pending_with_id).is_err());
    }

    #[test]
    fn fault_outcome_decodes_closed_variants_and_rejects_contradictions() {
        assert_eq!(
            serde_json::from_value::<FaultOutcome>(json!({"ok": false})).unwrap(),
            FaultOutcome::NotApplied
        );
        assert_eq!(
            serde_json::from_value::<FaultOutcome>(json!({
                "ok": true, "next": "queued", "poison": false, "strikes": 1, "degraded": false
            }))
            .unwrap(),
            FaultOutcome::Applied {
                next: FaultNext::Queued,
                poison: false,
                strike: FaultStrike::new(1),
            }
        );
        assert_eq!(
            serde_json::from_value::<FaultOutcome>(json!({
                "ok": true, "next": "fallback_local", "poison": false, "strikes": 3, "degraded": true
            }))
            .unwrap(),
            FaultOutcome::Applied {
                next: FaultNext::FallbackLocal,
                poison: false,
                strike: FaultStrike::new(3),
            }
        );
        assert_eq!(
            serde_json::from_value::<FaultOutcome>(json!({
                "ok": true, "next": "fallback_local", "poison": true, "strikes": null, "degraded": false
            }))
            .unwrap(),
            FaultOutcome::Applied {
                next: FaultNext::FallbackLocal,
                poison: true,
                strike: None,
            }
        );
        assert_eq!(
            serde_json::from_value::<FaultOutcome>(json!({
                "ok": true, "next": "keep", "poison": false, "strikes": null, "degraded": false
            }))
            .unwrap(),
            FaultOutcome::Applied {
                next: FaultNext::Keep,
                poison: false,
                strike: None,
            }
        );

        for invalid in [
            json!({"ok": false, "next": "queued"}),
            json!({"ok": false, "poison": false}),
            json!({"ok": false, "strikes": 1}),
            json!({"ok": false, "degraded": true}),
            json!({"ok": true, "next": "queued", "poison": false}),
            json!({"ok": true, "next": "queued", "poison": true, "strikes": null, "degraded": false}),
            json!({"ok": true, "next": "fallback_local", "poison": true, "strikes": 1, "degraded": false}),
            json!({"ok": true, "next": "fallback_local", "poison": true, "strikes": null, "degraded": true}),
            json!({"ok": true, "next": "queued", "poison": false, "strikes": 0, "degraded": false}),
            json!({"ok": true, "next": "queued", "poison": false, "strikes": 2, "degraded": true}),
            json!({"ok": true, "next": "queued", "poison": false, "strikes": 3, "degraded": false}),
            json!({"ok": true, "next": "queued", "poison": false, "strikes": null, "degraded": true}),
            json!({"ok": false, "unexpected": 1}),
        ] {
            assert!(
                serde_json::from_value::<FaultOutcome>(invalid.clone()).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn remote_job_decodes_all_valid_states() {
        let queued: RemoteJob = serde_json::from_value(base_job_json()).unwrap();
        assert_eq!(queued.state(), RemoteJobState::Queued);
        assert!(matches!(
            queued.progress,
            RemoteJobProgress::Queued {
                queue_deadline_ms: Some(31_000)
            }
        ));

        let mut leased = base_job_json();
        leased["state"] = json!("leased");
        leased["node_id"] = json!("n1");
        leased["leased_at_ms"] = json!(2_000);
        leased["lease_expires_ms"] = json!(92_000);
        leased["lease_epoch"] = json!(1);
        leased["attempts"] = json!(1);
        let decoded: RemoteJob = serde_json::from_value(leased.clone()).unwrap();
        assert_eq!(decoded.node_id(), Some("n1"));

        leased["state"] = json!("result_pending");
        leased["grace_until_ms"] = json!(272_000);
        assert_eq!(
            serde_json::from_value::<RemoteJob>(leased).unwrap().state(),
            RemoteJobState::ResultPending
        );

        let mut completed = base_job_json();
        completed["state"] = json!("completed");
        completed["node_id"] = json!("n1");
        completed["finished_at_ms"] = json!(5_000);
        completed["result"] = json!({"exit_code": null});
        completed["result_sha256"] = json!("c".repeat(64));
        completed["log_sha256"] = json!("d".repeat(64));
        completed["log_bytes"] = json!(0);
        let done: RemoteJob = serde_json::from_value(completed.clone()).unwrap();
        assert_eq!(done.completion().unwrap().exit_code, None);

        // Fault re-queued with residual prior completion columns stays valid when excluded_nodes is set.
        let mut requeued = completed.clone();
        requeued["state"] = json!("queued");
        requeued["node_id"] = Value::Null;
        requeued["excluded_nodes"] = json!(["n1"]);
        assert_eq!(
            serde_json::from_value::<RemoteJob>(requeued)
                .unwrap()
                .state(),
            RemoteJobState::Queued
        );

        let mut local = base_job_json();
        local["state"] = json!("fallback_local");
        local["finished_at_ms"] = json!(6_000);
        assert_eq!(
            serde_json::from_value::<RemoteJob>(local).unwrap().state(),
            RemoteJobState::FallbackLocal
        );

        let mut dead = completed;
        dead["state"] = json!("dead_letter");
        dead["excluded_nodes"] = json!(["n1"]);
        assert_eq!(
            serde_json::from_value::<RemoteJob>(dead).unwrap().state(),
            RemoteJobState::DeadLetter
        );

        let mut cancelled = base_job_json();
        cancelled["state"] = json!("cancelled");
        assert_eq!(
            serde_json::from_value::<RemoteJob>(cancelled)
                .unwrap()
                .state(),
            RemoteJobState::Cancelled
        );
    }

    #[test]
    fn remote_job_rejects_illegal_cross_field_rows() {
        let mut q_node = base_job_json();
        q_node["node_id"] = json!("n1");
        let mut q_exp = base_job_json();
        q_exp["lease_expires_ms"] = json!(10);
        let mut q_res = base_job_json();
        q_res["result"] = json!({});

        let mut l_no_node = base_job_json();
        l_no_node["state"] = json!("leased");
        l_no_node["leased_at_ms"] = json!(2_000);
        l_no_node["lease_expires_ms"] = json!(92_000);

        let mut l_grace = l_no_node.clone();
        l_grace["node_id"] = json!("n1");
        l_grace["grace_until_ms"] = json!(100_000);

        let mut rp_no_grace = l_no_node.clone();
        rp_no_grace["state"] = json!("result_pending");
        rp_no_grace["node_id"] = json!("n1");

        let mut c_no_sha = base_job_json();
        c_no_sha["state"] = json!("completed");
        c_no_sha["node_id"] = json!("n1");
        c_no_sha["finished_at_ms"] = json!(5_000);
        c_no_sha["result"] = json!({});
        c_no_sha["log_sha256"] = json!("d".repeat(64));
        c_no_sha["log_bytes"] = json!(0);

        let mut c_timer = c_no_sha.clone();
        c_timer["result_sha256"] = json!("c".repeat(64));
        c_timer["lease_expires_ms"] = json!(9_000);

        let mut fb_node_unexcluded = base_job_json();
        fb_node_unexcluded["state"] = json!("fallback_local");
        fb_node_unexcluded["node_id"] = json!("n1");

        let mut cancelled_node = base_job_json();
        cancelled_node["state"] = json!("cancelled");
        cancelled_node["node_id"] = json!("n1");

        for (label, row) in [
            ("queued with node_id", q_node),
            ("queued with lease_expires_ms", q_exp),
            ("queued unexcluded with result", q_res),
            ("leased without node_id", l_no_node),
            ("leased with grace_until_ms", l_grace),
            ("result_pending without grace_until_ms", rp_no_grace),
            ("completed missing result_sha256", c_no_sha),
            ("completed with lease_expires_ms", c_timer),
            ("fallback_local with unexcluded node", fb_node_unexcluded),
            ("cancelled with node_id", cancelled_node),
        ] {
            assert!(serde_json::from_value::<RemoteJob>(row).is_err(), "{label}");
        }
    }
}
