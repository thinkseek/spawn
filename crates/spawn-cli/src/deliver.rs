//! Delivery: the dispatcher's half of the mission layer.
//!
//! The supervisor decides tasks (`spawn task assign`); delivery only carries
//! them out, by rule:
//!
//! - one task in flight per worker, oldest first;
//! - a worker that never ran is started, one that ran before is resumed (its
//!   context is the point of a resident), a live one gets the task typed in,
//!   but only while it is idle at its prompt;
//! - a task sent back is delivered again with the reviewer's note;
//! - a report (`.spawn/report.md`) is collected once its worker is idle, and
//!   moved aside so the next task starts clean;
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
use spawn_core::command::CommandAction;
use spawn_core::event::Engine;
use spawn_core::mission::TaskState;
use spawn_postgres::PostgresStore;

use crate::dispatch::{Escalation, Facts, Order, Why};

/// Where a worker writes its report, relative to its workspace.
pub(crate) const REPORT: &str = ".spawn/report.md";

/// Where collected reports are kept, relative to the workspace.
const KEPT_REPORTS: &str = ".spawn/reports";

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
    /// When the current delivery was recorded; a report older than it is
    /// left over from before.
    #[serde(default)]
    pub(crate) delivered_at_ms: Option<i64>,
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

    /// A command to this task's session that delivers or retires it.
    fn order(&self, key: String, action: CommandAction) -> Order {
        Order {
            agent_id: self.agent_id.clone(),
            session_id: self.session_id.clone(),
            key,
            action,
            task: Some(self.id),
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

/// How a waiting task reaches its worker, given the worker's facts.
fn delivery(task: &TaskRow, found: Option<&Facts>) -> Option<Order> {
    let key = task.delivery_key();
    match found {
        Some(facts) if facts.open_command => None,
        Some(facts) if facts.live => facts
            .idle_at_prompt()
            .then(|| task.order(key, CommandAction::Input { text: typed(task) })),
        // Ran before: resume, so a resident keeps its context.
        Some(facts) if facts.execution_id.is_some() => Some(task.order(
            key,
            CommandAction::Resume {
                prompt: Some(resume_prompt(task)),
            },
        )),
        _ => Some(task.order(
            key,
            CommandAction::Start {
                engine: Engine::Claude,
                prompt: task_prompt(task),
                parent_session_id: None,
            },
        )),
    }
}

/// Delivery orders for the current facts.
pub(crate) fn orders(work: &Work, rows: &[Facts]) -> Vec<Order> {
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
                if turn && !blocked.contains(session) {
                    orders.extend(delivery(task, found));
                }
            }
            // A finished ephemeral worker is stopped.
            _ if task.is_finished()
                && task.kind == "ephemeral"
                && found.is_some_and(Facts::live_and_free) =>
            {
                let key = format!("retire-task-{}", task.id);
                orders.push(task.order(key, CommandAction::Stop));
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

/// What a live worker gets typed for a task: a new task, or a revision.
fn typed(task: &TaskRow) -> String {
    if task.is_revision() {
        format!(
            "Task #{} was sent back by the supervisor: {}\nFix it, then rewrite `{REPORT}`.",
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
            line: format!(
                "task #{} ({}, mission #{}): {}",
                report.id,
                report.kind,
                report.mission_id,
                one_line(&report.report, 400)
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
    for task in work
        .tasks
        .iter()
        .filter(|task| task.state == TaskState::Delivered)
    {
        let key = task.delivery_key();
        let failed = rows
            .iter()
            .find(|facts| facts.session_id == task.session_id)
            .and_then(|facts| facts.sent(&key))
            .is_some_and(|sent| sent.succeeded == Some(false));
        if failed {
            items.push(Escalation {
                session_id: task.session_id.clone(),
                id: key,
                why: Why::DeliveryFailed,
                line: format!(
                    "task #{} never reached its worker; cancel it and assign it again",
                    task.id
                ),
            });
        }
    }
    items
}

/// Move a collected report to `.spawn/reports/task-<id>.md` under the
/// workspace, or delete it if it cannot be moved, so the next task starts
/// clean.
fn put_aside(path: &Path, workspace: &Path, task: i64) {
    let kept = workspace.join(KEPT_REPORTS).join(format!("task-{task}.md"));
    let moved = kept
        .parent()
        .map(std::fs::create_dir_all)
        .and_then(Result::ok)
        .and_then(|()| std::fs::rename(path, &kept).ok());
    if moved.is_none() {
        let _ = std::fs::remove_file(path);
    }
}

/// Whether the file at `path` was written at or after `since_ms`; a report
/// older than the delivery belongs to something else.
fn written_since(path: &Path, since_ms: Option<i64>) -> bool {
    let Ok(modified) = std::fs::metadata(path).and_then(|meta| meta.modified()) else {
        return false;
    };
    let modified_ms = modified
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        });
    since_ms.is_none_or(|since| modified_ms >= since)
}

/// Collect the reports of delivered tasks whose worker is idle. Returns log
/// lines. The file is moved to `.spawn/reports/task-<id>.md`.
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
            .is_some_and(Facts::idle_at_prompt);
        if !idle {
            continue;
        }
        let workspace = root.join(&task.agent_id);
        let path = workspace.join(REPORT);
        if !written_since(&path, task.delivered_at_ms) {
            continue;
        }
        let Ok(report) = std::fs::read_to_string(&path) else {
            continue;
        };
        if report.trim().is_empty() {
            continue;
        }
        match store.task_reported(task.id, report.trim()) {
            Ok(true) => {
                put_aside(&path, &workspace, task.id);
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
        News, TaskRow, Work, news_items, orders, report_contract, task_prompt, written_since,
    };
    use crate::dispatch::{Facts, Sent, Why};
    use serde_json::json;
    use spawn_core::command::CommandAction;
    use spawn_core::event::SessionSeq;
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
            delivered_at_ms: None,
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
        }
    }

    fn work(tasks: Vec<TaskRow>) -> Work {
        Work {
            tasks,
            ..Work::default()
        }
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
        let CommandAction::Input { text } = &plan[0].action else {
            panic!("typed");
        };
        assert!(text.contains("tests fail"));
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
            items[1].marker(),
            "[spawn:esc session_worker_x_7 report-7-1]",
            "a report after a revision is news again"
        );
        assert!(items[1].line.contains("## Outcome done"));
    }

    #[test]
    fn a_failed_delivery_reaches_the_supervisor() {
        let delivered = task(4, TaskState::Delivered, "resident");
        let mut facts = worker(false, false);
        facts.dispatched.push(Sent {
            key: "deliver-4-0".to_string(),
            text: None,
            created_at_ms: 0,
            finished_at_ms: Some(1),
            succeeded: Some(true),
        });
        let tasks = work(vec![delivered]);
        assert!(news_items(&tasks, std::slice::from_ref(&facts)).is_empty());
        facts.dispatched[0].succeeded = Some(false);
        let items = news_items(&tasks, &[facts]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].why, Why::DeliveryFailed);
        assert_eq!(
            items[0].marker(),
            "[spawn:esc session_resident_core deliver-4-0]"
        );
    }

    #[test]
    fn a_report_older_than_the_delivery_is_not_collected() {
        let dir = std::env::temp_dir().join(format!("spawn-report-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("report.md");
        std::fs::write(&path, "## Outcome\ndone").expect("write");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or_default();
        assert!(written_since(&path, None));
        assert!(written_since(&path, Some(now - 60_000)));
        assert!(!written_since(&path, Some(now + 60_000)), "left over");
        assert!(!written_since(&dir.join("missing.md"), None));
        let _ = std::fs::remove_dir_all(&dir);
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
