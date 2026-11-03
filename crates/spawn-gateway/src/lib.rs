//! Controlled SQL Gateway: authorization, idempotent writes, read views.
//!
//! The gateway is the only command write path. It validates a draft, appends
//! the corresponding event, and enqueues the command. Read access is served
//! from projected views or directly through SQL read-only views.

// Pedantic doc-bookkeeping lints: public items carry purposeful docs, but
// mandatory Errors/Panics sections and identifier backticks add noise.
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::doc_markdown
)]
#![allow(clippy::module_name_repetitions, clippy::must_use_candidate)]
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use spawn_projector::{AgentStateRow, InMemoryViewStore};
use spawn_store::{CommandQueue, EventLog, StoreError};
use spawn_types::clock::Clock;
use spawn_types::command::{
    CommandAction, CommandId, CommandRecord, CommandStatus, IdempotencyKey, Principal,
};
use spawn_types::error::ValidationError;
use spawn_types::event::{EventKind, EventSource, NewEvent};
use spawn_types::identifiers::{AgentId, SessionId};
use thiserror::Error;

/// Maximum accepted prompt or input length in bytes.
const MAX_PAYLOAD_BYTES: usize = 16 * 1024;

/// Failures returned before a command reaches the queue.
#[derive(Debug, Error)]
pub enum GatewayError {
    /// The principal lacks permission for the target agent.
    #[error("principal {principal} is not authorized for agent {agent}")]
    Unauthorized {
        /// Rejected principal.
        principal: String,
        /// Target agent.
        agent: String,
    },

    /// A text payload exceeded the configured limit.
    #[error("payload of {size} bytes exceeds limit {limit}")]
    PayloadTooLarge {
        /// Observed size.
        size: usize,
        /// Configured limit.
        limit: usize,
    },

    /// A domain field failed validation.
    #[error(transparent)]
    Validation(ValidationError),

    /// The storage backend rejected the write.
    #[error(transparent)]
    Store(StoreError),
}

/// Authorization decision port.
pub trait Authorizer: Send + Sync {
    /// Return whether a principal may command one agent.
    fn allows(&self, principal: &Principal, agent: &AgentId) -> bool;
}

/// Static allow-list authorization policy.
#[derive(Debug, Default, Clone)]
pub struct AllowList {
    entries: HashSet<(String, String)>,
}

impl AllowList {
    /// Create an empty policy.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Grant one principal access to one agent and return the builder.
    #[must_use]
    pub fn grant(mut self, principal: impl Into<String>, agent: impl Into<String>) -> Self {
        self.entries.insert((principal.into(), agent.into()));
        self
    }
}

impl Authorizer for AllowList {
    fn allows(&self, principal: &Principal, agent: &AgentId) -> bool {
        self.entries
            .contains(&(principal.name.clone(), agent.as_str().to_string()))
    }
}

/// Validated command draft before identity and lifecycle fields are assigned.
#[derive(Debug, Clone)]
pub struct CommandDraft {
    /// Target agent.
    pub agent_id: AgentId,
    /// Target session.
    pub session_id: SessionId,
    /// Caller idempotency key.
    pub idempotency_key: IdempotencyKey,
    /// Requested action.
    pub action: CommandAction,
}

/// Result of a submission attempt.
#[derive(Debug, Clone)]
pub struct SubmitResult {
    /// Stored command record.
    pub record: CommandRecord,
    /// False when an existing idempotent command was returned.
    pub inserted: bool,
}

/// Validating command gateway.
pub struct Gateway {
    queue: Arc<dyn CommandQueue>,
    log: Arc<dyn EventLog>,
    views: Arc<InMemoryViewStore>,
    authorizer: Arc<dyn Authorizer>,
    clock: Arc<dyn Clock>,
    command_counter: AtomicU64,
}

impl Gateway {
    /// Construct a gateway with its ports.
    #[must_use]
    pub fn new(
        queue: Arc<dyn CommandQueue>,
        log: Arc<dyn EventLog>,
        views: Arc<InMemoryViewStore>,
        authorizer: Arc<dyn Authorizer>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            queue,
            log,
            views,
            authorizer,
            clock,
            command_counter: AtomicU64::new(0),
        }
    }

    /// Validate and submit one command.
    ///
    /// # Errors
    /// Returns [`GatewayError`] for authorization, validation or storage
    /// failures.
    // Principal is a small identity value that this entry point stores into the
    // record, so taking it by value keeps the public API ergonomic.
    #[allow(clippy::needless_pass_by_value)]
    pub fn submit(
        &self,
        principal: Principal,
        draft: CommandDraft,
    ) -> Result<SubmitResult, GatewayError> {
        if !self.authorizer.allows(&principal, &draft.agent_id) {
            return Err(GatewayError::Unauthorized {
                principal: principal.name.clone(),
                agent: draft.agent_id.as_str().to_string(),
            });
        }
        validate_payload_size(&draft.action)?;

        let sequence = self.command_counter.fetch_add(1, Ordering::SeqCst) + 1;
        let command_id =
            CommandId::new(format!("cmd_{sequence}")).map_err(GatewayError::Validation)?;
        let now = self.clock.now();
        let record = CommandRecord {
            command_id: command_id.clone(),
            agent_id: draft.agent_id.clone(),
            session_id: draft.session_id.clone(),
            idempotency_key: draft.idempotency_key.clone(),
            principal: principal.clone(),
            action: draft.action,
            status: CommandStatus::Pending,
            submitted_at: now,
        };

        let before = self
            .queue
            .submit(record.clone())
            .map_err(GatewayError::Store)?;
        let inserted = before.command_id == command_id;
        self.log
            .append(NewEvent {
                agent_id: draft.agent_id,
                session_id: draft.session_id,
                execution_id: None,
                source: EventSource::Gateway,
                // Use the queue's authoritative command id so an idempotent retry
                // (which returns the original command) builds the same dedup key.
                source_event_id: Some(format!("gateway:accepted:{}", before.command_id.as_str())),
                kind: EventKind::CommandAccepted {
                    command_id: before.command_id.clone(),
                },
                artifact: None,
                occurred_at: Some(now),
            })
            .map_err(GatewayError::Store)?;
        Ok(SubmitResult {
            record: before,
            inserted,
        })
    }

    /// Read the latest projected state of one session.
    #[must_use]
    pub fn latest_state(&self, session: &SessionId) -> Option<AgentStateRow> {
        self.views.snapshot().states.get(session).cloned()
    }
}

fn validate_payload_size(action: &CommandAction) -> Result<(), GatewayError> {
    let size = match action {
        CommandAction::Spawn { prompt, .. } => prompt.len(),
        CommandAction::SendInput { text } => text.len(),
        CommandAction::Stop
        | CommandAction::Resume { .. }
        | CommandAction::ForkBranch { .. }
        | CommandAction::MergeBranch { .. } => 0,
    };
    if size > MAX_PAYLOAD_BYTES {
        return Err(GatewayError::PayloadTooLarge {
            size,
            limit: MAX_PAYLOAD_BYTES,
        });
    }
    Ok(())
}
