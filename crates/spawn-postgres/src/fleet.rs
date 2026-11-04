//! Read model of the fleet for the dashboard.
//!
//! Every figure is computed by SQL from the log, the projection and the
//! command queue (see [`crate::queries::fleet`]), so the page shows storage
//! and nothing a process remembers.

use serde_json::Value;
use spawn_core::port::StoreError;

use crate::PostgresStore;
use crate::queries::fleet;

impl PostgresStore {
    /// Model judgements since `since_ms` with their outcomes, as JSON rows.
    pub fn corpus_rows(&self, since_ms: i64) -> Result<Value, StoreError> {
        self.query_json(fleet::JUDGEMENT_CORPUS, &[&since_ms])
    }

    /// Facts for the dispatcher, as JSON rows.
    pub fn dispatch_facts(&self) -> Result<Value, StoreError> {
        self.query_json(fleet::DISPATCH_FACTS, &[])
    }

    /// Every session with its judgement, task and activity, supervisor first.
    pub fn fleet_board(&self) -> Result<Value, StoreError> {
        self.query_json(fleet::FLEET_BOARD, &[])
    }

    /// Fleet health and the decision ladder over the last hour and day.
    pub fn fleet_health(&self) -> Result<Value, StoreError> {
        self.query_json(fleet::FLEET_HEALTH, &[])
    }

    /// One session's judgements, newest first, with the evidence each saw.
    pub fn judgements_json(&self, session: &str, limit: u32) -> Result<Value, StoreError> {
        self.query_json(fleet::SESSION_JUDGEMENTS, &[&session, &i64::from(limit)])
    }

    /// Commands into one session, newest first, with who sent them.
    pub fn commands_json(&self, session: &str, limit: u32) -> Result<Value, StoreError> {
        self.query_json(fleet::SESSION_COMMANDS, &[&session, &i64::from(limit)])
    }
}
