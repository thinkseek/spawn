//! Identifier and label newtypes.
//!
//! Spawn-owned identifiers carry a fixed prefix and use only `[A-Za-z0-9_-]`,
//! so they are safe as tmux session names, git branch names and
//! environment values. Opaque labels wrap values owned by other systems and
//! only reject empty, oversized or control-character input. Deserialization
//! runs the same validation as construction.

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Maximum byte length of any label.
pub const MAX_LABEL_BYTES: usize = 256;

/// Reasons a label is rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InvalidLabel {
    /// The label is empty or longer than [`MAX_LABEL_BYTES`].
    #[error("label length {0} is outside 1..=256 bytes")]
    Length(usize),
    /// The label contains a control character.
    #[error("label contains a control character")]
    Control,
    /// A Spawn identifier lacks its prefix, has an empty body or uses a
    /// character outside `[A-Za-z0-9_-]`.
    #[error("identifier must be `{0}` followed by [A-Za-z0-9_-]+")]
    Format(&'static str),
}

fn validate(raw: &str, prefix: &'static str) -> Result<(), InvalidLabel> {
    if raw.is_empty() || raw.len() > MAX_LABEL_BYTES {
        return Err(InvalidLabel::Length(raw.len()));
    }
    if raw.chars().any(char::is_control) {
        return Err(InvalidLabel::Control);
    }
    if prefix.is_empty() {
        return Ok(());
    }
    let body = raw
        .strip_prefix(prefix)
        .ok_or(InvalidLabel::Format(prefix))?;
    let valid_body = !body.is_empty()
        && body
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'));
    if valid_body {
        Ok(())
    } else {
        Err(InvalidLabel::Format(prefix))
    }
}

/// Check the `<workspace>:<path>` shape a workspace identifier requires.
fn check_workspace(raw: &str) -> Result<(), InvalidLabel> {
    let Some((sandbox, path)) = raw.split_once(':') else {
        return Err(InvalidLabel::Format("<sandbox>:<path>"));
    };
    if sandbox.is_empty() || !path.starts_with('/') {
        return Err(InvalidLabel::Format("<sandbox>:<path>"));
    }
    Ok(())
}

macro_rules! label {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        label!($(#[$meta])* $name, $prefix, |_raw| Ok(()));
    };
    ($(#[$meta:meta])* $name:ident, $prefix:literal, $check:expr) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// Validate and wrap a raw value.
            pub fn new(raw: impl Into<String>) -> Result<Self, InvalidLabel> {
                let raw = raw.into();
                validate(&raw, $prefix)?;
                ($check)(&raw)?;
                Ok(Self(raw))
            }

            /// Borrow the wire value.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidLabel;

            fn try_from(raw: String) -> Result<Self, Self::Error> {
                Self::new(raw)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

label!(
    /// Long-lived logical identity of an agent, for example `agent_001`.
    AgentId,
    "agent_"
);
label!(
    /// Continuous logical history of an agent, for example `session_001`.
    SessionId,
    "session_"
);
label!(
    /// One computation: one engine process in one terminal and one workspace.
    ExecutionId,
    "exec_"
);
label!(
    /// Name of the terminal session that hosts one execution.
    TerminalName,
    "spawn-"
);
label!(
    /// Branch name.
    BranchName,
    ""
);
label!(
    /// Commit identifier.
    CommitId,
    ""
);
label!(
    /// Workspace reference in `<workspace>:<path>` form.
    WorkspaceId,
    "",
    check_workspace
);
label!(
    /// Authenticated consumer that submitted a command.
    Principal,
    ""
);
label!(
    /// Caller key that makes command submission idempotent per principal.
    IdempotencyKey,
    ""
);

/// Store-assigned command number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CommandId(pub u64);

impl fmt::Display for CommandId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "cmd_{}", self.0)
    }
}

impl ExecutionId {
    /// Execution created by one command.
    ///
    /// The identifier is a pure function of the command, so a command that is
    /// retried after a lost lease maps to the same execution.
    pub fn for_command(command: CommandId) -> Self {
        Self(format!("exec_{}", command.0))
    }
}

impl TerminalName {
    /// Terminal that hosts one execution.
    pub fn for_execution(execution: &ExecutionId) -> Self {
        Self(format!("spawn-{}", execution.as_str()))
    }
}

impl BranchName {
    /// Working branch of an agent. Each agent owns one branch named after it.
    pub fn for_agent(agent: &AgentId) -> Self {
        Self(agent.as_str().to_string())
    }
}

impl WorkspaceId {
    /// Sandbox number, the part before the first `:`.
    pub fn sandbox(&self) -> &str {
        self.0.split_once(':').map_or("", |(sandbox, _)| sandbox)
    }

    /// Path inside the sandbox, the part after the first `:`.
    pub fn path(&self) -> &str {
        self.0
            .split_once(':')
            .map_or(self.0.as_str(), |(_, path)| path)
    }
}
