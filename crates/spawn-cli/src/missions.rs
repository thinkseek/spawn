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

use crate::{Flags, open_store};

const USAGE: &str = "usage:
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
                (with add, all of them at once; with configure, any of them alone, and
                --backlog with --resident for a mission without objectives)
                (--features: the mission's feature map, a JSON file of capabilities and
                their provenance sources; identities and links only, never a status)
                (--workers: objective tasks in flight at once, 1 to 16, default 1: the
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
  spawn mission close --id N --state done|dropped [--summary <text>]
  spawn task assign --mission N --kind ephemeral|resident --worker <label|resident> --brief <text>
                    [--model default|cheap] [--again] [--pin]   (cheap: every model on DeepSeek)
      (the supervisor's brief that a task of the mission in flight already has, trimmed of
       space, tab, CR and LF, is refused, naming that task; --again assigns it on purpose.
       Every duplicate assigned names the task it repeats in its record: duplicate_of.
       --pin keeps the task with its worker: it is never stolen)
  spawn task accept --id N [--revision R]
  spawn task reject --id N [--revision R] --note <what to fix>
      (R: the report's revision as read; refused once the task has moved on)
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
struct Settings {
    gate: Option<String>,
    accept: String,
    budget: Option<Value>,
    objectives: Option<Value>,
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
        let objectives = match (directory(flags, "backlog")?, flags.get("resident")) {
            (Some(backlog), Some(resident)) => {
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
        Ok(Self {
            gate: directory(flags, "gate")?,
            accept: flags.get("accept").unwrap_or_else(|| "review".to_string()),
            budget: (!budget.is_empty()).then_some(Value::Object(budget)),
            objectives,
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
        "add" => {
            // Checked here for a clear error; spawn_mission_add holds the
            // same rule for every caller.
            let sources = match flags.get("source") {
                Some(source) => json!([crate::sources::MissionSource::parse(&source)?.as_str()]),
                None => json!([]),
            };
            let settings = Settings::from(&flags)?;
            // spawn_mission_configure refuses it too, but only once the
            // mission exists.
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
                .mission_add(&text(&flags, "text")?, &sources, "human")
                .map_err(err)?;
            if settings.gate.is_some() {
                settings.apply(&store, id)?;
            }
            print(&json!({ "mission": id }));
        }
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
        "assign" => {
            let model = model(&flags)?;
            let task = store
                .task_assign_with(
                    id(&flags, "mission")?,
                    &text(&flags, "kind")?,
                    &flags.get("worker").unwrap_or_default(),
                    &text(&flags, "brief")?,
                    model.as_str(),
                    flags.switch("again"),
                    flags.switch("pin"),
                )
                .map_err(err)?;
            print(&json!({ "task": task, "model": model.as_str() }));
        }
        "accept" => {
            let task = id(&flags, "id")?;
            store
                .task_review(task, true, "", revision(&flags)?)
                .map_err(err)?;
            print(&json!({ "task": task, "accepted": true }));
        }
        "reject" => {
            let task = id(&flags, "id")?;
            store
                .task_review(task, false, &text(&flags, "note")?, revision(&flags)?)
                .map_err(err)?;
            print(&json!({ "task": task, "sent_back": true }));
        }
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

    use serde_json::json;

    use super::{
        change, hold_bound, model, objectives_row, pushes_commit, pushes_limit, split_mission,
        withdrawn,
    };
    use crate::Flags;

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
        let words: Vec<String> = [
            "add",
            "--text",
            "Spawn RSI",
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
}
