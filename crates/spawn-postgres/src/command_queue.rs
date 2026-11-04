//! The leased command queue port over `spawn_commands`.

use std::time::Duration;

use spawn_core::command::{Claimed, Outcome, Submission};
use spawn_core::id::{CommandId, Principal};
use spawn_core::port::{CommandQueue, StoreError};

use crate::PostgresStore;
use crate::row::{agent_id, bigint, command_id, corrupt, session_id};

/// `$1` principal, `$2` idempotency key, `$3` agent, `$4` session, `$5` action.
pub(crate) const SUBMIT: &str = "select out_id from spawn_submit($1, $2, $3, $4, $5)";

/// Lease the next command, if any. `$1` worker, `$2` lease in milliseconds.
pub(crate) const CLAIM: &str = "select out_id, out_attempt, out_agent, out_session, out_action
                   from spawn_claim($1, $2)";

impl CommandQueue for PostgresStore {
    fn submit(
        &self,
        principal: &Principal,
        submission: &Submission,
    ) -> Result<CommandId, StoreError> {
        let action = serde_json::to_value(&submission.action).map_err(corrupt)?;
        let row = self.query_one(
            SUBMIT,
            &[
                &principal.as_str(),
                &submission.idempotency_key.as_str(),
                &submission.agent_id.as_str(),
                &submission.session_id.as_str(),
                &action,
            ],
        )?;
        Ok(command_id(&row, "out_id"))
    }

    fn claim(&self, worker: &str, ttl: Duration) -> Result<Option<Claimed>, StoreError> {
        let ttl = bigint(ttl.as_millis());
        let rows = self.query(CLAIM, &[&worker, &ttl])?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        Ok(Some(Claimed {
            command_id: command_id(row, "out_id"),
            attempt: u32::try_from(row.get::<_, i32>("out_attempt")).unwrap_or(0),
            agent_id: agent_id(row, "out_agent")?,
            session_id: session_id(row, "out_session")?,
            action: row.get("out_action"),
        }))
    }

    fn finish(&self, claimed: &Claimed, outcome: &Outcome) -> Result<(), StoreError> {
        let (succeeded, error) = match outcome {
            Outcome::Succeeded => (true, None),
            Outcome::Failed(reason) => (false, Some(reason.clone())),
        };
        let attempt = i32::try_from(claimed.attempt).unwrap_or(i32::MAX);
        let command = bigint(claimed.command_id.0);
        let row = self.query_one(
            "select spawn_finish($1, $2, $3, $4)",
            &[&command, &attempt, &succeeded, &error],
        )?;
        let status: String = row.get(0);
        match status.as_str() {
            "done" | "already" => Ok(()),
            "stale" => Err(StoreError::LeaseLost {
                command: claimed.command_id,
                attempt: claimed.attempt,
            }),
            other => Err(StoreError::Backend(format!("finish returned {other}"))),
        }
    }
}
