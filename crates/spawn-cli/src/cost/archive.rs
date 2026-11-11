//! The cost of what only the transcript archive holds (objective 048;
//! docs/fleet-architecture.md §3.5).
//!
//! The cost step counts a message from the Bridge's `model.usage` events
//! alone, so an execution the Bridge read before it emitted them (every one
//! that ended before 041), or whose events are gone, is in the archive and
//! in no cost. Each time the loop starts, [`start`] counts, beside its
//! passes and without holding any of them up, every assistant message of
//! the archive (`<root>/.transcripts/<agent>/<native>.jsonl` and its copies
//! `<native>.<n>.jsonl`; never the live transcripts, never the event log)
//! that the cost record lacks: by 041's rules (once per `message.id` across
//! every line and copy file of the agent, its largest usage, at its earliest
//! line's time, under that line's model; crate spawn-core's
//! `ingest::claude_usage` reads a line as the Bridge does), for the agent
//! whose directory holds it, priced when recorded. The store places it
//! (`spawn_cost_archive_record`: the agent's execution and task at the
//! message's time) and keeps any message it already holds as it is, so
//! counting again adds nothing.
//!
//! The work is bounded by what the archive holds, one agent at a time: an
//! agent whose files are all as the store marked them when they were last
//! counted (their length and modification time) is not read again; the
//! others are read whole (a message's lines may sit in any copy) and sent
//! in batches of [`BATCH`]. A file that cannot be read, or a line that is
//! not JSON, is skipped; an agent whose count fails is logged and left
//! unmarked, so the next start counts it again.

use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::Duration;

use spawn_core::event::EventKind;
use spawn_postgres::{ArchiveCounted, ArchiveFile, ArchivedMessage, PostgresStore};

use super::{Prices, plural};

/// The most messages one call to the store carries.
pub(crate) const BATCH: usize = 500;

/// The longest the counter waits between tries to reach the database.
const MOST_WAIT: Duration = Duration::from_secs(300);

/// Count the archive under `root` beside the loop's passes: on its own
/// thread and its own connection, tried again (with a doubling wait) until
/// the database answers, once per start of the loop.
pub(crate) fn start(root: Option<&Path>, prices: &Prices) {
    let Some(root) = root else {
        return;
    };
    let archive = root.join(spawn_host::TRANSCRIPT_DIRECTORY);
    let prices = prices.clone();
    let spawned = std::thread::Builder::new()
        .name("cost-archive".to_string())
        .spawn(move || {
            let mut wait = Duration::from_secs(5);
            loop {
                match crate::config::open_store() {
                    Ok(store) => {
                        for line in count(&store, &archive, &prices) {
                            println!("{line}");
                        }
                        return;
                    }
                    Err(error) => {
                        eprintln!(
                            "spawn loop: cost archive: {error}; trying again in {} s",
                            wait.as_secs()
                        );
                        std::thread::sleep(wait);
                        wait = (wait * 2).min(MOST_WAIT);
                    }
                }
            }
        });
    if let Err(error) = spawned {
        eprintln!("spawn loop: cost archive: cannot start its thread: {error}");
    }
}

/// Count every agent of `archive` the store lacks messages of: the lines
/// for the loop's log (what it counted, the messages of agents with no
/// execution it skipped, and each agent it could not count).
pub(crate) fn count(store: &PostgresStore, archive: &Path, prices: &Prices) -> Vec<String> {
    let mut lines = Vec::new();
    let mut total = ArchiveCounted::default();
    let mut unplaced_agents = 0_usize;
    for (agent, directory) in agents(archive) {
        match count_agent(store, &agent, &directory, prices) {
            Ok(counted) => {
                total.counted += counted.counted;
                total.unplaced += counted.unplaced;
                if counted.unplaced > 0 {
                    unplaced_agents += 1;
                }
            }
            Err(error) => lines.push(format!("cost: archive of {agent}: {error}")),
        }
    }
    if total.counted > 0 {
        lines.push(format!(
            "cost: counted {} archived model call{}",
            total.counted,
            plural(total.counted)
        ));
    }
    if total.unplaced > 0 {
        lines.push(format!(
            "cost: skipped {} archived model call{} of {unplaced_agents} agent{} with no recorded execution",
            total.unplaced,
            plural(total.unplaced),
            if unplaced_agents == 1 { "" } else { "s" }
        ));
    }
    lines
}

/// Whether `name` is plain (letters, digits, `-`, `_`), as the archive
/// holds its agents and native ids to.
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

/// The agents' directories of the archive (not a link), by name.
fn agents(archive: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(archive) else {
        return Vec::new();
    };
    let mut found: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            plain(&name).then(|| (name, entry.path()))
        })
        .collect();
    found.sort();
    found
}

/// The copy files of an agent's directory (regular files, not links), as
/// they are now, by name.
fn files(directory: &Path) -> Result<Vec<ArchiveFile>, String> {
    let entries = std::fs::read_dir(directory)
        .map_err(|error| format!("list {}: {error}", directory.display()))?;
    let mut found: Vec<ArchiveFile> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !copy_file(&name) {
                return None;
            }
            let metadata = std::fs::symlink_metadata(entry.path()).ok()?;
            if !metadata.file_type().is_file() {
                return None;
            }
            let modified_ms = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX));
            Some(ArchiveFile {
                file: name,
                length: i64::try_from(metadata.len()).unwrap_or(i64::MAX),
                modified_ms,
            })
        })
        .collect();
    found.sort_by(|left, right| left.file.cmp(&right.file));
    Ok(found)
}

/// Count one agent: nothing when its files are as last marked; else read
/// them all, send what they hold in batches, and mark the files read once
/// every message found its execution.
fn count_agent(
    store: &PostgresStore,
    agent: &str,
    directory: &Path,
    prices: &Prices,
) -> Result<ArchiveCounted, String> {
    let now = files(directory)?;
    let marked = store
        .cost_archive_files(agent)
        .map_err(|error| format!("its marks: {error}"))?;
    if now == marked {
        return Ok(ArchiveCounted::default());
    }
    let mut messages = BTreeMap::new();
    let mut read = Vec::with_capacity(now.len());
    for file in now {
        // A file that cannot be read is skipped, and not marked.
        if fold_file(&directory.join(&file.file), &mut messages).is_ok() {
            read.push(file);
        }
    }
    let messages: Vec<ArchivedMessage> = messages
        .into_iter()
        .map(|(id, folded)| folded.message(agent, id))
        .collect();
    let mut counted = ArchiveCounted::default();
    for batch in messages.chunks(BATCH) {
        let done = store
            .cost_archive_record(prices.as_value(), batch)
            .map_err(|error| format!("recording its messages: {error}"))?;
        counted.counted += done.counted;
        counted.unplaced += done.unplaced;
    }
    if counted.unplaced == 0 {
        store
            .cost_archive_mark(agent, &read)
            .map_err(|error| format!("marking its files: {error}"))?;
    }
    Ok(counted)
}

/// One message as its lines so far say it: its earliest line's time and
/// model, its largest usage (input, output, cache write, cache read).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Folded {
    at_ms: i64,
    model: String,
    usage: [u64; 4],
}

impl Folded {
    fn total(usage: &[u64; 4]) -> u64 {
        usage.iter().fold(0_u64, |sum, count| sum.saturating_add(*count))
    }

    /// Fold one more line of the same message in.
    fn fold(&mut self, other: Folded) {
        if other.at_ms < self.at_ms {
            self.at_ms = other.at_ms;
            self.model = other.model;
        }
        if Self::total(&other.usage) > Self::total(&self.usage) {
            self.usage = other.usage;
        }
    }

    fn message(self, agent: &str, id: String) -> ArchivedMessage {
        let count = |value: u64| i64::try_from(value).unwrap_or(i64::MAX);
        let [input, output, cache_write, cache_read] = self.usage;
        ArchivedMessage {
            agent_id: agent.to_string(),
            message_id: id,
            model: self.model,
            at_ms: self.at_ms,
            input: count(input),
            output: count(output),
            cache_write: count(cache_write),
            cache_read: count(cache_read),
        }
    }
}

/// The message one line holds, read as the Bridge reads it.
fn line_message(line: &[u8]) -> Option<(String, Folded)> {
    let text = std::str::from_utf8(line).ok()?;
    match spawn_core::ingest::claude_usage(text.trim_end())?.kind {
        EventKind::ModelUsage {
            message_id,
            model,
            at_ms,
            input_tokens,
            output_tokens,
            cache_write_tokens,
            cache_read_tokens,
        } => Some((
            message_id,
            Folded {
                at_ms,
                model,
                usage: [
                    input_tokens,
                    output_tokens,
                    cache_write_tokens,
                    cache_read_tokens,
                ],
            },
        )),
        _ => None,
    }
}

/// Fold one file's lines into `messages`, keyed by message id. An error
/// reading it leaves what it read folded.
fn fold_file(path: &Path, messages: &mut BTreeMap<String, Folded>) -> Result<(), String> {
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
        if let Some((id, folded)) = line_message(&bytes) {
            match messages.get_mut(&id) {
                Some(held) => held.fold(folded),
                None => {
                    messages.insert(id, folded);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use serde_json::json;
    use spawn_postgres::PostgresStore;

    use super::{Folded, copy_file, count, fold_file};
    use crate::cost::Prices;

    fn line(id: &str, model: &str, at: &str, usage: [u64; 4]) -> String {
        let [input, output, cache_write, cache_read] = usage;
        format!(
            "{}\n",
            json!({
                "type": "assistant",
                "timestamp": at,
                "message": {"id": id, "model": model, "role": "assistant",
                            "usage": {"input_tokens": input, "output_tokens": output,
                                      "cache_creation_input_tokens": cache_write,
                                      "cache_read_input_tokens": cache_read}}
            })
        )
    }

    fn scratch(case: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let root = std::env::temp_dir().join(format!(
            "spawn-cost-archive-{case}-{}-{nanos:x}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("scratch directory");
        root
    }

    #[test]
    fn copy_files_are_the_native_and_its_numbered_copies() {
        assert!(copy_file("abc-123.jsonl"));
        assert!(copy_file("abc-123.2.jsonl"));
        for name in ["abc.x.jsonl", "abc..jsonl", ".jsonl", "abc.json", "a b.jsonl"] {
            assert!(!copy_file(name), "{name}");
        }
    }

    #[test]
    fn a_message_folds_to_its_earliest_line_and_largest_usage_across_files() {
        let root = scratch("fold");
        let first = root.join("n.jsonl");
        let copy = root.join("n.1.jsonl");
        std::fs::write(
            &first,
            [
                line("m1", "late", "2026-11-10T10:00:05.000Z", [10, 1, 0, 0]),
                "not json\n".to_string(),
                line("m1", "late", "2026-11-10T10:00:06.000Z", [10, 5, 0, 0]),
                line("m2", "x", "2026-11-10T10:01:00.000Z", [0, 0, 0, 0]),
                json!({"type": "user", "message": {"id": "m3", "usage": {"input_tokens": 3}}})
                    .to_string(),
            ]
            .concat(),
        )
        .expect("write");
        std::fs::write(
            &copy,
            line("m1", "early", "2026-11-10T10:00:01.500Z", [9, 1, 0, 0]),
        )
        .expect("write");
        let mut messages = BTreeMap::new();
        fold_file(&first, &mut messages).expect("read");
        fold_file(&copy, &mut messages).expect("read");
        assert!(fold_file(&root.join("gone.jsonl"), &mut messages).is_err());
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(
            messages.into_iter().collect::<Vec<_>>(),
            vec![(
                "m1".to_string(),
                Folded {
                    // 2026-11-10T10:00:01.500Z
                    at_ms: 1_794_304_801_500,
                    model: "early".to_string(),
                    usage: [10, 5, 0, 0],
                }
            )],
            "zero usage, other lines and a line that is not JSON count nothing"
        );
    }

    /// Owner SQL in the case's schema.
    fn owner(url: &str, schema: &str) -> postgres::Client {
        let separator = if url.contains('?') { '&' } else { '?' };
        let owner = format!("{url}{separator}options=-csearch_path%3D{schema}");
        postgres::Client::connect(&owner, postgres::NoTls).expect("the owner connects")
    }

    fn started(client: &mut postgres::Client, agent: &str, execution: &str, at_ms: i64) {
        client
            .execute(
                "insert into spawn_events (agent_id, session_id, execution_id, source,
                                           source_event_id, kind, session_seq, recorded_at_ms)
                 values ($1, replace($1, 'agent_', 'session_'), $2, 'bridge', 'start-' || $2,
                         jsonb_build_object('name', 'execution.started', 'detail', '{}'::jsonb),
                         (select coalesce(max(session_seq), 0) + 1 from spawn_events
                           where session_id = replace($1, 'agent_', 'session_')), $3)",
                &[&agent, &execution, &at_ms],
            )
            .expect("execution.started");
    }

    fn rows(client: &mut postgres::Client) -> Vec<(String, String, i64, i64, String)> {
        client
            .query(
                "select message_id, execution_id, at_ms, input_tokens + output_tokens, model
                   from spawn_cost_messages order by message_id",
                &[],
            )
            .expect("rows")
            .iter()
            .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4)))
            .collect()
    }

    fn write(path: &Path, text: &str) {
        std::fs::write(path, text).expect("write");
    }

    /// The archive counted once: a second count adds nothing, a copy file
    /// adds nothing, an agent with no execution is skipped and said, and
    /// files as they were marked are not read again.
    #[test]
    fn the_archive_counts_once_per_message_for_the_agents_execution_at_its_time() {
        let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
            "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
        );
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let schema = format!("spawn_test_{}_{nanos:x}_cost_archive", std::process::id());
        let store = PostgresStore::connect_in_schema(&url, &schema).expect("connect and migrate");
        let root = scratch("count");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut client = owner(&url, &schema);
            let prices = Prices::parse(r#"{"x":{"input":1,"output":1,"cache_write":1,"cache_read":1}}"#)
                .expect("prices");
            // 2026-11-10T10:00:00Z, and the executions at 10:00 and 11:00.
            let ten = 1_794_304_800_000_i64;
            started(&mut client, "agent_w_1", "exec_a", ten);
            started(&mut client, "agent_w_1", "exec_b", ten + 3_600_000);
            let worker = root.join("agent_w_1");
            let stranger = root.join("agent_none_2");
            std::fs::create_dir_all(&worker).expect("dir");
            std::fs::create_dir_all(&stranger).expect("dir");
            write(
                &worker.join("n1.jsonl"),
                &[
                    line("m-early", "x", "2026-11-10T09:00:00.000Z", [1, 0, 0, 0]),
                    line("m-a", "x", "2026-11-10T10:30:00.000Z", [2, 0, 0, 0]),
                    line("m-a", "x", "2026-11-10T10:30:00.000Z", [2, 0, 0, 0]),
                    line("m-b", "x", "2026-11-10T11:30:00.000Z", [3, 1, 0, 0]),
                ]
                .concat(),
            );
            write(
                &stranger.join("n2.jsonl"),
                &line("m-x", "x", "2026-11-10T10:30:00.000Z", [4, 0, 0, 0]),
            );
            // Not an archive file.
            write(&worker.join("notes.txt"), "{}");

            let lines = count(&store, &root, &prices);
            assert_eq!(
                lines,
                vec![
                    "cost: counted 3 archived model calls".to_string(),
                    "cost: skipped 1 archived model call of 1 agent with no recorded execution"
                        .to_string()
                ]
            );
            let first = rows(&mut client);
            assert_eq!(
                first,
                vec![
                    ("m-a".into(), "exec_a".into(), ten + 1_800_000, 2, "x".into()),
                    ("m-b".into(), "exec_b".into(), ten + 5_400_000, 4, "x".into()),
                    ("m-early".into(), "exec_a".into(), ten - 3_600_000, 1, "x".into()),
                ],
                "each at its execution then; one before them all at the first"
            );

            // Again: nothing more; only the agent with no execution is said.
            assert_eq!(
                count(&store, &root, &prices),
                vec![
                    "cost: skipped 1 archived model call of 1 agent with no recorded execution"
                        .to_string()
                ]
            );
            // A copy, with one line of a held message larger and earlier: the
            // record keeps what it holds.
            write(
                &worker.join("n1.1.jsonl"),
                &[
                    line("m-a", "x", "2026-11-10T10:20:00.000Z", [20, 0, 0, 0]),
                    line("m-c", "x", "2026-11-10T11:40:00.000Z", [5, 0, 0, 0]),
                ]
                .concat(),
            );
            assert_eq!(
                count(&store, &root, &prices)[0],
                "cost: counted 1 archived model call"
            );
            let after = rows(&mut client);
            assert_eq!(after.len(), 4);
            assert_eq!(after[0], first[0], "a held message keeps what it holds");

            // Marked as read: a message removed from the record is not
            // counted again until a file changes.
            client
                .execute("delete from spawn_cost_messages where message_id = 'm-c'", &[])
                .expect("delete");
            let _ = count(&store, &root, &prices);
            assert_eq!(rows(&mut client).len(), 3, "unchanged files are not read");
            write(
                &worker.join("n1.1.jsonl"),
                &[
                    line("m-a", "x", "2026-11-10T10:20:00.000Z", [20, 0, 0, 0]),
                    line("m-c", "x", "2026-11-10T11:40:00.000Z", [5, 0, 0, 0]),
                    line("m-d", "y", "2026-11-10T11:50:00.000Z", [6, 0, 0, 0]),
                ]
                .concat(),
            );
            let _ = count(&store, &root, &prices);
            let last = rows(&mut client);
            assert_eq!(last.len(), 5, "a changed file reads the agent again: {last:?}");
            let priced: Vec<(String, bool)> = client
                .query(
                    "select message_id, usd is not null from spawn_cost_messages
                      where message_id in ('m-b', 'm-d') order by message_id",
                    &[],
                )
                .expect("usd")
                .iter()
                .map(|row| (row.get(0), row.get(1)))
                .collect();
            assert_eq!(
                priced,
                vec![("m-b".into(), true), ("m-d".into(), false)],
                "a model with no price counts its tokens unpriced"
            );
        }));
        let _ = std::fs::remove_dir_all(&root);
        let _ = postgres::Client::connect(&url, postgres::NoTls)
            .and_then(|mut client| client.batch_execute(&format!("drop schema {schema} cascade")));
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
}
