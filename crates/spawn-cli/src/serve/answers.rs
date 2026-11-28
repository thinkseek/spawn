//! How `spawn serve` sends what it read: validators, answers shared among
//! the pages that poll, and the lists' first lines.
//!
//! - Validation. Every API answer that succeeded carries a weak `ETag`, a
//!   hash of what it says; a request whose `If-None-Match` names it gets
//!   `304 Not Modified` and no body. Pure freshness clocks (`generated_at_ms`
//!   or `captured_at_ms` across [`CLOCKS`] and [`PATTERN_CLOCKS`] — covering
//!   `/api/overview`, `/api/fleet`, `/api/missions`, `/api/objectives`,
//!   `/api/approvals`, `/api/context/digest`, `/api/attention`,
//!   `/api/features`, `/api/objectives/pane`, and `/api/sessions/*/pane`,
//!   plus sub-minute/sub-MB `health` bucketing in `normalize_fleet_etag`) are
//!   left out of the hash: an answer that differs only there is the same
//!   answer, read later. The page keeps what it last read with its tag, sends
//!   the tag on every poll, and on a 304 knows the answer still holds now.
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
//! - Compression. API answers and compressible static assets are
//!   negotiated with RFC 1952 `gzip` ([`super::gzip`]) whenever the client
//!   advertises `Accept-Encoding: gzip` and compression shrinks the payload,
//!   carrying `Vary: Accept-Encoding` and keeping the weak `ETag` (`W/"..."`).

use std::collections::{HashMap, VecDeque};
use std::hash::{DefaultHasher, Hasher};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};
use tiny_http::{Header, Request, Response};

use super::gzip;

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
/// status (400 for a caller's mistake, `bad_request` prefixed, 503 when
/// saturated, else 500) and no tag.
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
            } else if error.starts_with(super::BUSY) {
                503
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
        .chain(PATTERN_CLOCKS.iter())
        .find(|(pattern, _)| fits(pattern, path))
        .map_or(&[], |(_, fields)| fields)
}

/// Whether `path` fits `pattern`, segment by segment: a `*` segment is any
/// one segment that is not empty, any other must be equal.
fn fits(pattern: &str, path: &str) -> bool {
    let mut wanted = pattern.split('/');
    let mut given = path.split('/');
    loop {
        match (wanted.next(), given.next()) {
            (None, None) => return true,
            (Some("*"), Some(segment)) if !segment.is_empty() => {}
            (Some(want), Some(segment)) if want == segment => {}
            _ => return false,
        }
    }
}

/// [`clocks`]' table: a canonical path (`*` for one segment) and its clocks.
const CLOCKS: [(&str, &[&str]); 9] = [
    ("/api/overview", &["generated_at_ms"]),
    ("/api/fleet", &["generated_at_ms"]),
    ("/api/missions", &["generated_at_ms"]),
    ("/api/objectives", &["generated_at_ms"]),
    ("/api/approvals", &["generated_at_ms"]),
    ("/api/context/digest", &["generated_at_ms"]),
    ("/api/attention", &["generated_at_ms"]),
    ("/api/features", &["generated_at_ms"]),
    ("/api/objectives/pane", &["captured_at_ms"]),
];

/// Dynamic clock paths whose segment pattern has `*`.
const PATTERN_CLOCKS: [(&str, &[&str]); 1] = [("/api/sessions/*/pane", &["captured_at_ms"])];

fn bucket_i64(object: &mut serde_json::Map<String, Value>, key: &str, step: i64) {
    if let Some(value) = object.get(key).and_then(Value::as_i64) {
        object.insert(key.to_string(), Value::from(value.div_euclid(step)));
    }
}

/// Normalize volatile sub-minute / sub-MB host telemetry in `/api/fleet`'s
/// `health` object solely when computing the weak ETag, leaving the JSON body
/// untouched.
fn normalize_fleet_etag(root: &mut serde_json::Map<String, Value>) {
    let Some(health) = root.get_mut("health").and_then(Value::as_object_mut) else {
        return;
    };
    health.remove("generated_at_ms");
    health.remove("agent_hours_24h");
    bucket_i64(health, "agent_ms_24h", 60_000);
    if let Some(memory) = health.get_mut("memory").and_then(Value::as_object_mut) {
        bucket_i64(memory, "available_bytes", 32 * 1024 * 1024);
    }
    if let Some(storage) = health.get_mut("storage").and_then(Value::as_object_mut) {
        bucket_i64(storage, "free_bytes", 16 * 1024 * 1024);
    }
    if let Some(disks) = health.get_mut("disk").and_then(Value::as_array_mut) {
        for disk in disks {
            if let Some(disk_obj) = disk.as_object_mut() {
                bucket_i64(disk_obj, "free_bytes", 16 * 1024 * 1024);
            }
        }
    }
}

/// The weak ETag of an answer of `path`: a hash of its body (`body`, the
/// serialization of `value`) without its clocks.
fn etag(path: &str, value: &Value, body: &str) -> String {
    let fields = clocks(path);
    let mut hasher = DefaultHasher::new();
    let is_fleet = path == "/api/fleet";
    match value.as_object() {
        Some(object)
            if fields.iter().any(|field| object.contains_key(*field))
                || (is_fleet && object.contains_key("health")) =>
        {
            let mut without = object.clone();
            for field in fields {
                without.remove(*field);
            }
            if is_fleet {
                normalize_fleet_etag(&mut without);
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
/// open page polls, and the panes. `path` is canonical: `handle()` has
/// rewritten a legacy alias already. A session's commands and judgements,
/// its timeline, the log and a task's texts are read fresh every time (the
/// page's input box follows its commands).
pub(super) fn shared(path: &str) -> bool {
    matches!(
        path,
        "/api/overview"
            | "/api/attention"
            | "/api/fleet"
            | "/api/missions"
            | "/api/objectives"
            | "/api/objectives/pane"
            | "/api/features"
            | "/api/scorecard"
            | "/api/approvals"
            | "/api/context/digest"
            | "/api/alerts"
            | "/api/interventions"
            | "/api/core-lines"
            | "/api/nodes"
            | "/api/cost"
            | "/api/grading"
            | "/api/verifications"
            | "/api/capabilities"
    ) || fits("/api/sessions/*/pane", path)
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

/// Bounded cache of gzip-compressed payloads keyed by full SHA-256 of the
/// exact response bytes (so distinct `generated_at_ms` bodies never alias),
/// capped by entry count, single-entry compressed size, and total compressed bytes.
const GZIP_CACHE_MAX: usize = 24;
const GZIP_CACHE_MAX_ENTRY_BYTES: usize = 512 * 1024;
const GZIP_CACHE_MAX_TOTAL_BYTES: usize = 4 * 1024 * 1024;

struct GzipCache {
    max_entries: usize,
    max_entry_bytes: usize,
    max_total_bytes: usize,
    total_bytes: usize,
    order: VecDeque<[u8; 32]>,
    entries: HashMap<[u8; 32], Option<Arc<[u8]>>>,
}

impl GzipCache {
    fn new(max_entries: usize, max_entry_bytes: usize, max_total_bytes: usize) -> Self {
        Self {
            max_entries,
            max_entry_bytes,
            max_total_bytes,
            total_bytes: 0,
            order: VecDeque::new(),
            entries: HashMap::new(),
        }
    }

    fn entry_len(entry: Option<&Arc<[u8]>>) -> usize {
        entry.map_or(0, |bytes| bytes.len())
    }

    fn lookup(&self, digest: &[u8; 32]) -> GzipLookup {
        match self.entries.get(digest) {
            Some(entry) => GzipLookup::Hit(entry.clone()),
            None => GzipLookup::Miss,
        }
    }

    #[cfg(test)]
    fn get(&self, digest: &[u8; 32]) -> Option<Arc<[u8]>> {
        self.entries.get(digest).and_then(Clone::clone)
    }

    #[cfg(test)]
    fn contains(&self, digest: &[u8; 32]) -> bool {
        self.entries.contains_key(digest)
    }

    fn insert(&mut self, digest: [u8; 32], compressed: Option<Arc<[u8]>>) -> Option<Arc<[u8]>> {
        if let Some(hit) = self.entries.get(&digest) {
            return hit.clone();
        }
        let bytes_len = Self::entry_len(compressed.as_ref());
        if bytes_len > self.max_entry_bytes || bytes_len > self.max_total_bytes {
            return compressed;
        }
        while !self.order.is_empty()
            && (self.order.len() >= self.max_entries
                || self.total_bytes.saturating_add(bytes_len) > self.max_total_bytes)
        {
            if let Some(evicted) = self.order.pop_front()
                && let Some(removed) = self.entries.remove(&evicted)
            {
                self.total_bytes = self
                    .total_bytes
                    .saturating_sub(Self::entry_len(removed.as_ref()));
            }
        }
        if self.order.len() < self.max_entries
            && self.total_bytes.saturating_add(bytes_len) <= self.max_total_bytes
        {
            self.total_bytes += bytes_len;
            self.order.push_back(digest);
            self.entries.insert(digest, compressed.clone());
        }
        compressed
    }
}

enum GzipLookup {
    Hit(Option<Arc<[u8]>>),
    Miss,
}

static GZIP_CACHE: LazyLock<Mutex<GzipCache>> = LazyLock::new(|| {
    Mutex::new(GzipCache::new(
        GZIP_CACHE_MAX,
        GZIP_CACHE_MAX_ENTRY_BYTES,
        GZIP_CACHE_MAX_TOTAL_BYTES,
    ))
});

fn cached_gzip(body: &[u8]) -> Option<Arc<[u8]>> {
    if body.len() < gzip::MIN_COMPRESS_BYTES {
        return None;
    }
    let digest: [u8; 32] = Sha256::digest(body).into();
    if let GzipLookup::Hit(hit) = GZIP_CACHE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .lookup(&digest)
    {
        return hit;
    }
    let compressed = gzip::compress_if_smaller(body).map(Arc::<[u8]>::from);
    GZIP_CACHE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(digest, compressed)
}

/// `Cache-Control` of every API answer: the page asks again every time
/// (with its tag), and no cache in between keeps one.
const API_CACHE_CONTROL: &str = "no-cache";

/// Send `answer` to `request`: 304 with the tag alone when the request's
/// `If-None-Match` names it, else the answer (gzip-compressed when the client
/// advertises `Accept-Encoding: gzip` and compression shrinks the payload).
pub(super) fn respond(
    request: Request,
    answer: &Answer,
    if_none_match: Option<&str>,
) -> Result<(), String> {
    let accept_encoding = super::header(&request, "Accept-Encoding").map(str::to_string);
    let cache = Header::from_bytes("Cache-Control", API_CACHE_CONTROL).expect("static header");
    let vary = Header::from_bytes("Vary", "Accept-Encoding").expect("static header");
    let tag = answer
        .etag
        .as_deref()
        .map(|etag| Header::from_bytes("ETag", etag).expect("a hex tag is a valid header"));
    let unchanged = answer
        .etag
        .as_deref()
        .is_some_and(|etag| not_modified(if_none_match, etag));
    let mut response = match tag {
        Some(tag) if unchanged => Response::empty(304)
            .with_header(tag)
            .with_header(cache)
            .with_header(vary)
            .boxed(),
        tag => {
            let kind = Header::from_bytes("Content-Type", "application/json; charset=utf-8")
                .expect("static header");
            let mut response = if gzip::accepts_gzip(accept_encoding.as_deref())
                && let Some(compressed) = cached_gzip(answer.body.as_bytes())
            {
                Response::from_data(compressed.as_ref())
                    .with_status_code(answer.status)
                    .with_header(kind)
                    .with_header(cache)
                    .with_header(
                        Header::from_bytes("Content-Encoding", "gzip").expect("static header"),
                    )
                    .with_header(vary)
            } else {
                Response::from_string(answer.body.clone())
                    .with_status_code(answer.status)
                    .with_header(kind)
                    .with_header(cache)
                    .with_header(vary)
            };
            if let Some(tag) = tag {
                response.add_header(tag);
            }
            response.boxed()
        }
    };
    if let Some(link) = super::successor_link() {
        response.add_header(link);
    }
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
/// `/api/tasks/<n>`.
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

/// Fields kept on a task's `last_gate` object in `/api/missions?summary=1`.
const SUMMARY_GATE_FIELDS: &[&str] = &["verdict", "reason", "metric", "candidate", "at_ms"];

/// Fields kept on each entry of a task's `stages` array in `/api/missions?summary=1`.
const SUMMARY_STAGE_FIELDS: &[&str] = &["stage", "verdict", "at_ms", "took_ms"];

fn pick_fields(value: &mut Value, allowed: &[&str]) {
    if let Some(obj) = value.as_object_mut() {
        obj.retain(|key, _| allowed.contains(&key.as_str()));
    }
}

/// Summary view of `/api/missions` for `/api/missions?summary=1`: keeps live
/// mission/task status, counters, halt context, and light stage/gate summaries
/// needed by Home, Fleet, and Sidebar while stripping full text excerpts and
/// closed-mission task details to keep polling payloads small.
pub(super) fn summary_board(board: &mut Value) {
    let Some(missions) = board.get_mut("missions").and_then(Value::as_array_mut) else {
        return;
    };
    for mission in missions {
        let folded = closed(mission);
        let Some(tasks) = mission.get_mut("tasks").and_then(Value::as_array_mut) else {
            continue;
        };
        for task in tasks {
            trim_texts(
                task,
                &["brief", "review_note", "report"],
                EXCERPT_LINES,
                EXCERPT_BYTES,
                false,
            );
            let Some(obj) = task.as_object_mut() else {
                continue;
            };
            if folded {
                if obj.contains_key("stages") {
                    obj.insert("stages".to_string(), Value::Array(Vec::new()));
                }
                if obj.contains_key("last_gate") {
                    obj.insert("last_gate".to_string(), Value::Null);
                }
                if obj.contains_key("activity") {
                    obj.insert("activity".to_string(), Value::Null);
                }
                continue;
            }
            if let Some(last_gate) = obj.get_mut("last_gate") {
                pick_fields(last_gate, SUMMARY_GATE_FIELDS);
            }
            if let Some(stages) = obj.get_mut("stages").and_then(Value::as_array_mut) {
                for stage in stages {
                    pick_fields(stage, SUMMARY_STAGE_FIELDS);
                }
            }
        }
    }
}

/// Lines of an objective's prompt `/api/objectives` sends: more than the four its
/// card shows. Pointed at, the page reads the whole prompt
/// (`/api/objectives/<mission>/<id>`) for its title.
const PROMPT_LINES: usize = 10;

/// Bytes of an objective's prompt `/api/objectives` sends.
const PROMPT_BYTES: usize = 1024;

/// `/api/objectives`'s missions as the page shows them: each objective's prompt
/// cut to its first lines (whole at `/api/objectives/<mission>/<id>`),
/// and each task's brief and review note as their lengths alone (no list
/// of the page shows them; the task in full is `/api/tasks/<id>/gate`).
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
                    false,
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
        Answer, Answers, GZIP_CACHE_MAX_ENTRY_BYTES, GZIP_CACHE_MAX_TOTAL_BYTES, GzipCache,
        SHARE_FOR, SWEEP_PAST, accepted_age, answer, cached_gzip, excerpt, file_cache_control, key,
        not_modified, shared, slim_board, slim_rsi, summary_board, trim_texts,
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
            "/api/objectives",
            Ok(json!({"generated_at_ms": 1, "missions": []})),
            "",
        );
        let late = answer(
            "/api/objectives",
            Ok(json!({"generated_at_ms": 2, "missions": []})),
            "",
        );
        assert_eq!(early.etag, late.etag, "the same answer, read later");
        assert!(late.body.contains("\"generated_at_ms\":2"));
        let changed = answer(
            "/api/objectives",
            Ok(json!({"generated_at_ms": 2, "missions": [1]})),
            "",
        );
        assert_ne!(early.etag, changed.etag);
        // The feature map's clock is its own too: deleting the `/api/features`
        // row of CLOCKS makes the page poll a fresh tag every four seconds.
        let features = |at: i64, missions: serde_json::Value| {
            answer(
                "/api/features",
                Ok(json!({"generated_at_ms": at, "missions": missions})),
                "",
            )
        };
        let first = features(1, json!([]));
        let later = features(2, json!([]));
        assert_eq!(first.etag, later.etag, "the map, read later");
        assert!(later.body.contains("\"generated_at_ms\":2"));
        assert_ne!(first.etag, features(2, json!([1])).etag);
        // Panes, /api/missions, and /api/fleet also leave their clocks and sub-MB
        // telemetry jitter out of the tag while keeping exact values in the body.
        let pane = |at: i64| {
            answer(
                "/api/sessions/session_a/pane",
                Ok(json!({"captured_at_ms": at, "text": "x"})),
                "",
            )
            .etag
        };
        assert_eq!(pane(1), pane(2));

        let missions_early = answer(
            "/api/missions",
            Ok(json!({"generated_at_ms": 1, "missions": []})),
            "",
        );
        let missions_late = answer(
            "/api/missions",
            Ok(json!({"generated_at_ms": 9999, "missions": []})),
            "",
        );
        assert_eq!(missions_early.etag, missions_late.etag);
        assert!(missions_late.body.contains("\"generated_at_ms\":9999"));
        assert_ne!(
            missions_early.etag,
            answer(
                "/api/missions",
                Ok(json!({"generated_at_ms": 9999, "missions": [1]})),
                "",
            )
            .etag
        );

        let fleet_payload = |at: i64,
                             free_bytes: i64,
                             avail_bytes: i64,
                             agent_ms: i64,
                             agents: serde_json::Value| {
            answer(
                "/api/fleet",
                Ok(json!({
                    "generated_at_ms": at,
                    "agents": agents,
                    "health": {
                        "generated_at_ms": at,
                        "agent_ms_24h": agent_ms,
                        "agent_hours_24h": 1.0 + f64::from(i32::try_from(agent_ms).expect("ms")) / 3_600_000.0,
                        "memory": { "available_bytes": avail_bytes },
                        "storage": { "free_bytes": free_bytes },
                        "disk": [{ "free_bytes": free_bytes }]
                    }
                })),
                "",
            )
        };
        let f1 = fleet_payload(
            1_000,
            44_206_051_328,
            2_139_088_007_168,
            37_413_512,
            json!([]),
        );
        let f2 = fleet_payload(
            5_000,
            44_205_912_064,
            2_139_076_714_496,
            37_411_300,
            json!([]),
        );
        assert_eq!(
            f1.etag, f2.etag,
            "sub-MB / sub-minute fleet jitter keeps ETag"
        );
        assert!(f2.body.contains("\"generated_at_ms\":5000"));
        assert!(f2.body.contains("44205912064"));
        let f_changed = fleet_payload(
            5_000,
            44_205_912_064,
            2_139_076_714_496,
            37_411_300,
            json!([{"id": 1}]),
        );
        assert_ne!(f1.etag, f_changed.etag, "agent change updates fleet ETag");
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
            "/api/objectives",
            "/api/objectives/pane",
            "/api/features",
            "/api/scorecard",
            "/api/approvals",
            "/api/context/digest",
            "/api/alerts",
            "/api/interventions",
            "/api/core-lines",
            "/api/nodes",
            "/api/cost",
            "/api/grading",
            "/api/verifications",
            "/api/capabilities",
            "/api/sessions/session_a/pane",
        ] {
            assert!(shared(path), "{path}");
        }
        for path in [
            "/api/sessions/session_a/commands",
            "/api/sessions/session_a/judgements",
            "/api/sessions/session_a",
            "/api/events",
            "/api/tasks/3",
            "/api/tasks/3/gate",
            "/api/sessions//pane",
            "/api/sessions/a/b/pane",
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
    fn the_rsi_page_gets_its_prompts_cut_and_its_tasks_texts_as_lengths() {
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
        assert_eq!(missions[0]["tasks"][0]["brief"], json!(null));
        assert_eq!(missions[0]["tasks"][0]["brief_bytes"], 40);
        assert_eq!(missions[0]["tasks"][0]["review_note"], json!(null));
        assert_eq!(missions[0]["tasks"][0]["review_note_bytes"], 2);
    }

    #[test]
    fn built_assets_are_cached_for_good_and_the_page_is_asked_again() {
        assert_eq!(
            file_cache_control("assets/index-cf885A5y.js"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(file_cache_control("index.html"), "no-cache");
    }

    #[test]
    fn respond_negotiates_gzip_when_accepted_and_preserves_304() {
        use std::io::{Read, Write};
        use std::net::TcpStream;

        let items = vec!["\"ok\""; 300].join(",");
        let large_body = format!(r#"{{"missions":[{items}]}}"#);
        let ans = answer(
            "/api/missions",
            Ok(serde_json::from_str(&large_body).expect("json")),
            "",
        );
        let etag = ans.etag.clone().expect("etag");

        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind test server");
        let port = server.server_addr().to_ip().expect("ip").port();
        let ans_clone = ans.clone();
        let handle = std::thread::spawn(move || {
            for _ in 0..3 {
                let req = server.recv().expect("recv");
                let inm = super::super::header(&req, "If-None-Match").map(str::to_string);
                super::respond(req, &ans_clone, inm.as_deref()).expect("respond");
            }
        });

        let send_raw = |raw_req: &str| -> Vec<u8> {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
            stream.write_all(raw_req.as_bytes()).expect("write");
            stream
                .shutdown(std::net::Shutdown::Write)
                .expect("shutdown");
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).expect("read");
            buf
        };

        // 1. Accepts gzip -> Content-Encoding: gzip + Vary: Accept-Encoding + compressed body
        let raw_gz = send_raw(
            "GET /api/missions HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nAccept-Encoding: gzip\r\n\r\n",
        );
        let split = raw_gz
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("http headers");
        let hdrs = String::from_utf8_lossy(&raw_gz[..split]).to_ascii_lowercase();
        let gz_body = &raw_gz[split + 4..];
        assert!(hdrs.starts_with("http/1.1 200"));
        assert!(hdrs.contains("content-encoding: gzip"));
        assert!(hdrs.contains("vary: accept-encoding"));
        assert!(gz_body.len() < ans.body.len());

        // 2. No Accept-Encoding -> uncompressed plain JSON + Vary: Accept-Encoding
        let raw_plain =
            send_raw("GET /api/missions HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
        let split2 = raw_plain
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("http headers");
        let hdrs2 = String::from_utf8_lossy(&raw_plain[..split2]).to_ascii_lowercase();
        let plain_body = &raw_plain[split2 + 4..];
        assert!(hdrs2.starts_with("http/1.1 200"));
        assert!(!hdrs2.contains("content-encoding: gzip"));
        assert!(hdrs2.contains("vary: accept-encoding"));
        assert_eq!(plain_body, ans.body.as_bytes());

        // 3. Matching If-None-Match -> 304 Not Modified + Vary: Accept-Encoding
        let raw_304 = send_raw(&format!(
            "GET /api/missions HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nAccept-Encoding: gzip\r\nIf-None-Match: {etag}\r\n\r\n"
        ));
        let split3 = raw_304
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("http headers");
        let hdrs3 = String::from_utf8_lossy(&raw_304[..split3]).to_ascii_lowercase();
        assert!(hdrs3.starts_with("http/1.1 304"));
        assert!(hdrs3.contains("vary: accept-encoding"));
        assert_eq!(&raw_304[split3 + 4..], b"");

        handle.join().expect("server thread");
    }

    #[test]
    fn gzip_cache_enforces_entry_and_total_byte_budgets_with_fifo_eviction() {
        assert_eq!(GZIP_CACHE_MAX_ENTRY_BYTES, 512 * 1024);
        assert_eq!(GZIP_CACHE_MAX_TOTAL_BYTES, 4 * 1024 * 1024);

        let mut cache = GzipCache::new(4, 16, 24);
        let d1 = [1u8; 32];
        let d2 = [2u8; 32];
        let d3 = [3u8; 32];
        let d_huge = [4u8; 32];

        cache.insert(d1, Some(std::sync::Arc::from(vec![b'a'; 10])));
        cache.insert(d2, Some(std::sync::Arc::from(vec![b'b'; 10])));
        assert_eq!(cache.total_bytes, 20);
        assert!(cache.get(&d1).is_some());

        // Oversized single entry returns payload without entering cache or evicting existing entries.
        let bypassed = cache.insert(d_huge, Some(std::sync::Arc::from(vec![b'x'; 17])));
        assert_eq!(bypassed.as_ref().map(|b| b.len()), Some(17));
        assert!(!cache.contains(&d_huge));
        assert_eq!(cache.total_bytes, 20);

        // Inserting d3 (10 bytes) pushes total_bytes to 30 > 24, so d1 is FIFO-evicted.
        cache.insert(d3, Some(std::sync::Arc::from(vec![b'c'; 10])));
        assert!(!cache.contains(&d1));
        assert!(cache.contains(&d2));
        assert!(cache.contains(&d3));
        assert!(cache.get(&d3).is_some());
        assert_eq!(cache.total_bytes, 20);

        // Repeated >512B compressible payload gets compressed via global cached_gzip.
        let large = "hello-spawn-gzip-".repeat(60);
        let first = cached_gzip(large.as_bytes()).expect("compressed");
        let second = cached_gzip(large.as_bytes()).expect("cached");
        assert_eq!(first.as_ref(), second.as_ref());
    }

    #[test]
    fn summary_board_strips_closed_mission_details_and_keeps_light_open_summaries() {
        let mut board = json!({
            "missions": [
                {
                    "id": 10,
                    "state": "active",
                    "halted": null,
                    "tasks": [
                        {
                            "id": 101,
                            "state": "delivered",
                            "brief": "full brief text",
                            "report": "full report text",
                            "review_note": "note",
                            "activity": {"idle_ms": 120},
                            "last_gate": {
                                "verdict": "accepted",
                                "reason": "ok",
                                "metric": 1.0,
                                "candidate": "abc",
                                "at_ms": 500,
                                "stdout": "very bulky logs"
                            },
                            "stages": [
                                {"stage": "gate", "verdict": "pass", "at_ms": 500, "took_ms": 42, "details": "huge"}
                            ]
                        }
                    ]
                },
                {
                    "id": 9,
                    "state": "done",
                    "halted": null,
                    "tasks": [
                        {
                            "id": 99,
                            "state": "landed",
                            "brief": "closed brief",
                            "report": "closed report",
                            "review_note": "closed note",
                            "activity": {"idle_ms": 999},
                            "last_gate": {"verdict": "accepted", "reason": "ok", "at_ms": 100},
                            "stages": [{"stage": "gate", "verdict": "pass", "at_ms": 100, "took_ms": 10}]
                        }
                    ]
                }
            ]
        });

        summary_board(&mut board);
        let open_task = &board["missions"][0]["tasks"][0];
        assert_eq!(open_task["brief"], json!(null));
        assert_eq!(open_task["brief_bytes"], 15);
        assert_eq!(open_task["report"], json!(null));
        assert_eq!(open_task["report_bytes"], 16);
        assert_eq!(open_task["review_note"], json!(null));
        assert_eq!(open_task["review_note_bytes"], 4);
        assert_eq!(open_task["activity"], json!({"idle_ms": 120}));
        assert_eq!(
            open_task["last_gate"],
            json!({"verdict": "accepted", "reason": "ok", "metric": 1.0, "candidate": "abc", "at_ms": 500})
        );
        assert_eq!(
            open_task["stages"],
            json!([{"stage": "gate", "verdict": "pass", "at_ms": 500, "took_ms": 42}])
        );

        let closed_task = &board["missions"][1]["tasks"][0];
        assert_eq!(closed_task["brief"], json!(null));
        assert_eq!(closed_task["brief_bytes"], 12);
        assert_eq!(closed_task["report"], json!(null));
        assert_eq!(closed_task["last_gate"], json!(null));
        assert_eq!(closed_task["activity"], json!(null));
        assert_eq!(closed_task["stages"], json!([]));
    }
}
