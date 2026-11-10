//! What engines write, turned into [`EventKind`]s.
//!
//! The Bridge follows the files an engine writes anyway, through the
//! terminal host, so nothing depends on the engine reporting to Spawn:
//!
//! Claude Code's session transcript, one JSON object per line
//! ([`claude_line`]).
//!
//! Lines that carry no Spawn meaning give nothing, so a new engine release
//! adds no failure mode. Events carry a key the engine made unique (the
//! line's `uuid`, the `tool_use_id`, a background job's id with the call
//! that started it or a digest of the notification that ended it), so
//! reading a file again appends nothing twice.

use serde_json::Value;

use crate::event::{EventKind, JobEnd, JobKind};

/// Maximum characters kept from engine error text.
const MAX_ERROR_CHARS: usize = 2000;

/// Normalized engine event.
#[derive(Debug, Clone, PartialEq)]
pub struct Normalized {
    /// Unified event.
    pub kind: EventKind,
    /// Producer key for idempotent appends.
    pub source_event_id: Option<String>,
}

fn truncate(text: &str) -> String {
    text.chars().take(MAX_ERROR_CHARS).collect()
}

fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => truncate(text),
        Value::Array(blocks) => truncate(
            &blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        Value::Null => String::new(),
        other => truncate(&other.to_string()),
    }
}

/// A time with a `+HH:MM`/`-HH:MM` zone: the time without it, and the
/// zone's offset from UTC in milliseconds.
fn numeric_zone(text: &str) -> Option<(&str, i64)> {
    let split = text.len().checked_sub(6)?;
    let (text, zone) = (text.get(..split)?, text.get(split..)?);
    let sign = match zone.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let (hours, minutes) = zone.get(1..)?.split_once(':')?;
    let (hours, minutes) = (hours.parse::<i64>().ok()?, minutes.parse::<i64>().ok()?);
    if !(0..=23).contains(&hours) || !(0..=59).contains(&minutes) {
        return None;
    }
    Some((text, sign * (hours * 60 + minutes) * 60_000))
}

/// Milliseconds since the epoch of an RFC 3339 time such as
/// `2026-11-04T05:46:12.345Z` (the form Claude Code writes) or one with a
/// numeric offset, `2026-11-04T13:46:12.345+08:00`.
///
/// Total: any text that is not such a time, a field out of its range
/// (years 1 to 9999), gives `None`, never a panic or a wrapped value; the
/// text may come from a file an engine writes.
pub fn utc_ms(text: &str) -> Option<i64> {
    let (text, offset_ms) = match text.strip_suffix('Z') {
        Some(text) => (text, 0),
        None => numeric_zone(text)?,
    };
    let (date, time) = text.split_once('T')?;
    let mut date = date.splitn(3, '-').map(str::parse::<i64>);
    let (year, month, day) = (date.next()?.ok()?, date.next()?.ok()?, date.next()?.ok()?);
    let (clock, fraction) = time.split_once('.').unwrap_or((time, "0"));
    let mut clock = clock.splitn(3, ':').map(str::parse::<i64>);
    let (hour, minute, second) = (
        clock.next()?.ok()?,
        clock.next()?.ok()?,
        clock.next()?.ok()?,
    );
    let millis: i64 = format!("{fraction:0<3}").get(..3)?.parse().ok()?;
    if !(1..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&second)
    {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let shifted = if month <= 2 { year - 1 } else { year };
    let era = shifted.div_euclid(400);
    let year_of_era = shifted - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some((((days * 24 + hour) * 60 + minute) * 60 + second) * 1000 + millis - offset_ms)
}

/// The longest summary a `tool.started` keeps.
pub const TOOL_SUMMARY_CHARS: usize = 160;

/// `text` shortened to [`TOOL_SUMMARY_CHARS`] characters, an ellipsis
/// marking a cut.
fn summary_cut(text: &str) -> String {
    if text.chars().count() <= TOOL_SUMMARY_CHARS {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(TOOL_SUMMARY_CHARS - 1).collect();
    cut.push('…');
    cut
}

/// A string field of a tool call's input, trimmed and not empty.
fn input_text<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

/// A tool call's key input in a few words, read from its `input` by tool,
/// held to words that cannot be a credential ([`plain_word`]): the store
/// must never keep a secret, and a list of secret shapes is never complete.
///
/// - Bash: the leading words of its first command, after its set-up (`cd
///   <dir> &&`, `NAME=value;`), at most [`COMMAND_WORDS`], stopping at the
///   first flag, the first word holding `@ : = ' " ` $ \`, the first long
///   mix of letters and digits, and the command's end (`&&`, `|`, `;`):
///   `cargo test -p spawn-cli` keeps `cargo test …`. A variable the set-up
///   gave a program's path reads as that program (`$B` after
///   `B=/…/bin/spawn` is `spawn`).
/// - Edit, MultiEdit, Write, NotebookEdit, Read: the file's path up to its
///   first segment that is not a plain word (a Read with its line range).
/// - Grep: its pattern, when every word of it is plain, and where; Glob:
///   its pattern and where, bounded like a path.
/// - Task: its description's first words; WebSearch: its query's; both
///   bounded like a command.
/// - WebFetch: the URL's host alone, never its path (a bot's token, a
///   webhook's secret).
/// - TodoWrite: the count of its items.
///
/// None for any other tool or an input without the field. The redactor
/// still runs on what is kept, then the cut to [`TOOL_SUMMARY_CHARS`], and
/// the event's redaction runs again when it is stored.
pub fn tool_summary(tool: &str, input: Option<&Value>) -> Option<String> {
    let (text, _) = summary_text(tool, input?)?;
    Some(summary_cut(&crate::redact::redact(&text)))
}

/// The most words a summary keeps of a command, a description or a query.
pub const COMMAND_WORDS: usize = 4;

/// A word at least this long that mixes letters and digits can be a token.
const WORD_MIX_CHARS: usize = 8;

/// A path's segment (or a host's label) at least this long that mixes
/// letters and digits can be a token; shorter mixed segments are file and
/// crate names (`x86_64`, `sha256sum`, `spawn-v2`).
const SEGMENT_MIX_CHARS: usize = 16;

/// Programs whose arguments are what they print, set or pass on (`echo`,
/// `export`, `sudo`, `sshpass`, `env`): a summary keeps the program alone.
const PROGRAM_ONLY: [&str; 11] = [
    "echo", "printf", "read", "sudo", "su", "sshpass", "passwd", "export", "eval", "set", "env",
];

/// The characters a kept word never holds: a userinfo, an assignment, a
/// quote, an expansion.
const NEVER_KEPT: &str = "@:='\"`$\\";

/// What a summary puts where it left words out.
const MORE: &str = "…";

fn mixes_letters_and_digits(word: &str) -> bool {
    word.chars().any(|c| c.is_ascii_alphabetic()) && word.chars().any(|c| c.is_ascii_digit())
}

fn short_or_unmixed(part: &str, limit: usize) -> bool {
    part.chars().count() < limit || !mixes_letters_and_digits(part)
}

/// Can `word` be stored? Not a flag (the word after a flag may be its
/// value, and `-pSECRET` is both), not a word holding any of
/// [`NEVER_KEPT`], and not a mix of letters and digits [`WORD_MIX_CHARS`]
/// long or longer (a token), except in a path, whose segments are held to
/// [`SEGMENT_MIX_CHARS`].
///
/// What this lets through, by design: a word shorter than
/// [`WORD_MIX_CHARS`], or all letters, or all digits, in a positional slot
/// among a command's first words (`git checkout abc1234`, `mysql app`). A
/// secret of that shape there would be stored; the bound narrows what a
/// secret can look like, it does not read meaning.
fn plain_word(word: &str) -> bool {
    if word.is_empty() || word.starts_with('-') {
        return false;
    }
    if word.chars().any(|c| NEVER_KEPT.contains(c)) {
        return false;
    }
    if word.contains('/') {
        word.split('/')
            .all(|segment| short_or_unmixed(segment, SEGMENT_MIX_CHARS))
    } else {
        short_or_unmixed(word, WORD_MIX_CHARS)
    }
}

/// Can a path's segment be stored? As a word, but held to
/// [`SEGMENT_MIX_CHARS`].
fn plain_segment(segment: &str) -> bool {
    !segment.starts_with('-')
        && !segment.chars().any(|c| NEVER_KEPT.contains(c))
        && short_or_unmixed(segment, SEGMENT_MIX_CHARS)
}

/// Can a host be stored? Like a path: no flag, nothing of [`NEVER_KEPT`],
/// each label short or unmixed (`ec2-54-12-1-2.compute.amazonaws.com`).
fn plain_host(host: &str) -> bool {
    !host.is_empty()
        && !host.starts_with('-')
        && !host.chars().any(|c| NEVER_KEPT.contains(c))
        && host
            .split('.')
            .all(|label| short_or_unmixed(label, SEGMENT_MIX_CHARS))
}

/// Up to `most` leading plain words of `text`, and whether that is all of
/// it (nothing left out, no `…`).
fn plain_words(text: &str, most: usize) -> (String, bool) {
    let mut kept = Vec::new();
    let mut words = text.split_whitespace().peekable();
    while let Some(word) = words.peek() {
        if kept.len() == most || !plain_word(word) {
            break;
        }
        kept.push(*word);
        words.next();
    }
    let complete = words.peek().is_none();
    (with_more(&kept.join(" "), !complete), complete)
}

fn with_more(kept: &str, more: bool) -> String {
    match (kept.is_empty(), more) {
        (true, _) => MORE.to_string(),
        (false, true) => format!("{kept} {MORE}"),
        (false, false) => kept.to_string(),
    }
}

/// A path up to its first segment that is not a plain word, and whether
/// that is all of it.
fn plain_path(path: &str) -> (String, bool) {
    let mut kept = Vec::new();
    for (index, segment) in path.split('/').enumerate() {
        // The empty segment of a leading `/` (or of `//`) is no word.
        if !(segment.is_empty() || plain_segment(segment)) {
            let lead = kept.join("/");
            let shown = if index == 0 {
                MORE.to_string()
            } else {
                format!("{lead}/{MORE}")
            };
            return (shown, false);
        }
        kept.push(segment);
    }
    (path.to_string(), true)
}

/// The length of a shell value at the start of `text`: a quoted string, a
/// `$(…)` (balanced), or a run of non-blanks.
fn shell_value_len(text: &str) -> usize {
    let bytes = text.as_bytes();
    match bytes.first() {
        Some(b'$') if bytes.get(1) == Some(&b'(') => {
            let mut depth = 0usize;
            for (index, byte) in bytes.iter().enumerate() {
                match byte {
                    b'(' => depth += 1,
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            return index + 1;
                        }
                    }
                    _ => {}
                }
            }
            bytes.len()
        }
        Some(quote @ (b'"' | b'\'')) => bytes[1..]
            .iter()
            .position(|byte| byte == quote)
            .map_or(bytes.len(), |close| close + 2),
        _ => bytes
            .iter()
            .position(|byte| byte.is_ascii_whitespace() || b";&|".contains(byte))
            .unwrap_or(bytes.len()),
    }
}

/// A command line's set-up taken off its front: `cd <dir>` and
/// `NAME=value` followed by `&&`, `;` or (for an assignment) a blank. The
/// rest, the programs the set-up named (`B=/…/spawn` gives `B` → `spawn`),
/// and whether anything was taken.
fn without_setup(line: &str) -> (&str, Vec<(String, String)>, bool) {
    let mut rest = line.trim_start();
    let mut programs = Vec::new();
    let mut dropped = false;
    loop {
        let (taken, after) = if let Some(dir) = rest.strip_prefix("cd ") {
            let dir = dir.trim_start();
            let len = shell_value_len(dir);
            (None, &dir[len..])
        } else {
            let name_len = rest
                .bytes()
                .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                .count();
            if name_len == 0
                || rest.as_bytes()[0].is_ascii_digit()
                || rest.as_bytes().get(name_len) != Some(&b'=')
            {
                break;
            }
            let value = &rest[name_len + 1..];
            let len = shell_value_len(value);
            (Some((&rest[..name_len], &value[..len])), &value[len..])
        };
        let trimmed = after.trim_start();
        let next = if let Some(next) = trimmed.strip_prefix("&&") {
            next
        } else if let Some(next) = trimmed.strip_prefix(';') {
            next
        } else if taken.is_some() && trimmed.len() < after.len() {
            // `NAME=value cmd`: an assignment for one command.
            trimmed
        } else {
            break;
        };
        if next.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = taken
            && value.contains('/')
            && !value.contains(['"', '\'', '$', '`'])
            && let Some(program) = value.rsplit('/').next()
            && !program.is_empty()
        {
            programs.push((name.to_string(), program.to_string()));
        }
        rest = next.trim_start();
        dropped = true;
    }
    (rest, programs, dropped)
}

/// A Bash command's summary: see [`tool_summary`]. Whether it is the
/// whole command: one line, no set-up taken off, nothing left out.
fn command_summary(command: &str) -> (String, bool) {
    let mut lines = command
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let Some(first) = lines.next() else {
        return (MORE.to_string(), false);
    };
    let one_line = lines.next().is_none();
    let (rest, programs, dropped) = without_setup(first);
    let mut kept: Vec<String> = Vec::new();
    let mut words = rest.split_whitespace().peekable();
    while let Some(word) = words.peek() {
        if kept.len() == COMMAND_WORDS {
            break;
        }
        // `echo`, `export`, `sudo` …: the program alone.
        if let Some(program) = kept.first()
            && PROGRAM_ONLY.contains(&program.rsplit('/').next().unwrap_or(program))
        {
            break;
        }
        // The command ends at an operator; what follows is another one.
        if word.contains([';', '|', '&', '<', '>', '(', ')', '#']) {
            break;
        }
        let variable = word
            .strip_prefix("${")
            .and_then(|name| name.strip_suffix('}'))
            .or_else(|| word.strip_prefix('$'));
        let word = match variable.and_then(|name| programs.iter().find(|(known, _)| known == name))
        {
            Some((_, program)) => program.as_str(),
            None => word,
        };
        if !plain_word(word) {
            break;
        }
        kept.push(word.to_string());
        words.next();
    }
    let left_out = words.peek().is_some();
    let whole = one_line && !dropped && !left_out && kept.join(" ") == rest.trim();
    (with_more(&kept.join(" "), left_out || !one_line), whole)
}

/// The input fields a tool's summary says in full: a call whose input has
/// no other field is all in its summary. Bash's `description` and
/// `timeout` do not change what runs.
fn whole_fields(tool: &str, input: &Value) -> bool {
    let allowed: &[&str] = match tool {
        "Bash" => &["command", "description", "timeout"],
        "Read" => &["file_path", "offset", "limit"],
        "Glob" => &["pattern", "path"],
        "Grep" if input.get("path").is_some() => &["pattern", "path"],
        "Grep" => &["pattern", "glob"],
        "WebSearch" => &["query"],
        _ => return false,
    };
    input
        .as_object()
        .is_some_and(|fields| fields.keys().all(|key| allowed.contains(&key.as_str())))
}

/// Is a tool call's summary its whole input? Only when the summary leaves
/// no field out ([`whole_fields`]), no word of it was left out (a command's
/// set-up included), and neither the redactor nor the cut changed it: then
/// two calls with the same summary are the same call. Edit, Write, Task,
/// WebFetch and TodoWrite summaries never are.
pub fn tool_summary_whole(tool: &str, input: Option<&Value>) -> bool {
    let Some(input) = input else { return false };
    let Some((text, complete)) = summary_text(tool, input) else {
        return false;
    };
    complete
        && whole_fields(tool, input)
        && text.chars().count() <= TOOL_SUMMARY_CHARS
        && crate::redact::redact(&text) == text
}

/// A tool call's summary before redaction and the cut, and whether it is
/// all of what it reads (no word or segment left out).
fn summary_text(tool: &str, input: &Value) -> Option<(String, bool)> {
    let summary = match tool {
        "Bash" => command_summary(input_text(input, "command")?),
        "Edit" | "MultiEdit" | "Write" => plain_path(input_text(input, "file_path")?),
        "NotebookEdit" => plain_path(input_text(input, "notebook_path")?),
        "Read" => {
            let (path, complete) = plain_path(input_text(input, "file_path")?);
            let offset = input.get("offset").and_then(Value::as_u64);
            let limit = input.get("limit").and_then(Value::as_u64);
            let range = match (offset, limit) {
                (Some(offset), Some(limit)) => format!(
                    ":{offset}-{}",
                    offset.saturating_add(limit).saturating_sub(1)
                ),
                (Some(offset), None) => format!(":{offset}-"),
                (None, Some(limit)) => format!(":1-{limit}"),
                (None, None) => String::new(),
            };
            (format!("{path}{range}"), complete)
        }
        "Grep" => {
            let pattern = input_text(input, "pattern")?;
            let plain = pattern.split_whitespace().all(plain_word);
            let shown = if plain {
                format!("\"{pattern}\"")
            } else {
                MORE.to_string()
            };
            match input_text(input, "path").or_else(|| input_text(input, "glob")) {
                Some(place) => {
                    let (place, complete) = plain_path(place);
                    (format!("{shown} in {place}"), plain && complete)
                }
                None => (shown, plain),
            }
        }
        "Glob" => {
            let (pattern, pattern_whole) = plain_path(input_text(input, "pattern")?);
            match input_text(input, "path") {
                Some(path) => {
                    let (path, path_whole) = plain_path(path);
                    (format!("{pattern} in {path}"), pattern_whole && path_whole)
                }
                None => (pattern, pattern_whole),
            }
        }
        "Task" | "Agent" => plain_words(input_text(input, "description")?, 2 * COMMAND_WORDS),
        "WebFetch" => {
            // The redactor first, while the scheme is there: a password
            // holding `/` or `@` then cannot pass for the host.
            let url = crate::redact::redact(input_text(input, "url")?);
            let url = url.as_str();
            let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
            let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
            let host = authority.rsplit('@').next().unwrap_or(authority);
            let host = host.split(':').next().unwrap_or(host);
            if !plain_host(host) {
                return None;
            }
            (host.to_string(), false)
        }
        "WebSearch" => plain_words(input_text(input, "query")?, 2 * COMMAND_WORDS),
        "TodoWrite" => {
            let items = input.get("todos").and_then(Value::as_array)?.len();
            (
                format!("{items} item{}", if items == 1 { "" } else { "s" }),
                false,
            )
        }
        _ => return None,
    };
    Some(summary)
}

fn block_type(block: &Value) -> Option<&str> {
    block.get("type").and_then(Value::as_str)
}

fn keyed(kind: EventKind, key: String) -> Normalized {
    Normalized {
        kind,
        source_event_id: Some(key),
    }
}

/// The tool ends in a user line's `tool_result` blocks.
fn tool_results(blocks: &[Value]) -> Vec<Normalized> {
    blocks
        .iter()
        .filter(|block| block_type(block) == Some("tool_result"))
        .filter_map(|block| {
            let id = block.get("tool_use_id").and_then(Value::as_str)?;
            Some(
                if block.get("is_error").and_then(Value::as_bool) == Some(true) {
                    keyed(
                        EventKind::ToolFailed {
                            tool_use_id: id.to_string(),
                            error: text_of(block.get("content").unwrap_or(&Value::Null)),
                        },
                        format!("{id}:failed"),
                    )
                } else {
                    keyed(
                        EventKind::ToolCompleted {
                            tool_use_id: id.to_string(),
                            duration_ms: None,
                        },
                        format!("{id}:completed"),
                    )
                },
            )
        })
        .collect()
}

/// The whole text of a content value: a string, or its text blocks joined.
/// Not truncated, unlike [`text_of`]: what it is searched for may come late.
fn whole_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Opens every notification the engine gives the agent about a background
/// job (and the line that carries it must start with it).
const NOTIFICATION: &str = "<task-notification>";

/// The longest job id kept; the engine's are 9 characters (an Agent's 17).
const MAX_JOB_ID: usize = 64;

/// How a background Agent's notification ends its summary when the agent
/// only paused: it stopped with background work of its own still running,
/// may resume on its own, and notifies again under the same id. That is no
/// end of the job.
const AGENT_PAUSED: &str = "is waiting on its own background work";

/// A job id as the engine writes it (`b8w03og7k`): letters, digits, `_` and
/// `-`, not starting with `_` (the engine's own markers, such as
/// `__orphan_summary__:shell`, are no job). Anything else is no id.
fn job_id(text: &str) -> Option<&str> {
    let id = text.trim();
    (!id.is_empty()
        && id.len() <= MAX_JOB_ID
        && !id.starts_with('_')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))
    .then_some(id)
}

/// The leading id-like run of `text` (`b8w03og7k. Output is…` gives
/// `b8w03og7k`).
fn leading_id(text: &str) -> Option<&str> {
    let end = text
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .unwrap_or(text.len());
    job_id(&text[..end])
}

/// A start of job `id`, keyed by the tool call whose result started it
/// (`by`, its `tool_use_id`): a background Agent resumed under its id runs
/// again, and that run's start is a record of its own.
fn job_started(id: &str, kind: JobKind, by: &str) -> Normalized {
    keyed(
        EventKind::JobStarted {
            job_id: id.to_string(),
            kind,
        },
        format!("job:{id}:started:{by}"),
    )
}

/// An end of job `id`, keyed by `what` ended it: the notification
/// ([`notification_key`]) or the `TaskStop` call.
fn job_ended(id: &str, how: JobEnd, what: &str) -> Normalized {
    keyed(
        EventKind::JobEnded {
            job_id: id.to_string(),
            how,
        },
        format!("job:{id}:{what}"),
    )
}

/// The key of the end a notification records: a digest of the whole
/// notification (FNV-1a, the same across builds and hosts). A copy of one
/// notification (mid-turn and as a prompt, byte for byte the same) records
/// one end; a later run of the same Agent ends with another notification
/// (its own result and usage), so with an end of its own.
fn notification_key(body: &str) -> String {
    let digest = body.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("ended:{digest:016x}")
}

/// Whether a notification's `<summary>` says a background Agent only paused
/// ([`AGENT_PAUSED`]). The agent's description is quoted in the summary, so
/// only the summary's end is read.
fn agent_paused(body: &str) -> bool {
    element(body, "summary").is_some_and(|summary| {
        let summary = summary.trim();
        summary.starts_with("Agent \"") && summary.ends_with(AGENT_PAUSED)
    })
}

/// The jobs a `<task-notification>` (or several) says ended. Its head (up
/// to its `<summary>` or `<event>`, which quote the job's own output and
/// may hold anything) names the jobs (`<task-id>`, one or more) and, when
/// the job ended, how (`<status>`). With no status it is a Monitor's event,
/// which ends nothing, unless the event is the Monitor's expiry
/// (`[Monitor expired after …]`). A background Agent that only paused
/// ([`agent_paused`]) ends nothing either, whatever its status. The text
/// must open with the notification: a prompt that quotes one is no
/// notification.
fn job_ends(text: &str) -> Vec<Normalized> {
    let mut ended = Vec::new();
    if !text.trim_start().starts_with(NOTIFICATION) {
        return ended;
    }
    for body in text.split(NOTIFICATION).skip(1) {
        let body = body.split("</task-notification>").next().unwrap_or(body);
        let cut = ["<summary>", "<event>"]
            .iter()
            .filter_map(|tag| body.find(tag))
            .min()
            .unwrap_or(body.len());
        let head = &body[..cut];
        let how = match element(head, "status") {
            Some(_) if agent_paused(body) => continue,
            Some(status) => JobEnd::from_status(status),
            None if element(body, "event")
                .is_some_and(|event| event.trim_start().starts_with("[Monitor expired")) =>
            {
                JobEnd::Expired
            }
            None => continue,
        };
        let key = notification_key(body);
        ended.extend(
            elements(head, "task-id")
                .filter_map(job_id)
                .map(|id| job_ended(id, how, &key)),
        );
    }
    ended
}

/// The text of the first `<name>…</name>` in `text`.
fn element<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    elements(text, name).next()
}

/// The text of every `<name>…</name>` in `text`, in order.
fn elements<'a>(text: &'a str, name: &str) -> impl Iterator<Item = &'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let mut rest = text;
    std::iter::from_fn(move || {
        let start = rest.find(&open)? + open.len();
        let length = rest[start..].find(&close)?;
        let found = &rest[start..start + length];
        rest = &rest[start + length + close.len()..];
        Some(found)
    })
}

/// The job one tool result (of the call `by`) started or stopped, if any.
///
/// The engine keeps a structured copy of a tool's result on the line
/// (`toolUseResult`), which only a tool writes: a background Bash names
/// its `backgroundTaskId` (also one moved to the background), a Monitor
/// its `taskId` with its `timeoutMs`, a background Agent its `agentId`
/// (`isAsync`, `status` `async_launched`; or a Skill forked to run in the
/// background, `status` `forked`, `background`), a `SendMessage` that resumed a
/// finished Agent its `resumedAgentId` (`success`), and a `TaskStop` that
/// succeeded the `task_id` it stopped ("Successfully stopped task: …"). A
/// failed stop stops nothing: the job's own notification ends it. A message
/// only queued for a running Agent starts nothing. The result's text (what
/// the model reads) is the fallback only for a line with no structured
/// copy, and only where it opens with the engine's own words: a command's
/// output that quotes them is no job.
fn tool_job(structured: Option<&Value>, text: &str, by: &str) -> Option<Normalized> {
    if let Some(result) = structured.filter(|result| result.is_object()) {
        let field = |name: &str| result.get(name).and_then(Value::as_str).and_then(job_id);
        let flag = |name: &str| result.get(name).and_then(Value::as_bool) == Some(true);
        if let Some(id) = field("backgroundTaskId") {
            return Some(job_started(id, JobKind::Bash, by));
        }
        if result.get("timeoutMs").is_some()
            && let Some(id) = field("taskId")
        {
            return Some(job_started(id, JobKind::Monitor, by));
        }
        let status = result.get("status").and_then(Value::as_str);
        if (flag("isAsync") && status == Some("async_launched")
            || flag("background") && status == Some("forked"))
            && let Some(id) = field("agentId")
        {
            return Some(job_started(id, JobKind::Agent, by));
        }
        if flag("success")
            && let Some(id) = field("resumedAgentId")
        {
            return Some(job_started(id, JobKind::Agent, by));
        }
        let stopped = result
            .get("message")
            .and_then(Value::as_str)
            .is_some_and(|message| message.starts_with("Successfully stopped task"));
        return stopped
            .then(|| field("task_id").or_else(|| field("shell_id")))
            .flatten()
            .map(|id| job_ended(id, JobEnd::Stopped, &format!("stopped:{by}")));
    }
    // A line without the structured copy: the engine's words, at the start
    // of the result (after the notes it may put first).
    let text = text
        .trim_start()
        .lines()
        .skip_while(|line| line.starts_with("Shell cwd was reset to "))
        .collect::<Vec<_>>()
        .join("\n");
    if let Some(rest) = text
        .strip_prefix("Command running in background with ID: ")
        .or_else(|| text.strip_prefix("Command was moved to the background (ID: "))
    {
        return leading_id(rest).map(|id| job_started(id, JobKind::Bash, by));
    }
    if let Some(rest) = text.strip_prefix("Monitor started (task ") {
        return leading_id(rest).map(|id| job_started(id, JobKind::Monitor, by));
    }
    if text.starts_with("Async agent launched successfully") {
        return text
            .lines()
            .find_map(|line| line.strip_prefix("agentId: "))
            .and_then(leading_id)
            .map(|id| job_started(id, JobKind::Agent, by));
    }
    if text.starts_with('{') {
        let parsed = serde_json::from_str::<Value>(&text).ok()?;
        return tool_job(Some(&parsed), "", by);
    }
    None
}

/// The jobs a user line's tool results started or stopped. The structured
/// copy (`toolUseResult`) belongs to the line's one result: a line with
/// several results is read from their texts.
fn tool_jobs(value: &Value, blocks: &[Value]) -> Vec<Normalized> {
    let results: Vec<&Value> = blocks
        .iter()
        .filter(|block| block_type(block) == Some("tool_result"))
        .collect();
    let structured = (results.len() == 1)
        .then(|| value.get("toolUseResult"))
        .flatten();
    results
        .into_iter()
        .filter(|block| block.get("is_error").and_then(Value::as_bool) != Some(true))
        .filter_map(|block| {
            let by = block.get("tool_use_id").and_then(Value::as_str)?;
            let text = whole_text(block.get("content").unwrap_or(&Value::Null));
            tool_job(structured, &text, by)
        })
        .collect()
}

/// What a user line that is not a tool result means: a prompt, an
/// interrupt (which stops the turn), or a slash command (nothing). A
/// prompt that is the engine's notification about a background job also
/// ends the jobs it reports, before the turn it starts.
fn user_text(content: &Value, uuid: &str) -> Vec<Normalized> {
    let mut events = job_ends(&whole_text(content));
    events.extend(user_turn(content, uuid));
    events
}

/// The turn a user line that is not a tool result starts or stops.
fn user_turn(content: &Value, uuid: &str) -> Vec<Normalized> {
    let text = text_of(content);
    let text = text.trim_start();
    if text.starts_with("[Request interrupted") {
        vec![keyed(
            EventKind::TurnStopped {
                input_tokens: None,
                output_tokens: None,
            },
            uuid.to_string(),
        )]
    } else if text.is_empty()
        || text.starts_with("<command-")
        || text.starts_with("<local-command-")
    {
        Vec::new()
    } else {
        vec![keyed(EventKind::MessageReceived, uuid.to_string())]
    }
}

/// The events in one Claude Code transcript line. Which lines are an
/// execution's is decided by where they are in the file (the Bridge reads
/// from the transcript's length when the execution started), never by the
/// time a line claims: that time is written by the engine.
///
/// - a prompt (a user line that is not a tool result) starts a turn;
/// - each `tool_use` block of an assistant line starts a tool, and each
///   `tool_result` block of a user line ends one, failed when `is_error`;
/// - `system`/`turn_duration`, written when a turn ends, and an interrupt
///   stop the turn; an API error line fails it.
///
/// Sub-agent (`isSidechain`) lines are the sub-agent's, not the session's.
///
/// Background jobs ([`JobKind`]) are read from the same lines:
///
/// - a tool result that started one: a Bash's `backgroundTaskId`, a
///   Monitor's `taskId`, a background Agent's `agentId`, or a `SendMessage`
///   that resumed a finished Agent ([`tool_job`]), keyed by the job's id and
///   the call; one that stopped one: a `TaskStop` that succeeded;
/// - the engine's notification that one ended (`<task-notification>` with a
///   `<status>`, or a Monitor's expiry event; not an Agent's pause), as the
///   engine gave it to the agent: mid-turn (an `attachment` of type
///   `queued_command`) or as the prompt of a turn of its own (a user line).
///   Each is keyed by the job's id and a digest of the notification
///   ([`notification_key`]), so a second copy adds nothing.
///
/// The engine's queue (`queue-operation`) is not read. What it queues for
/// the agent comes back as one of those copies, after the tool result that
/// started the job; but it also queues the notifications of sub-agents'
/// jobs (then removes them and hands them to the sub-agent), and it can
/// queue a quick job's end before it writes the result that started it.
pub fn claude_line(line: &str) -> Vec<Normalized> {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let field = |name: &str| value.get(name).and_then(Value::as_str);
    if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
        return Vec::new();
    }
    // No line id: the engine's queue (`queue-operation`), which is not read.
    let Some(uuid) = field("uuid") else {
        return Vec::new();
    };
    let content = value.pointer("/message/content").unwrap_or(&Value::Null);
    let blocks = content.as_array().map(Vec::as_slice).unwrap_or_default();
    match field("type") {
        Some("system") if field("subtype") == Some("turn_duration") => vec![keyed(
            EventKind::TurnStopped {
                input_tokens: None,
                output_tokens: None,
            },
            uuid.to_string(),
        )],
        Some("assistant")
            if value.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true) =>
        {
            vec![keyed(
                EventKind::TurnFailed {
                    error: text_of(content),
                    // Claude Code 2.1 tags the line with the failed
                    // request's status and its own error class; older
                    // engines write neither, and the recovery reads the
                    // text (crate::bridge's `EndCause` note).
                    status: value.get("apiErrorStatus").and_then(Value::as_i64),
                    error_kind: field("error").map(str::to_string),
                },
                uuid.to_string(),
            )]
        }
        Some("assistant") => blocks
            .iter()
            .filter(|block| block_type(block) == Some("tool_use"))
            .filter_map(|block| {
                let id = block.get("id").and_then(Value::as_str)?;
                let name = block.get("name").and_then(Value::as_str)?;
                Some(keyed(
                    EventKind::ToolStarted {
                        tool_name: name.to_string(),
                        tool_use_id: id.to_string(),
                        summary: tool_summary(name, block.get("input")),
                        summary_whole: tool_summary_whole(name, block.get("input")),
                    },
                    format!("{id}:started"),
                ))
            })
            .collect(),
        Some("user")
            if blocks
                .iter()
                .any(|block| block_type(block) == Some("tool_result")) =>
        {
            let mut events = tool_results(blocks);
            events.extend(tool_jobs(&value, blocks));
            events
        }
        Some("user") if value.get("isMeta").and_then(Value::as_bool) != Some(true) => {
            user_text(content, uuid)
        }
        Some("attachment")
            if value.pointer("/attachment/type").and_then(Value::as_str)
                == Some("queued_command") =>
        {
            value
                .pointer("/attachment/prompt")
                .map_or_else(Vec::new, |prompt| job_ends(&whole_text(prompt)))
        }
        _ => Vec::new(),
    }
}

/// The model usage an assistant line of a Claude Code transcript carries:
/// a line of `type` `assistant` with `message.id`, `message.usage` and a
/// readable `timestamp` (the model `unknown` when `message.model` is
/// missing). Claude Code writes one line per content block, each repeating
/// the id, the model and the usage: the key is the message id with its four
/// counts, so an identical repeat appends nothing, and a line whose usage
/// grew is one more event, which the cost step folds into the same message
/// (crate spawn-cli's `cost`: once per message id, its largest usage, its
/// earliest time). A usage of four zeros (the engine's own synthetic
/// messages) costs nothing and gives none. A sub-agent's line
/// (`isSidechain`) is a model call too, and counts.
pub fn claude_usage(line: &str) -> Option<Normalized> {
    // Most lines carry no usage: skip them unparsed.
    if !line.contains("\"usage\"") {
        return None;
    }
    let value = serde_json::from_str::<Value>(line).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let message = value.get("message")?;
    let id = message
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())?;
    let usage = message.get("usage")?;
    let at_ms = utc_ms(value.get("timestamp").and_then(Value::as_str)?)?;
    let count = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
    let (input, output, cache_write, cache_read) = (
        count("input_tokens"),
        count("output_tokens"),
        count("cache_creation_input_tokens"),
        count("cache_read_input_tokens"),
    );
    if input == 0 && output == 0 && cache_write == 0 && cache_read == 0 {
        return None;
    }
    let model = message
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .unwrap_or("unknown");
    Some(keyed(
        EventKind::ModelUsage {
            message_id: id.to_string(),
            model: model.to_string(),
            at_ms,
            input_tokens: input,
            output_tokens: output,
            cache_write_tokens: cache_write,
            cache_read_tokens: cache_read,
        },
        format!("usage:{id}:{input}:{output}:{cache_write}:{cache_read}"),
    ))
}

/// The engine version a Claude Code transcript line names (its `version`
/// field). Like [`claude_line`], the caller reads only the execution's part
/// of the file.
pub fn claude_version(line: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    let version = value.get("version").and_then(Value::as_str)?.trim();
    (!version.is_empty() && version.len() <= 64).then(|| version.to_string())
}

#[cfg(test)]
mod tests {
    use super::{claude_line, claude_version, utc_ms};

    #[test]
    fn a_line_names_the_engine_version_it_was_written_by() {
        let line = r#"{"type":"user","uuid":"u1","timestamp":"2026-11-04T05:46:12.345Z","version":"2.1.318","message":{"content":"x"}}"#;
        assert_eq!(claude_version(line).as_deref(), Some("2.1.318"));
        assert_eq!(claude_version(r#"{"type":"mode","version":""}"#), None);
        assert_eq!(claude_version(r#"{"type":"mode"}"#), None);
        assert_eq!(claude_version("not json"), None);
    }
    use crate::event::EventKind;

    #[test]
    fn utc_times_parse_to_epoch_milliseconds() {
        assert_eq!(utc_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(utc_ms("2000-03-01T00:00:01.5Z"), Some(951_868_801_500));
        assert_eq!(utc_ms("2026-11-04T05:46:12.345Z"), Some(1_793_771_172_345));
        assert_eq!(utc_ms("2026-11-04 05:46:12"), None);
        // A numeric offset names the same instant.
        assert_eq!(
            utc_ms("2026-11-04T13:46:12.345+08:00"),
            Some(1_793_771_172_345)
        );
        assert_eq!(
            utc_ms("2026-11-04T00:46:12.345-05:00"),
            Some(1_793_771_172_345)
        );
        assert_eq!(
            utc_ms("2026-11-04T05:46:12.345+00:00"),
            Some(1_793_771_172_345)
        );
    }

    #[test]
    fn a_line_is_kept_whatever_time_it_claims() {
        for timestamp in [
            None,
            Some("yesterday"),
            Some("1970-01-01T00:00:00.000Z"),
            Some("99999999999999999-11-04T05:46:12.000Z"),
            Some("2026-11-04T99999999999999999:46:12.000Z"),
        ] {
            let at = timestamp.map_or(String::new(), |at| format!(r#""timestamp":"{at}","#));
            let line = format!(r#"{{"type":"user","uuid":"u9",{at}"message":{{"content":"x"}}}}"#);
            assert_eq!(claude_line(&line).len(), 1, "{timestamp:?}");
        }
    }

    #[test]
    fn utc_ms_is_total() {
        for text in [
            "99999999999999999-11-04T05:46:12.000Z",
            "2026-11-04T99999999999999999:46:12.000Z",
            "2026-11-04T05:99999999999999999:12.000Z",
            "2026-11-04T05:46:99999999999999999.000Z",
            "-99999999999999999-11-04T05:46:12.000Z",
            "10000-01-01T00:00:00.000Z",
            "0-01-01T00:00:00.000Z",
            "2026-11-04T24:00:00.000Z",
            "2026-11-04T05:60:00.000Z",
            "2026-11-04T05:46:12.000+99999999999999999:00",
            "2026-11-04T05:46:12.000+24:00",
        ] {
            assert_eq!(utc_ms(text), None, "{text}");
        }
        // Every byte string of a few shapes: never a panic.
        let pieces = [
            "9",
            "99999999999999999999",
            "-",
            "T",
            ":",
            ".",
            "Z",
            "+",
            "0",
        ];
        for a in pieces {
            for b in pieces {
                for c in pieces {
                    let _ = utc_ms(&format!("{a}-{b}-{c}T{a}:{b}:{c}.{a}Z"));
                    let _ = utc_ms(&format!("2026-01-01T00:00:00{a}{b}:{c}"));
                }
            }
        }
        assert_eq!(
            utc_ms("9999-12-31T23:59:60.999Z"),
            Some(253_402_300_800_999)
        );
    }

    fn kinds(line: &str) -> Vec<(String, Option<String>)> {
        claude_line(line)
            .into_iter()
            .map(|event| (event.kind.name().to_string(), event.source_event_id))
            .collect()
    }

    const AT: &str = r#""timestamp":"2026-11-04T05:46:12.345Z""#;

    #[test]
    fn a_transcript_gives_the_turn_and_its_tools() {
        let prompt = format!(
            r#"{{"type":"user","uuid":"u1",{AT},"message":{{"role":"user","content":"fix it"}}}}"#
        );
        assert_eq!(
            kinds(&prompt),
            vec![("message.received".into(), Some("u1".into()))]
        );
        let tool = format!(
            r#"{{"type":"assistant","uuid":"a1",{AT},"message":{{"content":[{{"type":"tool_use","id":"toolu_1","name":"Bash","input":{{}}}}],"stop_reason":"tool_use"}}}}"#
        );
        assert_eq!(
            kinds(&tool),
            vec![("tool.started".into(), Some("toolu_1:started".into()))]
        );
        let done = format!(
            r#"{{"type":"user","uuid":"u2",{AT},"message":{{"content":[{{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}}]}}}}"#
        );
        assert_eq!(
            kinds(&done),
            vec![("tool.completed".into(), Some("toolu_1:completed".into()))]
        );
        let failed = format!(
            r#"{{"type":"user","uuid":"u3",{AT},"message":{{"content":[{{"type":"tool_result","tool_use_id":"toolu_2","is_error":true,"content":[{{"type":"text","text":"exit 1"}}]}}]}}}}"#
        );
        let event = claude_line(&failed).remove(0);
        assert_eq!(
            event.kind,
            EventKind::ToolFailed {
                tool_use_id: "toolu_2".into(),
                error: "exit 1".into()
            }
        );
        let end = format!(
            r#"{{"type":"system","subtype":"turn_duration","uuid":"s1",{AT},"durationMs":5}}"#
        );
        assert_eq!(
            kinds(&end),
            vec![("turn.stopped".into(), Some("s1".into()))]
        );
        let error = format!(
            r#"{{"type":"assistant","uuid":"a2",{AT},"isApiErrorMessage":true,"message":{{"content":[{{"type":"text","text":"API Error: 529 overloaded"}}]}}}}"#
        );
        assert_eq!(
            claude_line(&error)[0].kind,
            EventKind::TurnFailed {
                error: "API Error: 529 overloaded".into(),
                status: None,
                error_kind: None,
            }
        );
        // Claude Code 2.1's line: the engine's own tags are kept, so a
        // recovery can tell a model outage from an expired login.
        let tagged = format!(
            r#"{{"type":"assistant","uuid":"a3",{AT},"isApiErrorMessage":true,"error":"rate_limit","apiErrorStatus":429,"message":{{"content":[{{"type":"text","text":"API Error: 429 quota"}}]}}}}"#
        );
        assert_eq!(
            claude_line(&tagged)[0].kind,
            EventKind::TurnFailed {
                error: "API Error: 429 quota".into(),
                status: Some(429),
                error_kind: Some("rate_limit".into()),
            }
        );
    }

    #[test]
    fn old_side_meta_and_unknown_lines_give_nothing() {
        let side = format!(
            r#"{{"type":"user","uuid":"u1",{AT},"isSidechain":true,"message":{{"content":"x"}}}}"#
        );
        assert!(kinds(&side).is_empty());
        let meta = format!(
            r#"{{"type":"user","uuid":"u1",{AT},"isMeta":true,"message":{{"content":"x"}}}}"#
        );
        assert!(kinds(&meta).is_empty());
        let command = format!(
            r#"{{"type":"user","uuid":"u1",{AT},"message":{{"content":"<command-name>/clear</command-name>"}}}}"#
        );
        assert!(kinds(&command).is_empty());
        assert!(kinds(r#"{"type":"mode","mode":"normal","sessionId":"x"}"#).is_empty());
        assert!(kinds("not json").is_empty());
        let interrupted = format!(
            r#"{{"type":"user","uuid":"u9",{AT},"message":{{"content":[{{"type":"text","text":"[Request interrupted by user]"}}]}}}}"#
        );
        assert_eq!(
            kinds(&interrupted),
            vec![("turn.stopped".into(), Some("u9".into()))]
        );
    }

    /// Lines as Claude Code 2.1.318 writes them, cut to the fields read
    /// (taken from worker transcripts of 2026-11-06).
    mod jobs {
        use super::kinds;
        use crate::event::{EventKind, JobEnd, JobKind};
        use crate::ingest::claude_line;

        fn jobs(line: &str) -> Vec<EventKind> {
            claude_line(line)
                .into_iter()
                .map(|event| event.kind)
                .filter(|kind| {
                    matches!(
                        kind,
                        EventKind::JobStarted { .. } | EventKind::JobEnded { .. }
                    )
                })
                .collect()
        }

        fn started(id: &str, kind: JobKind) -> EventKind {
            EventKind::JobStarted {
                job_id: id.to_string(),
                kind,
            }
        }

        fn ended(id: &str, how: JobEnd) -> EventKind {
            EventKind::JobEnded {
                job_id: id.to_string(),
                how,
            }
        }

        /// The keys of the job records of `line`, in order.
        fn keys(line: &str) -> Vec<String> {
            claude_line(line)
                .into_iter()
                .filter(|event| event.kind.name().starts_with("job."))
                .filter_map(|event| event.source_event_id)
                .collect()
        }

        /// The one end key of `line`, which must be the digest form.
        fn end_key(line: &str, id: &str) -> String {
            let keys = keys(line);
            assert_eq!(keys.len(), 1, "{keys:?}");
            let prefix = format!("job:{id}:ended:");
            let digest = keys[0].strip_prefix(&prefix).expect("an end key");
            assert_eq!(digest.len(), 16, "{keys:?}");
            assert!(digest.chars().all(|c| c.is_ascii_hexdigit()), "{keys:?}");
            keys[0].clone()
        }

        const BASH_USE: &str = r#"{"parentUuid":"p","isSidechain":false,"message":{"model":"claude-opus-5-5","role":"assistant","content":[{"type":"tool_use","id":"toolu_01CU","name":"Bash","input":{"command":"until grep -qE '^exit=' gate.log; do sleep 5; done","description":"Wait for test-gate v1","timeout":3600000,"run_in_background":true}}],"stop_reason":"tool_use"},"type":"assistant","uuid":"a1","timestamp":"2026-11-06T10:14:01.980Z","version":"2.1.318"}"#;
        const BASH_RESULT: &str = r#"{"parentUuid":"a1","isSidechain":false,"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_01CU","type":"tool_result","content":"Command running in background with ID: b8w03og7k. Output is being written to: /var/tmp/spawn/claude-1000/w/tasks/b8w03og7k.output. You will be notified when it completes — do not poll that file or sleep while waiting.","is_error":false}]},"uuid":"u1","timestamp":"2026-11-06T10:14:02.117Z","toolUseResult":{"stdout":"","stderr":"","interrupted":false,"isImage":false,"noOutputExpected":false,"backgroundTaskId":"b8w03og7k"},"version":"2.1.318"}"#;
        const BASH_QUEUED: &str = r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-11-06T12:39:59.670Z","sessionId":"s","content":"<task-notification>\n<task-id>b8w03og7k</task-id>\n<tool-use-id>toolu_01CU</tool-use-id>\n<output-file>/var/tmp/spawn/claude-1000/w/tasks/b8w03og7k.output</output-file>\n<status>completed</status>\n<summary>Background command \"Wait for test-gate v1\" completed (exit code 0)</summary>\n</task-notification>"}"#;
        const BASH_MID_TURN: &str = r#"{"parentUuid":"d5","isSidechain":false,"attachment":{"type":"queued_command","prompt":"<task-notification>\n<task-id>b8w03og7k</task-id>\n<tool-use-id>toolu_01CU</tool-use-id>\n<output-file>/var/tmp/spawn/claude-1000/w/tasks/b8w03og7k.output</output-file>\n<status>completed</status>\n<summary>Background command \"Wait for test-gate v1\" completed (exit code 0)</summary>\n</task-notification>","commandMode":"task-notification","origin":{"kind":"task-notification","producer":"session-task"}},"type":"attachment","uuid":"t0","version":"2.1.318"}"#;
        const BASH_DONE: &str = r#"{"parentUuid":"c7","isSidechain":false,"type":"user","message":{"role":"user","content":"<task-notification>\n<task-id>b8w03og7k</task-id>\n<tool-use-id>toolu_01CU</tool-use-id>\n<output-file>/var/tmp/spawn/claude-1000/w/tasks/b8w03og7k.output</output-file>\n<status>completed</status>\n<summary>Background command \"Wait for test-gate v1\" completed (exit code 0)</summary>\n</task-notification>"},"uuid":"u2","timestamp":"2026-11-06T12:39:59.677Z","origin":{"kind":"task-notification","producer":"session-task"},"promptSource":"system","turnOrigin":"task_notification","version":"2.1.318"}"#;

        #[test]
        fn a_background_bash_starts_with_its_result_and_ends_with_its_notification() {
            assert_eq!(
                kinds(BASH_USE),
                vec![("tool.started".into(), Some("toolu_01CU:started".into()))],
                "the call itself is a tool, not yet a job"
            );
            assert_eq!(
                kinds(BASH_RESULT),
                vec![
                    ("tool.completed".into(), Some("toolu_01CU:completed".into())),
                    (
                        "job.started".into(),
                        Some("job:b8w03og7k:started:toolu_01CU".into())
                    ),
                ],
                "keyed by the job and the call that started it"
            );
            assert_eq!(jobs(BASH_RESULT), [started("b8w03og7k", JobKind::Bash)]);
            // Queued when it ended: the engine's queue is not read
            // (`the_engines_queue_is_not_read`). Given to the agent mid-turn,
            // or as the prompt of a turn of its own: the same notification,
            // one key, so a second copy records nothing new.
            assert!(kinds(BASH_QUEUED).is_empty());
            let end = end_key(BASH_MID_TURN, "b8w03og7k");
            assert_eq!(
                kinds(BASH_DONE),
                vec![
                    ("job.ended".into(), Some(end)),
                    ("message.received".into(), Some("u2".into())),
                ],
                "the job ends before the turn its notification starts"
            );
            assert_eq!(jobs(BASH_DONE), [ended("b8w03og7k", JobEnd::Completed)]);
            let failed = BASH_DONE.replace("<status>completed", "<status>failed");
            assert_eq!(jobs(&failed), [ended("b8w03og7k", JobEnd::Failed)]);
            let killed = BASH_DONE.replace("<status>completed", "<status>killed");
            assert_eq!(jobs(&killed), [ended("b8w03og7k", JobEnd::Killed)]);
        }

        #[test]
        fn a_command_moved_to_the_background_is_a_bash_job() {
            let timed_out = r#"{"isSidechain":false,"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_02","type":"tool_result","content":"cli=0\nCommand did not complete within its 600s timeout and was moved to the background (ID: bq1n7c2x4). Output is being written to: /tmp/x/tasks/bq1n7c2x4.output.","is_error":false}]},"uuid":"u3","toolUseResult":{"stdout":"cli=0","stderr":"","interrupted":false,"isImage":false,"noOutputExpected":false,"backgroundTaskId":"bq1n7c2x4","timedOutAfterMs":600000},"version":"2.1.318"}"#;
            assert_eq!(jobs(timed_out), [started("bq1n7c2x4", JobKind::Bash)]);
            let for_a_message = r#"{"isSidechain":false,"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_03","type":"tool_result","content":"Command was moved to the background (ID: bcc6zkquw) so that a message that arrived while it was running can reach you; it was not interrupted.","is_error":false}]},"uuid":"u4","toolUseResult":{"stdout":"","stderr":"","interrupted":false,"backgroundTaskId":"bcc6zkquw","backgroundedToDeliverMessage":true},"version":"2.1.318"}"#;
            assert_eq!(jobs(for_a_message), [started("bcc6zkquw", JobKind::Bash)]);
        }

        const MONITOR_RESULT: &str = r#"{"parentUuid":"f1","isSidechain":false,"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_01BD","type":"tool_result","content":"Monitor started (task bv41gl0hm, expires in 30m unless the source ends first; you get one notice at expiry — re-arm if you still need the watch). You will be notified on each event. Keep working — do not poll or sleep."}]},"uuid":"u5","timestamp":"2026-11-06T10:14:21.356Z","toolUseResult":{"taskId":"bv41gl0hm","timeoutMs":1800000,"persistent":false},"version":"2.1.318"}"#;

        #[test]
        fn a_monitor_runs_through_its_events_until_it_expires_or_its_command_ends() {
            assert_eq!(
                jobs(MONITOR_RESULT),
                [started("bv41gl0hm", JobKind::Monitor)]
            );
            // Its events end nothing, however they reach the agent.
            let event = r#"{"isSidechain":false,"type":"user","message":{"role":"user","content":"<task-notification>\n<task-id>bv41gl0hm</task-id>\n<summary>Monitor event: \"test-gate v1 failures or completion\"</summary>\n<event>gate: all cases pass\nexit=0 seconds=849</event>\n</task-notification>"},"uuid":"u6","origin":{"kind":"task-notification","producer":"session-task"},"version":"2.1.318"}"#;
            assert_eq!(
                kinds(event),
                vec![("message.received".into(), Some("u6".into()))]
            );
            let quoting = r#"{"isSidechain":false,"type":"user","message":{"role":"user","content":"<task-notification>\n<task-id>bv41gl0hm</task-id>\n<summary>Monitor event: \"test-gate v1\"</summary>\n<event>ok   <status>completed</status> as the log printed it</event>\n</task-notification>"},"uuid":"u6b","origin":{"kind":"task-notification","producer":"session-task"},"version":"2.1.318"}"#;
            assert_eq!(
                kinds(quoting),
                vec![("message.received".into(), Some("u6b".into()))],
                "an event quoting a status is no end"
            );
            let mid_turn = r#"{"parentUuid":"d5","isSidechain":false,"attachment":{"type":"queued_command","prompt":"<task-notification>\n<task-id>bv41gl0hm</task-id>\n<summary>Monitor event: \"test-gate v1\"</summary>\n<event>ok   the verdict names the commits it judged</event>\n</task-notification>","commandMode":"task-notification","origin":{"kind":"task-notification","producer":"session-task"}},"type":"attachment","uuid":"t1","version":"2.1.318"}"#;
            assert!(kinds(mid_turn).is_empty());
            // Its expiry ends it: as a turn's prompt, or mid-turn.
            let expired = r#"{"isSidechain":false,"type":"user","message":{"role":"user","content":"<task-notification>\n<task-id>bv41gl0hm</task-id>\n<summary>Monitor event: \"test-gate v1\"</summary>\n<event>[Monitor expired after 30m with 1 event delivered. Re-arm it if you still need the watch.]</event>\n</task-notification>"},"uuid":"u7","origin":{"kind":"task-notification","producer":"session-task"},"version":"2.1.318"}"#;
            assert_eq!(jobs(expired), [ended("bv41gl0hm", JobEnd::Expired)]);
            let expired_mid_turn = mid_turn.replace(
                "ok   the verdict names the commits it judged",
                "[Monitor expired after 30m with no events delivered. Re-arm it if you still need the watch.]",
            );
            assert_eq!(
                jobs(&expired_mid_turn),
                [ended("bv41gl0hm", JobEnd::Expired)]
            );
            end_key(&expired_mid_turn, "bv41gl0hm");
            // Its command ended, or failed.
            let stream_ended = r#"{"parentUuid":"d5","isSidechain":false,"attachment":{"type":"queued_command","prompt":"<task-notification>\n<task-id>bv41gl0hm</task-id>\n<status>completed</status>\n<summary>Monitor \"test-gate v1\" stream ended</summary>\n<event>[exited with code 0]</event>\n</task-notification>","commandMode":"task-notification","origin":{"kind":"task-notification","producer":"session-task"}},"type":"attachment","uuid":"t3","version":"2.1.318"}"#;
            assert_eq!(jobs(stream_ended), [ended("bv41gl0hm", JobEnd::Completed)]);
            let script_failed = stream_ended
                .replace("<status>completed", "<status>failed")
                .replace("stream ended", "script failed (exit 1)");
            assert_eq!(jobs(&script_failed), [ended("bv41gl0hm", JobEnd::Failed)]);
        }

        #[test]
        fn a_task_stop_that_succeeded_ends_the_job_and_one_that_failed_does_not() {
            let stop = r#"{"parentUuid":"2b","isSidechain":false,"message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_0122","name":"TaskStop","input":{"task_id":"bpjhfs6ly"}}],"stop_reason":"tool_use"},"type":"assistant","uuid":"a2","version":"2.1.318"}"#;
            assert_eq!(
                kinds(stop),
                vec![("tool.started".into(), Some("toolu_0122:started".into()))],
                "asking for a stop stops nothing yet"
            );
            let stopped = r#"{"parentUuid":"a2","isSidechain":false,"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_0122","type":"tool_result","content":"{\"message\":\"Successfully stopped task: bpjhfs6ly (tail -f gate.log)\",\"task_id\":\"bpjhfs6ly\",\"task_type\":\"local_bash\",\"command\":\"tail -f gate.log\"}"}]},"uuid":"u8","toolUseResult":{"message":"Successfully stopped task: bpjhfs6ly (tail -f gate.log)","task_id":"bpjhfs6ly","task_type":"local_bash","command":"tail -f gate.log"},"version":"2.1.318"}"#;
            assert_eq!(
                kinds(stopped),
                vec![
                    ("tool.completed".into(), Some("toolu_0122:completed".into())),
                    (
                        "job.ended".into(),
                        Some("job:bpjhfs6ly:stopped:toolu_0122".into())
                    ),
                ]
            );
            assert_eq!(jobs(stopped), [ended("bpjhfs6ly", JobEnd::Stopped)]);
            let not_running = r#"{"isSidechain":false,"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"<tool_use_error>Task bpjhfs6ly is not running (status: completed)</tool_use_error>","is_error":true,"tool_use_id":"toolu_0123"}]},"uuid":"u9","toolUseResult":"Error: Task bpjhfs6ly is not running (status: completed)","version":"2.1.318"}"#;
            assert!(jobs(not_running).is_empty(), "a failed stop ends no job");
        }

        #[test]
        fn the_jobs_a_resume_finds_dead_are_marked_stopped() {
            let orphans = r#"{"isSidechain":false,"type":"user","message":{"role":"user","content":"<task-notification>\n<task-id>bqikdrfgk</task-id>\n<task-id>bo58jvmfb</task-id>\n<task-id>__orphan_summary__:shell</task-id>\n<status>stopped</status>\n<summary>2 background shell command tasks didn't finish before the previous session ended. Task ids: bqikdrfgk, bo58jvmfb.</summary>\n<note>They have been marked stopped. Task ids in this notification beginning with \"__orphan_summary\" are internal scan markers, not tasks.</note>\n</task-notification>"},"uuid":"u10","origin":{"kind":"task-notification"},"version":"2.1.312"}"#;
            assert_eq!(
                jobs(orphans),
                [
                    ended("bqikdrfgk", JobEnd::Stopped),
                    ended("bo58jvmfb", JobEnd::Stopped)
                ],
                "every job it names, and no marker"
            );
        }

        const AGENT_LAUNCH: &str = r#"{"type":"user","isSidechain":false,"uuid":"e0246752","timestamp":"2026-11-04T12:52:07.592Z","version":"2.1.317","toolUseResult":{"isAsync":true,"status":"async_launched","agentId":"a2adacaabc9f9b048","description":"Build storage phase 2a","resolvedModel":"claude-opus-5-5","prompt":"Implement storage phase 2a for","outputFile":"/data/tmp/claude-1000/p/s/tasks/a2adacaabc9f9b048.output","canReadOutputFile":true,"canContinueAgent":true},"message":{"role":"user","content":[{"tool_use_id":"toolu_018hgmBWFMc3fomfMC4sgXBo","type":"tool_result","content":[{"type":"text","text":"Async agent launched successfully. (This tool result is internal metadata — never quote or paste any part of it, including the agentId below, into a user-facing reply.)\nagentId: a2adacaabc9f9b048 (internal ID - do not mention to user. Use SendMessage with to: 'a2adacaabc9f9b048', summary: '<5-10 word recap>' to continue this agent.)\nThe agent is working in the background. You will be notified automatically when it completes.\noutput_file: /data/tmp/claude-1000/p/s/tasks/a2adacaabc9f9b048.output"}]}]}}"#;
        const AGENT_MID_TURN: &str = r#"{"parentUuid":"d5","isSidechain":false,"attachment":{"type":"queued_command","prompt":"<task-notification>\n<task-id>a2adacaabc9f9b048</task-id>\n<tool-use-id>toolu_018hgmBWFMc3fomfMC4sgXBo</tool-use-id>\n<output-file>/data/tmp/claude-1000/p/s/tasks/a2adacaabc9f9b048.output</output-file>\n<status>completed</status>\n<summary>Agent \"Build storage phase 2a\" finished</summary>\n<note>A task-notification fires each time this agent stops with no live background children of its own. The user can send it another message and resume it, so the same task-id may notify more than once.</note>\n<result>Storage phase 2a is built and committed.</result>\n<usage><subagent_tokens>256088</subagent_tokens><tool_uses>122</tool_uses><duration_ms>1751619</duration_ms></usage>\n<worktree><worktreePath>/home/ckl/projects/Spawn/.claude/worktrees/agent-a2adacaabc9f9b048</worktreePath><worktreeBranch>worktree-agent-a2adacaabc9f9b048</worktreeBranch></worktree>\n</task-notification>","commandMode":"task-notification","origin":{"kind":"task-notification","producer":"session-task"}},"type":"attachment","uuid":"t20","version":"2.1.318"}"#;
        const AGENT_DONE: &str = r#"{"type":"user","isSidechain":false,"uuid":"54ce99c8","timestamp":"2026-11-04T13:21:21.212Z","origin":{"kind":"task-notification","producer":"session-task","runId":"0mw24999i-aec44194"},"version":"2.1.317","message":{"role":"user","content":"<task-notification>\n<task-id>a2adacaabc9f9b048</task-id>\n<tool-use-id>toolu_018hgmBWFMc3fomfMC4sgXBo</tool-use-id>\n<output-file>/data/tmp/claude-1000/p/s/tasks/a2adacaabc9f9b048.output</output-file>\n<status>completed</status>\n<summary>Agent \"Build storage phase 2a\" finished</summary>\n<note>A task-notification fires each time this agent stops with no live background children of its own. The user can send it another message and resume it, so the same task-id may notify more than once.</note>\n<result>Storage phase 2a is built and committed.</result>\n<usage><subagent_tokens>256088</subagent_tokens><tool_uses>122</tool_uses><duration_ms>1751619</duration_ms></usage>\n<worktree><worktreePath>/home/ckl/projects/Spawn/.claude/worktrees/agent-a2adacaabc9f9b048</worktreePath><worktreeBranch>worktree-agent-a2adacaabc9f9b048</worktreeBranch></worktree>\n</task-notification>"}}"#;
        const AGENT_RESUMED: &str = r#"{"type":"user","isSidechain":false,"uuid":"8c491cc0","timestamp":"2026-11-04T13:39:01.340Z","version":"2.1.317","toolUseResult":{"success":true,"message":"Resuming agent a2adaca","resumedAgentId":"a2adacaabc9f9b048","revivedAgentId":"a2adacaabc9f9b048","pin":{"id":"a2adacaabc9f9b048","name":"a2adacaabc9f9b048","ref":"6d82fa"}},"message":{"role":"user","content":[{"tool_use_id":"toolu_01C8SfVsFL53ceFvQyhH19yS","type":"tool_result","content":[{"type":"text","text":"{\"success\":true,\"message\":\"Resuming agent a2adaca\",\"resumedAgentId\":\"a2adacaabc9f9b048\",\"pin\":{\"id\":\"a2adacaabc9f9b048\",\"name\":\"a2adacaabc9f9b048\",\"ref\":\"6d82fa\"}}"}]}]}}"#;
        const AGENT_DONE_AGAIN: &str = r#"{"parentUuid":"d5","isSidechain":false,"attachment":{"type":"queued_command","prompt":"<task-notification>\n<task-id>a2adacaabc9f9b048</task-id>\n<tool-use-id>toolu_01C8SfVsFL53ceFvQyhH19yS</tool-use-id>\n<output-file>/data/tmp/claude-1000/p/s/tasks/a2adacaabc9f9b048.output</output-file>\n<status>completed</status>\n<summary>Agent \"Build storage phase 2a\" finished</summary>\n<note>A task-notification fires each time this agent stops with no live background children of its own. The user can send it another message and resume it, so the same task-id may notify more than once.</note>\n<result>Done: the review's three points are fixed.</result>\n<usage><subagent_tokens>298923</subagent_tokens><tool_uses>39</tool_uses><duration_ms>614360</duration_ms></usage>\n<worktree><worktreePath>/home/ckl/projects/Spawn/.claude/worktrees/agent-a2adacaabc9f9b048</worktreePath><worktreeBranch>worktree-agent-a2adacaabc9f9b048</worktreeBranch></worktree>\n</task-notification>","commandMode":"task-notification","origin":{"kind":"task-notification","producer":"session-task"}},"type":"attachment","uuid":"t21","version":"2.1.318"}"#;
        const AGENT_PAUSED: &str = r#"{"parentUuid":"d5","isSidechain":false,"attachment":{"type":"queued_command","prompt":"<task-notification>\n<task-id>a0c5b4ae43e0f3f2d</task-id>\n<tool-use-id>toolu_01Aox6JqikTcY5AyfCxei3HP</tool-use-id>\n<output-file>/var/tmp/spawn/claude-1000/w/s/tasks/a0c5b4ae43e0f3f2d.output</output-file>\n<status>completed</status>\n<summary>Agent \"Write objective 032 soak scenario\" is waiting on its own background work</summary>\n<note>This agent stopped with background work of its own still running. It may resume on its own when that work completes or reports, and the same task-id notifies again if it does; the result below may be interim.</note>\n<result>The soak runs in the background; I will report when it ends.</result>\n<usage><subagent_tokens>362660</subagent_tokens><tool_uses>88</tool_uses><duration_ms>2226227</duration_ms></usage>\n</task-notification>","commandMode":"task-notification","origin":{"kind":"task-notification","producer":"session-task"}},"type":"attachment","uuid":"t22","version":"2.1.318"}"#;

        #[test]
        fn a_background_agent_runs_from_its_launch_to_its_notification_and_again_when_resumed() {
            const ID: &str = "a2adacaabc9f9b048";
            assert_eq!(jobs(AGENT_LAUNCH), [started(ID, JobKind::Agent)]);
            assert_eq!(
                keys(AGENT_LAUNCH),
                ["job:a2adacaabc9f9b048:started:toolu_018hgmBWFMc3fomfMC4sgXBo"]
            );
            // Its notification's task-id is the agentId: given mid-turn, then
            // as a prompt, byte for byte the same, so one end.
            assert_eq!(jobs(AGENT_MID_TURN), [ended(ID, JobEnd::Completed)]);
            let first = end_key(AGENT_MID_TURN, ID);
            assert_eq!(
                kinds(AGENT_DONE),
                vec![
                    ("job.ended".into(), Some(first.clone())),
                    ("message.received".into(), Some("54ce99c8".into())),
                ]
            );
            // A message resumes it: a new run, started by that call, ended by
            // a notification of its own (another result and usage), which
            // the first end's key does not swallow.
            assert_eq!(jobs(AGENT_RESUMED), [started(ID, JobKind::Agent)]);
            assert_eq!(
                keys(AGENT_RESUMED),
                ["job:a2adacaabc9f9b048:started:toolu_01C8SfVsFL53ceFvQyhH19yS"]
            );
            assert_eq!(jobs(AGENT_DONE_AGAIN), [ended(ID, JobEnd::Completed)]);
            assert_ne!(end_key(AGENT_DONE_AGAIN, ID), first);
            // Its other ends.
            let failed = AGENT_MID_TURN.replace("<status>completed", "<status>failed").replace(
                "finished</summary>",
                "failed: Agent terminated early due to an API error: API Error: 502 status code</summary>",
            );
            assert_eq!(jobs(&failed), [ended(ID, JobEnd::Failed)]);
            let killed = AGENT_MID_TURN
                .replace("<status>completed", "<status>killed")
                .replace("finished</summary>", "was stopped by Claude</summary>");
            assert_eq!(jobs(&killed), [ended(ID, JobEnd::Killed)]);
            let stopped = r#"{"type":"user","isSidechain":false,"uuid":"u30","version":"2.1.318","toolUseResult":{"message":"Successfully stopped task: a322ed2198f742e41 (Validate objective 026 variants)","task_id":"a322ed2198f742e41","task_type":"local_agent","command":"Validate objective 026 variants"},"message":{"role":"user","content":[{"tool_use_id":"toolu_01132fAc6a6EP9oKpFzTnwSS","type":"tool_result","content":"{\"message\":\"Successfully stopped task: a322ed2198f742e41 (Validate objective 026 variants)\",\"task_id\":\"a322ed2198f742e41\",\"task_type\":\"local_agent\",\"command\":\"Validate objective 026 variants\"}"}]}}"#;
            assert_eq!(jobs(stopped), [ended("a322ed2198f742e41", JobEnd::Stopped)]);
            // A Skill forked to run in the background is an Agent too: it
            // notifies under its agentId ("Agent \"/code-review …\" finished").
            let forked = r#"{"type":"user","isSidechain":false,"uuid":"7da83220","timestamp":"2026-10-28T09:49:33.065Z","version":"2.1.312","toolUseResult":{"success":true,"commandName":"code-review","status":"forked","background":true,"agentId":"aea25189ec7ec0cb0","result":"Running in the background as @code-review"},"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_863cd0d956c042169b9daf2732efa1e7","content":"Skill \"code-review\" launched (forked execution, running in the background).\n\nRunning in the background as @code-review"}]}}"#;
            assert_eq!(jobs(forked), [started("aea25189ec7ec0cb0", JobKind::Agent)]);
        }

        #[test]
        fn an_agent_that_only_paused_or_was_sent_a_message_is_still_running() {
            // Stopped with background work of its own still running: it may
            // resume on its own and notifies again under its id.
            assert!(kinds(AGENT_PAUSED).is_empty(), "{:?}", kinds(AGENT_PAUSED));
            let as_prompt = AGENT_DONE.replace(
                "finished</summary>",
                "is waiting on its own background work</summary>",
            );
            assert_eq!(
                kinds(&as_prompt),
                vec![("message.received".into(), Some("54ce99c8".into()))]
            );
            // Only the summary's end says so: a description that quotes the
            // words, in an agent that finished, is an end.
            let quoting = AGENT_MID_TURN.replace(
                "Build storage phase 2a\\\" finished",
                "Check is waiting on its own background work\\\" finished",
            );
            assert_ne!(quoting, AGENT_MID_TURN);
            assert_eq!(
                jobs(&quoting),
                [ended("a2adacaabc9f9b048", JobEnd::Completed)]
            );
            // A message queued for a running agent starts nothing.
            let queued = r#"{"type":"user","isSidechain":false,"uuid":"u31","version":"2.1.318","toolUseResult":{"success":true,"message":"Message queued for delivery to a90ac3ddd1ee082aa at its next tool round. Sending this moved nothing it is running to the background just now, so the message waits for that round unless something moves sooner. If it finishes first, it is resumed with the message and notifies again, so wait for that notification before resending.","pin":{"id":"a90ac3ddd1ee082aa","name":"a90ac3ddd1ee082aa","ref":"4b8ec1"}},"message":{"role":"user","content":[{"tool_use_id":"toolu_07","type":"tool_result","content":[{"type":"text","text":"{\"success\":true,\"message\":\"Message queued for delivery to a90ac3ddd1ee082aa at its next tool round.\"}"}]}]}}"#;
            assert!(jobs(queued).is_empty());
            // Without the structured copy, the engine's words.
            let launch = AGENT_LAUNCH.replace(
                r#""toolUseResult":{"isAsync":true,"status":"async_launched","agentId":"a2adacaabc9f9b048","description":"Build storage phase 2a","resolvedModel":"claude-opus-5-5","prompt":"Implement storage phase 2a for","outputFile":"/data/tmp/claude-1000/p/s/tasks/a2adacaabc9f9b048.output","canReadOutputFile":true,"canContinueAgent":true},"#,
                "",
            );
            assert_ne!(launch, AGENT_LAUNCH);
            assert_eq!(
                jobs(&launch),
                [started("a2adacaabc9f9b048", JobKind::Agent)]
            );
            let resumed = AGENT_RESUMED.replace(
                r#""toolUseResult":{"success":true,"message":"Resuming agent a2adaca","resumedAgentId":"a2adacaabc9f9b048","revivedAgentId":"a2adacaabc9f9b048","pin":{"id":"a2adacaabc9f9b048","name":"a2adacaabc9f9b048","ref":"6d82fa"}},"#,
                "",
            );
            assert_ne!(resumed, AGENT_RESUMED);
            assert_eq!(
                jobs(&resumed),
                [started("a2adacaabc9f9b048", JobKind::Agent)]
            );
        }

        #[test]
        fn the_engines_queue_is_not_read() {
            // A sub-agent's background job (its start is in the sub-agent's
            // own transcript): its end is queued in the session's queue, then
            // removed and handed to the sub-agent (2.1.312).
            let sub_agents = r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-10-30T14:23:49.953Z","sessionId":"b95b0598","content":"<task-notification>\n<task-id>b8zq0ev2d</task-id>\n<tool-use-id>toolu_015jsZnoPvD9rHp9A2vnbWAq</tool-use-id>\n<output-file>/data/tmp/claude-1000/p/b95b0598/tasks/b8zq0ev2d.output</output-file>\n<status>completed</status>\n<summary>Background command \"sparse checkout of TensorRT-LLM\" completed (exit code 0)</summary>\n</task-notification>"}"#;
            assert!(kinds(sub_agents).is_empty());
            // A quick job's end, queued before the line with the result that
            // started it (2.1.312): only the copy the agent is given, after
            // that result, ends it.
            let early = r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-10-28T21:22:22.200Z","sessionId":"9654af04","content":"<task-notification>\n<task-id>bf1cus1ov</task-id>\n<tool-use-id>call_a96802b9760049bdb0930dff1bccada5</tool-use-id>\n<status>failed</status>\n<summary>Background command \"Compile and run isolated world-one MPK Engram tasks\" failed with exit code 1</summary>\n</task-notification>"}"#;
            let start = r#"{"isSidechain":false,"type":"user","message":{"role":"user","content":[{"tool_use_id":"call_a96802b9760049bdb0930dff1bccada5","type":"tool_result","content":"Command running in background with ID: bf1cus1ov. Output is being written to: /data/tmp/claude-1000/p/9654af04/tasks/bf1cus1ov.output."}]},"uuid":"u40","toolUseResult":{"stdout":"","stderr":"","interrupted":false,"isImage":false,"noOutputExpected":false,"backgroundTaskId":"bf1cus1ov"},"version":"2.1.312"}"#;
            let given = early
                .replace(
                    r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-10-28T21:22:22.200Z","sessionId":"9654af04","content":"#,
                    r#"{"isSidechain":false,"type":"attachment","uuid":"t40","version":"2.1.312","attachment":{"type":"queued_command","commandMode":"task-notification","prompt":"#,
                )
                .replace(r#"</task-notification>"}"#, r#"</task-notification>"}}"#);
            assert!(kinds(early).is_empty());
            assert_eq!(jobs(start), [started("bf1cus1ov", JobKind::Bash)]);
            assert_eq!(jobs(&given), [ended("bf1cus1ov", JobEnd::Failed)]);
        }

        #[test]
        fn shapes_that_start_or_end_no_job_give_none() {
            // A prompt that quotes a notification (this task's own brief).
            let quoted = r#"{"isSidechain":false,"type":"user","message":{"role":"user","content":"Read it: a `user` message holds `<task-notification> <task-id><id></task-id> <status>completed</status>`."},"uuid":"u11","version":"2.1.318"}"#;
            assert_eq!(
                kinds(quoted),
                vec![("message.received".into(), Some("u11".into()))]
            );
            // A foreground command whose output quotes the engine's words.
            let quoting = r#"{"isSidechain":false,"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_04","type":"tool_result","content":"Command running in background with ID: b0uk1xi12.","is_error":false}]},"uuid":"u12","toolUseResult":{"stdout":"Command running in background with ID: b0uk1xi12.","stderr":"","interrupted":false,"isImage":false,"noOutputExpected":false},"version":"2.1.318"}"#;
            assert!(jobs(quoting).is_empty());
            // A sub-agent's background shell dies with the sub-agent.
            let side = BASH_RESULT.replace(r#""isSidechain":false"#, r#""isSidechain":true"#);
            assert!(kinds(&side).is_empty());
            // The queue handing a notification out again; other queue moves.
            let removed = BASH_QUEUED.replace(r#""enqueue""#, r#""remove""#);
            assert!(kinds(&removed).is_empty());
            assert!(
                kinds(
                    r#"{"type":"queue-operation","operation":"enqueue","content":"fix the build"}"#
                )
                .is_empty()
            );
            // Other attachments, and a status this build does not know.
            assert!(kinds(r#"{"type":"attachment","uuid":"t2","attachment":{"type":"task_status","taskId":"b8w03og7k","status":"running"}}"#).is_empty());
            let unknown = BASH_DONE.replace("<status>completed", "<status>vanished");
            assert_eq!(jobs(&unknown), [ended("b8w03og7k", JobEnd::Ended)]);
            let bad_id = BASH_DONE.replace("<task-id>b8w03og7k", "<task-id><id>");
            assert!(jobs(&bad_id).is_empty(), "no id, no job");
        }

        #[test]
        fn a_line_without_the_structured_copy_is_read_from_the_engines_words() {
            let bash = r#"{"isSidechain":false,"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_06","type":"tool_result","content":"Shell cwd was reset to /w\nCommand running in background with ID: b437vogks. Output is being written to: /tmp/b437vogks.output.","is_error":false}]},"uuid":"u14","version":"2.1.311"}"#;
            assert_eq!(jobs(bash), [started("b437vogks", JobKind::Bash)]);
            let monitor = MONITOR_RESULT.replace(
                r#","toolUseResult":{"taskId":"bv41gl0hm","timeoutMs":1800000,"persistent":false}"#,
                "",
            );
            assert_eq!(jobs(&monitor), [started("bv41gl0hm", JobKind::Monitor)]);
        }
    }

    mod tool_summaries {
        use crate::event::EventKind;
        use crate::ingest::{TOOL_SUMMARY_CHARS, claude_line, tool_summary, tool_summary_whole};
        use serde_json::json;

        fn summary(tool: &str, input: &serde_json::Value) -> Option<String> {
            tool_summary(tool, Some(input))
        }

        /// A transcript line's tool call carries its key input as the
        /// event's `summary`.
        #[test]
        fn a_tool_call_line_carries_its_summary() {
            let line = r#"{"type":"assistant","uuid":"a1","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"cargo test -p spawn-cli\necho done","description":"run the tests"}}],"stop_reason":"tool_use"}}"#;
            let events = claude_line(line);
            let kinds: Vec<&EventKind> = events.iter().map(|normalized| &normalized.kind).collect();
            assert!(
                matches!(
                    kinds.as_slice(),
                    [EventKind::ToolStarted { tool_name, summary: Some(summary), .. }]
                        if tool_name == "Bash" && summary == "cargo test …"
                ),
                "{kinds:?}"
            );
        }

        /// A fetched URL is kept as its host alone: a path, a query or a
        /// userinfo can carry a credential.
        #[test]
        fn a_fetch_keeps_its_host_only() {
            assert_eq!(
                summary(
                    "WebFetch",
                    &json!({"url": "https://docs.rs/tokio/latest?token=abc#x"})
                )
                .as_deref(),
                Some("docs.rs"),
                "the host alone: no scheme, path, query or fragment"
            );
            assert_eq!(
                summary(
                    "WebFetch",
                    &json!({"url": "https://bot:hunter2@example.com/a@b/c"})
                )
                .as_deref(),
                Some("b"),
                "an @ in the path after a userinfo is read as the userinfo's end: a wrong host, never a password"
            );
            // rev-e's probe: tokens in a path, and a password holding `/`.
            for (url, host) in [
                (
                    "https://api.telegram.org/bot123456:AAHdqTcvCH1vGWJxfSeofSAs0K5PALDsaw/getMe",
                    "api.telegram.org",
                ),
                (
                    "https://hooks.slack.com/services/T000/B000/XXXXXXXXXXXXXXXXXXXXXXXX",
                    "hooks.slack.com",
                ),
                ("https://u:pa/ss@example.com/x", "example.com"),
                ("https://u:p@ss@example.com:8443/x", "example.com"),
                ("u:hunter2pass@example.com/x", "example.com"),
                ("https://example.com/a@b", "example.com"),
            ] {
                assert_eq!(
                    summary("WebFetch", &json!({ "url": url })).as_deref(),
                    Some(host),
                    "{url}"
                );
            }
        }

        #[test]
        fn each_tool_says_its_key_input() {
            assert_eq!(
                summary("Bash", &json!({"command": "  git status  "})).as_deref(),
                Some("git status")
            );
            assert_eq!(
                summary(
                    "Edit",
                    &json!({"file_path": "crates/spawn-cli/src/integrate.rs", "old_string": "a"})
                )
                .as_deref(),
                Some("crates/spawn-cli/src/integrate.rs")
            );
            assert_eq!(
                summary("Write", &json!({"file_path": ".spawn/report.md"})).as_deref(),
                Some(".spawn/report.md")
            );
            assert_eq!(
                summary(
                    "Read",
                    &json!({"file_path": "docs/protocol.md", "offset": 120, "limit": 61})
                )
                .as_deref(),
                Some("docs/protocol.md:120-180")
            );
            assert_eq!(
                summary("Read", &json!({"file_path": "a.rs", "limit": 40})).as_deref(),
                Some("a.rs:1-40")
            );
            assert_eq!(
                summary("Read", &json!({"file_path": "a.rs"})).as_deref(),
                Some("a.rs")
            );
            assert_eq!(
                summary(
                    "Grep",
                    &json!({"pattern": "fn step", "path": "crates/spawn-cli"})
                )
                .as_deref(),
                Some("\"fn step\" in crates/spawn-cli")
            );
            assert_eq!(
                summary("Grep", &json!({"pattern": "x", "glob": "*.rs"})).as_deref(),
                Some("\"x\" in *.rs")
            );
            assert_eq!(
                summary("Glob", &json!({"pattern": "rsi/scenarios/0*.sh"})).as_deref(),
                Some("rsi/scenarios/0*.sh")
            );
            assert_eq!(
                summary("Task", &json!({"description": "find the retry path"})).as_deref(),
                Some("find the retry path")
            );
            assert_eq!(
                summary("TodoWrite", &json!({"todos": [{}, {}, {}]})).as_deref(),
                Some("3 items")
            );
            assert_eq!(
                summary("ScheduleWakeup", &json!({"delaySeconds": 60})),
                None,
                "a tool with no key input"
            );
            assert_eq!(summary("Bash", &json!({"description": "no command"})), None);
            assert_eq!(tool_summary("Bash", None), None);
        }

        /// A command's credentials never reach the summary: an assignment's
        /// value, a bearer token, a URL's password, an API key.
        #[test]
        fn a_summary_loses_the_credentials_a_command_carries() {
            assert_eq!(
                summary(
                    "Bash",
                    &json!({"command": "PGPASSWORD=s3cret psql -h db -U spawn"})
                )
                .as_deref(),
                Some("psql …"),
                "the set-up goes and the words stop at the first flag"
            );
            let curl = summary(
                "Bash",
                &json!({"command": "curl -H \"Authorization: Bearer abcdef0123456789abcdef\" https://api.example/v1"}),
            )
            .unwrap_or_default();
            assert!(!curl.contains("abcdef0123456789abcdef"), "{curl}");
            let url = summary(
                "Bash",
                &json!({"command": "psql postgres://spawn:pa55w0rd@db:5432/spawn"}),
            )
            .unwrap_or_default();
            assert!(!url.contains("pa55w0rd"), "{url}");
            let key = summary(
                "Bash",
                &json!({"command": "export X=sk-0123456789abcdefghij; run"}),
            )
            .unwrap_or_default();
            assert!(!key.contains("sk-0123456789abcdefghij"), "{key}");
        }

        /// The redaction runs before the cut: a key that the cut would split
        /// is gone whole, and a long summary is at most the limit.
        #[test]
        fn a_long_summary_is_cut_after_its_credentials_go() {
            let command = format!("{} sk-{}", "x".repeat(150), "a".repeat(30));
            let cut = summary("Bash", &json!({ "command": command })).unwrap_or_default();
            assert!(
                cut.chars().count() <= TOOL_SUMMARY_CHARS,
                "{}",
                cut.chars().count()
            );
            assert!(cut.ends_with('…'));
            assert!(!cut.contains("sk-aaaa"), "{cut}");
        }

        /// rev-e's probe (review-ui-ia-tools.md): every row's credential is
        /// gone from the summary a Bash call stores.
        #[test]
        fn a_commands_credentials_of_every_probed_shape_go() {
            let secrets = [
                (
                    "curl -u alice:hunter2pass https://api.example/v1",
                    "hunter2pass",
                ),
                (
                    "curl -H 'Authorization: Basic YWxpY2U6aHVudGVyMg==' https://x",
                    "YWxpY2U6aHVudGVyMg",
                ),
                (
                    "curl -H \"Authorization: s3cr3tR4wT0k3n\" https://x",
                    "s3cr3tR4wT0k3n",
                ),
                (
                    "curl -H 'Authorization: token 0123456789abcdef0123' https://x",
                    "0123456789abcdef0123",
                ),
                (
                    "curl -H 'PRIVATE-TOKEN: glpat-AbCdEf0123456789' https://x",
                    "AbCdEf0123456789",
                ),
                (
                    "curl -H 'X-Auth-Token: abcd1234efgh5678' https://x",
                    "abcd1234efgh5678",
                ),
                ("mysql --password hunter2pass -u root", "hunter2pass"),
                ("mysql -phunter2pass -u root", "hunter2pass"),
                ("sshpass -p hunter2pass ssh host", "hunter2pass"),
                ("DB_PASS=hunter2pass ./run", "hunter2pass"),
                ("PGPWD=hunter2pass ./run", "hunter2pass"),
                ("X_TOKEN=abcdef123456 ./run", "abcdef123456"),
                (
                    "DATABASE_URL=postgres://spawn:hunter2pass@db/spawn ./run",
                    "hunter2pass",
                ),
                ("psql postgres://spawn:hun@ter2pass@db/spawn", "ter2pass"),
                ("psql postgres://spawn:hun/ter2pass@db/spawn", "ter2pass"),
                (
                    "curl -d '{\"token\": \"abcdef123456\"}' https://x",
                    "abcdef123456",
                ),
                ("cat > .env <<EOF\nTOKEN=abcdef123456\nEOF", "abcdef123456"),
                (
                    "curl -H \"Authorization: Bearer \\\n  abcdef0123456789abcdef\" https://x",
                    "abcdef0123456789abcdef",
                ),
            ];
            for (command, secret) in secrets {
                let stored = summary("Bash", &json!({ "command": command })).unwrap_or_default();
                assert!(!stored.contains(secret), "{command:?} stored {stored:?}");
                assert!(!stored.is_empty(), "{command:?}");
            }
        }

        /// A summary is whole only when it is the call's entire input: two
        /// calls with the same whole summary are the same call.
        #[test]
        fn a_summary_is_whole_only_when_it_is_the_entire_input() {
            let whole =
                |tool: &str, input: serde_json::Value| tool_summary_whole(tool, Some(&input));
            assert!(whole(
                "Bash",
                json!({"command": "git status", "description": "status"})
            ));
            assert!(whole(
                "Read",
                json!({"file_path": "a.rs", "offset": 10, "limit": 20})
            ));
            assert!(whole("Glob", json!({"pattern": "*.rs", "path": "crates"})));
            assert!(whole(
                "Grep",
                json!({"pattern": "fn step", "path": "crates"})
            ));
            assert!(whole("WebSearch", json!({"query": "tokio select"})));

            // rev-e's cases: two Edits of one file differ in what they
            // change; two heredocs with one first line differ below it.
            assert!(!whole(
                "Edit",
                json!({"file_path": "a.rs", "old_string": "x", "new_string": "y"})
            ));
            assert!(!whole(
                "Write",
                json!({"file_path": "a.rs", "content": "x"})
            ));
            assert!(!whole(
                "Bash",
                json!({"command": "python3 - <<'EOF'\nprint(1)\nEOF"})
            ));
            assert!(!whole("Bash", json!({"command": "cat <<EOF > a"})));
            assert!(!whole("Bash", json!({"command": "make \\"})));
            assert!(!whole("Bash", json!({"command": "echo a\necho b"})));
            // A field the summary leaves out, a cut, a redaction.
            assert!(!whole(
                "Grep",
                json!({"pattern": "x", "path": "p", "-i": true})
            ));
            assert!(!whole(
                "Grep",
                json!({"pattern": "x", "path": "p", "glob": "*.rs"})
            ));
            assert!(!whole("Read", json!({"file_path": "a.rs", "pages": "1-2"})));
            assert!(!whole(
                "Bash",
                json!({"command": "ls", "run_in_background": true})
            ));
            assert!(!whole(
                "Bash",
                json!({"command": format!("echo {}", "x".repeat(200))})
            ));
            assert!(!whole("Bash", json!({"command": "PGPASSWORD=a psql"})));
            // Plain words the redactor still rewrites: the summary is not
            // the input any more.
            assert!(!whole(
                "Bash",
                json!({"command": "cat sk-abcdefghijklmnopqrst"})
            ));
            assert!(!whole(
                "Task",
                json!({"description": "find it", "prompt": "p"})
            ));
            assert!(!whole("WebFetch", json!({"url": "https://docs.rs"})));
            assert!(!whole("TodoWrite", json!({"todos": []})));
            assert!(!tool_summary_whole("Bash", None));
        }

        /// The transcript line's call carries whether its summary is whole.
        #[test]
        fn a_tool_call_line_says_whether_its_summary_is_whole() {
            let line = |command: &str| {
                let input = json!({"command": command});
                format!(
                    r#"{{"type":"assistant","uuid":"a1","message":{{"content":[{{"type":"tool_use","id":"toolu_1","name":"Bash","input":{input}}}],"stop_reason":"tool_use"}}}}"#
                )
            };
            for (command, expected) in [("git status", true), ("git status\ngit diff", false)] {
                let events = claude_line(&line(command));
                let started = events
                    .iter()
                    .find_map(|event| match &event.kind {
                        EventKind::ToolStarted { summary_whole, .. } => Some(*summary_whole),
                        _ => None,
                    })
                    .expect("a tool.started");
                assert_eq!(started, expected, "{command:?}");
            }
        }

        /// The lead's bound (structural, not a list of shapes): a command
        /// keeps its leading plain words, at most four.
        #[test]
        fn a_command_keeps_its_leading_plain_words_only() {
            let bash =
                |command: &str| summary("Bash", &json!({ "command": command })).unwrap_or_default();
            assert_eq!(bash("cargo test -p spawn-cli"), "cargo test …");
            assert_eq!(bash("git clone https://T@x"), "git clone …");
            assert_eq!(
                bash("B=/home/ckl/.local/share/spawn-rsi/bin/spawn; $B tail --session x"),
                "spawn tail …"
            );
            assert_eq!(bash("cd /w && cargo build --release"), "cargo build …");
            assert_eq!(bash("RUST_LOG=debug cargo run"), "cargo run");
            assert_eq!(bash("git status"), "git status");
            assert_eq!(bash("ls crates/spawn-core/src"), "ls crates/spawn-core/src");
            assert_eq!(bash("make a b c d e"), "make a b c …", "four words at most");
            assert_eq!(
                bash("git diff | head"),
                "git diff …",
                "the command ends at a pipe"
            );
            assert_eq!(
                bash("git status\ngit diff"),
                "git status …",
                "the first line only"
            );
            assert_eq!(bash("python3 - <<'EOF'\nprint(1)\nEOF"), "python3 …");
            assert_eq!(
                bash("cat a1b2c3d4e5f6g7h8i9"),
                "cat …",
                "a long mix of letters and digits"
            );
            assert_eq!(
                bash("cat abcdefghijklmnopqrstu"),
                "cat abcdefghijklmnopqrstu",
                "a long word"
            );
            assert_eq!(bash("$SECRET_CMD run"), "…", "nothing plain to keep");
            // Each never-kept character on its own, in a word short enough
            // that no other rule stops it.
            for c in ['@', ':', '=', '\'', '"', '`', '$', '\\'] {
                let word = format!("ab{c}cd");
                assert_eq!(bash(&format!("git {word} x")), "git …", "{word:?}");
            }
            assert_eq!(
                bash("cat target/debug/build/x-0123456789abcdef01/out"),
                "cat …",
                "a path with a long mixed segment"
            );
            // Paths, patterns and descriptions are bounded the same way.
            assert_eq!(
                summary("Read", &json!({"file_path": "/a/tok0123456789abcdef/b.rs"})).as_deref(),
                Some("/a/…")
            );
            assert_eq!(
                summary("Grep", &json!({"pattern": "token=", "path": "crates"})).as_deref(),
                Some("… in crates")
            );
            assert_eq!(
                summary(
                    "Task",
                    &json!({"description": "use key a1b2c3d4e5f6g7h8i9 here"})
                )
                .as_deref(),
                Some("use key …")
            );
            assert_eq!(
                summary(
                    "WebSearch",
                    &json!({"query": "why does sk1a2b3c4d5e6f7g8h9 fail"})
                )
                .as_deref(),
                Some("why does …")
            );
        }

        /// Every probe of rev-e's two rounds (review-ui-ia-tools.md): the
        /// secret is not in the summary, by construction.
        #[test]
        fn every_probed_secret_is_out_of_the_summary() {
            let bash =
                |command: &str| summary("Bash", &json!({ "command": command })).unwrap_or_default();
            let fetch = |url: &str| summary("WebFetch", &json!({ "url": url })).unwrap_or_default();
            for (stored, secret) in [
                (
                    bash("curl -H 'Authorization:Basic YWxpY2U6aHVudGVyMg==' https://x"),
                    "YWxpY2U6aHVudGVyMg",
                ),
                (
                    bash("curl -H 'Authorization: abcdefSECRETxyz' -H 'Accept: a' https://x"),
                    "SECRET",
                ),
                (bash("http GET x Authorization:abcdefSECRETxyz"), "SECRET"),
                (
                    bash("curl -H 'Cookie: theme=dark; sessionid=abc123SECRET' https://x"),
                    "SECRET",
                ),
                (bash("curl -b 'sessionid=abc123SECRET' https://x"), "SECRET"),
                (
                    bash("curl -H 'apikey: eyJhbGciOiJIUzI1NiJ9SECRET' https://x"),
                    "SECRET",
                ),
                (
                    bash("docker login -u alice -p hunter2pass registry.example"),
                    "hunter2pass",
                ),
                (bash("vault login --token hunter2pass"), "hunter2pass"),
                (bash("tool --api-key hunter2pass run"), "hunter2pass"),
                (bash("az login --client-secret hunter2pass"), "hunter2pass"),
                (bash("curl -ualice:hunter2pass https://x"), "hunter2pass"),
                (
                    bash("curl --user=alice:hunter2pass https://x"),
                    "hunter2pass",
                ),
                (
                    bash("git clone https://abcdef0123SECRETtoken@github.com/o/r"),
                    "SECRET",
                ),
                (
                    bash("cd db && mysql -u root -phunter2pass app"),
                    "hunter2pass",
                ),
                (bash("PGPASSWORD=hunter2pass psql"), "hunter2pass"),
                (
                    fetch("https://abcdef0123SECRETtoken@example.com/x"),
                    "SECRET",
                ),
            ] {
                assert!(!stored.contains(secret), "{stored:?} holds {secret:?}");
            }
            // A host that is itself token-shaped is not kept either.
            assert_eq!(
                summary(
                    "WebFetch",
                    &json!({"url": "https://k7a1b2c3d4e5f6g7h8x9.example/x"})
                ),
                None
            );
            // The same flags meaning something else: still no more than the
            // words before them.
            assert_eq!(bash("ssh -p 2222 host"), "ssh …");
            assert_eq!(bash("PASSPORT_DIR=/tmp ./run"), "./run");
        }

        /// Fuzz, seeded: a secret-shaped token put in any argument position
        /// of a command, or any segment of a path, never reaches the summary.
        #[test]
        fn a_token_in_any_position_never_reaches_the_summary() {
            const ALNUM: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
            const SPECIAL: &[u8] = b"@:='\"`$\\";
            let words = [
                "git", "curl", "run", "x", "status", "push", "origin", "main", "a/b", "-v",
                "./tool",
            ];
            let mut state: u64 = 0x5eed_2026_1109;
            // A number below `below`, from a fixed-seed generator.
            let mut pick = move |below: usize| -> usize {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                usize::try_from(state >> 33).expect("31 bits fit") % below
            };
            for round in 0..2_000 {
                // A token: a long mix of letters and digits, or a word
                // holding one of the never-kept characters.
                let len = 16 + pick(24);
                let mut token: Vec<u8> = (0..len).map(|_| ALNUM[pick(ALNUM.len())]).collect();
                token[0] = b'k';
                token[1] = b'7';
                if round % 2 == 1 {
                    let at = 2 + pick(len - 2);
                    token[at] = SPECIAL[pick(SPECIAL.len())];
                }
                let token = String::from_utf8(token).expect("ascii");
                let count = 1 + pick(6);
                let mut argv: Vec<String> = (0..count)
                    .map(|_| words[pick(words.len())].to_string())
                    .collect();
                argv.insert(pick(count + 1), token.clone());
                let command = argv.join(" ");
                let stored = summary("Bash", &json!({ "command": command })).unwrap_or_default();
                // The token's letters-and-digits stretches are what a
                // secret is; none of length 8 or more may be stored.
                for piece in token
                    .split(|c: char| !c.is_ascii_alphanumeric())
                    .filter(|piece| piece.len() >= 8)
                {
                    assert!(!stored.contains(piece), "{command:?} stored {stored:?}");
                }
                // A description or a query is prose: the same token pasted
                // anywhere in it is not stored either.
                let prose = argv.join(" ");
                for (tool, field) in [("Task", "description"), ("WebSearch", "query")] {
                    let stored = summary(tool, &json!({ field: prose })).unwrap_or_default();
                    for piece in token
                        .split(|c: char| !c.is_ascii_alphanumeric())
                        .filter(|piece| piece.len() >= 8)
                    {
                        assert!(
                            !stored.contains(piece),
                            "{tool} {prose:?} stored {stored:?}"
                        );
                    }
                }
                let path = format!("/w/{}/f.rs", argv.join("/"));
                let read = summary("Read", &json!({ "file_path": path })).unwrap_or_default();
                for piece in token
                    .split(|c: char| !c.is_ascii_alphanumeric())
                    .filter(|piece| piece.len() >= 8)
                {
                    assert!(!read.contains(piece), "{path:?} stored {read:?}");
                }
            }
        }

        /// The lead's narrowing on 04a816cc: a letters-and-digits mix of 8
        /// stops a command (a path's segments keep the 16 rule), and what a
        /// command is left to keep after it is written down.
        #[test]
        fn a_mix_of_eight_stops_a_word_and_a_path_segment_keeps_sixteen() {
            let bash =
                |command: &str| summary("Bash", &json!({ "command": command })).unwrap_or_default();
            assert_eq!(bash("git checkout ab12cd34"), "git checkout …");
            assert_eq!(bash("mysql app hunter22 x"), "mysql app …");
            // A path's segment of 8 to 15 mixed characters is a name.
            assert_eq!(bash("ls crates/ab12cd34ef/x"), "ls crates/ab12cd34ef/x");
            assert_eq!(bash("ls crates/ab12cd34ef56gh78/x"), "ls …");
            assert_eq!(
                summary("Read", &json!({"file_path": "a/sha256sum/v2x86x64/b.rs"})).as_deref(),
                Some("a/sha256sum/v2x86x64/b.rs")
            );
            assert_eq!(
                summary(
                    "WebFetch",
                    &json!({"url": "https://ec2-54-12-1-2.compute.amazonaws.com/x"})
                )
                .as_deref(),
                Some("ec2-54-12-1-2.compute.amazonaws.com")
            );
            // A path's segment is held to the never-kept characters and
            // the flag rule as a word is, however short.
            for segment in [
                "ab@cd", "u:pw", "k=v", "a'b", "a\"b", "a`b", "$X", "a\\b", "-p",
            ] {
                assert_eq!(
                    summary("Read", &json!({"file_path": format!("/a/{segment}/b.rs")})).as_deref(),
                    Some("/a/…"),
                    "{segment:?}"
                );
            }
            // The residual, by design: shorter than 8, or unmixed, in a
            // positional slot, is kept.
            assert_eq!(bash("git checkout abc1234"), "git checkout abc1234");
            assert_eq!(bash("mysql app hunter"), "mysql app hunter");
            assert_eq!(bash("cat 12345678901234567890"), "cat 12345678901234567890");
        }

        /// Programs whose arguments are what they print, set or pass on
        /// keep the program word alone.
        #[test]
        fn a_program_that_passes_its_arguments_on_keeps_its_name_only() {
            let bash =
                |command: &str| summary("Bash", &json!({ "command": command })).unwrap_or_default();
            for program in [
                "echo", "printf", "read", "sudo", "su", "sshpass", "passwd", "export", "eval",
                "set",
            ] {
                assert_eq!(
                    bash(&format!("{program} hunter x")),
                    format!("{program} …"),
                    "{program}"
                );
                assert_eq!(bash(program), program, "{program} alone is whole");
            }
            assert_eq!(bash("/usr/bin/sudo systemctl restart x"), "/usr/bin/sudo …");
            // rev-e on 69522339: `env`'s arguments are assignments and a
            // program, as `sudo`'s are.
            assert_eq!(bash("env hunter2 ls"), "env …");
            assert_eq!(bash("env FOO=1 cargo test"), "env …");
            assert_eq!(bash("cd /w && echo hunter"), "echo …");
            assert_eq!(
                bash("git status"),
                "git status",
                "other programs keep their words"
            );
        }
    }
}
