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

/// Objective 093: a drill's last pass is the newest record of its type
/// with `ok` true, a failed drill after it left aside; none without one,
/// and the dashboard's read-only login reads it.
#[test]
fn a_drills_last_pass_is_its_newest_record_with_ok_true() {
    let (store, schema) = store("backup_drill_pass");
    let mut client = admin(&schema);
    assert_eq!(
        store.newest_passed_record("backup.verified").expect("read"),
        None
    );
    let drill = |ok: bool| json!({"type": "backup.verified", "ok": ok, "reason": ""});
    store.backup_record(&drill(true)).expect("a first pass");
    let passed = store.backup_record(&drill(true)).expect("a second pass");
    store.backup_record(&drill(false)).expect("a failure after");
    let at: i64 = client
        .query_one(
            "select recorded_at_ms from spawn_events where id = $1",
            &[&passed],
        )
        .expect("read the pass")
        .get(0);
    assert_eq!(
        store.newest_passed_record("backup.verified").expect("read"),
        Some(at)
    );
    assert_eq!(
        store.newest_passed_record("backup.dumped").expect("read"),
        None,
        "another type has no pass"
    );
    let login = role_url(&schema_url(&schema), "spawn_reader").expect("the reader's URL");
    let reader = PostgresStore::connect(&login).expect("log in");
    assert_eq!(
        reader
            .newest_passed_record("backup.verified")
            .expect("the reader reads it"),
        Some(at)
    );
    // Objective 105: the newest record whatever its ok, and the last pass
    // beside it, in one statement; the reader reads it too.
    let newest = reader
        .drill_of_type("backup.verified")
        .expect("the reader reads the drill")
        .expect("a record of the type");
    assert_eq!(newest["record"]["ok"], json!(false), "the failure after");
    assert_eq!(newest["passed_at_ms"], json!(at));
    assert!(newest["at_ms"].as_i64() >= Some(at), "{newest}");
    let passed = store
        .backup_record(&drill(true))
        .expect("a pass after the failure");
    let newest = store
        .drill_of_type("backup.verified")
        .expect("read")
        .expect("a record");
    let at: i64 = client
        .query_one(
            "select recorded_at_ms from spawn_events where id = $1",
            &[&passed],
        )
        .expect("read the pass")
        .get(0);
    assert_eq!(newest["record"]["ok"], json!(true));
    assert_eq!(newest["at_ms"], json!(at));
    assert_eq!(
        newest["passed_at_ms"],
        json!(at),
        "the newest passed: itself"
    );
    assert_eq!(
        store.drill_of_type("backup.dumped").expect("read"),
        None,
        "no record of the type"
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

fn stored() -> Value {
    json!({
        "type": "backup.stored", "area": "transcripts", "host": "spawn", "account": "ckl",
        "manifest": "transcripts/manifests/spawn/ckl/20261113-003117-4460e808.json.gpg",
        "files": 2, "unchanged": 0, "grown": 1, "whole": 1,
        "segments_written": 2, "segments_found": 0, "bytes_written": 714,
    })
}

/// A `spawn store put` run (`backup.stored`, crate backup/store.rs) is
/// accepted through the owner's spawn_backup_record and folded into no
/// table; its shape is fixed: another field, an area it does not name, a
/// count that is not a whole number at or above 0, a key that is not a
/// string, each refuses the whole record.
#[test]
fn a_store_run_is_a_backup_stored_record_of_fixed_shape() {
    let (store, schema) = store("backup_stored");
    let mut client = admin(&schema);
    store.backup_record(&stored()).expect("a store record");
    assert_eq!(control_records(&mut client, "backup.stored"), 1);
    let with = |field: &str, value: Value| {
        let mut record = stored();
        record[field] = value;
        record
    };
    for (what, record) in [
        ("another field", with("sha256", json!("a".repeat(64)))),
        ("an area it does not name", with("area", json!("models"))),
        ("no area", with("area", Value::Null)),
        ("a negative count", with("files", json!(-1))),
        ("a fractional count", with("bytes_written", json!(1.5))),
        ("a count as text", with("whole", json!("1"))),
        (
            "a manifest that is not a string",
            with("manifest", json!(1)),
        ),
    ] {
        assert!(store.backup_record(&record).is_err(), "{what}: {record}");
    }
    assert_eq!(
        control_records(&mut client, "backup.stored"),
        1,
        "nothing refused was recorded"
    );
}

/// A `backup.stored` record may name the key that sealed its objects
/// (`sealed_with`, objective 115) and needs not: a key's name ("k2", "k12")
/// is accepted, a record without it is accepted as before, and anything
/// else under that name (another shape, a number, null) refuses the whole
/// record, as does another field beside a good `sealed_with`.
#[test]
fn a_store_run_may_name_the_key_that_sealed_it() {
    let (store, schema) = store("backup_sealed_with");
    let mut client = admin(&schema);
    let sealed = |value: Value| {
        let mut record = stored();
        record["sealed_with"] = value;
        record
    };
    store
        .backup_record(&stored())
        .expect("a record without sealed_with");
    for name in ["k2", "k12"] {
        store
            .backup_record(&sealed(json!(name)))
            .unwrap_or_else(|e| panic!("sealed_with {name}: {e}"));
    }
    assert_eq!(control_records(&mut client, "backup.stored"), 3);
    let mut extra = sealed(json!("k2"));
    extra["sealed_by"] = json!("k2");
    for (what, record) in [
        ("another shape", sealed(json!("x7"))),
        ("key zero", sealed(json!("k0"))),
        ("a trailing space", sealed(json!("k2 "))),
        ("a number", sealed(json!(2))),
        ("null", sealed(Value::Null)),
        ("another field beside sealed_with", extra),
    ] {
        let refused = store
            .backup_record(&record)
            .expect_err(&format!("{what}: {record}"));
        let message = refused.to_string();
        if what.starts_with("another field") {
            assert!(message.contains("nothing else"), "{what}: {message}");
        } else {
            assert!(message.contains("a key's name"), "{what}: {message}");
            assert!(
                !message.contains("x7") && !message.contains("k0"),
                "{message}"
            );
        }
    }
    assert_eq!(
        control_records(&mut client, "backup.stored"),
        3,
        "nothing refused was recorded"
    );
    let names: Vec<Option<String>> = client
        .query(
            "select kind -> 'detail' -> 'record' ->> 'sealed_with' from spawn_events
              where session_id = 'session_control' and event_name = 'control.recorded'
                and kind -> 'detail' -> 'record' ->> 'type' = 'backup.stored'
              order by id",
            &[],
        )
        .expect("read the stored records")
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(names, [None, Some("k2".into()), Some("k12".into())]);
}

fn drilled(kind: &str) -> Value {
    json!({
        "type": kind, "ok": true, "reason": "",
        "bundle": "bundles/spawn/full/20261113-040000.tar.zst.gpg", "bundle_event_id": 41,
        "mismatches": 0, "duration_ms": 1234,
    })
}

/// A restore drill's verdict per resource (`backup.drilled.transcripts`,
/// `backup.drilled.missions`, objective 104) is accepted through the
/// owner's spawn_backup_record and folded into no table; its shape is
/// fixed: another field, ok that is not a boolean or not "reason is empty",
/// a reason that is not a string or over 500 characters, a bundle that is
/// not a string, a count that is not a whole number at or above 0, each
/// refuses the whole record.
#[test]
fn a_drill_verdict_is_a_backup_drilled_record_of_fixed_shape() {
    let (store, schema) = store("backup_drilled");
    let mut client = admin(&schema);
    for kind in ["backup.drilled.transcripts", "backup.drilled.missions"] {
        store.backup_record(&drilled(kind)).expect("a passed drill");
    }
    let mut failed = drilled("backup.drilled.missions");
    failed["ok"] = json!(false);
    failed["reason"] = json!("r".repeat(500));
    failed["mismatches"] = json!(3);
    store.backup_record(&failed).expect("a failed drill");
    let with = |field: &str, value: Value| {
        let mut record = drilled("backup.drilled.transcripts");
        record[field] = value;
        record
    };
    for (what, record) in [
        ("another field", with("keys", json!([]))),
        ("ok false with no reason", with("ok", json!(false))),
        ("ok true with a reason", with("reason", json!("a mismatch"))),
        ("ok as text", with("ok", json!("true"))),
        ("a reason that is not a string", with("reason", json!(1))),
        ("a reason over 500 characters", {
            let mut record = with("ok", json!(false));
            record["reason"] = json!("r".repeat(501));
            record
        }),
        ("a bundle that is not a string", with("bundle", json!(1))),
        ("no bundle", with("bundle", Value::Null)),
        ("a negative count", with("mismatches", json!(-1))),
        ("a fractional count", with("duration_ms", json!(1.5))),
        ("an event id as text", with("bundle_event_id", json!("41"))),
    ] {
        assert!(store.backup_record(&record).is_err(), "{what}: {record}");
    }
    assert_eq!(
        control_records(&mut client, "backup.drilled.transcripts"),
        1
    );
    assert_eq!(
        control_records(&mut client, "backup.drilled.missions"),
        2,
        "nothing refused was recorded"
    );
}

/// Objective 217: the notes folder's drill (`backup.drilled.sync`) is
/// accepted in the drill record's shape and refused outside it, as the
/// other drill records; the reason is masked as theirs is.
#[test]
fn a_notes_drill_verdict_is_a_backup_drilled_sync_record_of_the_drill_shape() {
    let (store, schema) = store("backup_drilled_sync");
    let mut client = admin(&schema);
    store
        .backup_record(&drilled("backup.drilled.sync"))
        .expect("a passed notes drill");
    let mut failed = drilled("backup.drilled.sync");
    failed["ok"] = json!(false);
    failed["reason"] = json!("sync: 1 mismatch(es) older than bundle k; the first: sync/a.md");
    failed["mismatches"] = json!(1);
    store.backup_record(&failed).expect("a failed notes drill");
    let with = |field: &str, value: Value| {
        let mut record = drilled("backup.drilled.sync");
        record[field] = value;
        record
    };
    for (what, record) in [
        ("another field", with("files", json!(3))),
        ("a controllers field", with("controller", json!("spawn"))),
        ("ok false with no reason", with("ok", json!(false))),
        ("ok true with a reason", with("reason", json!("a mismatch"))),
        ("a reason over 500 characters", {
            let mut record = with("ok", json!(false));
            record["reason"] = json!("r".repeat(501));
            record
        }),
        ("no bundle", with("bundle", Value::Null)),
        ("a negative count", with("mismatches", json!(-1))),
        ("a fractional duration", with("duration_ms", json!(1.5))),
    ] {
        assert!(store.backup_record(&record).is_err(), "{what}: {record}");
    }
    assert_eq!(
        control_records(&mut client, "backup.drilled.sync"),
        2,
        "nothing refused was recorded"
    );
}

fn controllers_drilled() -> Value {
    json!({
        "type": "backup.drilled.controllers", "ok": true, "reason": "",
        "bundle": "", "bundle_event_id": 0, "mismatches": 0, "duration_ms": 120,
        "run": 1_763_190_000_000i64, "controller": "spawn", "missions": [4],
        "source": "/home/spawn/.local/share/spawn-rsi/spawn.git",
        "current": {"commit": "8adefdef".repeat(5), "live": {"ok": true, "files": 796, "differing": []}},
        "prev": {"commit": "53a3be15".repeat(5), "live": {"ok": true, "files": 792, "differing": []}},
        "left_out": [".controller", ".deps", "web/node_modules"],
    })
}

/// Objective 198: the controllers' drill records one
/// `backup.drilled.controllers` per controller, with the drill record's
/// fields and its own (run, controller, missions, source, current, prev,
/// left_out), each of its kind. A record with no previous tree, and a
/// failed one, are stored; another type with these keys, an unknown field,
/// and each own field of the wrong kind are refused.
#[test]
fn a_controllers_drill_record_takes_its_own_fields_each_of_its_kind() {
    let (store, schema) = store("backup_drilled_controllers");
    let mut client = admin(&schema);
    store
        .backup_record(&controllers_drilled())
        .expect("a passed controllers drill");
    let mut first = controllers_drilled();
    first["prev"] = Value::Null;
    first["controller"] = json!("flake-doctor");
    store.backup_record(&first).expect("no previous tree");
    let mut failed = controllers_drilled();
    failed["ok"] = json!(false);
    failed["reason"] = json!("spawn: current 8adefdef: 1 of 796 live paths differ");
    failed["mismatches"] = json!(1);
    failed["current"]["live"] =
        json!({"ok": false, "files": 796, "differing": ["README.md: other bytes"]});
    store
        .backup_record(&failed)
        .expect("a failed controllers drill");
    let with = |path: &[&str], value: Value| {
        let mut record = controllers_drilled();
        let mut at = &mut record;
        for key in &path[..path.len() - 1] {
            at = &mut at[*key];
        }
        at[path[path.len() - 1]] = value;
        record
    };
    let mut missions_type = controllers_drilled();
    missions_type["type"] = json!("backup.drilled.missions");
    let mut no_prev = controllers_drilled();
    no_prev.as_object_mut().expect("an object").remove("prev");
    for (what, record) in [
        ("another drill's type with these keys", missions_type),
        ("another field", with(&["held"], json!([]))),
        ("no prev", no_prev),
        ("run as text", with(&["run"], json!("1763190000000"))),
        ("a fractional run", with(&["run"], json!(1.5))),
        (
            "a controller that is not a string",
            with(&["controller"], json!(1)),
        ),
        ("an empty controller", with(&["controller"], json!(""))),
        ("missions not an array", with(&["missions"], json!(4))),
        ("a mission as text", with(&["missions"], json!(["4"]))),
        (
            "a source that is not a string",
            with(&["source"], json!(null)),
        ),
        ("current null", with(&["current"], Value::Null)),
        ("current a string", with(&["current"], json!("8adefdef"))),
        ("prev a string", with(&["prev"], json!("53a3be15"))),
        (
            "a commit that is not a string",
            with(&["current", "commit"], json!(1)),
        ),
        (
            "another tree field",
            with(&["current", "held"], json!(true)),
        ),
        (
            "live ok as text",
            with(&["current", "live", "ok"], json!("true")),
        ),
        (
            "live files negative",
            with(&["prev", "live", "files"], json!(-1)),
        ),
        (
            "differing not an array",
            with(&["current", "live", "differing"], json!("a")),
        ),
        (
            "a differing path not a string",
            with(&["current", "live", "differing"], json!([1])),
        ),
        ("left_out not an array", with(&["left_out"], json!(".deps"))),
        (
            "a left_out entry not a string",
            with(&["left_out"], json!([1])),
        ),
        ("ok true with a reason", with(&["reason"], json!("differs"))),
    ] {
        assert!(store.backup_record(&record).is_err(), "{what}: {record}");
    }
    assert_eq!(
        control_records(&mut client, "backup.drilled.controllers"),
        3,
        "nothing refused was recorded"
    );
    assert_eq!(control_records(&mut client, "backup.drilled.missions"), 0);
}

/// Every drill record shape the store takes through spawn_backup_record
/// (a pass, a failure, a compared controller with a previous tree and with
/// none, a controllers run that could not restore the backup) reads back
/// from the ledger through the strict parse, as its closed outcome
/// (`DrillOutcome`, `ControllersDrillOutcome`): what the store takes, the
/// typed reading takes.
#[test]
fn every_drill_record_the_store_takes_reads_back_strictly_as_its_outcome() {
    use spawn_core::control::{ControlRecord, ControllersDrillOutcome, DrillOutcome};

    let (store, _schema) = store("backup_drilled_strict");
    let mut failed = drilled("backup.drilled.missions");
    failed["ok"] = json!(false);
    failed["reason"] = json!("missions: 2 older than bundle");
    failed["mismatches"] = json!(2);
    let mut no_prev = controllers_drilled();
    no_prev["prev"] = Value::Null;
    let unrestorable = json!({
        "type": "backup.drilled.controllers", "ok": false, "reason": "no bundle to restore",
        "bundle": "", "bundle_event_id": 0, "mismatches": 0, "duration_ms": 5,
    });
    for record in [
        drilled("backup.drilled.transcripts"),
        drilled("backup.drilled.sync"),
        failed,
        controllers_drilled(),
        no_prev,
        unrestorable,
    ] {
        store.backup_record(&record).expect("the store takes it");
    }
    let (stream, _) = store
        .control_record_stream(true)
        .expect("every stored record reads strictly");
    let says = |outcome: DrillOutcome<'_>| match outcome {
        DrillOutcome::Passed => "passed".to_string(),
        DrillOutcome::Failed { reason } => format!("failed: {reason}"),
    };
    let read: Vec<String> = stream
        .iter()
        .filter_map(|(_, _, record)| match record {
            ControlRecord::BackupDrilledTranscripts(r)
            | ControlRecord::BackupDrilledMissions(r)
            | ControlRecord::BackupDrilledSync(r) => Some(says(r.outcome().expect("reads"))),
            ControlRecord::BackupDrilledControllers(r) => Some(match r.outcome().expect("reads") {
                ControllersDrillOutcome::Compared {
                    controller,
                    outcome,
                } => {
                    format!("{controller} {}", says(outcome))
                }
                ControllersDrillOutcome::Unrestorable { reason } => {
                    format!("unrestorable: {reason}")
                }
            }),
            _ => None,
        })
        .collect();
    assert_eq!(
        read,
        [
            "passed",
            "passed",
            "failed: missions: 2 older than bundle",
            "spawn passed",
            "spawn passed",
            "unrestorable: no bundle to restore",
        ]
    );
}
