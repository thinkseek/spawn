//! `spawn mission`, `spawn task` and `spawn resident`: the mission layer's
//! commands (docs/fleet-architecture.md §3.2).
//!
//! Humans run them with the owner's login, the supervisor with its own; the
//! SQL functions behind them decide who may do what (only a human approves a
//! resident). Every command prints JSON.
//!
//! `spawn mission configure` changes only the settings it is given (objective
//! 027): the database merges them into the mission's row under the append
//! lock and records the whole result; a setting is cleared only by its own
//! `--no-<setting>` switch.

use serde_json::{Value, json};
use spawn_core::event::ModelProfile;
use spawn_core::grading::{
    DifficultyClassification, GradedPreset, RoutingDecision, RoutingError, TaskDifficulty,
    catalog_covers, classify_difficulty, route_preset, route_with_reprobe,
};

use crate::{Flags, open_store};

/// The usage of `spawn mission`, `spawn task` and `spawn resident`, the only
/// copy: main's help shows each command its own lines of it.
pub(crate) const USAGE: &str = "usage:
  spawn mission add --text <mission> [--source <git repo path or URL>] [<settings>]
  spawn mission configure --id N [<settings>] [<clearing switches>]   (owner only)
      changes only the settings it is given and keeps every other as the mission has it;
      the database merges them under its lock, checks the result and records it whole.
      A setting is cleared only by its own switch, which takes no value: --no-gate,
      --no-budget (every field), --no-minutes, --no-tool-calls, --no-daily-usd, --no-objectives,
      --no-workers (back to the default 1), --no-features; a switch and a value for the
      same setting are refused. Prints {mission, before, after}; with nothing given it
      changes nothing.
      Clearing the gate, or moving accept away from gate, waits while a task is landing.
      settings: --gate <scenarios dir> [--accept review|gate] [--minutes N] [--tool-calls N]
                [--daily-usd <US dollars above 0>]
                [--backlog <dir> --resident <name> [--workers N] [--features <file>]]
                (with add, all of them at once, and --backlog, --resident, --workers and
                --features need --gate: an objective lands only through a gate;
                with configure, any of them alone, and --backlog with --resident for a
                mission without objectives)
                (--features: the mission's feature map, a JSON file of capabilities and
                their provenance sources; identities and links only, never a status)
                (--workers: objective tasks in flight at once, 1 to 128, default 1: the
                resident takes one, per-task workers the rest)
                (--daily-usd: what the mission's model calls may cost in a UTC day; the
                loop raises one cost_budget alert the day it is reached)
  spawn mission objectives [--mission N]
      the backlog of every objective mission (--mission: that one): the fleet's open
      objectives and separately the owner-judged ones (the backlog field `owner_judged`:
      the owner judges those outside the gate, and the fleet never assigns them),
      each with its title, plus the counts of solved and met
  spawn mission pause --id N [--reason <text>]   (owner, a human: holds the mission, objective 099)
      (records mission.paused {id, reason}: no objective is assigned to it and nothing is
       delivered to its workers; its resident is stopped by a Stop (its final checkpoint on its
       branch, its native session kept) and nothing revives it while the pause lasts; no
       mission_idle alert, and its paused time is not idle on the Scorecard. Its state stays
       as it is; a halt stays too. A closed or already paused mission is refused, nothing
       recorded. --reason: at most 500 characters. Prints {mission, paused})
  spawn mission resume --id N
      (a paused mission: records mission.unpaused {id, pause_event} and nothing else, and its
       resident is resumed in its own native session; a halt it is in stays, for a second
       resume. A halted mission that is not paused: the halt is cleared, mission.resumed)
  spawn mission push --source <path> --commit <commit> [--repo <dir>] [--wait <seconds>]
                     [--gated] [--urgent] [--reason <text>]
  spawn mission push --withdraw <request>
      (owner: land a commit on a mission source's main. <commit> is resolved in the git
       repository of the current directory (--repo: another), or is already in the source;
       it is put in the source at refs/spawn-owner/<hash>, no branch moves, and the request
       is recorded (source.push_requested): prints {request, source, commit}. The running
       integrator applies it between its own steps (on a held source too: a hold stops task
       landings, not the owner's), never while a gate of the source runs (it waits for
       that gate), under the landing lock: main moves to the commit (a fast-forward) or to
       a merge of it, and one source.moved naming the request moves the baselines; a
       conflict is source.push_refused. Nothing halts, and spawn-integrate is never
       stopped for it. --gated: the source's gate judges the landing first, as a judging of
       its own taken before the source's next task, with the commit's own scenarios: moved
       when accepted, source.push_refused (with the gate's reason and log) when rejected,
       pending again after a host fault or no room, and refused when it ends with no
       verdict twice in a row for the same reason (no room is never one). --urgent: a judging of a task in flight
       on the source is stopped and recorded as discarded by this request (stale, counted
       nowhere; the task is judged again on the new main), for a fix of a red main.
       --reason: the owner's words, on the record. --wait: wait that long for
       the outcome and print it; exit 0 applied, 1 refused, 3 still pending.
       --withdraw: the pending request <request> (waiting, backing off or being judged: its
       gate is stopped and whatever it judged is discarded) ends refused, source.push_refused
       with the reason \"withdrawn by the owner\", and nothing of it lands: prints {request,
       state, outcome}. A request that ended already, or that does not exist, is an error
       and records nothing; so is --withdraw with any other flag of a push)
  spawn mission pushes --source <path> [--commit <hash or prefix>] [--limit N]
      (the source's owner landing requests, newest first (default 20, at most 1000), pending
       and decided: each {request, source, commit, ref, at_ms, gated, urgent, reason, state,
       outcome}, state pending, applied or refused as push --wait says it and outcome its
       record (null while pending); --commit: that commit's requests only. <path> as the
       request named it. Records nothing)
  spawn mission moved --source <path> --from <commit> --to <commit>   (owner: a deploy moved main)
      (recorded as source.moved for every deploy, also when no mission's baseline moves;
       after it every delivery whose retries are spent, having typed nothing, is tried once more)
  spawn mission landing --source <path> [--deployed <commit>]   (owner: what deploy-on-landing
      decides on, #54: {source, main, landing, first_undeployed_ms, last_deploy, hold, now_ms}. main is the
      source's main now; landing the newest gate-accepted landing of exactly that commit on the
      source (a task.landed of a task of a mission on it, or a source.moved with verdict
      accepted), {record, event, at_ms, task, request}, or null; first_undeployed_ms when the
      first such landing after the one that moved main to --deployed was recorded; hold the
      source's hold or null; last_deploy the newest deploy.finished. Records nothing)
  spawn mission deployed --head <commit> --ok true|false --by landing|owner [--reason <text>]
      (owner: how a deploy ended, recorded as deploy.finished: a failure raises the
       deploy_failed alert, the next success clears it. --reason: at most 500 characters)
  spawn mission hold --source <path> [--reason <text>]   (owner: no task lands on the source until
      a record ends the hold: its release, or its expiry SPAWN_SOURCE_HOLD_SECS after it (1 to
      86400, default 10800); its gates still run and its accepted tasks wait; a hold on a held
      source renews it. Prints the hold and the main it was taken at: gate against that main)
  spawn mission release --source <path>   (owner: the waiting tasks land at the next pass; with
      no hold it records nothing and prints `released` null)
  spawn mission list [--limit N]   (every task with its stages)
  spawn mission show --id N [--root <workspace root>]
      (one mission by id, including one outside list's window: its resolved source and mirror,
      actual landing repository, gate and backlog paths, per-open-task delivery/report/gate
      diagnostics, and shell-quoted read-only checks. Prints JSON and creates nothing)
  spawn mission close --id N --state done|dropped [--summary <text>]
  spawn task assign --mission N --kind ephemeral|resident --worker <label|resident>
                    {--brief <text|-> | --brief-file <path>} [--model default|cheap]
                    [--engine <claude|agy>] [--model-label <label>] [--again] [--pin]
                    [--difficulty l1|l2|l3] [--preset <preset_id>]
      (the supervisor's brief that a task of the mission in flight already has, trimmed of
       space, tab, CR and LF, is refused, naming that task; --again assigns it on purpose.
       Every duplicate assigned names the task it repeats in its record: duplicate_of.
       --pin keeps the task with its worker: it is never stolen.
       --engine selects an enabled engine directly (bypassing grading catalog routing);
       --model-label sets that engine's launch model label (e.g. gemini-3.8-flash-low for agy).
       Agent grading (docs/agent-grading.md): --difficulty or --preset routes the task
       through the grading catalog (spawn grade): to --preset, or to the cheapest routable
       preset graded for the difficulty (l2 unless given), on that preset's --model profile;
       no such preset is an error. Without either, an ephemeral task's difficulty is
       classified from its brief (security or migration work, many files or crates, many
       steps: l3; a small low-risk change: l1; otherwise l2; the rules and their evidence
       are recorded on task.assigned as classification, and answered when any matched) and
       routed there when some routable preset is graded for it, else at l2; a preset that
       has had no task at its grade for SPAWN_GRADING_REPROBE_SECS (default 21600, 0: never)
       is given one as a re-probe (reprobe). With nothing graded for it nor l2 it is
       assigned on --model default as before, saying so (routing_fallback); a resident
       task, or one made from an objective, stays on --model. --model keeps the catalog out, and is refused with --difficulty or --preset)
  spawn task accept --id N [--revision R]
  spawn task reject --id N [--revision R] --note <what to fix>
      (R: the report's revision as read; refused once the task has moved on. A task routed
       through the grading catalog whose rework steps up past its preset's grade (an l1 task
       at its 1st and 3rd quality rejection, an l2 task at its 2nd; a gate's conflict is no
       quality rejection) is rerouted to the preset graded for it, in the same transaction:
       task.rerouted. A gate's rejected verdict steps rework up the same way, in the
       statement that records it; a conflict, an incompatible head, a host fault or a
       timeout never does)
  spawn task cancel --id N
      (a delivered task's resident is stopped at once, its work set aside on its branch,
       and it is told before its next task; an ephemeral worker is stopped)
  spawn task release --id N --reason <text> [--by <name>]
      (a task the integrator parked, load-sensitive twice on one pushed commit, is judged again)
  spawn task restart --id N [--reason <text>]
      (a delivered task's worker gets a fresh execution: its running one is stopped, its final
       checkpoint kept, and its native session resumed on its workspace. Prints the task and
       worker that carry the work; any task not delivered is refused, naming its state)
  spawn task reassign --id N [--reason <text>]
      (a delivered task moves to a fresh ephemeral worker whose branch starts from the old
       worker's last checkpoint: the task is cancelled, a new one assigned, the old worker
       stopped. Prints the new task and its worker; any task not delivered is refused)
  spawn task steal --id N --to <resident> [--reason <text>]
      (owner: a queued, unstarted task of a busy resident moves to an idle resident of the
       same pool, recorded as task.stolen; refused, naming the rule, otherwise)
  spawn task stages --id N   (each stage with its verdict, time, and how long since the one before)
  spawn resident propose --name <name> --charter <text> [--pool <pool>]
  spawn resident approve --name <name>
  spawn resident retire --name <name>";

/// The bound of an owner's hold: `SPAWN_SOURCE_HOLD_SECS`, 3 h by default
/// (longer than the slowest owner gate, 80 minutes, plus a deploy, and than
/// the gate's default wall clock, 120 minutes).
const HOLD_SECS_DEFAULT: i32 = 10_800;

/// The longest bound a hold may have: a day.
const HOLD_SECS_MAX: i32 = 86_400;

/// The longest reason a hold may give.
const HOLD_REASON_CHARS: usize = 500;

/// A hold's bound in seconds from `SPAWN_SOURCE_HOLD_SECS` (unset or empty:
/// the default): a whole number from 1 to [`HOLD_SECS_MAX`]. Any other value
/// is refused, naming the variable, never replaced by the default.
fn hold_bound(text: Option<&str>) -> Result<i32, String> {
    let Some(text) = text else {
        return Ok(HOLD_SECS_DEFAULT);
    };
    text.parse::<i32>()
        .ok()
        .filter(|secs| (1..=HOLD_SECS_MAX).contains(secs))
        .ok_or_else(|| {
            format!(
                "SPAWN_SOURCE_HOLD_SECS is a whole number of seconds from 1 to {HOLD_SECS_MAX}, \
                 not '{text}'"
            )
        })
}

/// `spawn mission landing --source <source> [--deployed <commit>]`: what
/// deploy/on-landing.sh decides on (#54; [`spawn_postgres::PostgresStore::main_landing`]),
/// with the source's main read now.
fn landing(store: &spawn_postgres::PostgresStore, flags: &Flags) -> Result<Value, String> {
    let source = text(flags, "source")?;
    crate::sources::MissionSource::parse(&source)?;
    let deployed = flags.get("deployed");
    if let Some(commit) = &deployed
        && !is_commit(commit)
    {
        return Err(format!("--deployed is a full commit id, not '{commit}'"));
    }
    let (_, main) = crate::integrate::source_head(&source)?;
    let mut read = store
        .main_landing(&source, &main, deployed.as_deref())
        .map_err(|error| error.to_string())?;
    if let Some(object) = read.as_object_mut() {
        object.insert("source".to_string(), json!(source));
        object.insert("main".to_string(), json!(main));
    }
    Ok(read)
}

/// `spawn mission deployed --head <commit> --ok true|false --by landing|owner
/// [--reason <text>]`: how a deploy ended, one `deploy.finished` (the store
/// checks its shape; `spawn_deploy_record`). Prints `{event}`.
fn deployed(store: &spawn_postgres::PostgresStore, flags: &Flags) -> Result<Value, String> {
    let head = text(flags, "head")?;
    if !is_commit(&head) {
        return Err(format!("--head is a full commit id, not '{head}'"));
    }
    let ok = match text(flags, "ok")?.as_str() {
        "true" => true,
        "false" => false,
        other => return Err(format!("--ok is true or false, not '{other}'")),
    };
    let by = text(flags, "by")?;
    if by != "landing" && by != "owner" {
        return Err(format!("--by is landing or owner, not '{by}'"));
    }
    let reason: String = flags
        .get("reason")
        .unwrap_or_default()
        .chars()
        .take(500)
        .collect();
    let event = store
        .deploy_record(
            &json!({"type": "deploy.finished", "ok": ok, "head": head, "by": by,
                               "reason": reason}),
        )
        .map_err(|error| error.to_string())?;
    Ok(json!({ "event": event }))
}

/// A full commit id: 40 lowercase hex digits.
fn is_commit(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `spawn mission hold --source <source> [--reason <text>]`, its bound from
/// `SPAWN_SOURCE_HOLD_SECS` ([`hold_bound`]): the hold as printed.
fn hold(store: &spawn_postgres::PostgresStore, flags: &Flags) -> Result<Value, String> {
    let source = text(flags, "source")?;
    let bound = hold_bound(crate::config::env_value("SPAWN_SOURCE_HOLD_SECS").as_deref())?;
    let reason = flags.get("reason");
    if let Some(reason) = &reason
        && reason.chars().count() > HOLD_REASON_CHARS
    {
        return Err(format!(
            "--reason is at most {HOLD_REASON_CHARS} characters, not {}",
            reason.chars().count()
        ));
    }
    hold_source(store, &source, bound, reason.as_deref())
}

/// Hold landings on `source` for `bound` seconds (`spawn mission hold`):
/// under the integrator's landing lock, so a landing whose push has begun
/// records its outcome first, the main read here is the one it left, and no
/// landing starts before the hold is recorded. The hold as printed:
/// `{hold, source, main, expires_ms, renews}`.
pub(crate) fn hold_source(
    store: &spawn_postgres::PostgresStore,
    source: &str,
    bound: i32,
    reason: Option<&str>,
) -> Result<Value, String> {
    let lock = store
        .source_hold_lock(source)
        .map_err(|error| error.to_string())?;
    let (_, main) = crate::integrate::source_head(source)?;
    let held = store
        .source_hold(source, &main, bound, reason)
        .map_err(|error| error.to_string())?;
    drop(lock);
    Ok(held)
}

/// Shell-quote one argument with single quotes. Embedded quotes close the
/// quoted word, add an escaped quote, and open it again (`'` -> `'\''`).
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// A stage without gate environment bindings: what an operator needs, never
/// an environment dump.
fn stage_view(stage: &Value) -> Value {
    json!({
        "stage": stage["stage"],
        "verdict": stage["verdict"],
        "at_ms": stage["at_ms"],
        "took_ms": stage["took_ms"],
    })
}

fn gate_view(gate: &Value) -> Value {
    if gate.is_null() {
        return Value::Null;
    }
    json!({
        "verdict": gate["verdict"],
        "reason": gate["reason"],
        "metric": gate["metric"],
        "candidate": gate["candidate"],
        "at_ms": gate["at_ms"],
    })
}

/// Operator diagnostics for a delivered task: filesystem report status plus
/// report-blocker facts when the caller's database login permits reading them.
fn delivered_task_diagnostics(
    task: &Value,
    task_id: i64,
    root: &std::path::Path,
    found: Option<&crate::deliver::TaskRow>,
    facts: Option<&crate::dispatch::Facts>,
    dispatcher_available: bool,
) -> Value {
    let key = found.map_or_else(
        || {
            format!(
                "deliver-{task_id}-{}",
                task["revisions"].as_i64().unwrap_or(0)
            )
        },
        crate::deliver::TaskRow::delivery_key,
    );
    let workspace = root.join(task["agent_id"].as_str().unwrap_or_default());
    let report = workspace.join(crate::deliver::REPORT);
    let report_fs = json!({
        "path": report,
        "status": crate::deliver::report_status(&workspace, &key).as_str(),
    });
    if !dispatcher_available {
        return json!({
            "report_blocker": Value::Null,
            "facts_unavailable": "restricted login",
            "report": report_fs,
            "turn_ended": Value::Null,
            "turn_seq": Value::Null,
            "checkpoint": Value::Null,
            "observation": Value::Null,
            "observation_current": false,
        });
    }
    let blocker = facts.map_or(
        Some("no session facts"),
        crate::dispatch::Facts::report_blocker,
    );
    json!({
        "report_blocker": blocker,
        "report": report_fs,
        "turn_ended": facts.map(|facts| facts.turn_ended),
        "turn_seq": facts.and_then(|facts| facts.turn_seq),
        "checkpoint": facts.and_then(|facts| facts.checkpoint.as_deref()),
        "observation": facts.and_then(|facts| facts.observation.as_ref())
            .map(|observation| observation.state.key()),
        "observation_current": facts
            .is_some_and(|facts| facts.current_observation().is_some()),
    })
}

/// One task and the diagnostics appropriate to its state. Every lookup is a
/// read: in particular a delivered task's report is inspected, never moved,
/// and no missing workspace path is made.
fn mission_task_view(
    task: &Value,
    root: &std::path::Path,
    dispatcher: Option<(&[crate::dispatch::Facts], &crate::deliver::Work)>,
    last_gates: Option<&Value>,
    now_ms: i64,
) -> Value {
    let task_id = task["id"].as_i64().unwrap_or_default();
    let state = task["state"].as_str().unwrap_or("unknown");
    let stages: Vec<Value> = task["stages"]
        .as_array()
        .into_iter()
        .flatten()
        .map(stage_view)
        .collect();
    let mut view = json!({
        "id": task["id"],
        "kind": task["kind"],
        "agent_id": task["agent_id"],
        "session_id": task["session_id"],
        "status": state,
        "revisions": task["revisions"],
        "candidate": task["candidate"],
        "objective": task["objective"],
        "stages": stages,
    });
    let found = dispatcher.and_then(|(_, work)| work.tasks.iter().find(|row| row.id == task_id));
    let facts = dispatcher.and_then(|(rows, _)| {
        rows.iter()
            .find(|facts| Some(facts.session_id.as_str()) == task["session_id"].as_str())
    });
    let diagnostics = match state {
        "assigned" | "revise" => match dispatcher {
            Some((_, work)) => found.map(|row| {
                let status = crate::deliver::delivery_status(work, row, facts, now_ms);
                json!({"delivery": {"given_up": status.given_up, "line": status.line}})
            }),
            None => Some(json!({"delivery": {"unavailable": "restricted login"}})),
        },
        "delivered" => Some(delivered_task_diagnostics(
            task,
            task_id,
            root,
            found,
            facts,
            dispatcher.is_some(),
        )),
        "reported" | "accepted" | "landing" => {
            let latest_landing = stages
                .iter()
                .rev()
                .find(|stage| stage["stage"] == "landed")
                .cloned();
            let latest_gate = match last_gates {
                Some(gates) => gate_view(gates.get(task_id.to_string()).unwrap_or(&Value::Null)),
                None => stages
                    .iter()
                    .rev()
                    .find(|stage| stage["stage"] == "gated")
                    .cloned()
                    .unwrap_or(Value::Null),
            };
            let mut diag = json!({
                "latest_gate": latest_gate,
                "landing": {
                    "candidate": task["candidate"],
                    "latest": latest_landing,
                },
            });
            if last_gates.is_none()
                && let Some(object) = diag.as_object_mut()
            {
                object.insert(
                    "gate_details_unavailable".to_string(),
                    json!("restricted login"),
                );
            }
            Some(diag)
        }
        _ => None,
    };
    if let (Some(diagnostics), Some(object)) = (diagnostics, view.as_object_mut()) {
        object.insert("diagnostics".to_string(), diagnostics);
    }
    view
}

/// The read-only operator projection. `runner` is resolved by the caller from
/// `SPAWN_GATE`; this function never reads the process environment.
fn mission_view(
    mission: &Value,
    root: &std::path::Path,
    runner: &std::path::Path,
    dispatcher: Option<(&[crate::dispatch::Facts], &crate::deliver::Work)>,
    last_gates: Option<&Value>,
    now_ms: i64,
) -> Result<Value, String> {
    let configured = mission["sources"].clone();
    let resolved = configured
        .as_array()
        .and_then(|sources| sources.first())
        .and_then(Value::as_str);
    let mirror = crate::sources::mirror_path(root, resolved).map_err(|error| error.to_string())?;
    let scenarios = mission["gate"].as_str();
    let backlog = mission["objectives"]["backlog"].as_str();
    let landing_kind = match resolved {
        None => "scratch",
        Some(source) if source.starts_with('/') && !source.contains("://") => "host-path",
        Some(_) => "remote",
    };
    let tasks: Vec<Value> = mission["tasks"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|task| mission_task_view(task, root, dispatcher, last_gates, now_ms))
        .collect();
    let quoted_mirror = shell_quote(&mirror.to_string_lossy());
    let quoted_runner = shell_quote(&runner.to_string_lossy());
    let mut checks = vec![
        format!("git --git-dir {quoted_mirror} rev-parse --verify refs/heads/main"),
        format!("git --git-dir {quoted_mirror} log -1 --oneline refs/heads/main"),
        format!("test -f {quoted_runner} && test -r {quoted_runner}"),
    ];
    if let Some(source) = resolved {
        checks.push(format!(
            "git ls-remote --symref -- {} HEAD refs/heads/main",
            shell_quote(source)
        ));
        checks.push(format!(
            "spawn mission landing --source {}",
            shell_quote(source)
        ));
    }
    if let Some(directory) = scenarios {
        checks.push(format!("test -d {}", shell_quote(directory)));
    }
    if let Some(directory) = backlog {
        checks.push(format!("test -d {}", shell_quote(directory)));
    }
    checks.extend(tasks.iter().filter_map(|task| {
        let open = matches!(
            task["status"].as_str(),
            Some("assigned" | "revise" | "delivered" | "reported" | "accepted" | "landing")
        );
        open.then(|| {
            format!(
                "spawn task stages --id {}",
                task["id"].as_i64().unwrap_or(0)
            )
        })
    }));
    Ok(json!({
        "mission": mission["id"],
        "state": mission["state"],
        "source": {"configured": configured, "resolved": resolved},
        "workspace_root": root,
        "mirror": {"path": mirror, "exists": mirror.exists()},
        // The integrator pushes to the source itself, never to this mirror.
        "landing_repo": {"target": resolved, "kind": landing_kind},
        "gate": {
            "scenarios_dir": scenarios,
            "exists": scenarios.is_some_and(|path| std::path::Path::new(path).is_dir()),
            "runner": runner,
        },
        "backlog": {
            "directory": backlog,
            "exists": backlog.is_some_and(|path| std::path::Path::new(path).is_dir()),
        },
        "tasks": tasks,
        "checks": checks,
    }))
}

/// `spawn mission show --id N [--root <dir>]`: the source and filesystem an
/// operator actually needs to inspect, plus the dispatcher's own per-task
/// blockers. Direct id lookup keeps missions beyond `mission list`'s window
/// visible. No mirror is ensured and no filesystem object is created.
fn show(store: &spawn_postgres::PostgresStore, flags: &Flags) -> Result<Value, String> {
    use spawn_core::time::{Clock, SystemClock};
    let id = id(flags, "id")?;
    let root = crate::config::workspace_root(flags)
        .ok_or_else(|| "--root or SPAWN_WORKSPACE_ROOT is required".to_string())?;
    let runner = std::path::absolute(crate::env_or("SPAWN_GATE", "scripts/rsi/gate.sh"))
        .map_err(|error| format!("SPAWN_GATE: {error}"))?;
    let mission = store
        .mission_by_id(id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("mission #{id} not found"))?;
    let dispatcher = crate::dispatch::load_if_permitted(store, crate::deliver::Waits::default())?;
    let last_gates = match store.last_gates() {
        Ok(gates) => Some(gates),
        Err(spawn_core::port::StoreError::Rejected(_)) => None,
        Err(error) => return Err(error.to_string()),
    };
    let mut view = mission_view(
        &mission,
        std::path::Path::new(&root),
        &runner,
        dispatcher
            .as_ref()
            .map(|(rows, work)| (rows.as_slice(), work)),
        last_gates.as_ref(),
        Clock::now(&SystemClock).0,
    )?;
    // Read, never written: no root made, no probe tried (a read command).
    view["workspace_backend"] = crate::config::WorkspaceSettings::peek(std::path::Path::new(&root));
    Ok(view)
}

fn print(value: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_string())
    );
}

fn id(flags: &Flags, name: &str) -> Result<i64, String> {
    let text = flags
        .get(name)
        .ok_or_else(|| format!("--{name} is required\n{USAGE}"))?;
    text.parse()
        .map_err(|_| format!("--{name} must be a number, not {text}"))
}

fn text(flags: &Flags, name: &str) -> Result<String, String> {
    flags
        .get(name)
        .ok_or_else(|| format!("--{name} is required\n{USAGE}"))
}

/// Brief text for `spawn task assign`: either `--brief <text|->` (where `-`
/// reads stdin) or `--brief-file <path>`, mutually exclusive and non-empty
/// after trimming.
fn brief_text(flags: &Flags) -> Result<String, String> {
    brief_text_with(flags, || {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
            .map_err(|error| format!("--brief -: cannot read stdin: {error}"))?;
        Ok(buf)
    })
}

fn brief_text_with(
    flags: &Flags,
    read_stdin: impl FnOnce() -> Result<String, String>,
) -> Result<String, String> {
    let raw = match (flags.get("brief"), flags.get("brief-file")) {
        (Some(_), Some(_)) => return Err("give --brief or --brief-file, not both".to_string()),
        (None, None) => return Err(format!("--brief or --brief-file is required\n{USAGE}")),
        (Some(val), None) if val == "-" => read_stdin()?,
        (Some(val), None) => val,
        (None, Some(path)) => std::fs::read_to_string(&path)
            .map_err(|error| format!("--brief-file {path}: cannot read: {error}"))?,
    };
    if raw.trim().is_empty() {
        return Err(format!("--brief must not be empty\n{USAGE}"));
    }
    Ok(raw)
}

/// The `--revision` of the report a review judged, if named: without it,
/// the report as it is now.
fn revision(flags: &Flags) -> Result<Option<i32>, String> {
    flags
        .get("revision")
        .map(|text| {
            text.parse()
                .map_err(|_| format!("--revision must be a number, not {text}"))
        })
        .transpose()
}

/// The `--model` profile a task runs on, `default` when unset.
fn model(flags: &Flags) -> Result<ModelProfile, String> {
    let Some(text) = flags.get("model") else {
        return Ok(ModelProfile::Default);
    };
    ModelProfile::parse(&text)
        .ok_or_else(|| format!("--model is default or cheap, not {text}\n{USAGE}"))
}

fn direct_engine(flags: &Flags) -> Result<Option<String>, String> {
    let Some(raw) = flags.get("engine") else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    let known = spawn_core::driver::driver(trimmed).ok_or_else(|| {
        format!("--engine must be a registered engine (claude, agy), not {raw:?}")
    })?;
    Ok(Some(known.id().to_string()))
}

fn direct_model_label(flags: &Flags, engine: Option<&str>) -> Result<Option<String>, String> {
    let Some(raw) = flags.get("model-label") else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() || !spawn_core::command::is_model_label(trimmed) {
        return Err(format!(
            "--model-label {raw:?} is not a valid engine model label"
        ));
    }
    if engine == Some(spawn_core::driver::DEFAULT) {
        return Err("claude uses --model default|cheap and takes no --model-label".to_string());
    }
    if engine.is_none() {
        return Err("--model-label requires --engine (e.g. --engine agy)".to_string());
    }
    Ok(Some(trimmed.to_string()))
}

/// How `spawn task assign` places a task (docs/agent-grading.md,
/// "Assigning").
enum Placement {
    /// As before agent grading: on `--model` (`default` unless given).
    /// `fallback`: why an unflagged ephemeral assignment did not route
    /// through a catalog that holds presets.
    Model { fallback: Option<String> },
    /// Through the grading catalog; with the difficulty classifier's
    /// decision when the assigner named neither a difficulty nor a preset.
    Routed(RoutingDecision, Option<DifficultyClassification>),
}

/// `spawn task assign`: through the grading catalog when [`placement`]
/// says so, else exactly as before agent grading. Prints the task and its
/// model (routed: its difficulty, preset and the preset's grade; fallen
/// back: why, as `routing_fallback`, also said on stderr).
pub(crate) fn assign(
    store: &spawn_postgres::PostgresStore,
    flags: &Flags,
) -> Result<Value, String> {
    let err = |error: spawn_core::port::StoreError| error.to_string();
    let direct_engine = direct_engine(flags)?;
    let direct_label = direct_model_label(flags, direct_engine.as_deref())?;
    if direct_engine.is_some() || direct_label.is_some() {
        if flags.get("difficulty").is_some() || flags.get("preset").is_some() {
            return Err(
                "--engine / --model-label bypasses the grading catalog: do not combine with \
                 --difficulty or --preset"
                    .to_string(),
            );
        }
        let model = model(flags)?;
        let brief = brief_text(flags)?;
        let task = store
            .task_assign_direct(
                id(flags, "mission")?,
                &text(flags, "kind")?,
                &flags.get("worker").unwrap_or_default(),
                &brief,
                model.as_str(),
                flags.switch("again"),
                flags.switch("pin"),
                direct_engine.as_deref(),
                direct_label.as_deref(),
            )
            .map_err(err)?;
        let mut answer = json!({ "task": task, "model": model.as_str() });
        if let Some(engine) = direct_engine {
            answer["engine"] = json!(engine);
        }
        if let Some(label) = direct_label {
            answer["model_label"] = json!(label);
        }
        return Ok(answer);
    }
    let brief = brief_text(flags)?;
    let (decision, classified) = match placement(store, flags, &brief)? {
        Placement::Routed(decision, classified) => (decision, classified),
        Placement::Model { fallback } => {
            let model = model(flags)?;
            let task = store
                .task_assign_with(
                    id(flags, "mission")?,
                    &text(flags, "kind")?,
                    &flags.get("worker").unwrap_or_default(),
                    &brief,
                    model.as_str(),
                    flags.switch("again"),
                    flags.switch("pin"),
                )
                .map_err(err)?;
            let mut answer = json!({ "task": task, "model": model.as_str() });
            if let Some(why) = fallback {
                eprintln!("spawn: agent grading: task {task}: {why}");
                answer["routing_fallback"] = json!(why);
            }
            return Ok(answer);
        }
    };
    let classification = classified
        .as_ref()
        .map(|classified| crate::grading::classification_record(classified, &decision));
    let task = store
        .task_assign_classified(
            id(flags, "mission")?,
            &text(flags, "kind")?,
            &flags.get("worker").unwrap_or_default(),
            &brief,
            decision.model_profile.as_str(),
            flags.switch("again"),
            flags.switch("pin"),
            decision.difficulty,
            Some(&decision.preset_id),
            classification.as_ref(),
        )
        .map_err(err)?;
    let mut answer = json!({ "task": task, "model": decision.model_profile.as_str(),
                             "difficulty": decision.difficulty, "preset_id": decision.preset_id,
                             "preset_grade": decision.preset_grade });
    // Phase 2's additions, each only when it says something, so a phase-1
    // answer reads as it did.
    if let Some(profile) = &decision.execution_profile {
        answer["execution_profile"] = json!(profile);
    }
    for note in route_notes(&format!("task {task}"), &decision, classified.as_ref()) {
        eprintln!("spawn: agent grading: {note}");
    }
    if let (Some(classified), Some(record)) = (&classified, classification)
        && classified.has_signal()
    {
        answer["classification"] = record;
    }
    if decision.reprobe {
        answer["reprobe"] = json!(true);
    }
    Ok(answer)
}

/// Where `spawn task assign` places a task (docs/agent-grading.md,
/// "Assigning"):
///
/// - `--model`: on it, as before; refused with `--difficulty` or
///   `--preset`.
/// - `--difficulty` or `--preset`: through the catalog, strictly: a catalog
///   that cannot take the task is an error.
/// - neither, `--kind resident`: on `--model default`, as before, so the
///   catalog never moves a resident between model profiles task by task.
/// - neither, an ephemeral task: its difficulty classified from its brief
///   ([`classify_difficulty`]) and routed through the catalog at it when
///   some routable preset is graded for it, else at `l2` when some is
///   graded `l2` or above, with the anti-starvation re-probe
///   ([`route_with_reprobe`]); otherwise on `--model default`, as before,
///   and when the catalog holds presets, saying why. An unflagged
///   assignment (the supervisor's) is never stopped by what the catalog
///   holds.
fn placement(
    store: &spawn_postgres::PostgresStore,
    flags: &Flags,
    brief: &str,
) -> Result<Placement, String> {
    let difficulty = flags
        .get("difficulty")
        .map(|text| {
            TaskDifficulty::parse(&text)
                .ok_or_else(|| format!("--difficulty is l1, l2 or l3, not {text}\n{USAGE}"))
        })
        .transpose()?;
    let preset = flags.get("preset");
    if flags.get("model").is_some() {
        if difficulty.is_some() || preset.is_some() {
            return Err(
                "--model keeps the grading catalog out: give it, or --difficulty and --preset, \
                 not both"
                    .to_string(),
            );
        }
        return Ok(Placement::Model { fallback: None });
    }
    let explicit = difficulty.is_some() || preset.is_some();
    if !explicit && flags.get("kind").as_deref() == Some("resident") {
        return Ok(Placement::Model { fallback: None });
    }
    if explicit {
        let catalog = store.grading_catalog().map_err(|error| error.to_string())?;
        return route_preset(
            difficulty.unwrap_or_default(),
            0,
            preset.as_deref(),
            &catalog,
        )
        .map(|decision| Placement::Routed(decision, None))
        .map_err(|error| format!("agent grading: {error}"));
    }
    Ok(match route_unflagged(store, brief, "--model default")? {
        CatalogRoute::Routed(decision, classified) => Placement::Routed(decision, Some(classified)),
        CatalogRoute::Unrouted { fallback } => Placement::Model { fallback },
    })
}

/// Where agent grading places a task its assigner named neither a
/// difficulty nor a preset for ([`route_unflagged`]).
pub(crate) enum CatalogRoute {
    /// Through the catalog, with the classifier's decision on its text.
    Routed(RoutingDecision, DifficultyClassification),
    /// Not through the catalog, as before agent grading. `fallback`: why,
    /// when the catalog holds presets (none when it is empty: nothing to
    /// say).
    Unrouted { fallback: Option<String> },
}

/// The route of an unflagged task (`spawn task assign` without
/// `--difficulty`, `--preset` or `--model` for an ephemeral worker, and
/// every objective the integrator assigns, `crate::objectives`), the one
/// rule for both: its difficulty classified from `text`
/// ([`classify_difficulty`]) and routed through the catalog at it when
/// some routable preset is graded for it, else at `l2` when some is graded
/// `l2` or above, with the anti-starvation re-probe
/// ([`route_with_reprobe`]); otherwise unrouted, on `unrouted_as` (what the
/// fallback reason names it), and when the catalog holds presets, saying
/// why. An empty catalog is never an error.
pub(crate) fn route_unflagged(
    store: &spawn_postgres::PostgresStore,
    text: &str,
    unrouted_as: &str,
) -> Result<CatalogRoute, String> {
    let catalog = store.grading_catalog().map_err(|error| error.to_string())?;
    if catalog.is_empty() {
        return Ok(CatalogRoute::Unrouted { fallback: None });
    }
    // Classified first: an `l1` task reaches an `l1` preset (a demoted one
    // included, refilling its window) even when nothing covers `l2`.
    let classified = classify_difficulty(text);
    let Some(target) = routed_difficulty(&catalog, classified.difficulty) else {
        return Ok(CatalogRoute::Unrouted {
            fallback: Some(format!(
                "no routable preset in the grading catalog is graded {} or above: \
                 assigned on {unrouted_as}, as without the catalog",
                classified
                    .difficulty
                    .max(TaskDifficulty::default())
                    .required_grade()
            )),
        });
    };
    let activity = store
        .grading_activity()
        .map_err(|error| error.to_string())?;
    route_with_reprobe(
        target,
        &catalog,
        &activity,
        crate::grading::now_ms(),
        crate::grading::reprobe_interval_ms(),
    )
    .map(|decision| CatalogRoute::Routed(decision, classified))
    .map_err(|error| format!("agent grading: {error}"))
}

/// The difficulty an unflagged task `classified` at is routed at: its own
/// when the catalog covers it, else the default (`l2`) when that is
/// covered, as phase 1 routed every unflagged task (the record keeps what
/// the classifier said beside the difficulty it ran at); none when neither
/// is.
fn routed_difficulty(
    catalog: &[GradedPreset],
    classified: TaskDifficulty,
) -> Option<TaskDifficulty> {
    let default = TaskDifficulty::default();
    if catalog_covers(catalog, classified) {
        Some(classified)
    } else if catalog_covers(catalog, default) {
        Some(default)
    } else {
        None
    }
}

/// What a routed assignment says of its route on stderr (`subject`: `task
/// 7`, ...): the classifier's rationale when it found a signal, and a
/// re-probe when it is one. Shared by `spawn task assign` and the
/// integrator's objective assignment.
pub(crate) fn route_notes(
    subject: &str,
    decision: &RoutingDecision,
    classified: Option<&DifficultyClassification>,
) -> Vec<String> {
    let mut notes = Vec::new();
    if let Some(classified) = classified.filter(|classified| classified.has_signal()) {
        notes.push(format!("{subject}: classified {}", classified.rationale()));
    }
    if decision.reprobe {
        notes.push(format!(
            "{subject}: a re-probe of preset {}, which had no {} task for the re-probe interval",
            decision.preset_id, decision.difficulty
        ));
    }
    notes
}

/// `spawn task reject`: send the task back, rerouting its rework when it
/// steps up past its preset ([`rework_reroute`]) in the same statement.
pub(crate) fn reject(
    store: &spawn_postgres::PostgresStore,
    flags: &Flags,
) -> Result<Value, String> {
    let err = |error: spawn_core::port::StoreError| error.to_string();
    let task = id(flags, "id")?;
    let note = text(flags, "note")?;
    let revision = revision(flags)?;
    let reroute = rework_reroute(store, task)?;
    if let Some(ReworkReroute {
        route: Ok(decision),
        revision: next,
        model,
    }) = &reroute
    {
        let rerouted = store
            .task_reject_rerouted(
                task,
                &note,
                revision,
                &decision.preset_id,
                *next,
                decision.routed_difficulty,
            )
            .map_err(err)?;
        let mut answer = json!({ "task": task, "sent_back": true, "rerouted": rerouted });
        if decision.model_profile.as_str() == model {
            // Phase 1 runs a worker on its ModelProfile alone: the record
            // moves the task's preset, and the worker runs as before.
            eprintln!(
                "spawn: agent grading: task {task} rerouted to preset {} on the same model \
                 profile ({model}): its worker runs as before",
                decision.preset_id
            );
            answer["same_model_profile"] = json!(true);
        }
        return Ok(answer);
    }
    store
        .task_review(task, false, &note, revision)
        .map_err(err)?;
    let mut answer = json!({ "task": task, "sent_back": true });
    // Sent back all the same: the reviewer decided that. The catalog's
    // failure to take the rework is said.
    if let Some(ReworkReroute {
        route: Err(error), ..
    }) = reroute
    {
        answer["reroute_error"] = json!(error.to_string());
    }
    Ok(answer)
}

/// A rework reroute for `spawn task reject` (and a gate's `rejected`,
/// crate::grading `gate_reroute`): where the task's next revision goes (or
/// why nowhere), the revision it is at once sent back, and the model
/// profile it runs on now.
pub(crate) struct ReworkReroute {
    pub(crate) route: Result<RoutingDecision, RoutingError>,
    pub(crate) revision: i32,
    pub(crate) model: String,
}

/// The rework reroute of a reported task routed through the catalog
/// (docs/agent-grading.md): routed at its difficulty stepped up for its
/// quality rejections, this one counted. None when its preset still takes
/// that (routable and graded for it), or the task is not routed or not
/// reported (the review itself refuses that).
fn rework_reroute(
    store: &spawn_postgres::PostgresStore,
    task: i64,
) -> Result<Option<ReworkReroute>, String> {
    quality_reroute(store, task, &["reported"])
}

/// [`rework_reroute`] for a task about to be sent back for its quality
/// from any of `states` (a review: `reported`; a gate's `rejected`:
/// `reported`, or `accepted` in a mission a review accepts before its
/// gate). The same transactional rule either way: the caller records the
/// send-back and the reroute in one statement.
pub(crate) fn quality_reroute(
    store: &spawn_postgres::PostgresStore,
    task: i64,
    states: &[&str],
) -> Result<Option<ReworkReroute>, String> {
    let Some(routed) = store.routed_task(task).map_err(|error| error.to_string())? else {
        return Ok(None);
    };
    let Some(current) = routed.preset_id.as_deref() else {
        return Ok(None);
    };
    if !states.contains(&routed.state.as_str()) {
        return Ok(None);
    }
    let rejections = u32::try_from(routed.quality_rejections.saturating_add(1)).unwrap_or(1);
    let catalog = store.grading_catalog().map_err(|error| error.to_string())?;
    if preset_holds(current, routed.difficulty, rejections, &catalog) {
        return Ok(None);
    }
    Ok(Some(ReworkReroute {
        route: route_preset(routed.difficulty, rejections, None, &catalog),
        revision: routed.revisions.saturating_add(1),
        model: routed.model,
    }))
}

/// Where the continuation of `task` (an objective's budget overrun,
/// `crate::objectives`) runs when `task` was routed through the catalog:
/// on `task`'s own preset while it still takes `task`'s rework (routable
/// and graded for its difficulty stepped up for its quality rejections),
/// else on the minimum-sufficient preset at that stepped-up difficulty, so
/// a continuation never falls back below where its rework had stepped up.
/// `spawn_objective_assign` carries `task`'s difficulty, first preset and
/// quality rejections with it. None when `task` was not routed (or does
/// not exist): the continuation is routed afresh; Some(Err) when no preset
/// takes it.
pub(crate) fn continuation_route(
    store: &spawn_postgres::PostgresStore,
    task: i64,
) -> Result<Option<Result<RoutingDecision, RoutingError>>, String> {
    let Some(routed) = store.routed_task(task).map_err(|error| error.to_string())? else {
        return Ok(None);
    };
    let Some(current) = routed.preset_id.as_deref() else {
        return Ok(None);
    };
    let rejections = u32::try_from(routed.quality_rejections).unwrap_or(0);
    let catalog = store.grading_catalog().map_err(|error| error.to_string())?;
    Ok(Some(
        route_preset(routed.difficulty, rejections, Some(current), &catalog)
            .or_else(|_| route_preset(routed.difficulty, rejections, None, &catalog)),
    ))
}

/// Whether the preset `current` still takes a task of `difficulty` after
/// `quality_rejections`: routable and graded for the stepped-up difficulty.
fn preset_holds(
    current: &str,
    difficulty: TaskDifficulty,
    quality_rejections: u32,
    catalog: &[GradedPreset],
) -> bool {
    let required = difficulty.for_rework(quality_rejections).required_grade();
    catalog.iter().any(|preset| {
        preset.preset_id == current
            && preset
                .routing_grade()
                .is_some_and(|grade| grade >= required)
    })
}

fn split(rest: &[String]) -> Result<(&str, Flags), String> {
    let Some((verb, rest)) = rest.split_first() else {
        return Err(USAGE.to_string());
    };
    Ok((verb.as_str(), Flags::parse(rest)?))
}

/// As [`split`], with `configure`'s clearing switches (`--no-gate`, ...),
/// which take no value; every other verb and flag needs one as before.
fn split_mission(rest: &[String]) -> Result<(&str, Flags), String> {
    let Some((verb, rest)) = rest.split_first() else {
        return Err(USAGE.to_string());
    };
    let switches: &[&str] = if verb == "configure" {
        &CLEARING.map(|(switch, _, _)| switch)
    } else if verb == "push" {
        &["gated", "urgent"]
    } else {
        &[]
    };
    Ok((verb.as_str(), Flags::parse_with_switches(rest, switches)?))
}

/// As [`split`], with `assign`'s `--again` (objective 212) and `--pin`
/// (objective 239), which take no value.
fn split_task(rest: &[String]) -> Result<(&str, Flags), String> {
    let Some((verb, rest)) = rest.split_first() else {
        return Err(USAGE.to_string());
    };
    let switches: &[&str] = if verb == "assign" {
        &["again", "pin"]
    } else {
        &[]
    };
    Ok((verb.as_str(), Flags::parse_with_switches(rest, switches)?))
}

/// An absolute path for a directory flag, resolved against the working
/// directory.
fn directory(flags: &Flags, name: &str) -> Result<Option<String>, String> {
    let Some(text) = flags.get(name) else {
        return Ok(None);
    };
    let path = std::path::absolute(&text).map_err(|error| format!("--{name}: {error}"))?;
    if !path.is_dir() {
        return Err(format!("--{name} {} is not a directory", path.display()));
    }
    Ok(Some(path.display().to_string()))
}

/// A mission's landing settings (docs/fleet-architecture.md §10.3-10.4).
#[derive(Debug)]
struct Settings {
    gate: Option<String>,
    accept: String,
    budget: Option<Value>,
    objectives: Option<Value>,
    /// Whether any setting flag was given: `mission add` applies the
    /// settings it is given, and records nothing when it is given none.
    given: bool,
}

impl Settings {
    fn from(flags: &Flags) -> Result<Self, String> {
        let number = |name: &str| -> Result<Option<i64>, String> {
            flags
                .get(name)
                .map(|text| {
                    text.parse()
                        .map_err(|_| format!("--{name} must be a number"))
                })
                .transpose()
        };
        let mut budget = serde_json::Map::new();
        if let Some(minutes) = number("minutes")? {
            budget.insert("minutes".to_string(), json!(minutes));
        }
        if let Some(calls) = number("tool-calls")? {
            budget.insert("tool_calls".to_string(), json!(calls));
        }
        if let Some(usd) = dollars(flags)? {
            budget.insert("daily_usd".to_string(), json!(usd));
        }
        let workers = number("workers")?;
        let features = features_file(flags, "features", 0)?;
        let gate = directory(flags, "gate")?;
        let objectives = match (directory(flags, "backlog")?, flags.get("resident")) {
            (Some(backlog), Some(resident)) => {
                // `spawn_mission_configure` refuses objectives without a
                // gate, and objectives without a gate never land: refuse it
                // here, where `mission add` can say so instead of dropping
                // the settings the command line named.
                if gate.is_none() {
                    return Err(format!(
                        "add with --backlog, --resident, --workers or --features needs \
                         --gate: an objective lands only through a gate\n{USAGE}"
                    ));
                }
                let mut objectives = json!({"backlog": backlog, "resident": resident});
                // Absent means 1, the default: a mission configured without
                // it stores exactly what it did before.
                if let (Some(workers), Some(map)) = (workers, objectives.as_object_mut()) {
                    map.insert("workers".to_string(), json!(workers));
                }
                if let (Some(features), Some(map)) = (features, objectives.as_object_mut()) {
                    map.insert("features".to_string(), json!(features));
                }
                Some(objectives)
            }
            (None, None) if workers.is_none() && features.is_none() => None,
            (None, None) => {
                return Err(format!(
                    "--workers and --features go with --backlog and --resident\n{USAGE}"
                ));
            }
            _ => return Err(format!("--backlog and --resident go together\n{USAGE}")),
        };
        let given = [
            "gate",
            "accept",
            "minutes",
            "tool-calls",
            "daily-usd",
            "backlog",
            "resident",
            "workers",
            "features",
        ]
        .iter()
        .any(|name| flags.get(name).is_some());
        Ok(Self {
            gate,
            accept: flags.get("accept").unwrap_or_else(|| "review".to_string()),
            budget: (!budget.is_empty()).then_some(Value::Object(budget)),
            objectives,
            given,
        })
    }

    fn apply(&self, store: &spawn_postgres::PostgresStore, mission: i64) -> Result<(), String> {
        store
            .mission_configure(
                mission,
                self.gate.as_deref(),
                &self.accept,
                self.budget.as_ref(),
                self.objectives.as_ref(),
            )
            .map_err(|error| error.to_string())
    }
}

/// `--daily-usd`: an amount of US dollars above zero, a plain decimal
/// (`0.50`, `12`).
fn dollars(flags: &Flags) -> Result<Option<f64>, String> {
    let Some(text) = flags.get("daily-usd") else {
        return Ok(None);
    };
    let decimal = !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
        && text.bytes().filter(|byte| *byte == b'.').count() <= 1
        && text.bytes().any(|byte| byte.is_ascii_digit());
    match text.parse::<f64>() {
        Ok(usd) if decimal && usd.is_finite() && usd > 0.0 => Ok(Some(usd)),
        _ => Err(format!(
            "--daily-usd must be an amount of US dollars above zero, not {text}"
        )),
    }
}

/// A mission's feature map: an absolute path to a JSON file, resolved
/// against the working directory and checked now, so a typo or an invalid
/// map is refused when it is configured rather than silently ignored by the
/// page. `mission_id` is 0 when the mission has no id yet (`mission add`):
/// the map may still name its own mission's objectives.
fn features_file(flags: &Flags, name: &str, mission_id: i64) -> Result<Option<String>, String> {
    let Some(text) = flags.get(name) else {
        return Ok(None);
    };
    let path = std::path::absolute(&text).map_err(|error| format!("--{name}: {error}"))?;
    if !path.is_file() {
        return Err(format!("--{name} {} is not a file", path.display()));
    }
    crate::serve::feature_map::validate_file(&path, mission_id)
        .map_err(|error| format!("--{name} {}: {error}", path.display()))?;
    Ok(Some(path.display().to_string()))
}

/// `configure`'s clearing switches (they take no value), and the setting
/// (and key in it) each clears.
const CLEARING: [(&str, &str, Option<&str>); 8] = [
    ("no-gate", "gate", None),
    ("no-budget", "budget", None),
    ("no-minutes", "budget", Some("minutes")),
    ("no-tool-calls", "budget", Some("tool_calls")),
    ("no-daily-usd", "budget", Some("daily_usd")),
    ("no-objectives", "objectives", None),
    ("no-workers", "objectives", Some("workers")),
    ("no-features", "objectives", Some("features")),
];

/// The change `spawn mission configure` asks for (objective 027): only the
/// settings its flags name, for `spawn_mission_reconfigure` to merge into
/// the mission's own under the append lock; each `--no-<setting>` switch
/// sets its setting to null, which clears it. A setting both given and
/// cleared, or a flag configure does not know, is refused. No flag at all is
/// an empty change, which changes and records nothing.
fn change(flags: &Flags) -> Result<Value, String> {
    const KNOWN: [&str; 10] = [
        "id",
        "gate",
        "accept",
        "minutes",
        "tool-calls",
        "daily-usd",
        "backlog",
        "resident",
        "workers",
        "features",
    ];
    if let Some(unknown) = flags.names().find(|name| !KNOWN.contains(name)) {
        return Err(format!(
            "spawn mission configure has no --{unknown}\n{USAGE}"
        ));
    }
    let number = |name: &str| -> Result<Option<i64>, String> {
        flags
            .get(name)
            .map(|text| {
                text.parse()
                    .map_err(|_| format!("--{name} must be a number, not {text}"))
            })
            .transpose()
    };
    let mut change = serde_json::Map::new();
    let mut part = |setting: &str, key: Option<&str>, value: Value| match key {
        None => {
            change.insert(setting.to_string(), value);
        }
        Some(key) => {
            if let Some(map) = change
                .entry(setting.to_string())
                .or_insert_with(|| json!({}))
                .as_object_mut()
            {
                map.insert(key.to_string(), value);
            }
        }
    };
    if let Some(gate) = directory(flags, "gate")? {
        part("gate", None, json!(gate));
    }
    if let Some(accept) = flags.get("accept") {
        part("accept", None, json!(accept));
    }
    if let Some(minutes) = number("minutes")? {
        part("budget", Some("minutes"), json!(minutes));
    }
    if let Some(calls) = number("tool-calls")? {
        part("budget", Some("tool_calls"), json!(calls));
    }
    if let Some(usd) = dollars(flags)? {
        part("budget", Some("daily_usd"), json!(usd));
    }
    if let Some(backlog) = directory(flags, "backlog")? {
        part("objectives", Some("backlog"), json!(backlog));
    }
    if let Some(resident) = flags.get("resident") {
        part("objectives", Some("resident"), json!(resident));
    }
    if let Some(workers) = number("workers")? {
        part("objectives", Some("workers"), json!(workers));
    }
    let mission = flags
        .get("id")
        .and_then(|id| id.parse::<i64>().ok())
        .unwrap_or(0);
    if let Some(features) = features_file(flags, "features", mission)? {
        part("objectives", Some("features"), json!(features));
    }
    // Every value flag and the setting (and key) it writes.
    let written = [
        ("gate", "gate", None),
        ("accept", "accept", None),
        ("minutes", "budget", Some("minutes")),
        ("tool-calls", "budget", Some("tool_calls")),
        ("daily-usd", "budget", Some("daily_usd")),
        ("backlog", "objectives", Some("backlog")),
        ("resident", "objectives", Some("resident")),
        ("workers", "objectives", Some("workers")),
        ("features", "objectives", Some("features")),
    ];
    for (switch, setting, key) in CLEARING {
        if !flags.switch(switch) {
            continue;
        }
        if let Some((flag, _, _)) = written.iter().find(|(flag, written_setting, written_key)| {
            flags.get(flag).is_some()
                && *written_setting == setting
                && (key.is_none() || *written_key == key)
        }) {
            return Err(format!(
                "--{switch} and --{flag} set the same setting: give one\n{USAGE}"
            ));
        }
        part(setting, key, Value::Null);
    }
    Ok(Value::Object(change))
}

/// `spawn mission ...`
/// `spawn mission objectives [--mission N]`: every objective mission's
/// backlog, the owner-judged objectives apart from the fleet's open ones.
fn list_objectives(store: &spawn_postgres::PostgresStore, flags: &Flags) -> Result<Value, String> {
    let only = flags
        .get("mission")
        .map(|text| {
            text.parse::<i64>()
                .map_err(|_| "--mission takes a mission id".to_string())
        })
        .transpose()?;
    let rows: Vec<Value> = crate::serve::objective_missions(store)?
        .into_iter()
        .filter(|mission| only.is_none_or(|id| mission["id"].as_i64() == Some(id)))
        .map(|mission| objectives_row(&mission))
        .collect();
    Ok(json!(rows))
}

/// One mission's backlog split by who judges it: the owner's objectives
/// (`status` "owner": the fleet never assigns them) and the fleet's open
/// ones; solved and met are counts.
fn objectives_row(mission: &Value) -> Value {
    let rows = mission["objectives"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let pick = |status: &str| -> Vec<Value> {
        rows.iter()
            .filter(|row| row["status"] == status)
            .map(|row| json!({"id": row["id"], "title": row["title"]}))
            .collect()
    };
    let count = |status: &str| rows.iter().filter(|row| row["status"] == status).count();
    json!({
        "mission": mission["id"],
        "backlog": mission["backlog"],
        "backlog_error": mission["backlog_error"],
        "owner_judged": pick("owner"),
        "open": pick("open"),
        "solved": count("solved"),
        "met": count("met"),
        "next_objective": mission["next_objective"],
    })
}

pub(crate) fn mission(rest: &[String]) -> Result<i32, String> {
    let (verb, flags) = split_mission(rest)?;
    let store = open_store()?;
    let err = |error: spawn_core::port::StoreError| error.to_string();
    match verb {
        "add" => print(&json!({ "mission": add(&store, &flags)? })),
        "configure" => {
            let mission = id(&flags, "id")?;
            let change = change(&flags)?;
            let settings = store.mission_reconfigure(mission, &change).map_err(err)?;
            print(&settings);
        }
        "pause" => print(&pause(&store, &flags)?),
        "resume" => {
            let mission = id(&flags, "id")?;
            store.mission_resume(mission).map_err(err)?;
            print(&json!({ "mission": mission, "resumed": true }));
        }
        "push" => return push(&store, &flags),
        "pushes" => print(&pushes(&store, &flags)?),
        "moved" => print(&moved(&store, &flags)?),
        "landing" => print(&landing(&store, &flags)?),
        "deployed" => print(&deployed(&store, &flags)?),
        "hold" => print(&hold(&store, &flags)?),
        "release" => {
            let source = text(&flags, "source")?;
            print(&store.source_release(&source).map_err(err)?);
        }
        "objectives" => print(&list_objectives(&store, &flags)?),
        "show" => print(&show(&store, &flags)?),
        "list" => {
            let limit = flags
                .get("limit")
                .and_then(|text| text.parse().ok())
                .unwrap_or(20);
            print(&store.mission_board(limit).map_err(err)?);
        }
        "close" => {
            let mission = id(&flags, "id")?;
            store
                .mission_close(
                    mission,
                    &text(&flags, "state")?,
                    &flags.get("summary").unwrap_or_default(),
                )
                .map_err(err)?;
            print(&json!({ "mission": mission, "closed": true }));
        }
        _ => return Err(USAGE.to_string()),
    }
    Ok(0)
}

/// `spawn mission add`: the mission, then every setting the command line
/// gave it. A setting is never dropped: `Settings::from` refuses objectives
/// without a gate (they could never land), and `given` is true for any
/// other setting so `--accept` or a budget is stored too.
fn add(store: &spawn_postgres::PostgresStore, flags: &Flags) -> Result<i64, String> {
    // Checked here for a clear error; spawn_mission_add holds the same rule
    // for every caller.
    let sources = match flags.get("source") {
        Some(source) => json!([crate::sources::MissionSource::parse(&source)?.as_str()]),
        None => json!([]),
    };
    if let Some(source) = sources.get(0).and_then(Value::as_str) {
        refuse_source_with_unresolved_head(source)?;
    }
    let settings = Settings::from(flags)?;
    // spawn_mission_configure refuses it too, but only once the mission
    // exists.
    if settings.gate.is_some()
        && sources
            .get(0)
            .and_then(Value::as_str)
            .is_some_and(|source| !source.starts_with('/'))
    {
        return Err(
            "a gate lands only on a source that is a directory on this host; agents' \
             branches never reach a remote one"
                .to_string(),
        );
    }
    let id = store
        .mission_add(&text(flags, "text")?, &sources, "human")
        .map_err(|error| error.to_string())?;
    if settings.given {
        settings.apply(store, id)?;
    }
    Ok(id)
}

/// Refuses a local source (an absolute path) whose `HEAD` names no commit
/// while it holds branches: a bare repository made with another default
/// branch (`HEAD` at `refs/heads/master`, only `main` pushed). Every task
/// would later fail on "`<source>` has no HEAD" ([`crate::integrate::source_head`]),
/// so `mission add` says it once, with the repair. A source that holds no
/// branch yet (nothing pushed) is accepted: it is a new project's, and so is
/// one git cannot read here, or a remote one, as before. The error names
/// the branch HEAD points at (`ls-remote` does not show an unborn HEAD, so
/// it is read with `symbolic-ref`) and a repair, `git -C <source>
/// symbolic-ref HEAD <branch>`, that works on a bare and a non-bare source.
fn refuse_source_with_unresolved_head(source: &str) -> Result<(), String> {
    if !source.starts_with('/') {
        return Ok(());
    }
    let Ok(listing) = crate::sources::git(&["ls-remote", "--symref", "--", source]) else {
        return Ok(());
    };
    let mut head_resolves = false;
    let mut head_names = None;
    let mut branches = Vec::new();
    for line in listing.lines() {
        let Some((left, name)) = line.split_once('\t') else {
            continue;
        };
        if let Some(target) = left.strip_prefix("ref: ") {
            if name == "HEAD" {
                head_names = Some(target.to_string());
            }
        } else if name == "HEAD" {
            head_resolves = true;
        } else if name.starts_with("refs/heads/") {
            branches.push(name.to_string());
        }
    }
    if head_resolves || branches.is_empty() {
        return Ok(());
    }
    let suggested = if branches.iter().any(|branch| branch == "refs/heads/main") {
        "refs/heads/main".to_string()
    } else {
        branches[0].clone()
    };
    let head_names = head_names.or_else(|| {
        crate::sources::git(&["-C", source, "symbolic-ref", "--quiet", "HEAD"])
            .ok()
            .filter(|target| !target.is_empty())
    });
    let names = head_names.map_or_else(String::new, |target| format!(" ({target})"));
    Err(format!(
        "{source}: its HEAD{names} names no commit, though it has branches \
         ({}), so no task could start from it; point HEAD at an existing branch: \
         git -C {source} symbolic-ref HEAD {suggested}",
        branches.join(", ")
    ))
}

/// `spawn mission moved --source <dir> --from <commit> --to <commit>`: a
/// deploy's record of main moved forward (`source.moved`). `--from` and
/// `--to` are each read through the commit map (objective 178): a deploy
/// recorded before a history rewrite names the old id. The new ids are
/// compared and recorded; a dropped or unknown one is refused with its
/// sentence, never as "does not descend".
fn moved(store: &spawn_postgres::PostgresStore, flags: &Flags) -> Result<Value, String> {
    let err = |error: spawn_core::port::StoreError| error.to_string();
    let source = text(flags, "source")?;
    crate::sources::MissionSource::parse(&source)?;
    let read = |name: &str| -> Result<String, String> {
        let recorded = text(flags, name)?;
        crate::commit_now::commit_now(store, std::path::Path::new(&source), &recorded)
            .map_err(err)?
            .id_or_why(&format!("--{name}"), &recorded)
    };
    let from = read("from")?;
    let to = read("to")?;
    // Forward only: a main that dropped landed work is a halt, not a deploy.
    let forward = crate::sources::controller_git()
        .args([
            "--git-dir",
            &source,
            "merge-base",
            "--is-ancestor",
            &from,
            &to,
        ])
        .status()
        .map_err(|error| format!("git: {error}"))?;
    if !forward.success() {
        return Err(format!("{to} does not descend from {from} in {source}"));
    }
    let missions = store.source_moved(&source, &from, &to).map_err(err)?;
    Ok(json!({ "source": source, "to": to, "missions": missions }))
}

/// `spawn mission pause --id N [--reason <text>]` (objective 099): the
/// owner's hold on a mission, `mission.paused`; the store refuses anyone but
/// the owner, a closed mission and one already paused, recording nothing.
fn pause(store: &spawn_postgres::PostgresStore, flags: &Flags) -> Result<Value, String> {
    let mission = id(flags, "id")?;
    let reason = flags.get("reason");
    if reason
        .as_deref()
        .is_some_and(|text| text.chars().count() > HOLD_REASON_CHARS)
    {
        return Err(format!(
            "--reason is at most {HOLD_REASON_CHARS} characters"
        ));
    }
    let event = store
        .mission_pause(mission, reason.as_deref())
        .map_err(|error| error.to_string())?;
    Ok(json!({
        "mission": mission,
        "paused": {"event_id": event, "reason": reason},
    }))
}

/// `spawn mission pushes`: the source's owner landing requests, newest
/// first, with their states ([`spawn_postgres::PostgresStore::source_pushes`]).
fn pushes(store: &spawn_postgres::PostgresStore, flags: &Flags) -> Result<Value, String> {
    let source = text(flags, "source")?;
    crate::sources::MissionSource::parse(&source)?;
    let commit = pushes_commit(flags)?;
    let limit = pushes_limit(flags)?;
    store
        .source_pushes(&source, commit.as_deref(), limit)
        .map_err(|error| error.to_string())
}

/// `spawn mission pushes --commit`: a hash or a prefix of one (hex), or none.
fn pushes_commit(flags: &Flags) -> Result<Option<String>, String> {
    let Some(commit) = flags.get("commit") else {
        return Ok(None);
    };
    if commit.is_empty() || commit.len() > 64 || !commit.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "--commit is a commit hash or a prefix of one, not {commit:?}"
        ));
    }
    Ok(Some(commit.to_ascii_lowercase()))
}

/// `spawn mission pushes --limit`: 1 to 1000, default 20.
fn pushes_limit(flags: &Flags) -> Result<i64, String> {
    let Some(text) = flags.get("limit") else {
        return Ok(20);
    };
    match text.parse::<i64>() {
        Ok(limit) if (1..=1000).contains(&limit) => Ok(limit),
        _ => Err(format!("--limit is 1 to 1000, not {text}")),
    }
}

/// `spawn mission push`: put the owner's commit in the source at
/// `refs/spawn-owner/<hash>` and record the request the integrator applies
/// (docs/fleet-architecture.md §10.3, "The owner's landings"). Exit 0, or,
/// with `--wait`, 0 when applied, 1 when refused, 3 when still pending.
/// `--withdraw <request>` withdraws a pending request instead
/// ([`crate::integrate::owner::withdraw`]).
fn push(store: &spawn_postgres::PostgresStore, flags: &Flags) -> Result<i32, String> {
    if let Some(request) = withdrawn(flags)? {
        let outcome = crate::integrate::owner::withdraw(store, request)?;
        print(&json!({ "request": request, "state": "refused", "outcome": outcome }));
        return Ok(0);
    }
    let source = text(flags, "source")?;
    crate::sources::MissionSource::parse(&source)?;
    if !source.starts_with('/') {
        return Err(format!(
            "its source {source} is remote: agents' branches reach only a directory on this \
             host, so nothing lands on it"
        ));
    }
    let wanted = text(flags, "commit")?;
    let wait = flags
        .get("wait")
        .map(|text| {
            text.parse::<u64>()
                .map_err(|_| format!("--wait must be a number of seconds, not {text}"))
        })
        .transpose()?;
    let repo = match directory(flags, "repo")? {
        Some(repo) => repo,
        None => std::env::current_dir()
            .map_err(|error| format!("the current directory: {error}"))?
            .display()
            .to_string(),
    };
    let how = spawn_postgres::PushRequest {
        gated: flags.switch("gated"),
        urgent: flags.switch("urgent"),
        reason: flags
            .get("reason")
            .filter(|reason| !reason.trim().is_empty()),
    };
    let (commit, reference) = put_in_source(&source, &repo, &wanted)?;
    let request = store
        .source_push_request(&source, &commit, &reference, &how)
        .map_err(|error| error.to_string())?;
    let mut answer = json!({ "request": request, "source": source, "commit": commit });
    let Some(seconds) = wait else {
        print(&answer);
        return Ok(0);
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    let outcome = loop {
        let outcome = store
            .source_push_outcome(request)
            .map_err(|error| error.to_string())?;
        if outcome.is_some() || std::time::Instant::now() >= deadline {
            break outcome;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    };
    let (state, code) = match outcome
        .as_ref()
        .and_then(|record| record.get("type"))
        .and_then(Value::as_str)
    {
        Some("source.moved") => ("applied", 0),
        Some(_) => ("refused", 1),
        None => ("pending", 3),
    };
    answer["state"] = json!(state);
    answer["outcome"] = outcome.unwrap_or(Value::Null);
    print(&answer);
    Ok(code)
}

/// `spawn mission push --withdraw <request>`: the request to withdraw, or
/// none for a push. It goes alone: with any flag of a push it is refused,
/// so nobody believes a withdrawal also pushed, waited or gave a reason.
fn withdrawn(flags: &Flags) -> Result<Option<i64>, String> {
    if flags.get("withdraw").is_none() {
        return Ok(None);
    }
    let mut others: Vec<String> = ["source", "commit", "repo", "wait", "reason"]
        .into_iter()
        .filter(|name| flags.get(name).is_some())
        .chain(
            ["gated", "urgent"]
                .into_iter()
                .filter(|name| flags.switch(name)),
        )
        .map(|name| format!("--{name}"))
        .collect();
    others.sort();
    if !others.is_empty() {
        return Err(format!(
            "--withdraw <request> goes alone: it withdraws a pending request and takes no other \
             flag of a push (given: {}); nothing is withdrawn",
            others.join(", ")
        ));
    }
    id(flags, "withdraw").map(Some)
}

/// Resolve `wanted` in the owner's repository `repo` (else in the source,
/// which may have it already) and put it in `source` at
/// `refs/spawn-owner/<hash>`, without touching any branch: `(hash, ref)`.
fn put_in_source(source: &str, repo: &str, wanted: &str) -> Result<(String, String), String> {
    use crate::integrate::owner::{full_hash, owner_ref};
    use crate::sources::controller_git;

    let resolve = |place: &[&str]| -> Option<String> {
        let output = controller_git()
            .args(place)
            .args([
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &format!("{wanted}^{{commit}}"),
            ])
            .output()
            .ok()?;
        let hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (output.status.success() && full_hash(&hash)).then_some(hash)
    };
    if let Some(commit) = resolve(&["-C", repo]) {
        let reference = owner_ref(&commit);
        let pushed = controller_git()
            .args([
                "-C",
                repo,
                "push",
                "--quiet",
                "--",
                source,
                &format!("{commit}:{reference}"),
            ])
            .output()
            .map_err(|error| format!("run git push: {error}"))?;
        if !pushed.status.success() {
            return Err(format!(
                "git push of {commit} to {source} ({reference}): {}",
                String::from_utf8_lossy(&pushed.stderr).trim()
            ));
        }
        return Ok((commit, reference));
    }
    let Some(commit) = resolve(&["--git-dir", source]) else {
        return Err(format!(
            "{wanted} is not a commit in {repo} nor in the source {source}"
        ));
    };
    let reference = owner_ref(&commit);
    crate::sources::git(&["--git-dir", source, "update-ref", &reference, &commit])
        .map_err(|error| error.to_string())?;
    Ok((commit, reference))
}

/// `spawn task ...`
pub(crate) fn task(rest: &[String]) -> Result<i32, String> {
    let (verb, flags) = split_task(rest)?;
    let store = open_store()?;
    let err = |error: spawn_core::port::StoreError| error.to_string();
    match verb {
        "assign" => print(&assign(&store, &flags)?),
        "accept" => {
            let task = id(&flags, "id")?;
            store
                .task_review(task, true, "", revision(&flags)?)
                .map_err(err)?;
            print(&json!({ "task": task, "accepted": true }));
        }
        "reject" => print(&reject(&store, &flags)?),
        "cancel" => {
            let task = id(&flags, "id")?;
            store.task_cancel(task).map_err(err)?;
            print(&json!({ "task": task, "cancelled": true }));
        }
        // A task the integrator's load-sensitive bound parked
        // (sync/design-load-sensitive-bound.md): a person releases it, by
        // name and with a reason, and its next gate judges it again (owner
        // only: the store takes `task.released` from the integrator's role).
        "release" => {
            let task = id(&flags, "id")?;
            let by = flags
                .get("by")
                .or_else(|| crate::config::env_value("USER"))
                .filter(|by| !by.trim().is_empty())
                .ok_or("--by NAME is required when USER is unset")?;
            let reason = text(&flags, "reason")?;
            print(&store.task_release(task, &by, &reason).map_err(err)?);
        }
        "restart" => {
            let task = id(&flags, "id")?;
            let reason = flags.get("reason");
            print(&store.task_restart(task, reason.as_deref()).map_err(err)?);
        }
        "reassign" => {
            let task = id(&flags, "id")?;
            let reason = flags.get("reason");
            print(&store.task_reassign(task, reason.as_deref()).map_err(err)?);
        }
        // Work stealing (objective 239): the owner's move, by `human`.
        "steal" => {
            let task = id(&flags, "id")?;
            let reason = flags.get("reason");
            let moved = store
                .task_steal(task, &text(&flags, "to")?, "human", reason.as_deref())
                .map_err(|error| format!("task steal: {}", refusal(&error)))?;
            print(&moved);
        }
        "stages" => print(&store.task_stages(id(&flags, "id")?).map_err(err)?),
        _ => return Err(USAGE.to_string()),
    }
    Ok(0)
}

/// The store's own words of a refusal: its message, without the driver's
/// wrapping or the SQLSTATE after it.
fn refusal(error: &spawn_core::port::StoreError) -> String {
    use spawn_core::port::StoreError;
    let text = match error {
        StoreError::Backend(text) | StoreError::Rejected(text) => text.trim_end(),
        other => return other.to_string(),
    };
    match text.rsplit_once(" (") {
        Some((message, code))
            if code.len() == 6
                && code.ends_with(')')
                && code[..5].bytes().all(|b| b.is_ascii_alphanumeric()) =>
        {
            message.to_string()
        }
        _ => text.to_string(),
    }
}

/// `spawn resident ...`
pub(crate) fn resident(rest: &[String]) -> Result<i32, String> {
    let (verb, flags) = split(rest)?;
    let store = open_store()?;
    let err = |error: spawn_core::port::StoreError| error.to_string();
    let name = text(&flags, "name")?;
    match verb {
        "propose" => store
            .resident_propose_in(
                &name,
                &text(&flags, "charter")?,
                flags.get("pool").as_deref(),
            )
            .map_err(err)?,
        "approve" => store.resident_approve(&name).map_err(err)?,
        "retire" => store.resident_retire(&name).map_err(err)?,
        _ => return Err(USAGE.to_string()),
    }
    print(&json!({ "resident": name, verb: true }));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use spawn_core::event::ModelProfile;

    use serde_json::{Value, json};

    use super::{
        change, hold_bound, model, objectives_row, pushes_commit, pushes_limit, split_mission,
        withdrawn,
    };
    use crate::Flags;

    /// `spawn mission show` names the engine from the root's marker, and
    /// reading it makes nothing: no root, no probe file, no probe mount.
    #[test]
    fn mission_show_names_the_workspace_backend_without_writing_anything() {
        use crate::config::WorkspaceSettings;
        let base = std::env::temp_dir().join(format!(
            "spawn-show-backend-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let missing = base.join("never-made");
        let view = WorkspaceSettings::peek(&missing);
        assert!(
            view.get("error").is_some() || view.get("configured").is_some(),
            "{view}"
        );
        assert!(!missing.exists(), "a read made the root");
        assert!(!base.exists());

        std::fs::create_dir_all(&base).expect("root");
        spawn_host::claim_root(&base, spawn_host::WorkspaceBackend::Cow).expect("claimed");
        let before: Vec<_> = std::fs::read_dir(&base)
            .expect("list")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        let marked = spawn_host::root_backend(&base).map_err(|error| error.to_string());
        let view = WorkspaceSettings::peek_with(
            Ok(crate::config::WorkspaceBackend::Auto),
            Ok(true),
            marked,
        );
        assert_eq!(
            view,
            json!({"configured": "auto", "active": "cow", "root": "cow", "cas_dedup": "on",
                   "servable": true})
        );
        let _ = WorkspaceSettings::peek(&base);
        let after: Vec<_> = std::fs::read_dir(&base)
            .expect("list")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(before, after, "a read wrote under the root");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn configure_sends_only_the_settings_it_is_given() {
        // The trap of 2026-11-08: --tool-calls alone replaced every setting.
        assert_eq!(
            change(&flags(&["--id", "4", "--tool-calls", "600"])),
            Ok(json!({"budget": {"tool_calls": 600}}))
        );
        assert_eq!(
            change(&flags(&["--id", "4", "--workers", "3", "--accept", "gate"])),
            Ok(json!({"accept": "gate", "objectives": {"workers": 3}}))
        );
        let gate = std::env::temp_dir().display().to_string();
        assert_eq!(
            change(&flags(&["--id", "4", "--gate", &gate, "--minutes", "45"])),
            Ok(json!({"gate": gate, "budget": {"minutes": 45}}))
        );
    }

    /// `configure`'s flags, its clearing switches taking no value.
    fn configure(words: &[&str]) -> Result<serde_json::Value, String> {
        let words: Vec<String> = ["configure"]
            .iter()
            .chain(words)
            .map(ToString::to_string)
            .collect();
        let (_, flags) = split_mission(&words)?;
        change(&flags)
    }

    #[test]
    fn configure_sets_and_clears_a_daily_budget_in_dollars() {
        assert_eq!(
            configure(&["--id", "4", "--daily-usd", "0.50"]),
            Ok(json!({"budget": {"daily_usd": 0.5}}))
        );
        assert_eq!(
            configure(&["--id", "4", "--no-daily-usd"]),
            Ok(json!({"budget": {"daily_usd": null}}))
        );
        for bad in ["0", "-1", "abc", "1e3", "0.000", ".", "1.2.3"] {
            let refused = configure(&["--id", "4", "--daily-usd", bad]).expect_err(bad);
            assert!(refused.contains("--daily-usd"), "{bad}: {refused}");
        }
        let refused =
            configure(&["--id", "4", "--daily-usd", "2", "--no-daily-usd"]).expect_err("both");
        assert!(
            refused.contains("--no-daily-usd and --daily-usd"),
            "{refused}"
        );
    }

    /// `--features` names an existing, valid feature map; `--no-features`
    /// clears it. A missing or invalid file is refused at configure time,
    /// not silently ignored by the page later.
    #[test]
    fn configure_sets_and_clears_a_feature_map_only_with_a_valid_file() {
        let dir = std::env::temp_dir().join(format!(
            "spawn-feature-flag-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("map.json"),
            r#"{"features":[{"id":"x","title":"X","sources":[{"kind":"objective","objective":1}]}]}"#,
        )
        .expect("a mapping");
        let path = dir.join("map.json").display().to_string();
        assert_eq!(
            configure(&["--id", "4", "--features", &path]),
            Ok(json!({"objectives": {"features": path}}))
        );
        assert_eq!(
            configure(&["--id", "4", "--no-features"]),
            Ok(json!({"objectives": {"features": null}}))
        );
        let refused =
            configure(&["--id", "4", "--features", "/definitely/not/here"]).expect_err("no file");
        assert!(refused.contains("is not a file"), "{refused}");
        std::fs::write(
            dir.join("bad.json"),
            r#"{"features":[{"id":"x","title":"X","status":"live"}]}"#,
        )
        .expect("a bad mapping");
        let bad = dir.join("bad.json").display().to_string();
        let refused = configure(&["--id", "4", "--features", &bad]).expect_err("a status");
        assert!(refused.contains("unknown field `status`"), "{refused}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `mission add` validates `--features` before the mission has an id, so
    /// a mapping that names its own mission in an objective source is
    /// accepted; only a known mission id refuses a foreign one.
    #[test]
    fn add_accepts_a_mapping_that_names_its_own_mission() {
        let dir = std::env::temp_dir().join(format!(
            "spawn-feature-add-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(dir.join("backlog")).expect("temp backlog");
        std::fs::write(
            dir.join("map.json"),
            r#"{"features":[{"id":"x","title":"X","sources":[{"kind":"objective","mission":7,"objective":488}]}]}"#,
        )
        .expect("a mapping");
        let backlog = dir.join("backlog").display().to_string();
        let path = dir.join("map.json").display().to_string();
        let gate = dir.display().to_string();
        let words: Vec<String> = [
            "add",
            "--text",
            "Spawn RSI",
            "--gate",
            &gate,
            "--backlog",
            &backlog,
            "--resident",
            "rsi",
            "--features",
            &path,
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        let (_, flags) = split_mission(&words).expect("flags");
        let settings = super::Settings::from(&flags).expect("add knows no mission id yet");
        assert_eq!(
            settings.objectives.expect("the objectives")["features"],
            path
        );
        // The same file is fine for its own mission and refused for another
        // once the id is known.
        assert!(crate::serve::feature_map::validate_file(std::path::Path::new(&path), 7).is_ok());
        let refused = crate::serve::feature_map::validate_file(std::path::Path::new(&path), 4)
            .expect_err("a foreign mission");
        assert!(refused.contains("names mission 7"), "{refused}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `mission add` never silently drops the settings it is given:
    /// objectives without a gate are refused (they could never land), and
    /// any setting that is given — a budget or `--accept` alone too — is
    /// carried to be applied.
    #[test]
    fn add_refuses_objective_settings_without_a_gate() {
        let dir = std::env::temp_dir().join(format!(
            "spawn-feature-add-settings-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(dir.join("backlog")).expect("temp backlog");
        std::fs::write(
            dir.join("map.json"),
            r#"{"features":[{"id":"x","title":"X","sources":[{"kind":"objective","objective":1}]}]}"#,
        )
        .expect("a mapping");
        let gate = dir.display().to_string();
        let backlog = dir.join("backlog").display().to_string();
        let path = dir.join("map.json").display().to_string();
        let settings_of = |words: &[&str]| {
            let words: Vec<String> = ["add", "--text", "Spawn RSI"]
                .iter()
                .chain(words.iter())
                .map(ToString::to_string)
                .collect();
            let (_, flags) = split_mission(&words).expect("flags");
            super::Settings::from(&flags)
        };
        let refused = settings_of(&[
            "--backlog",
            &backlog,
            "--resident",
            "rsi",
            "--features",
            &path,
        ])
        .expect_err("objectives without a gate");
        assert!(refused.contains("--gate"), "{refused}");
        // With the gate, every one of them is carried: none is dropped.
        let settings = settings_of(&[
            "--gate",
            &gate,
            "--backlog",
            &backlog,
            "--resident",
            "rsi",
            "--workers",
            "2",
            "--features",
            &path,
        ])
        .expect("all of them with a gate");
        assert!(settings.given);
        let objectives = settings.objectives.expect("the objectives");
        assert_eq!(objectives["backlog"], backlog);
        assert_eq!(objectives["resident"], "rsi");
        assert_eq!(objectives["workers"], 2);
        assert_eq!(objectives["features"], path);
        // No setting at all means no configure record.
        let bare = settings_of(&[]).expect("a bare add");
        assert!(!bare.given);
        assert!(bare.objectives.is_none());
        // A budget or `--accept` alone is a setting, and is applied.
        let budget = settings_of(&["--minutes", "45"]).expect("a budget");
        assert!(budget.given);
        assert_eq!(budget.budget, Some(json!({"minutes": 45})));
        let accept = settings_of(&["--accept", "gate"]).expect("an accept rule");
        assert!(accept.given);
        assert_eq!(accept.accept, "gate");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Through a real store, the command's own path: `mission add` with a
    /// gate, a backlog and `--features` stores the mapping path, and the
    /// feature map's mission row reads it back as `features_file`. Without a
    /// gate nothing is added (the refusal leaves no half-configured
    /// mission).
    #[test]
    #[allow(clippy::too_many_lines)] // one store, the add, the budget and the refusal
    fn add_stores_the_features_it_is_given() {
        let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
            "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
        );
        let schema = spawn_postgres::testing::TestSchema::new("mission_add_features");
        let store = spawn_postgres::PostgresStore::connect_in_schema(&url, &schema)
            .expect("a schema of its own, migrated");
        let dir = std::env::temp_dir().join(format!(
            "spawn-feature-roundtrip-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("backlog")).expect("temp backlog");
        std::fs::write(
            dir.join("map.json"),
            r#"{"features":[{"id":"x","title":"X","sources":[{"kind":"objective","objective":1}]}]}"#,
        )
        .expect("a mapping");
        std::fs::write(
            dir.join("backlog/001.json"),
            r#"{"id": 1, "title": "One", "prompt": "Do it."}"#,
        )
        .expect("an objective");
        let gate = dir.display().to_string();
        let backlog = dir.join("backlog").display().to_string();
        let path = dir.join("map.json").display().to_string();
        let flags_of = |words: &[&str]| {
            let words: Vec<String> = ["add", "--text", "Spawn RSI"]
                .iter()
                .chain(words.iter())
                .map(ToString::to_string)
                .collect();
            let (_, flags) = split_mission(&words).expect("flags");
            flags
        };
        let id = super::add(
            &store,
            &flags_of(&[
                "--gate",
                &gate,
                "--backlog",
                &backlog,
                "--resident",
                "rsi",
                "--workers",
                "2",
                "--features",
                &path,
            ]),
        )
        .expect("added with every setting");
        let missions = store.objective_missions().expect("the missions");
        let mission = missions
            .as_array()
            .unwrap()
            .iter()
            .find(|mission| mission["id"].as_i64() == Some(id))
            .cloned()
            .unwrap_or_else(|| panic!("mission {id} in {missions}"));
        assert_eq!(mission["gate"], gate);
        assert_eq!(mission["objectives"]["backlog"], backlog);
        assert_eq!(mission["objectives"]["workers"], 2);
        assert_eq!(mission["objectives"]["features"], path);
        let board = crate::serve::with_objectives(mission);
        assert_eq!(board["features_file"], path);
        // A budget with no gate is a setting too: it is applied, not dropped.
        let budgeted = super::add(
            &store,
            &flags_of(&["--text", "a budgeted mission", "--minutes", "45"]),
        )
        .expect("added with a budget");
        let budget = store.objective_missions().expect("the missions");
        let budgeted = budget
            .as_array()
            .unwrap()
            .iter()
            .find(|mission| mission["id"].as_i64() == Some(budgeted))
            .cloned()
            .unwrap_or_else(|| panic!("mission {budgeted} in {budget}"));
        assert_eq!(budgeted["budget"]["minutes"], 45);
        assert_eq!(budgeted["gate"], serde_json::Value::Null);
        // Without a gate the add is refused before it records anything.
        let before = budget.as_array().unwrap().len();
        let refused = super::add(
            &store,
            &flags_of(&[
                "--backlog",
                &backlog,
                "--resident",
                "rsi",
                "--features",
                &path,
            ]),
        )
        .expect_err("no gate");
        assert!(refused.contains("--gate"), "{refused}");
        let after = store
            .objective_missions()
            .expect("the missions")
            .as_array()
            .map_or(0, Vec::len);
        assert_eq!(after, before, "the refusal added no mission");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn configure_clears_only_what_its_switches_name() {
        assert_eq!(
            configure(&["--id", "4", "--no-minutes", "--no-workers"]),
            Ok(json!({"budget": {"minutes": null}, "objectives": {"workers": null}}))
        );
        assert_eq!(
            configure(&["--no-gate", "--id", "4", "--no-objectives", "--no-budget"]),
            Ok(json!({"gate": null, "objectives": null, "budget": null}))
        );
        assert_eq!(
            configure(&["--id", "4", "--minutes", "30", "--no-tool-calls"]),
            Ok(json!({"budget": {"minutes": 30, "tool_calls": null}}))
        );
        assert_eq!(
            configure(&["--id", "4", "--no-gate", "--accept", "review"]),
            Ok(json!({"gate": null, "accept": "review"}))
        );
        // Nothing given: an empty change, which changes nothing.
        assert_eq!(configure(&["--id", "4"]), Ok(json!({})));
    }

    #[test]
    fn a_switch_takes_no_value_and_every_other_flag_still_needs_one() {
        let refused = configure(&["--id", "4", "--no-gate", "yes"]).expect_err("a value");
        assert!(refused.contains("--no-gate takes no value"), "{refused}");
        let refused = configure(&["--id", "4", "--minutes"]).expect_err("no value");
        assert!(refused.contains("--minutes needs a value"), "{refused}");
        let refused = configure(&["--id", "4", "--minutes", "--no-budget"]).expect_err("a switch");
        assert!(refused.contains("--minutes needs a value"), "{refused}");
        // Only configure knows the switches.
        let words: Vec<String> = ["close", "--id", "4", "--no-gate"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let refused = split_mission(&words).expect_err("not a switch of close");
        assert!(refused.contains("--no-gate needs a value"), "{refused}");
    }

    #[test]
    fn configure_refuses_an_unknown_flag_or_a_switch_with_a_value_for_its_setting() {
        for (words, said) in [
            (
                vec!["--id", "4", "--toolcalls", "600"],
                "has no --toolcalls",
            ),
            (vec!["--id", "4", "--clear", "gate"], "has no --clear"),
            (
                vec!["--id", "4", "--minutes", "5", "--no-budget"],
                "same setting",
            ),
            (
                vec!["--id", "4", "--minutes", "5", "--no-minutes"],
                "same setting",
            ),
            (
                vec!["--id", "4", "--gate", "/", "--no-gate"],
                "same setting",
            ),
            (
                vec!["--id", "4", "--workers", "2", "--no-objectives"],
                "same setting",
            ),
            (
                vec!["--id", "4", "--resident", "r", "--no-objectives"],
                "same setting",
            ),
            (
                vec!["--id", "4", "--workers", "2", "--no-workers"],
                "same setting",
            ),
        ] {
            let refused = configure(&words).expect_err(said);
            assert!(refused.contains(said), "{words:?}: {refused}");
        }
        // A switch and a value for different keys of one setting go together.
        assert_eq!(
            configure(&["--id", "4", "--workers", "2", "--no-minutes"]),
            Ok(json!({"objectives": {"workers": 2}, "budget": {"minutes": null}}))
        );
    }

    #[test]
    fn a_holds_bound_is_one_second_to_a_day_three_hours_by_default() {
        assert_eq!(hold_bound(None), Ok(10_800));
        assert_eq!(hold_bound(Some("1")), Ok(1));
        assert_eq!(hold_bound(Some("20")), Ok(20));
        assert_eq!(hold_bound(Some("86400")), Ok(86_400));
        for bad in ["0", "-5", "86401", "3h", " 20", "1.5", "99999999999"] {
            let refused = hold_bound(Some(bad)).expect_err(bad);
            assert!(
                refused.contains("SPAWN_SOURCE_HOLD_SECS") && refused.contains(bad),
                "{bad}: {refused}"
            );
        }
    }

    fn flags(words: &[&str]) -> Flags {
        let words: Vec<String> = words.iter().map(ToString::to_string).collect();
        Flags::parse(&words).expect("valid flags")
    }

    /// `spawn mission push --withdraw` goes alone: with any other flag of a
    /// push it is an error that names them, never a withdrawal that
    /// ignored them.
    #[test]
    fn withdraw_takes_no_other_flag_of_a_push() {
        let push = |words: &[&str]| {
            let mut words: Vec<String> = words.iter().map(ToString::to_string).collect();
            words.insert(0, "push".to_string());
            withdrawn(&split_mission(&words).expect("flags").1)
        };
        assert_eq!(push(&["--source", "/s", "--commit", "abc"]), Ok(None));
        assert_eq!(push(&["--withdraw", "42"]), Ok(Some(42)));
        assert_eq!(
            push(&["--withdraw", "x"]),
            Err("--withdraw must be a number, not x".to_string())
        );
        for other in [
            &["--source", "/s"][..],
            &["--commit", "abc"],
            &["--repo", "/r"],
            &["--wait", "5"],
            &["--reason", "why"],
            &["--gated"],
            &["--urgent"],
        ] {
            let refused = push(&[&["--withdraw", "42"], other].concat()).expect_err("refused");
            assert!(
                refused.starts_with("--withdraw <request> goes alone")
                    && refused.contains(&format!("(given: {})", other[0])),
                "{refused}"
            );
        }
        assert!(
            push(&["--urgent", "--withdraw", "42", "--wait", "5", "--gated"])
                .expect_err("refused")
                .contains("(given: --gated, --urgent, --wait)")
        );
    }

    #[test]
    fn a_task_runs_on_the_default_profile_unless_model_names_another() {
        assert_eq!(model(&flags(&[])), Ok(ModelProfile::Default));
        assert_eq!(
            model(&flags(&["--model", "cheap"])),
            Ok(ModelProfile::Cheap)
        );
        assert_eq!(
            model(&flags(&["--model", "default"])),
            Ok(ModelProfile::Default)
        );
        let refused = model(&flags(&["--model", "opus"])).expect_err("not a profile");
        assert!(refused.contains("default or cheap"), "{refused}");
    }

    #[test]
    fn pushes_lists_twenty_unless_limit_says_and_commit_is_a_hex_prefix() {
        assert_eq!(pushes_limit(&flags(&[])), Ok(20));
        assert_eq!(pushes_limit(&flags(&["--limit", "1"])), Ok(1));
        assert_eq!(pushes_limit(&flags(&["--limit", "1000"])), Ok(1000));
        for bad in ["0", "-1", "1001", "ten", ""] {
            let refused = pushes_limit(&flags(&["--limit", bad])).expect_err(bad);
            assert!(refused.contains("1 to 1000"), "{bad}: {refused}");
        }
        assert_eq!(pushes_commit(&flags(&[])), Ok(None));
        assert_eq!(
            pushes_commit(&flags(&["--commit", "AB12"])),
            Ok(Some("ab12".to_string()))
        );
        let full = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            pushes_commit(&flags(&["--commit", full])),
            Ok(Some(full.to_string()))
        );
        for bad in ["", "main", "ab%", "ab_", &"a".repeat(65)] {
            let refused = pushes_commit(&flags(&["--commit", bad])).expect_err(bad);
            assert!(refused.contains("a prefix of one"), "{bad}: {refused}");
        }
    }

    /// Commit everything in the checkout `directory`, whatever the host's
    /// git configuration.
    fn commit_in(directory: &str, message: &str) {
        let git = crate::sources::test_git;
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

    /// A bare source made with `master` as its default branch, holding
    /// only `main`: its HEAD names no commit. `(base, source)`.
    fn source_with_head_at_a_missing_branch(tag: &str) -> (std::path::PathBuf, String) {
        let git = crate::sources::test_git;
        let base =
            std::env::temp_dir().join(format!("spawn-missing-head-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).expect("seed");
        let seed_path = seed.to_string_lossy().to_string();
        git(&["init", "-q", "--initial-branch=main", &seed_path]).expect("init");
        std::fs::write(seed.join("README.md"), "base\n").expect("write");
        commit_in(&seed_path, "base");
        let source = base.join("source.git").to_string_lossy().to_string();
        git(&["init", "-q", "--bare", "--initial-branch=master", &source]).expect("bare");
        git(&["-C", &seed_path, "push", "-q", &source, "main:main"]).expect("push main");
        (base, source)
    }

    /// The refusal alone, no store: a source whose HEAD names a branch it
    /// does not hold is refused, naming that branch, with the repair; after
    /// the repair, an empty source and a remote one are accepted.
    #[test]
    fn a_source_with_unresolved_head_is_refused_with_the_repair() {
        let git = crate::sources::test_git;
        let (base, source) = source_with_head_at_a_missing_branch("refusal");
        let refused =
            super::refuse_source_with_unresolved_head(&source).expect_err("HEAD names no commit");
        assert!(refused.contains(&source), "{refused}");
        assert!(refused.contains("refs/heads/main"), "{refused}");
        assert!(
            refused.contains("its HEAD (refs/heads/master) names no commit"),
            "{refused}"
        );
        assert!(
            refused.contains(&format!(
                "git -C {source} symbolic-ref HEAD refs/heads/main"
            )),
            "{refused}"
        );
        git(&["-C", &source, "symbolic-ref", "HEAD", "refs/heads/main"]).expect("repair");
        assert_eq!(super::refuse_source_with_unresolved_head(&source), Ok(()));
        // A source with nothing pushed yet is a new project's: accepted.
        let empty = base.join("empty.git").to_string_lossy().to_string();
        git(&["init", "-q", "--bare", "--initial-branch=main", &empty]).expect("empty");
        assert_eq!(super::refuse_source_with_unresolved_head(&empty), Ok(()));
        // A remote source is not read here.
        assert_eq!(
            super::refuse_source_with_unresolved_head("https://example.invalid/repo.git"),
            Ok(())
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A non-bare source, HEAD unborn at `trunk`, its one branch `feature`
    /// (no `main`): refused, naming `trunk` and suggesting the first
    /// existing branch; the `git -C` repair it names works on a work tree,
    /// and the source is then accepted.
    #[test]
    fn a_non_bare_source_with_unresolved_head_and_no_main_is_repaired_with_git_c() {
        let git = crate::sources::test_git;
        let base = std::env::temp_dir().join(format!(
            "spawn-mission-head-non-bare-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let work = base.join("work");
        std::fs::create_dir_all(&work).expect("work");
        let source = work.to_string_lossy().to_string();
        git(&["init", "-q", "--initial-branch=feature", &source]).expect("init");
        std::fs::write(work.join("README.md"), "base\n").expect("write");
        commit_in(&source, "base");
        git(&["-C", &source, "symbolic-ref", "HEAD", "refs/heads/trunk"]).expect("unborn HEAD");
        let refused =
            super::refuse_source_with_unresolved_head(&source).expect_err("HEAD names no commit");
        assert!(
            refused.contains("its HEAD (refs/heads/trunk) names no commit"),
            "{refused}"
        );
        assert!(refused.contains("(refs/heads/feature)"), "{refused}");
        let repair = format!("git -C {source} symbolic-ref HEAD refs/heads/feature");
        assert!(refused.contains(&repair), "{refused}");
        // The repair as the error words it, on a work tree.
        git(&["-C", &source, "symbolic-ref", "HEAD", "refs/heads/feature"])
            .expect("the named repair works on a non-bare source");
        assert_eq!(super::refuse_source_with_unresolved_head(&source), Ok(()));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Through a real store, the command's own path: `mission add` refuses a
    /// bare source whose HEAD names no commit and records nothing; after
    /// `git symbolic-ref HEAD refs/heads/main` the same add succeeds.
    #[test]
    fn mission_add_refuses_a_source_whose_head_names_no_commit_until_repaired() {
        let git = crate::sources::test_git;
        let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
            "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
        );
        let schema = spawn_postgres::testing::TestSchema::new("mission_add_missing_head");
        let store = spawn_postgres::PostgresStore::connect_in_schema(&url, &schema)
            .expect("a schema of its own, migrated");
        let (base, source) = source_with_head_at_a_missing_branch("add");
        let words: Vec<String> = ["add", "--text", "missing head", "--source", &source]
            .iter()
            .map(ToString::to_string)
            .collect();
        let (_, flags) = split_mission(&words).expect("flags");
        let recorded = || {
            store
                .mission_sources()
                .expect("the sources")
                .to_string()
                .contains(&source)
        };
        let refused = super::add(&store, &flags).expect_err("HEAD names no commit");
        assert!(
            refused.contains("symbolic-ref HEAD refs/heads/main"),
            "{refused}"
        );
        assert!(!recorded(), "the refusal added no mission");
        git(&["-C", &source, "symbolic-ref", "HEAD", "refs/heads/main"]).expect("repair");
        let id = super::add(&store, &flags).expect("added once HEAD names main");
        assert!(id > 0);
        assert!(recorded(), "the mission names its source");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A scratch world: a bare source with one commit on main, and the
    /// owner's clone of it. `(base, source, owner)`.
    fn owner_world(tag: &str) -> (std::path::PathBuf, String, String) {
        let git = crate::sources::test_git;
        let base = std::env::temp_dir().join(format!("spawn-push-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let seed = base.join("seed");
        std::fs::create_dir_all(&seed).expect("seed");
        let seed_path = seed.to_string_lossy().to_string();
        git(&["init", "-q", "--initial-branch=main", &seed_path]).expect("init");
        std::fs::write(seed.join("README.md"), "base\n").expect("write");
        commit_in(&seed_path, "base");
        let source = base.join("source.git").to_string_lossy().to_string();
        git(&["clone", "-q", "--bare", &seed_path, &source]).expect("source");
        let owner = base.join("owner").to_string_lossy().to_string();
        git(&["clone", "-q", &source, &owner]).expect("the owner's clone");
        (base, source, owner)
    }

    /// `spawn mission push` resolves the commit where the owner works and
    /// puts it in the source at `refs/spawn-owner/<hash>`, moving no branch;
    /// a commit already in the source (by a name it knows) gets the same
    /// ref; anything else is refused before any request is recorded.
    #[test]
    fn the_owners_commit_waits_in_the_source_and_no_branch_moves() {
        let git = crate::sources::test_git;
        let (base, source, owner) = owner_world("put");
        let main = git(&["--git-dir", &source, "rev-parse", "refs/heads/main"]).expect("main");
        std::fs::write(std::path::Path::new(&owner).join("CONTRACT.md"), "v1\n").expect("write");
        commit_in(&owner, "owner: the contract");
        let head = git(&["-C", &owner, "rev-parse", "HEAD"]).expect("head");

        let (commit, reference) = super::put_in_source(&source, &owner, "HEAD").expect("put");
        assert_eq!(commit, head, "resolved in the owner's repository");
        assert_eq!(reference, format!("refs/spawn-owner/{head}"));
        assert_eq!(
            git(&["--git-dir", &source, "rev-parse", &reference]).expect("the ref"),
            head
        );
        assert_eq!(
            git(&["--git-dir", &source, "rev-parse", "refs/heads/main"]).expect("main"),
            main,
            "no branch moves"
        );

        // Already in the source, named as the source knows it, from a
        // directory that is no repository.
        let elsewhere = base.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("a directory");
        let (known, known_ref) =
            super::put_in_source(&source, &elsewhere.to_string_lossy(), &main).expect("known");
        assert_eq!(known, main);
        assert_eq!(
            git(&["--git-dir", &source, "rev-parse", &known_ref]).expect("the ref"),
            main
        );

        for wanted in [
            "no-such-branch",
            "--all",
            "0123456789abcdef0123456789abcdef01234567",
        ] {
            let refused = super::put_in_source(&source, &owner, wanted).expect_err(wanted);
            assert!(refused.contains("is not a commit"), "{wanted}: {refused}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The objectives listing splits a mission's backlog by who judges it:
    /// the owner's apart from the fleet's open ones, with the counts.
    #[test]
    fn the_objectives_listing_puts_the_owners_apart_from_the_fleets() {
        let mission = json!({
            "id": 4,
            "backlog": "/b",
            "backlog_error": null,
            "next_objective": 30,
            "objectives": [
                {"id": 29, "title": "the owner's", "status": "owner"},
                {"id": 30, "title": "the fleet's", "status": "open"},
                {"id": 28, "title": "done", "status": "solved"},
                {"id": 7, "title": "met", "status": "met"},
            ],
        });
        let row = objectives_row(&mission);
        assert_eq!(row["mission"], json!(4));
        assert_eq!(
            row["owner_judged"],
            json!([{"id": 29, "title": "the owner's"}])
        );
        assert_eq!(row["open"], json!([{"id": 30, "title": "the fleet's"}]));
        assert_eq!(row["solved"], json!(1));
        assert_eq!(row["met"], json!(1));
        assert_eq!(row["next_objective"], json!(30));
    }

    #[allow(clippy::too_many_lines)] // all open states plus remote and scratch projections
    #[test]
    fn mission_view_quotes_checks_and_diagnoses_each_open_task_without_creating_paths() {
        use spawn_core::event::SessionSeq;
        use spawn_core::observation::{Ambiguity, Confidence, Observation, SemanticState};
        let root =
            std::env::temp_dir().join(format!("spawn mission '75' view {}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let source = root.join("source 'repo'.git").to_string_lossy().to_string();
        let scenarios = root.join("missing scenarios").to_string_lossy().to_string();
        let backlog = root.join("missing backlog").to_string_lossy().to_string();
        let runner = root.join("missing gate.sh");
        let states = [
            "assigned",
            "revise",
            "delivered",
            "reported",
            "accepted",
            "landing",
        ];
        let tasks: Vec<Value> = states
            .iter()
            .enumerate()
            .map(|(index, state)| {
                let id = index + 1;
                json!({
                    "id": id, "kind": "ephemeral", "agent_id": format!("agent_worker_{id}"),
                    "session_id": format!("session_worker_{id}"), "state": state,
                    "revisions": 0, "candidate": format!("candidate-{id}"), "objective": id,
                    "stages": [{"stage": "gated", "verdict": "accepted", "at_ms": 10,
                                "env": {"SPAWN_DATABASE_URL": "postgres://supersecret@db/spawn"}}],
                })
            })
            .collect();
        let work = crate::deliver::Work {
            tasks: serde_json::from_value(json!(
                tasks
                    .iter()
                    .map(|task| json!({
                        "id": task["id"], "mission_id": 75, "kind": task["kind"],
                        "agent_id": task["agent_id"], "session_id": task["session_id"],
                        "brief": "brief", "state": task["state"], "revisions": 0,
                    }))
                    .collect::<Vec<_>>()
            ))
            .expect("work rows"),
            ..crate::deliver::Work::default()
        };
        let mut observation = Observation::basic(
            SemanticState::Waiting,
            None,
            SessionSeq(5),
            Confidence::new(0.9).expect("confidence"),
            "rules",
        );
        observation.ambiguity = Some(Ambiguity::TurnStoppedIdle);
        let rows: Vec<crate::dispatch::Facts> = serde_json::from_value(json!([{
            "session_id": "session_worker_3", "agent_id": "agent_worker_3", "live": true,
            "turn_ended": false, "turn_seq": 5, "checkpoint": "checkpoint-3",
            "observation": observation, "evidence_seq": 5,
        }]))
        .expect("facts");
        let last_gates = json!({
            "4": {"verdict": "accepted", "candidate": "candidate-4", "at_ms": 20,
                  "env": {"SPAWN_TEST_DATABASE_URL": "postgres://supersecret@db/test"}},
            "5": {"verdict": "accepted", "candidate": "candidate-5"},
            "6": {"verdict": "accepted", "candidate": "candidate-6"},
        });
        let mission = json!({
            "id": 75, "state": "open", "sources": [source], "gate": scenarios,
            "objectives": {"backlog": backlog}, "tasks": tasks,
        });
        let view = super::mission_view(
            &mission,
            &root,
            &runner,
            Some((&rows, &work)),
            Some(&last_gates),
            100,
        )
        .expect("operator view");
        assert_eq!(view["landing_repo"]["target"], source);
        assert_eq!(view["landing_repo"]["kind"], "host-path");
        assert_eq!(view["mirror"]["exists"], false);
        assert_eq!(view["gate"]["exists"], false);
        assert_eq!(view["backlog"]["exists"], false);
        assert!(!root.exists(), "the view created a path");
        assert!(view["tasks"][0]["diagnostics"]["delivery"]["line"].is_string());
        assert!(view["tasks"][1]["diagnostics"]["delivery"]["line"].is_string());
        assert_eq!(
            view["tasks"][2]["diagnostics"]["report_blocker"],
            "turn has not ended in the transcript"
        );
        assert_eq!(
            view["tasks"][2]["diagnostics"]["report"]["status"],
            "report absent"
        );
        for index in 3..6 {
            assert_eq!(
                view["tasks"][index]["diagnostics"]["latest_gate"]["verdict"],
                "accepted"
            );
            assert!(view["tasks"][index]["diagnostics"]["landing"].is_object());
        }
        let checks: Vec<&str> = view["checks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let quoted_mirror = super::shell_quote(view["mirror"]["path"].as_str().expect("mirror"));
        let quoted_runner = super::shell_quote(&runner.to_string_lossy());
        let quoted_source = super::shell_quote(&source);
        assert!(
            checks.contains(
                &format!("git --git-dir {quoted_mirror} rev-parse --verify refs/heads/main")
                    .as_str()
            ),
            "{checks:?}"
        );
        assert!(
            checks.contains(
                &format!("git --git-dir {quoted_mirror} log -1 --oneline refs/heads/main").as_str()
            ),
            "{checks:?}"
        );
        assert!(
            !checks
                .iter()
                .any(|check| check.contains("rev-parse --verify HEAD")),
            "{checks:?}"
        );
        assert!(
            checks
                .contains(&format!("test -f {quoted_runner} && test -r {quoted_runner}").as_str()),
            "{checks:?}"
        );
        assert!(
            checks.contains(
                &format!("git ls-remote --symref -- {quoted_source} HEAD refs/heads/main").as_str()
            ),
            "{checks:?}"
        );
        let printed = view.to_string();
        for secret in [
            "SPAWN_DATABASE_URL",
            "SPAWN_TEST_DATABASE_URL",
            "postgres://",
            "supersecret",
        ] {
            assert!(
                !printed.contains(secret),
                "output contains {secret}: {printed}"
            );
        }

        // Absent session facts under an owner view must report `no session facts`,
        // not falsely claim `worker not live`.
        let absent_facts_view = super::mission_view(
            &mission,
            &root,
            &runner,
            Some((&[], &work)),
            Some(&last_gates),
            100,
        )
        .expect("absent facts view");
        assert_eq!(
            absent_facts_view["tasks"][2]["diagnostics"]["report_blocker"],
            "no session facts"
        );

        let remote = "https://git.example.org/acme/widget.git";
        let remote_view = super::mission_view(
            &json!({"id": 76, "state": "open", "sources": [remote], "tasks": []}),
            &root,
            &runner,
            Some((&[], &crate::deliver::Work::default())),
            Some(&json!({})),
            100,
        )
        .expect("remote view");
        assert_eq!(remote_view["landing_repo"]["kind"], "remote");
        assert!(
            remote_view["checks"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|check| {
                    check.as_str().is_some_and(|check| {
                        check
                            == format!("git ls-remote --symref -- '{remote}' HEAD refs/heads/main")
                    })
                })
        );
        assert!(
            remote_view["checks"]
                .as_array()
                .into_iter()
                .flatten()
                .all(|check| {
                    !check
                        .as_str()
                        .is_some_and(|check| check.starts_with("git --git-dir 'https://"))
                })
        );
        let scratch = super::mission_view(
            &json!({"id": 77, "state": "done", "sources": [], "tasks": []}),
            &root,
            &runner,
            Some((&[], &crate::deliver::Work::default())),
            Some(&json!({})),
            100,
        )
        .expect("scratch view");
        assert_eq!(
            scratch["landing_repo"],
            json!({"target": null, "kind": "scratch"})
        );
        assert_eq!(scratch["gate"]["exists"], false);
        assert_eq!(scratch["backlog"]["exists"], false);
        assert!(!root.exists(), "remote and scratch views created nothing");
    }

    #[test]
    fn mission_show_finds_an_old_closed_mission_and_an_unknown_id_is_clean() {
        let schema = spawn_postgres::testing::TestSchema::new("mission_show");
        let store = schema.store();
        let old = store
            .mission_add("old", &json!([]), "human")
            .expect("old mission");
        store.mission_close(old, "done", "finished").expect("close");
        for n in 0..20 {
            store
                .mission_add(&format!("newer {n}"), &json!([]), "human")
                .expect("newer mission");
        }
        let root =
            std::env::temp_dir().join(format!("spawn-mission-show-db-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let id = old.to_string();
        let root_text = root.to_string_lossy().to_string();
        let shown = super::show(&store, &flags(&["--id", &id, "--root", &root_text]))
            .expect("old mission is shown");
        assert_eq!(shown["mission"], old);
        assert_eq!(shown["state"], "done");
        assert_eq!(shown["mirror"]["exists"], false);
        assert!(!root.exists(), "show created its root or mirror");
        let missing = i64::MAX.to_string();
        let error = super::show(&store, &flags(&["--id", &missing, "--root", &root_text]))
            .expect_err("unknown mission");
        assert_eq!(error, format!("mission #{} not found", i64::MAX));
        assert!(!shown.to_string().contains("SPAWN_DATABASE_URL"));
        assert!(!shown.to_string().contains("SPAWN_TEST_DATABASE_URL"));
    }

    #[test]
    fn mission_show_succeeds_under_the_restricted_supervisor_login() {
        let schema = spawn_postgres::testing::TestSchema::new("mission_show_sup");
        let store = schema.store();
        let mission = store
            .mission_add("supervisor view mission", &json!([]), "human")
            .expect("mission");
        let _assigned = store
            .task_assign(mission, "ephemeral", "w_sup_1", "brief 1", "default")
            .expect("assigned");
        let delivered = store
            .task_assign(mission, "ephemeral", "w_sup_2", "brief 2", "default")
            .expect("delivered assign");
        store.task_delivered(delivered, 1).expect("delivered");
        let reported = store
            .task_assign(mission, "ephemeral", "w_sup_3", "brief 3", "default")
            .expect("reported assign");
        store.task_delivered(reported, 1).expect("delivered 3");
        assert!(store.task_reported(reported, "done").expect("reported"));

        let url = schema.url();
        let separator = if url.contains('?') { '&' } else { '?' };
        let schema_url = format!("{url}{separator}options=-csearch_path%3D{schema}");
        let sup_url =
            spawn_postgres::role_url(&schema_url, "spawn_supervisor").expect("supervisor url");
        let sup_store = spawn_postgres::PostgresStore::connect(&sup_url).expect("supervisor store");

        let root =
            std::env::temp_dir().join(format!("spawn-mission-show-sup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let id = mission.to_string();
        let root_text = root.to_string_lossy().to_string();
        let shown = super::show(&sup_store, &flags(&["--id", &id, "--root", &root_text]))
            .expect("supervisor login must show the mission");
        assert_eq!(shown["mission"], mission);
        assert_eq!(shown["landing_repo"]["kind"], "scratch");
        assert_eq!(shown["mirror"]["exists"], false);
        assert!(!root.exists(), "supervisor show created paths on disk");

        let tasks = shown["tasks"].as_array().expect("tasks array");
        assert_eq!(tasks.len(), 3);
        assert_eq!(
            tasks[0]["diagnostics"]["delivery"]["unavailable"],
            "restricted login"
        );
        assert_eq!(tasks[1]["diagnostics"]["report_blocker"], Value::Null);
        assert_eq!(
            tasks[1]["diagnostics"]["facts_unavailable"],
            "restricted login"
        );
        assert_eq!(tasks[1]["diagnostics"]["report"]["status"], "report absent");
        assert_eq!(
            tasks[2]["diagnostics"]["gate_details_unavailable"],
            "restricted login"
        );
        assert!(tasks[2]["diagnostics"]["landing"].is_object());
        let printed = shown.to_string();
        for secret in [
            "SPAWN_DATABASE_URL",
            "SPAWN_TEST_DATABASE_URL",
            "postgres://",
        ] {
            assert!(!printed.contains(secret), "{secret}: {printed}");
        }
        drop(sup_store);
        drop(store);
    }

    #[test]
    fn assign_brief_file_and_stdin_cases() {
        let dir = std::env::temp_dir().join(format!("spawn-brief-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let brief_path = dir.join("brief.md");
        let body = "line 1 `pgrep -f foo`\n$VAR && ps -p 1234";
        std::fs::write(&brief_path, body).expect("write brief file");

        let from_file = Flags::parse(&[
            "--brief-file".to_string(),
            brief_path.to_string_lossy().into_owned(),
        ])
        .expect("flags");
        assert_eq!(
            crate::missions::brief_text_with(&from_file, || unreachable!()).expect("read file"),
            body
        );

        let from_stdin = Flags::parse(&["--brief".to_string(), "-".to_string()]).expect("flags");
        assert_eq!(
            crate::missions::brief_text_with(&from_stdin, || Ok(body.to_string()))
                .expect("read stdin"),
            body
        );

        let both = Flags::parse(&[
            "--brief".to_string(),
            "inline".to_string(),
            "--brief-file".to_string(),
            brief_path.to_string_lossy().into_owned(),
        ])
        .expect("flags");
        let conflict =
            crate::missions::brief_text_with(&both, || unreachable!()).expect_err("exclusive");
        assert!(conflict.contains("not both"), "{conflict}");

        let empty = Flags::parse(&["--brief".to_string(), "   \n\t ".to_string()]).expect("flags");
        let blank =
            crate::missions::brief_text_with(&empty, || unreachable!()).expect_err("blank brief");
        assert!(blank.contains("must not be empty"), "{blank}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
