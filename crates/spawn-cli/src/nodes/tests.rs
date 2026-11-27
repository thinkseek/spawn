//! The nodes' rules, each without a machine: the probes' parsing (on
//! outputs read by hand from desktop-gr and spawn-runner, 2026-11-23),
//! the watch's decisions (lost, back, the ten-minute sample, the join a
//! probe was started for), and the bounded run with fake commands.

use super::power::PowerTelemetry;
use super::{
    BEAT, HeartbeatSample, HeartbeatScheduler, INVENTORY, NodeStoreHandle, PROBE_EVERY_MS,
    ProbeClock, ProbeOutcome, RECORD_EVERY_MS, alert_plan, beat, bounded, inventory, lost_ms,
    ssh_command_via, valid_name, valid_ssh,
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
/dev/nvme0n1p2   976745472 896741388  77481844      93% /home/ckl
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
            power: None,
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
        "ckl@desktop",
        "ssh://ckl@10.0.0.7:2222",
        "ssh://host",
    ] {
        assert!(valid_ssh(ssh), "{ssh}");
    }
    for ssh in [
        "-oProxyCommand=sh",
        "ckl:pw@host",
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
            power: None,
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
    assert_eq!(
        probes[0].route,
        Ok(super::Route::Ssh {
            destination: "spawn-runner".into(),
            jumps: Vec::new()
        })
    );
    assert_eq!((probes.len(), probes[0].join), (1, 7));
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
    let command = ssh_command_via("ckl@desktop", &[], BEAT);
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
    assert_eq!(&args[args.len() - 3..], ["--", "ckl@desktop", BEAT]);
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

/// The words `sh` reads `command` as, `ssh` standing for itself (a shell
/// function that prints each argument on a line of its own).
fn shell_words(command: &str) -> Vec<String> {
    let script = format!("ssh() {{ for a; do printf '%s\\n' \"$a\"; done; }}; {command}");
    let output = Command::new("sh")
        .args(["-c", &script])
        .output()
        .expect("sh runs");
    assert!(output.status.success(), "{command}");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

/// Each jump of a `ProxyCommand` as `sh` reads it, nearest the
/// destination first: the arguments of its ssh, and the nested
/// `ProxyCommand` (`%` undoubled) it carries, if any.
fn jump_hops(proxy: &str) -> Vec<Vec<String>> {
    let words = shell_words(proxy);
    let nested = words
        .iter()
        .find_map(|word| word.strip_prefix("ProxyCommand="))
        .map(|inner| inner.replace("%%", "%"));
    let mut hops = vec![words];
    if let Some(inner) = nested {
        hops.extend(jump_hops(&inner));
    }
    hops
}

#[test]
fn ssh_through_hops_hardens_every_jump_host_with_a_nested_proxy_command() {
    use super::{SSH_OPTIONS, proxy_command};
    let names = ["ckl@jump", "ssh://ops@bastion:2222", "edge-gw"];
    for count in 1..=3 {
        let jumps: Vec<String> = names[..count].iter().map(ToString::to_string).collect();
        let command = ssh_command_via("ckl@deep", &jumps, BEAT);
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(!args.iter().any(|arg| arg == "-J"), "{args:?}");
        let end = args.iter().position(|arg| arg == "--").expect("--");
        assert_eq!(&args[end..], ["--", "ckl@deep", BEAT]);
        let proxy = args[..end]
            .iter()
            .find_map(|arg| arg.strip_prefix("ProxyCommand="))
            .expect("a ProxyCommand before the destination");
        assert_eq!(Some(proxy.to_string()), proxy_command(&jumps));
        let hops = jump_hops(proxy);
        assert_eq!(hops.len(), count, "one ssh per jump: {proxy}");
        for (hop, jump) in hops.iter().zip(jumps.iter().rev()) {
            for option in SSH_OPTIONS {
                let at = hop.iter().position(|word| word == option);
                assert!(
                    at.is_some_and(|at| at > 0 && hop[at - 1] == "-o"),
                    "{jump}: {option} in {hop:?}"
                );
            }
            assert_eq!(
                &hop[hop.len() - 5..],
                ["-T", "-W", "[%h]:%p", "--", jump.as_str()]
            );
        }
    }
    // Single hop: no ProxyCommand at all.
    let args: Vec<String> = ssh_command_via("ckl@deep", &[], BEAT)
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    assert!(!args.iter().any(|arg| arg.starts_with("ProxyCommand")));
    // Quoting: a quote in a word stays one word.
    assert_eq!(
        shell_words(&format!("ssh {}", super::shell_quote("a'b c"))),
        ["a'b c"]
    );
    // A hop ssh would read as an option never reaches it.
    let route = super::Route::Ssh {
        destination: "ckl@deep".into(),
        jumps: vec!["-oProxyCommand=sh".into()],
    };
    assert_eq!(
        super::transport::probe_route(&route, super::Op::Beat, Duration::from_secs(1)),
        Err("not an ssh destination".to_string())
    );
}

#[test]
fn the_list_says_how_each_node_is_reached_and_through_which_hops() {
    let nodes = json!([
        {"name": "legacy", "state": "live", "arch": "x86_64", "cores": 4, "heartbeat": null},
        {"name": "gw", "state": "live", "transport": "tcp", "mode": "direct", "via": [],
         "arch": "x86_64", "cores": 4, "heartbeat": null},
        {"name": "edge", "state": "lost", "transport": "udp", "mode": "hole_punch",
         "via": ["gw", "relay-hk"], "arch": "aarch64", "cores": 8, "heartbeat": null},
        {"name": "deep", "state": "live", "transport": "ssh", "via": ["jump"],
         "arch": "x86_64", "cores": 2, "heartbeat": null},
    ]);
    let lines = super::lines(&nodes, 0);
    assert!(lines[0].contains("TRANSPORT"), "{}", lines[0]);
    let line = |name: &str| {
        lines
            .iter()
            .find(|line| line.starts_with(name))
            .cloned()
            .unwrap_or_default()
    };
    assert!(line("legacy").contains(" ssh "), "{}", line("legacy"));
    assert!(line("gw").contains("tcp/direct"), "{}", line("gw"));
    assert!(!line("gw").contains("via"), "{}", line("gw"));
    assert!(line("edge").contains("udp/hole_punch"), "{}", line("edge"));
    assert!(
        line("edge").ends_with("via gw->relay-hk"),
        "{}",
        line("edge")
    );
    assert!(line("deep").contains("ssh/relay"), "{}", line("deep"));
}

fn words(text: &str) -> Vec<String> {
    text.split_whitespace().map(ToString::to_string).collect()
}

#[test]
fn node_add_takes_ssh_its_hops_and_rejects_retired_flags() {
    use super::commands::add_target;
    let target =
        |text: &str| add_target("edge", &crate::Flags::parse(&words(text)).expect("flags"));
    let read = target("--ssh ckl@deep --via jump,bastion --note rack4").expect("ssh through jumps");
    assert_eq!(read.destination, "ckl@deep");
    assert_eq!(read.via, ["jump", "bastion"]);
    let read = target("--ssh ssh://ops@host:2222").expect("direct ssh");
    assert_eq!(read.destination, "ssh://ops@host:2222");
    assert!(read.via.is_empty());

    for (text, why) in [
        ("--note x", "requires --ssh"),
        ("--ssh -oProxyCommand=sh", "--ssh is"),
        ("--ssh host --via edge", "its own via"),
        ("--ssh host --via a,a", "twice"),
        ("--ssh host --via a,b,c,d,e,f,g,h,i", "at most 8"),
        ("--ssh host --port 7", "no --port"),
        ("--ssh host --mode relay", "no --mode"),
        ("--tcp gw:1", "retired"),
        ("--udp 203.0.113.9:7001", "retired"),
        ("--coord wss://c.example/mesh --peer edge", "retired"),
        ("--ssh host --node-id deadbeef", "retired"),
        ("--ssh host --peer edge", "retired"),
    ] {
        let error = target(text).expect_err(text);
        assert!(error.contains(why), "{text}: {error}");
    }
}

#[test]
fn a_join_over_ssh_records_its_destination_and_hops() {
    use super::commands::{add_target, joined_record};
    let facts = inventory(DESKTOP).expect("an inventory");
    let flags = crate::Flags::parse(&words("--ssh ckl@deep --via jump")).expect("flags");
    let target = add_target("deep", &flags).expect("a target");
    let record = joined_record("deep", &target, facts.clone(), Some("rack 4"));
    assert_eq!(
        json!([
            record["type"],
            record["ssh"],
            record["transport"],
            record["endpoint"],
            record["via"],
            record["mode"],
            record["note"],
            record["cores"]
        ]),
        json!([
            "node.joined",
            "ckl@deep",
            "ssh",
            null,
            ["jump"],
            "relay",
            "rack 4",
            16
        ])
    );
    let flags = crate::Flags::parse(&words("--ssh ckl@desktop")).expect("flags");
    let target = add_target("desktop", &flags).expect("a target");
    let record = joined_record("desktop", &target, facts, None);
    assert_eq!(
        json!([
            record["ssh"],
            record["transport"],
            record["endpoint"],
            record["via"],
            record["mode"]
        ]),
        json!(["ckl@desktop", "ssh", null, [], "direct"])
    );
}

#[test]
fn retired_mesh_commands_point_to_go_spawn_node() {
    for verb in ["serve", "register", "coord", "connect"] {
        let error = super::command(&words(verb)).expect_err(verb);
        assert!(
            error.contains("retired") && error.contains("spawn-node"),
            "{verb}: {error}"
        );
    }
}

#[test]
fn the_heartbeat_probes_ssh_nodes_and_handles_historical_non_ssh_records_gracefully() {
    let members = json!([
        {"name": "gw", "ssh": null, "transport": "tcp", "endpoint": "tcp://gw:7000", "via": [],
         "join": 1, "lost": null, "beat_ms": null},
        {"name": "edge", "ssh": null, "transport": "udp", "endpoint": "udp://edge:7001",
         "via": ["gw"], "join": 2, "lost": null, "beat_ms": null},
        {"name": "mesh", "ssh": null, "transport": "coord", "endpoint": "wss://c.example/mesh",
         "via": [], "join": 3, "lost": null, "beat_ms": null},
        {"name": "orphan", "ssh": "ckl@orphan", "transport": "ssh", "endpoint": null,
         "via": ["removed"], "join": 4, "lost": null, "beat_ms": null},
        {"name": "deep", "ssh": "ckl@deep", "transport": "ssh", "endpoint": null,
         "via": ["jump"], "join": 5, "lost": null, "beat_ms": null},
        {"name": "jump", "ssh": "ckl@jump", "join": 6, "lost": null, "beat_ms": null},
    ]);
    let mut watch = HeartbeatScheduler::new(60_000);
    watch.sync(&members, at(0));
    let (_, probes, _) = watch.decide(at(0));
    let route = |name: &str| {
        probes
            .iter()
            .find(|probe| probe.name == name)
            .map(|probe| probe.route.clone())
            .expect(name)
    };
    assert_eq!(
        route("jump"),
        Ok(super::Route::Ssh {
            destination: "ckl@jump".into(),
            jumps: Vec::new()
        })
    );
    assert_eq!(
        route("deep"),
        Ok(super::Route::Ssh {
            destination: "ckl@deep".into(),
            jumps: vec!["ckl@jump".into()]
        })
    );
    let error = route("orphan").expect_err("its hop is gone");
    assert!(error.contains("hop removed is not joined"), "{error}");

    // Historical non-SSH records produce retired-transport Err routes and
    // start() reports them on the outcome channel without panicking.
    let (done, ends) = std::sync::mpsc::channel();
    let origin = Instant::now();
    for (name, expected_transport) in [("gw", "tcp"), ("edge", "udp"), ("mesh", "coord")] {
        let err = route(name).expect_err(name);
        assert!(
            err.contains("retired") && err.contains(expected_transport),
            "{name}: {err}"
        );
        let job = probes
            .iter()
            .find(|probe| probe.name == name)
            .cloned()
            .expect(name);
        super::start(job, &done, origin);
        let outcome = ends
            .recv_timeout(Duration::from_secs(1))
            .expect("immediate retired outcome");
        assert_eq!(outcome.name, name);
        let why = outcome
            .answer
            .as_ref()
            .expect_err("retired route fails cleanly");
        assert!(why.contains("retired"), "{name}: {why}");
        watch.take(outcome);
    }
}

/// [`INVENTORY`]'s output on a MacBook (darwin/arm64, issue #91), as its
/// `Darwin` branch prints it: `sw_vers`, `sysctl`, `df` and `pmset`.
const MAC_INVENTORY: &str = "@arch
arm64
@os
macOS
@kernel
24.1.0
@cores
10
@memsize
17179869184
@df
Filesystem     1024-blocks      Used Available Capacity Mounted on
/dev/disk3s5     482797652 301258792 165338100    65% /System/Volumes/Data
@gpu
@power
Now drawing from 'Battery Power'
 -InternalBattery-0 (id=4653155)\t85%; discharging; 4:32 remaining present: true
@end
";

/// [`BEAT`]'s output on the same MacBook.
const MAC_BEAT: &str = "@loadavg
{ 1.52 1.71 1.80 }
@vm_stat
Mach Virtual Memory Statistics: (page size of 16384 bytes)
Pages free:                               12000.
Pages active:                            300000.
Pages inactive:                          200000.
Pages speculative:                         8000.
Pages throttled:                              0.
Pages wired down:                        150000.
Pages purgeable:                          10000.
@df
Filesystem     1024-blocks      Used Available Capacity Mounted on
/dev/disk3s5     482797652 301258792 165338100    65% /System/Volumes/Data
@power
Now drawing from 'Battery Power'
 -InternalBattery-0 (id=4653155)\t18%; discharging; 0:41 remaining present: true
@end
";

#[test]
fn a_mac_joins_with_its_cores_memory_and_battery() {
    assert_eq!(
        inventory(MAC_INVENTORY).expect("a whole macOS inventory"),
        json!({
            "arch": "arm64", "os": "macOS", "kernel": "24.1.0", "cores": 10,
            "cpu_max": null, "mem_total": 17_179_869_184_u64, "mem_max": null,
            "disk_free": 165_338_100 * KIB, "gpu": null,
            "power": {"on_ac": false, "battery_pct": 85, "charging": false,
                      "remaining_mins": 272},
        })
    );
    // A Mac with no battery (a mini): on AC, no charge.
    let mini = MAC_INVENTORY.replace(
        "Now drawing from 'Battery Power'\n -InternalBattery-0 (id=4653155)\t85%; discharging; 4:32 remaining present: true\n",
        "Now drawing from 'AC Power'\n",
    );
    assert_eq!(
        inventory(&mini).expect("a mini")["power"],
        json!({"on_ac": true, "battery_pct": null, "charging": false, "remaining_mins": null})
    );
    // No pmset at all: no power key, as every join before it.
    let bare = MAC_INVENTORY.replace("@power\nNow drawing from 'Battery Power'\n -InternalBattery-0 (id=4653155)\t85%; discharging; 4:32 remaining present: true\n", "@power\n");
    assert!(inventory(&bare).expect("no power").get("power").is_none());
    // Without memsize a macOS output is refused by name.
    let error = inventory(&MAC_INVENTORY.replace("@memsize\n17179869184\n", "")).unwrap_err();
    assert!(error.contains("MemTotal"), "{error}");
    let error = inventory(&MAC_INVENTORY.replace("17179869184", "x")).unwrap_err();
    assert!(error.contains("memsize"), "{error}");
}

#[test]
fn a_mac_heartbeat_reads_vm_loadavg_vm_stat_and_pmset() {
    let sample = beat(MAC_BEAT).expect("a whole macOS beat");
    assert!((sample.load1 - 1.52).abs() < 1e-9, "{sample:?}");
    assert_eq!(sample.mem_available, (12_000 + 200_000 + 8_000) * 16_384);
    assert_eq!(sample.disk_free, 165_338_100 * KIB);
    assert_eq!(
        sample.power,
        Some(PowerTelemetry {
            on_ac: false,
            battery_pct: Some(18),
            charging: false,
            remaining_mins: Some(41),
        })
    );
    assert_eq!(
        sample.facts()["power"],
        json!({"on_ac": false, "battery_pct": 18, "charging": false, "remaining_mins": 41})
    );
    let error = beat(&MAC_BEAT.replace("page size of", "pages of")).unwrap_err();
    assert!(error.contains("vm_stat"), "{error}");
    assert!(beat(&MAC_BEAT.replace("{ 1.52", "{ x")).is_err());
}

#[test]
fn a_linux_laptop_reads_its_power_supplies_and_a_desktop_has_none() {
    let laptop = RUNNER_BEAT.replace(
        "@end\n",
        "@power\nsupply AC\ntype=Mains\nonline=0\nsupply BAT0\ntype=Battery\n\
         status=Discharging\ncapacity=57\nenergy_now=24140000\npower_now=8610000\n\
         supply hidpp_battery_0\ntype=Battery\nscope=Device\ncapacity=5\nstatus=Discharging\n@end\n",
    );
    assert_eq!(
        beat(&laptop).expect("a laptop's beat").power,
        Some(PowerTelemetry {
            on_ac: false,
            battery_pct: Some(57),
            charging: false,
            remaining_mins: Some(168),
        })
    );
    let charging = laptop.replace("online=0", "online=1").replace(
        "status=Discharging\ncapacity=57",
        "status=Charging\ncapacity=57\ntime_to_full_now=1800",
    );
    assert_eq!(
        beat(&charging).expect("charging").power,
        Some(PowerTelemetry {
            on_ac: true,
            battery_pct: Some(57),
            charging: true,
            remaining_mins: Some(30),
        })
    );
    // An empty @power (a desktop, a VM), or none (an older node): no power,
    // and every other fact as before.
    let desktop = RUNNER_BEAT.replace("@end\n", "@power\n@end\n");
    assert_eq!(
        beat(&desktop).expect("a desktop"),
        beat(RUNNER_BEAT).expect("old")
    );
    assert_eq!(beat(RUNNER_BEAT).expect("old").power, None);
    assert!(inventory(DESKTOP).expect("old").get("power").is_none());
    // Only a peripheral's battery: no power either.
    let mouse = RUNNER_BEAT.replace(
        "@end\n",
        "@power\nsupply hid\ntype=Battery\nscope=Device\ncapacity=40\n@end\n",
    );
    assert_eq!(beat(&mouse).expect("mouse").power, None);
}

#[test]
fn the_probes_dispatch_on_uname_and_run_under_posix_sh() {
    for probe in [INVENTORY, BEAT] {
        assert!(
            probe.contains("uname -s") && probe.contains("Darwin"),
            "{probe}"
        );
        assert!(probe.contains("pmset -g batt") && probe.contains("/sys/class/power_supply"));
    }
    assert!(INVENTORY.contains("sysctl -n hw.ncpu") && INVENTORY.contains("sysctl -n hw.memsize"));
    assert!(INVENTORY.contains("nproc") && INVENTORY.contains("/proc/meminfo"));
    assert!(BEAT.contains("vm_stat") && BEAT.contains("sysctl -n vm.loadavg"));
    // This host (Linux) runs both under `sh` and reads its own answers.
    let run = |script: &str| {
        let output = Command::new("sh")
            .args(["-c", script])
            .output()
            .expect("sh");
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).expect("utf-8")
    };
    let facts = inventory(&run(INVENTORY)).expect("this host's inventory");
    assert!(
        facts["cores"].as_u64().is_some_and(|cores| cores > 0),
        "{facts}"
    );
    let sample = beat(&run(BEAT)).expect("this host's beat");
    assert!(sample.mem_available > 0);
}

#[test]
fn a_power_that_moves_records_a_heartbeat_before_ten_minutes() {
    let mut watch = HeartbeatScheduler::new(60_000);
    watch.sync(&member(7, &Value::Null, None), at(0));
    let plugged = |pct: u8, on_ac: bool| ProbeOutcome {
        answer: Ok(HeartbeatSample {
            load1: 0.5,
            mem_available: 1,
            disk_free: 1,
            power: Some(PowerTelemetry {
                on_ac,
                battery_pct: Some(pct),
                charging: false,
                remaining_mins: None,
            }),
        }),
        ..answer(7, 0)
    };
    let types = |records: &[Value]| -> Vec<String> {
        records
            .iter()
            .map(|record| record["type"].as_str().unwrap_or_default().to_string())
            .collect()
    };
    decide(&mut watch, at(0), true);
    watch.take(ProbeOutcome {
        at: at(1_000),
        ..plugged(80, true)
    });
    let first = decide(&mut watch, at(1_000), true);
    assert_eq!(types(&first), ["node.heartbeat"]);
    assert_eq!(first[0]["power"]["battery_pct"], 80);
    // The same power a probe later: nothing until ten minutes.
    watch.take(ProbeOutcome {
        at: at(16_000),
        ..plugged(78, true)
    });
    assert!(decide(&mut watch, at(16_000), true).is_empty());
    // Unplugged: recorded at once.
    watch.take(ProbeOutcome {
        at: at(31_000),
        ..plugged(78, false)
    });
    let unplugged = decide(&mut watch, at(31_000), true);
    assert_eq!(types(&unplugged), ["node.heartbeat"]);
    assert_eq!(unplugged[0]["power"]["on_ac"], false);
    // Five points down: recorded; two more: not.
    watch.take(ProbeOutcome {
        at: at(46_000),
        ..plugged(73, false)
    });
    assert_eq!(decide(&mut watch, at(46_000), true).len(), 1);
    watch.take(ProbeOutcome {
        at: at(61_000),
        ..plugged(71, false)
    });
    assert!(decide(&mut watch, at(61_000), true).is_empty());
}
