//! What a delivered task's worker did since the task reached it (objective
//! 211): `task_activity`, every delivered task's latest engine output and
//! the tool calls it started since its latest delivery, read by the
//! dashboard's reader login.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use serde_json::{Value, json};
use spawn_postgres::{PostgresStore, role_url};

mod common;
use common::{admin, schema_url, store};

/// The helpers the seed uses: control records and worker events in log
/// order (their ids follow it), each at `at` ms.
const HELPERS: &str = r"
create or replace function pg_temp.rec(at bigint, r jsonb) returns void language sql as $$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values ('agent_control', 'session_control', null, 'control', null,
            jsonb_build_object('name', 'control.recorded', 'detail', jsonb_build_object('record', r)),
            (select coalesce(max(session_seq), 0) + 1 from spawn_events
              where session_id = 'session_control'), at) $$;
create or replace function pg_temp.ev(w text, at bigint, src text, name text) returns void language sql as $$
    insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                              kind, session_seq, recorded_at_ms)
    values ('agent_worker_' || w, 'session_worker_' || w, 'exec_' || w, src, null,
            jsonb_build_object('name', name, 'detail', '{}'::jsonb),
            (select coalesce(max(session_seq), 0) + 1 from spawn_events
              where session_id = 'session_worker_' || w), at) $$;
";

/// Run `sql` (after [`HELPERS`]) in one transaction, then fold the control
/// records again.
fn seed(client: &mut Client, sql: &str) {
    client
        .batch_execute(&format!("begin;\n{HELPERS}\n{sql}\ncommit;"))
        .expect("seed");
    client
        .batch_execute("select spawn_control_refold()")
        .expect("refold");
}

fn assign(task: i64, worker: &str) -> String {
    format!(
        "select pg_temp.rec(1000, '{}');",
        json!({"type": "task.assigned", "id": task, "mission_id": 1, "kind": "ephemeral",
               "created_by": "human", "agent_id": format!("agent_worker_{worker}"),
               "session_id": format!("session_worker_{worker}"), "brief": format!("task {task}")})
    )
}

fn record(at: i64, value: &Value) -> String {
    format!("select pg_temp.rec({at}, '{value}');")
}

fn event(worker: &str, at: i64, source: &str, name: &str) -> String {
    format!("select pg_temp.ev('{worker}', {at}, '{source}', '{name}');")
}

fn states(client: &mut Client) -> Vec<(i64, String)> {
    client
        .query("select id, state from mission_tasks order by id", &[])
        .expect("states")
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect()
}

/// The seeded world: a mission and five tasks; task 1 delivered with its
/// worker's earlier call before the delivery and, after it, the brief
/// arriving, two calls, a usage, the Bridge's events and a judgement; task 2
/// delivered with only its brief arriving; task 3 delivered, sent back and
/// delivered again; task 4 delivered and reported; task 5 assigned.
fn world() -> String {
    let mut sql = vec![
        record(
            900,
            &json!({"type": "mission.added", "id": 1, "text": "m", "origin": "human",
                    "sources": []}),
        ),
        record(
            900,
            &json!({"type": "mission.configured", "id": 1, "accept": "review"}),
        ),
    ];
    for (task, worker) in [(1, "a"), (2, "b"), (3, "c"), (4, "d"), (5, "e")] {
        sql.push(assign(task, worker));
    }
    // Before task 1's delivery: a call of an earlier task.
    sql.push(event("a", 1_500, "claude", "tool.started"));
    sql.push(record(
        2_000,
        &json!({"type": "task.delivered", "id": 1, "command_id": 1}),
    ));
    // After it: the brief arriving (input), two calls, a Bridge call and a
    // Bridge completion of the same names (none), a busy pane and a
    // judgement (none).
    sql.push(event("a", 2_000, "claude", "message.received"));
    sql.push(event("a", 2_000, "claude", "message.taken"));
    sql.push(event("a", 2_100, "claude", "tool.started"));
    sql.push(event("a", 2_200, "claude", "tool.started"));
    sql.push(event("a", 2_300, "claude", "model.usage"));
    sql.push(event("a", 2_400, "bridge", "tool.started"));
    sql.push(event("a", 2_500, "bridge", "tool.completed"));
    sql.push(event("a", 2_600, "bridge", "screen.observed"));
    sql.push(event("a", 2_700, "observer", "observation.recorded"));
    // Task 2: delivered, only its brief arriving after.
    sql.push(record(
        3_000,
        &json!({"type": "task.delivered", "id": 2, "command_id": 2}),
    ));
    sql.push(event("b", 3_000, "claude", "message.received"));
    sql.push(event("b", 3_000, "claude", "message.taken"));
    // Task 3: delivered, a call, sent back, delivered again: it counts from
    // the latest delivery.
    sql.push(record(
        4_000,
        &json!({"type": "task.delivered", "id": 3, "command_id": 3}),
    ));
    sql.push(event("c", 4_100, "claude", "tool.started"));
    sql.push(event("c", 4_200, "claude", "turn.stopped"));
    sql.push(record(
        4_300,
        &json!({"type": "task.reported", "id": 3, "report": "r"}),
    ));
    sql.push(record(
        4_400,
        &json!({"type": "task.reviewed", "id": 3, "accepted": false, "note": "again"}),
    ));
    sql.push(record(
        5_000,
        &json!({"type": "task.delivered", "id": 3, "command_id": 4}),
    ));
    sql.push(event("c", 5_100, "claude", "job.started"));
    // Task 4: delivered and reported; task 5: assigned only.
    sql.push(record(
        6_000,
        &json!({"type": "task.delivered", "id": 4, "command_id": 5}),
    ));
    sql.push(event("d", 6_100, "claude", "tool.started"));
    sql.push(record(
        6_200,
        &json!({"type": "task.reported", "id": 4, "report": "r"}),
    ));
    sql.join("\n")
}

/// Objective 211: each delivered task's activity is its worker's latest
/// engine output (source claude) after its latest delivery and the tool
/// calls it started since. The engine's input (the brief arriving), the
/// Bridge's and the Observer's events, and anything before the delivery
/// are none; a task sent back and delivered again counts from the latest
/// delivery; a task not delivered has none; the reader login reads it.
#[test]
fn a_delivered_tasks_activity_is_its_engines_output_since_its_latest_delivery() {
    let (store, schema) = store("task_activity");
    let mut client = admin(&schema);
    seed(&mut client, &world());
    assert_eq!(
        states(&mut client),
        [
            (1, "delivered".to_string()),
            (2, "delivered".to_string()),
            (3, "delivered".to_string()),
            (4, "reported".to_string()),
            (5, "assigned".to_string()),
        ]
    );

    let activity = store.task_activity().expect("activity");
    assert_eq!(
        activity,
        json!({
            "1": {"since_ms": 2_000, "at_ms": 2_300, "event": "model.usage", "tool_calls": 2},
            "2": {"since_ms": 3_000, "at_ms": null, "event": null, "tool_calls": 0},
            "3": {"since_ms": 5_000, "at_ms": 5_100, "event": "job.started", "tool_calls": 0},
        })
    );

    // Each engine output name counts, latest by event id.
    for (at, name) in [
        (7_001, "tool.completed"),
        (7_002, "tool.failed"),
        (7_003, "turn.stopped"),
        (7_004, "turn.failed"),
        (7_005, "job.started"),
        (7_006, "job.ended"),
        (7_007, "model.usage"),
        (7_008, "tool.started"),
    ] {
        seed(&mut client, &event("b", at, "claude", name));
        let activity = store.task_activity().expect("activity");
        assert_eq!(activity["2"]["event"], name, "{activity}");
        assert_eq!(activity["2"]["at_ms"], at);
    }
    // The same name from the Bridge is none: the latest stays the engine's.
    seed(&mut client, &event("b", 7_100, "bridge", "turn.stopped"));
    let activity = store.task_activity().expect("activity");
    assert_eq!(activity["2"]["event"], "tool.started");
    assert_eq!(activity["2"]["tool_calls"], 1);

    // The dashboard's reader login reads it.
    let login = role_url(&schema_url(&schema), "spawn_reader").expect("the reader's URL");
    let reader = PostgresStore::connect(&login).expect("log in");
    assert_eq!(
        reader.task_activity().expect("the reader reads it"),
        activity
    );
}
