//! `GET /api/approvals` and `POST /api/approvals/<session_id>/act` (issue
//! #91 P1a): the prompts a person must answer, found deterministically on
//! the live panes, and the one click that answers one.
//!
//! No agent protocol is spoken (no ACP): the engines (Claude Code, Codex
//! CLI, AGY CLI, OpenCode) run in Spawn's terminal host as ever (`tmux` or
//! Herdr, `config::terminal_host`), and what each shows is read from the
//! tail of its pane, as `/api/pane/<id>` reads it, by [`detect`]: a fixed
//! set of text rules, the same for every engine, no model involved. An
//! answer is the named keys (spawn-core `NamedKey`, the digits `digit1` to
//! `digit9` of a numbered choice included) or the text a person could
//! press or type from the agent page, submitted through `spawn_human_key`
//! and `spawn_human_input` as the `spawn_human` login, so every click is a
//! command on the record, `principal` `human`, which the Bridge sends into
//! the pane.
//!
//! The read:
//!
//! ```text
//! { "generated_at_ms": ..., "items": [{
//!     "id": "session_worker_a:3f2c...",   // see below
//!     "kind": "choice" | "permission" | "yes_no" | "confirm" | "attention" | "mission_halted",
//!     "session_id", "agent_id",           // null for a mission's item
//!     "backend": "tmux" | "herdr" | null,
//!     "label": "...",                     // what it asks, in a few words
//!     "summary": "...",                   // its question line, when one is read
//!     "excerpt": "...",                   // the pane's last lines (masked, bounded)
//!     "options": [{"id", "label", "keys": ["digit1"], "text": null, "danger": false,
//!                  "action": "allow" | "allow_always" | "deny" | "keys" | "resume"}],
//!     "judgement": {state, needs_human, attention, reason} | null,
//!     "resume": {"endpoint": "/api/mission/<id>/resume", "halt": <event>},  // mission_halted only
//!     "detected_at_ms": ...
//! }], "warnings": [...] }
//! ```
//!
//! An item's `id` is `<session_id>:<16 hex>`, FNV-1a ([`fnv1a`]) over the
//! prompt's kind, its question, its body and its answers' labels,
//! normalized (whitespace collapsed, case kept: `git branch -D main` and
//! `git branch -d main` are two prompts). The body is what the
//! dialog asks to approve ([`dialog_body`]): in a drawn frame every line
//! from the frame's top to the first answer; without one the lines between
//! the question and the answers and the lines above the question up to a
//! rule, a frame, a spinner, two blank lines or [`BODY_ABOVE`] lines. So
//! the same question with the same answers over `rm -rf target` and over
//! `rm -rf /home/...` are two ids, and a click made for one presses nothing
//! on the other; the same prompt has the same id however its pane's other
//! lines (a spinner, a timer, output above the dialog) or its cursor move,
//! in every build.
//!
//! The prompts [`detect`] finds in the pane's last [`TAIL_LINES`] lines,
//! first found first: numbered choices (`1. Yes` / `❯ 2. Yes, and don't
//! ask again` / `3. No`, at least two, numbered from 1, each its digit key),
//! only as a dialog: the last thing on the pane (at most [`FOOTER_LINES`]
//! lines under it) with a cursor mark on one of them (`❯ › → ▶ ▌`; `●` and
//! `>` begin replies and quotes, so they count only with the next two),
//! inside a drawn frame, or above a key footer (a working spinner's `esc to
//! interrupt` is none); a numbered list an agent wrote is none; permission
//! buttons at the pane's bottom (`Allow once`, `Always
//! allow`/`Allow always`, `Deny`/`Reject`; Enter allows, Esc denies); a
//! yes/no question (`(y/N)`, `[Y/n]`, `(yes/no)`; `y` or `n`, then Enter);
//! a confirm (`Enter to confirm`, `Press Enter`, `Esc to cancel`). An
//! answer's action is read from its label by whole words. A live session
//! whose judgement asks for a person (BLOCKED or WAITING with `needs_human`
//! at least [`ATTENTION_NEEDS_HUMAN`]) with no prompt found is an
//! `attention` item without options (a person types). A halted mission is a
//! `mission_halted` item whose one option, `resume`, is done through its
//! `resume.endpoint` with its `resume.halt`.
//!
//! The engines' key maps (Claude Code's and Codex CLI's digits, OpenCode's
//! Enter / Right Enter / Esc) are checked against hand-written screens of
//! their dialogs (the tests), not yet against live panes of every engine
//! and version: a dialog drawn otherwise may be found as no prompt (then an
//! `attention` item) until a fixture of it is added.
//!
//! The act, `POST /api/approvals/<session_id>/act`: `{"action": "allow" |
//! "allow_always" | "deny" | "keys" | "text", "approval_id"?: "<item id>",
//! "keys"?: [...], "text"?: "...", "key": "<idempotency key>"}` (the key
//! may come in an `Idempotency-Key` header), or `{"probe": true}`, with
//! every guard of `POST /api/session/<id>/input` in its order. `allow`,
//! `allow_always` and `deny` need `approval_id`, the `id` of the item the
//! page showed (400 without): the pane is read again now, and the keys of
//! the answer are pressed only when the prompt showing has that id; 409,
//! nothing pressed, when no prompt shows, another one does (a page not
//! refreshed), or it offers no such answer. `keys` presses the named keys
//! given (1 to [`MAX_KEYS`]), `text` types a text; neither needs an
//! `approval_id`. Each key is its own command (idempotency key
//! `<key>:<n>`), a text one (`<key>`); a retried POST makes none again.
//! 202 `{session_id, action, command_ids, keys, text, principal}`; 404 for
//! a session no agent has used; 409 for one that runs no execution, or a
//! key another request used.

use serde_json::{Value, json};
use spawn_core::command::NamedKey;
use spawn_core::id::{IdempotencyKey, SessionId, TerminalName, WorkspaceId};
use spawn_core::port::TerminalHost;
use tiny_http::{Method, Request};

use super::{
    Access, BAD_REQUEST, Shared, idempotency, json_response, not_allowed, now_ms, session_id,
    typed_text, write_body, writer,
};

/// The pane lines a prompt is looked for in (the bottom of the screen).
pub(super) const TAIL_LINES: usize = 30;
/// The pane lines an item's excerpt keeps, and its bytes at most.
const EXCERPT_LINES: usize = 12;
const EXCERPT_BYTES: usize = 1500;
/// The most keys one act presses.
pub(super) const MAX_KEYS: usize = 8;
/// The judgement's `needs_human` from which a live session with no prompt
/// found is an `attention` item.
pub(super) const ATTENTION_NEEDS_HUMAN: f64 = 0.7;

/// One answer a prompt offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Choice {
    pub(super) id: String,
    pub(super) label: String,
    pub(super) keys: Vec<NamedKey>,
    pub(super) danger: bool,
    /// `allow`, `allow_always`, `deny`, or `keys` for any other.
    pub(super) action: &'static str,
}

/// A prompt found on a pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Prompt {
    pub(super) kind: &'static str,
    pub(super) label: String,
    pub(super) summary: Option<String>,
    pub(super) choices: Vec<Choice>,
    /// The choice the prompt's cursor stands on (its id), when one is drawn.
    pub(super) cursor: Option<String>,
    /// The dialog's body, normalized, one entry a line ([`dialog_body`]):
    /// what it asks to approve (the command, the file, the tool's input),
    /// which its id holds.
    pub(super) body: Vec<String>,
}

impl Choice {
    fn json(&self) -> Value {
        json!({
            "id": self.id, "label": self.label,
            "keys": self.keys.iter().map(|key| key.name()).collect::<Vec<_>>(),
            "text": null, "danger": self.danger, "action": self.action,
        })
    }
}

/// FNV-1a, 64 bits: a hash that is the same in every build and process
/// (`DefaultHasher` promises neither), for ids a page sends back.
pub(super) fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// Whitespace collapsed, case kept: what an id is computed over. Case is
/// kept because it can be the whole difference between two commands
/// (`git branch -D main` and `git branch -d main`), and a pane does not
/// change a line's case while the same prompt shows.
fn normalized(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Prompt {
    /// The prompt's id on `session`: its kind, its question, its body (the
    /// command or input it asks to approve) and its answers' labels,
    /// normalized and hashed ([`fnv1a`]). The rest of the pane (a spinner, a
    /// timer, output above the dialog, where its cursor stands) does not
    /// move it; another prompt, or the same question over another command,
    /// has another.
    pub(super) fn id(&self, session: &str) -> String {
        let mut basis = vec![self.kind.to_string()];
        basis.push(normalized(self.summary.as_deref().unwrap_or(&self.label)));
        basis.push(format!("body {}", self.body.len()));
        basis.extend(self.body.iter().cloned());
        basis.push(format!("answers {}", self.choices.len()));
        basis.extend(self.choices.iter().map(|choice| normalized(&choice.label)));
        format!("{session}:{:016x}", fnv1a(&basis.join("\n")))
    }
}

/// The selection marks an engine draws on its dialog's current choice.
const CURSOR_MARKS: [char; 7] = ['❯', '›', '>', '→', '▶', '●', '▌'];

/// The marks that are a dialog's cursor and nothing else. `>` (a quote, an
/// echoed message) and `●` (a reply's bullet) also begin lines an agent
/// wrote, so they are no evidence of a dialog on their own.
const STRONG_MARKS: [char; 5] = ['❯', '›', '→', '▶', '▌'];

/// The most non-blank lines of a dialog's body read above its question
/// when it has no frame.
const BODY_ABOVE: usize = 8;

/// Whether a line is a working engine's status (a spinner, a timer, `esc
/// to interrupt`): never part of a dialog, its body or its key footer.
fn status_line(line: &str) -> bool {
    let lower = line.to_lowercase();
    has_word(&lower, "to interrupt")
        || bare(line).chars().next().is_some_and(|first| {
            matches!(
                first,
                '✻' | '✶'
                    | '✳'
                    | '✢'
                    | '✽'
                    | '✺'
                    | '✹'
                    | '✷'
                    | '✸'
                    | '◐'
                    | '◓'
                    | '◑'
                    | '◒'
            ) || ('\u{2800}'..='\u{28ff}').contains(&first)
        })
}

/// Whether a line is a drawn rule (`────`, `━━━`): a dialog's top edge when
/// it has no box.
fn rule(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.chars().count() >= 3
        && trimmed
            .chars()
            .all(|character| matches!(character, '─' | '━' | '═' | '-' | '▔' | '▁' | '╌' | '┄'))
}

/// The lines a dialog shows before its first answer at `first`, normalized,
/// blank and status lines left out: in a frame, every line from the
/// frame's top; without one, the lines between its question (`question`)
/// and its answers and the lines above the question up to a rule, a frame,
/// a status line, two blank lines or [`BODY_ABOVE`] lines, whichever comes
/// first. What is above that (output, a spinner) is not the dialog's.
fn dialog_body(lines: &[&str], question: Option<usize>, first: usize) -> Vec<String> {
    let first = first.min(lines.len());
    let top = if lines.get(first).is_some_and(|line| framed(line)) {
        let mut top = first;
        while top > 0 && framed(lines[top - 1]) {
            top -= 1;
        }
        top
    } else {
        let mut top = question.unwrap_or(first).min(first);
        let (mut kept, mut blanks) = (0, 0);
        while top > 0 && kept < BODY_ABOVE {
            let line = lines[top - 1];
            if rule(line) || framed(line) || status_line(line) {
                break;
            }
            if bare(line).is_empty() {
                blanks += 1;
                if blanks == 2 {
                    break;
                }
            } else {
                (kept, blanks) = (kept + 1, 0);
            }
            top -= 1;
        }
        top
    };
    lines[top..first]
        .iter()
        .filter(|line| !status_line(line))
        .map(|line| normalized(bare(line)))
        .filter(|line| !line.is_empty())
        .collect()
}

/// A line without the box-drawing frame engines draw around a dialog
/// (`│ ❯ 1. Yes │`), trimmed; selection marks stay.
fn bare(line: &str) -> &str {
    line.trim_matches(|character: char| {
        character.is_whitespace()
            || matches!(
                character,
                '│' | '┃' | '║' | '|' | '╭' | '╮' | '╰' | '╯' | '─' | '▐' | '█'
            )
    })
}

/// Whether a line is part of a drawn dialog: a box-drawing frame.
fn framed(line: &str) -> bool {
    line.chars()
        .any(|character| matches!(character, '│' | '┃' | '║' | '╭' | '╮' | '╰' | '╯'))
}

/// Whether a line is a dialog's key footer (`Enter to confirm · Esc to
/// cancel`, `↑/↓ to select`).
fn key_footer(line: &str) -> bool {
    if status_line(line) {
        return false;
    }
    let lower = line.to_lowercase();
    lower.contains('↑')
        || lower.contains('↓')
        || [
            "esc",
            "enter",
            "to select",
            "to confirm",
            "to cancel",
            "to navigate",
        ]
        .iter()
        .any(|word| has_word(&lower, word))
}

/// A numbered choice's line: `1. Yes`, `❯ 2) No`, `> 3. Always` → its
/// digit, its label, and the mark before it, if any.
fn numbered(line: &str) -> Option<(u8, String, Option<char>)> {
    let line = bare(line);
    let unmarked = line.trim_start_matches(CURSOR_MARKS).trim_start();
    let cursor = line.chars().next().filter(|_| unmarked.len() < line.len());
    let unmarked = unmarked.trim_start_matches(['○', '*', '-']).trim_start();
    let mut characters = unmarked.chars();
    let digit = characters.next()?.to_digit(10)?;
    let rest = characters.as_str();
    let label = rest
        .strip_prefix('.')
        .or_else(|| rest.strip_prefix(')'))?
        .trim();
    (!label.is_empty() && (1..=9).contains(&digit))
        .then(|| (u8::try_from(digit).unwrap_or(0), clip(label, 120), cursor))
}

fn clip(text: &str, chars: usize) -> String {
    text.chars().take(chars).collect()
}

/// The words of a text, lowercase: letters, digits and apostrophes.
fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|character: char| !(character.is_alphanumeric() || matches!(character, '\'' | '’')))
        .filter(|word| !word.is_empty())
        .map(|word| word.replace('’', "'"))
        .collect()
}

/// Whether `text` (lowercase) holds `phrase` as whole words.
fn has_word(text: &str, phrase: &str) -> bool {
    let text = words(text);
    let phrase = words(phrase);
    !phrase.is_empty()
        && text
            .windows(phrase.len())
            .any(|window| window == phrase.as_slice())
}

/// What an answer's label does, by whole words: `allow_always` (always,
/// don't ask again, allow all, for this session), `deny` (it begins with
/// no, deny, reject, cancel, decline or abort), `allow` (it begins with
/// yes, allow, approve, proceed, accept, ok or okay), else `keys`. "Node
/// 18", "Normal mode", "Okta login", "Runtime only" and "Continue without
/// saving" are none of them.
fn action_of(label: &str) -> &'static str {
    let lower = label.to_lowercase();
    let first = words(&lower).into_iter().next().unwrap_or_default();
    let always = [
        "always",
        "don't ask again",
        "dont ask again",
        "do not ask again",
        "allow all",
        "for this session",
    ]
    .iter()
    .any(|phrase| has_word(&lower, phrase));
    if always {
        "allow_always"
    } else if ["no", "deny", "reject", "cancel", "decline", "abort"].contains(&first.as_str()) {
        "deny"
    } else if ["yes", "allow", "approve", "proceed", "accept", "ok", "okay"]
        .contains(&first.as_str())
    {
        "allow"
    } else {
        "keys"
    }
}

/// Whether an answer grants more than this one step (always, every
/// command, bypassing permissions): the page asks again before it.
fn dangerous(label: &str) -> bool {
    let lower = label.to_lowercase();
    action_of(label) == "allow_always"
        || has_word(&lower, "bypass")
        || has_word(&lower, "skip permissions")
        || has_word(&lower, "dangerously")
}

/// The line that asks, above the prompt's first answer line: the nearest
/// one ending in `?`, else the nearest one naming a permission.
fn question(lines: &[&str], before: usize) -> Option<String> {
    question_at(lines, before).map(|at| clip(bare(lines[at]), 200))
}

/// Where [`question`]'s line is.
fn question_at(lines: &[&str], before: usize) -> Option<usize> {
    let above = &lines[..before.min(lines.len())];
    let asks = |line: &&&str| bare(line).ends_with('?');
    let names = |line: &&&str| {
        let lower = line.to_lowercase();
        [
            "permission",
            "allow",
            "approve",
            "proceed",
            "confirm",
            "want to",
        ]
        .iter()
        .any(|word| has_word(&lower, word))
    };
    let back = |test: &dyn Fn(&&&str) -> bool| {
        above
            .iter()
            .rev()
            .take(8)
            .position(|line| test(&line))
            .map(|from_end| above.len() - 1 - from_end)
    };
    back(&asks)
        .or_else(|| back(&names))
        .filter(|at| !bare(lines[*at]).is_empty())
}

/// The prompt a pane's text shows at its bottom, if any (module doc).
pub(super) fn detect(text: &str) -> Option<Prompt> {
    let all: Vec<&str> = text.lines().collect();
    let mut end = all.len();
    while end > 0 && bare(all[end - 1]).is_empty() {
        end -= 1;
    }
    let lines = &all[end.saturating_sub(TAIL_LINES)..end];
    numbered_prompt(lines)
        .or_else(|| button_prompt(lines))
        .or_else(|| yes_no_prompt(lines))
        .or_else(|| confirm_prompt(lines))
}

/// The lines after `at` that show anything (a frame's edge, a rule and a
/// spinner included).
fn below<'a>(lines: &[&'a str], at: usize) -> Vec<&'a str> {
    lines[at + 1..]
        .iter()
        .copied()
        .filter(|line| !line.trim().is_empty())
        .collect()
}

/// The last run of numbered choices from 1 (at least two, each the next
/// number, other lines between allowed: a choice's second line) that is a
/// dialog, not prose. A dialog is the last thing on its pane: at most
/// [`FOOTER_LINES`] lines below its last choice (a frame's edge, a key
/// footer), and then a cursor mark ([`STRONG_MARKS`]) stands on one of its
/// choices, or the run is inside a drawn frame, or a key footer is below it
/// (a working spinner's `esc to interrupt` is none). A list an agent wrote
/// in its reply (`1. Build 2. Test`, `● 1. ...`, a quoted `> 1. ...`) and
/// left on screen, an idle input box or a spinner under it, is none. Each
/// choice is its digit (`digit1` to `digit9`), wherever the cursor stands.
fn numbered_prompt(lines: &[&str]) -> Option<Prompt> {
    let found: Vec<(usize, u8, String, Option<char>)> = lines
        .iter()
        .enumerate()
        .filter_map(|(at, line)| {
            numbered(line).map(|(digit, label, cursor)| (at, digit, label, cursor))
        })
        .collect();
    let start = found.iter().rposition(|(_, digit, _, _)| *digit == 1)?;
    let mut run = vec![found[start].clone()];
    for next in &found[start + 1..] {
        if next.1 == run.last().map_or(0, |last| last.1) + 1 {
            run.push(next.clone());
        } else {
            break;
        }
    }
    if run.len() < 2 {
        return None;
    }
    let (first, last) = (run[0].0, run[run.len() - 1].0);
    let after = below(lines, last);
    let marked = run
        .iter()
        .any(|(_, _, _, mark)| mark.is_some_and(|mark| STRONG_MARKS.contains(&mark)));
    let evidence = marked
        || lines[first..=last].iter().any(|line| framed(line))
        || after.iter().any(|line| key_footer(line));
    if after.len() > FOOTER_LINES || !evidence {
        return None;
    }
    let choices: Vec<Choice> = run
        .iter()
        .filter_map(|(_, digit, label, _)| {
            Some(Choice {
                id: format!("choice{digit}"),
                label: label.clone(),
                keys: vec![NamedKey::digit(*digit)?],
                danger: dangerous(label),
                action: action_of(label),
            })
        })
        .collect();
    let cursor = run
        .iter()
        .find(|(_, _, _, mark)| mark.is_some())
        .map(|(_, digit, _, _)| format!("choice{digit}"));
    let asked = question_at(lines, first);
    let summary = question(lines, first);
    let permission = choices.iter().any(|choice| choice.action != "keys");
    Some(Prompt {
        kind: if permission { "permission" } else { "choice" },
        label: summary
            .clone()
            .unwrap_or_else(|| format!("{} numbered choices", choices.len())),
        summary,
        choices,
        cursor,
        body: dialog_body(lines, asked, first),
    })
}

/// The most non-blank lines under a numbered dialog's last choice (its
/// frame's bottom, a key footer) for it to count as at the pane's bottom.
const FOOTER_LINES: usize = 3;

/// `Allow once` / `Always allow` / `Deny` buttons on one line or a few, at
/// the pane's bottom (at most [`FOOTER_LINES`] lines under them); a
/// numbered or quoted list line naming them is none.
fn button_prompt(lines: &[&str]) -> Option<Prompt> {
    let at = lines.iter().rposition(|line| {
        let lower = line.to_ascii_lowercase();
        lower.contains("allow once")
            || lower.contains("always allow")
            || lower.contains("allow always")
    })?;
    if numbered(lines[at]).is_some() || below(lines, at).len() > FOOTER_LINES {
        return None;
    }
    let window = lines[at.saturating_sub(2)..(at + 3).min(lines.len())]
        .join(" ")
        .to_ascii_lowercase();
    let mut choices = vec![Choice {
        id: "allow".to_string(),
        label: "Allow once".to_string(),
        keys: vec![NamedKey::Enter],
        danger: false,
        action: "allow",
    }];
    if window.contains("always allow") || window.contains("allow always") {
        choices.push(Choice {
            id: "allow_always".to_string(),
            label: "Always allow".to_string(),
            keys: vec![NamedKey::Right, NamedKey::Enter],
            danger: true,
            action: "allow_always",
        });
    }
    choices.push(Choice {
        id: "deny".to_string(),
        label: if window.contains("reject") {
            "Reject"
        } else {
            "Deny"
        }
        .to_string(),
        keys: vec![NamedKey::Escape],
        danger: false,
        action: "deny",
    });
    let summary = question(lines, at);
    Some(Prompt {
        kind: "permission",
        label: summary
            .clone()
            .unwrap_or_else(|| "a permission request".to_string()),
        summary,
        choices,
        cursor: None,
        body: dialog_body(lines, question_at(lines, at), at),
    })
}

/// A yes/no question: `(y/N)`, `[Y/n]`, `(y/n)`, `(yes/no)` on one of the
/// last lines.
fn yes_no_prompt(lines: &[&str]) -> Option<Prompt> {
    let at = lines.iter().rposition(|line| {
        let lower = line.to_ascii_lowercase().replace(' ', "");
        ["(y/n)", "[y/n]", "(yes/no)", "[yes/no]"]
            .iter()
            .any(|mark| lower.contains(mark))
    })?;
    (lines.len() - at <= 3).then_some(())?;
    let line = clip(bare(lines[at]), 200);
    Some(Prompt {
        kind: "yes_no",
        label: line.clone(),
        summary: Some(line),
        choices: vec![
            Choice {
                id: "yes".to_string(),
                label: "Yes".to_string(),
                keys: vec![NamedKey::Y, NamedKey::Enter],
                danger: false,
                action: "allow",
            },
            Choice {
                id: "no".to_string(),
                label: "No".to_string(),
                keys: vec![NamedKey::N, NamedKey::Enter],
                danger: false,
                action: "deny",
            },
        ],
        cursor: None,
        body: dialog_body(lines, Some(at), at),
    })
}

/// A confirm: `Enter to confirm`, `Press Enter`, `[Enter]`, with `Esc to
/// cancel` or `[Esc]` for its cancel.
fn confirm_prompt(lines: &[&str]) -> Option<Prompt> {
    let at = lines.iter().rposition(|line| {
        let lower = line.to_ascii_lowercase();
        lower.contains("enter to confirm")
            || lower.contains("press enter")
            || lower.contains("[enter]")
            || lower.contains("enter to continue")
            || lower.contains("enter to select")
    })?;
    (lines.len() - at <= 4).then_some(())?;
    let lower = lines[at.saturating_sub(1)..lines.len()]
        .join(" ")
        .to_ascii_lowercase();
    let mut choices = vec![Choice {
        id: "confirm".to_string(),
        label: "Confirm (Enter)".to_string(),
        keys: vec![NamedKey::Enter],
        danger: false,
        action: "allow",
    }];
    if has_word(&lower, "esc") {
        choices.push(Choice {
            id: "cancel".to_string(),
            label: "Cancel (Esc)".to_string(),
            keys: vec![NamedKey::Escape],
            danger: false,
            action: "deny",
        });
    }
    let summary = question(lines, at);
    Some(Prompt {
        kind: "confirm",
        label: summary
            .clone()
            .unwrap_or_else(|| clip(bare(lines[at]), 200)),
        summary,
        choices,
        cursor: None,
        body: dialog_body(lines, question_at(lines, at), at),
    })
}

/// The last lines of a pane, masked, at most [`EXCERPT_LINES`] and
/// [`EXCERPT_BYTES`].
pub(super) fn excerpt(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut end = lines.len();
    while end > 0 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    let kept: Vec<String> = lines[end.saturating_sub(EXCERPT_LINES)..end]
        .iter()
        .map(|line| clip(line.trim_end(), 200))
        .collect();
    let mut out = spawn_core::redact::redact(&kept.join("\n"));
    while out.len() > EXCERPT_BYTES {
        if let Some(cut) = out.find('\n') {
            out.drain(..=cut);
        } else {
            let mut cut = out.len() - EXCERPT_BYTES;
            while !out.is_char_boundary(cut) {
                cut += 1;
            }
            out.drain(..cut);
        }
    }
    out
}

/// What one live session shows, as [`items`] reads it.
#[derive(Debug, Clone, Default)]
pub(super) struct Pane {
    pub(super) session_id: String,
    pub(super) agent_id: String,
    pub(super) backend: Option<&'static str>,
    pub(super) text: String,
    /// `{state, needs_human, attention}` of its judgement, or null.
    pub(super) judgement: Value,
}

/// An attention item's id: the session and a hash of the judge's reason.
fn attention_id(session: &str, reason: &str) -> String {
    format!(
        "{session}:{:016x}",
        fnv1a(&format!("attention\n{}", normalized(reason).to_lowercase()))
    )
}

/// Whether a judgement asks for a person (module doc).
fn asks_a_person(judgement: &Value) -> bool {
    matches!(judgement["state"].as_str(), Some("BLOCKED" | "WAITING"))
        && judgement["needs_human"]
            .as_f64()
            .is_some_and(|needs| needs >= ATTENTION_NEEDS_HUMAN)
}

/// The items of the live `panes` and the `board`'s halted missions (the
/// store's mission board), each detected at `now`.
pub(super) fn items(panes: &[Pane], board: &Value, now: i64) -> Vec<Value> {
    let mut out = Vec::new();
    for pane in panes {
        let excerpt = excerpt(&pane.text);
        let item = |id: String,
                    kind: &str,
                    label: String,
                    summary: Option<String>,
                    options: Vec<Value>| {
            json!({
                "id": id,
                "kind": kind, "session_id": pane.session_id, "agent_id": pane.agent_id,
                "backend": pane.backend, "label": label, "summary": summary,
                "excerpt": excerpt, "options": options, "judgement": pane.judgement,
                "detected_at_ms": now,
            })
        };
        if let Some(prompt) = detect(&pane.text) {
            out.push(item(
                prompt.id(&pane.session_id),
                prompt.kind,
                prompt.label,
                prompt.summary,
                prompt.choices.iter().map(Choice::json).collect(),
            ));
        } else if asks_a_person(&pane.judgement) {
            let reason = pane.judgement["reason"]
                .as_str()
                .map(|reason| clip(reason, 200));
            out.push(item(
                attention_id(&pane.session_id, reason.as_deref().unwrap_or("")),
                "attention",
                reason
                    .clone()
                    .unwrap_or_else(|| "the judge says it waits for a person".to_string()),
                reason,
                Vec::new(),
            ));
        }
    }
    for mission in board["missions"].as_array().into_iter().flatten() {
        let (Some(id), Some(halt)) = (mission["id"].as_i64(), mission["halt"].as_object()) else {
            continue;
        };
        let event = halt.get("event").cloned().unwrap_or(Value::Null);
        let reason = halt
            .get("reason")
            .and_then(Value::as_str)
            .map(|reason| clip(reason, 300));
        out.push(json!({
            "id": format!("mission:{id}:{}", event),
            "kind": "mission_halted", "session_id": null, "agent_id": null, "backend": null,
            "label": format!("mission {id} is halted"),
            "summary": reason, "excerpt": reason.clone().unwrap_or_default(),
            "options": [{
                "id": "resume", "label": "Resume the mission", "keys": [], "text": null,
                "danger": false, "action": "resume",
            }],
            "resume": {"endpoint": format!("/api/mission/{id}/resume"), "halt": event},
            "judgement": null,
            "detected_at_ms": halt.get("at_ms").and_then(Value::as_i64).unwrap_or(now),
        }));
    }
    out
}

/// The backend of this process's terminal host.
fn backend(host: &spawn_host::LocalTerminalHost) -> &'static str {
    match host {
        spawn_host::LocalTerminalHost::Tmux(_) => "tmux",
        spawn_host::LocalTerminalHost::Herdr(_) => "herdr",
    }
}

/// The pane tail of execution `execution` of `agent`, as `/api/pane/<id>`
/// reads it (nothing when it cannot be captured).
fn capture(
    host: &spawn_host::LocalTerminalHost,
    execution: &spawn_core::id::ExecutionId,
    agent: &str,
) -> Result<String, String> {
    let terminal = TerminalName::for_execution(execution);
    let root = crate::env_or("SPAWN_WORKSPACE_ROOT", "/tmp");
    let workspace =
        WorkspaceId::new(format!("{agent}:{root}/{agent}")).map_err(|error| error.to_string())?;
    Ok(host
        .capture_tail(&terminal, &workspace, TAIL_LINES + 10)
        .unwrap_or_default())
}

/// The tail of `session`'s live pane; `None` when it runs no execution.
fn live_tail(
    store: &spawn_postgres::PostgresStore,
    host: &spawn_host::LocalTerminalHost,
    session: &SessionId,
    agent: &str,
) -> Result<Option<String>, String> {
    let latest = store
        .executions(session)
        .map_err(|error| error.to_string())?
        .into_iter()
        .next();
    let Some(execution) = latest.filter(|execution| execution.ended_at.is_none()) else {
        return Ok(None);
    };
    capture(host, &execution.execution_id, agent).map(Some)
}

/// The live panes: the fleet board's live sessions (one read), each pane
/// captured through the terminal host.
fn panes(
    store: &spawn_postgres::PostgresStore,
    warnings: &mut Vec<String>,
) -> Result<Vec<Pane>, String> {
    let host = crate::config::terminal_host()?;
    let board = store.fleet_board().map_err(|error| error.to_string())?;
    let mut out = Vec::new();
    for row in board.as_array().into_iter().flatten() {
        let (Some(session), Some(agent), Some(execution)) = (
            row["session_id"].as_str(),
            row["agent_id"].as_str(),
            row["execution_id"].as_str(),
        ) else {
            continue;
        };
        if row["live"] != true {
            continue;
        }
        let tail = spawn_core::id::ExecutionId::new(execution)
            .map_err(|error| error.to_string())
            .and_then(|execution| capture(&host, &execution, agent));
        let tail = match tail {
            Ok(tail) => tail,
            Err(error) => {
                warnings.push(format!("approvals: {session}: {error}"));
                continue;
            }
        };
        let observation = &row["judgement"];
        out.push(Pane {
            session_id: session.to_string(),
            agent_id: agent.to_string(),
            backend: Some(backend(&host)),
            text: tail,
            judgement: if observation.is_object() {
                json!({
                    "state": observation["state"], "needs_human": observation["needs_human"],
                    "attention": observation["attention"], "reason": observation["reason"],
                })
            } else {
                Value::Null
            },
        });
    }
    Ok(out)
}

/// `GET /api/approvals` (module doc).
pub(super) fn approvals() -> Result<Value, String> {
    let store = super::open_store()?;
    let mut warnings = Vec::new();
    let panes = panes(&store, &mut warnings)?;
    let board = store.mission_board(50).unwrap_or_else(|error| {
        warnings.push(format!("approvals: the missions: {error}"));
        Value::Null
    });
    let now = now_ms();
    Ok(json!({
        "items": items(&panes, &board, now),
        "generated_at_ms": now,
        "warnings": warnings,
    }))
}

/// The prompt `session` shows now, read again for an act.
fn prompt_now(session: &SessionId) -> Result<Option<Prompt>, String> {
    let store = super::open_store()?;
    let agent = store
        .state(session)
        .map_err(|error| error.to_string())?
        .map(|state| state.agent_id.to_string())
        .ok_or_else(|| format!("no agent has used {}", session.as_str()))?;
    let host = crate::config::terminal_host()?;
    Ok(live_tail(&store, &host, session, &agent)?.and_then(|tail| detect(&tail)))
}

/// The body of `POST /api/approvals/<session_id>/act`.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ActBody {
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    approval_id: Option<String>,
    #[serde(default)]
    keys: Option<Vec<String>>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    probe: bool,
}

/// What a person asked an act to do, from its body.
#[derive(Debug, Clone, Default)]
pub(super) struct Asked {
    pub(super) action: String,
    /// The `id` of the item the page showed (`GET /api/approvals`): needed
    /// for an answer of the prompt (`allow`, `allow_always`, `deny`).
    pub(super) approval_id: Option<String>,
    pub(super) keys: Option<Vec<String>>,
    pub(super) text: Option<String>,
}

/// What an act sends: keys, or a text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Send {
    Keys(Vec<NamedKey>),
    Text(String),
}

/// `POST /api/approvals/<session_id>/act`, past the read guard.
pub(super) fn act(
    mut request: Request,
    id: &str,
    access: Access,
    shared: &Shared,
) -> Result<(), String> {
    if *request.method() != Method::Post {
        return request
            .respond(not_allowed("POST"))
            .map_err(|error| error.to_string());
    }
    let (status, body) = match acted(&mut request, id, access, shared, &prompt_now) {
        Ok(answer) => answer,
        Err((status, error)) => (status, json!({ "error": error })),
    };
    request
        .respond(json_response(status, &body))
        .map_err(|error| error.to_string())
}

/// What `asked` sends, `prompt` reading the pane when the action names an
/// answer of its prompt: that answer is pressed only when the prompt
/// showing now is the item the page showed (its `approval_id`), so a click
/// on a prompt since answered or replaced presses nothing (409).
pub(super) fn what_to_send(
    asked: Asked,
    session: &SessionId,
    prompt: &dyn Fn(&SessionId) -> Result<Option<Prompt>, String>,
) -> Result<Send, (u16, String)> {
    let bad = |why: String| (400, format!("{BAD_REQUEST}{why}"));
    let Asked {
        action,
        approval_id,
        keys,
        text,
    } = asked;
    let action = action.as_str();
    match action {
        "allow" | "allow_always" | "deny" => {
            if keys.is_some() || text.is_some() {
                return Err(bad(format!("{action} carries no keys and no text")));
            }
            let wanted = approval_id.ok_or_else(|| {
                bad(format!(
                    "{action} names the approval it answers (\"approval_id\", the item's id \
                     from GET /api/approvals)"
                ))
            })?;
            let shown = prompt(session)
                .map_err(|error| (500, error))?
                .ok_or_else(|| {
                    (
                        409,
                        format!(
                            "no approval prompt shows on {} now: nothing to {action}",
                            session.as_str()
                        ),
                    )
                })?;
            let now = shown.id(session.as_str());
            if now != wanted {
                return Err((
                    409,
                    format!(
                        "the prompt on {} is not approval {wanted} any more (now {now}): \
                         nothing pressed; read GET /api/approvals again",
                        session.as_str()
                    ),
                ));
            }
            let choice = shown
                .choices
                .iter()
                .find(|choice| choice.action == action)
                .ok_or_else(|| {
                    (
                        409,
                        format!(
                            "the prompt on {} offers no {action} answer",
                            session.as_str()
                        ),
                    )
                })?;
            Ok(Send::Keys(choice.keys.clone()))
        }
        "keys" => {
            if text.is_some() {
                return Err(bad("keys carries no text".to_string()));
            }
            let keys = keys.unwrap_or_default();
            if keys.is_empty() || keys.len() > MAX_KEYS {
                return Err(bad(format!("keys names 1 to {MAX_KEYS} keys")));
            }
            keys.iter()
                .map(|name| NamedKey::from_name(name).map_err(&bad))
                .collect::<Result<Vec<_>, _>>()
                .map(Send::Keys)
        }
        "text" => {
            if keys.is_some() {
                return Err(bad("text carries no keys".to_string()));
            }
            typed_text(text).map(Send::Text).map_err(bad)
        }
        other => Err(bad(format!(
            "action {other:?} is not one of allow, allow_always, deny, keys, text"
        ))),
    }
}

/// The checks of an act after its method, then the commands.
pub(super) fn acted(
    request: &mut Request,
    id: &str,
    access: Access,
    shared: &Shared,
    prompt: &dyn Fn(&SessionId) -> Result<Option<Prompt>, String>,
) -> Result<(u16, Value), (u16, String)> {
    let (body, header_key) = write_body(request, access, shared, "an act")?;
    let body: ActBody = serde_json::from_slice(&body)
        .map_err(|error| (400, format!("{BAD_REQUEST}the body is not an act: {error}")))?;
    let session = session_id(id).map_err(|error| (400, error))?;
    if body.probe {
        if body.action.is_some()
            || body.approval_id.is_some()
            || body.keys.is_some()
            || body.text.is_some()
            || body.key.is_some()
            || header_key.is_some()
        {
            return Err((
                400,
                format!("{BAD_REQUEST}a probe carries no action, keys, text or key"),
            ));
        }
        writer(shared).map_err(|error| (500, error))?;
        return Ok((
            200,
            json!({ "writable": true, "session_id": session.as_str(),
                    "actions": ["allow", "allow_always", "deny", "keys", "text"],
                    "keys": NamedKey::ALL.iter().map(|key| key.name()).collect::<Vec<_>>(),
                    "max_keys": MAX_KEYS, "max_text_bytes": super::MAX_INPUT_TEXT }),
        ));
    }
    let key = idempotency(body.key, header_key, "an act")?;
    if key.as_str().len() > spawn_postgres::MAX_MASTER_KEY_BYTES {
        return Err((
            400,
            format!(
                "{BAD_REQUEST}an act's idempotency key is at most {} bytes",
                spawn_postgres::MAX_MASTER_KEY_BYTES
            ),
        ));
    }
    let action = body
        .action
        .ok_or_else(|| (400, format!("{BAD_REQUEST}an act names its action")))?;
    let asked = Asked {
        action: action.clone(),
        approval_id: body.approval_id,
        keys: body.keys,
        text: body.text,
    };
    let sending = what_to_send(asked, &session, prompt)?;
    let store = writer(shared).map_err(|error| (500, error))?;
    let no_agent = || {
        (
            404,
            format!(
                "no agent has used {}: there is nothing to answer",
                session.as_str()
            ),
        )
    };
    let mut ids = Vec::new();
    match &sending {
        Send::Keys(keys) => {
            for (at, name) in keys.iter().enumerate() {
                let derived = IdempotencyKey::new(format!("{}:{}", key.as_str(), at + 1))
                    .map_err(|error| (400, format!("{BAD_REQUEST}the key: {error}")))?;
                match store.submit_human_key(&session, &derived, *name) {
                    Ok(spawn_postgres::HumanKey::Submitted(command)) => ids.push(command.0),
                    Ok(spawn_postgres::HumanKey::NoAgent) => return Err(no_agent()),
                    Ok(
                        spawn_postgres::HumanKey::NotRunning(why)
                        | spawn_postgres::HumanKey::KeyUsed(why),
                    ) => return Err((409, why)),
                    Err(error) => return Err((500, error.to_string())),
                }
            }
        }
        Send::Text(text) => match store.submit_human_input(&session, &key, text) {
            Ok(spawn_postgres::HumanInput::Submitted(command)) => ids.push(command.0),
            Ok(spawn_postgres::HumanInput::NoAgent) => return Err(no_agent()),
            Ok(
                spawn_postgres::HumanInput::NotRunning(why)
                | spawn_postgres::HumanInput::KeyUsed(why),
            ) => return Err((409, why)),
            Err(error) => return Err((500, error.to_string())),
        },
    }
    let (keys, text) = match sending {
        Send::Keys(keys) => (
            json!(keys.iter().map(|key| key.name()).collect::<Vec<_>>()),
            Value::Null,
        ),
        Send::Text(text) => (json!([]), json!(text)),
    };
    Ok((
        202,
        json!({ "session_id": session.as_str(), "action": action, "command_ids": ids,
                "keys": keys, "text": text, "principal": "human" }),
    ))
}

#[cfg(test)]
mod tests;
