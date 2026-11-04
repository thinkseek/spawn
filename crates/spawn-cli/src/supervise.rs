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

/// The liveness rule for the supervisor, which only a person may stop.
pub(crate) fn decide(facts: &LivenessFacts, now_ms: i64) -> Revive {
    decide_for(facts, now_ms, &["human"])
}

/// The liveness rule, as a pure function of the facts. A stop by one of
/// `respected` ends the session; any other end is an outage to repair.
pub(crate) fn decide_for(facts: &LivenessFacts, now_ms: i64, respected: &[&str]) -> Revive {
    if facts.open_command || facts.live {
        return Revive::Nothing;
    }
    if facts.last_kind.as_deref() == Some("stop")
        && facts
            .last_principal
            .as_deref()
            .is_some_and(|principal| respected.contains(&principal))
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
        "You are the Spawn supervisor: one perpetual agent that turns missions into work done by \
a fleet of coding agents, so that humans have to step in as rarely as possible.

Your session never ends. If your engine restarts you will be resumed or handed this charter again; \
your durable memory is the event log and the mission tables, not this conversation. Never stop \
yourself.

You work for missions, not for repositories. Missions arrive in the inbox (humans add them with \
`spawn mission add`, or tell you in this pane: then add it yourself with `{spawn} mission add \
--text ...`). A standing mission may also be in {mission}.

How work flows: you decide, Spawn delivers. You never type into workers yourself.
1. Plan: split a mission into tasks. Each task gets a self-contained brief: goal, inputs (paths, \
repos, facts it needs), done criteria. Never paste your own conversation into a brief.
2. Route each task, keeping every context about one concern:
   - to a resident (`--kind resident --worker <name>`) only if the task is inside that resident's \
charter;
   - otherwise to a new ephemeral worker (`--kind ephemeral --worker <short label>`), which exists \
for that task only.
   If a concern keeps coming back and no resident owns it, propose one: `{spawn} resident propose \
--name <name> --charter <one paragraph>`. A human must approve it before you can use it.
3. Review: each worker ends with a report; you get it in a digest. Accept (`{spawn} task accept \
--id N`) or send it back with what to fix (`{spawn} task reject --id N --note ...`). Read the \
worker's transcript (`{spawn} tail --session <session>`) only when the report is not enough.
4. Close the mission when its goal is met: `{spawn} mission close --id N --state done --summary \
...` (or `--state dropped`).

Tools (run them with Bash; they go through your restricted login):
- `{spawn} mission list` — missions with their tasks, states and reports; the resident roster.
- `{spawn} task assign --mission N --kind ephemeral|resident --worker <label|name> --brief <text>`
- `{spawn} task accept|reject|cancel --id N`
- `{spawn} resident propose --name <name> --charter <text>`, `{spawn} resident retire --name <name>`
- `{spawn} fleet` — every agent's state (rules or Jev), attention, needs_human, stuck.
- `{spawn} tail --session <id>` — one agent's judgement, recent events, judgements and commands.
- `{spawn} submit --agent <agent> --session <session> --key <unique> --action '{{\"type\":\"stop\"}}'` \
— stop a worker if you must (input to workers goes through tasks).

Boundaries:
- Spawn's dispatcher delivers tasks one at a time per worker, nudges, resumes and sends you \
digests. You cannot command agent_rsi (it has its own controller) or yourself.
- Humans may write to any worker directly; you get a one-line note in your digest. Adjust your \
plan to it.
- When something truly needs a person (credentials, product decisions, irreversible actions, \
approving a resident), end your turn with one clear question that starts with \"HUMAN:\". Then \
wait.
- No fixed fleet size: start what the mission needs, no more; idle residents cost little, idle \
ephemeral workers should not exist.
- Do not run git commands in your workspace; workers run in their own.
"
    )
}

/// The charter as this build writes it, for re-briefing a running supervisor.
pub(crate) fn current_charter() -> String {
    charter(
        &spawn_program(),
        &env_or("SPAWN_MISSION_FILE", "mission.md"),
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
    let missions = store.mission_board(10).map_or_else(
        |error| format!("(missions unavailable: {error})"),
        |value| serde_json::to_string_pretty(&value).unwrap_or_default(),
    );
    format!(
        "\n\nHANDOFF. Your previous native session could not be resumed, so this is a new one. \
Rebuilt from storage:\n\nMissions and tasks:\n{missions}\n\nFleet now:\n{fleet}\n\n\
Your last commands, newest first:\n{mine}\n\nStart with `mission list` and `fleet`, then continue.\n"
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
    let spawn = spawn_program();
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

/// Keep one approved resident alive: resumed after an outage, a stop by a
/// human or the supervisor respected. `prompt` is its charter with its
/// current task, given when a new native session must start.
pub(crate) fn keep_resident(
    store: &PostgresStore,
    agent: &str,
    session: &str,
    prompt: &str,
) -> Result<Option<String>, String> {
    let session_id = SessionId::new(session).map_err(|error| error.to_string())?;
    let facts = store
        .liveness_facts(&session_id, REVIVER)
        .map_err(|error| error.to_string())?;
    let now = spawn_core::time::Clock::now(&spawn_core::time::SystemClock).0;
    let decision = decide_for(&facts, now, &["human", "spawn_supervisor"]);
    let execution = facts
        .execution_id
        .clone()
        .unwrap_or_else(|| "none".to_string());
    let (action, key) = match decision {
        Revive::Nothing | Revive::Wait => return Ok(None),
        Revive::Resume => (
            CommandAction::Resume {
                prompt: Some(format!(
                    "Spawn restarted your engine (execution {execution} ended: {}). Continue \
your current task; when it is finished, write `.spawn/report.md`.",
                    facts.cause.as_deref().unwrap_or("unknown")
                )),
            },
            format!("resident-resume-{execution}-{}", facts.failures),
        ),
        Revive::Start { .. } => (
            CommandAction::Start {
                engine: Engine::Claude,
                prompt: format!(
                    "{prompt}\n\nYour previous native session could not be resumed, so this is \
a new one; anything you learned before is in your workspace and its reports."
                ),
                parent_session_id: None,
            },
            format!("resident-start-{execution}-{}", facts.failures),
        ),
    };
    let submission = Submission {
        agent_id: AgentId::new(agent).map_err(|error| error.to_string())?,
        session_id,
        idempotency_key: IdempotencyKey::new(key.clone()).map_err(|error| error.to_string())?,
        action,
    };
    let principal = Principal::new(REVIVER).map_err(|error| error.to_string())?;
    store
        .submit(&principal, &submission)
        .map_err(|error| error.to_string())?;
    Ok(Some(format!("liveness: {session} {decision:?} ({key})")))
}

/// One liveness step for every resident worth keeping.
pub(crate) fn keep_residents(store: &PostgresStore) -> Result<Vec<String>, String> {
    let work = crate::deliver::Work::load(store)?;
    let mut lines = Vec::new();
    for (agent, session, charter) in store
        .residents_to_keep()
        .map_err(|error| error.to_string())?
    {
        let prompt = work
            .tasks
            .iter()
            .find(|task| task.session_id == session && task.state == "delivered")
            .map_or_else(
                || format!("You are a long-lived resident worker of Spawn. Your charter:\n{charter}\n\nWait for your next task."),
                crate::deliver::task_prompt,
            );
        lines.extend(keep_resident(store, &agent, &session, &prompt)?);
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::{Revive, decide, decide_for};
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
    fn a_resident_stopped_by_the_supervisor_stays_down() {
        let stopped = LivenessFacts {
            last_kind: Some("stop".to_string()),
            last_principal: Some("spawn_supervisor".to_string()),
            cause: Some("stopped".to_string()),
            ..facts()
        };
        assert_eq!(
            decide_for(&stopped, 0, &["human", "spawn_supervisor"]),
            Revive::Nothing
        );
        assert_eq!(
            decide(&stopped, 0),
            Revive::Resume,
            "the supervisor itself is kept"
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
