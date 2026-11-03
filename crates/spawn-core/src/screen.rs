//! Reading the tail of a Claude Code pane.
//!
//! Claude Code runs with `--ax-screen-reader`, so its terminal output is
//! plain text without the redrawn boxes of the default renderer. The pane
//! still keeps history, so a match anywhere in the pane can be stale. Every
//! decision here uses only the tail window and prefers the lowest matching
//! line, which is the newest one.
//!
//! The input box is always the bottom region: an `❯` prompt line, a rule
//! line and status lines. Activity signals (spinner, permission dialog,
//! error) appear above the prompt. A turn in progress still shows an empty
//! prompt, so an empty prompt alone does not mean that input is safe: the
//! body must be idle as well.

use serde::{Deserialize, Serialize};

/// Lines kept from the bottom of the pane.
pub const TAIL_LINES: usize = 40;

/// Body lines inspected above the input box.
const BODY_WINDOW: usize = 14;

/// Prompt markers of the input box.
///
/// `❯` is the boxed layout. The screen-reader layout draws `$` and keeps the
/// status lines below it, so the marker is not the last line of the pane.
const PROMPTS: [char; 2] = ['❯', '$'];

/// Spinner glyphs of a running turn.
const SPINNERS: [char; 8] = ['✻', '✽', '✳', '✢', '✶', '✷', '✸', '✹'];

/// Marker of a finished turn in the spinner line.
const FINISHED: &str = "· done";

/// Markers of a running turn in the screen-reader layout.
///
/// That layout prints words instead of spinner glyphs, so the glyph test in
/// [`busy_line`] never fires on it. Captured from Claude Code v2.1.316.
const SCREEN_READER_BUSY: [&str; 4] = [
    "Running…",
    "Nucleating…",
    "ctrl+x ctrl+s to send now",
    "(ctrl+b ctrl+b",
];

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
    /// Whether the pane accepts typed text.
    pub fn accepts_input(self) -> bool {
        matches!(self, Self::Ready)
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

fn permission_line(line: &str) -> bool {
    let trimmed = line.trim();
    if let Some(body) = prompt_body(line) {
        return is_choice_line(body);
    }
    trimmed.starts_with("Do you want to")
        || trimmed.starts_with("Would you like to")
        || (is_choice_line(trimmed) && trimmed.contains(". Yes"))
}

fn busy_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.contains(FINISHED) {
        return false;
    }
    if trimmed.contains("esc to interrupt") {
        return true;
    }
    if SCREEN_READER_BUSY
        .iter()
        .any(|marker| trimmed.contains(marker))
    {
        return true;
    }
    let mut chars = trimmed.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    SPINNERS.contains(&first) && (trimmed.contains('…') || trimmed.contains(" for "))
}

fn error_line(line: &str) -> bool {
    let trimmed = line.trim();
    let marker = trimmed.starts_with('⎿')
        || trimmed.starts_with("Error")
        || trimmed.starts_with("API Error");
    let word = trimmed.contains("error") || trimmed.contains("Error");
    marker && word
}

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

    let body_start = index.saturating_sub(BODY_WINDOW);
    let body: Vec<&str> = tail[body_start..index]
        .iter()
        .copied()
        .filter(|line| !is_rule(line) && !line.trim().is_empty())
        .collect();

    if let Some(line) = body.iter().rev().find(|line| permission_line(line)) {
        return ScreenReading::new(ScreenState::Permission, line);
    }
    if let Some(line) = body.iter().rev().find(|line| busy_line(line)) {
        return ScreenReading::new(ScreenState::Busy, line);
    }
    if body
        .iter()
        .rev()
        .take(3)
        .any(|line| line.contains("esc to interrupt"))
    {
        return ScreenReading::new(ScreenState::Busy, "esc to interrupt");
    }
    if let Some(line) = body.iter().rev().find(|line| error_line(line)) {
        return ScreenReading::new(ScreenState::Error, line);
    }

    match prompt_body(prompt) {
        Some("") => ScreenReading::new(ScreenState::Ready, prompt),
        Some(_) => ScreenReading::new(ScreenState::Drafted, prompt),
        None => ScreenReading::unknown(),
    }
}
