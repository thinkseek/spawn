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
//! - `missions`: missions, tasks and residents, and the integrator's lock.
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
mod sessions;

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
    /// The password in the URL this store connected with: the owner's, when
    /// it migrates, and the key the logins' passwords are derived from.
    password: String,
}

/// The schema's restricted logins. Their passwords are derived from the
/// owner's ([`role_url`]) and set on every migration.
const LOGINS: [&str; 2] = ["spawn_supervisor", "spawn_reader"];

impl PostgresStore {
    /// Connect to the database in the URL.
    pub fn connect(url: &str) -> Result<Self, StoreError> {
        let client = Client::connect(url, NoTls).map_err(|error| backend(&error))?;
        Ok(Self {
            client: Mutex::new(client),
            password: url_password(url).to_string(),
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

    /// Install or refresh the schema, and set the logins' passwords.
    /// Idempotent: every process of one install derives the same ones.
    pub fn migrate(&self) -> Result<(), StoreError> {
        let mut client = self.lock();
        client
            .batch_execute(SCHEMA)
            .map_err(|error| backend(&error))?;
        for role in LOGINS {
            // Hex only, so the literal needs no quoting beyond the quotes.
            let password = role_password(&self.password, role);
            client
                .batch_execute(&format!(
                    "alter role {role} with login password '{password}'"
                ))
                .map_err(|error| backend(&error))?;
        }
        Ok(())
    }
}

/// The password part of a `scheme://user:password@host/...` URL, or "".
fn url_password(url: &str) -> &str {
    url.split_once("://")
        .and_then(|(_, rest)| rest.rsplit_once('@'))
        .and_then(|(userinfo, _)| userinfo.split_once(':'))
        .map_or("", |(_, password)| password)
}

/// The password of one restricted login: SHA-256 over the owner's password
/// and the role name. Only a holder of the owner's credentials can form it,
/// and nothing has to be stored to agree on it.
fn role_password(owner_password: &str, role: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("spawn-login-v1\0{owner_password}\0{role}").as_bytes());
    digest.iter().fold(String::new(), |mut hex, byte| {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

/// The owner's URL with its credentials replaced by one of the schema's
/// restricted logins (`spawn_supervisor`, `spawn_reader`). The boundary is
/// the login's privileges; its password keeps anyone who lacks the owner's
/// credentials from using it, which matters once the database is reachable
/// from elsewhere.
#[must_use]
pub fn role_url(owner: &str, role: &str) -> String {
    let Some((scheme, rest)) = owner.split_once("://") else {
        return owner.to_string();
    };
    let host = rest.rsplit_once('@').map_or(rest, |(_, tail)| tail);
    let password = role_password(url_password(owner), role);
    format!("{scheme}://{role}:{password}@{host}")
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
