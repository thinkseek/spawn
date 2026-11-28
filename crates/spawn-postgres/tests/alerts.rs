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
use spawn_core::event::{EventKind, NewEvent, SessionSeq, Source};
use spawn_core::id::{AgentId, SessionId};
use spawn_core::observation::{Confidence, Observation, SemanticState};
use spawn_core::port::EventLog;
use spawn_postgres::{PostgresStore, role_url_with_passfile};

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

fn updated(key: &str, kind: &str, subject: &str, text: &str, old: i64) -> Value {
    json!({
        "type": "alert.updated", "key": key, "kind": kind, "subject": subject,
        "text": text, "evidence": {"old": old, "fired_old": 1},
    })
}

/// Objective 219: an `alert.updated` sets an open alert's text and evidence
/// only (its fire, state and verdict stay), and is refused, writing nothing,
/// for another kind or subject, an empty text, a key never fired, a notice
/// and a cleared alert. A refold gives the same rows.
#[test]
fn an_open_alert_is_updated_in_place_and_nothing_else_is() {
    let (store, schema) = store("alerts_updated");
    let mut client = admin(&schema);
    let fired_id = store
        .alert_record(&fired("memory_low:1", false))
        .expect("fire");
    let before = alert(&mut client, "memory_low:1");
    store
        .alert_record(&updated(
            "memory_low:1",
            "memory_low",
            "host",
            "MemAvailable is 900 MiB, under 4096",
            2,
        ))
        .expect("an update of the open alert");
    let row = alert(&mut client, "memory_low:1");
    assert_eq!(row["text"], "MemAvailable is 900 MiB, under 4096");
    assert_eq!(row["evidence"], json!({"old": 2, "fired_old": 1}));
    for field in [
        "state",
        "fired_event_id",
        "fired_at_ms",
        "verdict",
        "kind",
        "subject",
    ] {
        assert_eq!(row[field], before[field], "{field} unchanged");
    }
    assert_eq!(row["fired_event_id"], fired_id);

    store
        .alert_record(&fired("task_state:7:10", true))
        .expect("a notice");
    store
        .alert_record(&fired("memory_low:2", false))
        .expect("fire");
    store.alert_record(&cleared("memory_low:2")).expect("clear");
    let rows = alert_rows(&mut client);
    for (record, why) in [
        (
            updated("memory_low:1", "disk_low", "host", "other kind", 3),
            "another kind",
        ),
        (
            updated(
                "memory_low:1",
                "memory_low",
                "elsewhere",
                "other subject",
                3,
            ),
            "another subject",
        ),
        (
            updated("memory_low:1", "memory_low", "host", " ", 3),
            "no text",
        ),
        (
            json!({"type": "alert.updated", "key": "memory_low:1", "kind": "memory_low",
                   "subject": "host", "evidence": {"old": 3}}),
            "no text at all",
        ),
        (
            updated("memory_low:9", "memory_low", "host", "never fired", 3),
            "a key never fired",
        ),
        (
            updated("task_state:7:10", "memory_low", "host", "a notice", 3),
            "a notice",
        ),
        (
            updated("memory_low:2", "memory_low", "host", "cleared", 3),
            "a cleared alert",
        ),
    ] {
        assert!(store.alert_record(&record).is_err(), "{why} is refused");
        assert_eq!(alert_rows(&mut client), rows, "{why}: nothing written");
    }
    assert_eq!(
        control_types(&mut client, "memory_low:1"),
        ["alert.fired", "alert.updated"],
        "only the update that was taken"
    );

    // A refold of the log gives the same rows.
    client
        .batch_execute("select spawn_control_refold()")
        .expect("refold the control stream");
    assert_eq!(alert_rows(&mut client), rows);
}

/// How many `alert.judged` records the log holds.
fn judgements(client: &mut Client) -> i64 {
    client
        .query_one(
            "select count(*) from spawn_events
              where kind -> 'detail' -> 'record' ->> 'type' = 'alert.judged'",
            &[],
        )
        .expect("count the judgements")
        .get(0)
}

/// Each alert's key and outcome, as `/api/alerts` gives them.
fn outcomes(store: &PostgresStore) -> Vec<(String, Value)> {
    store
        .alerts(0)
        .expect("the window")
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| {
            (
                row["key"].as_str().expect("a key").to_string(),
                row["outcome"].clone(),
            )
        })
        .collect()
}

/// The table, row by row.
fn alert_rows(client: &mut Client) -> Value {
    client
        .query_one(
            "select jsonb_agg(to_jsonb(a) order by a.key) from spawn_alerts a",
            &[],
        )
        .expect("read the table")
        .get(0)
}

/// Objective 092: the owner judges an alert that should not have been
/// raised (`alert.judged` {key, verdict}). An open alert is cleared by it,
/// its cleared evidence the verdict; a cleared one keeps its evidence and
/// gains the verdict. Another verdict, a key no alert has, a notice and an
/// alert judged already are refused and write nothing. `/api/alerts`' rows
/// carry the outcome, and a refold gives the same rows.
#[test]
fn the_owner_judges_an_alert_and_every_alert_ends_with_an_outcome() {
    let (store, schema) = store("alerts_judged");
    let mut client = admin(&schema);
    for key in ["memory_low:1", "memory_low:2", "memory_low:3"] {
        store.alert_record(&fired(key, false)).expect("fire");
    }
    store
        .alert_record(&fired("task_state:7:10", true))
        .expect("a notice");
    store
        .alert_record(&cleared("memory_low:1"))
        .expect("clear one");

    // An open alert judged: cleared by the judgement.
    let id = store
        .alert_judge("memory_low:2", "duplicate")
        .expect("judge an open alert");
    let row = alert(&mut client, "memory_low:2");
    assert_eq!(row["state"], "cleared");
    assert_eq!(row["verdict"], "duplicate");
    assert_eq!(row["cleared_event_id"], id);
    assert_eq!(row["cleared_evidence"], json!({"judged": "duplicate"}));
    assert_eq!(row["judged_at_ms"], row["cleared_at_ms"]);

    // A cleared alert judged: its clearing stays, the verdict is added.
    let before = alert(&mut client, "memory_low:1");
    store
        .alert_judge("memory_low:1", "false")
        .expect("judge a cleared alert");
    let row = alert(&mut client, "memory_low:1");
    assert_eq!(row["verdict"], "false");
    assert_eq!(row["state"], "cleared");
    assert_eq!(row["cleared_event_id"], before["cleared_event_id"]);
    assert_eq!(row["cleared_evidence"], json!({"measured_mb": 9000}));

    // Refused, writing nothing.
    assert_eq!(judgements(&mut client), 2);
    for (key, verdict, why) in [
        ("memory_low:3", "wrong", "another verdict"),
        ("memory_low:9", "false", "a key no alert has"),
        ("task_state:7:10", "stale", "a notice"),
        ("memory_low:2", "stale", "an alert judged already"),
        (
            "memory_low:1",
            "false",
            "an alert judged already, the same verdict",
        ),
    ] {
        assert!(store.alert_judge(key, verdict).is_err(), "{why} is refused");
    }
    assert!(
        store
            .alert_record(&json!({"type": "alert.judged", "key": "memory_low:3",
                                  "verdict": "false", "why": "extra"}))
            .is_err(),
        "a judgement with another field is refused"
    );
    assert_eq!(
        judgements(&mut client),
        2,
        "the refused judgements wrote nothing"
    );
    assert_eq!(alert(&mut client, "memory_low:3")["state"], "open");
    assert!(
        store.alert_record(&cleared("memory_low:2")).is_err(),
        "a judged alert has nothing left to clear"
    );

    // The outcomes, as /api/alerts gives them.
    let mut got = outcomes(&store);
    got.sort_by(|left, right| left.0.cmp(&right.0));
    assert_eq!(
        got,
        [
            ("memory_low:1".to_string(), json!("false")),
            ("memory_low:2".to_string(), json!("duplicate")),
            ("memory_low:3".to_string(), Value::Null),
            ("task_state:7:10".to_string(), Value::Null),
        ]
    );
    store
        .alert_record(&cleared("memory_low:3"))
        .expect("clear the last");
    assert!(
        outcomes(&store).contains(&("memory_low:3".to_string(), json!("cleared"))),
        "cleared by its condition"
    );

    // The fold keeps the verdict and its time; a refold gives the same rows.
    let folded = alert_rows(&mut client);
    client
        .batch_execute("select spawn_control_refold()")
        .expect("refold the control stream");
    assert_eq!(alert_rows(&mut client), folded);
}

/// Objective 148: the owner judges a need right (a real need). It changes
/// nothing else: an open need judged right stays open, then clears by its
/// condition keeping the verdict; a cleared one keeps its clearing. Once,
/// never on a notice. The judged read counts the needs fired since its
/// bound, how many were judged and how many right: notices never, and
/// nothing fired before the bound. A refold gives the same rows.
#[test]
fn a_need_judged_right_stays_as_it_was_and_is_counted() {
    let (store, schema) = store("alerts_right");
    let mut client = admin(&schema);
    for key in ["memory_low:1", "memory_low:2", "memory_low:3"] {
        store.alert_record(&fired(key, false)).expect("fire");
    }
    store
        .alert_record(&fired("task_state:7:10", true))
        .expect("a notice");
    store
        .alert_record(&cleared("memory_low:2"))
        .expect("clear one");

    // An open need judged right stays open, no clearing.
    store
        .alert_judge("memory_low:1", "right")
        .expect("judge an open need right");
    let row = alert(&mut client, "memory_low:1");
    assert_eq!(row["state"], "open");
    assert_eq!(row["verdict"], "right");
    assert!(row["judged_at_ms"].is_i64(), "{row}");
    assert_eq!(row["cleared_event_id"], Value::Null);
    assert_eq!(row["cleared_evidence"], Value::Null);
    assert!(
        outcomes(&store).contains(&("memory_low:1".to_string(), json!("right"))),
        "its outcome is its verdict while open"
    );
    // Its condition clears it later, and it keeps its verdict.
    let cleared_id = store
        .alert_record(&cleared("memory_low:1"))
        .expect("its condition clears it");
    let row = alert(&mut client, "memory_low:1");
    assert_eq!(row["state"], "cleared");
    assert_eq!(row["verdict"], "right");
    assert_eq!(row["cleared_event_id"], cleared_id);
    assert_eq!(row["cleared_evidence"], json!({"measured_mb": 9000}));

    // A cleared need judged right keeps its clearing.
    let before = alert(&mut client, "memory_low:2");
    store
        .alert_judge("memory_low:2", "right")
        .expect("judge a cleared need right");
    let row = alert(&mut client, "memory_low:2");
    assert_eq!(row["verdict"], "right");
    assert_eq!(row["state"], "cleared");
    assert_eq!(row["cleared_event_id"], before["cleared_event_id"]);
    assert_eq!(row["cleared_at_ms"], before["cleared_at_ms"]);
    assert_eq!(row["cleared_evidence"], before["cleared_evidence"]);

    // Once, never on a notice, never for a key no alert has.
    assert_eq!(judgements(&mut client), 2);
    for (key, verdict, why) in [
        ("memory_low:1", "right", "judged right already"),
        (
            "memory_low:1",
            "false",
            "judged right already, another verdict",
        ),
        ("task_state:7:10", "right", "a notice"),
        ("memory_low:9", "right", "a key no alert has"),
    ] {
        assert!(store.alert_judge(key, verdict).is_err(), "{why} is refused");
    }
    assert_eq!(judgements(&mut client), 2, "the refused wrote nothing");
    assert_eq!(
        alert(&mut client, "task_state:7:10")["verdict"],
        Value::Null
    );

    // The judged read: three needs fired, two judged, both right; the
    // notice never counted.
    assert_eq!(
        store.needs_judged(0).expect("the judged read"),
        json!({"fired": 3, "judged": 2, "right": 2})
    );
    store
        .alert_judge("memory_low:3", "false")
        .expect("judge one false");
    assert_eq!(alert(&mut client, "memory_low:3")["state"], "cleared");
    assert_eq!(
        store.needs_judged(0).expect("the judged read"),
        json!({"fired": 3, "judged": 3, "right": 2})
    );
    // Nothing fired after the bound: none.
    assert_eq!(
        store.needs_judged(i64::MAX).expect("the judged read"),
        json!({"fired": 0, "judged": 0, "right": 0})
    );

    // A refold gives the same rows.
    let folded = alert_rows(&mut client);
    client
        .batch_execute("select spawn_control_refold()")
        .expect("refold the control stream");
    assert_eq!(alert_rows(&mut client), folded);

    // A need fired before the bound is not counted.
    let now = store.now_ms().expect("the clock");
    client
        .execute(
            "update spawn_alerts set fired_at_ms = fired_at_ms - 8 * 86400000
              where key = 'memory_low:1'",
            &[],
        )
        .expect("age one need");
    assert_eq!(
        store
            .needs_judged(now - 7 * 86_400_000)
            .expect("the judged read"),
        json!({"fired": 2, "judged": 2, "right": 1})
    );
}

/// The records are the owner's: a restricted login may read the table (the
/// supervisor's `spawn alerts`) and may not record an alert. Its read
/// carries an open agent_stuck alert's `scored_at_ms`, through a view: the
/// login has no grant on spawn_events (batch 14's pg failure, rev-e's
/// judge-down read the table directly).
#[test]
fn only_the_owner_records_alerts() {
    let (store, schema) = store("alerts_owner");
    let mut observation = Observation::basic(
        SemanticState::Unknown,
        None,
        SessionSeq(1),
        Confidence::new(1.0).expect("valid confidence"),
        "jev",
    );
    observation.stuck = Some(0.9);
    store
        .append(NewEvent {
            agent_id: AgentId::new("agent_one").expect("valid agent"),
            session_id: SessionId::new("session_stuck").expect("valid session"),
            execution_id: None,
            source: Source::Observer,
            source_event_id: None,
            kind: EventKind::ObservationRecorded { observation },
        })
        .expect("a scored judgement");
    store
        .alert_record(&json!({
            "type": "alert.fired", "key": "agent_stuck:session_stuck:1", "kind": "agent_stuck",
            "subject": "session_stuck", "evidence": {}, "text": "stuck", "notice": false,
        }))
        .expect("fire");
    let login = role_url_with_passfile(&schema_url(&schema), "spawn_supervisor", None)
        .expect("the login's URL");
    let supervisor =
        PostgresStore::connect_with_options(&login, &spawn_postgres::ConnectOptions::default())
            .expect("log in");
    let refused = supervisor
        .alert_record(&fired("memory_low:3", false))
        .expect_err("a restricted login records no alert");
    assert!(
        supervisor
            .alert_judge("agent_stuck:session_stuck:1", "false")
            .is_err(),
        "a restricted login judges no alert"
    );
    // Either the function's grant (revoked from everyone but the owner) or
    // its own caller check refuses it.
    let message = refused.to_string();
    assert!(
        message.contains("permission denied for function spawn_alert_record")
            || message.contains("may not record alerts"),
        "{refused}"
    );
    let alerts = supervisor.alerts(0).expect("read the alerts");
    let stuck = alerts
        .as_array()
        .into_iter()
        .flatten()
        .find(|alert| alert["kind"] == "agent_stuck")
        .unwrap_or_else(|| panic!("the stuck alert: {alerts}"));
    assert!(
        stuck["scored_at_ms"].as_i64().is_some_and(|at| at > 0),
        "the supervisor reads when the session was scored: {stuck}"
    );
}

/// P12: the `source_away` reading lists the open and active missions'
/// sources, and the dispatcher's news holds each `source_away` record (fired
/// and cleared, nothing of another kind) until a digest carries its tag.
#[test]
fn the_sources_away_are_listed_and_are_news_until_told() {
    let (store, schema) = store("alerts_source_away");
    let mut client = admin(&schema);
    let open = store
        .mission_add(
            "open",
            &json!(["/srv/open.git", "https://example.com/x.git"]),
            "human",
        )
        .expect("add");
    let done = store
        .mission_add("done", &json!(["/srv/done.git"]), "human")
        .expect("add");
    client
        .execute(
            "update mission_missions set state = 'done' where id = $1",
            &[&done],
        )
        .expect("close one");
    assert_eq!(
        store.alert_mission_sources().expect("sources"),
        json!([{"mission": open, "sources": ["/srv/open.git", "https://example.com/x.git"]}]),
        "the open mission only, every source as recorded"
    );

    let subject = format!("mission #{open}");
    let away = json!({
        "type": "alert.fired", "key": "source_away:1:100", "kind": "source_away",
        "subject": subject, "evidence": {"source": "/srv/open.git", "present": false},
        "text": "/srv/open.git is not a directory on this host", "notice": false,
    });
    let fired_id = store.alert_record(&away).expect("fire");
    store
        .alert_record(&fired("memory_low:1", false))
        .expect("another kind");
    let back = json!({
        "type": "alert.cleared", "key": "source_away:1:100", "kind": "source_away",
        "subject": subject, "evidence": {"source": "/srv/open.git", "present": true},
    });
    let cleared_id = store.alert_record(&back).expect("clear");
    let news = store.source_away_news().expect("news");
    let seen: Vec<(Value, Value)> = news
        .as_array()
        .expect("a list")
        .iter()
        .map(|record| (record["type"].clone(), record["event_id"].clone()))
        .collect();
    assert_eq!(
        seen,
        [
            (json!("alert.fired"), json!(fired_id)),
            (json!("alert.cleared"), json!(cleared_id))
        ],
        "both records, oldest first, nothing of memory_low: {news}"
    );
    assert_eq!(news[0]["evidence"]["source"], "/srv/open.git");
    assert!(news[0]["at_ms"].is_i64());

    client
        .execute(
            "insert into spawn_commands (principal, idempotency_key, agent_id, session_id, action,
                                         state, succeeded, created_at_ms)
             values ('spawn-dispatch', 'digest-1', 'agent_supervisor', 'session_supervisor',
                     jsonb_build_object('type', 'input', 'text', 'x',
                                        'tags', jsonb_build_array($1::text)),
                     'finished', true, 1)",
            &[&format!("host source-away-{fired_id}")],
        )
        .expect("a digest that told the first");
    let news = store.source_away_news().expect("news");
    assert_eq!(news.as_array().map(Vec::len), Some(1), "{news}");
    assert_eq!(
        news[0]["event_id"], cleared_id,
        "only the end is left to tell"
    );
}

/// P22: the `controller_behind` reading lists each gated mission, open or
/// active and not halted, that has a reported task, with its gate, the
/// source it lands on and the owner's latest landing there (none here); not
/// a mission without a gate, a halted or closed one, or one with nothing
/// reported.
#[test]
fn the_controller_reading_lists_the_gated_missions_with_reported_work() {
    let (store, schema) = store("alerts_controllers");
    let mut client = admin(&schema);
    let gate = "/live/controllers/spawn/rsi/scenarios";
    let mut mission = |name: &str, gated: bool, reported: bool| -> i64 {
        let id = store
            .mission_add(
                name,
                &json!([format!("/srv/{name}.git"), "/srv/second.git"]),
                "human",
            )
            .expect("add");
        if gated {
            store
                .mission_configure(id, Some(gate), "gate", None, None)
                .expect("configure");
        }
        let state = if reported { "reported" } else { "delivered" };
        client
            .execute(
                "insert into mission_tasks (mission_id, kind, agent_id, session_id, brief, state,
                                            created_by, created_at_ms, updated_at_ms)
                 values ($1, 'ephemeral', 'agent_w', 'session_w', 'b', $2, 'human', 1, 1)",
                &[&id, &state],
            )
            .expect("a task");
        id
    };
    let waiting = mission("waiting", true, true);
    let _idle = mission("idle", true, false);
    let _ungated = mission("ungated", false, true);
    let halted = mission("halted", true, true);
    let closed = mission("closed", true, true);
    client
        .execute(
            "update mission_missions set halted = 'stop' where id = $1",
            &[&halted],
        )
        .expect("halt one");
    client
        .execute(
            "update mission_missions set state = 'done' where id = $1",
            &[&closed],
        )
        .expect("close one");
    let reading = store.alert_controllers().expect("the reading");
    assert!(reading["now_ms"].is_i64(), "{reading}");
    assert_eq!(
        reading["missions"],
        json!([{"mission": waiting, "gate": gate, "source": "/srv/waiting.git", "landed": null}]),
        "{reading}"
    );
}

#[test]
fn alert_clock_mirror_and_control_hold_back_behind_in_flight_control_writer() {
    let (store, schema) = store("alerts_xid8_ctrl_hold");
    let baseline = store.watermark().expect("baseline");

    let mut conn_a = admin(&schema);
    let mut conn_b = admin(&schema);

    let mut tx_a = conn_a.transaction().expect("begin tx_a");
    let id_a: i64 = tx_a
        .query_one(
            "select spawn_control_record($1)",
            &[&json!({"type": "mission.added", "id": 91, "text": "in-flight mission", "origin": "human"})],
        )
        .expect("in-flight control record")
        .get(0);

    let worker_kind = json!({"name": "session.noted", "detail": {"text": "later commit"}});
    let id_b: i64 = conn_b
        .query_one(
            "select out_id from spawn_append_trusted('agent_w', 'session_ctrl_other', null, 'bridge', null, $1)",
            &[&worker_kind],
        )
        .expect("worker append")
        .get(0);
    assert!(id_b > id_a, "expected id_b ({id_b}) > id_a ({id_a})");

    let (clock_wm, _) = store.alert_clock().expect("clock");
    assert!(
        clock_wm < id_a && clock_wm <= baseline,
        "alert_clock ({clock_wm}) jumped past uncommitted control id_a={id_a}"
    );
    let mirror = store.alert_mirror().expect("mirror");
    let mirror_wm = mirror["watermark"].as_i64().expect("mirror watermark");
    assert!(
        mirror_wm < id_a && mirror_wm <= baseline,
        "alert_mirror watermark ({mirror_wm}) jumped past uncommitted control id_a={id_a}"
    );
    let batch_open = store.alert_control(baseline, 100).expect("control batch");
    assert_eq!(
        batch_open["records"].as_array().map(Vec::len),
        Some(0),
        "alert_control must not skip past uncommitted control id_a: {batch_open}"
    );

    tx_a.commit().expect("commit tx_a");
    let (clock_after, _) = store.alert_clock().expect("clock after");
    assert_eq!(clock_after, id_b);
    let batch_after = store.alert_control(baseline, 100).expect("control after");
    let records = batch_after["records"].as_array().expect("records array");
    assert_eq!(records.len(), 1, "{batch_after}");
    assert_eq!(records[0]["event_id"], id_a);
}

#[test]
fn alert_mirror_advances_past_committed_control_with_unrelated_in_flight_worker() {
    let (store, schema) = store("alerts_xid8_mirror_no_double");
    let mut conn_a = admin(&schema);
    let mut conn_b = admin(&schema);

    let mut tx_a = conn_a.transaction().expect("begin tx_a");
    let worker_kind = json!({"name": "session.noted", "detail": {"text": "in-flight worker"}});
    let id_worker: i64 = tx_a
        .query_one(
            "select out_id from spawn_append_trusted('agent_w', 'session_worker_open', null, 'bridge', null, $1)",
            &[&worker_kind],
        )
        .expect("in-flight worker append")
        .get(0);

    let id_control: i64 = conn_b
        .query_one(
            "select spawn_control_record($1)",
            &[&json!({"type": "mission.added", "id": 92, "text": "committed mission", "origin": "human"})],
        )
        .expect("committed control record")
        .get(0);
    assert!(
        id_control > id_worker,
        "expected id_control ({id_control}) > id_worker ({id_worker})"
    );

    let mirror = store.alert_mirror().expect("mirror");
    let mirror_wm = mirror["watermark"].as_i64().expect("mirror watermark");
    assert!(
        mirror_wm < id_worker,
        "mirror watermark ({mirror_wm}) must remain at safe frontier below in-flight worker ({id_worker})"
    );
    let snapshot = mirror["snapshot"]
        .as_str()
        .expect("mirror must export pg_current_snapshot text");
    assert!(
        mirror["snapshot_xmax"].as_str().is_some(),
        "mirror must export snapshot_xmax: {mirror}"
    );
    let has_mission = mirror["missions"]
        .as_array()
        .is_some_and(|missions| missions.iter().any(|m| m["id"] == 92));
    assert!(
        has_mission,
        "mission 92 must be present in mirror: {mirror}"
    );

    tx_a.commit().expect("commit tx_a");
    let after_commit = store
        .alert_control_excluding(mirror_wm, 100, Some(snapshot))
        .expect("subsequent alert_control_excluding must skip snapshot-visible mission 92");
    assert_eq!(
        after_commit["records"].as_array().map(Vec::len),
        Some(0),
        "already-folded mission 92 must not be re-fed when excluded by mirror snapshot: {after_commit}"
    );
}

#[test]
fn alert_archives_holds_back_behind_in_flight_writer_and_delivers_both_on_commit() {
    let (store, schema) = store("alerts_xid8_archives_hold");
    let baseline = store.watermark().expect("baseline");

    let mut conn_a = admin(&schema);
    let mut conn_b = admin(&schema);

    let archive_kind = |native: &str| {
        json!({
            "name": "archive.failed",
            "detail": {"native": native, "reason": "disk full"}
        })
    };

    let mut tx_a = conn_a.transaction().expect("begin tx_a");
    let id_a: i64 = tx_a
        .query_one(
            "select out_id from spawn_append_trusted('agent_a', 'session_arch_a', null, 'bridge', null, $1)",
            &[&archive_kind("native_a")],
        )
        .expect("in-flight archive a")
        .get(0);

    let id_b: i64 = conn_b
        .query_one(
            "select out_id from spawn_append_trusted('agent_b', 'session_arch_b', null, 'bridge', null, $1)",
            &[&archive_kind("native_b")],
        )
        .expect("committed archive b")
        .get(0);
    assert!(id_b > id_a, "expected id_b ({id_b}) > id_a ({id_a})");

    let while_open = store.alert_archives(baseline).expect("archives while open");
    assert_eq!(
        while_open,
        json!([]),
        "alert_archives must not return id_b ahead of uncommitted id_a"
    );

    tx_a.commit().expect("commit tx_a");
    let after_commit = store
        .alert_archives(baseline)
        .expect("archives after commit");
    let ids: Vec<i64> = after_commit
        .as_array()
        .expect("array")
        .iter()
        .map(|entry| entry["event_id"].as_i64().expect("event_id"))
        .collect();
    assert_eq!(ids, vec![id_a, id_b], "{after_commit}");
}

/// Reversed `commit_xid` vs `id` order across `alert_clock_watermark`,
/// `alert_mirror`, `alert_control`, and `alert_archives`, including an
/// older open blocker transaction:
/// 1) `tx_b` allocates `x_b` first, `tx_c` allocates `x_c` (blocker), `tx_a`
///    allocates `x_a` third.
/// 2) `tx_a` appends lower-ID control + archive events, `tx_b` appends higher-ID
///    control + archive events and commits first.
/// 3) First reads see `B` at `(x_b, id_b_arch)` and never move backward when `A`
///    commits behind open blocker `C`; once `C` commits, replaying from `B`'s
///    cursor recovers all lower-ID events from `A` with zero gaps or duplicates.
fn current_xid(tx: &mut postgres::Transaction<'_>) -> u64 {
    tx.query_one("select pg_current_xact_id()::text", &[])
        .expect("xid")
        .get::<_, String>(0)
        .parse()
        .expect("parse xid")
}

fn payload_event_ids(val: &Value) -> Vec<i64> {
    val.as_array()
        .expect("array")
        .iter()
        .map(|r| r["event_id"].as_i64().expect("event_id"))
        .collect()
}

#[test]
fn alerts_reversed_xid_vs_id_order_recovers_lower_ids_without_regression() {
    let (store, schema) = store("alerts_rev_xid_order");
    let baseline = store.committed_watermark().expect("baseline");

    let (mut conn_b, mut conn_c, mut conn_a) = (admin(&schema), admin(&schema), admin(&schema));
    let mut tx_b = conn_b.transaction().expect("begin tx_b");
    let x_b = current_xid(&mut tx_b);
    let mut tx_c = conn_c.transaction().expect("begin tx_c");
    let x_c = current_xid(&mut tx_c);
    tx_c.execute(
        "select pg_advisory_xact_lock(1399957401, hashtext('blocker_alerts_c'))",
        &[],
    )
    .expect("blocker lock");
    let mut tx_a = conn_a.transaction().expect("begin tx_a");
    let x_a = current_xid(&mut tx_a);
    assert!(x_b < x_c && x_c < x_a, "x_b={x_b} x_c={x_c} x_a={x_a}");

    let archive_kind = |native: &str| json!({"name": "archive.failed", "detail": {"native": native, "reason": "reversed xid"}});
    let id_ctrl_a: i64 = tx_a
        .query_one(
            "select spawn_control_record($1)",
            &[&json!({"type": "mission.added", "id": 901, "text": "a mission", "origin": "human"})],
        )
        .expect("ctrl a")
        .get(0);
    let id_arch_a: i64 = tx_a
        .query_one(
            "select out_id from spawn_append_trusted('agent_a', 'sess_rev_arch_a', null, 'bridge', null, $1)",
            &[&archive_kind("native_rev_a")],
        )
        .expect("arch a")
        .get(0);
    let id_arch_b: i64 = tx_b
        .query_one(
            "select out_id from spawn_append_trusted('agent_b', 'sess_rev_arch_b', null, 'bridge', null, $1)",
            &[&archive_kind("native_rev_b")],
        )
        .expect("arch b")
        .get(0);
    assert!(id_ctrl_a < id_arch_a && id_arch_a < id_arch_b);

    tx_b.commit().expect("commit tx_b");
    let (clock_1, _) = store.alert_clock_watermark().expect("clock 1");
    assert_eq!(
        clock_1,
        spawn_postgres::CommittedWatermark::new(x_b, id_arch_b)
    );
    let mirror_1 = store.alert_mirror().expect("mirror 1");
    assert_eq!(mirror_1["watermark"].as_i64(), Some(id_arch_b));
    assert_eq!(
        mirror_1["watermark_xid"].as_str(),
        Some(x_b.to_string().as_str())
    );

    let ctrl_1 = store.alert_control(baseline.event_id, 100).expect("ctrl 1");
    assert!(ctrl_1["records"].as_array().is_some_and(Vec::is_empty));
    let ctrl_cursor_1 = ctrl_1["watermark"].as_i64().expect("watermark");
    assert_eq!(ctrl_cursor_1, id_arch_b);
    let arch_1 = store.alert_archives(baseline.event_id).expect("arch 1");
    assert_eq!(payload_event_ids(&arch_1), vec![id_arch_b]);

    // Step 2: A commits while blocker C stays open -> clock must NOT move backward B -> 0!
    tx_a.commit().expect("commit tx_a");
    let (clock_2, _) = store.alert_clock_watermark().expect("clock 2");
    assert_eq!(
        clock_2, clock_1,
        "watermark must not move backward while C is open"
    );

    // Step 3: Blocker C commits -> clock advances monotonically to A, and replaying from B's
    // higher event_id cursor delivers both A's lower-id control event and lower-id archive event!
    tx_c.commit().expect("commit tx_c");
    let (clock_3, _) = store.alert_clock_watermark().expect("clock 3");
    assert_eq!(
        clock_3,
        spawn_postgres::CommittedWatermark::new(x_a, id_arch_a)
    );
    assert!(clock_3 > clock_2);
    assert_eq!(
        store.alert_mirror().expect("mirror 3")["watermark"].as_i64(),
        Some(id_arch_a)
    );

    let ctrl_3 = store.alert_control(ctrl_cursor_1, 100).expect("ctrl 3");
    assert_eq!(payload_event_ids(&ctrl_3["records"]), vec![id_ctrl_a]);
    let ctrl_cursor_3 = ctrl_3["next"].as_i64().expect("next 3");
    assert_eq!(ctrl_cursor_3, id_ctrl_a);
    assert!(
        store.alert_control(ctrl_cursor_3, 100).expect("ctrl tail")["records"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );

    let arch_3 = store.alert_archives(id_arch_b).expect("arch 3");
    assert_eq!(payload_event_ids(&arch_3), vec![id_arch_a]);
    assert_eq!(
        store.alert_archives(id_arch_a).expect("arch tail"),
        json!([])
    );
}

#[test]
fn alert_mirror_and_control_deliver_early_xid_for_update_writer_and_skip_folded() {
    let (store, schema) = store("alerts_early_xid_for_update");
    let mut setup = admin(&schema);
    setup
        .query_one(
            "select spawn_control_record($1)",
            &[&json!({"type": "mission.added", "id": 960, "text": "m960", "origin": "human"})],
        )
        .expect("add 960");
    setup
        .query_one(
            "select spawn_control_record($1)",
            &[&json!({"type": "mission.added", "id": 961, "text": "m961", "origin": "human"})],
        )
        .expect("add 961");
    setup
        .batch_execute("select spawn_control_refold()")
        .expect("refold");
    let baseline = store.committed_watermark().expect("baseline");

    let (mut conn_w, mut conn_r, mut conn_s) = (admin(&schema), admin(&schema), admin(&schema));
    let mut tx_w = conn_w.transaction().expect("begin tx_w");
    let _id_w: i64 = tx_w
        .query_one(
            "select out_id from spawn_append_trusted('agent_w', 'sess_early_w', null, 'bridge', null, $1)",
            &[&json!({"name": "session.noted", "detail": {"text": "open worker"}})],
        )
        .expect("append w")
        .get(0);

    // Writer R allocates an earlier XID via FOR UPDATE without holding session_control.
    let mut tx_r = conn_r.transaction().expect("begin tx_r");
    let locked: i64 = tx_r
        .query_one(
            "select id from mission_missions where id = 960 for update",
            &[],
        )
        .expect("for update 960")
        .get(0);
    assert_eq!(locked, 960);

    // Writer S obtains a newer XID and commits mission.halted before MIRROR reads.
    let id_s: i64 = conn_s
        .query_one(
            "select spawn_control_record($1)",
            &[&json!({"type": "mission.halted", "id": 961, "reason": "s_before_start"})],
        )
        .expect("commit s")
        .get(0);

    let mirror = store.alert_mirror().expect("mirror");
    assert_eq!(mirror["watermark"].as_i64(), Some(baseline.event_id));
    let snap = mirror["snapshot"].as_str().expect("snapshot").to_string();
    let history = store
        .alert_history_in_snapshot(0, 100, Some(&snap))
        .expect("history in mirror snapshot");
    assert!(
        payload_event_ids(&history).contains(&id_s),
        "S must appear in mirror-snapshot history: {history}"
    );

    // R resumes after MIRROR and commits with (x_r, id_r) < (x_s, id_s).
    let id_r: i64 = tx_r
        .query_one(
            "select spawn_control_record($1)",
            &[&json!({"type": "mission.halted", "id": 960, "reason": "r_early_xid"})],
        )
        .expect("record r")
        .get(0);
    tx_r.commit().expect("commit r");
    assert!(id_s < id_r, "expected id_s ({id_s}) < id_r ({id_r})");

    tx_w.commit().expect("commit w");
    let batch = store
        .alert_control_excluding(baseline.event_id, 100, Some(&snap))
        .expect("control excluding mirror snapshot");
    assert_eq!(
        payload_event_ids(&batch["records"]),
        vec![id_r],
        "CONTROL must deliver early-XID writer R and skip folded S: {batch}"
    );
    let next_wm = batch["watermark"].as_i64().expect("watermark");
    let tail = store
        .alert_control_excluding(next_wm, 100, Some(&snap))
        .expect("tail check");
    assert!(
        tail["records"].as_array().is_some_and(Vec::is_empty),
        "neither R nor S may replay past {next_wm}: {tail}"
    );
}
