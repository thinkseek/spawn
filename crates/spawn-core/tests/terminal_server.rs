//! Objective 44: the Bridge records the terminal server an execution's
//! terminal started on (`execution.terminal_started`), read after the start
//! (which may be what started the server), so the recovery can tell the
//! server that ran an execution from one that answers later: after a host
//! restart the first resume starts a new server, and every other end read
//! after it is the restart's all the same. A record that fails is warned
//! once, naming the execution; a host that cannot tell records nothing.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use spawn_core::bridge::{Bridge, BridgeConfig, TickReport};
use spawn_core::command::{CommandAction, Submission};
use spawn_core::event::{
    Appended, ENGINE, EventKind, EventRecord, GlobalPosition, NewEvent,
};
use spawn_core::id::{
    AgentId, CommandId, ExecutionId, IdempotencyKey, Principal, SessionId, TerminalName,
};
use spawn_core::memory::MemoryStore;
use spawn_core::port::{CommandQueue, EventLog, ServerSeen, StoreError, TerminalServer};
use spawn_core::time::{ManualClock, Timestamp};

const READY: &str = "❯ \n  deepseek-ai/DeepSeek-V4.1-Flash · 余量未知 · 249 tok/s";

const SERVER: TerminalServer = TerminalServer {
    pid: 4242,
    started_at_ms: 1_794_330_336_000,
};

fn store() -> Arc<MemoryStore> {
    let clock: Arc<ManualClock> = Arc::new(ManualClock::new(Timestamp(1_000)));
    let store = Arc::new(MemoryStore::new("shop", clock));
    for command in 1..=3 {
        let execution = ExecutionId::for_command(CommandId(command));
        store.set_pane(&TerminalName::for_execution(&execution), READY);
    }
    store
}

fn start(store: &MemoryStore) {
    store
        .submit(
            &Principal::new("tester").expect("valid principal"),
            &Submission {
                agent_id: AgentId::new("agent_supervisor").expect("valid agent"),
                session_id: SessionId::new("session_supervisor").expect("valid session"),
                idempotency_key: IdempotencyKey::new("key_start").expect("valid key"),
                action: CommandAction::Start {
                    engine: ENGINE.to_string(),
                    prompt: "watch the fleet".to_string(),
                    parent_session_id: None,
                    from: None,
                    model: None,
                    charter: None,
                },
            },
        )
        .expect("submit succeeds");
}

fn tick(log: Arc<dyn EventLog>, store: &Arc<MemoryStore>) -> TickReport {
    let mut bridge = Bridge::new(
        log,
        store.clone(),
        store.clone(),
        store.clone(),
        BridgeConfig::new("tester"),
    );
    bridge.recover().expect("recover succeeds");
    bridge.tick().expect("tick succeeds")
}

fn events(store: &MemoryStore) -> Vec<NewEvent> {
    store
        .read_global(GlobalPosition(0), 1000)
        .expect("read succeeds")
        .into_iter()
        .map(|record| record.event)
        .collect()
}

/// Every `execution.terminal_started`: its execution and server.
fn servers(store: &MemoryStore) -> Vec<(String, TerminalServer)> {
    events(store)
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::TerminalStarted { server } => {
                Some((event.execution_id?.to_string(), server))
            }
            _ => None,
        })
        .collect()
}

fn started(store: &MemoryStore) -> bool {
    events(store)
        .iter()
        .any(|event| matches!(event.kind, EventKind::ExecutionStarted { .. }))
}

/// A log that refuses every `execution.terminal_started` while `refuse` is
/// set, as a write that failed.
struct Refusing(Arc<MemoryStore>, AtomicBool);

impl EventLog for Refusing {
    fn append(&self, event: NewEvent) -> Result<Appended, StoreError> {
        if self.1.load(Ordering::SeqCst) && matches!(event.kind, EventKind::TerminalStarted { .. })
        {
            return Err(StoreError::Backend("the write failed".to_string()));
        }
        self.0.append(event)
    }

    fn read_global(
        &self,
        after: GlobalPosition,
        limit: usize,
    ) -> Result<Vec<EventRecord>, StoreError> {
        self.0.read_global(after, limit)
    }

    fn open_execution(&self, session: &SessionId) -> Result<Option<EventRecord>, StoreError> {
        self.0.open_execution(session)
    }
}

#[test]
fn a_started_terminal_records_the_server_it_runs_on_after_the_execution() {
    let store = store();
    store.set_terminal_server(ServerSeen::Answers(Some(SERVER)));
    start(&store);
    let report = tick(store.clone(), &store);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(servers(&store), vec![("exec_1".to_string(), SERVER)]);
    // After the execution's record: the start that may start the server
    // comes after it.
    let names: Vec<&str> = events(&store)
        .iter()
        .map(|event| event.kind.name())
        .filter(|name| name.starts_with("execution."))
        .collect();
    assert_eq!(names, ["execution.started", "execution.terminal_started"]);
}

#[test]
fn a_host_that_cannot_tell_its_server_records_nothing_and_warns_nothing() {
    let store = store();
    start(&store);
    let report = tick(store.clone(), &store);
    assert!(started(&store));
    assert!(servers(&store).is_empty());
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
}

#[test]
fn a_record_that_fails_is_warned_once_naming_the_execution() {
    let store = store();
    store.set_terminal_server(ServerSeen::Answers(Some(SERVER)));
    start(&store);
    let log = Arc::new(Refusing(store.clone(), AtomicBool::new(true)));
    let report = tick(log.clone(), &store);
    // The execution runs all the same: the identity is the recovery's
    // evidence, not the start's.
    assert!(started(&store));
    assert!(servers(&store).is_empty());
    let warned: Vec<&String> = report
        .warnings
        .iter()
        .filter(|warning| warning.contains("terminal server"))
        .collect();
    assert_eq!(warned.len(), 1, "{:?}", report.warnings);
    assert!(warned[0].contains("exec_1"), "{warned:?}");
    assert!(warned[0].contains("the write failed"), "{warned:?}");
    // Once: the next tick does not warn again.
    log.1.store(false, Ordering::SeqCst);
    let again = tick(log, &store);
    assert!(
        !again
            .warnings
            .iter()
            .any(|warning| warning.contains("terminal server")),
        "{:?}",
        again.warnings
    );
}

#[test]
fn a_server_gone_right_after_the_start_is_warned_naming_the_execution() {
    let store = store();
    store.set_terminal_server(ServerSeen::Gone);
    start(&store);
    let report = tick(store.clone(), &store);
    assert!(servers(&store).is_empty());
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("exec_1") && warning.contains("terminal server")),
        "{:?}",
        report.warnings
    );
}
