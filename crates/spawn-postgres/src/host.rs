//! Facts about this host in the control stream: `host.disk` records, one
//! per crossing of the loop's free-space threshold (docs/storage.md, "Disk
//! room"), and the resource envelope's records (objective 029): one
//! `host.envelope` when the slice's settings change, and an
//! `admission.waiting`/`admission.admitted` pair around an execution's or a
//! gate's wait for room.

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

/// The latest `host.envelope` record, with its `event_id` and `at_ms`, or
/// null when none was ever recorded.
pub(crate) const HOST_ENVELOPE: &str = r"
select coalesce((
  select (e.kind -> 'detail' -> 'record')
           || jsonb_build_object('event_id', e.id, 'at_ms', e.recorded_at_ms)
    from spawn_events e
   where e.session_id = 'session_control' and e.event_name = 'control.recorded'
     and e.kind -> 'detail' -> 'record' ->> 'type' = 'host.envelope'
   order by e.session_seq desc limit 1), 'null'::jsonb)";

/// Gate runs a task has had, and the event id of its latest judging: the
/// `n` of its next gate's scope starts after it, and an admission wait
/// begins there.
pub(crate) const GATE_RUNS: &str = r"
select count(*)::bigint, coalesce(max(e.id), 0)
  from spawn_events e
 where e.session_id = 'session_control' and e.event_name = 'control.recorded'
   and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.gated'
   and (e.kind -> 'detail' -> 'record' ->> 'id')::bigint = $1";

impl PostgresStore {
    /// The latest `host.envelope` record ([`HOST_ENVELOPE`]).
    pub fn host_envelope(&self) -> Result<Value, StoreError> {
        self.query_json(HOST_ENVELOPE, &[])
    }

    /// The admission waits still open (objective 029), through the
    /// grant-filtered `spawn_read_admissions`: for each execution and each
    /// gate whose latest record is an `admission.waiting`, the measure and
    /// the threshold.
    pub fn admissions(&self) -> Result<Value, StoreError> {
        self.query_json(
            "select coalesce(jsonb_agg(row_to_json(a) order by a.kind, a.command_id, a.task),
                             '[]'::jsonb) from spawn_read_admissions() a",
            &[],
        )
    }

    /// How many gates `task` has had, and the event id of its latest
    /// judging ([`GATE_RUNS`]).
    pub fn gate_runs(&self, task: i64) -> Result<(u32, i64), StoreError> {
        let row = self.query_one(GATE_RUNS, &[&task])?;
        let count: i64 = row.get(0);
        Ok((u32::try_from(count).unwrap_or(u32::MAX), row.get(1)))
    }
}

impl spawn_core::port::HostRecords for PostgresStore {
    fn record(&self, record: &Value) -> Result<spawn_core::port::Recorded, StoreError> {
        let id: i64 = self
            .query_one("select spawn_host_record($1)", &[record])?
            .get(0);
        let at_ms: i64 = self
            .query_one(
                "select recorded_at_ms from spawn_events where id = $1",
                &[&id],
            )?
            .get(0);
        Ok(spawn_core::port::Recorded { id, at_ms })
    }

    fn admission_record(
        &self,
        record_type: &str,
        kind: &str,
        field: &str,
        value: &str,
        after: i64,
    ) -> Result<Option<i64>, StoreError> {
        // The field is one of the record's own, named by the caller (never
        // by a user): the query is built from those names only.
        if !field
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
        {
            return Err(StoreError::Rejected(format!("not a record field: {field}")));
        }
        let sql = format!(
            "select min(e.recorded_at_ms) from spawn_events e \
              where e.session_id = 'session_control' and e.event_name = 'control.recorded' \
                and e.kind -> 'detail' -> 'record' ->> 'type' = $1 \
                and e.kind -> 'detail' -> 'record' ->> 'kind' = $2 \
                and e.kind -> 'detail' -> 'record' ->> '{field}' = $3 \
                and e.id > $4"
        );
        let row = self.query_one(&sql, &[&record_type, &kind, &value, &after])?;
        Ok(row.get(0))
    }

    fn interrupted(&self, execution: &str) -> Result<bool, StoreError> {
        let row = self.query_one(
            "select exists (
               select 1 from spawn_events e
                where e.session_id = 'session_control' and e.event_name = 'control.recorded'
                  and e.kind -> 'detail' -> 'record' ->> 'type' = 'execution.interrupted'
                  and e.kind -> 'detail' -> 'record' ->> 'execution_id' = $1)",
            &[&execution],
        )?;
        Ok(row.get(0))
    }

    fn now_ms(&self) -> Result<i64, StoreError> {
        Ok(self.query_one("select spawn_now_ms()", &[])?.get(0))
    }
}
