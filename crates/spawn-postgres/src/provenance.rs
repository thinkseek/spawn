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
//! it (forward) or that it depends on (backward), however far, through
//! cycles too.

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
    /// path.
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

/// The blast radius walk. `$1` the start's id, `$2` true to walk forward,
/// `$3` the deepest step. A change X depends on a change Y when one of X's
/// edges names Y's `change_id`, Y's `result` or the `after` digest of one of
/// Y's entries, or one of X's entries has Y as its `origin`. Each step
/// extends a path that never holds a change twice (`<> all(path)`), so a
/// cycle ends the path that would close it, and no path is longer than the
/// depth bound: the walk ends on any graph. Each change reached is returned
/// once, by its shortest path (the start itself is not returned).
const BLAST_RADIUS: &str = "\
with recursive walk(id, change_id, result, depth, path, via) as (
    select c.id, c.change_id, c.result, 0, array[c.id], null::text
      from prov_changes c where c.id = $1
    union all
    select nx.id, nx.change_id, nx.result, w.depth + 1, w.path || nx.id, nx.via
      from walk w
     cross join lateral (
        select c.id, c.change_id, c.result, e.kind as via
          from prov_edges e join prov_changes c on c.id = e.src
         where $2 and (e.dst = w.change_id or e.dst = w.result
                       or e.dst in (select n.after from prov_entries n
                                     where n.change = w.id and n.after is not null))
        union all
        select c.id, c.change_id, c.result, 'origin'
          from prov_entries n join prov_changes c on c.id = n.change
         where $2 and n.origin = w.id
        union all
        select c.id, c.change_id, c.result, e.kind
          from prov_edges e join prov_changes c on c.change_id = e.dst
         where not $2 and e.src = w.id
        union all
        select c.id, c.change_id, c.result, e.kind
          from prov_edges e join prov_changes c on c.result = e.dst
         where not $2 and e.src = w.id
        union all
        select c.id, c.change_id, c.result, e.kind
          from prov_edges e join prov_entries n on n.after = e.dst
          join prov_changes c on c.id = n.change
         where not $2 and e.src = w.id
        union all
        select c.id, c.change_id, c.result, 'origin'
          from prov_entries n join prov_changes c on c.id = n.origin
         where not $2 and n.change = w.id
     ) nx
     where w.depth < $3 and nx.id <> all(w.path)
)
select r.id, c.change_id, c.rev, c.agent_id, r.depth, r.path, r.via
  from (select distinct on (w.id) w.id, w.depth, w.path, w.via
          from walk w where w.depth > 0
         order by w.id, w.depth, w.path, w.via) r
  join prov_changes c on c.id = r.id
 order by r.depth, r.id";

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
    /// ([`ProvDirection::Backward`]), at most `max_depth` steps away, each
    /// once by its shortest path, nearest first. Cycles are safe: no path
    /// holds a change twice. An unknown id has an empty radius.
    pub fn prov_blast_radius(
        &self,
        id: i64,
        direction: ProvDirection,
        max_depth: u16,
    ) -> Result<Vec<ProvBlastRadiusNode>, StoreError> {
        let forward = direction == ProvDirection::Forward;
        let depth = i32::from(max_depth);
        self.query(BLAST_RADIUS, &[&id, &forward, &depth])?
            .iter()
            .map(|row| {
                let via: String = row.get(6);
                Ok(ProvBlastRadiusNode {
                    id: row.get(0),
                    change_id: row.get(1),
                    rev: row.get(2),
                    agent_id: row.get(3),
                    depth: row.get(4),
                    path: row.get(5),
                    via: if via == "origin" {
                        ProvLink::Origin
                    } else {
                        ProvLink::Edge(via.parse()?)
                    },
                })
            })
            .collect()
    }
}
