use std::sync::Arc;

use spawn_gateway::{AllowList, Authorizer, CommandDraft, Gateway, GatewayError};
use spawn_projector::InMemoryViewStore;
use spawn_store::{EventLog, InMemoryStore};
use spawn_types::clock::{Clock, DeterministicClock};
use spawn_types::command::{CommandAction, IdempotencyKey, Principal};
use spawn_types::engine::Engine;
use spawn_types::event::GlobalPosition;
use spawn_types::identifiers::{AgentId, SessionId};

fn ids() -> (AgentId, SessionId) {
    (
        AgentId::from_prefixed("agent_aaaa").unwrap(),
        SessionId::from_prefixed("session_bbbb").unwrap(),
    )
}

fn build_gateway() -> (Gateway, Arc<InMemoryStore>, AgentId, SessionId) {
    let clock = Arc::new(DeterministicClock::starting_at(0));
    let store = Arc::new(InMemoryStore::new().with_clock(clock.clone()));
    let views = Arc::new(InMemoryViewStore::new());
    let (agent, session) = ids();
    let policy = Arc::new(AllowList::new().grant("alice", agent.as_str()));
    let gateway = Gateway::new(store.clone(), store.clone(), views, policy, clock);
    (gateway, store, agent, session)
}

fn stop_draft(agent: &AgentId, session: &SessionId, key: &str) -> CommandDraft {
    CommandDraft {
        agent_id: agent.clone(),
        session_id: session.clone(),
        idempotency_key: IdempotencyKey::new(key).unwrap(),
        action: CommandAction::Stop,
    }
}

#[test]
fn rejects_unauthorized_principal() {
    let (gateway, _store, agent, session) = build_gateway();
    let result = gateway.submit(
        Principal {
            name: "bob".to_string(),
        },
        stop_draft(&agent, &session, "key-1"),
    );
    assert!(matches!(result, Err(GatewayError::Unauthorized { .. })));
}

#[test]
fn rejects_oversized_payload() {
    let (gateway, _store, agent, session) = build_gateway();
    let draft = CommandDraft {
        agent_id: agent,
        session_id: session,
        idempotency_key: IdempotencyKey::new("big").unwrap(),
        action: CommandAction::Spawn {
            engine: Engine::ClaudeCode,
            prompt: "x".repeat(20_000),
        },
    };
    match gateway.submit(
        Principal {
            name: "alice".to_string(),
        },
        draft,
    ) {
        Err(GatewayError::PayloadTooLarge { size, limit }) => {
            assert_eq!(size, 20_000);
            assert_eq!(limit, 16 * 1024);
        }
        other => panic!("expected payload error, got {other:?}"),
    }
}

#[test]
fn submits_once_and_is_idempotent() {
    let (gateway, store, agent, session) = build_gateway();
    let principal = Principal {
        name: "alice".to_string(),
    };

    let first = gateway
        .submit(principal.clone(), stop_draft(&agent, &session, "idem-1"))
        .unwrap();
    assert!(first.inserted);

    let second = gateway
        .submit(principal, stop_draft(&agent, &session, "idem-1"))
        .unwrap();
    assert!(!second.inserted);
    assert_eq!(first.record.command_id, second.record.command_id);

    // Exactly one accepted event reaches the log despite two submissions.
    let events = store.read_global(GlobalPosition(0), 100).unwrap();
    assert_eq!(events.len(), 1);
}

#[test]
fn allowlist_matches_principal_agent_pair() {
    let (agent, _session) = ids();
    let policy = AllowList::new().grant("alice", agent.as_str());
    assert!(policy.allows(
        &Principal {
            name: "alice".to_string()
        },
        &agent
    ));
    assert!(!policy.allows(
        &Principal {
            name: "mallory".to_string()
        },
        &agent
    ));
}

#[test]
fn system_clock_supplies_increasing_instants() {
    let clock = spawn_types::clock::SystemClock;
    let earlier = clock.now();
    let later = clock.now();
    assert!(later.unix_millis() >= earlier.unix_millis());
}
