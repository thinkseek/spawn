//! An objective's verification after its landing (objective 133): `spawn
//! verify` records one `objective.verified` through the owner-only
//! `spawn_verification_record`, and `spawn verify list` (and
//! `/api/verifications`) reads what it decides on: each objective's newest
//! landing, every verification, and Spawn's own newest deploy. The alert
//! step counts the landed objectives left unverified
//! (`unverified_landings`, objective 135).
//!
//! Each part names one record type, read through
//! `spawn_events_control_record_type` (type, time, id), never the whole
//! control stream; a landing's task is read from `mission_tasks` by id.

use serde_json::Value;
use spawn_core::port::StoreError;

use crate::PostgresStore;

/// What the list decides on, in one snapshot (one statement):
/// - `landings`: per mission and objective, its newest `task.landed` (by
///   the record's time, then its id) `{mission, objective, task,
///   landed_ms, brief}`, the task's brief in full;
/// - `verifications`: every `objective.verified` `{mission, objective,
///   result, by, how, at_ms}`, oldest first;
/// - `deploy`: the newest `deploy.finished` `{at_ms, head, ok, reason}`,
///   or null.
const FACTS: &str = r"
select jsonb_build_object(
  'landings', coalesce((
    select jsonb_agg(jsonb_build_object('mission', l.mission_id, 'objective', l.objective,
                                        'task', l.task, 'landed_ms', l.at_ms, 'brief', l.brief)
                     order by l.mission_id, l.objective)
      from (select distinct on (t.mission_id, t.objective)
                   t.mission_id, t.objective, t.id as task, e.recorded_at_ms as at_ms, t.brief
              from spawn_events e
              join mission_tasks t on t.id = (e.kind -> 'detail' -> 'record' ->> 'id')::bigint
             where e.session_id = 'session_control' and e.event_name = 'control.recorded'
               and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.landed'
               and t.objective is not null
             order by t.mission_id, t.objective, e.recorded_at_ms desc, e.id desc) l), '[]'),
  'verifications', coalesce((
    select jsonb_agg(jsonb_build_object('mission', r -> 'mission', 'objective', r -> 'objective',
                                        'result', r -> 'result', 'by', r -> 'by',
                                        'how', r -> 'how', 'at_ms', e.recorded_at_ms)
                     order by e.recorded_at_ms, e.id)
      from spawn_events e
      cross join lateral (select e.kind -> 'detail' -> 'record' as r) x
     where e.session_id = 'session_control' and e.event_name = 'control.recorded'
       and e.kind -> 'detail' -> 'record' ->> 'type' = 'objective.verified'), '[]'),
  'deploy', (
    select jsonb_build_object('at_ms', e.recorded_at_ms,
                              'head', e.kind -> 'detail' -> 'record' -> 'head',
                              'ok', e.kind -> 'detail' -> 'record' -> 'ok',
                              'reason', e.kind -> 'detail' -> 'record' -> 'reason')
      from spawn_events e
     where e.session_id = 'session_control' and e.event_name = 'control.recorded'
       and e.kind -> 'detail' -> 'record' ->> 'type' = 'deploy.finished'
     order by e.recorded_at_ms desc, e.id desc
     limit 1))";

/// The alert step's polled read (objective 135, `unverified_landings`):
/// the objectives whose newest landing (the newest `task.landed` of a task
/// of that mission and objective, by its record's time, then its id) is at
/// or after `$1` and whose newest `objective.verified` after that landing
/// is not a pass (none, or a fail: `spawn verify list`'s rule), counted:
/// `{unverified, old, newest_old_ms}`, `old` those that landed at or before
/// `$2`, `newest_old_ms` the newest of those landings (null: none).
///
/// Each part names one record type with the window, read through
/// `spawn_events_control_record_type` (type, time, id), never the whole
/// control stream: the window is applied before the newest landing is
/// picked (an objective's newest landing is at or after `$1` exactly when
/// it has a landing at or after it), and a verification after such a
/// landing is at or after `$1` too. The verifications are read once, not
/// once per landing. It grows with the window, not the log.
const UNVERIFIED: &str = r"
with landed as (
  select distinct on (t.mission_id, t.objective)
         t.mission_id, t.objective, e.recorded_at_ms as at_ms
    from spawn_events e
    join mission_tasks t on t.id = (e.kind -> 'detail' -> 'record' ->> 'id')::bigint
   where e.session_id = 'session_control' and e.event_name = 'control.recorded'
     and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.landed'
     and e.recorded_at_ms >= $1
     and t.objective is not null
   order by t.mission_id, t.objective, e.recorded_at_ms desc, e.id desc
), verified as materialized (
  select (e.kind -> 'detail' -> 'record' ->> 'mission')::bigint as mission_id,
         (e.kind -> 'detail' -> 'record' ->> 'objective')::integer as objective,
         e.kind -> 'detail' -> 'record' ->> 'result' as result,
         e.recorded_at_ms as at_ms, e.id
    from spawn_events e
   where e.session_id = 'session_control' and e.event_name = 'control.recorded'
     and e.kind -> 'detail' -> 'record' ->> 'type' = 'objective.verified'
     and e.recorded_at_ms >= $1
), unverified as (
  select l.at_ms
    from landed l
   where coalesce((select v.result from verified v
                    where v.mission_id = l.mission_id and v.objective = l.objective
                      and v.at_ms > l.at_ms
                    order by v.at_ms desc, v.id desc
                    limit 1), '') <> 'pass'
)
select jsonb_build_object(
  'unverified', count(*),
  'old', count(*) filter (where at_ms <= $2),
  'newest_old_ms', max(at_ms) filter (where at_ms <= $2))
  from unverified";

/// The claimed scratch folders' objectives that landed (objective 140): of
/// the pairs `unnest($1::bigint[], $2::integer[])` (missions and
/// objectives), those with a `task.landed` of a task of that mission and
/// objective (the fleet's code landed), each once, in order. The landings
/// are read through `spawn_events_control_record_type` (type, time, id),
/// never the whole control stream, then each task by its primary key. No
/// time window: it grows with landings only.
pub const CLAIMED_LANDED: &str = r"
select distinct t.mission_id, t.objective
  from spawn_events e
  join mission_tasks t on t.id = (e.kind -> 'detail' -> 'record' ->> 'id')::bigint
 where e.session_id = 'session_control' and e.event_name = 'control.recorded'
   and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.landed'
   and (t.mission_id, t.objective) in
       (select c.mission_id, c.objective from unnest($1::bigint[], $2::integer[]) as c(mission_id, objective))
 order by t.mission_id, t.objective";

/// The phrase an objective's "Verified by:" paragraph starts with.
const VERIFIED_BY: &str = "Verified by:";

/// The brief's last line that starts with "Verified by:" once its leading
/// blanks are trimmed, that whole line trimmed; `None` without one. The
/// phrase quoted inside another line is not it.
#[must_use]
pub fn verified_by_line(brief: &str) -> Option<String> {
    brief
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| line.starts_with(VERIFIED_BY))
        .map(str::to_string)
}

impl PostgresStore {
    /// Record an objective's verification, `objective.verified` (owner
    /// only; the shape, and that the objective landed in its mission, are
    /// checked by `spawn_verification_record`). Returns the event's id.
    pub fn verification_record(&self, record: &Value) -> Result<i64, StoreError> {
        Ok(self
            .query_one(
                "select spawn_verification_record($1)",
                &[&crate::control_mask::masked(record)],
            )?
            .get(0))
    }

    /// The landed objectives not verified since `since_ms`, and how many of
    /// them landed at or before `old_at_ms` ([`UNVERIFIED`]): `{unverified,
    /// old, newest_old_ms}`. The alert step reads it at every reading.
    pub fn unverified_landings(&self, since_ms: i64, old_at_ms: i64) -> Result<Value, StoreError> {
        self.query_json_prepared(UNVERIFIED, &[&since_ms, &old_at_ms])
    }

    /// Which of the claimed folders' `(mission, objective)` pairs landed
    /// ([`CLAIMED_LANDED`]), each once, in order. An objective out of an
    /// `integer`'s range never landed.
    pub fn claimed_landed(&self, pairs: &[(i64, i64)]) -> Result<Vec<(i64, i64)>, StoreError> {
        let (missions, objectives): (Vec<i64>, Vec<i32>) = pairs
            .iter()
            .filter_map(|&(mission, objective)| Some((mission, i32::try_from(objective).ok()?)))
            .unzip();
        if missions.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self
            .query_prepared(CLAIMED_LANDED, &[&missions, &objectives])?
            .iter()
            .map(|row| (row.get::<_, i64>(0), i64::from(row.get::<_, i32>(1))))
            .collect())
    }

    /// What `spawn verify list` decides on ([`FACTS`]): each landing with
    /// `verified_by`, its brief's "Verified by:" line or null
    /// ([`verified_by_line`]), in place of the brief.
    pub fn verification_facts(&self) -> Result<Value, StoreError> {
        let mut facts = self.query_json(FACTS, &[])?;
        if let Some(landings) = facts["landings"].as_array_mut() {
            for landing in landings {
                let line = landing["brief"].as_str().and_then(verified_by_line);
                if let Some(object) = landing.as_object_mut() {
                    object.remove("brief");
                    object.insert("verified_by".to_string(), Value::from(line));
                }
            }
        }
        Ok(facts)
    }
}

#[cfg(test)]
mod tests {
    use super::verified_by_line;

    #[test]
    fn the_verified_by_line_is_the_last_line_starting_with_it() {
        assert_eq!(verified_by_line("Objective 1."), None);
        assert_eq!(
            verified_by_line("Every objective carries a \"Verified by:\" paragraph."),
            None
        );
        assert_eq!(
            verified_by_line("a\n  Verified by: one.  \nb\nVerified by: two.\nc").as_deref(),
            Some("Verified by: two.")
        );
    }
}
