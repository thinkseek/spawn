//! Retired event kinds (`RETIRED_EVENTS`, #113 item 4) and unknown future
//! dotted event kinds (expand-contract rollback tolerance): historical or
//! newer-release rows are skipped with one warning per name without deleting or
//! corrupting rows, active event names stay strictly disjoint from retired ones,
//! and malformed rows or corrupt known-event payloads fail closed with
//! [`StoreError::Corrupt`].

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
        assert!(
            !EventKind::is_known_name(name),
            "EventKind::is_known_name must reject retired event {name}"
        );
        let wire = serde_json::json!({ "name": name, "detail": {} });
        assert!(
            serde_json::from_value::<EventKind>(wire).is_err(),
            "retired event {name} must not deserialize as EventKind"
        );
    }
}

fn assert_corrupt_rows_rejected(
    pg: &dyn EventLog,
    client: &mut postgres::Client,
    agent: &AgentId,
    session: &SessionId,
) {
    for (seq, kind_json, label) in [
        (
            99_i64,
            "{\"name\":\"message.typed\",\"detail\":{\"command_id\":\"not-a-number\"}}",
            "known event with corrupt detail",
        ),
        (100_i64, "{\"name\":\"BAD NAME!\"}", "invalid event name"),
        (101_i64, "42", "non-object JSON kind"),
    ] {
        let id: i64 = client
            .query_one(
                "insert into spawn_events (agent_id, session_id, source, kind, session_seq, recorded_at_ms)
                 values ($1, $2, 'claude', $3::text::jsonb, $4, 3000)
                 returning id",
                &[&agent.as_str(), &session.as_str(), &kind_json, &seq],
            )
            .expect("insert corrupt row")
            .get(0);
        let decoded = pg.read_global(spawn_core::event::GlobalPosition(0), 20);
        client
            .execute("delete from spawn_events where id = $1", &[&id])
            .expect("delete corrupt row");
        assert!(
            matches!(decoded, Err(StoreError::Corrupt(_))),
            "{label} ({kind_json}) must return StoreError::Corrupt, got {decoded:?}"
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

    // Insert historical retired and unknown dotted future rows without altering schema.
    for (seq_offset, kind) in [
        serde_json::json!({ "name": "session.native_ended", "detail": { "reason": "exit" } }),
        serde_json::json!({ "name": "notice.typed", "detail": { "through": 1 } }),
        serde_json::json!({ "name": "notice.received" }),
        serde_json::json!({ "name": "notification.received", "detail": { "notification_type": "idle_prompt" } }),
        serde_json::json!({ "name": "invented.corrupt_event" }),
        serde_json::json!({ "name": "future.v99_event", "detail": {} }),
    ]
    .into_iter()
    .enumerate()
    {
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
            .expect("insert historical or unknown dotted row");
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

    // Global log read skips all retired and unknown dotted rows cleanly and returns both active events.
    let read = pg
        .read_global(spawn_core::event::GlobalPosition(0), 20)
        .expect("read_global skips retired and unknown dotted events");
    let names: Vec<&str> = read.iter().map(|r| r.event.kind.name()).collect();
    assert_eq!(names, ["message.received", "turn.stopped"]);

    // All 8 rows remain untouched in PostgreSQL (none were deleted).
    let raw_count: i64 = client
        .query_one("select count(*) from spawn_events", &[])
        .expect("count")
        .get(0);
    assert_eq!(raw_count, 8);

    // Truly corrupt rows fail closed with StoreError::Corrupt.
    assert_corrupt_rows_rejected(&pg, &mut client, &agent, &session);
}
