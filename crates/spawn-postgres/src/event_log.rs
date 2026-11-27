//! The event log port over `spawn_events`.

use spawn_core::event::{Appended, EventRecord, GlobalPosition, NewEvent};
use spawn_core::id::SessionId;
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

/// Events of every session after a global position, ordered by commit watermark
/// `(commit_xid, id)` and skipping uncommitted / in-flight transactions whose
/// `commit_xid >= safe_xid`. `$1` after, `$2` limit.
pub(crate) const READ_GLOBAL: &str = concat!(
    "with ",
    commit_watermark_cte!(),
    "
                   select wm.wm_raced, e.id, e.agent_id, e.session_id, e.execution_id,
                          e.source, e.source_event_id, e.kind, e.session_seq, e.recorded_at_ms
                     from wm
                     left join lateral (
                         select ",
    event_record_columns!(),
    "
                           from spawn_events
                          where commit_xid < wm.safe_xid
                            and (commit_xid, id) > (
                                    case
                                        when $1::bigint <= 0::bigint then '0'::xid8
                                        else coalesce(
                                            (select w.commit_xid from spawn_events w where w.id = $1::bigint),
                                            (select w.commit_xid
                                               from spawn_events w
                                              where w.id < $1::bigint
                                                and w.commit_xid < wm.safe_xid
                                              order by w.commit_xid desc, w.id desc
                                              limit 1),
                                            '0'::xid8
                                        )
                                    end,
                                    greatest($1::bigint, 0::bigint)
                                )
                          order by commit_xid, id
                          limit $2
                     ) e on true"
);

/// The session's open execution: its latest `execution.started` when no
/// `execution.ended` the Bridge wrote follows it, the rule by which
/// `spawn_append_trusted` refuses a second start, read the same way. `$1`
/// session.
pub(crate) const OPEN_EXECUTION: &str = concat!(
    "select ",
    event_record_columns!(),
    "
                   from (select * from spawn_events e
                          where e.session_id = $1 and e.event_name = 'execution.started'
                            and e.execution_id is not null
                          order by e.id desc
                          limit 1) s
                  where not exists (
                        select 1 from spawn_events x
                         where x.session_id = s.session_id and x.event_name = 'execution.ended'
                           and x.execution_id = s.execution_id and x.source = 'bridge'
                           and x.id > s.id)"
);

impl EventLog for PostgresStore {
    fn append(&self, mut event: NewEvent) -> Result<Appended, StoreError> {
        spawn_core::redact::redact_event(&mut event.kind);
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

    /// Records this build cannot read (`event_record`) are skipped, and
    /// the read goes on past them: fewer than `limit` records still means
    /// the end of the log, however long a run of skipped ones.
    fn read_global(
        &self,
        after: GlobalPosition,
        limit: usize,
    ) -> Result<Vec<EventRecord>, StoreError> {
        let mut records = Vec::new();
        let mut after = after;
        while records.len() < limit {
            let want = limit - records.len();
            let mut rows = Vec::new();
            for attempt in 0..crate::WATERMARK_RACE_ATTEMPTS {
                rows = self.query(READ_GLOBAL, &[&bigint(after.0), &bigint(want)])?;
                let raced = rows
                    .first()
                    .is_some_and(|row| row.get::<_, bool>("wm_raced"));
                if !raced || attempt + 1 == crate::WATERMARK_RACE_ATTEMPTS {
                    break;
                }
            }
            let mut matched = 0usize;
            for row in &rows {
                if row.get::<_, Option<i64>>("id").is_none() {
                    continue;
                }
                matched += 1;
                after = global_position(row, "id");
                records.extend(event_record(row)?);
            }
            if matched < want {
                break;
            }
        }
        Ok(records)
    }

    fn open_execution(&self, session: &SessionId) -> Result<Option<EventRecord>, StoreError> {
        match self.query_opt(OPEN_EXECUTION, &[&session.as_str()])? {
            Some(row) => event_record(&row),
            None => Ok(None),
        }
    }
}
