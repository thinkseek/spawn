//! Controlled commands consumed by compute workers.

use serde::{Deserialize, Serialize};

use crate::event::Engine;
use crate::id::{AgentId, CommandId, IdempotencyKey, SessionId};

/// Command vocabulary accepted by the SQL gateway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommandAction {
    /// Start a new execution with a fresh native session.
    Start {
        /// Engine to launch.
        engine: Engine,
        /// First prompt. Codex requires a non-empty prompt.
        prompt: String,
        /// Parent session when this agent works for another agent.
        #[serde(default)]
        parent_session_id: Option<SessionId>,
    },
    /// Type text into the running Claude session.
    Input {
        /// Text to submit.
        text: String,
    },
    /// Stop the running execution after a final checkpoint.
    Stop,
    /// Start a new execution that resumes the latest native session.
    Resume {
        /// Prompt for the resumed session. Codex requires one.
        #[serde(default)]
        prompt: Option<String>,
    },
}

/// Command submission as a store receives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submission {
    /// Target agent.
    pub agent_id: AgentId,
    /// Target session.
    pub session_id: SessionId,
    /// Caller key; a repeated key for the same principal returns the first command.
    pub idempotency_key: IdempotencyKey,
    /// Requested action.
    pub action: CommandAction,
}

/// A command leased by one worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claimed {
    /// Command identifier.
    pub command_id: CommandId,
    /// Fencing attempt. A finish with an older attempt is rejected.
    pub attempt: u32,
    /// Target agent.
    pub agent_id: AgentId,
    /// Target session.
    pub session_id: SessionId,
    /// Raw action as stored. Parse with [`Claimed::action`].
    pub action: serde_json::Value,
}

impl Claimed {
    /// Parse the stored action.
    pub fn action(&self) -> Result<CommandAction, serde_json::Error> {
        serde_json::from_value(self.action.clone())
    }
}

/// Worker result of one command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The command took effect.
    Succeeded,
    /// The command was rejected or failed.
    Failed(String),
}
