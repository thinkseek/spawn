//! Shared by the database test files: a real server is required
//! (`SPAWN_TEST_DATABASE_URL`), and every case runs in a schema of its own.

#![allow(dead_code)]
#![allow(clippy::disallowed_methods)]

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

/// Store in a schema of its own, and the schema's guard ([`Schema`]):
/// dropped when the test passes, kept (and named) when it fails.
pub fn store(case: &str) -> (PostgresStore, Schema) {
    let schema = Schema::new(case);
    let store = schema.store();
    (store, schema)
}

/// A test's schema, by name (it derefs to `str`): the crate's guard
/// ([`spawn_postgres::testing::TestSchema`], backlog P19). Before it, every
/// case left its schema behind (2,342 of them, 2.1 GB, on one server by
/// 2026-11-09). A case that makes its schema some other way than [`store`]
/// (through [`schema_url`] alone, say) holds `Schema::new(case)` and lets it
/// go after its connections to the schema have closed.
pub use spawn_postgres::testing::TestSchema as Schema;

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

/// The base names of a database's restricted logins, as `migrate` makes
/// them (`spawn_login_role(<base>)`): every test that lists, drops or
/// counts a database's logins names all of them, so none is left behind on
/// the shared server.
pub const LOGIN_BASES: [&str; 4] = [
    "spawn_supervisor",
    "spawn_reader",
    "spawn_human",
    "spawn_node_gateway",
];

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
/// own password ([`spawn_postgres::role_url_with_passfile`]), as an install's login
/// connects: `session_user` is the login. Not `SET SESSION AUTHORIZATION`,
/// which needs a superuser; the test server's login need not be one (the
/// fleet's is not: deploy/fleet-test-db.sh).
pub fn as_login(schema: &str, base: &str) -> Client {
    let url = spawn_postgres::role_url_with_passfile(&schema_url(schema), base, None)
        .expect("the login's URL");
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
/// with its guard, as [`store`]'s is. Made as every test schema is
/// ([`spawn_postgres::made_schema_sql`]): its comment dates it and the
/// connection carries its name (objective 094).
pub fn first_shapes(case: &str) -> (Schema, Client) {
    let schema = Schema::new(case);
    let mut client = Client::connect(&url(), NoTls).expect("admin connection");
    client
        .batch_execute(&format!(
            "{}; set search_path to {schema}; {}",
            spawn_postgres::made_schema_sql(&schema),
            include_str!("../first-shapes.sql")
        ))
        .expect("the first shapes");
    (schema, client)
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
