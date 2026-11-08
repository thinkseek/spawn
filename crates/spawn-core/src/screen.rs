//! Reading the tail of a Claude Code pane.
//!
//! Claude Code runs with `--ax-screen-reader`, so its terminal output is
//! plain text without the redrawn boxes of the default renderer. Most of
//! what the pane shows is transcript: the model's prose, the tools it ran
//! and their output, a file it printed. None of that is the engine's own
//! UI, and the model or any program it runs can print any text there. So a
//! state is read only from the region the engine owns, at the bottom of the
//! pane, never from a phrase anywhere in the transcript:
//!
//! - the input box: the prompt line (`❯` boxed, `$` in the screen-reader
//!   layout) and the status lines around it (model, permission mode, mode
//!   tags);
//! - the activity region: the few lines the engine draws right above the
//!   status lines while a turn runs (its spinner line, the hints of a
//!   running or queued turn, an API error). It ends at a transcript line
//!   (`you:`, `claude:`, `tool:`) and at a finished turn's `· done` line:
//!   what is above those is history.
//!
//! A turn in progress still shows an empty prompt, so an empty prompt alone
//! does not mean that input is safe: the activity region must be idle too.
//!
//! - Busy: a spinner or a running-turn hint in the activity region.
//! - Error: an `API Error` (or a hook error) in the activity region.
//! - Permission: a choice at the prompt position (`$ 1. …`). Every engine
//!   runs with `bypassPermissions`, so no tool-permission dialog is drawn;
//!   the choices of a question the engine asks sit at the prompt.

use serde::{Deserialize, Serialize};

/// Lines kept from the bottom of the pane.
pub const TAIL_LINES: usize = 40;

/// Lines of the activity region, at most.
const ACTIVITY_LINES: usize = 3;

/// Prompt markers of the input box.
///
/// `❯` is the boxed layout. The screen-reader layout draws `$` and keeps the
/// status lines above or below it, so the marker is not always the last line
/// of the pane.
const PROMPTS: [char; 2] = ['❯', '$'];

/// Spinner glyphs of a running turn.
const SPINNERS: [char; 8] = ['✻', '✽', '✳', '✢', '✶', '✷', '✸', '✹'];

/// Marker of a finished turn in the spinner line.
const FINISHED: &str = "· done";

/// Hints of a running turn in the screen-reader layout.
///
/// That layout prints words instead of spinner glyphs (`Nucleating…`, a
/// word that changes; see [`spinner_word`]). Captured from Claude Code
/// v2.1.316.
const SCREEN_READER_BUSY: [&str; 4] = [
    "Running…",
    "Nucleating…",
    "ctrl+x ctrl+s to send now",
    "(ctrl+b ctrl+b",
];

/// Parts of the status lines around the prompt. Captured from v2.1.316.
const STATUS_HINTS: [&str; 3] = [
    "bypass permissions",
    "shift+tab to cycle",
    "? for shortcuts",
];

/// Speaker tags of the transcript in the screen-reader layout: a line with
/// one is history, never the engine's live state.
const SPEAKERS: [&str; 4] = ["you:", "claude:", "tool:", "tool error:"];

/// What the pane shows right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenState {
    /// Input box is empty and no turn runs. Text can be typed safely.
    Ready,
    /// Input box holds text that was not submitted.
    Drafted,
    /// A turn is running. Text typed now would corrupt the prompt.
    Busy,
    /// A permission dialog waits for an answer.
    Permission,
    /// The engine printed an error at the tail.
    Error,
    /// The tail does not match a known layout.
    Unknown,
}

impl ScreenState {
    /// Whether the pane accepts typed text. A finished turn shows `Ready`;
    /// an engine that printed an error above its prompt (`Error`) is at its
    /// prompt too — a model relay's outage leaves it there until the work
    /// goes on — so it accepts text as well (objective 032).
    pub fn accepts_input(self) -> bool {
        matches!(self, Self::Ready | Self::Error)
    }
}

/// Parsed pane tail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenReading {
    /// Detected state.
    pub state: ScreenState,
    /// Trimmed line that decided the state.
    pub evidence: String,
}

impl ScreenReading {
    fn new(state: ScreenState, evidence: &str) -> Self {
        let text: String = evidence.trim().chars().take(200).collect();
        Self {
            state,
            evidence: text,
        }
    }

    /// Unknown with no evidence.
    pub fn unknown() -> Self {
        Self::new(ScreenState::Unknown, "")
    }
}

/// Remove ANSI escape sequences: CSI (`ESC [ ... final`), OSC
/// (`ESC ] ... BEL|ST`) and two-byte escapes.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character != '\u{1b}' {
            out.push(character);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                while let Some(next) = chars.next() {
                    if next == '\u{7}' {
                        break;
                    }
                    if next == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    out
}

fn is_rule(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|character| matches!(character, '─' | '━' | '═' | '╌' | '-' | '—' | '·' | ' '))
}

/// Prompt text after the marker, trimmed.
///
/// `$` needs a following space: pasted shell output contains `$VAR` lines.
fn prompt_body(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    for marker in PROMPTS {
        let Some(rest) = trimmed.strip_prefix(marker) else {
            continue;
        };
        if rest.is_empty() || rest.starts_with(' ') {
            return Some(rest.trim());
        }
    }
    None
}

fn is_choice_line(text: &str) -> bool {
    let first = text.split('.').next().unwrap_or("");
    !first.is_empty() && first.chars().all(|character| character.is_ascii_digit())
}

/// A status line of the input box: the model line (`<model> · …`), the
/// permission mode, a key hint.
fn is_status(line: &str) -> bool {
    let trimmed = line.trim();
    if STATUS_HINTS.iter().any(|hint| trimmed.contains(hint)) {
        return true;
    }
    trimmed
        .split_once(" · ")
        .is_some_and(|(model, _)| !model.is_empty() && !model.contains(char::is_whitespace))
}

/// A mode tag below the status lines (`ultracode`): one lowercase word.
fn is_mode_tag(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|character| character.is_ascii_lowercase())
}

/// The screen-reader spinner: one word with an ellipsis (`Nucleating…`),
/// maybe followed by a parenthesis (`Running…  (16s)`).
fn spinner_word(line: &str) -> bool {
    let Some((word, rest)) = line.split_once('…') else {
        return false;
    };
    let rest = rest.trim();
    !word.is_empty()
        && word.chars().all(char::is_alphabetic)
        && (rest.is_empty() || rest.starts_with('('))
}

fn busy_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.contains("esc to interrupt")
        || SCREEN_READER_BUSY
            .iter()
            .any(|marker| trimmed.contains(marker))
        || spinner_word(trimmed)
    {
        return true;
    }
    let mut chars = trimmed.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    SPINNERS.contains(&first) && (trimmed.contains('…') || trimmed.contains(" for "))
}

/// An error the engine draws itself: an API error, or a hook's failure.
/// A failed tool call is transcript (`tool error:`, `Error: Exit code`).
fn error_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("API Error") || (trimmed.starts_with('⎿') && trimmed.contains("hook error"))
}

/// The activity region above the prompt at `index`, lowest line first: up
/// to [`ACTIVITY_LINES`] lines above the status lines, ending at a
/// transcript line or a finished turn's line.
fn activity_region<'a>(tail: &[&'a str], index: usize) -> Vec<&'a str> {
    let above: Vec<&str> = tail[..index]
        .iter()
        .copied()
        .filter(|line| !is_rule(line) && !line.trim().is_empty())
        .collect();
    let mut end = above.len();
    // The mode tags sit lowest, then the status lines.
    let mut tags = true;
    while end > 0 {
        let line = above[end - 1];
        if tags && is_mode_tag(line) {
            end -= 1;
        } else if is_status(line) {
            tags = false;
            end -= 1;
        } else {
            break;
        }
    }
    let mut region = Vec::new();
    for line in above[..end].iter().rev() {
        let trimmed = line.trim();
        if region.len() == ACTIVITY_LINES
            || trimmed.contains(FINISHED)
            || SPEAKERS.iter().any(|speaker| trimmed.starts_with(speaker))
        {
            break;
        }
        region.push(*line);
    }
    region
}

/// The finished turn's line right above the prompt at `index` (below it
/// only the status lines and mode tags), if the engine drew one there: a
/// spinner glyph, then `· done`.
fn finished_line<'a>(tail: &[&'a str], index: usize) -> Option<&'a str> {
    let line = tail[..index]
        .iter()
        .rev()
        .copied()
        .filter(|line| !is_rule(line) && !line.trim().is_empty())
        .find(|line| !is_status(line) && !is_mode_tag(line))?;
    let trimmed = line.trim();
    (trimmed.starts_with(SPINNERS) && trimmed.contains(FINISHED)).then_some(trimmed)
}

/// Whether a screen reading's evidence is a finished turn's line that says
/// background shells still run (`✻ Sautéed for 6s · done 11:27 PM · 5 shells
/// still running`). The engine sits at its prompt, but it is not idle: it
/// resumes the agent when they finish. Read only from the line the engine
/// draws right above its prompt ([`read_claude_screen`]), never from the
/// transcript.
pub fn shows_background_shells(evidence: &str) -> bool {
    let trimmed = evidence.trim();
    trimmed.starts_with(SPINNERS)
        && trimmed.contains(FINISHED)
        && (trimmed.contains("shells still running") || trimmed.contains("shell still running"))
}

/// The workspace trust dialog's last lines, as Claude Code draws them before
/// its input box. Captured from Claude Code v2.1.317.
const TRUST_FOOTER: [&str; 4] = [
    "y. Yes, I trust this folder",
    "n. No, exit",
    "Enter y/n:",
    "Enter to confirm · Esc to cancel",
];

/// Whether the pane ends with the workspace trust dialog.
///
/// Claude Code asks once per directory whether it trusts the folder, before
/// it draws the input box. `--settings` cannot silence it: trust lives in the
/// user's global state (`~/.claude.json`), not in settings. That state can
/// forget a directory whose session it ran (a host move, a reset or
/// rewritten file, a workspace exported at a new path) while the session's
/// transcript survives, so a resume can show the dialog as well as a fresh
/// start. The dialog is recognized only when its own last lines are the last
/// lines of the pane: a line of history that quotes it (a file the agent
/// printed, redrawn on a resume) is not the dialog. The caller answers it on
/// a fresh start and on a resume alike, before any prompt was drawn, only
/// when two reads agree, and once.
///
/// The dialog reads keystrokes, not pasted text: a bracketed paste of `y`
/// gets "Please answer y or n". Answer it with [`TRUST_KEYS`].
pub fn is_trust_dialog(pane: &str) -> bool {
    let lines: Vec<String> = pane
        .lines()
        .map(|line| strip_ansi(line).trim().to_string())
        .filter(|line| !line.is_empty())
        .collect();
    lines.len() >= TRUST_FOOTER.len() && lines[lines.len() - TRUST_FOOTER.len()..] == TRUST_FOOTER
}

/// Keys that accept the trust dialog.
pub const TRUST_KEYS: [&str; 2] = ["y", "Enter"];

/// Parse the pane tail.
pub fn read_claude_screen(pane: &str) -> ScreenReading {
    let mut stripped: Vec<String> = pane.lines().map(strip_ansi).collect();
    // A tall empty pane pads the bottom with blank rows. Drop them first: the
    // window must cover real text, or a prompt near the top falls outside it.
    while stripped.last().is_some_and(|line| line.trim().is_empty()) {
        stripped.pop();
    }
    let start = stripped.len().saturating_sub(TAIL_LINES);
    let tail: Vec<&str> = stripped[start..].iter().map(String::as_str).collect();
    if tail.is_empty() {
        return ScreenReading::unknown();
    }

    let prompt_index = tail.iter().rposition(|line| prompt_body(line).is_some());
    let Some(index) = prompt_index else {
        return ScreenReading::unknown();
    };
    let prompt = tail[index];

    if let Some(body) = prompt_body(prompt)
        && is_choice_line(body)
    {
        return ScreenReading::new(ScreenState::Permission, prompt);
    }

    let region = activity_region(&tail, index);
    if let Some(line) = region.iter().find(|line| busy_line(line)) {
        return ScreenReading::new(ScreenState::Busy, line);
    }
    if let Some(line) = region.iter().find(|line| error_line(line)) {
        return ScreenReading::new(ScreenState::Error, line);
    }

    match prompt_body(prompt) {
        // A finished turn that left background shells running: the line
        // that says so is the evidence, so the log records that the engine
        // still works ([`shows_background_shells`]).
        Some("") => {
            match finished_line(&tail, index).filter(|line| shows_background_shells(line)) {
                Some(line) => ScreenReading::new(ScreenState::Ready, line),
                None => ScreenReading::new(ScreenState::Ready, prompt),
            }
        }
        Some(_) => ScreenReading::new(ScreenState::Drafted, prompt),
        None => ScreenReading::unknown(),
    }
}

#[cfg(test)]
mod trust_tests {
    use super::{ScreenState, is_trust_dialog, read_claude_screen};

    const DIALOG: &str = "[Screen Reader Mode: on via flag]
Permission Required: Accessing workspace:
/tmp/ws/agent_rsi
Quick safety check: Is this a project you created or one you trust?
Claude Code'll be able to read, edit, and execute files here.
Security guide
y. Yes, I trust this folder
n. No, exit
Enter y/n:
Enter to confirm · Esc to cancel
";

    #[test]
    fn the_trust_dialog_is_recognized_and_not_ready() {
        assert!(is_trust_dialog(DIALOG));
        assert!(!read_claude_screen(DIALOG).state.accepts_input());
    }

    #[test]
    fn the_trust_dialog_is_found_above_blank_rows_of_a_tall_pane() {
        let pane = format!("{DIALOG}{}", "\n".repeat(40));
        assert!(is_trust_dialog(&pane));
    }

    #[test]
    fn a_ready_prompt_after_the_dialog_is_not_the_dialog() {
        let pane =
            format!("{DIALOG}Claude Code v2.1.317\n/tmp/ws/agent_rsi\n$ \nbypass permissions on\n");
        assert!(!is_trust_dialog(&pane));
        assert_eq!(read_claude_screen(&pane).state, ScreenState::Ready);
    }
}

#[cfg(test)]
mod tool_error_tests {
    use super::{ScreenState, read_claude_screen};

    /// A resumed session, captured from v2.1.317: the redrawn transcript ends
    /// with a failed Bash call, and the input box is empty and idle.
    const RESUMED_AFTER_TOOL_ERROR: &str =
        "tool error: Bash (rm -f .spawn/done; cat crates/spawn-core/src/ingest.rs)
Error: Exit code 2
//! Normalization of native engine events into [`EventKind`].
… +267 lines (ctrl+o to expand)
Listed 1 directory (ctrl+o to expand)
bypass permissions on (shift+tab to cycle)
$";

    #[test]
    fn a_failed_tool_call_in_the_transcript_does_not_block_input() {
        assert_eq!(
            read_claude_screen(RESUMED_AFTER_TOOL_ERROR).state,
            ScreenState::Ready
        );
    }

    #[test]
    fn an_api_error_still_reads_as_an_error() {
        let pane = "API Error: 529 overloaded_error\n$ \nbypass permissions on";
        assert_eq!(read_claude_screen(pane).state, ScreenState::Error);
    }
}

#[cfg(test)]
mod background_shell_tests {
    use super::{ScreenState, read_claude_screen, shows_background_shells};

    const STATUS: &str = "  deepseek-ai/DeepSeek-V4.1-Flash · 余量未知 · 249 tok/s";

    /// Live case (task 33): a resident ran clippy, the tests and the e2e in
    /// background shells and sat at its prompt for over 15 minutes. The
    /// reading is Ready (input is safe), and its evidence is the line that
    /// says the shells still run.
    #[test]
    fn a_finished_turn_with_shells_running_is_ready_with_that_line_as_evidence() {
        for line in [
            "✻ Sautéed for 6s · done 11:27 PM · 5 shells still running",
            "✻ Baked for 2m 3s · done 9:01 AM · 1 shell still running",
        ] {
            let reading =
                read_claude_screen(&format!("claude: started them\n{line}\n❯ \n{STATUS}"));
            assert_eq!(reading.state, ScreenState::Ready);
            assert_eq!(reading.evidence, line);
            assert!(shows_background_shells(&reading.evidence));
        }
        let done = read_claude_screen(&format!("✻ Sautéed for 6s · done 11:27 PM\n❯ \n{STATUS}"));
        assert_eq!(done.state, ScreenState::Ready);
        assert!(!shows_background_shells(&done.evidence));
        assert!(!shows_background_shells("❯"));
    }

    /// Text the model printed is transcript: a line that quotes the engine's
    /// above other output is not the engine's.
    #[test]
    fn a_quoted_shells_line_in_the_transcript_is_not_read() {
        let pane = format!(
            "claude: ✻ Sautéed for 6s · done 11:27 PM · 5 shells still running\n❯ \n{STATUS}"
        );
        let reading = read_claude_screen(&pane);
        assert_eq!(reading.state, ScreenState::Ready);
        assert!(!shows_background_shells(&reading.evidence));
    }
}
