#![allow(clippy::doc_markdown)]
//! PostgreSQL integration tests.
//!
//! These tests require a reachable database. Provide the connection string in
//! `SPAWN_TEST_DATABASE_URL`. The gate script starts an ephemeral container and
//! exports it. A missing variable fails the suite on purpose: storage behavior
//! must never be silently skipped.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use spawn_store::{CommandQueue, EventLog};
use spawn_store_postgres::PostgresStore;
use spawn_types::clock::{DeterministicClock, Timestamp};
use spawn_types::command::{
    CommandAction, CommandId, CommandRecord, CommandStatus, IdempotencyKey, Principal,
};
use spawn_types::engine::Engine;
use spawn_types::event::{EventKind, EventSource, NewEvent, SessionSeq};
use spawn_types::identifiers::{AgentId, SessionId};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

// claim() serves one process-wide work queue with FOR UPDATE SKIP LOCKED, so
// concurrent tests would otherwise grab each other's commands. Serialize the
// integration binary through one process-local guard; unique suffixes still
// keep rows distinct for assertions and repeated runs.
static QUEUE_GUARD: Mutex<()> = Mutex::new(());

fn queue_guard() -> MutexGuard<'static, ()> {
    match QUEUE_GUARD.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn database_url() -> String {
    std::env::var("SPAWN_TEST_DATABASE_URL").unwrap_or_else(|_| {
        panic!(
            "SPAWN_TEST_DATABASE_URL is required for spawn-store-postgres integration tests. \
             Start PostgreSQL and export the connection string; the gate script does this \
             automatically via docker."
        )
    })
}

fn unique_suffix() -> u64 {
    let nanos = u64::try_from(
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos(),
    )
    .unwrap_or(u64::MAX);
    nanos + SEQUENCE.fetch_add(1, Ordering::SeqCst)
}

fn connect_store() -> (PostgresStore, AgentId, SessionId) {
    let clock = Arc::new(DeterministicClock::starting_at(1_000));
    let store = PostgresStore::connect(&database_url(), clock).expect("connect and migrate");
    let suffix = unique_suffix();
    let agent = AgentId::from_prefixed(format!("agent_pg{suffix}")).unwrap();
    let session = SessionId::from_prefixed(format!("session_pg{suffix}")).unwrap();
    (store, agent, session)
}

fn turn_event(agent: &AgentId, session: &SessionId, key: &str) -> NewEvent {
    NewEvent {
        agent_id: agent.clone(),
        session_id: session.clone(),
        execution_id: None,
        source: EventSource::ClaudeHooks,
        source_event_id: Some(key.to_string()),
        kind: EventKind::TurnStopped {
            input_tokens: None,
            output_tokens: None,
        },
        artifact: None,
        occurred_at: None,
    }
}

#[test]
fn assigns_dual_ordering_and_deduplicates() {
    let _guard = queue_guard();
    let (store, agent, session) = connect_store();
    let first = store
        .append(turn_event(&agent, &session, "native-1"))
        .unwrap();
    let duplicate = store
        .append(turn_event(&agent, &session, "native-1"))
        .unwrap();
    let second = store
        .append(turn_event(&agent, &session, "native-2"))
        .unwrap();

    assert!(matches!(
        first,
        spawn_types::event::AppendOutcome::Recorded(_)
    ));
    assert!(matches!(
        duplicate,
        spawn_types::event::AppendOutcome::Deduplicated(_)
    ));
    assert_eq!(first.record().event_id, duplicate.record().event_id);
    assert_eq!(first.record().session_seq, SessionSeq(1));
    assert_eq!(second.record().session_seq, SessionSeq(2));
    assert!(second.record().global_position.0 > first.record().global_position.0);

    let read = store.read_session(&session, SessionSeq(0), 100).unwrap();
    assert_eq!(read.len(), 2);
}

#[test]
fn lease_fencing_rejects_stale_attempts() {
    let _guard = queue_guard();
    let (store, agent, session) = connect_store();
    let suffix = unique_suffix();
    let command = CommandRecord {
        command_id: CommandId::new(format!("cmd_pg{suffix}")).unwrap(),
        agent_id: agent.clone(),
        session_id: session.clone(),
        idempotency_key: IdempotencyKey::new(format!("key_pg{suffix}")).unwrap(),
        principal: Principal {
            name: "worker-test".to_string(),
        },
        action: CommandAction::Stop,
        status: CommandStatus::Pending,
        submitted_at: Timestamp::from_unix_millis(1_000),
    };
    let stored = store.submit(command).unwrap();
    assert_eq!(stored.command_id.as_str(), format!("cmd_pg{suffix}"));

    let leased = store
        .claim(
            "worker-a",
            Timestamp::from_unix_millis(1_000),
            Duration::from_secs(10),
        )
        .unwrap()
        .expect("command is claimable");
    let attempt = match leased.status {
        CommandStatus::Leased { lease } => lease.attempt,
        other => panic!("expected lease, got {other:?}"),
    };

    let stale = spawn_types::command::LeaseAttempt(attempt.0 + 5);
    assert!(
        store
            .finish(
                leased.command_id.as_str(),
                stale,
                true,
                Timestamp::from_unix_millis(1_001)
            )
            .is_err()
    );

    let renewed = store
        .renew(
            leased.command_id.as_str(),
            attempt,
            Timestamp::from_unix_millis(1_002),
            Duration::from_secs(20),
        )
        .unwrap();
    assert!(renewed.expires_at.unix_millis() >= 1_002);

    let finished = store
        .finish(
            leased.command_id.as_str(),
            attempt,
            true,
            Timestamp::from_unix_millis(1_003),
        )
        .unwrap();
    assert!(matches!(
        finished.status,
        CommandStatus::Finished {
            succeeded: true,
            ..
        }
    ));
}

#[test]
fn expired_lease_is_reclaimed_with_monotonic_attempt() {
    let _guard = queue_guard();
    let (store, agent, session) = connect_store();
    let suffix = unique_suffix();
    store
        .submit(CommandRecord {
            command_id: CommandId::new(format!("cmd_exp{suffix}")).unwrap(),
            agent_id: agent,
            session_id: session,
            idempotency_key: IdempotencyKey::new(format!("key_exp{suffix}")).unwrap(),
            principal: Principal {
                name: "worker-test".to_string(),
            },
            action: CommandAction::Spawn {
                engine: Engine::ClaudeCode,
                prompt: "continue".to_string(),
            },
            status: CommandStatus::Pending,
            submitted_at: Timestamp::from_unix_millis(0),
        })
        .unwrap();

    let ttl = Duration::from_secs(10);
    let first = store
        .claim("w1", Timestamp::from_unix_millis(0), ttl)
        .unwrap()
        .unwrap();
    let first_attempt = match first.status {
        CommandStatus::Leased { lease } => lease.attempt,
        _ => panic!("expected lease"),
    };
    let reclaimed = store
        .claim("w2", Timestamp::from_unix_millis(20_000), ttl)
        .unwrap()
        .unwrap();
    let second_attempt = match reclaimed.status {
        CommandStatus::Leased { lease } => lease.attempt,
        _ => panic!("expected reclaimed lease"),
    };
    assert_eq!(second_attempt.0, first_attempt.0 + 1);
}
