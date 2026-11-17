//! An objective retired by hand (objective 210, GitHub #65): `spawn
//! objective retire` records `objective.retired` through
//! `spawn_objective_retire`, which sets `caller` from the login and refuses,
//! recording nothing, a malformed record, a mission that does not exist or
//! has no backlog, an objective with a task in flight and one already
//! closed. The fold `mission_retired_objectives` feeds the board's
//! `retired`, the RSI page's and `OBJECTIVE_LANDINGS`' retired rows.
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

fn retire(mission: i64, objective: i64, by: &str, reason: &str) -> Value {
    json!({"type": "objective.retired", "id": mission, "objective": objective,
           "by": by, "reason": reason})
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

/// Each refusal, one at a time, records nothing; an objective of a mission
/// with a backlog, cancelled only or never assigned (not in any backlog
/// file: the store does not read the files), is retired with the owner's
/// caller, and retiring it again is refused.
#[test]
fn a_retirement_is_refused_for_each_reason_and_records_nothing() {
    let (store, schema) = store("objective_retire_refusals");
    let mut client = admin(&schema);
    world(&mut client);

    for (record, why) in [
        (retire(1, 2, "lead", "obsolete"), "has a task in flight"),
        (retire(1, 3, "lead", "obsolete"), "a task of it landed"),
        (retire(1, 4, "lead", "obsolete"), "solved on the base"),
        (retire(1, 6, "  ", "obsolete"), "by is 1 to 80 characters"),
        (retire(1, 6, "", "obsolete"), "by is 1 to 80 characters"),
        (retire(1, 6, &"b".repeat(81), "obsolete"), "by is 1 to 80 characters"),
        (retire(1, 6, "lead", " \t"), "reason is 1 to 500 characters"),
        (retire(1, 6, "lead", ""), "reason is 1 to 500 characters"),
        (retire(999_999, 6, "lead", "obsolete"), "does not exist"),
        (retire(2, 6, "lead", "obsolete"), "has no backlog"),
        (retire(0, 6, "lead", "obsolete"), "whole numbers from 1"),
        (retire(1, 0, "lead", "obsolete"), "whole numbers from 1"),
        (
            json!({"type": "objective.retired", "id": 1, "objective": 6.5, "by": "lead",
                   "reason": "obsolete"}),
            "whole numbers from 1",
        ),
        (
            json!({"type": "objective.retired", "id": "1", "objective": 6, "by": "lead",
                   "reason": "obsolete"}),
            "whole numbers from 1",
        ),
        (
            json!({"type": "objective.retired", "id": 1, "objective": 6, "by": "lead",
                   "reason": "obsolete", "caller": "owner"}),
            "and nothing else",
        ),
        (
            json!({"type": "objective.retired", "id": 1, "objective": 6, "by": "lead"}),
            "and nothing else",
        ),
        (
            json!({"type": "objective.solved", "id": 1, "objective": 6, "by": "lead",
                   "reason": "obsolete"}),
            "not a retirement record",
        ),
    ] {
        refused(&store, &mut client, &record, &why);
    }
    assert_eq!((recorded(&mut client), rows(&mut client)), (0, 0));

    // Cancelled only (5), and never assigned (6): retired.
    let event = store
        .objective_retire(&retire(1, 5, "lead", "its work is already on main"))
        .expect("retired");
    store
        .objective_retire(&retire(1, 6, "lead", "renumbered away"))
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
        json!({"type": "objective.retired", "id": 1, "objective": 5, "by": "lead",
               "reason": "its work is already on main", "caller": "owner"})
    );
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
        // Closed to the login (the reader's), or refused by the function
        // (the human's, which may execute it as public may not): both 42501.
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
        [(5, "spawn_supervisor".to_string()), (6, "owner".to_string())]
    );
}

/// The fold is the board's `retired` (oldest first, `[]` when none, read by
/// the supervisor's login), the RSI page's, and `OBJECTIVE_LANDINGS`'
/// retired rows; a refold rebuilds it the same, the first record winning.
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
                row["retired"].as_bool().expect("retired"),
                row["metric"].is_null(),
            )
        })
        .collect();
    closed.sort_unstable();
    assert_eq!(
        closed,
        [(3, false, true), (4, false, true), (5, true, true), (6, true, true)],
        "landed 3, solved 4, retired 5 and 6"
    );

    client
        .batch_execute("select spawn_control_refold()")
        .expect("refold");
    assert_eq!(read(&store), (one, two), "a refold rebuilds it");
}
