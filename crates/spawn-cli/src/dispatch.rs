//! Dispatcher: the control layer's deterministic policy for workers.
//!
//! The Observer says what state a worker is in; this decides what to do
//! about it, on the decision ladder of docs/fleet-architecture.md §3:
//!
//! 1. Rules over facts decide fixed cases: a lost execution is resumed once.
//! 2. One guarded nudge: when Jev judged that a worker stopped at an idle
//!    prompt without needing anyone (WAITING, confident, `needs_human` and
//!    `stuck` low), it gets `continue` typed in, once per episode and at most
//!    [`NUDGES_PER_HOUR`] times per agent-hour. Jev's judgement is an input,
//!    never the chooser of the action, and the action is reversible.
//! 3. Everything else escalates: attention, a nudge that did not help, a
//!    resume that failed, and completions (so work gets reviewed). Escalations
//!    are coalesced into one digest typed into the supervisor's pane while it
//!    is idle. Without a supervisor they stay in the attention queue.
//!
//! The dispatcher keeps no state. What it already did is read back from the
//! command queue: idempotency keys mark nudges and resumes, and a marker per
//! escalation in each delivered digest marks what the supervisor was told.
//! It commands only `agent_worker_*` sessions, plus digests to the supervisor.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use serde::Deserialize;
use spawn_core::command::{CommandAction, Submission};
use spawn_core::id::{AgentId, IdempotencyKey, Principal, SessionId};
use spawn_core::observation::{Ambiguity, Observation, SemanticState, WaitReason};
use spawn_core::port::CommandQueue;
use spawn_postgres::PostgresStore;

use crate::supervise::{SUPERVISOR_AGENT, SUPERVISOR_SESSION};

/// Principal the dispatcher submits as.
pub(crate) const DISPATCHER: &str = "spawn-dispatch";

/// Nudges per worker per hour, across episodes.
const NUDGES_PER_HOUR: usize = 3;

/// Least confidence of a WAITING judgement that may nudge.
const NUDGE_CONFIDENCE: f64 = 0.7;

/// Most `needs_human` that may nudge.
const NUDGE_NEEDS_HUMAN: f64 = 0.2;

/// Most `stuck` that may nudge.
const NUDGE_STUCK: f64 = 0.5;

/// Escalations per digest; the rest wait for the next one.
const DIGEST_ITEMS: usize = 8;

/// One command the dispatcher sent.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub(crate) struct Sent {
    pub(crate) key: String,
    #[serde(default)]
    pub(crate) text: Option<String>,
    pub(crate) created_at_ms: i64,
    #[serde(default)]
    pub(crate) finished_at_ms: Option<i64>,
    #[serde(default)]
    pub(crate) succeeded: Option<bool>,
}

/// Facts about one session, as `dispatch_facts` returns them.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Facts {
    pub(crate) session_id: String,
    pub(crate) agent_id: String,
    #[serde(default)]
    pub(crate) live: bool,
    #[serde(default)]
    pub(crate) execution_id: Option<String>,
    #[serde(default)]
    pub(crate) end_cause: Option<String>,
    #[serde(default)]
    pub(crate) observation: Option<Observation>,
    #[serde(default)]
    pub(crate) observation_event_id: Option<i64>,
    #[serde(default)]
    pub(crate) observation_at_ms: Option<i64>,
    /// Session position of the latest fact (not an observation).
    #[serde(default)]
    pub(crate) evidence_seq: Option<u64>,
    #[serde(default)]
    pub(crate) episode: Option<i64>,
    /// Action of that command (`start`, `input`, `resume`, `stop`).
    #[serde(default)]
    pub(crate) episode_action: Option<String>,
    #[serde(default)]
    pub(crate) open_command: bool,
    #[serde(default)]
    pub(crate) dispatched: Vec<Sent>,
}

/// Why a worker is escalated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Why {
    /// The observation asks for attention.
    Attention,
    /// A nudge was sent this episode and the worker stopped again.
    NudgeFailed,
    /// The execution was lost and the resume failed.
    ResumeFailed,
    /// The worker finished; someone should review the result.
    Completed,
    /// The worker stopped at its prompt and nothing allowed a nudge: it
    /// waits for its next instruction.
    Idle,
}

/// One escalation, identified so a digest can mark it delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Escalation {
    pub(crate) session_id: String,
    pub(crate) id: String,
    pub(crate) why: Why,
    pub(crate) line: String,
}

impl Escalation {
    /// Marker written into the digest; its presence in a delivered digest
    /// means the supervisor was told.
    pub(crate) fn marker(&self) -> String {
        format!("[spawn:esc {} {}]", self.session_id, self.id)
    }
}

/// One command to submit.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Order {
    pub(crate) agent_id: String,
    pub(crate) session_id: String,
    pub(crate) key: String,
    pub(crate) action: CommandAction,
}

/// What one dispatcher step does.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Plan {
    pub(crate) orders: Vec<Order>,
    /// Escalations not delivered yet, whether or not a digest goes out now.
    pub(crate) pending: Vec<Escalation>,
}

fn sent<'a>(facts: &'a Facts, key: &str) -> Option<&'a Sent> {
    facts.dispatched.iter().find(|sent| sent.key == key)
}

fn percent(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_string(), |value| format!("{:.0}%", value * 100.0))
}

fn describe(observation: &Observation) -> String {
    let mut parts = vec![format!(
        "{} by {} ({:.0}%)",
        observation.state.key(),
        observation.judge,
        observation.confidence.value() * 100.0
    )];
    if let Some(ambiguity) = observation.ambiguity {
        parts.push(format!("rules could not decide: {ambiguity:?}"));
    }
    if observation.needs_human.is_some() || observation.stuck.is_some() {
        parts.push(format!(
            "needs_human {} stuck {}",
            percent(observation.needs_human),
            percent(observation.stuck)
        ));
    }
    if observation.reason == Some(WaitReason::JudgeUnavailable) {
        parts.push("Jev unreachable, rules' guess".to_string());
    }
    parts.join("; ")
}

/// Whether Jev's judgement allows the one reversible nudge.
fn nudgeable(observation: &Observation) -> bool {
    observation.judge == "jev"
        && observation.state == SemanticState::Waiting
        && observation.ambiguity == Some(Ambiguity::TurnStoppedIdle)
        && !observation.attention
        && observation.confidence.value() >= NUDGE_CONFIDENCE
        && observation
            .needs_human
            .is_some_and(|value| value <= NUDGE_NEEDS_HUMAN)
        && observation.stuck.is_none_or(|value| value < NUDGE_STUCK)
}

fn order(facts: &Facts, key: String, action: CommandAction) -> Order {
    Order {
        agent_id: facts.agent_id.clone(),
        session_id: facts.session_id.clone(),
        key,
        action,
    }
}

fn escalate(facts: &Facts, id: String, why: Why, line: String) -> Escalation {
    Escalation {
        session_id: facts.session_id.clone(),
        id,
        why,
        line,
    }
}

/// Rule: a lost execution is resumed once; a failed resume escalates.
fn resume_lost(facts: &Facts) -> (Option<Order>, Option<Escalation>) {
    let execution = facts.execution_id.as_deref().unwrap_or("none");
    let key = format!("dispatch-resume-{execution}");
    match sent(facts, &key) {
        None => (
            Some(order(
                facts,
                key,
                CommandAction::Resume {
                    prompt: Some(
                        "Spawn restarted your engine after it was lost. Check where you were \
                         and continue your task."
                            .to_string(),
                    ),
                },
            )),
            None,
        ),
        Some(resume) if resume.succeeded == Some(false) => (
            None,
            Some(escalate(
                facts,
                format!("resume-{execution}"),
                Why::ResumeFailed,
                "engine was lost and the automatic resume failed".to_string(),
            )),
        ),
        Some(_) => (None, None),
    }
}

/// The guarded nudge, for a worker Jev judged idle and fine: `continue`
/// once per episode within the hourly limit; a nudge that did not help, or
/// the limit, escalates.
fn nudge(
    facts: &Facts,
    observation: &Observation,
    now_ms: i64,
) -> (Option<Order>, Option<Escalation>) {
    let episode = facts.episode.unwrap_or(0);
    let judged_at = facts.observation_at_ms.unwrap_or(0);
    let key = format!("dispatch-nudge-{}-{episode}", facts.session_id);
    match sent(facts, &key) {
        None => {
            let recent = facts
                .dispatched
                .iter()
                .filter(|sent| {
                    sent.key.starts_with("dispatch-nudge-")
                        && now_ms.saturating_sub(sent.created_at_ms) < 3_600_000
                })
                .count();
            if recent < NUDGES_PER_HOUR {
                let text = "continue".to_string();
                return (Some(order(facts, key, CommandAction::Input { text })), None);
            }
            (
                None,
                Some(escalate(
                    facts,
                    format!("nudges-{episode}"),
                    Why::NudgeFailed,
                    format!(
                        "stopped again; nudge limit of {NUDGES_PER_HOUR}/hour reached ({})",
                        describe(observation)
                    ),
                )),
            )
        }
        // Judged idle again after the nudge landed: it did not help.
        Some(sent) if sent.finished_at_ms.is_some_and(|done| judged_at > done) => (
            None,
            Some(escalate(
                facts,
                format!("nudge-{episode}"),
                Why::NudgeFailed,
                format!("stopped again after `continue` ({})", describe(observation)),
            )),
        ),
        Some(_) => (None, None),
    }
}

/// The policy for one worker: at most one order and at most one escalation.
fn decide_worker(facts: &Facts, now_ms: i64) -> (Option<Order>, Option<Escalation>) {
    // Something is in flight; its outcome is the next fact.
    if facts.open_command {
        return (None, None);
    }
    // 1. Rules over facts.
    if !facts.live && facts.end_cause.as_deref() == Some("lost") {
        return resume_lost(facts);
    }
    let Some(observation) = &facts.observation else {
        return (None, None);
    };
    // A judgement older than the latest fact describes a moment that has
    // passed (for example the ready screen just before the prompt was typed).
    // Act only on a current one; the Observer is about to judge again.
    if facts
        .evidence_seq
        .is_some_and(|latest| observation.based_on_seq.0 < latest)
    {
        return (None, None);
    }
    // 2. The one reversible action a Jev judgement may trigger.
    if facts.live && nudgeable(observation) {
        return nudge(facts, observation, now_ms);
    }
    // 3. Everything else that matters escalates.
    if observation.attention {
        let id = facts.observation_event_id.map_or_else(
            || format!("seq-{}", observation.based_on_seq.0),
            |id| id.to_string(),
        );
        return (
            None,
            Some(escalate(facts, id, Why::Attention, describe(observation))),
        );
    }
    let episode = facts.episode.unwrap_or(0);
    // Stopped at its prompt and not nudged: someone has to give the next
    // step or review the result. Once per episode.
    if facts.live && observation.ambiguity == Some(Ambiguity::TurnStoppedIdle) {
        let (why, line) = if observation.state == SemanticState::Completed {
            (Why::Completed, "finished its task")
        } else {
            (Why::Idle, "stopped at its prompt")
        };
        return (
            None,
            Some(escalate(
                facts,
                format!("idle-{episode}"),
                why,
                format!("{line} ({})", describe(observation)),
            )),
        );
    }
    // An execution someone stopped ended by their decision; telling them
    // again is noise.
    if !facts.live
        && observation.state == SemanticState::Completed
        && facts.episode_action.as_deref() != Some("stop")
    {
        return (
            None,
            Some(escalate(
                facts,
                format!("done-{episode}"),
                Why::Completed,
                format!("execution ended, task finished ({})", describe(observation)),
            )),
        );
    }
    (None, None)
}

/// Escalations the supervisor already received, from the markers in the
/// digests that reached its pane.
fn delivered(supervisor: Option<&Facts>) -> BTreeSet<String> {
    supervisor
        .map(|facts| {
            facts
                .dispatched
                .iter()
                .filter(|sent| sent.succeeded == Some(true))
                .filter_map(|sent| sent.text.as_deref())
                .flat_map(|text| {
                    text.lines()
                        .filter_map(|line| {
                            let start = line.find("[spawn:esc ")?;
                            let end = line[start..].find(']')? + start + 1;
                            Some(line[start..end].to_string())
                        })
                        .collect::<Vec<_>>()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Whether the supervisor can take a digest now: live, idle at its prompt,
/// nothing in flight.
fn supervisor_idle(supervisor: &Facts) -> bool {
    supervisor.live
        && !supervisor.open_command
        && supervisor.observation.as_ref().is_some_and(|observation| {
            matches!(
                observation.state,
                SemanticState::Waiting | SemanticState::Completed
            )
        })
}

fn fnv(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// The digest typed into the supervisor's pane.
fn digest(items: &[Escalation], spawn: &str) -> String {
    let mut text = format!(
        "Spawn dispatcher: {} worker{} need{} a decision. Check each with `{spawn} tail --session \
<session>` before acting.\n",
        items.len(),
        if items.len() == 1 { "" } else { "s" },
        if items.len() == 1 { "s" } else { "" },
    );
    for item in items {
        let kind = match item.why {
            Why::Attention => "attention",
            Why::NudgeFailed => "nudge did not help",
            Why::ResumeFailed => "resume failed",
            Why::Completed => "completed, review it",
            Why::Idle => "idle, give it the next step or review it",
        };
        let _ = writeln!(
            text,
            "- {} — {kind}: {} {}",
            item.session_id,
            item.line,
            item.marker()
        );
    }
    text
}

/// The whole policy as a pure function of the facts.
pub(crate) fn decide(rows: &[Facts], supervised: bool, now_ms: i64, spawn: &str) -> Plan {
    let supervisor = rows
        .iter()
        .find(|facts| facts.session_id == SUPERVISOR_SESSION);
    let told = delivered(supervisor);
    let mut plan = Plan::default();
    for facts in rows
        .iter()
        .filter(|facts| facts.agent_id.starts_with("agent_worker_"))
    {
        let (order, escalation) = decide_worker(facts, now_ms);
        plan.orders.extend(order);
        if let Some(escalation) = escalation
            && !told.contains(&escalation.marker())
        {
            plan.pending.push(escalation);
        }
    }
    let Some(supervisor) = supervisor.filter(|_| supervised) else {
        return plan;
    };
    if plan.pending.is_empty() || !supervisor_idle(supervisor) {
        return plan;
    }
    let items: Vec<Escalation> = plan.pending.iter().take(DIGEST_ITEMS).cloned().collect();
    let markers: String = items.iter().map(Escalation::marker).collect();
    plan.orders.push(Order {
        agent_id: SUPERVISOR_AGENT.to_string(),
        session_id: SUPERVISOR_SESSION.to_string(),
        // A new supervisor judgement allows one retry of an undelivered digest.
        key: format!(
            "dispatch-digest-{}-{:016x}",
            supervisor.observation_event_id.unwrap_or(0),
            fnv(&markers)
        ),
        action: CommandAction::Input {
            text: digest(&items, spawn),
        },
    });
    plan
}

/// The plan for the current facts, without submitting anything.
pub(crate) fn plan(store: &PostgresStore, supervised: bool, spawn: &str) -> Result<Plan, String> {
    let rows: Vec<Facts> =
        serde_json::from_value(store.dispatch_facts().map_err(|error| error.to_string())?)
            .map_err(|error| format!("dispatch facts: {error}"))?;
    let now = spawn_core::time::Clock::now(&spawn_core::time::SystemClock).0;
    Ok(decide(&rows, supervised, now, spawn))
}

/// Run one dispatcher step against the store. Returns log lines.
pub(crate) fn step(
    store: &PostgresStore,
    supervised: bool,
    spawn: &str,
) -> Result<Vec<String>, String> {
    let plan = plan(store, supervised, spawn)?;
    let principal = Principal::new(DISPATCHER).map_err(|error| error.to_string())?;
    let mut lines = Vec::new();
    for order in plan.orders {
        let submission = Submission {
            agent_id: AgentId::new(order.agent_id.clone()).map_err(|error| error.to_string())?,
            session_id: SessionId::new(order.session_id.clone())
                .map_err(|error| error.to_string())?,
            idempotency_key: IdempotencyKey::new(order.key.clone())
                .map_err(|error| error.to_string())?,
            action: order.action,
        };
        store
            .submit(&principal, &submission)
            .map_err(|error| error.to_string())?;
        lines.push(format!("dispatch: {} -> {}", order.key, order.session_id));
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::{Facts, Sent, Why, decide};
    use spawn_core::command::CommandAction;
    use spawn_core::event::SessionSeq;
    use spawn_core::observation::{Ambiguity, Confidence, Observation, SemanticState};

    const NOW: i64 = 10_000_000;

    fn judged(state: SemanticState, confidence: f64) -> Observation {
        let mut observation = Observation::basic(
            state,
            None,
            SessionSeq(9),
            Confidence::new(confidence).expect("valid"),
            "jev",
        );
        observation.ambiguity = Some(Ambiguity::TurnStoppedIdle);
        observation.needs_human = Some(0.05);
        observation.stuck = Some(0.1);
        observation
    }

    fn worker(observation: Observation) -> Facts {
        Facts {
            session_id: "session_worker_a".to_string(),
            agent_id: "agent_worker_a".to_string(),
            live: true,
            execution_id: Some("exec_5".to_string()),
            end_cause: None,
            observation: Some(observation),
            observation_event_id: Some(100),
            observation_at_ms: Some(NOW - 1_000),
            evidence_seq: Some(9),
            episode: Some(7),
            episode_action: Some("start".to_string()),
            open_command: false,
            dispatched: Vec::new(),
        }
    }

    fn idle_supervisor() -> Facts {
        let mut observation = judged(SemanticState::Waiting, 0.9);
        observation.ambiguity = Some(Ambiguity::TurnStoppedIdle);
        Facts {
            session_id: "session_supervisor".to_string(),
            agent_id: "agent_supervisor".to_string(),
            live: true,
            execution_id: Some("exec_2".to_string()),
            end_cause: None,
            observation: Some(observation),
            observation_event_id: Some(50),
            observation_at_ms: Some(NOW - 5_000),
            evidence_seq: Some(9),
            episode: Some(1),
            episode_action: Some("start".to_string()),
            open_command: false,
            dispatched: Vec::new(),
        }
    }

    fn nudge_sent(finished_at_ms: i64) -> Sent {
        Sent {
            key: "dispatch-nudge-session_worker_a-7".to_string(),
            text: Some("continue".to_string()),
            created_at_ms: finished_at_ms - 500,
            finished_at_ms: Some(finished_at_ms),
            succeeded: Some(true),
        }
    }

    #[test]
    fn a_confident_idle_worker_gets_one_continue() {
        let plan = decide(
            &[worker(judged(SemanticState::Waiting, 0.9))],
            false,
            NOW,
            "spawn",
        );
        assert_eq!(plan.orders.len(), 1);
        assert_eq!(plan.orders[0].key, "dispatch-nudge-session_worker_a-7");
        assert_eq!(
            plan.orders[0].action,
            CommandAction::Input {
                text: "continue".to_string()
            }
        );
        assert!(plan.pending.is_empty());
    }

    #[test]
    fn a_judgement_older_than_the_latest_fact_is_not_acted_on() {
        // Live case: the ready screen before the prompt was judged idle, and
        // the prompt had already arrived when the dispatcher looked.
        let mut stale = worker(judged(SemanticState::Waiting, 0.9));
        stale.evidence_seq = Some(10);
        let plan = decide(&[stale], false, NOW, "spawn");
        assert_eq!(plan, super::Plan::default());
    }

    #[test]
    fn an_unsure_or_human_needing_worker_is_not_nudged() {
        let unsure = worker(judged(SemanticState::Waiting, 0.6));
        assert!(decide(&[unsure], false, NOW, "spawn").orders.is_empty());
        let mut asking = judged(SemanticState::Waiting, 0.9);
        asking.needs_human = Some(0.4);
        assert!(
            decide(&[worker(asking)], false, NOW, "spawn")
                .orders
                .is_empty()
        );
        let mut rules = judged(SemanticState::Waiting, 1.0);
        rules.judge = "rules".to_string();
        assert!(
            decide(&[worker(rules)], false, NOW, "spawn")
                .orders
                .is_empty()
        );
    }

    #[test]
    fn a_nudge_that_did_not_help_escalates_instead_of_repeating() {
        let mut facts = worker(judged(SemanticState::Waiting, 0.9));
        facts.dispatched.push(nudge_sent(NOW - 5_000));
        let plan = decide(&[facts], false, NOW, "spawn");
        assert!(plan.orders.is_empty(), "no second nudge in one episode");
        assert_eq!(plan.pending.len(), 1);
        assert_eq!(plan.pending[0].why, Why::NudgeFailed);
    }

    #[test]
    fn the_judgement_from_before_the_nudge_landed_waits() {
        let mut facts = worker(judged(SemanticState::Waiting, 0.9));
        facts.dispatched.push(nudge_sent(NOW));
        let plan = decide(&[facts], false, NOW, "spawn");
        assert!(plan.orders.is_empty());
        assert!(plan.pending.is_empty());
    }

    #[test]
    fn a_blocked_worker_reaches_an_idle_supervisor_once() {
        let mut blocked = judged(SemanticState::Blocked, 0.8);
        blocked.needs_human = Some(0.9);
        blocked.attention = true;
        let rows = [worker(blocked), idle_supervisor()];
        let plan = decide(&rows, true, NOW, "spawn");
        assert_eq!(plan.orders.len(), 1);
        let digest = &plan.orders[0];
        assert_eq!(digest.session_id, "session_supervisor");
        let CommandAction::Input { text } = &digest.action else {
            panic!("a digest is typed input");
        };
        assert!(text.contains("[spawn:esc session_worker_a 100]"));
        assert!(text.contains("needs_human 90%"));

        // Once delivered, the same escalation is not sent again.
        let mut supervisor = idle_supervisor();
        supervisor.dispatched.push(Sent {
            key: digest.key.clone(),
            text: Some(text.clone()),
            created_at_ms: NOW,
            finished_at_ms: Some(NOW),
            succeeded: Some(true),
        });
        let rows = [rows[0].clone(), supervisor];
        let plan = decide(&rows, true, NOW, "spawn");
        assert!(plan.orders.is_empty());
        assert!(plan.pending.is_empty());
    }

    #[test]
    fn a_busy_or_absent_supervisor_leaves_escalations_pending() {
        let mut blocked = judged(SemanticState::Blocked, 0.8);
        blocked.attention = true;
        let mut busy = idle_supervisor();
        busy.open_command = true;
        let plan = decide(&[worker(blocked.clone()), busy], true, NOW, "spawn");
        assert!(plan.orders.is_empty());
        assert_eq!(plan.pending.len(), 1, "it waits, coalescing");
        let plan = decide(&[worker(blocked)], true, NOW, "spawn");
        assert!(
            plan.orders.is_empty(),
            "no supervisor: the attention queue holds it"
        );
        assert_eq!(plan.pending.len(), 1);
    }

    #[test]
    fn a_lost_worker_is_resumed_once_then_escalated() {
        let mut lost = worker(judged(SemanticState::Unknown, 0.5));
        lost.live = false;
        lost.end_cause = Some("lost".to_string());
        let plan = decide(&[lost.clone()], false, NOW, "spawn");
        assert_eq!(plan.orders[0].key, "dispatch-resume-exec_5");
        lost.dispatched.push(Sent {
            key: "dispatch-resume-exec_5".to_string(),
            text: None,
            created_at_ms: NOW - 10,
            finished_at_ms: Some(NOW - 5),
            succeeded: Some(false),
        });
        let plan = decide(&[lost], false, NOW, "spawn");
        assert!(plan.orders.is_empty());
        assert_eq!(plan.pending[0].why, Why::ResumeFailed);
    }

    #[test]
    fn nudges_are_rate_limited_per_agent_hour() {
        let mut facts = worker(judged(SemanticState::Waiting, 0.9));
        for episode in 1..=3 {
            facts.dispatched.push(Sent {
                key: format!("dispatch-nudge-session_worker_a-{episode}"),
                text: Some("continue".to_string()),
                created_at_ms: NOW - 60_000,
                finished_at_ms: Some(NOW - 59_000),
                succeeded: Some(true),
            });
        }
        let plan = decide(&[facts], false, NOW, "spawn");
        assert!(plan.orders.is_empty());
        assert_eq!(plan.pending[0].why, Why::NudgeFailed);
    }

    #[test]
    fn the_supervisor_and_rsi_are_never_dispatched() {
        let mut rsi = worker(judged(SemanticState::Waiting, 0.9));
        rsi.agent_id = "agent_rsi".to_string();
        rsi.session_id = "session_rsi".to_string();
        let plan = decide(&[rsi, idle_supervisor()], true, NOW, "spawn");
        assert_eq!(plan, super::Plan::default());
    }

    #[test]
    fn an_idle_worker_too_unsure_to_nudge_is_reported() {
        // Live case: WAITING 0.64 / COMPLETED 0.32, needs_human 0.28.
        let mut idle = judged(SemanticState::Waiting, 0.67);
        idle.needs_human = Some(0.28);
        let plan = decide(&[worker(idle), idle_supervisor()], true, NOW, "spawn");
        assert_eq!(plan.orders.len(), 1);
        let CommandAction::Input { text } = &plan.orders[0].action else {
            panic!("digest");
        };
        assert!(text.contains("idle, give it the next step"));
        assert!(text.contains("[spawn:esc session_worker_a idle-7]"));
    }

    #[test]
    fn a_worker_someone_stopped_is_not_reported_back() {
        let mut stopped = worker(Observation::basic(
            SemanticState::Completed,
            None,
            SessionSeq(9),
            Confidence::new(1.0).expect("valid"),
            "rules",
        ));
        stopped.live = false;
        stopped.end_cause = Some("stopped".to_string());
        stopped.episode_action = Some("stop".to_string());
        let plan = decide(&[stopped, idle_supervisor()], true, NOW, "spawn");
        assert_eq!(plan, super::Plan::default());
    }

    #[test]
    fn a_completed_worker_is_reported_for_review() {
        let mut done = judged(SemanticState::Completed, 0.9);
        done.ambiguity = Some(Ambiguity::TurnStoppedIdle);
        let plan = decide(&[worker(done), idle_supervisor()], true, NOW, "spawn");
        assert_eq!(plan.orders.len(), 1);
        let CommandAction::Input { text } = &plan.orders[0].action else {
            panic!("digest");
        };
        assert!(text.contains("completed, review it"));
    }
}
