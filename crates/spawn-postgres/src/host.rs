//! Facts about this host in the control stream: `host.disk` records, one
//! per crossing of the loop's free-space threshold (docs/storage.md, "Disk
//! room").

use serde_json::Value;
use spawn_core::port::StoreError;

use crate::PostgresStore;

/// The latest `host.disk` record of every path, each with the `event_id`
/// and `at_ms` of its event, ordered by path.
pub(crate) const HOST_DISK: &str = r"
select coalesce(jsonb_agg(d.record order by d.record ->> 'path'), '[]'::jsonb)
from (
  select distinct on (e.kind -> 'detail' -> 'record' ->> 'path')
         (e.kind -> 'detail' -> 'record')
           || jsonb_build_object('event_id', e.id, 'at_ms', e.recorded_at_ms) as record
    from spawn_events e
   where e.session_id = 'session_control' and e.event_name = 'control.recorded'
     and e.kind -> 'detail' -> 'record' ->> 'type' = 'host.disk'
   order by e.kind -> 'detail' -> 'record' ->> 'path', e.session_seq desc
) d";

impl PostgresStore {
    /// Record a `host.disk` record (owner only). Returns the event's id.
    pub fn host_record(&self, record: &Value) -> Result<i64, StoreError> {
        Ok(self
            .query_one("select spawn_host_record($1)", &[record])?
            .get(0))
    }

    /// The latest `host.disk` record of every path ([`HOST_DISK`]).
    pub fn host_disk(&self) -> Result<Value, StoreError> {
        self.query_json(HOST_DISK, &[])
    }
}
