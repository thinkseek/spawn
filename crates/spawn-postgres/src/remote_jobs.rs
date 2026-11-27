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

/// A remote job as the ledger holds it.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct RemoteJob {
    /// Unique job identifier (`spawn_remote_jobs.job_id`).
    pub job_id: String,
    /// Scheduling cell identifier (`c0` by default).
    pub cell_id: String,
    /// Kind of work performed (`gate`, `gate_shadow`, `build`, `de_batch`).
    pub kind: RemoteJobKind,
    /// Current lifecycle state in the ledger.
    pub state: RemoteJobState,
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
    /// The holder, while leased or pending (and the finisher, once done).
    pub node_id: Option<String>,
    /// Moves at every grant and every fence: only the current epoch's
    /// result is believed.
    pub lease_epoch: i64,
    /// Timestamp when the current lease expires, in epoch milliseconds.
    pub lease_expires_ms: Option<i64>,
    /// Timestamp until which a late result is accepted after lease expiry.
    pub grace_until_ms: Option<i64>,
    /// Total execution attempts used so far.
    pub attempts: i32,
    /// Maximum attempts permitted before fallback or dead-lettering.
    pub max_attempts: i32,
    /// Timestamp when the job was first enqueued, in epoch milliseconds.
    pub enqueued_at_ms: i64,
    /// Queue deadline timestamp in epoch milliseconds, if bounded.
    pub queue_deadline_ms: Option<i64>,
    /// Timestamp when the current attempt was leased, in epoch milliseconds.
    pub leased_at_ms: Option<i64>,
    /// Completion timestamp in epoch milliseconds, once finished.
    pub finished_at_ms: Option<i64>,
    /// Remote process exit code, once finished.
    pub exit_code: Option<i32>,
    /// The node's result metadata, as it sent it.
    pub result: Option<Value>,
    /// SHA-256 hex digest of the result artifact, if uploaded.
    pub result_sha256: Option<String>,
    /// SHA-256 hex digest of the captured log artifact, if uploaded.
    pub log_sha256: Option<String>,
    /// Byte length of the captured remote log, if known.
    pub log_bytes: Option<i64>,
}

/// A v1 node as the ledger holds it (its key and token digest left out).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct NodeV1 {
    /// Human-readable unique node name.
    pub name: String,
    /// SHA-256 of its Ed25519 key, in hex; none before enrollment.
    pub node_id: Option<String>,
    /// Scheduling cell identifier (`c0` by default).
    pub cell_id: String,
    /// `pending`, `active` or `revoked`.
    pub status: String,
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

/// What a fault judging did: `ok` false when the job is not completed at
/// that epoch, or was judged already.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct FaultOutcome {
    /// Whether the fault transition applied cleanly at the requested epoch.
    pub ok: bool,
    /// Where the job went (the poison rule and the attempts may override
    /// the asked one).
    #[serde(default)]
    pub next: Option<FaultNext>,
    /// The job faulted on another node before: the cause, not they.
    #[serde(default)]
    pub poison: Option<bool>,
    /// The node's strikes after a strike.
    #[serde(default)]
    pub strikes: Option<i32>,
    /// Whether the node crossed the strike limit and is now marked degraded.
    #[serde(default)]
    pub degraded: Option<bool>,
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
