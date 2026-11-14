//! The owner's interventions through the queue (objective 156): lead-mac's
//! headings in sync/queue.md, counted by `/api/interventions` beside what
//! the owner did through Spawn, when the ranges file names the queue
//! (`{"observability": {"queue_file": <absolute path>,
//! "queue_utc_offset_minutes": <minutes east of UTC>}}`).
//!
//! qpost writes each heading as `## <who> HH:MM — <text>`, stamped by its
//! host's local clock, with no date. A heading's day is read back from the
//! end of the file: the newest is on the local date of now (the day before
//! when its time is more than 5 minutes later than now's), and each earlier
//! heading more than 12 h later than the heading after it is a day before
//! it; each earlier heading more than 23 h 30 min earlier than the heading
//! after it is the day after it (a stamp up to half an hour out of order
//! across midnight: lead-mac's 00:10 above the lead's 23:50); otherwise the
//! same day (headings minutes out of order are no midnight). The price: a
//! gap of more than 23 h 30 min within one local day (a silence of nearly a
//! day starting right after midnight) is read as the next day. Only lead-mac's headings count: the lead relays what lead-mac
//! posted, and a reviewer's post is no intervention.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde_json::{Value, json};

use super::measure::Window;
use super::time::DAY_MS;

/// The only author whose headings count.
const OWNER_HOST: &str = "lead-mac";
/// How much of the file is read: its last 4 MiB, from the first whole line.
const CAP: u64 = 4 * 1024 * 1024;
/// The largest offset from UTC a clock has (14 h); beyond it, UTC.
const MAX_OFFSET_MINUTES: i64 = 14 * 60;
/// A newest heading later than now by more than this is yesterday's.
const NEWEST_SLACK_MINUTES: i64 = 5;
/// An earlier heading later than the one after it by more than this is a
/// day before it.
const WRAP_MINUTES: i64 = 12 * 60;
/// An earlier heading earlier than the one after it by more than this is
/// the day after it: a stamp up to half an hour out of order across midnight.
const BACK_ACROSS_MIDNIGHT_MINUTES: i64 = 23 * 60 + 30;
/// A line's length, in characters, before "…".
const LINE_CHARS: usize = 120;
const MINUTE_MS: i64 = 60_000;

/// The queue the ranges file names, and its clock's offset from UTC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Settings {
    pub(crate) path: PathBuf,
    pub(crate) offset_minutes: i64,
}

/// The settings in the ranges file's text: none without a file, a broken
/// one, no `observability.queue_file` or a relative one. The offset is 0
/// when absent, not a whole number, or beyond 14 h.
pub(crate) fn settings(ranges: Option<&str>) -> Option<Settings> {
    let value: Value = serde_json::from_str(ranges?).ok()?;
    let observability = value.get("observability")?;
    let path = PathBuf::from(observability.get("queue_file")?.as_str()?);
    if !path.is_absolute() {
        return None;
    }
    let offset_minutes = observability
        .get("queue_utc_offset_minutes")
        .and_then(Value::as_i64)
        .filter(|minutes| minutes.abs() <= MAX_OFFSET_MINUTES)
        .unwrap_or(0);
    Some(Settings {
        path,
        offset_minutes,
    })
}

/// A heading: its author, its minute of the day, its text.
#[derive(Debug, PartialEq, Eq)]
struct Heading<'a> {
    who: &'a str,
    minute: i64,
    text: &'a str,
}

/// `H:MM` or `HH:MM`, a time of day, as its minute.
fn minute_of(token: &str) -> Option<i64> {
    let (hours, minutes) = token.split_once(':')?;
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    if !(1..=2).contains(&hours.len()) || minutes.len() != 2 || !digits(hours) || !digits(minutes) {
        return None;
    }
    let (hours, minutes): (i64, i64) = (hours.parse().ok()?, minutes.parse().ok()?);
    (hours < 24 && minutes < 60).then_some(hours * 60 + minutes)
}

/// `## <who> <H:MM or HH:MM> <text>`, the text with its leading dash and
/// spaces trimmed; any other line is none.
fn heading(line: &str) -> Option<Heading<'_>> {
    let rest = line.strip_prefix("## ")?.trim_start();
    let (who, rest) = rest.split_once(char::is_whitespace)?;
    let rest = rest.trim_start();
    let (time, text) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let minute = minute_of(time)?;
    let text = text.trim_start();
    let text = text
        .strip_prefix('—')
        .or_else(|| text.strip_prefix('-'))
        .unwrap_or(text)
        .trim();
    Some(Heading { who, minute, text })
}

/// The word "owner", in any case, in `text`: "ownership" is not the word.
fn names_the_owner(text: &str) -> bool {
    text.split(|c: char| !c.is_alphanumeric())
        .any(|word| word.to_lowercase() == "owner")
}

/// The text as an item's line: control characters dropped, cut at 120
/// characters with "…" after.
fn line_of(text: &str) -> String {
    let kept: Vec<char> = text.chars().filter(|c| !c.is_control()).collect();
    if kept.len() > LINE_CHARS {
        let mut line: String = kept[..LINE_CHARS].iter().collect();
        line.push('…');
        line
    } else {
        kept.into_iter().collect()
    }
}

/// lead-mac's posts in `text` within `[since, until)`, as the store's items
/// are shaped, newest first, each placed against `now_ms`.
pub(crate) fn posts(text: &str, offset_minutes: i64, window: Window, now_ms: i64) -> Vec<Value> {
    let offset_ms = offset_minutes * MINUTE_MS;
    let local_now = now_ms + offset_ms;
    let mut day = local_now.div_euclid(DAY_MS);
    let now_minute = local_now.rem_euclid(DAY_MS) / MINUTE_MS;
    let mut after: Option<i64> = None;
    let mut items = Vec::new();
    for heading in text.lines().rev().filter_map(heading) {
        match after {
            None if heading.minute - now_minute > NEWEST_SLACK_MINUTES => day -= 1,
            Some(later) if heading.minute - later > WRAP_MINUTES => day -= 1,
            Some(later) if later - heading.minute > BACK_ACROSS_MIDNIGHT_MINUTES => day += 1,
            _ => {}
        }
        after = Some(heading.minute);
        if heading.who != OWNER_HOST {
            continue;
        }
        let at_ms = day * DAY_MS + heading.minute * MINUTE_MS - offset_ms;
        if at_ms < window.since || at_ms >= window.until {
            continue;
        }
        let kind = if names_the_owner(heading.text) {
            "owner_decision"
        } else {
            "steering"
        };
        items.push(json!({
            "kind": kind,
            "at_ms": at_ms,
            "principal": OWNER_HOST,
            "agent": null,
            "session": null,
            "line": line_of(heading.text),
            "commands": [],
            "attempts": null,
            "outcome": null,
            "source": "queue",
        }));
    }
    items
}

/// The text kept from the last read, and what the file was then.
struct Kept {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
    text: std::sync::Arc<str>,
}

static KEPT: Mutex<Option<Kept>> = Mutex::new(None);

/// The file read whole up to 4 MiB, beyond that its last 4 MiB from the
/// first whole line.
fn read_tail(path: &Path, len: u64, cap: u64) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    if len > cap {
        file.seek(SeekFrom::Start(len - cap))?;
        file.take(cap).read_to_end(&mut bytes)?;
        let start = bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |at| at + 1);
        bytes.drain(..start);
    } else {
        file.read_to_end(&mut bytes)?;
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The queue's text, read again only when its length or modification time
/// changed since the last read.
pub(crate) fn text(path: &Path) -> std::io::Result<std::sync::Arc<str>> {
    text_capped(path, CAP)
}

fn text_capped(path: &Path, cap: u64) -> std::io::Result<std::sync::Arc<str>> {
    let meta = std::fs::metadata(path)?;
    let (len, modified) = (meta.len(), meta.modified().ok());
    let mut kept = KEPT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(kept) = kept.as_ref()
        && kept.path == path
        && kept.len == len
        && kept.modified == modified
    {
        return Ok(kept.text.clone());
    }
    let text: std::sync::Arc<str> = read_tail(path, len, cap)?.into();
    *kept = Some(Kept {
        path: path.to_path_buf(),
        len,
        modified,
        text: text.clone(),
    });
    Ok(text)
}

/// The posts of the queue `settings` names within `window`, placed by the
/// wall clock; none when the file cannot be read (said on stderr).
pub(crate) fn read(settings: &Settings, window: Window, now_ms: i64) -> Vec<Value> {
    match text(&settings.path) {
        Ok(text) => posts(&text, settings.offset_minutes, window, now_ms),
        Err(error) => {
            eprintln!(
                "interventions: the queue {}: {error}",
                settings.path.display()
            );
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::Value;

    use super::{Settings, Window, posts, settings, text_capped};
    use crate::scorecard::time::{DAY_MS, HOUR_MS};

    const MIN: i64 = 60_000;

    /// `(kind, local minute of the day, day back from now's, line)`.
    fn summary(items: &[Value], offset: i64, now_ms: i64) -> Vec<(String, i64, i64, String)> {
        let today = (now_ms + offset * MIN).div_euclid(DAY_MS);
        items
            .iter()
            .map(|item| {
                let local = item["at_ms"].as_i64().unwrap() + offset * MIN;
                (
                    item["kind"].as_str().unwrap().to_string(),
                    local.rem_euclid(DAY_MS) / MIN,
                    today - local.div_euclid(DAY_MS),
                    item["line"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    fn all() -> Window {
        Window {
            since: i64::MIN,
            until: i64::MAX,
        }
    }

    #[test]
    fn lead_macs_posts_are_placed_by_the_midnight_wraps_and_counted_by_kind() {
        // Now: 2026-11-14 10:00 local, 8 h east of UTC.
        let offset = 480;
        let today = 20_406_i64;
        let now = today * DAY_MS + 10 * HOUR_MS - offset * MIN;
        let queue = "\
# The queue

## lead-mac 22:00 — two days back
## lead-mac 23:56 — Owner: before the first midnight
## lead-mac 00:34 — after it, ownership moved
## lead-mac 01:00 — a backstep follows
## lead 00:32 — 28 minutes earlier, the same day
## lead 23:50 — before the second midnight
## lead 00:20 — after it
## lead-mac 00:18 — two minutes under the lead's
## lead-mac 08:00 — morning
- 09:00 a list line
## lead-mac — no time
## rev-b 09:10 — a reviewer
## lead-mac 09:20 — the owner decides
## lead-mac 09:55 — stamped before the next
## lead-mac 09:40 — 15 minutes earlier
";
        let items = posts(queue, offset, all(), now);
        let got = summary(&items, offset, now);
        let want: Vec<(String, i64, i64, String)> = [
            ("steering", 9 * 60 + 40, 0, "15 minutes earlier"),
            ("steering", 9 * 60 + 55, 0, "stamped before the next"),
            ("owner_decision", 9 * 60 + 20, 0, "the owner decides"),
            ("steering", 8 * 60, 0, "morning"),
            ("steering", 18, 0, "two minutes under the lead's"),
            ("steering", 60, 1, "a backstep follows"),
            ("steering", 34, 1, "after it, ownership moved"),
            (
                "owner_decision",
                23 * 60 + 56,
                2,
                "Owner: before the first midnight",
            ),
            ("steering", 22 * 60, 2, "two days back"),
        ]
        .into_iter()
        .map(|(kind, minute, back, line)| (kind.to_string(), minute, back, line.to_string()))
        .collect();
        assert_eq!(got, want);
        // The day's window: today's five.
        let day = Window {
            since: today * DAY_MS - offset * MIN,
            until: (today + 1) * DAY_MS - offset * MIN,
        };
        assert_eq!(posts(queue, offset, day, now).len(), 5);
        // The window's end is left out.
        let until = Window {
            since: day.since,
            until: today * DAY_MS + (9 * 60 + 40) * MIN - offset * MIN,
        };
        assert_eq!(posts(queue, offset, until, now).len(), 3);

        // The 12 h line: 12:59 above 01:00 is the same day, 13:01 a day back.
        for (above, back) in [("12:59", 0), ("13:01", 1)] {
            let queue = format!("## lead-mac {above} — above\n## lead-mac 01:00 — below\n");
            let got = summary(&posts(&queue, offset, all(), now), offset, now);
            assert_eq!(
                (got[1].1, got[1].2),
                (if back == 0 { 779 } else { 781 }, back),
                "{above}"
            );
        }

        // A hostile heading keeps its text, the escape character dropped,
        // cut at 120 characters then "…".
        let long = "y".repeat(200);
        let queue = format!("## lead-mac 09:00 — x\u{1b}[31m <b>hi</b> & {long}\n");
        let line = summary(&posts(&queue, offset, all(), now), offset, now)[0]
            .3
            .clone();
        let want: String = format!("x[31m <b>hi</b> & {long}")
            .chars()
            .take(120)
            .collect();
        assert_eq!(line, format!("{want}…"));
        assert_eq!(line.chars().count(), 121);

        // The newest heading: more than 5 minutes later than now is
        // yesterday's, 3 minutes later today's.
        for (time, back) in [("10:06", 1), ("10:03", 0)] {
            let queue = format!("## lead-mac {time} — newest\n");
            let got = summary(&posts(&queue, offset, all(), now), offset, now);
            assert_eq!(got[0].2, back, "{time}");
        }

        // Offset 0 is UTC.
        let queue = "## lead-mac 01:30 — utc\n";
        let at = posts(queue, 0, all(), now)[0]["at_ms"].as_i64().unwrap();
        assert_eq!(at, today * DAY_MS + 90 * MIN);
        let at = posts(queue, offset, all(), now)[0]["at_ms"]
            .as_i64()
            .unwrap();
        assert_eq!(at, today * DAY_MS + 90 * MIN - offset * MIN);

        // Each item in the store's shape.
        let item = &posts("## lead-mac 09:00 — Owner: yes\n", offset, all(), now)[0];
        assert_eq!(item["principal"], "lead-mac");
        assert_eq!(item["source"], "queue");
        assert_eq!(item["kind"], "owner_decision");
        assert_eq!(item["commands"], serde_json::json!([]));
        for key in ["agent", "session", "attempts", "outcome"] {
            assert_eq!(item[key], Value::Null, "{key}");
        }
    }

    #[test]
    fn a_stamp_out_of_order_across_midnight_is_the_day_after() {
        // Now: 03:10 local, 8 h east of UTC.
        let offset = 480;
        let today = 20_407_i64;
        let now = today * DAY_MS + (3 * 60 + 10) * MIN - offset * MIN;
        let queue = "\
## lead-mac 22:00 — above it
## lead-mac 00:10 — stamped 15 minutes after the lead's, above it
## lead 23:55 — the lead's
## lead-mac 03:00 — newest
";
        let got = summary(&posts(queue, offset, all(), now), offset, now);
        let want: Vec<(String, i64, i64, String)> = [
            ("steering", 3 * 60, 0, "newest"),
            (
                "steering",
                10,
                0,
                "stamped 15 minutes after the lead's, above it",
            ),
            ("steering", 22 * 60, 1, "above it"),
        ]
        .into_iter()
        .map(|(kind, minute, back, line)| (kind.to_string(), minute, back, line.to_string()))
        .collect();
        assert_eq!(got, want);

        // The line: 29 minutes out of order across midnight is the day
        // after; 31 minutes is the 12 h rule's day, yesterday.
        for (above, back) in [("00:14", 0), ("00:16", 1)] {
            let queue = format!(
                "## lead-mac {above} — above\n## lead 23:45 — the lead's\n## lead-mac 03:00 — newest\n"
            );
            let got = summary(&posts(&queue, offset, all(), now), offset, now);
            assert_eq!(got.len(), 2, "{above}");
            assert_eq!(got[1].2, back, "{above}");
        }
    }

    #[test]
    fn the_queue_is_read_only_when_the_ranges_file_names_it() {
        let named = r#"{"observability": {"queue_file": "/srv/sync/queue.md", "queue_utc_offset_minutes": 480}}"#;
        assert_eq!(
            settings(Some(named)),
            Some(Settings {
                path: PathBuf::from("/srv/sync/queue.md"),
                offset_minutes: 480
            })
        );
        for (offset, want) in [
            ("-300", -300),
            ("840", 840),
            ("841", 0),
            ("-900", 0),
            ("\"8\"", 0),
        ] {
            let text = format!(
                r#"{{"observability": {{"queue_file": "/q.md", "queue_utc_offset_minutes": {offset}}}}}"#
            );
            assert_eq!(
                settings(Some(&text)).map(|s| s.offset_minutes),
                Some(want),
                "{offset}"
            );
        }
        let no_offset = r#"{"ranges": {}, "observability": {"queue_file": "/q.md"}}"#;
        assert_eq!(settings(Some(no_offset)).map(|s| s.offset_minutes), Some(0));
        for none in [
            r#"{"ranges": {}}"#,
            r#"{"observability": {}}"#,
            r#"{"observability": {"queue_file": "sync/queue.md"}}"#,
            r#"{"observability": {"queue_file": 3}}"#,
            "{\"observability\": ",
        ] {
            assert_eq!(settings(Some(none)), None, "{none}");
        }
        assert_eq!(settings(None), None);
    }

    #[test]
    fn the_file_is_read_from_its_tail_and_again_when_it_changes() {
        let dir = std::env::temp_dir().join(format!("spawn-queue-posts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("queue.md");
        // Over 4 MiB: the last 4 MiB from the first whole line.
        let head = "## lead-mac 01:00 — the head\n";
        let filler = format!("{}\n", "z".repeat(99));
        let mut big = String::from(head);
        while big.len() <= 4 * 1024 * 1024 + 50 {
            big.push_str(&filler);
        }
        big.push_str("## lead-mac 02:00 — the tail\n");
        std::fs::write(&path, &big).unwrap();
        let text = super::text(&path).unwrap();
        assert!(text.len() <= 4 * 1024 * 1024);
        assert!(!text.contains("the head"));
        assert!(text.ends_with("## lead-mac 02:00 — the tail\n"));
        assert!(text.starts_with(&filler), "from the first whole line");

        // A changed file is read again; an unchanged one is the kept text.
        std::fs::write(&path, "## lead-mac 01:00 — one\n").unwrap();
        assert_eq!(
            &*text_capped(&path, 1024).unwrap(),
            "## lead-mac 01:00 — one\n"
        );
        std::fs::write(&path, "## lead-mac 01:00 — one\n## lead-mac 02:00 — two\n").unwrap();
        let again = text_capped(&path, 1024).unwrap();
        assert!(again.contains("two"));
        let kept = text_capped(&path, 1024).unwrap();
        assert!(std::sync::Arc::ptr_eq(&again, &kept));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
