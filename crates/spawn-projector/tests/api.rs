use std::sync::Arc;

use spawn_projector::{InMemoryViewStore, Projector, ViewStore, event_kind_name, fold_batch};
use spawn_store::{EventLog, InMemoryStore};
use spawn_types::clock::DeterministicClock;
use spawn_types::engine::Engine;
use spawn_types::event::{EventKind, EventSource, GlobalPosition, NewEvent};
use spawn_types::identifiers::{AgentId, ExecutionId, SessionId};

fn ids() -> (AgentId, SessionId) {
    (
        AgentId::from_prefixed("agent_aaaa").unwrap(),
        SessionId::from_prefixed("session_bbbb").unwrap(),
    )
}

fn event(
    agent: &AgentId,
    session: &SessionId,
    execution: Option<ExecutionId>,
    key: &str,
    kind: EventKind,
) -> NewEvent {
    NewEvent {
        agent_id: agent.clone(),
        session_id: session.clone(),
        execution_id: execution,
        source: EventSource::ClaudeHooks,
        source_event_id: Some(key.to_string()),
        kind,
        artifact: None,
        occurred_at: None,
    }
}

#[test]
fn fold_batch_advances_cursor_and_emits_timeline() {
    let clock = Arc::new(DeterministicClock::starting_at(0));
    let store = InMemoryStore::new().with_clock(clock);
    let (agent, session) = ids();
    store
        .append(event(
            &agent,
            &session,
            None,
            "k1",
            EventKind::TurnStopped {
                input_tokens: None,
                output_tokens: None,
            },
        ))
        .unwrap();
    let records = store.read_global(GlobalPosition(0), 100).unwrap();
    let (patches, cursor) = fold_batch(&records, GlobalPosition(0));
    assert_eq!(cursor, GlobalPosition(1));
    assert_eq!(patches.len(), 1);
    assert!(matches!(
        patches[0],
        spawn_projector::ViewPatch::UpsertTimeline(_)
    ));
    assert_eq!(event_kind_name(&records[0].event.kind), "turn.stopped");
}

#[test]
fn projector_materializes_execution_and_is_resumable() {
    let clock = Arc::new(DeterministicClock::starting_at(0));
    let log = Arc::new(InMemoryStore::new().with_clock(clock));
    let views = Arc::new(InMemoryViewStore::new());
    let (agent, session) = ids();
    let execution = ExecutionId::from_prefixed("exec_9999").unwrap();

    log.append(event(
        &agent,
        &session,
        Some(execution.clone()),
        "start",
        EventKind::ExecutionStarted {
            engine: Engine::Codex,
            terminal_name: "term-2".to_string(),
            mount_id: "8:/ws".to_string(),
        },
    ))
    .unwrap();
    log.append(event(
        &agent,
        &session,
        Some(execution.clone()),
        "exit",
        EventKind::ExecutionExited { exit_code: Some(0) },
    ))
    .unwrap();

    let projector = Projector::new(log.clone(), views.clone(), GlobalPosition(0));
    let cursor = projector.process_available().unwrap();
    assert_eq!(cursor, GlobalPosition(2));

    let tables = views.snapshot();
    assert_eq!(tables.timeline.len(), 2);
    let lookup_id = ExecutionId::from_prefixed("exec_9999").unwrap();
    let row = tables
        .executions
        .get(&lookup_id)
        .expect("execution row exists");
    assert_eq!(row.engine, Engine::Codex);
    assert_eq!(row.exit_code, Some(0));
    assert!(row.ended_at.is_some());

    // Restarting from the committed cursor applies nothing twice.
    let restarted = Projector::new(log, views.clone(), cursor);
    let same = restarted.process_available().unwrap();
    assert_eq!(same, cursor);
    assert_eq!(views.snapshot().timeline.len(), 2);
}

#[test]
fn closing_unknown_execution_is_a_view_error() {
    let views = InMemoryViewStore::new();
    let result = views.apply(spawn_projector::ViewPatch::CloseExecution {
        execution_id: ExecutionId::from_prefixed("exec_missing").unwrap(),
        ended_at: spawn_types::clock::Timestamp::from_unix_millis(1),
        exit_code: Some(1),
    });
    assert!(result.is_err());
}
