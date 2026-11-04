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
use spawn_core::observation::Observation;
use spawn_core::port::{CommandQueue, EventLog, StoreError};
use spawn_core::time::Timestamp;

mod fleet;
mod missions;
mod rsi;
pub use rsi::{
    RSI_AGENT, RSI_SESSION, RsiAttempt, RsiCommand, RsiIteration, RsiIterationRow, RsiState,
};

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

/// URL a terminal uses: the controller's URL with the restricted login.
///
/// The agent runs inside the terminal and reads its environment. Handing it
/// the controller URL would let a candidate write `rsi_state` and advance its
/// own baseline past the gate, so the pane gets the login that can only
/// append engine events.
#[must_use]
pub fn terminal_url(controller: &str) -> String {
    role_url(controller, "spawn_terminal")
}

/// The controller URL with its credentials replaced by one of the schema's
/// fixed-password roles (`spawn_terminal`, `spawn_supervisor`). The boundary
/// is the role's privileges, not the password.
pub fn role_url(controller: &str, role: &str) -> String {
    let Some((scheme, rest)) = controller.split_once("://") else {
        return controller.to_string();
    };
    let host = rest.split_once('@').map_or(rest, |(_, tail)| tail);
    format!("{scheme}://{role}:{role}@{host}")
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

/// Current judgement of one session, from `agent_state`.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionState {
    /// Session.
    pub session_id: SessionId,
    /// Agent that owns the session.
    pub agent_id: AgentId,
    /// Latest judgement.
    pub observation: Observation,
    /// Sequence of the observation event.
    pub session_seq: SessionSeq,
    /// Global position of the observation event.
    pub event_id: GlobalPosition,
    /// Time the observation was recorded.
    pub recorded_at: Timestamp,
}

/// One non-Observer event, from `agent_timeline`.
#[derive(Debug, Clone, PartialEq)]
pub struct TimelineEntry {
    /// Sequence in the session; the keyset cursor.
    pub session_seq: SessionSeq,
    /// Global position.
    pub event_id: GlobalPosition,
    /// Execution the event belongs to.
    pub execution_id: Option<ExecutionId>,
    /// Writer of the event.
    pub source: Source,
    /// Dotted event name.
    pub event_name: String,
    /// Event payload without the name.
    pub detail: Option<serde_json::Value>,
    /// Time the event was recorded.
    pub recorded_at: Timestamp,
}

/// What the supervisor's liveness rule knows about its session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LivenessFacts {
    /// A command for the session is pending or leased.
    pub open_command: bool,
    /// Newest execution.
    pub execution_id: Option<String>,
    /// The newest execution has not ended.
    pub live: bool,
    /// How the newest execution ended.
    pub cause: Option<String>,
    /// Principal of the newest command.
    pub last_principal: Option<String>,
    /// Action type of the newest command.
    pub last_kind: Option<String>,
    /// The engine reported a native session at least once.
    pub native_bound: bool,
    /// Failed revivals since the newest execution started.
    pub failures: u32,
    /// When the latest failed revival finished.
    pub last_failure_ms: Option<i64>,
}

/// One execution, from `agent_executions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionRow {
    /// Execution.
    pub execution_id: ExecutionId,
    /// Command that created it.
    pub command_id: Option<CommandId>,
    /// Engine wire name.
    pub engine: Option<String>,
    /// Start time.
    pub started_at: Timestamp,
    /// Time the Bridge ended it; `None` while it runs.
    pub ended_at: Option<Timestamp>,
    /// End cause wire name.
    pub cause: Option<String>,
    /// Process exit code.
    pub exit_code: Option<i32>,
    /// The runner reported an engine exit.
    pub exit_reported: bool,
    /// `ended_at - started_at` in milliseconds.
    pub duration_ms: Option<i64>,
}

/// One tool call without a result, from `agent_open_tools`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenTool {
    /// Execution of the call.
    pub execution_id: Option<ExecutionId>,
    /// Engine tool call identifier.
    pub tool_use_id: String,
    /// Engine tool name.
    pub tool_name: Option<String>,
    /// Sequence of the start event.
    pub session_seq: SessionSeq,
    /// Start time.
    pub started_at: Timestamp,
}

/// Position of a session's log, from `agent_freshness`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Freshness {
    /// Highest session sequence.
    pub last_session_seq: SessionSeq,
    /// Highest global position.
    pub last_event_id: GlobalPosition,
    /// Time of the latest event.
    pub last_event_at: Timestamp,
    /// Number of events.
    pub event_count: u64,
}

fn corrupt(error: impl std::fmt::Display) -> StoreError {
    StoreError::Corrupt(error.to_string())
}

fn seq(row: &Row, column: &str) -> SessionSeq {
    SessionSeq(row.get::<_, i64>(column).unsigned_abs())
}

fn position(row: &Row, column: &str) -> GlobalPosition {
    GlobalPosition(row.get::<_, i64>(column).unsigned_abs())
}

fn execution(row: &Row) -> Result<Option<ExecutionId>, StoreError> {
    row.get::<_, Option<String>>("execution_id")
        .map(ExecutionId::new)
        .transpose()
        .map_err(corrupt)
}

fn state_from_row(row: &Row) -> Result<SessionState, StoreError> {
    Ok(SessionState {
        session_id: SessionId::new(row.get::<_, String>("session_id")).map_err(corrupt)?,
        agent_id: AgentId::new(row.get::<_, String>("agent_id")).map_err(corrupt)?,
        observation: serde_json::from_value(row.get("observation")).map_err(corrupt)?,
        session_seq: seq(row, "session_seq"),
        event_id: position(row, "event_id"),
        recorded_at: Timestamp(row.get("recorded_at_ms")),
    })
}

/// Read side. The queries go through the `agent_*` views, so the SQL
/// definitions stay the one source of each fold.
///
/// ponytail: methods on the store, no port; promote to a trait when a second
/// reader (for example an HTTP read service) appears.
impl PostgresStore {
    /// Current judgement of every session, ordered by session.
    pub fn states(&self) -> Result<Vec<SessionState>, StoreError> {
        let rows = self
            .lock()
            .query(
                "select session_id, agent_id, observation, session_seq, event_id, recorded_at_ms
                   from agent_state order by session_id",
                &[],
            )
            .map_err(|error| backend(&error))?;
        rows.iter().map(state_from_row).collect()
    }

    /// Current judgement of one session.
    pub fn state(&self, session: &SessionId) -> Result<Option<SessionState>, StoreError> {
        let row = self
            .lock()
            .query_opt(
                "select session_id, agent_id, observation, session_seq, event_id, recorded_at_ms
                   from agent_state where session_id = $1",
                &[&session.as_str()],
            )
            .map_err(|error| backend(&error))?;
        row.as_ref().map(state_from_row).transpose()
    }

    /// Up to `limit` timeline entries after `after`, in sequence order.
    pub fn timeline(
        &self,
        session: &SessionId,
        after: SessionSeq,
        limit: usize,
    ) -> Result<Vec<TimelineEntry>, StoreError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let after = i64::try_from(after.0).unwrap_or(i64::MAX);
        let rows = self
            .lock()
            .query(
                "select session_seq, id, execution_id, source, event_name, detail, recorded_at_ms
                   from agent_timeline
                  where session_id = $1 and session_seq > $2
                  order by session_seq
                  limit $3",
                &[&session.as_str(), &after, &limit],
            )
            .map_err(|error| backend(&error))?;
        rows.iter()
            .map(|row| {
                let source: String = row.get("source");
                Ok(TimelineEntry {
                    session_seq: seq(row, "session_seq"),
                    event_id: position(row, "id"),
                    execution_id: execution(row)?,
                    source: Source::parse(&source)
                        .ok_or_else(|| corrupt(format!("unknown source {source}")))?,
                    event_name: row
                        .get::<_, Option<String>>("event_name")
                        .unwrap_or_default(),
                    detail: row.get("detail"),
                    recorded_at: Timestamp(row.get("recorded_at_ms")),
                })
            })
            .collect()
    }

    /// Executions of one session, newest first.
    /// Facts the supervisor's liveness rule decides from, all from storage.
    pub fn liveness_facts(
        &self,
        session: &SessionId,
        reviver: &str,
    ) -> Result<LivenessFacts, StoreError> {
        let mut client = self.lock();
        let row = client
            .query_one(
                "with last_exec as (
                    select e.execution_id, e.started_at_ms, e.ended_at_ms, e.cause
                      from agent_executions e
                     where e.session_id = $1
                     order by e.started_at_ms desc, e.execution_id desc limit 1
                 ), last_cmd as (
                    select c.principal, c.action ->> 'type' as kind
                      from spawn_commands c where c.session_id = $1
                     order by c.id desc limit 1
                 )
                 select
                    exists (select 1 from spawn_commands c
                             where c.session_id = $1 and c.state <> 'finished') as open_command,
                    (select execution_id from last_exec) as execution_id,
                    coalesce((select ended_at_ms is null from last_exec), false) as live,
                    (select cause from last_exec) as cause,
                    (select principal from last_cmd) as last_principal,
                    (select kind from last_cmd) as last_kind,
                    exists (select 1 from spawn_events e
                             where e.session_id = $1
                               and e.event_name = 'session.native_started') as native_bound,
                    (select count(*) from spawn_commands c
                      where c.session_id = $1 and c.principal = $2 and c.succeeded = false
                        and c.created_at_ms > coalesce((select started_at_ms from last_exec), 0)
                    ) as failures,
                    (select max(c.finished_at_ms) from spawn_commands c
                      where c.session_id = $1 and c.principal = $2 and c.succeeded = false
                    ) as last_failure_ms",
                &[&session.as_str(), &reviver],
            )
            .map_err(|error| backend(&error))?;
        Ok(LivenessFacts {
            open_command: row.get("open_command"),
            execution_id: row.get("execution_id"),
            live: row.get("live"),
            cause: row.get("cause"),
            last_principal: row.get("last_principal"),
            last_kind: row.get("last_kind"),
            native_bound: row.get("native_bound"),
            failures: u32::try_from(row.get::<_, i64>("failures")).unwrap_or(u32::MAX),
            last_failure_ms: row.get("last_failure_ms"),
        })
    }

    /// Commands one principal submitted, newest first, as JSON.
    pub fn commands_by(
        &self,
        principal: &str,
        limit: i64,
    ) -> Result<serde_json::Value, StoreError> {
        let mut client = self.lock();
        let row = client
            .query_one(
                "select coalesce(jsonb_agg(c order by c.id desc), '[]'::jsonb) as commands
                   from (select id, agent_id, session_id, action, state, succeeded, error, created_at_ms
                           from spawn_commands where principal = $1
                          order by id desc limit $2) c",
                &[&principal, &limit],
            )
            .map_err(|error| backend(&error))?;
        Ok(row.get("commands"))
    }

    /// The fleet as one JSON array, read through the grant-filtered
    /// `spawn_read_*` functions, so it shows exactly what the caller's login
    /// may see.
    pub fn fleet_json(&self) -> Result<serde_json::Value, StoreError> {
        let mut client = self.lock();
        let row = client
            .query_one(
                "select coalesce(jsonb_agg(row_to_json(f) order by f.session_id), '[]'::jsonb) as fleet
                   from (
                     select s.session_id, s.agent_id, s.state, s.live, s.engine,
                            s.execution_id, s.started_at_ms, s.ended_at_ms, s.last_event_at_ms,
                            st.observation ->> 'judge' as judge,
                            (st.observation ->> 'confidence')::float8 as confidence,
                            coalesce((st.observation ->> 'attention')::boolean, false) as attention,
                            (st.observation ->> 'needs_human')::float8 as needs_human,
                            (st.observation ->> 'stuck')::float8 as stuck,
                            st.observation ->> 'ambiguity' as ambiguity,
                            (select left(t.prompt, 300) from spawn_read_tasks() t
                              where t.session_id = s.session_id and t.prompt is not null
                              order by t.command_id desc limit 1) as task
                       from spawn_read_status() s
                       left join spawn_read_state() st using (session_id)
                   ) f",
                &[],
            )
            .map_err(|error| backend(&error))?;
        Ok(row.get("fleet"))
    }

    /// One session's latest judgement (with the screen the judge saw) and
    /// its newest events, through the grant-filtered readers.
    pub fn tail_json(
        &self,
        session: &SessionId,
        events: i64,
    ) -> Result<serde_json::Value, StoreError> {
        let mut client = self.lock();
        let row = client
            .query_one(
                "select jsonb_build_object(
                    'session_id', $1::text,
                    'judgement', (select j.observation from spawn_read_judgements($1) j
                                   order by j.session_seq desc limit 1),
                    'events', coalesce((select jsonb_agg(e order by e.session_seq)
                                 from (select t.session_seq, t.source, t.event_name, t.detail, t.recorded_at_ms
                                         from spawn_read_timeline($1) t
                                        order by t.session_seq desc limit $2) e), '[]'::jsonb),
                    'judgements', coalesce((select jsonb_agg(j order by j.session_seq)
                                 from (select k.session_seq, k.recorded_at_ms,
                                              k.observation ->> 'state' as state,
                                              k.observation ->> 'judge' as judge,
                                              k.observation ->> 'confidence' as confidence,
                                              coalesce((k.observation ->> 'attention')::boolean, false) as attention,
                                              k.observation ->> 'attention_reason' as attention_reason
                                         from spawn_read_judgements($1) k
                                        order by k.session_seq desc limit 10) j), '[]'::jsonb),
                    'commands', coalesce((select jsonb_agg(c order by c.command_id)
                                 from (select t.command_id, t.principal, t.action,
                                              left(t.prompt, 300) as text, t.state, t.succeeded,
                                              t.error, t.created_at_ms
                                         from spawn_read_tasks() t
                                        where t.session_id = $1
                                        order by t.command_id desc limit 20) c), '[]'::jsonb)
                 ) as tail",
                &[&session.as_str(), &events],
            )
            .map_err(|error| backend(&error))?;
        Ok(row.get("tail"))
    }

    /// Submit through the `agent_commands` gateway. The principal is the
    /// database login, checked against `spawn_grants`; the caller cannot name
    /// another one.
    pub fn submit_via_gateway(&self, submission: &Submission) -> Result<CommandId, StoreError> {
        let action = serde_json::to_value(&submission.action)
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let mut client = self.lock();
        // No RETURNING: that needs SELECT on the view, which a restricted
        // login does not hold. The id is read back through the grant-filtered
        // reader instead.
        client
            .execute(
                "insert into agent_commands (agent_id, session_id, idempotency_key, action)
                 values ($1, $2, $3, $4)",
                &[
                    &submission.agent_id.as_str(),
                    &submission.session_id.as_str(),
                    &submission.idempotency_key.as_str(),
                    &action,
                ],
            )
            .map_err(|error| backend(&error))?;
        let row = client
            .query_opt(
                "select command_id from spawn_read_tasks()
                  where session_id = $1 order by command_id desc limit 1",
                &[&submission.session_id.as_str()],
            )
            .map_err(|error| backend(&error))?;
        Ok(CommandId(row.map_or(0, |row| {
            row.get::<_, i64>("command_id").unsigned_abs()
        })))
    }

    /// The latest task typed into a session: the prompt of its newest start,
    /// resume or input command.
    pub fn latest_task(&self, session: &SessionId) -> Result<Option<String>, StoreError> {
        let mut client = self.lock();
        let row = client
            .query_opt(
                "select coalesce(action ->> 'prompt', action ->> 'text') as task
                   from spawn_commands
                  where session_id = $1
                    and action ->> 'type' in ('start', 'resume', 'input')
                    and coalesce(action ->> 'prompt', action ->> 'text') is not null
                  order by id desc limit 1",
                &[&session.as_str()],
            )
            .map_err(|error| backend(&error))?;
        Ok(row.and_then(|row| row.get("task")))
    }

    /// Terminal and workspace of a session's newest execution.
    pub fn latest_terminal(
        &self,
        session: &SessionId,
    ) -> Result<Option<(String, String)>, StoreError> {
        let mut client = self.lock();
        let row = client
            .query_opt(
                "select kind -> 'detail' ->> 'terminal' as terminal,
                        kind -> 'detail' ->> 'workspace' as workspace
                   from spawn_events
                  where session_id = $1 and event_name = 'execution.started'
                  order by id desc limit 1",
                &[&session.as_str()],
            )
            .map_err(|error| backend(&error))?;
        Ok(row.and_then(|row| {
            Some((
                row.get::<_, Option<String>>("terminal")?,
                row.get::<_, Option<String>>("workspace")?,
            ))
        }))
    }

    pub fn executions(&self, session: &SessionId) -> Result<Vec<ExecutionRow>, StoreError> {
        let rows = self
            .lock()
            .query(
                "select execution_id, command_id, engine, started_at_ms, ended_at_ms, cause,
                        exit_code, exit_reported, duration_ms
                   from agent_executions
                  where session_id = $1
                  order by started_at_ms desc, execution_id",
                &[&session.as_str()],
            )
            .map_err(|error| backend(&error))?;
        rows.iter()
            .map(|row| {
                Ok(ExecutionRow {
                    execution_id: execution(row)?
                        .ok_or_else(|| corrupt("execution.started without execution_id"))?,
                    command_id: row
                        .get::<_, Option<i64>>("command_id")
                        .map(|id| CommandId(id.unsigned_abs())),
                    engine: row.get("engine"),
                    started_at: Timestamp(row.get("started_at_ms")),
                    ended_at: row.get::<_, Option<i64>>("ended_at_ms").map(Timestamp),
                    cause: row.get("cause"),
                    exit_code: row.get("exit_code"),
                    exit_reported: row.get("exit_reported"),
                    duration_ms: row.get("duration_ms"),
                })
            })
            .collect()
    }

    /// Tool calls of one session that have no result, in sequence order.
    pub fn open_tools(&self, session: &SessionId) -> Result<Vec<OpenTool>, StoreError> {
        let rows = self
            .lock()
            .query(
                "select execution_id, tool_use_id, tool_name, session_seq, started_at_ms
                   from agent_open_tools
                  where session_id = $1
                  order by session_seq",
                &[&session.as_str()],
            )
            .map_err(|error| backend(&error))?;
        rows.iter()
            .map(|row| {
                Ok(OpenTool {
                    execution_id: execution(row)?,
                    tool_use_id: row
                        .get::<_, Option<String>>("tool_use_id")
                        .ok_or_else(|| corrupt("tool.started without tool_use_id"))?,
                    tool_name: row.get("tool_name"),
                    session_seq: seq(row, "session_seq"),
                    started_at: Timestamp(row.get("started_at_ms")),
                })
            })
            .collect()
    }

    /// Position of one session's log; `None` when it has no events.
    pub fn freshness(&self, session: &SessionId) -> Result<Option<Freshness>, StoreError> {
        let row = self
            .lock()
            .query_opt(
                "select last_session_seq, last_event_id, last_event_at_ms, event_count
                   from agent_freshness where session_id = $1",
                &[&session.as_str()],
            )
            .map_err(|error| backend(&error))?;
        Ok(row.map(|row| Freshness {
            last_session_seq: seq(&row, "last_session_seq"),
            last_event_id: position(&row, "last_event_id"),
            last_event_at: Timestamp(row.get("last_event_at_ms")),
            event_count: row.get::<_, i64>("event_count").unsigned_abs(),
        }))
    }
}
