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

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use spawn_core::command::{CommandAction, Submission};
use spawn_core::event::Engine;
use spawn_core::id::{AgentId, IdempotencyKey, Principal, SessionId, WorkspaceId};
use spawn_core::port::{CommandQueue, WorkspaceStore};
use spawn_host::GitWorkspace;
use spawn_postgres::PostgresStore;

use crate::gate::{GateFailure, count_scenarios};
use crate::{Flags, env_or, env_value, open_store};

/// Rejections in a row that stop the loop.
const HALT_AFTER: u32 = 3;

/// `evolve` result when the backlog has no open task. The CLI reports it as
/// success; `spawn rsi-loop` waits longer before looking again.
pub(crate) const IDLE: i32 = 3;

/// Guard the candidate tree against runaway growth.
const MAX_PATCH_BYTES: usize = 1 << 20;

/// Wall clock of one iteration, from `docs/rsi.md` §9.
const ITERATION_WALL_CLOCK: Duration = Duration::from_mins(45);

/// Tool calls one iteration may make, from `docs/rsi.md` §9.
const ITERATION_TOOL_CALLS: u32 = 60;

/// Wall clock of one gate run. Two cargo builds fit well inside it, and a
/// hung build must not block the loop forever.
const GATE_WALL_CLOCK: Duration = Duration::from_mins(20);

/// Characters of a previous patch quoted in the next prompt.
const PROMPT_PATCH_CHARS: usize = 6000;

/// Characters of a previous gate log quoted in the next prompt.
const PROMPT_LOG_CHARS: usize = 4000;

/// Attempts summarized in a recurring task's prompt.
const PROMPT_HISTORY: u32 = 6;

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
    fn meets(&self, value: f64) -> bool {
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

/// The candidate's metric from a gate log.
///
/// Scenario output is written by candidate code, so a `METRIC` line in it
/// proves nothing: a regression scenario could print a better number after the
/// real one. The gate puts the candidate run's metric into its verdict, and
/// that is what counts. Without one (logs from before that change, or a run
/// that printed none) only the first candidate section is read: it ends
/// before any regression output, and the scenario prints its own line last.
pub(crate) fn parse_metrics(log: &str) -> Vec<Metric> {
    let verdict = log
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with('{'))
        .and_then(|line| serde_json::from_str::<serde_json::Value>(line).ok());
    if let Some(metric) = verdict
        .as_ref()
        .and_then(|verdict| verdict.get("metric"))
        .and_then(serde_json::Value::as_str)
    {
        return metric_lines(metric);
    }
    let section = log
        .lines()
        .skip_while(|line| !line.starts_with("| == candidate"))
        .skip(1)
        .take_while(|line| !line.starts_with("| =="))
        .collect::<Vec<_>>()
        .join("\n");
    metric_lines(&section)
}

fn metric_lines(text: &str) -> Vec<Metric> {
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

fn tail_chars(text: &str, limit: usize) -> &str {
    let count = text.chars().count();
    if count <= limit {
        return text;
    }
    let skip = text
        .char_indices()
        .nth(count - limit)
        .map_or(0, |(index, _)| index);
    &text[skip..]
}

fn head_chars(text: &str, limit: usize) -> &str {
    text.char_indices()
        .nth(limit)
        .map_or(text, |(index, _)| &text[..index])
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
    let gate = absolute(&env_or("SPAWN_RSI_JUDGE", "scripts/rsi/gate.sh"))?;

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

    // A halt holds until a person clears it: a restarted controller must not
    // start one more iteration first.
    let halted = check_halt(&store)?;
    if halted != 0 {
        return Ok(halted);
    }

    match store.rsi_open_iteration().map_err(describe_error)? {
        None => {
            // Between iterations the git baseline and the stored one agree.
            // If they do not, something outside the controller moved one.
            let main = workspaces
                .commit_id_of("refs/heads/main")
                .map_err(|error| error.to_string())?;
            if main != state.baseline {
                println!(
                    "evolve: HALT: main is {main} but the recorded baseline is {}",
                    state.baseline
                );
                return Ok(2);
            }
            start_iteration(&store, &root, &state, &backlog)
        }
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
    let Some(task) = next_open_task(store, backlog, &solved)? else {
        println!("evolve: backlog has no open task");
        return Ok(IDLE);
    };
    let iteration = state.iteration + 1;
    let prompt = task_prompt(store, &task)?;
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

/// The prompt of one iteration: the task, the working rules, and what the
/// previous attempts produced.
fn task_prompt(store: &PostgresStore, task: &Task) -> Result<String, String> {
    let mut prompt = task.prompt.clone();
    prompt.push_str(
        "\n\nWork only inside this repository. Do not run git commands and do not \
         create commits. Do not edit `rsi/` or `scripts/rsi/`: the gate rejects any \
         change there.\n\nWhen the task is finished, create the file `.spawn/done` \
         at the repository root. That file is the completion signal; without it the \
         task is treated as unfinished.\n",
    );
    let history = store
        .rsi_attempts_of(task.id, PROMPT_HISTORY)
        .map_err(describe_error)?;
    if task.recurring {
        if let Some(name) = &task.metric {
            let current = history.iter().find_map(|attempt| {
                let metric = parse_metrics(&attempt.gate_log)
                    .into_iter()
                    .find(|metric| &metric.name == name)?;
                if attempt.accepted {
                    metric.candidate
                } else {
                    metric.baseline
                }
            });
            let _ = write!(
                prompt,
                "\nObjective metric: `{name}` ({} is better).",
                if task.lower_is_better {
                    "lower"
                } else {
                    "higher"
                }
            );
            if let Some(value) = current {
                let _ = write!(prompt, " Current baseline: {value} {}.", task.unit);
            }
            if let Some(target) = task.target {
                let _ = write!(prompt, " Target: {target} {}.", task.unit);
            }
            prompt.push('\n');
        }
        if !history.is_empty() {
            prompt.push_str("\nPrevious attempts at this objective, newest first:\n");
            for attempt in &history {
                let outcome = if attempt.accepted {
                    "landed"
                } else {
                    "rejected"
                };
                let reason = attempt
                    .gate_log
                    .lines()
                    .rev()
                    .find(|line| line.trim_start().starts_with('{') || !line.starts_with('|'))
                    .unwrap_or("")
                    .trim();
                let metrics: Vec<String> = parse_metrics(&attempt.gate_log)
                    .iter()
                    .map(|metric| {
                        format!(
                            "{} {} -> {}",
                            metric.name,
                            metric.baseline.map_or("?".to_string(), |v| v.to_string()),
                            metric.candidate.map_or("?".to_string(), |v| v.to_string())
                        )
                    })
                    .collect();
                let _ = writeln!(
                    prompt,
                    "- iteration {}: {outcome}; {} {}",
                    attempt.iteration,
                    metrics.join(", "),
                    head_chars(reason, 300)
                );
            }
        }
    }
    if let Some(last) = history.first().filter(|attempt| !attempt.accepted) {
        prompt.push_str("\nThe previous attempt was rejected.\n\nPatch:\n");
        prompt.push_str(head_chars(&last.diff, PROMPT_PATCH_CHARS));
        prompt.push_str("\n\nGate output (tail):\n");
        prompt.push_str(tail_chars(&last.gate_log, PROMPT_LOG_CHARS));
        prompt.push('\n');
    }
    Ok(prompt)
}

/// Channel into the loop's one long-lived session.
///
/// A live execution takes typed input. A dead one resumes its native session,
/// so the agent keeps the memory it accumulated. A new session starts only for
/// the first task, or when no execution ever bound a native session.
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
    // An execution that died before the engine reported its session leaves
    // nothing to resume. Resuming would abort every later iteration the same
    // way until the loop halts.
    if !store.rsi_has_native_session().map_err(describe_error)? {
        return Ok(CommandAction::Start {
            engine: Engine::Claude,
            prompt,
            parent_session_id: None,
        });
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
    // transition. Without this, one hung agent stops the loop forever. An
    // accepted one is only landing, and `main` may already have moved.
    if iteration.phase != "Accepted"
        && let Some(budget) = store
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
        "Accepted" => {
            // The gate accepted and the controller stopped before landing.
            // Landing again is idempotent; the gate is not run twice.
            let (Some(candidate), Some(log)) = (
                iteration.candidate.as_deref(),
                iteration.verdict_log.as_deref(),
            ) else {
                return abort_iteration(store, iteration, "Accepted iteration lost its verdict");
            };
            land(
                store,
                workspaces,
                state,
                iteration,
                candidate,
                iteration.patch.as_deref().unwrap_or(""),
                log,
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
    // The agent never ran, so this says nothing about the task. Counting it as
    // a rejection would feed a good patch back as a failure.
    abort_iteration(
        store,
        iteration,
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
        // A gate that cannot run never judged the candidate, so it aborts.
        Err(GateFailure::Failed(error)) => {
            return abort_iteration(store, iteration, &format!("gate failed: {error}"));
        }
    };
    if !verdict.accepted {
        return reject_iteration(store, iteration, patch, &verdict.reason);
    }
    // The verdict is stored before git moves, so a crash after the git step
    // finishes the landing on the next call instead of judging again.
    store
        .rsi_accept(iteration.iteration, &verdict.output)
        .map_err(describe_error)?;
    land(
        store,
        workspaces,
        state,
        iteration,
        candidate,
        patch,
        &verdict.output,
    )
}

/// Move `main`, then the stored baseline, the phase and the attempt in one
/// transaction. Both steps are idempotent, so a crash between them is
/// finished by the next call.
fn land(
    store: &PostgresStore,
    workspaces: &GitWorkspace,
    state: &spawn_postgres::RsiState,
    iteration: &spawn_postgres::RsiIteration,
    candidate: &str,
    patch: &str,
    log: &str,
) -> Result<(), String> {
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
    let landed = store
        .rsi_land(
            iteration.iteration,
            iteration.task_id,
            &state.baseline,
            candidate,
            if patch.is_empty() {
                iteration.patch.as_deref().unwrap_or("")
            } else {
                patch
            },
            log,
        )
        .map_err(describe_error)?;
    if !landed {
        return Err(format!(
            "main is {candidate} but the stored baseline moved off {}",
            state.baseline
        ));
    }
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

fn judge_candidate(
    gate: &Path,
    workspaces: &GitWorkspace,
    baseline: &str,
    candidate: &str,
    scenarios: &Path,
    iteration: &spawn_postgres::RsiIteration,
) -> Result<GateVerdict, GateFailure> {
    let scenario = scenarios.join(format!("{:03}.sh", iteration.task_id));
    let verdict = crate::gate::judge(
        gate,
        Path::new(workspaces.project()),
        baseline,
        candidate,
        scenarios,
        Some(&scenario),
        GATE_WALL_CLOCK,
    )?;
    Ok(GateVerdict {
        accepted: verdict.accepted,
        reason: verdict.reason,
        output: verdict.output,
    })
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
    // Aborts are infrastructure. A candidate that can manufacture them would
    // otherwise bypass the rejection limit, so they halt on their own count.
    if state.infra_failures >= HALT_AFTER {
        println!(
            "evolve: HALT after {} aborts in a row",
            state.infra_failures
        );
        return Ok(2);
    }
    Ok(0)
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
        tasks.push(Task {
            id,
            title: text_field("title").unwrap_or_else(|| format!("Task {id}")),
            prompt,
            recurring: value.get("recurring").and_then(serde_json::Value::as_bool) == Some(true),
            metric: text_field("metric"),
            unit: text_field("unit").unwrap_or_default(),
            lower_is_better: text_field("direction").as_deref() != Some("higher"),
            target: value.get("target").and_then(serde_json::Value::as_f64),
        });
    }
    tasks.sort_by_key(|task| task.id);
    Ok(tasks)
}

/// The latest landed value of a recurring task's metric.
pub(crate) fn landed_metric(store: &PostgresStore, task: &Task) -> Result<Option<f64>, String> {
    let Some(name) = &task.metric else {
        return Ok(None);
    };
    let attempts = store
        .rsi_attempts_of(task.id, 200)
        .map_err(describe_error)?;
    Ok(attempts
        .iter()
        .filter(|attempt| attempt.accepted)
        .find_map(|attempt| {
            parse_metrics(&attempt.gate_log)
                .into_iter()
                .find(|metric| &metric.name == name)?
                .candidate
        }))
}

/// Lowest-numbered open task: a one-shot task without an accepted patch, or
/// a recurring task whose landed metric has not reached its target.
fn next_open_task(
    store: &PostgresStore,
    directory: &Path,
    solved: &[u32],
) -> Result<Option<Task>, String> {
    for task in load_backlog(directory)? {
        if !task.recurring {
            if !solved.contains(&task.id) {
                return Ok(Some(task));
            }
            continue;
        }
        match landed_metric(store, &task)? {
            Some(value) if task.meets(value) => {
                println!("evolve: objective {} met: {value} {}", task.id, task.unit);
            }
            _ => return Ok(Some(task)),
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::parse_metrics;

    #[test]
    fn the_metric_comes_from_the_verdict() {
        let log = "| == candidate: 002.sh must pass\n\
                   | METRIC ingest_total_ns baseline=1540 candidate=1400\n\
                   | == regression: 001.sh must still pass\n\
                   | METRIC ingest_total_ns baseline=1540 candidate=1\n\
                   {\"verdict\":\"accepted\",\"metric\":\"METRIC ingest_total_ns baseline=1540 candidate=1400\"}";
        let metrics = parse_metrics(log);
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].baseline, Some(1540.0));
        assert_eq!(metrics[0].candidate, Some(1400.0));
    }

    #[test]
    fn an_old_verdict_falls_back_to_the_candidate_section() {
        // Iteration 6's real log: the verdict line carries no metric.
        let log = "| == baseline: 002.sh must fail\n\
                   | METRIC ingest_total_ns baseline=1578 candidate=1586\n\
                   | == candidate: 002.sh must pass\n\
                   | METRIC ingest_total_ns baseline=1573 candidate=1021\n\
                   | == regression: 001.sh must still pass\n\
                   | METRIC ingest_total_ns baseline=1573 candidate=1\n\
                   {\"verdict\":\"accepted\"}";
        let metrics = parse_metrics(log);
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].candidate, Some(1021.0));
    }

    #[test]
    fn an_old_log_is_read_from_the_candidate_section_only() {
        let log = "| == baseline: 002.sh must fail\n\
                   | METRIC ingest_total_ns baseline=1544 candidate=1550\n\
                   | == candidate: 002.sh must pass\n\
                   | METRIC ingest_total_ns baseline=1540 candidate=1400\n\
                   | == regression: 001.sh must still pass\n\
                   | METRIC ingest_total_ns baseline=1540 candidate=1\n";
        let metrics = parse_metrics(log);
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].candidate, Some(1400.0));
    }
}
