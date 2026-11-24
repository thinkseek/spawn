//! The coordinators on this host's loopback: URLs and hellos checked;
//! nodes connected over ws:// and tcp://, probed end to end through a
//! coordinator that sees only ciphertext; a two-tier tree (a child zone
//! under a root) routing both ways, its leases running out; a relay sent
//! as another node, a revoked or unpinned peer, refused; a punched udp
//! path, and the relay when it gives nothing; a node joining itself
//! through a coordinator, into a keeper and into a real store.

use super::{
    ConnectConfig, CoordClient, CoordConfig, CoordUrl, HelloFields, Kind, NodeService,
    ProbeContext, Running, Say, ask, check_hello, coord_registration, hello, run_connector,
    start_with,
};
use crate::nodes::crypto::{NodeIdentity, PeerKeys, Responder, TrustPolicy};
use crate::nodes::relay::{Registrar, Runner};
use crate::nodes::transport::{Op, Route, probe_route};
use crate::nodes::{beat, inventory};
use serde_json::{Value, json};
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const INVENTORY_OUT: &str = "@arch\nx86_64\n@os\nGNU/Linux\n@kernel\n6.1.0\n@cores\n4\n\
@cpu_max\n@meminfo\nMemTotal:  8000000 kB\n@mem_max\nmax\n@df\n\
Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 100 50 50 50% /\n@gpu\n@end\n";
const BEAT_OUT: &str = "@loadavg\n0.75 0.1 0.1 1/1 1\n@meminfo\nMemAvailable: 4000000 kB\n\
@df\nFilesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 100 50 50 50% /\n@end\n";
const TOKEN: &str = "mesh-token-87";
const T: Duration = Duration::from_secs(10);

fn runner(name: &str) -> Runner {
    let name = name.to_string();
    Arc::new(move |op: &str| match op {
        "inventory" => Ok(INVENTORY_OUT.replace("6.1.0", &name)),
        "beat" => Ok(BEAT_OUT.to_string()),
        _ => Err("no such probe".to_string()),
    })
}

fn url(text: &str) -> CoordUrl {
    CoordUrl::parse(text).expect("a URL")
}

/// A coordinator on loopback (ws, tcp and udp), its every message kept.
fn coordinator(
    zone: Option<&str>,
    parent: Option<CoordUrl>,
    lease: Option<Duration>,
) -> (Running, Arc<Mutex<Vec<String>>>) {
    let tap = Arc::new(Mutex::new(Vec::new()));
    let config = CoordConfig {
        ws: Some("127.0.0.1:0".into()),
        tcp: Some("127.0.0.1:0".into()),
        udp: Some("127.0.0.1:0".into()),
        zone: zone.map(str::to_string),
        parent,
        lease,
        ..CoordConfig::default()
    };
    let running = start_with(&config, Some(Arc::clone(&tap))).expect("a coordinator");
    (running, tap)
}

fn ws_of(running: &Running) -> CoordUrl {
    url(&format!("ws://{}/mesh", running.ws.expect("ws")))
}

fn tcp_of(running: &Running) -> CoordUrl {
    url(&format!("tcp://{}", running.tcp.expect("tcp")))
}

/// A connected node (`spawn node connect`) on a thread of its own, until
/// dropped; what it says kept.
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

fn config(at: &CoordUrl, name: &str) -> ConnectConfig {
    ConnectConfig {
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
    }
}

fn node_with(config: ConnectConfig, policy: TrustPolicy, registrar: Option<Registrar>) -> Node {
    let identity = Arc::new(NodeIdentity::generate().expect("a key").0);
    let service = Arc::new(NodeService {
        responder: Responder::new(Arc::clone(&identity), policy),
        runner: runner(&config.name),
        registrar,
        in_flight: Arc::default(),
    });
    let stop = Arc::new(AtomicBool::new(false));
    let said = Arc::new(Mutex::new(Vec::new()));
    let thread = {
        let (identity, stop, said) = (Arc::clone(&identity), Arc::clone(&stop), Arc::clone(&said));
        let say: Say = Arc::new(move |line: String| said.lock().expect("said").push(line));
        std::thread::spawn(move || {
            let _ = run_connector(&config, &identity, &service, &stop, None, &say);
        })
    };
    let node = Node {
        identity,
        stop,
        said,
        thread: Some(thread),
    };
    wait_for(&node, "connected");
    node
}

fn node(at: &CoordUrl, name: &str) -> Node {
    node_with(
        config(at, name),
        TrustPolicy::new(Some(TOKEN), None, None),
        None,
    )
}

/// Wait until `node` said a line containing `what`.
fn wait_for(node: &Node, what: &str) -> String {
    let deadline = Instant::now() + T;
    loop {
        if let Some(line) = node
            .said
            .lock()
            .expect("said")
            .iter()
            .find(|line| line.contains(what))
        {
            return line.clone();
        }
        assert!(
            Instant::now() < deadline,
            "never said {what}: {:?}",
            node.said.lock().expect("said")
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn context(token: Option<&str>) -> ProbeContext {
    ProbeContext {
        identity: Arc::new(NodeIdentity::generate().expect("a key").0),
        policy: TrustPolicy::new(token, None, None),
        coord_token: None,
        tls: None,
        allow_loopback: true,
        punch: true,
    }
}

fn probe(
    at: &CoordUrl,
    node_id: &str,
    op: &str,
    context: &ProbeContext,
) -> Result<super::Probed, String> {
    ask(
        at,
        Some(node_id),
        None,
        None,
        &json!({"op": op}),
        T,
        context,
    )
}

#[test]
fn a_coordinator_url_is_ws_wss_or_tcp_and_nothing_else() {
    for (text, port, path) in [
        ("ws://coord.example", 80, "/"),
        ("wss://coord.example/mesh?zone=eu", 443, "/mesh?zone=eu"),
        ("wss://coord.example:8443/a/b", 8443, "/a/b"),
        ("tcp://10.0.0.1:7443", 7443, "/"),
        ("ws://[2001:db8::1]:9/x", 9, "/x"),
    ] {
        let read = url(text);
        assert_eq!(
            (read.port, read.path.as_str(), read.url()),
            (port, path, text.to_string())
        );
    }
    assert_eq!(url("ws://[::1]:9").bare_host(), "::1");
    for bad in [
        "coord.example:443",
        "http://coord.example",
        "udp://coord.example:1",
        "tcp://coord.example",
        "wss://user@coord.example",
        "wss://coord.example:0",
        "wss://coord.example:70000",
        "wss://coord example",
        "wss://-coord.example",
        "wss://coord.example/a\tb",
        "ws://[::1:9",
        "wss://coord.example/<script>",
    ] {
        assert!(CoordUrl::parse(bad).is_err(), "{bad}");
    }
    assert!(CoordUrl::parse(&format!("wss://c.example/{}", "a".repeat(250))).is_err());
}

#[test]
fn a_hello_is_signed_by_its_key_fresh_and_carries_the_coordinators_token() {
    let identity = NodeIdentity::generate().expect("a key").0;
    let fields = HelloFields {
        kind: Kind::Node,
        name: "laptop".into(),
        role: "peer".into(),
        zone: Some("home".into()),
        udp_reflexive: Some("203.0.113.7:4500".into()),
    };
    let signed = hello(&identity, &fields, Some("coord-secret"));
    assert!(!signed.to_string().contains("coord-secret"));
    let (kind, record) = check_hello(&signed, Some("coord-secret"), false).expect("checked");
    assert_eq!(kind, Kind::Node);
    assert_eq!(
        json!([
            record["node_id"],
            record["name"],
            record["role"],
            record["zone"],
            record["udp_reflexive"]
        ]),
        json!([
            identity.node_id(),
            "laptop",
            "peer",
            "home",
            "203.0.113.7:4500"
        ])
    );
    assert!(record.get("sig").is_none() && record.get("coord_auth").is_none());
    // Another token, none, a changed field, a stale one: refused; the
    // WebSocket's bearer stands for the token.
    assert!(check_hello(&signed, Some("other"), false).is_err());
    assert!(
        check_hello(
            &hello(&identity, &fields, None),
            Some("coord-secret"),
            false
        )
        .is_err()
    );
    assert!(check_hello(&hello(&identity, &fields, None), Some("coord-secret"), true).is_ok());
    for (key, value) in [
        ("name", json!("evil")),
        ("zone", json!("other")),
        ("udp_reflexive", json!("198.51.100.1:1")),
        ("kind", json!("coord")),
        ("at_ms", json!(0)),
    ] {
        let mut tampered = signed.clone();
        tampered[key] = value;
        assert!(
            check_hello(&tampered, Some("coord-secret"), false).is_err(),
            "{key}"
        );
    }
    let mut bad = fields.clone();
    bad.role = "admin".into();
    assert!(check_hello(&hello(&identity, &bad, None), None, false).is_err());
    bad = fields;
    bad.udp_reflexive = Some("not-an-address".into());
    assert!(check_hello(&hello(&identity, &bad, None), None, false).is_err());
}

#[test]
fn a_node_behind_a_coordinator_answers_sealed_probes_the_coordinator_cannot_read() {
    let (running, tap) = coordinator(None, None, None);
    for at in [ws_of(&running), tcp_of(&running)] {
        let laptop = node(&at, "laptop");
        let context = context(Some(TOKEN));
        let probed = probe(&at, laptop.identity.node_id(), "inventory", &context).expect("probed");
        assert_eq!(probed.mode, "relay");
        assert_eq!(probed.reply["ok"], true);
        let output = probed.reply["output"].as_str().expect("an output");
        assert_eq!(inventory(output).expect("an inventory")["kernel"], "laptop");
        assert_eq!(probed.peer["name"], "laptop");
        let probed = probe(&at, laptop.identity.node_id(), "beat", &context).expect("probed");
        assert!(beat(probed.reply["output"].as_str().expect("an output")).is_ok());
        // An op the node does not run: refused inside the seal.
        let refused = probe(&at, laptop.identity.node_id(), "reboot", &context).expect("answered");
        assert_eq!(refused.reply["ok"], false);
        // By name, too.
        let by_name = ask(
            &at,
            None,
            Some("laptop"),
            None,
            &json!({"op": "beat"}),
            T,
            &context,
        )
        .expect("by name");
        assert_eq!(by_name.peer["node_id"], laptop.identity.node_id());
    }
    // What the coordinator saw: hellos, lookups, relays of ciphertext.
    let seen = tap.lock().expect("tap").join("\n");
    assert!(
        seen.contains("\"relay\"") && seen.contains("\"ct\""),
        "relays seen"
    );
    for hidden in [
        "@arch",
        "x86_64",
        "MemTotal",
        "inventory",
        "reboot",
        "\"beat\"",
        TOKEN,
    ] {
        assert!(!seen.contains(hidden), "the coordinator saw {hidden}");
    }
    running.stop();
}

#[test]
fn a_probe_without_the_token_a_revoked_peer_or_another_key_is_refused() {
    let (running, _) = coordinator(None, None, None);
    let at = ws_of(&running);
    let laptop = node(&at, "laptop");
    let id = laptop.identity.node_id().to_string();
    // Without the token, or with another: the envelope does not open.
    for token in [None, Some("other")] {
        let error = probe(&at, &id, "beat", &context(token)).expect_err("refused");
        assert!(error.contains("does not open"), "{error}");
    }
    // A probe sealed to a revoked node is never sent.
    let mut revoking = context(Some(TOKEN));
    revoking.policy = TrustPolicy::new(Some(TOKEN), None, Some(&id));
    assert!(
        probe(&at, &id, "beat", &revoking)
            .expect_err("revoked")
            .contains("revoked")
    );
    // A node that revoked the asker refuses it.
    let asker = context(Some(TOKEN));
    let mut strict = config(&at, "strict");
    strict.name = "strict".into();
    let strict = node_with(
        strict,
        TrustPolicy::new(Some(TOKEN), None, Some(asker.identity.node_id())),
        None,
    );
    let error = probe(&at, strict.identity.node_id(), "beat", &asker).expect_err("revoked");
    assert!(error.contains("revoked"), "{error}");
    // A key pinned at the join that is not the node's: refused before any
    // request is sealed.
    let other = NodeIdentity::generate().expect("a key").0;
    let error = ask(
        &at,
        Some(&id),
        None,
        Some(&other.pubkey_b64()),
        &json!({"op": "beat"}),
        T,
        &asker,
    )
    .expect_err("another key");
    assert!(error.contains("pinned"), "{error}");
    // A node not connected.
    let error = probe(&at, &"0".repeat(64), "beat", &asker).expect_err("nowhere");
    assert!(error.contains("knows no such node"), "{error}");
    drop((laptop, strict));
    running.stop();
}

#[test]
fn a_session_cannot_relay_as_another_node() {
    let (running, _) = coordinator(None, None, None);
    let at = tcp_of(&running);
    let laptop = node(&at, "laptop");
    let mallory = NodeIdentity::generate().expect("a key").0;
    let client = CoordClient::connect(
        &at,
        Arc::new(mallory),
        TrustPolicy::default(),
        &HelloFields {
            kind: Kind::Client,
            name: "mallory".into(),
            role: "client".into(),
            zone: None,
            udp_reflexive: None,
        },
        None,
        T,
        None,
    )
    .expect("connected");
    client.send(
        json!({"type": "relay", "from": "f".repeat(64), "to": laptop.identity.node_id(),
                       "id": "x", "envelope": {}}),
    );
    let answer = client.next_incoming(T).expect("an answer");
    assert_eq!(answer["type"], "error");
    assert!(
        answer["error"]
            .as_str()
            .unwrap_or_default()
            .contains("another node"),
        "{answer}"
    );
    // Nor send an error that fails another's request: dropped, and a
    // probe of the laptop still gets its answer.
    let asker = context(Some(TOKEN));
    client.send(json!({"type": "error", "for": asker.identity.node_id(),
                       "to": laptop.identity.node_id(), "id": "x", "error": "forged"}));
    assert!(client.next_incoming(Duration::from_millis(300)).is_none());
    let probed = probe(&at, laptop.identity.node_id(), "beat", &asker).expect("answered");
    assert_eq!(probed.reply["ok"], true);
    client.close();
    running.stop();
}

#[test]
fn a_two_tier_tree_routes_down_and_up_and_its_leases_run_out() {
    let (root, root_tap) = coordinator(Some("root"), None, None);
    let (child, _) = coordinator(Some("eu"), Some(ws_of(&root)), Some(Duration::from_secs(3)));
    // A node in the child's zone (over tcp), another at the root (ws).
    let eu = node(&tcp_of(&child), "eu-worker");
    let home = node(&ws_of(&root), "home-peer");
    // The root learns the child's zone from its summary.
    let deadline = Instant::now() + T;
    while root
        .coordinator
        .find(Some(eu.identity.node_id()), None)
        .is_none()
    {
        assert!(
            Instant::now() < deadline,
            "the root never learnt the child's zone"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // From the root down to the child's node, by id and by name.
    let asker = context(Some(TOKEN));
    let probed =
        probe(&ws_of(&root), eu.identity.node_id(), "inventory", &asker).expect("down the tree");
    assert_eq!(
        inventory(probed.reply["output"].as_str().expect("output")).expect("inventory")["kernel"],
        "eu-worker"
    );
    let by_name = ask(
        &ws_of(&root),
        None,
        Some("eu-worker"),
        None,
        &json!({"op": "beat"}),
        T,
        &asker,
    )
    .expect("by name down the tree");
    assert_eq!(by_name.reply["ok"], true);
    // From the child up to the root's node (the lookup asked up too).
    let probed =
        probe(&tcp_of(&child), home.identity.node_id(), "beat", &asker).expect("up the tree");
    assert_eq!(probed.reply["ok"], true);
    // The root relayed ciphertext only.
    let seen = root_tap.lock().expect("tap").join("\n");
    assert!(seen.contains("zone_summary"));
    for hidden in ["@arch", "eu-worker\\n", "MemAvailable", TOKEN] {
        assert!(!seen.contains(hidden), "the root saw {hidden}");
    }
    // The child gone: its zone is forgotten (session closed; and a
    // summary's lease runs out on its own).
    drop(eu);
    child.stop();
    let deadline = Instant::now() + Duration::from_secs(8);
    while root.coordinator.find(None, Some("eu-worker")).is_some() {
        assert!(Instant::now() < deadline, "the child's zone outlived it");
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(home);
    root.stop();
}

#[test]
fn a_summary_lease_runs_out_without_its_refresh() {
    let (root, _) = coordinator(Some("root"), None, None);
    let identity = Arc::new(NodeIdentity::generate().expect("a key").0);
    let child = CoordClient::connect(
        &ws_of(&root),
        Arc::clone(&identity),
        TrustPolicy::default(),
        &HelloFields {
            kind: Kind::Coord,
            name: "coord-eu".into(),
            role: "coord".into(),
            zone: Some("eu".into()),
            udp_reflexive: None,
        },
        None,
        T,
        None,
    )
    .expect("a child");
    let peer = NodeIdentity::generate().expect("a key").0;
    let mut record = peer.public_info();
    record["name"] = json!("leased");
    record["role"] = json!("worker");
    child.send(
        json!({"type": "zone_summary", "zone": "eu", "peers_summary": [record],
                      "lease_epoch": 2, "lease_ms": 1000}),
    );
    let deadline = Instant::now() + T;
    while root.coordinator.find(None, Some("leased")).is_none() {
        assert!(Instant::now() < deadline, "never listed");
        std::thread::sleep(Duration::from_millis(20));
    }
    // An older epoch changes nothing; then the lease runs out.
    child.send(
        json!({"type": "zone_summary", "zone": "eu", "peers_summary": [],
                      "lease_epoch": 1, "lease_ms": 1000}),
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(root.coordinator.find(None, Some("leased")).is_some());
    std::thread::sleep(Duration::from_secs(1));
    assert!(
        root.coordinator.find(None, Some("leased")).is_none(),
        "the lease ran out"
    );
    child.close();
    root.stop();
}

#[test]
fn a_punched_udp_path_answers_and_a_dead_one_falls_back_to_the_relay() {
    let (running, _) = coordinator(None, None, None);
    let at = ws_of(&running);
    let mut punching = config(&at, "punchy");
    punching.udp_stun = Some(running.udp.expect("udp").to_string());
    let punchy = node_with(punching, TrustPolicy::new(Some(TOKEN), None, None), None);
    let line = wait_for(&punchy, "udp reflexive");
    assert!(line.contains("127.0.0.1:"), "{line}");
    let asker = context(Some(TOKEN));
    let probed = probe(&at, punchy.identity.node_id(), "beat", &asker).expect("probed");
    assert_eq!(
        (probed.mode, &probed.reply["ok"]),
        ("hole_punch", &json!(true))
    );
    // Punching off (or a mapping on loopback refused): the relay.
    let mut no_punch = context(Some(TOKEN));
    no_punch.allow_loopback = false;
    let probed = probe(&at, punchy.identity.node_id(), "beat", &no_punch).expect("relayed");
    assert_eq!(probed.mode, "relay");
    // A node whose udp mapping is dead: the punched try gives nothing, the
    // relay answers, within the bound.
    let dead = UdpSocket::bind("127.0.0.1:0").expect("a port");
    let dead_at = dead.local_addr().expect("addr");
    drop(dead);
    let identity = Arc::new(NodeIdentity::generate().expect("a key").0);
    let service = NodeService {
        responder: Responder::new(
            Arc::clone(&identity),
            TrustPolicy::new(Some(TOKEN), None, None),
        ),
        runner: runner("stale"),
        registrar: None,
        in_flight: Arc::default(),
    };
    let client = Arc::new(
        CoordClient::connect(
            &at,
            Arc::clone(&identity),
            TrustPolicy::default(),
            &HelloFields {
                kind: Kind::Node,
                name: "stale".into(),
                role: "worker".into(),
                zone: None,
                udp_reflexive: Some(dead_at.to_string()),
            },
            None,
            T,
            None,
        )
        .expect("connected"),
    );
    let answering = {
        let client = Arc::clone(&client);
        std::thread::spawn(move || {
            let deadline = Instant::now() + T;
            while Instant::now() < deadline {
                if let Some(message) = client.next_incoming(Duration::from_millis(50))
                    && message["type"] == "relay"
                {
                    let envelope = &message["envelope"];
                    let reply = service.answer(envelope);
                    client.send(json!({"type": "relay", "from": client.identity().node_id(),
                                       "to": envelope["from"], "id": envelope["req_nonce"],
                                       "envelope": reply}));
                    return;
                }
            }
        })
    };
    let begun = Instant::now();
    let probed = probe(&at, identity.node_id(), "beat", &asker).expect("relayed");
    assert_eq!(probed.mode, "relay");
    assert!(
        begun.elapsed() < Duration::from_secs(5),
        "{:?}",
        begun.elapsed()
    );
    answering.join().expect("answered");
    client.close();
    drop(punchy);
    running.stop();
}

/// A registrar keeping what it was handed.
fn keeper() -> (Registrar, Arc<Mutex<Vec<Value>>>) {
    let kept = Arc::new(Mutex::new(Vec::new()));
    let into = Arc::clone(&kept);
    let registrar: Registrar = Arc::new(move |record: &Value| {
        let mut kept = into.lock().expect("kept");
        kept.push(record.clone());
        Ok(i64::try_from(kept.len()).expect("a count") + 500)
    });
    (registrar, kept)
}

#[test]
fn a_node_joins_itself_through_a_coordinator_sealed_to_the_primary() {
    let (running, tap) = coordinator(None, None, None);
    let at = ws_of(&running);
    let (registrar, kept) = keeper();
    let mut primary = config(&at, "primary");
    primary.register_store = true;
    let primary = node_with(
        primary,
        TrustPolicy::new(Some(TOKEN), None, None),
        Some(registrar),
    );
    let mut joining = config(&at, "laptop");
    joining.register_to = Some("primary".into());
    joining.role = "peer".into();
    joining.zone = Some("home".into());
    joining.note = Some("desk; key=sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123".into());
    let laptop = node_with(joining, TrustPolicy::new(Some(TOKEN), None, None), None);
    let said = wait_for(&laptop, "registered");
    assert!(said.contains("event 501"), "{said}");
    let record = kept.lock().expect("kept")[0].clone();
    assert_eq!(
        json!([
            record["type"],
            record["name"],
            record["transport"],
            record["endpoint"],
            record["mode"],
            record["node_id"],
            record["pubkey"],
            record["role"],
            record["zone"],
            record["ssh"],
            record["via"]
        ]),
        json!([
            "node.joined",
            "laptop",
            "coord",
            at.url(),
            "relay",
            laptop.identity.node_id(),
            laptop.identity.pubkey_b64(),
            "peer",
            "home",
            null,
            []
        ])
    );
    assert!(record["cores"].as_u64().is_some_and(|cores| cores > 0));
    assert!(
        !record["note"]
            .as_str()
            .expect("a note")
            .contains("abcdefghijklmnop")
    );
    // The coordinator saw no inventory, note or name of the record.
    let seen = tap.lock().expect("tap").join("\n");
    for hidden in ["MemTotal", "desk;", "\"register\"", TOKEN] {
        assert!(!seen.contains(hidden), "the coordinator saw {hidden}");
    }
    // A node with another token: the primary cannot open it, nothing kept.
    let mut wrong = config(&at, "intruder");
    wrong.register_to = Some(primary.identity.node_id().to_string());
    let intruder = node_with(wrong, TrustPolicy::new(Some("other"), None, None), None);
    let said = wait_for(&intruder, "registration failed");
    assert!(said.contains("does not open"), "{said}");
    assert_eq!(kept.lock().expect("kept").len(), 1);
    drop((laptop, intruder, primary));
    running.stop();
}

#[test]
fn a_registration_names_its_sender_and_a_coordinator_url() {
    let identity = NodeIdentity::generate().expect("a key").0;
    let (from, from_pub) = (identity.node_id().to_string(), identity.pubkey_b64());
    let request = json!({"op": "register", "name": "laptop", "transport": "coord",
                         "endpoint": "wss://coord.example/mesh", "mode": "relay", "note": null,
                         "inventory": INVENTORY_OUT, "node_id": from, "pubkey": from_pub,
                         "role": "worker", "zone": null});
    let record = coord_registration(&request, &from, &from_pub).expect("a record");
    assert_eq!(record["transport"], "coord");
    let other = NodeIdentity::generate().expect("a key").0;
    for (key, value, why) in [
        ("node_id", json!(other.node_id()), "another node"),
        ("pubkey", json!(other.pubkey_b64()), "another node"),
        ("endpoint", json!("http://x"), "coordinator"),
        ("transport", json!("tcp"), "coord node"),
        ("mode", json!("direct"), "relay or hole_punch"),
        ("role", json!("primary"), "role"),
        ("zone", json!("-x"), "zone"),
        ("name", json!("-x"), "name"),
        ("note", json!("a\nb"), "note"),
        ("inventory", json!("@arch\n"), "cut short"),
    ] {
        let mut bad = request.clone();
        bad[key] = value;
        let error = coord_registration(&bad, &from, &from_pub).expect_err(key);
        assert!(error.contains(why), "{key}: {error}");
    }
}

/// Through a real store: a node joins itself through a coordinator, then
/// the primary's heartbeat probe (its route from the membership, the key
/// its join pinned) reaches it there.
#[test]
fn a_coord_node_registered_in_the_store_is_probed_by_its_membership() {
    let url_db = std::env::var("SPAWN_TEST_DATABASE_URL")
        .expect("SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip");
    let schema = spawn_postgres::testing::TestSchema::new("node_coord");
    let store = Arc::new(Mutex::new(
        spawn_postgres::PostgresStore::connect_in_schema(&url_db, &schema)
            .expect("a schema, migrated"),
    ));
    let registrar: Registrar = {
        let store = Arc::clone(&store);
        Arc::new(move |record: &Value| {
            store
                .lock()
                .expect("the store")
                .node_record(record)
                .map_err(|error| error.to_string())
        })
    };
    let (running, _) = coordinator(Some("lab"), None, None);
    let at = tcp_of(&running);
    let mut primary = config(&at, "primary");
    primary.register_store = true;
    let primary = node_with(
        primary,
        TrustPolicy::new(Some(TOKEN), None, None),
        Some(registrar),
    );
    let mut joining = config(&at, "laptop");
    joining.register_to = Some("primary".into());
    joining.zone = Some("lab".into());
    let laptop = node_with(joining, TrustPolicy::new(Some(TOKEN), None, None), None);
    wait_for(&laptop, "registered");
    let members = store
        .lock()
        .expect("the store")
        .node_members()
        .expect("members");
    let member = members
        .as_array()
        .into_iter()
        .flatten()
        .find(|member| member["name"] == "laptop")
        .expect("joined")
        .clone();
    assert_eq!(member["node_id"], laptop.identity.node_id());
    let route = crate::nodes::transport::Member::of(&member)
        .expect("a member")
        .route(&members)
        .expect("a route");
    assert!(
        matches!(&route, Route::Coord { node_id, pubkey: Some(_), .. } if node_id == laptop.identity.node_id())
    );
    let output = probe_route(&route, Op::Beat, T, Some(TOKEN)).expect("the heartbeat's probe");
    assert!(beat(&output).is_ok());
    // Its record holds the zone and the role; the web read drops the key.
    let nodes = store.lock().expect("the store").nodes().expect("nodes");
    let node = nodes
        .as_array()
        .into_iter()
        .flatten()
        .find(|n| n["name"] == "laptop")
        .expect("listed");
    assert_eq!(
        json!([node["transport"], node["zone"], node["role"]]),
        json!(["coord", "lab", "worker"])
    );
    // Joined again: refused in the writer's words.
    let mut again = config(&at, "laptop");
    again.register_to = Some("primary".into());
    let twice = node_with(again, TrustPolicy::new(Some(TOKEN), None, None), None);
    let said = wait_for(&twice, "registration failed");
    assert!(
        said.contains("joined already") && !said.contains("db error"),
        "{said}"
    );
    // Never probed with a key other than its join's.
    let Route::Coord { url, node_id, .. } = route else {
        unreachable!()
    };
    let other = NodeIdentity::generate().expect("a key").0;
    let forged = Route::Coord {
        url,
        node_id,
        pubkey: Some(other.pubkey_b64()),
    };
    assert!(
        probe_route(&forged, Op::Beat, T, Some(TOKEN))
            .expect_err("pinned")
            .contains("pinned")
    );
    drop((laptop, twice, primary));
    running.stop();
    let _ = PeerKeys::verify;
}

/// Review M5: a coordinator with SPAWN_COORD_TOKEN takes a ws:// or
/// tcp:// session on its hello's coord_auth alone (no bearer sent in the
/// clear), and refuses another token's.
#[test]
fn a_coordinator_token_is_proven_by_the_hello_on_ws_and_tcp() {
    let config = CoordConfig {
        ws: Some("127.0.0.1:0".into()),
        tcp: Some("127.0.0.1:0".into()),
        token: Some("coord-secret".into()),
        ..CoordConfig::default()
    };
    let running = super::start(&config).expect("a coordinator");
    let fields = HelloFields {
        kind: Kind::Client,
        name: "asker".into(),
        role: "client".into(),
        zone: None,
        udp_reflexive: None,
    };
    for at in [ws_of(&running), tcp_of(&running)] {
        for (token, taken) in [
            (Some("coord-secret"), true),
            (Some("other"), false),
            (None, false),
        ] {
            let identity = Arc::new(NodeIdentity::generate().expect("a key").0);
            let client = CoordClient::connect(
                &at,
                identity,
                TrustPolicy::default(),
                &fields,
                token,
                T,
                None,
            );
            assert_eq!(client.is_ok(), taken, "{at} {token:?}");
            if let Ok(client) = client {
                client.close();
            }
        }
    }
    running.stop();
}

/// Review M4: six probes with one identity in a row, beside a connector
/// whose key file the probes once inherited: all six answer, and the
/// connector is never dropped.
#[test]
fn probes_never_evict_a_connector_that_shares_their_key() {
    let (running, _) = coordinator(None, None, None);
    let at = ws_of(&running);
    let laptop = node(&at, "laptop");
    let id = laptop.identity.node_id().to_string();
    // The probes as a host with the connector's key file would make them:
    // the same node id as the connector.
    let mut shared = context(Some(TOKEN));
    shared.identity = Arc::clone(&laptop.identity);
    for round in 0..6 {
        let probed = probe(&at, &id, "beat", &shared)
            .unwrap_or_else(|error| panic!("round {round}: {error}"));
        assert_eq!(probed.reply["ok"], true, "round {round}");
        assert!(
            laptop
                .thread
                .as_ref()
                .is_some_and(|thread| !thread.is_finished()),
            "round {round}: the connector is still up"
        );
    }
    // The connector's session is the one that answered throughout.
    assert!(
        laptop
            .said
            .lock()
            .expect("said")
            .iter()
            .all(|line| !line.contains("connection closed")),
        "{:?}",
        laptop.said.lock().expect("said")
    );
    // A probe's own identity is fresh every time and names no join.
    let one = ProbeContext::from_env(Some(TOKEN)).expect("a context");
    let two = ProbeContext::from_env(Some(TOKEN)).expect("a context");
    assert_ne!(one.identity.node_id(), two.identity.node_id());
    drop(laptop);
    running.stop();
}

/// Review M2: a punch sends one fixed small datagram to a checked
/// target, or nothing: never with an unchecked nonce (60 KB of it), never
/// to loopback unless a test allows it, never twice to one address within
/// its window.
#[test]
fn a_punch_reflects_nothing_and_is_rate_limited() {
    let victim = UdpSocket::bind("127.0.0.1:0").expect("a port");
    victim
        .set_read_timeout(Some(Duration::from_millis(200)))
        .expect("a timeout");
    let target = victim.local_addr().expect("addr").to_string();
    let received = || {
        let mut buffer = vec![0_u8; 65_536];
        let mut got = Vec::new();
        while let Ok(read) = victim.recv(&mut buffer) {
            got.push(buffer[..read].to_vec());
        }
        got
    };
    let socket = || UdpSocket::bind("127.0.0.1:0").expect("a socket");
    let strict = super::Puncher::new(socket(), false);
    let huge = "n".repeat(60_000);
    for message in [
        json!({"type": "punch", "nonce": huge, "reflexive": target}),
        json!({"type": "punch", "nonce": "n0123456789abcdef", "reflexive": target}),
        json!({"type": "punch", "nonce": "n0123456789abcdef", "reflexive": "[::1]:9"}),
        json!({"type": "punch", "nonce": "n0123456789abcdef", "reflexive": "169.254.169.254:80"}),
        json!({"type": "punch", "nonce": "n0123456789abcdef", "reflexive": "0.0.0.0:9"}),
        json!({"type": "punch", "nonce": "n0123456789abcdef", "reflexive": null}),
    ] {
        assert!(!strict.punch(&message), "{message}");
    }
    assert!(received().is_empty(), "nothing reached the loopback socket");
    // Loopback allowed (tests): still no unchecked nonce; one fixed probe
    // per window to one address.
    let lenient = super::Puncher::new(socket(), true);
    assert!(!lenient.punch(&json!({"nonce": huge, "reflexive": target})));
    let mut sent = 0;
    for _ in 0..10 {
        sent +=
            usize::from(lenient.punch(&json!({"nonce": "n0123456789abcdef", "reflexive": target})));
    }
    assert_eq!(sent, 1, "rate limited per target");
    let got = received();
    assert_eq!(got.len(), 1);
    let probe: Value = serde_json::from_slice(&got[0]).expect("json");
    assert_eq!(
        probe,
        json!({"op": "punch_probe", "nonce": "n0123456789abcdef"})
    );
    assert!(got[0].len() < 80, "a small fixed datagram");
    // And the global bound: at most 16 in a window, over many addresses.
    let many = super::Puncher::new(socket(), false);
    let sent = (1..=40)
        .filter(|host| {
            many.admit(&json!({"nonce": "n0123456789abcdef",
                               "reflexive": format!("192.0.2.{host}:9")}))
                .is_some()
        })
        .count();
    assert_eq!(sent, 16);
}

/// Review S3: idle connections that never say hello hold at most
/// MAX_PENDING threads; one more is closed at once; once they go, a node
/// connects again. And a bounded count gives its slots back.
#[test]
fn connections_before_their_hello_are_bounded() {
    use std::io::Read as _;
    let (running, _) = coordinator(None, None, None);
    let at = running.tcp.expect("tcp");
    let idle: Vec<std::net::TcpStream> = (0..super::MAX_PENDING)
        .map(|_| std::net::TcpStream::connect(at).expect("connected"))
        .collect();
    // Let the accept loop take them all.
    std::thread::sleep(Duration::from_millis(500));
    let mut extra = std::net::TcpStream::connect(at).expect("connected");
    extra
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("a timeout");
    let begun = Instant::now();
    let mut byte = [0_u8; 1];
    assert_eq!(extra.read(&mut byte).ok(), Some(0), "closed at once");
    assert!(begun.elapsed() < Duration::from_secs(2));
    drop(idle);
    let deadline = Instant::now() + T;
    let fields = HelloFields {
        kind: Kind::Client,
        name: "asker".into(),
        role: "client".into(),
        zone: None,
        udp_reflexive: None,
    };
    loop {
        let identity = Arc::new(NodeIdentity::generate().expect("a key").0);
        if let Ok(client) = CoordClient::connect(
            &tcp_of(&running),
            identity,
            TrustPolicy::default(),
            &fields,
            None,
            T,
            None,
        ) {
            client.close();
            break;
        }
        assert!(Instant::now() < deadline, "never let in again");
        std::thread::sleep(Duration::from_millis(100));
    }
    running.stop();
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let held: Vec<_> = (0..3)
        .filter_map(|_| super::Held::take(&count, 2))
        .collect();
    assert_eq!(held.len(), 2);
    drop(held);
    assert_eq!(count.load(std::sync::atomic::Ordering::Acquire), 0);
}

/// Review S7 and S5: a summary of many peers still fits one frame; a
/// peer's error words are said as one short masked line.
#[test]
fn a_large_summary_fits_one_frame_and_peer_errors_are_short() {
    let peer = NodeIdentity::generate().expect("a key").0;
    let mut record = peer.public_info();
    record["name"] = json!("n".repeat(63));
    record["zone"] = json!("z".repeat(63));
    record["role"] = json!("worker");
    record["udp_reflexive"] = json!("[2001:db8::1]:65535");
    let many = vec![record; 5000];
    let fitted = super::fit_summary(many);
    assert!(
        !fitted.is_empty() && fitted.len() < 5000,
        "{}",
        fitted.len()
    );
    let message = json!({"type": "zone_summary", "zone": "z".repeat(63),
                         "peers_summary": fitted, "lease_epoch": u64::MAX, "lease_ms": 600_000});
    assert!(serde_json::to_vec(&message).expect("bytes").len() <= super::MAX_FRAME);
    let said = crate::nodes::crypto::said(&json!(format!("{}\nsecond line", "x".repeat(5000))));
    assert!(
        said.chars().count() <= 200 && !said.contains('\n'),
        "{said}"
    );
    assert_eq!(crate::nodes::crypto::said(&json!(7)), "refused");
    assert_eq!(crate::nodes::crypto::said(&json!("\u{7}")), "refused");
}
