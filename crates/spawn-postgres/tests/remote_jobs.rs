//! spawn-node v1's ledger (PR S1; design "spawn-node v1 and host-adaptive
//! storage" §§4–5, §12.2): the reference behaviour of §5.3 through the typed
//! API, and the tests §12.2 owes beyond it: two nodes' beats over one queue,
//! a sweep racing a beat, the poison pill, three strikes and an undrain, a
//! result racing `take_local`, the gateway login's whole surface, a second
//! migration, and an old schema's refold over a log holding the new records.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Barrier;

use common::{admin, store};
use postgres::Client;
use serde_json::{Value, json};
use spawn_core::port::StoreError;
use spawn_postgres::{
    FaultNext, NodeAction, NodeEnroll, PostgresStore, RemoteJobKind, RemoteJobSpec, RemoteJobState,
};

/// The schema's own text, and its S1 block's markers.
const SCHEMA: &str = include_str!("../sql/schema.sql");
const BLOCK_BEGIN: &str = "-- spawn-node v1 ledger: begin (PR S1).";
const BLOCK_END: &str = "-- spawn-node v1 ledger: end (PR S1).";

const SPEC_SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// A boot id: the counter in its first 16 hex digits.
fn boot(counter: u64) -> String {
    format!("{counter:016x}8796a5b4c3d2e1f0")
}

/// Enroll node `name` with a key made of `seed` and the spec `holds`;
/// returns its node id. Its boot is `boot(1)`, its spec [`SPEC_SHA`].
fn enroll(store: &PostgresStore, name: &str, seed: u8, holds: &Value) -> String {
    let token = [seed; 32];
    let pubkey = [seed.wrapping_add(100); 32];
    store
        .node_enroll_token(name, None, &token, 600_000)
        .expect("a token");
    let spec = json!({"v": 1, "name": name, "holds": holds});
    let enrolled = store
        .node_gw_enroll(
            &NodeEnroll {
                token_sha256: &token,
                pubkey: &pubkey,
                boot_id: &boot(1),
                spec_sha256: SPEC_SHA,
                spec: &spec,
            },
            |answer| answer["ok"] == true,
        )
        .expect("enroll");
    assert!(enrolled.committed, "enrolled: {}", enrolled.answer);
    enrolled.answer["node_id"]
        .as_str()
        .expect("a node id")
        .to_string()
}

/// A gate job of `cost`, needing `requires`.
fn job(id: &str, requires: &[(&str, &str)], cost: &[(&str, i64)]) -> RemoteJobSpec {
    RemoteJobSpec {
        job_id: id.to_string(),
        kind: RemoteJobKind::Gate,
        cell_id: None,
        priority: None,
        judging_id: None,
        mission_id: None,
        task_id: None,
        spec: json!({"inputs": {}}),
        requires: requires
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect(),
        cost: cost
            .iter()
            .map(|(key, value)| ((*key).to_string(), *value))
            .collect(),
        base_commit: Some("c".repeat(40)),
        max_attempts: None,
        queue_wait_ms: None,
    }
}

/// A beat body: the node holds `leases` and has results waiting for
/// `pending`, with `slots` gate sandboxes and `tmpfs` MB free.
fn beat(leases: &[(String, i64)], pending: &[(String, i64)], slots: i64, tmpfs: i64) -> Value {
    let list = |pairs: &[(String, i64)]| -> Value {
        pairs
            .iter()
            .map(|(job_id, epoch)| json!({"job_id": job_id, "lease_epoch": epoch}))
            .collect()
    };
    json!({"v": 1, "cell_id": "c0", "state": "idle", "spec_sha256": SPEC_SHA,
           "capacities": {"gate_sandbox": {"free": slots, "total": slots}},
           "tmpfs_budget_mb": {"free": tmpfs, "total": tmpfs},
           "leases": list(leases), "lease_results_pending": list(pending),
           "warm_bases": [], "edge": "timer"})
}

/// One beat, committed; its answer.
fn beat_once(store: &PostgresStore, node: &str, boot_id: &str, seq: i64, body: &Value) -> Value {
    store
        .node_gw_beat(node, boot_id, seq, body, |_| true)
        .expect("a beat")
        .answer
}

/// The `(job_id, lease_epoch)` of an answer's grants.
fn grants(answer: &Value) -> Vec<(String, i64)> {
    answer["grants"]
        .as_array()
        .expect("grants")
        .iter()
        .map(|grant| {
            (
                grant["job_id"].as_str().expect("job id").to_string(),
                grant["lease_epoch"].as_i64().expect("epoch"),
            )
        })
        .collect()
}

/// A result's metadata, as `spawn_node_gw_result` reads it.
fn result(exit_code: i64, digest: char) -> Value {
    json!({"v": 1, "exit_code": exit_code, "seconds": 3,
           "result_sha256": digest.to_string().repeat(64),
           "log_sha256": "e".repeat(64), "log_bytes": "18"})
}

fn complete(store: &PostgresStore, node: &str, job_id: &str, epoch: i64) {
    let done = store
        .node_gw_result(node, job_id, epoch, &result(6, 'd'), |_| true)
        .expect("a result");
    assert!(done.answer, "{job_id} completes at epoch {epoch}");
}

/// Whether `error` is PostgreSQL's deadlock (40P01).
fn deadlock(error: &StoreError) -> bool {
    format!("{error:?}").contains("(40P01)")
}

/// `call`, and once more after a deadlock (whose transaction rolled back);
/// a second deadlock, or any other error, fails the test. Returns how many
/// deadlocks it met.
fn at_most_one_retry<T>(what: &str, mut call: impl FnMut() -> Result<T, StoreError>) -> usize {
    match call() {
        Ok(_) => 0,
        Err(error) if deadlock(&error) => {
            call().unwrap_or_else(|again| panic!("{what} deadlocked twice: {again:?}"));
            1
        }
        Err(error) => panic!("{what}: {error:?}"),
    }
}

fn state(store: &PostgresStore, job_id: &str) -> spawn_postgres::RemoteJob {
    store
        .remote_job(job_id)
        .expect("read a job")
        .expect("the job")
}

fn node_records(client: &mut Client) -> BTreeMap<String, i64> {
    client
        .query(
            "select kind #>> '{detail,record,type}', count(*) from spawn_events
              where event_name = 'node.recorded' and kind #>> '{detail,record,proto}' = 'node-v1'
              group by 1",
            &[],
        )
        .expect("the v1 records")
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect()
}

/// §5.3's run, step by step: enrollment and a reused token, need_sync and
/// no events from idle beats, capacity, a lost grant given back, renewals
/// and `not_held`, results fenced by epoch and idempotent, a new boot, a
/// stale boot, `take_local`, a strike, the sweep (level-triggered), the
/// owner's drain and revoke.
#[test]
#[allow(clippy::too_many_lines)]
fn the_ledger_behaves_as_the_design_measured_it() {
    let (store, schema) = store("rj_reference");
    let mut client = admin(&schema);
    let node = enroll(&store, "n252", 1, &json!({"pg": "16"}));
    let reused = store
        .node_gw_enroll(
            &NodeEnroll {
                token_sha256: &[1; 32],
                pubkey: &[101; 32],
                boot_id: &boot(1),
                spec_sha256: SPEC_SHA,
                spec: &json!({}),
            },
            |_| true,
        )
        .expect("a second enroll");
    assert_eq!(reused.answer["error"], "bad_token", "a token is good once");
    let jobs: Vec<String> = (1..=3)
        .map(|n| format!("gate-00000000000{n}-aaaaaaaa"))
        .collect();
    for id in &jobs {
        store
            .remote_job_enqueue(&job(
                id,
                &[("registry", "R1"), ("pg", "16")],
                &[("gate_sandbox", 1), ("tmpfs_mb", 20480)],
            ))
            .expect("enqueue");
    }
    let before = node_records(&mut client);
    let b1 = boot(1);
    let idle = beat_once(&store, &node, &b1, 1, &beat(&[], &[], 4, 262_144));
    assert_eq!(idle["need_sync"], json!({"pg": "16", "registry": "R1"}));
    assert_eq!(grants(&idle), []);
    assert_eq!(idle["next_beat_ms"], 5000, "a queued job: beat often");
    let replay = beat_once(&store, &node, &b1, 1, &json!({}));
    assert_eq!(replay["error"], "replay");
    assert_eq!(
        node_records(&mut client),
        before,
        "idle beats record nothing"
    );

    let spec = json!({"holds": {"pg": "16", "registry": "R1"}});
    let registered = store
        .node_gw_spec(&node, &b1, SPEC_SHA, &spec, |_| true)
        .expect("spec")
        .answer;
    assert_eq!(
        registered["changed"], false,
        "the same digest changes nothing"
    );
    let other = "a".repeat(64);
    let registered = store
        .node_gw_spec(&node, &b1, &other, &spec, |_| true)
        .expect("spec")
        .answer;
    assert_eq!(registered["changed"], true);
    let mut body = beat(&[], &[], 4, 30_000);
    body["spec_sha256"] = json!(other);
    let one = beat_once(&store, &node, &b1, 2, &body);
    assert_eq!(
        grants(&one),
        [(jobs[0].clone(), 1)],
        "only one fits the tmpfs"
    );

    // The next beat does not hold it: the grant was lost, given back, and
    // granted again with another (two at most).
    let mut body = beat(&[], &[], 4, 262_144);
    body["spec_sha256"] = json!(other);
    let lost = beat_once(&store, &node, &b1, 3, &body);
    assert_eq!(grants(&lost), [(jobs[0].clone(), 3), (jobs[1].clone(), 1)]);
    assert_eq!(lost["grants"][0]["needs_baseline"], true);
    assert_eq!(
        state(&store, &jobs[0]).attempts,
        1,
        "the lost grant is refunded"
    );

    let held = [
        (jobs[0].clone(), 3),
        (jobs[1].clone(), 1),
        ("gate-ghost0000000-aaaaaaaa".to_string(), 9),
    ];
    let mut body = beat(&held, &[], 2, 200_000);
    body["spec_sha256"] = json!(other);
    let renewed = beat_once(&store, &node, &b1, 4, &body);
    assert_eq!(renewed["renewed"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        renewed["fenced"],
        json!([{"job_id": "gate-ghost0000000-aaaaaaaa", "lease_epoch": 9, "reason": "not_held"}])
    );
    assert_eq!(grants(&renewed), [(jobs[2].clone(), 1)]);

    let at = |epoch: i64, metadata: &Value| {
        store
            .node_gw_result(&node, &jobs[0], epoch, metadata, |_| true)
            .expect("a result")
            .answer
    };
    assert!(!at(2, &result(0, 'd')), "a stale epoch is fenced");
    assert!(at(3, &result(0, 'd')), "the current epoch completes");
    assert!(
        at(3, &result(0, 'd')),
        "the same result again is idempotent"
    );
    assert!(!at(3, &result(6, 'f')), "another result is refused");

    // A new boot: the finished job is kept, the running one fenced at once.
    let b2 = boot(2);
    let mut body = beat(
        &[],
        &[(jobs[1].clone(), 1), (jobs[0].clone(), 3)],
        4,
        262_144,
    );
    body["spec_sha256"] = json!(other);
    body["state"] = json!("draining");
    let rebooted = beat_once(&store, &node, &b2, 1, &body);
    assert_eq!(
        rebooted["acked_results"],
        json!([{"job_id": jobs[0], "lease_epoch": 3}])
    );
    assert_eq!(grants(&rebooted), [], "a draining node takes nothing");
    assert_eq!(state(&store, &jobs[1]).state, RemoteJobState::ResultPending);
    let fenced = state(&store, &jobs[2]);
    assert_eq!(
        (fenced.state, fenced.lease_epoch),
        (RemoteJobState::Queued, 2)
    );
    let stale = beat_once(&store, &node, &b1, 9, &json!({}));
    assert_eq!(stale["error"], "stale_boot");

    assert!(!store.remote_job_take_local(&jobs[2], 0).expect("take"));
    assert!(store.remote_job_take_local(&jobs[2], 2).expect("take"));
    assert_eq!(state(&store, &jobs[2]).state, RemoteJobState::FallbackLocal);

    let fault = store
        .remote_job_fault(&jobs[0], 3, "sandbox_never_started", true, FaultNext::Keep)
        .expect("a fault");
    assert_eq!(
        (fault.ok, fault.strikes, fault.degraded),
        (true, Some(1), Some(false))
    );

    client
        .batch_execute(&format!(
            "update spawn_remote_jobs set grace_until_ms = 1 where job_id = '{}';
             update spawn_node_state set last_beat_ms = 1;",
            jobs[1]
        ))
        .expect("age the pending result and the node");
    let swept = store.node_sweep().expect("a sweep");
    assert_eq!((swept.waiting, swept.fenced, swept.lost), (0, 1, 1));
    let again = store.node_sweep().expect("a sweep");
    assert_eq!(
        (again.waiting, again.fenced, again.lost),
        (0, 0, 0),
        "level-triggered"
    );
    let records = node_records(&mut client);
    for (record, count) in [
        ("node.enrolled", 1),
        ("node.lost", 1),
        ("node.spec_registered", 1),
        ("remote_job.completed", 1),
        ("remote_job.host_fault", 1),
        ("remote_job.lease_fenced", 3),
        ("remote_job.leased", 4),
    ] {
        assert_eq!(records.get(record), Some(&count), "{record}: {records:?}");
    }

    assert!(store.node_admin("n252", NodeAction::Drain).expect("drain"));
    assert!(!store.node_admin("n252", NodeAction::Drain).expect("drain"));
    assert!(
        store
            .node_admin("n252", NodeAction::Revoke)
            .expect("revoke")
    );
    let revoked = beat_once(&store, &node, &b2, 2, &json!({}));
    assert_eq!(revoked["error"], "revoked");
    assert!(
        store
            .node_enroll_token("n252", None, &[9; 32], 600_000)
            .is_err(),
        "a revoked name is never enrolled again"
    );
    // No v1 record is a P1 node's.
    assert_eq!(store.nodes().expect("the P1 nodes"), json!([]));
}

/// Verify-then-commit: an answer the gateway does not believe (a bad
/// signature) leaves nothing behind, not even the beat's sequence number.
#[test]
fn a_call_the_gateway_does_not_commit_changes_nothing() {
    let (store, schema) = store("rj_rollback");
    let node = enroll(&store, "n1", 2, &json!({}));
    store
        .remote_job_enqueue(&job("gate-rollback-0001", &[], &[("gate_sandbox", 1)]))
        .expect("enqueue");
    let refused = store
        .node_gw_beat(&node, &boot(1), 1, &beat(&[], &[], 1, 0), |answer| {
            assert_eq!(grants(answer).len(), 1, "the call itself granted");
            false
        })
        .expect("a beat");
    assert!(!refused.committed);
    assert_eq!(
        state(&store, "gate-rollback-0001").state,
        RemoteJobState::Queued
    );
    let seq: i64 = admin(&schema)
        .query_one(
            "select beat_seq from spawn_node_state where name = 'n1'",
            &[],
        )
        .expect("the node")
        .get(0);
    assert_eq!(seq, 0, "the replay window did not move");
    let granted = beat_once(&store, &node, &boot(1), 1, &beat(&[], &[], 1, 0));
    assert_eq!(grants(&granted).len(), 1, "the same beat, believed, grants");
}

/// Two nodes beat at once, again and again, over one queue: SKIP LOCKED
/// hands each job to exactly one of them.
#[test]
fn two_concurrent_beats_of_two_nodes_grant_each_job_once() {
    const JOBS: usize = 40;
    let (store, _schema) = store("rj_two_nodes");
    let nodes = [
        enroll(&store, "na", 3, &json!({})),
        enroll(&store, "nb", 4, &json!({})),
    ];
    for n in 0..JOBS {
        store
            .remote_job_enqueue(&job(
                &format!("gate-pair-{n:04}"),
                &[],
                &[("gate_sandbox", 1)],
            ))
            .expect("enqueue");
    }
    let start = Barrier::new(2);
    let granted: Vec<Vec<(String, i64)>> = std::thread::scope(|scope| {
        let workers: Vec<_> = nodes
            .iter()
            .map(|node| {
                let connection = store.new_connection().expect("a connection");
                let start = &start;
                scope.spawn(move || {
                    let mut held: Vec<(String, i64)> = Vec::new();
                    start.wait();
                    for seq in 1..=200 {
                        let answer =
                            beat_once(&connection, node, &boot(1), seq, &beat(&held, &[], 1000, 0));
                        assert_eq!(answer["ok"], true, "{answer}");
                        assert_eq!(answer["fenced"], json!([]), "nothing is lost");
                        let new = grants(&answer);
                        let done = new.is_empty()
                            && connection
                                .remote_job(&format!("gate-pair-{:04}", JOBS - 1))
                                .expect("a job")
                                .is_some_and(|job| job.state != RemoteJobState::Queued)
                            && (0..JOBS).all(|n| {
                                connection
                                    .remote_job(&format!("gate-pair-{n:04}"))
                                    .expect("a job")
                                    .is_some_and(|job| job.state != RemoteJobState::Queued)
                            });
                        held.extend(new);
                        if done {
                            break;
                        }
                    }
                    held
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().expect("a beating node"))
            .collect()
    });
    let all: Vec<&(String, i64)> = granted.iter().flatten().collect();
    let distinct: BTreeSet<&str> = all.iter().map(|(job_id, _)| job_id.as_str()).collect();
    assert_eq!(all.len(), JOBS, "every grant is of a job no one else got");
    assert_eq!(distinct.len(), JOBS, "every job granted");
    for (node, held) in nodes.iter().zip(&granted) {
        for (job_id, epoch) in held {
            let job = state(&store, job_id);
            assert_eq!(job.node_id.as_deref(), Some(node.as_str()), "{job_id}");
            assert_eq!((job.lease_epoch, job.attempts), (*epoch, 1), "{job_id}");
        }
    }
}

/// The owner's sweep and a node's beat, at once, over the same node and
/// leases, many times: both lock the node first, so neither ever needs more
/// than one retry (and, in this order, none).
#[test]
fn an_owner_sweep_racing_a_beat_never_deadlocks_past_one_retry() {
    let (store, schema) = store("rj_sweep_race");
    let mut client = admin(&schema);
    let node = enroll(&store, "ns", 5, &json!({}));
    for n in 0..4 {
        store
            .remote_job_enqueue(&job(
                &format!("gate-race-{n:04}"),
                &[],
                &[("gate_sandbox", 1)],
            ))
            .expect("enqueue");
    }
    let mut seq = 1;
    let mut held = Vec::new();
    while held.len() < 4 {
        held.extend(grants(&beat_once(
            &store,
            &node,
            &boot(1),
            seq,
            &beat(&held, &[], 4, 0),
        )));
        seq += 1;
    }
    let sweeper = store.new_connection().expect("a connection");
    let beater = store.new_connection().expect("a connection");
    let mut deadlocks = 0;
    for _ in 0..40 {
        // Every lease past its time and the node silent: the sweep has work
        // on exactly the rows the beat renews.
        client
            .batch_execute(
                "update spawn_remote_jobs set state = 'leased', lease_expires_ms = 1,
                        grace_until_ms = null where node_id is not null;
                 update spawn_node_state set last_beat_ms = 1;",
            )
            .expect("expire");
        let start = Barrier::new(2);
        let this_seq = seq;
        seq += 1;
        deadlocks += std::thread::scope(|scope| {
            let sweep = scope.spawn(|| {
                start.wait();
                at_most_one_retry("the sweep", || sweeper.node_sweep())
            });
            let beating = scope.spawn(|| {
                start.wait();
                at_most_one_retry("the beat", || {
                    beater
                        .node_gw_beat(&node, &boot(1), this_seq, &beat(&held, &[], 4, 0), |_| true)
                })
            });
            sweep.join().expect("the sweep") + beating.join().expect("the beat")
        });
    }
    assert_eq!(deadlocks, 0, "one lock order: no deadlock at all");
    for (job_id, epoch) in &held {
        let job = state(&store, job_id);
        assert_eq!(
            job.lease_epoch, *epoch,
            "{job_id}: never fenced (grace not over)"
        );
    }
}

/// A job that faults on a second node is the poison: the first node's
/// strike is given back, the second gets none, and the gate goes local.
#[test]
fn a_job_faulting_on_two_nodes_is_the_poison_and_refunds_the_strike() {
    let (store, _schema) = store("rj_poison");
    let first = enroll(&store, "np1", 6, &json!({}));
    let second = enroll(&store, "np2", 7, &json!({}));
    let id = "gate-poison-0001";
    store
        .remote_job_enqueue(&job(id, &[], &[("gate_sandbox", 1)]))
        .expect("enqueue");
    let granted = grants(&beat_once(
        &store,
        &first,
        &boot(1),
        1,
        &beat(&[], &[], 1, 0),
    ));
    assert_eq!(granted, [(id.to_string(), 1)]);
    complete(&store, &first, id, 1);
    let fault = store
        .remote_job_fault(id, 1, "host_fault", true, FaultNext::Queued)
        .expect("a fault");
    assert_eq!(
        (fault.next, fault.poison, fault.strikes),
        (Some(FaultNext::Queued), Some(false), Some(1))
    );
    let excluded = grants(&beat_once(
        &store,
        &first,
        &boot(1),
        2,
        &beat(&[], &[], 1, 0),
    ));
    assert_eq!(excluded, [], "the node it faulted on is excluded");
    let granted = grants(&beat_once(
        &store,
        &second,
        &boot(1),
        1,
        &beat(&[], &[], 1, 0),
    ));
    assert_eq!(granted, [(id.to_string(), 3)]);
    complete(&store, &second, id, 3);
    let poison = store
        .remote_job_fault(id, 3, "host_fault", true, FaultNext::Queued)
        .expect("a fault");
    assert_eq!(
        (poison.ok, poison.poison, poison.next, poison.strikes),
        (true, Some(true), Some(FaultNext::FallbackLocal), None)
    );
    let strikes = |name: &str| {
        store
            .node_v1(name)
            .expect("a node")
            .expect("the node")
            .host_fault_strikes
    };
    assert_eq!(
        (strikes("np1"), strikes("np2")),
        (0, 0),
        "neither node is blamed"
    );
    let job = state(&store, id);
    assert_eq!(job.state, RemoteJobState::FallbackLocal);
    assert_eq!(job.fault_nodes, [first.clone(), second.clone()]);
}

/// Three strikes make a node degraded: no grants until the owner's
/// undrain, which clears them. A completion is judged once.
#[test]
fn three_strikes_degrade_a_node_and_an_undrain_clears_them() {
    let (store, _schema) = store("rj_strikes");
    let node = enroll(&store, "nd", 8, &json!({}));
    for n in 0..4 {
        store
            .remote_job_enqueue(&job(
                &format!("gate-strike-{n:04}"),
                &[],
                &[("gate_sandbox", 1)],
            ))
            .expect("enqueue");
    }
    let mut seq = 1;
    for n in 0..3 {
        let id = format!("gate-strike-{n:04}");
        let granted = grants(&beat_once(
            &store,
            &node,
            &boot(1),
            seq,
            &beat(&[], &[], 1, 0),
        ));
        seq += 1;
        assert_eq!(granted, [(id.clone(), 1)]);
        complete(&store, &node, &id, 1);
        let fault = store
            .remote_job_fault(&id, 1, "wrapper_error", true, FaultNext::Keep)
            .expect("a fault");
        assert_eq!(fault.strikes, Some(n + 1));
        assert_eq!(fault.degraded, Some(n == 2));
        let again = store
            .remote_job_fault(&id, 1, "wrapper_error", true, FaultNext::Keep)
            .expect("a second judging");
        assert!(!again.ok, "a completion is judged once");
    }
    let row = store.node_v1("nd").expect("a node").expect("the node");
    assert!(row.degraded);
    assert_eq!(row.host_fault_strikes, 3);
    let refused = beat_once(&store, &node, &boot(1), seq, &beat(&[], &[], 1, 0));
    seq += 1;
    assert_eq!(grants(&refused), [], "a degraded node gets no grants");
    assert!(
        store
            .node_admin("nd", NodeAction::Undrain)
            .expect("undrain")
    );
    let row = store.node_v1("nd").expect("a node").expect("the node");
    assert_eq!((row.degraded, row.host_fault_strikes), (false, 0));
    assert!(
        !store
            .node_admin("nd", NodeAction::Undrain)
            .expect("undrain"),
        "nothing left to undrain"
    );
    let granted = grants(&beat_once(
        &store,
        &node,
        &boot(1),
        seq,
        &beat(&[], &[], 1, 0),
    ));
    assert_eq!(granted, [("gate-strike-0003".to_string(), 1)]);
}

/// A valid run (`spawn_remote_job_valid`) puts the node's strikes back to
/// 0, once per completion, and never clears `degraded`; a job not
/// completed at that epoch, or whose node it excludes, is refused, and only
/// the owner may call it.
#[test]
#[allow(clippy::too_many_lines)]
fn a_valid_run_resets_the_strikes_once_and_leaves_degraded() {
    let (store, schema) = store("rj_valid");
    let node = enroll(&store, "nv", 10, &json!({}));
    for n in 0..5 {
        store
            .remote_job_enqueue(&job(
                &format!("gate-valid-{n:04}"),
                &[],
                &[("gate_sandbox", 1)],
            ))
            .expect("enqueue");
    }
    for (seq, n) in (1..).zip(0..4) {
        let id = format!("gate-valid-{n:04}");
        let granted = grants(&beat_once(
            &store,
            &node,
            &boot(1),
            seq,
            &beat(&[], &[], 1, 0),
        ));
        assert_eq!(granted, [(id.clone(), 1)]);
        complete(&store, &node, &id, 1);
    }
    for n in 0..2 {
        let fault = store
            .remote_job_fault(
                &format!("gate-valid-{n:04}"),
                1,
                "wrapper_error",
                true,
                FaultNext::Keep,
            )
            .expect("a fault");
        assert_eq!(fault.strikes, Some(n + 1));
    }
    let node_row = |store: &PostgresStore| store.node_v1("nv").expect("a node").expect("the node");
    assert_eq!(node_row(&store).host_fault_strikes, 2);

    // Refused: a faulted job (its node excluded), a wrong epoch, a job not
    // completed. Nothing changes.
    assert!(
        !store
            .remote_job_valid("gate-valid-0000", 1)
            .expect("excluded")
    );
    assert!(
        !store
            .remote_job_valid("gate-valid-0002", 2)
            .expect("wrong epoch")
    );
    assert!(
        !store
            .remote_job_valid("gate-valid-0004", 0)
            .expect("queued")
    );
    assert!(
        !store
            .remote_job_valid("gate-none-000000", 1)
            .expect("unknown")
    );
    assert_eq!(node_row(&store).host_fault_strikes, 2);

    // A valid run: 2 -> 0, the row still completed at the next epoch.
    assert!(store.remote_job_valid("gate-valid-0002", 1).expect("valid"));
    let row = node_row(&store);
    assert_eq!((row.host_fault_strikes, row.degraded), (0, false));
    let judged = state(&store, "gate-valid-0002");
    assert_eq!(judged.state, RemoteJobState::Completed);
    assert_eq!(judged.lease_epoch, 2);
    assert!(judged.finished_at_ms.is_some());
    // Once only: neither a second valid nor a stale fault at the old epoch.
    assert!(
        !store.remote_job_valid("gate-valid-0002", 1).expect("again"),
        "a completion is judged once"
    );
    let stale = store
        .remote_job_fault("gate-valid-0002", 1, "wrapper_error", true, FaultNext::Keep)
        .expect("a stale fault");
    assert!(!stale.ok, "a fault after a valid judging is refused");
    assert_eq!(node_row(&store).host_fault_strikes, 0);

    // A degraded node's valid run clears its strikes, never `degraded`.
    let mut client = admin(&schema);
    client
        .execute(
            "update spawn_node_state set host_fault_strikes = 2, degraded = true where name = 'nv'",
            &[],
        )
        .expect("degrade");
    assert!(store.remote_job_valid("gate-valid-0003", 1).expect("valid"));
    let row = node_row(&store);
    assert_eq!((row.host_fault_strikes, row.degraded), (0, true));
    let answer: Value = client
        .query_one("select spawn_remote_job_valid('gate-valid-0003', 1)", &[])
        .expect("again")
        .get(0);
    assert_eq!(answer, json!({"ok": false}));

    // Only the owner: never the gateway, a reader, a supervisor or PUBLIC.
    for login in ["spawn_node_gateway", "spawn_reader", "spawn_supervisor"] {
        let error = common::as_login(&schema, login)
            .query_one("select spawn_remote_job_valid('gate-valid-0001', 1)", &[])
            .expect_err(login);
        assert_eq!(
            error.code(),
            Some(&postgres::error::SqlState::INSUFFICIENT_PRIVILEGE),
            "{login}: {error}"
        );
    }
    let public: bool = client
        .query_one(
            "select has_function_privilege('public', 'spawn_remote_job_valid(text, bigint)',
                                           'execute')",
            &[],
        )
        .expect("public's privilege")
        .get(0);
    assert!(!public, "closed to PUBLIC");
}

/// The integrator's `take_local` at the epoch it read, racing a node's
/// grant and result upload: exactly one of them wins, every time.
#[test]
fn a_result_upload_racing_take_local_has_exactly_one_winner() {
    let (store, _schema) = store("rj_take_local");
    let node = enroll(&store, "nt", 9, &json!({}));
    let uploader = store.new_connection().expect("a connection");
    let integrator = store.new_connection().expect("a connection");
    let (mut remote, mut local) = (0, 0);
    for round in 0..40_i64 {
        let id = format!("gate-take-{round:04}");
        store
            .remote_job_enqueue(&job(&id, &[], &[("gate_sandbox", 1)]))
            .expect("enqueue");
        let read_epoch = state(&store, &id).lease_epoch;
        let start = Barrier::new(2);
        let (node_won, local_won) = std::thread::scope(|scope| {
            let upload = scope.spawn(|| {
                start.wait();
                let answer =
                    beat_once(&uploader, &node, &boot(1), round + 1, &beat(&[], &[], 1, 0));
                grants(&answer).into_iter().any(|(job_id, epoch)| {
                    job_id == id
                        && uploader
                            .node_gw_result(&node, &job_id, epoch, &result(0, 'd'), |_| true)
                            .expect("a result")
                            .answer
                })
            });
            let take_back = scope.spawn(|| {
                start.wait();
                integrator
                    .remote_job_take_local(&id, read_epoch)
                    .expect("take local")
            });
            (
                upload.join().expect("upload"),
                take_back.join().expect("take"),
            )
        });
        assert!(node_won != local_won, "round {round}: exactly one winner");
        let job = state(&store, &id);
        if node_won {
            remote += 1;
            assert_eq!(job.state, RemoteJobState::Completed);
        } else {
            local += 1;
            assert_eq!(job.state, RemoteJobState::FallbackLocal);
            assert!(
                !store
                    .node_gw_result(&node, &id, read_epoch + 1, &result(0, 'd'), |_| true)
                    .expect("a late result")
                    .answer,
                "a job taken back is never completed by a node"
            );
        }
    }
    assert_eq!(remote + local, 40);
}

/// The gateway's login reaches exactly §4.3's list: the four
/// `spawn_node_gw_*` functions and PUBLIC's surface, which a later PUBLIC
/// grant may not widen silently; no table but `agent_commands`' INSERT (the
/// grant-checked gateway, where it has no grant). It is nobody's caller,
/// and its timeouts are its role's defaults.
#[test]
#[allow(clippy::too_many_lines)]
fn the_gateway_login_reaches_exactly_its_functions_and_no_table() {
    let (_store, schema) = store("rj_gateway_role");
    let mut client = admin(&schema);
    let role = common::login(&schema, "spawn_node_gateway");
    let functions: Vec<String> = client
        .query(
            "select p.oid::regprocedure::text from pg_proc p
              where p.pronamespace = current_schema()::regnamespace
                and has_function_privilege($1, p.oid, 'execute')
              order by 1",
            &[&role],
        )
        .expect("the reachable functions")
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(
        functions,
        [
            "spawn_experience_int(jsonb)",
            "spawn_gateway_insert()",
            "spawn_gateway_submit(text,text,text,jsonb)",
            "spawn_grant_matches(text,text)",
            "spawn_login_role(text)",
            "spawn_mission_caller()",
            "spawn_node_gw_beat(text,text,bigint,jsonb)",
            "spawn_node_gw_enroll(bytea,bytea,text,text,jsonb)",
            "spawn_node_gw_result(text,text,bigint,jsonb)",
            "spawn_node_gw_spec(text,text,text,jsonb)",
            "spawn_now_ms()",
            "spawn_principal()",
            "spawn_read_freshness()",
            "spawn_read_judgements(text)",
            "spawn_read_state()",
            "spawn_read_status()",
            "spawn_read_tasks()",
            "spawn_read_timeline(text)",
            "spawn_source_allowed(text)",
        ],
        "the gateway login's functions (design §4.3)"
    );
    let relations: Vec<String> = client
        .query(
            "select c.relname || ':' || pr from pg_class c
               cross join unnest(array['SELECT', 'INSERT', 'UPDATE', 'DELETE', 'TRUNCATE',
                                       'REFERENCES', 'TRIGGER']) pr
              where c.relnamespace = current_schema()::regnamespace
                and c.relkind in ('r', 'v', 'm', 'p', 'f')
                and (has_table_privilege($1, c.oid, pr)
                     or (pr in ('SELECT', 'UPDATE', 'REFERENCES')
                         and has_any_column_privilege($1, c.oid, pr)))
             union all
             select c.relname || ':' || pr from pg_class c
               cross join unnest(array['USAGE', 'SELECT', 'UPDATE']) pr
              where c.relnamespace = current_schema()::regnamespace and c.relkind = 'S'
                and has_sequence_privilege($1, c.oid, pr)
              order by 1",
            &[&role],
        )
        .expect("the reachable relations")
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(relations, ["agent_commands:INSERT"], "no table privilege");
    let creates: bool = client
        .query_one(
            "select has_schema_privilege($1, current_schema(), 'CREATE')",
            &[&role],
        )
        .expect("schema privilege")
        .get(0);
    assert!(!creates);

    // As the login itself.
    let mut gateway = common::as_login(&schema, "spawn_node_gateway");
    let caller: Option<String> = gateway
        .query_one("select spawn_mission_caller()", &[])
        .expect("its caller")
        .get(0);
    assert_eq!(caller, None, "the gateway is nobody's caller");
    for (setting, value) in [
        ("statement_timeout", "5s"),
        ("lock_timeout", "2s"),
        ("idle_in_transaction_session_timeout", "5s"),
    ] {
        let shown: String = gateway
            .query_one(&format!("show {setting}"), &[])
            .expect("a setting")
            .get(0);
        assert_eq!(shown, value, "{setting}");
    }
    for refused in [
        "select * from spawn_node_state",
        "select * from spawn_remote_jobs",
        "select spawn_node_sweep()",
        "select spawn_remote_job_take_local('gate-none-00000', 0)",
        "select spawn_remote_job_valid('gate-none-00000', 0)",
        "select spawn_node_events('[]')",
    ] {
        let error = gateway.query(refused, &[]).expect_err(refused);
        assert_eq!(
            error.code(),
            Some(&postgres::error::SqlState::INSUFFICIENT_PRIVILEGE),
            "{refused}"
        );
    }
    let unknown: Value = gateway
        .query_one(
            "select spawn_node_gw_beat($1, $2, 1, '{}')",
            &[&"0".repeat(64), &boot(1)],
        )
        .expect("a beat of an unknown node")
        .get(0);
    assert_eq!(unknown["error"], "unknown_node");
}

/// Everything a migration says about the ledger and the schema: functions
/// (body, settings, rights; a NULL ACL read as the default it means, which
/// some existing objects spell out on a second migration), relations and their rights, columns,
/// constraints, indexes, the gateway login, and the ledger's rows.
fn catalog(client: &mut Client) -> Vec<String> {
    client
        .query(
            "select 'fn ' || p.oid::regprocedure::text || ' ' || md5(p.prosrc) || ' '
                    || coalesce(array_to_string(p.proconfig, ','), '') || ' '
                    || coalesce(p.proacl, acldefault('f', p.proowner))::text
               from pg_proc p where p.pronamespace = current_schema()::regnamespace
             union all
             select 'rel ' || c.relname || ' ' || c.relkind::text || ' ' || coalesce(c.relacl,
                             acldefault(case c.relkind when 'S' then 's' else 'r' end::\"char\", c.relowner))::text
                    || ' ' || coalesce(array_to_string(c.reloptions, ','), '')
               from pg_class c where c.relnamespace = current_schema()::regnamespace
             union all
             select 'col ' || c.relname || '.' || a.attname || ' ' || format_type(a.atttypid, a.atttypmod)
                    || ' ' || a.attnotnull || ' ' || coalesce(pg_get_expr(d.adbin, d.adrelid), '')
               from pg_attribute a join pg_class c on c.oid = a.attrelid
               left join pg_attrdef d on d.adrelid = a.attrelid and d.adnum = a.attnum
              where c.relnamespace = current_schema()::regnamespace and a.attnum > 0
                and not a.attisdropped
             union all
             select 'con ' || conrelid::regclass::text || ' ' || conname || ' '
                    || pg_get_constraintdef(oid)
               from pg_constraint where connamespace = current_schema()::regnamespace
             union all
             select 'idx ' || pg_get_indexdef(indexrelid) from pg_index i
               join pg_class c on c.oid = i.indexrelid
              where c.relnamespace = current_schema()::regnamespace
             union all
             select 'role ' || r.rolname || ' ' || r.rolcanlogin || ' '
                    || coalesce(array_to_string(r.rolconfig, ','), '') || ' '
                    || coalesce(shobj_description(r.oid, 'pg_authid'), '')
               from pg_roles r where r.rolname = spawn_login_role('spawn_node_gateway')
             union all
             select 'schema ' || coalesce(n.nspacl::text, '') from pg_namespace n
              where n.oid = current_schema()::regnamespace
             union all
             select 'node ' || to_jsonb(n)::text from spawn_node_state n
             union all
             select 'job ' || to_jsonb(j)::text from spawn_remote_jobs j
             union all
             select 'events ' || count(*) from spawn_events
             order by 1",
            &[],
        )
        .expect("the catalog")
        .iter()
        .map(|row| row.get(0))
        .collect()
}

/// A second migration over a database the ledger has been used in changes
/// nothing: every statement of the S1 block is idempotent.
#[test]
fn a_second_migration_changes_nothing() {
    let (store, schema) = store("rj_migrate_twice");
    let node = enroll(&store, "nm", 10, &json!({}));
    store
        .remote_job_enqueue(&job("gate-twice-0001", &[], &[("gate_sandbox", 1)]))
        .expect("enqueue");
    beat_once(&store, &node, &boot(1), 1, &beat(&[], &[], 1, 0));
    let mut client = admin(&schema);
    let before = catalog(&mut client);
    assert!(
        before
            .iter()
            .any(|line| line.starts_with("fn spawn_node_gw_beat(")),
        "the ledger is in the catalog"
    );
    store.migrate().expect("migrate again");
    let after = catalog(&mut client);
    let gone: Vec<&String> = before.iter().filter(|line| !after.contains(line)).collect();
    let new: Vec<&String> = after.iter().filter(|line| !before.contains(line)).collect();
    assert!(
        gone.is_empty() && new.is_empty(),
        "changed: {gone:?} -> {new:?}"
    );
}

/// The schema as it was before PR S1: this file without its S1 block. (The
/// `spawn_node_record` name check stays: plpgsql reads the table only when
/// a join runs, and this test records none.)
fn old_schema() -> String {
    let start = SCHEMA.find(BLOCK_BEGIN).expect("the block's beginning");
    let end = SCHEMA.find(BLOCK_END).expect("the block's end") + BLOCK_END.len();
    format!("{}{}", &SCHEMA[..start], &SCHEMA[end..])
}

/// A database still on the old schema (a rollback, or a reader that lags)
/// refolds a log that holds every new v1 record: `spawn_control_refold`
/// (and the grading fold) read none of them, and rebuild the missions and
/// tasks exactly as the new schema's fold does.
#[test]
#[allow(clippy::too_many_lines)]
fn an_old_schema_refolds_a_log_holding_the_v1_records() {
    let (store, schema) = store("rj_old_refold");
    let mission = store
        .mission_add("Remote", &json!([]), "human")
        .expect("a mission");
    store
        .task_assign(
            mission,
            "ephemeral",
            "worker",
            "judge it remotely",
            "default",
        )
        .expect("a task");
    // Every v1 record type.
    let node = enroll(&store, "nr", 11, &json!({}));
    store
        .node_gw_spec(
            &node,
            &boot(1),
            &"a".repeat(64),
            &json!({"holds": {}}),
            |_| true,
        )
        .expect("spec");
    for n in 0..2 {
        store
            .remote_job_enqueue(&job(
                &format!("gate-old-{n:04}"),
                &[],
                &[("gate_sandbox", 1)],
            ))
            .expect("enqueue");
    }
    let mut body = beat(&[], &[], 2, 0);
    body["spec_sha256"] = json!("a".repeat(64));
    beat_once(&store, &node, &boot(1), 1, &body);
    complete(&store, &node, "gate-old-0000", 1);
    store
        .remote_job_fault("gate-old-0000", 1, "host_fault", true, FaultNext::Keep)
        .expect("a fault");
    let mut client = admin(&schema);
    client
        .batch_execute("update spawn_node_state set last_beat_ms = 1")
        .expect("silence");
    store.node_sweep().expect("a sweep: lost");
    beat_once(&store, &node, &boot(2), 1, &body); // back, and a new boot's fence
    store.node_admin("nr", NodeAction::Drain).expect("drain");
    store
        .node_admin("nr", NodeAction::Undrain)
        .expect("undrain");
    store.node_admin("nr", NodeAction::Revoke).expect("revoke");
    let records = node_records(&mut client);
    for record in [
        "node.enrolled",
        "node.spec_registered",
        "node.lost",
        "node.back",
        "node.drained",
        "node.undrained",
        "node.revoked",
        "remote_job.leased",
        "remote_job.completed",
        "remote_job.host_fault",
        "remote_job.lease_fenced",
    ] {
        assert!(
            records.contains_key(record),
            "{record} in the log: {records:?}"
        );
    }

    // The old schema, in a schema of its own, given that log.
    let old = common::Schema::new("rj_old_schema");
    let mut old_client = Client::connect(&common::url(), postgres::NoTls).expect("connect");
    old_client
        .batch_execute(&format!(
            "{}; set search_path to {}",
            spawn_postgres::made_schema_sql(&old),
            &*old
        ))
        .expect("the old schema's schema");
    old_client
        .batch_execute(&old_schema())
        .expect("the old schema applies");
    let present: Option<String> = old_client
        .query_one("select to_regclass('spawn_remote_jobs')::text", &[])
        .expect("regclass")
        .get(0);
    assert_eq!(present, None, "the old schema has no ledger");
    old_client
        .batch_execute(&format!(
            "insert into spawn_events (id, agent_id, session_id, execution_id, source,
                                       source_event_id, kind, session_seq, recorded_at_ms)
             select id, agent_id, session_id, execution_id, source, source_event_id, kind,
                    session_seq, recorded_at_ms
               from {schema}.spawn_events order by id;
             select spawn_control_refold();
             select spawn_grading_refold();",
            schema = &*schema
        ))
        .expect("the old schema refolds the new log");
    client
        .batch_execute("select spawn_control_refold(); select spawn_grading_refold();")
        .expect("the new schema refolds it too");
    let folded = |client: &mut Client| -> Vec<String> {
        client
            .query(
                "select 'mission ' || to_jsonb(m)::text from mission_missions m
                 union all select 'task ' || to_jsonb(t)::text from mission_tasks t
                 union all select 'resident ' || to_jsonb(r)::text from mission_residents r
                 order by 1",
                &[],
            )
            .expect("the folded tables")
            .iter()
            .map(|row| row.get(0))
            .collect()
    };
    let new_fold = folded(&mut client);
    assert!(
        new_fold.iter().any(|line| line.starts_with("task ")),
        "{new_fold:?}"
    );
    assert_eq!(
        folded(&mut old_client),
        new_fold,
        "the same fold, old schema or new"
    );
    drop(old_client);
}

/// One name is one node: `spawn_node_record` refuses to join a name the v1
/// ledger holds, and `spawn_node_enroll_token` a name a P1 join holds.
#[test]
fn a_v1_name_is_never_a_p1_node() {
    let (store, _schema) = store("rj_names");
    enroll(&store, "both", 12, &json!({}));
    let joined = store.node_record(&json!({"type": "node.joined", "name": "both",
        "ssh": "host.example", "arch": "x86_64", "os": "linux", "kernel": "6",
        "cores": 4, "mem_total": 1, "disk_free": 1, "cpu_max": null, "mem_max": null,
        "gpu": null, "exit_country": null}));
    let error = joined.expect_err("a v1 name is not joined");
    assert!(
        format!("{error:?}").contains("spawn-node v1 node"),
        "{error:?}"
    );
    store
        .node_record(&json!({"type": "node.joined", "name": "p1only",
            "ssh": "host.example", "arch": "x86_64", "os": "linux", "kernel": "6",
            "cores": 4, "mem_total": 1, "disk_free": 1, "cpu_max": null, "mem_max": null,
            "gpu": null, "exit_country": null}))
        .expect("a P1 join");
    assert!(
        store
            .node_enroll_token("p1only", None, &[13; 32], 600_000)
            .is_err(),
        "a P1 name is not enrolled"
    );
}
