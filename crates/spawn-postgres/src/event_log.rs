//! The event log port over `spawn_events`.

use spawn_core::event::{Appended, EventRecord, GlobalPosition, NewEvent, SessionSeq};
use spawn_core::id::SessionId;
use spawn_core::port::{EventLog, StoreError};
use spawn_core::time::Timestamp;

use crate::PostgresStore;
use crate::queries::events;
use crate::row::{bigint, corrupt, event_record, global_position, session_seq};

impl EventLog for PostgresStore {
    fn append(&self, event: NewEvent) -> Result<Appended, StoreError> {
        let kind = serde_json::to_value(&event.kind).map_err(corrupt)?;
        let row = self.query_one(
            events::APPEND,
            &[
                &event.agent_id.as_str(),
                &event.session_id.as_str(),
                &event
                    .execution_id
                    .as_ref()
                    .map(|id| id.as_str().to_string()),
                &event.source.as_str(),
                &event.source_event_id,
                &kind,
            ],
        )?;
        Ok(Appended {
            record: EventRecord {
                position: global_position(&row, "out_id"),
                session_seq: session_seq(&row, "out_seq"),
                recorded_at: Timestamp(row.get("out_at")),
                event,
            },
            inserted: row.get("out_inserted"),
        })
    }

    fn read_session(
        &self,
        session: &SessionId,
        after: SessionSeq,
        limit: usize,
    ) -> Result<Vec<EventRecord>, StoreError> {
        self.query(
            events::READ_SESSION,
            &[&session.as_str(), &bigint(after.0), &bigint(limit)],
        )?
        .iter()
        .map(event_record)
        .collect()
    }

    fn read_global(
        &self,
        after: GlobalPosition,
        limit: usize,
    ) -> Result<Vec<EventRecord>, StoreError> {
        self.query(events::READ_GLOBAL, &[&bigint(after.0), &bigint(limit)])?
            .iter()
            .map(event_record)
            .collect()
    }
}
