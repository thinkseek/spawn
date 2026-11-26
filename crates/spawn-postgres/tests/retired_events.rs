//! Retired event kinds (`RETIRED_EVENTS`, #113 item 4): historical rows are
//! skipped with one warning per name without deleting or corrupting existing
//! rows, while active event names stay strictly disjoint from retired ones
//! and unlisted unknown names remain hard corruption errors.

use std::collections::BTreeSet;

use spawn_core::event::{ACTIVE_EVENT_NAMES, EventKind};
use spawn_core::id::{AgentId, SessionId};
use spawn_core::port::{EventLog, StoreError};
use spawn_postgres::RETIRED_EVENTS;

mod common;
use common::{admin, store};

#[test]
fn retired_events_are_disjoint_from_active_event_names() {
    let active: BTreeSet<&str> = ACTIVE_EVENT_NAMES.iter().copied().collect();
    assert_eq!(
        active.len(),
        ACTIVE_EVENT_NAMES.len(),
        "duplicate active event name"
    );
    let mut retired = BTreeSet::new();
    for name in RETIRED_EVENTS {
        assert!(retired.insert(*name), "duplicate retired event {name}");
        assert!(
            !active.contains(name),
            "retired event {name} overlaps active EventKind"
        );
        let wire = serde_json::json!({ "name": name, "detail": {} });
        assert!(
            serde_json::from_value::<EventKind>(wire).is_err(),
            "retired event {name} must not deserialize as EventKind"
        );
    }
}

#[test]
fn retired_events_in_history_are_skipped_without_deleting_rows_while_unknown_fails() {
    let (pg, schema) = store("retired_events_skip");
    let mut client = admin(&schema);
    let agent = AgentId::new("agent_retired_test".to_string()).expect("agent");
    let session = SessionId::new("session_retired_test".to_string()).expect("session");

    pg.append(spawn_core::event::NewEvent {
        agent_id: agent.clone(),
        session_id: session.clone(),
        execution_id: None,
        source: spawn_core::event::Source::Claude,
        source_event_id: Some("active-1".to_string()),
        kind: EventKind::MessageReceived,
    })
    .expect("append active 1");

    // Insert historical retired rows directly (preserving legacy records intact).
    for (seq_offset, (name, detail)) in [
        (
            "session.native_ended",
            serde_json::json!({"reason": "exit"}),
        ),
        ("notice.typed", serde_json::json!({"through": 1})),
        ("notice.received", serde_json::json!(null)),
        (
            "notification.received",
            serde_json::json!({"notification_type": "idle_prompt"}),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let kind = match detail {
            serde_json::Value::Null => serde_json::json!({ "name": name }),
            other => serde_json::json!({ "name": name, "detail": other }),
        };
        client
            .execute(
                "insert into spawn_events (agent_id, session_id, source, kind, session_seq, recorded_at_ms)
                 values ($1, $2, 'claude', $3, $4, 2000)",
                &[
                    &agent.as_str(),
                    &session.as_str(),
                    &kind,
                    &(2 + i64::try_from(seq_offset).expect("seq")),
                ],
            )
            .expect("insert retired historical row");
    }

    pg.append(spawn_core::event::NewEvent {
        agent_id: agent.clone(),
        session_id: session.clone(),
        execution_id: None,
        source: spawn_core::event::Source::Claude,
        source_event_id: Some("active-2".to_string()),
        kind: EventKind::TurnStopped {
            input_tokens: None,
            output_tokens: None,
        },
    })
    .expect("append active 2");

    // Global log read skips all 4 retired rows cleanly and returns both active events.
    let read = pg
        .read_global(spawn_core::event::GlobalPosition(0), 10)
        .expect("read_global skips retired events");
    let names: Vec<&str> = read.iter().map(|r| r.event.kind.name()).collect();
    assert_eq!(names, ["message.received", "turn.stopped"]);

    // All 6 rows remain untouched in PostgreSQL (retired rows were never deleted).
    let raw_count: i64 = client
        .query_one("select count(*) from spawn_events", &[])
        .expect("count")
        .get(0);
    assert_eq!(raw_count, 6);

    // An unlisted unknown event name still causes StoreError::Corrupt.
    client
        .execute(
            "insert into spawn_events (agent_id, session_id, source, kind, session_seq, recorded_at_ms)
             values ($1, $2, 'claude', '{\"name\":\"invented.corrupt_event\"}'::jsonb, 99, 3000)",
            &[&agent.as_str(), &session.as_str()],
        )
        .expect("insert unknown row");
    let err = pg
        .read_global(spawn_core::event::GlobalPosition(0), 20)
        .expect_err("unlisted unknown event must remain Corrupt");
    assert!(matches!(err, StoreError::Corrupt(_)), "{err:?}");
}
