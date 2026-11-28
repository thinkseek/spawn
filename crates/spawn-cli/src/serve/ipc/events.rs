//! `GET /ipc/v1/events` NDJSON upstream stream and direct UDS `GET /api/stream`
//! SSE fallback (`docs/spawn-ipc-v1.md` §3.4).

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use spawn_core::event::GlobalPosition;
use spawn_core::port::StoreError;
use spawn_core::time::Timestamp;
use spawn_postgres::{LogEntry, PostgresStore};

use super::http::{MAX_IPC_BODY_BYTES, write_json_error};
use super::{IpcState, WRITE_TIMEOUT};
use crate::serve::stream::{
    CLIENT_BUFFER, Frame, PROBE, PROBE_FRAME, REPLAY_MAX, Subscription, cursor,
};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const POLL_WAIT: Duration = Duration::from_millis(500);
const DEFAULT_TAIL: usize = 0;

struct EventTimedWriter {
    chunks: SyncSender<Vec<u8>>,
    done: mpsc::Receiver<std::io::Result<()>>,
    timeout: Duration,
}

impl EventTimedWriter {
    fn new(mut stream: UnixStream, timeout: Duration) -> Self {
        let _ = stream.set_write_timeout(Some(timeout));
        let (chunks, take) = mpsc::sync_channel::<Vec<u8>>(CLIENT_BUFFER);
        let (report, done) = mpsc::sync_channel(CLIENT_BUFFER);
        let _ = std::thread::Builder::new()
            .name("ipc-stream-write".to_string())
            .spawn(move || {
                for chunk in take {
                    let res = stream.write_all(&chunk).and_then(|()| stream.flush());
                    let err = res.is_err();
                    if report.send(res).is_err() || err {
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

    fn emit_line(&mut self, line: &str) -> Result<(), EmitError> {
        if line.len() > MAX_IPC_BODY_BYTES {
            return Err(EmitError::Oversize);
        }
        let mut bytes = Vec::with_capacity(line.len() + 1);
        bytes.extend_from_slice(line.as_bytes());
        if !bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        }
        self.emit_raw(&bytes)
    }

    fn emit_raw(&mut self, bytes: &[u8]) -> Result<(), EmitError> {
        match self.chunks.try_send(bytes.to_vec()) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => return Err(EmitError::SlowConsumer),
            Err(TrySendError::Disconnected(_)) => return Err(EmitError::Io),
        }
        match self.done.recv_timeout(self.timeout) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) | Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                Err(EmitError::SlowConsumer)
            }
        }
    }
}

#[derive(Clone, Copy)]
enum EmitError {
    SlowConsumer,
    Oversize,
    Io,
}

#[derive(Debug, Clone, Copy)]
enum StartCursor {
    Live,
    After { id: u64, ts_ms: Option<i64> },
    Tail(usize),
}

fn parse_query_param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .find_map(|pair| match pair.split_once('=') {
            Some((k, v)) if k == name => Some(v),
            _ => None,
        })
}

fn parse_events_query(query: &str) -> Result<StartCursor, String> {
    let after = parse_query_param(query, "after");
    let tail = parse_query_param(query, "tail");
    let after_ts = parse_query_param(query, "after_ts");
    if after.is_some() && tail.is_some() {
        return Err("specify either after or tail, not both".to_string());
    }
    if let Some(after_raw) = after {
        let id = after_raw
            .parse::<u64>()
            .map_err(|_| format!("invalid after cursor {after_raw:?}"))?;
        let ts_ms = match after_ts {
            Some(ts_raw) => Some(
                ts_raw
                    .parse::<i64>()
                    .map_err(|_| format!("invalid after_ts cursor {ts_raw:?}"))?,
            ),
            None => None,
        };
        return Ok(StartCursor::After { id, ts_ms });
    }
    if let Some(tail_raw) = tail {
        let count = tail_raw
            .parse::<usize>()
            .map_err(|_| format!("invalid tail count {tail_raw:?}"))?;
        if count > REPLAY_MAX {
            return Err(format!("tail exceeds max replay window ({REPLAY_MAX})"));
        }
        return Ok(StartCursor::Tail(count));
    }
    let _ = DEFAULT_TAIL;
    Ok(StartCursor::Live)
}

pub(super) fn latest_cursor(store: &PostgresStore) -> Result<(u64, i64), StoreError> {
    let latest_pos = store.position_before(Timestamp(i64::MAX))?;
    let recorded_at_ms = if latest_pos.0 == 0 {
        0
    } else {
        store
            .log_after(GlobalPosition(latest_pos.0.saturating_sub(1)), 1)?
            .last()
            .map_or(0, |entry| entry.recorded_at.0)
    };
    Ok((latest_pos.0, recorded_at_ms))
}

fn event_line(entry: &LogEntry) -> String {
    json!({
        "type": "event",
        "id": entry.position.0,
        "agent_id": entry.agent_id,
        "session_id": entry.session_id,
        "execution_id": entry.execution_id,
        "source": entry.source,
        "event_name": entry.event_name,
        "detail": entry.detail,
        "recorded_at_ms": entry.recorded_at.0,
    })
    .to_string()
}

fn resync_line(
    reason: &str,
    after_id: Option<u64>,
    latest_id: Option<u64>,
    recorded_at_ms: Option<i64>,
) -> String {
    json!({
        "type": "resync",
        "reason": reason,
        "after_id": after_id,
        "latest_id": latest_id,
        "recorded_at_ms": recorded_at_ms,
    })
    .to_string()
}

fn collect_ndjson_catchup(
    store: &PostgresStore,
    start: StartCursor,
    latest_id: u64,
    latest_ts: i64,
) -> Result<(Vec<String>, u64, i64, usize), StoreError> {
    let mut pending_lines = Vec::new();
    let mut cursor_id = latest_id;
    let mut cursor_ts = latest_ts;
    let mut replayed = 0_usize;

    match start {
        StartCursor::Live => {}
        StartCursor::Tail(n) => {
            if n > 0 && latest_id > 0 {
                let start_pos = latest_id.saturating_sub(u64::try_from(n).unwrap_or(u64::MAX));
                let entries = store.log_after(GlobalPosition(start_pos), n)?;
                replayed = entries.len();
                for entry in &entries {
                    cursor_id = entry.position.0;
                    cursor_ts = entry.recorded_at.0;
                    pending_lines.push(event_line(entry));
                }
            }
        }
        StartCursor::After { id, ts_ms } => {
            if id > latest_id {
                pending_lines.push(resync_line(
                    "cursor_ahead",
                    Some(id),
                    Some(latest_id),
                    Some(latest_ts),
                ));
            } else if let Some(asked_ts) = ts_ms
                && asked_ts > latest_ts
                && latest_id > 0
            {
                pending_lines.push(resync_line(
                    "stream_reset",
                    Some(id),
                    Some(latest_id),
                    Some(latest_ts),
                ));
            } else {
                let entries = store.log_after(GlobalPosition(id), REPLAY_MAX + 1)?;
                if entries.len() > REPLAY_MAX {
                    pending_lines.push(resync_line(
                        "behind",
                        Some(id),
                        Some(latest_id),
                        Some(latest_ts),
                    ));
                } else {
                    replayed = entries.len();
                    for entry in &entries {
                        cursor_id = entry.position.0;
                        cursor_ts = entry.recorded_at.0;
                        pending_lines.push(event_line(entry));
                    }
                }
            }
        }
    }
    Ok((pending_lines, cursor_id, cursor_ts, replayed))
}

fn emit_stream_err(writer: &mut EventTimedWriter, err: EmitError, after_id: Option<u64>) {
    if matches!(err, EmitError::SlowConsumer | EmitError::Oversize) {
        let _ = writer.emit_line(&resync_line("slow_consumer", after_id, None, None));
    }
}

fn emit_ndjson_live_frame(
    writer: &mut EventTimedWriter,
    frame: &Frame,
    cursor_id: &mut u64,
    cursor_ts: &mut i64,
) -> bool {
    if frame.event == "resync" {
        if let Ok(data) = serde_json::from_str::<Value>(&frame.data) {
            let reason = data["reason"].as_str().unwrap_or("stream_reset");
            let after_id = data["after_event_id"].as_u64();
            let latest_id = data["latest_event_id"].as_u64();
            if let Err(err) = writer.emit_line(&resync_line(reason, after_id, latest_id, None)) {
                emit_stream_err(writer, err, Some(*cursor_id));
                return false;
            }
            if let Some(latest_id) = latest_id {
                *cursor_id = latest_id;
            }
        }
        return true;
    }
    let Some(id_i64) = frame.id else {
        return true;
    };
    let id = u64::try_from(id_i64).unwrap_or(0);
    if id <= *cursor_id {
        return true;
    }
    let Ok(Value::Object(mut obj)) = serde_json::from_str::<Value>(&frame.data) else {
        return true;
    };
    let recorded_at_ms = obj
        .get("recorded_at_ms")
        .and_then(Value::as_i64)
        .unwrap_or(*cursor_ts);
    if recorded_at_ms < *cursor_ts {
        let _ = writer.emit_line(&resync_line(
            "stream_reset",
            Some(*cursor_id),
            Some(id),
            Some(recorded_at_ms),
        ));
        return false;
    }
    obj.insert("type".to_string(), Value::String("event".to_string()));
    if let Err(err) = writer.emit_line(&Value::Object(obj).to_string()) {
        emit_stream_err(writer, err, Some(*cursor_id));
        return false;
    }
    *cursor_id = id;
    *cursor_ts = recorded_at_ms;
    true
}

/// Serve `GET /ipc/v1/events` as NDJSON over UDS via the shared `events_hub`
/// (`EVENTS_POOL`), holding a `READS_POOL` connection only for transient catch-up.
pub(super) fn serve_ndjson_events(
    mut stream: UnixStream,
    query: &str,
    state: &Arc<IpcState>,
    shutdown: &AtomicBool,
) {
    let start = match parse_events_query(query) {
        Ok(start) => start,
        Err(why) => {
            let _ = write_json_error(&mut stream, 400, "bad_request", &why, true);
            return;
        }
    };
    let live = state.pools.events_hub.subscribe();
    state.pools.ensure_stream_listener();
    let (latest_id, latest_ts, pending_lines, mut cursor_id, mut cursor_ts, replayed) =
        match super::with_read(&state.pools.reader, |reader_conn| {
            let (latest_id, latest_ts) = latest_cursor(reader_conn)?;
            let (pending_lines, cursor_id, cursor_ts, replayed) =
                collect_ndjson_catchup(reader_conn, start, latest_id, latest_ts)?;
            Ok((
                latest_id,
                latest_ts,
                pending_lines,
                cursor_id,
                cursor_ts,
                replayed,
            ))
        }) {
            Ok(caught) => caught,
            Err((status, msg)) => {
                let code = if status == 503 { "busy" } else { "internal" };
                let _ = write_json_error(&mut stream, status, code, &msg, true);
                return;
            }
        };
    live.floor(i64::try_from(cursor_id).unwrap_or(i64::MAX));

    let head = "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson; charset=utf-8\r\n\
                Cache-Control: no-store\r\nConnection: close\r\n\r\n";
    if stream.write_all(head.as_bytes()).is_err() || stream.flush().is_err() {
        return;
    }
    let mut writer = EventTimedWriter::new(stream, WRITE_TIMEOUT);
    let ready = json!({
        "type": "ready",
        "latest_id": latest_id,
        "recorded_at_ms": latest_ts,
        "replayed": replayed,
    })
    .to_string();
    if writer.emit_line(&ready).is_err() {
        return;
    }
    for line in &pending_lines {
        if let Err(err) = writer.emit_line(line) {
            emit_stream_err(&mut writer, err, Some(cursor_id));
            return;
        }
    }
    let mut last_hb = Instant::now();
    while !shutdown.load(Ordering::Acquire) {
        match live.recv_timeout(POLL_WAIT) {
            Ok(frame) => {
                let prev = cursor_id;
                if !emit_ndjson_live_frame(&mut writer, &frame, &mut cursor_id, &mut cursor_ts) {
                    return;
                }
                if cursor_id != prev {
                    last_hb = Instant::now();
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if last_hb.elapsed() >= HEARTBEAT_INTERVAL {
                    let hb = json!({
                        "type": "hb",
                        "latest_id": cursor_id,
                        "recorded_at_ms": cursor_ts,
                    })
                    .to_string();
                    if let Err(err) = writer.emit_line(&hb) {
                        emit_stream_err(&mut writer, err, Some(cursor_id));
                        return;
                    }
                    last_hb = Instant::now();
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ =
                    writer.emit_line(&resync_line("slow_consumer", Some(cursor_id), None, None));
                return;
            }
        }
    }
}

fn stream_sse_live(
    writer: &mut EventTimedWriter,
    live: &Subscription,
    shutdown: &AtomicBool,
    mut sent: i64,
) {
    let mut last_hb = Instant::now();
    while !shutdown.load(Ordering::Acquire) {
        match live.recv_timeout(PROBE) {
            Ok(frame) => {
                if frame.id.is_some_and(|id| id <= sent) {
                    continue;
                }
                sent = frame.id.unwrap_or(sent);
                if writer.emit_raw(frame.render().as_bytes()).is_err() {
                    return;
                }
                last_hb = Instant::now();
            }
            Err(RecvTimeoutError::Timeout) => {
                let frame_bytes = if last_hb.elapsed() >= HEARTBEAT_INTERVAL {
                    last_hb = Instant::now();
                    b": keepalive\n\n".as_slice()
                } else {
                    PROBE_FRAME.as_bytes()
                };
                if writer.emit_raw(frame_bytes).is_err() {
                    return;
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                let _ = writer.emit_raw(
                    crate::serve::stream::resync("slow_consumer", sent, None)
                        .render()
                        .as_bytes(),
                );
                return;
            }
        }
    }
}

/// Serve `GET /api/stream` over UDS via the shared `events_hub` (`EVENTS_POOL`),
/// borrowing `READS_POOL` strictly for transient catch-up before streaming.
pub(super) fn serve_api_sse(
    mut stream: UnixStream,
    query: &str,
    last_event_id: Option<&str>,
    state: &Arc<IpcState>,
    shutdown: &AtomicBool,
) {
    let wanted = match cursor(last_event_id, query) {
        Ok(wanted) => wanted,
        Err(why) => {
            let _ = write_json_error(&mut stream, 400, "bad_request", &why, true);
            return;
        }
    };
    let live = state.pools.events_hub.subscribe();
    state.pools.ensure_stream_listener();
    let (first, sent) = match super::with_read(&state.pools.reader, |reader_conn| {
        crate::serve::stream::catch_up_store(reader_conn, wanted)
    }) {
        Ok(caught) => caught,
        Err((status, msg)) => {
            let code = if status == 503 { "busy" } else { "internal" };
            let _ = write_json_error(&mut stream, status, code, &msg, true);
            return;
        }
    };
    live.floor(sent);

    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream; charset=utf-8\r\n\
                Cache-Control: no-cache, no-store\r\nConnection: close\r\n\r\nretry: 3000\n\n";
    if stream.write_all(head.as_bytes()).is_err() || stream.flush().is_err() {
        return;
    }
    let mut writer = EventTimedWriter::new(stream, WRITE_TIMEOUT);
    for frame in &first {
        if writer.emit_raw(frame.render().as_bytes()).is_err() {
            return;
        }
    }
    stream_sse_live(&mut writer, &live, shutdown, sent);
}
