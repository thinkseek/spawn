//! Waking on change: the committed event watermark (authoritative boundary)
//! and `LISTEN spawn_events` (wake-up hint).
//!
//! `spawn loop` decides what to do from records, and a record is written
//! only by an append: `spawn_append_trusted` notifies `spawn_events` with
//! every event it writes. So two things say something about the log,
//! and only one of them is authoritative:
//!
//! - the committed watermark ([`CommittedWatermark`], [`PostgresStore::committed_watermark`],
//!   [`PostgresStore::watermark`]): bounded below any in-flight transaction
//!   (`commit_xid < wm.safe_xid` and strictly before any later-id out-of-order
//!   commit via `commit_watermark_cte!()`). A step that has acted on this
//!   watermark has acted on every committed event at or below it, and an
//!   in-flight writer with a smaller `id` holds the watermark back until it
//!   commits so slower transactions are never silently skipped;
//! - a notification on `spawn_events` ([`PostgresStore::listen`]): an advisory
//!   wake-up **hint** only, never a commit or visibility boundary. A lost,
//!   missed, reordered, or uncommitted notification proves nothing, and its
//!   absence is never read as "nothing happened": the next pass reads the
//!   authoritative committed watermark anyway (docs/design.md §14).
//!
//! Times the loop waits for are not in this module: they are computed by
//! each step from the records it read (a delivery's retry, a stall's bound,
//! a lease's `lease_until_ms`), never counted by the loop.

use std::time::Duration;

use postgres::{Client, NoTls};

use crate::PostgresStore;
use spawn_core::port::StoreError;

use crate::backend;

/// Authoritative committed log frontier: `(commit_xid, event_id)` of the
/// newest committed event strictly below `wm.safe_xid` (`commit_watermark_cte!()`),
/// ordered lexicographically by `commit_xid` first and `event_id` second.
///
/// Both fields are `0` on an empty or wholly uncommitted log. Safe to compare
/// monotonically across commits and to resume replay from `(commit_xid, id) >
/// cursor` without skipping out-of-order lower `event_id` commits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommittedWatermark {
    /// Transaction ID (`xid8`) of the frontier committed event (`0` when empty).
    pub commit_xid: u64,
    /// Event ID of the frontier committed event (`0` when empty).
    pub event_id: i64,
}

impl CommittedWatermark {
    /// Construct a [`CommittedWatermark`] from a `(commit_xid, event_id)` pair.
    #[must_use]
    pub const fn new(commit_xid: u64, event_id: i64) -> Self {
        Self {
            commit_xid,
            event_id,
        }
    }

    /// Return the scalar `i64` event-id of the frontier event.
    #[must_use]
    pub const fn as_i64(self) -> i64 {
        self.event_id
    }
}

pub(crate) fn parse_commit_xid(raw: &str) -> Result<u64, StoreError> {
    raw.parse::<u64>()
        .map_err(|error| StoreError::Corrupt(format!("invalid commit_xid {raw:?}: {error}")))
}

/// The log's authoritative committed frontier in `(commit_xid, id)` order
/// strictly below the schema-scoped `xid8` commit watermark
/// (`commit_watermark_cte!()`), `('0', 0)` on an empty or wholly uncommitted log.
pub(crate) const WATERMARK: &str = concat!(
    "with ",
    commit_watermark_cte!(),
    "
      select coalesce(top.commit_xid, '0'::xid8)::text as xid,
             coalesce(top.id, 0) as id,
             wm.wm_raced
        from wm
        left join lateral (
            select commit_xid, id
              from spawn_events
             where commit_xid < wm.safe_xid
             order by commit_xid desc, id desc
             limit 1
        ) top on true"
);

/// When the earliest leased command in the queue may be claimed: the next
/// time the Bridge has work it cannot do now. `null` when nothing is
/// unfinished; `0` when an unfinished command holds no lease at all (a
/// command that waits behind another), which this loop cannot date: it is
/// due at the next pass.
pub(crate) const NEXT_LEASE: &str = concat!(
    "select min(coalesce(c.lease_until_ms, 0)) from spawn_commands c",
    " where c.state not in ('finished', 'failed', 'cancelled')"
);

/// The channel `spawn_append_trusted` notifies on every append.
const CHANNEL: &str = "spawn_events";

impl PostgresStore {
    /// Authoritative committed event frontier (`WATERMARK`), typed as
    /// [`CommittedWatermark`].
    pub fn committed_watermark(&self) -> Result<CommittedWatermark, StoreError> {
        let row = self.query_one_watermark_prepared(WATERMARK, &[])?;
        let xid: String = row.get("xid");
        let commit_xid = parse_commit_xid(&xid)?;
        Ok(CommittedWatermark::new(commit_xid, row.get("id")))
    }

    /// Scalar committed event watermark (`WATERMARK`), equivalent to
    /// `self.committed_watermark()?.as_i64()`.
    pub fn watermark(&self) -> Result<i64, StoreError> {
        self.committed_watermark().map(CommittedWatermark::as_i64)
    }

    /// The time the earliest unfinished command may be claimed
    /// (`NEXT_LEASE`): `None` when no command is unfinished.
    pub fn next_lease_ms(&self) -> Result<Option<i64>, StoreError> {
        Ok(self.query_one_prepared(NEXT_LEASE, &[])?.get(0))
    }

    /// Listen for this database's event notifications on a connection of
    /// its own ([`Listener`]).
    pub fn listen(&self) -> Result<Listener, StoreError> {
        self.listen_channel(CHANNEL)
    }

    /// Listen on a caller-chosen PostgreSQL channel on a connection of its own.
    ///
    /// Tests sharing `SPAWN_TEST_DATABASE_URL` via [`crate::testing::TestSchema`]
    /// share the database-wide `spawn_events` channel, so a test asserting that
    /// no PostgreSQL notification wakes its wait isolates itself by listening on
    /// a schema-unique channel instead.
    #[cfg(any(test, feature = "testing"))]
    pub fn listen_on(&self, channel: &str) -> Result<Listener, StoreError> {
        let valid = !channel.is_empty()
            && channel.len() <= 63
            && channel
                .bytes()
                .next()
                .is_some_and(|first| first.is_ascii_lowercase() || first == b'_')
            && channel
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
        if !valid {
            return Err(StoreError::Rejected(format!(
                "channel name {channel:?} is not a lowercase bare identifier of at most 63 bytes"
            )));
        }
        self.listen_channel(channel)
    }

    fn listen_channel(&self, channel: &str) -> Result<Listener, StoreError> {
        // `listen()` opens 1 private `LISTEN` connection outside any `RoleConnPool`:
        // `spawn serve`'s single shared hub in `ServePools` therefore holds 1 pooled
        // events connection + 1 private connection, staying within the default
        // `SPAWN_SERVE_EVENTS_POOL=2` budget solely because there is a single hub.
        let config = crate::passfile::config(self.url.expose_secret(), self.passfile.as_deref())
            .map_err(|error| crate::passfile_store_error(&error))?;
        let mut client = config.connect(NoTls).map_err(|error| backend(&error))?;
        client
            .batch_execute(&format!("listen {channel}"))
            .map_err(|error| backend(&error))?;
        Ok(Listener { client })
    }
}

/// A connection that holds `LISTEN spawn_events` for the loop, so a change
/// ends its wait at once. Notifications are **only an advisory hint** to look
/// again — never a commit or visibility boundary; always read
/// [`PostgresStore::committed_watermark`] / [`PostgresStore::watermark`] to determine
/// which committed events exist.
pub struct Listener {
    client: Client,
}

impl Listener {
    /// Wait up to `timeout` for a notification: the number drained. A
    /// notification already buffered ends the wait at once, and the ones
    /// behind it are counted too (a burst is one wake-up; the caller acts on
    /// it by reading the watermark once, so a notification left unread only
    /// wakes the next pass a little sooner). A disconnect ends the wait, and
    /// the caller reconnects on its next pass.
    pub fn wait(&mut self, timeout: Duration) -> Result<usize, StoreError> {
        use fallible_iterator::FallibleIterator;
        let mut received = self.drain();
        if received > 0 {
            return Ok(received);
        }
        let blocked = {
            let mut notifications = self.client.notifications();
            let mut waiting = notifications.timeout_iter(timeout);
            matches!(waiting.next(), Ok(Some(_)))
        };
        if blocked {
            received += 1;
            received += self.drain();
        }
        Ok(received)
    }

    /// Every notification already buffered on the connection.
    fn drain(&mut self) -> usize {
        use fallible_iterator::FallibleIterator;
        let mut received = 0;
        let mut notifications = self.client.notifications();
        let mut buffered = notifications.iter();
        while matches!(buffered.next(), Ok(Some(_))) {
            received += 1;
        }
        received
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]
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
        let schema = crate::testing::TestSchema::new("wake");
        let store = PostgresStore::connect_in_schema_with_options(
            &url,
            &schema,
            &crate::testing::test_connect_options(),
        )
        .expect("migrate");
        let mut listener = store.listen().expect("listen");
        // The channel is the database's, not the schema's: another case's
        // append may be waiting in it. Drain those first.
        let _ = listener.wait(Duration::from_millis(500));
        let before = store.watermark().expect("watermark");
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
        // The clock starts at the wait, not at the submit: `spawn_submit`
        // takes the append lock, which a loaded test database can hold for
        // seconds, and what this case is about is the wait, not the write.
        // A notification sent before the wait may still be buffered: it is
        // read back at once rather than waited for.
        let started = Instant::now();
        let received = listener.wait(Duration::from_secs(10)).expect("wait");
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
    }

    /// `next_lease_ms` dates the wait the Bridge's claim would otherwise
    /// have to poll for. Needs `SPAWN_TEST_DATABASE_URL`.
    #[test]
    fn the_next_lease_dates_the_queues_wait() {
        use spawn_core::port::CommandQueue;
        let url = std::env::var("SPAWN_TEST_DATABASE_URL")
            .expect("SPAWN_TEST_DATABASE_URL must point at a disposable database");
        let schema = crate::testing::TestSchema::new("lease");
        let store = PostgresStore::connect_in_schema_with_options(
            &url,
            &schema,
            &crate::testing::test_connect_options(),
        )
        .expect("migrate");
        assert_eq!(
            store.next_lease_ms().expect("lease"),
            None,
            "an empty queue"
        );
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
            .claim("worker-lease", Duration::from_mins(1))
            .expect("claim");
        assert!(claimed.is_some(), "the Bridge claims the command");
        let due = store.next_lease_ms().expect("lease").expect("a lease");
        assert!(due > 0, "the claimed command's lease dates the next pass");
        drop(store);
    }

    /// `listen_on` validates channel identifiers and isolates a test listener
    /// from the database-wide `spawn_events` channel while still delivering real
    /// PostgreSQL notifications on its own channel. Needs
    /// `SPAWN_TEST_DATABASE_URL`.
    #[test]
    fn listen_on_rejects_invalid_channels_and_isolates_from_spawn_events() {
        use postgres::NoTls;
        use spawn_core::port::{CommandQueue, StoreError};

        let url = std::env::var(crate::testing::TEST_DATABASE_URL)
            .expect("SPAWN_TEST_DATABASE_URL must point at a disposable database");
        let schema = crate::testing::TestSchema::new("wake_chan");
        let store = PostgresStore::connect_in_schema_with_options(
            &url,
            &schema,
            &crate::testing::test_connect_options(),
        )
        .expect("migrate");

        let too_long = "a".repeat(64);
        for bad in ["", "1bad", "a; select 1", "Upper", too_long.as_str()] {
            assert!(
                matches!(store.listen_on(bad), Err(StoreError::Rejected(_))),
                "invalid channel {bad:?} must be rejected"
            );
        }

        let mut listener = store.listen_on(&schema).expect("listen_on");
        let submission = spawn_core::command::Submission {
            agent_id: spawn_core::id::AgentId::new("agent_resident_wake_chan").expect("agent"),
            session_id: spawn_core::id::SessionId::new("session_resident_wake_chan")
                .expect("session"),
            idempotency_key: spawn_core::id::IdempotencyKey::new("wake-chan-test").expect("key"),
            action: spawn_core::command::CommandAction::Stop,
        };
        store
            .submit(
                &spawn_core::id::Principal::new("human").expect("principal"),
                &submission,
            )
            .expect("submit");
        assert_eq!(
            listener.wait(Duration::from_millis(50)).expect("wait"),
            0,
            "database-wide spawn_events must not wake an isolated test channel"
        );

        let mut notify = crate::passfile::config(&url, None)
            .expect("config")
            .connect(NoTls)
            .expect("notify client");
        notify
            .batch_execute(&format!("notify {}", schema.name()))
            .expect("notify");
        assert!(
            listener.wait(Duration::from_secs(5)).expect("wait") >= 1,
            "isolated channel must still deliver real PostgreSQL notifications"
        );
        drop(listener);
        drop(notify);
        drop(store);
    }

    /// Out-of-order commits: an in-flight writer holding a lower event id (`id_a`)
    /// holds `committed_watermark()` / `watermark()` below `id_a` even after another
    /// session commits a higher `id_b > id_a`, advancing cleanly to `id_b` once the
    /// earlier writer commits. Needs `SPAWN_TEST_DATABASE_URL`.
    #[test]
    fn watermark_holds_back_behind_uncommitted_transaction_and_advances_on_commit() {
        use postgres::{Client, NoTls};

        let url = std::env::var(crate::testing::TEST_DATABASE_URL)
            .expect("SPAWN_TEST_DATABASE_URL must point at a disposable database");
        let schema = crate::testing::TestSchema::new("wake_xid8_wm");
        let store = PostgresStore::connect_in_schema_with_options(
            &url,
            &schema,
            &crate::testing::test_connect_options(),
        )
        .expect("migrate");
        let baseline = store.committed_watermark().expect("baseline");

        let connect_in_schema = || -> Client {
            let mut client =
                crate::passfile::config(&url, crate::testing::test_passfile().as_deref())
                    .expect("config")
                    .connect(NoTls)
                    .expect("connect");
            client
                .batch_execute(&format!("set search_path to {}", schema.name()))
                .expect("search_path");
            client
        };

        let kind = serde_json::json!({"name": "session.noted", "detail": {"text": "wm test"}});
        let mut conn_a = connect_in_schema();
        let mut conn_b = connect_in_schema();

        let mut tx_a = conn_a.transaction().expect("begin tx_a");
        let id_a: i64 = tx_a
            .query_one(
                "select out_id from spawn_append_trusted('agent_a', 'session_wm_a', null, 'bridge', null, $1)",
                &[&kind],
            )
            .expect("append a")
            .get(0);

        let id_b: i64 = conn_b
            .query_one(
                "select out_id from spawn_append_trusted('agent_b', 'session_wm_b', null, 'bridge', null, $1)",
                &[&kind],
            )
            .expect("append b")
            .get(0);
        assert!(id_b > id_a, "expected id_b ({id_b}) > id_a ({id_a})");

        let while_open = store
            .committed_watermark()
            .expect("watermark while tx_a open");
        assert!(
            while_open <= baseline && while_open.event_id < id_a,
            "committed watermark {while_open:?} jumped past uncommitted id_a={id_a} (baseline={baseline:?})"
        );
        assert_eq!(
            store.watermark().expect("scalar watermark"),
            while_open.as_i64()
        );

        tx_a.commit().expect("commit tx_a");
        let after_commit = store.committed_watermark().expect("watermark after commit");
        assert_eq!(
            after_commit.event_id, id_b,
            "watermark advances past both committed writes once tx_a commits: {after_commit:?}"
        );
        assert!(after_commit > baseline && after_commit.commit_xid > 0);
    }

    /// Reversed `commit_xid` vs `id` order with a third older-XID blocker:
    /// B allocates `x_b` first, C allocates `x_c` second (staying open), A
    /// allocates `x_a` third and appends `id_a < id_b`, then B appends `id_b`
    /// and commits first. The frontier advances to `(x_b, id_b)`, does NOT
    /// move backward `id_b -> 0 -> id_b` when A commits behind open C, advances
    fn tx_row_xid(tx: &mut postgres::Transaction<'_>, event_id: i64) -> u64 {
        tx.query_one(
            "select commit_xid::text from spawn_events where id = $1",
            &[&event_id],
        )
        .expect("commit_xid")
        .get::<_, String>(0)
        .parse()
        .expect("parse commit_xid")
    }

    /// monotonically to `(x_a, id_a)` once C commits, and `read_global` from
    /// `id_b` recovers `id_a` without skipping or duplicating.
    #[test]
    fn watermark_advances_monotonically_in_xid_order_with_reversed_ids_and_blocker() {
        use postgres::{Client, NoTls};
        use spawn_core::event::GlobalPosition;
        use spawn_core::port::EventLog;

        let url = std::env::var(crate::testing::TEST_DATABASE_URL)
            .expect("SPAWN_TEST_DATABASE_URL must point at a disposable database");
        let schema = crate::testing::TestSchema::new("wake_rev_xid");
        let store = PostgresStore::connect_in_schema_with_options(
            &url,
            &schema,
            &crate::testing::test_connect_options(),
        )
        .expect("migrate");
        let baseline = store.committed_watermark().expect("baseline");

        let connect_in_schema = || -> Client {
            let mut client =
                crate::passfile::config(&url, crate::testing::test_passfile().as_deref())
                    .expect("config")
                    .connect(NoTls)
                    .expect("connect");
            let quoted = crate::ValidatedSchemaName::parse(schema.name())
                .expect("schema")
                .quote_ident();
            client
                .batch_execute(&format!("set search_path to {quoted}"))
                .expect("search_path");
            client
        };

        let kind = serde_json::to_value(spawn_core::event::EventKind::ArchiveFailed {
            native: "rev_xid".to_string(),
            reason: "test".to_string(),
        })
        .expect("event kind json");
        let (mut conn_b, mut conn_c, mut conn_a) = (
            connect_in_schema(),
            connect_in_schema(),
            connect_in_schema(),
        );
        let mut tx_b = conn_b.transaction().expect("begin tx_b");
        let id_b0: i64 = tx_b
            .query_one("select out_id from spawn_append_trusted('agent_b0', 'session_rev_b0', null, 'bridge', null, $1)", &[&kind])
            .expect("append b0")
            .get(0);
        let x_b = tx_row_xid(&mut tx_b, id_b0);
        let mut tx_c = conn_c.transaction().expect("begin tx_c");
        let id_c: i64 = tx_c
            .query_one("select out_id from spawn_append_trusted('agent_c', 'session_rev_c', null, 'bridge', null, $1)", &[&kind])
            .expect("append c")
            .get(0);
        let x_c = tx_row_xid(&mut tx_c, id_c);
        let mut tx_a = conn_a.transaction().expect("begin tx_a");
        let id_a: i64 = tx_a
            .query_one("select out_id from spawn_append_trusted('agent_a', 'session_rev_a', null, 'bridge', null, $1)", &[&kind])
            .expect("append a")
            .get(0);
        let x_a = tx_row_xid(&mut tx_a, id_a);
        assert!(x_b < x_c && x_c < x_a, "x_b={x_b}, x_c={x_c}, x_a={x_a}");

        // B appends id_b > id_a under earlier x_b and commits first.
        let id_b: i64 = tx_b
            .query_one("select out_id from spawn_append_trusted('agent_b', 'session_rev_b', null, 'bridge', null, $1)", &[&kind])
            .expect("append b")
            .get(0);
        assert!(id_a < id_b, "expected id_a ({id_a}) < id_b ({id_b})");

        tx_b.commit().expect("commit tx_b");
        let wm_b = store.committed_watermark().expect("wm after tx_b commit");
        assert_eq!(wm_b, super::CommittedWatermark::new(x_b, id_b));
        assert!(wm_b > baseline);

        // Now A (with x_a > x_c and id_a < id_b) commits while C stays open.
        tx_a.commit().expect("commit tx_a");
        assert_eq!(
            store
                .committed_watermark()
                .expect("wm while blocker C open"),
            wm_b,
            "frontier must not move backward behind committed B when lower-id A commits above open blocker C"
        );

        // Release blocker C -> frontier advances monotonically in XID order to (x_a, id_a).
        tx_c.commit().expect("commit tx_c");
        let wm_after_c = store
            .committed_watermark()
            .expect("wm after blocker C commit");
        assert_eq!(wm_after_c, super::CommittedWatermark::new(x_a, id_a));
        assert!(wm_after_c > wm_b);

        let replayed = store
            .read_global(GlobalPosition(u64::try_from(id_b).expect("i64")), 100)
            .expect("read_global from id_b");
        let replayed_ids: Vec<i64> = replayed
            .iter()
            .map(|r| i64::try_from(r.position.0).expect("i64"))
            .collect();
        assert_eq!(replayed_ids, vec![id_c, id_a]);
    }
}
