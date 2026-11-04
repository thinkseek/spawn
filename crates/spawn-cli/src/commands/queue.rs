//! `submit` and `dispatch`: writing commands to the queue.

use spawn_core::command::{CommandAction, Submission};
use spawn_core::id::{AgentId, IdempotencyKey, Principal, SessionId};
use spawn_core::port::CommandQueue;

use crate::Flags;
use crate::config::{env_value, open_store, spawn_program, supervised, workspace_root};
use crate::dispatch;

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

/// `spawn dispatch [--dry-run] [--root <dir>]`: one dispatcher step;
/// `--dry-run` prints the plan and submits nothing; `--root` (default
/// `SPAWN_WORKSPACE_ROOT`) is the workspace root, as for `spawn loop`.
pub(crate) fn dispatch_once(rest: &[String]) -> Result<i32, String> {
    let dry = rest.iter().any(|word| word == "--dry-run");
    let valued: Vec<String> = rest
        .iter()
        .filter(|word| *word != "--dry-run")
        .cloned()
        .collect();
    let flags =
        Flags::parse(&valued).map_err(|error| format!("{error}\n{}", crate::DISPATCH_USAGE))?;
    let root = workspace_root(&flags).map(std::path::PathBuf::from);
    let store = open_store()?;
    let supervised = supervised();
    let spawn = spawn_program();
    if dry {
        let plan = dispatch::plan(&store, supervised, &spawn)?;
        for order in &plan.orders {
            println!(
                "order {} -> {}: {}",
                order.key,
                order.session_id,
                serde_json::to_string(&order.action).unwrap_or_default()
            );
        }
        for item in &plan.pending {
            println!("pending {} {:?}: {}", item.session_id, item.why, item.line);
        }
        if plan.orders.is_empty() && plan.pending.is_empty() {
            println!("dispatch: nothing to do");
        }
        return Ok(0);
    }
    for line in dispatch::step(&store, supervised, &spawn, root.as_deref())? {
        println!("{line}");
    }
    Ok(0)
}
