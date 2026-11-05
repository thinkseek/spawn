//! Audit repros (GitHub PR #2, "engine output trusted as controller
//! authority"): pane text that is not engine UI decides the screen state.
//!
//! Each test states the behaviour the controller should have and fails on
//! the current parser. They are ignored so the suite stays green; run them
//! with `cargo test -p spawn-core --test screen_audit -- --ignored`.
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
#[ignore = "audit H2 repro"]
fn installer_output_is_not_a_permission_dialog_repro() {
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
#[ignore = "audit H2 repro"]
fn a_numbered_yes_in_output_is_not_a_permission_dialog_repro() {
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
#[ignore = "audit H3 repro"]
fn an_error_line_in_output_is_not_an_engine_error_repro() {
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
#[ignore = "audit M10 repro"]
fn busy_markers_in_a_catted_file_are_not_a_running_turn_repro() {
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
#[ignore = "audit M11 repro"]
fn the_trust_answer_in_redrawn_history_is_not_the_dialog_repro() {
    let redraw = "you: what does the Bridge answer at start?\n\
                  tool: Bash (grep -n TRUST_ANSWER crates/spawn-core/src/screen.rs)\n\
                  220:const TRUST_ANSWER: &str = \"Yes, I trust this folder\";\n\
                  claude: It answers the trust dialog with y and Enter.\n\
                  Crunched for 2s · done 9:20 PM\n";
    assert!(!is_trust_dialog(redraw));
}
