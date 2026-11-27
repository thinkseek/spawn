//! Hardening tests for SSH-only P1 node commands and Phase 2d mesh retirement:
//! probe flag parsing, retired command/flag refusal, SSH jump-host validation,
//! and panic-free handling of historical non-SSH records in the heartbeat worker.

use super::commands::{add_target, probe_flags, probe_json};
use super::transport::{Op, Route, loops_back, probe_route, route};
use super::{HeartbeatScheduler, ProbeClock, command, start};
use serde_json::json;
use std::time::{Duration, Instant};

const BEAT_OUT: &str = "@loadavg\n0.5 0.1 0.1 1/1 1\n@meminfo\nMemAvailable: 4000000 kB\n\
@df\nFilesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 100 50 50 50% /\n@end\n";

fn words(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_string).collect()
}

/// `spawn node probe` parses `--op inventory|beat` and `--json` cleanly and
/// formats JSON facts for SSH probes.
#[test]
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
        &json!("ssh"),
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
        json!(["edge", "beat", "ssh", 12, 0.5])
    );
}

/// Retired Issue #85/#87 mesh subcommands and flags fail closed with a clear
/// retirement message pointing to Go `spawn-node` (or `--ssh`).
#[test]
fn retired_mesh_commands_and_flags_fail_closed_with_guidance() {
    for args in [
        "serve --tcp 127.0.0.1:7000",
        "register --to 127.0.0.1:7000 --name edge --listen tcp://edge:7000",
        "coord --ws 127.0.0.1:8443",
        "connect --coord wss://c.example/mesh --name edge",
    ] {
        let error = command(&words(args)).expect_err(args);
        assert!(
            error.contains("retired") && error.contains("spawn-node"),
            "{args}: {error}"
        );
    }

    let target =
        |text: &str| add_target("laptop", &crate::Flags::parse(&words(text)).expect("flags"));
    for args in [
        "--tcp gw:7000",
        "--udp 203.0.113.9:7001",
        "--coord wss://coord.example/mesh --peer laptop",
        "--ssh ckl@laptop --node-id deadbeef",
        "--ssh ckl@laptop --peer laptop",
    ] {
        let error = target(args).expect_err(args);
        assert!(
            error.contains("retired") && error.contains("spawn-node"),
            "{args}: {error}"
        );
    }
}

/// SSH `--via` jump-host resolution rejects loops, duplicates, non-SSH hops,
/// and ssh option-injection attempts.
#[test]
fn ssh_jump_host_validation_rejects_loops_non_ssh_hops_and_option_injection() {
    let members = json!([
        {"name": "jump", "ssh": "ckl@jump", "transport": "ssh", "via": []},
        {"name": "deep", "ssh": "ckl@deep", "transport": "ssh", "via": ["jump"]},
        {"name": "legacy-tcp", "ssh": null, "transport": "tcp", "endpoint": "tcp://10.0.0.2:7000", "via": []},
        {"name": "evil-hop", "ssh": "-oProxyCommand=sh", "transport": "ssh", "via": []},
    ]);
    assert!(loops_back(&members, &words("deep"), "jump"));
    assert!(!loops_back(&members, &words("deep"), "fresh"));

    assert!(
        route("ckl@target", &words("legacy-tcp"), &members)
            .expect_err("tcp hop")
            .contains("ssh nodes")
    );
    assert!(
        route("ckl@target", &words("evil-hop"), &members)
            .expect_err("evil hop")
            .contains("no ssh destination")
    );
    assert_eq!(
        probe_route(
            &Route::Ssh {
                destination: "ssh://user@host:0".into(),
                jumps: Vec::new(),
            },
            Op::Beat,
            Duration::from_secs(1),
        ),
        Err("not an ssh destination".to_string())
    );
}

/// Historical non-SSH or incomplete DB records never panic in `HeartbeatScheduler`
/// or `start()`, returning a clean error outcome instead.
#[test]
fn historical_non_ssh_records_never_panic_in_heartbeat_start() {
    let members = json!([
        {"name": "old-tcp", "ssh": null, "transport": "tcp", "endpoint": "tcp://10.0.0.2:7000",
         "via": [], "join": 10, "lost": null, "beat_ms": null},
        {"name": "old-udp", "ssh": null, "transport": "udp", "endpoint": "udp://10.0.0.3:7001",
         "via": ["old-tcp"], "join": 11, "lost": null, "beat_ms": null},
        {"name": "old-coord", "ssh": null, "transport": "coord", "endpoint": "wss://c.example/mesh",
         "via": [], "join": 12, "lost": null, "beat_ms": null},
        {"name": "broken-ssh", "ssh": null, "transport": "ssh", "endpoint": null,
         "via": [], "join": 13, "lost": null, "beat_ms": null},
    ]);
    let mut watch = HeartbeatScheduler::new(60_000);
    let origin = Instant::now();
    watch.sync(&members, ProbeClock { mono: 0, wall: 0 });
    let (_, probes, _) = watch.decide(ProbeClock { mono: 0, wall: 0 });
    assert_eq!(probes.len(), 4);

    let (done, ends) = std::sync::mpsc::channel();
    for job in probes {
        start(job, &done, origin);
        let outcome = ends
            .recv_timeout(Duration::from_secs(1))
            .expect("outcome sent");
        assert!(outcome.answer.is_err(), "{}", outcome.name);
        watch.take(outcome);
    }
}
