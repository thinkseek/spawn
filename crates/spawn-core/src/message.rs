//! A message into an agent, tracked by its id (docs/fleet-architecture.md
//! §3.4, "Messages").
//!
//! What Spawn types into an engine for a command (an input, a start or a
//! resume with a prompt) is a message, and its id is the command's. The text
//! the Bridge types ends with the command's marker, `[spawn message #<id>]`,
//! on a line of its own after the text (the workspace notices before the
//! text are part of the same message). A text of one line carries it at the
//! end of that line instead, after a space, whatever the pane reads: a
//! program that reads keys raw takes each pasted line break as Enter (unless
//! it asked for a bracketed paste), and one that reads lines submits each
//! pasted line; either would take a marker on a line of its own as a prompt
//! of its own, and act on the text before the Bridge's Enter. After the
//! marker comes one line that says how to answer the message
//! ([`answer_line`], objective 046); for a text of one line it follows the
//! marker after a space, on the same line. The engine's transcript keeps
//! what it
//! was given, so the transcript's user line that holds the marker is the
//! message taken (`message.taken`, crate::ingest), whatever its text: two
//! messages with the same text are told apart by their ids, never by the
//! text or the order.

use crate::id::CommandId;

/// What opens a marker.
const OPEN: &str = "[spawn message #";

/// The marker of `command`: `[spawn message #<id>]`.
pub fn marker(command: CommandId) -> String {
    format!("{OPEN}{}]", command.0)
}

/// How the agent answers the message `command` (objective 046): one line
/// after the marker. `spawn ack` records the answer from the agent's own
/// pane; a task's delivery is answered by the task's report instead.
pub fn answer_line(command: CommandId) -> String {
    let id = command.0;
    format!(
        "(Answer this message by its id: `spawn ack {id} done`, or `spawn ack {id} declined \
         <why>`; if it delivers a task, your report answers it.)"
    )
}

/// `text` as the Bridge types it for `command`: the text, then its marker on
/// a line of its own, then the line that says how to answer it
/// ([`answer_line`]); after a space each, on the text's own line, when
/// `one_line` (the Bridge passes it for every text of one line) and the text
/// is one line.
pub fn marked(text: &str, command: CommandId, one_line: bool) -> String {
    if one_line && !text.contains('\n') {
        format!("{text} {} {}", marker(command), answer_line(command))
    } else {
        format!("{text}\n\n{}\n{}", marker(command), answer_line(command))
    }
}

/// The commands whose markers `text` holds, in the order they appear, each
/// once.
pub fn markers(text: &str) -> Vec<CommandId> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(OPEN) {
        rest = &rest[at + OPEN.len()..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0
            && rest[digits..].starts_with(']')
            && let Ok(id) = rest[..digits].parse::<u64>()
        {
            let id = CommandId(id);
            if !found.contains(&id) {
                found.push(id);
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_marked_text_ends_with_its_marker_on_its_own_line_or_one_lines_end() {
        let text = marked("do the task", CommandId(42), false);
        assert_eq!(
            text,
            format!(
                "do the task\n\n[spawn message #42]\n{}",
                answer_line(CommandId(42))
            )
        );
        assert_eq!(markers(&text), [CommandId(42)]);
        let several = marked("notice\n\ndo the task", CommandId(43), true);
        assert_eq!(
            several,
            format!(
                "notice\n\ndo the task\n\n[spawn message #43]\n{}",
                answer_line(CommandId(43))
            )
        );
        let line = marked("do the task", CommandId(7), true);
        assert_eq!(
            line,
            format!(
                "do the task [spawn message #7] {}",
                answer_line(CommandId(7))
            )
        );
        assert_eq!(markers(&line), [CommandId(7)]);
    }

    #[test]
    fn the_answer_line_names_the_ack_by_id_and_no_other_marker() {
        let line = answer_line(CommandId(42));
        assert!(!line.contains('\n'), "one line: {line}");
        assert!(line.contains("`spawn ack 42 done`"), "{line}");
        assert!(line.contains("`spawn ack 42 declined <why>`"), "{line}");
        assert!(line.contains("report answers it"), "{line}");
        assert_eq!(markers(&line), [], "the line holds no marker of its own");
    }

    #[test]
    fn markers_are_read_by_id_and_a_malformed_one_is_none() {
        assert_eq!(markers("no marker here"), []);
        assert_eq!(markers("[spawn message #]"), []);
        assert_eq!(markers("[spawn message #12"), []);
        assert_eq!(markers("[spawn message #x1]"), []);
        assert_eq!(
            markers("a\n[spawn message #7]\nb\n[spawn message #9] [spawn message #7]"),
            [CommandId(7), CommandId(9)]
        );
    }
}
