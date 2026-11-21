//! The feature map's two reads that no other page needs: the newest deploy
//! records (what has actually been shipped, and when) and every open
//! mission's current blocker (a halt, the owner's pause, a red main).
//!
//! The map itself is derived in `spawn serve` from reads that already
//! exist: an objective mission's backlog and tasks (`objective_missions`),
//! the landings and verifications (`verification_facts`), these deploys and
//! these blockers. Nothing here stores a capability's status: the four
//! states are computed from the records every time the page reads, so the
//! log stays the only truth.

use serde_json::Value;
use spawn_core::port::StoreError;

use crate::PostgresStore;

/// The newest `deploy.finished` records, newest first: `[{event, at_ms, ok,
/// head, by, reason}]`. `$1` is the limit. A deploy is how a change reached
/// the running controller (`deploy/deploy.sh`), so it is the only evidence a
/// feature map may call live.
pub(crate) const DEPLOY_RECORDS: &str = r"
select coalesce(jsonb_agg(jsonb_build_object(
         'event', e.id, 'at_ms', e.recorded_at_ms,
         'ok', e.kind -> 'detail' -> 'record' -> 'ok',
         'head', e.kind -> 'detail' -> 'record' ->> 'head',
         'by', e.kind -> 'detail' -> 'record' ->> 'by',
         'reason', e.kind -> 'detail' -> 'record' ->> 'reason')
         order by e.session_seq desc), '[]'::jsonb)
  from (select e.id, e.session_seq, e.recorded_at_ms, e.kind
          from spawn_events e
         where e.session_id = 'session_control' and e.event_name = 'control.recorded'
           and e.kind -> 'detail' -> 'record' ->> 'type' = 'deploy.finished'
         order by e.session_seq desc limit $1) e";

/// Every open or active mission's current blocker, by mission:
/// `[{id, halt, paused, main_red}]`, each of the three null when absent.
/// `halt` is the `mission.halted` record the fold keeps (`{reason, event,
/// at_ms}`), `paused` the owner's `mission.paused` (`{reason, event,
/// at_ms}`), and `main_red` the source's red main (`{scenario, since_ms,
/// red_ms}`), which stops the integrator starting a gate. A blocker names a
/// fact, not a judgement: the feature map decides which capabilities it
/// holds back.
pub(crate) const MISSION_BLOCKERS: &str = r"
select coalesce(jsonb_agg(jsonb_build_object(
         'id', m.id,
         'halt', case when m.halted is not null then jsonb_build_object(
                     'reason', m.halted, 'event', m.halt_event, 'at_ms', m.halt_at_ms) end,
         'paused', case when m.pause_event is not null then jsonb_build_object(
                     'reason', m.pause_reason, 'event', m.pause_event,
                     'at_ms', m.pause_at_ms) end,
         'main_red', (select jsonb_build_object(
                             'scenario', r.scenario, 'since_ms', r.since_ms,
                             'red_ms', r.red_ms)
                        from mission_source_reds r
                       where r.source = m.sources ->> 0))
         order by m.id), '[]'::jsonb)
  from mission_missions m
 where m.state in ('open', 'active')";

impl PostgresStore {
    /// The newest `deploy.finished` records, newest first, at most `limit`
    /// (clamped to 1..=100), each `{event, at_ms, ok, head, by, reason}`
    /// ([`DEPLOY_RECORDS`]).
    pub fn deploy_facts(&self, limit: i64) -> Result<Value, StoreError> {
        self.query_json(DEPLOY_RECORDS, &[&limit.clamp(1, 100)])
    }

    /// Every open or active mission's blocker, by mission
    /// ([`MISSION_BLOCKERS`]): `[{id, halt, paused, main_red}]`.
    pub fn mission_blockers(&self) -> Result<Value, StoreError> {
        self.query_json(MISSION_BLOCKERS, &[])
    }
}
