//! The SSH transport's rules without a host: the hops a join may say, the
//! route the membership gives, the loops refused, and retired transport
//! handling.

use super::{MAX_HOPS, Member, Op, Route, hop_names, loops_back, probe_route, route};
use serde_json::{Value, json};
use std::time::Duration;

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

/// The store's membership: SSH nodes (one an old join without `transport`)
/// and historical non-SSH joins (`tcp`, `udp`, `coord`) from before Phase 2d.
fn members() -> Value {
    json!([
        {"name": "jump", "ssh": "ckl@jump", "join": 1},
        {"name": "bastion", "ssh": "ssh://ops@bastion:2222", "transport": "ssh",
         "endpoint": "ssh://ops@bastion:2222", "via": [], "join": 2},
        {"name": "gw", "ssh": null, "transport": "tcp", "endpoint": "tcp://gw:7000",
         "via": [], "join": 3},
        {"name": "relay-hk", "ssh": null, "transport": "udp", "endpoint": "udp://[2001:db8::7]:7001",
         "via": ["gw"], "join": 4},
        {"name": "mesh-node", "ssh": null, "transport": "coord", "endpoint": "wss://c.example/mesh",
         "via": [], "join": 5},
    ])
}

#[test]
fn the_route_resolves_ssh_hops_and_rejects_retired_or_invalid_routes() {
    let hops = |names: &[&str]| names.iter().map(ToString::to_string).collect::<Vec<_>>();
    assert_eq!(
        route("ckl@deep", &hops(&["jump", "bastion"]), &members()),
        Ok(Route::Ssh {
            destination: "ckl@deep".into(),
            jumps: vec!["ckl@jump".into(), "ssh://ops@bastion:2222".into()],
        })
    );
    for (destination, via, why) in [
        ("deep", hops(&["gw"]), "reached over tcp"),
        ("deep", hops(&["mesh-node"]), "reached over coord"),
        ("deep", hops(&["nowhere"]), "hop nowhere is not joined"),
        ("-oProxyCommand=x", hops(&[]), "not an ssh destination"),
    ] {
        let error = route(destination, &via, &members()).expect_err(why);
        assert!(error.contains(why), "{why}: {error}");
    }
}

#[test]
fn historical_non_ssh_members_report_retired_transport_without_panicking() {
    let all = members();
    for (name, expected_transport) in [("gw", "tcp"), ("relay-hk", "udp"), ("mesh-node", "coord")] {
        let entry = all
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == name)
            .expect(name);
        let member = Member::of(entry);
        let error = member.route(&all).expect_err("retired transport");
        assert!(
            error.contains("retired") && error.contains(expected_transport),
            "{name}: {error}"
        );
    }
    let broken_ssh = json!({"name": "broken", "transport": "ssh", "ssh": null, "endpoint": null});
    let error = Member::of(&broken_ssh)
        .route(&all)
        .expect_err("missing ssh destination");
    assert!(error.contains("ssh destination"), "{error}");
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
        {"name": "a", "transport": "ssh", "ssh": "a", "via": ["b"]},
        {"name": "b", "transport": "ssh", "ssh": "b", "via": ["c"]},
    ]);
    assert!(loops_back(&chain, &hops(&["a"]), "c"));
}

#[test]
fn probe_route_rejects_unsafe_ssh_destinations_or_jumps() {
    let bad_dest = Route::Ssh {
        destination: "-oProxyCommand=sh".into(),
        jumps: Vec::new(),
    };
    assert_eq!(
        probe_route(&bad_dest, Op::Beat, Duration::from_secs(1)),
        Err("not an ssh destination".to_string())
    );
    let bad_jump = Route::Ssh {
        destination: "ckl@deep".into(),
        jumps: vec!["-oProxyCommand=sh".into()],
    };
    assert_eq!(
        probe_route(&bad_jump, Op::Beat, Duration::from_secs(1)),
        Err("not an ssh destination".to_string())
    );
}
