//! Controlled commands consumed by compute workers.

use serde::{Deserialize, Serialize};

use crate::event::{ENGINE, ModelProfile};
use crate::id::{AgentId, CommandId, IdempotencyKey, SessionId};

/// Command vocabulary accepted by the SQL gateway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommandAction {
    /// Start a new execution with a fresh native session.
    Start {
        /// Engine to launch: [`ENGINE`], the only one, which a command may
        /// leave out. A command naming another engine does not parse.
        #[serde(default = "engine", deserialize_with = "claude_only")]
        engine: String,
        /// First prompt.
        prompt: String,
        /// Parent session when this agent works for another agent.
        #[serde(default)]
        parent_session_id: Option<SessionId>,
        /// Where a new branch starts instead of the source's `main`: another
        /// agent of the same mission source (its branch head) or a full
        /// commit id. An agent whose branch exists keeps it, and the Bridge
        /// warns that `from` was ignored.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        /// Models the engine runs on. Unset: the profile the session last
        /// started with, or the default for a session that never ran.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<ModelProfile>,
        /// The hash of the standing instructions Spawn wrote into `prompt`
        /// (the supervisor's charter), when it wrote any: the record of
        /// which charter the native session this start begins was given
        /// (docs/fleet-architecture.md §3, the supervisor). Never read by
        /// the Bridge.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        charter: Option<String>,
    },
    /// Type text into the running Claude session.
    Input {
        /// Text to submit.
        text: String,
        /// What the sender files with this input to find it again (the
        /// dispatcher: the escalations a digest delivers). Never typed: the
        /// Bridge ignores them, and no reader takes them from the text, which
        /// may quote anyone.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tags: Vec<String>,
    },
    /// Stop the running execution after a final checkpoint. While the
    /// checkpoint fails the workspace stays mounted and the stop is retried.
    Stop,
    /// Stop the running execution, and unmount its workspace even when the
    /// final checkpoint fails: what it did not commit is lost. Only a person
    /// or the supervisor sends it, after `execution.end_stalled`, having
    /// decided the work can go.
    Discard,
    /// Press one named key in the running Claude session's terminal: Esc,
    /// an arrow, Enter, Y or N (`{"type":"key","key":"escape"}`). Its own
    /// action, never text: it is sent at once, in any pane state (Esc
    /// interrupts a turn, the others answer a dialog), at most once, and
    /// does not wait for the turn as an input does (`crate::bridge`).
    Key {
        /// The key.
        key: NamedKey,
    },
    /// Start a new execution that resumes the latest native session.
    Resume {
        /// Prompt for the resumed session.
        #[serde(default)]
        prompt: Option<String>,
        /// Models the resumed engine runs on. Unset: the profile the session
        /// last started with, so a resumed cheap worker stays cheap.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<ModelProfile>,
    },
}

/// A key a [`CommandAction::Key`] presses: the closed set a person may send
/// an agent from the page (backlog P1). Anything else does not parse, and
/// the error names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NamedKey {
    /// Esc: interrupts a running turn, closes a dialog.
    Escape,
    /// The up arrow.
    Up,
    /// The down arrow.
    Down,
    /// The left arrow.
    Left,
    /// The right arrow.
    Right,
    /// Enter: confirms a dialog's selection.
    Enter,
    /// The y key.
    Y,
    /// The n key.
    N,
}

impl NamedKey {
    /// Every key, in the order the page lists them.
    pub const ALL: [Self; 8] = [
        Self::Escape,
        Self::Up,
        Self::Down,
        Self::Left,
        Self::Right,
        Self::Enter,
        Self::Y,
        Self::N,
    ];

    /// The key's name in an action (`escape`, `up`, ..., `y`, `n`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Escape => "escape",
            Self::Up => "up",
            Self::Down => "down",
            Self::Left => "left",
            Self::Right => "right",
            Self::Enter => "enter",
            Self::Y => "y",
            Self::N => "n",
        }
    }

    /// The key as tmux's `send-keys` names it.
    pub fn tmux(self) -> &'static str {
        match self {
            Self::Escape => "Escape",
            Self::Up => "Up",
            Self::Down => "Down",
            Self::Left => "Left",
            Self::Right => "Right",
            Self::Enter => "Enter",
            Self::Y => "y",
            Self::N => "n",
        }
    }

    /// The key named `name`, or why there is none.
    pub fn from_name(name: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|key| key.name() == name)
            .ok_or_else(|| {
                let names: Vec<&str> = Self::ALL.iter().map(|key| key.name()).collect();
                format!("key {name:?} is not one of {}", names.join(", "))
            })
    }
}

impl<'de> Deserialize<'de> for NamedKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Self::from_name(&name).map_err(serde::de::Error::custom)
    }
}

/// The `engine` a start names when it names none.
fn engine() -> String {
    ENGINE.to_string()
}

/// The `engine` of a start: [`ENGINE`] only.
fn claude_only<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let engine = String::deserialize(deserializer)?;
    if engine == ENGINE {
        Ok(engine)
    } else {
        Err(serde::de::Error::custom(format!(
            "unknown engine {engine}: Claude Code ({ENGINE}) is the only engine"
        )))
    }
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
    /// Who submitted the command: an input from outside Spawn's own loops
    /// waits for its turn instead of being refused (`crate::bridge`).
    pub principal: String,
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

#[cfg(test)]
mod tests {
    use super::{CommandAction, NamedKey};

    #[test]
    fn a_key_action_names_one_key_of_the_closed_set() {
        for key in NamedKey::ALL {
            let json = format!(r#"{{"type":"key","key":"{}"}}"#, key.name());
            let action: CommandAction = serde_json::from_str(&json).expect("a named key parses");
            assert_eq!(action, CommandAction::Key { key });
            assert_eq!(serde_json::to_string(&action).expect("serializes"), json);
        }
        let tmux: Vec<&str> = NamedKey::ALL.iter().map(|key| key.tmux()).collect();
        assert_eq!(
            tmux,
            ["Escape", "Up", "Down", "Left", "Right", "Enter", "y", "n"]
        );
    }

    #[test]
    fn a_key_outside_the_set_does_not_parse_and_the_error_names_it() {
        let error = serde_json::from_str::<CommandAction>(r#"{"type":"key","key":"f1"}"#)
            .expect_err("f1 is no named key");
        let text = error.to_string();
        assert!(
            text.contains(r#"key "f1" is not one of escape, up, down, left, right, enter, y, n"#),
            "{text}"
        );
        // Names are exact: no case folding, no text, no second field.
        for json in [
            r#"{"type":"key","key":"Escape"}"#,
            r#"{"type":"key","key":"Y"}"#,
            r#"{"type":"key","key":""}"#,
            r#"{"type":"key"}"#,
            r#"{"type":"key","key":"y","text":"y"}"#,
            r#"{"type":"input","key":"y"}"#,
        ] {
            assert!(
                serde_json::from_str::<CommandAction>(json).is_err(),
                "{json} parsed"
            );
        }
    }
}
