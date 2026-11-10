//! A message into an agent, tracked by its id (docs/fleet-architecture.md
//! §3.4, "Messages").
//!
//! What Spawn types into an engine for a command (an input, a start or a
//! resume with a prompt) is a message, and its id is the command's. The text
//! the Bridge types ends with the command's marker, `[spawn message #<id>]`:
//! on a line of its own after a text of several lines (the workspace notices
//! before the text are part of the same message), and at the end of a text
//! of one line, after a space, so a one-line message stays one line: a pane
//! that reads a paste without bracketed paste takes each line break as
//! Enter, and would submit the marker as a second prompt. The engine's transcript keeps the
//! line it was given, so the transcript's user line that holds the marker is
//! the message taken (`message.taken`, crate::ingest), whatever its text: two
//! messages with the same text are told apart by their ids, never by the
//! text or the order.

use crate::id::CommandId;

/// What opens a marker.
const OPEN: &str = "[spawn message #";

/// The marker of `command`: `[spawn message #<id>]`.
pub fn marker(command: CommandId) -> String {
    format!("{OPEN}{}]", command.0)
}

/// `text` as the Bridge types it for `command`: the text, then its marker,
/// on a line of its own when the text has several, else after a space.
pub fn marked(text: &str, command: CommandId) -> String {
    if text.contains('\n') {
        format!("{text}\n\n{}", marker(command))
    } else {
        format!("{text} {}", marker(command))
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
    fn a_marked_text_ends_with_its_marker_and_one_line_stays_one_line() {
        let text = marked("notice\n\ndo the task", CommandId(42));
        assert_eq!(text, "notice\n\ndo the task\n\n[spawn message #42]");
        assert_eq!(markers(&text), [CommandId(42)]);
        let line = marked("do the task", CommandId(7));
        assert_eq!(line, "do the task [spawn message #7]");
        assert_eq!(markers(&line), [CommandId(7)]);
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
