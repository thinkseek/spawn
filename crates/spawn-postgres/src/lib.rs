//! PostgreSQL adapters for the event log and the leased command queue.
//!
//! Times travel as epoch milliseconds, so this crate needs no date library.
//! Every write goes through the SQL functions in `sql/schema.sql`; the
//! ordering guarantees live there and are shared by every writer.

use std::sync::{Mutex, MutexGuard};

use postgres::{Client, NoTls, Row};
use spawn_core::command::{Claimed, Outcome, Submission};
use spawn_core::event::{
    Appended, EventKind, EventRecord, GlobalPosition, NewEvent, SessionSeq, Source,
};
use spawn_core::id::{AgentId, CommandId, ExecutionId, Principal, SessionId};
use spawn_core::port::{CommandQueue, EventLog, StoreError};
use spawn_core::time::Timestamp;

/// Schema installed by [`PostgresStore::migrate`].
const SCHEMA: &str = include_str!("../sql/schema.sql");

/// Conversion of a driver error.
fn backend(error: &postgres::Error) -> StoreError {
    if let Some(db) = error.as_db_error() {
        return match db.code() {
            &postgres::error::SqlState::INSUFFICIENT_PRIVILEGE => {
                StoreError::Rejected(db.message().to_string())
            }
            _ => StoreError::Backend(format!(
                "{} ({}) {}",
                db.message(),
                db.code().code(),
                db.detail().unwrap_or("")
            )),
        };
    }
    StoreError::Backend(error.to_string())
}

/// PostgreSQL store.
pub struct PostgresStore {
    client: Mutex<Client>,
}

impl PostgresStore {
    /// Connect to the database in the URL.
    pub fn connect(url: &str) -> Result<Self, StoreError> {
        let client = Client::connect(url, NoTls).map_err(|error| backend(&error))?;
        Ok(Self {
            client: Mutex::new(client),
        })
    }

    /// Connect and move this session into `schema`, creating it when absent.
    ///
    /// Tests use one schema per case, so a shared database cannot leak
    /// commands or events between them.
    pub fn connect_in_schema(url: &str, schema: &str) -> Result<Self, StoreError> {
        if !schema
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            return Err(StoreError::Rejected(format!(
                "schema name {schema} is not a plain identifier"
            )));
        }
        let store = Self::connect(url)?;
        {
            let mut client = store.lock();
            client
                .batch_execute(&format!(
                    "create schema if not exists {schema}; set search_path to {schema}"
                ))
                .map_err(|error| backend(&error))?;
        }
        store.migrate()?;
        Ok(store)
    }

    fn lock(&self) -> MutexGuard<'_, Client> {
        self.client
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Install or refresh the schema. Idempotent.
    pub fn migrate(&self) -> Result<(), StoreError> {
        self.lock()
            .batch_execute(SCHEMA)
            .map_err(|error| backend(&error))
    }
}

fn record_from_row(row: &Row) -> Result<EventRecord, StoreError> {
    let kind_value: serde_json::Value = row.get("kind");
    let kind: EventKind = serde_json::from_value(kind_value)
        .map_err(|error| StoreError::Corrupt(error.to_string()))?;
    let source_text: String = row.get("source");
    let source = Source::parse(&source_text)
        .ok_or_else(|| StoreError::Corrupt(format!("unknown source {source_text}")))?;
    Ok(EventRecord {
        position: GlobalPosition(row.get::<_, i64>("id").unsigned_abs()),
        session_seq: SessionSeq(row.get::<_, i64>("session_seq").unsigned_abs()),
        recorded_at: Timestamp(row.get("recorded_at_ms")),
        event: NewEvent {
            agent_id: AgentId::new(row.get::<_, String>("agent_id"))
                .map_err(|error| StoreError::Corrupt(error.to_string()))?,
            session_id: SessionId::new(row.get::<_, String>("session_id"))
                .map_err(|error| StoreError::Corrupt(error.to_string()))?,
            execution_id: row
                .get::<_, Option<String>>("execution_id")
                .map(ExecutionId::new)
                .transpose()
                .map_err(|error| StoreError::Corrupt(error.to_string()))?,
            source,
            source_event_id: row.get("source_event_id"),
            kind,
        },
    })
}

impl EventLog for PostgresStore {
    fn append(&self, event: NewEvent) -> Result<Appended, StoreError> {
        let kind = serde_json::to_value(&event.kind)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?;
        let mut client = self.lock();
        let row = client
            .query_one(
                "select out_id, out_seq, out_at, out_inserted
                   from spawn_append($1, $2, $3, $4, $5, $6)",
                &[
                    &event.agent_id.as_str(),
                    &event.session_id.as_str(),
                    &event
                        .execution_id
                        .as_ref()
                        .map(|id| id.as_str().to_string()),
                    &event.source.as_str(),
                    &event.source_event_id,
                    &kind,
                ],
            )
            .map_err(|error| backend(&error))?;
        let stored = EventRecord {
            position: GlobalPosition(row.get::<_, i64>("out_id").unsigned_abs()),
            session_seq: SessionSeq(row.get::<_, i64>("out_seq").unsigned_abs()),
            recorded_at: Timestamp(row.get("out_at")),
            event,
        };
        Ok(Appended {
            record: stored,
            inserted: row.get("out_inserted"),
        })
    }

    fn read_session(
        &self,
        session: &SessionId,
        after: SessionSeq,
        limit: usize,
    ) -> Result<Vec<EventRecord>, StoreError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let after = i64::try_from(after.0).unwrap_or(i64::MAX);
        let mut client = self.lock();
        let rows = client
            .query(
                "select id, agent_id, session_id, execution_id, source, source_event_id,
                        kind, session_seq, recorded_at_ms
                   from spawn_events
                  where session_id = $1 and session_seq > $2
                  order by session_seq
                  limit $3",
                &[&session.as_str(), &after, &limit],
            )
            .map_err(|error| backend(&error))?;
        rows.iter().map(record_from_row).collect()
    }

    fn read_global(
        &self,
        after: GlobalPosition,
        limit: usize,
    ) -> Result<Vec<EventRecord>, StoreError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let after = i64::try_from(after.0).unwrap_or(i64::MAX);
        let mut client = self.lock();
        let rows = client
            .query(
                "select id, agent_id, session_id, execution_id, source, source_event_id,
                        kind, session_seq, recorded_at_ms
                   from spawn_events
                  where id > $1
                  order by id
                  limit $2",
                &[&after, &limit],
            )
            .map_err(|error| backend(&error))?;
        rows.iter().map(record_from_row).collect()
    }
}

impl CommandQueue for PostgresStore {
    fn submit(
        &self,
        principal: &Principal,
        submission: &Submission,
    ) -> Result<CommandId, StoreError> {
        let action = serde_json::to_value(&submission.action)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?;
        let mut client = self.lock();
        let row = client
            .query_one(
                "select out_id from spawn_submit($1, $2, $3, $4, $5)",
                &[
                    &principal.as_str(),
                    &submission.idempotency_key.as_str(),
                    &submission.agent_id.as_str(),
                    &submission.session_id.as_str(),
                    &action,
                ],
            )
            .map_err(|error| backend(&error))?;
        Ok(CommandId(row.get::<_, i64>("out_id").unsigned_abs()))
    }

    fn claim(&self, worker: &str, ttl: std::time::Duration) -> Result<Option<Claimed>, StoreError> {
        let ttl = i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX);
        let mut client = self.lock();
        let rows = client
            .query(
                "select out_id, out_attempt, out_agent, out_session, out_action
                   from spawn_claim($1, $2)",
                &[&worker, &ttl],
            )
            .map_err(|error| backend(&error))?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let attempt = row.get::<_, i32>("out_attempt");
        Ok(Some(Claimed {
            command_id: CommandId(row.get::<_, i64>("out_id").unsigned_abs()),
            attempt: u32::try_from(attempt).unwrap_or(0),
            agent_id: AgentId::new(row.get::<_, String>("out_agent"))
                .map_err(|error| StoreError::Corrupt(error.to_string()))?,
            session_id: SessionId::new(row.get::<_, String>("out_session"))
                .map_err(|error| StoreError::Corrupt(error.to_string()))?,
            action: row.get("out_action"),
        }))
    }

    fn finish(&self, claimed: &Claimed, outcome: &Outcome) -> Result<(), StoreError> {
        let (succeeded, error) = match outcome {
            Outcome::Succeeded => (true, None),
            Outcome::Failed(reason) => (false, Some(reason.clone())),
        };
        let attempt = i32::try_from(claimed.attempt).unwrap_or(i32::MAX);
        let command = i64::try_from(claimed.command_id.0).unwrap_or(i64::MAX);
        let mut client = self.lock();
        let row = client
            .query_one(
                "select spawn_finish($1, $2, $3, $4)",
                &[&command, &attempt, &succeeded, &error],
            )
            .map_err(|error| backend(&error))?;
        let status: String = row.get(0);
        match status.as_str() {
            "done" | "already" => Ok(()),
            "stale" => Err(StoreError::LeaseLost {
                command: claimed.command_id,
                attempt: claimed.attempt,
            }),
            other => Err(StoreError::Backend(format!("finish returned {other}"))),
        }
    }
}
