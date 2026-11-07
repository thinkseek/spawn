//! Alerts: the loop's `alert.fired` and `alert.cleared` records, folded into
//! `spawn_alerts` (docs/fleet-architecture.md, "Alerts"; docs/storage.md,
//! "Alerts").
//!
//! The loop computes each transition from records and readings and records it
//! through the owner-only `spawn_alert_record`; the store refuses a second
//! `alert.fired` for a key, an `alert.cleared` without its `alert.fired`, and
//! a second `alert.cleared`. The reads below are the loop's way back after a
//! restart (its alerts and the facts it evaluates), and what
//! `spawn alerts --once`, `spawn alerts --follow` and `/api/alerts` show.

use serde_json::Value;
use spawn_core::port::StoreError;

use crate::PostgresStore;

/// The event watermark: the newest event id, one index-only read.
pub(crate) const WATERMARK: &str = "select coalesce(max(id), 0) from spawn_events";

/// The database's clock, the one every bound is measured by.
pub(crate) const NOW: &str = "select spawn_now_ms()";

/// The missions and tasks the loop compares a record against, with what a
/// task's delivery bound needs (its `task.assigned` record, and whether its
/// agent holds a delivered or reported task).
pub(crate) const MIRROR: &str = r"
select jsonb_build_object(
  'now_ms', spawn_now_ms(),
  'missions', coalesce((select jsonb_agg(jsonb_build_object(
        'id', m.id, 'state', m.state, 'halted', m.halted, 'accept', m.accept,
        'gated', m.gate is not null, 'gate_rejections', m.gate_rejections,
        'gate_aborts', m.gate_aborts, 'gate_room_escalated', m.gate_room_escalated)
      order by m.id) from mission_missions m), '[]'::jsonb),
  'tasks', coalesce((select jsonb_agg(jsonb_build_object(
        'id', k.id, 'mission', k.mission_id, 'state', k.state, 'agent_id', k.agent_id,
        'session_id', k.session_id, 'objective', k.objective, 'revisions', k.revisions,
        'assigned_event', (select max(e.id) from spawn_events e
                            where e.session_id = 'session_control'
                              and e.event_name = 'control.recorded'
                              and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.assigned'
                              and e.kind -> 'detail' -> 'record' ->> 'id' = k.id::text),
        'assigned_at_ms', (select max(e.recorded_at_ms) from spawn_events e
                            where e.session_id = 'session_control'
                              and e.event_name = 'control.recorded'
                              and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.assigned'
                              and e.kind -> 'detail' -> 'record' ->> 'id' = k.id::text),
        'agent_free', not exists (select 1 from mission_tasks o
                                   where o.agent_id = k.agent_id
                                     and o.state in ('delivered', 'reported')))
      order by k.id) from mission_tasks k), '[]'::jsonb))";

/// Every alert row, and the newest transition event id: the loop reads its
/// alerts back after a restart, and fires nothing twice.
pub(crate) const ROWS: &str = r"
select jsonb_build_object(
  'cursor', coalesce((select greatest(coalesce(max(fired_event_id), 0),
                                      coalesce(max(cleared_event_id), 0))
                        from spawn_alerts), 0),
  'rows', coalesce((select jsonb_agg(jsonb_build_object(
        'key', a.key, 'kind', a.kind, 'subject', a.subject, 'state', a.state,
        'notice', a.notice, 'text', a.text, 'evidence', a.evidence,
        'fired_event_id', a.fired_event_id, 'fired_at_ms', a.fired_at_ms,
        'cleared_event_id', a.cleared_event_id, 'cleared_at_ms', a.cleared_at_ms,
        'cleared_evidence', a.cleared_evidence)
      order by a.fired_at_ms, a.key) from spawn_alerts a), '[]'::jsonb))";

/// The control records after `$1`, oldest first, at most `$2`: what the loop
/// has not looked at yet. `next` is the event id it continues from (the
/// batch's last one, or `$1` when there was none).
pub(crate) const CONTROL: &str = r"
select jsonb_build_object(
  'next', coalesce((select max(x.id) from (
      select e.id from spawn_events e
       where e.session_id = 'session_control' and e.event_name = 'control.recorded'
         and e.id > $1
       order by e.id limit $2) x), $1),
  'records', coalesce((select jsonb_agg(jsonb_build_object(
        'event_id', x.id, 'at_ms', x.recorded_at_ms, 'record', x.record)
      order by x.id) from (
      select e.id, e.recorded_at_ms, e.kind -> 'detail' -> 'record' as record
        from spawn_events e
       where e.session_id = 'session_control' and e.event_name = 'control.recorded'
         and e.id > $1
       order by e.id limit $2) x), '[]'::jsonb))";

/// The records the alert conditions read, newest last, since `$1`: the ones
/// objectives 028, 029, 031 and 033 define, and the halt and resume, so a
/// loop that restarted still knows what is pending.
pub(crate) const HISTORY: &str = r"
select coalesce(jsonb_agg(jsonb_build_object(
      'event_id', x.id, 'at_ms', x.recorded_at_ms, 'record', x.record)
    order by x.id), '[]'::jsonb)
from (select e.id, e.recorded_at_ms, e.kind -> 'detail' -> 'record' as record
        from spawn_events e
       where e.session_id = 'session_control' and e.event_name = 'control.recorded'
         and e.recorded_at_ms >= $1
         and e.kind -> 'detail' -> 'record' ->> 'type' in
             ('mission.halted', 'mission.resumed', 'admission.waiting', 'admission.admitted',
              'backup.dumped', 'backup.bundled', 'backup.verified',
              'execution.interrupted', 'execution.cleared')
       order by e.id
       limit $2) x";

/// Every session's latest judgement, with the execution end after it (a
/// stop clears a stuck alert). The Observer keeps the latest judgement
/// covering the latest fact; the alert reads the same one.
pub(crate) const OBSERVATIONS: &str = r"
select coalesce(jsonb_agg(jsonb_build_object(
      'session_id', o.session_id, 'agent_id', o.agent_id, 'event_id', o.id,
      'at_ms', o.recorded_at_ms, 'observation', o.observation,
      'ended_event', o.ended_event, 'ended_cause', o.ended_cause)
    order by o.session_id), '[]'::jsonb)
from (select distinct on (e.session_id)
             e.session_id, e.agent_id, e.id, e.recorded_at_ms,
             e.kind -> 'detail' -> 'observation' as observation,
             (select x.id from spawn_events x
               where x.session_id = e.session_id and x.event_name = 'execution.ended'
                 and x.id > e.id
               order by x.id limit 1) as ended_event,
             (select x.kind -> 'detail' ->> 'cause' from spawn_events x
               where x.session_id = e.session_id and x.event_name = 'execution.ended'
                 and x.id > e.id
               order by x.id limit 1) as ended_cause
        from spawn_events e
       where e.event_name = 'observation.recorded'
       order by e.session_id, e.id desc) o";

/// Commands that finished failed after `$1`, oldest first: a notice each,
/// an owner's note excepted (the alert step skips those).
pub(crate) const FAILED_COMMANDS: &str = r"
select coalesce(jsonb_agg(jsonb_build_object(
      'command', c.id, 'agent_id', c.agent_id, 'session_id', c.session_id,
      'principal', c.principal, 'action', c.action ->> 'type',
      'key', c.idempotency_key, 'error', left(coalesce(c.error, ''), 300),
      'finished_at_ms', c.finished_at_ms)
    order by c.id), '[]'::jsonb)
from spawn_commands c
where c.succeeded = false and c.finished_at_ms > $1";

/// The alerts the windows show: every open one, and every one fired or
/// cleared since `$1` (a notice is news within the window only).
pub(crate) const ALERTS: &str = r"
select coalesce(jsonb_agg(jsonb_build_object(
      'key', a.key, 'kind', a.kind, 'subject', a.subject, 'state', a.state,
      'fired_event_id', a.fired_event_id, 'fired_at_ms', a.fired_at_ms,
      'evidence', a.evidence,
      'cleared_event_id', a.cleared_event_id, 'cleared_at_ms', a.cleared_at_ms,
      'cleared_evidence', a.cleared_evidence, 'text', a.text)
    order by a.fired_at_ms, a.key), '[]'::jsonb)
from spawn_alerts a
where a.state = 'open' or a.fired_at_ms >= $1
   or coalesce(a.cleared_at_ms, 0) >= $1";

/// The transitions recorded after event `$1`, oldest first, at most `$2`:
/// what `spawn alerts --follow` prints as they are recorded.
pub(crate) const TRANSITIONS: &str = r"
select coalesce(jsonb_agg(jsonb_build_object(
      'transition', t.transition, 'key', t.key, 'kind', t.kind, 'subject', t.subject,
      'event_id', t.event_id, 'at_ms', t.at_ms, 'evidence', t.evidence, 'notice', t.notice)
    order by t.event_id, t.transition), '[]'::jsonb)
from (select 'fired' as transition, a.key, a.kind, a.subject,
             a.fired_event_id as event_id, a.fired_at_ms as at_ms,
             a.evidence, a.notice
        from spawn_alerts a where a.fired_event_id > $1
      union all
      select 'cleared', a.key, a.kind, a.subject,
             a.cleared_event_id, a.cleared_at_ms,
             coalesce(a.cleared_evidence, '{}'::jsonb), a.notice
        from spawn_alerts a where a.cleared_event_id > $1) t
limit $2";

impl PostgresStore {
    /// Record an `alert.fired` or `alert.cleared` record (owner only).
    /// Returns the event's id; a second `fired` for a key, a `cleared`
    /// without its `fired` and a second `cleared` are refused.
    pub fn alert_record(&self, record: &Value) -> Result<i64, StoreError> {
        Ok(self
            .query_one("select spawn_alert_record($1)", &[record])?
            .get(0))
    }

    /// The newest event id ([`WATERMARK`]).
    pub fn alert_watermark(&self) -> Result<i64, StoreError> {
        Ok(self.query_one(WATERMARK, &[])?.get(0))
    }

    /// The database's clock in epoch ms ([`NOW`]).
    pub fn now_ms(&self) -> Result<i64, StoreError> {
        Ok(self.query_one(NOW, &[])?.get(0))
    }

    /// The missions and tasks the loop compares records against ([`MIRROR`]).
    pub fn alert_mirror(&self) -> Result<Value, StoreError> {
        self.query_json(MIRROR, &[])
    }

    /// Every alert row and the newest transition's event id ([`ROWS`]).
    pub fn alert_rows(&self) -> Result<Value, StoreError> {
        self.query_json(ROWS, &[])
    }

    /// The control records after `after` ([`CONTROL`]).
    pub fn alert_control(&self, after: i64, limit: i64) -> Result<Value, StoreError> {
        self.query_json(CONTROL, &[&after, &limit])
    }

    /// The alert-family records since `since_ms` ([`HISTORY`]).
    pub fn alert_history(&self, since_ms: i64, limit: i64) -> Result<Value, StoreError> {
        self.query_json(HISTORY, &[&since_ms, &limit])
    }

    /// Every session's latest judgement ([`OBSERVATIONS`]).
    pub fn alert_observations(&self) -> Result<Value, StoreError> {
        self.query_json(OBSERVATIONS, &[])
    }

    /// Commands that finished failed after `since_ms` ([`FAILED_COMMANDS`]).
    pub fn alert_failed_commands(&self, since_ms: i64) -> Result<Value, StoreError> {
        self.query_json(FAILED_COMMANDS, &[&since_ms])
    }

    /// The alerts the window shows ([`ALERTS`]).
    pub fn alerts(&self, since_ms: i64) -> Result<Value, StoreError> {
        self.query_json(ALERTS, &[&since_ms])
    }

    /// The transitions recorded after event `since` ([`TRANSITIONS`]).
    pub fn alert_transitions(&self, since: i64, limit: i64) -> Result<Value, StoreError> {
        self.query_json(TRANSITIONS, &[&since, &limit])
    }
}
