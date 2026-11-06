//! A gate failure that is the host's (objective 20, docs/fleet-architecture.md
//! §10.3): when check-tests.sh fails on the candidate, the failures it
//! printed, read from the gate's recorded output, and the test database's
//! own log over the gate's measured window say whether the host failed
//! rather than the work.
//!
//! Two forms of check-tests.sh output are read (rsi/scenarios/check-tests.sh):
//! - the owner's: one line per failing test, `FAILED <test>: <first message
//!   line>`, before the excerpt. When the section holds one, those lines are
//!   the failures (they are complete) and the excerpt is not read;
//! - the excerpt alone (`grep -E 'FAILED|panicked|^error' -A5 | head -n
//!   40` of `cargo test`): a failure is a `thread '<test>' panicked at
//!   <location>:` line with its message (the lines after it up to a `note:`,
//!   `---- `, `test result:` or `--` line or a blank one), or the `Error:`
//!   line of a test that returned an error. A failure the 40 lines cut off
//!   is not seen: a wrong host fault never lands a candidate (it is judged
//!   again), so that is accepted.
//!
//! Every line of the gate's output is `| `-prefixed by the gate.

use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::Path;

/// The reason scripts/rsi/gate.sh gives when check-tests.sh fails on the
/// candidate: the only rejection whose failures are read.
pub(crate) const CHECK_TESTS_REGRESSION: &str = "regression: check-tests.sh fails on the candidate";

/// The line that opens the check-tests section of the gate's output.
const SECTION: &str = "| == regression: check-tests.sh must still pass";

/// A failure whose message holds one of these is a connection error to the
/// test database: the host's, whatever the test (matched without case).
pub(crate) const CONNECTION_ERRORS: &[&str] = &[
    "error connecting to server",
    "Connection refused",
    "connection closed",
    "server closed the connection unexpectedly",
    "terminating connection because of crash of another server process",
    "the database system is in recovery mode",
    "the database system is starting up",
    "the database system is shutting down",
    "the database system is not yet accepting connections",
    "could not resize shared memory segment",
    "SqlState(E57P0",
    "SqlState(E080",
];

/// A failure whose message holds one of these is a database error of some
/// kind: the host's only when the test database's log shows a crash in the
/// gate's window. The `postgres` crate's Display and Debug of a server error.
pub(crate) const DATABASE_ERRORS: &[&str] = &["db error", "Error { kind: Db"];

/// Lines of the test database's log that show a server crash.
pub(crate) const CRASH_MARKS: &[&str] = &[
    "was terminated by signal",
    "terminating any other active server processes",
    "all server processes terminated; reinitializing",
];

/// How far outside the gate's measured window a crash line still counts.
pub(crate) const WINDOW_SLACK_MS: i64 = 1000;

/// Bytes of the test database's log read, from its end: the gate's window
/// is the latest part of it.
const LOG_TAIL_BYTES: u64 = 16 << 20;

/// Characters of a failure's first message line a record keeps.
const MESSAGE_CHARS: usize = 300;

/// Characters of a crash line a record keeps.
const CRASH_CHARS: usize = 300;

/// Crash lines a record keeps at most.
const CRASH_LINES: usize = 10;

/// One failing test as check-tests.sh printed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Failure {
    pub(crate) test: String,
    /// Its message: the FAILED line's text, or the excerpt's lines.
    pub(crate) message: String,
}

/// Whose a failure is, by its message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Class {
    /// A connection error to the test database: the host's.
    Connection,
    /// A database error that is no connection error: the host's only with a
    /// crash in the test database's log during the gate.
    Database,
    /// Anything else (an assertion, a panic of the code under test, a
    /// timeout): the work's.
    Work,
}

impl Class {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Connection => "connection",
            Self::Database => "database",
            Self::Work => "work",
        }
    }
}

/// The class of a failure with this message.
pub(crate) fn classify(message: &str) -> Class {
    let lower = message.to_ascii_lowercase();
    let holds = |patterns: &[&str]| {
        patterns
            .iter()
            .any(|pattern| lower.contains(&pattern.to_ascii_lowercase()))
    };
    if holds(CONNECTION_ERRORS) {
        Class::Connection
    } else if holds(DATABASE_ERRORS) {
        Class::Database
    } else {
        Class::Work
    }
}

/// The lines of the check-tests section of the gate's output: after its
/// opening line, up to the next `| == ` or `| stage gate ` line. Empty when
/// the output has no such section.
fn section(log: &str) -> Vec<&str> {
    let lines: Vec<&str> = log.lines().collect();
    let Some(start) = lines.iter().rposition(|line| line.trim_end() == SECTION) else {
        return Vec::new();
    };
    lines[start + 1..]
        .iter()
        .copied()
        .take_while(|line| !line.starts_with("| == ") && !line.starts_with("| stage gate "))
        .collect()
}

/// A line's text without the gate's `| ` prefix (`|` alone is a blank
/// line). None for a line the gate did not prefix.
fn body(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('|')?;
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

/// The owner's form: `FAILED <test>: <first message line>`.
fn failed_line(line: &str) -> Option<Failure> {
    let rest = body(line)?.strip_prefix("FAILED ")?;
    let (test, message) = rest
        .split_once(": ")
        .or_else(|| rest.trim_end().strip_suffix(':').map(|test| (test, "")))?;
    let test = test.trim();
    (!test.is_empty() && !test.contains(char::is_whitespace)).then(|| Failure {
        test: test.to_string(),
        message: message.trim_end().to_string(),
    })
}

/// The test of a `thread '<test>' panicked at <location>:` line, or of
/// `thread '<test>' (<thread id>) panicked at <location>:` (Rust 1.88 on).
fn panicked(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("thread '")?;
    let (name, after) = rest.split_once('\'')?;
    let after = after.trim_start();
    let after = match after.strip_prefix('(') {
        Some(id) => id.split_once(')')?.1.trim_start(),
        None => after,
    };
    (!name.is_empty() && after.starts_with("panicked at")).then_some(name)
}

/// The test of a `---- <test> stdout ----` line.
fn stdout_header(line: &str) -> Option<&str> {
    let name = line.strip_prefix("---- ")?.strip_suffix(" stdout ----")?;
    (!name.is_empty()).then_some(name)
}

/// Whether a line ends a panic's message: a `note:`, `---- `, `test
/// result:` or `--` line, a blank one, a line the gate did not prefix, or
/// the next panic.
fn ends_message(line: &str) -> bool {
    body(line).is_none_or(|text| {
        text.trim().is_empty()
            || text.starts_with("note:")
            || text.starts_with("---- ")
            || text.starts_with("test result:")
            || text.starts_with("--")
            || panicked(text).is_some()
    })
}

/// Today's form: the excerpt alone.
fn excerpt_failures(lines: &[&str]) -> Vec<Failure> {
    let mut failures = Vec::new();
    // The test whose stdout the excerpt is in, from its header, if shown.
    let mut current: Option<&str> = None;
    let mut at = 0;
    while at < lines.len() {
        let Some(text) = body(lines[at]) else {
            at += 1;
            continue;
        };
        if let Some(test) = stdout_header(text) {
            current = Some(test);
            at += 1;
        } else if let Some(test) = panicked(text) {
            at += 1;
            let mut message = Vec::new();
            while at < lines.len() && !ends_message(lines[at]) {
                message.push(body(lines[at]).unwrap_or_default().trim_end());
                at += 1;
            }
            // A panic whose message the excerpt cut off is not seen.
            if !message.is_empty() {
                failures.push(Failure {
                    test: test.to_string(),
                    message: message.join("\n"),
                });
            }
            current = None;
        } else if text.starts_with("Error:") {
            failures.push(Failure {
                test: current.take().unwrap_or("an unnamed test").to_string(),
                message: text.trim_end().to_string(),
            });
            at += 1;
        } else {
            // grep's group separator and a binary's summary end what the
            // last header named.
            if text.trim_end() == "--" || text.starts_with("test result:") {
                current = None;
            }
            at += 1;
        }
    }
    failures
}

/// The failures check-tests.sh printed in the gate's output: its FAILED
/// lines when the section holds one, else the failures its excerpt shows.
pub(crate) fn check_tests_failures(log: &str) -> Vec<Failure> {
    let lines = section(log);
    let owner: Vec<Failure> = lines.iter().filter_map(|line| failed_line(line)).collect();
    if owner.is_empty() {
        excerpt_failures(&lines)
    } else {
        owner
    }
}

/// The time of a line of the test database's log, as PostgreSQL writes it
/// with `log_line_prefix` `%m [%p] `: `2026-11-06 01:46:58.017 UTC [1]
/// LOG:  ...`, the zone `UTC` (or `GMT`) or a numeric offset (`+09`,
/// `-0330`, `+05:30`). None for any other line.
pub(crate) fn log_time_ms(line: &str) -> Option<i64> {
    let mut words = line.split_whitespace();
    let (date, time, zone) = (words.next()?, words.next()?, words.next()?);
    let zone = match zone {
        "UTC" | "GMT" | "Z" => "Z".to_string(),
        numeric => {
            let (sign, digits) = numeric.split_at_checked(1)?;
            if sign != "+" && sign != "-" {
                return None;
            }
            let digits = digits.replace(':', "");
            if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            match digits.len() {
                2 => format!("{sign}{digits}:00"),
                4 => format!("{sign}{}:{}", &digits[..2], &digits[2..]),
                _ => return None,
            }
        }
    };
    spawn_core::ingest::utc_ms(&format!("{date}T{time}{zone}"))
}

/// The lines of the test database's log that show a crash stamped within
/// [`started_ms` - 1 s, `ended_ms` + 1 s], oldest first, at most
/// [`CRASH_LINES`], each cut to [`CRASH_CHARS`].
pub(crate) fn crash_lines(log: &str, started_ms: i64, ended_ms: i64) -> Vec<String> {
    let window =
        started_ms.saturating_sub(WINDOW_SLACK_MS)..=ended_ms.saturating_add(WINDOW_SLACK_MS);
    log.lines()
        .filter(|line| CRASH_MARKS.iter().any(|mark| line.contains(mark)))
        .filter(|line| log_time_ms(line).is_some_and(|at| window.contains(&at)))
        .take(CRASH_LINES)
        .map(|line| super::cut(line.trim_end(), CRASH_CHARS).to_string())
        .collect()
}

/// The end of the test database's log ([`LOG_TAIL_BYTES`] at most): its
/// first line may be cut, and then reads as no line of the log.
pub(crate) fn read_log(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let length = file.metadata()?.len();
    if length > LOG_TAIL_BYTES {
        file.seek(SeekFrom::Start(length - LOG_TAIL_BYTES))?;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The evidence that a rejection is the host's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostFault {
    /// Every failure read, each with its class (none is the work's).
    pub(crate) failures: Vec<(Failure, Class)>,
    /// The test database's log lines that showed a crash in the gate's
    /// window, when they decided it (not every failure was a connection
    /// error); empty otherwise.
    pub(crate) crash: Vec<String>,
}

impl HostFault {
    /// `failures` when every failure is a connection error, else
    /// `server_log`.
    pub(crate) fn evidence(&self) -> &'static str {
        if self.crash.is_empty() {
            "failures"
        } else {
            "server_log"
        }
    }

    /// The record's `failures`: per failure its test, its message's first
    /// line (bounded) and its class.
    pub(crate) fn failures_value(&self) -> serde_json::Value {
        serde_json::Value::Array(
            self.failures
                .iter()
                .map(|(failure, class)| {
                    serde_json::json!({
                        "test": failure.test,
                        "message": first_line(&failure.message),
                        "class": class.name(),
                    })
                })
                .collect(),
        )
    }
}

/// A message's first line, cut to [`MESSAGE_CHARS`].
pub(crate) fn first_line(message: &str) -> String {
    super::cut(
        message.lines().next().unwrap_or_default().trim(),
        MESSAGE_CHARS,
    )
    .to_string()
}

/// Whether the gate's rejection (`reason`, its output `log`) is the host's:
/// check-tests.sh failed on the candidate, at least one failure was read,
/// and every failure read is the host's, a connection error to the test
/// database, or a database error while the test database's log shows a
/// crash in the gate's window (`crash`, asked only then). None when the
/// rejection stays a rejection.
pub(crate) fn host_fault(
    reason: &str,
    log: &str,
    crash: impl FnOnce() -> Vec<String>,
) -> Option<HostFault> {
    if reason.trim() != CHECK_TESTS_REGRESSION {
        return None;
    }
    let failures: Vec<(Failure, Class)> = check_tests_failures(log)
        .into_iter()
        .map(|failure| {
            let class = classify(&failure.message);
            (failure, class)
        })
        .collect();
    if failures.is_empty() || failures.iter().any(|(_, class)| *class == Class::Work) {
        return None;
    }
    if failures
        .iter()
        .all(|(_, class)| *class == Class::Connection)
    {
        return Some(HostFault {
            failures,
            crash: Vec::new(),
        });
    }
    let crash = crash();
    (!crash.is_empty()).then_some(HostFault { failures, crash })
}
