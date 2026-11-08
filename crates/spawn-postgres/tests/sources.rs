//! Mission sources (audit H5): every source is git run as the owner, so
//! `spawn_mission_add` takes only a checked shape, and from the supervisor
//! only a source a human already gave a mission.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use serde_json::json;

mod common;
use common::{admin, store};

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

    let mut client = common::as_login(&schema, "spawn_supervisor");
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
    let mut client = admin(&schema);
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

/// The branches a restarted Bridge publishes again: every agent with an
/// open task, with its first task's source (causality audit #4).
#[test]
fn agents_with_an_open_task_are_published_again_from_their_first_source() {
    let (store, schema) = store("publishing_agents");
    let widget = store
        .mission_add("widget", &json!(["/srv/widget.git"]), "human")
        .expect("add");
    let scratch = store
        .mission_add("scratch", &json!([]), "human")
        .expect("add");
    let open = store
        .task_assign(widget, "ephemeral", "a", "brief", "default")
        .expect("assign");
    let cancelled = store
        .task_assign(widget, "ephemeral", "b", "brief", "default")
        .expect("assign");
    store.task_cancel(cancelled).expect("cancel");
    let elsewhere = store
        .task_assign(scratch, "ephemeral", "c", "brief", "default")
        .expect("assign");
    let mut client = admin(&schema);
    let agent = |client: &mut postgres::Client, task: i64| -> String {
        client
            .query_one("select agent_id from mission_tasks where id = $1", &[&task])
            .expect("the task")
            .get(0)
    };
    let mut expected = vec![
        (
            agent(&mut client, open),
            Some("/srv/widget.git".to_string()),
        ),
        (agent(&mut client, elsewhere), None),
    ];
    expected.sort();
    assert_eq!(store.publishing_agents().expect("agents"), expected);
}

/// A resident started before it had any task was mounted from the default
/// repository, where its branch lives: its first task's mission does not
/// move it (its checkpoints, and so its turns' ends, would find no branch),
/// and no mission publishes it. One whose first execution came after its
/// first task is the mission's, as before. Read from the records.
#[test]
fn an_agent_that_ran_before_its_first_task_stays_on_the_default_repository() {
    let (store, schema) = store("source_first_mount");
    let mission = store
        .mission_add("scratch", &json!([]), "human")
        .expect("add");
    let mut client = admin(&schema);
    let started = |client: &mut postgres::Client, agent: &str, at: &str| {
        client
            .batch_execute(&format!(
                "select spawn_append_trusted('{agent}', '{}', 'exec_{agent}', 'bridge', null,
                    '{{\"name\": \"execution.started\", \"detail\": {{\"engine\": \"claude\"}}}}'::jsonb);
                 update spawn_events set recorded_at_ms = {at}
                  where agent_id = '{agent}' and event_name = 'execution.started'",
                agent.replace("agent_", "session_")
            ))
            .expect("record an execution");
    };
    for name in ["early", "late"] {
        store.resident_propose(name, "a concern").expect("propose");
        store.resident_approve(name).expect("approve");
    }
    // `early` ran before its task; `late` only after.
    started(&mut client, "agent_resident_early", "1");
    store
        .task_assign(mission, "resident", "early", "brief", "default")
        .expect("assign");
    store
        .task_assign(mission, "resident", "late", "brief", "default")
        .expect("assign");
    started(&mut client, "agent_resident_late", "spawn_now_ms() + 1000");
    assert_eq!(
        store.agent_source("agent_resident_early").expect("source"),
        None
    );
    assert_eq!(
        store.agent_source("agent_resident_late").expect("source"),
        Some(None),
        "a scratch mission's agent"
    );
    assert_eq!(
        store.publishing_agents().expect("agents"),
        vec![("agent_resident_late".to_string(), None)]
    );
}
