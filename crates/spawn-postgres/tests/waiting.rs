//! An input that waits for its session's turn (objective 015): the Bridge
//! records `command.step` `waiting` for it and leaves it unfinished.
//! `spawn_claim` then hands out what comes behind it (a `stop`, and a later
//! input so it can be recorded waiting too), while an unfinished command
//! with no `waiting` step still holds everything behind it. `spawn_release`
//! makes a held command claimable again after the given time, and only for
//! the lease that holds it.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use std::time::Duration;

use spawn_core::command::{CommandAction, Outcome, Submission};
use spawn_core::event::{CommandStep, EventKind, NewEvent, Source};
use spawn_core::id::{AgentId, CommandId, IdempotencyKey, Principal, SessionId};
use spawn_core::port::{CommandQueue, EventLog};
use spawn_postgres::PostgresStore;

mod common;
use common::{admin, store};

const SESSION: &str = "session_waiting";
const AGENT: &str = "agent_waiting";

fn submit(store: &PostgresStore, principal: &str, key: &str, action: CommandAction) -> CommandId {
    store
        .submit(
            &Principal::new(principal).expect("valid principal"),
            &Submission {
                agent_id: AgentId::new(AGENT).expect("valid agent"),
                session_id: SessionId::new(SESSION).expect("valid session"),
                idempotency_key: IdempotencyKey::new(key).expect("valid key"),
                action,
            },
        )
        .expect("submit")
}

fn input(text: &str) -> CommandAction {
    CommandAction::Input {
        text: text.to_string(),
        tags: Vec::new(),
    }
}

/// The Bridge's `waiting` step for `command`, under its key.
fn waiting(store: &PostgresStore, command: CommandId, source: Source) {
    store
        .append(NewEvent {
            agent_id: AgentId::new(AGENT).expect("valid agent"),
            session_id: SessionId::new(SESSION).expect("valid session"),
            execution_id: None,
            source,
            source_event_id: Some(format!("cmd_{}:waiting", command.0)),
            kind: EventKind::CommandStepped {
                command_id: command,
                step: CommandStep::Waiting,
                workspace: None,
            },
        })
        .expect("append");
}

#[test]
fn a_stop_behind_a_waiting_input_is_handed_out_and_nothing_behind_an_input_not_waiting() {
    let (store, _schema) = store("waiting_claim");
    let ttl = Duration::from_mins(1);
    let first = submit(&store, "human", "note-1", input("first"));
    let second = submit(&store, "spawn_supervisor", "note-2", input("second"));
    let stop = submit(&store, "human", "stop-1", CommandAction::Stop);

    let claimed = store.claim("w1", ttl).expect("claim").expect("the first");
    assert_eq!(claimed.command_id, first);
    assert_eq!(claimed.principal, "human");
    // Its lease is let go: it is claimable again once the wait is over.
    store.release(&claimed, Duration::ZERO).expect("release");
    let again = store.claim("w1", ttl).expect("claim").expect("the first again");
    assert_eq!(again.command_id, first, "nothing behind an input not waiting");
    store.release(&again, Duration::from_mins(5)).expect("release");
    assert!(
        store.claim("w1", ttl).expect("claim").is_none(),
        "unfinished and not waiting: everything behind it waits, a stop too"
    );

    // Recorded waiting by the Bridge: what comes behind it is handed out,
    // the input behind it too, so it can be recorded waiting in its turn.
    waiting(&store, first, Source::Bridge);
    let next = store.claim("w1", ttl).expect("claim").expect("the second");
    assert_eq!(next.command_id, second);
    assert_eq!(next.principal, "spawn_supervisor");
    assert!(
        store.claim("w1", ttl).expect("claim").is_none(),
        "the second holds its lease and is not waiting: the stop waits"
    );
    waiting(&store, second, Source::Bridge);
    let handed = store.claim("w1", ttl).expect("claim").expect("the stop");
    assert_eq!(handed.command_id, stop);
    store.finish(&handed, &Outcome::Succeeded).expect("finish");
}

#[test]
fn only_the_bridges_waiting_step_lets_commands_pass() {
    let (store, _schema) = store("waiting_source");
    let ttl = Duration::from_mins(1);
    let first = submit(&store, "human", "note-1", input("first"));
    submit(&store, "human", "stop-1", CommandAction::Stop);
    let claimed = store.claim("w1", ttl).expect("claim").expect("the first");
    store.release(&claimed, Duration::from_mins(5)).expect("release");
    waiting(&store, first, Source::Gateway);
    assert!(
        store.claim("w1", ttl).expect("claim").is_none(),
        "a waiting step is the Bridge's"
    );
}

#[test]
fn a_release_moves_only_the_lease_that_holds_the_command() {
    let (store, schema) = store("waiting_release");
    let first = submit(&store, "human", "note-1", input("first"));
    let stale = store
        .claim("w1", Duration::ZERO)
        .expect("claim")
        .expect("the first");
    // The lease ran out and another claim took it over.
    let current = store
        .claim("w2", Duration::from_mins(1))
        .expect("claim")
        .expect("taken over");
    assert_eq!(current.command_id, first);
    assert!(current.attempt > stale.attempt);
    let lease = |schema: &str| -> i64 {
        admin(schema)
            .query_one(
                "select lease_until_ms - spawn_now_ms() from spawn_commands where id = $1",
                &[&i64::try_from(first.0).expect("small")],
            )
            .expect("lease")
            .get(0)
    };
    let before = lease(&schema);
    store
        .release(&stale, Duration::ZERO)
        .expect("a stale release is no error");
    let after = lease(&schema);
    assert!(
        after > 30_000 && after <= before,
        "the stale attempt left the lease alone: {before} then {after}"
    );
    store
        .release(&current, Duration::ZERO)
        .expect("release");
    assert!(lease(&schema) <= 0, "released at once");

    // A finished command is not revived by a release.
    let claimed = store
        .claim("w1", Duration::from_mins(1))
        .expect("claim")
        .expect("again");
    store.finish(&claimed, &Outcome::Succeeded).expect("finish");
    store.release(&claimed, Duration::ZERO).expect("release");
    assert!(store.claim("w1", Duration::from_mins(1)).expect("claim").is_none());
}
