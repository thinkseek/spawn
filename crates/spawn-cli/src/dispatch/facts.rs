//! The dispatcher's vocabulary, shared by the worker ladder, the digest and
//! delivery: the facts read per session, and the orders and escalations a
//! step produces.

use serde::Deserialize;
use spawn_core::command::CommandAction;
use spawn_core::observation::{Ambiguity, Observation, SemanticState};

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
