//! The supervisor digest: pending escalations coalesced into one message,
//! typed into the supervisor's pane while it is idle. What the supervisor
//! was already told is read back from the tags delivered digests filed on
//! their commands, never from their text: that quotes workers' reports.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use serde_json::Value;
use spawn_core::command::CommandAction;
use spawn_core::observation::SemanticState;

use super::{Escalation, Facts, Order, Why};
use crate::supervise::{SUPERVISOR_AGENT, SUPERVISOR_SESSION};

/// Lines per digest (each line one session and reason); the rest wait
/// for the next one.
const DIGEST_ITEMS: usize = 8;

/// Escalations the supervisor already received: the tags of the digests
/// that reached its pane.
pub(super) fn told(supervisor: Option<&Facts>) -> BTreeSet<String> {
    supervisor
        .map(|facts| facts.told.clone())
        .unwrap_or_default()
}

/// Whether the supervisor can take a digest now: live, nothing in flight,
/// and idle on its latest fact. Idle is the turn stopped at its prompt
/// (a fact the rules settle), whatever Jev made of it, or a judgement of
/// WAITING or COMPLETED. A supervisor whose turn ended on a question for
/// the human is judged BLOCKED but sits at its prompt all the same: if
/// that verdict held digests back, one question would silence every
/// escalation until a human answered it.
fn supervisor_idle(supervisor: &Facts) -> bool {
    supervisor.turn_stopped_idle()
        || (supervisor.live_and_free()
            && supervisor.current_observation().is_some_and(|observation| {
                matches!(
                    observation.state,
                    SemanticState::Waiting | SemanticState::Completed
                )
            }))
}

/// What the supervisor is asked to do about an item.
fn heading(why: Why) -> &'static str {
    match why {
        Why::Attention => "attention",
        Why::NudgeFailed => "nudge did not help",
        Why::ResumeFailed => "resume failed",
        Why::Completed => "completed, review it",
        Why::Idle => "idle, give it the next step or review it",
        Why::Mission => "new mission, plan it and assign tasks",
        Why::Report => "report, review it (name its revision)",
        Why::Approved => "resident approved",
        Why::Human => "human message",
        Why::DeliveryFailed => "delivery failed",
        Why::StopFailed => "stop failed",
        Why::Disk => "disk space on this host",
        Why::GateRoom => "no room for the gate on this host",
        Why::GateHostFault => "host faults at the gate (the host's, not the work's)",
        Why::Stalled => "stalled",
    }
}

/// Escalations about one session for one reason, told as one line: a
/// worker's repeated news of one kind (a human's notes that could not be
/// delivered, retried every few seconds) is one thing to act on, and as
/// separate lines it would fill every digest and starve the rest.
struct Line<'a> {
    first: &'a Escalation,
    latest: &'a Escalation,
    count: usize,
}

/// The pending escalations grouped by session and reason, in the order
/// their first item is pending. Items about the host (session `host`: a
/// disk place, a mission's gate room, a task's host faults) are each about
/// something else, and keep a line each.
fn lines(pending: &[Escalation]) -> Vec<(Line<'_>, Vec<String>)> {
    let mut lines: Vec<(Line<'_>, Vec<String>)> = Vec::new();
    for item in pending {
        match lines.iter_mut().find(|(line, _)| {
            item.session_id != HOST_SESSION
                && line.first.session_id == item.session_id
                && line.first.why == item.why
        }) {
            Some((line, tags)) => {
                line.latest = item;
                line.count += 1;
                tags.push(item.tag());
            }
            None => lines.push((
                Line {
                    first: item,
                    latest: item,
                    count: 1,
                },
                vec![item.tag()],
            )),
        }
    }
    lines
}

/// The session of escalations about the host rather than an agent.
const HOST_SESSION: &str = "host";

/// Crash lines and failures an escalation quotes at most.
const HOST_FAULT_QUOTED: usize = 3;

/// The digest items for `records`, the integrator's `host_fault` gate
/// records that carry `escalate` (the mission's second host fault in a row)
/// and that no delivered digest told yet
/// (`PostgresStore::gate_host_fault_escalations`). Each names the task, the
/// mission, the count, the host's evidence (the crash lines from the test
/// database's log, or the first failure's test and message) and when the
/// task is judged again (the record's `due_ms`). No time window: a record
/// stays an item until a digest carrying its tag
/// (`host gate-host-fault-<event id>`) reaches the supervisor, so it is told
/// once.
pub(crate) fn host_fault_escalations(records: &[Value]) -> Vec<Escalation> {
    records
        .iter()
        .filter_map(|record| {
            let id = record["event_id"].as_i64()?;
            let task = record["id"].as_i64()?;
            let mission = record["mission"]
                .as_i64()
                .map_or_else(|| "its mission".to_string(), |id| format!("mission {id}"));
            let faults = record["host_faults"].as_u64().unwrap_or(2);
            let quoted = |values: &Value, line: &dyn Fn(&Value) -> Option<String>| {
                values
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(line)
                    .take(HOST_FAULT_QUOTED)
                    .collect::<Vec<String>>()
                    .join(" / ")
            };
            let crash = quoted(&record["crash"], &|line| {
                line.as_str().map(|line| one_line(line, 300))
            });
            let evidence = if crash.is_empty() {
                let failures = record["failures"].as_array().map_or(0, Vec::len);
                format!(
                    "every check-tests.sh failure ({failures}) was a connection error to the \
                     test database, first {}",
                    quoted(&record["failures"], &|failure| {
                        Some(format!(
                            "{}: {}",
                            failure["test"].as_str()?,
                            one_line(failure["message"].as_str().unwrap_or_default(), 300)
                        ))
                    })
                )
            } else {
                format!("the test database's log shows a crash during the gate: {crash}")
            };
            let due = record["due_ms"].as_i64().map_or_else(
                || "after its backoff".to_string(),
                |due| format!("at {}", crate::scorecard::time::format_rfc3339(due)),
            );
            Some(Escalation {
                session_id: "host".to_string(),
                id: format!("gate-host-fault-{id}"),
                why: Why::GateHostFault,
                line: format!(
                    "task #{task} of {mission}: {faults} gate failures in a row were the \
                     host's, not the work's: {evidence}. It is judged again {due}; nothing is \
                     counted and nothing halts. Check the test database's host (its memory, \
                     /dev/shm, its log) or tell the owner."
                ),
            })
        })
        .collect()
}

/// `text` on one line (line breaks become spaces), cut to `limit`
/// characters.
fn one_line(text: &str, limit: usize) -> String {
    text.trim()
        .chars()
        .map(|char| {
            if char == '\n' || char == '\r' {
                ' '
            } else {
                char
            }
        })
        .take(limit)
        .collect()
}

/// The digest typed into the supervisor's pane.
fn text(lines: &[Line<'_>], spawn: &str) -> String {
    let mut text = format!(
        "Spawn dispatcher: {} update{} for you. Check a worker with `{spawn} tail --session \
<session>` before acting; missions and tasks with `{spawn} mission list`.\n",
        lines.len(),
        if lines.len() == 1 { "" } else { "s" },
    );
    for line in lines {
        let item = line.latest;
        let repeated = if line.count > 1 {
            format!(" ({} of them, the latest)", line.count)
        } else {
            String::new()
        };
        let _ = writeln!(
            text,
            "- {} — {}{repeated}: {}",
            item.session_id,
            heading(item.why),
            item.line
        );
    }
    text
}

/// The digest order for the pending escalations, if the supervisor can
/// take one now.
pub(super) fn order(supervisor: &Facts, pending: &[Escalation], spawn: &str) -> Option<Order> {
    if pending.is_empty() || !supervisor_idle(supervisor) {
        return None;
    }
    let (lines, tags): (Vec<Line<'_>>, Vec<Vec<String>>) =
        lines(pending).into_iter().take(DIGEST_ITEMS).unzip();
    let tags: Vec<String> = tags.into_iter().flatten().collect();
    Some(Order {
        agent_id: SUPERVISOR_AGENT.to_string(),
        session_id: SUPERVISOR_SESSION.to_string(),
        // A new supervisor judgement allows one retry of an undelivered digest.
        key: format!(
            "dispatch-digest-{}-{:016x}",
            supervisor.observation_event_id.unwrap_or(0),
            crate::sources::fnv(&tags.join("\n"))
        ),
        action: CommandAction::Input {
            text: text(&lines, spawn),
            tags,
        },
        task: None,
    })
}
