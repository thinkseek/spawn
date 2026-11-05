//! A transient terminal error is not a terminal outcome (design §14): one
//! refused or timed-out capture, or a staging refused before any key went,
//! spends an attempt and is tried again; only a terminal that `has-session`
//! says is gone fails a command at once. A refusal before typing is
//! recorded as nothing typed, never as "may have typed".

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spawn_core::bridge::{Bridge, BridgeConfig, MAY_HAVE_TYPED, PANE_NOT_READY};
use spawn_core::command::{CommandAction, Submission};
use spawn_core::event::{CommandStep, ENGINE, EventKind, GlobalPosition};
use spawn_core::id::{
    AgentId, CommandId, ExecutionId, IdempotencyKey, Principal, SessionId, TerminalName,
    WorkspaceId,
};
use spawn_core::memory::MemoryStore;
use spawn_core::port::{CommandQueue, EventLog, Launch, TerminalError, TerminalHost};
use spawn_core::time::{ManualClock, Timestamp};

const READY: &str = "❯ \n  deepseek-ai/DeepSeek-V4.1-Flash · 余量未知 · 249 tok/s";

/// The memory terminal, with host calls that are refused a set number of
/// times (tmux busy, a capture past its time limit), and one that can die
/// at its next capture.
struct Flaky {
    inner: Arc<MemoryStore>,
    /// Captures still to refuse.
    refuse_captures: AtomicUsize,
    /// Stagings still to refuse (`set-buffer` refused: no key went).
    refuse_stages: AtomicUsize,
    /// The terminal dies at the next capture.
    die: AtomicBool,
    captures: AtomicUsize,
    typed: Mutex<Vec<String>>,
    staged: Mutex<Option<String>>,
}

impl Flaky {
    fn take(counter: &AtomicUsize) -> bool {
        counter
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
    }
}

impl TerminalHost for Flaky {
    fn start(&self, launch: &Launch) -> Result<(), TerminalError> {
        self.inner.start(launch)
    }

    fn stage_text(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
        text: &str,
    ) -> Result<(), TerminalError> {
        if Self::take(&self.refuse_stages) {
            return Err(TerminalError("set-buffer failed: refused".to_string()));
        }
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
        self.captures.fetch_add(1, Ordering::SeqCst);
        if self.die.swap(false, Ordering::SeqCst) {
            self.inner.terminate(terminal, workspace)?;
        }
        if Self::take(&self.refuse_captures) {
            return Err(TerminalError("capture-pane failed: timed out".to_string()));
        }
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
    host: Arc<Flaky>,
    bridge: Bridge,
}

fn terminal() -> TerminalName {
    TerminalName::for_execution(&ExecutionId::for_command(CommandId(1)))
}

fn workspace() -> WorkspaceId {
    WorkspaceId::new("1:/workspaces/1").expect("valid workspace")
}

impl Rig {
    fn new() -> Self {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        store.set_pane(&terminal(), READY);
        let host = Arc::new(Flaky {
            inner: store.clone(),
            refuse_captures: AtomicUsize::new(0),
            refuse_stages: AtomicUsize::new(0),
            die: AtomicBool::new(false),
            captures: AtomicUsize::new(0),
            typed: Mutex::default(),
            staged: Mutex::default(),
        });
        let mut config = BridgeConfig::new("tester");
        config.ready_attempts = 40;
        config.ready_interval = Duration::ZERO;
        let bridge = Bridge::new(
            store.clone(),
            store.clone(),
            store.clone(),
            host.clone(),
            config,
        )
        .with_clock(clock);
        Self {
            store,
            host,
            bridge,
        }
    }

    fn submit(&self, key: &str, action: CommandAction) {
        self.store
            .submit(
                &Principal::new("tester").expect("valid principal"),
                &Submission {
                    agent_id: AgentId::new("agent_one").expect("valid agent"),
                    session_id: SessionId::new("session_one").expect("valid session"),
                    idempotency_key: IdempotencyKey::new(key).expect("valid key"),
                    action,
                },
            )
            .expect("submit");
    }

    fn start(&mut self, prompt: &str) {
        self.submit(
            "key_start",
            CommandAction::Start {
                engine: ENGINE.to_string(),
                prompt: prompt.to_string(),
                parent_session_id: None,
                from: None,
                model: None,
            },
        );
        self.bridge.tick().expect("tick");
    }

    fn input(&mut self, key: &str, text: &str) {
        self.submit(
            key,
            CommandAction::Input {
                text: text.to_string(),
                tags: Vec::new(),
            },
        );
        self.bridge.tick().expect("tick");
    }

    fn events(&self) -> Vec<EventKind> {
        self.store
            .read_global(GlobalPosition(0), 1_000)
            .expect("read")
            .into_iter()
            .map(|record| record.event.kind)
            .collect()
    }

    /// `(succeeded, error)` of the command's finish.
    fn finished(&self, command: u64) -> (bool, String) {
        self.events()
            .into_iter()
            .find_map(|kind| match kind {
                EventKind::CommandFinished {
                    command_id,
                    succeeded,
                    error,
                    ..
                } if command_id == CommandId(command) => {
                    Some((succeeded, error.unwrap_or_default()))
                }
                _ => None,
            })
            .expect("the command finished")
    }

    /// The recorded steps of the command.
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

    fn typed(&self, text: &str) -> usize {
        self.host
            .typed
            .lock()
            .expect("typed")
            .iter()
            .filter(|typed| typed.contains(text))
            .count()
    }

    fn ended(&self) -> bool {
        self.events()
            .iter()
            .any(|kind| matches!(kind, EventKind::ExecutionEnded { .. }))
    }
}

/// One capture refused during a start's ready-wait spends an attempt; the
/// next one reads the prompt, and the prompt is typed.
#[test]
fn a_start_survives_one_refused_capture() {
    let mut rig = Rig::new();
    rig.host.refuse_captures.store(1, Ordering::SeqCst);
    rig.start("do the task");
    let (succeeded, error) = rig.finished(1);
    assert!(succeeded, "the start failed: {error}");
    assert_eq!(rig.typed("do the task"), 1);
    assert!(!rig.ended(), "the execution was abandoned");
}

/// A staging refused before any key went types nothing and records
/// nothing: the start tries again within its budget.
#[test]
fn a_start_survives_one_refused_staging() {
    let mut rig = Rig::new();
    rig.host.refuse_stages.store(1, Ordering::SeqCst);
    rig.start("do the task");
    let (succeeded, error) = rig.finished(1);
    assert!(succeeded, "the start failed: {error}");
    assert_eq!(rig.typed("do the task"), 1);
    assert_eq!(
        rig.steps(1)
            .iter()
            .filter(|step| **step == CommandStep::Typing)
            .count(),
        1
    );
    assert!(!rig.ended(), "the execution was abandoned");
}

/// One capture refused before an input is read again; the input is typed.
#[test]
fn an_input_survives_one_refused_capture() {
    let mut rig = Rig::new();
    rig.start("");
    rig.host.refuse_captures.store(1, Ordering::SeqCst);
    rig.input("key_input", "delete the old table");
    let (succeeded, error) = rig.finished(2);
    assert!(succeeded, "the input failed: {error}");
    assert_eq!(rig.typed("delete the old table"), 1);
}

/// One staging refused before an input's keys went: nothing was typed and
/// the log says so (no `typing` step), so it is staged again and typed.
#[test]
fn an_input_survives_one_refused_staging() {
    let mut rig = Rig::new();
    rig.start("");
    rig.host.refuse_stages.store(1, Ordering::SeqCst);
    rig.input("key_input", "delete the old table");
    let (succeeded, error) = rig.finished(2);
    assert!(succeeded, "the input failed: {error}");
    assert_eq!(rig.typed("delete the old table"), 1);
    assert_eq!(rig.steps(2), [CommandStep::Typing, CommandStep::Typed]);
}

/// A terminal that keeps refusing the staging, while it is still there:
/// the input is refused as not ready, never as "may have typed", with no
/// `typing` step; a later input goes in.
#[test]
fn an_input_refused_before_typing_is_not_ready_and_says_nothing_typed() {
    let mut rig = Rig::new();
    rig.start("");
    rig.host.refuse_stages.store(100, Ordering::SeqCst);
    rig.input("key_input", "delete the old table");
    let (succeeded, error) = rig.finished(2);
    assert!(!succeeded);
    assert!(error.contains(PANE_NOT_READY), "{error}");
    assert!(!error.contains(MAY_HAVE_TYPED), "{error}");
    assert!(rig.steps(2).is_empty(), "{:?}", rig.steps(2));
    assert_eq!(rig.typed("delete the old table"), 0);
    rig.host.refuse_stages.store(0, Ordering::SeqCst);
    rig.input("key_again", "delete the old table");
    let (succeeded, error) = rig.finished(3);
    assert!(succeeded, "the second input failed: {error}");
    assert_eq!(rig.typed("delete the old table"), 1);
}

/// Captures that keep failing on a terminal still there: refused as not
/// ready, which the dispatcher tries again.
#[test]
fn an_input_whose_captures_keep_failing_is_not_ready() {
    let mut rig = Rig::new();
    rig.start("");
    rig.host.refuse_captures.store(100, Ordering::SeqCst);
    rig.input("key_input", "delete the old table");
    let (succeeded, error) = rig.finished(2);
    assert!(!succeeded);
    assert!(error.contains(PANE_NOT_READY), "{error}");
    assert!(!error.contains("terminal disappeared"), "{error}");
    assert!(rig.steps(2).is_empty());
}

/// A terminal that is really gone fails a start at once (one capture, no
/// wait through the budget), and the execution is abandoned.
#[test]
fn a_start_whose_terminal_is_gone_fails_at_once() {
    let mut rig = Rig::new();
    rig.host.die.store(true, Ordering::SeqCst);
    rig.start("do the task");
    let (succeeded, error) = rig.finished(1);
    assert!(!succeeded);
    assert!(error.contains("terminal disappeared"), "{error}");
    assert_eq!(rig.host.captures.load(Ordering::SeqCst), 1);
    assert!(rig.ended(), "the abandoned execution ended");
    assert_eq!(rig.typed("do the task"), 0);
}

/// An input into a terminal that is gone fails at once, as a lost
/// terminal, not as not ready.
#[test]
fn an_input_whose_terminal_is_gone_fails_at_once() {
    let mut rig = Rig::new();
    rig.start("");
    rig.host
        .inner
        .terminate(&terminal(), &workspace())
        .expect("kill");
    let before = rig.host.captures.load(Ordering::SeqCst);
    rig.input("key_input", "delete the old table");
    let (succeeded, error) = rig.finished(2);
    assert!(!succeeded);
    assert!(error.contains("terminal disappeared"), "{error}");
    assert!(!error.contains(PANE_NOT_READY), "{error}");
    assert_eq!(rig.host.captures.load(Ordering::SeqCst) - before, 1);
}
