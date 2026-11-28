//! A halted mission resumed from the page (objective 051): the board names
//! the halt a mission is in (`halt`: its `mission.halted` event, reason,
//! time and, for a halt on the scenario floor, the scenarios gone), folded
//! from the integrator's records; the `spawn_human` login resumes exactly
//! that halt through `spawn_human_resume` alone, once per idempotency key,
//! with `by` from the store's own `spawn_principal()`; anything else is
//! refused with the reason and records nothing.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use postgres::error::SqlState;
use serde_json::{Value, json};
use spawn_core::id::IdempotencyKey;
use spawn_postgres::{HumanResume, PostgresStore, role_url_with_passfile};

mod common;
use common::{admin, schema_url, store};

/// Every `mission.resumed` record, oldest first, with its event id.
fn resumes(client: &mut Client) -> Vec<(i64, Value)> {
    client
        .query(
            "select id, kind -> 'detail' -> 'record' from spawn_events
              where session_id = 'session_control'
                and kind -> 'detail' -> 'record' ->> 'type' = 'mission.resumed' order by id",
            &[],
        )
        .expect("the records")
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect()
}

fn mission_row(client: &mut Client, mission: i64) -> Value {
    client
        .query_one(
            "select to_jsonb(m) from mission_missions m where id = $1",
            &[&mission],
        )
        .expect("the mission")
        .get(0)
}

/// The board's `halt` of `mission`.
fn board_halt(store: &PostgresStore, mission: i64) -> Value {
    let board = store.mission_board(50).expect("the board");
    board["missions"]
        .as_array()
        .and_then(|missions| missions.iter().find(|row| row["id"] == mission))
        .expect("the mission on the board")["halt"]
        .clone()
}

fn key(text: &str) -> IdempotencyKey {
    IdempotencyKey::new(text.to_string()).expect("a key")
}

/// A gated mission with one accepted task, ready for gate records.
fn gated_mission(store: &PostgresStore) -> (i64, i64) {
    let mission = store
        .mission_add("Gated", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    store
        .mission_configure(
            mission,
            Some("/controller/rsi/scenarios"),
            "gate",
            None,
            None,
        )
        .expect("configure");
    let task = store
        .task_assign(mission, "ephemeral", "w", "brief", "default")
        .expect("assign");
    store.task_delivered(task, 1).expect("delivered");
    assert!(store.task_reported(task, "done").expect("reported"));
    (mission, task)
}

fn halt(store: &PostgresStore, mission: i64, reason: &str, gone: Option<&[&str]>) -> i64 {
    let mut record = json!({"type": "mission.halted", "id": mission, "reason": reason});
    if let Some(gone) = gone {
        record["gone"] = json!(gone);
    }
    store.integrator_record(&record).expect("halt")
}

#[test]
fn the_gating_that_sets_the_floor_names_its_scenarios_and_the_board_names_the_halt() {
    let (store, schema) = store("resume_board");
    let mut client = admin(&schema);
    let (mission, task) = gated_mission(&store);
    assert!(board_halt(&store, mission).is_null(), "not halted: no halt");

    let gated = |verdict: &str, scenarios: u32, names: Value| {
        store
            .integrator_record(
                &json!({"type": "task.gated", "id": task, "verdict": verdict,
                                        "revision": 0, "base": "b1", "scenarios": scenarios,
                                        "scenario_names": names}),
            )
            .expect("gated");
    };
    gated("stale", 3, json!(["a.sh", "b.sh", "c.sh"]));
    assert_eq!(
        mission_row(&mut client, mission)["scenario_names"],
        json!(["a.sh", "b.sh", "c.sh"])
    );
    gated("stale", 2, json!(["a.sh", "b.sh"]));
    assert_eq!(
        mission_row(&mut client, mission)["scenario_names"],
        json!(["a.sh", "b.sh", "c.sh"]),
        "under the floor: the floor's names stay"
    );
    gated("stale", 3, json!(["a.sh", "b.sh", "d.sh"]));
    assert_eq!(
        mission_row(&mut client, mission)["scenario_names"],
        json!(["a.sh", "b.sh", "d.sh"]),
        "the latest gating at the floor names it"
    );

    let event = halt(
        &store,
        mission,
        "the scenarios fell from 3 to 2",
        Some(&["d.sh"]),
    );
    let halt_value = board_halt(&store, mission);
    assert_eq!(halt_value["event"], event);
    assert_eq!(halt_value["reason"], "the scenarios fell from 3 to 2");
    assert_eq!(halt_value["gone"], json!(["d.sh"]));
    assert!(halt_value["at_ms"].is_i64());

    // The owner's resume folds the halt and the floor's names away.
    store.mission_resume(mission).expect("resume");
    assert!(board_halt(&store, mission).is_null());
    let row = mission_row(&mut client, mission);
    assert!(row["scenario_names"].is_null());
    assert_eq!(row["scenario_floor"], 0);

    let other = halt(&store, mission, "3 gate rejections in a row", None);
    let halt_value = board_halt(&store, mission);
    assert_eq!(halt_value["event"], other);
    assert!(halt_value["gone"].is_null(), "no list for another halt");

    // The fold is the log's: a refold gives the same halt.
    let before = mission_row(&mut client, mission);
    client
        .batch_execute("select spawn_control_refold()")
        .expect("refold");
    assert_eq!(mission_row(&mut client, mission), before);
}

#[test]
#[allow(clippy::too_many_lines)]
fn the_page_resumes_exactly_the_halt_it_shows_once() {
    let (store, schema) = store("resume_human");
    let mut client = admin(&schema);
    let (mission, _) = gated_mission(&store);
    let page = PostgresStore::connect_with_options(
        &role_url_with_passfile(&schema_url(&schema), "spawn_human", None)
            .expect("the human login's URL"),
        &spawn_postgres::ConnectOptions::default(),
    )
    .expect("the human login connects");

    let refused = |answer: HumanResume, what: &str| match answer {
        HumanResume::Refused(why) => why,
        other => panic!("{what}: refused, not {other:?}"),
    };
    let why = refused(
        page.human_resume(mission, 1, &key("k0")).expect("answered"),
        "not halted",
    );
    assert!(why.contains("not halted"), "{why}");
    assert_eq!(
        page.human_resume(999_999, 1, &key("k0")).expect("answered"),
        HumanResume::NoMission("mission 999999 does not exist".to_string())
    );

    let first = halt(
        &store,
        mission,
        "the scenarios fell from 5 to 2",
        Some(&["c.sh"]),
    );
    let why = refused(
        page.human_resume(mission, first - 1, &key("k0"))
            .expect("answered"),
        "another halt",
    );
    assert!(why.contains(&format!("halted on {first}")), "{why}");
    assert!(resumes(&mut client).is_empty(), "refusals record nothing");

    let HumanResume::Resumed(event) = page
        .human_resume(mission, first, &key("k1"))
        .expect("resumed")
    else {
        panic!("the mission is halted on {first}");
    };
    let records = resumes(&mut client);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].0, event);
    assert_eq!(records[0].1["id"], mission);
    assert_eq!(records[0].1["by"], "human", "the store's principal");
    assert_eq!(records[0].1["halt"], first);
    let row = mission_row(&mut client, mission);
    assert!(row["halted"].is_null(), "folded as the owner's resume");
    assert!(row["halt_event"].is_null());
    assert_eq!(row["scenario_floor"], 0);

    // The same key is the same resume; a new one finds nothing to resume.
    assert_eq!(
        page.human_resume(mission, first, &key("k1"))
            .expect("again"),
        HumanResume::Resumed(event)
    );
    let why = refused(
        page.human_resume(mission, first, &key("k2"))
            .expect("answered"),
        "not halted",
    );
    assert!(why.contains("not halted"), "{why}");
    assert_eq!(resumes(&mut client).len(), 1);

    // Halted again: the old halt is refused, the new one resumes; the old
    // key names the old halt, so it is no resume of the new one.
    let second = halt(&store, mission, "3 gate aborts in a row", None);
    refused(
        page.human_resume(mission, first, &key("k3"))
            .expect("answered"),
        "the first halt",
    );
    let why = refused(
        page.human_resume(mission, second, &key("k1"))
            .expect("answered"),
        "a key used for another resume",
    );
    assert!(why.contains("already used"), "{why}");
    assert!(matches!(
        page.human_resume(mission, second, &key("k4")).expect("resumed"),
        HumanResume::Resumed(next) if next > event
    ));
    assert_eq!(resumes(&mut client).len(), 2);

    // The page's login calls no other mission function; the owner does not
    // call the page's.
    halt(&store, mission, "x", None);
    let mut raw = Client::connect(
        &role_url_with_passfile(&schema_url(&schema), "spawn_human", None)
            .expect("the login's URL"),
        postgres::NoTls,
    )
    .expect("connects");
    let error = raw
        .execute("select spawn_mission_resume($1)", &[&mission])
        .expect_err("not the page's");
    assert_eq!(error.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE));
    let error = client
        .query_one(
            "select spawn_human_resume($1, $2, $3)",
            &[&mission, &second, &"k9"],
        )
        .expect_err("only the page");
    assert_eq!(error.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE));
    assert_eq!(resumes(&mut client).len(), 2);
}
