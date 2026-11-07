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
//! says the work did not make it: a gate's `rejected`, `incompatible` or
//! `conflict` (unless main could not be merged into the workspace, or merged
//! cleanly: `merge` names how), or an abort over budget (`task.aborted`).
//! The host's outcomes (`no_room`, `host_fault`, a gate `aborted`) are not
//! the work's and count for nothing, as for the halt rules. Once an
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
//! An objective assigned again after earlier tasks of it ended without
//! landing (cancelled, aborted, yielded) gets their work in its brief: each
//! one's id, branch and last checkpoint, its last verdict or reason, and its
//! report, with the instruction to build on it, not redo it
//! ([`PriorWork`], read from the records at assignment). Task 161 rebuilt
//! 2264 lines that task 155 had mostly written.
//!
//! The mission's resident takes a task
//! whenever it has none open in the mission; every other task goes to a new
//! per-task worker (`agent_worker_objective_<task>`, ephemeral: no approval,
//! a fresh workspace from the source's main, stopped once its task lands).
//! `spawn_objective_assign` checks both limits again under the append lock.
//! With N = 1 this is one task at a time on the resident.
//!
//! A one-shot objective is solved once a task for it lands; a recurring one
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
    pub(crate) branch: Option<String>,
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

/// Whether an objective is open: a one-shot one with no landing, or a
/// recurring one whose latest landed metric misses its target.
fn is_open(task: &Task, landings: &[Landing]) -> bool {
    if task.recurring {
        !landed_value(task, landings).is_some_and(|value| task.meets(value))
    } else {
        !landings.iter().any(|landing| landing.objective == task.id)
    }
}

/// The lowest open objective: what one free slot gets with nothing in flight.
#[cfg(test)]
pub(crate) fn next_open<'a>(backlog: &'a [Task], landings: &[Landing]) -> Option<&'a Task> {
    backlog.iter().find(|task| is_open(task, landings))
}

/// The objectives that fill `free` slots: the open ones without a task in
/// flight (`in_flight`), fewest attempts first (yields, then failed attempts
/// since the last yield, then the lowest id), at most `free` of them.
pub(crate) fn choose<'a>(
    backlog: &'a [Task],
    landings: &[Landing],
    in_flight: &BTreeSet<u32>,
    attempts: &BTreeMap<u32, Attempts>,
    free: usize,
) -> Vec<&'a Task> {
    let mut open: Vec<&Task> = backlog
        .iter()
        .filter(|task| is_open(task, landings) && !in_flight.contains(&task.id))
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
    #[serde(default)]
    pub(crate) verdict: Option<String>,
    /// For a conflict, how main went into the workspace (`main_merge`'s
    /// `Merge::kind`); absent on records from before it was named.
    #[serde(default)]
    pub(crate) merge: Option<String>,
}

impl Outcome {
    /// The failed attempt this outcome is, if it is one: what it says about
    /// the work. A conflict is one when the worker got conflicts to resolve
    /// (`marked`, `unmarked`, or markers still `pending`; a record from
    /// before `merge` was named counts too), not when main could not be
    /// merged (`not_mounted`, `busy`, `failed`) or merged cleanly.
    fn failure(&self) -> Option<&str> {
        match (self.kind.as_str(), self.verdict.as_deref()) {
            ("task.aborted", _) => Some("aborted over budget"),
            ("task.gated", Some(verdict @ ("rejected" | "incompatible"))) => Some(verdict),
            ("task.gated", Some("conflict")) => match self.merge.as_deref() {
                None | Some("marked" | "unmarked" | "pending") => Some("conflict"),
                Some(_) => None,
            },
            _ => None,
        }
    }

    fn is_yield(&self) -> bool {
        self.kind == "task.cancelled"
    }
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
}

impl Yield {
    /// The one line the record and the supervisor get.
    pub(crate) fn reason(&self) -> String {
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
            let tried = attempts.get(&objective)?;
            (tried.failures.len() >= limit).then(|| Yield {
                task: task.id,
                objective,
                failures: tried.failures.clone(),
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

/// The brief of an objective task: the objective, the working rules, the
/// work earlier tasks of it left without landing, and what the gate said
/// about earlier attempts.
pub(crate) fn brief(
    task: &Task,
    current: Option<f64>,
    prior: &[PriorWork],
    history: &[GateRecord],
) -> String {
    let mut brief = format!("Objective {}: {}\n\n{}\n", task.id, task.title, task.prompt);
    brief.push_str(
        "\nWork only inside this workspace. Do not run git commands and do not create \
         commits: the host commits your workspace. Do not edit `rsi/`, `scripts/rsi/`, \
         `scripts/blackbox/` or `scripts/e2e.sh`: the gate refuses any change there. \
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
    if !prior.is_empty() {
        brief.push_str(
            "\nEarlier tasks on this objective ended without landing, newest first. Their \
             work is kept on their branches. Start from that work, do not redo it: read what \
             each did and found, keep what holds, and finish or fix the rest.\n",
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
            match (&work.branch, &work.commit) {
                (Some(branch), Some(commit)) => {
                    let _ = write!(brief, "; branch `{branch}` at commit {commit}");
                }
                _ => brief.push_str("; it recorded no checkpoint"),
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
                    "limit": limit, "reason": reason}),
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
    let mut assigned = Vec::new();
    for task in choose(&objectives, &landings, &in_flight, &attempts, free) {
        match assign(store, mission, task, &landings) {
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
    let text = brief(task, landed_value(task, landings), &prior, &history);
    let assigned = store
        .objective_assign(mission.id, id, scenario.as_deref(), &text)
        .map_err(|error| error.to_string())?;
    println!(
        "integrate: mission {} objective {} -> task {assigned}",
        mission.id, task.id
    );
    Ok(assigned)
}

#[cfg(test)]
mod tests {
    use super::{
        Attempts, Landing, Outcome, PriorWork, Task, attempt_limit, attempts, brief, choose,
        load_backlog, metric_lines, next_open, workers, yields,
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
            ids(&choose(&backlog, &[], &none, &untried(), 3)),
            vec![1, 2, 3],
            "lowest first"
        );
        // Solved objectives and those in flight are skipped.
        let solved = [landing(1, None)];
        let flying = BTreeSet::from([2]);
        assert_eq!(
            ids(&choose(&backlog, &solved, &flying, &untried(), 3)),
            vec![3, 4]
        );
        // No free slot, nothing.
        assert!(choose(&backlog, &[], &none, &untried(), 0).is_empty());
        // Fewer open objectives than slots: only those.
        let three_solved = [landing(1, None), landing(2, None), landing(3, None)];
        assert_eq!(
            ids(&choose(&backlog, &three_solved, &none, &untried(), 3)),
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
            ids(&choose(&backlog, &missed, &none, &untried(), 2)),
            vec![1, 2]
        );
        let flying = BTreeSet::from([1]);
        assert_eq!(
            ids(&choose(&backlog, &missed, &flying, &untried(), 2)),
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

    fn outcome(kind: &str, objective: u32, verdict: Option<&str>, merge: Option<&str>) -> Outcome {
        Outcome {
            kind: kind.to_string(),
            objective,
            verdict: verdict.map(str::to_string),
            merge: merge.map(str::to_string),
        }
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

    /// What the work did counts; what the host did, or a clean or failed
    /// merge of main, does not; a yield or an acceptance starts the count
    /// again.
    #[test]
    fn failed_attempts_are_the_works_outcomes() {
        let outcomes = [
            gated(1, "rejected"),
            gated(1, "incompatible"),
            outcome("task.gated", 1, Some("conflict"), Some("marked")),
            outcome("task.gated", 1, Some("conflict"), Some("unmarked")),
            outcome("task.gated", 1, Some("conflict"), Some("pending")),
            outcome("task.gated", 1, Some("conflict"), None),
            outcome("task.aborted", 1, None, None),
            // The host's, or main not merged: nothing.
            gated(1, "no_room"),
            gated(1, "host_fault"),
            gated(1, "aborted"),
            outcome("task.gated", 1, Some("conflict"), Some("not_mounted")),
            outcome("task.gated", 1, Some("conflict"), Some("busy")),
            outcome("task.gated", 1, Some("conflict"), Some("failed")),
            outcome("task.gated", 1, Some("conflict"), Some("merged")),
        ];
        let counted = attempts(&outcomes);
        assert_eq!(counted[&1].yields, 0);
        assert_eq!(
            counted[&1].failures,
            [
                "rejected",
                "incompatible",
                "conflict",
                "conflict",
                "conflict",
                "conflict",
                "aborted over budget"
            ]
        );
        let after = attempts(&[
            gated(2, "rejected"),
            outcome("task.cancelled", 2, None, None),
            gated(2, "rejected"),
            gated(3, "rejected"),
            gated(3, "rejected"),
            gated(3, "accepted"),
            gated(3, "conflict"),
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
            ["conflict"],
            "an acceptance ends the count"
        );
    }

    /// Fewest yields first, then fewest failed attempts, then the lowest id.
    #[test]
    fn free_slots_go_to_the_fewest_attempts() {
        let backlog: Vec<Task> = [19, 21, 23, 25, 26].map(|id| objective(id, false)).to_vec();
        let tried = attempts(&[
            gated(19, "conflict"),
            gated(19, "conflict"),
            outcome("task.cancelled", 19, None, None),
            gated(21, "rejected"),
            gated(23, "rejected"),
            gated(23, "rejected"),
        ]);
        let none = BTreeSet::new();
        assert_eq!(
            ids(&choose(&backlog, &[], &none, &tried, 5)),
            vec![25, 26, 21, 23, 19]
        );
        assert_eq!(ids(&choose(&backlog, &[], &none, &tried, 1)), vec![25]);
    }

    /// Three slots held by three objectives at the limit: the tasks being
    /// worked again yield, and the others get the slots, 025 and 026 first.
    /// A healthy objective (under the limit) and a reported task keep
    /// theirs; a task under a gate or aborted this pass does not yield.
    #[test]
    fn objectives_at_the_limit_yield_their_slots() {
        let three = |objective| {
            [
                gated(objective, "conflict"),
                gated(objective, "rejected"),
                gated(objective, "conflict"),
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
            ids(&choose(&backlog, &[], &BTreeSet::new(), &after, 3)),
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
    }

    /// Mission 4 as the live log had it at 2026-11-07 23:05:46, when task
    /// 113 (objective 023) was aborted over budget: every gate verdict,
    /// abort and yield of its objective tasks until then (read-only, the
    /// store's objective outcomes). Then, 127 (019) and 106 (021) were
    /// reported, and the old rule gave the freed slot to 023 again (task
    /// 160, five seconds later). Now neither reported task yields (106 went
    /// on to land), and the slot goes to 025: 023 has five failed attempts
    /// and 025 none.
    #[test]
    fn mission_4_at_23_05_assigns_objective_25() {
        let outcomes: Vec<Outcome> = serde_json::from_str(include_str!(
            "../tests/data/mission-4-outcomes-2026-11-07T23-05-46.json"
        ))
        .expect("the outcomes");
        let tried = attempts(&outcomes);
        assert_eq!(tried[&19].failures.len(), 7);
        assert_eq!(tried[&21].failures.len(), 4);
        assert_eq!(tried[&23].failures.len(), 5);
        let after_abort = mission(3, &[(127, 19, "reported"), (106, 21, "reported")]);
        let none = BTreeSet::new();
        assert!(yields(&after_abort, &tried, attempt_limit(None), &none).is_empty());
        // The deployed backlog then: open are 019, 021, 023, 025-034.
        let backlog: Vec<Task> = [19, 21, 23, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34]
            .map(|id| objective(id, false))
            .to_vec();
        let flying = BTreeSet::from([19, 21]);
        assert_eq!(ids(&choose(&backlog, &[], &flying, &tried, 1)), vec![25]);
        // Had 127 been sent back to revise, it would yield as well, and 026
        // take its slot.
        let sent_back = mission(3, &[(127, 19, "revise"), (106, 21, "reported")]);
        let slots = yields(&sent_back, &tried, attempt_limit(None), &none);
        assert_eq!(
            slots.iter().map(|slot| slot.task).collect::<Vec<_>>(),
            [127]
        );
        assert_eq!(
            ids(&choose(&backlog, &[], &BTreeSet::from([21]), &tried, 2)),
            vec![25, 26]
        );
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
    /// outcome and reason, branch and commit, report, and the instruction
    /// to start from it.
    #[test]
    fn a_brief_carries_the_earlier_tasks_work() {
        let prior = [
            PriorWork {
                task: 155,
                branch: Some("agent_worker_objective_155".to_string()),
                commit: Some("c245db93e981880756015f1efac78939fbbfe11c".to_string()),
                outcome: Some("aborted".to_string()),
                reason: Some("the task ran past its 90-minute budget".to_string()),
                report: Some("## Outcome\nmostly done\n\n## Left\nthe tests".to_string()),
            },
            PriorWork {
                task: 150,
                branch: None,
                commit: None,
                outcome: Some("yielded".to_string()),
                reason: None,
                report: Some("x".repeat(5000)),
            },
        ];
        let text = brief(&objective(59, false), None, &prior, &[]);
        assert!(text.contains("Start from that work, do not redo it"));
        assert!(text.contains(
            "- task 155: aborted (the task ran past its 90-minute budget); branch \
             `agent_worker_objective_155` at commit c245db93e981880756015f1efac78939fbbfe11c"
        ));
        assert!(text.contains("  > mostly done\n"));
        assert!(text.contains("- task 150: yielded; it recorded no checkpoint"));
        assert!(text.contains("… (cut)"), "a long report is cut");
        assert!(text.len() < 5000);
        let first = brief(&objective(59, false), None, &[], &[]);
        assert!(!first.contains("Earlier tasks"), "a first task has none");
    }
}
