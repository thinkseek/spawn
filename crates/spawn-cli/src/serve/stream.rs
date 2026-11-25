//! `GET /api/stream`: the log as it is written, as Server-Sent Events
//! (issue #91 P1a), so a page follows the fleet without polling.
//!
//! - Its own slots. A stream lives as long as its page, so it never holds
//!   one of the requests' [`super::MAX_IN_FLIGHT`] slots: `serve` hands a
//!   `GET /api/stream` one of [`SSE_SLOTS`] instead, and answers 503 when
//!   those are all taken ([`is_stream`], `serve_requests`). One client
//!   address that is not loopback holds at most [`STREAMS_PER_PEER`] of
//!   them (503 `too many streams from this address` past that), so a far
//!   client cannot take them all; loopback clients count only toward
//!   [`SSE_SLOTS`]. Every other request is served exactly as before,
//!   however many pages stream.
//! - One listener. A single background thread ([`Hub::start`]) holds `LISTEN
//!   spawn_events` on a connection of the read-only `spawn_reader` login
//!   (`open_store`'s), reads each new event once through the reader's
//!   `log_after` and fans it out to every stream. A notification is only a
//!   hint: the thread also reads every [`POLL`] while anyone streams, from
//!   its own cursor, so nothing a lost notification would have announced is
//!   missed. A connection that fails is made again after a pause that
//!   doubles from [`BACKOFF_MIN`] to [`BACKOFF_MAX`]; the events written
//!   meanwhile are read from the cursor when it is back (at most
//!   [`REPLAY_MAX`]; past that every stream is told `resync`).
//! - Bounded fan-out. Each stream has a channel of [`CLIENT_BUFFER`]
//!   frames; the listener only ever `try_send`s, so a slow or dead page
//!   never stalls it, nor PostgreSQL: a stream whose channel is full is
//!   dropped, told `resync` and closed (the page reconnects with its
//!   cursor).
//! - The read guard is every API read's (`access`: the bearer header or
//!   `?token=`, `SPAWN_SERVE_LOCAL_OPEN`, `SPAWN_SERVE_OPEN_NETWORKS`), and
//!   only GET is taken (405 otherwise).
//! - Catch-up. `Last-Event-ID` (the browser's own reconnect), else
//!   `?after=<id>` (the cursor `/api/events` and `/api/sessions/<id>` take;
//!   `?after_event_id=<id>` is its alias, both at once a 400, as is a value
//!   that is no whole number at least 0) replays the events after that id, at most
//!   [`REPLAY_MAX`], before the live ones, none twice; a cursor further
//!   behind than that (or one the log never reached: a reset log) is told
//!   `event: resync` and the stream goes on live from now. Without a cursor
//!   it starts live. Every `resync` is `{"reason", "after_event_id",
//!   "latest_event_id"}` ([`resync`]): `reason` `behind` (past
//!   [`REPLAY_MAX`]), `cursor_ahead` (past the log's end), `stream_reset`
//!   (the listener fell more than [`REPLAY_MAX`] behind, a long reconnect)
//!   or `slow_consumer` (its channel overflowed; `latest_event_id` null),
//!   `after_event_id` the cursor it could not go on from.
//! - No event after a catch-up is skipped: each stream hands the listener
//!   the id it caught up to, its own floor ([`Subscription::floor`]), and a
//!   listener with no cursor (nobody streamed) starts, once every stream
//!   there is has handed its floor, from the lowest ([`Hub::start_floor`]),
//!   never from the log's end, which may already hold a stream's first
//!   live event. A floor leaves with its stream: one that connected and
//!   dropped moves no later stream's start.
//! - A page that stops reading ends its stream after [`WRITE_TIMEOUT`]
//!   ([`TimedWriter`]); a page that is gone (closed, reset: `SO_LINGER=0`)
//!   is found by the next write, and a quiet stream writes a 3-byte comment
//!   probe `:` every [`PROBE`] (500 ms: few wakeups for a phone's PWA, and
//!   well inside the page's `retry: 3000`), so a gone page frees its slot,
//!   its writer and its socket within about a second, never after a whole
//!   [`KEEPALIVE`]
//!   (N252: 64 reset streams held every slot ~16 s, and the pages'
//!   3-second reconnects found 503). The probe reads nothing: the database
//!   is not asked. A stream that ends is pruned from the hub at once;
//!   a listener whose thread ends (a panic included) is started again by
//!   the next stream.
//! - Frames: first `retry: 3000` and `event: ready` (`{"latest_event_id",
//!   "replayed"}`), then one per event, `id: <event id>\nevent: <its
//!   name>\ndata: <json>\n\n`, the JSON what `/api/events` lists for it
//!   (`{id, agent_id, session_id, execution_id, source, event_name, detail,
//!   recorded_at_ms}`), the probe comment `:` every [`PROBE`] with nothing
//!   to say, and the comment `: keepalive` in its place every
//!   [`KEEPALIVE`] of quiet.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use spawn_core::event::GlobalPosition;
use spawn_postgres::PostgresStore;
use tiny_http::{Method, Request};

use super::{BAD_REQUEST, json_response, not_allowed, parameter};

/// Streams served at once, beside the requests' own slots.
pub(super) const SSE_SLOTS: usize = 64;
/// The most events one catch-up replays (and the listener reads after a
/// reconnect) before it says `resync` instead.
pub(super) const REPLAY_MAX: usize = 500;
/// The streams one client address that is not loopback holds at once (as
/// `nodes/relay.rs`'s `PER_PEER`).
pub(super) const STREAMS_PER_PEER: usize = 16;
/// Between two keepalive comments of a quiet stream.
pub(super) const KEEPALIVE: Duration = Duration::from_secs(15);
/// Between two probe comments (`:`) of a quiet stream: each is a write,
/// which fails once the page is gone, so the stream ends and frees its slot
/// (module doc).
pub(super) const PROBE: Duration = Duration::from_millis(500);
/// A probe on the wire: a comment line and a blank line, which a browser's
/// `EventSource` reads as nothing.
pub(super) const PROBE_FRAME: &str = ":\n\n";
/// The frames a stream's channel holds before the stream is dropped.
pub(super) const CLIENT_BUFFER: usize = 256;
/// The listener's read of the log while anyone streams, notified or not.
const POLL: Duration = Duration::from_secs(1);
/// The longest one write to a page may take: a page that stops reading
/// (its socket buffer full) ends its stream after this, never holding a
/// stream slot until the kernel gives up on the connection.
pub(super) const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// The listener's pauses after a failed connection.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Whether a request is a stream's (it takes one of [`SSE_SLOTS`]).
pub(super) fn is_stream(request: &Request) -> bool {
    let url = request.url();
    url.split_once('?').map_or(url, |(path, _)| path) == "/api/stream"
}

/// One SSE frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Frame {
    pub(super) id: Option<i64>,
    pub(super) event: String,
    pub(super) data: String,
}

impl Frame {
    /// An event of the log as `/api/events` lists it.
    fn of(entry: &spawn_postgres::LogEntry) -> Self {
        let id = i64::try_from(entry.position.0).unwrap_or(i64::MAX);
        Self {
            id: Some(id),
            event: sse_word(&entry.event_name),
            data: json!({
                "id": entry.position.0,
                "agent_id": entry.agent_id,
                "session_id": entry.session_id,
                "execution_id": entry.execution_id,
                "source": entry.source,
                "event_name": entry.event_name,
                "detail": entry.detail,
                "recorded_at_ms": entry.recorded_at.0,
            })
            .to_string(),
        }
    }

    /// A frame of Spawn's own (`ready`, `resync`), with no id.
    pub(super) fn note(event: &str, data: &Value) -> Self {
        Self {
            id: None,
            event: event.to_string(),
            data: data.to_string(),
        }
    }

    /// As the wire carries it. `data` is one line of JSON (serde never
    /// writes a raw line end), so one `data:` field holds it.
    pub(super) fn render(&self) -> String {
        let id = self.id.map_or_else(String::new, |id| format!("id: {id}\n"));
        format!("{id}event: {}\ndata: {}\n\n", self.event, self.data)
    }
}

/// An event name as an SSE `event:` field: no line end or control
/// character can end the field early (a name is the engine's or Spawn's,
/// never a person's, but the wire is not trusted to that).
fn sse_word(name: &str) -> String {
    let word: String = name
        .chars()
        .filter(|character| !character.is_control())
        .take(128)
        .collect();
    if word.is_empty() {
        "message".to_string()
    } else {
        word
    }
}

/// A stream's floor not handed yet (its catch-up not read).
const NO_FLOOR: i64 = -1;

/// One stream's place in the hub: its channel's sending end, whether the
/// stream still reads it (its [`Subscription`] not dropped), and the id its
/// catch-up reached ([`Subscription::floor`]; [`NO_FLOOR`] until then).
struct Subscriber {
    send: SyncSender<Arc<Frame>>,
    alive: Arc<AtomicBool>,
    floor: Arc<AtomicI64>,
}

/// A stream's end of its channel. Dropping it (the stream ended, however
/// it ended) marks it gone at once, so the hub prunes it at its next look
/// rather than at its next frame.
pub(super) struct Subscription {
    receive: Receiver<Arc<Frame>>,
    alive: Arc<AtomicBool>,
    floor: Arc<AtomicI64>,
}

impl Subscription {
    /// This stream caught up to `sent`: a listener with no cursor starts
    /// no later than the lowest floor of the streams there are, so no
    /// event after this stream's catch-up is skipped. The floor is this
    /// stream's own and leaves the hub with it: a stream gone before the
    /// listener read its floor moves no one's start.
    pub(super) fn floor(&self, sent: i64) {
        self.floor.store(sent.max(0), Ordering::Release);
    }

    pub(super) fn recv_timeout(&self, timeout: Duration) -> Result<Arc<Frame>, RecvTimeoutError> {
        self.receive.recv_timeout(timeout)
    }

    #[cfg(test)]
    pub(super) fn try_iter(&self) -> std::sync::mpsc::TryIter<'_, Arc<Frame>> {
        self.receive.try_iter()
    }

    #[cfg(test)]
    pub(super) fn recv(&self) -> Result<Arc<Frame>, std::sync::mpsc::RecvError> {
        self.receive.recv()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
    }
}

/// The streams' fan-out: each stream's channel and floor, and the
/// listener's state.
pub(super) struct Hub {
    subscribers: Mutex<Vec<Subscriber>>,
    started: AtomicBool,
    buffer: usize,
}

/// The server's one hub, its listener started by the first stream.
static HUB: LazyLock<Hub> = LazyLock::new(|| Hub::new(CLIENT_BUFFER));

/// Clears the hub's `started` when the listener's thread ends, a panic
/// included, so the next stream starts it again.
struct Started<'a>(&'a AtomicBool);

impl Drop for Started<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Hub {
    pub(super) fn new(buffer: usize) -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
            started: AtomicBool::new(false),
            buffer,
        }
    }

    /// A new stream's channel.
    pub(super) fn subscribe(&self) -> Subscription {
        let (send, receive) = std::sync::mpsc::sync_channel(self.buffer);
        let alive = Arc::new(AtomicBool::new(true));
        let floor = Arc::new(AtomicI64::new(NO_FLOOR));
        self.subscribers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Subscriber {
                send,
                alive: Arc::clone(&alive),
                floor: Arc::clone(&floor),
            });
        Subscription {
            receive,
            alive,
            floor,
        }
    }

    /// Where a listener with no cursor starts: the lowest floor of the
    /// streams there are (those gone pruned first), once every one of them
    /// has handed its floor; `None` while one has not (its catch-up is
    /// still read: starting without it could skip an event it did not
    /// read) or nobody streams.
    pub(super) fn start_floor(&self) -> Option<i64> {
        let mut subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        subscribers.retain(|subscriber| subscriber.alive.load(Ordering::Acquire));
        let floors: Vec<i64> = subscribers
            .iter()
            .map(|subscriber| subscriber.floor.load(Ordering::Acquire))
            .collect();
        if floors.contains(&NO_FLOOR) {
            None
        } else {
            floors.into_iter().min()
        }
    }

    /// The streams subscribed now, those gone pruned first.
    pub(super) fn subscribers(&self) -> usize {
        let mut subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        subscribers.retain(|subscriber| subscriber.alive.load(Ordering::Acquire));
        subscribers.len()
    }

    /// Hand `frame` to every stream without waiting: one whose channel is
    /// full (a slow page) or closed (a page gone) is dropped.
    pub(super) fn publish(&self, frame: &Arc<Frame>) {
        self.subscribers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|subscriber| {
                subscriber.alive.load(Ordering::Acquire)
                    && match subscriber.send.try_send(Arc::clone(frame)) {
                        Ok(()) => true,
                        Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
                    }
            });
    }

    /// Start the listener once, its connections made by `open`; started
    /// again by a later stream if its thread ever ends.
    pub(super) fn start(
        &'static self,
        open: impl Fn() -> Result<PostgresStore, String> + Send + 'static,
    ) {
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let spawned = std::thread::Builder::new()
            .name("serve-stream-listen".to_string())
            .spawn(move || {
                let _started = Started(&self.started);
                listen(self, &open, &|| false);
            });
        if let Err(error) = spawned {
            eprintln!("serve: the stream's listener did not start: {error}");
            self.started.store(false, Ordering::Release);
        }
    }

    #[cfg(test)]
    pub(super) fn started(&self) -> bool {
        self.started.load(Ordering::Acquire)
    }
}

/// A `resync` frame: always `{reason, after_event_id, latest_event_id}`,
/// `latest_event_id` null where it was not read (module doc).
pub(super) fn resync(reason: &str, after_event_id: i64, latest_event_id: Option<i64>) -> Frame {
    Frame::note(
        "resync",
        &json!({"reason": reason, "after_event_id": after_event_id,
                "latest_event_id": latest_event_id}),
    )
}

/// The latest event's id, through the reader (`position_before` the end
/// of time: the last position).
fn latest(store: &PostgresStore) -> Result<i64, String> {
    store
        .position_before(spawn_core::time::Timestamp(i64::MAX))
        .map(|position| i64::try_from(position.0).unwrap_or(i64::MAX))
        .map_err(|error| error.to_string())
}

/// Up to `limit` events after `after`.
fn after(store: &PostgresStore, after: i64, limit: usize) -> Result<Vec<Arc<Frame>>, String> {
    store
        .log_after(GlobalPosition(after.unsigned_abs()), limit)
        .map(|entries| {
            entries
                .iter()
                .map(|entry| Arc::new(Frame::of(entry)))
                .collect()
        })
        .map_err(|error| error.to_string())
}

/// The listener (module doc): until `stop` says so (never, in `serve`).
pub(super) fn listen(
    hub: &Hub,
    open: &dyn Fn() -> Result<PostgresStore, String>,
    stop: &dyn Fn() -> bool,
) {
    let mut cursor: Option<i64> = None;
    let mut backoff = BACKOFF_MIN;
    let mut said: Option<String> = None;
    while !stop() {
        let failed = (|| -> Result<(), String> {
            let store = open()?;
            let mut listener = store.listen().map_err(|error| error.to_string())?;
            (backoff, said) = (BACKOFF_MIN, None);
            while !stop() {
                if hub.subscribers() == 0 {
                    // Nobody streams: nothing is read. The next stream
                    // catches up itself and hands the listener its cursor.
                    cursor = None;
                    listener.wait(POLL).map_err(|error| error.to_string())?;
                    continue;
                }
                let Some(from) = cursor.or_else(|| hub.start_floor()) else {
                    // A stream subscribed but not caught up yet: its floor
                    // comes with its catch-up.
                    listener
                        .wait(Duration::from_millis(50))
                        .map_err(|error| error.to_string())?;
                    continue;
                };
                cursor = Some(from);
                let page = after(&store, from, REPLAY_MAX + 1)?;
                if page.len() > REPLAY_MAX {
                    // Further behind than a catch-up replays (a long
                    // reconnect): every stream resyncs from now.
                    let now = latest(&store)?;
                    hub.publish(&Arc::new(resync("stream_reset", from, Some(now))));
                    cursor = Some(now);
                    continue;
                }
                for frame in &page {
                    hub.publish(frame);
                    cursor = frame.id.or(cursor);
                }
                if page.is_empty() {
                    listener.wait(POLL).map_err(|error| error.to_string())?;
                }
            }
            Ok(())
        })();
        if let Err(error) = failed {
            if said.as_ref() != Some(&error) {
                eprintln!("serve: the stream's listener: {error}; connecting again");
            }
            said = Some(error);
            let until = Instant::now() + backoff;
            while Instant::now() < until && !stop() {
                std::thread::sleep(Duration::from_millis(50));
            }
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }
}

/// The cursor a stream asks to catch up from: `Last-Event-ID` (the
/// browser's reconnect, newer than its URL's), else `?after=` as
/// `/api/events` and `/api/sessions/<id>` take it, `?after_event_id=` its
/// alias. Both query names at once, or a value that is no whole number at
/// least 0, is the caller's mistake (400).
pub(super) fn cursor(last_event_id: Option<&str>, query: &str) -> Result<Option<i64>, String> {
    let (after, alias) = (
        parameter(query, "after"),
        parameter(query, "after_event_id"),
    );
    if after.is_some() && alias.is_some() {
        return Err(format!(
            "{BAD_REQUEST}give the cursor as ?after= or ?after_event_id=, not both"
        ));
    }
    let named = after.or(alias);
    if let Some(text) = named
        && text.parse::<i64>().map_or(true, |id| id < 0)
    {
        return Err(format!("{BAD_REQUEST}bad event id {text:?}"));
    }
    let given = last_event_id
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .or(named);
    given
        .map(|text| {
            text.parse::<i64>()
                .ok()
                .filter(|id| *id >= 0)
                .ok_or_else(|| format!("{BAD_REQUEST}bad event id {text:?}"))
        })
        .transpose()
}

/// `GET /api/stream` (module doc), past the slots and the read guard
/// (`handle` refuses a stream exactly as any API read).
pub(super) fn stream(request: Request, query: &str) -> Result<(), String> {
    // A stream refused before it subscribes (405, 400) starts no listener.
    if *request.method() == Method::Get
        && cursor(super::header(&request, "Last-Event-ID"), query).is_ok()
    {
        HUB.start(super::open_store);
    }
    serve_stream(request, query, &HUB, &super::open_store, KEEPALIVE)
}

/// A stream on `hub`, its catch-up read through `open`.
pub(super) fn serve_stream(
    request: Request,
    query: &str,
    hub: &Hub,
    open: &dyn Fn() -> Result<PostgresStore, String>,
    keepalive: Duration,
) -> Result<(), String> {
    if *request.method() != Method::Get {
        return request
            .respond(not_allowed("GET"))
            .map_err(|error| error.to_string());
    }
    let wanted = match cursor(super::header(&request, "Last-Event-ID"), query) {
        Ok(wanted) => wanted,
        Err(error) => {
            return request
                .respond(json_response(400, &json!({ "error": error })))
                .map_err(|error| error.to_string());
        }
    };
    // Subscribed before the catch-up is read, so no event falls between
    // the two; one read in both is sent once (`sent`).
    let live = hub.subscribe();
    let store = match open() {
        Ok(store) => store,
        Err(error) => {
            return request
                .respond(json_response(500, &json!({ "error": error })))
                .map_err(|error| error.to_string());
        }
    };
    let (first, mut sent) = match catch_up(&store, wanted) {
        Ok(caught) => caught,
        Err(error) => {
            return request
                .respond(json_response(500, &json!({ "error": error })))
                .map_err(|error| error.to_string());
        }
    };
    drop(store);
    // The listener, if it has no cursor, starts no later than this.
    live.floor(sent);
    let mut writer = TimedWriter::new(request.into_writer(), WRITE_TIMEOUT);
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\n\
                Cache-Control: no-cache, no-store\r\nConnection: close\r\n\
                X-Accel-Buffering: no\r\n\r\nretry: 3000\n\n";
    if writer.emit(head).is_err() {
        return Ok(());
    }
    for frame in &first {
        if writer.emit(&frame.render()).is_err() {
            return Ok(());
        }
    }
    // A quiet stream probes every PROBE (never less often than its
    // keepalive), and says `: keepalive` once `keepalive` has passed
    // since its last frame or keepalive.
    let probe = PROBE.min(keepalive);
    let mut said = Instant::now();
    loop {
        match live.recv_timeout(probe) {
            Ok(frame) => {
                if frame.id.is_some_and(|id| id <= sent) {
                    continue;
                }
                sent = frame.id.unwrap_or(sent);
                if writer.emit(&frame.render()).is_err() {
                    return Ok(());
                }
                said = Instant::now();
            }
            Err(RecvTimeoutError::Timeout) => {
                let text = if said.elapsed() >= keepalive {
                    said = Instant::now();
                    ": keepalive\n\n"
                } else {
                    PROBE_FRAME
                };
                if writer.emit(text).is_err() {
                    return Ok(());
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                // Dropped by the hub: too slow to keep up.
                let _ = writer.emit(&resync("slow_consumer", sent, None).render());
                return Ok(());
            }
        }
    }
}

/// A stream's writer whose every write is bounded by a timeout: the raw
/// writer tiny_http gives has no socket to set one on, so the writes run on
/// a thread of their own and the stream waits for each at most `timeout`.
/// A write that does not end in time ends the stream: its slot and its
/// channel are freed at once, and the writing thread ends when the kernel
/// fails the connection.
pub(super) struct TimedWriter {
    chunks: SyncSender<Vec<u8>>,
    done: Receiver<std::io::Result<()>>,
    timeout: Duration,
}

impl TimedWriter {
    pub(super) fn new(mut writer: Box<dyn Write + Send>, timeout: Duration) -> Self {
        // One chunk at a time: `emit` waits for each one's end before the
        // next, so a send never waits.
        let (chunks, take) = std::sync::mpsc::sync_channel::<Vec<u8>>(1);
        let (report, done) = std::sync::mpsc::sync_channel(1);
        let _ = std::thread::Builder::new()
            .name("serve-stream-write".to_string())
            .spawn(move || {
                for chunk in take {
                    let written = writer.write_all(&chunk).and_then(|()| writer.flush());
                    let failed = written.is_err();
                    if report.send(written).is_err() || failed {
                        return;
                    }
                }
            });
        Self {
            chunks,
            done,
            timeout,
        }
    }

    /// Write `text` whole, or fail: an error, or no end in time.
    pub(super) fn emit(&mut self, text: &str) -> std::io::Result<()> {
        let gone =
            || std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the stream's writer ended");
        self.chunks
            .try_send(text.as_bytes().to_vec())
            .map_err(|_| gone())?;
        match self.done.recv_timeout(self.timeout) {
            Ok(written) => written,
            Err(RecvTimeoutError::Timeout) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "the page took no write in time",
            )),
            Err(RecvTimeoutError::Disconnected) => Err(gone()),
        }
    }
}

/// The frames a stream starts with, and the last event id they carry:
/// `ready`, after the replay of the events after `wanted` (module doc), or
/// `resync` first when that is too far behind or past the log's end.
pub(super) fn catch_up(
    store: &PostgresStore,
    wanted: Option<i64>,
) -> Result<(Vec<Arc<Frame>>, i64), String> {
    let now = latest(store)?;
    let mut frames = Vec::new();
    let mut sent = now;
    if let Some(wanted) = wanted {
        if wanted > now {
            frames.push(Arc::new(resync("cursor_ahead", wanted, Some(now))));
        } else {
            let page = after(store, wanted, REPLAY_MAX + 1)?;
            if page.len() > REPLAY_MAX {
                frames.push(Arc::new(resync("behind", wanted, Some(now))));
            } else {
                sent = page
                    .last()
                    .and_then(|frame| frame.id)
                    .unwrap_or(wanted)
                    .max(now);
                frames.extend(page);
            }
        }
    }
    let replayed = frames.iter().filter(|frame| frame.id.is_some()).count();
    frames.insert(
        0,
        Arc::new(Frame::note(
            "ready",
            &json!({"latest_event_id": now, "replayed": replayed}),
        )),
    );
    Ok((frames, sent))
}

#[cfg(test)]
mod tests;
