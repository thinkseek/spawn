//! The nodes' rules, each without a machine: the probes' parsing (on
//! outputs read by hand from desktop-gr and spawn-runner, 2026-11-23),
//! the watch's decisions (lost, back, the ten-minute sample, the join a
//! probe was started for), and the bounded run with fake commands.

use super::{
    BEAT, HeartbeatSample, HeartbeatScheduler, INVENTORY, NodeStoreHandle, PROBE_EVERY_MS,
    ProbeClock, ProbeOutcome, RECORD_EVERY_MS, alert_plan, beat, bounded, inventory, lost_ms,
    ssh_command, valid_name, valid_ssh,
};
use serde_json::{Value, json};
use spawn_postgres::PostgresStore;
use std::collections::BTreeMap;
use std::process::Command;
use std::time::{Duration, Instant};

const KIB: u64 = 1024;

/// [`INVENTORY`]'s output on desktop-gr (an x86_64 container; nvidia-smi
/// is not inside it), meminfo cut to a few fields.
const DESKTOP: &str = "@arch
x86_64
@os
GNU/Linux
@kernel
7.3.0-4-cachyos
@cores
16
@cpu_max
1000000 100000
@meminfo
MemTotal:       32800868 kB
MemFree:         4591944 kB
MemAvailable:   24557592 kB
@mem_max
21474836480
@df
Filesystem     1024-blocks      Used Available Capacity Mounted on
/dev/nvme0n1p2   976745472 896741388  77481844      93% /home/owner
@gpu
@end
";

/// [`BEAT`]'s output on spawn-runner (arm64).
const RUNNER_BEAT: &str = "@loadavg
7.60 6.07 4.02 9/1149 23275
@meminfo
MemAvailable:    7101652 kB
@df
Filesystem     1024-blocks     Used Available Capacity Mounted on
/dev/vdb1        304414720 89253552 215161168      30% /
@end
";

#[test]
fn an_inventory_reads_every_fact_of_a_node_and_each_gpu() {
    assert_eq!(
        inventory(DESKTOP).expect("a whole inventory"),
        json!({
            "arch": "x86_64", "os": "GNU/Linux", "kernel": "7.3.0-4-cachyos", "cores": 16,
            "cpu_max": "1000000 100000", "mem_total": 32_800_868 * KIB,
            "mem_max": 21_474_836_480_u64, "disk_free": 77_481_844 * KIB, "gpu": null,
        })
    );
    // No cgroup limit (`max`), and the cards nvidia-smi names in the
    // format the inventory asks (one memory not read).
    let host = DESKTOP
        .replace("21474836480", "max")
        .replace("1000000 100000", "max 100000")
        .replace(
            "@gpu\n",
            "@gpu\nNVIDIA GeForce RTX 4070 Ti SUPER, 16376\nNVIDIA A100-SXM4-80GB, [N/A]\n",
        );
    let read = inventory(&host).expect("a whole inventory");
    assert_eq!(
        (&read["mem_max"], &read["cpu_max"]),
        (&Value::Null, &json!("max 100000"))
    );
    assert_eq!(
        read["gpu"],
        json!([{"name": "NVIDIA GeForce RTX 4070 Ti SUPER", "memory_mb": 16_376},
               {"name": "NVIDIA A100-SXM4-80GB", "memory_mb": null}])
    );
    assert!(INVENTORY.contains("nvidia-smi") && !BEAT.contains("nvidia-smi"));
}

#[test]
fn a_probe_cut_short_or_missing_a_fact_is_no_answer() {
    assert!(
        inventory(&DESKTOP.replace("@end\n", ""))
            .unwrap_err()
            .contains("@end")
    );
    for (cut, what) in [("@cores\n16\n", "cores"), ("MemTotal:", "MemTotal")] {
        let error = inventory(&DESKTOP.replace(cut, "")).unwrap_err();
        assert!(error.contains(what), "{what}: {error}");
    }
    let escaped = DESKTOP.replace("x86_64\n", "x86\u{1b}[31m\n");
    assert!(
        inventory(&escaped).unwrap_err().contains("arch"),
        "no control character"
    );
    assert!(beat("").is_err());
    let error = beat(&RUNNER_BEAT.replace("MemAvailable", "MemFree")).unwrap_err();
    assert!(error.contains("MemAvailable"), "{error}");
}

#[test]
fn a_heartbeat_reads_load_available_memory_and_free_disk() {
    assert_eq!(
        beat(RUNNER_BEAT).expect("a whole beat"),
        HeartbeatSample {
            load1: 7.6,
            mem_available: 7_101_652 * KIB,
            disk_free: 215_161_168 * KIB,
        }
    );
    let spaced = RUNNER_BEAT.replace("30% /", "30% /mnt/my disk");
    assert_eq!(beat(&spaced).expect("whole").disk_free, 215_161_168 * KIB);
}

#[test]
fn names_and_ssh_destinations_are_what_ssh_reads_as_a_destination_only() {
    for name in ["runner", "desktop-gr", "x", "a.b_c-9"] {
        assert!(valid_name(name), "{name}");
    }
    for name in ["", "-x", ".x", "a b", "a/b", &"x".repeat(64)] {
        assert!(!valid_name(name), "{name}");
    }
    for ssh in [
        "spawn-runner",
        "owner@desktop",
        "ssh://owner@192.0.2.7:2222",
        "ssh://host",
    ] {
        assert!(valid_ssh(ssh), "{ssh}");
    }
    for ssh in [
        "-oProxyCommand=sh",
        "owner:pw@host",
        "a b",
        "host\n",
        "ssh://host:0",
        "ssh://host:65536",
        "ssh://host:",
        "@host",
        &format!("h{}", "x".repeat(255)),
    ] {
        assert!(!valid_ssh(ssh), "{ssh:?}");
    }
}

#[test]
fn the_lost_bound_is_whole_seconds_from_thirty_and_a_bad_one_is_said() {
    assert_eq!(lost_ms(None), (60_000, None));
    assert_eq!(lost_ms(Some("30")), (30_000, None));
    for bad in ["0", "29", "1.5", "-60", "x", "86401"] {
        let (ms, warning) = lost_ms(Some(bad));
        assert_eq!(ms, 60_000, "{bad}");
        assert!(warning.expect("said").contains("SPAWN_NODE_LOST_SECS"));
    }
}

/// The store's membership of one node.
fn member(join: i64, lost: &Value, beat_ms: Option<i64>) -> Value {
    json!([{"name": "runner", "ssh": "spawn-runner", "join": join, "lost": lost,
            "beat_ms": beat_ms}])
}

/// A moment whose two clocks agree.
fn at(ms: i64) -> ProbeClock {
    ProbeClock { mono: ms, wall: ms }
}

fn answer(join: i64, ms: i64) -> ProbeOutcome {
    ProbeOutcome {
        name: "runner".into(),
        join,
        at: at(ms),
        answer: Ok(HeartbeatSample {
            load1: 0.5,
            mem_available: 8 * KIB * KIB * KIB,
            disk_free: 100 * KIB * KIB * KIB,
        }),
    }
}

fn failure(join: i64, ms: i64, why: &str) -> ProbeOutcome {
    ProbeOutcome {
        answer: Err(why.into()),
        ..answer(join, ms)
    }
}

/// Decide at `now`, the store taking every proposal (`written`) or none.
fn decide(watch: &mut HeartbeatScheduler, now: ProbeClock, written: bool) -> Vec<Value> {
    let (proposals, _, _) = watch.decide(now);
    if written {
        for proposal in &proposals {
            watch.applied(proposal);
        }
    }
    proposals
        .into_iter()
        .map(|proposal| proposal.record)
        .collect()
}

fn types(records: &[Value]) -> Vec<&str> {
    records
        .iter()
        .map(|record| record["type"].as_str().unwrap_or("?"))
        .collect()
}

#[test]
fn a_silent_node_is_lost_once_and_back_at_its_next_answer() {
    let mut watch = HeartbeatScheduler::new(60_000);
    watch.sync(&member(7, &Value::Null, None), at(0));
    let (records, probes, _) = watch.decide(at(0));
    assert!(records.is_empty());
    assert_eq!((probes.len(), probes[0].join), (1, 7));
    assert_eq!(probes[0].ssh, "spawn-runner");
    watch.take(failure(7, 1000, "ssh exited 255: Connection refused"));
    // Never answered since the watch began: the bound counts from then,
    // and the record claims no answer it never had.
    let records = decide(&mut watch, at(60_000), true);
    assert_eq!(types(&records), ["node.lost"]);
    assert_eq!(records[0]["join"], 7);
    assert_eq!(records[0]["last_seen_ms"], Value::Null);
    assert_eq!(records[0]["silent_ms"], 60_000);
    assert_eq!(records[0]["error"], "ssh exited 255: Connection refused");
    assert!(
        decide(&mut watch, at(120_000), true).is_empty(),
        "lost once"
    );

    watch.take(answer(7, 130_000));
    let records = decide(&mut watch, at(130_000), true);
    assert_eq!(types(&records), ["node.back", "node.heartbeat"]);
    assert_eq!(records[0]["down_ms"], Value::Null, "no answer known before");
    watch.take(failure(7, 150_000, "no answer in 15 s"));
    let records = decide(&mut watch, at(190_000), true);
    assert_eq!(types(&records), ["node.lost"]);
    assert_eq!(records[0]["last_seen_ms"], 130_000);
    assert_eq!(records[0]["silent_ms"], 60_000);
    watch.take(answer(7, 200_000));
    assert_eq!(decide(&mut watch, at(200_000), true)[0]["down_ms"], 70_000);
}

#[test]
fn the_bounds_run_on_the_monotonic_clock_whatever_the_wall_clock_does() {
    let mut watch = HeartbeatScheduler::new(60_000);
    watch.sync(&member(7, &Value::Null, None), at(0));
    watch.decide(at(0));
    watch.take(answer(7, 1000));
    decide(&mut watch, at(1000), true);
    // The wall clock jumps an hour ahead, then a day back: nothing is
    // lost early, and a probe is due on its own interval only.
    let ahead = ProbeClock {
        mono: 2000,
        wall: 3_600_000,
    };
    assert!(decide(&mut watch, ahead, true).is_empty());
    assert!(watch.decide(ahead).1.is_empty(), "no probe before 15 s");
    let behind = ProbeClock {
        mono: 16_000,
        wall: -86_400_000,
    };
    assert_eq!(watch.decide(behind).1.len(), 1);
    let records = decide(
        &mut watch,
        ProbeClock {
            mono: 61_000,
            wall: 5,
        },
        true,
    );
    assert_eq!(types(&records), ["node.lost"]);
    assert_eq!(records[0]["silent_ms"], 60_000);
    assert_eq!(
        records[0]["last_seen_ms"], 1000,
        "the answer's own wall time"
    );
}

#[test]
fn a_record_the_store_did_not_take_is_proposed_again_with_its_evidence() {
    let mut watch = HeartbeatScheduler::new(60_000);
    watch.sync(&member(7, &Value::Null, Some(0)), at(0));
    watch.decide(at(0));
    watch.take(answer(7, 10_000));
    decide(&mut watch, at(10_000), true);
    // Lost, not written: proposed again at the next decision.
    assert_eq!(types(&decide(&mut watch, at(70_000), false)), ["node.lost"]);
    assert_eq!(types(&decide(&mut watch, at(71_000), true)), ["node.lost"]);
    // Back at 100 s, not written; the answer at 130 s proposes it again,
    // its down time still counted from the last answer before the loss.
    watch.take(answer(7, 100_000));
    let records = decide(&mut watch, at(100_000), false);
    assert_eq!(types(&records), ["node.back"]);
    assert_eq!(records[0]["down_ms"], 90_000);
    watch.take(answer(7, 130_000));
    let records = decide(&mut watch, at(130_000), true);
    assert_eq!(types(&records), ["node.back"]);
    assert_eq!(records[0]["down_ms"], 120_000);

    // A restarted worker takes the open loss from the store; after a
    // failed write it takes the store's word again, and the down time
    // stays counted from the recorded last answer.
    let mut watch = HeartbeatScheduler::new(60_000);
    let lost = json!({"event_id": 11, "last_seen_ms": 40_000, "silent_ms": 60_000,
                      "open": true});
    watch.sync(&member(7, &lost, Some(0)), at(500_000));
    watch.decide(at(500_000));
    watch.take(answer(7, 501_000));
    assert_eq!(
        decide(&mut watch, at(501_000), false)[0]["down_ms"],
        461_000
    );
    watch.resync();
    watch.sync(&member(7, &lost, Some(0)), at(502_000));
    watch.take(answer(7, 515_000));
    let records = decide(&mut watch, at(515_000), true);
    assert_eq!(types(&records), ["node.back"], "a heartbeat 0 s is recent");
    assert_eq!(records[0]["down_ms"], 475_000);
}

#[test]
fn one_probe_is_in_flight_and_a_node_that_answers_is_recorded_every_ten_minutes() {
    let mut watch = HeartbeatScheduler::new(60_000);
    watch.sync(&member(7, &Value::Null, None), at(0));
    let mut recorded = Vec::new();
    let mut started = 0;
    let mut now = 0;
    while now <= 2 * RECORD_EVERY_MS {
        let (proposals, probes, _) = watch.decide(at(now));
        for proposal in &proposals {
            watch.applied(proposal);
        }
        recorded.extend(proposals.into_iter().map(|proposal| proposal.record));
        started += probes.len();
        if now % (4 * PROBE_EVERY_MS) != 2 * PROBE_EVERY_MS {
            // Three probes in four answer at once; the fourth hangs past
            // the next interval, and none starts beside it.
            watch.take(answer(7, now + 100));
        }
        now += PROBE_EVERY_MS;
    }
    recorded.extend(decide(&mut watch, at(now), true));
    assert_eq!(
        types(&recorded),
        ["node.heartbeat"; 3],
        "at 0 s, 10 and 20 min"
    );
    assert_eq!(
        started, 61,
        "81 intervals, one in four skipped while a probe hangs"
    );
    assert!(recorded.iter().all(|record| record["join"] == 7));
}

#[test]
fn a_probe_of_a_removed_join_changes_nothing_after_the_node_joins_again() {
    let mut watch = HeartbeatScheduler::new(60_000);
    watch.sync(&member(7, &Value::Null, None), at(0));
    assert_eq!(
        watch.decide(at(0)).1.len(),
        1,
        "a probe of join 7 in flight"
    );
    // Removed and joined again (event 9) while that probe runs.
    watch.sync(&member(9, &Value::Null, None), at(5000));
    let (_, probes, _) = watch.decide(at(5000));
    assert_eq!(probes.iter().map(|work| work.join).collect::<Vec<_>>(), [9]);
    // The old endpoint answers: nothing is recorded for the new join.
    watch.take(answer(7, 6000));
    assert!(decide(&mut watch, at(6000), true).is_empty());
    // The new one is silent: lost on its own bound, from its own start.
    let records = decide(&mut watch, at(65_000), true);
    assert_eq!(types(&records), ["node.lost"]);
    assert_eq!(records[0]["join"], 9);
    assert_eq!(records[0]["last_seen_ms"], Value::Null);
    watch.take(failure(7, 66_000, "old"));
    watch.take(answer(9, 70_000));
    assert_eq!(
        types(&decide(&mut watch, at(70_000), true)),
        ["node.back", "node.heartbeat"]
    );
}

/// Two nodes' membership: `runner`, its latest loss `lost` (or none), and
/// `desktop`, never lost.
fn runner_lost(lost: &Value) -> Value {
    json!([
        {"name": "desktop", "ssh": "d", "join": 2, "lost": null, "beat_ms": null},
        {"name": "runner", "ssh": "r", "join": 3, "lost": lost, "beat_ms": null},
    ])
}

fn keys(records: &[Value]) -> Vec<String> {
    records
        .iter()
        .map(|record| format!("{} {}", record["type"], record["key"]))
        .collect()
}

#[test]
fn the_alert_plan_tells_each_loss_and_its_end_in_order() {
    let none = BTreeMap::new();
    let lost = |open: bool, alerted: bool| {
        json!({"event_id": 12, "last_seen_ms": 1000, "silent_ms": 61_000,
               "error": "refused", "open": open, "alerted": alerted})
    };
    let fired = r#""alert.fired" "node_lost:runner:12""#;
    let cleared = r#""alert.cleared" "node_lost:runner:12""#;
    // A loss with no alert: fired, with its evidence.
    let plan = alert_plan(&runner_lost(&lost(true, false)), &none);
    assert_eq!(keys(&plan), [fired]);
    assert_eq!(plan[0]["subject"], "node runner");
    assert_eq!(plan[0]["evidence"]["silent_ms"], 61_000);
    assert!(
        plan[0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("61 s"))
    );
    // Lost and back between two reads, or its fire failed and the back
    // came before the retry: fired, then cleared.
    let plan = alert_plan(&runner_lost(&lost(false, false)), &none);
    assert_eq!(keys(&plan), [fired, cleared]);
    let open: BTreeMap<String, String> =
        [("node_lost:runner:12".to_string(), "runner".to_string())].into();
    assert!(
        alert_plan(&runner_lost(&lost(true, true)), &open).is_empty(),
        "one alert"
    );
    let back = alert_plan(&runner_lost(&lost(false, true)), &open);
    assert_eq!(keys(&back), [cleared]);
    assert_eq!(back[0]["evidence"]["ended"], "back");
    // Removed (no longer listed), or joined again since.
    assert_eq!(
        alert_plan(&json!([]), &open)[0]["evidence"]["ended"],
        "gone"
    );
    assert!(alert_plan(&runner_lost(&Value::Null), &none).is_empty());
}

/// The node's `node_lost` alerts in the store, `(key, state)` by key.
fn node_alerts(store: &PostgresStore) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = store
        .alerts(0)
        .expect("the alerts")
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| row["kind"] == "node_lost")
        .map(|row| (row["key"].to_string(), row["state"].to_string()))
        .collect();
    rows.sort();
    rows
}

/// The worker's alert step against a real store, on a connection of its
/// own ([`NodeStoreHandle`]): each read of the membership writes what it asks for.
#[test]
fn the_worker_raises_and_clears_node_lost_in_the_store_in_order() {
    let url = std::env::var("SPAWN_TEST_DATABASE_URL")
        .expect("SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip");
    let schema = spawn_postgres::testing::TestSchema::new("node_alerts");
    let store = PostgresStore::connect_in_schema(&url, &schema).expect("a schema, migrated");
    let mut joined = inventory(DESKTOP).expect("an inventory");
    for (key, value) in [
        ("type", json!("node.joined")),
        ("name", json!("runner")),
        ("ssh", json!("spawn-runner")),
        ("note", Value::Null),
        ("exit_country", Value::Null),
    ] {
        joined[key] = value;
    }
    let join = store.node_record(&joined).expect("joined");
    let crossing = |kind: &str| {
        let record = if kind == "node.lost" {
            json!({"type": kind, "name": "runner", "join": join, "silent_ms": 60_000,
                   "last_seen_ms": null, "error": null})
        } else {
            json!({"type": kind, "name": "runner", "join": join, "down_ms": null})
        };
        store.node_record(&record).expect(kind)
    };
    let step = |db: &mut NodeStoreHandle| {
        let members = db.store.node_members().expect("the members");
        let mut lines = Vec::new();
        db.write(
            &mut HeartbeatScheduler::new(60_000),
            Vec::new(),
            Some(&members),
            &mut lines,
        )
        .expect("written");
        lines.len()
    };
    let state =
        |id: i64, state: &str| (format!("\"node_lost:runner:{id}\""), format!("\"{state}\""));
    let connector = store.connector(Duration::from_secs(5));

    // Lost and back before the worker read: both told, in order.
    let first = crossing("node.lost");
    crossing("node.back");
    let mut db = NodeStoreHandle::connect(&connector).expect("connected");
    assert_eq!(step(&mut db), 2);
    assert_eq!(node_alerts(&store), [state(first, "cleared")]);
    assert_eq!(step(&mut db), 0, "nothing twice");
    // Lost again; a worker started since reads the open alert back and
    // clears it at the back.
    let second = crossing("node.lost");
    assert_eq!(step(&mut db), 1);
    let mut restarted = NodeStoreHandle::connect(&connector).expect("connected again");
    assert_eq!(restarted.alerts.len(), 1);
    crossing("node.back");
    assert_eq!(step(&mut restarted), 1);
    // Lost, then removed: its alert is cleared too.
    let third = crossing("node.lost");
    step(&mut restarted);
    store
        .node_record(&json!({"type": "node.removed", "name": "runner"}))
        .expect("removed");
    assert_eq!(step(&mut restarted), 1);
    let mut want = vec![
        state(first, "cleared"),
        state(second, "cleared"),
        state(third, "cleared"),
    ];
    want.sort();
    assert_eq!(node_alerts(&store), want);
}

#[test]
fn ssh_runs_with_no_environment_shared_connection_or_forwarding() {
    let command = ssh_command("owner@desktop", BEAT);
    assert_eq!(command.get_program(), "ssh");
    let args: Vec<String> = command
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    for option in [
        "BatchMode=yes",
        "StrictHostKeyChecking=yes",
        "ControlMaster=no",
        "ControlPath=none",
        "ClearAllForwardings=yes",
        "ForwardAgent=no",
    ] {
        assert!(args.iter().any(|arg| arg == option), "{option}: {args:?}");
    }
    assert_eq!(&args[args.len() - 3..], ["--", "owner@desktop", BEAT]);
    let kept: Vec<String> = command
        .get_envs()
        .filter(|(_, value)| value.is_some())
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .collect();
    assert!(
        kept.iter()
            .all(|name| ["PATH", "HOME", "USER", "LOGNAME", "SSH_AUTH_SOCK"]
                .contains(&name.as_str())),
        "{kept:?}"
    );
}

fn sh(script: &str) -> Command {
    let mut command = Command::new("sh");
    command.args(["-c", script]);
    command
}

#[test]
fn a_bounded_run_says_its_output_or_its_last_error_line() {
    assert_eq!(
        bounded(sh("echo @end"), Duration::from_secs(5)),
        Ok("@end\n".into())
    );
    let error = bounded(
        sh("echo one >&2; echo 'Permission denied' >&2; exit 255"),
        Duration::from_secs(5),
    )
    .unwrap_err();
    assert_eq!(error, "ssh exited 255: Permission denied");
    // Masked before any log line or record holds it.
    let error = bounded(
        sh("echo 'fatal: postgres://spawn:hunter2secret@db/spawn' >&2; exit 1"),
        Duration::from_secs(5),
    )
    .unwrap_err();
    assert!(!error.contains("hunter2secret"), "{error}");
}

/// Whether process `pid` is gone (or a zombie, dead).
fn gone(pid: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let state = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        if state.is_empty() || state.contains(") Z ") {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_run_past_its_deadline_is_killed_with_everything_it_started() {
    let pidfile = std::env::temp_dir().join(format!("spawn-node-probe-{}", std::process::id()));
    for (script, why) in [
        // Hangs, with a child of its own.
        ("sleep 30 & echo $! > {pid}; wait", "no answer in 1 s"),
        // Exits at once, a child it left holding its output.
        (
            "sleep 30 & echo $! > {pid}; exit 0",
            "its output was not closed in 1 s",
        ),
    ] {
        let _ = std::fs::remove_file(&pidfile);
        let script = script.replace("{pid}", &pidfile.display().to_string());
        let begun = Instant::now();
        assert_eq!(
            bounded(sh(&script), Duration::from_secs(1)).unwrap_err(),
            why
        );
        assert!(
            begun.elapsed() < Duration::from_secs(3),
            "{:?}",
            begun.elapsed()
        );
        let pid = std::fs::read_to_string(&pidfile).expect("the child's pid");
        assert!(gone(pid.trim()), "{script}: its child outlived it");
    }
    let _ = std::fs::remove_file(&pidfile);
}
