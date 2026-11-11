//! The schema guard: a case that makes its schema some other way than
//! `store` (through `schema_url` alone, as a burst of migrations does)
//! leaves nothing behind once its guard goes.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`).

use postgres::{Client, NoTls};
use spawn_postgres::PostgresStore;

mod common;
use common::{Schema, schema_url, url};

fn exists(schema: &str) -> bool {
    Client::connect(&url(), NoTls)
        .expect("a connection")
        .query_one(
            "select exists (select 1 from pg_namespace where nspname = $1)",
            &[&schema],
        )
        .expect("the catalog")
        .get(0)
}

#[test]
fn a_schema_made_through_its_url_alone_is_dropped_with_its_guard() {
    let schema = Schema::new("guard_url");
    let name = schema.to_string();
    assert!(!exists(&name), "a fresh name names no schema yet");
    PostgresStore::connect(&schema_url(&schema))
        .expect("connect")
        .migrate()
        .expect("the migration makes the schema");
    assert!(exists(&name), "the migration made it");
    drop(schema);
    assert!(!exists(&name), "the guard dropped it");
}

#[test]
fn several_schemas_of_one_case_are_each_dropped_with_their_guards() {
    let schemas: Vec<Schema> = (0..3)
        .map(|number| Schema::new(&format!("guard_many_{number}")))
        .collect();
    let names: Vec<String> = schemas.iter().map(ToString::to_string).collect();
    for schema in &schemas {
        PostgresStore::connect(&schema_url(schema))
            .expect("connect")
            .migrate()
            .expect("migrate");
    }
    assert!(names.iter().all(|name| exists(name)), "each was made");
    drop(schemas);
    assert!(
        names.iter().all(|name| !exists(name)),
        "each was dropped: {names:?}"
    );
}

#[test]
fn a_stores_schema_is_dropped_with_its_guard() {
    let (store, schema) = common::store("guard_store");
    let name = schema.to_string();
    assert!(exists(&name));
    drop(store);
    drop(schema);
    assert!(!exists(&name), "the store's guard dropped its schema");
}
