//! `GET /api/context/digest` (issue #91 P1a): the fleet in one small
//! answer, for a phone's first screen or a prompt's context, so neither
//! reads every board: shared among the pages that poll like the boards
//! (`answers::shared`, its `generated_at_ms` left out of its ETag).
//!
//! ```text
//! { "generated_at_ms": ...,
//!   "fleet": {"sessions": n, "live": n, "states": {"WORKING": n, ...}},
//!   "nodes": [{name, state, load1, mem_available, power, low_battery_drain,
//!              effective_drained, schedulable}],
//!   "missions": [{id, title, state, halted, open_tasks,
//!                 last_gate_failure: {task, verdict, reason, at_ms} | null}],
//!   "approvals": [{id, kind, session_id, label}],
//!   "for_owner": [{text, at_ms}],
//!   "digest_text": "...",        // Markdown, under DIGEST_TEXT_MAX bytes
//!   "warnings": [...] }
//! ```
//!
//! Every part is read through the reader login as its own board reads it
//! (`/api/overview`'s states, `/api/nodes` with the soft drain,
//! `/api/missions`' board and last gates, `/api/approvals`, the supervisor's
//! `for_owner` of `/api/attention`); a part that cannot be read is empty and
//! named in `warnings`, and the rest still answers. Texts are masked
//! (`spawn_core::redact`) and cut short: a gate's reason to
//! [`REASON_CHARS`], a note to [`NOTE_CHARS`].

use std::collections::BTreeMap;

use serde_json::{Value, json};

/// The most bytes `digest_text` holds (it says when it was cut).
pub(super) const DIGEST_TEXT_MAX: usize = 4000;
/// The most missions, approvals and notes listed.
const MISSIONS_MAX: usize = 8;
const APPROVALS_MAX: usize = 10;
const NOTES_MAX: usize = 6;
/// A gate failure's reason, and a note, cut to this many characters.
const REASON_CHARS: usize = 240;
const NOTE_CHARS: usize = 240;

fn clip(text: &str, chars: usize) -> String {
    let masked = spawn_core::redact::redact(text.trim());
    let mut out: String = masked.chars().take(chars).collect();
    if masked.chars().count() > chars {
        out.push('…');
    }
    out
}

/// One line of a text: its first non-blank line.
fn first_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
}

/// What the digest is made of, as the boards read them.
#[derive(Debug, Default)]
pub(super) struct Parts {
    /// `/api/overview`'s `sessions` (each with its `observation`).
    pub(super) sessions: Vec<Value>,
    /// The sessions with a live execution.
    pub(super) live: usize,
    /// The node list with the soft drain (`nodes::power::with_soft_drain`).
    pub(super) nodes: Value,
    /// The mission board, and the last gate of each task by id.
    pub(super) board: Value,
    pub(super) gates: Value,
    /// `/api/approvals`' items.
    pub(super) approvals: Vec<Value>,
    /// The supervisor's `for_owner` notes.
    pub(super) for_owner: Value,
    pub(super) warnings: Vec<String>,
}

/// A mission's last gate that did not accept, among its tasks.
fn last_failure(mission: &Value, gates: &Value) -> Value {
    mission["tasks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|task| {
            let id = task["id"].as_i64()?;
            let gate = &gates[id.to_string()];
            let verdict = gate["verdict"].as_str()?;
            (verdict != "accepted").then(|| {
                json!({
                    "task": id, "verdict": verdict,
                    "reason": gate["reason"].as_str().map(|reason| clip(reason, REASON_CHARS)),
                    "at_ms": gate["at_ms"],
                })
            })
        })
        .max_by_key(|failure| failure["at_ms"].as_i64().unwrap_or(0))
        .unwrap_or(Value::Null)
}

/// The digest of `parts` at `now` (module doc).
pub(super) fn compose(parts: &Parts, now: i64) -> Value {
    let mut states: BTreeMap<String, usize> = BTreeMap::new();
    for session in &parts.sessions {
        let state = session["observation"]["state"]
            .as_str()
            .unwrap_or("UNJUDGED")
            .to_string();
        *states.entry(state).or_default() += 1;
    }
    let fleet = json!({"sessions": parts.sessions.len(), "live": parts.live, "states": states});
    let nodes: Vec<Value> = parts
        .nodes
        .as_array()
        .into_iter()
        .flatten()
        .map(|node| {
            json!({
                "name": node["name"], "state": node["state"],
                "load1": node["heartbeat"]["load1"],
                "mem_available": node["heartbeat"]["mem_available"],
                "power": node["power"],
                "low_battery_drain": node["low_battery_drain"],
                "effective_drained": node["effective_drained"],
                "schedulable": node["schedulable"],
            })
        })
        .collect();
    let missions: Vec<Value> = parts.board["missions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|mission| matches!(mission["state"].as_str(), Some("open" | "active")))
        .take(MISSIONS_MAX)
        .map(|mission| {
            let open_tasks = mission["tasks"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|task| {
                    !matches!(
                        task["state"].as_str(),
                        Some("accepted" | "landed" | "cancelled")
                    )
                })
                .count();
            json!({
                "id": mission["id"],
                "title": clip(first_line(mission["text"].as_str().unwrap_or("")), 120),
                "state": mission["state"],
                "halted": mission["halt"]["reason"].as_str().map(|reason| clip(reason, REASON_CHARS)),
                "open_tasks": open_tasks,
                "last_gate_failure": last_failure(mission, &parts.gates),
            })
        })
        .collect();
    let approvals: Vec<Value> = parts
        .approvals
        .iter()
        .take(APPROVALS_MAX)
        .map(|item| {
            json!({
                "id": item["id"], "kind": item["kind"], "session_id": item["session_id"],
                "label": clip(item["label"].as_str().unwrap_or(""), 160),
            })
        })
        .collect();
    let for_owner: Vec<Value> = parts
        .for_owner
        .as_array()
        .into_iter()
        .flatten()
        .filter(|note| note["settled"] != true)
        .take(NOTES_MAX)
        .map(|note| {
            json!({"text": clip(note["text"].as_str().unwrap_or(""), NOTE_CHARS),
                   "at_ms": note["at_ms"]})
        })
        .collect();
    let text = digest_text(
        &fleet,
        &nodes,
        &missions,
        &approvals,
        &for_owner,
        parts.approvals.len(),
    );
    json!({
        "generated_at_ms": now,
        "fleet": fleet,
        "nodes": nodes,
        "missions": missions,
        "approvals": approvals,
        "for_owner": for_owner,
        "digest_text": text,
        "warnings": parts.warnings,
    })
}

/// The Markdown block: a heading and a few bullets per part, under
/// [`DIGEST_TEXT_MAX`] bytes, whole lines only, with a last line saying it
/// was cut.
fn digest_text(
    fleet: &Value,
    nodes: &[Value],
    missions: &[Value],
    approvals: &[Value],
    for_owner: &[Value],
    approvals_total: usize,
) -> String {
    let mut lines = vec!["## Spawn digest".to_string()];
    let states: Vec<String> = fleet["states"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(state, count)| format!("{state} {count}"))
        .collect();
    lines.push(format!(
        "- Fleet: {} sessions, {} live{}",
        fleet["sessions"],
        fleet["live"],
        if states.is_empty() {
            String::new()
        } else {
            format!(" ({})", states.join(", "))
        }
    ));
    if !nodes.is_empty() {
        lines.push("### Nodes".to_string());
        for node in nodes {
            let mut words = vec![format!(
                "- {} {}",
                node["name"].as_str().unwrap_or("?"),
                node["state"].as_str().unwrap_or("?")
            )];
            if let Some(load) = node["load1"].as_f64() {
                words.push(format!("load {load:.2}"));
            }
            if let Some(power) = node["power"].as_object() {
                let on_ac = power.get("on_ac").and_then(Value::as_bool) == Some(true);
                let pct = power
                    .get("battery_pct")
                    .and_then(Value::as_u64)
                    .map_or_else(String::new, |pct| format!(" {pct}%"));
                words.push(format!("{}{pct}", if on_ac { "AC" } else { "battery" }));
            }
            if node["low_battery_drain"] == true {
                words.push("LOW-BATTERY soft-drain".to_string());
            } else if node["effective_drained"] == true {
                words.push("drained".to_string());
            }
            lines.push(words.join(", "));
        }
    }
    if !missions.is_empty() {
        lines.push("### Missions".to_string());
        for mission in missions {
            let mut line = format!(
                "- #{} {} ({}, {} open tasks)",
                mission["id"],
                mission["title"].as_str().unwrap_or(""),
                mission["state"].as_str().unwrap_or("?"),
                mission["open_tasks"]
            );
            if let Some(halted) = mission["halted"].as_str() {
                line = format!("{line} HALTED: {halted}");
            }
            lines.push(line);
            let failure = &mission["last_gate_failure"];
            if failure.is_object() {
                lines.push(format!(
                    "  - last gate failure: task {} {}{}",
                    failure["task"],
                    failure["verdict"].as_str().unwrap_or("?"),
                    failure["reason"]
                        .as_str()
                        .map_or_else(String::new, |reason| format!(": {reason}"))
                ));
            }
        }
    }
    lines.push(format!("### Pending approvals ({approvals_total})"));
    for item in approvals {
        lines.push(format!(
            "- [{}] {}: {}",
            item["kind"].as_str().unwrap_or("?"),
            item["session_id"].as_str().unwrap_or("mission"),
            item["label"].as_str().unwrap_or("")
        ));
    }
    if !for_owner.is_empty() {
        lines.push("### For the owner".to_string());
        for note in for_owner {
            lines.push(format!("- {}", note["text"].as_str().unwrap_or("")));
        }
    }
    let cut_note = "- (cut short: see /api/context/digest)";
    let mut out = String::new();
    for line in lines {
        let line: String = line.replace(['\r', '\n'], " ");
        if out.len() + line.len() + 1 + cut_note.len() + 1 > DIGEST_TEXT_MAX {
            out.push_str(cut_note);
            out.push('\n');
            return out;
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// `GET /api/context/digest`: the parts read, then [`compose`]d.
pub(super) fn digest() -> Result<Value, String> {
    let store = super::open_store()?;
    let mut parts = Parts::default();
    match super::overview() {
        Ok(overview) => {
            parts.sessions = overview["sessions"].as_array().cloned().unwrap_or_default();
        }
        Err(error) => parts
            .warnings
            .push(format!("digest: the sessions: {error}")),
    }
    match store.fleet_health() {
        Ok(health) => {
            parts.live = health["live"]
                .as_u64()
                .and_then(|live| usize::try_from(live).ok())
                .unwrap_or(0);
        }
        Err(error) => parts.warnings.push(format!("digest: the fleet: {error}")),
    }
    match store.nodes() {
        Ok(mut nodes) => {
            crate::nodes::power::with_soft_drain(
                &mut nodes,
                crate::nodes::power::low_battery_from_env(),
            );
            parts.nodes = nodes;
        }
        Err(error) => parts.warnings.push(format!("digest: the nodes: {error}")),
    }
    match store.mission_board(20) {
        Ok(board) => parts.board = board,
        Err(error) => parts
            .warnings
            .push(format!("digest: the missions: {error}")),
    }
    match store.last_gates() {
        Ok(gates) => parts.gates = gates,
        Err(error) => parts.warnings.push(format!("digest: the gates: {error}")),
    }
    drop(store);
    match super::approvals::approvals() {
        Ok(read) => parts.approvals = read["items"].as_array().cloned().unwrap_or_default(),
        Err(error) => parts
            .warnings
            .push(format!("digest: the approvals: {error}")),
    }
    match super::attention::attention() {
        Ok(view) => parts.for_owner = view["supervisor"]["for_owner"].clone(),
        Err(error) => parts
            .warnings
            .push(format!("digest: for the owner: {error}")),
    }
    Ok(compose(&parts, super::now_ms()))
}

#[cfg(test)]
mod tests;
