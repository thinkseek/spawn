//! Public API tests for spawn-core.

use std::sync::Arc;
use std::time::Duration;

use spawn_core::bridge::{
    Bridge, BridgeConfig, claude_arguments, claude_session_id, claude_settings,
};
use spawn_core::command::{CommandAction, Outcome, Submission};
use spawn_core::contract;
use spawn_core::event::{
    ENGINE, EndCause, EventKind, GlobalPosition, ModelProfile, NewEvent, SessionSeq, Source,
};
use spawn_core::id::{
    AgentId, CommandId, ExecutionId, IdempotencyKey, Principal, SessionId, TerminalName,
};
use spawn_core::memory::MemoryStore;
use spawn_core::observation::{SemanticState, WaitReason};
use spawn_core::observe::{NO_JUDGE, Observer};
use spawn_core::port::{CommandQueue, EventLog, TerminalHost, WorkspaceStore};
use spawn_core::screen::{ScreenState, read_claude_screen};
use spawn_core::time::{ManualClock, Timestamp};

/// A ready pane: input box empty, status line below it.
const READY: &str = "❯ \n  deepseek-ai/DeepSeek-V4.1-Flash · 余量未知 · 249 tok/s";

/// A running turn: spinner above an input box that is still empty.
const BUSY: &str = "✽ Effecting… (27m 12s · ↓ 92.3k tokens)\n❯ \n  deepseek-ai/DeepSeek-V4.1-Flash · 余量未知 · 249 tok/s";

/// A finished turn from a real pane.
const FINISHED: &str = "✻ Sautéed for 6s · done 11:27 PM · 5 shells still running\n❯ \n  deepseek-ai/DeepSeek-V4.1-Flash · 余量未知 · 249 tok/s";

/// A failed hook printed by the engine at the tail.
const HOOK_ERROR: &str = "⎿ UserPromptSubmit hook error\nFailed with non-blocking status code: /bin/sh: /Users/bytedance/.botmux/bin/botmux: No such file or directory\n❯ \n  deepseek-ai/DeepSeek-V4.1-Flash · 余量未知 · 249 tok/s";

/// A permission dialog above the input box.
const PERMISSION: &str = "Do you want to allow this tool call?\n❯ 1. Yes\n  2. No";

/// The screen-reader layout: `$` prompt above the status lines.
const SCREEN_READER: &str =
    "$ \n  deepseek-ai/DeepSeek-V4.1-Flash · 余量未知\nbypass permissions on\nultracode";

/// A running turn in the screen-reader layout, captured from v2.1.316.
///
/// That layout prints words, not spinner glyphs, so this pane carries no
/// glyph the boxed layout would match.
const SCREEN_READER_BUSY: &str = "you: run sleep 20\nThought for 2s (ctrl+o to expand)\ntool: Bash (sleep 20)\nRunning…  (16s)\n(ctrl+b ctrl+b (twice) to run in background)\nNucleating…\ndeepseek-ai/DeepSeek-V4.1-Flash · 余量未知\nbypass permissions on (shift+tab to cycle)\nultracode\n$";

/// A message typed during a running turn. The engine queues it.
const SCREEN_READER_QUEUED: &str = "you: run sleep 20\ntool: Bash (sleep 20)\nRunning…  (16s)\nyou: SECOND\nctrl+x ctrl+s to send now\nNucleating…\ndeepseek-ai/DeepSeek-V4.1-Flash · 余量未知\nbypass permissions on (shift+tab to cycle)\nultracode\n$";

/// The same session after the turn ended.
const SCREEN_READER_DONE: &str = "you: SECOND\nclaude: DONE\nCrunched for 24s · done 9:20 PM\ndeepseek-ai/DeepSeek-V4.1-Flash · 余量未知 · 318 tok/s\nbypass permissions on (shift+tab to cycle)\nultracode\n$";

fn store() -> Arc<MemoryStore> {
    let clock: Arc<ManualClock> = Arc::new(ManualClock::new(Timestamp(1_000)));
    Arc::new(MemoryStore::new("shop", clock))
}

fn agent() -> AgentId {
    AgentId::new("agent_one").expect("valid agent")
}

fn session() -> SessionId {
    SessionId::new("session_one").expect("valid session")
}

#[test]
fn ids_reject_empty_and_long_labels() {
    assert!(AgentId::new("").is_err());
    assert!(AgentId::new("x".repeat(257)).is_err());
    assert!(AgentId::new("agent_one").is_ok());
    assert!(TerminalName::new("spawn-exec_1").is_ok());
    let workspace = spawn_core::id::WorkspaceId::new("1:/workspaces/3").expect("valid workspace");
    assert_eq!(workspace.sandbox(), "1");
    assert_eq!(workspace.path(), "/workspaces/3");
    assert!(spawn_core::id::WorkspaceId::new("1:workspaces/3").is_err());
}

#[test]
fn events_round_trip_through_json() {
    let kinds = [
        EventKind::MessageReceived,
        EventKind::ToolStarted {
            tool_name: "Bash".to_string(),
            tool_use_id: "tool_1".to_string(),
        },
        EventKind::CommandAccepted {
            command_id: CommandId(7),
        },
        EventKind::ExecutionEnded {
            cause: EndCause::Exited,
            exit_code: Some(0),
            signal: None,
            evidence: None,
        },
    ];
    for kind in kinds {
        let value = serde_json::to_value(&kind).expect("serialize");
        assert!(value.get("name").is_some(), "kind carries a name tag");
        let back: EventKind = serde_json::from_value(value).expect("deserialize");
        assert_eq!(back, kind);
        assert_eq!(back.name(), kind.name());
    }
    assert_eq!(Source::parse("claude"), Some(Source::Claude));
    assert_eq!(Source::Gateway.as_str(), "gateway");
    assert!(Source::parse("nope").is_none());
    // A record naming an engine this build does not run still reads; a
    // command naming one does not parse, and its sender sees why.
    let started = serde_json::json!({
        "name": "execution.started",
        "detail": {"engine": "codex", "terminal": "spawn-exec_1",
                   "workspace": "1:/w", "command_id": 1},
    });
    assert!(matches!(
        serde_json::from_value::<EventKind>(started),
        Ok(EventKind::ExecutionStarted { engine, .. }) if engine == "codex"
    ));
    // The terminal is the execution's own: a new record does not name it,
    // and an old one that does reads the same.
    let old = serde_json::json!({
        "name": "execution.started",
        "detail": {"engine": ENGINE, "terminal": "spawn-exec_1",
                   "workspace": "1:/w", "command_id": 1},
    });
    let read: EventKind = serde_json::from_value(old).expect("an old record reads");
    let written = serde_json::to_value(&read).expect("serialize");
    assert!(written["detail"].get("terminal").is_none(), "{written}");
    assert_eq!(
        serde_json::from_value::<EventKind>(written).expect("reads back"),
        read
    );
    assert_eq!(
        TerminalName::for_execution(&ExecutionId::new("exec_1").expect("valid")).as_str(),
        "spawn-exec_1"
    );
    // `engine_version` is optional: a record from before it reads, and an
    // unset one is left out.
    let native = serde_json::json!({
        "name": "session.native_started",
        "detail": {"engine": ENGINE, "native_session_id": "n1", "transcript_path": null},
    });
    let read: EventKind = serde_json::from_value(native.clone()).expect("an old record reads");
    assert!(matches!(
        &read,
        EventKind::NativeSessionStarted {
            engine_version: None,
            ..
        }
    ));
    assert_eq!(serde_json::to_value(&read).expect("serialize"), native);
    let versioned = EventKind::NativeSessionStarted {
        engine: ENGINE.to_string(),
        native_session_id: "n1".to_string(),
        transcript_path: None,
        engine_version: Some("2.1.318".to_string()),
    };
    let written = serde_json::to_value(&versioned).expect("serialize");
    assert_eq!(written["detail"]["engine_version"], "2.1.318");
    assert_eq!(
        serde_json::from_value::<EventKind>(written).expect("reads back"),
        versioned
    );
    let start = |engine: &str| {
        serde_json::from_value::<CommandAction>(
            serde_json::json!({"type": "start", "engine": engine, "prompt": "x"}),
        )
    };
    assert!(start(ENGINE).is_ok());
    let refused = start("codex").expect_err("codex is not an engine");
    assert!(
        refused.to_string().contains("unknown engine codex"),
        "{refused}"
    );
    let unnamed = serde_json::json!({"type": "start", "prompt": "x"});
    assert!(matches!(
        serde_json::from_value::<CommandAction>(unnamed),
        Ok(CommandAction::Start { engine, .. }) if engine == ENGINE
    ));
}

#[test]
fn screen_reads_the_tail_not_the_history() {
    assert_eq!(read_claude_screen(READY).state, ScreenState::Ready);
    assert_eq!(read_claude_screen(BUSY).state, ScreenState::Busy);
    assert_eq!(read_claude_screen(FINISHED).state, ScreenState::Ready);
    assert_eq!(read_claude_screen(HOOK_ERROR).state, ScreenState::Error);
    assert_eq!(
        read_claude_screen(PERMISSION).state,
        ScreenState::Permission
    );
    assert_eq!(read_claude_screen("❯ hello").state, ScreenState::Drafted);

    // The screen-reader layout draws `$` and puts its status lines below it.
    assert_eq!(read_claude_screen(SCREEN_READER).state, ScreenState::Ready);
    // That layout prints words, not spinner glyphs, so a glyph test alone
    // reads a running turn as ready.
    assert_eq!(
        read_claude_screen(SCREEN_READER_BUSY).state,
        ScreenState::Busy
    );
    // A message typed during a running turn is queued, and the turn still runs.
    assert_eq!(
        read_claude_screen(SCREEN_READER_QUEUED).state,
        ScreenState::Busy
    );
    assert_eq!(
        read_claude_screen(SCREEN_READER_DONE).state,
        ScreenState::Ready
    );
    assert_eq!(
        read_claude_screen("$ hello\ndeepseek-ai").state,
        ScreenState::Drafted
    );
    // `$VAR` in pasted shell output is not the input box.
    assert_eq!(read_claude_screen("$HOME/x").state, ScreenState::Unknown);

    assert_eq!(read_claude_screen("").state, ScreenState::Unknown);
    assert_eq!(
        read_claude_screen("no prompt here").state,
        ScreenState::Unknown
    );

    // An empty input box does not mean that input is safe: a running turn
    // also shows an empty box.
    assert!(!read_claude_screen(BUSY).state.accepts_input());
    assert!(read_claude_screen(READY).state.accepts_input());

    // A stale spinner outside the tail window must not decide the state.
    let mut history = String::from("✽ Effecting… (27m 12s)\n");
    for line in 0..60 {
        history.push_str("old output line ");
        history.push_str(&line.to_string());
        history.push('\n');
    }
    history.push_str(READY);
    assert_eq!(read_claude_screen(&history).state, ScreenState::Ready);

    // A tall pane pads the bottom with blank rows. The prompt sits near the
    // top of that padding, so the window must ignore it.
    let mut padded = String::from("Welcome to Claude Code\n\n");
    padded.push_str(READY);
    for _ in 0..60 {
        padded.push('\n');
    }
    assert_eq!(read_claude_screen(&padded).state, ScreenState::Ready);

    // ANSI escapes and control sequences are stripped before matching.
    let ansi = "\u{1b}[2K\u{1b}[1m✽ Effecting…\u{1b}[0m\n\u{1b}[2K❯ \n  model line";
    assert_eq!(read_claude_screen(ansi).state, ScreenState::Busy);
    let ansi_ready = "\u{1b}[2K❯ \u{1b}[0m\n  model line";
    assert_eq!(read_claude_screen(ansi_ready).state, ScreenState::Ready);
}

#[test]
fn claude_arguments_name_the_session_and_install_no_hooks() {
    let arguments = claude_arguments(
        Some("native-1"),
        true,
        &claude_settings(None, ModelProfile::Default),
    );
    assert_eq!(arguments[0], "--ax-screen-reader");
    assert_eq!(arguments[1], "--allow-dangerously-skip-permissions");
    assert_eq!(arguments[2], "--settings");
    let settings = &arguments[3];
    assert!(
        !settings.contains("hooks"),
        "events come from the transcript"
    );
    // The engine must not stop on a permission dialog the Bridge cannot
    // answer, whatever the user's own settings say.
    assert!(settings.contains("skipDangerousModePermissionPrompt"));
    assert_eq!(
        arguments[4..],
        ["--resume".to_string(), "native-1".to_string()]
    );
    let fresh = claude_arguments(Some("native-2"), false, settings);
    assert_eq!(
        fresh[4..],
        ["--session-id".to_string(), "native-2".to_string()]
    );
}

#[test]
fn memory_backend_passes_the_contract() {
    let store = store();
    let concrete: &MemoryStore = store.as_ref();
    contract::event_log("mem_evt", concrete);
    contract::one_execution("mem_one", concrete);
    contract::command_queue("mem_cmd", concrete, concrete);
    contract::workspace_store("mem_ws", concrete);
    contract::terminal("mem_tty", concrete);
    contract::terminal_files("mem_files", concrete, "/scratch");
    contract::transcript_archive("mem_archive", concrete);
}

fn submit_start(store: &MemoryStore, prompt: &str) -> CommandId {
    let principal = Principal::new("tester").expect("valid principal");
    store
        .submit(
            &principal,
            &Submission {
                agent_id: agent(),
                session_id: session(),
                idempotency_key: IdempotencyKey::new("key_start").expect("valid key"),
                action: CommandAction::Start {
                    engine: ENGINE.to_string(),
                    prompt: prompt.to_string(),
                    parent_session_id: None,
                    from: None,
                    model: None,
                    charter: None,
                },
            },
        )
        .expect("submit succeeds")
}

/// The memory host ignores the workspace argument.
fn any_workspace() -> spawn_core::id::WorkspaceId {
    spawn_core::id::WorkspaceId::new("1:/workspaces/1").expect("valid workspace")
}

/// One `spawn tick` process: a new Bridge that refolds the log first.
fn tick_fresh(store: &Arc<MemoryStore>) -> spawn_core::bridge::TickReport {
    let mut bridge = Bridge::new(
        store.clone(),
        store.clone(),
        store.clone(),
        store.clone(),
        BridgeConfig::new("tester"),
    );
    bridge.recover().expect("recover succeeds");
    bridge.tick().expect("tick succeeds")
}

fn submit(store: &MemoryStore, key: &str, action: CommandAction) {
    let principal = Principal::new("tester").expect("valid principal");
    store
        .submit(
            &principal,
            &Submission {
                agent_id: agent(),
                session_id: session(),
                idempotency_key: IdempotencyKey::new(key).expect("valid key"),
                action,
            },
        )
        .expect("submit succeeds");
}

fn ended(store: &MemoryStore) -> Vec<EndCause> {
    events(store)
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::ExecutionEnded { cause, .. } => Some(cause),
            _ => None,
        })
        .collect()
}

#[test]
fn bridge_lifecycle_survives_a_refold_per_tick() {
    let store = store();
    for command in 1..=3 {
        let execution = ExecutionId::for_command(CommandId(command));
        store.set_pane(&TerminalName::for_execution(&execution), READY);
    }
    submit_start(&store, "first");
    tick_fresh(&store);
    submit(&store, "key_stop", CommandAction::Stop);
    let report = tick_fresh(&store);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(ended(&store), vec![EndCause::Stopped]);

    // After a stop the session is free again; a refold must not keep it busy.
    submit(
        &store,
        "key_again",
        CommandAction::Start {
            engine: ENGINE.to_string(),
            prompt: "second".to_string(),
            parent_session_id: None,
            from: None,
            model: None,
            charter: None,
        },
    );
    tick_fresh(&store);
    let finished: Vec<bool> = events(&store)
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::CommandFinished { succeeded, .. } => Some(succeeded),
            _ => None,
        })
        .collect();
    assert_eq!(finished, vec![true, true, true], "{:?}", names(&store));

    // The process dies: the next tick ends the execution once, as lost.
    let second = TerminalName::for_execution(&ExecutionId::for_command(CommandId(3)));
    store
        .terminate(&second, &any_workspace())
        .expect("kill succeeds");
    tick_fresh(&store);
    tick_fresh(&store);
    assert_eq!(ended(&store), vec![EndCause::Stopped, EndCause::Lost]);
}

/// A log whose reader sees nothing: a Bridge over it has not read what
/// another Bridge appended.
struct Unread(Arc<MemoryStore>);

impl EventLog for Unread {
    fn append(
        &self,
        event: NewEvent,
    ) -> Result<spawn_core::event::Appended, spawn_core::port::StoreError> {
        self.0.append(event)
    }

    fn read_global(
        &self,
        _after: GlobalPosition,
        _limit: usize,
    ) -> Result<Vec<spawn_core::event::EventRecord>, spawn_core::port::StoreError> {
        Ok(Vec::new())
    }
}

#[test]
fn a_second_bridge_that_missed_the_start_is_refused_by_the_log() {
    let store = store();
    let first = TerminalName::for_execution(&ExecutionId::for_command(CommandId(1)));
    store.set_pane(&first, READY);
    submit_start(&store, "first");
    tick_fresh(&store);

    // Another Bridge process, which has not read that start, takes the
    // session's next start.
    submit(
        &store,
        "key_again",
        CommandAction::Start {
            engine: ENGINE.to_string(),
            prompt: "second".to_string(),
            parent_session_id: None,
            from: None,
            model: None,
            charter: None,
        },
    );
    let mut other = Bridge::new(
        Arc::new(Unread(store.clone())),
        store.clone(),
        store.clone(),
        store.clone(),
        BridgeConfig::new("other"),
    );
    other.tick().expect("tick succeeds");

    let failures: Vec<String> = events(&store)
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::CommandFinished {
                succeeded: false,
                error,
                ..
            } => error,
            _ => None,
        })
        .collect();
    assert_eq!(failures.len(), 1, "{:?}", names(&store));
    assert!(failures[0].contains("already runs"), "{failures:?}");
    let started = names(&store)
        .into_iter()
        .filter(|name| *name == "execution.started")
        .count();
    assert_eq!(started, 1, "one execution recorded");
    assert_eq!(store.launches().len(), 1, "no second engine");
    let calls = store.workspace_calls();
    assert_eq!(
        calls.iter().filter(|call| *call == "unmount").count(),
        1,
        "the refused start's own mount goes: {calls:?}"
    );
    assert!(store.is_alive(&first, &any_workspace()).expect("alive"));
    // The first execution goes on and ends as before.
    assert!(tick_fresh(&store).warnings.is_empty());
    assert!(ended(&store).is_empty());
}

fn events(store: &MemoryStore) -> Vec<NewEvent> {
    store
        .read_global(spawn_core::event::GlobalPosition(0), 500)
        .expect("read succeeds")
        .into_iter()
        .map(|record| record.event)
        .collect()
}

fn names(store: &MemoryStore) -> Vec<&'static str> {
    events(store)
        .iter()
        .map(|event| event.kind.name())
        .collect()
}

#[test]
fn bridge_starts_an_execution_and_types_the_prompt() {
    let store = store();
    let log: Arc<dyn EventLog> = store.clone();
    let queue: Arc<dyn CommandQueue> = store.clone();
    let workspaces: Arc<dyn WorkspaceStore> = store.clone();
    let terminals: Arc<dyn TerminalHost> = store.clone();
    let mut bridge = Bridge::new(
        log,
        queue,
        workspaces,
        terminals,
        BridgeConfig::new("tester"),
    );
    let execution = ExecutionId::for_command(CommandId(1));
    let terminal = TerminalName::for_execution(&execution);
    store.set_pane(&terminal, READY);

    submit_start(&store, "list the files");
    let report = bridge.tick().expect("tick succeeds");
    assert_eq!(report.commands, 1);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);

    assert_eq!(
        names(&store),
        vec![
            "command.accepted",
            "command.step",
            "execution.started",
            "session.native_started",
            "screen.observed",
            "command.step",
            "command.step",
            "command.finished",
        ]
    );
    let pane = store
        .capture_tail(&terminal, &any_workspace(), 40)
        .expect("pane exists");
    assert!(
        pane.contains("list the files"),
        "prompt reached the pane: {pane}"
    );
    assert_eq!(bridge.session_count(), 1);
}

#[test]
fn the_bridge_follows_the_transcript_the_screen_and_the_exit_without_hooks() {
    let store = store();
    let mut bridge = Bridge::new(
        store.clone(),
        store.clone(),
        store.clone(),
        store.clone(),
        BridgeConfig::new("tester"),
    );
    let execution = ExecutionId::for_command(CommandId(1));
    let terminal = TerminalName::for_execution(&execution);
    store.set_pane(&terminal, READY);
    submit_start(&store, "list the files");
    bridge.tick().expect("tick succeeds");

    // The Bridge named the session, so it knows the transcript.
    let (native, path) = store
        .read_global(GlobalPosition(0), 100)
        .expect("read")
        .into_iter()
        .find_map(|record| match record.event.kind {
            EventKind::NativeSessionStarted {
                native_session_id,
                transcript_path,
                ..
            } => Some((native_session_id, transcript_path.expect("a path"))),
            _ => None,
        })
        .expect("the start records the native session");
    assert_eq!(native, claude_session_id(&execution));
    let at = r#""timestamp":"2099-01-01T00:00:00.000Z""#;
    store.append_file(
        &path,
        &format!(
            "{{\"type\":\"assistant\",\"uuid\":\"a1\",{at},\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"id\":\"toolu_9\",\"name\":\"Bash\"}}]}}}}\n\
             {{\"type\":\"user\",\"uuid\":\"u1\",{at},\"message\":{{\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"toolu_9\"}}]}}}}\n\
             {{\"type\":\"system\",\"subtype\":\"turn_dur"
        ),
    );
    bridge.tick().expect("tick");
    bridge.tick().expect("a second read appends nothing twice");
    let engine_events = |store: &MemoryStore| -> Vec<&'static str> {
        store
            .read_global(GlobalPosition(0), 200)
            .expect("read")
            .into_iter()
            .filter(|record| record.event.source == Source::Claude)
            .map(|record| record.event.kind.name())
            .collect()
    };
    assert_eq!(
        engine_events(&store),
        vec!["tool.started", "tool.completed"]
    );
    // The half-written line is read once it is whole.
    store.append_file(&path, &format!("ation\",\"uuid\":\"s1\",{at}}}\n"));
    bridge.tick().expect("tick");
    assert_eq!(
        engine_events(&store),
        vec!["tool.started", "tool.completed", "turn.stopped"]
    );

    // A screen state is recorded once two readings agree.
    store.set_pane(
        &terminal,
        "✽ Effecting… (esc to interrupt)\n❯ \n  model line",
    );
    let busy = |store: &MemoryStore| {
        store
            .read_global(GlobalPosition(0), 200)
            .expect("read")
            .into_iter()
            .filter(|record| {
                matches!(
                    record.event.kind,
                    EventKind::ScreenObserved {
                        state: ScreenState::Busy,
                        ..
                    }
                )
            })
            .count()
    };
    bridge.tick().expect("tick");
    assert_eq!(busy(&store), 0, "one reading is not a fact yet");
    bridge.tick().expect("tick");
    assert_eq!(busy(&store), 1);
    bridge.tick().expect("tick");
    assert_eq!(busy(&store), 1, "an unchanged screen is not recorded again");

    // The runner's exit file ends the execution with its status.
    store.append_file(
        &format!("{}/.spawn/exit-{execution}", any_workspace().path()),
        "3",
    );
    bridge.tick().expect("tick");
    let ended = store
        .read_global(GlobalPosition(0), 200)
        .expect("read")
        .into_iter()
        .find_map(|record| match record.event.kind {
            EventKind::ExecutionEnded {
                cause, exit_code, ..
            } => Some((cause, exit_code)),
            _ => None,
        });
    assert_eq!(ended, Some((EndCause::Exited, Some(3))));
}

#[test]
fn the_engine_version_is_read_from_the_transcript_once_per_execution() {
    let store = store();
    let execution = ExecutionId::for_command(CommandId(1));
    store.set_pane(&TerminalName::for_execution(&execution), READY);
    submit_start(&store, "list the files");
    tick_fresh(&store);
    let path = events(&store)
        .into_iter()
        .find_map(|event| match event.kind {
            EventKind::NativeSessionStarted {
                transcript_path, ..
            } => transcript_path,
            _ => None,
        })
        .expect("the start records the transcript");
    let at = r#""timestamp":"2099-01-01T00:00:00.000Z""#;
    store.append_file(
        &path,
        &format!(
            "{{\"type\":\"user\",\"uuid\":\"u1\",{at},\"version\":\"2.1.318\",\"message\":{{\"content\":\"go\"}}}}\n\
             {{\"type\":\"system\",\"subtype\":\"turn_duration\",\"uuid\":\"s1\",{at},\"version\":\"2.1.318\"}}\n"
        ),
    );
    tick_fresh(&store);
    // A restarted Bridge reads the file again; the key keeps one record.
    tick_fresh(&store);
    let versions: Vec<(Source, Option<String>)> = events(&store)
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::NativeSessionStarted { engine_version, .. } => {
                Some((event.source, engine_version))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        versions,
        vec![
            (Source::Bridge, None),
            (Source::Claude, Some("2.1.318".to_string()))
        ]
    );
}

/// The transcript `session.native_started` names for the latest execution.
fn transcript_of_latest(store: &MemoryStore) -> (String, String) {
    let events = events(store);
    let native = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::NativeSessionStarted {
                native_session_id, ..
            } => Some(native_session_id.clone()),
            _ => None,
        })
        .expect("a native session");
    let workspace = events
        .iter()
        .rev()
        .find_map(|event| match &event.kind {
            EventKind::ExecutionStarted { workspace, .. } => Some(workspace.clone()),
            _ => None,
        })
        .expect("an execution");
    let path = spawn_core::bridge::transcript_path(
        &BridgeConfig::new("tester").claude_projects,
        &workspace,
        &native,
    );
    (native, path)
}

fn archiving_bridge(store: &Arc<MemoryStore>) -> Bridge {
    Bridge::new(
        store.clone(),
        store.clone(),
        store.clone(),
        store.clone(),
        BridgeConfig::new("tester"),
    )
    .with_transcript_archive(store.clone())
}

#[test]
fn a_resume_restores_a_deleted_transcript_from_the_archive() {
    use spawn_core::port::TranscriptArchive;
    let store = store();
    for command in 1..=3 {
        let execution = ExecutionId::for_command(CommandId(command));
        store.set_pane(&TerminalName::for_execution(&execution), READY);
    }
    let mut bridge = archiving_bridge(&store);
    submit_start(&store, "first");
    bridge.tick().expect("start");
    let (native, first) = transcript_of_latest(&store);
    let at = r#""timestamp":"2099-01-01T00:00:00.000Z""#;
    store.append_file(
        &first,
        &format!(
            "{{\"type\":\"user\",\"uuid\":\"u1\",{at},\"message\":{{\"content\":\"go\"}}}}\n\
             {{\"type\":\"system\",\"subtype\":\"turn_duration\",\"uuid\":\"s1\",{at}}}\n"
        ),
    );
    bridge.tick().expect("read the turn");
    bridge
        .tick()
        .expect("checkpoint the turn end, and copy the transcript");
    let lengths = store.copies(&agent(), &native).expect("copies");
    assert_eq!(lengths.len(), 1, "copied at the turn end");

    // The engine rewrites its file (a format change, a compaction): the
    // copy taken at the execution's end is a new one, the old one stays.
    let rewritten =
        format!("{{\"type\":\"user\",\"uuid\":\"u1\",{at},\"message\":{{\"content\":\"GO\"}}}}\n");
    store.replace_file(&first, rewritten.as_bytes());
    submit(&store, "key_stop", CommandAction::Stop);
    bridge.tick().expect("stop");
    assert_eq!(ended(&store), vec![EndCause::Stopped]);
    assert_eq!(
        store.copies(&agent(), &native).expect("copies"),
        vec![lengths[0], u64::try_from(rewritten.len()).expect("fits")]
    );

    // Claude Code's cleanup deletes the transcript; the resume puts the
    // latest copy back where the engine reads it.
    store.remove_file(&first);
    submit(
        &store,
        "key_resume",
        CommandAction::Resume {
            prompt: Some("go on".to_string()),
            model: None,
        },
    );
    let report = bridge.tick().expect("resume");
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("restored")),
        "{:?}",
        report.warnings
    );
    let (_, resumed) = transcript_of_latest(&store);
    assert_eq!(
        store
            .read_from(&any_workspace(), &resumed, 0)
            .expect("read"),
        Some(rewritten.into_bytes())
    );
    let launch = store.launches().pop().expect("a launch");
    assert!(
        launch
            .arguments
            .windows(2)
            .any(|pair| pair[0] == "--resume" && pair[1] == native),
        "{:?}",
        launch.arguments
    );
}

#[test]
fn a_resume_whose_transcript_is_gone_without_a_copy_fails_and_starts_nothing() {
    let store = store();
    store.set_pane(
        &TerminalName::for_execution(&ExecutionId::for_command(CommandId(1))),
        READY,
    );
    // No archive.
    let mut bridge = Bridge::new(
        store.clone(),
        store.clone(),
        store.clone(),
        store.clone(),
        BridgeConfig::new("tester"),
    );
    submit_start(&store, "first");
    bridge.tick().expect("start");
    let (_, first) = transcript_of_latest(&store);
    store.append_file(
        &first,
        "{\"type\":\"user\",\"uuid\":\"u1\",\"timestamp\":\"2099-01-01T00:00:00.000Z\",\"message\":{\"content\":\"go\"}}\n",
    );
    bridge.tick().expect("read the prompt");
    submit(&store, "key_stop", CommandAction::Stop);
    bridge.tick().expect("stop");
    submit(
        &store,
        "key_resume",
        CommandAction::Resume {
            prompt: None,
            model: None,
        },
    );
    bridge.tick().expect("resume");
    let failure = events(&store)
        .into_iter()
        .find_map(|event| match event.kind {
            EventKind::CommandFinished {
                succeeded: false,
                error,
                ..
            } => error,
            _ => None,
        })
        .expect("the resume failed");
    assert!(failure.contains("start the agent blank"), "{failure}");
    assert_eq!(store.launches().len(), 1, "no engine was started blank");
    let calls = store.workspace_calls();
    assert_eq!(
        calls.last().map(String::as_str),
        Some("unmount"),
        "{calls:?}"
    );
}

#[test]
fn an_engine_that_exits_between_the_two_reads_is_exited_not_lost() {
    let store = store();
    let mut bridge = Bridge::new(
        store.clone(),
        store.clone(),
        store.clone(),
        store.clone(),
        BridgeConfig::new("tester"),
    );
    let execution = ExecutionId::for_command(CommandId(1));
    let terminal = TerminalName::for_execution(&execution);
    store.set_pane(&terminal, READY);
    submit_start(&store, "list the files");
    bridge.tick().expect("tick succeeds");
    // The exit file is absent when the Bridge reads it, then the engine
    // writes it and its pane closes before the liveness check.
    store.exit_on_next_check(
        &terminal,
        &format!("{}/.spawn/exit-{execution}", any_workspace().path()),
        "0",
    );
    bridge.tick().expect("tick");
    let ended = store
        .read_global(GlobalPosition(0), 200)
        .expect("read")
        .into_iter()
        .find_map(|record| match record.event.kind {
            EventKind::ExecutionEnded {
                cause, exit_code, ..
            } => Some((cause, exit_code)),
            _ => None,
        });
    assert_eq!(ended, Some((EndCause::Exited, Some(0))));
}

#[test]
fn a_prompt_that_cannot_be_typed_ends_the_execution_as_lost() {
    let store = store();
    let mut config = BridgeConfig::new("tester");
    config.ready_attempts = 2;
    config.ready_interval = std::time::Duration::from_millis(1);
    let mut bridge = Bridge::new(
        store.clone(),
        store.clone(),
        store.clone(),
        store.clone(),
        config,
    );
    // The pane never shows a ready input box.
    submit_start(&store, "list the files");
    bridge.tick().expect("tick succeeds");
    let names = names(&store);
    assert!(
        names.ends_with(&["execution.ended", "command.finished"]),
        "an engine without its task must not look alive: {names:?}"
    );
}

#[test]
fn bridge_refuses_input_while_a_turn_runs() {
    let store = store();
    let log: Arc<dyn EventLog> = store.clone();
    let queue: Arc<dyn CommandQueue> = store.clone();
    let workspaces: Arc<dyn WorkspaceStore> = store.clone();
    let terminals: Arc<dyn TerminalHost> = store.clone();
    let mut bridge = Bridge::new(
        log,
        queue,
        workspaces,
        terminals,
        BridgeConfig::new("tester"),
    );

    let execution = ExecutionId::for_command(CommandId(1));
    let terminal = TerminalName::for_execution(&execution);
    store.set_pane(&terminal, BUSY);

    submit_start(&store, "first");
    bridge.tick().expect("tick succeeds");
    let last = events(&store).pop().expect("an event");
    assert_eq!(last.kind.name(), "command.finished");
    match last.kind {
        EventKind::CommandFinished { succeeded, .. } => assert!(!succeeded),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn bridge_checkpoints_and_unmounts_on_stop() {
    let store = store();
    let log: Arc<dyn EventLog> = store.clone();
    let queue: Arc<dyn CommandQueue> = store.clone();
    let workspaces: Arc<dyn WorkspaceStore> = store.clone();
    let terminals: Arc<dyn TerminalHost> = store.clone();
    let mut bridge = Bridge::new(
        log,
        queue,
        workspaces,
        terminals,
        BridgeConfig::new("tester"),
    );

    let execution = ExecutionId::for_command(CommandId(1));
    let terminal = TerminalName::for_execution(&execution);
    store.set_pane(&terminal, READY);
    submit_start(&store, "change the readme");
    bridge.tick().expect("tick succeeds");

    let workspace = events(&store)
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::ExecutionStarted { workspace, .. } => Some(workspace.clone()),
            _ => None,
        })
        .expect("execution started");
    store.mark_dirty(&workspace);

    let principal = Principal::new("tester").expect("valid principal");
    store
        .submit(
            &principal,
            &Submission {
                agent_id: agent(),
                session_id: session(),
                idempotency_key: IdempotencyKey::new("key_stop").expect("valid key"),
                action: CommandAction::Stop,
            },
        )
        .expect("submit succeeds");
    bridge.tick().expect("tick succeeds");

    assert!(
        names(&store).contains(&"artifact.committed"),
        "stop commits the workspace: {:?}",
        names(&store)
    );
    assert!(
        !store
            .is_alive(&terminal, &any_workspace())
            .expect("query succeeds"),
        "terminal is gone"
    );
}

#[test]
fn bridge_resumes_the_stored_native_session() {
    let store = store();
    let log: Arc<dyn EventLog> = store.clone();
    let queue: Arc<dyn CommandQueue> = store.clone();
    let workspaces: Arc<dyn WorkspaceStore> = store.clone();
    let terminals: Arc<dyn TerminalHost> = store.clone();
    let mut bridge = Bridge::new(
        log,
        queue,
        workspaces,
        terminals,
        BridgeConfig::new("tester"),
    );

    // A hook already recorded the native session.
    store
        .append(NewEvent {
            agent_id: agent(),
            session_id: session(),
            execution_id: None,
            source: Source::Claude,
            source_event_id: None,
            kind: EventKind::NativeSessionStarted {
                engine: ENGINE.to_string(),
                native_session_id: "native-7".to_string(),
                transcript_path: None,
                engine_version: None,
            },
        })
        .expect("append succeeds");

    // A fresh store accepts this submission as command 1, so the execution
    // and its terminal are known before the tick runs.
    let execution = ExecutionId::for_command(CommandId(1));
    store.set_pane(&TerminalName::for_execution(&execution), READY);
    let execution_again = execution.clone();

    let principal = Principal::new("tester").expect("valid principal");
    store
        .submit(
            &principal,
            &Submission {
                agent_id: agent(),
                session_id: session(),
                idempotency_key: IdempotencyKey::new("key_resume").expect("valid key"),
                action: CommandAction::Resume {
                    prompt: Some("继续".to_string()),
                    model: None,
                },
            },
        )
        .expect("submit succeeds");
    bridge.tick().expect("tick succeeds");

    let accepted = events(&store)
        .into_iter()
        .find_map(|event| match event.kind {
            EventKind::CommandAccepted { command_id } => Some(command_id),
            _ => None,
        })
        .expect("the command was accepted");
    let started = events(&store)
        .into_iter()
        .find(|event| event.kind.name() == "execution.started")
        .expect("resume started an execution");
    assert_eq!(accepted, CommandId(1));
    assert_eq!(started.execution_id, Some(execution_again.clone()));
    let pane = store
        .capture_tail(
            &TerminalName::for_execution(&execution),
            &any_workspace(),
            40,
        )
        .expect("the resumed pane exists");
    assert!(pane.contains("继续"), "the resume prompt reached the pane");
}

#[test]
fn observer_writes_only_on_change() {
    let store = store();
    let log: Arc<dyn EventLog> = store.clone();
    let mut observer = Observer::new(log);

    store
        .append(NewEvent {
            agent_id: agent(),
            session_id: session(),
            execution_id: Some(ExecutionId::for_command(CommandId(1))),
            source: Source::Bridge,
            source_event_id: None,
            kind: EventKind::ExecutionStarted {
                engine: ENGINE.to_string(),
                workspace: spawn_core::id::WorkspaceId::new("1:/workspaces/1").expect("valid"),
                command_id: CommandId(1),
                fork: None,
                model: ModelProfile::Default,
                transcript_from: None,
            },
        })
        .expect("append succeeds");
    store
        .append(NewEvent {
            agent_id: agent(),
            session_id: session(),
            execution_id: None,
            source: Source::Claude,
            source_event_id: Some("t1:started".to_string()),
            kind: EventKind::ToolStarted {
                tool_name: "Bash".to_string(),
                tool_use_id: "t1".to_string(),
            },
        })
        .expect("append succeeds");

    assert_eq!(observer.tick().expect("tick succeeds"), 1);
    let recorded = events(&store)
        .into_iter()
        .find(|event| event.kind.name() == "observation.recorded")
        .expect("an observation exists");
    match recorded.kind {
        EventKind::ObservationRecorded { observation } => {
            assert_eq!(observation.state, SemanticState::Working);
        }
        other => panic!("unexpected {other:?}"),
    }

    assert_eq!(
        observer.tick().expect("tick succeeds"),
        0,
        "no change, no write"
    );
    let mut next = Observer::new(store.clone());
    next.recover().expect("recover succeeds");
    assert_eq!(
        next.tick().expect("tick succeeds"),
        0,
        "a new process restores the last observation"
    );

    store
        .append(NewEvent {
            agent_id: agent(),
            session_id: session(),
            execution_id: None,
            source: Source::Claude,
            source_event_id: None,
            kind: EventKind::TurnStopped {
                input_tokens: None,
                output_tokens: None,
            },
        })
        .expect("append succeeds");
    assert_eq!(observer.tick().expect("tick succeeds"), 1);
    let last = events(&store).pop().expect("an event");
    match last.kind {
        // The rules cannot settle a stopped turn and no judge is
        // configured: their guess stands, as ambiguous, and asks for
        // attention, as when the judge is down.
        EventKind::ObservationRecorded { observation } => {
            assert_eq!(observation.state, SemanticState::Waiting);
            assert!(observation.based_on_seq >= SessionSeq(1));
            assert_eq!(observation.judge, NO_JUDGE);
            assert!(observation.ambiguity.is_some());
            assert_eq!(observation.reason, Some(WaitReason::JudgeUnavailable));
            assert!(observation.attention);
            assert_eq!(
                observation.attention_reason.as_deref(),
                Some("judge_unavailable")
            );
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn lease_ttl_expires_and_takes_the_command_again() {
    let clock = Arc::new(ManualClock::new(Timestamp(1_000)));
    let store = MemoryStore::new("shop", clock.clone());
    let principal = Principal::new("tester").expect("valid principal");
    let submission = Submission {
        agent_id: agent(),
        session_id: session(),
        idempotency_key: IdempotencyKey::new("key_1").expect("valid key"),
        action: CommandAction::Start {
            engine: ENGINE.to_string(),
            prompt: "x".to_string(),
            parent_session_id: None,
            from: None,
            model: None,
            charter: None,
        },
    };
    let id = store
        .submit(&principal, &submission)
        .expect("submit succeeds");

    let first = store
        .claim("w1", Duration::from_mins(1))
        .expect("claim succeeds")
        .expect("ready");
    assert_eq!(first.command_id, id);
    assert_eq!(first.attempt, 1);
    assert!(
        store
            .claim("w2", Duration::from_mins(1))
            .expect("claim succeeds")
            .is_none(),
        "a live lease blocks a second worker"
    );

    clock.advance(Duration::from_secs(61));
    let second = store
        .claim("w2", Duration::from_mins(1))
        .expect("claim succeeds")
        .expect("the expired lease is taken again");
    assert_eq!(second.attempt, 2);
    assert!(
        store.finish(&first, &Outcome::Succeeded).is_err(),
        "the old attempt must not finish the command"
    );
    store
        .finish(&second, &Outcome::Succeeded)
        .expect("the current attempt finishes");
}
