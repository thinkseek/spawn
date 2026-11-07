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

use crate::event::{EventKind, NewEvent, Source};
use crate::id::{AgentId, WorkspaceId};
use crate::port::{READ_LIMIT, TerminalHost, TranscriptArchive};

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
        return Ok(0);
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
            return Ok(total);
        }
        bytes = read(length)?.unwrap_or_default();
        full = bytes.len() >= READ_LIMIT;
    }
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

    use super::{archive, restore};
    use crate::id::{AgentId, WorkspaceId};
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
