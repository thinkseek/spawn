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
//! items for the supervisor. Like the dispatcher, delivery keeps no state:
//! idempotency keys and the task rows say what was already done.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;
use spawn_core::command::CommandAction;
use spawn_core::event::Engine;
use spawn_core::observation::{Ambiguity, SemanticState};
use spawn_postgres::PostgresStore;

use crate::dispatch::{Escalation, Facts, Order, Why};

/// Where a worker writes its report, relative to its workspace.
pub(crate) const REPORT: &str = ".spawn/report.md";

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

    /// Sessions whose open task (delivered, under review, or sent back)
    /// keeps the next one waiting: a revision continues in the same context,
    /// and no other task is mixed in before the supervisor closes it.
    pub(crate) fn blocked(&self) -> BTreeSet<&str> {
        self.tasks
            .iter()
            .filter(|task| matches!(task.state.as_str(), "delivered" | "reported"))
            .map(|task| task.session_id.as_str())
            .collect()
    }

    /// Sessions with a task being worked on right now.
    pub(crate) fn in_flight(&self) -> BTreeSet<&str> {
        self.tasks
            .iter()
            .filter(|task| task.state == "delivered")
            .map(|task| task.session_id.as_str())
            .collect()
    }

    /// Sessions the mission layer manages (they have recent tasks, or are
    /// residents).
    pub(crate) fn managed(&self, session: &str) -> bool {
        session.starts_with("session_resident_")
            || self.tasks.iter().any(|task| task.session_id == session)
    }
}

/// Whether a worker sits idle at its prompt, judged on its latest fact,
/// with nothing in flight: the only moment typed input is safe.
pub(crate) fn idle(facts: &Facts) -> bool {
    facts.live
        && !facts.open_command
        && facts.observation.as_ref().is_some_and(|observation| {
            facts
                .evidence_seq
                .is_none_or(|latest| observation.based_on_seq.0 >= latest)
                && observation.ambiguity == Some(Ambiguity::TurnStoppedIdle)
                && matches!(
                    observation.state,
                    SemanticState::Waiting | SemanticState::Completed
                )
        })
}

const REPORT_CONTRACT: &str = "When the task is finished, or you cannot go on, write `.spawn/report.md` \
at the root of your workspace with three sections: `## Outcome` (done, blocked or failed, in one \
line), `## What changed` (files, commits, results) and `## Open issues`. Then stop and wait. Do not \
start other work. If a person must decide something, say so in the report.";

/// The full prompt that starts a task in a new or resumed session.
pub(crate) fn task_prompt(task: &TaskRow) -> String {
    let role = match (&task.resident, &task.charter) {
        (Some(name), Some(charter)) => format!(
            "You are `{name}`, a long-lived resident worker of Spawn. Your charter:\n{charter}\n\n\
Tasks inside your charter arrive one at a time; keep what you learn about your domain."
        ),
        _ => "You are a Spawn worker started for one task. Work only on it.".to_string(),
    };
    format!(
        "{role}\n\nMission #{}: {}\n\nTask #{}:\n{}\n\n{REPORT_CONTRACT}",
        task.mission_id, task.mission_text, task.id, task.brief
    )
}

/// What a live worker gets typed for a task: a new task, or a revision.
fn typed(task: &TaskRow) -> String {
    if task.state == "revise" {
        format!(
            "Task #{} was sent back by the supervisor: {}\nFix it, then rewrite `.spawn/report.md`.",
            task.id,
            task.review_note.as_deref().unwrap_or("see the review")
        )
    } else {
        format!(
            "New task #{} for mission #{}: {}\n\nTask:\n{}\n\n{REPORT_CONTRACT}",
            task.id, task.mission_id, task.mission_text, task.brief
        )
    }
}

fn order(task: &TaskRow, key: String, action: CommandAction) -> Order {
    Order {
        agent_id: task.agent_id.clone(),
        session_id: task.session_id.clone(),
        key,
        action,
        task: Some(task.id),
    }
}

/// Delivery orders for the current facts.
pub(crate) fn orders(work: &Work, rows: &[Facts]) -> Vec<Order> {
    let facts: BTreeMap<&str, &Facts> = rows
        .iter()
        .map(|facts| (facts.session_id.as_str(), facts))
        .collect();
    let busy = work.blocked();
    // A task sent back goes first: it is the one the worker has in mind.
    let mut served: BTreeSet<&str> = work
        .tasks
        .iter()
        .filter(|task| task.state == "revise")
        .map(|task| task.session_id.as_str())
        .collect();
    let mut sent_back = served.clone();
    let mut orders = Vec::new();
    for task in &work.tasks {
        let session = task.session_id.as_str();
        let found = facts.get(session).copied();
        match task.state.as_str() {
            "assigned" | "revise" => {
                // One open task per worker, oldest first, revisions before
                // new work.
                let first = if task.state == "revise" {
                    sent_back.remove(session)
                } else {
                    served.insert(session)
                };
                if busy.contains(session) || !first {
                    continue;
                }
                let key = format!("deliver-{}-{}", task.id, task.revisions);
                match found {
                    Some(facts) if facts.open_command => {}
                    Some(facts) if facts.live => {
                        if idle(facts) {
                            let text = typed(task);
                            orders.push(order(task, key, CommandAction::Input { text }));
                        }
                    }
                    // Ran before: resume, so a resident keeps its context.
                    Some(facts) if facts.execution_id.is_some() => {
                        let mut prompt = task_prompt(task);
                        if task.state == "revise" {
                            prompt = format!("{prompt}\n\n{}", typed(task));
                        }
                        orders.push(order(
                            task,
                            key,
                            CommandAction::Resume {
                                prompt: Some(prompt),
                            },
                        ));
                    }
                    _ => orders.push(order(
                        task,
                        key,
                        CommandAction::Start {
                            engine: Engine::Claude,
                            prompt: task_prompt(task),
                            parent_session_id: None,
                        },
                    )),
                }
            }
            "accepted" | "cancelled" if task.kind == "ephemeral" => {
                if let Some(facts) = found
                    && facts.live
                    && !facts.open_command
                {
                    let key = format!("retire-task-{}", task.id);
                    orders.push(order(task, key, CommandAction::Stop));
                }
            }
            _ => {}
        }
    }
    for (agent, session) in &work.retired {
        if let Some(facts) = facts.get(session.as_str())
            && facts.live
            && !facts.open_command
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

fn field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn whole(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or_default()
}

fn one_line(text: &str, limit: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= limit {
        flat
    } else {
        format!("{}…", flat.chars().take(limit).collect::<String>())
    }
}

/// Digest items from the mission layer.
pub(crate) fn news_items(news: &News) -> Vec<Escalation> {
    let mut items = Vec::new();
    for mission in &news.missions {
        let id = whole(mission, "id");
        items.push(Escalation {
            session_id: "missions".to_string(),
            id: format!("mission-{id}"),
            why: Why::Mission,
            line: format!(
                "#{id} from {}: {}",
                field(mission, "origin"),
                one_line(field(mission, "text"), 300)
            ),
        });
    }
    for report in &news.reports {
        let id = whole(report, "id");
        items.push(Escalation {
            session_id: field(report, "session_id").to_string(),
            id: format!("report-{id}"),
            why: Why::Report,
            line: format!(
                "task #{id} ({}, mission #{}): {}",
                field(report, "kind"),
                whole(report, "mission_id"),
                one_line(field(report, "report"), 400)
            ),
        });
    }
    for resident in &news.approved {
        let name = field(resident, "name");
        items.push(Escalation {
            session_id: format!("session_resident_{name}"),
            id: format!("approved-{name}"),
            why: Why::Approved,
            line: format!("a human approved resident `{name}`; assign it tasks with --kind resident --worker {name}"),
        });
    }
    for message in &news.human {
        items.push(Escalation {
            session_id: field(message, "session_id").to_string(),
            id: format!("human-{}", whole(message, "command_id")),
            why: Why::Human,
            line: format!("a human told it: {}", one_line(field(message, "text"), 200)),
        });
    }
    items
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
        let Some(facts) = rows
            .iter()
            .find(|facts| facts.session_id == task.session_id)
        else {
            continue;
        };
        if !idle(facts) {
            continue;
        }
        let path = root.join(&task.agent_id).join(REPORT);
        let Ok(report) = std::fs::read_to_string(&path) else {
            continue;
        };
        if report.trim().is_empty() {
            continue;
        }
        match store.task_reported(task.id, report.trim()) {
            Ok(true) => {
                let kept = root
                    .join(&task.agent_id)
                    .join(".spawn/reports")
                    .join(format!("task-{}.md", task.id));
                let moved = kept
                    .parent()
                    .map(std::fs::create_dir_all)
                    .and_then(Result::ok)
                    .and_then(|()| std::fs::rename(&path, &kept).ok());
                if moved.is_none() {
                    let _ = std::fs::remove_file(&path);
                }
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
