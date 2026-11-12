//! The owner pauses a mission and resumes it (objective 099), through the
//! real store: `spawn_mission_pause` records `mission.paused` {id, reason}
//! for the owner alone (an agent's login is refused, nothing recorded), never
//! for a closed or an already paused mission; the fold keeps the pause (the
//! board's `paused`, the state as it was); `spawn_objective_assign` refuses a
//! paused mission, naming the pause; `spawn_mission_resume` on a paused
//! mission records `mission.unpaused` {id, pause_event} and nothing else, so
//! a mission both paused and halted keeps its halt for a second resume.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use postgres::error::SqlState;
use serde_json::{Value, json};
use spawn_postgres::PostgresStore;

mod common;
use common::{admin, as_login, store};

/// Every control record of `kind`, oldest first, with its event id.
fn records(client: &mut Client, kind: &str) -> Vec<(i64, Value)> {
    client
        .query(
            "select id, kind -> 'detail' -> 'record' from spawn_events
              where session_id = 'session_control' and event_name = 'control.recorded'
                and kind -> 'detail' -> 'record' ->> 'type' = $1 order by id",
            &[&kind],
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

/// The board's row of `mission`.
fn board_row(store: &PostgresStore, mission: i64) -> Value {
    let board = store.mission_board(50).expect("the board");
    board["missions"]
        .as_array()
        .and_then(|missions| missions.iter().find(|row| row["id"] == mission))
        .expect("the mission on the board")
        .clone()
}

fn refusal<T: std::fmt::Debug>(result: Result<T, spawn_core::port::StoreError>) -> String {
    result.expect_err("refused").to_string()
}

/// An objective mission worked by the approved resident `name`.
fn objective_mission(store: &PostgresStore, name: &str) -> i64 {
    let mission = store
        .mission_add("Supply PA: an agent for every provider", &json!(["/tmp/source.git"]), "human")
        .expect("add");
    store
        .resident_propose(name, "Works the backlog")
        .expect("propose");
    store.resident_approve(name).expect("approve");
    let objectives = json!({"backlog": "/controller/rsi/backlog", "resident": name});
    store
        .mission_configure(mission, Some("/scenarios"), "gate", None, Some(&objectives))
        .expect("configure");
    mission
}

#[test]
fn the_owner_pauses_and_resumes_and_nothing_is_assigned_while_paused() {
    let (store, schema) = store("pause_owner");
    let mut client = admin(&schema);
    let mission = objective_mission(&store, "supply");
    let first = store
        .objective_assign(mission, 1, Some("/scenarios/001.sh"), "Objective 1")
        .expect("assign");
    store.task_cancel(first).expect("cancel");

    // An agent's login (the supervisor's) may not pause: nothing recorded.
    let mut agent = as_login(&schema, "spawn_supervisor");
    let error = agent
        .query_one("select spawn_mission_pause($1, $2)", &[&mission, &"mine"])
        .expect_err("an agent may not pause");
    assert_eq!(error.code(), Some(&SqlState::INSUFFICIENT_PRIVILEGE));
    assert!(records(&mut client, "mission.paused").is_empty());
    assert!(mission_row(&mut client, mission)["pause_event"].is_null());

    let event = store
        .mission_pause(mission, Some("owner hold 2026-11-12"))
        .expect("paused");
    let paused = records(&mut client, "mission.paused");
    assert_eq!(
        paused,
        vec![(
            event,
            json!({"type": "mission.paused", "id": mission, "reason": "owner hold 2026-11-12"})
        )]
    );
    let row = mission_row(&mut client, mission);
    assert_eq!(row["pause_event"], event);
    assert_eq!(row["pause_reason"], "owner hold 2026-11-12");
    assert!(row["pause_at_ms"].is_i64());
    let board = board_row(&store, mission);
    assert_eq!(board["state"], "active", "a pause keeps the state");
    assert_eq!(board["paused"]["event_id"], event);
    assert_eq!(board["paused"]["reason"], "owner hold 2026-11-12");
    assert_eq!(board["paused"]["at_ms"], row["pause_at_ms"]);

    // A second pause is refused, and records nothing.
    let again = refusal(store.mission_pause(mission, Some("again")));
    assert!(again.contains("already paused"), "{again}");
    assert_eq!(records(&mut client, "mission.paused").len(), 1);

    // No objective is assigned while it is paused, and the refusal says so.
    let refused = refusal(store.objective_assign(mission, 2, None, "Objective 2"));
    assert!(refused.contains("paused"), "{refused}");
    let tasks: i64 = client
        .query_one(
            "select count(*) from mission_tasks where mission_id = $1",
            &[&mission],
        )
        .expect("count")
        .get(0);
    assert_eq!(tasks, 1, "no task made");

    // The dispatcher reads the pause with the resident it holds.
    let pauses = store.mission_pauses().expect("the pauses");
    assert_eq!(pauses[0]["id"], mission);
    assert_eq!(pauses[0]["pause_event"], event);
    assert_eq!(
        pauses[0]["sessions"][0]["session_id"],
        "session_resident_supply"
    );
    assert_eq!(pauses[0]["sessions"][0]["final"], false);
    assert_eq!(pauses[0]["sessions"][0]["resumed"], false);

    // The resume unpauses it, naming the pause, and records nothing else.
    store.mission_resume(mission).expect("resumed");
    let unpaused = records(&mut client, "mission.unpaused");
    assert_eq!(unpaused.len(), 1);
    assert_eq!(
        unpaused[0].1,
        json!({"type": "mission.unpaused", "id": mission, "pause_event": event})
    );
    assert!(records(&mut client, "mission.resumed").is_empty());
    assert!(board_row(&store, mission)["paused"].is_null());
    let pauses = store.mission_pauses().expect("the pauses");
    assert!(pauses[0]["pause_event"].is_null());
    assert_eq!(pauses[0]["unpause_event"], unpaused[0].0);

    // A mission neither paused nor halted is not resumed.
    let neither = refusal(store.mission_resume(mission));
    assert!(neither.contains("neither halted nor paused"), "{neither}");
    assert_eq!(records(&mut client, "mission.unpaused").len(), 1);

    // Work goes on.
    store
        .objective_assign(mission, 2, None, "Objective 2")
        .expect("assigned again");

    // A closed mission is not paused; nor is one that does not exist.
    store.mission_close(mission, "done", "held no more").expect("close");
    let closed = refusal(store.mission_pause(mission, None));
    assert!(closed.contains("closed"), "{closed}");
    assert!(refusal(store.mission_pause(9_999, None)).contains("does not exist"));
    assert_eq!(records(&mut client, "mission.paused").len(), 1);
}

#[test]
fn a_mission_both_paused_and_halted_is_resumed_twice_the_pause_first() {
    let (store, schema) = store("pause_halted");
    let mut client = admin(&schema);
    let mission = objective_mission(&store, "core");
    let halt = store
        .integrator_record(&json!({"type": "mission.halted", "id": mission, "reason": "three rejections"}))
        .expect("halted");

    // A halted mission may be paused: its halt stays.
    let pause = store.mission_pause(mission, None).expect("paused");
    let row = mission_row(&mut client, mission);
    assert_eq!(row["halted"], "three rejections");
    assert_eq!(row["halt_event"], halt);
    assert_eq!(row["pause_event"], pause);
    assert!(row["pause_reason"].is_null(), "no reason given");
    assert_eq!(
        records(&mut client, "mission.paused")[0].1,
        json!({"type": "mission.paused", "id": mission, "reason": null})
    );

    // The first resume clears the pause alone.
    store.mission_resume(mission).expect("unpaused");
    let row = mission_row(&mut client, mission);
    assert!(row["pause_event"].is_null());
    assert_eq!(row["halted"], "three rejections", "the halt stays");
    assert_eq!(row["halt_event"], halt);
    assert!(records(&mut client, "mission.resumed").is_empty());
    assert_eq!(
        records(&mut client, "mission.unpaused")[0].1["pause_event"],
        pause
    );

    // The second clears the halt, as a resume always did.
    store.mission_resume(mission).expect("resumed");
    let row = mission_row(&mut client, mission);
    assert!(row["halted"].is_null());
    assert_eq!(records(&mut client, "mission.resumed").len(), 1);
    assert_eq!(records(&mut client, "mission.unpaused").len(), 1);
}

#[test]
fn the_fold_of_a_pause_replays_and_the_window_counts_its_time() {
    let (store, schema) = store("pause_refold");
    let mut client = admin(&schema);
    let mission = objective_mission(&store, "rsi");
    let pause = store.mission_pause(mission, Some("hold")).expect("paused");
    let before = mission_row(&mut client, mission);
    client
        .batch_execute("select spawn_control_refold()")
        .expect("refold");
    assert_eq!(mission_row(&mut client, mission), before, "the log gives the same row");

    // The window names the pause, open while it lasts.
    let now = store.now_ms().expect("now");
    let window = store
        .experience_window(now - 86_400_000, now + 1)
        .expect("the window");
    let row = window["missions"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["mission"] == mission))
        .expect("the mission's row")
        .clone();
    let paused_at = row["pauses"][0][0].as_i64().expect("paused at");
    assert!(row["pauses"][0][1].is_null(), "still paused: {row}");
    store.mission_resume(mission).expect("unpaused");
    let now = store.now_ms().expect("now") + 1;
    let window = store
        .experience_window(now - 86_400_000, now)
        .expect("the window");
    let row = window["missions"]
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["mission"] == mission))
        .expect("the mission's row")
        .clone();
    assert_eq!(row["pauses"][0][0], paused_at);
    assert!(row["pauses"][0][1].as_i64().expect("unpaused at") >= paused_at);
    let _ = pause;
}
