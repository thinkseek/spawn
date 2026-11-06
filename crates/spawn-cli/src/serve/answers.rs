//! How `spawn serve` sends what it read: validators, answers shared among
//! the pages that poll, and the lists' first lines.
//!
//! - Validation. Every API answer that succeeded carries a weak `ETag`, a
//!   hash of what it says; a request whose `If-None-Match` names it gets
//!   `304 Not Modified` and no body. A clock the answer carries only to say
//!   when it was made (`generated_at_ms` of `/api/rsi` and
//!   `/api/attention`, `captured_at_ms` of a pane) is left out of the hash:
//!   an answer that differs only there is the same answer, read later
//!   ([`CLOCKS`]). The page keeps what it last read with its tag, sends the
//!   tag on every poll, and on a 304 knows the answer still holds now.
//! - Sharing. The boards every open page polls ([`shared`]) are computed
//!   once per moment: an answer is kept [`SHARE_FOR`] (one second) from
//!   when its reads began, and a request that accepts an answer that old
//!   gets it, or waits for the one being computed, instead of reading the
//!   database again. A request accepts one by saying so, `Cache-Control:
//!   max-age=<seconds>` (capped at [`SHARE_FOR`]), which the page sends on
//!   its polls (every two seconds or more, so it never reads an answer
//!   older than its poll). Any other request (a script, `curl`, a test
//!   that reads what it just wrote) is computed fresh, and what it read is
//!   then shared. So N open pages cost one computation a second per board,
//!   and nobody reads an answer from before their own write.
//! - First lines. The boards' lists show a task's title and first lines;
//!   [`trim_texts`] cuts a long text to its first lines and says how long
//!   it was (`<field>_bytes`), and the whole text is a GET of its own,
//!   asked when the task is opened.
//!
//! Compression is not here: no gzip or deflate crate is in the offline
//! cargo registry the build uses (only zstd, which browsers send in
//! `Accept-Encoding` over https or to localhost only, never to this
//! server's plain http on the owner's network).

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hasher};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::Value;
use tiny_http::{Header, Request, Response};

/// How long a board's answer is shared from when its reads began.
pub(super) const SHARE_FOR: Duration = Duration::from_secs(1);

/// Answers kept past this many keys are swept of the expired ones.
const SWEEP_PAST: usize = 64;

/// One answer of an API read: its status, its JSON body and, when it
/// succeeded, its validator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Answer {
    pub(super) status: u16,
    pub(super) body: String,
    pub(super) etag: Option<String>,
}

/// The answer of a read of `path`: 200 with its weak ETag, or the error's
/// status (400 for a caller's mistake, `bad_request` prefixed, else 500)
/// and no tag.
pub(super) fn answer(path: &str, read: Result<Value, String>, bad_request: &str) -> Answer {
    match read {
        Ok(value) => {
            let body = value.to_string();
            let etag = etag(path, &value, &body);
            Answer {
                status: 200,
                body,
                etag: Some(etag),
            }
        }
        Err(error) => {
            let status = if error.starts_with(bad_request) {
                400
            } else {
                500
            };
            Answer {
                status,
                body: serde_json::json!({ "error": error }).to_string(),
                etag: None,
            }
        }
    }
}

/// Top-level fields of a path's answer that say only when it was made,
/// left out of its ETag.
fn clocks(path: &str) -> &'static [&'static str] {
    CLOCKS
        .iter()
        .find(|(prefix, _)| match prefix.strip_suffix('*') {
            Some(start) => path.starts_with(start),
            None => path == *prefix,
        })
        .map_or(&[], |(_, fields)| fields)
}

/// [`clocks`]' table: a path, or a prefix ending in `*`, and its clocks.
const CLOCKS: [(&str, &[&str]); 4] = [
    ("/api/rsi", &["generated_at_ms"]),
    ("/api/attention", &["generated_at_ms"]),
    ("/api/rsi/pane", &["captured_at_ms"]),
    ("/api/pane/*", &["captured_at_ms"]),
];

/// The weak ETag of an answer of `path`: a hash of its body (`body`, the
/// serialization of `value`) without its clocks.
fn etag(path: &str, value: &Value, body: &str) -> String {
    let fields = clocks(path);
    let mut hasher = DefaultHasher::new();
    match value.as_object() {
        Some(object) if fields.iter().any(|field| object.contains_key(*field)) => {
            let mut without = object.clone();
            for field in fields {
                without.remove(*field);
            }
            hasher.write(Value::Object(without).to_string().as_bytes());
        }
        _ => hasher.write(body.as_bytes()),
    }
    format!("W/\"{:016x}\"", hasher.finish())
}

/// The weak ETag of a static file's bytes.
pub(super) fn file_etag(bytes: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    hasher.write(bytes);
    format!("W/\"{:016x}\"", hasher.finish())
}

/// Whether an `If-None-Match` value names `etag` (weak comparison: `W/`
/// is ignored on both sides), or is `*`.
pub(super) fn not_modified(if_none_match: Option<&str>, etag: &str) -> bool {
    let opaque = |tag: &str| {
        let tag = tag.trim();
        tag.strip_prefix("W/").unwrap_or(tag).to_string()
    };
    let wanted = opaque(etag);
    if_none_match.is_some_and(|value| {
        value
            .split(',')
            .any(|tag| tag.trim() == "*" || opaque(tag) == wanted)
    })
}

/// How old a shared answer a request accepts, from its `Cache-Control`:
/// `max-age=<seconds>`, at most [`SHARE_FOR`]; none (zero) without it, or
/// with `no-cache` or `no-store`.
pub(super) fn accepted_age(cache_control: Option<&str>) -> Duration {
    let Some(value) = cache_control else {
        return Duration::ZERO;
    };
    let mut age = Duration::ZERO;
    for directive in value
        .split(',')
        .map(|part| part.trim().to_ascii_lowercase())
    {
        if directive == "no-cache" || directive == "no-store" {
            return Duration::ZERO;
        }
        if let Some(seconds) = directive
            .strip_prefix("max-age=")
            .and_then(|seconds| seconds.trim_matches('"').parse::<u64>().ok())
        {
            age = Duration::from_secs(seconds).min(SHARE_FOR);
        }
    }
    age
}

/// Whether `path`'s answers are shared ([`SHARE_FOR`]): the boards every
/// open page polls, and the panes. A session's commands and judgements,
/// its timeline, the log and a task's texts are read fresh every time (the
/// page's input box follows its commands).
pub(super) fn shared(path: &str) -> bool {
    matches!(
        path,
        "/api/overview"
            | "/api/attention"
            | "/api/fleet"
            | "/api/missions"
            | "/api/rsi"
            | "/api/rsi/pane"
            | "/api/scorecard"
    ) || path
        .strip_prefix("/api/pane/")
        .is_some_and(|id| !id.is_empty())
}

/// The key a shared answer is kept under: the path and its query without
/// the token (a page that sends it in the URL shares with one that sends a
/// header).
pub(super) fn key(path: &str, query: &str) -> String {
    let rest: Vec<&str> = query
        .split('&')
        .filter(|pair| !pair.is_empty() && !pair.starts_with("token=") && *pair != "token")
        .collect();
    if rest.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{}", rest.join("&"))
    }
}

/// An answer kept for sharing, with when its reads began.
struct Kept {
    at: Instant,
    answer: Arc<Answer>,
}

/// One key's place: the answer kept, locked while one is being computed.
type Slot = Arc<Mutex<Option<Kept>>>;

/// The shared answers, by [`key`].
#[derive(Default)]
pub(super) struct Answers {
    slots: Mutex<HashMap<String, Slot>>,
}

/// This server's shared answers.
pub(super) static ANSWERS: LazyLock<Answers> = LazyLock::new(Answers::default);

impl Answers {
    /// The answer for `key`: one kept from at most `accepted` ago, or the
    /// one another request is computing (waited for), or `compute`'s.
    /// With `accepted` zero, `compute`'s, computed now; it is kept for the
    /// requests that accept one, unless a later one is kept already.
    pub(super) fn get(
        &self,
        key: &str,
        accepted: Duration,
        compute: impl FnOnce() -> Answer,
    ) -> Arc<Answer> {
        let slot = self.slot(key);
        if accepted.is_zero() {
            let at = Instant::now();
            let answer = Arc::new(compute());
            // A computation in flight keeps its own, as fresh as this one.
            if let Ok(mut kept) = slot.try_lock()
                && kept.as_ref().is_none_or(|kept| kept.at < at)
            {
                *kept = Some(Kept {
                    at,
                    answer: Arc::clone(&answer),
                });
            }
            return answer;
        }
        let mut kept = slot.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(kept) = kept.as_ref()
            && kept.at.elapsed() <= accepted
        {
            return Arc::clone(&kept.answer);
        }
        let at = Instant::now();
        let answer = Arc::new(compute());
        *kept = Some(Kept {
            at,
            answer: Arc::clone(&answer),
        });
        answer
    }

    /// `key`'s slot, made when it has none. Past [`SWEEP_PAST`] keys, the
    /// ones whose answer expired (and that no request is computing) go.
    fn slot(&self, key: &str) -> Slot {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        if slots.len() > SWEEP_PAST && !slots.contains_key(key) {
            slots.retain(|_, slot| match slot.try_lock() {
                Ok(kept) => kept
                    .as_ref()
                    .is_some_and(|kept| kept.at.elapsed() <= SHARE_FOR),
                Err(_) => true,
            });
        }
        Arc::clone(slots.entry(key.to_string()).or_default())
    }

    /// How many keys are kept (for tests).
    #[cfg(test)]
    fn len(&self) -> usize {
        self.slots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

/// `Cache-Control` of every API answer: the page asks again every time
/// (with its tag), and no cache in between keeps one.
const API_CACHE_CONTROL: &str = "no-cache";

/// Send `answer` to `request`: 304 with the tag alone when the request's
/// `If-None-Match` names it, else the answer.
pub(super) fn respond(
    request: Request,
    answer: &Answer,
    if_none_match: Option<&str>,
) -> Result<(), String> {
    let cache = Header::from_bytes("Cache-Control", API_CACHE_CONTROL).expect("static header");
    let tag = answer
        .etag
        .as_deref()
        .map(|etag| Header::from_bytes("ETag", etag).expect("a hex tag is a valid header"));
    let unchanged = answer
        .etag
        .as_deref()
        .is_some_and(|etag| not_modified(if_none_match, etag));
    let response = match tag {
        Some(tag) if unchanged => Response::empty(304)
            .with_header(tag)
            .with_header(cache)
            .boxed(),
        tag => {
            let kind = Header::from_bytes("Content-Type", "application/json; charset=utf-8")
                .expect("static header");
            let mut response = Response::from_string(answer.body.clone())
                .with_status_code(answer.status)
                .with_header(kind)
                .with_header(cache);
            if let Some(tag) = tag {
                response.add_header(tag);
            }
            response.boxed()
        }
    };
    request.respond(response).map_err(|error| error.to_string())
}

/// `Cache-Control` of a static file: the build's `assets/` are named by
/// their content (Vite), so a name never changes what it holds; anything
/// else (index.html) is asked again every time, with its tag.
pub(super) fn file_cache_control(relative: &str) -> &'static str {
    if relative.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

/// Lines of a text a list shows.
pub(super) const EXCERPT_LINES: usize = 6;

/// Bytes of a text a list shows.
pub(super) const EXCERPT_BYTES: usize = 480;

/// The first lines of `text`: at most `lines` lines and `bytes` bytes,
/// cut at a character boundary. A prefix of `text`.
pub(super) fn excerpt(text: &str, lines: usize, bytes: usize) -> &str {
    let by_lines = text
        .match_indices('\n')
        .nth(lines.saturating_sub(1))
        .map_or(text.len(), |(at, _)| at);
    let mut end = by_lines.min(bytes).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Each of `fields` of `object` (a JSON object) cut to its first lines
/// ([`excerpt`]), with its whole length in bytes beside it as
/// `<field>_bytes` (0 for a null or absent one). With `keep` false the
/// field is null and only its length is sent.
pub(super) fn trim_texts(
    object: &mut Value,
    fields: &[&str],
    lines: usize,
    bytes: usize,
    keep: bool,
) {
    let Some(object) = object.as_object_mut() else {
        return;
    };
    for field in fields {
        let (length, cut) = match object.get(*field).and_then(Value::as_str) {
            Some(text) => (
                text.len(),
                keep.then(|| Value::from(excerpt(text, lines, bytes))),
            ),
            None => (0, None),
        };
        if object.contains_key(*field) || length > 0 {
            object.insert((*field).to_string(), cut.unwrap_or(Value::Null));
        }
        object.insert(format!("{field}_bytes"), Value::from(length));
    }
}

/// Whether a mission is closed, as the page folds it (web/src/ordering.ts
/// `isActiveMission`): not halted, and in a state other than open or
/// active.
fn closed(mission: &Value) -> bool {
    mission["halted"].is_null()
        && mission["state"]
            .as_str()
            .is_some_and(|state| state != "open" && state != "active")
}

/// `/api/missions`' board as the page's lists show it: every task's brief
/// and review note cut to their first lines, and its report too, except in
/// a closed mission or a cancelled task (both folded on the page), whose
/// report is sent as its length alone. The texts in full are
/// `/api/missions/task/<id>`.
pub(super) fn slim_board(board: &mut Value) {
    let Some(missions) = board.get_mut("missions").and_then(Value::as_array_mut) else {
        return;
    };
    for mission in missions {
        let folded = closed(mission);
        let Some(tasks) = mission.get_mut("tasks").and_then(Value::as_array_mut) else {
            continue;
        };
        for task in tasks {
            let cancelled = task["state"] == "cancelled";
            trim_texts(
                task,
                &["brief", "review_note"],
                EXCERPT_LINES,
                EXCERPT_BYTES,
                true,
            );
            trim_texts(
                task,
                &["report"],
                EXCERPT_LINES,
                EXCERPT_BYTES,
                !folded && !cancelled,
            );
        }
    }
}

/// Lines of an objective's prompt the RSI page shows (its card's four
/// lines, and the title a pointer shows, which browsers cut near 1 KB).
const PROMPT_LINES: usize = 10;

/// Bytes of an objective's prompt the RSI page shows.
const PROMPT_BYTES: usize = 1024;

/// `/api/rsi`'s missions as the page shows them: each objective's prompt
/// and each task's brief and review note cut to their first lines (the
/// task in full is `/api/rsi/task/<id>`).
pub(super) fn slim_rsi(missions: &mut [Value]) {
    for mission in missions {
        if let Some(objectives) = mission.get_mut("objectives").and_then(Value::as_array_mut) {
            for objective in objectives {
                trim_texts(objective, &["prompt"], PROMPT_LINES, PROMPT_BYTES, true);
            }
        }
        if let Some(tasks) = mission.get_mut("tasks").and_then(Value::as_array_mut) {
            for task in tasks {
                trim_texts(
                    task,
                    &["brief", "review_note"],
                    EXCERPT_LINES,
                    EXCERPT_BYTES,
                    true,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use serde_json::json;

    use super::{
        Answer, Answers, SHARE_FOR, SWEEP_PAST, accepted_age, answer, excerpt, file_cache_control,
        key, not_modified, shared, slim_board, slim_rsi, trim_texts,
    };

    #[test]
    fn an_answer_is_tagged_by_what_it_says_and_an_error_is_not_tagged() {
        let one = answer("/api/fleet", Ok(json!({"a": 1})), "bad request: ");
        let same = answer("/api/fleet", Ok(json!({"a": 1})), "bad request: ");
        let other = answer("/api/fleet", Ok(json!({"a": 2})), "bad request: ");
        assert_eq!(one.status, 200);
        assert_eq!(one.body, r#"{"a":1}"#);
        let tag = one.etag.clone().expect("a tag");
        assert!(tag.starts_with("W/\"") && tag.ends_with('"'), "{tag}");
        assert_eq!(one.etag, same.etag);
        assert_ne!(one.etag, other.etag);

        let bad = answer("/api/fleet", Err("bad request: no".into()), "bad request: ");
        assert_eq!((bad.status, bad.etag), (400, None));
        assert_eq!(bad.body, r#"{"error":"bad request: no"}"#);
        let failed = answer("/api/fleet", Err("down".into()), "bad request: ");
        assert_eq!((failed.status, failed.etag), (500, None));
    }

    #[test]
    fn a_clock_that_only_says_when_is_left_out_of_the_tag_and_kept_in_the_body() {
        let early = answer(
            "/api/rsi",
            Ok(json!({"generated_at_ms": 1, "missions": []})),
            "",
        );
        let late = answer(
            "/api/rsi",
            Ok(json!({"generated_at_ms": 2, "missions": []})),
            "",
        );
        assert_eq!(early.etag, late.etag, "the same answer, read later");
        assert!(late.body.contains("\"generated_at_ms\":2"));
        let changed = answer(
            "/api/rsi",
            Ok(json!({"generated_at_ms": 2, "missions": [1]})),
            "",
        );
        assert_ne!(early.etag, changed.etag);
        // A pane's capture time likewise; the fleet's clock is part of it.
        let pane = |at: i64| {
            answer(
                "/api/pane/session_a",
                Ok(json!({"captured_at_ms": at, "text": "x"})),
                "",
            )
            .etag
        };
        assert_eq!(pane(1), pane(2));
        let fleet = |at: i64| answer("/api/fleet", Ok(json!({"generated_at_ms": at})), "").etag;
        assert_ne!(fleet(1), fleet(2));
    }

    #[test]
    fn if_none_match_compares_weakly_and_takes_lists_and_a_star() {
        let tag = "W/\"00ff\"";
        assert!(not_modified(Some("W/\"00ff\""), tag));
        assert!(not_modified(Some("\"00ff\""), tag), "weak comparison");
        assert!(not_modified(Some("\"1\", W/\"00ff\""), tag));
        assert!(not_modified(Some("*"), tag));
        assert!(!not_modified(Some("W/\"00fe\""), tag));
        assert!(!not_modified(Some(""), tag));
        assert!(!not_modified(None, tag));
    }

    #[test]
    fn a_request_accepts_a_shared_answer_only_by_saying_how_old_capped_at_a_second() {
        assert_eq!(accepted_age(None), Duration::ZERO);
        assert_eq!(accepted_age(Some("max-age=1")), SHARE_FOR);
        assert_eq!(accepted_age(Some("max-age=60")), SHARE_FOR, "capped");
        assert_eq!(accepted_age(Some("max-age=0")), Duration::ZERO);
        assert_eq!(accepted_age(Some("no-cache")), Duration::ZERO);
        assert_eq!(accepted_age(Some("max-age=1, no-cache")), Duration::ZERO);
        assert_eq!(accepted_age(Some("Max-Age=1")), SHARE_FOR);
        assert_eq!(accepted_age(Some("max-age=x")), Duration::ZERO);
    }

    #[test]
    fn the_boards_and_panes_are_shared_and_a_sessions_commands_are_not() {
        for path in [
            "/api/overview",
            "/api/attention",
            "/api/fleet",
            "/api/missions",
            "/api/rsi",
            "/api/rsi/pane",
            "/api/scorecard",
            "/api/pane/session_a",
        ] {
            assert!(shared(path), "{path}");
        }
        for path in [
            "/api/fleet/session_a/commands",
            "/api/fleet/session_a/judgements",
            "/api/session/session_a",
            "/api/events",
            "/api/missions/task/3",
            "/api/rsi/task/3",
            "/api/pane/",
        ] {
            assert!(!shared(path), "{path}");
        }
        assert_eq!(key("/api/fleet", ""), "/api/fleet");
        assert_eq!(key("/api/fleet", "token=abc"), "/api/fleet");
        assert_eq!(
            key("/api/scorecard", "since=7d&token=abc&mission=4"),
            "/api/scorecard?since=7d&mission=4"
        );
    }

    fn computed(count: &AtomicUsize, body: &str) -> Answer {
        count.fetch_add(1, Ordering::SeqCst);
        Answer {
            status: 200,
            body: body.to_string(),
            etag: None,
        }
    }

    #[test]
    fn a_shared_answer_serves_for_a_second_and_a_fresh_request_always_reads() {
        let answers = Answers::default();
        let count = AtomicUsize::new(0);
        let first = answers.get("k", SHARE_FOR, || computed(&count, "one"));
        let second = answers.get("k", SHARE_FOR, || computed(&count, "two"));
        assert_eq!((first.body.as_str(), second.body.as_str()), ("one", "one"));
        assert_eq!(count.load(Ordering::SeqCst), 1, "computed once");
        // A request that accepts no age reads again, and shares what it read.
        let fresh = answers.get("k", Duration::ZERO, || computed(&count, "three"));
        assert_eq!(fresh.body, "three");
        let after = answers.get("k", SHARE_FOR, || computed(&count, "four"));
        assert_eq!(after.body, "three");
        assert_eq!(count.load(Ordering::SeqCst), 2);
        // Another key is another answer.
        assert_eq!(
            answers.get("j", SHARE_FOR, || computed(&count, "j")).body,
            "j"
        );
        // Past its age it is read again.
        std::thread::sleep(Duration::from_millis(30));
        let old = answers.get("k", Duration::from_millis(10), || computed(&count, "five"));
        assert_eq!(old.body, "five");
    }

    #[test]
    fn concurrent_requests_for_one_board_wait_for_one_computation() {
        let answers = Answers::default();
        let count = AtomicUsize::new(0);
        let bodies: Vec<String> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        answers
                            .get("board", SHARE_FOR, || {
                                std::thread::sleep(Duration::from_millis(100));
                                computed(&count, "board")
                            })
                            .body
                            .clone()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("a request"))
                .collect()
        });
        assert!(bodies.iter().all(|body| body == "board"));
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "eight requests, one computation"
        );
    }

    #[test]
    fn expired_answers_are_swept_once_many_keys_are_kept() {
        let answers = Answers::default();
        let count = AtomicUsize::new(0);
        for index in 0..=SWEEP_PAST {
            answers.get(&format!("k{index}"), Duration::from_millis(1), || {
                computed(&count, "x")
            });
        }
        assert_eq!(answers.len(), SWEEP_PAST + 1);
        std::thread::sleep(SHARE_FOR + Duration::from_millis(50));
        answers.get("new", SHARE_FOR, || computed(&count, "y"));
        assert_eq!(answers.len(), 1, "only the new key is left");
    }

    #[test]
    fn an_excerpt_is_the_first_lines_cut_at_a_character_boundary() {
        assert_eq!(excerpt("a\nb\nc\nd", 2, 100), "a\nb");
        assert_eq!(excerpt("a\nb", 6, 100), "a\nb");
        assert_eq!(excerpt("abcdef", 6, 4), "abcd");
        assert_eq!(excerpt("", 6, 4), "");
        // "é" is two bytes: a cut never splits it.
        assert_eq!(excerpt("aé", 6, 2), "a");
        assert_eq!(excerpt("ééé", 6, 5), "éé");
        let long = "x".repeat(1000);
        assert_eq!(excerpt(&long, 6, 480).len(), 480);
    }

    #[test]
    fn trimmed_texts_keep_their_first_lines_and_their_length() {
        let mut task = json!({"id": 1, "brief": "Title\nline 2\nline 3", "report": "r".repeat(600),
                              "review_note": null});
        trim_texts(
            &mut task,
            &["brief", "report", "review_note", "absent"],
            2,
            480,
            true,
        );
        assert_eq!(task["brief"], "Title\nline 2");
        assert_eq!(task["brief_bytes"], 19);
        assert_eq!(task["report"].as_str().map(str::len), Some(480));
        assert_eq!(task["report_bytes"], 600);
        assert_eq!(task["review_note"], json!(null));
        assert_eq!(task["review_note_bytes"], 0);
        assert!(task.get("absent").is_none(), "an absent field stays absent");
        assert_eq!(task["absent_bytes"], 0);
        let mut folded = json!({"report": "done"});
        trim_texts(&mut folded, &["report"], 2, 480, false);
        assert_eq!(folded, json!({"report": null, "report_bytes": 4}));
    }

    #[test]
    fn the_board_sends_first_lines_and_no_report_of_a_closed_mission_or_cancelled_task() {
        let report = "line\n".repeat(50);
        let mut board = json!({"missions": [
            {"id": 2, "state": "active", "halted": null, "tasks": [
                {"id": 5, "state": "reported", "brief": "B", "report": report, "review_note": "n"},
                {"id": 6, "state": "cancelled", "brief": "B", "report": "r", "review_note": null}]},
            {"id": 1, "state": "done", "halted": null, "tasks": [
                {"id": 3, "state": "landed", "brief": "B", "report": "r", "review_note": null}]},
            {"id": 0, "state": "done", "halted": "why", "tasks": [
                {"id": 4, "state": "landed", "brief": "B", "report": "r", "review_note": null}]}],
            "residents": []});
        slim_board(&mut board);
        let task = |mission: usize, task: usize| board["missions"][mission]["tasks"][task].clone();
        assert_eq!(
            task(0, 0)["report"],
            "line\n".repeat(6).trim_end_matches('\n')
        );
        assert_eq!(task(0, 0)["report_bytes"], 250);
        assert_eq!(task(0, 0)["review_note"], "n");
        assert_eq!(
            task(0, 1)["report"],
            json!(null),
            "a cancelled task is folded"
        );
        assert_eq!(task(0, 1)["report_bytes"], 1);
        assert_eq!(
            task(1, 0)["report"],
            json!(null),
            "a closed mission is folded"
        );
        assert_eq!(task(1, 0)["brief"], "B", "its titles stay");
        assert_eq!(task(2, 0)["report"], "r", "a halted mission is open");
    }

    #[test]
    fn the_rsi_page_gets_its_prompts_and_briefs_cut() {
        let prompt = "p".repeat(5000);
        let mut missions = vec![json!({"objectives": [{"id": 1, "prompt": prompt}],
                                       "tasks": [{"id": 9, "brief": "b\n".repeat(20),
                                                  "review_note": "ok"}]})];
        slim_rsi(&mut missions);
        assert_eq!(
            missions[0]["objectives"][0]["prompt"]
                .as_str()
                .map(str::len),
            Some(1024)
        );
        assert_eq!(missions[0]["objectives"][0]["prompt_bytes"], 5000);
        assert_eq!(
            missions[0]["tasks"][0]["brief"],
            "b\n".repeat(6).trim_end_matches('\n')
        );
        assert_eq!(missions[0]["tasks"][0]["brief_bytes"], 40);
        assert_eq!(missions[0]["tasks"][0]["review_note"], "ok");
    }

    #[test]
    fn built_assets_are_cached_for_good_and_the_page_is_asked_again() {
        assert_eq!(
            file_cache_control("assets/index-cf885A5y.js"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(file_cache_control("index.html"), "no-cache");
    }
}
