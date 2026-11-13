//! An objective's verification after its landing (objective 133): `spawn
//! verify` records one `objective.verified` through the owner-only
//! `spawn_verification_record`, and `spawn verify list` (and
//! `/api/verifications`) reads what it decides on: each objective's newest
//! landing, every verification, and Spawn's own newest deploy.
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

/// The phrase an objective's "Verified by:" paragraph starts with.
const VERIFIED_BY: &str = "Verified by:";

/// The brief's last line that starts with "Verified by:" once its leading
/// blanks are trimmed, that whole line trimmed; `None` without one. The
/// phrase quoted inside another line is not it.
#[must_use]
pub fn verified_by_line(brief: &str) -> Option<String> {
    brief
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with(VERIFIED_BY))
        .last()
        .map(str::to_string)
}

impl PostgresStore {
    /// Record an objective's verification, `objective.verified` (owner
    /// only; the shape, and that the objective landed in its mission, are
    /// checked by `spawn_verification_record`). Returns the event's id.
    pub fn verification_record(&self, record: &Value) -> Result<i64, StoreError> {
        Ok(self
            .query_one("select spawn_verification_record($1)", &[record])?
            .get(0))
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
