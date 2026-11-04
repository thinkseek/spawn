//! Shared by the database test files: a real server is required
//! (`SPAWN_TEST_DATABASE_URL`), and every case runs in a schema of its own.

#![allow(dead_code)]

use postgres::{Client, NoTls};
use spawn_postgres::PostgresStore;

pub fn url() -> String {
    std::env::var("SPAWN_TEST_DATABASE_URL")
        .expect("SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip")
}

/// Store in a schema of its own. Not named by the pid alone: in the gate's
/// PID namespace the pid is the same on every run, and a schema an earlier
/// run left would be reused.
pub fn store(case: &str) -> (PostgresStore, String) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let schema = format!("spawn_test_{}_{nanos:x}_{case}", std::process::id());
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
