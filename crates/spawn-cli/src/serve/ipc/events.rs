//! `GET /ipc/v1/events` NDJSON upstream stream and direct UDS `GET /api/stream`
//! SSE fallback (`docs/spawn-ipc-v1.md` §3.4).

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
use std::time::{Duration, Instant};

use serde_json::json;
use spawn_core::event::GlobalPosition;
use spawn_core::time::Timestamp;
use spawn_postgres::{LogEntry, PostgresStore};

use super::http::{MAX_IPC_BODY_BYTES, write_json_error};
use super::{IpcState, WRITE_TIMEOUT};
use crate::serve::stream::{CLIENT_BUFFER, Frame, PROBE, PROBE_FRAME, REPLAY_MAX, cursor};

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

pub(super) fn latest_cursor(store: &PostgresStore) -> Result<(u64, i64), String> {
    let latest_pos = store
        .position_before(Timestamp(i64::MAX))
        .map_err(|e| e.to_string())?;
    let recorded_at_ms = if latest_pos.0 == 0 {
        0
    } else {
        store
            .log_after(GlobalPosition(latest_pos.0.saturating_sub(1)), 1)
            .map_err(|e| e.to_string())?
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
) -> (Vec<String>, u64, i64, usize) {
    let mut pending_lines = Vec::new();
    let mut cursor_id = latest_id;
    let mut cursor_ts = latest_ts;
    let mut replayed = 0_usize;

    match start {
        StartCursor::Live => {}
        StartCursor::Tail(n) => {
            if n > 0 && latest_id > 0 {
                let start_pos = latest_id.saturating_sub(u64::try_from(n).unwrap_or(u64::MAX));
                if let Ok(entries) = store.log_after(GlobalPosition(start_pos), n) {
                    replayed = entries.len();
                    for entry in &entries {
                        cursor_id = entry.position.0;
                        cursor_ts = entry.recorded_at.0;
                        pending_lines.push(event_line(entry));
                    }
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
            } else if let Ok(entries) = store.log_after(GlobalPosition(id), REPLAY_MAX + 1) {
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
    (pending_lines, cursor_id, cursor_ts, replayed)
}

/// Serve `GET /ipc/v1/events` as NDJSON over UDS (`events` pool, `spawn_reader`).
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
    let Some(_permit) = state.pools.events_gate.try_acquire() else {
        let _ = write_json_error(
            &mut stream,
            503,
            "busy",
            "busy: too many IPC event streams",
            true,
        );
        return;
    };
    let reader_conn = match state.pools.reader.checkout() {
        Ok(conn) => conn,
        Err(err) => {
            let _ = write_json_error(&mut stream, 503, "busy", &err, true);
            return;
        }
    };
    let (latest_id, latest_ts) = match latest_cursor(&reader_conn) {
        Ok(cur) => cur,
        Err(err) => {
            let _ = write_json_error(&mut stream, 500, "internal", &err, true);
            return;
        }
    };
    let head = "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson; charset=utf-8\r\n\
                Cache-Control: no-store\r\nConnection: close\r\n\r\n";
    if stream.write_all(head.as_bytes()).is_err() || stream.flush().is_err() {
        return;
    }

    let (pending_lines, cursor_id, cursor_ts, replayed) =
        collect_ndjson_catchup(&reader_conn, start, latest_id, latest_ts);
    let Ok(mut listener) = reader_conn.listen() else {
        return;
    };
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
    run_ndjson_loop(
        &reader_conn,
        &mut listener,
        &mut writer,
        shutdown,
        cursor_id,
        cursor_ts,
    );
}

fn run_ndjson_loop(
    store: &PostgresStore,
    listener: &mut spawn_postgres::Listener,
    writer: &mut EventTimedWriter,
    shutdown: &AtomicBool,
    mut cursor_id: u64,
    mut cursor_ts: i64,
) {
    let mut last_hb = Instant::now();
    while !shutdown.load(Ordering::Acquire) {
        let Ok(page) = store.log_after(GlobalPosition(cursor_id), REPLAY_MAX + 1) else {
            return;
        };
        if page.len() > REPLAY_MAX {
            if let Ok((now_id, now_ts)) = latest_cursor(store) {
                let _ = writer.emit_line(&resync_line(
                    "stream_reset",
                    Some(cursor_id),
                    Some(now_id),
                    Some(now_ts),
                ));
                cursor_id = now_id;
                cursor_ts = now_ts;
            }
            last_hb = Instant::now();
            continue;
        }
        if page.is_empty() {
            if last_hb.elapsed() >= HEARTBEAT_INTERVAL {
                let hb = json!({
                    "type": "hb",
                    "latest_id": cursor_id,
                    "recorded_at_ms": cursor_ts,
                })
                .to_string();
                if let Err(err) = writer.emit_line(&hb) {
                    emit_stream_err(writer, err, Some(cursor_id));
                    return;
                }
                last_hb = Instant::now();
            }
            if listener.wait(POLL_WAIT).is_err() {
                return;
            }
            continue;
        }
        for entry in &page {
            if entry.recorded_at.0 < cursor_ts {
                let _ = writer.emit_line(&resync_line(
                    "stream_reset",
                    Some(cursor_id),
                    Some(entry.position.0),
                    Some(entry.recorded_at.0),
                ));
                return;
            }
            cursor_id = entry.position.0;
            cursor_ts = entry.recorded_at.0;
            if let Err(err) = writer.emit_line(&event_line(entry)) {
                emit_stream_err(writer, err, Some(cursor_id));
                return;
            }
            last_hb = Instant::now();
        }
    }
}

fn emit_stream_err(writer: &mut EventTimedWriter, err: EmitError, after_id: Option<u64>) {
    if matches!(err, EmitError::SlowConsumer | EmitError::Oversize) {
        let _ = writer.emit_line(&resync_line("slow_consumer", after_id, None, None));
    }
}

/// Serve `GET /api/stream` directly over UDS if requested without an edge
/// fan-out proxy (uses the same `spawn_reader` `events` pool).
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
    let Some(_permit) = state.pools.events_gate.try_acquire() else {
        let _ = write_json_error(&mut stream, 503, "busy", "busy: too many streams", true);
        return;
    };
    let reader_conn = match state.pools.reader.checkout() {
        Ok(conn) => conn,
        Err(err) => {
            let _ = write_json_error(&mut stream, 503, "busy", &err, true);
            return;
        }
    };
    let (first, mut sent) = match crate::serve::stream::catch_up(&reader_conn, wanted) {
        Ok(caught) => caught,
        Err(err) => {
            let _ = write_json_error(&mut stream, 500, "internal", &err, true);
            return;
        }
    };
    let mut listener = match reader_conn.listen() {
        Ok(listener) => listener,
        Err(err) => {
            let _ = write_json_error(&mut stream, 500, "internal", &err.to_string(), true);
            return;
        }
    };
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
    let mut last_hb = Instant::now();
    while !shutdown.load(Ordering::Acquire) {
        let start_from = GlobalPosition(u64::try_from(sent.max(0)).unwrap_or(0));
        let Ok(page) = reader_conn.log_after(start_from, REPLAY_MAX + 1) else {
            return;
        };
        if page.is_empty() {
            let frame_bytes = if last_hb.elapsed() >= HEARTBEAT_INTERVAL {
                last_hb = Instant::now();
                b": keepalive\n\n".as_slice()
            } else {
                PROBE_FRAME.as_bytes()
            };
            if writer.emit_raw(frame_bytes).is_err() || listener.wait(PROBE).is_err() {
                return;
            }
            continue;
        }
        for entry in &page {
            let id = i64::try_from(entry.position.0).unwrap_or(i64::MAX);
            let frame = Frame {
                id: Some(id),
                event: entry.event_name.clone(),
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
            };
            if writer.emit_raw(frame.render().as_bytes()).is_err() {
                return;
            }
            sent = id;
            last_hb = Instant::now();
        }
    }
}
