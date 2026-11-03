use std::sync::Arc;
use std::time::Duration;

use spawn_observer::{HeuristicJudge, Observer, SessionSnapshot, StateJudge};
use spawn_store::{EventLog, InMemoryStore};
use spawn_types::clock::DeterministicClock;
use spawn_types::engine::Engine;
use spawn_types::event::{EventKind, EventSource, NewEvent};
use spawn_types::identifiers::{AgentId, ExecutionId, SessionId};
use spawn_types::observation::SemanticState;

fn ids() -> (AgentId, SessionId) {
    (
        AgentId::from_prefixed("agent_aaaa").unwrap(),
        SessionId::from_prefixed("session_bbbb").unwrap(),
    )
}

fn new_event(
    agent: &AgentId,
    session: &SessionId,
    execution: Option<&ExecutionId>,
    key: &str,
    kind: EventKind,
) -> NewEvent {
    NewEvent {
        agent_id: agent.clone(),
        session_id: session.clone(),
        execution_id: execution.cloned(),
        source: EventSource::ClaudeHooks,
        source_event_id: Some(key.to_string()),
        kind,
        artifact: None,
        occurred_at: None,
    }
}

#[test]
fn snapshot_detects_open_tools_and_exit() {
    let clock = Arc::new(DeterministicClock::starting_at(0));
    let store = InMemoryStore::new().with_clock(clock);
    let (agent, session) = ids();
    let execution = ExecutionId::from_prefixed("exec_1111").unwrap();
    store
        .append(new_event(
            &agent,
            &session,
            Some(&execution),
            "start",
            EventKind::ExecutionStarted {
                engine: Engine::ClaudeCode,
                terminal_name: "term-1".to_string(),
                mount_id: "7:/workspace".to_string(),
            },
        ))
        .unwrap();
    store
        .append(new_event(
            &agent,
            &session,
            Some(&execution),
            "tool",
            EventKind::ToolStarted {
                tool_name: "Bash".to_string(),
                tool_correlation_id: "t1".to_string(),
            },
        ))
        .unwrap();

    let records = store
        .read_session(&session, spawn_types::event::SessionSeq(0), 100)
        .unwrap();
    let snapshot = SessionSnapshot::fold(records.iter());
    assert!(snapshot.execution_active);
    assert_eq!(snapshot.open_tool_count, 1);

    let judgement = HeuristicJudge
        .judge(
            &snapshot,
            spawn_types::clock::Timestamp::from_unix_millis(0),
        )
        .unwrap();
    assert_eq!(judgement.state, SemanticState::Working);
}

#[test]
fn observer_emits_once_per_state_change_and_skips_own_events() {
    let clock = Arc::new(DeterministicClock::starting_at(1_000));
    let store = Arc::new(InMemoryStore::new().with_clock(clock.clone()));
    let judge = Arc::new(HeuristicJudge::new());
    let mut observer = Observer::new(store.clone(), judge, clock.clone(), Duration::from_secs(1));
    let (agent, session) = ids();
    let execution = ExecutionId::from_prefixed("exec_2222").unwrap();

    store
        .append(new_event(
            &agent,
            &session,
            Some(&execution),
            "start",
            EventKind::ExecutionStarted {
                engine: Engine::ClaudeCode,
                terminal_name: "term-1".to_string(),
                mount_id: "7:/workspace".to_string(),
            },
        ))
        .unwrap();

    let first = observer.tick().unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].state, SemanticState::Working);
    assert_eq!(first[0].judge_name, "heuristic");

    // No new evidence and an unchanged conclusion produce no observation.
    clock.advance_millis(1);
    let second = observer.tick().unwrap();
    assert!(second.is_empty());

    // The observer-created event must not feed back into another judgement.
    clock.advance_millis(1);
    let third = observer.tick().unwrap();
    assert!(third.is_empty());

    let records = store
        .read_session(&session, spawn_types::event::SessionSeq(0), 100)
        .unwrap();
    let observer_events = records
        .iter()
        .filter(|record| record.event.source == EventSource::Observer)
        .count();
    assert_eq!(observer_events, 1);
}

#[test]
fn heartbeat_timeout_degrades_to_unknown() {
    let clock = Arc::new(DeterministicClock::starting_at(0));
    let store = Arc::new(InMemoryStore::new().with_clock(clock.clone()));
    let judge = Arc::new(HeuristicJudge::new());
    let mut observer = Observer::new(store.clone(), judge, clock.clone(), Duration::from_secs(10));
    let (agent, session) = ids();
    let execution = ExecutionId::from_prefixed("exec_3333").unwrap();
    store
        .append(new_event(
            &agent,
            &session,
            Some(&execution),
            "start",
            EventKind::ExecutionStarted {
                engine: Engine::ClaudeCode,
                terminal_name: "term-1".to_string(),
                mount_id: "7:/workspace".to_string(),
            },
        ))
        .unwrap();

    clock.advance_millis(11_000);
    let emitted = observer.tick().unwrap();
    assert_eq!(emitted.len(), 1);
    assert_eq!(emitted[0].state, SemanticState::Unknown);
}
