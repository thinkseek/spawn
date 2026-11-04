//! Missions, tasks and residents (docs/fleet-architecture.md §3.2).
//!
//! Every decision goes through the checked SQL functions, so the supervisor's
//! restricted login and the owner's CLI share one set of rules. The
//! dispatcher's delivery bookkeeping (delivered, reported) runs as the owner.
//! The SQL is in [`crate::queries::missions`].

use serde_json::Value;
use spawn_core::port::StoreError;

use crate::PostgresStore;
use crate::queries::missions;

/// Checked decisions.
impl PostgresStore {
    /// Add a mission; returns its id. `origin` is ignored for the
    /// supervisor's login, which is always recorded as itself.
    pub fn mission_add(
        &self,
        text: &str,
        sources: &Value,
        origin: &str,
    ) -> Result<i64, StoreError> {
        let row = self.query_one(missions::MISSION_ADD, &[&text, sources, &origin])?;
        Ok(row.get(0))
    }

    /// Close a mission as `done` or `dropped`.
    pub fn mission_close(
        &self,
        mission: i64,
        state: &str,
        summary: &str,
    ) -> Result<(), StoreError> {
        self.execute(missions::MISSION_CLOSE, &[&mission, &state, &summary])?;
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
        let row = self.query_one(missions::TASK_ASSIGN, &[&mission, &kind, &worker, &brief])?;
        Ok(row.get(0))
    }

    /// Accept a report, or send the task back with a note.
    pub fn task_review(&self, task: i64, accept: bool, note: &str) -> Result<(), StoreError> {
        self.execute(missions::TASK_REVIEW, &[&task, &accept, &note])?;
        Ok(())
    }

    /// Cancel a task that is not closed yet.
    pub fn task_cancel(&self, task: i64) -> Result<(), StoreError> {
        self.execute(missions::TASK_CANCEL, &[&task])?;
        Ok(())
    }

    /// Propose a resident (or revise a pending proposal).
    pub fn resident_propose(&self, name: &str, charter: &str) -> Result<(), StoreError> {
        self.execute(missions::RESIDENT_PROPOSE, &[&name, &charter])?;
        Ok(())
    }

    /// Approve a proposed resident. Only the owner's login (a human) may.
    pub fn resident_approve(&self, name: &str) -> Result<(), StoreError> {
        self.execute(missions::RESIDENT_APPROVE, &[&name])?;
        Ok(())
    }

    /// Retire a resident; its queued tasks are cancelled.
    pub fn resident_retire(&self, name: &str) -> Result<(), StoreError> {
        self.execute(missions::RESIDENT_RETIRE, &[&name])?;
        Ok(())
    }
}

/// Reads for the board, the dispatcher and the supervisor's digest.
impl PostgresStore {
    /// Missions with their tasks, and the resident roster.
    pub fn mission_board(&self, limit: i64) -> Result<Value, StoreError> {
        self.query_json(missions::MISSION_BOARD, &[&limit])
    }

    /// Tasks the dispatcher has to deliver, watch or wind down.
    pub fn delivery_rows(&self) -> Result<Value, StoreError> {
        self.query_json(missions::DELIVERY_TASKS, &[])
    }

    /// Missions, reports, approvals and human messages for the digest.
    pub fn mission_news(&self) -> Result<Value, StoreError> {
        self.query_json(missions::MISSION_NEWS, &[])
    }

    /// Approved residents that have run before: the ones the liveness rule
    /// keeps, as (agent, session, charter).
    pub fn residents_to_keep(&self) -> Result<Vec<(String, String, String)>, StoreError> {
        let rows = self.query(missions::RESIDENTS_TO_KEEP, &[])?;
        Ok(rows
            .iter()
            .map(|row| (row.get(0), row.get(1), row.get(2)))
            .collect())
    }

    /// Residents that are retired, for winding down their sessions.
    pub fn retired_residents(&self) -> Result<Vec<(String, String)>, StoreError> {
        let rows = self.query(missions::RETIRED_RESIDENTS, &[])?;
        Ok(rows.iter().map(|row| (row.get(0), row.get(1))).collect())
    }

    /// Where an agent's workspace comes from: `None` when no mission owns
    /// the agent, `Some(None)` for a mission without sources (a scratch
    /// workspace), `Some(Some(source))` otherwise. The agent's first task
    /// decides, so a resident's workspace never jumps between sources.
    pub fn agent_source(&self, agent: &str) -> Result<Option<Option<String>>, StoreError> {
        let rows = self.query(missions::AGENT_SOURCE, &[&agent])?;
        Ok(rows.first().map(|row| row.get::<_, Option<String>>(0)))
    }
}

/// Delivery bookkeeping, run by the dispatcher as the owner.
impl PostgresStore {
    /// Record that a task's delivery command was submitted.
    pub fn task_delivered(&self, task: i64, command: i64) -> Result<(), StoreError> {
        self.execute(missions::TASK_DELIVERED, &[&task, &command])?;
        Ok(())
    }

    /// Store a delivered task's report.
    pub fn task_reported(&self, task: i64, report: &str) -> Result<bool, StoreError> {
        let changed = self.execute(missions::TASK_REPORTED, &[&task, &report])?;
        Ok(changed == 1)
    }
}
