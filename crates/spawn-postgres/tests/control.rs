//! The control stream (docs/fleet-architecture.md §10.1): missions, tasks and
//! residents are events in `spawn_events`, and the `mission_*` tables are
//! their fold.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::{Client, NoTls};
use serde_json::json;
use spawn_postgres::PostgresStore;

fn url() -> String {
    std::env::var("SPAWN_TEST_DATABASE_URL")
        .expect("SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip")
}

fn store(case: &str) -> (PostgresStore, String) {
    let schema = format!("spawn_test_{}_{case}", std::process::id());
    let store = PostgresStore::connect_in_schema(&url(), &schema).expect("connect and migrate");
    (store, schema)
}

fn admin(schema: &str) -> Client {
    let mut client = Client::connect(&url(), NoTls).expect("admin connection");
    client
        .batch_execute(&format!("set search_path to {schema}"))
        .expect("select the schema");
    client
}

/// Every row of the three tables, in a stable order.
fn tables(client: &mut Client) -> serde_json::Value {
    let row = client
        .query_one(
            "select jsonb_build_object(
               'missions', (select coalesce(jsonb_agg(to_jsonb(m) order by m.id), '[]') from mission_missions m),
               'residents', (select coalesce(jsonb_agg(to_jsonb(r) order by r.name), '[]') from mission_residents r),
               'tasks', (select coalesce(jsonb_agg(to_jsonb(t) order by t.id), '[]') from mission_tasks t))",
            &[],
        )
        .expect("read the tables");
    row.get(0)
}

fn control_types(client: &mut Client) -> Vec<String> {
    client
        .query(
            "select kind -> 'detail' -> 'record' ->> 'type'
               from spawn_events
              where session_id = 'session_control' and source = 'control'
                and event_name = 'control.recorded'
              order by session_seq",
            &[],
        )
        .expect("read the control stream")
        .iter()
        .map(|row| row.get(0))
        .collect()
}

fn refold(client: &mut Client) {
    client
        .batch_execute("select spawn_control_refold()")
        .expect("refold the control stream");
}

#[test]
fn every_mission_change_is_an_event_and_the_tables_are_its_fold() {
    let (store, schema) = store("control_fold");
    let mission = store
        .mission_add("Ship it", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    store
        .resident_propose("core", "Owns the core")
        .expect("propose");
    store.resident_approve("core").expect("approve");
    let first = store
        .task_assign(mission, "resident", "core", "first")
        .expect("assign to the resident");
    let second = store
        .task_assign(mission, "ephemeral", "Probe!", "second")
        .expect("assign to an ephemeral worker");
    store.task_delivered(first, 41).expect("delivered");
    assert!(store.task_reported(first, "done once").expect("reported"));
    store.task_review(first, false, "again").expect("send back");
    store.task_delivered(first, 42).expect("delivered again");
    assert!(store.task_reported(first, "done twice").expect("reported"));
    store.task_review(first, true, "").expect("accept");
    assert!(
        !store.task_reported(first, "late").expect("a late report"),
        "an accepted task takes no report"
    );
    store.task_cancel(second).expect("cancel");
    store.resident_retire("core").expect("retire");
    store
        .mission_close(mission, "done", "shipped")
        .expect("close");

    let mut client = admin(&schema);
    assert_eq!(
        control_types(&mut client),
        [
            "mission.added",
            "resident.proposed",
            "resident.approved",
            "task.assigned",
            "task.assigned",
            "task.delivered",
            "task.reported",
            "task.reviewed",
            "task.delivered",
            "task.reported",
            "task.reviewed",
            "task.cancelled",
            "resident.retired",
            "mission.closed",
        ]
    );

    let live = tables(&mut client);
    assert_eq!(live["tasks"][0]["state"], "accepted");
    assert_eq!(live["tasks"][0]["revisions"], 1);
    assert_eq!(live["tasks"][0]["report"], "done twice");
    assert_eq!(
        live["tasks"][1]["agent_id"],
        format!("agent_worker_probe_{second}")
    );
    assert_eq!(live["missions"][0]["state"], "done");
    assert_eq!(live["residents"][0]["state"], "retired");

    refold(&mut client);
    assert_eq!(
        tables(&mut client),
        live,
        "the tables are the fold of the log"
    );

    // The stream belongs to no agent.
    let projected: i64 = client
        .query_one(
            "select count(*) from spawn_session_state where session_id = 'session_control'",
            &[],
        )
        .expect("count")
        .get(0);
    assert_eq!(projected, 0);
}

#[test]
fn a_refused_change_records_nothing() {
    let (store, schema) = store("control_refused");
    let mission = store.mission_add("One", &json!([]), "human").expect("add");
    let task = store
        .task_assign(mission, "ephemeral", "w", "brief")
        .expect("assign");
    assert!(store.task_review(task, true, "").is_err(), "no report yet");
    assert!(store.resident_approve("nobody").is_err());
    assert!(
        store
            .task_assign(mission, "resident", "nobody", "brief")
            .is_err()
    );
    let mut client = admin(&schema);
    assert_eq!(
        control_types(&mut client),
        ["mission.added", "task.assigned"]
    );
}

#[test]
fn rows_from_before_the_stream_are_imported_once() {
    let (store, schema) = store("control_import");
    let mission = store.mission_add("Old", &json!([]), "human").expect("add");
    store
        .task_assign(mission, "ephemeral", "w", "brief")
        .expect("assign");
    let mut client = admin(&schema);
    // A database from before the control stream: rows, and no events.
    client
        .batch_execute(
            "delete from spawn_events where session_id = 'session_control';
             update mission_tasks set state = 'reported', report = 'old report';",
        )
        .expect("simulate an old database");
    let before = tables(&mut client);

    store.migrate().expect("migrate");
    store.migrate().expect("migrate again");
    assert_eq!(
        control_types(&mut client),
        ["legacy.imported", "legacy.imported"]
    );
    assert_eq!(tables(&mut client), before);
    refold(&mut client);
    assert_eq!(tables(&mut client), before);

    // New work continues on the imported rows, and ids do not collide.
    let task: i64 = before["tasks"][0]["id"].as_i64().expect("task id");
    store
        .task_review(task, true, "")
        .expect("accept the old report");
    let next = store
        .task_assign(mission, "ephemeral", "w", "more")
        .expect("assign");
    assert!(next > task);
}
