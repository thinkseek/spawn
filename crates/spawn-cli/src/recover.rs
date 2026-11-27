//! The recovery: one rule for every way an execution ends
//! (docs/fleet-architecture.md §3.1, "the recovery"; docs/design.md §14).
//!
//! Spawn's core is one agent that runs forever; the fleet is a client of it.
//! An execution ends in many ways, and none of them may throw the work away:
//! every end, every turn the model failed and every outage of Spawn's own
//! database that a running execution lived through is **one interruption**,
//! recorded once with the facts it was told from, and given one of four ways
//! back:
//!
//! - `resume` (exited, crashed, memory_limit, host_restart): resume the same
//!   native session. The first resume of a run is due at once; the k-th of a
//!   run after `SPAWN_RECOVER_BACKOFF_SECS` (default 10) x 2^(k-1) seconds,
//!   capped at 64 x the base. A resume that dies within a minute before it
//!   makes any progress (no tool call completed, no turn ended) is a failed
//!   one; after [`RESUME_ATTEMPTS`] of them in a row the next interruption
//!   takes the handoff path.
//! - `handoff` (context_exhausted, budget): stop the execution if it still
//!   runs (its Stop's final checkpoint), then start a new native session in
//!   the same Spawn session, workspace and branch, told what is in the
//!   workspace to continue, not to redo.
//! - `wait` (model_unavailable, model_auth, database_unavailable,
//!   disk_full): no revival while the cause holds. A wait clears on a
//!   recorded observation, never on time alone: a model answer to a retried
//!   turn, a changed credentials file, the loop's first pass that reaches the
//!   database, a free-space reading at or above the floor. Clearing is one
//!   `execution.cleared` record and then the revival — an `input` into the
//!   live engine, or a resume if the execution ended. A wait never counts
//!   toward the handoff. A model wait whose engine *ended* is resumed at
//!   most `SPAWN_RECOVER_MODEL_RETRIES` (3) times in a row with no model
//!   answer between ([`model_resumes`]); its next such end is recorded
//!   `model_unavailable` on the `final` path, with `retries` in its
//!   evidence.
//! - `final` (stopped, model_error): never revived. `model_error` is the
//!   engine's own end after a request the model refused for good (a 400
//!   that is not an exhausted context, a 404, a 413), with no progress
//!   since ([`refused`]). The dispatcher tells the supervisor of every
//!   final model record once (`dispatch::digest::model_finals`).
//!
//! The cause is read from recorded facts, in this precedence: stopped,
//! budget, memory_limit, host_restart, disk_full, database_unavailable,
//! model_auth, model_unavailable, context_exhausted, model_error, crashed,
//! exited. Each
//! is named in the record's evidence ([`classify`], [`model_cause`]).
//! A host restart is the terminal server that ran the execution gone: none
//! answers, or the one that answers is another, told by the identity the
//! Bridge recorded when the execution's terminal started
//! (`execution.terminal_started`), not by whether something answers when the
//! end is read: the first resume after a reboot starts a new server, and the
//! ends read after it are the restart's all the same ([`server_gone`]).
//!
//! Every revival is one command keyed `recover-<execution>-<attempt>`, the
//! interruption it answers, submitted no earlier than its `due_ms`; its
//! outcome is read back from the command queue. Because the attempt, the
//! facts and the keys all come from the log, a restarted loop records
//! nothing twice and revives nothing twice ([`Facts`] is read from
//! `spawn_postgres::RECOVERY_FACTS`).
//!
//! One rule for every agent: the supervisor, the residents, and the
//! ephemeral workers that hold a delivered task ([`kept`]). It replaces
//! `supervise.rs`'s `decide` and the dispatcher's one-resume rule, which
//! give way to it where it has recorded an interruption
//! ([`supervise::revive`] and [`crate::dispatch::worker`]).

use std::collections::BTreeMap;
use std::path::Path as FsPath;

use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::{Map, Value, json};

use spawn_core::command::CommandAction;
pub(crate) use spawn_core::control::{InterruptionCause as Cause, RecoveryPath as Path};
use spawn_core::event::{ENGINE, EndCause};
use spawn_core::id::{AgentId, IdempotencyKey, Principal, SessionId};
use spawn_core::port::{CommandQueue, ServerSeen, TerminalHost, TerminalServer};
use spawn_postgres::PostgresStore;

use crate::deliver::{self, Work};
use crate::disk;
use crate::supervise;

/// Failed resumes in a row after which the next interruption hands off
/// instead of resuming (today's `supervise::RESUME_ATTEMPTS`).
pub(crate) const RESUME_ATTEMPTS: u32 = 3;

/// A resume whose execution ends within this of its start, before it made
/// progress, failed.
pub(crate) const YOUNG_MS: i64 = 60_000;

/// Most times a resume's backoff doubles (64 x the base: 640 s by default).
const RESUME_CAP: u32 = 64;

/// Most times a model retry's backoff doubles (4 x the base: 4 minutes by
/// default: a failed retry costs one refused request, and the cap bounds the
/// time to resume once the API is back).
const MODEL_CAP: u32 = 4;

/// Principal the recovery submits as (Spawn's own loop: its inputs do not
/// wait for a turn, they retry on the loop's schedule).
const RECOVERER: &str = "spawn-liveness";

/// Prefix of every command the recovery submits: its own stops and retries
/// are told from anyone else's by it, and a stop that is part of a revival
/// is no interruption of its own.
const KEY_PREFIX: &str = "recover-";

/// The recovery's settings (`SPAWN_RECOVER_*`), read once by the loop.
/// Every duration here is in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Config {
    /// Base of the resume backoff: `SPAWN_RECOVER_BACKOFF_SECS`, default 10.
    pub(crate) resume_backoff: i64,
    /// Base of the model retry backoff: `SPAWN_RECOVER_MODEL_BACKOFF_SECS`,
    /// default 60.
    pub(crate) model_backoff: i64,
    /// How long a login may stay unchanged before one retry is made anyway:
    /// `SPAWN_RECOVER_AUTH_RETRY_SECS`, default 1800.
    pub(crate) auth_retry: i64,
    /// How often an open wait re-reads its cause (the credentials file, the
    /// free space): `SPAWN_RECOVER_PROBE_SECS`, default 10.
    pub(crate) probe: i64,
    /// Resumes in a row of an execution that ended on a transient model
    /// error, with no model answer in between, before the next such end is
    /// final and told to the supervisor: `SPAWN_RECOVER_MODEL_RETRIES`,
    /// default 3, from 1 to [`MODEL_RETRIES_MAX`].
    pub(crate) model_retries: u32,
}

/// The most `SPAWN_RECOVER_MODEL_RETRIES` may be.
pub(crate) const MODEL_RETRIES_MAX: u32 = 20;

impl Default for Config {
    fn default() -> Self {
        Self {
            resume_backoff: 10_000,
            model_backoff: 60_000,
            auth_retry: 1_800_000,
            probe: 10_000,
            model_retries: 3,
        }
    }
}

impl Config {
    /// The settings, as the loop's environment names them. A
    /// `SPAWN_RECOVER_MODEL_RETRIES` that is not a whole number from 1 to
    /// [`MODEL_RETRIES_MAX`] is refused: the loop does not start.
    pub(crate) fn from_env() -> Result<Self, String> {
        let seconds = |name: &str, fallback: i64| {
            crate::config::env_value(name)
                .and_then(|text| text.trim().parse::<i64>().ok())
                .filter(|value| *value > 0)
                .unwrap_or(fallback)
                .saturating_mul(1000)
        };
        Ok(Self {
            resume_backoff: seconds("SPAWN_RECOVER_BACKOFF_SECS", 10),
            model_backoff: seconds("SPAWN_RECOVER_MODEL_BACKOFF_SECS", 60),
            auth_retry: seconds("SPAWN_RECOVER_AUTH_RETRY_SECS", 1800),
            probe: seconds("SPAWN_RECOVER_PROBE_SECS", 10),
            model_retries: model_retries(
                crate::config::env_value("SPAWN_RECOVER_MODEL_RETRIES").as_deref(),
            )?,
        })
    }
}

/// `SPAWN_RECOVER_MODEL_RETRIES` read: unset is the default (3); anything
/// but a whole number from 1 to [`MODEL_RETRIES_MAX`] is refused.
pub(crate) fn model_retries(value: Option<&str>) -> Result<u32, String> {
    let Some(text) = value else {
        return Ok(Config::default().model_retries);
    };
    text.trim()
        .parse::<u32>()
        .ok()
        .filter(|retries| (1..=MODEL_RETRIES_MAX).contains(retries))
        .ok_or_else(|| {
            format!(
                "SPAWN_RECOVER_MODEL_RETRIES must be a whole number from 1 to \
                 {MODEL_RETRIES_MAX}, not {text:?}"
            )
        })
}

/// Wire names and default recovery paths for [`Cause`]
/// (`spawn_host_record`'s interruption cause vocabulary).
pub(crate) trait CauseExt {
    fn name(self) -> &'static str;
    fn path(self) -> Path;
}

impl CauseExt for Cause {
    fn name(self) -> &'static str {
        match self {
            Self::Exited => "exited",
            Self::Crashed => "crashed",
            Self::MemoryLimit => "memory_limit",
            Self::ContextExhausted => "context_exhausted",
            Self::ModelUnavailable => "model_unavailable",
            Self::ModelAuth => "model_auth",
            Self::ModelError => "model_error",
            Self::HostRestart => "host_restart",
            Self::DatabaseUnavailable => "database_unavailable",
            Self::DiskFull => "disk_full",
            Self::Budget => "budget",
            Self::Stopped => "stopped",
            Self::Restarted => "restarted",
            Self::Reassigned => "reassigned",
        }
    }

    /// The way back this cause takes.
    fn path(self) -> Path {
        match self {
            Self::Exited | Self::Crashed | Self::MemoryLimit | Self::HostRestart => Path::Resume,
            Self::ContextExhausted | Self::Budget => Path::Handoff,
            Self::ModelUnavailable
            | Self::ModelAuth
            | Self::DatabaseUnavailable
            | Self::DiskFull => Path::Wait,
            Self::ModelError | Self::Stopped | Self::Restarted | Self::Reassigned => Path::Final,
        }
    }
}

/// Wire names for [`Path`] (`spawn_host_record`'s recovery path vocabulary).
pub(crate) trait PathExt {
    fn name(self) -> &'static str;
}

impl PathExt for Path {
    fn name(self) -> &'static str {
        match self {
            Self::Resume => "resume",
            Self::Handoff => "handoff",
            Self::Wait => "wait",
            Self::Final => "final",
        }
    }
}

// --- the classification, a pure function of the facts ------------------------

/// One failed turn, as the transcript's API error line records it
/// (`turn.failed`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct Turn {
    pub(crate) event_id: i64,
    #[serde(default)]
    pub(crate) at_ms: i64,
    #[serde(default)]
    pub(crate) status: Option<i64>,
    /// The engine's `error` tag: `overloaded`, `rate_limit`, ... (`kind` in
    /// the facts, `error_kind` in the event).
    #[serde(default, rename = "kind")]
    pub(crate) error_kind: Option<String>,
    /// The text the engine showed.
    #[serde(default, rename = "text")]
    pub(crate) text: String,
}

impl Turn {
    /// The model cause this line names, if any: Claude Code writes the API
    /// error line and waits at its prompt, so a failed turn is an
    /// interruption of the execution, never its end.
    ///
    /// The engine's own tags (`status`, `error`) decide when it wrote them;
    /// an engine that wrote neither (Claude Code before 2.1) is read from
    /// its text. The order is the contract's: an expired login, then a
    /// model that is not answering, then an exhausted context.
    pub(crate) fn cause(&self) -> Option<Cause> {
        let text = self.text.to_lowercase();
        let kind = self.error_kind.as_deref().unwrap_or_default();
        if matches!(self.status, Some(401 | 403))
            || kind == "authentication_failed"
            || text.contains("/login")
        {
            return Some(Cause::ModelAuth);
        }
        // `Prompt is too long`: the context window is spent, and no retry
        // of the same native session can help.
        if text.contains("prompt is too long") {
            return Some(Cause::ContextExhausted);
        }
        if matches!(self.status, Some(529 | 503 | 500 | 429))
            || matches!(kind, "overloaded" | "rate_limit" | "server_error")
            // A connection error has no status (`Connection lost
            // mid-response`, `API Error: Connection error`).
            || self.status.is_none()
        {
            return Some(Cause::ModelUnavailable);
        }
        None
    }

    /// Whether the model refused the request in a way no retry changes:
    /// neither transient, nor a login, nor an exhausted context (a 400 of
    /// another kind, a 404, a 413). The engine may wait at its prompt after
    /// it (no interruption: the agent can go on another way); an engine that
    /// ended after it, with no progress since, is a `model_error` end.
    pub(crate) fn model_error(&self) -> bool {
        self.cause().is_none()
    }

    /// The evidence of an interruption told from this turn: its event,
    /// status, error tag and text.
    pub(crate) fn evidence(&self) -> Map<String, Value> {
        let mut evidence = Map::new();
        evidence.insert("event_id".to_string(), json!(self.event_id));
        evidence.insert(
            "status".to_string(),
            self.status.map_or(Value::Null, |status| json!(status)),
        );
        evidence.insert(
            "error".to_string(),
            self.error_kind.clone().map_or(Value::Null, Value::String),
        );
        evidence.insert(
            "text".to_string(),
            json!(crate::deliver::one_line(&self.text, 600)),
        );
        evidence
    }
}

/// A tool call that failed, with the engine's error text (`tool.failed`).
/// A full disk ends a turn with one — Claude Code's ENOSPC message — and
/// leaves the engine at its prompt; no other record of the execution says
/// so, so the recovery reads it here.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct ToolFailure {
    pub(crate) event_id: i64,
    #[serde(default)]
    pub(crate) at_ms: i64,
    /// The engine's error text, as the transcript showed it.
    #[serde(default)]
    pub(crate) text: String,
}

impl ToolFailure {
    /// The cause this failure names, if any: a full disk is the one tool
    /// failure that interrupts an execution (any other failed tool is the
    /// work's, and the engine goes on).
    pub(crate) fn cause(&self) -> Option<Cause> {
        let text = self.text.to_lowercase();
        (text.contains("enospc") || text.contains("disk is full") || text.contains("no space left"))
            .then_some(Cause::DiskFull)
    }
}

/// Cause read from a persisted `execution.ended` event: the Bridge's three
/// current [`EndCause`] variants plus the historical `memory_limit` variant
/// emitted on 2026-11-08 (`4492e363..47717497`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecordedEndCause {
    Exited,
    Stopped,
    Lost,
    MemoryLimit,
}

impl From<EndCause> for RecordedEndCause {
    fn from(cause: EndCause) -> Self {
        match cause {
            EndCause::Exited => Self::Exited,
            EndCause::Stopped => Self::Stopped,
            EndCause::Lost => Self::Lost,
        }
    }
}

/// The engine's end, as `execution.ended` recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct End {
    pub(crate) event_id: i64,
    pub(crate) ended_at_ms: i64,
    /// `exited`, `stopped` or `lost` (the Bridge's three), plus legacy
    /// `memory_limit` written on 2026-11-08.
    pub(crate) cause: RecordedEndCause,
    pub(crate) exit_code: Option<i32>,
    pub(crate) signal: Option<i32>,
    pub(crate) evidence: Option<String>,
}

/// Lifecycle state of an execution from `RECOVERY_FACTS`: either live
/// (`ended_at_ms` and all `execution.ended` fields absent) or ended with
/// its full `execution.ended` record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExecutionState {
    Running,
    Ended(End),
}

/// The stop-like command action returned by `RECOVERY_FACTS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StopAction {
    Stop,
    Discard,
}

/// A Stop or Discard into the session during the execution.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct Stop {
    pub(crate) command_id: i64,
    #[serde(default)]
    pub(crate) principal: String,
    #[serde(default)]
    pub(crate) key: String,
    pub(crate) action: StopAction,
    #[serde(default)]
    pub(crate) created_at_ms: i64,
    #[serde(default)]
    pub(crate) succeeded: Option<bool>,
    #[serde(default)]
    pub(crate) error: Option<String>,
}

impl Stop {
    /// The recovery's own: a handoff's Stop, or a retry's. It is part of a
    /// revival, never an interruption of its own.
    pub(crate) fn ours(&self) -> bool {
        self.key.starts_with(KEY_PREFIX)
    }

    /// Whether the dispatcher sent it to stop a cancelled task's turn
    /// (`cancel-stop-<task>-<execution>`, GitHub #76). It interrupts a turn
    /// so the notice can be typed; it does not end the session, so the
    /// recovery must not record it as `final`.
    pub(crate) fn cancelled_task(&self) -> bool {
        self.principal == crate::dispatch::DISPATCHER && self.key.starts_with("cancel-stop-")
    }

    /// Whether it may have stopped the engine: one refused before typing
    /// stopped nothing.
    pub(crate) fn reached(&self) -> bool {
        self.succeeded != Some(false)
            || self
                .error
                .as_deref()
                .is_some_and(|error| error.contains(spawn_core::bridge::MAY_HAVE_TYPED))
    }
}

/// The facts a cause is told from, gathered for one fact (a turn, an end or
/// an outage) by the loop. Everything but the host's own observations (the
/// terminal server, the disk, the credentials) is recorded.
#[derive(Debug, Clone, Default)]
pub(crate) struct Facts<'a> {
    /// The end of the execution, when it ended.
    pub(crate) end: Option<&'a End>,
    /// A Stop or Discard from a person, the supervisor, a cancelled task, or
    /// the dispatcher's restart/reassign (never the recovery's own),
    /// paired with its attribution.
    pub(crate) stop: Option<StopFact<'a>>,
    /// The integrator's over-budget record of the task
    /// (`{"event_id", "record": {"budget", ...}}`).
    pub(crate) over_budget: Option<&'a Value>,
    /// The execution's resource envelope killed it (objective 029's facts).
    pub(crate) memory: Option<&'a Value>,
    /// The terminal server that ran the execution is gone ([`server_gone`]):
    /// a host restart took the execution with it.
    pub(crate) terminal_server_gone: bool,
    /// A free-space reading of the workspace's filesystem under the floor,
    /// taken at the end.
    pub(crate) disk: Option<&'a disk::Reading>,
    /// The database outage the execution lived through.
    pub(crate) outage: Option<&'a Outage>,
    /// The failed turn the engine ended after, with no progress since, when
    /// the model refused it for good ([`Turn::model_error`]).
    pub(crate) model_error: Option<&'a Turn>,
    /// Whether the session is a retired resident: how a `stopped` end came
    /// about, in its evidence.
    pub(crate) retired: bool,
}

impl Facts<'_> {
    pub(crate) fn task_cancelled(&self) -> bool {
        matches!(
            self.stop,
            Some(StopFact {
                kind: StopKind::CancelledTask,
                ..
            })
        )
    }
}

/// How the dispatcher attributed the reaching external Stop that ended an
/// execution, when the dispatcher sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StopKind {
    /// A person's, the supervisor's, or an unattributed Stop/Discard.
    Other,
    /// The dispatcher's cancel-stop (`cancel-stop-<task>-<execution>`).
    CancelledTask,
    /// `restart-<task>-<n>-stop`, of the task's latest `task.restarted`
    /// (`n`).
    Restarted { task: i64, n: i64 },
    /// `reassign-<task>-stop`, of the task whose `task.cancelled` names
    /// `reassigned_as` (`to_task`), whose worker runs in `to_session`.
    Reassigned {
        task: i64,
        to_task: i64,
        to_session: Option<String>,
    },
}

/// The reaching external Stop (never the recovery's own) paired with its
/// dispatcher attribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StopFact<'a> {
    pub(crate) stop: &'a Stop,
    pub(crate) kind: StopKind,
}

/// The evidence of a `restarted` or `reassigned` end: the Stop that ended
/// it (its command, principal, action and key) and what moved the work
/// (the task and its `n`; the task, the new one and its worker's session,
/// `to_session`).
fn moved_evidence(stop_fact: &StopFact<'_>, evidence: &mut Map<String, Value>) {
    let stop = stop_fact.stop;
    evidence.insert("command_id".to_string(), json!(stop.command_id));
    evidence.insert("principal".to_string(), json!(stop.principal));
    evidence.insert("command".to_string(), json!(stop.action));
    evidence.insert("key".to_string(), json!(stop.key));
    match &stop_fact.kind {
        StopKind::Restarted { task, n } => {
            evidence.insert("task".to_string(), json!(task));
            evidence.insert("n".to_string(), json!(n));
        }
        StopKind::Reassigned {
            task,
            to_task,
            to_session,
        } => {
            evidence.insert("task".to_string(), json!(task));
            evidence.insert("to_task".to_string(), json!(to_task));
            evidence.insert("to_session".to_string(), json!(to_session));
        }
        StopKind::Other | StopKind::CancelledTask => {}
    }
}

/// The dispatcher's original Stop when `last` (the latest Stop that may
/// have stopped the execution, excluding the recovery's own) was sent by
/// the dispatcher; a retry of the dispatcher's Stop
/// (`stop-<execution>-retry-<n>`) stands for the first non-retry Stop it
/// retries. `None` when `last` is anyone else's.
fn dispatchers_stop<'a>(stops: &'a [Stop], last: &'a Stop) -> Option<&'a Stop> {
    let dispatcher = crate::dispatch::DISPATCHER;
    if last.principal != dispatcher {
        return None;
    }
    if last.key.starts_with("stop-") {
        stops
            .iter()
            .find(|stop| stop.principal == dispatcher && !stop.key.starts_with("stop-"))
    } else {
        Some(last)
    }
}

fn moved_from_stop(stop: &Stop, session: &str, work: &Work) -> Option<StopKind> {
    if let Some((task, n)) = stop
        .key
        .strip_prefix("restart-")
        .and_then(|rest| rest.strip_suffix("-stop"))
        .and_then(|rest| rest.split_once('-'))
    {
        let (task, n) = (task.parse::<i64>().ok()?, n.parse::<i64>().ok()?);
        return work
            .tasks
            .iter()
            .any(|row| {
                row.id == task
                    && row.session_id == session
                    && row.restart.as_ref().is_some_and(|restart| restart.n == n)
            })
            .then_some(StopKind::Restarted { task, n });
    }
    let task = stop
        .key
        .strip_prefix("reassign-")
        .and_then(|rest| rest.strip_suffix("-stop"))?
        .parse::<i64>()
        .ok()?;
    let from_rows = work.tasks.iter().find_map(|row| {
        (row.id == task && row.session_id == session)
            .then(|| {
                row.reassigned_as
                    .zip(Some(row.reassigned_to_session.clone()))
            })
            .flatten()
    });
    let from_cancelled = || {
        work.cancelled.iter().find_map(|row| {
            (row.id == task && row.session_id == session)
                .then(|| {
                    row.reassigned_as
                        .zip(Some(row.reassigned_to_session.clone()))
                })
                .flatten()
        })
    };
    let (to_task, to_session) = from_rows.or_else(from_cancelled)?;
    Some(StopKind::Reassigned {
        task,
        to_task,
        to_session,
    })
}

/// The latest Stop or Discard that may have stopped an execution of
/// `session` (never the recovery's own), paired with its dispatcher
/// attribution (`cancel-stop-`, `restart-`, or `reassign-`) backed by the
/// mission rows when the dispatcher sent it.
pub(crate) fn stop_fact<'a>(stops: &'a [Stop], session: &str, work: &Work) -> Option<StopFact<'a>> {
    let last = stops
        .iter()
        .rev()
        .find(|stop| !stop.ours() && stop.reached())?;
    let kind = if let Some(origin) = dispatchers_stop(stops, last) {
        if origin.cancelled_task() {
            StopKind::CancelledTask
        } else {
            moved_from_stop(origin, session, work).unwrap_or(StopKind::Other)
        }
    } else {
        StopKind::Other
    };
    Some(StopFact { stop: last, kind })
}

/// Whether the terminal server that ran an execution is gone, `now` being
/// the server that answers (or none). A server is told as a process is, by
/// when it started, never by whether something answers: after a host
/// restart the first resume starts a new server, and an end processed after
/// it must read as the restart all the same, whatever the order of the ends.
///
/// - No server answers: gone.
/// - The execution's server was recorded when its terminal started
///   (`ran_on`): gone when the one that answers is another. Never the
///   execution's record time: its terminal's start may be what started the
///   server, a whole launch after the record.
/// - None was recorded (an execution started before it was kept, or whose
///   record failed): gone when the answering server started after the
///   execution's record (`recorded_at_ms`): that server is not the one it
///   ran on, which started before it (a host restart's survivors started
///   before the boot).
/// - A host that cannot tell which server answers: not gone (today's
///   rule), so an end reads as a crash, never as a restart it cannot show.
pub(crate) fn server_gone(
    ran_on: Option<&TerminalServer>,
    recorded_at_ms: i64,
    now: ServerSeen,
) -> bool {
    match now {
        ServerSeen::Gone => true,
        ServerSeen::Answers(None) => false,
        ServerSeen::Answers(Some(answers)) => match ran_on {
            Some(ran) => *ran != answers,
            None => answers.started_at_ms > recorded_at_ms,
        },
    }
}

/// The cause of an execution's end, its facts given. The first that holds
/// wins, in the contract's order: a person's Stop before the budget, a
/// memory kill before a host restart, a full disk before a database outage,
/// a signal before a clean exit.
pub(crate) fn classify(facts: &Facts<'_>) -> Cause {
    if let Some(stop) = &facts.stop {
        return match stop.kind {
            StopKind::Restarted { .. } => Cause::Restarted,
            StopKind::Reassigned { .. } => Cause::Reassigned,
            StopKind::Other | StopKind::CancelledTask => Cause::Stopped,
        };
    }
    if facts
        .end
        .is_some_and(|end| end.cause == RecordedEndCause::Stopped)
    {
        return Cause::Stopped;
    }
    if facts.over_budget.is_some() {
        return Cause::Budget;
    }
    if facts.memory.is_some() {
        return Cause::MemoryLimit;
    }
    if facts.terminal_server_gone {
        return Cause::HostRestart;
    }
    if facts.disk.is_some() {
        return Cause::DiskFull;
    }
    if facts.outage.is_some() {
        return Cause::DatabaseUnavailable;
    }
    // The engine's own end after a request the model will never accept:
    // resuming it sends the same request again.
    if facts.model_error.is_some()
        && facts
            .end
            .is_none_or(|end| end.cause != RecordedEndCause::Lost)
    {
        return Cause::ModelError;
    }
    // A signal, a non-zero code, or a terminal gone while its server lived:
    // the engine did not exit by itself.
    if facts.end.is_some_and(|end| {
        end.exit_code.is_some_and(|code| code != 0)
            || end.signal.is_some()
            || end.cause == RecordedEndCause::Lost
    }) {
        return Cause::Crashed;
    }
    Cause::Exited
}

/// The `retry_at_ms` a 429's text names, when it names one: Relay-style
/// quota messages end with "… available again at 22:00 UTC", and waiting
/// past that is what the owner asked for. `at_ms` is the record's own time
/// (the day the clock is on); a time already past is the next day's.
pub(crate) fn retry_at(text: &str, at_ms: i64) -> Option<i64> {
    let marker = "available again at ";
    let lower = text.to_lowercase();
    let start = lower.find(marker)? + marker.len();
    let rest = &lower[start..];
    let time = rest.split_whitespace().next()?;
    let (hour, minute) = time.split_once(':')?;
    let hour: i64 = hour
        .trim_end_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .ok()?;
    let minute: i64 = minute
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    let day_ms = 86_400_000;
    let midnight = at_ms - at_ms.rem_euclid(day_ms);
    let mut at = midnight + (hour * 60 + minute) * 60_000;
    if at <= at_ms {
        at += day_ms;
    }
    Some(at)
}

// --- the path and its schedule, pure functions of the facts and the time ---

/// When the next resume of a session is due, `failures` consecutive failed
/// resumes behind it (`last_failure_ms`: when the latest of them ended).
/// The first resume of a run is due at once; then the base doubles per
/// failure, at most [`RESUME_CAP`] times it (10, 20, 40, 80 … 640 s by
/// default).
pub(crate) fn resume_due(
    base_ms: i64,
    failures: u32,
    last_failure_ms: Option<i64>,
    now_ms: i64,
) -> i64 {
    if failures == 0 {
        return now_ms;
    }
    let exponent = failures.min(RESUME_CAP);
    let pause = base_ms
        .saturating_mul(1_i64 << exponent.min(20))
        .min(base_ms.saturating_mul(i64::from(RESUME_CAP)));
    match last_failure_ms {
        Some(failed) => failed.saturating_add(pause),
        None => now_ms,
    }
}

/// Whether a resume failed: its execution ended within [`YOUNG_MS`] of its
/// start before it made any progress (no tool call completed and no turn
/// ended after it started). A revival that made progress succeeded,
/// whatever ends it later.
pub(crate) fn resume_failed(ended_after_start_ms: Option<i64>, made_progress: bool) -> bool {
    ended_after_start_ms.is_some_and(|took| took < YOUNG_MS) && !made_progress
}

/// When the `j`-th failed turn in a row (`j` from 1) is retried: the base
/// doubling per failure up to [`MODEL_CAP`] times it, or `retry_at_ms` when
/// the text named a later time an account is available again.
pub(crate) fn model_due(base_ms: i64, j: u32, at_ms: i64, retry_at_ms: Option<i64>) -> i64 {
    let step = 1_u32 << (j.saturating_sub(1)).min(MODEL_CAP);
    let pause = base_ms.saturating_mul(i64::from(step.min(MODEL_CAP)));
    let due = at_ms.saturating_add(pause);
    retry_at_ms.map_or(due, |named| named.max(due))
}

/// How many resumes in a row the execution `current` stands at, each of an
/// execution that ended on a transient model error with no model answer in
/// between: from `current` back, every execution a revival started
/// ([`KEY_PREFIX`]) that made no progress, whose predecessor's latest
/// interruption was `model_unavailable`. A revived execution that made
/// progress, or one a person or the supervisor started, ends the run.
pub(crate) fn model_resumes(executions: &[Executed], current: &str) -> u32 {
    let Some(mut at) = executions
        .iter()
        .position(|execution| execution.id == current)
    else {
        return 0;
    };
    let mut resumes = 0_u32;
    while at > 0 {
        let execution = &executions[at];
        let before = &executions[at - 1];
        if !execution.key.starts_with(KEY_PREFIX)
            || execution.progress
            || before.last_cause != Some(Cause::ModelUnavailable)
        {
            break;
        }
        resumes = resumes.saturating_add(1);
        at -= 1;
    }
    resumes
}

/// Whether an open login wait may retry: only when the credentials file
/// still holds the modification time and size it had at the interruption
/// (a changed file is the clear, not a retry), and only every
/// [`Config::auth_retry`].
pub(crate) fn auth_retry_due(config: &Config, at_ms: i64, last_attempt_ms: Option<i64>) -> i64 {
    match last_attempt_ms {
        Some(tried) => tried.saturating_add(config.auth_retry),
        None => at_ms.saturating_add(config.auth_retry),
    }
}

/// The engines' credentials file, as the loop's own environment names it.
pub(crate) fn credentials_path() -> Option<String> {
    resolve_credentials_path(crate::config::env_value)
}

/// [`credentials_path`] over the environment `lookup` reads, in the order
/// the engines' directory is chosen (`runtime`): `$SPAWN_CLAUDE_CONFIG_DIR`
/// (absolute, or `inherit`/`host` for `~/.claude`), else ambient
/// `$CLAUDE_CONFIG_DIR`, else the default managed isolated directory
/// (`$SPAWN_CLAUDE_DATA_DIR/config`, `$XDG_STATE_HOME/spawn/claude-config`,
/// or `$HOME/.local/state/spawn/claude-config`); `.credentials.json` in it.
fn resolve_credentials_path(lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
    let nonempty = |name: &str| lookup(name).filter(|value| !value.is_empty());
    match crate::runtime::claude_config_dir(nonempty) {
        Ok(Some(dir)) => Some(format!("{dir}/.credentials.json")),
        Ok(None) | Err(_) => {
            nonempty("HOME").map(|home| format!("{home}/.claude/.credentials.json"))
        }
    }
}

/// What the credentials file looked like: its path, modification time and
/// size. A wait for an expired login clears when this changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Credentials {
    pub(crate) path: String,
    pub(crate) mtime_ms: i64,
    pub(crate) size: u64,
}

impl Credentials {
    /// Read the file now; `None` when it cannot be read (a missing file is
    /// no observation, and the wait goes on).
    pub(crate) fn read(path: &str) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        let mtime_ms = meta
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_millis()
            .try_into()
            .ok()?;
        Some(Self {
            path: path.to_string(),
            mtime_ms,
            size: meta.len(),
        })
    }

    pub(crate) fn json(&self) -> Value {
        json!({"path": self.path, "mtime_ms": self.mtime_ms, "size": self.size})
    }
}

/// A database outage the loop lived through: its first failed pass, the
/// first good one, and how many passes failed in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Outage {
    pub(crate) from_ms: i64,
    pub(crate) to_ms: i64,
    pub(crate) failed_passes: u32,
    pub(crate) error: String,
}

impl Outage {
    /// The evidence of the wait this outage opens: its window, its passes,
    /// one line of the error.
    pub(crate) fn evidence(&self) -> Map<String, Value> {
        let mut evidence = Map::new();
        evidence.insert("from_ms".to_string(), json!(self.from_ms));
        evidence.insert("to_ms".to_string(), json!(self.to_ms));
        evidence.insert("failed_passes".to_string(), json!(self.failed_passes));
        evidence.insert(
            "error".to_string(),
            json!(crate::deliver::one_line(&self.error, 300)),
        );
        evidence
    }
}

/// The key of an interruption's revival: `recover-<execution>-<attempt>`.
/// The attempt names the interruption, so the key names this one and no
/// other, whatever an earlier loop recorded and whether or not it revived it.
pub(crate) fn key(execution: &str, attempt: u32) -> String {
    format!("{KEY_PREFIX}{execution}-{attempt}")
}

/// The way back from an end of `cause`, and what its record's evidence
/// gains: [`way`]'s, except that a cancelled task's Stop (`task_cancelled`,
/// [`Stop::cancelled_task`]) interrupts the turn and takes `resume` where
/// any other Stop is `final` (GitHub #76), under `way`'s bounds: with no
/// native session to resume, or after [`RESUME_ATTEMPTS`] failed resumes in
/// a row, it is a handoff, so a resume the Bridge refuses is not retried
/// without end. `final` left a resident dead for good when the notice's
/// resume did not follow, while the dispatcher went on assigning it work; a
/// `wait` would never clear, since a stop leaves nothing to observe.
pub(crate) fn stopped_path(
    cause: Cause,
    task_cancelled: bool,
    native_bound: bool,
    failed_resumes: u32,
) -> (Path, Map<String, Value>) {
    if cause == Cause::Stopped && task_cancelled {
        bounded(Path::Resume, native_bound, failed_resumes)
    } else {
        way(cause, native_bound, failed_resumes)
    }
}

/// What a handoff's new native session is told of the interruption `last`,
/// after its role or brief; `head` names the branch's head. The handoff
/// after a cancelled task's Stop does not tell it to carry on.
fn handoff_interruption(last: &Interrupted, head: &str) -> String {
    let then = if last.is_cancelled_stop() {
        "Your last turn was stopped because its task was cancelled. Do not continue that task \
         and do not write a report for it. Wait at your prompt: Spawn's notice of the cancel \
         and your next task come next."
    } else {
        "The work is in the workspace to continue, not to redo: read what is there and carry on."
    };
    format!(
        "\n\nYour previous native session could not go on: {} ({}). This is a new native \
         session in the same Spawn session, workspace and branch{head}. {then}",
        last.cause.name(),
        describe(last)
    )
}

/// What the revival after a cancelled task's Stop types or resumes with:
/// the dispatcher's notice names the task; until it comes, nothing is to be
/// continued.
const CANCELLED_REVIVAL: &str = "Spawn resumed your engine: your last turn was stopped because its task was cancelled. Do not continue that task and do not write a report for it. Wait at your prompt: Spawn's notice of the cancel and your next task come next.";

/// The way back from `cause`, and what its record's evidence gains: a resume
/// path becomes a handoff after [`RESUME_ATTEMPTS`] failed resumes in a row,
/// or when there is no native session to resume. `final` is never revived.
pub(crate) fn way(
    cause: Cause,
    native_bound: bool,
    failed_resumes: u32,
) -> (Path, Map<String, Value>) {
    bounded(cause.path(), native_bound, failed_resumes)
}

/// `path`, made a handoff where a resume cannot work ([`way`]'s bounds), and
/// what the record's evidence gains then.
fn bounded(path: Path, native_bound: bool, failed_resumes: u32) -> (Path, Map<String, Value>) {
    let mut evidence = Map::new();
    if path == Path::Resume && failed_resumes >= RESUME_ATTEMPTS {
        evidence.insert("resumes_failed".to_string(), json!(RESUME_ATTEMPTS));
        return (Path::Handoff, evidence);
    }
    if path == Path::Resume && !native_bound {
        evidence.insert("native".to_string(), json!("none"));
        return (Path::Handoff, evidence);
    }
    (path, evidence)
}

/// Whether a free-space reading clears a disk wait: at the floor or above
/// it, as `SPAWN_DISK_FLOOR_GB` names it.
pub(crate) fn disk_cleared(free: u64, floor: u64) -> bool {
    free >= floor
}

/// Whether a pass's failures count toward the loop's `MAX_FAILURES`: one
/// that failed only because the database could not be reached does not, so
/// an outage never ends the loop ([`crate::commands::loops`]).
pub(crate) fn counts_as_failure(errors: &[String]) -> bool {
    errors
        .iter()
        .any(|error| !spawn_core::port::is_unreachable(error))
}

// --- the facts, as the store returns them -----------------------------------

/// The session's latest execution from `RECOVERY_FACTS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Execution {
    pub(crate) id: String,
    pub(crate) started_at_ms: i64,
    pub(crate) workspace: Option<String>,
    /// The terminal server its terminal started on, as the Bridge recorded
    /// it (`execution.terminal_started`): none for an execution started
    /// before it was kept, or whose record failed.
    pub(crate) terminal_server: Option<TerminalServer>,
    pub(crate) state: ExecutionState,
}

impl Execution {
    pub(crate) fn is_live(&self) -> bool {
        matches!(self.state, ExecutionState::Running)
    }

    /// The end, as the classification reads it.
    pub(crate) fn end(&self) -> Option<&End> {
        match &self.state {
            ExecutionState::Running => None,
            ExecutionState::Ended(end) => Some(end),
        }
    }
}

#[derive(Deserialize)]
struct RawExecution {
    id: String,
    #[serde(default)]
    started_at_ms: i64,
    #[serde(default)]
    ended_at_ms: Option<i64>,
    #[serde(default)]
    cause: Option<RecordedEndCause>,
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(default)]
    signal: Option<i32>,
    #[serde(default)]
    evidence: Option<String>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    end_event_id: Option<i64>,
    #[serde(default)]
    terminal_server: Option<TerminalServer>,
}

impl<'de> Deserialize<'de> for Execution {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawExecution::deserialize(deserializer)?;
        let state = match raw.ended_at_ms {
            None => {
                if raw.cause.is_some()
                    || raw.exit_code.is_some()
                    || raw.signal.is_some()
                    || raw.evidence.is_some()
                    || raw.end_event_id.is_some()
                {
                    return Err(de::Error::custom(
                        "running execution cannot carry execution.ended fields",
                    ));
                }
                ExecutionState::Running
            }
            Some(ended_at_ms) => {
                let cause = raw.cause.ok_or_else(|| {
                    de::Error::custom("ended execution is missing execution.ended cause")
                })?;
                let event_id = raw.end_event_id.filter(|id| *id > 0).ok_or_else(|| {
                    de::Error::custom("ended execution requires positive end_event_id")
                })?;
                ExecutionState::Ended(End {
                    event_id,
                    ended_at_ms,
                    cause,
                    exit_code: raw.exit_code,
                    signal: raw.signal,
                    evidence: raw.evidence,
                })
            }
        };
        Ok(Self {
            id: raw.id,
            started_at_ms: raw.started_at_ms,
            workspace: raw.workspace,
            terminal_server: raw.terminal_server,
            state,
        })
    }
}

/// Coupled `(path, due_ms)` contract enforced by `spawn_host_record`:
/// `final` never carries a numeric `due_ms`; every revival path (`resume`,
/// `handoff`, `wait`) requires a `due_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoverySchedule {
    Resume { due_ms: i64 },
    Handoff { due_ms: i64 },
    Wait { due_ms: i64 },
    Final,
}

impl RecoverySchedule {
    pub(crate) fn path(self) -> Path {
        match self {
            Self::Resume { .. } => Path::Resume,
            Self::Handoff { .. } => Path::Handoff,
            Self::Wait { .. } => Path::Wait,
            Self::Final => Path::Final,
        }
    }

    pub(crate) fn due_ms(self) -> Option<i64> {
        match self {
            Self::Resume { due_ms } | Self::Handoff { due_ms } | Self::Wait { due_ms } => {
                Some(due_ms)
            }
            Self::Final => None,
        }
    }
}

/// Schedule parsed from a persisted `execution.interrupted` record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordedSchedule {
    Explicit(RecoverySchedule),
    /// Historical record that omitted `path`, or a historical 2026-11-08
    /// non-`final` record (`4492e363..47717497`) written before
    /// `spawn_host_record` required numeric `due_ms`.
    Legacy {
        path: Option<Path>,
        due_ms: Option<i64>,
    },
}

/// A parsed `execution.interrupted` record from the control stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Interrupted {
    pub(crate) event_id: i64,
    pub(crate) at_ms: i64,
    pub(crate) cause: Cause,
    pub(crate) schedule: RecordedSchedule,
    pub(crate) attempt: u32,
    pub(crate) fact_event_id: i64,
    pub(crate) evidence: Value,
}

impl Interrupted {
    pub(crate) fn explicit_path(&self) -> Option<Path> {
        match self.schedule {
            RecordedSchedule::Explicit(schedule) => Some(schedule.path()),
            RecordedSchedule::Legacy { path, .. } => path,
        }
    }

    /// The way back recorded for this interruption, or its cause's own path
    /// when a legacy record omitted `path` (GitHub #76).
    pub(crate) fn effective_path(&self) -> Path {
        match self.schedule {
            RecordedSchedule::Explicit(schedule) => schedule.path(),
            RecordedSchedule::Legacy { path, .. } => path.unwrap_or_else(|| self.cause.path()),
        }
    }

    pub(crate) fn due_ms(&self) -> Option<i64> {
        match self.schedule {
            RecordedSchedule::Explicit(schedule) => schedule.due_ms(),
            RecordedSchedule::Legacy { due_ms, .. } => due_ms,
        }
    }

    /// Whether this interruption is a cancelled task's Stop (`task_cancelled`
    /// in evidence): its revival must not tell the worker to continue.
    pub(crate) fn is_cancelled_stop(&self) -> bool {
        self.cause == Cause::Stopped && self.evidence["task_cancelled"].as_bool() == Some(true)
    }
}

/// A record the recovery wrote about the execution (`execution.interrupted`
/// or `execution.cleared`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Recorded {
    Interrupted(Interrupted),
    Cleared { event_id: i64, at_ms: i64 },
}

#[derive(Deserialize)]
struct RawRecorded {
    event_id: i64,
    #[serde(default)]
    at_ms: i64,
    record: Value,
}

#[derive(Deserialize)]
struct RawInterruptedPayload {
    cause: Cause,
    #[serde(default)]
    path: Option<Path>,
    attempt: u32,
    #[serde(default)]
    due_ms: Option<i64>,
    #[serde(default)]
    fact_event_id: i64,
    #[serde(default)]
    evidence: Value,
}

impl<'de> Deserialize<'de> for Recorded {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawRecorded::deserialize(deserializer)?;
        let Some(record_type) = raw.record.get("type").and_then(Value::as_str) else {
            return Err(de::Error::custom(
                "recovery record is missing string field `type`",
            ));
        };
        match record_type {
            "execution.cleared" => Ok(Self::Cleared {
                event_id: raw.event_id,
                at_ms: raw.at_ms,
            }),
            "execution.interrupted" => {
                let payload: RawInterruptedPayload =
                    serde_json::from_value(raw.record).map_err(de::Error::custom)?;
                if payload.attempt < 1 {
                    return Err(de::Error::custom(
                        "execution.interrupted requires attempt >= 1",
                    ));
                }
                let schedule = match (payload.path, payload.due_ms) {
                    (Some(Path::Final), None) => {
                        RecordedSchedule::Explicit(RecoverySchedule::Final)
                    }
                    (Some(Path::Final), Some(_)) => {
                        return Err(de::Error::custom(
                            "final execution.interrupted cannot carry due_ms",
                        ));
                    }
                    (Some(Path::Resume), Some(due_ms)) => {
                        RecordedSchedule::Explicit(RecoverySchedule::Resume { due_ms })
                    }
                    (Some(Path::Handoff), Some(due_ms)) => {
                        RecordedSchedule::Explicit(RecoverySchedule::Handoff { due_ms })
                    }
                    (Some(Path::Wait), Some(due_ms)) => {
                        RecordedSchedule::Explicit(RecoverySchedule::Wait { due_ms })
                    }
                    (Some(path), None) => RecordedSchedule::Legacy {
                        path: Some(path),
                        due_ms: None,
                    },
                    (None, due_ms) => RecordedSchedule::Legacy { path: None, due_ms },
                };
                Ok(Self::Interrupted(Interrupted {
                    event_id: raw.event_id,
                    at_ms: raw.at_ms,
                    cause: payload.cause,
                    schedule,
                    attempt: payload.attempt,
                    fact_event_id: payload.fact_event_id,
                    evidence: payload.evidence,
                }))
            }
            other => Err(de::Error::custom(format!(
                "unexpected recovery record type `{other}`"
            ))),
        }
    }
}

/// Lifecycle outcome of a recovery command in `spawn_commands`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SentOutcome {
    Pending,
    Cancelled { finished_at_ms: i64 },
    Succeeded { finished_at_ms: i64 },
    Failed { finished_at_ms: i64 },
}

impl SentOutcome {
    pub(crate) fn is_failed(self) -> bool {
        matches!(self, Self::Failed { .. })
    }

    pub(crate) fn finished_at_ms(self) -> Option<i64> {
        match self {
            Self::Pending => None,
            Self::Cancelled { finished_at_ms }
            | Self::Succeeded { finished_at_ms }
            | Self::Failed { finished_at_ms } => Some(finished_at_ms),
        }
    }
}

/// A command into the session (the recovery's, or anyone's).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Sent {
    pub(crate) key: String,
    pub(crate) created_at_ms: i64,
    pub(crate) outcome: SentOutcome,
}

#[derive(Deserialize)]
struct RawSent {
    key: String,
    #[serde(default)]
    created_at_ms: i64,
    #[serde(default)]
    finished_at_ms: Option<i64>,
    #[serde(default)]
    succeeded: Option<bool>,
}

impl<'de> Deserialize<'de> for Sent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawSent::deserialize(deserializer)?;
        let outcome = match (raw.finished_at_ms, raw.succeeded) {
            (None, None) => SentOutcome::Pending,
            (None, Some(_)) => {
                return Err(de::Error::custom(
                    "pending command cannot carry a succeeded outcome without finished_at_ms",
                ));
            }
            (Some(finished_at_ms), None) => SentOutcome::Cancelled { finished_at_ms },
            (Some(finished_at_ms), Some(true)) => SentOutcome::Succeeded { finished_at_ms },
            (Some(finished_at_ms), Some(false)) => SentOutcome::Failed { finished_at_ms },
        };
        Ok(Self {
            key: raw.key,
            created_at_ms: raw.created_at_ms,
            outcome,
        })
    }
}

/// One session's facts (`spawn_postgres::RECOVERY_FACTS`).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct FactsRow {
    pub(crate) session_id: String,
    pub(crate) agent_id: String,
    #[serde(default)]
    pub(crate) execution: Option<Execution>,
    #[serde(default)]
    pub(crate) native_session_id: Option<String>,
    #[serde(default)]
    pub(crate) turns: Vec<Turn>,
    /// Every `tool.failed` of the execution, oldest first: a full disk shows
    /// here and nowhere else while the engine waits at its prompt.
    #[serde(default)]
    pub(crate) failed_tools: Vec<ToolFailure>,
    #[serde(default)]
    pub(crate) progress: Progress,
    #[serde(default)]
    pub(crate) stops: Vec<Stop>,
    #[serde(default)]
    pub(crate) records: Vec<Recorded>,
    #[serde(default)]
    pub(crate) commands: Vec<Sent>,
    /// Every execution of the session, oldest first ([`Executed`]): the
    /// resume rule counts the ones a revival started that died young.
    #[serde(default)]
    pub(crate) executions: Vec<Executed>,
    #[serde(default)]
    pub(crate) head: Option<String>,
    #[serde(default)]
    pub(crate) over_budget: Option<Value>,
}

/// A session the recovery looks at (`spawn_postgres::RECOVERY_SESSIONS`).
#[derive(Debug, Clone, Deserialize)]
struct Candidate {
    session_id: String,
    agent_id: String,
}

/// One execution of a session, as the resume rule reads it: when it ran,
/// whether a revival started it, and whether it made any progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Executed {
    pub(crate) id: String,
    pub(crate) started_at_ms: i64,
    pub(crate) ended_at_ms: Option<i64>,
    /// A tool call completed or a turn ended after it started.
    pub(crate) progress: bool,
    /// The key of the command that started it: a revival's when it starts
    /// with [`KEY_PREFIX`].
    pub(crate) key: String,
    /// The cause of the latest interruption the recovery recorded for it,
    /// if any: an execution that ended on a transient model error reads
    /// [`Cause::ModelUnavailable`] ([`model_resumes`]).
    pub(crate) last_cause: Option<Cause>,
}

#[derive(Deserialize)]
struct RawExecuted {
    id: String,
    started_at_ms: i64,
    #[serde(default)]
    ended_at_ms: Option<i64>,
    #[serde(default)]
    duration_ms: Option<i64>,
    #[serde(default)]
    progress: bool,
    #[serde(default)]
    key: String,
    #[serde(default)]
    last_cause: Option<Cause>,
}

impl<'de> Deserialize<'de> for Executed {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawExecuted::deserialize(deserializer)?;
        if raw.ended_at_ms.is_none() && raw.duration_ms.is_some() {
            return Err(de::Error::custom(
                "running executed entry cannot carry duration_ms",
            ));
        }
        Ok(Self {
            id: raw.id,
            started_at_ms: raw.started_at_ms,
            ended_at_ms: raw.ended_at_ms,
            progress: raw.progress,
            key: raw.key,
            last_cause: raw.last_cause,
        })
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct Progress {
    #[serde(default)]
    pub(crate) event_id: i64,
    #[serde(default)]
    pub(crate) at_ms: i64,
}

// --- the loop's state -------------------------------------------------------

/// Whether this loop process has recorded its `host.loop_started` record,
/// together with the earlier loop's `boot_id` read right before writing it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
enum LoopStart {
    #[default]
    Pending,
    Recorded {
        earlier_boot_id: Option<String>,
    },
}

/// What the loop knows between passes: the outage it is in, the loop before
/// it, and when an open wait last re-read its cause. Everything the
/// decisions rest on is in the log; this is only what no record holds yet.
#[derive(Debug, Default)]
pub(crate) struct Recovery {
    /// Whether `host.loop_started` has been written, paired with the
    /// earlier loop's `boot_id`.
    start: LoopStart,
    /// The outage in progress, or the one since this pass reached the
    /// database.
    outage: Option<Outage>,
    /// When an open wait last re-read its cause, by session and attempt.
    probes: BTreeMap<String, i64>,
    /// Whether this pass left a wait open: its cause (a model that refuses,
    /// a login that expires, a full disk) is cleared by an observation no
    /// append announces, so the loop must run the step again every probe,
    /// not only when the log moves.
    open_waits: bool,
    /// The earliest time a revival this pass left pending is due (a resume
    /// or a handoff behind its backoff): the step must run then, whether
    /// the log moves or not.
    pending_due: Option<i64>,
}

impl Recovery {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn earlier_boot_id(&self) -> Option<&str> {
        match &self.start {
            LoopStart::Pending => None,
            LoopStart::Recorded { earlier_boot_id } => earlier_boot_id.as_deref(),
        }
    }

    /// A pass that failed, `error` being one line of why: an outage while
    /// the database is unreachable, counted pass by pass.
    pub(crate) fn failed(&mut self, now_ms: i64, error: &str) {
        match &mut self.outage {
            Some(outage) => {
                outage.failed_passes = outage.failed_passes.saturating_add(1);
            }
            None => {
                self.outage = Some(Outage {
                    from_ms: now_ms,
                    to_ms: now_ms,
                    failed_passes: 1,
                    error: error.to_string(),
                });
            }
        }
    }

    /// Whether the failed passes since the last good one made an outage the
    /// recovery must record: the first pass that reaches the database runs
    /// it at once, whether the log moved or not.
    pub(crate) fn outage_pending(&self) -> bool {
        self.outage.is_some()
    }

    /// The earliest time the loop must run the step again with no log
    /// move: a wait's probe, a pending revival's due, or now while an
    /// outage is pending.
    pub(crate) fn next_due(&self, probe_ms: i64, now_ms: i64) -> Option<i64> {
        let probe = self.open_waits.then(|| now_ms.saturating_add(probe_ms));
        let outage = self.outage.is_some().then_some(now_ms);
        [probe, self.pending_due, outage]
            .into_iter()
            .flatten()
            .min()
    }

    /// A revival is due at `due`, later than now: the step runs then.
    fn hold_until(&mut self, due: i64) {
        self.pending_due = Some(self.pending_due.map_or(due, |held| held.min(due)));
    }

    /// The outage this pass clears: the one the last failed passes made, if
    /// any, ending at `now_ms`. `None` when no pass failed or the last pass
    /// did.
    pub(crate) fn clearing(&self, now_ms: i64) -> Option<Outage> {
        self.outage.as_ref().map(|outage| Outage {
            to_ms: now_ms,
            ..outage.clone()
        })
    }
}

// --- the step: record what happened, then take the way back ----------------

/// What one recovery pass reads the world with: the database, the mission
/// rows, the terminal host, the loop's disk watch (under its floor no new
/// execution starts), the workspace root (the filesystem a free-space
/// reading is taken on), the settings, the loop's clock and the engine's
/// program.
pub(crate) struct World<'a> {
    pub(crate) store: &'a PostgresStore,
    pub(crate) work: &'a Work,
    pub(crate) terminals: &'a dyn TerminalHost,
    pub(crate) monitor: &'a disk::Monitor,
    pub(crate) root: Option<&'a FsPath>,
    pub(crate) config: &'a Config,
    pub(crate) now_ms: i64,
    pub(crate) spawn: &'a str,
}

/// One recovery pass: record what happened to every agent the rule keeps,
/// and take each due way back ([`World`]).
///
/// A pass that reaches the database also ends the outage the failed passes
/// before it made: its executions that ran through the window are recorded
/// with `database_unavailable` and cleared at once (the pass that reads this
/// *is* the observation), and one that ended inside it is resumed.
pub(crate) fn step(state: &mut Recovery, world: &World<'_>) -> Result<Vec<String>, String> {
    let World {
        store,
        work,
        terminals,
        monitor,
        root,
        config,
        now_ms,
        spawn,
    } = *world;
    let mut lines = Vec::new();
    state.open_waits = false;
    state.pending_due = None;
    state.record_start(store, now_ms)?;
    let outage = state.clearing(now_ms);
    let sessions: Vec<Candidate> = serde_json::from_value(
        store
            .recovery_sessions()
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("recovery sessions: {error}"))?;
    let residents: BTreeMap<String, String> = store
        .residents_to_keep()
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|(_, session, charter)| (session, charter))
        .collect();
    let retired: Vec<String> = store
        .retired_residents()
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|(_, session)| session)
        .collect();
    let rows = sessions.into_iter().filter_map(
        |Candidate {
             session_id,
             agent_id,
         }| {
            if !kept(&agent_id, &session_id, work, &residents) {
                return None;
            }
            let fetched = SessionId::new(&session_id)
                .map_err(|error| error.to_string())
                .and_then(|id| store.recovery_facts(&id).map_err(|error| error.to_string()));
            Some((session_id, fetched))
        },
    );
    visit_pass_rows(rows, &mut lines, |row| {
        let Some(execution) = row.execution.as_ref() else {
            return Ok(Vec::new());
        };
        let records = row.records.clone();
        let mut one = One {
            state,
            store,
            work,
            terminals,
            monitor,
            root,
            config,
            now_ms,
            spawn,
            row: &row,
            execution,
            records,
            residents: &residents,
            retired: &retired,
            lines: Vec::new(),
        };
        one.run(outage.as_ref())?;
        Ok(one.lines)
    })?;
    // The database answered every session this pass read: the outage is
    // over, its records written.
    if outage.is_some() {
        state.outage = None;
    }
    Ok(lines)
}

fn redact_delimited_after(
    text: &str,
    marker: &str,
    open: char,
    close: char,
    replacement: &str,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find(marker) {
        let prefix_end = pos + marker.len();
        out.push_str(&rest[..prefix_end]);
        let after = &rest[prefix_end..];
        if let Some(after_open) = after.strip_prefix(open) {
            let mut escaped = false;
            let mut end_idx = None;
            for (idx, ch) in after_open.char_indices() {
                if escaped {
                    escaped = false;
                    continue;
                }
                if open == '"' && ch == '\\' {
                    escaped = true;
                    continue;
                }
                if ch == close {
                    end_idx = Some(idx + ch.len_utf8());
                    break;
                }
            }
            if let Some(end) = end_idx {
                out.push_str(replacement);
                rest = &after_open[end..];
                continue;
            }
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Strip literal unexpected field/variant/string values echoed by serde before
/// logging malformed `RECOVERY_FACTS` rows.
fn sanitize_parse_error(raw: &str) -> String {
    let mut cleaned = redact_delimited_after(raw, "unknown variant ", '`', '`', "`<redacted>`");
    cleaned = redact_delimited_after(
        &cleaned,
        "unexpected recovery record type ",
        '`',
        '`',
        "`<redacted>`",
    );
    cleaned = redact_delimited_after(&cleaned, "unknown field ", '`', '`', "`<redacted>`");
    cleaned = redact_delimited_after(&cleaned, "string ", '"', '"', "\"<redacted>\"");
    crate::deliver::one_line(&cleaned, 200)
}

fn parse_session_facts(session: &str, facts: Value) -> Result<FactsRow, String> {
    serde_json::from_value(facts).map_err(|error| {
        format!(
            "recover: skipping {session}: malformed recovery facts ({})",
            sanitize_parse_error(&error.to_string())
        )
    })
}

/// Stream kept session rows through recovery while isolating malformed `FactsRow`
/// payloads to their own session; transport/store errors still fail the pass.
fn visit_pass_rows<I>(
    rows: I,
    lines: &mut Vec<String>,
    mut on_row: impl FnMut(FactsRow) -> Result<Vec<String>, String>,
) -> Result<(), String>
where
    I: IntoIterator<Item = (String, Result<Value, String>)>,
{
    for (session, facts_result) in rows {
        let facts = facts_result?;
        let row = match parse_session_facts(&session, facts) {
            Ok(row) => row,
            Err(skip_line) => {
                lines.push(skip_line);
                continue;
            }
        };
        let more = on_row(row)?;
        lines.extend(more);
    }
    Ok(())
}

impl Recovery {
    /// Record this loop's start once, keeping the loop before it.
    fn record_start(&mut self, store: &PostgresStore, now_ms: i64) -> Result<(), String> {
        if matches!(self.start, LoopStart::Recorded { .. }) {
            return Ok(());
        }
        let earlier_boot_id = store
            .loop_started()
            .map_err(|error| error.to_string())?
            .and_then(|earlier| earlier["boot_id"].as_str().map(str::to_string));
        let record = json!({
            "type": "host.loop_started",
            "boot_id": boot_id(),
            "started_at_ms": now_ms,
        });
        store
            .host_record(&record)
            .map_err(|error| format!("host.loop_started: {error}"))?;
        self.start = LoopStart::Recorded { earlier_boot_id };
        Ok(())
    }
}

/// Whether the rule keeps this agent: the supervisor always, a resident
/// always, an ephemeral worker while it holds a delivered task. An idle
/// worker with no open task is not revived: nothing waits on it.
fn kept(agent: &str, session: &str, work: &Work, residents: &BTreeMap<String, String>) -> bool {
    if session == supervise::SUPERVISOR_SESSION {
        return true;
    }
    if residents.contains_key(session) {
        return true;
    }
    agent.starts_with("agent_worker_")
        && (work.tasks.iter().any(|task| {
            task.session_id == session && task.state == spawn_core::mission::TaskState::Delivered
        }) || reassigned_away(session, work))
}

/// Whether `session` is an ephemeral worker whose task was reassigned
/// (`spawn task reassign`): the recovery records its end (`reassigned`, by
/// the reassign's Stop) and never revives it, whatever earlier record it
/// has: its task went to a fresh worker.
fn reassigned_away(session: &str, work: &Work) -> bool {
    work.tasks.iter().any(|task| {
        task.session_id == session && task.kind == "ephemeral" && task.reassigned_as.is_some()
    })
}

/// The host's boot id (`/proc/sys/kernel/random/boot_id`), or `unknown` on a
/// host that has none.
fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map_or_else(|_| "unknown".to_string(), |id| id.trim().to_string())
}

/// One session's pass.
struct One<'a> {
    state: &'a mut Recovery,
    store: &'a PostgresStore,
    work: &'a Work,
    terminals: &'a dyn TerminalHost,
    monitor: &'a disk::Monitor,
    root: Option<&'a FsPath>,
    config: &'a Config,
    now_ms: i64,
    spawn: &'a str,
    row: &'a FactsRow,
    execution: &'a Execution,
    /// The execution's recovery records as this pass knows them: what was
    /// read, and what this pass recorded.
    records: Vec<Recorded>,
    residents: &'a BTreeMap<String, String>,
    retired: &'a [String],
    lines: Vec<String>,
}

/// The fact an interruption is told from.
enum Fact {
    Turn { turn: Turn, cause: Cause },
    Tool { tool: ToolFailure, cause: Cause },
    End(End),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthRetry {
    Input,
    Resume,
    Handoff,
}

/// The way an expired login's periodic retry reaches the engine. Input works
/// only in a live execution; a stopped one takes the usual resume bounds,
/// handing off when there is no native session or resumes are spent.
fn auth_retry_action(live: bool, native_bound: bool, failed_resumes: u32) -> AuthRetry {
    if live {
        return AuthRetry::Input;
    }
    match bounded(Path::Resume, native_bound, failed_resumes).0 {
        Path::Resume => AuthRetry::Resume,
        _ => AuthRetry::Handoff,
    }
}

/// A recovery command planned by a pass before submission.
#[derive(Debug, Clone)]
pub(crate) struct PlannedCommand {
    pub(crate) key: String,
    pub(crate) action: CommandAction,
    pub(crate) note: &'static str,
}

/// The pure decision of a revival or auth-retry planning pass.
#[derive(Debug, Clone)]
pub(crate) enum RevivalPlan {
    Idle,
    HoldUntil(i64),
    Submit(Box<PlannedCommand>),
}

/// Shared execution context needed to plan a revival or auth retry.
#[derive(Clone, Copy)]
struct RevivalEnv<'a> {
    execution_id: &'a str,
    commands: &'a [Sent],
    config: &'a Config,
    now_ms: i64,
    live: bool,
}

fn resume_text_for(execution_id: &str, last: &Interrupted) -> String {
    if last.is_cancelled_stop() {
        return CANCELLED_REVIVAL.to_string();
    }
    format!(
        "Spawn restarted your engine: execution {execution_id} {} ({}). Continue your task from where \
         you were; what you did is in your workspace.",
        last.cause.name(),
        describe(last)
    )
}

fn is_retry_key(candidate: &str, retry_prefix: &str) -> bool {
    candidate.strip_prefix(retry_prefix).is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn start_handoff_action(prompt: String, charter: Option<String>) -> CommandAction {
    CommandAction::Start {
        engine: ENGINE.to_string(),
        prompt,
        parent_session_id: None,
        from: None,
        model: None,
        charter,
        model_label: None,
    }
}

fn schedule_for_path(path: Path, due_ms: impl FnOnce() -> i64) -> RecoverySchedule {
    match path {
        Path::Resume => RecoverySchedule::Resume { due_ms: due_ms() },
        Path::Handoff => RecoverySchedule::Handoff { due_ms: due_ms() },
        Path::Wait => RecoverySchedule::Wait { due_ms: due_ms() },
        Path::Final => RecoverySchedule::Final,
    }
}

/// Plan the next `<key>-r<n>` retry after a failed revival command, preserving
/// `build_action` so a failed handoff `Start` retries as `Start` rather than `Resume`.
fn plan_retry_action(
    commands: &[Sent],
    config: &Config,
    now_ms: i64,
    key: &str,
    build_action: impl FnOnce() -> CommandAction,
) -> RevivalPlan {
    let retry_prefix = format!("{key}-r");
    let retries: Vec<&Sent> = commands
        .iter()
        .filter(|sent| is_retry_key(&sent.key, &retry_prefix))
        .collect();
    if retries.iter().any(|sent| !sent.outcome.is_failed()) {
        return RevivalPlan::Idle;
    }
    let failures = u32::try_from(retries.len())
        .unwrap_or(u32::MAX)
        .saturating_add(1);
    let failed_at = commands
        .iter()
        .filter(|sent| sent.key == key || is_retry_key(&sent.key, &retry_prefix))
        .filter_map(|sent| sent.outcome.finished_at_ms())
        .max()
        .unwrap_or(now_ms);
    let due = resume_due(config.resume_backoff, failures, Some(failed_at), now_ms);
    if now_ms < due {
        return RevivalPlan::HoldUntil(due);
    }
    let next = format!("{key}-r{}", retries.len().saturating_add(1));
    RevivalPlan::Submit(Box::new(PlannedCommand {
        key: next,
        action: build_action(),
        note: "retrying a failed revival",
    }))
}

/// Plan a handoff: stop a live execution first, or start (and retry on
/// failure as `CommandAction::Start`) a new native session once stopped.
fn plan_handoff(
    env: &RevivalEnv<'_>,
    last: &Interrupted,
    handoff_prompt: impl FnOnce() -> (String, Option<String>),
) -> RevivalPlan {
    let key = key(env.execution_id, last.attempt);
    let stop = format!("{key}-stop");
    if env.live {
        if env.commands.iter().any(|sent| sent.key == stop) {
            return RevivalPlan::Idle;
        }
        return RevivalPlan::Submit(Box::new(PlannedCommand {
            key: stop,
            action: CommandAction::Stop,
            note: "stopping for its handoff",
        }));
    }
    if let Some(sent) = env.commands.iter().find(|sent| sent.key == key) {
        return match sent.outcome {
            SentOutcome::Pending
            | SentOutcome::Cancelled { .. }
            | SentOutcome::Succeeded { .. } => RevivalPlan::Idle,
            SentOutcome::Failed { .. } => {
                plan_retry_action(env.commands, env.config, env.now_ms, &key, || {
                    let (prompt, charter) = handoff_prompt();
                    start_handoff_action(prompt, charter)
                })
            }
        };
    }
    let due = last.due_ms().unwrap_or(env.now_ms);
    if env.now_ms < due {
        return RevivalPlan::HoldUntil(due);
    }
    let (prompt, charter) = handoff_prompt();
    RevivalPlan::Submit(Box::new(PlannedCommand {
        key,
        action: start_handoff_action(prompt, charter),
        note: "starting a new native session for its handoff",
    }))
}

/// Plan one periodic retry of an expired login while the credentials file holds.
fn plan_auth_retry(
    env: &RevivalEnv<'_>,
    last: &Interrupted,
    native_bound: bool,
    failed_resumes: u32,
    handoff_prompt: impl FnOnce() -> (String, Option<String>),
) -> RevivalPlan {
    let prefix = key(env.execution_id, last.attempt);
    let auth_prefix = format!("{prefix}-auth-");
    let tried = env
        .commands
        .iter()
        .filter(|sent| sent.key.starts_with(&auth_prefix))
        .count();
    let last_try = env
        .commands
        .iter()
        .filter(|sent| sent.key.starts_with(&auth_prefix))
        .map(|sent| sent.created_at_ms)
        .max();
    let due = auth_retry_due(env.config, last.at_ms, last_try);
    if env.now_ms < due {
        return RevivalPlan::Idle;
    }
    let retry_key = format!("{auth_prefix}{}", tried.saturating_add(1));
    match auth_retry_action(env.live, native_bound, failed_resumes) {
        AuthRetry::Input => RevivalPlan::Submit(Box::new(PlannedCommand {
            key: retry_key,
            action: CommandAction::Input {
                text: One::input_text(last),
                tags: Vec::new(),
            },
            note: "retrying the expired login",
        })),
        AuthRetry::Resume => RevivalPlan::Submit(Box::new(PlannedCommand {
            key: retry_key,
            action: CommandAction::Resume {
                prompt: Some(resume_text_for(env.execution_id, last)),
                model: None,
            },
            note: "resuming its native session to retry the expired login",
        })),
        AuthRetry::Handoff => plan_handoff(env, last, handoff_prompt),
    }
}

impl One<'_> {
    /// The session's pass, in order: the outage it lived through, the next
    /// fact of its execution, then the way back from its latest
    /// interruption.
    fn run(&mut self, outage: Option<&Outage>) -> Result<(), String> {
        if let Some(outage) = outage
            && self.ran_through(outage)
            && !self.recorded_outage(outage)
        {
            self.interrupt_outage(outage)?;
        }
        if let Some(fact) = self.next_fact() {
            self.interrupt(fact)?;
        }
        // A worker whose task went to a fresh one is never revived.
        if reassigned_away(&self.row.session_id, self.work) {
            return Ok(());
        }
        self.resume_path()
    }

    fn live(&self) -> bool {
        self.execution.is_live()
    }

    /// The interruptions recorded for the execution, oldest first.
    fn interruptions(&self) -> Vec<&Interrupted> {
        self.records
            .iter()
            .filter_map(|record| match record {
                Recorded::Interrupted(interrupted) => Some(interrupted),
                Recorded::Cleared { .. } => None,
            })
            .collect()
    }

    /// Whether a clear answering `record` has already been recorded.
    fn cleared_after(&self, record: &Interrupted) -> bool {
        self.records.iter().any(|later| {
            matches!(later, Recorded::Cleared { event_id, .. } if *event_id > record.event_id)
        })
    }

    /// Whether the execution ran through the outage: it started before the
    /// first failed pass and had not ended by it.
    fn ran_through(&self, outage: &Outage) -> bool {
        self.execution.started_at_ms < outage.from_ms
            && self
                .execution
                .end()
                .is_none_or(|end| end.ended_at_ms > outage.from_ms)
    }

    /// Whether this outage is already recorded for the execution: its record
    /// names the window's start.
    fn recorded_outage(&self, outage: &Outage) -> bool {
        self.interruptions().iter().any(|record| {
            record.cause == Cause::DatabaseUnavailable
                && record.evidence["from_ms"].as_i64() == Some(outage.from_ms)
        })
    }

    /// The next fact of the execution that no interruption explains, in log
    /// order: the failed turn after the latest one recorded, or the end.
    ///
    /// While a wait is open its execution's end is no interruption of its
    /// own — the wait's revival resumes it — unless a Stop ended it: a
    /// person's decision outranks the wait.
    fn next_fact(&self) -> Option<Fact> {
        pending(
            &self.records,
            &self.row.turns,
            &self.row.failed_tools,
            self.execution,
            &self.row.stops,
            &self.row.commands,
        )
    }

    /// The failed turn the engine ended after when the model refused it for
    /// good, with no progress since ([`refused`]).
    fn refused_turn(&self, end: &End) -> Option<&Turn> {
        let after = self
            .interruptions()
            .last()
            .map_or(0, |record| record.fact_event_id);
        refused(
            &self.row.turns,
            self.row.progress.event_id,
            after,
            Some(end.event_id),
        )
    }

    /// A free-space reading of the workspace's filesystem, taken now: on
    /// the workspace itself while its mount is there, else on the root that
    /// holds every workspace (the same filesystem, on this host).
    fn disk_reading(&self) -> Option<disk::Reading> {
        let mounted = self
            .execution
            .workspace
            .as_deref()
            .map(|workspace| {
                workspace
                    .split_once(':')
                    .map_or(workspace, |(_, path)| path)
                    .to_string()
            })
            .map(std::path::PathBuf::from)
            .filter(|path| path.exists());
        let path = mounted.or_else(|| self.root.map(std::path::Path::to_path_buf))?;
        let free = disk::free_bytes(&path).ok()?;
        Some(disk::Reading {
            place: disk::Place {
                label: "workspace",
                path,
            },
            free,
        })
    }

    fn floor_bytes() -> u64 {
        disk::Thresholds::from_env().floor
    }

    /// Record one interruption of the execution: once, with its cause, its
    /// evidence, and its coupled `(path, due_ms)` schedule.
    fn record(
        &mut self,
        cause: Cause,
        evidence: Map<String, Value>,
        fact_event_id: i64,
        schedule: RecoverySchedule,
        note: &str,
    ) -> Result<Interrupted, String> {
        let execution_id = self.execution.id.as_str();
        let attempt = u32::try_from(self.interruptions().len())
            .unwrap_or(u32::MAX)
            .saturating_add(1);
        let path = schedule.path();
        let due_ms = schedule.due_ms();
        let evidence_val = Value::Object(evidence);
        let record = json!({
            "type": "execution.interrupted",
            "agent_id": self.row.agent_id,
            "session_id": self.row.session_id,
            "execution_id": execution_id,
            "cause": cause.name(),
            "evidence": &evidence_val,
            "path": path.name(),
            "attempt": attempt,
            "due_ms": due_ms,
            "fact_event_id": fact_event_id,
        });
        let event_id = self
            .store
            .host_record(&record)
            .map_err(|error| format!("execution.interrupted: {error}"))?;
        self.lines.push(format!(
            "recover: {} {} of execution {execution_id} (attempt {attempt}, {}): {note}",
            self.row.session_id,
            cause.name(),
            path.name()
        ));
        let interrupted = Interrupted {
            event_id,
            at_ms: self.now_ms,
            cause,
            schedule: RecordedSchedule::Explicit(schedule),
            attempt,
            fact_event_id,
            evidence: evidence_val,
        };
        self.records
            .push(Recorded::Interrupted(interrupted.clone()));
        Ok(interrupted)
    }

    /// Record the clear of the wait `record` opened.
    fn clear(&mut self, record: &Interrupted, evidence: Map<String, Value>) -> Result<(), String> {
        let wait_start = self
            .interruptions()
            .iter()
            .rev()
            .take_while(|older| older.explicit_path() == Some(Path::Wait))
            .last()
            .map_or(record.at_ms, |first| first.at_ms);
        let cleared = json!({
            "type": "execution.cleared",
            "execution_id": self.execution.id,
            "session_id": self.row.session_id,
            "cause": record.cause.name(),
            "evidence": Value::Object(evidence),
            "waited_ms": self.now_ms.saturating_sub(wait_start),
        });
        let event_id = self
            .store
            .host_record(&cleared)
            .map_err(|error| format!("execution.cleared: {error}"))?;
        self.lines.push(format!(
            "recover: {} {cause_name} cleared after {} s",
            self.row.session_id,
            self.now_ms.saturating_sub(wait_start) / 1000,
            cause_name = record.cause.name()
        ));
        self.records.push(Recorded::Cleared {
            event_id,
            at_ms: self.now_ms,
        });
        Ok(())
    }

    // --- recording a fact's interruption ----------------------------------

    /// The interruption an outage makes for an execution that lived through
    /// it, and its clear: the pass that reads this reached the database.
    fn interrupt_outage(&mut self, outage: &Outage) -> Result<(), String> {
        let mut evidence = outage.evidence();
        if let Some(end) = self.execution.end() {
            if let Some(code) = end.exit_code {
                evidence.insert("exit_code".to_string(), json!(code));
            }
            if let Some(signal) = end.signal {
                evidence.insert("signal".to_string(), json!(signal));
            }
        }
        // What the log already knew of the execution is explained by this
        // record: the next fact it waits for is the one after it.
        let explained = self.row.progress.event_id.max(
            self.interruptions()
                .last()
                .map_or(0, |record| record.fact_event_id),
        );
        let record = self.record(
            Cause::DatabaseUnavailable,
            evidence,
            explained,
            RecoverySchedule::Wait {
                due_ms: outage.to_ms,
            },
            "the loop could not reach its database while the execution ran",
        )?;
        let mut cleared = Map::new();
        cleared.insert("from_ms".to_string(), json!(outage.from_ms));
        cleared.insert("to_ms".to_string(), json!(outage.to_ms));
        cleared.insert("failed_passes".to_string(), json!(outage.failed_passes));
        cleared.insert("read_at_ms".to_string(), json!(self.now_ms));
        cleared.insert(
            "checkpoint".to_string(),
            self.row.head.clone().map_or(Value::Null, Value::String),
        );
        self.clear(&record, cleared)
    }

    /// The interruption a fact makes.
    fn interrupt(&mut self, fact: Fact) -> Result<(), String> {
        match fact {
            Fact::Turn { turn, cause } => self.interrupt_turn(&turn, cause),
            Fact::Tool { tool, cause } => self.interrupt_tool(&tool, cause),
            Fact::End(end) => self.interrupt_end(&end),
        }
    }

    fn interrupt_turn(&mut self, turn: &Turn, cause: Cause) -> Result<(), String> {
        let mut evidence = turn.evidence();
        let mut due = self.now_ms;
        match cause {
            Cause::ModelUnavailable => {
                if let Some(named) = retry_at(&turn.text, turn.at_ms.max(self.now_ms)) {
                    evidence.insert("retry_at_ms".to_string(), json!(named));
                }
                let j = self.consecutive(cause).saturating_add(1);
                due = model_due(
                    self.config.model_backoff,
                    j,
                    turn.at_ms.max(self.now_ms),
                    retry_at(&turn.text, turn.at_ms.max(self.now_ms)),
                );
            }
            Cause::ModelAuth => {
                // The credentials file as the loop's environment names it:
                // a wait for a login clears when that file changes.
                if let Some(path) = credentials_path()
                    && let Some(credentials) = Credentials::read(&path)
                {
                    evidence.insert("credentials".to_string(), credentials.json());
                } else if let Some(path) = credentials_path() {
                    evidence.insert(
                        "credentials".to_string(),
                        json!({"path": path, "mtime_ms": Value::Null, "size": Value::Null}),
                    );
                }
                due = self.now_ms.saturating_add(self.config.auth_retry);
            }
            _ => {}
        }
        let (path, mut gains) = way(
            cause,
            self.row.native_session_id.is_some(),
            self.failures_in_a_row(),
        );
        evidence.append(&mut gains);
        self.record(
            cause,
            evidence,
            turn.event_id,
            schedule_for_path(path, || due),
            "its turn failed with a model cause and its engine waits at its prompt",
        )?;
        Ok(())
    }

    /// The interruption a failed tool call makes: a full disk ends the turn
    /// and leaves the engine at its prompt, waiting for room. The wait is
    /// the disk's: every probe reads the free space again, and the clear is
    /// the reading at or above the floor.
    fn interrupt_tool(&mut self, tool: &ToolFailure, cause: Cause) -> Result<(), String> {
        let mut evidence = Map::new();
        evidence.insert("event_id".to_string(), json!(tool.event_id));
        evidence.insert(
            "text".to_string(),
            json!(crate::deliver::one_line(&tool.text, 600)),
        );
        // The reading the wait is told by, taken now; the clear takes a
        // fresh one every probe.
        if let Some(reading) = self.disk_reading() {
            evidence.insert(
                "path".to_string(),
                json!(reading.place.path.display().to_string()),
            );
            evidence.insert("free_bytes".to_string(), json!(reading.free));
            evidence.insert("floor_bytes".to_string(), json!(Self::floor_bytes()));
            evidence.insert("read_at_ms".to_string(), json!(self.now_ms));
        }
        let (path, mut gains) = way(
            cause,
            self.row.native_session_id.is_some(),
            self.failures_in_a_row(),
        );
        evidence.append(&mut gains);
        self.record(
            cause,
            evidence,
            tool.event_id,
            schedule_for_path(path, || self.now_ms),
            "its tool call failed on a full disk and its engine waits at its prompt",
        )?;
        Ok(())
    }

    fn interrupt_end(&mut self, end: &End) -> Result<(), String> {
        // The reading a full disk is told by is taken now: the workspace's
        // filesystem, as the engine found it.
        let disk = if end.exit_code.is_some_and(|code| code != 0) || end.signal.is_some() {
            self.disk_reading()
        } else {
            None
        };
        let below = disk
            .as_ref()
            .filter(|reading| reading.free < Self::floor_bytes())
            .cloned();
        let stop = stop_fact(&self.row.stops, &self.row.session_id, self.work);
        let facts = Facts {
            end: Some(end),
            stop,
            over_budget: self.row.over_budget.as_ref(),
            memory: None,
            terminal_server_gone: server_gone(
                self.execution.terminal_server.as_ref(),
                self.execution.started_at_ms,
                self.terminals.server(),
            ),
            disk: below.as_ref(),
            outage: None,
            retired: self.retired.contains(&self.row.session_id),
            model_error: self.refused_turn(end),
        };
        let cause = classify(&facts);
        let mut evidence = self.end_evidence(cause, end, &facts);
        let (path, mut gains) = stopped_path(
            cause,
            facts.task_cancelled(),
            self.row.native_session_id.is_some(),
            self.failures_in_a_row(),
        );
        evidence.append(&mut gains);
        let schedule = match path {
            Path::Final => RecoverySchedule::Final,
            Path::Resume => RecoverySchedule::Resume {
                due_ms: self.resume_due(),
            },
            Path::Handoff => RecoverySchedule::Handoff {
                due_ms: self.resume_due(),
            },
            Path::Wait => RecoverySchedule::Wait {
                due_ms: self.now_ms,
            },
        };
        self.record(
            cause,
            evidence,
            end.event_id,
            schedule,
            "its execution ended",
        )?;
        Ok(())
    }

    /// The evidence of an end's cause ([`classify`]): what the cause was
    /// told from, named field by field.
    fn end_evidence(&self, cause: Cause, end: &End, facts: &Facts<'_>) -> Map<String, Value> {
        let mut evidence = Map::new();
        match cause {
            Cause::Stopped => {
                if let Some(stop_fact) = &facts.stop {
                    evidence.insert("command_id".to_string(), json!(stop_fact.stop.command_id));
                    evidence.insert("principal".to_string(), json!(stop_fact.stop.principal));
                    evidence.insert("command".to_string(), json!(stop_fact.stop.action));
                }
                if facts.retired {
                    evidence.insert("retired".to_string(), json!(true));
                }
                if facts.task_cancelled() {
                    evidence.insert("task_cancelled".to_string(), json!(true));
                }
            }
            Cause::Restarted | Cause::Reassigned => {
                if let Some(stop_fact) = &facts.stop {
                    moved_evidence(stop_fact, &mut evidence);
                }
            }
            Cause::Budget => {
                if let Some(over) = facts.over_budget {
                    evidence.insert("event_id".to_string(), over["event_id"].clone());
                    evidence.insert("budget".to_string(), over["record"]["budget"].clone());
                    evidence.insert("task".to_string(), over["record"]["id"].clone());
                }
            }
            Cause::MemoryLimit => {
                if let Some(memory) = facts.memory {
                    for key in ["scope", "memory_max", "oom_kills"] {
                        evidence.insert(key.to_string(), memory[key].clone());
                    }
                }
            }
            Cause::HostRestart => {
                evidence.insert("terminal_server".to_string(), json!("gone"));
                evidence.insert("loop_started_at_ms".to_string(), json!(self.now_ms));
                evidence.insert("boot_id".to_string(), json!(boot_id()));
                evidence.insert(
                    "earlier_boot_id".to_string(),
                    self.state
                        .earlier_boot_id()
                        .map_or(Value::Null, |id| json!(id)),
                );
            }
            Cause::DiskFull => {
                if let Some(reading) = facts.disk {
                    evidence.insert(
                        "path".to_string(),
                        json!(reading.place.path.display().to_string()),
                    );
                    evidence.insert("free_bytes".to_string(), json!(reading.free));
                    evidence.insert("floor_bytes".to_string(), json!(Self::floor_bytes()));
                    evidence.insert("read_at_ms".to_string(), json!(self.now_ms));
                }
                if let Some(code) = end.exit_code {
                    evidence.insert("exit_code".to_string(), json!(code));
                }
                if let Some(signal) = end.signal {
                    evidence.insert("signal".to_string(), json!(signal));
                }
            }
            Cause::Crashed => {
                if let Some(code) = end.exit_code {
                    evidence.insert("exit_code".to_string(), json!(code));
                }
                if let Some(signal) = end.signal {
                    evidence.insert("signal".to_string(), json!(signal));
                }
                if let Some(seen) = end.evidence.clone() {
                    evidence.insert("terminal".to_string(), json!(seen));
                }
                evidence.insert("cause".to_string(), json!(end.cause));
            }
            Cause::Exited => {
                evidence.insert(
                    "exit_code".to_string(),
                    end.exit_code.map_or(Value::Null, |code| json!(code)),
                );
            }
            Cause::ModelError => {
                if let Some(turn) = facts.model_error {
                    evidence = turn.evidence();
                }
                if let Some(code) = end.exit_code {
                    evidence.insert("exit_code".to_string(), json!(code));
                }
                if let Some(signal) = end.signal {
                    evidence.insert("signal".to_string(), json!(signal));
                }
            }
            Cause::ContextExhausted
            | Cause::ModelUnavailable
            | Cause::ModelAuth
            | Cause::DatabaseUnavailable => {}
        }
        evidence
    }

    /// Failed resumes in a row since the session's last execution that
    /// stayed up, and the failed revival commands after them.
    fn failures_in_a_row(&self) -> u32 {
        let commands = self.failed_commands();
        self.failed_resumes().saturating_add(commands)
    }

    /// The executions a revival started that died young, since the
    /// session's last execution that stayed up (or made progress): a
    /// revival that made progress succeeded, whatever ended it later.
    fn failed_resumes(&self) -> u32 {
        let healthy = self
            .row
            .executions
            .iter()
            .filter(|execution| !Self::died_young(execution))
            .map(|execution| execution.started_at_ms)
            .max()
            .unwrap_or(0);
        u32::try_from(
            self.row
                .executions
                .iter()
                .filter(|execution| {
                    execution.started_at_ms > healthy
                        && execution.key.starts_with(KEY_PREFIX)
                        && Self::died_young(execution)
                })
                .count(),
        )
        .unwrap_or(u32::MAX)
    }

    fn died_young(execution: &Executed) -> bool {
        resume_failed(
            execution
                .ended_at_ms
                .map(|ended| ended.saturating_sub(execution.started_at_ms)),
            execution.progress,
        )
    }

    /// When the latest failed resume ended.
    fn last_failure_ms(&self) -> Option<i64> {
        self.row
            .executions
            .iter()
            .filter(|execution| {
                execution.key.starts_with(KEY_PREFIX) && Self::died_young(execution)
            })
            .filter_map(|execution| execution.ended_at_ms)
            .max()
    }

    /// Failed `recover-` commands of the execution's latest interruption:
    /// a revival that never started an execution counts as a failed resume.
    fn failed_commands(&self) -> u32 {
        let Some(last) = self.interruptions().last().copied() else {
            return 0;
        };
        let prefix = self.key_prefix(last);
        u32::try_from(
            self.row
                .commands
                .iter()
                .filter(|sent| {
                    sent.key.starts_with(&prefix)
                        && sent.outcome.is_failed()
                        && sent.key.starts_with(KEY_PREFIX)
                })
                .count(),
        )
        .unwrap_or(u32::MAX)
    }

    /// When the next resume is due.
    fn resume_due(&self) -> i64 {
        resume_due(
            self.config.resume_backoff,
            self.failures_in_a_row(),
            self.last_failure_ms(),
            self.now_ms,
        )
    }

    /// How many records in a row hold `cause`, before the new one.
    fn consecutive(&self, cause: Cause) -> u32 {
        u32::try_from(
            self.interruptions()
                .iter()
                .rev()
                .take_while(|record| record.cause == cause)
                .count(),
        )
        .unwrap_or(u32::MAX)
    }

    // --- the way back -----------------------------------------------------

    /// The key of an interruption's revival: `recover-<execution>-<attempt>`.
    fn key_prefix(&self, record: &Interrupted) -> String {
        key(&self.execution.id, record.attempt)
    }

    /// Take the way back from the execution's latest interruption, if it is
    /// due.
    fn resume_path(&mut self) -> Result<(), String> {
        let Some(last) = self.interruptions().last().copied().cloned() else {
            return Ok(());
        };
        match last.effective_path() {
            Path::Final => Ok(()),
            Path::Wait => self.wait_path(&last),
            Path::Resume | Path::Handoff => self.revive(&last, false),
        }
    }

    /// A wait: nothing while the cause holds. When it clears, one
    /// `execution.cleared` record and then the revival.
    fn wait_path(&mut self, last: &Interrupted) -> Result<(), String> {
        if self.cleared_after(last) {
            // Cleared: the revival is due now.
            return self.revive(last, true);
        }
        // The cause still holds: no append may announce its clearing, so the
        // loop re-reads it every probe until it does.
        self.state.open_waits = true;
        match last.cause {
            Cause::ModelUnavailable => {
                // The retry is the revival; the clear is the model's answer
                // to it.
                if self.row.progress.event_id > last.fact_event_id {
                    let mut evidence = Map::new();
                    evidence.insert(
                        "answer_event_id".to_string(),
                        json!(self.row.progress.event_id),
                    );
                    evidence.insert("answer_at_ms".to_string(), json!(self.row.progress.at_ms));
                    self.clear(last, evidence)?;
                    return self.revive(last, true);
                }
                // An engine that ended on it is resumed a bounded number
                // of times in a row; past that, its end is final and the
                // supervisor is told (the dispatcher's digest).
                if let Some(end) = self.execution.end()
                    && !self.revived(last)
                {
                    let resumes = model_resumes(&self.row.executions, &self.execution.id);
                    if resumes >= self.config.model_retries {
                        return self.retries_spent(last, end, resumes);
                    }
                }
                self.revive(last, false)
            }
            Cause::ModelAuth => {
                if let Some(changed) = self.credentials_changed(last) {
                    self.clear(last, changed)?;
                    return self.revive(last, true);
                }
                // One retry every `auth_retry` while the file holds: a
                // long outage of a login must not leave the agent down for
                // good.
                self.auth_retry(last)
            }
            Cause::DiskFull => {
                if let Some(evidence) = self.disk_clear(last) {
                    self.clear(last, evidence)?;
                    return self.revive(last, true);
                }
                Ok(())
            }
            // The pass that reads this reached the database: the clear was
            // recorded with the interruption.
            Cause::DatabaseUnavailable => self.revive(last, true),
            _ => Ok(()),
        }
    }

    /// Whether the terminal host says the execution's runner is no longer
    /// there (its pane dead or gone); a host that cannot tell says nothing.
    fn runner_gone(&self) -> bool {
        let (Ok(id), Ok(workspace)) = (
            spawn_core::id::ExecutionId::new(self.execution.id.as_str()),
            spawn_core::id::WorkspaceId::new(self.execution.workspace.as_deref().unwrap_or("0:/")),
        ) else {
            return false;
        };
        let terminal = spawn_core::id::TerminalName::for_execution(&id);
        matches!(
            self.terminals.runner(&terminal, &workspace),
            Ok(spawn_core::port::Runner::Gone | spawn_core::port::Runner::Untracked(_))
        )
    }

    /// Whether the revival of `last` was already submitted (its key, or a
    /// retry of it).
    fn revived(&self, last: &Interrupted) -> bool {
        let key = self.key_prefix(last);
        self.row
            .commands
            .iter()
            .any(|sent| sent.key == key || sent.key.starts_with(&format!("{key}-")))
    }

    /// The end of an execution whose transient model error was retried
    /// `resumes` times in a row already: recorded once, `model_unavailable`
    /// on the final path, with the failed turn and the setting, and never
    /// revived (a person or the supervisor may resume it).
    fn retries_spent(&mut self, last: &Interrupted, end: &End, resumes: u32) -> Result<(), String> {
        let mut evidence = Map::new();
        for field in ["event_id", "status", "error", "text"] {
            evidence.insert(field.to_string(), last.evidence[field].clone());
        }
        evidence.insert("retries".to_string(), json!(self.config.model_retries));
        evidence.insert("resumes".to_string(), json!(resumes));
        if let Some(code) = end.exit_code {
            evidence.insert("exit_code".to_string(), json!(code));
        }
        if let Some(signal) = end.signal {
            evidence.insert("signal".to_string(), json!(signal));
        }
        let fact = end.event_id.max(last.fact_event_id);
        self.record(
            Cause::ModelUnavailable,
            evidence,
            fact,
            RecoverySchedule::Final,
            "its engine ended on a transient model error again and its retries are spent",
        )?;
        Ok(())
    }

    /// Whether the credentials file changed since the interruption read it;
    /// the evidence when it did.
    fn credentials_changed(&mut self, last: &Interrupted) -> Option<Map<String, Value>> {
        let recorded = &last.evidence["credentials"];
        let path = recorded["path"]
            .as_str()
            .map(str::to_string)
            .or_else(credentials_path)?;
        let key = self.probe_key(last);
        if !self.probe_due(&key) {
            return None;
        }
        let now = Credentials::read(&path)?;
        let was = (recorded["mtime_ms"].as_i64(), recorded["size"].as_u64());
        if was == (Some(now.mtime_ms), Some(now.size)) {
            return None;
        }
        let mut evidence = Map::new();
        evidence.insert("credentials".to_string(), now.json());
        evidence.insert("was".to_string(), json!({"mtime_ms": was.0, "size": was.1}));
        Some(evidence)
    }

    /// The disk has room again; the evidence when it does.
    fn disk_clear(&mut self, last: &Interrupted) -> Option<Map<String, Value>> {
        let key = self.probe_key(last);
        if !self.probe_due(&key) {
            return None;
        }
        let reading = self.disk_reading()?;
        if !disk_cleared(reading.free, Self::floor_bytes()) {
            return None;
        }
        let mut evidence = Map::new();
        evidence.insert(
            "path".to_string(),
            json!(reading.place.path.display().to_string()),
        );
        evidence.insert("free_bytes".to_string(), json!(reading.free));
        evidence.insert("floor_bytes".to_string(), json!(Self::floor_bytes()));
        evidence.insert("read_at_ms".to_string(), json!(self.now_ms));
        Some(evidence)
    }

    /// Whether the probe of `key` is due (every `probe`), and mark it.
    fn probe_due(&mut self, key: &str) -> bool {
        match self.state.probes.get(key) {
            Some(at) if self.now_ms.saturating_sub(*at) < self.config.probe => false,
            _ => {
                self.state.probes.insert(key.to_string(), self.now_ms);
                true
            }
        }
    }

    fn probe_key(&self, last: &Interrupted) -> String {
        format!(
            "{}:{}:{}",
            self.row.session_id, self.execution.id, last.attempt
        )
    }

    /// One retry of an expired login, every `auth_retry` while the
    /// credentials file does not change.
    fn auth_retry(&mut self, last: &Interrupted) -> Result<(), String> {
        let env = RevivalEnv {
            execution_id: &self.execution.id,
            commands: &self.row.commands,
            config: self.config,
            now_ms: self.now_ms,
            live: self.live(),
        };
        let plan = plan_auth_retry(
            &env,
            last,
            self.row.native_session_id.is_some(),
            self.failures_in_a_row(),
            || self.handoff_prompt(last),
        );
        self.apply_plan(plan)
    }

    /// Take the interruption's way back: `input` into the live engine,
    /// `resume` if the execution ended, a `start` for a handoff (its `stop`
    /// first while the execution runs).
    fn revive(&mut self, last: &Interrupted, cleared: bool) -> Result<(), String> {
        let cause = last.cause;
        if cause.path() == Path::Handoff || last.explicit_path() == Some(Path::Handoff) {
            return self.handoff(last);
        }
        let key = self.key_prefix(last);
        // A wait revives once (after its clear); a resume's command is its
        // revival whether or not the wait was cleared.
        if let Some(sent) = self.row.commands.iter().find(|sent| sent.key == key) {
            return match sent.outcome {
                // In flight, cancelled, or done.
                SentOutcome::Pending
                | SentOutcome::Cancelled { .. }
                | SentOutcome::Succeeded { .. } => Ok(()),
                SentOutcome::Failed { .. } => self.retry(last, &key),
            };
        }
        // A wait that cleared is due now: its record's due_ms is when its
        // retry or its clear was expected (an expired login's is half an
        // hour out), and the observation is what made it due.
        let due = if cleared {
            self.now_ms
        } else {
            last.due_ms().unwrap_or(self.now_ms)
        };
        if self.now_ms < due {
            self.state.hold_until(due);
            return Ok(());
        }
        if cause.path() == Path::Wait && !cleared {
            // Not cleared yet: a model wait's retry is its revival (a
            // resume when its engine ended, an input while it stands at
            // its prompt); every other wait waits for its clear.
            if cause != Cause::ModelUnavailable {
                return Ok(());
            }
        }
        if self.live() {
            // An execution that ran through the database outage needs no
            // revival of its own: the loop caught up with it.
            if cause == Cause::DatabaseUnavailable {
                return Ok(());
            }
            // An engine that ended on the model's error is no longer at its
            // prompt, though the Bridge has not recorded its end yet: typing
            // into it would fail, and its end is the resume's (bounded) to
            // answer. The end's record moves the log, and the step runs
            // again.
            if cause == Cause::ModelUnavailable && self.runner_gone() {
                return Ok(());
            }
            self.submit(
                &key,
                CommandAction::Input {
                    text: Self::input_text(last),
                    tags: Vec::new(),
                },
                "nudging the engine at its prompt",
            )
        } else {
            self.submit(
                &key,
                CommandAction::Resume {
                    prompt: Some(self.resume_text(last)),
                    model: None,
                },
                "resuming its native session",
            )
        }
    }

    /// A revival whose command failed: again after the resume's backoff,
    /// under a key of its own.
    fn retry(&mut self, last: &Interrupted, key: &str) -> Result<(), String> {
        let execution_id = &self.execution.id;
        let live = self.live();
        let plan = plan_retry_action(&self.row.commands, self.config, self.now_ms, key, || {
            if live {
                CommandAction::Input {
                    text: Self::input_text(last),
                    tags: Vec::new(),
                }
            } else {
                CommandAction::Resume {
                    prompt: Some(resume_text_for(execution_id, last)),
                    model: None,
                }
            }
        });
        self.apply_plan(plan)
    }

    /// A handoff: stop the execution if it still runs, then start a new
    /// native session in the same Spawn session, workspace and branch.
    fn handoff(&mut self, last: &Interrupted) -> Result<(), String> {
        let env = RevivalEnv {
            execution_id: &self.execution.id,
            commands: &self.row.commands,
            config: self.config,
            now_ms: self.now_ms,
            live: self.live(),
        };
        let plan = plan_handoff(&env, last, || self.handoff_prompt(last));
        self.apply_plan(plan)
    }

    fn apply_plan(&mut self, plan: RevivalPlan) -> Result<(), String> {
        match plan {
            RevivalPlan::Idle => Ok(()),
            RevivalPlan::HoldUntil(due) => {
                self.state.hold_until(due);
                Ok(())
            }
            RevivalPlan::Submit(planned) => self.submit(&planned.key, planned.action, planned.note),
        }
    }

    /// Submit one command as the recovery; the engine runs on under the disk
    /// floor, but no new execution starts there (the supervisor's own start
    /// excepted: someone must be able to act).
    fn submit(&mut self, key: &str, action: CommandAction, note: &str) -> Result<(), String> {
        let starts = matches!(
            action,
            CommandAction::Start { .. } | CommandAction::Resume { .. }
        );
        if starts
            && self.monitor.below_floor()
            && self.row.session_id != supervise::SUPERVISOR_SESSION
        {
            self.lines.push(format!(
                "recover: {} held ({key}): free disk is under SPAWN_DISK_FLOOR_GB",
                self.row.session_id
            ));
            return Ok(());
        }
        let submission = spawn_core::command::Submission {
            agent_id: AgentId::new(&self.row.agent_id).map_err(|error| error.to_string())?,
            session_id: SessionId::new(&self.row.session_id).map_err(|error| error.to_string())?,
            idempotency_key: IdempotencyKey::new(key).map_err(|error| error.to_string())?,
            action,
        };
        let principal = Principal::new(RECOVERER).map_err(|error| error.to_string())?;
        self.store
            .submit(&principal, &submission)
            .map_err(|error| format!("{key}: {error}"))?;
        self.lines
            .push(format!("recover: {} {key}: {note}", self.row.session_id));
        Ok(())
    }

    // --- what the agent is told -------------------------------------------

    /// What the agent is told when its engine is resumed or nudged: the
    /// interruption, by its cause and evidence, and that its work is where
    /// it left it.
    fn resume_text(&self, last: &Interrupted) -> String {
        resume_text_for(&self.execution.id, last)
    }

    fn input_text(last: &Interrupted) -> String {
        if last.is_cancelled_stop() {
            return CANCELLED_REVIVAL.to_string();
        }
        format!(
            "Spawn: the {} that stopped your last turn is over ({}). Continue your task from \
             where you were; the work is in your workspace.",
            last.cause.name().replace('_', " "),
            describe(last)
        )
    }

    /// The prompt of a handoff: the task's brief (a resident's role, the
    /// supervisor's charter and summary), the interruption, the branch's
    /// head, and that the work is in the workspace to continue.
    fn handoff_prompt(&self, last: &Interrupted) -> (String, Option<String>) {
        let head = self
            .row
            .head
            .as_ref()
            .map_or_else(String::new, |head| format!(" (its head is {head})"));
        let interruption = handoff_interruption(last, &head);
        if self.row.session_id == supervise::SUPERVISOR_SESSION {
            let supervise::Charter { text, hash } = supervise::Charter::deployed(self.spawn);
            let summary = supervise::handoff(self.store);
            return (format!("{text}{summary}{interruption}"), Some(hash));
        }
        if let Some(charter) = self.residents.get(&self.row.session_id) {
            let name = self
                .row
                .session_id
                .trim_start_matches("session_resident_")
                .to_string();
            return (
                format!("{}{interruption}", deliver::resident_role(&name, charter)),
                None,
            );
        }
        let brief = self.row_task().map_or_else(
            || "You are a Spawn worker. Continue your task.".to_string(),
            deliver::task_prompt,
        );
        (format!("{brief}{interruption}"), None)
    }

    /// The session's latest delivered task, if any.
    fn row_task(&self) -> Option<&crate::deliver::TaskRow> {
        self.work
            .tasks
            .iter()
            .rfind(|task| task.session_id == self.row.session_id)
    }
}

/// The event details of the interruption `record`, one line: what the agent
/// is told and the log shows.
fn describe(record: &Interrupted) -> String {
    let evidence = &record.evidence;
    let parts: Vec<String> = [
        "exit_code",
        "signal",
        "status",
        "text",
        "command_id",
        "free_bytes",
    ]
    .iter()
    .filter_map(|key| {
        let value = &evidence[*key];
        (!value.is_null()).then(|| format!("{key} {value}"))
    })
    .collect();
    let cause = record.cause.name();
    if parts.is_empty() {
        cause.to_string()
    } else {
        format!(
            "{cause}: {}",
            crate::deliver::one_line(&parts.join(", "), 300)
        )
    }
}

/// The failed turn an execution's end is told from when the model refused
/// it for good: the latest failed turn after `after` (the fact of the latest
/// interruption) and before the end (`end_event_id`, when known), if it is
/// a model error ([`Turn::model_error`]) and nothing made progress after it
/// (`progress_event_id`: the latest tool call or turn end).
pub(crate) fn refused(
    turns: &[Turn],
    progress_event_id: i64,
    after: i64,
    end_event_id: Option<i64>,
) -> Option<&Turn> {
    let turn = turns
        .iter()
        .rev()
        .find(|turn| turn.event_id > after && end_event_id.is_none_or(|end| turn.event_id < end))?;
    (turn.model_error() && progress_event_id < turn.event_id).then_some(turn)
}

/// The next fact of `execution` that no interruption explains, in log
/// order: the first failed turn or failed tool call after the latest one
/// recorded, or the end.
///
/// While a wait is open its execution's end is no interruption of its own —
/// the wait's revival resumes it — unless a Stop ended it: a person's
/// decision outranks the wait. A handoff's own Stop is no interruption
/// either: that end is the revival's. A restarted loop reads the same
/// records and so finds nothing to record twice.
fn pending(
    records: &[Recorded],
    turns: &[Turn],
    tools: &[ToolFailure],
    execution: &Execution,
    stops: &[Stop],
    commands: &[Sent],
) -> Option<Fact> {
    let interruptions: Vec<&Interrupted> = records
        .iter()
        .filter_map(|record| match record {
            Recorded::Interrupted(interrupted) => Some(interrupted),
            Recorded::Cleared { .. } => None,
        })
        .collect();
    let last = interruptions.last().copied();
    let after = last.map_or(0, |record| record.fact_event_id);
    let cleared = |record: &Interrupted| {
        records.iter().any(|later| {
            matches!(later, Recorded::Cleared { event_id, .. } if *event_id > record.event_id)
        })
    };
    let open_wait =
        last.is_some_and(|record| record.explicit_path() == Some(Path::Wait) && !cleared(record));
    let turn = turns.iter().find_map(|turn| {
        turn.cause()
            .filter(|_| turn.event_id > after)
            .map(|cause| (turn, cause))
    });
    let tool = tools.iter().find_map(|tool| {
        tool.cause()
            .filter(|_| tool.event_id > after)
            .map(|cause| (tool, cause))
    });
    match (turn, tool) {
        // The first of the two in the log: a disk's tool failure and a
        // model's turn failure can sit in the same stretch of work.
        (Some((turn, _)), Some((tool, tool_cause))) if tool.event_id < turn.event_id => {
            return Some(Fact::Tool {
                tool: tool.clone(),
                cause: tool_cause,
            });
        }
        (Some((turn, turn_cause)), _) => {
            return Some(Fact::Turn {
                turn: turn.clone(),
                cause: turn_cause,
            });
        }
        (None, Some((tool, tool_cause))) => {
            return Some(Fact::Tool {
                tool: tool.clone(),
                cause: tool_cause,
            });
        }
        (None, None) => {}
    }
    // A handoff ends the execution with its own Stop.
    if last.is_some_and(|record| {
        record.explicit_path() == Some(Path::Handoff)
            && commands
                .iter()
                .any(|sent| sent.key == format!("{}-stop", key(&execution.id, record.attempt)))
    }) {
        return None;
    }
    let end = execution.end()?;
    if end.event_id <= after {
        return None;
    }
    let stopped = stops
        .iter()
        .rev()
        .any(|stop| !stop.ours() && stop.reached());
    if open_wait && !stopped {
        return None;
    }
    Some(Fact::End(end.clone()))
}

#[cfg(test)]
mod credentials_tests {
    use super::resolve_credentials_path;

    fn lookup(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(known, _)| *known == name)
                .map(|(_, value)| (*value).to_string())
        }
    }

    /// Issue #79's review (F5): the login an expired-login wait watches is
    /// the engines' own, in the order `spawn loop` gives them a directory.
    #[test]
    fn the_credentials_are_the_engines_own_directorys() {
        let all = lookup(&[
            ("HOME", "/home/o"),
            ("CLAUDE_CONFIG_DIR", "/cc"),
            ("SPAWN_CLAUDE_CONFIG_DIR", "/data/fleet-claude/"),
        ]);
        assert_eq!(
            resolve_credentials_path(all).as_deref(),
            Some("/data/fleet-claude/.credentials.json")
        );
        let claude = lookup(&[
            ("HOME", "/home/o"),
            ("CLAUDE_CONFIG_DIR", "/cc"),
            ("SPAWN_CLAUDE_CONFIG_DIR", ""),
        ]);
        assert_eq!(
            resolve_credentials_path(claude).as_deref(),
            Some("/cc/.credentials.json")
        );
        let relative = lookup(&[("HOME", "/home/o"), ("SPAWN_CLAUDE_CONFIG_DIR", "fleet")]);
        assert_eq!(
            resolve_credentials_path(relative).as_deref(),
            Some("/home/o/.claude/.credentials.json")
        );
        let default_isolated = lookup(&[("HOME", "/home/o")]);
        assert_eq!(
            resolve_credentials_path(default_isolated).as_deref(),
            Some("/home/o/.local/state/spawn/claude-config/.credentials.json")
        );
        let inherited = lookup(&[
            ("HOME", "/home/o"),
            ("CLAUDE_CONFIG_DIR", "/cc"),
            ("SPAWN_CLAUDE_CONFIG_DIR", "inherit"),
        ]);
        assert_eq!(
            resolve_credentials_path(inherited).as_deref(),
            Some("/home/o/.claude/.credentials.json")
        );
        assert_eq!(resolve_credentials_path(lookup(&[])), None);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AuthRetry, Cause, CauseExt, CommandAction, Config, Credentials, ENGINE, End, EndCause,
        Executed, Execution, ExecutionState, Facts, FactsRow, Interrupted, LoopStart, Outage, Path,
        PlannedCommand, Recorded, RecordedEndCause, RecordedSchedule, Recovery, RecoverySchedule,
        RevivalEnv, RevivalPlan, Sent, SentOutcome, Stop, StopAction, StopFact, StopKind,
        ToolFailure, Turn, auth_retry_action, auth_retry_due, classify, counts_as_failure,
        disk_cleared, key, model_due, model_resumes, model_retries, pending, plan_auth_retry,
        refused, resume_due, resume_failed, retry_at, server_gone, stop_fact, visit_pass_rows, way,
    };
    use serde_json::{Value, json};

    /// A cancelled task's stop is the dispatcher's interrupt: the notice's
    /// resume follows it, so the recovery must not record it as `final`
    /// (`interrupt_end`). A person's stop, the supervisor's, the
    /// dispatcher's other stops and the recovery's own still end it.
    #[test]
    fn only_a_dispatchers_cancel_stop_is_an_interrupt() {
        let stop = |principal: &str, key: &str| Stop {
            command_id: 1,
            principal: principal.to_string(),
            key: key.to_string(),
            action: StopAction::Stop,
            created_at_ms: 0,
            succeeded: Some(true),
            error: None,
        };
        let dispatcher = crate::dispatch::DISPATCHER;
        assert!(stop(dispatcher, "cancel-stop-63-exec_482").cancelled_task());
        assert!(!stop(dispatcher, "restart-18-1-stop").cancelled_task());
        assert!(!stop(dispatcher, "reassign-18-stop").cancelled_task());
        assert!(!stop(dispatcher, "pause-8-23954-exec_206").cancelled_task());
        assert!(!stop("bb-person", "cancel-stop-63-exec_482").cancelled_task());
        assert!(!stop("spawn-supervisor", "cancel-stop-63-exec_482").cancelled_task());
        assert!(!stop("spawn-liveness", "recover-exec_4-1-stop").cancelled_task());

        // Its end is `stopped`, on the way back of a resume; any other
        // stop's end stays final.
        let cancelled = stop(dispatcher, "cancel-stop-63-exec_482");
        let facts = Facts {
            stop: Some(StopFact {
                stop: &cancelled,
                kind: StopKind::CancelledTask,
            }),
            ..Facts::default()
        };
        assert!(facts.task_cancelled());
        let cause = classify(&facts);
        assert_eq!(cause, Cause::Stopped);
        let (path, evidence) = super::stopped_path(cause, true, true, 0);
        assert_eq!(path, Path::Resume);
        assert!(evidence.is_empty());
        assert_eq!(super::stopped_path(cause, false, true, 0).0, Path::Final);
        assert_eq!(
            super::stopped_path(Cause::Restarted, true, true, 0).0,
            Path::Final
        );

        // A cancel stop that failed and succeeded only on the dispatcher's
        // retry (`stop-<execution>-retry-<n>`) is still the cancelled
        // task's stop: the retry carries its purpose, as for `moved`.
        let failed = Stop {
            succeeded: Some(false),
            error: Some("kill-session refused".to_string()),
            ..stop(dispatcher, "cancel-stop-63-exec_482")
        };
        let retried = [failed.clone(), stop(dispatcher, "stop-exec_482-retry-1")];
        assert!(
            !retried[1].cancelled_task(),
            "the retry's own key says nothing"
        );
        let empty_work = crate::deliver::Work::default();
        assert_eq!(
            stop_fact(&retried, "session", &empty_work),
            Some(StopFact {
                stop: &retried[1],
                kind: StopKind::CancelledTask,
            })
        );
        assert_eq!(
            stop_fact(std::slice::from_ref(&cancelled), "session", &empty_work),
            Some(StopFact {
                stop: &cancelled,
                kind: StopKind::CancelledTask,
            })
        );
        // A retry of a restart's stop, a person's stop after the cancel
        // stop, or nothing that reached the engine: not a cancelled task's.
        let restart_retried = [
            stop(dispatcher, "restart-18-1-stop"),
            stop(dispatcher, "stop-exec_482-retry-1"),
        ];
        assert_eq!(
            stop_fact(&restart_retried, "session", &empty_work),
            Some(StopFact {
                stop: &restart_retried[1],
                kind: StopKind::Other,
            })
        );
        let person_after = [cancelled.clone(), stop("bb-person", "person-stop-1")];
        assert_eq!(
            stop_fact(&person_after, "session", &empty_work),
            Some(StopFact {
                stop: &person_after[1],
                kind: StopKind::Other,
            })
        );
        assert_eq!(stop_fact(&[failed], "session", &empty_work), None);
        assert_eq!(stop_fact(&[], "session", &empty_work), None);
    }

    /// A cancelled task's stop resumes under `way`'s bounds: with no native
    /// session to resume (the Bridge would refuse every resume), or after
    /// three failed resumes in a row, it is a handoff, never a resume
    /// retried without end. With a native session and fewer failures it is
    /// a resume.
    #[test]
    fn a_cancelled_tasks_stop_with_no_native_session_hands_off() {
        let (path, evidence) = super::stopped_path(Cause::Stopped, true, false, 0);
        assert_eq!(path, Path::Handoff);
        assert_eq!(evidence["native"], json!("none"));
        for failures in 0..super::RESUME_ATTEMPTS {
            let (path, evidence) = super::stopped_path(Cause::Stopped, true, true, failures);
            assert_eq!(path, Path::Resume, "{failures} failed resumes");
            assert!(evidence.is_empty());
        }
        let (path, evidence) =
            super::stopped_path(Cause::Stopped, true, true, super::RESUME_ATTEMPTS);
        assert_eq!(path, Path::Handoff);
        assert_eq!(evidence["resumes_failed"], json!(super::RESUME_ATTEMPTS));
        // Any other stop stays final, native session or not.
        assert_eq!(
            super::stopped_path(Cause::Stopped, false, false, 0).0,
            Path::Final
        );

        // The handoff's new native session is not told to carry on.
        let record = |evidence: Value| Interrupted {
            event_id: 1,
            at_ms: 1_000,
            cause: Cause::Stopped,
            schedule: RecordedSchedule::Explicit(RecoverySchedule::Handoff { due_ms: 1_000 }),
            attempt: 1,
            fact_event_id: 0,
            evidence,
        };
        let cancelled = record(
            json!({"principal": "spawn-dispatch", "task_cancelled": true,
                                      "native": "none"}),
        );
        let text = super::handoff_interruption(&cancelled, "");
        assert!(text.contains("Do not continue that task"), "{text}");
        assert!(!text.contains("carry on"), "{text}");
        let other = record(json!({"principal": "spawn-dispatch"}));
        assert!(super::handoff_interruption(&other, "").contains("carry on"));
    }

    /// `resume_path` takes the record's own path (GitHub #76): a `stopped`
    /// recorded as `resume` is revived, a `final` one (a person's stop, or
    /// a wait made final) never is, and a record that names no path takes
    /// its cause's own.
    #[test]
    fn the_way_back_is_the_recorded_path_and_a_final_one_is_never_revived() {
        let parse_interrupted = |cause: &str, path: Option<&str>| -> Interrupted {
            let record = match path {
                Some("final") => json!({"type": "execution.interrupted", "cause": cause,
                                        "path": "final", "attempt": 1, "due_ms": null}),
                Some(path) => json!({"type": "execution.interrupted", "cause": cause,
                                     "path": path, "attempt": 1, "due_ms": 1_000}),
                None => json!({"type": "execution.interrupted", "cause": cause, "attempt": 1}),
            };
            let parsed: Recorded = serde_json::from_value(json!({
                "event_id": 1,
                "at_ms": 1_000,
                "record": record,
            }))
            .expect("valid interrupted record");
            match parsed {
                Recorded::Interrupted(interrupted) => interrupted,
                Recorded::Cleared { .. } => panic!("expected interrupted"),
            }
        };
        assert_eq!(
            parse_interrupted("stopped", Some("resume")).effective_path(),
            Path::Resume
        );
        assert_eq!(
            parse_interrupted("stopped", Some("final")).effective_path(),
            Path::Final
        );
        assert_eq!(
            parse_interrupted("stopped", None).effective_path(),
            Path::Final
        );
        assert_eq!(
            parse_interrupted("stopped", Some("handoff")).effective_path(),
            Path::Handoff
        );
        assert_eq!(
            parse_interrupted("stopped", Some("wait")).effective_path(),
            Path::Wait
        );
        // A model wait whose retries are spent is recorded final: skipped.
        assert_eq!(
            parse_interrupted("model_unavailable", Some("final")).effective_path(),
            Path::Final
        );
        assert_eq!(
            parse_interrupted("model_unavailable", None).effective_path(),
            Path::Wait
        );
    }

    /// The revival after a cancelled task's stop never tells the worker to
    /// continue: its task is cancelled. Any other revival still does.
    #[test]
    fn the_revival_after_a_cancelled_tasks_stop_does_not_continue_it() {
        let record = |cause: Cause, evidence: Value| Interrupted {
            event_id: 1,
            at_ms: 1_000,
            cause,
            schedule: RecordedSchedule::Explicit(RecoverySchedule::Resume { due_ms: 1_000 }),
            attempt: 1,
            fact_event_id: 0,
            evidence,
        };
        let cancelled = record(
            Cause::Stopped,
            json!({"principal": "spawn-dispatch", "task_cancelled": true}),
        );
        assert!(cancelled.is_cancelled_stop());
        let text = super::One::input_text(&cancelled);
        assert_eq!(text, super::CANCELLED_REVIVAL);
        assert!(text.contains("Do not continue that task"), "{text}");
        assert!(!text.contains("Continue your task"), "{text}");
        let crashed = record(Cause::Crashed, json!({"exit_code": 1}));
        assert!(!crashed.is_cancelled_stop());
        assert!(super::One::input_text(&crashed).contains("Continue your task"));
        assert!(!record(Cause::Stopped, json!({"principal": "bb-person"})).is_cancelled_stop());
    }
    use spawn_core::port::{ServerSeen, TerminalServer};

    /// A host restart is the server that ran the execution gone, told by
    /// its identity, whatever answers when the end is read and whatever
    /// order the ends are read in (objective 44).
    #[test]
    fn a_terminal_server_is_gone_when_the_one_that_ran_the_execution_is_gone() {
        // The server before the restart, and the one the first resume
        // started after it, 3.5 s into its launch.
        let before = TerminalServer {
            pid: 812,
            started_at_ms: 1_000_000,
        };
        let after = TerminalServer {
            pid: 812,
            started_at_ms: 9_000_000,
        };
        // Recorded at 5_000_000, before the restart; ran on `before`.
        let recorded = 5_000_000;
        // (a) No server answers: gone.
        assert!(server_gone(Some(&before), recorded, ServerSeen::Gone));
        assert!(server_gone(None, recorded, ServerSeen::Gone));
        // (a) Another server answers (a resume started it): gone, read
        // before or after that resume alike. The same pid is not the
        // same server.
        assert!(server_gone(
            Some(&before),
            recorded,
            ServerSeen::Answers(Some(after))
        ));
        let other_pid = TerminalServer { pid: 813, ..before };
        assert!(server_gone(
            Some(&before),
            recorded,
            ServerSeen::Answers(Some(other_pid))
        ));
        // (b) The server that ran it answers: not gone, a crash. Also for an
        // execution recorded before the slow launch that started its
        // server: its server is younger than its record, and is its own.
        assert!(!server_gone(
            Some(&before),
            recorded,
            ServerSeen::Answers(Some(before))
        ));
        let resumed_at = after.started_at_ms - 3_500;
        assert!(!server_gone(
            Some(&after),
            resumed_at,
            ServerSeen::Answers(Some(after))
        ));
        // (c) No identity recorded (before this change, or a failed
        // write): gone when the answering server started after the record,
        // not gone when it started before.
        assert!(server_gone(
            None,
            recorded,
            ServerSeen::Answers(Some(after))
        ));
        assert!(!server_gone(
            None,
            recorded,
            ServerSeen::Answers(Some(before))
        ));
        // A host that cannot tell which server answers keeps today's rule:
        // a crash, never a restart it cannot show.
        assert!(!server_gone(
            Some(&before),
            recorded,
            ServerSeen::Answers(None)
        ));
        assert!(!server_gone(None, recorded, ServerSeen::Answers(None)));
    }

    #[test]
    fn an_execution_reads_the_server_its_terminal_started_on() {
        let row: Execution = serde_json::from_value(json!({
            "id": "exec_7",
            "started_at_ms": 1_000,
            "terminal_server": {"pid": 812, "started_at_ms": 2_000},
        }))
        .expect("an execution with its server");
        assert!(row.is_live());
        assert_eq!(
            row.terminal_server,
            Some(TerminalServer {
                pid: 812,
                started_at_ms: 2_000,
            })
        );
        let old: Execution = serde_json::from_value(json!({
            "id": "exec_7",
            "started_at_ms": 1_000,
            "terminal_server": null,
        }))
        .expect("an execution from before the record");
        assert!(old.is_live());
        assert_eq!(old.terminal_server, None);
    }

    /// The loop's next pass reads again although the log did not move only
    /// while a wait is open, a revival is pending or an outage is pending:
    /// with none, the recovery waits for the log (an idle pass costs the
    /// watermark read alone), and a pending revival is never lost to its
    /// backoff.
    #[test]
    fn the_loop_re_reads_for_a_wait_a_pending_revival_or_an_outage() {
        let mut state = Recovery::new();
        assert_eq!(state.next_due(10_000, 1_000), None, "nothing to read for");
        state.open_waits = true;
        assert_eq!(
            state.next_due(10_000, 1_000),
            Some(11_000),
            "a wait's cause is re-read every probe"
        );
        state.open_waits = false;
        state.pending_due = Some(4_000);
        assert_eq!(
            state.next_due(10_000, 2_000),
            Some(4_000),
            "a revival behind its backoff runs when it is due"
        );
        state.pending_due = None;
        state.failed(1_000, "the database is unreachable");
        assert!(state.outage_pending(), "the first good pass records it");
        assert_eq!(state.next_due(10_000, 2_000), Some(2_000), "at once");
    }

    fn end(cause: EndCause, exit_code: Option<i32>, signal: Option<i32>) -> End {
        End {
            event_id: 90,
            ended_at_ms: 2_000,
            cause: cause.into(),
            exit_code,
            signal,
            evidence: None,
        }
    }

    fn turn(status: Option<i64>, kind: Option<&str>, text: &str) -> Turn {
        Turn {
            event_id: 50,
            at_ms: 1_000_000,
            status,
            error_kind: kind.map(str::to_string),
            text: text.to_string(),
        }
    }

    fn stop(key: &str, command_id: i64) -> Stop {
        Stop {
            command_id,
            principal: "bb-person".to_string(),
            key: key.to_string(),
            action: StopAction::Stop,
            created_at_ms: 0,
            succeeded: Some(true),
            error: None,
        }
    }

    /// The facts a cause is told from, with every other fact absent.
    fn facts(end: Option<&End>) -> Facts<'_> {
        Facts {
            end,
            ..Facts::default()
        }
    }

    #[test]
    fn a_signal_is_a_crash_and_a_clean_exit_is_an_exit() {
        let crashed = end(EndCause::Exited, None, Some(9));
        assert_eq!(classify(&facts(Some(&crashed))), Cause::Crashed);
        let nonzero = end(EndCause::Exited, Some(1), None);
        assert_eq!(classify(&facts(Some(&nonzero))), Cause::Crashed);
        let clean = end(EndCause::Exited, Some(0), None);
        assert_eq!(classify(&facts(Some(&clean))), Cause::Exited);
        // A terminal that disappeared while its server lives on is a crash.
        let gone = end(EndCause::Lost, None, None);
        assert_eq!(classify(&facts(Some(&gone))), Cause::Crashed);
        // A stop the Bridge recorded on its own (a workspace kept off its
        // branch) ends the session: nothing revives it.
        let stopped = end(EndCause::Stopped, None, None);
        assert_eq!(classify(&facts(Some(&stopped))), Cause::Stopped);
    }

    #[test]
    fn the_more_specific_fact_wins() {
        let nonzero = end(EndCause::Exited, Some(1), None);
        // A non-zero exit under the disk floor is a full disk, not a crash.
        let reading = crate::disk::Reading {
            place: crate::disk::Place {
                label: "workspace",
                path: std::path::PathBuf::from("/ws"),
            },
            free: 100 * 1024 * 1024,
        };
        let full = Facts {
            disk: Some(&reading),
            ..facts(Some(&nonzero))
        };
        assert_eq!(classify(&full), Cause::DiskFull);
        // A host restart outranks the full disk it may have found.
        let restarted = Facts {
            terminal_server_gone: true,
            ..full
        };
        assert_eq!(classify(&restarted), Cause::HostRestart);
        // A person's Stop outranks everything.
        let stopped = stop("bb-stop-1", 7);
        let by_person = Facts {
            stop: Some(StopFact {
                stop: &stopped,
                kind: StopKind::Other,
            }),
            terminal_server_gone: true,
            ..facts(Some(&nonzero))
        };
        assert_eq!(classify(&by_person), Cause::Stopped);
        // The budget outranks the end the integrator's Stop made.
        let over = json!({"event_id": 12, "record": {"budget": {"tool_calls": 150}}});
        let over_budget = Facts {
            over_budget: Some(&over),
            ..facts(Some(&nonzero))
        };
        assert_eq!(classify(&over_budget), Cause::Budget);
        // A memory kill outranks the signal a host restart would explain.
        let memory = json!({"scope": "agent_worker_x", "memory_max": 4096, "oom_kills": 1});
        let oom = Facts {
            memory: Some(&memory),
            terminal_server_gone: true,
            ..facts(Some(&nonzero))
        };
        assert_eq!(classify(&oom), Cause::MemoryLimit);
        // An end inside a database outage is the outage's.
        let outage = Outage {
            from_ms: 10,
            to_ms: 20,
            failed_passes: 3,
            error: "connection closed".to_string(),
        };
        let dark = Facts {
            outage: Some(&outage),
            ..facts(Some(&nonzero))
        };
        assert_eq!(classify(&dark), Cause::DatabaseUnavailable);
        // An end found at recovery with its terminal server gone.
        let lost_end = end(EndCause::Lost, None, None);
        let lost = Facts {
            terminal_server_gone: true,
            ..facts(Some(&lost_end))
        };
        assert_eq!(classify(&lost), Cause::HostRestart);
    }

    #[test]
    fn the_model_causes_are_read_from_the_engines_tags() {
        assert_eq!(
            turn(Some(529), Some("overloaded"), "API Error: 529").cause(),
            Some(Cause::ModelUnavailable)
        );
        assert_eq!(
            turn(Some(503), None, "API Error: 503 no eligible upstream").cause(),
            Some(Cause::ModelUnavailable)
        );
        assert_eq!(
            turn(Some(429), Some("rate_limit"), "API Error: 429 quota").cause(),
            Some(Cause::ModelUnavailable)
        );
        // A dropped connection has no status at all.
        assert_eq!(
            turn(
                None,
                Some("unknown"),
                "API Error: Connection lost mid-response"
            )
            .cause(),
            Some(Cause::ModelUnavailable)
        );
        assert_eq!(
            turn(Some(401), Some("authentication_failed"), "API Error: 401").cause(),
            Some(Cause::ModelAuth)
        );
        assert_eq!(
            turn(Some(403), None, "API Error: 403 Request not allowed").cause(),
            Some(Cause::ModelAuth)
        );
        // Only the text: "Not logged in · Please run /login".
        assert_eq!(
            turn(None, None, "Not logged in · Please run /login").cause(),
            Some(Cause::ModelAuth)
        );
        assert_eq!(
            turn(Some(400), Some("invalid_request"), "Prompt is too long").cause(),
            Some(Cause::ContextExhausted)
        );
        // An engine that wrote no status (Claude Code before 2.1).
        assert_eq!(
            turn(None, None, "API Error: 529 {\"type\":\"error\"}").cause(),
            Some(Cause::ModelUnavailable)
        );
        assert_eq!(
            turn(None, None, "Prompt is too long").cause(),
            Some(Cause::ContextExhausted)
        );
        // A refused request of another kind is no interruption at all.
        assert_eq!(
            turn(
                Some(400),
                Some("invalid_request"),
                "max_tokens is too large"
            )
            .cause(),
            None
        );
    }

    #[test]
    fn a_quota_message_that_names_a_time_sets_retry_at() {
        let at = 20 * 3_600_000; // 20:00 UTC
        let named = retry_at(
            "API Error: Request rejected (429) · upstream account quota usage limit has been \
             reached · available again at 22:00 UTC",
            at,
        );
        assert_eq!(named, Some(22 * 3_600_000));
        // A time already past is the next day's.
        assert_eq!(
            retry_at("available again at 09:30 UTC", 20 * 3_600_000),
            Some(86_400_000 + 9 * 3_600_000 + 30 * 60_000)
        );
        assert_eq!(retry_at("API Error: 529 Overloaded", at), None);
        assert_eq!(retry_at("available again at 99:00 UTC", at), None);
    }

    #[test]
    fn the_first_resume_is_due_at_once_then_doubles_to_its_cap() {
        assert_eq!(resume_due(10_000, 0, None, 5_000), 5_000);
        assert_eq!(resume_due(10_000, 1, Some(1_000), 0), 1_000 + 20_000);
        assert_eq!(resume_due(10_000, 2, Some(1_000), 0), 1_000 + 40_000);
        assert_eq!(resume_due(10_000, 3, Some(1_000), 0), 1_000 + 80_000);
        // 64 x the base: 640 s by default.
        assert_eq!(resume_due(10_000, 4, Some(0), 0), 160_000);
        assert_eq!(resume_due(10_000, 64, Some(0), 0), 640_000);
        assert_eq!(resume_due(10_000, 500, Some(0), 0), 640_000);
    }

    #[test]
    fn the_model_retry_doubles_to_its_cap_and_waits_for_a_named_time() {
        assert_eq!(model_due(60_000, 1, 0, None), 60_000);
        assert_eq!(model_due(60_000, 2, 0, None), 120_000);
        assert_eq!(model_due(60_000, 3, 0, None), 240_000);
        // 4 x the base: 4 minutes, forever after.
        assert_eq!(model_due(60_000, 4, 0, None), 240_000);
        assert_eq!(model_due(60_000, 9, 0, None), 240_000);
        // A named time later than the backoff wins; an earlier one does not.
        assert_eq!(model_due(60_000, 1, 0, Some(900_000)), 900_000);
        assert_eq!(model_due(60_000, 4, 0, Some(1_000)), 240_000);
    }

    #[test]
    fn a_resume_that_made_progress_did_not_fail() {
        // Ended at once, before any tool call or turn: a failed resume.
        assert!(resume_failed(Some(3_000), false));
        // Worked for 20 s and was killed: it made progress, and a revival
        // that made progress succeeded.
        assert!(!resume_failed(Some(20_000), true));
        assert!(!resume_failed(Some(3_000), true));
        // Stayed up past a minute.
        assert!(!resume_failed(Some(90_000), false));
        // Still running.
        assert!(!resume_failed(None, false));
    }

    #[test]
    fn three_failed_resumes_hand_off_and_final_is_never_revived() {
        for failures in 0..3 {
            let (path, evidence) = way(Cause::Crashed, true, failures);
            assert_eq!(path, Path::Resume, "{failures} failed resumes");
            assert!(evidence.is_empty());
        }
        let (path, evidence) = way(Cause::Crashed, true, 3);
        assert_eq!(path, Path::Handoff);
        assert_eq!(evidence["resumes_failed"], json!(3));
        // Nothing to resume: a new native session.
        let (path, evidence) = way(Cause::Crashed, false, 0);
        assert_eq!(path, Path::Handoff);
        assert_eq!(evidence["native"], json!("none"));
        // Each cause's own way.
        assert_eq!(way(Cause::ContextExhausted, true, 0).0, Path::Handoff);
        assert_eq!(way(Cause::Budget, true, 0).0, Path::Handoff);
        for wait in [
            Cause::ModelUnavailable,
            Cause::ModelAuth,
            Cause::DatabaseUnavailable,
            Cause::DiskFull,
        ] {
            assert_eq!(way(wait, true, 0).0, Path::Wait);
        }
        assert_eq!(way(Cause::Stopped, true, 0).0, Path::Final);
    }

    #[test]
    fn the_disk_wait_clears_at_the_floor() {
        let floor = 1024 * 1024 * 1024;
        assert!(!disk_cleared(floor - 1, floor));
        assert!(disk_cleared(floor, floor));
        assert!(disk_cleared(floor * 2, floor));
    }

    #[test]
    fn an_attempt_names_its_interruption_and_no_other() {
        // The keys of one execution's interruptions differ, whatever their
        // causes (a model wait, a database outage, a host restart).
        let keys: Vec<String> = (1..=3).map(|attempt| key("exec_7", attempt)).collect();
        assert_eq!(
            keys,
            ["recover-exec_7-1", "recover-exec_7-2", "recover-exec_7-3"]
        );
        assert_ne!(key("exec_7", 1), key("exec_8", 1));
        assert_ne!(key("exec_7", 1), key("exec_77", 1));
    }

    fn execution(ended: Option<i64>) -> Execution {
        Execution {
            id: "exec_7".to_string(),
            started_at_ms: 1_000,
            workspace: None,
            terminal_server: None,
            state: match ended {
                None => ExecutionState::Running,
                Some(ended_at_ms) => ExecutionState::Ended(End {
                    event_id: 90,
                    ended_at_ms,
                    cause: RecordedEndCause::Exited,
                    exit_code: Some(0),
                    signal: None,
                    evidence: None,
                }),
            },
        }
    }

    fn recorded_interrupted(
        schedule: RecoverySchedule,
        attempt: u32,
        fact: i64,
        event_id: i64,
    ) -> Recorded {
        Recorded::Interrupted(Interrupted {
            event_id,
            at_ms: 1_000,
            cause: Cause::Crashed,
            schedule: RecordedSchedule::Explicit(schedule),
            attempt,
            fact_event_id: fact,
            evidence: Value::Null,
        })
    }

    /// The clear of an interruption: what shows its cause gone, recorded after it.
    fn cleared_after(_wait: &Recorded, event_id: i64) -> Recorded {
        Recorded::Cleared {
            event_id,
            at_ms: 1_400,
        }
    }

    /// A restarted loop reads the same records and finds nothing to record
    /// twice: the fact behind the latest interruption is explained, and the
    /// next one is not.
    #[test]
    fn a_restarted_loop_records_nothing_twice() {
        let execution = execution(Some(2_000));
        let turns = vec![
            Turn {
                event_id: 50,
                ..turn(Some(529), Some("overloaded"), "API Error: 529")
            },
            Turn {
                event_id: 60,
                at_ms: 1_500,
                ..turn(Some(529), Some("overloaded"), "API Error: 529")
            },
        ];
        // Nothing recorded: the first failed turn is the next fact.
        match pending(&[], &turns, &[], &execution, &[], &[]) {
            Some(super::Fact::Turn { turn, cause }) => {
                assert_eq!(turn.event_id, 50);
                assert_eq!(cause, Cause::ModelUnavailable);
            }
            _ => panic!("the first failed turn"),
        }
        // That turn recorded: the second one.
        let first = recorded_interrupted(RecoverySchedule::Wait { due_ms: 1_000 }, 1, 50, 10);
        match pending(
            std::slice::from_ref(&first),
            &turns,
            &[],
            &execution,
            &[],
            &[],
        ) {
            Some(super::Fact::Turn { turn, cause }) => {
                assert_eq!(turn.event_id, 60);
                assert_eq!(cause, Cause::ModelUnavailable);
            }
            _ => panic!("the second failed turn"),
        }
        // Both, their wait cleared, and the end after them.
        let second = recorded_interrupted(RecoverySchedule::Wait { due_ms: 1_000 }, 2, 60, 11);
        let clear = cleared_after(&second, 12);
        match pending(
            &[first.clone(), second.clone(), clear.clone()],
            &turns,
            &[],
            &execution,
            &[],
            &[],
        ) {
            Some(super::Fact::End(end)) => assert_eq!(end.event_id, 90),
            _ => panic!("the end"),
        }
        // An open wait explains the end: its revival resumes it.
        assert!(
            pending(
                &[first.clone(), second.clone()],
                &turns,
                &[],
                &execution,
                &[],
                &[]
            )
            .is_none(),
            "an open wait's end is its revival's, not a new interruption"
        );
        // The end recorded: nothing, however often the loop reads it.
        let ended = recorded_interrupted(RecoverySchedule::Resume { due_ms: 1_000 }, 3, 90, 13);
        assert!(
            pending(
                &[first, second, clear, ended],
                &turns,
                &[],
                &execution,
                &[],
                &[]
            )
            .is_none(),
            "a restarted loop records the same interruption once"
        );
    }

    /// A full disk's tool failure is an interruption of its own: the engine
    /// ends the turn with Claude Code's ENOSPC message and waits at its
    /// prompt, and no failed turn or end says so. Any other failed tool is
    /// the work's, and no fact at all.
    #[test]
    fn a_full_disks_tool_failure_is_a_fact_and_the_earliest_one_wins() {
        let full = ToolFailure {
            event_id: 55,
            at_ms: 1_000,
            text: "Disk full: a test write failed with ENOSPC (the volume, or the user's quota \
                   on it, has no space left)"
                .to_string(),
        };
        assert_eq!(full.cause(), Some(Cause::DiskFull));
        let other = ToolFailure {
            event_id: 56,
            at_ms: 1_100,
            text: "exit 1".to_string(),
        };
        assert_eq!(other.cause(), None, "a failed tool is the work's");
        let execution = execution(Some(2_000));
        // The disk's failure is the next fact, ahead of the end at 90.
        match pending(&[], &[], std::slice::from_ref(&full), &execution, &[], &[]) {
            Some(super::Fact::Tool { tool, cause }) => {
                assert_eq!(tool.event_id, 55);
                assert_eq!(cause, Cause::DiskFull);
            }
            _ => panic!("the disk's tool failure"),
        }
        // An ordinary failed tool explains nothing: the end is next.
        match pending(&[], &[], std::slice::from_ref(&other), &execution, &[], &[]) {
            Some(super::Fact::End(end)) => assert_eq!(end.event_id, 90),
            _ => panic!("the end"),
        }
        // The first of a failed turn and a disk failure in the log is the
        // fact, whichever it is.
        let earlier = Turn {
            event_id: 50,
            ..turn(Some(529), Some("overloaded"), "API Error: 529")
        };
        assert!(matches!(
            pending(
                &[],
                std::slice::from_ref(&earlier),
                std::slice::from_ref(&full),
                &execution,
                &[],
                &[]
            ),
            Some(super::Fact::Turn { .. })
        ));
        let later = Turn {
            event_id: 60,
            ..turn(Some(529), Some("overloaded"), "API Error: 529")
        };
        assert!(matches!(
            pending(
                &[],
                std::slice::from_ref(&later),
                std::slice::from_ref(&full),
                &execution,
                &[],
                &[]
            ),
            Some(super::Fact::Tool { .. })
        ));
    }

    /// While a disk_full wait is open the free space is read again every
    /// `SPAWN_RECOVER_PROBE_SECS` (the soak's 2 s), not once a minute: the
    /// loop's next pass is due at the probe, and the clear is that reading.
    #[test]
    fn a_disk_wait_is_re_read_every_probe_not_once_a_minute() {
        let mut state = Recovery::new();
        state.open_waits = true;
        let now = 1_000_000;
        assert_eq!(
            state.next_due(2_000, now),
            Some(now + 2_000),
            "the wait's cause is re-read at the probe"
        );
    }

    #[test]
    fn an_open_wait_explains_its_execution_until_a_person_stops_it() {
        let execution = execution(Some(2_000));
        let waiting = recorded_interrupted(RecoverySchedule::Wait { due_ms: 1_000 }, 1, 50, 10);
        // The wait is open: the end is its revival's, not a new interruption.
        assert!(
            pending(
                std::slice::from_ref(&waiting),
                &[],
                &[],
                &execution,
                &[],
                &[]
            )
            .is_none()
        );
        // A person's Stop since: it is.
        let stopped = stop("bb-stop-3", 7);
        assert!(matches!(
            pending(
                std::slice::from_ref(&waiting),
                &[],
                &[],
                &execution,
                &[stopped],
                &[]
            ),
            Some(super::Fact::End(_))
        ));
        // A handoff's own Stop explains the end it made.
        let handoff = recorded_interrupted(RecoverySchedule::Handoff { due_ms: 1_000 }, 1, 50, 10);
        let sent = Sent {
            key: "recover-exec_7-1-stop".to_string(),
            created_at_ms: 1,
            outcome: SentOutcome::Succeeded { finished_at_ms: 2 },
        };
        assert!(
            pending(&[handoff], &[], &[], &execution, &[], &[sent]).is_none(),
            "the handoff's stop is no interruption of its own"
        );
    }

    #[test]
    fn a_pass_that_only_could_not_reach_its_database_counts_toward_nothing() {
        let dark = vec![format!(
            "{}: connection closed",
            spawn_core::port::UNREACHABLE
        )];
        assert!(!counts_as_failure(&dark));
        let mixed = vec![
            dark[0].clone(),
            "storage backend failure: permission denied (42501)".to_string(),
        ];
        assert!(counts_as_failure(&mixed));
        assert!(counts_as_failure(&[
            "storage backend failure: something".to_string()
        ]));
    }

    /// An expired login's periodic retry types only into a live execution;
    /// native-session and failed-resume bounds apply only once it has ended.
    #[test]
    fn a_live_model_auth_retry_is_an_input() {
        for native_bound in [false, true] {
            for failed_resumes in [0, super::RESUME_ATTEMPTS] {
                assert_eq!(
                    auth_retry_action(true, native_bound, failed_resumes),
                    AuthRetry::Input
                );
            }
        }
    }

    /// A stopped execution with a native session is resumed, rather than
    /// sent input that its supervisor must reject, until the resumes are spent.
    #[test]
    fn a_stopped_resumable_model_auth_retry_is_a_resume() {
        for failed_resumes in 0..super::RESUME_ATTEMPTS {
            assert_eq!(
                auth_retry_action(false, true, failed_resumes),
                AuthRetry::Resume,
                "{failed_resumes} failed resumes"
            );
        }
    }

    /// With no native session to resume, or after the usual resume bound, an
    /// expired login's stopped execution follows the handoff path instead.
    #[test]
    fn a_stopped_non_resumable_model_auth_retry_hands_off() {
        for failed_resumes in 0..=super::RESUME_ATTEMPTS {
            assert_eq!(
                auth_retry_action(false, false, failed_resumes),
                AuthRetry::Handoff,
                "no native session, {failed_resumes} failed resumes"
            );
        }
        assert_eq!(
            auth_retry_action(false, true, super::RESUME_ATTEMPTS),
            AuthRetry::Handoff
        );
    }

    fn assert_start_action(
        planned: PlannedCommand,
        expected_key: &str,
        expected_prompt: &str,
        expected_charter: Option<&str>,
    ) {
        assert_eq!(planned.key, expected_key);
        match planned.action {
            CommandAction::Start {
                engine,
                prompt,
                parent_session_id,
                from,
                model,
                charter,
                model_label: None,
            } => {
                assert_eq!(engine, ENGINE);
                assert_eq!(prompt, expected_prompt);
                assert_eq!(parent_session_id, None);
                assert_eq!(from, None);
                assert_eq!(model, None);
                assert_eq!(charter.as_deref(), expected_charter);
            }
            other => panic!("expected CommandAction::Start for {expected_key}, got {other:?}"),
        }
    }

    fn auth_wait_interrupted() -> Interrupted {
        Interrupted {
            event_id: 1,
            at_ms: 1_000,
            cause: Cause::ModelAuth,
            schedule: RecordedSchedule::Explicit(RecoverySchedule::Wait { due_ms: 1_801_000 }),
            attempt: 1,
            fact_event_id: 0,
            evidence: Value::Null,
        }
    }

    /// A failed `Start` from `AuthRetry::Handoff` with no native session must
    /// wait its resume backoff and then retry under `-r1` as `CommandAction::Start`
    /// (preserving the handoff prompt and charter), never `CommandAction::Resume`.
    #[test]
    fn a_failed_model_auth_handoff_without_native_session_retries_as_start() {
        let config = Config::default();
        let last = auth_wait_interrupted();
        let commands = vec![Sent {
            key: "recover-exec_7-1".to_string(),
            created_at_ms: 1_801_000,
            outcome: SentOutcome::Failed {
                finished_at_ms: 1_802_000,
            },
        }];
        let expected_due = resume_due(config.resume_backoff, 1, Some(1_802_000), 1_810_000);

        let early_env = RevivalEnv {
            execution_id: "exec_7",
            commands: &commands,
            config: &config,
            now_ms: expected_due - 1,
            live: false,
        };
        assert!(matches!(
            plan_auth_retry(&early_env, &last, false, 0, || {
                ("handoff prompt".to_string(), Some("charter-1".to_string()))
            }),
            RevivalPlan::HoldUntil(due) if due == expected_due
        ));

        let due_env = RevivalEnv {
            execution_id: "exec_7",
            commands: &commands,
            config: &config,
            now_ms: expected_due,
            live: false,
        };
        match plan_auth_retry(&due_env, &last, false, 0, || {
            ("handoff prompt".to_string(), Some("charter-1".to_string()))
        }) {
            RevivalPlan::Submit(planned) => assert_start_action(
                *planned,
                "recover-exec_7-1-r1",
                "handoff prompt",
                Some("charter-1"),
            ),
            other => panic!("failed handoff start should emit -r1 once due, got {other:?}"),
        }
    }

    /// A failed `Start` from `AuthRetry::Handoff` after resumes were already spent
    /// must also retry as `CommandAction::Start`, never returning to the unrecoverable
    /// native session with `CommandAction::Resume`.
    #[test]
    fn a_failed_model_auth_handoff_after_spent_resumes_retries_as_start() {
        let config = Config::default();
        let last = auth_wait_interrupted();
        let commands = vec![Sent {
            key: "recover-exec_7-1".to_string(),
            created_at_ms: 1_801_000,
            outcome: SentOutcome::Failed {
                finished_at_ms: 1_802_000,
            },
        }];
        let expected_due = resume_due(config.resume_backoff, 1, Some(1_802_000), 1_802_000);
        let env = RevivalEnv {
            execution_id: "exec_7",
            commands: &commands,
            config: &config,
            now_ms: expected_due,
            live: false,
        };
        match plan_auth_retry(&env, &last, true, super::RESUME_ATTEMPTS, || {
            (
                "spent-resumes handoff".to_string(),
                Some("charter-2".to_string()),
            )
        }) {
            RevivalPlan::Submit(planned) => assert_start_action(
                *planned,
                "recover-exec_7-1-r1",
                "spent-resumes handoff",
                Some("charter-2"),
            ),
            other => panic!("spent-resumes failed handoff start should emit -r1, got {other:?}"),
        }
    }

    /// While `-r1` is still in flight or already succeeded, no `-r2` is emitted;
    /// once `-r1` also fails, `-r2` backs off from `-r1` and emits `Start`.
    #[test]
    fn a_pending_or_succeeded_handoff_retry_emits_nothing_and_a_failed_r1_schedules_r2_as_start() {
        let config = Config::default();
        let last = auth_wait_interrupted();
        let base_failed = Sent {
            key: "recover-exec_7-1".to_string(),
            created_at_ms: 1_801_000,
            outcome: SentOutcome::Failed {
                finished_at_ms: 1_802_000,
            },
        };
        for outcome in [
            SentOutcome::Pending,
            SentOutcome::Cancelled {
                finished_at_ms: 1_825_000,
            },
            SentOutcome::Succeeded {
                finished_at_ms: 1_825_000,
            },
        ] {
            let commands = vec![
                base_failed.clone(),
                Sent {
                    key: "recover-exec_7-1-r1".to_string(),
                    created_at_ms: 1_822_000,
                    outcome,
                },
            ];
            let env = RevivalEnv {
                execution_id: "exec_7",
                commands: &commands,
                config: &config,
                now_ms: 2_000_000,
                live: false,
            };
            assert!(
                matches!(
                    plan_auth_retry(&env, &last, false, 0, || { ("unused".to_string(), None) }),
                    RevivalPlan::Idle
                ),
                "retry outcome {outcome:?} must not emit -r2"
            );
        }

        let failed_r1 = vec![
            base_failed,
            Sent {
                key: "recover-exec_7-1-r1".to_string(),
                created_at_ms: 1_822_000,
                outcome: SentOutcome::Failed {
                    finished_at_ms: 1_825_000,
                },
            },
        ];
        let due_r2 = resume_due(config.resume_backoff, 2, Some(1_825_000), 1_825_000);
        let early_env = RevivalEnv {
            execution_id: "exec_7",
            commands: &failed_r1,
            config: &config,
            now_ms: due_r2 - 1,
            live: false,
        };
        assert!(matches!(
            plan_auth_retry(&early_env, &last, false, 0, || {
                ("second retry".to_string(), Some("charter-3".to_string()))
            }),
            RevivalPlan::HoldUntil(due) if due == due_r2
        ));

        let due_env = RevivalEnv {
            execution_id: "exec_7",
            commands: &failed_r1,
            config: &config,
            now_ms: due_r2,
            live: false,
        };
        match plan_auth_retry(&due_env, &last, false, 0, || {
            ("second retry".to_string(), Some("charter-3".to_string()))
        }) {
            RevivalPlan::Submit(planned) => assert_start_action(
                *planned,
                "recover-exec_7-1-r2",
                "second retry",
                Some("charter-3"),
            ),
            other => panic!("failed -r1 should schedule -r2, got {other:?}"),
        }
    }

    /// An expired login is retried only on its slow schedule — never while
    /// the credentials file is unchanged — and a changed file is the clear.
    #[test]
    fn a_login_waits_for_the_credentials_to_change_or_the_slow_retry() {
        let config = Config::default();
        // The first retry waits the slow interval from the interruption, and
        // each later one the same interval from the last attempt.
        assert_eq!(
            auth_retry_due(&config, 1_000, None),
            1_000 + config.auth_retry
        );
        assert_eq!(
            auth_retry_due(&config, 1_000, Some(9_000)),
            9_000 + config.auth_retry
        );
        assert!(auth_retry_due(&config, 1_000, Some(9_000)) > 9_000);
        // The credentials file as the loop read it: unchanged is no
        // observation, a renewed one is.
        let path = std::env::temp_dir().join(format!(
            "spawn-recover-credentials-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.subsec_nanos())
        ));
        let text = path.to_string_lossy().into_owned();
        std::fs::write(&path, b"{}").expect("write the credentials");
        let before = Credentials::read(&text).expect("read them");
        assert_eq!(
            Credentials::read(&text).as_ref(),
            Some(&before),
            "an unchanged credentials file is no observation"
        );
        std::fs::write(&path, b"{\"claudeAiOauth\":{\"accessToken\":\"new\"}}")
            .expect("renew them");
        let renewed = Credentials::read(&text).expect("read the new one");
        assert_ne!(before, renewed, "a renewed file clears the wait");
        assert_eq!(renewed.path, text);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_settings_read_the_contracts_defaults_and_the_environment() {
        let defaults = Config::default();
        assert_eq!(defaults.resume_backoff, 10_000);
        assert_eq!(defaults.model_backoff, 60_000);
        assert_eq!(defaults.auth_retry, 1_800_000);
        assert_eq!(defaults.probe, 10_000);
        assert_eq!(defaults.model_retries, 3);
    }

    /// Objective 39: SPAWN_RECOVER_MODEL_RETRIES is a whole number from 1
    /// to 20, 3 when unset; anything else is refused, naming the setting.
    #[test]
    fn the_model_retries_setting_is_a_whole_number_from_1_to_20() {
        assert_eq!(model_retries(None), Ok(3));
        assert_eq!(model_retries(Some("1")), Ok(1));
        assert_eq!(model_retries(Some("2")), Ok(2));
        assert_eq!(model_retries(Some(" 20 ")), Ok(20));
        for bad in ["0", "21", "-1", "two", "2.5", ""] {
            let refused = model_retries(Some(bad)).expect_err(bad);
            assert!(
                refused.contains("SPAWN_RECOVER_MODEL_RETRIES"),
                "{bad}: {refused}"
            );
        }
    }

    /// Objective 39: a request the model refuses for good (neither
    /// transient, nor a login, nor an exhausted context) is a model error;
    /// the engine's own end after it, with no progress since, is
    /// `model_error` on the final path, below a person's Stop.
    #[test]
    fn an_end_after_a_refused_request_is_a_final_model_error() {
        let invalid = turn(
            Some(400),
            Some("invalid_request"),
            "API Error: 400 unexpected tool_use_id found in tool_result blocks",
        );
        assert!(invalid.model_error());
        assert!(turn(Some(404), None, "API Error: 404 model not found").model_error());
        assert!(turn(Some(413), None, "API Error: 413 request too large").model_error());
        for transient in [
            turn(Some(529), Some("overloaded"), "API Error: 529"),
            turn(Some(500), None, "API Error: 500"),
            turn(None, Some("rate_limit"), "API Error: rate limited"),
            turn(Some(401), Some("authentication_failed"), "API Error: 401"),
            turn(Some(400), None, "Prompt is too long"),
        ] {
            assert!(!transient.model_error(), "{transient:?}");
        }
        assert_eq!(Cause::ModelError.name(), "model_error");
        assert_eq!(Cause::ModelError.path(), Path::Final);
        assert_eq!(
            serde_json::from_value::<Cause>(json!("model_error")).expect("cause deserialize"),
            Cause::ModelError
        );

        // The latest failed turn before the end, with no progress after it.
        let turns = [invalid.clone()];
        assert_eq!(refused(&turns, 0, 0, Some(90)), Some(&invalid));
        assert_eq!(refused(&turns, 49, 0, Some(90)), Some(&invalid));
        assert_eq!(refused(&turns, 60, 0, None), None, "progress after it");
        assert_eq!(refused(&turns, 0, 50, None), None, "already recorded");
        assert_eq!(refused(&turns, 0, 0, Some(40)), None, "after the end");
        let later = Turn {
            event_id: 70,
            ..turn(Some(529), Some("overloaded"), "API Error: 529")
        };
        assert_eq!(
            refused(&[invalid.clone(), later], 0, 0, None),
            None,
            "a transient failure after it is the latest"
        );

        let nonzero = end(EndCause::Exited, Some(1), None);
        let after_refusal = Facts {
            model_error: Some(&invalid),
            ..facts(Some(&nonzero))
        };
        assert_eq!(classify(&after_refusal), Cause::ModelError);
        let clean = end(EndCause::Exited, Some(0), None);
        assert_eq!(
            classify(&Facts {
                model_error: Some(&invalid),
                ..facts(Some(&clean))
            }),
            Cause::ModelError
        );
        let stopped = stop("bb-stop-1", 7);
        assert_eq!(
            classify(&Facts {
                stop: Some(StopFact {
                    stop: &stopped,
                    kind: StopKind::Other,
                }),
                ..after_refusal
            }),
            Cause::Stopped,
            "a person's Stop outranks every model cause"
        );
        assert_eq!(classify(&facts(Some(&nonzero))), Cause::Crashed);
        let (path, _) = way(Cause::ModelError, true, 0);
        assert_eq!(path, Path::Final);
    }

    fn executed(id: &str, key: &str, progress: bool, last_cause: Option<Cause>) -> Executed {
        Executed {
            id: id.to_string(),
            started_at_ms: 0,
            ended_at_ms: Some(5_000),
            progress,
            key: key.to_string(),
            last_cause,
        }
    }

    /// Objective 39: the resumes in a row of an execution that ended on a
    /// transient model error are counted back from the current execution
    /// across the session's executions; a revived execution that made
    /// progress, or one a person started, ends the run.
    #[test]
    fn model_resumes_count_the_run_of_revivals_after_transient_ends() {
        let unavailable = Some(Cause::ModelUnavailable);
        let chain = [
            executed("exec_1", "dispatch-task-7", false, unavailable),
            executed("exec_2", "recover-exec_1-1", false, unavailable),
            executed("exec_3", "recover-exec_2-1", false, unavailable),
        ];
        assert_eq!(model_resumes(&chain, "exec_1"), 0);
        assert_eq!(model_resumes(&chain, "exec_2"), 1);
        assert_eq!(model_resumes(&chain, "exec_3"), 2);
        assert_eq!(model_resumes(&chain, "exec_9"), 0, "an unknown execution");
        // A retry of a failed revival is a revival too.
        let retried = [
            executed("exec_1", "dispatch-task-7", false, unavailable),
            executed("exec_2", "recover-exec_1-1-r1", false, unavailable),
        ];
        assert_eq!(model_resumes(&retried, "exec_2"), 1);
        // A revived execution that made progress resets the count.
        let answered = [
            executed("exec_1", "dispatch-task-7", false, unavailable),
            executed("exec_2", "recover-exec_1-1", true, unavailable),
            executed("exec_3", "recover-exec_2-1", false, unavailable),
        ];
        assert_eq!(model_resumes(&answered, "exec_3"), 1);
        assert_eq!(model_resumes(&answered, "exec_2"), 0);
        // A resume by a person starts a new run.
        let by_person = [
            executed("exec_1", "recover-exec_0-1", false, unavailable),
            executed("exec_2", "bb-resume-1", false, unavailable),
            executed("exec_3", "recover-exec_2-1", false, unavailable),
        ];
        assert_eq!(model_resumes(&by_person, "exec_3"), 1);
        // A crash's resume is no model retry.
        let crashed = [
            executed("exec_1", "dispatch-task-7", false, Some(Cause::Crashed)),
            executed("exec_2", "recover-exec_1-1", false, unavailable),
        ];
        assert_eq!(model_resumes(&crashed, "exec_2"), 0);
    }

    #[test]
    fn a_resume_counts_as_failed_only_when_it_died_young_without_progress() {
        let young = Executed {
            id: "exec_1".to_string(),
            started_at_ms: 1_000,
            ended_at_ms: Some(4_000),
            progress: false,
            key: "recover-exec_1-1".to_string(),
            last_cause: None,
        };
        assert!(resume_failed(
            young.ended_at_ms.map(|ended| ended - young.started_at_ms),
            young.progress
        ));
        let worked = Executed {
            progress: true,
            ..young
        };
        assert!(!resume_failed(
            worked.ended_at_ms.map(|ended| ended - worked.started_at_ms),
            worked.progress
        ));
    }

    /// Mission rows with task 7 of `session_worker_a_7` delivered and
    /// restarted once, and task 8 of `session_worker_b_8` reassigned as
    /// task 9 of `session_worker_b_9`.
    fn moved_work() -> crate::deliver::Work {
        let row = |id: i64, session: &str, state: &str| -> crate::deliver::TaskRow {
            serde_json::from_value(json!({
                "id": id, "mission_id": 1, "kind": "ephemeral",
                "agent_id": session.replace("session_", "agent_"), "session_id": session,
                "brief": "b", "state": state,
            }))
            .expect("a task row")
        };
        let mut restarted = row(7, "session_worker_a_7", "delivered");
        restarted.restart = Some(crate::deliver::Restart {
            n: 1,
            execution_id: Some("exec_a".to_string()),
            by: Some("human".to_string()),
            reason: None,
        });
        let mut old = row(8, "session_worker_b_8", "cancelled");
        old.reassigned_as = Some(9);
        old.reassigned_to_session = Some("session_worker_b_9".to_string());
        crate::deliver::Work {
            tasks: vec![restarted, old, row(9, "session_worker_b_9", "assigned")],
            ..crate::deliver::Work::default()
        }
    }

    fn dispatcher_stop(key: &str, command_id: i64) -> Stop {
        Stop {
            principal: "spawn-dispatch".to_string(),
            ..stop(key, command_id)
        }
    }

    /// The cause `restarted` or `reassigned` comes from the record the
    /// dispatcher's own Stop names, never from a key alone: the same key
    /// from anyone else, or one no record gives, is `stopped`.
    #[test]
    fn a_restart_or_reassign_is_told_from_its_record_and_the_dispatchers_stop() {
        let work = moved_work();
        let assert_fact = |stops: &[Stop], session: &str, expected: &Stop, kind: StopKind| {
            assert_eq!(
                stop_fact(stops, session, &work),
                Some(StopFact {
                    stop: expected,
                    kind,
                })
            );
        };
        let restart = dispatcher_stop("restart-7-1-stop", 40);
        assert_fact(
            std::slice::from_ref(&restart),
            "session_worker_a_7",
            &restart,
            StopKind::Restarted { task: 7, n: 1 },
        );
        let reassign = dispatcher_stop("reassign-8-stop", 41);
        let expected_reassigned = StopKind::Reassigned {
            task: 8,
            to_task: 9,
            to_session: Some("session_worker_b_9".to_string()),
        };
        assert_fact(
            std::slice::from_ref(&reassign),
            "session_worker_b_8",
            &reassign,
            expected_reassigned.clone(),
        );
        // A retry of the dispatcher's Stop carries its purpose.
        let retry = dispatcher_stop("stop-exec_b-retry-1", 42);
        assert_fact(
            &[reassign.clone(), retry.clone()],
            "session_worker_b_8",
            &retry,
            expected_reassigned,
        );
        // A person's Stop with the dispatcher's key is a person's.
        let fake = stop("restart-7-1-stop", 43);
        assert_fact(
            std::slice::from_ref(&fake),
            "session_worker_a_7",
            &fake,
            StopKind::Other,
        );
        // A restart no record gives (another n), or another session's.
        let wrong_n = dispatcher_stop("restart-7-2-stop", 44);
        assert_fact(
            std::slice::from_ref(&wrong_n),
            "session_worker_a_7",
            &wrong_n,
            StopKind::Other,
        );
        assert_fact(
            std::slice::from_ref(&restart),
            "session_worker_b_8",
            &restart,
            StopKind::Other,
        );
        // A later person's Stop is the one that ended it.
        let later_person = stop("bb-stop", 45);
        assert_fact(
            &[restart.clone(), later_person.clone()],
            "session_worker_a_7",
            &later_person,
            StopKind::Other,
        );

        let ended = end(EndCause::Stopped, None, None);
        let told = Facts {
            stop: stop_fact(std::slice::from_ref(&restart), "session_worker_a_7", &work),
            ..facts(Some(&ended))
        };
        assert_eq!(classify(&told), Cause::Restarted);
        assert_eq!(way(Cause::Restarted, true, 0).0, Path::Final);
        let reassigned = Facts {
            stop: stop_fact(std::slice::from_ref(&reassign), "session_worker_b_8", &work),
            ..facts(Some(&ended))
        };
        assert_eq!(classify(&reassigned), Cause::Reassigned);
        assert_eq!(way(Cause::Reassigned, true, 0).0, Path::Final);
        assert_eq!(
            classify(&Facts {
                stop: Some(StopFact {
                    stop: &restart,
                    kind: StopKind::Other,
                }),
                ..facts(Some(&ended))
            }),
            Cause::Stopped,
            "without the record it is a Stop like any other"
        );
        assert_eq!(
            serde_json::from_value::<Cause>(json!("restarted")).expect("restarted cause"),
            Cause::Restarted
        );
        assert_eq!(
            serde_json::from_value::<Cause>(json!("reassigned")).expect("reassigned cause"),
            Cause::Reassigned
        );
    }

    /// The recovery keeps a reassigned ephemeral worker (to record its end)
    /// but never revives it.
    #[test]
    fn a_reassigned_worker_is_kept_to_record_its_end_and_never_revived() {
        let work = moved_work();
        let none = std::collections::BTreeMap::new();
        assert!(super::kept(
            "agent_worker_b_8",
            "session_worker_b_8",
            &work,
            &none
        ));
        assert!(super::reassigned_away("session_worker_b_8", &work));
        assert!(!super::reassigned_away("session_worker_a_7", &work));
        assert!(super::kept(
            "agent_worker_a_7",
            "session_worker_a_7",
            &work,
            &none
        ));
        assert!(!super::kept(
            "agent_worker_b_9",
            "session_worker_b_9",
            &work,
            &none
        ));
    }

    #[test]
    fn execution_boundary_rejects_impossible_states_and_facts_row_fails_closed() {
        for end_cause in ["exited", "stopped", "lost"] {
            let parsed: Execution = serde_json::from_value(json!({
                "id": "exec_1",
                "started_at_ms": 1_000,
                "ended_at_ms": 2_000,
                "cause": end_cause,
                "exit_code": 1,
                "signal": 9,
                "evidence": "pane gone",
                "end_event_id": 42,
            }))
            .expect("valid ended execution");
            assert!(!parsed.is_live());
            assert_eq!(parsed.end().expect("ended").event_id, 42);
        }

        // Running execution with any execution.ended field is rejected.
        for bad in [
            json!({"id": "exec_1", "started_at_ms": 1_000, "cause": "exited"}),
            json!({"id": "exec_1", "started_at_ms": 1_000, "exit_code": 0}),
            json!({"id": "exec_1", "started_at_ms": 1_000, "signal": 9}),
            json!({"id": "exec_1", "started_at_ms": 1_000, "evidence": "lost"}),
            json!({"id": "exec_1", "started_at_ms": 1_000, "end_event_id": 90}),
            // Ended execution missing cause, unknown cause, or non-positive/missing end_event_id.
            json!({"id": "exec_1", "started_at_ms": 1_000, "ended_at_ms": 2_000, "end_event_id": 90}),
            json!({"id": "exec_1", "started_at_ms": 1_000, "ended_at_ms": 2_000, "cause": "bogus", "end_event_id": 90}),
            json!({"id": "exec_1", "started_at_ms": 1_000, "ended_at_ms": 2_000, "cause": "exited"}),
            json!({"id": "exec_1", "started_at_ms": 1_000, "ended_at_ms": 2_000, "cause": "exited", "end_event_id": 0}),
        ] {
            assert!(
                serde_json::from_value::<Execution>(bad.clone()).is_err(),
                "expected illegal execution rejected: {bad}"
            );
        }

        // FactsRow with execution: null succeeds (never ran); malformed execution fails closed.
        let never_ran: FactsRow = serde_json::from_value(json!({
            "session_id": "session_worker_1",
            "agent_id": "agent_worker_1",
            "execution": null,
        }))
        .expect("null execution is legal");
        assert!(never_ran.execution.is_none());

        let malformed_row = serde_json::from_value::<FactsRow>(json!({
            "session_id": "session_worker_1",
            "agent_id": "agent_worker_1",
            "execution": {"id": "exec_1", "started_at_ms": 1_000, "ended_at_ms": 2_000},
        }));
        assert!(
            malformed_row.is_err(),
            "malformed execution in FactsRow must fail closed instead of swallowing"
        );
    }

    #[test]
    fn recorded_boundary_enforces_schedule_and_rejects_illegal_combinations() {
        for (path, due_ms, expected) in [
            (
                "resume",
                json!(1_000),
                RecoverySchedule::Resume { due_ms: 1_000 },
            ),
            (
                "handoff",
                json!(2_000),
                RecoverySchedule::Handoff { due_ms: 2_000 },
            ),
            (
                "wait",
                json!(3_000),
                RecoverySchedule::Wait { due_ms: 3_000 },
            ),
            ("final", Value::Null, RecoverySchedule::Final),
        ] {
            let parsed: Recorded = serde_json::from_value(json!({
                "event_id": 10,
                "at_ms": 500,
                "record": {
                    "type": "execution.interrupted",
                    "cause": "crashed",
                    "path": path,
                    "attempt": 1,
                    "due_ms": due_ms,
                    "fact_event_id": 7,
                    "evidence": {},
                }
            }))
            .expect("valid schedule");
            match parsed {
                Recorded::Interrupted(interrupted) => {
                    assert_eq!(interrupted.schedule, RecordedSchedule::Explicit(expected));
                    assert_eq!(interrupted.explicit_path(), Some(expected.path()));
                    assert_eq!(interrupted.effective_path(), expected.path());
                }
                Recorded::Cleared { .. } => panic!("expected Interrupted"),
            }
        }

        let cleared: Recorded = serde_json::from_value(json!({
            "event_id": 11,
            "at_ms": 600,
            "record": {"type": "execution.cleared", "cause": "disk_full", "waited_ms": 100}
        }))
        .expect("valid cleared record");
        assert_eq!(
            cleared,
            Recorded::Cleared {
                event_id: 11,
                at_ms: 600
            }
        );

        for bad_record in [
            json!({"type": "execution.interrupted", "cause": "stopped", "path": "final", "attempt": 1, "due_ms": 1_000}),
            json!({"type": "execution.interrupted", "cause": "crashed", "path": "resume", "attempt": 1, "due_ms": "soon"}),
            json!({"type": "execution.interrupted", "cause": "crashed", "path": "resume", "attempt": 0, "due_ms": 1_000}),
            json!({"type": "execution.interrupted", "cause": "unknown_cause", "path": "final", "attempt": 1}),
            json!({"type": "execution.interrupted", "cause": "crashed", "path": "loop_forever", "attempt": 1, "due_ms": 1_000}),
            json!({"type": "host.disk", "state": "low"}),
            json!({"cause": "crashed"}),
            json!(42),
        ] {
            let err = serde_json::from_value::<Recorded>(json!({
                "event_id": 1,
                "at_ms": 1_000,
                "record": bad_record,
            }));
            assert!(
                err.is_err(),
                "expected illegal recovery record rejected: {bad_record}"
            );
        }
    }

    #[test]
    fn sent_boundary_enforces_outcome_invariants() {
        let pending: Sent = serde_json::from_value(json!({
            "key": "recover-exec_1-1",
            "created_at_ms": 100,
            "finished_at_ms": null,
            "succeeded": null,
        }))
        .expect("pending sent");
        assert_eq!(pending.outcome, SentOutcome::Pending);

        let cancelled: Sent = serde_json::from_value(json!({
            "key": "recover-exec_1-1",
            "created_at_ms": 100,
            "finished_at_ms": 200,
            "succeeded": null,
        }))
        .expect("cancelled sent");
        assert_eq!(
            cancelled.outcome,
            SentOutcome::Cancelled {
                finished_at_ms: 200
            }
        );

        for (ok, expected) in [
            (
                true,
                SentOutcome::Succeeded {
                    finished_at_ms: 300,
                },
            ),
            (
                false,
                SentOutcome::Failed {
                    finished_at_ms: 300,
                },
            ),
        ] {
            let parsed: Sent = serde_json::from_value(json!({
                "key": "recover-exec_1-1",
                "created_at_ms": 100,
                "finished_at_ms": 300,
                "succeeded": ok,
            }))
            .expect("finished sent");
            assert_eq!(parsed.outcome, expected);
        }

        for succeeded in [true, false] {
            assert!(
                serde_json::from_value::<Sent>(json!({
                    "key": "recover-exec_1-1",
                    "created_at_ms": 100,
                    "finished_at_ms": null,
                    "succeeded": succeeded,
                }))
                .is_err()
            );
        }
    }

    #[test]
    fn executed_stop_action_and_loop_start_enforce_invariants() {
        // Executed: legal running, legal ended with last_cause, reject duration without ended_at_ms & invalid cause.
        let ok_exec: Executed = serde_json::from_value(json!({
            "id": "exec_1",
            "started_at_ms": 100,
            "ended_at_ms": 500,
            "duration_ms": 400,
            "progress": true,
            "key": "recover-exec_0-1",
            "last_cause": "model_unavailable",
        }))
        .expect("valid executed");
        assert_eq!(ok_exec.last_cause, Some(Cause::ModelUnavailable));
        assert!(
            serde_json::from_value::<Executed>(json!({
                "id": "exec_1",
                "started_at_ms": 100,
                "ended_at_ms": null,
                "duration_ms": 400,
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<Executed>(json!({
                "id": "exec_1",
                "started_at_ms": 100,
                "last_cause": "bogus",
            }))
            .is_err()
        );

        // StopAction: stop & discard allowed, others rejected.
        for (action, expected) in [("stop", StopAction::Stop), ("discard", StopAction::Discard)] {
            let stop: Stop = serde_json::from_value(json!({
                "command_id": 1,
                "principal": "human",
                "key": "human-1",
                "action": action,
            }))
            .expect("valid stop action");
            assert_eq!(stop.action, expected);
        }
        assert!(
            serde_json::from_value::<Stop>(json!({
                "command_id": 1,
                "principal": "human",
                "key": "human-1",
                "action": "input",
            }))
            .is_err()
        );

        // Recovery LoopStart invariants.
        let mut recovery = Recovery::new();
        assert_eq!(recovery.earlier_boot_id(), None);
        recovery.start = LoopStart::Recorded {
            earlier_boot_id: Some("boot-abc".to_string()),
        };
        assert_eq!(recovery.earlier_boot_id(), Some("boot-abc"));
    }

    #[test]
    fn historical_2026_11_08_shapes_decode_cleanly() {
        // 1. Historical `execution.ended` with `cause: "memory_limit"` (4492e363..47717497).
        let legacy_exec: Execution = serde_json::from_value(json!({
            "id": "exec_1436",
            "started_at_ms": 1_762_560_000_000_i64,
            "ended_at_ms": 1_762_560_005_000_i64,
            "cause": "memory_limit",
            "exit_code": null,
            "signal": 9,
            "evidence": "systemd oom-kill",
            "end_event_id": 77,
        }))
        .expect("historical memory_limit execution.ended must decode");
        let ended = legacy_exec.end().expect("ended");
        assert_eq!(ended.cause, RecordedEndCause::MemoryLimit);
        assert_eq!(json!(ended.cause), json!("memory_limit"));
        assert_eq!(
            classify(&Facts {
                end: Some(ended),
                ..Facts::default()
            }),
            Cause::Crashed
        );

        // 2. Historical non-`final` `execution.interrupted` without `due_ms` (or `due_ms: null`).
        for (path, due_field, expected_path) in [
            ("resume", None, Path::Resume),
            ("handoff", Some(Value::Null), Path::Handoff),
            ("wait", None, Path::Wait),
        ] {
            let mut rec = json!({
                "type": "execution.interrupted",
                "cause": "memory_limit",
                "path": path,
                "attempt": 1,
                "fact_event_id": 77,
                "evidence": {},
            });
            if let Some(due) = due_field {
                rec["due_ms"] = due;
            }
            let parsed: Recorded = serde_json::from_value(json!({
                "event_id": 88,
                "at_ms": 1_762_560_006_000_i64,
                "record": rec,
            }))
            .expect("historical non-final record without due_ms must decode");
            match parsed {
                Recorded::Interrupted(interrupted) => {
                    assert_eq!(
                        interrupted.schedule,
                        RecordedSchedule::Legacy {
                            path: Some(expected_path),
                            due_ms: None,
                        }
                    );
                    assert_eq!(interrupted.explicit_path(), Some(expected_path));
                    assert_eq!(interrupted.effective_path(), expected_path);
                    assert_eq!(interrupted.due_ms(), None);
                }
                Recorded::Cleared { .. } => panic!("expected Interrupted"),
            }
        }
    }

    #[test]
    fn malformed_facts_row_skips_only_affected_session_with_sanitized_log() {
        let healthy = |session: &str| {
            Ok(json!({
                "session_id": session,
                "agent_id": format!("agent_{session}"),
                "execution": {
                    "id": format!("exec_{session}"),
                    "started_at_ms": 1_000,
                },
            }))
        };
        let malformed = Ok(json!({
            "session_id": "bad_session",
            "agent_id": "agent_bad",
            "execution": {
                "id": "exec_bad",
                "started_at_ms": 1_000,
                "ended_at_ms": 2_000,
                "cause": "sk-live-secret-do-not-echo",
                "end_event_id": 55,
            },
        }));
        let mut lines = Vec::new();
        let mut visited = Vec::new();
        let outcome = visit_pass_rows(
            vec![
                ("healthy_1".to_string(), healthy("healthy_1")),
                ("bad_session".to_string(), malformed),
                ("healthy_2".to_string(), healthy("healthy_2")),
            ],
            &mut lines,
            |row| {
                visited.push(row.session_id.clone());
                Ok(vec![format!("ok: {}", row.session_id)])
            },
        );
        assert_eq!(outcome, Ok(()));
        assert_eq!(visited, ["healthy_1", "healthy_2"]);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "ok: healthy_1");
        assert!(
            lines[1].contains("bad_session") && !lines[1].contains("sk-live-secret-do-not-echo"),
            "skip line must name session and redact raw value: {}",
            lines[1]
        );
        assert_eq!(lines[2], "ok: healthy_2");

        // Store/transport failures still propagate for outage backoff and handling.
        let mut err_lines = Vec::new();
        let transport_err = visit_pass_rows(
            vec![("s1".to_string(), Err("connection refused".to_string()))],
            &mut err_lines,
            |_row| Ok(Vec::new()),
        );
        assert_eq!(transport_err, Err("connection refused".to_string()));
    }
}
