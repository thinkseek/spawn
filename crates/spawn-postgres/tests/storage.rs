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
        .query_one(
            "select count(*) from mission_missions",
            &[],
        )
        .expect("count")
        .get(0);
    // Three samples, at 1000, 5000 and 9000.
    let mut ids = Vec::new();
    for (at, archive, files) in [(1000, 5 * MIB, 40), (5000, 11 * MIB, 40), (9000, 12 * MIB, 3)] {
        let id = store.host_record(&sample(archive, files)).expect("recorded");
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
    assert_eq!(read, json!({"oldest": null, "before": null}));
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
    assert!(sizes.windows(2).all(|pair| pair[0] >= pair[1]), "largest first: {sizes:?}");
}
