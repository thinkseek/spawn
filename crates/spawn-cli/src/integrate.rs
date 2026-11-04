//! `spawn integrate`: the one landing path (docs/fleet-architecture.md
//! §10.3).
//!
//! For every open mission with a gate, one pass:
//!
//! 1. a halt rule that fired stops the mission's landings until a human
//!    resumes it: three gate rejections in a row, three aborts in a row, the
//!    scenario count below its floor, or the source's main not where the
//!    last landing left it;
//! 2. a delivered task over its mission's budget is aborted and its worker
//!    stopped;
//! 3. a task whose gate passed is landed (again, after a crash);
//! 4. otherwise the next ready task is judged: accepted by the supervisor
//!    (`accept` = review) or just reported (`gate`). Its workspace is
//!    committed, merged with the source's main when main moved, and judged
//!    by `scripts/rsi/gate.sh` with the mission's scenarios. A pass records
//!    the verdict before git moves, then moves the source's main with a
//!    compare-and-swap push and records the landing. A rejection sends the
//!    task back to its worker with the gate's output as the note;
//! 5. an objective mission with no open task gets its next objective
//!    ([`objectives`]).
//!
//! Like the dispatcher, it keeps no state of its own: the control stream
//! says what was judged and landed. Passes are serialized by the same
//! advisory lock the RSI controller takes, so two gates never run at once.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use spawn_core::command::{CommandAction, Submission};
use spawn_core::id::{AgentId, IdempotencyKey, Principal, SessionId, WorkspaceId};
use spawn_core::mission::TaskState;
use spawn_core::port::{CommandQueue, WorkspaceStore};
use spawn_host::GitWorkspace;
use spawn_postgres::PostgresStore;

use crate::config::{loop_interval, workspace_root};
use crate::gate::{self, GateFailure};
use crate::{Flags, env_or, open_store};
use crate::{objectives, sources};

/// Rejections, or aborts, in a row that halt a mission.
const HALT_AFTER: u32 = 3;

/// Wall clock of one gate run: two builds, clippy, the tests, the e2e and
/// the black-box scenarios fit well inside it.
const GATE_WALL_CLOCK: Duration = Duration::from_mins(45);

/// Guard against runaway candidates.
const MAX_PATCH_BYTES: usize = 1 << 20;

/// Characters of gate output quoted back to the worker.
const NOTE_LOG_CHARS: usize = 4000;

/// Who the integrator's commands come from.
const PRINCIPAL: &str = "spawn-integrate";

const USAGE: &str = "usage: spawn integrate [--once] [--root <dir>]";

/// One open mission with a gate, from `gated_missions`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Mission {
    pub(crate) id: i64,
    pub(crate) gate: String,
    pub(crate) accept: String,
    #[serde(default)]
    pub(crate) budget: Option<Value>,
    #[serde(default)]
    pub(crate) baseline: Option<String>,
    #[serde(default)]
    pub(crate) halted: Option<String>,
    #[serde(default)]
    pub(crate) scenario_floor: u32,
    #[serde(default)]
    pub(crate) gate_rejections: u32,
    #[serde(default)]
    pub(crate) gate_aborts: u32,
    #[serde(default)]
    pub(crate) objectives: Option<Value>,
    #[serde(default)]
    pub(crate) source: Option<String>,
    /// Its open tasks (assigned, delivered, reported, revise, accepted,
    /// landing).
    #[serde(default)]
    pub(crate) tasks: Vec<Task>,
}

/// One of its tasks the integrator may act on.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Task {
    pub(crate) id: i64,
    pub(crate) agent_id: String,
    pub(crate) session_id: String,
    pub(crate) state: TaskState,
    #[serde(default)]
    pub(crate) base: Option<String>,
    #[serde(default)]
    pub(crate) candidate: Option<String>,
    #[serde(default)]
    pub(crate) scenario: Option<String>,
    #[serde(default)]
    pub(crate) delivered_at_ms: Option<i64>,
}

impl Mission {
    /// The task the gate judges next.
    fn ready(&self) -> Option<&Task> {
        let wanted = if self.accept == "gate" {
            TaskState::Reported
        } else {
            TaskState::Accepted
        };
        self.tasks.iter().find(|task| task.state == wanted)
    }

    /// A task whose gate passed and whose landing is not recorded yet.
    fn landing(&self) -> Option<&Task> {
        self.tasks
            .iter()
            .find(|task| task.state == TaskState::Landing)
    }
}

/// The halt rule that fires before anything is judged, if one does.
pub(crate) fn halt_reason(mission: &Mission, scenarios: u32) -> Option<String> {
    if mission.gate_rejections >= HALT_AFTER {
        return Some(format!(
            "{} gate rejections in a row",
            mission.gate_rejections
        ));
    }
    if mission.gate_aborts >= HALT_AFTER {
        return Some(format!("{} gate aborts in a row", mission.gate_aborts));
    }
    // A candidate that deletes the scenario that judges it passes by removing
    // the work, so the count may not fall.
    if mission.scenario_floor > 0 && scenarios < mission.scenario_floor {
        return Some(format!(
            "the scenarios fell from {} to {scenarios}",
            mission.scenario_floor
        ));
    }
    None
}

/// Whether a delivered task ran past its mission's budget
/// (`{"minutes": n, "tool_calls": n}`, either optional).
pub(crate) fn over_budget(budget: &Value, elapsed_ms: i64, tool_calls: i64) -> Option<String> {
    if let Some(minutes) = budget.get("minutes").and_then(Value::as_i64)
        && elapsed_ms > minutes.saturating_mul(60_000)
    {
        return Some(format!("the task ran past its {minutes}-minute budget"));
    }
    if let Some(limit) = budget.get("tool_calls").and_then(Value::as_i64)
        && tool_calls > limit
    {
        return Some(format!("the task made more than {limit} tool calls"));
    }
    None
}

fn tail(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut start = text.len() - limit;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

fn git(arguments: &[&str]) -> Result<String, String> {
    sources::git(arguments).map_err(|error| error.to_string())
}

/// The source's default branch and its commit, as `git ls-remote` sees it.
fn source_head(source: &str) -> Result<(String, String), String> {
    let listing = git(&["ls-remote", "--symref", "--", source, "HEAD"])?;
    let mut branch = "refs/heads/main".to_string();
    let mut commit = String::new();
    for line in listing.lines() {
        if let Some(rest) = line.strip_prefix("ref: ") {
            if let Some(name) = rest.split_whitespace().next() {
                branch = name.to_string();
            }
        } else if let Some(id) = line.split_whitespace().next() {
            commit = id.to_string();
        }
    }
    if commit.is_empty() {
        return Err(format!("{source} has no HEAD"));
    }
    Ok((branch, commit))
}

/// `spawn integrate [--once]`: pass after pass, or one.
pub(crate) fn integrate(rest: &[String]) -> Result<i32, String> {
    let once = rest.iter().any(|word| word == "--once");
    let valued: Vec<String> = rest
        .iter()
        .filter(|word| *word != "--once")
        .cloned()
        .collect();
    let flags = Flags::parse(&valued).map_err(|error| format!("{error}\n{USAGE}"))?;
    let root = workspace_root(&flags)
        .map(PathBuf::from)
        .ok_or_else(|| format!("--root or SPAWN_WORKSPACE_ROOT is required\n{USAGE}"))?;
    let gate = absolute(&env_or("SPAWN_GATE", "scripts/rsi/gate.sh"))?;
    let integrator = Integrator { root, gate };
    loop {
        if let Err(error) = integrator.pass() {
            eprintln!("integrate: {error}");
            if once {
                return Err(error);
            }
        }
        if once {
            return Ok(0);
        }
        std::thread::sleep(loop_interval().max(Duration::from_secs(5)));
    }
}

/// Resolve a path against the working directory: the gate must not resolve
/// inside the tree it judges.
fn absolute(path: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        return Ok(path);
    }
    std::env::current_dir()
        .map(|base| base.join(path))
        .map_err(|error| format!("current directory: {error}"))
}

struct Integrator {
    root: PathBuf,
    gate: PathBuf,
}

impl Integrator {
    fn pass(&self) -> Result<(), String> {
        // A store per pass: the advisory lock belongs to the connection, and
        // is released with it.
        let store = open_store()?;
        if !store.rsi_try_lock().map_err(|error| error.to_string())? {
            return Ok(());
        }
        let missions: Vec<Mission> =
            serde_json::from_value(store.gated_missions().map_err(|error| error.to_string())?)
                .map_err(|error| format!("gated missions: {error}"))?;
        for mission in &missions {
            match self.mission(&store, mission) {
                // Whatever one mission did may move another's view (a shared
                // source's main): the next pass reads everything again.
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(error) => eprintln!("integrate: mission {}: {error}", mission.id),
            }
        }
        Ok(())
    }

    /// One mission's step. True when it recorded anything.
    fn mission(&self, store: &PostgresStore, mission: &Mission) -> Result<bool, String> {
        if mission.halted.is_some() {
            return Ok(false);
        }
        let scenarios = PathBuf::from(&mission.gate);
        let count = gate::count_scenarios(&scenarios)?;
        if let Some(reason) = halt_reason(mission, count) {
            return halt(store, mission, &reason).map(|()| true);
        }
        if Self::enforce_budget(store, mission)? {
            return Ok(true);
        }
        let Some(source) = mission.source.as_deref() else {
            return Ok(false);
        };
        let mirror =
            sources::ensure_mirror(&self.root, Some(source)).map_err(|error| error.to_string())?;
        if let Some(task) = mission.landing() {
            return land(store, &mirror, source, task).map(|()| true);
        }
        sources::refresh(&mirror, source);
        let main = git(&[
            "--git-dir",
            &mirror.to_string_lossy(),
            "rev-parse",
            "refs/heads/main",
        ])?;
        if let Some(baseline) = &mission.baseline
            && *baseline != main
        {
            return halt(
                store,
                mission,
                &format!("the source's main is {main}, but the last landing left {baseline}"),
            )
            .map(|()| true);
        }
        let Some(task) = mission.ready() else {
            // An objective mission with nothing open gets its next task.
            return Ok(objectives::assign_next(store, mission)?.is_some());
        };
        self.judge(store, &mirror, source, &main, &scenarios, count, task)
            .map(|()| true)
    }

    /// Abort delivered tasks over budget. True when it aborted one.
    fn enforce_budget(store: &PostgresStore, mission: &Mission) -> Result<bool, String> {
        let Some(budget) = &mission.budget else {
            return Ok(false);
        };
        let mut aborted = false;
        let now = spawn_core::time::Clock::now(&spawn_core::time::SystemClock).0;
        for task in &mission.tasks {
            let (TaskState::Delivered, Some(at)) = (task.state, task.delivered_at_ms) else {
                continue;
            };
            let calls = store
                .tool_calls_since(&task.session_id, at)
                .map_err(|error| error.to_string())?;
            if let Some(reason) = over_budget(budget, now - at, calls) {
                record(
                    store,
                    &json!({"type": "task.aborted", "id": task.id, "reason": reason}),
                )?;
                stop(store, task);
                println!("integrate: task {} aborted: {reason}", task.id);
                aborted = true;
            }
        }
        Ok(aborted)
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn judge(
        &self,
        store: &PostgresStore,
        mirror: &Path,
        source: &str,
        main: &str,
        scenarios: &Path,
        count: u32,
        task: &Task,
    ) -> Result<(), String> {
        let repo = GitWorkspace::new(mirror, &self.root);
        let directory = self.root.join(&task.agent_id);
        if directory.exists() {
            let workspace = WorkspaceId::new(format!("{}:{}", task.agent_id, directory.display()))
                .map_err(|error| error.to_string())?;
            repo.commit(&workspace, &format!("spawn integrate: task {}", task.id))
                .map_err(|error| error.to_string())?;
        }
        let gated = |verdict: &str, extra: Value| -> Result<(), String> {
            let mut record_value = json!({"type": "task.gated", "id": task.id, "verdict": verdict,
                                          "base": main, "scenarios": count});
            if let (Some(target), Some(extra)) = (record_value.as_object_mut(), extra.as_object()) {
                target.extend(extra.clone());
            }
            record(store, &record_value)?;
            println!("integrate: task {} gated: {verdict}", task.id);
            Ok(())
        };
        let rejected = |reason: &str, log: &str| {
            gated(
                "rejected",
                json!({"log": log, "reason": reason, "note": format!(
                    "The gate rejected this work: {reason}.\n\nGate output (tail):\n{}",
                    tail(log, NOTE_LOG_CHARS))}),
            )
        };

        let mirror_path = mirror.to_string_lossy().to_string();
        let Ok(branch) = repo.commit_id_of(&format!("refs/heads/{}", task.agent_id)) else {
            return rejected("the worker has no branch to land", "");
        };
        let candidate = if git(&[
            "--git-dir",
            &mirror_path,
            "merge-base",
            "--is-ancestor",
            main,
            &branch,
        ])
        .is_ok()
        {
            branch
        } else {
            // Main moved since the worker started: land the merge of both.
            let Ok(tree) = git(&[
                "--git-dir",
                &mirror_path,
                "merge-tree",
                "--write-tree",
                main,
                &branch,
            ]) else {
                return gated(
                    "conflict",
                    json!({"note": format!(
                        "Your work conflicts with the source's current main ({main}). \
                         Bring the conflicting files in line with it and report again.")}),
                );
            };
            let tree = tree.lines().next().unwrap_or_default().to_string();
            git(&[
                "--git-dir",
                &mirror_path,
                "commit-tree",
                &tree,
                "-p",
                main,
                "-p",
                &branch,
                "-m",
                &format!("Land task {} (merged with main)", task.id),
            ])?
        };
        let tree_of = |commit: &str| {
            git(&[
                "--git-dir",
                &mirror_path,
                "rev-parse",
                &format!("{commit}^{{tree}}"),
            ])
        };
        if tree_of(&candidate)? == tree_of(main)? {
            return rejected("the work changes nothing on main", "");
        }
        let patch = repo
            .patch_between(main, &candidate)
            .map_err(|error| error.to_string())?;
        if patch.len() > MAX_PATCH_BYTES {
            return rejected("the patch is larger than 1 MiB", "");
        }

        let scenario = task.scenario.as_deref().map(PathBuf::from);
        match gate::judge(
            &self.gate,
            mirror,
            main,
            &candidate,
            scenarios,
            scenario.as_deref(),
            GATE_WALL_CLOCK,
        ) {
            Ok(verdict) if verdict.accepted => {
                // The verdict is stored before git moves, so a crash lands
                // on the next pass instead of judging again.
                gated(
                    "accepted",
                    json!({"candidate": candidate, "log": verdict.output, "metric": verdict.metric}),
                )?;
                let landing = Task {
                    state: TaskState::Landing,
                    base: Some(main.to_string()),
                    candidate: Some(candidate),
                    ..task.clone()
                };
                land(store, mirror, source, &landing)
            }
            Ok(verdict) => rejected(&verdict.reason, &verdict.output),
            // A gate that never returns, or cannot run, says nothing about
            // the work: an abort, not a rejection.
            Err(GateFailure::Timeout) => gated(
                "aborted",
                json!({"reason": "the gate exceeded its wall clock"}),
            ),
            Err(GateFailure::Failed(error)) => gated(
                "aborted",
                json!({"reason": format!("the gate failed: {error}")}),
            ),
        }
    }
}

/// Move the source's default branch from the task's base to its candidate,
/// only if it is still at the base, and record the landing. A branch that
/// moved meanwhile sends the task back to be judged again.
fn land(store: &PostgresStore, mirror: &Path, source: &str, task: &Task) -> Result<(), String> {
    let (Some(base), Some(candidate)) = (task.base.as_deref(), task.candidate.as_deref()) else {
        return record(
            store,
            &json!({"type": "task.gated", "id": task.id, "verdict": "stale"}),
        );
    };
    let (branch, _) = source_head(source)?;
    let mirror_path = mirror.to_string_lossy().to_string();
    let pushed = git(&[
        "--git-dir",
        &mirror_path,
        "push",
        "--quiet",
        &format!("--force-with-lease={branch}:{base}"),
        "--",
        source,
        &format!("{candidate}:{branch}"),
    ]);
    // A push that failed may still have happened (a crash, a lost reply).
    let (_, now) = source_head(source)?;
    if pushed.is_err() && now != candidate {
        println!(
            "integrate: task {}: {branch} moved to {now}; judging again",
            task.id
        );
        return record(
            store,
            &json!({"type": "task.gated", "id": task.id, "verdict": "stale"}),
        );
    }
    let _ = git(&[
        "--git-dir",
        &mirror_path,
        "update-ref",
        "refs/heads/main",
        candidate,
    ]);
    record(
        store,
        &json!({"type": "task.landed", "id": task.id, "from": base, "to": candidate}),
    )?;
    println!("integrate: task {} landed {candidate} on {source}", task.id);
    Ok(())
}

fn record(store: &PostgresStore, value: &Value) -> Result<(), String> {
    store
        .integrator_record(value)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn halt(store: &PostgresStore, mission: &Mission, reason: &str) -> Result<(), String> {
    record(
        store,
        &json!({"type": "mission.halted", "id": mission.id, "reason": reason}),
    )?;
    println!("integrate: mission {} HALTED: {reason}", mission.id);
    Ok(())
}

/// Stop an aborted task's worker; best effort.
fn stop(store: &PostgresStore, task: &Task) {
    let submission = (|| {
        Some(Submission {
            agent_id: AgentId::new(task.agent_id.clone()).ok()?,
            session_id: SessionId::new(task.session_id.clone()).ok()?,
            idempotency_key: IdempotencyKey::new(format!("integrate-abort-{}", task.id)).ok()?,
            action: CommandAction::Stop,
        })
    })();
    if let (Some(submission), Ok(principal)) = (submission, Principal::new(PRINCIPAL)) {
        let _ = store.submit(&principal, &submission);
    }
}

#[cfg(test)]
mod tests {
    use super::{Mission, halt_reason, over_budget};
    use serde_json::json;

    fn mission(rejections: u32, aborts: u32, floor: u32) -> Mission {
        serde_json::from_value(json!({
            "id": 1, "gate": "/g", "accept": "review",
            "gate_rejections": rejections, "gate_aborts": aborts, "scenario_floor": floor,
        }))
        .expect("a mission")
    }

    #[test]
    fn three_in_a_row_halt_and_the_scenario_floor_holds() {
        assert_eq!(halt_reason(&mission(2, 2, 7), 7), None);
        assert!(halt_reason(&mission(3, 0, 0), 7).is_some_and(|r| r.contains("rejections")));
        assert!(halt_reason(&mission(0, 3, 0), 7).is_some_and(|r| r.contains("aborts")));
        assert!(halt_reason(&mission(0, 0, 7), 6).is_some_and(|r| r.contains("fell")));
        assert_eq!(halt_reason(&mission(0, 0, 0), 0), None, "no floor yet");
    }

    #[test]
    fn a_budget_bounds_minutes_and_tool_calls() {
        let budget = json!({"minutes": 45, "tool_calls": 60});
        assert_eq!(over_budget(&budget, 44 * 60_000, 60), None);
        assert!(over_budget(&budget, 46 * 60_000, 0).is_some());
        assert!(over_budget(&budget, 0, 61).is_some());
        assert_eq!(over_budget(&json!({}), i64::MAX, i64::MAX), None);
    }

    #[test]
    fn the_ready_task_follows_the_acceptance_rule() {
        let mut gated = mission(0, 0, 0);
        gated.tasks = serde_json::from_value(json!([
            {"id": 1, "agent_id": "agent_a", "session_id": "session_a", "state": "reported"},
            {"id": 2, "agent_id": "agent_b", "session_id": "session_b", "state": "accepted"},
        ]))
        .expect("tasks");
        assert_eq!(gated.ready().map(|task| task.id), Some(2));
        gated.accept = "gate".to_string();
        assert_eq!(gated.ready().map(|task| task.id), Some(1));
    }
}
