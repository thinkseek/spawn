//! The tcp and udp transports on this host's loopback: probes answered
//! directly and through chains of relays (tcp and udp mixed, signed),
//! replies naming another request refused, a forward's bounds, its inner
//! request rebuilt and its hops checked, each nonce run once (udp resends
//! coalesced: no fan-out to a dead hop, no registration twice), the slots
//! per peer, and a node's own registration taken directly, relayed, once,
//! only signed, and into a real store.

use super::{
    Admit, MAX_ACTIVE, NO_ANSWER, PER_PEER, REQUEST_TIMEOUT, Registrar, Relay, Runner, Slots,
    inner_request, listen, registration,
};
use crate::nodes::auth::sign;
use crate::nodes::commands::{Registration, register_once};
use crate::nodes::transport::{
    Endpoint, MAX_HOPS, Op, Route, Transport, exchange, nonce, probe_route, request, route,
};
use crate::nodes::{beat, inventory};
use crate::verify::now_ms;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const INVENTORY_OUT: &str = "@arch\nx86_64\n@os\nGNU/Linux\n@kernel\n6.1.0\n@cores\n4\n\
@cpu_max\n@meminfo\nMemTotal:  8000000 kB\n@mem_max\nmax\n@df\n\
Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 100 50 50 50% /\n@gpu\n@end\n";
const BEAT_OUT: &str = "@loadavg\n0.25 0.1 0.1 1/1 1\n@meminfo\nMemAvailable: 4000000 kB\n\
@df\nFilesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 100 50 50 50% /\n@end\n";
const TIMEOUT: Duration = Duration::from_secs(10);
const TOKEN: &str = "s3cret-fleet-token";

/// A node's probes as fixed outputs, its name in the kernel's place (so a
/// test sees which node of a chain answered).
fn runner(name: &str) -> Runner {
    let name = name.to_string();
    Arc::new(move |op: &str| match op {
        "inventory" => Ok(INVENTORY_OUT.replace("6.1.0", &name)),
        "beat" => Ok(BEAT_OUT.to_string()),
        _ => Err("no such probe".to_string()),
    })
}

/// A node with no token, forwarding to loopback (all of these tests' hops).
fn node(name: &str, binds: &[&str]) -> (super::Listening, Vec<Endpoint>) {
    node_with(Relay::new(runner(name), None, None, true), binds)
}

fn node_with(relay: Relay, binds: &[&str]) -> (super::Listening, Vec<Endpoint>) {
    let binds: Vec<Endpoint> = binds
        .iter()
        .map(|bind| Endpoint::bind(bind, None).expect("a bind"))
        .collect();
    let listening = listen(&Arc::new(relay), &binds).expect("listening");
    let endpoints = listening.endpoints.clone();
    (listening, endpoints)
}

fn signed(mut message: Value, token: Option<&str>) -> Value {
    sign(&mut message, token);
    message
}

fn error_of(reply: &Value) -> &str {
    reply["error"].as_str().unwrap_or_default()
}

#[test]
fn a_node_answers_its_probes_over_tcp_and_udp_on_one_port_each() {
    let (listening, endpoints) = node("edge", &["tcp://127.0.0.1:0", "udp://127.0.0.1:0"]);
    for endpoint in &endpoints {
        let route = Route::Net(vec![endpoint.clone()]);
        let output = probe_route(&route, Op::Beat, TIMEOUT, None).expect("an answer");
        assert!(
            (beat(&output).expect("a beat").load1 - 0.25).abs() < 1e-9,
            "{endpoint}"
        );
        let output = probe_route(&route, Op::Inventory, TIMEOUT, None).expect("an answer");
        assert_eq!(inventory(&output).expect("an inventory")["kernel"], "edge");
    }
    listening.stop();
}

#[test]
fn a_signed_probe_goes_through_a_chain_of_relays_tcp_and_udp_mixed() {
    let signing = |name: &str, bind: &str| {
        node_with(
            Relay::new(runner(name), None, Some(TOKEN.into()), true),
            &[bind],
        )
    };
    let (a, a_at) = signing("a", "tcp://127.0.0.1:0");
    let (b, b_at) = signing("b", "udp://127.0.0.1:0");
    let (c, c_at) = signing("c", "tcp://127.0.0.1:0");
    let (d, d_at) = signing("d", "udp://127.0.0.1:0");
    // As the heartbeat resolves it: d joined via a, b and c.
    let members = json!([
        {"name": "a", "transport": "tcp", "endpoint": a_at[0].url(), "via": []},
        {"name": "b", "transport": "udp", "endpoint": b_at[0].url(), "via": ["a"]},
        {"name": "c", "transport": "tcp", "endpoint": c_at[0].url(), "via": ["a", "b"]},
    ]);
    let hops: Vec<String> = ["a", "b", "c"].iter().map(ToString::to_string).collect();
    let way = route(Transport::Udp, &d_at[0].url(), &hops, &members).expect("a route");
    let output = probe_route(&way, Op::Inventory, TIMEOUT, Some(TOKEN))
        .expect("an answer through three relays");
    assert_eq!(
        inventory(&output).expect("an inventory")["kernel"],
        "d",
        "the last node answered"
    );
    assert!(beat(&probe_route(&way, Op::Beat, TIMEOUT, Some(TOKEN)).expect("a beat")).is_ok());
    // Unsigned, or signed with another token: refused at the first relay.
    for token in [None, Some("another")] {
        let error = probe_route(&way, Op::Beat, TIMEOUT, token).expect_err("unsigned");
        assert!(error.contains("auth"), "{error}");
    }
    // A hop that does not answer: the probe fails within its bound.
    let dead = UdpSocket::bind("127.0.0.1:0").expect("a port");
    let silent = Endpoint::parse(&format!("udp://{}", dead.local_addr().expect("addr")), None)
        .expect("an endpoint");
    let begun = Instant::now();
    let way = Route::Net(vec![a_at[0].clone(), silent, d_at[0].clone()]);
    assert!(probe_route(&way, Op::Beat, Duration::from_secs(2), Some(TOKEN)).is_err());
    assert!(
        begun.elapsed() < Duration::from_secs(4),
        "{:?}",
        begun.elapsed()
    );
    for node in [a, b, c, d] {
        node.stop();
    }
}

/// Issue #85 review: a probe through two udp relays to a dead udp port
/// sends it a bounded number of datagrams (each relay's resends of one
/// nonce coalesced, never a loop of its own each), and the second relay
/// answers others all along.
#[test]
fn udp_resends_through_relays_to_a_dead_port_are_coalesced_not_multiplied() {
    let (r1, r1_at) = node("r1", &["udp://127.0.0.1:0"]);
    let (r2, r2_at) = node("r2", &["udp://127.0.0.1:0"]);
    let dead = UdpSocket::bind("127.0.0.1:0").expect("a port");
    let dead_at = Endpoint::parse(&format!("udp://{}", dead.local_addr().expect("addr")), None)
        .expect("an endpoint");
    dead.set_read_timeout(Some(Duration::from_millis(100)))
        .expect("a timeout");
    let done = Arc::new(AtomicBool::new(false));
    let counted = {
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            let mut buffer = vec![0_u8; 65_536];
            let mut count = 0_usize;
            while !done.load(Ordering::Relaxed) {
                if dead.recv_from(&mut buffer).is_ok() {
                    count += 1;
                }
            }
            count
        })
    };
    let healthy = {
        let done = Arc::clone(&done);
        let r2 = Route::Net(vec![r2_at[0].clone()]);
        std::thread::spawn(move || {
            let mut answers = 0;
            while !done.load(Ordering::Relaxed) {
                probe_route(&r2, Op::Beat, Duration::from_secs(2), None)
                    .expect("r2 answers all along");
                answers += 1;
                std::thread::sleep(Duration::from_millis(100));
            }
            answers
        })
    };
    let way = Route::Net(vec![r1_at[0].clone(), r2_at[0].clone(), dead_at]);
    let begun = Instant::now();
    assert!(probe_route(&way, Op::Beat, Duration::from_secs(5), None).is_err());
    assert!(begun.elapsed() < Duration::from_secs(7));
    std::thread::sleep(Duration::from_millis(500));
    done.store(true, Ordering::Relaxed);
    let count = counted.join().expect("the dead port");
    assert!(
        (1..=25).contains(&count),
        "{count} datagrams to the dead port"
    );
    assert!(healthy.join().expect("the healthy probes") >= 10);
    r1.stop();
    r2.stop();
}

#[test]
fn a_udp_reply_naming_another_request_is_dropped_and_the_wait_goes_on() {
    let socket = UdpSocket::bind("127.0.0.1:0").expect("a port");
    let at = Endpoint::parse(
        &format!("udp://{}", socket.local_addr().expect("addr")),
        None,
    )
    .expect("an endpoint");
    let spoofer = std::thread::spawn(move || {
        let mut buffer = vec![0_u8; 65_536];
        let (read, peer) = socket.recv_from(&mut buffer).expect("a request");
        let request: Value = serde_json::from_slice(&buffer[..read]).expect("json");
        // A spoofed reply first, then garbage, then the real one.
        for reply in [
            json!({"nonce": nonce(), "ok": true, "output": "spoofed"}),
            json!("not an object"),
            json!({"nonce": request["nonce"], "ok": true, "output": "real"}),
        ] {
            socket
                .send_to(&serde_json::to_vec(&reply).expect("bytes"), peer)
                .expect("sent");
        }
        // Then only spoofed replies to the next request.
        let (_, peer) = socket.recv_from(&mut buffer).expect("a request");
        for _ in 0..3 {
            let reply = json!({"nonce": nonce(), "ok": true, "output": "spoofed"});
            let _ = socket.send_to(&serde_json::to_vec(&reply).expect("bytes"), peer);
        }
        // Open past the wait, so the port never refuses a datagram sent again.
        std::thread::sleep(Duration::from_millis(1500));
    });
    let reply =
        exchange(&at, &json!({"nonce": nonce(), "op": "beat"}), TIMEOUT).expect("the reply");
    assert_eq!(reply["output"], "real");
    let begun = Instant::now();
    let error = exchange(
        &at,
        &json!({"nonce": nonce(), "op": "beat"}),
        Duration::from_secs(1),
    )
    .expect_err("only spoofed replies");
    assert!(error.contains("no answer in time"), "{error}");
    assert!(begun.elapsed() < Duration::from_secs(3));
    spoofer.join().expect("the spoofer");
}

#[test]
fn a_tcp_reply_naming_another_request_or_too_long_is_refused() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let at = Endpoint::parse(
        &format!("tcp://{}", listener.local_addr().expect("addr")),
        None,
    )
    .expect("an endpoint");
    let server = std::thread::spawn(move || {
        for reply in [
            serde_json::to_vec(&json!({"nonce": nonce(), "ok": true})).expect("bytes"),
            vec![b'x'; crate::nodes::transport::TCP_FRAME + 10],
        ] {
            let (stream, _) = listener.accept().expect("a connection");
            let mut line = String::new();
            BufReader::new(&stream)
                .read_line(&mut line)
                .expect("a request");
            let mut stream = stream;
            let _ = stream.write_all(&reply);
            let _ = stream.write_all(b"\n");
        }
    });
    let error = exchange(&at, &json!({"nonce": nonce(), "op": "beat"}), TIMEOUT)
        .expect_err("another request's reply");
    assert!(error.contains("answered another request"), "{error}");
    let error =
        exchange(&at, &json!({"nonce": nonce(), "op": "beat"}), TIMEOUT).expect_err("too long");
    assert!(error.contains("a message over"), "{error}");
    server.join().expect("the server");
    assert!(
        exchange(&at, &json!({"op": "beat"}), TIMEOUT).is_err(),
        "no nonce, nothing sent"
    );
}

fn envelope(hops: &[&str], ttl: usize, inner: Value, timeout_ms: u64) -> Value {
    let mut envelope = json!({"nonce": nonce(), "op": "forward", "ttl": ttl, "hops": hops,
                              "timeout_ms": timeout_ms});
    envelope["inner"] = inner;
    envelope
}

#[test]
fn a_forward_past_its_bounds_or_carrying_anything_but_a_probe_is_refused() {
    let relay = Arc::new(Relay::new(runner("r"), None, None, false));
    let inner = || json!({"nonce": nonce(), "op": "beat"});
    let many: Vec<String> = (0..=MAX_HOPS).map(|i| format!("tcp://h{i}:1")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    let mut extra = envelope(&["tcp://h:1"], 1, inner(), 5000);
    extra["inner"]["script"] = json!("reboot");
    let mut outer_extra = envelope(&["tcp://h:1"], 1, inner(), 5000);
    outer_extra["exec"] = json!("x");
    for (message, why) in [
        (envelope(&many, MAX_HOPS + 1, inner(), 5000), "1 to 8 hops"),
        (envelope(&many[..3], 2, inner(), 5000), "within its ttl"),
        (envelope(&many[..1], 9, inner(), 5000), "within its ttl"),
        (envelope(&[], 1, inner(), 5000), "1 to 8 hops"),
        (
            envelope(&["tcp://h:1", "tcp://h:1"], 2, inner(), 5000),
            "twice",
        ),
        (
            envelope(&["ssh://h:22"], 1, inner(), 5000),
            "a hop of the forward",
        ),
        (envelope(&["h:1"], 1, inner(), 5000), "a hop of the forward"),
        (
            envelope(&["tcp://h:1"], 1, json!({"op": "beat"}), 5000),
            "no nonce",
        ),
        (
            envelope(
                &["tcp://h:1"],
                1,
                json!({"nonce": nonce(), "op": "forward"}),
                5000,
            ),
            "a probe or a register only",
        ),
        (
            envelope(
                &["tcp://h:1"],
                1,
                json!({"nonce": nonce(), "op": "sh"}),
                5000,
            ),
            "a probe or a register only",
        ),
        (extra, "has no field \"script\""),
        (outer_extra, "has no field \"exec\""),
        (envelope(&["tcp://h:1"], 1, inner(), 600_000), "timeout_ms"),
        (json!({"op": "beat"}), "without a nonce"),
        (json!({"nonce": nonce(), "op": "exec"}), "no such op"),
    ] {
        let reply = relay.handle(&message);
        assert_eq!(reply["ok"], false, "{why}: {reply}");
        assert!(error_of(&reply).contains(why), "{why}: {reply}");
    }
    // The inner request is rebuilt from its own fields, each of its type.
    let probe = json!({"op": "beat", "nonce": nonce(), "at_ms": 5, "auth": "ab"});
    assert_eq!(inner_request(&probe), Ok(probe.clone()));
    assert!(inner_request(&json!({"op": "beat", "nonce": nonce(), "at_ms": "5"})).is_err());
    let register = json!({"op": "register", "nonce": nonce(), "name": "e", "via": ["gw"],
                          "note": null, "inventory": "@end\n"});
    let rebuilt = inner_request(&register).expect("a register");
    assert!(rebuilt.get("note").is_none(), "{rebuilt}");
    assert!(inner_request(&json!({"op": "register", "nonce": nonce(), "via": [1]})).is_err());
}

#[test]
fn a_relay_never_forwards_to_its_own_host_or_link_local_and_says_no_more_than_no_answer() {
    let refusing = Arc::new(Relay::new(runner("r"), None, None, false));
    for hop in [
        "tcp://127.0.0.1:9",
        "tcp://127.8.9.10:9",
        "udp://[::1]:9",
        "tcp://[::ffff:127.0.0.1]:9",
        "tcp://[::ffff:169.254.169.254]:80",
        "tcp://169.254.169.254:80",
        "tcp://0.0.0.0:9",
        "udp://[::]:9",
        "tcp://[fe80::1]:9",
        "udp://224.0.0.1:9",
    ] {
        let reply = refusing.handle(&envelope(
            &[hop],
            1,
            json!({"nonce": nonce(), "op": "beat"}),
            2000,
        ));
        assert!(
            error_of(&reply).contains("forwards to no loopback"),
            "{hop}: {reply}"
        );
    }
    // Allowed (`--allow-loopback-forward`): a closed port and a service
    // that is not a node fail in the same words.
    let allowing = Arc::new(Relay::new(runner("r"), None, None, true));
    let closed = TcpListener::bind("127.0.0.1:0").expect("a port");
    let closed_at = format!("tcp://{}", closed.local_addr().expect("addr"));
    drop(closed);
    let banner = TcpListener::bind("127.0.0.1:0").expect("a port");
    let banner_at = format!("tcp://{}", banner.local_addr().expect("addr"));
    let server = std::thread::spawn(move || {
        let (mut stream, _) = banner.accept().expect("a connection");
        let _ = stream.write_all(b"SSH-2.0-OpenSSH_9.6\r\n");
    });
    let mut errors = Vec::new();
    for hop in [&closed_at, &banner_at] {
        let reply = allowing.handle(&envelope(
            &[hop.as_str()],
            1,
            json!({"nonce": nonce(), "op": "beat"}),
            2000,
        ));
        errors.push(error_of(&reply).to_string());
    }
    server.join().expect("the banner");
    assert_eq!(errors, [NO_ANSWER, NO_ANSWER]);
}

/// A registrar keeping what it was handed.
fn keeper() -> (Registrar, Arc<Mutex<Vec<Value>>>) {
    let kept = Arc::new(Mutex::new(Vec::new()));
    let into = Arc::clone(&kept);
    let registrar: Registrar = Arc::new(move |record: &Value| {
        let mut kept = into.lock().expect("kept");
        kept.push(record.clone());
        Ok(i64::try_from(kept.len()).expect("a count") + 100)
    });
    (registrar, kept)
}

fn registering(name: &str, transport: &str, endpoint: &str, path: Vec<Endpoint>) -> Registration {
    Registration {
        message: json!({"op": "register", "name": name, "transport": transport,
                        "endpoint": endpoint, "via": [], "mode": "direct",
                        "note": "behind NAT; token=sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123"}),
        path,
    }
}

/// A `register` as `spawn node register` sends it, signed with `token`.
fn register_message(token: Option<&str>) -> Value {
    let mut message = registering("edge", "udp", "udp://edge:7", Vec::new()).message;
    message["nonce"] = json!(nonce());
    message["inventory"] = json!(INVENTORY_OUT);
    signed(message, token)
}

#[test]
fn every_request_is_signed_with_the_token_and_a_nonce_runs_once() {
    let (registrar, kept) = keeper();
    let relay = Arc::new(Relay::new(
        runner("p"),
        Some(registrar),
        Some(TOKEN.into()),
        false,
    ));
    // Probes: unsigned, signed by another, tampered, stale: refused.
    assert!(error_of(&relay.handle(&request(Op::Beat, None))).contains("auth"));
    assert!(error_of(&relay.handle(&request(Op::Beat, Some("other")))).contains("auth"));
    let mut tampered = request(Op::Beat, Some(TOKEN));
    tampered["op"] = json!("inventory");
    assert!(error_of(&relay.handle(&tampered)).contains("auth"));
    let stale = signed(
        json!({"nonce": nonce(), "op": "beat", "at_ms": now_ms() - 600_000}),
        Some(TOKEN),
    );
    assert!(error_of(&relay.handle(&stale)).contains("at_ms"));
    assert_eq!(relay.handle(&request(Op::Beat, Some(TOKEN)))["ok"], true);
    // A register: signed, taken once; sent again, the same answer.
    let message = register_message(Some(TOKEN));
    let first = relay.handle(&message);
    assert_eq!(first["ok"], true, "{first}");
    assert_eq!(relay.handle(&message), first, "the same answer");
    assert_eq!(kept.lock().expect("kept").len(), 1, "recorded once");
    let mut changed = message.clone();
    changed["endpoint"] = json!("udp://evil:7");
    assert!(error_of(&relay.handle(&changed)).contains("auth"));
    // A node without a token takes no registration, signed or not.
    let tokenless = Arc::new(Relay::new(runner("p"), Some(keeper().0), None, false));
    for message in [register_message(None), register_message(Some(TOKEN))] {
        assert!(error_of(&tokenless.handle(&message)).contains("without SPAWN_NODE_TOKEN"));
    }
    // An unsigned request marks nothing: the signed one after it runs.
    let mut unsigned = request(Op::Beat, None);
    assert!(error_of(&relay.handle(&unsigned)).contains("auth"));
    sign(&mut unsigned, Some(TOKEN));
    assert_eq!(relay.handle(&unsigned)["ok"], true);
}

#[test]
fn the_token_never_goes_on_the_wire() {
    let socket = UdpSocket::bind("127.0.0.1:0").expect("a port");
    let at = Endpoint::parse(
        &format!("udp://{}", socket.local_addr().expect("addr")),
        None,
    )
    .expect("an endpoint");
    let capture = std::thread::spawn(move || {
        let mut buffer = vec![0_u8; 65_536];
        let (read, _) = socket.recv_from(&mut buffer).expect("a datagram");
        buffer.truncate(read);
        buffer
    });
    let up = registering("edge", "udp", "udp://edge:7", vec![at]);
    assert!(register_once(&up, INVENTORY_OUT, Duration::from_secs(1), TOKEN).is_err());
    let sent: Value = serde_json::from_slice(&capture.join().expect("captured")).expect("json");
    assert!(!sent.to_string().contains(TOKEN), "{sent}");
    assert_eq!(sent["auth"].as_str().map(str::len), Some(64));
    assert!(sent.get("token").is_none());
}

#[test]
fn a_node_registers_itself_directly_and_through_relays_over_tcp_and_udp() {
    let (registrar, kept) = keeper();
    let (primary, primary_at) = node_with(
        Relay::new(runner("primary"), Some(registrar), Some(TOKEN.into()), true),
        &["tcp://127.0.0.1:0", "udp://127.0.0.1:0"],
    );
    let (relay, relay_at) = node_with(
        Relay::new(runner("gw"), None, Some(TOKEN.into()), true),
        &["tcp://127.0.0.1:0"],
    );
    for at in &primary_at {
        let event = register_once(
            &registering(
                "edge-direct",
                "tcp",
                "tcp://edge.example:7000",
                vec![at.clone()],
            ),
            INVENTORY_OUT,
            TIMEOUT,
            TOKEN,
        )
        .expect("registered");
        assert!(event > 100);
    }
    let mut up = registering(
        "edge",
        "udp",
        "udp://203.0.113.9:7001",
        vec![relay_at[0].clone(), primary_at[1].clone()],
    );
    up.message["via"] = json!(["gw"]);
    up.message["mode"] = json!("hole_punch");
    register_once(&up, INVENTORY_OUT, TIMEOUT, TOKEN).expect("registered through a relay");
    let kept = kept.lock().expect("kept").clone();
    assert_eq!(kept.len(), 3);
    let record = &kept[2];
    assert_eq!(
        json!([
            record["type"],
            record["name"],
            record["ssh"],
            record["transport"],
            record["endpoint"],
            record["via"],
            record["mode"],
            record["kernel"]
        ]),
        json!([
            "node.joined",
            "edge",
            null,
            "udp",
            "udp://203.0.113.9:7001",
            ["gw"],
            "hole_punch",
            "6.1.0"
        ])
    );
    let note = record["note"].as_str().expect("a note");
    assert!(note.starts_with("behind NAT"), "{note}");
    assert!(
        !note.contains("abcdefghijklmnopqrstuvwxyz0123"),
        "masked: {note}"
    );
    primary.stop();
    relay.stop();
}

/// Issue #85 review: a udp register sent again while the first is still
/// being recorded is not run a second time (which a store refuses as
/// "joined already"): the first answer comes back.
#[test]
fn a_udp_register_sent_again_in_flight_is_recorded_once_and_answered() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let slow: Registrar = Arc::new(move |_: &Value| {
        if counted.fetch_add(1, Ordering::SeqCst) > 0 {
            return Err("node edge is joined already (spawn node remove first)".to_string());
        }
        std::thread::sleep(Duration::from_millis(1500));
        Ok(7)
    });
    let (primary, primary_at) = node_with(
        Relay::new(runner("primary"), Some(slow), Some(TOKEN.into()), true),
        &["udp://127.0.0.1:0"],
    );
    let up = registering("edge", "udp", "udp://edge:7", vec![primary_at[0].clone()]);
    assert_eq!(register_once(&up, INVENTORY_OUT, TIMEOUT, TOKEN), Ok(7));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    primary.stop();
}

#[test]
fn a_bad_registration_is_never_recorded() {
    let (registrar, kept) = keeper();
    let relay = Arc::new(Relay::new(
        runner("p"),
        Some(registrar),
        Some(TOKEN.into()),
        false,
    ));
    let message = register_message(None);
    let with = |key: &str, value: Value| {
        let mut message = message.clone();
        message["nonce"] = json!(nonce());
        message[key] = value;
        signed(message, Some(TOKEN))
    };
    for (message, why) in [
        (with("name", json!("-edge")), "name"),
        (with("transport", json!("ssh")), "transport is tcp or udp"),
        (with("endpoint", json!("tcp://edge:7")), "endpoint"),
        (with("endpoint", json!("udp://0.0.0.0:7")), "not 0.0.0.0"),
        (with("via", json!(["edge"])), "its own via"),
        (with("via", json!(["a", "a"])), "twice"),
        (with("via", json!("gw")), "list of node names"),
        (with("mode", json!("mesh")), "mode"),
        (with("via", json!(["gw"])), "direct node has no"),
        (with("note", json!("x".repeat(501))), "note"),
        (with("note", json!("a\u{7}b")), "note"),
        (with("inventory", json!("@arch\nx86_64\n")), "cut short"),
        (with("inventory", Value::Null), "inventory"),
    ] {
        let reply = relay.handle(&message);
        assert_eq!(reply["ok"], false, "{why}: {reply}");
        assert!(error_of(&reply).contains(why), "{why}: {reply}");
    }
    assert!(
        kept.lock().expect("kept").is_empty(),
        "none of them recorded"
    );
    let mut relayed = message.clone();
    relayed["via"] = json!(["gw"]);
    relayed["mode"] = Value::Null;
    let checked = registration(&relayed).expect("a record");
    assert_eq!(
        (&checked["mode"], &checked["via"]),
        (&json!("relay"), &json!(["gw"]))
    );
}

#[test]
fn one_peer_holds_at_most_its_share_of_the_slots_and_an_idle_one_is_closed_soon() {
    let slots = Arc::new(Slots::default());
    let a: IpAddr = "10.0.0.1".parse().expect("an address");
    let held: Vec<_> = (0..PER_PEER)
        .map(|_| slots.take(a).expect("a slot"))
        .collect();
    assert!(slots.take(a).is_none(), "past its share");
    let mut others = Vec::new();
    for n in 0..(MAX_ACTIVE - PER_PEER) {
        let peer: IpAddr = format!("10.0.1.{}", n / PER_PEER + 1)
            .parse()
            .expect("an address");
        others.push(slots.take(peer).expect("another peer's slot"));
    }
    assert!(
        slots
            .take("10.0.9.9".parse().expect("an address"))
            .is_none(),
        "all taken"
    );
    drop(held);
    assert!(slots.take(a).is_some(), "given back");

    let (node, at) = node("n", &["tcp://127.0.0.1:0"]);
    let address = at[0].addresses().expect("an address")[0];
    let idle: Vec<TcpStream> = (0..PER_PEER)
        .map(|_| TcpStream::connect(address).expect("a connection"))
        .collect();
    std::thread::sleep(Duration::from_millis(300));
    let route = Route::Net(vec![at[0].clone()]);
    assert!(
        probe_route(&route, Op::Beat, Duration::from_secs(1), None).is_err(),
        "this peer's slots are all held"
    );
    let begun = Instant::now();
    let mut first = &idle[0];
    first
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("a timeout");
    let mut said = String::new();
    let _ = first.read_to_string(&mut said);
    assert!(
        begun.elapsed() < REQUEST_TIMEOUT + Duration::from_secs(1),
        "{:?}",
        begun.elapsed()
    );
    assert!(said.contains("no whole message in time"), "{said}");
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        probe_route(&route, Op::Beat, TIMEOUT, None).is_ok(),
        "slots given back"
    );
    drop(idle);
    node.stop();
}

#[test]
fn an_admitted_nonce_is_dropped_while_in_flight_and_freed_when_dropped_unanswered() {
    let relay = Arc::new(Relay::new(runner("r"), None, None, false));
    let message = request(Op::Beat, None);
    let Admit::Run(ticket) = relay.admit(&message) else {
        panic!("a fresh nonce runs");
    };
    assert!(matches!(relay.admit(&message), Admit::Drop));
    drop(ticket);
    let Admit::Run(ticket) = relay.admit(&message) else {
        panic!("run again once dropped unanswered");
    };
    let reply = relay.run(ticket, &message);
    assert!(matches!(relay.admit(&message), Admit::Reply(cached) if cached == reply));
}

/// Through a real store: a node joined over udp relays the registration of
/// another, recorded as `node.joined` via it; a hop reached over ssh is
/// refused for a udp node.
#[test]
fn a_registration_relayed_by_a_joined_node_is_recorded_in_the_store() {
    let url = std::env::var("SPAWN_TEST_DATABASE_URL")
        .expect("SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip");
    let schema = spawn_postgres::testing::TestSchema::new("node_register");
    let store = spawn_postgres::PostgresStore::connect_in_schema(&url, &schema)
        .expect("a schema, migrated");
    let (relay, relay_at) = node_with(
        Relay::new(runner("gw"), None, Some(TOKEN.into()), true),
        &["udp://127.0.0.1:0"],
    );
    for (name, transport, endpoint, ssh) in [
        ("gw", "udp", Some(relay_at[0].url()), None),
        ("jump", "ssh", None, Some("ckl@jump")),
    ] {
        let mut joined = inventory(INVENTORY_OUT).expect("an inventory");
        for (key, value) in [
            ("type", json!("node.joined")),
            ("name", json!(name)),
            ("ssh", json!(ssh)),
            ("transport", json!(transport)),
            ("endpoint", json!(endpoint)),
            ("note", Value::Null),
            ("exit_country", Value::Null),
        ] {
            joined[key] = value;
        }
        store.node_record(&joined).expect("joined");
    }
    let store = Mutex::new(store);
    let into: Registrar = Arc::new(move |record: &Value| {
        store
            .lock()
            .expect("the store")
            .node_record(record)
            .map_err(|error| error.to_string())
    });
    let (primary, primary_at) = node_with(
        Relay::new(runner("primary"), Some(into), Some(TOKEN.into()), true),
        &["tcp://127.0.0.1:0"],
    );
    let mut up = registering("edge", "tcp", "tcp://198.51.100.4:7000", Vec::new());
    up.path = vec![relay_at[0].clone(), primary_at[0].clone()];
    up.message["via"] = json!(["gw"]);
    up.message["mode"] = Value::Null;
    register_once(&up, INVENTORY_OUT, TIMEOUT, TOKEN).expect("recorded");
    let error = register_once(&up, INVENTORY_OUT, TIMEOUT, TOKEN).expect_err("joined already");
    assert!(error.contains("joined already"), "{error}");
    up.message["name"] = json!("other");
    up.message["via"] = json!(["nowhere"]);
    let error = register_once(&up, INVENTORY_OUT, TIMEOUT, TOKEN).expect_err("no such hop");
    assert!(error.contains("hop nowhere"), "{error}");
    up.message["via"] = json!(["jump"]);
    let error = register_once(&up, INVENTORY_OUT, TIMEOUT, TOKEN).expect_err("an ssh hop");
    assert!(
        error.contains("hop jump is reached over another transport"),
        "{error}"
    );
    let check = spawn_postgres::PostgresStore::connect_in_schema(&url, &schema).expect("a reader");
    let nodes = check.nodes().expect("the nodes");
    let edge = nodes
        .as_array()
        .into_iter()
        .flatten()
        .find(|node| node["name"] == "edge")
        .cloned()
        .expect("edge joined");
    assert_eq!(
        json!([
            edge["transport"],
            edge["endpoint"],
            edge["via"],
            edge["mode"]
        ]),
        json!(["tcp", "tcp://198.51.100.4:7000", ["gw"], "relay"])
    );
    assert!(nodes.as_array().is_some_and(|nodes| nodes.len() == 3));
    primary.stop();
    relay.stop();
}
