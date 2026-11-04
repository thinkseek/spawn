//! Configuration read from the environment, and the store it points at.
//!
//! Configuration is environment first, so a terminal started by the Bridge
//! carries everything its hook and runner need.

use spawn_core::id::{AgentId, ExecutionId, SessionId};
use spawn_postgres::PostgresStore;

use crate::flags::Flags;
use crate::trace;

pub(crate) fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

pub(crate) fn env_or(name: &str, fallback: &str) -> String {
    env_value(name).unwrap_or_else(|| fallback.to_string())
}

pub(crate) fn required_env(name: &str) -> Result<String, String> {
    env_value(name).ok_or_else(|| format!("{name} is not set"))
}

pub(crate) fn open_store() -> Result<PostgresStore, String> {
    let url = required_env("SPAWN_DATABASE_URL")?;
    trace::time("pg.connect", || PostgresStore::connect(&url)).map_err(|error| error.to_string())
}

/// Whether this host runs the supervisor (`SPAWN_SUPERVISOR` is set).
pub(crate) fn supervised() -> bool {
    env_value("SPAWN_SUPERVISOR").is_some()
}

/// The `spawn` program the dispatcher names in the commands it submits.
pub(crate) fn spawn_program() -> String {
    env_or("SPAWN_PROGRAM", "spawn")
}

/// `--root`, else `SPAWN_WORKSPACE_ROOT`.
pub(crate) fn workspace_root(flags: &Flags) -> Option<String> {
    flags
        .get("root")
        .or_else(|| env_value("SPAWN_WORKSPACE_ROOT"))
}

/// Pause between loop passes: `SPAWN_LOOP_INTERVAL_MS`, 2000 by default.
pub(crate) fn loop_interval() -> std::time::Duration {
    std::time::Duration::from_millis(
        env_value("SPAWN_LOOP_INTERVAL_MS")
            .and_then(|text| text.parse().ok())
            .unwrap_or(2000),
    )
}

/// The session a hook or runner inside a terminal reports for, read from the
/// environment the Bridge gave that terminal.
pub(crate) struct TerminalTarget {
    pub(crate) agent: AgentId,
    pub(crate) session: SessionId,
    pub(crate) execution: Option<ExecutionId>,
}

impl TerminalTarget {
    pub(crate) fn from_env() -> Result<Self, String> {
        Ok(Self {
            agent: AgentId::new(required_env("SPAWN_AGENT_ID")?)
                .map_err(|error| error.to_string())?,
            session: SessionId::new(required_env("SPAWN_SESSION_ID")?)
                .map_err(|error| error.to_string())?,
            execution: env_value("SPAWN_EXECUTION_ID")
                .map(|text| ExecutionId::new(text).map_err(|error| error.to_string()))
                .transpose()?,
        })
    }
}
