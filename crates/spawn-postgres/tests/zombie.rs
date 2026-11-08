//! Issue #15, item 2, against the log's own guard: a session whose latest
//! `execution.started` has no `execution.ended` the Bridge wrote, while no
//! runner of Spawn's runs it (after a restart its terminal is a shell a
//! terminal manager restored), is refused every start by
//! `spawn_append_trusted`. The sequence the reporter saw (the digest's
//! input, the liveness rule's resume, a start, a stop) now ends that
//! execution and goes on, and the log answers the same open execution the
//! guard refuses by ([`EventLog::open_execution`]).

mod common;

use std::sync::Arc;

use spawn_core::bridge::{Bridge, BridgeConfig};
use spawn_core::command::{CommandAction, Submission};
use spawn_core::event::{
    ENGINE, EndCause, EventKind, EventRecord, GlobalPosition, NewEvent, Source,
};
use spawn_core::id::{
    AgentId, CommandId, ExecutionId, IdempotencyKey, Principal, SessionId, TerminalName,
    WorkspaceId,
};
use spawn_core::memory::MemoryStore;
use spawn_core::port::{CommandQueue, EventLog, StoreError, TerminalHost};
use spawn_core::time::SystemClock;
use spawn_postgres::PostgresStore;

use common::store;

const READY: &str = "❯ \n  deepseek-ai/DeepSeek-V4.1-Flash · 余量未知 · 249 tok/s";

fn agent() -> AgentId {
    AgentId::new("agent_supervisor").expect("valid agent")
}

fn session() -> SessionId {
    SessionId::new("session_supervisor").expect("valid session")
}

fn terminal(command: u64) -> TerminalName {
    TerminalName::for_execution(&ExecutionId::for_command(CommandId(command)))
}

fn submit(store: &PostgresStore, key: &str, principal: &str, action: CommandAction) -> CommandId {
    store
        .submit(
            &Principal::new(principal).expect("valid principal"),
            &Submission {
                agent_id: agent(),
                session_id: session(),
                idempotency_key: IdempotencyKey::new(key).expect("valid key"),
                action,
            },
        )
        .expect("submit succeeds")
}

fn start(prompt: &str) -> CommandAction {
    CommandAction::Start {
        engine: ENGINE.to_string(),
        prompt: prompt.to_string(),
        parent_session_id: None,
        from: None,
        model: None,
        charter: None,
    }
}

fn resume() -> CommandAction {
    CommandAction::Resume {
        prompt: None,
        model: None,
    }
}

fn digest() -> CommandAction {
    CommandAction::Input {
        text: "digest: task 1 reported".to_string(),
        tags: Vec::new(),
    }
}

/// One `spawn tick` process: a new Bridge over `log` that refolds it.
fn tick(log: Arc<dyn EventLog>, store: &Arc<PostgresStore>, host: &Arc<MemoryStore>) {
    let mut bridge = Bridge::new(
        log,
        store.clone(),
        host.clone(),
        host.clone(),
        BridgeConfig::new("tester"),
    );
    bridge.recover().expect("recover");
    bridge.tick().expect("tick");
}

fn events(store: &PostgresStore) -> Vec<NewEvent> {
    store
        .read_global(GlobalPosition(0), 2000)
        .expect("read")
        .into_iter()
        .map(|record| record.event)
        .collect()
}

fn outcome(store: &PostgresStore, id: CommandId) -> (bool, String) {
    events(store)
        .into_iter()
        .find_map(|event| match event.kind {
            EventKind::CommandFinished {
                command_id,
                succeeded,
                error,
            } if command_id == id => Some((succeeded, error.unwrap_or_default())),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{id} finished"))
}

fn ends(store: &PostgresStore) -> Vec<(String, EndCause, Option<String>)> {
    events(store)
        .into_iter()
        .filter(|event| event.source == Source::Bridge)
        .filter_map(|event| match event.kind {
            EventKind::ExecutionEnded {
                cause, evidence, ..
            } => Some((event.execution_id?.to_string(), cause, evidence)),
            _ => None,
        })
        .collect()
}

/// The supervisor's exec_1 runs, then the host restarts: its runner died
/// without an exit status, and a terminal manager restored `spawn-exec_1`
/// at a shell prompt. The log holds exec_1 open.
fn restarted(case: &str) -> (Arc<PostgresStore>, Arc<MemoryStore>, common::Schema) {
    let (store, schema) = store(case);
    let store = Arc::new(store);
    let host = Arc::new(MemoryStore::new("supervisor", Arc::new(SystemClock)));
    for command in 1..=9 {
        host.set_pane(&terminal(command), READY);
    }
    submit(&store, "key_start", "spawn-liveness", start("charter"));
    tick(store.clone(), &store, &host);
    let workspace = WorkspaceId::new("1:/workspaces/1").expect("valid workspace");
    host.terminate(&terminal(1), &workspace)
        .expect("the host restarts");
    host.untrack(
        &terminal(1),
        "spawn-exec_1 does not host the runner Spawn started there: its pane was started with \
         no command and runs `bash`",
    );
    host.set_pane(&terminal(1), "user@digest:~$ ");
    let open = store
        .open_execution(&session())
        .expect("ask the log")
        .and_then(|record| record.event.execution_id);
    assert_eq!(open, Some(ExecutionId::for_command(CommandId(1))));
    (store, host, schema)
}

/// The log through the port's default `open_execution`: a scan of every
/// record.
struct Scan<'a>(&'a PostgresStore);

impl EventLog for Scan<'_> {
    fn append(&self, event: NewEvent) -> Result<spawn_core::event::Appended, StoreError> {
        self.0.append(event)
    }

    fn read_global(
        &self,
        after: GlobalPosition,
        limit: usize,
    ) -> Result<Vec<EventRecord>, StoreError> {
        self.0.read_global(after, limit)
    }
}

#[test]
fn the_log_answers_the_open_execution_its_guard_refuses_by() {
    let (store, _schema) = store("open_execution");
    assert!(store.open_execution(&session()).expect("ask").is_none());
    let append = |execution: &str, source: Source, key: &str, kind: EventKind| {
        store
            .append(NewEvent {
                agent_id: agent(),
                session_id: session(),
                execution_id: Some(ExecutionId::new(execution).expect("valid execution")),
                source,
                source_event_id: Some(key.to_string()),
                kind,
            })
            .expect("append");
    };
    let started = |command| EventKind::ExecutionStarted {
        engine: ENGINE.to_string(),
        workspace: WorkspaceId::new("1:/workspaces/1").expect("valid workspace"),
        command_id: CommandId(command),
        fork: None,
        model: spawn_core::event::ModelProfile::Default,
        transcript_from: None,
    };
    let ended = |cause| EventKind::ExecutionEnded {
        cause,
        exit_code: None,
        signal: None,
        evidence: None,
    };
    let open = || {
        store
            .open_execution(&session())
            .expect("ask")
            .and_then(|record| record.event.execution_id)
            .map(|id| id.to_string())
    };
    append("exec_1", Source::Bridge, "exec_1:started", started(1));
    assert_eq!(open().as_deref(), Some("exec_1"));
    // An old runner's exit report is no end.
    append(
        "exec_1",
        Source::Claude,
        "exec_1:exit",
        ended(EndCause::Exited),
    );
    assert_eq!(open().as_deref(), Some("exec_1"));
    append(
        "exec_1",
        Source::Bridge,
        "exec_1:ended",
        ended(EndCause::Lost),
    );
    assert_eq!(open(), None);
    append("exec_2", Source::Bridge, "exec_2:started", started(2));
    assert_eq!(open().as_deref(), Some("exec_2"));
    // The default (a scan of the whole log) agrees with the query.
    let scanned = Scan(&store)
        .open_execution(&session())
        .expect("scan")
        .and_then(|record| record.event.execution_id)
        .map(|id| id.to_string());
    assert_eq!(scanned, open());
}

#[test]
fn the_digest_input_ends_it_and_the_liveness_resume_goes_on() {
    let (store, host, _schema) = restarted("zombie_input");
    let input = submit(&store, "dispatch-digest-1", "spawn-dispatch", digest());
    tick(store.clone(), &store, &host);
    let (succeeded, error) = outcome(&store, input);
    assert!(!succeeded);
    assert!(
        error.contains("was not running") && error.contains("ended as lost"),
        "{error}"
    );
    let ends = ends(&store);
    assert_eq!(ends.len(), 1, "{ends:?}");
    assert_eq!((ends[0].0.as_str(), ends[0].1), ("exec_1", EndCause::Lost));
    assert!(
        ends[0]
            .2
            .as_deref()
            .is_some_and(|text| text.contains("runs `bash`")),
        "{:?}",
        ends[0].2
    );
    // The guard now takes the resume.
    let resumed = submit(&store, "liveness-resume-exec_1", "spawn-liveness", resume());
    tick(store.clone(), &store, &host);
    assert!(outcome(&store, resumed).0, "{:?}", outcome(&store, resumed));
    assert!(store.open_execution(&session()).expect("ask").is_some());
}

#[test]
fn a_start_and_a_stop_end_it_rather_than_fail() {
    let (store, host, _schema) = restarted("zombie_start");
    let started = submit(&store, "key_again", "spawn-liveness", start("charter"));
    tick(store.clone(), &store, &host);
    assert!(outcome(&store, started).0, "{:?}", outcome(&store, started));
    assert_eq!(ends(&store).len(), 1);

    let (store, host, _schema) = restarted("zombie_stop");
    let stopped = submit(&store, "key_stop", "tester", CommandAction::Stop);
    tick(store.clone(), &store, &host);
    assert!(outcome(&store, stopped).0);
    let ends = ends(&store);
    assert_eq!(ends.len(), 1, "the stop wrote the end the log waited for");
    assert_eq!(ends[0].1, EndCause::Stopped);
    assert!(store.open_execution(&session()).expect("ask").is_none());
}

/// A log whose reader missed exec_1's start and its mount: the Bridge's
/// view has no execution while the log holds one open, as the reporter
/// found (input "runs no execution", start refused by the guard, stop
/// "succeeded" without an end). Asked directly, the log answers truly.
struct Missed(Arc<PostgresStore>);

impl EventLog for Missed {
    fn append(&self, event: NewEvent) -> Result<spawn_core::event::Appended, StoreError> {
        self.0.append(event)
    }

    fn read_global(
        &self,
        after: GlobalPosition,
        limit: usize,
    ) -> Result<Vec<EventRecord>, StoreError> {
        let first = ExecutionId::for_command(CommandId(1));
        Ok(self
            .0
            .read_global(after, limit)?
            .into_iter()
            .filter(|record| {
                let start = matches!(record.event.kind, EventKind::ExecutionStarted { .. })
                    && record.event.execution_id.as_ref() == Some(&first);
                let step = matches!(
                    record.event.kind,
                    EventKind::CommandStepped {
                        command_id: CommandId(1),
                        ..
                    }
                );
                !(start || step)
            })
            .collect())
    }

    fn open_execution(&self, session: &SessionId) -> Result<Option<EventRecord>, StoreError> {
        self.0.open_execution(session)
    }
}

#[test]
fn a_view_that_lost_the_execution_converges_with_the_log() {
    for (case, action) in [
        ("lost_view_input", digest()),
        ("lost_view_start", start("charter")),
        ("lost_view_stop", CommandAction::Stop),
    ] {
        let (store, host, _schema) = restarted(case);
        let stop = matches!(action, CommandAction::Stop);
        let input = matches!(action, CommandAction::Input { .. });
        let id = submit(&store, "key_command", "spawn-dispatch", action);
        tick(Arc::new(Missed(store.clone())), &store, &host);
        let (succeeded, error) = outcome(&store, id);
        assert_eq!(succeeded, !input, "{case}: {error}");
        if input {
            assert!(error.contains("ended as lost"), "{case}: {error}");
        }
        let ends = ends(&store);
        assert_eq!(ends.len(), 1, "{case}: {ends:?}");
        let cause = if stop {
            EndCause::Stopped
        } else {
            EndCause::Lost
        };
        assert_eq!(ends[0].1, cause, "{case}");
    }
}
