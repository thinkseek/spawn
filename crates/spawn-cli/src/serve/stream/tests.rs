//! The stream: its frames, its cursor, the hub's bounded fan-out, and,
//! through the test database as the reader login, the catch-up, the one
//! `LISTEN spawn_events` listener and a whole stream over HTTP.

use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use spawn_core::event::{EventKind, NewEvent, Source};
use spawn_core::id::{AgentId, SessionId};
use spawn_core::port::EventLog;
use spawn_postgres::PostgresStore;

use super::{
    Frame, Hub, REPLAY_MAX, TimedWriter, catch_up, cursor, listen, resync, serve_stream, sse_word,
};

#[test]
fn a_frame_is_id_event_and_one_data_line() {
    let frame = Frame {
        id: Some(42),
        event: "tool.started".to_string(),
        data: json!({"text": "two\nlines"}).to_string(),
    };
    assert_eq!(
        frame.render(),
        "id: 42\nevent: tool.started\ndata: {\"text\":\"two\\nlines\"}\n\n"
    );
    let note = Frame::note("resync", &json!({"reason": "behind"}));
    assert_eq!(
        note.render(),
        "event: resync\ndata: {\"reason\":\"behind\"}\n\n"
    );
    assert_eq!(sse_word("a\nb\rc"), "abc");
    assert_eq!(sse_word("\n"), "message");
}

/// Review S1: `?after=` as `/api/events` takes it, `?after_event_id=` its
/// alias, never both; `Last-Event-ID` (the browser's reconnect) first.
#[test]
fn the_cursor_is_last_event_id_else_after_or_its_alias() {
    assert_eq!(cursor(None, ""), Ok(None));
    assert_eq!(cursor(None, "after=3"), Ok(Some(3)));
    assert_eq!(cursor(None, "token=x&after=0"), Ok(Some(0)));
    assert_eq!(cursor(None, "after_event_id=7"), Ok(Some(7)));
    assert_eq!(cursor(Some("9"), "after_event_id=7"), Ok(Some(9)));
    assert_eq!(cursor(Some("9"), "after=3"), Ok(Some(9)));
    assert_eq!(cursor(Some(" "), "after_event_id=7"), Ok(Some(7)));
    assert_eq!(cursor(Some(" "), "after=4"), Ok(Some(4)));
    assert_eq!(cursor(None, "token=x&after_event_id=0"), Ok(Some(0)));
    for bad in [
        "after_event_id=-1",
        "after_event_id=x",
        "after_event_id=1.5",
        "after=",
        "after=x",
        "after=-2",
        "after=3&after_event_id=3",
        "after_event_id=1&after=2",
    ] {
        assert!(
            cursor(None, bad).is_err_and(|error| error.starts_with(super::BAD_REQUEST)),
            "{bad}"
        );
    }
    assert!(cursor(Some("abc"), "").is_err());
}

#[test]
fn the_hub_drops_a_slow_or_gone_stream_and_never_waits() {
    let hub = Hub::new(2);
    let fast = hub.subscribe();
    let slow = hub.subscribe();
    let gone = hub.subscribe();
    drop(gone);
    let frame = |id: i64| Arc::new(Frame::note("x", &json!(id)));
    let started = Instant::now();
    hub.publish(&frame(1));
    assert_eq!(hub.subscribers(), 2, "the gone one is dropped");
    assert_eq!(fast.recv().expect("one").data, "1");
    hub.publish(&frame(2));
    assert_eq!(fast.recv().expect("two").data, "2");
    // `slow` reads nothing: its channel of 2 is full at the third.
    hub.publish(&frame(3));
    assert_eq!(hub.subscribers(), 1, "the slow one is dropped");
    assert!(started.elapsed() < Duration::from_secs(1), "never waited");
    assert_eq!(fast.recv().expect("three").data, "3");
    let kept: Vec<String> = slow.try_iter().map(|frame| frame.data.clone()).collect();
    assert_eq!(kept, ["1", "2"]);
    assert!(slow.recv().is_err(), "then its channel is closed");
}

/// An opener of reader connections.
type Opener = Arc<dyn Fn() -> Result<PostgresStore, String> + Send + Sync>;

/// A test schema, its owner's store, and an opener of reader connections
/// in it (the dashboard's login).
fn reader_schema(case: &str) -> (spawn_postgres::testing::TestSchema, PostgresStore, Opener) {
    let url = std::env::var("SPAWN_TEST_DATABASE_URL")
        .expect("SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip");
    let schema = spawn_postgres::testing::TestSchema::new(case);
    let store = PostgresStore::connect_in_schema(&url, &schema).expect("migrated");
    let separator = if url.contains('?') { '&' } else { '?' };
    let owner = format!("{url}{separator}options=-csearch_path%3D{}", &*schema);
    let reader = spawn_postgres::role_url(&owner, "spawn_reader").expect("the reader's URL");
    let open = Arc::new(move || {
        PostgresStore::connect_with_statement_timeout(&reader, Duration::from_secs(10))
            .map_err(|error| error.to_string())
    });
    (schema, store, open)
}

fn append(store: &PostgresStore, n: usize) -> i64 {
    let event = store
        .append(NewEvent {
            agent_id: AgentId::new("agent_worker_stream").expect("agent"),
            session_id: SessionId::new("session_worker_stream").expect("session"),
            execution_id: None,
            source: Source::Claude,
            source_event_id: Some(format!("stream-{n}")),
            kind: EventKind::MessageReceived,
        })
        .expect("appended");
    i64::try_from(event.record.position.0).expect("an id")
}

fn ids(frames: &[Arc<Frame>]) -> Vec<i64> {
    frames.iter().filter_map(|frame| frame.id).collect()
}

#[test]
fn a_catch_up_replays_after_the_cursor_or_says_resync() {
    let (_schema, store, open) = reader_schema("stream_catch_up");
    let first = append(&store, 1);
    let second = append(&store, 2);
    let third = append(&store, 3);
    let reader = open().expect("the reader connects");
    let (frames, sent) = catch_up(&reader, None).expect("live only");
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].event, "ready");
    assert_eq!(sent, third, "a live start sends nothing older");
    let (frames, sent) = catch_up(&reader, Some(first)).expect("a replay");
    assert_eq!(ids(&frames), [second, third]);
    assert_eq!(sent, third);
    let ready: Value = serde_json::from_str(&frames[0].data).expect("json");
    assert_eq!(ready, json!({"latest_event_id": third, "replayed": 2}));
    let replayed: Value = serde_json::from_str(&frames[1].data).expect("json");
    assert_eq!(replayed["id"], second);
    assert_eq!(replayed["session_id"], "session_worker_stream");
    assert_eq!(replayed["event_name"], "message.received");
    assert_eq!(frames[1].event, "message.received");
    // A cursor the log never reached: resync, nothing replayed.
    let (frames, _) = catch_up(&reader, Some(third + 1000)).expect("ahead");
    assert_eq!(frames[1].event, "resync");
    assert!(frames[1].data.contains("cursor_ahead"));
    assert!(ids(&frames).is_empty());
    // Further behind than REPLAY_MAX: resync.
    for n in 0..=REPLAY_MAX {
        append(&store, 10 + n);
    }
    let (frames, sent) = catch_up(&reader, Some(first)).expect("behind");
    assert_eq!(frames[1].event, "resync");
    let resync: Value = serde_json::from_str(&frames[1].data).expect("json");
    assert_eq!(resync["reason"], "behind");
    assert_eq!(resync["after_event_id"], first);
    assert!(ids(&frames).is_empty());
    assert_eq!(Some(sent), resync["latest_event_id"].as_i64());
}

/// Wait at most `within` for a frame with id `id` on `receive`.
fn frame_with(receive: &super::Subscription, id: i64, within: Duration) -> Option<Arc<Frame>> {
    let until = Instant::now() + within;
    while Instant::now() < until {
        if let Ok(frame) = receive.recv_timeout(Duration::from_millis(100))
            && frame.id == Some(id)
        {
            return Some(frame);
        }
    }
    None
}

#[test]
fn one_listener_fans_each_new_event_out_to_every_stream() {
    let (_schema, store, open) = reader_schema("stream_listener");
    append(&store, 1);
    let hub = Arc::new(Hub::new(64));
    let one = hub.subscribe();
    let two = hub.subscribe();
    // As each stream's catch-up does: the id it caught up to.
    let caught = catch_up(&open().expect("reader"), None)
        .expect("caught up")
        .1;
    one.floor(caught);
    two.floor(caught);
    let stop = Arc::new(AtomicBool::new(false));
    let listening = {
        let (hub, stop, open) = (Arc::clone(&hub), Arc::clone(&stop), Arc::clone(&open));
        std::thread::spawn(move || listen(&hub, &*open, &|| stop.load(Ordering::Acquire)))
    };
    let new = append(&store, 2);
    let got = frame_with(&one, new, Duration::from_secs(10)).expect("the first stream");
    assert_eq!(got.event, "message.received");
    assert!(
        frame_with(&two, new, Duration::from_secs(10)).is_some(),
        "the second"
    );
    stop.store(true, Ordering::Release);
    listening.join().expect("the listener ends");
}

#[test]
fn a_whole_stream_over_http_replays_keeps_alive_and_goes_on_live() {
    let (_schema, store, open) = reader_schema("stream_http");
    let first = append(&store, 1);
    let second = append(&store, 2);
    let hub = Arc::new(Hub::new(64));
    let server = tiny_http::Server::http("127.0.0.1:0").expect("listen");
    let port = server.server_addr().to_ip().expect("ip").port();
    let serving = {
        let (hub, open) = (Arc::clone(&hub), Arc::clone(&open));
        std::thread::spawn(move || {
            let request = server.recv().expect("a request");
            serve_stream(
                request,
                &format!("after_event_id={first}"),
                &hub,
                &*open,
                Duration::from_millis(200),
            )
        })
    };
    let mut client = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    client
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    client
        .write_all(
            format!(
                "GET /api/stream?after_event_id={first} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"
            )
            .as_bytes(),
        )
        .expect("send");
    let mut seen = String::new();
    let mut buffer = [0u8; 4096];
    let mut read_until = |seen: &mut String, wanted: &str| {
        while !seen.contains(wanted) {
            let n = client.read(&mut buffer).expect("read");
            assert!(n > 0, "closed before {wanted:?}: {seen}");
            seen.push_str(&String::from_utf8_lossy(&buffer[..n]));
        }
    };
    read_until(&mut seen, ": keepalive\n\n");
    assert!(seen.starts_with("HTTP/1.1 200 OK\r\n"), "{seen}");
    assert!(seen.contains("Content-Type: text/event-stream"), "{seen}");
    assert!(seen.contains("retry: 3000\n\n"));
    assert!(seen.contains("event: ready\n"));
    assert!(seen.contains(&format!("id: {second}\nevent: message.received\ndata: ")));
    assert!(
        !seen.contains(&format!("id: {first}\n")),
        "nothing at or before the cursor"
    );
    // A live frame, and the replayed one again (from the listener's side),
    // which is not sent twice.
    hub.publish(&Arc::new(Frame {
        id: Some(second),
        event: "message.received".to_string(),
        data: "{}".to_string(),
    }));
    hub.publish(&Arc::new(Frame {
        id: Some(second + 1),
        event: "tool.started".to_string(),
        data: "{\"live\":true}".to_string(),
    }));
    read_until(&mut seen, "data: {\"live\":true}\n\n");
    assert_eq!(
        seen.matches(&format!("id: {second}\n")).count(),
        1,
        "{seen}"
    );
    drop(client);
    // The page gone, the next keepalive's write fails and the stream ends.
    let until = Instant::now() + Duration::from_secs(10);
    while !serving.is_finished() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(serving.is_finished(), "the stream ended with its page");
    serving.join().expect("joined").expect("no error");
    // Its channel closed with it: the next frame drops it from the hub.
    hub.publish(&Arc::new(Frame::note("x", &json!(0))));
    assert_eq!(hub.subscribers(), 0, "its channel is dropped with it");
}

#[test]
fn every_resync_has_one_shape() {
    for (frame, latest) in [
        (resync("behind", 3, Some(9)), json!(9)),
        (resync("slow_consumer", 3, None), Value::Null),
    ] {
        let data: Value = serde_json::from_str(&frame.data).expect("json");
        let mut keys: Vec<&str> = data
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["after_event_id", "latest_event_id", "reason"]);
        assert_eq!(data["latest_event_id"], latest);
        assert_eq!(frame.event, "resync");
    }
}

#[test]
fn a_gone_stream_is_pruned_at_once_not_at_the_next_frame() {
    let hub = Hub::new(4);
    let kept = hub.subscribe();
    let gone = hub.subscribe();
    assert_eq!(hub.subscribers(), 2);
    drop(gone);
    assert_eq!(hub.subscribers(), 1, "pruned without a publish");
    drop(kept);
    assert_eq!(hub.subscribers(), 0);
}

/// A writer that never takes a byte (a page that stopped reading).
struct Stuck;

impl Write for Stuck {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        std::thread::sleep(Duration::from_hours(1));
        Ok(0)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_write_a_page_never_takes_ends_its_stream_in_time() {
    let mut writer = TimedWriter::new(Box::new(Stuck), Duration::from_millis(200));
    let started = Instant::now();
    let error = writer.emit(": keepalive\n\n").expect_err("timed out");
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(2));
    // The writer is spent: the next write fails at once too.
    assert!(writer.emit("x").is_err());
    let mut fine = TimedWriter::new(Box::new(Vec::new()), Duration::from_secs(1));
    assert!(fine.emit("event: ready\n\n").is_ok());
}

#[test]
fn a_listener_that_panics_is_started_again_by_the_next_stream() {
    let hub: &'static Hub = Box::leak(Box::new(Hub::new(4)));
    let _stream = hub.subscribe();
    hub.start(|| -> Result<PostgresStore, String> { panic!("the listener's connect panics") });
    let until = Instant::now() + Duration::from_secs(10);
    while hub.started() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!hub.started(), "started is cleared when its thread ends");
    hub.start(|| Err("no database".to_string()));
    assert!(hub.started(), "the next stream starts it again");
}

/// Review H1: with nobody streaming, the listener holds no cursor; a page
/// that connects, reads `ready` and then sees an event appended must get
/// that event, every time (it was lost when the listener started from the
/// log's end, read after the event).
#[test]
fn after_an_idle_hub_a_new_stream_gets_its_first_live_event_every_round() {
    let (_schema, store, open) = reader_schema("stream_idle_rounds");
    append(&store, 0);
    let hub = Arc::new(Hub::new(64));
    let stop = Arc::new(AtomicBool::new(false));
    let listening = {
        let (hub, stop, open) = (Arc::clone(&hub), Arc::clone(&stop), Arc::clone(&open));
        std::thread::spawn(move || listen(&hub, &*open, &|| stop.load(Ordering::Acquire)))
    };
    for round in 1..=5 {
        // Idle: nobody streams, and the listener has seen that.
        let until = Instant::now() + Duration::from_secs(10);
        while hub.subscribers() > 0 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(hub.subscribers(), 0, "round {round}: idle");
        std::thread::sleep(Duration::from_millis(1200));
        let server = tiny_http::Server::http("127.0.0.1:0").expect("listen");
        let port = server.server_addr().to_ip().expect("ip").port();
        let serving = {
            let (hub, open) = (Arc::clone(&hub), Arc::clone(&open));
            std::thread::spawn(move || {
                let request = server.recv().expect("a request");
                serve_stream(request, "", &hub, &*open, Duration::from_millis(200))
            })
        };
        let mut client = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        client
            .write_all(
                format!("GET /api/stream HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes(),
            )
            .expect("send");
        let mut seen = String::new();
        let mut buffer = [0u8; 4096];
        let mut read_until = |seen: &mut String, wanted: &str| {
            let until = Instant::now() + Duration::from_secs(10);
            while !seen.contains(wanted) && Instant::now() < until {
                match client.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => seen.push_str(&String::from_utf8_lossy(&buffer[..n])),
                }
            }
            seen.contains(wanted)
        };
        assert!(
            read_until(&mut seen, "event: ready\n"),
            "round {round}: {seen}"
        );
        let new = append(&store, round);
        assert!(
            read_until(&mut seen, &format!("id: {new}\n")),
            "round {round}: the first live event {new} was not sent: {seen}"
        );
        drop(client);
        serving.join().expect("joined").expect("no error");
    }
    stop.store(true, Ordering::Release);
    listening.join().expect("the listener ends");
}

/// Review N1: a stream that hands its floor and leaves (a connect and
/// drop) takes its floor with it: the next stream's start is its own, never
/// the gone one's, so no spurious `stream_reset` follows it.
#[test]
fn a_gone_streams_floor_leaves_with_it() {
    let hub = Hub::new(8);
    let gone = hub.subscribe();
    gone.floor(5);
    assert_eq!(hub.start_floor(), Some(5));
    drop(gone);
    assert_eq!(hub.start_floor(), None, "nobody streams");
    let next = hub.subscribe();
    assert_eq!(hub.start_floor(), None, "its catch-up not read yet");
    next.floor(1000);
    assert_eq!(hub.start_floor(), Some(1000), "not the gone stream's 5");
}

/// Review N2: two streams attaching to an idle hub at once: the listener
/// starts only when both have handed their floors, from the lower, so the
/// event between the two catch-ups reaches the one that caught up first.
#[test]
fn concurrent_attaches_start_from_the_lowest_floor_once_all_are_handed() {
    let hub = Hub::new(8);
    let early = hub.subscribe();
    let late = hub.subscribe();
    late.floor(10);
    assert_eq!(
        hub.start_floor(),
        None,
        "the early one is still catching up"
    );
    early.floor(8);
    assert_eq!(hub.start_floor(), Some(8));
    drop(early);
    assert_eq!(hub.start_floor(), Some(10));
}

/// Review S1: `GET /api/stream?after=<id>` over HTTP replays the events
/// after it, as `?after_event_id=` does.
#[test]
fn a_stream_with_after_replays_the_events_after_it() {
    let (_schema, store, open) = reader_schema("stream_after");
    let ids: Vec<i64> = (1..=5).map(|n| append(&store, n)).collect();
    let hub = Arc::new(Hub::new(64));
    let server = tiny_http::Server::http("127.0.0.1:0").expect("listen");
    let port = server.server_addr().to_ip().expect("ip").port();
    let serving = {
        let (hub, open) = (Arc::clone(&hub), Arc::clone(&open));
        std::thread::spawn(move || {
            let request = server.recv().expect("a request");
            // As `handle` routes it: the query is the URL's.
            let query = request
                .url()
                .split_once('?')
                .map(|(_, query)| query.to_string())
                .unwrap_or_default();
            serve_stream(request, &query, &hub, &*open, Duration::from_millis(200))
        })
    };
    let mut client = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    client
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let third = ids[2];
    client
        .write_all(
            format!("GET /api/stream?after={third} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n")
                .as_bytes(),
        )
        .expect("send");
    let mut seen = String::new();
    let mut buffer = [0u8; 4096];
    while !seen.contains(": keepalive\n\n") {
        let n = client.read(&mut buffer).expect("read");
        assert!(n > 0, "closed early: {seen}");
        seen.push_str(&String::from_utf8_lossy(&buffer[..n]));
    }
    assert!(seen.contains("\"replayed\":2"), "{seen}");
    for id in &ids[..3] {
        assert!(
            !seen.contains(&format!("id: {id}\n")),
            "{id} is at or before the cursor: {seen}"
        );
    }
    let (fourth, fifth) = (ids[3], ids[4]);
    let (at4, at5) = (
        seen.find(&format!("id: {fourth}\n")).expect("the fourth"),
        seen.find(&format!("id: {fifth}\n")).expect("the fifth"),
    );
    assert!(at4 < at5, "in order: {seen}");
    drop(client);
    serving.join().expect("joined").expect("no error");
}
