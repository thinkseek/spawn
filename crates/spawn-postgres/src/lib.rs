//! PostgreSQL adapters for the event log and the leased command queue.
//!
//! Times travel as epoch milliseconds, so this crate needs no date library.
//! Every write goes through the SQL functions in `sql/schema.sql`; the
//! ordering guarantees live there and are shared by every writer.
//!
//! Layout: each module holds its SQL next to the code that binds its
//! parameters and maps its rows (`row` holds the shared column decoders).
//!
//! - `event_log`: the [`EventLog`] port.
//! - `command_queue`: the [`CommandQueue`] port.
//! - `sessions`: per-session read side over the `agent_*` views.
//! - `gateway`: grant-filtered reads and submissions for restricted logins.
//! - `fleet`: the dashboard and dispatcher read model.
//! - `missions`: missions, tasks and residents.
//! - `rsi`: controller state of the self-improvement loop.
//!
//! [`EventLog`]: spawn_core::port::EventLog
//! [`CommandQueue`]: spawn_core::port::CommandQueue

use std::sync::{Mutex, MutexGuard};

use postgres::types::ToSql;
use postgres::{Client, NoTls, Row};
use spawn_core::port::StoreError;

mod command_queue;
mod event_log;
mod fleet;
mod gateway;
mod missions;
mod row;
mod rsi;
mod sessions;

pub use rsi::{
    RSI_AGENT, RSI_SESSION, RsiAttempt, RsiCommand, RsiIteration, RsiIterationRow, RsiState,
};
pub use sessions::{ExecutionRow, Freshness, LivenessFacts, OpenTool, SessionState, TimelineEntry};

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

/// Statement parameters, as the driver takes them.
type Params<'a> = [&'a (dyn ToSql + Sync)];

/// One statement under the connection lock, with driver errors converted.
/// A method that needs several statements without interleaving takes
/// [`PostgresStore::lock`] itself.
impl PostgresStore {
    fn query(&self, sql: &str, params: &Params<'_>) -> Result<Vec<Row>, StoreError> {
        self.lock()
            .query(sql, params)
            .map_err(|error| backend(&error))
    }

    fn query_opt(&self, sql: &str, params: &Params<'_>) -> Result<Option<Row>, StoreError> {
        self.lock()
            .query_opt(sql, params)
            .map_err(|error| backend(&error))
    }

    fn query_one(&self, sql: &str, params: &Params<'_>) -> Result<Row, StoreError> {
        self.lock()
            .query_one(sql, params)
            .map_err(|error| backend(&error))
    }

    /// The single value of a query that returns one row with one column,
    /// typically a `jsonb` aggregate.
    fn query_json(&self, sql: &str, params: &Params<'_>) -> Result<serde_json::Value, StoreError> {
        Ok(self.query_one(sql, params)?.get(0))
    }

    /// Rows changed.
    fn execute(&self, sql: &str, params: &Params<'_>) -> Result<u64, StoreError> {
        self.lock()
            .execute(sql, params)
            .map_err(|error| backend(&error))
    }
}
