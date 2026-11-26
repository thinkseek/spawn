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
//! An engine that printed an error above its prompt sits at its prompt and
//! accepts text: the error is what it says about the turn, not a lock on
//! the box.
//!
//! - Busy: a spinner or a running-turn hint in the activity region.
//! - Error: an `API Error` (or a hook error) in the activity region.
//! - Permission: a choice at the prompt position (`$ 1. …`), or the footer
//!   Claude Code 2.1.330 ends a question (`AskUserQuestion`) or a
//!   tool-permission dialog with, as the pane's last lines
//!   ([`is_choice_footer`]).

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
pub(crate) fn strip_ansi(text: &str) -> String {
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
/// Any whitespace counts: Claude Code 2.1.330 draws a draft as `$`, a
/// no-break space (U+00A0), a space, then the text.
fn prompt_body(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    for marker in PROMPTS {
        let Some(rest) = trimmed.strip_prefix(marker) else {
            continue;
        };
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
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
/// maybe followed by a parenthesis (`Running…  (16s)`). The word starts
/// with a letter and may hold an apostrophe or a hyphen (`Beboppin'…`,
/// `Hullaballooing…`, `Topsy-turvying…`).
fn spinner_word(line: &str) -> bool {
    let Some((word, rest)) = line.split_once('…') else {
        return false;
    };
    let rest = rest.trim();
    word.starts_with(char::is_alphabetic)
        && word
            .chars()
            .all(|character| character.is_alphabetic() || matches!(character, '\'' | '’' | '-'))
        && (rest.is_empty() || rest.starts_with('('))
}

/// The running-turn hint Claude Code may draw on a status line.
const INTERRUPT_HINT: &str = "esc to interrupt";

fn busy_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.contains(INTERRUPT_HINT)
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
///
/// A status line that says a turn runs ([`INTERRUPT_HINT`]) comes first:
/// it is checked before the status lines are passed over. Under tmux the
/// engine draws a line of its own (`tmux focus-events off`) between that
/// hint and the status lines below it, so the hint stayed in the region;
/// under Herdr it does not, the hint's line reads as a status line, and
/// passing it over read a running turn as `Ready`.
fn activity_region<'a>(tail: &[&'a str], index: usize) -> Vec<&'a str> {
    let above: Vec<&str> = tail[..index]
        .iter()
        .copied()
        .filter(|line| !is_rule(line) && !line.trim().is_empty())
        .collect();
    let mut end = above.len();
    let mut region = Vec::new();
    // The mode tags sit lowest, then the status lines.
    let mut tags = true;
    while end > 0 {
        let line = above[end - 1];
        if tags && is_mode_tag(line) {
            end -= 1;
        } else if is_status(line) {
            if line.contains(INTERRUPT_HINT) {
                region.push(line);
            }
            tags = false;
            end -= 1;
        } else {
            break;
        }
    }
    let hints = region.len();
    for line in above[..end].iter().rev() {
        let trimmed = line.trim();
        if region.len() - hints == ACTIVITY_LINES
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

/// The start of the footer Claude Code 2.1.330 ends a question
/// (`AskUserQuestion`) or a tool-permission dialog with, in the
/// screen-reader layout: `Select with numbers [1-3]. Then Enter to submit
/// or Escape to cancel:`. Those dialogs draw no `$ 1.` line.
const CHOICE_FOOTER_START: &str = "Select with numbers [";

/// The end of that footer, after the range in brackets.
const CHOICE_FOOTER_END: &str = "]. Then Enter to submit or Escape to cancel:";

/// The key hint a tool-permission dialog may draw after the footer.
const CHOICE_CANCEL_HINT: &str = "Esc to cancel";

/// Whether `line` is the choice footer: its start, a range in brackets,
/// its end, then nothing or the cancel hint.
fn choice_footer_line(line: &str) -> bool {
    let Some(rest) = line.trim().strip_prefix(CHOICE_FOOTER_START) else {
        return false;
    };
    let Some((range, after)) = rest.split_once(CHOICE_FOOTER_END) else {
        return false;
    };
    let after = after.trim();
    !range.trim().is_empty()
        && !range.contains(']')
        && (after.is_empty() || after.contains(CHOICE_CANCEL_HINT))
}

/// The choice footer of a question or a tool-permission dialog, when it is
/// the last non-empty line of the pane, or the line above a last line that
/// is only the cancel hint (`Esc to cancel`, maybe with other key hints).
/// Like the trust dialog ([`is_trust_dialog`]), it is the dialog only at
/// the bottom of the pane: the same words in the transcript, with the
/// prompt and status lines drawn below them, are history.
fn choice_footer<'a>(lines: &[&'a str]) -> Option<&'a str> {
    let mut rows = lines.iter().rev().filter(|line| !line.trim().is_empty());
    let last = rows.next()?;
    if choice_footer_line(last) {
        return Some(last);
    }
    let above = rows.next()?;
    let hint = last.trim();
    (hint.contains(CHOICE_CANCEL_HINT) && hint.len() <= 80 && choice_footer_line(above))
        .then_some(*above)
}

/// Whether the pane ends with the choice footer of a question or a
/// tool-permission dialog Claude Code 2.1.330 draws (`Select with numbers
/// [1-3]. Then Enter to submit or Escape to cancel:`), never the trust
/// dialog: [`read_claude_screen`] reads it as [`ScreenState::Permission`].
pub fn is_choice_footer(pane: &str) -> bool {
    let stripped: Vec<String> = pane.lines().map(strip_ansi).collect();
    let lines: Vec<&str> = stripped.iter().map(String::as_str).collect();
    choice_footer(&lines).is_some()
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

    // A question or a tool-permission dialog of Claude Code 2.1.330: no
    // `$ 1.` line, its footer at the bottom of the pane.
    if let Some(footer) = choice_footer(&tail) {
        return ScreenReading::new(ScreenState::Permission, footer);
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
    // The line nearest the prompt decides which turn it is: a fresh spinner
    // over an older error is a running turn; an error over a spinner left
    // behind is the engine's failed turn, at its prompt (a relay outage
    // leaves the turn's spinner line above the error, objective 032).
    if let Some(line) = region
        .iter()
        .find(|line| busy_line(line) || error_line(line))
    {
        return if busy_line(line) {
            ScreenReading::new(ScreenState::Busy, line)
        } else {
            ScreenReading::new(ScreenState::Error, line)
        };
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

/// The questions the Antigravity CLI (`agy`) trust dialog asks before its
/// prompt box, lowercased. Captured from agy 1.5.14: "Do you trust the
/// contents of this project?", the choice "Yes, I trust this folder", and
/// the key row "↑/↓ Navigate · enter Confirm".
const AGY_TRUST: [&str; 3] = [
    "do you trust the contents of this project?",
    "yes, i trust this folder",
    "enter confirm",
];

/// Keys that accept the agy trust dialog: its first choice (trust) is
/// selected, Enter confirms it.
pub const AGY_TRUST_KEYS: [&str; 1] = ["Enter"];

/// The lowest non-empty lines of a pane, ANSI stripped, trimmed, at most
/// [`TAIL_LINES`] of them, oldest first.
fn tail_lines(pane: &str) -> Vec<String> {
    let lines: Vec<String> = pane
        .lines()
        .map(|line| strip_ansi(line).trim_end().to_string())
        .collect();
    let end = lines
        .iter()
        .rposition(|line| !line.trim().is_empty())
        .map_or(0, |at| at + 1);
    let start = end.saturating_sub(TAIL_LINES);
    lines[start..end].to_vec()
}

/// Whether a line carries an e-mail address (`name@host.tld`): agy draws the
/// signed-in account (and its plan) in its startup banner, which never goes
/// into a reading's evidence or an excerpt Spawn records.
fn holds_email(line: &str) -> bool {
    line.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | '<' | '>' | ',' | ';'))
        .any(|word| {
            let Some((user, host)) = word.split_once('@') else {
                return false;
            };
            !user.is_empty()
                && host.contains('.')
                && !host.starts_with('.')
                && !host.ends_with('.')
                && host
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
                && host.rsplit('.').next().is_some_and(|tld| {
                    tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic())
                })
        })
}

/// The pane without agy's account lines: every line that holds an e-mail
/// address (the startup banner's `▀▀▀▀▀▀ <account> (<plan>)`) is dropped,
/// so neither a reading's evidence nor the excerpt of a pane that never got
/// ready names the account.
pub fn drop_agy_account_lines(pane: &str) -> String {
    let mut kept = String::with_capacity(pane.len());
    for line in pane.lines() {
        if holds_email(&strip_ansi(line)) {
            continue;
        }
        kept.push_str(line);
        kept.push('\n');
    }
    kept
}

/// What agy prints when `--model` names a model it does not know (it then
/// goes on, silently, on its own default, which may be its costliest).
pub const AGY_UNKNOWN_MODEL: &str = "is not recognized as a known model";

/// agy's unknown-model warning line ([`AGY_UNKNOWN_MODEL`]), trimmed and
/// cut to 200 characters, when the pane shows one: the start that shows it
/// fails, never runs on a model nobody chose. Never an account line
/// ([`drop_agy_account_lines`]).
pub fn agy_unknown_model(pane: &str) -> Option<String> {
    drop_agy_account_lines(pane)
        .lines()
        .map(strip_ansi)
        .find(|line| line.to_lowercase().contains(AGY_UNKNOWN_MODEL))
        .map(|line| line.trim().chars().take(200).collect())
}

/// Whether the pane shows agy's workspace trust dialog: its question, its
/// trusting choice and its key row, in its last lines (as Herdr's manifest
/// reads it, `bottom_non_empty_lines(8)`). A prompt box drawn after it is
/// no dialog any more.
pub fn is_agy_trust_dialog(pane: &str) -> bool {
    let lines: Vec<String> = tail_lines(pane)
        .into_iter()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let bottom = &lines[lines.len().saturating_sub(8)..];
    let text = bottom.join("\n").to_lowercase();
    AGY_TRUST.iter().all(|part| text.contains(part)) && agy_prompt_box(&lines).is_none()
}

/// agy's prompt line: `>`, alone or followed by a space and the draft.
fn agy_prompt(line: &str) -> Option<&str> {
    let rest = line.trim().strip_prefix('>')?;
    (rest.is_empty() || rest.starts_with(' ')).then(|| rest.trim())
}

/// Whether `text`, what agy's prompt line holds after its `>`, is agy's own
/// placeholder for an empty prompt: the hint of its permission mode, as
/// agy 1.5.17 draws it after a turn (`Auto mode: edits & safe commands
/// auto-approved (shift+tab to cycle)`), never something typed. A mode's
/// name, `mode:`, its description, and the cycle hint last.
fn agy_mode_placeholder(text: &str) -> bool {
    let Some(described) = text.trim().strip_suffix("(shift+tab to cycle)") else {
        return false;
    };
    let Some((head, description)) = described.split_once(':') else {
        return false;
    };
    let words: Vec<&str> = head.split_whitespace().collect();
    words.len() <= 3
        && words.last().is_some_and(|word| word.eq_ignore_ascii_case("mode"))
        && head.chars().all(|c| c.is_alphabetic() || c == ' ' || c == '-')
        && !description.trim().is_empty()
}

/// The most lines a draft wraps onto below agy's prompt line.
const AGY_WRAPPED_LINES: usize = 8;

/// agy's prompt box in the non-empty `lines`: the index of its prompt line,
/// which a rule bounds above and below. The lowest one.
fn agy_prompt_box(lines: &[String]) -> Option<usize> {
    agy_prompt_span(lines).map(|(at, _)| at)
}

/// agy's prompt box in the non-empty `lines`: its prompt line and its lower
/// rule. A draft longer than the box is wide wraps onto the lines under the
/// prompt line (at most [`AGY_WRAPPED_LINES`]), so the lower rule may be
/// below them (carry list N11). The lowest box.
fn agy_prompt_span(lines: &[String]) -> Option<(usize, usize)> {
    (1..lines.len().saturating_sub(1)).rev().find_map(|at| {
        if agy_prompt(&lines[at]).is_none() || !is_rule(&lines[at - 1]) {
            return None;
        }
        let lower = (at + 1..lines.len().min(at + 2 + AGY_WRAPPED_LINES))
            .find(|&below| is_rule(&lines[below]))?;
        Some((at, lower))
    })
}

/// Whether a footer part counts open background work: `2 tasks`,
/// `1 task`, `1 subagent`, `3 subagents`.
/// agy 1.5.14 writes `1 subagent(s) · 2 task(s)`; `task`, `tasks`,
/// `subagent` and `subagents` are read too.
fn open_work_part(part: &str) -> bool {
    let mut words = part.split_whitespace();
    let (Some(count), Some(noun), None) = (words.next(), words.next(), words.next()) else {
        return false;
    };
    count.parse::<u32>().is_ok_and(|count| count > 0)
        && matches!(
            noun,
            "task" | "tasks" | "task(s)" | "subagent" | "subagents" | "subagent(s)"
        )
}

/// A line of agy's task panel, which it draws between the prompt box and
/// its footer while background work runs: a `●` entry, a `⎿` detail, or
/// an indented continuation.
fn agy_panel_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    !trimmed.is_empty()
        && (trimmed.starts_with('●') || trimmed.starts_with('⎿') || line.starts_with(' '))
}

/// agy's footer below the prompt box whose lower rule is `lower`: the last
/// non-empty line of the pane, past the box's lower rule and an optional
/// task panel with its closing rule. Anything else between them is no
/// layout of agy's, and there is no footer.
fn agy_footer(lines: &[String], lower: usize) -> Option<&str> {
    let (footer, between) = lines.get(lower + 1..)?.split_last()?;
    (!is_rule(footer)
        && between
            .iter()
            .all(|line| is_rule(line) || agy_panel_line(line)))
    .then_some(footer.as_str())
}

/// A braille spinner followed by text (`⠋ Thinking`): a running turn.
fn agy_spinner(line: &str) -> bool {
    let trimmed = line.trim_start();
    let mut chars = trimmed.chars();
    let mut spun = false;
    for character in chars.by_ref() {
        if ('\u{2801}'..='\u{28ff}').contains(&character) {
            spun = true;
        } else {
            return spun && character.is_whitespace() && !chars.as_str().trim().is_empty();
        }
    }
    false
}

/// Parse the tail of an Antigravity CLI (`agy`) pane. Only agy's own UI at
/// the bottom of the pane counts, never the transcript above it:
///
/// - its dialogs (a permission, a question) replace the prompt box and end
///   with a key row (`↑/↓ Navigate · tab Amend …`, `… esc Skip`) in the last
///   two lines: [`ScreenState::Permission`];
/// - the trust dialog, before the prompt box is first drawn, is
///   [`ScreenState::Unknown`], as Claude Code's is: the Bridge answers it
///   ([`is_agy_trust_dialog`]);
/// - a running turn reads `esc to cancel` right under the prompt box, or a
///   braille spinner line above it: [`ScreenState::Busy`];
/// - the prompt box (`>` between two rules) with the `? for shortcuts`
///   footer: [`ScreenState::Ready`] when empty, [`ScreenState::Drafted`]
///   when it holds text. A banner above it (an announcement, a warning, the
///   line of an interrupted turn) is no dialog. The footer is the pane's
///   last line, below the box and an optional task panel (`●` entries and
///   a closing rule, [`agy_footer`]). A footer that counts open background
///   work (`· 1 subagent(s) · 2 task(s) · /tasks`) is [`ScreenState::Busy`]:
///   agy is at its prompt, but the turn goes on when that work reports
///   back.
///
/// The evidence never holds agy's account line ([`drop_agy_account_lines`]).
pub fn read_agy_screen(pane: &str) -> ScreenReading {
    let pane = drop_agy_account_lines(pane);
    let lines: Vec<String> = tail_lines(&pane)
        .into_iter()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let Some(last) = lines.last() else {
        return ScreenReading::unknown();
    };
    let bottom = lines[lines.len().saturating_sub(2)..]
        .join("\n")
        .to_lowercase();
    if bottom.contains("navigate") && (bottom.contains("tab amend") || bottom.contains("esc skip"))
    {
        let row = lines[lines.len().saturating_sub(2)..]
            .iter()
            .find(|line| line.to_lowercase().contains("navigate"))
            .unwrap_or(last);
        return ScreenReading::new(ScreenState::Permission, row);
    }
    if is_agy_trust_dialog(&pane) {
        return ScreenReading::unknown();
    }
    let Some((at, lower)) = agy_prompt_span(&lines) else {
        return ScreenReading::unknown();
    };
    let footer = agy_footer(&lines, lower).unwrap_or_default();
    if footer.trim().starts_with("esc to cancel") {
        return ScreenReading::new(ScreenState::Busy, footer);
    }
    if let Some(spinner) = lines[..at.saturating_sub(1)]
        .iter()
        .rev()
        .take(10)
        .find(|line| agy_spinner(line))
    {
        return ScreenReading::new(ScreenState::Busy, spinner);
    }
    if !footer.contains("? for shortcuts") {
        return ScreenReading::unknown();
    }
    if footer.split('·').any(|part| open_work_part(part.trim())) {
        return ScreenReading::new(ScreenState::Busy, footer);
    }
    match agy_prompt(&lines[at]) {
        // A draft wrapped onto the lines under the prompt line is a draft,
        // whatever its first line holds.
        Some(_) if lower > at + 1 => ScreenReading::new(ScreenState::Drafted, &lines[at]),
        Some("") => ScreenReading::new(ScreenState::Ready, &lines[at]),
        // agy's mode hint in the empty prompt (1.5.17, after a turn).
        Some(text) if agy_mode_placeholder(text) => {
            ScreenReading::new(ScreenState::Ready, &lines[at])
        }
        Some(_) => ScreenReading::new(ScreenState::Drafted, &lines[at]),
        None => ScreenReading::unknown(),
    }
}

#[cfg(test)]
mod agy_tests {
    use super::{
        ScreenState, agy_unknown_model, drop_agy_account_lines, is_agy_trust_dialog,
        is_trust_dialog, read_agy_screen,
    };

    #[test]
    fn agys_unknown_model_warning_is_found_and_names_no_account() {
        let pane = "▀▀▀▀▀▀ someone@example.invalid (Pro)\n\u{1b}[33mWarning: model \"gemini-9\" \
                    is not recognized as a known model; using the default\u{1b}[0m\n>\n";
        let line = agy_unknown_model(pane).expect("the warning");
        assert_eq!(
            line,
            "Warning: model \"gemini-9\" is not recognized as a known model; using the default"
        );
        assert!(!line.contains('@'));
        assert_eq!(agy_unknown_model("──\n>\n──\n? for shortcuts\n"), None);
        assert_eq!(agy_unknown_model(""), None);
    }

    const RULE: &str = "────────────────────────────────────────────────────────";

    /// agy 1.5.14's trust dialog, as captured (the workspace path made up).
    const TRUST: &str = "Accessing workspace:\n\n/tmp/ws/agent_a\n\n\
                         Do you trust the contents of this project?\n\n\
                         Antigravity CLI requires permission to read, edit, and execute files here.\n\n\
                         > Yes, I trust this folder\n  No, exit\n\n  ↑/↓ Navigate · enter Confirm\n";

    /// The startup banner: the logo, the version, a made-up account and
    /// plan (never a real one), the workspace.
    fn banner() -> String {
        "      ▄▀▀▄        Antigravity CLI 1.5.14\n     ▀▀▀▀▀▀       someone@example.invalid (Pro)\n    \
         ▀▀▀▀▀▀▀▀      /tmp/ws/agent_a\n"
            .to_string()
    }

    fn idle_with(above: &str, prompt: &str, footer: &str, below: &str) -> String {
        format!(
            "{}{above}{RULE}\n{prompt}\n{RULE}\n{footer}\n{below}",
            banner()
        )
    }

    #[test]
    fn the_trust_dialog_is_recognized_and_reads_as_nothing_yet() {
        assert!(is_agy_trust_dialog(TRUST));
        assert!(!is_trust_dialog(TRUST), "not Claude Code's dialog");
        let reading = read_agy_screen(TRUST);
        assert_eq!(reading.state, ScreenState::Unknown);
        let padded = format!("{TRUST}{}", "\n".repeat(30));
        assert!(is_agy_trust_dialog(&padded));
        // The prompt box drawn after it: the dialog is history.
        let after = format!("{TRUST}{}", idle_with("", ">", "? for shortcuts", ""));
        assert!(!is_agy_trust_dialog(&after));
        assert_eq!(read_agy_screen(&after).state, ScreenState::Ready);
    }

    /// F6 (O55): agy 1.5.17 draws its permission mode's hint in the empty
    /// prompt after a turn. That is ready, not a draft; a real draft (one
    /// line, wrapped, or text that only mentions the hint) stays drafted.
    #[test]
    fn agys_mode_hint_in_the_empty_prompt_is_ready() {
        let footer = "? for shortcuts            auto · Gemini 3.8 Flash · low";
        let hint = "> Auto mode: edits & safe commands auto-approved (shift+tab to cycle)";
        let pane = idle_with("● Done: wrote the file.\n", hint, footer, "");
        let reading = read_agy_screen(&pane);
        assert_eq!(reading.state, ScreenState::Ready, "{reading:?}");
        assert_eq!(reading.evidence, hint);
        for other in [
            "> Plan mode: read-only, nothing is changed (shift+tab to cycle)",
            "> Default mode: asks before edits (shift+tab to cycle)",
            "  >   Auto mode: edits & safe commands auto-approved (shift+tab to cycle)  ",
        ] {
            let pane = idle_with("", other, footer, "");
            assert_eq!(read_agy_screen(&pane).state, ScreenState::Ready, "{other}");
        }
        for draft in [
            "> fix the bug",
            "> fix the bug (shift+tab to cycle)",
            "> please explain: what does (shift+tab to cycle) do",
            "> Auto mode: (shift+tab to cycle)",
            "> rm -rf mode: x (shift+tab to cycle)",
            "> Auto mode: edits & safe commands auto-approved",
        ] {
            let pane = idle_with("", draft, footer, "");
            assert_eq!(read_agy_screen(&pane).state, ScreenState::Drafted, "{draft}");
        }
        // The hint's text with a second input line under it is a draft.
        let wrapped = idle_with(
            "",
            &format!("{hint}\n  and more of what was typed"),
            footer,
            "",
        );
        assert_eq!(read_agy_screen(&wrapped).state, ScreenState::Drafted);
        // A running turn under the hint is still busy.
        let busy = idle_with("", hint, "esc to cancel", "");
        assert_eq!(read_agy_screen(&busy).state, ScreenState::Busy);
    }

    #[test]
    fn the_idle_prompt_box_is_ready_under_any_banner() {
        let plain = idle_with("", ">", "? for shortcuts", "");
        assert_eq!(read_agy_screen(&plain).state, ScreenState::Ready);
        // An announcement banner above the box is no dialog.
        let announced = idle_with(
            "╭──────────────────────────╮\n│ New: Gemini 4 is here    │\n╰──────────────────────────╯\n\
             ⚠ Warning\n  ⎿  model x is not recognized as a known model. Ignoring the flag.\n",
            ">",
            "? for shortcuts",
            "",
        );
        let reading = read_agy_screen(&announced);
        assert_eq!(reading.state, ScreenState::Ready, "{reading:?}");
        let drafted = idle_with("", "> half a thought", "? for shortcuts", "");
        assert_eq!(read_agy_screen(&drafted).state, ScreenState::Drafted);
    }

    /// agy 1.5.14's footer with open work, as the review of task #576
    /// quotes a real capture (no account in it).
    const OPEN_WORK_FOOTER: &str = "? for shortcuts            auto · Gemini 3.8 Flash · low · \
                                    1 subagent(s) · 2 task(s) · /tasks";

    /// The prompt box, then agy's task panel (`●` entries) and its closing
    /// rule, then `footer`: the layout of agy 1.5.14 while background work
    /// runs. The panel's entries are made up; their layout is agy's.
    fn with_panel(footer: &str, panel: &str) -> String {
        format!("{}{RULE}\n>\n{RULE}\n{panel}{RULE}\n{footer}\n", banner())
    }

    const PANEL: &str = "● Start job A (sleep 25; echo JOB_A_DONE)\n\
                         ● Start job B (sleep 40; echo JOB_B_DONE)\n\
                         ● SSH File Counter (subagent)\n  ⎿ running\n";

    #[test]
    fn open_tasks_or_subagents_keep_the_turn_busy_at_the_prompt() {
        for footer in [
            OPEN_WORK_FOOTER,
            "? for shortcuts            auto · Gemini 3.8 Flash · low · 2 task(s) · /tasks",
            "? for shortcuts            auto · Gemini 3.8 Flash · low · 1 subagent(s) · /tasks",
            "? for shortcuts · 1 task · /tasks",
            "? for shortcuts · 3 subagents · 2 tasks · /tasks",
        ] {
            let pane = with_panel(footer, PANEL);
            let reading = read_agy_screen(&pane);
            assert_eq!(reading.state, ScreenState::Busy, "{footer}");
            assert_eq!(reading.evidence, footer);
            // The panel may be gone already while the footer still counts.
            let bare = idle_with("", ">", footer, "");
            assert_eq!(read_agy_screen(&bare).state, ScreenState::Busy, "{footer}");
        }
        // The panel drawn, the counts at 0 or not there: at the prompt.
        for footer in [
            "? for shortcuts            auto · Gemini 3.8 Flash · low · 0 subagent(s) · 0 task(s) · /tasks",
            "? for shortcuts            auto · Gemini 3.8 Flash · low",
            "? for shortcuts",
        ] {
            let pane = with_panel(footer, PANEL);
            let reading = read_agy_screen(&pane);
            assert_eq!(reading.state, ScreenState::Ready, "{footer}");
            assert_eq!(reading.evidence, ">");
            let empty = with_panel(footer, "");
            assert_eq!(
                read_agy_screen(&empty).state,
                ScreenState::Ready,
                "{footer}"
            );
        }
        // Something below the box that is no panel of agy's: unknown.
        let odd = format!("{RULE}\n>\n{RULE}\nplain text\n{OPEN_WORK_FOOTER}\n");
        assert_eq!(read_agy_screen(&odd).state, ScreenState::Unknown);
        // A footer not drawn yet (the panel's rule last): unknown.
        let unfinished = format!("{RULE}\n>\n{RULE}\n● Start job A\n{RULE}\n");
        assert_eq!(read_agy_screen(&unfinished).state, ScreenState::Unknown);
    }

    #[test]
    fn a_running_turn_is_busy() {
        let footer = idle_with("⠋ Thinking about the change\n", ">", "esc to cancel", "");
        let reading = read_agy_screen(&footer);
        assert_eq!(reading.state, ScreenState::Busy);
        assert_eq!(reading.evidence, "esc to cancel");
        // The spinner alone, before the footer is drawn.
        let spinner = idle_with("⠹ Reading files\n", ">", "? for shortcuts", "");
        assert_eq!(read_agy_screen(&spinner).state, ScreenState::Busy);
        // A braille character in the transcript with nothing after it is no spinner.
        let quoted = idle_with("⠿\n", ">", "? for shortcuts", "");
        assert_eq!(read_agy_screen(&quoted).state, ScreenState::Ready);
    }

    #[test]
    fn permission_and_question_dialogs_wait_for_an_answer() {
        let permission = format!(
            "{}Run command?\n  cargo test\n> 1. Yes\n  2. No\n\
             ↑/↓ Navigate · tab Amend · ctrl+g edit/expand command\nesc to cancel\n",
            banner()
        );
        let reading = read_agy_screen(&permission);
        assert_eq!(reading.state, ScreenState::Permission);
        assert!(reading.evidence.contains("tab Amend"), "{reading:?}");
        let diff = "Edit file?\n↑/↓ Navigate · tab Amend · f full diff\nesc to cancel\n";
        assert_eq!(read_agy_screen(diff).state, ScreenState::Permission);
        let question =
            "Which one?\n> 1. A\n  2. B\n↑/↓ Navigate · enter Select · esc Skip\nesc to cancel\n";
        assert_eq!(read_agy_screen(question).state, ScreenState::Permission);
        // Only the footer rows count: the words quoted higher up are transcript.
        let quoted = idle_with(
            "it said ↑/↓ Navigate · tab Amend\n",
            ">",
            "? for shortcuts",
            "",
        );
        assert_eq!(read_agy_screen(&quoted).state, ScreenState::Ready);
    }

    #[test]
    fn an_interrupted_turn_is_back_at_the_prompt() {
        let pane = idle_with(
            "> do the thing\n  ⎿  Interrupted · What should Antigravity CLI do instead?\n",
            ">",
            "? for shortcuts",
            "",
        );
        assert_eq!(read_agy_screen(&pane).state, ScreenState::Ready);
    }

    #[test]
    fn nothing_known_is_unknown() {
        for pane in ["", "\n\n", "$ ls\nCargo.toml\n", "> no box around me\n"] {
            assert_eq!(
                read_agy_screen(pane).state,
                ScreenState::Unknown,
                "{pane:?}"
            );
        }
    }

    #[test]
    fn the_account_line_never_reaches_evidence_or_excerpts() {
        let pane = idle_with("", "> mail someone@example.invalid", "? for shortcuts", "");
        let reading = read_agy_screen(&pane);
        assert!(!reading.evidence.contains("example.invalid"), "{reading:?}");
        let kept = drop_agy_account_lines(&banner());
        assert!(!kept.contains('@'), "{kept}");
        assert!(kept.contains("Antigravity CLI 1.5.14"), "{kept}");
        assert!(kept.contains("/tmp/ws/agent_a"), "{kept}");
        // Not an address: kept.
        let plain = drop_agy_account_lines("cargo@1.95 a@b\n@scope/pkg\n");
        assert_eq!(plain, "cargo@1.95 a@b\n@scope/pkg\n");
    }

    /// Carry list N11: a draft longer than the box is wide wraps onto the
    /// lines under the prompt line, and still reads `Drafted`, never
    /// `Unknown` or `Ready`. Footer and open-work rules are unchanged.
    #[test]
    fn a_wrapped_draft_reads_drafted() {
        let wrapped = idle_with(
            "",
            "> a long draft that agy wrapped because it is wider than the\n  box it is typed in, onto a second line",
            "? for shortcuts",
            "",
        );
        let reading = read_agy_screen(&wrapped);
        assert_eq!(reading.state, ScreenState::Drafted, "{wrapped}");
        assert!(
            reading.evidence.starts_with("> a long draft"),
            "{reading:?}"
        );
        // The first line may wrap to nothing but the marker.
        let first_empty = idle_with("", ">\n  wrapped text", "? for shortcuts", "");
        assert_eq!(read_agy_screen(&first_empty).state, ScreenState::Drafted);
        // One line still reads as before.
        let one = idle_with("", "> short", "? for shortcuts", "");
        assert_eq!(read_agy_screen(&one).state, ScreenState::Drafted);
        let empty = idle_with("", ">", "? for shortcuts", "");
        assert_eq!(read_agy_screen(&empty).state, ScreenState::Ready);
        // A wrapped draft while a turn runs is still busy.
        let busy = idle_with("", "> queued\n  more", "esc to cancel", "");
        assert_eq!(read_agy_screen(&busy).state, ScreenState::Busy);
        // No lower rule within reach: no box.
        let unbounded = format!("{RULE}\n> text\n{}? for shortcuts\n", "  more\n".repeat(12));
        assert_eq!(read_agy_screen(&unbounded).state, ScreenState::Unknown);
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

    /// The line nearest the prompt decides: a turn that failed leaves its
    /// spinner line above the error, and the engine is at its prompt (its
    /// nudge gets in); a turn started after an older error is running.
    #[test]
    fn the_line_nearest_the_prompt_decides_between_a_running_turn_and_a_failure() {
        let failed = "✻ Working… (esc to interrupt)\nAPI Error: 529 overloaded_error\n\n❯ ";
        assert_eq!(read_claude_screen(failed).state, ScreenState::Error);
        assert!(read_claude_screen(failed).state.accepts_input());
        let running = "API Error: 529 overloaded_error\n✻ Working… (esc to interrupt)\n\n❯ ";
        assert_eq!(read_claude_screen(running).state, ScreenState::Busy);
        assert!(!read_claude_screen(running).state.accepts_input());
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

/// Claude Code 2.1.330 in the screen-reader layout, under tmux and Herdr
/// (PR H3-screen-fix, R2–R4 of the C2 capture gate). The panes are
/// sanitized captures: paths, models and the drafted text made up.
#[cfg(test)]
mod claude_2_1_330_tests {
    use super::{
        ScreenState, activity_region, is_choice_footer, is_trust_dialog, prompt_body,
        read_claude_screen, spinner_word,
    };

    const STATUS: &str = "deepseek-ai/DeepSeek-V4.1-Flash · 余量未知 · 318 tok/s";
    const MODE: &str = "bypass permissions on (shift+tab to cycle)";

    /// R2: a draft is `$`, a no-break space, a space, then the text.
    #[test]
    fn a_draft_after_a_no_break_space_reads_drafted() {
        assert_eq!(prompt_body("$\u{a0} fix the tests"), Some("fix the tests"));
        assert_eq!(prompt_body("❯\u{a0} fix the tests"), Some("fix the tests"));
        assert_eq!(prompt_body("$\u{a0}"), Some(""));
        // Still no prompt: a shell variable.
        assert_eq!(prompt_body("$HOME/bin"), None);
        for pane in [
            format!("{STATUS}\n{MODE}\n$\u{a0} fix the tests"),
            format!("you: hi\nclaude: hello\n{STATUS}\n$\u{a0} fix the tests\n{MODE}\n"),
        ] {
            let reading = read_claude_screen(&pane);
            assert_eq!(reading.state, ScreenState::Drafted, "{pane:?}");
            assert!(reading.evidence.ends_with("fix the tests"), "{reading:?}");
        }
        let empty = format!("{STATUS}\n{MODE}\n$\u{a0}");
        assert_eq!(read_claude_screen(&empty).state, ScreenState::Ready);
    }

    /// R3: a question (`AskUserQuestion`) at the bottom of the pane.
    const QUESTION: &str = "claude: I need a choice.\n\
                            Which database should the service use?\n\
                            1. PostgreSQL\n\
                            2. SQLite\n\
                            3. Type something.\n\
                            Select with numbers [1-3]. Then Enter to submit or Escape to cancel:\n";

    /// R3: a tool-permission dialog at the bottom of the pane.
    const TOOL: &str = "Bash command\n\
                        rm -rf target/tmp\n\
                        Do you want to proceed?\n\
                        1. Yes\n\
                        2. Yes, and don't ask again for rm commands\n\
                        3. No, and tell Claude what to do differently\n\
                        Select with numbers [1-3]. Then Enter to submit or Escape to cancel:\n\
                        Esc to cancel\n";

    #[test]
    fn a_question_or_tool_dialog_footer_is_a_permission() {
        for pane in [
            QUESTION.to_string(),
            TOOL.to_string(),
            format!("{QUESTION}\n\n\n"),
            "Select with numbers [1-2]. Then Enter to submit or Escape to cancel: Esc to cancel\n"
                .to_string(),
        ] {
            assert!(is_choice_footer(&pane), "{pane:?}");
            assert!(!is_trust_dialog(&pane), "{pane:?}");
            let reading = read_claude_screen(&pane);
            assert_eq!(reading.state, ScreenState::Permission, "{pane:?}");
            assert!(
                reading.evidence.starts_with("Select with numbers ["),
                "{reading:?}"
            );
            assert!(!reading.state.accepts_input());
        }
    }

    #[test]
    fn the_footers_words_elsewhere_are_no_permission() {
        // Quoted in the transcript, the input box drawn below: history.
        let quoted = format!(
            "claude: It ends with Select with numbers [1-3]. Then Enter to submit or Escape \
             to cancel:\nSelect with numbers [1-3]. Then Enter to submit or Escape to cancel:\n\
             {STATUS}\n{MODE}\n$ "
        );
        assert!(!is_choice_footer(&quoted));
        assert_eq!(read_claude_screen(&quoted).state, ScreenState::Ready);
        // Ordinary prose as the last line.
        for prose in [
            "Select with numbers or letters, then press Enter.\n",
            "claude: Select with numbers [1-3]. Then Enter to submit or Escape to cancel:\n",
            "Select with numbers []. Then Enter to submit or Escape to cancel:\n",
            "Select with numbers [1-3]. Then Enter to submit or Escape to cancel: and more\n",
            "Select with numbers [1-3]. Then Enter to submit or Escape to cancel:\nplain text\n",
        ] {
            assert!(!is_choice_footer(prose), "{prose:?}");
            assert_ne!(
                read_claude_screen(prose).state,
                ScreenState::Permission,
                "{prose:?}"
            );
        }
        // The trust dialog is no question.
        let trust = "Quick safety check: Is this a project you created or one you trust?\n\
                     y. Yes, I trust this folder\nn. No, exit\nEnter y/n:\n\
                     Enter to confirm · Esc to cancel\n";
        assert!(is_trust_dialog(trust));
        assert!(!is_choice_footer(trust));
        assert_eq!(read_claude_screen(trust).state, ScreenState::Unknown);
    }

    /// R4: spinner words with an apostrophe or a hyphen.
    #[test]
    fn spinner_words_may_hold_an_apostrophe_or_a_hyphen() {
        for line in [
            "Beboppin'…",
            "Beboppin'… (12s)",
            "Beboppin’…",
            "Topsy-turvying…",
            "Nucleating…",
        ] {
            assert!(spinner_word(line), "{line}");
        }
        for line in ["'…", "-…", "…", "two words…", "Beboppin'… and more", "1st…"] {
            assert!(!spinner_word(line), "{line}");
        }
    }

    /// R4: under Herdr the `esc to interrupt` hint sits on a status line
    /// right above the prompt, no `tmux focus-events off` line between them.
    #[test]
    fn an_interrupt_hint_on_a_status_line_is_a_running_turn() {
        let herdr = format!(
            "you: run the tests\nBeboppin'… (12s)\n{STATUS}\n\
             bypass permissions on · esc to interrupt\n$ "
        );
        let tail: Vec<&str> = herdr.lines().collect();
        let region = activity_region(&tail, tail.len() - 1);
        assert_eq!(
            region.first(),
            Some(&"bypass permissions on · esc to interrupt")
        );
        assert_eq!(region.len(), 2, "{region:?}");
        let reading = read_claude_screen(&herdr);
        assert_eq!(reading.state, ScreenState::Busy, "{reading:?}");
        assert!(!reading.state.accepts_input());
        // The hint alone, no spinner line: still running.
        let hint =
            format!("you: run the tests\n{STATUS}\nbypass permissions on · esc to interrupt\n$ ");
        assert_eq!(read_claude_screen(&hint).state, ScreenState::Busy);
        // The spinner alone, no hint: running.
        let spinner = format!("you: run the tests\nBeboppin'…\n{STATUS}\n{MODE}\n$ ");
        assert_eq!(read_claude_screen(&spinner).state, ScreenState::Busy);
        // The turn done, the hint gone: ready.
        let done =
            format!("you: run the tests\n✻ Bebopped for 12s · done 9:20 PM\n{STATUS}\n{MODE}\n$ ");
        assert_eq!(read_claude_screen(&done).state, ScreenState::Ready);
    }
}
