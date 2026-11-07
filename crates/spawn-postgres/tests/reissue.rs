//! A first delivery spent without typing is re-issued (objective 019):
//! `spawn_task_reissue` cancels the task and assigns it again in one
//! transaction, each record naming the other, and refuses any task that is
//! not waiting for its first delivery. `spawn_source_moved` records every
//! deploy, also one that moves no mission's baseline: the dispatcher tries
//! every spent delivery once more after it.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use serde_json::{Value, json};

mod common;
use common::{admin, store};

/// The attempts of a spent delivery of `task`, as the dispatcher names them.
fn attempts(task: i64, count: i64) -> Value {
    Value::Array(
        (0..count)
            .map(|n| {
                let key = if n == 0 {
                    format!("deliver-{task}-0")
                } else {
                    format!("deliver-{task}-0-retry-{n}")
                };
                json!({"command": 100 + n, "key": key,
                       "error": "pane state Ready does not accept input: command too long"})
            })
            .collect(),
    )
}

/// The control record of event `id`, and the transaction that wrote it.
fn record(client: &mut Client, id: i64) -> (Value, String) {
    let row = client
        .query_one(
            "select kind -> 'detail' -> 'record', xmin::text from spawn_events where id = $1",
            &[&id],
        )
        .expect("the record");
    (row.get(0), row.get(1))
}

/// The event id of the latest control record of `kind` about task `task`.
fn latest(client: &mut Client, kind: &str, task: i64) -> i64 {
    client
        .query_one(
            "select max(id) from spawn_events
              where session_id = 'session_control'
                and kind -> 'detail' -> 'record' ->> 'type' = $1
                and kind -> 'detail' -> 'record' ->> 'id' = $2",
            &[&kind, &task.to_string()],
        )
        .expect("the latest record")
        .get::<_, Option<i64>>(0)
        .unwrap_or_else(|| panic!("no {kind} for task {task}"))
}

fn task_row(client: &mut Client, task: i64) -> Value {
    client
        .query_one(
            "select to_jsonb(t) from mission_tasks t where id = $1",
            &[&task],
        )
        .expect("the task")
        .get(0)
}

fn refusal<T: std::fmt::Debug>(result: Result<T, spawn_core::port::StoreError>) -> String {
    result.expect_err("refused").to_string()
}

#[test]
fn a_reissue_cancels_and_assigns_again_in_one_transaction_each_naming_the_other() {
    let (store, schema) = store("reissue_records");
    let mut client = admin(&schema);
    let mission = store
        .mission_add("Work", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    store.resident_propose("frail", "Owns it").expect("propose");
    store.resident_approve("frail").expect("approve");
    let old = store
        .task_assign(mission, "resident", "frail", "Write f1.", "cheap")
        .expect("assign");
    let assigned = latest(&mut client, "task.assigned", old);

    let new = store
        .task_reissue(old, &attempts(old, 14))
        .expect("re-issued");
    assert!(new > old);
    let cancelled_id = latest(&mut client, "task.cancelled", old);
    let reassigned_id = latest(&mut client, "task.assigned", new);
    let (cancelled, cancel_xact) = record(&mut client, cancelled_id);
    let (reassigned, assign_xact) = record(&mut client, reassigned_id);
    assert_eq!(cancel_xact, assign_xact, "one transaction");
    assert_eq!(cancelled["by"], "spawn-dispatch");
    assert_eq!(cancelled["reissued_as"], new);
    assert_eq!(cancelled["attempts"], attempts(old, 14));
    assert_eq!(reassigned["reissue_of"], old);
    let (original, _) = record(&mut client, assigned);
    for field in [
        "mission_id",
        "kind",
        "agent_id",
        "session_id",
        "brief",
        "model",
        "created_by",
    ] {
        assert_eq!(reassigned[field], original[field], "{field}");
    }
    assert_eq!(task_row(&mut client, old)["state"], "cancelled");
    let row = task_row(&mut client, new);
    assert_eq!(row["state"], "assigned");
    assert_eq!(row["reissue_of"], old);
    assert_eq!(row["model"], "cheap");
    assert_eq!(row["session_id"], "session_resident_frail");

    // The dispatcher reads it back as a re-issue, with the attempts before.
    let rows = store.delivery_rows().expect("delivery rows");
    let found = rows
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| row["id"] == new)
        .expect("the re-issue is waiting");
    assert_eq!(found["reissue_of"], old);
    assert_eq!(found["reissued_attempts"], attempts(old, 14));
    assert!(
        rows.as_array()
            .expect("rows")
            .iter()
            .all(|row| row["id"] != old),
        "the cancelled resident task is closed"
    );

    // The tables are the fold of the records.
    let before: Value = client
        .query_one(
            "select jsonb_agg(to_jsonb(t) order by t.id) from mission_tasks t",
            &[],
        )
        .expect("tasks")
        .get(0);
    client
        .batch_execute("select spawn_control_refold()")
        .expect("refold");
    let after: Value = client
        .query_one(
            "select jsonb_agg(to_jsonb(t) order by t.id) from mission_tasks t",
            &[],
        )
        .expect("tasks")
        .get(0);
    assert_eq!(after, before);
}

#[test]
fn only_a_task_waiting_for_its_first_delivery_is_reissued_and_only_once() {
    let (store, schema) = store("reissue_refused");
    let mission = store
        .mission_add("Work", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    let delivered = store
        .task_assign(mission, "ephemeral", "w", "delivered", "default")
        .expect("assign");
    store.task_delivered(delivered, 1).expect("delivered");
    let refused = refusal(store.task_reissue(delivered, &attempts(delivered, 14)));
    assert!(
        refused.contains("not waiting for its first delivery"),
        "{refused}"
    );

    // Sent back: a revision's delivery is escalated, never re-issued.
    assert!(store.task_reported(delivered, "done").expect("reported"));
    store
        .task_review(delivered, false, "again", None)
        .expect("send back");
    let refused = refusal(store.task_reissue(delivered, &attempts(delivered, 14)));
    assert!(
        refused.contains("not waiting for its first delivery"),
        "{refused}"
    );

    let cancelled = store
        .task_assign(mission, "ephemeral", "w", "cancelled", "default")
        .expect("assign");
    store.task_cancel(cancelled).expect("cancel");
    assert!(
        store
            .task_reissue(cancelled, &attempts(cancelled, 14))
            .is_err()
    );
    assert!(store.task_reissue(9_999, &attempts(9_999, 14)).is_err());

    // No attempts named: nothing to rest the re-issue on.
    let waiting = store
        .task_assign(mission, "ephemeral", "w", "waiting", "default")
        .expect("assign");
    let refused = refusal(store.task_reissue(waiting, &json!([])));
    assert!(refused.contains("names no attempts"), "{refused}");

    // An ephemeral task gets a new worker with the same label; the re-issue
    // is never re-issued again.
    let new = store
        .task_reissue(waiting, &attempts(waiting, 14))
        .expect("re-issued");
    let mut client = admin(&schema);
    let row = task_row(&mut client, new);
    assert_eq!(row["agent_id"], format!("agent_worker_w_{new}"));
    assert_eq!(row["session_id"], format!("session_worker_w_{new}"));
    assert_eq!(row["kind"], "ephemeral");
    let again = refusal(store.task_reissue(new, &attempts(new, 14)));
    assert!(again.contains("not re-issued again"), "{again}");

    // A closed mission's task is not assigned again.
    let closing = store
        .task_assign(mission, "ephemeral", "w", "closing", "default")
        .expect("assign");
    store.mission_close(mission, "done", "over").expect("close");
    assert!(store.task_reissue(closing, &attempts(closing, 14)).is_err());

    // The dispatcher's, the owner's: the supervisor may not re-issue.
    let other = store
        .mission_add("Other", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    let task = store
        .task_assign(other, "ephemeral", "w", "brief", "default")
        .expect("assign");
    let supervisor = common::login(&schema, "spawn_supervisor");
    client
        .batch_execute(&format!("set session authorization {supervisor}"))
        .expect("become the supervisor");
    assert!(
        client
            .query_one(
                "select spawn_task_reissue($1, $2)",
                &[&task, &attempts(task, 14)]
            )
            .is_err(),
        "the supervisor re-issues nothing"
    );
}

/// An objective task's re-issue keeps the objective in flight: it carries
/// the objective and its scenario, and the integrator is refused a second
/// task for that objective beside it.
#[test]
fn an_objective_tasks_reissue_keeps_its_objective_in_flight() {
    let (store, schema) = store("reissue_objective");
    let mission = store
        .mission_add("Improve", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    store
        .resident_propose("rsi", "Pursues objectives")
        .expect("propose");
    store.resident_approve("rsi").expect("approve");
    let objectives = json!({"backlog": "/controller/rsi/backlog", "resident": "rsi"});
    store
        .mission_configure(mission, Some("/scenarios"), "gate", None, Some(&objectives))
        .expect("configure");
    let old = store
        .objective_assign(mission, 19, Some("/scenarios/019.sh"), "Objective 19")
        .expect("assign");
    let new = store
        .task_reissue(old, &attempts(old, 14))
        .expect("re-issued");
    let mut client = admin(&schema);
    let row = task_row(&mut client, new);
    assert_eq!(row["objective"], 19);
    assert_eq!(row["scenario"], "/scenarios/019.sh");
    assert_eq!(row["created_by"], "spawn-integrate");
    assert_eq!(row["agent_id"], "agent_resident_rsi");
    let doubled = refusal(store.objective_assign(mission, 19, None, "again"));
    assert!(
        doubled.contains("already has a task in flight"),
        "{doubled}"
    );
}

/// Every deploy is recorded, also one that moves no mission's baseline
/// (no mission has one yet): the record names no missions, and the fold
/// moves nothing. The dispatcher reads the latest one back.
#[test]
fn a_source_move_that_moves_no_mission_is_recorded() {
    let (store, schema) = store("reissue_moved");
    let source = "/tmp/source.git";
    let mission = store
        .mission_add("Work", &json!([source]), "human")
        .expect("add");
    assert_eq!(store.latest_source_moved().expect("read"), Value::Null);
    let mut client = admin(&schema);
    let tables = |client: &mut Client| -> Value {
        client
            .query_one(
                "select jsonb_agg(to_jsonb(m) order by m.id) from mission_missions m",
                &[],
            )
            .expect("missions")
            .get(0)
    };
    let before = tables(&mut client);
    assert_eq!(
        store.source_moved(source, "b0", "b1").expect("moved"),
        0,
        "no mission has a baseline"
    );
    let moved = store.latest_source_moved().expect("read");
    let id = moved["id"].as_i64().expect("an event id");
    assert!(moved["at_ms"].is_i64(), "{moved}");
    let (recorded, _) = record(&mut client, id);
    assert_eq!(recorded["type"], "source.moved");
    assert_eq!(recorded["source"], source);
    assert_eq!(recorded["from"], "b0");
    assert_eq!(recorded["to"], "b1");
    assert_eq!(recorded["missions"], json!([]));
    let after = tables(&mut client);
    assert_eq!(
        after[0]["baseline"], before[0]["baseline"],
        "mission {mission} did not move"
    );
    assert_eq!(after[0]["baseline"], Value::Null);

    // The next deploy is the latest.
    store.source_moved(source, "b1", "b2").expect("moved");
    let next = store.latest_source_moved().expect("read");
    assert!(next["id"].as_i64().expect("an event id") > id);
}
