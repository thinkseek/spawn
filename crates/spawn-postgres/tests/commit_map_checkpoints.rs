//! The agents' commits through the commit map of a history rewrite
//! (objective 177): an agent's latest checkpoint, the one an overrun keeps,
//! the commit a report names, and a cancelled delivery's start and
//! set-aside, recorded before the rewrite, are read by their new commits
//! once the map is loaded; an id the map cannot translate (dropped) reads as
//! itself.
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
use common::store;

const SESSION: &str = "session_resident_core";
const AGENT: &str = "agent_resident_core";

fn commit(n: u32) -> String {
    format!("{n:040x}")
}

/// The commit `n` after the rewrite.
fn rewritten(n: u32) -> String {
    format!("{:040x}", 0xf000 + n)
}

/// A checkpoint of the resident's branch at commit `n`, for `reason`.
fn checkpoint(store: &PostgresStore, n: u32, reason: CheckpointReason, key: Option<String>) {
    let commit = CommitId::new(commit(n)).expect("valid commit");
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

fn rows(store: &PostgresStore) -> Vec<Value> {
    store
        .cancelled_deliveries()
        .expect("cancelled deliveries")
        .as_array()
        .cloned()
        .expect("a list")
}

/// Recorded before the rewrite: the resident's cancelled delivery (started
/// from commit 2, set aside at 5), its latest checkpoint (5), and an
/// ephemeral task's report naming 6. With the map loaded each reads as its
/// new commit; a checkpoint the map names dropped (7) reads as itself.
#[test]
fn an_agents_checkpoints_are_read_by_their_new_commits_after_a_rewrite() {
    let (store, _schema) = store("commit_map_checkpoints");
    let mission = store
        .mission_add("Work", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    store.resident_propose("core", "Owns it").expect("propose");
    store.resident_approve("core").expect("approve");
    checkpoint(&store, 1, CheckpointReason::TurnStopped, None);
    checkpoint(&store, 2, CheckpointReason::BaseMerged, None);
    let task = store
        .task_assign(mission, "resident", "core", "brief", "default")
        .expect("assign");
    let sent = store
        .submit(
            &Principal::new("spawn-dispatch").expect("valid principal"),
            &Submission {
                agent_id: AgentId::new(AGENT).expect("valid agent"),
                session_id: SessionId::new(SESSION).expect("valid session"),
                idempotency_key: IdempotencyKey::new(format!("deliver-{task}-0"))
                    .expect("valid key"),
                action: CommandAction::Input {
                    text: "the task".to_string(),
                    tags: Vec::new(),
                },
            },
        )
        .expect("submit");
    store
        .task_delivered(task, i64::try_from(sent.0).expect("a small id"))
        .expect("delivered");
    checkpoint(&store, 3, CheckpointReason::ToolCompleted, None);
    store.task_cancel(task).expect("cancel");
    checkpoint(
        &store,
        5,
        CheckpointReason::SetAside,
        Some(format!("set-aside:{task}")),
    );
    let other = store
        .mission_add("Other", &json!(["/tmp/other.git"]), "human")
        .expect("add");
    let reporter = store
        .task_assign(other, "ephemeral", "w", "brief", "default")
        .expect("assign");
    store.task_delivered(reporter, 1).expect("delivered");
    assert!(
        store
            .task_reported_with(reporter, "done", Some(&commit(6)))
            .expect("reported")
    );

    // Before the map: the recorded ids.
    assert_eq!(rows(&store)[0]["start"], commit(2));
    assert_eq!(rows(&store)[0]["set_aside"], commit(5));
    assert_eq!(
        store.latest_checkpoint(AGENT).expect("latest"),
        Some(commit(5))
    );
    assert_eq!(
        store.reported_commit(reporter).expect("reported"),
        Some(commit(6))
    );

    let mut pairs: Vec<(String, Option<String>)> = [1, 2, 3, 5, 6]
        .into_iter()
        .map(|n| (commit(n), Some(rewritten(n))))
        .collect();
    pairs.push((commit(7), None));
    store.commit_map_load(&pairs).expect("the map");

    let found = rows(&store);
    assert_eq!(found[0]["start"], rewritten(2), "{found:?}");
    assert_eq!(found[0]["set_aside"], rewritten(5), "{found:?}");
    assert_eq!(
        store.latest_checkpoint(AGENT).expect("latest"),
        Some(rewritten(5))
    );
    assert_eq!(
        store.checkpoint_at(AGENT).expect("at").expect("one")["commit"],
        rewritten(5).as_str()
    );
    assert_eq!(
        store.reported_commit(reporter).expect("reported"),
        Some(rewritten(6))
    );

    // A checkpoint the rewrite dropped reads as itself: the writers refuse it.
    checkpoint(&store, 7, CheckpointReason::TurnStopped, None);
    assert_eq!(
        store.latest_checkpoint(AGENT).expect("latest"),
        Some(commit(7))
    );
    assert_eq!(
        store.checkpoint_at(AGENT).expect("at").expect("one")["commit"],
        commit(7).as_str()
    );
}
