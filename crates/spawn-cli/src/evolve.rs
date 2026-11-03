//! One step of the recursive self-improvement loop.
//!
//! The controller is a one-shot process like `tick`. Each call takes the
//! advisory lock, reads the phase from the `rsi_*` tables, makes one
//! transition, and exits. State lives only in those tables: `EventKind` is a
//! closed enum, so an `rsi.*` event would make the next tick fail.
//!
//! Acceptance is black box. A scenario script drives the `spawn` binary as an
//! outside observer and must fail on the baseline before it is allowed to
//! pass on the candidate.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use spawn_core::command::{CommandAction, Submission};
use spawn_core::event::Engine;
use spawn_core::id::{AgentId, IdempotencyKey, Principal, SessionId, WorkspaceId};
use spawn_core::port::{CommandQueue, WorkspaceStore};
use spawn_host::GitWorkspace;
use spawn_postgres::PostgresStore;

use crate::{Flags, env_or, env_value, open_store};

/// Rejections in a row that stop the loop.
const HALT_AFTER: u32 = 3;

/// Guard the candidate tree against runaway growth.
const MAX_PATCH_BYTES: usize = 1 << 20;

/// Wall clock of one iteration, from `docs/rsi.md` §9.
const ITERATION_WALL_CLOCK: Duration = Duration::from_mins(45);

/// Tool calls one iteration may make, from `docs/rsi.md` §9.
const ITERATION_TOOL_CALLS: u32 = 60;

/// Wall clock of one gate run. Two cargo builds fit well inside it, and a
/// hung build must not block the loop forever.
const GATE_WALL_CLOCK: Duration = Duration::from_mins(20);

/// One backlog task.
struct Task {
    id: u32,
    prompt: String,
}

/// Advance the loop by one step.
pub fn evolve(flags: &Flags) -> Result<i32, String> {
    let store = open_store()?;
    let repo = flags
        .get("repo")
        .or_else(|| env_value("SPAWN_GIT_REPO"))
        .ok_or_else(|| "--repo or SPAWN_GIT_REPO is required".to_string())?;
    let root = flags
        .get("root")
        .or_else(|| env_value("SPAWN_WORKSPACE_ROOT"))
        .ok_or_else(|| "--root or SPAWN_WORKSPACE_ROOT is required".to_string())?;
    let root = PathBuf::from(root);
    let backlog = absolute(&env_or("SPAWN_RSI_BACKLOG", "rsi/backlog"))?;
    let scenarios = absolute(&env_or("SPAWN_RSI_SCENARIOS", "rsi/scenarios"))?;
    let gate = absolute(&env_or("SPAWN_RSI_JUDGE", "scripts/rsi/judge-candidate.sh"))?;

    // The lock belongs to the connection, so a crash never blocks the next
    // controller.
    if !store.rsi_try_lock().map_err(describe_error)? {
        println!("evolve: another controller holds the lock");
        return Ok(0);
    }

    let workspaces = GitWorkspace::new(&repo, &root);
    if store.rsi_state().map_err(describe_error)?.is_none() {
        let base = workspaces
            .commit_id_of("refs/heads/main")
            .map_err(|error| error.to_string())?;
        store.rsi_seed_baseline(&base).map_err(describe_error)?;
    }
    let state = store
        .rsi_state()
        .map_err(describe_error)?
        .ok_or_else(|| "rsi_state is not seeded".to_string())?;

    // A candidate that deletes the scenario that judges it passes the gate by
    // removing the work, so the count of scenario files may not fall.
    if scenario_floor(&store, &scenarios, state.scenario_count)? {
        println!(
            "evolve: HALT: scenarios fell below {}",
            state.scenario_count
        );
        return Ok(2);
    }

    match store.rsi_open_iteration().map_err(describe_error)? {
        None => start_iteration(&store, &root, &state, &backlog),
        Some(iteration) => {
            advance_iteration(
                &store,
                &workspaces,
                &root,
                &scenarios,
                &gate,
                &state,
                &iteration,
            )?;
            check_halt(&store)
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
fn describe_error(error: spawn_core::port::StoreError) -> String {
    error.to_string()
}

/// Resolve a path against the controller's working directory.
///
/// The gate enters the candidate tree before it runs the scenario, so a
/// relative scenario path would resolve inside that tree and let the candidate
/// supply the test that judges it.
fn absolute(path: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        return Ok(path);
    }
    std::env::current_dir()
        .map(|base| base.join(path))
        .map_err(|error| format!("current directory: {error}"))
}

fn start_iteration(
    store: &PostgresStore,
    root: &Path,
    state: &spawn_postgres::RsiState,
    backlog: &Path,
) -> Result<i32, String> {
    let solved = store.rsi_solved_tasks().map_err(describe_error)?;
    let Some(task) = next_open_task(backlog, &solved)? else {
        println!("evolve: backlog has no open task");
        return Ok(0);
    };
    let iteration = state.iteration + 1;
    let mut prompt = task.prompt.clone();
    prompt.push_str(
        "\n\nWork only inside this repository. Do not run git commands and do not \
         create commits.\n\nWhen the task is finished, create the file `.spawn/done` \
         at the repository root. That file is the completion signal; without it the \
         task is treated as unfinished.\n",
    );
    if let Some((diff, log)) = store.rsi_last_attempt_of(task.id).map_err(describe_error)? {
        prompt.push_str("\nThe previous attempt failed.\n\nPatch:\n");
        prompt.push_str(&diff);
        prompt.push_str("\n\nGate output:\n");
        prompt.push_str(&log);
        prompt.push('\n');
    }
    // The completion marker belongs to the task that ends. Leaving it in
    // place would make the next task look finished before it starts.
    let marker = root.join(spawn_postgres::RSI_AGENT).join(".spawn/done");
    match std::fs::remove_file(&marker) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("remove {}: {error}", marker.display())),
    }
    let action = next_action(store, prompt)?;
    store
        .rsi_begin_iteration(task.id, iteration, action)
        .map_err(describe_error)?;
    println!("evolve: iteration {iteration} starts task {}", task.id);
    Ok(0)
}

/// Channel into the loop's one long-lived session.
///
/// A live execution takes typed input. A dead one resumes its native session,
/// so the agent keeps the memory it accumulated. Only the first task starts a
/// new session.
fn next_action(store: &PostgresStore, prompt: String) -> Result<CommandAction, String> {
    let session = spawn_core::id::SessionId::new(spawn_postgres::RSI_SESSION)
        .map_err(|error| error.to_string())?;
    let executions = store.executions(&session).map_err(describe_error)?;
    let Some(latest) = executions.first() else {
        return Ok(CommandAction::Start {
            engine: Engine::Claude,
            prompt,
            parent_session_id: None,
        });
    };
    if latest.ended_at.is_none() {
        return Ok(CommandAction::Input { text: prompt });
    }
    Ok(CommandAction::Resume {
        prompt: Some(prompt),
    })
}

fn advance_iteration(
    store: &PostgresStore,
    workspaces: &GitWorkspace,
    root: &Path,
    scenarios: &Path,
    gate: &Path,
    state: &spawn_postgres::RsiState,
    iteration: &spawn_postgres::RsiIteration,
) -> Result<(), String> {
    // An iteration that runs past its budget is aborted before the next
    // transition. Without this, one hung agent stops the loop forever.
    if let Some(budget) = store
        .rsi_iteration_budget(iteration.iteration)
        .map_err(describe_error)?
    {
        if let Some(elapsed) = budget.elapsed_ms {
            let millis = u64::try_from(elapsed).unwrap_or(0);
            if Duration::from_millis(millis) > ITERATION_WALL_CLOCK {
                return abort_iteration(store, iteration, "iteration exceeded its wall clock");
            }
        }
        if budget.elapsed_ms.is_some() && budget.tool_calls > ITERATION_TOOL_CALLS {
            return abort_iteration(store, iteration, "iteration exceeded its tool-call budget");
        }
    }

    match iteration.phase.as_str() {
        "Starting" => settle_started(store, iteration),
        "Working" => capture_candidate(store, workspaces, root, scenarios, gate, state, iteration),
        "Gating" => {
            // Reached when the previous call stopped between the phase change
            // and the verdict. Judging again is idempotent.
            let Some(candidate) = iteration.candidate.as_deref() else {
                return reject_iteration(store, iteration, "", "Gating iteration has no candidate");
            };
            settle_gating(
                store,
                workspaces,
                scenarios,
                gate,
                state,
                iteration,
                candidate,
                iteration.patch.as_deref().unwrap_or(""),
            )
        }
        _ => Ok(()),
    }
}

/// Settle a `Starting` iteration from the state of its command.
fn settle_started(
    store: &PostgresStore,
    iteration: &spawn_postgres::RsiIteration,
) -> Result<(), String> {
    let Some(command) = iteration.command_id else {
        return store
            .rsi_close_iteration(iteration.iteration, "Aborted", "iteration has no command")
            .map_err(describe_error);
    };
    let Some(row) = store.rsi_command_state(command).map_err(describe_error)? else {
        return Err(format!("command {} is missing", command.0));
    };
    if row.state != "finished" {
        return Ok(());
    }
    if row.succeeded == Some(true) {
        return store
            .rsi_set_phase(iteration.iteration, "Working", None, None)
            .map_err(describe_error);
    }
    reject_iteration(
        store,
        iteration,
        "",
        row.error.as_deref().unwrap_or("start failed"),
    )
}

/// Take the patch out of the workspace once the agent signals completion.
fn capture_candidate(
    store: &PostgresStore,
    workspaces: &GitWorkspace,
    root: &Path,
    scenarios: &Path,
    gate: &Path,
    state: &spawn_postgres::RsiState,
    iteration: &spawn_postgres::RsiIteration,
) -> Result<(), String> {
    let directory = root.join(&iteration.agent_id);
    let branch = format!("refs/heads/{}", iteration.agent_id);
    let done = directory.join(".spawn/done").exists();

    if directory.exists() {
        let workspace = WorkspaceId::new(format!("{}:{}", iteration.agent_id, directory.display()))
            .map_err(|error| error.to_string())?;
        workspaces
            .commit(&workspace, "rsi candidate")
            .map_err(|error| error.to_string())?;
    }

    let candidate = workspaces
        .commit_id_of(&branch)
        .map_err(|error| error.to_string())?;
    if candidate == state.baseline {
        // A finished execution that changed nothing is a rejection. Waiting
        // for the marker would wait forever once the Bridge unmounts the
        // directory and deletes `.spawn/done` with it.
        if done || !has_running_execution(store, iteration)? {
            return reject_iteration(store, iteration, "", "workspace has no change");
        }
        return Ok(());
    }
    if !done && has_running_execution(store, iteration)? {
        return Ok(());
    }

    let patch = workspaces
        .patch_between(&state.baseline, &candidate)
        .map_err(|error| error.to_string())?;
    if patch.len() > MAX_PATCH_BYTES {
        return reject_iteration(store, iteration, "", "patch is too large");
    }
    store
        .rsi_set_phase(
            iteration.iteration,
            "Gating",
            Some(&patch),
            Some(&candidate),
        )
        .map_err(describe_error)?;

    settle_gating(
        store, workspaces, scenarios, gate, state, iteration, &candidate, &patch,
    )
}

/// Judge a candidate and land it or reject it.
///
/// A gate that fails to run rejects the iteration instead of returning an
/// error: an error leaves the phase at `Gating`, and every later call would
/// repeat the same failure without moving the loop.
#[allow(clippy::too_many_arguments)]
fn settle_gating(
    store: &PostgresStore,
    workspaces: &GitWorkspace,
    scenarios: &Path,
    gate: &Path,
    state: &spawn_postgres::RsiState,
    iteration: &spawn_postgres::RsiIteration,
    candidate: &str,
    patch: &str,
) -> Result<(), String> {
    let verdict = match judge_candidate(
        gate,
        workspaces,
        &state.baseline,
        candidate,
        scenarios,
        iteration,
    ) {
        Ok(verdict) => verdict,
        // A gate that never returns says nothing about the candidate, so it
        // aborts instead of counting as a rejection.
        Err(GateFailure::Timeout) => {
            return abort_iteration(store, iteration, "gate exceeded its wall clock");
        }
        Err(GateFailure::Failed(error)) => {
            return reject_iteration(store, iteration, patch, &format!("gate failed: {error}"));
        }
    };
    if !verdict.accepted {
        return reject_iteration(store, iteration, patch, &verdict.reason);
    }
    let advanced = workspaces
        .advance_base(&state.baseline, candidate)
        .map_err(|error| error.to_string())?;
    if !advanced {
        return reject_iteration(
            store,
            iteration,
            patch,
            "baseline moved during the iteration",
        );
    }
    store
        .rsi_advance_baseline(iteration.iteration, &state.baseline, candidate)
        .map_err(describe_error)?;
    record_attempt(store, iteration, patch, &verdict.output, true)?;
    println!(
        "evolve: iteration {} landed {candidate}",
        iteration.iteration
    );
    Ok(())
}

/// Whether the iteration still has a running execution.
fn has_running_execution(
    store: &PostgresStore,
    iteration: &spawn_postgres::RsiIteration,
) -> Result<bool, String> {
    let session =
        spawn_core::id::SessionId::new(&iteration.session_id).map_err(|error| error.to_string())?;
    let executions = store.executions(&session).map_err(describe_error)?;
    Ok(executions.first().is_none_or(|row| row.ended_at.is_none()))
}

/// Close an iteration as `Aborted` and stop its agent.
///
/// An abort is not a rejection: the candidate was never judged, so it must not
/// count toward `HALT_AFTER`. The stop is best effort; a failed submission
/// only leaves the agent running, which the next iteration resumes into.
fn abort_iteration(
    store: &PostgresStore,
    iteration: &spawn_postgres::RsiIteration,
    note: &str,
) -> Result<(), String> {
    stop_execution(store, iteration);
    store
        .rsi_close_iteration(iteration.iteration, "Aborted", note)
        .map_err(describe_error)?;
    println!("evolve: iteration {} aborted: {note}", iteration.iteration);
    Ok(())
}

fn stop_execution(store: &PostgresStore, iteration: &spawn_postgres::RsiIteration) {
    let key = format!("rsi-{}-abort", iteration.iteration);
    let submission = (|| {
        Some(Submission {
            agent_id: AgentId::new(iteration.agent_id.clone()).ok()?,
            session_id: SessionId::new(iteration.session_id.clone()).ok()?,
            idempotency_key: IdempotencyKey::new(key).ok()?,
            action: CommandAction::Stop,
        })
    })();
    let Some(submission) = submission else {
        return;
    };
    let Ok(principal) = Principal::new("spawn") else {
        return;
    };
    let _ = store.submit(&principal, &submission);
}

fn reject_iteration(
    store: &PostgresStore,
    iteration: &spawn_postgres::RsiIteration,
    patch: &str,
    note: &str,
) -> Result<(), String> {
    store
        .rsi_close_iteration(iteration.iteration, "Rejected", note)
        .map_err(describe_error)?;
    record_attempt(store, iteration, patch, note, false)?;
    println!("evolve: iteration {} rejected: {note}", iteration.iteration);
    Ok(())
}

fn record_attempt(
    store: &PostgresStore,
    iteration: &spawn_postgres::RsiIteration,
    patch: &str,
    output: &str,
    accepted: bool,
) -> Result<(), String> {
    let attempt = store
        .rsi_count_attempts(iteration.task_id)
        .map_err(describe_error)?
        + 1;
    store
        .rsi_record_attempt(
            iteration.task_id,
            attempt,
            iteration.iteration,
            Some(if patch.is_empty() {
                iteration.patch.as_deref().unwrap_or("")
            } else {
                patch
            }),
            Some(output),
            if accepted { "accepted" } else { "rejected" },
            accepted,
        )
        .map_err(describe_error)
}

/// One gate verdict.
struct GateVerdict {
    accepted: bool,
    reason: String,
    output: String,
}

/// Why the gate produced no verdict.
#[derive(Debug)]
enum GateFailure {
    /// The gate ran past `GATE_WALL_CLOCK`. Infrastructure, not the candidate.
    Timeout,
    /// The gate could not start or printed no verdict.
    Failed(String),
}

fn judge_candidate(
    gate: &Path,
    workspaces: &GitWorkspace,
    baseline: &str,
    candidate: &str,
    scenarios: &Path,
    iteration: &spawn_postgres::RsiIteration,
) -> Result<GateVerdict, GateFailure> {
    let scenario = scenarios.join(format!("{:03}.sh", iteration.task_id));
    let mut command = Command::new("sh");
    command
        .arg(gate)
        .arg(workspaces.project())
        .arg(baseline)
        .arg(candidate)
        .arg(&scenario);
    let output = run_bounded(command, GATE_WALL_CLOCK)?;
    let line = output
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with('{'))
        .ok_or_else(|| GateFailure::Failed("gate produced no verdict".to_string()))?;
    let verdict: serde_json::Value = serde_json::from_str(line)
        .map_err(|error| GateFailure::Failed(format!("gate verdict: {error}")))?;
    Ok(GateVerdict {
        accepted: verdict.get("verdict").and_then(|v| v.as_str()) == Some("accepted"),
        reason: verdict
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("gate rejected the candidate")
            .to_string(),
        output,
    })
}

/// Run a command with a wall clock, returning its combined output.
///
/// The child writes to files, not to pipes: a pipe that fills up would block
/// the child while this loop waits for it to exit.
fn run_bounded(mut command: Command, limit: Duration) -> Result<String, GateFailure> {
    let failed = |error: std::io::Error| GateFailure::Failed(error.to_string());
    let directory = std::env::temp_dir().join(format!("spawn-gate-{}", std::process::id()));
    std::fs::create_dir_all(&directory).map_err(failed)?;
    let out_path = directory.join("stdout");
    let err_path = directory.join("stderr");
    let stdout = std::fs::File::create(&out_path).map_err(failed)?;
    let stderr = std::fs::File::create(&err_path).map_err(failed)?;
    let mut child = command
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .map_err(failed)?;
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(GateFailure::Timeout);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(error) => return Err(failed(error)),
        }
    }
    let stdout = std::fs::read_to_string(&out_path).unwrap_or_default();
    let stderr = std::fs::read_to_string(&err_path).unwrap_or_default();
    Ok(format!("{stdout}{stderr}"))
}

/// Scenario files in the controller's checkout.
fn count_scenarios(directory: &Path) -> Result<u32, String> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(format!("read {}: {error}", directory.display())),
    };
    let mut count = 0;
    for entry in entries {
        let path = entry
            .map_err(|error| format!("read {}: {error}", directory.display()))?
            .path();
        if path.extension().and_then(|text| text.to_str()) == Some("sh") {
            count += 1;
        }
    }
    Ok(count)
}

/// Whether the scenario count fell below the recorded floor.
///
/// A zero floor means no round has recorded one yet, so the current count
/// becomes the floor.
fn scenario_floor(store: &PostgresStore, directory: &Path, floor: u32) -> Result<bool, String> {
    let count = count_scenarios(directory)?;
    if floor != 0 && count < floor {
        return Ok(true);
    }
    if count != floor {
        store
            .rsi_set_scenario_count(count)
            .map_err(describe_error)?;
    }
    Ok(false)
}

fn check_halt(store: &PostgresStore) -> Result<i32, String> {
    let state = store
        .rsi_state()
        .map_err(describe_error)?
        .ok_or_else(|| "rsi_state is not seeded".to_string())?;
    if state.consecutive_rejected >= HALT_AFTER {
        println!(
            "evolve: HALT after {} rejections in a row",
            state.consecutive_rejected
        );
        return Ok(2);
    }
    Ok(0)
}

/// Lowest-numbered task the loop has not solved, according to its own file.
fn next_open_task(directory: &Path, solved: &[u32]) -> Result<Option<Task>, String> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
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
        let task = Task {
            id: value
                .get("id")
                .and_then(serde_json::Value::as_u64)
                .and_then(|id| u32::try_from(id).ok())
                .ok_or_else(|| format!("{} has no id", path.display()))?,
            prompt: value
                .get("prompt")
                .and_then(|prompt| prompt.as_str())
                .ok_or_else(|| format!("{} has no prompt", path.display()))?
                .to_string(),
        };
        tasks.push(task);
    }
    tasks.sort_by_key(|task| task.id);
    Ok(tasks.into_iter().find(|task| !solved.contains(&task.id)))
}

#[cfg(test)]
mod tests {
    use super::{GateFailure, run_bounded};
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn shell(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        command
    }

    #[test]
    fn a_command_past_its_deadline_is_killed() {
        let started = Instant::now();
        let killed = run_bounded(shell("exec sleep 30"), Duration::from_millis(300));
        assert!(matches!(killed, Err(GateFailure::Timeout)));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_command_inside_its_deadline_returns_its_output() {
        let output = run_bounded(shell("echo accepted"), Duration::from_secs(10)).expect("output");
        assert!(output.contains("accepted"));
    }
}
