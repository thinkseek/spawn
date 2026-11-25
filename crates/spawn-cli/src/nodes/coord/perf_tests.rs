//! Issue #96 P0 on this host's loopback: a coordinator's pumps flush at
//! once (no 15 ms poll tick: no round trip quantized to 16 ms steps), a
//! 64 KiB sealed exchange runs far past the old 1.3 MiB/s, a pooled
//! session and its kept prekey are reused (one handshake, one lookup) and
//! recover from a restarted coordinator, a rotated prekey and an idle
//! eviction; a frame over MAX_FRAME still ends a session, and a reader
//! that takes nothing slows no one else.

use super::pool::{CoordPool, ask_pooled};
use super::{
    Channel, ConnectConfig, CoordClient, CoordConfig, CoordUrl, HelloFields, Kind, MAX_FRAME,
    NodeService, ProbeContext, Running, Say, hello, run_connector, start_with,
};
use crate::nodes::crypto::{NodeIdentity, Responder, TrustPolicy};
use crate::nodes::relay::Runner;
use crate::nodes::ws::WsStream;
use serde_json::{Value, json};
use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TOKEN: &str = "mesh-token-96";
const T: Duration = Duration::from_secs(10);
/// What the old pump's poll cost each hop at least: a round trip was a
/// multiple of it.
const OLD_TICK: Duration = Duration::from_millis(16);
/// The old loopback rate of a 64 KiB sealed exchange.
const OLD_MIB_S: f64 = 1.3;

// The bounds of a 64 KiB exchange through the coordinator. Optimized, the
// strict ones (measured 32 to 83 MiB/s, p50 0.75 to 1.75 ms). Unoptimized,
// a raw exchange alone costs 12 to 17 ms of pure-Rust X25519 and more under
// a gate's concurrent load (overheads of 16.6 to 27.9 ms and 1.22 MiB/s
// seen there): bounds that still refuse the old pumps (47 ms added to the
// raw cost) without failing on a busy host.
/// The most the coordinator may add to a raw exchange's p50.
#[cfg(not(debug_assertions))]
const MAX_OVERHEAD: Duration = OLD_TICK;
#[cfg(debug_assertions)]
const MAX_OVERHEAD: Duration = Duration::from_millis(40);
/// Unoptimized, the coordinator's own parsing slows with the host's load as
/// the raw exchange does: the overhead allowed is at least this many times
/// the raw cost measured in the same run (the 40 ms of a quiet host, 12 to
/// 17 ms raw, still refuse the old 47 ms).
#[cfg(debug_assertions)]
const RAW_TIMES: u32 = 2;
#[cfg(not(debug_assertions))]
const RAW_TIMES: u32 = 0;
/// The least share of the raw rate kept through the coordinator (the old
/// pumps kept 1.2 %).
#[cfg(not(debug_assertions))]
const RAW_SHARE: f64 = 0.2;
#[cfg(debug_assertions)]
const RAW_SHARE: f64 = 0.1;
/// A lookup round trip's largest p50 (the old pumps': 16 ms and more).
#[cfg(not(debug_assertions))]
const MAX_LOOKUP_P50: Duration = Duration::from_millis(5);
#[cfg(debug_assertions)]
const MAX_LOOKUP_P50: Duration = Duration::from_millis(12);
/// How many batches a timing check may measure: it passes when one of them
/// is within its bounds (a busy host's bursts spoil a batch; the old pumps'
/// waits were in every one).
const BATCHES: usize = 3;

/// Run `batch` until it passes, at most [`BATCHES`] times: the last
/// failure said when none does.
fn within_batches(mut batch: impl FnMut() -> Result<(), String>) {
    let mut failures = Vec::new();
    for _ in 0..BATCHES {
        match batch() {
            Ok(()) => return,
            Err(why) => failures.push(why),
        }
    }
    panic!("no batch of {BATCHES} within its bounds: {failures:#?}");
}

/// The least rate through the coordinator.
#[cfg(not(debug_assertions))]
const MIB_S_FLOOR: f64 = 10.0 * OLD_MIB_S;
#[cfg(debug_assertions)]
const MIB_S_FLOOR: f64 = 0.25 * OLD_MIB_S;
const KIB_64: usize = 64 * 1024;
/// 64 KiB in MiB.
const KIB_64_IN_MIB: f64 = 1.0 / 16.0;

/// How long the test runner's `inventory` takes (a slow answer).
const SLOW: Duration = Duration::from_millis(600);

/// A runner whose `beat` says 64 KiB, and whose `inventory` takes
/// [`SLOW`].
fn runner() -> Runner {
    Arc::new(move |op: &str| match op {
        "beat" => Ok("b".repeat(KIB_64)),
        "inventory" => {
            std::thread::sleep(SLOW);
            Ok("@arch\nx86_64\n@end\n".to_string())
        }
        _ => Err("no such probe".to_string()),
    })
}

fn url(text: &str) -> CoordUrl {
    CoordUrl::parse(text).expect("a URL")
}

/// A coordinator on loopback at `ws` and `tcp` (port 0: the system's).
fn coordinator(ws: &str, tcp: &str) -> Running {
    let config = CoordConfig {
        ws: Some(ws.into()),
        tcp: Some(tcp.into()),
        ..CoordConfig::default()
    };
    start_with(&config, None).expect("a coordinator")
}

fn ws_of(running: &Running) -> CoordUrl {
    url(&format!("ws://{}/mesh", running.ws.expect("ws")))
}

fn tcp_of(running: &Running) -> CoordUrl {
    url(&format!("tcp://{}", running.tcp.expect("tcp")))
}

/// A connected node on a thread of its own, until dropped.
struct Node {
    identity: Arc<NodeIdentity>,
    stop: Arc<AtomicBool>,
    said: Arc<Mutex<Vec<String>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Node {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Node {
    fn start(at: &CoordUrl, name: &str) -> Self {
        let config = ConnectConfig {
            url: at.clone(),
            name: name.to_string(),
            role: "worker".to_string(),
            zone: None,
            key_file: None,
            udp_stun: None,
            register_to: None,
            register_store: false,
            note: None,
            coord_token: None,
            punch_loopback: false,
        };
        let identity = Arc::new(NodeIdentity::generate().expect("a key").0);
        let service = Arc::new(NodeService {
            responder: Responder::new(
                Arc::clone(&identity),
                TrustPolicy::new(Some(TOKEN), None, None),
            ),
            runner: runner(),
            registrar: None,
            in_flight: Arc::default(),
        });
        let stop = Arc::new(AtomicBool::new(false));
        let said = Arc::new(Mutex::new(Vec::new()));
        let thread = {
            let (identity, stop, said) =
                (Arc::clone(&identity), Arc::clone(&stop), Arc::clone(&said));
            let say: Say = Arc::new(move |line: String| said.lock().expect("said").push(line));
            std::thread::spawn(move || {
                let _ = run_connector(&config, &identity, &service, &stop, None, &say);
            })
        };
        let node = Self {
            identity,
            stop,
            said,
            thread: Some(thread),
        };
        node.wait_connected(1);
        node
    }

    /// Wait until the node said it connected `times` times.
    fn wait_connected(&self, times: usize) {
        let deadline = Instant::now() + T;
        loop {
            let count = self
                .said
                .lock()
                .expect("said")
                .iter()
                .filter(|line| line.contains(" connected to "))
                .count();
            if count >= times {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "never connected {times} times: {:?}",
                self.said.lock().expect("said")
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn context(coord_token: Option<&str>) -> ProbeContext {
    ProbeContext {
        identity: Arc::new(NodeIdentity::generate().expect("a key").0),
        policy: TrustPolicy::new(Some(TOKEN), None, None),
        coord_token: coord_token.map(str::to_string),
        tls: None,
        allow_loopback: true,
        punch: false,
    }
}

fn client_fields() -> HelloFields {
    HelloFields {
        kind: Kind::Client,
        name: "perf-client".to_string(),
        role: "client".to_string(),
        zone: None,
        udp_reflexive: None,
    }
}

fn client(at: &CoordUrl) -> CoordClient {
    CoordClient::connect(
        at,
        Arc::new(NodeIdentity::generate().expect("a key").0),
        TrustPolicy::new(Some(TOKEN), None, None),
        &client_fields(),
        None,
        T,
        None,
    )
    .expect("a client")
}

fn pooled_beat(pool: &CoordPool, at: &CoordUrl, node: &Node, context: &ProbeContext) -> Value {
    let probed = ask_pooled(
        pool,
        at,
        Some(node.identity.node_id()),
        None,
        None,
        &json!({"op": "beat"}),
        T,
        context,
    )
    .expect("probed");
    assert_eq!(probed.mode, "relay");
    assert_eq!(probed.reply["ok"], true, "{}", probed.reply);
    probed.peer
}

fn median(samples: &mut [Duration]) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

/// Round trips that fell near a multiple of the old tick (16, 32, 48 ms
/// and on): what a polling pump makes of every exchange.
fn on_old_ticks(samples: &[Duration]) -> usize {
    let tick = OLD_TICK.as_secs_f64() * 1000.0;
    samples
        .iter()
        .filter(|sample| {
            let ms = sample.as_secs_f64() * 1000.0;
            ms >= tick - 1.0 && {
                let off = ms % tick;
                !(1.5..=tick - 1.5).contains(&off)
            }
        })
        .count()
}

#[test]
fn a_lookup_round_trip_is_flushed_at_once_not_on_a_poll_tick() {
    let running = coordinator("127.0.0.1:0", "127.0.0.1:0");
    for at in [ws_of(&running), tcp_of(&running)] {
        let _node = Node::start(&at, "quick");
        let client = client(&at);
        within_batches(|| {
            let mut samples = Vec::new();
            for _ in 0..60 {
                let started = Instant::now();
                let found = client
                    .lookup(None, Some("quick"), T)
                    .expect("a lookup")
                    .expect("found");
                samples.push(started.elapsed());
                assert_eq!(found["name"], "quick");
            }
            let ticked = on_old_ticks(&samples);
            let p50 = median(&mut samples);
            if p50 >= MAX_LOOKUP_P50 {
                return Err(format!("{at}: a lookup's p50 is {p50:?}: {samples:?}"));
            }
            if ticked * 10 > samples.len() {
                return Err(format!(
                    "{at}: {ticked} round trips on the old 16 ms ticks: {samples:?}"
                ));
            }
            Ok(())
        });
        client.close();
    }
    running.stop();
}

/// The p50 of one 64 KiB sealed exchange with no coordinator (the raw
/// E2E cost: the request sealed and opened, the 64 KiB reply sealed and
/// opened), on this build and this host's load.
fn raw_e2e_p50(rounds: usize) -> Duration {
    use crate::nodes::crypto::{PeerKeys, seal_request};
    let node = Arc::new(NodeIdentity::generate().expect("a key").0);
    let me = NodeIdentity::generate().expect("a key").0;
    let peer = PeerKeys::verify(&node.public_info(), None, None).expect("a peer");
    let policy = TrustPolicy::new(Some(TOKEN), None, None);
    let responder = Responder::new(node, policy.clone());
    let output = runner();
    let mut samples = Vec::new();
    for index in 0..rounds {
        let started = Instant::now();
        let (envelope, key) = seal_request(
            &me,
            &peer,
            &policy,
            &format!("raw{index}"),
            &json!({"op": "beat"}),
        )
        .expect("sealed");
        let opened = responder.open(&envelope).expect("opened");
        let reply = opened
            .reply
            .seal(&json!({"ok": true, "output": output("beat").expect("an output")}))
            .expect("a sealed reply");
        key.open(&reply).expect("an opened reply");
        samples.push(started.elapsed());
    }
    median(&mut samples)
}

#[test]
fn repeated_64_kib_sealed_exchanges_beat_the_old_poll_bottleneck() {
    let running = coordinator("127.0.0.1:0", "127.0.0.1:0");
    for at in [ws_of(&running), tcp_of(&running)] {
        let node = Node::start(&at, "bulk");
        let pool = CoordPool::default();
        let context = context(None);
        // The first one connects and looks up; the rest are measured.
        pooled_beat(&pool, &at, &node, &context);
        within_batches(|| {
            let mut samples = Vec::new();
            for _ in 0..24 {
                let one = Instant::now();
                pooled_beat(&pool, &at, &node, &context);
                samples.push(one.elapsed());
            }
            let p50 = median(&mut samples);
            // The rate as the baseline said it: 64 KiB over the p50 (1.3
            // MiB/s, 48 ms), a busy host's stragglers left out.
            let mib_s = KIB_64_IN_MIB / p50.as_secs_f64();
            let raw = raw_e2e_p50(8);
            // What the coordinator adds to the raw E2E cost: its two hops'
            // framing and parsing. The old pumps added 47 ms of it (1.17
            // ms raw, 48 ms through the coordinator), all of it waiting on
            // their 15 ms polls: three hops, each up to a tick.
            let overhead = p50.saturating_sub(raw);
            let max_overhead = MAX_OVERHEAD.max(raw.saturating_mul(RAW_TIMES));
            if overhead >= max_overhead {
                return Err(format!(
                    "{at}: the coordinator adds {overhead:?} (at most {max_overhead:?}) to a \
                     raw {raw:?} (p50 {p50:?}): {samples:?}"
                ));
            }
            // The old rate was 1.2 % of the raw one (1.25 of 105 MiB/s).
            let raw_mib_s = KIB_64_IN_MIB / raw.as_secs_f64();
            if mib_s <= RAW_SHARE * raw_mib_s {
                return Err(format!(
                    "{at}: {mib_s:.2} MiB/s through the coordinator, {raw_mib_s:.2} raw: \
                     {samples:?}"
                ));
            }
            // Past the old bottleneck: ten times over when optimized; an
            // unoptimized build's rate is its crypto's, and only kept from
            // collapsing (the overhead above is its measure).
            if mib_s <= MIB_S_FLOOR {
                return Err(format!(
                    "{at}: {mib_s:.2} MiB/s, not past {MIB_S_FLOOR:.2} MiB/s (the old \
                     {OLD_MIB_S}): {samples:?}"
                ));
            }
            Ok(())
        });
        assert_eq!(pool.connects(), 1, "{at}: one handshake");
        assert_eq!(pool.lookups(), 1, "{at}: one lookup");
    }
    running.stop();
}

#[test]
fn a_pooled_session_is_reused_and_recovers_from_a_restart_and_a_rotated_prekey() {
    let running = coordinator("127.0.0.1:0", "127.0.0.1:0");
    let (ws, tcp) = (running.ws.expect("ws"), running.tcp.expect("tcp"));
    let at = ws_of(&running);
    let node = Node::start(&at, "rotor");
    let pool = CoordPool::default();
    let context = context(None);
    for _ in 0..5 {
        pooled_beat(&pool, &at, &node, &context);
    }
    assert_eq!(pool.connects(), 1, "five probes, one handshake");
    assert_eq!(pool.lookups(), 1, "five probes, one lookup");
    assert_eq!(pool.live(), 1);

    // The node rotates its prekey twice (the one the pool keeps is no
    // longer opened) and the coordinator restarts: the pooled session
    // closed, the kept prekey stale.
    let old_epoch = node.identity.bundle().epoch;
    node.identity.rotate().expect("rotated");
    node.identity.rotate().expect("rotated");
    let epoch = node.identity.bundle().epoch;
    assert!(epoch > old_epoch);
    running.stop();
    let running = coordinator(&ws.to_string(), &tcp.to_string());
    node.wait_connected(2);
    let deadline = Instant::now() + T;
    while pool.live() > 0 {
        assert!(Instant::now() < deadline, "the closed session stays live");
        std::thread::sleep(Duration::from_millis(10));
    }
    let peer = pooled_beat(&pool, &at, &node, &context);
    assert_eq!(peer["epoch"], epoch, "the new prekey's record");
    assert_eq!(pool.connects(), 2, "connected again once");
    assert_eq!(pool.lookups(), 2, "looked up again once");
    // And reused again from there.
    pooled_beat(&pool, &at, &node, &context);
    assert_eq!((pool.connects(), pool.lookups()), (2, 2));

    // Another coordinator token: a session of its own, never the other's.
    let other = self::context(Some("another-coord-token"));
    pooled_beat(&pool, &at, &node, &other);
    assert_eq!(pool.connects(), 3);
    running.stop();
}

#[test]
fn a_pooled_session_idle_past_its_limit_is_closed_and_made_again() {
    let running = coordinator("127.0.0.1:0", "127.0.0.1:0");
    let at = tcp_of(&running);
    let node = Node::start(&at, "sleepy");
    let pool = CoordPool::with_limits(
        Duration::from_millis(300),
        Duration::from_mins(30),
        Duration::from_mins(5),
    );
    let context = context(None);
    pooled_beat(&pool, &at, &node, &context);
    pooled_beat(&pool, &at, &node, &context);
    assert_eq!(pool.connects(), 1);
    let deadline = Instant::now() + T;
    while pool.live() > 0 {
        assert!(Instant::now() < deadline, "an idle session is never closed");
        std::thread::sleep(Duration::from_millis(20));
    }
    pooled_beat(&pool, &at, &node, &context);
    assert_eq!(pool.connects(), 2, "connected again after its eviction");
    assert_eq!(pool.lookups(), 1, "its prekey still kept");
    // An answer slower than the idle limit: the session waiting for it is
    // not idle, never closed under its ask.
    let slow = ask_pooled(
        &pool,
        &at,
        Some(node.identity.node_id()),
        None,
        None,
        &json!({"op": "inventory"}),
        T,
        &context,
    )
    .expect("a slow answer");
    assert_eq!(slow.reply["ok"], true);
    assert_eq!(pool.connects(), 2, "the session asking was kept open");
    // A prekey past its time is looked up again.
    let pool = CoordPool::with_limits(T, T, Duration::from_millis(1));
    pooled_beat(&pool, &at, &node, &context);
    std::thread::sleep(Duration::from_millis(5));
    pooled_beat(&pool, &at, &node, &context);
    assert_eq!((pool.connects(), pool.lookups()), (1, 2));
    running.stop();
}

/// A raw session that said its hello and got its welcome.
fn hello_line(running: &Running) -> TcpStream {
    let mut stream = TcpStream::connect(running.tcp.expect("tcp")).expect("connected");
    let identity = NodeIdentity::generate().expect("a key").0;
    let mut line = hello(&identity, &client_fields(), None).to_string();
    line.push('\n');
    stream.write_all(line.as_bytes()).expect("hello");
    stream.set_read_timeout(Some(T)).expect("a read timeout");
    let mut welcome = Vec::new();
    let mut byte = [0_u8; 1];
    while welcome.last() != Some(&b'\n') {
        assert_eq!(stream.read(&mut byte).expect("the welcome"), 1);
        welcome.push(byte[0]);
    }
    assert!(String::from_utf8_lossy(&welcome).contains("welcome"));
    stream
}

/// Whether the coordinator closed `stream` within `T` (what it sent
/// before skipped).
fn closed(stream: &mut TcpStream) -> bool {
    let deadline = Instant::now() + T;
    let mut sink = [0_u8; 4096];
    while Instant::now() < deadline {
        match stream.read(&mut sink) {
            Ok(0) | Err(_) => return true,
            Ok(_) => {}
        }
    }
    false
}

#[test]
fn a_frame_over_max_frame_ends_a_session_after_its_hello() {
    let running = coordinator("127.0.0.1:0", "127.0.0.1:0");
    // A line that never ends, past MAX_FRAME.
    let mut stream = hello_line(&running);
    stream.write_all(b"{\"type\":\"ping\"}\n").expect("a ping");
    let _ = stream.write_all(&vec![b'a'; MAX_FRAME + 2]);
    assert!(closed(&mut stream), "an endless line was taken");
    // A line that ends, one byte past MAX_FRAME.
    let mut stream = hello_line(&running);
    let mut long = vec![b' '; MAX_FRAME + 1];
    long.push(b'\n');
    let _ = stream.write_all(&long);
    assert!(closed(&mut stream), "a line past MAX_FRAME was taken");
    // A WebSocket frame that says it is past MAX_FRAME.
    let mut ws = WsStream::connect(&ws_of(&running), None, T, None).expect("a WebSocket");
    let identity = NodeIdentity::generate().expect("a key").0;
    ws.send_text(&hello(&identity, &client_fields(), None).to_string())
        .expect("hello");
    let welcome = ws.recv(T).expect("a welcome").expect("a message");
    assert!(format!("{welcome:?}").contains("welcome"));
    let mut huge = vec![0x81, 0xFF];
    huge.extend_from_slice(&(u64::try_from(MAX_FRAME).expect("len") + 1).to_be_bytes());
    huge.extend_from_slice(&[1, 2, 3, 4]);
    ws.raw(&huge).expect("a frame head");
    let deadline = Instant::now() + T;
    loop {
        match ws.recv(Duration::from_millis(200)) {
            Err(_) | Ok(Some(crate::nodes::ws::Message::Close)) => break,
            Ok(_) => assert!(
                Instant::now() < deadline,
                "a frame past MAX_FRAME was taken"
            ),
        }
    }
    // A client refuses one from its coordinator alike.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a listener");
    let fake = url(&format!("tcp://{}", listener.local_addr().expect("addr")));
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accepted");
        let mut buffer = [0_u8; 4096];
        let _ = stream.read(&mut buffer);
        stream
            .write_all(b"{\"type\":\"welcome\"}\n")
            .expect("a welcome");
        let _ = stream.write_all(&vec![b'a'; MAX_FRAME + 2]);
        std::thread::sleep(Duration::from_millis(500));
    });
    let client = client(&fake);
    let deadline = Instant::now() + T;
    while client.alive() {
        assert!(
            Instant::now() < deadline,
            "a client took a frame past MAX_FRAME"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = server.join();
    running.stop();
}

#[test]
fn a_reader_that_takes_nothing_slows_no_other_session() {
    let running = coordinator("127.0.0.1:0", "127.0.0.1:0");
    let at = ws_of(&running);
    // The slow reader: a session that never reads what it is sent.
    let slow_identity = NodeIdentity::generate().expect("a key").0;
    let mut slow = TcpStream::connect(running.tcp.expect("tcp")).expect("connected");
    let mut line = hello(&slow_identity, &client_fields(), None).to_string();
    line.push('\n');
    slow.write_all(line.as_bytes()).expect("hello");
    // The flooder: relays of 32 KiB to it, far past its queue and its
    // socket's buffers.
    let flooder = client(&at);
    let measured = client(&at);
    let _node = Node::start(&at, "steady");
    let pad = "p".repeat(32 * 1024);
    let flood = {
        let from = flooder.identity().node_id().to_string();
        let to = slow_identity.node_id().to_string();
        let flooder = &flooder;
        move || {
            for index in 0..600 {
                flooder.send(json!({"type": "relay", "from": from, "to": to,
                                    "id": format!("f{index}"),
                                    "envelope": {"v": "flood", "pad": pad}}));
                if index % 32 == 0 {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
    };
    let mut samples = Vec::new();
    std::thread::scope(|scope| {
        let flooding = scope.spawn(flood);
        for _ in 0..40 {
            let started = Instant::now();
            let found = measured
                .lookup(None, Some("steady"), T)
                .expect("a lookup")
                .expect("found");
            samples.push(started.elapsed());
            assert_eq!(found["name"], "steady");
        }
        flooding.join().expect("flooded");
    });
    let p50 = median(&mut samples);
    assert!(
        p50 < Duration::from_millis(25),
        "a flooded slow reader slowed another session: p50 {p50:?}: {samples:?}"
    );
    // The flooder and the coordinator still answer.
    assert!(flooder.alive() && measured.alive());
    // (Its own first ask may be dropped: its queue, bounded, is still full
    // of its flood.)
    let deadline = Instant::now() + T;
    while flooder
        .lookup(None, Some("steady"), Duration::from_millis(500))
        .ok()
        .flatten()
        .is_none()
    {
        assert!(Instant::now() < deadline, "the flooder is never answered");
    }
    drop(slow);
    flooder.close();
    measured.close();
    running.stop();
}

#[test]
fn a_line_channel_takes_whole_messages_without_io_and_refuses_past_max_frame() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a listener");
    let address = listener.local_addr().expect("addr");
    let writer = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accepted");
        stream
            .write_all(b"{\"a\":1}\n{\"b\":2}\n{\"c\"")
            .expect("written");
        std::thread::sleep(Duration::from_millis(300));
    });
    let mut channel = Channel::Line {
        stream: TcpStream::connect(address).expect("connected"),
        buffer: Vec::new(),
    };
    let deadline = Instant::now() + T;
    // All 20 bytes written.
    while buffered(&channel) < 20 {
        assert!(Instant::now() < deadline, "nothing read");
        channel.read_ready().expect("read");
    }
    assert_eq!(channel.next_buffered().expect("a").expect("whole")["a"], 1);
    assert_eq!(channel.next_buffered().expect("b").expect("whole")["b"], 2);
    assert!(channel.next_buffered().expect("no whole one").is_none());
    let _ = writer.join();
    let Channel::Line { buffer, .. } = &mut channel else {
        unreachable!()
    };
    buffer.clear();
    buffer.extend(std::iter::repeat_n(b'x', MAX_FRAME + 1));
    assert!(channel.next_buffered().is_err());
}

fn buffered(channel: &Channel) -> usize {
    match channel {
        Channel::Line { buffer, .. } => buffer.len(),
        Channel::Ws(_) => 0,
    }
}

/// Review of #569 (F1): a client's pump over `wss://` delivers the answer
/// that comes in the same flight as its coordinator's TLS close_notify
/// (it used to drop it and fail the ask), then sees the connection end.
#[test]
fn a_wss_client_takes_the_answer_that_comes_with_close_notify() {
    use crate::nodes::ws::tests::{
        TEST_CA, accept_tls_upgrade, read_client_frame, send_then_close_notify, server_frame,
    };
    for round in 0..5 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
        let port = listener.local_addr().expect("addr").port();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("a connection");
            let mut tls = accept_tls_upgrade(stream);
            let hello: Value = serde_json::from_slice(&read_client_frame(&mut tls)).expect("json");
            assert_eq!(hello["type"], "hello");
            let welcome = json!({"type": "welcome", "v": super::PROTOCOL}).to_string();
            tls.write_all(&server_frame(0x1, welcome.as_bytes()))
                .expect("a welcome");
            tls.flush().expect("flushed");
            // A ping or two may come first; the lookup is answered with the
            // close in one flight.
            let lookup = loop {
                let message: Value =
                    serde_json::from_slice(&read_client_frame(&mut tls)).expect("json");
                if message["type"] == "lookup" {
                    break message;
                }
            };
            let answer = json!({"type": "peer_info", "id": lookup["id"], "found": false});
            send_then_close_notify(&mut tls, &server_frame(0x1, answer.to_string().as_bytes()));
            std::thread::sleep(Duration::from_millis(300));
        });
        let client = CoordClient::connect(
            &url(&format!("wss://localhost:{port}/mesh")),
            Arc::new(NodeIdentity::generate().expect("a key").0),
            TrustPolicy::new(Some(TOKEN), None, None),
            &client_fields(),
            None,
            T,
            Some(crate::nodes::ws::tls_config_with(TEST_CA.as_bytes()).expect("a TLS config")),
        )
        .expect("a wss client");
        assert_eq!(
            client.lookup(None, Some("anyone"), T),
            Ok(None),
            "round {round}: the answer sent with close_notify"
        );
        let deadline = Instant::now() + T;
        while client.alive() {
            assert!(
                Instant::now() < deadline,
                "the closed connection stays alive"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        server.join().expect("the server");
    }
}
