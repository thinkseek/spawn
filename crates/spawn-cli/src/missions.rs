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
  spawn mission configure --id N <settings>
      settings: --gate <scenarios dir> [--accept review|gate] [--minutes N] [--tool-calls N]
                [--backlog <dir> --resident <name>]   (owner only)
  spawn mission resume --id N
  spawn mission moved --source <path> --from <commit> --to <commit>   (owner: a deploy moved main)
  spawn mission list [--limit N]   (every task with its stages)
  spawn mission close --id N --state done|dropped [--summary <text>]
  spawn task assign --mission N --kind ephemeral|resident --worker <label|resident> --brief <text>
                    [--model default|cheap]   (cheap: every model on DeepSeek)
  spawn task accept --id N [--revision R]
  spawn task reject --id N [--revision R] --note <what to fix>
      (R: the report's revision as read; refused once the task has moved on)
  spawn task cancel --id N
  spawn task stages --id N   (each stage with its verdict, time, and how long since the one before)
  spawn resident propose --name <name> --charter <text>
  spawn resident approve --name <name>
  spawn resident retire --name <name>";

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
        let objectives = match (directory(flags, "backlog")?, flags.get("resident")) {
            (Some(backlog), Some(resident)) => {
                Some(json!({"backlog": backlog, "resident": resident}))
            }
            (None, None) => None,
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
            let settings = Settings::from(&flags)?;
            settings.apply(&store, mission)?;
            print(
                &json!({ "mission": mission, "gate": settings.gate, "accept": settings.accept,
                           "objectives": settings.objectives }),
            );
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

    use super::model;
    use crate::Flags;

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
