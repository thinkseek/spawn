//! The nodes (multi-host P1), through the real store: `spawn_node_record`
//! records the owner's join, drain, undrain and removal and the loop's
//! heartbeat, lost and back of the current join, and refuses the rest with
//! nothing recorded; the reads give each node's state from one seek per
//! fact; and the records stay outside the control fold.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); each case runs
//! in a schema of its own.

use postgres::error::SqlState;
use serde_json::{Value, json};

mod common;
use common::{admin, as_login, store};

fn joined(name: &str, ssh: &str) -> Value {
    json!({
        "type": "node.joined", "name": name, "ssh": ssh, "note": "P1 test node",
        "arch": "aarch64", "os": "GNU/Linux", "kernel": "7.2.12", "cores": 10,
        "cpu_max": "600000 100000", "mem_total": 18_903_552_000_u64,
        "mem_max": null, "disk_free": 220_324_995_072_u64,
        "gpu": null, "exit_country": null,
    })
}

fn beat(join: i64) -> Value {
    json!({"type": "node.heartbeat", "name": "runner", "join": join, "load1": 7.6,
           "mem_available": 7_272_091_648_u64, "disk_free": 220_324_995_072_u64})
}

/// The node records the log holds, and those of them in the fold's stream.
fn counts(client: &mut postgres::Client) -> (i64, i64) {
    let row = client
        .query_one(
            "select count(*) filter (where event_name = 'node.recorded'),
                    count(*) filter (where event_name = 'control.recorded'
                                       and kind -> 'detail' -> 'record' ->> 'type' like 'node.%')
               from spawn_events where session_id = 'session_control'",
            &[],
        )
        .expect("count");
    (row.get(0), row.get(1))
}

fn only(store: &spawn_postgres::PostgresStore) -> Value {
    let nodes = store.nodes().expect("the nodes");
    assert_eq!(nodes.as_array().map(Vec::len), Some(1), "{nodes}");
    nodes[0].clone()
}

#[test]
#[allow(clippy::too_many_lines)]
fn a_node_joins_once_beats_is_lost_and_back_drains_and_is_removed() {
    let (store, schema) = store("nodes_lifecycle");
    let mut client = admin(&schema);
    assert_eq!(store.nodes().expect("the nodes"), json!([]));

    // Only the owner records.
    for login in ["spawn_supervisor", "spawn_reader"] {
        let error = as_login(&schema, login)
            .query_one(
                "select spawn_node_record($1)",
                &[&joined("runner", "spawn-runner")],
            )
            .expect_err("not the owner");
        assert_eq!(
            error.code(),
            Some(&SqlState::INSUFFICIENT_PRIVILEGE),
            "{login}: {error}"
        );
    }
    // What is refused before any join: a destination ssh would read as an
    // option, a password, a port out of range, a join missing a fact, a
    // record of a node not joined.
    let mut short = joined("runner", "spawn-runner");
    short.as_object_mut().expect("an object").remove("cores");
    for (record, why) in [
        (joined("runner", "-oProxyCommand=sh"), "ssh destination"),
        (joined("runner", "user:pw@host"), "ssh destination"),
        (joined("runner", "ssh://host:70000"), "ssh destination"),
        (joined("-x", "spawn-runner"), "name"),
        (short, "cores"),
        (
            json!({"type": "node.drained", "name": "runner"}),
            "not joined",
        ),
        (beat(1), "not joined"),
    ] {
        let error = store.node_record(&record).expect_err(why);
        assert!(error.to_string().contains(why), "{why}: {error}");
    }
    assert_eq!(counts(&mut client), (0, 0));

    let join = store
        .node_record(&joined("runner", "spawn-runner"))
        .expect("joined");
    let node = only(&store);
    assert_eq!(
        (
            &node["join"],
            &node["ssh"],
            &node["state"],
            &node["heartbeat"],
            &node["lost_since_ms"]
        ),
        (
            &json!(join),
            &json!("spawn-runner"),
            &json!("live"),
            &Value::Null,
            &Value::Null
        )
    );
    let error = store
        .node_record(&joined("runner", "other"))
        .expect_err("again");
    assert!(error.to_string().contains("joined already"), "{error}");

    // The loop's records name the current join, and their numbers.
    let at = store.node_record(&beat(join)).expect("a heartbeat");
    assert!(at > join);
    assert_eq!(only(&store)["heartbeat"]["load1"], 7.6);
    for (record, why) in [
        (
            json!({"type": "node.heartbeat", "name": "runner", "join": join}),
            "load1 is a number at least 0",
        ),
        (with(beat(join), "load1", json!(-1)), "load1"),
        (
            json!({"type": "node.heartbeat", "name": "runner", "load1": 1,
                "mem_available": 1, "disk_free": 1}),
            "stale probe",
        ),
        (
            json!({"type": "node.back", "name": "runner", "join": join, "down_ms": 1}),
            "not lost",
        ),
        (
            json!({"type": "node.lost", "name": "runner", "join": join}),
            "silent_ms",
        ),
    ] {
        let error = store.node_record(&record).expect_err(why);
        assert!(error.to_string().contains(why), "{why}: {error}");
    }

    // Lost once, back once; lost before drained.
    store
        .node_record(&json!({"type": "node.lost", "name": "runner", "join": join,
                              "last_seen_ms": null, "silent_ms": 60_000, "error": "x"}))
        .expect("lost");
    store
        .node_record(&json!({"type": "node.drained", "name": "runner"}))
        .expect("drained");
    let node = only(&store);
    assert_eq!(node["state"], "lost");
    assert!(node["lost_since_ms"].is_i64(), "{node}");
    let error = store
        .node_record(&json!({"type": "node.lost", "name": "runner", "join": join,
                              "last_seen_ms": 1, "silent_ms": 1, "error": null}))
        .expect_err("lost twice");
    assert!(error.to_string().contains("lost already"), "{error}");
    store
        .node_record(&json!({"type": "node.back", "name": "runner", "join": join, "down_ms": null}))
        .expect("back");
    let node = only(&store);
    assert_eq!(
        (&node["state"], &node["lost_since_ms"]),
        (&json!("drained"), &Value::Null)
    );
    assert!(
        store
            .node_record(&json!({"type": "node.drained", "name": "runner"}))
            .is_err()
    );
    store
        .node_record(&json!({"type": "node.undrained", "name": "runner"}))
        .expect("undrained");
    assert_eq!(only(&store)["state"], "live");

    // Removed: off the list, nothing more of it, and the name joins again
    // from nothing. The reader login reads the list.
    store
        .node_record(&json!({"type": "node.removed", "name": "runner"}))
        .expect("removed");
    assert_eq!(store.nodes().expect("the nodes"), json!([]));
    assert!(
        store
            .node_record(&json!({"type": "node.removed", "name": "runner"}))
            .is_err()
    );
    let again = store
        .node_record(&joined("runner", "ssh://ckl@spawn-runner:22"))
        .expect("joined again");
    let node = only(&store);
    assert_eq!(
        (&node["join"], &node["heartbeat"], &node["state"]),
        (&json!(again), &Value::Null, &json!("live"))
    );
    let url = spawn_postgres::role_url(&common::schema_url(&schema), "spawn_reader")
        .expect("the reader's URL");
    let reader = spawn_postgres::PostgresStore::connect(&url).expect("the reader connects");
    assert_eq!(reader.nodes().expect("read")[0]["name"], "runner");
}

/// `record` with `key` set to `value`.
fn with(mut record: Value, key: &str, value: Value) -> Value {
    record[key] = value;
    record
}

/// Issue #555: a node record in the log never stops a global reader (the
/// Bridge, the Observer): `read_global` decodes it as a control-session
/// event, the node's facts as written.
#[test]
fn the_global_log_decodes_node_records_as_control_session_events() {
    use spawn_core::event::{EventKind, GlobalPosition};
    use spawn_core::port::EventLog;

    let (store, _schema) = store("nodes_read_global");
    let join = store
        .node_record(&joined("runner", "spawn-runner"))
        .expect("joined");
    let at = store.node_record(&beat(join)).expect("a heartbeat");
    let records = store
        .read_global(GlobalPosition(0), 1000)
        .expect("the whole log decodes");
    let nodes: Vec<_> = records
        .iter()
        .filter_map(|record| match &record.event.kind {
            EventKind::NodeRecorded { record: fact } => Some((record, fact)),
            _ => None,
        })
        .collect();
    assert_eq!(nodes.len(), 2, "{records:?}");
    for (record, _) in &nodes {
        assert_eq!(record.event.session_id.as_str(), "session_control");
        assert!(record.event.kind.is_control());
        assert_eq!(record.event.kind.name(), "node.recorded");
    }
    assert_eq!(
        (
            nodes[0].0.position.0,
            &nodes[0].1["type"],
            &nodes[0].1["name"]
        ),
        (
            u64::try_from(join).expect("an id"),
            &json!("node.joined"),
            &json!("runner")
        )
    );
    assert_eq!(
        (
            nodes[1].0.position.0,
            &nodes[1].1["type"],
            &nodes[1].1["join"]
        ),
        (
            u64::try_from(at).expect("an id"),
            &json!("node.heartbeat"),
            &json!(join)
        )
    );
    // A reader resuming between the two takes the heartbeat alone.
    let rest = store
        .read_global(nodes[0].0.position, 1000)
        .expect("the rest decodes");
    assert!(
        rest.iter()
            .any(|record| matches!(record.event.kind, EventKind::NodeRecorded { .. })),
        "{rest:?}"
    );
}

#[test]
fn a_probe_of_a_removed_join_records_nothing_for_the_next_one() {
    let (store, schema) = store("nodes_rejoin");
    let mut client = admin(&schema);
    let old = store
        .node_record(&joined("runner", "old-host"))
        .expect("joined");
    store
        .node_record(&json!({"type": "node.lost", "name": "runner", "join": old,
                              "last_seen_ms": 5, "silent_ms": 60_000, "error": null}))
        .expect("lost");
    store
        .node_record(&json!({"type": "node.removed", "name": "runner"}))
        .expect("removed");
    let new = store
        .node_record(&joined("runner", "new-host"))
        .expect("joined again");
    let before = counts(&mut client);
    // The old endpoint's pending probe answers, or fails, after the rejoin.
    for record in [
        beat(old),
        json!({"type": "node.back", "name": "runner", "join": old, "down_ms": 1}),
        json!({"type": "node.lost", "name": "runner", "join": old,
               "last_seen_ms": 5, "silent_ms": 60_000, "error": null}),
    ] {
        let error = store.node_record(&record).expect_err("a stale probe");
        assert!(error.to_string().contains("stale probe"), "{error}");
    }
    assert_eq!(counts(&mut client), before, "nothing recorded");
    let members = store.node_members().expect("members");
    assert_eq!(
        members,
        json!([{"name": "runner", "ssh": "new-host", "join": new, "lost": null, "beat_ms": null}]),
        "the old join's loss is not the new one's"
    );
    store
        .node_record(&beat(new))
        .expect("the new join's heartbeat");
}

#[test]
fn node_records_stay_outside_the_fold_and_each_read_is_a_seek() {
    let (store, schema) = store("nodes_bounded");
    let mut client = admin(&schema);
    let join = store
        .node_record(&joined("runner", "spawn-runner"))
        .expect("joined");
    store
        .node_record(&joined("desktop", "desktop"))
        .expect("joined");
    for _ in 0..1000 {
        store.node_record(&beat(join)).expect("a heartbeat");
    }
    store
        .node_record(&json!({"type": "node.lost", "name": "runner", "join": join,
                              "last_seen_ms": 1, "silent_ms": 60_000, "error": null}))
        .expect("lost");
    assert_eq!(
        counts(&mut client),
        (1003, 0),
        "none in the control fold's stream"
    );
    // Any fold, this schema's or an older one's, reads only
    // `control.recorded`: a refold, and a second migration, take the log.
    client
        .batch_execute("select spawn_control_refold()")
        .expect("a refold");
    store.migrate().expect("migrated again");
    let members = store.node_members().expect("members");
    assert_eq!(members[1]["lost"]["silent_ms"], 60_000, "{members}");
    assert!(members[1]["beat_ms"].is_i64());

    // A worker's own connection reaches the same schema.
    let worker = store
        .connector(std::time::Duration::from_secs(5))
        .connect()
        .expect("a connection of its own");
    assert_eq!(worker.node_members().expect("members"), members);

    // A name removed and joined again, many times over.
    for _ in 0..30 {
        store
            .node_record(&json!({"type": "node.removed", "name": "desktop"}))
            .expect("removed");
        store
            .node_record(&joined("desktop", "desktop"))
            .expect("joined again");
    }
    assert_eq!(
        store.nodes().expect("the nodes").as_array().map(Vec::len),
        Some(2)
    );

    assert_each_lookup_is_one_seek(&mut client, join);
}

/// With the planner as it is: every lookup of the log is a seek of
/// spawn_events_node that reads at most one row, never a heartbeat
/// history nor the joins of the past, in both reads and in each lookup
/// the writer makes (a type there or not there at all).
fn assert_each_lookup_is_one_seek(client: &mut postgres::Client, join: i64) {
    client
        .batch_execute("analyze spawn_events")
        .expect("analyzed");
    let explain = |client: &mut postgres::Client,
                   sql: &str,
                   params: &[&(dyn postgres::types::ToSql + Sync)]|
     -> Value {
        client
            .query_one(
                &format!("explain (analyze, buffers, format json) {sql}"),
                params,
            )
            .expect("a plan")
            .get(0)
    };
    let mut plans = vec![
        (
            "NODES".to_string(),
            explain(client, spawn_postgres::testing::NODES, &[]),
        ),
        (
            "MEMBERS".to_string(),
            explain(client, spawn_postgres::testing::NODE_MEMBERS, &[]),
        ),
    ];
    for kind in [
        "node.joined",
        "node.removed",
        "node.heartbeat",
        "node.lost",
        "node.back",
        "node.drained",
        "node.undrained",
    ] {
        for (name, after) in [("runner", 0_i64), ("desktop", 0), ("runner", join)] {
            plans.push((
                format!("{kind} {name} {after}"),
                explain(
                    client,
                    "select * from spawn_node_latest($1, $2, $3)",
                    &[&kind, &name, &after],
                ),
            ));
        }
    }
    for (what, plan) in &plans {
        let mut found = Vec::new();
        event_scans(&plan[0]["Plan"], &mut found);
        assert!(!found.is_empty(), "{what}: {plan}");
        for scan in found {
            assert_eq!(scan["Node Type"], "Index Scan", "{what}: {scan}");
            assert_eq!(scan["Index Name"], "spawn_events_node", "{what}: {scan}");
            assert!(scan["Actual Rows"].as_f64() <= Some(1.0), "{what}: {scan}");
            assert!(
                scan["Rows Removed by Filter"].as_f64().unwrap_or(0.0) == 0.0,
                "{what}: {scan}"
            );
        }
        let blocks = plan[0]["Plan"]["Shared Hit Blocks"]
            .as_i64()
            .unwrap_or_default()
            + plan[0]["Plan"]["Shared Read Blocks"]
                .as_i64()
                .unwrap_or_default();
        let bound = if what.starts_with("node.") { 10 } else { 200 };
        assert!(blocks <= bound, "{what}: {blocks} blocks: {plan}");
    }
    // And as the writer runs them, planned once for any name (its generic
    // plan; PostgreSQL 16 and later say it).
    let version: i32 = client
        .query_one("select current_setting('server_version_num')::integer", &[])
        .expect("the version")
        .get(0);
    if version >= 160_000 {
        // Sent as a simple query: its parameters are left unbound.
        let text = client
            .simple_query(
                "explain (generic_plan, format json) \
                 select * from spawn_node_latest($1::text, $2::text, $3::bigint)",
            )
            .expect("a generic plan")
            .into_iter()
            .find_map(|message| match message {
                postgres::SimpleQueryMessage::Row(row) => row.get(0).map(str::to_string),
                _ => None,
            })
            .expect("its text");
        let plan: Value = serde_json::from_str(&text).expect("its JSON");
        let mut found = Vec::new();
        event_scans(&plan[0]["Plan"], &mut found);
        assert_eq!(found.len(), 1, "{plan}");
        assert_eq!(found[0]["Index Name"], "spawn_events_node", "{plan}");
    }
}

/// Every scan of spawn_events in an EXPLAIN plan, subplans included.
fn event_scans<'a>(plan: &'a Value, found: &mut Vec<&'a Value>) {
    if plan["Relation Name"] == "spawn_events" {
        found.push(plan);
    }
    for child in plan["Plans"].as_array().into_iter().flatten() {
        event_scans(child, found);
    }
}

#[test]
fn the_node_index_is_built_after_the_schema_and_again_when_invalid() {
    let (store, schema) = store("nodes_index");
    let mut client = admin(&schema);
    let valid = |client: &mut postgres::Client| -> Option<bool> {
        client
            .query_opt(
                "select indisvalid from pg_index where indexrelid = to_regclass('spawn_events_node')",
                &[],
            )
            .expect("the catalog")
            .map(|row| row.get(0))
    };
    assert_eq!(valid(&mut client), Some(true));
    // Not in the schema's transaction: its text never makes it.
    let sql = include_str!("../sql/schema.sql");
    assert!(!sql.contains("create index if not exists spawn_events_node"));
    // A build cut short leaves an invalid index: the next migration drops
    // it and builds it again; one that finds it valid does nothing.
    client
        .batch_execute("update pg_index set indisvalid = false where indexrelid = to_regclass('spawn_events_node')")
        .ok();
    if valid(&mut client) == Some(false) {
        store.migrate().expect("migrated again");
        assert_eq!(valid(&mut client), Some(true));
    }
    client
        .batch_execute("drop index spawn_events_node")
        .expect("dropped");
    store.migrate().expect("migrated again");
    assert_eq!(valid(&mut client), Some(true));
}
