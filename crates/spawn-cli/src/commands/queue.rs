//! `submit`: writing a command to the queue.

use spawn_core::command::{CommandAction, Submission};
use spawn_core::id::{AgentId, IdempotencyKey, Principal, SessionId};
use spawn_core::port::CommandQueue;

use crate::Flags;
use crate::config::{env_value, open_store};

/// `spawn submit`: write one command to the queue.
pub(crate) fn submit(flags: &Flags) -> Result<i32, String> {
    let store = open_store()?;
    let principal = Principal::new(
        flags
            .get("principal")
            .unwrap_or_else(|| "spawn".to_string()),
    )
    .map_err(|error| error.to_string())?;
    let agent = AgentId::new(flags.required("agent")?).map_err(|error| error.to_string())?;
    let session = SessionId::new(flags.required("session")?).map_err(|error| error.to_string())?;
    let key = IdempotencyKey::new(flags.required("key")?).map_err(|error| error.to_string())?;
    let action: CommandAction = serde_json::from_str(&flags.required("action")?)
        .map_err(|error| format!("--action is not a command action: {error}"))?;
    let submission = Submission {
        agent_id: agent,
        session_id: session,
        idempotency_key: key,
        action,
    };
    // A restricted login (the supervisor's terminal) submits through the
    // gateway, which takes the principal from the login and checks grants.
    let id = if env_value("SPAWN_GATEWAY").is_some() {
        store.submit_via_gateway(&submission)
    } else {
        store.submit(&principal, &submission)
    }
    .map_err(|error| error.to_string())?;
    println!("command={}", id.0);
    Ok(0)
}
