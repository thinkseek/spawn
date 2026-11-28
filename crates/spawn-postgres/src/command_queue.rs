//! The leased command queue port over `spawn_commands`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use postgres::Client;
use spawn_core::command::{Claimed, Outcome, Submission};
use spawn_core::id::{CommandId, Principal};
use spawn_core::port::{BridgeShard, CommandOwnership, CommandQueue, StoreError};

use crate::PostgresStore;
use crate::row::{agent_id, bigint, command_id, corrupt, session_id};

/// `$1` principal, `$2` idempotency key, `$3` agent, `$4` session, `$5` action.
pub(crate) const SUBMIT: &str = "select out_id from spawn_submit($1, $2, $3, $4, $5)";

/// Lease the next command, if any. `$1` worker, `$2` lease in milliseconds.
pub(crate) const CLAIM: &str =
    "select out_id, out_attempt, out_agent, out_session, out_action, out_principal,
            out_created_at_ms
                   from spawn_claim($1, $2)";

/// Lease the next command owned by shard `$3 / $4`, if any.
pub(crate) const CLAIM_SHARD: &str =
    "select out_id, out_attempt, out_agent, out_session, out_action, out_principal,
            out_created_at_ms
                   from spawn_claim_shard($1, $2, $3, $4)";

fn claimed_from_row(row: &postgres::Row) -> Result<Claimed, StoreError> {
    Ok(Claimed {
        command_id: command_id(row, "out_id"),
        attempt: u32::try_from(row.get::<_, i32>("out_attempt")).unwrap_or(0),
        agent_id: agent_id(row, "out_agent")?,
        session_id: session_id(row, "out_session")?,
        principal: row.get("out_principal"),
        created_at_ms: row.get("out_created_at_ms"),
        action: row.get("out_action"),
    })
}

/// Two-key advisory-lock namespace for Bridges, a key of their own: the
/// integrator's lock (`INTEGRATOR_LOCK_KEY`, 1_399_957_336), the append lock
/// (1_399_957_335), the landing lock (1_399_957_337) and the others in
/// `schema.sql` never share it, so a Bridge cannot read as an integrator. The
/// second key includes both schema and worker, so test schemas and
/// differently named workers stay apart.
const WORKER_LOCK_NAMESPACE: i32 = 1_399_957_338;
const WORKER_LOCK: &str = "select pg_try_advisory_lock(
    $1, hashtext(current_schema() || ':bridge:' || $2)) as held";
const WORKER_UNLOCK: &str = "select pg_advisory_unlock(
    $1, hashtext(current_schema() || ':bridge:' || $2))";

/// Holds the same server session as the store, not a separate connection: if
/// this session dies, both the ownership lock and every queue operation die
/// together. SIGKILL therefore frees the lock without running Rust destructors.
struct PostgresOwnership {
    client: Arc<Mutex<Client>>,
    workers: Arc<Mutex<std::collections::HashSet<String>>>,
    worker: String,
}

impl CommandOwnership for PostgresOwnership {}

impl Drop for PostgresOwnership {
    fn drop(&mut self) {
        let _ = self
            .client
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .query(
                WORKER_UNLOCK,
                &[&WORKER_LOCK_NAMESPACE, &self.worker.as_str()],
            );
        self.workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.worker);
    }
}

impl CommandQueue for PostgresStore {
    fn recover_worker(
        &self,
        worker: &str,
    ) -> Result<(Box<dyn CommandOwnership>, usize), StoreError> {
        if !self
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(worker.to_string())
        {
            return Err(StoreError::Rejected(format!(
                "Bridge worker {worker} is still owned by a live process"
            )));
        }
        let acquired = self
            .query_one(WORKER_LOCK, &[&WORKER_LOCK_NAMESPACE, &worker])
            .map(|row| row.get::<_, bool>("held"));
        match acquired {
            Ok(true) => {
                // From here the guard rolls both the local and server-side
                // ownership back if the recovery update fails.
                let ownership = PostgresOwnership {
                    client: self.client.clone(),
                    workers: self.workers.clone(),
                    worker: worker.to_string(),
                };
                // Deliberately a direct owner-only update, not a definer
                // function a restricted queue login could use to steal leases.
                let recovered = self.execute(
                    "update spawn_commands
                        set lease_until_ms = spawn_now_ms()
                      where state not in ('finished', 'failed', 'cancelled')
                        and lease_worker = $1
                        and lease_until_ms > spawn_now_ms()",
                    &[&worker],
                )?;
                Ok((
                    Box::new(ownership),
                    usize::try_from(recovered).unwrap_or(usize::MAX),
                ))
            }
            Ok(false) => {
                self.workers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(worker);
                Err(StoreError::Rejected(format!(
                    "Bridge worker {worker} is still owned by a live process"
                )))
            }
            Err(error) => {
                self.workers
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(worker);
                Err(error)
            }
        }
    }

    fn submit(
        &self,
        principal: &Principal,
        submission: &Submission,
    ) -> Result<CommandId, StoreError> {
        let action = serde_json::to_value(&submission.action).map_err(corrupt)?;
        let row = self.query_one(
            SUBMIT,
            &[
                &principal.as_str(),
                &submission.idempotency_key.as_str(),
                &submission.agent_id.as_str(),
                &submission.session_id.as_str(),
                &action,
            ],
        )?;
        Ok(command_id(&row, "out_id"))
    }

    fn claim(&self, worker: &str, ttl: Duration) -> Result<Option<Claimed>, StoreError> {
        self.claim_for_shard(worker, ttl, BridgeShard::All)
    }

    fn claim_for_shard(
        &self,
        worker: &str,
        ttl: Duration,
        shard: BridgeShard,
    ) -> Result<Option<Claimed>, StoreError> {
        let ttl = bigint(ttl.as_millis());
        let rows = match shard {
            BridgeShard::All => self.query_prepared(CLAIM, &[&worker, &ttl])?,
            BridgeShard::Partition(partition) => {
                let shard_index = i32::from(partition.index());
                let shard_total = i32::from(partition.total());
                self.query_prepared(CLAIM_SHARD, &[&worker, &ttl, &shard_index, &shard_total])?
            }
        };
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        claimed_from_row(row).map(Some)
    }

    fn release(&self, claimed: &Claimed, retry_in: Duration) -> Result<(), StoreError> {
        let attempt = i32::try_from(claimed.attempt).unwrap_or(i32::MAX);
        let command = bigint(claimed.command_id.0);
        let retry = bigint(retry_in.as_millis());
        self.query(
            "select spawn_release($1, $2, $3)",
            &[&command, &attempt, &retry],
        )?;
        Ok(())
    }

    fn finish(&self, claimed: &Claimed, outcome: &Outcome) -> Result<(), StoreError> {
        let attempt = i32::try_from(claimed.attempt).unwrap_or(i32::MAX);
        let command = bigint(claimed.command_id.0);
        let row = match outcome {
            Outcome::Succeeded => self.query_one(
                "select spawn_finish($1, $2, true, $3)",
                &[&command, &attempt, &None::<String>],
            )?,
            Outcome::Failed(reason) => {
                let error = Some(spawn_core::redact::redact(reason));
                self.query_one(
                    "select spawn_finish($1, $2, false, $3)",
                    &[&command, &attempt, &error],
                )?
            }
            Outcome::Cancelled(reason) => {
                let reason = spawn_core::redact::redact(reason);
                self.query_one(
                    "select spawn_cancel($1, $2, $3)",
                    &[&command, &attempt, &reason],
                )?
            }
        };
        let status: String = row.get(0);
        match status.as_str() {
            "done" | "already" => Ok(()),
            "stale" => Err(StoreError::LeaseLost {
                command: claimed.command_id,
                attempt: claimed.attempt,
            }),
            other => Err(StoreError::Backend(format!("finish returned {other}"))),
        }
    }
}

impl PostgresStore {
    /// Cancels an open (unleased or leased) command without counting it as a
    /// `command_failed` failure.
    pub fn cancel_open_command(&self, command_id: i64, reason: &str) -> Result<bool, StoreError> {
        let reason = spawn_core::redact::redact(reason);
        let row = self.query_one("select spawn_cancel($1, null, $2)", &[&command_id, &reason])?;
        let status: String = row.get(0);
        Ok(status == "done")
    }

    /// Returns open non-human `"continue"` input command IDs for `session_id`
    /// (`#108/#109` stale nudge auto-cancel when task state moves on).
    pub fn open_continue_commands(&self, session_id: &str) -> Result<Vec<i64>, StoreError> {
        let rows = self.query(
            "select id
               from spawn_commands
              where session_id = $1
                and state not in ('finished', 'failed', 'cancelled')
                and principal <> 'human'
                and action ->> 'type' = 'input'
                and lower(btrim(coalesce(action ->> 'text', ''))) = 'continue'
              order by id",
            &[&session_id],
        )?;
        Ok(rows.into_iter().map(|row| row.get(0)).collect())
    }
}
