//! Shared by the database test files: a real server is required
//! (`SPAWN_TEST_DATABASE_URL`), and every case runs in a schema of its own.

#![allow(dead_code)]

use postgres::{Client, NoTls};
use spawn_postgres::PostgresStore;

pub fn url() -> String {
    std::env::var("SPAWN_TEST_DATABASE_URL")
        .expect("SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip")
}

/// A schema name of its own. Not named by the pid alone: in the gate's PID
/// namespace the pid is the same on every run, and a schema an earlier run
/// left would be reused.
pub fn schema_name(case: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    format!("spawn_test_{}_{nanos:x}_{case}", std::process::id())
}

/// Store in a schema of its own, and the schema's name: a guard that drops
/// the schema when the test lets go of it ([`Schema`]).
pub fn store(case: &str) -> (PostgresStore, Schema) {
    let schema = schema_name(case);
    let store = PostgresStore::connect_in_schema(&url(), &schema).expect("connect and migrate");
    (store, Schema(schema))
}

/// A test's schema, by name (it derefs to `str`), dropped with everything
/// in it when the test ends, passed or failed. Before it, every case left
/// its schema behind: a shared test database grew without bound (2,342 of
/// them, 2.1 GB, on one server by 2026-11-09). A failed case's schema is
/// not kept either: the test process ends with the case, so nothing reads
/// it, and the case run again builds the same state. The drop waits at most
/// 5 s for a lock (a connection of the test still open in another thread)
/// and gives up quietly: a schema left then is the old behaviour, never a
/// failed test.
pub struct Schema(String);

impl std::ops::Deref for Schema {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Schema {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Drop for Schema {
    fn drop(&mut self) {
        if let Ok(mut client) = Client::connect(&url(), NoTls) {
            let _ = client.batch_execute(&format!(
                "set lock_timeout = '5s'; drop schema if exists {} cascade",
                self.0
            ));
        }
    }
}

/// An owner connection inside the case's schema.
pub fn admin(schema: &str) -> Client {
    let mut client = Client::connect(&url(), NoTls).expect("admin connection");
    client
        .batch_execute(&format!("set search_path to {schema}"))
        .expect("select the schema");
    client
}

/// The test URL with `schema` as its search path, as an install's URL
/// names its schema.
pub fn schema_url(schema: &str) -> String {
    let url = url();
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}options=-csearch_path%3D{schema}")
}

/// This database's role for a restricted login (`spawn_supervisor`,
/// `spawn_reader`), as the migrated `schema` names it.
pub fn login(schema: &str, base: &str) -> String {
    admin(schema)
        .query_one("select spawn_login_role($1)", &[&base])
        .expect("the database's login")
        .get(0)
}

/// A connection logged in as the database's restricted login `base`
/// (`spawn_supervisor`, `spawn_reader`, `spawn_human`) in `schema`, by its
/// own password ([`spawn_postgres::role_url`]), as an install's login
/// connects: `session_user` is the login. Not `SET SESSION AUTHORIZATION`,
/// which needs a superuser; the test server's login need not be one (the
/// fleet's is not: deploy/fleet-test-db.sh).
pub fn as_login(schema: &str, base: &str) -> Client {
    let url = spawn_postgres::role_url(&schema_url(schema), base).expect("the login's URL");
    Client::connect(&url, NoTls).expect("log in as the restricted login")
}

/// A random password for a role a test makes: hex, nothing to quote.
pub fn password() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!("{:x}{nanos:x}", std::process::id())
}

/// A connection logged in as `role` with `password` (a role the test
/// made with `login password`), in `schema`.
pub fn connect_as(schema: &str, role: &str, password: &str) -> Client {
    let mut config: postgres::Config = url().parse().expect("the test URL parses");
    config
        .user(role)
        .password(password)
        .options(&format!("-csearch_path={schema}"));
    config.connect(NoTls).expect("log in as the role")
}

/// A schema holding the first shape of every table (tests/first-shapes.sql),
/// not migrated yet, and an owner connection in it; the schema is dropped
/// with its guard, as [`store`]'s is.
pub fn first_shapes(case: &str) -> (Schema, Client) {
    let schema = schema_name(case);
    let mut client = Client::connect(&url(), NoTls).expect("admin connection");
    client
        .batch_execute(&format!(
            "create schema {schema}; set search_path to {schema}; {}",
            include_str!("../first-shapes.sql")
        ))
        .expect("the first shapes");
    (Schema(schema), client)
}

/// Run the schema over `schema` as the owner and return its warnings, but
/// those about the cluster-wide logins of earlier installs, which the
/// shared test cluster has (tests/roles.rs covers them).
pub fn migrate_warnings(schema: &str) -> Vec<String> {
    let mut warnings = all_migrate_warnings(schema);
    warnings.retain(|warning| !warning.contains("from an earlier install"));
    warnings
}

/// Run the schema over `schema` as the owner and return all its warnings.
pub fn all_migrate_warnings(schema: &str) -> Vec<String> {
    let warnings = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&warnings);
    let mut config: postgres::Config = url().parse().expect("the test URL parses");
    config.notice_callback(move |notice| {
        if notice.parsed_severity() == Some(postgres::error::Severity::Warning) {
            sink.lock()
                .expect("warnings")
                .push(notice.message().to_string());
        }
    });
    let mut client = config.connect(NoTls).expect("owner connection");
    client
        .batch_execute(&format!(
            "set search_path to {schema}; {}",
            include_str!("../../sql/schema.sql")
        ))
        .expect("migrate");
    drop(client);
    std::mem::take(&mut *warnings.lock().expect("warnings"))
}
