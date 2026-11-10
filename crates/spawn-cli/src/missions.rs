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
      --no-budget (both fields), --no-minutes, --no-tool-calls, --no-objectives,
      --no-workers (back to the default 1); a switch and a value for the same setting
      are refused. Prints {mission, before, after}; with nothing given it changes nothing.
      Clearing the gate, or moving accept away from gate, waits while a task is landing.
      settings: --gate <scenarios dir> [--accept review|gate] [--minutes N] [--tool-calls N]
                [--backlog <dir> --resident <name> [--workers N]]
                (with add, all of them at once; with configure, any of them alone, and
                --backlog with --resident for a mission without objectives)
                (--workers: objective tasks in flight at once, 1 to 16, default 1: the
                resident takes one, per-task workers the rest)
  spawn mission objectives [--mission N]
      the backlog of every objective mission (--mission: that one): the fleet's open
      objectives and separately the owner-judged ones (the backlog field `owner_judged`:
      the owner judges those outside the gate, and the fleet never assigns them),
      each with its title, plus the counts of solved and met
  spawn mission resume --id N
  spawn mission push --source <path> --commit <commit> [--repo <dir>] [--wait <seconds>]
                     [--gated] [--urgent] [--reason <text>]
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
       pending again after a host fault or no room. --urgent: a judging of a task in flight
       on the source is stopped and recorded as discarded by this request (stale, counted
       nowhere; the task is judged again on the new main), for a fix of a red main.
       --reason: the owner's words, on the record. --wait: wait that long for
       the outcome and print it; exit 0 applied, 1 refused, 3 still pending)
  spawn mission pushes --source <path> [--commit <hash or prefix>] [--limit N]
      (the source's owner landing requests, newest first (default 20, at most 1000), pending
       and decided: each {request, source, commit, ref, at_ms, gated, urgent, reason, state,
       outcome}, state pending, applied or refused as push --wait says it and outcome its
       record (null while pending); --commit: that commit's requests only. <path> as the
       request named it. Records nothing)
  spawn mission moved --source <path> --from <commit> --to <commit>   (owner: a deploy moved main)
      (recorded as source.moved for every deploy, also when no mission's baseline moves;
       after it every delivery whose retries are spent, having typed nothing, is tried once more)
  spawn mission hold --source <path> [--reason <text>]   (owner: no task lands on the source until
      a record ends the hold: its release, or its expiry SPAWN_SOURCE_HOLD_SECS after it (1 to
      86400, default 10800); its gates still run and its accepted tasks wait; a hold on a held
      source renews it. Prints the hold and the main it was taken at: gate against that main)
  spawn mission release --source <path>   (owner: the waiting tasks land at the next pass; with
      no hold it records nothing and prints `released` null)
  spawn mission list [--limit N]   (every task with its stages)
  spawn mission close --id N --state done|dropped [--summary <text>]
  spawn task assign --mission N --kind ephemeral|resident --worker <label|resident> --brief <text>
                    [--model default|cheap]   (cheap: every model on DeepSeek)
  spawn task accept --id N [--revision R]
  spawn task reject --id N [--revision R] --note <what to fix>
      (R: the report's revision as read; refused once the task has moved on)
  spawn task cancel --id N
      (a delivered task's resident is stopped at once, its work set aside on its branch,
       and it is told before its next task; an ephemeral worker is stopped)
  spawn task stages --id N   (each stage with its verdict, time, and how long since the one before)
  spawn resident propose --name <name> --charter <text>
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
        let workers = number("workers")?;
        let objectives = match (directory(flags, "backlog")?, flags.get("resident")) {
            (Some(backlog), Some(resident)) => {
                let mut objectives = json!({"backlog": backlog, "resident": resident});
                // Absent means 1, the default: a mission configured without
                // it stores exactly what it did before.
                if let (Some(workers), Some(map)) = (workers, objectives.as_object_mut()) {
                    map.insert("workers".to_string(), json!(workers));
                }
                Some(objectives)
            }
            (None, None) if workers.is_none() => None,
            (None, None) => {
                return Err(format!(
                    "--workers goes with --backlog and --resident\n{USAGE}"
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

/// `configure`'s clearing switches (they take no value), and the setting
/// (and key in it) each clears.
const CLEARING: [(&str, &str, Option<&str>); 6] = [
    ("no-gate", "gate", None),
    ("no-budget", "budget", None),
    ("no-minutes", "budget", Some("minutes")),
    ("no-tool-calls", "budget", Some("tool_calls")),
    ("no-objectives", "objectives", None),
    ("no-workers", "objectives", Some("workers")),
];

/// The change `spawn mission configure` asks for (objective 027): only the
/// settings its flags name, for `spawn_mission_reconfigure` to merge into
/// the mission's own under the append lock; each `--no-<setting>` switch
/// sets its setting to null, which clears it. A setting both given and
/// cleared, or a flag configure does not know, is refused. No flag at all is
/// an empty change, which changes and records nothing.
fn change(flags: &Flags) -> Result<Value, String> {
    const KNOWN: [&str; 8] = [
        "id",
        "gate",
        "accept",
        "minutes",
        "tool-calls",
        "backlog",
        "resident",
        "workers",
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
    if let Some(backlog) = directory(flags, "backlog")? {
        part("objectives", Some("backlog"), json!(backlog));
    }
    if let Some(resident) = flags.get("resident") {
        part("objectives", Some("resident"), json!(resident));
    }
    if let Some(workers) = number("workers")? {
        part("objectives", Some("workers"), json!(workers));
    }
    // Every value flag and the setting (and key) it writes.
    let written = [
        ("gate", "gate", None),
        ("accept", "accept", None),
        ("minutes", "budget", Some("minutes")),
        ("tool-calls", "budget", Some("tool_calls")),
        ("backlog", "objectives", Some("backlog")),
        ("resident", "objectives", Some("resident")),
        ("workers", "objectives", Some("workers")),
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
    let missions = store
        .objective_missions()
        .map_err(|error| error.to_string())?;
    let missions = missions.as_array().cloned().unwrap_or_default();
    let rows: Vec<Value> = missions
        .into_iter()
        .map(crate::serve::with_objectives)
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
        "resume" => {
            let mission = id(&flags, "id")?;
            store.mission_resume(mission).map_err(err)?;
            print(&json!({ "mission": mission, "resumed": true }));
        }
        "push" => return push(&store, &flags),
        "pushes" => print(&pushes(&store, &flags)?),
        "moved" => {
            let source = text(&flags, "source")?;
            crate::sources::MissionSource::parse(&source)?;
            let from = text(&flags, "from")?;
            let to = text(&flags, "to")?;
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
            print(&json!({ "source": source, "to": to, "missions": missions }));
        }
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
fn push(store: &spawn_postgres::PostgresStore, flags: &Flags) -> Result<i32, String> {
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
    let (verb, flags) = split(rest)?;
    let store = open_store()?;
    let err = |error: spawn_core::port::StoreError| error.to_string();
    match verb {
        "assign" => {
            let model = model(&flags)?;
            let task = store
                .task_assign(
                    id(&flags, "mission")?,
                    &text(&flags, "kind")?,
                    &flags.get("worker").unwrap_or_default(),
                    &text(&flags, "brief")?,
                    model.as_str(),
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
        "stages" => print(&store.task_stages(id(&flags, "id")?).map_err(err)?),
        _ => return Err(USAGE.to_string()),
    }
    Ok(0)
}

/// `spawn resident ...`
pub(crate) fn resident(rest: &[String]) -> Result<i32, String> {
    let (verb, flags) = split(rest)?;
    let store = open_store()?;
    let err = |error: spawn_core::port::StoreError| error.to_string();
    let name = text(&flags, "name")?;
    match verb {
        "propose" => store
            .resident_propose(&name, &text(&flags, "charter")?)
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
