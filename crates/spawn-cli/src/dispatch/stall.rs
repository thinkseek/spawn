//! Stalls: no task waits silently (docs/fleet-architecture.md §3, "Stalls").
//!
//! The supervisor hears of events; a task that stops moving makes none. So
//! the dispatcher knows, for every open task, the step it waits on and since
//! which record, and escalates a step that has not moved within its bound,
//! once per stall ([`Why::Stalled`], heading `stalled`):
//!
//! | Step | The task | Measured from | Bound |
//! | --- | --- | --- | --- |
//! | `delivery` | assigned or sent back, and its session's turn ([`deliver::next_up`]) | `task.assigned`, or the review or verdict that sent it back | `SPAWN_STALL_DELIVERY_SECS` (600) |
//! | `activity` | delivered | the later of `task.delivered` and the session's latest fact | `SPAWN_STALL_ACTIVITY_SECS` (900) |
//! | `judgement` | reported, under review | `task.reported` | `SPAWN_STALL_REVIEW_SECS` (1800) |
//! | `judgement` | the task the integrator judges next in its gated mission | the later of the record that made it ready and the mission's latest verdict that is not `stale` | `SPAWN_STALL_GATE_SECS` (3600) |
//! | `landing` | landing | its `task.gated` accepted | `SPAWN_STALL_LANDING_SECS` (600) |
//! | `stop` | an ephemeral task closed whose worker still runs | the record that closed it | `SPAWN_STALL_STOP_SECS` (300) |
//! | `halted` | a halted mission with open tasks | its `mission.halted` | `SPAWN_STALL_HALTED_SECS` (0) |
//!
//! A task queued behind another of its session, or behind the task its
//! mission's gate judges next, is not escalated: the one ahead of it is.
//! The tasks of a halted mission waiting for the gate or a landing are the
//! halt's: the escalation is about the mission, once per `mission.halted`.
//! A failed command (a delivery attempt, a Stop, a resume) is the evidence
//! of the step it blocks, never a step of its own; a delivery whose retries
//! are spent was escalated as [`Why::DeliveryFailed`], and gets no stall.
//!
//! The tag is `<session> stall-<task>-<step>-<n>`, where `n` names the
//! record the step is measured from (a control record's id, or for
//! `activity` the `session_seq` of the latest fact): the existing `told`
//! mechanism sends it once, and a step that moved and stalled anew has a new
//! record, so a new tag. A failed retry, a `stale` verdict, a nudge, a
//! judgement or an input recorded `waiting` for the session's turn moves no
//! step (the waiting step is not a fact: `stall_facts` skips it). A worker at its prompt whose screen says its
//! background shells still run (Claude Code resumes it when they finish)
//! is treated as active: its `activity` step is not escalated while that
//! is its latest screen reading.
//!
//! This layer takes no action: it adds digest items, and says what Spawn
//! already tried and still will (a retry due, or due and held, and why).
//! Every age is on the database's clock (`spawn_now_ms()` against the
//! records' times, [`Stalls::now_ms`]), never this host's. It keeps no
//! state: a restarted loop computes the same stalls and the same tags.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use spawn_core::mission::TaskState;
use spawn_core::screen::shows_background_shells;

use super::{Escalation, Facts, Why};
use crate::deliver::{self, Work, one_line};
use crate::supervise::SUPERVISOR_SESSION;

/// The bound of every step, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Bounds {
    pub(crate) delivery: i64,
    pub(crate) activity: i64,
    pub(crate) review: i64,
    pub(crate) gate: i64,
    pub(crate) landing: i64,
    pub(crate) stop: i64,
    pub(crate) halted: i64,
}

/// The variables, with their defaults in seconds (the reasons are in
/// docs/architecture.md's environment table).
const VARIABLES: [(&str, u64); 7] = [
    ("SPAWN_STALL_DELIVERY_SECS", 600),
    ("SPAWN_STALL_ACTIVITY_SECS", 900),
    ("SPAWN_STALL_REVIEW_SECS", 1800),
    ("SPAWN_STALL_GATE_SECS", 3600),
    ("SPAWN_STALL_LANDING_SECS", 600),
    ("SPAWN_STALL_STOP_SECS", 300),
    ("SPAWN_STALL_HALTED_SECS", 0),
];

impl Default for Bounds {
    fn default() -> Self {
        Self::read(|_| None).0
    }
}

impl Bounds {
    /// The bounds `spawn loop` reads from its environment, with a warning
    /// for every value that is not a non-negative integer (its default
    /// holds).
    pub(crate) fn from_env() -> (Self, Vec<String>) {
        Self::read(crate::config::env_value)
    }

    fn read(lookup: impl Fn(&str) -> Option<String>) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let mut values = [0_i64; VARIABLES.len()];
        for ((name, fallback), value) in VARIABLES.iter().zip(values.iter_mut()) {
            let secs = match lookup(name) {
                None => *fallback,
                Some(text) => text.trim().parse::<u64>().unwrap_or_else(|_| {
                    warnings.push(format!(
                        "{name}={text:?} is not a whole number of seconds; using {fallback}"
                    ));
                    *fallback
                }),
            };
            *value = i64::try_from(secs)
                .unwrap_or(i64::MAX / 1000)
                .saturating_mul(1000);
        }
        let [delivery, activity, review, gate, landing, stop, halted] = values;
        (
            Self {
                delivery,
                activity,
                review,
                gate,
                landing,
                stop,
                halted,
            },
            warnings,
        )
    }
}

/// A control record a step is measured from.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct Record {
    /// The event's id.
    pub(crate) id: i64,
    /// The record's type (`task.assigned`, ...).
    #[serde(rename = "type")]
    pub(crate) kind: String,
    pub(crate) at_ms: i64,
}

/// The latest `stale` verdict on a task.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub(crate) struct Stale {
    #[serde(default)]
    pub(crate) reason: Option<String>,
    pub(crate) at_ms: i64,
}

/// One open task, as `stall_facts` reads it.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OpenTask {
    pub(crate) id: i64,
    pub(crate) mission_id: i64,
    pub(crate) kind: String,
    pub(crate) session_id: String,
    pub(crate) state: TaskState,
    #[serde(default)]
    pub(crate) revisions: i32,
    #[serde(default)]
    pub(crate) gated: bool,
    /// `review` or `gate`.
    #[serde(default)]
    pub(crate) accept: String,
    #[serde(default)]
    pub(crate) halted: bool,
    /// The latest record that put it in its state.
    #[serde(default)]
    pub(crate) record: Option<Record>,
    #[serde(default)]
    pub(crate) stale: Option<Stale>,
}

/// A gated mission's latest verdict that is not `stale`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Verdict {
    pub(crate) mission_id: i64,
    pub(crate) id: i64,
    /// `accepted`, `rejected`, `conflict`, `incompatible` or `aborted`.
    #[serde(rename = "verdict")]
    pub(crate) outcome: String,
    #[serde(default)]
    pub(crate) task: Option<String>,
    pub(crate) at_ms: i64,
}

/// A session's latest fact and latest screen reading.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Latest {
    pub(crate) session_id: String,
    #[serde(default)]
    pub(crate) seq: Option<i64>,
    #[serde(default)]
    pub(crate) event_name: Option<String>,
    #[serde(default)]
    pub(crate) at_ms: Option<i64>,
    /// The evidence of its latest `screen.observed`.
    #[serde(default)]
    pub(crate) screen: Option<String>,
}

/// A halted mission with open tasks.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Halt {
    pub(crate) mission_id: i64,
    pub(crate) reason: String,
    #[serde(default)]
    pub(crate) record: Option<Record>,
    #[serde(default)]
    pub(crate) tasks: Vec<i64>,
}

/// What stalls are measured from, as `stall_facts` returns it.
#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct Stalls {
    /// The database's clock when it was read.
    pub(crate) now_ms: i64,
    #[serde(default)]
    pub(crate) tasks: Vec<OpenTask>,
    #[serde(default)]
    pub(crate) verdicts: Vec<Verdict>,
    #[serde(default)]
    pub(crate) sessions: Vec<Latest>,
    #[serde(default)]
    pub(crate) halted: Vec<Halt>,
}

/// The step a task waits on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Delivery,
    Activity,
    Judgement,
    Landing,
    Stop,
}

impl Step {
    fn word(self) -> &'static str {
        match self {
            Self::Delivery => "delivery",
            Self::Activity => "activity",
            Self::Judgement => "judgement",
            Self::Landing => "landing",
            Self::Stop => "stop",
        }
    }
}

/// One task's step, measured.
struct Measured {
    step: Step,
    /// What names the record the step is measured from (the tag's `<n>`).
    n: i64,
    since_ms: i64,
    bound_ms: i64,
    /// The evidence and what Spawn already tried, for the line.
    line: String,
}

/// `ms` as a short duration: `45s`, `3m05s`, `2h04m`.
fn took(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, secs % 3600 / 60),
    }
}

/// A record and its age, for the line.
fn named(record: &Record, now_ms: i64) -> String {
    format!(
        "{} (record {}, {} ago)",
        record.kind,
        record.id,
        took(now_ms - record.at_ms)
    )
}

/// The session's latest judgement and whether it covers its latest fact.
fn judgement(facts: Option<&Facts>) -> String {
    let Some(facts) = facts else {
        return "no session yet".to_string();
    };
    let Some(observation) = facts.observation.as_ref() else {
        return "no judgement yet".to_string();
    };
    let covers = if facts.current_observation().is_some() {
        "covers the latest fact".to_string()
    } else {
        format!(
            "older than the latest fact (seq {})",
            facts.evidence_seq.unwrap_or(0)
        )
    };
    format!(
        "latest judgement {} by {} ({:.0}%), {covers}",
        observation.state.key(),
        observation.judge,
        observation.confidence.value() * 100.0
    )
}

/// Whether the worker runs, and how its last execution ended if not.
fn liveness(facts: Option<&Facts>) -> String {
    match facts {
        Some(facts) if facts.live => format!(
            "its worker runs (execution {})",
            facts.execution_id.as_deref().unwrap_or("?")
        ),
        Some(facts) => format!(
            "its worker does not run (its last execution ended: {})",
            facts.end_cause.as_deref().unwrap_or("unknown")
        ),
        None => "its worker never ran".to_string(),
    }
}

/// A command's outcome, with its key.
fn outcome(sent: &super::Sent) -> String {
    match sent.succeeded {
        None => format!("`{}` in flight", sent.key),
        Some(true) => format!("`{}` succeeded", sent.key),
        Some(false) => format!(
            "`{}` failed ({})",
            sent.key,
            one_line(sent.error.as_deref().unwrap_or("no error recorded"), 200)
        ),
    }
}

/// What the dispatcher already did about a delivered task's worker: its
/// nudges and resume in this episode and execution, the Stops sent into
/// it, and the items the supervisor was told about the session.
fn tried_on_worker(facts: Option<&Facts>, told: &BTreeSet<String>, task: &OpenTask) -> String {
    let Some(facts) = facts else {
        return "nothing".to_string();
    };
    let mut tried: Vec<String> = facts
        .dispatched
        .iter()
        .filter(|sent| {
            sent.key.starts_with("dispatch-nudge-") || sent.key.starts_with("dispatch-resume-")
        })
        .map(outcome)
        .collect();
    tried.extend(facts.stops.iter().map(|stop| match stop.succeeded {
        None => format!("`{}` ({}) in flight", stop.key, stop.action),
        Some(true) => format!("`{}` ({}) succeeded", stop.key, stop.action),
        Some(false) => format!(
            "`{}` ({}) failed ({})",
            stop.key,
            stop.action,
            one_line(stop.error.as_deref().unwrap_or("no error recorded"), 200)
        ),
    }));
    let episode = facts.episode.unwrap_or(0);
    for (id, what) in [
        (format!("idle-{episode}"), "the idle item"),
        (format!("done-{episode}"), "the finished item"),
        (
            format!("report-{}-{}", task.id, task.revisions),
            "the report item",
        ),
    ] {
        if told.contains(&format!("{} {id}", task.session_id)) {
            tried.push(format!("{what} was sent"));
        }
    }
    if tried.is_empty() {
        "nothing".to_string()
    } else {
        tried.join("; ")
    }
}

/// The task each gated mission's integrator judges next
/// (`integrate::Mission::ready`'s order: the lowest accepted task, or under
/// `accept` = `gate` also reported), by mission.
fn judged_next(stalls: &Stalls) -> BTreeSet<i64> {
    let mut next: BTreeMap<i64, i64> = BTreeMap::new();
    for task in &stalls.tasks {
        let ready = task.gated
            && (task.state == TaskState::Accepted
                || (task.accept == "gate" && task.state == TaskState::Reported));
        if ready {
            next.entry(task.mission_id)
                .and_modify(|id| *id = (*id).min(task.id))
                .or_insert(task.id);
        }
    }
    next.into_values().collect()
}

/// A delivered task's `activity` step: from the later of its delivery
/// record and its session's latest fact; none while its screen shows
/// background shells running.
fn activity(
    task: &OpenTask,
    record: &Record,
    context: &Context<'_>,
    facts: Option<&Facts>,
    bound_ms: i64,
) -> Option<Measured> {
    let now = context.stalls.now_ms;
    let latest = context
        .stalls
        .sessions
        .iter()
        .find(|latest| latest.session_id == task.session_id);
    if latest
        .and_then(|latest| latest.screen.as_deref())
        .is_some_and(shows_background_shells)
    {
        // At its prompt with background shells running: active.
        return None;
    }
    let fact =
        latest.and_then(|latest| Some((latest.seq?, latest.event_name.clone()?, latest.at_ms?)));
    let fact_line = fact.as_ref().map_or_else(
        || "the session has no fact".to_string(),
        |(seq, name, at)| {
            format!(
                "the session's latest fact: {name} (seq {seq}, {} ago)",
                took(now - at)
            )
        },
    );
    let line = format!(
        "delivered by {}; {fact_line}; {}; {}; tried: {}",
        named(record, now),
        judgement(facts),
        liveness(facts),
        tried_on_worker(facts, context.told, task)
    );
    Some(match fact {
        Some((seq, _, at)) if at >= record.at_ms => Measured {
            step: Step::Activity,
            n: seq,
            since_ms: at,
            bound_ms,
            line,
        },
        _ => Measured {
            step: Step::Activity,
            n: record.id,
            since_ms: record.at_ms,
            bound_ms,
            line,
        },
    })
}

/// A gated task's `judgement` step, if the gate judges it next: from
/// the later of the record that made it ready and the mission's latest
/// verdict that is not `stale`.
fn gate(
    task: &OpenTask,
    record: &Record,
    context: &Context<'_>,
    bound_ms: i64,
) -> Option<Measured> {
    let now = context.stalls.now_ms;
    if task.halted || !context.judged_next.contains(&task.id) {
        // The halt's, or queued behind the task the gate judges next.
        return None;
    }
    let verdict = context
        .stalls
        .verdicts
        .iter()
        .find(|verdict| verdict.mission_id == task.mission_id);
    let verdict_line = verdict.map_or_else(
        || "no verdict in the mission yet".to_string(),
        |verdict| {
            format!(
                "the mission's latest verdict: {} on task #{} (record {}, {} ago)",
                verdict.outcome,
                verdict.task.as_deref().unwrap_or("?"),
                verdict.id,
                took(now - verdict.at_ms)
            )
        },
    );
    let stale_line = task.stale.as_ref().map_or_else(
        || "no stale verdict on it".to_string(),
        |stale| {
            format!(
                "its latest stale verdict ({} ago): {}; the integrator judges it again \
                 on its next pass",
                took(now - stale.at_ms),
                one_line(stale.reason.as_deref().unwrap_or("no reason"), 200)
            )
        },
    );
    let line = format!(
        "the gate of mission #{} judges it next; ready since {}; {verdict_line}; \
         {stale_line}",
        task.mission_id,
        named(record, now)
    );
    Some(match verdict {
        Some(verdict) if verdict.at_ms > record.at_ms => Measured {
            step: Step::Judgement,
            n: verdict.id,
            since_ms: verdict.at_ms,
            bound_ms,
            line,
        },
        _ => Measured {
            step: Step::Judgement,
            n: record.id,
            since_ms: record.at_ms,
            bound_ms,
            line,
        },
    })
}
/// The step `task` waits on, measured, or none: it is queued, closed with
/// no worker left, the halt's, or its delivery was given up.
fn measure(
    task: &OpenTask,
    record: &Record,
    context: &Context<'_>,
    bounds: &Bounds,
) -> Option<Measured> {
    let now = context.stalls.now_ms;
    let facts = context.facts.get(task.session_id.as_str()).copied();
    let since_record = |step: Step, bound_ms: i64, line: String| Measured {
        step,
        n: record.id,
        since_ms: record.at_ms,
        bound_ms,
        line,
    };
    match task.state {
        TaskState::Assigned | TaskState::Revise => {
            if !context.next_up.contains(&task.id) {
                // Queued behind another task of its session.
                return None;
            }
            let row = context.work.tasks.iter().find(|row| row.id == task.id)?;
            let status = deliver::delivery_status(context.work, row, facts, now);
            if status.given_up {
                return None;
            }
            Some(since_record(
                Step::Delivery,
                bounds.delivery,
                format!(
                    "waits since {}; {}; {}; {}",
                    named(record, now),
                    liveness(facts),
                    judgement(facts),
                    status.line
                ),
            ))
        }
        TaskState::Delivered => activity(task, record, context, facts, bounds.activity),
        TaskState::Reported if !(task.gated && task.accept == "gate") => {
            let told = context.told.contains(&format!(
                "{} report-{}-{}",
                task.session_id, task.id, task.revisions
            ));
            Some(since_record(
                Step::Judgement,
                bounds.review,
                format!(
                    "its report (revision {}) waits for the supervisor's review since {}; the \
                     report item {}",
                    task.revisions,
                    named(record, now),
                    if told { "was sent" } else { "is not sent yet" }
                ),
            ))
        }
        TaskState::Reported | TaskState::Accepted if task.gated => {
            gate(task, record, context, bounds.gate)
        }
        TaskState::Landing if !task.halted => Some(since_record(
            Step::Landing,
            bounds.landing,
            format!(
                "the gate accepted it: {}; the integrator lands it on its next pass of mission \
                 #{}",
                named(record, now),
                task.mission_id
            ),
        )),
        TaskState::Accepted | TaskState::Landed | TaskState::Cancelled
            if task.kind == "ephemeral" && facts.is_some_and(|facts| facts.live) =>
        {
            let retire = format!("retire-task-{}", task.id);
            let sent = facts
                .and_then(|facts| facts.sent(&retire))
                .map_or_else(|| format!("`{retire}` not sent"), outcome);
            Some(since_record(
                Step::Stop,
                bounds.stop,
                format!(
                    "closed by {}, and {}; tried: {sent}; {}",
                    named(record, now),
                    liveness(facts),
                    tried_on_worker(facts, context.told, task)
                ),
            ))
        }
        TaskState::Reported
        | TaskState::Accepted
        | TaskState::Landing
        | TaskState::Landed
        | TaskState::Cancelled => None,
    }
}

/// What every task's measure reads besides the task.
struct Context<'a> {
    stalls: &'a Stalls,
    work: &'a Work,
    facts: BTreeMap<&'a str, &'a Facts>,
    told: &'a BTreeSet<String>,
    next_up: BTreeSet<i64>,
    judged_next: BTreeSet<i64>,
}

/// The stall items for the current facts: one per open task (or halted
/// mission) whose step has stood still for its bound or longer. Whether
/// the supervisor was told already is the digest's business (the tag).
pub(crate) fn items(
    rows: &[Facts],
    work: &Work,
    stalls: &Stalls,
    bounds: &Bounds,
) -> Vec<Escalation> {
    let empty = BTreeSet::new();
    let told = rows
        .iter()
        .find(|facts| facts.session_id == SUPERVISOR_SESSION)
        .map_or(&empty, |facts| &facts.told);
    let context = Context {
        stalls,
        work,
        facts: rows
            .iter()
            .map(|facts| (facts.session_id.as_str(), facts))
            .collect(),
        told,
        next_up: deliver::next_up(work),
        judged_next: judged_next(stalls),
    };
    let now = stalls.now_ms;
    let mut items = Vec::new();
    for task in &stalls.tasks {
        let Some(record) = &task.record else {
            // No record names it (a row from before the control stream).
            continue;
        };
        let Some(measured) = measure(task, record, &context, bounds) else {
            continue;
        };
        let still = now - measured.since_ms;
        if still < measured.bound_ms {
            continue;
        }
        let step = measured.step.word();
        items.push(Escalation {
            session_id: task.session_id.clone(),
            id: format!("stall-{}-{step}-{}", task.id, measured.n),
            why: Why::Stalled,
            line: one_line(
                &format!(
                    "task #{} (mission #{}) has not moved from step {step} for {} (bound {}): {}",
                    task.id,
                    task.mission_id,
                    took(still),
                    took(measured.bound_ms),
                    measured.line
                ),
                3000,
            ),
        });
    }
    for halt in &stalls.halted {
        let Some(record) = &halt.record else {
            continue;
        };
        let still = now - record.at_ms;
        if still < bounds.halted {
            continue;
        }
        let tasks: Vec<String> = halt.tasks.iter().map(|id| format!("#{id}")).collect();
        items.push(Escalation {
            session_id: "missions".to_string(),
            id: format!("stall-mission-{}-halted-{}", halt.mission_id, record.id),
            why: Why::Stalled,
            line: one_line(
                &format!(
                    "mission #{} has been halted for {} ({}): {}; its open tasks {} wait; only a \
                     human resumes it (`spawn mission resume --id {}`)",
                    halt.mission_id,
                    took(still),
                    named(record, now),
                    halt.reason,
                    tasks.join(", "),
                    halt.mission_id
                ),
                3000,
            ),
        });
    }
    items
}

#[cfg(test)]
mod tests;
