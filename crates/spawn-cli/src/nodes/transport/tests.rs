//! The transports' rules without a socket: what an endpoint is, the hops
//! and modes a join may say, the route the membership gives, the loops
//! refused, the nonces and the forward's bounds.

use super::{
    Endpoint, MAX_HOPS, Mode, Route, Transport, accepted, forward, hop_names, loops_back, nonce,
    route, valid_nonce,
};
use serde_json::{Value, json};
use std::time::Duration;

#[test]
fn an_endpoint_is_host_and_port_with_its_transport_and_nothing_else() {
    for (text, transport, url) in [
        (
            "gw.example:7000",
            Some(Transport::Tcp),
            "tcp://gw.example:7000",
        ),
        ("tcp://gw:1", Some(Transport::Tcp), "tcp://gw:1"),
        ("udp://10.0.0.1:65535", None, "udp://10.0.0.1:65535"),
        ("[::1]:9", Some(Transport::Udp), "udp://[::1]:9"),
        ("tcp://[2001:db8::7]:7001", None, "tcp://[2001:db8::7]:7001"),
    ] {
        let endpoint = Endpoint::parse(text, transport).expect(text);
        assert_eq!(endpoint.url(), url);
    }
    let long = format!("{}:1", "a".repeat(260));
    for (text, transport) in [
        ("gw", Some(Transport::Tcp)),
        ("gw:0", Some(Transport::Tcp)),
        ("gw:65536", Some(Transport::Tcp)),
        ("gw:123456", Some(Transport::Tcp)),
        ("gw:", Some(Transport::Tcp)),
        ("user@gw:22", Some(Transport::Tcp)),
        ("tcp://user:pw@gw:22", None),
        ("-oProxyCommand=sh:22", Some(Transport::Tcp)),
        ("gw x:22", Some(Transport::Tcp)),
        ("gw\n:22", Some(Transport::Tcp)),
        ("udp://gw:1", Some(Transport::Tcp)),
        ("ssh://gw:22", None),
        ("http://gw:80", None),
        ("gw:1", None),
        ("gw:1", Some(Transport::Ssh)),
        ("[::1:22", Some(Transport::Tcp)),
        ("[zz]:22", Some(Transport::Tcp)),
        ("::1:22", Some(Transport::Tcp)),
        ("_gw:1", Some(Transport::Tcp)),
        (long.as_str(), Some(Transport::Tcp)),
    ] {
        assert!(
            Endpoint::parse(text, transport).is_err(),
            "{text:?} taken as an endpoint"
        );
    }
    assert!(
        Endpoint::parse("tcp://0.0.0.0:7", None)
            .expect("bind")
            .unspecified()
    );
    assert!(
        Endpoint::parse("udp://[::]:7", None)
            .expect("bind")
            .unspecified()
    );
    assert!(
        !Endpoint::parse("tcp://127.0.0.1:7", None)
            .expect("an address")
            .unspecified()
    );
}

#[test]
fn hops_are_at_most_eight_names_none_twice_never_the_node_itself() {
    let names = |hops: &[&str]| hops.iter().map(ToString::to_string).collect::<Vec<_>>();
    assert!(hop_names(&names(&[]), "edge").is_ok());
    assert!(hop_names(&names(&["relay-hk", "gw.1"]), "edge").is_ok());
    let nine: Vec<String> = (0..=MAX_HOPS).map(|n| format!("h{n}")).collect();
    for (hops, why) in [
        (nine, "at most 8"),
        (names(&["gw", "edge"]), "its own via"),
        (names(&["gw", "gw"]), "twice"),
        (names(&["-gw"]), "no node's name"),
        (names(&[""]), "no node's name"),
    ] {
        let error = hop_names(&hops, "edge").expect_err(why);
        assert!(error.contains(why), "{why}: {error}");
    }
}

#[test]
fn a_mode_defaults_to_relay_with_hops_and_direct_without() {
    assert_eq!(Mode::or_default(None, 0), Ok(Mode::Direct));
    assert_eq!(Mode::or_default(None, 2), Ok(Mode::Relay));
    assert_eq!(Mode::or_default(Some("hole_punch"), 0), Ok(Mode::HolePunch));
    assert_eq!(Mode::or_default(Some("relay"), 0), Ok(Mode::Relay));
    assert!(Mode::or_default(Some("direct"), 1).is_err());
    assert!(Mode::or_default(Some("mesh"), 0).is_err());
}

/// The store's membership: ssh nodes (one an old join, without transport),
/// tcp and udp nodes, one relayed.
fn members() -> Value {
    json!([
        {"name": "jump", "ssh": "ckl@jump", "join": 1},
        {"name": "bastion", "ssh": "ssh://ops@bastion:2222", "transport": "ssh",
         "endpoint": "ssh://ops@bastion:2222", "via": [], "join": 2},
        {"name": "gw", "ssh": null, "transport": "tcp", "endpoint": "tcp://gw:7000",
         "via": [], "join": 3},
        {"name": "relay-hk", "ssh": null, "transport": "udp", "endpoint": "udp://[2001:db8::7]:7001",
         "via": ["gw"], "join": 4},
    ])
}

#[test]
fn the_route_resolves_each_hop_from_the_membership_by_its_transport() {
    let hops = |names: &[&str]| names.iter().map(ToString::to_string).collect::<Vec<_>>();
    assert_eq!(
        route(
            Transport::Ssh,
            "ckl@deep",
            &hops(&["jump", "bastion"]),
            &members()
        ),
        Ok(Route::Ssh {
            destination: "ckl@deep".into(),
            jumps: vec!["ckl@jump".into(), "ssh://ops@bastion:2222".into()],
        })
    );
    let Ok(Route::Net(path)) = route(
        Transport::Udp,
        "udp://edge:9",
        &hops(&["gw", "relay-hk"]),
        &members(),
    ) else {
        panic!("a path");
    };
    assert_eq!(
        path.iter().map(Endpoint::url).collect::<Vec<_>>(),
        ["tcp://gw:7000", "udp://[2001:db8::7]:7001", "udp://edge:9"]
    );
    assert_eq!(
        route(Transport::Tcp, "tcp://edge:9", &[], &members()),
        Ok(Route::Net(vec![
            Endpoint::parse("tcp://edge:9", None).expect("an endpoint")
        ]))
    );
    for (transport, endpoint, via, why) in [
        (Transport::Ssh, "deep", hops(&["gw"]), "reached over tcp"),
        (
            Transport::Tcp,
            "edge:9",
            hops(&["jump"]),
            "reached over ssh",
        ),
        (
            Transport::Tcp,
            "edge:9",
            hops(&["nowhere"]),
            "hop nowhere is not joined",
        ),
        (
            Transport::Ssh,
            "-oProxyCommand=x",
            hops(&[]),
            "not an ssh destination",
        ),
    ] {
        let error = route(transport, endpoint, &via, &members()).expect_err(why);
        assert!(error.contains(why), "{why}: {error}");
    }
}

#[test]
fn a_hop_reached_through_the_node_itself_is_a_loop() {
    let hops = |names: &[&str]| names.iter().map(ToString::to_string).collect::<Vec<_>>();
    // relay-hk is reached via gw: gw may not join via relay-hk.
    assert!(loops_back(&members(), &hops(&["relay-hk"]), "gw"));
    assert!(!loops_back(&members(), &hops(&["relay-hk"]), "edge"));
    assert!(!loops_back(&members(), &hops(&[]), "gw"));
    // Deeper: a via b via c; c may not join via a.
    let chain = json!([
        {"name": "a", "transport": "tcp", "endpoint": "a:1", "via": ["b"]},
        {"name": "b", "transport": "tcp", "endpoint": "b:1", "via": ["c"]},
    ]);
    assert!(loops_back(&chain, &hops(&["a"]), "c"));
}

#[test]
fn every_nonce_is_new_and_a_message_carries_only_a_plain_one() {
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..1000 {
        let nonce = nonce();
        assert!(valid_nonce(&nonce), "{nonce}");
        assert!(seen.insert(nonce), "a nonce twice");
    }
    for bad in [
        "",
        "short",
        "has space in it ok ok",
        "{\"a\":1}xxxxxxxxxxxxxx",
    ] {
        assert!(!valid_nonce(bad), "{bad:?}");
    }
    assert!(!valid_nonce(&"a".repeat(81)));
}

#[test]
fn a_forward_is_bounded_in_hops_and_passes_on_less_time() {
    let hop = Endpoint::parse("tcp://h:1", None).expect("an endpoint");
    let envelope = forward(
        std::slice::from_ref(&hop),
        &json!({"nonce": nonce(), "op": "beat"}),
        Duration::from_secs(5),
        None,
    )
    .expect("a forward");
    assert_eq!(envelope["op"], "forward");
    assert_eq!(envelope["ttl"], 1);
    assert_eq!(envelope["hops"], json!(["tcp://h:1"]));
    assert!(envelope["timeout_ms"].as_u64() < Some(5000));
    assert!(forward(&[], &json!({}), Duration::from_secs(5), None).is_err());
    assert!(
        forward(
            &vec![hop.clone(); MAX_HOPS + 1],
            &json!({}),
            Duration::from_secs(5),
            None
        )
        .is_err()
    );
    assert!(
        forward(&[hop], &json!({}), Duration::from_millis(500), None).is_err(),
        "no time left"
    );
}

#[test]
fn a_refusal_is_said_in_one_masked_line() {
    let error = accepted(json!({"ok": false,
                                "error": "fatal: postgres://spawn:hunter2secret@db/spawn"}))
    .expect_err("refused");
    assert!(error.starts_with("the node refused"), "{error}");
    assert!(!error.contains("hunter2secret"), "{error}");
    assert!(accepted(json!({"ok": true, "output": "x"})).is_ok());
    assert!(accepted(json!({"ok": "true"})).is_err());
}

#[test]
fn a_relay_counts_loopback_link_local_and_their_mapped_forms_forbidden() {
    for ip in [
        "127.0.0.1",
        "127.255.0.9",
        "0.0.0.0",
        "169.254.169.254",
        "255.255.255.255",
        "224.0.0.1",
        "::1",
        "::",
        "fe80::1",
        "febf::1",
        "::ffff:127.0.0.1",
        "::ffff:0.0.0.0",
        "::ffff:169.254.1.1",
        "ff02::1",
    ] {
        assert!(super::forbidden(ip.parse().expect(ip)), "{ip}");
    }
    for ip in [
        "10.0.0.1",
        "192.168.1.9",
        "203.0.113.9",
        "2001:db8::7",
        "fec0::1",
        "::ffff:10.0.0.1",
    ] {
        assert!(!super::forbidden(ip.parse().expect(ip)), "{ip}");
    }
}
