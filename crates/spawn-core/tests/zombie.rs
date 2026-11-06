//! Issue #15, item 2: an execution the log holds open that nothing runs.
//!
//! After a restart a session's latest `execution.started` can have no
//! `execution.ended` while no runner of Spawn's runs it: the runner died
//! with the host and left no exit status, and its terminal is gone, or is
//! there and hosts no runner (a session a terminal manager restored at a
//! shell prompt, a dead pane). The log's guard then refuses every start or
//! resume until the Bridge writes that end. The Bridge called an execution
//! running while a terminal of its name existed, and handled commands
//! before it advanced executions, so the end never came while the terminal
//! stayed, and every start, resume and input failed meanwhile. Each of
//! start, resume, input and stop now ends such an execution (as lost, with
//! what the host saw; a stop as stopped) and goes on; so does the tick.
//! The same holds when the Bridge's own view has lost the execution the log
//! holds open (the reporter's state: input "runs no execution", start
//! refused by the log, stop "succeeded" with no end).

use std::sync::Arc;

use spawn_core::bridge::{Bridge, BridgeConfig, TickReport};
use spawn_core::command::{CommandAction, Submission};
use spawn_core::event::{ENGINE, EndCause, EventKind, EventRecord, GlobalPosition, NewEvent};
use spawn_core::id::{
    AgentId, CommandId, ExecutionId, IdempotencyKey, Principal, SessionId, TerminalName,
    WorkspaceId,
};
use spawn_core::memory::MemoryStore;
use spawn_core::port::{CommandQueue, EventLog, StoreError, TerminalHost};
use spawn_core::time::{ManualClock, Timestamp};

const READY: &str = "❯ \n  deepseek-ai/DeepSeek-V4.1-Flash · 余量未知 · 249 tok/s";

/// What the host saw in the restored terminal.
const RESTORED: &str = "spawn-exec_1 does not host the runner Spawn started there: its pane was \
                        started with no command and runs `bash`";

fn store() -> Arc<MemoryStore> {
    let clock: Arc<ManualClock> = Arc::new(ManualClock::new(Timestamp(1_000)));
    let store = Arc::new(MemoryStore::new("shop", clock));
    for command in 1..=9 {
        let execution = ExecutionId::for_command(CommandId(command));
        store.set_pane(&TerminalName::for_execution(&execution), READY);
    }
    store
}

fn session() -> SessionId {
    SessionId::new("session_supervisor").expect("valid session")
}

fn first() -> TerminalName {
    TerminalName::for_execution(&ExecutionId::for_command(CommandId(1)))
}

fn any_workspace() -> WorkspaceId {
    WorkspaceId::new("1:/workspaces/1").expect("valid workspace")
}

fn submit(store: &MemoryStore, key: &str, principal: &str, action: CommandAction) {
    store
        .submit(
            &Principal::new(principal).expect("valid principal"),
            &Submission {
                agent_id: AgentId::new("agent_supervisor").expect("valid agent"),
                session_id: session(),
                idempotency_key: IdempotencyKey::new(key).expect("valid key"),
                action,
            },
        )
        .expect("submit succeeds");
}

fn start(prompt: &str) -> CommandAction {
    CommandAction::Start {
        engine: ENGINE.to_string(),
        prompt: prompt.to_string(),
        parent_session_id: None,
        from: None,
        model: None,
    }
}

/// One `spawn tick` process over `log`: a new Bridge that refolds it first.
fn tick_over(log: Arc<dyn EventLog>, store: &Arc<MemoryStore>) -> TickReport {
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

fn tick_fresh(store: &Arc<MemoryStore>) -> TickReport {
    tick_over(store.clone(), store)
}

fn events(store: &MemoryStore) -> Vec<NewEvent> {
    store
        .read_global(GlobalPosition(0), 1000)
        .expect("read succeeds")
        .into_iter()
        .map(|record| record.event)
        .collect()
}

/// Every `execution.ended` the Bridge wrote: its execution, cause and
/// evidence.
fn ends(store: &MemoryStore) -> Vec<(String, EndCause, Option<String>)> {
    events(store)
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::ExecutionEnded {
                cause, evidence, ..
            } => Some((event.execution_id?.to_string(), cause, evidence)),
            _ => None,
        })
        .collect()
}

/// How the command `id` finished: succeeded, and its error.
fn outcome(store: &MemoryStore, id: u64) -> (bool, String) {
    events(store)
        .into_iter()
        .find_map(|event| match event.kind {
            EventKind::CommandFinished {
                command_id,
                succeeded,
                error,
            } if command_id == CommandId(id) => Some((succeeded, error.unwrap_or_default())),
            _ => None,
        })
        .unwrap_or_else(|| panic!("command {id} finished: {:?}", events(store)))
}

fn started(store: &MemoryStore) -> Vec<String> {
    events(store)
        .into_iter()
        .filter(|event| matches!(event.kind, EventKind::ExecutionStarted { .. }))
        .filter_map(|event| event.execution_id.map(|id| id.to_string()))
        .collect()
}

/// How the runner of exec_1 is gone after the restart.
#[derive(Clone, Copy, Debug)]
enum Gone {
    /// Its terminal went with it.
    Terminal,
    /// Its terminal is there, at a shell prompt, hosting no runner.
    Restored,
}

/// exec_1 runs (its prompt typed), then the host restarts: the runner is
/// gone without an exit status, as `how` says. Nothing is ended yet.
fn zombie(how: Gone) -> Arc<MemoryStore> {
    let store = store();
    submit(&store, "key_start", "tester", start("watch the fleet"));
    tick_fresh(&store);
    assert_eq!(started(&store), vec!["exec_1"]);
    match how {
        Gone::Terminal => store.terminate(&first(), &any_workspace()).expect("kill"),
        Gone::Restored => {
            store.terminate(&first(), &any_workspace()).expect("kill");
            store.untrack(&first(), RESTORED);
            store.set_pane(&first(), "user@host:~/ws/agent_supervisor$ ");
        }
    }
    assert!(ends(&store).is_empty());
    store
}

/// The evidence names what the host saw.
fn saw(evidence: Option<&String>, how: Gone) -> bool {
    let evidence = evidence.map_or("", String::as_str);
    match how {
        Gone::Terminal => evidence.contains("spawn-exec_1 is gone"),
        Gone::Restored => evidence.contains(RESTORED),
    }
}

#[test]
fn a_terminal_that_hosts_no_runner_holds_no_execution_open() {
    let store = zombie(Gone::Restored);
    // The next tick ends it: "a lost execution gets its end from the next
    // tick" holds for a terminal that is there but hosts no runner.
    tick_fresh(&store);
    let ends = ends(&store);
    assert_eq!(ends.len(), 1, "{ends:?}");
    let (execution, cause, evidence) = &ends[0];
    assert_eq!((execution.as_str(), *cause), ("exec_1", EndCause::Lost));
    assert!(saw(evidence.as_ref(), Gone::Restored), "{evidence:?}");
    assert!(
        evidence
            .as_deref()
            .is_some_and(|text| text.contains("no exit status")),
        "{evidence:?}"
    );
    assert!(
        !store.is_alive(&first(), &any_workspace()).expect("alive"),
        "the restored terminal was killed, not adopted"
    );
    assert!(
        store.workspace_calls().iter().any(|call| call == "unmount"),
        "the end unmounted the workspace after its final checkpoint"
    );
}

#[test]
fn a_start_ends_an_execution_nothing_runs_and_starts() {
    for how in [Gone::Terminal, Gone::Restored] {
        let store = zombie(how);
        submit(&store, "key_again", "spawn-liveness", start("charter"));
        tick_fresh(&store);
        let (succeeded, error) = outcome(&store, 2);
        assert!(succeeded, "{how:?}: {error}");
        let ends = ends(&store);
        assert_eq!(ends.len(), 1, "{how:?}: {ends:?}");
        assert_eq!((ends[0].0.as_str(), ends[0].1), ("exec_1", EndCause::Lost));
        assert!(saw(ends[0].2.as_ref(), how), "{how:?}: {:?}", ends[0].2);
        assert!(
            ends[0]
                .2
                .as_deref()
                .is_some_and(|text| text.contains("cmd_2")),
            "the evidence names the command that found it: {:?}",
            ends[0].2
        );
        assert_eq!(started(&store), vec!["exec_1", "exec_2"], "{how:?}");
    }
}

#[test]
fn a_resume_ends_an_execution_nothing_runs_and_resumes_its_session() {
    for how in [Gone::Terminal, Gone::Restored] {
        let store = zombie(how);
        submit(
            &store,
            "key_resume",
            "spawn-liveness",
            CommandAction::Resume {
                prompt: None,
                model: None,
            },
        );
        tick_fresh(&store);
        let (succeeded, error) = outcome(&store, 2);
        assert!(succeeded, "{how:?}: {error}");
        assert_eq!(ends(&store).len(), 1, "{how:?}");
        assert_eq!(ends(&store)[0].1, EndCause::Lost);
        assert_eq!(started(&store), vec!["exec_1", "exec_2"], "{how:?}");
        let resumed = store.launches().last().cloned().expect("a launch");
        assert!(
            resumed.arguments.iter().any(|word| word == "--resume"),
            "{how:?}: the same native session: {:?}",
            resumed.arguments
        );
    }
}

#[test]
fn an_input_ends_an_execution_nothing_runs_and_the_resume_goes_on() {
    for (how, principal) in [
        (Gone::Terminal, "spawn-dispatch"),
        (Gone::Restored, "spawn-dispatch"),
        (Gone::Restored, "tester"),
    ] {
        let store = zombie(how);
        submit(
            &store,
            "key_digest",
            principal,
            CommandAction::Input {
                text: "digest".to_string(),
                tags: Vec::new(),
            },
        );
        tick_fresh(&store);
        let (succeeded, error) = outcome(&store, 2);
        assert!(!succeeded, "{how:?} {principal}");
        assert!(
            error.contains("was not running")
                && error.contains("ended as lost")
                && error.contains("resume the session"),
            "{how:?} {principal}: {error}"
        );
        assert_eq!(ends(&store).len(), 1, "{how:?} {principal}");
        // Nothing was typed into the restored shell.
        assert_eq!(
            store.submitted_held().len(),
            1,
            "only the start's prompt was typed"
        );
        // The normal resume path applies.
        submit(
            &store,
            "key_resume",
            "spawn-liveness",
            CommandAction::Resume {
                prompt: None,
                model: None,
            },
        );
        tick_fresh(&store);
        assert!(outcome(&store, 3).0, "{how:?} {principal}");
        assert_eq!(started(&store), vec!["exec_1", "exec_3"]);
    }
}

#[test]
fn a_stop_writes_the_end_it_finds_no_runner_for() {
    for how in [Gone::Terminal, Gone::Restored] {
        let store = zombie(how);
        submit(&store, "key_stop", "tester", CommandAction::Stop);
        tick_fresh(&store);
        assert!(outcome(&store, 2).0, "{how:?}");
        let ends = ends(&store);
        assert_eq!(ends.len(), 1, "{how:?}: {ends:?}");
        assert_eq!(ends[0].1, EndCause::Stopped, "a stop is final");
        assert!(saw(ends[0].2.as_ref(), how), "{how:?}: {:?}", ends[0].2);
        assert!(
            ends[0]
                .2
                .as_deref()
                .is_some_and(|text| text.contains("no runner was running when cmd_2 came")),
            "{:?}",
            ends[0].2
        );
        assert!(!store.is_alive(&first(), &any_workspace()).expect("alive"));
    }
}

/// A log whose reader never sees exec_1's start: a Bridge over it has lost
/// that execution from its view while the log holds it open, the state the
/// issue's reporter found (input "runs no execution", start refused by the
/// log, stop "succeeded" without an end). Asked directly, the log answers
/// truly.
struct Missed(Arc<MemoryStore>);

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
                !(matches!(record.event.kind, EventKind::ExecutionStarted { .. })
                    && record.event.execution_id.as_ref() == Some(&first))
            })
            .collect())
    }

    fn open_execution(&self, session: &SessionId) -> Result<Option<EventRecord>, StoreError> {
        self.0.open_execution(session)
    }
}

#[test]
fn an_execution_the_view_lost_and_the_log_holds_open_is_ended_by_each_command() {
    // Input: ended as lost, not "runs no execution".
    let store = zombie(Gone::Restored);
    let missed: Arc<dyn EventLog> = Arc::new(Missed(store.clone()));
    submit(
        &store,
        "key_digest",
        "spawn-dispatch",
        CommandAction::Input {
            text: "digest".to_string(),
            tags: Vec::new(),
        },
    );
    let report = tick_over(missed.clone(), &store);
    let (succeeded, error) = outcome(&store, 2);
    assert!(!succeeded);
    assert!(error.contains("ended as lost"), "{error}");
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("the log holds execution exec_1 open")),
        "{:?}",
        report.warnings
    );
    assert_eq!(ends(&store).len(), 1);

    // Start: the log's guard no longer refuses it.
    let store = zombie(Gone::Restored);
    let missed: Arc<dyn EventLog> = Arc::new(Missed(store.clone()));
    submit(&store, "key_again", "spawn-liveness", start("charter"));
    tick_over(missed, &store);
    let (succeeded, error) = outcome(&store, 2);
    assert!(succeeded, "{error}");
    assert_eq!(started(&store), vec!["exec_1", "exec_2"]);

    // Stop: it writes the end the log waits for.
    let store = zombie(Gone::Restored);
    let missed: Arc<dyn EventLog> = Arc::new(Missed(store.clone()));
    submit(&store, "key_stop", "tester", CommandAction::Stop);
    tick_over(missed, &store);
    assert!(outcome(&store, 2).0);
    assert_eq!(ends(&store).len(), 1, "the stop wrote the end");
    assert_eq!(ends(&store)[0].1, EndCause::Stopped);
}

#[test]
fn an_engine_its_runner_still_runs_is_never_ended_by_a_start() {
    // The view lost exec_1, but Spawn's runner still runs it: the start is
    // refused (the session runs an execution), nothing is killed, and the
    // execution is followed again.
    let store = store();
    submit(&store, "key_start", "tester", start("watch the fleet"));
    tick_fresh(&store);
    let missed: Arc<dyn EventLog> = Arc::new(Missed(store.clone()));
    submit(&store, "key_again", "spawn-liveness", start("charter"));
    tick_over(missed, &store);
    let (succeeded, error) = outcome(&store, 2);
    assert!(!succeeded);
    assert!(error.contains("already runs"), "{error}");
    assert!(ends(&store).is_empty());
    assert!(store.is_alive(&first(), &any_workspace()).expect("alive"));
    // And an input reaches it.
    submit(
        &store,
        "key_input",
        "tester",
        CommandAction::Input {
            text: "status?".to_string(),
            tags: Vec::new(),
        },
    );
    tick_over(Arc::new(Missed(store.clone())), &store);
    assert!(outcome(&store, 3).0, "{:?}", outcome(&store, 3));
    assert!(ends(&store).is_empty());
}
