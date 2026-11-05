//! Pane text that is not engine UI never decides the screen state (audit,
//! GitHub PR #2: H2, H3, M10, M11). Each pane is idle; its transcript holds
//! text that a phrase match anywhere above the prompt took for engine UI.
//!
//! The panes are the screen-reader layout Spawn runs (`--ax-screen-reader`):
//! the transcript (`you:`, `claude:`, `tool:` lines and the tool's output,
//! unprefixed), then the status lines, then the `$` prompt. Every engine
//! runs with `bypassPermissions`, so no real permission dialog is drawn.

use spawn_core::screen::{ScreenState, is_trust_dialog, read_claude_screen};

/// The status lines and prompt of an idle engine (tests/api.rs,
/// SCREEN_READER_DONE, captured from v2.1.316).
const IDLE_FOOTER: &str = "deepseek-ai/DeepSeek-V4.1-Flash · 余量未知 · 318 tok/s\nbypass permissions on (shift+tab to cycle)\nultracode\n$";

fn idle_after(transcript: &str) -> String {
    format!("{transcript}\n{IDLE_FOOTER}")
}

/// H2: an installer's question in a tool's output is read as a permission
/// dialog; the Observer then writes a rules-tier BLOCKED and input is refused.
#[test]
fn installer_output_is_not_a_permission_dialog() {
    let pane = idle_after(
        "you: install the tools\n\
         tool: Bash (apt-get install jq)\n\
         Do you want to continue? [Y/n] Abort.\n\
         claude: apt wants a confirmation; I stopped there.\n\
         Crunched for 4s · done 9:20 PM",
    );
    assert_eq!(read_claude_screen(&pane).state, ScreenState::Ready);
}

/// H2: a numbered list in a tool's output (a catted file) reads as a dialog.
#[test]
fn a_numbered_yes_in_output_is_not_a_permission_dialog() {
    let pane = idle_after(
        "tool: Read (docs/cleanup.md)\n\
         1. Yes, delete it and continue\n\
         2. No, keep the branch\n\
         claude: Done.\n\
         Crunched for 2s · done 9:20 PM",
    );
    assert_eq!(read_claude_screen(&pane).state, ScreenState::Ready);
}

/// H3: a capitalized `Error:` line in a tool's output reads as an engine
/// error; `type_prompt` then waits for Ready and fails a start or resume.
#[test]
fn an_error_line_in_output_is_not_an_engine_error() {
    let pane = idle_after(
        "tool: Bash (./client --once)\n\
         Error: connection refused, retrying in 5s\n\
         claude: The server is not up yet.\n\
         Crunched for 7s · done 9:20 PM",
    );
    assert_eq!(read_claude_screen(&pane).state, ScreenState::Ready);
}

/// M10: this repository's own screen.rs, catted by an agent, holds the busy
/// markers; an idle pane then reads Busy and refuses input.
#[test]
fn busy_markers_in_a_catted_file_are_not_a_running_turn() {
    let pane = idle_after(
        "tool: Bash (sed -n 35,45p crates/spawn-core/src/screen.rs)\n\
         const SCREEN_READER_BUSY: [&str; 4] = [\n    \"Running…\",\n    \"Nucleating…\",\n\
         claude: Those are the busy markers.\n\
         Crunched for 3s · done 9:20 PM",
    );
    assert_eq!(read_claude_screen(&pane).state, ScreenState::Ready);
}

/// M11: a resumed engine redraws its history before it draws the input box.
/// A line of that history holding the trust dialog's answer (this
/// repository's screen.rs holds it) reads as the dialog, and the Bridge
/// sends `y`, `Enter`, which become a prompt once the input box exists.
#[test]
fn the_trust_answer_in_redrawn_history_is_not_the_dialog() {
    let redraw = "you: what does the Bridge answer at start?\n\
                  tool: Bash (grep -n TRUST_ANSWER crates/spawn-core/src/screen.rs)\n\
                  220:const TRUST_ANSWER: &str = \"Yes, I trust this folder\";\n\
                  claude: It answers the trust dialog with y and Enter.\n\
                  Crunched for 2s · done 9:20 PM\n";
    assert!(!is_trust_dialog(redraw));
}

/// The activity region does decide: a spinner word the screen-reader layout
/// draws (not only the captured `Nucleating…`) is a running turn, and an API
/// error right above the status lines is the engine's.
#[test]
fn the_activity_region_decides() {
    let busy = "you: build it\ntool: Bash (cargo build)\nPondering…\n\
                deepseek-ai/DeepSeek-V4.1-Flash · 余量未知\n\
                bypass permissions on (shift+tab to cycle)\nultracode\n$";
    assert_eq!(read_claude_screen(busy).state, ScreenState::Busy);
    let failed = idle_after("you: build it\nAPI Error: 529 overloaded_error");
    assert_eq!(read_claude_screen(&failed).state, ScreenState::Error);
    // The same words in the transcript are history.
    let quoted = idle_after(
        "claude: I saw API Error: 529 earlier, and Pondering… in the docs.\n\
         Crunched for 2s · done 9:20 PM",
    );
    assert_eq!(read_claude_screen(&quoted).state, ScreenState::Ready);
}

/// The trust dialog is the pane's last lines, nothing else: a frame that
/// quotes the dialog and goes on is not it.
#[test]
fn only_the_dialogs_own_footer_is_the_dialog() {
    let footer = "y. Yes, I trust this folder\nn. No, exit\nEnter y/n:\n\
                  Enter to confirm · Esc to cancel";
    assert!(is_trust_dialog(&format!(
        "Quick safety check: Is this a project you created or one you trust?\n{footer}\n\n\n"
    )));
    assert!(!is_trust_dialog(&format!(
        "tool: Bash (cat notes.txt)\n{footer}\nclaude: That is the dialog."
    )));
}

mod bridge {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use spawn_core::bridge::{Bridge, BridgeConfig};
    use spawn_core::command::{CommandAction, Submission};
    use spawn_core::event::GlobalPosition;
    use spawn_core::event::{ENGINE, EventKind};
    use spawn_core::id::{
        AgentId, CommandId, ExecutionId, IdempotencyKey, Principal, SessionId, TerminalName,
        WorkspaceId,
    };
    use spawn_core::memory::MemoryStore;
    use spawn_core::port::{CommandQueue, EventLog, Launch, TerminalError, TerminalHost};
    use spawn_core::time::{ManualClock, Timestamp};

    const DIALOG: &str = "[Screen Reader Mode: on via flag]
Quick safety check: Is this a project you created or one you trust?
y. Yes, I trust this folder
n. No, exit
Enter y/n:
Enter to confirm · Esc to cancel
";

    /// The idle input box the engine draws once it runs.
    const READY: &str = "deepseek-ai/DeepSeek-V4.1-Flash · 余量未知\nbypass permissions on\n$ ";

    /// The memory terminal, keeping the keys pressed; once a key is pressed
    /// the engine draws its input box, as Claude Code does after the dialog.
    struct Keys {
        inner: Arc<MemoryStore>,
        pressed: Mutex<Vec<String>>,
    }

    impl TerminalHost for Keys {
        fn start(&self, launch: &Launch) -> Result<(), TerminalError> {
            self.inner.start(launch)
        }
        fn submit_text(
            &self,
            terminal: &TerminalName,
            workspace: &WorkspaceId,
            text: &str,
        ) -> Result<(), TerminalError> {
            self.inner.submit_text(terminal, workspace, text)
        }
        fn send_keys(
            &self,
            terminal: &TerminalName,
            _workspace: &WorkspaceId,
            keys: &[&str],
        ) -> Result<(), TerminalError> {
            let mut pressed = self.pressed.lock().expect("lock");
            pressed.extend(keys.iter().map(|key| (*key).to_string()));
            self.inner.set_pane(terminal, READY);
            Ok(())
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

    /// Start a session whose first pane is `pane`, with a short wait for a
    /// ready prompt: the keys pressed, and whether the start succeeded.
    fn start_on(pane: &str) -> (Vec<String>, bool) {
        let clock: Arc<ManualClock> = Arc::new(ManualClock::new(Timestamp(1_000)));
        let store = Arc::new(MemoryStore::new("shop", clock));
        let terminal = TerminalName::for_execution(&ExecutionId::for_command(CommandId(1)));
        store.set_pane(&terminal, pane);
        let keys = Arc::new(Keys {
            inner: store.clone(),
            pressed: Mutex::new(Vec::new()),
        });
        let mut config = BridgeConfig::new("tester");
        config.ready_attempts = 4;
        config.ready_interval = Duration::ZERO;
        let mut bridge = Bridge::new(
            store.clone(),
            store.clone(),
            store.clone(),
            keys.clone(),
            config,
        );
        store
            .submit(
                &Principal::new("tester").expect("valid principal"),
                &Submission {
                    agent_id: AgentId::new("agent_one").expect("valid agent"),
                    session_id: SessionId::new("session_one").expect("valid session"),
                    idempotency_key: IdempotencyKey::new("key_start").expect("valid key"),
                    action: CommandAction::Start {
                        engine: ENGINE.to_string(),
                        prompt: "work".to_string(),
                        parent_session_id: None,
                        from: None,
                        model: None,
                    },
                },
            )
            .expect("submit succeeds");
        bridge.tick().expect("tick");
        let succeeded = store
            .read_global(GlobalPosition(0), 100)
            .expect("read")
            .iter()
            .any(|record| {
                matches!(
                    record.event.kind,
                    EventKind::CommandFinished {
                        succeeded: true,
                        ..
                    }
                )
            });
        let pressed = keys.pressed.lock().expect("lock").clone();
        (pressed, succeeded)
    }

    #[test]
    fn the_dialog_is_answered_once_after_two_equal_reads() {
        let (pressed, started) = start_on(DIALOG);
        assert_eq!(pressed, ["y", "Enter"]);
        assert!(started, "the prompt was typed once the input box came");
    }

    /// M11: history that quotes the dialog, redrawn before the input box,
    /// gets no keys.
    #[test]
    fn history_quoting_the_dialog_gets_no_keys() {
        let quoted = "tool: Bash (grep -n TRUST screen.rs)\n\
                      220:    \"y. Yes, I trust this folder\",\n\
                      claude: It answers with y and Enter.\n";
        let (pressed, _) = start_on(quoted);
        assert!(pressed.is_empty(), "{pressed:?}");
        let (pressed, _) = start_on(&format!("{READY}\n{DIALOG}"));
        assert!(
            pressed.is_empty(),
            "a prompt above is not the dialog: {pressed:?}"
        );
    }
}
