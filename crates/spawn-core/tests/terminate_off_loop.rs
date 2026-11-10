//! An end's kill runs off the Bridge's loop when the host's kill waits for
//! the engine's processes (backlog P7: a stop held the serial loop up to
//! 7 s). The tick goes on while the kill waits; the end, and the stop that
//! asked for it, finish at a later tick once it is done; each kill's wait
//! is in the tick's notes. A host whose kill returns at once (in memory)
//! ends in the same tick, as before.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use spawn_core::bridge::{Bridge, BridgeConfig, TickReport};
use spawn_core::command::{CommandAction, Submission};
use spawn_core::event::{ENGINE, EventKind, GlobalPosition};
use spawn_core::id::{
    AgentId, CommandId, ExecutionId, IdempotencyKey, Principal, SessionId, TerminalName,
    WorkspaceId,
};
use spawn_core::memory::MemoryStore;
use spawn_core::port::{CommandQueue, EventLog, Launch, TerminalError, TerminalHost};
use spawn_core::time::{ManualClock, Timestamp};

const READY: &str = "❯ \n  deepseek-ai/DeepSeek-V4.1-Flash · 余量未知 · 249 tok/s";
const BUSY: &str = "✻ Churning… (esc to interrupt)\n❯ ";

/// What the held kill answers once released.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Release {
    Held,
    Gone,
    Outlived,
}

/// The memory terminal whose kill waits until the test releases it, as a
/// real host's waits for the session's processes.
struct Slow {
    inner: Arc<MemoryStore>,
    gate: Mutex<Release>,
    moved: Condvar,
    kills: Mutex<usize>,
    /// Texts staged into a pane (the first half of typing an input).
    staged: Mutex<usize>,
}

impl Slow {
    fn release(&self, release: Release) {
        *self.gate.lock().expect("gate") = release;
        self.moved.notify_all();
    }
}

impl TerminalHost for Slow {
    fn start(&self, launch: &Launch) -> Result<(), TerminalError> {
        self.inner.start(launch)
    }
    fn stage_text(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
        text: &str,
    ) -> Result<(), TerminalError> {
        *self.staged.lock().expect("staged") += 1;
        self.inner.stage_text(terminal, workspace, text)
    }
    fn submit_staged(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
    ) -> Result<(), TerminalError> {
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
    fn runner(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
    ) -> Result<spawn_core::port::Runner, TerminalError> {
        self.inner.runner(terminal, workspace)
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
        *self.kills.lock().expect("kills") += 1;
        let gate = self.gate.lock().expect("gate");
        let (gate, _) = self
            .moved
            .wait_timeout_while(gate, Duration::from_secs(20), |release| {
                *release == Release::Held
            })
            .expect("gate");
        let release = *gate;
        drop(gate);
        match release {
            Release::Outlived => Err(TerminalError("a process outlived the kill".to_string())),
            Release::Gone | Release::Held => self.inner.terminate(terminal, workspace),
        }
    }
    fn terminate_waits(&self) -> bool {
        true
    }
}

struct Rig {
    store: Arc<MemoryStore>,
    clock: Arc<ManualClock>,
    host: Arc<Slow>,
    bridge: Bridge,
    notes: Vec<String>,
}

fn terminal() -> TerminalName {
    TerminalName::for_execution(&ExecutionId::for_command(CommandId(1)))
}

impl Rig {
    /// A live engine at its prompt (started by command 1); commands left
    /// waiting are claimed again at once.
    fn new() -> Self {
        Self::with_retry(Duration::ZERO)
    }

    /// As [`Rig::new`], commands left waiting claimed again after `retry`
    /// of the rig's clock.
    fn with_retry(retry: Duration) -> Self {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        store.set_pane(&terminal(), READY);
        let host = Arc::new(Slow {
            inner: store.clone(),
            gate: Mutex::new(Release::Held),
            moved: Condvar::new(),
            kills: Mutex::new(0),
            staged: Mutex::new(0),
        });
        let mut config = BridgeConfig::new("tester");
        config.ready_interval = Duration::ZERO;
        config.input_wait_retry = retry;
        let bridge = Bridge::new(
            store.clone(),
            store.clone(),
            store.clone(),
            host.clone(),
            config,
        )
        .with_clock(clock.clone());
        let mut rig = Self {
            store,
            clock,
            host,
            bridge,
            notes: Vec::new(),
        };
        rig.submit(
            "key_start",
            CommandAction::Start {
                engine: ENGINE.to_string(),
                prompt: String::new(),
                parent_session_id: None,
                from: None,
                model: None,
                charter: None,
            },
        );
        rig.tick();
        assert!(rig.finished(1).is_some_and(|(ok, _)| ok), "the start");
        rig
    }

    fn submit(&self, key: &str, action: CommandAction) {
        self.store
            .submit(
                &Principal::new("human").expect("valid principal"),
                &Submission {
                    agent_id: AgentId::new("agent_one").expect("valid agent"),
                    session_id: SessionId::new("session_one").expect("valid session"),
                    idempotency_key: IdempotencyKey::new(key).expect("valid key"),
                    action,
                },
            )
            .expect("submit");
    }

    /// One tick, and how long it held the loop.
    fn tick(&mut self) -> Duration {
        let started = Instant::now();
        let report: TickReport = self.bridge.tick().expect("tick");
        self.notes.extend(report.notes);
        started.elapsed()
    }

    /// Ticks until `done` holds, at most 5 s.
    fn tick_until(&mut self, done: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done(self) {
            assert!(Instant::now() < deadline, "not done within 5 s");
            self.tick();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn events(&self) -> Vec<EventKind> {
        self.store
            .read_global(GlobalPosition(0), 10_000)
            .expect("read")
            .into_iter()
            .map(|record| record.event.kind)
            .collect()
    }

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

    fn ended(&self) -> bool {
        self.events()
            .iter()
            .any(|kind| matches!(kind, EventKind::ExecutionEnded { .. }))
    }

    fn kills(&self) -> usize {
        *self.host.kills.lock().expect("kills")
    }
}

/// A stop whose kill waits: the tick returns at once, again and again,
/// with one kill under way and nothing ended or finished; released, the
/// end is recorded and the stop succeeds, and the kill's wait is noted.
#[test]
fn a_stop_whose_kill_waits_does_not_hold_the_loop() {
    let mut rig = Rig::new();
    rig.submit("key_stop", CommandAction::Stop);
    for _ in 0..5 {
        let held = rig.tick();
        assert!(
            held < Duration::from_secs(1),
            "the tick held the loop {held:?}"
        );
    }
    assert_eq!(
        rig.kills(),
        1,
        "one kill, read again by each tick, never started twice"
    );
    assert!(!rig.ended(), "nothing ends before the kill is done");
    assert_eq!(rig.finished(2), None, "the stop waits for it, unfinished");
    assert!(rig.notes.is_empty(), "{:?}", rig.notes);

    std::thread::sleep(Duration::from_millis(150));
    rig.host.release(Release::Gone);
    rig.tick_until(|rig| rig.finished(2).is_some());
    assert!(rig.ended(), "the end is recorded");
    assert_eq!(
        rig.finished(2),
        Some((true, String::new())),
        "the stop succeeds"
    );
    assert_eq!(rig.kills(), 1);
    assert_eq!(rig.notes.len(), 1, "{:?}", rig.notes);
    let note = &rig.notes[0];
    assert!(
        note.starts_with(&format!("session_one: the kill of {} waited ", terminal())),
        "{note}"
    );
    let millis: u64 = note
        .split("waited ")
        .nth(1)
        .and_then(|rest| rest.split(" ms").next())
        .and_then(|text| text.parse().ok())
        .expect("the wait in ms");
    assert!(millis >= 150, "the wait is the kill's own: {note}");
}

/// A kill that fails after its wait (a process outlived it) is the end's
/// failure, as before: the stop fails with why, and the execution stays.
#[test]
fn a_kill_that_fails_off_the_loop_fails_the_stop_as_before() {
    let mut rig = Rig::new();
    rig.submit("key_stop", CommandAction::Stop);
    rig.tick();
    assert_eq!(rig.finished(2), None);
    rig.host.release(Release::Outlived);
    rig.tick_until(|rig| rig.finished(2).is_some());
    let (succeeded, error) = rig.finished(2).expect("finished");
    assert!(!succeeded);
    assert!(error.contains("a process outlived the kill"), "{error}");
    assert!(!rig.ended(), "the execution stays, its workspace mounted");
    assert!(
        rig.notes.iter().any(|note| note.ends_with(", and failed")),
        "{:?}",
        rig.notes
    );
}

/// An input waiting for its turn when the stop came is failed once the end
/// is done, never typed; the stop itself is not failed with it.
#[test]
fn an_input_waiting_behind_a_pending_kill_fails_and_the_stop_succeeds() {
    let mut rig = Rig::new();
    rig.store.set_pane(&terminal(), BUSY);
    rig.submit(
        "key_note",
        CommandAction::Input {
            text: "the note".to_string(),
            tags: Vec::new(),
        },
    );
    rig.tick();
    rig.tick();
    assert_eq!(rig.finished(2), None, "the input waits for a turn");
    rig.submit("key_stop", CommandAction::Stop);
    rig.tick();
    rig.tick();
    assert_eq!(rig.finished(3), None, "the stop waits for its kill");
    rig.host.release(Release::Gone);
    rig.tick_until(|rig| rig.finished(2).is_some() && rig.finished(3).is_some());
    let (input_ok, why) = rig.finished(2).expect("the input finished");
    assert!(!input_ok, "the input was never typed: {why}");
    assert_eq!(
        rig.finished(3),
        Some((true, String::new())),
        "the stop succeeds"
    );
}

/// The end done by the tick's own pass while the stop waits for its next
/// claim: the stop is not failed with the waiting inputs; claimed again, it
/// finds nothing running and succeeds.
#[test]
fn a_stop_waiting_for_its_claim_is_not_failed_by_the_end() {
    let mut rig = Rig::with_retry(Duration::from_secs(1));
    rig.submit("key_stop", CommandAction::Stop);
    rig.tick();
    assert_eq!(rig.finished(2), None);
    rig.host.release(Release::Gone);
    // The clock stands still: the stop is not claimed again, and the end is
    // the tick's own pass's.
    rig.tick_until(Rig::ended);
    assert_eq!(
        rig.finished(2),
        None,
        "the end does not fail the waiting stop"
    );
    rig.clock.advance(Duration::from_secs(2));
    rig.tick_until(|rig| rig.finished(2).is_some());
    assert_eq!(
        rig.finished(2),
        Some((true, String::new())),
        "the stop succeeds"
    );
}

/// While an end's kill waits, an input to the session waits too, never
/// typed into the terminal being killed; once the end is done it is failed,
/// never typed.
#[test]
fn an_input_sent_while_the_kill_waits_is_never_typed() {
    let mut rig = Rig::new();
    // The tick's own pass finds the runner gone and ends the execution.
    rig.store
        .untrack(&terminal(), "a shell where the runner was");
    rig.tick();
    assert_eq!(rig.kills(), 1, "the end's kill is under way");
    rig.submit(
        "key_note",
        CommandAction::Input {
            text: "the note".to_string(),
            tags: Vec::new(),
        },
    );
    for _ in 0..3 {
        rig.tick();
    }
    assert_eq!(rig.finished(2), None, "the input waits for the end");
    assert_eq!(*rig.host.staged.lock().expect("staged"), 0, "nothing typed");
    rig.host.release(Release::Gone);
    rig.tick_until(|rig| rig.finished(2).is_some());
    assert!(rig.ended());
    let (typed, why) = rig.finished(2).expect("finished");
    assert!(
        !typed,
        "an input to an ended execution is never typed: {why}"
    );
    assert_eq!(rig.kills(), 1);
}

/// A start sent right after a stop whose kill waits is not refused as
/// "still runs an execution": it waits, and starts once the end is done.
#[test]
fn a_start_after_a_stop_waits_for_the_end_and_goes_on() {
    let mut rig = Rig::new();
    rig.submit("key_stop", CommandAction::Stop);
    rig.tick();
    rig.submit(
        "key_start_again",
        CommandAction::Start {
            engine: ENGINE.to_string(),
            prompt: String::new(),
            parent_session_id: None,
            from: None,
            model: None,
            charter: None,
        },
    );
    for _ in 0..3 {
        rig.tick();
    }
    assert_eq!(rig.finished(3), None, "the start waits for the end");
    rig.host.release(Release::Gone);
    rig.tick_until(|rig| rig.finished(3).is_some());
    assert_eq!(rig.finished(2), Some((true, String::new())), "the stop");
    assert_eq!(
        rig.finished(3),
        Some((true, String::new())),
        "the start, after it"
    );
}

/// The queue's view: the stop that waited is not left claimed or failed.
#[test]
fn the_stop_that_waited_leaves_nothing_in_the_queue() {
    let mut rig = Rig::new();
    rig.submit("key_stop", CommandAction::Stop);
    rig.tick();
    rig.host.release(Release::Gone);
    rig.tick_until(|rig| rig.finished(2).is_some());
    rig.tick();
    let claimed = rig
        .store
        .claim("someone", Duration::from_secs(30))
        .expect("claim");
    assert!(claimed.is_none(), "nothing left to claim");
}

/// A Bridge that goes away while a kill waits (the loop's clean exit on
/// SIGTERM, a Bridge built again after a store outage) waits for that kill
/// before it is gone: a kill abandoned mid-wait is never waited for again,
/// since the host reads the session's processes once, before it kills the
/// session (rev-e on 471e29d8).
#[test]
fn a_bridge_dropped_while_its_kill_waits_waits_for_it() {
    let mut rig = Rig::new();
    rig.submit("key_stop", CommandAction::Stop);
    rig.tick();
    assert_eq!(rig.kills(), 1, "the kill is under way");
    assert!(!rig.ended());
    let Rig { bridge, host, .. } = rig;
    let dropped = std::thread::spawn(move || {
        drop(bridge);
        Instant::now()
    });
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !dropped.is_finished(),
        "the drop returned while the kill still waited"
    );
    let released = Instant::now();
    host.release(Release::Gone);
    let gone = dropped.join().expect("the drop");
    assert!(gone >= released, "the drop returned before the release");
    assert!(
        gone.duration_since(released) < Duration::from_secs(2),
        "and soon after it"
    );
}

/// `ends_pending` says whether a later tick still has an end to finish: a
/// kill under way, and after it a stop that waits to be claimed again with
/// its end already done by a tick's own pass. A one-shot `spawn tick` ticks
/// until it is false, so a stop it claimed is finished when it returns
/// (003 under load, rev-f on 283a5482).
#[test]
fn ends_pending_holds_until_the_waiting_stop_is_finished() {
    let mut rig = Rig::with_retry(Duration::from_secs(1));
    assert!(!rig.bridge.ends_pending(), "nothing ends yet");
    rig.submit("key_stop", CommandAction::Stop);
    rig.tick();
    assert!(rig.bridge.ends_pending(), "the kill is under way");
    rig.host.release(Release::Gone);
    rig.tick_until(Rig::ended);
    assert_eq!(rig.finished(2), None, "the stop still waits for its claim");
    assert!(
        rig.bridge.ends_pending(),
        "the end is done, the stop that asked for it is not"
    );
    rig.clock.advance(Duration::from_secs(2));
    rig.tick_until(|rig| rig.finished(2).is_some());
    assert_eq!(rig.finished(2), Some((true, String::new())));
    assert!(!rig.bridge.ends_pending(), "nothing is left to finish");
}
