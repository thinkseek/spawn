//! `status`, `fleet`, and `tail`: read-only views of the fleet log and tasks.

use std::fmt::Write as _;

use serde_json::{Value, json};
use spawn_core::id::SessionId;

use crate::Flags;
use crate::config::open_store;

/// The usage of `spawn status`, the only copy (main's help reads it).
pub(crate) const STATUS_USAGE: &str = "usage: spawn status [--json]\n      \
                                       (single-command overview of active fleet sessions, open tasks, and\n       \
                                       uncleared alerts; human table by default, stable JSON with --json)";

/// The usage of `spawn fleet`, the only copy (main's help reads it).
pub(crate) const FLEET_USAGE: &str = "usage: spawn fleet\n      \
                                      (every session the login may read, with its judgement, as JSON)";

/// The usage of `spawn tail`, the only copy (main's help reads it).
pub(crate) const TAIL_USAGE: &str = "usage: spawn tail --session <session id> [--events N]\n      \
                                     (one session's latest judgement, newest events, judgements and\n       \
                                     commands, as JSON; --events: 1 to 200, default 20)";

const MAX_TASK_TITLE_CHARS: usize = 120;
const MAX_ALERT_TEXT_CHARS: usize = 200;

/// Single-command overview combining live sessions, open tasks, and uncleared alerts.
pub(crate) fn status(flags: &Flags) -> Result<i32, String> {
    if let Some(unknown) = flags.names().next() {
        return Err(format!("unknown flag --{unknown}\n{STATUS_USAGE}"));
    }
    let json_output = flags.switch("json");
    let store = open_store()?;
    let fleet = store.fleet_json().map_err(|error| error.to_string())?;
    let missions = store.mission_board(50).map_err(|error| error.to_string())?;
    let alerts = store.alerts(i64::MAX).map_err(|error| error.to_string())?;
    let generated_at_ms = store.now_ms().map_err(|error| error.to_string())?;

    let report = build_status_report(&fleet, &missions, &alerts, generated_at_ms);
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_else(|_| "{}".to_string())
        );
    } else {
        print!("{}", format_status_table(&report));
    }
    Ok(0)
}

/// The fleet as JSON: one object per session the login may read.
pub(crate) fn fleet() -> Result<i32, String> {
    let store = open_store()?;
    let fleet = store.fleet_json().map_err(|error| error.to_string())?;
    println!(
        "{}",
        serde_json::to_string_pretty(&fleet).unwrap_or_else(|_| "[]".to_string())
    );
    Ok(0)
}

/// One session's latest judgement and newest events as JSON.
pub(crate) fn tail(flags: &Flags) -> Result<i32, String> {
    let store = open_store()?;
    let session = SessionId::new(flags.required("session")?).map_err(|error| error.to_string())?;
    let events = flags
        .get("events")
        .and_then(|text| text.parse::<i64>().ok())
        .unwrap_or(20)
        .clamp(1, 200);
    let tail = store
        .tail_json(&session, events)
        .map_err(|error| error.to_string())?;
    println!(
        "{}",
        serde_json::to_string_pretty(&tail).unwrap_or_else(|_| "{}".to_string())
    );
    Ok(0)
}

fn is_open_task_state(state: &str, mission_has_gate: bool) -> bool {
    matches!(
        state,
        "assigned" | "delivered" | "reported" | "revise" | "landing"
    ) || (state == "accepted" && mission_has_gate)
}

fn collapse_single_line(raw: &str, max_chars: usize) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let count = collapsed.chars().count();
    if count <= max_chars {
        return collapsed;
    }
    let keep = max_chars.saturating_sub(3);
    let mut clipped: String = collapsed.chars().take(keep).collect();
    clipped.push_str("...");
    clipped
}

fn extract_task_title(brief: Option<&str>) -> String {
    let first_line = brief
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("-");
    collapse_single_line(first_line, MAX_TASK_TITLE_CHARS)
}

fn collect_live_sessions(fleet: &Value) -> (u64, u64, Vec<Value>) {
    let rows = fleet.as_array().map_or(&[][..], Vec::as_slice);
    let sessions_total = u64::try_from(rows.len()).unwrap_or(u64::MAX);
    let mut attention_count = 0_u64;
    let mut live_rows: Vec<(String, String, Value)> = Vec::new();

    for row in rows {
        if row["live"].as_bool() != Some(true) {
            continue;
        }
        let attention = row["attention"].as_bool().unwrap_or(false);
        if attention {
            attention_count = attention_count.saturating_add(1);
        }
        let agent_id = row["agent_id"].as_str().unwrap_or("").to_string();
        let session_id = row["session_id"].as_str().unwrap_or("").to_string();
        let state = row["state"].as_str().unwrap_or("UNKNOWN");
        let projected = json!({
            "session_id": session_id,
            "agent_id": agent_id,
            "state": state,
            "live": true,
            "engine": row.get("engine").and_then(Value::as_str),
            "attention": attention,
            "last_event_at_ms": row.get("last_event_at_ms").and_then(Value::as_i64),
        });
        live_rows.push((agent_id, session_id, projected));
    }

    live_rows.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    let sessions = live_rows.into_iter().map(|(_, _, item)| item).collect();
    (sessions_total, attention_count, sessions)
}

fn collect_open_tasks(missions: &Value) -> Vec<Value> {
    let board_missions = missions["missions"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    let mut items: Vec<(i64, i64, Value)> = Vec::new();

    for mission in board_missions {
        let Some(mission_id) = mission["id"].as_i64() else {
            continue;
        };
        let mission_has_gate = !mission["gate"].is_null();
        let tasks = mission["tasks"].as_array().map_or(&[][..], Vec::as_slice);
        for task in tasks {
            let Some(id) = task["id"].as_i64() else {
                continue;
            };
            let state = task["state"].as_str().unwrap_or("");
            if !is_open_task_state(state, mission_has_gate) {
                continue;
            }
            let title = extract_task_title(task["brief"].as_str());
            let projected = json!({
                "id": id,
                "mission_id": mission_id,
                "state": state,
                "kind": task.get("kind").and_then(Value::as_str),
                "agent_id": task.get("agent_id").and_then(Value::as_str),
                "session_id": task.get("session_id").and_then(Value::as_str),
                "difficulty": task.get("difficulty").and_then(Value::as_str),
                "preset_id": task.get("preset_id").and_then(Value::as_str),
                "title": title,
                "updated_at_ms": task.get("updated_at_ms").and_then(Value::as_i64),
            });
            items.push((mission_id, id, projected));
        }
    }

    items.sort_by_key(|(mission_id, id, _)| (*mission_id, *id));
    items.into_iter().map(|(_, _, item)| item).collect()
}

fn collect_open_alerts(alerts: &Value) -> Vec<Value> {
    let rows = alerts.as_array().map_or(&[][..], Vec::as_slice);
    let mut items: Vec<(i64, String, Value)> = Vec::new();

    for alert in rows {
        if alert["state"].as_str() != Some("open") {
            continue;
        }
        let key = alert["key"].as_str().unwrap_or("").to_string();
        let kind = alert["kind"].as_str().unwrap_or("unknown");
        let subject = alert["subject"].as_str().unwrap_or("-");
        let fired_at_ms = alert.get("fired_at_ms").and_then(Value::as_i64);
        let text =
            collapse_single_line(alert["text"].as_str().unwrap_or("-"), MAX_ALERT_TEXT_CHARS);
        let projected = json!({
            "key": key,
            "kind": kind,
            "subject": subject,
            "state": "open",
            "fired_at_ms": fired_at_ms,
            "text": text,
        });
        items.push((fired_at_ms.unwrap_or(0), key, projected));
    }

    items.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    items.into_iter().map(|(_, _, item)| item).collect()
}

/// Build the secret-free, deterministic `spawn status --json` payload from raw store reads.
pub(crate) fn build_status_report(
    fleet: &Value,
    missions: &Value,
    alerts: &Value,
    generated_at_ms: i64,
) -> Value {
    let (sessions_total, sessions_attention, sessions) = collect_live_sessions(fleet);
    let tasks = collect_open_tasks(missions);
    let open_alerts = collect_open_alerts(alerts);

    let sessions_live = u64::try_from(sessions.len()).unwrap_or(u64::MAX);
    let open_tasks = u64::try_from(tasks.len()).unwrap_or(u64::MAX);
    let open_alert_count = u64::try_from(open_alerts.len()).unwrap_or(u64::MAX);

    json!({
        "schema_version": 1,
        "generated_at_ms": generated_at_ms,
        "summary": {
            "sessions_total": sessions_total,
            "sessions_live": sessions_live,
            "sessions_attention": sessions_attention,
            "open_tasks": open_tasks,
            "open_alerts": open_alert_count,
        },
        "sessions": sessions,
        "tasks": tasks,
        "alerts": open_alerts,
    })
}

/// Render the human-readable overview table from [`build_status_report`].
pub(crate) fn format_status_table(report: &Value) -> String {
    let summary = &report["summary"];
    let sessions_live = summary["sessions_live"].as_u64().unwrap_or(0);
    let sessions_total = summary["sessions_total"].as_u64().unwrap_or(0);
    let sessions_att = summary["sessions_attention"].as_u64().unwrap_or(0);
    let open_tasks = summary["open_tasks"].as_u64().unwrap_or(0);
    let open_alerts = summary["open_alerts"].as_u64().unwrap_or(0);

    let mut out = String::new();
    let _ = writeln!(
        out,
        "SPAWN STATUS  sessions: {sessions_live} live ({sessions_total} total, {sessions_att} attention)  tasks: {open_tasks} open  alerts: {open_alerts} open"
    );

    let _ = writeln!(out, "\nSESSIONS ({sessions_live} live)");
    let sessions = report["sessions"].as_array().map_or(&[][..], Vec::as_slice);
    if sessions.is_empty() {
        out.push_str("  (none)\n");
    } else {
        let _ = writeln!(
            out,
            "  {:<24} {:<28} {:<10} {:<8} ATTN",
            "AGENT", "SESSION", "STATE", "ENGINE"
        );
        for s in sessions {
            let agent = s["agent_id"].as_str().unwrap_or("-");
            let sid = s["session_id"].as_str().unwrap_or("-");
            let state = s["state"].as_str().unwrap_or("UNKNOWN");
            let engine = s["engine"].as_str().unwrap_or("-");
            let attn = if s["attention"].as_bool() == Some(true) {
                "yes"
            } else {
                "no"
            };
            let _ = writeln!(
                out,
                "  {agent:<24} {sid:<28} {state:<10} {engine:<8} {attn}"
            );
        }
    }

    let _ = writeln!(out, "\nOPEN TASKS ({open_tasks})");
    let tasks = report["tasks"].as_array().map_or(&[][..], Vec::as_slice);
    if tasks.is_empty() {
        out.push_str("  (none)\n");
    } else {
        let _ = writeln!(
            out,
            "  {:<6} {:<5} {:<10} {:<4} {:<20} TITLE",
            "TASK", "MIS", "STATE", "DIFF", "WORKER"
        );
        for t in tasks {
            let tid = t["id"].as_i64().unwrap_or(0);
            let mid = t["mission_id"].as_i64().unwrap_or(0);
            let state = t["state"].as_str().unwrap_or("-");
            let diff = t["difficulty"].as_str().unwrap_or("-");
            let worker = t["agent_id"].as_str().unwrap_or("-");
            let title = t["title"].as_str().unwrap_or("-");
            let _ = writeln!(
                out,
                "  {:<6} {:<5} {:<10} {:<4} {:<20} {}",
                format!("T{tid}"),
                format!("M{mid}"),
                state,
                diff,
                worker,
                title
            );
        }
    }

    let _ = writeln!(out, "\nOPEN ALERTS ({open_alerts})");
    let alerts = report["alerts"].as_array().map_or(&[][..], Vec::as_slice);
    if alerts.is_empty() {
        out.push_str("  (none)\n");
    } else {
        let _ = writeln!(out, "  {:<22} {:<20} TEXT", "KIND", "SUBJECT");
        for a in alerts {
            let kind = a["kind"].as_str().unwrap_or("-");
            let subject = a["subject"].as_str().unwrap_or("-");
            let text = a["text"].as_str().unwrap_or("-");
            let _ = writeln!(out, "  {kind:<22} {subject:<20} {text}");
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{Flags, build_status_report, format_status_table, status};

    #[test]
    fn status_rejects_unknown_flag_before_opening_store() {
        let flags =
            Flags::parse(&["--bogus".to_string(), "1".to_string()]).expect("valid flag syntax");
        let err = status(&flags).expect_err("unknown flag must fail");
        assert!(err.contains("--bogus"), "{err}");
        assert!(err.contains("spawn status [--json]"), "{err}");
    }

    #[test]
    fn empty_status_report_has_stable_schema_and_renders_none_sections() {
        let report = build_status_report(&json!([]), &json!({"missions": []}), &json!([]), 42);
        assert_eq!(report["schema_version"], 1);
        assert_eq!(report["generated_at_ms"], 42);
        assert_eq!(
            report["summary"],
            json!({
                "sessions_total": 0,
                "sessions_live": 0,
                "sessions_attention": 0,
                "open_tasks": 0,
                "open_alerts": 0,
            })
        );
        assert_eq!(report["sessions"], json!([]));
        assert_eq!(report["tasks"], json!([]));
        assert_eq!(report["alerts"], json!([]));

        let rendered = format_status_table(&report);
        assert!(
            rendered.contains("0 live (0 total, 0 attention)"),
            "{rendered}"
        );
        assert!(
            rendered.contains("SESSIONS (0 live)\n  (none)"),
            "{rendered}"
        );
        assert!(rendered.contains("OPEN TASKS (0)\n  (none)"), "{rendered}");
        assert!(rendered.contains("OPEN ALERTS (0)\n  (none)"), "{rendered}");
    }

    fn sample_fleet() -> Value {
        json!([
            {
                "session_id": "session_stopped",
                "agent_id": "agent_worker_1",
                "state": "WAITING",
                "live": false,
                "engine": "claude",
                "attention": true,
                "last_event_at_ms": 10,
                "launch": {"secret_env": "must-not-appear"},
                "judgement": {"evidence": "must-not-appear"}
            },
            {
                "session_id": "session_sup_live",
                "agent_id": "agent_supervisor",
                "state": "WORKING",
                "live": true,
                "engine": "claude",
                "attention": false,
                "last_event_at_ms": 200,
                "launch": {"model": "claude-sonnet"}
            },
            {
                "session_id": "session_worker_b",
                "agent_id": "agent_worker_b",
                "state": "BLOCKED",
                "live": true,
                "engine": "agy",
                "attention": true,
                "last_event_at_ms": 300
            }
        ])
    }

    fn sample_missions() -> Value {
        json!({
            "missions": [
                {
                    "id": 2,
                    "gate": "/repo/scenarios",
                    "tasks": [
                        {
                            "id": 12,
                            "state": "delivered",
                            "kind": "ephemeral",
                            "agent_id": "agent_worker_b",
                            "session_id": "session_worker_b",
                            "difficulty": "l2",
                            "preset_id": "topdown-l2-neon",
                            "brief": "First line title\nSecond line details",
                            "updated_at_ms": 500
                        },
                        {
                            "id": 11,
                            "state": "accepted",
                            "kind": "ephemeral",
                            "brief": "Accepted awaiting gate",
                            "updated_at_ms": 400
                        },
                        {
                            "id": 10,
                            "state": "landed",
                            "brief": "Done task",
                            "updated_at_ms": 300
                        }
                    ]
                },
                {
                    "id": 1,
                    "gate": null,
                    "tasks": [
                        {
                            "id": 9,
                            "state": "accepted",
                            "brief": "Ungated accepted is terminal",
                            "updated_at_ms": 200
                        },
                        {
                            "id": 8,
                            "state": "cancelled",
                            "brief": "Cancelled task",
                            "updated_at_ms": 100
                        }
                    ]
                }
            ]
        })
    }

    fn sample_alerts() -> Value {
        json!([
            {
                "key": "k-open",
                "kind": "controller_behind",
                "subject": "mission #2",
                "state": "open",
                "fired_at_ms": 900,
                "text": "Controller behind\non main",
                "evidence": {"token": "must-not-appear"}
            },
            {
                "key": "k-cleared",
                "kind": "memory_low",
                "subject": "host",
                "state": "cleared",
                "fired_at_ms": 800,
                "text": "Old cleared alert"
            },
            {
                "key": "k-notice",
                "kind": "task_state",
                "subject": "task #12",
                "state": "notice",
                "fired_at_ms": 950,
                "text": "Notice is not open alert"
            }
        ])
    }

    #[test]
    fn status_report_filters_live_open_items_and_strips_sensitive_fields() {
        let fleet = sample_fleet();
        let missions = sample_missions();
        let alerts = sample_alerts();

        let report = build_status_report(&fleet, &missions, &alerts, 1_234_567);
        assert_eq!(report["summary"]["sessions_total"], 3);
        assert_eq!(report["summary"]["sessions_live"], 2);
        assert_eq!(report["summary"]["sessions_attention"], 1);
        assert_eq!(report["summary"]["open_tasks"], 2);
        assert_eq!(report["summary"]["open_alerts"], 1);

        let sessions = report["sessions"].as_array().expect("sessions array");
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0]["agent_id"], "agent_supervisor");
        assert_eq!(sessions[1]["agent_id"], "agent_worker_b");
        assert!(sessions[0].get("launch").is_none());
        assert!(sessions[0].get("judgement").is_none());

        let tasks = report["tasks"].as_array().expect("tasks array");
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0]["id"], 11);
        assert_eq!(tasks[1]["id"], 12);
        assert_eq!(tasks[1]["title"], "First line title");

        let open_alerts = report["alerts"].as_array().expect("alerts array");
        assert_eq!(open_alerts.len(), 1);
        assert_eq!(open_alerts[0]["key"], "k-open");
        assert_eq!(open_alerts[0]["text"], "Controller behind on main");
        assert!(open_alerts[0].get("evidence").is_none());

        let table = format_status_table(&report);
        assert!(table.contains("session_sup_live"), "{table}");
        assert!(table.contains("First line title"), "{table}");
        assert!(table.contains("controller_behind"), "{table}");
        assert!(!table.contains("must-not-appear"), "{table}");
    }
}
