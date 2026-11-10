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

/// The words every digest opens with: a prompt in the supervisor's
/// transcript that starts with them is a digest, not a person's answer.
pub(crate) const OPENING: &str = "Spawn dispatcher:";

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
pub(super) fn supervisor_idle(supervisor: &Facts) -> bool {
    supervisor.turn_stopped_idle()
        || (supervisor.live_and_free()
            && supervisor.current_observation().is_some_and(|observation| {
                matches!(
                    observation.state,
                    SemanticState::Waiting | SemanticState::Completed
                )
            }))
}

/// What the supervisor is asked to do about an item: the digest's heading,
/// and the attention view's (`GET /api/attention`).
pub(crate) fn heading(why: Why) -> &'static str {
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
        Why::DiskRecovered => "disk space back on this host",
        Why::GateRoom => "no room for the gate on this host",
        Why::GateHostFault => "host faults at the gate (the host's, not the work's)",
        Why::GateBaseDisagrees => "the base disagreed with itself at the gate (the host's)",
        Why::MainRed => "a source's main is red on a check (its gates wait)",
        Why::MainGreen => "a source's main is green again",
        Why::SourceAway => "a mission's source is missing on this host (its branches wait)",
        Why::SourceBack => "a mission's source is back on this host",
        Why::ShadowDisagrees => {
            "a gate runner's shadow verdict disagreed with the primary's (it decided nothing)"
        }
        Why::Stalled => "stalled",
        Why::Charter => "new charter not typed",
        Why::BackgroundJob => "background job past its bound",
        Why::HoldExpired => "a source hold expired",
        Why::ObjectiveYielded => "an objective ran out of attempts and yielded its slot",
        Why::ObjectiveSolved => "an objective closed: solved on the base (not a rejection)",
        Why::ObjectiveOrder => "a backlog names an order no task can meet",
        Why::OverBudget => "a task went over its budget twice",
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
/// disk place, a mission's gate room, a task's host faults), expired holds
/// (each another hold, with its own waiting tasks) and an objective's
/// close (each its own objective, task and base) are each about something
/// else, and keep a line each.
fn lines(pending: &[Escalation]) -> Vec<(Line<'_>, Vec<String>)> {
    let mut lines: Vec<(Line<'_>, Vec<String>)> = Vec::new();
    for item in pending {
        match lines.iter_mut().find(|(line, _)| {
            item.session_id != HOST_SESSION
                && item.why != Why::HoldExpired
                && item.why != Why::ObjectiveYielded
                && item.why != Why::ObjectiveSolved
                && item.why != Why::ObjectiveOrder
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
            // Only a record that carries the escalation is one: a base that
            // disagreed with itself never does (objective 033).
            if record["escalate"].as_bool() != Some(true) {
                return None;
            }
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
            let fact = &record["fact"];
            let evidence = if record["evidence"] == "cannot_judge" {
                // The host cannot judge the objective at all: the scenario's
                // own cause, as the record quotes it from the gate's log
                // (integrate.rs `cannot_judge_cause`), or none — never one
                // scenario's cause for another (029's systemd user manager
                // was said of 073, which needed a PostgreSQL server).
                let scenario = record["scenario"].as_str().unwrap_or("its scenario");
                match record["cause"].as_str() {
                    Some(cause) => format!(
                        "this host cannot judge the objective here ({scenario} says: {})",
                        one_line(cause, 300)
                    ),
                    None => format!(
                        "this host cannot judge the objective here ({scenario}; the gate's log \
                         gives no cause)"
                    ),
                }
            } else if let Some(said) = never_started(&record["evidence"], fact) {
                said
            } else if record["evidence"] == "gate" {
                let scenario = fact["scenario"].as_str().unwrap_or("a scenario");
                let runs = one_line(fact["runs"].as_str().unwrap_or_default(), 200);
                if fact["on"] == "base" {
                    format!(
                        "the gate found {scenario} exiting {runs} on the base before any \
                         candidate code ran (a harness error, or runs that disagree)"
                    )
                } else {
                    format!(
                        "the gate found {scenario} failing on the candidate and on the base \
                         ({runs})"
                    )
                }
            } else if crash.is_empty() {
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
            // The head's run alone on the base it was built on (integrate.rs
            // `on_the_head`): its fault is that base's and the work's, never
            // the merge's on main, and the item says so.
            let evidence = match record["head_base"].as_str() {
                Some(head_base) => format!(
                    "judging the work alone against the base it was built on ({}), after main \
                     rejected the merge, {evidence}",
                    head_base.get(..12).unwrap_or(head_base)
                ),
                None => evidence,
            };
            let due = record["due_ms"].as_i64().map_or_else(
                || "after its backoff".to_string(),
                |due| format!("at {}", crate::scorecard::time::format_rfc3339(due)),
            );
            let check = host_fault_check(&record["evidence"]);
            Some(Escalation {
                session_id: "host".to_string(),
                id: format!("gate-host-fault-{id}"),
                why: Why::GateHostFault,
                line: format!(
                    "task #{task} of {mission}: {faults} gate failures in a row were the \
                     host's, not the work's: {evidence}. It is judged again {due}; nothing is \
                     counted and nothing halts. {check}"
                ),
                since_ms: record["at_ms"].as_i64(),
            })
        })
        .collect()
}

/// What a gate host fault that never started something says: its sandbox
/// (`sandbox_never_started`) or the gate's own test server
/// (`test_server_never_started`), with the gate's reason; `None` for
/// another evidence.
fn never_started(evidence: &Value, fact: &Value) -> Option<String> {
    let said = one_line(fact["reason"].as_str().unwrap_or_default(), 300);
    if evidence == crate::gate::SANDBOX_NEVER_STARTED {
        let scenario = fact["scenario"].as_str().unwrap_or("a scenario");
        Some(format!(
            "the gate's sandbox never started {scenario} ({said})"
        ))
    } else if evidence == crate::gate::TEST_SERVER_NEVER_STARTED {
        Some(format!("the gate's own test server never started ({said})"))
    } else {
        None
    }
}

/// What a gate host fault's item asks the supervisor to do, by its
/// evidence.
fn host_fault_check(evidence: &Value) -> &'static str {
    if evidence == "cannot_judge" {
        // What to do follows from the scenario's own cause (quoted in the
        // item), not from 029's: give the gate what the scenario says it
        // lacks, or judge it outside the gate.
        "Give the gate what the scenario says it lacks (its environment, a server, a service), \
         or have the owner judge the objective outside the gate."
    } else if evidence == crate::gate::SANDBOX_NEVER_STARTED {
        "Check that bubblewrap starts on this host (user namespaces, the paths it binds and \
         masks) or tell the owner."
    } else if evidence == crate::gate::TEST_SERVER_NEVER_STARTED {
        "Check that the gate's test server starts on this host (its memory, disk, \
         SPAWN_GATE_PG_BIN or image, the server's FATAL line in the record) or tell the owner."
    } else if evidence == "gate" {
        "Check the host's load and that scenario's harness (its log in the gate output), or \
         tell the owner."
    } else {
        "Check the test database's host (its memory, /dev/shm, its log) or tell the owner."
    }
}

/// The digest items for `records`, the integrator's `host_fault` gate
/// records whose base disagreed with itself and that carry `alert` (the
/// mission's third such base in a row, objective 033), and that no delivered
/// digest told yet (`PostgresStore::gate_base_disagrees_alerts`). Each names
/// the task, the mission, the count in a row, the scenario, its exits, the
/// gate's setup time when it printed one, and when the task is judged again
/// (the record's `due_ms`). No time window: a record stays an item until a
/// digest carrying its tag (`host gate-base-disagrees-<event id>`) reaches
/// the supervisor, so it is told once. Before objective 030 exists this item
/// is how the alert reaches a person; 030 raises its `gate_base_disagrees`
/// notice from the same record.
pub(crate) fn base_disagrees_alerts(records: &[Value]) -> Vec<Escalation> {
    records
        .iter()
        .filter_map(|record| {
            let id = record["event_id"].as_i64()?;
            let task = record["id"].as_i64()?;
            // Only the record that makes the run three carries the alert;
            // the other ones of the run are no news.
            if record["alert"].as_bool() != Some(true) {
                return None;
            }
            let mission = record["mission"]
                .as_i64()
                .map_or_else(|| "its mission".to_string(), |id| format!("mission {id}"));
            let count = record["base_disagrees"].as_u64().unwrap_or(3);
            let scenario = record["scenario"].as_str().unwrap_or("a scenario");
            let setup = record["setup_s"]
                .as_u64()
                .map_or_else(String::new, |secs| format!(", setup {secs} s"));
            let due = record["due_ms"].as_i64().map_or_else(
                || "after its backoff".to_string(),
                |due| format!("at {}", crate::scorecard::time::format_rfc3339(due)),
            );
            Some(Escalation {
                session_id: HOST_SESSION.to_string(),
                id: format!("gate-base-disagrees-{id}"),
                why: Why::GateBaseDisagrees,
                line: format!(
                    "task #{task} of {mission}: the base disagreed with itself {count} in a row \
                     at the gate ({scenario} exited 1, then 0 on the base{setup}): no candidate \
                     code had run, so it is the host's (too loaded or too flaky to judge), not \
                     the work's. It is judged again {due}; nothing is counted and nothing halts. \
                     Check the host's load or tell the owner."
                ),
                since_ms: record["at_ms"].as_i64(),
            })
        })
        .collect()
}

/// The supervisor's news of a mission's source away from this host and
/// back (P12): one item per `source_away` alert record not told yet
/// (the store's `source_away_news`), tagged `host source-away-<event_id>` or
/// `host source-back-<event_id>`.
pub(crate) fn source_away_news(records: &[Value]) -> Vec<Escalation> {
    records
        .iter()
        .filter_map(|record| {
            let id = record["event_id"].as_i64()?;
            let subject = record["subject"].as_str().unwrap_or("a mission");
            let source = record["evidence"]["source"].as_str().unwrap_or("its source");
            match record["type"].as_str()? {
                "alert.fired" => Some(Escalation {
                    session_id: HOST_SESSION.to_string(),
                    id: format!("source-away-{id}"),
                    why: Why::SourceAway,
                    line: format!(
                        "{subject}'s source {source} is not a directory on this host: its \
                         agents' branches are not published to it until it is back. Check \
                         the mount or the path; Spawn retries on its own."
                    ),
                    since_ms: record["at_ms"].as_i64(),
                }),
                "alert.cleared" => {
                    let gone = record["evidence"]["reason"].as_str() == Some("no longer watched");
                    Some(Escalation {
                        session_id: HOST_SESSION.to_string(),
                        id: format!("source-back-{id}"),
                        why: Why::SourceBack,
                        line: if gone {
                            format!("{subject} no longer has {source} as an open source: its away alert is closed.")
                        } else {
                            format!("{subject}'s source {source} is back on this host: its branches are published again.")
                        },
                        since_ms: record["at_ms"].as_i64(),
                    })
                }
                _ => None,
            }
        })
        .collect()
}

/// The digest items for `records`, the red mains that started and ended
/// that no delivered digest told yet (`PostgresStore::main_red_news`): a
/// `source.main_red` with `alert` (the first of its red) is item
/// `main-red-<event id>` of session `host`, naming the source, its main, the
/// check, up to [`HOST_FAULT_QUOTED`] of the failing tests and what holds; a
/// `source.main_green` is `main-green-<event id>`, naming how long it was red.
/// Each is told once.
pub(crate) fn main_red_news(records: &[Value]) -> Vec<Escalation> {
    records
        .iter()
        .filter_map(|record| {
            let id = record["event_id"].as_i64()?;
            let source = record["source"].as_str()?;
            let main = record["main"].as_str().unwrap_or("its main");
            let main = main.get(..12).unwrap_or(main);
            let scenario = record["scenario"].as_str().unwrap_or("a check");
            if record["type"] == "source.main_green" {
                let red_for = match (record["at_ms"].as_i64(), record["since_ms"].as_i64()) {
                    (Some(at), Some(since)) if at >= since => {
                        format!(" after {} min", (at - since) / 60_000)
                    }
                    _ => String::new(),
                };
                return Some(Escalation {
                    session_id: HOST_SESSION.to_string(),
                    id: format!("main-green-{id}"),
                    why: Why::MainGreen,
                    line: format!(
                        "main of {source} passes {scenario} again at {main}{red_for}: it is \
                         green, and its gates run as usual."
                    ),
                    since_ms: record["at_ms"].as_i64(),
                });
            }
            if record["type"] != "source.main_red" || record["alert"].as_bool() != Some(true) {
                return None;
            }
            let failures = record["failures"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|failure| {
                    let test = failure["test"].as_str()?;
                    let message = failure["message"].as_str().unwrap_or_default();
                    Some(one_line(&format!("{test}: {message}"), 200))
                })
                .take(HOST_FAULT_QUOTED)
                .collect::<Vec<String>>()
                .join(" / ");
            let task = record["task"]
                .as_i64()
                .map_or_else(String::new, |task| format!(" (task #{task}'s gate)"));
            Some(Escalation {
                session_id: HOST_SESSION.to_string(),
                id: format!("main-red-{id}"),
                why: Why::MainRed,
                line: format!(
                    "main of {source} ({main}) is red on {scenario}: it failed twice on the \
                     base with the same failing tests{task}: {failures}. No candidate is to \
                     blame: the gates on {source} wait until main moves (a probe gate each \
                     SPAWN_GATE_MAIN_RED_PROBE_MS, an hour by default), nothing is counted and \
                     nothing halts. Fix main, or tell the owner."
                ),
                since_ms: record["at_ms"].as_i64(),
            })
        })
        .collect()
}

/// One side's `env` as one phrase: its host, toolchain and runner, each
/// only when named.
fn env_phrase(env: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(host) = env["host"].as_str() {
        parts.push(format!("host {host}"));
    }
    if let Some(runner) = env["runner"].as_str() {
        parts.push(format!("runner {runner}"));
    }
    parts.push(env["toolchain"].as_str().map_or_else(
        || "toolchain unknown".to_string(),
        |toolchain| format!("toolchain {}", one_line(toolchain, 200)),
    ));
    parts.join(", ")
}

/// The digest items for `records`, the shadow verdicts that disagreed with
/// the primary's on the same inputs (`task.shadow_gated` with `agrees`
/// false) that no delivered digest told yet
/// (`PostgresStore::shadow_disagreements`). Each is item
/// `shadow-disagrees-<event id>` of session `host`, told once: the task, the
/// runner, both verdicts, the commits and both environments. It decided
/// nothing (the primary's verdict stands); it is what milestone M3 must
/// have explained before a runner may decide a landing.
pub(crate) fn shadow_disagreements(records: &[Value]) -> Vec<Escalation> {
    records
        .iter()
        .filter_map(|record| {
            let id = record["event_id"].as_i64()?;
            let task = record["id"].as_i64()?;
            if record["agrees"].as_bool() != Some(false) {
                return None;
            }
            let runner = record["runner"].as_str().unwrap_or("the runner");
            let mission = record["mission"]
                .as_i64()
                .map_or_else(|| "its mission".to_string(), |id| format!("mission {id}"));
            let short = |field: &str| {
                let commit = record[field].as_str().unwrap_or("?");
                commit.get(..12).unwrap_or(commit).to_string()
            };
            let primary = record["primary"].as_str().unwrap_or("no verdict");
            let shadow = record["verdict"].as_str().unwrap_or("no verdict");
            Some(Escalation {
                session_id: HOST_SESSION.to_string(),
                id: format!("shadow-disagrees-{id}"),
                why: Why::ShadowDisagrees,
                line: format!(
                    "task #{task} of {mission}: the shadow judging on {runner} disagreed with \
                     the primary on the same inputs ({} -> {}): the primary {primary} it \
                     ({}), the runner {shadow} it ({}). The primary's verdict stands; \
                     explain the difference before a runner decides any landing.",
                    short("base"),
                    short("candidate"),
                    env_phrase(&record["primary_env"]),
                    env_phrase(&record["env"]),
                ),
                since_ms: record["at_ms"].as_i64(),
            })
        })
        .collect()
}

/// The digest items for `records`, the integrator's yields (`task.cancelled`
/// naming `yielded`) that no delivered digest told yet
/// (`PostgresStore::objective_yields`). Each is an item of session
/// `missions`, id `objective-yielded-<event id>`: the task, its mission and
/// objective, and the record's one-line reason, told once.
pub(crate) fn objective_yields(records: &[Value]) -> Vec<Escalation> {
    records
        .iter()
        .filter_map(|record| {
            let id = record["event_id"].as_i64()?;
            let task = record["id"].as_i64()?;
            let mission = record["mission"]
                .as_i64()
                .map_or_else(|| "its mission".to_string(), |id| format!("mission {id}"));
            let reason = record["reason"].as_str().map_or_else(
                || format!("task #{task} yielded its slot"),
                |reason| one_line(reason, 400),
            );
            Some(Escalation {
                session_id: MISSIONS_SESSION.to_string(),
                id: format!("objective-yielded-{id}"),
                why: Why::ObjectiveYielded,
                line: format!(
                    "{mission}: {reason}. It is tried again after the other open objectives; \
                     nothing halts. Split the objective, sharpen its brief, or raise its budget \
                     if it keeps failing."
                ),
                since_ms: record["at_ms"].as_i64(),
            })
        })
        .collect()
}

/// The digest items for `records`, the integrator's objectives closed solved
/// on the base (`objective.solved`) that no delivered digest told yet
/// (`PostgresStore::objective_solved_news`). Each is an item of session
/// `missions`, id `objective-solved-<event id>`: the mission, the objective,
/// the task it ended, and the record's own scenario and base — a reader
/// knows an objective closed without opening the record, and that it ended
/// solved rather than rejected.
pub(crate) fn objective_solved(records: &[Value]) -> Vec<Escalation> {
    records
        .iter()
        .filter_map(|record| {
            let id = record["event_id"].as_i64()?;
            // The record's `id` is the mission it closes.
            let mission = record["mission"]
                .as_i64()
                .or_else(|| record["id"].as_i64())
                .unwrap_or_default();
            let objective = record["objective"].as_i64()?;
            let task = record["task"].as_i64()?;
            let scenario = record["scenario"].as_str().map_or_else(
                || "its scenario".to_string(),
                |path| {
                    path.rsplit('/')
                        .next()
                        .filter(|name| !name.is_empty())
                        .unwrap_or(path)
                        .to_string()
                },
            );
            let base = record["base"].as_str().unwrap_or_default();
            let base: String = base.chars().take(7).collect();
            Some(Escalation {
                session_id: MISSIONS_SESSION.to_string(),
                id: format!("objective-solved-{id}"),
                why: Why::ObjectiveSolved,
                line: format!(
                    "mission #{mission} objective {objective}: task #{task} ended solved, not \
                     rejected — {scenario} passes on the base at {base}, so the objective is \
                     closed and no task for it is assigned again."
                ),
                since_ms: record["at_ms"].as_i64(),
            })
        })
        .collect()
}

/// The digest items for `records` (`PostgresStore::over_budget_escalations`):
/// a task that continued an overrun task of its objective went over the
/// budget too, so no third task is issued for it and the objective waits
/// for the mission to be configured again. The item names both tasks, the
/// objective, the branch and the work it holds (`git diff --stat main`),
/// the budget and the tool calls, and what can be done. Told once: the
/// record carries no window, a digest that reached the supervisor with the
/// tag `host over-budget-<event id>` means it was told.
pub(crate) fn over_budget_escalations(records: &[Value]) -> Vec<Escalation> {
    records
        .iter()
        .filter_map(|record| {
            let id = record["event_id"].as_i64()?;
            let task = record["id"].as_i64()?;
            let continues = record["continues"].as_i64()?;
            let mission = record["mission"]
                .as_i64()
                .map_or_else(|| "its mission".to_string(), |id| format!("mission {id}"));
            let objective = record["objective"].as_u64().map_or_else(
                || "an objective".to_string(),
                |id| format!("objective {id}"),
            );
            let branch = record["branch"].as_str().unwrap_or("its branch");
            let commit = record["commit"]
                .as_str()
                .map_or_else(|| "none (its push failed)".to_string(), str::to_string);
            let diff = record["diff"]
                .as_str()
                .map(str::trim)
                .filter(|diff| !diff.is_empty())
                .map_or_else(
                    || ":the work it holds: none recorded".to_string(),
                    |diff| format!(":\n{diff}"),
                );
            let budget = record["budget"].to_string();
            let calls = record["tool_calls"]
                .as_i64()
                .map_or_else(|| "its".to_string(), |calls| calls.to_string());
            let reason = record["reason"].as_str().map_or_else(
                || format!("task #{task} went over its budget too"),
                |reason| one_line(reason, 300),
            );
            Some(Escalation {
                session_id: "host".to_string(),
                id: format!("over-budget-{id}"),
                why: Why::OverBudget,
                line: format!(
                    "{mission}: task #{task} (a continuation of task #{continues}) went over \
                     {objective}'s budget too: {reason}; it made {calls} tool calls, the budget \
                     {budget}. Its branch {branch} at {commit}{diff}. Split the objective, raise \
                     the budget with `spawn mission configure`, or take it over: no task is \
                     issued for it until the mission is configured again, and its other \
                     objectives go on."
                ),
                since_ms: record["at_ms"].as_i64(),
            })
        })
        .collect()
}

/// The digest items for `missions` (`PostgresStore::objective_backlogs`):
/// one per open objective mission whose backlog names an order that cannot
/// be met ([`crate::objectives::order_problems`]), of session `missions`,
/// id `objective-order-<mission>-<digest of the problems>`, so it is told
/// once for each content and holds until the backlog is fixed. A backlog
/// that cannot be read is the integrator's to report: no item.
pub(crate) fn objective_order(missions: &[Value]) -> Vec<Escalation> {
    missions
        .iter()
        .filter_map(|row| {
            let mission = row["mission"].as_i64()?;
            let problems = backlog_problems(std::path::Path::new(row["backlog"].as_str()?))?;
            if problems.is_empty() {
                return None;
            }
            let digest = crate::sources::fnv(&problems.join("\n"));
            Some(Escalation {
                session_id: MISSIONS_SESSION.to_string(),
                id: format!("objective-order-{mission}-{digest:016x}"),
                why: Why::ObjectiveOrder,
                line: format!(
                    "mission {mission}'s backlog names an order no task can meet: {}. Fix \
                     `after` in the backlog.",
                    problems.join("; ")
                ),
                since_ms: None,
            })
        })
        .collect()
}

/// What a backlog directory's files are now: each JSON file's name, size
/// and modification time, by name.
type Fingerprint = Vec<(std::ffi::OsString, u64, u128)>;

fn fingerprint(directory: &std::path::Path) -> Option<Fingerprint> {
    let mut files: Fingerprint = std::fs::read_dir(directory)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            let modified = metadata
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_nanos());
            Some((entry.file_name(), metadata.len(), modified))
        })
        .collect();
    files.sort();
    Some(files)
}

/// The order problems of the backlog in `directory`
/// ([`crate::objectives::order_problems`]), parsed again only when its
/// files' names, sizes or modification times changed: the dispatcher asks
/// every pass, and the backlog is over a megabyte of JSON. `None` when the
/// directory or a file cannot be read (the integrator reports that).
fn backlog_problems(directory: &std::path::Path) -> Option<Vec<String>> {
    type Cache = std::collections::BTreeMap<std::path::PathBuf, (Fingerprint, Vec<String>)>;
    static CACHE: std::sync::Mutex<Cache> = std::sync::Mutex::new(Cache::new());
    let now = fingerprint(directory)?;
    if let Ok(cache) = CACHE.lock()
        && let Some((seen, problems)) = cache.get(directory)
        && *seen == now
    {
        return Some(problems.clone());
    }
    let problems =
        crate::objectives::order_problems(&crate::objectives::load_backlog(directory).ok()?);
    if let Ok(mut cache) = CACHE.lock() {
        cache.insert(directory.to_path_buf(), (now, problems.clone()));
    }
    Some(problems)
}

/// The session of escalations about missions rather than an agent (a
/// halted mission, an expired hold).
const MISSIONS_SESSION: &str = "missions";

/// The digest items for `records`, the integrator's `source.hold_expired`
/// records that no delivered digest told yet
/// (`PostgresStore::hold_expiries`). Each is an item of session `missions`,
/// id `source-hold-expired-<event id>`, naming the source, the hold (its
/// event id, its reason, when it was taken and its bound), when it expired,
/// each task that waited behind it with its mission, and that landings on
/// the source resumed: an owner verdict judged against the hold's main may
/// be stale now. No time window: a record stays an item until a digest
/// carrying its tag (`missions source-hold-expired-<event id>`) reaches the
/// supervisor, so it is told once. A released hold makes no record, and is
/// told to nobody.
pub(crate) fn hold_expiries(records: &[Value]) -> Vec<Escalation> {
    let time = |ms: Option<i64>| {
        ms.map_or_else(
            || "at an unknown time".to_string(),
            crate::scorecard::time::format_rfc3339,
        )
    };
    records
        .iter()
        .filter_map(|record| {
            let id = record["event_id"].as_i64()?;
            let source = record["source"].as_str()?;
            let hold = record["hold"]
                .as_i64()
                .map_or_else(|| "?".to_string(), |hold| hold.to_string());
            let reason = record["reason"]
                .as_str()
                .map_or_else(String::new, |reason| {
                    format!(" (\"{}\")", one_line(reason, 300))
                });
            let bound = record["bound_s"].as_i64().map_or_else(
                || "its bound".to_string(),
                |secs| format!("a bound of {secs} s"),
            );
            let main = record["main"].as_str().map_or_else(
                || "its main".to_string(),
                |main| format!("its main {}", main.chars().take(12).collect::<String>()),
            );
            let waiting: Vec<String> = record["waiting"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|entry| {
                    let task = entry["task"].as_i64()?;
                    Some(entry["mission"].as_i64().map_or_else(
                        || format!("task #{task}"),
                        |mission| format!("task #{task} of mission {mission}"),
                    ))
                })
                .collect();
            let waited = if waiting.is_empty() {
                "no task waited behind it".to_string()
            } else {
                format!("{} waited behind it", waiting.join(", "))
            };
            Some(Escalation {
                session_id: MISSIONS_SESSION.to_string(),
                id: format!("source-hold-expired-{id}"),
                why: Why::HoldExpired,
                line: format!(
                    "the owner's hold {hold}{reason} on source {source}, taken {} with {bound}, \
                     expired at {} and ended by record {id}: {waited}; landings on {source} \
                     resumed. An owner verdict judged against {main} may be stale now: tell \
                     the owner.",
                    time(record["held_at_ms"].as_i64()),
                    time(record["expires_ms"].as_i64()),
                ),
                since_ms: record["at_ms"].as_i64(),
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
        "{OPENING} {} update{} for you. Check a worker with `{spawn} tail --session \
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_source_away_and_its_end_are_told_to_the_supervisor() {
        let records = [
            json!({"type": "alert.fired", "event_id": 11, "at_ms": 5, "kind": "source_away",
                   "subject": "mission #4", "evidence": {"source": "/mnt/vol/a.git"}}),
            json!({"type": "alert.cleared", "event_id": 12, "at_ms": 6, "kind": "source_away",
                   "subject": "mission #4",
                   "evidence": {"source": "/mnt/vol/a.git", "present": true}}),
            json!({"type": "alert.cleared", "event_id": 13, "at_ms": 7, "kind": "source_away",
                   "subject": "mission #6",
                   "evidence": {"source": "/srv/b.git", "reason": "no longer watched"}}),
        ];
        let news = source_away_news(&records);
        assert_eq!(
            news.iter()
                .map(|item| (item.id.as_str(), item.why))
                .collect::<Vec<_>>(),
            [
                ("source-away-11", Why::SourceAway),
                ("source-back-12", Why::SourceBack),
                ("source-back-13", Why::SourceBack)
            ]
        );
        assert!(news.iter().all(|item| item.session_id == HOST_SESSION));
        assert!(
            news[0]
                .line
                .starts_with("mission #4's source /mnt/vol/a.git is not a directory"),
            "{}",
            news[0].line
        );
        assert!(
            news[1].line.contains("is back on this host"),
            "{}",
            news[1].line
        );
        assert!(
            news[2].line.contains("no longer has /srv/b.git"),
            "{}",
            news[2].line
        );
        assert!(Why::SourceAway.is_news() && Why::SourceBack.is_news());
    }
}
