use serde::{Deserialize, Serialize};

use crate::clock::Timestamp;
use crate::engine::Engine;
use crate::error::ValidationError;
use crate::identifiers::{AgentId, BranchName, SessionId};

/// Store-generated command identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CommandId(String);

impl CommandId {
    /// Construct from a wire value after label validation.
    pub fn new(raw: impl Into<String>) -> Result<Self, ValidationError> {
        let raw = raw.into();
        crate::identifiers::validate_opaque_label(&raw)?;
        Ok(Self(raw))
    }

    /// Borrow the wire value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Caller-supplied idempotency key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct IdempotencyKey(String);

impl IdempotencyKey {
    /// Construct and validate a caller key.
    pub fn new(raw: impl Into<String>) -> Result<Self, ValidationError> {
        let raw = raw.into();
        crate::identifiers::validate_opaque_label(&raw)?;
        Ok(Self(raw))
    }

    /// Borrow the key.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Authenticated consumer identity used by authorization rules.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Principal {
    /// Stable principal name.
    pub name: String,
}

/// Controlled command vocabulary accepted by the gateway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommandAction {
    /// Start a new execution on a session.
    Spawn {
        /// Engine to launch.
        engine: Engine,
        /// First user prompt.
        prompt: String,
    },
    /// Send input to the running native session.
    SendInput {
        /// Input text delivered to the terminal.
        text: String,
    },
    /// Request an orderly stop.
    Stop,
    /// Resume or rebuild the native session while keeping logical history.
    Resume {
        /// Prefer native resume when the engine supports it.
        prefer_native_resume: bool,
    },
    /// Fork a LayerFS branch for parallel work.
    ForkBranch {
        /// New branch name.
        branch: BranchName,
    },
    /// Merge a branch back into its source.
    MergeBranch {
        /// Branch to merge.
        source_branch: BranchName,
        /// Target branch that receives the merge.
        target_branch: BranchName,
    },
}

/// Monotonic fencing attempt for one command lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LeaseAttempt(pub u64);

/// Active lease held by one worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    /// Worker identity that holds the lease.
    pub worker: String,
    /// Fencing attempt; older attempts are rejected.
    pub attempt: LeaseAttempt,
    /// Instant at which the lease expires unless renewed.
    pub expires_at: Timestamp,
}

/// Lifecycle state of a command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum CommandStatus {
    /// Waiting for a worker.
    Pending,
    /// Leased by a worker.
    Leased {
        /// Current lease.
        lease: Lease,
    },
    /// Reached a terminal state.
    Finished {
        /// Worker-reported success flag.
        succeeded: bool,
        /// Completion instant.
        finished_at: Timestamp,
    },
}

/// One controlled command and its lifecycle state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandRecord {
    /// Store-unique command identifier.
    pub command_id: CommandId,
    /// Agent the command targets.
    pub agent_id: AgentId,
    /// Logical session the command targets.
    pub session_id: SessionId,
    /// Caller key used for idempotent submission.
    pub idempotency_key: IdempotencyKey,
    /// Principal that submitted the command.
    pub principal: Principal,
    /// Validated action.
    pub action: CommandAction,
    /// Current lifecycle state.
    pub status: CommandStatus,
    /// Submission instant.
    pub submitted_at: Timestamp,
}
