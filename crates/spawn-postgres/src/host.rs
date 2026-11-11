//! Facts about this host in the control stream: `host.disk` records, one
//! per crossing of the loop's free-space threshold (docs/storage.md, "Disk
//! room"), and `host.judge` records, one per crossing of the model judge
//! between failing and answering.

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

/// The latest `host.judge` record of every judge, each with the `event_id`
/// and `at_ms` of its event, ordered by judge: where a restarted watcher of
/// the model judge stands (`down` or `up`).
pub(crate) const HOST_JUDGE: &str = r"
select coalesce(jsonb_agg(j.record order by j.record ->> 'judge'), '[]'::jsonb)
from (
  select distinct on (e.kind -> 'detail' -> 'record' ->> 'judge')
         (e.kind -> 'detail' -> 'record')
           || jsonb_build_object('event_id', e.id, 'at_ms', e.recorded_at_ms) as record
    from spawn_events e
   where e.session_id = 'session_control' and e.event_name = 'control.recorded'
     and e.kind -> 'detail' -> 'record' ->> 'type' = 'host.judge'
   order by e.kind -> 'detail' -> 'record' ->> 'judge', e.session_seq desc
) j";

/// The `host.storage` samples `/api/fleet`'s storage figures are read
/// from, each as `{at_ms, archive_bytes, archive_files, projects_bytes,
/// projects_files}` or null: `oldest`, the oldest recorded at or after `$1`
/// (the archive's growth is measured from it), and `before`, the newest
/// recorded at or before `$2` (Claude Code's files then).
pub(crate) const HOST_STORAGE: &str = r"
with samples as (
  select e.recorded_at_ms, e.id,
         (e.kind -> 'detail' -> 'record') - 'type'
           || jsonb_build_object('at_ms', e.recorded_at_ms) as sample
    from spawn_events e
   where e.session_id = 'session_control' and e.event_name = 'control.recorded'
     and e.kind -> 'detail' -> 'record' ->> 'type' = 'host.storage'
)
select jsonb_build_object(
  'oldest', (select s.sample from samples s where s.recorded_at_ms >= $1
              order by s.recorded_at_ms, s.id limit 1),
  'before', (select s.sample from samples s where s.recorded_at_ms <= $2
              order by s.recorded_at_ms desc, s.id desc limit 1))";

/// The database's size (`pg_database_size`) and its five largest tables by
/// `pg_total_relation_size`, largest first, as `{bytes, tables: [{name,
/// bytes}]}`.
pub(crate) const DATABASE_SIZE: &str = r"
select jsonb_build_object(
  'bytes', pg_database_size(current_database()),
  'tables', coalesce((
    select jsonb_agg(jsonb_build_object('name', t.relname, 'bytes', t.bytes)
                     order by t.bytes desc, t.relname)
      from (select c.relname, pg_total_relation_size(c.oid) as bytes
              from pg_class c join pg_namespace n on n.oid = c.relnamespace
             where c.relkind in ('r', 'p') and n.nspname = 'public'
             order by pg_total_relation_size(c.oid) desc, c.relname
             limit 5) t), '[]'::jsonb))";

impl PostgresStore {
    /// The `host.storage` samples the storage figures need ([`HOST_STORAGE`]):
    /// the oldest at or after `since_ms` and the newest at or before
    /// `before_ms`.
    pub fn host_storage(&self, since_ms: i64, before_ms: i64) -> Result<Value, StoreError> {
        self.query_json(HOST_STORAGE, &[&since_ms, &before_ms])
    }

    /// The database's size and its five largest tables ([`DATABASE_SIZE`]).
    pub fn database_size(&self) -> Result<Value, StoreError> {
        self.query_json(DATABASE_SIZE, &[])
    }

    /// Record a host fact through `spawn_host_record` (owner only):
    /// `host.disk`, `host.storage`, `host.judge`, and the recovery's records. Returns the
    /// event's id.
    pub fn host_record(&self, record: &Value) -> Result<i64, StoreError> {
        Ok(self
            .query_one("select spawn_host_record($1)", &[record])?
            .get(0))
    }

    /// The latest `host.disk` record of every path ([`HOST_DISK`]).
    pub fn host_disk(&self) -> Result<Value, StoreError> {
        self.query_json(HOST_DISK, &[])
    }

    /// The latest `host.judge` record of every judge ([`HOST_JUDGE`]).
    pub fn host_judge(&self) -> Result<Value, StoreError> {
        self.query_json(HOST_JUDGE, &[])
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
