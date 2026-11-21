//! The feature map's own reads (crates/spawn-postgres/src/features.rs): the
//! newest deploy records (the only evidence a capability is live) and every
//! open mission's blocker (a halt, the owner's pause, a red main). Also the
//! mission setting that names the mapping file, which is validated but
//! never stores a status.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use serde_json::{Value, json};
use spawn_postgres::PostgresStore;

mod common;
use common::{admin, schema_url, store};

fn mission(client: &mut Client, id: i64, sources: &Value) {
    client
        .query_one(
            "select spawn_control_record($1)",
            &[&json!({"type": "mission.added", "id": id, "text": format!("mission {id}"),
                      "origin": "human", "sources": sources})],
        )
        .expect("a mission");
}

fn deploy(store: &PostgresStore, head: &str, ok: bool) -> i64 {
    store
        .deploy_record(&json!({"type": "deploy.finished", "ok": ok, "head": head,
                               "by": "landing", "reason": if ok { "" } else { "the build" }}))
        .expect("a deploy")
}

/// The deploys are read newest first, bounded, with the fields a live claim
/// stands on; a failed deploy is there too, so the page can show it.
#[test]
fn deploy_facts_are_newest_first_and_bounded() {
    let (store, _schema) = store("feature_deploys");
    assert_eq!(store.deploy_facts(50).expect("no deploys yet"), json!([]));
    let first = deploy(&store, &"a".repeat(40), true);
    let second = deploy(&store, &"b".repeat(40), false);
    let all = store.deploy_facts(50).expect("the deploys");
    let rows = all.as_array().expect("an array");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["event"], second, "newest first");
    assert_eq!(rows[0]["ok"], false);
    assert_eq!(rows[0]["reason"], "the build");
    assert_eq!(rows[1]["event"], first);
    assert_eq!(rows[1]["ok"], true);
    assert_eq!(rows[1]["head"], "a".repeat(40));
    assert_eq!(rows[1]["by"], "landing");
    assert!(rows[1]["at_ms"].as_i64().is_some(), "{all}");
    assert_eq!(
        store.deploy_facts(1).expect("one").as_array().unwrap().len(),
        1
    );
}

/// A halted mission, a paused one and a red main are named with the record
/// that says so; a healthy mission is not in the list.
#[test]
fn mission_blockers_name_halt_pause_and_red_main() {
    let (store, schema) = store("feature_blockers");
    let mut client = admin(&schema);
    mission(&mut client, 1, &json!(["/m/missions/spawn.git"]));
    mission(&mut client, 2, &json!(["/m/missions/other.git"]));
    mission(&mut client, 3, &json!(["/m/missions/red.git"]));
    // Halt 1, pause 2, leave 3 healthy but red.
    client
        .query_one(
            "select spawn_control_record($1)",
            &[&json!({"type": "mission.halted", "id": 1, "reason": "the gate rejected three"})],
        )
        .expect("a halt");
    let pause = store.mission_pause(2, Some("owner hold")).expect("a pause");
    client
        .execute(
            "insert into mission_source_reds
                    (source, event, main, scenario, failures, since_ms, red_ms, tasks)
             values ('/m/missions/red.git', 9, $1, '137.sh', '[]', 10, 20, '[]')",
            &[&"c".repeat(40)],
        )
        .expect("a red main");
    let blockers = store.mission_blockers().expect("the blockers");
    let row = |id: i64| {
        blockers
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id)
            .cloned()
            .expect("a row")
    };
    assert_eq!(row(1)["halt"]["reason"], "the gate rejected three");
    assert!(row(1)["halt"]["event"].as_i64().is_some());
    assert_eq!(row(2)["paused"]["event"], pause);
    assert_eq!(row(2)["paused"]["reason"], "owner hold");
    assert!(row(2)["paused"]["at_ms"].as_i64().is_some());
    assert_eq!(row(3)["main_red"]["scenario"], "137.sh");
    assert_eq!(row(3)["main_red"]["since_ms"], 10);
    assert_eq!(row(3)["halt"], Value::Null);
    assert_eq!(row(3)["paused"], Value::Null);
}

/// `objectives.features` is an absolute path the owner sets and clears like
/// any other setting; a relative one is refused. Nothing about a
/// capability's state is stored beside it.
#[test]
fn the_feature_map_path_is_a_mission_setting_an_absolute_file_path() {
    let (store, _schema) = store("feature_setting");
    let mission = store
        .mission_add("Spawn RSI", &json!([]), "human")
        .expect("a mission");
    let good = json!({"backlog": "/m/backlog", "resident": "rsi",
                      "features": "/m/rsi/features.json"});
    store
        .mission_configure(mission, Some("/m/gate"), "gate", None, Some(&good))
        .expect("an absolute features path");
    let read = store.objective_missions().expect("the mission");
    assert_eq!(read[0]["objectives"]["features"], "/m/rsi/features.json");
    assert_eq!(read[0]["objectives"]["backlog"], "/m/backlog");

    let relative = json!({"backlog": "/m/backlog", "resident": "rsi",
                          "features": "features.json"});
    let refused = store
        .mission_configure(mission, Some("/m/gate"), "gate", None, Some(&relative))
        .expect_err("a relative path");
    assert!(refused.to_string().contains("absolute path"), "{refused}");

    let cleared = store
        .mission_reconfigure(mission, &json!({"objectives": {"features": null}}))
        .expect("a cleared path");
    assert!(cleared["after"]["objectives"].get("features").is_none(), "{cleared}");
    let read = store.objective_missions().expect("the mission");
    assert!(read[0]["objectives"].get("features").is_none());
    let _ = schema_url; // the module's other helpers are for logins
}
