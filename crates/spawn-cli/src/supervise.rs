//! The supervisor session never ends.
//!
//! The supervisor is one Claude Code session, `session_supervisor`, for the
//! life of the system (docs/fleet-architecture.md §3.1). This module is the
//! liveness rule that keeps it: a control-layer rule over facts read from
//! storage, with no model involved. Each loop step it decides one of:
//!
//! - nothing: a command is in flight, the engine is live, or a human stopped it;
//! - resume: the engine ended and a native session exists to resume;
//! - start with a handoff: nothing to resume, or resuming failed three times.
//!   The new native session gets the charter plus a summary rebuilt from the
//!   log, because the log, not the context window, is its durable memory.
//!
//! Idempotency keys derive from the facts, so a restarted loop never submits
//! the same revival twice.

use spawn_core::command::{CommandAction, Submission};
use spawn_core::event::Engine;
use spawn_core::id::{AgentId, IdempotencyKey, Principal, SessionId};
use spawn_core::port::CommandQueue;
use spawn_postgres::{LivenessFacts, PostgresStore};

use crate::{env_or, open_store};

/// Agent of the supervisor.
pub(crate) const SUPERVISOR_AGENT: &str = "agent_supervisor";

/// Session of the supervisor, for the life of the system.
pub(crate) const SUPERVISOR_SESSION: &str = "session_supervisor";

/// Principal the liveness rule submits as.
const REVIVER: &str = "spawn-liveness";

/// Failed resumes before the rule starts a new native session instead.
const RESUME_ATTEMPTS: u32 = 3;

/// Base pause after a failed revival; doubles per failure, capped.
const BACKOFF_MS: i64 = 10_000;

/// What the rule does this step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Revive {
    /// Leave the session alone.
    Nothing,
    /// A revival failed recently; wait out the backoff.
    Wait,
    /// Resume the recorded native session.
    Resume,
    /// Start a new native session; `handoff` adds the summary from the log.
    Start { handoff: bool },
}

/// The liveness rule, as a pure function of the facts.
pub(crate) fn decide(facts: &LivenessFacts, now_ms: i64) -> Revive {
    if facts.open_command || facts.live {
        return Revive::Nothing;
    }
    // Only a person ends the supervisor; every other end is an outage.
    if facts.last_kind.as_deref() == Some("stop")
        && facts.last_principal.as_deref() == Some("human")
    {
        return Revive::Nothing;
    }
    if facts.failures > 0 {
        let exponent = facts.failures.min(6);
        let pause = BACKOFF_MS.saturating_mul(1 << exponent);
        if facts
            .last_failure_ms
            .is_some_and(|failed| now_ms.saturating_sub(failed) < pause)
        {
            return Revive::Wait;
        }
    }
    if facts.execution_id.is_none() && facts.failures == 0 {
        return Revive::Start { handoff: false };
    }
    if facts.native_bound && facts.failures < RESUME_ATTEMPTS {
        return Revive::Resume;
    }
    Revive::Start { handoff: true }
}

/// The supervisor's standing instructions, given at every new native session.
fn charter(spawn: &str, mission: &str) -> String {
    format!(
        "You are the Spawn supervisor: one perpetual agent that runs a fleet of coding agents so that \
humans have to step in as rarely as possible.

Your session never ends. If your engine restarts you will be resumed or handed this charter again; \
your durable memory is the event log, not this conversation. Never stop yourself.

Mission: read {mission}. If it is missing or empty, keep the fleet healthy and wait for a human \
message.

Tools (run them with Bash; they read and write the shared PostgreSQL log through a restricted login):
- `{spawn} fleet` — every agent's state as JSON: state, who judged it (rules or jev), attention, \
needs_human, stuck, task, live.
- `{spawn} tail --session <session_id>` — one agent's latest judgement, the screen the judge saw, \
and its newest events.
- `{spawn} submit --agent agent_worker_<name> --session session_worker_<name> --key <unique-key> \
--action '<json>'` — command a worker. Actions: \
{{\"type\":\"start\",\"engine\":\"claude\",\"prompt\":\"...\"}}, {{\"type\":\"input\",\"text\":\"...\"}}, \
{{\"type\":\"resume\",\"prompt\":\"...\"}}, {{\"type\":\"stop\"}}. Keys must be unique per command.

Boundaries:
- You may command only agents named agent_worker_*. You can read but not command agent_rsi (the RSI \
loop has its own controller) or yourself.
- Spawn's dispatcher handles routine cases and sends you a digest when a worker needs a decision. \
Decide from the facts (`tail`), not from a guess.
- When something truly needs a person (credentials, product decisions, irreversible actions), \
end your turn with one clear question that starts with \"HUMAN:\". Then wait.
- Prefer small, reversible steps. Do not run git commands in your workspace; workers run in \
their own workspaces.
"
    )
}

/// Summary of the log for a new native session.
fn handoff(store: &PostgresStore) -> String {
    let fleet = store.fleet_json().map_or_else(
        |error| format!("(fleet unavailable: {error})"),
        |value| serde_json::to_string_pretty(&value).unwrap_or_default(),
    );
    let mine = store.commands_by("spawn_supervisor", 15).map_or_else(
        |error| format!("(history unavailable: {error})"),
        |value| serde_json::to_string_pretty(&value).unwrap_or_default(),
    );
    format!(
        "\n\nHANDOFF. Your previous native session could not be resumed, so this is a new one. \
Rebuilt from the event log:\n\nFleet now:\n{fleet}\n\nYour last commands, newest first:\n{mine}\n\n\
Start by running `fleet`, then continue the mission.\n"
    )
}

/// Advance the liveness rule by one step. Returns a line for the loop's log
/// when it acted.
pub(crate) fn keep_alive() -> Result<Option<String>, String> {
    let store = open_store()?;
    let session = SessionId::new(SUPERVISOR_SESSION).map_err(|error| error.to_string())?;
    let facts = store
        .liveness_facts(&session, REVIVER)
        .map_err(|error| error.to_string())?;
    let now = spawn_core::time::Clock::now(&spawn_core::time::SystemClock).0;
    let decision = decide(&facts, now);
    let spawn = env_or("SPAWN_PROGRAM", "spawn");
    let mission = env_or("SPAWN_MISSION_FILE", "mission.md");
    let execution = facts
        .execution_id
        .clone()
        .unwrap_or_else(|| "none".to_string());
    let (action, key) = match decision {
        Revive::Nothing | Revive::Wait => return Ok(None),
        Revive::Resume => (
            CommandAction::Resume {
                prompt: Some(format!(
                    "Spawn restarted your engine (the previous execution {execution} ended: {}). \
Run `{spawn} fleet` and continue supervising per your charter.",
                    facts.cause.as_deref().unwrap_or("unknown")
                )),
            },
            format!("supervisor-resume-{execution}-{}", facts.failures),
        ),
        Revive::Start {
            handoff: with_handoff,
        } => {
            let mut prompt = charter(&spawn, &mission);
            if with_handoff {
                prompt.push_str(&handoff(&store));
            }
            (
                CommandAction::Start {
                    engine: Engine::Claude,
                    prompt,
                    parent_session_id: None,
                },
                format!(
                    "supervisor-start-{execution}-{}-{}",
                    facts.failures,
                    if with_handoff { "handoff" } else { "first" }
                ),
            )
        }
    };
    let submission = Submission {
        agent_id: AgentId::new(SUPERVISOR_AGENT).map_err(|error| error.to_string())?,
        session_id: session,
        idempotency_key: IdempotencyKey::new(key.clone()).map_err(|error| error.to_string())?,
        action,
    };
    let principal = Principal::new(REVIVER).map_err(|error| error.to_string())?;
    store
        .submit(&principal, &submission)
        .map_err(|error| error.to_string())?;
    Ok(Some(format!("supervise: {decision:?} ({key})")))
}

#[cfg(test)]
mod tests {
    use super::{Revive, decide};
    use spawn_postgres::LivenessFacts;

    fn facts() -> LivenessFacts {
        LivenessFacts {
            open_command: false,
            execution_id: Some("exec_3".to_string()),
            live: false,
            cause: Some("lost".to_string()),
            last_principal: Some("spawn-liveness".to_string()),
            last_kind: Some("resume".to_string()),
            native_bound: true,
            failures: 0,
            last_failure_ms: None,
        }
    }

    #[test]
    fn a_live_or_busy_supervisor_is_left_alone() {
        let live = LivenessFacts {
            live: true,
            ..facts()
        };
        assert_eq!(decide(&live, 0), Revive::Nothing);
        let busy = LivenessFacts {
            open_command: true,
            ..facts()
        };
        assert_eq!(decide(&busy, 0), Revive::Nothing);
    }

    #[test]
    fn the_first_start_has_no_handoff() {
        let fresh = LivenessFacts {
            execution_id: None,
            native_bound: false,
            ..facts()
        };
        assert_eq!(decide(&fresh, 0), Revive::Start { handoff: false });
    }

    #[test]
    fn a_lost_engine_is_resumed() {
        assert_eq!(decide(&facts(), 0), Revive::Resume);
    }

    #[test]
    fn only_a_human_stop_is_respected() {
        let human = LivenessFacts {
            last_kind: Some("stop".to_string()),
            last_principal: Some("human".to_string()),
            cause: Some("stopped".to_string()),
            ..facts()
        };
        assert_eq!(decide(&human, 0), Revive::Nothing);
        let automatic = LivenessFacts {
            last_principal: Some("spawn".to_string()),
            ..human
        };
        assert_eq!(decide(&automatic, 0), Revive::Resume);
    }

    #[test]
    fn failed_resumes_back_off_then_hand_off() {
        let failed = LivenessFacts {
            failures: 1,
            last_failure_ms: Some(1_000),
            ..facts()
        };
        assert_eq!(decide(&failed, 1_500), Revive::Wait);
        assert_eq!(decide(&failed, 1_000 + 20_000), Revive::Resume);
        let exhausted = LivenessFacts {
            failures: 3,
            last_failure_ms: Some(0),
            ..facts()
        };
        assert_eq!(
            decide(&exhausted, 10_000_000),
            Revive::Start { handoff: true }
        );
    }

    #[test]
    fn nothing_to_resume_means_a_handoff() {
        let unbound = LivenessFacts {
            native_bound: false,
            ..facts()
        };
        assert_eq!(decide(&unbound, 0), Revive::Start { handoff: true });
    }
}
