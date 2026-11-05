//! `spawn integrate`: the one landing path (docs/fleet-architecture.md
//! §10.3).
//!
//! For every open mission with a gate, and every done one while the closure
//! rule (`spawn_task_may_continue`) lets its work go on, one pass:
//!
//! 1. a halt rule that fired stops the mission's landings until a human
//!    resumes it: three gate rejections in a row, three aborts in a row, the
//!    scenario count below its floor, a remote source (agents' branches
//!    never reach it), or the source's main not where the last landing left
//!    it. That last one is read only from a fetch that succeeded this pass,
//!    and only while no other mission's landing on the source is unresolved
//!    (its push may have moved main before its record);
//! 2. a delivered task over its mission's budget is aborted and its worker
//!    stopped;
//! 3. a task whose gate passed is landed (again, after a crash);
//! 4. otherwise the next ready task is judged: accepted by the supervisor
//!    (`accept` = review), or also just reported (`gate`). The candidate is
//!    what the agent published: the source's `refs/heads/spawn/<agent>`
//!    (pushed by the Bridge's pusher, [`crate::push`]), fetched into this mirror's
//!    own `refs/spawn-integrate/<agent>`. The integrator never commits a
//!    mounted workspace and never moves the agent's `refs/heads/<agent>`,
//!    so it works the same on a host that never ran the agent, and a file
//!    written after the last checkpoint never lands. The candidate is
//!    merged with the source's main when main moved, and judged by
//!    `scripts/rsi/gate.sh` with the mission's scenarios. Every verdict
//!    names the revision of the report it judged (the task's `revisions`),
//!    and the store refuses it once the task has moved on. A pass records
//!    the verdict before git moves, then moves the source's main with a
//!    compare-and-swap push and records the landing. A rejection sends the
//!    task back to its worker with the gate's output as the note;
//!
//!    **Stale pushes do not land.** The pushed head must be the commit of
//!    the agent's latest `artifact.committed` event (its last checkpoint or
//!    merge of main) or a descendant of it. A push that lags (behind,
//!    diverged, missing that commit, or no `spawn/<agent>` at all while the
//!    agent has a checkpoint) is waited for briefly ([`PUSH_WAIT`], the
//!    pusher coalesces pushes), then gated `stale`: the gate does not run,
//!    the task goes back to `reported` (`accepted` under review), no
//!    rejection is counted, nothing reaches the worker, and a later pass
//!    judges again. An agent with neither a checkpoint nor a pushed branch
//!    has no branch to land;
//! 5. an objective mission's free slots are filled ([`objectives`]): it keeps
//!    up to its worker limit N (`--workers`, default 1) of its tasks in
//!    flight, each on its own agent (its resident, then per-task workers),
//!    and never two for one objective. Filling happens on every pass that
//!    gets this far, before the judging of step 4. A halted mission gets no
//!    new task.
//!
//! **Gates run per mission, a few at once.** A judging (step 4, from the
//! fetch of the pushed branch to the landing) runs on a thread of its own,
//! so one mission's 20-minute gate does not hold another's:
//! - a mission has at most one judging in flight, its lowest ready task:
//!   its tasks stay serial, each landing on the main the next is judged
//!   against. While it runs, the pass skips the mission altogether (halts,
//!   budgets, landings, assignments), and every other mission on its source
//!   too: they share the mirror and the main it judges against;
//! - at most [`DEFAULT_GATES`] judgings run at once across missions
//!   (`--gates N` or `SPAWN_INTEGRATE_GATES`, 1 to [`MAX_GATES`]): gates
//!   share the CPU and the test database. A ready task over the cap waits
//!   for a later pass.
//!
//! Every judging records its own outcome (`task.gated`, `task.landed`), as
//! before; the pass reads those records, never the thread's result. A
//! finished thread is reaped only at the start of a pass, before the
//! missions are read, so no pass acts on a snapshot older than a judging's
//! records. A restart mid-gate behaves as it did with one gate: the process
//! (and with it every gate under it) dies before any record, so the next
//! integrator judges the task again.
//!
//! Like the dispatcher, it keeps no state of its own beyond the gates it is
//! running: the control stream says what was judged and landed. An advisory
//! lock allows one integrator per database; it belongs to the connection,
//! which the integrator keeps while any of its gates runs.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use spawn_core::command::{CommandAction, Submission};
use spawn_core::id::{AgentId, IdempotencyKey, Principal, SessionId};
use spawn_core::mission::TaskState;
use spawn_core::port::CommandQueue;
use spawn_host::GitWorkspace;
use spawn_postgres::PostgresStore;

use crate::config::{loop_interval, workspace_root};
use crate::gate::{self, GateFailure};
use crate::{Flags, env_or, open_store};
use crate::{main_merge, objectives, sources};

/// Rejections, or aborts, in a row that halt a mission.
const HALT_AFTER: u32 = 3;

/// Wall clock of one gate run: two builds, clippy, the tests, the e2e and
/// the black-box scenarios fit well inside it.
const GATE_WALL_CLOCK: Duration = Duration::from_mins(45);

/// Guard against runaway candidates.
const MAX_PATCH_BYTES: usize = 1 << 20;

/// Characters of gate output quoted back to the worker.
const NOTE_LOG_CHARS: usize = 4000;

/// How long a pass waits for a pushed branch that lags the agent's latest
/// checkpoint before it gates the task `stale`: the pusher pushes a branch at
/// most once per [`crate::push::INTERVAL`].
const PUSH_WAIT: Duration = Duration::from_secs(12);

/// Pause between two looks at a lagging pushed branch.
const PUSH_POLL: Duration = Duration::from_millis(500);

/// Who the integrator's commands come from.
const PRINCIPAL: &str = "spawn-integrate";

const USAGE: &str = "usage: spawn integrate [--once] [--root <dir>] [--gates <n>]";

/// Gates that run at once, across missions, unless `--gates` or
/// `SPAWN_INTEGRATE_GATES` says otherwise.
pub(crate) const DEFAULT_GATES: usize = 2;

/// The most gates `--gates` allows: each holds two trees, their builds and
/// a share of the test database.
pub(crate) const MAX_GATES: usize = 8;

/// How many gates may run at once: `--gates`, else `SPAWN_INTEGRATE_GATES`,
/// else [`DEFAULT_GATES`]; a whole number from 1 to [`MAX_GATES`].
pub(crate) fn gate_slots(flag: Option<String>, env: Option<String>) -> Result<usize, String> {
    let Some((name, text)) = flag
        .map(|text| ("--gates", text))
        .or_else(|| env.map(|text| ("SPAWN_INTEGRATE_GATES", text)))
    else {
        return Ok(DEFAULT_GATES);
    };
    match text.trim().parse::<usize>() {
        Ok(slots) if (1..=MAX_GATES).contains(&slots) => Ok(slots),
        _ => Err(format!(
            "{name} is a whole number from 1 to {MAX_GATES}, not '{text}'"
        )),
    }
}

/// One mission with a gate the integrator visits, from `gated_missions`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Mission {
    pub(crate) id: i64,
    /// `open` or `active`; `done` while the closure rule lets its work go
    /// on (`spawn_task_may_continue`).
    #[serde(default)]
    pub(crate) state: String,
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
    /// Its tasks the closure rule lets go on: every open task of an open
    /// mission; of a done one, delivered, reported, accepted and landing.
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
    /// The backlog objective it pursues, for an objective mission's task.
    #[serde(default)]
    pub(crate) objective: Option<u32>,
    /// The revision of its report (`mission_tasks.revisions`): every
    /// verdict names the one it judged, and the store refuses it once the
    /// task has moved on to another.
    #[serde(default)]
    pub(crate) revisions: i32,
}

impl Mission {
    /// The task the gate judges next: one the supervisor accepted, or, when
    /// the gate alone accepts, also one just reported.
    fn ready(&self) -> Option<&Task> {
        self.tasks.iter().find(|task| {
            task.state == TaskState::Accepted
                || (self.accept == "gate" && task.state == TaskState::Reported)
        })
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

/// Whether a pushed branch may be judged, against the agent's latest
/// checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Freshness {
    /// The pushed head is the checkpoint, a descendant of it, or there is
    /// no checkpoint to compare with: judge this commit.
    Fresh(String),
    /// The push lags the checkpoint: wait for the pusher.
    Stale(String),
    /// Neither a checkpoint nor a pushed branch: nothing to land.
    Nothing,
}

/// The stale rule. `contains(ancestor, commit)` says whether `ancestor` is
/// reachable from `commit` in the integrator's mirror; a commit the mirror
/// lacks is contained by nothing.
pub(crate) fn freshness(
    checkpoint: Option<&str>,
    pushed: Option<&str>,
    contains: impl Fn(&str, &str) -> bool,
) -> Freshness {
    match (checkpoint, pushed) {
        (None, None) => Freshness::Nothing,
        (None, Some(pushed)) => Freshness::Fresh(pushed.to_string()),
        (Some(checkpoint), None) => Freshness::Stale(format!(
            "the source has no spawn/<agent> yet, and the latest checkpoint is {checkpoint}"
        )),
        (Some(checkpoint), Some(pushed)) => {
            if pushed == checkpoint || contains(checkpoint, pushed) {
                Freshness::Fresh(pushed.to_string())
            } else if contains(pushed, checkpoint) {
                Freshness::Stale(format!(
                    "the pushed branch ({pushed}) is behind the latest checkpoint ({checkpoint})"
                ))
            } else {
                Freshness::Stale(format!(
                    "the pushed branch ({pushed}) does not contain the latest checkpoint \
                     ({checkpoint})"
                ))
            }
        }
    }
}

/// The mirror's own copy of an agent's pushed branch: never
/// `refs/heads/<agent>`, which on the Bridge's host is the Bridge's.
fn integrator_ref(agent: &str) -> String {
    format!("refs/spawn-integrate/{agent}")
}

fn git(arguments: &[&str]) -> Result<String, String> {
    sources::git(arguments).map_err(|error| error.to_string())
}

/// The source's default branch and its commit, as `git ls-remote` sees it.
fn source_head(source: &str) -> Result<(String, String), String> {
    let source = sources::MissionSource::parse(source)?;
    let listing = git(&["ls-remote", "--symref", "--", source.as_str(), "HEAD"])?;
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
        return Err(format!("{} has no HEAD", source.as_str()));
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
    crate::config::require_git()?;
    let root = workspace_root(&flags)
        .map(PathBuf::from)
        .ok_or_else(|| format!("--root or SPAWN_WORKSPACE_ROOT is required\n{USAGE}"))?;
    // Resolved against the working directory: the gate must not resolve
    // inside the tree it judges.
    let gate = std::path::absolute(env_or("SPAWN_GATE", "scripts/rsi/gate.sh"))
        .map_err(|error| format!("SPAWN_GATE: {error}"))?;
    // The gate gets an allowlist of this environment, never all of it: the
    // owner's SPAWN_DATABASE_URL must not reach candidate code.
    let gate_env = gate::gate_env(|name| std::env::var(name).ok());
    if std::env::var_os("SPAWN_TEST_DATABASE_URL").is_some()
        && !gate_env
            .iter()
            .any(|(name, _)| name == "SPAWN_TEST_DATABASE_URL")
    {
        eprintln!(
            "integrate: SPAWN_TEST_DATABASE_URL names this integrator's own database; \
             the gate does not get it"
        );
    }
    let gates = gate_slots(
        flags.get("gates"),
        crate::config::env_value("SPAWN_INTEGRATE_GATES"),
    )
    .map_err(|error| format!("{error}\n{USAGE}"))?;
    let integrator = Integrator::new(root, gate, gate_env, gates);
    loop {
        let pass = integrator.pass();
        if once {
            // One pass, and the gates it started run to their records.
            integrator.settle();
        }
        if let Err(error) = pass {
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

struct Integrator {
    judge: Judge,
    /// Gates that may run at once ([`gate_slots`]).
    gates: usize,
    /// The judgings in flight, one per mission at most.
    running: RefCell<Vec<Running>>,
    /// The store whose connection holds the integrator's lock, kept while
    /// a judging runs: its thread records through it.
    held: RefCell<Option<Arc<PostgresStore>>>,
}

/// What a judging needs, owned, so it runs on a thread of its own.
#[derive(Clone)]
struct Judge {
    root: PathBuf,
    gate: PathBuf,
    /// The gate's whole environment ([`gate::gate_env`]).
    gate_env: Vec<(String, String)>,
}

/// One judging in flight.
struct Running {
    mission: i64,
    source: String,
    task: i64,
    handle: std::thread::JoinHandle<()>,
}

impl Integrator {
    fn new(root: PathBuf, gate: PathBuf, gate_env: Vec<(String, String)>, gates: usize) -> Self {
        Self {
            judge: Judge {
                root,
                gate,
                gate_env,
            },
            gates,
            running: RefCell::new(Vec::new()),
            held: RefCell::new(None),
        }
    }

    fn pass(&self) -> Result<(), String> {
        self.reap();
        // A store per pass while no gate runs: the advisory lock belongs to
        // the connection, and is released with it. While one runs, its
        // thread holds the store, so the lock stays this integrator's.
        if self.running.borrow().is_empty() {
            self.held.borrow_mut().take();
        }
        let held = self.held.borrow().clone();
        let store = if let Some(store) = held {
            store
        } else {
            let store = Arc::new(open_store()?);
            if !store
                .integrator_try_lock()
                .map_err(|error| error.to_string())?
            {
                return Ok(());
            }
            store
        };
        let pass = self.pass_on(&store);
        // A pass that failed may have lost the connection: the next pass
        // asks for the lock again (refused while the old connection lives).
        *self.held.borrow_mut() =
            (pass.is_ok() && !self.running.borrow().is_empty()).then_some(store);
        pass
    }

    /// Join the judgings that ended. Their records are in the log already;
    /// a thread that panicked recorded nothing, so its task is judged again.
    fn reap(&self) {
        let finished: Vec<Running> = {
            let mut running = self.running.borrow_mut();
            let (finished, still) = running
                .drain(..)
                .partition(|gate: &Running| gate.handle.is_finished());
            *running = still;
            finished
        };
        for gate in finished {
            Self::join(gate);
        }
    }

    /// Wait for every judging in flight (`--once`, and tests).
    fn settle(&self) {
        let running: Vec<Running> = self.running.borrow_mut().drain(..).collect();
        for gate in running {
            Self::join(gate);
        }
    }

    fn join(gate: Running) {
        if gate.handle.join().is_err() {
            eprintln!(
                "integrate: mission {}: the judging of task {} panicked; it is judged again",
                gate.mission, gate.task
            );
        }
    }

    /// The judging in flight that keeps `mission` out of this pass: its
    /// own, or one on its source.
    fn busy(&self, mission: &Mission) -> bool {
        self.running.borrow().iter().any(|gate| {
            gate.mission == mission.id || mission.source.as_deref() == Some(gate.source.as_str())
        })
    }

    /// One pass over every gated mission, holding the integrator's lock.
    fn pass_on(&self, store: &Arc<PostgresStore>) -> Result<(), String> {
        let missions: Vec<Mission> =
            serde_json::from_value(store.gated_missions().map_err(|error| error.to_string())?)
                .map_err(|error| format!("gated missions: {error}"))?;
        for mission in &missions {
            match self.mission(store, &missions, mission) {
                // Whatever one mission did may move another's view (a shared
                // source's main): the next pass reads everything again.
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(error) => eprintln!("integrate: mission {}: {error}", mission.id),
            }
        }
        Ok(())
    }

    /// One mission's step. True when it recorded anything. `missions` is
    /// every mission of the pass, for the landings on a shared source.
    fn mission(
        &self,
        store: &Arc<PostgresStore>,
        missions: &[Mission],
        mission: &Mission,
    ) -> Result<bool, String> {
        if mission.halted.is_some() || self.busy(mission) {
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
        if let Some(reason) = unlandable(source) {
            return halt(store, mission, &reason).map(|()| true);
        }
        let mirror = sources::ensure_mirror(&self.judge.root, Some(source))
            .map_err(|error| error.to_string())?;
        if let Some(task) = mission.landing() {
            return land(store, &mirror, source, task).map(|()| true);
        }
        // Another mission's landing on this source may have moved main
        // without its record yet (a crash between the push and
        // `task.landed`): main says nothing about this mission until that
        // landing is resolved, by its own mission's next step.
        if let Some((other, task)) = landing_elsewhere(missions, mission) {
            println!(
                "integrate: mission {}: waits for task {task} of mission {other} to land on                  its source",
                mission.id
            );
            return Ok(false);
        }
        // A fetch that failed leaves the mirror's last copy of main, which
        // is no reading of the source's main: no halt, no verdict by it.
        sources::refresh(&mirror, source)?;
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
        // An objective mission's free slots get their objectives; a closed
        // mission gets none. An assignment that fails does not keep a ready
        // task from its judging.
        let assigned = if mission.state == "done" {
            false
        } else {
            match objectives::assign_next(store, mission) {
                Ok(assigned) => !assigned.is_empty(),
                Err(error) if mission.ready().is_none() => return Err(error),
                Err(error) => {
                    eprintln!("integrate: mission {}: {error}", mission.id);
                    false
                }
            }
        };
        let Some(task) = mission.ready() else {
            return Ok(assigned);
        };
        let running = self.running.borrow().len();
        if running >= self.gates {
            println!(
                "integrate: mission {}: task {} waits for a gate ({running} of {} running)",
                mission.id, task.id, self.gates
            );
            return Ok(assigned);
        }
        self.start_judging(store, mirror, source, main, scenarios, count, mission.id, task, running)?;
        Ok(assigned)
    }

    /// Judge `task` on a thread of its own, under the gate cap.
    #[allow(clippy::too_many_arguments)]
    fn start_judging(
        &self,
        store: &Arc<PostgresStore>,
        mirror: PathBuf,
        source: &str,
        main: String,
        scenarios: PathBuf,
        count: u32,
        mission_id: i64,
        task: &Task,
        running: usize,
    ) -> Result<(), String> {
        let judge = self.judge.clone();
        let task_id = task.id;
        let (store, task, source_owned) = (Arc::clone(store), task.clone(), source.to_string());
        let handle = std::thread::Builder::new()
            .name(format!("gate-{}", task.id))
            .spawn(move || {
                if let Err(error) = judge.judge(
                    &store,
                    &mirror,
                    &source_owned,
                    &main,
                    &scenarios,
                    count,
                    &task,
                ) {
                    eprintln!("integrate: mission {mission_id}: {error}");
                }
            })
            .map_err(|error| format!("start the judging of task {task_id}: {error}"))?;
        println!(
            "integrate: mission {}: judging task {} ({} of {} gates)",
            mission_id,
            task_id,
            running + 1,
            self.gates
        );
        self.running.borrow_mut().push(Running {
            mission: mission_id,
            source: source.to_string(),
            task: task_id,
            handle,
        });
        Ok(())
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
                // The Stop first, under its fixed key: if the record then
                // fails, the next pass finds the task still delivered and
                // over budget, and submits (a no-op) and records again.
                stop(store, task)?;
                record(
                    store,
                    &json!({"type": "task.aborted", "id": task.id, "reason": reason}),
                )?;
                println!("integrate: task {} aborted: {reason}", task.id);
                aborted = true;
            }
        }
        Ok(aborted)
    }
}

impl Judge {
    /// The agent's pushed branch, fetched into the mirror, checked against
    /// its latest checkpoint. A lagging push is fetched again for up to
    /// [`PUSH_WAIT`] before it counts as stale.
    fn pushed_candidate(
        store: &PostgresStore,
        mirror: &Path,
        source: &str,
        agent: &str,
    ) -> Result<Freshness, String> {
        let checkpoint = store
            .latest_checkpoint(agent)
            .map_err(|error| error.to_string())?;
        let mirror_path = mirror.to_string_lossy().to_string();
        let contains = |ancestor: &str, commit: &str| {
            git(&[
                "--git-dir",
                &mirror_path,
                "merge-base",
                "--is-ancestor",
                ancestor,
                commit,
            ])
            .is_ok()
        };
        let deadline = std::time::Instant::now() + PUSH_WAIT;
        loop {
            let pushed = sources::fetch_pushed_into(mirror, source, agent, &integrator_ref(agent))
                .map_err(|error| error.to_string())?;
            let verdict = freshness(checkpoint.as_deref(), pushed.as_deref(), contains);
            if !matches!(verdict, Freshness::Stale(_)) || std::time::Instant::now() >= deadline {
                return Ok(verdict);
            }
            std::thread::sleep(PUSH_POLL);
        }
    }

    /// Judge one task from its pushed branch, on its own thread. True when
    /// the gate ran; a stale push runs none.
    #[allow(clippy::too_many_arguments)]
    fn judge(
        &self,
        store: &Arc<PostgresStore>,
        mirror: &Path,
        source: &str,
        main: &str,
        scenarios: &Path,
        count: u32,
        task: &Task,
    ) -> Result<bool, String> {
        let branch = match Self::pushed_candidate(store, mirror, source, &task.agent_id)? {
            Freshness::Fresh(branch) => Some(branch),
            Freshness::Nothing => None,
            Freshness::Stale(reason) => {
                // Not a verdict on the work: the pusher has not caught up.
                record(
                    store,
                    &json!({"type": "task.gated", "id": task.id, "verdict": "stale",
                            "revision": task.revisions, "reason": reason}),
                )?;
                println!("integrate: task {} stale: {reason}", task.id);
                return Ok(false);
            }
        };
        self.judge_pushed(
            store,
            mirror,
            source,
            main,
            scenarios,
            count,
            task,
            branch.as_deref(),
        )
        .map(|()| true)
    }

    /// Judge `branch`, the agent's pushed head already in the mirror (`None`:
    /// the agent has nothing to land).
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn judge_pushed(
        &self,
        store: &Arc<PostgresStore>,
        mirror: &Path,
        source: &str,
        main: &str,
        scenarios: &Path,
        count: u32,
        task: &Task,
        branch: Option<&str>,
    ) -> Result<(), String> {
        // Read only: the integrator never commits a workspace.
        let repo = GitWorkspace::new(mirror, &self.root);
        let gated = |verdict: &str, extra: Value| -> Result<(), String> {
            let mut record_value = json!({"type": "task.gated", "id": task.id, "verdict": verdict,
                                          "revision": task.revisions, "base": main,
                                          "scenarios": count});
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
        let Some(branch) = branch.map(str::to_string) else {
            return rejected("the worker has no branch to land", "");
        };
        // A merge of main whose markers are not resolved is not judged: the
        // markers could land, and a file rewritten instead of resolved would
        // revert main.
        let marked = repo
            .conflict_markers(main, &branch)
            .map_err(|error| error.to_string())?;
        if let Some(note) = markers_note(repo.has_pending_merge(&task.agent_id), &marked) {
            return gated("conflict", json!({ "note": note }));
        }
        let Some(candidate) = landing_candidate(&mirror_path, main, &branch, task.id)? else {
            // The worker cannot merge main itself: merge it into the
            // workspace now, while the worker waits for this verdict, so
            // the note names what it has to resolve.
            let merge = main_merge::merge_main(store, &self.root, &task.agent_id, &task.session_id);
            println!(
                "integrate: task {}: main into its workspace: {merge:?}",
                task.id
            );
            return gated(
                "conflict",
                json!({"note": main_merge::conflict_note(main, &merge)}),
            );
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
        // The test server must not be the owner's, by identity, asked now:
        // a socket mount or a DNS name can change between gate runs.
        let owner = std::env::var("SPAWN_DATABASE_URL").ok();
        let (gate_env, warning) =
            gate::withhold_owners_cluster(&self.gate_env, owner.as_deref(), |url| {
                spawn_postgres::cluster_identity(url).map_err(|error| error.to_string())
            });
        if let Some(warning) = warning {
            eprintln!("integrate: {warning}");
        }
        match gate::judge(
            &self.gate,
            mirror,
            main,
            &candidate,
            scenarios,
            scenario.as_deref(),
            &gate_env,
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
            Err(failure) => gated("aborted", aborted(&failure)),
        }
    }
}

/// The commit that would land `branch` (the agent's pushed head, in the
/// mirror) on `main`, or `None` when the two conflict. Only objects are
/// written, never a ref: the agent's `refs/heads/<agent>` stays the Bridge's,
/// and `spawn/<agent>` catches up with a landing through the Bridge's next
/// merge of main, which its pusher pushes like any checkpoint.
fn landing_candidate(
    mirror_path: &str,
    main: &str,
    branch: &str,
    task: i64,
) -> Result<Option<String>, String> {
    let main_is_in = git(&[
        "--git-dir",
        mirror_path,
        "merge-base",
        "--is-ancestor",
        main,
        branch,
    ])
    .is_ok();
    if main_is_in {
        // Large files are workspace state, never source: a branch head
        // that names a large-file manifest lands as a commit of the same
        // tree without it, so nothing forked from main inherits them.
        let message = git(&[
            "--git-dir",
            mirror_path,
            "show",
            "-s",
            "--format=%B",
            branch,
        ])?;
        if !spawn_host::names_files_outside_git(&message) {
            return Ok(Some(branch.to_string()));
        }
        let tree = git(&[
            "--git-dir",
            mirror_path,
            "rev-parse",
            &format!("{branch}^{{tree}}"),
        ])?;
        return commit_landing(
            mirror_path,
            &[&tree, "-p", branch, "-m", &format!("Land task {task}")],
        )
        .map(Some);
    }
    // Main moved since the worker started: land the merge of both, by
    // main's merge rules (a bare repository reads no in-tree
    // .gitattributes otherwise), as main is merged into agents' branches.
    let output = sources::controller_git()
        .args([
            "--git-dir",
            mirror_path,
            &format!("--attr-source={main}"),
            "merge-tree",
            "--write-tree",
            main,
            branch,
        ])
        .output()
        .map_err(|error| format!("run git merge-tree: {error}"))?;
    let Some(tree) = merged_tree(
        output.status.code(),
        &String::from_utf8_lossy(&output.stdout),
        &String::from_utf8_lossy(&output.stderr),
    )?
    else {
        return Ok(None);
    };
    commit_landing(
        mirror_path,
        &[
            &tree,
            "-p",
            main,
            "-p",
            branch,
            "-m",
            &format!("Land task {task} (merged with main)"),
        ],
    )
    .map(Some)
}

/// What `git merge-tree --write-tree` said, from its exit code (`None`: no
/// code, killed by a signal) and output: `Ok(Some(tree))` for a clean merge,
/// `Ok(None)` for a conflict (exit 1, the only code git gives one), and any
/// other failure an error with git's own message. A git that cannot run the
/// merge (an unknown option, a crash) says nothing about the work: it must
/// reach the operator, never send the worker after conflicts that do not
/// exist.
fn merged_tree(code: Option<i32>, stdout: &str, stderr: &str) -> Result<Option<String>, String> {
    match code {
        Some(0) => Ok(Some(
            stdout.lines().next().unwrap_or_default().trim().to_string(),
        )),
        Some(1) => Ok(None),
        failure => {
            let how = failure.map_or_else(
                || "was killed by a signal".to_string(),
                |code| format!("exited {code}"),
            );
            Err(format!(
                "git merge-tree {how}, so the landing could not be merged (this is not a \
                 conflict; Spawn needs git {}.{} or later): {}",
                crate::config::MIN_GIT.0,
                crate::config::MIN_GIT.1,
                stderr.trim()
            ))
        }
    }
}

/// Name and email of the author and committer of every landing commit.
const LANDING_IDENTITY: (&str, &str) = ("spawn-integrate", "integrate@localhost");

/// `git commit-tree <arguments>` in the mirror, authored and committed as
/// [`LANDING_IDENTITY`] and unsigned: a landing never depends on the host's
/// git configuration (no identity in `~/.gitconfig`, another one there or in
/// the environment, `commit.gpgSign`). The identity goes in the environment,
/// which git takes over any configuration.
fn commit_landing(mirror_path: &str, arguments: &[&str]) -> Result<String, String> {
    let (name, email) = LANDING_IDENTITY;
    let output = sources::controller_git()
        .args(["--git-dir", mirror_path, "commit-tree", "--no-gpg-sign"])
        .args(arguments)
        .env("GIT_AUTHOR_NAME", name)
        .env("GIT_AUTHOR_EMAIL", email)
        .env("GIT_COMMITTER_NAME", name)
        .env("GIT_COMMITTER_EMAIL", email)
        .output()
        .map_err(|error| format!("run git: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git commit-tree: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The note that sends work back before it is judged, when a merge of main
/// waits for its markers (`pending`) or the branch carries marker blocks in
/// the files it changes (`marked`).
fn markers_note(pending: bool, marked: &[String]) -> Option<String> {
    if !pending && marked.is_empty() {
        return None;
    }
    let files = if marked.is_empty() {
        "the files the merge listed".to_string()
    } else {
        marked.join(", ")
    };
    Some(format!(
        "Your work still carries conflict markers from merging the source's main ({files}). \
         Resolve them first: edit the marked regions (<<<<<<< / ======= / >>>>>>>), keeping \
         both sides' intent; do not rewrite the files from memory. Then report again."
    ))
}

/// The record of a gate run without a verdict to believe. A gate that never
/// returns, cannot run, or dies (killed, or exiting non-zero, whatever it
/// printed) says nothing about the work: an abort, never an acceptance.
fn aborted(failure: &GateFailure) -> Value {
    match failure {
        GateFailure::Timeout => json!({"reason": "the gate exceeded its wall clock"}),
        GateFailure::Failed(error) => json!({"reason": format!("the gate failed: {error}")}),
        GateFailure::Aborted { reason, log } => {
            json!({"reason": format!("the gate failed: {reason}"), "log": log})
        }
    }
}

/// Move the source's default branch from the task's base to its candidate,
/// only if it is still at the base, and record the landing. A branch that
/// moved meanwhile sends the task back to be judged again, unless the
/// candidate is already in its history: a push of an earlier pass that was
/// never recorded (the integrator stopped in between), with main moved on
/// top of it since. That landing happened, and is recorded so.
fn land(store: &PostgresStore, mirror: &Path, source: &str, task: &Task) -> Result<(), String> {
    let (Some(base), Some(candidate)) = (task.base.as_deref(), task.candidate.as_deref()) else {
        return record(
            store,
            &json!({"type": "task.gated", "id": task.id, "verdict": "stale",
                    "revision": task.revisions}),
        );
    };
    // Held until the landing is recorded: dropping the mission waits for it.
    let Some(_hold) = store
        .landing_hold(task.id)
        .map_err(|error| error.to_string())?
    else {
        println!(
            "integrate: task {} is no longer landing (its mission was dropped)",
            task.id
        );
        return Ok(());
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
        // Main moved on: past this candidate (it landed before), or past
        // its base without it. The fetch brings `now` into the mirror; one
        // that fails decides nothing this pass.
        sources::refresh(mirror, source)?;
        let under_main = git(&[
            "--git-dir",
            &mirror_path,
            "merge-base",
            "--is-ancestor",
            candidate,
            &now,
        ])
        .is_ok();
        if !under_main {
            println!(
                "integrate: task {}: {branch} moved to {now}; judging again",
                task.id
            );
            return record(
                store,
                &json!({"type": "task.gated", "id": task.id, "verdict": "stale",
                        "revision": task.revisions}),
            );
        }
        println!(
            "integrate: task {}: {candidate} is already under {branch} ({now})",
            task.id
        );
    } else {
        let _ = git(&[
            "--git-dir",
            &mirror_path,
            "update-ref",
            "refs/heads/main",
            candidate,
        ]);
    }
    record(
        store,
        &json!({"type": "task.landed", "id": task.id, "from": base, "to": candidate}),
    )?;
    println!("integrate: task {} landed {candidate} on {source}", task.id);
    Ok(())
}

/// Why a mission on `source` can never land, if it cannot: agents' branches
/// reach only a directory on this host (the pusher's one destination since
/// the cross-host cut), so a gate on a URL would wait for a push nobody
/// makes, `stale` on every pass. `spawn_mission_configure` refuses such a
/// gate; one configured before that is halted, so a human sees it. A local
/// path that is missing now is not this: its fetch fails, and the pass
/// skips the mission.
fn unlandable(source: &str) -> Option<String> {
    (!source.starts_with('/')).then(|| {
        format!(
            "its source {source} is remote: agents' branches reach only a directory on this \
             host, so its work never lands"
        )
    })
}

/// Another mission's task landing on `mission`'s source, if one is:
/// `(mission, task)`. Its push may have moved main before its record.
fn landing_elsewhere(missions: &[Mission], mission: &Mission) -> Option<(i64, i64)> {
    missions
        .iter()
        .filter(|other| other.id != mission.id && other.source == mission.source)
        .find_map(|other| other.landing().map(|task| (other.id, task.id)))
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

/// Submit the Stop of an aborted task's worker. Its outcome is the
/// command's: one that fails is pursued and escalated by the dispatcher
/// (`dispatch::stops`), like every Stop.
fn stop(store: &PostgresStore, task: &Task) -> Result<(), String> {
    let error = |error: &dyn std::fmt::Display| format!("stop task {}'s worker: {error}", task.id);
    let submission = Submission {
        agent_id: AgentId::new(task.agent_id.clone()).map_err(|e| error(&e))?,
        session_id: SessionId::new(task.session_id.clone()).map_err(|e| error(&e))?,
        idempotency_key: IdempotencyKey::new(format!("integrate-abort-{}", task.id))
            .map_err(|e| error(&e))?,
        action: CommandAction::Stop,
    };
    let principal = Principal::new(PRINCIPAL).map_err(|e| error(&e))?;
    store
        .submit(&principal, &submission)
        .map(|_| ())
        .map_err(|e| error(&e))
}

#[cfg(test)]
mod tests {
    use super::{
        Freshness, Mission, aborted, freshness, halt_reason, markers_note, merged_tree, over_budget,
    };
    use crate::gate::GateFailure;
    use serde_json::json;

    /// Only exit 1 is a conflict: a merge-tree git cannot run (an unknown
    /// option, a crash, a signal) is an error carrying git's message.
    #[test]
    fn the_gate_cap_defaults_to_two_and_is_bounded() {
        use super::{DEFAULT_GATES, MAX_GATES, gate_slots};
        assert_eq!(gate_slots(None, None), Ok(DEFAULT_GATES));
        assert_eq!(DEFAULT_GATES, 2);
        assert_eq!(gate_slots(None, Some("3".into())), Ok(3));
        assert_eq!(
            gate_slots(Some("1".into()), Some("3".into())),
            Ok(1),
            "the flag first"
        );
        for wrong in ["0", "-1", "two", ""] {
            assert!(gate_slots(Some(wrong.into()), None).is_err(), "{wrong}");
        }
        assert!(gate_slots(None, Some((MAX_GATES + 1).to_string())).is_err());
    }

    #[test]
    fn a_merge_git_cannot_run_is_an_error_not_a_conflict() {
        assert_eq!(
            merged_tree(Some(0), "abc123\n", ""),
            Ok(Some("abc123".to_string()))
        );
        assert_eq!(merged_tree(Some(1), "def456\nCONFLICT", ""), Ok(None));
        let unknown = merged_tree(
            Some(129),
            "",
            "error: unknown option `attr-source=abc'\nusage: git [-v | --version]",
        )
        .expect_err("exit 129 is no conflict");
        assert!(unknown.contains("unknown option"), "{unknown}");
        assert!(unknown.contains("exited 129"), "{unknown}");
        assert!(unknown.contains("not a conflict"), "{unknown}");
        assert!(unknown.contains("2.43"), "{unknown}");
        let crash = merged_tree(None, "", "").expect_err("a signal is no conflict");
        assert!(crash.contains("killed by a signal"), "{crash}");
        assert!(merged_tree(Some(139), "", "Segmentation fault").is_err());
        assert!(merged_tree(Some(128), "", "fatal: bad object").is_err());
        assert!(merged_tree(Some(2), "", "").is_err());
    }

    /// Commit everything in the checkout `directory`.
    pub(super) fn commit_in(directory: &str, message: &str) {
        let git = crate::sources::git;
        git(&["-C", directory, "add", "-A"]).expect("add");
        git(&[
            "-C",
            directory,
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "--no-gpg-sign",
            "-qm",
            message,
        ])
        .expect("commit");
    }

    /// What the integrator does with an agent's push that passed: fetch it
    /// into its own ref, build the landing ([`super::landing_candidate`]),
    /// move the source's main and the mirror's as `land` does.
    fn land_as_the_integrator(mirror: &std::path::Path, source: &str, agent: &str, task: i64) {
        let git = crate::sources::git;
        let mirror_path = mirror.to_string_lossy().to_string();
        let tracking = format!("refs/spawn-integrate/{agent}");
        let pushed = crate::sources::fetch_pushed_into(mirror, source, agent, &tracking)
            .expect("fetch")
            .expect("pushed");
        let main = git(&["--git-dir", &mirror_path, "rev-parse", "refs/heads/main"]).expect("main");
        let candidate = super::landing_candidate(&mirror_path, &main, &pushed, task)
            .expect("candidate")
            .expect("no conflict");
        assert_ne!(candidate, pushed, "a landing of its own");
        let to_main = format!("{candidate}:refs/heads/main");
        git(&["--git-dir", &mirror_path, "push", "-q", source, &to_main]).expect("land");
        git(&[
            "--git-dir",
            &mirror_path,
            "update-ref",
            "refs/heads/main",
            &candidate,
        ])
        .expect("mirror main");
    }

    /// After a landing, the agent's pushed branch is not left behind: the
    /// integrator writes no ref of the agent, and the Bridge's next merge of
    /// main is a new commit (which `MissionWorkspaces` marks for the
    /// pusher), so `spawn/<agent>` contains main again once pushed. Both
    /// landing shapes that are not the branch head itself: a commit without
    /// the large-file trailer, and a merge with a main that moved.
    #[test]
    fn a_landing_reaches_the_agents_pushed_branch_through_the_next_merge() {
        use crate::push::{Pushed, push_branch};
        use crate::sources::{ensure_mirror, git};
        use spawn_core::id::BranchName;
        use spawn_core::port::{BaseMerge, Commit, WorkspaceStore};
        use spawn_host::GitWorkspace;

        let base = std::env::temp_dir().join(format!("spawn-land-push-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).expect("seed directory");
        let seed_path = seed.to_string_lossy().to_string();
        git(&["init", "-q", "--initial-branch=main", &seed_path]).expect("init");
        std::fs::write(seed.join("README.md"), "base\n").expect("write");
        commit_in(&seed_path, "base");
        let source = base.join("source.git").to_string_lossy().to_string();
        git(&["clone", "-q", "--bare", &seed_path, &source]).expect("bare source");
        let root = base.join("host");
        let mirror = ensure_mirror(&root, Some(&source)).expect("mirror");
        let mirror_path = mirror.to_string_lossy().to_string();
        let store = GitWorkspace::new(&mirror, &root);
        let rev = |reference: &str| {
            git(&["--git-dir", &mirror_path, "rev-parse", reference]).expect("rev-parse")
        };
        let source_rev = |reference: &str| {
            git(&["--git-dir", &source, "rev-parse", reference]).expect("rev-parse on source")
        };

        for (task, agent, main_moves) in [(1, "agent_large", false), (2, "agent_late", true)] {
            let branch = BranchName::new(agent).expect("valid branch");
            store.ensure_branch(&branch, None).expect("branch");
            let workspace = store.mount(&branch).expect("mount");
            let directory = std::path::Path::new(workspace.path());
            std::fs::write(directory.join(format!("{agent}.txt")), "work\n").expect("write");
            assert!(matches!(
                store.commit(&workspace, "work").expect("commit"),
                Commit::Created(_)
            ));
            if !main_moves {
                // A head an earlier version wrote, naming large files kept
                // outside git: the landing is a commit of its own, without
                // the trailer.
                let head = rev(&format!("refs/heads/{agent}"));
                let message = format!("work\n\nSpawn-Large: {}", "a".repeat(64));
                let named = git(&[
                    "--git-dir",
                    &mirror_path,
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "commit-tree",
                    &format!("{head}^{{tree}}"),
                    "-p",
                    &head,
                    "-m",
                    &message,
                ])
                .expect("a head with the trailer");
                let reference = format!("refs/heads/{agent}");
                git(&[
                    "--git-dir",
                    &mirror_path,
                    "update-ref",
                    &reference,
                    &named,
                    &head,
                ])
                .expect("move the branch");
            }
            assert_eq!(push_branch(&mirror, &source, agent), Pushed::Done);
            if main_moves {
                // Main moved since the worker started.
                let clone = base.join(format!("clone-{agent}"));
                let clone_path = clone.to_string_lossy().to_string();
                git(&["clone", "-q", &source, &clone_path]).expect("clone");
                std::fs::write(clone.join("other.txt"), "theirs\n").expect("write");
                commit_in(&clone_path, "elsewhere");
                git(&["-C", &clone_path, "push", "-q", "origin", "main"]).expect("push main");
                crate::sources::refresh(&mirror, &source).expect("refresh");
            }

            let agent_head = rev(&format!("refs/heads/{agent}"));
            land_as_the_integrator(&mirror, &source, agent, task);
            assert_eq!(
                rev(&format!("refs/heads/{agent}")),
                agent_head,
                "the integrator moves no ref of the agent"
            );

            // The Bridge's next merge of main is a new commit, and once
            // pushed the agent's branch on the source holds the landing.
            let BaseMerge::Merged { commit, .. } = store.merge_base(&workspace).expect("merge")
            else {
                panic!("the landing is new to the branch");
            };
            assert_eq!(push_branch(&mirror, &source, agent), Pushed::Done);
            let pushed_now = source_rev(&format!("refs/heads/spawn/{agent}"));
            assert_eq!(pushed_now, commit.as_str());
            git(&[
                "--git-dir",
                &source,
                "merge-base",
                "--is-ancestor",
                &source_rev("refs/heads/main"),
                &pushed_now,
            ])
            .expect("spawn/<agent> contains main");
            store.unmount(&workspace).expect("unmount");
        }
    }

    /// A source with `README.md` and `notes.txt` on main, its mirror under a
    /// host root, and that host's store: `(base, source, root, mirror, store)`.
    fn world(
        tag: &str,
    ) -> (
        std::path::PathBuf,
        String,
        std::path::PathBuf,
        std::path::PathBuf,
        spawn_host::GitWorkspace,
    ) {
        use crate::sources::{ensure_mirror, git};
        let base = std::env::temp_dir().join(format!("spawn-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).expect("seed directory");
        let seed_path = seed.to_string_lossy().to_string();
        git(&["init", "-q", "--initial-branch=main", &seed_path]).expect("init");
        std::fs::write(seed.join("README.md"), "one\ntwo\nthree\n").expect("write");
        std::fs::write(seed.join("notes.txt"), "notes\n").expect("write");
        commit_in(&seed_path, "base");
        let source = base.join("source.git").to_string_lossy().to_string();
        git(&["clone", "-q", "--bare", &seed_path, &source]).expect("bare source");
        let root = base.join("host");
        let mirror = ensure_mirror(&root, Some(&source)).expect("mirror");
        let store = spawn_host::GitWorkspace::new(&mirror, &root);
        (base, source, root, mirror, store)
    }

    /// Main moves on the source by `change` in a clone, and the mirror
    /// follows; main's new commit.
    fn main_moves(
        base: &std::path::Path,
        source: &str,
        mirror: &std::path::Path,
        change: impl Fn(&std::path::Path),
    ) -> String {
        use crate::sources::git;
        let clone = base.join(format!("clone-{}", fastrand_name()));
        let clone_path = clone.to_string_lossy().to_string();
        git(&["clone", "-q", source, &clone_path]).expect("clone");
        change(&clone);
        commit_in(&clone_path, "main moves");
        git(&["-C", &clone_path, "push", "-q", "origin", "main"]).expect("push main");
        crate::sources::refresh(mirror, source).expect("refresh");
        git(&[
            "--git-dir",
            &mirror.to_string_lossy(),
            "rev-parse",
            "refs/heads/main",
        ])
        .expect("main")
    }

    fn fastrand_name() -> String {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        NEXT.fetch_add(1, Ordering::Relaxed).to_string()
    }

    /// An agent's branch with one checkpoint by `work`, mounted, and pushed
    /// as `spawn/<agent>`: its workspace.
    fn agent_with_work(
        store: &spawn_host::GitWorkspace,
        mirror: &std::path::Path,
        source: &str,
        agent: &str,
        work: impl Fn(&std::path::Path),
    ) -> spawn_core::id::WorkspaceId {
        use crate::push::{Pushed, push_branch};
        use spawn_core::id::BranchName;
        use spawn_core::port::{Commit, WorkspaceStore};
        let branch = BranchName::new(agent).expect("valid branch");
        store.ensure_branch(&branch, None).expect("branch");
        let workspace = store.mount(&branch).expect("mount");
        work(std::path::Path::new(workspace.path()));
        assert!(matches!(
            store.commit(&workspace, "work").expect("commit"),
            Commit::Created(_)
        ));
        assert_eq!(push_branch(mirror, source, agent), Pushed::Done);
        workspace
    }

    /// What one integrator pass judges of `agent`'s push against the
    /// mirror's main: the landing candidate, `None` for a conflict.
    fn judged(mirror: &std::path::Path, source: &str, agent: &str) -> Option<String> {
        let mirror_path = mirror.to_string_lossy().to_string();
        let pushed = crate::sources::fetch_pushed_into(
            mirror,
            source,
            agent,
            &format!("refs/spawn-integrate/{agent}"),
        )
        .expect("fetch")
        .expect("pushed");
        let main =
            crate::sources::git(&["--git-dir", &mirror_path, "rev-parse", "refs/heads/main"])
                .expect("main");
        super::landing_candidate(&mirror_path, &main, &pushed, 1).expect("candidate")
    }

    /// The whole loop of a conflict: the verdict merges main into the idle
    /// workspace with markers and leaves the merge pending, its note names
    /// the marked file, a report before they are resolved is sent back, the
    /// checkpoint that resolves them has main as its second parent, and that
    /// work lands.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn a_conflict_verdict_leaves_markers_the_resolution_merges_main_and_lands() {
        use crate::main_merge::{Merge, conflict_note, into_workspace};
        use crate::push::{Pushed, push_branch};
        use crate::sources::git;
        use spawn_core::port::{Commit, WorkspaceStore};

        let (base, source, root, mirror, store) = world("conflict-loop");
        let mirror_path = mirror.to_string_lossy().to_string();
        let agent = "agent_resident";
        let workspace = agent_with_work(&store, &mirror, &source, agent, |directory| {
            std::fs::write(
                directory.join("README.md"),
                "one\nTWO by the agent\nthree\n",
            )
            .expect("write");
        });
        let main = main_moves(&base, &source, &mirror, |clone| {
            std::fs::write(clone.join("README.md"), "one\nTWO on main\nthree\n").expect("write");
        });
        assert_eq!(judged(&mirror, &source, agent), None, "a conflict verdict");

        // The verdict's merge, before the note is written.
        let merge = into_workspace(&store, &mirror, &root, agent);
        assert_eq!(
            merge,
            Merge::Marked {
                files: vec!["README.md".to_string()]
            }
        );
        assert!(
            store.has_pending_merge(agent),
            "main waits as the second parent"
        );
        let readme = std::fs::read_to_string(root.join(agent).join("README.md")).expect("read");
        assert!(
            readme.contains("<<<<<<<") && readme.contains("TWO on main"),
            "{readme}"
        );
        let note = conflict_note(&main, &merge);
        assert!(
            note.contains("README.md")
                && note.contains("carry conflict markers")
                && note.contains(&main)
                && !note.contains("within about 30 s"),
            "{note}"
        );
        // Asked again (the next delivery), the merge is still the same one.
        assert_eq!(into_workspace(&store, &mirror, &root, agent), merge);

        // A report with the markers in: the judge sends it back unjudged.
        assert!(matches!(
            store.commit(&workspace, "markers").expect("commit"),
            Commit::Created(_)
        ));
        assert_eq!(push_branch(&mirror, &source, agent), Pushed::Done);
        let head = git(&[
            "--git-dir",
            &mirror_path,
            "rev-parse",
            &format!("refs/heads/{agent}"),
        ])
        .expect("head");
        let marked = store.conflict_markers(&main, &head).expect("markers");
        assert!(
            super::markers_note(store.has_pending_merge(agent), &marked).is_some(),
            "unresolved markers are not judged"
        );

        // The agent resolves them; that checkpoint records the merge.
        std::fs::write(
            root.join(agent).join("README.md"),
            "one\nTWO by the agent, on main\nthree\n",
        )
        .expect("resolve");
        let Commit::Created(resolved) = store.commit(&workspace, "resolved").expect("commit")
        else {
            panic!("the resolution is a commit");
        };
        assert_eq!(
            git(&[
                "--git-dir",
                &mirror_path,
                "rev-parse",
                &format!("{resolved}^2")
            ])
            .expect("second parent"),
            main,
            "main is the resolved checkpoint's second parent"
        );
        assert!(!store.has_pending_merge(agent));
        assert_eq!(push_branch(&mirror, &source, agent), Pushed::Done);
        assert!(
            store
                .conflict_markers(&main, resolved.as_str())
                .expect("markers")
                .is_empty()
        );
        assert_eq!(super::markers_note(false, &[]), None);

        // Judged again: it contains main, so it lands as it is.
        let candidate = judged(&mirror, &source, agent).expect("no conflict now");
        assert_eq!(candidate, resolved.as_str());
        git(&[
            "--git-dir",
            &mirror_path,
            "push",
            "-q",
            &source,
            &format!("{candidate}:refs/heads/main"),
        ])
        .expect("lands with a fast-forward of main");
        assert_eq!(
            git(&["--git-dir", &source, "show", "main:README.md"]).expect("main's README"),
            "one\nTWO by the agent, on main\nthree"
        );
        store.unmount(&workspace).expect("unmount");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A conflict markers cannot show: nothing in the workspace changes but
    /// main's version of each file, copied aside, and the note says what to
    /// do with each one. Done as it says, the work lands.
    #[test]
    fn a_conflict_without_markers_is_explained_and_resolvable_without_git() {
        use crate::main_merge::{MAIN_COPIES, Merge, conflict_note, into_workspace};
        use crate::push::{Pushed, push_branch};
        use spawn_core::port::{Commit, WorkspaceStore};

        let (base, source, root, mirror, store) = world("conflict-unmarked");
        let agent = "agent_unmarked";
        // The agent deletes notes.txt and changes README.md; main changes
        // notes.txt and deletes README.md.
        let workspace = agent_with_work(&store, &mirror, &source, agent, |directory| {
            std::fs::remove_file(directory.join("notes.txt")).expect("delete");
            std::fs::write(directory.join("README.md"), "one\nmine\nthree\n").expect("write");
            std::fs::write(directory.join("work.txt"), "my work\n").expect("write");
        });
        let main = main_moves(&base, &source, &mirror, |clone| {
            std::fs::write(clone.join("notes.txt"), "notes, revised\n").expect("write");
            std::fs::remove_file(clone.join("README.md")).expect("delete");
        });
        assert_eq!(judged(&mirror, &source, agent), None, "a conflict verdict");

        let merge = into_workspace(&store, &mirror, &root, agent);
        let Merge::Unmarked { files, copied } = &merge else {
            panic!("no markers can show this: {merge:?}");
        };
        assert_eq!(files, &["README.md".to_string(), "notes.txt".to_string()]);
        assert_eq!(copied, &["notes.txt".to_string()]);
        assert!(!store.has_pending_merge(agent));
        let directory = root.join(agent);
        assert_eq!(
            std::fs::read_to_string(directory.join(MAIN_COPIES).join("notes.txt")).expect("copy"),
            "notes, revised\n"
        );
        assert!(
            !directory.join("notes.txt").exists(),
            "the workspace is unchanged"
        );
        let note = conflict_note(&main, &merge);
        assert!(
            note.contains("cannot show")
                && note.contains(".spawn/main")
                && note.contains("make each of notes.txt exactly main's version")
                && note.contains("delete README.md"),
            "{note}"
        );

        // What the note says, done by hand: then it merges and lands.
        std::fs::copy(
            directory.join(MAIN_COPIES).join("notes.txt"),
            directory.join("notes.txt"),
        )
        .expect("copy main's version");
        std::fs::remove_file(directory.join("README.md")).expect("delete");
        assert!(matches!(
            store.commit(&workspace, "resolved").expect("commit"),
            Commit::Created(_)
        ));
        assert_eq!(push_branch(&mirror, &source, agent), Pushed::Done);
        assert!(
            judged(&mirror, &source, agent).is_some(),
            "the resolved work merges with main"
        );
        store.unmount(&workspace).expect("unmount");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A task delivered to a worker whose branch does not contain main gets
    /// main first: a clean merge is a merge commit (the `base_merged`
    /// checkpoint) and the brief names what changed; a conflicting one
    /// leaves markers and the brief names the marked files. A worker with
    /// no workspace on this host gets nothing.
    #[test]
    fn a_delivery_to_a_stale_branch_merges_main_first() {
        use crate::main_merge::{Merge, delivery_note, into_workspace};
        use crate::sources::git;
        use spawn_core::port::WorkspaceStore;

        let (base, source, root, mirror, store) = world("delivery-merge");
        let mirror_path = mirror.to_string_lossy().to_string();
        let clean = agent_with_work(&store, &mirror, &source, "agent_clean", |directory| {
            std::fs::write(directory.join("clean.txt"), "mine\n").expect("write");
        });
        let marked = agent_with_work(&store, &mirror, &source, "agent_marked", |directory| {
            std::fs::write(
                directory.join("README.md"),
                "one\nTWO by the agent\nthree\n",
            )
            .expect("write");
        });
        let main = main_moves(&base, &source, &mirror, |clone| {
            std::fs::write(clone.join("README.md"), "one\nTWO on main\nthree\n").expect("write");
            std::fs::write(clone.join("added.txt"), "from main\n").expect("write");
        });

        let merge = into_workspace(&store, &mirror, &root, "agent_clean");
        let Merge::Merged { commit, files } = &merge else {
            panic!("a clean merge: {merge:?}");
        };
        assert_eq!(files, &["README.md".to_string(), "added.txt".to_string()]);
        assert_eq!(
            git(&[
                "--git-dir",
                &mirror_path,
                "rev-parse",
                "refs/heads/agent_clean"
            ])
            .expect("head"),
            *commit
        );
        assert_eq!(
            git(&[
                "--git-dir",
                &mirror_path,
                "rev-parse",
                &format!("{commit}^2")
            ])
            .expect("second parent"),
            main
        );
        assert!(root.join("agent_clean").join("added.txt").exists());
        let note = delivery_note(&merge, &[]).expect("the brief says so");
        assert!(
            note.contains("added.txt") && note.contains("without a conflict"),
            "{note}"
        );
        assert_eq!(
            into_workspace(&store, &mirror, &root, "agent_clean"),
            Merge::Current,
            "a branch that contains main is left alone"
        );
        // The merge is read back from the branch: a record whose append
        // failed is written at the next merge (causality audit #8).
        assert_eq!(
            crate::main_merge::head_merge(&mirror, "agent_clean"),
            Some(merge.clone())
        );
        assert_eq!(
            crate::main_merge::head_merge(&mirror, "agent_marked"),
            None,
            "a checkpoint is no merge"
        );
        assert_eq!(delivery_note(&Merge::Current, &[]), None);
        // The first delivery was refused before typing (a pane not ready);
        // its retry finds main already in, and still tells what the
        // recorded merge changed (causality audit #7).
        let note = delivery_note(&Merge::Current, &["added.txt".to_string()])
            .expect("the retry's brief says so");
        assert!(
            note.contains("added.txt") && note.contains("without a conflict"),
            "{note}"
        );

        let merge = into_workspace(&store, &mirror, &root, "agent_marked");
        assert_eq!(
            merge,
            Merge::Marked {
                files: vec!["README.md".to_string()]
            }
        );
        assert!(store.has_pending_merge("agent_marked"));
        let note = delivery_note(&merge, &[]).expect("the brief says so");
        assert!(
            note.contains("README.md") && note.contains("conflict markers"),
            "{note}"
        );

        assert_eq!(
            into_workspace(&store, &mirror, &root, "agent_elsewhere"),
            Merge::NotMounted
        );
        store.unmount(&clean).expect("unmount");
        store.unmount(&marked).expect("unmount");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// History c1 <- c2 <- c3, and d2 forked from c1: `contains(a, b)` when
    /// `a` is reachable from `b`.
    fn history(ancestor: &str, commit: &str) -> bool {
        let parents = |commit: &str| match commit {
            "c2" | "d2" => Some("c1"),
            "c3" => Some("c2"),
            _ => None,
        };
        let mut at = Some(commit);
        while let Some(current) = at {
            if current == ancestor {
                return true;
            }
            at = parents(current);
        }
        false
    }

    #[test]
    fn a_push_at_the_checkpoint_or_past_it_is_judged() {
        assert_eq!(
            freshness(Some("c2"), Some("c2"), history),
            Freshness::Fresh("c2".to_string()),
            "equal"
        );
        assert_eq!(
            freshness(Some("c2"), Some("c3"), history),
            Freshness::Fresh("c3".to_string()),
            "descendant"
        );
        assert_eq!(
            freshness(None, Some("c1"), history),
            Freshness::Fresh("c1".to_string()),
            "no checkpoint to compare with"
        );
    }

    #[test]
    fn a_push_that_lags_the_checkpoint_is_stale() {
        let behind = freshness(Some("c2"), Some("c1"), history);
        assert!(
            matches!(&behind, Freshness::Stale(reason) if reason.contains("behind")),
            "{behind:?}"
        );
        let diverged = freshness(Some("c2"), Some("d2"), history);
        assert!(
            matches!(&diverged, Freshness::Stale(reason)
                if reason == "the pushed branch (d2) does not contain the latest checkpoint (c2)"),
            "{diverged:?}"
        );
        let unknown = freshness(Some("elsewhere"), Some("c3"), history);
        assert!(
            matches!(unknown, Freshness::Stale(_)),
            "a checkpoint the mirror lacks"
        );
        let absent = freshness(Some("c2"), None, history);
        assert!(
            matches!(&absent, Freshness::Stale(reason) if reason.contains("no spawn/<agent>")),
            "{absent:?}"
        );
    }

    #[test]
    fn no_checkpoint_and_no_pushed_branch_is_nothing_to_land() {
        assert_eq!(freshness(None, None, history), Freshness::Nothing);
    }

    #[test]
    fn unresolved_markers_send_work_back_before_the_gate() {
        assert_eq!(markers_note(false, &[]), None);
        let note = markers_note(false, &["README.md".to_string()]).expect("a note");
        assert!(
            note.contains("README.md") && note.contains("do not rewrite"),
            "{note}"
        );
        let note = markers_note(true, &[]).expect("a pending merge is enough");
        assert!(note.contains("the files the merge listed"), "{note}");
    }

    #[test]
    fn a_gate_without_a_believable_verdict_is_recorded_as_an_abort_with_its_log() {
        let record = aborted(&GateFailure::Aborted {
            reason: "the gate ended with signal: 9 (SIGKILL)".to_string(),
            log: "{\"verdict\":\"accepted\"}\n".to_string(),
        });
        assert!(
            record["reason"]
                .as_str()
                .is_some_and(|r| r.contains("SIGKILL"))
        );
        assert!(
            record["log"]
                .as_str()
                .is_some_and(|l| l.contains("accepted"))
        );
        assert!(
            record.get("verdict").is_none(),
            "the caller records it as aborted"
        );
        assert!(
            aborted(&GateFailure::Timeout)["reason"]
                .as_str()
                .is_some_and(|r| r.contains("wall clock"))
        );
    }

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
        // A report the supervisor accepted anyway is judged too, not left
        // accepted forever.
        gated.tasks.remove(0);
        assert_eq!(gated.ready().map(|task| task.id), Some(2));
    }

    /// Both landing commits [`super::landing_candidate`] writes (a branch
    /// head with the large-file trailer, a merge with a main that moved) are
    /// by [`super::LANDING_IDENTITY`], as author and as committer. Run by
    /// [`landing_commits_ignore_the_hosts_git_configuration`] under a home
    /// without or with a hostile git configuration; alone it checks the same
    /// under the caller's.
    #[test]
    #[ignore = "run by landing_commits_ignore_the_hosts_git_configuration"]
    fn landing_commits_are_by_the_integrator() {
        let git = crate::sources::git;
        let base = std::env::temp_dir().join(format!(
            "spawn-land-identity-{}-{}",
            std::process::id(),
            std::env::var("SPAWN_TEST_LANDING_CASE").unwrap_or_default()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).expect("seed directory");
        let seed_path = seed.to_string_lossy().to_string();
        git(&["init", "-q", "--initial-branch=main", &seed_path]).expect("init");
        std::fs::write(seed.join("README.md"), "base\n").expect("write");
        commit_in(&seed_path, "base");
        let main = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("main");
        // A branch head on main that names a large-file manifest.
        git(&["-C", &seed_path, "checkout", "-qb", "agent_large"]).expect("branch");
        std::fs::write(seed.join("work.txt"), "work\n").expect("write");
        let trailer = format!("work\n\nSpawn-Large: {}", "a".repeat(64));
        commit_in(&seed_path, &trailer);
        let large = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("large");
        // A branch beside a main that moved on.
        git(&["-C", &seed_path, "checkout", "-qb", "agent_late", &main]).expect("branch");
        std::fs::write(seed.join("late.txt"), "late\n").expect("write");
        commit_in(&seed_path, "late");
        let late = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("late");
        git(&["-C", &seed_path, "checkout", "-q", "main"]).expect("main");
        std::fs::write(seed.join("other.txt"), "theirs\n").expect("write");
        commit_in(&seed_path, "elsewhere");
        let moved = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("moved");
        let mirror = format!("{seed_path}/.git");

        for (main, branch) in [(main.as_str(), large.as_str()), (&moved, &late)] {
            let landing = super::landing_candidate(&mirror, main, branch, 7)
                .expect("a landing")
                .expect("no conflict");
            assert_ne!(landing, branch, "a commit of the integrator's own");
            let who = git(&[
                "--git-dir",
                &mirror,
                "show",
                "-s",
                "--format=%an <%ae>%n%cn <%ce>",
                &landing,
            ])
            .expect("show");
            let expected = format!(
                "{0} <{1}>",
                super::LANDING_IDENTITY.0,
                super::LANDING_IDENTITY.1
            );
            assert_eq!(who, format!("{expected}\n{expected}"));
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn landing_commits_ignore_the_hosts_git_configuration() {
        let base = std::env::temp_dir().join(format!("spawn-land-homes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let empty = base.join("empty");
        std::fs::create_dir_all(&empty).expect("an empty home");
        // Another identity, no identity guessed, and every commit signed by
        // a gpg that does not exist.
        let hostile = base.join("hostile");
        std::fs::create_dir_all(&hostile).expect("a hostile home");
        std::fs::write(
            hostile.join(".gitconfig"),
            "[user]\n\tname = Someone Else\n\temail = else@example.com\n\tuseConfigOnly = true\n\
             [commit]\n\tgpgSign = true\n[gpg]\n\tprogram = /nonexistent/gpg\n",
        )
        .expect("write the hostile configuration");
        let program = std::env::current_exe().expect("the test binary");
        for (case, home) in [("empty", &empty), ("hostile", &hostile)] {
            let output = std::process::Command::new(&program)
                .args([
                    "--exact",
                    "integrate::tests::landing_commits_are_by_the_integrator",
                    "--ignored",
                    "--nocapture",
                ])
                .env("HOME", home)
                .env("XDG_CONFIG_HOME", home)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("SPAWN_TEST_LANDING_CASE", case)
                .env_remove("GIT_CONFIG_GLOBAL")
                .env_remove("GIT_AUTHOR_NAME")
                .env_remove("GIT_AUTHOR_EMAIL")
                .env_remove("GIT_COMMITTER_NAME")
                .env_remove("GIT_COMMITTER_EMAIL")
                .env_remove("EMAIL")
                .output()
                .expect("run the test binary");
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "{case} home: {stdout}{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}

/// The integrator's passes against the test database
/// (`SPAWN_TEST_DATABASE_URL`; like the spawn-postgres tests, they do not
/// skip), each in a schema of its own, dropped afterwards, with real git
/// sources and a stub gate.
#[cfg(test)]
mod database_tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use serde_json::{Value, json};
    use spawn_postgres::PostgresStore;

    use super::Integrator;
    use crate::sources::git;

    struct Db {
        store: Arc<PostgresStore>,
        url: String,
        schema: String,
    }

    impl Db {
        fn new(case: &str) -> Self {
            let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
                "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
            );
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.subsec_nanos());
            let schema = format!("spawn_test_{}_{nanos:x}_{case}", std::process::id());
            let store = PostgresStore::connect_in_schema(&url, &schema).expect("connect");
            Self {
                store: Arc::new(store),
                url,
                schema,
            }
        }

        /// An owner connection in the schema.
        fn admin(&self) -> postgres::Client {
            let mut client =
                postgres::Client::connect(&self.url, postgres::NoTls).expect("admin connection");
            client
                .batch_execute(&format!("set search_path to {}", self.schema))
                .expect("schema");
            client
        }

        /// A mission its gate alone accepts, on `source`.
        fn gated_mission(&self, source: &str, scenarios: &Path) -> i64 {
            let mission = self
                .store
                .mission_add("gated", &json!([source]), "human")
                .expect("add");
            self.store
                .mission_configure(
                    mission,
                    Some(&scenarios.to_string_lossy()),
                    "gate",
                    None,
                    None,
                )
                .expect("configure");
            mission
        }

        /// A task of `mission` for `worker`, delivered and reported.
        fn reported_task(&self, mission: i64, worker: &str) -> i64 {
            let task = self
                .store
                .task_assign(mission, "ephemeral", worker, "brief", "default")
                .expect("assign");
            self.store.task_delivered(task, 1).expect("delivered");
            assert!(self.store.task_reported(task, "done").expect("reported"));
            task
        }

        fn record(&self, record: &Value) {
            self.store.integrator_record(record).expect("record");
        }

        fn mission(&self, id: i64) -> Value {
            let board = self.store.mission_board(50).expect("board");
            board["missions"]
                .as_array()
                .expect("missions")
                .iter()
                .find(|mission| mission["id"] == id)
                .cloned()
                .expect("the mission")
        }
    }

    impl Drop for Db {
        fn drop(&mut self) {
            if let Ok(mut client) = postgres::Client::connect(&self.url, postgres::NoTls) {
                let _ =
                    client.batch_execute(&format!("drop schema if exists {} cascade", self.schema));
            }
        }
    }

    /// A bare source with one commit on main, under a fresh directory, and
    /// an integrator whose workspace root is beside it: `(base, source,
    /// scenarios, integrator)`.
    fn world(tag: &str, gate: &str) -> (PathBuf, String, PathBuf, Integrator) {
        let base = std::env::temp_dir().join(format!("spawn-db-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).expect("seed");
        let seed_path = seed.to_string_lossy().to_string();
        git(&["init", "-q", "--initial-branch=main", &seed_path]).expect("init");
        std::fs::write(seed.join("README.md"), "base\n").expect("write");
        super::tests::commit_in(&seed_path, "base");
        let source = base.join("source.git").to_string_lossy().to_string();
        git(&["clone", "-q", "--bare", &seed_path, &source]).expect("bare source");
        let scenarios = base.join("scenarios");
        std::fs::create_dir_all(&scenarios).expect("scenarios");
        let gate_path = base.join("gate.sh");
        std::fs::write(&gate_path, gate).expect("gate");
        let integrator = Integrator::new(
            base.join("host"),
            gate_path,
            test_gate_env(),
            super::DEFAULT_GATES,
        );
        (base, source, scenarios, integrator)
    }

    /// Main moves on the source by one commit, pushed from a clone; the
    /// integrator's mirror does not follow. Main's new commit.
    fn main_moves(base: &Path, source: &str, file: &str) -> String {
        let clone = base.join(format!("clone-{file}"));
        let clone_path = clone.to_string_lossy().to_string();
        git(&["clone", "-q", source, &clone_path]).expect("clone");
        std::fs::write(clone.join(file), "moved\n").expect("write");
        super::tests::commit_in(&clone_path, "main moves");
        git(&["-C", &clone_path, "push", "-q", "origin", "main"]).expect("push main");
        git(&["--git-dir", source, "rev-parse", "refs/heads/main"]).expect("main")
    }

    fn source_main(source: &str) -> String {
        git(&["--git-dir", source, "rev-parse", "refs/heads/main"]).expect("main")
    }

    /// The stub gates' environment: PATH only, for `sleep` and `cat`.
    fn test_gate_env() -> Vec<(String, String)> {
        std::env::var("PATH")
            .map(|path| vec![("PATH".to_string(), path)])
            .unwrap_or_default()
    }

    /// `agent`'s work on `source`'s main, pushed as `spawn/<agent>`: one new
    /// file. The pushed commit.
    fn push_work(base: &Path, source: &str, agent: &str, file: &str) -> String {
        let clone = base.join(format!("agent-{file}"));
        let clone_path = clone.to_string_lossy().to_string();
        git(&["clone", "-q", source, &clone_path]).expect("clone");
        std::fs::write(clone.join(file), "work\n").expect("write");
        super::tests::commit_in(&clone_path, "work");
        git(&[
            "-C",
            &clone_path,
            "push",
            "-q",
            "origin",
            &format!("HEAD:refs/heads/spawn/{agent}"),
        ])
        .expect("push the agent's branch");
        git(&["-C", &clone_path, "rev-parse", "HEAD"]).expect("work")
    }

    /// A gate that notes the candidate it judges in `<marks>/started`, then
    /// holds until `<marks>/release` exists, and accepts.
    fn holding_gate(marks: &Path) -> String {
        let marks = marks.to_string_lossy();
        format!(
            "echo \"$3\" >>'{marks}/started'\n\
             while [ ! -e '{marks}/release' ]; do sleep 0.05; done\n\
             printf '{{\"verdict\":\"accepted\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n"
        )
    }

    fn started(marks: &Path) -> Vec<String> {
        std::fs::read_to_string(marks.join("started"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Wait until `count` gates started (a gate starts on its own thread).
    fn wait_started(marks: &Path, count: usize) -> Vec<String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let started = started(marks);
            if started.len() >= count || std::time::Instant::now() > deadline {
                return started;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    fn agent_of(db: &Db, mission: i64, task: i64) -> String {
        db.mission(mission)["tasks"]
            .as_array()
            .expect("tasks")
            .iter()
            .find(|row| row["id"] == task)
            .and_then(|row| row["agent_id"].as_str())
            .expect("the task's agent")
            .to_string()
    }

    /// Gates run per mission, concurrently, under the cap: mission A has two
    /// ready tasks, B and C one each, all on sources of their own, and the
    /// cap is 2. The first pass starts A's first task and B's; A's second
    /// waits for A's first (a mission's tasks stay serial), and C waits for
    /// a free gate, through any number of passes. Once both land, the next
    /// pass starts A's second task, judged against the main A's first left,
    /// and C's.
    #[test]
    fn gates_run_per_mission_concurrently_up_to_the_cap() {
        let marks =
            std::env::temp_dir().join(format!("spawn-db-gate-marks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&marks);
        std::fs::create_dir_all(&marks).expect("marks");
        let gate = holding_gate(&marks);
        let (base_a, source_a, scenarios, integrator) = world("concurrent-a", &gate);
        let (base_b, source_b, _, _) = world("concurrent-b", "exit 1\n");
        let (base_c, source_c, _, _) = world("concurrent-c", "exit 1\n");
        let db = Db::new("concurrent_gates");
        let a = db.gated_mission(&source_a, &scenarios);
        let b = db.gated_mission(&source_b, &scenarios);
        let c = db.gated_mission(&source_c, &scenarios);
        let a_one = db.reported_task(a, "a1");
        let a_two = db.reported_task(a, "a2");
        let b_task = db.reported_task(b, "b1");
        let c_task = db.reported_task(c, "c1");
        let pushed_a_one = push_work(&base_a, &source_a, &agent_of(&db, a, a_one), "a1.txt");
        let pushed_a_two = push_work(&base_a, &source_a, &agent_of(&db, a, a_two), "a2.txt");
        let pushed_b = push_work(&base_b, &source_b, &agent_of(&db, b, b_task), "b1.txt");
        let pushed_c = push_work(&base_c, &source_c, &agent_of(&db, c, c_task), "c1.txt");

        integrator.pass_on(&db.store).expect("the first pass");
        let first = wait_started(&marks, 2);
        assert_eq!(
            first,
            vec![pushed_a_one.clone(), pushed_b.clone()],
            "A's first and B's"
        );
        // More passes while both hold: nothing else starts.
        for _ in 0..3 {
            integrator
                .pass_on(&db.store)
                .expect("a pass while two gates run");
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert_eq!(
            started(&marks).len(),
            2,
            "the cap holds: {:?}",
            started(&marks)
        );
        assert_eq!(db.mission(c)["tasks"][0]["state"], "reported");

        std::fs::write(marks.join("release"), "").expect("release");
        integrator.settle();
        assert_eq!(source_main(&source_a), pushed_a_one, "A's first landed");
        assert_eq!(source_main(&source_b), pushed_b, "B's landed");
        std::fs::remove_file(marks.join("release")).expect("hold again");

        integrator.reap();
        integrator.pass_on(&db.store).expect("the next pass");
        let next = wait_started(&marks, 4);
        assert_eq!(next.len(), 4, "A's second and C's: {next:?}");
        assert_eq!(next[3], pushed_c, "C's after A's second: {next:?}");
        assert_ne!(
            next[2], pushed_a_two,
            "A's second is merged with the main A's first left"
        );
        std::fs::write(marks.join("release"), "").expect("release");
        integrator.settle();
        let after = db.mission(a);
        let states: Vec<&Value> = after["tasks"]
            .as_array()
            .expect("tasks")
            .iter()
            .map(|row| &row["state"])
            .collect();
        assert!(states.iter().all(|state| *state == "landed"), "{after}");
        assert_eq!(db.mission(c)["tasks"][0]["state"], "landed");
        for base in [base_a, base_b, base_c, marks] {
            let _ = std::fs::remove_dir_all(&base);
        }
    }

    /// A cap of one is one gate at a time across missions, as before.
    #[test]
    fn a_cap_of_one_gates_one_mission_at_a_time() {
        let marks = std::env::temp_dir().join(format!("spawn-db-gate-cap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&marks);
        std::fs::create_dir_all(&marks).expect("marks");
        let (base_a, source_a, scenarios, mut integrator) =
            world("cap-one-a", &holding_gate(&marks));
        integrator.gates = 1;
        let (base_b, source_b, _, _) = world("cap-one-b", "exit 1\n");
        let db = Db::new("cap_one");
        let a = db.gated_mission(&source_a, &scenarios);
        let b = db.gated_mission(&source_b, &scenarios);
        let a_one = db.reported_task(a, "a1");
        let b_task = db.reported_task(b, "b1");
        let pushed_a_one = push_work(&base_a, &source_a, &agent_of(&db, a, a_one), "a1.txt");
        let pushed_b = push_work(&base_b, &source_b, &agent_of(&db, b, b_task), "b1.txt");

        integrator.pass_on(&db.store).expect("a pass");
        integrator.pass_on(&db.store).expect("another pass");
        assert_eq!(wait_started(&marks, 1), vec![pushed_a_one.clone()]);
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert_eq!(
            started(&marks),
            vec![pushed_a_one],
            "B waits for the one gate"
        );
        std::fs::write(marks.join("release"), "").expect("release");
        integrator.settle();
        integrator.pass_on(&db.store).expect("the pass after");
        integrator.settle();
        assert_eq!(started(&marks).last(), Some(&pushed_b));
        assert_eq!(db.mission(b)["tasks"][0]["state"], "landed");
        for base in [base_a, base_b, marks] {
            let _ = std::fs::remove_dir_all(&base);
        }
    }

    /// Audit #17: a landing's push moved main, and the integrator stopped
    /// before it recorded `task.landed`. A lower-id mission on the same
    /// source runs first in the next pass: it must not read the moved main
    /// as an unexplained move (a halt) while the landing is unresolved. The
    /// landing then resolves (main holds its candidate), and its record
    /// moves the waiting mission's baseline with it.
    #[test]
    fn an_unrecorded_landing_holds_the_other_missions_on_its_source() {
        let (base, source, scenarios, integrator) = world("unrecorded-landing", "exit 1\n");
        let db = Db::new("unrecorded_landing");
        let b0 = source_main(&source);
        let waiting = db.gated_mission(&source, &scenarios);
        let landing = db.gated_mission(&source, &scenarios);
        let waiting_task = db.reported_task(waiting, "w");
        let landing_task = db.reported_task(landing, "l");
        db.record(
            &json!({"type": "task.gated", "id": waiting_task, "revision": 0,
                          "verdict": "aborted", "base": b0, "reason": "a gate that died"}),
        );
        let c1 = main_moves(&base, &source, "landed.txt");
        db.record(
            &json!({"type": "task.gated", "id": landing_task, "revision": 0,
                          "verdict": "accepted", "base": b0, "candidate": c1}),
        );
        // The mirror exists, as it did when the push went out.
        crate::sources::ensure_mirror(&integrator.judge.root, Some(&source)).expect("mirror");

        integrator.pass_on(&db.store).expect("a pass");
        let waited = db.mission(waiting);
        assert!(
            waited["halted"].is_null(),
            "the waiting mission halted: {}",
            waited["halted"]
        );
        let landed = db.mission(landing);
        assert_eq!(landed["tasks"][0]["state"], "landed");
        assert_eq!(landed["baseline"], c1.as_str());
        assert_eq!(
            db.mission(waiting)["baseline"],
            c1.as_str(),
            "the landing names the mission that waited at its from"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Audit #17, the other half: main moved past the landing's candidate
    /// (another landing went on top of it before the record), so the
    /// compare-and-swap push fails and main is not the candidate. The
    /// candidate is in main's history: it landed, and is recorded so,
    /// instead of judging landed work again.
    #[test]
    fn a_landing_already_under_main_is_recorded_landed() {
        let (base, source, scenarios, integrator) = world("landing-under-main", "exit 1\n");
        let db = Db::new("landing_under_main");
        let b0 = source_main(&source);
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "l");
        let c1 = main_moves(&base, &source, "landed.txt");
        let c2 = main_moves(&base, &source, "on-top.txt");
        db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                          "verdict": "accepted", "base": b0, "candidate": c1}));
        crate::sources::ensure_mirror(&integrator.judge.root, Some(&source)).expect("mirror");

        integrator.pass_on(&db.store).expect("a pass");
        let after = db.mission(mission);
        assert_eq!(after["tasks"][0]["state"], "landed", "{after}");
        assert_eq!(after["baseline"], c1.as_str());
        assert_eq!(source_main(&source), c2, "main is left where it is");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Audit #13: the owner moved main F -> T and recorded it (the baseline
    /// is T), but the integrator's fetch fails, so its mirror still says F.
    /// That is no reading of the source's main: the mission is skipped this
    /// pass, not halted.
    #[test]
    fn a_failed_fetch_halts_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let (base, source, scenarios, integrator) = world("failed-fetch", "exit 1\n");
        let db = Db::new("failed_fetch");
        crate::sources::ensure_mirror(&integrator.judge.root, Some(&source)).expect("mirror at F");
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        let t = main_moves(&base, &source, "deploy.txt");
        db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                          "verdict": "aborted", "base": t, "reason": "a gate that died"}));
        let permissions = |mode| {
            std::fs::set_permissions(&source, std::fs::Permissions::from_mode(mode))
                .expect("permissions");
        };
        permissions(0o000);
        let pass = integrator.pass_on(&db.store);
        permissions(0o755);
        pass.expect("a pass");
        let after = db.mission(mission);
        assert!(after["halted"].is_null(), "halted: {}", after["halted"]);
        assert_eq!(after["baseline"], t.as_str());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Audit #14: only a directory on this host receives agents' branches.
    /// A gated mission on a remote source (configured before the gate was
    /// refused on one) would be gated `stale` on every pass, forever: it is
    /// halted with the reason instead.
    #[test]
    fn a_gated_mission_on_a_remote_source_is_halted() {
        let db = Db::new("remote_source");
        let source = "https://example.invalid/widget.git";
        let mission = db
            .store
            .mission_add("remote", &json!([source]), "human")
            .expect("add");
        db.admin()
            .execute(
                "select spawn_control_record($1)",
                &[&json!({"type": "mission.configured", "id": mission,
                          "gate": "/controller/rsi/scenarios", "accept": "gate"})],
            )
            .expect("a gate configured before the check");
        db.reported_task(mission, "w");
        let base = std::env::temp_dir().join(format!("spawn-db-remote-{}", std::process::id()));
        let integrator = Integrator::new(
            base.join("host"),
            base.join("gate.sh"),
            Vec::new(),
            super::DEFAULT_GATES,
        );
        integrator.pass_on(&db.store).expect("a pass");
        let halted = db.mission(mission)["halted"].clone();
        assert!(
            halted
                .as_str()
                .is_some_and(|reason| reason.contains("never lands")),
            "halted: {halted}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Audit #10: the integrator's verdict names the revision it judged. A
    /// task sent back once by the gate is reported again (revision 1); the
    /// stub gate accepts, and the verdict for revision 1 lands it.
    #[test]
    fn a_verdict_names_the_revision_it_judged_and_lands() {
        let gate = "printf '{\"verdict\":\"accepted\"}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
                    echo '{\"verdict\":\"accepted\"}'\n";
        let (base, source, scenarios, integrator) = world("verdict-revision", gate);
        let db = Db::new("verdict_revision");
        let b0 = source_main(&source);
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        let agent_id = db.mission(mission)["tasks"][0]["agent_id"]
            .as_str()
            .expect("the task's agent")
            .to_string();
        db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                          "verdict": "rejected", "base": b0, "note": "again"}));
        db.store.task_delivered(task, 2).expect("delivered again");
        assert!(
            db.store
                .task_reported(task, "again")
                .expect("reported again")
        );
        // The agent's work, pushed as spawn/<agent> (no checkpoint recorded,
        // so the push is what lands).
        let clone = base.join("agent");
        let clone_path = clone.to_string_lossy().to_string();
        git(&["clone", "-q", &source, &clone_path]).expect("clone");
        std::fs::write(clone.join("work.txt"), "work\n").expect("write");
        super::tests::commit_in(&clone_path, "work");
        git(&[
            "-C",
            &clone_path,
            "push",
            "-q",
            "origin",
            &format!("HEAD:refs/heads/spawn/{agent_id}"),
        ])
        .expect("push the agent's branch");
        let work = git(&["-C", &clone_path, "rev-parse", "HEAD"]).expect("work");

        integrator.pass_on(&db.store).expect("a pass");
        integrator.settle();
        let after = db.mission(mission);
        assert_eq!(after["tasks"][0]["state"], "landed", "{after}");
        assert_eq!(source_main(&source), work);
        let revisions: Vec<Option<String>> = db
            .admin()
            .query(
                "select kind -> 'detail' -> 'record' ->> 'revision' from spawn_events
                  where session_id = 'session_control'
                    and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
                  order by session_seq",
                &[],
            )
            .expect("verdicts")
            .iter()
            .map(|row| row.get(0))
            .collect();
        assert_eq!(
            revisions,
            vec![Some("0".to_string()), Some("1".to_string())]
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
