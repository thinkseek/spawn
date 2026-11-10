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
