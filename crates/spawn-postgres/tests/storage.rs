//! What uses the disk (objective 065, docs/storage.md, "What uses the
//! disk"): `spawn_host_record` takes the loop's `host.storage` samples with
//! their four numbers (and refuses one without them), folds them into no
//! table, and the store reads back the samples the growth and the fall of
//! Claude Code's files are measured from, and the database's size and its
//! largest tables.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use serde_json::{Value, json};

mod common;
use common::{admin, store};

const MIB: i64 = 1024 * 1024;

fn sample(archive_bytes: i64, projects_files: i64) -> Value {
    json!({"type": "host.storage", "archive_bytes": archive_bytes, "archive_files": 3,
           "projects_bytes": MIB, "projects_files": projects_files})
}

/// Move control record `id` to `at_ms`.
fn place(client: &mut Client, id: i64, at_ms: i64) {
    client
        .execute(
            "update spawn_events set recorded_at_ms = $1 where id = $2",
            &[&at_ms, &id],
        )
        .expect("place the record");
}

#[test]
fn samples_are_recorded_into_no_table_and_read_back_by_age() {
    let (store, schema) = store("storage_samples");
    let mut client = admin(&schema);
    let tables_before: i64 = client
        .query_one("select count(*) from mission_missions", &[])
        .expect("count")
        .get(0);
    // Three samples, at 1000, 5000 and 9000.
    let mut ids = Vec::new();
    for (at, archive, files) in [
        (1000, 5 * MIB, 40),
        (5000, 11 * MIB, 40),
        (9000, 12 * MIB, 3),
    ] {
        let id = store
            .host_record(&sample(archive, files))
            .expect("recorded");
        place(&mut client, id, at);
        ids.push(id);
    }
    // A sample without its numbers is refused, and written nowhere.
    let bad = json!({"type": "host.storage", "archive_bytes": 1, "archive_files": 1,
                     "projects_bytes": "lots", "projects_files": 1});
    assert!(store.host_record(&bad).is_err(), "a sample without numbers");
    let count: i64 = client
        .query_one(
            "select count(*) from spawn_events
              where kind -> 'detail' -> 'record' ->> 'type' = 'host.storage'",
            &[],
        )
        .expect("count")
        .get(0);
    assert_eq!(count, 3);
    let tables_after: i64 = client
        .query_one("select count(*) from mission_missions", &[])
        .expect("count")
        .get(0);
    assert_eq!(tables_before, tables_after, "folded into no table");

    // The oldest since 2000 is the one at 5000; the newest at or before
    // 6000 is the same one; nothing before 500, nothing since 10000.
    let read = store.host_storage(2000, 6000).expect("read");
    assert_eq!(
        read["oldest"],
        json!({"at_ms": 5000, "archive_bytes": 11 * MIB, "archive_files": 3,
               "projects_bytes": MIB, "projects_files": 40})
    );
    assert_eq!(read["before"]["at_ms"], 5000);
    let read = store.host_storage(0, 9000).expect("read");
    assert_eq!(read["oldest"]["at_ms"], 1000);
    assert_eq!(read["before"]["projects_files"], 3);
    let read = store.host_storage(10_000, 500).expect("read");
    assert_eq!(
        read,
        json!({"oldest": null, "before": null,
               "carrying": {"archive_bytes": null, "projects_bytes": null, "database_bytes": null}})
    );
}

/// Objective 085: each grower's growth is measured from the oldest sample
/// of the window that carries its size; an older sample without it (the
/// shape before `database_bytes`) is passed over for that one alone.
#[test]
fn the_oldest_sample_carrying_a_size_passes_over_older_ones_without_it() {
    let (store, schema) = store("storage_carrying");
    let mut client = admin(&schema);
    let with_database = |archive: i64, database: i64| {
        let mut sample = sample(archive, 40);
        sample["database_bytes"] = json!(database);
        sample
    };
    for (at, record) in [
        (500, with_database(4 * MIB, 90 * MIB)),
        (1000, sample(5 * MIB, 40)),
        (3000, with_database(6 * MIB, 100 * MIB)),
        (5000, with_database(7 * MIB, 101 * MIB)),
    ] {
        let id = store.host_record(&record).expect("recorded");
        place(&mut client, id, at);
    }
    // Since 800: the old-shape sample at 1000 is the oldest, and the
    // oldest carrying the archive's and the projects' sizes; the database's
    // is measured from 3000 (the one at 500 is outside the window).
    let read = store.host_storage(800, 0).expect("read");
    assert_eq!(read["oldest"]["at_ms"], 1000);
    assert_eq!(read["carrying"]["archive_bytes"]["at_ms"], 1000);
    assert_eq!(read["carrying"]["projects_bytes"]["at_ms"], 1000);
    assert_eq!(read["carrying"]["database_bytes"]["at_ms"], 3000);
    assert_eq!(
        read["carrying"]["database_bytes"]["database_bytes"],
        100 * MIB
    );
    assert!(read["carrying"]["archive_bytes"].get("database_bytes").is_none());
    // Since 0, the one at 500 carries all three.
    let read = store.host_storage(0, 0).expect("read");
    assert_eq!(read["carrying"]["database_bytes"]["at_ms"], 500);
    // Since 6000: none.
    let read = store.host_storage(6000, 0).expect("read");
    assert_eq!(read["carrying"]["database_bytes"], Value::Null);
}

#[test]
fn the_database_figures_are_its_size_and_its_five_largest_tables() {
    let (store, schema) = store("storage_database");
    let mut client = admin(&schema);
    let figures = store.database_size().expect("read");
    let want_bytes: i64 = client
        .query_one("select pg_database_size(current_database())", &[])
        .expect("size")
        .get(0);
    let got = figures["bytes"].as_i64().expect("bytes");
    // Other cases write to the same database meanwhile.
    assert!(
        (got - want_bytes).abs() <= want_bytes / 20,
        "{got} against {want_bytes}"
    );
    let want: Vec<Value> = client
        .query(
            "select c.relname::text, pg_total_relation_size(c.oid)
               from pg_class c join pg_namespace n on n.oid = c.relnamespace
              where c.relkind in ('r', 'p') and n.nspname = $1
              order by pg_total_relation_size(c.oid) desc, c.relname limit 5",
            &[&schema.to_string()],
        )
        .expect("tables")
        .iter()
        .map(|row| json!({"name": row.get::<_, String>(0), "bytes": row.get::<_, i64>(1)}))
        .collect();
    assert_eq!(want.len(), 5, "a migrated schema has five tables at least");
    assert_eq!(figures["tables"], Value::Array(want));
    let sizes: Vec<i64> = figures["tables"]
        .as_array()
        .expect("tables")
        .iter()
        .map(|table| table["bytes"].as_i64().expect("bytes"))
        .collect();
    assert!(
        sizes.windows(2).all(|pair| pair[0] >= pair[1]),
        "largest first: {sizes:?}"
    );
}

/// Objective 074: the live reading carries the database's name, its
/// schemas and its event log's bytes (the sum of `spawn_events.kind`'s
/// stored size); a test database's reading, through a connection of its
/// own, its name, size and schemas (other than `pg_*`,
/// `information_schema` and `public`), and an unreachable one an error that
/// does not name its URL.
#[test]
fn the_size_reading_names_the_database_its_log_and_its_schemas() {
    let (store, schema) = store("storage_reading");
    let mut client = admin(&schema);
    let figures = store.database_size().expect("read");
    let (name, schemas): (String, i64) = {
        let row = client
            .query_one(
                "select current_database()::text, (select count(*) from pg_namespace n
                   where n.nspname not like 'pg\\_%'
                     and n.nspname not in ('information_schema', 'public'))",
                &[],
            )
            .expect("name");
        (row.get(0), row.get(1))
    };
    assert_eq!(figures["name"], json!(name));
    // Other cases create and drop schemas meanwhile; this one's is counted.
    let got_schemas = figures["schemas"].as_i64().expect("schemas");
    assert!(got_schemas >= 1, "this case's schema at least: {figures}");
    assert!(
        (got_schemas - schemas).abs() <= 50,
        "{got_schemas} against {schemas}"
    );
    let log_before = figures["log_bytes"].as_i64().expect("log bytes");
    let id = store.host_record(&sample(5 * MIB, 40)).expect("recorded");
    let stored: i32 = client
        .query_one(
            "select pg_column_size(kind) from spawn_events where id = $1",
            &[&id],
        )
        .expect("size")
        .get(0);
    let log_after = store.database_size().expect("read")["log_bytes"]
        .as_i64()
        .expect("log bytes");
    assert_eq!(
        log_after - log_before,
        i64::from(stored),
        "the new record's stored size"
    );
    let want_log: i64 = client
        .query_one(
            "select coalesce(sum(pg_column_size(kind)), 0)::bigint from spawn_events",
            &[],
        )
        .expect("log")
        .get(0);
    assert_eq!(log_after, want_log);

    // The test database's reading: the same server, through its URL.
    let test = spawn_postgres::test_database_size(&common::url()).expect("read");
    assert_eq!(test["name"], json!(name));
    assert!(test["bytes"].as_i64().expect("bytes") > 0);
    assert!(test["schemas"].as_i64().expect("schemas") >= 1, "{test}");
    assert!(
        test.get("log_bytes").is_none(),
        "a test database has no log of its own"
    );
    // A server that is not there: an error, without the URL.
    let error =
        spawn_postgres::test_database_size("postgres://spawn:secret-074@127.0.0.1:1/nothing")
            .expect_err("nothing listens on port 1");
    assert!(!error.to_string().contains("secret-074"), "{error}");
    let error = spawn_postgres::test_database_size("not a url secret-074").expect_err("no URL");
    assert!(!error.to_string().contains("secret-074"), "{error}");
}
