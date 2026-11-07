//! The audit findings about the execution lifecycle and its crash windows
//! (PR #2): each test states the behaviour the audit asks for. The matrix
//! tests fail every append of a handler in turn, and its finish, with a
//! lease expiry and optionally a restart after.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spawn_core::bridge::{Bridge, BridgeConfig, claude_session_id};
use spawn_core::command::{Claimed, CommandAction, Outcome, Submission};
use spawn_core::event::{
    Appended, CommandStep, ENGINE, EventKind, EventRecord, GlobalPosition, NewEvent, Source,
};
use spawn_core::id::{
    AgentId, CommandId, ExecutionId, IdempotencyKey, Principal, SessionId, TerminalName,
    WorkspaceId,
};
use spawn_core::memory::MemoryStore;
use spawn_core::port::{
    BaseMerge, CommandQueue, EventLog, Launch, StoreError, TerminalError, TerminalHost,
};
use spawn_core::time::{ManualClock, Timestamp};

const READY: &str = "❯ \n  deepseek-ai/DeepSeek-V4.1-Flash · 余量未知 · 249 tok/s";
const BUSY: &str = "✽ Effecting… (27m 12s · ↓ 92.3k tokens)\n❯ \n  deepseek-ai/DeepSeek-V4.1-Flash · 余量未知 · 249 tok/s";
const AT: &str = r#""timestamp":"2099-01-01T00:00:00.000Z""#;

type Rule = Box<dyn Fn(&NewEvent) -> bool + Send>;

/// The memory store, with appends that fail once when a rule matches (a
/// database error at that moment), finishes that fail once, and a record
/// of every text typed into a terminal.
struct Rig {
    store: Arc<MemoryStore>,
    clock: Arc<ManualClock>,
    failing_appends: Mutex<Vec<Rule>>,
    failing_finishes: Mutex<usize>,
    typed: Mutex<Vec<String>>,
    staged: Mutex<Option<String>>,
    outcomes: Mutex<Vec<(CommandId, Outcome)>>,
}

impl Rig {
    fn new() -> Arc<Self> {
        let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock.clone()));
        for command in 1..=4 {
            store.set_pane(&terminal(command), READY);
        }
        Arc::new(Self {
            store,
            clock,
            failing_appends: Mutex::default(),
            failing_finishes: Mutex::default(),
            typed: Mutex::default(),
            staged: Mutex::default(),
            outcomes: Mutex::default(),
        })
    }

    fn bridge(self: &Arc<Self>) -> Bridge {
        Bridge::new(
            self.clone(),
            self.clone(),
            self.store.clone(),
            self.clone(),
            BridgeConfig::new("tester"),
        )
        .with_clock(self.clock.clone())
    }

    /// A Bridge process that starts over from the log, as after a crash.
    fn restarted(self: &Arc<Self>) -> Bridge {
        let mut bridge = self.bridge();
        bridge.recover().expect("recover");
        bridge
    }

    fn fail_append_once(&self, rule: impl Fn(&NewEvent) -> bool + Send + 'static) {
        self.failing_appends
            .lock()
            .expect("rules")
            .push(Box::new(rule));
    }

    fn fail_finish_once(&self) {
        *self.failing_finishes.lock().expect("finishes") += 1;
    }

    fn typed(&self, text: &str) -> usize {
        self.typed
            .lock()
            .expect("typed")
            .iter()
            .filter(|typed| typed.contains(text))
            .count()
    }

    /// Past the two-minute lease of a command whose tick died.
    fn lease_expires(&self) {
        self.clock.advance(Duration::from_mins(3));
    }

    fn events(&self) -> Vec<NewEvent> {
        self.store
            .read_global(GlobalPosition(0), 1_000)
            .expect("read")
            .into_iter()
            .map(|record| record.event)
            .collect()
    }

    fn count(&self, name: &str) -> usize {
        self.events()
            .iter()
            .filter(|event| event.kind.name() == name)
            .count()
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

    fn start(&self, prompt: &str) {
        self.submit(
            "key_start",
            CommandAction::Start {
                engine: ENGINE.to_string(),
                prompt: prompt.to_string(),
                parent_session_id: None,
                from: None,
                model: None,
                charter: None,
            },
        );
    }

    /// Lines the first execution's engine writes to its transcript.
    fn transcript(&self, lines: &[String]) {
        let path = spawn_core::bridge::transcript_path(
            &BridgeConfig::new("tester").claude_projects,
            &WorkspaceId::new("1:/workspaces/1").expect("valid workspace"),
            &claude_session_id(&ExecutionId::for_command(CommandId(1))),
        );
        self.store.append_file(&path, &(lines.join("\n") + "\n"));
    }

    /// Fail the `n`th append from now (counting from 0), once. The returned
    /// flag tells whether it came.
    fn fail_nth_append(&self, n: usize) -> Arc<AtomicUsize> {
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = seen.clone();
        self.fail_append_once(move |_| counter.fetch_add(1, Ordering::SeqCst) == n);
        seen
    }

    /// Drop a failure rule that never matched.
    fn clear_failures(&self) {
        self.failing_appends.lock().expect("rules").clear();
        *self.failing_finishes.lock().expect("finishes") = 0;
    }

    fn outcome(&self, command: u64) -> Option<Outcome> {
        self.outcomes
            .lock()
            .expect("outcomes")
            .iter()
            .rev()
            .find(|(id, _)| *id == CommandId(command))
            .map(|(_, outcome)| outcome.clone())
    }

    /// Mounts not unmounted again.
    fn mounted(&self) -> usize {
        let calls = self.store.workspace_calls();
        let mounts = calls
            .iter()
            .filter(|call| call.starts_with("mount "))
            .count();
        let unmounts = calls.iter().filter(|call| *call == "unmount").count();
        mounts.saturating_sub(unmounts)
    }

    fn merges(&self) -> usize {
        self.store
            .workspace_calls()
            .iter()
            .filter(|call| call.starts_with("merge"))
            .count()
    }
}

fn terminal(command: u64) -> TerminalName {
    TerminalName::for_execution(&ExecutionId::for_command(CommandId(command)))
}

fn turn_end(id: &str) -> String {
    format!(r#"{{"type":"system","subtype":"turn_duration","uuid":"t-{id}",{AT}}}"#)
}

fn prompt(id: &str) -> String {
    format!(r#"{{"type":"user","uuid":"p-{id}",{AT},"message":{{"content":"next"}}}}"#)
}

impl EventLog for Rig {
    fn append(&self, event: NewEvent) -> Result<Appended, StoreError> {
        let mut rules = self.failing_appends.lock().expect("rules");
        if let Some(index) = rules.iter().position(|rule| rule(&event)) {
            let _ = rules.remove(index);
            return Err(StoreError::Backend("the database is down".to_string()));
        }
        drop(rules);
        self.store.append(event)
    }

    fn read_global(
        &self,
        after: GlobalPosition,
        limit: usize,
    ) -> Result<Vec<EventRecord>, StoreError> {
        self.store.read_global(after, limit)
    }
}

impl CommandQueue for Rig {
    fn submit(
        &self,
        principal: &Principal,
        submission: &Submission,
    ) -> Result<CommandId, StoreError> {
        self.store.submit(principal, submission)
    }

    fn claim(&self, worker: &str, ttl: Duration) -> Result<Option<Claimed>, StoreError> {
        self.store.claim(worker, ttl)
    }

    fn finish(&self, claimed: &Claimed, outcome: &Outcome) -> Result<(), StoreError> {
        let mut failing = self.failing_finishes.lock().expect("finishes");
        if *failing > 0 {
            *failing -= 1;
            return Err(StoreError::Backend("the database is down".to_string()));
        }
        drop(failing);
        self.store.finish(claimed, outcome)?;
        self.outcomes
            .lock()
            .expect("outcomes")
            .push((claimed.command_id, outcome.clone()));
        Ok(())
    }
}

impl TerminalHost for Rig {
    fn start(&self, launch: &Launch) -> Result<(), TerminalError> {
        self.store.start(launch)
    }

    fn stage_text(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
        text: &str,
    ) -> Result<(), TerminalError> {
        *self.staged.lock().expect("staged") = Some(text.to_string());
        self.store.stage_text(terminal, workspace, text)
    }

    fn submit_staged(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
    ) -> Result<(), TerminalError> {
        if let Some(text) = self.staged.lock().expect("staged").take() {
            self.typed.lock().expect("typed").push(text);
        }
        self.store.submit_staged(terminal, workspace)
    }

    fn send_keys(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
        keys: &[&str],
    ) -> Result<(), TerminalError> {
        self.store.send_keys(terminal, workspace, keys)
    }

    fn is_alive(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
    ) -> Result<bool, TerminalError> {
        self.store.is_alive(terminal, workspace)
    }

    fn capture_tail(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
        lines: usize,
    ) -> Result<String, TerminalError> {
        self.store.capture_tail(terminal, workspace, lines)
    }

    fn read_from(
        &self,
        workspace: &WorkspaceId,
        path: &str,
        offset: u64,
    ) -> Result<Option<Vec<u8>>, TerminalError> {
        self.store.read_from(workspace, path, offset)
    }

    fn write_new(
        &self,
        workspace: &WorkspaceId,
        path: &str,
        bytes: &[u8],
    ) -> Result<bool, TerminalError> {
        self.store.write_new(workspace, path, bytes)
    }

    fn terminate(
        &self,
        terminal: &TerminalName,
        workspace: &WorkspaceId,
    ) -> Result<(), TerminalError> {
        self.store.terminate(terminal, workspace)
    }
}

/// M5: the ready-screen append fails after the engine was launched and
/// before the prompt is typed. The retried Start finds the execution and
/// fails; the engine is left live at an empty prompt, with no end coming.
#[test]
fn m5_a_store_error_before_the_start_prompt_leaves_a_promptless_engine() {
    let rig = Rig::new();
    let mut bridge = rig.bridge();
    rig.start("do the task");
    rig.fail_append_once(|event| {
        event
            .source_event_id
            .as_deref()
            .is_some_and(|key| key.ends_with(":screen:ready"))
    });
    assert!(bridge.tick().is_err(), "the database is down");
    rig.lease_expires();
    for _ in 0..3 {
        let _ = bridge.tick();
    }
    let ended = rig.count("execution.ended") > 0;
    assert!(
        rig.typed("do the task") == 1 || ended,
        "the engine got its prompt ({} times) or was ended ({ended})",
        rig.typed("do the task")
    );
}

/// M6: an Input whose command is not finished (the tick died after the
/// paste, or a host call outlived the lease) is claimed again and typed
/// again once the pane reads ready.
#[test]
fn m6_an_input_whose_lease_expires_is_typed_twice() {
    let rig = Rig::new();
    let mut bridge = rig.bridge();
    rig.start("");
    bridge.tick().expect("start");
    rig.submit(
        "key_input",
        CommandAction::Input {
            text: "delete the old table".to_string(),
            tags: Vec::new(),
        },
    );
    rig.fail_finish_once();
    assert!(bridge.tick().is_err(), "typed, then the finish failed");
    assert_eq!(rig.typed("delete the old table"), 1);
    // The engine did the work and is back at its prompt.
    rig.store.set_pane(&terminal(2), READY);
    rig.lease_expires();
    bridge.tick().expect("the claim comes back");
    assert_eq!(
        rig.typed("delete the old table"),
        1,
        "one command is typed once"
    );
}

/// M7: the end unmounted the workspace, then the `execution.ended` append
/// failed (or the process died). After a restart the end is retried: its
/// final checkpoint runs against the absent workspace, fails, and the end
/// stalls; the execution never ends.
#[test]
fn m7_a_crash_between_unmount_and_ended_wedges_the_execution() {
    let rig = Rig::new();
    let mut bridge = rig.bridge();
    rig.start("");
    bridge.tick().expect("start");
    rig.submit("key_stop", CommandAction::Stop);
    rig.fail_append_once(|event| matches!(event.kind, EventKind::ExecutionEnded { .. }));
    assert!(
        bridge.tick().is_err(),
        "unmounted, then the end's append failed"
    );
    assert!(
        rig.store
            .workspace_calls()
            .iter()
            .any(|call| call == "unmount")
    );
    drop(bridge);
    let mut bridge = rig.restarted();
    for _ in 0..25 {
        let _ = bridge.tick();
        rig.clock.advance(Duration::from_secs(31));
    }
    assert_eq!(
        rig.count("execution.end_stalled"),
        0,
        "a workspace already unmounted is no failed checkpoint"
    );
    assert_eq!(rig.count("execution.ended"), 1, "the end completes");
}

/// M8: a transcript timestamp out of range overflows the date arithmetic
/// (a panic in debug builds, a wrapped time in release ones).
#[test]
fn m8_utc_ms_overflows_on_an_out_of_range_year() {
    for text in [
        "99999999999999999-11-04T05:46:12.000Z",
        "2026-11-04T99999999999999999:46:12.000Z",
    ] {
        let parsed = std::panic::catch_unwind(|| spawn_core::ingest::utc_ms(text));
        assert!(matches!(parsed, Ok(None)), "{text}: {parsed:?}");
    }
}

/// M4: main is never merged into a running execution, not after a Bridge
/// restart between typing a prompt and the engine's transcript recording it
/// either (main reaches a branch only when an execution starts or resumes).
#[test]
fn m4_main_merges_under_a_turn_typed_before_a_restart() {
    let rig = Rig::new();
    let mut bridge = rig.bridge();
    rig.start("");
    bridge.tick().expect("start");
    rig.transcript(&[prompt("1"), turn_end("1")]);
    bridge.tick().expect("read the turn");
    bridge.tick().expect("checkpoint and merge");
    let merged = rig.merges();
    rig.submit(
        "key_input",
        CommandAction::Input {
            text: "go on".to_string(),
            tags: Vec::new(),
        },
    );
    bridge.tick().expect("typed: a turn runs");
    rig.store.set_pane(&terminal(1), BUSY);
    drop(bridge);
    // The engine has not written the prompt to its transcript yet.
    let mut bridge = rig.restarted();
    bridge.tick().expect("first tick after the restart");
    assert_eq!(rig.merges(), merged, "no merge while the typed turn runs");
}

/// M4, without a restart: a turn someone else typed into the pane, whose
/// first busy frame is not confirmed yet. No idle merge exists to slip in.
#[test]
fn m4_main_merges_on_the_first_busy_frame_of_a_turn() {
    let rig = Rig::new();
    let mut bridge = rig.bridge();
    rig.start("");
    bridge.tick().expect("start");
    rig.transcript(&[prompt("1"), turn_end("1")]);
    bridge.tick().expect("read the turn");
    bridge.tick().expect("checkpoint and merge");
    bridge.tick().expect("ready, confirmed");
    let merged = rig.merges();
    // A person types into the pane; the engine is busy at once.
    rig.store.set_pane(&terminal(1), BUSY);
    rig.clock.advance(Duration::from_secs(31));
    bridge.tick().expect("the first busy frame");
    assert_eq!(rig.merges(), merged, "the frame just read is busy");
}

/// L1: a conflict is recorded once: at the start, keyed by the branch head
/// and its files; a restart records nothing again.
#[test]
fn l1_the_same_conflict_is_recorded_again_after_a_restart() {
    let rig = Rig::new();
    let mut bridge = rig.bridge();
    rig.store.set_base_merge(BaseMerge::Conflicted {
        files: vec!["README.md".to_string()],
        marked: true,
    });
    rig.start("");
    bridge.tick().expect("start");
    rig.transcript(&[prompt("1"), turn_end("1")]);
    bridge.tick().expect("read the turn");
    bridge.tick().expect("checkpoint and conflict");
    assert_eq!(rig.count("merge.conflicted"), 1);
    drop(bridge);
    let mut bridge = rig.restarted();
    bridge.tick().expect("after the restart");
    rig.clock.advance(Duration::from_secs(31));
    bridge.tick().expect("idle");
    assert_eq!(rig.count("merge.conflicted"), 1, "one conflict, one record");
}

/// One failure at a handler: its `n`th append (`Some(n)`) or its finish
/// (`None`); then the lease expires and, with `restart`, the Bridge starts
/// over from the log. Returns the rig after the retries, or `None` when the
/// handler made fewer than `n + 1` appends.
fn fail_once(
    rig: &Arc<Rig>,
    bridge: Bridge,
    failure: Option<usize>,
    restart: bool,
    retries: usize,
) -> Option<()> {
    let mut bridge = bridge;
    let seen = failure.map(|n| (n, rig.fail_nth_append(n)));
    if failure.is_none() {
        rig.fail_finish_once();
    }
    let _ = bridge.tick();
    if let Some((n, seen)) = seen
        && seen.load(Ordering::SeqCst) <= n
    {
        rig.clear_failures();
        return None;
    }
    rig.lease_expires();
    if restart {
        drop(bridge);
        bridge = rig.restarted();
    }
    for _ in 0..retries {
        rig.store.set_pane(&terminal(1), READY);
        let _ = bridge.tick();
        rig.clock.advance(Duration::from_secs(31));
    }
    Some(())
}

/// Every failure point of a Start with a prompt: the engine gets its
/// prompt at most once; an execution that is not ended got it and its
/// command succeeded; nothing stays mounted that no execution uses.
#[test]
fn every_start_ends_prompted_or_ended() {
    for restart in [false, true] {
        for failure in std::iter::once(None).chain((0..).map(Some)) {
            let rig = Rig::new();
            rig.start("do the task");
            if fail_once(&rig, rig.bridge(), failure, restart, 3).is_none() {
                break;
            }
            let at = format!("failure {failure:?}, restart {restart}");
            let typed = rig.typed("do the task");
            let running = rig.count("execution.started") - rig.count("execution.ended");
            assert!(typed <= 1, "{at}: typed {typed} times");
            if running > 0 {
                assert_eq!(typed, 1, "{at}: a live engine got its prompt");
                assert_eq!(
                    rig.outcome(1),
                    Some(Outcome::Succeeded),
                    "{at}: a prompted start succeeded"
                );
            }
            assert_eq!(rig.mounted(), running, "{at}: mounts left");
        }
    }
}

/// Every failure point of an Input: typed at most once, and a finished
/// command that succeeded did type it.
#[test]
fn every_input_is_typed_at_most_once() {
    for restart in [false, true] {
        for failure in std::iter::once(None).chain((0..).map(Some)) {
            let rig = Rig::new();
            let mut bridge = rig.bridge();
            rig.start("");
            bridge.tick().expect("start");
            rig.submit(
                "key_input",
                CommandAction::Input {
                    text: "delete the old table".to_string(),
                    tags: Vec::new(),
                },
            );
            if fail_once(&rig, bridge, failure, restart, 3).is_none() {
                break;
            }
            let at = format!("failure {failure:?}, restart {restart}");
            let typed = rig.typed("delete the old table");
            assert!(typed <= 1, "{at}: typed {typed} times");
            let outcome = rig.outcome(2);
            assert!(outcome.is_some(), "{at}: the input finished");
            if outcome == Some(Outcome::Succeeded) {
                assert_eq!(typed, 1, "{at}: a succeeded input was typed");
            }
        }
    }
}

/// Every failure point of a Stop: the end completes once, without a stall,
/// and the workspace is unmounted.
#[test]
fn every_end_completes() {
    for restart in [false, true] {
        for failure in std::iter::once(None).chain((0..).map(Some)) {
            let rig = Rig::new();
            let mut bridge = rig.bridge();
            rig.start("");
            bridge.tick().expect("start");
            rig.submit("key_stop", CommandAction::Stop);
            if fail_once(&rig, bridge, failure, restart, 25).is_none() {
                break;
            }
            let at = format!("failure {failure:?}, restart {restart}");
            assert_eq!(rig.count("execution.end_stalled"), 0, "{at}: stalled");
            assert_eq!(rig.count("execution.ended"), 1, "{at}: ended once");
            assert_eq!(rig.mounted(), 0, "{at}: mounts left");
        }
    }
}

/// L2: a start that died after recording its mount and before its
/// execution left a mount no execution owns. A restart unmounts it and
/// records the release, once.
#[test]
fn a_restart_unmounts_a_mount_whose_start_died() {
    let rig = Rig::new();
    let workspace = WorkspaceId::new("9:/workspaces/9").expect("valid workspace");
    rig.store
        .append(NewEvent {
            agent_id: AgentId::new("agent_one").expect("valid agent"),
            session_id: SessionId::new("session_one").expect("valid session"),
            execution_id: None,
            source: Source::Bridge,
            source_event_id: Some("cmd_9:mounted".to_string()),
            kind: EventKind::CommandStepped {
                command_id: CommandId(9),
                step: CommandStep::Mounted,
                workspace: Some(workspace),
            },
        })
        .expect("append");
    let unmounts = || {
        rig.store
            .workspace_calls()
            .iter()
            .filter(|call| *call == "unmount")
            .count()
    };
    drop(rig.restarted());
    assert_eq!(unmounts(), 1, "the orphan mount goes");
    drop(rig.restarted());
    assert_eq!(unmounts(), 1, "its release is recorded: once");
}
