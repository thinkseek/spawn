//! The event log port over `spawn_events`.

use spawn_core::event::{Appended, EventRecord, GlobalPosition, NewEvent};
use spawn_core::port::{EventLog, StoreError};
use spawn_core::time::Timestamp;

use crate::PostgresStore;
use crate::row::{bigint, corrupt, event_record, global_position, session_seq};

/// Columns of `spawn_events` that make one `EventRecord`
/// (see [`crate::row::event_record`]).
macro_rules! event_record_columns {
    () => {
        "id, agent_id, session_id, execution_id, source, source_event_id,
                        kind, session_seq, recorded_at_ms"
    };
}

/// Append one event through `spawn_append`, which assigns its positions.
/// `$1` agent, `$2` session, `$3` execution, `$4` source,
/// `$5` source event id, `$6` kind.
pub(crate) const APPEND: &str = "select out_id, out_seq, out_at, out_inserted
                   from spawn_append($1, $2, $3, $4, $5, $6)";

/// Events of every session after a global position. `$1` after, `$2` limit.
pub(crate) const READ_GLOBAL: &str = concat!(
    "select ",
    event_record_columns!(),
    "
                   from spawn_events
                  where id > $1
                  order by id
                  limit $2"
);

impl EventLog for PostgresStore {
    fn append(&self, event: NewEvent) -> Result<Appended, StoreError> {
        let kind = serde_json::to_value(&event.kind).map_err(corrupt)?;
        let row = self.query_one(
            APPEND,
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

    fn read_global(
        &self,
        after: GlobalPosition,
        limit: usize,
    ) -> Result<Vec<EventRecord>, StoreError> {
        self.query(READ_GLOBAL, &[&bigint(after.0), &bigint(limit)])?
            .iter()
            .map(event_record)
            .collect()
    }
}
