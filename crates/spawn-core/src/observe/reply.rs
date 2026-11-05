//! The model's latest reply, read from its transcript by role.
//!
//! A question for a person (`HUMAN:`) is the model's own words. On the
//! screen a tool's output, or a file the model printed, looks the same as
//! its prose; in the transcript every line names its role, so only the
//! assistant's text blocks count.

use serde_json::Value;

use crate::event::EventKind;
use crate::ingest::claude_line;
use crate::port::READ_LIMIT;

/// Characters of the reply kept: the end of a long reply is where its
/// question is.
const REPLY_CHARS: usize = 16 * 1024;

/// What one transcript line adds to the model's latest reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyPart {
    /// A prompt reached the engine: a reply after it is a new one.
    Prompt,
    /// Text the model wrote, its own words (not a tool's output).
    Text(String),
}

/// What a Claude Code transcript line adds to the model's latest reply: a
/// prompt (a user line that starts a turn) or the text blocks of an
/// assistant line. Sub-agent lines and API errors give nothing.
pub fn claude_reply(line: &str) -> Option<ReplyPart> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    match value.get("type").and_then(Value::as_str)? {
        "user" => claude_line(line, i64::MIN)
            .iter()
            .any(|event| matches!(event.kind, EventKind::MessageReceived))
            .then_some(ReplyPart::Prompt),
        "assistant" if value.get("isApiErrorMessage").and_then(Value::as_bool) != Some(true) => {
            let blocks = value.pointer("/message/content")?.as_array()?;
            let text: Vec<&str> = blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect();
            (!text.is_empty()).then(|| ReplyPart::Text(text.join("\n")))
        }
        _ => None,
    }
}

/// The model's reply since the last prompt, followed through a transcript
/// read piece by piece from where the last read ended.
#[derive(Debug, Default, Clone)]
pub struct Reply {
    offset: u64,
    text: String,
}

impl Reply {
    /// Where the next read of the transcript starts.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Take `bytes` read from [`Self::offset`]. Only whole lines are taken;
    /// a line being written waits for the next read. Returns whether
    /// anything was taken.
    pub fn feed(&mut self, bytes: &[u8]) -> bool {
        let Some(end) = bytes.iter().rposition(|byte| *byte == b'\n') else {
            // A full read without a newline is one line over the limit: skip
            // it rather than stall on it.
            if bytes.len() >= READ_LIMIT {
                self.offset += u64::try_from(bytes.len()).unwrap_or(u64::MAX);
                return true;
            }
            return false;
        };
        for line in bytes[..end].split(|byte| *byte == b'\n') {
            match claude_reply(&String::from_utf8_lossy(line)) {
                Some(ReplyPart::Prompt) => self.text.clear(),
                Some(ReplyPart::Text(text)) => {
                    if !self.text.is_empty() {
                        self.text.push('\n');
                    }
                    self.text.push_str(&text);
                    let count = self.text.chars().count();
                    if count > REPLY_CHARS {
                        self.text = self.text.chars().skip(count - REPLY_CHARS).collect();
                    }
                }
                None => {}
            }
        }
        self.offset += u64::try_from(end + 1).unwrap_or(u64::MAX);
        true
    }

    /// The reply so far, if the model wrote any since the last prompt.
    pub fn text(&self) -> Option<&str> {
        (!self.text.is_empty()).then_some(self.text.as_str())
    }
}
