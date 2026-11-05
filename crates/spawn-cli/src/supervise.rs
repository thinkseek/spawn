//! The supervisor session never ends.
//!
//! The supervisor is one Claude Code session, `session_supervisor`, for the
//! life of the system (docs/fleet-architecture.md §3.1). This module is the
//! liveness rule that keeps it: a control-layer rule over facts read from
//! storage, with no model involved. Each loop step it decides one of:
//!
//! - nothing: a command is in flight, the engine is live, someone stopped it,
//!   or a failed revival is backing off;
//! - resume: the engine ended and a native session exists to resume;
//! - start with a handoff: nothing to resume, or resuming failed three times.
//!   The new native session gets the charter plus a summary rebuilt from the
//!   log, because the log, not the context window, is its durable memory.
//!
//! Approved residents are kept by the same rule ([`keep_residents`]).
//! Idempotency keys derive from the facts, so a restarted loop never submits
//! the same revival twice.

use spawn_core::command::{CommandAction, Submission};
use spawn_core::event::ENGINE;
use spawn_core::id::{AgentId, IdempotencyKey, Principal, SessionId};
use spawn_core::mission::TaskState;
use spawn_core::port::CommandQueue;
use spawn_postgres::{LivenessFacts, PostgresStore};

use crate::config::spawn_program;
use crate::deliver::{REPORT, Work, resident_role, task_prompt};
use crate::env_or;

/// Agent of the supervisor.
pub(crate) const SUPERVISOR_AGENT: &str = "agent_supervisor";

/// Session of the supervisor, for the life of the system.
pub(crate) const SUPERVISOR_SESSION: &str = "session_supervisor";

/// Principal (and database login) of the supervisor.
pub(crate) const SUPERVISOR_PRINCIPAL: &str = "spawn_supervisor";

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
    /// Resume the recorded native session.
    Resume,
    /// Start a new native session; `handoff` adds the summary from the log.
    Start { handoff: bool },
}

/// The liveness rule, as a pure function of the facts. A stop (or discard)
/// command ends the session: only a person, the supervisor (for a resident)
/// or delivery (for a retired resident) sends one. Any other end is an
/// outage to repair.
pub(crate) fn decide(facts: &LivenessFacts, now_ms: i64) -> Revive {
    let stopped = matches!(facts.last_kind.as_deref(), Some("stop" | "discard"));
    if facts.open_command || facts.live || stopped {
        return Revive::Nothing;
    }
    if facts.failures > 0 {
        let exponent = facts.failures.min(6);
        let pause = BACKOFF_MS.saturating_mul(1 << exponent);
        if facts
            .last_failure_ms
            .is_some_and(|failed| now_ms.saturating_sub(failed) < pause)
        {
            return Revive::Nothing;
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
   Add `--model cheap` to run a task entirely on a cheaper model (DeepSeek) when it is simple and \
fully specified: docs, mechanical edits, adding tests for behaviour the brief states. Keep the \
default for design, concurrency, security, debugging, and anything a gate judges as hard.
3. Review: each worker ends with a report; you get it in a digest. Accept (`{spawn} task accept \
--id N`) or send it back with what to fix (`{spawn} task reject --id N --note ...`). Read the \
worker's transcript (`{spawn} tail --session <session>`) only when the report is not enough.
Landing: a mission a human gave a gate lands its work on its source through Spawn's integrator, \
not through you. After you accept, the gate (black-box scenarios, clippy, tests, e2e) judges the \
work; if it rejects, the task comes back to its worker with the gate's output, and you review it \
again. A mission that accepts by gate alone (an objective mission, such as RSI) gets its tasks \
from the controller's backlog and needs no review from you. A halted mission (three rejections in \
a row, or its source moved outside the gate) waits for a human: tell them with HUMAN:.
4. Close the mission when its goal is met: `{spawn} mission close --id N --state done --summary \
...` (or `--state dropped`). Work you accepted in a gated mission still lands after `done`; \
`dropped` lets it go.

Tools (run them with Bash; they go through your restricted login):
- `{spawn} mission list` — missions with their tasks, states and reports; the resident roster.
- `{spawn} task assign --mission N --kind ephemeral|resident --worker <label|name> --brief <text> [--model cheap]`
- `{spawn} task accept|reject|cancel --id N`
- `{spawn} resident propose --name <name> --charter <text>`, `{spawn} resident retire --name <name>`
- `{spawn} fleet` — every agent's state (rules or Jev), attention, needs_human, stuck.
- `{spawn} tail --session <id>` — one agent's judgement, recent events, judgements and commands.
- `{spawn} submit --agent <agent> --session <session> --key <unique> --action '{{\"type\":\"stop\"}}'` \
— stop a worker if you must (input to workers goes through tasks). An agent whose `fleet` \
attention says `end_stalled` has ended but its final checkpoint keeps failing: its workspace \
stays mounted and holds work that is on no branch. Fix the cause if you can (a full disk, the \
chunk store); `{{\"type\":\"discard\"}}` unmounts it and loses that work, so send it only \
when the work may go.

Boundaries:
- Spawn's dispatcher delivers tasks one at a time per worker, nudges, resumes and sends you \
digests. You cannot command yourself, set a mission's gate, or resume a halted mission.
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
    let mine = store.commands_by(SUPERVISOR_PRINCIPAL, 15).map_or_else(
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

/// One liveness step for one session: decide, and submit the revival.
/// `resume` words the resume prompt from the facts; `start` builds the
/// prompt of a new native session, given whether it is a handoff. Returns a
/// line for the loop's log when it acted.
fn revive(
    store: &PostgresStore,
    agent: &str,
    session: &str,
    resume: impl FnOnce(&LivenessFacts) -> String,
    start: impl FnOnce(bool) -> String,
) -> Result<Option<String>, String> {
    let error = |error: &dyn std::fmt::Display| error.to_string();
    let session_id = SessionId::new(session).map_err(|e| error(&e))?;
    let facts = store
        .liveness_facts(&session_id, REVIVER)
        .map_err(|e| error(&e))?;
    let now = spawn_core::time::Clock::now(&spawn_core::time::SystemClock).0;
    let decision = decide(&facts, now);
    let execution = facts.execution_id.as_deref().unwrap_or("none");
    let (action, key) = match decision {
        Revive::Nothing => return Ok(None),
        Revive::Resume => (
            // No profile named: a revived session keeps the one it last
            // started with, so a resident keeps its models.
            CommandAction::Resume {
                prompt: Some(resume(&facts)),
                model: None,
            },
            format!("{session}-resume-{execution}-{}", facts.failures),
        ),
        Revive::Start { handoff } => (
            CommandAction::Start {
                engine: ENGINE.to_string(),
                prompt: start(handoff),
                parent_session_id: None,
                from: None,
                model: None,
            },
            format!(
                "{session}-start-{execution}-{}-{}",
                facts.failures,
                if handoff { "handoff" } else { "first" }
            ),
        ),
    };
    let submission = Submission {
        agent_id: AgentId::new(agent).map_err(|e| error(&e))?,
        session_id,
        idempotency_key: IdempotencyKey::new(key.clone()).map_err(|e| error(&e))?,
        action,
    };
    let principal = Principal::new(REVIVER).map_err(|e| error(&e))?;
    store
        .submit(&principal, &submission)
        .map_err(|e| error(&e))?;
    Ok(Some(format!("liveness: {session} {decision:?} ({key})")))
}

/// Keep the supervisor alive.
pub(crate) fn keep_alive(store: &PostgresStore) -> Result<Option<String>, String> {
    let spawn = spawn_program();
    let mission = env_or("SPAWN_MISSION_FILE", "mission.md");
    revive(
        store,
        SUPERVISOR_AGENT,
        SUPERVISOR_SESSION,
        |facts| {
            format!(
                "Spawn restarted your engine (the previous execution {} ended: {}). Run `{spawn} \
fleet` and continue supervising per your charter.",
                facts.execution_id.as_deref().unwrap_or("none"),
                facts.cause.as_deref().unwrap_or("unknown")
            )
        },
        |with_handoff| {
            let mut prompt = charter(&spawn, &mission);
            if with_handoff {
                prompt.push_str(&handoff(store));
            }
            prompt
        },
    )
}

/// Keep every approved resident alive: resumed after an outage, given its
/// charter (and its current task) when a new native session must start.
pub(crate) fn keep_residents(store: &PostgresStore) -> Result<Vec<String>, String> {
    let mut lines = Vec::new();
    for (agent, session, charter) in store
        .residents_to_keep()
        .map_err(|error| error.to_string())?
    {
        let start = |_handoff: bool| {
            let task = Work::load(store).ok().and_then(|work| {
                work.tasks
                    .into_iter()
                    .find(|task| task.session_id == session && task.state == TaskState::Delivered)
            });
            let prompt = task.map_or_else(
                || {
                    let name = session.trim_start_matches("session_resident_");
                    format!(
                        "{}\n\nWait for your next task.",
                        resident_role(name, &charter)
                    )
                },
                |task| task_prompt(&task),
            );
            format!(
                "{prompt}\n\nYour previous native session could not be resumed, so this is a new \
one; anything you learned before is in your workspace and its reports."
            )
        };
        let resume = |facts: &LivenessFacts| {
            format!(
                "Spawn restarted your engine (execution {} ended: {}). Continue your current \
task; when it is finished, write `{REPORT}`.",
                facts.execution_id.as_deref().unwrap_or("none"),
                facts.cause.as_deref().unwrap_or("unknown")
            )
        };
        lines.extend(revive(store, &agent, &session, resume, start)?);
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::{Revive, charter, decide};
    use spawn_postgres::LivenessFacts;

    #[test]
    fn the_charter_says_when_a_task_runs_cheap() {
        let text = charter("spawn", "mission.md");
        assert!(text.contains("--model cheap"));
        assert!(text.contains("simple and fully specified"));
        assert!(text.contains("security"));
    }

    fn facts() -> LivenessFacts {
        LivenessFacts {
            open_command: false,
            execution_id: Some("exec_3".to_string()),
            live: false,
            cause: Some("lost".to_string()),
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
    fn a_stopped_session_stays_down() {
        let stopped = LivenessFacts {
            last_kind: Some("stop".to_string()),
            cause: Some("stopped".to_string()),
            ..facts()
        };
        assert_eq!(decide(&stopped, 0), Revive::Nothing);
        let discarded = LivenessFacts {
            last_kind: Some("discard".to_string()),
            ..stopped
        };
        assert_eq!(decide(&discarded, 0), Revive::Nothing);
    }

    #[test]
    fn failed_resumes_back_off_then_hand_off() {
        let failed = LivenessFacts {
            failures: 1,
            last_failure_ms: Some(1_000),
            ..facts()
        };
        assert_eq!(decide(&failed, 1_500), Revive::Nothing);
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
