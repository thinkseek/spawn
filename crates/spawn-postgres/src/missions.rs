//! Missions, tasks and residents (docs/fleet-architecture.md §3.2).
//!
//! Every decision goes through the checked SQL functions, so the supervisor's
//! restricted login and the owner's CLI share one set of rules. The
//! dispatcher's delivery bookkeeping (delivered, reported) runs as the owner.

use serde_json::Value;

use crate::{PostgresStore, backend};
use spawn_core::port::StoreError;

/// One task the dispatcher has to look at, with what it needs to deliver it.
const DELIVERY: &str = r"
select coalesce(jsonb_agg(row_to_json(t) order by t.id), '[]'::jsonb)
from (
  select k.id, k.mission_id, k.kind, k.agent_id, k.session_id, k.brief, k.state,
         k.revisions, k.review_note, k.command_id,
         m.text as mission_text, m.sources as mission_sources,
         r.charter, r.name as resident
    from mission_tasks k
    join mission_missions m on m.id = k.mission_id
    left join mission_residents r on r.agent_id = k.agent_id
   where k.state in ('assigned', 'delivered', 'reported', 'revise')
      or (k.state in ('accepted', 'cancelled') and k.kind = 'ephemeral'
          and k.updated_at_ms > spawn_now_ms() - 86400000)
) t";

/// What the supervisor should hear about, beyond worker states: open
/// missions, reports, residents approved, and human messages to workers.
const NEWS: &str = r"
select jsonb_build_object(
  'missions', coalesce((select jsonb_agg(jsonb_build_object(
                 'id', m.id, 'text', left(m.text, 400), 'origin', m.origin,
                 'sources', m.sources) order by m.id)
               from mission_missions m where m.state = 'open'), '[]'::jsonb),
  'reports', coalesce((select jsonb_agg(jsonb_build_object(
                 'id', k.id, 'mission_id', k.mission_id, 'kind', k.kind,
                 'agent_id', k.agent_id, 'session_id', k.session_id,
                 'report', left(k.report, 600)) order by k.id)
               from mission_tasks k where k.state = 'reported'), '[]'::jsonb),
  'approved', coalesce((select jsonb_agg(jsonb_build_object('name', r.name) order by r.name)
               from mission_residents r
              where r.state = 'approved'
                and r.updated_at_ms > spawn_now_ms() - 86400000), '[]'::jsonb),
  'proposed', coalesce((select jsonb_agg(jsonb_build_object(
                 'name', r.name, 'charter', left(r.charter, 400)) order by r.name)
               from mission_residents r where r.state = 'proposed'), '[]'::jsonb),
  'human', coalesce((select jsonb_agg(jsonb_build_object(
                 'command_id', c.id, 'session_id', c.session_id,
                 'text', left(coalesce(c.action ->> 'text', c.action ->> 'prompt', c.action ->> 'type'), 200))
                 order by c.id)
               from spawn_commands c
              where c.principal = 'human'
                and (c.agent_id like 'agent\_worker\_%' or c.agent_id like 'agent\_resident\_%')
                and c.created_at_ms > spawn_now_ms() - 86400000), '[]'::jsonb)
)";

/// The board of missions with their tasks, newest first.
const BOARD: &str = r"
select jsonb_build_object(
  'missions', coalesce((select jsonb_agg(row_to_json(x) order by x.id desc) from (
      select m.id, m.text, m.origin, m.sources, m.state, m.summary,
             m.created_at_ms, m.updated_at_ms,
             coalesce((select jsonb_agg(jsonb_build_object(
                 'id', k.id, 'kind', k.kind, 'agent_id', k.agent_id,
                 'session_id', k.session_id, 'brief', left(k.brief, 600),
                 'state', k.state, 'revisions', k.revisions,
                 'review_note', k.review_note, 'report', k.report,
                 'created_by', k.created_by, 'created_at_ms', k.created_at_ms,
                 'updated_at_ms', k.updated_at_ms) order by k.id)
               from mission_tasks k where k.mission_id = m.id), '[]'::jsonb) as tasks
        from mission_missions m
       order by m.id desc limit $1) x), '[]'::jsonb),
  'residents', coalesce((select jsonb_agg(row_to_json(r) order by r.name) from (
      select name, agent_id, session_id, charter, state, proposed_by, approved_by,
             created_at_ms, updated_at_ms from mission_residents) r), '[]'::jsonb)
)";

impl PostgresStore {
    /// Add a mission; returns its id. `origin` is ignored for the
    /// supervisor's login, which is always recorded as itself.
    pub fn mission_add(
        &self,
        text: &str,
        sources: &Value,
        origin: &str,
    ) -> Result<i64, StoreError> {
        let mut client = self.lock();
        let row = client
            .query_one(
                "select spawn_mission_add($1, $2, $3)",
                &[&text, sources, &origin],
            )
            .map_err(|error| backend(&error))?;
        Ok(row.get(0))
    }

    /// Close a mission as `done` or `dropped`.
    pub fn mission_close(
        &self,
        mission: i64,
        state: &str,
        summary: &str,
    ) -> Result<(), StoreError> {
        let mut client = self.lock();
        client
            .execute(
                "select spawn_mission_close($1, $2, $3)",
                &[&mission, &state, &summary],
            )
            .map_err(|error| backend(&error))?;
        Ok(())
    }

    /// Assign a task; returns its id.
    pub fn task_assign(
        &self,
        mission: i64,
        kind: &str,
        worker: &str,
        brief: &str,
    ) -> Result<i64, StoreError> {
        let mut client = self.lock();
        let row = client
            .query_one(
                "select spawn_task_assign($1, $2, $3, $4)",
                &[&mission, &kind, &worker, &brief],
            )
            .map_err(|error| backend(&error))?;
        Ok(row.get(0))
    }

    /// Accept a report, or send the task back with a note.
    pub fn task_review(&self, task: i64, accept: bool, note: &str) -> Result<(), StoreError> {
        let mut client = self.lock();
        client
            .execute(
                "select spawn_task_review($1, $2, $3)",
                &[&task, &accept, &note],
            )
            .map_err(|error| backend(&error))?;
        Ok(())
    }

    /// Cancel a task that is not closed yet.
    pub fn task_cancel(&self, task: i64) -> Result<(), StoreError> {
        let mut client = self.lock();
        client
            .execute("select spawn_task_cancel($1)", &[&task])
            .map_err(|error| backend(&error))?;
        Ok(())
    }

    /// Propose a resident (or revise a pending proposal).
    pub fn resident_propose(&self, name: &str, charter: &str) -> Result<(), StoreError> {
        let mut client = self.lock();
        client
            .execute("select spawn_resident_propose($1, $2)", &[&name, &charter])
            .map_err(|error| backend(&error))?;
        Ok(())
    }

    /// Approve a proposed resident. Only the owner's login (a human) may.
    pub fn resident_approve(&self, name: &str) -> Result<(), StoreError> {
        let mut client = self.lock();
        client
            .execute("select spawn_resident_approve($1)", &[&name])
            .map_err(|error| backend(&error))?;
        Ok(())
    }

    /// Retire a resident; its queued tasks are cancelled.
    pub fn resident_retire(&self, name: &str) -> Result<(), StoreError> {
        let mut client = self.lock();
        client
            .execute("select spawn_resident_retire($1)", &[&name])
            .map_err(|error| backend(&error))?;
        Ok(())
    }

    /// Missions with their tasks, and the resident roster.
    pub fn mission_board(&self, limit: i64) -> Result<Value, StoreError> {
        let mut client = self.lock();
        let row = client
            .query_one(BOARD, &[&limit])
            .map_err(|error| backend(&error))?;
        Ok(row.get(0))
    }

    /// Tasks the dispatcher has to deliver, watch or wind down.
    pub fn delivery_rows(&self) -> Result<Value, StoreError> {
        let mut client = self.lock();
        let row = client
            .query_one(DELIVERY, &[])
            .map_err(|error| backend(&error))?;
        Ok(row.get(0))
    }

    /// Missions, reports, approvals and human messages for the digest.
    pub fn mission_news(&self) -> Result<Value, StoreError> {
        let mut client = self.lock();
        let row = client
            .query_one(NEWS, &[])
            .map_err(|error| backend(&error))?;
        Ok(row.get(0))
    }

    /// Approved residents that have run before: the ones the liveness rule
    /// keeps, as (agent, session, charter).
    pub fn residents_to_keep(&self) -> Result<Vec<(String, String, String)>, StoreError> {
        let mut client = self.lock();
        let rows = client
            .query(
                "select r.agent_id, r.session_id, r.charter
                   from mission_residents r
                  where r.state = 'approved'
                    and exists (select 1 from spawn_events e
                                 where e.session_id = r.session_id
                                   and e.event_name = 'execution.started')
                  order by r.name",
                &[],
            )
            .map_err(|error| backend(&error))?;
        Ok(rows
            .iter()
            .map(|row| (row.get(0), row.get(1), row.get(2)))
            .collect())
    }

    /// Residents that are retired, for winding down their sessions.
    pub fn retired_residents(&self) -> Result<Vec<(String, String)>, StoreError> {
        let mut client = self.lock();
        let rows = client
            .query(
                "select agent_id, session_id from mission_residents where state = 'retired'",
                &[],
            )
            .map_err(|error| backend(&error))?;
        Ok(rows.iter().map(|row| (row.get(0), row.get(1))).collect())
    }

    /// Where an agent's workspace comes from: `None` when no mission owns
    /// the agent, `Some(None)` for a mission without sources (a scratch
    /// workspace), `Some(Some(source))` otherwise. The agent's first task
    /// decides, so a resident's workspace never jumps between sources.
    pub fn agent_source(&self, agent: &str) -> Result<Option<Option<String>>, StoreError> {
        let mut client = self.lock();
        let rows = client
            .query(
                "select m.sources ->> 0
                   from mission_tasks k join mission_missions m on m.id = k.mission_id
                  where k.agent_id = $1
                  order by k.id limit 1",
                &[&agent],
            )
            .map_err(|error| backend(&error))?;
        Ok(rows.first().map(|row| row.get::<_, Option<String>>(0)))
    }

    /// Record that a task's delivery command was submitted.
    pub fn task_delivered(&self, task: i64, command: i64) -> Result<(), StoreError> {
        let mut client = self.lock();
        client
            .execute(
                "update mission_tasks set state = 'delivered', command_id = $2,
                        updated_at_ms = spawn_now_ms()
                  where id = $1 and state in ('assigned', 'revise')",
                &[&task, &command],
            )
            .map_err(|error| backend(&error))?;
        Ok(())
    }

    /// Store a delivered task's report.
    pub fn task_reported(&self, task: i64, report: &str) -> Result<bool, StoreError> {
        let mut client = self.lock();
        let changed = client
            .execute(
                "update mission_tasks set state = 'reported', report = $2,
                        updated_at_ms = spawn_now_ms()
                  where id = $1 and state = 'delivered'",
                &[&task, &report],
            )
            .map_err(|error| backend(&error))?;
        Ok(changed == 1)
    }
}
