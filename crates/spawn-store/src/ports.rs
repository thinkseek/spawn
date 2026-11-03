use std::time::Duration;

use thiserror::Error;

use spawn_types::clock::Timestamp;
use spawn_types::command::{CommandRecord, Lease, LeaseAttempt};
use spawn_types::event::{
    AppendOutcome, EventKind, EventRecord, GlobalPosition, NewEvent, SessionCursor, SessionSeq,
};
use spawn_types::identifiers::SessionId;

/// Failures declared by storage port contracts.
#[derive(Debug, Error)]
pub enum StoreError {
    /// A referenced command or event did not exist.
    #[error("entity not found: {0}")]
    NotFound(String),

    /// The caller did not hold the current lease attempt.
    #[error("lease lost for command {command_id}: expected {expected}, got {actual}")]
    LeaseLost {
        /// Command whose lease was checked.
        command_id: String,
        /// Attempt the store currently accepts.
        expected: u64,
        /// Attempt the caller presented.
        actual: u64,
    },

    /// The command was already in a terminal or incompatible state.
    #[error("invalid command state: {0}")]
    InvalidState(String),

    /// A uniqueness or ordering invariant was violated.
    #[error("storage conflict: {0}")]
    Conflict(String),

    /// The backend was temporarily unavailable.
    #[error("storage unavailable: {0}")]
    Unavailable(String),
}

/// Append-only event log with per-session and global ordering.
pub trait EventLog: Send + Sync {
    /// Append one event, assigning identity and ordering.
    ///
    /// When `source_event_id` is present, a repeated `(session, source, key)`
    /// tuple returns [`AppendOutcome::Deduplicated`] with the stored record.
    ///
    /// # Errors
    /// Fails with [`StoreError`] when the backend cannot durably commit.
    fn append(&self, event: NewEvent) -> Result<AppendOutcome, StoreError>;

    /// Read one session starting after `since`, bounded by `limit`.
    fn read_session(
        &self,
        session: &SessionId,
        since: SessionSeq,
        limit: usize,
    ) -> Result<Vec<EventRecord>, StoreError>;

    /// Read the global stream starting after `since`, bounded by `limit`.
    fn read_global(
        &self,
        since: GlobalPosition,
        limit: usize,
    ) -> Result<Vec<EventRecord>, StoreError>;

    /// Return the current cursor of every session.
    fn session_cursors(&self) -> Result<Vec<SessionCursor>, StoreError>;
}

/// Leased command queue consumed by compute workers.
pub trait CommandQueue: Send + Sync {
    /// Submit a validated command.
    ///
    /// A repeated idempotency key for the same principal returns the existing
    /// record instead of inserting a second command.
    fn submit(&self, command: CommandRecord) -> Result<CommandRecord, StoreError>;

    /// Claim the next eligible command for a worker.
    ///
    /// At most one command per session is leased at a time. Expired leases are
    /// reclaimed and receive a fresh fencing attempt.
    fn claim(
        &self,
        worker: &str,
        now: Timestamp,
        ttl: Duration,
    ) -> Result<Option<CommandRecord>, StoreError>;

    /// Extend a held lease when the attempt still matches.
    fn renew(
        &self,
        command_id: &str,
        attempt: LeaseAttempt,
        now: Timestamp,
        ttl: Duration,
    ) -> Result<Lease, StoreError>;

    /// Move a leased command to its terminal state.
    fn finish(
        &self,
        command_id: &str,
        attempt: LeaseAttempt,
        succeeded: bool,
        now: Timestamp,
    ) -> Result<CommandRecord, StoreError>;
}

/// Helpers used by storage implementations and consumers.
#[allow(dead_code)]
#[must_use]
pub fn is_command_event(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::CommandAccepted { .. }
            | EventKind::CommandClaimed { .. }
            | EventKind::CommandFinished { .. }
    )
}
