//! Residents whose latest delivered task is cancelled (objective 012): the
//! dispatcher stops them, sets the task's work aside to the checkpoint it
//! started from, and tells them. `cancelled_deliveries` finds them, and that
//! checkpoint.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use serde_json::{Value, json};
use spawn_core::command::{CommandAction, Submission};
use spawn_core::event::{ArtifactRef, CheckpointReason, EventKind, NewEvent, Source};
use spawn_core::id::{AgentId, BranchName, CommitId, IdempotencyKey, Principal, SessionId};
use spawn_core::port::{CommandQueue, EventLog};
use spawn_postgres::PostgresStore;

mod common;
use common::{admin, store};

const SESSION: &str = "session_resident_core";
const AGENT: &str = "agent_resident_core";

/// A checkpoint of the resident's branch at commit `n`, for `reason`.
fn checkpoint(store: &PostgresStore, n: u32, reason: CheckpointReason, key: Option<String>) {
    let commit = CommitId::new(format!("{n:040x}")).expect("valid commit");
    store
        .append(NewEvent {
            agent_id: AgentId::new(AGENT).expect("valid agent"),
            session_id: SessionId::new(SESSION).expect("valid session"),
            execution_id: None,
            source: Source::Bridge,
            source_event_id: Some(key.unwrap_or_else(|| format!("commit:{commit}"))),
            kind: EventKind::ArtifactCommitted {
                reason,
                artifact: ArtifactRef {
                    project: "missions".to_string(),
                    branch: BranchName::new(AGENT).expect("valid branch"),
                    commit,
                },
                files: Vec::new(),
            },
        })
        .expect("append");
}

fn commit(n: u32) -> String {
    format!("{n:040x}")
}

/// A dispatcher command into the resident's session, under `key`.
fn command(store: &PostgresStore, key: &str, action: CommandAction) -> i64 {
    let id = store
        .submit(
            &Principal::new("spawn-dispatch").expect("valid principal"),
            &Submission {
                agent_id: AgentId::new(AGENT).expect("valid agent"),
                session_id: SessionId::new(SESSION).expect("valid session"),
                idempotency_key: IdempotencyKey::new(key).expect("valid key"),
                action,
            },
        )
        .expect("submit");
    i64::try_from(id.0).expect("a small id")
}

fn input() -> CommandAction {
    CommandAction::Input {
        text: "the task".to_string(),
        tags: Vec::new(),
    }
}

/// The resident's task, delivered by a command typed in.
fn delivered(store: &PostgresStore, mission: i64, brief: &str) -> i64 {
    let task = store
        .task_assign(mission, "resident", "core", brief, "default")
        .expect("assign");
    let sent = command(store, &format!("deliver-{task}-0"), input());
    store.task_delivered(task, sent).expect("delivered");
    task
}

fn rows(store: &PostgresStore) -> Vec<Value> {
    store
        .cancelled_deliveries()
        .expect("cancelled deliveries")
        .as_array()
        .cloned()
        .expect("a list")
}

/// The start is the latest checkpoint before the delivery was accepted,
/// the main merge typed just before it included, not the task's own work; a
/// merge of main recorded between the acceptance and `task.delivered` (a
/// start's) counts too. The row says whether a set-aside is recorded and
/// whether the notice reached the worker.
#[test]
fn a_residents_cancelled_latest_delivery_comes_with_the_checkpoint_it_started_from() {
    let (store, schema) = store("cancelled_start");
    let mission = store
        .mission_add("Work", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    store.resident_propose("core", "Owns it").expect("propose");
    store.resident_approve("core").expect("approve");
    checkpoint(&store, 1, CheckpointReason::TurnStopped, None);
    // The merge of main done right before the task is typed in.
    checkpoint(&store, 2, CheckpointReason::BaseMerged, None);
    let task = store
        .task_assign(mission, "resident", "core", "brief", "default")
        .expect("assign");
    let sent = command(&store, &format!("deliver-{task}-0"), input());
    // The task's own work, checkpointed before `task.delivered` was recorded.
    checkpoint(&store, 3, CheckpointReason::ToolCompleted, None);
    store.task_delivered(task, sent).expect("delivered");
    checkpoint(&store, 4, CheckpointReason::ToolCompleted, None);
    assert!(rows(&store).is_empty(), "nothing is cancelled yet");

    store.task_cancel(task).expect("cancel");
    let found = rows(&store);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0]["id"], task);
    assert_eq!(found[0]["mission_id"], mission);
    assert_eq!(found[0]["agent_id"], AGENT);
    assert_eq!(found[0]["session_id"], SESSION);
    assert_eq!(found[0]["start"], commit(2), "{found:?}");
    assert_eq!(found[0]["set_aside"], Value::Null);
    assert_eq!(found[0]["told"], false);
    assert_eq!(found[0]["closed_by"], "task.cancelled");

    // The set-aside, once recorded, is read back and is never a start.
    checkpoint(
        &store,
        5,
        CheckpointReason::SetAside,
        Some(format!("set-aside:{task}")),
    );
    let found = rows(&store);
    assert_eq!(found[0]["set_aside"], commit(5));
    assert_eq!(found[0]["start"], commit(2));

    // The notice reached the worker.
    let notice = command(
        &store,
        &format!("cancel-notice-{task}-0"),
        CommandAction::Resume {
            prompt: Some("Task cancelled".to_string()),
            model: None,
        },
    );
    assert_eq!(rows(&store)[0]["told"], false, "submitted is not told");
    admin(&schema)
        .execute(
            "update spawn_commands set state = 'finished', succeeded = true where id = $1",
            &[&notice],
        )
        .expect("finish the notice");
    assert_eq!(rows(&store)[0]["told"], true);
}

/// Only the latest delivery counts: a cancelled task delivered before
/// another is not found, nor one never delivered, nor an ephemeral worker's,
/// nor a retired resident's.
#[test]
fn only_the_latest_delivery_of_a_resident_still_serving_is_found() {
    let (store, _schema) = store("cancelled_latest");
    let mission = store
        .mission_add("Work", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    store.resident_propose("core", "Owns it").expect("propose");
    store.resident_approve("core").expect("approve");
    let first = delivered(&store, mission, "first");
    store.task_cancel(first).expect("cancel");
    let second = delivered(&store, mission, "second");
    assert!(rows(&store).is_empty(), "a later task was delivered since");

    let never = store
        .task_assign(mission, "resident", "core", "never delivered", "default")
        .expect("assign");
    store.task_cancel(never).expect("cancel");
    let ephemeral = store
        .task_assign(mission, "ephemeral", "probe", "ephemeral", "default")
        .expect("assign");
    store.task_cancel(ephemeral).expect("cancel");
    assert!(rows(&store).is_empty());

    store.task_cancel(second).expect("cancel");
    let found = rows(&store);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["id"], second);

    store.resident_retire("core").expect("retire");
    assert!(
        rows(&store).is_empty(),
        "a retired resident is only stopped"
    );
}
