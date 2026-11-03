//! PostgreSQL-backed storage implementing the spawn-store ports.
//!
//! A private Tokio runtime blocks on async driver calls so the public port
//! traits stay synchronous. Ordering, dedup and leasing invariants live in the
//! database, not in application-level locks.

// Pedantic doc-bookkeeping lints: public items carry purposeful docs, but
// mandatory Errors/Panics sections and identifier backticks add noise.
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::doc_markdown
)]
#![allow(clippy::module_name_repetitions, clippy::must_use_candidate)]
use std::sync::Arc;
use std::time::Duration;

use spawn_store::{CommandQueue, EventLog, StoreError};
use spawn_types::clock::Timestamp;
use spawn_types::command::{CommandRecord, Lease, LeaseAttempt};
use spawn_types::event::{
    AppendOutcome, EventId, EventKind, EventRecord, GlobalPosition, NewEvent, SessionCursor,
    SessionSeq,
};
use spawn_types::identifiers::SessionId;
use thiserror::Error;
use tokio::runtime::Runtime;
use tokio_postgres::{Client, NoTls};

const MIGRATION_SQL: &str = include_str!("../migrations/0001_init.sql");

/// Failures specific to the PostgreSQL backend.
#[derive(Debug, Error)]
pub enum PgError {
    /// Connection or driver failure.
    #[error("postgres failure: {0}")]
    Driver(String),
}

impl From<PgError> for StoreError {
    fn from(error: PgError) -> Self {
        StoreError::Unavailable(error.to_string())
    }
}

/// Synchronous PostgreSQL store.
pub struct PostgresStore {
    runtime: Runtime,
    client: Arc<tokio::sync::Mutex<Client>>,
    clock: Arc<dyn spawn_types::clock::Clock>,
}

impl PostgresStore {
    /// Connect, run migrations and return a ready store.
    ///
    /// # Errors
    /// Fails when the database cannot be reached or migrated.
    pub fn connect(
        database_url: &str,
        clock: Arc<dyn spawn_types::clock::Clock>,
    ) -> Result<Self, PgError> {
        let runtime = Runtime::new().map_err(|error| PgError::Driver(error.to_string()))?;
        let (client, connection) = runtime
            .block_on(tokio_postgres::connect(database_url, NoTls))
            .map_err(|error| PgError::Driver(error.to_string()))?;
        spawn_connection(runtime.handle(), connection);
        let store = Self {
            runtime,
            client: Arc::new(tokio::sync::Mutex::new(client)),
            clock,
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<(), PgError> {
        self.runtime.block_on(async {
            let client = self.client.lock().await;
            client
                .batch_execute(MIGRATION_SQL)
                .await
                .map_err(|error| PgError::Driver(error.to_string()))
        })
    }

    fn block_on<T>(
        &self,
        future: impl std::future::Future<Output = Result<T, StoreError>>,
    ) -> Result<T, StoreError> {
        self.runtime.block_on(future)
    }
}

fn spawn_connection<F>(handle: &tokio::runtime::Handle, connection: F)
where
    F: std::future::Future<Output = Result<(), tokio_postgres::Error>> + Send + 'static,
{
    handle.spawn(async move {
        let _ = connection.await;
    });
}

fn map_driver(error: &tokio_postgres::Error) -> StoreError {
    use std::fmt::Write;
    let mut message = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(inner) = source {
        message.push_str(" | caused by: ");
        message.push_str(&inner.to_string());
        source = inner.source();
    }
    if let Some(db) = error.as_db_error() {
        let _ = write!(
            message,
            " [code={} severity={} detail={:?}]",
            db.code().code(),
            db.severity(),
            db.detail()
        );
    }
    StoreError::Unavailable(message)
}

fn row_to_event(row: &tokio_postgres::Row) -> Result<EventRecord, StoreError> {
    let kind: EventKind = serde_json::from_value(row.get("kind"))
        .map_err(|error| StoreError::Conflict(format!("invalid stored kind: {error}")))?;
    let artifact = row
        .get::<_, Option<serde_json::Value>>("artifact")
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| StoreError::Conflict(format!("invalid stored artifact: {error}")))?;
    Ok(EventRecord {
        event_id: EventId::new(row.get::<_, String>("event_id"))
            .map_err(|error| StoreError::Conflict(error.to_string()))?,
        event: NewEvent {
            agent_id: serde_json::from_value(serde_json::Value::String(row.get("agent_id")))
                .map_err(|error| StoreError::Conflict(error.to_string()))?,
            session_id: serde_json::from_value(serde_json::Value::String(row.get("session_id")))
                .map_err(|error| StoreError::Conflict(error.to_string()))?,
            execution_id: optional_label(row, "execution_id")?,
            source: serde_json::from_value(serde_json::Value::String(row.get("source")))
                .map_err(|error| StoreError::Conflict(error.to_string()))?,
            source_event_id: row.get("source_event_id"),
            kind,
            artifact,
            occurred_at: Some(Timestamp::from_unix_millis(row.get("occurred_at"))),
        },
        session_seq: SessionSeq(u64::try_from(row.get::<_, i64>("session_seq")).unwrap_or(0)),
        global_position: GlobalPosition(u64::try_from(row.get::<_, i64>("id")).unwrap_or(0)),
        recorded_at: Timestamp::from_unix_millis(row.get("recorded_at")),
    })
}

fn optional_label<T>(row: &tokio_postgres::Row, column: &str) -> Result<Option<T>, StoreError>
where
    T: serde::de::DeserializeOwned,
{
    row.get::<_, Option<String>>(column)
        .map(|value| {
            serde_json::from_value(serde_json::Value::String(value))
                .map_err(|error| StoreError::Conflict(error.to_string()))
        })
        .transpose()
}

impl EventLog for PostgresStore {
    fn append(&self, event: NewEvent) -> Result<AppendOutcome, StoreError> {
        let clock = self.clock.clone();
        let client = self.client.clone();
        self.block_on(async move {
            let mut client = client.lock().await;
            let transaction = client
                .transaction()
                .await
                .map_err(|error| map_driver(&error))?;
            let recorded_at = event
                .occurred_at
                .unwrap_or_else(|| clock.now())
                .unix_millis();

            if let Some(source_event_id) = &event.source_event_id {
                let existing = transaction
                    .query_opt(
                        "SELECT * FROM spawn_events \
                         WHERE session_id = $1 AND source = $2 AND source_event_id = $3",
                        &[
                            &event.session_id.as_str(),
                            &serde_json::to_value(event.source)
                                .ok()
                                .and_then(|value| value.as_str().map(str::to_string))
                                .unwrap_or_default(),
                            source_event_id,
                        ],
                    )
                    .await
                    .map_err(|error| map_driver(&error))?;
                if let Some(row) = existing {
                    return Ok(AppendOutcome::Deduplicated(row_to_event(&row)?));
                }
            }

            let kind_json = serde_json::to_value(&event.kind)
                .map_err(|error| StoreError::Conflict(error.to_string()))?;
            let artifact_json = match &event.artifact {
                Some(artifact) => Some(
                    serde_json::to_value(artifact)
                        .map_err(|error| StoreError::Conflict(error.to_string()))?,
                ),
                None => None,
            };
            let source_json = serde_json::to_value(event.source)
                .map_err(|error| StoreError::Conflict(error.to_string()))?;
            let source_text = source_json.as_str().unwrap_or("bridge");
            // The head CTE increments the per-session sequence exactly once.
            // The global id is taken explicitly with nextval(); currval() then
            // derives the matching textual event_id within the same statement
            // without a second sequence advance or a post-insert UPDATE (which
            // could not see sibling-CTE rows under the statement snapshot).
            let row = transaction
                .query_one(
                    "WITH head AS ( \
                        INSERT INTO spawn_session_heads(session_id, next_seq, last_occurred_at) \
                        VALUES ($2, 1, $9) \
                        ON CONFLICT (session_id) DO UPDATE \
                        SET next_seq = spawn_session_heads.next_seq + 1, \
                            last_occurred_at = EXCLUDED.last_occurred_at \
                        RETURNING next_seq \
                     ) \
                     INSERT INTO spawn_events( \
                        id, event_id, agent_id, session_id, execution_id, session_seq, source, \
                        source_event_id, kind, artifact, occurred_at, recorded_at \
                     ) VALUES ( \
                        nextval(pg_get_serial_sequence('spawn_events', 'id')), \
                        'evt_' || currval(pg_get_serial_sequence('spawn_events', 'id')), \
                        $1, $2, $3, (SELECT next_seq FROM head), $4, \
                        $5, $6, $7, $8, $9) \
                     RETURNING *",
                    &[
                        &event.agent_id.as_str(),   // $1
                        &event.session_id.as_str(), // $2
                        &event
                            .execution_id
                            .as_ref()
                            .map(<spawn_types::identifiers::ExecutionId as AsRef<str>>::as_ref), // $3
                        &source_text,           // $4
                        &event.source_event_id, // $5
                        &kind_json,             // $6
                        &artifact_json,         // $7
                        &recorded_at,           // $8 occurred_at
                        &recorded_at,           // $9 recorded_at / head timestamp
                    ],
                )
                .await
                .map_err(|error| map_driver(&error))?;
            transaction
                .commit()
                .await
                .map_err(|error| map_driver(&error))?;
            Ok(AppendOutcome::Recorded(row_to_event(&row)?))
        })
    }

    fn read_session(
        &self,
        session: &SessionId,
        since: SessionSeq,
        limit: usize,
    ) -> Result<Vec<EventRecord>, StoreError> {
        let client = self.client.clone();
        let session_id = session.clone();
        self.block_on(async move {
            let client = client.lock().await;
            let rows = client
                .query(
                    "SELECT * FROM spawn_events WHERE session_id = $1 AND session_seq > $2 \
                     ORDER BY session_seq ASC LIMIT $3",
                    &[
                        &session_id.as_str(),
                        &i64::try_from(since.0).unwrap_or(i64::MAX),
                        &i64::try_from(limit).unwrap_or(i64::MAX),
                    ],
                )
                .await
                .map_err(|error| map_driver(&error))?;
            #[allow(clippy::redundant_closure)]
            rows.iter().map(|row| row_to_event(row)).collect()
        })
    }

    fn read_global(
        &self,
        since: GlobalPosition,
        limit: usize,
    ) -> Result<Vec<EventRecord>, StoreError> {
        let client = self.client.clone();
        self.block_on(async move {
            let client = client.lock().await;
            let rows = client
                .query(
                    "SELECT * FROM spawn_events WHERE id > $1 ORDER BY id ASC LIMIT $2",
                    &[
                        &i64::try_from(since.0).unwrap_or(i64::MAX),
                        &i64::try_from(limit).unwrap_or(i64::MAX),
                    ],
                )
                .await
                .map_err(|error| map_driver(&error))?;
            #[allow(clippy::redundant_closure)]
            rows.iter().map(|row| row_to_event(row)).collect()
        })
    }

    fn session_cursors(&self) -> Result<Vec<SessionCursor>, StoreError> {
        let client = self.client.clone();
        self.block_on(async move {
            let client = client.lock().await;
            let rows = client
                .query(
                    "SELECT session_id, next_seq, last_occurred_at FROM spawn_session_heads",
                    &[],
                )
                .await
                .map_err(|error| map_driver(&error))?;
            Ok(rows
                .iter()
                .map(|row| SessionCursor {
                    session_id: SessionId::from_prefixed(row.get::<_, String>(0))
                        .expect("stored session ids are valid"),
                    last_seq: SessionSeq(u64::try_from(row.get::<_, i64>(1)).unwrap_or(0)),
                    last_occurred_at: Timestamp::from_unix_millis(row.get(2)),
                })
                .collect())
        })
    }
}

impl CommandQueue for PostgresStore {
    fn submit(&self, command: CommandRecord) -> Result<CommandRecord, StoreError> {
        let client = self.client.clone();
        self.block_on(async move {
            let client = client.lock().await;
            let action_json = serde_json::to_value(&command.action)
                .map_err(|error| StoreError::Conflict(error.to_string()))?;
            let status_json = serde_json::to_value(&command.status)
                .map_err(|error| StoreError::Conflict(error.to_string()))?;
            let principal_json = serde_json::to_value(&command.principal)
                .map_err(|error| StoreError::Conflict(error.to_string()))?;
            let existing = client
                .query_opt(
                    "SELECT * FROM spawn_commands WHERE idempotency_key = $1",
                    &[&command.idempotency_key.as_str()],
                )
                .await
                .map_err(|error| map_driver(&error))?;
            if let Some(row) = existing {
                return row_to_command(&row);
            }
            let inserted = client
                .query_one(
                    "INSERT INTO spawn_commands( \
                        command_id, agent_id, session_id, idempotency_key, principal, action, \
                        status, submitted_at \
                     ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8) RETURNING *",
                    &[
                        &command.command_id.as_str(),
                        &command.agent_id.as_str(),
                        &command.session_id.as_str(),
                        &command.idempotency_key.as_str(),
                        &principal_json,
                        &action_json,
                        &status_json,
                        &command.submitted_at.unix_millis(),
                    ],
                )
                .await
                .map_err(|error| map_driver(&error))?;
            row_to_command(&inserted)
        })
    }

    fn claim(
        &self,
        worker: &str,
        now: Timestamp,
        ttl: Duration,
    ) -> Result<Option<CommandRecord>, StoreError> {
        let client = self.client.clone();
        let worker = worker.to_string();
        self.block_on(async move {
            let mut client = client.lock().await;
            let transaction = client.transaction().await.map_err(|error| map_driver(&error))?;
            let expires = now.unix_millis() + i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX);
            let now_millis = now.unix_millis();
            transaction
                .execute(
                    "UPDATE spawn_commands r SET status = $1 \
                     WHERE r.status->>'phase' = 'leased' \
                     AND (r.status->'lease'->>'expires_at')::bigint <= $2",
                    &[
                        &serde_json::json!({"phase":"pending"}) as &(dyn tokio_postgres::types::ToSql + Sync),
                        &now_millis,
                    ],
                )
                .await
                .map_err(|error| map_driver(&error))?;
            let row = transaction
                .query_opt(
                    "SELECT c.* FROM spawn_commands c \
                     WHERE c.status->>'phase' = 'pending' \
                     AND NOT EXISTS ( \
                        SELECT 1 FROM spawn_commands l \
                        WHERE l.session_id = c.session_id \
                        AND l.command_id <> c.command_id \
                        AND l.status->>'phase' = 'leased' \
                        AND (l.status->'lease'->>'expires_at')::bigint > $1 \
                     ) \
                     ORDER BY c.submitted_at, c.command_id \
                     FOR UPDATE OF c SKIP LOCKED LIMIT 1",
                    &[&now_millis],
                )
                .await
                .map_err(|error| map_driver(&error))?;
            let Some(row) = row else {
                transaction.commit().await.map_err(|error| map_driver(&error))?;
                return Ok(None);
            };
            // Reclaimed leases keep a monotonically increasing fencing attempt.
            let prior_attempt = row.get::<_, i64>("lease_attempt");
            let mut record = row_to_command(&row)?;
            let attempt = LeaseAttempt(u64::try_from(prior_attempt).unwrap_or(0) + 1);
            let lease = Lease {
                worker,
                attempt,
                expires_at: Timestamp::from_unix_millis(expires),
            };
            record.status = spawn_types::command::CommandStatus::Leased {
                lease: lease.clone(),
            };
            let status_json = serde_json::to_value(&record.status)
                .map_err(|error| StoreError::Conflict(error.to_string()))?;
            transaction
                .execute(
                    "UPDATE spawn_commands SET status = $1, lease_attempt = $2 WHERE command_id = $3",
                    &[&status_json, &i64::try_from(attempt.0).unwrap_or(i64::MAX), &record.command_id.as_str()],
                )
                .await
                .map_err(|error| map_driver(&error))?;
            transaction.commit().await.map_err(|error| map_driver(&error))?;
            Ok(Some(record))
        })
    }

    fn renew(
        &self,
        command_id: &str,
        attempt: LeaseAttempt,
        now: Timestamp,
        ttl: Duration,
    ) -> Result<Lease, StoreError> {
        self.mutate_lease(command_id, attempt, now, ttl, false, true)
    }

    fn finish(
        &self,
        command_id: &str,
        attempt: LeaseAttempt,
        succeeded: bool,
        now: Timestamp,
    ) -> Result<CommandRecord, StoreError> {
        let client = self.client.clone();
        self.block_on(async move {
            let client = client.lock().await;
            let row = client
                .query_one(
                    "SELECT * FROM spawn_commands WHERE command_id = $1 FOR UPDATE",
                    &[&command_id],
                )
                .await
                .map_err(|error| StoreError::NotFound(error.to_string()))?;
            let mut record = row_to_command(&row)?;
            let current = match &record.status {
                spawn_types::command::CommandStatus::Leased { lease } => lease.attempt,
                _ => return Err(StoreError::InvalidState("command is not leased".into())),
            };
            if current != attempt {
                return Err(StoreError::LeaseLost {
                    command_id: command_id.to_string(),
                    expected: current.0,
                    actual: attempt.0,
                });
            }
            record.status = spawn_types::command::CommandStatus::Finished {
                succeeded,
                finished_at: now,
            };
            let status_json = serde_json::to_value(&record.status)
                .map_err(|error| StoreError::Conflict(error.to_string()))?;
            client
                .execute(
                    "UPDATE spawn_commands SET status = $1 WHERE command_id = $2",
                    &[&status_json, &command_id],
                )
                .await
                .map_err(|error| map_driver(&error))?;
            Ok(record)
        })
    }
}

impl PostgresStore {
    fn mutate_lease(
        &self,
        command_id: &str,
        attempt: LeaseAttempt,
        now: Timestamp,
        ttl: Duration,
        _finish: bool,
        renew: bool,
    ) -> Result<Lease, StoreError> {
        let client = self.client.clone();
        self.block_on(async move {
            let client = client.lock().await;
            let row = client
                .query_one(
                    "SELECT * FROM spawn_commands WHERE command_id = $1 FOR UPDATE",
                    &[&command_id],
                )
                .await
                .map_err(|error| StoreError::NotFound(error.to_string()))?;
            let mut record = row_to_command(&row)?;
            let updated_lease = {
                let spawn_types::command::CommandStatus::Leased { lease } = &mut record.status
                else {
                    return Err(StoreError::InvalidState("command is not leased".into()));
                };
                if lease.attempt != attempt {
                    return Err(StoreError::LeaseLost {
                        command_id: command_id.to_string(),
                        expected: lease.attempt.0,
                        actual: attempt.0,
                    });
                }
                if renew {
                    lease.expires_at = Timestamp::from_unix_millis(
                        now.unix_millis() + i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX),
                    );
                }
                lease.clone()
            };
            let status_json = serde_json::to_value(&record.status)
                .map_err(|error| StoreError::Conflict(error.to_string()))?;
            client
                .execute(
                    "UPDATE spawn_commands SET status = $1 WHERE command_id = $2",
                    &[&status_json, &command_id],
                )
                .await
                .map_err(|error| map_driver(&error))?;
            Ok(updated_lease)
        })
    }
}

fn row_to_command(row: &tokio_postgres::Row) -> Result<CommandRecord, StoreError> {
    let action: spawn_types::command::CommandAction = serde_json::from_value(row.get("action"))
        .map_err(|error| StoreError::Conflict(format!("invalid action: {error}")))?;
    let status: spawn_types::command::CommandStatus = serde_json::from_value(row.get("status"))
        .map_err(|error| StoreError::Conflict(format!("invalid status: {error}")))?;
    let principal: spawn_types::command::Principal =
        serde_json::from_value(row.get("principal"))
            .map_err(|error| StoreError::Conflict(format!("invalid principal: {error}")))?;
    Ok(CommandRecord {
        command_id: spawn_types::command::CommandId::new(row.get::<_, String>("command_id"))
            .map_err(|error| StoreError::Conflict(error.to_string()))?,
        agent_id: serde_json::from_value(serde_json::Value::String(row.get("agent_id")))
            .map_err(|error| StoreError::Conflict(error.to_string()))?,
        session_id: serde_json::from_value(serde_json::Value::String(row.get("session_id")))
            .map_err(|error| StoreError::Conflict(error.to_string()))?,
        idempotency_key: spawn_types::command::IdempotencyKey::new(
            row.get::<_, String>("idempotency_key"),
        )
        .map_err(|error| StoreError::Conflict(error.to_string()))?,
        principal,
        action,
        status,
        submitted_at: Timestamp::from_unix_millis(row.get("submitted_at")),
    })
}
