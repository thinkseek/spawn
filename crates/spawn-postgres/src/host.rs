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
                 from unnest(array['archive_bytes', 'projects_bytes', 'database_bytes',
                             'session_backup_bytes']) f(field)),
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

/// When the first `host.core_lines` sample carrying the figure `$1` as a
/// number was recorded (objective 181: the week of judged data U2's range
/// waits for), or null before one: the samples read oldest first through
/// their partial index (`spawn_events_host_core_lines`, on
/// `recorded_at_ms`), stopping at the first that carries it.
pub(crate) const FIRST_CORE_LINES_VALUE: &str = r"
select coalesce(to_jsonb((
  select e.recorded_at_ms
    from spawn_events e
   where e.session_id = 'session_control' and e.event_name = 'control.recorded'
     and e.kind -> 'detail' -> 'record' ->> 'type' = 'host.core_lines'
     and jsonb_typeof(e.kind -> 'detail' -> 'record' -> 'values' -> $1::text) = 'number'
   order by e.recorded_at_ms
   limit 1)), 'null'::jsonb)";

/// The `host.storage_bytes` readings (objective 180) the storage figures are
/// measured from (objective 185), and the database's clock `now_ms`:
/// `newest`, the newest reading as `{at_ms, sources}`, or null before the
/// first; `older`, the newest recorded at least a day before it, or null;
/// `landings`, the landings recorded after `older` and up to `newest` (0
/// without both): every `task.landed`, and every owner's landing, a
/// `source.moved` naming its request (a deploy's names none), on any mission
/// and source, through `spawn_events_landing` between the two readings'
/// places in the stream. The readings are not materialized, so `newest` and
/// `older` each walk `spawn_events_host_storage_bytes` back and stop at their
/// row, never reading every reading.
pub(crate) const HOST_STORAGE_BYTES: &str = r"
with readings as not materialized (
  select e.session_seq as seq, e.recorded_at_ms as at_ms,
         e.kind -> 'detail' -> 'record' -> 'sources' as sources
    from spawn_events e
   where e.session_id = 'session_control' and e.event_name = 'control.recorded'
     and e.kind -> 'detail' -> 'record' ->> 'type' = 'host.storage_bytes'
), newest as (
  select * from readings order by seq desc limit 1
), older as (
  select r.* from readings r, newest n
   where r.at_ms <= n.at_ms - 86400000
   order by r.seq desc limit 1
)
select jsonb_build_object(
  'now_ms', (floor(extract(epoch from clock_timestamp()) * 1000))::bigint,
  'newest', (select jsonb_build_object('at_ms', n.at_ms, 'sources', n.sources) from newest n),
  'older', (select jsonb_build_object('at_ms', o.at_ms, 'sources', o.sources) from older o),
  'landings', (select count(*) from spawn_events e, newest n, older o
                where e.session_id = 'session_control' and e.event_name = 'control.recorded'
                  and e.kind -> 'detail' -> 'record' ->> 'type'
                      in ('task.landed', 'source.moved', 'deploy.finished')
                  and e.session_seq > o.seq and e.session_seq <= n.seq
                  and (e.kind -> 'detail' -> 'record' ->> 'type' = 'task.landed'
                       or e.kind -> 'detail' -> 'record' ->> 'type' = 'source.moved'
                          and jsonb_typeof(e.kind -> 'detail' -> 'record' -> 'request')
                              is distinct from 'null'
                          and e.kind -> 'detail' -> 'record' ? 'request')))";

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
/// connecting and 5 s querying, using an explicit passfile when the URL omits its password.
///
/// # Errors
///
/// A URL that does not parse, a server that cannot be reached, or a query failure.
/// The error never names the URL.
pub fn test_database_size_with_passfile(
    url: &str,
    passfile: Option<&std::path::Path>,
) -> Result<Value, StoreError> {
    test_database_read(
        url,
        &TEST_DATABASE_SIZE.replace("SCHEMAS_COUNT", SCHEMAS),
        passfile,
    )
}

/// One read-only statement answering one jsonb value, on a connection of its
/// own to the test database at `url` that gives up after
/// [`TEST_DATABASE_LIMIT`] connecting and as long querying. The error never
/// names the URL.
fn test_database_read(
    url: &str,
    statement: &str,
    passfile: Option<&std::path::Path>,
) -> Result<Value, StoreError> {
    let mut config = crate::passfile::config(url, passfile)
        .map_err(|_| StoreError::Backend("the URL is not a postgres URL".to_string()))?;
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
        .query_one(statement, &[])
        .map_err(|error| crate::backend(&error))?;
    row.try_get(0).map_err(|error| crate::backend(&error))
}

/// A test database's schemas but `public` and the system's (objective 094),
/// as `[{name, comment, held}]` by name: `comment` its comment (null
/// without one), `held` whether a live session's `application_name` is its
/// name. One statement over the catalogs (pg_namespace, its comments) and
/// pg_stat_activity, taking no lock beyond catalog reads.
const TEST_SCHEMAS: &str = r"
select coalesce(jsonb_agg(jsonb_build_object(
         'name', n.nspname,
         'comment', obj_description(n.oid, 'pg_namespace'),
         'held', exists (select 1 from pg_stat_activity a
                          where a.application_name = n.nspname::text))
       order by n.nspname), '[]'::jsonb)
  from pg_namespace n
 where n.nspname not like 'pg\_%' and n.nspname not in ('information_schema', 'public')";

/// The schemas of the test database at `url` (`TEST_SCHEMAS`): one
/// read-only statement on a connection of its own, which gives up after 5 s
/// connecting and 5 s querying, as [`test_database_size_with_passfile`]'s, using
/// an explicit passfile when the URL omits its password.
///
/// # Errors
///
/// A URL that does not parse, a server that cannot be reached, or a query failure.
/// The error never names the URL.
pub fn test_schemas_with_passfile(
    url: &str,
    passfile: Option<&std::path::Path>,
) -> Result<Value, StoreError> {
    test_database_read(url, TEST_SCHEMAS, passfile)
}

/// Explicit fallback defaults corresponding to libpq's `PGPORT`, `PGUSER`/`USER`,
/// and `PGDATABASE`, passed by entry boundaries instead of reading env vars in
/// the PostgreSQL layer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LibpqDefaults {
    /// Fallback port when omitted from the connection URL (defaults to `5432` if `None`).
    pub port: Option<u16>,
    /// Fallback user name when omitted from the connection URL.
    pub user: Option<String>,
    /// Fallback database name when omitted from the connection URL (falls back to `user`).
    pub database: Option<String>,
}

/// Whether a PostgreSQL server is on this host or at a remote address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostLocality {
    /// A Unix socket, `localhost`, or a loopback IP address (`127.0.0.0/8`, `::1`).
    Local,
    /// A non-empty, lowercased remote host name or non-loopback IP address.
    Remote(String),
}

impl HostLocality {
    /// Whether this server is local to this host (a Unix socket or loopback address).
    #[must_use]
    pub const fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }

    /// The lowercased remote host name or address, or `None` when [`Self::Local`].
    #[must_use]
    pub fn remote_host(&self) -> Option<&str> {
        match self {
            Self::Local => None,
            Self::Remote(host) => Some(host),
        }
    }
}

/// The database a URL names, after libpq's defaults (objective 094): whether
/// its server is this host's (a socket, or a loopback host) or remote, the
/// port and the database name. Two spellings of one database (a socket against
/// localhost or 127.0.0.1, an explicit 5432, another user) name the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseIdentity {
    /// Whether the server is local to this host or at a remote host name.
    pub locality: HostLocality,
    /// TCP or Unix-socket port number (defaults to `LibpqDefaults::port` or 5432).
    pub port: u16,
    /// Target database name resolved after libpq defaults.
    pub database: String,
}

impl DatabaseIdentity {
    /// Whether this server is local to this host (a Unix socket or loopback address).
    #[must_use]
    pub const fn is_local(&self) -> bool {
        self.locality.is_local()
    }
}

/// Resolve [`DatabaseIdentity`] using caller-supplied [`LibpqDefaults`] instead of
/// ambient environment variables. Never names the URL in its error.
///
/// # Errors
///
/// A URL that does not parse as a postgres URL or key-value string, or one
/// naming an empty host or more than one host or port.
pub fn database_identity_with_defaults(
    url: &str,
    defaults: &LibpqDefaults,
) -> Result<DatabaseIdentity, String> {
    let config: postgres::Config = url
        .trim()
        .parse()
        .map_err(|_: postgres::Error| "not a postgres URL".to_string())?;
    let hosts = config.get_hosts();
    if hosts.len() > 1 {
        return Err("names more than one host".to_string());
    }
    let locality = match hosts.first() {
        None | Some(postgres::config::Host::Unix(_)) => HostLocality::Local,
        Some(postgres::config::Host::Tcp(name)) => {
            let bare = name.trim().trim_start_matches('[').trim_end_matches(']');
            if bare.is_empty() {
                return Err("names an empty host".to_string());
            }
            let loopback = bare.eq_ignore_ascii_case("localhost")
                || bare
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback());
            if loopback {
                HostLocality::Local
            } else {
                HostLocality::Remote(bare.to_ascii_lowercase())
            }
        }
    };
    let ports = config.get_ports();
    if ports.len() > 1 {
        return Err("names more than one port".to_string());
    }
    let port = ports.first().copied().or(defaults.port).unwrap_or(5432);
    let user = config
        .get_user()
        .map(str::to_string)
        .or_else(|| defaults.user.clone())
        .unwrap_or_default();
    let database = config
        .get_dbname()
        .map(str::to_string)
        .or_else(|| defaults.database.clone())
        .unwrap_or(user);
    Ok(DatabaseIdentity {
        locality,
        port,
        database,
    })
}

impl PostgresStore {
    /// The `host.storage` samples the storage figures need (`HOST_STORAGE`):
    /// the oldest at or after `since_ms` (overall and carrying each grower's
    /// size) and the newest at or before `before_ms`.
    pub fn host_storage(&self, since_ms: i64, before_ms: i64) -> Result<Value, StoreError> {
        self.query_json(HOST_STORAGE, &[&since_ms, &before_ms])
    }

    /// The `host.core_lines` samples recorded at or after `since_ms`, oldest
    /// first, as `[{at_ms, values}]` (`HOST_CORE_LINES`).
    pub fn host_core_lines(&self, since_ms: i64) -> Result<Value, StoreError> {
        self.query_json(HOST_CORE_LINES, &[&since_ms])
    }

    /// When the first `host.core_lines` sample carrying `key` as a number
    /// was recorded, in ms; `None` before one (`FIRST_CORE_LINES_VALUE`).
    pub fn first_core_lines_value(&self, key: &str) -> Result<Option<i64>, StoreError> {
        Ok(self
            .query_json_prepared(FIRST_CORE_LINES_VALUE, &[&key])?
            .as_i64())
    }

    /// The storage readings and the database's clock (`HOST_STORAGE_BYTES`):
    /// `{now_ms, newest, older: {at_ms, sources} | null, landings}`.
    pub fn host_storage_bytes(&self) -> Result<Value, StoreError> {
        self.query_json(HOST_STORAGE_BYTES, &[])
    }

    /// The database's name, size, schemas, event log bytes and five
    /// largest tables (`DATABASE_SIZE`).
    pub fn database_size(&self) -> Result<Value, StoreError> {
        self.query_json(&DATABASE_SIZE.replace("SCHEMAS_COUNT", SCHEMAS), &[])
    }

    /// Record a host fact through `spawn_host_record` (owner only):
    /// `host.disk`, `host.storage`, `host.core_lines`, `host.storage_bytes`,
    /// `host.judge`, and the
    /// recovery's records. Returns the
    /// event's id.
    pub fn host_record(&self, record: &Value) -> Result<i64, StoreError> {
        Ok(self
            .query_one(
                "select spawn_host_record($1)",
                &[&crate::control_mask::masked(record)],
            )?
            .get(0))
    }

    /// The latest `host.disk` record of every path (`HOST_DISK`).
    pub fn host_disk(&self) -> Result<Value, StoreError> {
        self.query_json(HOST_DISK, &[])
    }

    /// The latest `host.judge` record of every judge (`HOST_JUDGE`).
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
 order by e.recorded_at_ms desc, e.id desc
 limit 1";

/// A drill record type's two facts in one statement (objective 105): the
/// newest control record of type `$1` whatever its `ok` (`at_ms`,
/// `record`), and its last pass (`passed_at_ms`): that record's time when
/// it passed, else the newest record of the type with `ok` true (a subplan
/// in the CASE branch not taken does not run, so a passed newest costs one
/// lookup). Newest is by recorded time, the id breaking a tie: the order of
/// spawn_events_control_record_type, one backward index read each. No row
/// without a record of the type.
pub(crate) const DRILL_OF_TYPE: &str = r"
with newest as (
  select e.recorded_at_ms as at_ms, e.kind -> 'detail' -> 'record' as record
    from spawn_events e
   where e.session_id = 'session_control' and e.event_name = 'control.recorded'
     and e.kind -> 'detail' -> 'record' ->> 'type' = $1
   order by e.recorded_at_ms desc, e.id desc
   limit 1)
select jsonb_build_object('at_ms', n.at_ms, 'record', n.record,
  'passed_at_ms', case when n.record -> 'ok' = 'true'::jsonb then n.at_ms
                       else (select e.recorded_at_ms from spawn_events e
                              where e.session_id = 'session_control'
                                and e.event_name = 'control.recorded'
                                and e.kind -> 'detail' -> 'record' ->> 'type' = $1
                                and e.kind -> 'detail' -> 'record' -> 'ok' = 'true'::jsonb
                              order by e.recorded_at_ms desc, e.id desc
                              limit 1) end)
  from newest n";

/// Every distinct mission source, sorted: what a full bundle holds.
pub(crate) const SOURCES: &str = r"
select coalesce(jsonb_agg(distinct s.source order by s.source), '[]'::jsonb)
from mission_missions m
cross join lateral jsonb_array_elements_text(coalesce(m.sources, '[]'::jsonb)) s(source)";

impl PostgresStore {
    /// Record a `backup.dumped`, `backup.bundled`, `backup.verified` or
    /// `backup.stored` record (owner only). Returns the event's id.
    pub fn backup_record(&self, record: &Value) -> Result<i64, StoreError> {
        Ok(self
            .query_one(
                "select spawn_backup_record($1)",
                &[&crate::control_mask::masked(record)],
            )?
            .get(0))
    }

    /// The records of one `backup.*` type since `since_ms` (`BACKUP_RECORDS`).
    pub fn backup_records(
        &self,
        kind: &str,
        since_ms: i64,
        limit: i64,
    ) -> Result<Value, StoreError> {
        self.query_json(BACKUP_RECORDS, &[&kind, &since_ms, &limit])
    }

    /// The newest database copy and bundle (`NEWEST_BACKUPS`).
    pub fn newest_backups(&self) -> Result<Value, StoreError> {
        self.query_json(NEWEST_BACKUPS, &[])
    }

    /// When the newest record of type `kind` with `ok` true was recorded
    /// (`NEWEST_PASSED`); `None` without one.
    pub fn newest_passed_record(&self, kind: &str) -> Result<Option<i64>, StoreError> {
        Ok(self
            .query_opt(NEWEST_PASSED, &[&kind])?
            .map(|row| row.get(0)))
    }

    /// The newest record of type `kind` and its last pass
    /// (`DRILL_OF_TYPE`): `{at_ms, record, passed_at_ms}`; `None` without
    /// a record of the type.
    pub fn drill_of_type(&self, kind: &str) -> Result<Option<Value>, StoreError> {
        Ok(self
            .query_opt(DRILL_OF_TYPE, &[&kind])?
            .map(|row| row.get(0)))
    }

    /// Every distinct mission source (`SOURCES`).
    pub fn mission_sources(&self) -> Result<Value, StoreError> {
        self.query_json(SOURCES, &[])
    }
}

#[cfg(test)]
mod tests {
    use super::{HostLocality, LibpqDefaults, database_identity_with_defaults};

    #[test]
    fn database_identity_uses_explicit_defaults_without_reading_env() {
        let bare =
            database_identity_with_defaults("postgres:///spawn_test", &LibpqDefaults::default())
                .expect("parse");
        assert!(bare.is_local());
        assert_eq!(bare.locality, HostLocality::Local);
        assert_eq!(bare.locality.remote_host(), None);
        assert_eq!(bare.port, 5432);
        assert_eq!(bare.database, "spawn_test");

        let with_defaults = database_identity_with_defaults(
            "host=127.0.0.1",
            &LibpqDefaults {
                port: Some(65432),
                user: Some("alice".to_string()),
                database: None,
            },
        )
        .expect("parse with defaults");
        assert!(with_defaults.is_local());
        assert_eq!(with_defaults.locality, HostLocality::Local);
        assert_eq!(with_defaults.port, 65432);
        assert_eq!(with_defaults.database, "alice");

        let explicit_db = database_identity_with_defaults(
            "host=DB.EXAMPLE.COM",
            &LibpqDefaults {
                port: Some(5433),
                user: Some("alice".to_string()),
                database: Some("custom_db".to_string()),
            },
        )
        .expect("parse explicit db");
        assert!(!explicit_db.is_local());
        assert_eq!(
            explicit_db.locality,
            HostLocality::Remote("db.example.com".to_string())
        );
        assert_eq!(explicit_db.locality.remote_host(), Some("db.example.com"));
        assert_eq!(explicit_db.port, 5433);
        assert_eq!(explicit_db.database, "custom_db");

        let uri_defaults = database_identity_with_defaults(
            "postgres://bob@127.0.0.1",
            &LibpqDefaults {
                port: Some(65432),
                user: Some("alice".to_string()),
                database: Some("custom_db".to_string()),
            },
        )
        .expect("parse uri");
        assert!(uri_defaults.is_local());
        assert_eq!(uri_defaults.locality, HostLocality::Local);
        assert_eq!(uri_defaults.port, 5432);
        assert_eq!(uri_defaults.database, "custom_db");

        let ipv6_loopback = database_identity_with_defaults(
            "postgres://bob@[::1]/spawn_test",
            &LibpqDefaults::default(),
        )
        .expect("parse ipv6 loopback");
        assert!(ipv6_loopback.is_local());
        assert_eq!(ipv6_loopback.locality, HostLocality::Local);

        let subnet_loopback = database_identity_with_defaults(
            "postgres://bob@127.0.0.2/spawn_test",
            &LibpqDefaults::default(),
        )
        .expect("parse 127/8 loopback");
        assert!(subnet_loopback.is_local());

        let socket = database_identity_with_defaults(
            "host=/var/run/postgresql dbname=spawn_test",
            &LibpqDefaults::default(),
        )
        .expect("parse unix socket");
        assert!(socket.is_local());

        let empty_host = database_identity_with_defaults(
            "postgres://user:secret_pw@[]/spawn_test",
            &LibpqDefaults::default(),
        )
        .expect_err("empty bracket host must be refused");
        assert!(empty_host.contains("empty host"), "{empty_host}");
        assert!(!empty_host.contains("secret_pw"), "{empty_host}");
    }
}
