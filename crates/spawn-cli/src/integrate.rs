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
//!    (`accept` = review) or just reported (`gate`). The candidate is what
//!    the agent published: the source's `refs/heads/spawn/<agent>` (pushed
//!    by the Bridge's pusher, [`crate::push`]), fetched into this mirror's
//!    own `refs/spawn-integrate/<agent>`. The integrator never commits a
//!    mounted workspace and never moves the agent's `refs/heads/<agent>`,
//!    so it works the same on a host that never ran the agent, and a file
//!    written after the last checkpoint never lands. The candidate is
//!    merged with the source's main when main moved, and judged by
//!    `scripts/rsi/gate.sh` with the mission's scenarios. A pass records
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
//! 5. an objective mission with no open task gets its next objective
//!    ([`objectives`]).
//!
//! Like the dispatcher, it keeps no state of its own: the control stream
//! says what was judged and landed. An advisory lock allows one integrator
//! per database, so two gates never run at once.

use std::path::{Path, PathBuf};
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

/// How long a pass waits for a pushed branch that lags the agent's latest
/// checkpoint before it gates the task `stale`: the pusher pushes a branch at
/// most once per [`crate::push::INTERVAL`].
const PUSH_WAIT: Duration = Duration::from_secs(12);

/// Pause between two looks at a lagging pushed branch.
const PUSH_POLL: Duration = Duration::from_millis(500);

/// Who the integrator's commands come from.
const PRINCIPAL: &str = "spawn-integrate";

const USAGE: &str = "usage: spawn integrate [--once] [--root <dir>]";

/// One open mission with a gate, from `gated_missions`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Mission {
    pub(crate) id: i64,
    /// `open` or `active`; `done` while accepted work still lands.
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
        let store = open_store()?;
        if !store
            .integrator_try_lock()
            .map_err(|error| error.to_string())?
        {
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
            // An objective mission with nothing open gets its next task; a
            // closed mission gets none.
            if mission.state == "done" {
                return Ok(false);
            }
            return Ok(objectives::assign_next(store, mission)?.is_some());
        };
        self.judge(store, &mirror, source, &main, &scenarios, count, task)
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

    /// Judge one task from its pushed branch. True when what it recorded may
    /// move another mission's view; a stale push moves nothing, so the other
    /// missions still get their turn in this pass.
    #[allow(clippy::too_many_arguments)]
    fn judge(
        &self,
        store: &PostgresStore,
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
                if !sources::shares_branches(source, &sources::push_sources()) {
                    eprintln!(
                        "integrate: {source} does not receive agents' branches; list it in \
                         SPAWN_PUSH_SOURCES for its work to land"
                    );
                }
                record(
                    store,
                    &json!({"type": "task.gated", "id": task.id, "verdict": "stale",
                            "reason": reason}),
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
        store: &PostgresStore,
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
            return gated(
                "conflict",
                json!({"note": format!(
                        "Your work conflicts with the source's current main ({main}). \
                         Main is merged into your workspace when a turn of yours ends and \
                         while you are idle (within about 30 s), so the conflicting files \
                         carry conflict markers there (<<<<<<< / ======= / >>>>>>>; if \
                         none show yet, they will once this turn ends). Resolve them, keeping both sides' intent and removing every \
                         marker, then report again: the checkpoint after the last marker \
                         is gone records the merge.")}),
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
        if spawn_host::chunks::trailer_of(&message).is_none() {
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
    // .gitattributes otherwise), as the Bridge merges main into idle
    // branches.
    let Ok(tree) = git(&[
        "--git-dir",
        mirror_path,
        &format!("--attr-source={main}"),
        "merge-tree",
        "--write-tree",
        main,
        branch,
    ]) else {
        return Ok(None);
    };
    let tree = tree.lines().next().unwrap_or_default().to_string();
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
/// moved meanwhile sends the task back to be judged again.
fn land(store: &PostgresStore, mirror: &Path, source: &str, task: &Task) -> Result<(), String> {
    let (Some(base), Some(candidate)) = (task.base.as_deref(), task.candidate.as_deref()) else {
        return record(
            store,
            &json!({"type": "task.gated", "id": task.id, "verdict": "stale"}),
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
    use super::{Freshness, Mission, aborted, freshness, halt_reason, markers_note, over_budget};
    use crate::gate::GateFailure;
    use serde_json::json;

    /// Commit everything in the checkout `directory`.
    fn commit_in(directory: &str, message: &str) {
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
            if !main_moves {
                // A large file: the head carries the trailer, so the landing
                // is a commit of its own.
                std::fs::write(directory.join("weights.bin"), vec![1_u8; 9 << 20]).expect("large");
            }
            assert!(matches!(
                store.commit(&workspace, "work").expect("commit"),
                Commit::Created(_)
            ));
            assert_eq!(push_branch(&mirror, &source, agent), Pushed::Done);
            if main_moves {
                // Main moved since the worker started.
                let clone = base.join(format!("clone-{agent}"));
                let clone_path = clone.to_string_lossy().to_string();
                git(&["clone", "-q", &source, &clone_path]).expect("clone");
                std::fs::write(clone.join("other.txt"), "theirs\n").expect("write");
                commit_in(&clone_path, "elsewhere");
                git(&["-C", &clone_path, "push", "-q", "origin", "main"]).expect("push main");
                crate::sources::refresh(&mirror, &source);
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
