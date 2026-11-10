//! Restart and reassign (objective 42): `spawn_task_restart` records a
//! delivered task's `task.restarted` (numbered per task, naming who, why
//! and the running execution); `spawn_task_reassign` cancels a delivered
//! task and assigns it again to a fresh ephemeral worker in one
//! transaction, each record naming the other; both refuse any task that is
//! not delivered, naming its state, and record nothing. The page's
//! `spawn_human` login does either through `spawn_human_task` alone, with
//! `by` `human`; the recovery's records take the causes `restarted` and
//! `reassigned`; the session's links say what replaced what.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use postgres::error::SqlState;
use serde_json::{Value, json};
use spawn_core::event::{ArtifactRef, CheckpointReason, EventKind, NewEvent, Source};
use spawn_core::id::{AgentId, BranchName, CommitId, SessionId};
use spawn_core::port::EventLog;
use spawn_postgres::{HumanTask, PostgresStore, role_url};

mod common;
use common::{admin, schema_url, store};

/// Every control record of `kind`, oldest first, with the transaction
/// that wrote it.
fn records(client: &mut Client, kind: &str) -> Vec<(Value, String)> {
    client
        .query(
            "select kind -> 'detail' -> 'record', xmin::text from spawn_events
              where session_id = 'session_control'
                and kind -> 'detail' -> 'record' ->> 'type' = $1 order by id",
            &[&kind],
        )
        .expect("the records")
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect()
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

/// A mission with one ephemeral task (`label`), delivered.
fn delivered(store: &PostgresStore, label: &str) -> (i64, i64) {
    let mission = store.mission_add("Work", &json!([]), "human").expect("add");
    let task = store
        .task_assign(
            mission,
            "ephemeral",
            label,
            "Do the work and report.",
            "cheap",
        )
        .expect("assign");
    store.task_delivered(task, 1).expect("delivered");
    (mission, task)
}

#[test]
fn a_restart_is_recorded_per_task_and_only_for_a_delivered_task() {
    let (store, schema) = store("restart_records");
    let mut client = admin(&schema);
    let mission = store.mission_add("Work", &json!([]), "human").expect("add");
    let task = store
        .task_assign(mission, "ephemeral", "restarted", "Do it.", "default")
        .expect("assign");
    let error = refusal(store.task_restart(task, Some("too early")));
    assert!(error.contains("is assigned, not delivered"), "{error}");
    store.task_delivered(task, 1).expect("delivered");

    let answer = store
        .task_restart(task, Some("  stalled on a hung tool  "))
        .expect("restarted");
    let session = format!("session_worker_restarted_{task}");
    let agent = format!("agent_worker_restarted_{task}");
    assert_eq!(
        answer,
        json!({"task": task, "session_id": session, "agent_id": agent})
    );
    store.task_restart(task, None).expect("restarted again");
    let restarts = records(&mut client, "task.restarted");
    assert_eq!(restarts.len(), 2);
    let (first, _) = &restarts[0];
    assert_eq!(first["task"], task);
    assert_eq!(first["session_id"], session.as_str());
    assert_eq!(first["by"], "human", "the owner's CLI is a person's");
    assert_eq!(first["reason"], "stalled on a hung tool");
    assert_eq!(first["n"], 1);
    assert_eq!(
        first["execution_id"],
        Value::Null,
        "no execution runs: the worker is resumed at once"
    );
    assert_eq!(restarts[1].0["n"], 2, "numbered per task");
    assert_eq!(restarts[1].0["reason"], Value::Null);
    assert_eq!(
        task_row(&mut client, task)["state"],
        "delivered",
        "the task stays delivered"
    );

    // The dispatcher reads the latest restart with the task.
    let rows = store.delivery_rows().expect("rows");
    let row = rows
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["id"] == task))
        .expect("the task's row");
    assert_eq!(row["restart"]["n"], 2);

    store
        .task_reported(task, "## Outcome\ndone")
        .expect("reported");
    let error = refusal(store.task_restart(task, Some("again")));
    assert!(error.contains("is reported, not delivered"), "{error}");
    let error = refusal(store.task_restart(task + 1000, Some("again")));
    assert!(error.contains("does not exist"), "{error}");
    assert_eq!(
        records(&mut client, "task.restarted").len(),
        2,
        "a refusal records nothing"
    );
}

#[test]
fn a_reassign_cancels_and_assigns_again_in_one_transaction_each_naming_the_other() {
    let (store, schema) = store("reassign_records");
    let mut client = admin(&schema);
    let (mission, old) = delivered(&store, "moved");
    let answer = store
        .task_reassign(old, Some("stuck on the tests"))
        .expect("reassigned");
    let new = answer["task"].as_i64().expect("the new task");
    assert!(new > old);
    let session = format!("session_worker_moved_{new}");
    assert_eq!(
        answer,
        json!({"task": new, "session_id": session,
               "agent_id": format!("agent_worker_moved_{new}")})
    );
    let (cancelled, cancel_xact) = records(&mut client, "task.cancelled")
        .pop()
        .expect("the cancel");
    let (assigned, assign_xact) = records(&mut client, "task.assigned")
        .pop()
        .expect("the new task");
    assert_eq!(cancel_xact, assign_xact, "one transaction");
    assert_eq!(cancelled["id"], old);
    assert_eq!(cancelled["by"], "human");
    assert_eq!(cancelled["reason"], "stuck on the tests");
    assert_eq!(cancelled["reassigned_as"], new);
    assert_eq!(assigned["id"], new);
    assert_eq!(assigned["reassign_of"], old);
    assert_eq!(assigned["kind"], "ephemeral");
    assert_eq!(assigned["mission_id"], mission);
    assert_eq!(assigned["brief"], "Do the work and report.");
    assert_eq!(assigned["model"], "cheap");
    assert_eq!(assigned["created_by"], "human");
    assert_eq!(task_row(&mut client, old)["state"], "cancelled");
    let row = task_row(&mut client, new);
    assert_eq!(row["state"], "assigned");
    assert_eq!(row["reassign_of"], old);
    assert_eq!(row["session_id"], session.as_str());

    // The dispatcher reads both ends.
    let rows = store.delivery_rows().expect("rows");
    let find = |id: i64| {
        rows.as_array()
            .and_then(|rows| rows.iter().find(|row| row["id"] == id).cloned())
            .expect("the row")
    };
    let old_session = format!("session_worker_moved_{old}");
    assert_eq!(find(old)["reassigned_as"], new);
    assert_eq!(find(old)["reassigned_to_session"], session.as_str());
    assert_eq!(find(new)["reassign_of"], old);
    assert_eq!(find(new)["reassign_from_session"], old_session.as_str());

    // The session's links, both ways.
    let links = store.session_links(&old_session).expect("links");
    assert_eq!(
        links["reassigned_to"],
        json!({"task": new, "session_id": session})
    );
    assert_eq!(links["reassigned_from"], Value::Null);
    let links = store.session_links(&session).expect("links");
    assert_eq!(
        links["reassigned_from"],
        json!({"task": old, "session_id": old_session})
    );
    assert_eq!(links["restart_of"], json!({}));

    // Only a delivered task is reassigned, and a refusal records nothing.
    let error = refusal(store.task_reassign(old, Some("again")));
    assert!(error.contains("is cancelled, not delivered"), "{error}");
    let error = refusal(store.task_reassign(new, Some("again")));
    assert!(error.contains("is assigned, not delivered"), "{error}");
    assert_eq!(records(&mut client, "task.cancelled").len(), 1);
}

/// The new task starts from the old worker's last checkpoint (not a
/// set-aside), named as a reassign's.
#[test]
fn a_reassigned_task_starts_from_the_old_workers_last_checkpoint() {
    let (store, _schema) = store("reassign_fork");
    let (_, old) = delivered(&store, "forked");
    let agent = format!("agent_worker_forked_{old}");
    let session = format!("session_worker_forked_{old}");
    let commit = |reason: CheckpointReason, n: u32| {
        let commit = CommitId::new(format!("{n:040x}")).expect("valid commit");
        store
            .append(NewEvent {
                agent_id: AgentId::new(&agent).expect("valid agent"),
                session_id: SessionId::new(&session).expect("valid session"),
                execution_id: None,
                source: Source::Bridge,
                source_event_id: Some(format!("commit:{n}")),
                kind: EventKind::ArtifactCommitted {
                    reason,
                    artifact: ArtifactRef {
                        project: "missions".to_string(),
                        branch: BranchName::new(&agent).expect("valid branch"),
                        commit,
                    },
                    files: Vec::new(),
                },
            })
            .expect("append");
    };
    commit(CheckpointReason::ToolCompleted, 1);
    commit(CheckpointReason::ExecutionEnded, 2);
    let new = store.task_reassign(old, None).expect("reassigned")["task"]
        .as_i64()
        .expect("the new task");
    assert_eq!(
        store.objective_fork(new).expect("the fork"),
        json!({"task": old, "commit": format!("{:040x}", 2), "reassign": true})
    );
}

#[test]
fn the_page_restarts_and_reassigns_through_its_own_function_alone() {
    let (store, schema) = store("restart_human");
    let mut client = admin(&schema);
    let (_, task) = delivered(&store, "paged");
    let session = SessionId::new(format!("session_worker_paged_{task}")).expect("session");
    let page = PostgresStore::connect(
        &role_url(&schema_url(&schema), "spawn_human").expect("the human login's URL"),
    )
    .expect("the human login connects");
    let HumanTask::Moved(answer) = page
        .human_task(&session, "restart", Some("from the page"))
        .expect("restarted")
    else {
        panic!("the session holds a delivered task");
    };
    assert_eq!(answer["task"], task);
    let (restart, _) = records(&mut client, "task.restarted")
        .pop()
        .expect("the restart");
    assert_eq!(restart["by"], "human");
    assert_eq!(restart["reason"], "from the page");

    let supervisor = SessionId::new("session_supervisor").expect("session");
    let HumanTask::Refused(why) = page
        .human_task(&supervisor, "restart", Some("no task"))
        .expect("answered")
    else {
        panic!("the supervisor holds no delivered task");
    };
    assert!(why.contains("holds no delivered task"), "{why}");
    assert!(
        page.human_task(&session, "stop", None).is_err(),
        "a closed set of actions"
    );
    assert_eq!(records(&mut client, "task.restarted").len(), 1);

    // The page's login calls no other mission function; the owner does not
    // call the page's.
    let mut raw = Client::connect(
        &role_url(&schema_url(&schema), "spawn_human").expect("the login's URL"),
        postgres::NoTls,
    )
    .expect("connects");
    let error = raw
        .query_one("select spawn_task_restart($1, $2)", &[&task, &"x"])
        .expect_err("not the page's");
    assert_eq!(error.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE));
    let error = client
        .query_one(
            "select spawn_human_task($1, $2, $3)",
            &[&session.as_str(), &"restart", &"x"],
        )
        .expect_err("only the page");
    assert_eq!(error.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE));

    let HumanTask::Moved(answer) = page
        .human_task(&session, "reassign", Some("moved from the page"))
        .expect("reassigned")
    else {
        panic!("the session holds a delivered task");
    };
    assert_ne!(answer["task"], task);
    let (cancelled, _) = records(&mut client, "task.cancelled")
        .pop()
        .expect("the cancel");
    assert_eq!(cancelled["by"], "human");
    assert_eq!(cancelled["reassigned_as"], answer["task"]);
}

/// The supervisor's login restarts and reassigns, named as it is.
#[test]
fn the_supervisor_restarts_and_reassigns_under_its_own_name() {
    let (store, schema) = store("restart_supervisor");
    let mut client = admin(&schema);
    let (_, task) = delivered(&store, "supervised");
    let supervisor = PostgresStore::connect(
        &role_url(&schema_url(&schema), "spawn_supervisor").expect("the login's URL"),
    )
    .expect("the supervisor's login connects");
    supervisor
        .task_restart(task, Some("stalled"))
        .expect("restarted");
    assert_eq!(
        records(&mut client, "task.restarted")
            .pop()
            .expect("the restart")
            .0["by"],
        "spawn_supervisor"
    );
    let new = supervisor
        .task_reassign(task, Some("still stalled"))
        .expect("reassigned")["task"]
        .clone();
    let (cancelled, _) = records(&mut client, "task.cancelled")
        .pop()
        .expect("the cancel");
    assert_eq!(cancelled["by"], "spawn_supervisor");
    assert_eq!(cancelled["reassigned_as"], new);
}

/// The recovery's records take the two new causes, on the final path.
#[test]
fn an_interruption_may_be_restarted_or_reassigned() {
    let (store, _schema) = store("restart_causes");
    for cause in ["restarted", "reassigned"] {
        store
            .host_record(&json!({
                "type": "execution.interrupted", "agent_id": "agent_worker_a_1",
                "session_id": "session_worker_a_1", "execution_id": format!("exec_{cause}"),
                "cause": cause, "evidence": {"to_session": "session_worker_a_2"},
                "path": "final", "attempt": 1, "due_ms": null, "fact_event_id": 1,
            }))
            .unwrap_or_else(|error| panic!("{cause}: {error}"));
    }
    let error = refusal(store.host_record(&json!({
        "type": "execution.interrupted", "agent_id": "agent_worker_a_1",
        "session_id": "session_worker_a_1", "execution_id": "exec_x",
        "cause": "moved", "evidence": {}, "path": "final", "attempt": 1,
    })));
    assert!(error.contains("not an interruption cause"), "{error}");
}
