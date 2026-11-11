//! A judging's outcome is recorded once (gate v2,
//! design-gate-survives-restart.md D4): a gate may outlive the integrator
//! that started it, and whoever reaps it records its outcome naming the
//! judging. A second record of that kind for that judging returns the
//! first and records nothing; the store's unique index refuses one that
//! gets past the function.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use serde_json::{Value, json};
use spawn_postgres::PostgresStore;

mod common;
use common::{admin, store};

const MAIN: &str = "1111111111111111111111111111111111111111";
const JUDGING: &str = "0193a2b4c5d6e7f80123456789abcdef";

/// The control records of `kind` naming `judging`, oldest first.
fn recorded(client: &mut Client, kind: &str, judging: &str) -> Vec<(i64, Value)> {
    client
        .query(
            "select id, kind -> 'detail' -> 'record' from spawn_events
              where session_id = 'session_control'
                and kind #>> '{detail,record,type}' = $1
                and kind #>> '{detail,record,judging}' = $2
              order by id",
            &[&kind, &judging],
        )
        .expect("read the control records")
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect()
}

/// A gated mission on `source` with a reported task, ready to be judged.
fn reported_task(store: &PostgresStore, source: &str) -> i64 {
    let mission = store
        .mission_add("judged work", &json!([source]), "human")
        .expect("add");
    store
        .mission_configure(mission, Some("/scenarios"), "gate", None, None)
        .expect("configure");
    let task = store
        .task_assign(mission, "ephemeral", "w", "brief", "default")
        .expect("assign");
    store.task_delivered(task, 1).expect("delivered");
    assert!(store.task_reported(task, "done").expect("reported"));
    task
}

#[test]
fn a_judgings_verdict_and_landing_are_each_recorded_once() {
    let (store, schema) = store("judging_once");
    let mut client = admin(&schema);
    let task = reported_task(&store, "/srv/judged");
    let verdict = json!({"type": "task.gated", "id": task, "revision": 0, "verdict": "accepted",
                         "base": MAIN, "candidate": "c1", "judging": JUDGING});
    let first = store.integrator_record(&verdict).expect("the verdict");
    // The reaper again (a second integrator, or one restarted between its
    // record and its cleanup): the first verdict, nothing more, although
    // the task has moved to landing since.
    assert_eq!(store.integrator_record(&verdict).expect("again"), first);
    assert_eq!(recorded(&mut client, "task.gated", JUDGING).len(), 1);
    let landed = json!({"type": "task.landed", "id": task, "from": MAIN, "to": "c1",
                        "judging": JUDGING});
    let landing = store.integrator_record(&landed).expect("the landing");
    assert_ne!(landing, first, "a landing is its own record");
    assert_eq!(store.integrator_record(&landed).expect("again"), landing);
    assert_eq!(recorded(&mut client, "task.landed", JUDGING).len(), 1);
    // The verdict again after the landing: still the first.
    assert_eq!(store.integrator_record(&verdict).expect("once more"), first);
    // Another verdict of the same judging is its own record (the store's
    // checks apply to it: the task has landed, so it is refused).
    let stale = json!({"type": "task.gated", "id": task, "revision": 0, "verdict": "stale",
                       "base": MAIN, "judging": JUDGING});
    assert!(
        store.integrator_record(&stale).is_err(),
        "a stale is no accepted again"
    );
    // Another judging of the same task is its own: the store's own checks
    // apply to it (the task has landed, so a verdict is refused).
    let other = json!({"type": "task.gated", "id": task, "revision": 0, "verdict": "accepted",
                       "base": MAIN, "candidate": "c1",
                       "judging": "0193a2b4c5d6e7f8fedcba9876543210"});
    assert!(
        store.integrator_record(&other).is_err(),
        "a new judging is checked"
    );
}

#[test]
fn a_judging_is_hex_and_names_only_a_gates_outcome() {
    let (store, _schema) = store("judging_shape");
    let task = reported_task(&store, "/srv/shaped");
    for judging in [
        json!("short"),
        json!("0193A2B4C5D6E7F8"),
        json!(12),
        json!("0193a2b4/c5d6e7f8"),
    ] {
        let error = store
            .integrator_record(&json!({"type": "task.gated", "id": task, "revision": 0,
                                       "verdict": "accepted", "base": MAIN, "candidate": "c1",
                                       "judging": judging}))
            .expect_err("refused")
            .to_string();
        assert!(error.contains("a judging names"), "{judging}: {error}");
    }
    let error = store
        .integrator_record(&json!({"type": "task.held", "id": task, "hold": 1,
                                   "judging": JUDGING}))
        .expect_err("refused")
        .to_string();
    assert!(error.contains("a judging names"), "{error}");
}

#[test]
fn the_index_refuses_a_second_outcome_of_one_judging_that_gets_past_the_function() {
    let (store, schema) = store("judging_index");
    let mut client = admin(&schema);
    let task = reported_task(&store, "/srv/indexed");
    // Without a judging, records of one kind repeat as before.
    let plain = json!({"type": "task.gated", "id": task, "revision": 0, "verdict": "stale",
                       "base": MAIN, "main_now": "2222222222222222222222222222222222222222"});
    client
        .query_one("select spawn_control_record($1)", &[&plain])
        .expect("one");
    client
        .query_one("select spawn_control_record($1)", &[&plain])
        .expect("two");
    let record = json!({"type": "task.gated", "id": task, "revision": 0, "verdict": "rejected",
                        "base": MAIN, "candidate": "c1", "judging": JUDGING});
    client
        .query_one("select spawn_control_record($1)", &[&record])
        .expect("the first");
    let error = client
        .query_one("select spawn_control_record($1)", &[&record])
        .expect_err("the second is refused");
    assert_eq!(
        error.code(),
        Some(&postgres::error::SqlState::UNIQUE_VIOLATION),
        "{error}"
    );
    // Another verdict of the same judging, past the function: refused too.
    let other = json!({"type": "task.gated", "id": task, "revision": 0, "verdict": "aborted",
                       "base": MAIN, "reason": "x", "judging": JUDGING});
    let error = client
        .query_one("select spawn_control_record($1)", &[&other])
        .expect_err("another verdict is refused");
    assert_eq!(
        error.code(),
        Some(&postgres::error::SqlState::UNIQUE_VIOLATION),
        "{error}"
    );
}

/// A reap that decides differently the second time (rev-e F1: the
/// host-fault cap counts the judging's own host fault, so a replay may say
/// `aborted` where the first said `host_fault`) gets the first verdict
/// back, and nothing it would have changed moves: one verdict per judging.
#[test]
fn a_judgings_first_verdict_stands_whatever_a_replay_decides() {
    let (store, schema) = store("judging_first_stands");
    let mut client = admin(&schema);
    let task = reported_task(&store, "/srv/replayed");
    let host_fault = json!({"type": "task.gated", "id": task, "revision": 0,
                            "verdict": "host_fault", "base": MAIN, "evidence": "gate",
                            "backoff_ms": 60_000,
                            "fact": {"scenario": "check-tests.sh", "on": "base", "runs": "2",
                                     "reason": "x"},
                            "judging": JUDGING});
    let aborted = json!({"type": "task.gated", "id": task, "revision": 0, "verdict": "aborted",
                         "base": MAIN, "reason": "the host-fault cap", "judging": JUDGING});
    let first = store
        .integrator_record(&host_fault)
        .expect("the host fault");
    assert_eq!(
        store.integrator_record(&aborted).expect("the replay"),
        first,
        "the first verdict is returned"
    );
    let stale = json!({"type": "task.gated", "id": task, "revision": 0, "verdict": "stale",
                       "base": MAIN, "main_now": "2222222222222222222222222222222222222222",
                       "judging": JUDGING});
    assert_eq!(
        store.integrator_record(&stale).expect("a stale replay"),
        first,
        "a stale after a verdict that is no acceptance is a replay too"
    );
    let gated = recorded(&mut client, "task.gated", JUDGING);
    assert_eq!(gated.len(), 1, "{gated:?}");
    assert_eq!(gated[0].1["verdict"], "host_fault");
    let (aborts, faults): (i32, i32) = client
        .query_one(
            "select gate_aborts, gate_host_faults from mission_missions m
               join mission_tasks t on t.mission_id = m.id where t.id = $1",
            &[&task],
        )
        .map(|row| (row.get(0), row.get(1)))
        .expect("the mission");
    assert_eq!((aborts, faults), (0, 1), "only the host fault counted");
}

/// A judging is about one task or one owner's request (rev-e F2): an
/// outcome naming another under it is refused, never answered with the
/// first's id.
#[test]
fn a_judging_is_about_one_task() {
    let (store, schema) = store("judging_one_subject");
    let mut client = admin(&schema);
    let first = reported_task(&store, "/srv/subject-a");
    let second = reported_task(&store, "/srv/subject-b");
    let rejected = |task: i64| {
        json!({"type": "task.gated", "id": task, "revision": 0, "verdict": "rejected",
               "base": MAIN, "candidate": "c1", "reason": "r", "judging": JUDGING})
    };
    store
        .integrator_record(&rejected(first))
        .expect("the first task's");
    let error = store
        .integrator_record(&rejected(second))
        .expect_err("another task under that judging")
        .to_string();
    assert!(
        error.contains(&format!("is about task {first} already, not task {second}")),
        "{error}"
    );
    let landed = json!({"type": "task.landed", "id": second, "from": MAIN, "to": "c1",
                        "judging": JUDGING});
    assert!(store.integrator_record(&landed).is_err(), "nor its landing");
    let refused = json!({"type": "source.push_refused", "request": 1, "reason": "x",
                         "judging": JUDGING});
    let error = store
        .integrator_record(&refused)
        .expect_err("an owner's request under a task's judging")
        .to_string();
    assert!(error.contains("not request 1"), "{error}");
    assert_eq!(recorded(&mut client, "task.gated", JUDGING).len(), 1);
}
/// One judging records `accepted` and then `stale` when main moved under
/// its landing: two records, each once, whoever repeats them.
#[test]
fn a_judging_records_accepted_then_stale_each_once() {
    let (store, schema) = store("judging_accepted_stale");
    let mut client = admin(&schema);
    let task = reported_task(&store, "/srv/moved");
    let accepted = json!({"type": "task.gated", "id": task, "revision": 0, "verdict": "accepted",
                          "base": MAIN, "candidate": "c1", "judging": JUDGING});
    let stale = json!({"type": "task.gated", "id": task, "revision": 0, "verdict": "stale",
                       "base": MAIN, "main_now": "2222222222222222222222222222222222222222",
                       "judging": JUDGING});
    let first = store.integrator_record(&accepted).expect("accepted");
    let second = store.integrator_record(&stale).expect("stale, after it");
    assert_ne!(first, second);
    assert_eq!(
        store.integrator_record(&stale).expect("stale again"),
        second
    );
    assert_eq!(
        store.integrator_record(&accepted).expect("accepted again"),
        first
    );
    assert_eq!(recorded(&mut client, "task.gated", JUDGING).len(), 2);
}

/// A gate whose runner ended without the gate's status (gate v2 step 2,
/// D6) is a host fault with evidence `abandoned`, `on` `runner`, runs `no
/// status`; another shape of it is refused.
#[test]
fn an_abandoned_gate_is_a_host_fault_of_its_own_shape() {
    let (store, _schema) = store("judging_abandoned");
    let task = reported_task(&store, "/srv/abandoned");
    let fault = |fact: Value| {
        json!({"type": "task.gated", "id": task, "revision": 0, "verdict": "host_fault",
               "base": MAIN, "evidence": "abandoned", "backoff_ms": 60_000,
               "fact": fact, "judging": JUDGING})
    };
    for wrong in [
        json!({"scenario": "gate", "on": "candidate and base", "runs": "no status"}),
        json!({"scenario": "gate", "on": "runner", "runs": "1 1"}),
        json!({"scenario": "gate"}),
    ] {
        let error = store
            .integrator_record(&fault(wrong.clone()))
            .expect_err("refused")
            .to_string();
        assert!(
            error.contains("a host_fault verdict names"),
            "{wrong}: {error}"
        );
    }
    store
        .integrator_record(&fault(json!({"scenario": "gate", "on": "runner",
                                          "runs": "no status", "reason": "OOM"})))
        .expect("abandoned");
}

/// A judging whose verdict is recorded has no `abandoned` after it: the
/// integrator that read the gate's status recorded the verdict and removed
/// the judging's directory, so another that adopted the same gate finds its
/// runner gone and no status. Its host fault returns the verdict, and the
/// task stays as the verdict left it. A judging with no verdict yet records
/// its `abandoned`.
#[test]
fn a_judgings_verdict_stands_over_a_later_abandoned() {
    let (store, schema) = store("judging_verdict_then_abandoned");
    let mut client = admin(&schema);
    let task = reported_task(&store, "/srv/verdict-abandoned");
    let accepted = json!({"type": "task.gated", "id": task, "revision": 0, "verdict": "accepted",
                          "base": MAIN, "candidate": "c1", "judging": JUDGING});
    let abandoned = |judging: &str| {
        json!({"type": "task.gated", "id": task, "revision": 0, "verdict": "host_fault",
               "base": MAIN, "evidence": "abandoned", "backoff_ms": 60_000,
               "fact": {"scenario": "gate", "on": "runner", "runs": "no status",
                        "reason": "gone"},
               "judging": judging})
    };
    let verdict = store.integrator_record(&accepted).expect("accepted");
    assert_eq!(
        store
            .integrator_record(&abandoned(JUDGING))
            .expect("abandoned after it"),
        verdict,
        "the verdict is returned"
    );
    let gated = recorded(&mut client, "task.gated", JUDGING);
    assert_eq!(gated.len(), 1, "{gated:?}");
    assert_eq!(gated[0].1["verdict"], "accepted");
    let host_faults: i32 = client
        .query_one(
            "select gate_host_faults from mission_missions m
               join mission_tasks t on t.mission_id = m.id where t.id = $1",
            &[&task],
        )
        .expect("the mission")
        .get(0);
    assert_eq!(host_faults, 0, "no host fault counted");

    let other = "0193a2b4c5d6e7f80123456789abcde0";
    let fault = store
        .integrator_record(&abandoned(other))
        .expect("abandoned with no verdict");
    assert_ne!(fault, verdict);
    assert_eq!(recorded(&mut client, "task.gated", other).len(), 1);
}

/// An owner judging's `source.push_no_verdict` (P26) is recorded once per
/// judging, and none after that judging's outcome: a later one returns the
/// outcome, nothing more recorded. It is no outcome itself: the request it
/// names stays pending (`source_pushes_pending`).
#[test]
fn an_owner_judgings_no_verdict_is_once_and_never_after_its_outcome() {
    let (store, schema) = store("judging_no_verdict");
    let mut client = admin(&schema);
    reported_task(&store, "/srv/owned");
    let request = store
        .source_push_request(
            "/srv/owned",
            MAIN,
            &format!("refs/spawn-owner/{MAIN}"),
            &spawn_postgres::PushRequest {
                gated: true,
                urgent: false,
                reason: None,
            },
        )
        .expect("a gated request");
    let no_verdict = json!({"type": "source.push_no_verdict", "request": request,
                            "reason": "the host's fault on check-tests.sh: r",
                            "log": "/root/no-verdict-logs/1-request-1-x.log",
                            "judging": JUDGING});
    let first = store
        .integrator_record(&no_verdict)
        .expect("the no-verdict");
    assert_eq!(store.integrator_record(&no_verdict).expect("again"), first);
    assert_eq!(
        recorded(&mut client, "source.push_no_verdict", JUDGING).len(),
        1
    );
    let pending = store.source_pushes_pending().expect("pending");
    assert!(
        pending
            .to_string()
            .contains(&format!("\"request\":{request}")),
        "still pending: {pending}"
    );
    // Another judging of the request: refused, then a no-verdict under it
    // returns the refusal, nothing more.
    let second = "0193a2b4c5d6e7f8fedcba9876543210";
    let refused = json!({"type": "source.push_refused", "request": request, "reason": "x",
                         "judging": second});
    let outcome = store.integrator_record(&refused).expect("the refusal");
    let late = json!({"type": "source.push_no_verdict", "request": request, "reason": "r",
                      "log": null, "judging": second});
    assert_eq!(
        store.integrator_record(&late).expect("after its outcome"),
        outcome
    );
    assert!(recorded(&mut client, "source.push_no_verdict", second).is_empty());
}
