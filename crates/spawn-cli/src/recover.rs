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
//!   toward the handoff.
//! - `final` (stopped): never revived.
//!
//! The cause is read from recorded facts, in this precedence: stopped,
//! budget, memory_limit, host_restart, disk_full, database_unavailable,
//! model_auth, model_unavailable, context_exhausted, crashed, exited. Each
//! is named in the record's evidence ([`classify`], [`model_cause`]).
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

use serde::Deserialize;
use serde_json::{Map, Value, json};

use spawn_core::command::CommandAction;
use spawn_core::event::ENGINE;
use spawn_core::id::{AgentId, IdempotencyKey, Principal, SessionId};
use spawn_core::port::{CommandQueue, TerminalHost};
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

/// One cause of an interruption: the whole vocabulary, and the only one the
/// record's schema accepts (`spawn_host_record`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cause {
    Exited,
    Crashed,
    MemoryLimit,
    ContextExhausted,
    ModelUnavailable,
    ModelAuth,
    /// The model refused the request in a way no retry changes (a 400 of
    /// another kind than an exhausted context, a 404, a 413): the engine
    /// ended after it with no progress.
    ModelError,
    HostRestart,
    DatabaseUnavailable,
    DiskFull,
    Budget,
    Stopped,
}

impl Cause {
    pub(crate) fn name(self) -> &'static str {
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
        }
    }

    /// The way back this cause takes.
    pub(crate) fn path(self) -> Path {
        match self {
            Self::Exited | Self::Crashed | Self::MemoryLimit | Self::HostRestart => Path::Resume,
            Self::ContextExhausted | Self::Budget => Path::Handoff,
            Self::ModelUnavailable
            | Self::ModelAuth
            | Self::DatabaseUnavailable
            | Self::DiskFull => Path::Wait,
            Self::ModelError | Self::Stopped => Path::Final,
        }
    }
}

/// The way back from an interruption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Path {
    Resume,
    Handoff,
    Wait,
    Final,
}

impl Path {
    pub(crate) fn name(self) -> &'static str {
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

/// The engine's end, as `execution.ended` recorded it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub(crate) struct End {
    #[serde(default)]
    pub(crate) event_id: i64,
    /// `exited`, `stopped` or `lost` (the Bridge's three).
    #[serde(default)]
    pub(crate) cause: String,
    #[serde(default)]
    pub(crate) exit_code: Option<i32>,
    #[serde(default)]
    pub(crate) signal: Option<i32>,
    #[serde(default)]
    pub(crate) evidence: Option<String>,
}

/// A Stop or Discard into the session during the execution.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct Stop {
    pub(crate) command_id: i64,
    #[serde(default)]
    pub(crate) principal: String,
    #[serde(default)]
    pub(crate) key: String,
    /// `stop` or `discard`.
    #[serde(default)]
    pub(crate) action: String,
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
    /// A Stop or Discard from a person, the supervisor or a cancelled task
    /// (never the recovery's own).
    pub(crate) stop: Option<&'a Stop>,
    /// The integrator's over-budget record of the task
    /// (`{"event_id", "record": {"budget", ...}}`).
    pub(crate) over_budget: Option<&'a Value>,
    /// The execution's resource envelope killed it (objective 029's facts).
    pub(crate) memory: Option<&'a Value>,
    /// The terminal server that held every terminal is gone: a host
    /// restart took the execution with it.
    pub(crate) terminal_server_gone: bool,
    /// A free-space reading of the workspace's filesystem under the floor,
    /// taken at the end.
    pub(crate) disk: Option<&'a disk::Reading>,
    /// The database outage the execution lived through.
    pub(crate) outage: Option<&'a Outage>,
    /// The failed turn the engine ended after, with no progress since, when
    /// the model refused it for good ([`Turn::model_error`]).
    pub(crate) model_error: Option<&'a Turn>,
    /// Whether the session is a retired resident, or its task cancelled:
    /// how a `stopped` end came about, in its evidence.
    pub(crate) retired: bool,
    pub(crate) task_cancelled: bool,
}

/// The cause of an execution's end, its facts given. The first that holds
/// wins, in the contract's order: a person's Stop before the budget, a
/// memory kill before a host restart, a full disk before a database outage,
/// a signal before a clean exit.
pub(crate) fn classify(facts: &Facts<'_>) -> Cause {
    let end = facts.end.cloned().unwrap_or_default();
    if facts.stop.is_some() || end.cause == "stopped" {
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
    if facts.model_error.is_some() && end.cause != "lost" {
        return Cause::ModelError;
    }
    // A signal, a non-zero code, or a terminal gone while its server lived:
    // the engine did not exit by itself.
    if end.exit_code.is_some_and(|code| code != 0) || end.signal.is_some() || end.cause == "lost" {
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

/// The credentials file as the loop's own environment names it:
/// `$CLAUDE_CONFIG_DIR/.credentials.json`, else `$HOME/.claude/.credentials.json`.
pub(crate) fn credentials_path() -> Option<String> {
    if let Some(dir) = crate::config::env_value("CLAUDE_CONFIG_DIR") {
        return Some(format!("{dir}/.credentials.json"));
    }
    crate::config::env_value("HOME").map(|home| format!("{home}/.claude/.credentials.json"))
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

/// The way back from `cause`, and what its record's evidence gains: a resume
/// path becomes a handoff after [`RESUME_ATTEMPTS`] failed resumes in a row,
/// or when there is no native session to resume. `final` is never revived.
pub(crate) fn way(
    cause: Cause,
    native_bound: bool,
    failed_resumes: u32,
) -> (Path, Map<String, Value>) {
    let mut evidence = Map::new();
    if cause.path() == Path::Resume && failed_resumes >= RESUME_ATTEMPTS {
        evidence.insert("resumes_failed".to_string(), json!(RESUME_ATTEMPTS));
        return (Path::Handoff, evidence);
    }
    if cause.path() == Path::Resume && !native_bound {
        evidence.insert("native".to_string(), json!("none"));
        return (Path::Handoff, evidence);
    }
    (cause.path(), evidence)
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

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Execution {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) started_at_ms: i64,
    #[serde(default)]
    pub(crate) ended_at_ms: Option<i64>,
    #[serde(default)]
    pub(crate) cause: Option<String>,
    #[serde(default)]
    pub(crate) exit_code: Option<i32>,
    #[serde(default)]
    pub(crate) signal: Option<i32>,
    #[serde(default)]
    pub(crate) evidence: Option<String>,
    #[serde(default)]
    pub(crate) workspace: Option<String>,
    #[serde(default)]
    pub(crate) end_event_id: Option<i64>,
}

impl Execution {
    /// The end, as the classification reads it.
    fn end(&self) -> Option<End> {
        self.ended_at_ms.map(|_| End {
            event_id: self.end_event_id.unwrap_or(0),
            cause: self.cause.clone().unwrap_or_default(),
            exit_code: self.exit_code,
            signal: self.signal,
            evidence: self.evidence.clone(),
        })
    }
}

/// A record the recovery wrote about the execution.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Recorded {
    pub(crate) event_id: i64,
    #[serde(default)]
    pub(crate) at_ms: i64,
    pub(crate) record: Value,
}

impl Recorded {
    fn is_type(&self, what: &str) -> bool {
        self.record["type"].as_str() == Some(what)
    }

    fn path(&self) -> Option<&str> {
        self.record["path"].as_str()
    }

    fn cause(&self) -> Option<&str> {
        self.record["cause"].as_str()
    }

    fn attempt(&self) -> u32 {
        self.record["attempt"]
            .as_u64()
            .and_then(|attempt| u32::try_from(attempt).ok())
            .unwrap_or(0)
    }

    fn due_ms(&self) -> Option<i64> {
        self.record["due_ms"].as_i64()
    }

    /// The log event the interruption rests on: the `turn.failed` or the
    /// end it was told from, so the next fact is the next one after it and
    /// a restarted loop records nothing twice.
    fn fact_event_id(&self) -> i64 {
        self.record["fact_event_id"].as_i64().unwrap_or(0)
    }
}

/// A command into the session (the recovery's, or anyone's).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Sent {
    pub(crate) key: String,
    #[serde(default)]
    pub(crate) created_at_ms: i64,
    #[serde(default)]
    pub(crate) finished_at_ms: Option<i64>,
    #[serde(default)]
    pub(crate) succeeded: Option<bool>,
}

/// One session's facts (`spawn_postgres::RECOVERY_FACTS`).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct FactsRow {
    pub(crate) session_id: String,
    pub(crate) agent_id: String,
    #[serde(default)]
    pub(crate) execution: Option<Value>,
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
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct Executed {
    pub(crate) id: String,
    pub(crate) started_at_ms: i64,
    #[serde(default)]
    pub(crate) ended_at_ms: Option<i64>,
    /// How long it ran, when it ended.
    #[serde(default)]
    pub(crate) duration_ms: Option<i64>,
    /// A tool call completed or a turn ended after it started.
    #[serde(default)]
    pub(crate) progress: bool,
    /// The key of the command that started it: a revival's when it starts
    /// with [`KEY_PREFIX`].
    #[serde(default)]
    pub(crate) key: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct Progress {
    #[serde(default)]
    pub(crate) event_id: i64,
    #[serde(default)]
    pub(crate) at_ms: i64,
}

impl FactsRow {
    pub(crate) fn execution(&self) -> Option<Execution> {
        self.execution
            .clone()
            .and_then(|value| serde_json::from_value(value).ok())
    }
}

// --- the loop's state -------------------------------------------------------

/// What the loop knows between passes: the outage it is in, the loop before
/// it, and when an open wait last re-read its cause. Everything the
/// decisions rest on is in the log; this is only what no record holds yet.
#[derive(Debug, Default)]
pub(crate) struct Recovery {
    /// This process recorded its `host.loop_started`.
    started: bool,
    /// The loop before this one, as it recorded its start.
    earlier: Option<Value>,
    /// The outage in progress, or the one since this pass reached the
    /// database.
    outage: Option<Outage>,
    /// When an open wait last re-read its cause, by session and attempt.
    probes: BTreeMap<String, i64>,
    /// Whether this pass left a wait open: its cause (a model that refuses,
    /// a login that expires, a full disk) is cleared by an observation no
    /// append announces, so the loop must run the step again every probe,
    /// not only when the log moves ([`Self::probing`]).
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
    for Candidate {
        session_id,
        agent_id,
    } in sessions
    {
        let (session, agent) = (session_id, agent_id);
        if !kept(&agent, &session, work, &residents) {
            continue;
        }
        let facts = store
            .recovery_facts(&SessionId::new(&session).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
        let row: FactsRow = serde_json::from_value(facts)
            .map_err(|error| format!("recovery facts of {session}: {error}"))?;
        if row.execution().is_none() {
            continue;
        }
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
            records,
            residents: &residents,
            retired: &retired,
            lines: Vec::new(),
        };
        one.run(outage.as_ref())?;
        lines.extend(one.lines);
    }
    // The database answered every session this pass read: the outage is
    // over, its records written.
    if outage.is_some() {
        state.outage = None;
    }
    Ok(lines)
}

impl Recovery {
    /// Record this loop's start once, keeping the loop before it.
    fn record_start(&mut self, store: &PostgresStore, now_ms: i64) -> Result<(), String> {
        if self.started {
            return Ok(());
        }
        self.earlier = store.loop_started().map_err(|error| error.to_string())?;
        let record = json!({
            "type": "host.loop_started",
            "boot_id": boot_id(),
            "started_at_ms": now_ms,
        });
        store
            .host_record(&record)
            .map_err(|error| format!("host.loop_started: {error}"))?;
        self.started = true;
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
        && work.tasks.iter().any(|task| {
            task.session_id == session && task.state == spawn_core::mission::TaskState::Delivered
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
    /// The execution's recovery records as this pass knows them: what was
    /// read, and what this pass recorded.
    records: Vec<Recorded>,
    residents: &'a BTreeMap<String, String>,
    retired: &'a [String],
    lines: Vec<String>,
}

/// The fact an interruption is told from.
enum Fact {
    Turn(Turn),
    Tool(ToolFailure),
    End(End),
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
        self.resume_path()
    }

    fn execution(&self) -> Execution {
        self.row
            .execution()
            .expect("the pass is only run for a session with an execution")
    }

    fn live(&self) -> bool {
        self.execution().ended_at_ms.is_none()
    }

    /// The interruptions recorded for the execution, oldest first.
    fn interruptions(&self) -> Vec<&Recorded> {
        self.records
            .iter()
            .filter(|record| record.is_type("execution.interrupted"))
            .collect()
    }

    /// The clear that answers `record`, if the wait is over.
    fn cleared_after(&self, record: &Recorded) -> Option<&Recorded> {
        self.records
            .iter()
            .find(|later| later.is_type("execution.cleared") && later.event_id > record.event_id)
    }

    /// Whether the execution ran through the outage: it started before the
    /// first failed pass and had not ended by it.
    fn ran_through(&self, outage: &Outage) -> bool {
        let execution = self.execution();
        execution.started_at_ms < outage.from_ms
            && execution
                .ended_at_ms
                .is_none_or(|ended| ended > outage.from_ms)
    }

    /// Whether this outage is already recorded for the execution: its record
    /// names the window's start.
    fn recorded_outage(&self, outage: &Outage) -> bool {
        self.interruptions().iter().any(|record| {
            record.cause() == Some(Cause::DatabaseUnavailable.name())
                && record.record["evidence"]["from_ms"].as_i64() == Some(outage.from_ms)
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
            &self.execution(),
            &self.row.stops,
            &self.row.commands,
        )
    }

    /// The latest Stop or Discard that may have stopped the execution, not
    /// the recovery's own.
    fn last_stop(&self) -> Option<&Stop> {
        self.row
            .stops
            .iter()
            .rev()
            .find(|stop| !stop.ours() && stop.reached())
    }

    /// The failed turn the engine ended after when the model refused it for
    /// good, with no progress since ([`refused`]).
    fn refused_turn(&self, end: &End) -> Option<&Turn> {
        let after = self
            .interruptions()
            .last()
            .map_or(0, |record| record.fact_event_id());
        refused(
            &self.row.turns,
            self.row.progress.event_id,
            after,
            (end.event_id > 0).then_some(end.event_id),
        )
    }

    /// A free-space reading of the workspace's filesystem, taken now: on
    /// the workspace itself while its mount is there, else on the root that
    /// holds every workspace (the same filesystem, on this host).
    fn disk_reading(&self) -> Option<disk::Reading> {
        let mounted = self
            .execution()
            .workspace
            .map(|workspace| {
                workspace
                    .split_once(':')
                    .map_or(workspace.as_str(), |(_, path)| path)
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
    /// evidence, its path and its due time.
    fn record(
        &mut self,
        cause: Cause,
        evidence: Map<String, Value>,
        fact_event_id: i64,
        path: Path,
        due_ms: Option<i64>,
        note: &str,
    ) -> Result<Recorded, String> {
        let execution = self.execution();
        let attempt = u32::try_from(self.interruptions().len())
            .unwrap_or(u32::MAX)
            .saturating_add(1);
        let record = json!({
            "type": "execution.interrupted",
            "agent_id": self.row.agent_id,
            "session_id": self.row.session_id,
            "execution_id": execution.id,
            "cause": cause.name(),
            "evidence": Value::Object(evidence),
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
            "recover: {} {} of execution {} (attempt {attempt}, {}): {note}",
            self.row.session_id,
            cause.name(),
            execution.id,
            path.name()
        ));
        let recorded = Recorded {
            event_id,
            at_ms: self.now_ms,
            record,
        };
        self.records.push(recorded.clone());
        Ok(recorded)
    }

    /// Record the clear of the wait `record` opened.
    fn clear(
        &mut self,
        record: &Recorded,
        cause: Cause,
        evidence: Map<String, Value>,
    ) -> Result<(), String> {
        let wait_start = self
            .interruptions()
            .iter()
            .rev()
            .take_while(|older| older.path() == Some(Path::Wait.name()))
            .last()
            .map_or(record.at_ms, |first| first.at_ms);
        let cleared = json!({
            "type": "execution.cleared",
            "execution_id": self.execution().id,
            "session_id": self.row.session_id,
            "cause": cause.name(),
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
            cause_name = cause.name()
        ));
        self.records.push(Recorded {
            event_id,
            at_ms: self.now_ms,
            record: cleared,
        });
        Ok(())
    }

    // --- recording a fact's interruption ----------------------------------

    /// The interruption an outage makes for an execution that lived through
    /// it, and its clear: the pass that reads this reached the database.
    fn interrupt_outage(&mut self, outage: &Outage) -> Result<(), String> {
        let execution = self.execution();
        let mut evidence = outage.evidence();
        if let Some(code) = execution.exit_code {
            evidence.insert("exit_code".to_string(), json!(code));
        }
        if let Some(signal) = execution.signal {
            evidence.insert("signal".to_string(), json!(signal));
        }
        // What the log already knew of the execution is explained by this
        // record: the next fact it waits for is the one after it.
        let explained = self.row.progress.event_id.max(
            self.interruptions()
                .last()
                .map_or(0, |record| record.fact_event_id()),
        );
        let record = self.record(
            Cause::DatabaseUnavailable,
            evidence,
            explained,
            Path::Wait,
            Some(outage.to_ms),
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
        self.clear(&record, Cause::DatabaseUnavailable, cleared)
    }

    /// The interruption a fact makes.
    fn interrupt(&mut self, fact: Fact) -> Result<(), String> {
        match fact {
            Fact::Turn(turn) => self.interrupt_turn(&turn),
            Fact::Tool(tool) => self.interrupt_tool(&tool),
            Fact::End(end) => self.interrupt_end(&end),
        }
    }

    fn interrupt_turn(&mut self, turn: &Turn) -> Result<(), String> {
        let Some(cause) = turn.cause() else {
            return Ok(());
        };
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
            path,
            Some(due),
            "its turn failed with a model cause and its engine waits at its prompt",
        )?;
        Ok(())
    }

    /// The interruption a failed tool call makes: a full disk ends the turn
    /// and leaves the engine at its prompt, waiting for room. The wait is
    /// the disk's: every probe reads the free space again, and the clear is
    /// the reading at or above the floor.
    fn interrupt_tool(&mut self, tool: &ToolFailure) -> Result<(), String> {
        let Some(cause) = tool.cause() else {
            return Ok(());
        };
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
            path,
            Some(self.now_ms),
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
        let facts = Facts {
            end: Some(end),
            stop: self.last_stop(),
            over_budget: self.row.over_budget.as_ref(),
            memory: None,
            terminal_server_gone: !self.terminals.server_alive(),
            disk: below.as_ref(),
            outage: None,
            retired: self.retired.contains(&self.row.session_id),
            task_cancelled: false,
            model_error: self.refused_turn(end),
        };
        let cause = classify(&facts);
        let mut evidence = self.end_evidence(cause, end, &facts);
        let (path, mut gains) = way(
            cause,
            self.row.native_session_id.is_some(),
            self.failures_in_a_row(),
        );
        evidence.append(&mut gains);
        let due = match path {
            Path::Final => None,
            Path::Resume | Path::Handoff => Some(self.resume_due()),
            Path::Wait => Some(self.now_ms),
        };
        self.record(
            cause,
            evidence,
            end.event_id,
            path,
            due,
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
                if let Some(stop) = facts.stop {
                    evidence.insert("command_id".to_string(), json!(stop.command_id));
                    evidence.insert("principal".to_string(), json!(stop.principal));
                    evidence.insert("command".to_string(), json!(stop.action));
                }
                if facts.retired {
                    evidence.insert("retired".to_string(), json!(true));
                }
                if facts.task_cancelled {
                    evidence.insert("task_cancelled".to_string(), json!(true));
                }
            }
            Cause::Budget => {
                if let Some(over) = self.row.over_budget.as_ref() {
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
                        .earlier
                        .as_ref()
                        .and_then(|earlier| earlier["boot_id"].as_str())
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
                        && sent.succeeded == Some(false)
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
                .take_while(|record| record.cause() == Some(cause.name()))
                .count(),
        )
        .unwrap_or(u32::MAX)
    }

    // --- the way back -----------------------------------------------------

    /// The key of an interruption's revival: `recover-<execution>-<attempt>`.
    fn key_prefix(&self, record: &Recorded) -> String {
        key(&self.execution().id, record.attempt())
    }

    /// Take the way back from the execution's latest interruption, if it is
    /// due.
    fn resume_path(&mut self) -> Result<(), String> {
        let Some(last) = self.interruptions().last().copied().cloned() else {
            return Ok(());
        };
        let Some(cause) = Cause::from_name(last.cause().unwrap_or_default()) else {
            return Ok(());
        };
        match cause.path() {
            Path::Final => Ok(()),
            Path::Wait => self.wait_path(&last, cause),
            Path::Resume | Path::Handoff => self.revive(&last, cause, false),
        }
    }

    /// A wait: nothing while the cause holds. When it clears, one
    /// `execution.cleared` record and then the revival.
    fn wait_path(&mut self, last: &Recorded, cause: Cause) -> Result<(), String> {
        if self.cleared_after(last).is_some() {
            // Cleared: the revival is due now.
            return self.revive(last, cause, true);
        }
        // The cause still holds: no append may announce its clearing, so the
        // loop re-reads it every probe until it does.
        self.state.open_waits = true;
        match cause {
            Cause::ModelUnavailable => {
                // The retry is the revival; the clear is the model's answer
                // to it.
                if self.row.progress.event_id > last.fact_event_id() {
                    let mut evidence = Map::new();
                    evidence.insert(
                        "answer_event_id".to_string(),
                        json!(self.row.progress.event_id),
                    );
                    evidence.insert("answer_at_ms".to_string(), json!(self.row.progress.at_ms));
                    self.clear(last, cause, evidence)?;
                    return self.revive(last, cause, true);
                }
                self.revive(last, cause, false)
            }
            Cause::ModelAuth => {
                if let Some(changed) = self.credentials_changed(last) {
                    self.clear(last, cause, changed)?;
                    return self.revive(last, cause, true);
                }
                // One retry every `auth_retry` while the file holds: a
                // long outage of a login must not leave the agent down for
                // good.
                self.auth_retry(last)
            }
            Cause::DiskFull => {
                if let Some(evidence) = self.disk_clear(last) {
                    self.clear(last, cause, evidence)?;
                    return self.revive(last, cause, true);
                }
                Ok(())
            }
            // The pass that reads this reached the database: the clear was
            // recorded with the interruption.
            Cause::DatabaseUnavailable => self.revive(last, cause, true),
            _ => Ok(()),
        }
    }

    /// Whether the credentials file changed since the interruption read it;
    /// the evidence when it did.
    fn credentials_changed(&mut self, last: &Recorded) -> Option<Map<String, Value>> {
        let recorded = last.record["evidence"]["credentials"].clone();
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
    fn disk_clear(&mut self, last: &Recorded) -> Option<Map<String, Value>> {
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

    fn probe_key(&self, last: &Recorded) -> String {
        format!(
            "{}:{}:{}",
            self.row.session_id,
            self.execution().id,
            last.attempt()
        )
    }

    /// One retry of an expired login, every `auth_retry` while the
    /// credentials file does not change.
    fn auth_retry(&mut self, last: &Recorded) -> Result<(), String> {
        let prefix = self.key_prefix(last);
        let tried = self
            .row
            .commands
            .iter()
            .filter(|sent| sent.key.starts_with(&format!("{prefix}-auth-")))
            .count();
        let last_try = self
            .row
            .commands
            .iter()
            .filter(|sent| sent.key.starts_with(&format!("{prefix}-auth-")))
            .filter_map(|sent| sent.created_at_ms.into())
            .max();
        let due = auth_retry_due(self.config, last.at_ms, last_try);
        if self.now_ms < due {
            return Ok(());
        }
        let key = format!("{prefix}-auth-{}", tried.saturating_add(1));
        self.submit(
            &key,
            CommandAction::Input {
                text: Self::input_text(last, Cause::ModelAuth),
                tags: Vec::new(),
            },
            "retrying the expired login",
        )
    }

    /// Take the interruption's way back: `input` into the live engine,
    /// `resume` if the execution ended, a `start` for a handoff (its `stop`
    /// first while the execution runs).
    fn revive(&mut self, last: &Recorded, cause: Cause, cleared: bool) -> Result<(), String> {
        if cause.path() == Path::Handoff || last.path() == Some(Path::Handoff.name()) {
            return self.handoff(last, cause);
        }
        let key = self.key_prefix(last);
        // A wait revives once (after its clear); a resume's command is its
        // revival whether or not the wait was cleared.
        let sent = self
            .row
            .commands
            .iter()
            .find(|sent| sent.key == key)
            .cloned();
        if let Some(sent) = &sent {
            return match sent.succeeded {
                // In flight, or done.
                None | Some(true) => Ok(()),
                Some(false) => self.retry(last, cause, &key),
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
            self.submit(
                &key,
                CommandAction::Input {
                    text: Self::input_text(last, cause),
                    tags: Vec::new(),
                },
                "nudging the engine at its prompt",
            )
        } else {
            self.submit(
                &key,
                CommandAction::Resume {
                    prompt: Some(self.resume_text(last, cause)),
                    model: None,
                },
                "resuming its native session",
            )
        }
    }

    /// A revival whose command failed: again after the resume's backoff,
    /// under a key of its own.
    fn retry(&mut self, last: &Recorded, cause: Cause, key: &str) -> Result<(), String> {
        let retries: Vec<&Sent> = self
            .row
            .commands
            .iter()
            .filter(|sent| sent.key.starts_with(&format!("{key}-r")))
            .collect();
        let failures = u32::try_from(retries.len())
            .unwrap_or(u32::MAX)
            .saturating_add(1);
        let failed_at = self
            .row
            .commands
            .iter()
            .find(|sent| sent.key == key)
            .and_then(|sent| sent.finished_at_ms)
            .unwrap_or(self.now_ms);
        let due = resume_due(
            self.config.resume_backoff,
            failures,
            Some(failed_at),
            self.now_ms,
        );
        if self.now_ms < due {
            self.state.hold_until(due);
            return Ok(());
        }
        let next = format!("{key}-r{}", retries.len().saturating_add(1));
        let action = if self.live() {
            CommandAction::Input {
                text: Self::input_text(last, cause),
                tags: Vec::new(),
            }
        } else {
            CommandAction::Resume {
                prompt: Some(self.resume_text(last, cause)),
                model: None,
            }
        };
        self.submit(&next, action, "retrying a failed revival")
    }

    /// A handoff: stop the execution if it still runs, then start a new
    /// native session in the same Spawn session, workspace and branch.
    fn handoff(&mut self, last: &Recorded, cause: Cause) -> Result<(), String> {
        let key = self.key_prefix(last);
        let stop = format!("{key}-stop");
        if self.live() {
            if !self.row.commands.iter().any(|sent| sent.key == stop) {
                self.submit(&stop, CommandAction::Stop, "stopping for its handoff")?;
            }
            return Ok(());
        }
        if let Some(sent) = self.row.commands.iter().find(|sent| sent.key == key) {
            return match sent.succeeded {
                None | Some(true) => Ok(()),
                Some(false) => self.retry(last, cause, &key),
            };
        }
        let due = last.due_ms().unwrap_or(self.now_ms);
        if self.now_ms < due {
            self.state.hold_until(due);
            return Ok(());
        }
        let (prompt, charter) = self.handoff_prompt(last, cause);
        self.submit(
            &key,
            CommandAction::Start {
                engine: ENGINE.to_string(),
                prompt,
                parent_session_id: None,
                from: None,
                model: None,
                charter,
            },
            "starting a new native session for its handoff",
        )
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
    fn resume_text(&self, last: &Recorded, cause: Cause) -> String {
        format!(
            "Spawn restarted your engine: execution {} {} ({}). Continue your task from where \
             you were; what you did is in your workspace.",
            self.execution().id,
            cause.name(),
            describe(last)
        )
    }

    fn input_text(last: &Recorded, cause: Cause) -> String {
        format!(
            "Spawn: the {} that stopped your last turn is over ({}). Continue your task from \
             where you were; the work is in your workspace.",
            cause.name().replace('_', " "),
            describe(last)
        )
    }

    /// The prompt of a handoff: the task's brief (a resident's role, the
    /// supervisor's charter and summary), the interruption, the branch's
    /// head, and that the work is in the workspace to continue.
    fn handoff_prompt(&self, last: &Recorded, cause: Cause) -> (String, Option<String>) {
        let head = self
            .row
            .head
            .as_ref()
            .map_or_else(String::new, |head| format!(" (its head is {head})"));
        let interruption = format!(
            "\n\nYour previous native session could not go on: {} ({}). This is a new native \
             session in the same Spawn session, workspace and branch{head}. The work is in the \
             workspace to continue, not to redo: read what is there and carry on.",
            cause.name(),
            describe(last)
        );
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
fn describe(record: &Recorded) -> String {
    let evidence = &record.record["evidence"];
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
    let cause = record.cause().unwrap_or("interruption");
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
    let turn = turns.iter().rev().find(|turn| {
        turn.event_id > after && end_event_id.is_none_or(|end| turn.event_id < end)
    })?;
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
    let interruptions: Vec<&Recorded> = records
        .iter()
        .filter(|record| record.is_type("execution.interrupted"))
        .collect();
    let last = interruptions.last().copied();
    let after = last.map_or(0, Recorded::fact_event_id);
    let cleared = |record: &Recorded| {
        records
            .iter()
            .any(|later| later.is_type("execution.cleared") && later.event_id > record.event_id)
    };
    let open_wait =
        last.is_some_and(|record| record.path() == Some(Path::Wait.name()) && !cleared(record));
    let turn = turns
        .iter()
        .find(|turn| turn.event_id > after && turn.cause().is_some());
    let tool = tools
        .iter()
        .find(|tool| tool.event_id > after && tool.cause().is_some());
    match (turn, tool) {
        // The first of the two in the log: a disk's tool failure and a
        // model's turn failure can sit in the same stretch of work.
        (Some(turn), Some(tool)) if tool.event_id < turn.event_id => {
            return Some(Fact::Tool(tool.clone()));
        }
        (Some(turn), _) => return Some(Fact::Turn(turn.clone())),
        (None, Some(tool)) => return Some(Fact::Tool(tool.clone())),
        (None, None) => {}
    }
    // A handoff ends the execution with its own Stop.
    if last.is_some_and(|record| {
        record.path() == Some(Path::Handoff.name())
            && commands
                .iter()
                .any(|sent| sent.key == format!("{}-stop", key(&execution.id, record.attempt())))
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
    Some(Fact::End(end))
}

impl Cause {
    /// [`Cause::name`] read back, for a recorded interruption.
    fn from_name(name: &str) -> Option<Self> {
        [
            Self::Exited,
            Self::Crashed,
            Self::MemoryLimit,
            Self::ContextExhausted,
            Self::ModelUnavailable,
            Self::ModelAuth,
            Self::ModelError,
            Self::HostRestart,
            Self::DatabaseUnavailable,
            Self::DiskFull,
            Self::Budget,
            Self::Stopped,
        ]
        .into_iter()
        .find(|cause| cause.name() == name)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Cause, Config, Credentials, End, Executed, Execution, Facts, Outage, Path, Recorded,
        Recovery, Sent, Stop, ToolFailure, Turn, auth_retry_due, classify, counts_as_failure,
        disk_cleared, key, model_due, pending, resume_due, resume_failed, retry_at, way,
    };
    use serde_json::json;

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

    fn end(cause: &str, exit_code: Option<i32>, signal: Option<i32>) -> End {
        End {
            event_id: 90,
            cause: cause.to_string(),
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
            action: "stop".to_string(),
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
        let crashed = end("exited", None, Some(9));
        assert_eq!(classify(&facts(Some(&crashed))), Cause::Crashed);
        let nonzero = end("exited", Some(1), None);
        assert_eq!(classify(&facts(Some(&nonzero))), Cause::Crashed);
        let clean = end("exited", Some(0), None);
        assert_eq!(classify(&facts(Some(&clean))), Cause::Exited);
        // A terminal that disappeared while its server lives on is a crash.
        let gone = end("lost", None, None);
        assert_eq!(classify(&facts(Some(&gone))), Cause::Crashed);
        // A stop the Bridge recorded on its own (a workspace kept off its
        // branch) ends the session: nothing revives it.
        let stopped = end("stopped", None, None);
        assert_eq!(classify(&facts(Some(&stopped))), Cause::Stopped);
    }

    #[test]
    fn the_more_specific_fact_wins() {
        let nonzero = end("exited", Some(1), None);
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
            stop: Some(&stopped),
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
        let lost_end = end("lost", None, None);
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
            ended_at_ms: ended,
            cause: ended.map(|_| "exited".to_string()),
            exit_code: Some(0),
            signal: None,
            evidence: None,
            workspace: None,
            end_event_id: ended.map(|_| 90),
        }
    }

    fn recorded(what: &str, path: &str, attempt: u32, fact: i64, event_id: i64) -> Recorded {
        Recorded {
            event_id,
            at_ms: 1_000,
            record: json!({
                "type": what,
                "cause": "crashed",
                "path": path,
                "attempt": attempt,
                "fact_event_id": fact,
                "due_ms": 1_000,
            }),
        }
    }

    /// The clear of the wait `wait` answers: what shows its cause gone,
    /// recorded after it.
    fn cleared_after(wait: &Recorded, event_id: i64) -> Recorded {
        Recorded {
            event_id,
            at_ms: 1_400,
            record: json!({
                "type": "execution.cleared",
                "execution_id": "exec_1",
                "session_id": "session_worker_recovery",
                "cause": wait.record["cause"],
                "evidence": {},
                "waited_ms": 400,
            }),
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
            Some(super::Fact::Turn(turn)) => assert_eq!(turn.event_id, 50),
            _ => panic!("the first failed turn"),
        }
        // That turn recorded: the second one.
        let first = recorded("execution.interrupted", "wait", 1, 50, 10);
        match pending(
            std::slice::from_ref(&first),
            &turns,
            &[],
            &execution,
            &[],
            &[],
        ) {
            Some(super::Fact::Turn(turn)) => assert_eq!(turn.event_id, 60),
            _ => panic!("the second failed turn"),
        }
        // Both, their wait cleared, and the end after them.
        let second = recorded("execution.interrupted", "wait", 2, 60, 11);
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
        let ended = recorded("execution.interrupted", "resume", 3, 90, 13);
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
            Some(super::Fact::Tool(tool)) => assert_eq!(tool.event_id, 55),
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
            Some(super::Fact::Turn(_))
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
            Some(super::Fact::Tool(_))
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
        let waiting = recorded("execution.interrupted", "wait", 1, 50, 10);
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
        let handoff = recorded("execution.interrupted", "handoff", 1, 50, 10);
        let sent = Sent {
            key: "recover-exec_7-1-stop".to_string(),
            created_at_ms: 1,
            finished_at_ms: Some(2),
            succeeded: Some(true),
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
    }

    #[test]
    fn a_resume_counts_as_failed_only_when_it_died_young_without_progress() {
        let young = Executed {
            id: "exec_1".to_string(),
            started_at_ms: 1_000,
            ended_at_ms: Some(4_000),
            duration_ms: Some(3_000),
            progress: false,
            key: "recover-exec_1-1".to_string(),
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
}
