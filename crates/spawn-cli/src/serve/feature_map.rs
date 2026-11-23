//! The feature map: what Spawn's missions say they can do, and how far each
//! capability has actually got.
//!
//! A capability is a stable name (`id`) with one or more of a mission's
//! backlog objectives, and optionally the GitHub issues and pull requests
//! that carry it. The mapping is identities and links only: it never holds
//! a status. Every status is derived when the page reads, from the records
//! that already exist:
//!
//! - **planned**: no `task.landed` and no `objective.solved` record for any
//!   objective the capability names. A merged PR, an accepted task and a
//!   reviewed one are none of them a landing;
//! - **implemented**: a landing (or `objective.solved`: the objective's
//!   scenario passes at the base) exists. This is code on the source, not a
//!   product anybody has looked at;
//! - **verified**: after the newest landing of every landed objective, the
//!   newest `objective.verified` for that objective is `pass` (`spawn
//!   verify`'s rule, the same one `spawn verify list` and `/api/verifications`
//!   read);
//! - **live**: a `deploy.finished` with `ok` true was recorded after the
//!   capability's newest landing, on a mission whose source Spawn's own
//!   deploy ships. A mission no deploy ships (a product deployed outside
//!   Spawn) is never called live: the map says live is not observable
//!   instead of guessing.
//!
//! `state` is only as far as the least advanced objective the capability
//! names: every named objective must have landed before the capability is
//! implemented, and a landed objective without a pass after its newest
//! landing holds the capability at implemented. Each stage is evidence of a
//! different kind, and the four booleans are independent: a capability can
//! be live and never verified (the deploy shipped unverified work), and it
//! is shown as live with its verification evidence beside it. What the map
//! never does is call a landing, a merge or a review live.
//!
//! The mapping is a mission's `objectives.features`, an absolute path (or
//! one relative to the mission's backlog) to a JSON file. A capability
//! names provenance sources, native or external; an `objective` source is
//! what decides its state:
//!
//! ```json
//! {"features": [
//!   {"id": "feature-map", "title": "Feature map",
//!    "summary": "planned, implemented, verified and live capabilities",
//!    "sources": [
//!      {"kind": "objective", "objective": 488},
//!      {"kind": "issue", "ref": "63", "title": "a feature map",
//!       "url": "https://github.com/thinkseek/spawn/issues/63"},
//!      {"kind": "pr", "ref": "69",
//!       "url": "https://github.com/thinkseek/spawn/pull/69"},
//!      {"kind": "mission", "mission": 7},
//!      {"kind": "task", "task": 544},
//!      {"kind": "request", "ref": "owner-2026-11-21"}
//!    ]}
//! ]}
//! ```
//!
//! The native kinds (`objective`, `mission`, `task`, `verification`,
//! `deploy`) link the records Spawn already holds, by a stable reference
//! (`objective:4/488`, `mission:7`, `task:544`, `verification:4/488`,
//! `deploy:1234`); every other kind is an external reference with a stable
//! `ref` and an optional http(s) `url`. The page also adds the native
//! sources that exist whether or not the file names them: each objective,
//! each task a person assigned, each verification and the deploy that made
//! the capability live. Two sources with one reference are one source; an
//! objective belongs to one capability. A source is never a state: external
//! content, a merged PR or a person's request does not change what the
//! records prove.
//!
//! The mapping may also be the project's curated feature registry
//! (`docs/feature-map/registry.json`, `"schema_version": 1`): its nodes'
//! `parent`, `kind`, `files` and `relations` draw the hierarchy, and each
//! node's `provenance` lists become its sources. `"<mission>/<number>"`
//! objectives of this mission are native `objective` sources (a parent and
//! its child may share one); other objective names, non-numeric tasks and
//! deploys, agents, commits, tests, artifacts and discussions are links.
//!
//! Without a mapping, each backlog objective is a capability of its own.
//! A mapping that cannot be read falls back to the objectives and names the
//! error: the page stays useful, and the gap is visible.
//!
//! `GET /api/features[?mission=<id>][&limit=<missions>]` answers the map;
//! the Features view (`#/features`) draws it. Both are read-only.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::{BAD_REQUEST, now_ms, objective_missions, open_store, parameter, task_in_flight};

/// The largest mapping file read. A capability list is identities, not
/// prose: a megabyte is already far more than any mission needs.
const MAX_MAPPING_BYTES: u64 = 1 << 20;

/// Tasks a capability sends as evidence (the newest first).
const MAX_TASKS: usize = 20;
/// Verifications a capability sends as evidence (the newest first).
const MAX_VERIFICATIONS: usize = 20;
/// Gate verdicts a task sends (the newest).
const MAX_GATES: usize = 3;
/// Text sent as it is; longer text is cut (the whole text is not evidence a
/// list needs, and a gate's reason or a review note can be long).
const TEXT_CHARS: usize = 600;

/// `GET /api/features`: the feature map of every open objective mission, or
/// of `?mission=<id>`, at most `?limit=<n>` missions (default 10, most 50).
pub(crate) fn feature_map(query: &str) -> Result<Value, String> {
    let mission = parameter(query, "mission")
        .map(|text| {
            text.parse::<i64>()
                .map_err(|_| format!("{BAD_REQUEST}bad mission {text}"))
        })
        .transpose()?;
    let limit = parameter(query, "limit")
        .map(|text| {
            text.parse::<usize>()
                .map_err(|_| format!("{BAD_REQUEST}bad limit {text}"))
        })
        .transpose()?
        .unwrap_or(10)
        .clamp(1, 50);
    let store = open_store()?;
    read_map(&store, mission, limit)
}

/// The answer from one store: the live anchor comes from the whole deploy
/// log (`live_deploy_fact`), not from the display window `deploy_facts(50)`
/// returns, so a burst of failures never hides the deploy that shipped the
/// source. The reads are separate so a test can drive this wiring with a
/// store the way `feature_map` does.
fn read_map(
    store: &spawn_postgres::PostgresStore,
    mission: Option<i64>,
    limit: usize,
) -> Result<Value, String> {
    let mut missions = objective_missions(store)?;
    if let Some(wanted) = mission {
        missions.retain(|mission| mission["id"].as_i64() == Some(wanted));
    }
    missions.truncate(limit);
    let undeployed = store
        .scorecard_undeployed_missions()
        .map_err(|error| error.to_string())?;
    with_deployed(&mut missions, &undeployed);
    let verifications = store
        .verification_facts()
        .map_err(|error| error.to_string())?;
    let deployments = store.deploy_facts(50).map_err(|error| error.to_string())?;
    let live_anchor = store
        .live_deploy_fact()
        .map_err(|error| error.to_string())?;
    let blockers = store
        .mission_blockers()
        .map_err(|error| error.to_string())?;
    Ok(build(
        &missions,
        &verifications,
        &deployments,
        &live_anchor,
        &blockers,
        now_ms(),
    ))
}

/// One capability as the mapping (or the backlog fallback) names it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Spec {
    id: String,
    title: String,
    summary: Option<String>,
    /// Optional parent feature id defining explicit hierarchy.
    parent: Option<String>,
    /// Capability classification (`domain`, `capability`, `initiative`,
    /// `slice`, `research-initiative`, `subsystem`).
    kind: String,
    /// Optional lowercase tags for grouping and discovery.
    tags: Vec<String>,
    /// Repo-relative normalized POSIX file paths (many-to-many across features).
    files: Vec<String>,
    /// Typed feature-to-feature relations.
    relations: Vec<Relation>,
    /// The native objectives whose records decide the capability's state.
    objectives: Vec<u32>,
    /// Every provenance source the mapping names, native or external.
    sources: Vec<Source>,
}

/// One typed link from a capability to another capability in the same mission.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Relation {
    kind: String,
    target: String,
    label: Option<String>,
}

/// Allowed values for a feature's `kind` field.
const FEATURE_KINDS: &[&str] = &[
    "domain",
    "capability",
    "initiative",
    "slice",
    "research-initiative",
    "subsystem",
];

/// The fields a capability of a plain `{"features": [...]}` mapping takes.
const MAPPING_FEATURE_FIELDS: &[&str] = &[
    "id",
    "title",
    "summary",
    "sources",
    "parent",
    "kind",
    "tags",
    "files",
    "relations",
];

/// The fields a node of a `schema_version: 1` feature registry takes.
const REGISTRY_FEATURE_FIELDS: &[&str] = &[
    "id",
    "title",
    "summary",
    "kind",
    "parent",
    "tags",
    "files",
    "relations",
    "provenance",
    "status_note",
];

/// A registry node's `provenance` lists, in the order their sources are
/// listed, and the source kind each non-native entry becomes.
const PROVENANCE_FIELDS: &[(&str, &str)] = &[
    ("objectives", "external-objective"),
    ("tasks", "external-task"),
    ("agents", "agent"),
    ("commits", "commit"),
    ("tests", "test"),
    ("deploys", "external-deploy"),
    ("artifacts", "artifact"),
    ("discussions", "discussion"),
];

/// The most entries one `provenance` list holds.
const MAX_PROVENANCE_ENTRIES: usize = 500;

/// Allowed values for a relation's `kind` field.
const RELATION_KINDS: &[&str] = &[
    "depends_on",
    "blocks",
    "implements",
    "extends",
    "relates_to",
    "supersedes",
    "verifies",
    "documented_by",
    "contains",
];

/// One place the work came from: a native record Spawn holds (an objective,
/// a mission, a task, a verification, a deploy) or an external reference (an
/// issue, a pull request, a person's request, a document). A source is an
/// identity and a link, never a status: the state still comes from the
/// records the native kinds point at.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Source {
    kind: String,
    /// The stable reference, canonical per kind: `objective:4/488`,
    /// `mission:7`, `task:544`, `verification:4/488`, `deploy:1234`, or
    /// `<kind>:<ref>` for an external source.
    reference: String,
    url: Option<String>,
    title: Option<String>,
    /// The native record this source names, when its kind is one.
    native: Option<Native>,
}

/// The native record behind a source.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Native {
    Objective { mission: i64, objective: u32 },
    Mission(i64),
    Task(i64),
    Verification { mission: i64, objective: u32 },
    Deploy(i64),
}

/// One objective's newest landing evidence.
struct Landing {
    objective: u32,
    task: Option<i64>,
    at_ms: i64,
    kind: &'static str,
    event: Option<i64>,
    commit: Option<String>,
    base: Option<String>,
}

/// The records a map is derived from.
struct Facts<'a> {
    landings: &'a [Value],
    verifications: &'a [Value],
    /// The newest deploys of any outcome, in the display window.
    deployments: &'a [Value],
    /// The newest successful deploy in the whole log, if any: the live
    /// anchor, which the display window does not bound.
    anchor: Option<&'a Value>,
    blockers: &'a [Value],
}

/// The whole answer, from already-read records: pure apart from the
/// mapping files `mission_row` reads.
fn build(
    missions: &[Value],
    verification: &Value,
    deployments: &Value,
    live_anchor: &Value,
    blockers: &Value,
    now: i64,
) -> Value {
    let facts = Facts {
        landings: verification["landings"]
            .as_array()
            .map_or(&[], Vec::as_slice),
        verifications: verification["verifications"]
            .as_array()
            .map_or(&[], Vec::as_slice),
        deployments: deployments.as_array().map_or(&[], Vec::as_slice),
        anchor: live_anchor.as_object().map(|_| live_anchor),
        blockers: blockers.as_array().map_or(&[], Vec::as_slice),
    };
    let rows: Vec<Value> = missions
        .iter()
        .map(|mission| mission_row(mission, &facts))
        .collect();
    json!({
        "generated_at_ms": now,
        // The newest deploy of any outcome (the display window), and the
        // newest successful one over the whole log (the live anchor), so the
        // page can say what it stands on.
        "deploy": facts.deployments.first().cloned().unwrap_or(Value::Null),
        "live_anchor": live_anchor.clone(),
        "missions": rows,
    })
}

/// One mission's capabilities, counts, blocker and mapping.
fn mission_row(mission: &Value, facts: &Facts) -> Value {
    let id = mission["id"].as_i64().unwrap_or_default();
    let (specs, mapping) = specs_for(mission);
    let mut features: Vec<Value> = specs
        .iter()
        .map(|spec| derive(spec, mission, facts))
        .collect();
    // Planned first, then implemented, verified, live: the page's columns.
    features.sort_by(|left, right| {
        state_rank(&left["state"])
            .cmp(&state_rank(&right["state"]))
            .then_with(|| {
                text(&left["title"])
                    .to_lowercase()
                    .cmp(&text(&right["title"]).to_lowercase())
            })
            .then_with(|| text(&left["id"]).cmp(text(&right["id"])))
    });
    let covered: BTreeSet<u32> = specs
        .iter()
        .flat_map(|spec| spec.objectives.iter().copied())
        .collect();
    let unmapped: Vec<Value> = mission["objectives"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|objective| {
            let number = u32::try_from(objective["id"].as_u64()?).ok()?;
            (!covered.contains(&number)).then(|| {
                json!({"objective": number, "title": objective["title"], "status": objective["status"]})
            })
        })
        .collect();
    let blocker = facts
        .blockers
        .iter()
        .find(|blocker| blocker["id"].as_i64() == Some(id))
        .and_then(blocker_of);
    json!({
        "id": id,
        "text": mission["text"],
        "state": mission["state"],
        "halted": mission["halted"],
        "baseline": mission["baseline"],
        "sources": mission["sources"],
        "gate": mission["gate"],
        "accept": mission["accept"],
        "resident": mission["resident"],
        "gate_rejections": mission["gate_rejections"],
        "gate_aborts": mission["gate_aborts"],
        "backlog": mission["backlog"],
        // A backlog that cannot be read leaves the fallback empty: say so
        // rather than render four empty columns as "no capabilities".
        "backlog_error": mission["backlog_error"],
        // The mission is a person's request (origin `human`): the owner's
        // provenance for every capability inside it.
        "source": {"kind": "mission", "ref": format!("mission:{id}"),
                   "url": Value::Null, "title": first_line(text(&mission["text"])),
                   "native": true, "linked": true},
        "mapping": mapping,
        "blocker": blocker,
        "shipped_by_deploy": shipped_by_deploy(mission),
        "counts": counts(&features),
        "graph": graph_summary(&specs),
        "features": features,
        "unmapped": unmapped,
    })
}

/// Mission-level graph summary over the capabilities' hierarchy, typed links,
/// and indexed repo files (highlighting files shared by more than one feature).
fn graph_summary(specs: &[Spec]) -> Value {
    let mut relations_total: usize = 0;
    let mut file_owners: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut hierarchy_roots: Vec<&str> = Vec::new();
    for spec in specs {
        relations_total += spec.relations.len();
        if spec.parent.is_none() {
            hierarchy_roots.push(&spec.id);
        }
        for file in &spec.files {
            file_owners.entry(file.as_str()).or_default().push(&spec.id);
        }
    }
    let files_indexed = file_owners.len();
    let mut shared_files = Map::new();
    for (file, mut owners) in file_owners {
        if owners.len() > 1 {
            owners.sort_unstable();
            owners.dedup();
            shared_files.insert(
                file.to_string(),
                Value::Array(
                    owners
                        .into_iter()
                        .map(|owner| Value::String(owner.to_string()))
                        .collect(),
                ),
            );
        }
    }
    json!({
        "relations_total": relations_total,
        "files_indexed": files_indexed,
        "shared_files": shared_files,
        "hierarchy_roots": hierarchy_roots,
    })
}

/// The source path ending a deploy ships when the deployed missions are
/// not read (a mission row without `shipped_by_deploy`): the one
/// deploy/on-landing.sh deploys, as the store's `UNDEPLOYED_MISSIONS` names
/// it. Anything else (a broader suffix, `.../spawn.git.bak`, a bare
/// `spawn.git`) is another source.
const DEPLOYED_SOURCE_SUFFIX: &str = "/missions/spawn.git";

/// Whether a deploy ships the mission's source. The store's own answer
/// when the row carries it ([`with_deployed`], from the scorecard's
/// `UNDEPLOYED_MISSIONS` predicate, so a mission is never "deployed" here
/// that the scorecard counts as undeployed); else the same predicate over
/// the row's `sources` ([`DEPLOYED_SOURCE_SUFFIX`]).
fn shipped_by_deploy(mission: &Value) -> bool {
    if let Some(known) = mission["shipped_by_deploy"].as_bool() {
        return known;
    }
    mission["sources"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|source| source.ends_with(DEPLOYED_SOURCE_SUFFIX))
}

/// Mark each mission row with `shipped_by_deploy` from the store's
/// undeployed missions (`undeployed`, a JSON array of ids): a mission with
/// at least one source that is not among them is shipped by a deploy; one
/// with no source ships nothing known.
fn with_deployed(missions: &mut [Value], undeployed: &Value) {
    let undeployed: BTreeSet<i64> = undeployed
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_i64)
        .collect();
    for mission in missions {
        let sourced = mission["sources"]
            .as_array()
            .is_some_and(|sources| !sources.is_empty());
        let shipped = sourced
            && mission["id"]
                .as_i64()
                .is_some_and(|id| !undeployed.contains(&id));
        if let Some(fields) = mission.as_object_mut() {
            fields.insert("shipped_by_deploy".to_string(), json!(shipped));
        }
    }
}

/// The mission's capabilities: a readable mapping file, or one capability
/// per backlog objective.
fn specs_for(mission: &Value) -> (Vec<Spec>, Value) {
    let fallback = fallback_specs(mission);
    let Some(given) = mission["features_file"].as_str() else {
        return (
            fallback,
            json!({"kind": "objectives", "path": null, "error": null}),
        );
    };
    let path = match resolve(mission, given) {
        Ok(path) => path,
        Err(error) => {
            return (
                fallback,
                json!({"kind": "objectives", "path": given, "error": error}),
            );
        }
    };
    let mission_id = mission["id"].as_i64().unwrap_or_default();
    let read = read_mapping_value(&path)
        .and_then(|value| Ok((parse_mapping(&value, mission_id)?, value)));
    match read {
        // `registry`: whether the file is a coverage registry the coverage
        // page (`/api/features/coverage?mission=`) can scan, not a legacy
        // identity-only map.
        Ok((specs, value)) => (
            specs,
            json!({"kind": "file", "path": path.display().to_string(), "error": null,
                   "registry": crate::feature_inventory::is_coverage_registry(&value)}),
        ),
        Err(error) => (
            fallback,
            json!({"kind": "objectives", "path": path.display().to_string(),
                   "error": error}),
        ),
    }
}

/// `given` against the mission's backlog: an absolute path as it is, a
/// relative one beside the backlog the mission names.
pub(super) fn resolve(mission: &Value, given: &str) -> Result<PathBuf, String> {
    let path = Path::new(given);
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let backlog = mission["backlog"]
        .as_str()
        .ok_or_else(|| format!("`{given}` is relative and the mission names no backlog"))?;
    Ok(Path::new(backlog).join(path))
}

/// Check a mapping file at configure time (`spawn mission configure
/// --features`), before the mission stores its path. `mission_id` is the
/// mission being configured when the caller knows it, else 0: an
/// `objective` source that names another mission is refused here too.
pub(crate) fn validate_file(path: &Path, mission_id: i64) -> Result<(), String> {
    read_mapping(path, mission_id).map(|_| ())
}

/// The mapping at `path`, checked. `mission_id` is the mission the file
/// belongs to: an objective source may leave its mission out, and may not
/// name another.
fn read_mapping(path: &Path, mission_id: i64) -> Result<Vec<Spec>, String> {
    parse_mapping(&read_mapping_value(path)?, mission_id)
}

/// The JSON of the mapping file at `path`, at most [`MAX_MAPPING_BYTES`].
fn read_mapping_value(path: &Path) -> Result<Value, String> {
    let meta =
        std::fs::metadata(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a file", path.display()));
    }
    if meta.len() > MAX_MAPPING_BYTES {
        return Err(format!(
            "{} is {} bytes; a feature map is at most {MAX_MAPPING_BYTES}",
            path.display(),
            meta.len()
        ));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    serde_json::from_str(&text).map_err(|error| format!("parse {}: {error}", path.display()))
}

/// Check a feature map file for `mission_id`, a mission's own: the
/// capabilities' ids and titles, and each capability's provenance sources.
/// Unknown fields are refused (a typo must not silently drop a source), as
/// are two capabilities with one id and one objective claimed by two
/// capabilities (its records would count twice).
///
/// A root with `"schema_version": 1` is the project's curated
/// `FeatureRegistry` (`docs/feature-map/registry.json`): its nodes carry
/// `provenance` instead of `sources` (see `provenance_of`), and one
/// objective may sit on a parent and its children, since the hierarchy
/// shows the same work at two depths rather than claiming it twice.
fn parse_mapping(value: &Value, mission_id: i64) -> Result<Vec<Spec>, String> {
    let root = value
        .as_object()
        .ok_or("a feature map is a JSON object with a `features` array")?;
    let registry = registry_format(root)?;
    let features = root
        .get("features")
        .and_then(Value::as_array)
        .ok_or("a feature map has a `features` array")?;
    if features.len() > 10_000 {
        return Err(format!(
            "a feature map has at most 10000 features, not {}",
            features.len()
        ));
    }
    let mut specs = Vec::with_capacity(features.len());
    let mut ids = BTreeSet::new();
    let mut claims: BTreeMap<u32, String> = BTreeMap::new();
    for (index, feature) in features.iter().enumerate() {
        let at = format!("features[{index}]");
        let object = feature
            .as_object()
            .ok_or_else(|| format!("{at} is an object, not {feature}"))?;
        let allowed: &[&str] = if registry {
            REGISTRY_FEATURE_FIELDS
        } else {
            MAPPING_FEATURE_FIELDS
        };
        if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
            return Err(format!("{at} has an unknown field `{key}`"));
        }
        let id = required_text(object, "id", &at)?;
        validate_feature_id(&id, &format!("{at}.id"))?;
        if !ids.insert(id.clone()) {
            return Err(format!("two features have the id `{id}`"));
        }
        let title = required_text(object, "title", &at)?;
        if title.chars().count() > 200 {
            return Err(format!("{at}.title is at most 200 characters"));
        }
        let summary =
            optional_text(object, "summary", &at)?.filter(|summary| !summary.trim().is_empty());
        let parent = optional_text(object, "parent", &at)?
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        if let Some(parent_id) = &parent {
            validate_feature_id(parent_id, &format!("{at}.parent"))?;
        }
        let kind = match optional_text(object, "kind", &at)?
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        {
            Some(value) if FEATURE_KINDS.contains(&value.as_str()) => value,
            Some(other) => {
                return Err(format!(
                    "{at}.kind must be one of {}, not `{other}`",
                    FEATURE_KINDS.join(", ")
                ));
            }
            None => "capability".to_string(),
        };
        let tags = tags_of(object, &at)?;
        let files = files_of(object, &at)?;
        let relations = relations_of(object, &at)?;
        if registry {
            optional_text(object, "status_note", &at)?;
        }
        let sources = if registry {
            provenance_of(object, &at, mission_id)?
        } else {
            sources_of(object, &at, mission_id, &id, &mut claims)?
        };
        let objectives: Vec<u32> = sources
            .iter()
            .filter_map(|source| match &source.native {
                Some(Native::Objective { objective, .. }) => Some(*objective),
                _ => None,
            })
            .collect();
        specs.push(Spec {
            id,
            title,
            summary,
            parent,
            kind,
            tags,
            files,
            relations,
            objectives,
            sources,
        });
    }
    validate_graph(&specs, &ids)?;
    Ok(specs)
}

/// Whether `root` is a `schema_version: 1` feature registry (else a plain
/// `{"features": [...]}` map), with only the top-level keys its format takes.
fn registry_format(root: &Map<String, Value>) -> Result<bool, String> {
    let registry = match root.get("schema_version") {
        None => false,
        Some(version) if version.as_u64() == Some(1) => true,
        Some(other) => {
            return Err(format!(
                "a feature registry's `schema_version` is 1, not {other}"
            ));
        }
    };
    if registry {
        if let Some(key) = root.keys().find(|key| {
            !matches!(
                key.as_str(),
                "schema_version" | "initiative_id" | "boundary" | "features"
            )
        }) {
            return Err(format!(
                "a feature registry has only `schema_version`, `initiative_id`, \
                 `boundary` and `features`, not `{key}`"
            ));
        }
    } else if let Some(key) = root.keys().find(|key| key.as_str() != "features") {
        return Err(format!("a feature map has only `features`, not `{key}`"));
    }
    Ok(registry)
}

fn validate_feature_id(id: &str, place: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 80
        || !id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
    {
        return Err(format!(
            "{place} is lowercase letters, digits, - and _ (at most 80), not `{id}`"
        ));
    }
    Ok(())
}

fn tags_of(object: &Map<String, Value>, at: &str) -> Result<Vec<String>, String> {
    let Some(value) = object.get("tags") else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let list = value
        .as_array()
        .ok_or_else(|| format!("{at}.tags is an array"))?;
    if list.len() > 50 {
        return Err(format!("{at}.tags has at most 50 entries"));
    }
    let mut out = Vec::with_capacity(list.len());
    let mut seen = BTreeSet::new();
    for (index, entry) in list.iter().enumerate() {
        let place = format!("{at}.tags[{index}]");
        let raw = entry
            .as_str()
            .ok_or_else(|| format!("{place} is a string, not {entry}"))?;
        let tag = raw.trim();
        if tag.is_empty()
            || tag.len() > 40
            || !tag.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            })
        {
            return Err(format!(
                "{place} is a lowercase tag (letters, digits, - and _, at most 40), not `{raw}`"
            ));
        }
        if seen.insert(tag.to_string()) {
            out.push(tag.to_string());
        }
    }
    Ok(out)
}

fn files_of(object: &Map<String, Value>, at: &str) -> Result<Vec<String>, String> {
    let Some(value) = object.get("files") else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let list = value
        .as_array()
        .ok_or_else(|| format!("{at}.files is an array"))?;
    if list.len() > 500 {
        return Err(format!("{at}.files has at most 500 entries"));
    }
    let mut out = Vec::with_capacity(list.len());
    let mut seen = BTreeSet::new();
    for (index, entry) in list.iter().enumerate() {
        let place = format!("{at}.files[{index}]");
        let raw = entry
            .as_str()
            .ok_or_else(|| format!("{place} is a string, not {entry}"))?;
        let normalized = normalize_repo_path(raw, &place)?;
        if seen.insert(normalized.clone()) {
            out.push(normalized);
        }
    }
    Ok(out)
}

fn normalize_repo_path(raw: &str, place: &str) -> Result<String, String> {
    if raw.is_empty() || raw.len() > 400 {
        return Err(format!(
            "{place} is a non-empty repo-relative path (at most 400 bytes)"
        ));
    }
    if raw.chars().any(char::is_control) {
        return Err(format!("{place} must not contain control characters"));
    }
    if raw.starts_with('/') {
        return Err(format!(
            "{place} must be repo-relative without a leading `/`, not `{raw}`"
        ));
    }
    if raw.contains('\\') {
        return Err(format!(
            "{place} must use POSIX `/` separators, not `{raw}`"
        ));
    }
    let mut parts: Vec<&str> = Vec::new();
    for segment in raw.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            return Err(format!("{place} must not contain `..`, not `{raw}`"));
        }
        parts.push(segment);
    }
    if parts.is_empty() {
        return Err(format!("{place} is an empty path, not `{raw}`"));
    }
    Ok(parts.join("/"))
}

fn relations_of(object: &Map<String, Value>, at: &str) -> Result<Vec<Relation>, String> {
    let Some(value) = object.get("relations") else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let list = value
        .as_array()
        .ok_or_else(|| format!("{at}.relations is an array"))?;
    if list.len() > 200 {
        return Err(format!("{at}.relations has at most 200 entries"));
    }
    let mut out: Vec<Relation> = Vec::with_capacity(list.len());
    for (index, entry) in list.iter().enumerate() {
        let place = format!("{at}.relations[{index}]");
        let obj = entry
            .as_object()
            .ok_or_else(|| format!("{place} is an object"))?;
        if let Some(key) = obj
            .keys()
            .find(|key| !matches!(key.as_str(), "kind" | "target" | "label"))
        {
            return Err(format!("{place} has an unknown field `{key}`"));
        }
        let kind = required_text(obj, "kind", &place)?;
        if !RELATION_KINDS.contains(&kind.as_str()) {
            return Err(format!(
                "{place}.kind must be one of {}, not `{kind}`",
                RELATION_KINDS.join(", ")
            ));
        }
        let target = required_text(obj, "target", &place)?;
        validate_feature_id(&target, &format!("{place}.target"))?;
        let label = optional_text(obj, "label", &place)?
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty());
        if let Some(label_text) = &label
            && (label_text.chars().count() > 120 || label_text.chars().any(char::is_control))
        {
            return Err(format!(
                "{place}.label is at most 120 characters without control characters"
            ));
        }
        if let Some(existing) = out
            .iter_mut()
            .find(|existing| existing.kind == kind && existing.target == target)
        {
            if existing.label.is_none() {
                existing.label = label;
            }
            continue;
        }
        out.push(Relation {
            kind,
            target,
            label,
        });
    }
    Ok(out)
}

/// Cross-feature structural validation:
/// - Refuse self-links (`parent == id` or `relation.target == id`).
/// - Refuse dangling `parent` and `relation.target` references.
/// - Refuse cycles in `parent`, `depends_on`, and `contains` edges.
fn validate_graph(specs: &[Spec], ids: &BTreeSet<String>) -> Result<(), String> {
    let mut parent_edges: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut depends_edges: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut contains_edges: BTreeMap<&str, Vec<&str>> = BTreeMap::new();

    for spec in specs {
        if let Some(parent) = &spec.parent {
            if parent == &spec.id {
                return Err(format!("feature `{}` cannot be its own parent", spec.id));
            }
            if !ids.contains(parent) {
                return Err(format!(
                    "feature `{}` names unknown parent `{parent}`",
                    spec.id
                ));
            }
            parent_edges
                .entry(spec.id.as_str())
                .or_default()
                .push(parent.as_str());
        }
        for relation in &spec.relations {
            if relation.target == spec.id {
                return Err(format!(
                    "feature `{}` cannot link `{}` to itself",
                    spec.id, relation.kind
                ));
            }
            if !ids.contains(&relation.target) {
                return Err(format!(
                    "feature `{}` relation `{}` names unknown target `{}`",
                    spec.id, relation.kind, relation.target
                ));
            }
            match relation.kind.as_str() {
                "depends_on" => depends_edges
                    .entry(spec.id.as_str())
                    .or_default()
                    .push(relation.target.as_str()),
                "contains" => contains_edges
                    .entry(spec.id.as_str())
                    .or_default()
                    .push(relation.target.as_str()),
                _ => {}
            }
        }
    }

    detect_cycle(&parent_edges, "parent")?;
    detect_cycle(&depends_edges, "depends_on")?;
    detect_cycle(&contains_edges, "contains")?;
    Ok(())
}

/// Three-color DFS cycle detector over directed feature edges.
fn detect_cycle(edges: &BTreeMap<&str, Vec<&str>>, edge_kind: &str) -> Result<(), String> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mark {
        Visiting,
        Visited,
    }

    fn visit<'a>(
        node: &'a str,
        edges: &BTreeMap<&'a str, Vec<&'a str>>,
        marks: &mut BTreeMap<&'a str, Mark>,
        edge_kind: &str,
    ) -> Result<(), String> {
        marks.insert(node, Mark::Visiting);
        if let Some(neighbors) = edges.get(node) {
            for &next in neighbors {
                match marks.get(next) {
                    Some(Mark::Visiting) => {
                        return Err(format!(
                            "`{edge_kind}` cycle detected between `{node}` and `{next}`"
                        ));
                    }
                    Some(Mark::Visited) => {}
                    None => visit(next, edges, marks, edge_kind)?,
                }
            }
        }
        marks.insert(node, Mark::Visited);
        Ok(())
    }

    let mut marks: BTreeMap<&str, Mark> = BTreeMap::new();
    for &node in edges.keys() {
        if !marks.contains_key(node) {
            visit(node, edges, &mut marks, edge_kind)?;
        }
    }
    Ok(())
}

/// A capability's `sources`: identities and links, native or external.
/// Two sources with one canonical reference are one source (the first
/// title and URL win, the missing one is filled); an objective may belong
/// to one capability in a mission.
fn sources_of(
    object: &Map<String, Value>,
    at: &str,
    mission_id: i64,
    feature: &str,
    claims: &mut BTreeMap<u32, String>,
) -> Result<Vec<Source>, String> {
    let Some(value) = object.get("sources") else {
        return Ok(Vec::new());
    };
    let list = value
        .as_array()
        .ok_or_else(|| format!("{at}.sources is an array"))?;
    let mut sources: Vec<Source> = Vec::with_capacity(list.len());
    for (index, entry) in list.iter().enumerate() {
        let place = format!("{at}.sources[{index}]");
        let source = parse_source(entry, &place, mission_id)?;
        // The same objective twice in one capability is one source (it is
        // deduplicated below); two capabilities claiming it would count its
        // records twice, and that is refused.
        if let Some(Native::Objective { objective, .. }) = &source.native {
            if let Some(other) = claims.get(objective)
                && other != feature
            {
                return Err(format!(
                    "objective {objective} is in both `{other}` and `{feature}`: give it to one"
                ));
            }
            claims.insert(*objective, feature.to_string());
        }
        if let Some(seen) = sources
            .iter_mut()
            .find(|seen| seen.reference == source.reference)
        {
            if seen.url.is_none() {
                seen.url = source.url;
            }
            if seen.title.is_none() {
                seen.title = source.title;
            }
            continue;
        }
        sources.push(source);
    }
    Ok(sources)
}

/// A registry node's `provenance`, as sources. Every list is strings:
///
/// - `objectives`: `"<mission>/<number>"` naming this map's mission (or any
///   mission while it is not known yet, 0) is a native `objective` source,
///   the one that decides the state; anything else (another mission, an
///   initiative id such as `t559u`) is an `external-objective` link;
/// - `tasks`: a whole number is a native `task`, any other name an
///   `external-task` link;
/// - `deploys`: a whole number is a native `deploy` event, any other name an
///   `external-deploy` link;
/// - `agents`, `commits`, `tests`, `artifacts` and `discussions` are
///   external `agent`, `commit`, `test`, `artifact` and `discussion` links.
///
/// One reference twice in a node is one source; unknown lists are refused.
fn provenance_of(
    object: &Map<String, Value>,
    at: &str,
    mission_id: i64,
) -> Result<Vec<Source>, String> {
    let Some(value) = object.get("provenance") else {
        return Ok(Vec::new());
    };
    if value.is_null() {
        return Ok(Vec::new());
    }
    let place = format!("{at}.provenance");
    let provenance = value
        .as_object()
        .ok_or_else(|| format!("{place} is an object"))?;
    if let Some(key) = provenance
        .keys()
        .find(|key| !PROVENANCE_FIELDS.iter().any(|(name, _)| name == key))
    {
        return Err(format!("{place} has an unknown field `{key}`"));
    }
    let mut sources: Vec<Source> = Vec::new();
    for (field, external) in PROVENANCE_FIELDS {
        let Some(list) = provenance.get(*field) else {
            continue;
        };
        if list.is_null() {
            continue;
        }
        let list = list
            .as_array()
            .ok_or_else(|| format!("{place}.{field} is an array of strings"))?;
        if list.len() > MAX_PROVENANCE_ENTRIES {
            return Err(format!(
                "{place}.{field} has at most {MAX_PROVENANCE_ENTRIES} entries"
            ));
        }
        for (index, entry) in list.iter().enumerate() {
            let at = format!("{place}.{field}[{index}]");
            let raw = entry
                .as_str()
                .map(str::trim)
                .filter(|raw| !raw.is_empty())
                .ok_or_else(|| format!("{at} is a non-empty string, not {entry}"))?;
            if raw.chars().count() > 300 || raw.chars().any(char::is_control) {
                return Err(format!(
                    "{at} is at most 300 characters, without control characters"
                ));
            }
            let source = provenance_source(field, external, raw, mission_id);
            if !sources
                .iter()
                .any(|seen| seen.reference == source.reference)
            {
                sources.push(source);
            }
        }
    }
    Ok(sources)
}

/// One `provenance` entry as a source: native when it names a record this
/// map's mission holds, an external link otherwise.
fn provenance_source(field: &str, external: &str, raw: &str, mission_id: i64) -> Source {
    let whole = |text: &str| -> Option<i64> {
        if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        text.parse::<i64>().ok().filter(|number| *number >= 1)
    };
    let native = match field {
        "objectives" => raw.split_once('/').and_then(|(mission, objective)| {
            let mission = whole(mission)?;
            let objective = u32::try_from(whole(objective)?).ok()?;
            (mission_id <= 0 || mission == mission_id)
                .then_some(Native::Objective { mission, objective })
        }),
        "tasks" => whole(raw).map(Native::Task),
        "deploys" => whole(raw).map(Native::Deploy),
        _ => None,
    };
    let (kind, reference) = match &native {
        Some(Native::Objective { mission, objective }) => (
            "objective".to_string(),
            format!("objective:{mission}/{objective:03}"),
        ),
        Some(Native::Task(task)) => ("task".to_string(), format!("task:{task}")),
        Some(Native::Deploy(event)) => ("deploy".to_string(), format!("deploy:{event}")),
        _ => (external.to_string(), format!("{external}:{raw}")),
    };
    Source {
        kind,
        reference,
        url: None,
        title: None,
        native,
    }
}

/// One source: `kind` and the fields its kind takes. Native kinds name the
/// record they link (`objective`, `mission`, `task`, `verification`,
/// `deploy`); any other kind is an external reference with a stable `ref`
/// and an optional http(s) `url`. External or source content is data, never
/// authorization.
#[allow(clippy::too_many_lines)] // one field table per source kind
fn parse_source(entry: &Value, place: &str, mission_id: i64) -> Result<Source, String> {
    let object = entry
        .as_object()
        .ok_or_else(|| format!("{place} is an object with a `kind`"))?;
    let kind = required_text(object, "kind", place)?;
    if kind.len() > 40
        || !kind
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        || !kind.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
    {
        return Err(format!(
            "{place}.kind is a lowercase word (letters, digits, - and _), not `{kind}`"
        ));
    }
    let title = optional_text(object, "title", place)?.filter(|title| !title.trim().is_empty());
    let refuse = |allowed: &[&str]| -> Result<(), String> {
        if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
            return Err(format!("{place} has an unknown field `{key}` for `{kind}`"));
        }
        Ok(())
    };
    match kind.as_str() {
        "objective" | "verification" => {
            refuse(&["kind", "mission", "objective", "title"])?;
            let mission = optional_positive(object, "mission", place)?.unwrap_or(mission_id);
            // 0 means the caller does not know the mission yet (`mission
            // add` validates before the mission has an id): any mission the
            // source names is then the one it will belong to.
            if mission_id > 0 && mission != mission_id {
                return Err(format!(
                    "{place} names mission {mission}, not this map's mission {mission_id}"
                ));
            }
            let objective = required_positive(object, "objective", place)?;
            let objective = u32::try_from(objective)
                .map_err(|_| format!("{place}.objective is a whole number from 1"))?;
            let native = if kind == "objective" {
                Native::Objective { mission, objective }
            } else {
                Native::Verification { mission, objective }
            };
            Ok(Source {
                reference: format!("{kind}:{mission}/{objective:03}"),
                kind,
                url: None,
                title,
                native: Some(native),
            })
        }
        "mission" => {
            refuse(&["kind", "mission", "title"])?;
            let mission = required_positive(object, "mission", place)?;
            Ok(Source {
                reference: format!("mission:{mission}"),
                kind,
                url: None,
                title,
                native: Some(Native::Mission(mission)),
            })
        }
        "task" => {
            refuse(&["kind", "task", "title"])?;
            let task = required_positive(object, "task", place)?;
            Ok(Source {
                reference: format!("task:{task}"),
                kind,
                url: None,
                title,
                native: Some(Native::Task(task)),
            })
        }
        "deploy" => {
            refuse(&["kind", "event", "title"])?;
            let event = required_positive(object, "event", place)?;
            Ok(Source {
                reference: format!("deploy:{event}"),
                kind,
                url: None,
                title,
                native: Some(Native::Deploy(event)),
            })
        }
        _ => {
            refuse(&["kind", "ref", "url", "title"])?;
            let reference = required_text(object, "ref", place)?;
            if reference.chars().count() > 300 || reference.chars().any(char::is_control) {
                return Err(format!(
                    "{place}.ref is at most 300 characters, without control characters"
                ));
            }
            let url = optional_text(object, "url", place)?.filter(|url| !url.trim().is_empty());
            if let Some(url) = &url
                && (url.len() > 2000
                    || url.chars().any(char::is_whitespace)
                    || !(url.starts_with("https://") || url.starts_with("http://")))
            {
                return Err(format!("{place}.url is an http(s) URL, not `{url}`"));
            }
            Ok(Source {
                reference: format!("{kind}:{reference}"),
                kind,
                url,
                title,
                native: None,
            })
        }
    }
}

fn required_positive(object: &Map<String, Value>, name: &str, at: &str) -> Result<i64, String> {
    object
        .get(name)
        .and_then(Value::as_i64)
        .filter(|number| *number >= 1)
        .ok_or_else(|| format!("{at}.{name} is a whole number from 1"))
}

fn optional_positive(
    object: &Map<String, Value>,
    name: &str,
    at: &str,
) -> Result<Option<i64>, String> {
    match object.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_i64()
            .filter(|number| *number >= 1)
            .map(Some)
            .ok_or_else(|| format!("{at}.{name} is a whole number from 1, not {value}")),
    }
}

fn required_text(object: &Map<String, Value>, name: &str, at: &str) -> Result<String, String> {
    match object.get(name) {
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(text.trim().to_string()),
        Some(other) => Err(format!("{at}.{name} is a non-empty string, not {other}")),
        None => Err(format!("{at} has no `{name}`")),
    }
}

fn optional_text(
    object: &Map<String, Value>,
    name: &str,
    at: &str,
) -> Result<Option<String>, String> {
    match object.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(other) => Err(format!("{at}.{name} is a string, not {other}")),
    }
}

/// The capability of every objective when the mission names no mapping:
/// each backlog objective is its own capability, with its native source.
fn fallback_specs(mission: &Value) -> Vec<Spec> {
    let mission_id = mission["id"].as_i64().unwrap_or_default();
    mission["objectives"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|objective| {
            let number = u32::try_from(objective["id"].as_u64()?).ok()?;
            let title = objective["title"]
                .as_str()
                .filter(|title| !title.trim().is_empty())
                .map_or_else(|| format!("Objective {number}"), str::to_string);
            Some(Spec {
                id: format!("objective-{number:03}"),
                title: title.clone(),
                summary: None,
                parent: None,
                kind: "capability".to_string(),
                tags: Vec::new(),
                files: Vec::new(),
                relations: Vec::new(),
                objectives: vec![number],
                sources: vec![Source {
                    kind: "objective".to_string(),
                    reference: format!("objective:{mission_id}/{number:03}"),
                    url: None,
                    title: Some(title),
                    native: Some(Native::Objective {
                        mission: mission_id,
                        objective: number,
                    }),
                }],
            })
        })
        .collect()
}

/// One capability's row: its four-state verdict, evidence, owner, gaps and
/// blocker.
#[allow(clippy::too_many_lines)] // the row fields, each read from the facts above
fn derive(spec: &Spec, mission: &Value, facts: &Facts) -> Value {
    let mission_id = mission["id"].as_i64().unwrap_or_default();
    let gated = !mission["gate"].is_null();
    let rows: Vec<&Value> = mission["objectives"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|objective| named(spec, &objective["id"]))
        .collect();
    let present: BTreeSet<u32> = rows
        .iter()
        .filter_map(|objective| u32::try_from(objective["id"].as_u64()?).ok())
        .collect();
    let missing: Vec<u32> = spec
        .objectives
        .iter()
        .copied()
        .filter(|number| !present.contains(number))
        .collect();
    let tasks: Vec<&Value> = mission["tasks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|task| named(spec, &task["objective"]))
        .collect();
    let landed = landings_of(spec, mission, facts);
    let newest_landing = landed.values().max_by_key(|landing| landing.at_ms);
    let checks = checks_of(&landed, mission_id, facts);
    // The lowest state among the capability's objectives: a capability is
    // only implemented when every objective it names has landed, so a live
    // sibling never lifts a never-landed one. Verified and live inherit
    // this, and a landed objective without a pass holds the capability at
    // implemented.
    let implemented = !spec.objectives.is_empty()
        && spec
            .objectives
            .iter()
            .all(|objective| landed.contains_key(objective));
    let verified = implemented && checks.iter().all(|check| check["result"] == "pass");
    let unverified: Vec<u32> = checks
        .iter()
        .filter(|check| check["result"] != "pass")
        .filter_map(|check| u32::try_from(check["objective"].as_u64()?).ok())
        .collect();
    let anchor = facts.anchor;
    let shipped = shipped_by_deploy(mission);
    let live = implemented
        && shipped
        && anchor.is_some_and(|anchor| {
            anchor["at_ms"].as_i64().unwrap_or(i64::MAX)
                > newest_landing.map_or(i64::MIN, |landing| landing.at_ms)
        });
    let state = if live {
        "live"
    } else if verified {
        "verified"
    } else if implemented {
        "implemented"
    } else {
        "planned"
    };
    let newest_in_flight = tasks
        .iter()
        .copied()
        .filter(|task| task_in_flight(task, gated))
        .max_by_key(|task| task_key(task));
    let newest_task = tasks.iter().copied().max_by_key(|task| task_key(task));
    let blocker = blocked_of(
        state,
        mission_id,
        gated,
        newest_in_flight,
        newest_task,
        facts,
    );
    let mut gaps = landing_gaps(spec, &present, &landed, &unverified, &checks);
    gaps.extend(delivery_gaps(
        state,
        &landed,
        anchor,
        shipped,
        tasks.is_empty(),
    ));
    gaps.extend(accepted_gaps(spec, mission));
    let complete = live && verified && missing.is_empty() && landed.len() == spec.objectives.len();
    json!({
        "id": spec.id,
        "title": spec.title,
        "summary": spec.summary,
        "parent": spec.parent,
        "kind": spec.kind,
        "tags": spec.tags,
        "files": spec.files,
        "relations": spec.relations.iter().map(|rel| json!({
            "kind": rel.kind,
            "target": rel.target,
            "label": rel.label,
        })).collect::<Vec<_>>(),
        "state": state,
        // Four separate pieces of evidence, not synonyms: a live
        // capability can be unverified, and says so.
        "planned": !implemented,
        "implemented": implemented,
        "verified": verified,
        "live": live,
        "complete": complete,
        "objectives": spec.objectives,
        "objective_rows": rows.iter().map(|row| json!({
            "id": row["id"], "title": row["title"], "status": row["status"]})).collect::<Vec<_>>(),
        "sources": sources_json(spec, mission, &rows, &tasks, &checks,
                                 live.then_some(anchor).flatten(), facts),
        "owner": newest_in_flight.map_or(Value::Null, owner_json),
        "last_owner": newest_task.map_or(Value::Null, owner_json),
        "unowned": state != "live" && newest_in_flight.is_none(),
        "blocked": blocker,
        "gaps": gaps,
        "evidence": evidence_of(
            &landed, &checks, newest_landing, anchor, facts.deployments.first(), &tasks, live,
        ),
    })
}

fn named(spec: &Spec, value: &Value) -> bool {
    value
        .as_u64()
        .and_then(|number| u32::try_from(number).ok())
        .is_some_and(|number| spec.objectives.contains(&number))
}

fn task_key(task: &Value) -> (i64, i64) {
    (
        task["updated_at_ms"].as_i64().unwrap_or(i64::MIN),
        task["id"].as_i64().unwrap_or(i64::MIN),
    )
}

fn owner_json(task: &Value) -> Value {
    json!({
        "task": task["id"],
        "agent_id": task["agent_id"],
        "session_id": task["session_id"],
        "kind": task["kind"],
        "state": task["state"],
        "since_ms": task["updated_at_ms"],
    })
}

/// Each named objective's newest landing, one per objective: the
/// `task.landed` record (`verification_facts`' landings), an
/// `objective.solved` on the base, or the task row's landed history, which
/// is what the page shows when the new read is older than the row.
fn landings_of(spec: &Spec, mission: &Value, facts: &Facts) -> BTreeMap<u32, Landing> {
    let mission_id = mission["id"].as_i64().unwrap_or_default();
    let mut landed: BTreeMap<u32, Landing> = BTreeMap::new();
    let task_of = |id: i64| {
        mission["tasks"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|task| task["id"].as_i64() == Some(id))
    };
    let evidence = |objective: u32, task: Option<i64>, at_ms: i64, kind: &'static str| {
        let row = task.and_then(task_of);
        Landing {
            objective,
            task,
            at_ms,
            kind,
            event: None,
            commit: row
                .and_then(|row| row["candidate"].as_str())
                .map(str::to_string),
            base: row.and_then(|row| row["base"].as_str()).map(str::to_string),
        }
    };
    for number in &spec.objectives {
        if let Some(record) = facts.landings.iter().find(|record| {
            record["mission"].as_i64() == Some(mission_id)
                && record["objective"].as_u64() == Some(u64::from(*number))
        }) {
            let mut landing = evidence(
                *number,
                record["task"].as_i64(),
                record["landed_ms"].as_i64().unwrap_or(i64::MIN),
                "landed",
            );
            landing.event = record["event"].as_i64();
            landed.insert(*number, landing);
        }
    }
    for row in mission["objectives"].as_array().into_iter().flatten() {
        let Some(number) = row["id"].as_u64().and_then(|id| u32::try_from(id).ok()) else {
            continue;
        };
        if !spec.objectives.contains(&number) {
            continue;
        }
        if let Some(solved) = row["solved_on_base"].as_object() {
            let at_ms = solved["at_ms"].as_i64().unwrap_or(i64::MIN);
            if landed
                .get(&number)
                .is_none_or(|landing| at_ms > landing.at_ms)
            {
                landed.insert(
                    number,
                    Landing {
                        objective: number,
                        task: solved["task"].as_i64(),
                        at_ms,
                        kind: "solved_on_base",
                        event: solved["event"].as_i64(),
                        commit: solved["base"].as_str().map(str::to_string),
                        base: solved["base"].as_str().map(str::to_string),
                    },
                );
            }
        }
        if let Some(history) = row["landings"].as_array() {
            for entry in history {
                let at_ms = entry["at_ms"].as_i64().unwrap_or(i64::MIN);
                if landed
                    .get(&number)
                    .is_none_or(|landing| at_ms > landing.at_ms)
                {
                    landed.insert(
                        number,
                        evidence(number, entry["task"].as_i64(), at_ms, "landed"),
                    );
                }
            }
        }
    }
    landed
}

/// The newest verification after each landed objective's landing, one per
/// objective: `[{objective, result, by, how, at_ms, event}]`. A landed
/// objective with none gets a `null` result, so "not verified" and "failed"
/// stay different.
fn checks_of(landed: &BTreeMap<u32, Landing>, mission_id: i64, facts: &Facts) -> Vec<Value> {
    landed
        .values()
        .map(|landing| {
            let newest = facts
                .verifications
                .iter()
                .filter(|seen| {
                    seen["mission"].as_i64() == Some(mission_id)
                        && seen["objective"].as_u64() == Some(u64::from(landing.objective))
                        && seen["at_ms"].as_i64().unwrap_or(i64::MIN) > landing.at_ms
                })
                .max_by_key(|seen| {
                    (
                        seen["at_ms"].as_i64().unwrap_or(i64::MIN),
                        seen["event"].as_i64().unwrap_or(i64::MIN),
                    )
                });
            match newest {
                Some(seen) => json!({
                    "objective": landing.objective,
                    "result": seen["result"],
                    "by": seen["by"],
                    "how": cut(&seen["how"]),
                    "at_ms": seen["at_ms"],
                    "event": seen["event"],
                }),
                None => json!({
                    "objective": landing.objective,
                    "result": "none",
                    "by": Value::Null,
                    "how": Value::Null,
                    "at_ms": Value::Null,
                    "event": Value::Null,
                }),
            }
        })
        .collect()
}

/// The gaps between a capability and its objectives: one it names but the
/// backlog does not hold, one never landed, one landed but never verified,
/// one whose verification failed. No percentages: a gap names what is
/// missing.
fn landing_gaps(
    spec: &Spec,
    present: &BTreeSet<u32>,
    landed: &BTreeMap<u32, Landing>,
    unverified: &[u32],
    checks: &[Value],
) -> Vec<Value> {
    let mut gaps = Vec::new();
    if spec.objectives.is_empty() {
        gaps.push(json!({"kind": "no_objective",
                         "detail": "the capability names no objective"}));
    }
    let missing: Vec<u32> = spec
        .objectives
        .iter()
        .copied()
        .filter(|number| !present.contains(number))
        .collect();
    if !missing.is_empty() {
        gaps.push(json!({"kind": "objective_missing", "objectives": missing,
                         "detail": "named by the mapping, absent from the mission's backlog"}));
    }
    let unlanded: Vec<u32> = present
        .iter()
        .copied()
        .filter(|number| !landed.contains_key(number))
        .collect();
    if !unlanded.is_empty() {
        gaps.push(json!({"kind": "not_landed", "objectives": unlanded}));
    }
    let failed: Vec<u32> = checks
        .iter()
        .filter(|check| check["result"] == "fail")
        .filter_map(|check| u32::try_from(check["objective"].as_u64()?).ok())
        .collect();
    if !failed.is_empty() {
        gaps.push(json!({"kind": "verification_failed", "objectives": failed}));
    }
    let open: Vec<u32> = unverified
        .iter()
        .copied()
        .filter(|number| !failed.contains(number))
        .collect();
    if !open.is_empty() {
        gaps.push(json!({"kind": "unverified", "objectives": open}));
    }
    gaps
}

/// The gaps between a landing and a running product: a mission Spawn's own
/// deploy does not ship (live is not observable), or one it does and the
/// landing is not in a successful deploy yet.
fn delivery_gaps(
    state: &str,
    landed: &BTreeMap<u32, Landing>,
    anchor: Option<&Value>,
    shipped: bool,
    no_task: bool,
) -> Vec<Value> {
    let mut gaps = Vec::new();
    if state == "planned" && no_task {
        gaps.push(json!({"kind": "no_task",
                         "detail": "no task has ever been assigned for it"}));
    }
    if state == "planned" {
        return gaps;
    }
    if !shipped {
        gaps.push(json!({"kind": "live_not_observable",
                         "detail": "no deploy ships this mission's source"}));
        return gaps;
    }
    let Some(anchor) = anchor else {
        gaps.push(json!({"kind": "no_deploy_record",
                         "detail": "no deploy.finished has been recorded"}));
        return gaps;
    };
    let at = anchor["at_ms"].as_i64().unwrap_or(i64::MAX);
    let waiting: Vec<u32> = landed
        .values()
        .filter(|landing| landing.at_ms >= at)
        .map(|landing| landing.objective)
        .collect();
    if !waiting.is_empty() {
        gaps.push(json!({"kind": "not_deployed", "objectives": waiting,
                         "detail": "landed after the newest successful deploy"}));
    }
    gaps
}

/// An accepted task of a mission without a gate never lands: its work is
/// closed, and the map says so rather than counting it implemented.
fn accepted_gaps(spec: &Spec, mission: &Value) -> Vec<Value> {
    if !mission["gate"].is_null() {
        return Vec::new();
    }
    let accepted = mission["tasks"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|task| unmatched_accepted(spec, task));
    if accepted {
        vec![json!({"kind": "accepted_without_landing",
                    "detail": "the mission has no gate: an accepted task never lands"})]
    } else {
        Vec::new()
    }
}

/// An accepted task of a mission without a gate: its work is closed, not
/// landed, and the map says so rather than counting it implemented.
fn unmatched_accepted(spec: &Spec, task: &Value) -> bool {
    named(spec, &task["objective"]) && task["state"] == "accepted"
}

/// What holds a capability back, if anything: the mission's own blocker
/// first, then the newest task's last outcome when nothing is in flight.
fn blocked_of(
    state: &str,
    mission_id: i64,
    gated: bool,
    newest_in_flight: Option<&Value>,
    newest_task: Option<&Value>,
    facts: &Facts,
) -> Value {
    if state == "live" {
        return Value::Null;
    }
    if let Some(blocker) = facts
        .blockers
        .iter()
        .find(|blocker| blocker["id"].as_i64() == Some(mission_id))
        .and_then(blocker_of)
    {
        return blocker;
    }
    if newest_in_flight.is_some() {
        return Value::Null;
    }
    let Some(task) = newest_task else {
        return Value::Null;
    };
    let kind = match task["state"].as_str() {
        Some("cancelled") => "cancelled",
        Some("accepted") if !gated => "accepted_without_landing",
        _ => return Value::Null,
    };
    let reason = task["review_note"]
        .as_str()
        .map(str::trim)
        .filter(|note| !note.is_empty())
        .map(str::to_string)
        .or_else(|| {
            task["gates"].as_array().and_then(|gates| {
                gates
                    .last()
                    .and_then(|gate| gate["reason"].as_str().map(str::trim).map(str::to_string))
            })
        });
    json!({
        "kind": kind,
        "task": task["id"],
        "reason": reason.map_or(Value::Null, Value::String),
        "at_ms": task["updated_at_ms"],
        "agent_id": task["agent_id"],
        "session_id": task["session_id"],
    })
}

/// The mission's blocker as one object, or null.
fn blocker_of(blocker: &Value) -> Option<Value> {
    if blocker["halt"].is_object() {
        return Some(json!({
            "kind": "mission_halted",
            "reason": blocker["halt"]["reason"],
            "event": blocker["halt"]["event"],
            "at_ms": blocker["halt"]["at_ms"],
        }));
    }
    if blocker["paused"].is_object() {
        return Some(json!({
            "kind": "mission_paused",
            "reason": blocker["paused"]["reason"],
            "event": blocker["paused"]["event"],
            "at_ms": blocker["paused"]["at_ms"],
        }));
    }
    if blocker["main_red"].is_object() {
        return Some(json!({
            "kind": "main_red",
            "scenario": blocker["main_red"]["scenario"],
            "since_ms": blocker["main_red"]["since_ms"],
            "red_ms": blocker["main_red"]["red_ms"],
        }));
    }
    None
}

/// What the page shows as proof: the tasks, the landings, every decisive
/// verification, and the deploy the live claim stands on. `deploy` is the
/// anchor only when the capability is actually live: a mission no deploy
/// ships keeps the newest deploy record as `last_deploy`, never as proof.
fn evidence_of(
    landed: &BTreeMap<u32, Landing>,
    checks: &[Value],
    newest_landing: Option<&Landing>,
    anchor: Option<&Value>,
    last_deploy: Option<&Value>,
    tasks: &[&Value],
    live: bool,
) -> Value {
    let mut sorted: Vec<&Value> = tasks.to_vec();
    sorted.sort_by_key(|task| std::cmp::Reverse(task_key(task)));
    let total = sorted.len();
    let tasks: Vec<Value> = sorted
        .into_iter()
        .take(MAX_TASKS)
        .map(|task| {
            let state = task["state"].as_str().unwrap_or_default();
            let gates: Vec<Value> = task["gates"]
                .as_array()
                .into_iter()
                .flatten()
                .rev()
                .take(MAX_GATES)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .map(|gate| {
                    json!({"verdict": gate["verdict"], "reason": cut(&gate["reason"]),
                           "at_ms": gate["at_ms"]})
                })
                .collect();
            json!({
                "id": task["id"],
                "state": task["state"],
                "kind": task["kind"],
                "agent_id": task["agent_id"],
                "session_id": task["session_id"],
                "objective": task["objective"],
                "revisions": task["revisions"],
                "candidate": task["candidate"],
                "base": task["base"],
                "updated_at_ms": task["updated_at_ms"],
                "review": {
                    "state": task["state"],
                    "revisions": task["revisions"],
                    "note": cut(&task["review_note"]),
                    "accepted": matches!(state, "accepted" | "landing" | "landed"),
                    "sent_back": state == "revise",
                    "last_gate": gates.last().cloned().unwrap_or(Value::Null),
                },
                "gates": gates,
            })
        })
        .collect();
    let mut checks: Vec<Value> = checks.to_vec();
    checks.sort_by(|left, right| {
        right["at_ms"]
            .as_i64()
            .unwrap_or(i64::MIN)
            .cmp(&left["at_ms"].as_i64().unwrap_or(i64::MIN))
    });
    checks.truncate(MAX_VERIFICATIONS);
    json!({
        "tasks_total": total,
        "tasks": tasks,
        "landings": landed.values().map(|landing| json!({
            "objective": landing.objective,
            "task": landing.task,
            "kind": landing.kind,
            "at_ms": landing.at_ms,
            "event": landing.event,
            "commit": landing.commit,
            "base": landing.base,
        })).collect::<Vec<_>>(),
        "verifications": checks,
        // The deploy the live claim stands on, and only that: an unshipped
        // mission's newest Spawn deploy is not proof of its capability.
        "deploy": if live {
            anchor.cloned().unwrap_or(Value::Null)
        } else {
            Value::Null
        },
        "last_deploy": last_deploy.cloned().unwrap_or(Value::Null),
        "newest_landing_at_ms": newest_landing.map_or(Value::Null, |landing| json!(landing.at_ms)),
    })
}

/// A capability's provenance: the mapping's sources plus the native records
/// that already exist for it (the objectives themselves, the tasks a person
/// assigned, the verifications it has, and the deploy that made it live),
/// deduplicated by stable reference. `linked` says a native record is
/// present; an external source is a link, never a state.
fn sources_json(
    spec: &Spec,
    mission: &Value,
    rows: &[&Value],
    tasks: &[&Value],
    checks: &[Value],
    live_deploy: Option<&Value>,
    facts: &Facts,
) -> Vec<Value> {
    let mission_id = mission["id"].as_i64().unwrap_or_default();
    let mut out: Vec<Value> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for source in &spec.sources {
        let linked = source_linked(source, mission, rows, facts);
        let title = source
            .title
            .clone()
            .or_else(|| native_title(source.native.as_ref(), mission, rows, checks, facts));
        push_source(
            &mut out,
            &mut seen,
            json!({
                "kind": source.kind,
                "ref": source.reference,
                "url": source.url,
                "title": title,
                "native": source.native.is_some(),
                "linked": linked,
            }),
        );
    }
    for task in tasks.iter().filter(|task| task["created_by"] == "human") {
        push_source(
            &mut out,
            &mut seen,
            json!({
                "kind": "task",
                "ref": format!("task:{}", task["id"].as_i64().unwrap_or_default()),
                "url": Value::Null,
                "title": first_line(text(&task["brief"])),
                "native": true,
                "linked": true,
            }),
        );
    }
    for check in checks.iter().filter(|check| check["result"] != "none") {
        let objective = check["objective"].as_u64().unwrap_or_default();
        push_source(
            &mut out,
            &mut seen,
            json!({
                "kind": "verification",
                "ref": format!("verification:{mission_id}/{objective:03}"),
                "url": Value::Null,
                "title": format!("{} by {}", text(&check["result"]), text(&check["by"])),
                "native": true,
                "linked": true,
            }),
        );
    }
    if let Some(deploy) = live_deploy {
        push_source(
            &mut out,
            &mut seen,
            json!({
                "kind": "deploy",
                "ref": format!("deploy:{}", deploy["event"].as_i64().unwrap_or_default()),
                "url": Value::Null,
                "title": format!("deploys {}", short(&deploy["head"])),
                "native": true,
                "linked": true,
            }),
        );
    }
    out
}

fn push_source(out: &mut Vec<Value>, seen: &mut BTreeSet<String>, source: Value) {
    let reference = text(&source["ref"]).to_string();
    if seen.insert(reference) {
        out.push(source);
    }
}

fn source_linked(source: &Source, mission: &Value, rows: &[&Value], facts: &Facts) -> bool {
    match &source.native {
        None => false,
        Some(Native::Objective { objective, .. }) => rows
            .iter()
            .any(|row| row["id"].as_u64() == Some(u64::from(*objective))),
        Some(Native::Mission(id)) => mission["id"].as_i64() == Some(*id),
        Some(Native::Task(task)) => mission["tasks"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|row| row["id"].as_i64() == Some(*task)),
        Some(Native::Verification { mission, objective }) => {
            facts.verifications.iter().any(|seen| {
                seen["mission"].as_i64() == Some(*mission)
                    && seen["objective"].as_u64() == Some(u64::from(*objective))
            })
        }
        Some(Native::Deploy(event)) => facts
            .deployments
            .iter()
            .any(|deploy| deploy["event"].as_i64() == Some(*event)),
    }
}

fn native_title(
    native: Option<&Native>,
    mission: &Value,
    rows: &[&Value],
    checks: &[Value],
    facts: &Facts,
) -> Option<String> {
    match native? {
        Native::Objective { objective, .. } => rows
            .iter()
            .find(|row| row["id"].as_u64() == Some(u64::from(*objective)))
            .and_then(|row| row["title"].as_str())
            .map(str::to_string),
        Native::Mission(id) if mission["id"].as_i64() == Some(*id) => {
            Some(first_line(text(&mission["text"])))
        }
        Native::Mission(id) => Some(format!("mission {id}")),
        Native::Task(task) => mission["tasks"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|row| row["id"].as_i64() == Some(*task))
            .map(|row| first_line(text(&row["brief"]))),
        Native::Verification { objective, .. } => checks
            .iter()
            .find(|check| check["objective"].as_u64() == Some(u64::from(*objective)))
            .filter(|check| check["result"] != "none")
            .map(|check| format!("{} by {}", text(&check["result"]), text(&check["by"]))),
        Native::Deploy(event) => facts
            .deployments
            .iter()
            .find(|deploy| deploy["event"].as_i64() == Some(*event))
            .map(|deploy| format!("deploys {}", short(&deploy["head"]))),
    }
}

/// A commit as a list shows it.
fn short(commit: &Value) -> String {
    commit
        .as_str()
        .map(|commit| commit.chars().take(12).collect())
        .unwrap_or_default()
}

/// The first line of `text`, trimmed and at most 200 characters.
fn first_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or_default()
        .trim()
        .chars()
        .take(200)
        .collect()
}

fn counts(features: &[Value]) -> Value {
    let count = |state: &str| {
        features
            .iter()
            .filter(|feature| feature["state"] == state)
            .count()
    };
    json!({
        "features": features.len(),
        "planned": count("planned"),
        "implemented": count("implemented"),
        "verified": count("verified"),
        "live": count("live"),
        "blocked": features.iter().filter(|feature| feature["blocked"].is_object()).count(),
        "unowned": features.iter().filter(|feature| feature["unowned"] == true).count(),
        "complete": features.iter().filter(|feature| feature["complete"] == true).count(),
    })
}

fn state_rank(state: &Value) -> u8 {
    match state.as_str() {
        Some("planned") => 0,
        Some("implemented") => 1,
        Some("verified") => 2,
        Some("live") => 3,
        _ => 4,
    }
}

fn text(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

/// `value` as text, cut at [`TEXT_CHARS`] on a character boundary; null is
/// null.
fn cut(value: &Value) -> Value {
    match value.as_str() {
        None => Value::Null,
        Some(text) if text.chars().count() <= TEXT_CHARS => Value::String(text.to_string()),
        Some(text) => {
            let cut: String = text.chars().take(TEXT_CHARS).collect();
            Value::String(format!("{cut}…"))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::{Value, json};

    use super::{
        Facts, Native, Source, Spec, build, derive, fallback_specs, parse_mapping, read_map,
        read_mapping, shipped_by_deploy, with_deployed,
    };

    fn spec(id: &str, objectives: &[u32]) -> Spec {
        Spec {
            id: id.to_string(),
            title: format!("Feature {id}"),
            summary: None,
            parent: None,
            kind: "capability".to_string(),
            tags: Vec::new(),
            files: Vec::new(),
            relations: Vec::new(),
            objectives: objectives.to_vec(),
            sources: objectives
                .iter()
                .map(|objective| Source {
                    kind: "objective".to_string(),
                    reference: format!("objective:4/{objective:03}"),
                    url: None,
                    title: None,
                    native: Some(Native::Objective {
                        mission: 4,
                        objective: *objective,
                    }),
                })
                .collect(),
        }
    }

    fn mission() -> serde_json::Value {
        json!({
            "id": 4, "text": "Spawn RSI", "state": "active", "sources": ["/m/missions/spawn.git"],
            "gate": "/g", "accept": "gate", "halted": null, "baseline": "b",
            "features_file": null, "backlog": "/backlog",
            "objectives": [
                {"id": 1, "title": "One", "status": "open", "landings": [], "solved_on_base": null},
                {"id": 2, "title": "Two", "status": "solved", "landings": [],
                 "solved_on_base": null},
                {"id": 3, "title": "Three", "status": "solved", "landings": [],
                 "solved_on_base": null},
                {"id": 4, "title": "Four", "status": "solved", "landings": [],
                 "solved_on_base": null},
                {"id": 5, "title": "Five", "status": "open", "landings": [], "solved_on_base": null},
            ],
            "tasks": [
                {"id": 20, "state": "landed", "kind": "ephemeral", "agent_id": "a20",
                 "session_id": "s20", "objective": 2, "revisions": 0, "candidate": "c2",
                 "base": "b2", "updated_at_ms": 100, "review_note": null, "gates": [],
                 "created_by": "spawn-integrate", "brief": "objective 2"},
                {"id": 30, "state": "landed", "kind": "ephemeral", "agent_id": "a30",
                 "session_id": "s30", "objective": 3, "revisions": 0, "candidate": "c3",
                 "base": "b3", "updated_at_ms": 200, "review_note": null, "gates": [],
                 "created_by": "spawn-integrate", "brief": "objective 3"},
                {"id": 40, "state": "landed", "kind": "ephemeral", "agent_id": "a40",
                 "session_id": "s40", "objective": 4, "revisions": 0, "candidate": "c4",
                 "base": "b4", "updated_at_ms": 300, "review_note": null, "gates": [],
                 "created_by": "human", "brief": "the owner asked for feature four"},
                {"id": 50, "state": "accepted", "kind": "ephemeral", "agent_id": "a50",
                 "session_id": "s50", "objective": 5, "revisions": 1, "candidate": null,
                 "base": null, "updated_at_ms": 400, "review_note": "ok", "gates": [],
                 "created_by": "spawn_supervisor", "brief": "objective 5"},
            ],
        })
    }

    fn facts_of<'a>(
        landings: &'a [serde_json::Value],
        verifications: &'a [serde_json::Value],
        deployments: &'a [serde_json::Value],
        blockers: &'a [serde_json::Value],
    ) -> Facts<'a> {
        // The anchor is the newest success in the list, unless a test says
        // otherwise (`facts_anchored`).
        let anchor = deployments.iter().find(|deploy| deploy["ok"] == true);
        Facts {
            landings,
            verifications,
            deployments,
            anchor,
            blockers,
        }
    }

    /// [`facts_of`] with a live anchor that the display window does not
    /// hold: what the store reads over the whole deploy log.
    fn facts_anchored<'a>(
        landings: &'a [serde_json::Value],
        verifications: &'a [serde_json::Value],
        deployments: &'a [serde_json::Value],
        anchor: &'a serde_json::Value,
        blockers: &'a [serde_json::Value],
    ) -> Facts<'a> {
        Facts {
            landings,
            verifications,
            deployments,
            anchor: Some(anchor),
            blockers,
        }
    }

    /// The four states are four different pieces of evidence: an accepted
    /// task is not implemented, a landing is not verified, a pass is not
    /// live, and a deploy after the landing is. The booleans stay separate:
    /// a live capability can be unverified.
    #[test]
    fn planned_implemented_verified_and_live_never_collapse() {
        // The one deploy came after 3's and 4's landings, before 2's.
        let landings = vec![
            json!({"mission": 4, "objective": 2, "task": 20, "landed_ms": 300, "event": 302, "brief": ""}),
            json!({"mission": 4, "objective": 3, "task": 30, "landed_ms": 200, "event": 202, "brief": ""}),
            json!({"mission": 4, "objective": 4, "task": 40, "landed_ms": 100, "event": 102, "brief": ""}),
        ];
        let verifications = vec![
            json!({"mission": 4, "objective": 2, "result": "pass", "by": "lead", "how": "looked",
                   "at_ms": 350, "event": 352}),
            json!({"mission": 4, "objective": 4, "result": "pass", "by": "lead", "how": "looked",
                   "at_ms": 150, "event": 152}),
        ];
        let deployments = vec![
            json!({"event": 400, "at_ms": 250, "ok": true, "head": "d".repeat(40), "by": "landing",
                   "reason": ""}),
        ];
        let facts = facts_of(&landings, &verifications, &deployments, &[]);
        let planned = derive(&spec("p", &[1]), &mission(), &facts);
        assert_eq!(planned["state"], "planned");
        assert_eq!(planned["implemented"], false);
        // An accepted task of a gated mission is still in flight, not landed.
        let accepted = derive(&spec("a", &[5]), &mission(), &facts);
        assert_eq!(accepted["state"], "planned");
        assert_eq!(accepted["implemented"], false);
        let verified = derive(&spec("v", &[2]), &mission(), &facts);
        assert_eq!(verified["state"], "verified");
        assert_eq!(verified["live"], false, "a pass is not a deploy");
        let live_unverified = derive(&spec("u", &[3]), &mission(), &facts);
        assert_eq!(live_unverified["state"], "live");
        assert_eq!(
            live_unverified["verified"], false,
            "a deploy ships work nobody has verified"
        );
        let live = derive(&spec("l", &[4]), &mission(), &facts);
        assert_eq!(live["state"], "live");
        assert_eq!(live["verified"], true);
        assert_eq!(live["evidence"]["deploy"]["event"], 400);
    }

    /// A capability is only as far as its least advanced objective: one that
    /// never landed holds the capability at planned (a live sibling does not
    /// lift it), and one that landed without a pass holds it at implemented.
    #[test]
    fn a_capability_is_only_as_far_as_its_least_advanced_objective() {
        let landings = vec![
            json!({"mission": 4, "objective": 1, "task": 20, "landed_ms": 100, "event": 102}),
            json!({"mission": 4, "objective": 2, "task": 20, "landed_ms": 300, "event": 302}),
        ];
        let verifications = vec![json!({"mission": 4, "objective": 1, "result": "pass",
                                        "by": "lead", "how": "looked", "at_ms": 150, "event": 152})];
        let deployments = vec![json!({"event": 400, "at_ms": 400, "ok": true,
                                      "head": "d".repeat(40), "by": "landing", "reason": ""})];
        let facts = facts_of(&landings[..1], &verifications, &deployments, &[]);
        // 1 landed and verified; 2 never landed. The capability is planned,
        // not live on 1's evidence.
        let never_landed = derive(&spec("multi", &[1, 2]), &mission(), &facts);
        assert_eq!(never_landed["state"], "planned", "{never_landed}");
        assert_eq!(never_landed["implemented"], false);
        assert_eq!(never_landed["verified"], false);
        assert!(
            never_landed["gaps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|gap| gap["kind"] == "not_landed" && gap["objectives"] == json!([2])),
            "{never_landed}"
        );
        // Both landed; only 1 has a pass, and nothing shipped. The lowest
        // state is implemented: 1's pass does not verify 2.
        let facts = facts_of(&landings, &verifications, &[], &[]);
        let unverified = derive(&spec("multi", &[1, 2]), &mission(), &facts);
        assert_eq!(unverified["state"], "implemented", "{unverified}");
        assert_eq!(unverified["verified"], false);
        assert!(
            unverified["gaps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|gap| gap["kind"] == "unverified" && gap["objectives"] == json!([2])),
            "{unverified}"
        );
    }

    /// A capability that names no objective is planned: there is nothing to
    /// land, verify or deploy. The emptiness must not let `all` over an empty
    /// list (and its vacuously passing checks) call such a capability live.
    #[test]
    fn a_capability_with_no_objective_is_planned() {
        let specs = parse_mapping(
            &json!({"features": [{"id": "sources-only", "title": "Sources only",
                                  "sources": [{"kind": "issue", "ref": "63"}]}]}),
            4,
        )
        .expect("a mapping of identities only");
        let deployments = vec![json!({"event": 400, "at_ms": 400, "ok": true,
                                      "head": "d".repeat(40), "by": "landing", "reason": ""})];
        let facts = facts_of(&[], &[], &deployments, &[]);
        let feature = derive(&specs[0], &mission(), &facts);
        assert_eq!(feature["state"], "planned", "{feature}");
        assert_eq!(feature["implemented"], false);
        assert_eq!(feature["verified"], false);
        assert_eq!(feature["live"], false);
        assert!(
            feature["gaps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|gap| gap["kind"] == "no_objective"),
            "{feature}"
        );
        assert_eq!(feature["evidence"]["deploy"], Value::Null);
    }

    /// Only a source ending `/missions/spawn.git` is shipped by Spawn's own
    /// deploy: a broader suffix would make a mission Spawn does not ship
    /// eligible for live.
    #[test]
    fn only_the_scorecards_exact_source_is_shipped() {
        let shipped = |source: &str| shipped_by_deploy(&json!({"sources": [source]}));
        assert!(shipped("/m/missions/spawn.git"));
        for other in [
            "/m/missions/supply-pa.git",
            "/m/missions/not-spawn.git",
            "/m/missions/myspawn.git",
            "spawn.git",
            "/m/missions/spawn.git/",
            "/m/missions/spawn.git.bak",
        ] {
            assert!(!shipped(other), "{other}");
        }
        assert!(!shipped_by_deploy(&json!({"sources": []})));
        assert!(shipped_by_deploy(
            &json!({"sources": ["/m/missions/flake-doctor.git", "/m/missions/spawn.git"]})
        ));
    }

    /// The store's deployed-missions fact decides when a row carries it
    /// (any project's deploy source, not one path): Spawn RSI shipped,
    /// Supply PA and Flake Doctor not, a sourceless mission never; a row
    /// without it falls back to the source predicate with the same truth.
    #[test]
    fn the_stores_deployed_missions_decide_shipped() {
        let mut missions = vec![
            json!({"id": 4, "sources": ["/m/missions/spawn.git"]}),
            json!({"id": 5, "sources": ["/m/missions/supply-pa.git"]}),
            json!({"id": 6, "sources": ["/m/missions/flake-doctor.git"]}),
            json!({"id": 7, "sources": []}),
            json!({"id": 8, "sources": ["/srv/other/acme.git"]}),
        ];
        let fallback: Vec<bool> = missions.iter().map(shipped_by_deploy).collect();
        assert_eq!(fallback, [true, false, false, false, false]);
        // The store says Supply PA and Flake Doctor are undeployed; Acme's
        // source is deployed by a deploy of its own.
        with_deployed(&mut missions, &json!([5, 6]));
        let shipped: Vec<bool> = missions.iter().map(shipped_by_deploy).collect();
        assert_eq!(shipped, [true, false, false, false, true]);
        // An undeployed answer overrides a source the fallback would ship.
        with_deployed(&mut missions, &json!([4, 5, 6, 8]));
        assert!(!shipped_by_deploy(&missions[0]));
        assert!(!shipped_by_deploy(&missions[4]));
    }

    /// The landing's time is not a deploy's: a deploy at exactly the
    /// landing's millisecond did not follow it (strict `>`), so the landing
    /// is not live and the gap names it (delivery waits at `>=`).
    #[test]
    fn a_deploy_at_the_landing_time_does_not_ship_it() {
        let landings =
            vec![json!({"mission": 4, "objective": 2, "task": 20, "landed_ms": 100, "event": 102})];
        let deployments = vec![json!({"event": 400, "at_ms": 100, "ok": true,
                                      "head": "d".repeat(40), "by": "landing", "reason": ""})];
        let facts = facts_of(&landings, &[], &deployments, &[]);
        let feature = derive(&spec("same", &[2]), &mission(), &facts);
        assert_eq!(feature["state"], "implemented", "{feature}");
        assert_eq!(feature["live"], false);
        assert!(
            feature["gaps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|gap| gap["kind"] == "not_deployed" && gap["objectives"] == json!([2])),
            "{feature}"
        );
    }

    /// A verification at exactly the landing's millisecond is not after it
    /// (strict `>`), so it verifies nothing.
    #[test]
    fn a_verification_at_the_landing_time_does_not_verify_it() {
        let landings =
            vec![json!({"mission": 4, "objective": 2, "task": 20, "landed_ms": 100, "event": 102})];
        let verifications = vec![json!({"mission": 4, "objective": 2, "result": "pass",
                                        "by": "lead", "how": "looked", "at_ms": 100,
                                        "event": 103})];
        let facts = facts_of(&landings, &verifications, &[], &[]);
        let feature = derive(&spec("same", &[2]), &mission(), &facts);
        assert_eq!(feature["state"], "implemented", "{feature}");
        assert_eq!(feature["verified"], false);
        assert!(
            feature["gaps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|gap| gap["kind"] == "unverified" && gap["objectives"] == json!([2])),
            "{feature}"
        );
        assert_eq!(feature["evidence"]["verifications"][0]["result"], "none");
    }

    /// Two verifications in one millisecond: the later event decides, not
    /// the order the records arrive in (`checks_of`'s `event` tie-break).
    #[test]
    fn the_later_event_decides_when_two_verifications_share_a_millisecond() {
        let landings =
            vec![json!({"mission": 4, "objective": 2, "task": 20, "landed_ms": 100, "event": 102})];
        let verifications = vec![
            json!({"mission": 4, "objective": 2, "result": "fail", "by": "lead",
                   "how": "looked", "at_ms": 300, "event": 302}),
            json!({"mission": 4, "objective": 2, "result": "pass", "by": "lead",
                   "how": "looked", "at_ms": 300, "event": 301}),
        ];
        let facts = facts_of(&landings, &verifications, &[], &[]);
        let feature = derive(&spec("tie", &[2]), &mission(), &facts);
        assert_eq!(feature["verified"], false, "{feature}");
        assert!(
            feature["gaps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|gap| gap["kind"] == "verification_failed" && gap["objectives"] == json!([2])),
            "{feature}"
        );
    }

    /// A pass recorded before the landing verifies nothing: the newest
    /// verification after the landing is the one that counts.
    #[test]
    fn only_a_verification_after_the_landing_verifies_it() {
        let landings =
            vec![json!({"mission": 4, "objective": 2, "task": 20, "landed_ms": 300, "event": 302})];
        let verifications = vec![json!({"mission": 4, "objective": 2, "result": "pass",
                                        "by": "lead", "how": "looked", "at_ms": 200, "event": 202})];
        let facts = facts_of(&landings, &verifications, &[], &[]);
        let feature = derive(&spec("late", &[2]), &mission(), &facts);
        assert_eq!(feature["state"], "implemented", "{feature}");
        assert_eq!(feature["verified"], false);
        assert!(
            feature["gaps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|gap| gap["kind"] == "unverified"),
            "{feature}"
        );
        assert_eq!(feature["evidence"]["verifications"][0]["result"], "none");
    }

    /// The newest verification decides: a later fail does not let an older
    /// pass verify the landing, whatever order the records arrive in.
    #[test]
    fn the_newest_verification_decides() {
        let landings =
            vec![json!({"mission": 4, "objective": 2, "task": 20, "landed_ms": 100, "event": 102})];
        let verifications = vec![
            json!({"mission": 4, "objective": 2, "result": "pass", "by": "lead",
                   "how": "looked", "at_ms": 300, "event": 301}),
            json!({"mission": 4, "objective": 2, "result": "fail", "by": "lead",
                   "how": "looked", "at_ms": 400, "event": 402}),
        ];
        let facts = facts_of(&landings, &verifications, &[], &[]);
        let feature = derive(&spec("two", &[2]), &mission(), &facts);
        assert_eq!(feature["state"], "implemented", "{feature}");
        assert_eq!(feature["verified"], false);
        assert!(
            feature["gaps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|gap| gap["kind"] == "verification_failed" && gap["objectives"] == json!([2])),
            "{feature}"
        );
    }

    /// The live anchor comes from the whole deploy log, not the display
    /// window: a burst of failures after the last success cannot pull a
    /// shipped capability back to implemented.
    #[test]
    fn a_capability_stays_live_when_the_newest_deploys_fail() {
        let landings =
            vec![json!({"mission": 4, "objective": 2, "task": 20, "landed_ms": 100, "event": 102})];
        let verifications = vec![json!({"mission": 4, "objective": 2, "result": "pass",
                                        "by": "lead", "how": "looked", "at_ms": 150, "event": 152})];
        // The display window the store returns: the newest outcome first.
        let deployments = vec![
            json!({"event": 900, "at_ms": 900, "ok": false, "head": "f".repeat(40),
                   "by": "owner", "reason": "the build failed"}),
            json!({"event": 800, "at_ms": 800, "ok": false, "head": "e".repeat(40),
                   "by": "owner", "reason": "the build failed"}),
        ];
        let anchor = json!({"event": 400, "at_ms": 400, "ok": true, "head": "d".repeat(40),
                            "by": "landing", "reason": ""});
        let facts = facts_anchored(&landings, &verifications, &deployments, &anchor, &[]);
        let feature = derive(&spec("shipped", &[2]), &mission(), &facts);
        assert_eq!(feature["state"], "live", "{feature}");
        assert_eq!(feature["evidence"]["deploy"]["event"], 400);
        assert_eq!(
            feature["evidence"]["last_deploy"]["event"], 900,
            "the failure, not the anchor"
        );
        // The anchor the card stands on is the whole-log success.
        let answer = build(
            &[mission()],
            &json!({"landings": landings, "verifications": verifications}),
            &json!(deployments),
            &anchor,
            &json!([]),
            0,
        );
        assert_eq!(answer["live_anchor"]["event"], 400);
        assert_eq!(answer["deploy"]["event"], 900);
    }

    /// The anchor the route asks the store for is the whole-log success, not
    /// the newest of the display window: one successful deploy and then
    /// sixty failures still leaves the capability live. Regressing
    /// `read_map` to a window-derived anchor fails here.
    #[test]
    fn the_map_reads_the_live_anchor_from_the_whole_deploy_log() {
        let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
            "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
        );
        let schema = spawn_postgres::testing::TestSchema::new("feature_map_live_anchor");
        let store = spawn_postgres::PostgresStore::connect_in_schema(&url, &schema)
            .expect("a schema of its own, migrated");
        let separator = if url.contains('?') { '&' } else { '?' };
        let owner = format!("{url}{separator}options=-csearch_path%3D{schema}");
        let mut client =
            postgres::Client::connect(&owner, postgres::NoTls).expect("the owner connects");
        let dir = std::env::temp_dir().join(format!(
            "spawn-feature-anchor-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a backlog");
        std::fs::write(
            dir.join("002.json"),
            json!({"id": 2, "title": "Two", "prompt": "Objective 2."}).to_string(),
        )
        .expect("an objective");
        let record = |client: &mut postgres::Client, value: Value| {
            client
                .query_one("select spawn_control_record($1)", &[&value])
                .expect("a control record");
        };
        record(
            &mut client,
            json!({"type": "mission.added", "id": 4, "text": "Spawn RSI",
                   "origin": "human", "sources": ["/m/missions/spawn.git"]}),
        );
        client
            .query_one(
                "select spawn_mission_configure($1, $2, $3, $4, $5)",
                &[
                    &4_i64,
                    &"/g",
                    &"gate",
                    &None::<Value>,
                    &json!({"backlog": dir.display().to_string(), "resident": "rsi"}),
                ],
            )
            .expect("configured");
        record(
            &mut client,
            json!({"type": "task.assigned", "id": 20, "mission_id": 4,
                   "kind": "ephemeral", "created_by": "spawn-integrate",
                   "agent_id": "agent_20", "session_id": "session_20",
                   "objective": 2, "brief": "Objective 2."}),
        );
        record(
            &mut client,
            json!({"type": "task.landed", "id": 20, "from": "a".repeat(40),
                   "to": "b".repeat(40)}),
        );
        store
            .verification_record(&json!({"type": "objective.verified", "mission": 4,
                                         "objective": 2, "by": "lead", "how": "read it",
                                         "result": "pass"}))
            .expect("verified");
        // The landing and the success must not share a millisecond: live is
        // strictly after the newest landing.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let anchor = store
            .deploy_record(&json!({"type": "deploy.finished", "ok": true,
                                   "head": "c".repeat(40), "by": "landing", "reason": ""}))
            .expect("the success");
        for attempt in 0..60 {
            store
                .deploy_record(&json!({"type": "deploy.finished", "ok": false,
                                       "head": format!("{attempt:040x}"), "by": "owner",
                                       "reason": "the build failed"}))
                .expect("a failure");
        }
        let map = read_map(&store, Some(4), 50).expect("the map");
        assert_eq!(
            map["deploy"]["ok"], false,
            "the display window holds only the failures: {map}"
        );
        assert_eq!(
            map["live_anchor"]["event"], anchor,
            "the whole log's success"
        );
        let feature = map["missions"][0]["features"]
            .as_array()
            .unwrap()
            .iter()
            .find(|feature| feature["id"] == "objective-002")
            .cloned()
            .unwrap_or_else(|| panic!("objective-002 in {map}"));
        assert_eq!(feature["state"], "live", "{feature}");
        assert_eq!(feature["evidence"]["deploy"]["event"], anchor);
        assert_eq!(feature["evidence"]["last_deploy"]["ok"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A backlog that could not be read is named in the answer: the page
    /// must not show empty columns as "no capabilities".
    #[test]
    fn a_backlog_that_cannot_be_read_is_named_in_the_answer() {
        let mut row = mission();
        row["objectives"] = json!([]);
        row["backlog_error"] = json!("read /backlog: No such file or directory");
        let answer = build(
            &[row],
            &json!({"landings": [], "verifications": []}),
            &json!([]),
            &Value::Null,
            &json!([]),
            0,
        );
        assert_eq!(
            answer["missions"][0]["backlog_error"],
            "read /backlog: No such file or directory"
        );
        assert_eq!(answer["missions"][0]["features"], json!([]));
        let readable = mission();
        let answer = build(
            &[readable],
            &json!({"landings": [], "verifications": []}),
            &json!([]),
            &Value::Null,
            &json!([]),
            0,
        );
        assert_eq!(answer["missions"][0]["backlog_error"], Value::Null);
    }

    /// A capability's provenance includes the native records that already
    /// exist: its objective, a task a person assigned, its verification and
    /// the deploy that made it live. External sources stay links; nothing
    /// here changes the derived state.
    #[test]
    fn sources_link_native_objectives_tasks_verifications_and_the_deploy() {
        let landings =
            vec![json!({"mission": 4, "objective": 4, "task": 40, "landed_ms": 100, "event": 102})];
        let verifications = vec![json!({"mission": 4, "objective": 4, "result": "pass",
                                        "by": "lead", "how": "seen", "at_ms": 150, "event": 152})];
        let deployments = vec![json!({"event": 400, "at_ms": 250, "ok": true,
                                      "head": "d".repeat(40), "by": "landing", "reason": ""})];
        let facts = facts_of(&landings, &verifications, &deployments, &[]);
        let mut row = mission();
        row["id"] = json!(4);
        row["objectives"] = json!([
            {"id": 4, "title": "Four", "status": "solved", "landings": [],
             "solved_on_base": null}
        ]);
        let feature = derive(&spec("four", &[4]), &row, &facts);
        let refs: Vec<&str> = feature["sources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|source| source["ref"].as_str().unwrap())
            .collect();
        assert!(refs.contains(&"objective:4/004"), "{refs:?}");
        assert!(refs.contains(&"task:40"), "{refs:?}");
        assert!(refs.contains(&"verification:4/004"), "{refs:?}");
        assert!(refs.contains(&"deploy:400"), "{refs:?}");
        assert_eq!(feature["state"], "live");
        // An external source is a link, never a native record.
        let mut external = spec("x", &[4]);
        external.sources.push(Source {
            kind: "issue".to_string(),
            reference: "issue:63".to_string(),
            url: Some("https://github.com/thinkseek/spawn/issues/63".to_string()),
            title: None,
            native: None,
        });
        let feature = derive(&external, &row, &facts);
        let issue = feature["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|source| source["ref"] == "issue:63")
            .expect("the external source");
        assert_eq!(issue["native"], false);
        assert_eq!(issue["linked"], false);
        assert_eq!(issue["url"], "https://github.com/thinkseek/spawn/issues/63");
    }

    /// A deploy recorded before the landing shipped nothing of it; a failed
    /// deploy is not an anchor.
    #[test]
    fn a_deploy_only_makes_live_what_it_followed() {
        let landings =
            vec![json!({"mission": 4, "objective": 2, "task": 20, "landed_ms": 500, "event": 502})];
        let verifications = Vec::new();
        let deployments = vec![
            json!({"event": 900, "at_ms": 900, "ok": false, "head": "f".repeat(40), "by": "owner",
                   "reason": "failed"}),
            json!({"event": 400, "at_ms": 400, "ok": true, "head": "d".repeat(40), "by": "landing",
                   "reason": ""}),
        ];
        let facts = facts_of(&landings, &verifications, &deployments, &[]);
        let row = derive(&spec("late", &[2]), &mission(), &facts);
        assert_eq!(row["state"], "implemented");
        assert!(
            row["gaps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|gap| gap["kind"] == "not_deployed"),
            "{row}"
        );
        let failed_only = vec![
            json!({"event": 900, "at_ms": 900, "ok": false, "head": "f".repeat(40), "by": "owner",
                   "reason": "failed"}),
        ];
        let facts = facts_of(&landings, &verifications, &failed_only, &[]);
        let row = derive(&spec("late", &[2]), &mission(), &facts);
        assert_eq!(row["state"], "implemented", "a failed deploy is no anchor");
    }

    /// A source Spawn's deploy does not ship is never called live: the map
    /// names the gap instead of guessing from the landing.
    #[test]
    fn a_mission_no_deploy_ships_is_not_live() {
        let landings =
            vec![json!({"mission": 6, "objective": 1, "task": 1, "landed_ms": 100, "event": 2})];
        let verifications = vec![json!({"mission": 6, "objective": 1, "result": "pass",
                                        "by": "lead", "how": "seen", "at_ms": 200, "event": 3})];
        let deployments = vec![json!({"event": 4, "at_ms": 400, "ok": true,
                                      "head": "d".repeat(40), "by": "landing", "reason": ""})];
        let facts = facts_of(&landings, &verifications, &deployments, &[]);
        let mut row = mission();
        row["id"] = json!(6);
        row["sources"] = json!(["/m/missions/supply-pa.git"]);
        row["tasks"] = json!([]);
        row["objectives"] = json!([
            {"id": 1, "title": "One", "status": "solved",
             "landings": [{"task": 1, "at_ms": 100}], "solved_on_base": null}
        ]);
        assert!(!shipped_by_deploy(&row));
        let feature = derive(&spec("supply", &[1]), &row, &facts);
        assert_eq!(feature["state"], "verified");
        assert_eq!(feature["live"], false);
        assert!(
            feature["gaps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|gap| gap["kind"] == "live_not_observable"),
            "{feature}"
        );
        // The Spawn deploy is not this capability's proof: it ships another
        // source. It stays the newest deploy record, never `deploy`.
        assert_eq!(feature["evidence"]["deploy"], Value::Null, "{feature}");
        assert_eq!(feature["evidence"]["last_deploy"]["event"], 4);
    }

    /// A mission's pause, halt or red main blocks its capabilities; a
    /// feature with no task in flight is unowned; one with a task is not.
    #[test]
    fn blocked_and_unowned_come_from_the_records() {
        let blockers = vec![json!({"id": 4, "halt": null,
                                   "paused": {"reason": "owner hold", "event": 7, "at_ms": 70},
                                   "main_red": null})];
        let facts = facts_of(&[], &[], &[], &blockers);
        let mut row = mission();
        row["tasks"] = json!([
            {"id": 60, "state": "delivered", "kind": "ephemeral", "agent_id": "a60",
             "session_id": "s60", "objective": 1, "revisions": 0, "candidate": null,
             "base": null, "updated_at_ms": 600, "review_note": null, "gates": []}
        ]);
        let working = derive(&spec("one", &[1]), &row, &facts);
        assert_eq!(working["blocked"]["kind"], "mission_paused");
        assert_eq!(working["blocked"]["reason"], "owner hold");
        assert_eq!(working["unowned"], false);
        assert_eq!(working["owner"]["task"], 60);
        let idle = derive(&spec("five", &[5]), &row, &facts);
        assert_eq!(idle["blocked"]["kind"], "mission_paused");
        assert_eq!(idle["unowned"], true);
    }

    /// A cancelled task with nothing newer in flight is blocked; a revise
    /// is still in flight (the worker gets it again), so it is not.
    #[test]
    fn a_cancelled_task_is_blocked_but_a_revision_is_not() {
        let facts = facts_of(&[], &[], &[], &[]);
        let mut row = mission();
        row["tasks"] = json!([
            {"id": 70, "state": "cancelled", "kind": "ephemeral", "agent_id": "a70",
             "session_id": "s70", "objective": 1, "revisions": 1, "candidate": null,
             "base": null, "updated_at_ms": 700, "review_note": null,
             "gates": [{"verdict": "rejected", "reason": "137.sh failed", "at_ms": 690}]}
        ]);
        let feature = derive(&spec("one", &[1]), &row, &facts);
        assert_eq!(feature["state"], "planned");
        assert_eq!(feature["blocked"]["kind"], "cancelled");
        assert_eq!(feature["blocked"]["reason"], "137.sh failed");
        assert_eq!(feature["unowned"], true);
        row["tasks"] = json!([
            {"id": 71, "state": "revise", "kind": "ephemeral", "agent_id": "a71",
             "session_id": "s71", "objective": 1, "revisions": 1, "candidate": null,
             "base": null, "updated_at_ms": 710, "review_note": "add the test", "gates": []}
        ]);
        let feature = derive(&spec("one", &[1]), &row, &facts);
        assert_eq!(feature["blocked"], Value::Null);
        assert_eq!(feature["unowned"], false);
    }

    /// A mapping is identities and links only, native or external; a typo,
    /// a duplicate objective or a non-http link is refused rather than
    /// silently dropped.
    #[test]
    fn a_mapping_is_checked_and_holds_no_status() {
        let good = json!({"features": [
            {"id": "feature-map", "title": "Feature map",
             "summary": "capabilities",
             "sources": [
                {"kind": "objective", "objective": 488},
                {"kind": "objective", "mission": 4, "objective": 489},
                {"kind": "issue", "ref": "63", "title": "map",
                 "url": "https://github.com/thinkseek/spawn/issues/63"},
                {"kind": "pr", "ref": "69", "url": "https://github.com/thinkseek/spawn/pull/69"},
                {"kind": "mission", "mission": 7},
                {"kind": "task", "task": 544},
                {"kind": "verification", "objective": 488},
                {"kind": "deploy", "event": 123},
                {"kind": "request", "ref": "owner-2026-11-21", "title": "make a feature map"}
             ]},
            {"id": "other", "title": "Other",
             "sources": [{"kind": "objective", "objective": 490}]}
        ]});
        let specs = parse_mapping(&good, 4).expect("a good mapping");
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].objectives, vec![488, 489]);
        assert_eq!(specs[0].sources[2].reference, "issue:63");
        assert_eq!(specs[0].sources[4].reference, "mission:7");
        assert_eq!(specs[0].sources[7].reference, "deploy:123");
        assert_eq!(specs[1].sources.len(), 1);
        // `mission add` validates before the mission has an id (0): a
        // mapping naming its own mission is accepted then, and still refused
        // for another mission the moment the id is known.
        let own = json!({"features": [{"id": "a", "title": "A", "sources": [
            {"kind": "objective", "mission": 7, "objective": 488}]}]});
        assert!(
            parse_mapping(&own, 0).is_ok(),
            "mission add's unknown mission"
        );
        assert!(parse_mapping(&own, 7).is_ok());
        assert!(parse_mapping(&own, 4).is_err(), "a foreign mission");
        // One source twice is one source, not an error.
        let deduped = json!({"features": [{"id": "a", "title": "A", "sources": [
            {"kind": "issue", "ref": "1", "title": "one"},
            {"kind": "issue", "ref": "1", "url": "https://github.com/o/r/issues/1"}
        ]}]});
        let specs = parse_mapping(&deduped, 4).expect("deduplicated");
        assert_eq!(specs[0].sources.len(), 1);
        assert_eq!(specs[0].sources[0].title.as_deref(), Some("one"));
        assert_eq!(
            specs[0].sources[0].url.as_deref(),
            Some("https://github.com/o/r/issues/1")
        );
        // The same objective named twice in one capability is one source,
        // not two capabilities ("in both `a` and `a`").
        let twice = json!({"features": [{"id": "a", "title": "A", "sources": [
            {"kind": "objective", "objective": 1},
            {"kind": "objective", "mission": 4, "objective": 1}
        ]}]});
        let specs = parse_mapping(&twice, 4).expect("one source, not a conflict");
        assert_eq!(specs[0].objectives, vec![1]);
        assert_eq!(specs[0].sources.len(), 1);
        for bad in [
            json!({"features": [{"id": "a", "title": "A", "status": "live"}]}),
            json!({"features": [{"id": "a", "title": "A", "sources": [
                {"kind": "objective", "objective": 1}]},
                                {"id": "a", "title": "B", "sources": [
                {"kind": "objective", "objective": 2}]}]}),
            json!({"features": [{"id": "a", "title": "A", "sources": [
                {"kind": "objective", "objective": 1}]},
                                {"id": "b", "title": "B", "sources": [
                {"kind": "objective", "objective": 1}]}]}),
            json!({"features": [{"id": "A", "title": "A"}]}),
            json!({"features": [{"id": "a", "title": "A", "sources": [
                {"kind": "issue", "ref": "1", "url": "file:///x"}]}]}),
            json!({"features": [{"id": "a", "title": "A", "sources": [
                {"kind": "objective", "objective": "1"}]}]}),
            json!({"features": [{"id": "a", "title": "A", "sources": [
                {"kind": "objective", "mission": 9, "objective": 1}]}]}),
            json!({"features": [{"id": "a", "title": "A", "sources": [
                {"kind": "deploy", "event": 0}]}]}),
            json!({"features": [{"id": "a", "title": "A", "sources": [
                {"kind": "issue", "ref": "1", "number": 1}]}]}),
            json!({"features": [{"id": "a", "title": ""}]}),
            json!({"features": "none"}),
            json!({"features": [], "extra": true}),
        ] {
            assert!(parse_mapping(&bad, 4).is_err(), "refused: {bad}");
        }
    }

    /// The fallback is one capability per backlog objective; a relative
    /// features path is read beside the backlog.
    #[test]
    fn the_fallback_and_the_relative_mapping_file() {
        let specs = fallback_specs(&mission());
        assert_eq!(specs.len(), 5);
        assert_eq!(specs[0].id, "objective-001");
        assert_eq!(specs[0].objectives, vec![1]);

        let dir = std::env::temp_dir().join(format!(
            "spawn-feature-map-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(dir.join("backlog")).expect("temp backlog");
        std::fs::write(
            dir.join("backlog/map.json"),
            r#"{"features":[{"id":"x","title":"X","sources":[{"kind":"objective","objective":1}]}]}"#,
        )
        .expect("a mapping");
        let mut row = mission();
        row["backlog"] = json!(dir.join("backlog").display().to_string());
        row["features_file"] = json!("map.json");
        let (specs, mapping) = super::specs_for(&row);
        assert_eq!(specs.len(), 1);
        assert_eq!(mapping["kind"], "file");
        assert_eq!(mapping["error"], Value::Null);
        row["features_file"] = json!("missing.json");
        let (specs, mapping) = super::specs_for(&row);
        assert_eq!(specs.len(), 5, "the fallback keeps the page useful");
        assert!(mapping["error"].as_str().unwrap().contains("missing.json"));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = read_mapping(Path::new("/definitely/not/here"), 4);
    }

    /// Through a real store: four objectives, one left planned, one landed
    /// and unverified, one landed and verified after the deploy, one landed
    /// and verified before it. The map reads the four states from the
    /// records, not from the tasks' shapes.
    #[test]
    #[allow(clippy::too_many_lines)] // one mission's four objectives, in order
    fn the_store_gives_the_map_the_four_states() {
        let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
            "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
        );
        let schema = spawn_postgres::testing::TestSchema::new("feature_map_states");
        let store = spawn_postgres::PostgresStore::connect_in_schema(&url, &schema)
            .expect("a schema of its own, migrated");
        let separator = if url.contains('?') { '&' } else { '?' };
        let owner = format!("{url}{separator}options=-csearch_path%3D{schema}");
        let mut client =
            postgres::Client::connect(&owner, postgres::NoTls).expect("the owner connects");
        let dir = std::env::temp_dir().join(format!("spawn-feature-states-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a backlog");
        for objective in 1..=4 {
            std::fs::write(
                dir.join(format!("{objective:03}.json")),
                json!({"id": objective, "title": format!("Capability {objective}"),
                       "prompt": format!("Objective {objective}.")})
                .to_string(),
            )
            .expect("an objective");
        }
        let record = |client: &mut postgres::Client, value: Value| {
            client
                .query_one("select spawn_control_record($1)", &[&value])
                .expect("a control record");
        };
        record(
            &mut client,
            json!({"type": "mission.added", "id": 4, "text": "Spawn RSI",
                   "origin": "human", "sources": ["/m/missions/spawn.git"]}),
        );
        client
            .query_one(
                "select spawn_mission_configure($1, $2, $3, $4, $5)",
                &[
                    &4_i64,
                    &"/g",
                    &"gate",
                    &None::<Value>,
                    &json!({"backlog": dir.display().to_string(), "resident": "rsi"}),
                ],
            )
            .expect("configured");
        let assign = |client: &mut postgres::Client, task: i64, objective: i64| {
            record(
                client,
                json!({"type": "task.assigned", "id": task, "mission_id": 4,
                       "kind": "ephemeral", "created_by": "spawn-integrate",
                       "agent_id": format!("agent_{task}"),
                       "session_id": format!("session_{task}"),
                       "objective": objective, "brief": format!("Capability {objective}")}),
            );
            record(
                client,
                json!({"type": "task.landed", "id": task, "from": "a".repeat(40),
                       "to": format!("{:040x}", task)}),
            );
        };
        // 3 lands and is verified, then the deploy ships it: live.
        assign(&mut client, 3, 3);
        store
            .verification_record(&json!({"type": "objective.verified", "mission": 4,
                                         "objective": 3, "by": "lead", "how": "read it",
                                         "result": "pass"}))
            .expect("3 verified");
        store
            .deploy_record(&json!({"type": "deploy.finished", "ok": true,
                                   "head": "b".repeat(40), "by": "landing", "reason": ""}))
            .expect("a deploy");
        // 2 lands and is verified after it: verified, not live.
        assign(&mut client, 2, 2);
        store
            .verification_record(&json!({"type": "objective.verified", "mission": 4,
                                         "objective": 2, "by": "lead", "how": "read it",
                                         "result": "pass"}))
            .expect("2 verified");
        // 1 lands and is never verified: implemented.
        assign(&mut client, 1, 1);
        // 4 is never assigned: planned.
        let missions = crate::serve::objective_missions(&store).expect("the missions");
        let map = build(
            &missions,
            &store.verification_facts().expect("the verifications"),
            &store.deploy_facts(50).expect("the deploys"),
            &store.live_deploy_fact().expect("the live anchor"),
            &store.mission_blockers().expect("the blockers"),
            0,
        );
        let feature = |id: &str| {
            map["missions"][0]["features"]
                .as_array()
                .unwrap()
                .iter()
                .find(|feature| feature["id"] == id)
                .cloned()
                .unwrap_or_else(|| panic!("{id} in {map}"))
        };
        assert_eq!(feature("objective-004")["state"], "planned");
        assert_eq!(feature("objective-001")["state"], "implemented");
        assert_eq!(feature("objective-001")["verified"], false);
        assert_eq!(feature("objective-002")["state"], "verified");
        assert_eq!(feature("objective-002")["live"], false);
        assert_eq!(feature("objective-003")["state"], "live");
        assert!(
            feature("objective-003")["sources"]
                .as_array()
                .unwrap()
                .iter()
                .any(|source| source["kind"] == "deploy"),
            "{map}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The whole answer carries the seconds and the deploy it stands on.
    #[test]
    fn the_answer_says_which_deploy_it_stands_on() {
        let deployments = vec![json!({"event": 9, "at_ms": 90, "ok": true,
                                      "head": "d".repeat(40), "by": "landing", "reason": ""})];
        let anchor = deployments[0].clone();
        let answer = build(
            &[mission()],
            &json!({"landings": [], "verifications": []}),
            &json!(deployments),
            &anchor,
            &json!([]),
            1000,
        );
        assert_eq!(answer["generated_at_ms"], 1000);
        assert_eq!(answer["live_anchor"]["event"], 9);
        assert_eq!(answer["missions"][0]["counts"]["features"], 5);
        // The accepted, ungated task test below is a different mission.
        let mut ungated = mission();
        ungated["gate"] = Value::Null;
        let answer = build(
            &[ungated],
            &json!({"landings": [], "verifications": []}),
            &json!([]),
            &Value::Null,
            &json!([]),
            1000,
        );
        assert!(
            answer["missions"][0]["features"]
                .as_array()
                .unwrap()
                .iter()
                .any(|feature| feature["gaps"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|gap| gap["kind"] == "accepted_without_landing"))
        );
    }

    /// Extended schema: explicit parent hierarchy, kind, tags, normalized
    /// many-to-many files, and typed relations (with deduplication) flow into
    /// both per-feature rows and mission-level `graph` summary.
    #[test]
    #[allow(clippy::too_many_lines)] // full end-to-end schema + graph summary fixture
    fn extended_hierarchy_relations_and_shared_files_flow_into_graph_summary() {
        let map_json = json!({
            "features": [
                {
                    "id": "ui-domain",
                    "title": "Dashboard UI",
                    "kind": "domain",
                    "tags": ["web", "ui", "web"],
                    "files": ["./web/src//shared.ts", "web/src/App.tsx"],
                    "relations": [
                        {"kind": "contains", "target": "feature-mindmap"},
                        {"kind": "contains", "target": "feature-mindmap", "label": "owns view"}
                    ]
                },
                {
                    "id": "feature-mindmap",
                    "title": "Feature Mind Map",
                    "parent": "ui-domain",
                    "kind": "initiative",
                    "tags": ["web", "graph"],
                    "files": ["web/src/shared.ts", "web/src/featuresGraph.ts"],
                    "relations": [
                        {"kind": "depends_on", "target": "feature-api", "label": "reads /api/features"},
                        {"kind": "documented_by", "target": "mindmap-docs"}
                    ],
                    "sources": [{"kind": "objective", "objective": 2}]
                },
                {
                    "id": "feature-api",
                    "title": "Feature Map API",
                    "kind": "subsystem",
                    "files": ["crates/spawn-cli/src/serve/feature_map.rs", "web/src/shared.ts"],
                    "relations": [
                        {"kind": "implements", "target": "ui-domain"}
                    ],
                    "sources": [{"kind": "objective", "objective": 3}]
                },
                {
                    "id": "mindmap-docs",
                    "title": "Mind Map Docs",
                    "parent": "feature-mindmap",
                    "kind": "slice",
                    "files": ["docs/feature-map-mindmap.md"]
                }
            ]
        });
        let specs = parse_mapping(&map_json, 4).expect("valid hierarchical mapping");
        assert_eq!(specs.len(), 4);
        assert_eq!(specs[0].kind, "domain");
        assert_eq!(specs[0].tags, vec!["web", "ui"]);
        assert_eq!(specs[0].files, vec!["web/src/shared.ts", "web/src/App.tsx"]);
        assert_eq!(
            specs[0].relations.len(),
            1,
            "duplicate (kind, target) merged"
        );
        assert_eq!(specs[0].relations[0].label.as_deref(), Some("owns view"));
        assert_eq!(specs[1].parent.as_deref(), Some("ui-domain"));
        assert_eq!(specs[3].kind, "slice");

        let dir = std::env::temp_dir().join(format!(
            "spawn-feature-graph-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let map_path = dir.join("features.json");
        std::fs::write(&map_path, map_json.to_string()).expect("write map");

        let mut row = mission();
        row["features_file"] = json!(map_path.display().to_string());
        let answer = build(
            &[row],
            &json!({"landings": [], "verifications": []}),
            &json!([]),
            &Value::Null,
            &json!([]),
            42,
        );
        let m = &answer["missions"][0];
        assert_eq!(m["mapping"]["kind"], "file");
        assert_eq!(m["mapping"]["error"], Value::Null);
        assert_eq!(m["graph"]["relations_total"], 4);
        assert_eq!(m["graph"]["files_indexed"], 5);
        assert_eq!(
            m["graph"]["shared_files"],
            json!({"web/src/shared.ts": ["feature-api", "feature-mindmap", "ui-domain"]})
        );
        assert_eq!(
            m["graph"]["hierarchy_roots"],
            json!(["ui-domain", "feature-api"])
        );

        let mindmap_row = m["features"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["id"] == "feature-mindmap")
            .expect("feature-mindmap row");
        assert_eq!(mindmap_row["parent"], "ui-domain");
        assert_eq!(mindmap_row["kind"], "initiative");
        assert_eq!(mindmap_row["tags"], json!(["web", "graph"]));
        assert_eq!(
            mindmap_row["files"],
            json!(["web/src/shared.ts", "web/src/featuresGraph.ts"])
        );
        assert_eq!(mindmap_row["relations"].as_array().unwrap().len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Strict graph validation refuses self-links, dangling references,
    /// parent cycles, depends_on cycles, contains cycles, bad paths, and
    /// unknown fields, while allowing associative `relates_to` pairs.
    #[test]
    fn graph_validation_rejects_self_links_dangling_targets_cycles_and_bad_paths() {
        // Associative mutual `relates_to` is valid (not a hierarchy/dependency cycle).
        let mutual = json!({"features": [
            {"id": "a", "title": "A", "relations": [{"kind": "relates_to", "target": "b"}]},
            {"id": "b", "title": "B", "relations": [{"kind": "relates_to", "target": "a"}]}
        ]});
        assert!(parse_mapping(&mutual, 4).is_ok());

        for (label, bad) in [
            (
                "parent self-link",
                json!({"features": [{"id": "a", "title": "A", "parent": "a"}]}),
            ),
            (
                "relation self-link",
                json!({"features": [{"id": "a", "title": "A",
                    "relations": [{"kind": "depends_on", "target": "a"}]}]}),
            ),
            (
                "dangling parent",
                json!({"features": [{"id": "a", "title": "A", "parent": "ghost"}]}),
            ),
            (
                "dangling relation target",
                json!({"features": [{"id": "a", "title": "A",
                    "relations": [{"kind": "blocks", "target": "ghost"}]}]}),
            ),
            (
                "2-node parent cycle",
                json!({"features": [
                    {"id": "a", "title": "A", "parent": "b"},
                    {"id": "b", "title": "B", "parent": "a"}
                ]}),
            ),
            (
                "3-node parent cycle",
                json!({"features": [
                    {"id": "a", "title": "A", "parent": "b"},
                    {"id": "b", "title": "B", "parent": "c"},
                    {"id": "c", "title": "C", "parent": "a"}
                ]}),
            ),
            (
                "depends_on cycle",
                json!({"features": [
                    {"id": "a", "title": "A", "relations": [{"kind": "depends_on", "target": "b"}]},
                    {"id": "b", "title": "B", "relations": [{"kind": "depends_on", "target": "c"}]},
                    {"id": "c", "title": "C", "relations": [{"kind": "depends_on", "target": "a"}]}
                ]}),
            ),
            (
                "contains cycle",
                json!({"features": [
                    {"id": "a", "title": "A", "relations": [{"kind": "contains", "target": "b"}]},
                    {"id": "b", "title": "B", "relations": [{"kind": "contains", "target": "a"}]}
                ]}),
            ),
            (
                "unknown feature kind",
                json!({"features": [{"id": "a", "title": "A", "kind": "epic"}]}),
            ),
            (
                "unknown relation kind",
                json!({"features": [
                    {"id": "a", "title": "A", "relations": [{"kind": "owns", "target": "b"}]},
                    {"id": "b", "title": "B"}
                ]}),
            ),
            (
                "unknown relation field",
                json!({"features": [
                    {"id": "a", "title": "A", "relations": [{"kind": "blocks", "target": "b", "weight": 2}]},
                    {"id": "b", "title": "B"}
                ]}),
            ),
            (
                "file path traversal",
                json!({"features": [{"id": "a", "title": "A", "files": ["src/../secret.rs"]}]}),
            ),
            (
                "absolute file path",
                json!({"features": [{"id": "a", "title": "A", "files": ["/etc/passwd"]}]}),
            ),
            (
                "file path with NUL",
                json!({"features": [{"id": "a", "title": "A", "files": ["src/a\0b.rs"]}]}),
            ),
            (
                "file path with backslash",
                json!({"features": [{"id": "a", "title": "A", "files": ["src\\lib.rs"]}]}),
            ),
            (
                "uppercase tag",
                json!({"features": [{"id": "a", "title": "A", "tags": ["Web"]}]}),
            ),
        ] {
            assert!(
                parse_mapping(&bad, 4).is_err(),
                "{label} should be rejected: {bad}"
            );
        }
    }

    /// Minimal legacy mappings without hierarchy/relations/files remain 100%
    /// backward-compatible with default values on derived rows and graph summary.
    #[test]
    fn legacy_mapping_defaults_remain_backward_compatible() {
        let legacy = json!({"features": [
            {"id": "core", "title": "Core", "sources": [{"kind": "objective", "objective": 1}]}
        ]});
        let specs = parse_mapping(&legacy, 4).expect("legacy mapping");
        assert_eq!(specs[0].parent, None);
        assert_eq!(specs[0].kind, "capability");
        assert_eq!(specs[0].tags, Vec::<String>::new());
        assert_eq!(specs[0].files, Vec::<String>::new());
        assert_eq!(specs[0].relations, Vec::new());

        let facts = facts_of(&[], &[], &[], &[]);
        let row = derive(&specs[0], &mission(), &facts);
        assert_eq!(row["parent"], Value::Null);
        assert_eq!(row["kind"], "capability");
        assert_eq!(row["tags"], json!([]));
        assert_eq!(row["files"], json!([]));
        assert_eq!(row["relations"], json!([]));
    }

    /// A `schema_version: 1` registry: nodes carry `provenance`, a parent and
    /// its child may share an objective, and every provenance list becomes a
    /// source (native when it names this mission's record, a link otherwise).
    #[test]
    #[allow(clippy::too_many_lines)] // one registry, then each refusal
    fn a_registry_maps_provenance_into_sources_and_shares_objectives() {
        let registry = json!({
            "schema_version": 1,
            "initiative_id": "t559u",
            "boundary": {"project_name": "Spawn", "exclusions": []},
            "features": [
                {"id": "domain-a", "title": "Domain A", "kind": "domain", "parent": null,
                 "summary": "a domain", "files": [],
                 "relations": [{"kind": "contains", "target": "cap-a"}],
                 "provenance": {"objectives": ["4/001", "4/002"], "tasks": [],
                                "agents": [], "commits": [], "tests": [], "deploys": [],
                                "artifacts": ["docs/a.md"], "discussions": []}},
                {"id": "cap-a", "title": "Cap A", "kind": "capability", "parent": "domain-a",
                 "summary": "a capability", "status_note": "half done",
                 "files": ["crates/a/src/lib.rs"],
                 "relations": [{"kind": "depends_on", "target": "domain-a"}],
                 "provenance": {
                    "objectives": ["4/001", "4/1", "t559u", "5/003"],
                    "tasks": ["488", "slice-a"],
                    "agents": ["lead-mac"],
                    "commits": ["6544fe424"],
                    "tests": ["crates/a/tests/a.rs"],
                    "deploys": ["2026-11-22-pr82", "77"],
                    "artifacts": ["docs/a.md"],
                    "discussions": ["owner-t559u"]}}
            ]
        });
        let specs = parse_mapping(&registry, 4).expect("a registry");
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].kind, "domain");
        assert_eq!(specs[0].objectives, vec![1, 2]);
        assert_eq!(specs[1].parent.as_deref(), Some("domain-a"));
        assert_eq!(specs[1].files, vec!["crates/a/src/lib.rs".to_string()]);
        // `4/001` and `4/1` are one objective, shared with the parent.
        assert_eq!(specs[1].objectives, vec![1]);
        let refs: Vec<(&str, &str, bool)> = specs[1]
            .sources
            .iter()
            .map(|source| {
                (
                    source.kind.as_str(),
                    source.reference.as_str(),
                    source.native.is_some(),
                )
            })
            .collect();
        assert_eq!(
            refs,
            vec![
                ("objective", "objective:4/001", true),
                ("external-objective", "external-objective:t559u", false),
                ("external-objective", "external-objective:5/003", false),
                ("task", "task:488", true),
                ("external-task", "external-task:slice-a", false),
                ("agent", "agent:lead-mac", false),
                ("commit", "commit:6544fe424", false),
                ("test", "test:crates/a/tests/a.rs", false),
                ("external-deploy", "external-deploy:2026-11-22-pr82", false),
                ("deploy", "deploy:77", true),
                ("artifact", "artifact:docs/a.md", false),
                ("discussion", "discussion:owner-t559u", false),
            ]
        );
        // The Inspector receives every provenance link.
        let facts = facts_of(&[], &[], &[], &[]);
        let row = derive(&specs[1], &mission(), &facts);
        let shown: Vec<&str> = row["sources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|source| source["ref"].as_str().unwrap())
            .collect();
        for wanted in [
            "objective:4/001",
            "external-objective:t559u",
            "commit:6544fe424",
            "test:crates/a/tests/a.rs",
            "external-deploy:2026-11-22-pr82",
            "artifact:docs/a.md",
            "discussion:owner-t559u",
        ] {
            assert!(shown.contains(&wanted), "{wanted} in {shown:?}");
        }
        // While the mission is unknown (0) any mission's objective is native.
        let unknown = parse_mapping(&registry, 0).expect("mission add's unknown mission");
        assert!(unknown[1].objectives.contains(&3));

        // The legacy format still refuses one objective in two capabilities.
        let shared_legacy = json!({"features": [
            {"id": "a", "title": "A", "sources": [{"kind": "objective", "objective": 1}]},
            {"id": "b", "title": "B", "parent": "a",
             "sources": [{"kind": "objective", "objective": 1}]}
        ]});
        assert!(parse_mapping(&shared_legacy, 4).is_err());

        let node = |extra: serde_json::Value| {
            let mut node = json!({"id": "a", "title": "A"});
            for (key, value) in extra.as_object().unwrap() {
                node[key] = value.clone();
            }
            node
        };
        for bad in [
            // Registry fields without `schema_version` are a legacy typo.
            json!({"features": [node(json!({"provenance": {}}))]}),
            json!({"features": [node(json!({"status_note": "x"}))]}),
            json!({"initiative_id": "t559u", "features": []}),
            // Only version 1 is known.
            json!({"schema_version": 2, "features": []}),
            json!({"schema_version": "1", "features": []}),
            json!({"schema_version": 1, "features": [], "extra": true}),
            json!({"schema_version": 1, "features": [node(json!({"status": "live"}))]}),
            json!({"schema_version": 1, "features": [node(json!({"sources": []}))]}),
            json!({"schema_version": 1, "features": [
                node(json!({"provenance": {"objective": ["4/001"]}}))]}),
            json!({"schema_version": 1, "features": [
                node(json!({"provenance": {"objectives": "4/001"}}))]}),
            json!({"schema_version": 1, "features": [
                node(json!({"provenance": {"commits": [1]}}))]}),
            json!({"schema_version": 1, "features": [
                node(json!({"provenance": {"tests": [" "]}}))]}),
            json!({"schema_version": 1, "features": [node(json!({"status_note": 3}))]}),
        ] {
            assert!(parse_mapping(&bad, 4).is_err(), "refused: {bad}");
        }
    }

    /// The repository's own curated registry reads directly as mission 4's
    /// map: every node, with its hierarchy, files, relations and native
    /// objectives, instead of the flat objective fallback.
    #[test]
    fn the_repository_registry_reads_as_a_feature_map() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/feature-map/registry.json");
        let specs = read_mapping(&path, 4).expect("docs/feature-map/registry.json reads");
        let raw: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("the registry"))
                .expect("JSON");
        let nodes = raw["features"].as_array().expect("features");
        assert_eq!(specs.len(), nodes.len());
        assert!(specs.len() >= 40, "the curated hierarchy, not a stub");
        let mut shared = std::collections::BTreeMap::<u32, usize>::new();
        for (spec, node) in specs.iter().zip(nodes) {
            assert_eq!(spec.id, node["id"].as_str().unwrap());
            assert_eq!(spec.kind, node["kind"].as_str().unwrap(), "{}", spec.id);
            assert_eq!(
                spec.parent.as_deref(),
                node["parent"].as_str(),
                "{}",
                spec.id
            );
            let files: Vec<&str> = node["files"]
                .as_array()
                .unwrap()
                .iter()
                .map(|file| file.as_str().unwrap())
                .collect();
            assert_eq!(spec.files, files, "{}", spec.id);
            let relations: Vec<(&str, &str)> = node["relations"]
                .as_array()
                .unwrap()
                .iter()
                .map(|relation| {
                    (
                        relation["kind"].as_str().unwrap(),
                        relation["target"].as_str().unwrap(),
                    )
                })
                .collect();
            let parsed: Vec<(&str, &str)> = spec
                .relations
                .iter()
                .map(|relation| (relation.kind.as_str(), relation.target.as_str()))
                .collect();
            assert_eq!(parsed, relations, "{}", spec.id);
            let mut native: Vec<u32> = node["provenance"]["objectives"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|entry| entry.as_str()?.strip_prefix("4/")?.parse().ok())
                .collect();
            let mut seen = std::collections::BTreeSet::new();
            native.retain(|objective| seen.insert(*objective));
            assert_eq!(spec.objectives, native, "{}", spec.id);
            for objective in &spec.objectives {
                *shared.entry(*objective).or_default() += 1;
            }
            // Every provenance entry is a source; nothing is dropped.
            let entries: usize = node["provenance"]
                .as_object()
                .unwrap()
                .values()
                .map(|list| list.as_array().map_or(0, Vec::len))
                .sum();
            assert!(spec.sources.len() <= entries, "{}", spec.id);
            assert_eq!(spec.sources.is_empty(), entries == 0, "{}", spec.id);
        }
        assert!(
            shared.values().any(|count| *count > 1),
            "parent and child nodes share objectives"
        );
        assert!(
            specs
                .iter()
                .flat_map(|spec| &spec.sources)
                .any(|source| source.reference == "external-objective:t559u"),
            "the initiative id is an external link, not a parse failure"
        );
        let summary = super::graph_summary(&specs);
        assert!(
            summary["hierarchy_roots"]
                .as_array()
                .is_some_and(|roots| !roots.is_empty())
        );
    }
}
