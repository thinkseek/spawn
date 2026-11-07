//! Waking on change: the event watermark (the record) and
//! `LISTEN spawn_events` (the hint).
//!
//! `spawn loop` decides what to do from records, and a record is written
//! only by an append: `spawn_append_trusted` notifies `spawn_events` with
//! every event it writes. So two things say the same thing about the log,
//! and only one of them is authoritative:
//!
//! - the watermark, `max(spawn_events.id)` ([`PostgresStore::watermark`]):
//!   the index-only read of the log's primary key that names everything
//!   written up to it. It is the record: a step that has acted on the
//!   watermark has acted on every event at or below it, and a step whose
//!   watermark is behind has work to do;
//! - a notification on `spawn_events` ([`PostgresStore::listen`]): a hint
//!   that the watermark may have moved, which ends the loop's wait at once
//!   so a change is acted on sooner. A notification lost, missed or never
//!   sent proves nothing, and its absence is never read as "nothing
//!   happened": the next pass reads the watermark anyway
//!   (docs/design.md §14).
//!
//! Times the loop waits for are not in this module: they are computed by
//! each step from the records it read (a delivery's retry, a stall's bound,
//! a lease's `lease_until_ms`), never counted by the loop.

use std::time::Duration;

use postgres::{Client, NoTls};

use crate::PostgresStore;
use spawn_core::port::StoreError;

use crate::backend;

/// The log's watermark: the largest event id written, `0` on an empty log.
/// A read of the primary key alone (PostgreSQL answers `max` with the index's
/// last entry, never a table scan).
pub(crate) const WATERMARK: &str = "select coalesce(max(id), 0) from spawn_events";

/// When the earliest leased command in the queue may be claimed: the next
/// time the Bridge has work it cannot do now. `null` when nothing is
/// unfinished; `0` when an unfinished command holds no lease at all (a
/// command that waits behind another), which this loop cannot date: it is
/// due at the next pass.
pub(crate) const NEXT_LEASE: &str = concat!(
    "select min(coalesce(c.lease_until_ms, 0)) from spawn_commands c",
    " where c.state <> 'finished'"
);

/// The channel `spawn_append_trusted` notifies on every append.
pub const CHANNEL: &str = "spawn_events";

impl PostgresStore {
    /// The log's watermark ([`WATERMARK`]).
    pub fn watermark(&self) -> Result<i64, StoreError> {
        Ok(self.query_one_prepared(WATERMARK, &[])?.get(0))
    }

    /// The time the earliest unfinished command may be claimed
    /// ([`NEXT_LEASE`]): `None` when no command is unfinished.
    pub fn next_lease_ms(&self) -> Result<Option<i64>, StoreError> {
        Ok(self.query_one_prepared(NEXT_LEASE, &[])?.get(0))
    }

    /// Listen for this database's event notifications on a connection of
    /// its own ([`Listener`]).
    pub fn listen(&self) -> Result<Listener, StoreError> {
        let config: postgres::Config = self
            .url
            .parse()
            .map_err(|error: postgres::Error| backend(&error))?;
        let mut client = config.connect(NoTls).map_err(|error| backend(&error))?;
        client
            .batch_execute(&format!("listen {CHANNEL}"))
            .map_err(|error| backend(&error))?;
        Ok(Listener { client })
    }
}

/// A connection that holds `LISTEN spawn_events` for the loop, so a change
/// ends its wait at once. Notifications are only a hint to look again; the
/// watermark is what says whether anything changed ([`PostgresStore::watermark`]).
pub struct Listener {
    client: Client,
}

impl Listener {
    /// Wait up to `timeout` for a notification, draining every one that
    /// arrives (a burst is one wake-up): the count. A disconnect ends the
    /// wait too, and the caller's next pass reconnects.
    pub fn wait(&mut self, timeout: Duration) -> Result<usize, StoreError> {
        let mut received = 0;
        let mut notifications = self.client.notifications();
        let mut waiting = notifications.timeout_iter(timeout);
        loop {
            match fallible_iterator::FallibleIterator::next(&mut waiting) {
                Ok(Some(_)) => received += 1,
                Ok(None) => return Ok(received),
                Err(_) => return Ok(received),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crate::PostgresStore;

    /// A notification ends the wait at once, another connection's append is
    /// what sends it, and the watermark names that append afterwards. Needs
    /// `SPAWN_TEST_DATABASE_URL`, like the integration tests.
    #[test]
    fn a_notification_ends_the_wait_and_the_watermark_names_the_append() {
        use spawn_core::port::CommandQueue;
        let url = std::env::var("SPAWN_TEST_DATABASE_URL")
            .expect("SPAWN_TEST_DATABASE_URL must point at a disposable database");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let schema = format!("spawn_test_wake_{}_{nanos:x}", std::process::id());
        let store = PostgresStore::connect_in_schema(&url, &schema).expect("migrate");
        let before = store.watermark().expect("watermark");
        let mut listener = store.listen().expect("listen");
        let started = Instant::now();
        assert_eq!(
            listener.wait(Duration::from_millis(200)).expect("wait"),
            0,
            "an idle log sends nothing"
        );
        assert!(started.elapsed() >= Duration::from_millis(200));
        let started = Instant::now();
        let submission = spawn_core::command::Submission {
            agent_id: spawn_core::id::AgentId::new("agent_resident_wake").expect("agent"),
            session_id: spawn_core::id::SessionId::new("session_resident_wake").expect("session"),
            idempotency_key: spawn_core::id::IdempotencyKey::new("wake-test").expect("key"),
            action: spawn_core::command::CommandAction::Stop,
        };
        store
            .submit(
                &spawn_core::id::Principal::new("human").expect("principal"),
                &submission,
            )
            .expect("submit");
        let received = listener.wait(Duration::from_secs(5)).expect("wait");
        assert!(received >= 1, "the append notified the listener");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the wait ended with the notification, not its timeout"
        );
        assert!(
            store.watermark().expect("watermark") > before,
            "the append moved the watermark"
        );
        drop(store);
        let mut admin = postgres::Client::connect(&url, postgres::NoTls).expect("admin");
        admin
            .batch_execute(&format!("drop schema {schema} cascade"))
            .expect("drop the schema");
    }

    /// `next_lease_ms` dates the wait the Bridge's claim would otherwise
    /// have to poll for. Needs `SPAWN_TEST_DATABASE_URL`.
    #[test]
    fn the_next_lease_dates_the_queues_wait() {
        use spawn_core::port::CommandQueue;
        let url = std::env::var("SPAWN_TEST_DATABASE_URL")
            .expect("SPAWN_TEST_DATABASE_URL must point at a disposable database");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let schema = format!("spawn_test_lease_{}_{nanos:x}", std::process::id());
        let store = PostgresStore::connect_in_schema(&url, &schema).expect("migrate");
        assert_eq!(store.next_lease_ms().expect("lease"), None, "an empty queue");
        let submission = spawn_core::command::Submission {
            agent_id: spawn_core::id::AgentId::new("agent_resident_lease").expect("agent"),
            session_id: spawn_core::id::SessionId::new("session_resident_lease").expect("session"),
            idempotency_key: spawn_core::id::IdempotencyKey::new("lease-test").expect("key"),
            action: spawn_core::command::CommandAction::Stop,
        };
        store
            .submit(
                &spawn_core::id::Principal::new("human").expect("principal"),
                &submission,
            )
            .expect("submit");
        assert_eq!(
            store.next_lease_ms().expect("lease"),
            Some(0),
            "an unfinished command with no lease is due at once"
        );
        let claimed = store
            .claim("worker-lease", Duration::from_secs(60))
            .expect("claim");
        assert!(claimed.is_some(), "the Bridge claims the command");
        let due = store.next_lease_ms().expect("lease").expect("a lease");
        assert!(due > 0, "the claimed command's lease dates the next pass");
        drop(store);
        let mut admin = postgres::Client::connect(&url, postgres::NoTls).expect("admin");
        admin
            .batch_execute(&format!("drop schema {schema} cascade"))
            .expect("drop the schema");
    }
}
