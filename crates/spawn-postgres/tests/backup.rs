//! The backup records: `spawn backup` writes them through the owner-only
//! `spawn_backup_record`, and the fold accepts them (it folds them into no
//! table, docs/storage.md "Where state lives").
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use serde_json::{Value, json};

mod common;
use common::{admin, schema_url, store};
use spawn_postgres::{PostgresStore, role_url};

fn record(kind: &str) -> Value {
    json!({
        "type": kind,
        "keys": ["postgres/hourly/spawn-20261108-071500.dump.gpg"],
        "tiers": ["hourly", "daily"], "skipped": [],
        "sha256": "a".repeat(64), "bytes": 1_700_000, "stored_bytes": 1_700_100,
        "events": 23868, "max_event_id": 24868,
        "held_bytes": 0, "written_bytes": 1_700_100,
        "alert_bytes": 8_000_000_000u64, "limit_bytes": 10_000_000_000u64,
        "over_alert": false, "dump_ms": 270, "upload_ms": 300,
    })
}

fn control_records(client: &mut Client, kind: &str) -> i64 {
    client
        .query_one(
            "select count(*) from spawn_events
              where session_id = 'session_control' and event_name = 'control.recorded'
                and kind -> 'detail' -> 'record' ->> 'type' = $1",
            &[&kind],
        )
        .expect("read the control stream")
        .get(0)
}

/// The three record types are accepted and folded into no table; any other
/// type is refused.
#[test]
fn the_backup_records_are_accepted_and_any_other_type_refused() {
    let (store, schema) = store("backup_records");
    let mut client = admin(&schema);
    store
        .backup_record(&record("backup.dumped"))
        .expect("a dump record");
    store
        .backup_record(&json!({
            "type": "backup.bundled", "kind": "full", "base_event_id": null,
            "keys": [], "tiers": ["full"], "skipped": [], "sha256": "b",
            "bytes": 1, "stored_bytes": 2, "repos": [], "missing": [],
            "transcripts": {"files": 0, "bytes": 0, "sizes": {}},
            "held_bytes": 0, "written_bytes": 0, "alert_bytes": 1, "limit_bytes": 2,
            "over_alert": false, "duration_ms": 1,
        }))
        .expect("a bundle record");
    store
        .backup_record(&json!({
            "type": "backup.verified", "ok": false,
            "reason": "SPAWN_BACKUP_DRILL_DATABASE_URL is not set",
        }))
        .expect("a drill record");
    assert_eq!(control_records(&mut client, "backup.dumped"), 1);
    assert_eq!(control_records(&mut client, "backup.bundled"), 1);
    assert_eq!(control_records(&mut client, "backup.verified"), 1);
    assert!(
        store
            .backup_record(&json!({"type": "backup.pruned"}))
            .is_err(),
        "a type the contract does not name is refused"
    );
}

/// Only the owner writes them: the supervisor's login reads the log through
/// its own paths and may not record a backup.
#[test]
fn only_the_owner_records_backups() {
    let (_store, schema) = store("backup_owner");
    let login = role_url(&schema_url(&schema), "spawn_supervisor").expect("the login's URL");
    let supervisor = PostgresStore::connect(&login).expect("log in");
    let refused = supervisor
        .backup_record(&record("backup.dumped"))
        .expect_err("a restricted login records no backup");
    let message = refused.to_string();
    assert!(
        message.contains("permission denied for function spawn_backup_record")
            || message.contains("may not record backups"),
        "{refused}"
    );
}
