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
//!   [`KEEPALIVE`] of quiet. A stream opened with [`AuthPrincipal::ReadOnlyViewer`]
//!   closes automatically at its `expires_at_ms` deadline so expired viewer
//!   tokens receive no post-expiry events or probes.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use spawn_core::event::GlobalPosition;
use spawn_core::port::StoreError;
use spawn_postgres::PostgresStore;
use tiny_http::{Method, Request};

use super::auth_token::{AuthPrincipal, current_epoch_ms};
use super::{AccessDenied, BAD_REQUEST, json_response, not_allowed, parameter, refuse};

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
const POLL: Duration = Duration::from_millis(250);
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
pub(crate) struct Frame {
    pub(crate) id: Option<i64>,
    pub(crate) event: String,
    pub(crate) data: String,
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
pub(crate) struct Subscription {
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
    pub(crate) fn floor(&self, sent: i64) {
        self.floor.store(sent.max(0), Ordering::Release);
    }

    pub(crate) fn recv_timeout(&self, timeout: Duration) -> Result<Arc<Frame>, RecvTimeoutError> {
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
pub(crate) struct Hub {
    subscribers: Mutex<Vec<Subscriber>>,
    wake: Condvar,
    started: AtomicBool,
    stopped: AtomicBool,
    buffer: usize,
}

/// Clears the hub's `started` when the listener's thread ends, a panic
/// included, so the next stream starts it again.
#[cfg(test)]
struct Started<'a>(&'a AtomicBool);

#[cfg(test)]
impl Drop for Started<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

struct ArcStarted(Arc<Hub>);

impl Drop for ArcStarted {
    fn drop(&mut self) {
        self.0.started.store(false, Ordering::Release);
    }
}

impl Hub {
    pub(crate) fn new(buffer: usize) -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
            wake: Condvar::new(),
            started: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            buffer,
        }
    }

    /// A new stream's channel.
    pub(crate) fn subscribe(&self) -> Subscription {
        let (send, receive) = std::sync::mpsc::sync_channel(self.buffer);
        let alive = Arc::new(AtomicBool::new(true));
        let floor = Arc::new(AtomicI64::new(NO_FLOOR));
        let mut subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        subscribers.push(Subscriber {
            send,
            alive: Arc::clone(&alive),
            floor: Arc::clone(&floor),
        });
        self.wake.notify_all();
        Subscription {
            receive,
            alive,
            floor,
        }
    }

    pub(crate) fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        let _guard = self
            .subscribers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.wake.notify_all();
    }

    fn wait_for_subscriber(&self, timeout: Duration) {
        let subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !subscribers.is_empty() || self.stopped.load(Ordering::Acquire) {
            return;
        }
        let _ = self.wake.wait_timeout(subscribers, timeout);
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
    #[cfg(test)]
    pub(super) fn start<S: StoreHandle + 'static>(
        &'static self,
        open: impl Fn() -> Result<S, String> + Send + 'static,
    ) {
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let spawned = std::thread::Builder::new()
            .name("serve-stream-listen".to_string())
            .spawn(move || {
                let _started = Started(&self.started);
                listen(self, &open, &|| self.stopped.load(Ordering::Acquire));
            });
        if let Err(error) = spawned {
            eprintln!("serve: the stream's listener did not start: {error}");
            self.started.store(false, Ordering::Release);
        }
    }

    pub(crate) fn start_arc<S: StoreHandle + 'static>(
        self: &Arc<Self>,
        open: impl Fn() -> Result<S, String> + Send + 'static,
    ) {
        if self.stopped.load(Ordering::Acquire) || self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let hub = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("serve-stream-listen".to_string())
            .spawn(move || {
                let guard = ArcStarted(Arc::clone(&hub));
                listen(&hub, &open, &|| hub.stopped.load(Ordering::Acquire));
                drop(guard);
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

/// Abstract handle over a [`PostgresStore`] (pooled in production, direct in tests)
/// so stream listeners and catch-up can evict broken pooled connections on error.
pub(crate) trait StoreHandle {
    fn store(&self) -> &PostgresStore;
    fn mark_broken(&self) {}
}

impl StoreHandle for PostgresStore {
    fn store(&self) -> &PostgresStore {
        self
    }
}

impl StoreHandle for super::PooledStore {
    fn store(&self) -> &PostgresStore {
        self
    }

    fn mark_broken(&self) {
        super::PooledStore::mark_broken(self);
    }
}

impl StoreHandle for super::ipc::CheckedOutConn {
    fn store(&self) -> &PostgresStore {
        self
    }

    fn mark_broken(&self) {
        super::ipc::CheckedOutConn::mark_broken(self);
    }
}

/// A `resync` frame: always `{reason, after_event_id, latest_event_id}`,
/// `latest_event_id` null where it was not read (module doc).
pub(crate) fn resync(reason: &str, after_event_id: i64, latest_event_id: Option<i64>) -> Frame {
    Frame::note(
        "resync",
        &json!({"reason": reason, "after_event_id": after_event_id,
                "latest_event_id": latest_event_id}),
    )
}

/// The latest event's id, through the reader (`position_before` the end
/// of time: the last position).
fn latest(store: &PostgresStore) -> Result<i64, StoreError> {
    store
        .position_before(spawn_core::time::Timestamp(i64::MAX))
        .map(|position| i64::try_from(position.0).unwrap_or(i64::MAX))
}

/// Up to `limit` events after `after`.
fn after(store: &PostgresStore, after: i64, limit: usize) -> Result<Vec<Arc<Frame>>, StoreError> {
    store
        .log_after(GlobalPosition(after.unsigned_abs()), limit)
        .map(|entries| {
            entries
                .iter()
                .map(|entry| Arc::new(Frame::of(entry)))
                .collect()
        })
}

/// The listener (module doc): until `stop` says so (never, in `serve`).
pub(super) fn listen<S: StoreHandle>(
    hub: &Hub,
    open: &dyn Fn() -> Result<S, String>,
    stop: &dyn Fn() -> bool,
) {
    let mut cursor: Option<i64> = None;
    let mut backoff = BACKOFF_MIN;
    let mut said: Option<String> = None;
    while !stop() {
        if hub.subscribers() == 0 {
            // Release all PG listener/events-pool sockets while zero clients stream.
            cursor = None;
            hub.wait_for_subscriber(Duration::from_millis(50));
            continue;
        }
        let failed = (|| -> Result<(), String> {
            let handle = open()?;
            let store = handle.store();
            let result = (|| -> Result<(), String> {
                let mut listener = store.listen().map_err(|error| error.to_string())?;
                (backoff, said) = (BACKOFF_MIN, None);
                while !stop() {
                    if hub.subscribers() == 0 {
                        cursor = None;
                        return Ok(());
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
                    let page = after(store, from, REPLAY_MAX + 1).map_err(|e| e.to_string())?;
                    if page.len() > REPLAY_MAX {
                        // Further behind than a catch-up replays (a long
                        // reconnect): every stream resyncs from now.
                        let now = latest(store).map_err(|e| e.to_string())?;
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
            if result.is_err() {
                handle.mark_broken();
            }
            result
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
pub(crate) fn cursor(last_event_id: Option<&str>, query: &str) -> Result<Option<i64>, String> {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamWindow {
    Unbounded,
    Remaining(Duration),
    Expired,
}

fn stream_window(deadline_ms: Option<i64>, now_ms: Option<i64>) -> StreamWindow {
    let Some(expires_at_ms) = deadline_ms else {
        return StreamWindow::Unbounded;
    };
    let Some(now) = now_ms else {
        return StreamWindow::Expired;
    };
    match expires_at_ms.checked_sub(now) {
        Some(remaining_ms) if remaining_ms > 0 => u64::try_from(remaining_ms)
            .map_or(StreamWindow::Expired, |ms| {
                StreamWindow::Remaining(Duration::from_millis(ms))
            }),
        _ => StreamWindow::Expired,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmitOutcome {
    Sent,
    Expired,
    WriteFailed,
}

fn encode_chunk(text: &str) -> String {
    format!("{:x}\r\n{text}\r\n", text.len())
}

fn emit_raw_bounded(writer: &mut TimedWriter, text: &str, deadline_ms: Option<i64>) -> EmitOutcome {
    let sent = match stream_window(deadline_ms, current_epoch_ms()) {
        StreamWindow::Expired => return EmitOutcome::Expired,
        StreamWindow::Unbounded => writer.emit(text).is_ok(),
        StreamWindow::Remaining(remaining) => writer.emit_within(text, remaining).is_ok(),
    };
    if sent {
        EmitOutcome::Sent
    } else {
        EmitOutcome::WriteFailed
    }
}

fn emit_sse_bounded(
    writer: &mut TimedWriter,
    text: &str,
    chunked: bool,
    deadline_ms: Option<i64>,
) -> EmitOutcome {
    if text.is_empty() {
        return EmitOutcome::Sent;
    }
    if chunked {
        emit_raw_bounded(writer, &encode_chunk(text), deadline_ms)
    } else {
        emit_raw_bounded(writer, text, deadline_ms)
    }
}

fn finish_chunked(writer: &mut TimedWriter, chunked: bool) {
    if chunked {
        let _ = writer.emit_within("0\r\n\r\n", Duration::from_millis(500));
    }
}

fn stream_head(chunked: bool) -> String {
    if chunked {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\n\
             Cache-Control: no-cache, no-store\r\nConnection: close\r\n\
             Transfer-Encoding: chunked\r\nX-Accel-Buffering: no\r\n\r\n{}",
            encode_chunk("retry: 3000\n\n")
        )
    } else {
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\n\
         Cache-Control: no-cache, no-store\r\nConnection: close\r\n\
         X-Accel-Buffering: no\r\n\r\nretry: 3000\n\n"
            .to_string()
    }
}

fn pump_live_frames(
    live: &Subscription,
    writer: &mut TimedWriter,
    mut sent: i64,
    keepalive: Duration,
    chunked: bool,
    deadline_ms: Option<i64>,
) -> bool {
    // A quiet stream probes every PROBE (never less often than its
    // keepalive), and says `: keepalive` once `keepalive` has passed
    // since its last frame or keepalive; any active deadline bounds wait time.
    let probe = PROBE.min(keepalive);
    let mut said = Instant::now();
    loop {
        let wait = match stream_window(deadline_ms, current_epoch_ms()) {
            StreamWindow::Expired => return true,
            StreamWindow::Unbounded => probe,
            StreamWindow::Remaining(remaining) => probe.min(remaining),
        };
        match live.recv_timeout(wait) {
            Ok(frame) => {
                if frame.id.is_some_and(|id| id <= sent) {
                    continue;
                }
                sent = frame.id.unwrap_or(sent);
                match emit_sse_bounded(writer, &frame.render(), chunked, deadline_ms) {
                    EmitOutcome::Sent => said = Instant::now(),
                    EmitOutcome::Expired => return true,
                    EmitOutcome::WriteFailed => return false,
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if matches!(
                    stream_window(deadline_ms, current_epoch_ms()),
                    StreamWindow::Expired
                ) {
                    return true;
                }
                let text = if said.elapsed() >= keepalive {
                    said = Instant::now();
                    ": keepalive\n\n"
                } else {
                    PROBE_FRAME
                };
                match emit_sse_bounded(writer, text, chunked, deadline_ms) {
                    EmitOutcome::Sent => {}
                    EmitOutcome::Expired => return true,
                    EmitOutcome::WriteFailed => return false,
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                // Dropped by the hub: too slow to keep up.
                return matches!(
                    emit_sse_bounded(
                        writer,
                        &resync("slow_consumer", sent, None).render(),
                        chunked,
                        deadline_ms,
                    ),
                    EmitOutcome::Sent | EmitOutcome::Expired
                );
            }
        }
    }
}

/// `GET /api/stream` (module doc), past the slots and the read guard
/// (`handle` refuses a stream exactly as any API read).
pub(super) fn stream(
    request: Request,
    query: &str,
    principal: AuthPrincipal,
    shared: &super::Shared,
) -> Result<(), String> {
    let pools = super::stream_pools(shared);
    let open_reader = || -> Result<super::PooledStore, String> {
        match &pools {
            Ok(Some(pools)) => super::checkout_pooled(&pools.reader),
            _ => super::open_store(),
        }
    };
    if let Ok(Some(ref active_pools)) = pools {
        // Shared hub listener runs strictly on `EVENTS_POOL`, leaving `READS_POOL`
        // free for transient catch-up and ordinary HTTP/IPC reads.
        if *request.method() == Method::Get
            && cursor(super::header(&request, "Last-Event-ID"), query).is_ok()
            && !matches!(
                stream_window(principal.stream_expires_at_ms(), current_epoch_ms()),
                StreamWindow::Expired
            )
        {
            active_pools.ensure_stream_listener();
        }
        return serve_stream(
            request,
            query,
            principal,
            &active_pools.events_hub,
            &open_reader,
            KEEPALIVE,
        );
    }
    let fallback_hub = Hub::new(CLIENT_BUFFER);
    serve_stream(
        request,
        query,
        principal,
        &fallback_hub,
        &open_reader,
        KEEPALIVE,
    )
}

/// A stream on `hub`, its catch-up read through `open`, bounded by `principal` expiry if set.
pub(super) fn serve_stream<S: StoreHandle>(
    request: Request,
    query: &str,
    principal: AuthPrincipal,
    hub: &Hub,
    open: &dyn Fn() -> Result<S, String>,
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
    let deadline_ms = principal.stream_expires_at_ms();
    if matches!(
        stream_window(deadline_ms, current_epoch_ms()),
        StreamWindow::Expired
    ) {
        return refuse(request, AccessDenied::Unauthorized);
    }
    // Subscribed before the catch-up is read, so no event falls between
    // the two; one read in both is sent once (`sent`).
    let live = hub.subscribe();
    let store = match open() {
        Ok(store) => store,
        Err(error) => {
            let status = if error.starts_with(super::BUSY) {
                503
            } else {
                500
            };
            return request
                .respond(json_response(status, &json!({ "error": error })))
                .map_err(|error| error.to_string());
        }
    };
    let (first, sent) = match catch_up(store.store(), wanted) {
        Ok(caught) => caught,
        Err(error) => {
            store.mark_broken();
            return request
                .respond(json_response(500, &json!({ "error": error })))
                .map_err(|error| error.to_string());
        }
    };
    drop(store);
    if matches!(
        stream_window(deadline_ms, current_epoch_ms()),
        StreamWindow::Expired
    ) {
        return refuse(request, AccessDenied::Unauthorized);
    }
    // The listener, if it has no cursor, starts no later than this.
    live.floor(sent);
    let chunked = *request.http_version() >= tiny_http::HTTPVersion(1, 1);
    let mut writer = TimedWriter::new(request.into_writer(), WRITE_TIMEOUT);
    let head = stream_head(chunked);
    if emit_raw_bounded(&mut writer, &head, deadline_ms) != EmitOutcome::Sent {
        return Ok(());
    }
    for frame in &first {
        match emit_sse_bounded(&mut writer, &frame.render(), chunked, deadline_ms) {
            EmitOutcome::Sent => {}
            EmitOutcome::Expired => {
                finish_chunked(&mut writer, chunked);
                return Ok(());
            }
            EmitOutcome::WriteFailed => return Ok(()),
        }
    }
    if pump_live_frames(&live, &mut writer, sent, keepalive, chunked, deadline_ms) {
        finish_chunked(&mut writer, chunked);
    }
    Ok(())
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

    /// Write `text` whole within `self.timeout`, or fail: an error, or no end in time.
    pub(super) fn emit(&mut self, text: &str) -> std::io::Result<()> {
        self.emit_within(text, self.timeout)
    }

    /// Write `text` whole within `self.timeout.min(timeout)`, or fail: an error, or no end in time.
    pub(super) fn emit_within(&mut self, text: &str, timeout: Duration) -> std::io::Result<()> {
        let gone =
            || std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the stream's writer ended");
        self.chunks
            .try_send(text.as_bytes().to_vec())
            .map_err(|_| gone())?;
        match self.done.recv_timeout(self.timeout.min(timeout)) {
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
pub(crate) fn catch_up_store(
    store: &PostgresStore,
    wanted: Option<i64>,
) -> Result<(Vec<Arc<Frame>>, i64), StoreError> {
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

pub(super) fn catch_up(
    store: &PostgresStore,
    wanted: Option<i64>,
) -> Result<(Vec<Arc<Frame>>, i64), String> {
    catch_up_store(store, wanted).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests;
