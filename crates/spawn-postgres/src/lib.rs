//! PostgreSQL adapters for the event log and the leased command queue.
//!
//! Times travel as epoch milliseconds, so this crate needs no date library.
//! Every write goes through the SQL functions in `sql/schema.sql`; the
//! ordering guarantees, role gates, and schema-local advisory locks live there
//! and are shared by every writer.
//!
//! # Security & Role Model
//!
//! Connections either authenticate as the schema **owner** (for migrations,
//! dispatcher/integrator mutations, alerts, backups, and D6/provenance ledger
//! writes) or as one of four database-scoped least-privilege logins derived by
//! [`role_url_with_passfile`]:
//! - `spawn_supervisor`: decision/mission actions and supervised table reads.
//! - `spawn_reader`: read-only queries (`spawn_read_*` functions and relation `SELECT`).
//! - `spawn_human`: `SECURITY DEFINER` human-gateway entrypoints (`spawn_human_*`).
//! - `spawn_node_gateway`: verify-then-commit node gateway RPCs (`spawn_node_gw_*`).
//!
//! Read helpers run single SQL statements (or prepared index-backed reads) and
//! map PostgreSQL errors into [`StoreError`]: privilege / exclusion checks map
//! to [`StoreError::Rejected`], transient disconnects or shutdowns map to
//! [`StoreError::Unreachable`], corrupt domain values map to
//! [`StoreError::Corrupt`], and unexpected SQL errors map to
//! [`StoreError::Backend`].
//!
//! Layout: each module holds its SQL next to the code that binds its
//! parameters and maps its rows (`row` holds the shared column decoders).
//!
//! - `event_log`: the [`EventLog`] port.
//! - `command_queue`: the [`CommandQueue`] port.
//! - `alerts`: the loop's alert transitions and the alerts they fold into.
//! - `sessions`: per-session read side over the `agent_*` views.
//! - `gateway`: grant-filtered reads and submissions for restricted logins.
//! - `fleet`: the dashboard and dispatcher read model.
//! - `missions`: missions, tasks and residents, and the integrator's lock.
//! - `scorecard`: the facts `spawn scorecard` measures, read in one snapshot.
//! - `verifications`: objectives' verifications after their landings.
//! - `provenance`: universal file provenance, append-only, and its blast radius.
//! - `remote_jobs`: spawn-node v1's ledger, the nodes' leases of remote jobs.
//!
//! [`EventLog`]: spawn_core::port::EventLog
//! [`CommandQueue`]: spawn_core::port::CommandQueue

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::sync::{Arc, Mutex, MutexGuard};

use postgres::types::ToSql;
use postgres::{Client, NoTls, Row};
use spawn_core::port::StoreError;

/// SQL: whether the `agent_executions` row aliased `$x` died young: it
/// ended lost or with a non-zero exit within a minute of its start, before
/// it made any progress (no tool call completed and no turn ended after it
/// started). An engine that launches and dies at once is a failed revival,
/// whatever its command's outcome says; one that worked before a fault took
/// it is not, and is revived at once (objective 032's soak).
macro_rules! died_young {
    ($x:literal) => {
        concat!(
            "(",
            $x,
            ".ended_at_ms is not null and ",
            $x,
            ".duration_ms < 60000 and (",
            $x,
            ".cause = 'lost' or coalesce(",
            $x,
            ".exit_code, 0) <> 0) and not exists (select 1 from spawn_events p \
             where p.session_id = ",
            $x,
            ".session_id and p.execution_id = ",
            $x,
            ".execution_id and p.event_name in ('tool.completed', 'turn.stopped')))"
        )
    };
}

/// SQL: whether the `agent_executions` row aliased `$x` ended lost before
/// its engine became ready: the log has no Bridge `screen.observed` of it
/// in state `ready` (the ready wait of a start or resume records one keyed
/// `<execution>:screen:ready` before it types; the Bridge's follow records
/// others). A start whose ready wait failed is abandoned and ends so,
/// whatever its duration.
macro_rules! lost_before_ready {
    ($x:literal) => {
        concat!(
            "(coalesce(",
            $x,
            ".cause, '') = 'lost' and not exists (select 1 from spawn_events r where \
             r.session_id = ",
            $x,
            ".session_id and r.source = 'bridge' and r.source_event_id = ",
            $x,
            ".execution_id || ':screen:ready') and not exists (select 1 from spawn_events r \
             where r.session_id = ",
            $x,
            ".session_id and r.event_name = 'screen.observed' and r.source = 'bridge' and \
             r.execution_id = ",
            $x,
            ".execution_id and r.kind -> 'detail' ->> 'state' = 'ready'))"
        )
    };
}

/// Maximum number of retries if an in-flight transaction in `pg_snapshot_xip`
/// commits mid-statement before its `pg_locks` entry is inspected. On the final
/// attempt the watermark fails closed to `pg_snapshot_xmin(snap.s)`.
pub(crate) const WATERMARK_RACE_ATTEMPTS: usize = 5;

/// Fail-closed schema-scoped `xid8` commit watermark CTE (`snap`, `xip`, `wm`):
/// - Fast path (`xmin = xmax`, empty `xip`): returns `pg_snapshot_xmax(s)` with
///   `wm_raced = false` and zero `pg_locks` scans.
/// - Active path (`xmin < xmax`): maps each active top-level `x` in
///   `pg_snapshot_xip(s)` to its `pg_locks` `virtualtransaction` (using 32-bit
///   epoch wraparound-safe match on `transactionid`).
///   - If any `xip` transaction already exited (`vtx is null`), sets
///     `wm_raced = true` and fails closed to `pg_snapshot_xmin(s)` so callers
///     retry or stay safe on the final attempt.
///   - Otherwise holds back every `x` unless ALL three conditions hold:
///     1. `vtx` holds zero non-`AccessShareLock` relation locks in `snap.ns_oid`;
///     2. `vtx` holds zero `L0` (`1399957400`) advisory locks on `snap.ns_oid`;
///     3. `vtx` holds >= 1 granted positive outside proof: cross-DB non-shared
///        relation lock, same-DB committed user-schema relation lock (excluding
///        `pg_catalog`, `information_schema`, `pg_temp_*`, `pg_toast_temp_*`),
///        or same-DB other-schema exclusive `L0` migration lock.
///
/// Uncommitted relations (`c.oid is null`) and `pg_catalog` locks never count
/// as outside proof (fail-closed for `CREATE TEMP TABLE`, `CREATE TABLE` in
/// this schema, bare `pg_current_xact_id()`, and lockless transactions).
///
/// Single-schema writer invariant: Spawn writers never mix writes to another
/// user schema's committed table with `spawn_append` in the same transaction
/// (every SQL entrypoint pins `search_path = %I, pg_temp`, and production runs
/// a single schema).
macro_rules! commit_watermark_cte {
    () => {
        "snap as materialized (
                    select pg_current_snapshot() as s,
                           (select oid from pg_database where datname = current_database()) as db_oid,
                           (select relnamespace from pg_class where oid = 'spawn_events'::regclass) as ns_oid
                 ), locks as materialized (
                    select l.locktype, l.mode, l.granted, l.database, l.relation,
                           l.classid, l.objid, l.objsubid, l.transactionid, l.virtualtransaction
                      from snap, pg_locks l
                     where pg_snapshot_xmin(snap.s) < pg_snapshot_xmax(snap.s)
                       and (
                           (l.locktype = 'transactionid' and l.mode = 'ExclusiveLock' and l.granted)
                           or (l.locktype = 'relation' and l.mode <> 'AccessShareLock')
                           or (l.locktype = 'advisory'
                               and l.database = snap.db_oid
                               and l.classid = 1399957400::oid
                               and l.objsubid = 2)
                       )
                 ), xip as materialized (
                    select x,
                           (select tl.virtualtransaction
                              from locks tl
                             where tl.locktype = 'transactionid'
                               and tl.transactionid = ((x::text::numeric % 4294967296)::bigint::text)::xid
                             limit 1) as vtx
                      from snap,
                           lateral pg_snapshot_xip(snap.s) as x
                     where pg_snapshot_xmin(snap.s) < pg_snapshot_xmax(snap.s)
                 ), wm as materialized (
                    select
                        case
                            when pg_snapshot_xmin(snap.s) = pg_snapshot_xmax(snap.s) then pg_snapshot_xmax(snap.s)
                            else coalesce(
                                (select min(xip.x)
                                   from xip, snap
                                  where xip.vtx is null
                                     or exists (
                                            select 1
                                              from locks rl
                                              join pg_class c on c.oid = rl.relation
                                             where rl.virtualtransaction = xip.vtx
                                               and rl.locktype = 'relation'
                                               and rl.database = snap.db_oid
                                               and c.relnamespace = snap.ns_oid
                                        )
                                     or exists (
                                            select 1
                                              from locks al
                                             where al.virtualtransaction = xip.vtx
                                               and al.locktype = 'advisory'
                                               and al.database = snap.db_oid
                                               and al.classid = 1399957400::oid
                                               and al.objsubid = 2
                                               and al.objid = snap.ns_oid
                                        )
                                     or not exists (
                                            select 1
                                              from locks rl
                                              left join pg_class c
                                                on rl.locktype = 'relation'
                                               and rl.database = snap.db_oid
                                               and c.oid = rl.relation
                                              left join pg_namespace n
                                                on n.oid = c.relnamespace
                                             where rl.virtualtransaction = xip.vtx
                                               and rl.granted
                                               and (
                                                   (
                                                       rl.locktype = 'relation'
                                                       and rl.database is not null
                                                       and rl.database <> 0::oid
                                                       and rl.database <> snap.db_oid
                                                   )
                                                   or (
                                                       rl.locktype = 'relation'
                                                       and rl.database = snap.db_oid
                                                       and c.oid is not null
                                                       and c.relnamespace <> snap.ns_oid
                                                       and n.nspname not in ('pg_catalog', 'information_schema')
                                                       and left(n.nspname, 8) <> 'pg_temp_'
                                                       and left(n.nspname, 14) <> 'pg_toast_temp_'
                                                   )
                                                   or (
                                                       rl.locktype = 'advisory'
                                                       and rl.database = snap.db_oid
                                                       and rl.classid = 1399957400::oid
                                                       and rl.objsubid = 2
                                                       and rl.objid <> snap.ns_oid
                                                       and rl.mode = 'ExclusiveLock'
                                                   )
                                               )
                                        )),
                                pg_snapshot_xmax(snap.s)
                            )
                        end as safe_xid,
                        (pg_snapshot_xmin(snap.s) < pg_snapshot_xmax(snap.s)
                         and exists (select 1 from xip where vtx is null)) as wm_raced
                      from snap
                 )"
    };
}

mod acks;
mod activity;
mod alerts;
mod command_queue;
mod commit_map;
pub mod control_mask;
mod cost;
pub mod d6_refs;
mod event_log;
mod experience;
mod features;
mod fleet;
mod gateway;
mod grading;
mod host;
mod missions;
mod nodes;
mod passfile;
pub mod provenance;
mod recovery;
mod remote_jobs;
mod row;
mod scorecard;
mod sessions;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
mod verifications;
mod wake;

pub use acks::{AckRecord, ReportAnswer};
pub use control_mask::stored_text as control_stored_text;
pub use cost::{
    ArchiveCounted, ArchiveFile, ArchivedMessage, SessionCounted, SessionFile, SessionMessage,
};
pub use d6_refs::{DbIdentity, RefKind, RefLogRow, RefRow, SnapshotRow, SnapshotState};
pub use gateway::{
    HumanInput, HumanKey, HumanMaster, HumanResume, HumanTask, HumanWrite, MAX_MASTER_KEY_BYTES,
    MasterMessageMode,
};
pub use grading::RoutedTask;
pub use host::{
    DatabaseIdentity, LibpqDefaults, database_identity_with_defaults,
    test_database_size_with_passfile, test_schemas_with_passfile,
};
pub use missions::{ActivityEvent, LandingHold, PushRequest};
pub use nodes::StoreConnector;
pub use passfile::resolve_passfile_path;
pub use provenance::{
    ProvAttributionSource, ProvBlastRadiusNode, ProvBoundary, ProvChangeInput, ProvChangeKind,
    ProvChangeRecord, ProvDirection, ProvEdgeInput, ProvEdgeKind, ProvEdgeRecord, ProvEntryClass,
    ProvEntryInput, ProvEntryRecord, ProvLink,
};
pub use remote_jobs::{
    Committed, FaultNext, FaultOutcome, FaultStrike, NodeAction, NodeEnroll, NodeStatus, NodeV1,
    RemoteJob, RemoteJobKind, RemoteJobProgress, RemoteJobResult, RemoteJobSpec, RemoteJobState,
    SweepOutcome,
};
pub use row::RETIRED_EVENTS;
pub use sessions::{
    ExecutionRow, Freshness, LivenessFacts, LogEntry, OpenTool, SessionState, TimelineEntry,
};
pub use wake::Listener;

/// Schema installed by [`PostgresStore::migrate`].
const SCHEMA: &str = include_str!("../sql/schema.sql");

/// Conversion of a driver error.
fn backend(error: &postgres::Error) -> StoreError {
    if let Some(db) = error.as_db_error() {
        return match db.code() {
            // Not allowed, or a second active execution of one session.
            &postgres::error::SqlState::INSUFFICIENT_PRIVILEGE
            | &postgres::error::SqlState::EXCLUSION_VIOLATION => {
                StoreError::Rejected(db.message().to_string())
            }
            // The server is going away, or coming back (a restart: an
            // administrator's shutdown, a crash's, a server that cannot
            // serve connections yet). The work is not at fault; the loop
            // waits for the database instead of ending (crate::port's
            // `is_unreachable`, objective 032).
            &postgres::error::SqlState::ADMIN_SHUTDOWN
            | &postgres::error::SqlState::CRASH_SHUTDOWN
            | &postgres::error::SqlState::CANNOT_CONNECT_NOW => {
                StoreError::Unreachable(db.message().to_string())
            }
            _ => StoreError::Backend(format!(
                "{} ({}) {}",
                db.message(),
                db.code().code(),
                db.detail().unwrap_or("")
            )),
        };
    }
    // No server-side error: the server did not answer (a connection cut, a
    // refused connect, a timeout). The work is not at fault, and the loop
    // waits for the database instead of ending (crate::port's
    // `is_unreachable`).
    StoreError::Unreachable(error.to_string())
}

/// Explicit connection options injected by entry boundaries into [`PostgresStore`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectOptions {
    /// Optional explicit passfile path (`PGPASSFILE` / `$HOME/.pgpass`) resolved outside core.
    pub passfile: Option<std::path::PathBuf>,
    /// Optional per-session `statement_timeout` (with 5 s connect timeout).
    pub statement_timeout: Option<std::time::Duration>,
}

/// PostgreSQL store.
pub struct PostgresStore {
    client: Arc<Mutex<Client>>,
    /// Workers owned on this session. PostgreSQL advisory locks are
    /// re-entrant on one session, so this set still refuses a second Bridge.
    workers: Arc<Mutex<std::collections::HashSet<String>>>,
    /// The password in the URL this store connected with: the owner's, when
    /// it migrates, and the key the logins' passwords are derived from.
    password: String,
    /// The URL this store connected with, for the server-wide migration
    /// lock ([`PostgresStore::migrate`]).
    url: String,
    /// Optional passfile path passed when constructing this store, reused by reconnects/LISTEN.
    passfile: Option<std::path::PathBuf>,
    /// The statements this connection has prepared, by the SQL they were
    /// prepared from: the loop's fixed reads are parsed and planned once and
    /// reused, not once per pass ([`PostgresStore::query_prepared`]).
    prepared: Mutex<std::collections::HashMap<&'static str, postgres::Statement>>,
    /// The schema a test store moved into ([`Self::connect_in_schema`]),
    /// for a connection of its own made elsewhere (nodes.rs `StoreConnector`).
    schema: Option<String>,
}

/// The restricted logins, by their base name; each database has its own
/// role for each (`spawn_login_role`). `spawn_supervisor` and `spawn_reader`
/// act as the principals of their names, `spawn_human` (which may only call
/// `spawn_human_input`) as `human`, and `spawn_node_gateway` (spawn-node
/// v1's gateway, which may only call the four `spawn_node_gw_*` functions)
/// as nobody. Their passwords derive from the owner's ([`role_url_with_passfile`]) and are
/// set on every migration.
const LOGINS: [&str; 4] = [
    "spawn_supervisor",
    "spawn_reader",
    "spawn_human",
    remote_jobs::GATEWAY_LOGIN,
];

impl PostgresStore {
    /// Connect to the database in `url` using explicit boundary [`ConnectOptions`].
    pub fn connect_with_options(url: &str, options: &ConnectOptions) -> Result<Self, StoreError> {
        let mut config =
            passfile::config(url, options.passfile.as_deref()).map_err(|error| backend(&error))?;
        if let Some(timeout) = options.statement_timeout {
            let limit = format!("-c statement_timeout={}", timeout.as_millis().max(1));
            let merged = match config.get_options() {
                Some(existing) if !existing.trim().is_empty() => format!("{existing} {limit}"),
                _ => limit,
            };
            config
                .options(&merged)
                .connect_timeout(std::time::Duration::from_secs(5));
        }
        Self::connect_config(config, url, options.passfile.clone())
    }

    fn connect_config(
        mut config: postgres::Config,
        url: &str,
        passfile: Option<std::path::PathBuf>,
    ) -> Result<Self, StoreError> {
        // The schema warns (`raise warning`) about what a migration left for
        // a human, such as tables it kept. The many notices of an idempotent
        // migration ("already exists, skipping") stay quiet.
        config.notice_callback(|notice| {
            if notice.parsed_severity() == Some(postgres::error::Severity::Warning) {
                eprintln!("spawn: database: {}", notice.message());
            }
        });
        let client = config.connect(NoTls).map_err(|error| backend(&error))?;
        Ok(Self {
            client: Arc::new(Mutex::new(client)),
            workers: Arc::new(Mutex::new(std::collections::HashSet::new())),
            password: url_password_with_passfile(url, passfile.as_deref()),
            url: url.to_string(),
            passfile,
            prepared: Mutex::new(std::collections::HashMap::new()),
            schema: None,
        })
    }

    /// Connect and move this session into `schema`, creating it when absent,
    /// using explicit [`ConnectOptions`].
    ///
    /// Tests use one schema per case, so a shared database cannot leak
    /// commands or events between them. The session carries the schema's
    /// name as its `application_name`, and a schema it makes carries
    /// `{"made_at_ms": <ms>}` as its comment ([`made_schema_sql`]), so a
    /// reader tells a schema a live test holds, and how old one is
    /// (objective 094).
    pub fn connect_in_schema_with_options(
        url: &str,
        schema: &str,
        options: &ConnectOptions,
    ) -> Result<Self, StoreError> {
        if !schema
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            return Err(StoreError::Rejected(format!(
                "schema name {schema} is not a plain identifier"
            )));
        }
        let mut store = Self::connect_with_options(url, options)?;
        store.schema = Some(schema.to_string());
        {
            let mut client = store.lock();
            client
                .batch_execute(&format!(
                    "{}; set search_path to {schema}",
                    made_schema_sql(schema)
                ))
                .map_err(|error| backend(&error))?;
        }
        store.migrate()?;
        Ok(store)
    }

    /// Connect and move this session into `schema`, creating it when absent.
    pub fn connect_in_schema(url: &str, schema: &str) -> Result<Self, StoreError> {
        Self::connect_in_schema_with_options(url, schema, &ConnectOptions::default())
    }

    fn lock(&self) -> MutexGuard<'_, Client> {
        self.client
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A store over a connection of its own, in this store's schema and as
    /// its owner: `SELECT`s go through the caller (no migration, no lock on
    /// the shared connection). The integrator uses one for an owner's
    /// landing, so the landing hold it takes is its own session's and is
    /// never granted again on the shared connection a judging records
    /// through; the landing's records still go through the shared store.
    pub fn new_connection(&self) -> Result<PostgresStore, StoreError> {
        // The schema this store's session is in (a test store connects into
        // a schema the URL does not name).
        let schema: String = self
            .lock()
            .query_one("select current_schema()", &[])
            .map_err(|error| backend(&error))?
            .get(0);
        let store = PostgresStore::connect_with_options(
            &self.url,
            &ConnectOptions {
                passfile: self.passfile.clone(),
                statement_timeout: None,
            },
        )?;
        {
            let mut client = store.lock();
            client
                .batch_execute(&format!("set search_path to {schema}"))
                .map_err(|error| backend(&error))?;
        }
        Ok(store)
    }

    /// Install or refresh the schema, and set the logins' passwords.
    /// Idempotent: every process of one install derives the same ones.
    pub fn migrate(&self) -> Result<(), StoreError> {
        // Roles and grants are cluster-wide, so migrations of two databases
        // (or schemas) on one server race on them ("tuple concurrently
        // updated"). Advisory locks belong to one database, so migrations
        // take theirs in the server's maintenance database, which every
        // database on the server shares: one migration at a time per server.
        // Where that database cannot be reached, a lost race is run again
        // (every statement of the schema is idempotent), a bounded number of
        // times.
        let _server = server_migration_lock(&self.url, self.passfile.as_deref());
        let mut attempt: u32 = 0;
        loop {
            match self.migrate_once() {
                Err(error) if attempt < MIGRATE_ATTEMPTS && concurrent_catalog_update(&error) => {
                    attempt += 1;
                    std::thread::sleep(retry_pause(attempt));
                }
                // The node index after the schema's transaction, built
                // concurrently: appends never wait for it (nodes.rs).
                result => {
                    return result
                        .and_then(|()| self.node_index())
                        .map_err(|error| backend(&error));
                }
            }
        }
    }

    /// One migration, in one transaction: the schema takes the append lock
    /// first, so the migrations of one database (which share its logins)
    /// set the logins one at a time. Dropped on failure, it rolls back.
    fn migrate_once(&self) -> Result<(), postgres::Error> {
        let mut client = self.lock();
        let mut transaction = client.transaction()?;
        transaction.batch_execute(SCHEMA)?;
        // No owner password (peer or trust login, PGPASSWORD): a password
        // derived from "" would be a constant anyone can compute, and a
        // password an earlier migration set (or a fixed one from an older
        // install) would outlive the owner's choice to have none. Such a
        // migration turns the logins off (spawn_logins_off warns).
        if self.password.is_empty() {
            transaction.batch_execute("select spawn_logins_off()")?;
            remote_jobs::gateway_login_off(&mut transaction)?;
        } else {
            for base in LOGINS {
                // The schema above made the login and its salt, in this
                // transaction: it is there.
                let Some((role, salt)) = login_row(&mut transaction, base)? else {
                    continue;
                };
                // The server gets a SCRAM verifier computed here, never the
                // password: `alter role` takes no bind parameter, so its text
                // reaches pg_stat_activity and, on an error, the server log.
                // The verifier is base64, `$` and `:`, and the role
                // [a-z0-9_]: nothing needs quoting.
                let password = role_password(&self.password, &salt, &role);
                let verifier = postgres_protocol::password::scram_sha_256(password.as_bytes());
                transaction.batch_execute(&format!(
                    "alter role \"{role}\" with login password '{verifier}'"
                ))?;
            }
        }
        remote_jobs::gateway_login_timeouts(&mut transaction)?;
        transaction.commit()
    }
}

/// Key of the server-wide migration lock (an arbitrary constant).
const MIGRATION_LOCK_KEY: i64 = 1_399_957_400;

/// The migration lock, held in the server's `postgres` database until the
/// returned connection drops; `None` where that database cannot be reached.
fn server_migration_lock(url: &str, passfile: Option<&std::path::Path>) -> Option<Client> {
    let mut client = maintenance_config_with_passfile(url, passfile)?
        .connect(NoTls)
        .ok()?;
    client
        .execute("select pg_advisory_lock($1)", &[&MIGRATION_LOCK_KEY])
        .ok()?;
    Some(client)
}

/// `url`'s connection with its database replaced by the server's
/// maintenance database `postgres`, keeping host, port, credentials and
/// every other parameter, as the driver reads them; `None` when the URL
/// does not parse.
fn maintenance_config_with_passfile(
    url: &str,
    passfile: Option<&std::path::Path>,
) -> Option<postgres::Config> {
    let mut config = passfile::config(url, passfile).ok()?;
    config.dbname("postgres");
    Some(config)
}

#[cfg(test)]
fn maintenance_config(url: &str) -> Option<postgres::Config> {
    maintenance_config_with_passfile(url, None)
}

/// One restricted login of the database `client` is in: its role name
/// (`spawn_login_role`) and the salt in its comment, or `None` before the
/// schema made it. The schema must be on the connection's search path.
fn login_row(
    client: &mut impl postgres::GenericClient,
    base: &str,
) -> Result<Option<(String, String)>, postgres::Error> {
    let row = client.query_opt(
        "select r.rolname::text,
                substring(shobj_description(r.oid, 'pg_authid')
                          from '^spawn login salt ([0-9a-f]{64})$')
           from pg_roles r where r.rolname = spawn_login_role($1)",
        &[&base],
    )?;
    Ok(row.and_then(|row| {
        let salt: Option<String> = row.get(1);
        salt.map(|salt| (row.get(0), salt))
    }))
}

fn no_login(base: &str) -> StoreError {
    StoreError::Backend(format!(
        "{base}: this database has no such login yet; run spawn init-db"
    ))
}

/// How often a migration that lost a race on the cluster's catalogs is run
/// again.
const MIGRATE_ATTEMPTS: u32 = 12;

/// Whether `error` is another session changing the same cluster-wide catalog
/// row at the same moment: a lost race, not a broken migration.
fn concurrent_catalog_update(error: &postgres::Error) -> bool {
    use postgres::error::SqlState;
    let Some(database) = error.as_db_error() else {
        return false;
    };
    let code = database.code();
    *code == SqlState::T_R_SERIALIZATION_FAILURE
        || *code == SqlState::T_R_DEADLOCK_DETECTED
        || (*code == SqlState::INTERNAL_ERROR
            && database.message().contains("tuple concurrently"))
        // Two `create role` / `create type` at once: the loser hits the
        // catalog's unique index.
        || (*code == SqlState::UNIQUE_VIOLATION
            && database.constraint().is_some_and(|name| name.starts_with("pg_")))
}

/// A pause that grows with the attempt and differs between processes, so
/// racing migrations fall out of step: 20 ms * attempt, plus up to 50 ms.
fn retry_pause(attempt: u32) -> std::time::Duration {
    let jitter = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos() ^ std::process::id())
        % 50;
    std::time::Duration::from_millis(u64::from(20 * attempt + jitter))
}

/// The password `url` connects with, as the driver reads it (the user
/// info, decoded, or a `password=` parameter), else `passfile`'s matching
/// entry ([`passfile`], objective 109), or "".
fn url_password_with_passfile(url: &str, passfile: Option<&std::path::Path>) -> String {
    passfile::config(url, passfile)
        .ok()
        .and_then(|config| config.get_password().map(<[u8]>::to_vec))
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
fn url_password(url: &str) -> String {
    url_password_with_passfile(url, None)
}

/// PBKDF2-HMAC-SHA256 rounds of a login password: a guess at the owner's
/// password from a login's password costs this many HMACs.
const ROLE_PASSWORD_ROUNDS: u32 = 600_000;

/// The password of one restricted login: PBKDF2-HMAC-SHA256 of the owner's
/// password, salted with the login's random salt (in its role comment) and
/// its name, [`ROLE_PASSWORD_ROUNDS`] rounds, in hex. Only a holder of the
/// owner's password can form it, nothing secret is stored to agree on it,
/// and a login that knows its own password and salt still needs the rounds
/// for every guess at the owner's. Computed once per process.
fn role_password(owner_password: &str, salt: &str, role: &str) -> String {
    use std::collections::HashMap;
    use std::sync::{Arc, OnceLock};
    type Key = (String, String, String);
    static DERIVED: OnceLock<Mutex<HashMap<Key, Arc<OnceLock<String>>>>> = OnceLock::new();
    let key = (
        owner_password.to_string(),
        salt.to_string(),
        role.to_string(),
    );
    // One cell per login: threads asking for one login at once derive it
    // once, and different logins derive side by side.
    let cell = Arc::clone(
        DERIVED
            .get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(key)
            .or_default(),
    );
    cell.get_or_init(|| pbkdf2_hex(owner_password, salt, role))
        .clone()
}

fn pbkdf2_hex(owner_password: &str, salt: &str, role: &str) -> String {
    // The derivation itself is spawn-kdf's, compiled optimised in debug
    // builds too; its output is pinned (`the_login_passwords_are_the_ones_installs_have`).
    let output = spawn_kdf::pbkdf2_sha256(
        owner_password.as_bytes(),
        format!("spawn-login-v2\0{salt}\0{role}").as_bytes(),
        ROLE_PASSWORD_ROUNDS,
    );
    output.iter().fold(String::new(), |mut hex, byte| {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

/// The owner's URL with its credentials replaced by one of the database's
/// restricted logins (`spawn_supervisor`, `spawn_reader`, `spawn_human`: the
/// role of that name for this database, see `spawn_login_role`), using an explicit
/// `passfile` when the owner's URL omits its password. It reads the login's
/// name and salt through the owner's URL, which must be the one that
/// migrated (its search path finds the schema). The boundary is the
/// login's privileges; its password keeps anyone who lacks the owner's
/// credentials from using it, which matters once the database is reachable
/// from elsewhere.
///
/// # Errors
///
/// When the owner's URL is not `scheme://...`, has no password (even in
/// `passfile`), or the database is unreachable or not migrated.
pub fn role_url_with_passfile(
    owner: &str,
    role: &str,
    passfile: Option<&std::path::Path>,
) -> Result<String, String> {
    let password = url_password_with_passfile(owner, passfile);
    let Some((scheme, rest)) = owner.split_once("://").filter(|_| !password.is_empty()) else {
        return Err(format!(
            "{role}: the database URL must be scheme://user:password@host/..., or its \
             password a passfile line's (the restricted logins' passwords derive from the owner's)"
        ));
    };
    let mut config =
        passfile::config(owner, passfile).map_err(|error| format!("{role}: {error}"))?;
    config.connect_timeout(std::time::Duration::from_secs(5));
    let mut client = config
        .connect(NoTls)
        .map_err(|error| format!("{role}: {}", backend(&error)))?;
    let (name, salt) = login_row(&mut client, role)
        .map_err(|error| format!("{role}: {}", backend(&error)))?
        .ok_or_else(|| no_login(role).to_string())?;
    login_url(scheme, rest, &name, &role_password(&password, &salt, &name))
        .map_err(|error| format!("{role}: {error}"))
}

/// `scheme://role:password@` and the rest of the owner's URL (`rest`, what
/// follows `scheme://`): its host, port, database and parameters, without
/// its user info and without a `user=` or `password=` parameter. The result
/// is read back with the driver's `Config`: it must name `role` and
/// `password` and the owner's hosts, ports and database, or it is an error,
/// never a URL that would log in as someone else or somewhere else.
fn login_url(scheme: &str, rest: &str, role: &str, password: &str) -> Result<String, String> {
    let owner: postgres::Config = format!("{scheme}://{rest}")
        .parse()
        .map_err(|error: postgres::Error| error.to_string())?;
    // The user info ends the authority's last `@`; the authority ends at the
    // first `/` or `?`.
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let tail = match tail.split_once('?') {
        Some((path, query)) => {
            let kept: Vec<&str> = query
                .split('&')
                .filter(|pair| !pair.starts_with("user=") && !pair.starts_with("password="))
                .collect();
            if kept.is_empty() {
                path.to_string()
            } else {
                format!("{path}?{}", kept.join("&"))
            }
        }
        None => tail.to_string(),
    };
    let url = format!("{scheme}://{role}:{password}@{host}{tail}");
    let login: postgres::Config = url
        .parse()
        .map_err(|error: postgres::Error| error.to_string())?;
    let same = login.get_user() == Some(role)
        && login.get_password() == Some(password.as_bytes())
        && login.get_hosts() == owner.get_hosts()
        && login.get_ports() == owner.get_ports()
        && login.get_dbname() == owner.get_dbname();
    if same {
        Ok(url)
    } else {
        Err("the owner's URL does not give a login URL that reaches the same database".to_string())
    }
}

/// The identity of the server `url` reaches: its `system_identifier`, set
/// once by initdb. Two URLs that spell one server differently (socket or
/// TCP, a hostname, 127.0.0.2, a LAN address) give the same identity. One
/// read, no writes; the connection gives up after 5 seconds. Uses an explicit
/// `passfile` when `url` omits its password.
///
/// # Errors
///
/// When the URL does not parse, the server is unreachable, or the login may
/// not read `pg_control_system()`.
pub fn cluster_identity_with_passfile(
    url: &str,
    passfile: Option<&std::path::Path>,
) -> Result<String, StoreError> {
    let mut config = passfile::config(url, passfile).map_err(|error| backend(&error))?;
    config.connect_timeout(std::time::Duration::from_secs(5));
    let mut client = config.connect(NoTls).map_err(|error| backend(&error))?;
    let row = client
        .query_one(
            "select system_identifier::text from pg_control_system()",
            &[],
        )
        .map_err(|error| backend(&error))?;
    row.try_get(0).map_err(|error| backend(&error))
}

/// A probe of a test database that failed: the probe, in words, and the
/// error it met. Neither names the URL, so neither holds its password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeFailure {
    /// Human-readable description of the probe step that failed.
    pub probe: String,
    /// Diagnostic error message (sanitized; never includes connection credentials).
    pub error: String,
}

/// The rows the parallel probe counts.
const PROBE_ROWS: i64 = 100_000;

/// Whether the test database `url` can run what the workspace tests run:
/// a connection within `limit`, then, in one transaction that changes
/// nothing, a parallel query within `limit` (`debug_parallel_query`, or
/// `force_parallel_mode` before PostgreSQL 16, with parallel plans made
/// free): a Gather with a worker, so the server allocates the dynamic shared
/// memory a parallel query needs (/dev/shm in a container). A server whose
/// /dev/shm is too small fails it with "could not resize shared memory
/// segment" instead of failing a gate's tests 25 minutes in. Uses an
/// explicit `passfile` when `url` omits its password.
///
/// Each step is bounded by `limit` whatever the server does: the probe runs
/// on a thread of its own, abandoned if it does not answer in time.
///
/// # Errors
///
/// The step that failed and its error: a URL that does not parse, a
/// connection refused or not made within `limit`, a query that errs or does
/// not finish within `limit`.
pub fn parallel_query_probe_with_passfile(
    url: &str,
    limit: std::time::Duration,
    passfile: Option<&std::path::Path>,
) -> Result<(), ProbeFailure> {
    let seconds = limit.as_secs().max(1);
    let connect = format!("connect within {seconds} s");
    let query = format!(
        "a parallel query within {seconds} s (one transaction: SET LOCAL \
         max_parallel_workers_per_gather = 2, parallel_setup_cost = 0, parallel_tuple_cost = 0, \
         debug_parallel_query = on (force_parallel_mode before PostgreSQL 16); SELECT count(*) \
         FROM generate_series(1, {PROBE_ROWS}))"
    );
    let failure = |probe: &str, error: String| ProbeFailure {
        probe: probe.to_string(),
        error,
    };
    let mut config = passfile::config(url, passfile).map_err(|error| {
        failure(
            "read SPAWN_TEST_DATABASE_URL as a postgres URL",
            probe_error(&error),
        )
    })?;
    config.connect_timeout(limit);
    let (sender, receiver) = std::sync::mpsc::channel::<Result<bool, String>>();
    std::thread::Builder::new()
        .name("test-database-probe".to_string())
        .spawn(move || {
            let mut client = match config.connect(NoTls) {
                Ok(client) => client,
                Err(error) => {
                    let _ = sender.send(Err(probe_error(&error)));
                    return;
                }
            };
            if sender.send(Ok(false)).is_err() {
                return;
            }
            let _ = sender.send(parallel_query(&mut client, limit).map(|()| true));
        })
        .map_err(|error| failure(&connect, format!("cannot start the probe: {error}")))?;
    let late = |step: &str| format!("no answer within {seconds} s ({step})");
    match receiver.recv_timeout(limit) {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => return Err(failure(&connect, error)),
        Err(_) => return Err(failure(&connect, late("connecting"))),
    }
    match receiver.recv_timeout(limit) {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(failure(&query, error)),
        Err(_) => Err(failure(&query, late("querying"))),
    }
}

/// The probe's query on `client`: a parallel plan forced, counted, rolled
/// back. The server cancels it after `limit` too.
fn parallel_query(client: &mut Client, limit: std::time::Duration) -> Result<(), String> {
    let error = |error: postgres::Error| probe_error(&error);
    let mut transaction = client.transaction().map_err(error)?;
    let version: i32 = transaction
        .query_one("select current_setting('server_version_num')::integer", &[])
        .map_err(error)?
        .get(0);
    // Renamed in PostgreSQL 16.
    let force = if version >= 160_000 {
        "debug_parallel_query"
    } else {
        "force_parallel_mode"
    };
    transaction
        .batch_execute(&format!(
            "set local statement_timeout = {}; \
             set local max_parallel_workers_per_gather = 2; \
             set local parallel_setup_cost = 0; \
             set local parallel_tuple_cost = 0; \
             set local {force} = on",
            limit.as_millis().max(1)
        ))
        .map_err(error)?;
    let count: i64 = transaction
        .query_one(
            &format!("select count(*) from generate_series(1, {PROBE_ROWS})"),
            &[],
        )
        .map_err(error)?
        .get(0);
    transaction.rollback().map_err(error)?;
    if count == PROBE_ROWS {
        Ok(())
    } else {
        Err(format!("the query counted {count} rows, not {PROBE_ROWS}"))
    }
}

/// A driver error in words: the server's own message and code when it sent
/// one, else the driver's (which says what the connection met).
fn probe_error(error: &postgres::Error) -> String {
    match error.as_db_error() {
        Some(db) => format!("{}: {} ({})", db.severity(), db.message(), db.code().code()),
        None => error.to_string(),
    }
}

/// Statement parameters, as the driver takes them.
type Params<'a> = [&'a (dyn ToSql + Sync)];

/// One statement under the connection lock, with driver errors converted.
/// A method that needs several statements without interleaving takes
/// [`PostgresStore::lock`] itself.
impl PostgresStore {
    fn query(&self, sql: &str, params: &Params<'_>) -> Result<Vec<Row>, StoreError> {
        self.lock()
            .query(sql, params)
            .map_err(|error| backend(&error))
    }

    fn query_opt(&self, sql: &str, params: &Params<'_>) -> Result<Option<Row>, StoreError> {
        self.lock()
            .query_opt(sql, params)
            .map_err(|error| backend(&error))
    }

    fn query_one(&self, sql: &str, params: &Params<'_>) -> Result<Row, StoreError> {
        self.lock()
            .query_one(sql, params)
            .map_err(|error| backend(&error))
    }

    /// The single value of a query that returns one row with one column,
    /// typically a `jsonb` aggregate.
    fn query_json(&self, sql: &str, params: &Params<'_>) -> Result<serde_json::Value, StoreError> {
        Ok(self.query_one(sql, params)?.get(0))
    }

    /// A fixed statement, prepared on this connection the first time and
    /// reused after: the loop runs the same reads on every pass, and parsing
    /// and planning them again each time is work with no decision in it.
    /// `sql` is `'static` so the statement cache is keyed by the SQL text
    /// this build ships, never by a string built at run time.
    fn query_prepared(
        &self,
        sql: &'static str,
        params: &Params<'_>,
    ) -> Result<Vec<Row>, StoreError> {
        let mut client = self.lock();
        let mut prepared = self
            .prepared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let statement = if let Some(statement) = prepared.get(sql) {
            statement.clone()
        } else {
            let statement = client.prepare(sql).map_err(|error| backend(&error))?;
            prepared.insert(sql, statement.clone());
            statement
        };
        drop(prepared);
        client
            .query(&statement, params)
            .map_err(|error| backend(&error))
    }

    /// One row from a fixed statement (prepared once, [`Self::query_prepared`]).
    fn query_one_prepared(
        &self,
        sql: &'static str,
        params: &Params<'_>,
    ) -> Result<Row, StoreError> {
        let mut rows = self.query_prepared(sql, params)?;
        rows.pop().ok_or_else(|| {
            StoreError::Backend(format!("a prepared statement returned no row: {sql}"))
        })
    }

    /// The single `jsonb` value of a fixed statement (prepared once,
    /// [`Self::query_prepared`]).
    fn query_json_prepared(
        &self,
        sql: &'static str,
        params: &Params<'_>,
    ) -> Result<serde_json::Value, StoreError> {
        Ok(self.query_one_prepared(sql, params)?.get(0))
    }

    /// [`Self::query_json`] for a query that may return no row.
    fn query_opt_json(
        &self,
        sql: &str,
        params: &Params<'_>,
    ) -> Result<Option<serde_json::Value>, StoreError> {
        Ok(self.query_opt(sql, params)?.map(|row| row.get(0)))
    }

    /// Rows changed.
    fn execute(&self, sql: &str, params: &Params<'_>) -> Result<u64, StoreError> {
        self.lock()
            .execute(sql, params)
            .map_err(|error| backend(&error))
    }
}

/// The statements that make a test's `schema` when absent and name the
/// session after it (objective 094): `application_name` is the schema's
/// name (unless the URL named the session itself: a test that finds its own
/// backend by a name of its own keeps it), so a reader sees which schemas a
/// live session holds, and a schema
/// made here carries `{"made_at_ms": <ms>}` (the server's clock) as its
/// comment, the time a reader dates it by. A schema another session made
/// first keeps its own comment. `schema` must be a plain identifier (the
/// callers check it): it goes into SQL as it is.
pub fn made_schema_sql(schema: &str) -> String {
    format!(
        "do $named$ begin \
           if current_setting('application_name') = '' then \
             perform set_config('application_name', '{schema}', false); \
           end if; \
         end $named$; \
         do $made$ \
         declare \
           v_ns_oid bigint; \
           v_ns_key integer; \
         begin \
           begin \
             if not exists (select 1 from pg_namespace where nspname = '{schema}') then \
               create schema {schema}; \
               v_ns_oid := (quote_ident('{schema}')::regnamespace)::oid::bigint; \
               v_ns_key := (v_ns_oid - case when v_ns_oid >= 2147483648 then 4294967296 else 0 end)::integer; \
               perform pg_advisory_xact_lock(1399957400, v_ns_key); \
               execute format('comment on schema {schema} is %L', \
                 jsonb_build_object('made_at_ms', \
                   floor(extract(epoch from clock_timestamp()) * 1000)::bigint)::text); \
             end if; \
           exception when duplicate_schema then null; \
           end; \
           v_ns_oid := (quote_ident('{schema}')::regnamespace)::oid::bigint; \
           v_ns_key := (v_ns_oid - case when v_ns_oid >= 2147483648 then 4294967296 else 0 end)::integer; \
           perform pg_advisory_xact_lock(1399957400, v_ns_key); \
         end $made$"
    )
}

#[cfg(test)]
mod tests {
    use super::{ConnectOptions, login_url, role_password, role_url_with_passfile, url_password};

    /// The restricted logins' passwords are byte for byte what every install
    /// already has (computed by the derivation as it was at 554a4bd4, before
    /// spawn-kdf): a different one would set new passwords at the next
    /// migration and lock the fleet's other processes out of its database.
    /// The last case is a login as an install names it
    /// (`spawn_login_role('spawn_supervisor')` in database `spawn`).
    #[test]
    fn the_login_passwords_are_the_ones_installs_have() {
        for (owner, salt, role, password) in [
            (
                "owner",
                "ab".repeat(32),
                "spawn_reader_db",
                "f4e769b999554a01831a49183a5a9ffe13646043436172744f18a3fed9cc7185",
            ),
            (
                "",
                "00".to_string(),
                "spawn_human_x",
                "b06e99b80df3e38614f43f508ccd574abe465cfad207b8f37bbf23b6313b5798",
            ),
            (
                "p\u{e4}ssw\u{f6}rd with spaces",
                "cd".repeat(32),
                "spawn_supervisor_spawn_test",
                "b369a5c6b48d61eab9515eebe860416ee78c572573d7f450db8171e28804ddad",
            ),
            (
                "fleet-owner-secret",
                "0123456789abcdef".repeat(4),
                "spawn_supervisor_spawn",
                "2b9dc89a75cd2e38bf9d10f7272d67a6c698e8e1fa53fd4ae8498d40f66a3b6a",
            ),
        ] {
            // role_password, the path a migration takes (cached), and
            // through it pbkdf2_hex.
            assert_eq!(role_password(owner, &salt, role), password, "{role}");
        }
    }

    /// A dashboard reader gives up on a slow statement (audit M2): the
    /// timeout is its connection's, not the server's. Needs
    /// `SPAWN_TEST_DATABASE_URL`, like the integration tests.
    #[test]
    fn a_readers_statement_gives_up_after_its_timeout() {
        use std::time::{Duration, Instant};
        let url = std::env::var("SPAWN_TEST_DATABASE_URL")
            .expect("SPAWN_TEST_DATABASE_URL must point at a disposable database");
        let reader = super::PostgresStore::connect_with_options(
            &url,
            &ConnectOptions {
                passfile: None,
                statement_timeout: Some(Duration::from_millis(200)),
            },
        )
        .expect("connect");
        let started = Instant::now();
        let slow = reader.lock().query_one("select pg_sleep(5)::text", &[]);
        let error = slow.expect_err("a five-second statement ran to the end");
        assert_eq!(
            error.code(),
            Some(&postgres::error::SqlState::QUERY_CANCELED),
            "{error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        let plain = super::PostgresStore::connect_with_options(&url, &ConnectOptions::default())
            .expect("connect");
        assert!(
            plain
                .lock()
                .query_one("select pg_sleep(0.5)::text", &[])
                .is_ok()
        );
    }

    /// [`a_migration_without_the_owner_password_turns_the_logins_off`]'s check
    /// after the passwordless migration: the four logins cannot log in, read
    /// from `pg_roles`, which every login reads (`pg_authid` is the
    /// superuser's; the test server's login need not be one). Its `rolpassword`
    /// is a mask, so "no password is left" is shown by the refused logins, and
    /// read from `pg_authid` only where the login may.
    fn assert_logins_off(name: &str, ours: &str) {
        use postgres::{Client, NoTls};
        let logins = "rolname in ('spawn_supervisor_' || $1, 'spawn_reader_' || $1,
                              'spawn_human_' || $1, 'spawn_node_gateway_' || $1)";
        let mut admin = Client::connect(ours, NoTls).expect("admin");
        let off: Vec<(String, bool)> = admin
            .query(
                &format!(
                    "select rolname::text, rolcanlogin from pg_roles where {logins} order by 1"
                ),
                &[&name],
            )
            .expect("the logins")
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect();
        assert_eq!(
            off,
            [
                (format!("spawn_human_{name}"), false),
                (format!("spawn_node_gateway_{name}"), false),
                (format!("spawn_reader_{name}"), false),
                (format!("spawn_supervisor_{name}"), false),
            ]
        );
        let superuser: bool = admin
            .query_one(
                "select rolsuper from pg_roles where rolname = current_user",
                &[],
            )
            .expect("the test login")
            .get(0);
        if superuser {
            let passwords: i64 = admin
                .query_one(
                    &format!(
                        "select count(*) from pg_authid where {logins} and rolpassword is null"
                    ),
                    &[&name],
                )
                .expect("the passwords")
                .get(0);
            assert_eq!(passwords, 4, "no password is left");
        }
    }

    /// With no owner password (a peer or trust login: nothing in the URL,
    /// so nothing the logins could derive from), a migration turns the
    /// logins off, says so, and removes a password an earlier install set;
    /// a migration with the password turns them on again. Needs
    /// `SPAWN_TEST_DATABASE_URL`; the case gets a database of its own.
    #[test]
    fn a_migration_without_the_owner_password_turns_the_logins_off() {
        use postgres::{Client, NoTls};
        let url = std::env::var("SPAWN_TEST_DATABASE_URL")
            .expect("SPAWN_TEST_DATABASE_URL must point at a disposable database");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let name = format!("spawn_test_{}_{nanos:x}_logins_off", std::process::id());
        let mut admin = Client::connect(&url, NoTls).expect("admin");
        admin
            .batch_execute(&format!("create database {name}"))
            .expect("a database of its own");
        let mut config: postgres::Config = url.parse().expect("parses");
        config.dbname(&name);
        let ours = url
            .rsplit_once('/')
            .map_or(url.clone(), |(base, _)| format!("{base}/{name}"));
        let login = format!("spawn_supervisor_{name}");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let store =
                super::PostgresStore::connect_with_options(&ours, &ConnectOptions::default())
                    .expect("connect");
            store.migrate().expect("migrate with the owner's password");
            let supervisor =
                role_url_with_passfile(&ours, "spawn_supervisor", None).expect("the login's URL");
            assert!(Client::connect(&supervisor, NoTls).is_ok());
            // As an earlier install left it: a fixed password anyone can guess.
            Client::connect(&url, NoTls)
                .expect("admin")
                .batch_execute(&format!("alter role {login} login password '{login}'"))
                .expect("an earlier install's password");
            let mut fixed = config.clone();
            fixed.user(&login).password(&login);
            assert!(fixed.connect(NoTls).is_ok());

            let mut passwordless =
                super::PostgresStore::connect_with_options(&ours, &ConnectOptions::default())
                    .expect("connect");
            passwordless.password = String::new();
            passwordless
                .migrate()
                .expect("migrate without the owner's password");
            assert!(
                fixed.connect(NoTls).is_err(),
                "the earlier password is gone"
            );
            assert!(
                Client::connect(&supervisor, NoTls).is_err(),
                "the derived one too"
            );
            assert_logins_off(&name, &ours);
            // It says so.
            let warnings = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let sink = std::sync::Arc::clone(&warnings);
            let mut owner = config.clone();
            owner.notice_callback(move |notice| {
                if notice.parsed_severity() == Some(postgres::error::Severity::Warning) {
                    sink.lock()
                        .expect("warnings")
                        .push(notice.message().to_string());
                }
            });
            owner
                .connect(NoTls)
                .expect("connect")
                .batch_execute("select spawn_logins_off()")
                .expect("turn them off again");
            let warnings = warnings.lock().expect("warnings").clone();
            assert_eq!(warnings.len(), 1, "{warnings:?}");
            assert!(
                warnings[0].contains("no owner password") && warnings[0].contains(&login),
                "{warnings:?}"
            );
            // A migration with the password turns them on again.
            super::PostgresStore::connect_with_options(&ours, &ConnectOptions::default())
                .expect("connect")
                .migrate()
                .expect("migrate with the owner's password");
            assert!(Client::connect(&supervisor, NoTls).is_ok());
            assert!(fixed.connect(NoTls).is_err());
        }));
        let _ = admin.batch_execute(&format!("drop database if exists {name} with (force)"));
        for base in super::LOGINS {
            let _ = admin.batch_execute(&format!("drop role if exists {base}_{name}"));
        }
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }

    #[test]
    fn the_migration_lock_lives_in_the_servers_maintenance_database() {
        use super::maintenance_config;
        let config = maintenance_config("postgres://u:p%40ss@db.local:5433/spawn?sslmode=disable")
            .expect("parses");
        assert_eq!(config.get_dbname(), Some("postgres"));
        assert_eq!(config.get_user(), Some("u"));
        assert_eq!(config.get_password(), Some(&b"p@ss"[..]));
        assert_eq!(config.get_ports(), [5433]);
        let socket = maintenance_config("postgres://u@%2Frun%2Fpostgresql/spawn").expect("parses");
        assert_eq!(socket.get_dbname(), Some("postgres"));
        assert_eq!(
            socket.get_hosts(),
            [postgres::config::Host::Unix("/run/postgresql".into())]
        );
        assert!(maintenance_config("not a url").is_none());
    }

    #[test]
    fn every_spelling_of_the_owner_password_is_one_password() {
        // As the driver reads it, the one that connects: decoded user info,
        // or a `password=` parameter.
        assert_eq!(url_password("postgres://o:p%40ss@h/db"), "p@ss");
        assert_eq!(url_password("postgres://o@h/db?password=p%40ss"), "p@ss");
        assert_eq!(
            url_password("host=h user=o password=p@ss dbname=db"),
            "p@ss"
        );
        assert_eq!(url_password("postgres://o@h/db"), "");
        assert_eq!(url_password("not a url"), "");
    }

    #[test]
    fn a_login_password_is_salted_per_login_and_derived_slowly() {
        let salt = "ab".repeat(32);
        let password = role_password("owner", &salt, "spawn_reader_db");
        assert_eq!(password.len(), 64);
        assert_eq!(password, role_password("owner", &salt, "spawn_reader_db"));
        assert_ne!(
            password,
            role_password("owner", &"cd".repeat(32), "spawn_reader_db")
        );
        assert_ne!(
            password,
            role_password("owner", &salt, "spawn_supervisor_db")
        );
        assert_ne!(password, role_password("other", &salt, "spawn_reader_db"));
        // PBKDF2-HMAC-SHA256, checked against Python's hashlib.pbkdf2_hmac
        // ('sha256', b'owner', b'spawn-login-v2\0' + salt + b'\0spawn_reader_db',
        // 600000).hex().
        assert_eq!(
            password,
            "f4e769b999554a01831a49183a5a9ffe13646043436172744f18a3fed9cc7185"
        );
    }

    #[test]
    fn no_owner_password_means_no_login_url() {
        assert!(
            role_url_with_passfile("postgres://spawn@localhost/spawn", "spawn_reader", None)
                .is_err()
        );
        assert!(
            role_url_with_passfile("host=/run/postgresql dbname=spawn", "spawn_reader", None)
                .is_err()
        );
        assert_eq!(
            login_url(
                "postgres",
                "o:pw@h:1/db?sslmode=disable",
                "spawn_reader_db",
                "secret"
            ),
            Ok("postgres://spawn_reader_db:secret@h:1/db?sslmode=disable".to_string())
        );
        // A password given as a parameter, or an `@` in a parameter, never
        // moves the login elsewhere or keeps the owner's credentials.
        assert_eq!(
            login_url(
                "postgres",
                "o@h:1/db?password=owner&application_name=a@b&user=o",
                "spawn_reader_db",
                "secret"
            ),
            Ok("postgres://spawn_reader_db:secret@h:1/db?application_name=a@b".to_string())
        );
        assert_eq!(
            login_url("postgres", "o:p%40ss@h/db", "spawn_reader_db", "secret"),
            Ok("postgres://spawn_reader_db:secret@h/db".to_string())
        );
    }

    #[test]
    fn explicit_passfile_resolves_url_password_and_connect_options_connects() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!(
            "spawn-lib-passfile-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let pf = dir.join("pgpass");
        std::fs::write(&pf, "db.local:5433:spawn_db:spawn_u:passfileSecret!\n").expect("write");
        std::fs::set_permissions(&pf, std::fs::Permissions::from_mode(0o600)).expect("chmod");

        let url = "postgres://spawn_u@db.local:5433/spawn_db";
        assert_eq!(super::url_password_with_passfile(url, None), "");
        assert_eq!(
            super::url_password_with_passfile(url, Some(&pf)),
            "passfileSecret!"
        );

        let opts = super::ConnectOptions {
            passfile: Some(pf.clone()),
            statement_timeout: None,
        };
        assert_eq!(opts.passfile.as_deref(), Some(pf.as_path()));
        assert!(opts.statement_timeout.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
