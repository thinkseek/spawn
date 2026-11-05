//! Mission sources (audit H5): every source is git run as the owner, so
//! `spawn_mission_add` takes only a checked shape, and from the supervisor
//! only a source a human already gave a mission.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use serde_json::json;

mod common;
use common::{admin, login, store};

#[test]
fn a_mission_source_is_a_checked_shape_and_the_supervisor_names_only_listed_ones() {
    let (store, schema) = store("mission_sources");
    for good in [
        "/srv/widget.git",
        "https://github.com/acme/widget.git",
        "ssh://git@github.com/acme/widget.git",
        "git@github.com:acme/widget.git",
    ] {
        store
            .mission_add("from a human", &json!([good]), "human")
            .unwrap_or_else(|error| panic!("{good}: {error}"));
    }
    for bad in [
        json!(["ext::sh -c touch% /tmp/x"]),
        json!(["fd::3"]),
        json!(["file:///srv/widget.git"]),
        json!(["widget"]),
        json!(["-uhelp"]),
        json!(["https://-oProxyCommand=x/r"]),
        json!(["https://host/a b"]),
        json!(["git@host:-path"]),
        json!([42]),
        json!("/srv/widget.git"),
    ] {
        assert!(
            store.mission_add("bad", &bad, "human").is_err(),
            "the owner may not add {bad}"
        );
    }

    let mut client = admin(&schema);
    let supervisor = login(&schema, "spawn_supervisor");
    client
        .batch_execute(&format!("set session authorization {supervisor}"))
        .expect("become the supervisor");
    let add = |client: &mut postgres::Client, sources: serde_json::Value| {
        client.query_one(
            "select spawn_mission_add('from the supervisor', $1, null)",
            &[&sources],
        )
    };
    add(&mut client, json!([])).expect("a mission with no source");
    add(&mut client, json!(["https://github.com/acme/widget.git"]))
        .expect("a source a human already gave a mission");
    for unlisted in [
        "https://github.com/attacker/payload.git",
        "/home/owner/private-repo",
        "ext::sh -c touch% /tmp/x",
    ] {
        let refused = add(&mut client, json!([unlisted])).expect_err("not listed");
        assert!(
            refused.code().is_some_and(|code| {
                *code == postgres::error::SqlState::INSUFFICIENT_PRIVILEGE
                    || *code == postgres::error::SqlState::INVALID_PARAMETER_VALUE
            }),
            "{unlisted}: {refused}"
        );
    }
    client
        .batch_execute("reset session authorization")
        .expect("back to the owner");
    // A source only the supervisor named (here none was recorded) never
    // lists itself: the supervisor's own missions do not count.
    let listed: i64 = client
        .query_one(
            "select count(*) from mission_missions where sources @> '[\"https://github.com/attacker/payload.git\"]'",
            &[],
        )
        .expect("count")
        .get(0);
    assert_eq!(listed, 0);
}
