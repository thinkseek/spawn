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
//!    written after the last checkpoint never lands.
//!
//!    **A landing is one commit per task on main's first-parent line.**
//!    The candidate is a new merge commit C ([`landing_candidate`]), never
//!    the worker's commit and never a fast-forward to one: first parent the
//!    source's main (the task's base), second parent the pushed head
//!    judged fresh; its tree is the head's when main is in it, and the merge
//!    by main's merge rules when main moved (a conflict is `conflict`). Its
//!    message ([`landing_message`]) says what landed, from the record alone:
//!    the objective's title with the objective and the task on the first
//!    line (a task without one: its brief's first line and the task), the
//!    report's `## What changed` (or the whole report, bounded) after a
//!    blank line, with no line of it able to pass for a `Spawn-` trailer,
//!    and the integrator's `Spawn-Task`, `Spawn-Objective` and `Spawn-Head`
//!    trailers. C is judged by `scripts/rsi/gate.sh` with the mission's
//!    scenarios, named in the accepted `task.gated` (`candidate`), pushed
//!    as it is, and named in `task.landed` (`to`). So `git log
//!    --first-parent` is one line per landed task, `git bisect start
//!    --first-parent` visits only gated landings, `git revert -m 1 <C>`
//!    undoes one task, and every checkpoint stays reachable from main
//!    through the second parents. Every verdict
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
//!
//!    **No room is not a verdict.** A gate that finds TMPDIR short of room
//!    (exit [`gate::NO_ROOM_EXIT`] with its no-room fact,
//!    [`GateFailure::NoRoom`]) is gated `no_room`: the task stays where it
//!    was, the verdict names the free space and TMPDIR, and neither the
//!    abort nor the rejection count moves. The store folds the mission's
//!    run of them (`gate_room_*`); the next try waits [`room_backoff`] after
//!    the latest, and the first `no_room` more than [`ROOM_ESCALATE_AFTER`]
//!    into a run carries `escalate`, which the dispatcher's digest takes to
//!    the supervisor once ([`crate::disk::gate_room_escalations`]). Every
//!    other gate failure, the gate's own exit 3 included (a missing bind
//!    directory, a bad SPAWN_GATE_MIN_FREE_GB), is an abort and counts;
//!
//!    **A report is judged against the checkpoint of the turn that produced
//!    it.** The dispatcher collects a report only after the turn that wrote
//!    it ended in the log, and the Bridge records a turn's checkpoint before
//!    its end; the `task.reported` record names that checkpoint in `commit`.
//!    The pushed head must also be that commit or a descendant of it
//!    ([`report_freshness`]); otherwise the verdict is `stale`, as above. A
//!    report that names a commit is never judged for lack of a checkpoint
//!    (the branch pushed when the workspace was mounted is only the task's
//!    base). A report with a null commit (a record from before reports named
//!    one) is judged by the rule above alone;
//! 5. an objective mission's free slots are filled ([`objectives`]): it keeps
//!    up to its worker limit N (`--workers`, default 1) of its tasks in
//!    flight, each on its own agent (its resident, then per-task workers),
//!    and never two for one objective. Filling happens on every pass that
//!    gets this far, before the judging of step 4, which stays one
//!    candidate per pass, the lowest ready task first. A halted mission gets
//!    no new task.
//!
//! Like the dispatcher, it keeps no state of its own: the control stream
//! says what was judged and landed. An advisory lock allows one integrator
//! per database, so two gates never run at once.

use std::fmt::Write as _;
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

/// The wait before the first retry of a gate that had no room; it doubles
/// with each `no_room` in a row, up to [`ROOM_BACKOFF_MAX`].
const ROOM_BACKOFF: Duration = Duration::from_mins(5);

/// The longest wait between two tries of a gate that had no room.
const ROOM_BACKOFF_MAX: Duration = Duration::from_mins(40);

/// How long a mission's gate may go without room before the supervisor is
/// told, once per run.
const ROOM_ESCALATE_AFTER: Duration = Duration::from_hours(2);

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

const USAGE: &str = "usage: spawn integrate [--once] [--root <dir>]";

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
    /// The mission's current run of `no_room` gate outcomes, folded by the
    /// store: the task it is about (a run is one task's; it ends when that
    /// task leaves the gate), when it began, when the latest was recorded,
    /// how many, and whether one carried `escalate`.
    #[serde(default)]
    pub(crate) gate_room_task: Option<i64>,
    #[serde(default)]
    pub(crate) gate_room_since_ms: Option<i64>,
    #[serde(default)]
    pub(crate) gate_room_last_ms: Option<i64>,
    #[serde(default)]
    pub(crate) gate_room_waits: u32,
    #[serde(default)]
    pub(crate) gate_room_escalated: bool,
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
    /// Its brief (`mission_tasks.brief`): an objective task's first line is
    /// `Objective <id>: <title>`.
    #[serde(default)]
    pub(crate) brief: String,
    /// The report the gate's verdict is for (`mission_tasks.report`), as the
    /// record holds it: the landing's message summarises it.
    #[serde(default)]
    pub(crate) report: Option<String>,
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

/// The wait after the `waits`-th `no_room` in a row: [`ROOM_BACKOFF`],
/// doubled each time, at most [`ROOM_BACKOFF_MAX`]. None before the first.
pub(crate) fn room_backoff(waits: u32) -> Duration {
    if waits == 0 {
        return Duration::ZERO;
    }
    ROOM_BACKOFF
        .saturating_mul(2u32.saturating_pow(waits - 1))
        .min(ROOM_BACKOFF_MAX)
}

/// How long `task`'s gate still waits for room at `now_ms`, read from the
/// store's record of its latest `no_room` (never from memory). None when the
/// gate may run: also when the mission's run is another task's, which never
/// lends its backoff.
pub(crate) fn room_wait(mission: &Mission, task: i64, now_ms: i64) -> Option<Duration> {
    if mission.gate_room_task != Some(task) {
        return None;
    }
    let last = mission.gate_room_last_ms?;
    let backoff = i64::try_from(room_backoff(mission.gate_room_waits).as_millis()).ok()?;
    let left = last.saturating_add(backoff).saturating_sub(now_ms);
    (left > 0).then(|| Duration::from_millis(left.unsigned_abs()))
}

/// The record of a gate that had no room to judge `task`, at `now_ms`: the
/// reason with the free space and TMPDIR, the facts themselves, and
/// `escalate` when the run of them on this task began more than
/// [`ROOM_ESCALATE_AFTER`] ago and none escalated yet. Another task's run
/// gives it no age.
pub(crate) fn no_room(
    mission: &Mission,
    task: i64,
    free_kb: u64,
    min_gb: u64,
    tmpdir: &str,
    log: &str,
    now_ms: i64,
) -> Value {
    let after = i64::try_from(ROOM_ESCALATE_AFTER.as_millis()).unwrap_or(i64::MAX);
    let escalate = mission.gate_room_task == Some(task)
        && !mission.gate_room_escalated
        && mission
            .gate_room_since_ms
            .is_some_and(|since| now_ms.saturating_sub(since) > after);
    #[allow(clippy::cast_precision_loss)]
    let free_gib = free_kb as f64 / 1_048_576.0;
    json!({
        "reason": format!(
            "no room to judge now: {free_gib:.1} GiB free in {tmpdir} (TMPDIR), under \
             SPAWN_GATE_MIN_FREE_GB={min_gb}; not a verdict, judged again later"
        ),
        "free_kb": free_kb,
        "min_gb": min_gb,
        "tmpdir": tmpdir,
        "log": log,
        "escalate": escalate,
    })
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

/// The stale rule for a reported task: the pushed head must be the
/// report's commit (`reported`, the agent's checkpoint the report's turn
/// ended with) or a descendant of it, and pass [`freshness`] against the
/// agent's latest checkpoint as well. A report that names a commit is never
/// judged for lack of a checkpoint: with no pushed branch, or one that lacks
/// the commit, it is stale. A report that names none (a record from before
/// reports named their checkpoint) is judged by [`freshness`] alone.
pub(crate) fn report_freshness(
    reported: Option<&str>,
    checkpoint: Option<&str>,
    pushed: Option<&str>,
    contains: impl Fn(&str, &str) -> bool,
) -> Freshness {
    if let Some(reported) = reported {
        match pushed {
            None => {
                return Freshness::Stale(format!(
                    "the source has no spawn/<agent> yet, and the report's checkpoint is \
                     {reported}"
                ));
            }
            Some(pushed) if pushed != reported && !contains(reported, pushed) => {
                return Freshness::Stale(if contains(pushed, reported) {
                    format!(
                        "the pushed branch ({pushed}) is behind the report's checkpoint \
                         ({reported})"
                    )
                } else {
                    format!(
                        "the pushed branch ({pushed}) does not contain the report's checkpoint \
                         ({reported})"
                    )
                });
            }
            Some(_) => {}
        }
    }
    freshness(checkpoint, pushed, contains)
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
    let integrator = Integrator {
        root,
        gate,
        gate_env,
    };
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

struct Integrator {
    root: PathBuf,
    gate: PathBuf,
    /// The gate's whole environment ([`gate::gate_env`]).
    gate_env: Vec<(String, String)>,
}

impl Integrator {
    fn pass(&self) -> Result<(), String> {
        // A store per pass: the advisory lock belongs to the connection, and
        // is released with it.
        let store = Arc::new(open_store()?);
        if !store
            .integrator_try_lock()
            .map_err(|error| error.to_string())?
        {
            return Ok(());
        }
        self.pass_on(&store)
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
        if mission.halted.is_some() {
            return Ok(false);
        }
        let scenarios = pinned(Path::new(&mission.gate));
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
        let mirror =
            sources::ensure_mirror(&self.root, Some(source)).map_err(|error| error.to_string())?;
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
        // The gate had no room last time: wait out the backoff, then try.
        if let Some(wait) = room_wait(mission, task.id, now_ms()) {
            println!(
                "integrate: mission {}: the gate had no room; task {} is judged again in {}s",
                mission.id,
                task.id,
                wait.as_secs()
            );
            return Ok(assigned);
        }
        let judged = self.judge(store, &mirror, source, &main, mission, count, task)?;
        Ok(judged || assigned)
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

    /// The agent's pushed branch, fetched into the mirror, checked against
    /// the checkpoint the task's report names and the agent's latest
    /// checkpoint ([`report_freshness`]). A lagging push is fetched again for
    /// up to [`PUSH_WAIT`] before it counts as stale.
    fn pushed_candidate(
        store: &PostgresStore,
        mirror: &Path,
        source: &str,
        task: &Task,
    ) -> Result<Freshness, String> {
        let agent = task.agent_id.as_str();
        let checkpoint = store
            .latest_checkpoint(agent)
            .map_err(|error| error.to_string())?;
        let reported = store
            .reported_commit(task.id)
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
            let verdict = report_freshness(
                reported.as_deref(),
                checkpoint.as_deref(),
                pushed.as_deref(),
                contains,
            );
            if !matches!(verdict, Freshness::Stale(_)) || std::time::Instant::now() >= deadline {
                return Ok(verdict);
            }
            std::thread::sleep(PUSH_POLL);
        }
    }

    /// Judge one task from its pushed branch. True when what it recorded may
    /// move another mission's view; a stale push moves nothing, so the other
    /// missions still get their turn in this pass.
    #[allow(clippy::too_many_arguments)]
    fn judge(
        &self,
        store: &Arc<PostgresStore>,
        mirror: &Path,
        source: &str,
        main: &str,
        mission: &Mission,
        count: u32,
        task: &Task,
    ) -> Result<bool, String> {
        let branch = match Self::pushed_candidate(store, mirror, source, task)? {
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
            mission,
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
        mission: &Mission,
        count: u32,
        task: &Task,
        branch: Option<&str>,
    ) -> Result<(), String> {
        let scenarios = Path::new(&mission.gate);
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
        let message = landing_message(task, &branch);
        let Some(candidate) = landing_candidate(&mirror_path, main, &branch, &message)? else {
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

        let scenario = task_scenario(scenarios, task.scenario.as_deref());
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
            &pinned(&self.gate),
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
            // No room in TMPDIR: a fact about the host, never a count.
            Err(GateFailure::NoRoom {
                free_kb,
                min_gb,
                tmpdir,
                log,
            }) => gated(
                "no_room",
                no_room(mission, task.id, free_kb, min_gb, &tmpdir, &log, now_ms()),
            ),
            Err(failure) => gated("aborted", aborted(&failure)),
        }
    }
}

/// The commit that lands `branch` (the agent's pushed head, in the mirror)
/// on `main`, with `message` ([`landing_message`]), or `None` when the two
/// conflict. Always one new merge commit: first parent `main` (the base the
/// gate judges against), second parent `branch`, so main's first-parent line
/// is one commit per landing and every checkpoint stays reachable through
/// the second parents. Its tree is the branch's when main is in it, and the
/// merge of both by main's merge rules otherwise. Never the branch head
/// itself, so a head that names a large-file manifest lands without it.
/// Only objects are written, never a ref: the agent's `refs/heads/<agent>`
/// stays the Bridge's, and `spawn/<agent>` catches up with a landing through
/// the Bridge's next merge of main, which its pusher pushes like any
/// checkpoint.
fn landing_candidate(
    mirror_path: &str,
    main: &str,
    branch: &str,
    message: &str,
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
    let tree = if main_is_in {
        git(&[
            "--git-dir",
            mirror_path,
            "rev-parse",
            &format!("{branch}^{{tree}}"),
        ])?
    } else {
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
        tree
    };
    commit_landing(
        mirror_path,
        &[&tree, "-p", main, "-p", branch, "-m", message],
    )
    .map(Some)
}

/// Characters of a report's summary a landing's message holds at most.
const SUMMARY_CHARS: usize = 3000;

/// Characters of a brief's first line a landing's first line holds at most.
const TITLE_CHARS: usize = 100;

/// The message of `task`'s landing of `head`: what landed, from the record
/// alone (never a workspace). The first line holds the objective's title
/// (the brief's first line, `Objective <id>: <title>`) and names the
/// objective and the task, or, for a task without an objective, the brief's
/// first non-empty line and the task. After a blank line, the report's
/// `## What changed` section (the whole report when it has none), bounded,
/// with every line that could read as a `Spawn-` trailer indented. Last, the
/// integrator's own trailers.
pub(crate) fn landing_message(task: &Task, head: &str) -> String {
    let first = task
        .brief
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let mut subject = match task.objective {
        Some(objective) => {
            let title = first
                .strip_prefix(&format!("Objective {objective}:"))
                .map_or(first, str::trim);
            let title = cut(title, TITLE_CHARS);
            if title.is_empty() {
                format!("Land objective {objective}, task {}", task.id)
            } else {
                format!("{title} (objective {objective}, task {})", task.id)
            }
        }
        None if first.is_empty() => format!("Land task {}", task.id),
        None => format!("{} (task {})", cut(first, TITLE_CHARS), task.id),
    };
    if subject.to_ascii_lowercase().starts_with("spawn checkpoint") {
        subject = format!("Land {subject}");
    }
    let mut message = subject;
    message.push_str("\n\n");
    let summary = report_summary(task.report.as_deref().unwrap_or_default());
    if summary.is_empty() {
        message.push_str("The task's report is empty.");
    } else {
        for line in summary.lines() {
            if line.trim_start().to_ascii_lowercase().starts_with("spawn-") {
                message.push_str("    ");
            }
            message.push_str(line);
            message.push('\n');
        }
    }
    let message = message.trim_end().to_string();
    let mut trailers = format!("Spawn-Task: {}\n", task.id);
    if let Some(objective) = task.objective {
        let _ = writeln!(trailers, "Spawn-Objective: {objective}");
    }
    let _ = writeln!(trailers, "Spawn-Head: {head}");
    format!("{message}\n\n{trailers}")
}

/// What a landing's message quotes of `report`: its `## What changed`
/// section, or the whole report when it has none (or an empty one),
/// trimmed and cut to [`SUMMARY_CHARS`].
fn report_summary(report: &str) -> String {
    fn heading(line: &str) -> Option<&str> {
        let level = line.bytes().take_while(|&byte| byte == b'#').count();
        (1..=2)
            .contains(&level)
            .then(|| line[level..].trim().trim_end_matches(':').trim())
    }
    let mut lines = report.lines();
    let mut section = None;
    if lines.any(|line| heading(line).is_some_and(|text| text.eq_ignore_ascii_case("what changed")))
    {
        let body: Vec<&str> = lines.take_while(|line| heading(line).is_none()).collect();
        let body = body.join("\n");
        if !body.trim().is_empty() {
            section = Some(body);
        }
    }
    let text = section.as_deref().unwrap_or(report).trim();
    let summary = cut(text, SUMMARY_CHARS);
    if summary.len() < text.len() {
        format!("{summary}\n[the report continues]")
    } else {
        summary.to_string()
    }
}

/// `text` cut to at most `limit` characters, on a character boundary.
fn cut(text: &str, limit: usize) -> &str {
    text.char_indices()
        .nth(limit)
        .map_or(text, |(at, _)| text[..at].trim_end())
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
        // Recorded as `no_room` by the caller; counted here only if a
        // caller forgets that.
        GateFailure::NoRoom { tmpdir, log, .. } => {
            json!({"reason": format!("the gate had no room in {tmpdir}"), "log": log})
        }
    }
}

fn now_ms() -> i64 {
    spawn_core::time::Clock::now(&spawn_core::time::SystemClock).0
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

/// A path through the controller's symlink (`controllers/<name>` in the
/// live directory, switched by deploy/controller.sh), resolved once for a
/// step: what counts the scenarios, the gate and its scenarios are then one
/// deployed commit's, even if a switch happens while the gate runs (the
/// previous controller stays until nothing uses it). A path that does not
/// resolve stays as named; the step that reads it reports it.
fn pinned(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The scenario that judges an objective task: its file (`<objective>.sh`,
/// named when the task was assigned) in the mission's scenarios directory as
/// it is now. Not the recorded path: a mission moved to another directory (the
/// owner's checkout to the deployed controller) must not keep judging its
/// tasks in flight with a scenario from the old one, which nothing deploys.
fn task_scenario(scenarios: &Path, recorded: Option<&str>) -> Option<PathBuf> {
    let name = Path::new(recorded?).file_name()?;
    Some(scenarios.join(name))
}

#[cfg(test)]
mod tests {
    use super::{
        Freshness, Mission, aborted, freshness, halt_reason, markers_note, merged_tree, no_room,
        over_budget, pinned, report_freshness, room_backoff, room_wait, task_scenario,
    };
    use crate::gate::GateFailure;
    use serde_json::json;

    /// Only exit 1 is a conflict: a merge-tree git cannot run (an unknown
    /// option, a crash, a signal) is an error carrying git's message.
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

    #[test]
    fn a_task_scenario_is_read_from_the_mission_s_scenarios_as_they_are_now() {
        let now = std::path::Path::new("/live/controllers/spawn-abc/rsi/scenarios");
        assert_eq!(
            task_scenario(now, Some("/home/owner/projects/Spawn/rsi/scenarios/007.sh")),
            Some(now.join("007.sh"))
        );
        assert_eq!(task_scenario(now, None), None);
        assert_eq!(task_scenario(now, Some("/")), None);
    }

    #[test]
    fn pinned_resolves_the_controller_symlink_once() {
        let base = std::env::temp_dir().join(format!("spawn-pinned-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("spawn-one/rsi/scenarios")).expect("one");
        std::fs::create_dir_all(base.join("spawn-two/rsi/scenarios")).expect("two");
        std::os::unix::fs::symlink("spawn-one", base.join("spawn")).expect("link");
        let through = base.join("spawn/rsi/scenarios");
        let held = pinned(&through);
        assert!(
            held.ends_with("spawn-one/rsi/scenarios"),
            "{}",
            held.display()
        );
        // The switch: what was pinned stays the first controller's.
        std::fs::remove_file(base.join("spawn")).expect("unlink");
        std::os::unix::fs::symlink("spawn-two", base.join("spawn")).expect("relink");
        assert!(held.ends_with("spawn-one/rsi/scenarios"));
        assert!(pinned(&through).ends_with("spawn-two/rsi/scenarios"));
        // A path that does not resolve stays as named.
        let missing = base.join("missing/scenarios");
        assert_eq!(pinned(&missing), missing);
        std::fs::remove_dir_all(&base).expect("clean");
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
        let message = format!("Work (task {task})\n\nThe work.\n");
        let candidate = super::landing_candidate(&mirror_path, &main, &pushed, &message)
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
        super::landing_candidate(&mirror_path, &main, &pushed, "Work (task 1)\n\nThe work.\n")
            .expect("candidate")
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

        // Judged again: it contains main, so it lands as a commit of its
        // tree on top of main, with the resolution as its second parent.
        let candidate = judged(&mirror, &source, agent).expect("no conflict now");
        assert_ne!(candidate, resolved.as_str(), "never the worker's commit");
        let rev = |reference: String| {
            git(&["--git-dir", &mirror_path, "rev-parse", &reference]).expect("rev-parse")
        };
        assert_eq!(
            rev(format!("{candidate}^1")),
            main,
            "main is the first parent"
        );
        assert_eq!(rev(format!("{candidate}^2")), resolved.as_str());
        assert_eq!(
            rev(format!("{candidate}^{{tree}}")),
            rev(format!("{resolved}^{{tree}}"))
        );
        git(&[
            "--git-dir",
            &mirror_path,
            "push",
            "-q",
            &source,
            &format!("{candidate}:refs/heads/main"),
        ])
        .expect("lands with one commit on top of main");
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
    fn a_report_is_judged_against_its_own_checkpoint() {
        assert_eq!(
            report_freshness(Some("c2"), Some("c2"), Some("c2"), history),
            Freshness::Fresh("c2".to_string()),
            "equal"
        );
        assert_eq!(
            report_freshness(Some("c2"), Some("c2"), Some("c3"), history),
            Freshness::Fresh("c3".to_string()),
            "descendant"
        );
        let behind = report_freshness(Some("c2"), None, Some("c1"), history);
        assert!(
            matches!(&behind, Freshness::Stale(reason)
                if reason.contains("behind the report's checkpoint")),
            "the base pushed at the mount, with no checkpoint recorded: {behind:?}"
        );
        let diverged = report_freshness(Some("c2"), Some("c2"), Some("d2"), history);
        assert!(
            matches!(&diverged, Freshness::Stale(reason)
                if reason.contains("does not contain the report's checkpoint")),
            "{diverged:?}"
        );
        let absent = report_freshness(Some("c2"), None, None, history);
        assert!(
            matches!(&absent, Freshness::Stale(reason) if reason.contains("no spawn/<agent>")),
            "no pushed branch: {absent:?}"
        );
        // Past the report's checkpoint but behind a later one: the latest
        // checkpoint still rules.
        let lagging = report_freshness(Some("c1"), Some("c3"), Some("c2"), history);
        assert!(
            matches!(&lagging, Freshness::Stale(reason) if reason.contains("behind")),
            "{lagging:?}"
        );
        // A report that names no commit is judged as before.
        assert_eq!(
            report_freshness(None, None, Some("c1"), history),
            Freshness::Fresh("c1".to_string())
        );
        assert_eq!(
            report_freshness(None, None, None, history),
            Freshness::Nothing
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

    const MINUTE: i64 = 60_000;

    #[test]
    fn no_room_backs_off_doubling_to_a_cap() {
        let minutes: Vec<u64> = (0..=6)
            .map(|waits| room_backoff(waits).as_secs() / 60)
            .collect();
        assert_eq!(minutes, [0, 5, 10, 20, 40, 40, 40]);
        assert_eq!(room_backoff(u32::MAX).as_secs(), 40 * 60, "no overflow");
    }

    #[test]
    fn the_gate_waits_out_its_backoff_from_the_recorded_no_room() {
        let mut waiting = mission(0, 0, 0);
        assert_eq!(
            room_wait(&waiting, 4, 0),
            None,
            "no run of no_room: judge now"
        );
        waiting.gate_room_task = Some(4);
        waiting.gate_room_since_ms = Some(100 * MINUTE);
        waiting.gate_room_last_ms = Some(110 * MINUTE);
        waiting.gate_room_waits = 2;
        assert_eq!(
            room_wait(&waiting, 4, 112 * MINUTE).map(|wait| wait.as_secs()),
            Some(8 * 60)
        );
        assert_eq!(room_wait(&waiting, 4, 120 * MINUTE), None, "backoff over");
        assert_eq!(
            room_wait(&waiting, 5, 112 * MINUTE),
            None,
            "another task does not inherit the backoff"
        );
    }

    #[test]
    fn no_room_names_the_reason_and_the_free_space_and_escalates_once_after_two_hours() {
        let mut waiting = mission(0, 2, 0);
        let first = no_room(
            &waiting,
            4,
            3 * 1_048_576 + 104_858,
            10,
            "/data/tmp",
            "log",
            0,
        );
        assert_eq!(
            first["reason"],
            "no room to judge now: 3.1 GiB free in /data/tmp (TMPDIR), under \
             SPAWN_GATE_MIN_FREE_GB=10; not a verdict, judged again later"
        );
        assert_eq!(first["free_kb"], 3 * 1_048_576 + 104_858);
        assert_eq!(first["tmpdir"], "/data/tmp");
        assert_eq!(first["min_gb"], 10);
        assert_eq!(first["escalate"], false, "the first of a run");
        waiting.gate_room_task = Some(4);
        waiting.gate_room_since_ms = Some(0);
        let at = |mission: &Mission, minutes: i64| {
            no_room(mission, 4, 1, 10, "/t", "", minutes * MINUTE)["escalate"].clone()
        };
        assert_eq!(
            no_room(&waiting, 5, 1, 10, "/t", "", 500 * MINUTE)["escalate"],
            false,
            "a later task's first no_room starts its own run"
        );
        assert_eq!(
            at(&waiting, 120),
            false,
            "two hours exactly is not more than two"
        );
        assert_eq!(at(&waiting, 121), true);
        waiting.gate_room_escalated = true;
        assert_eq!(at(&waiting, 500), false, "told once per run");
        // And it never counts toward the halt the mission already nears.
        assert_eq!(halt_reason(&waiting, 0), None);
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

    /// A task as `GATED` reads it, with a brief and a report.
    fn task_of(id: i64, objective: Option<u32>, brief: &str, report: Option<&str>) -> super::Task {
        serde_json::from_value(json!({
            "id": id, "agent_id": "agent_a", "session_id": "session_a", "state": "landing",
            "objective": objective, "brief": brief, "report": report,
        }))
        .expect("a task")
    }

    const REPORT: &str = "## Outcome\ndone\n\n## What changed\nsummary: HARBOUR.txt now holds \
                          kestrel\n\n### Details\nOne file.\n\n## Open issues\nnone\n";

    #[test]
    fn an_objective_landing_names_its_title_objective_and_task_and_summarises_the_report() {
        let task = task_of(
            42,
            Some(931),
            "Objective 931: Greet the harbour with a kestrel\n\nWrite the file.\n\
             Earlier gate verdicts on this objective, newest first:\n- task 41: rejected\n",
            Some(REPORT),
        );
        let message = super::landing_message(&task, "abc123");
        let mut lines = message.lines();
        assert_eq!(
            lines.next(),
            Some("Greet the harbour with a kestrel (objective 931, task 42)")
        );
        assert_eq!(lines.next(), Some(""));
        let body: String = lines.collect::<Vec<_>>().join("\n");
        assert!(
            body.starts_with("summary: HARBOUR.txt now holds kestrel\n\n### Details\nOne file."),
            "{body}"
        );
        assert!(
            !body.contains("## Outcome") && !body.contains("Open issues"),
            "{body}"
        );
        assert!(!message.contains("task 41"), "no other task: {message}");
        assert!(
            message.ends_with("\n\nSpawn-Task: 42\nSpawn-Objective: 931\nSpawn-Head: abc123\n"),
            "{message}"
        );
        assert!(!spawn_host::names_files_outside_git(&message));
    }

    #[test]
    fn a_landing_without_an_objective_names_its_brief_and_task() {
        let task = task_of(
            5,
            None,
            "\n\n  Fix the parser's error messages  \nMore.\n",
            Some(REPORT),
        );
        let message = super::landing_message(&task, "abc123");
        assert_eq!(
            message.lines().next(),
            Some("Fix the parser's error messages (task 5)")
        );
        assert!(!message.contains("Spawn-Objective"), "{message}");
        assert!(message.contains("\nsummary: HARBOUR.txt now holds kestrel\n"));
        // A long first line is cut; an empty brief still names the task;
        // nothing reads like a checkpoint.
        let long = task_of(6, None, &"word ".repeat(100), None);
        let first = super::landing_message(&long, "h")
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        assert!(first.len() < 120 && first.ends_with(" (task 6)"), "{first}");
        assert!(
            super::landing_message(&task_of(7, None, "", None), "h").starts_with("Land task 7\n\n")
        );
        let checkpoint = task_of(8, None, "spawn checkpoint exec_3", None);
        assert!(super::landing_message(&checkpoint, "h").starts_with("Land spawn checkpoint"));
    }

    #[test]
    fn a_report_without_what_changed_is_quoted_whole_and_a_long_one_is_bounded() {
        let task = task_of(
            3,
            Some(7),
            "Objective 7: Title",
            Some("\n  I changed the parser.\n\nAll green.\n\n"),
        );
        let message = super::landing_message(&task, "h");
        assert!(
            message.starts_with("Title (objective 7, task 3)\n\nI changed the parser.\n\nAll green.\n\nSpawn-Task: 3\n"),
            "{message}"
        );
        let report = format!(
            "## What changed\n{}\n## Open issues\nnone\n",
            "é long line\n".repeat(2000)
        );
        let long = super::landing_message(
            &task_of(3, Some(7), "Objective 7: Title", Some(&report)),
            "h",
        );
        assert!(
            long.chars().count() < super::SUMMARY_CHARS + 300,
            "{}",
            long.len()
        );
        assert!(long.contains("[the report continues]"));
        assert!(!long.contains("Open issues"));
        let empty = super::landing_message(&task_of(3, Some(7), "Objective 7: Title", None), "h");
        assert!(
            empty.contains("\n\nThe task's report is empty.\n\nSpawn-Task: 3"),
            "{empty}"
        );
    }

    /// A worker's report cannot make main unmountable, nor sign the landing
    /// with trailers it did not earn.
    #[test]
    fn a_report_naming_a_large_file_manifest_leaves_the_message_clean() {
        let manifest = format!("Spawn-Large: {}", "a".repeat(64));
        for report in [
            format!("## What changed\nwork\n\n{manifest}\n"),
            format!("{manifest}\nSpawn-Task: 99\n"),
            manifest.clone(),
        ] {
            let message = super::landing_message(&task_of(4, None, "Brief", Some(&report)), "h");
            assert!(!spawn_host::names_files_outside_git(&message), "{message}");
            assert!(!message.lines().any(|line| line == manifest), "{message}");
            assert!(
                !message.lines().any(|line| line == "Spawn-Task: 99"),
                "{message}"
            );
            assert!(
                message.contains(&format!("    {manifest}")),
                "indented: {message}"
            );
            let trailers = message.rsplit("\n\n").next().unwrap_or_default();
            assert_eq!(trailers, "Spawn-Task: 4\nSpawn-Head: h\n");
        }
    }

    /// Every landing is one merge commit: main first, the pushed head
    /// second, the message given. Its tree is the head's when main is in it
    /// (here a head naming a large-file manifest, which lands with the
    /// landing's own message), and the merge's when main moved.
    #[test]
    fn a_landing_is_one_two_parent_commit_on_main_with_its_message() {
        let git = crate::sources::git;
        let base = std::env::temp_dir().join(format!("spawn-land-shape-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).expect("seed directory");
        let seed_path = seed.to_string_lossy().to_string();
        git(&["init", "-q", "--initial-branch=main", &seed_path]).expect("init");
        std::fs::write(seed.join("README.md"), "base\n").expect("write");
        commit_in(&seed_path, "base");
        let main = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("main");
        git(&["-C", &seed_path, "checkout", "-qb", "agent_a"]).expect("branch");
        std::fs::write(seed.join("one.txt"), "one\n").expect("write");
        commit_in(&seed_path, "spawn checkpoint exec_1");
        let checkpoint = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("checkpoint");
        std::fs::write(seed.join("two.txt"), "two\n").expect("write");
        commit_in(
            &seed_path,
            &format!("spawn checkpoint exec_2\n\nSpawn-Large: {}", "b".repeat(64)),
        );
        let head = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("head");
        git(&["-C", &seed_path, "checkout", "-q", "main"]).expect("main");
        std::fs::write(seed.join("other.txt"), "theirs\n").expect("write");
        commit_in(&seed_path, "elsewhere");
        let moved = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("moved");
        let mirror = format!("{seed_path}/.git");
        let rev = |reference: String| {
            git(&["--git-dir", &mirror, "rev-parse", &reference]).expect("rev-parse")
        };
        let task = task_of(
            9,
            Some(12),
            "Objective 12: Count the gulls",
            Some("## What changed\ngulls\n"),
        );
        let message = super::landing_message(&task, &head);

        for (main, merged) in [(main.as_str(), false), (moved.as_str(), true)] {
            let landing = super::landing_candidate(&mirror, main, &head, &message)
                .expect("a landing")
                .expect("no conflict");
            assert_eq!(rev(format!("{landing}^1")), main, "first parent: the base");
            assert_eq!(rev(format!("{landing}^2")), head, "second parent: the head");
            assert!(
                git(&[
                    "--git-dir",
                    &mirror,
                    "rev-parse",
                    "--verify",
                    "-q",
                    &format!("{landing}^3")
                ])
                .is_err(),
                "two parents"
            );
            let said = git(&["--git-dir", &mirror, "show", "-s", "--format=%B", &landing])
                .expect("message");
            assert_eq!(said, message.trim_end());
            assert!(
                said.starts_with("Count the gulls (objective 12, task 9)\n\ngulls\n"),
                "{said}"
            );
            assert!(
                !spawn_host::names_files_outside_git(&said),
                "the manifest stays off main"
            );
            let files =
                git(&["--git-dir", &mirror, "ls-tree", "--name-only", &landing]).expect("tree");
            if merged {
                assert_eq!(files, "README.md\none.txt\nother.txt\ntwo.txt", "the merge");
            } else {
                assert_eq!(
                    rev(format!("{landing}^{{tree}}")),
                    rev(format!("{head}^{{tree}}"))
                );
            }
            // One commit on the first-parent line; the checkpoints off it.
            let line = git(&[
                "--git-dir",
                &mirror,
                "rev-list",
                "--first-parent",
                &format!("{main}..{landing}"),
            ])
            .expect("first parents");
            assert_eq!(line, landing);
            git(&[
                "--git-dir",
                &mirror,
                "merge-base",
                "--is-ancestor",
                &checkpoint,
                &landing,
            ])
            .expect("the checkpoints stay reachable");
        }
        let _ = std::fs::remove_dir_all(&base);
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
            let message = "Work (task 7)\n\nThe work.\n\nSpawn-Task: 7\n";
            let landing = super::landing_candidate(&mirror, main, branch, message)
                .expect("a landing")
                .expect("no conflict");
            assert_ne!(landing, branch, "a commit of the integrator's own");
            let said = git(&["--git-dir", &mirror, "show", "-s", "--format=%B", &landing])
                .expect("message");
            assert_eq!(said, message.trim_end(), "the integrator's message");
            assert!(!spawn_host::names_files_outside_git(&said));
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
        let integrator = Integrator {
            root: base.join("host"),
            gate: gate_path,
            gate_env: Vec::new(),
        };
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
        crate::sources::ensure_mirror(&integrator.root, Some(&source)).expect("mirror");

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
        crate::sources::ensure_mirror(&integrator.root, Some(&source)).expect("mirror");

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
        crate::sources::ensure_mirror(&integrator.root, Some(&source)).expect("mirror at F");
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
        let integrator = Integrator {
            root: base.join("host"),
            gate: base.join("gate.sh"),
            gate_env: Vec::new(),
        };
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

    fn gated(db: &Db, mission: i64) -> super::Mission {
        let missions: Vec<super::Mission> =
            serde_json::from_value(db.store.gated_missions().expect("gated")).expect("missions");
        missions
            .into_iter()
            .find(|found| found.id == mission)
            .expect("the mission")
    }

    /// Task 40: the fold. `no_room` keeps the task where it was, names the
    /// free space, moves neither the abort nor the rejection count, and
    /// folds the mission's run of them; a gate that judged ends the run, and
    /// a real abort still counts.
    #[test]
    fn no_room_counts_toward_no_halt_and_a_real_abort_still_does() {
        let (base, source, scenarios, _) = world("no-room-fold", "exit 1\n");
        let db = Db::new("no_room_fold");
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        let no_room = |escalate: bool| {
            json!({"type": "task.gated", "id": task, "revision": 0, "verdict": "no_room",
                   "reason": "no room to judge now", "free_kb": 1024, "min_gb": 10,
                   "tmpdir": "/data/tmp", "log": "! gate: short", "escalate": escalate})
        };
        db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                          "verdict": "aborted", "reason": "a gate that died"}));
        for _ in 0..4 {
            db.record(&no_room(false));
        }
        db.record(&no_room(true));
        let after = gated(&db, mission);
        assert_eq!(after.gate_aborts, 1, "no_room moves no abort count");
        assert_eq!(after.gate_rejections, 0);
        assert_eq!(after.gate_room_waits, 5);
        assert!(after.gate_room_escalated);
        assert!(after.gate_room_since_ms.is_some() && after.gate_room_last_ms.is_some());
        assert_eq!(
            after.tasks[0].state,
            spawn_core::mission::TaskState::Reported
        );
        assert!(after.halted.is_none());
        assert_eq!(after.gate_room_task, Some(task));
        let escalations = db.store.gate_room_escalations().expect("escalations");
        assert_eq!(
            escalations.as_array().map(Vec::len),
            Some(1),
            "{escalations}"
        );
        assert_eq!(escalations[0]["mission"], mission);
        assert_eq!(escalations[0]["free_kb"], 1024);
        // Told is a record: a digest that reached the supervisor with the
        // item's tag, however long ago. Until then it stays, whatever its age.
        let tag = format!("host gate-room-{}", escalations[0]["event_id"]);
        let digest = |key: &str, succeeded: bool| {
            db.admin()
                .execute(
                    "insert into spawn_commands (principal, idempotency_key, agent_id, session_id,
                                                 action, state, succeeded, created_at_ms)
                     values ('spawn-dispatch', $1, 'agent_supervisor', 'session_supervisor',
                             jsonb_build_object('type', 'input', 'text', 'digest',
                                                'tags', jsonb_build_array($2::text)),
                             'finished', $3, 0)",
                    &[&key, &tag, &succeeded],
                )
                .expect("a digest");
        };
        digest("dispatch-digest-failed", false);
        assert_eq!(
            db.store
                .gate_room_escalations()
                .expect("escalations")
                .as_array()
                .map(Vec::len),
            Some(1),
            "a digest that failed told nothing"
        );
        digest("dispatch-digest-told", true);
        assert_eq!(
            db.store
                .gate_room_escalations()
                .expect("escalations")
                .as_array()
                .map(Vec::len),
            Some(0),
            "told once, by its record"
        );

        // A no_room that names no free space or TMPDIR is refused.
        assert!(
            db.store
                .integrator_record(&json!({"type": "task.gated", "id": task, "revision": 0,
                                           "verdict": "no_room", "reason": "?"}))
                .is_err()
        );

        // Two more real aborts make three in a row: the halt still fires.
        for _ in 0..2 {
            db.record(&json!({"type": "task.gated", "id": task, "revision": 0,
                              "verdict": "aborted", "reason": "bind dir missing"}));
        }
        let after = gated(&db, mission);
        assert_eq!(after.gate_aborts, 3);
        assert_eq!(after.gate_room_waits, 0, "a gate that ran ends the run");
        assert!(after.gate_room_since_ms.is_none() && !after.gate_room_escalated);
        assert!(super::halt_reason(&after, 0).is_some_and(|reason| reason.contains("aborts")));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Task 40 review: a run of `no_room` is one task's. The task leaving
    /// the gate (cancelled, aborted over budget) ends it, and a later task's
    /// first `no_room` starts a new run: no inherited age, backoff or
    /// escalation.
    #[test]
    fn a_task_leaving_the_gate_ends_its_run_of_no_room() {
        let (base, source, scenarios, _) = world("no-room-leaves", "exit 1\n");
        let db = Db::new("no_room_leaves");
        let mission = db.gated_mission(&source, &scenarios);
        let no_room = |task: i64, escalate: bool| {
            json!({"type": "task.gated", "id": task, "revision": 0, "verdict": "no_room",
                   "reason": "no room", "free_kb": 0, "min_gb": 10, "tmpdir": "/t",
                   "escalate": escalate})
        };
        let first = db.reported_task(mission, "a");
        for escalate in [false, false, true] {
            db.record(&no_room(first, escalate));
        }
        let before = gated(&db, mission);
        assert_eq!(
            (
                before.gate_room_task,
                before.gate_room_waits,
                before.gate_room_escalated
            ),
            (Some(first), 3, true)
        );
        db.store.task_cancel(first).expect("cancel");
        let after = gated(&db, mission);
        assert_eq!(
            (
                after.gate_room_task,
                after.gate_room_waits,
                after.gate_room_escalated
            ),
            (None, 0, false),
            "cancelled: the run ends"
        );
        assert!(after.gate_room_since_ms.is_none() && after.gate_room_last_ms.is_none());

        // A no_room on another task, while the run's task still waits (or
        // left without a record), starts a new run too.
        let second = db.reported_task(mission, "b");
        db.record(&no_room(second, true));
        let third = db.reported_task(mission, "c");
        db.record(&no_room(third, false));
        let after = gated(&db, mission);
        assert_eq!(
            (
                after.gate_room_task,
                after.gate_room_waits,
                after.gate_room_escalated
            ),
            (Some(third), 1, false),
            "another task's no_room starts a new run"
        );
        assert_eq!(after.gate_room_since_ms, after.gate_room_last_ms);
        assert_eq!(super::room_wait(&after, second, i64::MAX), None);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Task 40, end to end: a gate that exits 5 with its no-room fact is
    /// recorded `no_room` with the free space, the task stays reported, and
    /// the next pass waits out the backoff instead of running the gate.
    #[test]
    fn a_gate_without_room_is_recorded_no_room_and_retried_later() {
        let gate = "echo 'gate: 0.0 GiB free' >&2\n\
                    printf '{\"no_room\":true,\"free_kb\":2048,\"min_gb\":10,\"tmpdir\":\"/data/tmp\"}\\n' \
                    >\"$SPAWN_GATE_VERDICT\"\n\
                    echo ran >>\"$(dirname \"$0\")/runs\"\n\
                    exit 5\n";
        let (base, source, scenarios, integrator) = world("no-room-pass", gate);
        let db = Db::new("no_room_pass");
        let mission = db.gated_mission(&source, &scenarios);
        let task = db.reported_task(mission, "w");
        let agent_id = db.mission(mission)["tasks"][0]["agent_id"]
            .as_str()
            .expect("the task's agent")
            .to_string();
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

        integrator.pass_on(&db.store).expect("a pass");
        integrator.pass_on(&db.store).expect("a second pass");
        let runs = std::fs::read_to_string(base.join("runs")).expect("the gate ran");
        assert_eq!(
            runs.lines().count(),
            1,
            "the second pass waits out the backoff"
        );
        let after = gated(&db, mission);
        assert_eq!(
            after.tasks[0].state,
            spawn_core::mission::TaskState::Reported
        );
        assert_eq!((after.gate_aborts, after.gate_rejections), (0, 0));
        assert_eq!(after.gate_room_waits, 1);
        let record: Value = db
            .admin()
            .query_one(
                "select kind -> 'detail' -> 'record' from spawn_events
                  where session_id = 'session_control'
                    and kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
                  order by session_seq desc limit 1",
                &[],
            )
            .expect("the verdict")
            .get(0);
        assert_eq!(record["id"], task);
        assert_eq!(record["verdict"], "no_room");
        assert_eq!(record["free_kb"], 2048);
        assert_eq!(record["tmpdir"], "/data/tmp");
        assert_eq!(record["escalate"], false);
        assert!(
            record["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("0.0 GiB free in /data/tmp")),
            "{record}"
        );
        assert!(
            record["log"]
                .as_str()
                .is_some_and(|log| log.contains("! gate: 0.0 GiB free"))
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
        let after = db.mission(mission);
        assert_eq!(after["tasks"][0]["state"], "landed", "{after}");
        // One landing commit on top of the base, the work its second parent,
        // its message from the record (the brief and the report).
        let main = source_main(&source);
        let rev = |reference: String| {
            git(&["--git-dir", &source, "rev-parse", &reference]).expect("rev-parse")
        };
        assert_eq!(rev(format!("{main}^1")), b0);
        assert_eq!(rev(format!("{main}^2")), work);
        let message =
            git(&["--git-dir", &source, "show", "-s", "--format=%B", &main]).expect("message");
        assert!(
            message.starts_with(&format!(
                "brief (task {task})\n\nagain\n\nSpawn-Task: {task}\n"
            )),
            "{message}"
        );
        assert_eq!(after["baseline"], main.as_str());
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
