//! A review of a task that has no report (GitHub #63, part 3 a):
//! `spawn_task_review` names the task's state ("task N is accepted: it has
//! no report to review"; one sent back "is sent back for revision"), and a
//! task that does not exist says so, through the real store, and records
//! nothing.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use serde_json::json;
use spawn_postgres::PostgresStore;

mod common;
use common::{admin, store};

fn reviewed_records(client: &mut Client) -> i64 {
    client
        .query_one(
            "select count(*) from spawn_events
              where session_id = 'session_control'
                and kind -> 'detail' -> 'record' ->> 'type' = 'task.reviewed'",
            &[],
        )
        .expect("count")
        .get(0)
}

fn refused(store: &PostgresStore, task: i64, accept: bool) -> String {
    let note = if accept { "" } else { "again" };
    store
        .task_review(task, accept, note, None)
        .expect_err("a task with no report is not reviewed")
        .to_string()
}

#[test]
fn a_task_with_no_report_is_refused_naming_its_state() {
    let (store, schema) = store("review_state");
    let mission = store.mission_add("One", &json!([]), "human").expect("add");

    let accepted = store
        .task_assign(mission, "ephemeral", "w", "first", "default")
        .expect("assign");
    store.task_delivered(accepted, 41).expect("delivered");
    assert!(store.task_reported(accepted, "done").expect("reported"));
    store.task_review(accepted, true, "", None).expect("accept");

    let delivered = store
        .task_assign(mission, "ephemeral", "w", "second", "default")
        .expect("assign");
    store.task_delivered(delivered, 42).expect("delivered");

    let sent_back = store
        .task_assign(mission, "ephemeral", "w", "third", "default")
        .expect("assign");
    store.task_delivered(sent_back, 43).expect("delivered");
    assert!(store.task_reported(sent_back, "done").expect("reported"));
    store
        .task_review(sent_back, false, "again", None)
        .expect("send back");

    let missing = sent_back + 1000;

    let mut client = admin(&schema);
    let before = reviewed_records(&mut client);

    for accept in [true, false] {
        let error = refused(&store, accepted, accept);
        assert!(
            error.contains(&format!(
                "task {accepted} is accepted: it has no report to review"
            )),
            "{error}"
        );
        let error = refused(&store, delivered, accept);
        assert!(
            error.contains(&format!(
                "task {delivered} is delivered: it has no report to review"
            )),
            "{error}"
        );
        let error = refused(&store, sent_back, accept);
        assert!(
            error.contains(&format!(
                "task {sent_back} is sent back for revision: it has no report to review"
            )),
            "{error}"
        );
        let error = refused(&store, missing, accept);
        assert!(
            error.contains(&format!(
                "task {missing} does not exist: it has no report to review"
            )),
            "{error}"
        );
    }
    assert_eq!(reviewed_records(&mut client), before, "nothing recorded");
}
