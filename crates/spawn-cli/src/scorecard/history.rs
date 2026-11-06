//! The record, folded into what the scorecard counts: the tasks and their
//! open intervals, the landings, and every counted mark with its time and
//! its mission (docs/design.md §15).
//!
//! The control records are folded in stream order (`session_seq`), each at
//! its own `recorded_at_ms`; the command rows by id, each at its own times;
//! the transcript messages at their own timestamps. A mark's mission is read
//! from a record that names it: a task's `mission_id`, a delivery key's task,
//! a stall tag's task, or the task open on a command's session when the
//! command was created.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::Deserialize;
use serde_json::Value;

use super::transcripts::Message;
use crate::deliver::{delivery_of, delivery_spent};
use crate::dispatch::Sent;

/// What a mark counts, by dimension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Kind {
    /// Rework: a gate verdict `rejected`.
    Rejected,
    /// Rework: a gate verdict `conflict`.
    Conflict,
    /// Rework: a gate verdict `incompatible`.
    Incompatible,
    /// Rework: a review with `accepted` false.
    Reviewed,
    /// Autonomy: a command of a person (`human`, the board) or the CLI
    /// (`spawn`).
    Command,
    /// Autonomy: a `mission.resumed`.
    Resume,
    /// Autonomy: a task a person assigned on a mission that a review accepts.
    OwnerTask,
    /// Autonomy: a person's cancellation followed by the same work assigned
    /// again.
    CancelReassign,
    /// Reliability: a command finished with `succeeded` false.
    FailedCommand,
    /// Reliability: a delivery whose retries are spent.
    SpentDelivery,
    /// Reliability: a stall tag, the first digest that carried it.
    Stall,
    /// Reliability: a gate outcome of the host, not the work (`aborted`,
    /// `no_room`, `host_fault`).
    GateInfrastructure,
}

/// One counted fact: when, of which mission (none: the fleet's only), what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Mark {
    pub(crate) at: i64,
    pub(crate) mission: Option<i64>,
    pub(crate) kind: Kind,
}

/// A landing: the task's `task.landed`, or its accepting review on a mission
/// without a gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Landing {
    pub(crate) at: i64,
    pub(crate) mission: i64,
    pub(crate) task: i64,
    /// The landing time minus the assignment its wait is measured from.
    pub(crate) lead_ms: i64,
    /// The task had no send-back before it landed.
    pub(crate) first_pass: bool,
}

/// A gate run that names its wall clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GateRun {
    pub(crate) at: i64,
    pub(crate) mission: Option<i64>,
    pub(crate) ms: i64,
}

/// A transcript message's tokens, with the missions whose tasks' agents it
/// belongs to (a task's, between its first delivery and its close).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tokens {
    pub(crate) at: i64,
    pub(crate) missions: Vec<i64>,
    pub(crate) model: String,
    pub(crate) count: i64,
}

/// A task as its records tell it.
#[derive(Debug, Clone)]
pub(crate) struct Task {
    pub(crate) id: i64,
    pub(crate) mission: i64,
    pub(crate) agent: String,
    pub(crate) session: String,
    pub(crate) objective: Option<i64>,
    pub(crate) brief: Option<String>,
    pub(crate) assigned_at: i64,
    pub(crate) delivered_at: Option<i64>,
    pub(crate) closed_at: Option<i64>,
    /// When it was cancelled, if it was (`task.cancelled`, `task.aborted`).
    pub(crate) cancelled_at: Option<i64>,
    pub(crate) sent_back: bool,
}

impl Task {
    /// Open at `at`: assigned at or before it, not closed by then.
    fn open_at(&self, at: i64) -> bool {
        self.assigned_at <= at && self.closed_at.is_none_or(|closed| at < closed)
    }
}

/// Everything the scorecard counts, from the record.
#[derive(Debug, Default)]
pub(crate) struct History {
    /// Every mission a record names.
    pub(crate) missions: BTreeSet<i64>,
    pub(crate) tasks: BTreeMap<i64, Task>,
    pub(crate) landings: Vec<Landing>,
    pub(crate) marks: Vec<Mark>,
    pub(crate) gate_runs: Vec<GateRun>,
    pub(crate) tokens: Vec<Tokens>,
}

/// One control record, as the store projects it (spawn-postgres,
/// `scorecard`). Fields are read leniently: a number may be written as text.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct Record {
    pub(crate) seq: i64,
    pub(crate) at: i64,
    #[serde(rename = "type")]
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) id: Value,
    #[serde(default)]
    pub(crate) mission_id: Value,
    #[serde(default)]
    pub(crate) agent_id: Value,
    #[serde(default)]
    pub(crate) session_id: Value,
    #[serde(default)]
    pub(crate) created_by: Value,
    #[serde(default)]
    pub(crate) objective: Value,
    /// The md5 of the brief.
    #[serde(default)]
    pub(crate) brief: Value,
    #[serde(default)]
    pub(crate) verdict: Value,
    #[serde(default)]
    pub(crate) accepted: Value,
    #[serde(default)]
    pub(crate) by: Value,
    #[serde(default)]
    pub(crate) gate: Value,
    #[serde(default)]
    pub(crate) accept: Value,
    #[serde(default)]
    pub(crate) gate_ms: Value,
    #[serde(default)]
    pub(crate) stage_ms: Value,
    #[serde(default)]
    pub(crate) table: Value,
    #[serde(default)]
    pub(crate) state: Value,
    #[serde(default)]
    pub(crate) created_at_ms: Value,
    #[serde(default)]
    pub(crate) updated_at_ms: Value,
    #[serde(default)]
    pub(crate) delivered_at_ms: Value,
}

/// One command row, as the store projects it.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct Command {
    pub(crate) id: i64,
    pub(crate) principal: String,
    pub(crate) key: String,
    pub(crate) session_id: String,
    #[serde(default)]
    pub(crate) action: Option<String>,
    pub(crate) created_at_ms: i64,
    #[serde(default)]
    pub(crate) finished_at_ms: Option<i64>,
    #[serde(default)]
    pub(crate) succeeded: Option<bool>,
    #[serde(default)]
    pub(crate) error: Option<String>,
    /// Its stall tags (`<session> stall-<task>-<step>-<n>`).
    #[serde(default)]
    pub(crate) stalls: Vec<String>,
}

/// The facts as the store answers them: `{control, commands}`.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct Facts {
    #[serde(default)]
    pub(crate) control: Vec<Record>,
    #[serde(default)]
    pub(crate) commands: Vec<Command>,
}

fn int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn text(value: &Value) -> Option<&str> {
    value.as_str()
}

fn boolean(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(flag) => Some(*flag),
        Value::String(text) => match text.as_str() {
            "true" | "t" => Some(true),
            "false" | "f" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// A mission's settings as of the record being folded.
#[derive(Debug, Clone, Copy)]
struct Mission {
    gated: bool,
    review: bool,
}

impl Default for Mission {
    fn default() -> Self {
        Self {
            gated: false,
            review: true,
        }
    }
}

/// The stall a digest tag names: `<session> stall-<task>-<step>-<n>` gives
/// `stall-<task>-<step>-<n>` and the task.
pub(crate) fn stall_of(tag: &str) -> Option<(&str, i64)> {
    let stall = tag.rsplit(' ').next()?;
    let mut parts = stall.strip_prefix("stall-")?.splitn(3, '-');
    let task = parts.next()?;
    let step = parts.next()?;
    let n = parts.next()?;
    let number = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    if !number(task)
        || !number(n)
        || step.is_empty()
        || !step
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
    {
        return None;
    }
    Some((stall, task.parse().ok()?))
}

/// The fold of the control stream (in stream order).
#[derive(Default)]
struct Fold {
    history: History,
    settings: BTreeMap<i64, Mission>,
    /// Person's cancellations not yet followed by the same work: (mission,
    /// objective, brief).
    cancelled: Vec<(i64, Option<i64>, Option<String>)>,
    /// Landing times by (mission, objective).
    objective_landings: BTreeMap<(i64, i64), Vec<i64>>,
    /// The tasks of each (mission, objective).
    objective_tasks: BTreeMap<(i64, i64), Vec<i64>>,
}

impl Fold {
    fn mission(&self, id: i64) -> Mission {
        self.settings.get(&id).copied().unwrap_or_default()
    }

    fn mark(&mut self, at: i64, mission: Option<i64>, kind: Kind) {
        self.history.marks.push(Mark { at, mission, kind });
    }

    fn task_mission(&self, task: Option<i64>) -> Option<i64> {
        self.history.tasks.get(&task?).map(|task| task.mission)
    }

    fn apply(&mut self, record: &Record) {
        let id = int(&record.id);
        match record.kind.as_str() {
            "mission.added" => {
                if let Some(id) = id {
                    self.history.missions.insert(id);
                    self.settings.insert(id, Mission::default());
                }
            }
            "mission.configured" => {
                if let Some(id) = id {
                    self.history.missions.insert(id);
                    self.settings.insert(
                        id,
                        Mission {
                            gated: !record.gate.is_null(),
                            review: text(&record.accept).is_none_or(|accept| accept == "review"),
                        },
                    );
                }
            }
            "mission.resumed" => {
                if let Some(id) = id {
                    self.mark(record.at, Some(id), Kind::Resume);
                }
            }
            "task.assigned" => self.assigned(record, record.at),
            "legacy.imported" => self.legacy(record),
            "task.delivered" => {
                if let Some(task) = id.and_then(|id| self.history.tasks.get_mut(&id)) {
                    task.delivered_at.get_or_insert(record.at);
                }
            }
            "task.gated" => self.gated(record, id),
            "task.reviewed" => {
                let Some(task) = id else { return };
                match boolean(&record.accepted) {
                    Some(false) => {
                        let mission = self.task_mission(Some(task));
                        self.mark(record.at, mission, Kind::Reviewed);
                        if let Some(task) = self.history.tasks.get_mut(&task) {
                            task.sent_back = true;
                        }
                    }
                    Some(true) => {
                        if let Some(mission) = self.task_mission(Some(task))
                            && !self.mission(mission).gated
                        {
                            self.landed(task, record.at);
                        }
                    }
                    None => {}
                }
            }
            "task.landed" => {
                if let Some(task) = id {
                    self.landed(task, record.at);
                }
            }
            "task.cancelled" | "task.aborted" => {
                let Some(task) = id.and_then(|id| self.history.tasks.get_mut(&id)) else {
                    return;
                };
                if task.closed_at.is_some() {
                    return;
                }
                task.closed_at = Some(record.at);
                task.cancelled_at = Some(record.at);
                // A person's: `by` absent (a record from before it) or `human`.
                let person = record.kind == "task.cancelled"
                    && text(&record.by).is_none_or(|by| by == "human");
                if person {
                    let entry = (task.mission, task.objective, task.brief.clone());
                    self.cancelled.push(entry);
                }
            }
            _ => {}
        }
    }

    fn assigned(&mut self, record: &Record, at: i64) {
        let (Some(id), Some(mission)) = (int(&record.id), int(&record.mission_id)) else {
            return;
        };
        self.history.missions.insert(mission);
        let task = Task {
            id,
            mission,
            agent: text(&record.agent_id).unwrap_or_default().to_string(),
            session: text(&record.session_id).unwrap_or_default().to_string(),
            objective: int(&record.objective),
            brief: text(&record.brief).map(str::to_string),
            assigned_at: at,
            delivered_at: None,
            closed_at: None,
            cancelled_at: None,
            sent_back: false,
        };
        if record.kind == "task.assigned" {
            if text(&record.created_by) == Some("human") && self.mission(mission).review {
                self.mark(at, Some(mission), Kind::OwnerTask);
            }
            // Each person's cancellation of the same work in this mission
            // (the same objective, or without one the same brief) is followed
            // by this assignment: one intervention each, counted now.
            let before = self.cancelled.len();
            self.cancelled.retain(|(of, objective, brief)| {
                let same = *of == mission
                    && match objective {
                        Some(objective) => task.objective == Some(*objective),
                        None => brief.is_some() && *brief == task.brief,
                    };
                !same
            });
            for _ in self.cancelled.len()..before {
                self.mark(at, Some(mission), Kind::CancelReassign);
            }
        }
        if let Some(objective) = task.objective {
            self.objective_tasks
                .entry((mission, objective))
                .or_default()
                .push(id);
        }
        self.history.tasks.insert(id, task);
    }

    /// A row from before the control stream: a mission's settings, or a task
    /// assigned at its row's `created_at_ms`, delivered at its
    /// `delivered_at_ms`, and, when the row is closed, closed at its
    /// `updated_at_ms` (no landing: no record of one).
    fn legacy(&mut self, record: &Record) {
        match text(&record.table) {
            Some("mission_missions") => {
                if let Some(id) = int(&record.id) {
                    self.history.missions.insert(id);
                    self.settings.insert(
                        id,
                        Mission {
                            gated: !record.gate.is_null(),
                            review: text(&record.accept).is_none_or(|accept| accept == "review"),
                        },
                    );
                }
            }
            Some("mission_tasks") => {
                let at = int(&record.created_at_ms).unwrap_or(record.at);
                self.assigned(record, at);
                let Some(id) = int(&record.id) else { return };
                let gated = self
                    .task_mission(Some(id))
                    .is_some_and(|m| self.mission(m).gated);
                let state = text(&record.state).unwrap_or_default();
                let closed =
                    matches!(state, "cancelled" | "landed") || (state == "accepted" && !gated);
                if let Some(task) = self.history.tasks.get_mut(&id) {
                    task.delivered_at = int(&record.delivered_at_ms);
                    if closed {
                        let at = int(&record.updated_at_ms).unwrap_or(record.at);
                        task.closed_at = Some(at);
                        if state == "cancelled" {
                            task.cancelled_at = Some(at);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn gated(&mut self, record: &Record, id: Option<i64>) {
        let mission = self.task_mission(id);
        let kind = match text(&record.verdict).unwrap_or_default() {
            "rejected" => Some(Kind::Rejected),
            "conflict" => Some(Kind::Conflict),
            "incompatible" => Some(Kind::Incompatible),
            "aborted" | "no_room" | "host_fault" => Some(Kind::GateInfrastructure),
            _ => None,
        };
        if let Some(kind) = kind {
            self.mark(record.at, mission, kind);
            if kind != Kind::GateInfrastructure
                && let Some(task) = id.and_then(|id| self.history.tasks.get_mut(&id))
            {
                task.sent_back = true;
            }
        }
        // The gate's wall clock: `gate_ms`, else the log's last stage line.
        if let Some(ms) = int(&record.gate_ms).or_else(|| int(&record.stage_ms)) {
            self.history.gate_runs.push(GateRun {
                at: record.at,
                mission,
                ms,
            });
        }
    }

    /// Task `id` lands at `at`, unless it is closed already.
    fn landed(&mut self, id: i64, at: i64) {
        let Some(task) = self.history.tasks.get(&id) else {
            return;
        };
        if task.closed_at.is_some() {
            return;
        }
        let start = self.lead_start(task);
        let landing = Landing {
            at,
            mission: task.mission,
            task: id,
            lead_ms: at - start,
            first_pass: !task.sent_back,
        };
        if let Some(objective) = task.objective {
            self.objective_landings
                .entry((task.mission, objective))
                .or_default()
                .push(at);
        }
        if let Some(task) = self.history.tasks.get_mut(&id) {
            task.closed_at = Some(at);
        }
        self.history.landings.push(landing);
    }

    /// Where a landed task's wait starts: its assignment, or for a task with
    /// an objective the assignment of the earliest task of the same mission
    /// and objective that was cancelled at or before this task's assignment
    /// (the objective's first try, so a re-issue does not hide the wait).
    /// A try from before the objective's previous landing in the mission
    /// belongs to that landing, not this one.
    fn lead_start(&self, task: &Task) -> i64 {
        let Some(objective) = task.objective else {
            return task.assigned_at;
        };
        let round = self
            .objective_landings
            .get(&(task.mission, objective))
            .and_then(|landings| {
                landings
                    .iter()
                    .copied()
                    .filter(|landed| *landed <= task.assigned_at)
                    .max()
            })
            .unwrap_or(i64::MIN);
        self.objective_tasks
            .get(&(task.mission, objective))
            .into_iter()
            .flatten()
            .filter_map(|id| self.history.tasks.get(id))
            .filter(|other| {
                other.assigned_at >= round
                    && other
                        .cancelled_at
                        .is_some_and(|cancelled| cancelled <= task.assigned_at)
            })
            .map(|other| other.assigned_at)
            .fold(task.assigned_at, i64::min)
    }
}

impl History {
    /// The history the facts and the archive's messages tell.
    pub(crate) fn from_record(facts: &Facts, messages: &[Message]) -> Self {
        let mut fold = Fold::default();
        let mut control: Vec<&Record> = facts.control.iter().collect();
        control.sort_by_key(|record| record.seq);
        for record in control {
            fold.apply(record);
        }
        let mut history = fold.history;
        history.commands(&facts.commands);
        history.messages(messages);
        history
    }

    /// The mission a command belongs to: that of the task its key names
    /// (`deliver-<task>-...`), else that of the task open on its session
    /// when it was created (the oldest, when several are: delivery takes a
    /// session's tasks oldest first), else none (the fleet's only).
    pub(crate) fn command_mission(
        &self,
        command: &Command,
        by_session: &HashMap<&str, Vec<&Task>>,
    ) -> Option<i64> {
        if let Some((task, _)) = delivery_of(&command.key)
            && let Some(task) = self.tasks.get(&task)
        {
            return Some(task.mission);
        }
        by_session
            .get(command.session_id.as_str())?
            .iter()
            .filter(|task| task.open_at(command.created_at_ms))
            .min_by_key(|task| (task.assigned_at, task.id))
            .map(|task| task.mission)
    }

    fn commands(&mut self, commands: &[Command]) {
        let mut sorted: Vec<&Command> = commands.iter().collect();
        sorted.sort_by_key(|command| command.id);
        let by_session = {
            let mut map: HashMap<&str, Vec<&Task>> = HashMap::new();
            for task in self.tasks.values() {
                map.entry(task.session.as_str()).or_default().push(task);
            }
            map
        };
        let mut marks = Vec::new();
        let mut deliveries: BTreeMap<&str, (i64, Vec<Sent>)> = BTreeMap::new();
        let mut stalls = BTreeSet::new();
        for command in sorted {
            let mission = self.command_mission(command, &by_session);
            let at = command.created_at_ms;
            if matches!(command.principal.as_str(), "human" | "spawn") {
                marks.push(Mark {
                    at,
                    mission,
                    kind: Kind::Command,
                });
            }
            if command.succeeded == Some(false) {
                marks.push(Mark {
                    at,
                    mission,
                    kind: Kind::FailedCommand,
                });
            }
            if command.principal == "spawn-dispatch" {
                if let Some((task, delivery)) = delivery_of(&command.key) {
                    deliveries
                        .entry(delivery)
                        .or_insert_with(|| (task, Vec::new()))
                        .1
                        .push(Sent {
                            id: Some(command.id),
                            key: command.key.clone(),
                            text: None,
                            created_at_ms: command.created_at_ms,
                            finished_at_ms: command.finished_at_ms,
                            succeeded: command.succeeded,
                            error: command.error.clone(),
                        });
                }
                if command.action.as_deref() == Some("input") {
                    for tag in &command.stalls {
                        let Some((stall, task)) = stall_of(tag) else {
                            continue;
                        };
                        // Once, at the first digest that carried it.
                        if stalls.insert(stall.to_string()) {
                            let mission = self.tasks.get(&task).map(|task| task.mission);
                            marks.push(Mark {
                                at,
                                mission,
                                kind: Kind::Stall,
                            });
                        }
                    }
                }
            }
        }
        for (task, attempts) in deliveries.values() {
            let sent: Vec<&Sent> = attempts.iter().collect();
            if delivery_spent(&sent) {
                let last = sent.last().expect("a delivery has an attempt");
                marks.push(Mark {
                    at: last.finished_at_ms.unwrap_or(last.created_at_ms),
                    mission: self.tasks.get(task).map(|task| task.mission),
                    kind: Kind::SpentDelivery,
                });
            }
        }
        self.marks.extend(marks);
    }

    fn messages(&mut self, messages: &[Message]) {
        let mut by_agent: HashMap<&str, Vec<&Task>> = HashMap::new();
        for task in self.tasks.values() {
            by_agent.entry(task.agent.as_str()).or_default().push(task);
        }
        let tokens = messages
            .iter()
            .map(|message| {
                let missions: BTreeSet<i64> = by_agent
                    .get(message.agent.as_str())
                    .into_iter()
                    .flatten()
                    .filter(|task| {
                        task.delivered_at
                            .is_some_and(|delivered| delivered <= message.at_ms)
                            && task.closed_at.is_none_or(|closed| message.at_ms < closed)
                    })
                    .map(|task| task.mission)
                    .collect();
                Tokens {
                    at: message.at_ms,
                    missions: missions.into_iter().collect(),
                    model: message.model.clone(),
                    count: message.tokens,
                }
            })
            .collect();
        self.tokens = tokens;
    }

    /// Only what counts in `[since, until)`: the landings, marks, gate runs
    /// and tokens in it, and the tasks open in it. Every count of a window
    /// inside it is the same after; there is less to read for each.
    pub(crate) fn clip(&mut self, since: i64, until: i64) {
        let inside = |at: i64| since <= at && at < until;
        self.landings.retain(|landing| inside(landing.at));
        self.marks.retain(|mark| inside(mark.at));
        self.gate_runs.retain(|run| inside(run.at));
        self.tokens.retain(|tokens| inside(tokens.at));
        self.tasks.retain(|_, task| {
            task.assigned_at < until && task.closed_at.is_none_or(|closed| closed > since)
        });
    }

    /// The open intervals of the tasks of `mission` (every task's for none):
    /// from the assignment to the close, `None` while open.
    pub(crate) fn open_intervals(&self, mission: Option<i64>) -> Vec<(i64, Option<i64>)> {
        self.tasks
            .values()
            .filter(|task| mission.is_none_or(|mission| task.mission == mission))
            .map(|task| (task.assigned_at, task.closed_at))
            .collect()
    }
}
