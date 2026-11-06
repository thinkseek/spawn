//! Delivery: the dispatcher's half of the mission layer.
//!
//! The supervisor decides tasks (`spawn task assign`); delivery only carries
//! them out, by rule:
//!
//! - one task in flight per worker, oldest first;
//! - a worker that never ran is started, one that ran before is resumed (its
//!   context is the point of a resident), a live one gets the task typed in,
//!   but only while it is idle at its prompt, after the source's main is
//!   merged into its workspace ([`crate::main_merge`], done by
//!   [`crate::dispatch::step`]);
//! - a task sent back is delivered again with the reviewer's note;
//! - a task runs on its own model profile: a start or resume names it, and a
//!   live worker on another profile is stopped first, then resumed on the
//!   task's (a running engine keeps the models it started with);
//! - a worker whose stop is under way gets nothing typed in; a Stop that
//!   fails (this module's, or anyone's) is pursued and escalated by the
//!   dispatcher (`dispatch::stops`), and the task keeps waiting;
//! - a report (`.spawn/report.md`) is collected once its worker is idle, if
//!   it was written after the delivery's mark in the workspace (one clock,
//!   the filesystem's, sets both times), and moved aside so the next task
//!   starts clean. **A report is judged against the checkpoint of the turn
//!   that produced it.** The Bridge commits a turn's workspace and records
//!   its `artifact.committed` before the turn's end (`turn.stopped`,
//!   `turn.failed`), so a report is collected only once the worker's latest
//!   turn event in the log is a turn's end, not a newer `message.received`,
//!   and never on a ready screen alone ([`Facts::report_due`]). The
//!   `task.reported` record names in `commit` the agent's latest checkpoint
//!   read with that turn's end (null only when the agent has none); a turn
//!   that changed nothing leaves the branch head, the earlier checkpoint, as
//!   its own. An idle judgement with no model answer need not be newer than
//!   the turn's end: such a judgement is written again only when its verdict
//!   changes (a model's answer is written for every new situation, and a
//!   stale one waits for the next);
//! - an ephemeral worker whose task was accepted or cancelled is stopped, and
//!   so is a retired resident. In a mission with a gate, acceptance is not
//!   the end: the worker's workspace is the candidate, so it stays (and its
//!   next task waits) until the work lands or the gate sends it back;
//! - a resident whose latest delivered task is cancelled, whatever cancelled
//!   it (`spawn task cancel`, the supervisor, the integrator's abort over
//!   budget, the closure rule), is stopped at once if it runs, idle or in
//!   the middle of a turn: a Stop takes a final checkpoint, and nothing of
//!   the task's turn is left to finish. Once it no longer runs, the task's
//!   work is set aside ([`crate::set_aside`]: a commit on top of its branch
//!   whose tree is the checkpoint the task started from; the work stays in
//!   the branch's history) and the worker is resumed with a notice that
//!   names the task, says it is cancelled and where its work is. Its next
//!   task waits until that notice reached it ([`Work::blocked`]). A task
//!   never delivered, or not the latest delivery, changes nothing for its
//!   worker; an ephemeral worker is only stopped, and a retired resident is
//!   stopped and not told. A cancelled task's report is never collected: only
//!   delivered tasks are, and only a report written after their own mark.
//!
//! Missions, reports, approvals, human commands to workers (with their
//! outcome) and failed deliveries become digest items for the supervisor
//! ([`news_items`]). Like the dispatcher, delivery keeps no state:
//! idempotency keys and the task rows say what was already done.
//!
//! **A delivery that never reached its worker** (docs/fleet-architecture.md
//! §3). A delivery that fails having typed nothing is retried, under
//! `<key>-retry-<n>`, after `SPAWN_DELIVERY_RETRY_SECS` (60) doubling, up
//! to [`DELIVERY_RETRIES`] times past the free refusals of a pane not ready
//! ([`attempts`], [`Waits`]). Its retries spent:
//!
//! - a task's first delivery (the task `assigned`, revision 0), none of
//!   whose attempts may have typed ([`MAY_HAVE_TYPED`]), is re-issued, not
//!   escalated: `SPAWN_REISSUE_AFTER_SECS` (300) after its last attempt
//!   finished (the database's clock), the task is cancelled and assigned
//!   again in one transaction (`spawn_task_reissue`, [`reissues`]), the
//!   `task.cancelled` naming `by` `spawn-dispatch`, `reissued_as` and every
//!   attempt, the new `task.assigned` naming `reissue_of` and copying the
//!   task. The new task is delivered by the usual rules; the old one was
//!   never delivered, so its worker is not stopped, set aside or told, and
//!   an objective keeps a task in flight throughout;
//! - a re-issue is never re-issued again: its spent delivery is escalated
//!   ([`Why::DeliveryFailed`], tag `<session> deliver-<task>-<revision>`),
//!   naming the task, the task it re-issued and the attempts of both. A
//!   delivery that may have typed, and a revision's, are escalated and never
//!   re-issued;
//! - after a deploy (the latest `source.moved` record, which `spawn mission
//!   moved` writes for every deploy) every spent delivery, spent before the
//!   record and none of whose attempts may have typed, gets exactly one more
//!   attempt, at once, keyed `<key>-moved-<the record's event id>`
//!   ([`moved_attempt`]). A success delivers the task (a re-issue still due
//!   is then not made); a failure leaves it as it was, with no new
//!   escalation and no further attempt until the next record. A re-issue
//!   due goes before it.
//!
//! The re-issue rests on the recorded attempts and their outcomes, the
//! extra attempt on a recorded `source.moved`, never on time or absence
//! alone, and each is itself recorded (docs/design.md §14).
//!
//! A waiting task that does not move is not left to wait silently: the
//! task whose turn it is ([`next_up`]) is at its `delivery` step from the
//! record that put it there, and the dispatcher's stall layer
//! (`dispatch::stall`) escalates it once its bound
//! (`SPAWN_STALL_DELIVERY_SECS`) has passed, naming every attempt and its
//! outcome and the retry still due or held ([`delivery_status`]). One whose
//! retries are spent is re-issued or was escalated as delivery failed, and
//! gets no stall.
//!
//! [`orders`] is pure; [`collect_reports`] is the one step with side effects
//! (the report file and the task row).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use spawn_core::bridge::{MAY_HAVE_TYPED, PANE_NOT_READY};
use spawn_core::command::CommandAction;
use spawn_core::event::{ENGINE, ModelProfile};
use spawn_core::mission::TaskState;
use spawn_core::observation::Ambiguity;
use spawn_host::tree_path::TreePath;
use spawn_postgres::PostgresStore;

use crate::dispatch::{Escalation, Facts, Order, Why};

/// Where a worker writes its report, relative to its workspace.
pub(crate) const REPORT: &str = ".spawn/report.md";

/// Where collected reports are kept, relative to the workspace.
const KEPT_REPORTS: &str = ".spawn/reports";

/// Where delivery leaves one empty file per delivery it sends, named by the
/// delivery's key, relative to the workspace ([`mark_delivery`]).
const DELIVERY_MARKS: &str = ".spawn/deliveries";

/// Retries of a failed delivery, after the first attempt; a failed Stop
/// gets as many ([`crate::dispatch`]'s stop pursuit).
pub(crate) const DELIVERY_RETRIES: usize = 3;

/// Wait before the first counted retry of a delivery (the default of
/// `SPAWN_DELIVERY_RETRY_SECS`, [`Waits::retry_ms`]), and of a failed Stop
/// or cancel notice; it doubles with every retry, up to [`RETRY_MAX_MS`].
const RETRY_MS: i64 = 60_000;

/// Wait after the last attempt of a spent first delivery before its
/// re-issue (the default of `SPAWN_REISSUE_AFTER_SECS`,
/// [`Waits::reissue_after_ms`]).
const REISSUE_AFTER_MS: i64 = 5 * 60_000;

/// Wait before a delivery refused because the pane was not ready
/// ([`PANE_NOT_READY`]) is tried again: nothing was typed, and the pane is
/// usually a moment from ready.
const NOT_READY_RETRY_MS: i64 = 5_000;

/// Refusals for a pane not ready that are not counted as failed attempts;
/// past them each counts like any failure, so a pane that never shows its
/// prompt still ends in an escalation.
const NOT_READY_FREE: usize = 10;

/// Longest wait between two retries.
const RETRY_MAX_MS: i64 = 30 * 60_000;

/// The wait after the `failures`-th failed attempt before the next:
/// [`RETRY_MS`], doubling with each failure, at most [`RETRY_MAX_MS`].
pub(crate) fn retry_wait_ms(failures: usize) -> i64 {
    backoff(RETRY_MS, failures)
}

/// `base_ms` after the first failure, doubling with each one after it, at
/// most [`RETRY_MAX_MS`].
fn backoff(base_ms: i64, failures: usize) -> i64 {
    let doublings = u32::try_from(failures.saturating_sub(1))
        .unwrap_or(u32::MAX)
        .min(20);
    base_ms.saturating_mul(1_i64 << doublings).min(RETRY_MAX_MS)
}

/// The dispatcher's two delivery waits, settings `spawn loop` reads from
/// its environment ([`Waits::from_env`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Waits {
    /// Before the first counted retry of a failed delivery
    /// (`SPAWN_DELIVERY_RETRY_SECS`, default 60): it doubles with every
    /// failure, up to [`RETRY_MAX_MS`].
    pub(crate) retry_ms: i64,
    /// After the last attempt of a spent first delivery, before its
    /// re-issue (`SPAWN_REISSUE_AFTER_SECS`, default 300).
    pub(crate) reissue_after_ms: i64,
}

impl Default for Waits {
    fn default() -> Self {
        Self {
            retry_ms: RETRY_MS,
            reissue_after_ms: REISSUE_AFTER_MS,
        }
    }
}

impl Waits {
    /// The waits `spawn loop` reads from its environment, with a warning
    /// for every value that is not a whole number of seconds (its default
    /// holds).
    pub(crate) fn from_env() -> (Self, Vec<String>) {
        Self::read(crate::config::env_value)
    }

    fn read(lookup: impl Fn(&str) -> Option<String>) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let mut millis = |name: &str, fallback_ms: i64| match lookup(name) {
            None => fallback_ms,
            Some(text) => match text.trim().parse::<u64>() {
                Ok(secs) => i64::try_from(secs)
                    .unwrap_or(i64::MAX)
                    .min(i64::MAX / 1000)
                    .saturating_mul(1000),
                Err(_) => {
                    warnings.push(format!(
                        "{name}={text:?} is not a whole number of seconds; using {}",
                        fallback_ms / 1000
                    ));
                    fallback_ms
                }
            },
        };
        let waits = Self {
            retry_ms: millis("SPAWN_DELIVERY_RETRY_SECS", RETRY_MS),
            reissue_after_ms: millis("SPAWN_REISSUE_AFTER_SECS", REISSUE_AFTER_MS),
        };
        (waits, warnings)
    }

    /// The wait after a delivery's `failures`-th counted failure before
    /// its next retry.
    fn retry_wait_ms(&self, failures: usize) -> i64 {
        backoff(self.retry_ms, failures)
    }
}

/// The latest `source.moved` record (a deploy): its event's id, which keys
/// the extra attempts it gives spent deliveries, and its time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub(crate) struct Moved {
    pub(crate) id: i64,
    pub(crate) at_ms: i64,
}

/// One attempt at a delivery, as a re-issue's `task.cancelled` record keeps
/// it: its command, key and error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Attempt {
    #[serde(default)]
    pub(crate) command: Option<i64>,
    pub(crate) key: String,
    #[serde(default)]
    pub(crate) error: Option<String>,
}

/// A task to re-issue ([`reissues`]): cancelled and assigned again in one
/// transaction, its delivery's attempts named in the cancellation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reissue {
    pub(crate) task: i64,
    pub(crate) attempts: Vec<Attempt>,
}

/// One task row from `delivery_rows`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TaskRow {
    pub(crate) id: i64,
    pub(crate) mission_id: i64,
    pub(crate) kind: String,
    pub(crate) agent_id: String,
    pub(crate) session_id: String,
    pub(crate) brief: String,
    pub(crate) state: TaskState,
    #[serde(default)]
    pub(crate) revisions: i32,
    #[serde(default)]
    pub(crate) review_note: Option<String>,
    #[serde(default)]
    pub(crate) mission_text: String,
    #[serde(default)]
    pub(crate) charter: Option<String>,
    #[serde(default)]
    pub(crate) resident: Option<String>,
    /// The mission has a gate: its accepted work still has to land.
    #[serde(default)]
    pub(crate) gated: bool,
    /// The models the task's worker runs on.
    #[serde(default)]
    pub(crate) model: ModelProfile,
    /// The task this one re-issues (its `task.assigned` names it).
    #[serde(default)]
    pub(crate) reissue_of: Option<i64>,
    /// The attempts of the delivery it re-issues, as that task's
    /// `task.cancelled` record keeps them.
    #[serde(default)]
    pub(crate) reissued_attempts: Option<Vec<Attempt>>,
}

impl TaskRow {
    /// Sent back by the supervisor, waiting to be delivered again.
    fn is_revision(&self) -> bool {
        self.state == TaskState::Revise
    }

    /// Nothing more happens to this task: closed, or accepted without a
    /// gate to pass.
    fn is_finished(&self) -> bool {
        matches!(self.state, TaskState::Cancelled | TaskState::Landed)
            || (self.state == TaskState::Accepted && !self.gated)
    }

    /// A spent delivery of this task is re-issued, not escalated: its first
    /// delivery (assigned, never sent back), and it is no re-issue itself.
    fn reissuable(&self) -> bool {
        self.state == TaskState::Assigned && self.revisions == 0 && self.reissue_of.is_none()
    }

    /// Idempotency key of this task's current delivery.
    fn delivery_key(&self) -> String {
        format!("deliver-{}-{}", self.id, self.revisions)
    }

    /// A command to this task's session that carries the task to its
    /// worker: the task is marked delivered once it succeeded.
    fn delivery(&self, key: String, action: CommandAction) -> Order {
        Order {
            task: Some(self.id),
            ..self.command(key, action)
        }
    }

    /// A command to this task's session that does not carry the task (a
    /// stop before a model switch, a retirement): the task's state stays.
    fn command(&self, key: String, action: CommandAction) -> Order {
        Order {
            agent_id: self.agent_id.clone(),
            session_id: self.session_id.clone(),
            key,
            action,
            task: None,
        }
    }
}

/// A resident whose latest delivered task is cancelled, from
/// `cancelled_deliveries`: it is stopped, its work set aside, and it is told.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Cancelled {
    /// The cancelled task.
    pub(crate) id: i64,
    pub(crate) mission_id: i64,
    pub(crate) agent_id: String,
    pub(crate) session_id: String,
    /// The checkpoint the task started from; `None`: the branch's fork from
    /// `main`.
    #[serde(default)]
    pub(crate) start: Option<String>,
    /// The set-aside already recorded for the task.
    #[serde(default)]
    pub(crate) set_aside: Option<String>,
    /// The notice reached the worker.
    #[serde(default)]
    pub(crate) told: bool,
    /// The task's last control record (`task.cancelled`, `task.aborted`).
    #[serde(default)]
    pub(crate) closed_by: Option<String>,
    #[serde(default)]
    pub(crate) reason: Option<String>,
}

impl Cancelled {
    /// Key prefix of the notice's attempts: `cancel-notice-<task>-<n>`.
    fn notice_prefix(&self) -> String {
        format!("cancel-notice-{}-", self.id)
    }

    fn command(&self, key: String, action: CommandAction) -> Order {
        Order {
            agent_id: self.agent_id.clone(),
            session_id: self.session_id.clone(),
            key,
            action,
            task: None,
        }
    }
}

/// The task a cancel notice's key (`cancel-notice-<task>-<n>`) is about.
pub(crate) fn notice_task(key: &str) -> Option<i64> {
    key.strip_prefix("cancel-notice-")?
        .split('-')
        .next()?
        .parse()
        .ok()
}

/// What a resident is told when its latest task was cancelled: the task,
/// that it is cancelled, and what to do now. Where its work went is added
/// when the work is set aside ([`crate::set_aside::note`]).
pub(crate) fn cancel_notice(cancelled: &Cancelled) -> String {
    let by = match (cancelled.closed_by.as_deref(), cancelled.reason.as_deref()) {
        (Some("task.aborted"), Some(reason)) => {
            format!(" by the integrator ({})", one_line(reason, 300))
        }
        (Some("task.aborted"), None) => " by the integrator".to_string(),
        _ => String::new(),
    };
    format!(
        "[Task #{id} was cancelled — this is not a new task]\nTask #{id} of mission #{mission} \
was cancelled{by}. Stop working on it: do not continue it and do not write a report for it; \
nothing written for it is collected. Wait for your next task.",
        id = cancelled.id,
        mission = cancelled.mission_id,
    )
}

/// An open mission, for the supervisor's inbox.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct MissionNote {
    pub(crate) id: i64,
    pub(crate) text: String,
    pub(crate) origin: String,
}

/// A task waiting for review.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ReportNote {
    pub(crate) id: i64,
    pub(crate) mission_id: i64,
    pub(crate) kind: String,
    pub(crate) session_id: String,
    #[serde(default)]
    pub(crate) revisions: i32,
    pub(crate) report: String,
}

/// A resident a human approved.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ApprovedNote {
    pub(crate) name: String,
}

/// A command a human sent to a worker, once it finished.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct HumanNote {
    pub(crate) command_id: i64,
    pub(crate) session_id: String,
    /// The command's type (`input`, `stop`, ...).
    #[serde(default)]
    pub(crate) action: String,
    #[serde(default)]
    pub(crate) text: String,
    /// What it did: a failed one told the worker nothing.
    #[serde(default)]
    pub(crate) succeeded: Option<bool>,
    #[serde(default)]
    pub(crate) error: Option<String>,
}

impl HumanNote {
    /// The digest line: what the human told the worker, or what failed.
    fn line(&self) -> String {
        let text = one_line(&self.text, 200);
        if self.succeeded == Some(true) {
            return format!("a human told it: {text}");
        }
        let error = one_line(self.error.as_deref().unwrap_or("no error recorded"), 200);
        if self.action == "input" {
            format!("a human's message could not be delivered ({error}): {text}")
        } else {
            format!("a human's `{}` failed ({error})", self.action)
        }
    }
}

/// What the supervisor should hear about besides worker states.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct News {
    #[serde(default)]
    pub(crate) missions: Vec<MissionNote>,
    #[serde(default)]
    pub(crate) reports: Vec<ReportNote>,
    #[serde(default)]
    pub(crate) approved: Vec<ApprovedNote>,
    #[serde(default)]
    pub(crate) human: Vec<HumanNote>,
}

/// Everything delivery decides from, besides the per-session facts.
#[derive(Debug, Clone, Default)]
pub(crate) struct Work {
    pub(crate) tasks: Vec<TaskRow>,
    pub(crate) news: News,
    /// Retired residents, as (agent, session).
    pub(crate) retired: Vec<(String, String)>,
    /// Residents whose latest delivered task is cancelled.
    pub(crate) cancelled: Vec<Cancelled>,
    /// The latest `source.moved` record, if any.
    pub(crate) moved: Option<Moved>,
    /// The delivery waits (the defaults, unless the loop set its settings).
    pub(crate) waits: Waits,
}

impl Work {
    /// Read the mission layer's rows.
    pub(crate) fn load(store: &PostgresStore) -> Result<Self, String> {
        let error = |error: spawn_core::port::StoreError| error.to_string();
        Ok(Self {
            tasks: serde_json::from_value(store.delivery_rows().map_err(error)?)
                .map_err(|error| format!("delivery rows: {error}"))?,
            news: serde_json::from_value(store.mission_news().map_err(error)?)
                .map_err(|error| format!("mission news: {error}"))?,
            retired: store.retired_residents().map_err(error)?,
            cancelled: serde_json::from_value(store.cancelled_deliveries().map_err(error)?)
                .map_err(|error| format!("cancelled deliveries: {error}"))?,
            moved: serde_json::from_value(store.latest_source_moved().map_err(error)?)
                .map_err(|error| format!("latest source.moved: {error}"))?,
            waits: Waits::default(),
        })
    }

    /// Sessions whose tasks are in one of `states`.
    fn sessions_in(&self, states: &[TaskState]) -> BTreeSet<&str> {
        self.tasks
            .iter()
            .filter(|task| states.contains(&task.state))
            .map(|task| task.session_id.as_str())
            .collect()
    }

    /// Sessions whose open task (delivered, under review, waiting for the
    /// gate, or landing) keeps the next one waiting: a revision continues in
    /// the same context, and no other task is mixed in before it closes.
    pub(crate) fn blocked(&self) -> BTreeSet<&str> {
        let mut blocked = self.sessions_in(&[
            TaskState::Delivered,
            TaskState::Reported,
            TaskState::Landing,
        ]);
        blocked.extend(
            self.tasks
                .iter()
                .filter(|task| task.state == TaskState::Accepted && task.gated)
                .map(|task| task.session_id.as_str()),
        );
        // A resident not yet told that its latest task was cancelled gets
        // nothing else first.
        blocked.extend(
            self.cancelled
                .iter()
                .filter(|cancelled| !cancelled.told)
                .map(|cancelled| cancelled.session_id.as_str()),
        );
        blocked
    }

    /// Sessions with a task being worked on right now.
    pub(crate) fn in_flight(&self) -> BTreeSet<&str> {
        self.sessions_in(&[TaskState::Delivered])
    }

    /// Sessions the mission layer manages (they have recent tasks, or are
    /// residents).
    pub(crate) fn managed(&self, session: &str) -> bool {
        session.starts_with("session_resident_")
            || self.tasks.iter().any(|task| task.session_id == session)
    }
}

/// Which waiting task of each session gets its one delivery: a task sent
/// back goes first (it is the one the worker has in mind), then the oldest
/// new task. Tasks are offered in row order.
struct Turns<'a> {
    /// Sessions with a revision not offered yet.
    revisions: BTreeSet<&'a str>,
    /// Sessions whose turn is taken, by a revision or a new task.
    taken: BTreeSet<&'a str>,
}

impl<'a> Turns<'a> {
    fn new(work: &'a Work) -> Self {
        let revisions = work.sessions_in(&[TaskState::Revise]);
        Self {
            taken: revisions.clone(),
            revisions,
        }
    }

    /// Whether `task` is the one its session delivers now.
    fn take(&mut self, task: &'a TaskRow) -> bool {
        let session = task.session_id.as_str();
        if task.is_revision() {
            self.revisions.remove(session)
        } else {
            self.taken.insert(session)
        }
    }
}

/// Whether the last task typed into `facts`' session is closed. A question
/// the worker asked at the end of it (judged BLOCKED) is then settled by
/// the close, and new work may be typed in. The last task typed in is the
/// last delivery that reached the worker or may have
/// ([`Facts::last_delivery`]): a delivery refused before typing, such as a
/// waiting task's own first attempt, told the worker nothing, so it neither
/// is the question nor holds back its own retry.
fn last_delivery_closed(work: &Work, facts: &Facts) -> bool {
    // Not `dispatched`, which holds only the last day: a worker blocked
    // over a quiet weekend would never take new work.
    facts
        .last_delivery
        .as_deref()
        .and_then(|key| key.split('-').nth(1)?.parse::<i64>().ok())
        // Open tasks are always among the rows; a closed resident task is not.
        .is_some_and(|id| {
            !work
                .tasks
                .iter()
                .any(|task| task.id == id && !task.is_finished())
        })
}

/// How a waiting task reaches its worker, given the worker's facts.
/// `settled`: the worker's last task is closed (see [`last_delivery_closed`]).
fn delivery(task: &TaskRow, found: Option<&Facts>, settled: bool, key: String) -> Option<Order> {
    let model = Some(task.model);
    match found {
        // A worker whose stop is under way (a model switch's included) gets
        // nothing typed: the stop is pursued, and escalated if it keeps
        // failing ([`crate::dispatch`]), whatever its judgement says.
        Some(facts) if facts.open_command || facts.stopping() => None,
        // A revision answers the worker's own report, which may have asked
        // a question (judged BLOCKED): a stopped turn is enough. New work
        // waits for a worker that is not blocked, unless what it asked about
        // is a task that has closed since.
        Some(facts) if facts.live => {
            let ready = if task.is_revision() {
                facts.turn_stopped_idle()
            } else {
                facts.idle_at_prompt() || (settled && facts.turn_stopped_idle())
            };
            if !ready {
                return None;
            }
            if facts.model != task.model {
                // Stopped at the same moment typing would be safe. The stop
                // does not carry the task, which stays waiting: once the
                // stop has finished the worker is no longer live, and a
                // later pass resumes it, context kept, on the task's models,
                // with the resume that delivers the task.
                let key = format!("switch-model-{}-{}", task.id, task.revisions);
                return Some(task.command(key, CommandAction::Stop));
            }
            Some(task.delivery(
                key,
                CommandAction::Input {
                    text: typed(task),
                    tags: Vec::new(),
                },
            ))
        }
        // Ran before: resume, so a resident keeps its context.
        Some(facts) if facts.execution_id.is_some() => Some(task.delivery(
            key,
            CommandAction::Resume {
                prompt: Some(resume_prompt(task)),
                model,
            },
        )),
        _ => Some(task.delivery(
            key,
            CommandAction::Start {
                engine: ENGINE.to_string(),
                prompt: task_prompt(task),
                parent_session_id: None,
                from: None,
                model,
            },
        )),
    }
}

/// The attempts at a task's current delivery into its session: the
/// delivery's own command, then its retries (`<key>-retry-<n>`) and the
/// extra attempts deploys gave it once spent (`<key>-moved-<event>`), in
/// order.
fn delivery_attempts<'a>(task: &TaskRow, facts: &'a Facts) -> Vec<&'a crate::dispatch::Sent> {
    let key = task.delivery_key();
    let retry = format!("{key}-retry-");
    let moved = format!("{key}-moved-");
    facts
        .dispatched
        .iter()
        .filter(|sent| {
            sent.key == key || sent.key.starts_with(&retry) || sent.key.starts_with(&moved)
        })
        .collect()
}

/// Every attempt at `task`'s current delivery, as a re-issue's
/// cancellation names them.
fn attempt_records(task: &TaskRow, facts: &Facts) -> Vec<Attempt> {
    delivery_attempts(task, facts)
        .into_iter()
        .map(|sent| Attempt {
            command: sent.id,
            key: sent.key.clone(),
            error: sent.error.clone(),
        })
        .collect()
}

/// Where a waiting task's current delivery stands, from its attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attempts {
    /// Nothing sent for it yet.
    None,
    /// The latest attempt has not finished.
    Open,
    /// The latest attempt (this command) succeeded: the task reached its
    /// worker.
    Delivered(i64),
    /// The latest attempt failed, having typed nothing; attempt `number`
    /// goes at `due_ms`.
    Retry { number: usize, due_ms: i64 },
    /// The latest attempt failed and is not tried again: why, for the
    /// supervisor, whether its retries are spent (`false`: an attempt may
    /// have typed the task, so it is never tried again), and when the
    /// latest attempt finished. A spent one is re-issued if it is a first
    /// delivery ([`reissues`]), and tried once more after a deploy
    /// ([`moved_attempt`]).
    Failed {
        why: &'static str,
        spent: bool,
        last_ms: i64,
    },
}

/// Where `task`'s current delivery into `facts`' session stands. A failure
/// that typed nothing is tried again under a key of its own, with a wait
/// that doubles each time ([`Waits::retry_ms`] first), [`DELIVERY_RETRIES`]
/// times. One that may have typed the task ([`MAY_HAVE_TYPED`]) is not, and
/// neither is a delivery any of whose attempts may have. A refusal because
/// the pane was not ready ([`PANE_NOT_READY`]) is no failed attempt:
/// nothing was tried on the worker, so it goes again after
/// [`NOT_READY_RETRY_MS`], once the worker is ready by the delivery rules,
/// and counts only past [`NOT_READY_FREE`] of them. A deploy's extra
/// attempt (`-moved-`) is no retry: one that succeeds delivers the task,
/// one that fails leaves the delivery as spent as it was.
fn attempts(waits: &Waits, task: &TaskRow, facts: &Facts) -> Attempts {
    let sent = delivery_attempts(task, facts);
    let Some(latest) = sent.last() else {
        return Attempts::None;
    };
    match latest.succeeded {
        None => return Attempts::Open,
        Some(true) => return latest.id.map_or(Attempts::Open, Attempts::Delivered),
        Some(false) => {}
    }
    let last_ms = latest.finished_at_ms.unwrap_or(latest.created_at_ms);
    if sent.iter().any(|sent| {
        sent.error
            .as_deref()
            .is_some_and(|error| error.contains(MAY_HAVE_TYPED))
    }) {
        return Attempts::Failed {
            why: "may or may not have reached its worker; ask the worker before assigning it again",
            spent: false,
            last_ms,
        };
    }
    let moved = format!("{}-moved-", task.delivery_key());
    let sent: Vec<&crate::dispatch::Sent> = sent
        .into_iter()
        .filter(|sent| !sent.key.starts_with(&moved))
        .collect();
    let Some(last) = sent.last() else {
        return Attempts::None;
    };
    let not_ready = |sent: &&crate::dispatch::Sent| {
        sent.succeeded == Some(false)
            && sent
                .error
                .as_deref()
                .is_some_and(|error| error.contains(PANE_NOT_READY))
    };
    let refusals = sent.iter().filter(|sent| not_ready(sent)).count();
    // Failed attempts: every failure but the refusals still free.
    let failures = sent.len() - refusals.min(NOT_READY_FREE);
    let finished = last.finished_at_ms.unwrap_or(last.created_at_ms);
    if not_ready(last) && refusals <= NOT_READY_FREE {
        return Attempts::Retry {
            number: sent.len(),
            due_ms: finished.saturating_add(NOT_READY_RETRY_MS),
        };
    }
    if failures > DELIVERY_RETRIES {
        return Attempts::Failed {
            why: "never reached its worker; cancel it and assign it again",
            spent: true,
            last_ms,
        };
    }
    Attempts::Retry {
        number: sent.len(),
        due_ms: finished.saturating_add(waits.retry_wait_ms(failures)),
    }
}

/// Whether `task`'s spent delivery is re-issued now: a first delivery
/// ([`TaskRow::reissuable`]) whose retries are spent and none of whose
/// attempts may have typed, [`Waits::reissue_after_ms`] after its latest
/// attempt finished (the database's clock, as `now_ms`).
fn reissue_due(work: &Work, task: &TaskRow, attempts: Attempts, now_ms: i64) -> bool {
    matches!(attempts, Attempts::Failed { spent: true, last_ms, .. }
        if task.reissuable() && now_ms >= last_ms.saturating_add(work.waits.reissue_after_ms))
}

/// The key of the one more attempt a deploy gives `task`'s spent delivery,
/// if it is due: the latest `source.moved` record came after the
/// delivery's latest attempt finished, and no attempt was made for it yet
/// (`<key>-moved-<its event id>`). Rests on that record and the recorded
/// attempts, never on time alone.
fn moved_attempt(work: &Work, task: &TaskRow, facts: &Facts, last_ms: i64) -> Option<String> {
    let moved = work.moved?;
    if moved.at_ms < last_ms {
        return None;
    }
    let key = format!("{}-moved-{}", task.delivery_key(), moved.id);
    (!facts.dispatched.iter().any(|sent| sent.key == key)).then_some(key)
}

/// Why a delivery due now is not sent, if it is held: what [`delivery`]
/// waits for on the worker. `settled`: as there.
fn held(task: &TaskRow, found: Option<&Facts>, settled: bool) -> Option<String> {
    let facts = found?;
    if facts.open_command {
        return Some("a command is in flight in its session".to_string());
    }
    if facts.stopping() {
        return Some("its worker's stop is under way".to_string());
    }
    if !facts.live {
        // Started or resumed with the task: nothing to wait for.
        return None;
    }
    let ready = if task.is_revision() {
        facts.turn_stopped_idle()
    } else {
        facts.idle_at_prompt() || (settled && facts.turn_stopped_idle())
    };
    if ready {
        return None;
    }
    Some(
        match (facts.current_observation(), facts.observation.as_ref()) {
            (None, Some(latest)) => format!(
                "its worker has no judgement covering its latest fact (the latest, {}, is older)",
                latest.state.key()
            ),
            (None, None) => "its worker has no judgement yet".to_string(),
            (Some(current), _) if current.ambiguity != Some(Ambiguity::TurnStoppedIdle) => format!(
                "its worker is not idle at its prompt (judged {})",
                current.state.key()
            ),
            (Some(current), _) => format!(
                "its worker stopped at its prompt judged {} and the task it last got is still open",
                current.state.key()
            ),
        },
    )
}

/// What delivery did and does about one waiting task, for its stall
/// ([`crate::dispatch`]'s `stall`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeliveryStatus {
    /// Its retries are spent: it is re-issued, or [`Why::DeliveryFailed`]
    /// was its escalation.
    pub(crate) given_up: bool,
    /// Every attempt with its key and outcome (a failed one with its
    /// error), then what comes next: a retry due, or due and held and why,
    /// or none.
    pub(crate) line: String,
}

/// Where `task`'s delivery stands, at `now_ms` (the database's clock).
pub(crate) fn delivery_status(
    work: &Work,
    task: &TaskRow,
    found: Option<&Facts>,
    now_ms: i64,
) -> DeliveryStatus {
    let sent = found
        .map(|facts| delivery_attempts(task, facts))
        .unwrap_or_default();
    let mut parts = Vec::new();
    let mut last_error: Option<&str> = None;
    for attempt in &sent {
        parts.push(match attempt.succeeded {
            None => format!("`{}` in flight", attempt.key),
            Some(true) => format!("`{}` succeeded", attempt.key),
            Some(false) => {
                let error = attempt.error.as_deref().unwrap_or("no error recorded");
                let said = if last_error == Some(error) {
                    "the same error".to_string()
                } else {
                    one_line(error, 200)
                };
                last_error = Some(error);
                format!("`{}` failed ({said})", attempt.key)
            }
        });
    }
    let tried = if parts.is_empty() {
        "nothing sent yet".to_string()
    } else {
        format!("attempts: {}", parts.join("; "))
    };
    let settled = found.is_some_and(|facts| last_delivery_closed(work, facts));
    let mut given_up = false;
    let attempts = found.map_or(Attempts::None, |facts| attempts(&work.waits, task, facts));
    let next = match attempts {
        Attempts::None => held(task, found, settled).map_or_else(
            || "its first attempt goes on the next pass".to_string(),
            |why| format!("its first attempt is held: {why}"),
        ),
        Attempts::Open => "an attempt is in flight".to_string(),
        Attempts::Delivered(_) => {
            "the last attempt succeeded; the task is marked delivered on the next pass".to_string()
        }
        Attempts::Retry { number, due_ms } => {
            let key = format!("{}-retry-{number}", task.delivery_key());
            if now_ms < due_ms {
                format!(
                    "retry `{key}` due in {}s",
                    (due_ms - now_ms).saturating_add(999) / 1000
                )
            } else {
                held(task, found, settled).map_or_else(
                    || format!("retry `{key}` due now"),
                    |why| format!("retry `{key}` due and held: {why}"),
                )
            }
        }
        // A first delivery spent without typing is re-issued, not escalated.
        Attempts::Failed {
            spent: true,
            last_ms,
            ..
        } if task.reissuable() => {
            given_up = true;
            let due = last_ms.saturating_add(work.waits.reissue_after_ms);
            let when = if now_ms < due {
                format!("in {}s", (due - now_ms).saturating_add(999) / 1000)
            } else {
                "now".to_string()
            };
            format!(
                "not tried again: its retries are spent and nothing was typed; it is re-issued \
                 (cancelled and assigned again) {when}"
            )
        }
        Attempts::Failed { why, spent, .. } => {
            given_up = spent;
            format!("not tried again: it {why} (escalated as delivery failed)")
        }
    };
    DeliveryStatus {
        given_up,
        line: format!("{tried}; {next}"),
    }
}

/// Where each waiting task's delivery stands, with the task and its
/// session's facts.
fn waiting_attempts<'a, 'r>(
    work: &'a Work,
    rows: &'r [Facts],
) -> Vec<(&'a TaskRow, &'r Facts, Attempts)> {
    work.tasks
        .iter()
        .filter(|task| matches!(task.state, TaskState::Assigned | TaskState::Revise))
        .filter_map(|task| {
            let facts = rows
                .iter()
                .find(|facts| facts.session_id == task.session_id)?;
            Some((task, facts, attempts(&work.waits, task, facts)))
        })
        .collect()
}

/// Waiting tasks whose delivery command succeeded, as (task, command): the
/// task is marked delivered now, not when the command was submitted.
pub(crate) fn delivered(work: &Work, rows: &[Facts]) -> Vec<(i64, i64)> {
    waiting_attempts(work, rows)
        .into_iter()
        .filter_map(|(task, _, attempts)| match attempts {
            Attempts::Delivered(command) => Some((task.id, command)),
            _ => None,
        })
        .collect()
}

/// The tasks to re-issue now, at `now_ms` (the database's clock): first
/// deliveries whose retries are spent, none of whose attempts may have
/// typed, [`Waits::reissue_after_ms`] after their latest attempt finished
/// ([`reissue_due`]), each with every attempt of its delivery. Rests on the
/// recorded attempts and their outcomes; the re-issue (one cancellation and
/// one assignment naming each other) is itself recorded.
pub(crate) fn reissues(work: &Work, rows: &[Facts], now_ms: i64) -> Vec<Reissue> {
    waiting_attempts(work, rows)
        .into_iter()
        .filter(|(task, _, attempts)| reissue_due(work, task, *attempts, now_ms))
        .map(|(task, facts, _)| Reissue {
            task: task.id,
            attempts: attempt_records(task, facts),
        })
        .collect()
}

/// The waiting tasks (assigned, or sent back) that are their session's
/// turn ([`Turns`]: a revision first, then the oldest new task) with no
/// open task of the session holding it ([`Work::blocked`]): the ones
/// delivery tries now. Every other waiting task is queued behind one.
pub(crate) fn next_up(work: &Work) -> BTreeSet<i64> {
    let blocked = work.blocked();
    let mut turns = Turns::new(work);
    work.tasks
        .iter()
        .filter(|task| matches!(task.state, TaskState::Assigned | TaskState::Revise))
        .filter(|task| turns.take(task) && !blocked.contains(task.session_id.as_str()))
        .map(|task| task.id)
        .collect()
}

/// Delivery orders for the current facts: a task's delivery, its retry
/// once due ([`attempts`]), or the one more attempt a deploy gives a spent
/// delivery ([`moved_attempt`]) unless it is re-issued now ([`reissues`]).
pub(crate) fn orders(work: &Work, rows: &[Facts], now_ms: i64) -> Vec<Order> {
    let facts: BTreeMap<&str, &Facts> = rows
        .iter()
        .map(|facts| (facts.session_id.as_str(), facts))
        .collect();
    let next = next_up(work);
    let mut orders = Vec::new();
    for task in &work.tasks {
        let session = task.session_id.as_str();
        let found = facts.get(session).copied();
        match task.state {
            TaskState::Assigned | TaskState::Revise => {
                if !next.contains(&task.id) {
                    continue;
                }
                let attempts =
                    found.map_or(Attempts::None, |facts| attempts(&work.waits, task, facts));
                let key = match attempts {
                    Attempts::None => task.delivery_key(),
                    Attempts::Retry { number, due_ms } if now_ms >= due_ms => {
                        format!("{}-retry-{number}", task.delivery_key())
                    }
                    // Spent: after a deploy, tried once more, at once.
                    Attempts::Failed {
                        spent: true,
                        last_ms,
                        ..
                    } if !reissue_due(work, task, attempts, now_ms) => {
                        match found.and_then(|facts| moved_attempt(work, task, facts, last_ms)) {
                            Some(key) => key,
                            None => continue,
                        }
                    }
                    Attempts::Retry { .. }
                    | Attempts::Open
                    | Attempts::Delivered(_)
                    | Attempts::Failed { .. } => continue,
                };
                let settled = found.is_some_and(|facts| last_delivery_closed(work, facts));
                orders.extend(delivery(task, found, settled, key));
            }
            // A finished ephemeral worker is stopped, once: a stop that
            // fails is pursued from then on ([`crate::dispatch`]).
            _ if task.is_finished()
                && task.kind == "ephemeral"
                && found.is_some_and(|facts| facts.live_and_free() && !facts.stopping()) =>
            {
                let key = format!("retire-task-{}", task.id);
                orders.push(task.command(key, CommandAction::Stop));
            }
            TaskState::Accepted
            | TaskState::Cancelled
            | TaskState::Delivered
            | TaskState::Reported
            | TaskState::Landing
            | TaskState::Landed => {}
        }
    }
    for cancelled in work.cancelled.iter().filter(|cancelled| !cancelled.told) {
        if let Some(facts) = facts.get(cancelled.session_id.as_str()) {
            orders.extend(cancel_order(cancelled, facts, now_ms));
        }
    }
    for (agent, session) in &work.retired {
        if let Some(facts) = facts.get(session.as_str())
            && facts.live_and_free()
            && !facts.stopping()
        {
            orders.push(Order {
                agent_id: agent.clone(),
                session_id: session.clone(),
                key: format!(
                    "retire-{session}-{}",
                    facts.execution_id.as_deref().unwrap_or("none")
                ),
                action: CommandAction::Stop,
                task: None,
            });
        }
    }
    orders
}

/// The next step for a resident not yet told that its latest task was
/// cancelled: a live one is stopped (once per execution; a stop that fails is
/// pursued by [`crate::dispatch`]), one that no longer runs is resumed with
/// the notice, whose work is set aside first ([`crate::dispatch::step`]). A
/// notice that failed goes again under a key of its own, after a wait that
/// doubles ([`retry_wait_ms`]).
fn cancel_order(cancelled: &Cancelled, facts: &Facts, now_ms: i64) -> Option<Order> {
    if facts.open_command || facts.stopping() {
        return None;
    }
    let prefix = cancelled.notice_prefix();
    let attempts: Vec<&crate::dispatch::Sent> = facts
        .dispatched
        .iter()
        .filter(|sent| sent.key.starts_with(&prefix))
        .collect();
    let due = match attempts.last() {
        None => true,
        // Under way, or told (the log says so by the next pass): a worker
        // its notice resumed is not stopped again.
        Some(last) if last.succeeded != Some(false) => return None,
        Some(last) => {
            let finished = last.finished_at_ms.unwrap_or(last.created_at_ms);
            now_ms >= finished.saturating_add(retry_wait_ms(attempts.len()))
        }
    };
    if facts.live {
        let execution = facts.execution_id.as_deref().unwrap_or("none");
        let key = format!("cancel-stop-{}-{execution}", cancelled.id);
        return Some(cancelled.command(key, CommandAction::Stop));
    }
    if !due {
        return None;
    }
    facts.execution_id.as_ref()?;
    Some(cancelled.command(
        format!("{prefix}{}", attempts.len()),
        CommandAction::Resume {
            prompt: Some(cancel_notice(cancelled)),
            model: None,
        },
    ))
}

/// How every task asks to be reported on.
fn report_contract() -> String {
    format!(
        "When the task is finished, or you cannot go on, write `{REPORT}` \
at the root of your workspace with three sections: `## Outcome` (done, blocked or failed, in one \
line), `## What changed` (files, commits, results) and `## Open issues`. Then stop and wait. Do not \
start other work. If a person must decide something, say so in the report."
    )
}

/// Who a resident is, for every prompt that starts its session.
pub(crate) fn resident_role(name: &str, charter: &str) -> String {
    format!(
        "You are `{name}`, a long-lived resident worker of Spawn. Your charter:\n{charter}\n\n\
Tasks inside your charter arrive one at a time; keep what you learn about your domain."
    )
}

/// The full prompt that starts a task in a new or resumed session.
pub(crate) fn task_prompt(task: &TaskRow) -> String {
    let role = match (&task.resident, &task.charter) {
        (Some(name), Some(charter)) => resident_role(name, charter),
        _ => "You are a Spawn worker started for one task. Work only on it.".to_string(),
    };
    format!(
        "{role}\n\nMission #{}: {}\n\nTask #{}:\n{}\n\n{}",
        task.mission_id,
        task.mission_text,
        task.id,
        task.brief,
        report_contract()
    )
}

/// What a live worker gets typed for a task: a new task, or a revision. A
/// revision opens with a header no one can read as the answer to a question
/// the worker left open (it may be typed at a BLOCKED worker's prompt).
fn typed(task: &TaskRow) -> String {
    if task.is_revision() {
        format!(
            "{}\nTask #{} was sent back by the supervisor: {}\nFix it, then rewrite `{REPORT}`.",
            revision_header(task.id),
            task.id,
            task.review_note.as_deref().unwrap_or("see the review")
        )
    } else {
        format!(
            "New task #{} for mission #{}: {}\n\nTask:\n{}\n\n{}",
            task.id,
            task.mission_id,
            task.mission_text,
            task.brief,
            report_contract()
        )
    }
}

/// The first line of every typed revision.
fn revision_header(task: i64) -> String {
    format!(
        "[Review of your report on task #{task} — this is not an answer to any question you asked]"
    )
}

/// The prompt that resumes a session for a task; a revision also carries
/// the reviewer's note.
fn resume_prompt(task: &TaskRow) -> String {
    let prompt = task_prompt(task);
    if task.is_revision() {
        format!("{prompt}\n\n{}", typed(task))
    } else {
        prompt
    }
}

/// `text` on one line, cut to `limit` characters.
pub(crate) fn one_line(text: &str, limit: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= limit {
        flat
    } else {
        format!("{}…", flat.chars().take(limit).collect::<String>())
    }
}

/// Digest items from the mission layer: new missions, reports (once per
/// revision), approved residents, human messages, and deliveries whose
/// command failed, which leave a task delivered to nobody.
pub(crate) fn news_items(work: &Work, rows: &[Facts]) -> Vec<Escalation> {
    let news = &work.news;
    let mut items = Vec::new();
    for mission in &news.missions {
        items.push(Escalation {
            session_id: "missions".to_string(),
            id: format!("mission-{}", mission.id),
            why: Why::Mission,
            line: format!(
                "#{} from {}: {}",
                mission.id,
                mission.origin,
                one_line(&mission.text, 300)
            ),
        });
    }
    for report in &news.reports {
        items.push(Escalation {
            session_id: report.session_id.clone(),
            id: format!("report-{}-{}", report.id, report.revisions),
            why: Why::Report,
            // The report is the worker's text, not the dispatcher's: framed
            // as a quote, so an instruction in it reads as the worker's.
            // The review names the revision read here: one that arrives after
            // the task moved on is refused, not applied to a newer report.
            line: format!(
                "task #{id} revision {revision} ({}, mission #{}): `task accept --id {id} \
                 --revision {revision}` or `task reject --id {id} --revision {revision} --note \
                 ...`; the worker's report, quoted: «{}»",
                report.kind,
                report.mission_id,
                one_line(&report.report, 400),
                id = report.id,
                revision = report.revisions,
            ),
        });
    }
    for resident in &news.approved {
        let name = &resident.name;
        items.push(Escalation {
            session_id: format!("session_resident_{name}"),
            id: format!("approved-{name}"),
            why: Why::Approved,
            line: format!(
                "a human approved resident `{name}`; assign it tasks with --kind resident --worker {name}"
            ),
        });
    }
    for message in &news.human {
        items.push(Escalation {
            session_id: message.session_id.clone(),
            id: format!("human-{}", message.command_id),
            why: Why::Human,
            line: message.line(),
        });
    }
    for (task, facts, attempts) in waiting_attempts(work, rows) {
        let Attempts::Failed { why, spent, .. } = attempts else {
            continue;
        };
        // A first delivery spent without typing is re-issued instead.
        if spent && task.reissuable() {
            continue;
        }
        let line = match task.reissue_of {
            Some(old) if spent => reissue_spent(task, facts, old),
            _ => format!("task #{} {why}", task.id),
        };
        items.push(Escalation {
            session_id: task.session_id.clone(),
            id: task.delivery_key(),
            why: Why::DeliveryFailed,
            line,
        });
    }
    items
}

/// The escalation of a re-issue whose delivery is spent too: it names the
/// task and the task it re-issued, and the attempts of both.
fn reissue_spent(task: &TaskRow, facts: &Facts, old: i64) -> String {
    let own = attempt_records(task, facts);
    let before = task.reissued_attempts.as_deref().unwrap_or_default();
    format!(
        "task #{id} (a re-issue of task #{old}) never reached its worker; nor did task #{old} \
         before it, so nothing was typed and the host likely refuses it. Task #{id}'s attempts: \
         {}; task #{old}'s: {}. Fix the host: a deploy (`spawn mission moved`) tries it once \
         more; or cancel it",
        attempts_line(&own),
        attempts_line(before),
        id = task.id,
    )
}

/// `attempts` in one line: each run of attempts that failed with the same
/// error, with its first and last key, how many, and the error.
fn attempts_line(attempts: &[Attempt]) -> String {
    if attempts.is_empty() {
        return "none recorded".to_string();
    }
    let mut runs: Vec<(&Attempt, &Attempt, usize)> = Vec::new();
    for attempt in attempts {
        match runs.last_mut() {
            Some((_, last, count)) if last.error == attempt.error => {
                *last = attempt;
                *count += 1;
            }
            _ => runs.push((attempt, attempt, 1)),
        }
    }
    runs.iter()
        .map(|(first, last, count)| {
            let error = one_line(first.error.as_deref().unwrap_or("no error recorded"), 160);
            if *count == 1 {
                format!("`{}` failed ({error})", first.key)
            } else {
                format!(
                    "`{}` to `{}` ({count} attempts) failed ({error})",
                    first.key, last.key
                )
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Move a collected report to `.spawn/reports/task-<id>.md` under the
/// workspace, or delete it if it cannot be moved, so the next task starts
/// clean. Both inside the workspace only, never through a symlink
/// ([`spawn_host::agent_files`]).
fn put_aside(workspace: &Path, task: i64) {
    use spawn_host::agent_files::{remove, rename_within};
    let report = TreePath::from(REPORT);
    let kept = TreePath::from(format!("{KEPT_REPORTS}/task-{task}.md").as_str());
    if rename_within(workspace, &report, &kept).is_err() {
        let _ = remove(workspace, &report);
    }
}

/// The mark of the delivery `key`, relative to the workspace.
fn delivery_mark(key: &str) -> TreePath {
    TreePath::from(format!("{DELIVERY_MARKS}/{key}").as_str())
}

/// Mark in the worker's workspace under `root` that `order`, which carries
/// a task, is about to be sent: an empty file named by the delivery's key
/// (a retry's too), written once (a delivery planned or tried again keeps
/// its first mark). Its
/// modification time is set by the clock of the filesystem the worker writes
/// its report on, so [`report_is_new`] compares two times of one clock and
/// never a file's time with the database's.
///
/// A worker with no workspace yet gets no mark: the start or resume that
/// delivers the task mounts a fresh one, with no report in it.
pub(crate) fn mark_delivery(root: &Path, order: &Order) -> Result<(), String> {
    let workspace = root.join(&order.agent_id);
    if order.task.is_none() || !workspace.is_dir() {
        return Ok(());
    }
    // Made inside the workspace only: no directory or mark is followed
    // through a symlink the agent put there. One already there stays.
    let key = order.key.split("-retry-").next().unwrap_or(&order.key);
    let mark = delivery_mark(key);
    spawn_host::agent_files::create_new(&workspace, &mark)
        .map(|_| ())
        .map_err(|error| format!("mark {}: {error}", workspace.join(mark.as_path()).display()))
}

/// The most of a report [`new_report`] reads.
const REPORT_LIMIT: u64 = 1 << 20;

/// The report in `workspace` written for the delivery `key`, if there is
/// one: written at or after the delivery's mark, by the same filesystem
/// clock. Without a mark the workspace was mounted after the delivery was
/// sent (a start or resume mounts a fresh one), so everything in it is
/// newer. The agent writes both files, so each is opened the safe way
/// ([`spawn_host::agent_files`]): a symlink is not followed, a FIFO is not
/// waited on, the times are the opened files' own, and at most
/// [`REPORT_LIMIT`] bytes are read. A report that cannot be read that way
/// is never new.
fn new_report(workspace: &Path, key: &str) -> Option<String> {
    use spawn_host::agent_files::{open_regular, read_regular};
    let (bytes, metadata, _) = read_regular(workspace, &TreePath::from(REPORT), REPORT_LIMIT)
        .ok()
        .flatten()?;
    let written = metadata.modified().ok()?;
    let sent = match open_regular(workspace, &delivery_mark(key)) {
        Ok(None) => None,
        Ok(Some(mark)) => Some(mark.metadata.modified().ok()?),
        Err(_) => return None,
    };
    if sent.is_some_and(|sent| written < sent) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// Collect the reports of delivered tasks whose worker's turn stopped at an
/// idle prompt, judged waiting, completed or blocked alike: the report
/// contract asks for one when the task is done or the worker cannot go on.
/// Returns log lines. The file is moved to `.spawn/reports/task-<id>.md`.
/// The facts of `task`'s worker when a report of it may be collected now
/// ([`Facts::report_due`]): the turn that wrote it has ended in the log,
/// after its checkpoint, which the report then names.
fn report_due<'a>(task: &TaskRow, rows: &'a [Facts]) -> Option<&'a Facts> {
    rows.iter()
        .find(|facts| facts.session_id == task.session_id)
        .filter(|facts| facts.report_due())
}

pub(crate) fn collect_reports(
    store: &PostgresStore,
    work: &Work,
    rows: &[Facts],
    root: &Path,
) -> Vec<String> {
    let mut lines = Vec::new();
    for task in work
        .tasks
        .iter()
        .filter(|task| task.state == TaskState::Delivered)
    {
        let Some(facts) = report_due(task, rows) else {
            continue;
        };
        let workspace = root.join(&task.agent_id);
        let key = task.delivery_key();
        let Some(report) = new_report(&workspace, &key) else {
            continue;
        };
        if report.trim().is_empty() {
            continue;
        }
        match store.task_reported_with(task.id, report.trim(), facts.checkpoint.as_deref()) {
            Ok(true) => {
                put_aside(&workspace, task.id);
                let _ = spawn_host::agent_files::remove(&workspace, &delivery_mark(&key));
                lines.push(format!("deliver: task #{} reported", task.id));
            }
            Ok(false) => {}
            Err(error) => lines.push(format!("deliver: task #{} report: {error}", task.id)),
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::{
        Cancelled, DELIVERY_RETRIES, News, REPORT, TaskRow, Work, delivered, delivery_mark,
        mark_delivery, new_report, news_items, notice_task, put_aside, report_contract, report_due,
        task_prompt,
    };
    use crate::dispatch::{Facts, Order, Sent, Why};
    use serde_json::json;
    use spawn_core::command::CommandAction;
    use spawn_core::event::{ModelProfile, SessionSeq};
    use spawn_core::mission::TaskState;
    use spawn_core::observation::{Ambiguity, Confidence, Observation, SemanticState};

    fn task(id: i64, state: TaskState, kind: &str) -> TaskRow {
        TaskRow {
            id,
            mission_id: 1,
            kind: kind.to_string(),
            agent_id: "agent_resident_core".to_string(),
            session_id: "session_resident_core".to_string(),
            brief: format!("brief {id}"),
            state,
            revisions: 0,
            review_note: Some("tests fail".to_string()),
            mission_text: "Make ingest robust".to_string(),
            charter: Some("Owns spawn-core".to_string()),
            resident: Some("core".to_string()),
            gated: false,
            model: ModelProfile::Default,
            reissue_of: None,
            reissued_attempts: None,
        }
    }

    fn worker(live: bool, idle: bool) -> Facts {
        let mut observation = Observation::basic(
            if idle {
                SemanticState::Waiting
            } else {
                SemanticState::Working
            },
            None,
            SessionSeq(5),
            Confidence::new(0.9).expect("valid"),
            "jev",
        );
        if idle {
            observation.ambiguity = Some(Ambiguity::TurnStoppedIdle);
        }
        Facts {
            session_id: "session_resident_core".to_string(),
            agent_id: "agent_resident_core".to_string(),
            live,
            execution_id: Some("exec_3".to_string()),
            end_cause: None,
            observation: Some(observation),
            observation_event_id: Some(9),
            observation_at_ms: Some(0),
            evidence_seq: Some(5),
            episode: Some(1),
            stops: Vec::new(),
            open_command: false,
            dispatched: Vec::new(),
            told: std::collections::BTreeSet::new(),
            last_delivery: None,
            model: ModelProfile::Default,
            resumed_young: 0,
            turn_ended: idle,
            checkpoint: None,
        }
    }

    /// The orders of a pass at a time no retry is due yet.
    fn orders(work: &Work, rows: &[Facts]) -> Vec<Order> {
        super::orders(work, rows, 0)
    }

    /// The dispatcher's command `key` into the worker, finished at
    /// `finished`: succeeded, or failed with `error`.
    fn sent(id: i64, key: &str, finished: i64, error: Option<&str>) -> Sent {
        Sent {
            id: Some(id),
            key: key.to_string(),
            text: None,
            created_at_ms: finished - 1,
            finished_at_ms: Some(finished),
            succeeded: Some(error.is_none()),
            error: error.map(str::to_string),
        }
    }

    fn work(tasks: Vec<TaskRow>) -> Work {
        Work {
            tasks,
            ..Work::default()
        }
    }

    /// A worker at a ready screen, judged idle on it (`TurnStoppedIdle`),
    /// whose turn's end the transcript has not given yet: the log's latest
    /// turn event is the prompt (`message.received`).
    fn ready_screen_before_the_turn_end() -> Facts {
        let mut facts = worker(true, true);
        facts.turn_ended = false;
        facts.checkpoint = Some("c0".to_string());
        facts
    }

    #[test]
    fn no_report_is_collected_on_a_ready_screen_before_the_turns_end() {
        let task = task(7, TaskState::Delivered, "ephemeral");
        let facts = ready_screen_before_the_turn_end();
        assert!(facts.turn_stopped_idle(), "the screen alone says idle");
        assert!(
            report_due(&task, std::slice::from_ref(&facts)).is_none(),
            "the turn that wrote the report has not ended in the log: its checkpoint may not exist"
        );
    }

    #[test]
    fn a_report_is_collected_after_the_turns_end_with_its_checkpoint() {
        let task = task(7, TaskState::Delivered, "ephemeral");
        let mut facts = ready_screen_before_the_turn_end();
        // The Bridge recorded the turn's checkpoint, then its end: both are
        // newer than the idle judgement, which the Observer (no model judge,
        // the same verdict) does not write again.
        facts.turn_ended = true;
        facts.checkpoint = Some("c1".to_string());
        facts.evidence_seq = Some(8);
        assert!(
            facts.current_observation().is_none(),
            "the judgement is older"
        );
        let rows = [facts];
        let due = report_due(&task, &rows).expect("the turn ended: the report is collected");
        assert_eq!(
            due.checkpoint.as_deref(),
            Some("c1"),
            "the report names the checkpoint of its turn"
        );

        let mut busy = rows[0].clone();
        busy.observation = Some(Observation::basic(
            SemanticState::Working,
            None,
            SessionSeq(9),
            Confidence::new(0.9).expect("valid"),
            "rules",
        ));
        assert!(
            report_due(&task, &[busy]).is_none(),
            "a verdict that changed since holds it back"
        );
        let mut answered = rows[0].clone();
        if let Some(observation) = answered.observation.as_mut() {
            observation.fingerprint = Some("asked".to_string());
        }
        assert!(
            report_due(&task, &[answered]).is_none(),
            "a model's answer is written for every new situation: a stale one waits for the next"
        );
        let mut open = rows[0].clone();
        open.open_command = true;
        assert!(report_due(&task, &[open]).is_none(), "a command in flight");
        let mut gone = rows[0].clone();
        gone.live = false;
        assert!(report_due(&task, &[gone]).is_none(), "not live");
        let mut none = rows[0].clone();
        none.checkpoint = None;
        assert_eq!(
            report_due(&task, std::slice::from_ref(&none)).map(|facts| facts.checkpoint.clone()),
            Some(None),
            "an agent with no checkpoint at all reports a null commit"
        );
    }

    #[test]
    fn a_revision_reaches_a_blocked_worker_at_its_prompt_but_new_work_does_not() {
        let mut blocked = worker(true, true);
        if let Some(observation) = blocked.observation.as_mut() {
            observation.state = SemanticState::Blocked;
        }
        let revise = orders(
            &work(vec![task(7, TaskState::Revise, "resident")]),
            &[blocked.clone()],
        );
        assert_eq!(revise.len(), 1, "the revision is typed in");
        assert!(matches!(revise[0].action, CommandAction::Input { .. }));
        let new = orders(
            &work(vec![task(8, TaskState::Assigned, "resident")]),
            &[blocked],
        );
        assert!(
            new.is_empty(),
            "new work waits for a worker that is not blocked"
        );
    }

    #[test]
    fn new_work_reaches_a_worker_blocked_on_a_task_that_has_closed() {
        let mut blocked = worker(true, true);
        if let Some(observation) = blocked.observation.as_mut() {
            observation.state = SemanticState::Blocked;
        }
        // Delivered more than a day ago: out of `dispatched`'s window.
        blocked.last_delivery = Some("deliver-7-1".to_string());
        let waiting = |closed: TaskState| {
            orders(
                &work(vec![
                    task(7, closed, "resident"),
                    task(8, TaskState::Assigned, "resident"),
                ]),
                &[blocked.clone()],
            )
        };
        let plan = waiting(TaskState::Landed);
        assert_eq!(plan.len(), 1, "its question was about task 7, which landed");
        assert_eq!(plan[0].task, Some(8));
        assert!(matches!(plan[0].action, CommandAction::Input { .. }));
        assert!(
            waiting(TaskState::Revise)
                .iter()
                .all(|order| order.task != Some(8)),
            "task 7 is still open: no new work"
        );
        let gone = orders(
            &work(vec![task(8, TaskState::Assigned, "resident")]),
            &[blocked.clone()],
        );
        assert_eq!(
            gone.len(),
            1,
            "a landed resident task is not among the rows"
        );
    }

    #[test]
    fn a_new_worker_is_started_with_charter_brief_and_report_contract() {
        let plan = orders(&work(vec![task(4, TaskState::Assigned, "resident")]), &[]);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].task, Some(4));
        let CommandAction::Start { prompt, .. } = &plan[0].action else {
            panic!("a start");
        };
        assert!(prompt.contains("Owns spawn-core"));
        assert!(prompt.contains("brief 4"));
        assert!(prompt.contains(".spawn/report.md"));
    }

    #[test]
    fn a_resident_that_ran_before_is_resumed_not_restarted() {
        let plan = orders(
            &work(vec![task(4, TaskState::Assigned, "resident")]),
            &[worker(false, false)],
        );
        assert!(matches!(plan[0].action, CommandAction::Resume { .. }));
    }

    #[test]
    fn a_cheap_task_starts_and_resumes_its_worker_on_the_cheap_profile() {
        let cheap = TaskRow {
            model: ModelProfile::Cheap,
            ..task(4, TaskState::Assigned, "ephemeral")
        };
        let started = orders(&work(vec![cheap.clone()]), &[]);
        assert!(matches!(
            started[0].action,
            CommandAction::Start {
                model: Some(ModelProfile::Cheap),
                ..
            }
        ));
        let resumed = orders(&work(vec![cheap]), &[worker(false, false)]);
        assert!(matches!(
            resumed[0].action,
            CommandAction::Resume {
                model: Some(ModelProfile::Cheap),
                ..
            }
        ));
        // A default task names its profile too: a worker that last ran
        // cheap does not stay cheap for it.
        let plain = orders(
            &work(vec![task(5, TaskState::Assigned, "resident")]),
            &[worker(false, false)],
        );
        assert!(matches!(
            plain[0].action,
            CommandAction::Resume {
                model: Some(ModelProfile::Default),
                ..
            }
        ));
    }

    #[test]
    fn a_live_worker_on_another_profile_is_stopped_when_idle_then_resumed() {
        let cheap = TaskRow {
            model: ModelProfile::Cheap,
            ..task(4, TaskState::Assigned, "resident")
        };
        assert!(orders(&work(vec![cheap.clone()]), &[worker(true, false)]).is_empty());
        let plan = orders(&work(vec![cheap.clone()]), &[worker(true, true)]);
        assert_eq!(plan.len(), 1);
        assert!(matches!(plan[0].action, CommandAction::Stop));
        assert_eq!(plan[0].key, "switch-model-4-0");
        // On the task's profile already: typed in as usual.
        let on_cheap = Facts {
            model: ModelProfile::Cheap,
            ..worker(true, true)
        };
        let plan = orders(&work(vec![cheap]), &[on_cheap]);
        assert!(matches!(plan[0].action, CommandAction::Input { .. }));
    }

    #[test]
    fn a_model_switch_stops_the_worker_then_resumes_it_with_the_task() {
        let cheap = TaskRow {
            model: ModelProfile::Cheap,
            ..task(4, TaskState::Assigned, "resident")
        };
        let mut work = work(vec![cheap]);

        // Idle on the other profile: the stop, which carries no task.
        let plan = orders(&work, &[worker(true, true)]);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].action, CommandAction::Stop);
        assert_eq!(plan[0].task, None, "a stop delivers nothing");
        let mut idle = worker(true, true);
        idle.dispatched
            .push(sent(10, "switch-model-4-0", 100, None));
        assert!(delivered(&work, &[idle]).is_empty(), "still waiting");

        // The stop runs: nothing else goes to the session meanwhile.
        let stopping = Facts {
            open_command: true,
            ..worker(true, true)
        };
        assert!(orders(&work, &[stopping]).is_empty());

        // The stop finished: the resume on the task's profile delivers it.
        let stopped = worker(false, false);
        let plan = orders(&work, std::slice::from_ref(&stopped));
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].task, Some(4));
        assert_eq!(plan[0].key, "deliver-4-0");
        assert!(matches!(
            plan[0].action,
            CommandAction::Resume {
                model: Some(ModelProfile::Cheap),
                ..
            }
        ));
        // Submitted: nothing more while it runs, and the task still waits.
        let mut resuming = Facts {
            open_command: true,
            ..stopped
        };
        resuming.dispatched.push(Sent {
            finished_at_ms: None,
            succeeded: None,
            ..sent(11, "deliver-4-0", 200, None)
        });
        assert!(orders(&work, std::slice::from_ref(&resuming)).is_empty());
        assert!(delivered(&work, std::slice::from_ref(&resuming)).is_empty());
        // It succeeded: the task is delivered, by that command, once.
        let mut resumed = worker(true, false);
        resumed.dispatched.push(sent(11, "deliver-4-0", 200, None));
        assert_eq!(
            delivered(&work, std::slice::from_ref(&resumed)),
            vec![(4, 11)]
        );
        assert!(orders(&work, &[resumed]).is_empty(), "delivered once");
        work.tasks[0].state = TaskState::Delivered;
        assert!(orders(&work, &[worker(false, false)]).is_empty());
    }

    #[test]
    fn a_live_worker_gets_the_task_only_when_idle() {
        let tasks = vec![task(4, TaskState::Assigned, "resident")];
        assert!(orders(&work(tasks.clone()), &[worker(true, false)]).is_empty());
        let plan = orders(&work(tasks), &[worker(true, true)]);
        assert!(matches!(plan[0].action, CommandAction::Input { .. }));
    }

    #[test]
    fn one_task_in_flight_per_worker() {
        let tasks = vec![
            task(3, TaskState::Delivered, "resident"),
            task(4, TaskState::Assigned, "resident"),
        ];
        assert!(orders(&work(tasks), &[worker(true, true)]).is_empty());
        let queued = vec![
            task(4, TaskState::Assigned, "resident"),
            task(5, TaskState::Assigned, "resident"),
        ];
        let plan = orders(&work(queued), &[worker(true, true)]);
        assert_eq!(plan.len(), 1, "oldest first, one at a time");
        assert_eq!(plan[0].task, Some(4));
    }

    #[test]
    fn the_next_task_waits_for_the_review() {
        let tasks = vec![
            task(3, TaskState::Reported, "resident"),
            task(4, TaskState::Assigned, "resident"),
        ];
        assert!(orders(&work(tasks), &[worker(true, true)]).is_empty());
        let mut revise = task(3, TaskState::Revise, "resident");
        revise.revisions = 1;
        let tasks = vec![revise, task(4, TaskState::Assigned, "resident")];
        let plan = orders(&work(tasks), &[worker(true, true)]);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].task, Some(3), "the revision goes first");
    }

    #[test]
    fn a_task_sent_back_carries_the_note() {
        let mut revise = task(4, TaskState::Revise, "resident");
        revise.revisions = 1;
        let plan = orders(&work(vec![revise]), &[worker(true, true)]);
        assert_eq!(plan[0].key, "deliver-4-1");
        let CommandAction::Input { text, .. } = &plan[0].action else {
            panic!("typed");
        };
        assert!(text.contains("tests fail"));
        // Typed into a prompt that may hold an open question: it says first
        // that it is no answer.
        assert!(
            text.starts_with(
                "[Review of your report on task #4 — this is not an answer to any question you asked]\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn an_accepted_ephemeral_worker_is_stopped() {
        let mut done = task(4, TaskState::Accepted, "ephemeral");
        done.resident = None;
        let plan = orders(&work(vec![done]), &[worker(true, true)]);
        assert_eq!(plan[0].action, CommandAction::Stop);
        let resident = task(5, TaskState::Accepted, "resident");
        assert!(orders(&work(vec![resident]), &[worker(true, true)]).is_empty());
    }

    #[test]
    fn gated_work_keeps_its_worker_until_it_lands() {
        let mut accepted = task(4, TaskState::Accepted, "ephemeral");
        accepted.gated = true;
        let next = task(6, TaskState::Assigned, "ephemeral");
        let plan = orders(
            &work(vec![accepted.clone(), next.clone()]),
            &[worker(true, true)],
        );
        assert!(
            plan.is_empty(),
            "no stop, and no next task, before landing: {plan:?}"
        );

        let mut landing = accepted.clone();
        landing.state = TaskState::Landing;
        assert!(orders(&work(vec![landing, next.clone()]), &[worker(true, true)]).is_empty());

        let mut landed = accepted;
        landed.state = TaskState::Landed;
        let plan = orders(&work(vec![landed]), &[worker(true, true)]);
        assert_eq!(plan[0].action, CommandAction::Stop);
    }

    #[test]
    fn missions_reports_and_human_messages_become_digest_items() {
        let news: News = serde_json::from_value(json!({
            "missions": [{"id": 2, "text": "Ship it", "origin": "human"}],
            "reports": [{"id": 7, "mission_id": 2, "kind": "ephemeral", "revisions": 1,
                         "session_id": "session_worker_x_7", "report": "## Outcome\ndone"}],
            "approved": [{"name": "core"}],
            "human": [{"command_id": 40, "session_id": "session_worker_x_7", "text": "use eu"}],
        }))
        .expect("the shape mission_news returns");
        let items = news_items(
            &Work {
                news,
                ..Work::default()
            },
            &[],
        );
        let whys: Vec<Why> = items.iter().map(|item| item.why).collect();
        assert_eq!(
            whys,
            vec![Why::Mission, Why::Report, Why::Approved, Why::Human]
        );
        assert_eq!(
            items[1].tag(),
            "session_worker_x_7 report-7-1",
            "a report after a revision is news again"
        );
        assert!(items[1].line.contains("## Outcome done"));
        assert!(
            items[1].line.contains("`task accept --id 7 --revision 1`"),
            "the review names the revision it read: {}",
            items[1].line
        );
    }

    /// Audit #9: a human command is news once it finished, with what it
    /// did: one that failed told the worker nothing, and says so.
    #[test]
    fn a_human_command_that_failed_is_not_reported_as_said() {
        let news: News = serde_json::from_value(json!({
            "human": [
                {"command_id": 40, "session_id": "session_worker_x_7", "action": "input",
                 "text": "use eu", "succeeded": false,
                 "error": "pane state Busy does not accept input"},
                {"command_id": 41, "session_id": "session_worker_x_7", "action": "stop",
                 "text": "stop", "succeeded": false, "error": "kill-session refused"},
                {"command_id": 42, "session_id": "session_worker_x_7", "action": "input",
                 "text": "use us", "succeeded": true},
            ],
        }))
        .expect("the shape mission_news returns");
        let lines: Vec<String> = news_items(
            &Work {
                news,
                ..Work::default()
            },
            &[],
        )
        .into_iter()
        .map(|item| item.line)
        .collect();
        assert_eq!(
            lines,
            [
                "a human's message could not be delivered (pane state Busy does not accept \
                 input): use eu",
                "a human's `stop` failed (kill-session refused)",
                "a human told it: use us",
            ]
        );
    }

    /// The whole dispatcher pass for `work` and the resident's `facts`,
    /// with an idle supervisor, at `now`.
    fn pass(work: &Work, facts: &Facts, now: i64) -> crate::dispatch::Plan {
        let mut supervisor = worker(true, true);
        supervisor.session_id = "session_supervisor".to_string();
        supervisor.agent_id = "agent_supervisor".to_string();
        crate::dispatch::decide_with(&[facts.clone(), supervisor], work, &[], true, now, "spawn")
    }

    /// The switch Stop `key` of the live worker, as `dispatch_facts` lists
    /// it, failed at `finished`.
    fn failed_switch(key: &str, finished: i64) -> crate::dispatch::StopSent {
        crate::dispatch::StopSent {
            id: 10,
            key: key.to_string(),
            principal: "spawn-dispatch".to_string(),
            action: "stop".to_string(),
            created_at_ms: finished - 100,
            finished_at_ms: Some(finished),
            succeeded: Some(false),
            error: Some("terminate spawn-exec_3: kill-session refused".to_string()),
        }
    }

    /// Audit #2, GitHub issue #3, objective 006: a model switch's Stop that
    /// fails leaves the worker live on the wrong profile. The task stays
    /// waiting; the Stop is sent again under a new key once due, though the
    /// worker's judgement went stale with the Stop's own records (it changed
    /// nothing on the worker); after the retries no Stop goes out, and the
    /// supervisor hears, with the task named.
    #[test]
    fn a_failed_switch_stop_is_retried_under_a_new_key_then_escalated() {
        let cheap = TaskRow {
            model: ModelProfile::Cheap,
            ..task(4, TaskState::Assigned, "resident")
        };
        let work = work(vec![cheap]);
        let mut facts = worker(true, true);
        // The Stop's records are newer than the judgement.
        facts.evidence_seq = Some(8);
        facts.stops.push(failed_switch("switch-model-4-0", 1_000));
        facts.dispatched.push(sent(
            10,
            "switch-model-4-0",
            1_000,
            Some("kill-session refused"),
        ));

        let early = pass(&work, &facts, 1_000 + 59_999);
        assert!(early.orders.is_empty(), "{:?}", early.orders);
        assert!(
            early.pending.is_empty(),
            "a retry is coming: {:?}",
            early.pending
        );
        assert!(early.delivered.is_empty());
        let due = pass(&work, &facts, 1_000 + 60_000);
        assert_eq!(due.orders.len(), 1, "{:?}", due.orders);
        assert_eq!(due.orders[0].key, "stop-exec_3-retry-1");
        assert_eq!(due.orders[0].session_id, "session_resident_core");
        assert_eq!(due.orders[0].action, CommandAction::Stop);
        assert_eq!(due.orders[0].task, None, "a stop delivers nothing");

        // Every retry fails: at most four Stops in all, then none.
        let mut at = 70_000;
        for retry in 1..=DELIVERY_RETRIES {
            facts
                .stops
                .push(failed_switch(&format!("stop-exec_3-retry-{retry}"), at));
            at += 2 * 3_600_000;
        }
        assert_eq!(facts.stops.len(), 4);
        let spent = pass(&work, &facts, at);
        assert!(spent.delivered.is_empty(), "never delivered");
        assert_eq!(spent.orders.len(), 1, "only the digest: {:?}", spent.orders);
        assert_eq!(spent.orders[0].session_id, "session_supervisor");
        let CommandAction::Input { text, tags } = &spent.orders[0].action else {
            panic!("a digest is typed input");
        };
        assert_eq!(tags, &["session_resident_core stop-exec_3"]);
        assert!(text.contains("task #4"), "{text}");
        assert!(text.contains("stop failed"), "{text}");
        assert_eq!(spent.pending[0].why, Why::StopFailed);

        // Told: nothing more, and the task still waits.
        let told = pass(&work, &facts, at + 3_600_000);
        assert!(
            told.orders
                .iter()
                .all(|order| order.session_id != "session_resident_core"),
            "{:?}",
            told.orders
        );
        // A fresh idle judgement does not plan the switch again either.
        facts.evidence_seq = Some(5);
        assert!(super::orders(&work, &[facts], at).is_empty());
    }

    /// Before the fix the retry waited for the planned Stop's own key: the
    /// same key every pass, deduplicated to the finished command, forever.
    /// A switch Stop that succeeds still ends in the Resume that delivers.
    #[test]
    fn a_switch_stop_that_succeeds_leads_to_the_resume_that_delivers() {
        let cheap = TaskRow {
            model: ModelProfile::Cheap,
            ..task(4, TaskState::Assigned, "resident")
        };
        let work = work(vec![cheap]);
        let first = pass(&work, &worker(true, true), 0);
        assert_eq!(first.orders[0].key, "switch-model-4-0");
        // Stopped: no execution is live, so no stop is under way.
        let mut stopped = worker(false, false);
        stopped
            .dispatched
            .push(sent(10, "switch-model-4-0", 1_000, None));
        let resume = pass(&work, &stopped, 2_000);
        assert_eq!(resume.orders.len(), 1, "{:?}", resume.orders);
        assert_eq!(resume.orders[0].key, "deliver-4-0");
        assert_eq!(resume.orders[0].task, Some(4));
        assert!(matches!(
            resume.orders[0].action,
            CommandAction::Resume {
                model: Some(ModelProfile::Cheap),
                ..
            }
        ));
    }

    /// Audit #3: a closed task's worker, or a retired resident, whose Stop
    /// failed is not planned the same Stop again: the pursuit owns it.
    #[test]
    fn a_retirement_stop_is_sent_once_then_pursued() {
        let mut done = task(4, TaskState::Cancelled, "ephemeral");
        done.resident = None;
        let work = work(vec![done]);
        assert_eq!(orders(&work, &[worker(true, true)])[0].key, "retire-task-4");
        let mut failed = worker(true, true);
        failed.stops.push(failed_switch("retire-task-4", 1_000));
        assert!(orders(&work, &[failed.clone()]).is_empty());
        let plan = pass(&work, &failed, 1_000 + 60_000);
        assert_eq!(plan.orders[0].key, "stop-exec_3-retry-1");
        let retired = Work {
            retired: vec![(
                "agent_resident_core".to_string(),
                "session_resident_core".to_string(),
            )],
            ..Work::default()
        };
        assert_eq!(
            orders(&retired, &[worker(true, true)])[0].key,
            "retire-session_resident_core-exec_3"
        );
        assert!(orders(&retired, &[failed]).is_empty());
    }

    /// H1 and E: a delivery that failed typed nothing, so the task waits
    /// and the delivery is planned again under a key of its own, with a
    /// wait that doubles; once the retries are spent the supervisor hears.
    #[test]
    fn a_failed_delivery_waits_and_is_retried_then_escalated() {
        let tasks = work(vec![task(4, TaskState::Assigned, "resident")]);
        let mut facts = worker(true, true);
        facts
            .dispatched
            .push(sent(10, "deliver-4-0", 1_000, Some("terminal disappeared")));
        let rows = |facts: &Facts| vec![facts.clone()];
        assert!(delivered(&tasks, &rows(&facts)).is_empty(), "not delivered");
        assert!(
            news_items(&tasks, &rows(&facts)).is_empty(),
            "a retry is coming: no one is asked yet"
        );
        assert!(super::orders(&tasks, &rows(&facts), 1_000 + 59_999).is_empty());
        let retry = super::orders(&tasks, &rows(&facts), 1_000 + 60_000);
        assert_eq!(retry.len(), 1);
        assert_eq!(retry[0].key, "deliver-4-0-retry-1");
        assert_eq!(retry[0].task, Some(4), "the retry delivers the task");
        assert!(matches!(retry[0].action, CommandAction::Input { .. }));

        // The wait doubles, and the retry fits what the worker is now.
        facts.dispatched.push(sent(
            11,
            "deliver-4-0-retry-1",
            70_000,
            Some("terminal disappeared"),
        ));
        let mut down = facts.clone();
        down.live = false;
        assert!(super::orders(&tasks, &rows(&down), 70_000 + 119_999).is_empty());
        let retry = super::orders(&tasks, &rows(&down), 70_000 + 120_000);
        assert_eq!(retry[0].key, "deliver-4-0-retry-2");
        assert!(matches!(retry[0].action, CommandAction::Resume { .. }));

        // A retry that succeeds delivers the task, by its own command.
        let mut landed = facts.clone();
        landed
            .dispatched
            .push(sent(12, "deliver-4-0-retry-2", 300_000, None));
        assert_eq!(delivered(&tasks, &rows(&landed)), vec![(4, 12)]);

        // Once the retries are spent nothing is sent. A first delivery is
        // re-issued, not escalated; a re-issue's is: a person is asked.
        for retry in 2..=DELIVERY_RETRIES {
            facts.dispatched.push(sent(
                20,
                &format!("deliver-4-0-retry-{retry}"),
                100_000,
                Some("terminal disappeared"),
            ));
        }
        assert!(super::orders(&tasks, &rows(&facts), i64::MAX / 2).is_empty());
        assert!(news_items(&tasks, &rows(&facts)).is_empty(), "re-issued");
        let reissue = work(vec![TaskRow {
            reissue_of: Some(3),
            ..task(4, TaskState::Assigned, "resident")
        }]);
        assert!(super::orders(&reissue, &rows(&facts), i64::MAX / 2).is_empty());
        assert!(super::reissues(&reissue, &rows(&facts), i64::MAX / 2).is_empty());
        let items = news_items(&reissue, &rows(&facts));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].why, Why::DeliveryFailed);
        assert_eq!(items[0].tag(), "session_resident_core deliver-4-0");
    }

    /// The live sequence of task 25: the worker's turn on task 24 ended,
    /// task 25 was typed in at that instant and refused (the pane still
    /// showed the turn: `pane state Busy does not accept input`), and then
    /// the model judge read the idle worker's report on task 24, which asked
    /// a question, as BLOCKED. The retry must still go out: the refused
    /// delivery told the worker nothing, so its last delivery is task 24's,
    /// which is closed, and a stopped turn is enough.
    #[test]
    fn a_delivery_refused_by_a_busy_pane_is_retried_to_a_blocked_worker_at_its_prompt() {
        let tasks = work(vec![
            task(24, TaskState::Accepted, "resident"),
            task(25, TaskState::Assigned, "resident"),
        ]);
        let mut facts = worker(true, true);
        if let Some(observation) = facts.observation.as_mut() {
            observation.state = SemanticState::Blocked;
        }
        facts.dispatched = vec![
            sent(30, "deliver-24-0", 500, None),
            sent(
                31,
                "deliver-25-0",
                1_000,
                Some("pane state Busy does not accept input"),
            ),
        ];
        // What `dispatch_facts` names now: the refused delivery is not the
        // worker's.
        facts.last_delivery = Some("deliver-24-0".to_string());
        let rows = vec![facts.clone()];
        assert!(delivered(&tasks, &rows).is_empty(), "task 25 never arrived");
        assert!(
            news_items(&tasks, &rows).is_empty(),
            "a retry is coming: no one is asked"
        );
        assert!(super::orders(&tasks, &rows, 1_000 + 4_999).is_empty());
        let retry = super::orders(&tasks, &rows, 1_000 + 5_000);
        assert_eq!(retry.len(), 1, "the retry goes out: {retry:?}");
        assert_eq!(retry[0].key, "deliver-25-0-retry-1");
        assert_eq!(retry[0].task, Some(25));
        let CommandAction::Input { text, .. } = &retry[0].action else {
            panic!("typed into the live worker");
        };
        assert!(text.contains("New task #25"), "{text}");

        // As the facts named it before the fix: the refused delivery as the
        // worker's last, so task 25 itself was "the open task it asked
        // about", and the BLOCKED worker never got its retry.
        let mut before = facts.clone();
        before.last_delivery = Some("deliver-25-0".to_string());
        assert!(super::orders(&tasks, &[before], 1_000 + 600_000).is_empty());

        // The retry succeeds: task 25 is delivered by it.
        facts
            .dispatched
            .push(sent(32, "deliver-25-0-retry-1", 6_500, None));
        assert_eq!(delivered(&tasks, &[facts]), vec![(25, 32)]);
    }

    /// A pane not ready refuses before anything is typed: tried again soon,
    /// without spending the retries a real failure gets, and escalated only
    /// once refusals keep coming.
    #[test]
    fn a_pane_not_ready_is_not_a_failed_attempt() {
        let tasks = work(vec![task(4, TaskState::Assigned, "resident")]);
        let busy = "pane state Busy does not accept input";
        let mut facts = worker(true, true);
        let mut at = 1_000;
        for n in 0..5 {
            let key = if n == 0 {
                "deliver-4-0".to_string()
            } else {
                format!("deliver-4-0-retry-{n}")
            };
            facts.dispatched.push(sent(10 + n, &key, at, Some(busy)));
            let retry = super::orders(&tasks, &[facts.clone()], at + 5_000);
            assert_eq!(retry.len(), 1, "refusal {n} is retried after 5 s");
            assert_eq!(retry[0].key, format!("deliver-4-0-retry-{}", n + 1));
            at += 6_000;
        }
        // A real failure after them is the first failed attempt: one minute.
        facts.dispatched.push(sent(
            20,
            "deliver-4-0-retry-5",
            at,
            Some("terminal disappeared"),
        ));
        assert!(super::orders(&tasks, &[facts.clone()], at + 59_999).is_empty());
        assert_eq!(
            super::orders(&tasks, &[facts.clone()], at + 60_000)[0].key,
            "deliver-4-0-retry-6"
        );

        // Refusals without end count once past the free ones: spent, and
        // escalated (a revision's delivery is never re-issued).
        let mut stuck = worker(true, true);
        for n in 0..=(super::NOT_READY_FREE + DELIVERY_RETRIES) {
            let key = if n == 0 {
                "deliver-4-1".to_string()
            } else {
                format!("deliver-4-1-retry-{n}")
            };
            stuck.dispatched.push(sent(40, &key, 1_000, Some(busy)));
        }
        let revision = work(vec![TaskRow {
            revisions: 1,
            ..task(4, TaskState::Revise, "resident")
        }]);
        assert!(super::orders(&revision, &[stuck.clone()], i64::MAX / 2).is_empty());
        assert!(super::reissues(&revision, &[stuck.clone()], i64::MAX / 2).is_empty());
        let items = news_items(&revision, &[stuck]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].why, Why::DeliveryFailed);
    }

    /// A delivery that may have typed the task is not sent again: the
    /// supervisor is asked at once.
    #[test]
    fn a_delivery_that_may_have_typed_is_escalated_not_retried() {
        let tasks = work(vec![task(4, TaskState::Assigned, "resident")]);
        let mut facts = worker(true, true);
        facts.dispatched.push(sent(
            10,
            "deliver-4-0",
            1_000,
            Some(&format!("the text {}", spawn_core::bridge::MAY_HAVE_TYPED)),
        ));
        let rows = vec![facts];
        assert!(super::orders(&tasks, &rows, i64::MAX / 2).is_empty());
        let items = news_items(&tasks, &rows);
        assert_eq!(items[0].why, Why::DeliveryFailed);
        assert!(
            items[0].line.contains("may or may not"),
            "{}",
            items[0].line
        );
    }

    /// A retry keeps the delivery's first mark, which decides which report
    /// is new.
    #[test]
    fn a_retry_keeps_the_deliverys_mark() {
        let (root, workspace, order) = marked_workspace("retry");
        let mark = workspace.join(delivery_mark(&order.key).as_path());
        let first = std::fs::metadata(&mark).and_then(|meta| meta.modified());
        let retry = Order {
            key: format!("{}-retry-1", order.key),
            ..order
        };
        mark_delivery(&root, &retry).expect("mark");
        assert!(!workspace.join(delivery_mark(&retry.key).as_path()).exists());
        assert_eq!(
            std::fs::metadata(&mark)
                .and_then(|meta| meta.modified())
                .ok(),
            first.ok()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A root with one worker's workspace in it, its report written, and
    /// the delivery `deliver-4-0` to that worker marked.
    fn marked_workspace(name: &str) -> (std::path::PathBuf, std::path::PathBuf, Order) {
        let root = std::env::temp_dir().join(format!("spawn-report-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let workspace = root.join("agent_resident_core");
        std::fs::create_dir_all(workspace.join(".spawn")).expect("workspace");
        let order = Order {
            agent_id: "agent_resident_core".to_string(),
            session_id: "session_resident_core".to_string(),
            key: "deliver-4-0".to_string(),
            action: CommandAction::Stop,
            task: Some(4),
        };
        mark_delivery(&root, &order).expect("marked");
        std::fs::write(workspace.join(REPORT), "## Outcome\ndone").expect("report");
        (root, workspace, order)
    }

    fn set_modified(path: &std::path::Path, at: std::time::SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .and_then(|file| file.set_modified(at))
            .expect("set the modification time");
    }

    #[test]
    fn a_report_older_than_its_delivery_is_not_collected() {
        let (root, workspace, _) = marked_workspace("older");
        let report = workspace.join(REPORT);
        let mark = workspace.join(delivery_mark("deliver-4-0").as_path());
        let sent = std::time::SystemTime::now();
        set_modified(&mark, sent);
        set_modified(&report, sent - std::time::Duration::from_secs(10));
        assert_eq!(
            new_report(&workspace, "deliver-4-0"),
            None,
            "left over from before"
        );
        set_modified(&report, sent + std::time::Duration::from_secs(10));
        assert_eq!(
            new_report(&workspace, "deliver-4-0").as_deref(),
            Some("## Outcome\ndone")
        );
        std::fs::remove_file(&report).expect("remove the report");
        assert_eq!(new_report(&workspace, "deliver-4-0"), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_report_is_judged_by_its_filesystems_clock_never_the_databases() {
        let (root, workspace, order) = marked_workspace("skewed");
        let report = workspace.join(REPORT);
        let mark = workspace.join(delivery_mark(&order.key).as_path());
        let is_new = || new_report(&workspace, &order.key).is_some();
        // The host's clock is a day behind the database's (or ahead: the
        // database is never asked): the mark and the report share it.
        let host = std::time::SystemTime::now() - std::time::Duration::from_hours(24);
        set_modified(&mark, host);
        set_modified(&report, host + std::time::Duration::from_secs(5));
        assert!(is_new());
        // A delivery planned again keeps its first mark.
        mark_delivery(&root, &order).expect("marked again");
        let kept = std::fs::metadata(&mark).and_then(|meta| meta.modified());
        assert_eq!(kept.ok(), Some(host));
        assert!(is_new());
        // No mark: the workspace was mounted after the delivery.
        std::fs::remove_file(&mark).expect("remove the mark");
        assert!(is_new());
        // A worker with no workspace yet gets none, and no error.
        let unmounted = Order {
            agent_id: "agent_worker_new".to_string(),
            ..order
        };
        mark_delivery(&root, &unmounted).expect("nothing to mark");
        assert!(!root.join("agent_worker_new").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The agent writes the report and the marks: a report that is a
    /// symlink (to an owner's file) or a FIFO with no writer, and a mark
    /// whose directory is a symlink, are refused without reading through
    /// them or waiting; the report is put aside inside the workspace only.
    #[test]
    fn a_report_or_mark_the_agent_planted_is_refused_without_following_or_waiting() {
        let (root, workspace, order) = marked_workspace("planted");
        let report = workspace.join(REPORT);
        let secret = root.join("owner-secret.txt");
        std::fs::write(&secret, "owner only\n").expect("secret");
        std::fs::remove_file(&report).expect("remove the report");
        std::os::unix::fs::symlink(&secret, &report).expect("symlink");
        assert_eq!(new_report(&workspace, &order.key), None, "not followed");

        std::fs::remove_file(&report).expect("remove the symlink");
        let made = std::process::Command::new("mkfifo")
            .arg(&report)
            .status()
            .expect("mkfifo");
        assert!(made.success());
        let started = std::time::Instant::now();
        assert_eq!(
            new_report(&workspace, &order.key),
            None,
            "a FIFO is no report"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "the read waited on the FIFO"
        );
        std::fs::remove_file(&report).expect("remove the FIFO");

        // Marks whose directory is a symlink to a directory outside.
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).expect("outside");
        let marks = workspace.join(".spawn/deliveries");
        std::fs::remove_dir_all(&marks).expect("remove the marks");
        std::os::unix::fs::symlink(&outside, &marks).expect("symlink");
        assert!(mark_delivery(&root, &order).is_err(), "refused");
        assert_eq!(std::fs::read_dir(&outside).expect("outside").count(), 0);

        // A report put aside where `.spawn/reports` is a symlink outside:
        // removed, never moved through it.
        std::fs::write(&report, "## Outcome\ndone").expect("report");
        std::os::unix::fs::symlink(&outside, workspace.join(".spawn/reports")).expect("symlink");
        put_aside(&workspace, 4);
        assert!(!report.exists(), "the report is gone from its place");
        assert_eq!(std::fs::read_dir(&outside).expect("outside").count(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_report_contract_names_the_report_file() {
        assert!(report_contract().starts_with(
            "When the task is finished, or you cannot go on, write `.spawn/report.md` at the root"
        ));
    }

    #[test]
    fn an_ephemeral_prompt_has_no_charter() {
        let mut one = task(9, TaskState::Assigned, "ephemeral");
        one.resident = None;
        one.charter = None;
        assert!(task_prompt(&one).contains("started for one task"));
    }

    /// Resident `core`'s latest delivered task, 7, is cancelled.
    fn cancelled(told: bool) -> Cancelled {
        Cancelled {
            id: 7,
            mission_id: 1,
            agent_id: "agent_resident_core".to_string(),
            session_id: "session_resident_core".to_string(),
            start: Some("abc123".to_string()),
            set_aside: None,
            told,
            closed_by: Some("task.cancelled".to_string()),
            reason: None,
        }
    }

    /// Task 7 cancelled while delivered, task 8 waiting for the same worker.
    fn after_cancel(told: bool) -> Work {
        Work {
            tasks: vec![task(8, TaskState::Assigned, "resident")],
            cancelled: vec![cancelled(told)],
            ..Work::default()
        }
    }

    /// The worker in the middle of a turn.
    fn working() -> Facts {
        let mut facts = worker(true, false);
        facts.last_delivery = Some("deliver-7-0".to_string());
        facts
    }

    /// The worker after its execution ended.
    fn stopped() -> Facts {
        let mut facts = worker(false, false);
        facts.last_delivery = Some("deliver-7-0".to_string());
        facts
    }

    /// Objective 012: a resident in the middle of a turn whose latest task
    /// was cancelled is stopped at once (the Stop takes its final
    /// checkpoint), once per execution; its waiting task gets nothing. An
    /// idle one is stopped the same way: no turn is interrupted, and its
    /// notice comes with the resume.
    #[test]
    fn a_resident_working_on_a_cancelled_task_is_stopped_and_its_next_task_waits() {
        for facts in [working(), worker(true, true)] {
            let plan = orders(&after_cancel(false), &[facts]);
            assert_eq!(plan.len(), 1, "{plan:?}");
            assert_eq!(plan[0].key, "cancel-stop-7-exec_3");
            assert_eq!(plan[0].action, CommandAction::Stop);
            assert_eq!(plan[0].task, None, "the stop carries no task");
        }
    }

    /// Once it has stopped, the resident is resumed with the notice, which
    /// names the task and says it is cancelled; the waiting task is held
    /// until the notice succeeded, then typed in as usual.
    #[test]
    fn a_stopped_resident_is_told_before_its_waiting_task_which_waits_for_the_notice() {
        let plan = orders(&after_cancel(false), &[stopped()]);
        assert_eq!(plan.len(), 1, "only the notice: {plan:?}");
        assert_eq!(plan[0].key, "cancel-notice-7-0");
        assert_eq!(notice_task(&plan[0].key), Some(7));
        let CommandAction::Resume {
            prompt: Some(text),
            model: None,
        } = &plan[0].action
        else {
            panic!("a resume that carries the notice: {:?}", plan[0].action);
        };
        assert!(
            text.contains("Task #7 of mission #1 was cancelled"),
            "{text}"
        );
        assert!(text.contains("do not write a report"), "{text}");
        assert!(
            !text.contains("brief 7"),
            "the notice does not repeat the brief"
        );

        // The notice is under way, then failed: the waiting task still waits.
        let mut sending = stopped();
        sending.open_command = true;
        assert!(orders(&after_cancel(false), &[sending]).is_empty());
        let mut resumed = worker(true, true);
        resumed.dispatched.push(Sent {
            succeeded: None,
            finished_at_ms: None,
            ..sent(30, "cancel-notice-7-0", 1_000, None)
        });
        assert!(
            orders(&after_cancel(false), &[resumed]).is_empty(),
            "a live worker whose notice is open is neither stopped nor given the task"
        );
        let mut failed = stopped();
        failed
            .dispatched
            .push(sent(30, "cancel-notice-7-0", 1_000, Some("pane not ready")));
        assert!(super::orders(&after_cancel(false), &[failed.clone()], 1_001).is_empty());
        let retry = super::orders(&after_cancel(false), &[failed], 1_000 + 60_000);
        assert_eq!(retry.len(), 1);
        assert_eq!(
            retry[0].key, "cancel-notice-7-1",
            "a retry under a key of its own"
        );

        // Told: the waiting task goes to the idle worker.
        let plan = orders(&after_cancel(true), &[worker(true, true)]);
        assert_eq!(plan.len(), 1, "{plan:?}");
        assert_eq!(plan[0].task, Some(8));
        assert!(matches!(plan[0].action, CommandAction::Input { .. }));
    }

    /// Every pass after the first sends nothing new: a stop submitted (in
    /// flight, or being pursued), a notice in flight, a worker told.
    #[test]
    fn a_second_pass_after_a_cancel_sends_nothing_new() {
        let mut stopping = working();
        stopping.open_command = true;
        assert!(orders(&after_cancel(false), &[stopping]).is_empty());
        let mut pursued = working();
        pursued
            .stops
            .push(failed_switch("cancel-stop-7-exec_3", 1_000));
        assert!(orders(&after_cancel(false), &[pursued]).is_empty());
        let told = Work {
            cancelled: vec![cancelled(true)],
            ..Work::default()
        };
        for facts in [working(), stopped(), worker(true, true)] {
            assert!(orders(&told, &[facts]).is_empty(), "told: nothing more");
        }
    }

    /// A cancelled task that is not the worker's latest delivery (a later
    /// task was delivered since) is not among the cancelled deliveries: its
    /// worker is left alone. An ephemeral worker is only stopped, and a
    /// retired resident is stopped, never told.
    #[test]
    fn only_a_residents_latest_cancelled_delivery_is_told_and_others_are_only_stopped() {
        let mut later = worker(true, false);
        later.last_delivery = Some("deliver-9-0".to_string());
        let not_latest = Work {
            tasks: vec![
                task(7, TaskState::Cancelled, "resident"),
                task(9, TaskState::Delivered, "resident"),
            ],
            ..Work::default()
        };
        assert!(orders(&not_latest, &[later]).is_empty());

        let ephemeral = work(vec![task(4, TaskState::Cancelled, "ephemeral")]);
        let plan = orders(&ephemeral, &[working()]);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].key, "retire-task-4");
        assert_eq!(plan[0].action, CommandAction::Stop);
        assert!(orders(&ephemeral, &[stopped()]).is_empty(), "not told");

        let retired = Work {
            retired: vec![(
                "agent_resident_core".to_string(),
                "session_resident_core".to_string(),
            )],
            ..Work::default()
        };
        let plan = orders(&retired, &[working()]);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].action, CommandAction::Stop);
        assert!(
            orders(&retired, &[stopped()]).is_empty(),
            "not resumed to be told"
        );
    }

    /// An abort over budget says so in the notice.
    #[test]
    fn the_notice_of_an_abort_says_who_cancelled_and_why() {
        let mut aborted = cancelled(false);
        aborted.closed_by = Some("task.aborted".to_string());
        aborted.reason = Some("over budget: 5 hours".to_string());
        let text = super::cancel_notice(&aborted);
        assert!(
            text.contains("cancelled by the integrator (over budget: 5 hours)"),
            "{text}"
        );
    }

    // --- spent deliveries: re-issued, escalated, tried after a deploy -------

    /// The host's refusal of task 66 on 2026-11-06, as the Bridge reports a
    /// staging refused before typing to the dispatcher.
    const REFUSED: &str = "pane state Ready does not accept input: submit text refused before \
                           typing (set-buffer failed: command too long)";

    /// The waits a loop with `SPAWN_DELIVERY_RETRY_SECS=2` and
    /// `SPAWN_REISSUE_AFTER_SECS=10` reads.
    fn short_waits() -> super::Waits {
        super::Waits {
            retry_ms: 2_000,
            reissue_after_ms: 10_000,
        }
    }

    /// `facts` with the delivery `key` of a task refused before typing at
    /// every attempt until spent: the ten free refusals, then four counted,
    /// the last one finished at `last`.
    fn spent_facts(key: &str, last: i64) -> Facts {
        let mut facts = worker(true, true);
        let total = super::NOT_READY_FREE + DELIVERY_RETRIES + 1;
        for n in 0..total {
            let attempt = if n == 0 {
                key.to_string()
            } else {
                format!("{key}-retry-{n}")
            };
            let at = last - i64::try_from(total - 1 - n).expect("small") * 1_000;
            let id = 100 + i64::try_from(n).expect("small");
            facts.dispatched.push(sent(id, &attempt, at, Some(REFUSED)));
        }
        facts
    }

    fn with(tasks: Vec<TaskRow>, moved: Option<super::Moved>) -> Work {
        Work {
            tasks,
            moved,
            waits: short_waits(),
            ..Work::default()
        }
    }

    #[test]
    fn the_waits_are_read_from_the_environment_with_their_defaults() {
        let (waits, warnings) = super::Waits::read(|_| None);
        assert_eq!(waits, super::Waits::default());
        assert_eq!(waits.retry_ms, 60_000);
        assert_eq!(waits.reissue_after_ms, 300_000);
        assert!(warnings.is_empty());
        let (waits, warnings) = super::Waits::read(|name| match name {
            "SPAWN_DELIVERY_RETRY_SECS" => Some("2".to_string()),
            "SPAWN_REISSUE_AFTER_SECS" => Some(" 10 ".to_string()),
            _ => None,
        });
        assert_eq!(waits, short_waits());
        assert!(warnings.is_empty());
        let (waits, warnings) = super::Waits::read(|name| {
            (name == "SPAWN_REISSUE_AFTER_SECS").then(|| "soon".to_string())
        });
        assert_eq!(waits.reissue_after_ms, 300_000, "the default holds");
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("SPAWN_REISSUE_AFTER_SECS"),
            "{warnings:?}"
        );
    }

    /// The first counted retry waits `SPAWN_DELIVERY_RETRY_SECS`, doubling.
    #[test]
    fn a_counted_retry_waits_the_set_retry_wait_doubling() {
        let tasks = with(vec![task(4, TaskState::Assigned, "resident")], None);
        let mut facts = worker(true, true);
        facts
            .dispatched
            .push(sent(10, "deliver-4-0", 1_000, Some("terminal disappeared")));
        assert!(super::orders(&tasks, &[facts.clone()], 2_999).is_empty());
        assert_eq!(
            super::orders(&tasks, &[facts.clone()], 3_000)[0].key,
            "deliver-4-0-retry-1"
        );
        facts.dispatched.push(sent(
            11,
            "deliver-4-0-retry-1",
            5_000,
            Some("terminal disappeared"),
        ));
        assert!(super::orders(&tasks, &[facts.clone()], 8_999).is_empty());
        assert_eq!(
            super::orders(&tasks, &[facts], 9_000)[0].key,
            "deliver-4-0-retry-2"
        );
    }

    /// Task 66: a first delivery refused before typing at every attempt is
    /// not escalated; once `SPAWN_REISSUE_AFTER_SECS` have passed since its
    /// last attempt, not before, it is re-issued with every attempt.
    #[test]
    fn a_spent_first_delivery_is_not_escalated_and_is_reissued_after_the_wait() {
        let tasks = with(vec![task(66, TaskState::Assigned, "resident")], None);
        let facts = spent_facts("deliver-66-0", 50_000);
        let rows = [facts];
        assert!(news_items(&tasks, &rows).is_empty(), "not for a person");
        assert!(super::orders(&tasks, &rows, 59_999).is_empty(), "no retry");
        assert!(super::reissues(&tasks, &rows, 50_000 + 9_999).is_empty());
        let due = super::reissues(&tasks, &rows, 50_000 + 10_000);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].task, 66);
        let keys: Vec<&str> = due[0]
            .attempts
            .iter()
            .map(|attempt| attempt.key.as_str())
            .collect();
        assert_eq!(keys.len(), 14, "every attempt: {keys:?}");
        assert_eq!(keys[0], "deliver-66-0");
        assert_eq!(keys[13], "deliver-66-0-retry-13");
        assert_eq!(due[0].attempts[0].command, Some(100));
        assert_eq!(due[0].attempts[13].error.as_deref(), Some(REFUSED));
        assert!(
            super::orders(&tasks, &rows, 50_000 + 10_000).is_empty(),
            "the re-issue, not a delivery"
        );
        // The stall layer hears it is given up: no stall either.
        let status = super::delivery_status(&tasks, &tasks.tasks[0], rows.first(), 55_000);
        assert!(status.given_up);
        assert!(status.line.contains("re-issued"), "{}", status.line);
        assert!(status.line.contains("in 5s"), "{}", status.line);
        // Through the whole pass, with an idle supervisor.
        let plan = pass(&tasks, &rows[0], 50_000 + 10_000);
        assert_eq!(plan.reissues, due);
        assert!(plan.pending.is_empty(), "{:?}", plan.pending);
    }

    /// One whose attempt may have typed the task is escalated and never
    /// re-issued: the worker may have it.
    #[test]
    fn a_delivery_that_may_have_typed_is_escalated_and_not_reissued() {
        let tasks = with(vec![task(4, TaskState::Assigned, "resident")], None);
        let mut facts = spent_facts("deliver-4-0", 50_000);
        // An earlier attempt may have typed it, the later ones refused.
        facts.dispatched[3].error = Some(format!(
            "submit text: tmux died; the text {}",
            spawn_core::bridge::MAY_HAVE_TYPED
        ));
        let rows = [facts];
        assert!(super::reissues(&tasks, &rows, i64::MAX / 2).is_empty());
        assert!(super::orders(&tasks, &rows, i64::MAX / 2).is_empty());
        let items = news_items(&tasks, &rows);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].why, Why::DeliveryFailed);
        assert!(
            items[0].line.contains("may or may not"),
            "{}",
            items[0].line
        );
        let status = super::delivery_status(&tasks, &tasks.tasks[0], rows.first(), 60_000);
        assert!(!status.given_up, "its stall is escalated");
    }

    /// A re-issue whose delivery is spent too is escalated, naming the task
    /// and the task it re-issued with the attempts of both, and never
    /// re-issued again.
    #[test]
    fn a_spent_reissue_is_escalated_naming_both_tasks_and_not_reissued() {
        let before: Vec<super::Attempt> = (0..14)
            .map(|n| super::Attempt {
                command: Some(n),
                key: if n == 0 {
                    "deliver-3-0".to_string()
                } else {
                    format!("deliver-3-0-retry-{n}")
                },
                error: Some(REFUSED.to_string()),
            })
            .collect();
        let reissue = TaskRow {
            reissue_of: Some(3),
            reissued_attempts: Some(before),
            ..task(4, TaskState::Assigned, "resident")
        };
        let tasks = with(vec![reissue], None);
        let rows = [spent_facts("deliver-4-0", 50_000)];
        assert!(super::reissues(&tasks, &rows, i64::MAX / 2).is_empty());
        assert!(super::orders(&tasks, &rows, i64::MAX / 2).is_empty());
        let items = news_items(&tasks, &rows);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].why, Why::DeliveryFailed);
        assert_eq!(items[0].tag(), "session_resident_core deliver-4-0");
        let line = &items[0].line;
        assert!(line.starts_with("task #4 (a re-issue of task #3) never reached its worker"));
        assert!(
            line.contains("`deliver-4-0` to `deliver-4-0-retry-13` (14 attempts)"),
            "{line}"
        );
        assert!(
            line.contains("`deliver-3-0` to `deliver-3-0-retry-13` (14 attempts)"),
            "{line}"
        );
        assert!(line.contains("command too long"), "{line}");
        assert!(!line.contains('\n'));
        let status = super::delivery_status(&tasks, &tasks.tasks[0], rows.first(), 60_000);
        assert!(status.given_up, "escalated as delivery failed: no stall");
    }

    /// After a `source.moved` record every spent delivery gets one attempt,
    /// at once, keyed by the record's event id; none again for the same
    /// record, none for a record from before it was spent. A success
    /// delivers the task (its re-issue is then not made); a failure leaves
    /// it as it was, until the next record.
    #[test]
    fn after_a_deploy_a_spent_delivery_is_tried_once_more_under_its_moved_key() {
        let reissue = TaskRow {
            reissue_of: Some(3),
            ..task(4, TaskState::Assigned, "resident")
        };
        let facts = spent_facts("deliver-4-0", 50_000);
        let moved = |id, at_ms| Some(super::Moved { id, at_ms });
        // A deploy before the delivery was spent gives it nothing.
        let early = with(vec![reissue.clone()], moved(70, 40_000));
        assert!(super::orders(&early, &[facts.clone()], 60_000).is_empty());
        // One after it: one attempt at once, delivering the task.
        let tasks = with(vec![reissue.clone()], moved(90, 60_000));
        let tried = super::orders(&tasks, &[facts.clone()], 60_001);
        assert_eq!(tried.len(), 1, "{tried:?}");
        assert_eq!(tried[0].key, "deliver-4-0-moved-90");
        assert_eq!(tried[0].task, Some(4));
        assert!(matches!(tried[0].action, CommandAction::Input { .. }));

        // In flight: nothing more, and no re-issue for a first delivery.
        let mut flying = facts.clone();
        flying.dispatched.push(Sent {
            succeeded: None,
            finished_at_ms: None,
            ..sent(200, "deliver-4-0-moved-90", 60_002, None)
        });
        assert!(super::orders(&tasks, &[flying.clone()], 90_000).is_empty());
        let first = with(
            vec![task(4, TaskState::Assigned, "resident")],
            moved(90, 60_000),
        );
        assert!(super::reissues(&first, &[flying], i64::MAX / 2).is_empty());

        // Succeeded: the task is delivered by it; a pending re-issue is not made.
        let mut through = facts.clone();
        through
            .dispatched
            .push(sent(200, "deliver-4-0-moved-90", 62_000, None));
        assert_eq!(delivered(&first, &[through.clone()]), vec![(4, 200)]);
        assert!(super::reissues(&first, &[through], i64::MAX / 2).is_empty());

        // Failed: as it was. No attempt again for this record, no new
        // escalation; the re-issue of a first delivery still comes.
        let mut refused = facts.clone();
        refused
            .dispatched
            .push(sent(200, "deliver-4-0-moved-90", 62_000, Some(REFUSED)));
        assert!(super::orders(&tasks, &[refused.clone()], i64::MAX / 2).is_empty());
        let items = news_items(&tasks, &[refused.clone()]);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].tag(),
            "session_resident_core deliver-4-0",
            "the same item"
        );
        assert!(news_items(&first, &[refused.clone()]).is_empty());
        assert_eq!(super::reissues(&first, &[refused.clone()], 72_000).len(), 1);
        // The next deploy: one attempt more.
        let next = with(vec![reissue], moved(95, 63_000));
        assert_eq!(
            super::orders(&next, &[refused], 63_001)[0].key,
            "deliver-4-0-moved-95"
        );

        // A spent revision's delivery gets its attempt too; one that may have
        // typed does not.
        let revision = with(
            vec![TaskRow {
                revisions: 1,
                ..task(5, TaskState::Revise, "resident")
            }],
            moved(90, 60_000),
        );
        assert_eq!(
            super::orders(&revision, &[spent_facts("deliver-5-1", 50_000)], 60_001)[0].key,
            "deliver-5-1-moved-90"
        );
        let mut unsure = spent_facts("deliver-4-0", 50_000);
        unsure.dispatched[13].error = Some(spawn_core::bridge::MAY_HAVE_TYPED.to_string());
        assert!(super::orders(&tasks, &[unsure], 60_001).is_empty());
    }

    /// A deploy recorded while a first delivery waits for its re-issue: the
    /// re-issue, once due, goes first, and no extra attempt beside it.
    #[test]
    fn a_due_reissue_takes_the_place_of_the_deploys_attempt() {
        let tasks = with(
            vec![task(4, TaskState::Assigned, "resident")],
            Some(super::Moved {
                id: 90,
                at_ms: 55_000,
            }),
        );
        let rows = [spent_facts("deliver-4-0", 50_000)];
        assert_eq!(
            super::orders(&tasks, &rows, 55_001)[0].key,
            "deliver-4-0-moved-90",
            "before the re-issue is due"
        );
        assert!(super::orders(&tasks, &rows, 60_000).is_empty());
        assert_eq!(super::reissues(&tasks, &rows, 60_000).len(), 1);
    }
}
