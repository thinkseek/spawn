//! Public API tests for spawn-postgres.
//!
//! A real database is required. `SPAWN_TEST_DATABASE_URL` must point at a
//! disposable server, for example
//! `postgres://postgres:spawn@127.0.0.1:15432/spawn`. The test panics when the
//! variable is missing: silently skipping would let a broken adapter pass.
//!
//! Every case runs in a schema of its own, so a shared database cannot leak
//! events or commands between cases or between runs.

use std::time::Duration;

use postgres::{Client, NoTls};
use spawn_core::command::{CommandAction, Outcome, Submission};
use spawn_core::contract;
use spawn_core::event::{Engine, EventKind, GlobalPosition, NewEvent, SessionSeq, Source};
use spawn_core::id::{AgentId, IdempotencyKey, Principal, SessionId};
use spawn_core::observation::{Confidence, Observation, SemanticState};
use spawn_core::port::{CommandQueue, EventLog};
use spawn_postgres::PostgresStore;

fn url() -> String {
    std::env::var("SPAWN_TEST_DATABASE_URL")
        .expect("SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip")
}

/// Store in a schema of its own.
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

fn agent() -> AgentId {
    AgentId::new("agent_one").expect("valid agent")
}

fn session() -> SessionId {
    SessionId::new("session_one").expect("valid session")
}

fn message(kind: EventKind) -> NewEvent {
    NewEvent {
        agent_id: agent(),
        session_id: session(),
        execution_id: None,
        source: Source::Observer,
        source_event_id: None,
        kind,
    }
}

fn start(submission_session: SessionId, key: &str) -> Submission {
    Submission {
        agent_id: agent(),
        session_id: submission_session,
        idempotency_key: IdempotencyKey::new(key).expect("valid key"),
        action: CommandAction::Start {
            engine: Engine::Claude,
            prompt: "hello".to_string(),
            parent_session_id: None,
        },
    }
}

#[test]
fn postgres_passes_the_shared_contract() {
    let (store, _schema) = store("contract");
    contract::event_log("pg_evt", &store);
    contract::command_queue("pg_cmd", &store, &store);
}

#[test]
fn append_is_idempotent_per_source_key() {
    let (store, _schema) = store("append");
    let mut keyed = message(EventKind::MessageReceived);
    keyed.source_event_id = Some("hook:prompt_1".to_string());
    let kept = store.append(keyed.clone()).expect("append succeeds");
    assert!(kept.inserted);
    let repeated = store.append(keyed).expect("repeat append succeeds");
    assert!(!repeated.inserted, "the same source key is stored once");
    assert_eq!(
        repeated.record.position, kept.record.position,
        "a repeated key keeps the stored position"
    );
}

#[test]
fn session_sequences_are_dense_and_positions_increase() {
    let (store, _schema) = store("seq");
    for expected in 1..=3 {
        let appended = store
            .append(message(EventKind::TurnStopped {
                input_tokens: Some(10),
                output_tokens: Some(20),
            }))
            .expect("append succeeds");
        assert_eq!(appended.record.session_seq, SessionSeq(expected));
    }
    let read = store
        .read_session(&session(), SessionSeq(0), 10)
        .expect("read succeeds");
    assert_eq!(read.len(), 3);
    assert!(
        read.windows(2)
            .all(|pair| pair[0].position < pair[1].position)
    );
}

#[test]
fn submit_is_idempotent_and_accepts_once() {
    let (store, _schema) = store("submit");
    let principal = Principal::new("tester").expect("valid principal");
    let submission = start(session(), "key_one");
    let first = store
        .submit(&principal, &submission)
        .expect("submit succeeds");
    let second = store
        .submit(&principal, &submission)
        .expect("submit succeeds");
    assert_eq!(first, second, "the same key maps to the same command");
    let read = store
        .read_session(&session(), SessionSeq(0), 10)
        .expect("read succeeds");
    assert_eq!(
        read.iter()
            .filter(|record| record.event.kind.name() == "command.accepted")
            .count(),
        1,
        "command.accepted is written once"
    );
}

#[test]
fn one_session_runs_one_command_at_a_time() {
    let (store, _schema) = store("serial");
    let principal = Principal::new("tester").expect("valid principal");
    let older = store
        .submit(&principal, &start(session(), "key_a"))
        .expect("submit succeeds");
    let younger = store
        .submit(&principal, &start(session(), "key_b"))
        .expect("submit succeeds");
    let claimed = store
        .claim("w1", Duration::from_mins(1))
        .expect("claim succeeds")
        .expect("one command is ready");
    assert_eq!(claimed.command_id, older, "the older command goes first");
    assert!(
        store
            .claim("w1", Duration::from_mins(1))
            .expect("claim succeeds")
            .is_none(),
        "the younger command waits"
    );
    store
        .finish(&claimed, &Outcome::Succeeded)
        .expect("finish succeeds");
    let next = store
        .claim("w1", Duration::from_mins(1))
        .expect("claim succeeds")
        .expect("the younger command is ready");
    assert_eq!(next.command_id, younger);
}

#[test]
fn an_expired_lease_is_taken_again_with_a_new_attempt() {
    let (store, schema) = store("lease");
    let principal = Principal::new("tester").expect("valid principal");
    let id = store
        .submit(&principal, &start(session(), "key_lease"))
        .expect("submit succeeds");
    let first = store
        .claim("w1", Duration::from_mins(1))
        .expect("claim succeeds")
        .expect("a command is ready");
    assert_eq!(first.attempt, 1);
    assert!(
        store
            .claim("w2", Duration::from_mins(1))
            .expect("claim succeeds")
            .is_none(),
        "a live lease is not stolen"
    );

    admin(&schema)
        .execute(
            "update spawn_commands set lease_until_ms = spawn_now_ms() - 1 where id = $1",
            &[&id.0.cast_signed()],
        )
        .expect("expire the lease");

    let second = store
        .claim("w2", Duration::from_mins(1))
        .expect("claim succeeds")
        .expect("the expired lease is taken again");
    assert_eq!(second.attempt, 2, "an expired lease increments the attempt");
    assert!(
        store.finish(&first, &Outcome::Succeeded).is_err(),
        "the old attempt cannot finish the command"
    );
    store
        .finish(&second, &Outcome::Succeeded)
        .expect("the current attempt finishes");
}

#[test]
fn views_answer_from_the_event_log() {
    let (store, schema) = store("views");
    let observation = Observation {
        state: SemanticState::Working,
        reason: None,
        based_on_seq: SessionSeq(1),
        confidence: Confidence::new(0.75).expect("valid confidence"),
        judge: "heuristic".to_string(),
    };
    store
        .append(message(EventKind::ObservationRecorded { observation }))
        .expect("append succeeds");
    store
        .append(message(EventKind::TurnStopped {
            input_tokens: Some(10),
            output_tokens: Some(20),
        }))
        .expect("append succeeds");

    let row = admin(&schema)
        .query_one(
            "select state, judge, session_seq, based_on_seq from agent_state",
            &[],
        )
        .expect("agent_state answers");
    let state: Option<String> = row.get(0);
    assert_eq!(state.as_deref(), Some("WORKING"));
    let judge: Option<String> = row.get(1);
    assert_eq!(judge.as_deref(), Some("heuristic"));
    let seq: i64 = row.get(2);
    assert_eq!(seq, 1, "agent_state keeps the observation row");
    let based_on: Option<i64> = row.get(3);
    assert_eq!(based_on, Some(1));

    let timeline = admin(&schema)
        .query("select event_name from agent_timeline order by id", &[])
        .expect("agent_timeline answers");
    let names: Vec<String> = timeline.iter().map(|row| row.get(0)).collect();
    assert_eq!(names, vec!["observation.recorded", "turn.stopped"]);
}

#[test]
fn the_gateway_view_rejects_a_principal_without_a_grant() {
    let (store, schema) = store("grant");
    // A plain insert through `agent_commands` runs the trigger as the
    // database session user, which has no grant.
    let error = admin(&schema)
        .execute(
            "insert into agent_commands (agent_id, session_id, idempotency_key, action) \
             values ('agent_one', 'session_one', 'key_1', '{\"type\":\"start\"}'::jsonb)",
            &[],
        )
        .expect_err("an ungranted insert is refused");
    let code = error
        .code()
        .expect("a database error carries a code")
        .code()
        .to_string();
    assert_eq!(code, "42501", "the trigger raises insufficient privilege");
    assert!(
        store
            .submit(
                &Principal::new("tester").expect("valid principal"),
                &start(session(), "key_ok"),
            )
            .is_ok(),
        "the sanctioned path still works"
    );
}

#[test]
fn a_global_read_returns_session_records_in_order() {
    let (store, _schema) = store("global");
    for kind in [
        EventKind::MessageReceived,
        EventKind::TurnStopped {
            input_tokens: Some(10),
            output_tokens: Some(20),
        },
    ] {
        store.append(message(kind)).expect("append succeeds");
    }
    let records = store
        .read_global(GlobalPosition(0), 10)
        .expect("read succeeds");
    assert_eq!(records.len(), 2);
    assert!(
        records
            .windows(2)
            .all(|pair| pair[0].position < pair[1].position)
    );
}

#[test]
fn migrating_twice_changes_nothing() {
    let (store, _schema) = store("migrate");
    store.migrate().expect("a second migration succeeds");
}
