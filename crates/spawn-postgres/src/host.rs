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
/// projects_files}` (and `database_bytes` since objective 085) or null:
/// `oldest`, the oldest recorded at or after `$1`; `carrying`, for each of
/// `archive_bytes`, `projects_bytes` and `database_bytes`, the oldest
/// recorded at or after `$1` that carries it as a number (each grower's
/// growth is measured from it: an older sample without it is passed over);
/// and `before`, the newest recorded at or before `$2` (Claude Code's files
/// then).
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
  'carrying', (select jsonb_object_agg(f.field, (
                 select s.sample from samples s
                  where s.recorded_at_ms >= $1
                    and jsonb_typeof(s.sample -> f.field) = 'number'
                  order by s.recorded_at_ms, s.id limit 1))
                 from unnest(array['archive_bytes', 'projects_bytes', 'database_bytes']) f(field)),
  'before', (select s.sample from samples s where s.recorded_at_ms <= $2
              order by s.recorded_at_ms desc, s.id desc limit 1))";

/// The `host.core_lines` samples recorded at or after `$1` (objective 090),
/// oldest first, each as `{at_ms, values}`: through their own partial index
/// (`spawn_events_host_core_lines`), never the rest of the control stream.
pub(crate) const HOST_CORE_LINES: &str = r"
select coalesce(jsonb_agg(jsonb_build_object('at_ms', s.recorded_at_ms, 'values', s.vals)
                          order by s.recorded_at_ms, s.id), '[]'::jsonb)
  from (select e.recorded_at_ms, e.id, e.kind -> 'detail' -> 'record' -> 'values' as vals
          from spawn_events e
         where e.session_id = 'session_control' and e.event_name = 'control.recorded'
           and e.kind -> 'detail' -> 'record' ->> 'type' = 'host.core_lines'
           and e.recorded_at_ms >= $1) s";

/// The schemas a database holds other than PostgreSQL's own (`pg_*`,
/// `information_schema`) and `public`: what tests leave behind (objective
/// 074).
const SCHEMAS: &str = r"(select count(*) from pg_namespace n
     where n.nspname not like 'pg\_%' and n.nspname not in ('information_schema', 'public'))";

/// The database's name, its size (`pg_database_size`), its schemas
/// ([`SCHEMAS`]), its event log's bytes (the sum of `spawn_events.kind`'s
/// stored size, `pg_column_size`: what the data justifies, objective 074)
/// and its five largest tables by `pg_total_relation_size`, largest first,
/// as `{name, bytes, schemas, log_bytes, tables: [{name, bytes}]}`.
pub(crate) const DATABASE_SIZE: &str = r"
select jsonb_build_object(
  'name', current_database(),
  'bytes', pg_database_size(current_database()),
  'schemas', SCHEMAS_COUNT,
  'log_bytes', (select coalesce(sum(pg_column_size(e.kind)), 0)::bigint from spawn_events e),
  'tables', coalesce((
    select jsonb_agg(jsonb_build_object('name', t.relname, 'bytes', t.bytes)
                     order by t.bytes desc, t.relname)
      from (select c.relname, pg_total_relation_size(c.oid) as bytes
              from pg_class c join pg_namespace n on n.oid = c.relnamespace
             where c.relkind in ('r', 'p') and n.nspname = current_schema()
             order by pg_total_relation_size(c.oid) desc, c.relname
             limit 5) t), '[]'::jsonb))";

/// A test database's name, size and schemas ([`SCHEMAS`]), as `{name,
/// bytes, schemas}`.
const TEST_DATABASE_SIZE: &str = r"
select jsonb_build_object(
  'name', current_database(),
  'bytes', pg_database_size(current_database()),
  'schemas', SCHEMAS_COUNT)";

/// How long a test database's reading may take, connecting or querying: a
/// server that does not answer costs the reader no more than this.
const TEST_DATABASE_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

/// The name, size and schemas of the test database at `url` (the gates' or
/// the fleet's, objective 074), as `{name, bytes, schemas}`: one read-only
/// statement on a connection of its own, which gives up after 5 s
/// connecting and 5 s querying.
///
/// # Errors
///
/// A URL that does not parse, a server that cannot be reached, a statement
/// that fails. The error never names the URL.
pub fn test_database_size(url: &str) -> Result<Value, StoreError> {
    let mut config: postgres::Config = url.parse().map_err(|_: postgres::Error| {
        StoreError::Backend("the URL is not a postgres URL".to_string())
    })?;
    let limit = format!("-c statement_timeout={}", TEST_DATABASE_LIMIT.as_millis());
    let options = match config.get_options() {
        Some(options) if !options.trim().is_empty() => format!("{options} {limit}"),
        _ => limit,
    };
    config
        .options(&options)
        .connect_timeout(TEST_DATABASE_LIMIT);
    let mut client = config
        .connect(postgres::NoTls)
        .map_err(|error| crate::backend(&error))?;
    let row = client
        .query_one(&TEST_DATABASE_SIZE.replace("SCHEMAS_COUNT", SCHEMAS), &[])
        .map_err(|error| crate::backend(&error))?;
    row.try_get(0).map_err(|error| crate::backend(&error))
}

impl PostgresStore {
    /// The `host.storage` samples the storage figures need ([`HOST_STORAGE`]):
    /// the oldest at or after `since_ms` (overall and carrying each grower's
    /// size) and the newest at or before `before_ms`.
    pub fn host_storage(&self, since_ms: i64, before_ms: i64) -> Result<Value, StoreError> {
        self.query_json(HOST_STORAGE, &[&since_ms, &before_ms])
    }

    /// The `host.core_lines` samples recorded at or after `since_ms`, oldest
    /// first, as `[{at_ms, values}]` ([`HOST_CORE_LINES`]).
    pub fn host_core_lines(&self, since_ms: i64) -> Result<Value, StoreError> {
        self.query_json(HOST_CORE_LINES, &[&since_ms])
    }

    /// The database's name, size, schemas, event log bytes and five
    /// largest tables ([`DATABASE_SIZE`]).
    pub fn database_size(&self) -> Result<Value, StoreError> {
        self.query_json(&DATABASE_SIZE.replace("SCHEMAS_COUNT", SCHEMAS), &[])
    }

    /// Record a host fact through `spawn_host_record` (owner only):
    /// `host.disk`, `host.storage`, `host.core_lines`, `host.judge`, and the
    /// recovery's records. Returns the
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

/// The newest `backup.dumped` and `backup.bundled` record (objective 82),
/// as `{dump, bundle}`: each `{at_ms, key, bytes}` (`key` the record's first
/// uploaded key, `bytes` its bytes), or null when there is none. Newest by
/// event id, as the alert step folds them; each a backward read of
/// `spawn_events_session_name`.
pub(crate) const NEWEST_BACKUPS: &str = r"
select jsonb_build_object(
  'dump', (select jsonb_build_object('at_ms', x.recorded_at_ms,
                                     'key', x.record -> 'keys' ->> 0,
                                     'bytes', x.record -> 'bytes')
             from (select e.recorded_at_ms, e.kind -> 'detail' -> 'record' as record
                     from spawn_events e
                    where e.session_id = 'session_control' and e.event_name = 'control.recorded'
                      and e.kind -> 'detail' -> 'record' ->> 'type' = 'backup.dumped'
                    order by e.id desc
                    limit 1) x),
  'bundle', (select jsonb_build_object('at_ms', x.recorded_at_ms,
                                       'key', x.record -> 'keys' ->> 0,
                                       'bytes', x.record -> 'bytes')
               from (select e.recorded_at_ms, e.kind -> 'detail' -> 'record' as record
                       from spawn_events e
                      where e.session_id = 'session_control' and e.event_name = 'control.recorded'
                        and e.kind -> 'detail' -> 'record' ->> 'type' = 'backup.bundled'
                      order by e.id desc
                      limit 1) x))";

/// When the newest control record of type `$1` with `ok` true was
/// recorded (objective 093: a restore drill's last pass, the database
/// drill's `backup.verified`); null without one.
pub(crate) const NEWEST_PASSED: &str = r"
select e.recorded_at_ms
  from spawn_events e
 where e.session_id = 'session_control' and e.event_name = 'control.recorded'
   and e.kind -> 'detail' -> 'record' ->> 'type' = $1
   and e.kind -> 'detail' -> 'record' -> 'ok' = 'true'::jsonb
 order by e.id desc
 limit 1";

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

    /// The newest database copy and bundle ([`NEWEST_BACKUPS`]).
    pub fn newest_backups(&self) -> Result<Value, StoreError> {
        self.query_json(NEWEST_BACKUPS, &[])
    }

    /// When the newest record of type `kind` with `ok` true was recorded
    /// ([`NEWEST_PASSED`]); `None` without one.
    pub fn newest_passed_record(&self, kind: &str) -> Result<Option<i64>, StoreError> {
        Ok(self
            .query_opt(NEWEST_PASSED, &[&kind])?
            .map(|row| row.get(0)))
    }

    /// Every distinct mission source ([`SOURCES`]).
    pub fn mission_sources(&self) -> Result<Value, StoreError> {
        self.query_json(SOURCES, &[])
    }
}
