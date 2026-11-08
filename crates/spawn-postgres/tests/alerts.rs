//! Alerts: the loop's `alert.fired` and `alert.cleared` records, folded into
//! `spawn_alerts` (docs/fleet-architecture.md, "Alerts"; docs/storage.md,
//! "Alerts").
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use serde_json::{Value, json};

mod common;
use common::{admin, schema_url, store};
use spawn_postgres::{PostgresStore, role_url};

fn alert(client: &mut Client, key: &str) -> Value {
    client
        .query_opt(
            "select to_jsonb(a) from spawn_alerts a where a.key = $1",
            &[&key],
        )
        .expect("read the table")
        .map_or(Value::Null, |row| row.get(0))
}

fn control_types(client: &mut Client, key: &str) -> Vec<String> {
    client
        .query(
            "select kind -> 'detail' -> 'record' ->> 'type'
               from spawn_events
              where session_id = 'session_control'
                and kind -> 'detail' -> 'record' ->> 'key' = $1
              order by session_seq",
            &[&key],
        )
        .expect("read the control stream")
        .iter()
        .map(|row| row.get(0))
        .collect()
}

fn fired(key: &str, notice: bool) -> Value {
    json!({
        "type": "alert.fired", "key": key, "kind": "memory_low", "subject": "host",
        "evidence": {"measured_mb": 1000, "threshold_mb": 4096},
        "text": "MemAvailable is 1000 MiB, under 4096", "notice": notice,
    })
}

fn cleared(key: &str) -> Value {
    json!({
        "type": "alert.cleared", "key": key, "kind": "memory_low", "subject": "host",
        "evidence": {"measured_mb": 9000},
    })
}

/// An alert fires once per occurrence and clears at most once, whatever
/// restarts: the store refuses a second `fired`, a `cleared` without its
/// `fired`, and a second `cleared`, and the table is the log's fold.
#[test]
fn an_alert_fires_once_and_clears_once() {
    let (store, schema) = store("alerts_fold");
    let mut client = admin(&schema);
    let id = store
        .alert_record(&fired("memory_low:1", false))
        .expect("fire");
    let row = alert(&mut client, "memory_low:1");
    assert_eq!(row["state"], "open");
    assert_eq!(row["kind"], "memory_low");
    assert_eq!(row["subject"], "host");
    assert_eq!(row["fired_event_id"], id);
    assert_eq!(row["notice"], false);
    assert_eq!(row["evidence"]["measured_mb"], 1000);
    assert_eq!(row["cleared_event_id"], Value::Null);

    assert!(
        store.alert_record(&fired("memory_low:1", false)).is_err(),
        "a second fired for a key is refused"
    );
    assert!(
        store.alert_record(&cleared("memory_low:2")).is_err(),
        "a cleared without its fired is refused"
    );

    let cleared_id = store.alert_record(&cleared("memory_low:1")).expect("clear");
    let row = alert(&mut client, "memory_low:1");
    assert_eq!(row["state"], "cleared");
    assert_eq!(row["cleared_event_id"], cleared_id);
    assert_eq!(row["cleared_evidence"]["measured_mb"], 9000);
    assert!(
        store.alert_record(&cleared("memory_low:1")).is_err(),
        "a second cleared is refused"
    );
    assert_eq!(
        control_types(&mut client, "memory_low:1"),
        ["alert.fired", "alert.cleared"],
        "the log holds each transition once"
    );
}

/// A notice is news: it never clears.
#[test]
fn a_notice_is_recorded_once_and_never_clears() {
    let (store, schema) = store("alerts_notice");
    let mut client = admin(&schema);
    store
        .alert_record(&fired("task_state:7:10", true))
        .expect("fire");
    let row = alert(&mut client, "task_state:7:10");
    assert_eq!(row["state"], "notice");
    assert_eq!(row["notice"], true);
    assert!(
        store.alert_record(&cleared("task_state:7:10")).is_err(),
        "a notice has nothing to clear"
    );
    assert_eq!(alert(&mut client, "task_state:7:10")["state"], "notice");
}

/// The records are the owner's: a restricted login may read the table (the
/// supervisor's `spawn alerts`) and may not record an alert.
#[test]
fn only_the_owner_records_alerts() {
    let (_store, schema) = store("alerts_owner");
    let login = role_url(&schema_url(&schema), "spawn_supervisor").expect("the login's URL");
    let supervisor = PostgresStore::connect(&login).expect("log in");
    let refused = supervisor
        .alert_record(&fired("memory_low:3", false))
        .expect_err("a restricted login records no alert");
    // Either the function's grant (revoked from everyone but the owner) or
    // its own caller check refuses it.
    let message = refused.to_string();
    assert!(
        message.contains("permission denied for function spawn_alert_record")
            || message.contains("may not record alerts"),
        "{refused}"
    );
    let alerts = supervisor.alerts(0).expect("read the alerts");
    assert!(alerts.is_array(), "{alerts}");
}
