//! The transcript archive: Spawn's own copy of each native session.
//!
//! Claude Code's transcript (one JSONL file per native session, under its
//! projects directory) is the agent's history, and `--resume` needs it. It
//! is not Spawn's to keep: Claude Code deletes old transcripts (its
//! `cleanupPeriodDays` sweep runs at every engine start, over the shared
//! projects directory), and the format is the engine's. So the Bridge copies what the
//! engine appended since the last copy into a [`TranscriptArchive`] at each
//! turn end and at the end of the execution ([`archive`]), and before a
//! resume puts a missing file back from it ([`restore`]).
//!
//! The file is append-only. A file that shrank, or whose bytes before the
//! end of the last copy changed (rewritten, replaced), is copied whole as
//! the next copy, so archived bytes are never overwritten. The event log
//! holds no transcript content and no record of the copies: the archive is
//! its own index.
//!
//! Beside the copies, the Bridge keeps one [`ArchiveProgress`] per native
//! session: how far the archive has the transcript, and since when Spawn
//! knows it. It is what tells a reader of the archive whether a window may
//! still gain a model call ([`ArchiveProgress::holds_open`]: the window is
//! *settling*), and what a restarted Bridge reads to go on copying the
//! transcript of an execution that ended behind.

use serde::{Deserialize, Serialize};

use crate::event::{EventKind, NewEvent, Source};
use crate::id::{AgentId, ExecutionId, SessionId, WorkspaceId};
use crate::port::{READ_LIMIT, TerminalHost, TranscriptArchive};

/// Where the archive stands with one native session's transcript, as the
/// Bridge last knew it.
///
/// Every line the engine wrote before `known_ms` is either archived or at
/// or after the first line that is not, whose time is `behind_ms`. So a
/// window ending at `until` may still gain a line exactly while
/// [`Self::holds_open`] says so: a line before `until` is known not to be
/// archived, or the execution was live and Spawn has not looked at its
/// transcript since `until`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveProgress {
    pub agent: AgentId,
    pub native: String,
    pub session: SessionId,
    /// The execution that ran the session last.
    pub execution: ExecutionId,
    /// Its workspace: where the transcript is read from
    /// ([`crate::bridge::transcript_path`]).
    pub workspace: WorkspaceId,
    /// The execution had not ended when this was written.
    pub live: bool,
    /// Until when (epoch ms, the Bridge's clock, less a margin for a line
    /// written as it was read) the transcript is known.
    pub known_ms: i64,
    /// The length of the latest copy after the last copy that succeeded,
    /// if one did.
    pub archived: Option<u64>,
    /// The time of the first line the archive lacks (its `timestamp`;
    /// `i64::MIN` when it has none that reads), `None` when the archive
    /// has everything the transcript held.
    pub behind_ms: Option<i64>,
}

impl ArchiveProgress {
    /// Whether the archive may lack a line written before `until_ms`.
    pub fn holds_open(&self, until_ms: i64) -> bool {
        self.behind_ms.is_some_and(|at| at < until_ms) || (self.live && self.known_ms < until_ms)
    }

    /// The record as one JSON line.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// A record written by [`Self::to_json`]; `None` when it is not one.
    pub fn from_json(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }
}

/// Whether a window ending at `until_ms` is still settling: some record
/// says the archive may lack a line written before it.
pub fn settling(progress: &[ArchiveProgress], until_ms: i64) -> bool {
    progress.iter().any(|record| record.holds_open(until_ms))
}

/// What one [`archive_to`] did: the bytes copied, and the length of the
/// latest copy after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Archived {
    pub copied: u64,
    pub length: Option<u64>,
}

/// Bytes before the end of the latest copy that are compared with the file
/// to tell a file that went on from one that was replaced.
const OVERLAP: u64 = 4096;

/// Copy what the transcript at `path` gained since the latest copy of
/// `native` into the archive: the bytes copied (0 when the file is missing
/// or has nothing new).
pub fn archive(
    host: &dyn TerminalHost,
    archive: &dyn TranscriptArchive,
    workspace: &WorkspaceId,
    path: &str,
    agent: &AgentId,
    native: &str,
) -> Result<u64, String> {
    archive_to(host, archive, workspace, path, agent, native).map(|done| done.copied)
}

/// [`archive`], with the length of the latest copy after it.
pub fn archive_to(
    host: &dyn TerminalHost,
    archive: &dyn TranscriptArchive,
    workspace: &WorkspaceId,
    path: &str,
    agent: &AgentId,
    native: &str,
) -> Result<Archived, String> {
    let read = |offset: u64| {
        host.read_from(workspace, path, offset)
            .map_err(|error| format!("read {path}: {error}"))
    };
    let copies = archive
        .copies(agent, native)
        .map_err(|error| error.to_string())?;
    let (mut copy, mut length) = copies
        .last()
        .map_or((0, 0), |length| (copies.len() - 1, *length));
    let overlap = length.min(OVERLAP);
    let Some(mut bytes) = read(length - overlap)? else {
        return Ok(Archived {
            copied: 0,
            length: copies.last().copied(),
        });
    };
    let mut full = bytes.len() >= READ_LIMIT;
    if overlap > 0 {
        let width = usize::try_from(overlap).unwrap_or(usize::MAX);
        let archived = archive
            .read(agent, native, copy, length - overlap, width)
            .map_err(|error| error.to_string())?;
        if bytes.get(..width) == Some(archived.as_slice()) {
            bytes.drain(..width);
        } else {
            // Shrunk or replaced: the whole file, as the next copy.
            copy += 1;
            length = 0;
            bytes = read(0)?.unwrap_or_default();
            full = bytes.len() >= READ_LIMIT;
        }
    }
    let mut total = 0;
    loop {
        if !bytes.is_empty() {
            archive
                .append(agent, native, copy, length, &bytes)
                .map_err(|error| error.to_string())?;
            let grown = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            length += grown;
            total += grown;
        }
        if !full {
            return Ok(Archived {
                copied: total,
                length: Some(length),
            });
        }
        bytes = read(length)?.unwrap_or_default();
        full = bytes.len() >= READ_LIMIT;
    }
}

/// The time of the first line of the transcript at `path` that is not
/// whole before byte `offset` (`timestamp`, RFC 3339; the line `offset`
/// falls inside counts, as its end is not archived): `None` when the file
/// holds nothing from `offset` on, `i64::MIN` when what it holds has no
/// time that reads (a reader must take it as earlier than anything).
pub fn first_line_ms(
    host: &dyn TerminalHost,
    workspace: &WorkspaceId,
    path: &str,
    offset: u64,
) -> Result<Option<i64>, String> {
    let start = offset.saturating_sub(OVERLAP);
    let bytes = host
        .read_from(workspace, path, start)
        .map_err(|error| format!("read {path}: {error}"))?
        .unwrap_or_default();
    let cut = usize::try_from(offset - start).unwrap_or(usize::MAX);
    if bytes.len() <= cut {
        return Ok(None);
    }
    let from = match bytes[..cut].iter().rposition(|byte| *byte == b'\n') {
        Some(newline) => newline + 1,
        None if start == 0 => 0,
        // The line began before what was read: its time is not known.
        None => return Ok(Some(i64::MIN)),
    };
    // From the start of the line `offset` falls in, if it began before.
    let at = bytes[from..].split(|byte| *byte == b'\n').find_map(|line| {
        serde_json::from_slice::<serde_json::Value>(line)
            .ok()?
            .get("timestamp")?
            .as_str()
            .and_then(parse_rfc3339_ms)
    });
    Ok(Some(at.unwrap_or(i64::MIN)))
}

/// Epoch milliseconds of an RFC 3339 time (`2026-11-10T08:00:00.123Z`, an
/// offset instead of `Z`, any digits of fraction), as a transcript line's
/// `timestamp` is written.
pub fn parse_rfc3339_ms(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let number = |from: usize, to: usize| -> Option<i64> {
        let part = text.get(from..to)?;
        part.bytes()
            .all(|byte| byte.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't' | b' ')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let (year, month, day) = (number(0, 4)?, number(5, 7)?, number(8, 10)?);
    let (hour, minute, second) = (number(11, 13)?, number(14, 16)?, number(17, 19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    if second > 60 {
        return None;
    }
    let mut rest = &text[19..];
    let mut millis = 0;
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let mut scaled = String::from(&fraction[..digits.min(3)]);
        while scaled.len() < 3 {
            scaled.push('0');
        }
        millis = scaled.parse::<i64>().ok()?;
        rest = &fraction[digits..];
    }
    let offset_minutes = match rest {
        "Z" | "z" => 0,
        zone if zone.len() == 6 && matches!(zone.as_bytes()[0], b'+' | b'-') => {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let hours: i64 = zone.get(1..3)?.parse().ok()?;
            let minutes: i64 = zone.get(4..6)?.parse().ok()?;
            if zone.as_bytes()[3] != b':' {
                return None;
            }
            sign * (hours * 60 + minutes)
        }
        _ => return None,
    };
    // Days from 1970-01-01 to the civil date (Howard Hinnant's algorithm).
    let shifted = if month <= 2 { year - 1 } else { year };
    let era = shifted.div_euclid(400);
    let of_era = shifted - era * 400;
    let of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let of_cycle = of_era * 365 + of_era / 4 - of_era / 100 + of_year;
    let days = era * 146_097 + of_cycle - 719_468;
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset_minutes * 60;
    Some(seconds * 1000 + millis)
}

/// Before a resume of `native`: the transcript is where the engine will
/// look for it. A missing file is put back from the latest copy: the bytes
/// restored, or `None` when nothing was missing, or the session never had a
/// transcript (`read_before`: the log holds events read from one, so it
/// did). A transcript that is gone with no copy is an error: resuming would
/// start the agent blank under its old id.
pub fn restore(
    host: &dyn TerminalHost,
    archive: Option<&dyn TranscriptArchive>,
    workspace: &WorkspaceId,
    path: &str,
    agent: &AgentId,
    native: &str,
    read_before: bool,
) -> Result<Option<u64>, String> {
    let copies = match archive {
        Some(archive) => archive
            .copies(agent, native)
            .map_err(|error| error.to_string())?,
        None => Vec::new(),
    };
    // Read from the end of the latest copy: only what is newer comes back.
    let probe = copies.last().copied().unwrap_or(0);
    let present = host
        .read_from(workspace, path, probe)
        .map_err(|error| format!("read {path}: {error}"))?
        .is_some();
    if present {
        return Ok(None);
    }
    let (Some(archive), Some(length)) = (archive, copies.last().copied()) else {
        if read_before {
            return Err(format!(
                "the transcript of native session {native} is gone from {path} and the \
                 transcript archive holds no copy; resuming would start the agent blank under \
                 its old id"
            ));
        }
        return Ok(None);
    };
    let copy = copies.len() - 1;
    let mut bytes = Vec::new();
    while u64::try_from(bytes.len()).unwrap_or(u64::MAX) < length {
        let offset = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let chunk = archive
            .read(agent, native, copy, offset, READ_LIMIT)
            .map_err(|error| error.to_string())?;
        if chunk.is_empty() {
            return Err(format!(
                "copy {copy} of the transcript of {native} ends at {offset} bytes, not {length}"
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    host.write_new(workspace, path, &bytes)
        .map_err(|error| format!("restore {path}: {error}"))?;
    Ok(Some(length))
}

/// Whether `event` was read from a native transcript: what tells a session
/// whose transcript went missing from one that never wrote any.
pub fn read_from_transcript(event: &NewEvent) -> bool {
    event.source == Source::Claude
        && matches!(
            event.kind,
            EventKind::MessageReceived
                | EventKind::NoticeReceived
                | EventKind::ToolStarted { .. }
                | EventKind::ToolCompleted { .. }
                | EventKind::ToolFailed { .. }
                | EventKind::TurnStopped { .. }
                | EventKind::TurnFailed { .. }
                | EventKind::JobStarted { .. }
                | EventKind::JobEnded { .. }
                | EventKind::NativeSessionStarted {
                    engine_version: Some(_),
                    ..
                }
        )
}

#[cfg(all(test, feature = "testing"))]
mod tests {
    use std::sync::Arc;

    use super::{
        ArchiveProgress, archive, archive_to, first_line_ms, parse_rfc3339_ms, restore, settling,
    };
    use crate::id::{AgentId, WorkspaceId};
    use crate::id::{ExecutionId, SessionId};
    use crate::memory::MemoryStore;
    use crate::port::{READ_LIMIT, TerminalHost, TranscriptArchive};
    use crate::time::SystemClock;

    const PATH: &str = "/projects/-w/n1.jsonl";

    fn setup() -> (MemoryStore, AgentId, WorkspaceId) {
        (
            MemoryStore::new("shop", Arc::new(SystemClock)),
            AgentId::new("agent_one").expect("valid agent"),
            WorkspaceId::new("w:/w").expect("valid workspace"),
        )
    }

    fn step(store: &MemoryStore, agent: &AgentId, workspace: &WorkspaceId) -> u64 {
        archive(store, store, workspace, PATH, agent, "n1").expect("archive")
    }

    fn copies(store: &MemoryStore, agent: &AgentId) -> Vec<Vec<u8>> {
        let lengths = store.copies(agent, "n1").expect("copies");
        (0..lengths.len())
            .map(|copy| {
                store
                    .read(agent, "n1", copy, 0, usize::MAX)
                    .expect("read copy")
            })
            .collect()
    }

    #[test]
    fn what_the_engine_appends_is_copied_once() {
        let (store, agent, workspace) = setup();
        assert_eq!(step(&store, &agent, &workspace), 0, "no file yet");
        assert!(copies(&store, &agent).is_empty());
        store.append_file(PATH, "{\"a\":1}\n");
        assert_eq!(step(&store, &agent, &workspace), 8);
        assert_eq!(step(&store, &agent, &workspace), 0, "nothing new");
        store.append_file(PATH, "{\"b\":2}\n{\"c\"");
        assert_eq!(step(&store, &agent, &workspace), 12);
        assert_eq!(
            copies(&store, &agent),
            vec![b"{\"a\":1}\n{\"b\":2}\n{\"c\"".to_vec()]
        );
    }

    #[test]
    fn a_copy_says_how_long_the_archive_is_after_it() {
        let (store, agent, workspace) = setup();
        let copy = |store: &MemoryStore| {
            archive_to(store, store, &workspace, PATH, &agent, "n1").expect("archive")
        };
        assert_eq!(copy(&store).length, None, "nothing archived, no file");
        store.append_file(PATH, "{\"a\":1}\n");
        assert_eq!(copy(&store).length, Some(8));
        let again = copy(&store);
        assert_eq!((again.copied, again.length), (0, Some(8)));
    }

    #[test]
    fn rfc3339_times_read_as_epoch_milliseconds() {
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:01.5Z"), Some(1_500));
        assert_eq!(
            parse_rfc3339_ms("2026-11-10T08:15:30.123Z"),
            Some(1_794_298_530_123)
        );
        assert_eq!(
            parse_rfc3339_ms("2026-11-10T09:15:30.123+01:00"),
            Some(1_794_298_530_123)
        );
        assert_eq!(
            parse_rfc3339_ms("2000-03-01T00:00:00.000Z"),
            Some(951_868_800_000)
        );
        assert_eq!(
            parse_rfc3339_ms("2024-02-29T23:59:59.999999Z"),
            Some(1_709_251_199_999)
        );
        assert_eq!(parse_rfc3339_ms("2024-02-29"), None);
        assert_eq!(parse_rfc3339_ms("2024-13-01T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_ms("not a time at all, no"), None);
    }

    #[test]
    fn the_first_line_the_archive_lacks_is_read_from_its_start() {
        let (store, _, workspace) = setup();
        assert_eq!(
            first_line_ms(&store, &workspace, PATH, 0),
            Ok(None),
            "no file"
        );
        let one = "{\"timestamp\":\"1970-01-01T00:00:01.000Z\"}\n";
        let two = "{\"type\":\"summary\"}\n{\"timestamp\":\"1970-01-01T00:00:02.000Z\"}\n";
        store.append_file(PATH, one);
        store.append_file(PATH, two);
        let length = u64::try_from(one.len() + two.len()).expect("fits");
        let first = u64::try_from(one.len()).expect("fits");
        assert_eq!(first_line_ms(&store, &workspace, PATH, 0), Ok(Some(1_000)));
        // A line with no time is passed over for the next that has one.
        assert_eq!(
            first_line_ms(&store, &workspace, PATH, first),
            Ok(Some(2_000))
        );
        // An offset inside a line: that line, whose end is not archived.
        assert_eq!(first_line_ms(&store, &workspace, PATH, 5), Ok(Some(1_000)));
        assert_eq!(first_line_ms(&store, &workspace, PATH, length), Ok(None));
        // What holds no time that reads is earlier than anything.
        store.append_file(PATH, "{\"half");
        assert_eq!(
            first_line_ms(&store, &workspace, PATH, length),
            Ok(Some(i64::MIN))
        );
    }

    fn record(live: bool, known_ms: i64, behind_ms: Option<i64>) -> ArchiveProgress {
        ArchiveProgress {
            agent: AgentId::new("agent_one").expect("agent"),
            native: "n1".to_string(),
            session: SessionId::new("session_one").expect("session"),
            execution: ExecutionId::new("exec_1").expect("execution"),
            workspace: WorkspaceId::new("w:/w").expect("workspace"),
            live,
            known_ms,
            archived: Some(10),
            behind_ms,
        }
    }

    #[test]
    fn a_window_settles_once_nothing_before_its_end_can_be_missing() {
        // Live, all archived, known until 100: windows to 100 have settled.
        let idle = record(true, 100, None);
        assert!(!idle.holds_open(100));
        assert!(idle.holds_open(101), "not known since the window's end");
        // Ended with everything archived: holds nothing open, ever.
        assert!(!record(false, 100, None).holds_open(i64::MAX));
        // A line from 50 on not archived, live or ended: open past 50.
        for live in [true, false] {
            let behind = record(live, 100, Some(50));
            assert!(!behind.holds_open(50), "a call after the end holds nothing");
            assert!(behind.holds_open(51));
        }
        assert!(!settling(&[], 1_000));
        assert!(settling(&[idle.clone(), record(false, 0, Some(10))], 20));
        assert!(!settling(&[idle.clone(), record(false, 0, Some(10))], 10));
        let json = idle.to_json();
        assert_eq!(ArchiveProgress::from_json(json.as_bytes()), Some(idle));
        assert_eq!(ArchiveProgress::from_json(b"{\"live\":"), None);
    }

    #[test]
    fn a_shrunk_or_replaced_file_is_copied_whole_under_the_next_copy() {
        let (store, agent, workspace) = setup();
        store.append_file(PATH, "line one\nline two\n");
        step(&store, &agent, &workspace);
        // Truncated: shorter than what was copied.
        store.replace_file(PATH, b"line one\n");
        step(&store, &agent, &workspace);
        // Replaced by a file as long, but other bytes.
        store.replace_file(PATH, b"LINE ONE\n and more\n");
        step(&store, &agent, &workspace);
        store.append_file(PATH, "next\n");
        step(&store, &agent, &workspace);
        assert_eq!(
            copies(&store, &agent),
            vec![
                b"line one\nline two\n".to_vec(),
                b"line one\n".to_vec(),
                b"LINE ONE\n and more\nnext\n".to_vec(),
            ],
            "every earlier copy is kept as it was"
        );
    }

    #[test]
    fn a_file_over_the_read_limit_is_copied_in_reads() {
        let (store, agent, workspace) = setup();
        let big = "x".repeat(READ_LIMIT + 10);
        store.append_file(PATH, &big);
        assert_eq!(
            step(&store, &agent, &workspace),
            u64::try_from(big.len()).expect("fits")
        );
        assert_eq!(copies(&store, &agent)[0].len(), big.len());
    }

    #[test]
    fn a_missing_transcript_is_restored_from_the_latest_copy() {
        let (store, agent, workspace) = setup();
        store.append_file(PATH, "old\n");
        step(&store, &agent, &workspace);
        store.replace_file(PATH, b"new\n");
        step(&store, &agent, &workspace);
        store.remove_file(PATH);
        let restored = restore(&store, Some(&store), &workspace, PATH, &agent, "n1", true);
        assert_eq!(restored, Ok(Some(4)));
        assert_eq!(
            store.read_from(&workspace, PATH, 0).expect("read"),
            Some(b"new\n".to_vec())
        );
        // Present now: nothing to do, and the archive goes on as before.
        assert_eq!(
            restore(&store, Some(&store), &workspace, PATH, &agent, "n1", true),
            Ok(None)
        );
        store.append_file(PATH, "more\n");
        step(&store, &agent, &workspace);
        assert_eq!(copies(&store, &agent).len(), 2, "the restored file went on");
    }

    #[test]
    fn a_transcript_gone_without_a_copy_fails_the_resume() {
        let (store, agent, workspace) = setup();
        let gone = restore(&store, Some(&store), &workspace, PATH, &agent, "n1", true)
            .expect_err("events were read from it");
        assert!(gone.contains("start the agent blank"), "{gone}");
        assert!(restore(&store, None, &workspace, PATH, &agent, "n1", true).is_err());
        // A session that never wrote a transcript has nothing to lose.
        assert_eq!(
            restore(&store, Some(&store), &workspace, PATH, &agent, "n1", false),
            Ok(None)
        );
    }
}
