//! `spawn mission`, `spawn task` and `spawn resident`: the mission layer's
//! commands (docs/fleet-architecture.md §3.2).
//!
//! Humans run them with the owner's login, the supervisor with its own; the
//! SQL functions behind them decide who may do what (only a human approves a
//! resident). Every command prints JSON.

use serde_json::{Value, json};
use spawn_core::event::ModelProfile;

use crate::{Flags, open_store};

const USAGE: &str = "usage:
  spawn mission add --text <mission> [--source <git repo path or URL>] [<settings>]
  spawn mission configure --id N [<settings>] [--clear <names>]   (owner only)
      changes only the settings it is given and keeps the mission's others; --clear removes
      some, a comma-separated list of gate, budget, minutes, tool-calls, objectives, workers
      (e.g. --clear minutes,workers). Prints every setting the mission has after the change.
      settings: --gate <scenarios dir> [--accept review|gate] [--minutes N] [--tool-calls N]
                [--backlog <dir> --resident <name> [--workers N]]
                (with add, all of them at once; with configure, any of them alone)
                (--workers: objective tasks in flight at once, 1 to 16, default 1: the
                resident takes one, per-task workers the rest)
  spawn mission resume --id N
  spawn mission moved --source <path> --from <commit> --to <commit>   (owner: a deploy moved main)
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

/// What `--clear` may name, and the setting (and key in it) each clears.
const CLEARABLE: [(&str, &str, Option<&str>); 6] = [
    ("gate", "gate", None),
    ("budget", "budget", None),
    ("minutes", "budget", Some("minutes")),
    ("tool-calls", "budget", Some("tool_calls")),
    ("objectives", "objectives", None),
    ("workers", "objectives", Some("workers")),
];

/// The change `spawn mission configure` asks for: only the settings its
/// flags name, for `spawn_mission_reconfigure` to merge into the mission's
/// own; `--clear a,b` sets the named ones to null. A setting both given and
/// cleared, an unknown name, or no change at all is refused.
fn change(flags: &Flags) -> Result<Value, String> {
    const KNOWN: [&str; 9] = [
        "id",
        "gate",
        "accept",
        "minutes",
        "tool-calls",
        "backlog",
        "resident",
        "workers",
        "clear",
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
    for name in flags.get("clear").iter().flat_map(|list| list.split(',')) {
        let name = name.trim();
        let Some((_, setting, key)) = CLEARABLE.iter().find(|(flag, _, _)| *flag == name) else {
            return Err(format!(
                "--clear names gate, budget, minutes, tool-calls, objectives or workers, not \
                 {name}\n{USAGE}"
            ));
        };
        let conflict = written.iter().any(|(flag, written_setting, written_key)| {
            flags.get(flag).is_some()
                && written_setting == setting
                && (key.is_none() || written_key == key)
        });
        if conflict {
            return Err(format!(
                "--clear {name} and a value for it together\n{USAGE}"
            ));
        }
        part(setting, *key, Value::Null);
    }
    if change.is_empty() {
        return Err(format!("nothing to change\n{USAGE}"));
    }
    Ok(Value::Object(change))
}

/// `spawn mission ...`
pub(crate) fn mission(rest: &[String]) -> Result<i32, String> {
    let (verb, flags) = split(rest)?;
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

    use super::{change, hold_bound, model};
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

    #[test]
    fn configure_clears_only_what_clear_names() {
        assert_eq!(
            change(&flags(&["--id", "4", "--clear", "minutes,workers"])),
            Ok(json!({"budget": {"minutes": null}, "objectives": {"workers": null}}))
        );
        assert_eq!(
            change(&flags(&["--id", "4", "--clear", "gate,objectives,budget"])),
            Ok(json!({"gate": null, "objectives": null, "budget": null}))
        );
        assert_eq!(
            change(&flags(&[
                "--id",
                "4",
                "--minutes",
                "30",
                "--clear",
                "tool-calls"
            ])),
            Ok(json!({"budget": {"minutes": 30, "tool_calls": null}}))
        );
    }

    #[test]
    fn configure_refuses_an_empty_unknown_or_contradictory_change() {
        for (words, said) in [
            (vec!["--id", "4"], "nothing to change"),
            (
                vec!["--id", "4", "--toolcalls", "600"],
                "has no --toolcalls",
            ),
            (vec!["--id", "4", "--clear", "accept"], "--clear names"),
            (
                vec!["--id", "4", "--minutes", "5", "--clear", "budget"],
                "together",
            ),
            (
                vec!["--id", "4", "--minutes", "5", "--clear", "minutes"],
                "together",
            ),
            (
                vec!["--id", "4", "--workers", "2", "--clear", "objectives"],
                "together",
            ),
            (
                vec!["--id", "4", "--resident", "r", "--clear", "objectives"],
                "together",
            ),
        ] {
            let refused = change(&flags(&words)).expect_err(said);
            assert!(refused.contains(said), "{words:?}: {refused}");
        }
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
}
