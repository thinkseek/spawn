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

pub use missions::LandingHold;
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
        let mut config: postgres::Config = url
            .parse()
            .map_err(|error: postgres::Error| backend(&error))?;
        // The schema warns (`raise warning`) about what a migration left for
        // a human, such as tables it kept. The many notices of an idempotent
        // migration ("already exists, skipping") stay quiet.
        config.notice_callback(|notice| {
            if notice.parsed_severity() == Some(postgres::error::Severity::Warning) {
                eprintln!("spawn: database: {}", notice.message());
            }
        });
        let client = config.connect(NoTls).map_err(|error| backend(&error))?;
        Ok(Self {
            client: Mutex::new(client),
            password: url_password(url),
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
        // No owner password (peer or trust login, PGPASSWORD): a password
        // derived from "" would be a constant anyone can compute. Such a
        // migration leaves the logins as they are: created `nologin`, or as
        // the last migration with a password set them (so processes with
        // and without one do not flip them back and forth).
        if !self.password.is_empty() {
            for role in LOGINS {
                // Hex only, so the literal needs no quoting beyond the quotes.
                let password = role_password(&self.password, role);
                client
                    .batch_execute(&format!(
                        "alter role {role} with login password '{password}'"
                    ))
                    .map_err(|error| backend(&error))?;
            }
        }
        Ok(())
    }
}

/// The password part of a `scheme://user:password@host/...` URL, decoded
/// (`%40` is `@`), or "": every spelling of one password derives the same
/// login passwords.
fn url_password(url: &str) -> String {
    let raw = url
        .split_once("://")
        .and_then(|(_, rest)| rest.rsplit_once('@'))
        .and_then(|(userinfo, _)| userinfo.split_once(':'))
        .map_or("", |(_, password)| password);
    let mut bytes = Vec::with_capacity(raw.len());
    let mut rest = raw.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        let escaped = (byte == b'%')
            .then(|| tail.get(..2))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        if let Some(decoded) = escaped {
            bytes.push(decoded);
            rest = &tail[2..];
        } else {
            bytes.push(byte);
            rest = tail;
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
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
///
/// # Errors
///
/// When the owner's URL is not `scheme://...` or carries no password: the
/// logins' passwords derive from it, and from nothing they would be public.
pub fn role_url(owner: &str, role: &str) -> Result<String, String> {
    let password = url_password(owner);
    let Some((scheme, rest)) = owner.split_once("://").filter(|_| !password.is_empty()) else {
        return Err(format!(
            "{role}: the database URL must be scheme://user:password@host/... \
             (the restricted logins' passwords derive from the owner's)"
        ));
    };
    let host = rest.rsplit_once('@').map_or(rest, |(_, tail)| tail);
    Ok(format!(
        "{scheme}://{role}:{}@{host}",
        role_password(&password, role)
    ))
}

/// The identity of the server `url` reaches: its `system_identifier`, set
/// once by initdb. Two URLs that spell one server differently (socket or
/// TCP, a hostname, 127.0.0.2, a LAN address) give the same identity. One
/// read, no writes; the connection gives up after 5 seconds.
///
/// # Errors
///
/// When the URL does not parse, the server is unreachable, or the login may
/// not read `pg_control_system()`.
pub fn cluster_identity(url: &str) -> Result<String, StoreError> {
    let mut config: postgres::Config = url
        .parse()
        .map_err(|error: postgres::Error| backend(&error))?;
    config.connect_timeout(std::time::Duration::from_secs(5));
    let mut client = config.connect(NoTls).map_err(|error| backend(&error))?;
    let row = client
        .query_one(
            "select system_identifier::text from pg_control_system()",
            &[],
        )
        .map_err(|error| backend(&error))?;
    row.try_get(0).map_err(|error| backend(&error))
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

#[cfg(test)]
mod tests {
    use super::{role_url, url_password};

    #[test]
    fn every_spelling_of_the_owner_password_derives_the_same_logins() {
        assert_eq!(url_password("postgres://o:p%40ss@h/db"), "p@ss");
        assert_eq!(url_password("postgres://o:p@ss@h/db"), "p@ss");
        assert_eq!(
            role_url("postgres://o:p%40ss@h:5432/db", "spawn_reader"),
            role_url("postgres://o:p@ss@h:5432/db", "spawn_reader")
        );
        assert_eq!(
            url_password("postgres://o:100%@h/db"),
            "100%",
            "a stray % stays"
        );
    }

    #[test]
    fn no_owner_password_means_no_login_url() {
        assert!(role_url("postgres://spawn@localhost/spawn", "spawn_reader").is_err());
        assert!(role_url("host=/run/postgresql dbname=spawn", "spawn_reader").is_err());
        let url = role_url("postgres://o:pw@h:1/db?sslmode=disable", "spawn_reader").unwrap();
        assert!(url.starts_with("postgres://spawn_reader:"));
        assert!(url.ends_with("@h:1/db?sslmode=disable"));
    }
}
