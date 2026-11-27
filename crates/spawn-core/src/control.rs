//! Strongly-typed `ControlRecord` ADT, strict/compatible parsers, and the
//! pure RSI Episode Replay engine over `session_control` records.
//!
//! SQL (`crates/spawn-postgres/sql/schema.sql`) is the source of truth:
//! - [`ControlRecord`] covers every `control.recorded` `'type'` folded or
//!   accepted by `spawn_control_apply` / `spawn_control_record` (67 variants).
//! - [`GradingRecord`] covers every `grading.recorded` `'type'` folded by
//!   `spawn_grading_apply` / `spawn_grading_record` (`preset.*`, 5 variants).
//! - Spawn's restore drills are emitted under `backup.drilled.*`; there are no
//!   separate `restore.*` event types in `schema.sql`.
//!
//! `spawn-postgres` pins [`ControlRecord::ALL_KINDS`] and
//! [`GradingRecord::ALL_KINDS`] bidirectionally against `sql/schema.sql` in
//! `crates/spawn-postgres/tests/control_records.rs`.
//!
//! This module performs no IO and uses zero `unsafe` code.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::mission::TaskState;

/// Wire grade rank (`l1`..`l3`) carried on task assignments, reroutes, and
/// grading records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GradeWire {
    /// Entry-level deterministic tier (`"l1"`, rank `1`).
    L1 = 1,
    /// Standard mainline tier (`"l2"`, rank `2`).
    L2 = 2,
    /// Strategic decision / highest-capability tier (`"l3"`, rank `3`).
    L3 = 3,
}

impl GradeWire {
    /// Wire name (`"l1"`, `"l2"`, or `"l3"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::L1 => "l1",
            Self::L2 => "l2",
            Self::L3 => "l3",
        }
    }

    /// Integer rank (`1..=3`), matching `mission_tasks.difficulty`.
    #[must_use]
    pub const fn rank(self) -> i16 {
        self as i16
    }

    /// Parse either `"l1".."l3"` or historical numeric `"1".."3"`.
    fn parse_wire(raw: &str) -> Option<Self> {
        match raw {
            "l1" | "1" => Some(Self::L1),
            "l2" | "2" => Some(Self::L2),
            "l3" | "3" => Some(Self::L3),
            _ => None,
        }
    }

    /// Construct from an integer rank (`1..=3`).
    pub const fn from_rank(rank: i64) -> Option<Self> {
        match rank {
            1 => Some(Self::L1),
            2 => Some(Self::L2),
            3 => Some(Self::L3),
            _ => None,
        }
    }
}

impl fmt::Display for GradeWire {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

fn deserialize_opt_grade_wire<'de, D>(deserializer: D) -> Result<Option<GradeWire>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Option::<Value>::deserialize(deserializer)?;
    match raw {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => GradeWire::parse_wire(&text)
            .map(Some)
            .ok_or_else(|| D::Error::custom(format!("invalid wire grade {text:?}"))),
        Some(Value::Number(num)) => num
            .as_i64()
            .and_then(GradeWire::from_rank)
            .map(Some)
            .ok_or_else(|| D::Error::custom(format!("invalid numeric grade {num}"))),
        Some(other) => Err(D::Error::custom(format!(
            "expected grade wire, got {other}"
        ))),
    }
}

/// Deserialize an optional list of `i64` IDs accepting either JSON integers
/// (`[1, 2]`, e.g. from `spawn_integrator_record`) or base-10 integer strings
/// (`["1", "2"]`, e.g. from `spawn_source_moved` via `jsonb_array_elements_text`).
fn deserialize_opt_vec_i64<'de, D>(deserializer: D) -> Result<Option<Vec<i64>>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Option::<Vec<Value>>::deserialize(deserializer)?;
    let Some(items) = raw else {
        return Ok(None);
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let id = match item {
            Value::Number(num) => num.as_i64().ok_or_else(|| {
                D::Error::custom(format!("expected integer mission id, got {num}"))
            })?,
            Value::String(text) => text.trim().parse::<i64>().map_err(|_| {
                D::Error::custom(format!("expected integer string mission id, got {text:?}"))
            })?,
            other => {
                return Err(D::Error::custom(format!(
                    "expected integer or numeric string id, got {other}"
                )));
            }
        };
        out.push(id);
    }
    Ok(Some(out))
}

/// Worker kind on `task.assigned` (`resident` or `ephemeral`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    /// Persistent named resident worker holding charter state across tasks.
    Resident,
    /// Single-task ephemeral worker spawned for one assignment.
    Ephemeral,
}

impl TaskKind {
    /// Wire string representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Resident => "resident",
            Self::Ephemeral => "ephemeral",
        }
    }
}

/// Terminal state of a closed mission (`done` or `dropped`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionCloseState {
    /// Mission completed cleanly (`done`).
    Done,
    /// Mission abandoned or cancelled (`dropped`).
    Dropped,
}

/// How a gated mission accepts tasks (`review` or `gate`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptMode {
    /// Accepted by human/coordinator review before landing.
    #[default]
    Review,
    /// Requires integrator gate acceptance and landing onto base branch.
    Gate,
}

/// Integrator gate verdict on `task.gated` and `task.shadow_gated`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateVerdict {
    /// Gate scenarios passed and commit is eligible to land.
    Accepted,
    /// Code/behavior checks failed (`code` fault).
    Rejected,
    /// Rebase or merge conflict against current main (`code` fault).
    Conflict,
    /// Candidate head incompatible with baseline (`code` fault).
    Incompatible,
    /// Gate run was aborted before reaching a code verdict (`infrastructure` fault).
    Aborted,
    /// Candidate became obsolete before gate completion (`superseded`).
    Stale,
    /// Host ran out of disk or temporary workspace capacity (`infrastructure` fault).
    NoRoom,
    /// Gate runner or host infrastructure failed (`infrastructure` fault).
    HostFault,
}

impl GateVerdict {
    /// SQL wire value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::Conflict => "conflict",
            Self::Incompatible => "incompatible",
            Self::Aborted => "aborted",
            Self::Stale => "stale",
            Self::NoRoom => "no_room",
            Self::HostFault => "host_fault",
        }
    }
}

/// Responsibility class of a gate verdict (`code`, `infrastructure`, `superseded`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateVerdictClass {
    /// Verdict reflects the submitted code change itself.
    Code,
    /// Verdict reflects runner, host, or storage infrastructure conditions.
    Infrastructure,
    /// Submission was superseded by newer state on main or a newer task.
    Superseded,
}

/// Reason category for `objective.retired`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ObjectiveRetireKind {
    /// Objective was already fulfilled on `main`.
    #[serde(rename = "on-main")]
    OnMain,
    /// Objective was merged/renumbered into another objective ID.
    #[serde(rename = "renumbered")]
    Renumbered,
    /// Objective is no longer relevant or applicable.
    #[serde(rename = "obsolete")]
    Obsolete,
}

/// Result of `objective.verified` (`pass` or `fail`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationResult {
    /// Live environment verification succeeded.
    Pass,
    /// Live environment verification failed.
    Fail,
}

/// Originator of `deploy.finished` (`landing` or `owner`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeployBy {
    /// Automated post-landing deploy pipeline.
    Landing,
    /// Manual deploy initiated by the operator/owner.
    Owner,
}

/// Human verdict on `alert.judged`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertVerdict {
    /// Alert accurately identified a real problem.
    Right,
    /// Alert was a false positive.
    False,
    /// Alert described a condition already resolved before review.
    Stale,
    /// Alert duplicated an existing active alert.
    Duplicate,
}

/// Threshold crossing state on `host.disk`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostDiskState {
    /// Free disk bytes crossed below configured warning/floor threshold.
    Low,
    /// Free disk bytes recovered above thresholds.
    Recovered,
}

/// Health crossing state on `host.judge`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostJudgeState {
    /// Semantic judge endpoint failed calls and transitioned down.
    Down,
    /// Semantic judge endpoint resumed healthy responses.
    Up,
}

/// Execution interruption causes allowed by `spawn_host_record`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptionCause {
    /// Engine process exited normally or unexpectedly mid-task.
    Exited,
    /// Engine process crashed or terminated with fatal signal.
    Crashed,
    /// Engine execution hit cgroup/host memory limit.
    MemoryLimit,
    /// Engine context window was exhausted.
    ContextExhausted,
    /// Configured model endpoint was unavailable.
    ModelUnavailable,
    /// Engine authentication credentials expired or were rejected.
    ModelAuth,
    /// Model provider returned a runtime API error.
    ModelError,
    /// Host or daemon restarted underneath running executions.
    HostRestart,
    /// Backing PostgreSQL database was unavailable.
    DatabaseUnavailable,
    /// Host disk or temporary volume filled up.
    DiskFull,
    /// Task reached budget or elapsed-time limit.
    Budget,
    /// Operator or loop stopped the execution.
    Stopped,
    /// Execution was explicitly restarted.
    Restarted,
    /// Task was reassigned to another worker.
    Reassigned,
}

/// Recovery paths allowed by `spawn_host_record`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPath {
    /// Resume the existing native session cleanly.
    Resume,
    /// Hand off work into a fresh session/continuation.
    Handoff,
    /// Hold and wait for external/human remediation before retrying.
    Wait,
    /// Terminal state with no automatic recovery.
    Final,
}

/// Storage area on `backup.stored`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupArea {
    /// Archived native session transcripts directory.
    Transcripts,
    /// Session snapshots and metadata directory.
    Sessions,
}

/// Table imported by `legacy.imported`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LegacyTable {
    /// Imported `mission_missions` row.
    #[serde(rename = "mission_missions")]
    MissionMissions,
    /// Imported `mission_residents` row.
    #[serde(rename = "mission_residents")]
    MissionResidents,
    /// Imported `mission_tasks` row.
    #[serde(rename = "mission_tasks")]
    MissionTasks,
}

/// Error returned when parsing or strictly validating a control/grading record.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ControlRecordError {
    /// Input JSON value was not an object.
    #[error("control record must be a JSON object")]
    NotAnObject,
    /// JSON object lacked a string `"type"` field.
    #[error("control record is missing string field `type`")]
    MissingType,
    /// `"type"` named a record kind unknown to this schema version.
    #[error("unknown control record type `{0}`")]
    UnknownKind(String),
    /// Typed serde decoding failed for the identified record kind.
    #[error("invalid payload for `{kind}`: {message}")]
    Deserialize {
        /// Parsed `"type"` name.
        kind: String,
        /// Serde error explanation.
        message: String,
    },
    /// Decoded record violated a SQL/domain invariant under strict parsing.
    #[error("invariant violation for `{kind}`: {message}")]
    Invariant {
        /// Parsed `"type"` name.
        kind: String,
        /// Violated invariant description.
        message: String,
    },
    /// Closed-schema record carried unknown keys in strict mode.
    #[error("unexpected extra fields for fixed-schema `{kind}`: {fields:?}")]
    UnexpectedFields {
        /// Parsed `"type"` name.
        kind: String,
        /// Sorted unexpected top-level field keys.
        fields: Vec<String>,
    },
    /// Failed to serialize a control/grading record back to JSON.
    #[error("failed to serialize control record: {0}")]
    Serialize(String),
}

// ---------------------------------------------------------------------------
// ControlRecord payloads (67 variants)
// ---------------------------------------------------------------------------

/// Payload for `mission.added`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissionAddedRecord {
    /// Assigned mission ID.
    pub id: i64,
    /// Mission goal text.
    pub text: String,
    /// Optional origin caller or source label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// Source repository names attached to the mission.
    #[serde(default)]
    pub sources: Vec<String>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `mission.configured`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissionConfiguredRecord {
    /// Mission ID being configured.
    pub id: i64,
    /// Optional gate script or configuration path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<String>,
    /// Optional acceptance mode (`review` or `gate`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accept: Option<AcceptMode>,
    /// Optional JSON budget configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Value>,
    /// Optional structured objectives JSON specification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objectives: Option<Value>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `mission.closed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissionClosedRecord {
    /// Mission ID being closed.
    pub id: i64,
    /// Terminal mission state (`done` or `dropped`).
    pub state: MissionCloseState,
    /// Optional closure summary text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `mission.halted`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissionHaltedRecord {
    /// Mission ID that halted.
    pub id: i64,
    /// Optional halt reason message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Optional machine-readable halt cause.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<String>,
    /// Optional source repository implicated in the halt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Optional previous head commit/ref on `source`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Optional new head commit/ref on `source`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// Optional branches/items reported missing or gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gone: Option<Vec<String>>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `mission.resumed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissionResumedRecord {
    /// Mission ID being resumed.
    pub id: i64,
    /// Optional actor resuming the mission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// Optional halt event sequence ID cleared by this resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub halt: Option<i64>,
    /// Optional idempotency/alert key cleared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `mission.paused`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissionPausedRecord {
    /// Mission ID being paused.
    pub id: i64,
    /// Optional human-readable pause reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `mission.unpaused`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissionUnpausedRecord {
    /// Mission ID being unpaused.
    pub id: i64,
    /// Optional sequence ID of the prior pause event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_event: Option<i64>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `task.assigned`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskAssignedRecord {
    /// Assigned task ID.
    pub id: i64,
    /// Parent mission ID.
    pub mission_id: i64,
    /// Worker kind (`resident` or `ephemeral`).
    pub kind: TaskKind,
    /// Assigned agent identifier.
    pub agent_id: String,
    /// Spawn session identifier assigned to run the task.
    pub session_id: String,
    /// Task brief text.
    pub brief: String,
    /// Optional requested model profile wire string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Optional creator/assigner label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    /// Optional numeric objective index within the mission.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objective: Option<i32>,
    /// Optional scenario name targeted by this task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scenario: Option<String>,
    /// Optional prior task ID if this task reissues an earlier one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reissue_of: Option<i64>,
    /// Optional prior task ID whose branch/work this task continues.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continues: Option<i64>,
    /// Optional prior task ID reassigned onto this task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reassign_of: Option<i64>,
    /// Optional task ID this assignment duplicates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duplicate_of: Option<i64>,
    /// Whether this task is pinned against automatic reassignment/stealing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned: Option<bool>,
    /// Classified or explicitly assigned task difficulty rank.
    #[serde(
        default,
        deserialize_with = "deserialize_opt_grade_wire",
        skip_serializing_if = "Option::is_none"
    )]
    pub difficulty: Option<GradeWire>,
    /// Routing preset ID chosen for this task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset_id: Option<String>,
    /// Initial preset ID first assigned to the task (calibration attribution).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_preset_id: Option<String>,
    /// Cumulative quality rejection count carried onto the task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality_rejections: Option<u32>,
    /// Optional execution profile name carried from the preset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_profile: Option<String>,
    /// Optional structured difficulty classification metadata JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classification: Option<Value>,
    /// Optional engine label from the routed preset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
    /// Optional model label from the routed preset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_label: Option<String>,
    /// Optional top-down tier role (`mechanical-offload`, `mainline`, or `top-decision`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier_role: Option<String>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `task.delivered`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskDeliveredRecord {
    /// Task ID whose assignment command was delivered.
    pub id: i64,
    /// Delivered command ID.
    pub command_id: i64,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `task.reported`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskReportedRecord {
    /// Task ID being reported.
    pub id: i64,
    /// Structured or textual completion report submitted by the worker.
    pub report: String,
    /// Optional candidate commit SHA associated with the report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `task.report_unfinished`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskReportUnfinishedRecord {
    /// Task ID whose report had unfinished markers.
    pub id: i64,
    /// Report revision rejected by the unfinished-marker check.
    pub revision: u32,
    /// Digest of the rejected report text.
    pub digest: String,
    /// Detected unfinished-marker evidence JSON.
    pub markers: Value,
    /// Feedback note sent back to the worker.
    pub note: String,
    /// Command ID used to deliver the feedback note.
    pub command_id: i64,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `task.fork_failed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskForkFailedRecord {
    /// New task ID whose workspace fork failed.
    pub id: i64,
    /// Prior task ID from which the fork was attempted.
    pub from: i64,
    /// Base commit SHA that failed to check out/fork.
    pub commit: String,
    /// Error description from the workspace adapter.
    pub error: String,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `task.reviewed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskReviewedRecord {
    /// Reviewed task ID.
    pub id: i64,
    /// Whether the review accepted (`true`) or sent the task back (`false`).
    pub accepted: bool,
    /// Optional reviewer feedback note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Optional report revision targeted by this review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u32>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `task.rerouted`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskReroutedRecord {
    /// Rerouted task ID.
    pub id: i64,
    /// Newly selected preset identifier.
    pub preset_id: String,
    /// Model profile string selected by the reroute.
    pub model: String,
    /// Previous preset identifier replaced by `preset_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_preset_id: Option<String>,
    /// Optional report revision triggering the reroute.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u32>,
    /// Stepped difficulty rank routed at.
    #[serde(
        default,
        deserialize_with = "deserialize_opt_grade_wire",
        skip_serializing_if = "Option::is_none"
    )]
    pub routed_difficulty: Option<GradeWire>,
    /// Optional execution profile selected with the new preset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_profile: Option<String>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `task.restarted`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskRestartedRecord {
    /// SQL `spawn_task_restart_as` writes `task` (not `id`).
    pub task: i64,
    /// Target Spawn session identifier.
    pub session_id: String,
    /// Agent identifier restarted for this task.
    pub agent_id: String,
    /// Optional new execution identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
    /// Optional actor that initiated the restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// Optional restart reason text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Optional restart sequence counter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<u64>,
    /// Optional alert or idempotency key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `task.cancelled`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskCancelledRecord {
    /// Cancelled task ID.
    pub id: i64,
    /// Optional actor cancelling the task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// Optional cancellation reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Optional follow-up task ID that reissued this task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reissued_as: Option<i64>,
    /// Optional follow-up task ID that reassigned this task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reassigned_as: Option<i64>,
    /// Optional recorded attempt diagnostics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempts: Option<Vec<Value>>,
    /// Whether cancellation followed explicit worker yield/exhaustion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yielded: Option<bool>,
    /// Optional objective number marked solved prior to closure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solved: Option<i64>,
    /// Optional idempotency/alert key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `task.stolen`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskStolenRecord {
    /// Task ID stolen from another queued worker.
    pub id: i64,
    /// Previous worker session ID.
    pub from_session: String,
    /// Previous worker agent ID.
    pub from_agent: String,
    /// New worker session ID.
    pub to_session: String,
    /// New worker agent ID.
    pub to_agent: String,
    /// Optional worker pool name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
    /// Actor/subsystem executing the work steal.
    pub by: String,
    /// Optional reason for stealing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Epoch milliseconds since the task sat queued on the previous worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_since_ms: Option<i64>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `task.aborted`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskAbortedRecord {
    /// Aborted task ID.
    pub id: i64,
    /// Optional abort reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Optional judging identifier if aborted after judge escalation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judging: Option<String>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `task.over_budget`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskOverBudgetRecord {
    /// Task ID that exceeded its configured budget.
    pub id: i64,
    /// Budget limit reason (`tool_calls`, `elapsed_ms`, etc.).
    pub reason: String,
    /// Observed tool call count when halted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<u64>,
    /// Observed elapsed execution time in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<i64>,
    /// Configured budget JSON object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Value>,
    /// Retained branch name carrying partial work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Retained commit SHA on `branch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `task.gated`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskGatedRecord {
    /// Gated task ID.
    pub id: i64,
    /// Gate outcome verdict.
    pub verdict: GateVerdict,
    /// Optional report revision tested by this gate run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u32>,
    /// Optional verdict classification (`code`, `infrastructure`, `superseded`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<GateVerdictClass>,
    /// Baseline commit SHA used by the gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Candidate commit SHA tested by the gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<String>,
    /// Path or reference to the gate runner log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
    /// Numeric or summary metric string emitted by the gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric: Option<String>,
    /// Human/gate summary note explaining rejection or failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Optional judging identifier if judge assisted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judging: Option<String>,
    /// Optional structured evidence excerpt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    /// Retry backoff duration in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff_ms: Option<i64>,
    /// Next retry due timestamp in Unix epoch milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_ms: Option<i64>,
    /// Whether this gate failure escalated the task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalate: Option<bool>,
    /// Whether this gate failure raised an operator alert.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert: Option<bool>,
    /// Count of scenarios executed in the gate run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scenarios: Option<u32>,
    /// Names of scenarios executed in the gate run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scenario_names: Option<Vec<String>>,
    /// Remote ref or push outcome note when gate pushed results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pushed: Option<String>,
    /// Whether the candidate was parked on failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked: Option<bool>,
    /// Sensitive paths or rules flagged during gating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sensitive: Option<Vec<String>>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// What a `task.gated` record says, one variant per verdict: the record's
/// loose fields read once, so a class that is not its verdict's, or a host
/// fault's retry on another verdict, has no value.
///
/// The ledger keeps [`TaskGatedRecord`]'s wire fields unchanged (an
/// append-only log is never rewritten); this is the one reading of them.
/// A field a variant does not carry says nothing more about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateOutcome<'a> {
    /// The gate passed the candidate: it may land.
    Accepted {
        /// The candidate commit the gate judged.
        candidate: Option<&'a str>,
        /// The gate's metric.
        metric: Option<&'a str>,
    },
    /// The work failed its checks (a code fault).
    Rejected {
        /// Why, for the worker.
        note: Option<&'a str>,
    },
    /// The work conflicts with main (a code fault).
    Conflict {
        /// Why, for the worker.
        note: Option<&'a str>,
    },
    /// The work is incompatible with the baseline (a code fault).
    Incompatible {
        /// Why, for the worker.
        note: Option<&'a str>,
    },
    /// The gate stopped before it could judge (infrastructure).
    Aborted,
    /// The host had no room to judge (infrastructure).
    NoRoom {
        /// This shortage was escalated to the supervisor.
        escalate: bool,
    },
    /// The host could not judge (infrastructure); judged again later.
    HostFault {
        /// When it is judged again; none on a record from before backoffs.
        retry: Option<HostRetry>,
        /// This fault was escalated to the supervisor.
        escalate: bool,
        /// It parked the task (the load-sensitive bound).
        parked: bool,
    },
    /// Newer work or a newer main superseded the candidate.
    Stale,
}

/// When a host fault is judged again: `backoff_ms` after its record, at
/// `due_ms` (which the store writes as the record's time plus the backoff).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostRetry {
    /// The wait, in milliseconds.
    pub backoff_ms: i64,
    /// When the wait ends, in Unix epoch milliseconds, once the store wrote it.
    pub due_ms: Option<i64>,
}

impl GateOutcome<'_> {
    /// The verdict this outcome is.
    pub const fn verdict(self) -> GateVerdict {
        match self {
            Self::Accepted { .. } => GateVerdict::Accepted,
            Self::Rejected { .. } => GateVerdict::Rejected,
            Self::Conflict { .. } => GateVerdict::Conflict,
            Self::Incompatible { .. } => GateVerdict::Incompatible,
            Self::Aborted => GateVerdict::Aborted,
            Self::NoRoom { .. } => GateVerdict::NoRoom,
            Self::HostFault { .. } => GateVerdict::HostFault,
            Self::Stale => GateVerdict::Stale,
        }
    }

    /// Whose the verdict is, as `spawn_integrator_record` (`schema.sql`)
    /// pins it: the work's, the host's, or no one's (superseded).
    pub const fn class(self) -> GateVerdictClass {
        match self {
            Self::Accepted { .. }
            | Self::Rejected { .. }
            | Self::Conflict { .. }
            | Self::Incompatible { .. } => GateVerdictClass::Code,
            Self::Aborted | Self::NoRoom { .. } | Self::HostFault { .. } => {
                GateVerdictClass::Infrastructure
            }
            Self::Stale => GateVerdictClass::Superseded,
        }
    }
}

impl TaskGatedRecord {
    /// What the record says ([`GateOutcome`]). Refuses only what no writer
    /// ever recorded (`spawn_integrator_record` and the integrator, read
    /// through their history): a `class` that is not its verdict's (a record
    /// from before classes names none, and reads as its verdict's); a host
    /// fault's own field (`backoff_ms`, `due_ms`, `parked`, `alert`) on
    /// another verdict; a `due_ms` without its `backoff_ms`; a negative
    /// `backoff_ms`.
    pub fn outcome(&self) -> Result<GateOutcome<'_>, ControlRecordError> {
        let kind = ControlRecordKind::TaskGated.as_str();
        let escalate = self.escalate.unwrap_or(false);
        let outcome = match self.verdict {
            GateVerdict::Accepted => GateOutcome::Accepted {
                candidate: self.candidate.as_deref(),
                metric: self.metric.as_deref(),
            },
            GateVerdict::Rejected => GateOutcome::Rejected {
                note: self.note.as_deref(),
            },
            GateVerdict::Conflict => GateOutcome::Conflict {
                note: self.note.as_deref(),
            },
            GateVerdict::Incompatible => GateOutcome::Incompatible {
                note: self.note.as_deref(),
            },
            GateVerdict::Aborted => GateOutcome::Aborted,
            GateVerdict::NoRoom => GateOutcome::NoRoom { escalate },
            GateVerdict::Stale => GateOutcome::Stale,
            GateVerdict::HostFault => GateOutcome::HostFault {
                retry: match (self.backoff_ms, self.due_ms) {
                    (Some(backoff_ms), _) if backoff_ms < 0 => {
                        return Err(invariant_err(kind, "`backoff_ms` must not be negative"));
                    }
                    (Some(backoff_ms), due_ms) => Some(HostRetry { backoff_ms, due_ms }),
                    (None, Some(_)) => {
                        return Err(invariant_err(kind, "`due_ms` without `backoff_ms`"));
                    }
                    (None, None) => None,
                },
                escalate,
                parked: self.parked.unwrap_or(false),
            },
        };
        if let Some(class) = self.class
            && class != outcome.class()
        {
            return Err(invariant_err(
                kind,
                format!(
                    "verdict `{}` expects class `{}`, not `{class:?}`",
                    self.verdict.as_str(),
                    serde_json::to_string(&outcome.class()).unwrap_or_default()
                ),
            ));
        }
        if !matches!(outcome, GateOutcome::HostFault { .. }) {
            let host_fields = [
                ("backoff_ms", self.backoff_ms.is_some()),
                ("due_ms", self.due_ms.is_some()),
                ("parked", self.parked.is_some()),
                ("alert", self.alert.is_some()),
            ];
            if let Some((field, _)) = host_fields.iter().find(|(_, set)| *set) {
                return Err(invariant_err(
                    kind,
                    format!(
                        "`{field}` belongs to a host_fault verdict, not `{}`",
                        self.verdict.as_str()
                    ),
                ));
            }
        }
        Ok(outcome)
    }
}

/// Payload for `task.shadow_gated`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskShadowGatedRecord {
    /// Gated task ID.
    pub id: i64,
    /// Shadow gate runner identifier.
    pub runner: String,
    /// Optional report revision tested.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u32>,
    /// Verdict produced by the shadow runner, if completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<GateVerdict>,
    /// Input parameters passed to the shadow runner.
    pub inputs: Value,
    /// Per-field diffs against primary gate results.
    #[serde(default)]
    pub unequal: Vec<Value>,
    /// Whether shadow verdict agreed with primary verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agrees: Option<bool>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `task.held`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskHeldRecord {
    /// Task ID placed on hold.
    pub id: i64,
    /// Source hold ID blocking landing.
    pub hold: i64,
    /// Source repository name under hold.
    pub source: String,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `task.released`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskReleasedRecord {
    /// Task ID released from hold.
    pub id: i64,
    /// Actor releasing the task.
    pub by: String,
    /// Human-readable release reason.
    pub reason: String,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `task.landed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskLandedRecord {
    /// Landed task ID.
    pub id: i64,
    /// Previous target branch commit before landing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// New target branch commit after landing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    /// Mission IDs advanced by this landing.
    #[serde(
        default,
        deserialize_with = "deserialize_opt_vec_i64",
        skip_serializing_if = "Option::is_none"
    )]
    pub missions: Option<Vec<i64>>,
    /// Optional judging identifier if judge intervened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judging: Option<String>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `resident.proposed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResidentProposedRecord {
    /// Proposed resident name.
    pub name: String,
    /// Resident charter markdown/text specification.
    pub charter: String,
    /// Optional actor proposing the resident.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_by: Option<String>,
    /// Optional worker pool name assigned to the resident.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `resident.approved` and `resident.retired`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResidentNamedRecord {
    /// Resident worker name.
    pub name: String,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `objective.solved`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectiveSolvedRecord {
    /// Mission ID owning the solved objective.
    pub id: i64,
    /// Numeric objective index within the mission.
    pub objective: i32,
    /// Optional task ID whose gate proved the objective.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<i64>,
    /// Scenario path/name that passed verification.
    pub scenario: String,
    /// Base commit SHA on which the scenario ran.
    pub base: String,
    /// Optional verification log path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `objective.verified`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectiveVerifiedRecord {
    /// Mission ID owning the objective.
    pub mission: i64,
    /// Numeric objective index within the mission.
    pub objective: i32,
    /// Actor recording live verification.
    pub by: String,
    /// Description of how the objective was verified.
    pub how: String,
    /// Verification verdict (`pass` or `fail`).
    pub result: VerificationResult,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `objective.retired`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectiveRetiredRecord {
    /// Mission ID owning the retired objective.
    pub id: i64,
    /// Numeric objective index retired.
    pub objective: i32,
    /// Retirement classification (`on-main`, `renumbered`, `obsolete`).
    pub kind: ObjectiveRetireKind,
    /// Actor retiring the objective.
    pub by: String,
    /// Explanation for retiring the objective.
    pub reason: String,
    /// Optional CLI/subsystem caller identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller: Option<String>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `source.moved`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceMovedRecord {
    /// Source repository name.
    pub source: String,
    /// Previous `main` commit SHA.
    pub from: String,
    /// New `main` commit SHA.
    pub to: String,
    /// Optional actor moving the source head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// Optional source push request ID fulfilled by this move.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<i64>,
    /// Mission IDs notified/updated by this source move.
    #[serde(
        default,
        deserialize_with = "deserialize_opt_vec_i64",
        skip_serializing_if = "Option::is_none"
    )]
    pub missions: Option<Vec<i64>>,
    /// Mission IDs cleared to green by this move.
    #[serde(
        default,
        deserialize_with = "deserialize_opt_vec_i64",
        skip_serializing_if = "Option::is_none"
    )]
    pub clear: Option<Vec<i64>>,
    /// Mission IDs recorded without an active baseline.
    #[serde(
        default,
        deserialize_with = "deserialize_opt_vec_i64",
        skip_serializing_if = "Option::is_none"
    )]
    pub without_baseline: Option<Vec<i64>>,
    /// Optional judging identifier if judge assisted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judging: Option<String>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `source.held`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceHeldRecord {
    /// Source repository name placed on hold.
    pub source: String,
    /// Mainline commit SHA pinned when the hold was taken.
    pub main: String,
    /// Actor holding the source.
    pub by: String,
    /// Hold lease duration in seconds.
    pub bound_s: u64,
    /// Hold expiration timestamp in Unix epoch milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_ms: Option<i64>,
    /// Prior hold ID renewed by this record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renews: Option<i64>,
    /// Optional hold rationale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `source.released`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceReleasedRecord {
    /// Source repository name released from hold.
    pub source: String,
    /// Released hold ID.
    pub hold: i64,
    /// Optional actor releasing the hold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `source.hold_expired`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceHoldExpiredRecord {
    /// Source repository whose hold expired.
    pub source: String,
    /// Expired hold ID.
    pub hold: i64,
    /// Expiration timestamp in Unix epoch milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_ms: Option<i64>,
    /// Task IDs unblocked when the hold expired.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waiting: Option<Vec<i64>>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `source.push_requested`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourcePushRequestedRecord {
    /// Target source repository name.
    pub source: String,
    /// Candidate commit SHA requested for push.
    pub commit: String,
    /// Target git ref (serialized as `"ref"`).
    #[serde(rename = "ref")]
    pub git_ref: String,
    /// Optional actor requesting the push.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// Whether the push goes through integrator gating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gated: Option<bool>,
    /// Whether the push was flagged urgent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub urgent: Option<bool>,
    /// Optional rationale for the push request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `source.push_refused`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourcePushRefusedRecord {
    /// Push request ID that was refused.
    pub request: i64,
    /// Refusal reason message.
    pub reason: String,
    /// Optional source repository name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Optional commit SHA refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Optional judging identifier if judge assisted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judging: Option<String>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `source.push_no_verdict`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourcePushNoVerdictRecord {
    /// Push request ID left without a verdict.
    pub request: i64,
    /// Optional diagnostic reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Optional gate/push log path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
    /// Optional judging identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judging: Option<String>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `source.main_red`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceMainRedRecord {
    /// Source repository whose `main` turned red.
    pub source: String,
    /// Failing `main` commit SHA.
    pub main: String,
    /// Scenario name that failed.
    pub scenario: String,
    /// Structured scenario failure details.
    pub failures: Vec<Value>,
    /// Remediation task ID created for the failure.
    pub task: i64,
    /// Whether an operator alert was raised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert: Option<bool>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `source.main_probe`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceMainProbeRecord {
    /// Source repository probed.
    pub source: String,
    /// Main commit SHA probed.
    pub main: String,
    /// Task ID inspecting the red `main`.
    pub task: i64,
    /// Probe execution timestamp in Unix epoch milliseconds.
    pub probe_ms: i64,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `source.main_green`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceMainGreenRecord {
    /// Source repository whose `main` returned green.
    pub source: String,
    /// Passing scenario name.
    pub scenario: String,
    /// Optional commit SHA that passed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub main: Option<String>,
    /// Timestamp since `main` became red before turning green (epoch ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<i64>,
    /// Earlier failing `main` commit SHA cleared by this green run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub red_main: Option<String>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Parent/ancestor process entry recorded in [`IntegratorStartedRecord`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessAncestor {
    /// Operating system process ID.
    pub pid: u64,
    /// Process executable name (`comm`).
    pub comm: String,
}

/// Payload for `integrator.started`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntegratorStartedRecord {
    /// Integrator daemon PID.
    pub pid: u64,
    /// Parent process PID.
    pub ppid: u64,
    /// Optional parent executable name.
    #[serde(default)]
    pub parent_comm: Option<String>,
    /// Process ancestor chain captured at startup.
    pub ancestors: Vec<ProcessAncestor>,
    /// Working directory of the integrator process.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Linux cgroup path, if readable.
    #[serde(default)]
    pub cgroup: Option<String>,
    /// Gate script path configured for this integrator.
    pub gate: String,
    /// Whether `gate` came from environment override rather than mission config.
    pub gate_from_env: bool,
    /// Workspace store root directory.
    pub root: String,
    /// Whether running single-shot (`--once`).
    pub once: bool,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `integrator.draining`, `integrator.drained`, and `integrator.drain_expired`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntegratorDrainRecord {
    /// Drain timeout window in seconds.
    pub bound_s: u64,
    /// Optional gate run IDs still active or drained.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gates: Option<Vec<i64>>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `host.disk`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostDiskRecord {
    /// Filesystem path checked.
    pub path: String,
    /// Threshold crossing direction (`low` or `recovered`).
    pub state: HostDiskState,
    /// Available bytes measured on the filesystem.
    pub free_bytes: u64,
    /// Optional filesystem label (e.g. `root`, `tmp`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Configured warning threshold in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn_bytes: Option<u64>,
    /// Configured hard floor threshold in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor_bytes: Option<u64>,
    /// Optional human/diagnostic explanation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `host.storage`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostStorageRecord {
    /// Total bytes across archived transcripts.
    pub archive_bytes: u64,
    /// Total files across archived transcripts.
    pub archive_files: u64,
    /// Total bytes across managed workspace project trees.
    pub projects_bytes: u64,
    /// Total files across managed workspace project trees.
    pub projects_files: u64,
    /// Optional PostgreSQL database size in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database_bytes: Option<u64>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `host.core_lines`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostCoreLinesRecord {
    /// Sampled core source-line metrics keyed by subsystem.
    pub values: BTreeMap<String, Value>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `host.storage_bytes`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostStorageBytesRecord {
    /// Per-source storage byte breakdown keyed by source/area name.
    pub sources: BTreeMap<String, Value>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `host.judge`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostJudgeRecord {
    /// Configured judge identifier/label.
    pub judge: String,
    /// Judge availability state (`down` or `up`).
    pub state: HostJudgeState,
    /// Optional last error string when state is `down`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Timestamp when judge failures began (epoch ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_failed_ms: Option<i64>,
    /// Consecutive failed judge call count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_calls: Option<u64>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `host.messages_marked`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostMessagesMarkedRecord {
    /// Baseline cutoff timestamp in Unix epoch milliseconds.
    pub started_at_ms: i64,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `host.loop_started`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostLoopStartedRecord {
    /// Ephemeral boot identifier for the started host loop instance.
    pub boot_id: String,
    /// Loop start timestamp in Unix epoch milliseconds.
    pub started_at_ms: i64,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `execution.interrupted`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionInterruptedRecord {
    /// Agent identifier of the interrupted worker.
    pub agent_id: String,
    /// Session identifier owning the execution.
    pub session_id: String,
    /// Execution identifier that was interrupted.
    pub execution_id: String,
    /// Categorized interruption cause.
    pub cause: InterruptionCause,
    /// Recovery action selected for the interruption.
    pub path: RecoveryPath,
    /// Recovery attempt counter (1-based).
    pub attempt: u64,
    /// Structured evidence explaining the interruption.
    pub evidence: Value,
    /// Next retry due timestamp in Unix epoch milliseconds, if scheduled.
    #[serde(default)]
    pub due_ms: Option<i64>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `execution.cleared`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionClearedRecord {
    /// Execution identifier cleared after recovery.
    pub execution_id: String,
    /// Session identifier owning the execution.
    pub session_id: String,
    /// Optional interruption cause that had paused execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<InterruptionCause>,
    /// Total duration spent waiting before clearance (ms).
    pub waited_ms: i64,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `alert.fired`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertFiredRecord {
    /// Unique deduplication key for the alert.
    pub key: String,
    /// Alert category name.
    pub kind: String,
    /// Target entity identifier/subject of the alert.
    pub subject: String,
    /// Optional human-readable alert text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Whether this record is an informational notice rather than an active alert.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<bool>,
    /// Optional structured evidence JSON payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Value>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `alert.updated`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertUpdatedRecord {
    /// Deduplication key of the updated alert.
    pub key: String,
    /// Alert category name.
    pub kind: String,
    /// Target entity identifier/subject of the alert.
    pub subject: String,
    /// Updated human-readable alert description.
    pub text: String,
    /// Optional updated evidence JSON payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Value>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `alert.judged`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertJudgedRecord {
    /// Deduplication key of the judged alert.
    pub key: String,
    /// Operator verdict recorded against the alert.
    pub verdict: AlertVerdict,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `alert.cleared`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertClearedRecord {
    /// Deduplication key of the cleared alert.
    pub key: String,
    /// Alert category name.
    pub kind: String,
    /// Target entity identifier/subject of the alert.
    pub subject: String,
    /// Optional structured clearance evidence JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Value>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `deploy.finished`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeployFinishedRecord {
    /// Whether the deploy succeeded (`true`) or failed (`false`).
    pub ok: bool,
    /// Deployed commit SHA.
    pub head: String,
    /// Deploy trigger source (`landing` or `owner`).
    pub by: DeployBy,
    /// Explanation or status detail.
    pub reason: String,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `backup.dumped`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupDumpedRecord {
    /// Target key identifiers dumped into backup storage.
    #[serde(default)]
    pub keys: Vec<String>,
    /// Optional backup retention tier labels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tiers: Option<Vec<String>>,
    /// Optional details of skipped destinations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<Vec<Value>>,
    /// Hex SHA-256 of the dump payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Uncompressed dump size in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// Stored (possibly encrypted/compressed) dump size in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stored_bytes: Option<u64>,
    /// Number of events captured in the dump.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events: Option<u64>,
    /// Encryption key fingerprint used to seal the backup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sealed_with: Option<String>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `backup.bundled`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupBundledRecord {
    /// Bundle kind (`full` or `incremental`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Base event ID for incremental git/wal bundles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_event_id: Option<i64>,
    /// Destination keys written for the bundle.
    #[serde(default)]
    pub keys: Vec<String>,
    /// Retention tiers applied to this bundle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tiers: Option<Vec<String>>,
    /// Optional skipped destination notes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<Vec<Value>>,
    /// Hex SHA-256 of the bundle archive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Unsealed bundle byte size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// Stored bundle byte size after sealing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stored_bytes: Option<u64>,
    /// Encryption key fingerprint used to seal the bundle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sealed_with: Option<String>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `backup.verified`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupVerifiedRecord {
    /// Whether integrity verification passed.
    pub ok: bool,
    /// Optional failure/diagnostic reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Verified remote object key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Observed SHA-256 digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Verification wall-clock duration in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `backup.stored`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupStoredRecord {
    /// Storage area replicated (`transcripts` or `sessions`).
    pub area: BackupArea,
    /// Remote storage host label.
    pub host: String,
    /// Storage account identifier.
    pub account: String,
    /// Manifest key/identifier written for this pass.
    pub manifest: String,
    /// Total files accounted for in the manifest.
    pub files: u64,
    /// Count of files unchanged since previous pass.
    pub unchanged: u64,
    /// Count of append-grown files incrementally synced.
    pub grown: u64,
    /// Count of files uploaded whole.
    pub whole: u64,
    /// Number of new segments uploaded.
    pub segments_written: u64,
    /// Number of existing segments verified on remote.
    pub segments_found: u64,
    /// Raw payload bytes transferred.
    pub bytes_written: u64,
    /// Encryption key fingerprint used to seal segments/manifests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sealed_with: Option<String>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `backup.drilled.transcripts`, `backup.drilled.missions`, and `backup.drilled.sync`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupDrillRecord {
    /// Whether the restore drill succeeded.
    pub ok: bool,
    /// Optional drill failure reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Drilled backup bundle key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<String>,
    /// Base event ID covered by the drilled bundle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_event_id: Option<i64>,
    /// Count of state mismatches discovered during the drill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mismatches: Option<u64>,
    /// Drill execution time in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `backup.drilled.controllers`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupDrilledControllersRecord {
    /// Whether the controller parity drill passed.
    pub ok: bool,
    /// Optional failure reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Drilled backup bundle key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<String>,
    /// Base event ID of the drilled bundle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_event_id: Option<i64>,
    /// Count of controller mismatches found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mismatches: Option<u64>,
    /// Drill duration in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Drill run identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<i64>,
    /// Controller identifier checked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller: Option<String>,
    /// Mission IDs replayed during the controller drill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub missions: Option<Vec<i64>>,
    /// Source name checked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Current controller state snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<Value>,
    /// Previous controller state snapshot for comparison.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev: Option<Value>,
    /// Fields intentionally excluded from parity comparison.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left_out: Option<Vec<String>>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `legacy.imported`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LegacyImportedRecord {
    /// Legacy SQL table name imported.
    pub table: LegacyTable,
    /// Raw JSON representation of the imported row.
    pub row: Value,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Discriminant for every `control.recorded` variant handled by
/// `spawn_control_apply`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ControlRecordKind {
    /// `mission.added` record.
    MissionAdded,
    /// `mission.configured` record.
    MissionConfigured,
    /// `mission.closed` record.
    MissionClosed,
    /// `mission.halted` record.
    MissionHalted,
    /// `mission.resumed` record.
    MissionResumed,
    /// `mission.paused` record.
    MissionPaused,
    /// `mission.unpaused` record.
    MissionUnpaused,
    /// `task.assigned` record.
    TaskAssigned,
    /// `task.delivered` record.
    TaskDelivered,
    /// `task.reported` record.
    TaskReported,
    /// `task.report_unfinished` record.
    TaskReportUnfinished,
    /// `task.fork_failed` record.
    TaskForkFailed,
    /// `task.reviewed` record.
    TaskReviewed,
    /// `task.rerouted` record.
    TaskRerouted,
    /// `task.restarted` record.
    TaskRestarted,
    /// `task.cancelled` record.
    TaskCancelled,
    /// `task.stolen` record.
    TaskStolen,
    /// `task.aborted` record.
    TaskAborted,
    /// `task.over_budget` record.
    TaskOverBudget,
    /// `task.gated` record.
    TaskGated,
    /// `task.shadow_gated` record.
    TaskShadowGated,
    /// `task.held` record.
    TaskHeld,
    /// `task.released` record.
    TaskReleased,
    /// `task.landed` record.
    TaskLanded,
    /// `resident.proposed` record.
    ResidentProposed,
    /// `resident.approved` record.
    ResidentApproved,
    /// `resident.retired` record.
    ResidentRetired,
    /// `objective.solved` record.
    ObjectiveSolved,
    /// `objective.verified` record.
    ObjectiveVerified,
    /// `objective.retired` record.
    ObjectiveRetired,
    /// `source.moved` record.
    SourceMoved,
    /// `source.held` record.
    SourceHeld,
    /// `source.released` record.
    SourceReleased,
    /// `source.hold_expired` record.
    SourceHoldExpired,
    /// `source.push_requested` record.
    SourcePushRequested,
    /// `source.push_refused` record.
    SourcePushRefused,
    /// `source.push_no_verdict` record.
    SourcePushNoVerdict,
    /// `source.main_red` record.
    SourceMainRed,
    /// `source.main_probe` record.
    SourceMainProbe,
    /// `source.main_green` record.
    SourceMainGreen,
    /// `integrator.started` record.
    IntegratorStarted,
    /// `integrator.draining` record.
    IntegratorDraining,
    /// `integrator.drained` record.
    IntegratorDrained,
    /// `integrator.drain_expired` record.
    IntegratorDrainExpired,
    /// `host.disk` record.
    HostDisk,
    /// `host.storage` record.
    HostStorage,
    /// `host.core_lines` record.
    HostCoreLines,
    /// `host.storage_bytes` record.
    HostStorageBytes,
    /// `host.judge` record.
    HostJudge,
    /// `host.messages_marked` record.
    HostMessagesMarked,
    /// `host.loop_started` record.
    HostLoopStarted,
    /// `execution.interrupted` record.
    ExecutionInterrupted,
    /// `execution.cleared` record.
    ExecutionCleared,
    /// `alert.fired` record.
    AlertFired,
    /// `alert.updated` record.
    AlertUpdated,
    /// `alert.judged` record.
    AlertJudged,
    /// `alert.cleared` record.
    AlertCleared,
    /// `deploy.finished` record.
    DeployFinished,
    /// `backup.dumped` record.
    BackupDumped,
    /// `backup.bundled` record.
    BackupBundled,
    /// `backup.verified` record.
    BackupVerified,
    /// `backup.stored` record.
    BackupStored,
    /// `backup.drilled.transcripts` record.
    BackupDrilledTranscripts,
    /// `backup.drilled.missions` record.
    BackupDrilledMissions,
    /// `backup.drilled.controllers` record.
    BackupDrilledControllers,
    /// `backup.drilled.sync` record.
    BackupDrilledSync,
    /// `legacy.imported` record.
    LegacyImported,
}

impl ControlRecordKind {
    /// Every `ControlRecordKind` in canonical declaration order (67 variants).
    pub const ALL: [Self; 67] = [
        Self::MissionAdded,
        Self::MissionConfigured,
        Self::MissionClosed,
        Self::MissionHalted,
        Self::MissionResumed,
        Self::MissionPaused,
        Self::MissionUnpaused,
        Self::TaskAssigned,
        Self::TaskDelivered,
        Self::TaskReported,
        Self::TaskReportUnfinished,
        Self::TaskForkFailed,
        Self::TaskReviewed,
        Self::TaskRerouted,
        Self::TaskRestarted,
        Self::TaskCancelled,
        Self::TaskStolen,
        Self::TaskAborted,
        Self::TaskOverBudget,
        Self::TaskGated,
        Self::TaskShadowGated,
        Self::TaskHeld,
        Self::TaskReleased,
        Self::TaskLanded,
        Self::ResidentProposed,
        Self::ResidentApproved,
        Self::ResidentRetired,
        Self::ObjectiveSolved,
        Self::ObjectiveVerified,
        Self::ObjectiveRetired,
        Self::SourceMoved,
        Self::SourceHeld,
        Self::SourceReleased,
        Self::SourceHoldExpired,
        Self::SourcePushRequested,
        Self::SourcePushRefused,
        Self::SourcePushNoVerdict,
        Self::SourceMainRed,
        Self::SourceMainProbe,
        Self::SourceMainGreen,
        Self::IntegratorStarted,
        Self::IntegratorDraining,
        Self::IntegratorDrained,
        Self::IntegratorDrainExpired,
        Self::HostDisk,
        Self::HostStorage,
        Self::HostCoreLines,
        Self::HostStorageBytes,
        Self::HostJudge,
        Self::HostMessagesMarked,
        Self::HostLoopStarted,
        Self::ExecutionInterrupted,
        Self::ExecutionCleared,
        Self::AlertFired,
        Self::AlertUpdated,
        Self::AlertJudged,
        Self::AlertCleared,
        Self::DeployFinished,
        Self::BackupDumped,
        Self::BackupBundled,
        Self::BackupVerified,
        Self::BackupStored,
        Self::BackupDrilledTranscripts,
        Self::BackupDrilledMissions,
        Self::BackupDrilledControllers,
        Self::BackupDrilledSync,
        Self::LegacyImported,
    ];

    /// Exact SQL `'type'` wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissionAdded => "mission.added",
            Self::MissionConfigured => "mission.configured",
            Self::MissionClosed => "mission.closed",
            Self::MissionHalted => "mission.halted",
            Self::MissionResumed => "mission.resumed",
            Self::MissionPaused => "mission.paused",
            Self::MissionUnpaused => "mission.unpaused",
            Self::TaskAssigned => "task.assigned",
            Self::TaskDelivered => "task.delivered",
            Self::TaskReported => "task.reported",
            Self::TaskReportUnfinished => "task.report_unfinished",
            Self::TaskForkFailed => "task.fork_failed",
            Self::TaskReviewed => "task.reviewed",
            Self::TaskRerouted => "task.rerouted",
            Self::TaskRestarted => "task.restarted",
            Self::TaskCancelled => "task.cancelled",
            Self::TaskStolen => "task.stolen",
            Self::TaskAborted => "task.aborted",
            Self::TaskOverBudget => "task.over_budget",
            Self::TaskGated => "task.gated",
            Self::TaskShadowGated => "task.shadow_gated",
            Self::TaskHeld => "task.held",
            Self::TaskReleased => "task.released",
            Self::TaskLanded => "task.landed",
            Self::ResidentProposed => "resident.proposed",
            Self::ResidentApproved => "resident.approved",
            Self::ResidentRetired => "resident.retired",
            Self::ObjectiveSolved => "objective.solved",
            Self::ObjectiveVerified => "objective.verified",
            Self::ObjectiveRetired => "objective.retired",
            Self::SourceMoved => "source.moved",
            Self::SourceHeld => "source.held",
            Self::SourceReleased => "source.released",
            Self::SourceHoldExpired => "source.hold_expired",
            Self::SourcePushRequested => "source.push_requested",
            Self::SourcePushRefused => "source.push_refused",
            Self::SourcePushNoVerdict => "source.push_no_verdict",
            Self::SourceMainRed => "source.main_red",
            Self::SourceMainProbe => "source.main_probe",
            Self::SourceMainGreen => "source.main_green",
            Self::IntegratorStarted => "integrator.started",
            Self::IntegratorDraining => "integrator.draining",
            Self::IntegratorDrained => "integrator.drained",
            Self::IntegratorDrainExpired => "integrator.drain_expired",
            Self::HostDisk => "host.disk",
            Self::HostStorage => "host.storage",
            Self::HostCoreLines => "host.core_lines",
            Self::HostStorageBytes => "host.storage_bytes",
            Self::HostJudge => "host.judge",
            Self::HostMessagesMarked => "host.messages_marked",
            Self::HostLoopStarted => "host.loop_started",
            Self::ExecutionInterrupted => "execution.interrupted",
            Self::ExecutionCleared => "execution.cleared",
            Self::AlertFired => "alert.fired",
            Self::AlertUpdated => "alert.updated",
            Self::AlertJudged => "alert.judged",
            Self::AlertCleared => "alert.cleared",
            Self::DeployFinished => "deploy.finished",
            Self::BackupDumped => "backup.dumped",
            Self::BackupBundled => "backup.bundled",
            Self::BackupVerified => "backup.verified",
            Self::BackupStored => "backup.stored",
            Self::BackupDrilledTranscripts => "backup.drilled.transcripts",
            Self::BackupDrilledMissions => "backup.drilled.missions",
            Self::BackupDrilledControllers => "backup.drilled.controllers",
            Self::BackupDrilledSync => "backup.drilled.sync",
            Self::LegacyImported => "legacy.imported",
        }
    }

    /// Parse a SQL `'type'` wire name.
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == raw)
    }
}

impl fmt::Display for ControlRecordKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Strongly-typed ADT covering every `control.recorded` record folded or
/// stored through `spawn_control_apply` / `spawn_control_record`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ControlRecord {
    /// `mission.added` control record.
    #[serde(rename = "mission.added")]
    MissionAdded(MissionAddedRecord),
    /// `mission.configured` control record.
    #[serde(rename = "mission.configured")]
    MissionConfigured(MissionConfiguredRecord),
    /// `mission.closed` control record.
    #[serde(rename = "mission.closed")]
    MissionClosed(MissionClosedRecord),
    /// `mission.halted` control record.
    #[serde(rename = "mission.halted")]
    MissionHalted(Box<MissionHaltedRecord>),
    /// `mission.resumed` control record.
    #[serde(rename = "mission.resumed")]
    MissionResumed(MissionResumedRecord),
    /// `mission.paused` control record.
    #[serde(rename = "mission.paused")]
    MissionPaused(MissionPausedRecord),
    /// `mission.unpaused` control record.
    #[serde(rename = "mission.unpaused")]
    MissionUnpaused(MissionUnpausedRecord),

    /// `task.assigned` control record.
    #[serde(rename = "task.assigned")]
    TaskAssigned(Box<TaskAssignedRecord>),
    /// `task.delivered` control record.
    #[serde(rename = "task.delivered")]
    TaskDelivered(TaskDeliveredRecord),
    /// `task.reported` control record.
    #[serde(rename = "task.reported")]
    TaskReported(TaskReportedRecord),
    /// `task.report_unfinished` control record.
    #[serde(rename = "task.report_unfinished")]
    TaskReportUnfinished(Box<TaskReportUnfinishedRecord>),
    /// `task.fork_failed` control record.
    #[serde(rename = "task.fork_failed")]
    TaskForkFailed(TaskForkFailedRecord),
    /// `task.reviewed` control record.
    #[serde(rename = "task.reviewed")]
    TaskReviewed(TaskReviewedRecord),
    /// `task.rerouted` control record.
    #[serde(rename = "task.rerouted")]
    TaskRerouted(Box<TaskReroutedRecord>),
    /// `task.restarted` control record.
    #[serde(rename = "task.restarted")]
    TaskRestarted(Box<TaskRestartedRecord>),
    /// `task.cancelled` control record.
    #[serde(rename = "task.cancelled")]
    TaskCancelled(Box<TaskCancelledRecord>),
    /// `task.stolen` control record.
    #[serde(rename = "task.stolen")]
    TaskStolen(Box<TaskStolenRecord>),
    /// `task.aborted` control record.
    #[serde(rename = "task.aborted")]
    TaskAborted(TaskAbortedRecord),
    /// `task.over_budget` control record.
    #[serde(rename = "task.over_budget")]
    TaskOverBudget(Box<TaskOverBudgetRecord>),
    /// `task.gated` control record.
    #[serde(rename = "task.gated")]
    TaskGated(Box<TaskGatedRecord>),
    /// `task.shadow_gated` control record.
    #[serde(rename = "task.shadow_gated")]
    TaskShadowGated(Box<TaskShadowGatedRecord>),
    /// `task.held` control record.
    #[serde(rename = "task.held")]
    TaskHeld(TaskHeldRecord),
    /// `task.released` control record.
    #[serde(rename = "task.released")]
    TaskReleased(TaskReleasedRecord),
    /// `task.landed` control record.
    #[serde(rename = "task.landed")]
    TaskLanded(Box<TaskLandedRecord>),

    /// `resident.proposed` control record.
    #[serde(rename = "resident.proposed")]
    ResidentProposed(ResidentProposedRecord),
    /// `resident.approved` control record.
    #[serde(rename = "resident.approved")]
    ResidentApproved(ResidentNamedRecord),
    /// `resident.retired` control record.
    #[serde(rename = "resident.retired")]
    ResidentRetired(ResidentNamedRecord),

    /// `objective.solved` control record.
    #[serde(rename = "objective.solved")]
    ObjectiveSolved(Box<ObjectiveSolvedRecord>),
    /// `objective.verified` control record.
    #[serde(rename = "objective.verified")]
    ObjectiveVerified(ObjectiveVerifiedRecord),
    /// `objective.retired` control record.
    #[serde(rename = "objective.retired")]
    ObjectiveRetired(ObjectiveRetiredRecord),

    /// `source.moved` control record.
    #[serde(rename = "source.moved")]
    SourceMoved(Box<SourceMovedRecord>),
    /// `source.held` control record.
    #[serde(rename = "source.held")]
    SourceHeld(Box<SourceHeldRecord>),
    /// `source.released` control record.
    #[serde(rename = "source.released")]
    SourceReleased(SourceReleasedRecord),
    /// `source.hold_expired` control record.
    #[serde(rename = "source.hold_expired")]
    SourceHoldExpired(SourceHoldExpiredRecord),
    /// `source.push_requested` control record.
    #[serde(rename = "source.push_requested")]
    SourcePushRequested(Box<SourcePushRequestedRecord>),
    /// `source.push_refused` control record.
    #[serde(rename = "source.push_refused")]
    SourcePushRefused(Box<SourcePushRefusedRecord>),
    /// `source.push_no_verdict` control record.
    #[serde(rename = "source.push_no_verdict")]
    SourcePushNoVerdict(SourcePushNoVerdictRecord),
    /// `source.main_red` control record.
    #[serde(rename = "source.main_red")]
    SourceMainRed(Box<SourceMainRedRecord>),
    /// `source.main_probe` control record.
    #[serde(rename = "source.main_probe")]
    SourceMainProbe(SourceMainProbeRecord),
    /// `source.main_green` control record.
    #[serde(rename = "source.main_green")]
    SourceMainGreen(Box<SourceMainGreenRecord>),

    /// `integrator.started` control record.
    #[serde(rename = "integrator.started")]
    IntegratorStarted(Box<IntegratorStartedRecord>),
    /// `integrator.draining` control record.
    #[serde(rename = "integrator.draining")]
    IntegratorDraining(IntegratorDrainRecord),
    /// `integrator.drained` control record.
    #[serde(rename = "integrator.drained")]
    IntegratorDrained(IntegratorDrainRecord),
    /// `integrator.drain_expired` control record.
    #[serde(rename = "integrator.drain_expired")]
    IntegratorDrainExpired(IntegratorDrainRecord),

    /// `host.disk` control record.
    #[serde(rename = "host.disk")]
    HostDisk(Box<HostDiskRecord>),
    /// `host.storage` control record.
    #[serde(rename = "host.storage")]
    HostStorage(HostStorageRecord),
    /// `host.core_lines` control record.
    #[serde(rename = "host.core_lines")]
    HostCoreLines(HostCoreLinesRecord),
    /// `host.storage_bytes` control record.
    #[serde(rename = "host.storage_bytes")]
    HostStorageBytes(HostStorageBytesRecord),
    /// `host.judge` control record.
    #[serde(rename = "host.judge")]
    HostJudge(Box<HostJudgeRecord>),
    /// `host.messages_marked` control record.
    #[serde(rename = "host.messages_marked")]
    HostMessagesMarked(HostMessagesMarkedRecord),
    /// `host.loop_started` control record.
    #[serde(rename = "host.loop_started")]
    HostLoopStarted(HostLoopStartedRecord),

    /// `execution.interrupted` control record.
    #[serde(rename = "execution.interrupted")]
    ExecutionInterrupted(Box<ExecutionInterruptedRecord>),
    /// `execution.cleared` control record.
    #[serde(rename = "execution.cleared")]
    ExecutionCleared(ExecutionClearedRecord),

    /// `alert.fired` control record.
    #[serde(rename = "alert.fired")]
    AlertFired(Box<AlertFiredRecord>),
    /// `alert.updated` control record.
    #[serde(rename = "alert.updated")]
    AlertUpdated(Box<AlertUpdatedRecord>),
    /// `alert.judged` control record.
    #[serde(rename = "alert.judged")]
    AlertJudged(AlertJudgedRecord),
    /// `alert.cleared` control record.
    #[serde(rename = "alert.cleared")]
    AlertCleared(Box<AlertClearedRecord>),

    /// `deploy.finished` control record.
    #[serde(rename = "deploy.finished")]
    DeployFinished(DeployFinishedRecord),

    /// `backup.dumped` control record.
    #[serde(rename = "backup.dumped")]
    BackupDumped(Box<BackupDumpedRecord>),
    /// `backup.bundled` control record.
    #[serde(rename = "backup.bundled")]
    BackupBundled(Box<BackupBundledRecord>),
    /// `backup.verified` control record.
    #[serde(rename = "backup.verified")]
    BackupVerified(Box<BackupVerifiedRecord>),
    /// `backup.stored` control record.
    #[serde(rename = "backup.stored")]
    BackupStored(Box<BackupStoredRecord>),
    /// `backup.drilled.transcripts` control record.
    #[serde(rename = "backup.drilled.transcripts")]
    BackupDrilledTranscripts(Box<BackupDrillRecord>),
    /// `backup.drilled.missions` control record.
    #[serde(rename = "backup.drilled.missions")]
    BackupDrilledMissions(Box<BackupDrillRecord>),
    /// `backup.drilled.controllers` control record.
    #[serde(rename = "backup.drilled.controllers")]
    BackupDrilledControllers(Box<BackupDrilledControllersRecord>),
    /// `backup.drilled.sync` control record.
    #[serde(rename = "backup.drilled.sync")]
    BackupDrilledSync(Box<BackupDrillRecord>),

    /// `legacy.imported` control record.
    #[serde(rename = "legacy.imported")]
    LegacyImported(LegacyImportedRecord),
}

/// Legacy/forward-compatible parse result for `control.recorded` rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CompatControlRecord {
    /// Decoded known [`ControlRecord`] variant.
    Known(Box<ControlRecord>),
    /// Forward-compatible fallback carrying an unrecognized `'type'` name and raw payload.
    Unknown {
        /// Unrecognized `"type"` string.
        kind: String,
        /// Full raw JSON object payload.
        payload: Value,
    },
}

impl ControlRecord {
    /// Every `'type'` wire name in `ControlRecord`, pinned against `schema.sql`.
    pub const ALL_KINDS: &'static [&'static str] = &[
        "mission.added",
        "mission.configured",
        "mission.closed",
        "mission.halted",
        "mission.resumed",
        "mission.paused",
        "mission.unpaused",
        "task.assigned",
        "task.delivered",
        "task.reported",
        "task.report_unfinished",
        "task.fork_failed",
        "task.reviewed",
        "task.rerouted",
        "task.restarted",
        "task.cancelled",
        "task.stolen",
        "task.aborted",
        "task.over_budget",
        "task.gated",
        "task.shadow_gated",
        "task.held",
        "task.released",
        "task.landed",
        "resident.proposed",
        "resident.approved",
        "resident.retired",
        "objective.solved",
        "objective.verified",
        "objective.retired",
        "source.moved",
        "source.held",
        "source.released",
        "source.hold_expired",
        "source.push_requested",
        "source.push_refused",
        "source.push_no_verdict",
        "source.main_red",
        "source.main_probe",
        "source.main_green",
        "integrator.started",
        "integrator.draining",
        "integrator.drained",
        "integrator.drain_expired",
        "host.disk",
        "host.storage",
        "host.core_lines",
        "host.storage_bytes",
        "host.judge",
        "host.messages_marked",
        "host.loop_started",
        "execution.interrupted",
        "execution.cleared",
        "alert.fired",
        "alert.updated",
        "alert.judged",
        "alert.cleared",
        "deploy.finished",
        "backup.dumped",
        "backup.bundled",
        "backup.verified",
        "backup.stored",
        "backup.drilled.transcripts",
        "backup.drilled.missions",
        "backup.drilled.controllers",
        "backup.drilled.sync",
        "legacy.imported",
    ];

    /// Strong kind enum for this record.
    pub const fn kind(&self) -> ControlRecordKind {
        match self {
            Self::MissionAdded(_) => ControlRecordKind::MissionAdded,
            Self::MissionConfigured(_) => ControlRecordKind::MissionConfigured,
            Self::MissionClosed(_) => ControlRecordKind::MissionClosed,
            Self::MissionHalted(_) => ControlRecordKind::MissionHalted,
            Self::MissionResumed(_) => ControlRecordKind::MissionResumed,
            Self::MissionPaused(_) => ControlRecordKind::MissionPaused,
            Self::MissionUnpaused(_) => ControlRecordKind::MissionUnpaused,
            Self::TaskAssigned(_) => ControlRecordKind::TaskAssigned,
            Self::TaskDelivered(_) => ControlRecordKind::TaskDelivered,
            Self::TaskReported(_) => ControlRecordKind::TaskReported,
            Self::TaskReportUnfinished(_) => ControlRecordKind::TaskReportUnfinished,
            Self::TaskForkFailed(_) => ControlRecordKind::TaskForkFailed,
            Self::TaskReviewed(_) => ControlRecordKind::TaskReviewed,
            Self::TaskRerouted(_) => ControlRecordKind::TaskRerouted,
            Self::TaskRestarted(_) => ControlRecordKind::TaskRestarted,
            Self::TaskCancelled(_) => ControlRecordKind::TaskCancelled,
            Self::TaskStolen(_) => ControlRecordKind::TaskStolen,
            Self::TaskAborted(_) => ControlRecordKind::TaskAborted,
            Self::TaskOverBudget(_) => ControlRecordKind::TaskOverBudget,
            Self::TaskGated(_) => ControlRecordKind::TaskGated,
            Self::TaskShadowGated(_) => ControlRecordKind::TaskShadowGated,
            Self::TaskHeld(_) => ControlRecordKind::TaskHeld,
            Self::TaskReleased(_) => ControlRecordKind::TaskReleased,
            Self::TaskLanded(_) => ControlRecordKind::TaskLanded,
            Self::ResidentProposed(_) => ControlRecordKind::ResidentProposed,
            Self::ResidentApproved(_) => ControlRecordKind::ResidentApproved,
            Self::ResidentRetired(_) => ControlRecordKind::ResidentRetired,
            Self::ObjectiveSolved(_) => ControlRecordKind::ObjectiveSolved,
            Self::ObjectiveVerified(_) => ControlRecordKind::ObjectiveVerified,
            Self::ObjectiveRetired(_) => ControlRecordKind::ObjectiveRetired,
            Self::SourceMoved(_) => ControlRecordKind::SourceMoved,
            Self::SourceHeld(_) => ControlRecordKind::SourceHeld,
            Self::SourceReleased(_) => ControlRecordKind::SourceReleased,
            Self::SourceHoldExpired(_) => ControlRecordKind::SourceHoldExpired,
            Self::SourcePushRequested(_) => ControlRecordKind::SourcePushRequested,
            Self::SourcePushRefused(_) => ControlRecordKind::SourcePushRefused,
            Self::SourcePushNoVerdict(_) => ControlRecordKind::SourcePushNoVerdict,
            Self::SourceMainRed(_) => ControlRecordKind::SourceMainRed,
            Self::SourceMainProbe(_) => ControlRecordKind::SourceMainProbe,
            Self::SourceMainGreen(_) => ControlRecordKind::SourceMainGreen,
            Self::IntegratorStarted(_) => ControlRecordKind::IntegratorStarted,
            Self::IntegratorDraining(_) => ControlRecordKind::IntegratorDraining,
            Self::IntegratorDrained(_) => ControlRecordKind::IntegratorDrained,
            Self::IntegratorDrainExpired(_) => ControlRecordKind::IntegratorDrainExpired,
            Self::HostDisk(_) => ControlRecordKind::HostDisk,
            Self::HostStorage(_) => ControlRecordKind::HostStorage,
            Self::HostCoreLines(_) => ControlRecordKind::HostCoreLines,
            Self::HostStorageBytes(_) => ControlRecordKind::HostStorageBytes,
            Self::HostJudge(_) => ControlRecordKind::HostJudge,
            Self::HostMessagesMarked(_) => ControlRecordKind::HostMessagesMarked,
            Self::HostLoopStarted(_) => ControlRecordKind::HostLoopStarted,
            Self::ExecutionInterrupted(_) => ControlRecordKind::ExecutionInterrupted,
            Self::ExecutionCleared(_) => ControlRecordKind::ExecutionCleared,
            Self::AlertFired(_) => ControlRecordKind::AlertFired,
            Self::AlertUpdated(_) => ControlRecordKind::AlertUpdated,
            Self::AlertJudged(_) => ControlRecordKind::AlertJudged,
            Self::AlertCleared(_) => ControlRecordKind::AlertCleared,
            Self::DeployFinished(_) => ControlRecordKind::DeployFinished,
            Self::BackupDumped(_) => ControlRecordKind::BackupDumped,
            Self::BackupBundled(_) => ControlRecordKind::BackupBundled,
            Self::BackupVerified(_) => ControlRecordKind::BackupVerified,
            Self::BackupStored(_) => ControlRecordKind::BackupStored,
            Self::BackupDrilledTranscripts(_) => ControlRecordKind::BackupDrilledTranscripts,
            Self::BackupDrilledMissions(_) => ControlRecordKind::BackupDrilledMissions,
            Self::BackupDrilledControllers(_) => ControlRecordKind::BackupDrilledControllers,
            Self::BackupDrilledSync(_) => ControlRecordKind::BackupDrilledSync,
            Self::LegacyImported(_) => ControlRecordKind::LegacyImported,
        }
    }

    /// SQL `'type'` string for this record.
    const fn kind_str(&self) -> &'static str {
        self.kind().as_str()
    }

    /// Deserialize a `Value` into a strongly-typed [`ControlRecord`]. Refuses
    /// any unknown `'type'` rather than silently falling back.
    pub fn from_value(value: &Value) -> Result<Self, ControlRecordError> {
        let kind = extract_record_type(value)?;
        if ControlRecordKind::parse(kind).is_none() {
            return Err(ControlRecordError::UnknownKind(kind.to_string()));
        }
        serde_json::from_value(value.clone()).map_err(|err| ControlRecordError::Deserialize {
            kind: kind.to_string(),
            message: err.to_string(),
        })
    }

    /// Parse with [`Self::from_value`] and enforce closed-schema & SQL domain
    /// invariants.
    pub fn from_value_strict(value: &Value) -> Result<Self, ControlRecordError> {
        let record = Self::from_value(value)?;
        validate_control_record_strict(&record)?;
        Ok(record)
    }

    /// Legacy- and forward-compatible parser: normalizes older string-encoded
    /// fields when decoding known types, and preserves unrecognized future
    /// `'type'` records in [`CompatControlRecord::Unknown`].
    pub fn from_value_compat(value: &Value) -> Result<CompatControlRecord, ControlRecordError> {
        let kind = extract_record_type(value)?;
        if ControlRecordKind::parse(kind).is_none() {
            return Ok(CompatControlRecord::Unknown {
                kind: kind.to_string(),
                payload: value.clone(),
            });
        }
        let normalized = normalize_legacy_json(value);
        let parsed = Self::from_value(&normalized)?;
        Ok(CompatControlRecord::Known(Box::new(parsed)))
    }

    /// Serialize to `serde_json::Value` preserving the exact wire format.
    pub fn to_value(&self) -> Result<Value, ControlRecordError> {
        serde_json::to_value(self).map_err(|err| ControlRecordError::Serialize(err.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Sibling GradingRecord ADT (`grading.recorded` preset.* stream, 5 variants)
// ---------------------------------------------------------------------------

/// Payload for `preset.registered`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PresetRegisteredRecord {
    /// Registered preset identifier.
    pub preset_id: String,
    /// Runtime engine label (e.g. `claude`, `agy`).
    pub engine_label: String,
    /// Model identifier passed to the engine.
    pub model_label: String,
    /// Default model cost/capability profile name.
    pub model_profile: String,
    /// Relative routing cost tiebreaker.
    pub cost_rank: i32,
    /// Optional named execution profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_profile: Option<String>,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `preset.onboarded`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PresetOnboardedRecord {
    /// Onboarded preset identifier.
    pub preset_id: String,
    /// Preset lifecycle state prior to onboarding.
    pub from_state: String,
    /// Resulting preset lifecycle state (`active` or `disabled`).
    pub to_state: String,
    /// Proved grade rank (`None` when onboarding failed `l1`).
    #[serde(default, deserialize_with = "deserialize_opt_grade_wire")]
    pub to_grade: Option<GradeWire>,
    /// Number of onboarding exam cases passed.
    pub passed: u32,
    /// Total number of onboarding exam cases executed.
    pub total: u32,
    /// Failed exam case IDs or descriptors.
    #[serde(default)]
    pub failed_cases: Vec<Value>,
    /// Structured onboarding exam suite summary JSON.
    pub suite: Value,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `preset.regraded`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PresetRegradedRecord {
    /// Regraded preset identifier.
    pub preset_id: String,
    /// Calibration decision (`demote`, `restore`, etc.).
    pub decision: String,
    /// Grade rank before regrading.
    #[serde(default, deserialize_with = "deserialize_opt_grade_wire")]
    pub from_grade: Option<GradeWire>,
    /// Grade rank after regrading.
    #[serde(default, deserialize_with = "deserialize_opt_grade_wire")]
    pub to_grade: Option<GradeWire>,
    /// Lifecycle state before regrading.
    pub from_state: String,
    /// Lifecycle state after regrading.
    pub to_state: String,
    /// First-pass task successes observed in the calibration window.
    pub first_pass: u32,
    /// Total tasks evaluated in the calibration window.
    pub total: u32,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `preset.grade_locked`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PresetGradeLockedRecord {
    /// Locked/unlocked preset identifier.
    pub preset_id: String,
    /// Previous grade rank before lock change.
    #[serde(default, deserialize_with = "deserialize_opt_grade_wire")]
    pub from_grade: Option<GradeWire>,
    /// Target locked grade rank (`None` when unlocking).
    #[serde(default, deserialize_with = "deserialize_opt_grade_wire")]
    pub to_grade: Option<GradeWire>,
    /// Previous preset lifecycle state.
    pub from_state: String,
    /// New preset lifecycle state.
    pub to_state: String,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Payload for `preset.state_changed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PresetStateChangedRecord {
    /// Preset identifier whose state changed.
    pub preset_id: String,
    /// Previous lifecycle state.
    pub from_state: String,
    /// New lifecycle state.
    pub to_state: String,
    /// Forward-compatible extra JSON fields.
    #[serde(flatten, default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

/// Discriminant for every `grading.recorded` variant in `spawn_grading_apply`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum GradingRecordKind {
    /// `preset.registered` record.
    PresetRegistered,
    /// `preset.onboarded` record.
    PresetOnboarded,
    /// `preset.regraded` record.
    PresetRegraded,
    /// `preset.grade_locked` record.
    PresetGradeLocked,
    /// `preset.state_changed` record.
    PresetStateChanged,
}

impl GradingRecordKind {
    /// Every `GradingRecordKind` (5 variants).
    pub const ALL: [Self; 5] = [
        Self::PresetRegistered,
        Self::PresetOnboarded,
        Self::PresetRegraded,
        Self::PresetGradeLocked,
        Self::PresetStateChanged,
    ];

    /// Exact SQL `'type'` wire name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PresetRegistered => "preset.registered",
            Self::PresetOnboarded => "preset.onboarded",
            Self::PresetRegraded => "preset.regraded",
            Self::PresetGradeLocked => "preset.grade_locked",
            Self::PresetStateChanged => "preset.state_changed",
        }
    }

    /// Parse a SQL `'type'` wire name.
    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == raw)
    }
}

/// Strongly-typed ADT covering every `grading.recorded` (`preset.*`) record
/// folded by `spawn_grading_apply` in `schema.sql`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum GradingRecord {
    /// `preset.registered` grading record.
    #[serde(rename = "preset.registered")]
    PresetRegistered(PresetRegisteredRecord),
    /// `preset.onboarded` grading record.
    #[serde(rename = "preset.onboarded")]
    PresetOnboarded(Box<PresetOnboardedRecord>),
    /// `preset.regraded` grading record.
    #[serde(rename = "preset.regraded")]
    PresetRegraded(Box<PresetRegradedRecord>),
    /// `preset.grade_locked` grading record.
    #[serde(rename = "preset.grade_locked")]
    PresetGradeLocked(PresetGradeLockedRecord),
    /// `preset.state_changed` grading record.
    #[serde(rename = "preset.state_changed")]
    PresetStateChanged(PresetStateChangedRecord),
}

impl GradingRecord {
    /// Every `'type'` wire name in `GradingRecord`, pinned against `schema.sql`.
    pub const ALL_KINDS: &'static [&'static str] = &[
        "preset.registered",
        "preset.onboarded",
        "preset.regraded",
        "preset.grade_locked",
        "preset.state_changed",
    ];

    /// Strong kind enum for this record.
    pub const fn kind(&self) -> GradingRecordKind {
        match self {
            Self::PresetRegistered(_) => GradingRecordKind::PresetRegistered,
            Self::PresetOnboarded(_) => GradingRecordKind::PresetOnboarded,
            Self::PresetRegraded(_) => GradingRecordKind::PresetRegraded,
            Self::PresetGradeLocked(_) => GradingRecordKind::PresetGradeLocked,
            Self::PresetStateChanged(_) => GradingRecordKind::PresetStateChanged,
        }
    }

    /// SQL `'type'` string for this record.
    const fn kind_str(&self) -> &'static str {
        self.kind().as_str()
    }

    /// Deserialize a `Value` into a strongly-typed [`GradingRecord`].
    pub fn from_value(value: &Value) -> Result<Self, ControlRecordError> {
        let kind = extract_record_type(value)?;
        if GradingRecordKind::parse(kind).is_none() {
            return Err(ControlRecordError::UnknownKind(kind.to_string()));
        }
        serde_json::from_value(value.clone()).map_err(|err| ControlRecordError::Deserialize {
            kind: kind.to_string(),
            message: err.to_string(),
        })
    }

    /// Deserialize and verify closed-schema (`extra.is_empty()`) invariants.
    pub fn from_value_strict(value: &Value) -> Result<Self, ControlRecordError> {
        let record = Self::from_value(value)?;
        let extra = match &record {
            Self::PresetRegistered(r) => &r.extra,
            Self::PresetOnboarded(r) => &r.extra,
            Self::PresetRegraded(r) => &r.extra,
            Self::PresetGradeLocked(r) => &r.extra,
            Self::PresetStateChanged(r) => &r.extra,
        };
        reject_extra(record.kind_str(), extra)?;
        Ok(record)
    }

    /// Serialize to `serde_json::Value`.
    pub fn to_value(&self) -> Result<Value, ControlRecordError> {
        serde_json::to_value(self).map_err(|err| ControlRecordError::Serialize(err.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Parsing & strict validation helpers
// ---------------------------------------------------------------------------

fn extract_record_type(value: &Value) -> Result<&str, ControlRecordError> {
    let map = value.as_object().ok_or(ControlRecordError::NotAnObject)?;
    map.get("type")
        .and_then(Value::as_str)
        .ok_or(ControlRecordError::MissingType)
}

fn reject_extra(kind: &str, extra: &BTreeMap<String, Value>) -> Result<(), ControlRecordError> {
    if extra.is_empty() {
        return Ok(());
    }
    Err(ControlRecordError::UnexpectedFields {
        kind: kind.to_string(),
        fields: extra.keys().cloned().collect(),
    })
}

fn invariant_err(kind: &str, message: impl Into<String>) -> ControlRecordError {
    ControlRecordError::Invariant {
        kind: kind.to_string(),
        message: message.into(),
    }
}

fn validate_control_record_strict(record: &ControlRecord) -> Result<(), ControlRecordError> {
    let kind = record.kind_str();
    match record {
        ControlRecord::TaskAssigned(r) => {
            if r.brief.trim().is_empty() {
                return Err(invariant_err(kind, "`brief` must not be blank"));
            }
            if r.agent_id.is_empty() || r.session_id.is_empty() {
                return Err(invariant_err(kind, "`agent_id` and `session_id` required"));
            }
        }
        ControlRecord::TaskGated(r) => {
            r.outcome()?;
        }
        ControlRecord::TaskReviewed(r)
            if !r.accepted && r.note.as_deref().is_none_or(|n| n.trim().is_empty()) =>
        {
            return Err(invariant_err(
                kind,
                "sending a task back requires a non-blank `note`",
            ));
        }
        ControlRecord::ExecutionInterrupted(r) => validate_execution_interrupted(kind, r)?,
        ControlRecord::HostJudge(r) => validate_host_judge(kind, r)?,
        ControlRecord::DeployFinished(r) => {
            reject_extra(kind, &r.extra)?;
            let valid_hex = r.head.len() == 40
                && r.head
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            if !valid_hex {
                return Err(invariant_err(
                    kind,
                    "`head` must be a 40-char lowercase commit",
                ));
            }
        }
        ControlRecord::ObjectiveVerified(r) => reject_extra(kind, &r.extra)?,
        ControlRecord::AlertJudged(r) => reject_extra(kind, &r.extra)?,
        ControlRecord::IntegratorStarted(r) => validate_integrator_started(kind, r)?,
        ControlRecord::BackupStored(r) => reject_extra(kind, &r.extra)?,
        ControlRecord::BackupDrilledTranscripts(r)
        | ControlRecord::BackupDrilledMissions(r)
        | ControlRecord::BackupDrilledSync(r) => reject_extra(kind, &r.extra)?,
        ControlRecord::BackupDrilledControllers(r) => reject_extra(kind, &r.extra)?,
        _ => {}
    }
    Ok(())
}

fn validate_execution_interrupted(
    kind: &str,
    r: &ExecutionInterruptedRecord,
) -> Result<(), ControlRecordError> {
    if r.attempt < 1 {
        return Err(invariant_err(kind, "`attempt` must be >= 1"));
    }
    match (r.path, r.due_ms) {
        (RecoveryPath::Final, Some(_)) => Err(invariant_err(
            kind,
            "`final` recovery must have null `due_ms`",
        )),
        (RecoveryPath::Resume | RecoveryPath::Handoff | RecoveryPath::Wait, None) => Err(
            invariant_err(kind, "non-final recovery path requires `due_ms`"),
        ),
        _ => Ok(()),
    }
}

fn validate_host_judge(kind: &str, r: &HostJudgeRecord) -> Result<(), ControlRecordError> {
    match r.state {
        HostJudgeState::Down if r.error.is_none() || r.first_failed_ms.is_none() => Err(
            invariant_err(kind, "`down` state requires `error` and `first_failed_ms`"),
        ),
        HostJudgeState::Up if r.failed_calls.is_none() => {
            Err(invariant_err(kind, "`up` state requires `failed_calls`"))
        }
        _ => Ok(()),
    }
}

fn validate_integrator_started(
    kind: &str,
    r: &IntegratorStartedRecord,
) -> Result<(), ControlRecordError> {
    reject_extra(kind, &r.extra)?;
    if r.pid < 1 || r.gate.is_empty() || r.root.is_empty() {
        return Err(invariant_err(
            kind,
            "`pid >= 1`, `gate`, and `root` required",
        ));
    }
    Ok(())
}

/// Normalize legacy string-encoded integers/booleans into JSON primitives for
/// [`ControlRecord::from_value_compat`].
fn normalize_legacy_json(value: &Value) -> Value {
    const INT_FIELDS: &[&str] = &[
        "id",
        "mission_id",
        "command_id",
        "revision",
        "objective",
        "task",
        "hold",
        "from",
        "reissue_of",
        "continues",
        "reassign_of",
        "duplicate_of",
        "reissued_as",
        "reassigned_as",
        "pause_event",
        "halt",
        "request",
        "bound_s",
        "expires_ms",
        "renews",
        "mission",
    ];
    let Some(map) = value.as_object() else {
        return value.clone();
    };
    let mut out = map.clone();
    for key in INT_FIELDS {
        if let Some(Value::String(s)) = out.get(*key)
            && let Ok(n) = s.trim().parse::<i64>()
        {
            out.insert((*key).to_string(), Value::from(n));
        }
    }
    Value::Object(out)
}

// ---------------------------------------------------------------------------
// Pure Functional RSI Episode Replay & Attribution Engine
// ---------------------------------------------------------------------------

/// Terminal or active attribution outcome of a replayed [`TaskEpisode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeOutcome {
    /// Gated task landed onto the mission's target branch (`task.landed`).
    Landed,
    /// Review-only task accepted (`task.reviewed` `accepted: true`).
    Accepted,
    /// Task or objective rejected/exhausted via yield (`task.cancelled` `yielded: true`).
    Rejected,
    /// Aborted due to budget overrun/stale limit with work dropped (`task.aborted`).
    Aborted,
    /// Ended over budget with branch work kept for continuation (`task.over_budget`).
    OverBudget,
    /// Explicitly cancelled or closed by closure rule (`task.cancelled`).
    Cancelled,
    /// Still open (`assigned`, `delivered`, `reported`, `revise`, gated `accepted`, `landing`).
    InFlight,
}

/// Synchronized review checkpoint recorded against a specific report revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSyncPoint {
    /// Event sequence number of the `task.reviewed` record.
    pub event_seq: i64,
    /// Event timestamp in Unix epoch milliseconds.
    pub at_ms: i64,
    /// Report revision reviewed.
    pub revision: u32,
    /// Whether the review accepted (`true`) or requested revisions (`false`).
    pub accepted: bool,
    /// Optional reviewer feedback note.
    pub note: Option<String>,
}

/// Gate verdict checkpoint recorded during episode replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateSyncPoint {
    /// Event sequence number of the `task.gated` record.
    pub event_seq: i64,
    /// Event timestamp in Unix epoch milliseconds.
    pub at_ms: i64,
    /// Report revision tested by the gate.
    pub revision: u32,
    /// Integrator gate verdict.
    pub verdict: GateVerdict,
    /// Verdict responsibility class (`code`, `infrastructure`, `superseded`).
    pub class: Option<GateVerdictClass>,
    /// Baseline commit SHA used by the gate.
    pub base: Option<String>,
    /// Candidate commit SHA tested by the gate.
    pub candidate: Option<String>,
    /// Optional verdict explanation note.
    pub note: Option<String>,
}

/// Ordered control record timeline entry on an episode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpisodeStage {
    /// Control event sequence number.
    pub event_seq: i64,
    /// Event timestamp in Unix epoch milliseconds.
    pub at_ms: i64,
    /// Strong kind discriminant of the control record.
    pub kind: ControlRecordKind,
}

/// Deterministic projection of a single task's lifecycle from `control.recorded`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskEpisode {
    /// Task identifier.
    pub task_id: i64,
    /// Parent mission identifier.
    pub mission_id: i64,
    /// Worker kind (`resident` or `ephemeral`).
    pub kind: TaskKind,
    /// Assigned agent identifier (updated on restart/steal).
    pub agent_id: String,
    /// Assigned Spawn session identifier (updated on restart/steal).
    pub session_id: String,
    /// Task brief text.
    pub brief: String,
    /// Optional creator/assigner label.
    pub created_by: Option<String>,
    /// Optional numeric objective index within the mission.
    pub objective: Option<i32>,
    /// Optional scenario targeted by this task.
    pub scenario: Option<String>,
    /// Effective model profile string (`default`, `cheap`, etc.).
    pub model: String,
    /// Effective difficulty rank (`l1`..`l3`).
    pub difficulty: GradeWire,
    /// Currently attributed preset identifier, if routed.
    pub preset_id: Option<String>,
    /// Initial preset identifier first assigned to the task.
    pub first_preset_id: Option<String>,
    /// Routed engine label.
    pub engine: Option<String>,
    /// Routed model label.
    pub model_label: Option<String>,
    /// Whether the task is pinned against work stealing/rerouting.
    pub pinned: bool,
    /// Original task ID if reissued.
    pub reissue_of: Option<i64>,
    /// Prior task ID whose work this episode continues.
    pub continues: Option<i64>,
    /// Original task ID if reassigned.
    pub reassign_of: Option<i64>,

    /// Current replayed lifecycle state of the task.
    pub state: TaskState,
    /// High-level terminal or in-flight episode outcome.
    pub outcome: EpisodeOutcome,
    /// Total revision loops (`mission_tasks.revisions`): quality rejections
    /// plus merge conflicts / incompatible heads.
    pub rework: u32,
    /// Quality-only sends-back (`mission_tasks.quality_rejections`).
    pub quality_rejections: u32,
    /// Count of `task.rerouted` transitions observed.
    pub reroutes: u32,
    /// Count of `task.restarted` transitions observed.
    pub restarts: u32,
    /// Count of `task.stolen` transitions observed.
    pub steals: u32,
    /// Count of `task.delivered` transitions observed.
    pub deliveries: u32,
    /// Count of `task.reported` submissions observed.
    pub reports: u32,

    /// Most recent delivered command identifier.
    pub last_command_id: Option<i64>,
    /// Latest submitted completion report text.
    pub last_report: Option<String>,
    /// Commit SHA carried on the latest completion report.
    pub last_report_commit: Option<String>,
    /// Latest review feedback note.
    pub review_note: Option<String>,
    /// Baseline commit SHA from `task.landed` or `task.gated`.
    pub base: Option<String>,
    /// Latest gated candidate commit SHA.
    pub candidate: Option<String>,
    /// Latest metric string emitted by the gate.
    pub metric: Option<String>,
    /// Most recent integrator gate verdict observed.
    pub last_gate_verdict: Option<GateVerdict>,
    /// Chronological review checkpoints observed in the episode.
    pub review_sync_points: Vec<ReviewSyncPoint>,
    /// Chronological gate checkpoints observed in the episode.
    pub gate_verdicts: Vec<GateSyncPoint>,
    /// Actor credited with closing/aborting the task, if terminal.
    pub terminal_by: Option<String>,
    /// Reason recorded when closing/aborting the task, if terminal.
    pub terminal_reason: Option<String>,

    /// Event sequence number of `task.assigned`.
    pub assigned_seq: i64,
    /// Assignment timestamp in Unix epoch milliseconds.
    pub assigned_at_ms: i64,
    /// First delivery timestamp in Unix epoch milliseconds.
    pub delivered_at_ms: Option<i64>,
    /// Sequence number of the closing terminal event, if closed.
    pub closed_seq: Option<i64>,
    /// Closure timestamp in Unix epoch milliseconds, if closed.
    pub closed_at_ms: Option<i64>,
    /// Most recent event timestamp folded into this episode (epoch ms).
    pub updated_at_ms: i64,
    /// Ordered timeline of control events touching this episode.
    pub stages: Vec<EpisodeStage>,
}

impl TaskEpisode {
    /// Alias for [`Self::rework`] matching SQL `mission_tasks.revisions`.
    pub const fn revisions(&self) -> u32 {
        self.rework
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MissionTrackState {
    Open,
    Active,
    Done,
    Dropped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MissionMeta {
    gated: bool,
    accept: AcceptMode,
    state: MissionTrackState,
}

impl Default for MissionMeta {
    fn default() -> Self {
        Self {
            gated: false,
            accept: AcceptMode::Review,
            state: MissionTrackState::Open,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResidentTrackState {
    Proposed,
    Approved,
    Retired,
}

#[derive(Default)]
struct ReplayState {
    missions: BTreeMap<i64, MissionMeta>,
    residents: BTreeMap<String, ResidentTrackState>,
    episodes: BTreeMap<i64, TaskEpisode>,
}

/// Replay an ordered slice of `(event_seq, recorded_at_ms, ControlRecord)`
/// entries into deterministic [`TaskEpisode`] projections matching SQL
/// `spawn_control_apply` & `spawn_task_spans`.
pub fn replay_task_episodes(records: &[(i64, i64, ControlRecord)]) -> Vec<TaskEpisode> {
    let mut state = ReplayState::default();
    for (seq, at_ms, record) in records {
        state.apply(*seq, *at_ms, record);
    }
    let mut out: Vec<TaskEpisode> = state.episodes.into_values().collect();
    out.sort_by_key(|ep| (ep.assigned_seq, ep.task_id));
    out
}

impl ReplayState {
    fn apply(&mut self, seq: i64, at_ms: i64, record: &ControlRecord) {
        match record {
            ControlRecord::MissionAdded(r) => {
                self.missions.insert(r.id, MissionMeta::default());
            }
            ControlRecord::MissionConfigured(r) => {
                let meta = self.missions.entry(r.id).or_default();
                meta.gated = r.gate.is_some();
                meta.accept = r.accept.unwrap_or(AcceptMode::Review);
            }
            ControlRecord::MissionClosed(r) => self.on_mission_closed(seq, at_ms, r),
            ControlRecord::ResidentProposed(r) => {
                self.residents
                    .entry(format!("agent_resident_{}", r.name))
                    .or_insert(ResidentTrackState::Proposed);
            }
            ControlRecord::ResidentApproved(r) => {
                self.residents.insert(
                    format!("agent_resident_{}", r.name),
                    ResidentTrackState::Approved,
                );
            }
            ControlRecord::ResidentRetired(r) => self.on_resident_retired(seq, at_ms, &r.name),
            ControlRecord::TaskAssigned(r) => self.on_task_assigned(seq, at_ms, r),
            ControlRecord::TaskDelivered(r) => self.on_task_delivered(seq, at_ms, r),
            ControlRecord::TaskReported(r) => self.on_task_reported(seq, at_ms, r),
            ControlRecord::TaskReviewed(r) => self.on_task_reviewed(seq, at_ms, r),
            ControlRecord::TaskRerouted(r) => self.on_task_rerouted(seq, at_ms, r),
            ControlRecord::TaskRestarted(r) => self.on_task_restarted(seq, at_ms, r),
            ControlRecord::TaskStolen(r) => self.on_task_stolen(seq, at_ms, r),
            ControlRecord::TaskGated(r) => self.on_task_gated(seq, at_ms, r),
            ControlRecord::TaskLanded(r) => self.on_task_landed(seq, at_ms, r.id),
            ControlRecord::TaskAborted(r) => self.on_task_aborted(seq, at_ms, r),
            ControlRecord::TaskOverBudget(r) => self.on_task_over_budget(seq, at_ms, r),
            ControlRecord::TaskCancelled(r) => self.on_task_cancelled(seq, at_ms, r),
            ControlRecord::TaskReportUnfinished(r) => {
                self.record_stage(r.id, seq, at_ms, record.kind());
            }
            ControlRecord::TaskForkFailed(r) => self.record_stage(r.id, seq, at_ms, record.kind()),
            ControlRecord::TaskShadowGated(r) => self.record_stage(r.id, seq, at_ms, record.kind()),
            ControlRecord::TaskHeld(r) => self.record_stage(r.id, seq, at_ms, record.kind()),
            ControlRecord::TaskReleased(r) => self.record_stage(r.id, seq, at_ms, record.kind()),
            ControlRecord::LegacyImported(r) => self.on_legacy_imported(seq, at_ms, r),
            _ => {}
        }
    }

    fn record_stage(&mut self, task_id: i64, seq: i64, at_ms: i64, kind: ControlRecordKind) {
        if let Some(ep) = self.episodes.get_mut(&task_id) {
            ep.updated_at_ms = at_ms;
            ep.stages.push(EpisodeStage {
                event_seq: seq,
                at_ms,
                kind,
            });
        }
    }

    fn on_mission_closed(&mut self, seq: i64, at_ms: i64, r: &MissionClosedRecord) {
        let meta = self.missions.entry(r.id).or_default();
        meta.state = match r.state {
            MissionCloseState::Done => MissionTrackState::Done,
            MissionCloseState::Dropped => MissionTrackState::Dropped,
        };
        let meta_copy = *meta;
        for ep in self.episodes.values_mut() {
            if ep.mission_id != r.id {
                continue;
            }
            let resident = self.residents.get(&ep.agent_id).copied();
            if !task_may_continue(ep.state, meta_copy, resident) && ep.state != TaskState::Cancelled
            {
                ep.state = TaskState::Cancelled;
                ep.outcome = EpisodeOutcome::Cancelled;
                ep.updated_at_ms = at_ms;
                ep.closed_seq.get_or_insert(seq);
                ep.closed_at_ms.get_or_insert(at_ms);
            }
        }
    }

    fn on_resident_retired(&mut self, seq: i64, at_ms: i64, name: &str) {
        let agent_id = format!("agent_resident_{name}");
        self.residents
            .insert(agent_id.clone(), ResidentTrackState::Retired);
        for ep in self.episodes.values_mut() {
            if ep.agent_id != agent_id {
                continue;
            }
            let meta = self
                .missions
                .get(&ep.mission_id)
                .copied()
                .unwrap_or_default();
            if !task_may_continue(ep.state, meta, Some(ResidentTrackState::Retired))
                && ep.state != TaskState::Cancelled
            {
                ep.state = TaskState::Cancelled;
                ep.outcome = EpisodeOutcome::Cancelled;
                ep.updated_at_ms = at_ms;
                ep.closed_seq.get_or_insert(seq);
                ep.closed_at_ms.get_or_insert(at_ms);
            }
        }
    }

    fn on_task_assigned(&mut self, seq: i64, at_ms: i64, r: &TaskAssignedRecord) {
        let meta = self.missions.entry(r.mission_id).or_default();
        if meta.state == MissionTrackState::Open {
            meta.state = MissionTrackState::Active;
        }
        let first_preset_id = r.first_preset_id.clone().or_else(|| r.preset_id.clone());
        let ep = TaskEpisode {
            task_id: r.id,
            mission_id: r.mission_id,
            kind: r.kind,
            agent_id: r.agent_id.clone(),
            session_id: r.session_id.clone(),
            brief: r.brief.clone(),
            created_by: r.created_by.clone(),
            objective: r.objective,
            scenario: r.scenario.clone(),
            model: r.model.clone().unwrap_or_else(|| "default".to_string()),
            difficulty: r.difficulty.unwrap_or(GradeWire::L2),
            preset_id: r.preset_id.clone(),
            first_preset_id,
            engine: r.engine.clone(),
            model_label: r.model_label.clone(),
            pinned: r.pinned.unwrap_or(false),
            reissue_of: r.reissue_of,
            continues: r.continues,
            reassign_of: r.reassign_of,
            state: TaskState::Assigned,
            outcome: EpisodeOutcome::InFlight,
            rework: 0,
            quality_rejections: r.quality_rejections.unwrap_or(0),
            reroutes: 0,
            restarts: 0,
            steals: 0,
            deliveries: 0,
            reports: 0,
            last_command_id: None,
            last_report: None,
            last_report_commit: None,
            review_note: None,
            base: None,
            candidate: None,
            metric: None,
            last_gate_verdict: None,
            review_sync_points: Vec::new(),
            gate_verdicts: Vec::new(),
            terminal_by: None,
            terminal_reason: None,
            assigned_seq: seq,
            assigned_at_ms: at_ms,
            delivered_at_ms: None,
            closed_seq: None,
            closed_at_ms: None,
            updated_at_ms: at_ms,
            stages: vec![EpisodeStage {
                event_seq: seq,
                at_ms,
                kind: ControlRecordKind::TaskAssigned,
            }],
        };
        self.episodes.insert(r.id, ep);
    }

    fn on_task_delivered(&mut self, seq: i64, at_ms: i64, r: &TaskDeliveredRecord) {
        if let Some(ep) = self.episodes.get_mut(&r.id) {
            ep.state = TaskState::Delivered;
            ep.deliveries += 1;
            ep.last_command_id = Some(r.command_id);
            ep.delivered_at_ms = Some(at_ms);
            ep.updated_at_ms = at_ms;
            ep.stages.push(EpisodeStage {
                event_seq: seq,
                at_ms,
                kind: ControlRecordKind::TaskDelivered,
            });
        }
    }

    fn on_task_reported(&mut self, seq: i64, at_ms: i64, r: &TaskReportedRecord) {
        if let Some(ep) = self.episodes.get_mut(&r.id) {
            ep.state = TaskState::Reported;
            ep.reports += 1;
            ep.last_report = Some(r.report.clone());
            ep.last_report_commit.clone_from(&r.commit);
            ep.updated_at_ms = at_ms;
            ep.stages.push(EpisodeStage {
                event_seq: seq,
                at_ms,
                kind: ControlRecordKind::TaskReported,
            });
        }
    }

    fn on_task_reviewed(&mut self, seq: i64, at_ms: i64, r: &TaskReviewedRecord) {
        let Some(ep) = self.episodes.get_mut(&r.id) else {
            return;
        };
        ep.stages.push(EpisodeStage {
            event_seq: seq,
            at_ms,
            kind: ControlRecordKind::TaskReviewed,
        });
        if let Some(expected_rev) = r.revision
            && ep.rework != expected_rev
        {
            return;
        }
        let reviewed_revision = ep.rework;
        ep.review_sync_points.push(ReviewSyncPoint {
            event_seq: seq,
            at_ms,
            revision: reviewed_revision,
            accepted: r.accepted,
            note: r.note.clone(),
        });
        ep.updated_at_ms = at_ms;
        if r.accepted {
            ep.state = TaskState::Accepted;
            let gated = self.missions.get(&ep.mission_id).is_some_and(|m| m.gated);
            if !gated {
                ep.outcome = EpisodeOutcome::Accepted;
                ep.closed_seq.get_or_insert(seq);
                ep.closed_at_ms.get_or_insert(at_ms);
            }
        } else {
            ep.state = TaskState::Revise;
            ep.review_note.clone_from(&r.note);
            ep.rework += 1;
            ep.quality_rejections += 1;
        }
    }

    fn on_task_rerouted(&mut self, seq: i64, at_ms: i64, r: &TaskReroutedRecord) {
        if let Some(ep) = self.episodes.get_mut(&r.id) {
            ep.preset_id = Some(r.preset_id.clone());
            ep.model.clone_from(&r.model);
            ep.reroutes += 1;
            ep.updated_at_ms = at_ms;
            ep.stages.push(EpisodeStage {
                event_seq: seq,
                at_ms,
                kind: ControlRecordKind::TaskRerouted,
            });
        }
    }

    fn on_task_restarted(&mut self, seq: i64, at_ms: i64, r: &TaskRestartedRecord) {
        if let Some(ep) = self.episodes.get_mut(&r.task) {
            ep.restarts += 1;
            ep.updated_at_ms = at_ms;
            ep.stages.push(EpisodeStage {
                event_seq: seq,
                at_ms,
                kind: ControlRecordKind::TaskRestarted,
            });
        }
    }

    fn on_task_stolen(&mut self, seq: i64, at_ms: i64, r: &TaskStolenRecord) {
        if let Some(ep) = self.episodes.get_mut(&r.id) {
            ep.session_id.clone_from(&r.to_session);
            ep.agent_id.clone_from(&r.to_agent);
            ep.steals += 1;
            ep.updated_at_ms = at_ms;
            ep.stages.push(EpisodeStage {
                event_seq: seq,
                at_ms,
                kind: ControlRecordKind::TaskStolen,
            });
        }
    }

    fn on_task_gated(&mut self, seq: i64, at_ms: i64, r: &TaskGatedRecord) {
        let Some(ep) = self.episodes.get_mut(&r.id) else {
            return;
        };
        ep.stages.push(EpisodeStage {
            event_seq: seq,
            at_ms,
            kind: ControlRecordKind::TaskGated,
        });
        if let Some(expected_rev) = r.revision
            && ep.rework != expected_rev
        {
            return;
        }
        // A record the strict parser refuses (no writer made one) moves
        // nothing: it is a stage, never a verdict.
        let Ok(outcome) = r.outcome() else {
            return;
        };
        let accept_mode = self
            .missions
            .get(&ep.mission_id)
            .map_or(AcceptMode::Review, |m| m.accept);
        let judged_revision = ep.rework;
        ep.gate_verdicts.push(GateSyncPoint {
            event_seq: seq,
            at_ms,
            revision: judged_revision,
            verdict: outcome.verdict(),
            class: r.class,
            base: r.base.clone(),
            candidate: r.candidate.clone(),
            note: r.note.clone(),
        });
        if let Some(base) = &r.base {
            ep.base = Some(base.clone());
        }
        ep.last_gate_verdict = Some(outcome.verdict());
        ep.updated_at_ms = at_ms;
        apply_gate_verdict(ep, outcome, accept_mode);
    }

    fn on_task_landed(&mut self, seq: i64, at_ms: i64, task_id: i64) {
        if let Some(ep) = self.episodes.get_mut(&task_id) {
            ep.state = TaskState::Landed;
            ep.outcome = EpisodeOutcome::Landed;
            ep.updated_at_ms = at_ms;
            ep.closed_seq.get_or_insert(seq);
            ep.closed_at_ms.get_or_insert(at_ms);
            ep.stages.push(EpisodeStage {
                event_seq: seq,
                at_ms,
                kind: ControlRecordKind::TaskLanded,
            });
        }
    }

    fn on_task_aborted(&mut self, seq: i64, at_ms: i64, r: &TaskAbortedRecord) {
        if let Some(ep) = self.episodes.get_mut(&r.id) {
            ep.state = TaskState::Cancelled;
            ep.outcome = EpisodeOutcome::Aborted;
            ep.review_note.clone_from(&r.reason);
            ep.terminal_reason.clone_from(&r.reason);
            ep.updated_at_ms = at_ms;
            ep.closed_seq.get_or_insert(seq);
            ep.closed_at_ms.get_or_insert(at_ms);
            ep.stages.push(EpisodeStage {
                event_seq: seq,
                at_ms,
                kind: ControlRecordKind::TaskAborted,
            });
        }
    }

    fn on_task_over_budget(&mut self, seq: i64, at_ms: i64, r: &TaskOverBudgetRecord) {
        if let Some(ep) = self.episodes.get_mut(&r.id) {
            ep.state = TaskState::Cancelled;
            ep.outcome = EpisodeOutcome::OverBudget;
            ep.review_note = Some(r.reason.clone());
            ep.terminal_reason = Some(r.reason.clone());
            ep.updated_at_ms = at_ms;
            ep.closed_seq.get_or_insert(seq);
            ep.closed_at_ms.get_or_insert(at_ms);
            ep.stages.push(EpisodeStage {
                event_seq: seq,
                at_ms,
                kind: ControlRecordKind::TaskOverBudget,
            });
        }
    }

    fn on_task_cancelled(&mut self, seq: i64, at_ms: i64, r: &TaskCancelledRecord) {
        if let Some(ep) = self.episodes.get_mut(&r.id) {
            ep.state = TaskState::Cancelled;
            ep.outcome = if r.yielded == Some(true) {
                EpisodeOutcome::Rejected
            } else {
                EpisodeOutcome::Cancelled
            };
            if ep.terminal_by.is_none() {
                ep.terminal_by.clone_from(&r.by);
            }
            if ep.terminal_reason.is_none() {
                ep.terminal_reason.clone_from(&r.reason);
            }
            ep.updated_at_ms = at_ms;
            ep.closed_seq.get_or_insert(seq);
            ep.closed_at_ms.get_or_insert(at_ms);
            ep.stages.push(EpisodeStage {
                event_seq: seq,
                at_ms,
                kind: ControlRecordKind::TaskCancelled,
            });
        }
    }

    fn on_legacy_imported(&mut self, seq: i64, at_ms: i64, r: &LegacyImportedRecord) {
        match r.table {
            LegacyTable::MissionMissions => self.import_legacy_mission(&r.row),
            LegacyTable::MissionResidents => self.import_legacy_resident(&r.row),
            LegacyTable::MissionTasks => self.import_legacy_task(seq, at_ms, &r.row),
        }
    }

    fn import_legacy_mission(&mut self, row: &Value) {
        let Some(id) = row.get("id").and_then(Value::as_i64) else {
            return;
        };
        let state = match row.get("state").and_then(Value::as_str) {
            Some("active") => MissionTrackState::Active,
            Some("done") => MissionTrackState::Done,
            Some("dropped") => MissionTrackState::Dropped,
            _ => MissionTrackState::Open,
        };
        let accept = match row.get("accept").and_then(Value::as_str) {
            Some("gate") => AcceptMode::Gate,
            _ => AcceptMode::Review,
        };
        self.missions.insert(
            id,
            MissionMeta {
                gated: row.get("gate").is_some_and(|g| !g.is_null()),
                accept,
                state,
            },
        );
    }

    fn import_legacy_resident(&mut self, row: &Value) {
        let Some(agent_id) = row.get("agent_id").and_then(Value::as_str) else {
            return;
        };
        let st = match row.get("state").and_then(Value::as_str) {
            Some("approved") => ResidentTrackState::Approved,
            Some("retired") => ResidentTrackState::Retired,
            _ => ResidentTrackState::Proposed,
        };
        self.residents.insert(agent_id.to_string(), st);
    }

    fn import_legacy_task(&mut self, seq: i64, at_ms: i64, row: &Value) {
        let Some(id) = row.get("id").and_then(Value::as_i64) else {
            return;
        };
        let Some(mission_id) = row.get("mission_id").and_then(Value::as_i64) else {
            return;
        };
        let state = legacy_opt_str(row, "state")
            .and_then(|s| s.parse::<TaskState>().ok())
            .unwrap_or(TaskState::Assigned);
        let gated = self.missions.get(&mission_id).is_some_and(|m| m.gated);
        let outcome = match state {
            TaskState::Landed => EpisodeOutcome::Landed,
            TaskState::Cancelled => EpisodeOutcome::Cancelled,
            TaskState::Accepted if !gated => EpisodeOutcome::Accepted,
            _ => EpisodeOutcome::InFlight,
        };
        let preset_id = legacy_opt_str(row, "preset_id");
        let first_preset_id = legacy_opt_str(row, "first_preset_id").or_else(|| preset_id.clone());
        let ep = build_legacy_episode(
            seq,
            at_ms,
            id,
            mission_id,
            state,
            outcome,
            preset_id,
            first_preset_id,
            row,
        );
        self.episodes.insert(id, ep);
    }
}

fn legacy_opt_str(row: &Value, key: &str) -> Option<String> {
    row.get(key).and_then(Value::as_str).map(str::to_string)
}

fn legacy_opt_u32(row: &Value, key: &str) -> Option<u32> {
    row.get(key)
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
}

#[allow(clippy::too_many_arguments)]
fn build_legacy_episode(
    seq: i64,
    at_ms: i64,
    id: i64,
    mission_id: i64,
    state: TaskState,
    outcome: EpisodeOutcome,
    preset_id: Option<String>,
    first_preset_id: Option<String>,
    row: &Value,
) -> TaskEpisode {
    let kind = match row.get("kind").and_then(Value::as_str) {
        Some("resident") => TaskKind::Resident,
        _ => TaskKind::Ephemeral,
    };
    let difficulty = row
        .get("difficulty")
        .and_then(Value::as_i64)
        .and_then(GradeWire::from_rank)
        .unwrap_or(GradeWire::L2);
    let updated_at_ms = row
        .get("updated_at_ms")
        .and_then(Value::as_i64)
        .unwrap_or(at_ms);
    TaskEpisode {
        task_id: id,
        mission_id,
        kind,
        agent_id: legacy_opt_str(row, "agent_id").unwrap_or_default(),
        session_id: legacy_opt_str(row, "session_id").unwrap_or_default(),
        brief: legacy_opt_str(row, "brief").unwrap_or_default(),
        created_by: legacy_opt_str(row, "created_by"),
        objective: row
            .get("objective")
            .and_then(Value::as_i64)
            .and_then(|n| i32::try_from(n).ok()),
        scenario: legacy_opt_str(row, "scenario"),
        model: legacy_opt_str(row, "model").unwrap_or_else(|| "default".to_string()),
        difficulty,
        preset_id,
        first_preset_id,
        engine: legacy_opt_str(row, "engine"),
        model_label: legacy_opt_str(row, "model_label"),
        pinned: row.get("pinned").and_then(Value::as_bool).unwrap_or(false),
        reissue_of: row.get("reissue_of").and_then(Value::as_i64),
        continues: row.get("continues").and_then(Value::as_i64),
        reassign_of: row.get("reassign_of").and_then(Value::as_i64),
        state,
        outcome,
        rework: legacy_opt_u32(row, "revisions").unwrap_or(0),
        quality_rejections: legacy_opt_u32(row, "quality_rejections").unwrap_or(0),
        reroutes: 0,
        restarts: 0,
        steals: 0,
        deliveries: u32::from(row.get("delivered_at_ms").is_some_and(|v| !v.is_null())),
        reports: u32::from(row.get("report").is_some_and(|v| !v.is_null())),
        last_command_id: row.get("command_id").and_then(Value::as_i64),
        last_report: legacy_opt_str(row, "report"),
        last_report_commit: None,
        review_note: legacy_opt_str(row, "review_note"),
        base: legacy_opt_str(row, "base"),
        candidate: legacy_opt_str(row, "candidate"),
        metric: legacy_opt_str(row, "metric"),
        last_gate_verdict: None,
        review_sync_points: Vec::new(),
        gate_verdicts: Vec::new(),
        terminal_by: None,
        terminal_reason: None,
        assigned_seq: seq,
        assigned_at_ms: row
            .get("created_at_ms")
            .and_then(Value::as_i64)
            .unwrap_or(at_ms),
        delivered_at_ms: row.get("delivered_at_ms").and_then(Value::as_i64),
        closed_seq: (outcome != EpisodeOutcome::InFlight).then_some(seq),
        closed_at_ms: (outcome != EpisodeOutcome::InFlight).then_some(updated_at_ms),
        updated_at_ms,
        stages: Vec::new(),
    }
}

fn apply_gate_verdict(ep: &mut TaskEpisode, outcome: GateOutcome<'_>, accept_mode: AcceptMode) {
    match outcome {
        GateOutcome::Accepted { candidate, metric } => {
            ep.state = TaskState::Landing;
            ep.candidate = candidate.map(str::to_string);
            if let Some(metric) = metric {
                ep.metric = Some(metric.to_string());
            }
        }
        GateOutcome::Rejected { note } => {
            ep.state = TaskState::Revise;
            ep.candidate = None;
            ep.review_note = note.map(str::to_string);
            ep.rework += 1;
            ep.quality_rejections += 1;
        }
        GateOutcome::Conflict { note } | GateOutcome::Incompatible { note } => {
            ep.state = TaskState::Revise;
            ep.candidate = None;
            ep.review_note = note.map(str::to_string);
            ep.rework += 1;
        }
        GateOutcome::Stale => {
            ep.state = match accept_mode {
                AcceptMode::Gate => TaskState::Reported,
                AcceptMode::Review => TaskState::Accepted,
            };
            ep.candidate = None;
        }
        GateOutcome::Aborted | GateOutcome::NoRoom { .. } | GateOutcome::HostFault { .. } => {}
    }
}

/// Closure rule matching SQL `spawn_task_may_continue` (`schema.sql`).
const fn task_may_continue(
    state: TaskState,
    mission: MissionMeta,
    resident: Option<ResidentTrackState>,
) -> bool {
    let resident_retired = matches!(resident, Some(ResidentTrackState::Retired));
    let mission_open = matches!(
        mission.state,
        MissionTrackState::Open | MissionTrackState::Active
    );
    match state {
        TaskState::Cancelled | TaskState::Landed => true,
        TaskState::Accepted if !mission.gated => true,
        _ if mission_open && !resident_retired => true,
        TaskState::Accepted | TaskState::Landing
            if mission.gated && !matches!(mission.state, MissionTrackState::Dropped) =>
        {
            true
        }
        TaskState::Delivered | TaskState::Reported
            if mission.gated
                && matches!(mission.state, MissionTrackState::Done)
                && !resident_retired =>
        {
            true
        }
        _ => false,
    }
}

/// Helper for tests & tooling verifying [`ControlRecord::ALL_KINDS`] and
/// [`ControlRecordKind::ALL`] uniqueness and bijective alignment.
pub fn control_kind_set() -> BTreeSet<&'static str> {
    ControlRecord::ALL_KINDS.iter().copied().collect()
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::{Value, json};

    use super::{
        ControlRecord, ControlRecordError, ControlRecordKind, EpisodeOutcome, GateOutcome,
        GateVerdict, GateVerdictClass, GradeWire, GradingRecord, GradingRecordKind, HostRetry,
        TaskGatedRecord, invariant_err, replay_task_episodes,
    };
    use crate::mission::TaskState;

    fn sample_control_payloads() -> Vec<Value> {
        let sha_a = "a".repeat(40);
        let sha_b = "b".repeat(40);
        vec![
            json!({"type": "mission.added", "id": 1, "text": "ship", "origin": "human", "sources": ["/tmp/repo"]}),
            json!({"type": "mission.configured", "id": 1, "gate": "/tmp/gate.sh", "accept": "gate", "budget": {"tool_calls": 5}, "objectives": null}),
            json!({"type": "mission.closed", "id": 1, "state": "done", "summary": "ok"}),
            json!({"type": "mission.halted", "id": 1, "reason": "main moved", "cause": "main_moved", "source": "/tmp/repo", "from": sha_a, "to": sha_b}),
            json!({"type": "mission.resumed", "id": 1, "by": "human", "halt": 42, "key": "res-1"}),
            json!({"type": "mission.paused", "id": 1, "reason": "maintenance"}),
            json!({"type": "mission.unpaused", "id": 1, "pause_event": 99}),
            json!({"type": "task.assigned", "id": 10, "mission_id": 1, "kind": "ephemeral", "agent_id": "agent_w_10", "session_id": "session_w_10", "brief": "do it", "model": "default", "created_by": "human", "difficulty": "l2", "preset_id": "neon"}),
            json!({"type": "task.delivered", "id": 10, "command_id": 50}),
            json!({"type": "task.reported", "id": 10, "report": "done", "commit": sha_a}),
            json!({"type": "task.report_unfinished", "id": 10, "revision": 0, "digest": "deadbeef", "markers": [{"line": 3}], "note": "finish todo", "command_id": 51}),
            json!({"type": "task.fork_failed", "id": 10, "from": 9, "commit": sha_a, "error": "git error"}),
            json!({"type": "task.reviewed", "id": 10, "accepted": false, "note": "fix bug", "revision": 0}),
            json!({"type": "task.rerouted", "id": 10, "preset_id": "fibble", "model": "default", "from_preset_id": "neon", "revision": 1, "routed_difficulty": "l3", "execution_profile": "heavy"}),
            json!({"type": "task.restarted", "task": 10, "session_id": "session_w_10", "agent_id": "agent_w_10", "execution_id": "exec-1", "by": "human", "reason": "wedged", "n": 1}),
            json!({"type": "task.cancelled", "id": 10, "by": "human", "reason": "no longer needed"}),
            json!({"type": "task.stolen", "id": 10, "from_session": "session_r_a", "from_agent": "agent_r_a", "to_session": "session_r_b", "to_agent": "agent_r_b", "pool": "workers", "by": "spawn-dispatch", "queued_since_ms": 1000}),
            json!({"type": "task.aborted", "id": 10, "reason": "stale limit"}),
            json!({"type": "task.over_budget", "id": 10, "reason": "exceeded", "tool_calls": 6, "budget": {"tool_calls": 5}}),
            json!({"type": "task.gated", "id": 10, "verdict": "accepted", "class": "code", "revision": 1, "base": sha_a, "candidate": sha_b}),
            json!({"type": "task.shadow_gated", "id": 10, "runner": "shadow-1", "revision": 1, "verdict": "accepted", "inputs": {"base": sha_a}, "unequal": [], "agrees": true}),
            json!({"type": "task.held", "id": 10, "hold": 77, "source": "/tmp/repo"}),
            json!({"type": "task.released", "id": 10, "by": "human", "reason": "flaky fixed"}),
            json!({"type": "task.landed", "id": 10, "from": sha_a, "to": sha_b, "missions": [2]}),
            json!({"type": "resident.proposed", "name": "core", "charter": "own core", "proposed_by": "human", "pool": "workers"}),
            json!({"type": "resident.approved", "name": "core"}),
            json!({"type": "resident.retired", "name": "core"}),
            json!({"type": "objective.solved", "id": 1, "objective": 12, "task": 10, "scenario": "012.sh", "base": sha_a}),
            json!({"type": "objective.verified", "mission": 1, "objective": 12, "by": "owner", "how": "live curl", "result": "pass"}),
            json!({"type": "objective.retired", "id": 1, "objective": 13, "kind": "obsolete", "by": "owner", "reason": "superseded"}),
            json!({"type": "source.moved", "source": "/tmp/repo", "from": sha_a, "to": sha_b, "by": "human", "missions": [1]}),
            json!({"type": "source.held", "source": "/tmp/repo", "main": sha_a, "by": "human", "bound_s": 60, "expires_ms": 9000}),
            json!({"type": "source.released", "source": "/tmp/repo", "hold": 77, "by": "human"}),
            json!({"type": "source.hold_expired", "source": "/tmp/repo", "hold": 77, "expires_ms": 9000, "waiting": [10]}),
            json!({"type": "source.push_requested", "source": "/tmp/repo", "commit": sha_b, "ref": format!("refs/spawn-owner/{sha_b}"), "by": "human", "gated": true}),
            json!({"type": "source.push_refused", "request": 88, "reason": "conflict", "source": "/tmp/repo", "commit": sha_b}),
            json!({"type": "source.push_no_verdict", "request": 88, "reason": "host fault"}),
            json!({"type": "source.main_red", "source": "/tmp/repo", "main": sha_a, "scenario": "check-a.sh", "failures": ["001"], "task": 10, "alert": true}),
            json!({"type": "source.main_probe", "source": "/tmp/repo", "main": sha_a, "task": 10, "probe_ms": 60000}),
            json!({"type": "source.main_green", "source": "/tmp/repo", "scenario": "check-a.sh", "main": sha_b, "since_ms": 1000, "red_main": sha_a}),
            json!({"type": "integrator.started", "pid": 123, "ppid": 1, "parent_comm": "init", "ancestors": [{"pid": 1, "comm": "init"}], "cwd": "/tmp", "cgroup": "/user.slice", "gate": "/tmp/g.sh", "gate_from_env": false, "root": "/tmp", "once": true}),
            json!({"type": "integrator.draining", "bound_s": 300, "gates": [10]}),
            json!({"type": "integrator.drained", "bound_s": 300}),
            json!({"type": "integrator.drain_expired", "bound_s": 300, "gates": [10]}),
            json!({"type": "host.disk", "path": "/tmp", "state": "low", "free_bytes": 1024}),
            json!({"type": "host.storage", "archive_bytes": 10, "archive_files": 1, "projects_bytes": 20, "projects_files": 2}),
            json!({"type": "host.core_lines", "values": {"spawn_core": 500}}),
            json!({"type": "host.storage_bytes", "sources": {"pg": {"bytes": 4096}}}),
            json!({"type": "host.judge", "judge": "jev", "state": "down", "error": "timeout", "first_failed_ms": 100}),
            json!({"type": "host.messages_marked", "started_at_ms": 100}),
            json!({"type": "host.loop_started", "boot_id": "boot-1", "started_at_ms": 100}),
            json!({"type": "execution.interrupted", "agent_id": "a", "session_id": "s", "execution_id": "e", "cause": "stopped", "path": "final", "attempt": 1, "evidence": {}, "due_ms": null}),
            json!({"type": "execution.cleared", "execution_id": "e", "session_id": "s", "cause": "model_unavailable", "waited_ms": 500}),
            json!({"type": "alert.fired", "key": "k1", "kind": "disk_low", "subject": "host", "text": "low"}),
            json!({"type": "alert.updated", "key": "k1", "kind": "unverified_landings", "subject": "host", "text": "2"}),
            json!({"type": "alert.judged", "key": "k1", "verdict": "right"}),
            json!({"type": "alert.cleared", "key": "k1", "kind": "disk_low", "subject": "host"}),
            json!({"type": "deploy.finished", "ok": true, "head": sha_a, "by": "owner", "reason": ""}),
            json!({"type": "backup.dumped", "keys": ["pg/a.dump"], "bytes": 100}),
            json!({"type": "backup.bundled", "kind": "full", "base_event_id": null, "keys": []}),
            json!({"type": "backup.verified", "ok": true, "reason": ""}),
            json!({"type": "backup.stored", "area": "sessions", "host": "h", "account": "a", "manifest": "m", "files": 1, "unchanged": 0, "grown": 0, "whole": 1, "segments_written": 1, "segments_found": 0, "bytes_written": 10}),
            json!({"type": "backup.drilled.transcripts", "ok": true, "reason": "", "bundle": "b", "bundle_event_id": 1, "mismatches": 0, "duration_ms": 5}),
            json!({"type": "backup.drilled.missions", "ok": true, "reason": "", "bundle": "b", "bundle_event_id": 1, "mismatches": 0, "duration_ms": 5}),
            json!({"type": "backup.drilled.controllers", "ok": false, "reason": "fail", "bundle": "b", "bundle_event_id": 1, "mismatches": 1, "duration_ms": 5}),
            json!({"type": "backup.drilled.sync", "ok": true, "reason": "", "bundle": "b", "bundle_event_id": 1, "mismatches": 0, "duration_ms": 5}),
            json!({"type": "legacy.imported", "table": "mission_missions", "row": {"id": 1, "text": "old"}}),
        ]
    }

    #[test]
    fn every_control_and_grading_variant_round_trips_and_covers_all_kinds() {
        let samples = sample_control_payloads();
        assert_eq!(ControlRecord::ALL_KINDS.len(), ControlRecordKind::ALL.len());
        let mut seen = BTreeSet::new();
        for payload in &samples {
            let parsed = ControlRecord::from_value_strict(payload).expect("strict parse");
            seen.insert(parsed.kind_str());
            let wire = parsed.to_value().expect("serialize");
            let reparsed = ControlRecord::from_value(&wire).expect("reparse");
            assert_eq!(reparsed, parsed);
        }
        let expected: BTreeSet<&str> = ControlRecord::ALL_KINDS.iter().copied().collect();
        assert_eq!(seen, expected, "every ControlRecord kind must be covered");

        let grading_samples = [
            json!({"type": "preset.registered", "preset_id": "neon", "engine_label": "agy", "model_label": "neon", "model_profile": "default", "cost_rank": 1, "execution_profile": "standard"}),
            json!({"type": "preset.onboarded", "preset_id": "neon", "from_state": "unexamined", "to_state": "active", "to_grade": "l2", "passed": 4, "total": 4, "failed_cases": [], "suite": "evals"}),
            json!({"type": "preset.regraded", "preset_id": "neon", "decision": "demote", "from_grade": "l3", "to_grade": "l2", "from_state": "active", "to_state": "demoted", "first_pass": 1, "total": 5}),
            json!({"type": "preset.grade_locked", "preset_id": "neon", "from_grade": null, "to_grade": "l2", "from_state": "active", "to_state": "active"}),
            json!({"type": "preset.state_changed", "preset_id": "neon", "from_state": "active", "to_state": "disabled"}),
        ];
        let mut seen_grading = BTreeSet::new();
        for payload in &grading_samples {
            let parsed = GradingRecord::from_value_strict(payload).expect("strict grading parse");
            seen_grading.insert(parsed.kind_str());
            let wire = parsed.to_value().expect("serialize grading");
            assert_eq!(GradingRecord::from_value(&wire).expect("reparse"), parsed);
        }
        assert_eq!(seen_grading.len(), GradingRecordKind::ALL.len());
        let expected_grading: BTreeSet<&str> = GradingRecord::ALL_KINDS.iter().copied().collect();
        assert_eq!(seen_grading, expected_grading);
    }

    #[test]
    fn invalid_and_invariant_violating_records_are_rejected() {
        assert_eq!(
            ControlRecord::from_value(&json!(42)),
            Err(ControlRecordError::NotAnObject)
        );
        assert_eq!(
            ControlRecord::from_value(&json!({"id": 1})),
            Err(ControlRecordError::MissingType)
        );
        assert_eq!(
            ControlRecord::from_value(&json!({"type": "future.unknown"})),
            Err(ControlRecordError::UnknownKind(
                "future.unknown".to_string()
            ))
        );
        assert!(matches!(
            ControlRecord::from_value(
                &json!({"type": "task.delivered", "id": "not-an-int", "command_id": 1})
            ),
            Err(ControlRecordError::Deserialize { .. })
        ));
        assert!(matches!(
            ControlRecord::from_value_strict(&json!({
                "type": "task.gated", "id": 1, "verdict": "accepted", "class": "infrastructure"
            })),
            Err(ControlRecordError::Invariant { .. })
        ));
        assert!(matches!(
            ControlRecord::from_value_strict(&json!({
                "type": "deploy.finished", "ok": true, "head": "a".repeat(40),
                "by": "owner", "reason": "", "unexpected": 1
            })),
            Err(ControlRecordError::UnexpectedFields { .. })
        ));
        assert!(matches!(
            ControlRecord::from_value_strict(&json!({
                "type": "execution.interrupted", "agent_id": "a", "session_id": "s",
                "execution_id": "e", "cause": "stopped", "path": "final",
                "attempt": 1, "evidence": {}, "due_ms": 123
            })),
            Err(ControlRecordError::Invariant { .. })
        ));
    }

    #[test]
    fn compat_parser_preserves_legacy_strings_and_future_kinds() {
        let legacy = json!({"type": "task.delivered", "id": "77", "command_id": "9"});
        assert!(ControlRecord::from_value(&legacy).is_err());
        let compat = ControlRecord::from_value_compat(&legacy).expect("compat parse");
        let super::CompatControlRecord::Known(rec) = compat else {
            panic!("expected Known, got {compat:?}");
        };
        let ControlRecord::TaskDelivered(d) = *rec else {
            panic!("unexpected variant: {rec:?}");
        };
        assert_eq!(d.id, 77);
        assert_eq!(d.command_id, 9);

        let future =
            ControlRecord::from_value_compat(&json!({"type": "restore.drilled", "ok": true}))
                .expect("unknown future allowed only in compat mode");
        assert!(matches!(future, super::CompatControlRecord::Unknown { .. }));

        let sha_a = "a".repeat(40);
        let sha_b = "b".repeat(40);
        let moved_ints = json!({"type": "source.moved", "source": "/s", "from": sha_a, "to": sha_b, "missions": [1, 2]});
        let moved_strs = json!({"type": "source.moved", "source": "/s", "from": sha_a, "to": sha_b, "missions": ["1", "2"], "clear": ["3"]});
        let p_ints = ControlRecord::from_value(&moved_ints).expect("int missions");
        let p_strs = ControlRecord::from_value(&moved_strs).expect("str missions");
        assert!(
            matches!(p_ints, ControlRecord::SourceMoved(ref m) if m.missions.as_deref() == Some(&[1, 2]))
        );
        assert!(
            matches!(p_strs, ControlRecord::SourceMoved(ref m) if m.missions.as_deref() == Some(&[1, 2]) && m.clear.as_deref() == Some(&[3]))
        );
    }

    fn parse_stream(entries: &[(i64, i64, Value)]) -> Vec<(i64, i64, ControlRecord)> {
        entries
            .iter()
            .map(|(s, t, v)| (*s, *t, ControlRecord::from_value(v).expect("parse")))
            .collect()
    }

    #[test]
    fn replay_review_mission_tracks_sync_points_rework_reroutes_and_restart() {
        let stream = parse_stream(&[
            (
                1,
                100,
                json!({"type": "mission.added", "id": 1, "text": "review", "sources": []}),
            ),
            (
                2,
                110,
                json!({"type": "task.assigned", "id": 10, "mission_id": 1, "kind": "ephemeral", "agent_id": "agent_a", "session_id": "session_a", "brief": "task A", "model": "cheap", "created_by": "human", "difficulty": "l1", "preset_id": "cheap-l1"}),
            ),
            (
                3,
                120,
                json!({"type": "task.delivered", "id": 10, "command_id": 1}),
            ),
            (
                4,
                130,
                json!({"type": "task.reported", "id": 10, "report": "v0"}),
            ),
            (
                5,
                135,
                json!({"type": "task.reviewed", "id": 10, "accepted": true, "note": "stale", "revision": 99}),
            ),
            (
                6,
                140,
                json!({"type": "task.reviewed", "id": 10, "accepted": false, "note": "needs tests", "revision": 0}),
            ),
            (
                7,
                145,
                json!({"type": "task.rerouted", "id": 10, "preset_id": "neon-l2", "model": "default", "from_preset_id": "cheap-l1", "revision": 1, "routed_difficulty": "l2"}),
            ),
            (
                8,
                150,
                json!({"type": "task.delivered", "id": 10, "command_id": 2}),
            ),
            (
                9,
                160,
                json!({"type": "task.restarted", "task": 10, "session_id": "session_a", "agent_id": "agent_a"}),
            ),
            (
                10,
                170,
                json!({"type": "task.reported", "id": 10, "report": "v1"}),
            ),
            (
                11,
                180,
                json!({"type": "task.reviewed", "id": 10, "accepted": true, "note": "", "revision": 1}),
            ),
        ]);

        let episodes = replay_task_episodes(&stream);
        assert_eq!(episodes.len(), 1);
        let ep10 = &episodes[0];
        assert_eq!(ep10.task_id, 10);
        assert_eq!(ep10.state, TaskState::Accepted);
        assert_eq!(ep10.outcome, EpisodeOutcome::Accepted);
        assert_eq!(ep10.rework, 1);
        assert_eq!(ep10.quality_rejections, 1);
        assert_eq!(ep10.reroutes, 1);
        assert_eq!(ep10.restarts, 1);
        assert_eq!(ep10.first_preset_id.as_deref(), Some("cheap-l1"));
        assert_eq!(ep10.preset_id.as_deref(), Some("neon-l2"));
        assert_eq!(ep10.difficulty, GradeWire::L1);
        assert_eq!(ep10.review_sync_points.len(), 2);
        assert!(!ep10.review_sync_points[0].accepted);
        assert_eq!(ep10.review_sync_points[0].revision, 0);
        assert!(ep10.review_sync_points[1].accepted);
        assert_eq!(ep10.review_sync_points[1].revision, 1);
    }

    fn gated_mission_stream() -> Vec<(i64, i64, ControlRecord)> {
        parse_stream(&[
            (
                1,
                200,
                json!({"type": "mission.added", "id": 2, "text": "gated", "sources": ["/tmp/r"]}),
            ),
            (
                2,
                210,
                json!({"type": "mission.configured", "id": 2, "gate": "/tmp/g.sh", "accept": "gate"}),
            ),
            (
                3,
                220,
                json!({"type": "resident.proposed", "name": "w1", "charter": "c", "pool": "p"}),
            ),
            (4, 225, json!({"type": "resident.approved", "name": "w1"})),
            (
                5,
                230,
                json!({"type": "resident.proposed", "name": "w2", "charter": "c", "pool": "p"}),
            ),
            (6, 235, json!({"type": "resident.approved", "name": "w2"})),
            (
                7,
                240,
                json!({"type": "task.assigned", "id": 20, "mission_id": 2, "kind": "resident", "agent_id": "agent_resident_w1", "session_id": "session_resident_w1", "brief": "gated task", "created_by": "spawn-integrate", "objective": 5}),
            ),
            (
                8,
                250,
                json!({"type": "task.stolen", "id": 20, "from_session": "session_resident_w1", "from_agent": "agent_resident_w1", "to_session": "session_resident_w2", "to_agent": "agent_resident_w2", "pool": "p", "by": "spawn-dispatch"}),
            ),
            (
                9,
                260,
                json!({"type": "task.delivered", "id": 20, "command_id": 3}),
            ),
            (
                10,
                270,
                json!({"type": "task.reported", "id": 20, "report": "g0", "commit": "c0"}),
            ),
            (
                11,
                280,
                json!({"type": "task.gated", "id": 20, "verdict": "host_fault", "revision": 0, "base": "b0"}),
            ),
            (
                12,
                290,
                json!({"type": "task.gated", "id": 20, "verdict": "rejected", "revision": 0, "note": "fail"}),
            ),
            (
                13,
                300,
                json!({"type": "task.delivered", "id": 20, "command_id": 4}),
            ),
            (
                14,
                310,
                json!({"type": "task.reported", "id": 20, "report": "g1", "commit": "c1"}),
            ),
            (
                15,
                320,
                json!({"type": "task.gated", "id": 20, "verdict": "conflict", "revision": 1, "note": "rebase"}),
            ),
            (
                16,
                330,
                json!({"type": "task.delivered", "id": 20, "command_id": 5}),
            ),
            (
                17,
                340,
                json!({"type": "task.reported", "id": 20, "report": "g2", "commit": "c2"}),
            ),
            (
                18,
                350,
                json!({"type": "task.gated", "id": 20, "verdict": "accepted", "revision": 2, "base": "b0", "candidate": "c2"}),
            ),
            (
                19,
                360,
                json!({"type": "task.landed", "id": 20, "from": "b0", "to": "b1"}),
            ),
        ])
    }

    #[test]
    fn replay_gated_mission_tracks_steal_verdicts_and_landing() {
        let stream = gated_mission_stream();
        let episodes = replay_task_episodes(&stream);
        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].state, TaskState::Landed);
        assert_eq!(episodes[0].outcome, EpisodeOutcome::Landed);
        assert_eq!(episodes[0].steals, 1);
        assert_eq!(episodes[0].agent_id, "agent_resident_w2");
        assert_eq!(episodes[0].rework, 2);
        assert_eq!(episodes[0].quality_rejections, 1);
        assert_eq!(episodes[0].last_gate_verdict, Some(GateVerdict::Accepted));
        assert_eq!(episodes[0].gate_verdicts.len(), 4);
    }

    #[test]
    fn replay_tracks_over_budget_yield_and_mission_drop_closure() {
        let stream = parse_stream(&[
            (
                1,
                200,
                json!({"type": "mission.added", "id": 2, "text": "gated", "sources": ["/tmp/r"]}),
            ),
            (
                2,
                210,
                json!({"type": "mission.configured", "id": 2, "gate": "/tmp/g.sh", "accept": "gate"}),
            ),
            (
                3,
                370,
                json!({"type": "task.assigned", "id": 21, "mission_id": 2, "kind": "ephemeral", "agent_id": "agent_w_21", "session_id": "session_w_21", "brief": "over", "created_by": "spawn-integrate", "objective": 6}),
            ),
            (
                4,
                380,
                json!({"type": "task.delivered", "id": 21, "command_id": 6}),
            ),
            (
                5,
                390,
                json!({"type": "task.over_budget", "id": 21, "reason": "tool cap"}),
            ),
            (
                6,
                400,
                json!({"type": "task.assigned", "id": 22, "mission_id": 2, "kind": "ephemeral", "agent_id": "agent_w_22", "session_id": "session_w_22", "brief": "yield", "continues": 21, "created_by": "spawn-integrate", "objective": 6}),
            ),
            (
                7,
                410,
                json!({"type": "task.cancelled", "id": 22, "by": "spawn-integrate", "yielded": true}),
            ),
            (
                8,
                420,
                json!({"type": "task.assigned", "id": 23, "mission_id": 2, "kind": "ephemeral", "agent_id": "agent_w_23", "session_id": "session_w_23", "brief": "open at drop", "created_by": "human"}),
            ),
            (
                9,
                430,
                json!({"type": "mission.closed", "id": 2, "state": "dropped", "summary": "drop"}),
            ),
        ]);

        let episodes = replay_task_episodes(&stream);
        assert_eq!(episodes.len(), 3);
        assert_eq!(episodes[0].state, TaskState::Cancelled);
        assert_eq!(episodes[0].outcome, EpisodeOutcome::OverBudget);
        assert_eq!(episodes[1].state, TaskState::Cancelled);
        assert_eq!(episodes[1].outcome, EpisodeOutcome::Rejected);
        assert_eq!(episodes[1].continues, Some(21));
        assert_eq!(episodes[2].state, TaskState::Cancelled);
        assert_eq!(episodes[2].outcome, EpisodeOutcome::Cancelled);
    }

    /// A `task.gated` record, parsed loosely (as replay reads the log).
    fn gated(value: Value) -> TaskGatedRecord {
        let mut value = value;
        value["type"] = json!("task.gated");
        match ControlRecord::from_value(&value).expect("parse") {
            ControlRecord::TaskGated(record) => *record,
            other => panic!("not task.gated: {other:?}"),
        }
    }

    const ALL_VERDICTS: [(&str, GateVerdictClass); 8] = [
        ("accepted", GateVerdictClass::Code),
        ("rejected", GateVerdictClass::Code),
        ("conflict", GateVerdictClass::Code),
        ("incompatible", GateVerdictClass::Code),
        ("aborted", GateVerdictClass::Infrastructure),
        ("no_room", GateVerdictClass::Infrastructure),
        ("host_fault", GateVerdictClass::Infrastructure),
        ("stale", GateVerdictClass::Superseded),
    ];

    const ALL_CLASSES: [GateVerdictClass; 3] = [
        GateVerdictClass::Code,
        GateVerdictClass::Infrastructure,
        GateVerdictClass::Superseded,
    ];

    /// Each verdict reads as its own variant, with no class (a record from
    /// before classes) or its own; every other class is refused, by
    /// `outcome` and the strict parser alike: 8 of the 32 pairs plus the 8
    /// classless records read, the other 16 do not.
    #[test]
    fn every_verdict_reads_as_its_variant_and_refuses_every_other_class() {
        let mut read = 0;
        for (verdict, class) in ALL_VERDICTS {
            let bare = gated(json!({"id": 1, "verdict": verdict}));
            let outcome = bare.outcome().expect("a classless record reads");
            assert_eq!(outcome.verdict().as_str(), verdict);
            assert_eq!(outcome.class(), class, "{verdict}");
            read += 1;
            for other in ALL_CLASSES {
                let value =
                    json!({"type": "task.gated", "id": 1, "verdict": verdict, "class": other});
                let record = gated(value.clone());
                if other == class {
                    assert_eq!(record.outcome(), Ok(outcome), "{verdict}");
                    ControlRecord::from_value_strict(&value).expect("strict");
                    read += 1;
                } else {
                    let want = format!(
                        "verdict `{verdict}` expects class `{}`, not `{other:?}`",
                        serde_json::to_string(&class).unwrap()
                    );
                    assert_eq!(
                        record.outcome(),
                        Err(invariant_err("task.gated", want.clone()))
                    );
                    assert_eq!(
                        ControlRecord::from_value_strict(&value),
                        Err(invariant_err("task.gated", want))
                    );
                }
            }
        }
        assert_eq!(read, 16);
    }

    #[test]
    fn each_variant_carries_what_its_readers_use() {
        let accepted = gated(
            json!({"id": 1, "verdict": "accepted", "candidate": "c2", "metric": "7", "note": "ignored"}),
        );
        assert_eq!(
            accepted.outcome(),
            Ok(GateOutcome::Accepted {
                candidate: Some("c2"),
                metric: Some("7")
            })
        );
        for (verdict, want) in [
            ("rejected", GateOutcome::Rejected { note: Some("why") }),
            ("conflict", GateOutcome::Conflict { note: Some("why") }),
            (
                "incompatible",
                GateOutcome::Incompatible { note: Some("why") },
            ),
        ] {
            assert_eq!(
                gated(json!({"id": 1, "verdict": verdict, "note": "why"})).outcome(),
                Ok(want)
            );
        }
        assert_eq!(
            gated(json!({"id": 1, "verdict": "aborted", "evidence": "parameter_error"})).outcome(),
            Ok(GateOutcome::Aborted)
        );
        assert_eq!(
            gated(json!({"id": 1, "verdict": "stale", "scenario_names": ["a.sh"]})).outcome(),
            Ok(GateOutcome::Stale)
        );
        assert_eq!(
            gated(json!({"id": 1, "verdict": "no_room"})).outcome(),
            Ok(GateOutcome::NoRoom { escalate: false })
        );
        assert_eq!(
            gated(json!({"id": 1, "verdict": "no_room", "escalate": true})).outcome(),
            Ok(GateOutcome::NoRoom { escalate: true })
        );
        // `escalate` was written on host faults and no_room only, but other
        // records never relied on its absence: it is read, not refused.
        assert!(
            gated(json!({"id": 1, "verdict": "accepted", "escalate": false}))
                .outcome()
                .is_ok()
        );
    }

    /// A host fault's retry: none on a record from before backoffs, the
    /// backoff alone before the store adds `due_ms`, both after; `due_ms`
    /// alone or a negative backoff are refused.
    #[test]
    fn a_host_faults_retry_reads_and_its_impossible_shapes_are_refused() {
        let host = |extra: Value| {
            let mut value = json!({"id": 1, "verdict": "host_fault"});
            value
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            gated(value)
        };
        let fault = |retry, escalate, parked| GateOutcome::HostFault {
            retry,
            escalate,
            parked,
        };
        assert_eq!(host(json!({})).outcome(), Ok(fault(None, false, false)));
        assert_eq!(
            host(json!({"backoff_ms": 600_000})).outcome(),
            Ok(fault(
                Some(HostRetry {
                    backoff_ms: 600_000,
                    due_ms: None
                }),
                false,
                false
            ))
        );
        assert_eq!(
            host(json!({"backoff_ms": 600_000, "due_ms": 1_793_930_000_000_i64, "escalate": true}))
                .outcome(),
            Ok(fault(
                Some(HostRetry {
                    backoff_ms: 600_000,
                    due_ms: Some(1_793_930_000_000)
                }),
                true,
                false
            ))
        );
        assert_eq!(
            host(
                json!({"backoff_ms": 0, "parked": true, "escalate": true, "evidence": "load_sensitive_parked"})
            )
            .outcome(),
            Ok(fault(
                Some(HostRetry {
                    backoff_ms: 0,
                    due_ms: None
                }),
                true,
                true
            ))
        );
        assert_eq!(
            host(json!({"due_ms": 5})).outcome(),
            Err(invariant_err("task.gated", "`due_ms` without `backoff_ms`"))
        );
        assert_eq!(
            host(json!({"backoff_ms": -1})).outcome(),
            Err(invariant_err(
                "task.gated",
                "`backoff_ms` must not be negative"
            ))
        );
    }

    /// `backoff_ms`, `due_ms`, `parked` and `alert` were only ever written on
    /// a host fault (`integrate/verdicts.rs`, `spawn_integrator_record`): on
    /// any other verdict the record is refused.
    #[test]
    fn a_host_faults_own_fields_on_another_verdict_are_refused() {
        let fields = [
            ("backoff_ms", json!(1000)),
            ("due_ms", json!(5)),
            ("parked", json!(true)),
            ("alert", json!(false)),
        ];
        let mut refused = 0;
        for (verdict, _) in ALL_VERDICTS.iter().filter(|(v, _)| *v != "host_fault") {
            for (field, value) in &fields {
                let mut record = json!({"type": "task.gated", "id": 1, "verdict": verdict});
                record[*field] = value.clone();
                let want = invariant_err(
                    "task.gated",
                    format!("`{field}` belongs to a host_fault verdict, not `{verdict}`"),
                );
                assert_eq!(gated(record.clone()).outcome(), Err(want.clone()));
                assert_eq!(ControlRecord::from_value_strict(&record), Err(want));
                refused += 1;
            }
        }
        assert_eq!(refused, 7 * 4);
    }

    /// Records shaped as the integrator writes them (`integrate/verdicts.rs`)
    /// pass the strict parser, and their wire bytes round-trip unchanged.
    #[test]
    fn records_as_the_integrator_writes_them_read_and_round_trip() {
        let records = [
            json!({"type": "task.gated", "id": 7, "verdict": "accepted", "class": "code", "revision": 0,
                   "base": "b", "candidate": "c", "metric": "12", "scenarios": 2, "scenario_names": ["a.sh", "b.sh"]}),
            json!({"type": "task.gated", "id": 7, "verdict": "rejected", "class": "code", "revision": 0,
                   "note": "regression", "log": "/l"}),
            json!({"type": "task.gated", "id": 7, "verdict": "host_fault", "class": "infrastructure",
                   "revision": 0, "evidence": "base_disagrees", "scenario": "011.sh", "exits": [1, 0],
                   "base_disagrees": 1, "alert": false, "escalate": false, "backoff_ms": 16_000,
                   "due_ms": 1_793_930_000_000_i64}),
            json!({"type": "task.gated", "id": 7, "verdict": "host_fault", "class": "infrastructure",
                   "revision": 0, "evidence": "load_sensitive_parked", "scenario": "x.sh",
                   "pushed": "0123abc", "sensitive": ["x.sh"], "parked": true, "escalate": true,
                   "backoff_ms": 0}),
            json!({"type": "task.gated", "id": 7, "verdict": "no_room", "class": "infrastructure",
                   "revision": 0, "check": "disk", "measured": "1", "threshold": "2", "free_kb": 1,
                   "min_gb": 2, "tmpdir": "/data/tmp", "log": "! gate: short", "escalate": true}),
            json!({"type": "task.gated", "id": 7, "verdict": "aborted", "class": "infrastructure",
                   "revision": 0, "reason": "the gate failed: x", "evidence": "parameter_error"}),
            json!({"type": "task.gated", "id": 7, "verdict": "stale", "class": "superseded",
                   "revision": 0, "found": "discarded", "scenario_names": []}),
        ];
        for value in records {
            let parsed = ControlRecord::from_value_strict(&value).expect("strict");
            assert_eq!(
                serde_json::to_value(&parsed).unwrap(),
                value,
                "wire unchanged"
            );
            let ControlRecord::TaskGated(record) = parsed else {
                panic!()
            };
            assert_eq!(record.outcome().unwrap().verdict(), record.verdict);
        }
    }

    /// Replay moves an episode only on a record that reads: a refused one
    /// (no writer made one) is a stage, never a verdict.
    #[test]
    fn replay_moves_nothing_on_a_record_that_does_not_read() {
        let mut stream = gated_mission_stream();
        let index = stream.iter().position(|(seq, _, _)| *seq == 18).unwrap();
        let mut accepted = json!({"type": "task.gated", "id": 20, "verdict": "accepted", "revision": 2,
                                  "base": "b0", "candidate": "c2", "class": "infrastructure"});
        stream[index].2 = ControlRecord::from_value(&accepted).expect("loose parse");
        stream.truncate(index + 1);
        let episodes = replay_task_episodes(&stream);
        assert_eq!(
            episodes[0].state,
            TaskState::Reported,
            "not moved to landing"
        );
        assert_eq!(episodes[0].last_gate_verdict, Some(GateVerdict::Conflict));
        assert_eq!(episodes[0].gate_verdicts.len(), 3);
        assert_eq!(
            episodes[0].stages.last().unwrap().kind,
            ControlRecordKind::TaskGated
        );
        accepted["class"] = json!("code");
        stream[index].2 = ControlRecord::from_value(&accepted).unwrap();
        let episodes = replay_task_episodes(&stream);
        assert_eq!(episodes[0].state, TaskState::Landing);
        assert_eq!(episodes[0].candidate.as_deref(), Some("c2"));
    }
}
