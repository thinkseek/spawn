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

/// Store in a schema of its own.
pub fn store(case: &str) -> (PostgresStore, String) {
    let schema = schema_name(case);
    let store = PostgresStore::connect_in_schema(&url(), &schema).expect("connect and migrate");
    (store, schema)
}

/// An owner connection inside the case's schema.
pub fn admin(schema: &str) -> Client {
    let mut client = Client::connect(&url(), NoTls).expect("admin connection");
    client
        .batch_execute(&format!("set search_path to {schema}"))
        .expect("select the schema");
    client
}

/// A schema holding the first shape of every table (tests/first-shapes.sql),
/// not migrated yet, and an owner connection in it.
pub fn first_shapes(case: &str) -> (String, Client) {
    let schema = schema_name(case);
    let mut client = Client::connect(&url(), NoTls).expect("admin connection");
    client
        .batch_execute(&format!(
            "create schema {schema}; set search_path to {schema}; {}",
            include_str!("../first-shapes.sql")
        ))
        .expect("the first shapes");
    (schema, client)
}

/// Run the schema over `schema` as the owner and return its warnings.
pub fn migrate_warnings(schema: &str) -> Vec<String> {
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
