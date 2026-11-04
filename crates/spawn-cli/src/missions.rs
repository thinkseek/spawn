//! `spawn mission`, `spawn task` and `spawn resident`: the mission layer's
//! commands (docs/fleet-architecture.md §3.2).
//!
//! Humans run them with the owner's login, the supervisor with its own; the
//! SQL functions behind them decide who may do what (only a human approves a
//! resident). Every command prints JSON.

use serde_json::{Value, json};

use crate::{Flags, open_store};

const USAGE: &str = "usage:
  spawn mission add --text <mission> [--source <git repo path or URL>]
                    [--gate <scenarios dir> [--accept review|gate] [--minutes N] [--tool-calls N]]
  spawn mission configure --id N [--gate <scenarios dir>] [--accept review|gate] [--minutes N] [--tool-calls N]
  spawn mission resume --id N
  spawn mission list [--limit N]
  spawn mission close --id N --state done|dropped [--summary <text>]
  spawn task assign --mission N --kind ephemeral|resident --worker <label|resident> --brief <text>
  spawn task accept --id N
  spawn task reject --id N --note <what to fix>
  spawn task cancel --id N
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

fn split(rest: &[String]) -> Result<(&str, Flags), String> {
    let Some((verb, rest)) = rest.split_first() else {
        return Err(USAGE.to_string());
    };
    Ok((verb.as_str(), Flags::parse(rest)?))
}

/// A mission's landing settings from the flags: the gate as an absolute
/// path (resolved against the working directory), the acceptance rule, and
/// the budget. `None` when no flag sets a gate.
fn landing(flags: &Flags) -> Result<Option<(String, String, Option<Value>)>, String> {
    let Some(gate) = flags.get("gate") else {
        return Ok(None);
    };
    let path = std::path::PathBuf::from(&gate);
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map_err(|error| format!("current directory: {error}"))?
            .join(path)
    };
    if !path.is_dir() {
        return Err(format!("--gate {} is not a directory", path.display()));
    }
    let accept = flags.get("accept").unwrap_or_else(|| "review".to_string());
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
    Ok(Some((
        path.display().to_string(),
        accept,
        (!budget.is_empty()).then_some(Value::Object(budget)),
    )))
}

/// `spawn mission ...`
pub(crate) fn mission(rest: &[String]) -> Result<i32, String> {
    let (verb, flags) = split(rest)?;
    let store = open_store()?;
    let err = |error: spawn_core::port::StoreError| error.to_string();
    match verb {
        "add" => {
            let sources = flags
                .get("source")
                .map_or_else(|| json!([]), |source| json!([source]));
            let landing = landing(&flags)?;
            let id = store
                .mission_add(&text(&flags, "text")?, &sources, "human")
                .map_err(err)?;
            if let Some((gate, accept, budget)) = landing {
                store
                    .mission_configure(id, Some(&gate), &accept, budget.as_ref())
                    .map_err(err)?;
            }
            print(&json!({ "mission": id }));
        }
        "configure" => {
            let mission = id(&flags, "id")?;
            let (gate, accept, budget) = landing(&flags)?.map_or(
                (
                    None,
                    flags.get("accept").unwrap_or_else(|| "review".to_string()),
                    None,
                ),
                |(gate, accept, budget)| (Some(gate), accept, budget),
            );
            store
                .mission_configure(mission, gate.as_deref(), &accept, budget.as_ref())
                .map_err(err)?;
            print(&json!({ "mission": mission, "gate": gate, "accept": accept }));
        }
        "resume" => {
            let mission = id(&flags, "id")?;
            store.mission_resume(mission).map_err(err)?;
            print(&json!({ "mission": mission, "resumed": true }));
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
            let task = store
                .task_assign(
                    id(&flags, "mission")?,
                    &text(&flags, "kind")?,
                    &flags.get("worker").unwrap_or_default(),
                    &text(&flags, "brief")?,
                )
                .map_err(err)?;
            print(&json!({ "task": task }));
        }
        "accept" => {
            let task = id(&flags, "id")?;
            store.task_review(task, true, "").map_err(err)?;
            print(&json!({ "task": task, "accepted": true }));
        }
        "reject" => {
            let task = id(&flags, "id")?;
            store
                .task_review(task, false, &text(&flags, "note")?)
                .map_err(err)?;
            print(&json!({ "task": task, "sent_back": true }));
        }
        "cancel" => {
            let task = id(&flags, "id")?;
            store.task_cancel(task).map_err(err)?;
            print(&json!({ "task": task, "cancelled": true }));
        }
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
