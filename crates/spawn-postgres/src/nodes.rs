//! The nodes (multi-host P1, docs/fleet-architecture.md §1.2): their
//! records are written through the owner-only `spawn_node_record` and read
//! back from the log, each fact one seek of `spawn_events_node`
//! (`spawn_node_latest`): no table holds them, and no read scans a node's
//! heartbeats.

use std::time::Duration;

use serde_json::Value;
use spawn_core::port::StoreError;

use crate::{PostgresStore, backend};

/// How a background worker reaches this store's database and schema on a
/// connection of its own, made on the worker's thread (never the caller's):
/// every statement bounded by `timeout`, and a lock waited for at most
/// half of it, so the worker never holds the append lock long nor waits on
/// it long.
#[derive(Clone)]
pub struct StoreConnector {
    url: String,
    schema: Option<String>,
    timeout: Duration,
}

impl StoreConnector {
    /// Connect (the connect itself bounded at 5 s).
    pub fn connect(&self) -> Result<PostgresStore, StoreError> {
        let mut store = PostgresStore::connect_with_statement_timeout(&self.url, self.timeout)?;
        let timeout = (self.timeout / 2).as_millis().max(1);
        let session = match &self.schema {
            Some(schema) => format!("set lock_timeout = {timeout}; set search_path to {schema}"),
            None => format!("set lock_timeout = {timeout}"),
        };
        store
            .lock()
            .batch_execute(&session)
            .map_err(|error| backend(&error))?;
        store.schema.clone_from(&self.schema);
        Ok(store)
    }
}

/// The current joins: every name a `node.joined` ever named, found by a
/// skip scan of `spawn_events_node` (one seek per name, however often a
/// name was removed and joined again), each name's latest `node.joined`
/// with no `node.removed` after it, and after it its latest record of each
/// type the reads name; `$select` is what each read builds of them.
macro_rules! joined {
    ($select:literal) => {
        concat!(
            r"with recursive names(name) as (
    (select e.kind -> 'detail' -> 'record' ->> 'name' from spawn_events e
      where e.session_id = 'session_control' and e.event_name = 'node.recorded'
        and e.kind -> 'detail' -> 'record' ->> 'type' = 'node.joined'
      order by 1 limit 1)
  union all
    select (select e.kind -> 'detail' -> 'record' ->> 'name' from spawn_events e
             where e.session_id = 'session_control' and e.event_name = 'node.recorded'
               and e.kind -> 'detail' -> 'record' ->> 'type' = 'node.joined'
               and e.kind -> 'detail' -> 'record' ->> 'name' > n.name
             order by 1 limit 1)
      from names n where n.name is not null)
select coalesce(jsonb_agg(x.node order by x.name), '[]'::jsonb) from (select ",
            $select,
            r" from names n
 cross join lateral (select n.name, latest.* from spawn_node_latest('node.joined', n.name, 0) latest) j
  left join lateral spawn_node_latest('node.lost', j.name, j.seq) l on true
  left join lateral spawn_node_latest('node.back', j.name, j.seq) b on true
  left join lateral spawn_node_latest('node.heartbeat', j.name, j.seq) h on true
  left join lateral spawn_node_latest('node.drained', j.name, j.seq) d on true
  left join lateral spawn_node_latest('node.undrained', j.name, j.seq) u on true
  left join lateral spawn_node_latest('node.removed', j.name, j.seq) r on true
 where r.id is null) x"
        )
    };
}

/// Every node joined and not removed, by name: its join record's fields
/// with `join` (the event id) and `joined_ms`; how the primary reaches it,
/// `transport` (`ssh` for a join that says none), `endpoint` (a tcp or udp
/// node's; null for ssh, whose destination stays in `ssh`, which the web
/// read drops), `via` (its hops, `[]` without) and `mode` (relay with hops
/// and direct without when the join says none); `node_id`, `role`
/// (`worker` when the join says none) and `zone` (issue #87, null when the
/// join says none; a join's `pubkey` stays among its fields, which the web
/// read drops); `state`, `lost` before
/// `drained` before `live`; `lost_since_ms`, the time of its open
/// `node.lost` (null unless lost); `heartbeat`, its latest sample
/// `{at_ms, load1, mem_available, disk_free, power}` or null; `power`
/// (issue #91), its latest heartbeat's power, or its join's when no
/// heartbeat holds one, `{on_ac, battery_pct, charging, remaining_mins}`
/// or null (no battery or AC sensor, or records from before power was
/// read).
pub(crate) const NODES: &str = joined!(
    r"j.name, (j.record - 'type') || jsonb_build_object(
      'join', j.id, 'joined_ms', j.at_ms,
      'transport', coalesce(j.record ->> 'transport', 'ssh'),
      'endpoint', case when coalesce(j.record ->> 'transport', 'ssh') <> 'ssh'
                       then j.record ->> 'endpoint' end,
      'via', case when jsonb_typeof(j.record -> 'via') = 'array'
                  then j.record -> 'via' else '[]'::jsonb end,
      'mode', coalesce(j.record ->> 'mode',
                       case when jsonb_array_length(case when jsonb_typeof(j.record -> 'via') = 'array'
                                                         then j.record -> 'via' else '[]'::jsonb end) > 0
                            then 'relay' else 'direct' end),
      'node_id', j.record ->> 'node_id',
      'role', coalesce(j.record ->> 'role', 'worker'),
      'zone', j.record ->> 'zone',
      'state', case when l.seq > coalesce(b.seq, 0) then 'lost'
                    when d.seq > coalesce(u.seq, 0) then 'drained' else 'live' end,
      'lost_since_ms', case when l.seq > coalesce(b.seq, 0) then l.at_ms end,
      'heartbeat', case when h.id is not null then jsonb_build_object(
          'at_ms', h.at_ms, 'load1', h.record -> 'load1',
          'mem_available', h.record -> 'mem_available',
          'disk_free', h.record -> 'disk_free',
          'power', h.record -> 'power') end,
      'power', case when h.id is not null and h.record ? 'power' then h.record -> 'power'
                    else j.record -> 'power' end) as node"
);

/// What the heartbeat worker needs of each node: `{name, ssh, transport,
/// endpoint, via, mode, join, beat_ms, lost}` and, when its join names
/// them (issue #87), `node_id`, `pubkey`, `role` and `zone` (a coord
/// node's `node_id` and `pubkey` pin whom its probes are sealed to; a join
/// without a role is a worker's) (`transport` `ssh`,
/// `endpoint` the ssh destination, `via` `[]` and `mode` as [`NODES`] says
/// for a join that names none), `beat_ms` its latest heartbeat's time or null, `lost`
/// its latest `node.lost` since the join (with `event_id`, `at_ms`,
/// `open`, no `node.back` after it, and `alerted`, its `node_lost` alert
/// fired) or null.
pub(crate) const MEMBERS: &str = joined!(
    r"j.name, jsonb_build_object(
      'name', j.name, 'ssh', j.record ->> 'ssh',
      'transport', coalesce(j.record ->> 'transport', 'ssh'),
      'endpoint', coalesce(j.record ->> 'endpoint', j.record ->> 'ssh'),
      'via', case when jsonb_typeof(j.record -> 'via') = 'array'
                  then j.record -> 'via' else '[]'::jsonb end,
      'mode', coalesce(j.record ->> 'mode',
                       case when jsonb_array_length(case when jsonb_typeof(j.record -> 'via') = 'array'
                                                         then j.record -> 'via' else '[]'::jsonb end) > 0
                            then 'relay' else 'direct' end),
      'join', j.id, 'beat_ms', h.at_ms,
      'lost', case when l.id is not null then l.record || jsonb_build_object(
          'event_id', l.id, 'at_ms', l.at_ms, 'open', l.seq > coalesce(b.seq, 0),
          'alerted', exists (select 1 from spawn_alerts a
                              where a.key = format('node_lost:%s:%s', j.name, l.id))) end)
      || jsonb_strip_nulls(jsonb_build_object(
          'node_id', j.record ->> 'node_id', 'pubkey', j.record ->> 'pubkey',
          'role', j.record ->> 'role', 'zone', j.record ->> 'zone')) as node"
);

/// The open `node_lost` alerts, `{key: subject}`: read once per connection
/// by the heartbeat worker, which keeps them after.
const NODE_ALERTS: &str = r"
select coalesce(jsonb_object_agg(a.key, a.subject), '{}'::jsonb)
  from spawn_alerts a where a.kind = 'node_lost' and a.state = 'open'";

/// The node index (schema.sql, "The nodes"), built concurrently after the
/// schema's transaction: appends go on while it is built.
const NODE_INDEX: &str = r"
create index concurrently if not exists spawn_events_node
    on spawn_events ((kind -> 'detail' -> 'record' ->> 'type'),
                     (kind -> 'detail' -> 'record' ->> 'name'), session_seq)
    where session_id = 'session_control' and event_name = 'node.recorded'";

impl PostgresStore {
    /// A [`StoreConnector`] to this store's database and schema; no I/O.
    pub fn connector(&self, timeout: Duration) -> StoreConnector {
        StoreConnector {
            url: self.url.clone(),
            schema: self.schema.clone(),
            timeout,
        }
    }

    /// Record a node's fact through `spawn_node_record` (owner only), every
    /// string masked as every control record is. Returns the event's id.
    pub fn node_record(&self, record: &Value) -> Result<i64, StoreError> {
        Ok(self
            .query_one(
                "select spawn_node_record($1)",
                &[&crate::control_mask::masked(record)],
            )?
            .get(0))
    }

    /// Every node joined and not removed (`NODES`), an array.
    pub fn nodes(&self) -> Result<Value, StoreError> {
        self.query_json(NODES, &[])
    }

    /// The joined nodes' membership (`MEMBERS`), an array; prepared
    /// once, since the heartbeat worker reads it again and again.
    pub fn node_members(&self) -> Result<Value, StoreError> {
        self.query_json_prepared(MEMBERS, &[])
    }

    /// The open `node_lost` alerts (`NODE_ALERTS`), `{key: subject}`.
    pub fn node_alerts(&self) -> Result<Value, StoreError> {
        self.query_json(NODE_ALERTS, &[])
    }

    /// Build the node index when it is not there, or not valid (a build
    /// cut short leaves an invalid one, dropped and built again):
    /// concurrently, outside any transaction, so no append waits for it.
    /// A second migration finds it and does nothing.
    pub(crate) fn node_index(&self) -> Result<(), postgres::Error> {
        let mut client = self.lock();
        let valid: Option<bool> = client
            .query_opt(
                "select i.indisvalid from pg_index i
                  where i.indexrelid = to_regclass('spawn_events_node')",
                &[],
            )?
            .map(|row| row.get(0));
        match valid {
            Some(true) => return Ok(()),
            Some(false) => {
                client.batch_execute("drop index concurrently if exists spawn_events_node")?;
            }
            None => {}
        }
        client.batch_execute(NODE_INDEX)
    }
}
