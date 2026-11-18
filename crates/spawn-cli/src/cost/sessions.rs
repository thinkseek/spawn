//! The cost of the other Claude Code sessions on this host (cost coverage;
//! the lead's scope OK of 2026-11-17 16:39): the people's and reviewers'
//! sessions, whose model calls no engine's `model.usage` event carries, so
//! `/api/cost` counted 8.4% of the host's output tokens.
//!
//! Beside the loop's passes, on its own thread at nice 19, [`start`] counts
//! every assistant message of Claude Code's own transcripts
//! (`$HOME/.claude/projects/<project>/<session>.jsonl`) into
//! `spawn_cost_session_messages`, KEPT APART from the engines' record, so no
//! engine figure (a window's total, a mission, dollars per landing) moves.
//! Once at start and then every [`EVERY`]:
//!
//! - Read only, and only what was appended: each file is opened read-only
//!   (`O_NOFOLLOW`, a regular file) and read from the offset the store marked
//!   for it (the end of its last whole line), never whole again; a file whose
//!   inode changed or that is now shorter than its mark is read from 0, once,
//!   and named in the loop's log. Nothing under `~/.claude` is ever written,
//!   renamed or removed.
//! - Bounded: a pass reads at most [`MOST_BYTES`] and starts no file after
//!   [`MOST_TIME`]; the rest is read by the next pass, from its marks.
//! - The engines' own project directories (the workspace root's, whose
//!   messages the engines' record holds) are skipped, and a message that
//!   record holds anyway is left to it (`spawn_cost_session_record`).
//! - A message counts once, by 041's rules ([`super::archive`]'s fold: its
//!   largest usage, at its earliest line's time and model), priced when it
//!   is recorded; only the project directory's name, the session's file
//!   stem, the model, the time and the four counts are kept.
//! - A file's messages and its new mark go to the store in one call, so a
//!   mark never runs ahead of what it counted.

use std::collections::BTreeMap;
use std::io::{BufRead as _, Read as _, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use spawn_postgres::{PostgresStore, SessionFile, SessionMessage};

use super::archive::{Folded, line_message};
use super::{Prices, plural};

/// How often the sessions are counted again.
pub(super) const EVERY: Duration = Duration::from_hours(1);

/// The most bytes one pass reads (line by line: memory holds one line).
pub(super) const MOST_BYTES: u64 = 1024 * 1024 * 1024;

/// A pass starts no new file after this long.
pub(super) const MOST_TIME: Duration = Duration::from_mins(5);

/// The most messages one call to the store carries.
const BATCH: usize = 500;

/// The longest the counter waits between tries to reach the database.
const MOST_WAIT: Duration = Duration::from_mins(5);

/// Count the sessions' transcripts under `$HOME/.claude/projects` beside the
/// loop's passes, on a thread of its own at nice 19 with its own
/// connection: once at start, then every [`EVERY`]. The engines' project
/// directories (those of `root`, the workspace root) are skipped.
pub(crate) fn start(root: Option<&Path>, prices: &Prices) {
    let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) else {
        return;
    };
    let projects = PathBuf::from(home).join(".claude").join("projects");
    let skip = root.map(engine_prefix);
    let prices = prices.clone();
    let spawned = std::thread::Builder::new()
        .name("cost-sessions".to_string())
        .spawn(move || {
            lower_priority();
            let mut wait = Duration::from_secs(5);
            loop {
                match crate::config::open_store() {
                    Ok(store) => {
                        wait = Duration::from_secs(5);
                        for line in
                            count(&store, &projects, skip.as_deref(), &prices, Budget::pass())
                        {
                            println!("{line}");
                        }
                        std::thread::sleep(EVERY);
                    }
                    Err(error) => {
                        eprintln!(
                            "spawn loop: cost sessions: {error}; trying again in {} s",
                            wait.as_secs()
                        );
                        std::thread::sleep(wait);
                        wait = (wait * 2).min(MOST_WAIT);
                    }
                }
            }
        });
    if let Err(error) = spawned {
        eprintln!("spawn loop: cost sessions: cannot start its thread: {error}");
    }
}

/// This thread at nice 19 (Linux sets it per thread); nothing else changes.
fn lower_priority() {
    let tid = std::fs::read_link("/proc/thread-self")
        .ok()
        .and_then(|link| link.file_name()?.to_str()?.parse::<i32>().ok())
        .and_then(rustix::process::Pid::from_raw);
    if let Err(error) = rustix::process::setpriority_process(tid, 19) {
        eprintln!("spawn loop: cost sessions: nice 19: {error}");
    }
}

/// The project-directory name Claude Code gives a directory: its path with
/// every character but a letter or digit made `-`. The workspace root's,
/// with a `-` after it, begins every engine's.
pub(super) fn engine_prefix(root: &Path) -> String {
    let mut name: String = root
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    name.push('-');
    name
}

/// What one pass may still read: bytes and time.
#[derive(Debug, Clone, Copy)]
pub(super) struct Budget {
    pub(super) bytes: u64,
    pub(super) until: Instant,
}

impl Budget {
    pub(super) fn pass() -> Self {
        Self {
            bytes: MOST_BYTES,
            until: Instant::now() + MOST_TIME,
        }
    }
}

/// What a pass did, for the loop's log.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Pass {
    pub(super) counted: i64,
    pub(super) changed: i64,
    /// Messages the store did not count (`invalid`: no id, model, time,
    /// project or session); their file's mark moved past them.
    pub(super) invalid: i64,
    pub(super) files: usize,
    pub(super) bytes: u64,
    /// Files read from 0 again: their inode changed, or they shrank.
    pub(super) restarted: Vec<String>,
    pub(super) errors: Vec<String>,
    /// The pass stopped at its byte or time bound.
    pub(super) bounded: bool,
    /// The longest one call to the store took: it holds the cost record's
    /// lock that long, and the loop's cost step waits for it.
    pub(super) longest_call: Duration,
}

/// One pass over `projects`: the lines for the loop's log.
pub(super) fn count(
    store: &PostgresStore,
    projects: &Path,
    skip: Option<&str>,
    prices: &Prices,
    budget: Budget,
) -> Vec<String> {
    let started = Instant::now();
    let pass = pass(store, projects, skip, prices, budget);
    let mut lines = Vec::new();
    if pass.counted > 0 || pass.changed > 0 || pass.bounded {
        lines.push(format!(
            "cost: sessions: counted {} model call{} ({} grew) from {} file{}, {} bytes read in {} s{}",
            pass.counted,
            plural(pass.counted),
            pass.changed,
            pass.files,
            if pass.files == 1 { "" } else { "s" },
            pass.bytes,
            started.elapsed().as_secs(),
            if pass.bounded { "; the rest next pass" } else { "" }
        ));
        lines.push(format!(
            "cost: sessions: the longest call to the store took {} ms",
            pass.longest_call.as_millis()
        ));
    }
    if pass.invalid > 0 {
        lines.push(format!(
            "cost: sessions: {} message{} not counted (no id, model, time, project or session)",
            pass.invalid,
            if pass.invalid == 1 { "" } else { "s" }
        ));
    }
    if !pass.restarted.is_empty() {
        lines.push(format!(
            "cost: sessions: read from 0 again (inode changed or shorter): {}",
            pass.restarted.join(", ")
        ));
    }
    lines.extend(
        pass.errors
            .iter()
            .map(|error| format!("cost: sessions: {error}")),
    );
    lines
}

/// One pass: every file with bytes past its mark, within `budget`.
pub(super) fn pass(
    store: &PostgresStore,
    projects: &Path,
    skip: Option<&str>,
    prices: &Prices,
    mut budget: Budget,
) -> Pass {
    let mut done = Pass::default();
    let marks: BTreeMap<String, SessionFile> = match store.cost_session_files() {
        Ok(marks) => marks
            .into_iter()
            .map(|mark| (mark.file.clone(), mark))
            .collect(),
        Err(error) => {
            done.errors.push(format!("the marks: {error}"));
            return done;
        }
    };
    for (project, path, name) in transcripts(projects, skip) {
        if budget.bytes == 0 || Instant::now() >= budget.until {
            done.bounded = true;
            break;
        }
        let key = format!("{project}/{name}");
        match read_file(&path, marks.get(&key), budget.bytes) {
            Ok(None) => {}
            Ok(Some(read)) => {
                budget.bytes = budget.bytes.saturating_sub(read.bytes);
                done.bytes += read.bytes;
                if read.restarted {
                    done.restarted.push(key.clone());
                }
                if read.bounded {
                    done.bounded = true;
                }
                let session = name.strip_suffix(".jsonl").unwrap_or(&name).to_string();
                let mark = SessionFile {
                    file: key.clone(),
                    inode: read.inode,
                    offset_bytes: read.offset,
                    length: read.length,
                    modified_ms: read.modified_ms,
                };
                match record(
                    store,
                    prices,
                    &project,
                    &session,
                    read.messages,
                    &mark,
                    &mut done,
                ) {
                    Ok(()) => done.files += 1,
                    Err(error) => done.errors.push(format!("{key}: {error}")),
                }
            }
            Err(error) => done.errors.push(format!("{key}: {error}")),
        }
    }
    done
}

/// Send one file's messages in batches, its mark with the last; what each
/// call did is added to `done`. Each batch is one call, one transaction:
/// the cost record's lock is held for one batch at most.
fn record(
    store: &PostgresStore,
    prices: &Prices,
    project: &str,
    session: &str,
    messages: BTreeMap<String, Folded>,
    mark: &SessionFile,
    done: &mut Pass,
) -> Result<(), String> {
    let count = |value: u64| i64::try_from(value).unwrap_or(i64::MAX);
    let messages: Vec<SessionMessage> = messages
        .into_iter()
        .map(|(id, folded)| {
            let [input, output, cache_write, cache_read] = folded.usage;
            SessionMessage {
                project: project.to_string(),
                session_id: session.to_string(),
                message_id: id,
                model: folded.model,
                at_ms: folded.at_ms,
                input: count(input),
                output: count(output),
                cache_write: count(cache_write),
                cache_read: count(cache_read),
            }
        })
        .collect();
    let mut batches: Vec<&[SessionMessage]> = messages.chunks(BATCH).collect();
    if batches.is_empty() {
        batches.push(&[]);
    }
    let last = batches.len() - 1;
    for (index, batch) in batches.into_iter().enumerate() {
        let files = if index == last {
            std::slice::from_ref(mark)
        } else {
            &[]
        };
        let started = Instant::now();
        let call = store
            .cost_session_record(prices.as_value(), batch, files)
            .map_err(|error| format!("recording: {error}"))?;
        done.longest_call = done.longest_call.max(started.elapsed());
        done.counted += call.counted;
        done.changed += call.changed;
        done.invalid += call.invalid;
    }
    Ok(())
}

/// The transcripts under `projects`: `(project, path, file name)` of each
/// regular `*.jsonl` file (not a link) directly in a project directory (not
/// a link), by project and name; the projects whose name begins with
/// `skip` are left out.
pub(super) fn transcripts(projects: &Path, skip: Option<&str>) -> Vec<(String, PathBuf, String)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(projects) else {
        return found;
    };
    for entry in entries.flatten() {
        let Ok(project) = entry.file_name().into_string() else {
            continue;
        };
        if skip.is_some_and(|prefix| project.starts_with(prefix))
            || !entry.file_type().is_ok_and(|kind| kind.is_dir())
        {
            continue;
        }
        let Ok(files) = std::fs::read_dir(entry.path()) else {
            continue;
        };
        for file in files.flatten() {
            let Ok(name) = file.file_name().into_string() else {
                continue;
            };
            // `<session>.jsonl`: a file named `.jsonl` alone has no
            // extension (`Path::extension`), so no empty session is counted.
            if Path::new(&name)
                .extension()
                .is_some_and(|extension| extension == "jsonl")
                && file.file_type().is_ok_and(|kind| kind.is_file())
            {
                found.push((project.clone(), file.path(), name));
            }
        }
    }
    found.sort();
    found
}

/// What reading one file past its mark gave.
#[derive(Debug, Default)]
pub(super) struct Appended {
    pub(super) messages: BTreeMap<String, Folded>,
    /// The end of the last whole line read: the file's new mark.
    pub(super) offset: i64,
    pub(super) bytes: u64,
    pub(super) inode: i64,
    pub(super) length: i64,
    pub(super) modified_ms: i64,
    pub(super) restarted: bool,
    pub(super) bounded: bool,
}

/// Read `path` (read-only, no link followed, a regular file) from its mark
/// to its end, at most `most` bytes, whole lines only: None when nothing
/// whole was appended since the mark.
pub(super) fn read_file(
    path: &Path,
    mark: Option<&SessionFile>,
    most: u64,
) -> Result<Option<Appended>, String> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(
            (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK)
                .bits()
                .cast_signed(),
        )
        .open(path)
        .map_err(|error| format!("open: {error}"))?;
    let metadata = file.metadata().map_err(|error| format!("stat: {error}"))?;
    if !metadata.file_type().is_file() {
        return Err("not a regular file".to_string());
    }
    let inode = i64::try_from(metadata.ino()).unwrap_or(i64::MAX);
    let length = i64::try_from(metadata.len()).unwrap_or(i64::MAX);
    let modified_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        });
    let (from, restarted) = match mark {
        Some(mark) if mark.inode == inode && mark.offset_bytes <= length => {
            (mark.offset_bytes, false)
        }
        Some(_) => (0, true),
        None => (0, false),
    };
    if from == length && !restarted {
        return Ok(None);
    }
    let start = u64::try_from(from).unwrap_or(0);
    let available = u64::try_from(length).unwrap_or(0).saturating_sub(start);
    let take = available.min(most);
    file.seek(SeekFrom::Start(start))
        .map_err(|error| format!("seek: {error}"))?;
    // Line by line, so memory holds one line at a time, never the read.
    let mut reader = std::io::BufReader::new(file.take(take));
    let mut line = Vec::new();
    let (mut whole, mut bytes) = (0_u64, 0_u64);
    let mut messages = BTreeMap::new();
    loop {
        line.clear();
        let got = reader
            .read_until(b'\n', &mut line)
            .map_err(|error| format!("read: {error}"))?;
        if got == 0 {
            break;
        }
        bytes += u64::try_from(got).unwrap_or(0);
        // Whole lines only: a line still being written (or cut by the
        // bound) is read next time, from the mark.
        if line.last() != Some(&b'\n') {
            break;
        }
        whole += u64::try_from(got).unwrap_or(0);
        if let Some((id, folded)) = line_message(&line) {
            match messages.get_mut(&id) {
                Some(held) => Folded::fold(held, folded),
                None => {
                    messages.insert(id, folded);
                }
            }
        }
    }
    let read = whole;
    Ok(Some(Appended {
        messages,
        offset: from + i64::try_from(whole).unwrap_or(0),
        bytes,
        inode,
        length,
        modified_ms,
        restarted,
        // The bound cut the read short of the file's end.
        bounded: take < available && read < available,
    }))
    .map(|read| read.filter(|read| read.offset > from || read.restarted))
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use serde_json::json;

    use super::{Budget, Folded, engine_prefix, pass, read_file, record, transcripts};
    use crate::cost::Prices;

    fn line(id: &str, at: &str, usage: [u64; 4]) -> String {
        let [input, output, cache_write, cache_read] = usage;
        format!(
            "{}\n",
            json!({
                "type": "assistant", "timestamp": at,
                "message": {"id": id, "model": "model-x", "role": "assistant",
                            "content": [{"type": "text", "text": "never stored"}],
                            "usage": {"input_tokens": input, "output_tokens": output,
                                      "cache_creation_input_tokens": cache_write,
                                      "cache_read_input_tokens": cache_read}}
            })
        )
    }

    /// A fake `$HOME/.claude/projects` in scratch, never under the real home
    /// (tests of anything reading the native transcripts use a fake HOME).
    fn fake_projects(case: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let home = std::env::temp_dir().join(format!(
            "spawn-cost-sessions-{case}-{}-{nanos:x}",
            std::process::id()
        ));
        if let Some(real) = std::env::var_os("HOME").filter(|home| !home.is_empty()) {
            let real = PathBuf::from(real).join(".claude");
            assert!(
                !home.starts_with(&real),
                "a fake HOME, never the real one: {}",
                home.display()
            );
        }
        let projects = home.join(".claude").join("projects");
        std::fs::create_dir_all(&projects).expect("fake projects");
        projects
    }

    fn append(path: &Path, text: &str) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open");
        file.write_all(text.as_bytes()).expect("append");
    }

    fn budget() -> Budget {
        Budget {
            bytes: 1 << 20,
            until: Instant::now() + Duration::from_mins(1),
        }
    }

    #[test]
    fn the_engines_project_directories_are_named_from_the_workspace_root() {
        assert_eq!(
            engine_prefix(Path::new("/home/ckl/.local/share/spawn-rsi/ws")),
            "-home-ckl--local-share-spawn-rsi-ws-"
        );
    }

    #[test]
    fn only_regular_jsonl_files_of_project_directories_are_transcripts() {
        let projects = fake_projects("list");
        let rev = projects.join("-data-tmp-ccd-rev-x");
        let engine = projects.join("-ws-agent-resident-rsi");
        std::fs::create_dir_all(rev.join("sub")).expect("dirs");
        std::fs::create_dir_all(&engine).expect("dirs");
        append(&rev.join("a.jsonl"), "");
        append(&rev.join("notes.txt"), "");
        // No session in its name: not a transcript.
        append(&rev.join(".jsonl"), "");
        append(&rev.join("sub").join("deep.jsonl"), "");
        append(&engine.join("e.jsonl"), "");
        std::os::unix::fs::symlink(rev.join("a.jsonl"), rev.join("link.jsonl")).expect("link");
        std::os::unix::fs::symlink(&rev, projects.join("linked-project")).expect("link");
        let found: Vec<String> = transcripts(&projects, Some("-ws-"))
            .into_iter()
            .map(|(project, _, name)| format!("{project}/{name}"))
            .collect();
        let _ = std::fs::remove_dir_all(projects.parent().and_then(Path::parent).expect("home"));
        assert_eq!(found, vec!["-data-tmp-ccd-rev-x/a.jsonl".to_string()]);
    }

    #[test]
    fn the_projects_of_the_workspace_root_are_skipped_and_no_other() {
        let projects = fake_projects("skip");
        let root = Path::new("/home/u/.local/share/spawn-rsi/ws");
        // Claude Code's names for: an engine under the root, the root
        // itself, a sibling sharing the root's name as a prefix, a reviewer.
        for project in [
            "-home-u--local-share-spawn-rsi-ws-agent-worker-1",
            "-home-u--local-share-spawn-rsi-ws",
            "-home-u--local-share-spawn-rsi-wsx",
            "-data-tmp-ccd-rev-x",
        ] {
            std::fs::create_dir_all(projects.join(project)).expect("dirs");
            append(&projects.join(project).join("s.jsonl"), "");
        }
        let found: Vec<String> = transcripts(&projects, Some(&engine_prefix(root)))
            .into_iter()
            .map(|(project, _, _)| project)
            .collect();
        let _ = std::fs::remove_dir_all(projects.parent().and_then(Path::parent).expect("home"));
        assert_eq!(
            found,
            vec![
                "-data-tmp-ccd-rev-x".to_string(),
                "-home-u--local-share-spawn-rsi-ws".to_string(),
                "-home-u--local-share-spawn-rsi-wsx".to_string(),
            ]
        );
    }

    #[test]
    fn a_file_is_read_from_its_mark_whole_lines_only_and_never_changed() {
        let projects = fake_projects("read");
        let path = projects.join("s.jsonl");
        let first = line("m1", "2026-11-17T08:00:00.000Z", [10, 1, 0, 0]);
        append(&path, &first);
        // A line still being written: not read yet.
        let partial = "{\"type\": \"assistant\", \"mess";
        append(&path, partial);
        let before = std::fs::read(&path).expect("bytes");
        let read = read_file(&path, None, 1 << 20)
            .expect("read")
            .expect("new bytes");
        assert_eq!(read.offset, i64::try_from(first.len()).expect("small"));
        assert_eq!(read.messages.len(), 1);
        assert!(!read.restarted && !read.bounded);
        assert_eq!(std::fs::read(&path).expect("bytes"), before, "read only");
        let mark = spawn_postgres::SessionFile {
            file: "p/s.jsonl".to_string(),
            inode: read.inode,
            offset_bytes: read.offset,
            length: read.length,
            modified_ms: read.modified_ms,
        };
        // Nothing whole appended since: nothing to read.
        assert!(
            read_file(&path, Some(&mark), 1 << 20)
                .expect("read")
                .is_none()
        );
        // The line ends, one more follows: only they are read.
        let rest = format!(
            "ssage\": {{}}}}\n{}",
            line("m2", "2026-11-17T08:01:00.000Z", [5, 5, 0, 0])
        );
        append(&path, &rest);
        let next = read_file(&path, Some(&mark), 1 << 20)
            .expect("read")
            .expect("new bytes");
        assert_eq!(
            next.bytes,
            u64::try_from(partial.len() + rest.len()).expect("small")
        );
        assert_eq!(next.messages.keys().collect::<Vec<_>>(), vec!["m2"]);
        // A mark past the end (the file shrank) or of another inode: from 0.
        let shrunk = spawn_postgres::SessionFile {
            offset_bytes: 1 << 30,
            ..mark.clone()
        };
        assert!(
            read_file(&path, Some(&shrunk), 1 << 20)
                .expect("read")
                .expect("again")
                .restarted
        );
        let other = spawn_postgres::SessionFile {
            inode: mark.inode + 1,
            ..mark.clone()
        };
        let again = read_file(&path, Some(&other), 1 << 20)
            .expect("read")
            .expect("again");
        assert!(again.restarted);
        assert_eq!(again.messages.len(), 2);
        // A bound short of the end reads whole lines up to it only.
        let cut = read_file(&path, None, u64::try_from(first.len() + 3).expect("small"))
            .expect("read")
            .expect("some");
        assert!(cut.bounded);
        assert_eq!(cut.offset, i64::try_from(first.len()).expect("small"));
        // A link is not followed.
        std::os::unix::fs::symlink(&path, projects.join("l.jsonl")).expect("link");
        assert!(read_file(&projects.join("l.jsonl"), None, 1 << 20).is_err());
        let _ = std::fs::remove_dir_all(projects.parent().and_then(Path::parent).expect("home"));
    }

    #[test]
    fn a_pass_counts_appended_messages_once_and_skips_the_engines() {
        let schema = spawn_postgres::testing::TestSchema::new("cost_sessions_pass");
        let store = schema.store();
        let projects = fake_projects("pass");
        let rev = projects.join("-data-tmp-ccd-rev-x");
        let engine = projects.join("-ws-agent-worker-1");
        std::fs::create_dir_all(&rev).expect("dirs");
        std::fs::create_dir_all(&engine).expect("dirs");
        let path = rev.join("s1.jsonl");
        append(
            &path,
            &line("m1", "2026-11-17T08:00:00.000Z", [1_000_000, 0, 0, 0]),
        );
        append(
            &path,
            &line("m1", "2026-11-17T08:00:01.000Z", [1_000_000, 0, 0, 0]),
        );
        append(
            &engine.join("e.jsonl"),
            &line("e1", "2026-11-17T08:00:00.000Z", [9, 9, 9, 9]),
        );
        let prices = Prices::parse(
            r#"{"model-x": {"input": 3, "output": 15, "cache_write": 3.75, "cache_read": 0.3}}"#,
        )
        .expect("prices");
        let first = pass(&store, &projects, Some("-ws-"), &prices, budget());
        assert_eq!(
            (first.counted, first.changed, first.files),
            (1, 0, 1),
            "{first:?}"
        );
        // Nothing appended: nothing read.
        let quiet = pass(&store, &projects, Some("-ws-"), &prices, budget());
        assert_eq!(
            (quiet.counted, quiet.files, quiet.bytes),
            (0, 0, 0),
            "{quiet:?}"
        );
        // One message appended: only it is read and counted.
        let more = line("m2", "2026-11-17T09:00:00.000Z", [0, 1000, 0, 0]);
        append(&path, &more);
        let next = pass(&store, &projects, Some("-ws-"), &prices, budget());
        assert_eq!(
            (next.counted, next.bytes),
            (1, u64::try_from(more.len()).expect("small"))
        );
        let at = |text: &str| spawn_core::ingest::utc_ms(text).expect("time");
        let sessions = store
            .cost_sessions(
                at("2026-11-17T00:00:00.000Z"),
                at("2026-11-18T00:00:00.000Z"),
            )
            .expect("sessions");
        let _ = std::fs::remove_dir_all(projects.parent().and_then(Path::parent).expect("home"));
        // m1 $3.00 + m2 $0.015; the engine's file is not read.
        assert_eq!(
            sessions["rows"].as_array().map(Vec::len),
            Some(1),
            "{sessions}"
        );
        assert_eq!(sessions["rows"][0]["project"], "-data-tmp-ccd-rev-x");
        let usd = sessions["total"]["usd"].as_f64().expect("usd");
        assert!((usd - 3.015).abs() < 1e-9, "{sessions}");
        assert!(!sessions.to_string().contains("never stored"));
    }

    #[test]
    fn the_messages_the_store_does_not_count_are_counted_back() {
        let schema = spawn_postgres::testing::TestSchema::new("cost_sessions_invalid");
        let store = schema.store();
        let prices = Prices::parse(
            r#"{"model-x": {"input": 3, "output": 15, "cache_write": 3.75, "cache_read": 0.3}}"#,
        )
        .expect("prices");
        let folded = |at_ms| Folded {
            at_ms,
            model: "model-x".to_string(),
            usage: [10, 1, 0, 0],
        };
        let mark = |file: &str| spawn_postgres::SessionFile {
            file: file.to_string(),
            inode: 1,
            offset_bytes: 10,
            length: 10,
            modified_ms: 1,
        };
        // No session: the store counts none and says so; the mark still moves.
        let messages = [
            ("m1".to_string(), folded(1_000)),
            ("m2".to_string(), folded(2_000)),
        ];
        let mut done = super::Pass::default();
        let none = record(
            &store,
            &prices,
            "p",
            "",
            messages.clone().into(),
            &mark("p/.jsonl"),
            &mut done,
        );
        assert_eq!(none, Ok(()));
        assert_eq!(
            (done.counted, done.changed, done.invalid),
            (0, 0, 2),
            "{done:?}"
        );
        let mut done = super::Pass::default();
        let some = record(
            &store,
            &prices,
            "p",
            "s",
            messages.into(),
            &mark("p/s.jsonl"),
            &mut done,
        );
        assert_eq!(some, Ok(()));
        assert_eq!(
            (done.counted, done.changed, done.invalid),
            (2, 0, 0),
            "{done:?}"
        );
        assert!(done.longest_call > Duration::ZERO, "{done:?}");
        let marks: Vec<String> = store
            .cost_session_files()
            .expect("marks")
            .into_iter()
            .map(|mark| mark.file)
            .collect();
        assert_eq!(marks, vec!["p/.jsonl".to_string(), "p/s.jsonl".to_string()]);
    }
}
