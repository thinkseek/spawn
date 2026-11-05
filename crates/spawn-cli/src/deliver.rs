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
//! - a report (`.spawn/report.md`) is collected once its worker is idle, if
//!   it was written after the delivery's mark in the workspace (one clock,
//!   the filesystem's, sets both times), and moved aside so the next task
//!   starts clean;
//! - an ephemeral worker whose task was accepted or cancelled is stopped, and
//!   so is a retired resident. In a mission with a gate, acceptance is not
//!   the end: the worker's workspace is the candidate, so it stays (and its
//!   next task waits) until the work lands or the gate sends it back.
//!
//! Missions, reports, approvals, human messages to workers and failed
//! deliveries become digest items for the supervisor ([`news_items`]). Like
//! the dispatcher, delivery keeps no state: idempotency keys and the task
//! rows say what was already done.
//!
//! [`orders`] is pure; [`collect_reports`] is the one step with side effects
//! (the report file and the task row).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Deserialize;
use spawn_core::bridge::{MAY_HAVE_TYPED, PANE_NOT_READY};
use spawn_core::command::CommandAction;
use spawn_core::event::{ENGINE, ModelProfile};
use spawn_core::mission::TaskState;
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

/// Retries of a failed delivery, after the first attempt.
const DELIVERY_RETRIES: usize = 3;

/// Wait before the first retry; it doubles with every retry, up to
/// [`RETRY_MAX_MS`].
const RETRY_MS: i64 = 60_000;

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

/// Something a human typed to a worker.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct HumanNote {
    pub(crate) command_id: i64,
    pub(crate) session_id: String,
    #[serde(default)]
    pub(crate) text: String,
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
        Some(facts) if facts.open_command => None,
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
/// delivery's own command, then its retries (`<key>-retry-<n>`), in order.
fn delivery_attempts<'a>(task: &TaskRow, facts: &'a Facts) -> Vec<&'a crate::dispatch::Sent> {
    let key = task.delivery_key();
    let retry = format!("{key}-retry-");
    facts
        .dispatched
        .iter()
        .filter(|sent| sent.key == key || sent.key.starts_with(&retry))
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
    /// supervisor.
    Failed(&'static str),
}

/// Where `task`'s current delivery into `facts`' session stands. A failure
/// that typed nothing is tried again under a key of its own, with a wait
/// that doubles each time, [`DELIVERY_RETRIES`] times. One that may have
/// typed the task ([`MAY_HAVE_TYPED`]) is not. A refusal because the pane
/// was not ready ([`PANE_NOT_READY`]) is no failed attempt: nothing was
/// tried on the worker, so it goes again after [`NOT_READY_RETRY_MS`], once
/// the worker is ready by the delivery rules, and counts only past
/// [`NOT_READY_FREE`] of them.
fn attempts(task: &TaskRow, facts: &Facts) -> Attempts {
    let sent = delivery_attempts(task, facts);
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
    match last.succeeded {
        None => Attempts::Open,
        Some(true) => last.id.map_or(Attempts::Open, Attempts::Delivered),
        Some(false) => {
            let error = last.error.as_deref().unwrap_or_default();
            if error.contains(MAY_HAVE_TYPED) {
                return Attempts::Failed(
                    "may or may not have reached its worker; ask the worker before assigning it \
                     again",
                );
            }
            let finished = last.finished_at_ms.unwrap_or(last.created_at_ms);
            if not_ready(last) && refusals <= NOT_READY_FREE {
                return Attempts::Retry {
                    number: sent.len(),
                    due_ms: finished.saturating_add(NOT_READY_RETRY_MS),
                };
            }
            if failures > DELIVERY_RETRIES {
                return Attempts::Failed("never reached its worker; cancel it and assign it again");
            }
            let doublings = u32::try_from(failures.saturating_sub(1))
                .unwrap_or(u32::MAX)
                .min(20);
            let wait = RETRY_MS
                .saturating_mul(1_i64 << doublings)
                .min(RETRY_MAX_MS);
            Attempts::Retry {
                number: sent.len(),
                due_ms: finished.saturating_add(wait),
            }
        }
    }
}

/// Where each waiting task's delivery stands, with the task.
fn waiting_attempts<'a>(work: &'a Work, rows: &[Facts]) -> Vec<(&'a TaskRow, Attempts)> {
    work.tasks
        .iter()
        .filter(|task| matches!(task.state, TaskState::Assigned | TaskState::Revise))
        .filter_map(|task| {
            let facts = rows
                .iter()
                .find(|facts| facts.session_id == task.session_id)?;
            Some((task, attempts(task, facts)))
        })
        .collect()
}

/// Waiting tasks whose delivery command succeeded, as (task, command): the
/// task is marked delivered now, not when the command was submitted.
pub(crate) fn delivered(work: &Work, rows: &[Facts]) -> Vec<(i64, i64)> {
    waiting_attempts(work, rows)
        .into_iter()
        .filter_map(|(task, attempts)| match attempts {
            Attempts::Delivered(command) => Some((task.id, command)),
            _ => None,
        })
        .collect()
}

/// Delivery orders for the current facts: a task's delivery, or its retry
/// once due ([`attempts`]).
pub(crate) fn orders(work: &Work, rows: &[Facts], now_ms: i64) -> Vec<Order> {
    let facts: BTreeMap<&str, &Facts> = rows
        .iter()
        .map(|facts| (facts.session_id.as_str(), facts))
        .collect();
    let blocked = work.blocked();
    let mut turns = Turns::new(work);
    let mut orders = Vec::new();
    for task in &work.tasks {
        let session = task.session_id.as_str();
        let found = facts.get(session).copied();
        match task.state {
            TaskState::Assigned | TaskState::Revise => {
                let turn = turns.take(task);
                if !turn || blocked.contains(session) {
                    continue;
                }
                let key = match found.map_or(Attempts::None, |facts| attempts(task, facts)) {
                    Attempts::None => task.delivery_key(),
                    Attempts::Retry { number, due_ms } if now_ms >= due_ms => {
                        format!("{}-retry-{number}", task.delivery_key())
                    }
                    Attempts::Retry { .. }
                    | Attempts::Open
                    | Attempts::Delivered(_)
                    | Attempts::Failed(_) => continue,
                };
                let settled = found.is_some_and(|facts| last_delivery_closed(work, facts));
                orders.extend(delivery(task, found, settled, key));
            }
            // A finished ephemeral worker is stopped.
            _ if task.is_finished()
                && task.kind == "ephemeral"
                && found.is_some_and(Facts::live_and_free) =>
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
    for (agent, session) in &work.retired {
        if let Some(facts) = facts.get(session.as_str())
            && facts.live_and_free()
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
fn one_line(text: &str, limit: usize) -> String {
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
            line: format!("a human told it: {}", one_line(&message.text, 200)),
        });
    }
    for (task, attempts) in waiting_attempts(work, rows) {
        if let Attempts::Failed(why) = attempts {
            items.push(Escalation {
                session_id: task.session_id.clone(),
                id: task.delivery_key(),
                why: Why::DeliveryFailed,
                line: format!("task #{} {why}", task.id),
            });
        }
    }
    items
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
        let idle = rows
            .iter()
            .find(|facts| facts.session_id == task.session_id)
            .is_some_and(Facts::turn_stopped_idle);
        if !idle {
            continue;
        }
        let workspace = root.join(&task.agent_id);
        let key = task.delivery_key();
        let Some(report) = new_report(&workspace, &key) else {
            continue;
        };
        if report.trim().is_empty() {
            continue;
        }
        match store.task_reported(task.id, report.trim()) {
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
        DELIVERY_RETRIES, News, REPORT, TaskRow, Work, delivered, delivery_mark, mark_delivery,
        new_report, news_items, put_aside, report_contract, task_prompt,
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
            episode_action: Some("start".to_string()),
            open_command: false,
            dispatched: Vec::new(),
            told: std::collections::BTreeSet::new(),
            last_delivery: None,
            model: ModelProfile::Default,
            resumed_young: 0,
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

        // Once the retries are spent, a person is asked, and nothing is sent.
        for retry in 2..=DELIVERY_RETRIES {
            facts.dispatched.push(sent(
                20,
                &format!("deliver-4-0-retry-{retry}"),
                100_000,
                Some("terminal disappeared"),
            ));
        }
        assert!(super::orders(&tasks, &rows(&facts), i64::MAX / 2).is_empty());
        let items = news_items(&tasks, &rows(&facts));
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

        // Refusals without end count once past the free ones: escalated.
        let mut stuck = worker(true, true);
        for n in 0..=(super::NOT_READY_FREE + DELIVERY_RETRIES) {
            let key = if n == 0 {
                "deliver-4-0".to_string()
            } else {
                format!("deliver-4-0-retry-{n}")
            };
            stuck.dispatched.push(sent(40, &key, 1_000, Some(busy)));
        }
        assert!(super::orders(&tasks, &[stuck.clone()], i64::MAX / 2).is_empty());
        let items = news_items(&tasks, &[stuck]);
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
}
