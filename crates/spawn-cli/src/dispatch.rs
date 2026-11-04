//! Dispatcher: the control layer's deterministic policy for workers.
//!
//! The Observer says what state a worker is in; this decides what to do
//! about it, on the decision ladder of docs/fleet-architecture.md §3:
//!
//! 1. Rules over facts decide fixed cases: a lost execution is resumed once.
//! 2. One guarded nudge: when Jev judged that a worker stopped at an idle
//!    prompt without needing anyone (WAITING, confident, `needs_human` and
//!    `stuck` low), it gets `continue` typed in, once per episode and at most
//!    [`NUDGES_PER_HOUR`](worker::NUDGES_PER_HOUR) times per agent-hour.
//!    Jev's judgement is an input, never the chooser of the action, and the
//!    action is reversible.
//! 3. Everything else escalates: attention, a nudge that did not help, a
//!    resume that failed, and completions (so work gets reviewed). Escalations
//!    are coalesced into one digest typed into the supervisor's pane while it
//!    is idle. Without a supervisor they stay in the attention queue.
//!
//! The dispatcher keeps no state. What it already did is read back from the
//! command queue: idempotency keys mark nudges and resumes, and a marker per
//! escalation in each delivered digest marks what the supervisor was told.
//! It commands only `agent_worker_*` sessions, plus digests to the supervisor.
//!
//! Layout: this file holds the shared vocabulary (facts, orders,
//! escalations) and the step; [`worker`] is the per-worker ladder and
//! [`digest`] the supervisor digest. Task delivery (the mission layer's
//! rules) lives in [`crate::deliver`]; this module combines both into one
//! [`Plan`] ([`decide_with`]) and submits it ([`step`]).

mod digest;
mod worker;

use std::path::Path;

use serde::Deserialize;
use spawn_core::command::{CommandAction, Submission};
use spawn_core::id::{AgentId, IdempotencyKey, Principal, SessionId};
use spawn_core::observation::{Ambiguity, Observation, SemanticState};
use spawn_core::port::CommandQueue;
use spawn_postgres::PostgresStore;

use crate::deliver::{self, Work};
use crate::supervise::SUPERVISOR_SESSION;

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

impl Facts {
    /// The command the dispatcher sent under `key`, if any.
    pub(crate) fn sent(&self, key: &str) -> Option<&Sent> {
        self.dispatched.iter().find(|sent| sent.key == key)
    }

    /// The latest judgement, unless it is older than the latest fact.
    ///
    /// A judgement older than the latest fact describes a moment that has
    /// passed (for example the ready screen just before the prompt was
    /// typed); the Observer is about to judge again.
    pub(crate) fn current_observation(&self) -> Option<&Observation> {
        self.observation.as_ref().filter(|observation| {
            self.evidence_seq
                .is_none_or(|latest| observation.based_on_seq.0 >= latest)
        })
    }

    /// Live, with no command in flight.
    pub(crate) fn live_and_free(&self) -> bool {
        self.live && !self.open_command
    }

    /// Whether the worker sits idle at its prompt, judged on its latest
    /// fact, with nothing in flight: the only moment typed input is safe.
    /// The turn stopped and the pane waits at its prompt, whatever Jev made
    /// of it: a fact the rules settle. A report written by then is the
    /// worker's answer even when it asks a question (judged BLOCKED).
    pub(crate) fn turn_stopped_idle(&self) -> bool {
        self.live_and_free()
            && self.current_observation().is_some_and(|observation| {
                observation.ambiguity == Some(Ambiguity::TurnStoppedIdle)
            })
    }

    pub(crate) fn idle_at_prompt(&self) -> bool {
        self.live_and_free()
            && self.current_observation().is_some_and(|observation| {
                observation.ambiguity == Some(Ambiguity::TurnStoppedIdle)
                    && matches!(
                        observation.state,
                        SemanticState::Waiting | SemanticState::Completed
                    )
            })
    }
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
    /// A new mission is in the inbox.
    Mission,
    /// A worker reported on its task.
    Report,
    /// A human approved a resident.
    Approved,
    /// A human wrote to a worker directly.
    Human,
    /// The command that delivered a task failed.
    DeliveryFailed,
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
    /// An escalation about the session `facts` describe.
    pub(crate) fn about(facts: &Facts, id: String, why: Why, line: String) -> Self {
        Self {
            session_id: facts.session_id.clone(),
            id,
            why,
            line,
        }
    }

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
    /// The task this command delivers, if any.
    pub(crate) task: Option<i64>,
}

impl Order {
    /// A command to the session `facts` describe, delivering no task.
    pub(crate) fn to(facts: &Facts, key: String, action: CommandAction) -> Self {
        Self {
            agent_id: facts.agent_id.clone(),
            session_id: facts.session_id.clone(),
            key,
            action,
            task: None,
        }
    }
}

/// What one dispatcher step does.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Plan {
    pub(crate) orders: Vec<Order>,
    /// Escalations not delivered yet, whether or not a digest goes out now.
    pub(crate) pending: Vec<Escalation>,
}

/// Principal the dispatcher submits as.
pub(crate) const DISPATCHER: &str = "spawn-dispatch";

/// Whether the dispatcher's worker ladder applies to this agent.
fn is_worker(facts: &Facts) -> bool {
    facts.agent_id.starts_with("agent_worker_") || facts.agent_id.starts_with("agent_resident_")
}

/// The whole policy as a pure function of the facts, without missions.
#[cfg(test)]
pub(crate) fn decide(rows: &[Facts], supervised: bool, now_ms: i64, spawn: &str) -> Plan {
    decide_with(rows, &Work::default(), supervised, now_ms, spawn)
}

/// The whole policy as a pure function of the facts and the mission rows:
/// delivery orders first, then the worker ladder, then at most one digest.
pub(crate) fn decide_with(
    rows: &[Facts],
    work: &Work,
    supervised: bool,
    now_ms: i64,
    spawn: &str,
) -> Plan {
    let supervisor = rows
        .iter()
        .find(|facts| facts.session_id == SUPERVISOR_SESSION);
    let told = digest::told(supervisor);
    let mut plan = Plan {
        orders: deliver::orders(work, rows),
        pending: Vec::new(),
    };
    let in_flight = work.in_flight();
    for facts in rows.iter().filter(|facts| is_worker(facts)) {
        // A managed worker between tasks is not news: its report already
        // was, and the supervisor decides what comes next.
        if work.managed(&facts.session_id) && !in_flight.contains(facts.session_id.as_str()) {
            continue;
        }
        match worker::decide_worker(facts, now_ms) {
            worker::Decision::Nothing => {}
            worker::Decision::Act(order) => plan.orders.push(order),
            worker::Decision::Escalate(escalation) => plan.pending.push(escalation),
        }
    }
    plan.pending.extend(deliver::news_items(work, rows));
    plan.pending.retain(|item| !told.contains(&item.marker()));
    if let Some(supervisor) = supervisor.filter(|_| supervised) {
        plan.orders
            .extend(digest::order(supervisor, &plan.pending, spawn));
    }
    plan
}

/// Facts and mission rows, read once.
fn load(store: &PostgresStore) -> Result<(Vec<Facts>, Work), String> {
    let rows: Vec<Facts> =
        serde_json::from_value(store.dispatch_facts().map_err(|error| error.to_string())?)
            .map_err(|error| format!("dispatch facts: {error}"))?;
    Ok((rows, Work::load(store)?))
}

/// The plan for the current facts, without submitting anything.
pub(crate) fn plan(store: &PostgresStore, supervised: bool, spawn: &str) -> Result<Plan, String> {
    let (rows, work) = load(store)?;
    Ok(decide_with(&rows, &work, supervised, now_ms(), spawn))
}

fn now_ms() -> i64 {
    spawn_core::time::Clock::now(&spawn_core::time::SystemClock).0
}

/// Submit one order as the dispatcher; an order that delivers a task marks
/// the task delivered with the command's id.
fn submit(store: &PostgresStore, principal: &Principal, order: Order) -> Result<(), String> {
    let submission = Submission {
        agent_id: AgentId::new(order.agent_id).map_err(|error| error.to_string())?,
        session_id: SessionId::new(order.session_id).map_err(|error| error.to_string())?,
        idempotency_key: IdempotencyKey::new(order.key).map_err(|error| error.to_string())?,
        action: order.action,
    };
    let command = store
        .submit(principal, &submission)
        .map_err(|error| error.to_string())?;
    if let Some(task) = order.task {
        store
            .task_delivered(task, i64::try_from(command.0).unwrap_or(i64::MAX))
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Run one dispatcher step against the store: collect reports from idle
/// workers under `root`, then submit the plan. Returns log lines.
pub(crate) fn step(
    store: &PostgresStore,
    supervised: bool,
    spawn: &str,
    root: Option<&Path>,
) -> Result<Vec<String>, String> {
    let (mut rows, mut work) = load(store)?;
    let mut lines = root.map_or_else(Vec::new, |root| {
        deliver::collect_reports(store, &work, &rows, root)
    });
    if !lines.is_empty() {
        // Collecting a report changed a task's state.
        (rows, work) = load(store)?;
    }
    let plan = decide_with(&rows, &work, supervised, now_ms(), spawn);
    let principal = Principal::new(DISPATCHER).map_err(|error| error.to_string())?;
    for order in plan.orders {
        let line = format!("dispatch: {} -> {}", order.key, order.session_id);
        submit(store, &principal, order)?;
        lines.push(line);
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

    #[test]
    fn a_blocked_worker_at_its_prompt_has_its_report_collected_but_gets_no_new_work() {
        let blocked = worker(judged(SemanticState::Blocked, 0.9));
        assert!(blocked.turn_stopped_idle(), "its report is collected");
        assert!(!blocked.idle_at_prompt(), "but nothing new is delivered");
        let mut running = judged(SemanticState::Working, 0.9);
        running.ambiguity = None;
        assert!(
            !worker(running).turn_stopped_idle(),
            "a running turn is not idle"
        );
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
    fn a_lost_resident_is_left_to_the_liveness_rule() {
        let mut lost = worker(judged(SemanticState::Unknown, 0.5));
        lost.agent_id = "agent_resident_core".to_string();
        lost.session_id = "session_resident_core".to_string();
        lost.live = false;
        lost.end_cause = Some("lost".to_string());
        assert!(decide(&[lost], false, NOW, "spawn").orders.is_empty());
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

    #[test]
    fn a_supervisor_judged_before_its_latest_fact_gets_no_digest() {
        let mut blocked = judged(SemanticState::Blocked, 0.8);
        blocked.attention = true;
        let mut busy = idle_supervisor();
        busy.evidence_seq = Some(10);
        let plan = decide(&[worker(blocked), busy], true, NOW, "spawn");
        assert!(plan.orders.is_empty());
        assert_eq!(plan.pending.len(), 1);
    }
}
