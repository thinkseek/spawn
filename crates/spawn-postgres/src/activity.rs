//! What a delivered task's worker did since the task reached it (objective
//! 211, GitHub #63 part 1): between delivered and reported the board had no
//! signal, and a task 30 to 74 minutes in delivered could not be told
//! working from hung. The data is the worker's own session: its engine's
//! output since the task's delivery.

use serde_json::Value;
use spawn_core::port::StoreError;

use crate::PostgresStore;

/// Every task in state `delivered`, keyed by task id: `{since_ms, at_ms,
/// event, tool_calls}`.
///
/// - `since_ms`: the time of its latest `task.delivered` record (a task sent
///   back and delivered again counts from the latest delivery).
/// - `at_ms`, `event`: its session's latest engine output after that record
///   (by event id), source `claude` as its transcript gives it:
///   `model.usage`, `tool.started`, `tool.completed`, `tool.failed`,
///   `turn.stopped`, `turn.failed`, `job.started`, `job.ended`; both null
///   when there is none. Never the Bridge's `screen.observed` (a pane stuck
///   on a spinner is refreshed as busy), the Observer's judgements or a
///   checkpoint, and never the engine's input: `message.received` (a prompt
///   reaching the engine) and `message.taken` (the engine taking a typed
///   command's marker) come at +0 s after every delivery as the brief
///   arrives, so a worker hung right after its brief would read as moved.
/// - `tool_calls`: its session's `tool.started` events (source `claude`)
///   after that record.
///
/// Bounded by the delivered tasks and their tool calls since delivery, never
/// by how long a session is: the delivery record through
/// `spawn_events_control_record`, the latest output as one backward step of
/// the `(session_id, event_name, id)` index per output name down to the
/// record's id, the calls as that index's range over the session's
/// `tool.started` from the record's id. Kept out of `MISSION_BOARD`: the
/// supervisor's login reads the board and may not read `spawn_events`.
pub(crate) const TASK_ACTIVITY: &str = r"
select coalesce(jsonb_object_agg(t.id::text, jsonb_build_object(
         'since_ms', d.recorded_at_ms, 'at_ms', o.recorded_at_ms, 'event', o.event_name,
         'tool_calls', c.calls)), '{}'::jsonb)
  from mission_tasks t
 cross join lateral (
       select e.id, e.recorded_at_ms
         from spawn_events e
        where e.session_id = 'session_control' and e.event_name = 'control.recorded'
          and e.kind -> 'detail' -> 'record' ->> 'id' = t.id::text
          and e.kind -> 'detail' -> 'record' ->> 'type' = 'task.delivered'
        order by e.session_seq desc
        limit 1) d
  left join lateral (
       select l.id, l.recorded_at_ms, l.event_name
         from unnest(array['model.usage', 'tool.started', 'tool.completed', 'tool.failed',
                           'turn.stopped', 'turn.failed', 'job.started', 'job.ended']) n(name)
        cross join lateral (
              select e.id, e.recorded_at_ms, e.event_name
                from spawn_events e
               where e.session_id = t.session_id and e.event_name = n.name
                 and e.id > d.id and e.source = 'claude'
               order by e.id desc
               limit 1) l
        order by l.id desc
        limit 1) o on true
 cross join lateral (
       select count(*) as calls
         from spawn_events e
        where e.session_id = t.session_id and e.event_name = 'tool.started'
          and e.id > d.id and e.source = 'claude') c
 where t.state = 'delivered'";

impl PostgresStore {
    /// What each delivered task's worker did since the task reached it
    /// ([`TASK_ACTIVITY`]), keyed by task id. Needs read access to
    /// `spawn_events` (the owner, the dashboard's reader).
    pub fn task_activity(&self) -> Result<Value, StoreError> {
        self.query_json(TASK_ACTIVITY, &[])
    }
}
