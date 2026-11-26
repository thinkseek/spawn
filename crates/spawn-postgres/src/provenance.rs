//! Universal file provenance (PV1): which change made each file as it is,
//! who made it and what it used, in three append-only tables
//! (`prov_changes`, `prov_entries`, `prov_edges`, `sql/schema.sql`).
//!
//! Writes go through `spawn_prov_record_change` alone (owner only): one
//! change, its entries and its edges, atomically. Recording a
//! `(change_id, rev)` again with the same content returns the recorded id;
//! with different content it is refused. Nothing is ever updated or deleted:
//! triggers refuse it.
//!
//! Reads: one change by `(change_id, rev)` or id, the changes that touched a
//! path or were made by an agent or an execution, and the blast radius of a
//! change ([`PostgresStore::prov_blast_radius`]): the changes that depend on
//! it (forward) or that it depends on (backward), up to a depth, each
//! expanded once (a breadth-first walk), so diamonds, cliques and cycles
//! cost what they hold, not their paths.

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use postgres::Row;
use serde_json::{Map, Value, json};
use spawn_core::port::StoreError;

use crate::PostgresStore;

/// A closed set of names stored as text: each enum below is one of the
/// schema's `check (… in (…))` lists.
macro_rules! text_enum {
    ($(#[$meta:meta])* $name:ident { $($(#[$vmeta:meta])* $variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $($(#[$vmeta])* $variant),+
        }

        impl $name {
            /// Every value, in the schema's order.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// The value as stored.
            #[must_use]
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = StoreError;

            fn from_str(text: &str) -> Result<Self, StoreError> {
                match text {
                    $($text => Ok(Self::$variant),)+
                    other => Err(StoreError::Backend(format!(
                        concat!("an unknown ", stringify!($name), ": {:?}"),
                        other
                    ))),
                }
            }
        }
    };
}

text_enum! {
    /// What a change did (`prov_changes.kind`).
    ProvChangeKind {
        /// An agent's edit of files.
        Edit => "edit",
        /// Content derived from other content (a build, a generator).
        Derive => "derive",
        /// A landing onto a source's main line.
        Land => "land",
        /// The undoing of an earlier change.
        Revert => "revert",
        /// Content brought in from outside.
        Import => "import",
    }
}

text_enum! {
    /// Who attributed a change (`prov_changes.attributed`).
    ProvAttributionSource {
        /// The host saw the change made.
        Host => "host",
        /// Attributed after the fact, by inference.
        Inferred => "inferred",
        /// Taken from an imported record.
        Imported => "imported",
    }
}

text_enum! {
    /// The point at which a change was recorded (`prov_changes.boundary`).
    ProvBoundary {
        /// A worker's checkpoint.
        Checkpoint => "checkpoint",
        /// A worker's submission.
        Submit => "submit",
        /// A landing.
        Land => "land",
        /// A job's end.
        Job => "job",
        /// A backfill of history.
        Backfill => "backfill",
    }
}

text_enum! {
    /// What kind of content an entry is (`prov_entries.class`).
    ProvEntryClass {
        /// Authored content.
        Content => "content",
        /// A mechanical change (formatting, a rename).
        Mechanical => "mechanical",
        /// Generated content.
        Generated => "generated",
        /// A directory tree.
        Tree => "tree",
    }
}

text_enum! {
    /// How a change relates to what an edge names (`prov_edges.kind`).
    ProvEdgeKind {
        /// The change used it.
        Used => "used",
        /// The change includes it.
        Includes => "includes",
        /// The change supersedes it.
        Supersedes => "supersedes",
        /// The change redacts it.
        Redacts => "redacts",
    }
}

/// One change to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvChangeInput {
    pub change_id: String,
    pub rev: i32,
    pub kind: ProvChangeKind,
    pub domain: String,
    pub agent_id: String,
    pub execution_id: Option<String>,
    pub task_id: Option<i64>,
    pub preset_id: Option<String>,
    pub model_label: Option<String>,
    pub transcript_ref: Option<String>,
    pub attributed: ProvAttributionSource,
    /// The tree digest before the change.
    pub base: String,
    /// The tree digest after it.
    pub result: String,
    pub boundary: ProvBoundary,
    pub at_ms: i64,
}

/// One path a change touched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvEntryInput {
    pub path: String,
    /// The content digest before (`None`: the path was absent).
    pub before: Option<String>,
    /// The content digest after (`None`: the change removed it).
    pub after: Option<String>,
    pub size: Option<i64>,
    pub class: ProvEntryClass,
    /// The recorded change (its id) this content came from.
    pub origin: Option<i64>,
}

/// One edge from the change being recorded to what it names.
#[derive(Debug, Clone, PartialEq)]
pub struct ProvEdgeInput {
    pub kind: ProvEdgeKind,
    /// A change's `change_id`, a tree digest or a content digest.
    pub dst: String,
    /// Free-form detail (`{}` when there is none).
    pub detail: Value,
    /// When; `None`: the change's own `at_ms`.
    pub at_ms: Option<i64>,
}

/// A recorded change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvChangeRecord {
    pub id: i64,
    pub change_id: String,
    pub rev: i32,
    pub kind: ProvChangeKind,
    pub domain: String,
    pub agent_id: String,
    pub execution_id: Option<String>,
    pub task_id: Option<i64>,
    pub preset_id: Option<String>,
    pub model_label: Option<String>,
    pub transcript_ref: Option<String>,
    pub attributed: ProvAttributionSource,
    pub base: String,
    pub result: String,
    pub boundary: ProvBoundary,
    pub at_ms: i64,
}

/// A recorded entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvEntryRecord {
    pub change: i64,
    pub path: String,
    pub before: Option<String>,
    pub after: Option<String>,
    pub size: Option<i64>,
    pub class: ProvEntryClass,
    pub origin: Option<i64>,
}

/// A recorded edge.
#[derive(Debug, Clone, PartialEq)]
pub struct ProvEdgeRecord {
    pub id: i64,
    pub kind: ProvEdgeKind,
    pub src: i64,
    pub dst: String,
    pub detail: Value,
    pub at_ms: i64,
}

/// Which way [`PostgresStore::prov_blast_radius`] walks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvDirection {
    /// To the changes that depend on the start: what it affects.
    Forward,
    /// To the changes the start depends on: where it came from.
    Backward,
}

/// How one change was reached from the one before it on its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvLink {
    /// Through an edge of this kind.
    Edge(ProvEdgeKind),
    /// Through an entry's `origin`.
    Origin,
}

/// One change in a blast radius.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvBlastRadiusNode {
    pub id: i64,
    pub change_id: String,
    pub rev: i32,
    pub agent_id: String,
    /// Steps from the start (1: a direct dependency or dependent).
    pub depth: i32,
    /// The ids from the start to this change, both included: one shortest
    /// path (the parent pointers of the walk).
    pub path: Vec<i64>,
    /// The link of the path's last step.
    pub via: ProvLink,
}

/// The columns of `prov_changes`, in [`change_record`]'s order.
const CHANGE_COLUMNS: &str = "c.id, c.change_id, c.rev, c.kind, c.domain, c.agent_id, \
     c.execution_id, c.task_id, c.preset_id, c.model_label, c.transcript_ref, c.attributed, \
     c.base, c.result, c.boundary, c.at_ms";

fn change_record(row: &Row) -> Result<ProvChangeRecord, StoreError> {
    Ok(ProvChangeRecord {
        id: row.get(0),
        change_id: row.get(1),
        rev: row.get(2),
        kind: row.get::<_, String>(3).parse()?,
        domain: row.get(4),
        agent_id: row.get(5),
        execution_id: row.get(6),
        task_id: row.get(7),
        preset_id: row.get(8),
        model_label: row.get(9),
        transcript_ref: row.get(10),
        attributed: row.get::<_, String>(11).parse()?,
        base: row.get(12),
        result: row.get(13),
        boundary: row.get::<_, String>(14).parse()?,
        at_ms: row.get(15),
    })
}

fn change_records(rows: &[Row]) -> Result<Vec<ProvChangeRecord>, StoreError> {
    rows.iter().map(change_record).collect()
}

/// The `p_change` argument of `spawn_prov_record_change`.
fn change_json(change: &ProvChangeInput) -> Value {
    json!({
        "change_id": change.change_id,
        "rev": change.rev,
        "kind": change.kind.as_str(),
        "domain": change.domain,
        "agent_id": change.agent_id,
        "execution_id": change.execution_id,
        "task_id": change.task_id,
        "preset_id": change.preset_id,
        "model_label": change.model_label,
        "transcript_ref": change.transcript_ref,
        "attributed": change.attributed.as_str(),
        "base": change.base,
        "result": change.result,
        "boundary": change.boundary.as_str(),
        "at_ms": change.at_ms,
    })
}

fn entry_json(entry: &ProvEntryInput) -> Value {
    json!({
        "path": entry.path,
        "before": entry.before,
        "after": entry.after,
        "size": entry.size,
        "class": entry.class.as_str(),
        "origin": entry.origin,
    })
}

fn edge_json(edge: &ProvEdgeInput) -> Value {
    let mut object = Map::new();
    object.insert("kind".into(), edge.kind.as_str().into());
    object.insert("dst".into(), edge.dst.clone().into());
    object.insert("detail".into(), edge.detail.clone());
    if let Some(at_ms) = edge.at_ms {
        object.insert("at_ms".into(), at_ms.into());
    }
    Value::Object(object)
}

/// One step of the blast radius walk, forward: from each change of the
/// frontier `$1` to the changes that depend on it, each target once (by its
/// smallest frontier change, then link). A change X depends on a change Y
/// when one of X's edges names Y's `change_id`, Y's `result` or the `after`
/// digest of one of Y's entries, or one of X's entries has Y as its
/// `origin`. Every arm is an index lookup (`prov_edges_dst`,
/// `prov_entries_origin`, the entries' key).
const STEP_FORWARD: &str = "\
with f(id) as (select unnest($1::bigint[])),
links(src, dst, via) as (
    select f.id, e.src, e.kind
      from f join prov_changes w on w.id = f.id join prov_edges e on e.dst = w.change_id
    union all
    select f.id, e.src, e.kind
      from f join prov_changes w on w.id = f.id join prov_edges e on e.dst = w.result
    union all
    select f.id, e.src, e.kind
      from f join prov_entries n on n.change = f.id and n.after is not null
      join prov_edges e on e.dst = n.after
    union all
    select f.id, n.change, 'origin'
      from f join prov_entries n on n.origin = f.id
)
select distinct on (dst) src, dst, via from links order by dst, src, via";

/// [`STEP_FORWARD`] backward: from each change of the frontier `$1` to the
/// changes it depends on (`prov_changes_result`, the `(change_id, rev)`
/// key, `prov_entries_after`, `prov_edges_src`).
const STEP_BACKWARD: &str = "\
with f(id) as (select unnest($1::bigint[])),
links(src, dst, via) as (
    select f.id, c.id, e.kind
      from f join prov_edges e on e.src = f.id join prov_changes c on c.change_id = e.dst
    union all
    select f.id, c.id, e.kind
      from f join prov_edges e on e.src = f.id join prov_changes c on c.result = e.dst
    union all
    select f.id, n.change, e.kind
      from f join prov_edges e on e.src = f.id join prov_entries n on n.after = e.dst
    union all
    select f.id, n.origin, 'origin'
      from f join prov_entries n on n.change = f.id and n.origin is not null
)
select distinct on (dst) src, dst, via from links order by dst, src, via";

/// Where the walk reached a change: its depth, the change it was reached
/// from and the link.
struct Reached {
    depth: i32,
    parent: i64,
    via: String,
}

impl PostgresStore {
    /// Record `change` with its `entries` and `edges`, atomically, through
    /// `spawn_prov_record_change` (owner only), and return its id. The same
    /// `(change_id, rev)` recorded again with the same content returns the
    /// recorded id and writes nothing; with different content it is
    /// refused, and so is anything the schema's checks refuse.
    pub fn record_change(
        &self,
        change: &ProvChangeInput,
        entries: &[ProvEntryInput],
        edges: &[ProvEdgeInput],
    ) -> Result<i64, StoreError> {
        let entries = Value::Array(entries.iter().map(entry_json).collect());
        let edges = Value::Array(edges.iter().map(edge_json).collect());
        Ok(self
            .query_one(
                "select spawn_prov_record_change($1, $2, $3)",
                &[&change_json(change), &entries, &edges],
            )?
            .get(0))
    }

    /// The change recorded as `(change_id, rev)`.
    pub fn prov_change(
        &self,
        change_id: &str,
        rev: i32,
    ) -> Result<Option<ProvChangeRecord>, StoreError> {
        self.query_opt(
            &format!(
                "select {CHANGE_COLUMNS} from prov_changes c where c.change_id = $1 and c.rev = $2"
            ),
            &[&change_id, &rev],
        )?
        .as_ref()
        .map(change_record)
        .transpose()
    }

    /// The change recorded with `id`.
    pub fn prov_change_by_id(&self, id: i64) -> Result<Option<ProvChangeRecord>, StoreError> {
        self.query_opt(
            &format!("select {CHANGE_COLUMNS} from prov_changes c where c.id = $1"),
            &[&id],
        )?
        .as_ref()
        .map(change_record)
        .transpose()
    }

    /// The changes that touched `path`, oldest first.
    pub fn prov_changes_for_path(&self, path: &str) -> Result<Vec<ProvChangeRecord>, StoreError> {
        change_records(&self.query(
            &format!(
                "select {CHANGE_COLUMNS} from prov_changes c
                  where c.id in (select n.change from prov_entries n where n.path = $1)
                  order by c.at_ms, c.id"
            ),
            &[&path],
        )?)
    }

    /// The changes `agent_id` made, oldest first.
    pub fn prov_changes_for_agent(
        &self,
        agent_id: &str,
    ) -> Result<Vec<ProvChangeRecord>, StoreError> {
        change_records(&self.query(
            &format!(
                "select {CHANGE_COLUMNS} from prov_changes c where c.agent_id = $1
                  order by c.at_ms, c.id"
            ),
            &[&agent_id],
        )?)
    }

    /// The changes made in `execution_id`, oldest first.
    pub fn prov_changes_for_execution(
        &self,
        execution_id: &str,
    ) -> Result<Vec<ProvChangeRecord>, StoreError> {
        change_records(&self.query(
            &format!(
                "select {CHANGE_COLUMNS} from prov_changes c where c.execution_id = $1
                  order by c.at_ms, c.id"
            ),
            &[&execution_id],
        )?)
    }

    /// The entries of the change `change`, by path.
    pub fn prov_entries(&self, change: i64) -> Result<Vec<ProvEntryRecord>, StoreError> {
        self.query(
            "select change, path, before, after, size, class, origin
               from prov_entries where change = $1 order by path",
            &[&change],
        )?
        .iter()
        .map(|row| {
            Ok(ProvEntryRecord {
                change: row.get(0),
                path: row.get(1),
                before: row.get(2),
                after: row.get(3),
                size: row.get(4),
                class: row.get::<_, String>(5).parse()?,
                origin: row.get(6),
            })
        })
        .collect()
    }

    /// The edges of the change `change`, in the order they were recorded.
    pub fn prov_edges(&self, change: i64) -> Result<Vec<ProvEdgeRecord>, StoreError> {
        self.query(
            "select id, kind, src, dst, detail, at_ms from prov_edges where src = $1 order by id",
            &[&change],
        )?
        .iter()
        .map(|row| {
            Ok(ProvEdgeRecord {
                id: row.get(0),
                kind: row.get::<_, String>(1).parse()?,
                src: row.get(2),
                dst: row.get(3),
                detail: row.get(4),
                at_ms: row.get(5),
            })
        })
        .collect()
    }

    /// The blast radius of the change `id`: every change that depends on it
    /// ([`ProvDirection::Forward`]) or that it depends on
    /// ([`ProvDirection::Backward`]), at most `max_depth` steps away, nearest
    /// first (then by id). An unknown id has an empty radius.
    ///
    /// A breadth-first walk, one statement per depth, in one read-only
    /// repeatable-read transaction (one snapshot): each change is expanded
    /// at most once, at the depth it is first reached, so the work is
    /// linear in the changes and links reached whatever the graph's shape
    /// (diamonds, cliques, cycles), and that depth is the exact shortest
    /// distance. Each change keeps the change it was first reached from
    /// (the smallest id of the frontier, then the link), and its `path` is
    /// rebuilt from those parent pointers: one shortest path.
    pub fn prov_blast_radius(
        &self,
        id: i64,
        direction: ProvDirection,
        max_depth: u16,
    ) -> Result<Vec<ProvBlastRadiusNode>, StoreError> {
        let step = match direction {
            ProvDirection::Forward => STEP_FORWARD,
            ProvDirection::Backward => STEP_BACKWARD,
        };
        let mut client = self.lock();
        let mut transaction = client
            .build_transaction()
            .isolation_level(postgres::IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .map_err(|error| crate::backend(&error))?;
        let statement = transaction
            .prepare(step)
            .map_err(|error| crate::backend(&error))?;
        let mut reached: HashMap<i64, Reached> = HashMap::new();
        let mut order: Vec<i64> = Vec::new();
        let mut frontier = vec![id];
        let mut depth: i32 = 0;
        while !frontier.is_empty() && depth < i32::from(max_depth) {
            depth += 1;
            let mut next = Vec::new();
            for row in transaction
                .query(&statement, &[&frontier])
                .map_err(|error| crate::backend(&error))?
            {
                let target: i64 = row.get(1);
                if target == id || reached.contains_key(&target) {
                    continue;
                }
                reached.insert(
                    target,
                    Reached {
                        depth,
                        parent: row.get(0),
                        via: row.get(2),
                    },
                );
                order.push(target);
                next.push(target);
            }
            next.sort_unstable();
            frontier = next;
        }
        let rows = transaction
            .query(
                "select id, change_id, rev, agent_id from prov_changes where id = any($1)",
                &[&order],
            )
            .map_err(|error| crate::backend(&error))?;
        transaction
            .commit()
            .map_err(|error| crate::backend(&error))?;
        drop(client);
        let mut nodes = rows
            .iter()
            .map(|row| {
                let node: i64 = row.get(0);
                let at = &reached[&node];
                let mut path = vec![node];
                let mut back = node;
                while back != id {
                    back = reached[&back].parent;
                    path.push(back);
                }
                path.reverse();
                Ok(ProvBlastRadiusNode {
                    id: node,
                    change_id: row.get(1),
                    rev: row.get(2),
                    agent_id: row.get(3),
                    depth: at.depth,
                    path,
                    via: if at.via == "origin" {
                        ProvLink::Origin
                    } else {
                        ProvLink::Edge(at.via.parse()?)
                    },
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        nodes.sort_by_key(|node| (node.depth, node.id));
        Ok(nodes)
    }
}

/// A recorded change as the indexer (`spawn prov index`) reads it back: its
/// identity and its `result` read through the commit map (`git1:` results
/// only; `None`: the history rewrite dropped the commit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvRecorded {
    pub id: i64,
    pub change_id: String,
    pub rev: i32,
    pub kind: ProvChangeKind,
    pub result_now: Option<String>,
}

/// What the event log and the mission layer say of one task, for a change's
/// attribution: its worker, and its preset with the preset's model label as
/// they are now (copied into the change by value).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvTaskFacts {
    pub agent_id: String,
    pub preset_id: Option<String>,
    pub model_label: Option<String>,
}

/// The ids of a `git1:` digest's commit as recorded rows may hold it: the
/// commit itself, the commit the map makes of it, and every old commit the
/// map made into it. Other digests are themselves alone.
const RESULT_NAMES: &str = "\
with given(r) as (select $1::text),
names(r) as (
    select r from given
    union
    select 'git1:' || spawn_commit_now(substr(r, 6)) from given
     where r like 'git1:%' and spawn_commit_now(substr(r, 6)) is not null
    union
    select 'git1:' || m.old from given, spawn_commit_map m
     where r like 'git1:%' and m.new = lower(substr(r, 6))
)";

impl PostgresStore {
    /// The changes whose `result` is `result` (`git1:<commit>` or another
    /// digest), oldest first. A `git1:` commit is read through the commit
    /// map both ways: a change recorded with an old commit the map renamed
    /// answers for its new one, and the old one given answers for a change
    /// recorded with the new one.
    pub fn prov_changes_for_result(
        &self,
        result: &str,
    ) -> Result<Vec<ProvChangeRecord>, StoreError> {
        change_records(&self.query(
            &format!(
                "{RESULT_NAMES}
                 select {CHANGE_COLUMNS} from prov_changes c
                  where c.result in (select r from names)
                  order by c.at_ms, c.id"
            ),
            &[&result],
        )?)
    }

    /// The changes recorded for task `task_id`, oldest first.
    pub fn prov_changes_for_task(&self, task_id: i64) -> Result<Vec<ProvChangeRecord>, StoreError> {
        change_records(&self.query(
            &format!(
                "select {CHANGE_COLUMNS} from prov_changes c where c.task_id = $1
                  order by c.at_ms, c.id"
            ),
            &[&task_id],
        )?)
    }

    /// Every change recorded in `domain`, by id, each `git1:` result read
    /// through `spawn_commit_now`.
    pub fn prov_recorded(&self, domain: &str) -> Result<Vec<ProvRecorded>, StoreError> {
        self.query(
            "select c.id, c.change_id, c.rev, c.kind,
                    case when c.result like 'git1:%'
                         then 'git1:' || spawn_commit_now(substr(c.result, 6))
                         else c.result end
               from prov_changes c where c.domain = $1 order by c.id",
            &[&domain],
        )?
        .iter()
        .map(|row| {
            Ok(ProvRecorded {
                id: row.get(0),
                change_id: row.get(1),
                rev: row.get(2),
                kind: row.get::<_, String>(3).parse()?,
                result_now: row.get(4),
            })
        })
        .collect()
    }

    /// The entries of the changes `ids`, by change and path.
    pub fn prov_entries_of(&self, ids: &[i64]) -> Result<Vec<ProvEntryRecord>, StoreError> {
        self.query(
            "select change, path, before, after, size, class, origin
               from prov_entries where change = any($1) order by change, path",
            &[&ids],
        )?
        .iter()
        .map(|row| {
            Ok(ProvEntryRecord {
                change: row.get(0),
                path: row.get(1),
                before: row.get(2),
                after: row.get(3),
                size: row.get(4),
                class: row.get::<_, String>(5).parse()?,
                origin: row.get(6),
            })
        })
        .collect()
    }

    /// `path`'s history in `domain`: every change with an entry for it, or,
    /// with `folded` (its top-level directory), a `tree` entry of that
    /// directory, each with that entry; oldest first (`at_ms`, then id).
    pub fn prov_path_history(
        &self,
        domain: &str,
        path: &str,
        folded: Option<&str>,
    ) -> Result<Vec<(ProvChangeRecord, ProvEntryRecord)>, StoreError> {
        self.query(
            &format!(
                "select {CHANGE_COLUMNS}, n.path, n.before, n.after, n.size, n.class, n.origin
                   from prov_entries n join prov_changes c on c.id = n.change
                  where c.domain = $1
                    and (n.path = $2 or (n.class = 'tree' and n.path = $3))
                  order by c.at_ms, c.id"
            ),
            &[&domain, &path, &folded],
        )?
        .iter()
        .map(|row| {
            let change = change_record(row)?;
            let entry = ProvEntryRecord {
                change: change.id,
                path: row.get(16),
                before: row.get(17),
                after: row.get(18),
                size: row.get(19),
                class: row.get::<_, String>(20).parse()?,
                origin: row.get(21),
            };
            Ok((change, entry))
        })
        .collect()
    }

    /// The agent of each execution of `ids` the event log holds (its
    /// `execution.started`); an execution it does not hold is absent.
    pub fn prov_execution_agents(
        &self,
        ids: &[String],
    ) -> Result<HashMap<String, String>, StoreError> {
        Ok(self
            .query(
                "select distinct on (execution_id) execution_id, agent_id from spawn_events
                  where event_name = 'execution.started' and execution_id = any($1)
                  order by execution_id, id",
                &[&ids],
            )?
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect())
    }

    /// The facts of each task of `ids` the mission layer holds.
    pub fn prov_task_facts(&self, ids: &[i64]) -> Result<HashMap<i64, ProvTaskFacts>, StoreError> {
        Ok(self
            .query(
                "select t.id, t.agent_id, t.preset_id, p.model_label
                   from mission_tasks t left join grading_presets p on p.preset_id = t.preset_id
                  where t.id = any($1)",
                &[&ids],
            )?
            .iter()
            .map(|row| {
                (
                    row.get(0),
                    ProvTaskFacts {
                        agent_id: row.get(1),
                        preset_id: row.get(2),
                        model_label: row.get(3),
                    },
                )
            })
            .collect())
    }

    /// Every task of the agents `agents`: `(agent, task, created_at_ms)`, by
    /// agent and then creation (then id).
    pub fn prov_agent_tasks(
        &self,
        agents: &[String],
    ) -> Result<Vec<(String, i64, i64)>, StoreError> {
        Ok(self
            .query(
                "select agent_id, id, created_at_ms from mission_tasks
                  where agent_id = any($1) order by agent_id, created_at_ms, id",
                &[&agents],
            )?
            .iter()
            .map(|row| (row.get(0), row.get(1), row.get(2)))
            .collect())
    }
}

/// A data ref's head (PV3b): the newest revision recorded in its domain
/// (`data:<ref>`), with how many revisions the domain holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvHead {
    pub head: ProvChangeRecord,
    pub revisions: i64,
}

impl PostgresStore {
    /// Every change recorded in `domain`, newest first (by revision, then
    /// id): the first is the domain's head.
    pub fn prov_domain_changes(&self, domain: &str) -> Result<Vec<ProvChangeRecord>, StoreError> {
        change_records(&self.query(
            &format!(
                "select {CHANGE_COLUMNS} from prov_changes c where c.domain = $1
                  order by c.rev desc, c.id desc"
            ),
            &[&domain],
        )?)
    }

    /// The head of `domain`: its newest change (by revision, then id), if
    /// it holds any.
    pub fn prov_head(&self, domain: &str) -> Result<Option<ProvChangeRecord>, StoreError> {
        self.query_opt(
            &format!(
                "select {CHANGE_COLUMNS} from prov_changes c where c.domain = $1
                  order by c.rev desc, c.id desc limit 1"
            ),
            &[&domain],
        )?
        .as_ref()
        .map(change_record)
        .transpose()
    }

    /// The head of every domain whose name starts with `prefix` (`data:`:
    /// every data ref), by domain.
    pub fn prov_heads(&self, prefix: &str) -> Result<Vec<ProvHead>, StoreError> {
        self.query(
            &format!(
                "select {CHANGE_COLUMNS}, n.revisions
                   from (select distinct on (domain) id, domain from prov_changes
                          where starts_with(domain, $1)
                          order by domain, rev desc, id desc) h
                   join prov_changes c on c.id = h.id
                   join (select domain, count(*) as revisions from prov_changes
                          where starts_with(domain, $1) group by domain) n
                     on n.domain = c.domain
                  order by c.domain collate \"C\""
            ),
            &[&prefix],
        )?
        .iter()
        .map(|row| {
            Ok(ProvHead {
                head: change_record(row)?,
                revisions: row.get(16),
            })
        })
        .collect()
    }

    /// The changes with an edge to `dst` (a commit `git1:<oid>`, a snapshot
    /// or a content digest), each with that edge's kind, oldest first (one
    /// row per change and kind).
    pub fn prov_changes_using(
        &self,
        dst: &str,
    ) -> Result<Vec<(ProvChangeRecord, ProvEdgeKind)>, StoreError> {
        self.query(
            &format!(
                "select {CHANGE_COLUMNS}, e.kind
                   from (select distinct src, kind from prov_edges where dst = $1) e
                   join prov_changes c on c.id = e.src
                  order by c.at_ms, c.id, e.kind"
            ),
            &[&dst],
        )?
        .iter()
        .map(|row| Ok((change_record(row)?, row.get::<_, String>(16).parse()?)))
        .collect()
    }
}
