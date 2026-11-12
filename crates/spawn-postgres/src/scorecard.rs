//! The scorecard's facts (`spawn scorecard`, `/api/scorecard`; docs/design.md
//! §15): the control records of `session_control` and the command rows, read
//! up to a time and projected to the fields the measures read. Nothing here
//! counts or judges: spawn-cli's `scorecard` module does, from these rows and
//! the archived transcripts, so a reader of the same record gets the same
//! numbers. Both reads are one statement, one snapshot. The reader login
//! (`spawn serve`) may run them: it selects every table.

use serde_json::Value;
use spawn_core::port::StoreError;

use crate::PostgresStore;

/// The control records recorded before `$1`, in stream order, each projected
/// to `{seq, at, type, ...}`: the fields a measure reads, big text (a brief,
/// a gate's log, a report) reduced to what is compared or read from it. A
/// brief becomes its md5 (a re-issue is found by its brief); a gate's log
/// becomes `stage_ms`, the wall clock its last `| stage gate <result> <ms>ms
/// total` line names (scripts/stage.sh, scripts/rsi/gate.sh). A
/// `legacy.imported` row (from before the control stream) lends its own
/// fields, plus its `state` and times. Records of no measure (a report's text,
/// residents' and halts' records, host facts) are left out; a report is
/// kept as its time (`task.reported`, where a landing's wait for live
/// starts) and a deploy as its `ok` (`deploy.finished`, objective 081). `owner_runs_from_ms` is the
/// time of the earliest owner judging (`source.moved`,
/// `source.push_refused`) that names its gate's wall clock, read over the
/// whole log, not up to `$1`; null when none does. `live_from_ms` is the
/// time of the first `deploy.finished` of any outcome before `$1`, where
/// report to live's record begins (objective 095); null with none.
const FACTS: &str = r"
with h as materialized (
  select u.id, i.intent, i.at_ms
    from spawn_human_intents i
   cross join lateral unnest(i.commands) as u(id)
   where i.at_ms < $1)
select jsonb_build_object(
  'control', (
    select coalesce(jsonb_agg(jsonb_strip_nulls(jsonb_build_object(
             'seq', e.session_seq,
             'at', e.recorded_at_ms,
             'type', x.r ->> 'type',
             'id', coalesce(x.r -> 'id', x.r -> 'row' -> 'id'),
             'mission_id', coalesce(x.r -> 'mission_id', x.r -> 'row' -> 'mission_id'),
             'agent_id', coalesce(x.r -> 'agent_id', x.r -> 'row' -> 'agent_id'),
             'session_id', coalesce(x.r -> 'session_id', x.r -> 'row' -> 'session_id'),
             'created_by', coalesce(x.r -> 'created_by', x.r -> 'row' -> 'created_by'),
             'objective', coalesce(x.r -> 'objective', x.r -> 'row' -> 'objective'),
             'brief', to_jsonb(md5(coalesce(x.r ->> 'brief', x.r -> 'row' ->> 'brief'))),
             'verdict', x.r -> 'verdict',
             'accepted', x.r -> 'accepted',
             'ok', x.r -> 'ok',
             'by', x.r -> 'by',
             'gate', coalesce(x.r -> 'gate', x.r -> 'row' -> 'gate'),
             'accept', coalesce(x.r -> 'accept', x.r -> 'row' -> 'accept'),
             'gate_ms', case when jsonb_typeof(x.r -> 'gate_ms') = 'number'
                             then x.r -> 'gate_ms' end,
             -- An owner judging's mission: the one whose gate judged it.
             'mission', case when jsonb_typeof(x.r -> 'mission') = 'number'
                             then x.r -> 'mission' end,
             'stage_ms', (select to_jsonb((s.m[1])::bigint)
                            from regexp_matches(x.r ->> 'log',
                                   '^[ \t]*\| stage gate [^ \t\n]+ ([0-9]{1,15})ms total[ \t\r]*$',
                                   'gn') with ordinality as s(m, i)
                           order by s.i desc limit 1),
             'table', x.r -> 'table',
             'state', x.r -> 'row' -> 'state',
             'created_at_ms', x.r -> 'row' -> 'created_at_ms',
             'updated_at_ms', x.r -> 'row' -> 'updated_at_ms',
             'delivered_at_ms', x.r -> 'row' -> 'delivered_at_ms'))
             order by e.session_seq), '[]'::jsonb)
      from spawn_events e
     -- `offset 0` keeps the record a value of its own: inlined, each of the
     -- thirty `x.r` above would take it out of the event's `kind` (a gate's
     -- log, a brief) again, which took most of the statement's time.
     cross join lateral (select e.kind -> 'detail' -> 'record' as r offset 0) x
     where e.session_id = 'session_control' and e.event_name = 'control.recorded'
       and e.recorded_at_ms < $1
       and x.r ->> 'type' in ('mission.added', 'mission.configured', 'mission.resumed',
                              'task.assigned', 'task.delivered', 'task.gated', 'task.landed',
                              'task.reviewed', 'task.cancelled', 'task.aborted',
                              'task.over_budget', 'legacy.imported',
                              'source.moved', 'source.push_refused',
                              'task.reported', 'deploy.finished')),
  'commands', (
    select coalesce(jsonb_agg(jsonb_strip_nulls(jsonb_build_object(
             'id', c.id,
             'principal', c.principal,
             'key', c.idempotency_key,
             'session_id', c.session_id,
             'action', c.action ->> 'type',
             'created_at_ms', c.created_at_ms,
             'finished_at_ms', r.finished_at_ms,
             'succeeded', r.succeeded,
             'error', case when r.succeeded is false and c.principal = 'spawn-dispatch'
                                and c.idempotency_key like 'deliver-%' then c.error end,
             'stalls', t.stalls,
             'intent', h.intent,
             'intent_at_ms', h.at_ms))
             order by c.id), '[]'::jsonb)
      from spawn_commands c
     -- A command's result is a fact of the moment it finished: one that
     -- finished at or after `$1` is unfinished in these facts, so the same
     -- `$1` answers the same rows whenever it is asked.
     cross join lateral (
       select case when c.finished_at_ms < $1 then c.finished_at_ms end as finished_at_ms,
              case when c.finished_at_ms < $1 then c.succeeded end as succeeded) r
     cross join lateral (
       select case when c.principal = 'spawn-dispatch' and c.action ->> 'type' = 'input'
                        and jsonb_typeof(c.action -> 'tags') = 'array'
                   then (select jsonb_agg(g.tag order by g.n)
                           from jsonb_array_elements_text(c.action -> 'tags')
                                with ordinality as g(tag, n)
                          where g.tag ~ '(^| )stall-[0-9]+-[a-z_]+-[0-9]+$') end as stalls) t
     left join h on h.id = c.id
     where c.created_at_ms < $1
       and (c.principal in ('human', 'spawn')
            or r.succeeded is false
            or (c.principal = 'spawn-dispatch'
                and (c.idempotency_key like 'deliver-%' or t.stalls is not null)))),
  -- From when the owner's judgings are counted (objective 076): the
  -- earliest owner judging that carries its gate's wall clock, whatever
  -- `$1` is; null when none does.
  'owner_runs_from_ms', (
    select min(e.recorded_at_ms)
      from spawn_events e
     cross join lateral (select e.kind -> 'detail' -> 'record' as r offset 0) x
     where e.session_id = 'session_control' and e.event_name = 'control.recorded'
       and x.r ->> 'type' in ('source.moved', 'source.push_refused')
       and jsonb_typeof(x.r -> 'gate_ms') = 'number'),
  -- Where the live record begins (objective 095): the first
  -- `deploy.finished` of any outcome, over the whole log before `$1`
  -- (whatever the window); null when there is none.
  'live_from_ms', (
    select min(e.recorded_at_ms)
      from spawn_events e
     cross join lateral (select e.kind -> 'detail' -> 'record' as r offset 0) x
     where e.session_id = 'session_control' and e.event_name = 'control.recorded'
       and e.recorded_at_ms < $1
       and x.r ->> 'type' = 'deploy.finished'))";

impl PostgresStore {
    /// The database's clock (`spawn_now_ms()`), in epoch milliseconds: the
    /// scorecard's "now", never this host's.
    pub fn database_now_ms(&self) -> Result<i64, StoreError> {
        Ok(self.query_one("select spawn_now_ms()", &[])?.get(0))
    }

    /// The scorecard's facts recorded before `until_ms`, in one snapshot:
    /// `{control: [...], commands: [...]}` ([`FACTS`]). `commands` holds the
    /// rows a measure reads: the owner's (`human`, `spawn`), every failed
    /// one, the dispatcher's deliveries (`deliver-<task>-<revision>` and their
    /// retries; `error` only on a failed one) and its digests that carry a
    /// stall tag (`stalls`: those tags, `<session> stall-<task>-<step>-<n>`).
    /// A person's command names its intent (`spawn_human_intents`: `intent`,
    /// its first command's id, and `intent_at_ms`, that one's creation): a
    /// refused command sent again continues the intent of the one before.
    /// A command is in them from its creation and its result (`succeeded`,
    /// `finished_at_ms`, `error`) from its finish: one created before
    /// `until_ms` that finished later is unfinished here.
    pub fn scorecard_facts(&self, until_ms: i64) -> Result<Value, StoreError> {
        self.query_json(FACTS, &[&until_ms])
    }

    /// When the live record starts: the first `deploy.finished` of any
    /// outcome (objective 095's record start), or `None` without one.
    pub fn first_deploy_finished_ms(&self) -> Result<Option<i64>, StoreError> {
        Ok(self
            .query_one(
                "select min(e.recorded_at_ms) from spawn_events e
                  where e.session_id = 'session_control' and e.event_name = 'control.recorded'
                    and e.kind -> 'detail' -> 'record' ->> 'type' = 'deploy.finished'",
                &[],
            )?
            .get(0))
    }
}
