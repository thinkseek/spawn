//! The supervisor digest: pending escalations coalesced into one message,
//! typed into the supervisor's pane while it is idle. What the supervisor
//! was already told is read back from the tags delivered digests filed on
//! their commands, never from their text: that quotes workers' reports.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use serde_json::{Value, json};
use spawn_core::command::CommandAction;
use spawn_core::observation::SemanticState;
use spawn_postgres::PostgresStore;

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
        Why::SourceAway => {
            "a mission's source is not a directory on this host, or not one git can read (its \
             branches wait)"
        }
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
        Why::DrainExpired => "the integrator's drain reached its bound (gates stopped)",
        Why::ModelFinal => "the model's error ended a worker's engine for good (not revived)",
        Why::EngineUnavailable => {
            "a task's engine or model label is not available on this host (no start is made)"
        }
        Why::MainAdvanced => {
            "a mission source's main advanced outside spawn integrate (refresh your view of main)"
        }
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
                && item.why != Why::ModelFinal
                && item.why != Why::MainAdvanced
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

/// The supervisor's item for a task the load-sensitive bound parked
/// ([`host_fault_escalations`], integrate.rs `Judge::load_sensitive`): its
/// own item, naming the scenarios and each one's root-fix owner, and how it
/// is released.
fn parked_escalation(record: &Value, id: i64, task: i64, mission: &str) -> Escalation {
    let pushed = record["pushed"].as_str().unwrap_or("?");
    let owners = record["owners"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|entry| {
            let scenario = entry["scenario"].as_str().unwrap_or("a scenario");
            match entry["owner"].as_str() {
                Some(owner) => format!("{scenario} (root fix: {owner})"),
                None => format!("{scenario} (no root-fix owner named in scenario-owners)"),
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    Escalation {
        session_id: "host".to_string(),
        id: format!("gate-host-fault-{id}"),
        why: Why::GateHostFault,
        line: format!(
            "task #{task} of {mission} is parked: its pushed commit {} was load-sensitive twice: \
             {owners}. It is not judged again until a new commit is pushed or a person releases \
             it (spawn task release --id {task} --reason <why>); the mission's other tasks go on.",
            pushed.get(..12).unwrap_or(pushed)
        ),
        since_ms: record["at_ms"].as_i64(),
    }
}

/// The evidence of a first `load_sensitive` host fault: the scenarios the
/// gate found load-sensitive (the record's `sensitive`) on the task's pushed
/// commit.
fn load_sensitive_evidence(record: &Value) -> String {
    format!(
        "the gate found {} load-sensitive on the task's pushed commit {} (failed at load, passed \
         alone as the base does); a second time on the same pushed commit parks it",
        record["sensitive"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", "),
        record["pushed"].as_str().unwrap_or("?")
    )
}

/// The digest items for `records`, the integrator's `host_fault` gate
/// records that carry `escalate` (the mission's second host fault in a row)
/// and that no delivered digest told yet
/// (`PostgresStore::gate_host_fault_escalations`). Each names the task, the
/// mission, the count, the host's evidence (the crash lines from the test
/// database's log, or the first failure's test and message) and when the
/// task is judged again (the record's `due_ms`). No time window: a record
/// stays an item until a digest carrying its tag
/// (`host gate-host-fault-<event id>`) reaches the supervisor, so it is told
/// once. A record of the load-sensitive bound is its own item
/// ([`parked_escalation`], [`load_sensitive_evidence`]).
#[allow(clippy::too_many_lines)]
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
            if record["evidence"] == "load_sensitive_parked" {
                return Some(parked_escalation(record, id, task, &mission));
            }
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
            let evidence = if record["evidence"] == "load_sensitive" {
                load_sensitive_evidence(record)
            } else if record["evidence"] == "cannot_judge" {
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
/// (`test_server_never_started`), or a gate abandoned by its runner
/// (`abandoned`), with the gate's reason; `None` for another evidence.
fn never_started(evidence: &Value, fact: &Value) -> Option<String> {
    let said = one_line(fact["reason"].as_str().unwrap_or_default(), 300);
    if evidence == crate::gate::SANDBOX_NEVER_STARTED {
        let scenario = fact["scenario"].as_str().unwrap_or("a scenario");
        Some(format!(
            "the gate's sandbox never started {scenario} ({said})"
        ))
    } else if evidence == crate::gate::TEST_SERVER_NEVER_STARTED {
        Some(format!("the gate's own test server never started ({said})"))
    } else if evidence == crate::gate::ABANDONED {
        Some(format!("the gate was abandoned ({said})"))
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
    } else if evidence == crate::gate::ABANDONED {
        "Check why the gate's unit ended (journalctl --user -u spawn-gate-*: a reboot, the OOM \
         killer, a stop from outside) or tell the owner."
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
                        "{subject}'s source {source} is not a directory on this host, or not \
                         one git can read: its agents' branches are not published to it until \
                         it is back. Check the mount or the path; Spawn retries on its own."
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

/// The digest items for `records`, the integrator's
/// `integrator.drain_expired` records no delivered digest told yet
/// (`PostgresStore::drain_expiries`): one each, of session `host`, id
/// `integrator-drain-expired-<event id>`, naming each task whose gate the
/// bound stopped as `task #<id>`. Told once: a digest that reached the
/// supervisor with its tag means it was told.
pub(crate) fn drain_expiries(records: &[Value]) -> Vec<Escalation> {
    records
        .iter()
        .filter_map(|record| {
            let id = record["event_id"].as_i64()?;
            let bound = record["bound_s"].as_i64().map_or_else(
                || "its bound".to_string(),
                |secs| format!("its bound of {secs} s (SPAWN_INTEGRATE_DRAIN_SECS)"),
            );
            let tasks: Vec<String> = record["gates"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|task| task.as_i64().map(|task| format!("task #{task}")))
                .collect();
            let stopped = if tasks.is_empty() {
                "no gate was running; nothing was lost".to_string()
            } else {
                format!(
                    "it stopped the gate{} of {}, which recorded no verdict: the next \
                     integrator judges {} again from nothing",
                    if tasks.len() == 1 { "" } else { "s" },
                    tasks.join(", "),
                    if tasks.len() == 1 { "it" } else { "them" },
                )
            };
            Some(Escalation {
                session_id: HOST_SESSION.to_string(),
                id: format!("integrator-drain-expired-{id}"),
                why: Why::DrainExpired,
                line: format!(
                    "the integrator was asked to drain and reached {bound} before its gates \
                     ended (record {id}): {stopped}. A longer bound keeps such gates; tell the \
                     owner if this repeats."
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

/// First-tier ceiling (ms) for consecutive `dispatch-digest-*` `PANE_NOT_READY`
/// backoffs (streaks 1..=6 double up to 60s), preserving the short initial
/// schedule before escalating to second-tier delays.
const SUPERVISOR_NOT_READY_TIER1_CAP_MS: i64 = 60_000;

/// Streak length after which second-tier exponential backoff kicks in past 60s.
const SUPERVISOR_NOT_READY_TIER1_STREAK: usize = 6;

/// Number of doublings used to compute the wait at [`SUPERVISOR_NOT_READY_TIER1_STREAK`].
const SUPERVISOR_NOT_READY_TIER1_DOUBLINGS: u32 = 5;

/// Second-tier ceiling (15 minutes) for chronic `dispatch-digest-*`
/// `PANE_NOT_READY` failures so a stuck supervisor never produces an unbounded
/// retry storm.
const SUPERVISOR_NOT_READY_MAX_MS: i64 = 15 * 60_000;

/// Deduplicates per-streak supervisor `PANE_NOT_READY` diagnostic logs by
/// `(last_command_key, consecutive_count)` so an unchanged failure does not spam
/// stderr on every tick.
static LAST_NOT_READY_DIAG: std::sync::Mutex<Option<(String, usize)>> = std::sync::Mutex::new(None);

/// Exponential backoff wait (ms) for trailing `dispatch-digest-*`
/// `PANE_NOT_READY` failures starting from `base_ms`:
/// - Streaks 1..=6 double up to `max(base_ms, 60_000)` (`5s..=60s` at default);
/// - Streaks 7+ continue doubling past 60s up to `max(base_ms, 900_000)`.
fn not_ready_wait_ms(consecutive: usize, base_ms: i64) -> i64 {
    let base = base_ms.max(1);
    let tier1_cap = base.max(SUPERVISOR_NOT_READY_TIER1_CAP_MS);
    if consecutive <= SUPERVISOR_NOT_READY_TIER1_STREAK {
        let doublings = u32::try_from(consecutive.saturating_sub(1))
            .unwrap_or(u32::MAX)
            .min(6);
        return base.saturating_mul(1_i64 << doublings).min(tier1_cap);
    }
    let at_tier1_end = base
        .saturating_mul(1_i64 << SUPERVISOR_NOT_READY_TIER1_DOUBLINGS)
        .min(tier1_cap);
    let extra = u32::try_from(consecutive - SUPERVISOR_NOT_READY_TIER1_STREAK)
        .unwrap_or(u32::MAX)
        .min(10);
    let max_cap = base.max(SUPERVISOR_NOT_READY_MAX_MS);
    at_tier1_end.saturating_mul(1_i64 << extra).min(max_cap)
}

/// Trailing `PANE_NOT_READY` streak on `dispatch-digest-*` commands paired with
/// its effective backoff duration in milliseconds.
fn not_ready_streak(
    supervisor: &Facts,
    base_ms: i64,
) -> Option<(crate::deliver::NotReadyStreak<'_>, i64)> {
    let streak = crate::deliver::trailing_not_ready_streak(
        supervisor
            .dispatched
            .iter()
            .filter(|sent| sent.key.starts_with("dispatch-digest-")),
    )?;
    let wait_ms = not_ready_wait_ms(streak.consecutive, base_ms);
    Some((streak, wait_ms))
}

/// Structured escalation diagnostic emitted once per streak step once a
/// `dispatch-digest-*` command hits [`crate::deliver::NOT_READY_WARN_THRESHOLD`]
/// consecutive `PANE_NOT_READY` failures.
fn not_ready_diagnostic(
    streak: &crate::deliver::NotReadyStreak<'_>,
    wait_ms: i64,
    pending_count: usize,
) -> Option<String> {
    (streak.consecutive >= crate::deliver::NOT_READY_WARN_THRESHOLD).then(|| {
        format!(
            "dispatch: escalation=supervisor_digest_not_ready session={SUPERVISOR_SESSION} \
             consecutive={} same_cause={} ready_pane_stall={} pending={pending_count} \
             backoff_ms={wait_ms} last_command_id={} last_key={} cause=\"{}\"",
            streak.consecutive,
            streak.same_cause,
            streak.ready_pane_stall,
            streak
                .last_command_id
                .map_or_else(|| "-".to_string(), |id| id.to_string()),
            streak.last_key,
            one_line(streak.last_error, 200),
        )
    })
}

fn log_not_ready_diagnostic(
    streak: &crate::deliver::NotReadyStreak<'_>,
    wait_ms: i64,
    pending_count: usize,
) {
    let Some(diag) = not_ready_diagnostic(streak, wait_ms, pending_count) else {
        return;
    };
    let key = (streak.last_key.to_string(), streak.consecutive);
    if let Ok(mut last) = LAST_NOT_READY_DIAG.lock() {
        if last.as_ref() == Some(&key) {
            return;
        }
        *last = Some(key);
    }
    eprintln!("{diag}");
}

/// Remaining backoff duration in milliseconds using an explicit `base_ms`.
fn due_in_with(supervisor: &Facts, base_ms: i64, now_ms: i64) -> Option<i64> {
    if !supervisor_idle(supervisor) {
        return None;
    }
    let (streak, wait_ms) = not_ready_streak(supervisor, base_ms)?;
    let left = streak
        .last_finished_ms
        .saturating_add(wait_ms)
        .saturating_sub(now_ms);
    (left > 0).then_some(left)
}

/// Remaining backoff duration in milliseconds if the supervisor digest is
/// currently cooled down by prior `PANE_NOT_READY` failures.
pub(super) fn due_in(supervisor: &Facts, now_ms: i64) -> Option<i64> {
    due_in_with(supervisor, crate::deliver::env_not_ready_ms(), now_ms)
}

/// Time-aware digest order using an explicit `base_ms` for `PANE_NOT_READY`
/// exponential backoff.
fn order_at_with(
    supervisor: &Facts,
    pending: &[Escalation],
    spawn: &str,
    base_ms: i64,
    now_ms: i64,
) -> Option<Order> {
    if pending.is_empty() || !supervisor_idle(supervisor) {
        return None;
    }
    if let Some((streak, wait_ms)) = not_ready_streak(supervisor, base_ms)
        && now_ms < streak.last_finished_ms.saturating_add(wait_ms)
    {
        log_not_ready_diagnostic(&streak, wait_ms, pending.len());
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

/// Time-aware digest order: applies bounded exponential backoff when recent
/// `dispatch-digest-*` commands failed with `PANE_NOT_READY`.
pub(super) fn order_at(
    supervisor: &Facts,
    pending: &[Escalation],
    spawn: &str,
    now_ms: i64,
) -> Option<Order> {
    order_at_with(
        supervisor,
        pending,
        spawn,
        crate::deliver::env_not_ready_ms(),
        now_ms,
    )
}

/// The digest items for `records`, the recovery's final model records that
/// no delivered digest told yet (`PostgresStore::model_finals`). Each is an
/// item of the worker's session, id `model-<execution id>`, naming the
/// session, the execution, the cause and either the refused request's
/// status (and its text) or the retries spent. No time window: a record
/// stays an item until a digest carrying its tag (`<session>
/// model-<execution id>`) reaches the supervisor, so it is told once.
pub(crate) fn model_finals(records: &[Value]) -> Vec<Escalation> {
    records
        .iter()
        .filter_map(|record| {
            let session = record["session_id"].as_str()?;
            let execution = record["execution_id"].as_str()?;
            let cause = record["cause"].as_str()?;
            let status = record["status"].as_i64().map_or_else(
                || "no status".to_string(),
                |status| format!("status {status}"),
            );
            let text = record["text"]
                .as_str()
                .filter(|text| !text.is_empty())
                .map_or_else(String::new, |text| {
                    format!(" (\"{}\")", one_line(text, 200))
                });
            let line = if cause == "model_unavailable" {
                let retries = record["retries"]
                    .as_u64()
                    .map_or_else(|| "its".to_string(), |retries| retries.to_string());
                format!(
                    "session {session}: execution {execution} ended on the model's transient \
                     error again ({cause}, {status}){text} after {retries} retries in a row \
                     (SPAWN_RECOVER_MODEL_RETRIES): Spawn will not resume it. Resume it once \
                     the model answers, or reassign its task."
                )
            } else {
                format!(
                    "session {session}: execution {execution} ended on a request the model \
                     refused ({cause}, {status}){text}: Spawn will not retry it, the same \
                     request fails the same way. Look at the session, then resume it with \
                     another instruction or reassign its task."
                )
            };
            Some(Escalation {
                session_id: session.to_string(),
                id: format!("model-{execution}"),
                why: Why::ModelFinal,
                line,
                since_ms: record["at_ms"].as_i64(),
            })
        })
        .collect()
}

/// Durable baseline/landing checkpoint kind written as a notice so restarts
/// track the last seen `refs/heads/main` without notifying the supervisor.
pub(crate) const SOURCE_MAIN_SYNCED_KIND: &str = "source_main_synced";

/// Durable notice kind recorded when a watched mission source's
/// `refs/heads/main` advances outside `spawn integrate`'s own landing path.
pub(crate) const SOURCE_MAIN_ADVANCED_KIND: &str = "source_main_advanced";

const EXTERNAL_MAIN_SUBJECT_LIMIT: usize = 5;
const EXTERNAL_MAIN_SUBJECT_CHARS: usize = 120;
const EXTERNAL_MAIN_POLL_DEFAULT_SECS: i64 = 15;
const EXTERNAL_MAIN_POLL_MAX_SECS: i64 = 3600;
const EXTERNAL_MAIN_POLL_ENV: &str = "SPAWN_EXTERNAL_MAIN_POLL_SECS";

/// Bounded git diff summary between an anchor `old_sha` and `new_sha` on a
/// mission source's `refs/heads/main`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExternalMainDiff {
    pub(crate) update_kind: &'static str,
    pub(crate) old_reachable: bool,
    pub(crate) ahead: u64,
    pub(crate) behind: u64,
    pub(crate) subjects: Vec<String>,
}

/// Polling interval (ms) for checking local mission sources' `refs/heads/main`
/// when no database event moved the log watermark.
pub(crate) fn external_main_poll_ms() -> i64 {
    let raw = crate::config::env_value(EXTERNAL_MAIN_POLL_ENV);
    parse_external_main_poll_ms(raw.as_deref())
}

fn parse_external_main_poll_ms(raw: Option<&str>) -> i64 {
    let secs = raw
        .and_then(|text| text.trim().parse::<i64>().ok())
        .filter(|secs| (1..=EXTERNAL_MAIN_POLL_MAX_SECS).contains(secs))
        .unwrap_or(EXTERNAL_MAIN_POLL_DEFAULT_SECS);
    secs.saturating_mul(1_000)
}

fn short_sha(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

fn read_local_main(source: &str) -> Result<Option<String>, String> {
    if source.contains("://") || !Path::new(source).is_absolute() || !Path::new(source).is_dir() {
        return Ok(None);
    }
    let parsed = crate::sources::MissionSource::parse(source)?;
    let Ok(sha) = crate::sources::git(&[
        "-C",
        parsed.as_str(),
        "rev-parse",
        "--verify",
        "--quiet",
        "refs/heads/main^{commit}",
    ]) else {
        return Ok(None);
    };
    let trimmed = sha.trim();
    if trimmed.len() >= 40 && trimmed.chars().all(|ch| ch.is_ascii_hexdigit()) {
        Ok(Some(trimmed.to_string()))
    } else {
        Ok(None)
    }
}

fn git_count(source: &str, rev_expr: &str) -> u64 {
    crate::sources::git(&["-C", source, "rev-list", "--count", rev_expr])
        .ok()
        .and_then(|out| out.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

fn git_subjects(source: &str, rev_expr: &str, limit: usize) -> Vec<String> {
    let max_arg = format!("-n{limit}");
    let Ok(out) = crate::sources::git(&[
        "-C",
        source,
        "log",
        "--no-decorate",
        &max_arg,
        "--format=%h %s",
        rev_expr,
    ]) else {
        return Vec::new();
    };
    out.lines()
        .map(|line| one_line(line, EXTERNAL_MAIN_SUBJECT_CHARS))
        .filter(|line| !line.is_empty())
        .collect()
}

/// Summarize how `refs/heads/main` moved from `old_sha` to `new_sha` in the
/// local repository `source`.
pub(crate) fn summarize_main_move(source: &str, old_sha: &str, new_sha: &str) -> ExternalMainDiff {
    let old_object = format!("{old_sha}^{{commit}}");
    let old_reachable = crate::sources::git(&["-C", source, "cat-file", "-e", &old_object]).is_ok();
    if !old_reachable {
        return ExternalMainDiff {
            update_kind: "non_fast_forward",
            old_reachable: false,
            ahead: 0,
            behind: 0,
            subjects: git_subjects(source, new_sha, EXTERNAL_MAIN_SUBJECT_LIMIT),
        };
    }
    let is_ff = crate::sources::git(&[
        "-C",
        source,
        "merge-base",
        "--is-ancestor",
        old_sha,
        new_sha,
    ])
    .is_ok();
    let range_ahead = format!("{old_sha}..{new_sha}");
    let range_behind = format!("{new_sha}..{old_sha}");
    let ahead = git_count(source, &range_ahead);
    let behind = if is_ff {
        0
    } else {
        git_count(source, &range_behind)
    };
    let update_kind = if is_ff {
        "fast_forward"
    } else if ahead == 0 && behind > 0 {
        "rewound"
    } else {
        "non_fast_forward"
    };
    let subjects = if ahead > 0 {
        git_subjects(source, &range_ahead, EXTERNAL_MAIN_SUBJECT_LIMIT)
    } else {
        git_subjects(source, new_sha, 1)
    };
    ExternalMainDiff {
        update_kind,
        old_reachable: true,
        ahead,
        behind,
        subjects,
    }
}

fn watched_local_sources(store: &PostgresStore) -> Result<BTreeMap<String, Vec<i64>>, String> {
    let rows = store
        .alert_mission_sources()
        .map_err(|error| format!("alert mission sources: {error}"))?;
    let mut watched: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    for entry in rows.as_array().into_iter().flatten() {
        let Some(mission_id) = entry["mission"].as_i64() else {
            continue;
        };
        for source in entry["sources"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !source.contains("://")
                && Path::new(source).is_absolute()
                && Path::new(source).is_dir()
            {
                watched
                    .entry(source.to_string())
                    .or_default()
                    .push(mission_id);
            }
        }
    }
    for missions in watched.values_mut() {
        missions.sort_unstable();
        missions.dedup();
    }
    Ok(watched)
}

fn latest_sync_anchors(alert_rows: &[Value]) -> BTreeMap<String, (i64, String)> {
    let mut anchors: BTreeMap<String, (i64, String)> = BTreeMap::new();
    for row in alert_rows {
        let Some(kind) = row["kind"].as_str() else {
            continue;
        };
        if kind != SOURCE_MAIN_SYNCED_KIND && kind != SOURCE_MAIN_ADVANCED_KIND {
            continue;
        }
        let Some(event_id) = row["fired_event_id"].as_i64() else {
            continue;
        };
        let Some(source) = row["evidence"]["source"]
            .as_str()
            .or_else(|| row["subject"].as_str())
        else {
            continue;
        };
        let Some(head) = row["evidence"]["to"]
            .as_str()
            .or_else(|| row["evidence"]["head"].as_str())
        else {
            continue;
        };
        let replace = anchors
            .get(source)
            .is_none_or(|(existing_id, _)| event_id >= *existing_id);
        if replace {
            anchors.insert(source.to_string(), (event_id, head.to_string()));
        }
    }
    anchors
}

#[derive(Default)]
struct GatedSourceState {
    baselines: Vec<String>,
    in_flight_candidates: Vec<String>,
}

fn gated_source_states(store: &PostgresStore) -> BTreeMap<String, GatedSourceState> {
    let Ok(raw) = store.gated_missions() else {
        return BTreeMap::new();
    };
    let mut non_halted: Vec<(String, Option<String>, Vec<String>)> = Vec::new();
    let mut halted: Vec<(String, Option<String>, Vec<String>)> = Vec::new();
    for mission in raw.as_array().into_iter().flatten() {
        let Some(source) = mission["source"].as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        let baseline = mission["baseline"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let candidates: Vec<String> = mission["tasks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|task| matches!(task["state"].as_str(), Some("accepted" | "landing")))
            .filter_map(|task| task["candidate"].as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        let entry = (source.to_string(), baseline, candidates);
        if mission["halted"].is_null() {
            non_halted.push(entry);
        } else {
            halted.push(entry);
        }
    }
    let mut by_source: BTreeMap<String, GatedSourceState> = BTreeMap::new();
    for (source, baseline, candidates) in non_halted.into_iter().chain(halted) {
        let state = by_source.entry(source).or_default();
        if let Some(sha) = baseline
            && !state.baselines.contains(&sha)
        {
            state.baselines.push(sha);
        }
        for sha in candidates {
            if !state.in_flight_candidates.contains(&sha) {
                state.in_flight_candidates.push(sha);
            }
        }
    }
    by_source
}

fn main_landing_info(
    store: &PostgresStore,
    source: &str,
    sha: &str,
) -> Option<(Option<(i64, String)>, bool)> {
    let value = store.main_landing(source, sha, None).ok()?;
    let landing = value["landing"].as_object().and_then(|obj| {
        let event = obj.get("event")?.as_i64()?;
        let record = obj.get("record")?.as_str()?.to_string();
        Some((event, record))
    });
    let pending_owner_match = value["owner_requests"]
        .as_array()
        .is_some_and(|reqs| reqs.iter().any(|req| req["commit"].as_str() == Some(sha)));
    Some((landing, pending_owner_match))
}

fn owner_request_move_after(
    store: &PostgresStore,
    after_event: i64,
    source: &str,
    target_sha: &str,
) -> Option<(i64, String)> {
    let mut cursor = after_event;
    loop {
        let Ok(page) = store.alert_control(cursor, 500) else {
            return None;
        };
        let records = page["records"].as_array()?;
        for entry in records {
            let Some(event_id) = entry["event_id"].as_i64() else {
                continue;
            };
            let record = &entry["record"];
            if event_id > after_event
                && record["type"].as_str() == Some("source.moved")
                && record["source"].as_str() == Some(source)
                && record["to"].as_str() == Some(target_sha)
                && record.get("request").is_some_and(|req| !req.is_null())
            {
                return Some((event_id, "source.moved".to_string()));
            }
        }
        let next = page["next"].as_i64().unwrap_or(cursor);
        if records.is_empty() || next <= cursor {
            return None;
        }
        cursor = next;
    }
}

fn effective_anchor(
    store: &PostgresStore,
    source: &str,
    gated_states: &BTreeMap<String, GatedSourceState>,
    notice_anchor: Option<&(i64, String)>,
) -> (i64, Option<String>) {
    let mut best = notice_anchor.map_or((0_i64, None), |(eid, sha)| (*eid, Some(sha.clone())));
    let Some(state) = gated_states.get(source) else {
        return best;
    };
    for baseline in &state.baselines {
        if let Some((Some((landing_event, _)), _)) = main_landing_info(store, source, baseline)
            && landing_event > best.0
        {
            best = (landing_event, Some(baseline.clone()));
            continue;
        }
        if best.1.is_none() {
            best = (0, Some(baseline.clone()));
        }
    }
    best
}

fn record_notice_idempotent(store: &PostgresStore, record: &Value) -> Result<Option<i64>, String> {
    match store.alert_record(record) {
        Ok(event_id) => Ok(Some(event_id)),
        Err(error) if error.to_string().contains("was already fired") => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

/// Detect external `refs/heads/main` movements across open/active local mission
/// repositories outside `spawn integrate`'s own landing path and record
/// deduplicated durable notices for the supervisor digest. Returns the count of
/// watched local sources.
#[allow(clippy::too_many_lines)]
pub(crate) fn sync_external_main(
    store: &PostgresStore,
    caller: &str,
    lines: &mut Vec<String>,
) -> usize {
    let watched = match watched_local_sources(store) {
        Ok(watched) => watched,
        Err(error) => {
            lines.push(format!("{caller}: external main sync: {error}"));
            return 0;
        }
    };
    if watched.is_empty() {
        return 0;
    }
    let alert_rows_val = match store.alert_rows() {
        Ok(rows) => rows,
        Err(error) => {
            lines.push(format!("{caller}: external main sync: alert_rows: {error}"));
            return watched.len();
        }
    };
    let rows = alert_rows_val["rows"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    let anchors = latest_sync_anchors(rows);
    let gated_states = gated_source_states(store);

    for (source, missions) in &watched {
        let current_head = match read_local_main(source) {
            Ok(Some(sha)) => sha,
            Ok(None) => continue,
            Err(error) => {
                lines.push(format!("{caller}: external main sync {source}: {error}"));
                continue;
            }
        };
        let (anchor_event, anchor_sha) =
            effective_anchor(store, source, &gated_states, anchors.get(source));
        let source_hash = crate::sources::fnv(source);
        let Some(old_sha) = anchor_sha else {
            let init_record = json!({
                "type": "alert.fired",
                "key": format!("source_main_synced:{source_hash:016x}:init"),
                "kind": SOURCE_MAIN_SYNCED_KIND,
                "subject": source,
                "notice": true,
                "text": format!(
                    "initialized main tracking on {source} at {}",
                    short_sha(&current_head)
                ),
                "evidence": {
                    "source": source,
                    "ref": "refs/heads/main",
                    "head": current_head,
                    "reason": "baseline",
                    "missions": missions,
                },
            });
            if let Err(error) = record_notice_idempotent(store, &init_record) {
                lines.push(format!("{caller}: external main sync {source}: {error}"));
            }
            continue;
        };
        if old_sha == current_head {
            continue;
        }
        let candidate_in_flight = gated_states.get(source).is_some_and(|state| {
            state
                .in_flight_candidates
                .iter()
                .any(|cand| cand == &current_head)
        });
        if candidate_in_flight {
            continue;
        }
        if let Some((landing, pending_owner_match)) =
            main_landing_info(store, source, &current_head)
        {
            if pending_owner_match {
                continue;
            }
            if let Some((landing_event, record_type)) = landing
                && landing_event > anchor_event
            {
                let synced_record = json!({
                    "type": "alert.fired",
                    "key": format!("source_main_synced:{source_hash:016x}:landed:{landing_event}"),
                    "kind": SOURCE_MAIN_SYNCED_KIND,
                    "subject": source,
                    "notice": true,
                    "text": format!(
                        "recorded integrate landing on {source} ({}..{}, {record_type})",
                        short_sha(&old_sha),
                        short_sha(&current_head)
                    ),
                    "evidence": {
                        "source": source,
                        "ref": "refs/heads/main",
                        "from": old_sha,
                        "head": current_head,
                        "reason": record_type,
                        "landing_event_id": landing_event,
                        "missions": missions,
                    },
                });
                if let Err(error) = record_notice_idempotent(store, &synced_record) {
                    lines.push(format!("{caller}: external main sync {source}: {error}"));
                }
                continue;
            }
        }
        if let Some((landing_event, record_type)) =
            owner_request_move_after(store, anchor_event, source, &current_head)
        {
            let synced_record = json!({
                "type": "alert.fired",
                "key": format!("source_main_synced:{source_hash:016x}:landed:{landing_event}"),
                "kind": SOURCE_MAIN_SYNCED_KIND,
                "subject": source,
                "notice": true,
                "text": format!(
                    "recorded integrate owner push on {source} ({}..{}, {record_type})",
                    short_sha(&old_sha),
                    short_sha(&current_head)
                ),
                "evidence": {
                    "source": source,
                    "ref": "refs/heads/main",
                    "from": old_sha,
                    "head": current_head,
                    "reason": record_type,
                    "landing_event_id": landing_event,
                    "missions": missions,
                },
            });
            if let Err(error) = record_notice_idempotent(store, &synced_record) {
                lines.push(format!("{caller}: external main sync {source}: {error}"));
            }
            continue;
        }

        let diff = summarize_main_move(source, &old_sha, &current_head);
        let range_hash = crate::sources::fnv(&format!("{old_sha}..{current_head}"));
        let key =
            format!("source_main_advanced:{source_hash:016x}:{anchor_event}:{range_hash:016x}");
        let adv_record = json!({
            "type": "alert.fired",
            "key": key,
            "kind": SOURCE_MAIN_ADVANCED_KIND,
            "subject": source,
            "notice": true,
            "text": format!(
                "{source} refs/heads/main moved externally {}..{} ({})",
                short_sha(&old_sha),
                short_sha(&current_head),
                diff.update_kind
            ),
            "evidence": {
                "source": source,
                "ref": "refs/heads/main",
                "from": old_sha,
                "to": current_head,
                "update_kind": diff.update_kind,
                "old_reachable": diff.old_reachable,
                "ahead": diff.ahead,
                "behind": diff.behind,
                "subjects": diff.subjects,
                "missions": missions,
            },
        });
        match record_notice_idempotent(store, &adv_record) {
            Ok(Some(event_id)) => lines.push(format!(
                "{caller}: external main advance on {source} ({}..{}, {}): recorded event \
                 #{event_id}",
                short_sha(&old_sha),
                short_sha(&current_head),
                diff.update_kind
            )),
            Ok(None) => {}
            Err(error) => lines.push(format!(
                "{caller}: external main advance on {source}: {error}"
            )),
        }
    }

    watched.len()
}

/// Digest news items for external `refs/heads/main` movements stored in
/// `spawn_alerts` (`SOURCE_MAIN_ADVANCED_KIND`) and not yet told to the
/// supervisor (`missions main-advanced-<event_id>`).
pub(crate) fn main_advanced_news(alert_rows: &[Value]) -> Vec<Escalation> {
    alert_rows
        .iter()
        .filter(|row| row["kind"].as_str() == Some(SOURCE_MAIN_ADVANCED_KIND))
        .filter_map(|row| {
            let id = row["fired_event_id"].as_i64()?;
            let evidence = &row["evidence"];
            let source = evidence["source"]
                .as_str()
                .or_else(|| row["subject"].as_str())?;
            let from = short_sha(evidence["from"].as_str().unwrap_or("?"));
            let to = short_sha(evidence["to"].as_str().unwrap_or("?"));
            let update_kind = evidence["update_kind"].as_str().unwrap_or("updated");
            let old_reachable = evidence["old_reachable"].as_bool().unwrap_or(true);
            let ahead = evidence["ahead"].as_u64().unwrap_or(0);
            let behind = evidence["behind"].as_u64().unwrap_or(0);
            let missions: Vec<String> = evidence["missions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_i64)
                .map(|m| format!("#{m}"))
                .collect();
            let mission_phrase = if missions.is_empty() {
                String::new()
            } else {
                let noun = if missions.len() == 1 {
                    "mission"
                } else {
                    "missions"
                };
                format!(" ({noun} {})", missions.join(", "))
            };
            let delta = match update_kind {
                "fast_forward" => format!(
                    "fast-forward, +{ahead} commit{}",
                    if ahead == 1 { "" } else { "s" }
                ),
                "rewound" => format!(
                    "rewound, -{behind} commit{}",
                    if behind == 1 { "" } else { "s" }
                ),
                "non_fast_forward" if !old_reachable => {
                    "non-fast-forward, previous commit unreachable".to_string()
                }
                _ => format!("non-fast-forward / rebase, +{ahead}/-{behind}"),
            };
            let subjects: Vec<&str> = evidence["subjects"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let shown_count = u64::try_from(subjects.len()).unwrap_or(u64::MAX);
            let summary = if subjects.is_empty() {
                String::new()
            } else if old_reachable && ahead > shown_count {
                format!(
                    " — {}; (+{} more)",
                    subjects.join("; "),
                    ahead - shown_count
                )
            } else {
                format!(" — {}", subjects.join("; "))
            };
            Some(Escalation {
                session_id: MISSIONS_SESSION.to_string(),
                id: format!("main-advanced-{id}"),
                why: Why::MainAdvanced,
                line: format!(
                    "{source}{mission_phrase} refs/heads/main moved {from}..{to} ({delta}){summary}. \
                     Refresh your view of main before assigning new tasks to avoid duplicating \
                     already-merged work."
                ),
                since_ms: row["fired_at_ms"].as_i64(),
            })
        })
        .collect()
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

    fn idle_supervisor_facts(dispatched: Vec<crate::dispatch::Sent>) -> Facts {
        let mut observation = spawn_core::observation::Observation::basic(
            SemanticState::Waiting,
            None,
            spawn_core::event::SessionSeq(9),
            spawn_core::observation::Confidence::new(0.9).expect("confidence"),
            "rules",
        );
        observation.ambiguity = Some(spawn_core::observation::Ambiguity::TurnStoppedIdle);
        let mut facts: Facts = serde_json::from_value(json!({
            "session_id": SUPERVISOR_SESSION,
            "agent_id": SUPERVISOR_AGENT,
            "live": true,
            "turn_ended": true,
            "evidence_seq": 9,
            "observation_event_id": 99
        }))
        .expect("supervisor facts");
        facts.observation = Some(observation);
        facts.dispatched = dispatched;
        facts
    }

    fn failed_digest_sent(
        id: i64,
        idx: usize,
        finished_ms: i64,
        err: &str,
    ) -> crate::dispatch::Sent {
        crate::dispatch::Sent {
            id: Some(id),
            key: format!("dispatch-digest-99-{idx:016x}"),
            text: None,
            created_at_ms: finished_ms - 50,
            finished_at_ms: Some(finished_ms),
            succeeded: Some(false),
            error: Some(err.to_string()),
        }
    }

    #[test]
    fn supervisor_digest_escalates_past_six_failures_to_bounded_cap_and_emits_diagnostic() {
        let report_esc = Escalation {
            session_id: "session_worker_x".to_string(),
            id: "report-42-1".to_string(),
            why: Why::Report,
            line: "report ready".to_string(),
            since_ms: None,
        };
        let ready_err = format!(
            "pane state Ready {}: the input typed last has not been answered yet",
            spawn_core::bridge::PANE_NOT_READY
        );
        let expected_waits = [
            5_000_i64, 10_000, 20_000, 40_000, 60_000, 60_000, 120_000, 240_000, 480_000, 900_000,
            900_000, 900_000,
        ];
        let mut sup = idle_supervisor_facts(Vec::new());
        let mut t = 1_000_000_i64;
        for (idx, wait) in expected_waits.into_iter().enumerate() {
            sup.dispatched.push(failed_digest_sent(
                i64::try_from(idx + 1).expect("small"),
                idx,
                t,
                &ready_err,
            ));
            assert_eq!(due_in(&sup, t), Some(wait), "streak {}", idx + 1);
            assert!(
                order_at(
                    &sup,
                    std::slice::from_ref(&report_esc),
                    "spawn",
                    t + wait - 1
                )
                .is_none()
            );
            assert!(order_at(&sup, std::slice::from_ref(&report_esc), "spawn", t + wait).is_some());
            let (streak, computed_wait) = not_ready_streak(&sup, 5_000).expect("streak");
            assert_eq!(computed_wait, wait);
            if idx + 1 < crate::deliver::NOT_READY_WARN_THRESHOLD {
                assert_eq!(not_ready_diagnostic(&streak, computed_wait, 1), None);
            } else {
                let diag = not_ready_diagnostic(&streak, computed_wait, 1).expect("diagnostic");
                assert!(
                    diag.contains("escalation=supervisor_digest_not_ready"),
                    "{diag}"
                );
                assert!(diag.contains("ready_pane_stall=true"), "{diag}");
                assert!(diag.contains(&format!("consecutive={}", idx + 1)), "{diag}");
                assert!(diag.contains(&format!("same_cause={}", idx + 1)), "{diag}");
                assert!(diag.contains(&format!("backoff_ms={wait}")), "{diag}");
            }
            t += wait + 100;
        }

        // A subsequent successful digest clears the trailing streak immediately.
        sup.dispatched.push(crate::dispatch::Sent {
            id: Some(99),
            key: "dispatch-digest-99-delivered".to_string(),
            text: None,
            created_at_ms: t,
            finished_at_ms: Some(t + 10),
            succeeded: Some(true),
            error: None,
        });
        assert_eq!(due_in(&sup, t + 10), None);
        assert!(order_at(&sup, &[report_esc], "spawn", t + 10).is_some());
    }

    #[test]
    fn supervisor_digest_backoff_honors_short_base_ms_and_caps_at_fifteen_minutes() {
        let report_esc = Escalation {
            session_id: "session_worker_y".to_string(),
            id: "report-7-0".to_string(),
            why: Why::Report,
            line: "report ready".to_string(),
            since_ms: None,
        };
        let busy_err = format!("pane state Busy {}", spawn_core::bridge::PANE_NOT_READY);
        let expected_short = [
            500_i64, 1_000, 2_000, 4_000, 8_000, 16_000, 32_000, 64_000, 128_000, 256_000, 512_000,
            900_000, 900_000,
        ];
        let mut sup = idle_supervisor_facts(Vec::new());
        let mut t = 2_000_000_i64;
        for (idx, wait) in expected_short.into_iter().enumerate() {
            sup.dispatched.push(failed_digest_sent(
                i64::try_from(idx + 1).expect("small"),
                idx,
                t,
                &busy_err,
            ));
            assert_eq!(
                due_in_with(&sup, 500, t),
                Some(wait),
                "short base streak {}",
                idx + 1
            );
            assert!(
                order_at_with(
                    &sup,
                    std::slice::from_ref(&report_esc),
                    "spawn",
                    500,
                    t + wait - 1
                )
                .is_none()
            );
            assert!(
                order_at_with(
                    &sup,
                    std::slice::from_ref(&report_esc),
                    "spawn",
                    500,
                    t + wait
                )
                .is_some()
            );
            t += wait + 50;
        }
    }

    #[test]
    fn summarize_main_move_fast_forward_bounds_subjects() {
        let root = std::env::temp_dir().join(format!("spawn-ext-main-ff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let work = root.join("work");
        let bare = root.join("source.git");
        std::fs::create_dir_all(&work).expect("work dir");
        let work_str = work.to_string_lossy().to_string();
        let bare_str = bare.to_string_lossy().to_string();
        crate::sources::test_git(&["init", "-q", "--initial-branch=main", &work_str])
            .expect("init");
        let commit_file = |name: &str, msg: &str| {
            std::fs::write(work.join(name), format!("{msg}\n")).expect("write");
            crate::sources::test_git(&["-C", &work_str, "add", "."]).expect("add");
            crate::sources::test_git(&[
                "-C",
                &work_str,
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@localhost",
                "commit",
                "-q",
                "-m",
                msg,
            ])
            .expect("commit");
            crate::sources::test_git(&["-C", &work_str, "rev-parse", "HEAD"]).expect("rev-parse")
        };
        let old_sha = commit_file("f0.txt", "c0 base");
        for idx in 1..=7 {
            commit_file(&format!("f{idx}.txt"), &format!("c{idx} external feature"));
        }
        crate::sources::test_git(&["clone", "-q", "--bare", &work_str, &bare_str]).expect("bare");
        let new_sha = crate::sources::test_git(&["-C", &bare_str, "rev-parse", "refs/heads/main"])
            .expect("head");

        let diff = summarize_main_move(&bare_str, &old_sha, &new_sha);
        assert_eq!(diff.update_kind, "fast_forward");
        assert!(diff.old_reachable);
        assert_eq!(diff.ahead, 7);
        assert_eq!(diff.behind, 0);
        assert_eq!(diff.subjects.len(), 5, "{:?}", diff.subjects);
        assert!(diff.subjects[0].contains("c7 external feature"));

        let items = main_advanced_news(&[json!({
            "kind": SOURCE_MAIN_ADVANCED_KIND,
            "subject": bare_str,
            "fired_event_id": 41,
            "fired_at_ms": 1_000_i64,
            "evidence": {
                "source": bare_str,
                "from": old_sha,
                "to": new_sha,
                "update_kind": diff.update_kind,
                "old_reachable": diff.old_reachable,
                "ahead": diff.ahead,
                "behind": diff.behind,
                "subjects": diff.subjects,
                "missions": [2, 5],
            }
        })]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].why, Why::MainAdvanced);
        assert_eq!(items[0].id, "main-advanced-41");
        assert!(
            items[0].line.contains("(missions #2, #5)"),
            "{}",
            items[0].line
        );
        assert!(
            items[0].line.contains("fast-forward, +7 commits"),
            "{}",
            items[0].line
        );
        assert!(items[0].line.contains("(+2 more)"), "{}", items[0].line);
        assert!(
            items[0]
                .line
                .contains("Refresh your view of main before assigning new tasks"),
            "{}",
            items[0].line
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn summarize_main_move_handles_rebase_rewind_and_unreachable_old_sha() {
        let root =
            std::env::temp_dir().join(format!("spawn-ext-main-rebase-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let work = root.join("work");
        let bare = root.join("source.git");
        std::fs::create_dir_all(&work).expect("work dir");
        let work_str = work.to_string_lossy().to_string();
        let bare_str = bare.to_string_lossy().to_string();
        crate::sources::test_git(&["init", "-q", "--initial-branch=main", &work_str])
            .expect("init");
        let commit_file = |name: &str, msg: &str| {
            std::fs::write(work.join(name), format!("{msg}\n")).expect("write");
            crate::sources::test_git(&["-C", &work_str, "add", "."]).expect("add");
            crate::sources::test_git(&[
                "-C",
                &work_str,
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@localhost",
                "commit",
                "-q",
                "-m",
                msg,
            ])
            .expect("commit");
            crate::sources::test_git(&["-C", &work_str, "rev-parse", "HEAD"]).expect("rev-parse")
        };
        let c0 = commit_file("f0.txt", "c0 root");
        let c1 = commit_file("f1.txt", "c1 old branch");
        crate::sources::test_git(&["-C", &work_str, "branch", "keep-old", &c1]).expect("keep old");
        crate::sources::test_git(&["-C", &work_str, "reset", "--hard", &c0]).expect("reset");
        let c2 = commit_file("f2.txt", "c2 rebased commit");
        crate::sources::test_git(&["clone", "-q", "--bare", &work_str, &bare_str]).expect("bare");
        crate::sources::test_git(&[
            "-C",
            &work_str,
            "push",
            "-q",
            &bare_str,
            &format!("{c1}:refs/heads/keep-old"),
        ])
        .expect("push keep-old");

        let rebased = summarize_main_move(&bare_str, &c1, &c2);
        assert_eq!(rebased.update_kind, "non_fast_forward");
        assert!(rebased.old_reachable);
        assert_eq!((rebased.ahead, rebased.behind), (1, 1));
        assert_eq!(rebased.subjects.len(), 1);
        assert!(rebased.subjects[0].contains("c2 rebased commit"));

        let rewound = summarize_main_move(&bare_str, &c2, &c0);
        assert_eq!(rewound.update_kind, "rewound");
        assert!(rewound.old_reachable);
        assert_eq!((rewound.ahead, rewound.behind), (0, 1));
        assert_eq!(rewound.subjects.len(), 1);
        assert!(rewound.subjects[0].contains("c0 root"));

        let missing_sha = "deadbeef".repeat(5);
        let pruned = summarize_main_move(&bare_str, &missing_sha, &c2);
        assert_eq!(pruned.update_kind, "non_fast_forward");
        assert!(!pruned.old_reachable);
        assert!(!pruned.subjects.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn external_main_poll_ms_bounds() {
        assert_eq!(parse_external_main_poll_ms(None), 15_000);
        assert_eq!(parse_external_main_poll_ms(Some("0")), 15_000);
        assert_eq!(parse_external_main_poll_ms(Some("99999")), 15_000);
        assert_eq!(parse_external_main_poll_ms(Some("bad")), 15_000);
        assert_eq!(parse_external_main_poll_ms(Some(" 42 ")), 42_000);
    }

    #[test]
    fn main_advanced_news_is_told_once_and_keeps_distinct_lines() {
        assert!(Why::MainAdvanced.is_news());
        let records = [
            json!({
                "kind": SOURCE_MAIN_ADVANCED_KIND,
                "subject": "/srv/a.git",
                "fired_event_id": 10,
                "fired_at_ms": 100,
                "evidence": {
                    "source": "/srv/a.git",
                    "from": "aaaaaaaaaaaaaaaaaaaa11111111111111111111",
                    "to": "bbbbbbbbbbbbbbbbbbbb22222222222222222222",
                    "update_kind": "fast_forward",
                    "old_reachable": true,
                    "ahead": 1,
                    "behind": 0,
                    "subjects": ["bbbbbbb fix auth"],
                    "missions": [1],
                }
            }),
            json!({
                "kind": SOURCE_MAIN_ADVANCED_KIND,
                "subject": "/srv/b.git",
                "fired_event_id": 11,
                "fired_at_ms": 105,
                "evidence": {
                    "source": "/srv/b.git",
                    "from": "cccccccccccccccccccc33333333333333333333",
                    "to": "dddddddddddddddddddd44444444444444444444",
                    "update_kind": "non_fast_forward",
                    "old_reachable": false,
                    "ahead": 0,
                    "behind": 0,
                    "subjects": ["ddddddd force reset"],
                    "missions": [],
                }
            }),
            json!({
                "kind": SOURCE_MAIN_SYNCED_KIND,
                "subject": "/srv/c.git",
                "fired_event_id": 12,
                "fired_at_ms": 110,
                "evidence": {"source": "/srv/c.git", "head": "eeee"}
            }),
        ];
        let news = main_advanced_news(&records);
        assert_eq!(news.len(), 2);
        let mut sup = idle_supervisor_facts(Vec::new());
        let plan = crate::dispatch::decide_with(
            std::slice::from_ref(&sup),
            &crate::deliver::Work::default(),
            &news,
            true,
            1_000,
            "spawn",
        );
        assert_eq!(plan.orders.len(), 1);
        let CommandAction::Input { text, tags } = &plan.orders[0].action else {
            panic!("expected digest input: {:?}", plan.orders[0].action);
        };
        assert!(text.contains("2 updates for you"), "{text}");
        assert!(text.contains("/srv/a.git"), "{text}");
        assert!(text.contains("/srv/b.git"), "{text}");
        assert_eq!(
            tags,
            &["missions main-advanced-10", "missions main-advanced-11"]
        );

        for tag in tags {
            sup.told.insert(tag.clone());
        }
        let after = crate::dispatch::decide_with(
            &[sup],
            &crate::deliver::Work::default(),
            &news,
            true,
            2_000,
            "spawn",
        );
        assert!(after.orders.is_empty());
        assert!(after.pending.is_empty());
        assert!(after.told.is_empty());
    }
}
