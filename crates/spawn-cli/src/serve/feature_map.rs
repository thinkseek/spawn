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
//! The states are nested (live implies verified implies implemented implies
//! planned) but each is evidence of a different kind: a capability can be
//! live and never verified (the deploy shipped unverified work), and it is
//! shown as live with its verification evidence beside it. What the map
//! never does is call a landing, a merge or a review live.
//!
//! The mapping is a mission's `objectives.features`, an absolute path (or
//! one relative to the mission's backlog) to a JSON file:
//!
//! ```json
//! {"features": [
//!   {"id": "feature-map", "title": "Feature map",
//!    "summary": "planned, implemented, verified and live capabilities",
//!    "objectives": [488],
//!    "issues": [{"url": "https://github.com/acupof-ai/Spawn/issues/63",
//!                "title": "a feature map", "number": 63}],
//!    "prs": [{"url": "https://github.com/acupof-ai/Spawn/pull/69"}]}
//! ]}
//! ```
//!
//! Without one, each backlog objective is a capability of its own. A
//! mapping that cannot be read falls back to the objectives and names the
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
    let mut missions = objective_missions(&store)?;
    if let Some(wanted) = mission {
        missions.retain(|mission| mission["id"].as_i64() == Some(wanted));
    }
    missions.truncate(limit);
    let verifications = store
        .verification_facts()
        .map_err(|error| error.to_string())?;
    let deployments = store
        .deploy_facts(50)
        .map_err(|error| error.to_string())?;
    let blockers = store
        .mission_blockers()
        .map_err(|error| error.to_string())?;
    Ok(build(
        &missions,
        &verifications,
        &deployments,
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
    objectives: Vec<u32>,
    issues: Vec<Link>,
    prs: Vec<Link>,
}

/// An issue or pull request the mapping points at: a link, never a state.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Link {
    url: String,
    title: Option<String>,
    number: Option<u32>,
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
    deployments: &'a [Value],
    blockers: &'a [Value],
}

/// The whole answer, from already-read records: pure apart from the
/// mapping files `mission_row` reads.
fn build(
    missions: &[Value],
    verification: &Value,
    deployments: &Value,
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
        blockers: blockers.as_array().map_or(&[], Vec::as_slice),
    };
    let rows: Vec<Value> = missions
        .iter()
        .map(|mission| mission_row(mission, &facts))
        .collect();
    json!({
        "generated_at_ms": now,
        // The newest deploy of any outcome, and the newest successful one
        // (the live anchor), so the page can say what it stands on.
        "deploy": facts.deployments.first().cloned().unwrap_or(Value::Null),
        "live_anchor": facts.deployments.iter().find(|deploy| deploy["ok"] == true)
            .cloned().unwrap_or(Value::Null),
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
        "mapping": mapping,
        "blocker": blocker,
        "shipped_by_deploy": shipped_by_deploy(mission),
        "counts": counts(&features),
        "features": features,
        "unmapped": unmapped,
    })
}

/// Whether Spawn's own deploy ships the mission's source: the scorecard's
/// own predicate (`UNDEPLOYED_MISSIONS`), so a mission is never "deployed"
/// here that the scorecard counts as undeployed.
fn shipped_by_deploy(mission: &Value) -> bool {
    mission["sources"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|source| source.ends_with("/missions/spawn.git"))
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
    match read_mapping(&path) {
        Ok(specs) => (
            specs,
            json!({"kind": "file", "path": path.display().to_string(), "error": null}),
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
fn resolve(mission: &Value, given: &str) -> Result<PathBuf, String> {
    let path = Path::new(given);
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let backlog = mission["backlog"]
        .as_str()
        .ok_or_else(|| format!("`{given}` is relative and the mission names no backlog"))?;
    Ok(Path::new(backlog).join(path))
}

/// The mapping at `path`, checked.
fn read_mapping(path: &Path) -> Result<Vec<Spec>, String> {
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
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| format!("parse {}: {error}", path.display()))?;
    parse_mapping(&value)
}

/// The mapping's checked capabilities. Unknown fields are refused (a typo
/// must not silently drop a link), as are two capabilities with one id and
/// one objective claimed by two capabilities.
fn parse_mapping(value: &Value) -> Result<Vec<Spec>, String> {
    let root = value
        .as_object()
        .ok_or("a feature map is a JSON object with a `features` array")?;
    if let Some(key) = root.keys().find(|key| key.as_str() != "features") {
        return Err(format!("a feature map has only `features`, not `{key}`"));
    }
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
    let mut owners: BTreeMap<u32, String> = BTreeMap::new();
    for (index, feature) in features.iter().enumerate() {
        let at = format!("features[{index}]");
        let object = feature
            .as_object()
            .ok_or_else(|| format!("{at} is an object, not {feature}"))?;
        if let Some(key) = object
            .keys()
            .find(|key| !matches!(key.as_str(), "id" | "title" | "summary" | "objectives" | "issues" | "prs"))
        {
            return Err(format!("{at} has an unknown field `{key}`"));
        }
        let id = required_text(object, "id", &at)?;
        if id.is_empty()
            || id.len() > 80
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_'))
        {
            return Err(format!(
                "{at}.id is lowercase letters, digits, - and _ (at most 80), not `{id}`"
            ));
        }
        if !ids.insert(id.clone()) {
            return Err(format!("two features have the id `{id}`"));
        }
        let title = required_text(object, "title", &at)?;
        if title.chars().count() > 200 {
            return Err(format!("{at}.title is at most 200 characters"));
        }
        let summary = optional_text(object, "summary", &at)?.filter(|summary| !summary.trim().is_empty());
        let objectives = objectives_of(object, &at, &id, &mut owners)?;
        let issues = links_of(object, "issues", &at)?;
        let prs = links_of(object, "prs", &at)?;
        specs.push(Spec {
            id,
            title,
            summary,
            objectives,
            issues,
            prs,
        });
    }
    Ok(specs)
}

fn objectives_of(
    object: &Map<String, Value>,
    at: &str,
    id: &str,
    owners: &mut BTreeMap<u32, String>,
) -> Result<Vec<u32>, String> {
    let Some(value) = object.get("objectives") else {
        return Ok(Vec::new());
    };
    let list = value
        .as_array()
        .ok_or_else(|| format!("{at}.objectives is an array of numbers"))?;
    let mut objectives = Vec::with_capacity(list.len());
    for item in list {
        let number = item
            .as_u64()
            .and_then(|number| u32::try_from(number).ok())
            .filter(|number| *number >= 1)
            .ok_or_else(|| format!("{at}.objectives has {item}, not a whole number from 1"))?;
        if objectives.contains(&number) {
            return Err(format!("{at}.objectives names {number} twice"));
        }
        if let Some(other) = owners.insert(number, id.to_string()) {
            return Err(format!(
                "objective {number} is in both `{other}` and `{id}`: give it to one"
            ));
        }
        objectives.push(number);
    }
    Ok(objectives)
}

fn links_of(
    object: &Map<String, Value>,
    name: &str,
    at: &str,
) -> Result<Vec<Link>, String> {
    let Some(value) = object.get(name) else {
        return Ok(Vec::new());
    };
    let list = value
        .as_array()
        .ok_or_else(|| format!("{at}.{name} is an array"))?;
    let mut links = Vec::with_capacity(list.len());
    for (index, link) in list.iter().enumerate() {
        let place = format!("{at}.{name}[{index}]");
        let entry = link
            .as_object()
            .ok_or_else(|| format!("{place} is an object with a url"))?;
        if let Some(key) = entry
            .keys()
            .find(|key| !matches!(key.as_str(), "url" | "title" | "number"))
        {
            return Err(format!("{place} has an unknown field `{key}`"));
        }
        let url = required_text(entry, "url", &place)?;
        if url.len() > 2000
            || url.chars().any(char::is_whitespace)
            || !(url.starts_with("https://") || url.starts_with("http://"))
        {
            return Err(format!("{place}.url is an http(s) URL, not `{url}`"));
        }
        let title = optional_text(entry, "title", &place)?.filter(|title| !title.trim().is_empty());
        let number = match entry.get("number") {
            None => None,
            Some(number) => Some(
                number
                    .as_u64()
                    .and_then(|number| u32::try_from(number).ok())
                    .filter(|number| *number >= 1)
                    .ok_or_else(|| format!("{place}.number is a whole number from 1"))?,
            ),
        };
        links.push(Link { url, title, number });
    }
    Ok(links)
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

/// The capability of every objective when the mission names no mapping.
fn fallback_specs(mission: &Value) -> Vec<Spec> {
    mission["objectives"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|objective| {
            let number = u32::try_from(objective["id"].as_u64()?).ok()?;
            Some(Spec {
                id: format!("objective-{number:03}"),
                title: objective["title"]
                    .as_str()
                    .filter(|title| !title.trim().is_empty())
                    .map_or_else(|| format!("Objective {number}"), str::to_string),
                summary: None,
                objectives: vec![number],
                issues: Vec::new(),
                prs: Vec::new(),
            })
        })
        .collect()
}

/// One capability's row: its four-state verdict, evidence, owner, gaps and
/// blocker.
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
    let implemented = !landed.is_empty();
    let verified = implemented && checks.iter().all(|check| check["result"] == "pass");
    let unverified: Vec<u32> = checks
        .iter()
        .filter(|check| check["result"] != "pass")
        .filter_map(|check| u32::try_from(check["objective"].as_u64()?).ok())
        .collect();
    let anchor = facts.deployments.iter().find(|deploy| deploy["ok"] == true);
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
    let complete = live
        && verified
        && missing.is_empty()
        && landed.len() == spec.objectives.len();
    json!({
        "id": spec.id,
        "title": spec.title,
        "summary": spec.summary,
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
        "issues": links_json(&spec.issues),
        "prs": links_json(&spec.prs),
        "owner": newest_in_flight.map_or(Value::Null, owner_json),
        "last_owner": newest_task.map_or(Value::Null, owner_json),
        "unowned": state != "live" && newest_in_flight.is_none(),
        "blocked": blocker,
        "gaps": gaps,
        "evidence": evidence_of(
            &landed, &checks, newest_landing, anchor, facts.deployments.first(), &tasks,
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
            commit: row.and_then(|row| row["candidate"].as_str()).map(str::to_string),
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
            if landed.get(&number).is_none_or(|landing| at_ms > landing.at_ms) {
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
                if landed.get(&number).is_none_or(|landing| at_ms > landing.at_ms) {
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
                gates.last().and_then(|gate| {
                    gate["reason"].as_str().map(str::trim).map(str::to_string)
                })
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
/// verification, and the deploy the live claim stands on.
fn evidence_of(
    landed: &BTreeMap<u32, Landing>,
    checks: &[Value],
    newest_landing: Option<&Landing>,
    anchor: Option<&Value>,
    last_deploy: Option<&Value>,
    tasks: &[&Value],
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
        "deploy": if anchor.is_some() && newest_landing.is_some()
            && anchor.is_some_and(|anchor| {
                anchor["at_ms"].as_i64().unwrap_or(i64::MAX)
                    > newest_landing.map_or(i64::MIN, |landing| landing.at_ms)
            })
        {
            anchor.cloned().unwrap_or(Value::Null)
        } else {
            Value::Null
        },
        "last_deploy": last_deploy.cloned().unwrap_or(Value::Null),
        "newest_landing_at_ms": newest_landing.map_or(Value::Null, |landing| json!(landing.at_ms)),
    })
}

fn links_json(links: &[Link]) -> Vec<Value> {
    links
        .iter()
        .map(|link| {
            json!({"url": link.url, "title": link.title, "number": link.number})
        })
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
        Facts, Spec, build, derive, fallback_specs, parse_mapping, read_mapping, shipped_by_deploy,
    };

    fn spec(id: &str, objectives: &[u32]) -> Spec {
        Spec {
            id: id.to_string(),
            title: format!("Feature {id}"),
            summary: None,
            objectives: objectives.to_vec(),
            issues: Vec::new(),
            prs: Vec::new(),
        }
    }

    fn mission() -> serde_json::Value {
        json!({
            "id": 4, "text": "Spawn RSI", "state": "active", "sources": ["/m/missions/spawn.git"],
            "gate": "/g", "accept": "gate", "halted": null, "baseline": "b",
            "features_file": null, "backlog": "/backlog",
            "objectives": [
                {"id": 1, "title": "One", "status": "open", "landings": [], "solved_on_base": null},
                {"id": 2, "title": "Two", "status": "solved", "landings": [{"task": 20, "at_ms": 100}],
                 "solved_on_base": null},
                {"id": 3, "title": "Three", "status": "solved", "landings": [{"task": 30, "at_ms": 200}],
                 "solved_on_base": null},
                {"id": 4, "title": "Four", "status": "solved", "landings": [{"task": 40, "at_ms": 300}],
                 "solved_on_base": null},
                {"id": 5, "title": "Five", "status": "open", "landings": [], "solved_on_base": null},
            ],
            "tasks": [
                {"id": 20, "state": "landed", "kind": "ephemeral", "agent_id": "a20",
                 "session_id": "s20", "objective": 2, "revisions": 0, "candidate": "c2",
                 "base": "b2", "updated_at_ms": 100, "review_note": null, "gates": []},
                {"id": 30, "state": "landed", "kind": "ephemeral", "agent_id": "a30",
                 "session_id": "s30", "objective": 3, "revisions": 0, "candidate": "c3",
                 "base": "b3", "updated_at_ms": 200, "review_note": null, "gates": []},
                {"id": 40, "state": "landed", "kind": "ephemeral", "agent_id": "a40",
                 "session_id": "s40", "objective": 4, "revisions": 0, "candidate": "c4",
                 "base": "b4", "updated_at_ms": 300, "review_note": null, "gates": []},
                {"id": 50, "state": "accepted", "kind": "ephemeral", "agent_id": "a50",
                 "session_id": "s50", "objective": 5, "revisions": 1, "candidate": null,
                 "base": null, "updated_at_ms": 400, "review_note": "ok", "gates": []},
            ],
        })
    }

    fn facts_of<'a>(
        landings: &'a [serde_json::Value],
        verifications: &'a [serde_json::Value],
        deployments: &'a [serde_json::Value],
        blockers: &'a [serde_json::Value],
    ) -> Facts<'a> {
        Facts {
            landings,
            verifications,
            deployments,
            blockers,
        }
    }

    /// The four states are four different pieces of evidence: an accepted
    /// task is not implemented, a landing is not verified, a pass is not
    /// live, and a deploy after the landing is.
    #[test]
    fn planned_implemented_verified_and_live_never_collapse() {
        let landings = vec![
            json!({"mission": 4, "objective": 2, "task": 20, "landed_ms": 100, "event": 102, "brief": ""}),
            json!({"mission": 4, "objective": 3, "task": 30, "landed_ms": 200, "event": 202, "brief": ""}),
            json!({"mission": 4, "objective": 4, "task": 40, "landed_ms": 300, "event": 302, "brief": ""}),
        ];
        let verifications = vec![
            json!({"mission": 4, "objective": 3, "result": "pass", "by": "lead", "how": "looked",
                   "at_ms": 250, "event": 252}),
            json!({"mission": 4, "objective": 4, "result": "pass", "by": "lead", "how": "looked",
                   "at_ms": 350, "event": 352}),
        ];
        let deployments = vec![
            json!({"event": 400, "at_ms": 400, "ok": true, "head": "d".repeat(40), "by": "landing",
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
        let landed = derive(&spec("i", &[2]), &mission(), &facts);
        assert_eq!(landed["state"], "implemented");
        assert_eq!(landed["verified"], false);
        assert_eq!(landed["live"], false);
        let verified = derive(&spec("v", &[3]), &mission(), &facts);
        assert_eq!(verified["state"], "verified");
        assert_eq!(verified["live"], false, "a pass is not a deploy");
        let live = derive(&spec("l", &[4]), &mission(), &facts);
        assert_eq!(live["state"], "live");
        assert_eq!(live["verified"], true);
        assert_eq!(live["evidence"]["deploy"]["event"], 400);
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

    /// A sent-back task with nothing newer in flight is blocked, and the
    /// note is the reason.
    #[test]
    fn a_sent_back_task_is_blocked() {
        let mut row = mission();
        row["tasks"] = json!([
            {"id": 70, "state": "revise", "kind": "ephemeral", "agent_id": "a70",
             "session_id": "s70", "objective": 1, "revisions": 1, "candidate": null,
             "base": null, "updated_at_ms": 700, "review_note": "add the test", "gates": []}
        ]);
        let facts = facts_of(&[], &[], &[], &[]);
        let feature = derive(&spec("one", &[1]), &row, &facts);
        assert_eq!(feature["state"], "planned");
        assert_eq!(feature["blocked"]["kind"], "sent_back");
        assert_eq!(feature["blocked"]["reason"], "add the test");
        assert_eq!(feature["unowned"], true);
    }

    /// A mapping is identities and links only; a typo, a duplicate or a
    /// non-http link is refused rather than silently dropped.
    #[test]
    fn a_mapping_is_checked_and_holds_no_status() {
        let good = json!({"features": [
            {"id": "feature-map", "title": "Feature map",
             "summary": "capabilities", "objectives": [488, 489],
             "issues": [{"url": "https://github.com/o/r/issues/63", "title": "map", "number": 63}],
             "prs": [{"url": "https://github.com/o/r/pull/69"}]},
            {"id": "other", "title": "Other", "objectives": [490]}
        ]});
        let specs = parse_mapping(&good).expect("a good mapping");
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].objectives, vec![488, 489]);
        assert_eq!(specs[0].issues[0].number, Some(63));
        assert_eq!(specs[1].prs.len(), 0);
        for bad in [
            json!({"features": [{"id": "a", "title": "A", "status": "live"}]}),
            json!({"features": [{"id": "a", "title": "A", "objectives": [1]},
                                {"id": "a", "title": "B", "objectives": [2]}]}),
            json!({"features": [{"id": "a", "title": "A", "objectives": [1]},
                                {"id": "b", "title": "B", "objectives": [1]}]}),
            json!({"features": [{"id": "A", "title": "A"}]}),
            json!({"features": [{"id": "a", "title": "A", "issues": [{"url": "file:///x"}]}]}),
            json!({"features": [{"id": "a", "title": "A", "objectives": ["1"]}]}),
            json!({"features": [{"id": "a", "title": "A", "objectives": [1, 1]}]}),
            json!({"features": [{"id": "a", "title": ""}]}),
            json!({"features": "none"}),
            json!({"features": [], "extra": true}),
        ] {
            assert!(parse_mapping(&bad).is_err(), "refused: {bad}");
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
            r#"{"features":[{"id":"x","title":"X","objectives":[1]}]}"#,
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
        let _ = read_mapping(Path::new("/definitely/not/here"));
    }

    /// The whole answer carries the seconds and the deploy it stands on.
    #[test]
    fn the_answer_says_which_deploy_it_stands_on() {
        let deployments = vec![json!({"event": 9, "at_ms": 90, "ok": true,
                                      "head": "d".repeat(40), "by": "landing", "reason": ""})];
        let answer = build(&[mission()], &json!({"landings": [], "verifications": []}),
                           &json!(deployments), &json!([]), 1000);
        assert_eq!(answer["generated_at_ms"], 1000);
        assert_eq!(answer["live_anchor"]["event"], 9);
        assert_eq!(answer["missions"][0]["counts"]["features"], 5);
        // The accepted, ungated task test below is a different mission.
        let mut ungated = mission();
        ungated["gate"] = Value::Null;
        let answer = build(&[ungated], &json!({"landings": [], "verifications": []}),
                           &json!([]), &json!([]), 1000);
        assert!(answer["missions"][0]["features"]
            .as_array()
            .unwrap()
            .iter()
            .any(|feature| feature["gaps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|gap| gap["kind"] == "accepted_without_landing")));
    }
}
