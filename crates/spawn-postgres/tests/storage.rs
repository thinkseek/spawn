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
               "carrying": {"archive_bytes": null, "projects_bytes": null, "database_bytes": null,
                            "session_backup_bytes": null}})
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
    assert!(
        read["carrying"]["archive_bytes"]
            .get("database_bytes")
            .is_none()
    );
    // Since 0, the one at 500 carries all three.
    let read = store.host_storage(0, 0).expect("read");
    assert_eq!(read["carrying"]["database_bytes"]["at_ms"], 500);
    // Since 6000: none.
    let read = store.host_storage(6000, 0).expect("read");
    assert_eq!(read["carrying"]["database_bytes"], Value::Null);
}

/// Objective 097: the session backups' growth is measured from the oldest
/// sample of the window carrying `session_backup_bytes`; samples without it
/// (before the field, or a status file that did not read) are passed over
/// for it alone.
#[test]
fn the_oldest_sample_carrying_the_session_backups_is_found() {
    let (store, schema) = store("storage_session_backups");
    let mut client = admin(&schema);
    let with_backups = |archive: i64, backups: i64| {
        let mut sample = sample(archive, 40);
        sample["database_bytes"] = json!(100 * MIB);
        sample["session_backup_bytes"] = json!(backups);
        sample
    };
    for (at, record) in [
        (500, with_backups(4 * MIB, 30 * MIB)),
        (1000, sample(5 * MIB, 40)),
        (2000, {
            let mut sample = sample(6 * MIB, 40);
            sample["database_bytes"] = json!(100 * MIB);
            sample
        }),
        (3000, with_backups(7 * MIB, 36 * MIB)),
        (5000, with_backups(8 * MIB, 38 * MIB)),
    ] {
        let id = store.host_record(&record).expect("recorded");
        place(&mut client, id, at);
    }
    let read = store.host_storage(800, 0).expect("read");
    assert_eq!(read["carrying"]["archive_bytes"]["at_ms"], 1000);
    assert_eq!(read["carrying"]["database_bytes"]["at_ms"], 2000);
    assert_eq!(read["carrying"]["session_backup_bytes"]["at_ms"], 3000);
    assert_eq!(
        read["carrying"]["session_backup_bytes"]["session_backup_bytes"],
        36 * MIB
    );
    // Since 0, the one at 500 carries it; since 6000, none does.
    let read = store.host_storage(0, 0).expect("read");
    assert_eq!(read["carrying"]["session_backup_bytes"]["at_ms"], 500);
    let read = store.host_storage(6000, 0).expect("read");
    assert_eq!(read["carrying"]["session_backup_bytes"], Value::Null);
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

/// The names of every index a JSON plan uses.
fn plan_indexes(plan: &Value, out: &mut Vec<String>) {
    match plan {
        Value::Object(map) => {
            for (name, value) in map {
                if name == "Index Name"
                    && let Some(index) = value.as_str()
                {
                    out.push(index.to_string());
                }
                plan_indexes(value, out);
            }
        }
        Value::Array(list) => list.iter().for_each(|value| plan_indexes(value, out)),
        _ => {}
    }
}

/// Objective 090: the loop's `host.core_lines` samples go through
/// `spawn_host_record` (one without its values object is refused), fold
/// into no table, and are read back since a time, oldest first, through
/// their own partial index.
#[test]
fn core_lines_samples_are_recorded_and_read_back_through_their_index() {
    let (store, schema) = store("storage_core_lines");
    let mut client = admin(&schema);
    let tables_before: i64 = client
        .query_one("select count(*) from mission_missions", &[])
        .expect("count")
        .get(0);
    for (at, free) in [(1000, 5), (5000, 6), (9000, 7)] {
        let id = store
            .host_record(&json!({"type": "host.core_lines",
                                 "values": {"storage.free_bytes": free, "storage.days_left": 10}}))
            .expect("recorded");
        place(&mut client, id, at);
    }
    for bad in [
        json!({"type": "host.core_lines"}),
        json!({"type": "host.core_lines", "values": [1, 2]}),
        json!({"type": "host.core_lines", "values": null}),
    ] {
        assert!(store.host_record(&bad).is_err(), "refused: {bad}");
    }
    let count: i64 = client
        .query_one(
            "select count(*) from spawn_events
              where kind -> 'detail' -> 'record' ->> 'type' = 'host.core_lines'",
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

    let read = store.host_core_lines(2000).expect("read");
    assert_eq!(
        read,
        json!([
            {"at_ms": 5000, "values": {"storage.free_bytes": 6, "storage.days_left": 10}},
            {"at_ms": 9000, "values": {"storage.free_bytes": 7, "storage.days_left": 10}},
        ])
    );
    assert_eq!(store.host_core_lines(10_000).expect("read"), json!([]));

    // The partial index exists, and the 7-day read goes through it.
    let definition: String = client
        .query_one(
            "select indexdef from pg_indexes
              where schemaname = $1 and indexname = 'spawn_events_host_core_lines'",
            &[&&*schema],
        )
        .expect("the index exists")
        .get(0);
    assert!(definition.contains("recorded_at_ms"), "{definition}");
    assert!(definition.contains("host.core_lines"), "{definition}");
    // A control stream of some size, mostly other records, as the live
    // one is: the read must not walk it.
    client
        .batch_execute(
            "insert into spawn_events (agent_id, session_id, execution_id, source,
                                       source_event_id, kind, session_seq, recorded_at_ms)
             select 'agent_control', 'session_control', null, 'control', null,
                    jsonb_build_object('name', 'control.recorded', 'detail',
                        jsonb_build_object('record', jsonb_build_object('type', 'host.storage',
                            'archive_bytes', n, 'archive_files', 1,
                            'projects_bytes', 1, 'projects_files', 1))),
                    m.seq + n, 3000 + n
               from generate_series(1, 5000) n,
                    (select max(session_seq) as seq from spawn_events
                      where session_id = 'session_control') m;
             analyze spawn_events;",
        )
        .expect("a stream of other records");
    let plan: Value = client
        .query_one(
            "explain (format json)
             select e.recorded_at_ms, e.kind -> 'detail' -> 'record' -> 'values'
               from spawn_events e
              where e.session_id = 'session_control' and e.event_name = 'control.recorded'
                and e.kind -> 'detail' -> 'record' ->> 'type' = 'host.core_lines'
                and e.recorded_at_ms >= $1",
            &[&2000_i64],
        )
        .expect("the plan")
        .get(0);
    let mut indexes = Vec::new();
    plan_indexes(&plan, &mut indexes);
    assert!(
        indexes.contains(&"spawn_events_host_core_lines".to_string()),
        "the samples' index unused: {indexes:?}"
    );
}

/// Objective 180: the loop's daily `host.storage_bytes` readings go through
/// `spawn_host_record` (each source with bytes or why it is not measured;
/// any other shape refused), fold into no table, and the newest is read
/// back with the database's clock through its own partial index.
#[test]
fn storage_readings_are_recorded_and_the_newest_read_back() {
    let (store, schema) = store("storage_bytes");
    let mut client = admin(&schema);
    let empty = store.host_storage_bytes().expect("read");
    assert_eq!(empty["newest"], Value::Null, "{empty}");
    assert!(
        empty["now_ms"].as_i64().is_some_and(|now| now > 0),
        "{empty}"
    );
    let tables_before: i64 = client
        .query_one("select count(*) from mission_missions", &[])
        .expect("count")
        .get(0);
    let reading = |scratch: i64| {
        json!({"type": "host.storage_bytes", "sources": {
            "scratch": {"bytes": scratch, "files": 3, "unreadable": 0, "seconds": 0.5},
            "session_backups": {"not_measured": "no status file is configured"},
            "live_database": {"bytes": 300}}})
    };
    for (at, scratch) in [(1000, 8 * MIB), (9000, 9 * MIB), (5000, 7 * MIB)] {
        let id = store.host_record(&reading(scratch)).expect("recorded");
        place(&mut client, id, at);
    }
    for bad in [
        json!({"type": "host.storage_bytes"}),
        json!({"type": "host.storage_bytes", "sources": []}),
        json!({"type": "host.storage_bytes", "sources": {}}),
        json!({"type": "host.storage_bytes", "sources": {"scratch": 5}}),
        json!({"type": "host.storage_bytes", "sources": {"scratch": {"bytes": "lots"}}}),
        json!({"type": "host.storage_bytes", "sources": {"scratch": {"bytes": -1}}}),
        json!({"type": "host.storage_bytes", "sources": {"scratch": {"files": 3}}}),
        json!({"type": "host.storage_bytes", "sources": {"scratch": {"not_measured": 3}}}),
        json!({"type": "host.storage_bytes",
               "sources": {"scratch": {"bytes": 1, "not_measured": "both"}}}),
        json!({"type": "host.storage_bytes", "sources": {"": {"bytes": 1}}}),
    ] {
        assert!(store.host_record(&bad).is_err(), "refused: {bad}");
    }
    let count: i64 = client
        .query_one(
            "select count(*) from spawn_events
              where kind -> 'detail' -> 'record' ->> 'type' = 'host.storage_bytes'",
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
    // The newest is the last recorded (its place in the stream), with its
    // time.
    let read = store.host_storage_bytes().expect("read");
    assert_eq!(read["newest"]["at_ms"], 5000, "{read}");
    assert_eq!(read["newest"]["sources"]["scratch"]["bytes"], 7 * MIB);
    assert_eq!(
        read["newest"]["sources"]["session_backups"],
        json!({"not_measured": "no status file is configured"})
    );
    let clock: i64 = client
        .query_one(
            "select (floor(extract(epoch from clock_timestamp()) * 1000))::bigint",
            &[],
        )
        .expect("clock")
        .get(0);
    let now = read["now_ms"].as_i64().expect("the database's clock");
    assert!((clock - now).abs() < 60_000, "{now} against {clock}");

    the_readings_index_is_used(&store, &mut client, &schema);
}

/// The readings' partial index exists, and the newest is read through it in
/// a stream of 5,000 other records.
fn the_readings_index_is_used(
    store: &spawn_postgres::PostgresStore,
    client: &mut Client,
    schema: &str,
) {
    // The partial index exists, and the read goes through it in a stream of
    // other records.
    let definition: String = client
        .query_one(
            "select indexdef from pg_indexes
              where schemaname = $1 and indexname = 'spawn_events_host_storage_bytes'",
            &[&schema],
        )
        .expect("the index exists")
        .get(0);
    assert!(definition.contains("session_seq"), "{definition}");
    assert!(definition.contains("host.storage_bytes"), "{definition}");
    client
        .batch_execute(
            "insert into spawn_events (agent_id, session_id, execution_id, source,
                                       source_event_id, kind, session_seq, recorded_at_ms)
             select 'agent_control', 'session_control', null, 'control', null,
                    jsonb_build_object('name', 'control.recorded', 'detail',
                        jsonb_build_object('record', jsonb_build_object('type', 'host.storage',
                            'archive_bytes', n, 'archive_files', 1,
                            'projects_bytes', 1, 'projects_files', 1))),
                    m.seq + n, 10000 + n
               from generate_series(1, 5000) n,
                    (select max(session_seq) as seq from spawn_events
                      where session_id = 'session_control') m;
             analyze spawn_events;",
        )
        .expect("a stream of other records");
    assert_eq!(
        store.host_storage_bytes().expect("read")["newest"]["at_ms"],
        5000,
        "other records are not readings"
    );
    let plan: Value = client
        .query_one(
            "explain (format json)
             select e.session_seq from spawn_events e
              where e.session_id = 'session_control' and e.event_name = 'control.recorded'
                and e.kind -> 'detail' -> 'record' ->> 'type' = 'host.storage_bytes'
              order by e.session_seq desc limit 1",
            &[],
        )
        .expect("the plan")
        .get(0);
    let mut indexes = Vec::new();
    plan_indexes(&plan, &mut indexes);
    assert!(
        indexes.contains(&"spawn_events_host_storage_bytes".to_string()),
        "the readings' index unused: {indexes:?}"
    );
}

/// One control record after every other, at `at_ms`, as the loop and the
/// integrator append them.
fn seed(client: &mut Client, record: &Value, at_ms: i64) {
    client
        .execute(
            "insert into spawn_events (agent_id, session_id, execution_id, source,
                                       source_event_id, kind, session_seq, recorded_at_ms)
             select 'agent_control', 'session_control', null, 'control', null,
                    jsonb_build_object('name', 'control.recorded', 'detail',
                        jsonb_build_object('record', $1::jsonb)),
                    coalesce(max(session_seq), 0) + 1, $2
               from spawn_events where session_id = 'session_control'",
            &[record, &at_ms],
        )
        .expect("a control record");
}

/// Objective 185: the readings read back are the newest and the newest at
/// least a day older, with the landings between them: every task.landed and
/// every owner's landing (a source.moved naming its request), never a
/// deploy's source.moved or a deploy.finished, nor one outside the two.
#[test]
fn storage_readings_are_read_back_with_the_landings_between_them() {
    const H: i64 = 3_600_000;
    let (store, schema) = store("storage_bytes_landings");
    let mut client = admin(&schema);
    let reading = |scratch: i64| json!({"type": "host.storage_bytes", "sources": {"scratch": {"bytes": scratch}}});
    seed(&mut client, &json!({"type": "task.landed", "id": 1}), 0);
    seed(&mut client, &reading(MIB), 10 * H);
    let one = store.host_storage_bytes().expect("read");
    assert_eq!(one["newest"]["at_ms"], 10 * H);
    assert_eq!(one["older"], Value::Null, "one reading: no older one");
    assert_eq!(one["landings"], 0);
    seed(
        &mut client,
        &json!({"type": "task.landed", "id": 2}),
        11 * H,
    );
    seed(
        &mut client,
        &json!({"type": "source.moved", "source": "/s", "from": "a", "to": "b",
                              "request": 7}),
        12 * H,
    );
    seed(
        &mut client,
        &json!({"type": "source.moved", "source": "/s", "from": "b", "to": "c"}),
        13 * H,
    );
    seed(
        &mut client,
        &json!({"type": "deploy.finished", "ok": true}),
        14 * H,
    );
    seed(
        &mut client,
        &json!({"type": "task.landed", "id": 3}),
        20 * H,
    );
    // A reading less than a day after the first: not a day older than the
    // next one, which is.
    seed(&mut client, &reading(2 * MIB), 30 * H);
    seed(
        &mut client,
        &json!({"type": "task.landed", "id": 4}),
        31 * H,
    );
    seed(&mut client, &reading(5 * MIB), 40 * H);
    seed(
        &mut client,
        &json!({"type": "task.landed", "id": 5}),
        41 * H,
    );
    let read = store.host_storage_bytes().expect("read");
    assert_eq!(read["newest"]["at_ms"], 40 * H);
    assert_eq!(read["newest"]["sources"]["scratch"]["bytes"], 5 * MIB);
    assert_eq!(
        read["older"]["at_ms"],
        10 * H,
        "the newest a day older, not the one 10 h before: {read}"
    );
    // Between them: task.landed 2, 3 and 4 and the owner's landing; not the
    // deploy's move, deploy.finished, task 1 (before) or task 5 (after).
    assert_eq!(read["landings"], 4, "{read}");
}

/// Objective 181: the first `host.core_lines` sample carrying a figure as a
/// number is found by when it was recorded, not by its place in the
/// stream; a sample without the figure, or with it as no number, is passed
/// over; a figure no sample carries reads none. The read goes through the
/// samples' index.
#[test]
fn the_first_sample_carrying_a_figure_is_found() {
    const DAY: i64 = 86_400_000;
    const KEY: &str = "observability.needs_you_right";
    let (store, schema) = store("storage_first_value");
    let mut client = admin(&schema);
    assert_eq!(store.first_core_lines_value(KEY).expect("read"), None);
    let sample = |values: Value| json!({"type": "host.core_lines", "values": values});
    seed(&mut client, &sample(json!({"storage.free_bytes": 5})), DAY);
    seed(&mut client, &sample(json!({KEY: null})), 2 * DAY);
    seed(&mut client, &sample(json!({KEY: "0.5"})), 3 * DAY);
    seed(
        &mut client,
        &json!({"type": "host.storage", "values": {KEY: 0.9}}),
        4 * DAY,
    );
    seed(&mut client, &sample(json!({KEY: 0.5})), 8 * DAY);
    assert_eq!(store.first_core_lines_value(KEY).expect("read"), Some(8 * DAY));
    // Appended after, recorded before: the older one is the first.
    seed(&mut client, &sample(json!({KEY: 0.4})), 6 * DAY);
    seed(&mut client, &sample(json!({KEY: 0.3})), 9 * DAY);
    assert_eq!(store.first_core_lines_value(KEY).expect("read"), Some(6 * DAY));
    assert_eq!(
        store
            .first_core_lines_value("storage.free_bytes")
            .expect("read"),
        Some(DAY)
    );
    assert_eq!(
        store.first_core_lines_value("no.such_figure").expect("read"),
        None
    );

    client
        .batch_execute("analyze spawn_events")
        .expect("analyze");
    let plan: Value = client
        .query_one(
            "explain (format json)
             select e.recorded_at_ms
               from spawn_events e
              where e.session_id = 'session_control' and e.event_name = 'control.recorded'
                and e.kind -> 'detail' -> 'record' ->> 'type' = 'host.core_lines'
                and jsonb_typeof(e.kind -> 'detail' -> 'record' -> 'values' -> $1::text) = 'number'
              order by e.recorded_at_ms
              limit 1",
            &[&KEY],
        )
        .expect("the plan")
        .get(0);
    let mut indexes = Vec::new();
    plan_indexes(&plan, &mut indexes);
    assert!(
        indexes.contains(&"spawn_events_host_core_lines".to_string()),
        "the samples' index unused: {indexes:?}"
    );
}
