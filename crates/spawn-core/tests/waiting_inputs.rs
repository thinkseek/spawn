//! An input to a busy session waits for its turn (design §5, objective
//! 015): recorded `waiting`, typed when the pane reads ready, oldest first,
//! at most once, and failed with the reason when the session's execution
//! ends first. Inputs of Spawn's own loops keep their contract: refused at
//! once while the pane is not ready.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use spawn_core::bridge::{Bridge, BridgeConfig, MAY_HAVE_TYPED, PANE_NOT_READY};
use spawn_core::command::{CommandAction, Submission};
use spawn_core::event::{CommandStep, ENGINE, EventKind, GlobalPosition, NewEvent, Source};
use spawn_core::id::{
    AgentId, CommandId, ExecutionId, IdempotencyKey, Principal, SessionId, TerminalName,
    WorkspaceId,
};
use spawn_core::memory::MemoryStore;
use spawn_core::port::{CommandQueue, EventLog, Launch, TerminalError, TerminalHost};
use spawn_core::time::{ManualClock, Timestamp};

const READY: &str = "❯ \n  deepseek-ai/DeepSeek-V4.1-Flash · 余量未知 · 249 tok/s";
const BUSY: &str = "✻ Churning… (esc to interrupt)\n❯ ";

/// The memory terminal, keeping every text that was submitted.
struct Recording {
    inner: Arc<MemoryStore>,
    typed: Mutex<Vec<String>>,
    staged: Mutex<Option<String>>,
}

impl TerminalHost for Recording {
    fn start(&self, launch: &Launch) -> Result<(), TerminalError> {
        self.inner.start(launch)
    }

    fn stage_text(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
        text: &str,
    ) -> Result<(), TerminalError> {
        *self.staged.lock().expect("staged") = Some(text.to_string());
        self.inner.stage_text(terminal, workspace, text)
    }

    fn submit_staged(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
    ) -> Result<(), TerminalError> {
        if let Some(text) = self.staged.lock().expect("staged").take() {
            self.typed.lock().expect("typed").push(text);
        }
        self.inner.submit_staged(terminal, workspace)
    }

    fn send_keys(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
        keys: &[&str],
    ) -> Result<(), TerminalError> {
        self.inner.send_keys(terminal, workspace, keys)
    }

    fn is_alive(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
    ) -> Result<bool, TerminalError> {
        self.inner.is_alive(terminal, workspace)
    }

    fn capture_tail(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
        lines: usize,
    ) -> Result<String, TerminalError> {
        self.inner.capture_tail(terminal, workspace, lines)
    }

    fn read_from(
        &self,
        workspace: &WorkspaceId,
        path: &str,
        offset: u64,
    ) -> Result<Option<Vec<u8>>, TerminalError> {
        self.inner.read_from(workspace, path, offset)
    }

    fn write_new(
        &self,
        workspace: &WorkspaceId,
        path: &str,
        bytes: &[u8],
    ) -> Result<bool, TerminalError> {
        self.inner.write_new(workspace, path, bytes)
    }

    fn terminate(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
    ) -> Result<(), TerminalError> {
        self.inner.terminate(terminal, workspace)
    }
}

struct Rig {
    store: Arc<MemoryStore>,
    host: Arc<Recording>,
    clock: Arc<ManualClock>,
    config: BridgeConfig,
    bridge: Bridge,
}

fn terminal() -> TerminalName {
    TerminalName::for_execution(&ExecutionId::for_command(CommandId(1)))
}

fn workspace() -> WorkspaceId {
    WorkspaceId::new("1:/workspaces/1").expect("valid workspace")
}

impl Rig {
    /// A live engine at its prompt (started by command 1, with no prompt),
    /// whose waiting inputs are claimed again after `retry`.
    fn new(retry: Duration) -> Self {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        store.set_pane(&terminal(), READY);
        let host = Arc::new(Recording {
            inner: store.clone(),
            typed: Mutex::default(),
            staged: Mutex::default(),
        });
        let mut config = BridgeConfig::new("tester");
        config.ready_interval = Duration::ZERO;
        config.input_wait_retry = retry;
        let bridge = Self::bridge(&store, &host, &clock, &config);
        let mut rig = Self {
            store,
            host,
            clock,
            config,
            bridge,
        };
        rig.submit(
            "bb-harness",
            "key_start",
            CommandAction::Start {
                engine: ENGINE.to_string(),
                prompt: String::new(),
                parent_session_id: None,
                from: None,
                model: None,
            },
        );
        rig.tick();
        assert!(rig.finished(1).is_some_and(|(ok, _)| ok), "the start");
        rig
    }

    fn bridge(
        store: &Arc<MemoryStore>,
        host: &Arc<Recording>,
        clock: &Arc<ManualClock>,
        config: &BridgeConfig,
    ) -> Bridge {
        Bridge::new(
            store.clone(),
            store.clone(),
            store.clone(),
            host.clone(),
            config.clone(),
        )
        .with_clock(clock.clone())
    }

    /// The Bridge of a restarted loop: a new one, rebuilt from the log.
    fn restart(&mut self) {
        self.bridge = Self::bridge(&self.store, &self.host, &self.clock, &self.config);
        self.bridge.recover().expect("recover");
    }

    fn submit(&self, principal: &str, key: &str, action: CommandAction) {
        self.store
            .submit(
                &Principal::new(principal).expect("valid principal"),
                &Submission {
                    agent_id: AgentId::new("agent_one").expect("valid agent"),
                    session_id: SessionId::new("session_one").expect("valid session"),
                    idempotency_key: IdempotencyKey::new(key).expect("valid key"),
                    action,
                },
            )
            .expect("submit");
    }

    fn say(&self, principal: &str, key: &str, text: &str) {
        self.submit(
            principal,
            key,
            CommandAction::Input {
                text: text.to_string(),
                tags: Vec::new(),
            },
        );
    }

    fn tick(&mut self) {
        self.bridge.tick().expect("tick");
    }

    fn pane(&self, pane: &str) {
        self.store.set_pane(&terminal(), pane);
    }

    /// The engine's turn for what was typed: busy, then at its prompt again,
    /// each read by the Bridge on two ticks (a screen is recorded once two
    /// readings agree).
    fn turn(&mut self) {
        self.pane(BUSY);
        self.tick();
        self.tick();
        self.pane(READY);
        self.tick();
        self.tick();
    }

    fn events(&self) -> Vec<EventKind> {
        self.store
            .read_global(GlobalPosition(0), 10_000)
            .expect("read")
            .into_iter()
            .map(|record| record.event.kind)
            .collect()
    }

    /// `(succeeded, error)` of the command's finish, if it finished.
    fn finished(&self, command: u64) -> Option<(bool, String)> {
        self.events().into_iter().find_map(|kind| match kind {
            EventKind::CommandFinished {
                command_id,
                succeeded,
                error,
                ..
            } if command_id == CommandId(command) => Some((succeeded, error.unwrap_or_default())),
            _ => None,
        })
    }

    fn steps(&self, command: u64) -> Vec<CommandStep> {
        self.events()
            .into_iter()
            .filter_map(|kind| match kind {
                EventKind::CommandStepped {
                    command_id, step, ..
                } if command_id == CommandId(command) => Some(step),
                _ => None,
            })
            .collect()
    }

    fn typed(&self) -> Vec<String> {
        self.host.typed.lock().expect("typed").clone()
    }
}

/// An input to a busy pane is recorded `waiting` once and left unfinished,
/// with nothing typed, however often it is tried; once the pane reads
/// ready it is typed, once, and succeeds.
#[test]
fn an_input_to_a_busy_pane_waits_then_is_typed_once_the_pane_is_ready() {
    let mut rig = Rig::new(Duration::ZERO);
    rig.pane(BUSY);
    rig.say("human", "note-one", "the note");
    for _ in 0..4 {
        rig.tick();
    }
    assert_eq!(rig.finished(2), None, "it waits, unfinished");
    assert_eq!(rig.steps(2), [CommandStep::Waiting], "recorded once");
    assert!(rig.typed().is_empty(), "nothing typed into a busy pane");

    rig.pane(READY);
    rig.tick();
    assert_eq!(rig.finished(2), Some((true, String::new())));
    assert_eq!(
        rig.steps(2),
        [
            CommandStep::Waiting,
            CommandStep::Typing,
            CommandStep::Typed
        ]
    );
    assert_eq!(rig.typed(), ["the note"]);
    for _ in 0..3 {
        rig.tick();
    }
    assert_eq!(rig.typed(), ["the note"], "typed once");
}

/// Two waiting inputs are typed oldest first, each on a ready reading of
/// its own: the second waits while the first's turn runs, even though the
/// pane read ready right before the first was typed.
#[test]
fn two_waiting_inputs_are_typed_oldest_first_each_on_its_own_ready_reading() {
    let mut rig = Rig::new(Duration::ZERO);
    rig.pane(BUSY);
    rig.say("human", "note-one", "first note");
    rig.say("spawn_supervisor", "note-two", "second note");
    rig.tick();
    rig.tick();
    assert_eq!(rig.steps(2), [CommandStep::Waiting]);
    assert_eq!(rig.steps(3), [CommandStep::Waiting]);

    rig.pane(READY);
    rig.tick();
    assert_eq!(rig.typed(), ["first note"], "the older one first, alone");
    assert_eq!(rig.finished(3), None, "the second waits for an answer");
    // The pane still shows the prompt the first was typed at: that ready
    // reading was consumed by it.
    rig.tick();
    assert_eq!(rig.typed(), ["first note"]);

    rig.turn();
    // Commands come before the screen in a tick: the next tick types it.
    rig.tick();
    assert_eq!(rig.typed(), ["first note", "second note"]);
    assert_eq!(rig.finished(2), Some((true, String::new())));
    assert_eq!(rig.finished(3), Some((true, String::new())));
}

/// The dispatcher's input to a busy pane fails at once as not ready and is
/// never recorded waiting; one sent behind a waiting input is refused, not
/// typed first, though the pane reads ready.
#[test]
fn a_dispatcher_input_is_refused_at_once_and_never_typed_before_a_waiting_one() {
    let mut rig = Rig::new(Duration::from_mins(1));
    rig.pane(BUSY);
    rig.say("spawn-dispatch", "deliver-7-0", "the task");
    rig.tick();
    let (succeeded, error) = rig.finished(2).expect("refused at once");
    assert!(!succeeded);
    assert!(
        error.contains(&format!("pane state Busy {PANE_NOT_READY}")),
        "{error}"
    );
    assert!(rig.steps(2).is_empty(), "never waiting: {:?}", rig.steps(2));

    rig.say("human", "note", "the note");
    rig.tick();
    assert_eq!(rig.steps(3), [CommandStep::Waiting]);
    // The pane is ready, but the human's input waits for its next claim.
    rig.pane(READY);
    rig.say("spawn-dispatch", "deliver-7-0-retry-1", "the task");
    rig.tick();
    let (succeeded, error) = rig.finished(4).expect("refused at once");
    assert!(!succeeded);
    assert!(error.contains(PANE_NOT_READY), "{error}");
    assert!(
        error.contains("cmd_3"),
        "names the input it waits behind: {error}"
    );
    assert!(
        rig.typed().is_empty(),
        "the dispatcher's input was not typed first"
    );

    rig.clock.advance(Duration::from_mins(1));
    rig.tick();
    assert_eq!(rig.typed(), ["the note"]);
}

/// A stop behind a waiting input is claimed and handled at once; the input
/// fails, never typed, saying the session was stopped. A later resume does
/// not revive it.
#[test]
fn a_stop_behind_a_waiting_input_is_handled_and_the_input_fails_as_stopped() {
    let mut rig = Rig::new(Duration::from_mins(1));
    rig.pane(BUSY);
    rig.say("human", "note", "the note");
    rig.tick();
    assert_eq!(rig.steps(2), [CommandStep::Waiting]);
    rig.submit("human", "stop", CommandAction::Stop);
    rig.tick();
    assert_eq!(rig.finished(3), Some((true, String::new())), "the stop");
    let (succeeded, error) = rig.finished(2).expect("failed in the stop's tick");
    assert!(!succeeded);
    assert!(error.contains("stopped"), "{error}");
    assert!(!rig.steps(2).contains(&CommandStep::Typing));
    assert!(rig.typed().is_empty());
}

/// An engine that ends by itself (its terminal lost here) fails what waits
/// for it, with the cause.
#[test]
fn an_execution_that_ends_fails_its_waiting_input_with_the_cause() {
    let mut rig = Rig::new(Duration::from_mins(1));
    rig.pane(BUSY);
    rig.say("human", "note", "the note");
    rig.tick();
    assert_eq!(rig.steps(2), [CommandStep::Waiting]);
    rig.store
        .terminate(&terminal(), &workspace())
        .expect("the terminal goes");
    rig.tick();
    let (succeeded, error) = rig.finished(2).expect("failed in the end's tick");
    assert!(!succeeded);
    assert!(
        error.contains("the session's execution ended (lost) before the input was typed"),
        "{error}"
    );
    assert!(rig.typed().is_empty());
}

/// A restarted Bridge reads the waiting from the log: it keeps waiting and
/// types the input once the pane is ready, once.
#[test]
fn a_bridge_rebuilt_from_the_log_keeps_waiting_and_types_once() {
    let mut rig = Rig::new(Duration::ZERO);
    rig.pane(BUSY);
    rig.say("human", "note", "the note");
    rig.tick();
    assert_eq!(rig.steps(2), [CommandStep::Waiting]);
    rig.restart();
    rig.tick();
    assert_eq!(rig.finished(2), None, "still waiting after the restart");
    assert_eq!(rig.steps(2), [CommandStep::Waiting], "not recorded again");
    rig.pane(READY);
    rig.tick();
    rig.tick();
    assert_eq!(rig.finished(2), Some((true, String::new())));
    assert_eq!(rig.typed(), ["the note"]);
}

/// A waiting input whose typing began and was never recorded done (the
/// Bridge died mid-typing) is never typed again: it fails as one that may
/// have been typed.
#[test]
fn a_waiting_input_with_typing_and_no_typed_fails_as_may_have_typed() {
    let mut rig = Rig::new(Duration::ZERO);
    rig.pane(BUSY);
    rig.say("human", "note", "the note");
    rig.tick();
    rig.store
        .append(NewEvent {
            agent_id: AgentId::new("agent_one").expect("valid agent"),
            session_id: SessionId::new("session_one").expect("valid session"),
            execution_id: Some(ExecutionId::for_command(CommandId(1))),
            source: Source::Bridge,
            source_event_id: Some("cmd_2:typing".to_string()),
            kind: EventKind::CommandStepped {
                command_id: CommandId(2),
                step: CommandStep::Typing,
                workspace: None,
            },
        })
        .expect("the typing a dead Bridge recorded");
    rig.restart();
    rig.pane(READY);
    rig.tick();
    let (succeeded, error) = rig.finished(2).expect("failed");
    assert!(!succeeded);
    assert!(error.contains(MAY_HAVE_TYPED), "{error}");
    assert!(rig.typed().is_empty(), "never typed again");
}
