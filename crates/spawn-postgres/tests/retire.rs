//! An objective retired by hand (objective 210, GitHub #65): `spawn
//! objective retire` records `objective.retired` through
//! `spawn_objective_retire`, which sets `caller` from the login and refuses,
//! recording nothing, a malformed record, a mission that does not exist or
//! has no backlog, an objective with a task in flight and one already
//! closed. The fold `mission_retired_objectives` feeds the board's
//! `retired`, the RSI page's and `OBJECTIVE_LANDINGS`' retired rows.
//!
//! Every retirement names its kind (objective 223): on-main, renumbered or
//! obsolete. on-main is recorded only when an accepted gate of a landed
//! task of the mission saw the objective's scenario pass, with no gate since
//! finding it red on main.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use serde_json::{Value, json};
use spawn_postgres::{PostgresStore, role_url};

mod common;
use common::{admin, schema_url, store};

/// A control record as the integrator writes it.
fn control(client: &mut Client, record: &Value) -> i64 {
    client
        .query_one("select spawn_control_record($1)", &[record])
        .expect("a control record")
        .get(0)
}

/// Mission `id`; an objective mission (a backlog) when `backlog`.
fn mission(client: &mut Client, id: i64, backlog: bool) {
    control(
        client,
        &json!({"type": "mission.added", "id": id, "text": format!("mission {id}"),
                "origin": "human", "sources": []}),
    );
    let objectives = backlog.then(|| json!({"backlog": "/backlog", "resident": "r"}));
    control(
        client,
        &json!({"type": "mission.configured", "id": id, "gate": "/gate", "accept": "gate",
                "objectives": objectives}),
    );
}

fn assigned(client: &mut Client, task: i64, mission: i64, objective: i64) {
    control(
        client,
        &json!({"type": "task.assigned", "id": task, "mission_id": mission,
                "kind": "ephemeral", "created_by": "resident",
                "agent_id": format!("agent_worker_{task}"),
                "session_id": format!("session_worker_{task}"),
                "objective": objective, "brief": format!("objective {objective}")}),
    );
}

/// A retirement as obsolete, which needs no record (objective 223).
fn retire(mission: i64, objective: i64, by: &str, reason: &str) -> Value {
    retire_as("obsolete", mission, objective, by, reason)
}

/// A retirement of `kind` (objective 223).
fn retire_as(kind: &str, mission: i64, objective: i64, by: &str, reason: &str) -> Value {
    json!({"type": "objective.retired", "id": mission, "objective": objective,
           "kind": kind, "by": by, "reason": reason})
}

fn recorded(client: &mut Client) -> i64 {
    client
        .query_one(
            "select count(*) from spawn_events
              where session_id = 'session_control'
                and kind -> 'detail' -> 'record' ->> 'type' = 'objective.retired'",
            &[],
        )
        .expect("count")
        .get(0)
}

fn rows(client: &mut Client) -> i64 {
    client
        .query_one("select count(*) from mission_retired_objectives", &[])
        .expect("rows")
        .get(0)
}

/// Whether `record` is refused with a message containing `why`, recording
/// nothing.
fn refused(store: &PostgresStore, client: &mut Client, record: &Value, why: &str) {
    let before = (recorded(client), rows(client));
    let error = store
        .objective_retire(record)
        .expect_err(&format!("refused: {record}"))
        .to_string();
    assert!(error.contains(why), "{record}: {error}");
    assert_eq!((recorded(client), rows(client)), before, "{record}");
}

/// The world: mission 1 an objective mission with objective 2 in flight
/// (task 1), objective 3 landed (task 2), objective 4 solved on the base
/// (task 3, cancelled) and objective 5 cancelled only; mission 2 with no
/// backlog.
fn world(client: &mut Client) {
    mission(client, 1, true);
    mission(client, 2, false);
    assigned(client, 1, 1, 2);
    assigned(client, 2, 1, 3);
    control(
        client,
        &json!({"type": "task.landed", "id": 2, "from": "a".repeat(40), "to": "b".repeat(40)}),
    );
    assigned(client, 3, 1, 4);
    control(
        client,
        &json!({"type": "objective.solved", "id": 1, "objective": 4, "task": 3,
                "base": "c".repeat(40)}),
    );
    control(client, &json!({"type": "task.cancelled", "id": 3}));
    assigned(client, 4, 1, 5);
    control(client, &json!({"type": "task.cancelled", "id": 4}));
}

/// The refusal of an on-main retire no gate shows (objective 223).
fn no_record(mission: i64, objective: i64, scenario: &str) -> String {
    format!(
        "objective {mission}/{objective} is not shown on main: no accepted gate of a landed task \
         of mission {mission} lists {scenario} as passed (a landing's title is not a gate \
         record); run its scenario through a gate, or retire it as renumbered or obsolete"
    )
}

/// The refusal of an on-main retire whose scenario a gate found red on
/// main since its pass (objective 223).
fn red_since(mission: i64, objective: i64, scenario: &str, task: i64, base: &str) -> String {
    format!(
        "objective {mission}/{objective} is not shown on main: {scenario} is red on main since \
         task #{task}'s gate ({base}); run its scenario through a gate, or retire it as \
         renumbered or obsolete"
    )
}

/// Each refusal of [`world`] with the text it holds.
fn refusals() -> Vec<(Value, String)> {
    let none_shown = no_record(1, 6, "006.sh");
    [
        (retire(1, 2, "lead", "obsolete"), "has a task in flight"),
        (retire(1, 3, "lead", "obsolete"), "a task of it landed"),
        (retire(1, 4, "lead", "obsolete"), "solved on the base"),
        (retire(1, 6, "  ", "obsolete"), "by is 1 to 80 characters"),
        (retire(1, 6, "", "obsolete"), "by is 1 to 80 characters"),
        (
            retire(1, 6, &"b".repeat(81), "obsolete"),
            "by is 1 to 80 characters",
        ),
        (retire(1, 6, "lead", " \t"), "reason is 1 to 500 characters"),
        (retire(1, 6, "lead", ""), "reason is 1 to 500 characters"),
        (retire(999_999, 6, "lead", "obsolete"), "does not exist"),
        (retire(2, 6, "lead", "obsolete"), "has no backlog"),
        (retire(0, 6, "lead", "obsolete"), "whole numbers from 1"),
        (retire(1, 0, "lead", "obsolete"), "whole numbers from 1"),
        (
            json!({"type": "objective.retired", "id": 1, "objective": 6.5, "kind": "obsolete",
                   "by": "lead", "reason": "obsolete"}),
            "whole numbers from 1",
        ),
        (
            json!({"type": "objective.retired", "id": "1", "objective": 6, "kind": "obsolete",
                   "by": "lead", "reason": "obsolete"}),
            "whole numbers from 1",
        ),
        (
            json!({"type": "objective.retired", "id": 1, "objective": 6, "kind": "obsolete",
                   "by": "lead", "reason": "obsolete", "caller": "owner"}),
            "and nothing else",
        ),
        (
            json!({"type": "objective.retired", "id": 1, "objective": 6, "kind": "obsolete",
                   "by": "lead"}),
            "and nothing else",
        ),
        (
            json!({"type": "objective.solved", "id": 1, "objective": 6, "kind": "obsolete",
                   "by": "lead", "reason": "obsolete"}),
            "not a retirement record",
        ),
        // Objective 223: the kind is required, and one of three; on-main
        // needs a gate's record.
        (
            json!({"type": "objective.retired", "id": 1, "objective": 6, "by": "lead",
                   "reason": "obsolete"}),
            "a retirement record has type, id, objective, kind, by and reason, and nothing else",
        ),
        (
            retire_as("landed", 1, 6, "lead", "obsolete"),
            "a retirement record's kind is on-main, renumbered or obsolete, not landed",
        ),
        (
            json!({"type": "objective.retired", "id": 1, "objective": 6, "kind": 3,
                   "by": "lead", "reason": "obsolete"}),
            "a retirement record's kind is on-main, renumbered or obsolete, not 3",
        ),
        (
            retire_as("on-main", 1, 6, "lead", "its work is on main"),
            none_shown.as_str(),
        ),
        // The refusals before it keep their texts with on-main.
        (
            retire_as("on-main", 1, 2, "lead", "on main"),
            "has a task in flight",
        ),
        (
            retire_as("on-main", 1, 3, "lead", "on main"),
            "a task of it landed",
        ),
    ]
    .into_iter()
    .map(|(record, why)| (record, why.to_string()))
    .collect()
}

/// Each refusal, one at a time, records nothing; an objective of a mission
/// with a backlog, cancelled only or never assigned (not in any backlog
/// file: the store does not read the files), is retired with the owner's
/// caller, and retiring it again is refused.
#[test]
fn a_retirement_is_refused_for_each_reason_and_records_nothing() {
    let (store, schema) = store("objective_retire_refusals");
    let mut client = admin(&schema);
    world(&mut client);

    for (record, why) in refusals() {
        refused(&store, &mut client, &record, &why);
    }
    assert_eq!((recorded(&mut client), rows(&mut client)), (0, 0));

    // Cancelled only (5), and never assigned (6): retired.
    let event = store
        .objective_retire(&retire(1, 5, "lead", "its work is already on main"))
        .expect("retired");
    let renumbered = store
        .objective_retire(&retire_as("renumbered", 1, 6, "lead", "renumbered away"))
        .expect("retired");
    let record: Value = client
        .query_one(
            "select kind -> 'detail' -> 'record' from spawn_events where id = $1",
            &[&event],
        )
        .expect("the record")
        .get(0);
    assert_eq!(
        record,
        json!({"type": "objective.retired", "id": 1, "objective": 5, "kind": "obsolete",
               "by": "lead", "reason": "its work is already on main", "caller": "owner"})
    );
    let record: Value = client
        .query_one(
            "select kind -> 'detail' -> 'record' from spawn_events where id = $1",
            &[&renumbered],
        )
        .expect("the record")
        .get(0);
    assert_eq!(record["kind"], "renumbered", "{record}");
    assert_eq!((recorded(&mut client), rows(&mut client)), (2, 2));
    refused(
        &store,
        &mut client,
        &retire(1, 5, "lead", "again"),
        "already retired",
    );
}

/// `caller` is the login's: `owner` for the owner, `spawn_supervisor` for
/// the supervisor's; the reader's and the human's logins are refused
/// (42501: the function is closed to them) and record nothing.
#[test]
fn the_caller_is_the_logins_and_only_the_owner_and_supervisor_may_retire() {
    let (store, schema) = store("objective_retire_callers");
    let mut client = admin(&schema);
    world(&mut client);
    let login = |role: &str| {
        PostgresStore::connect(&role_url(&schema_url(&schema), role).expect("the login's URL"))
            .expect("log in")
    };
    for role in ["spawn_reader", "spawn_human"] {
        let error = login(role)
            .objective_retire(&retire(1, 6, "lead", "obsolete"))
            .expect_err(role)
            .to_string();
        // Closed to the login, or refused by the function's caller check:
        // 42501 either way.
        assert!(
            error.contains("may not retire objectives") || error.contains("permission denied"),
            "{role}: {error}"
        );
        assert_eq!((recorded(&mut client), rows(&mut client)), (0, 0), "{role}");
    }
    store
        .objective_retire(&retire(1, 6, "lead", "renumbered"))
        .expect("the owner");
    login("spawn_supervisor")
        .objective_retire(&retire(1, 5, "supervisor", "parked"))
        .expect("the supervisor");
    let callers: Vec<(i32, String)> = client
        .query(
            "select objective, caller from mission_retired_objectives order by objective",
            &[],
        )
        .expect("rows")
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    assert_eq!(
        callers,
        [
            (5, "spawn_supervisor".to_string()),
            (6, "owner".to_string())
        ]
    );
}

/// The fold is the board's `retired` (oldest first, `[]` when none, read by
/// the supervisor's login), the RSI page's, and `OBJECTIVE_LANDINGS`'
/// retired rows (`retired: true`, on no other row); a refold rebuilds it the same, the first record winning.
#[test]
fn a_retired_objective_is_on_the_board_and_closes_as_a_landing() {
    let (store, schema) = store("objective_retire_board");
    let mut client = admin(&schema);
    world(&mut client);
    store
        .objective_retire(&retire(1, 6, "lead", "renumbered"))
        .expect("retired");
    store
        .objective_retire(&retire(1, 5, "lead", "obsolete"))
        .expect("retired");
    // A second record of 6 straight into the log (no function refuses it):
    // the first one wins, in the fold and in a refold.
    control(
        &mut client,
        &json!({"type": "objective.retired", "id": 1, "objective": 6, "by": "other",
                "reason": "later", "caller": "owner"}),
    );

    let read = |store: &PostgresStore| {
        let board = store.mission_board(10).expect("the board");
        let missions = board["missions"].as_array().expect("missions").clone();
        let of = |id: i64| {
            missions
                .iter()
                .find(|mission| mission["id"] == json!(id))
                .map(|mission| mission["retired"].clone())
                .expect("the mission")
        };
        (of(1), of(2))
    };
    let (one, two) = read(&store);
    let shape: Vec<(i64, String, String, String, bool)> = one
        .as_array()
        .expect("a list")
        .iter()
        .map(|row| {
            (
                row["objective"].as_i64().expect("objective"),
                row["by"].as_str().expect("by").to_string(),
                row["reason"].as_str().expect("reason").to_string(),
                row["caller"].as_str().expect("caller").to_string(),
                row["at_ms"].is_i64(),
            )
        })
        .collect();
    assert_eq!(
        shape,
        [
            (6, "lead".into(), "renumbered".into(), "owner".into(), true),
            (5, "lead".into(), "obsolete".into(), "owner".into(), true),
        ],
        "oldest first, the first record of 6 wins"
    );
    assert_eq!(two, json!([]), "none");
    let supervisor = PostgresStore::connect(
        &role_url(&schema_url(&schema), "spawn_supervisor").expect("the login's URL"),
    )
    .expect("log in");
    assert_eq!(read(&supervisor), (one.clone(), two.clone()));

    let rsi = store.objective_missions().expect("the RSI read");
    let mission = rsi
        .as_array()
        .and_then(|missions| missions.iter().find(|mission| mission["id"] == json!(1)))
        .expect("mission 1");
    assert_eq!(mission["retired"], one);

    let landings = store.objective_landings(1).expect("landings");
    let mut closed: Vec<(i64, bool, bool)> = landings
        .as_array()
        .expect("a list")
        .iter()
        .map(|row| {
            (
                row["objective"].as_i64().expect("objective"),
                row["retired"] == json!(true),
                row["metric"].is_null(),
            )
        })
        .collect();
    closed.sort_unstable();
    assert!(
        landings
            .as_array()
            .expect("a list")
            .iter()
            .all(|row| row.get("retired").is_none() || row["retired"] == json!(true)),
        "only a retired row carries retired: {landings}"
    );
    assert_eq!(
        closed,
        [
            (3, false, true),
            (4, false, true),
            (5, true, true),
            (6, true, true)
        ],
        "landed 3, solved 4, retired 5 and 6"
    );

    client
        .batch_execute("select spawn_control_refold()")
        .expect("refold");
    assert_eq!(read(&store), (one, two), "a refold rebuilds it");
}

/// A gate's verdict on `task`, as the integrator records it: `extra`
/// carries `passed`, `reused` and `main_red_scenarios`.
fn gated(client: &mut Client, task: i64, verdict: &str, base: &str, extra: &Value) {
    let mut record = json!({"type": "task.gated", "id": task, "verdict": verdict,
                            "base": base, "scenarios": 3,
                            "scenario_names": ["001.sh", "002.sh", "003.sh"]});
    if let (Some(record), Some(extra)) = (record.as_object_mut(), extra.as_object()) {
        record.extend(extra.clone());
    }
    control(client, &record);
}

fn landed(client: &mut Client, task: i64) {
    control(
        client,
        &json!({"type": "task.landed", "id": task, "from": "a".repeat(40), "to": "b".repeat(40)}),
    );
}

/// Whether `objective` of mission 1 retires as on-main.
fn on_main(store: &PostgresStore, objective: i64) -> Result<i64, String> {
    store
        .objective_retire(&retire_as("on-main", 1, objective, "lead", "on main"))
        .map_err(|error| error.to_string())
}

/// Objective 223: on-main is recorded only when an accepted gate of a task
/// of the same mission, that task landed, lists the objective's scenario
/// as passed or reused, with no gate of the mission since naming it red on
/// main. Mission 1's tasks are of objectives from 101, mission 3's of 1.
#[test]
fn on_main_needs_a_landed_gate_that_saw_the_scenario_pass() {
    let (store, schema) = store("objective_retire_on_main");
    let mut client = admin(&schema);
    world(&mut client);
    mission(&mut client, 3, true);
    shows_nothing(&store, &mut client);
    shows_it(&store, &mut client);
    red_since_the_pass(&store, &mut client);

    // renumbered and obsolete need no record.
    for (kind, objective) in [("renumbered", 20), ("obsolete", 21)] {
        store
            .objective_retire(&retire_as(kind, 1, objective, "lead", "no record"))
            .expect(kind);
    }
    assert_eq!(rows(&mut client), 8);
}

/// A base commit of `c`s.
fn base(c: char) -> String {
    c.to_string().repeat(40)
}

/// Mission 1's gates that show nothing for 009.sh.
fn shows_nothing(store: &PostgresStore, client: &mut Client) {
    // Shows nothing: a rejected gate's pass on a task that then landed
    // through another gate (10); an accepted gate of a task that never
    // landed (11); another mission's landed gate (12); names that only
    // look like 009.sh, and 009.sh among the gate's scenario_names (13).
    assigned(client, 10, 1, 101);
    gated(
        client,
        10,
        "rejected",
        &base('1'),
        &json!({"passed": ["009.sh"]}),
    );
    gated(
        client,
        10,
        "accepted",
        &base('1'),
        &json!({"passed": ["001.sh"]}),
    );
    landed(client, 10);
    assigned(client, 11, 1, 102);
    gated(
        client,
        11,
        "accepted",
        &base('1'),
        &json!({"passed": ["009.sh"]}),
    );
    assigned(client, 12, 3, 1);
    gated(
        client,
        12,
        "accepted",
        &base('1'),
        &json!({"passed": ["009.sh"]}),
    );
    landed(client, 12);
    assigned(client, 13, 1, 103);
    gated(
        client,
        13,
        "accepted",
        &base('1'),
        &json!({"passed": ["1009.sh", "9.sh", "0009.sh"], "scenario_names": ["009.sh"]}),
    );
    landed(client, 13);
    let before = (recorded(client), rows(client));
    let error = on_main(store, 9).expect_err("shows nothing");
    assert!(error.contains(&no_record(1, 9, "009.sh")), "{error}");
    assert_eq!((recorded(client), rows(client)), before);
}

/// Gates that show it: passed and reused, 1234.sh, a red before the pass.
fn shows_it(store: &PostgresStore, client: &mut Client) {
    // Shows it: a red before the pass (15 on 011.sh), then 14's pass of
    // 009.sh, 1234.sh, 011.sh, 015.sh and 017.sh and reuse of 010.sh.
    assigned(client, 15, 1, 105);
    gated(
        client,
        15,
        "rejected",
        &base('2'),
        &json!({"main_red_scenarios": ["011.sh"]}),
    );
    assigned(client, 14, 1, 104);
    gated(
        client,
        14,
        "accepted",
        &base('3'),
        &json!({"passed": ["009.sh", "1234.sh", "011.sh", "015.sh", "017.sh"],
                "reused": ["010.sh"]}),
    );
    assert!(
        on_main(store, 10)
            .expect_err("not landed yet")
            .contains(&no_record(1, 10, "010.sh")),
        "an accepted gate of a task not landed"
    );
    landed(client, 14);
    for objective in [9, 1234, 10, 11] {
        on_main(store, objective).unwrap_or_else(|error| panic!("{objective}: {error}"));
    }
    let kinds: Vec<(i32, String)> = client
        .query(
            "select (kind -> 'detail' -> 'record' ->> 'objective')::integer,
                    kind -> 'detail' -> 'record' ->> 'kind'
               from spawn_events
              where session_id = 'session_control'
                and kind -> 'detail' -> 'record' ->> 'type' = 'objective.retired'
              order by session_seq",
            &[],
        )
        .expect("records")
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    assert_eq!(
        kinds,
        [9, 1234, 10, 11].map(|objective| (objective, "on-main".to_string()))
    );
}

/// A red after the pass, and a pass again after the red.
fn red_since_the_pass(store: &PostgresStore, client: &mut Client) {
    // A red after the pass (16 on 015.sh and 017.sh): the second text,
    // naming the first red gate after it and its base.
    assigned(client, 16, 1, 106);
    gated(
        client,
        16,
        "rejected",
        &base('4'),
        &json!({"main_red_scenarios": ["015.sh", "017.sh"]}),
    );
    assigned(client, 18, 1, 108);
    gated(
        client,
        18,
        "rejected",
        &base('5'),
        &json!({"main_red_scenarios": ["017.sh"]}),
    );
    for objective in [15, 17] {
        let scenario = format!("0{objective}.sh");
        let error = on_main(store, objective).expect_err("red since");
        assert!(
            error.contains(&red_since(1, objective, &scenario, 16, &base('4'))),
            "{error}"
        );
    }
    // (R3) a pass, a red, then a pass again: shown. (R4) one gate naming
    // 016.sh both passed and red on main: its red is not after its pass.
    assigned(client, 17, 1, 107);
    gated(
        client,
        17,
        "accepted",
        &base('6'),
        &json!({"passed": ["015.sh", "016.sh"], "main_red_scenarios": ["016.sh"]}),
    );
    landed(client, 17);
    on_main(store, 15).expect("passed again");
    on_main(store, 16).expect("repaired in one gate");
    let error = on_main(store, 17).expect_err("still red");
    assert!(
        error.contains(&red_since(1, 17, "017.sh", 16, &base('4'))),
        "{error}"
    );
}

/// Objective 223 (R2): an on-main retire reads the gates before the append
/// lock and only what was recorded since under it. While another
/// connection holds the lock, a retire waits after its first read; what
/// that connection records meanwhile counts.
#[test]
fn on_main_reads_the_gates_recorded_while_it_waits_for_the_lock() {
    let (_store, schema) = store("objective_retire_waits");
    let mut client = admin(&schema);
    world(&mut client);
    mission(&mut client, 3, true);
    let base = "e".repeat(40);
    // 20: accepted, passed 030.sh, not landed yet. 21: accepted, passed
    // 031.sh, landed. 22 and 23 have no gate yet.
    assigned(&mut client, 20, 1, 120);
    gated(
        &mut client,
        20,
        "accepted",
        &base,
        &json!({"passed": ["030.sh"]}),
    );
    assigned(&mut client, 21, 1, 121);
    gated(
        &mut client,
        21,
        "accepted",
        &base,
        &json!({"passed": ["031.sh"]}),
    );
    landed(&mut client, 21);
    assigned(&mut client, 22, 1, 122);
    assigned(&mut client, 23, 3, 1);
    assigned(&mut client, 24, 1, 124);

    // While the lock is held: `meanwhile` records what it records, after
    // the retire of `objective` waits on the lock.
    let waits = |objective: i64, meanwhile: &dyn Fn(&mut postgres::Transaction)| {
        let mut holder = admin(&schema);
        let mut hold = holder.transaction().expect("a transaction");
        hold.execute("select pg_advisory_xact_lock(1399957335)", &[])
            .expect("the append lock");
        let mut retirer = admin(&schema);
        let pid: i32 = retirer
            .query_one("select pg_backend_pid()", &[])
            .expect("its pid")
            .get(0);
        let record = retire_as("on-main", 1, objective, "lead", "on main");
        let retiring = std::thread::spawn(move || {
            retirer
                .query_one("select spawn_objective_retire($1)", &[&record])
                .map(|row| row.get::<_, i64>(0))
                .map_err(|error| {
                    error
                        .as_db_error()
                        .map_or_else(|| error.to_string(), |db| db.message().to_string())
                })
        });
        let mut watcher = admin(&schema);
        let started = std::time::Instant::now();
        while !watcher
            .query_one(
                "select exists (select 1 from pg_locks
                                 where pid = $1 and locktype = 'advisory' and not granted)",
                &[&pid],
            )
            .expect("its locks")
            .get::<_, bool>(0)
        {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(30),
                "the retire of {objective} never waited on the lock"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        meanwhile(&mut hold);
        hold.commit().expect("commit");
        retiring.join().expect("the retire's thread")
    };
    let record = |hold: &mut postgres::Transaction, record: Value| {
        hold.query_one("select spawn_control_record($1)", &[&record])
            .expect("a control record");
    };
    let land = |task: i64| json!({"type": "task.landed", "id": task, "from": "a".repeat(40), "to": "b".repeat(40)});
    let gate = |task: i64, verdict: &str, extra: Value| {
        let mut gate = json!({"type": "task.gated", "id": task, "verdict": verdict,
                              "base": "f".repeat(40)});
        if let (Some(gate), Some(extra)) = (gate.as_object_mut(), extra.as_object()) {
            gate.extend(extra.clone());
        }
        gate
    };

    // The landing of a task whose gate the retire had read.
    waits(30, &|hold| record(hold, land(20))).expect("the landing counts");
    // A red on main since.
    let error = waits(31, &|hold| {
        record(
            hold,
            gate(22, "rejected", json!({"main_red_scenarios": ["031.sh"]})),
        );
    })
    .expect_err("the red counts");
    assert!(
        error.contains(&red_since(1, 31, "031.sh", 22, &"f".repeat(40))),
        "{error}"
    );
    // (R4) a passing gate of a task that lands, the first record after the
    // retire's read.
    waits(32, &|hold| {
        record(hold, gate(24, "accepted", json!({"passed": ["032.sh"]})));
        record(hold, land(24));
    })
    .expect("the gate and the landing count");
    // Another mission's landed gate shows nothing.
    let error = waits(33, &|hold| {
        record(hold, gate(23, "accepted", json!({"passed": ["033.sh"]})));
        record(hold, land(23));
    })
    .expect_err("another mission's");
    assert!(error.contains(&no_record(1, 33, "033.sh")), "{error}");
    assert_eq!(rows(&mut client), 2);
}
