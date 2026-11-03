//! Controller state for the recursive self-improvement loop.
//!
//! The loop keeps its state in three tables, never in `spawn_events`:
//! `EventKind` is a closed enum and an unknown event name makes the next tick
//! fail with a corrupt row.
//!
//! The controller is a one-shot process. Every call here is a complete
//! transaction, so a killed process leaves either the old state or the new
//! one, never a half-written iteration.

use postgres::Row;

use crate::{PostgresStore, backend};
use spawn_core::command::{CommandAction, Submission};
use spawn_core::id::{AgentId, CommandId, IdempotencyKey, Principal, SessionId};
use spawn_core::port::StoreError;

/// Advisory lock key. One controller per database.
const CONTROLLER_LOCK: i64 = 1_399_957_336;

/// Phases that end an iteration.
const TERMINAL: [&str; 3] = ["Landed", "Rejected", "Aborted"];

/// Agent of the loop. One agent, one session, one workspace: the engine
/// keeps its context across tasks, so the loop accumulates memory instead of
/// starting cold every iteration.
pub const RSI_AGENT: &str = "agent_rsi";

/// Session of the loop.
pub const RSI_SESSION: &str = "session_rsi";

/// Baseline and counters of the loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RsiState {
    /// Commit the next iteration starts from.
    pub baseline: String,
    /// Last iteration number handed out.
    pub iteration: u32,
    /// Rejections in a row, toward the halt threshold.
    pub consecutive_rejected: u32,
    /// Scenario files the previous round saw. The next round must not fall
    /// below it. Zero means the floor is not set yet.
    pub scenario_count: u32,
    /// Aborts in a row, toward the infrastructure halt.
    pub infra_failures: u32,
}

/// Resource use of one iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RsiBudget {
    /// Milliseconds since the iteration started. `None` when the row was
    /// written before `started_at_ms` existed, in which case no limit applies.
    pub elapsed_ms: Option<i64>,
    /// `tool.started` events recorded since the iteration started.
    pub tool_calls: u32,
}

/// One iteration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RsiIteration {
    /// Iteration number.
    pub iteration: u32,
    /// Backlog task it works on.
    pub task_id: u32,
    /// Current phase.
    pub phase: String,
    /// Agent of this iteration.
    pub agent_id: String,
    /// Session of this iteration.
    pub session_id: String,
    /// Command that started the execution.
    pub command_id: Option<CommandId>,
    /// Patch captured from the workspace.
    pub patch: Option<String>,
    /// Commit produced from the workspace by the controller.
    pub candidate: Option<String>,
    /// Failure classification or free text.
    pub note: Option<String>,
}

/// Outcome of the command that started an iteration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RsiCommand {
    /// `pending`, `leased` or `finished`.
    pub state: String,
    /// Result, once finished.
    pub succeeded: Option<bool>,
    /// Failure text.
    pub error: Option<String>,
}

fn number(row: &Row, column: &str) -> u32 {
    row.get::<_, i32>(column).unsigned_abs()
}

fn state_of(row: &Row) -> RsiState {
    RsiState {
        baseline: row.get("baseline_commit"),
        iteration: number(row, "iteration"),
        consecutive_rejected: number(row, "consecutive_rejected"),
        scenario_count: number(row, "scenario_count"),
        infra_failures: number(row, "infra_failures"),
    }
}

fn iteration_of(row: &Row) -> RsiIteration {
    RsiIteration {
        iteration: number(row, "iteration"),
        task_id: number(row, "task_id"),
        phase: row.get("phase"),
        agent_id: row.get("agent_id"),
        session_id: row.get("session_id"),
        command_id: row
            .get::<_, Option<i64>>("command_id")
            .map(|id| CommandId(id.unsigned_abs())),
        patch: row.get("patch"),
        candidate: row.get("candidate"),
        note: row.get("note"),
    }
}

impl PostgresStore {
    /// Take the controller lock. False means another controller runs now.
    ///
    /// The lock belongs to the connection, and the connection dies with this
    /// process, so a killed controller never blocks the next one.
    pub fn rsi_try_lock(&self) -> Result<bool, StoreError> {
        let mut client = self.lock();
        let row = client
            .query_one(
                "select pg_try_advisory_lock($1) as held",
                &[&CONTROLLER_LOCK],
            )
            .map_err(|error| backend(&error))?;
        Ok(row.get("held"))
    }

    /// Current state, when the loop has been seeded.
    pub fn rsi_state(&self) -> Result<Option<RsiState>, StoreError> {
        let mut client = self.lock();
        let rows = client
            .query("select * from rsi_state where id = 1", &[])
            .map_err(|error| backend(&error))?;
        Ok(rows.first().map(state_of))
    }

    /// Wall clock and tool calls of one iteration.
    ///
    /// Both limits measure from `started_at_ms` on the database clock, so the
    /// controller and the store cannot disagree about when the iteration
    /// began. A row written before that column existed carries zero and
    /// reports no elapsed time, which disables both limits.
    pub fn rsi_iteration_budget(&self, iteration: u32) -> Result<Option<RsiBudget>, StoreError> {
        let mut client = self.lock();
        let rows = client
            .query(
                "select case when i.started_at_ms = 0 then null
                             else greatest(spawn_now_ms() - i.started_at_ms, 0)
                        end as elapsed_ms,
                        (select count(*) from spawn_events e
                          where e.session_id = i.session_id
                            and e.event_name = 'tool.started'
                            and e.recorded_at_ms >= i.started_at_ms) as tool_calls
                   from rsi_iterations i where i.iteration = $1",
                &[&i32::try_from(iteration).unwrap_or(i32::MAX)],
            )
            .map_err(|error| backend(&error))?;
        Ok(rows.first().map(|row| RsiBudget {
            elapsed_ms: row.get("elapsed_ms"),
            tool_calls: u32::try_from(row.get::<_, i64>("tool_calls")).unwrap_or(u32::MAX),
        }))
    }

    /// Record the scenario count this round saw.
    pub fn rsi_set_scenario_count(&self, count: u32) -> Result<(), StoreError> {
        let mut client = self.lock();
        client
            .execute(
                "update rsi_state set scenario_count = $1 where id = 1",
                &[&i32::try_from(count).unwrap_or(i32::MAX)],
            )
            .map_err(|error| backend(&error))?;
        Ok(())
    }

    /// Seed the baseline. Harmless when a row already exists.
    pub fn rsi_seed_baseline(&self, baseline: &str) -> Result<(), StoreError> {
        let mut client = self.lock();
        client
            .execute(
                "insert into rsi_state (id, baseline_commit) values (1, $1)
                 on conflict (id) do nothing",
                &[&baseline],
            )
            .map_err(|error| backend(&error))?;
        Ok(())
    }

    /// Iteration that has not reached a terminal phase.
    pub fn rsi_open_iteration(&self) -> Result<Option<RsiIteration>, StoreError> {
        let mut client = self.lock();
        let rows = client
            .query(
                "select * from rsi_iterations
                  where phase <> all($1)
                  order by iteration desc limit 1",
                &[&TERMINAL.map(str::to_string).to_vec()],
            )
            .map_err(|error| backend(&error))?;
        Ok(rows.first().map(iteration_of))
    }

    /// Tasks that already produced an accepted patch.
    pub fn rsi_solved_tasks(&self) -> Result<Vec<u32>, StoreError> {
        let mut client = self.lock();
        let rows = client
            .query(
                "select distinct task_id from rsi_attempts where accept_ok",
                &[],
            )
            .map_err(|error| backend(&error))?;
        Ok(rows.iter().map(|row| number(row, "task_id")).collect())
    }

    /// Attempts already made at one task.
    pub fn rsi_count_attempts(&self, task_id: u32) -> Result<u32, StoreError> {
        let mut client = self.lock();
        let row = client
            .query_one(
                "select count(*) as n from rsi_attempts where task_id = $1",
                &[&i32::try_from(task_id).unwrap_or(i32::MAX)],
            )
            .map_err(|error| backend(&error))?;
        Ok(u32::try_from(row.get::<_, i64>("n").unsigned_abs()).unwrap_or(u32::MAX))
    }

    /// State of the command that started an iteration.
    pub fn rsi_command_state(&self, command: CommandId) -> Result<Option<RsiCommand>, StoreError> {
        let mut client = self.lock();
        let row = client
            .query_opt(
                "select state, succeeded, error from spawn_commands where id = $1",
                &[&i64::try_from(command.0).unwrap_or(i64::MAX)],
            )
            .map_err(|error| backend(&error))?;
        Ok(row.map(|row| RsiCommand {
            state: row.get("state"),
            succeeded: row.get("succeeded"),
            error: row.get("error"),
        }))
    }

    /// Start one iteration and submit its first command in one transaction.
    ///
    /// A crash between the two writes would leave an iteration no phase can
    /// advance, so they share a transaction.
    pub fn rsi_begin_iteration(
        &self,
        task_id: u32,
        iteration: u32,
        action: CommandAction,
    ) -> Result<CommandId, StoreError> {
        let agent_id = RSI_AGENT.to_string();
        let session_id = RSI_SESSION.to_string();
        let rejected =
            |error: spawn_core::id::InvalidLabel| StoreError::Rejected(error.to_string());
        let submission = Submission {
            agent_id: AgentId::new(agent_id.clone()).map_err(rejected)?,
            session_id: SessionId::new(session_id.clone()).map_err(rejected)?,
            idempotency_key: IdempotencyKey::new(format!("rsi-{iteration}-start"))
                .map_err(rejected)?,
            action,
        };
        let principal = Principal::new("spawn").map_err(rejected)?;
        let action = serde_json::to_value(&submission.action)
            .map_err(|error| StoreError::Backend(error.to_string()))?;

        let mut client = self.lock();
        let mut transaction = client.transaction().map_err(|error| backend(&error))?;
        let row = transaction
            .query_one(
                "select out_id from spawn_submit($1, $2, $3, $4, $5)",
                &[
                    &principal.as_str(),
                    &submission.idempotency_key.as_str(),
                    &submission.agent_id.as_str(),
                    &submission.session_id.as_str(),
                    &action,
                ],
            )
            .map_err(|error| backend(&error))?;
        let command_id = CommandId(row.get::<_, i64>("out_id").unsigned_abs());
        transaction
            .execute(
                "insert into rsi_iterations
                     (iteration, task_id, phase, agent_id, session_id, command_id,
                      started_at_ms, updated_at_ms)
                 values ($1, $2, 'Starting', $3, $4, $5, spawn_now_ms(), spawn_now_ms())",
                &[
                    &i32::try_from(iteration).unwrap_or(i32::MAX),
                    &i32::try_from(task_id).unwrap_or(i32::MAX),
                    &agent_id,
                    &session_id,
                    &i64::try_from(command_id.0).unwrap_or(i64::MAX),
                ],
            )
            .map_err(|error| backend(&error))?;
        transaction
            .execute(
                "update rsi_state set iteration = greatest(iteration, $1) where id = 1",
                &[&i32::try_from(iteration).unwrap_or(i32::MAX)],
            )
            .map_err(|error| backend(&error))?;
        transaction.commit().map_err(|error| backend(&error))?;
        Ok(command_id)
    }

    /// Move one iteration to `phase`, recording the patch and candidate.
    pub fn rsi_set_phase(
        &self,
        iteration: u32,
        phase: &str,
        patch: Option<&str>,
        candidate: Option<&str>,
    ) -> Result<(), StoreError> {
        let mut client = self.lock();
        client
            .execute(
                "update rsi_iterations
                    set phase = $2,
                        patch = coalesce($3, patch),
                        candidate = coalesce($4, candidate),
                        updated_at_ms = spawn_now_ms()
                  where iteration = $1",
                &[
                    &i32::try_from(iteration).unwrap_or(i32::MAX),
                    &phase,
                    &patch,
                    &candidate,
                ],
            )
            .map_err(|error| backend(&error))?;
        Ok(())
    }

    /// Advance the baseline. The compare-and-swap rejects a stale writer.
    pub fn rsi_advance_baseline(
        &self,
        iteration: u32,
        expected: &str,
        candidate: &str,
    ) -> Result<bool, StoreError> {
        let mut client = self.lock();
        let mut transaction = client.transaction().map_err(|error| backend(&error))?;
        let changed = transaction
            .execute(
                "update rsi_state
                    set baseline_commit = $2, consecutive_rejected = 0,
                        infra_failures = 0
                  where id = 1 and baseline_commit = $1",
                &[&expected, &candidate],
            )
            .map_err(|error| backend(&error))?;
        if changed == 1 {
            transaction
                .execute(
                    "update rsi_iterations
                        set phase = 'Landed', candidate = $2, updated_at_ms = spawn_now_ms()
                      where iteration = $1",
                    &[&i32::try_from(iteration).unwrap_or(i32::MAX), &candidate],
                )
                .map_err(|error| backend(&error))?;
        }
        transaction.commit().map_err(|error| backend(&error))?;
        Ok(changed == 1)
    }

    /// Close one iteration as rejected or aborted, counting the failure.
    pub fn rsi_close_iteration(
        &self,
        iteration: u32,
        phase: &str,
        note: &str,
    ) -> Result<(), StoreError> {
        let mut client = self.lock();
        let mut transaction = client.transaction().map_err(|error| backend(&error))?;
        // A second close of the same iteration must not count a second
        // failure, so the row is only touched when the phase changes.
        let changed = transaction
            .execute(
                "update rsi_iterations set phase = $2, note = $3, updated_at_ms = spawn_now_ms()
                  where iteration = $1 and phase <> $2",
                &[&i32::try_from(iteration).unwrap_or(i32::MAX), &phase, &note],
            )
            .map_err(|error| backend(&error))?;
        if changed == 1 && phase == "Rejected" {
            transaction
                .execute(
                    "update rsi_state
                        set consecutive_rejected = consecutive_rejected + 1
                      where id = 1",
                    &[],
                )
                .map_err(|error| backend(&error))?;
        }
        // An abort means the candidate was never judged. Counting it as a
        // rejection would feed a good patch back as a failure.
        if changed == 1 && phase == "Aborted" {
            transaction
                .execute(
                    "update rsi_state
                        set infra_failures = infra_failures + 1
                      where id = 1",
                    &[],
                )
                .map_err(|error| backend(&error))?;
        }
        transaction.commit().map_err(|error| backend(&error))?;
        Ok(())
    }

    /// Record one attempt of a task.
    #[allow(clippy::too_many_arguments)]
    pub fn rsi_record_attempt(
        &self,
        task_id: u32,
        attempt: u32,
        iteration: u32,
        diff: Option<&str>,
        gate_log: Option<&str>,
        classifier: &str,
        accepted: bool,
    ) -> Result<(), StoreError> {
        let mut client = self.lock();
        client
            .execute(
                "insert into rsi_attempts
                     (task_id, attempt, iteration, diff, gate_log, classifier,
                      accept_ok, created_at_ms)
                 values ($1, $2, $3, $4, $5, $6, $7, spawn_now_ms())
                 on conflict (task_id, attempt) do nothing",
                &[
                    &i32::try_from(task_id).unwrap_or(i32::MAX),
                    &i32::try_from(attempt).unwrap_or(i32::MAX),
                    &i32::try_from(iteration).unwrap_or(i32::MAX),
                    &diff,
                    &gate_log,
                    &classifier,
                    &accepted,
                ],
            )
            .map_err(|error| backend(&error))?;
        Ok(())
    }

    /// Last attempt of a task, for the prompt that feeds failures back.
    pub fn rsi_last_attempt_of(
        &self,
        task_id: u32,
    ) -> Result<Option<(String, String)>, StoreError> {
        let mut client = self.lock();
        let rows = client
            .query(
                "select coalesce(diff, '') as diff, coalesce(gate_log, '') as gate_log
                   from rsi_attempts where task_id = $1
                  order by attempt desc limit 1",
                &[&i32::try_from(task_id).unwrap_or(i32::MAX)],
            )
            .map_err(|error| backend(&error))?;
        Ok(rows
            .first()
            .map(|row| (row.get("diff"), row.get("gate_log"))))
    }
}
