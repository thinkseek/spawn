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

/// The `backup.*` records since `since_ms`, oldest first, as
/// `{"event_id", "at_ms", "record"}`: what a run decides its tiers, its
/// bundle's base and its drill from (docs/storage.md, "Where state lives").
pub(crate) const BACKUP_RECORDS: &str = r"
select coalesce(jsonb_agg(jsonb_build_object(
      'event_id', x.id, 'at_ms', x.recorded_at_ms, 'record', x.record)
    order by x.id), '[]'::jsonb)
from (select e.id, e.recorded_at_ms, e.kind -> 'detail' -> 'record' as record
        from spawn_events e
       where e.session_id = 'session_control' and e.event_name = 'control.recorded'
         and e.kind -> 'detail' -> 'record' ->> 'type' = $1
         and e.recorded_at_ms >= $2
       order by e.id
       limit $3) x";

/// Every distinct mission source, sorted: what a full bundle holds.
pub(crate) const SOURCES: &str = r"
select coalesce(jsonb_agg(distinct s.source order by s.source), '[]'::jsonb)
from mission_missions m
cross join lateral jsonb_array_elements_text(coalesce(m.sources, '[]'::jsonb)) s(source)";

impl PostgresStore {
    /// Record a `backup.dumped`, `backup.bundled` or `backup.verified`
    /// record (owner only). Returns the event's id.
    pub fn backup_record(&self, record: &Value) -> Result<i64, StoreError> {
        Ok(self
            .query_one("select spawn_backup_record($1)", &[record])?
            .get(0))
    }

    /// The records of one `backup.*` type since `since_ms` ([`BACKUP_RECORDS`]).
    pub fn backup_records(
        &self,
        kind: &str,
        since_ms: i64,
        limit: i64,
    ) -> Result<Value, StoreError> {
        self.query_json(BACKUP_RECORDS, &[&kind, &since_ms, &limit])
    }

    /// Every distinct mission source ([`SOURCES`]).
    pub fn mission_sources(&self) -> Result<Value, StoreError> {
        self.query_json(SOURCES, &[])
    }
}
