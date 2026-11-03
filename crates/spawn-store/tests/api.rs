use std::sync::Arc;
use std::time::Duration;

use spawn_store::{CommandQueue, EventLog, InMemoryStore};
use spawn_types::clock::{Clock, DeterministicClock, Timestamp};
use spawn_types::command::{
    CommandAction, CommandId, CommandRecord, CommandStatus, IdempotencyKey, Principal,
};
use spawn_types::engine::Engine;
use spawn_types::event::{EventKind, EventSource, GlobalPosition, NewEvent, SessionSeq};
use spawn_types::identifiers::{AgentId, BranchName, SessionId};

fn ids() -> (AgentId, SessionId) {
    (
        AgentId::from_prefixed("agent_aaaa").unwrap(),
        SessionId::from_prefixed("session_bbbb").unwrap(),
    )
}

fn event(agent: &AgentId, session: &SessionId, source_key: &str, clock: &dyn Clock) -> NewEvent {
    NewEvent {
        agent_id: agent.clone(),
        session_id: session.clone(),
        execution_id: None,
        source: EventSource::ClaudeHooks,
        source_event_id: Some(source_key.to_string()),
        kind: EventKind::TurnStopped {
            input_tokens: None,
            output_tokens: None,
        },
        artifact: None,
        occurred_at: Some(clock.now()),
    }
}

#[test]
fn assigns_dual_ordering_and_deduplicates() {
    let clock = Arc::new(DeterministicClock::starting_at(100));
    let store = InMemoryStore::new().with_clock(clock.clone());
    let (agent, session) = ids();

    let first = store.append(event(&agent, &session, "k1", &clock)).unwrap();
    let second = store.append(event(&agent, &session, "k1", &clock)).unwrap();
    let third = store.append(event(&agent, &session, "k2", &clock)).unwrap();

    assert!(matches!(
        first,
        spawn_types::event::AppendOutcome::Recorded(_)
    ));
    assert!(matches!(
        second,
        spawn_types::event::AppendOutcome::Deduplicated(_)
    ));
    assert_eq!(first.record().event_id, second.record().event_id);
    assert_eq!(first.record().session_seq, SessionSeq(1));
    assert_eq!(third.record().session_seq, SessionSeq(2));
    assert_eq!(third.record().global_position, GlobalPosition(2));

    let read = store.read_session(&session, SessionSeq(0), 100).unwrap();
    assert_eq!(read.len(), 2);
    let global = store.read_global(GlobalPosition(0), 100).unwrap();
    assert_eq!(global.len(), 2);
}

fn command(
    clock: &dyn Clock,
    id: &str,
    key: &str,
    session: &SessionId,
    agent: &AgentId,
) -> CommandRecord {
    CommandRecord {
        command_id: CommandId::new(id).unwrap(),
        agent_id: agent.clone(),
        session_id: session.clone(),
        idempotency_key: IdempotencyKey::new(key).unwrap(),
        principal: Principal {
            name: "tester".to_string(),
        },
        action: CommandAction::Stop,
        status: CommandStatus::Pending,
        submitted_at: clock.now(),
    }
}

#[test]
fn idempotent_submit_returns_existing_command() {
    let clock = DeterministicClock::starting_at(0);
    let store = InMemoryStore::new();
    let (agent, session) = ids();
    let original = command(&clock, "cmd_1", "key-1", &session, &agent);
    let duplicate = command(&clock, "cmd_2", "key-1", &session, &agent);

    let stored_first = store.submit(original).unwrap();
    let stored_second = store.submit(duplicate).unwrap();
    assert_eq!(stored_first.command_id, stored_second.command_id);
}

#[test]
fn lease_serializes_one_session_and_uses_fencing() {
    let clock = DeterministicClock::starting_at(1_000);
    let store = InMemoryStore::new();
    let (agent, session_a) = ids();
    let session_b = SessionId::from_prefixed("session_cccc").unwrap();

    let now = Timestamp::from_unix_millis(1_000);
    let ttl = Duration::from_secs(10);

    // Claim deterministically regardless of HashMap iteration order: lease the
    // first session-A command before any competing command is visible.
    store
        .submit(command(&clock, "cmd_a", "ka", &session_a, &agent))
        .unwrap();
    let leased_a = store.claim("worker-1", now, ttl).unwrap().unwrap();
    assert_eq!(leased_a.session_id, session_a);

    store
        .submit(command(&clock, "cmd_b", "kb", &session_b, &agent))
        .unwrap();
    store
        .submit(command(&clock, "cmd_a2", "ka2", &session_a, &agent))
        .unwrap();
    let leased_b = store.claim("worker-2", now, ttl).unwrap().unwrap();
    assert_eq!(leased_b.session_id, session_b);
    // Session A already has an active lease, so its second command waits.
    assert!(store.claim("worker-3", now, ttl).unwrap().is_none());

    let attempt = match leased_a.status {
        CommandStatus::Leased { ref lease } => lease.attempt,
        _ => panic!("expected lease"),
    };
    // A stale attempt is fenced off.
    let stale = store.finish(
        leased_a.command_id.as_str(),
        crate_stale_attempt(attempt),
        true,
        now,
    );
    assert!(stale.is_err());
    // The current attempt completes.
    let finished = store
        .finish(leased_a.command_id.as_str(), attempt, true, now)
        .unwrap();
    assert!(matches!(
        finished.status,
        CommandStatus::Finished {
            succeeded: true,
            ..
        }
    ));
    // The next command for session A can now be claimed.
    assert!(store.claim("worker-4", now, ttl).unwrap().is_some());
}

fn crate_stale_attempt(
    current: spawn_types::command::LeaseAttempt,
) -> spawn_types::command::LeaseAttempt {
    spawn_types::command::LeaseAttempt(current.0 + 99)
}

#[test]
fn expired_lease_is_reclaimed_with_higher_attempt() {
    let clock = DeterministicClock::starting_at(0);
    let store = InMemoryStore::new();
    let (agent, session) = ids();
    store
        .submit(command(&clock, "cmd_x", "kx", &session, &agent))
        .unwrap();

    let ttl = Duration::from_secs(10);
    let first = store
        .claim("worker-1", Timestamp::from_unix_millis(0), ttl)
        .unwrap()
        .unwrap();
    let first_attempt = match first.status {
        CommandStatus::Leased { lease } => lease.attempt,
        _ => panic!("expected lease"),
    };
    let reclaimed = store
        .claim("worker-2", Timestamp::from_unix_millis(20_000), ttl)
        .unwrap()
        .unwrap();
    let second_attempt = match reclaimed.status {
        CommandStatus::Leased { lease } => lease.attempt,
        _ => panic!("expected lease"),
    };
    assert_eq!(second_attempt.0, first_attempt.0 + 1);
    let _ = Engine::ClaudeCode;
    let _ = BranchName::new("branch");
}
