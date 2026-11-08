//! Waking on change: the two indexes the dispatcher's per-session reads
//! need, made by `spawn init-db` on a fresh and on an existing database, and
//! the plan those reads get over a table large enough for the planner to
//! choose: no sequential scan of `spawn_commands`.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`), like the other
//! files in this directory.

mod common;

use common::{admin, first_shapes, migrate_warnings, store};
use serde_json::Value;

/// Every plain (not partial) index of `spawn_commands`, as its key columns
/// `a,b`.
fn plain_keys(client: &mut postgres::Client) -> Vec<String> {
    client
        .query(
            "select string_agg(a.attname, ',' order by k.ord)
               from pg_index i
               join pg_class t on t.oid = i.indrelid
               cross join lateral unnest(i.indkey::int2[]) with ordinality as k(attnum, ord)
               join pg_attribute a on a.attrelid = i.indrelid and a.attnum = k.attnum
              where t.oid = 'spawn_commands'::regclass and i.indpred is null
                and k.ord <= i.indnkeyatts
              group by i.indexrelid
              order by 1",
            &[],
        )
        .expect("the indexes")
        .iter()
        .map(|row| row.get(0))
        .collect()
}

fn assert_both_keys(schema: &str) {
    let mut client = admin(schema);
    let keys = plain_keys(&mut client);
    for want in ["session_id,id", "session_id,principal,id"] {
        assert!(
            keys.iter().any(|key| key == want),
            "no plain index on ({want}); the plain indexes are {keys:?}"
        );
    }
}

/// A fresh database gets both indexes from `spawn init-db`.
#[test]
fn init_db_makes_both_indexes_on_a_fresh_schema() {
    let (_store, schema) = store("wake_indexes_fresh");
    assert_both_keys(&schema);
}

/// A database made before them gets both on its next `spawn init-db`: the
/// first shapes of the tables, then the schema over them.
#[test]
fn init_db_makes_both_indexes_on_an_existing_schema() {
    let (schema, mut client) = first_shapes("wake_indexes_upgrade");
    assert!(
        plain_keys(&mut client)
            .iter()
            .all(|key| key != "session_id,principal,id"),
        "the first shapes have no such index: the migration must make it"
    );
    drop(client);
    migrate_warnings(&schema);
    assert_both_keys(&schema);
}

/// On a table of 20,000 commands, the dispatcher's facts read is planned
/// with no sequential scan of `spawn_commands`: every per-session read of it
/// is a range scan of one of the two keys.
#[test]
fn the_dispatch_facts_do_not_scan_the_command_table() {
    let (store, schema) = store("wake_dispatch_plan");
    let mut client = admin(&schema);
    client
        .batch_execute(
            "insert into spawn_commands
                 (principal, idempotency_key, agent_id, session_id, action, state,
                  succeeded, created_at_ms, finished_at_ms)
             select case when n % 3 = 0 then 'spawn-dispatch' else 'human' end,
                    'key-' || n,
                    'agent_resident_' || (n % 7),
                    'session_resident_' || (n % 7),
                    jsonb_build_object('type', 'input', 'text', 'task ' || n,
                                       'tags', jsonb_build_array('tag-' || (n % 11))),
                    case when n % 5 = 0 then 'pending' else 'finished' end,
                    n % 5 <> 0,
                    100000000 + n,
                    case when n % 5 = 0 then null else 100000000 + n end
               from generate_series(1, 20000) as n",
        )
        .expect("20,000 command rows");
    let rows: i64 = client
        .query_one("select count(*) from spawn_commands", &[])
        .expect("count")
        .get(0);
    assert_eq!(rows, 20_000);

    let plan = store.dispatch_facts_plan().expect("explain");
    let mut scans = Vec::new();
    collect(&plan, &mut scans);
    assert!(
        !scans
            .iter()
            .any(|node| node == "Seq Scan on spawn_commands"),
        "the dispatch facts read the command table: {scans:#?}"
    );
    assert!(
        scans.iter().any(|node| node.contains("on spawn_commands")),
        "the plan never reads the command table at all: {scans:#?}"
    );
}

/// Every plan node of an `EXPLAIN (FORMAT JSON)` document, as
/// `<node type> on <relation>`.
fn collect(node: &Value, scans: &mut Vec<String>) {
    match node {
        Value::Array(items) => {
            for item in items {
                collect(item, scans);
            }
        }
        Value::Object(map) => {
            if let Some(Value::String(kind)) = map.get("Node Type") {
                let relation = map
                    .get("Relation Name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                scans.push(format!("{kind} on {relation}"));
            }
            // Every value: `EXPLAIN`'s document nests the plan under `Plan`
            // and its child nodes under `Plans`.
            for value in map.values() {
                collect(value, scans);
            }
        }
        _ => {}
    }
}
