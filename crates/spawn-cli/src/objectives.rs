//! Objective missions (docs/fleet-architecture.md §10.4): the controller's
//! backlog (`rsi/backlog/*.json`, in its checkout) turned into tasks.
//!
//! The integrator keeps up to N of the mission's tasks in flight, N being its
//! worker limit (`objectives ->> 'workers'`, 1 to 16, default 1). A task is
//! in flight from `task.assigned` until it lands or is cancelled. Each pass
//! fills the free slots with the open objectives that have no task in flight
//! (an objective never has two), fewest attempts first: those that yielded
//! fewest times, then those with fewest failed attempts since their last
//! yield, then the lowest id. With no failures this is the lowest open id.
//!
//! No objective holds a slot for ever. A failed attempt is an outcome that
//! is evidence against the work, and only that ([`Outcome::failure`]):
//!
//! - A gate's `rejected`: the scenario or a check failed on the work. It
//!   counts, even when the cause was the host's (a contended test server,
//!   a flaky self-test): the gate cannot tell, and the supervisor reads
//!   the yield's reasons. `incompatible` counts the same way: the work, as
//!   delivered, fails once merged with main.
//! - A gate's `conflict` never counts. It says main moved under the task
//!   (another landing, an owner batch), not that the work failed: the
//!   worker gets the conflict to resolve, and one that cannot resolve it
//!   shows up as `rejected` or as an overrun over its budget, which the
//!   supervisor reads. Mission 4's objectives 023 and 025 yielded at
//!   2026-11-08 04:47 on conflicts from an owner batch, with work that had
//!   passed review.
//! - An overrun over budget is not one either: the work is kept and
//!   continued ([`Overrun`]), and the next task of the objective takes it
//!   on. Its record names the budget that made it (`measure: "active"`,
//!   with the elapsed time and tool calls it measured, [`crate::integrate`]'s
//!   `enforce_budget`), so a reader of the record sees what ended the task.
//!   A record from before an overrun was its own record (`task.aborted`)
//!   counts only with that measure; one without it is an older abort (the
//!   wall-clock budget's, which aborted tasks that a restart had kept
//!   waiting, the thing the owner said must never happen again) or the
//!   store's (a landing gone stale after its mission was done): neither
//!   judged the work.
//! - The host's outcomes (`no_room`, `host_fault`, `stale`, a gate
//!   `aborted`) are not the work's and count for nothing, as for the halt
//!   rules.
//!
//! Once an
//! objective has `SPAWN_OBJECTIVE_ATTEMPTS` (3) failed attempts since its
//! last yield (an accepted verdict also ends the count), its task yields
//! the slot once it is being worked again (assigned, delivered or sent back;
//! a reported task waits for its gate): its worker is stopped and it is
//! recorded `task.cancelled` with `yielded` (its work set aside as any
//! cancel's; no halt counts it), and the supervisor is told once. The
//! objective goes to the back of the queue: it is tried again after the
//! others had their turn. A task whose gate runs now, or that this pass
//! aborted, does not yield, and a slot an abort frees is filled from the
//! next pass on, fewest attempts first, so the aborted objective is not
//! re-issued at once (task 113 aborted at 23:05:46, task 160 for the same
//! objective 023 five seconds later). All of it is read from the control
//! stream ([`attempts`]), never from the integrator's memory. Before this
//! (2026-11-07), three objectives whose tasks kept failing held mission 4's
//! three slots for a day and ten objectives were never assigned.
//!
//! **A task over its budget keeps its work** (docs/fleet-architecture.md
//! §10.4). The integrator stops it and records `task.over_budget`, which
//! counts toward no halt; this module assigns the objective's next task
//! once that work has reached the source's `spawn/<agent>` (the overrun's
//! record names the branch, the commit and its `git diff --stat main`), its
//! `task.assigned` names `continues` (the overrun task) and its brief names
//! that task, the branch, the commit and the diff stat, so the work is
//! continued, not redone. A second overrun of a task that continues one is
//! not followed by a third task: the supervisor is told once (the
//! dispatcher's digest, tag `host over-budget-<record>`) and the objective
//! is held — no task for it — until the mission is configured again (a
//! `mission.configured` record newer than the overrun's), while the
//! mission's other objectives take the freed slot. A third overrun after
//! such a configure is treated as a first.
//!
//! An objective assigned again after earlier tasks of it ended without
//! landing (cancelled, aborted, yielded) gets their work in its brief: each
//! one's id, last checkpoint, last verdict or reason, and report, with the
//! instruction to use the reports and not redo what they established
//! ([`PriorWork`], read from the records at assignment). A worker has no
//! `.git` and cannot read another branch: an ephemeral task's workspace is
//! forked from the newest such work instead, and its first prompt says so
//! (`main_merge::fork_from_prior`). Task 161 rebuilt 2264 lines that task
//! 155 had mostly written.
//!
//! **Order.** A backlog objective may name the objectives it comes after
//! (`"after": [28, 29]`): it is not assigned while one of them is open (a
//! named objective the backlog does not hold must have landed). Order is
//! kept only where it is named; otherwise fewest attempts first decides.
//! An order that cannot be met (an id the backlog does not hold and that
//! has not landed, or objectives that come after each other) would hold
//! them back for ever without a word, so the dispatcher checks each open
//! mission's backlog ([`order_problems`]) and tells the supervisor once per
//! content of the problems (`missions objective-order-<mission>-<digest>`).
//!
//! The mission's resident takes a task
//! whenever it has none open in the mission; every other task goes to a new
//! per-task worker (`agent_worker_objective_<task>`, ephemeral: no approval,
//! a fresh workspace from the source's main, stopped once its task lands).
//! `spawn_objective_assign` checks both limits again under the append lock.
//! With N = 1 this is one task at a time on the resident.
//!
//! A one-shot objective is solved once a task for it lands, or once its
//! scenario passes at a base commit main already holds (`objective.solved`,
//! read as a landing: the work could change nothing); a recurring one
//! stays open until the metric of its latest landing meets its target. The
//! metric is the gate's verdict line, never scenario output, which candidate
//! code controls.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use serde::Deserialize;
use spawn_postgres::PostgresStore;

use serde_json::json;
use spawn_core::mission::TaskState;

use crate::integrate::Mission;

/// Gate verdicts quoted in an objective task's brief.
const HISTORY: i64 = 6;

/// Earlier tasks whose work an objective task's brief carries.
const PRIOR_WORK: i64 = 3;

/// Characters of an earlier task's report, and of its reason, a brief
/// quotes; the rest is cut, marked.
const REPORT_CHARS: usize = 3000;
const REASON_CHARS: usize = 300;

/// One backlog task.
///
/// A recurring task is an objective: landing once does not solve it. It stays
/// open until the metric its scenario prints reaches `target`.
#[derive(Debug, Clone)]
pub(crate) struct Task {
    pub(crate) id: u32,
    pub(crate) title: String,
    pub(crate) prompt: String,
    pub(crate) recurring: bool,
    pub(crate) metric: Option<String>,
    pub(crate) unit: String,
    pub(crate) lower_is_better: bool,
    pub(crate) target: Option<f64>,
    /// The objectives it comes after (`"after": [..]`): it is not assigned
    /// while one of them is open ([`choose`]).
    pub(crate) after: Vec<u32>,
    /// The owner judges it, outside the gate (`"owner_judged": true`): it is
    /// never assigned to the fleet ([`choose`], [`next_open`]), and the
    /// fleet's board shows it as the owner's. It still counts as open for
    /// [`ordered`]: a dependent objective waits until it is solved, since
    /// the owner's work is what its dependents build on. It stays open
    /// here until it lands in this mission (a landing is the only thing
    /// this backlog's openness reads).
    pub(crate) owner_judged: bool,
}

impl Task {
    /// Whether `value` meets the target.
    pub(crate) fn meets(&self, value: f64) -> bool {
        self.target.is_some_and(|target| {
            if self.lower_is_better {
                value <= target
            } else {
                value >= target
            }
        })
    }
}

/// One `METRIC <name> baseline=<n> candidate=<n>` line of a gate log.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Metric {
    pub(crate) name: String,
    pub(crate) baseline: Option<f64>,
    pub(crate) candidate: Option<f64>,
}

/// The `METRIC <name> baseline=<n> candidate=<n>` lines of a text (a
/// verdict's metric line), the last line of each name winning.
pub(crate) fn metric_lines(text: &str) -> Vec<Metric> {
    let mut metrics: Vec<Metric> = Vec::new();
    for line in text.lines() {
        let line = line.trim_start_matches('|').trim();
        let Some(rest) = line.strip_prefix("METRIC ") else {
            continue;
        };
        let mut words = rest.split_whitespace();
        let Some(name) = words.next() else {
            continue;
        };
        let mut metric = Metric {
            name: name.to_string(),
            baseline: None,
            candidate: None,
        };
        for word in words {
            match word.split_once('=') {
                Some(("baseline", value)) => metric.baseline = value.parse().ok(),
                Some(("candidate", value)) => metric.candidate = value.parse().ok(),
                _ => {}
            }
        }
        metrics.retain(|known| known.name != metric.name);
        metrics.push(metric);
    }
    metrics
}

/// The candidate value of metric `name` in one verdict metric line.
pub(crate) fn candidate_value(line: &str, name: &str) -> Option<f64> {
    metric_lines(line)
        .into_iter()
        .find(|metric| metric.name == name)?
        .candidate
}

/// Every task in the controller's backlog directory, by id.
pub(crate) fn load_backlog(directory: &Path) -> Result<Vec<Task>, String> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("read {}: {error}", directory.display())),
    };
    let mut tasks = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|error| format!("read {}: {error}", directory.display()))?
            .path();
        if path.extension().and_then(|text| text.to_str()) != Some("json") {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|error| format!("parse {}: {error}", path.display()))?;
        let text_field = |name: &str| value.get(name).and_then(|v| v.as_str()).map(str::to_string);
        let id = value
            .get("id")
            .and_then(serde_json::Value::as_u64)
            .and_then(|id| u32::try_from(id).ok())
            .ok_or_else(|| format!("{} has no id", path.display()))?;
        let prompt =
            text_field("prompt").ok_or_else(|| format!("{} has no prompt", path.display()))?;
        let task = Task {
            id,
            title: text_field("title").unwrap_or_else(|| format!("Task {id}")),
            prompt,
            recurring: value.get("recurring").and_then(serde_json::Value::as_bool) == Some(true),
            metric: text_field("metric"),
            unit: text_field("unit").unwrap_or_default(),
            lower_is_better: text_field("direction").as_deref() != Some("higher"),
            target: value.get("target").and_then(serde_json::Value::as_f64),
            // A boolean and nothing else: an absent field is the fleet's,
            // and `null` is refused like any other non-boolean (a flag that
            // json tools leave unset is not a statement about the owner).
            owner_judged: match value.get("owner_judged") {
                None => false,
                Some(flag) => flag.as_bool().ok_or_else(|| {
                    format!("{}: `owner_judged` is not true or false", path.display())
                })?,
            },
            after: match value.get("after") {
                None | Some(serde_json::Value::Null) => Vec::new(),
                Some(after) => after
                    .as_array()
                    .and_then(|ids| {
                        ids.iter()
                            .map(|id| id.as_u64().and_then(|id| u32::try_from(id).ok()))
                            .collect::<Option<Vec<u32>>>()
                    })
                    .ok_or_else(|| {
                        format!("{}: `after` is not a list of objective ids", path.display())
                    })?,
            },
        };
        // Without both, a recurring objective could never be met.
        if task.recurring && (task.metric.is_none() || task.target.is_none()) {
            return Err(format!(
                "{} is recurring but has no metric and target",
                path.display()
            ));
        }
        tasks.push(task);
    }
    tasks.sort_by_key(|task| task.id);
    Ok(tasks)
}

/// One landed objective task, newest first in a list.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Landing {
    pub(crate) objective: u32,
    #[serde(default)]
    pub(crate) metric: Option<String>,
}

/// One gate verdict on an objective's task.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GateRecord {
    pub(crate) task: i64,
    pub(crate) verdict: String,
    #[serde(default)]
    pub(crate) reason: Option<String>,
    #[serde(default)]
    pub(crate) metric: Option<String>,
}

/// An earlier task of an objective that ended without landing, and the
/// work it left (`PostgresStore::objective_prior_work`), newest first.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct PriorWork {
    pub(crate) task: i64,
    #[serde(default)]
    pub(crate) commit: Option<String>,
    #[serde(default)]
    pub(crate) outcome: Option<String>,
    #[serde(default)]
    pub(crate) reason: Option<String>,
    #[serde(default)]
    pub(crate) report: Option<String>,
}

/// `text` up to `chars` characters, cut marked.
fn cut(text: &str, chars: usize) -> String {
    let text = text.trim();
    match text.char_indices().nth(chars) {
        Some((end, _)) => format!("{}… (cut)", &text[..end]),
        None => text.to_string(),
    }
}

/// The latest landed value of a recurring objective's metric.
pub(crate) fn landed_value(task: &Task, landings: &[Landing]) -> Option<f64> {
    let name = task.metric.as_deref()?;
    landings
        .iter()
        .filter(|landing| landing.objective == task.id)
        .find_map(|landing| candidate_value(landing.metric.as_deref()?, name))
}

/// Whether an objective is open: a one-shot one with no landing (a landing
/// or an `objective.solved`: the objective's scenario passed at a base commit
/// main already holds), or a recurring one whose latest landed metric misses
/// its target.
pub(crate) fn is_open(task: &Task, landings: &[Landing]) -> bool {
    if task.recurring {
        !landed_value(task, landings).is_some_and(|value| task.meets(value))
    } else {
        !landings.iter().any(|landing| landing.objective == task.id)
    }
}

/// Whether `objective` of `mission` is a one-shot objective: the base-solved
/// rule (`crate::integrate`'s `solved_on_base`) ends a one-shot objective on
/// the record; a recurring one stays open until its metric meets its target,
/// and a scenario passing on the base says nothing about the metric the gate
/// would land. False when the backlog cannot be read (the old verdict
/// stands).
pub(crate) fn one_shot(mission: &Mission, objective: u32) -> bool {
    let Some(backlog) = mission
        .objectives
        .as_ref()
        .and_then(|config| config.get("backlog"))
    else {
        return false;
    };
    let Some(backlog) = backlog.as_str().map(std::path::Path::new) else {
        return false;
    };
    load_backlog(backlog)
        .ok()
        .and_then(|tasks| tasks.into_iter().find(|task| task.id == objective))
        .is_some_and(|task| !task.recurring)
}

/// Whether the fleet may be given `task`: open, and not the owner's to
/// judge.
fn assignable(task: &Task, landings: &[Landing]) -> bool {
    is_open(task, landings) && !task.owner_judged
}

/// The lowest open objective the fleet may be given: what one free slot
/// gets with nothing in flight. An owner-judged objective is never it.
#[cfg(test)]
pub(crate) fn next_open<'a>(backlog: &'a [Task], landings: &[Landing]) -> Option<&'a Task> {
    backlog.iter().find(|task| assignable(task, landings))
}

/// Whether every objective `task` comes after is solved: not open in
/// `backlog`, or, when the backlog does not hold it, landed.
fn ordered(task: &Task, backlog: &[Task], landings: &[Landing]) -> bool {
    task.after.iter().all(|before| {
        backlog
            .iter()
            .find(|other| other.id == *before)
            .map_or_else(
                || landings.iter().any(|landing| landing.objective == *before),
                |other| !is_open(other, landings),
            )
    })
}

/// What is wrong with the order `backlog` names: each `after` id the
/// backlog does not hold (its objective waits until that id lands, which
/// no task of this backlog can make happen), and each set of objectives
/// that come after each other (a strongly connected component of more than
/// one, or one that comes after itself: none of them is ever assigned),
/// once each, by id. Linear in the objectives and their `after` ids
/// (Tarjan's components, without recursion): it runs in every dispatcher
/// pass, and a long chain of objectives each after the ones before must
/// cost no more than its length.
pub(crate) fn order_problems(backlog: &[Task]) -> Vec<String> {
    let index: BTreeMap<u32, usize> = backlog
        .iter()
        .enumerate()
        .map(|(at, task)| (task.id, at))
        .collect();
    let mut problems = Vec::new();
    for task in backlog {
        for before in &task.after {
            if !index.contains_key(before) {
                problems.push(format!(
                    "objective {} comes after {before}, which the backlog does not hold: it \
                     waits until {before} lands",
                    task.id
                ));
            }
        }
    }
    let edges: Vec<Vec<usize>> = backlog
        .iter()
        .map(|task| {
            task.after
                .iter()
                .filter_map(|id| index.get(id).copied())
                .collect()
        })
        .collect();
    let mut cycles: Vec<Vec<u32>> = components(&edges)
        .into_iter()
        .filter(|component| component.len() > 1 || edges[component[0]].contains(&component[0]))
        .map(|component| {
            let mut ids: Vec<u32> = component.iter().map(|at| backlog[*at].id).collect();
            ids.sort_unstable();
            ids
        })
        .collect();
    cycles.sort();
    for ids in cycles {
        problems.push(if let [id] = ids.as_slice() {
            format!("objective {id} comes after itself: it is never assigned")
        } else {
            let shown: Vec<String> = ids.iter().map(u32::to_string).collect();
            format!(
                "objectives {} come after each other: none of them is ever assigned",
                shown.join(", ")
            )
        });
    }
    problems
}

/// The strongly connected components of the graph `edges` (node `n`'s
/// successors are `edges[n]`), by Tarjan's algorithm with an explicit
/// stack: O(nodes + edges).
fn components(edges: &[Vec<usize>]) -> Vec<Vec<usize>> {
    const UNSEEN: usize = usize::MAX;
    let mut order = vec![UNSEEN; edges.len()];
    let mut low = vec![0; edges.len()];
    let mut on_stack = vec![false; edges.len()];
    let mut stack = Vec::new();
    let mut found = Vec::new();
    let mut next = 0;
    for root in 0..edges.len() {
        if order[root] != UNSEEN {
            continue;
        }
        // Each frame: a node and how many of its edges were followed.
        let mut frames = vec![(root, 0)];
        order[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on_stack[root] = true;
        while let Some(&mut (node, ref mut edge)) = frames.last_mut() {
            if let Some(&target) = edges[node].get(*edge) {
                *edge += 1;
                if order[target] == UNSEEN {
                    order[target] = next;
                    low[target] = next;
                    next += 1;
                    stack.push(target);
                    on_stack[target] = true;
                    frames.push((target, 0));
                } else if on_stack[target] {
                    low[node] = low[node].min(order[target]);
                }
                continue;
            }
            frames.pop();
            if let Some(&(parent, _)) = frames.last() {
                low[parent] = low[parent].min(low[node]);
            }
            if low[node] == order[node] {
                let mut component = Vec::new();
                while let Some(member) = stack.pop() {
                    on_stack[member] = false;
                    component.push(member);
                    if member == node {
                        break;
                    }
                }
                found.push(component);
            }
        }
    }
    found
}

/// The objectives that fill `free` slots: the open ones the fleet may be
/// given ([`assignable`]: an owner-judged objective is never one) without a
/// task in flight (`in_flight`) whose `after` objectives are solved, fewest
/// attempts first (yields, then failed attempts since the last yield, then
/// the lowest id), at most `free` of them.
pub(crate) fn choose<'a>(
    backlog: &'a [Task],
    landings: &[Landing],
    in_flight: &BTreeSet<u32>,
    attempts: &BTreeMap<u32, Attempts>,
    held: &BTreeSet<u32>,
    free: usize,
) -> Vec<&'a Task> {
    let mut open: Vec<&Task> = backlog
        .iter()
        .filter(|task| {
            assignable(task, landings)
                && !in_flight.contains(&task.id)
                && !held.contains(&task.id)
                && ordered(task, backlog, landings)
        })
        .collect();
    open.sort_by_key(|task| {
        let tried = attempts.get(&task.id).cloned().unwrap_or_default();
        (tried.yields, tried.failures.len(), task.id)
    });
    open.truncate(free);
    open
}

/// One outcome of an objective mission's task, oldest first
/// (`PostgresStore::objective_outcomes`): a gate verdict, an abort or a
/// yield.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Outcome {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    pub(crate) objective: u32,
    /// The task the outcome is about, and the record's event id and time
    /// (the overrun's, for a continuation and its hold).
    #[serde(default)]
    pub(crate) task: i64,
    #[serde(default)]
    pub(crate) at_ms: Option<i64>,
    /// A `task.over_budget`'s work: the task it continues (a second overrun
    /// in a chain), the branch, its commit and the diff stat against main,
    /// its tool calls, budget and reason.
    #[serde(default)]
    pub(crate) continues: Option<i64>,
    #[serde(default)]
    pub(crate) chain: Option<bool>,
    #[serde(default)]
    pub(crate) branch: Option<String>,
    #[serde(default)]
    pub(crate) commit: Option<String>,
    #[serde(default)]
    pub(crate) diff: Option<String>,
    #[serde(default)]
    pub(crate) tool_calls: Option<i64>,
    #[serde(default)]
    pub(crate) budget: Option<serde_json::Value>,
    #[serde(default)]
    pub(crate) reason: Option<String>,
    #[serde(default)]
    pub(crate) verdict: Option<String>,
    /// The budget measure that ended the task: `active` for the
    /// integrator's active-time budget, on its abort record and on the
    /// overrun record that replaced it; absent on older aborts and on the
    /// store's.
    #[serde(default)]
    pub(crate) measure: Option<String>,
    /// The task yielded its objective's slot by this record: a
    /// `task.cancelled` of the integrator's, or a `task.aborted` whose
    /// abort was the objective's last failed attempt before its limit.
    #[serde(default)]
    pub(crate) yielded: Option<bool>,
}

impl Outcome {
    /// The failed attempt this outcome is, if it is one: evidence against
    /// the work (the module comment argues each case). A gate's `rejected`
    /// or `incompatible`, and a task aborted by the active-time budget
    /// (`measure: "active"`; the integrator's overrun record of that budget
    /// is a continuation, not a failed attempt). Never a `conflict` (main
    /// moved), an abort without a measure (the old wall-clock budget's, or
    /// the store's for a stale landing), or the host's verdicts.
    fn failure(&self) -> Option<&str> {
        match (self.kind.as_str(), self.verdict.as_deref()) {
            ("task.aborted", _) if self.measure.as_deref() == Some(ACTIVE_MEASURE) => {
                Some("aborted over budget")
            }
            ("task.gated", Some(verdict @ ("rejected" | "incompatible"))) => Some(verdict),
            _ => None,
        }
    }

    /// A yield: an abort that yields counts as the failure that reached
    /// the limit, then as the yield, which ends the count.
    fn is_yield(&self) -> bool {
        self.kind == "task.cancelled" || (self.kind == "task.aborted" && self.yielded == Some(true))
    }
}

/// An objective's latest overrun (`task.over_budget`), and the work its next
/// task continues.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Overrun {
    /// The overrun task.
    pub(crate) task: i64,
    /// The task it continues, for a second overrun of a chain.
    pub(crate) continues: Option<i64>,
    /// Whether it is a second overrun of a chain (no configure between): one
    /// that holds its objective and goes to the supervisor.
    pub(crate) chain: bool,
    pub(crate) reason: String,
    pub(crate) tool_calls: i64,
    /// `spawn/<agent>`: where the work was kept.
    pub(crate) branch: String,
    /// The commit the branch holds, when the push made it there.
    pub(crate) commit: Option<String>,
    /// `git diff --stat main <commit>`, when there is a commit.
    pub(crate) diff: Option<String>,
    pub(crate) budget: Option<serde_json::Value>,
    /// When the record was written: a `mission.configured` newer than it
    /// ends the chain's hold.
    pub(crate) at_ms: i64,
}

/// Every objective's latest overrun, from `outcomes` (oldest first).
pub(crate) fn overruns(outcomes: &[Outcome]) -> BTreeMap<u32, Overrun> {
    let mut overruns: BTreeMap<u32, Overrun> = BTreeMap::new();
    for outcome in outcomes.iter().filter(|o| o.kind == "task.over_budget") {
        overruns.insert(
            outcome.objective,
            Overrun {
                task: outcome.task,
                continues: outcome.continues,
                chain: outcome.chain == Some(true),
                reason: outcome.reason.clone().unwrap_or_default(),
                tool_calls: outcome.tool_calls.unwrap_or_default(),
                branch: outcome.branch.clone().unwrap_or_default(),
                commit: outcome.commit.clone(),
                diff: outcome.diff.clone(),
                budget: outcome.budget.clone(),
                at_ms: outcome.at_ms.unwrap_or_default(),
            },
        );
    }
    overruns
}

/// Whether an overrun holds its objective: it continues an overrun task of
/// the same objective, and the mission was not configured since its record.
pub(crate) fn holds(overrun: &Overrun, configured_ms: Option<i64>) -> bool {
    overrun.chain && configured_ms.is_none_or(|at| at <= overrun.at_ms)
}

/// An objective's attempts, read from its tasks' outcomes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Attempts {
    /// Times one of its tasks yielded the slot.
    pub(crate) yields: u32,
    /// Its failed attempts since its last yield or acceptance, oldest
    /// first.
    pub(crate) failures: Vec<String>,
}

/// Every objective's attempts, from `outcomes` (oldest first). An accepted
/// verdict ends the failures before it: the work made it (a recurring
/// objective whose landing missed its target starts afresh).
pub(crate) fn attempts(outcomes: &[Outcome]) -> BTreeMap<u32, Attempts> {
    let mut attempts: BTreeMap<u32, Attempts> = BTreeMap::new();
    for outcome in outcomes {
        let entry = attempts.entry(outcome.objective).or_default();
        if outcome.is_yield() {
            entry.yields += 1;
            entry.failures.clear();
        } else if outcome.kind == "task.gated" && outcome.verdict.as_deref() == Some("accepted") {
            entry.failures.clear();
        } else if let Some(failure) = outcome.failure() {
            entry.failures.push(failure.to_string());
        }
    }
    attempts
}

/// The `measure` of an abort the active-time budget made: the only abort
/// that counts as a failed attempt.
pub(crate) const ACTIVE_MEASURE: &str = "active";

/// Failed attempts after which an objective's task yields its slot:
/// `SPAWN_OBJECTIVE_ATTEMPTS`, a whole number from 1 up; anything else is 3.
pub(crate) fn attempt_limit(text: Option<&str>) -> usize {
    const DEFAULT: usize = 3;
    text.and_then(|text| text.trim().parse::<usize>().ok())
        .filter(|limit| *limit > 0)
        .unwrap_or(DEFAULT)
}

/// A task that yields its objective's slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Yield {
    pub(crate) task: i64,
    pub(crate) objective: u32,
    /// The objective's failed attempts since its last yield, oldest first.
    pub(crate) failures: Vec<String>,
    /// The mission's run of gate rejections when every one of them was a
    /// task of this objective and the run reached the halt (the store's
    /// `gate_rejection_objective`): the objective yields instead of the
    /// mission halting. None when the yield is the failed attempts' alone.
    pub(crate) rejections: Option<u32>,
}

impl Yield {
    /// The one line the record and the supervisor get.
    pub(crate) fn reason(&self) -> String {
        if let Some(rejections) = self.rejections {
            return format!(
                "{rejections} gate rejections in a row, all objective {}'s tasks: task #{} \
                 yields its slot to the next objective instead of halting the mission, its \
                 work set aside",
                self.objective, self.task
            );
        }
        format!(
            "objective {} had {} failed attempts since its last turn ({}); task #{} yields its \
             slot to the next objective, its work set aside",
            self.objective,
            self.failures.len(),
            self.failures.join(", "),
            self.task
        )
    }
}

/// The tasks of `mission` being worked (assigned, delivered, or sent back
/// to revise) whose objective has `limit` failed attempts or more since its
/// last yield, but none in `settled` (aborted this pass, or under a gate
/// now). A reported task waits for its gate, whose verdict settles it: it
/// lands, or goes back to revise and yields then (task 106 of mission 4 had
/// four failures when its fifth verdict was `accepted`).
///
/// A task of the objective that the mission's run of gate rejections names
/// (`gate_rejection_objective`, every rejection of the run that
/// objective's) yields too once the run reaches the halt, whatever `limit`
/// is: three rejections of one objective say the objective is hard, not
/// that the gate rejects everything, so it yields and the mission does not
/// halt ([`crate::integrate::halt_reason`]).
pub(crate) fn yields(
    mission: &Mission,
    attempts: &BTreeMap<u32, Attempts>,
    limit: usize,
    settled: &BTreeSet<i64>,
) -> Vec<Yield> {
    mission
        .tasks
        .iter()
        .filter(|task| {
            matches!(
                task.state,
                TaskState::Assigned | TaskState::Delivered | TaskState::Revise
            ) && !settled.contains(&task.id)
        })
        .filter_map(|task| {
            let objective = task.objective?;
            let failures = attempts
                .get(&objective)
                .map(|tried| tried.failures.clone())
                .unwrap_or_default();
            let rejections = (mission.gate_rejection_objective == Some(objective)
                && mission.gate_rejections >= crate::integrate::HALT_AFTER)
                .then_some(mission.gate_rejections);
            (failures.len() >= limit || rejections.is_some()).then_some(Yield {
                task: task.id,
                objective,
                failures,
                rejections,
            })
        })
        .collect()
}

/// An objective mission's worker limit: `objectives ->> 'workers'`, 1 when
/// absent (every mission configured before the limit existed).
pub(crate) fn workers(objectives: &serde_json::Value) -> usize {
    objectives
        .get("workers")
        .and_then(serde_json::Value::as_u64)
        .and_then(|workers| usize::try_from(workers).ok())
        .unwrap_or(1)
        .max(1)
}

/// What a continuation is told: the overrun task it continues, the branch,
/// the commit and the diff stat its workspace holds, and its budget.
pub(crate) fn continuation_note(over: &Overrun) -> String {
    let mut note = format!(
        "\nYour task continues task #{} of this objective, which went over its budget: {}. \
         It made {} tool calls. Its work is safe on branch {}",
        over.task,
        cut(&over.reason, REASON_CHARS),
        over.tool_calls,
        over.branch
    );
    match (&over.commit, &over.diff) {
        (Some(commit), diff) => {
            let _ = write!(note, ", at commit {commit}");
            match diff.as_deref().filter(|diff| !diff.trim().is_empty()) {
                Some(diff) => {
                    let _ = writeln!(
                        note,
                        "; against main it holds:\n{}\nThat work is in your workspace: \
                         continue it, do not redo it.",
                        diff.trim_end()
                    );
                }
                None => {
                    let _ = writeln!(
                        note,
                        ".\nThat work is in your workspace: continue it, do not redo it."
                    );
                }
            }
        }
        _ => {
            let _ = writeln!(
                note,
                ", but its push failed: the log says so, and your workspace may not hold all \
                 of it. The report of the task it continues says what it did."
            );
        }
    }
    if let Some(budget) = &over.budget {
        let _ = writeln!(
            note,
            "Your budget is the mission's ({budget}); it may be raised once, for this task \
             only, to at most twice that, by the owner's `spawn mission configure`."
        );
    }
    note
}

/// The brief of an objective task: the objective, the working rules, the
/// work earlier tasks of it left without landing, and what the gate said
/// about earlier attempts.
pub(crate) fn brief(
    task: &Task,
    current: Option<f64>,
    prior: &[PriorWork],
    history: &[GateRecord],
    continues: Option<&Overrun>,
) -> String {
    let mut brief = format!("Objective {}: {}\n\n{}\n", task.id, task.title, task.prompt);
    brief.push_str(
        "\nWork only inside this workspace. Do not run git commands and do not create \
         commits: the host commits your workspace. Do not edit `rsi/`, `scripts/rsi/`, \
         `scripts/blackbox/` or `scripts/e2e.sh`: the gate refuses any change there. \
         A crate new in your tree must carry `[lints]` with `workspace = true` (and \
         nothing else under it) in its Cargo.toml: the gate refuses one without it, and \
         any other change to the lint configuration (`[lints]` tables, clippy.toml, a \
         crate-level `#![allow]`). \
         The gate builds your tree, runs this objective's scenario (it must fail on the \
         current main and pass on yours) and every other check (clippy, the tests, the \
         e2e, the black-box scenarios, the web build). If it rejects your work, you get \
         the task back with its output.\n",
    );
    if task.recurring
        && let Some(name) = &task.metric
    {
        let _ = write!(
            brief,
            "\nObjective metric: `{name}` ({} is better).",
            if task.lower_is_better {
                "lower"
            } else {
                "higher"
            }
        );
        if let Some(value) = current {
            let _ = write!(brief, " Current landed value: {value} {}.", task.unit);
        }
        if let Some(target) = task.target {
            let _ = write!(brief, " Target: {target} {}.", task.unit);
        }
        brief.push('\n');
    }
    if let Some(over) = continues {
        brief.push_str(&continuation_note(over));
    }
    if !prior.is_empty() {
        brief.push_str(
            "\nEarlier tasks on this objective ended without landing, newest first. Their \
             reports below say what each did and found: use them, and do not redo what they \
             established. When your workspace starts from one's work, your first prompt says \
             so.\n",
        );
        for work in prior {
            let _ = write!(
                brief,
                "- task {}: {}",
                work.task,
                work.outcome.as_deref().unwrap_or("cancelled")
            );
            if let Some(reason) = &work.reason {
                let _ = write!(brief, " ({})", cut(reason, REASON_CHARS));
            }
            match &work.commit {
                Some(commit) => {
                    let _ = write!(brief, "; its last checkpoint {commit}");
                }
                None => brief.push_str("; it recorded no checkpoint"),
            }
            brief.push('\n');
            if let Some(report) = work
                .report
                .as_deref()
                .filter(|text| !text.trim().is_empty())
            {
                let _ = writeln!(
                    brief,
                    "  Its report:\n  > {}",
                    cut(report, REPORT_CHARS).replace('\n', "\n  > ")
                );
            }
        }
    }
    if !history.is_empty() {
        brief.push_str("\nEarlier gate verdicts on this objective, newest first:\n");
        for verdict in history {
            let _ = writeln!(
                brief,
                "- task {}: {}{}{}",
                verdict.task,
                if verdict.verdict == "incompatible" {
                    "incompatible (passed the gate alone, failed only merged with main; not a \
                     rejection)"
                } else {
                    &verdict.verdict
                },
                verdict
                    .reason
                    .as_deref()
                    .map_or(String::new(), |reason| format!("; {reason}")),
                verdict
                    .metric
                    .as_deref()
                    .map_or(String::new(), |metric| format!("; {metric}")),
            );
        }
    }
    brief
}

/// Fill an objective mission's free slots: first the tasks of objectives
/// at their limit yield theirs ([`yields`]: worker stopped, `task.cancelled`
/// with `yielded`), then, up to its worker limit, every open task of the
/// mission taking one, the slots go to the open objectives without a task
/// in flight, fewest attempts first ([`choose`]). `settled`: tasks this
/// pass aborted, or under a gate now; they do not yield, and `mission` is
/// the pass's read, so an aborted task's slot frees only from the next
/// pass on. Returns the ids of the tasks assigned. A first assignment that
/// fails is the error; a later one ends the filling, logged, keeping what
/// was assigned.
pub(crate) fn assign_next(
    store: &PostgresStore,
    mission: &Mission,
    settled: &BTreeSet<i64>,
) -> Result<Vec<i64>, String> {
    let Some(config) = &mission.objectives else {
        return Ok(Vec::new());
    };
    let outcomes: Vec<Outcome> = serde_json::from_value(
        store
            .objective_outcomes(mission.id)
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("objective outcomes: {error}"))?;
    let mut attempts = attempts(&outcomes);
    let overruns = overruns(&outcomes);
    let limit = attempt_limit(crate::config::env_value("SPAWN_OBJECTIVE_ATTEMPTS").as_deref());
    let mut yielded = BTreeSet::new();
    for slot in yields(mission, &attempts, limit, settled) {
        let Some(task) = mission.tasks.iter().find(|task| task.id == slot.task) else {
            continue;
        };
        // The Stop first, under its fixed key: if the record then fails,
        // the next pass finds the task still in flight and yields again.
        crate::integrate::stop_worker(store, task, "integrate-yield")?;
        let reason = slot.reason();
        crate::integrate::record(
            store,
            &json!({"type": "task.cancelled", "id": slot.task, "yielded": true,
                    "objective": slot.objective, "failures": slot.failures.len(),
                    "limit": limit, "rejections": slot.rejections, "reason": reason}),
        )?;
        println!("integrate: mission {}: {reason}", mission.id);
        yielded.insert(slot.task);
        let tried = attempts.entry(slot.objective).or_default();
        tried.yields += 1;
        tried.failures.clear();
    }
    let flying: Vec<_> = mission
        .tasks
        .iter()
        .filter(|task| !yielded.contains(&task.id))
        .collect();
    let free = workers(config).saturating_sub(flying.len());
    if free == 0 {
        return Ok(Vec::new());
    }
    let backlog = config
        .get("backlog")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "objectives have no backlog".to_string())?;
    let objectives = load_backlog(Path::new(backlog))?;
    let landings: Vec<Landing> = serde_json::from_value(
        store
            .objective_landings(mission.id)
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("objective landings: {error}"))?;
    let in_flight: BTreeSet<u32> = flying.iter().filter_map(|task| task.objective).collect();
    // An overrun chain whose second overrun is not yet answered by a
    // configure holds its objective: no task is issued for it, and the
    // freed slot goes to the next open objective (the dispatcher tells the
    // supervisor once; this only reads the records).
    let held: BTreeSet<u32> = overruns
        .iter()
        .filter(|(_, overrun)| holds(overrun, mission.configured_ms))
        .map(|(objective, _)| *objective)
        .collect();
    if !held.is_empty() {
        println!(
            "integrate: mission {}: objectives {:?} held after a second overrun, until the \
             mission is configured again",
            mission.id,
            held.iter().collect::<Vec<_>>()
        );
    }
    let mut assigned = Vec::new();
    for task in choose(&objectives, &landings, &in_flight, &attempts, &held, free) {
        match assign(store, mission, task, &landings, overruns.get(&task.id)) {
            Ok(id) => assigned.push(id),
            Err(error) if assigned.is_empty() => return Err(error),
            Err(error) => {
                eprintln!(
                    "integrate: mission {} objective {}: {error}",
                    mission.id, task.id
                );
                break;
            }
        }
    }
    Ok(assigned)
}

/// Assign one objective of `mission`; the task's id.
fn assign(
    store: &PostgresStore,
    mission: &Mission,
    task: &Task,
    landings: &[Landing],
    continues: Option<&Overrun>,
) -> Result<i64, String> {
    let id = i32::try_from(task.id).map_err(|_| format!("objective {} is too large", task.id))?;
    let history: Vec<GateRecord> = serde_json::from_value(
        store
            .objective_history(mission.id, id, HISTORY)
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("objective history: {error}"))?;
    let prior: Vec<PriorWork> = serde_json::from_value(
        store
            .objective_prior_work(mission.id, id, PRIOR_WORK)
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("objective prior work: {error}"))?;
    let scenario = Path::new(&mission.gate).join(format!("{:03}.sh", task.id));
    let scenario = scenario.is_file().then(|| scenario.display().to_string());
    let text = brief(
        task,
        landed_value(task, landings),
        &prior,
        &history,
        continues,
    );
    let assigned = store
        .objective_assign_continuing(
            mission.id,
            id,
            scenario.as_deref(),
            &text,
            continues.map(|overrun| overrun.task),
        )
        .map_err(|error| error.to_string())?;
    match continues {
        Some(overrun) => println!(
            "integrate: mission {} objective {} -> task {assigned}, continuing task #{} on {} \
             at {}",
            mission.id,
            task.id,
            overrun.task,
            overrun.branch,
            overrun
                .commit
                .as_deref()
                .unwrap_or("no commit (the push failed)")
        ),
        None => println!(
            "integrate: mission {} objective {} -> task {assigned}",
            mission.id, task.id
        ),
    }
    Ok(assigned)
}

#[cfg(test)]
mod tests {
    use super::{
        ACTIVE_MEASURE, Attempts, Landing, Outcome, PriorWork, Task, attempt_limit, attempts,
        brief, choose, holds, load_backlog, metric_lines, next_open, order_problems, overruns,
        workers, yields,
    };
    use serde_json::json;
    use std::collections::{BTreeMap, BTreeSet};

    fn untried() -> BTreeMap<u32, Attempts> {
        BTreeMap::new()
    }

    fn objective(id: u32, recurring: bool) -> Task {
        Task {
            id,
            title: format!("objective {id}"),
            prompt: "do it".to_string(),
            recurring,
            metric: recurring.then(|| "ingest_total_ns".to_string()),
            unit: "ns".to_string(),
            lower_is_better: true,
            target: recurring.then_some(1100.0),
            after: Vec::new(),
            owner_judged: false,
        }
    }

    fn landing(objective: u32, metric: Option<&str>) -> Landing {
        Landing {
            objective,
            metric: metric.map(str::to_string),
        }
    }

    #[test]
    fn the_lowest_open_objective_is_next() {
        let backlog = [objective(1, false), objective(2, true), objective(3, false)];
        assert_eq!(next_open(&backlog, &[]).map(|task| task.id), Some(1));
        let solved_one = [landing(1, None)];
        assert_eq!(
            next_open(&backlog, &solved_one).map(|task| task.id),
            Some(2)
        );
        let missed = [
            landing(
                2,
                Some("METRIC ingest_total_ns baseline=1500 candidate=1200"),
            ),
            landing(1, None),
        ];
        assert_eq!(next_open(&backlog, &missed).map(|task| task.id), Some(2));
        // Newest first: the latest landing decides.
        let met = [
            landing(
                2,
                Some("METRIC ingest_total_ns baseline=1200 candidate=1021"),
            ),
            landing(
                2,
                Some("METRIC ingest_total_ns baseline=1500 candidate=1200"),
            ),
            landing(1, None),
        ];
        assert_eq!(next_open(&backlog, &met).map(|task| task.id), Some(3));
        let all = [landing(3, None), met[0].clone(), landing(1, None)];
        assert!(next_open(&backlog, &all).is_none());
    }

    fn ids(tasks: &[&Task]) -> Vec<u32> {
        tasks.iter().map(|task| task.id).collect()
    }

    /// An overrun outcome: `task.over_budget` with the work it kept.
    fn overrun_outcome(
        task: i64,
        objective: u32,
        continues: Option<i64>,
        commit: Option<&str>,
        diff: Option<&str>,
    ) -> Outcome {
        Outcome {
            kind: "task.over_budget".to_string(),
            objective,
            task,
            at_ms: Some(1_000),
            continues,
            chain: continues.map(|_| true),
            branch: Some(format!("spawn/agent_worker_objective_{task}")),
            commit: commit.map(str::to_string),
            diff: diff.map(str::to_string),
            tool_calls: Some(6),
            budget: Some(json!({"tool_calls": 5})),
            reason: Some("the task made more than 5 tool calls".to_string()),
            verdict: None,
            measure: None,
            yielded: None,
        }
    }

    /// The next task of an objective is told what it continues: the task,
    /// the branch, the commit and that commit's diff stat, and that the work
    /// is in its workspace to continue.
    #[test]
    fn a_continuation_is_told_the_task_the_branch_the_commit_and_its_diff() {
        let outcome = overrun_outcome(
            84,
            17,
            None,
            Some("d44c6011aa22bb33cc44dd55ee66ff7700112233"),
            Some(" big.txt | 120 ++++++\n 1 file changed, 120 insertions(+)"),
        );
        let overrun = overruns(&[outcome]).remove(&17).expect("the overrun");
        let text = brief(&objective(17, false), None, &[], &[], Some(&overrun));
        assert!(text.contains("continues task #84"), "{text}");
        assert!(
            text.contains("spawn/agent_worker_objective_84"),
            "the branch: {text}"
        );
        assert!(
            text.contains("d44c6011aa22bb33cc44dd55ee66ff7700112233"),
            "the commit: {text}"
        );
        assert!(text.contains("big.txt"), "the diff: {text}");
        assert!(text.contains("1 file changed, 120 insertions(+)"), "{text}");
        assert!(text.contains("continue it, do not redo it"), "{text}");
        assert!(text.contains("6 tool calls"), "{text}");
        assert!(text.contains("{\"tool_calls\":5}"), "the budget: {text}");
        // A push that failed is said so, and the branch is still named.
        let lost = Outcome {
            commit: None,
            diff: None,
            ..overrun_outcome(84, 17, None, None, None)
        };
        let lost = overruns(&[lost]).remove(&17).expect("the overrun");
        let text = brief(&objective(17, false), None, &[], &[], Some(&lost));
        assert!(text.contains("its push failed"), "{text}");
        assert!(text.contains("spawn/agent_worker_objective_84"), "{text}");
    }

    /// A second overrun in a chain holds its objective until the mission is
    /// configured again; a first overrun holds nothing.
    #[test]
    fn a_second_overrun_holds_its_objective_until_a_configure() {
        let first = overrun_outcome(84, 17, None, Some("aa11bb22"), None);
        let second = overrun_outcome(87, 17, Some(84), Some("aa11bb22"), None);
        let outcomes = [first.clone(), second.clone()];
        let latest = overruns(&outcomes);
        let chain = latest.get(&17).expect("the latest overrun");
        assert_eq!(chain.task, 87, "the latest overrun is the continuation's");
        assert!(
            holds(chain, None),
            "a second overrun holds without a configure"
        );
        assert!(
            holds(chain, Some(1_000)),
            "a configure at the record's own time does not release it"
        );
        assert!(!holds(chain, Some(1_001)), "a newer configure releases it");
        let first = overruns(&[first]).remove(&17).expect("the first overrun");
        assert!(!holds(&first, None), "a first overrun holds nothing");
    }

    /// The first overrun of an objective is continued (not held); the
    /// mission's next open objective takes the freed slot of a held one.
    #[test]
    fn the_next_open_objective_takes_the_slot_of_a_held_one() {
        let backlog = [objective(17, false), objective(18, false)];
        let first = overruns(&[overrun_outcome(84, 17, None, Some("aa11bb22"), None)]);
        let held: BTreeSet<u32> = first
            .iter()
            .filter(|(_, overrun)| holds(overrun, None))
            .map(|(id, _)| *id)
            .collect();
        assert!(held.is_empty(), "the first overrun is continued");
        let chained = overruns(&[overrun_outcome(87, 17, Some(84), Some("aa11bb22"), None)]);
        let held: BTreeSet<u32> = chained
            .iter()
            .filter(|(_, overrun)| holds(overrun, None))
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(held, BTreeSet::from([17]));
        assert_eq!(
            ids(&choose(
                &backlog,
                &[],
                &BTreeSet::new(),
                &untried(),
                &held,
                2
            )),
            vec![18],
            "the freed slot goes to the next open objective"
        );
        assert_eq!(
            ids(&choose(
                &backlog,
                &[],
                &BTreeSet::new(),
                &untried(),
                &BTreeSet::new(),
                2
            )),
            vec![17, 18],
            "without the hold, both are open"
        );
    }

    #[test]
    fn free_slots_get_the_lowest_open_objectives_not_in_flight() {
        let backlog = [
            objective(1, false),
            objective(2, false),
            objective(3, false),
            objective(4, false),
        ];
        let none = BTreeSet::new();
        assert_eq!(
            ids(&choose(
                &backlog,
                &[],
                &none,
                &untried(),
                &BTreeSet::new(),
                3
            )),
            vec![1, 2, 3],
            "lowest first"
        );
        // Solved objectives and those in flight are skipped.
        let solved = [landing(1, None)];
        let flying = BTreeSet::from([2]);
        assert_eq!(
            ids(&choose(
                &backlog,
                &solved,
                &flying,
                &untried(),
                &BTreeSet::new(),
                3
            )),
            vec![3, 4]
        );
        // No free slot, nothing.
        assert!(choose(&backlog, &[], &none, &untried(), &BTreeSet::new(), 0).is_empty());
        // Fewer open objectives than slots: only those.
        let three_solved = [landing(1, None), landing(2, None), landing(3, None)];
        assert_eq!(
            ids(&choose(
                &backlog,
                &three_solved,
                &none,
                &untried(),
                &BTreeSet::new(),
                3
            )),
            vec![4]
        );
    }

    #[test]
    fn a_recurring_objective_in_flight_is_not_chosen_twice() {
        let backlog = [objective(1, true), objective(2, false)];
        // Recurring and not met: open after every landing, but one task at a time.
        let missed = [landing(
            1,
            Some("METRIC ingest_total_ns baseline=1500 candidate=1200"),
        )];
        let none = BTreeSet::new();
        assert_eq!(
            ids(&choose(
                &backlog,
                &missed,
                &none,
                &untried(),
                &BTreeSet::new(),
                2
            )),
            vec![1, 2]
        );
        let flying = BTreeSet::from([1]);
        assert_eq!(
            ids(&choose(
                &backlog,
                &missed,
                &flying,
                &untried(),
                &BTreeSet::new(),
                2
            )),
            vec![2]
        );
    }

    #[test]
    fn the_worker_limit_defaults_to_one() {
        assert_eq!(workers(&json!({"backlog": "/b", "resident": "r"})), 1);
        assert_eq!(
            workers(&json!({"backlog": "/b", "resident": "r", "workers": 3})),
            3
        );
        assert_eq!(workers(&json!({"workers": 0})), 1, "never below one");
    }

    #[test]
    fn a_metric_line_is_read() {
        let metrics = metric_lines("METRIC ingest_total_ns baseline=1573 candidate=1021");
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].name, "ingest_total_ns");
        assert_eq!(metrics[0].baseline, Some(1573.0));
        assert_eq!(metrics[0].candidate, Some(1021.0));
    }

    #[test]
    fn a_recurring_objective_needs_a_metric_and_a_target() {
        let dir = std::env::temp_dir().join(format!("spawn-backlog-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let write = |body: &str| std::fs::write(dir.join("001.json"), body).expect("write");
        write(
            r#"{"id": 1, "prompt": "go faster", "recurring": true, "metric": "ns", "target": 900}"#,
        );
        assert_eq!(load_backlog(&dir).expect("valid").len(), 1);
        write(r#"{"id": 1, "prompt": "go faster", "recurring": true, "metric": "ns"}"#);
        assert!(load_backlog(&dir).is_err(), "never met");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An outcome as the store gives it. A conflict's `merge` (how main went
    /// into the workspace) is in the record but no longer read: no conflict
    /// counts, however it merged.
    fn outcome(kind: &str, objective: u32, verdict: Option<&str>, _merge: Option<&str>) -> Outcome {
        Outcome {
            kind: kind.to_string(),
            objective,
            task: 0,
            at_ms: None,
            continues: None,
            chain: None,
            branch: None,
            commit: None,
            diff: None,
            tool_calls: None,
            budget: None,
            reason: None,
            verdict: verdict.map(str::to_string),
            measure: None,
            yielded: None,
        }
    }

    /// An abort the active-time budget made, as `enforce_budget` records it.
    fn active_abort(objective: u32) -> Outcome {
        let mut abort = outcome("task.aborted", objective, None, None);
        abort.measure = Some(ACTIVE_MEASURE.to_string());
        abort
    }

    fn gated(objective: u32, verdict: &str) -> Outcome {
        outcome("task.gated", objective, Some(verdict), None)
    }

    /// An objective mission with `workers` slots and these tasks in flight:
    /// `(id, objective, state)`.
    fn mission(workers: u32, tasks: &[(i64, u32, &str)]) -> crate::integrate::Mission {
        serde_json::from_value(json!({
            "id": 4, "gate": "/g", "accept": "gate",
            "objectives": {"backlog": "/b", "resident": "rsi", "workers": workers},
            "tasks": tasks.iter().map(|(id, objective, state)| json!({
                "id": id, "agent_id": format!("agent_worker_objective_{id}"),
                "session_id": format!("session_worker_objective_{id}"),
                "state": state, "objective": objective,
            })).collect::<Vec<_>>(),
        }))
        .expect("a mission")
    }

    /// Only evidence against the work counts: a rejection, an
    /// incompatibility, an abort the active-time budget made. Main moving
    /// (a conflict, however it was merged), an abort without a measure (the
    /// old wall-clock budget's, the store's for a stale landing) and the
    /// host's verdicts count for nothing; a yield or an acceptance starts
    /// the count again.
    #[test]
    fn failed_attempts_are_evidence_against_the_work() {
        let outcomes = [
            gated(1, "rejected"),
            gated(1, "incompatible"),
            active_abort(1),
            // Main moved under the task: nothing, whatever the merge.
            outcome("task.gated", 1, Some("conflict"), Some("marked")),
            outcome("task.gated", 1, Some("conflict"), Some("unmarked")),
            outcome("task.gated", 1, Some("conflict"), Some("pending")),
            outcome("task.gated", 1, Some("conflict"), None),
            outcome("task.gated", 1, Some("conflict"), Some("not_mounted")),
            outcome("task.gated", 1, Some("conflict"), Some("busy")),
            outcome("task.gated", 1, Some("conflict"), Some("failed")),
            outcome("task.gated", 1, Some("conflict"), Some("merged")),
            // An abort no active-time measure made: nothing.
            outcome("task.aborted", 1, None, None),
            // The host's: nothing.
            gated(1, "no_room"),
            gated(1, "host_fault"),
            gated(1, "stale"),
            gated(1, "aborted"),
        ];
        let counted = attempts(&outcomes);
        assert_eq!(counted[&1].yields, 0);
        assert_eq!(
            counted[&1].failures,
            ["rejected", "incompatible", "aborted over budget"]
        );
        let after = attempts(&[
            gated(2, "rejected"),
            outcome("task.cancelled", 2, None, None),
            gated(2, "rejected"),
            gated(3, "rejected"),
            gated(3, "rejected"),
            gated(3, "accepted"),
            gated(3, "incompatible"),
        ]);
        assert_eq!(
            after[&2],
            Attempts {
                yields: 1,
                failures: vec!["rejected".to_string()]
            }
        );
        assert_eq!(
            after[&3].failures,
            ["incompatible"],
            "an acceptance ends the count"
        );
    }

    /// Fewest yields first, then fewest failed attempts, then the lowest id.
    #[test]
    fn free_slots_go_to_the_fewest_attempts() {
        let backlog: Vec<Task> = [19, 21, 23, 25, 26].map(|id| objective(id, false)).to_vec();
        let tried = attempts(&[
            gated(19, "rejected"),
            gated(19, "rejected"),
            outcome("task.cancelled", 19, None, None),
            gated(21, "rejected"),
            gated(23, "rejected"),
            gated(23, "rejected"),
        ]);
        let none = BTreeSet::new();
        assert_eq!(
            ids(&choose(&backlog, &[], &none, &tried, &BTreeSet::new(), 5)),
            vec![25, 26, 21, 23, 19]
        );
        assert_eq!(
            ids(&choose(&backlog, &[], &none, &tried, &BTreeSet::new(), 1)),
            vec![25]
        );
    }

    /// Three slots held by three objectives at the limit: the tasks being
    /// worked again yield, and the others get the slots, 025 and 026 first.
    /// A healthy objective (under the limit) and a reported task keep
    /// theirs; a task under a gate or aborted this pass does not yield.
    #[test]
    fn objectives_at_the_limit_yield_their_slots() {
        let three = |objective| {
            [
                gated(objective, "rejected"),
                gated(objective, "incompatible"),
                active_abort(objective),
            ]
        };
        let outcomes: Vec<Outcome> = [three(19), three(21), three(23)].concat();
        let tried = attempts(&outcomes);
        let stuck = mission(
            3,
            &[
                (127, 19, "revise"),
                (106, 21, "delivered"),
                (160, 23, "assigned"),
            ],
        );
        let none = BTreeSet::new();
        let slots = yields(&stuck, &tried, 3, &none);
        assert_eq!(
            slots
                .iter()
                .map(|slot| (slot.task, slot.objective))
                .collect::<Vec<_>>(),
            [(127, 19), (106, 21), (160, 23)]
        );
        assert!(
            slots[0]
                .reason()
                .contains("objective 19 had 3 failed attempts")
        );
        assert!(slots[0].reason().contains("task #127 yields its slot"));
        // After the yields: every slot free, the others first.
        let mut after = tried.clone();
        for slot in &slots {
            let entry = after.entry(slot.objective).or_default();
            entry.yields += 1;
            entry.failures.clear();
        }
        let backlog: Vec<Task> = [19, 21, 23, 25, 26].map(|id| objective(id, false)).to_vec();
        assert_eq!(
            ids(&choose(
                &backlog,
                &[],
                &BTreeSet::new(),
                &after,
                &BTreeSet::new(),
                3
            )),
            vec![25, 26, 19]
        );

        // Under the limit, reported, under a gate, aborted this pass: none yields.
        assert!(yields(&stuck, &tried, 4, &none).is_empty(), "healthy");
        let waiting = mission(
            3,
            &[
                (127, 19, "reported"),
                (106, 21, "landing"),
                (160, 23, "accepted"),
            ],
        );
        assert!(yields(&waiting, &tried, 3, &none).is_empty());
        assert!(yields(&stuck, &tried, 3, &BTreeSet::from([127, 106, 160])).is_empty());
        // An objective with no failures is untouched at any limit.
        let healthy = mission(1, &[(200, 30, "revise")]);
        assert!(yields(&healthy, &tried, 1, &none).is_empty());

        // Three gate rejections in a row, all objective 30's: it yields at
        // any limit, with the run in its record's reason; two do not, nor a
        // run that names another objective, nor its task under a gate.
        let mut rejected = mission(1, &[(200, 30, "revise")]);
        rejected.gate_rejections = 3;
        rejected.gate_rejection_objective = Some(30);
        let slots = yields(&rejected, &tried, 99, &none);
        assert_eq!(slots.len(), 1);
        assert_eq!((slots[0].task, slots[0].rejections), (200, Some(3)));
        assert!(
            slots[0]
                .reason()
                .starts_with("3 gate rejections in a row, all objective 30's tasks: task #200"),
            "{}",
            slots[0].reason()
        );
        assert!(yields(&rejected, &tried, 99, &BTreeSet::from([200])).is_empty());
        rejected.gate_rejections = 2;
        assert!(yields(&rejected, &tried, 99, &none).is_empty(), "two");
        rejected.gate_rejections = 3;
        rejected.gate_rejection_objective = Some(31);
        assert!(
            yields(&rejected, &tried, 99, &none).is_empty(),
            "another's run"
        );
    }

    /// Mission 4 as the live log had it at 2026-11-07 23:05:46, when task
    /// 113 (objective 023) was aborted over budget: every gate verdict,
    /// abort and yield of its objective tasks until then (read-only, the
    /// store's objective outcomes). Then, 127 (019) and 106 (021) were
    /// reported, and the old rule gave the freed slot to 023 again (task
    /// 160, five seconds later). Now neither reported task yields (106 went
    /// on to land, 127 to be accepted), and the slot goes to 025: 023 has a
    /// failed attempt (113's rejection; its conflicts were main moving) and
    /// 025 none.
    #[test]
    fn mission_4_at_23_05_assigns_objective_25() {
        let outcomes: Vec<Outcome> = serde_json::from_str(include_str!(
            "../tests/data/mission-4-outcomes-2026-11-07T23-05-46.json"
        ))
        .expect("the outcomes");
        let tried = attempts(&outcomes);
        assert_eq!(tried[&19].failures, ["incompatible"]);
        assert_eq!(tried[&21].failures, ["rejected"]);
        assert_eq!(tried[&23].failures, ["rejected"]);
        let after_abort = mission(3, &[(127, 19, "reported"), (106, 21, "reported")]);
        let none = BTreeSet::new();
        assert!(yields(&after_abort, &tried, attempt_limit(None), &none).is_empty());
        // The deployed backlog then: open are 019, 021, 023, 025-034.
        let backlog: Vec<Task> = [19, 21, 23, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34]
            .map(|id| objective(id, false))
            .to_vec();
        let flying = BTreeSet::from([19, 21]);
        assert_eq!(
            ids(&choose(&backlog, &[], &flying, &tried, &BTreeSet::new(), 1)),
            vec![25]
        );
        // Had 127 been sent back to revise, it would keep its slot too: one
        // failed attempt (the old rule counted seven and yielded it).
        let sent_back = mission(3, &[(127, 19, "revise"), (106, 21, "reported")]);
        assert!(yields(&sent_back, &tried, attempt_limit(None), &none).is_empty());
        assert_eq!(
            ids(&choose(
                &backlog,
                &[],
                &BTreeSet::from([21]),
                &tried,
                &BTreeSet::new(),
                2
            )),
            vec![25, 26]
        );
    }

    /// Mission 4 as the live log had it at 2026-11-08 04:47:05, right after
    /// the owner batch deployed: every outcome of its objective tasks up to
    /// task 167's conflict (seq 1027), without the yield of 166 (seq 1025)
    /// that the old rule made then (read-only, the store's objective
    /// outcomes). The old rule set aside 166 (objective 025: "aborted over
    /// budget, rejected, conflict") and 167 (023: "conflict, conflict,
    /// conflict, rejected, aborted over budget, aborted over budget,
    /// conflict"), both reviewed and passing. The aborts were the old
    /// budget's (113 by the wall clock at 23:05, 160 and 164 at 300 tool
    /// calls), recorded without a measure, and the conflicts were main
    /// moving. Now each has one failed attempt, its rejection, and neither
    /// yields.
    #[test]
    fn mission_4_at_04_47_sets_aside_neither_166_nor_167() {
        let outcomes: Vec<Outcome> = serde_json::from_str(include_str!(
            "../tests/data/mission-4-outcomes-2026-11-08T04-47-05.json"
        ))
        .expect("the outcomes");
        let tried = attempts(&outcomes);
        assert_eq!(tried[&25].failures, ["rejected"]);
        assert_eq!(tried[&23].failures, ["rejected"]);
        assert_eq!(tried[&25].yields, 0);
        assert_eq!(tried[&23].yields, 0);
        // Both sent back to resolve their conflicts.
        let at_04_47 = mission(3, &[(166, 25, "revise"), (167, 23, "revise")]);
        assert!(
            yields(&at_04_47, &tried, attempt_limit(None), &BTreeSet::new()).is_empty(),
            "neither 166 nor 167 yields"
        );
        // An objective failing for real still yields: three rejections, in
        // the shape of the live records, with conflicts between them that
        // do not change the count.
        let mut failing = outcomes.clone();
        for (seq, verdict, merge) in [
            (2000, "rejected", None),
            (2001, "conflict", Some("marked")),
            (2002, "rejected", None),
            (2003, "rejected", None),
        ] {
            failing.push(
                serde_json::from_value(json!({
                    "seq": seq, "type": "task.gated", "task": 166, "objective": 26,
                    "verdict": verdict, "merge": merge, "measure": null, "yielded": null,
                }))
                .expect("an outcome"),
            );
        }
        let tried = attempts(&failing);
        assert_eq!(tried[&26].failures, ["rejected", "rejected", "rejected"]);
        let stuck = mission(3, &[(166, 25, "revise"), (200, 26, "revise")]);
        let slots = yields(&stuck, &tried, attempt_limit(None), &BTreeSet::new());
        assert_eq!(
            slots
                .iter()
                .map(|slot| (slot.task, slot.objective))
                .collect::<Vec<_>>(),
            [(200, 26)]
        );
        // So does one whose aborts the active-time budget made.
        let mut aborted = outcomes;
        aborted.extend([active_abort(27), gated(27, "rejected"), active_abort(27)]);
        assert_eq!(attempts(&aborted)[&27].failures.len(), 3);
    }

    #[test]
    fn the_attempt_limit_is_a_whole_number_from_one() {
        assert_eq!(attempt_limit(None), 3);
        assert_eq!(attempt_limit(Some(" 5 ")), 5);
        for bad in ["0", "-1", "two", "", "2.5"] {
            assert_eq!(attempt_limit(Some(bad)), 3, "{bad:?}");
        }
    }

    /// A re-assigned objective's brief carries its predecessors' work: id,
    /// outcome and reason, last checkpoint, report, and the instruction to
    /// use the reports; it sends no worker to a branch it cannot read.
    #[test]
    fn a_brief_carries_the_earlier_tasks_work() {
        let prior = [
            PriorWork {
                task: 155,
                commit: Some("c245db93e981880756015f1efac78939fbbfe11c".to_string()),
                outcome: Some("aborted".to_string()),
                reason: Some("the task ran past its 90-minute budget".to_string()),
                report: Some("## Outcome\nmostly done\n\n## Left\nthe tests".to_string()),
            },
            PriorWork {
                task: 150,
                commit: None,
                outcome: Some("yielded".to_string()),
                reason: None,
                report: Some("x".repeat(5000)),
            },
        ];
        let text = brief(&objective(59, false), None, &prior, &[], None);
        assert!(text.contains("Their reports below say what each did and found"));
        assert!(!text.contains("branch"), "{text}");
        assert!(text.contains(
            "- task 155: aborted (the task ran past its 90-minute budget); its last checkpoint \
             c245db93e981880756015f1efac78939fbbfe11c"
        ));
        assert!(text.contains("  > mostly done\n"));
        assert!(text.contains("- task 150: yielded; it recorded no checkpoint"));
        assert!(text.contains("… (cut)"), "a long report is cut");
        assert!(text.len() < 5000);
        let first = brief(&objective(59, false), None, &[], &[], None);
        assert!(!first.contains("Earlier tasks"), "a first task has none");
        assert!(
            first.contains(
                "A crate new in your tree must carry `[lints]` with `workspace = true` (and \
                 nothing else under it) in its Cargo.toml"
            ),
            "every brief carries the lint rule the gate holds a new crate to: {first}"
        );
    }

    /// A yield ends an objective's count, and its next task starts afresh;
    /// an abort that was recorded as the yield (a task.aborted with
    /// `yielded`, from before an overrun was its own record) counts so.
    /// After that an abort the active-time budget made is one failed
    /// attempt; one without a measure is none.
    #[test]
    fn a_yield_ends_the_count_and_an_abort_counts_by_its_measure() {
        let mut aborted = outcome("task.aborted", 23, None, None);
        aborted.yielded = Some(true);
        let outcomes = [
            gated(23, "rejected"),
            gated(23, "rejected"),
            aborted,
            gated(25, "rejected"),
            outcome("task.cancelled", 25, None, None),
        ];
        let tried = attempts(&outcomes);
        assert_eq!(
            tried[&23],
            Attempts {
                yields: 1,
                failures: Vec::new()
            }
        );
        let backlog: Vec<Task> = [23, 25, 26].map(|id| objective(id, false)).to_vec();
        assert_eq!(
            ids(&choose(
                &backlog,
                &[],
                &BTreeSet::new(),
                &tried,
                &BTreeSet::new(),
                3
            )),
            vec![26, 23, 25],
            "the untried first, then the yielded"
        );
        let next = mission(1, &[(200, 23, "assigned")]);
        assert!(
            yields(&next, &tried, 3, &BTreeSet::new()).is_empty(),
            "its next task does not yield at once"
        );
        // An abort under the limit stays a failure, as before, when the
        // active-time budget made it; one without a measure is none.
        let plain = attempts(&[active_abort(23)]);
        assert_eq!(plain[&23].failures, ["aborted over budget"]);
        assert_eq!(plain[&23].yields, 0);
        let old = attempts(&[outcome("task.aborted", 23, None, None)]);
        assert!(old[&23].failures.is_empty());
    }

    /// An objective that names the objectives it comes after is not chosen
    /// while one of them is open; a named objective the backlog does not
    /// hold must have landed. `after` is read from the backlog's JSON, and
    /// one that is not a list of ids is an error.
    #[test]
    fn an_objective_waits_for_those_it_comes_after() {
        let mut later = objective(30, false);
        later.after = vec![28, 29];
        let mut gone = objective(32, false);
        gone.after = vec![22];
        let backlog = vec![objective(28, false), objective(29, false), later, gone];
        let none = BTreeSet::new();
        assert_eq!(
            ids(&choose(
                &backlog,
                &[],
                &none,
                &untried(),
                &BTreeSet::new(),
                4
            )),
            vec![28, 29]
        );
        let one = [landing(28, None)];
        assert_eq!(
            ids(&choose(
                &backlog,
                &one,
                &none,
                &untried(),
                &BTreeSet::new(),
                4
            )),
            vec![29]
        );
        let both = [landing(29, None), landing(28, None), landing(22, None)];
        assert_eq!(
            ids(&choose(
                &backlog,
                &both,
                &none,
                &untried(),
                &BTreeSet::new(),
                4
            )),
            vec![30, 32]
        );

        let dir = std::env::temp_dir().join(format!("spawn-backlog-after-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("030.json"),
            r#"{"id": 30, "prompt": "p", "after": [28, 29]}"#,
        )
        .expect("write");
        std::fs::write(dir.join("031.json"), r#"{"id": 31, "prompt": "p"}"#).expect("write");
        let read = load_backlog(&dir).expect("valid");
        assert_eq!(read[0].after, [28, 29]);
        assert!(read[1].after.is_empty());
        std::fs::write(
            dir.join("031.json"),
            r#"{"id": 31, "prompt": "p", "after": "028"}"#,
        )
        .expect("write");
        assert!(load_backlog(&dir).is_err(), "not a list of ids");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An owner-judged objective is never assigned: not by `choose` (even
    /// with a free slot and nothing in flight), not as `next_open`; it still
    /// counts as open, so an objective after it waits, and a mission whose
    /// only unsolved objective is the owner's gets no task at all.
    #[test]
    fn an_owner_judged_objective_is_never_assigned() {
        let mut owner = objective(1, false);
        owner.owner_judged = true;
        let backlog = [owner, objective(2, false)];
        assert_eq!(next_open(&backlog, &[]).map(|task| task.id), Some(2));
        assert_eq!(
            ids(&choose(
                &backlog,
                &[],
                &BTreeSet::new(),
                &untried(),
                &BTreeSet::new(),
                5
            )),
            [2],
            "the fleet's slots skip the owner's objective"
        );
        // The mission's only unsolved objective being the owner's assigns
        // nothing, as does one whose dependents all wait on it.
        let owner_only = [backlog[0].clone()];
        assert!(next_open(&owner_only, &[]).is_none());
        assert!(
            choose(
                &owner_only,
                &[],
                &BTreeSet::new(),
                &untried(),
                &BTreeSet::new(),
                5
            )
            .is_empty()
        );
        let mut dependent = objective(3, false);
        dependent.after = vec![1];
        assert!(
            choose(
                &[backlog[0].clone(), dependent.clone()],
                &[],
                &BTreeSet::new(),
                &untried(),
                &BTreeSet::new(),
                5
            )
            .is_empty(),
            "an objective after the owner's waits until it is solved"
        );
        // Solved (landed), the owner's objective no longer holds its
        // dependent.
        let landed = [landing(1, None)];
        assert_eq!(
            ids(&choose(
                &[backlog[0].clone(), dependent],
                &landed,
                &BTreeSet::new(),
                &untried(),
                &BTreeSet::new(),
                5
            )),
            [3]
        );
    }

    /// `owner_judged` is read from the file; anything but a boolean is
    /// refused rather than read as true or false.
    #[test]
    fn the_backlog_reads_owner_judged_and_refuses_another_value() {
        let dir = std::env::temp_dir().join(format!("spawn-backlog-owner-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("029.json"),
            r#"{"id": 29, "prompt": "p", "owner_judged": true}"#,
        )
        .expect("write");
        std::fs::write(dir.join("030.json"), r#"{"id": 30, "prompt": "p"}"#).expect("write");
        let read = load_backlog(&dir).expect("valid");
        assert!(read[0].owner_judged);
        assert!(!read[1].owner_judged, "absent is the fleet's");
        std::fs::write(
            dir.join("030.json"),
            r#"{"id": 30, "prompt": "p", "owner_judged": "yes"}"#,
        )
        .expect("write");
        assert!(load_backlog(&dir).is_err(), "not a boolean");
        std::fs::write(
            dir.join("030.json"),
            r#"{"id": 30, "prompt": "p", "owner_judged": null}"#,
        )
        .expect("write");
        assert!(load_backlog(&dir).is_err(), "null is not a boolean either");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The backlog names the one order its objectives need: 032, the
    /// soak that exercises 028's wait paths, after 028. The owner's order
    /// of 028-033 is a preference the prompts say may be broken (029 "with
    /// or after 028", 033 "with or after 020", 030 "can be solved before
    /// 028, 029, 031 and 033"): no `after` holds them back.
    #[test]
    fn the_backlog_names_its_order() {
        let backlog = load_backlog(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../rsi/backlog"),
        )
        .expect("the backlog");
        let after = |id: u32| {
            backlog
                .iter()
                .find(|task| task.id == id)
                .map(|task| task.after.clone())
                .expect("in the backlog")
        };
        assert_eq!(after(32), [28]);
        for free in [29, 30, 31, 33] {
            assert!(after(free).is_empty(), "{free}");
        }
    }

    /// An order no task can meet is found: an `after` id the backlog does
    /// not hold, and every cycle once (two and three objectives), by its
    /// lowest id; a sound order, and the repository's backlog, have none.
    #[test]
    fn an_order_that_cannot_be_met_is_found() {
        let task = |id: u32, after: &[u32]| {
            let mut task = objective(id, false);
            task.after = after.to_vec();
            task
        };
        assert!(order_problems(&[task(28, &[]), task(32, &[28])]).is_empty());
        let problems = order_problems(&[
            task(28, &[30]),
            task(30, &[28]),
            task(31, &[33]),
            task(32, &[31]),
            task(33, &[32, 99]),
        ]);
        assert_eq!(
            problems,
            [
                "objective 33 comes after 99, which the backlog does not hold: it waits until \
                 99 lands",
                "objectives 28, 30 come after each other: none of them is ever assigned",
                "objectives 31, 32, 33 come after each other: none of them is ever assigned",
            ]
        );
        assert_eq!(
            order_problems(&[task(30, &[30])]),
            ["objective 30 comes after itself: it is never assigned"]
        );
        // A dense order with no cycle (each of 200 after every one before
        // it, 19,900 edges) is checked at once: no path is enumerated.
        let dense: Vec<Task> = (1..=200)
            .map(|id| task(id, &(1..id).collect::<Vec<u32>>()))
            .collect();
        let started = std::time::Instant::now();
        assert!(order_problems(&dense).is_empty());
        assert!(
            started.elapsed() < std::time::Duration::from_millis(200),
            "{:?}",
            started.elapsed()
        );
        let mut cyclic = dense.clone();
        cyclic[0].after = vec![200];
        assert_eq!(
            order_problems(&cyclic).len(),
            1,
            "one component of all 200, said once"
        );
        let backlog = load_backlog(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../rsi/backlog"),
        )
        .expect("the backlog");
        assert!(order_problems(&backlog).is_empty());
    }
}
