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
//!   so is a retired resident.
//!
//! Missions, reports, approvals and human messages to workers become digest
//! items for the supervisor ([`news`]). Like the dispatcher, delivery keeps
//! no state: idempotency keys and the task rows say what was already done.
//!
//! [`orders`] is pure; [`collect_reports`] is the one step with side effects
//! (the report file and the task row). What workers are told is in
//! [`prompt`].

mod news;
mod prompt;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;
use spawn_core::command::CommandAction;
use spawn_core::event::Engine;
use spawn_postgres::PostgresStore;

use crate::dispatch::{Facts, Order};

pub(crate) use news::news_items;
pub(crate) use prompt::task_prompt;

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
    pub(crate) state: String,
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
}

impl TaskRow {
    /// Sent back by the supervisor, waiting to be delivered again.
    fn is_revision(&self) -> bool {
        self.state == "revise"
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

/// What the supervisor should hear about besides worker states.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct News {
    #[serde(default)]
    pub(crate) missions: Vec<Value>,
    #[serde(default)]
    pub(crate) reports: Vec<Value>,
    #[serde(default)]
    pub(crate) approved: Vec<Value>,
    #[serde(default)]
    pub(crate) human: Vec<Value>,
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
    fn sessions_in(&self, states: &[&str]) -> BTreeSet<&str> {
        self.tasks
            .iter()
            .filter(|task| states.contains(&task.state.as_str()))
            .map(|task| task.session_id.as_str())
            .collect()
    }

    /// Sessions whose open task (delivered, under review, or sent back)
    /// keeps the next one waiting: a revision continues in the same context,
    /// and no other task is mixed in before the supervisor closes it.
    pub(crate) fn blocked(&self) -> BTreeSet<&str> {
        self.sessions_in(&["delivered", "reported"])
    }

    /// Sessions with a task being worked on right now.
    pub(crate) fn in_flight(&self) -> BTreeSet<&str> {
        self.sessions_in(&["delivered"])
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
        let revisions = work.sessions_in(&["revise"]);
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
    let key = format!("deliver-{}-{}", task.id, task.revisions);
    match found {
        Some(facts) if facts.open_command => None,
        Some(facts) if facts.live => facts.idle_at_prompt().then(|| {
            task.order(
                key,
                CommandAction::Input {
                    text: prompt::typed(task),
                },
            )
        }),
        // Ran before: resume, so a resident keeps its context.
        Some(facts) if facts.execution_id.is_some() => Some(task.order(
            key,
            CommandAction::Resume {
                prompt: Some(prompt::resume_prompt(task)),
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
        match task.state.as_str() {
            "assigned" | "revise" => {
                let turn = turns.take(task);
                if turn && !blocked.contains(session) {
                    orders.extend(delivery(task, found));
                }
            }
            "accepted" | "cancelled" if task.kind == "ephemeral" => {
                if found.is_some_and(Facts::live_and_free) {
                    let key = format!("retire-task-{}", task.id);
                    orders.push(task.order(key, CommandAction::Stop));
                }
            }
            _ => {}
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

/// Move a collected report to `.spawn/reports/task-<id>.md` under the
/// workspace, or delete it if it cannot be moved, so the next task starts
/// clean.
fn put_aside(path: &Path, workspace: &Path, task: i64) {
    let kept = workspace
        .join(KEPT_REPORTS)
        .join(format!("task-{task}.md"));
    let moved = kept
        .parent()
        .map(std::fs::create_dir_all)
        .and_then(Result::ok)
        .and_then(|()| std::fs::rename(path, &kept).ok());
    if moved.is_none() {
        let _ = std::fs::remove_file(path);
    }
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
    for task in work.tasks.iter().filter(|task| task.state == "delivered") {
        let idle = rows
            .iter()
            .find(|facts| facts.session_id == task.session_id)
            .is_some_and(Facts::idle_at_prompt);
        if !idle {
            continue;
        }
        let workspace = root.join(&task.agent_id);
        let path = workspace.join(REPORT);
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
    use super::{News, TaskRow, Work, news_items, orders, task_prompt};
    use crate::dispatch::{Facts, Why};
    use serde_json::json;
    use spawn_core::command::CommandAction;
    use spawn_core::event::SessionSeq;
    use spawn_core::observation::{Ambiguity, Confidence, Observation, SemanticState};

    fn task(id: i64, state: &str, kind: &str) -> TaskRow {
        TaskRow {
            id,
            mission_id: 1,
            kind: kind.to_string(),
            agent_id: "agent_resident_core".to_string(),
            session_id: "session_resident_core".to_string(),
            brief: format!("brief {id}"),
            state: state.to_string(),
            revisions: 0,
            review_note: Some("tests fail".to_string()),
            mission_text: "Make ingest robust".to_string(),
            charter: Some("Owns spawn-core".to_string()),
            resident: Some("core".to_string()),
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
        let plan = orders(&work(vec![task(4, "assigned", "resident")]), &[]);
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
            &work(vec![task(4, "assigned", "resident")]),
            &[worker(false, false)],
        );
        assert!(matches!(plan[0].action, CommandAction::Resume { .. }));
    }

    #[test]
    fn a_live_worker_gets_the_task_only_when_idle() {
        let tasks = vec![task(4, "assigned", "resident")];
        assert!(orders(&work(tasks.clone()), &[worker(true, false)]).is_empty());
        let plan = orders(&work(tasks), &[worker(true, true)]);
        assert!(matches!(plan[0].action, CommandAction::Input { .. }));
    }

    #[test]
    fn one_task_in_flight_per_worker() {
        let tasks = vec![
            task(3, "delivered", "resident"),
            task(4, "assigned", "resident"),
        ];
        assert!(orders(&work(tasks), &[worker(true, true)]).is_empty());
        let queued = vec![
            task(4, "assigned", "resident"),
            task(5, "assigned", "resident"),
        ];
        let plan = orders(&work(queued), &[worker(true, true)]);
        assert_eq!(plan.len(), 1, "oldest first, one at a time");
        assert_eq!(plan[0].task, Some(4));
    }

    #[test]
    fn the_next_task_waits_for_the_review() {
        let tasks = vec![
            task(3, "reported", "resident"),
            task(4, "assigned", "resident"),
        ];
        assert!(orders(&work(tasks), &[worker(true, true)]).is_empty());
        let mut revise = task(3, "revise", "resident");
        revise.revisions = 1;
        let tasks = vec![revise, task(4, "assigned", "resident")];
        let plan = orders(&work(tasks), &[worker(true, true)]);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].task, Some(3), "the revision goes first");
    }

    #[test]
    fn a_task_sent_back_carries_the_note() {
        let mut revise = task(4, "revise", "resident");
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
        let mut done = task(4, "accepted", "ephemeral");
        done.resident = None;
        let plan = orders(&work(vec![done]), &[worker(true, true)]);
        assert_eq!(plan[0].action, CommandAction::Stop);
        let resident = task(5, "accepted", "resident");
        assert!(orders(&work(vec![resident]), &[worker(true, true)]).is_empty());
    }

    #[test]
    fn missions_reports_and_human_messages_become_digest_items() {
        let news = News {
            missions: vec![json!({"id": 2, "text": "Ship it", "origin": "human"})],
            reports: vec![json!({"id": 7, "mission_id": 2, "kind": "ephemeral",
                                 "session_id": "session_worker_x_7", "report": "## Outcome\ndone"})],
            approved: vec![json!({"name": "core"})],
            human: vec![
                json!({"command_id": 40, "session_id": "session_worker_x_7", "text": "use eu"}),
            ],
        };
        let items = news_items(&news);
        let whys: Vec<Why> = items.iter().map(|item| item.why).collect();
        assert_eq!(
            whys,
            vec![Why::Mission, Why::Report, Why::Approved, Why::Human]
        );
        assert_eq!(items[1].marker(), "[spawn:esc session_worker_x_7 report-7]");
        assert!(items[1].line.contains("## Outcome done"));
    }

    #[test]
    fn an_ephemeral_prompt_has_no_charter() {
        let mut one = task(9, "assigned", "ephemeral");
        one.resident = None;
        one.charter = None;
        assert!(task_prompt(&one).contains("started for one task"));
    }
}
