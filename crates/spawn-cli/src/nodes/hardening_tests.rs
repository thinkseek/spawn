//! Issue #87's hardening of the tcp and udp wire, each on this host's
//! loopback: replays kept as long as their auth holds, a closed udp port
//! given up at once, a node's early refusal said in its own words, a
//! store's refusal never said in SQL, an exposed listener only with its
//! token, IPv6 and IPv4 interleaved and bounded, the probe command's
//! flags; and the seal: with a token every request (and a forward's inner
//! one) travels sealed, the token before it still taken.

use super::commands::{
    add_target, connect_config, coord_config, probe_flags, probe_json, require_auth, serve_config,
};
use super::relay::{Registrar, Relay, Runner, keep_for, listen, store_refusal};
use super::transport::{
    Endpoint, Mode, Op, Route, Transport, accepted, connect_happy, exchange, happy_order, nonce,
    probe_route, request,
};
use crate::nodes::auth::MAX_SKEW_MS;
use crate::nodes::beat;
use crate::verify::now_ms;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

const BEAT_OUT: &str = "@loadavg\n0.5 0.1 0.1 1/1 1\n@meminfo\nMemAvailable: 4000000 kB\n\
@df\nFilesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 100 50 50 50% /\n@end\n";
const TOKEN: &str = "fleet-token-87";

fn words(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_string).collect()
}

fn runner() -> Runner {
    Arc::new(|op: &str| match op {
        "beat" => Ok(BEAT_OUT.to_string()),
        _ => Err("no such probe".to_string()),
    })
}

fn endpoint(scheme: &str, address: SocketAddr) -> Endpoint {
    Endpoint::parse(&format!("{scheme}://{address}"), None).expect("an endpoint")
}

/// Fix 1: an answer is kept until its request's own `at_ms` can no longer
/// pass the auth, however fast its sender's clock, within twice the skew.
#[test]
fn a_replayed_request_from_a_fast_clock_is_kept_as_long_as_its_auth_holds() {
    let now = now_ms();
    let fast = json!({"at_ms": now + 100_000});
    let keep = keep_for(&fast, now);
    assert!(
        keep >= Duration::from_millis(u64::try_from(100_000 + MAX_SKEW_MS).expect("ms")),
        "{keep:?}: past the auth's last moment"
    );
    assert!(keep > super::relay::SEEN_TTL);
    let far = keep_for(&json!({"at_ms": i64::MAX}), now);
    assert_eq!(
        far,
        Duration::from_millis(u64::try_from(2 * MAX_SKEW_MS + 10_000).expect("ms"))
    );
    assert_eq!(
        keep_for(&json!({"at_ms": now - 600_000}), now),
        super::relay::SEEN_TTL
    );
    // A relay answers the same fast-clock request from its cache.
    let relay = Arc::new(Relay::new(runner(), None, Some(TOKEN.into()), false));
    let mut message = json!({"nonce": nonce(), "op": "beat", "at_ms": now + 100_000});
    super::auth::sign(&mut message, Some(TOKEN));
    let first = relay.handle(&message);
    assert_eq!(first["ok"], true, "{first}");
    assert_eq!(relay.handle(&message), first);
}

/// Fix 2: a udp exchange with a closed port gives up after two refusals
/// in a row (~250 ms), not at its bound.
#[test]
fn a_udp_probe_of_a_closed_port_fails_fast() {
    let closed = UdpSocket::bind("127.0.0.1:0").expect("a port");
    let at = endpoint("udp", closed.local_addr().expect("addr"));
    drop(closed);
    let begun = Instant::now();
    let error = exchange(
        &at,
        &json!({"nonce": nonce(), "op": "beat"}),
        Duration::from_secs(20),
    )
    .expect_err("closed");
    assert!(
        begun.elapsed() < Duration::from_secs(3),
        "{:?}: {error}",
        begun.elapsed()
    );
    assert!(error.to_lowercase().contains("refused"), "{error}");
}

/// Fix 3: a node's refusal said before it read the request's nonce comes
/// back in its own words, not as an answer to another request.
#[test]
fn an_early_refusal_without_a_nonce_keeps_the_nodes_words() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let at = endpoint("tcp", listener.local_addr().expect("addr"));
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("a connection");
        let mut line = String::new();
        BufReader::new(&stream)
            .read_line(&mut line)
            .expect("a request");
        let mut stream = stream;
        let reply = json!({"nonce": null, "ok": false, "error": "no whole message in time"});
        stream
            .write_all(format!("{reply}\n").as_bytes())
            .expect("written");
    });
    let reply = exchange(
        &at,
        &json!({"nonce": nonce(), "op": "beat"}),
        Duration::from_secs(5),
    )
    .expect("the refusal");
    let error = accepted(reply).expect_err("refused");
    assert!(error.contains("no whole message in time"), "{error}");
    server.join().expect("the server");
}

/// Fix 4: a store's refusal is said in the writer's words, never SQL; a
/// sealed register's answer is sealed.
#[test]
fn a_registration_refused_by_the_store_says_no_sql() {
    assert_eq!(
        store_refusal("db error: ERROR: node edge is joined already (spawn node remove first)"),
        "node edge is joined already (spawn node remove first)"
    );
    assert_eq!(
        store_refusal("hop gw of the via is not joined"),
        "hop gw of the via is not joined"
    );
    for internal in [
        "db error: ERROR: relation \"spawn_events\" does not exist",
        "db error: ERROR: function spawn_node_record(jsonb) does not exist\nHINT: x",
        "error connecting to server: Connection refused",
        "",
    ] {
        let said = store_refusal(internal);
        assert_eq!(
            said, "the primary's store refused the registration",
            "{internal}"
        );
    }
    // The writer's words, its first line only: the CONTEXT naming the
    // function is dropped.
    assert_eq!(
        store_refusal(
            "db error: ERROR: a node's name is x\nCONTEXT: PL/pgSQL function spawn_node_record line 4"
        ),
        "a node's name is x"
    );
    // Through a relay: the SQL never reaches the node that registered.
    let registrar: Registrar = Arc::new(|_: &Value| {
        Err(
            "db error: ERROR: column \"kind\" of relation \"spawn_events\" does not exist"
                .to_string(),
        )
    });
    let relay = Arc::new(Relay::new(
        runner(),
        Some(registrar),
        Some(TOKEN.into()),
        false,
    ));
    let inventory = "@arch\nx86_64\n@os\nGNU/Linux\n@kernel\n6\n@cores\n4\n@meminfo\n\
MemTotal: 8000000 kB\n@df\nFilesystem 1024-blocks Used Available Capacity Mounted on\n\
/dev/sda1 100 50 50 50% /\n@end\n";
    let mut register = json!({"op": "register", "nonce": nonce(), "name": "edge",
                              "transport": "tcp", "endpoint": "tcp://edge:7", "via": [],
                              "mode": "direct", "note": null, "inventory": inventory});
    super::auth::sign(&mut register, Some(TOKEN));
    let (sealed, key) = super::crypto::seal_wire(&register, TOKEN).expect("sealed");
    let reply = relay.handle(&sealed);
    assert_eq!(reply["op"], "sealed_reply", "{reply}");
    assert!(!reply.to_string().contains("refused"), "sealed: {reply}");
    let opened = key.open_reply(&reply).expect("opened");
    assert_eq!(opened["ok"], false);
    let error = opened["error"].as_str().expect("an error");
    assert_eq!(error, "the primary's store refused the registration");
}

/// Fix 5: listening beyond loopback needs the token, or
/// `--allow-unauthenticated`; loopback does not.
#[test]
fn an_exposed_listener_needs_its_token_unless_told_otherwise() {
    let exposed = |text: &str, token: Option<&str>| {
        let config = serve_config(&words(text), token.map(str::to_string)).expect("flags");
        require_auth(
            config.binds.iter().any(|bind| !bind.loopback()),
            config.token.is_some(),
            config.allow_unauthenticated,
            "SPAWN_NODE_TOKEN",
        )
    };
    let error = exposed("--tcp 0.0.0.0:7000", None).expect_err("open to anyone");
    assert!(error.contains("SPAWN_NODE_TOKEN") && error.contains("--allow-unauthenticated"));
    assert!(exposed("--udp 192.0.2.1:7000", None).is_err());
    assert!(exposed("--listen tcp://127.0.0.1:0,udp://[::1]:0", None).is_ok());
    assert!(exposed("--tcp localhost:7000", None).is_ok());
    assert!(exposed("--tcp 0.0.0.0:7000", Some(TOKEN)).is_ok());
    assert!(exposed("--tcp 0.0.0.0:7000 --allow-unauthenticated", None).is_ok());
    // The coordinator: the same, with SPAWN_COORD_TOKEN.
    let error = coord_config(&words("--ws 0.0.0.0:8443"), None).expect_err("exposed");
    assert!(error.contains("SPAWN_COORD_TOKEN"), "{error}");
    assert!(coord_config(&words("--ws 0.0.0.0:8443"), Some("c".into())).is_ok());
    assert!(coord_config(&words("--ws 0.0.0.0:8443 --allow-unauthenticated"), None).is_ok());
    let config = coord_config(
        &words(
            "--ws 127.0.0.1:0 --tcp 127.0.0.1:0 --udp 127.0.0.1:0 --zone eu-1 \
                --parent wss://root.example/mesh --lease-secs 9",
        ),
        None,
    )
    .expect("loopback");
    assert_eq!(config.zone.as_deref(), Some("eu-1"));
    assert_eq!(config.lease, Some(Duration::from_secs(9)));
    assert_eq!(
        config.parent.expect("a parent").url(),
        "wss://root.example/mesh"
    );
    for (text, why) in [
        ("--udp 127.0.0.1:0", "--ws or --tcp"),
        ("--ws nowhere", "a bind is ip:port"),
        ("--ws 127.0.0.1:0 --zone -x", "--zone"),
        ("--ws 127.0.0.1:0 --parent http://x", "--parent"),
        ("--ws 127.0.0.1:0 --lease-secs 0", "--lease-secs"),
        ("--ws 127.0.0.1:0 --port 1", "no --port"),
    ] {
        let error = coord_config(&words(text), None).expect_err(text);
        assert!(error.contains(why), "{text}: {error}");
    }
}

/// Fix 6: IPv6 and IPv4 interleaved, the first family first; a silent
/// address costs at most its bound before the next is tried.
#[test]
fn a_connect_interleaves_families_and_skips_a_silent_address_in_time() {
    let address = |text: &str| text.parse::<SocketAddr>().expect("an address");
    let order = happy_order(&[
        address("[2001:db8::1]:1"),
        address("[2001:db8::2]:1"),
        address("[2001:db8::3]:1"),
        address("192.0.2.1:1"),
        address("192.0.2.2:1"),
    ]);
    assert_eq!(
        order,
        [
            address("[2001:db8::1]:1"),
            address("192.0.2.1:1"),
            address("[2001:db8::2]:1"),
            address("192.0.2.2:1"),
            address("[2001:db8::3]:1"),
        ]
    );
    assert_eq!(
        happy_order(&[address("192.0.2.1:1"), address("[2001:db8::1]:1")]),
        [address("192.0.2.1:1"), address("[2001:db8::1]:1")]
    );
    // A blackholed IPv6 and IPv4 first, then the address that answers.
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let live = listener.local_addr().expect("addr");
    let begun = Instant::now();
    let stream = connect_happy(
        &"test",
        &[address("[100::1]:9"), address("10.255.255.1:9"), live],
        Instant::now() + Duration::from_secs(30),
    )
    .expect("connected to the live one");
    assert_eq!(stream.peer_addr().expect("peer"), live);
    assert!(
        begun.elapsed() < Duration::from_millis(4500),
        "{:?}",
        begun.elapsed()
    );
}

/// Fix 7: `spawn node probe`'s flags, and `spawn node add --coord`'s.
#[test]
#[allow(clippy::too_many_lines)]
fn the_probe_command_reads_its_flags_and_says_its_facts() {
    assert_eq!(probe_flags(&[]), Ok((Op::Beat, false)));
    assert_eq!(
        probe_flags(&words("--op inventory --json")),
        Ok((Op::Inventory, true))
    );
    assert!(probe_flags(&words("--op reboot")).is_err());
    assert!(probe_flags(&words("--force x")).is_err());
    let said = probe_json(
        "edge",
        Op::Beat,
        &json!("coord"),
        BEAT_OUT,
        Duration::from_millis(12),
    )
    .expect("facts");
    assert_eq!(
        json!([
            said["name"],
            said["op"],
            said["transport"],
            said["elapsed_ms"],
            said["facts"]["load1"]
        ]),
        json!(["edge", "beat", "coord", 12, 0.5])
    );
    let id = "ab".repeat(32);
    let target =
        |text: &str| add_target("laptop", &crate::Flags::parse(&words(text)).expect("flags"));
    let read =
        target(&format!("--coord wss://coord.example/mesh --node-id {id}")).expect("a target");
    assert_eq!(
        (
            read.transport,
            read.endpoint.as_str(),
            read.mode,
            read.node_id.as_deref()
        ),
        (
            Transport::Coord,
            "wss://coord.example/mesh",
            Mode::Relay,
            Some(id.as_str())
        )
    );
    let read =
        target("--coord tcp://10.0.0.1:7443 --peer laptop --mode hole_punch").expect("by name");
    assert_eq!(
        (read.peer.as_deref(), read.mode),
        (Some("laptop"), Mode::HolePunch)
    );
    for (text, why) in [
        (
            "--coord wss://c.example",
            "exactly one of --node-id, --peer",
        ),
        (
            "--coord wss://c.example --peer a --node-id x",
            "exactly one",
        ),
        ("--coord wss://c.example --node-id ABC", "--node-id is 64"),
        ("--coord wss://c.example --peer -x", "--peer is"),
        ("--coord wss://c.example --peer a --via gw", "--via"),
        ("--coord wss://c.example --peer a --mode direct", "--mode"),
        ("--coord http://c.example --peer a", "--coord"),
        ("--coord tcp://c.example --peer a", "--coord"),
        ("--tcp gw:1 --peer a", "--node-id and --peer"),
        (
            "--tcp gw:1 --coord ws://c",
            "exactly one of --ssh, --tcp, --udp",
        ),
    ] {
        let error = target(text).expect_err(text);
        assert!(error.contains(why), "{text}: {error}");
    }
    // `spawn node connect`'s.
    let config = connect_config(
        &words(
            "--coord ws://127.0.0.1:9 --name laptop --role peer --zone home \
                --udp-stun 127.0.0.1:9 --register-to primary --note desk",
        ),
        Some(TOKEN),
        Some(String::new()),
    )
    .expect("a config");
    assert_eq!(
        (
            config.role.as_str(),
            config.zone.as_deref(),
            config.register_to.as_deref(),
            config.coord_token
        ),
        ("peer", Some("home"), Some("primary"), None)
    );
    for (text, token, why) in [
        (
            "--coord ws://c --name a --register-store",
            None,
            "--register-store requires",
        ),
        (
            "--coord ws://c --name a --register-to p",
            None,
            "--register-to requires",
        ),
        ("--coord ws://c --name a --role boss", Some(TOKEN), "--role"),
        ("--coord ws://c --name -a", Some(TOKEN), "--name"),
        ("--coord ftp://c --name a", Some(TOKEN), "--coord"),
        (
            "--coord ws://c --name a --udp-stun x",
            Some(TOKEN),
            "--udp-stun",
        ),
        ("--name a", Some(TOKEN), "--coord is required"),
    ] {
        let error = connect_config(&words(text), token, None).expect_err(text);
        assert!(error.contains(why), "{text}: {error}");
    }
    // Review S1: without the node token a connector answers any client of
    // its coordinator, so it is refused unless told otherwise.
    let error = connect_config(&words("--coord ws://c --name a"), None, None).expect_err("open");
    assert!(
        error.contains("SPAWN_NODE_TOKEN") && error.contains("--allow-unauthenticated"),
        "{error}"
    );
    assert!(connect_config(&words("--coord ws://c --name a"), Some(""), None).is_err());
    assert!(
        connect_config(
            &words("--coord ws://c --name a --allow-unauthenticated"),
            None,
            None
        )
        .is_ok()
    );
    assert!(connect_config(&words("--coord ws://c --name a"), Some(TOKEN), None).is_ok());
}

/// Review S2: a refusal without a nonce is the node's answer on tcp (its
/// connection is the request's) but never on udp, where anyone may send
/// one: there the wait goes on, and the real reply is taken.
#[test]
fn a_udp_refusal_without_a_nonce_is_no_answer() {
    let socket = UdpSocket::bind("127.0.0.1:0").expect("a port");
    let at = endpoint("udp", socket.local_addr().expect("addr"));
    let server = std::thread::spawn(move || {
        let mut buffer = vec![0_u8; 65_536];
        let (read, peer) = socket.recv_from(&mut buffer).expect("a request");
        let request: Value = serde_json::from_slice(&buffer[..read]).expect("json");
        for reply in [
            json!({"nonce": null, "ok": false, "error": "spoofed refusal"}),
            json!({"nonce": request["nonce"], "ok": true, "output": "real"}),
        ] {
            socket
                .send_to(reply.to_string().as_bytes(), peer)
                .expect("sent");
        }
        std::thread::sleep(Duration::from_millis(300));
    });
    let reply = exchange(
        &at,
        &json!({"nonce": nonce(), "op": "beat"}),
        Duration::from_secs(5),
    )
    .expect("the real reply");
    assert_eq!(reply["output"], "real");
    server.join().expect("the server");
}

/// With a token, a probe is sealed on the wire: what a capture sees is
/// `op: "sealed"`, its auth and ciphertext, never the op or the token.
#[test]
fn a_probe_with_a_token_travels_sealed_and_its_forward_carries_a_sealed_inner() {
    let capture = UdpSocket::bind("127.0.0.1:0").expect("a port");
    let at = endpoint("udp", capture.local_addr().expect("addr"));
    let sniff = std::thread::spawn(move || {
        let mut buffer = vec![0_u8; 65_536];
        let (read, _) = capture.recv_from(&mut buffer).expect("a datagram");
        serde_json::from_slice::<Value>(&buffer[..read]).expect("json")
    });
    let route = Route::Net(vec![at.clone()]);
    assert!(probe_route(&route, Op::Beat, Duration::from_millis(600), Some(TOKEN)).is_err());
    let seen = sniff.join().expect("sniffed");
    assert_eq!(seen["op"], "sealed", "{seen}");
    assert_eq!(seen["auth"].as_str().map(str::len), Some(64));
    for hidden in ["beat", TOKEN] {
        assert!(!seen.to_string().contains(hidden), "{hidden}: {seen}");
    }
    // Through a relay: the inner request it passes on is the sender's
    // seal, unread (it would not open with another token either).
    let relay = Relay::new(runner(), None, Some(TOKEN.into()), true);
    let relayed = listen(
        &Arc::new(relay),
        &[Endpoint::bind("tcp://127.0.0.1:0", None).expect("a bind")],
    )
    .expect("listening");
    let capture = UdpSocket::bind("127.0.0.1:0").expect("a port");
    let last = endpoint("udp", capture.local_addr().expect("addr"));
    let sniff = std::thread::spawn(move || {
        let mut buffer = vec![0_u8; 65_536];
        let (read, _) = capture.recv_from(&mut buffer).expect("a datagram");
        serde_json::from_slice::<Value>(&buffer[..read]).expect("json")
    });
    let route = Route::Net(vec![relayed.endpoints[0].clone(), last]);
    assert!(probe_route(&route, Op::Beat, Duration::from_millis(900), Some(TOKEN)).is_err());
    let inner = sniff.join().expect("sniffed");
    assert_eq!(inner["op"], "sealed", "{inner}");
    assert!(!inner.to_string().contains("beat"), "{inner}");
    assert!(super::crypto::open_wire(&inner, &[TOKEN]).is_ok());
    relayed.stop();
}

/// A fleet rotating its token: a node with the new token and the old one
/// takes probes sealed and signed with either, through a relay too; a
/// third is refused; plain signed requests (#85) still answer.
#[test]
fn a_node_takes_the_previous_token_while_a_fleet_rotates() {
    let node = |name: &str| {
        let relay = Relay::new(runner(), None, Some("new".into()), true)
            .with_previous_token(Some("old".into()));
        let _ = name;
        listen(
            &Arc::new(relay),
            &[Endpoint::bind("udp://127.0.0.1:0", None).expect("a bind")],
        )
        .expect("listening")
    };
    let (a, b) = (node("a"), node("b"));
    let direct = Route::Net(vec![b.endpoints[0].clone()]);
    let relayed = Route::Net(vec![a.endpoints[0].clone(), b.endpoints[0].clone()]);
    for token in ["new", "old"] {
        for route in [&direct, &relayed] {
            let output =
                probe_route(route, Op::Beat, Duration::from_secs(5), Some(token)).expect("taken");
            assert!(beat(&output).is_ok());
        }
    }
    let error =
        probe_route(&direct, Op::Beat, Duration::from_secs(5), Some("older")).expect_err("refused");
    assert!(error.contains("auth"), "{error}");
    // A previous token without a current one is no token.
    let tokenless = Relay::new(runner(), None, None, false).with_previous_token(Some("old".into()));
    let tokenless = Arc::new(tokenless);
    assert_eq!(tokenless.handle(&request(Op::Beat, None))["ok"], true);
    let (sealed, _) =
        super::crypto::seal_wire(&request(Op::Beat, Some("old")), "old").expect("sealed");
    assert!(
        tokenless.handle(&sealed)["error"]
            .as_str()
            .unwrap_or_default()
            .contains("without SPAWN_NODE_TOKEN")
    );
    // A plain signed request, as #85 sends it, still answers.
    let plain = Arc::new(
        Relay::new(runner(), None, Some("new".into()), false)
            .with_previous_token(Some("old".into())),
    );
    assert_eq!(plain.handle(&request(Op::Beat, Some("old")))["ok"], true);
    a.stop();
    b.stop();
}

/// Review M1: one sealed request answered by two nodes sharing the token
/// (or answered again) never reuses a keystream: each reply has its own
/// salt and nonce, so the XOR of the ciphertexts is not the XOR of the
/// plaintexts.
#[test]
fn two_replies_to_one_sealed_request_never_share_a_keystream() {
    let answering = |load: &'static str| -> Runner {
        Arc::new(move |_: &str| Ok(BEAT_OUT.replace("0.5 ", load)))
    };
    let a = Arc::new(Relay::new(
        answering("1.5 "),
        None,
        Some(TOKEN.into()),
        false,
    ));
    let b = Arc::new(Relay::new(
        answering("7.5 "),
        None,
        Some(TOKEN.into()),
        false,
    ));
    let (sealed, key) =
        super::crypto::seal_wire(&request(Op::Beat, Some(TOKEN)), TOKEN).expect("sealed");
    let (reply_a, reply_b) = (a.handle(&sealed), b.handle(&sealed));
    for field in ["reply_salt", "reply_nonce"] {
        assert!(reply_a[field].is_string(), "{reply_a}");
        assert_ne!(reply_a[field], reply_b[field], "{field}");
    }
    let opened_a = key.open_reply(&reply_a).expect("a opens");
    let opened_b = key.open_reply(&reply_b).expect("b opens");
    let (pt_a, pt_b) = (
        serde_json::to_vec(&opened_a).expect("bytes"),
        serde_json::to_vec(&opened_b).expect("bytes"),
    );
    assert_eq!(pt_a.len(), pt_b.len(), "equal lengths make the check exact");
    assert_ne!(pt_a, pt_b);
    let ct = |reply: &Value| super::crypto::unb64(reply["ct"].as_str().expect("ct")).expect("b64");
    let (ct_a, ct_b) = (ct(&reply_a), ct(&reply_b));
    let xor = |x: &[u8], y: &[u8]| x.iter().zip(y).map(|(p, q)| p ^ q).collect::<Vec<u8>>();
    assert_ne!(
        xor(&ct_a[..pt_a.len()], &ct_b[..pt_b.len()]),
        xor(&pt_a, &pt_b),
        "a reused keystream"
    );
    // A reply's salt or nonce changed on its way: it does not open.
    for field in ["reply_salt", "reply_nonce"] {
        let mut changed = reply_a.clone();
        changed[field] = reply_b[field].clone();
        assert!(key.open_reply(&changed).is_err(), "{field}");
    }
}
