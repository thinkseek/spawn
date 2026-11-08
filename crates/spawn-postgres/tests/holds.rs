//! The owner's hold on a mission source (objective 034,
//! docs/fleet-architecture.md §10.3, "Landing: the owner's hold"): the hold,
//! the release and the expiry are records, and the `mission_source_holds`
//! table and each task's `held_by` are their fold.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use serde_json::{Value, json};
use spawn_postgres::PostgresStore;

mod common;
use common::{admin, login, store};

/// A main as a source's commit id.
const MAIN: &str = "1111111111111111111111111111111111111111";
const OTHER_MAIN: &str = "2222222222222222222222222222222222222222";

/// The control records of `kind`, oldest first, each with its event id and
/// time: `[{id, at_ms, record}]`.
fn records(client: &mut Client, kind: &str) -> Vec<Value> {
    client
        .query(
            "select jsonb_build_object('id', id, 'at_ms', recorded_at_ms,
                                       'record', kind -> 'detail' -> 'record')
               from spawn_events
              where session_id = 'session_control' and event_name = 'control.recorded'
                and kind -> 'detail' -> 'record' ->> 'type' = $1
              order by session_seq",
            &[&kind],
        )
        .expect("read the control records")
        .iter()
        .map(|row| row.get(0))
        .collect()
}

/// Every source record (`source.%`), by type.
fn source_types(client: &mut Client) -> Vec<String> {
    client
        .query(
            "select kind -> 'detail' -> 'record' ->> 'type' from spawn_events
              where session_id = 'session_control'
                and kind -> 'detail' -> 'record' ->> 'type' like 'source.%'
              order by session_seq",
            &[],
        )
        .expect("read the source records")
        .iter()
        .map(|row| row.get(0))
        .collect()
}

/// The holds table and every task's `held_by`, in a stable order.
fn folded(client: &mut Client) -> Value {
    client
        .query_one(
            "select jsonb_build_object(
               'holds', (select coalesce(jsonb_agg(to_jsonb(h) order by h.source), '[]')
                           from mission_source_holds h),
               'tasks', (select coalesce(jsonb_agg(jsonb_build_object('id', k.id, 'state', k.state,
                                                                      'held_by', k.held_by)
                                                   order by k.id), '[]')
                           from mission_tasks k))",
            &[],
        )
        .expect("read the fold")
        .get(0)
}

fn refold(client: &mut Client) {
    client
        .batch_execute("select spawn_control_refold()")
        .expect("refold the control stream");
}

/// Hold `source` as the owner's CLI does: under the landing lock.
fn hold(
    store: &PostgresStore,
    source: &str,
    main: &str,
    bound_s: i32,
    reason: Option<&str>,
) -> Value {
    let lock = store.source_hold_lock(source).expect("the landing lock");
    let held = store
        .source_hold(source, main, bound_s, reason)
        .expect("the hold");
    drop(lock);
    held
}

/// A gated mission on `source` with a task its gate accepted: `landing`.
fn landing_task(store: &PostgresStore, source: &str) -> (i64, i64) {
    let mission = store
        .mission_add("held work", &json!([source]), "human")
        .expect("add");
    store
        .mission_configure(mission, Some("/scenarios"), "gate", None, None)
        .expect("configure");
    let task = store
        .task_assign(mission, "ephemeral", "w", "brief", "default")
        .expect("assign");
    store.task_delivered(task, 1).expect("delivered");
    assert!(store.task_reported(task, "done").expect("reported"));
    store
        .integrator_record(&json!({"type": "task.gated", "id": task, "revision": 0,
                                    "verdict": "accepted", "base": MAIN, "candidate": "c1"}))
        .expect("accepted");
    (mission, task)
}

fn refusal<T: std::fmt::Debug>(result: Result<T, spawn_core::port::StoreError>) -> String {
    result.expect_err("refused").to_string()
}

#[test]
fn a_hold_is_the_owners_on_a_local_source_a_mission_names_with_a_bound() {
    let (store, schema) = store("holds_refused");
    let source = "/tmp/held.git";
    let remote = "https://example.invalid/held.git";
    for named in [source, remote] {
        store
            .mission_add("on it", &json!([named]), "human")
            .expect("add");
    }
    // A source no mission names, or one only a later mission source names.
    store
        .mission_add(
            "two sources",
            &json!(["/tmp/first.git", "/tmp/second.git"]),
            "human",
        )
        .expect("add");
    for (unnamed, why) in [
        ("/tmp/no-mission.git", "no mission names"),
        ("/tmp/second.git", "no mission names"),
        (remote, "remote"),
        ("relative/path.git", "a source is"),
    ] {
        let refused = refusal(store.source_hold_lock(unnamed));
        assert!(refused.contains(why), "{unnamed}: {refused}");
    }
    let mut client = admin(&schema);
    assert!(source_types(&mut client).is_empty(), "nothing was recorded");

    // A bad bound, a long reason or a main that is no commit: refused under
    // the lock, nothing recorded.
    let lock = store.source_hold_lock(source).expect("the lock");
    for bound in [0, -1, 86_401] {
        let refused = refusal(store.source_hold(source, MAIN, bound, None));
        assert!(
            refused.contains("SPAWN_SOURCE_HOLD_SECS"),
            "{bound}: {refused}"
        );
    }
    let long = "x".repeat(501);
    assert!(refusal(store.source_hold(source, MAIN, 60, Some(&long))).contains("500"));
    assert!(refusal(store.source_hold(source, "main", 60, None)).contains("commit"));
    drop(lock);
    // Not under the landing lock: refused.
    let refused = refusal(store.source_hold(source, MAIN, 60, None));
    assert!(refused.contains("landing lock"), "{refused}");
    assert!(source_types(&mut client).is_empty(), "nothing was recorded");

    // The supervisor's login is refused every command.
    let supervisor = login(&schema, "spawn_supervisor");
    client
        .batch_execute(&format!("set session authorization {supervisor}"))
        .expect("become the supervisor");
    for call in [
        "select spawn_source_hold_lock('/tmp/held.git')",
        "select spawn_source_hold('/tmp/held.git', '1111111111111111111111111111111111111111', 60, null)",
        "select spawn_source_release('/tmp/held.git')",
    ] {
        let refused = client
            .batch_execute(call)
            .expect_err("the supervisor is refused");
        assert_eq!(
            refused.code(),
            Some(&postgres::error::SqlState::INSUFFICIENT_PRIVILEGE),
            "{call}: {refused}"
        );
    }
    // It reads the holds on the board.
    client
        .batch_execute("select * from mission_source_holds")
        .expect("the supervisor reads the holds");
    client
        .batch_execute("reset session authorization")
        .expect("back to the owner");
    assert!(source_types(&mut client).is_empty(), "nothing was recorded");
}

#[test]
fn the_store_writes_expires_ms_and_a_second_hold_renews_the_first() {
    let (store, schema) = store("holds_renewed");
    let source = "/tmp/renewed.git";
    let (_, task) = landing_task(&store, source);
    let first = hold(&store, source, MAIN, 10_800, Some("owner gate abc123"));
    let mut client = admin(&schema);
    let held = records(&mut client, "source.held");
    assert_eq!(held.len(), 1);
    let record = &held[0]["record"];
    assert_eq!(
        first["hold"], held[0]["id"],
        "the hold is its record's event id"
    );
    assert_eq!(record["source"], source);
    assert_eq!(record["main"], MAIN);
    assert_eq!(record["by"], "human");
    assert_eq!(record["bound_s"], 10_800);
    assert_eq!(record["reason"], "owner gate abc123");
    assert!(record["renews"].is_null(), "{record}");
    assert_eq!(
        record["expires_ms"].as_i64(),
        held[0]["at_ms"].as_i64().map(|at| at + 10_800_000),
        "its own time plus its bound"
    );
    assert_eq!(first["expires_ms"], record["expires_ms"]);
    assert_eq!(first["main"], MAIN);
    assert!(first["renews"].is_null());
    let first_hold = first["hold"].as_i64().expect("an id");
    let waited = store.task_held(task, first_hold, source).expect("held");
    assert_eq!(
        waited,
        records(&mut client, "task.held")[0]["id"].as_i64(),
        "the task waits behind the first"
    );

    // A second hold renews it: its own main, bound and expiry, `renews` the
    // first, and no release or expiry of the first.
    let second = hold(&store, source, OTHER_MAIN, 60, None);
    assert_eq!(second["renews"], first_hold);
    let held = records(&mut client, "source.held");
    assert_eq!(held.len(), 2);
    let record = &held[1]["record"];
    assert_eq!(record["renews"], first_hold);
    assert_eq!(record["main"], OTHER_MAIN);
    assert!(record.get("reason").is_none(), "no reason given: {record}");
    assert_eq!(
        record["expires_ms"].as_i64(),
        held[1]["at_ms"].as_i64().map(|at| at + 60_000)
    );
    assert_eq!(source_types(&mut client), ["source.held", "source.held"]);
    let fold = folded(&mut client);
    let holds = fold["holds"].as_array().expect("holds");
    assert_eq!(holds.len(), 1, "one active hold per source: {fold}");
    assert_eq!(holds[0]["hold"], second["hold"]);
    assert_eq!(holds[0]["main"], OTHER_MAIN);
    assert_eq!(holds[0]["by"], "human");
    assert_eq!(holds[0]["held_at_ms"], held[1]["at_ms"]);
    // The renewal itself moved the waiting task to the new hold in the fold:
    // the board, a release and an expiry name it at once, and the fold wrote
    // no record of its own.
    let second_hold = second["hold"].as_i64().expect("an id");
    assert_eq!(fold["tasks"][0]["held_by"], second_hold);
    assert_eq!(
        records(&mut client, "task.held").len(),
        1,
        "the fold records nothing"
    );
    let board = store.mission_board(10).expect("board");
    assert_eq!(board["holds"][0]["waiting"], json!([task]));
    assert_eq!(board["missions"][0]["hold"], second_hold);

    // Its landing's own `task.held` follows, once per task and hold.
    assert!(
        store
            .task_held(task, second_hold, source)
            .expect("held")
            .is_some()
    );
    assert_eq!(records(&mut client, "task.held").len(), 2);
    let board = store.mission_board(10).expect("board");
    assert_eq!(board["holds"][0]["waiting"], json!([task]));
    assert_eq!(board["missions"][0]["hold"], second_hold);
}

#[test]
fn a_release_ends_the_active_hold_and_with_none_records_nothing() {
    let (store, schema) = store("holds_released");
    let source = "/tmp/released.git";
    let (_, task) = landing_task(&store, source);
    let mut client = admin(&schema);
    let none = store
        .source_release(source)
        .expect("a release with no hold");
    assert_eq!(
        none,
        json!({"source": source, "released": null, "waiting": []})
    );
    assert!(source_types(&mut client).is_empty(), "nothing was recorded");

    let held = hold(&store, source, MAIN, 600, None);
    let id = held["hold"].as_i64().expect("an id");
    store.task_held(task, id, source).expect("held");
    let released = store.source_release(source).expect("the release");
    assert_eq!(
        released,
        json!({"source": source, "released": id, "waiting": [task]})
    );
    let record = &records(&mut client, "source.released")[0]["record"];
    assert_eq!(record["hold"], id);
    assert_eq!(record["source"], source);
    assert_eq!(record["by"], "human");
    assert_eq!(folded(&mut client)["holds"], json!([]));
    assert_eq!(store.active_hold(source).expect("read"), None);
    // The task waited behind it, and lands by the existing rules: still
    // `landing`, its `held_by` the hold it waited behind.
    assert_eq!(folded(&mut client)["tasks"][0]["held_by"], id);
    assert_eq!(folded(&mut client)["tasks"][0]["state"], "landing");

    let again = store.source_release(source).expect("a second release");
    assert!(again["released"].is_null());
    assert_eq!(
        source_types(&mut client),
        ["source.held", "source.released"]
    );
    // No hold is left to name in a `task.held`.
    assert!(store.task_held(task, id, source).is_err());
}

#[test]
fn a_hold_expires_by_record_only_once_the_database_clock_reached_it() {
    let (store, schema) = store("holds_expired");
    let source = "/tmp/expired.git";
    let (mission, task) = landing_task(&store, source);
    let mut client = admin(&schema);
    // Before its expires_ms (ten minutes after its record): refused.
    let first = hold(&store, source, MAIN, 600, None);
    let first_id = first["hold"].as_i64().expect("an id");
    assert!(refusal(store.hold_expired(source, first_id)).contains("expires at"));
    // Renewed with a bound of a second: the first is no longer the active
    // hold, and its expiry is refused whatever the clock says.
    let held = hold(&store, source, MAIN, 1, None);
    let id = held["hold"].as_i64().expect("an id");
    assert_eq!(held["renews"], first_id);
    assert!(refusal(store.hold_expired(source, first_id)).contains("not the active hold"));
    store.task_held(task, id, source).expect("held");
    // A hold that is not the active one: refused.
    assert!(refusal(store.hold_expired(source, id + 1000)).contains("not the active hold"));
    assert!(refusal(store.hold_expired("/tmp/other.git", id)).contains("not the active hold"));
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let expired = store.hold_expired(source, id).expect("expired");
    let records = records(&mut client, "source.hold_expired");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["id"], expired);
    let record = &records[0]["record"];
    assert_eq!(record["hold"], id);
    assert_eq!(record["source"], source);
    assert_eq!(record["expires_ms"], held["expires_ms"]);
    assert_eq!(record["waiting"], json!([task]));
    assert!(
        records[0]["at_ms"].as_i64() >= held["expires_ms"].as_i64(),
        "recorded at or after its expires_ms"
    );
    assert_eq!(folded(&mut client)["holds"], json!([]));
    // Once: the hold is no longer active.
    assert!(refusal(store.hold_expired(source, id)).contains("not the active hold"));
    // The dispatcher's reading names the hold, its source and its waiting
    // task with its mission, until a digest carries its tag.
    let expiries = store.hold_expiries().expect("expiries");
    assert_eq!(expiries[0]["event_id"], expired);
    assert_eq!(expiries[0]["source"], source);
    assert_eq!(expiries[0]["bound_s"], 1);
    assert_eq!(expiries[0]["main"], MAIN);
    assert_eq!(
        expiries[0]["waiting"],
        json!([{"task": task, "mission": mission}])
    );
    client
        .execute(
            "insert into spawn_commands (principal, idempotency_key, agent_id, session_id, action,
                                         state, succeeded, created_at_ms)
             values ('spawn-dispatch', 'digest-1', 'agent_supervisor', 'session_supervisor',
                     jsonb_build_object('type', 'input', 'text', 'x',
                                        'tags', jsonb_build_array($1::text)),
                     'finished', true, 1)",
            &[&format!("missions source-hold-expired-{expired}")],
        )
        .expect("a digest that told it");
    assert_eq!(store.hold_expiries().expect("expiries"), json!([]));
}

#[test]
fn a_landing_waits_once_per_task_and_hold_and_the_fold_and_refold_agree() {
    let (store, schema) = store("holds_task_held");
    let source = "/tmp/task-held.git";
    let (mission, task) = landing_task(&store, source);
    // A task that is not landing is refused, whatever the hold.
    let other = store
        .task_assign(mission, "ephemeral", "w2", "brief", "default")
        .expect("assign");
    let held = hold(&store, source, MAIN, 600, Some("owner gate"));
    let id = held["hold"].as_i64().expect("an id");
    assert!(refusal(store.task_held(other, id, source)).contains("not waiting for task.held"));
    // Not the active hold, or not the task's source: refused.
    assert!(refusal(store.task_held(task, id + 1, source)).contains("not the active hold"));
    assert!(refusal(store.task_held(task, id, "/tmp/other.git")).contains("not the active hold"));
    let first = store.task_held(task, id, source).expect("held");
    assert!(first.is_some());
    assert_eq!(
        store.task_held(task, id, source).expect("again"),
        None,
        "once per task and hold"
    );
    // As a plain integrator record, the second is refused, not recorded.
    assert!(
        store
            .integrator_record(&json!({"type": "task.held", "id": task, "hold": id,
                                        "source": source}))
            .is_err()
    );
    let mut client = admin(&schema);
    assert_eq!(records(&mut client, "task.held").len(), 1);
    let stages = store.task_stages(task).expect("stages");
    assert_eq!(
        stages
            .as_array()
            .expect("stages")
            .iter()
            .filter_map(|stage| stage["stage"].as_str())
            .collect::<Vec<_>>(),
        ["assigned", "delivered", "reported", "gated", "held"]
    );

    let live = folded(&mut client);
    assert_eq!(live["tasks"][0]["held_by"], id);
    assert_eq!(live["tasks"][0]["state"], "landing");
    assert_eq!(live["holds"][0]["reason"], "owner gate");
    refold(&mut client);
    assert_eq!(
        folded(&mut client),
        live,
        "the refold rebuilds the holds and held_by"
    );

    // Renewed, released: the refold agrees at every end.
    let renewed = hold(&store, source, OTHER_MAIN, 600, None);
    store
        .task_held(task, renewed["hold"].as_i64().expect("id"), source)
        .expect("held behind the renewal");
    let live = folded(&mut client);
    refold(&mut client);
    assert_eq!(folded(&mut client), live);
    store.source_release(source).expect("release");
    let live = folded(&mut client);
    assert_eq!(live["holds"], json!([]));
    refold(&mut client);
    assert_eq!(folded(&mut client), live);
    // After the release the task lands as any other.
    store
        .integrator_record(&json!({"type": "task.landed", "id": task, "from": MAIN, "to": "c1"}))
        .expect("landed");
}
