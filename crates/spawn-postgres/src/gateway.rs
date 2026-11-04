//! Access as a restricted login: reads through the grant-filtered
//! `spawn_read_*` functions and submissions through the `agent_commands`
//! gateway, so the caller sees and sends exactly what `spawn_grants` allows.

use spawn_core::command::Submission;
use spawn_core::id::{CommandId, SessionId};
use spawn_core::port::StoreError;

use crate::queries::gateway;
use crate::{PostgresStore, backend};

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
        // One lock across both statements: the id is read back through the
        // grant-filtered reader (the gateway has no RETURNING), and no other
        // statement on this connection may run in between.
        let mut client = self.lock();
        client
            .execute(
                gateway::SUBMIT,
                &[
                    &submission.agent_id.as_str(),
                    &submission.session_id.as_str(),
                    &submission.idempotency_key.as_str(),
                    &action,
                ],
            )
            .map_err(|error| backend(&error))?;
        let row = client
            .query_opt(
                gateway::NEWEST_COMMAND_ID,
                &[&submission.session_id.as_str()],
            )
            .map_err(|error| backend(&error))?;
        Ok(CommandId(row.map_or(0, |row| {
            row.get::<_, i64>("command_id").unsigned_abs()
        })))
    }
}
