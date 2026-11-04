//! Access as a restricted login: reads through the grant-filtered
//! `spawn_read_*` functions and submissions through the `agent_commands`
//! gateway, so the caller sees and sends exactly what `spawn_grants` allows.

use spawn_core::command::Submission;
use spawn_core::id::{CommandId, SessionId};
use spawn_core::port::StoreError;

use crate::PostgresStore;
use crate::queries::gateway;

impl PostgresStore {
    /// The fleet as one JSON array, read through the grant-filtered
    /// `spawn_read_*` functions, so it shows exactly what the caller's login
    /// may see.
    pub fn fleet_json(&self) -> Result<serde_json::Value, StoreError> {
        self.query_json(gateway::FLEET, &[])
    }

    /// One session's latest judgement (with the screen the judge saw) and
    /// its newest events, through the grant-filtered readers.
    pub fn tail_json(
        &self,
        session: &SessionId,
        events: i64,
    ) -> Result<serde_json::Value, StoreError> {
        self.query_json(gateway::TAIL, &[&session.as_str(), &events])
    }

    /// Submit through the `agent_commands` gateway. The principal is the
    /// database login, checked against `spawn_grants`; the caller cannot name
    /// another one.
    pub fn submit_via_gateway(&self, submission: &Submission) -> Result<CommandId, StoreError> {
        let action = serde_json::to_value(&submission.action)
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        // The gateway's trigger returns the command it made (a new one, or
        // the one an earlier call with this key made), and RETURNING reads it.
        let row = self.query_one(
            gateway::SUBMIT,
            &[
                &submission.agent_id.as_str(),
                &submission.session_id.as_str(),
                &submission.idempotency_key.as_str(),
                &action,
            ],
        )?;
        Ok(CommandId(row.get::<_, i64>(0).unsigned_abs()))
    }
}
