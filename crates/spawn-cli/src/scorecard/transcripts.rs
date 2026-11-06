//! The model tokens in the transcript archive (`<root>/.transcripts/<agent>/
//! <native>.jsonl`, copies `<native>.<n>.jsonl`; crate spawn-host,
//! `transcripts`).
//!
//! Claude Code writes one assistant message over several lines, one per
//! content block, each repeating `message.id`, `message.model` and
//! `message.usage`, and a copy file repeats lines of the one before. So a
//! message counts once per (agent, `message.id`), across every line and
//! copy file of the agent: at its earliest line's `timestamp`, its input +
//! cache creation + cache read + output tokens (the largest of its lines',
//! which are the same in practice), under its `message.model`. A line that
//! is not an assistant's, has no usage, no message id or no readable
//! timestamp counts nothing.

use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::Path;

use serde::Deserialize;

use super::time::{DAY_MS, parse_rfc3339};

/// One assistant message's tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Message {
    pub(crate) agent: String,
    pub(crate) id: String,
    pub(crate) at_ms: i64,
    pub(crate) model: String,
    pub(crate) tokens: i64,
}

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    message: Option<LineMessage>,
}

#[derive(Deserialize)]
struct LineMessage {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Usage {
    #[serde(rename = "input_tokens", default)]
    input: Option<u64>,
    #[serde(rename = "cache_creation_input_tokens", default)]
    cache_creation: Option<u64>,
    #[serde(rename = "cache_read_input_tokens", default)]
    cache_read: Option<u64>,
    #[serde(rename = "output_tokens", default)]
    output: Option<u64>,
}

impl Usage {
    fn total(&self) -> i64 {
        let sum = [
            self.input,
            self.cache_creation,
            self.cache_read,
            self.output,
        ]
        .into_iter()
        .flatten()
        .fold(0_u64, u64::saturating_add);
        i64::try_from(sum).unwrap_or(i64::MAX)
    }
}

/// Whether `name` is a plain archive name (letters, digits, `-`, `_`), as
/// the archive itself holds agents to.
fn plain(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

/// Whether `name` is a copy file's: `<native>.jsonl` or `<native>.<n>.jsonl`.
fn copy_file(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".jsonl") else {
        return false;
    };
    match stem.rsplit_once('.') {
        Some((native, copy)) => {
            plain(native) && !copy.is_empty() && copy.bytes().all(|byte| byte.is_ascii_digit())
        }
        None => plain(stem),
    }
}

/// The messages of one line, if it holds one that counts.
fn message_of(bytes: &[u8]) -> Option<(String, i64, String, i64)> {
    // Most lines are not an assistant's usage: skip them unparsed.
    if !contains(bytes, b"\"usage\"") {
        return None;
    }
    let line: Line = serde_json::from_slice(bytes).ok()?;
    if line.kind.as_deref() != Some("assistant") {
        return None;
    }
    let message = line.message?;
    let usage = message.usage?;
    let id = message.id.filter(|id| !id.is_empty())?;
    let at_ms = parse_rfc3339(line.timestamp.as_deref()?)?;
    let model = message.model.unwrap_or_else(|| "unknown".to_string());
    Some((id, at_ms, model, usage.total()))
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Fold one file's lines into `messages`, keyed by message id.
fn read_file(
    path: &Path,
    messages: &mut BTreeMap<String, (i64, String, i64)>,
) -> Result<(), String> {
    let file =
        std::fs::File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        let read = reader
            .read_until(b'\n', &mut bytes)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        if read == 0 {
            return Ok(());
        }
        let Some((id, at_ms, model, tokens)) = message_of(&bytes) else {
            continue;
        };
        messages
            .entry(id)
            .and_modify(|(first_ms, first_model, most)| {
                if at_ms < *first_ms {
                    *first_ms = at_ms;
                    first_model.clone_from(&model);
                }
                *most = (*most).max(tokens);
            })
            .or_insert((at_ms, model, tokens));
    }
}

/// Every message of the archive under `archive` (a `.transcripts`
/// directory), oldest first. A directory that does not exist holds none.
///
/// Files last written more than a day before `not_before_ms` are not read:
/// a line is written when its message is, so such a file holds no line from
/// `not_before_ms` on (the day absorbs a clock apart from the database's).
pub(crate) fn read_archive(archive: &Path, not_before_ms: i64) -> Result<Vec<Message>, String> {
    let agents = match std::fs::read_dir(archive) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("read {}: {error}", archive.display())),
    };
    let mut agent_names: Vec<String> = agents
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| plain(name))
        .collect();
    agent_names.sort();
    let mut out = Vec::new();
    for agent in agent_names {
        let directory = archive.join(&agent);
        let mut files: Vec<String> = std::fs::read_dir(&directory)
            .map_err(|error| format!("read {}: {error}", directory.display()))?
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
            .filter(|entry| written_since(entry, not_before_ms))
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| copy_file(name))
            .collect();
        files.sort();
        let mut messages = BTreeMap::new();
        for file in files {
            read_file(&directory.join(file), &mut messages)?;
        }
        out.extend(
            messages
                .into_iter()
                .map(|(id, (at_ms, model, tokens))| Message {
                    agent: agent.clone(),
                    id,
                    at_ms,
                    model,
                    tokens,
                }),
        );
    }
    out.sort_by(|a, b| (a.at_ms, &a.agent, &a.id).cmp(&(b.at_ms, &b.agent, &b.id)));
    Ok(out)
}

/// Whether the file was last written within a day before `not_before_ms`,
/// or later (or its time cannot be read).
fn written_since(entry: &std::fs::DirEntry, not_before_ms: i64) -> bool {
    let Some(modified) = entry
        .metadata()
        .ok()
        .and_then(|metadata| metadata.modified().ok())
    else {
        return true;
    };
    let modified_ms = match modified.duration_since(std::time::UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_millis()).unwrap_or(i64::MAX),
        Err(before) => -i64::try_from(before.duration().as_millis()).unwrap_or(i64::MAX),
    };
    modified_ms >= not_before_ms.saturating_sub(DAY_MS)
}
