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
    classify_difficulty, route_preset, route_with_reprobe,
};
use spawn_core::mission::WorkspaceKind;

use crate::{Flags, open_store};

/// The usage of `spawn mission`, `spawn task` and `spawn resident`, the only
/// copy: main's help shows each command its own lines of it.
pub(crate) const USAGE: &str = "usage:
  spawn mission add --text <mission> [--kind code|media|factory|general] [--source <git repo path or URL>] [<settings>]
  spawn mission configure --id N [<settings>] [<clearing switches>]   (owner only)
      changes only the settings it is given and keeps every other as the mission has it;
      the database merges them under its lock, checks the result and records it whole.
      A setting is cleared only by its own switch, which takes no value: --no-gate,
      --no-budget (every field), --no-minutes, --no-tool-calls, --no-daily-usd, --no-objectives,
      --no-workers (back to the default 1), --no-features, --no-model; a switch and a value for the
      same setting are refused. Prints {mission, before, after}; with nothing given it
      changes nothing.
      Clearing the gate, or moving accept away from gate, waits while a task is landing.
      settings: --gate <scenarios dir> [--accept review|gate] [--minutes N] [--tool-calls N]
                [--daily-usd <US dollars above 0>]
                [--backlog <dir> --resident <name> [--workers N] [--features <file>] [--model default|cheap]]
                (with add, all of them at once, and --backlog, --resident, --workers,
                --features and --model need --gate: an objective lands only through a gate;
                with configure, any of them alone, and --backlog with --resident for a
                mission without objectives)
                (--features: the mission's feature map, a JSON file of capabilities and
                their provenance sources; identities and links only, never a status)
                (--workers: objective tasks in flight at once, 1 to 128, default 1: the
                resident takes one, per-task workers the rest)
                (--model: explicit model profile override for an objective mission's
                assignments, bypassing the grading catalog)
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
  spawn mission list [--limit N] [--json | --table]   (every task with its stages and grading: JSON by default, --table for a readable summary)
  spawn mission show --id N [--root <workspace root>] [--json]
      (one mission by id, including one outside list's window: its workspace (kind and summary)
      and gate hint, resolved source and mirror, actual landing repository, gate and backlog
      paths, per-open-task delivery/report/gate diagnostics, and shell-quoted read-only checks.
      Prints JSON and creates nothing)
      (also: spawn mission episodes [--mission N] [--strict] [--verify-parity] [--json]:
       replay session_control into typed TaskEpisodes and optionally verify 100% parity
       against mission_tasks SQL projections; --verify-parity exits 0 on match, 1 on drift)
  spawn mission close --id N --state done|dropped [--summary <text>]
  spawn task assign --mission N --kind ephemeral|resident --worker <label|resident> (--brief <text|-> | --brief-file <path>)
                    [--model default|cheap] [--engine <claude|agy>] [--model-label <label>] [--again] [--pin]
                    [--difficulty l1|l2|l3] [--preset <preset_id>]
      (--brief - reads the brief from stdin; --brief-file reads it from a file cleanly off the
       command line. The supervisor's brief that a task of the mission in flight already has,
       trimmed of space, tab, CR and LF, is refused, naming that task; --again assigns it on
       purpose. Every duplicate assigned names the task it repeats in its record: duplicate_of.
       --pin keeps the task with its worker: it is never stolen.
       --engine selects an enabled engine directly (bypassing grading catalog routing);
       --model-label sets that engine's launch model label (e.g. gemini-pro-orchestrator-neon for agy).
       Agent grading (docs/agent-grading.md): --difficulty or --preset routes the task
       through the grading catalog (spawn grade): to --preset, or to the cheapest routable
       active preset graded for the difficulty (l2 unless given), on that preset's --model profile;
       no such preset is an error. Without either, an ephemeral task's difficulty is
       classified from its brief (brief-v3: security, architecture or migration work, many files or
       crates, many steps: l3; strictly deterministic low-risk mechanical work: l1; otherwise l2;
       the rules and their evidence are recorded on task.assigned as classification, and answered
       when any matched) and routed top-down: at l3/l2 when covered, and at l1 when an active l1
       preset exists, else falling back upward to l2 (never downgrading l3 to l2); an opt-in
       anti-starvation re-probe runs only when SPAWN_GRADING_REPROBE_SECS is set > 0 (default 0:
       off). With nothing covering it, it is assigned on --model default, saying so
       (routing_fallback); a resident task, or one made from an objective, stays on --model.
       --model keeps the catalog out, and is refused with --difficulty or --preset)
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
  spawn task reassign --id N [--reason <text|-> | --reason-file <path>]
      (a delivered task moves to a fresh ephemeral worker whose branch starts from the old
       worker's last checkpoint: the task is cancelled, a new one assigned, the old worker
       stopped. Prints the new task and its worker; any task not delivered is refused)
  spawn task steal --id N --to <resident> [--reason <text>]
      (owner: a queued, unstarted task of a busy resident moves to an idle resident of the
       same pool, recorded as task.stolen; refused, naming the rule, otherwise)
  spawn task stages --id N   (each stage with its verdict, time, and how long since the one before)
  spawn task commits <id> [--json]
      (lists commits ahead of main on the task's preserved ref, distinguishing author vs
       synthetic checkpoint commits; readable table by default, stable JSON with --json)
  spawn task diff <id> [--stat]
      (shows git diff main...<resolved_ref> in the mission bare repository; --stat for summary)
  spawn task land <id> [--ff-only | --squash] [--dry-run] [--json]
      (lands the task's preserved ref onto refs/heads/main in the mission bare repository;
       defaults to --ff-only, --squash creates one clean commit without checkpoint history,
       --dry-run validates without updating refs/heads/main)
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
    let (workspace_kind, _) =
        WorkspaceKind::parse_mission_text(mission["text"].as_str().unwrap_or_default());
    let profile = workspace_kind.profile();
    let mut checks = vec![
        format!("git --git-dir {quoted_mirror} rev-parse --verify refs/heads/main"),
        format!("git --git-dir {quoted_mirror} log -1 --oneline refs/heads/main"),
    ];
    if profile.expects_code_gate || scenarios.is_some() {
        checks.push(format!(
            "test -f {quoted_runner} && test -r {quoted_runner}"
        ));
    }
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
        "workspace": {
            "kind": workspace_kind.as_str(),
            "summary": profile.summary,
        },
        "source": {"configured": configured, "resolved": resolved},
        "workspace_root": root,
        "mirror": {"path": mirror, "exists": mirror.exists()},
        // The integrator pushes to the source itself, never to this mirror.
        "landing_repo": {"target": resolved, "kind": landing_kind},
        "gate": {
            "scenarios_dir": scenarios,
            "exists": scenarios.is_some_and(|path| std::path::Path::new(path).is_dir()),
            "runner": runner,
            "expects_code_gate": profile.expects_code_gate,
            "hint": profile.gate_hint,
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

/// Resolve `--<value_flag> <text|->` or `--<file_flag> <path>`, refusing
/// conflicting flags and empty or whitespace-only text.
fn resolve_text_or_file(
    flags: &Flags,
    value_flag: &str,
    file_flag: &str,
    stdin: &mut impl std::io::BufRead,
) -> Result<String, String> {
    let inline = flags.get(value_flag);
    let path = flags.get(file_flag);
    match (inline, path) {
        (Some(_), Some(_)) => Err(format!(
            "use either --{value_flag} or --{file_flag}, not both\n{USAGE}"
        )),
        (None, Some(path)) => {
            let content = std::fs::read_to_string(&path)
                .map_err(|error| format!("read --{file_flag} {path}: {error}"))?;
            if content.trim().is_empty() {
                return Err(format!("{value_flag} file {path} is empty"));
            }
            Ok(content)
        }
        (Some(text), None) if text == "-" => {
            let mut content = String::new();
            stdin
                .read_to_string(&mut content)
                .map_err(|error| format!("read --{value_flag} - from stdin: {error}"))?;
            if content.trim().is_empty() {
                return Err(format!("--{value_flag} - read empty input from stdin"));
            }
            Ok(content)
        }
        (Some(text), None) => {
            if text.trim().is_empty() {
                return Err(format!("--{value_flag} must not be empty"));
            }
            Ok(text)
        }
        (None, None) => Err(format!(
            "--{value_flag} <text|-> or --{file_flag} <path> is required\n{USAGE}"
        )),
    }
}

/// Optional variant of [`resolve_text_or_file`] for flags like `--reason`.
fn resolve_optional_text_or_file(
    flags: &Flags,
    value_flag: &str,
    file_flag: &str,
    stdin: &mut impl std::io::BufRead,
) -> Result<Option<String>, String> {
    if flags.get(value_flag).is_none() && flags.get(file_flag).is_none() {
        return Ok(None);
    }
    resolve_text_or_file(flags, value_flag, file_flag, stdin).map(Some)
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
    let stdin = std::io::stdin();
    let mut lock = stdin.lock();
    assign_with_reader(store, flags, &mut lock)
}

pub(crate) fn assign_with_reader(
    store: &spawn_postgres::PostgresStore,
    flags: &Flags,
    reader: &mut impl std::io::BufRead,
) -> Result<Value, String> {
    assign_with_options(store, flags, reader, crate::grading::reprobe_interval_ms())
}

pub(crate) fn assign_with_options(
    store: &spawn_postgres::PostgresStore,
    flags: &Flags,
    reader: &mut impl std::io::BufRead,
    reprobe_interval_ms: i64,
) -> Result<Value, String> {
    let err = |error: spawn_core::port::StoreError| error.to_string();
    let brief = resolve_text_or_file(flags, "brief", "brief-file", reader)?;
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
    let (decision, classified) = match placement(store, flags, &brief, reprobe_interval_ms)? {
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
    if let Some(engine_label) = &decision.engine_label {
        answer["engine_label"] = json!(engine_label);
    }
    if let Some(model_label) = &decision.model_label {
        answer["model_label"] = json!(model_label);
    }
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
    reprobe_interval_ms: i64,
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
    Ok(
        match route_unflagged_with_reprobe(store, brief, "--model default", reprobe_interval_ms)? {
            CatalogRoute::Routed(decision, classified) => {
                Placement::Routed(decision, Some(classified))
            }
            CatalogRoute::Unrouted { fallback } => Placement::Model { fallback },
        },
    )
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
    route_unflagged_with_reprobe(
        store,
        text,
        unrouted_as,
        crate::grading::reprobe_interval_ms(),
    )
}

fn route_unflagged_with_reprobe(
    store: &spawn_postgres::PostgresStore,
    text: &str,
    unrouted_as: &str,
    reprobe_interval_ms: i64,
) -> Result<CatalogRoute, String> {
    let catalog = store.grading_catalog().map_err(|error| error.to_string())?;
    if catalog.is_empty() {
        return Ok(CatalogRoute::Unrouted { fallback: None });
    }
    let classified = classify_difficulty(text);
    let Some(target) =
        spawn_core::grading::resolve_routing_difficulty(&catalog, classified.difficulty)
    else {
        let reason = if classified.difficulty == TaskDifficulty::L3 {
            format!(
                "no routable preset in the grading catalog is graded l3 (refusing to downgrade l3 to l2): \
                 assigned on {unrouted_as}, as without the catalog"
            )
        } else {
            format!(
                "no routable preset in the grading catalog is graded {} or above: \
                 assigned on {unrouted_as}, as without the catalog",
                classified
                    .difficulty
                    .max(TaskDifficulty::default())
                    .required_grade()
            )
        };
        return Ok(CatalogRoute::Unrouted {
            fallback: Some(reason),
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
        reprobe_interval_ms,
    )
    .map(|decision| CatalogRoute::Routed(decision, classified))
    .map_err(|error| format!("agent grading: {error}"))
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
    } else if verb == "list" {
        &["json", "table"]
    } else if verb == "episodes" {
        &["strict", "verify-parity", "json"]
    } else if verb == "show" {
        &["json"]
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
        let model = flags
            .get("model")
            .map(|text| {
                ModelProfile::parse(&text)
                    .ok_or_else(|| format!("--model is default or cheap, not {text}\n{USAGE}"))
            })
            .transpose()?;
        let gate = directory(flags, "gate")?;
        let objectives = match (directory(flags, "backlog")?, flags.get("resident")) {
            (Some(backlog), Some(resident)) => {
                // `spawn_mission_configure` refuses objectives without a
                // gate, and objectives without a gate never land: refuse it
                // here, where `mission add` can say so instead of dropping
                // the settings the command line named.
                if gate.is_none() {
                    return Err(format!(
                        "add with --backlog, --resident, --workers, --features or --model needs \
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
                if let (Some(model), Some(map)) = (model, objectives.as_object_mut()) {
                    map.insert("model".to_string(), json!(model.as_str()));
                }
                Some(objectives)
            }
            (None, None) if workers.is_none() && features.is_none() && model.is_none() => None,
            (None, None) => {
                return Err(format!(
                    "--workers, --features and --model go with --backlog and --resident\n{USAGE}"
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
            "model",
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
const CLEARING: [(&str, &str, Option<&str>); 9] = [
    ("no-gate", "gate", None),
    ("no-budget", "budget", None),
    ("no-minutes", "budget", Some("minutes")),
    ("no-tool-calls", "budget", Some("tool_calls")),
    ("no-daily-usd", "budget", Some("daily_usd")),
    ("no-objectives", "objectives", None),
    ("no-workers", "objectives", Some("workers")),
    ("no-features", "objectives", Some("features")),
    ("no-model", "objectives", Some("model")),
];

/// The change `spawn mission configure` asks for (objective 027): only the
/// settings its flags name, for `spawn_mission_reconfigure` to merge into
/// the mission's own under the append lock; each `--no-<setting>` switch
/// sets its setting to null, which clears it. A setting both given and
/// cleared, or a flag configure does not know, is refused. No flag at all is
/// an empty change, which changes and records nothing.
fn change(flags: &Flags) -> Result<Value, String> {
    const KNOWN: [&str; 11] = [
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
        "model",
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
    if let Some(text) = flags.get("model") {
        let parsed = ModelProfile::parse(&text)
            .ok_or_else(|| format!("--model is default or cheap, not {text}\n{USAGE}"))?;
        part("objectives", Some("model"), json!(parsed.as_str()));
    }
    apply_clearing_switches(flags, &mut part)?;
    Ok(Value::Object(change))
}

fn apply_clearing_switches(
    flags: &Flags,
    part: &mut impl FnMut(&str, Option<&str>, Value),
) -> Result<(), String> {
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
        ("model", "objectives", Some("model")),
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
    Ok(())
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

/// Read the mission board (`store.mission_board(limit)`), which projects
/// task grading metadata (`difficulty`, `routed_difficulty`, `preset_id`,
/// `first_preset_id`, `tier_role`, `quality_rejections`, and
/// `rework_step_up`) natively in SQL.
pub(crate) fn list_board(
    store: &spawn_postgres::PostgresStore,
    limit: i64,
) -> Result<Value, String> {
    store
        .mission_board(limit)
        .map_err(|error| error.to_string())
}

fn mission_summary_line(mission: &Value) -> &str {
    mission["text"]
        .as_str()
        .and_then(|text| text.lines().map(str::trim).find(|line| !line.is_empty()))
        .unwrap_or("(untitled)")
}

/// Human-readable `spawn mission list --table` view showing missions,
/// tasks, grading (`difficulty`, `tier_role`, `preset_id`) and rework step-up history.
fn format_mission_board_table(board: &Value) -> String {
    let missions = board["missions"].as_array().map_or(&[][..], Vec::as_slice);
    if missions.is_empty() {
        return "(no missions)".to_string();
    }
    let mut lines = Vec::new();
    for (idx, mission) in missions.iter().enumerate() {
        if idx > 0 {
            lines.push(String::new());
        }
        let mission_id = mission["id"]
            .as_i64()
            .map_or_else(|| "?".to_string(), |id| id.to_string());
        let state = mission["state"].as_str().unwrap_or("-");
        lines.push(format!(
            "Mission #{mission_id} [{state}] {}",
            mission_summary_line(mission)
        ));
        let tasks = mission["tasks"].as_array().map_or(&[][..], Vec::as_slice);
        if tasks.is_empty() {
            lines.push("  (no tasks)".to_string());
            continue;
        }
        lines.push(format!(
            "  {:<6} {:<10} {:<7} {:<4} {:<18} {:<24} {}",
            "TASK", "STATE", "MODEL", "DIFF", "TIER", "PRESET", "REWORK / AGENT"
        ));
        for task in tasks {
            let task_id = task["id"]
                .as_i64()
                .map_or_else(|| "T?".to_string(), |id| format!("T{id}"));
            let t_state = task["state"].as_str().unwrap_or("-");
            let model = task["model"].as_str().unwrap_or("-");
            let diff = task["difficulty"].as_str().unwrap_or("-");
            let tier = task["tier_role"].as_str().unwrap_or("-");
            let preset = task["preset_id"].as_str().unwrap_or("-");
            let rework = task["rework_step_up"].as_object().map(|step| {
                let from_d = step
                    .get("from_difficulty")
                    .and_then(Value::as_str)
                    .unwrap_or("-");
                let to_d = step
                    .get("routed_difficulty")
                    .and_then(Value::as_str)
                    .unwrap_or("-");
                let rej = step
                    .get("quality_rejections")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                let from_p = step
                    .get("first_preset_id")
                    .and_then(Value::as_str)
                    .unwrap_or("-");
                let to_p = step.get("preset_id").and_then(Value::as_str).unwrap_or("-");
                format!("step-up {from_d}->{to_d}, {rej} rej, {from_p}->{to_p}")
            });
            let detail = match (rework, task["agent_id"].as_str()) {
                (Some(rw), Some(agent)) if !agent.is_empty() => format!("{rw} ({agent})"),
                (Some(rw), _) => rw,
                (None, Some(agent)) if !agent.is_empty() => agent.to_string(),
                _ => "-".to_string(),
            };
            lines.push(format!(
                "  {task_id:<6} {t_state:<10} {model:<7} {diff:<4} {tier:<18} {preset:<24} {detail}"
            ));
        }
    }
    lines.join("\n")
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
        "episodes" => return episodes_cmd(&store, &flags),
        "show" => print(&show(&store, &flags)?),
        "list" => {
            if flags.switch("json") && flags.switch("table") {
                return Err("use either --json or --table, not both".to_string());
            }
            let limit = flags
                .get("limit")
                .and_then(|text| text.parse().ok())
                .unwrap_or(20);
            let board = list_board(&store, limit)?;
            if flags.switch("table") {
                println!("{}", format_mission_board_table(&board));
            } else {
                print(&board);
            }
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

fn parse_optional_mission_id(flags: &Flags) -> Result<Option<i64>, String> {
    flags
        .get("mission")
        .map(|raw| {
            raw.parse::<i64>()
                .map_err(|_| format!("--mission must be a number, not {raw}"))
        })
        .transpose()
}

fn build_episodes_cli_payload(
    mission_id: Option<i64>,
    strict: bool,
    episodes: &[spawn_core::control::TaskEpisode],
    summary: &Value,
    parity: Option<(bool, &[Value])>,
) -> Value {
    let mut payload = json!({
        "mission": mission_id,
        "strict": strict,
        "episodes": episodes,
        "summary": summary,
        "warnings": summary["warnings"].as_u64().unwrap_or(0),
    });
    if let Some((parity_ok, drifts)) = parity
        && let Some(obj) = payload.as_object_mut()
    {
        obj.insert("parity_ok".to_string(), json!(parity_ok));
        obj.insert("drift_count".to_string(), json!(drifts.len()));
        obj.insert("drifts".to_string(), json!(drifts));
    }
    payload
}

fn format_episodes_text(
    episodes: &[spawn_core::control::TaskEpisode],
    summary: &Value,
    parity: Option<(bool, &[Value])>,
) -> String {
    let mut lines = vec![format!(
        "episodes: total={} landed={} accepted={} in_flight={} rejected={} cancelled={} aborted={} over_budget={} rework={} reroutes={} reviews={} gates={} warnings={}",
        summary["total"].as_u64().unwrap_or(0),
        summary["landed"].as_u64().unwrap_or(0),
        summary["accepted"].as_u64().unwrap_or(0),
        summary["in_flight"].as_u64().unwrap_or(0),
        summary["rejected"].as_u64().unwrap_or(0),
        summary["cancelled"].as_u64().unwrap_or(0),
        summary["aborted"].as_u64().unwrap_or(0),
        summary["over_budget"].as_u64().unwrap_or(0),
        summary["total_rework"].as_u64().unwrap_or(0),
        summary["total_reroutes"].as_u64().unwrap_or(0),
        summary["review_sync_points"].as_u64().unwrap_or(0),
        summary["gate_verdicts"].as_u64().unwrap_or(0),
        summary["warnings"].as_u64().unwrap_or(0),
    )];

    for ep in episodes {
        let preset = ep.preset_id.as_deref().unwrap_or("-");
        lines.push(format!(
            "  task=#{} mission=#{} state={} outcome={:?} diff={} preset={} rework={} rej={} reroutes={} reviews={} gates={}",
            ep.task_id,
            ep.mission_id,
            ep.state.as_str(),
            ep.outcome,
            ep.difficulty.as_str(),
            preset,
            ep.rework,
            ep.quality_rejections,
            ep.reroutes,
            ep.review_sync_points.len(),
            ep.gate_verdicts.len(),
        ));
    }

    if let Some((parity_ok, drifts)) = parity {
        if parity_ok {
            lines.push(format!(
                "parity: OK ({} task(s) matched mission_tasks)",
                episodes.len()
            ));
        } else {
            lines.push(format!("parity: DRIFT ({} mismatch(es)):", drifts.len()));
            for d in drifts {
                lines.push(format!(
                    "  task=#{} mission=#{} field={} replayed={} sql={}",
                    d["task_id"],
                    d["mission_id"],
                    d["field"].as_str().unwrap_or("?"),
                    d["replayed"],
                    d["sql"]
                ));
            }
        }
    }

    lines.join("\n")
}

/// `spawn mission episodes [--mission N] [--strict] [--verify-parity] [--json]`
fn episodes_cmd(store: &spawn_postgres::PostgresStore, flags: &Flags) -> Result<i32, String> {
    let mission_id = parse_optional_mission_id(flags)?;
    let strict = flags.switch("strict");
    let verify_parity = flags.switch("verify-parity");
    let as_json = flags.switch("json");
    let err = |error: spawn_core::port::StoreError| error.to_string();

    if verify_parity {
        let (episodes, summary, parity_ok, drifts) = store
            .episode_parity_report(mission_id, strict)
            .map_err(err)?;
        if as_json {
            let payload = build_episodes_cli_payload(
                mission_id,
                strict,
                &episodes,
                &summary,
                Some((parity_ok, &drifts)),
            );
            print(&payload);
        } else {
            println!(
                "{}",
                format_episodes_text(&episodes, &summary, Some((parity_ok, &drifts)))
            );
        }
        return Ok(i32::from(!parity_ok));
    }

    let (episodes, summary) = store.replay_episodes(mission_id, strict).map_err(err)?;
    if as_json {
        let payload = build_episodes_cli_payload(mission_id, strict, &episodes, &summary, None);
        print(&payload);
    } else {
        println!("{}", format_episodes_text(&episodes, &summary, None));
    }
    Ok(0)
}

fn parse_workspace_kind(flags: &Flags) -> Result<WorkspaceKind, String> {
    match flags.get("kind") {
        Some(raw) => raw
            .trim()
            .parse::<WorkspaceKind>()
            .map_err(|error| format!("--kind {error}")),
        None => Ok(WorkspaceKind::Code),
    }
}

/// `spawn mission add`: the mission, then every setting the command line
/// gave it. A setting is never dropped: `Settings::from` refuses objectives
/// without a gate (they could never land), and `given` is true for any
/// other setting so `--accept` or a budget is stored too.
fn add(store: &spawn_postgres::PostgresStore, flags: &Flags) -> Result<i64, String> {
    // Checked here for a clear error; spawn_mission_add holds the same rule
    // for every caller.
    let kind = parse_workspace_kind(flags)?;
    let raw_text = text(flags, "text")?;
    let body = WorkspaceKind::strip_workspace_tags(&raw_text);
    if body.trim().is_empty() {
        return Err("a mission needs text".to_string());
    }
    let stored_text = kind.encode_mission_text(body);
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
        .mission_add(&stored_text, &sources, "human")
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
    if let Some((verb, tail)) = rest.split_first() {
        match verb.as_str() {
            "commits" => return task_commits_cmd(tail),
            "diff" => return task_diff_cmd(tail),
            "land" => return task_land_cmd(tail),
            _ => {}
        }
    }
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
            let stdin = std::io::stdin();
            let mut lock = stdin.lock();
            let reason = resolve_optional_text_or_file(&flags, "reason", "reason-file", &mut lock)?;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LandMode {
    FfOnly,
    Squash,
}

impl LandMode {
    const fn as_str(self) -> &'static str {
        match self {
            Self::FfOnly => "ff-only",
            Self::Squash => "squash",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct TaskCommitsArgs {
    task_id: i64,
    json: bool,
    root: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct TaskDiffArgs {
    task_id: i64,
    stat: bool,
    root: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct TaskLandArgs {
    task_id: i64,
    mode: LandMode,
    dry_run: bool,
    json: bool,
    root: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReviewCommit {
    sha: String,
    short_sha: String,
    author_name: String,
    author_email: String,
    authored_at: String,
    subject: String,
    is_checkpoint: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum TaskRepo {
    LocalGit {
        git_dir: std::path::PathBuf,
        bare: bool,
    },
    D6Ledger {
        project: String,
    },
}

impl TaskRepo {
    fn local_git_dir(&self) -> Option<&std::path::Path> {
        match self {
            Self::LocalGit { git_dir, .. } => Some(git_dir.as_path()),
            Self::D6Ledger { .. } => None,
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::LocalGit { git_dir, .. } => format!("local repository {}", git_dir.display()),
            Self::D6Ledger { project } => format!("D6 ledger project {project:?}"),
        }
    }
}

struct TaskRepoContext {
    task_id: i64,
    mission_id: i64,
    agent_id: String,
    brief: String,
    candidate: Option<String>,
    repo: TaskRepo,
    root: Option<std::path::PathBuf>,
    source: Option<String>,
}

impl TaskRepoContext {
    fn is_landable(&self) -> bool {
        let allowed_source = self.source.is_none()
            || self
                .source
                .as_deref()
                .is_some_and(|path| path.starts_with('/') && !path.contains("://"));
        match &self.repo {
            TaskRepo::LocalGit { bare, .. } => *bare && allowed_source,
            TaskRepo::D6Ledger { .. } => allowed_source,
        }
    }

    fn storage(&self, has_d6_refs: bool) -> crate::sources::ProjectStorage {
        crate::sources::project_storage(crate::sources::ProjectStorageQuery {
            root: self.root.as_deref(),
            git_dir: self.repo.local_git_dir(),
            ledger_only_d6: matches!(self.repo, TaskRepo::D6Ledger { .. }) && has_d6_refs,
        })
    }
}

struct TempDirGuard {
    path: std::path::PathBuf,
}

impl TempDirGuard {
    fn create(prefix: &str) -> Result<Self, String> {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("{prefix}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&path)
            .map_err(|error| format!("create temporary directory {}: {error}", path.display()))?;
        Ok(Self { path })
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn parse_positive_task_id(raw: &str, flag_hint: &str) -> Result<i64, String> {
    match raw.parse::<i64>() {
        Ok(value) if value > 0 => Ok(value),
        _ => Err(format!("{flag_hint} must be a positive number, not {raw}")),
    }
}

fn parse_task_review_common(
    rest: &[String],
    synopsis: &str,
    allowed_switches: &[&str],
) -> Result<(i64, Option<String>, std::collections::BTreeSet<String>), String> {
    let mut positional_id: Option<i64> = None;
    let mut flag_id: Option<i64> = None;
    let mut root: Option<String> = None;
    let mut switches = std::collections::BTreeSet::new();

    let mut iter = rest.iter();
    while let Some(word) = iter.next() {
        match word.as_str() {
            flag @ ("--id" | "--root") => {
                let Some(value) = iter.next() else {
                    return Err(format!("{flag} needs a value\n{synopsis}"));
                };
                if value.starts_with("--") {
                    return Err(format!("{flag} needs a value, found {value}\n{synopsis}"));
                }
                if flag == "--id" {
                    flag_id = Some(parse_positive_task_id(value, "--id")?);
                } else {
                    root = Some(value.clone());
                }
            }
            word if word.starts_with("--") => {
                let name = &word[2..];
                if allowed_switches.contains(&name) {
                    switches.insert(name.to_string());
                } else {
                    return Err(format!("unknown flag {word}\n{synopsis}"));
                }
            }
            word if word.starts_with('-') => {
                return Err(format!("unknown flag {word}\n{synopsis}"));
            }
            positional => {
                if positional_id.is_some() {
                    return Err(format!(
                        "unexpected positional argument {positional}\n{synopsis}"
                    ));
                }
                positional_id = Some(parse_positive_task_id(positional, "<id>")?);
            }
        }
    }

    let task_id = match (positional_id, flag_id) {
        (Some(_), Some(_)) => {
            return Err(format!("give <id> once (positional or --id)\n{synopsis}"));
        }
        (Some(value), None) | (None, Some(value)) => value,
        (None, None) => return Err(format!("task <id> is required\n{synopsis}")),
    };
    Ok((task_id, root, switches))
}

fn parse_task_commits_args(rest: &[String]) -> Result<TaskCommitsArgs, String> {
    let synopsis = "usage: spawn task commits <id> [--json]";
    let (task_id, root, switches) = parse_task_review_common(rest, synopsis, &["json"])?;
    Ok(TaskCommitsArgs {
        task_id,
        json: switches.contains("json"),
        root,
    })
}

fn parse_task_diff_args(rest: &[String]) -> Result<TaskDiffArgs, String> {
    let synopsis = "usage: spawn task diff <id> [--stat]";
    let (task_id, root, switches) = parse_task_review_common(rest, synopsis, &["stat"])?;
    Ok(TaskDiffArgs {
        task_id,
        stat: switches.contains("stat"),
        root,
    })
}

fn parse_task_land_args(rest: &[String]) -> Result<TaskLandArgs, String> {
    let synopsis = "usage: spawn task land <id> [--ff-only | --squash] [--dry-run] [--json]";
    let (task_id, root, switches) =
        parse_task_review_common(rest, synopsis, &["ff-only", "squash", "dry-run", "json"])?;
    let ff_only = switches.contains("ff-only");
    let squash = switches.contains("squash");
    if ff_only && squash {
        return Err(format!(
            "use either --ff-only or --squash, not both\n{synopsis}"
        ));
    }
    let mode = if squash {
        LandMode::Squash
    } else {
        LandMode::FfOnly
    };
    Ok(TaskLandArgs {
        task_id,
        mode,
        dry_run: switches.contains("dry-run"),
        json: switches.contains("json"),
        root,
    })
}

fn run_git_raw(git_dir: &std::path::Path, args: &[&str], context: &str) -> Result<String, String> {
    let output = crate::sources::controller_git()
        .arg("--git-dir")
        .arg(git_dir)
        .args(args)
        .output()
        .map_err(|error| format!("run git {context}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {context} in {}: {}",
            git_dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn run_work_git(
    work_dir: &std::path::Path,
    args: &[&str],
    context: &str,
) -> Result<String, String> {
    let output = crate::sources::controller_git()
        .arg("-C")
        .arg(work_dir)
        .args(args)
        .output()
        .map_err(|error| format!("run git {context}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {context}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn verify_bare_repo(path: &std::path::Path) -> Result<bool, String> {
    let out = run_git_raw(
        path,
        &["rev-parse", "--is-bare-repository"],
        "rev-parse --is-bare-repository",
    )?;
    Ok(out.trim() == "true")
}

fn resolve_local_source_repo(
    source_path: &std::path::Path,
) -> Result<(std::path::PathBuf, bool), String> {
    if !source_path.exists() {
        return Err(format!(
            "mission repository {} does not exist",
            source_path.display()
        ));
    }
    let git_dir = if source_path.join("HEAD").is_file() && source_path.join("objects").is_dir() {
        source_path.to_path_buf()
    } else if source_path.join(".git").exists() {
        source_path.join(".git")
    } else {
        source_path.to_path_buf()
    };
    let is_bare = verify_bare_repo(&git_dir)?;
    Ok((git_dir, is_bare))
}

fn load_task_repo(
    store: &spawn_postgres::PostgresStore,
    task_id: i64,
    root_override: Option<&str>,
) -> Result<TaskRepoContext, String> {
    let err = |error: spawn_core::port::StoreError| error.to_string();
    let task = store
        .objective_task(task_id)
        .map_err(err)?
        .ok_or_else(|| format!("task #{task_id} not found"))?;
    let mission_id = task["mission_id"]
        .as_i64()
        .ok_or_else(|| format!("task #{task_id} has no mission_id"))?;
    let agent_id = task["agent_id"].as_str().unwrap_or_default().to_string();
    let brief = task["brief"].as_str().unwrap_or_default().to_string();
    let candidate = task["candidate"].as_str().map(str::to_string);
    let mission = store
        .mission_by_id(mission_id)
        .map_err(err)?
        .ok_or_else(|| format!("mission #{mission_id} for task #{task_id} not found"))?;
    let source = mission["sources"]
        .as_array()
        .and_then(|sources| sources.first())
        .and_then(Value::as_str);
    let resolved_root = root_override
        .map(std::path::PathBuf::from)
        .or_else(|| crate::config::env_value("SPAWN_WORKSPACE_ROOT").map(std::path::PathBuf::from));

    let repo = match source {
        Some(path) if path.starts_with('/') && !path.contains("://") => {
            match resolve_local_source_repo(std::path::Path::new(path)) {
                Ok((git_dir, bare)) => TaskRepo::LocalGit { git_dir, bare },
                Err(local_err) => {
                    let probe = std::path::Path::new(path);
                    let candidates =
                        d6_project_candidates(Some(path), Some(probe), resolved_root.as_deref());
                    if let Some((project, _)) = load_d6_project_refs(store, &candidates)? {
                        TaskRepo::D6Ledger { project }
                    } else {
                        return Err(local_err);
                    }
                }
            }
        }
        other_source => {
            if let Some(root_path) = resolved_root.as_deref() {
                let mirror = crate::sources::mirror_path(root_path, other_source)
                    .map_err(|error| error.to_string())?;
                if mirror.exists() {
                    let bare = verify_bare_repo(&mirror)?;
                    TaskRepo::LocalGit {
                        git_dir: mirror,
                        bare,
                    }
                } else {
                    let candidates =
                        d6_project_candidates(other_source, Some(&mirror), Some(root_path));
                    if let Some((project, _)) = load_d6_project_refs(store, &candidates)? {
                        TaskRepo::D6Ledger { project }
                    } else {
                        return Err(format!(
                            "mission bare repository {} does not exist",
                            mirror.display()
                        ));
                    }
                }
            } else {
                let candidates = d6_project_candidates(other_source, None, None);
                if !candidates.is_empty()
                    && let Some((project, _)) = load_d6_project_refs(store, &candidates)?
                {
                    TaskRepo::D6Ledger { project }
                } else {
                    return Err(format!(
                        "mission #{mission_id} has no local source directory; pass --root <dir> or set SPAWN_WORKSPACE_ROOT"
                    ));
                }
            }
        }
    };

    Ok(TaskRepoContext {
        task_id,
        mission_id,
        agent_id,
        brief,
        candidate,
        repo,
        root: resolved_root,
        source: source.map(str::to_string),
    })
}

fn candidate_task_refs(task_id: i64, agent_id: &str) -> Vec<String> {
    spawn_host::workspace_rules::candidate_task_refs(task_id, agent_id)
}

#[derive(Debug, Clone)]
struct ResolvedTaskRefs {
    base_sha: String,
    resolved_ref: String,
    head_sha: String,
    storage: crate::sources::ProjectStorage,
    d6_project: Option<String>,
    base_snapshot: Option<spawn_cas::Digest>,
    head_snapshot: Option<spawn_cas::Digest>,
}

fn push_unique_project(candidates: &mut Vec<String>, value: &str) {
    if !value.is_empty() && !candidates.iter().any(|existing| existing == value) {
        candidates.push(value.to_string());
    }
}

fn push_path_candidates(candidates: &mut Vec<String>, path: &std::path::Path) {
    push_unique_project(candidates, &path.to_string_lossy());
    if let Ok(canonical) = std::fs::canonicalize(path) {
        push_unique_project(candidates, &canonical.to_string_lossy());
    }
}

fn d6_project_candidates(
    source: Option<&str>,
    git_dir: Option<&std::path::Path>,
    root: Option<&std::path::Path>,
) -> Vec<String> {
    let mut candidates = Vec::new();
    if let Some(dir) = git_dir {
        push_path_candidates(&mut candidates, dir);
    }
    if let Some(src) = source {
        push_unique_project(&mut candidates, src);
        let trimmed = src.trim_end_matches('/');
        push_unique_project(&mut candidates, trimmed);
        if src.starts_with('/') && !src.contains("://") {
            push_path_candidates(&mut candidates, std::path::Path::new(src));
        }
        if let Some(root_dir) = root
            && let Ok(mirror) = crate::sources::mirror_path(root_dir, Some(src))
        {
            push_path_candidates(&mut candidates, &mirror);
        }
    } else if let Some(root_dir) = root
        && let Ok(mirror) = crate::sources::mirror_path(root_dir, None)
    {
        push_path_candidates(&mut candidates, &mirror);
    }
    candidates
}

fn load_d6_project_refs(
    store: &spawn_postgres::PostgresStore,
    candidates: &[String],
) -> Result<Option<(String, Vec<spawn_postgres::RefRow>)>, String> {
    for project in candidates {
        let rows = store
            .list_refs(Some(project))
            .map_err(|error| format!("list D6 refs for {project}: {error}"))?;
        if !rows.is_empty() {
            return Ok(Some((project.clone(), rows)));
        }
    }
    Ok(None)
}

fn resolve_snapshot_target(
    store: &spawn_postgres::PostgresStore,
    row: &spawn_postgres::RefRow,
) -> Result<(String, spawn_cas::Digest), String> {
    let snapshot = store
        .get_snapshot(&row.snapshot)
        .map_err(|error| format!("read D6 snapshot {}: {error}", row.snapshot))?
        .ok_or_else(|| {
            format!(
                "D6 ref '{}' points to missing snapshot {}",
                row.name, row.snapshot
            )
        })?;
    if !is_commit(&snapshot.git_oid) {
        return Err(format!(
            "D6 snapshot {} has invalid git_oid {:?}",
            snapshot.id, snapshot.git_oid
        ));
    }
    let digest = spawn_cas::Digest::parse(&row.snapshot)
        .map_err(|error| format!("invalid D6 snapshot digest {}: {error}", row.snapshot))?;
    Ok((snapshot.git_oid, digest))
}

fn resolve_main_from_d6_ledger(
    store: &spawn_postgres::PostgresStore,
    project: &str,
    rows: &[spawn_postgres::RefRow],
) -> Result<(String, spawn_cas::Digest), String> {
    let main_row = rows
        .iter()
        .find(|row| {
            matches!(row.kind, spawn_postgres::RefKind::Main)
                || row.name == "main"
                || row.name == "refs/heads/main"
        })
        .ok_or_else(|| format!("main ref not found in D6 ledger for project {project}"))?;
    resolve_snapshot_target(store, main_row)
}

fn match_d6_ref<'a>(
    rows: &'a [spawn_postgres::RefRow],
    aliases: &[&str],
) -> Option<&'a spawn_postgres::RefRow> {
    for alias in aliases {
        if let Some(row) = rows
            .iter()
            .find(|row| !matches!(row.kind, spawn_postgres::RefKind::Wip) && row.name == *alias)
        {
            return Some(row);
        }
    }
    None
}

fn resolve_preserved_ref_from_d6_ledger(
    store: &spawn_postgres::PostgresStore,
    rows: &[spawn_postgres::RefRow],
    task_id: i64,
    agent_id: &str,
) -> Result<Option<(String, String, spawn_cas::Digest)>, String> {
    let candidates = candidate_task_refs(task_id, agent_id);
    for display_ref in candidates {
        let short_ref = display_ref.strip_prefix("refs/").unwrap_or(&display_ref);
        let row = if let Some(branch_name) =
            display_ref.strip_prefix(spawn_host::workspace_rules::SPAWN_BRANCH_REF_PREFIX)
        {
            let short_branch = format!("spawn/{branch_name}");
            match_d6_ref(
                rows,
                &[display_ref.as_str(), short_branch.as_str(), branch_name],
            )
        } else {
            match_d6_ref(rows, &[display_ref.as_str(), short_ref])
        };
        if let Some(row) = row {
            let (git_oid, digest) = resolve_snapshot_target(store, row)?;
            return Ok(Some((display_ref, git_oid, digest)));
        }
    }
    Ok(None)
}

fn lookup_snapshot_digest_by_oid(
    store: &spawn_postgres::PostgresStore,
    git_oid: &str,
) -> Result<Option<spawn_cas::Digest>, String> {
    let Some(row) = store
        .get_snapshot_by_oid(git_oid)
        .map_err(|error| format!("lookup D6 snapshot for commit {git_oid}: {error}"))?
    else {
        return Ok(None);
    };
    spawn_cas::Digest::parse(&row.id)
        .map(Some)
        .map_err(|error| format!("invalid D6 snapshot digest {}: {error}", row.id))
}

fn lookup_task_repo_d6_refs(
    store: &spawn_postgres::PostgresStore,
    ctx: &TaskRepoContext,
) -> Result<Option<(String, Vec<spawn_postgres::RefRow>)>, String> {
    match &ctx.repo {
        TaskRepo::D6Ledger { project } => {
            let rows = store
                .list_refs(Some(project))
                .map_err(|error| format!("list D6 refs for {project}: {error}"))?;
            Ok((!rows.is_empty()).then(|| (project.clone(), rows)))
        }
        TaskRepo::LocalGit { git_dir, .. } => {
            let candidates = d6_project_candidates(
                ctx.source.as_deref(),
                Some(git_dir.as_path()),
                ctx.root.as_deref(),
            );
            load_d6_project_refs(store, &candidates)
        }
    }
}

fn resolve_task_refs(
    store: &spawn_postgres::PostgresStore,
    ctx: &TaskRepoContext,
) -> Result<ResolvedTaskRefs, String> {
    let d6_project_refs = lookup_task_repo_d6_refs(store, ctx)?;
    let local_git_dir = ctx.repo.local_git_dir();
    let storage = ctx.storage(d6_project_refs.is_some());
    let d6_project = d6_project_refs
        .as_ref()
        .map(|(project, _)| project.clone())
        .or_else(|| match &ctx.repo {
            TaskRepo::D6Ledger { project } => Some(project.clone()),
            TaskRepo::LocalGit { .. } => None,
        });

    let (base_sha, base_snapshot) = match (&d6_project_refs, local_git_dir) {
        (Some((project, rows)), None) => {
            let (oid, digest) = resolve_main_from_d6_ledger(store, project, rows)?;
            (oid, Some(digest))
        }
        (Some((project, rows)), Some(dir)) if storage.is_d6() => {
            match resolve_main_from_d6_ledger(store, project, rows) {
                Ok((oid, digest)) => (oid, Some(digest)),
                Err(_) => (resolve_main_sha(dir)?, None),
            }
        }
        (Some((project, rows)), Some(dir)) => {
            if let Ok(oid) = resolve_main_sha(dir) {
                (oid, None)
            } else {
                let (oid, digest) = resolve_main_from_d6_ledger(store, project, rows)?;
                (oid, Some(digest))
            }
        }
        (None, Some(dir)) => (resolve_main_sha(dir)?, None),
        (None, None) => {
            return Err(format!(
                "no D6 ledger refs or local repository found for task #{}",
                ctx.task_id
            ));
        }
    };

    let (resolved_ref, head_sha, head_snapshot) = match (&d6_project_refs, local_git_dir) {
        (Some((_, rows)), None) => {
            if let Some((r, oid, digest)) =
                resolve_preserved_ref_from_d6_ledger(store, rows, ctx.task_id, &ctx.agent_id)?
            {
                (r, oid, Some(digest))
            } else {
                let checked = candidate_task_refs(ctx.task_id, &ctx.agent_id).join(", ");
                return Err(format!(
                    "no preserved commits found in D6 ledger for task #{} (checked: {checked})",
                    ctx.task_id
                ));
            }
        }
        (Some((_, rows)), Some(dir)) if storage.is_d6() => {
            if let Some((r, oid, digest)) =
                resolve_preserved_ref_from_d6_ledger(store, rows, ctx.task_id, &ctx.agent_id)?
            {
                (r, oid, Some(digest))
            } else {
                let (r, oid) = resolve_preserved_ref(dir, ctx.task_id, &ctx.agent_id)?;
                (r, oid, None)
            }
        }
        (Some((_, rows)), Some(dir)) => {
            if let Ok((r, oid)) = resolve_preserved_ref(dir, ctx.task_id, &ctx.agent_id) {
                (r, oid, None)
            } else if let Some((r, oid, digest)) =
                resolve_preserved_ref_from_d6_ledger(store, rows, ctx.task_id, &ctx.agent_id)?
            {
                (r, oid, Some(digest))
            } else {
                let (r, oid) = resolve_preserved_ref(dir, ctx.task_id, &ctx.agent_id)?;
                (r, oid, None)
            }
        }
        (None, Some(dir)) => {
            let (r, oid) = resolve_preserved_ref(dir, ctx.task_id, &ctx.agent_id)?;
            (r, oid, None)
        }
        (None, None) => {
            return Err(format!(
                "no D6 ledger refs or local repository found for task #{}",
                ctx.task_id
            ));
        }
    };

    Ok(ResolvedTaskRefs {
        base_sha,
        resolved_ref,
        head_sha,
        storage,
        d6_project,
        base_snapshot,
        head_snapshot,
    })
}

fn resolve_d6_cas_dir(
    ctx: &TaskRepoContext,
    cas_dir_override: Option<&std::path::Path>,
) -> Result<std::path::PathBuf, String> {
    if let Some(dir) = cas_dir_override {
        return Ok(dir.to_path_buf());
    }
    let cfg = crate::config::D6RuntimeConfig::from_env()?;
    if let Some(root) = ctx.root.as_deref() {
        return Ok(cfg.effective_cas_dir(root, spawn_host::WorkspaceBackend::D6));
    }
    cfg.cas_dir.ok_or_else(|| {
        format!(
            "cannot export D6 commits for task #{}: pass --root <dir> or set SPAWN_WORKSPACE_ROOT / SPAWN_D6_CAS_DIR",
            ctx.task_id
        )
    })
}

fn materialize_review_repo(
    store: &std::sync::Arc<spawn_postgres::PostgresStore>,
    ctx: &TaskRepoContext,
    resolved: &ResolvedTaskRefs,
    cas_dir_override: Option<&std::path::Path>,
) -> Result<(Option<TempDirGuard>, std::path::PathBuf), String> {
    if let Some(git_dir) = ctx.repo.local_git_dir()
        && rev_parse_commit(git_dir, &resolved.base_sha)?
            .as_deref()
            .is_some_and(|oid| oid == resolved.base_sha)
        && rev_parse_commit(git_dir, &resolved.head_sha)?
            .as_deref()
            .is_some_and(|oid| oid == resolved.head_sha)
    {
        return Ok((None, git_dir.to_path_buf()));
    }

    let base_snap = match resolved.base_snapshot.clone() {
        Some(digest) => Some(digest),
        None => lookup_snapshot_digest_by_oid(store, &resolved.base_sha)?,
    }
    .ok_or_else(|| {
        format!(
            "D6 base commit {} for task #{} has no snapshot record for border export",
            resolved.base_sha, ctx.task_id
        )
    })?;
    let head_snap = match resolved.head_snapshot.clone() {
        Some(digest) => Some(digest),
        None => lookup_snapshot_digest_by_oid(store, &resolved.head_sha)?,
    }
    .ok_or_else(|| {
        format!(
            "D6 task commit {} for task #{} has no snapshot record for border export",
            resolved.head_sha, ctx.task_id
        )
    })?;

    let cas_dir = resolve_d6_cas_dir(ctx, cas_dir_override)?;
    if !cas_dir.is_dir() || !cas_dir.join(spawn_cas::CAS_MARKER).is_file() {
        return Err(format!(
            "D6 CAS store missing or uninitialized at {} while materializing task #{}",
            cas_dir.display(),
            ctx.task_id
        ));
    }
    let cas = spawn_host::CasMerkleStore::open(&cas_dir)
        .map_err(|error| format!("open D6 CAS store at {}: {error}", cas_dir.display()))?;
    let ledger = crate::sources::PostgresRefLedger::new(std::sync::Arc::clone(store));
    let guard = TempDirGuard::create("spawn-task-d6")?;
    let bare_dir = guard.path.join("repo.git");
    let mut targets = vec![spawn_host::BorderRefTarget {
        git_ref: "refs/heads/main".to_string(),
        snapshot: base_snap,
    }];
    if resolved.resolved_ref != "refs/heads/main" {
        targets.push(spawn_host::BorderRefTarget {
            git_ref: resolved.resolved_ref.clone(),
            snapshot: head_snap,
        });
    }
    spawn_host::GitBorderAdapter::new()
        .export_refs(&cas, Some(&ledger), &bare_dir, &targets)
        .map_err(|error| {
            format!(
                "export D6 commits for task #{} via Git border adapter: {error}",
                ctx.task_id
            )
        })?;
    Ok((Some(guard), bare_dir))
}

fn rev_parse_commit(git_dir: &std::path::Path, rev: &str) -> Result<Option<String>, String> {
    let target = format!("{rev}^{{commit}}");
    let output = crate::sources::controller_git()
        .arg("--git-dir")
        .arg(git_dir)
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &target,
        ])
        .output()
        .map_err(|error| format!("run git rev-parse: {error}"))?;
    if !output.status.success() {
        return Ok(None);
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(is_commit(&sha).then_some(sha))
}

fn resolve_main_sha(git_dir: &std::path::Path) -> Result<String, String> {
    rev_parse_commit(git_dir, "refs/heads/main")?.ok_or_else(|| {
        format!(
            "refs/heads/main not found in mission repository {}",
            git_dir.display()
        )
    })
}

fn resolve_preserved_ref(
    git_dir: &std::path::Path,
    task_id: i64,
    agent_id: &str,
) -> Result<(String, String), String> {
    let candidates = candidate_task_refs(task_id, agent_id);
    for reference in &candidates {
        if let Some(sha) = rev_parse_commit(git_dir, reference)? {
            return Ok((reference.clone(), sha));
        }
    }
    Err(format!(
        "no preserved ref found for task #{task_id} in {} (checked: {})",
        git_dir.display(),
        candidates.join(", ")
    ))
}

fn is_checkpoint_subject(subject: &str) -> bool {
    let trimmed = subject.trim();
    crate::provenance::index::checkpoint_execution(trimmed).is_some()
        || trimmed.eq_ignore_ascii_case("spawn checkpoint")
        || trimmed
            .to_ascii_lowercase()
            .starts_with("spawn checkpoint ")
}

fn list_commits_ahead(
    git_dir: &std::path::Path,
    base_sha: &str,
    head_sha: &str,
) -> Result<Vec<ReviewCommit>, String> {
    let range = format!("{base_sha}..{head_sha}");
    let raw = run_git_raw(
        git_dir,
        &[
            "log",
            "-z",
            "--format=%H%x1f%h%x1f%an%x1f%ae%x1f%aI%x1f%s",
            &range,
            "--",
        ],
        "log",
    )?;
    let mut commits = Vec::new();
    for entry in raw.split('\0') {
        let trimmed = entry.trim_matches(|ch| ch == '\n' || ch == '\r');
        if trimmed.is_empty() {
            continue;
        }
        let mut fields = trimmed.splitn(6, '\x1f');
        let sha = fields.next().unwrap_or_default().to_string();
        let short_sha = fields.next().unwrap_or_default().to_string();
        let author_name = fields.next().unwrap_or_default().to_string();
        let author_email = fields.next().unwrap_or_default().to_string();
        let authored_at = fields.next().unwrap_or_default().to_string();
        let subject = fields.next().unwrap_or_default().to_string();
        let is_checkpoint = is_checkpoint_subject(&subject);
        commits.push(ReviewCommit {
            sha,
            short_sha,
            author_name,
            author_email,
            authored_at,
            subject,
            is_checkpoint,
        });
    }
    Ok(commits)
}

fn commit_counts(commits: &[ReviewCommit]) -> (usize, usize, usize) {
    let total = commits.len();
    let checkpoints = commits.iter().filter(|commit| commit.is_checkpoint).count();
    let authors = total.saturating_sub(checkpoints);
    (total, authors, checkpoints)
}

fn short_sha_prefix(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

fn is_ancestor(
    git_dir: &std::path::Path,
    ancestor: &str,
    descendant: &str,
) -> Result<bool, String> {
    let status = crate::sources::controller_git()
        .arg("--git-dir")
        .arg(git_dir)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .status()
        .map_err(|error| format!("run git merge-base: {error}"))?;
    match status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        other => Err(format!("git merge-base exited unexpectedly ({other:?})")),
    }
}

fn commits_json_payload(
    ctx: &TaskRepoContext,
    resolved_ref: &str,
    base_sha: &str,
    head_sha: &str,
    commits: &[ReviewCommit],
) -> Value {
    let (total, authors, checkpoints) = commit_counts(commits);
    let rows: Vec<Value> = commits
        .iter()
        .map(|commit| {
            json!({
                "sha": commit.sha,
                "short_sha": commit.short_sha,
                "kind": if commit.is_checkpoint { "checkpoint" } else { "author" },
                "is_checkpoint": commit.is_checkpoint,
                "author_name": commit.author_name,
                "author_email": commit.author_email,
                "authored_at": commit.authored_at,
                "subject": commit.subject,
            })
        })
        .collect();
    json!({
        "schema_version": 1,
        "task_id": ctx.task_id,
        "mission_id": ctx.mission_id,
        "agent_id": if ctx.agent_id.is_empty() { Value::Null } else { json!(ctx.agent_id) },
        "resolved_ref": resolved_ref,
        "base_ref": "refs/heads/main",
        "base_sha": base_sha,
        "head_sha": head_sha,
        "commit_count": total,
        "author_commit_count": authors,
        "checkpoint_commit_count": checkpoints,
        "commits": rows,
    })
}

fn format_task_commits_table(
    task_id: i64,
    resolved_ref: &str,
    base_sha: &str,
    head_sha: &str,
    commits: &[ReviewCommit],
) -> String {
    use std::fmt::Write as _;

    let (total, authors, checkpoints) = commit_counts(commits);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "TASK #{task_id}  ref={resolved_ref}  base={}..{}  commits: {total} ({authors} author, {checkpoints} checkpoint)",
        short_sha_prefix(base_sha),
        short_sha_prefix(head_sha)
    );
    if commits.is_empty() {
        out.push_str("  (no commits ahead of refs/heads/main)\n");
        return out;
    }
    let _ = writeln!(
        out,
        "  {:<12} {:<11} {:<22} SUBJECT",
        "SHA", "KIND", "AUTHOR"
    );
    for commit in commits {
        let kind = if commit.is_checkpoint {
            "checkpoint"
        } else {
            "author"
        };
        let _ = writeln!(
            out,
            "  {:<12} {:<11} {:<22} {}",
            short_sha_prefix(&commit.sha),
            kind,
            commit.author_name,
            commit.subject
        );
    }
    out
}

fn task_commits_cmd(rest: &[String]) -> Result<i32, String> {
    let args = parse_task_commits_args(rest)?;
    let store = std::sync::Arc::new(open_store()?);
    let ctx = load_task_repo(&store, args.task_id, args.root.as_deref())?;
    let resolved = resolve_task_refs(&store, &ctx)?;
    let (_guard, review_dir) = materialize_review_repo(&store, &ctx, &resolved, None)?;
    let commits = list_commits_ahead(&review_dir, &resolved.base_sha, &resolved.head_sha)?;
    if args.json {
        print(&commits_json_payload(
            &ctx,
            &resolved.resolved_ref,
            &resolved.base_sha,
            &resolved.head_sha,
            &commits,
        ));
    } else {
        print!(
            "{}",
            format_task_commits_table(
                ctx.task_id,
                &resolved.resolved_ref,
                &resolved.base_sha,
                &resolved.head_sha,
                &commits,
            )
        );
    }
    Ok(0)
}

fn task_diff_cmd(rest: &[String]) -> Result<i32, String> {
    let args = parse_task_diff_args(rest)?;
    let store = std::sync::Arc::new(open_store()?);
    let ctx = load_task_repo(&store, args.task_id, args.root.as_deref())?;
    let resolved = resolve_task_refs(&store, &ctx)?;
    let (_guard, review_dir) = materialize_review_repo(&store, &ctx, &resolved, None)?;
    let range = format!("{}...{}", resolved.base_sha, resolved.head_sha);
    let mut git_args = vec!["diff", "--no-ext-diff"];
    if args.stat {
        git_args.push("--stat");
    }
    git_args.push(&range);
    git_args.push("--");
    let diff = run_git_raw(&review_dir, &git_args, "diff")?;
    print!("{diff}");
    Ok(0)
}

fn build_clean_squash_message(
    task_id: i64,
    brief: &str,
    head_sha: &str,
    commits: &[ReviewCommit],
) -> String {
    use std::fmt::Write as _;

    let author_commits: Vec<&ReviewCommit> = commits
        .iter()
        .filter(|commit| !commit.is_checkpoint && !commit.subject.trim().is_empty())
        .collect();
    let brief_title = brief
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !is_checkpoint_subject(line));
    let subject = if author_commits.len() == 1 {
        author_commits[0].subject.trim().to_string()
    } else if let Some(title) = brief_title {
        format!("{title} (task #{task_id})")
    } else if let Some(first) = author_commits.first() {
        first.subject.trim().to_string()
    } else {
        format!("Land task #{task_id}")
    };

    let mut msg = format!("{subject}\n\n");
    if author_commits.len() > 1 {
        for commit in &author_commits {
            let _ = writeln!(
                msg,
                "- {} {}",
                short_sha_prefix(&commit.sha),
                commit.subject.trim()
            );
        }
        msg.push('\n');
    }
    let _ = writeln!(msg, "Spawn-Task: {task_id}");
    let _ = write!(msg, "Spawn-Head: {head_sha}");
    msg
}

fn squash_author_identity(commits: &[ReviewCommit]) -> (&str, &str) {
    let mut unique: Vec<(&str, &str)> = commits
        .iter()
        .filter(|commit| {
            !commit.is_checkpoint
                && !commit.author_name.is_empty()
                && !commit.author_email.is_empty()
        })
        .map(|commit| (commit.author_name.as_str(), commit.author_email.as_str()))
        .collect();
    unique.sort_unstable();
    unique.dedup();
    if unique.len() == 1 {
        unique[0]
    } else {
        ("spawn", "spawn@localhost")
    }
}

fn prepare_squash_worktree(
    repo_git_dir: &std::path::Path,
    base_sha: &str,
    head_sha: &str,
    task_id: i64,
) -> Result<(TempDirGuard, std::path::PathBuf, String), String> {
    let guard = TempDirGuard::create("spawn-task-land")?;
    let work = guard.path.join("work");
    let init = crate::sources::controller_git()
        .args(["init", "--quiet"])
        .arg(&work)
        .output()
        .map_err(|error| format!("run git init: {error}"))?;
    if !init.status.success() {
        return Err(format!(
            "git init: {}",
            String::from_utf8_lossy(&init.stderr).trim()
        ));
    }
    let canonical_objects = std::fs::canonicalize(repo_git_dir.join("objects"))
        .map_err(|error| format!("canonicalize repo objects: {error}"))?;
    std::fs::write(
        work.join(".git/objects/info/alternates"),
        format!("{}\n", canonical_objects.display()),
    )
    .map_err(|error| format!("write git alternates: {error}"))?;

    run_work_git(
        &work,
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "checkout",
            "--quiet",
            "-B",
            "land",
            base_sha,
        ],
        "checkout base commit",
    )?;
    let merge = crate::sources::controller_git()
        .arg("-C")
        .arg(&work)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "merge",
            "--squash",
            "--no-commit",
            head_sha,
        ])
        .env("GIT_AUTHOR_NAME", "spawn")
        .env("GIT_AUTHOR_EMAIL", "spawn@localhost")
        .env("GIT_COMMITTER_NAME", "spawn")
        .env("GIT_COMMITTER_EMAIL", "spawn@localhost")
        .output()
        .map_err(|error| format!("run git merge --squash: {error}"))?;
    if !merge.status.success() {
        let conflicts = run_work_git(
            &work,
            &["diff", "--name-only", "--diff-filter=U"],
            "list conflicts",
        )
        .unwrap_or_default();
        let detail = if conflicts.trim().is_empty() {
            String::from_utf8_lossy(&merge.stderr).trim().to_string()
        } else {
            format!(
                "conflicting files: {}",
                conflicts.lines().collect::<Vec<_>>().join(", ")
            )
        };
        return Err(format!(
            "cannot squash-land task #{task_id}: merge conflict against refs/heads/main ({detail})"
        ));
    }

    let tree_oid = run_work_git(&work, &["write-tree"], "write-tree")?;
    let base_tree = run_git_raw(
        repo_git_dir,
        &["rev-parse", &format!("{base_sha}^{{tree}}")],
        "rev-parse base tree",
    )?;
    if tree_oid == base_tree.trim() {
        return Err(format!(
            "nothing to squash for task #{task_id}: merged tree matches refs/heads/main"
        ));
    }
    Ok((guard, work, tree_oid))
}

fn commit_squash_and_update_main(
    ctx: &TaskRepoContext,
    git_dir: &std::path::Path,
    base_sha: &str,
    head_sha: &str,
    commits: &[ReviewCommit],
    work: &std::path::Path,
    tree_oid: &str,
) -> Result<String, String> {
    let message = build_clean_squash_message(ctx.task_id, &ctx.brief, head_sha, commits);
    let (author_name, author_email) = squash_author_identity(commits);
    let commit_out = crate::sources::controller_git()
        .arg("-C")
        .arg(work)
        .args([
            "commit-tree",
            "--no-gpg-sign",
            tree_oid,
            "-p",
            base_sha,
            "-m",
            &message,
        ])
        .env("GIT_AUTHOR_NAME", author_name)
        .env("GIT_AUTHOR_EMAIL", author_email)
        .env("GIT_COMMITTER_NAME", "spawn")
        .env("GIT_COMMITTER_EMAIL", "spawn@localhost")
        .output()
        .map_err(|error| format!("run git commit-tree: {error}"))?;
    if !commit_out.status.success() {
        return Err(format!(
            "git commit-tree: {}",
            String::from_utf8_lossy(&commit_out.stderr).trim()
        ));
    }
    let new_commit = String::from_utf8_lossy(&commit_out.stdout)
        .trim()
        .to_string();
    run_work_git(
        work,
        &["update-ref", "refs/heads/squash-land", &new_commit],
        "update temp ref",
    )?;
    let work_str = work.to_string_lossy().to_string();
    run_git_raw(
        git_dir,
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            &work_str,
            "refs/heads/squash-land",
        ],
        "fetch squash commit into mission repo",
    )?;
    run_git_raw(
        git_dir,
        &["update-ref", "refs/heads/main", &new_commit, base_sha],
        "update-ref refs/heads/main",
    )?;
    Ok(new_commit)
}

fn task_land_payload(
    ctx: &TaskRepoContext,
    resolved_ref: &str,
    base_sha: &str,
    head_sha: &str,
    new_main_sha: Option<&str>,
    args: &TaskLandArgs,
    commits: &[ReviewCommit],
) -> Value {
    let (total, authors, checkpoints) = commit_counts(commits);
    json!({
        "schema_version": 1,
        "task_id": ctx.task_id,
        "mission_id": ctx.mission_id,
        "resolved_ref": resolved_ref,
        "base_ref": "refs/heads/main",
        "base_sha": base_sha,
        "head_sha": head_sha,
        "new_main_sha": new_main_sha,
        "mode": args.mode.as_str(),
        "dry_run": args.dry_run,
        "landed": !args.dry_run,
        "can_land": true,
        "commit_count": total,
        "author_commit_count": authors,
        "checkpoint_commit_count": checkpoints,
    })
}

enum LandTarget<'a> {
    LocalGit(&'a std::path::Path),
    D6Ledger {
        project: String,
        base_snapshot: spawn_cas::Digest,
        head_snapshot: spawn_cas::Digest,
    },
}

fn resolve_land_target<'a>(
    store: &spawn_postgres::PostgresStore,
    ctx: &'a TaskRepoContext,
    resolved: &ResolvedTaskRefs,
) -> Result<LandTarget<'a>, String> {
    if resolved.storage.is_d6()
        || resolved.base_snapshot.is_some()
        || resolved.head_snapshot.is_some()
        || ctx.repo.local_git_dir().is_none()
    {
        let project = resolved.d6_project.clone().ok_or_else(|| {
            format!(
                "cannot land task #{} on {}: no D6 ledger project found",
                ctx.task_id,
                ctx.repo.describe()
            )
        })?;
        let base_snapshot = match resolved.base_snapshot.clone() {
            Some(digest) => digest,
            None => lookup_snapshot_digest_by_oid(store, &resolved.base_sha)?.ok_or_else(|| {
                format!(
                    "cannot land task #{}: base commit {} has no D6 snapshot",
                    ctx.task_id, resolved.base_sha
                )
            })?,
        };
        let head_snapshot = match resolved.head_snapshot.clone() {
            Some(digest) => digest,
            None => lookup_snapshot_digest_by_oid(store, &resolved.head_sha)?.ok_or_else(|| {
                format!(
                    "cannot land task #{}: head commit {} has no D6 snapshot",
                    ctx.task_id, resolved.head_sha
                )
            })?,
        };
        return Ok(LandTarget::D6Ledger {
            project,
            base_snapshot,
            head_snapshot,
        });
    }
    let git_dir = ctx.repo.local_git_dir().ok_or_else(|| {
        format!(
            "cannot land task #{}: {} has no local repository",
            ctx.task_id,
            ctx.repo.describe()
        )
    })?;
    Ok(LandTarget::LocalGit(git_dir))
}

fn open_d6_workspace_for_land(
    store: &std::sync::Arc<spawn_postgres::PostgresStore>,
    ctx: &TaskRepoContext,
    project: &str,
    cas_dir_override: Option<&std::path::Path>,
) -> Result<
    (
        std::sync::Arc<spawn_host::CasMerkleStore>,
        spawn_host::d6_workspace::D6Workspace<crate::sources::PostgresRefLedger>,
    ),
    String,
> {
    let cas_dir = resolve_d6_cas_dir(ctx, cas_dir_override)?;
    if !cas_dir.is_dir() || !cas_dir.join(spawn_cas::CAS_MARKER).is_file() {
        return Err(format!(
            "D6 CAS store missing or uninitialized at {} while landing task #{}",
            cas_dir.display(),
            ctx.task_id
        ));
    }
    let cas = std::sync::Arc::new(
        spawn_host::CasMerkleStore::open(&cas_dir)
            .map_err(|error| format!("open D6 CAS store at {}: {error}", cas_dir.display()))?,
    );
    let ledger = std::sync::Arc::new(crate::sources::PostgresRefLedger::new(
        std::sync::Arc::clone(store),
    ));
    let ws_root = ctx
        .root
        .as_deref()
        .unwrap_or_else(|| cas_dir.parent().unwrap_or(cas_dir.as_path()));
    let d6 = spawn_host::d6_workspace::D6Workspace::new(
        project,
        ws_root,
        std::sync::Arc::clone(&cas),
        ledger,
    );
    Ok((cas, d6))
}

fn land_d6_ff_only(
    store: &std::sync::Arc<spawn_postgres::PostgresStore>,
    ctx: &TaskRepoContext,
    project: &str,
    base_snapshot: &spawn_cas::Digest,
    head_snapshot: &spawn_cas::Digest,
    head_sha: &str,
    cas_dir_override: Option<&std::path::Path>,
) -> Result<String, String> {
    let (_cas, d6) = open_d6_workspace_for_land(store, ctx, project, cas_dir_override)?;
    let reason = format!("task #{} land (ff-only)", ctx.task_id);
    d6.land_main_snapshot(
        Some(base_snapshot),
        head_snapshot,
        Some(ctx.task_id),
        &reason,
    )
    .map_err(|error| format!("cannot land task #{} in D6 ledger: {error}", ctx.task_id))?;
    Ok(head_sha.to_string())
}

fn land_d6_squash(
    store: &std::sync::Arc<spawn_postgres::PostgresStore>,
    ctx: &TaskRepoContext,
    project: &str,
    base_snapshot: &spawn_cas::Digest,
    tree_oid: &str,
    cas_dir_override: Option<&std::path::Path>,
) -> Result<String, String> {
    let (cas, d6) = open_d6_workspace_for_land(store, ctx, project, cas_dir_override)?;
    let cand_oid = ctx.candidate.as_deref().ok_or_else(|| {
        format!(
            "cannot land task #{} in D6 ledger (--squash): task has no accepted gate candidate snapshot matching base {base_snapshot} and squashed tree {tree_oid}",
            ctx.task_id
        )
    })?;
    let cand_digest = lookup_snapshot_digest_by_oid(store, cand_oid)?.ok_or_else(|| {
        format!(
            "cannot land task #{} in D6 ledger (--squash): candidate commit {cand_oid} has no D6 snapshot",
            ctx.task_id
        )
    })?;
    let cand_snap = cas
        .cas()
        .read_merkle_snapshot(&cand_digest)
        .map_err(|error| format!("read D6 candidate snapshot {cand_digest}: {error}"))?;
    let cand_tree_oid = cas
        .project_tree_oid(&cand_snap.tree)
        .map_err(|error| format!("project candidate tree {cand_digest}: {error}"))?;
    if cand_snap.parents.as_slice() != [base_snapshot.clone()] {
        return Err(format!(
            "cannot land task #{} in D6 ledger (--squash): candidate {cand_oid} parents {:?} do not match expected base snapshot {base_snapshot}",
            ctx.task_id, cand_snap.parents
        ));
    }
    if cand_tree_oid.to_hex() != tree_oid {
        return Err(format!(
            "cannot land task #{} in D6 ledger (--squash): candidate {cand_oid} tree {} does not match expected squashed tree {tree_oid}",
            ctx.task_id,
            cand_tree_oid.to_hex()
        ));
    }
    d6.land_main_snapshot(
        Some(base_snapshot),
        &cand_digest,
        Some(ctx.task_id),
        &format!("task #{} land (squash)", ctx.task_id),
    )
    .map_err(|error| format!("cannot land task #{} in D6 ledger: {error}", ctx.task_id))?;
    Ok(cand_oid.to_string())
}

fn execute_task_land(
    store: &std::sync::Arc<spawn_postgres::PostgresStore>,
    args: &TaskLandArgs,
    cas_dir_override: Option<&std::path::Path>,
) -> Result<(), String> {
    let ctx = load_task_repo(store, args.task_id, args.root.as_deref())?;
    if !ctx.is_landable() {
        return Err(format!(
            "cannot land task #{}: {} must be a local bare repository",
            ctx.task_id,
            ctx.repo.describe()
        ));
    }
    let resolved = resolve_task_refs(store, &ctx)?;
    let (_guard, review_dir) = materialize_review_repo(store, &ctx, &resolved, cas_dir_override)?;
    let base_sha = resolved.base_sha.clone();
    let resolved_ref = resolved.resolved_ref.clone();
    let head_sha = resolved.head_sha.clone();
    if head_sha == base_sha || is_ancestor(&review_dir, &head_sha, &base_sha)? {
        return Err(format!(
            "nothing to land for task #{}: {} ({}) is already in refs/heads/main ({})",
            ctx.task_id,
            resolved_ref,
            short_sha_prefix(&head_sha),
            short_sha_prefix(&base_sha)
        ));
    }
    let commits = list_commits_ahead(&review_dir, &base_sha, &head_sha)?;

    let new_main_sha: Option<String> = match args.mode {
        LandMode::FfOnly => {
            if !is_ancestor(&review_dir, &base_sha, &head_sha)? {
                return Err(format!(
                    "cannot fast-forward refs/heads/main ({}) to {} ({}): branch has diverged; use --squash",
                    short_sha_prefix(&base_sha),
                    resolved_ref,
                    short_sha_prefix(&head_sha)
                ));
            }
            if args.dry_run {
                Some(head_sha.clone())
            } else {
                match resolve_land_target(store, &ctx, &resolved)? {
                    LandTarget::LocalGit(git_dir) => {
                        run_git_raw(
                            git_dir,
                            &["update-ref", "refs/heads/main", &head_sha, &base_sha],
                            "update-ref refs/heads/main",
                        )?;
                        Some(head_sha.clone())
                    }
                    LandTarget::D6Ledger {
                        project,
                        base_snapshot,
                        head_snapshot,
                    } => Some(land_d6_ff_only(
                        store,
                        &ctx,
                        &project,
                        &base_snapshot,
                        &head_snapshot,
                        &head_sha,
                        cas_dir_override,
                    )?),
                }
            }
        }
        LandMode::Squash => {
            let (_squash_guard, work, tree_oid) =
                prepare_squash_worktree(&review_dir, &base_sha, &head_sha, ctx.task_id)?;
            if args.dry_run {
                None
            } else {
                match resolve_land_target(store, &ctx, &resolved)? {
                    LandTarget::LocalGit(git_dir) => Some(commit_squash_and_update_main(
                        &ctx, git_dir, &base_sha, &head_sha, &commits, &work, &tree_oid,
                    )?),
                    LandTarget::D6Ledger {
                        project,
                        base_snapshot,
                        ..
                    } => Some(land_d6_squash(
                        store,
                        &ctx,
                        &project,
                        &base_snapshot,
                        &tree_oid,
                        cas_dir_override,
                    )?),
                }
            }
        }
    };

    print_task_land_result(
        &ctx,
        &resolved_ref,
        &base_sha,
        &head_sha,
        new_main_sha.as_deref(),
        args,
        &commits,
    );
    Ok(())
}

fn print_task_land_result(
    ctx: &TaskRepoContext,
    resolved_ref: &str,
    base_sha: &str,
    head_sha: &str,
    new_main_sha: Option<&str>,
    args: &TaskLandArgs,
    commits: &[ReviewCommit],
) {
    if args.json {
        print(&task_land_payload(
            ctx,
            resolved_ref,
            base_sha,
            head_sha,
            new_main_sha,
            args,
            commits,
        ));
    } else if args.dry_run {
        println!(
            "task #{} dry-run OK (mode={} base={} head={} ref={resolved_ref})",
            ctx.task_id,
            args.mode.as_str(),
            short_sha_prefix(base_sha),
            short_sha_prefix(head_sha)
        );
    } else {
        println!(
            "task #{} landed onto refs/heads/main at {} (mode={} ref={resolved_ref})",
            ctx.task_id,
            short_sha_prefix(new_main_sha.unwrap_or(head_sha)),
            args.mode.as_str()
        );
    }
}

fn task_land_cmd(rest: &[String]) -> Result<i32, String> {
    let args = parse_task_land_args(rest)?;
    let store = std::sync::Arc::new(open_store()?);
    execute_task_land(&store, &args, None)?;
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

    /// `spawn mission show` names the backend from the root's marker, and
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
    fn configure_sets_clears_and_validates_objective_model_override() {
        assert_eq!(
            configure(&["--id", "4", "--model", "cheap"]),
            Ok(json!({"objectives": {"model": "cheap"}}))
        );
        assert_eq!(
            configure(&["--id", "4", "--model", "default"]),
            Ok(json!({"objectives": {"model": "default"}}))
        );
        assert_eq!(
            configure(&["--id", "4", "--no-model"]),
            Ok(json!({"objectives": {"model": null}}))
        );
        assert!(configure(&["--id", "4", "--model", "turbo"]).is_err());
        let conflict = configure(&["--id", "4", "--no-model", "--model", "cheap"]).unwrap_err();
        assert!(conflict.contains("--no-model and --model"), "{conflict}");
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
    #[allow(clippy::too_many_lines)]
    fn resolve_text_or_file_handles_inline_stdin_file_and_rejects_bad_inputs() {
        use std::io::Cursor;

        // 1. --brief <inline>
        let inline = super::resolve_text_or_file(
            &flags(&["--brief", "Fix typo"]),
            "brief",
            "brief-file",
            &mut Cursor::new(b""),
        )
        .unwrap();
        assert_eq!(inline, "Fix typo");

        // 2. --brief - from stdin with multiline & special characters verbatim
        let special =
            "Line 1: $HOME `whoami` $(id) \"quotes\" 'single'\nLine 2: 重大决策 & chmod\n";
        let from_stdin = super::resolve_text_or_file(
            &flags(&["--brief", "-"]),
            "brief",
            "brief-file",
            &mut Cursor::new(special.as_bytes()),
        )
        .unwrap();
        assert_eq!(from_stdin, special);

        // 3. --brief-file <path> with >8KB long UTF-8 text verbatim
        let tmp = std::env::temp_dir().join(format!("spawn-brief-test-{}.md", std::process::id()));
        let long_brief = format!(
            "# Brief\n{}{}",
            "架构设计与安全校验 --flag=\"val\" $VAR\n",
            "x".repeat(9000)
        );
        std::fs::write(&tmp, &long_brief).unwrap();
        let tmp_str = tmp.to_string_lossy().to_string();
        let from_file = super::resolve_text_or_file(
            &flags(&["--brief-file", &tmp_str]),
            "brief",
            "brief-file",
            &mut Cursor::new(b""),
        )
        .unwrap();
        assert_eq!(from_file, long_brief);

        // 4. Reject mutually exclusive flags
        let conflict = super::resolve_text_or_file(
            &flags(&["--brief", "a", "--brief-file", &tmp_str]),
            "brief",
            "brief-file",
            &mut Cursor::new(b""),
        )
        .unwrap_err();
        assert!(conflict.contains("not both"), "{conflict}");

        // 5. Reject missing brief / missing file / whitespace-only file / empty stdin
        assert!(
            super::resolve_text_or_file(&flags(&[]), "brief", "brief-file", &mut Cursor::new(b""))
                .unwrap_err()
                .contains("is required")
        );
        assert!(
            super::resolve_text_or_file(
                &flags(&["--brief-file", "/nonexistent/spawn-brief-missing.md"]),
                "brief",
                "brief-file",
                &mut Cursor::new(b""),
            )
            .is_err()
        );
        std::fs::write(&tmp, "   \n\t  ").unwrap();
        assert!(
            super::resolve_text_or_file(
                &flags(&["--brief-file", &tmp_str]),
                "brief",
                "brief-file",
                &mut Cursor::new(b""),
            )
            .unwrap_err()
            .contains("is empty")
        );
        let _ = std::fs::remove_file(&tmp);

        assert!(
            super::resolve_text_or_file(
                &flags(&["--brief", "-"]),
                "brief",
                "brief-file",
                &mut Cursor::new(b"   \n"),
            )
            .unwrap_err()
            .contains("empty input")
        );

        // 6. Optional --reason / --reason-file for reassign
        assert_eq!(
            super::resolve_optional_text_or_file(
                &flags(&[]),
                "reason",
                "reason-file",
                &mut Cursor::new(b""),
            )
            .unwrap(),
            None
        );
        let reason_stdin = super::resolve_optional_text_or_file(
            &flags(&["--reason", "-"]),
            "reason",
            "reason-file",
            &mut Cursor::new(b"worker stalled on OOM"),
        )
        .unwrap();
        assert_eq!(reason_stdin.as_deref(), Some("worker stalled on OOM"));
        assert!(
            super::resolve_optional_text_or_file(
                &flags(&["--reason", "x", "--reason-file", "/tmp/r.txt"]),
                "reason",
                "reason-file",
                &mut Cursor::new(b""),
            )
            .is_err()
        );
    }

    #[test]
    fn mission_list_flags_and_table_formatting_show_grading_and_rework_step_up() {
        let table_args = [
            "list".to_string(),
            "--table".to_string(),
            "--limit".to_string(),
            "5".to_string(),
        ];
        let (verb, parsed_table) = split_mission(&table_args).unwrap();
        assert_eq!(verb, "list");
        assert!(parsed_table.switch("table"));
        assert!(!parsed_table.switch("json"));
        assert_eq!(parsed_table.get("limit").as_deref(), Some("5"));

        let json_args = ["list".to_string(), "--json".to_string()];
        let (_, parsed_json) = split_mission(&json_args).unwrap();
        assert!(parsed_json.switch("json"));

        assert_eq!(
            super::format_mission_board_table(&json!({ "missions": [] })),
            "(no missions)"
        );

        let board = json!({
            "missions": [{
                "id": 42,
                "state": "open",
                "text": "Top-down rollout\nSecond line",
                "tasks": [
                    {
                        "id": 101,
                        "state": "revise",
                        "model": "default",
                        "agent_id": "agent_w1",
                        "difficulty": "l1",
                        "routed_difficulty": "l2",
                        "tier_role": "mainline",
                        "preset_id": spawn_core::grading::TOPDOWN_L2_PRESET_ID,
                        "first_preset_id": spawn_core::grading::TOPDOWN_L1_PRESET_ID,
                        "quality_rejections": 1,
                        "engine_label": "agy",
                        "model_label": "gemini-pro-orchestrator-neon",
                        "execution_profile": "neon-mainline",
                        "rework_step_up": {
                            "quality_rejections": 1,
                            "from_difficulty": "l1",
                            "routed_difficulty": "l2",
                            "stepped_up": true,
                            "first_preset_id": spawn_core::grading::TOPDOWN_L1_PRESET_ID,
                            "preset_id": spawn_core::grading::TOPDOWN_L2_PRESET_ID
                        }
                    },
                    {
                        "id": 102,
                        "state": "assigned",
                        "model": "default",
                        "agent_id": "agent_w2",
                        "difficulty": "l2",
                        "routed_difficulty": "l2",
                        "tier_role": "mainline",
                        "preset_id": Value::Null,
                        "first_preset_id": Value::Null,
                        "quality_rejections": 0,
                        "engine_label": Value::Null,
                        "model_label": Value::Null,
                        "execution_profile": Value::Null,
                        "rework_step_up": Value::Null
                    }
                ]
            }]
        });

        let table = super::format_mission_board_table(&board);
        assert!(
            table.contains("Mission #42 [open] Top-down rollout"),
            "{table}"
        );
        assert!(table.contains("T101"), "{table}");
        assert!(table.contains("mainline"), "{table}");
        assert!(table.contains("topdown-l2-neon"), "{table}");
        assert!(
            table.contains("step-up l1->l2, 1 rej, topdown-l1-mechanical->topdown-l2-neon"),
            "{table}"
        );
        assert!(table.contains("T102"), "{table}");
        assert!(
            !table.contains(
                "T102   assigned   default l2   mainline           -                        step-up"
            ),
            "{table}"
        );
    }

    #[test]
    fn episodes_flags_parse_switches_and_reject_bad_mission_id() {
        let args: Vec<String> = [
            "episodes",
            "--mission",
            "4",
            "--strict",
            "--verify-parity",
            "--json",
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        let (verb, parsed) = super::split_mission(&args).expect("episodes flags");
        assert_eq!(verb, "episodes");
        assert_eq!(super::parse_optional_mission_id(&parsed), Ok(Some(4)));
        assert!(parsed.switch("strict"));
        assert!(parsed.switch("verify-parity"));
        assert!(parsed.switch("json"));

        let default_args = [String::from("episodes")];
        let (_, plain) = super::split_mission(&default_args).expect("plain episodes");
        assert_eq!(super::parse_optional_mission_id(&plain), Ok(None));
        assert!(!plain.switch("strict"));
        assert!(!plain.switch("verify-parity"));
        assert!(!plain.switch("json"));

        let bad_args: Vec<String> = ["episodes", "--mission", "abc"]
            .iter()
            .map(ToString::to_string)
            .collect();
        let (_, bad) = super::split_mission(&bad_args).expect("flags");
        assert!(
            super::parse_optional_mission_id(&bad)
                .unwrap_err()
                .contains("--mission must be a number")
        );
    }

    #[test]
    fn episodes_usage_preserves_mission_contract_help() {
        assert!(super::USAGE.contains(
            "spawn mission episodes [--mission N] [--strict] [--verify-parity] [--json]"
        ));
        for line in super::USAGE.lines() {
            if line.trim_start().starts_with("spawn mission ") {
                let verb = line.split_whitespace().nth(2).unwrap_or_default();
                assert_ne!(
                    verb, "episodes",
                    "episodes must stay a note line so main.rs contract verb list remains unchanged"
                );
            }
        }
    }

    fn sample_parity_episodes() -> Vec<spawn_core::control::TaskEpisode> {
        use spawn_core::control::{ControlRecord, replay_task_episodes};

        let events = [
            (
                1,
                1000,
                ControlRecord::from_value_strict(&json!({
                    "type": "mission.added",
                    "id": 10,
                    "text": "ship",
                    "origin": "human",
                    "sources": []
                }))
                .unwrap(),
            ),
            (
                2,
                1001,
                ControlRecord::from_value_strict(&json!({
                    "type": "task.assigned",
                    "id": 50,
                    "mission_id": 10,
                    "kind": "ephemeral",
                    "agent_id": "agent_w_50",
                    "session_id": "session_w_50",
                    "brief": "fix parser",
                    "model": "cheap",
                    "difficulty": "l1",
                    "preset_id": "cheap-l1"
                }))
                .unwrap(),
            ),
            (
                3,
                1002,
                ControlRecord::from_value_strict(&json!({
                    "type": "task.delivered",
                    "id": 50,
                    "command_id": 7
                }))
                .unwrap(),
            ),
            (
                4,
                1003,
                ControlRecord::from_value_strict(&json!({
                    "type": "task.reported",
                    "id": 50,
                    "report": "draft"
                }))
                .unwrap(),
            ),
            (
                5,
                1004,
                ControlRecord::from_value_strict(&json!({
                    "type": "task.reviewed",
                    "id": 50,
                    "accepted": false,
                    "note": "edge case",
                    "revision": 0
                }))
                .unwrap(),
            ),
            (
                6,
                1005,
                ControlRecord::from_value_strict(&json!({
                    "type": "task.rerouted",
                    "id": 50,
                    "preset_id": "neon-l2",
                    "model": "default",
                    "from_preset_id": "cheap-l1",
                    "revision": 1
                }))
                .unwrap(),
            ),
        ];
        replay_task_episodes(&events)
    }

    #[test]
    fn episodes_parity_matches_and_detects_every_field_drift() {
        let episodes = sample_parity_episodes();
        let matching_row = json!({
            "id": 50,
            "mission_id": 10,
            "state": "revise",
            "revisions": 1,
            "quality_rejections": 1,
            "difficulty": 1,
            "preset_id": "neon-l2",
            "first_preset_id": "cheap-l1",
            "model": "default",
            "agent_id": "agent_w_50",
            "session_id": "session_w_50",
        });

        let (ok, drifts) = spawn_postgres::PostgresStore::compare_episode_parity(
            &episodes,
            std::slice::from_ref(&matching_row),
        );
        assert!(ok, "expected parity match, got {drifts:?}");
        assert!(drifts.is_empty());

        let summary = json!({
            "total": 1,
            "landed": 0,
            "accepted": 0,
            "rejected": 0,
            "cancelled": 0,
            "aborted": 0,
            "over_budget": 0,
            "in_flight": 1,
            "total_rework": 1,
            "total_reroutes": 1,
            "review_sync_points": 1,
            "gate_verdicts": 0,
            "warnings": 0,
        });
        let ok_payload = super::build_episodes_cli_payload(
            Some(10),
            true,
            &episodes,
            &summary,
            Some((ok, &drifts)),
        );
        assert_eq!(ok_payload["parity_ok"], json!(true));
        assert_eq!(ok_payload["drift_count"], json!(0));
        let ok_text = super::format_episodes_text(&episodes, &summary, Some((ok, &drifts)));
        assert!(ok_text.contains("parity: OK (1 task(s) matched mission_tasks)"));

        for (sql_key, drift_field, bad_val) in [
            ("state", "state", json!("delivered")),
            ("revisions", "rework", json!(9)),
            ("quality_rejections", "quality_rejections", json!(0)),
            ("difficulty", "difficulty", json!(3)),
            ("preset_id", "preset_id", json!("other-preset")),
            ("first_preset_id", "first_preset_id", json!(null)),
            ("model", "model", json!("cheap")),
            ("agent_id", "agent_id", json!("agent_other")),
            ("session_id", "session_id", json!("session_other")),
        ] {
            let mut mutated = matching_row.clone();
            mutated[sql_key] = bad_val;
            let (is_ok, field_drifts) =
                spawn_postgres::PostgresStore::compare_episode_parity(&episodes, &[mutated]);
            assert!(!is_ok, "expected drift on {sql_key}");
            assert_eq!(field_drifts.len(), 1);
            assert_eq!(field_drifts[0]["field"], json!(drift_field));
            let drift_text =
                super::format_episodes_text(&episodes, &summary, Some((is_ok, &field_drifts)));
            assert!(drift_text.contains("parity: DRIFT (1 mismatch(es))"));
            assert!(drift_text.contains(&format!("field={drift_field}")));
        }

        let (missing_sql_ok, missing_sql_drifts) =
            spawn_postgres::PostgresStore::compare_episode_parity(&episodes, &[]);
        assert!(!missing_sql_ok);
        assert_eq!(missing_sql_drifts[0]["field"], json!("sql_row"));

        let extra_row = json!({
            "id": 999,
            "mission_id": 10,
            "state": "assigned",
            "revisions": 0,
            "quality_rejections": 0,
            "difficulty": 2,
            "preset_id": null,
            "first_preset_id": null,
            "model": "default",
            "agent_id": "agent_w_999",
            "session_id": "session_w_999",
        });
        let (extra_ok, extra_drifts) = spawn_postgres::PostgresStore::compare_episode_parity(
            &episodes,
            &[matching_row, extra_row],
        );
        assert!(!extra_ok);
        assert_eq!(extra_drifts[0]["field"], json!("episode"));
        assert_eq!(extra_drifts[0]["task_id"], json!(999));
    }
}

#[cfg(test)]
mod database_tests {
    use std::io::Cursor;

    use serde_json::json;
    use spawn_postgres::testing::TestSchema;

    use crate::Flags;

    fn flags(words: &[&str]) -> Flags {
        Flags::parse_with_switches(
            &words.iter().map(ToString::to_string).collect::<Vec<_>>(),
            &["again", "pin"],
        )
        .unwrap()
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn assign_with_brief_file_and_stdin_routes_topdown_without_downgrading_l3() {
        let schema = TestSchema::new("cli_missions_topdown_brief_file");
        let store = schema.store();
        let mission = store.mission_add("TopDown", &json!([]), "human").unwrap();

        // Seed default 2-tier stack (L3 Fable 5.2 locked, L2 Neon locked; no L1).
        crate::grading::init_topdown(&store, false, None).unwrap();

        // 1. L3 brief via --brief-file -> routes to claude / claude-fable-5-2.
        let brief_path =
            std::env::temp_dir().join(format!("spawn-topdown-l3-{}.md", std::process::id()));
        let l3_brief = "重大决策：架构重构与跨模块安全沙箱隔离\n保留 $HOME 和 `backticks` 原样";
        std::fs::write(&brief_path, l3_brief).unwrap();
        let path_str = brief_path.to_string_lossy().to_string();
        let mission_id = mission.to_string();
        let l3 = super::assign_with_reader(
            &store,
            &Flags::parse_with_switches(
                &[
                    "--mission".into(),
                    mission_id.clone(),
                    "--kind".into(),
                    "ephemeral".into(),
                    "--worker".into(),
                    "w-l3".into(),
                    "--brief-file".into(),
                    path_str,
                ],
                &["again", "pin"],
            )
            .unwrap(),
            &mut Cursor::new(b""),
        )
        .unwrap();
        let _ = std::fs::remove_file(&brief_path);
        assert_eq!(l3["difficulty"], json!("l3"));
        assert_eq!(
            l3["preset_id"],
            json!(spawn_core::grading::TOPDOWN_L3_PRESET_ID)
        );
        assert_eq!(l3["engine_label"], json!("claude"));
        assert_eq!(l3["model_label"], json!("claude-fable-5-2"));
        let stored_mission = store.mission_by_id(mission).unwrap().unwrap();
        let stored_task = stored_mission["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == l3["task"])
            .unwrap();
        assert_eq!(stored_task["brief"], json!(l3_brief));

        // 2. Neutral L2 brief via --brief - -> routes to agy / gemini-pro-orchestrator-neon.
        let l2 = super::assign_with_reader(
            &store,
            &Flags::parse_with_switches(
                &[
                    "--mission".into(),
                    mission_id.clone(),
                    "--kind".into(),
                    "ephemeral".into(),
                    "--worker".into(),
                    "w-l2".into(),
                    "--brief".into(),
                    "-".into(),
                ],
                &["again", "pin"],
            )
            .unwrap(),
            &mut Cursor::new(b"Adjust button padding in dashboard panel"),
        )
        .unwrap();
        assert_eq!(l2["difficulty"], json!("l2"));
        assert_eq!(
            l2["preset_id"],
            json!(spawn_core::grading::TOPDOWN_L2_PRESET_ID)
        );
        assert_eq!(l2["engine_label"], json!("agy"));
        assert_eq!(l2["model_label"], json!("gemini-pro-orchestrator-neon"));

        // 3. Deterministic L1 brief with no L1 preset -> upward fallback to L2 Neon.
        let l1_up = super::assign_with_reader(
            &store,
            &flags(&[
                "--mission",
                &mission_id,
                "--kind",
                "ephemeral",
                "--worker",
                "w-l1-up",
                "--brief",
                "Fix the typo in README.md",
            ]),
            &mut Cursor::new(b""),
        )
        .unwrap();
        assert_eq!(l1_up["difficulty"], json!("l1"));
        assert_eq!(
            l1_up["preset_id"],
            json!(spawn_core::grading::TOPDOWN_L2_PRESET_ID)
        );
        assert_eq!(l1_up["preset_grade"], json!("l2"));

        // 4. Enable L1 preset (`deepseek-v4.1-flash`) -> deterministic L1 now offloads to T1.
        let second_schema = TestSchema::new("cli_missions_topdown_with_l1");
        let second_store = second_schema.store();
        let second_mission = second_store
            .mission_add("WithL1", &json!([]), "human")
            .unwrap();
        let second_mission_id = second_mission.to_string();
        crate::grading::init_topdown(&second_store, true, Some("deepseek-v4.1-flash")).unwrap();
        let l1_offload = super::assign_with_reader(
            &second_store,
            &flags(&[
                "--mission",
                &second_mission_id,
                "--kind",
                "ephemeral",
                "--worker",
                "w-l1",
                "--brief",
                "Fix the typo in README.md",
            ]),
            &mut Cursor::new(b""),
        )
        .unwrap();
        assert_eq!(l1_offload["difficulty"], json!("l1"));
        assert_eq!(
            l1_offload["preset_id"],
            json!(spawn_core::grading::TOPDOWN_L1_PRESET_ID)
        );
        assert_eq!(l1_offload["model"], json!("cheap"));
        assert_eq!(l1_offload["model_label"], json!("deepseek-v4.1-flash"));

        // 5. Disable L3 preset -> L3 brief MUST NOT silently downgrade to L2 Neon!
        second_store
            .grading_record(&json!({
                "type": "preset.state_changed",
                "preset_id": spawn_core::grading::TOPDOWN_L3_PRESET_ID,
                "from_state": "active",
                "to_state": "disabled"
            }))
            .unwrap();
        let l3_no_downgrade = super::assign_with_reader(
            &second_store,
            &flags(&[
                "--mission",
                &second_mission_id,
                "--kind",
                "ephemeral",
                "--worker",
                "w-l3-fb",
                "--brief",
                "Harden authentication against SQL injection",
            ]),
            &mut Cursor::new(b""),
        )
        .unwrap();
        assert!(
            l3_no_downgrade.get("preset_id").is_none(),
            "{l3_no_downgrade}"
        );
        assert_eq!(l3_no_downgrade["model"], json!("default"));
        assert!(
            l3_no_downgrade["routing_fallback"]
                .as_str()
                .unwrap()
                .contains("refusing to downgrade l3 to l2"),
            "{l3_no_downgrade}"
        );
    }

    #[test]
    fn mission_list_enriches_tasks_with_grading_and_rework_step_up_history() {
        let schema = TestSchema::new("cli_missions_list_grading");
        let store = schema.store();
        let mission = store
            .mission_add("Grading Board Visibility", &json!([]), "human")
            .unwrap();
        let mission_id = mission.to_string();

        crate::grading::init_topdown(&store, true, Some("deepseek-v4.1-flash")).unwrap();

        let t1 = super::assign(
            &store,
            &flags(&[
                "--mission",
                &mission_id,
                "--kind",
                "ephemeral",
                "--worker",
                "w-l1",
                "--brief",
                "Fix the typo in README.md",
            ]),
        )
        .unwrap()["task"]
            .as_i64()
            .unwrap();
        let _t2 = super::assign(
            &store,
            &flags(&[
                "--mission",
                &mission_id,
                "--kind",
                "ephemeral",
                "--worker",
                "w-l2",
                "--brief",
                "Adjust button spacing in dashboard panel",
            ]),
        )
        .unwrap();
        let _t3 = super::assign(
            &store,
            &flags(&[
                "--mission",
                &mission_id,
                "--kind",
                "ephemeral",
                "--worker",
                "w-l3",
                "--brief",
                "Harden authentication against SQL injection",
            ]),
        )
        .unwrap();

        // Deliver & report t1, then send back once for quality so L1 steps up to L2 Neon.
        store.task_delivered(t1, 1).unwrap();
        assert!(store.task_reported(t1, "done").unwrap());
        let t1_str = t1.to_string();
        super::reject(
            &store,
            &flags(&["--id", &t1_str, "--note", "fix edge case"]),
        )
        .unwrap();

        let board = super::list_board(&store, 20).unwrap();
        let tasks = board["missions"][0]["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 3);

        // Task 1: L1 stepped up to L2 Neon with rework_step_up metadata.
        assert_eq!(tasks[0]["difficulty"], json!("l1"));
        assert_eq!(tasks[0]["routed_difficulty"], json!("l2"));
        assert_eq!(tasks[0]["quality_rejections"], json!(1));
        assert_eq!(
            tasks[0]["first_preset_id"],
            json!(spawn_core::grading::TOPDOWN_L1_PRESET_ID)
        );
        assert_eq!(
            tasks[0]["preset_id"],
            json!(spawn_core::grading::TOPDOWN_L2_PRESET_ID)
        );
        assert_eq!(tasks[0]["tier_role"], json!("mainline"));
        assert_eq!(tasks[0]["rework_step_up"]["stepped_up"], json!(true));
        assert_eq!(tasks[0]["rework_step_up"]["from_difficulty"], json!("l1"));
        assert_eq!(tasks[0]["rework_step_up"]["routed_difficulty"], json!("l2"));

        // Task 2: L2 Neon mainline.
        assert_eq!(tasks[1]["difficulty"], json!("l2"));
        assert_eq!(tasks[1]["tier_role"], json!("mainline"));
        assert_eq!(tasks[1]["engine_label"], json!("agy"));
        assert_eq!(
            tasks[1]["model_label"],
            json!("gemini-pro-orchestrator-neon")
        );
        assert_eq!(tasks[1]["execution_profile"], json!("neon-mainline"));
        assert!(tasks[1]["rework_step_up"].is_null(), "{}", tasks[1]);

        // Task 3: L3 Fable top-decision.
        assert_eq!(tasks[2]["difficulty"], json!("l3"));
        assert_eq!(tasks[2]["tier_role"], json!("top-decision"));
        assert_eq!(tasks[2]["model_label"], json!("claude-fable-5-2"));

        // `mission_by_id` (`spawn mission show`) projects the exact same grading fields.
        let by_id = store.mission_by_id(mission).unwrap().unwrap();
        assert_eq!(by_id["tasks"], board["missions"][0]["tasks"]);

        let table = super::format_mission_board_table(&board);
        assert!(table.contains("Mission #"), "{table}");
        assert!(table.contains("topdown-l2-neon"), "{table}");
        assert!(table.contains("topdown-l3-fable"), "{table}");
        assert!(table.contains("top-decision"), "{table}");
        assert!(table.contains("step-up l1->l2, 1 rej"), "{table}");
    }

    #[test]
    fn parse_task_commits_diff_and_land_args_validate_correctly() {
        let commits = super::parse_task_commits_args(&["42".to_string(), "--json".to_string()])
            .expect("commits positional");
        assert_eq!(commits.task_id, 42);
        assert!(commits.json);

        let diff = super::parse_task_diff_args(&[
            "--id".to_string(),
            "7".to_string(),
            "--stat".to_string(),
        ])
        .expect("diff --id");
        assert_eq!(diff.task_id, 7);
        assert!(diff.stat);

        let land_default = super::parse_task_land_args(&["9".to_string()]).expect("default land");
        assert_eq!(land_default.mode, super::LandMode::FfOnly);
        assert!(!land_default.dry_run);
        assert!(!land_default.json);

        let land_squash = super::parse_task_land_args(&[
            "9".to_string(),
            "--squash".to_string(),
            "--dry-run".to_string(),
            "--json".to_string(),
        ])
        .expect("squash dry run");
        assert_eq!(land_squash.mode, super::LandMode::Squash);
        assert!(land_squash.dry_run);
        assert!(land_squash.json);

        let conflict = super::parse_task_land_args(&[
            "9".to_string(),
            "--ff-only".to_string(),
            "--squash".to_string(),
        ])
        .expect_err("ff-only + squash must fail");
        assert!(conflict.contains("not both"), "{conflict}");

        assert!(super::parse_task_commits_args(&[]).is_err());
        assert!(super::parse_task_commits_args(&["0".to_string()]).is_err());
        assert!(super::parse_task_diff_args(&["5".to_string(), "--bogus".to_string()]).is_err());
    }

    #[test]
    fn checkpoint_detection_and_clean_squash_message_strip_synthetic_checkpoints() {
        assert!(super::is_checkpoint_subject("spawn checkpoint"));
        assert!(super::is_checkpoint_subject("spawn checkpoint exec_123"));
        assert!(super::is_checkpoint_subject("  Spawn Checkpoint exec_9  "));
        assert!(!super::is_checkpoint_subject("Add operator review CLI"));

        let commits = vec![
            super::ReviewCommit {
                sha: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
                short_sha: "bbbbbbb".to_string(),
                author_name: "spawn".to_string(),
                author_email: "spawn@localhost".to_string(),
                authored_at: "2026-11-26T12:01:00Z".to_string(),
                subject: "spawn checkpoint exec_1".to_string(),
                is_checkpoint: true,
            },
            super::ReviewCommit {
                sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                short_sha: "aaaaaaa".to_string(),
                author_name: "Alice".to_string(),
                author_email: "alice@example.com".to_string(),
                authored_at: "2026-11-26T12:00:00Z".to_string(),
                subject: "Implement widget".to_string(),
                is_checkpoint: false,
            },
        ];
        let msg = super::build_clean_squash_message(
            15,
            "Build widget\nDetails",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            &commits,
        );
        assert!(msg.starts_with("Implement widget\n\n"), "{msg}");
        assert!(!msg.contains("spawn checkpoint"), "{msg}");
        assert!(msg.contains("Spawn-Task: 15"), "{msg}");
        assert!(
            msg.contains("Spawn-Head: bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            "{msg}"
        );
        assert_eq!(
            super::squash_author_identity(&commits),
            ("Alice", "alice@example.com")
        );

        let table = super::format_task_commits_table(
            15,
            "refs/spawn/tasks/15/head",
            "1111111111111111111111111111111111111111",
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            &commits,
        );
        assert!(table.contains("1 author, 1 checkpoint"), "{table}");
        assert!(
            table.contains("checkpoint") && table.contains("author"),
            "{table}"
        );
    }

    #[test]
    fn resolve_preserved_ref_prefers_task_head_then_worker_head_then_spawn_branch() {
        let dir = super::TempDirGuard::create("spawn-task-ref-order").expect("temp dir");
        let bare = dir.path.join("mission.git");
        let work = dir.path.join("work");
        let run_test_git = |cwd: Option<&std::path::Path>, args: &[&str]| -> String {
            let mut cmd = spawn_host::test_git_command();
            if let Some(path) = cwd {
                cmd.arg("-C").arg(path);
            }
            let out = cmd.args(args).output().expect("run test git");
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        run_test_git(None, &["init", "--bare", bare.to_str().expect("utf8")]);
        run_test_git(
            None,
            &[
                "clone",
                bare.to_str().expect("utf8"),
                work.to_str().expect("utf8"),
            ],
        );
        run_test_git(Some(&work), &["checkout", "-B", "main"]);
        std::fs::write(work.join("README.md"), "base\n").expect("write");
        run_test_git(Some(&work), &["add", "README.md"]);
        run_test_git(Some(&work), &["commit", "-m", "base"]);
        run_test_git(Some(&work), &["push", "origin", "main:refs/heads/main"]);

        std::fs::write(work.join("a.txt"), "branch\n").expect("write a");
        run_test_git(Some(&work), &["add", "a.txt"]);
        run_test_git(Some(&work), &["commit", "-m", "branch commit"]);
        let sha_branch = run_test_git(Some(&work), &["rev-parse", "HEAD"]);
        run_test_git(
            Some(&work),
            &[
                "push",
                "origin",
                &format!("{sha_branch}:refs/heads/spawn/agent_w1"),
            ],
        );
        let (r3, s3) = super::resolve_preserved_ref(&bare, 7, "agent_w1").expect("branch fallback");
        assert_eq!(r3, "refs/heads/spawn/agent_w1");
        assert_eq!(s3, sha_branch);

        std::fs::write(work.join("b.txt"), "worker\n").expect("write b");
        run_test_git(Some(&work), &["add", "b.txt"]);
        run_test_git(Some(&work), &["commit", "-m", "worker head"]);
        let sha_worker = run_test_git(Some(&work), &["rev-parse", "HEAD"]);
        run_test_git(
            Some(&work),
            &[
                "push",
                "origin",
                &format!("{sha_worker}:refs/spawn/workers/agent_w1/head"),
            ],
        );
        let (r2, s2) = super::resolve_preserved_ref(&bare, 7, "agent_w1").expect("worker fallback");
        assert_eq!(r2, "refs/spawn/workers/agent_w1/head");
        assert_eq!(s2, sha_worker);

        std::fs::write(work.join("c.txt"), "task\n").expect("write c");
        run_test_git(Some(&work), &["add", "c.txt"]);
        run_test_git(Some(&work), &["commit", "-m", "task head"]);
        let sha_task = run_test_git(Some(&work), &["rev-parse", "HEAD"]);
        run_test_git(
            Some(&work),
            &[
                "push",
                "origin",
                &format!("{sha_task}:refs/spawn/tasks/7/head"),
            ],
        );
        let (r1, s1) = super::resolve_preserved_ref(&bare, 7, "agent_w1").expect("task primary");
        assert_eq!(r1, "refs/spawn/tasks/7/head");
        assert_eq!(s1, sha_task);

        let missing =
            super::resolve_preserved_ref(&bare, 99, "agent_none").expect_err("missing ref");
        assert!(missing.contains("refs/spawn/tasks/99/head"), "{missing}");
    }

    #[test]
    fn missions_rs_has_zero_spawn_host_git_references() {
        let src = include_str!("missions.rs");
        let forbidden = format!("spawn_host{}git", "::");
        assert!(
            !src.contains(&forbidden),
            "missions.rs must not reference {forbidden}"
        );
    }

    struct D6ReviewFixture {
        harness: TestD6Harness,
        project: String,
        task_id: i64,
        task_ref: String,
        base_pub: spawn_host::PublishedSnapshot,
        task_pub: spawn_host::PublishedSnapshot,
        ctx: super::TaskRepoContext,
        resolved: super::ResolvedTaskRefs,
    }

    impl D6ReviewFixture {
        fn new(schema_name: &str, dir_prefix: &str) -> Self {
            let harness = TestD6Harness::new(schema_name, dir_prefix, true);
            // Intentional non-existent source repo path on disk (pure D6 mode).
            let project = harness
                .guard
                .path
                .join("pure-d6-source.git")
                .to_string_lossy()
                .into_owned();
            let base_pub = harness.publish_main(
                &project,
                &[(b"hello.txt", b"base line\n")],
                1_700_000_000_000,
                "initial base commit",
            );
            let mission_id = harness
                .store
                .mission_add(
                    "D6 task inspection mission",
                    &serde_json::json!([project]),
                    "human",
                )
                .expect("mission add");
            let task_id = harness
                .store
                .task_assign(
                    mission_id,
                    "ephemeral",
                    "agent_d6",
                    "d6 change brief",
                    "default",
                )
                .expect("task assign");
            let task_ref = spawn_host::workspace_rules::task_head_ref(task_id, "head");
            let task_pub = harness.publish_branch(
                &project,
                &task_ref,
                &[(b"hello.txt", b"updated line\nsecond line\n")],
                std::slice::from_ref(&base_pub.snapshot),
                1_700_000_100_000,
                "implement d6 change",
            );
            let ctx = super::load_task_repo(&harness.store, task_id, Some(&harness.root_str))
                .expect("load d6 ctx");
            let resolved = super::resolve_task_refs(&harness.store, &ctx).expect("resolve d6 refs");
            Self {
                harness,
                project,
                task_id,
                task_ref,
                base_pub,
                task_pub,
                ctx,
                resolved,
            }
        }

        fn assert_commits_and_diff(&self) {
            assert!(matches!(self.ctx.repo, super::TaskRepo::D6Ledger { .. }));
            assert!(self.ctx.is_landable());
            let described = self.ctx.repo.describe();
            assert!(described.starts_with("D6 ledger project "), "{described}");
            assert_eq!(self.resolved.storage, crate::sources::ProjectStorage::D6);
            assert_eq!(self.resolved.resolved_ref, self.task_ref);
            assert_eq!(self.resolved.base_sha, self.base_pub.git_oid.to_hex());
            assert_eq!(self.resolved.head_sha, self.task_pub.git_oid.to_hex());

            let (_review_guard, review_dir) = super::materialize_review_repo(
                &self.harness.store,
                &self.ctx,
                &self.resolved,
                Some(&self.harness.cas_dir),
            )
            .expect("materialize d6 review repo");
            let commits = super::list_commits_ahead(
                &review_dir,
                &self.resolved.base_sha,
                &self.resolved.head_sha,
            )
            .expect("list d6 commits ahead");
            assert_eq!(commits.len(), 1);
            assert_eq!(commits[0].sha, self.task_pub.git_oid.to_hex());
            assert_eq!(commits[0].subject, "implement d6 change");

            let range = format!("{}...{}", self.resolved.base_sha, self.resolved.head_sha);
            let diff = super::run_git_raw(
                &review_dir,
                &["diff", "--no-ext-diff", &range, "--"],
                "diff",
            )
            .expect("d6 diff");
            assert!(diff.contains("+updated line"), "{diff}");
        }

        fn execute_land(&self, mode: super::LandMode, dry_run: bool) -> Result<(), String> {
            super::execute_task_land(
                &self.harness.store,
                &super::TaskLandArgs {
                    task_id: self.task_id,
                    mode,
                    dry_run,
                    json: false,
                    root: Some(self.harness.root_str.clone()),
                },
                Some(&self.harness.cas_dir),
            )
        }

        fn cas_file_count(&self) -> usize {
            let mut count = 0usize;
            let mut stack = vec![self.harness.cas_dir.clone()];
            while let Some(dir) = stack.pop() {
                for entry in std::fs::read_dir(&dir).expect("read cas dir") {
                    let path = entry.expect("cas entry").path();
                    if path.is_dir() {
                        stack.push(path);
                    } else {
                        count += 1;
                    }
                }
            }
            count
        }
    }

    #[test]
    fn d6_task_commits_diff_and_land_dry_run_and_refusal_work_against_pg_and_cas() {
        let f = D6ReviewFixture::new("missions_d6_review_refusal", "missions-d6-review-refusal");
        f.assert_commits_and_diff();

        let cas_files_before = f.cas_file_count();
        let refs_before = f
            .harness
            .store
            .list_refs(Some(&f.project))
            .expect("list refs before");
        let logs_before = f
            .harness
            .store
            .list_ref_log_after(0)
            .expect("list ref log before");

        f.execute_land(super::LandMode::FfOnly, true)
            .expect("d6 ff-only dry-run");
        f.execute_land(super::LandMode::Squash, true)
            .expect("d6 squash dry-run");

        for (mode, expected) in [
            (super::LandMode::FfOnly, "accepted gate verdict"),
            (super::LandMode::Squash, "no accepted gate candidate"),
        ] {
            let err = f
                .execute_land(mode, false)
                .expect_err("ungated live d6 task land must fail closed");
            assert!(!err.contains("spawn ship"), "{err}");
            assert!(err.contains(expected), "{err}");
        }

        assert_eq!(f.cas_file_count(), cas_files_before);
        assert_eq!(
            f.harness
                .store
                .list_refs(Some(&f.project))
                .expect("list refs after"),
            refs_before
        );
        assert_eq!(
            f.harness
                .store
                .list_ref_log_after(0)
                .expect("list ref log after"),
            logs_before
        );
        assert_eq!(
            latest_main_ref_snapshot(&f.harness.store, &f.project),
            f.base_pub.snapshot.to_string()
        );

        let missing_cas = f.harness.guard.path.join("missing-cas");
        let missing_err = super::materialize_review_repo(
            &f.harness.store,
            &f.ctx,
            &f.resolved,
            Some(&missing_cas),
        )
        .err()
        .expect("missing cas fails closed");
        assert!(
            missing_err.contains("D6 CAS store missing"),
            "{missing_err}"
        );
        assert!(!missing_cas.join(spawn_cas::CAS_MARKER).exists());
    }

    #[test]
    fn d6_task_commits_diff_and_land_work_against_pg_and_cas() {
        let f = D6ReviewFixture::new("missions_d6_review", "missions-d6-review");
        f.assert_commits_and_diff();

        record_task_gate_acceptance(
            &f.harness.store,
            f.task_id,
            &f.base_pub.git_oid.to_hex(),
            &f.task_pub.git_oid.to_hex(),
        );
        f.execute_land(super::LandMode::FfOnly, false)
            .expect("live d6 ff-only task land succeeds");

        assert_eq!(
            latest_main_ref_snapshot(&f.harness.store, &f.project),
            f.task_pub.snapshot.to_string()
        );
        let logs = f.harness.store.list_ref_log_after(0).expect("list ref log");
        assert!(
            logs.iter().any(|row| row.project == f.project
                && row.name == "main"
                && row.old_snapshot.as_deref() == Some(f.base_pub.snapshot.to_string().as_str())
                && row.new_snapshot == f.task_pub.snapshot.to_string()),
            "missing D6 land entry in ref log: {logs:?}"
        );

        let repeat_err = f
            .execute_land(super::LandMode::FfOnly, false)
            .expect_err("re-landing already landed task must report nothing to land");
        assert!(repeat_err.contains("nothing to land"), "{repeat_err}");
    }

    fn publish_d6_main_and_task_head_pair(
        cas: &spawn_host::CasMerkleStore,
        ledger: &crate::sources::PostgresRefLedger,
        project: &str,
        task_ref: &str,
        tag: &str,
    ) -> (String, String) {
        use spawn_cas::{Tree, TreeEntry, TreeEntryKind};
        use spawn_host::{D6RefKind, PublishRequest, format_commit_body};

        let (b1, l1) = cas
            .put_bytes_blob(format!("base-{tag}\n").as_bytes())
            .unwrap();
        let t1 = cas
            .put_tree_cached(
                &Tree::from_entries(vec![TreeEntry {
                    kind: TreeEntryKind::File,
                    name: b"file.txt".to_vec(),
                    id: b1.as_bytes(),
                    size: l1,
                }])
                .unwrap(),
            )
            .unwrap();
        let body1 = format_commit_body(
            "T <t@s> 1700000000 +0000",
            "T <t@s> 1700000000 +0000",
            &format!("base {tag}\n"),
        );
        let p1 = cas
            .publish_snapshot(
                ledger,
                &PublishRequest {
                    project,
                    ref_name: "main",
                    kind: D6RefKind::Main,
                    expected_ref: None,
                    tree: &t1,
                    parents: &[],
                    commit_body: &body1,
                    created_at_ms: 1_700_000_000_000,
                    reason: "main",
                    landing_task_id: None,
                },
            )
            .unwrap();
        let (b2, l2) = cas
            .put_bytes_blob(format!("head-{tag}\n").as_bytes())
            .unwrap();
        let t2 = cas
            .put_tree_cached(
                &Tree::from_entries(vec![TreeEntry {
                    kind: TreeEntryKind::File,
                    name: b"file.txt".to_vec(),
                    id: b2.as_bytes(),
                    size: l2,
                }])
                .unwrap(),
            )
            .unwrap();
        let body2 = format_commit_body(
            "T <t@s> 1700000100 +0000",
            "T <t@s> 1700000100 +0000",
            &format!("head {tag}\n"),
        );
        let p2 = cas
            .publish_snapshot(
                ledger,
                &PublishRequest {
                    project,
                    ref_name: task_ref,
                    kind: D6RefKind::Branch,
                    expected_ref: None,
                    tree: &t2,
                    parents: std::slice::from_ref(&p1.snapshot),
                    commit_body: &body2,
                    created_at_ms: 1_700_000_100_000,
                    reason: "head",
                    landing_task_id: None,
                },
            )
            .unwrap();
        (p1.git_oid.to_hex(), p2.git_oid.to_hex())
    }

    #[test]
    fn d6_ledger_task_refs_read_only_selected_project_when_mirror_and_source_url_both_exist() {
        let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
            "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
        );
        let schema = spawn_postgres::testing::TestSchema::new("missions_d6_two_names");
        let store = std::sync::Arc::new(
            spawn_postgres::PostgresStore::connect_in_schema(&url, &schema)
                .expect("isolated schema"),
        );

        let guard = super::TempDirGuard::create("missions-d6-two-names").expect("temp dir");
        let root = guard.path.join("ws-root");
        std::fs::create_dir_all(&root).expect("create root");
        std::fs::write(root.join(".spawn-backend"), "d6\n").expect("backend marker");
        let source_url = "https://example.invalid/thinkseek/spawn.git";
        let mirror_project = crate::sources::mirror_path(&root, Some(source_url))
            .expect("mirror path")
            .to_string_lossy()
            .into_owned();
        let cas = spawn_host::CasMerkleStore::open(root.join(".cas")).expect("open cas");
        let ledger = crate::sources::PostgresRefLedger::new(std::sync::Arc::clone(&store));

        let mission_id = store
            .mission_add(
                "Remote D6 mission",
                &serde_json::json!([source_url]),
                "human",
            )
            .expect("mission add");
        let task_id = store
            .task_assign(
                mission_id,
                "ephemeral",
                "agent_d6",
                "two-name brief",
                "default",
            )
            .expect("task assign");
        let task_ref = spawn_host::workspace_rules::task_head_ref(task_id, "head");

        let (mirror_base, mirror_head) =
            publish_d6_main_and_task_head_pair(&cas, &ledger, &mirror_project, &task_ref, "mirror");
        let (url_base, url_head) =
            publish_d6_main_and_task_head_pair(&cas, &ledger, source_url, &task_ref, "url");
        assert_ne!(mirror_base, url_base);
        assert_ne!(mirror_head, url_head);

        let root_str = root.to_string_lossy().into_owned();
        let mut ctx = super::load_task_repo(&store, task_id, Some(&root_str)).expect("load d6 ctx");
        assert_eq!(
            ctx.repo,
            super::TaskRepo::D6Ledger {
                project: mirror_project
            }
        );

        let resolved = super::resolve_task_refs(&store, &ctx).expect("resolve mirror refs");
        assert_eq!(
            (resolved.base_sha.as_str(), resolved.head_sha.as_str()),
            (mirror_base.as_str(), mirror_head.as_str())
        );

        ctx.repo = super::TaskRepo::D6Ledger {
            project: source_url.to_string(),
        };
        let resolved_url = super::resolve_task_refs(&store, &ctx).expect("resolve url refs");
        assert_eq!(
            (
                resolved_url.base_sha.as_str(),
                resolved_url.head_sha.as_str()
            ),
            (url_base.as_str(), url_head.as_str())
        );

        ctx.repo = super::TaskRepo::D6Ledger {
            project: "missing-ledger-project".to_string(),
        };
        assert!(super::resolve_task_refs(&store, &ctx).is_err());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn mission_add_and_show_support_workspace_kinds_and_preserve_code_default() {
        let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
            "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
        );
        let schema = spawn_postgres::testing::TestSchema::new("mission_workspace_kinds");
        let store = spawn_postgres::PostgresStore::connect_in_schema(&url, &schema)
            .expect("isolated schema");
        let root = std::env::temp_dir().join(format!(
            "spawn-mission-kind-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("temp root");
        let runner = root.join("missing-runner.sh");

        let split_mission = super::split_mission;
        let show_words = [
            "show".to_string(),
            "--id".to_string(),
            "1".to_string(),
            "--json".to_string(),
        ];
        let (show_verb, show_flags) = split_mission(&show_words).expect("show --json flags parse");
        assert_eq!(show_verb, "show");
        assert!(show_flags.switch("json"));

        let bad_words = vec![
            "add".to_string(),
            "--text".to_string(),
            "test".to_string(),
            "--kind".to_string(),
            "video-only".to_string(),
        ];
        let (_, bad_flags) = split_mission(&bad_words).expect("flags");
        let err = super::add(&store, &bad_flags).expect_err("unknown kind rejected");
        assert!(err.contains("unknown workspace kind"), "{err}");

        for (raw_text, kind_flag) in [
            ("   ", Some("media")),
            ("[workspace:media]", None),
            ("[workspace:media]", Some("media")),
            ("  [workspace:code]  [workspace:factory]  ", Some("general")),
        ] {
            let mut empty_words = vec![
                "add".to_string(),
                "--text".to_string(),
                raw_text.to_string(),
            ];
            if let Some(kind) = kind_flag {
                empty_words.push("--kind".to_string());
                empty_words.push(kind.to_string());
            }
            let (_, empty_flags) = split_mission(&empty_words).expect("flags");
            let empty_err = super::add(&store, &empty_flags).expect_err("empty text rejected");
            assert!(empty_err.contains("needs text"), "{empty_err}");
        }

        let spoof_words = vec![
            "add".to_string(),
            "--text".to_string(),
            "[workspace:code] [workspace:media] ship fix".to_string(),
        ];
        let (_, spoof_flags) = split_mission(&spoof_words).expect("flags");
        let spoof_id = super::add(&store, &spoof_flags).expect("spoofed code mission added");
        let spoof_mission = store
            .mission_by_id(spoof_id)
            .expect("query")
            .expect("present");
        assert_eq!(spoof_mission["text"], "ship fix");
        let spoof_view = super::mission_view(&spoof_mission, &root, &runner, None, None, 100)
            .expect("spoof view");
        assert_eq!(spoof_view["workspace"]["kind"], "code");

        let boundary_words = vec![
            "add".to_string(),
            "--text".to_string(),
            "[workspace:media]foo".to_string(),
        ];
        let (_, boundary_flags) = split_mission(&boundary_words).expect("flags");
        let boundary_id =
            super::add(&store, &boundary_flags).expect("boundary literal mission added");
        let boundary_mission = store
            .mission_by_id(boundary_id)
            .expect("query")
            .expect("present");
        assert_eq!(boundary_mission["text"], "[workspace:media]foo");
        let boundary_view = super::mission_view(&boundary_mission, &root, &runner, None, None, 100)
            .expect("boundary view");
        assert_eq!(boundary_view["workspace"]["kind"], "code");

        let default_words = vec![
            "add".to_string(),
            "--text".to_string(),
            "code default".to_string(),
        ];
        let (_, default_flags) = split_mission(&default_words).expect("flags");
        let default_id = super::add(&store, &default_flags).expect("default mission added");
        let default_mission = store
            .mission_by_id(default_id)
            .expect("query")
            .expect("present");
        assert_eq!(default_mission["text"], "code default");
        let default_view = super::mission_view(&default_mission, &root, &runner, None, None, 100)
            .expect("default view");
        assert!(
            default_view.get("workspace_kind").is_none(),
            "{default_view}"
        );
        assert!(
            default_view.get("workspace_profile").is_none(),
            "{default_view}"
        );
        assert_eq!(default_view["workspace"]["kind"], "code");
        assert_eq!(
            default_view["workspace"]["summary"],
            spawn_core::mission::WorkspaceKind::Code.profile().summary
        );
        assert_eq!(default_view["gate"]["expects_code_gate"], true);
        assert!(
            default_view["checks"]
                .as_array()
                .expect("checks array")
                .iter()
                .any(|c| c.as_str().is_some_and(|s| s.contains("missing-runner.sh")))
        );

        for kind in [
            spawn_core::mission::WorkspaceKind::Media,
            spawn_core::mission::WorkspaceKind::Factory,
            spawn_core::mission::WorkspaceKind::General,
        ] {
            let kind_str = kind.as_str();
            let words = vec![
                "add".to_string(),
                "--text".to_string(),
                format!("produce {kind_str} workflow"),
                "--kind".to_string(),
                kind_str.to_string(),
            ];
            let (_, flags) = split_mission(&words).expect("flags");
            let id = super::add(&store, &flags).expect("kind mission added");
            let mission = store.mission_by_id(id).expect("query").expect("present");
            let view =
                super::mission_view(&mission, &root, &runner, None, None, 100).expect("view");
            assert!(view.get("workspace_kind").is_none(), "{view}");
            assert!(view.get("workspace_profile").is_none(), "{view}");
            assert_eq!(view["workspace"]["kind"], kind_str);
            assert_eq!(view["workspace"]["summary"], kind.profile().summary);
            assert_eq!(view["gate"]["expects_code_gate"], false);
            assert_eq!(view["gate"]["hint"], kind.profile().gate_hint);
            assert!(
                view["checks"]
                    .as_array()
                    .expect("checks array")
                    .iter()
                    .all(|c| !c.as_str().is_some_and(|s| s.contains("missing-runner.sh")))
            );
        }

        let media_source = root.join("media-source");
        let media_gate = root.join("media-gate");
        std::fs::create_dir_all(&media_source).expect("media source dir");
        std::fs::create_dir_all(&media_gate).expect("media gate dir");
        let media_source_str = media_source.to_string_lossy().into_owned();
        let media_gate_str = media_gate.to_string_lossy().into_owned();
        let gated_media_words = vec![
            "add".to_string(),
            "--text".to_string(),
            "render media with explicit gate".to_string(),
            "--kind".to_string(),
            "media".to_string(),
            "--source".to_string(),
            media_source_str.clone(),
            "--gate".to_string(),
            media_gate_str.clone(),
        ];
        let (_, gated_media_flags) = split_mission(&gated_media_words).expect("flags");
        let gated_media_id =
            super::add(&store, &gated_media_flags).expect("gated media mission added");
        let gated_media_mission = store
            .mission_by_id(gated_media_id)
            .expect("query")
            .expect("present");
        let gated_media_view =
            super::mission_view(&gated_media_mission, &root, &runner, None, None, 100)
                .expect("gated media view");
        assert_eq!(gated_media_view["workspace"]["kind"], "media");
        assert_eq!(gated_media_view["gate"]["expects_code_gate"], false);
        assert_eq!(gated_media_view["gate"]["scenarios_dir"], media_gate_str);
        assert_eq!(gated_media_view["gate"]["exists"], true);
        let quoted_runner = super::shell_quote(&runner.to_string_lossy());
        let expected_runner_check = format!("test -f {quoted_runner} && test -r {quoted_runner}");
        let expected_gate_dir_check = format!("test -d {}", super::shell_quote(&media_gate_str));
        let gated_checks: Vec<&str> = gated_media_view["checks"]
            .as_array()
            .expect("checks array")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        assert!(
            gated_checks.contains(&expected_runner_check.as_str()),
            "{gated_checks:?}"
        );
        assert!(
            gated_checks.contains(&expected_gate_dir_check.as_str()),
            "{gated_checks:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn task_repo_landability_and_description_match_adt() {
        let mk_ctx = |repo: super::TaskRepo, source: Option<&str>| super::TaskRepoContext {
            task_id: 1,
            mission_id: 1,
            agent_id: "w_1".to_string(),
            brief: "test".to_string(),
            candidate: None,
            repo,
            root: None,
            source: source.map(str::to_string),
        };

        let bare_local = mk_ctx(
            super::TaskRepo::LocalGit {
                git_dir: std::path::PathBuf::from("/tmp/origin.git"),
                bare: true,
            },
            None,
        );
        assert!(bare_local.is_landable());
        assert_eq!(
            bare_local.repo.describe(),
            "local repository /tmp/origin.git"
        );

        let work_local = mk_ctx(
            super::TaskRepo::LocalGit {
                git_dir: std::path::PathBuf::from("/tmp/work"),
                bare: false,
            },
            Some("/tmp/work"),
        );
        assert!(!work_local.is_landable());

        let remote_mirror = mk_ctx(
            super::TaskRepo::LocalGit {
                git_dir: std::path::PathBuf::from("/tmp/mirror.git"),
                bare: true,
            },
            Some("https://github.com/org/repo.git"),
        );
        assert!(!remote_mirror.is_landable());

        let d6_default = mk_ctx(
            super::TaskRepo::D6Ledger {
                project: "default".to_string(),
            },
            None,
        );
        assert!(d6_default.is_landable());
        assert!(d6_default.repo.local_git_dir().is_none());
        assert_eq!(d6_default.repo.describe(), "D6 ledger project \"default\"");

        let d6_remote = mk_ctx(
            super::TaskRepo::D6Ledger {
                project: "https://github.com/org/repo.git".to_string(),
            },
            Some("https://github.com/org/repo.git"),
        );
        assert!(!d6_remote.is_landable());
        assert_eq!(
            d6_remote.repo.describe(),
            "D6 ledger project \"https://github.com/org/repo.git\""
        );
    }

    fn record_task_gate_acceptance(
        store: &spawn_postgres::PostgresStore,
        task_id: i64,
        base_oid: &str,
        candidate_oid: &str,
    ) {
        store.task_delivered(task_id, 1).expect("delivered");
        assert!(store.task_reported(task_id, "done").expect("reported"));
        store
            .integrator_record(&json!({
                "type": "task.gated",
                "id": task_id,
                "revision": 0,
                "verdict": "accepted",
                "base": base_oid,
                "candidate": candidate_oid,
            }))
            .expect("accepted verdict");
    }

    struct PublishSpec<'a> {
        project: &'a str,
        kind: spawn_host::D6RefKind,
        ref_name: &'a str,
        files: &'a [(&'a [u8], &'a [u8])],
        parents: &'a [spawn_cas::Digest],
        at_ms: i64,
        msg: &'a str,
    }

    fn publish_tree_commit(
        cas: &spawn_host::CasMerkleStore,
        ledger: &crate::sources::PostgresRefLedger,
        spec: &PublishSpec<'_>,
    ) -> spawn_host::PublishedSnapshot {
        use spawn_cas::{Tree, TreeEntry, TreeEntryKind};
        use spawn_host::{PublishRequest, format_commit_body};

        let mut entries = Vec::with_capacity(spec.files.len());
        for (name, bytes) in spec.files {
            let (blob, len) = cas.put_bytes_blob(bytes).expect("blob");
            entries.push(TreeEntry {
                kind: TreeEntryKind::File,
                name: name.to_vec(),
                id: blob.as_bytes(),
                size: len,
            });
        }
        let tree = cas
            .put_tree_cached(&Tree::from_entries(entries).expect("tree"))
            .expect("put tree");
        let ts_secs = spec.at_ms / 1000;
        let author = format!("Spawn Test <test@spawn.local> {ts_secs} +0000");
        let body = format_commit_body(&author, &author, &format!("{}\n", spec.msg));
        cas.publish_snapshot(
            ledger,
            &PublishRequest {
                project: spec.project,
                ref_name: spec.ref_name,
                kind: spec.kind,
                expected_ref: None,
                tree: &tree,
                parents: spec.parents,
                commit_body: &body,
                created_at_ms: spec.at_ms,
                reason: spec.msg,
                landing_task_id: None,
            },
        )
        .expect("publish snapshot")
    }

    struct TestD6Harness {
        _schema: spawn_postgres::testing::TestSchema,
        store: std::sync::Arc<spawn_postgres::PostgresStore>,
        guard: super::TempDirGuard,
        root: std::path::PathBuf,
        root_str: String,
        cas_dir: std::path::PathBuf,
        cas: spawn_host::CasMerkleStore,
        ledger: crate::sources::PostgresRefLedger,
    }

    impl TestD6Harness {
        fn new(schema_name: &str, dir_prefix: &str, mark_d6: bool) -> Self {
            let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
                "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
            );
            let schema = spawn_postgres::testing::TestSchema::new(schema_name);
            let store = std::sync::Arc::new(
                spawn_postgres::PostgresStore::connect_in_schema(&url, &schema).expect("test pg"),
            );
            let guard = super::TempDirGuard::create(dir_prefix).expect("temp dir");
            let root = guard.path.join("ws-root");
            std::fs::create_dir_all(&root).expect("create root");
            if mark_d6 {
                std::fs::write(root.join(".spawn-backend"), "d6\n").expect("backend marker");
            }
            let root_str = root.to_string_lossy().into_owned();
            let cas_dir = root.join(".cas");
            let cas = spawn_host::CasMerkleStore::open(&cas_dir).expect("open cas");
            let ledger = crate::sources::PostgresRefLedger::new(std::sync::Arc::clone(&store));
            Self {
                _schema: schema,
                store,
                guard,
                root,
                root_str,
                cas_dir,
                cas,
                ledger,
            }
        }

        fn land(&self, task_id: i64, mode: super::LandMode) -> Result<(), String> {
            super::execute_task_land(
                &self.store,
                &super::TaskLandArgs {
                    task_id,
                    mode,
                    dry_run: false,
                    json: false,
                    root: Some(self.root_str.clone()),
                },
                Some(&self.cas_dir),
            )
        }

        fn publish_main(
            &self,
            project: &str,
            files: &[(&[u8], &[u8])],
            at_ms: i64,
            msg: &str,
        ) -> spawn_host::PublishedSnapshot {
            publish_tree_commit(
                &self.cas,
                &self.ledger,
                &PublishSpec {
                    project,
                    kind: spawn_host::D6RefKind::Main,
                    ref_name: "main",
                    files,
                    parents: &[],
                    at_ms,
                    msg,
                },
            )
        }

        fn publish_branch(
            &self,
            project: &str,
            ref_name: &str,
            files: &[(&[u8], &[u8])],
            parents: &[spawn_cas::Digest],
            at_ms: i64,
            msg: &str,
        ) -> spawn_host::PublishedSnapshot {
            publish_tree_commit(
                &self.cas,
                &self.ledger,
                &PublishSpec {
                    project,
                    kind: spawn_host::D6RefKind::Branch,
                    ref_name,
                    files,
                    parents,
                    at_ms,
                    msg,
                },
            )
        }
    }

    fn latest_main_ref_snapshot(store: &spawn_postgres::PostgresStore, project: &str) -> String {
        store
            .list_refs(Some(project))
            .expect("list refs")
            .into_iter()
            .find(|r| matches!(r.kind, spawn_postgres::RefKind::Main))
            .expect("main present")
            .snapshot
    }

    fn init_bare_repo_with_readme(
        scratch: &std::path::Path,
        body: &str,
    ) -> (std::path::PathBuf, String) {
        let bare = scratch.join("origin.git");
        let work = scratch.join("work");
        let bare_s = bare.to_string_lossy().into_owned();
        let work_s = work.to_string_lossy().into_owned();
        assert!(crate::sources::test_git(&["init", "--bare", "-q", &bare_s]).is_ok());
        assert!(crate::sources::test_git(&["init", "-q", &work_s]).is_ok());
        assert!(crate::sources::test_git(&["-C", &work_s, "config", "user.name", "T"]).is_ok());
        assert!(crate::sources::test_git(&["-C", &work_s, "config", "user.email", "t@t"]).is_ok());
        std::fs::write(work.join("README.md"), body).expect("write");
        assert!(crate::sources::test_git(&["-C", &work_s, "add", "README.md"]).is_ok());
        assert!(crate::sources::test_git(&["-C", &work_s, "commit", "-q", "-m", "v1"]).is_ok());
        assert!(crate::sources::test_git(&["-C", &work_s, "branch", "-M", "main"]).is_ok());
        assert!(
            crate::sources::test_git(&["-C", &work_s, "remote", "add", "origin", &bare_s]).is_ok()
        );
        assert!(crate::sources::test_git(&["-C", &work_s, "push", "-q", "origin", "main"]).is_ok());
        let sha = super::resolve_main_sha(&bare).expect("bare main sha");
        (bare, sha)
    }

    #[test]
    fn d6_task_land_squash_diverged_branch_advances_ledger_main() {
        let h = TestD6Harness::new("missions_d6_squash", "missions-d6-squash", true);
        let project = h
            .guard
            .path
            .join("d6-squash-source.git")
            .to_string_lossy()
            .into_owned();
        let m1 = h.publish_main(
            &project,
            &[(b"hello.txt", b"base\n")],
            1_700_000_000_000,
            "m1",
        );
        let mission_id = h
            .store
            .mission_add("D6 squash test", &json!([project]), "human")
            .expect("mission add");
        let adv_task = h
            .store
            .task_assign(mission_id, "ephemeral", "w_adv", "advance main", "default")
            .expect("assign adv");
        let adv_head = spawn_host::workspace_rules::task_head_ref(adv_task, "head");
        let m2_cand = h.publish_branch(
            &project,
            &adv_head,
            &[(b"hello.txt", b"base\n"), (b"other.txt", b"main v2\n")],
            std::slice::from_ref(&m1.snapshot),
            1_700_000_050_000,
            "add other",
        );
        record_task_gate_acceptance(
            &h.store,
            adv_task,
            &m1.git_oid.to_hex(),
            &m2_cand.git_oid.to_hex(),
        );
        h.land(adv_task, super::LandMode::FfOnly)
            .expect("advance main to m2");

        // Diverged task modifies hello.txt on top of m1 while main is now at m2.
        let div_task = h
            .store
            .task_assign(mission_id, "ephemeral", "w_div", "squash task", "default")
            .expect("assign div");
        let div_head = spawn_host::workspace_rules::task_head_ref(div_task, "head");
        h.publish_branch(
            &project,
            &div_head,
            &[(b"hello.txt", b"task edited\n")],
            std::slice::from_ref(&m1.snapshot),
            1_700_000_100_000,
            "edit hello",
        );
        let ff_err = h
            .land(div_task, super::LandMode::FfOnly)
            .expect_err("diverged branch cannot fast-forward");
        assert!(
            ff_err.contains("branch has diverged; use --squash"),
            "{ff_err}"
        );
        let ungated_err = h
            .land(div_task, super::LandMode::Squash)
            .expect_err("ungated squash refuses without accepted gate candidate");
        assert!(
            ungated_err.contains("no accepted gate candidate"),
            "{ungated_err}"
        );
        assert_eq!(
            latest_main_ref_snapshot(&h.store, &project),
            m2_cand.snapshot.to_string()
        );

        let div_accepted = spawn_host::workspace_rules::task_head_ref(div_task, "accepted");
        let gated_squash = h.publish_branch(
            &project,
            &div_accepted,
            &[
                (b"hello.txt", b"task edited\n"),
                (b"other.txt", b"main v2\n"),
            ],
            std::slice::from_ref(&m2_cand.snapshot),
            1_700_000_200_000,
            "squash merged",
        );
        record_task_gate_acceptance(
            &h.store,
            div_task,
            &m2_cand.git_oid.to_hex(),
            &gated_squash.git_oid.to_hex(),
        );
        h.land(div_task, super::LandMode::Squash)
            .expect("live d6 squash land succeeds");
        assert_eq!(
            latest_main_ref_snapshot(&h.store, &project),
            gated_squash.snapshot.to_string()
        );
    }

    #[test]
    fn d6_readonly_git_border_routes_task_land_to_ledger_without_mutating_bare_git() {
        let h = TestD6Harness::new("missions_d6_border", "missions-d6-border", false);
        let (bare, bare_main_v1) = init_bare_repo_with_readme(&h.guard.path, "border v1\n");
        let border = spawn_host::GitBorderAdapter::new();
        let project = bare.to_string_lossy().into_owned();
        let mut cutover = spawn_host::ProjectCutover::new(&project, &bare, &h.root);
        cutover
            .step1_backfill_history(&h.cas, &h.ledger)
            .expect("s1");
        cutover
            .step2_pause_dispatch_and_verify_clean(&h.cas)
            .expect("s2");
        cutover
            .step3_import_tail_commits(&h.cas, &h.ledger)
            .expect("s3");
        cutover
            .step4_commit_refs_and_flip_mode(&h.cas, &h.ledger, &border, "border cutover")
            .expect("s4");
        cutover.step5_verify_parity(&h.cas, &h.ledger).expect("s5");
        cutover
            .step6_configure_readonly_border(&border)
            .expect("s6");
        assert!(spawn_host::is_readonly_border(&bare).expect("ro border active"));

        // Overwrite root marker to `git` so only the read-only Git border classifies this task as D6.
        std::fs::write(h.root.join(".spawn-backend"), "git\n").expect("flip root marker");

        let mission_id = h
            .store
            .mission_add("Border D6 land test", &json!([project]), "human")
            .expect("mission add");
        let task_id = h
            .store
            .task_assign(
                mission_id,
                "ephemeral",
                "w_border",
                "border work",
                "default",
            )
            .expect("assign");
        let main_snap = spawn_cas::Digest::parse(&latest_main_ref_snapshot(&h.store, &project))
            .expect("main digest");
        let head_ref = spawn_host::workspace_rules::task_head_ref(task_id, "head");
        let cand = h.publish_branch(
            &project,
            &head_ref,
            &[(b"README.md", b"border v2\n")],
            std::slice::from_ref(&main_snap),
            1_700_000_300_000,
            "border v2",
        );
        record_task_gate_acceptance(&h.store, task_id, &bare_main_v1, &cand.git_oid.to_hex());

        let ctx = super::load_task_repo(&h.store, task_id, Some(&h.root_str)).expect("load ctx");
        assert_eq!(ctx.storage(true), crate::sources::ProjectStorage::D6);
        h.land(task_id, super::LandMode::FfOnly)
            .expect("readonly border routes land to D6 ledger");

        assert_eq!(
            latest_main_ref_snapshot(&h.store, &project),
            cand.snapshot.to_string()
        );
        assert_eq!(
            super::resolve_main_sha(&bare).expect("bare main unchanged"),
            bare_main_v1
        );
    }
}
