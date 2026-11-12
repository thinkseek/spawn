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
use std::io::{BufRead, Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

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

/// A message's earliest time, its model then, and its most tokens.
type Folded = BTreeMap<String, (i64, String, i64)>;

/// Count one line's message in `messages`: at its earliest time (the model
/// of the first line at that time), with the most tokens of its lines.
fn fold(messages: &mut Folded, id: String, at_ms: i64, model: String, tokens: i64) {
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

/// Fold the lines of `reader` into `messages`: the whole lines into
/// `messages`, and how many bytes they took; a last line with no newline
/// (a line being written) into `tail` instead, so a later read of a longer
/// file starts at that line again.
fn read_lines(
    reader: impl Read,
    path: &Path,
    messages: &mut Folded,
    tail: &mut Folded,
) -> Result<u64, String> {
    let mut reader = std::io::BufReader::new(reader);
    let mut bytes = Vec::new();
    let mut whole = 0_u64;
    loop {
        bytes.clear();
        let read = reader
            .read_until(b'\n', &mut bytes)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        if read == 0 {
            return Ok(whole);
        }
        let ended = bytes.last() == Some(&b'\n');
        if ended {
            whole += u64::try_from(read).unwrap_or(u64::MAX);
        }
        let Some((id, at_ms, model, tokens)) = message_of(&bytes) else {
            continue;
        };
        fold(
            if ended { messages } else { tail },
            id,
            at_ms,
            model,
            tokens,
        );
    }
}

/// How many bytes of a file's start are kept to tell the same file grown
/// from another one written in its place.
const HEAD_BYTES: u64 = 4096;

/// What one archive file held when it was last read: its length, time and
/// first bytes then, how far its whole lines went, and their messages.
struct Kept {
    length: u64,
    modified: Option<SystemTime>,
    head: Vec<u8>,
    whole: u64,
    messages: Folded,
}

/// The files read by each archive's last reading (crate serve's Core
/// lines and the loop's guardrail reading read the archive every few
/// minutes, and it is hundreds of megabytes): a file read again parses
/// only what was added to it since.
static KEPT: Mutex<BTreeMap<PathBuf, BTreeMap<PathBuf, Kept>>> = Mutex::new(BTreeMap::new());

/// What a reading of the archive parsed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Parsed {
    /// Files read from their start.
    pub(crate) whole_files: u64,
    /// Bytes parsed, in all files.
    pub(crate) bytes: u64,
}

/// Fold one file's messages into `messages`: from what `kept` holds of it
/// when the file is the one it read, grown or not (the same first bytes,
/// and longer, or the same length and time), else from its start; `kept`
/// then holds the file as it is now.
fn read_file(
    path: &Path,
    kept: Option<Kept>,
    messages: &mut Folded,
    parsed: &mut Parsed,
) -> Result<Kept, String> {
    let mut file =
        std::fs::File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    let length = metadata.len();
    let modified = metadata.modified().ok();
    let mut head = Vec::new();
    (&mut file)
        .take(HEAD_BYTES)
        .read_to_end(&mut head)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    let same = kept.filter(|kept| {
        head.starts_with(&kept.head)
            && (length > kept.length || (length == kept.length && modified == kept.modified))
    });
    let mut kept = same.unwrap_or_else(|| {
        parsed.whole_files += 1;
        Kept {
            length: 0,
            modified: None,
            head: Vec::new(),
            whole: 0,
            messages: Folded::new(),
        }
    });
    file.seek(std::io::SeekFrom::Start(kept.whole))
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    let mut tail = Folded::new();
    let whole = read_lines(&mut file, path, &mut kept.messages, &mut tail)?;
    parsed.bytes += length.saturating_sub(kept.whole);
    kept.whole += whole;
    kept.length = length;
    kept.modified = modified;
    kept.head = head;
    for (id, (at_ms, model, tokens)) in kept.messages.iter().chain(&tail) {
        fold(messages, id.clone(), *at_ms, model.clone(), *tokens);
    }
    Ok(kept)
}

/// How far back the archive's tokens reach, from one reading of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reach {
    /// The archive holds a file last written more than a day before the
    /// time asked from: it holds lines from before that time, so its tokens
    /// reach back past it.
    Before,
    /// Every file was read: the archive's tokens begin at its oldest
    /// message, or it holds none.
    From(Option<i64>),
}

/// Every message of the archive under `archive` (a `.transcripts`
/// directory), oldest first. A directory that does not exist holds none.
///
/// Files last written more than a day before `not_before_ms` are not read:
/// a line is written when its message is, so such a file holds no line from
/// `not_before_ms` on (the day absorbs a clock apart from the database's).
///
/// And how far back the archive's tokens reach: told from the archive
/// itself, never inferred from a window's lack of calls.
///
/// A file this process read before is parsed only past what it read then
/// ([`KEPT`]): the archive is written by appending, and a file that is not
/// the one read (other first bytes, shorter, or the same length at another
/// time) is read from its start.
pub(crate) fn read_archive(
    archive: &Path,
    not_before_ms: i64,
) -> Result<(Vec<Message>, Reach), String> {
    read_archive_parsed(archive, not_before_ms).map(|(messages, reach, _)| (messages, reach))
}

/// [`read_archive`], and what it parsed.
pub(crate) fn read_archive_parsed(
    archive: &Path,
    not_before_ms: i64,
) -> Result<(Vec<Message>, Reach, Parsed), String> {
    let mut all_kept = KEPT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut kept = all_kept.remove(archive).unwrap_or_default();
    let mut now_kept = BTreeMap::new();
    let mut parsed = Parsed::default();
    let read = read_agents(
        archive,
        not_before_ms,
        &mut kept,
        &mut now_kept,
        &mut parsed,
    );
    // Only the files this reading read are kept: a file gone, or no longer
    // read, is dropped with it.
    all_kept.insert(archive.to_path_buf(), now_kept);
    read.map(|(messages, reach)| (messages, reach, parsed))
}

fn read_agents(
    archive: &Path,
    not_before_ms: i64,
    kept: &mut BTreeMap<PathBuf, Kept>,
    now_kept: &mut BTreeMap<PathBuf, Kept>,
    parsed: &mut Parsed,
) -> Result<(Vec<Message>, Reach), String> {
    let agents = match std::fs::read_dir(archive) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), Reach::From(None)));
        }
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
    let mut older = false;
    for agent in agent_names {
        let directory = archive.join(&agent);
        let mut files: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(&directory)
            .map_err(|error| format!("read {}: {error}", directory.display()))?
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            if !copy_file(&name) {
                continue;
            }
            if written_since(&entry, not_before_ms) {
                files.push(name);
            } else {
                older = true;
            }
        }
        files.sort();
        let mut messages = Folded::new();
        for file in files {
            let path = directory.join(file);
            let file_kept = read_file(&path, kept.remove(&path), &mut messages, parsed)?;
            now_kept.insert(path, file_kept);
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
    let reach = if older {
        Reach::Before
    } else {
        Reach::From(out.first().map(|message| message.at_ms))
    };
    Ok((out, reach))
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
