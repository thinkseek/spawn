//! `spawn prov durability` (PV4, decisions D4 and B6): a shadow report of
//! what happened to landed work after it landed. It only reads; it never
//! changes routing (`calibrate_preset` and grading are not touched: the
//! switch into them is PV5, on D4's trigger).
//!
//! The rules (decisions B6, binding):
//! 1. **The unit is the task**: a liability is charged to the task whose
//!    landing introduced the content, and through the `preset_id` captured
//!    in its `land` Change to that preset. Never to an agent's name, a
//!    reviewer or a later editor.
//! 2. **What counts**: only a `supersedes` edge of a landing with `reason:
//!    "defect"` and evidence of level 1 (`level1_revert`: a dispatcher's
//!    `reverts: task N` whose landing restores N's paths to their `before`)
//!    or level 2 (`level2_bisect`). A dispatcher's `fixes: task N`
//!    (`dispatcher_fixes`) is reported in `fixes_reported` and never
//!    counted; an unverified revert neither.
//! 3. **Window (D4)**: the edge's landing is among the 50 landings after the
//!    task's landing (ordered by `prov_changes.id`, in the same domain) and
//!    within 14 days of it (`at_ms`), whichever ends first. **At most one
//!    liability per task**, whatever the number of defects or paths.
//! 4. **Never a liability**: `refactor`, `format`, `planned`, `requirement`;
//!    a `redacts` edge (only `supersedes` edges are read); a fix inside the
//!    same task (the same `change_id`) before landing, which is no edge.
//! 5. **A composed landing**: the liability follows each path's `origin`
//!    to the task of the revision its content came from; a path that is the
//!    landing's own (no origin) is its task's, through its second parent.
//!    When that names more than one task, or a revision with no task, the
//!    event is `unattributed` and nobody is charged.
//! 6. No survival score, and no count of lines, files or Changes in any
//!    column.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;

use serde_json::{Value, json};
use spawn_postgres::PostgresStore;
use spawn_postgres::provenance::{
    ProvChangeRecord, ProvEntryClass, ProvEntryRecord, ProvSupersedes,
};

use super::index::{
    EVIDENCE_FIXES, EVIDENCE_LEVEL1, EVIDENCE_LEVEL2, EVIDENCE_UNVERIFIED, REASON_DEFECT,
};
use crate::Flags;

/// D4's window, by default: 50 landings.
pub(crate) const WINDOW_LANDINGS: usize = 50;

/// D4's window, by default: 14 days.
pub(crate) const WINDOW_DAYS: u64 = 14;

const DAY_MS: i64 = 86_400_000;

/// The usage of `spawn prov durability`.
const USAGE: &str = "usage: spawn prov durability [--source <source>] [--window-landings 50] \
     [--window-days 14] [--json]";

/// D4's window: the landings and days after a task's landing in which a
/// confirmed defect is charged to it, whichever ends first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Window {
    pub(crate) landings: usize,
    pub(crate) days: u64,
}

impl Default for Window {
    fn default() -> Self {
        Self {
            landings: WINDOW_LANDINGS,
            days: WINDOW_DAYS,
        }
    }
}

/// `spawn prov durability ...`.
pub(crate) fn command(rest: &[String]) -> Result<i32, String> {
    let flags = Flags::parse_with_switches(rest, &["json"])
        .map_err(|error| format!("{error}\n{USAGE}"))?;
    if let Some(name) = flags
        .names()
        .find(|name| !matches!(*name, "source" | "window-landings" | "window-days"))
    {
        return Err(format!("unknown flag --{name}\n{USAGE}"));
    }
    let positive = |name: &str, default: u64| -> Result<u64, String> {
        match flags.get(name) {
            None => Ok(default),
            Some(text) => text
                .parse::<u64>()
                .ok()
                .filter(|value| *value > 0)
                .ok_or_else(|| format!("--{name} must be a positive number, not {text}")),
        }
    };
    let window = Window {
        landings: usize::try_from(positive("window-landings", WINDOW_LANDINGS as u64)?)
            .map_err(|_| "--window-landings is too large".to_string())?,
        days: positive("window-days", WINDOW_DAYS)?,
    };
    let domain = flags.get("source").map(|source| super::index::domain_of(&source));
    let store = crate::open_store()?;
    let report = report(&store, domain.as_deref(), window)?;
    if flags.switch("json") {
        println!("{report}");
    } else {
        print!("{}", text(&report));
    }
    Ok(0)
}

/// What the report reads from the store.
#[derive(Debug, Default)]
pub(crate) struct Input {
    /// Every `land` Change with a task, by id.
    pub(crate) lands: Vec<ProvChangeRecord>,
    /// Every `supersedes` edge of a landing.
    pub(crate) supersedes: Vec<ProvSupersedes>,
    /// The entries of each superseded landing.
    pub(crate) entries: HashMap<i64, Vec<ProvEntryRecord>>,
    /// The Changes those entries' origins name.
    pub(crate) origins: HashMap<i64, ProvChangeRecord>,
}

/// The report over `domain` (every Git domain when `None`).
pub(crate) fn report(
    store: &PostgresStore,
    domain: Option<&str>,
    window: Window,
) -> Result<Value, String> {
    let error = |error: spawn_core::port::StoreError| error.to_string();
    let lands = store.prov_land_changes(domain).map_err(error)?;
    let supersedes = store.prov_land_supersedes(domain).map_err(error)?;
    let targets: BTreeSet<i64> = supersedes
        .iter()
        .filter_map(|edge| target_task(&edge.edge.detail))
        .collect();
    let ids: Vec<i64> = lands
        .iter()
        .filter(|land| land.task_id.is_some_and(|task| targets.contains(&task)))
        .map(|land| land.id)
        .collect();
    let mut entries: HashMap<i64, Vec<ProvEntryRecord>> = HashMap::new();
    for entry in store.prov_entries_of(&ids).map_err(error)? {
        entries.entry(entry.change).or_default().push(entry);
    }
    let mut origin_ids: Vec<i64> = entries
        .values()
        .flatten()
        .filter_map(|entry| entry.origin)
        .collect();
    origin_ids.sort_unstable();
    origin_ids.dedup();
    let origins = store.prov_changes_by_ids(&origin_ids).map_err(error)?;
    let input = Input {
        lands,
        supersedes,
        entries,
        origins,
    };
    let mut report = tally(&input, window);
    report["domain"] = json!(domain);
    Ok(report)
}

/// The task an edge's detail names (`target_task`).
fn target_task(detail: &Value) -> Option<i64> {
    detail.get("target_task").and_then(Value::as_i64)
}

/// One preset's row.
#[derive(Debug, Default)]
struct Row {
    landed: BTreeSet<i64>,
    defects: BTreeSet<i64>,
    reverted: BTreeSet<i64>,
    fixes: BTreeSet<i64>,
}

/// Who an event is charged to.
enum Charge {
    Task(i64),
    /// Nobody: the tasks it could be (`None`: a revision with no task).
    Unattributed(Vec<Option<i64>>),
}

/// The task the liability of task `target`'s landings `lands` goes to
/// (rule 5): each path's origin's task, a path with no origin the landing's
/// own (`target`); one task, or nobody.
fn charge(input: &Input, target: i64, lands: &[&ProvChangeRecord]) -> Charge {
    let mut tasks: BTreeSet<Option<i64>> = BTreeSet::new();
    for land in lands {
        for entry in input.entries.get(&land.id).into_iter().flatten() {
            if entry.class == ProvEntryClass::Generated {
                continue;
            }
            match entry.origin {
                Some(origin) => {
                    tasks.insert(input.origins.get(&origin).and_then(|change| change.task_id));
                }
                None => {
                    tasks.insert(Some(target));
                }
            }
        }
    }
    match tasks.len() {
        0 => Charge::Task(target),
        1 => match tasks.into_iter().next().flatten() {
            Some(task) => Charge::Task(task),
            None => Charge::Unattributed(vec![None]),
        },
        _ => Charge::Unattributed(tasks.into_iter().collect()),
    }
}

/// Each task's landings and each landing's place in its domain (the order
/// of `prov_changes.id`).
struct Landings<'a> {
    of_task: HashMap<i64, Vec<&'a ProvChangeRecord>>,
    place: HashMap<i64, usize>,
}

impl<'a> Landings<'a> {
    fn new(lands: &'a [ProvChangeRecord]) -> Self {
        let mut of_task: HashMap<i64, Vec<&ProvChangeRecord>> = HashMap::new();
        let mut place = HashMap::new();
        let mut per_domain: HashMap<&str, usize> = HashMap::new();
        for land in lands {
            let counter = per_domain.entry(land.domain.as_str()).or_insert(0);
            place.insert(land.id, *counter);
            *counter += 1;
            if let Some(task) = land.task_id {
                of_task.entry(task).or_default().push(land);
            }
        }
        Self { of_task, place }
    }

    /// A task's preset: the one captured in its first landing that has one.
    fn preset_of(&self, task: i64) -> Option<String> {
        self.of_task
            .get(&task)
            .and_then(|lands| lands.iter().find_map(|land| land.preset_id.clone()))
    }
}

/// An edge that is scored or reported: a confirmed defect or a dispatcher's
/// `fixes:`, inside the window.
struct Event<'a> {
    confirmed: bool,
    level1: bool,
    target: i64,
    /// The target's landings in the edge's domain before the edge.
    lands: Vec<&'a ProvChangeRecord>,
    shown: Value,
}

/// What `edge` of the landing `src` is (rules 2 to 4): an [`Event`], or why
/// it is neither scored nor reported.
fn event<'a>(
    landings: &Landings<'a>,
    edge: &spawn_postgres::provenance::ProvEdgeRecord,
    src: &ProvChangeRecord,
    window: Window,
) -> Result<Event<'a>, &'static str> {
    let target = target_task(&edge.detail).ok_or("no_target_task")?;
    if src.task_id == Some(target) {
        return Err("same_task");
    }
    let reason = edge.detail.get("reason").and_then(Value::as_str);
    let evidence = edge.detail.get("evidence").and_then(Value::as_str);
    let confirmed = reason == Some(REASON_DEFECT)
        && matches!(evidence, Some(EVIDENCE_LEVEL1 | EVIDENCE_LEVEL2));
    let reported = reason == Some(REASON_DEFECT) && evidence == Some(EVIDENCE_FIXES);
    if !confirmed && !reported {
        return Err(match (reason, evidence) {
            (Some(REASON_DEFECT), Some(EVIDENCE_UNVERIFIED)) => EVIDENCE_UNVERIFIED,
            (Some(REASON_DEFECT), _) => "defect_without_evidence",
            (Some("refactor"), _) => "refactor",
            (Some("format"), _) => "format",
            (Some("planned"), _) => "planned",
            (Some("requirement"), _) => "requirement",
            (Some(_), _) => "other_reason",
            (None, _) => "no_reason",
        });
    }
    let lands: Vec<&ProvChangeRecord> = landings
        .of_task
        .get(&target)
        .into_iter()
        .flatten()
        .copied()
        .filter(|land| land.domain == src.domain && land.id < src.id)
        .collect();
    let first = lands.first().ok_or("target_not_landed_before")?;
    let after = landings
        .place
        .get(&src.id)
        .zip(landings.place.get(&first.id))
        .map_or(usize::MAX, |(src, first)| src.saturating_sub(*first));
    let window_ms = i64::try_from(window.days)
        .unwrap_or(i64::MAX / DAY_MS)
        .saturating_mul(DAY_MS);
    let ms_after = src.at_ms.saturating_sub(first.at_ms);
    if after > window.landings || ms_after > window_ms {
        return Err("out_of_window");
    }
    Ok(Event {
        confirmed,
        level1: evidence == Some(EVIDENCE_LEVEL1),
        target,
        lands,
        shown: json!({
            "edge": edge.id,
            "target_task": target,
            "by_task": src.task_id,
            "by_change": src.change_id,
            "reason": reason,
            "evidence": evidence,
            "landings_after": after,
            "ms_after": ms_after,
        }),
    })
}

/// The preset a liability charged to `task` goes to: the one its landing
/// captured; for a task that never landed itself (its revision came in
/// through another's landing, `lands`), the one its revision captured.
fn charged_preset(
    input: &Input,
    landings: &Landings,
    task: i64,
    lands: &[&ProvChangeRecord],
) -> Option<String> {
    landings.preset_of(task).or_else(|| {
        lands.iter().find_map(|land| {
            input
                .entries
                .get(&land.id)
                .into_iter()
                .flatten()
                .filter_map(|entry| input.origins.get(&entry.origin?))
                .find(|change| change.task_id == Some(task))
                .and_then(|change| change.preset_id.clone())
        })
    })
}

/// The report from what was read (module doc for the rules).
pub(crate) fn tally(input: &Input, window: Window) -> Value {
    let landings = Landings::new(&input.lands);
    let mut rows: BTreeMap<Option<String>, Row> = BTreeMap::new();
    for task in landings.of_task.keys() {
        rows.entry(landings.preset_of(*task))
            .or_default()
            .landed
            .insert(*task);
    }
    let mut not_scored: BTreeMap<&str, u64> = BTreeMap::new();
    let mut unattributed = Vec::new();
    let mut defects = Vec::new();
    let mut reported = Vec::new();
    for ProvSupersedes { edge, src } in &input.supersedes {
        let event = match event(&landings, edge, src, window) {
            Ok(event) => event,
            Err(why) => {
                *not_scored.entry(why).or_insert(0) += 1;
                continue;
            }
        };
        let mut shown = event.shown;
        match charge(input, event.target, &event.lands) {
            Charge::Unattributed(candidates) => {
                shown["candidates"] = json!(candidates);
                unattributed.push(shown);
            }
            Charge::Task(task) => {
                let preset = charged_preset(input, &landings, task, &event.lands);
                shown["charged_task"] = json!(task);
                shown["preset_id"] = json!(preset);
                let row = rows.entry(preset).or_default();
                if event.confirmed {
                    row.defects.insert(task);
                    if event.level1 {
                        row.reverted.insert(task);
                    }
                    defects.push(shown);
                } else {
                    row.fixes.insert(task);
                    reported.push(shown);
                }
            }
        }
    }
    let presets: Vec<Value> = rows
        .into_iter()
        .map(|(preset, row)| {
            json!({
                "preset_id": preset,
                "landed_tasks": row.landed.len(),
                "confirmed_defects": row.defects.len(),
                "reverted_tasks": row.reverted.len(),
                "fixes_reported": row.fixes.len(),
            })
        })
        .collect();
    json!({
        "shadow": true,
        "window": {"landings": window.landings, "days": window.days},
        "landings": input.lands.len(),
        "presets": presets,
        "unattributed": unattributed.len(),
        "unattributed_events": unattributed,
        "defects": defects,
        "fixes": reported,
        "not_scored": not_scored,
    })
}

/// The report for a person: one line per preset, then the totals.
pub(crate) fn text(report: &Value) -> String {
    let mut out = format!(
        "durability (shadow only, never routes): window {} landings or {} days, {} landings\n",
        report["window"]["landings"], report["window"]["days"], report["landings"]
    );
    let _ = writeln!(
        out,
        "{:<32} {:>8} {:>9} {:>9} {:>7}",
        "preset_id", "landed", "defects", "reverted", "fixes"
    );
    for row in report["presets"].as_array().into_iter().flatten() {
        let _ = writeln!(
            out,
            "{:<32} {:>8} {:>9} {:>9} {:>7}",
            row["preset_id"].as_str().unwrap_or("(none)"),
            row["landed_tasks"].to_string(),
            row["confirmed_defects"].to_string(),
            row["reverted_tasks"].to_string(),
            row["fixes_reported"].to_string(),
        );
    }
    let _ = writeln!(out, "unattributed: {}", report["unattributed"]);
    out
}
