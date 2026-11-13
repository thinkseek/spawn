//! An objective's verification after its landing (objective 133): the
//! owner-only `spawn_verification_record` and what `spawn verify list`
//! reads (`PostgresStore::verification_facts`).
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use serde_json::{Value, json};
use spawn_postgres::{PostgresStore, role_url};

mod common;
use common::{admin, schema_url, store};

/// Objective 133's own prompt, as a landed task's brief holds it.
const BRIEF_133: &str = include_str!("verification-133-brief.txt");

/// A control record as the integrator writes it.
fn control(client: &mut Client, record: &Value) -> i64 {
    client
        .query_one("select spawn_control_record($1)", &[record])
        .expect("a control record")
        .get(0)
}

fn mission(client: &mut Client, id: i64) {
    control(
        client,
        &json!({"type": "mission.added", "id": id, "text": format!("mission {id}"),
                "origin": "human", "sources": []}),
    );
}

fn assigned(client: &mut Client, task: i64, mission: i64, objective: i64, brief: &str) {
    control(
        client,
        &json!({"type": "task.assigned", "id": task, "mission_id": mission,
                "kind": "ephemeral", "created_by": "resident",
                "agent_id": format!("agent_worker_{task}"),
                "session_id": format!("session_worker_{task}"),
                "objective": objective, "brief": brief}),
    );
}

fn landed(client: &mut Client, task: i64) -> i64 {
    control(
        client,
        &json!({"type": "task.landed", "id": task, "from": "a".repeat(40),
                "to": "b".repeat(40)}),
    )
}

fn verification(mission: i64, objective: i64, result: &str) -> Value {
    json!({"type": "objective.verified", "mission": mission, "objective": objective,
           "by": "lead", "how": "read the running product", "result": result})
}

fn recorded(client: &mut Client) -> i64 {
    client
        .query_one(
            "select count(*) from spawn_events
              where session_id = 'session_control'
                and kind -> 'detail' -> 'record' ->> 'type' = 'objective.verified'",
            &[],
        )
        .expect("count")
        .get(0)
}

/// A verification record has exactly its six fields, each checked; the
/// objective must have landed in that mission.
#[test]
fn a_verification_record_has_its_fields_and_nothing_else() {
    let (store, schema) = store("verification_record");
    let mut client = admin(&schema);
    mission(&mut client, 1);
    mission(&mut client, 2);
    assigned(&mut client, 1, 1, 28, "Objective 28.");
    landed(&mut client, 1);
    assigned(&mut client, 2, 1, 29, "Objective 29, not landed yet.");

    let good = verification(1, 28, "pass");
    store.verification_record(&good).expect("a verification");
    store
        .verification_record(&verification(1, 28, "fail"))
        .expect("a fail");
    let with = |field: &str, value: Value| {
        let mut record = good.clone();
        record[field] = value;
        record
    };
    let without = |field: &str| {
        let mut record = good.clone();
        record.as_object_mut().expect("an object").remove(field);
        record
    };
    for (bad, why) in [
        (with("type", json!("objective.checked")), "the type"),
        (with("at_ms", json!(1)), "a field more"),
        (without("mission"), "no mission"),
        (without("objective"), "no objective"),
        (without("by"), "no by"),
        (without("how"), "no how"),
        (without("result"), "no result"),
        (with("mission", json!(0)), "mission 0"),
        (with("mission", json!(1.5)), "mission not whole"),
        (with("mission", json!("1")), "mission a string"),
        (with("objective", json!(0)), "objective 0"),
        (with("objective", json!(-28)), "objective negative"),
        (with("objective", json!(28.5)), "objective not whole"),
        (with("by", json!("")), "by empty"),
        (with("by", json!("   ")), "by blank"),
        (with("by", json!("x".repeat(81))), "by too long"),
        (with("by", json!(7)), "by not a string"),
        (with("how", json!("")), "how empty"),
        (with("how", json!(" \n ")), "how blank"),
        (with("how", json!("x".repeat(501))), "how too long"),
        (
            with("result", json!("maybe")),
            "result neither pass nor fail",
        ),
        (with("result", json!(true)), "result not a string"),
    ] {
        assert!(store.verification_record(&bad).is_err(), "{why}: {bad}");
    }
    store
        .verification_record(&with("by", json!("x".repeat(80))))
        .expect("by of 80 characters");
    store
        .verification_record(&with("how", json!("x".repeat(500))))
        .expect("how of 500 characters");
    for (mission, objective, why) in [
        (2, 28, "the same number in another mission"),
        (1, 30, "an objective never assigned"),
        (1, 29, "an objective not landed yet"),
    ] {
        let refused = store
            .verification_record(&verification(mission, objective, "pass"))
            .expect_err(why);
        assert!(
            refused
                .to_string()
                .contains(&format!("objective {mission}/{objective} never landed")),
            "{why}: {refused}"
        );
    }
    assert_eq!(recorded(&mut client), 4, "only the good ones were recorded");
    // The fold takes it again (folded into no table).
    client
        .batch_execute("select spawn_control_refold()")
        .expect("refold");
}

/// Only the owner records verifications.
#[test]
fn only_the_owner_records_verifications() {
    let (_store, schema) = store("verification_owner");
    let mut client = admin(&schema);
    mission(&mut client, 1);
    assigned(&mut client, 1, 1, 28, "Objective 28.");
    landed(&mut client, 1);
    let login = role_url(&schema_url(&schema), "spawn_supervisor").expect("the login's URL");
    let supervisor = PostgresStore::connect(&login).expect("log in");
    let refused = supervisor
        .verification_record(&verification(1, 28, "pass"))
        .expect_err("a restricted login records no verification");
    let message = refused.to_string();
    assert!(
        message.contains("permission denied for function spawn_verification_record")
            || message.contains("may not record verifications"),
        "{refused}"
    );
    assert_eq!(recorded(&mut client), 0);
}

/// The read: the newest landing per mission and objective with its
/// "Verified by:" line (objective 133's own prompt as its brief: the
/// paragraph's line, not the quote) or null, every verification oldest
/// first, and the newest deploy.
#[test]
fn the_read_has_each_objectives_newest_landing_its_line_the_verifications_and_the_deploy() {
    let (store, schema) = store("verification_read");
    let mut client = admin(&schema);
    let empty = store.verification_facts().expect("the facts");
    assert_eq!(
        empty,
        json!({"landings": [], "verifications": [], "deploy": null})
    );

    mission(&mut client, 4);
    mission(&mut client, 9);
    assigned(&mut client, 1, 4, 133, BRIEF_133);
    landed(&mut client, 1);
    assigned(&mut client, 2, 9, 133, "Objective 133 of another mission.");
    landed(&mut client, 2);
    assigned(
        &mut client,
        3,
        4,
        28,
        "Objective 28.\nVerified by: the first.",
    );
    landed(&mut client, 3);
    // 4/028 again, its brief with two such lines: the newest landing and
    // the last line.
    assigned(
        &mut client,
        4,
        4,
        28,
        "Objective 28 again.\nVerified by: the first.\n\n  Verified by: the second.  \nrev-g reviews.",
    );
    landed(&mut client, 4);
    // Assigned, never landed: no landing.
    assigned(&mut client, 5, 4, 29, "Objective 29.\nVerified by: never.");
    store
        .verification_record(&verification(4, 133, "fail"))
        .expect("a fail");
    store
        .verification_record(&verification(9, 133, "pass"))
        .expect("a pass");
    store
        .deploy_record(
            &json!({"type": "deploy.finished", "ok": true, "head": "a".repeat(40),
                               "by": "owner", "reason": ""}),
        )
        .expect("a deploy");
    store
        .deploy_record(
            &json!({"type": "deploy.finished", "ok": false, "head": "b".repeat(40),
                               "by": "landing", "reason": "the build failed"}),
        )
        .expect("a deploy");

    let facts = store.verification_facts().expect("the facts");
    let mut at = |event: &str| -> i64 {
        client
            .query_one(
                "select recorded_at_ms from spawn_events
                  where session_id = 'session_control'
                    and kind -> 'detail' -> 'record' ->> 'type' = 'task.landed'
                    and kind -> 'detail' -> 'record' ->> 'id' = $1",
                &[&event],
            )
            .expect("a landing")
            .get(0)
    };
    let landings: Vec<Value> = facts["landings"]
        .as_array()
        .unwrap_or_else(|| panic!("landings: {facts}"))
        .clone();
    assert_eq!(
        landings,
        [
            json!({"mission": 4, "objective": 28, "task": 4, "landed_ms": at("4"),
                   "verified_by": "Verified by: the second."}),
            json!({"mission": 4, "objective": 133, "task": 1, "landed_ms": at("1"),
                   "verified_by": "Verified by: the gate runs 133.sh on the candidate. After \
                   the landing, the lead runs `spawn verify list` on the live store and posts \
                   the unverified objectives since 11-13 beside sync/features.md, records one \
                   verification (`spawn verify 4/133 --by lead --how … --result pass`, once 133 \
                   itself has been looked at) and posts that 4/133 leaves the list; rev-g \
                   reviews."}),
            json!({"mission": 9, "objective": 133, "task": 2, "landed_ms": at("2"),
                   "verified_by": null}),
        ],
        "{facts}"
    );
    let verifications = facts["verifications"].as_array().expect("verifications");
    assert_eq!(verifications.len(), 2, "{facts}");
    assert_eq!(verifications[0]["mission"], 4, "oldest first: {facts}");
    assert_eq!(verifications[0]["result"], "fail");
    assert_eq!(verifications[0]["by"], "lead");
    assert_eq!(verifications[0]["how"], "read the running product");
    assert_eq!(verifications[1]["mission"], 9);
    assert_eq!(verifications[1]["objective"], 133);
    assert_eq!(verifications[1]["result"], "pass");
    assert!(
        verifications[0]["at_ms"].as_i64() <= verifications[1]["at_ms"].as_i64(),
        "{facts}"
    );
    let deploy = &facts["deploy"];
    assert_eq!(deploy["head"], "b".repeat(40), "the newest deploy: {facts}");
    assert_eq!(deploy["ok"], false);
    assert_eq!(deploy["reason"], "the build failed");
    assert!(deploy["at_ms"].as_i64().is_some(), "{facts}");
}

/// When the event `event` was recorded.
fn event_ms(client: &mut Client, event: i64) -> i64 {
    client
        .query_one(
            "select recorded_at_ms from spawn_events where id = $1",
            &[&event],
        )
        .expect("an event")
        .get(0)
}

/// When the newest control record was recorded.
fn last_control_ms(client: &mut Client) -> i64 {
    client
        .query_one(
            "select max(recorded_at_ms) from spawn_events where session_id = 'session_control'",
            &[],
        )
        .expect("the last record")
        .get(0)
}

/// The alert step's polled read (objective 135): the objectives whose
/// newest landing is at or after `since_ms` and whose newest verification
/// after it is not a pass, and how many of them landed at or before
/// `old_at_ms`, with the newest of those.
#[test]
fn the_unverified_landings_read_counts_the_window_and_the_old() {
    let (store, schema) = store("verification_unverified");
    let mut client = admin(&schema);
    // Each record a millisecond or more after the one before it, so "after
    // the landing" is never the same millisecond.
    let pause = || std::thread::sleep(std::time::Duration::from_millis(3));
    mission(&mut client, 1);
    let land = |client: &mut Client, task: i64, objective: i64| -> i64 {
        assigned(client, task, 1, objective, "Objective.");
        pause();
        let event = landed(client, task);
        pause();
        event_ms(client, event)
    };
    let verify = |objective: i64, result: &str| {
        store
            .verification_record(&verification(1, objective, result))
            .expect("a verification");
        pause();
    };

    // 1: landed before the window.
    let before = land(&mut client, 1, 1);
    // 2: pass, then fail (counted, old). 3: fail, then pass (not counted).
    let two = land(&mut client, 2, 2);
    verify(2, "pass");
    verify(2, "fail");
    land(&mut client, 3, 3);
    verify(3, "fail");
    verify(3, "pass");
    // 5: a fail, then a landing again; 6: a pass, then a landing again.
    land(&mut client, 5, 5);
    verify(5, "fail");
    land(&mut client, 6, 6);
    verify(6, "pass");
    // Every landing so far is old; what lands after is under the bound.
    let bound = last_control_ms(&mut client);
    pause();
    // 4: landed under the bound. 5 and 6 land again.
    land(&mut client, 4, 4);
    land(&mut client, 7, 5);
    land(&mut client, 8, 6);

    let since = before + 1;
    assert_eq!(
        store.unverified_landings(since, bound).expect("the read"),
        json!({"unverified": 4, "old": 1, "newest_old_ms": two}),
        "2 (failed after its pass), 4 (under the bound), 5 and 6 (landed again): only 2 is old"
    );
    assert_eq!(
        store.unverified_landings(before, bound).expect("the read"),
        json!({"unverified": 5, "old": 2, "newest_old_ms": two}),
        "the window is at or after since_ms"
    );
    let newest = last_control_ms(&mut client);
    assert_eq!(
        store.unverified_landings(since, newest).expect("the read")["old"],
        4,
        "at or before old_at_ms"
    );
    assert_eq!(
        store
            .unverified_landings(newest + 1, newest + 1)
            .expect("the read"),
        json!({"unverified": 0, "old": 0, "newest_old_ms": null}),
        "nothing in the window"
    );
}
