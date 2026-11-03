use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use spawn_types::clock::{Clock, Timestamp};
use spawn_types::command::{
    CommandId, CommandRecord, CommandStatus, IdempotencyKey, Lease, LeaseAttempt,
};
use spawn_types::event::{
    AppendOutcome, EventId, EventRecord, EventSource, GlobalPosition, NewEvent, SessionCursor,
    SessionSeq,
};
use spawn_types::identifiers::SessionId;

use crate::ports::{CommandQueue, EventLog, StoreError};

#[derive(Default)]
struct SessionState {
    events: Vec<EventRecord>,
    last_occurred_at: Timestamp,
}

#[derive(Default)]
struct InnerState {
    global: Vec<EventRecord>,
    sessions: HashMap<SessionId, SessionState>,
    dedup_keys: HashSet<(SessionId, EventSource, String)>,
    commands: HashMap<CommandId, CommandRecord>,
    idempotency: HashMap<IdempotencyKey, CommandId>,
    leased_sessions: HashSet<SessionId>,
}

/// In-memory storage implementing both storage ports behind one lock.
#[derive(Default)]
pub struct InMemoryStore {
    clock: Option<std::sync::Arc<dyn Clock>>,
    inner: Mutex<InnerState>,
}

impl InMemoryStore {
    /// Create an empty store. Missing event timestamps fail until a clock is
    /// supplied through [`Self::with_clock`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach the clock used to fill missing producer timestamps.
    #[must_use]
    pub fn with_clock(mut self, clock: std::sync::Arc<dyn Clock>) -> Self {
        self.clock = Some(clock);
        self
    }

    fn resolve_timestamp(&self, supplied: Option<Timestamp>) -> Result<Timestamp, StoreError> {
        match supplied {
            Some(timestamp) => Ok(timestamp),
            None => self
                .clock
                .as_ref()
                .map(spawn_types::clock::Clock::now)
                .ok_or_else(|| StoreError::InvalidState("no clock configured for store".into())),
        }
    }
}

impl EventLog for InMemoryStore {
    fn append(&self, event: NewEvent) -> Result<AppendOutcome, StoreError> {
        let mut state = self.inner.lock().expect("store lock is not poisoned");

        if let Some(key) = &event.source_event_id {
            let dedup = (event.session_id.clone(), event.source, key.clone());
            if let Some(existing) = state
                .sessions
                .get(&event.session_id)
                .and_then(|session| {
                    session
                        .events
                        .iter()
                        .find(|record| record.event.source_event_id.as_deref() == Some(key))
                })
                .cloned()
            {
                return Ok(AppendOutcome::Deduplicated(existing));
            }
            state.dedup_keys.insert(dedup);
        }

        let position = u64::try_from(state.global.len())
            .map_err(|_| StoreError::Conflict("global position exceeded u64 bounds".into()))?
            + 1;
        let session = state.sessions.entry(event.session_id.clone()).or_default();
        let seq = u64::try_from(session.events.len())
            .map_err(|_| StoreError::Conflict("session sequence exceeded u64 bounds".into()))?
            + 1;
        let recorded_at = self.resolve_timestamp(event.occurred_at)?;
        let record = EventRecord {
            event_id: EventId::new(format!("evt_{position}")).expect("generated event id is valid"),
            session_seq: SessionSeq(seq),
            global_position: GlobalPosition(position),
            recorded_at,
            event,
        };
        session.last_occurred_at = recorded_at;
        session.events.push(record.clone());
        state.global.push(record.clone());
        Ok(AppendOutcome::Recorded(record))
    }

    fn read_session(
        &self,
        session: &SessionId,
        since: SessionSeq,
        limit: usize,
    ) -> Result<Vec<EventRecord>, StoreError> {
        let state = self.inner.lock().expect("store lock is not poisoned");
        Ok(state
            .sessions
            .get(session)
            .map(|session_state| {
                session_state
                    .events
                    .iter()
                    .filter(|record| record.session_seq.0 > since.0)
                    .take(limit)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    fn read_global(
        &self,
        since: GlobalPosition,
        limit: usize,
    ) -> Result<Vec<EventRecord>, StoreError> {
        let state = self.inner.lock().expect("store lock is not poisoned");
        Ok(state
            .global
            .iter()
            .filter(|record| record.global_position.0 > since.0)
            .take(limit)
            .cloned()
            .collect())
    }

    fn session_cursors(&self) -> Result<Vec<SessionCursor>, StoreError> {
        let state = self.inner.lock().expect("store lock is not poisoned");
        Ok(state
            .sessions
            .iter()
            .map(|(session_id, session_state)| SessionCursor {
                session_id: session_id.clone(),
                last_seq: SessionSeq(session_state.events.len() as u64),
                last_occurred_at: session_state.last_occurred_at,
            })
            .collect())
    }
}

impl CommandQueue for InMemoryStore {
    fn submit(&self, command: CommandRecord) -> Result<CommandRecord, StoreError> {
        let mut state = self.inner.lock().expect("store lock is not poisoned");
        if let Some(existing_id) = state.idempotency.get(&command.idempotency_key) {
            return state
                .commands
                .get(existing_id)
                .cloned()
                .ok_or_else(|| StoreError::NotFound("idempotent command missing".into()));
        }
        state
            .idempotency
            .insert(command.idempotency_key.clone(), command.command_id.clone());
        state
            .commands
            .insert(command.command_id.clone(), command.clone());
        Ok(command)
    }

    fn claim(
        &self,
        worker: &str,
        now: Timestamp,
        ttl: Duration,
    ) -> Result<Option<CommandRecord>, StoreError> {
        let mut state = self.inner.lock().expect("store lock is not poisoned");

        let mut expired: Vec<(CommandId, LeaseAttempt)> = Vec::new();
        for (command_id, record) in &state.commands {
            if let CommandStatus::Leased { lease } = &record.status
                && lease.expires_at <= now
            {
                expired.push((command_id.clone(), lease.attempt));
            }
        }
        for (command_id, _attempt) in &expired {
            let expired_session = {
                let Some(record) = state.commands.get_mut(command_id) else {
                    continue;
                };
                if let CommandStatus::Leased { .. } = &record.status {
                    record.status = CommandStatus::Pending;
                }
                record.session_id.clone()
            };
            state.leased_sessions.remove(&expired_session);
        }

        let candidate = state
            .commands
            .values()
            .filter(|record| record.status == CommandStatus::Pending)
            .find(|record| !state.leased_sessions.contains(&record.session_id))
            .cloned();

        let Some(mut candidate) = candidate else {
            return Ok(None);
        };
        // Reclaimed leases keep a monotonically increasing fencing attempt.
        let previous_attempt = expired
            .iter()
            .find(|(command_id, _)| *command_id == candidate.command_id)
            .map_or(0, |(_, attempt)| attempt.0);
        let attempt = LeaseAttempt(previous_attempt + 1);
        candidate.status = CommandStatus::Leased {
            lease: Lease {
                worker: worker.to_string(),
                attempt,
                expires_at: Timestamp::from_unix_millis(
                    now.unix_millis() + i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX),
                ),
            },
        };
        state.leased_sessions.insert(candidate.session_id.clone());
        state
            .commands
            .insert(candidate.command_id.clone(), candidate.clone());
        Ok(Some(candidate))
    }

    fn renew(
        &self,
        command_id: &str,
        attempt: LeaseAttempt,
        now: Timestamp,
        ttl: Duration,
    ) -> Result<Lease, StoreError> {
        let mut state = self.inner.lock().expect("store lock is not poisoned");
        let id =
            CommandId::new(command_id).map_err(|error| StoreError::Conflict(error.to_string()))?;
        let record = state
            .commands
            .get_mut(&id)
            .ok_or_else(|| StoreError::NotFound(command_id.to_string()))?;
        let CommandStatus::Leased { lease } = &mut record.status else {
            return Err(StoreError::InvalidState("command is not leased".into()));
        };
        if lease.attempt != attempt {
            return Err(StoreError::LeaseLost {
                command_id: command_id.to_string(),
                expected: lease.attempt.0,
                actual: attempt.0,
            });
        }
        lease.expires_at = Timestamp::from_unix_millis(
            now.unix_millis() + i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX),
        );
        Ok(lease.clone())
    }

    fn finish(
        &self,
        command_id: &str,
        attempt: LeaseAttempt,
        succeeded: bool,
        now: Timestamp,
    ) -> Result<CommandRecord, StoreError> {
        let mut state = self.inner.lock().expect("store lock is not poisoned");
        let id =
            CommandId::new(command_id).map_err(|error| StoreError::Conflict(error.to_string()))?;
        let finished_session = {
            let record = state
                .commands
                .get_mut(&id)
                .ok_or_else(|| StoreError::NotFound(command_id.to_string()))?;
            let current_attempt = match &record.status {
                CommandStatus::Leased { lease } => lease.attempt,
                _ => return Err(StoreError::InvalidState("command is not leased".into())),
            };
            if current_attempt != attempt {
                return Err(StoreError::LeaseLost {
                    command_id: command_id.to_string(),
                    expected: current_attempt.0,
                    actual: attempt.0,
                });
            }
            record.status = CommandStatus::Finished {
                succeeded,
                finished_at: now,
            };
            record.session_id.clone()
        };
        state.leased_sessions.remove(&finished_session);
        state
            .commands
            .get(&id)
            .cloned()
            .ok_or_else(|| StoreError::NotFound(command_id.to_string()))
    }
}
