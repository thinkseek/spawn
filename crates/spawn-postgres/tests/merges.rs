//! What main's merges into a worker's branch changed, told with the next
//! delivery that reaches the worker (causality audit #7): derived from the
//! recorded merges and the recorded typing, not from one attempt's merge.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use spawn_core::command::{CommandAction, Submission};
use spawn_core::event::{ArtifactRef, CheckpointReason, CommandStep, EventKind, NewEvent, Source};
use spawn_core::id::{
    AgentId, BranchName, CommandId, CommitId, IdempotencyKey, Principal, SessionId,
};
use spawn_core::port::{CommandQueue, EventLog};
use spawn_postgres::PostgresStore;

mod common;
use common::store;

const SESSION: &str = "session_resident_core";
const AGENT: &str = "agent_resident_core";

fn append(store: &PostgresStore, key: String, kind: EventKind) {
    store
        .append(NewEvent {
            agent_id: AgentId::new(AGENT).expect("valid agent"),
            session_id: SessionId::new(SESSION).expect("valid session"),
            execution_id: None,
            source: Source::Bridge,
            source_event_id: Some(key),
            kind,
        })
        .expect("append");
}

fn merged(store: &PostgresStore, commit: u32, files: &[&str]) {
    let commit = CommitId::new(format!("{commit:040x}")).expect("valid commit");
    append(
        store,
        format!("commit:{commit}"),
        EventKind::ArtifactCommitted {
            reason: CheckpointReason::BaseMerged,
            artifact: ArtifactRef {
                project: "missions".to_string(),
                branch: BranchName::new(AGENT).expect("valid branch"),
                commit,
            },
            files: files.iter().map(ToString::to_string).collect(),
        },
    );
}

fn delivery(store: &PostgresStore, key: &str) -> CommandId {
    store
        .submit(
            &Principal::new("spawn-dispatch").expect("valid principal"),
            &Submission {
                agent_id: AgentId::new(AGENT).expect("valid agent"),
                session_id: SessionId::new(SESSION).expect("valid session"),
                idempotency_key: IdempotencyKey::new(key).expect("valid key"),
                action: CommandAction::Input {
                    text: "the task".to_string(),
                    tags: Vec::new(),
                },
            },
        )
        .expect("submit")
}

fn typed(store: &PostgresStore, command: CommandId) {
    append(
        store,
        format!("{}:typed", command.0),
        EventKind::CommandStepped {
            command_id: command,
            step: CommandStep::Typed,
            workspace: None,
        },
    );
}

#[test]
fn a_merge_is_untold_until_a_delivery_is_typed_after_it() {
    let (store, _schema) = store("untold_merges");
    assert!(store.untold_merges(SESSION).expect("asked").is_empty());

    // Main merged before the delivery; the delivery was refused (pane not
    // ready): nothing typed, so its retry, whose merge is `Current`, must
    // still tell what the first merge changed.
    merged(&store, 1, &["src/lib.rs", "README.md"]);
    let _refused = delivery(&store, "deliver-7-0");
    assert_eq!(
        store.untold_merges(SESSION).expect("asked"),
        ["README.md", "src/lib.rs"]
    );

    // The retry is typed: told.
    let retry = delivery(&store, "deliver-7-1");
    typed(&store, retry);
    assert!(store.untold_merges(SESSION).expect("asked").is_empty());

    // A later merge is untold again; a human's typed input tells nothing.
    merged(&store, 2, &["src/main.rs"]);
    let human = store
        .submit(
            &Principal::new("human").expect("valid principal"),
            &Submission {
                agent_id: AgentId::new(AGENT).expect("valid agent"),
                session_id: SessionId::new(SESSION).expect("valid session"),
                idempotency_key: IdempotencyKey::new("human-1").expect("valid key"),
                action: CommandAction::Input {
                    text: "hello".to_string(),
                    tags: Vec::new(),
                },
            },
        )
        .expect("submit");
    typed(&store, human);
    assert_eq!(
        store.untold_merges(SESSION).expect("asked"),
        ["src/main.rs"]
    );
}
