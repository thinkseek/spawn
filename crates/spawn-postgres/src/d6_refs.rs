//! D6 Phase 4 Step 1 (`#113` Fine Item 7.2): PostgreSQL CAS snapshot ledger,
//! atomic ref CAS (`spawn_refs`), append-only ref history (`spawn_ref_log`),
//! and cluster identity for `spawn-cas` GC fencing.

use std::fmt;
use std::str::FromStr;

use postgres::Row;
use serde::{Deserialize, Serialize};
use spawn_core::port::StoreError;

use crate::PostgresStore;

/// Classification of a ref row in `spawn_refs` and `spawn_ref_log`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefKind {
    /// Protected project main line; advances only via [`PostgresStore::land_main_ref`].
    Main,
    /// Task or feature branch.
    Branch,
    /// Background WIP flush ref (`wip/<branch>`).
    Wip,
    /// Immutable tag ref.
    Tag,
}

impl RefKind {
    /// Wire/database string representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Branch => "branch",
            Self::Wip => "wip",
            Self::Tag => "tag",
        }
    }
}

impl fmt::Display for RefKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for RefKind {
    type Err = StoreError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "main" => Ok(Self::Main),
            "branch" => Ok(Self::Branch),
            "wip" => Ok(Self::Wip),
            "tag" => Ok(Self::Tag),
            other => Err(StoreError::Backend(format!("unknown RefKind: {other:?}"))),
        }
    }
}

/// Reachability lifecycle state of a `spawn_snapshots` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotState {
    /// Snapshot metadata is active and its objects are eligible for retention.
    Live,
    /// Snapshot was unreachable and its CAS objects were garbage-collected.
    Collected,
}

impl SnapshotState {
    /// Wire/database string representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Collected => "collected",
        }
    }
}

impl fmt::Display for SnapshotState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for SnapshotState {
    type Err = StoreError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "live" => Ok(Self::Live),
            "collected" => Ok(Self::Collected),
            other => Err(StoreError::Backend(format!(
                "unknown SnapshotState: {other:?}"
            ))),
        }
    }
}

/// A row in `spawn_snapshots`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRow {
    /// Canonical `sha256:<64 hex>` snapshot digest.
    pub id: String,
    /// Deterministic 40-hex projected Git commit OID.
    pub git_oid: String,
    /// Root `spawn-tree-v1` `sha256:<64 hex>` digest.
    pub tree: String,
    /// Parent snapshot `sha256:<64 hex>` digests in first-parent order.
    pub parents: Vec<String>,
    /// Project name this snapshot belongs to.
    pub project: String,
    /// Lifecycle state (`live` or `collected`).
    pub state: SnapshotState,
    /// Creation timestamp in Unix epoch milliseconds.
    pub created_at_ms: i64,
}

/// A row in `spawn_refs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefRow {
    /// Project name.
    pub project: String,
    /// Reference name (e.g. `main`, `branch/foo`, `wip/foo`).
    pub name: String,
    /// Ref classification (`main`, `branch`, `wip`, `tag`).
    pub kind: RefKind,
    /// Target `spawn_snapshots.id` (`sha256:<64 hex>`).
    pub snapshot: String,
    /// Last update timestamp in Unix epoch milliseconds.
    pub updated_at_ms: i64,
}

/// A row in the append-only `spawn_ref_log` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefLogRow {
    /// Monotonic log sequence number.
    pub seq: i64,
    /// Project name.
    pub project: String,
    /// Reference name.
    pub name: String,
    /// Ref classification (`main`, `branch`, `wip`, `tag`).
    pub kind: RefKind,
    /// Previous snapshot (`None` when the ref was newly created).
    pub old_snapshot: Option<String>,
    /// New snapshot (`sha256:<64 hex>`).
    pub new_snapshot: String,
    /// Human/system transition reason.
    pub reason: String,
    /// Transition timestamp in Unix epoch milliseconds.
    pub at_ms: i64,
}

/// Live PostgreSQL cluster/database/schema identity used to verify `spawn-cas`
/// GC fencing before any persistent garbage collection pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DbIdentity {
    /// Cluster `system_identifier` from `pg_control_system()`.
    pub db_system_id: String,
    /// Database `oid` from `pg_database` for `current_database()`.
    pub db_oid: u32,
    /// Active schema name from `current_schema()`.
    pub schema_name: String,
}

const SNAPSHOT_COLUMNS: &str = "id, git_oid, tree, parents, project, state, created_at_ms";

fn snapshot_row(row: &Row) -> Result<SnapshotRow, StoreError> {
    Ok(SnapshotRow {
        id: row.get(0),
        git_oid: row.get(1),
        tree: row.get(2),
        parents: row.get(3),
        project: row.get(4),
        state: row.get::<_, String>(5).parse()?,
        created_at_ms: row.get(6),
    })
}

fn ref_row(row: &Row) -> Result<RefRow, StoreError> {
    Ok(RefRow {
        project: row.get(0),
        name: row.get(1),
        kind: row.get::<_, String>(2).parse()?,
        snapshot: row.get(3),
        updated_at_ms: row.get(4),
    })
}

fn ref_log_row(row: &Row) -> Result<RefLogRow, StoreError> {
    Ok(RefLogRow {
        seq: row.get(0),
        project: row.get(1),
        name: row.get(2),
        kind: row.get::<_, String>(3).parse()?,
        old_snapshot: row.get(4),
        new_snapshot: row.get(5),
        reason: row.get(6),
        at_ms: row.get(7),
    })
}

impl PostgresStore {
    /// Idempotently record a D6 CAS snapshot row (`spawn_snapshot_record`, owner only).
    ///
    /// Returns `id` on insert or identical replay, or an error (`23505`) if `id`
    /// already exists with different metadata or `git_oid` collides.
    pub fn record_snapshot(
        &self,
        id: &str,
        git_oid: &str,
        tree: &str,
        parents: &[String],
        project: &str,
        created_at_ms: i64,
    ) -> Result<String, StoreError> {
        Ok(self
            .query_one(
                "select spawn_snapshot_record($1, $2, $3, $4, $5, $6)",
                &[&id, &git_oid, &tree, &parents, &project, &created_at_ms],
            )?
            .get(0))
    }

    /// Fetch a snapshot row by its canonical `sha256:<64 hex>` digest.
    pub fn get_snapshot(&self, id: &str) -> Result<Option<SnapshotRow>, StoreError> {
        self.query_opt(
            &format!("select {SNAPSHOT_COLUMNS} from spawn_snapshots where id = $1"),
            &[&id],
        )?
        .as_ref()
        .map(snapshot_row)
        .transpose()
    }

    /// Fetch a snapshot row by its projected 40-hex Git commit OID.
    pub fn get_snapshot_by_oid(&self, git_oid: &str) -> Result<Option<SnapshotRow>, StoreError> {
        self.query_opt(
            &format!("select {SNAPSHOT_COLUMNS} from spawn_snapshots where git_oid = $1"),
            &[&git_oid],
        )?
        .as_ref()
        .map(snapshot_row)
        .transpose()
    }

    /// Atomically compare-and-swap a non-main ref (`branch`, `wip`, `tag`) and
    /// append a `spawn_ref_log` entry (`spawn_ref_cas`, owner only).
    ///
    /// Refuses `RefKind::Main` (`StoreError::Rejected`). Returns the appended
    /// `spawn_ref_log.seq`.
    pub fn cas_ref(
        &self,
        project: &str,
        name: &str,
        kind: RefKind,
        expected: Option<&str>,
        new_snapshot: &str,
        reason: &str,
    ) -> Result<i64, StoreError> {
        Ok(self
            .query_one(
                "select spawn_ref_cas($1, $2, $3, $4, $5, $6)",
                &[
                    &project,
                    &name,
                    &kind.as_str(),
                    &expected,
                    &new_snapshot,
                    &reason,
                ],
            )?
            .get(0))
    }

    /// Atomically advance or bootstrap a `kind = 'main'` ref (`spawn_ref_land_main`,
    /// owner only) and append a `spawn_ref_log` entry.
    ///
    /// Bootstrapping (`expected_base == None`) requires `task_id == None` and an
    /// absent ref; advancing from `expected_base` requires a `landing` task in
    /// `mission_tasks` with a matching accepted `task.gated` verdict in
    /// `spawn_events`. Returns the appended `spawn_ref_log.seq`.
    pub fn land_main_ref(
        &self,
        project: &str,
        name: &str,
        expected_base: Option<&str>,
        new_snapshot: &str,
        task_id: Option<i64>,
        reason: &str,
    ) -> Result<i64, StoreError> {
        Ok(self
            .query_one(
                "select spawn_ref_land_main($1, $2, $3, $4, $5, $6)",
                &[
                    &project,
                    &name,
                    &expected_base,
                    &new_snapshot,
                    &task_id,
                    &reason,
                ],
            )?
            .get(0))
    }

    /// List live refs in `spawn_refs`, optionally filtered to one `project`.
    pub fn list_refs(&self, project: Option<&str>) -> Result<Vec<RefRow>, StoreError> {
        let rows = match project {
            Some(project) => self.query(
                "select project, name, kind, snapshot, updated_at_ms
                   from spawn_refs
                  where project = $1
                  order by project, name",
                &[&project],
            )?,
            None => self.query(
                "select project, name, kind, snapshot, updated_at_ms
                   from spawn_refs
                  order by project, name",
                &[],
            )?,
        };
        rows.iter().map(ref_row).collect()
    }

    /// List `spawn_ref_log` entries strictly after `after_seq`, ascending by `seq`.
    pub fn list_ref_log_after(&self, after_seq: i64) -> Result<Vec<RefLogRow>, StoreError> {
        self.query(
            "select seq, project, name, kind, old_snapshot, new_snapshot, reason, at_ms
               from spawn_ref_log
              where seq > $1
              order by seq",
            &[&after_seq],
        )?
        .iter()
        .map(ref_log_row)
        .collect()
    }

    /// Return the highest committed `spawn_ref_log.seq`, or `0` when empty.
    pub fn latest_ref_log_seq(&self) -> Result<u64, StoreError> {
        let latest: i64 = self
            .query_one("select coalesce(max(seq), 0) from spawn_ref_log", &[])?
            .get(0);
        u64::try_from(latest)
            .map_err(|_| StoreError::Backend(format!("negative spawn_ref_log seq: {latest}")))
    }

    /// Mark unreferenced `live` snapshots as `collected`
    /// (`spawn_snapshot_mark_collected`, owner only). Returns the updated row count.
    pub fn mark_snapshots_collected(&self, ids: &[String]) -> Result<i32, StoreError> {
        if ids.is_empty() {
            return Ok(0);
        }
        Ok(self
            .query_one("select spawn_snapshot_mark_collected($1)", &[&ids])?
            .get(0))
    }

    /// Read the live PostgreSQL cluster, database OID, and active schema identity
    /// for `spawn-cas` GC fencing.
    pub fn database_identity(&self) -> Result<DbIdentity, StoreError> {
        let row = self.query_one(
            "select (select system_identifier::text from pg_control_system()),
                    (select oid::bigint from pg_database where datname = current_database()),
                    current_schema()::text",
            &[],
        )?;
        let db_system_id: String = row.get(0);
        let oid_raw: i64 = row.get(1);
        let db_oid = u32::try_from(oid_raw)
            .map_err(|_| StoreError::Backend(format!("invalid pg_database oid: {oid_raw}")))?;
        let schema_name: Option<String> = row.get(2);
        let schema_name = schema_name
            .ok_or_else(|| StoreError::Backend("current_schema() returned null".to_string()))?;
        Ok(DbIdentity {
            db_system_id,
            db_oid,
            schema_name,
        })
    }
}
