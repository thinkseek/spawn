//! `GET /api/core-lines` (objective 084): the four core lines the owner
//! steers by (performance, storage, observability and recoverability), each
//! one north star and its guardrails, every figure with its value, the range
//! the ranges file gives it and whether the value lies in it.
//!
//! The answer:
//!
//! ```text
//! {
//!   "lines": [{
//!     "line": "storage",
//!     "north_star": {key, name, unit, value, range, state, trend},
//!     "guardrails": [{key, name, unit, value, range, state, trend}, ...]
//!   }, ...],                     // performance, storage, observability, recoverability
//!   "ranges_file": "/ws/core-lines.json",
//!   "ranges_error": null | "why the file could not be read",
//!   "ignored_ranges": [{"key", "reason"}]   // sorted by key
//! }
//! ```
//!
//! `unit` is one of bytes, bytes_per_day, days, count, ratio, ms, share, usd.
//! `range` is the file's entry for the figure (`{min?, max?}`, numbers) or
//! null; `state` is `in` when the value lies within it (bounds included),
//! `out` when it does not, `no_range` without a range and `not_collected`
//! without a value. `trend` (objective 090) is the figure's last 7 days:
//! one `{day, value}` for each UTC day (today and the 6 before) that has a
//! `host.core_lines` sample carrying the figure, that day's last such
//! sample, oldest first; empty without one. The loop records those samples
//! at its start and hourly ([`sample_values`], crate core_lines_sample.rs),
//! and the answer reads only the window's, through their own index
//! (`spawn_events_host_core_lines`).
//!
//! The performance figures (objective 086) are the pages': the Scorecard's
//! fleet scope over its default 7 days (report to live, the gate time's
//! median, the share of gate time that decided nothing, first pass) and
//! `/api/cost?window=7d`'s dollars per landing ([`performance`]), kept for a
//! minute so a Home poll does not compute them again at every request. The
//! storage figures are health.storage's on `/api/fleet`, read the same way
//! ([`super::storage::report`]). The observability line's guardrails
//! (objective 088) are the share of the 7-day cost window's tokens no price
//! covers (`/api/cost?window=7d`'s total, kept with the performance figures:
//! unpriced_tokens over its input, output, cache write and cache read
//! tokens; null with none), health's `human_per_agent_hour` on
//! `/api/fleet`, and how many figures of this same answer have no value,
//! itself left out. The recoverability guardrails (objective 087,
//! [`recoverability`]) are the ages of the newest database copy and bundle
//! (health.backups on `/api/fleet`), the age of the oldest nightly judging
//! and how many sources the nightly holds red (its fact files,
//! [`nightly_facts`]). The other lines' north stars are not collected yet;
//! later objectives add their figures to this frame.
//!
//! No range lives in code: they come only from the ranges file,
//! `SPAWN_CORE_LINES_FILE`, by default `<workspace root>/core-lines.json`,
//! shaped `{"ranges": {<figure key>: {"min"?: number, "max"?: number}}}`,
//! read at every request, so an edit shows without a restart. An entry whose
//! key names no figure, or with no number min or max, is left out and listed
//! in `ignored_ranges`. A missing file is no ranges and no error; a file that
//! is not such JSON is no ranges and a `ranges_error`. Nothing but `spawn
//! serve` and the loop's alert step ([`reading`], `guardrail_out_of_range`)
//! read the file, and no gate, integrator or deploy step reads the answer:
//! monitoring only.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

/// The file's name under the workspace root, when `SPAWN_CORE_LINES_FILE`
/// names none.
const RANGES_FILE: &str = "core-lines.json";

/// One figure before its range: its key, name, unit and value.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Figure {
    pub(super) key: &'static str,
    pub(super) name: &'static str,
    pub(super) unit: &'static str,
    pub(super) value: Value,
}

impl Figure {
    fn new(key: &'static str, name: &'static str, unit: &'static str, value: Value) -> Self {
        Figure {
            key,
            name,
            unit,
            value,
        }
    }

    fn not_collected(key: &'static str, name: &'static str, unit: &'static str) -> Self {
        Figure::new(key, name, unit, Value::Null)
    }
}

/// One core line: its name, north star and guardrails.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Line {
    pub(super) name: &'static str,
    pub(super) north_star: Figure,
    pub(super) guardrails: Vec<Figure>,
}

/// A range: either bound may be missing, never both.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Range {
    min: Option<f64>,
    max: Option<f64>,
}

impl Range {
    fn json(self) -> Value {
        let mut object = Map::new();
        if let Some(min) = self.min {
            object.insert("min".into(), number(min));
        }
        if let Some(max) = self.max {
            object.insert("max".into(), number(max));
        }
        Value::Object(object)
    }

    fn holds(self, value: f64) -> bool {
        self.min.is_none_or(|min| value >= min) && self.max.is_none_or(|max| value <= max)
    }
}

/// A bound as JSON: whole when it is whole (as the file most likely wrote
/// it), else as it is.
fn number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9.0e15 {
        // A whole number this small converts exactly.
        #[allow(clippy::cast_possible_truncation)]
        let whole = value as i64;
        json!(whole)
    } else {
        json!(value)
    }
}

/// What the ranges file gave: the ranges by key, the entries left out with
/// why, and why the file could not be read, if it could not.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Ranges {
    pub(super) bounds: BTreeMap<String, Range>,
    pub(super) ignored: Vec<(String, &'static str)>,
    pub(super) error: Option<String>,
}

/// The ranges in `text` (the file's contents; `None` for no file), for the
/// figures named `known`.
pub(super) fn parse_ranges(text: Option<&str>, known: &[&str]) -> Ranges {
    let Some(text) = text else {
        return Ranges::default();
    };
    let failed = |error: String| Ranges {
        error: Some(error),
        ..Ranges::default()
    };
    let value: Value = match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => return failed(error.to_string()),
    };
    let Some(entries) = value.get("ranges").and_then(Value::as_object) else {
        return failed(r#"expected {"ranges": {<figure key>: {"min"?, "max"?}}}"#.to_string());
    };
    let mut ranges = Ranges::default();
    for (key, entry) in entries {
        if !known.contains(&key.as_str()) {
            ranges.ignored.push((key.clone(), "unknown figure"));
            continue;
        }
        let bound = |name: &str| entry.get(name).and_then(Value::as_f64);
        let range = Range {
            min: bound("min"),
            max: bound("max"),
        };
        if range.min.is_none() && range.max.is_none() {
            ranges
                .ignored
                .push((key.clone(), "needs a number min or max"));
            continue;
        }
        ranges.bounds.insert(key.clone(), range);
    }
    ranges.ignored.sort();
    ranges
}

/// One UTC day, in milliseconds.
const DAY_MS: i64 = 86_400_000;

/// How many UTC days a trend covers: today and the 6 before.
const TREND_DAYS: i64 = 7;

/// Where the trend's window starts at `now_ms`: the UTC midnight 6 days
/// before today's.
pub(crate) fn trend_since(now_ms: i64) -> i64 {
    now_ms.div_euclid(DAY_MS) * DAY_MS - (TREND_DAYS - 1) * DAY_MS
}

/// Every figure's trend from the `host.core_lines` samples (`[{at_ms,
/// values}]`, oldest first, as the store reads them), at `now_ms`: for each
/// figure, one `{day, value}` per UTC day of the window that has a sample
/// carrying it (a number), that day's last such sample, oldest first. A
/// sample outside the window, or from after `now_ms`'s day, is left out.
pub(crate) fn trends(samples: &Value, now_ms: i64) -> BTreeMap<String, Vec<Value>> {
    let since = trend_since(now_ms);
    let until = since + TREND_DAYS * DAY_MS;
    // figure -> day -> (at_ms, value); a later sample replaces an earlier.
    let mut days: BTreeMap<String, BTreeMap<String, (i64, Value)>> = BTreeMap::new();
    for sample in samples.as_array().into_iter().flatten() {
        let Some(at) = sample["at_ms"].as_i64() else {
            continue;
        };
        if at < since || at >= until {
            continue;
        }
        let day = crate::scorecard::time::day_of(at);
        for (key, value) in sample["values"].as_object().into_iter().flatten() {
            if !value.is_number() {
                continue;
            }
            let slot = days.entry(key.clone()).or_default();
            match slot.get(&day) {
                Some((kept, _)) if *kept > at => {}
                _ => {
                    slot.insert(day.clone(), (at, value.clone()));
                }
            }
        }
    }
    days.into_iter()
        .map(|(key, by_day)| {
            let points = by_day
                .into_iter()
                .map(|(day, (_, value))| json!({"day": day, "value": value}))
                .collect();
            (key, points)
        })
        .collect()
}

/// A figure as the answer gives it: with its range and its state.
pub(super) fn figure_json(figure: &Figure, range: Option<Range>) -> Value {
    let state = match (figure.value.as_f64(), range) {
        (None, _) => "not_collected",
        (Some(_), None) => "no_range",
        (Some(value), Some(range)) if range.holds(value) => "in",
        (Some(_), Some(_)) => "out",
    };
    json!({
        "key": figure.key,
        "name": figure.name,
        "unit": figure.unit,
        "value": figure.value,
        "range": range.map_or(Value::Null, Range::json),
        "state": state,
    })
}

/// The nightly's fact files under `<workspace root>/nightly`, as
/// scripts/rsi/nightly.sh writes them (`<source>.json`; a source's name may
/// hold dots): every non-hidden `*.json` there, read and parsed. The
/// scenario logs beside them (`<source>.<scenario>.log`), the lock
/// (`.<source>.lock`) and the run files (`.run-*`) are not facts; a file
/// that cannot be read or parsed as an object is passed over. `None`
/// without the directory.
pub(super) fn nightly_facts(dir: Option<&Path>) -> Option<Vec<Value>> {
    let entries = std::fs::read_dir(dir?).ok()?;
    let mut facts = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.starts_with('.') || Path::new(name).extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        if let Ok(fact @ Value::Object(_)) = serde_json::from_str::<Value>(&text) {
            facts.push(fact);
        }
    }
    Some(facts)
}

/// The recoverability guardrails at `now_ms`, from `backups` (health.backups
/// on `/api/fleet`: `{dump, bundle}`, each `{at_ms, ...}` or null) and the
/// nightly's fact files (`None`: no directory):
/// - the age of the newest database copy and of the newest bundle, null
///   without the record;
/// - the age of the oldest nightly judging (`judged_at`), a fact with none
///   passed over; null without one;
/// - the facts whose `red_since` is set (a non-empty string, as the
///   integrator's `nightly_red` holds a source's landings); null without a
///   fact.
pub(super) fn recoverability(backups: &Value, facts: Option<&[Value]>, now_ms: i64) -> Vec<Figure> {
    let age =
        |at: Option<i64>| at.map_or(Value::Null, |at| json!(now_ms.saturating_sub(at).max(0)));
    let facts = facts.filter(|facts| !facts.is_empty());
    let oldest = facts.and_then(|facts| {
        facts
            .iter()
            .filter_map(|fact| fact["judged_at"].as_i64())
            .min()
    });
    let red = facts.map_or(Value::Null, |facts| {
        json!(
            facts
                .iter()
                .filter(|fact| fact["red_since"]
                    .as_str()
                    .is_some_and(|since| !since.is_empty()))
                .count()
        )
    });
    vec![
        Figure::new(
            "recoverability.cloud_dump_age",
            "Age of the newest database copy",
            "ms",
            age(backups["dump"]["at_ms"].as_i64()),
        ),
        Figure::new(
            "recoverability.cloud_bundle_age",
            "Age of the newest bundle",
            "ms",
            age(backups["bundle"]["at_ms"].as_i64()),
        ),
        Figure::new(
            "recoverability.nightly_age",
            "Age of the oldest nightly judging",
            "ms",
            age(oldest),
        ),
        Figure::new(
            "recoverability.nightly_red",
            "Nightly sources held red",
            "count",
            red,
        ),
    ]
}

/// How long the performance figures are kept: a Home poll must not compute
/// the 7-day scorecard and cost window again at every request.
const PERFORMANCE_KEPT_MS: i64 = 60_000;

/// The performance figures of the last read and the 7-day cost window's
/// `total` (objective 088), and when.
static PERFORMANCE: std::sync::Mutex<Option<(i64, Value, Value)>> = std::sync::Mutex::new(None);

/// The performance line's values (objective 086), from the Scorecard's
/// answer over its default 7 days and `/api/cost?window=7d`'s: the fleet
/// scope's `report_to_live` value, `gate_time` value (the median),
/// `gate_time`'s `undecided_ms` over its `total_ms` (null when the total is
/// 0), `first_pass` value, and the cost window's `usd_per_landing`. A value
/// the page does not have is null.
pub(super) fn performance(scorecard: &Value, cost: &Value) -> Value {
    let fleet = scorecard["scopes"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|scope| scope["scope"] == "fleet")
        .map_or(Value::Null, |scope| scope["dimensions"].clone());
    let gate = &fleet["gate_time"];
    let decided_nothing = match (gate["undecided_ms"].as_f64(), gate["total_ms"].as_f64()) {
        (Some(undecided), Some(total)) if total > 0.0 => json!(undecided / total),
        _ => Value::Null,
    };
    json!({
        "report_to_live": fleet["report_to_live"]["value"],
        "gate_time": gate["value"],
        "decided_nothing": decided_nothing,
        "first_pass": fleet["first_pass"]["value"],
        "usd_per_landing": cost["usd_per_landing"],
    })
}

/// The performance values and the 7-day cost window's `total`, kept for
/// [`PERFORMANCE_KEPT_MS`]: computed from the 7-day scorecard and cost
/// window as the pages compute them. A part that cannot be read counts as
/// not collected.
fn performance_now(store: &spawn_postgres::PostgresStore, now_ms: i64) -> (Value, Value) {
    let mut kept = PERFORMANCE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((_, values, cost_total)) = kept
        .as_ref()
        .filter(|(at, _, _)| (0..PERFORMANCE_KEPT_MS).contains(&now_ms.saturating_sub(*at)))
    {
        return (values.clone(), cost_total.clone());
    }
    let root = crate::env_value("SPAWN_WORKSPACE_ROOT");
    let scorecard = crate::scorecard::measure(
        store,
        root.as_deref().map(std::path::Path::new),
        &crate::scorecard::Query::default(),
    )
    .unwrap_or(Value::Null);
    let cost = crate::cost::window_of(Some("7d"))
        .and_then(|(name, length_ms)| crate::cost::window(store, name, length_ms))
        .unwrap_or(Value::Null);
    let values = performance(&scorecard, &cost);
    let cost_total = cost["total"].clone();
    *kept = Some((now_ms, values.clone(), cost_total.clone()));
    (values, cost_total)
}

/// The performance line ([`performance`]'s values).
fn performance_line(performance: &Value) -> Line {
    Line {
        name: "performance",
        north_star: Figure::new(
            "performance.report_to_live",
            "Time from report to live",
            "ms",
            performance["report_to_live"].clone(),
        ),
        guardrails: vec![
            Figure::new(
                "performance.gate_time",
                "Gate time per run",
                "ms",
                performance["gate_time"].clone(),
            ),
            Figure::new(
                "performance.decided_nothing",
                "Gate time that decided nothing",
                "share",
                performance["decided_nothing"].clone(),
            ),
            Figure::new(
                "performance.first_pass",
                "Landings that pass first time",
                "share",
                performance["first_pass"].clone(),
            ),
            Figure::new(
                "performance.usd_per_landing",
                "Dollars per landing",
                "usd",
                performance["usd_per_landing"].clone(),
            ),
        ],
    }
}

/// What the observability line reads besides the answer itself: the 7-day
/// cost window's `total` and health's `human_per_agent_hour`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Observed {
    pub(super) cost_total: Value,
    pub(super) human_per_agent_hour: Value,
}

/// The share of `total`'s tokens (input, output, cache write and cache
/// read) that no price covers: null with no tokens.
pub(super) fn unpriced_share(total: &Value) -> Value {
    let tokens = &total["tokens"];
    let all: f64 = ["input", "output", "cache_write", "cache_read"]
        .iter()
        .filter_map(|count| tokens[*count].as_f64())
        .sum();
    match total["unpriced_tokens"].as_f64() {
        Some(unpriced) if all > 0.0 => json!(unpriced / all),
        _ => Value::Null,
    }
}

/// The key of the count of figures not measured.
const NOT_MEASURED: &str = "observability.not_measured";

/// The storage line, from health.storage.
fn storage_line(storage: &Value) -> Line {
    let database = &storage["database"];
    let ratio = match (database["bytes"].as_f64(), database["log_bytes"].as_f64()) {
        (Some(bytes), Some(log)) if log > 0.0 => json!(bytes / log),
        _ => Value::Null,
    };
    // The largest readable test database, and the most schemas.
    let largest = |field: &str| {
        storage["test_databases"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|reading| reading[field].as_u64())
            .max()
            .map_or(Value::Null, |most| json!(most))
    };
    Line {
        name: "storage",
        north_star: Figure::new(
            "storage.days_left",
            "Days until the disk is full",
            "days",
            storage["days_left"].clone(),
        ),
        guardrails: vec![
            Figure::new(
                "storage.free_bytes",
                "Free disk",
                "bytes",
                storage["free_bytes"].clone(),
            ),
            Figure::new(
                "storage.archive_growth",
                "Transcript archive growth",
                "bytes_per_day",
                storage["growth_bytes_per_day"].clone(),
            ),
            Figure::new(
                "storage.database_ratio",
                "Database against its log",
                "ratio",
                ratio,
            ),
            Figure::new(
                "storage.test_database_bytes",
                "Largest test database",
                "bytes",
                largest("bytes"),
            ),
            Figure::new(
                "storage.test_database_schemas",
                "Most test schemas",
                "count",
                largest("schemas"),
            ),
        ],
    }
}

/// The four lines: the performance line's figures from `performance`
/// ([`performance`]), the storage line's from `storage` (health.storage as
/// [`super::storage::report`] gives it), the observability line's from
/// `observed`, the recoverability guardrails `recoverability`
/// ([`recoverability`]).
pub(super) fn lines(
    storage: &Value,
    performance: &Value,
    observed: &Observed,
    recoverability: Vec<Figure>,
) -> Vec<Line> {
    let mut lines = vec![
        performance_line(performance),
        storage_line(storage),
        observability_line(observed),
        Line {
            name: "recoverability",
            north_star: Figure::not_collected(
                "recoverability.restores_passed",
                "Critical resources with a restore passed in 7 days",
                "count",
            ),
            guardrails: recoverability,
        },
    ];
    count_not_measured(&mut lines);
    lines
}

/// The observability line (objective 088): its count not measured is set by
/// [`count_not_measured`] once every line is built.
fn observability_line(observed: &Observed) -> Line {
    Line {
        name: "observability",
        north_star: Figure::not_collected(
            "observability.needs_you_right",
            "Needs you is right",
            "share",
        ),
        guardrails: vec![
            Figure::new(
                "observability.unpriced_share",
                "Use with no price",
                "share",
                unpriced_share(&observed.cost_total),
            ),
            Figure::new(
                "observability.interventions_per_agent_hour",
                "Human interventions per agent-hour",
                "ratio",
                if observed.human_per_agent_hour.is_number() {
                    observed.human_per_agent_hour.clone()
                } else {
                    Value::Null
                },
            ),
            Figure::not_collected(NOT_MEASURED, "Figures not measured", "count"),
        ],
    }
}

/// Sets the count of figures not measured: those of `lines` with no value,
/// the count itself left out.
fn count_not_measured(lines: &mut [Line]) {
    let missing = lines
        .iter()
        .flat_map(|line| std::iter::once(&line.north_star).chain(&line.guardrails))
        .filter(|figure| figure.key != NOT_MEASURED && figure.value.is_null())
        .count();
    for figure in lines
        .iter_mut()
        .flat_map(|line| std::iter::once(&mut line.north_star).chain(&mut line.guardrails))
        .filter(|figure| figure.key == NOT_MEASURED)
    {
        figure.value = json!(missing);
    }
}

/// The answer from the lines, the ranges file's path (null when there is
/// no workspace root to find it under) and its contents
/// (`Err` when it could not be read for another reason than not being
/// there; `Ok(None)` when it is not there), every figure with its trend
/// from `trends` ([`trends`]; empty without one).
pub(super) fn answer(
    lines: &[Line],
    ranges_file: &Value,
    contents: Result<Option<String>, String>,
    trends: &BTreeMap<String, Vec<Value>>,
) -> Value {
    let known: Vec<&str> = lines
        .iter()
        .flat_map(|line| std::iter::once(&line.north_star).chain(&line.guardrails))
        .map(|figure| figure.key)
        .collect();
    let ranges = match contents {
        Ok(text) => parse_ranges(text.as_deref(), &known),
        Err(error) => Ranges {
            error: Some(error),
            ..Ranges::default()
        },
    };
    let with_range = |figure: &Figure| {
        let mut view = figure_json(figure, ranges.bounds.get(figure.key).copied());
        view["trend"] = json!(trends.get(figure.key).cloned().unwrap_or_default());
        view
    };
    json!({
        "lines": lines
            .iter()
            .map(|line| json!({
                "line": line.name,
                "north_star": with_range(&line.north_star),
                "guardrails": line.guardrails.iter().map(with_range).collect::<Vec<_>>(),
            }))
            .collect::<Vec<_>>(),
        "ranges_file": ranges_file,
        "ranges_error": ranges.error,
        "ignored_ranges": ranges
            .ignored
            .iter()
            .map(|(key, reason)| json!({"key": key, "reason": reason}))
            .collect::<Vec<_>>(),
    })
}

/// Where the ranges file is: `SPAWN_CORE_LINES_FILE`, else
/// `<SPAWN_WORKSPACE_ROOT>/core-lines.json`, else `None`.
fn ranges_path() -> Option<PathBuf> {
    crate::env_value("SPAWN_CORE_LINES_FILE")
        .map(PathBuf::from)
        .or_else(|| {
            crate::env_value("SPAWN_WORKSPACE_ROOT")
                .map(|root| PathBuf::from(root).join(RANGES_FILE))
        })
}

/// The file's contents: `Ok(None)` when there is none.
pub(super) fn read_ranges(path: Option<&std::path::Path>) -> Result<Option<String>, String> {
    let Some(path) = path else {
        return Ok(None);
    };
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// Where the nightly's fact files are: `<SPAWN_WORKSPACE_ROOT>/nightly`.
fn nightly_dir() -> Option<PathBuf> {
    crate::env_value("SPAWN_WORKSPACE_ROOT").map(|root| PathBuf::from(root).join("nightly"))
}

/// `GET /api/core-lines`.
pub(super) fn core_lines() -> Result<Value, String> {
    core_lines_at(
        &super::open_store()?,
        ranges_path().as_deref(),
        nightly_dir().as_deref(),
    )
}

/// The same answer for the loop's alert step (`guardrail_out_of_range`,
/// objective 089): the same figures, computed by the same code, the ranges
/// file and the nightly's fact files read now. The costly figures are kept
/// as for `spawn serve` (the performance line for a minute, health.storage's
/// sizes for ten), so a reading does not stall the loop.
pub(crate) fn reading(store: &spawn_postgres::PostgresStore) -> Result<Value, String> {
    core_lines_at(store, ranges_path().as_deref(), nightly_dir().as_deref())
}

/// The answer with the ranges file at `path` and the nightly's fact files
/// in `nightly`, read now, with the trends of the last 7 days' samples.
pub(super) fn core_lines_at(
    store: &spawn_postgres::PostgresStore,
    path: Option<&Path>,
    nightly: Option<&Path>,
) -> Result<Value, String> {
    let now_ms = super::now_ms();
    let figures = lines_at(store, nightly, now_ms)?;
    let samples = store
        .host_core_lines(trend_since(now_ms))
        .map_err(super::err)?;
    let file = path.map_or(Value::Null, |path| json!(path.display().to_string()));
    Ok(answer(
        &figures,
        &file,
        read_ranges(path),
        &trends(&samples, now_ms),
    ))
}

/// The four lines' figures, read at `now_ms` with the nightly's fact files
/// in `nightly`.
fn lines_at(
    store: &spawn_postgres::PostgresStore,
    nightly: Option<&Path>,
    now_ms: i64,
) -> Result<Vec<Line>, String> {
    let storage = super::storage::report(store, now_ms)?;
    let (performance, cost_total) = performance_now(store, now_ms);
    let health = store.fleet_health().map_err(super::err)?;
    let observed = Observed {
        cost_total,
        human_per_agent_hour: health["human_per_agent_hour"].clone(),
    };
    let backups = store.newest_backups().map_err(super::err)?;
    let facts = nightly_facts(nightly);
    Ok(lines(
        &storage,
        &performance,
        &observed,
        recoverability(&backups, facts.as_deref(), now_ms),
    ))
}

/// The figures that have a value, by key: what a `host.core_lines` sample
/// holds.
pub(super) fn values_of(lines: &[Line]) -> Map<String, Value> {
    lines
        .iter()
        .flat_map(|line| std::iter::once(&line.north_star).chain(&line.guardrails))
        .filter(|figure| !figure.value.is_null())
        .map(|figure| (figure.key.to_string(), figure.value.clone()))
        .collect()
}

/// Every figure of `GET /api/core-lines` that has a value, as it would
/// give it now, by key (objective 090): read the same way, through the
/// dashboard's own read-only connection and its statement timeout.
pub(crate) fn sample_values() -> Result<Map<String, Value>, String> {
    let store = super::open_store()?;
    Ok(values_of(&lines_at(
        &store,
        nightly_dir().as_deref(),
        super::now_ms(),
    )?))
}

#[cfg(test)]
mod tests {
    use super::{
        Figure, Observed, answer, figure_json, lines, nightly_facts, parse_ranges, performance,
        read_ranges, recoverability, unpriced_share,
    };
    use serde_json::{Value, json};

    const KNOWN: [&str; 3] = ["a.one", "a.two", "a.three"];

    fn state(value: Value, range: &str) -> String {
        let ranges = parse_ranges(
            Some(&format!(r#"{{"ranges": {{"a.one": {range}}}}}"#)),
            &KNOWN,
        );
        let figure = Figure::new("a.one", "One", "count", value);
        figure_json(&figure, ranges.bounds.get("a.one").copied())["state"]
            .as_str()
            .expect("a state")
            .to_string()
    }

    #[test]
    fn a_figure_is_in_out_without_a_range_or_not_collected() {
        assert_eq!(state(json!(5), r#"{"min": 1}"#), "in");
        assert_eq!(state(json!(1), r#"{"min": 1}"#), "in", "bounds included");
        assert_eq!(state(json!(0), r#"{"min": 1}"#), "out");
        assert_eq!(state(json!(10), r#"{"max": 10}"#), "in");
        assert_eq!(state(json!(10.5), r#"{"max": 10}"#), "out");
        assert_eq!(state(json!(3), r#"{"min": 1, "max": 5}"#), "in");
        assert_eq!(state(json!(6), r#"{"min": 1, "max": 5}"#), "out");
        assert_eq!(state(json!(0.5), r#"{"min": 0, "max": 1e12}"#), "in");
        assert_eq!(state(Value::Null, r#"{"min": 1}"#), "not_collected");
        let figure = Figure::new("a.one", "One", "count", json!(4));
        assert_eq!(
            figure_json(&figure, None),
            json!({"key": "a.one", "name": "One", "unit": "count", "value": 4,
                   "range": null, "state": "no_range"})
        );
        let ranges = parse_ranges(Some(r#"{"ranges": {"a.one": {"max": 524288}}}"#), &KNOWN);
        assert_eq!(
            figure_json(&figure, ranges.bounds.get("a.one").copied())["range"],
            json!({"max": 524_288}),
            "a whole bound stays whole"
        );
    }

    #[test]
    fn an_entry_naming_no_figure_or_without_a_number_bound_is_ignored() {
        let ranges = parse_ranges(
            Some(
                r#"{"ranges": {
                    "a.two": {"max": "many"},
                    "a.one": {"min": 1, "max": "x"},
                    "z.nothing": {"min": 1},
                    "a.three": {},
                    "b.free_bites": {"min": 1}
                }}"#,
            ),
            &KNOWN,
        );
        assert_eq!(ranges.error, None);
        assert_eq!(
            ranges.ignored,
            [
                ("a.three".to_string(), "needs a number min or max"),
                ("a.two".to_string(), "needs a number min or max"),
                ("b.free_bites".to_string(), "unknown figure"),
                ("z.nothing".to_string(), "unknown figure"),
            ],
            "sorted by key"
        );
        // A number bound beside one that is not: the number counts.
        assert_eq!(ranges.bounds.len(), 1);
        assert!(ranges.bounds.contains_key("a.one"));
    }

    #[test]
    fn a_missing_file_is_no_ranges_and_a_broken_one_says_why() {
        assert_eq!(parse_ranges(None, &KNOWN), super::Ranges::default());
        for broken in [
            "{\"ranges\": ",
            "[1, 2]",
            r#"{"ranges": [1]}"#,
            r#"{"other": {}}"#,
        ] {
            let ranges = parse_ranges(Some(broken), &KNOWN);
            assert!(ranges.error.is_some(), "{broken}");
            assert!(
                ranges.bounds.is_empty() && ranges.ignored.is_empty(),
                "{broken}"
            );
        }
        let dir = std::env::temp_dir().join(format!("spawn-core-lines-{}", std::process::id()));
        assert_eq!(read_ranges(Some(&dir.join("absent.json"))), Ok(None));
        assert_eq!(read_ranges(None), Ok(None));
    }

    #[test]
    fn the_storage_line_reads_health_storage() {
        let storage = json!({
            "days_left": 1234, "free_bytes": 104, "growth_bytes_per_day": 1_048_576,
            "database": {"bytes": 300, "log_bytes": 100},
            "test_databases": [
                {"role": "test", "bytes": 7, "schemas": 3},
                {"role": "fleet_test", "bytes": null, "schemas": null},
                {"role": "x", "bytes": 9, "schemas": 1},
            ],
        });
        let view = answer(
            &lines(&storage, &Value::Null, &Observed::default(), Vec::new()),
            &json!("/ws/core-lines.json"),
            Ok(None),
            &std::collections::BTreeMap::new(),
        );
        let names: Vec<&str> = view["lines"]
            .as_array()
            .expect("lines")
            .iter()
            .map(|line| line["line"].as_str().expect("a name"))
            .collect();
        assert_eq!(
            names,
            ["performance", "storage", "observability", "recoverability"]
        );
        let storage_line = &view["lines"][1];
        let values: Vec<(&str, Value)> = std::iter::once(&storage_line["north_star"])
            .chain(storage_line["guardrails"].as_array().expect("guardrails"))
            .map(|figure| {
                (
                    figure["key"].as_str().expect("a key"),
                    figure["value"].clone(),
                )
            })
            .collect();
        assert_eq!(
            values,
            [
                ("storage.days_left", json!(1234)),
                ("storage.free_bytes", json!(104)),
                ("storage.archive_growth", json!(1_048_576)),
                ("storage.database_ratio", json!(3.0)),
                ("storage.test_database_bytes", json!(9)),
                ("storage.test_database_schemas", json!(3)),
            ]
        );
        // No log size: no ratio; no readable test database: none.
        let bare = lines(
            &json!({"database": {"bytes": 300, "log_bytes": 0}, "test_databases": []}),
            &Value::Null,
            &Observed::default(),
            Vec::new(),
        );
        assert_eq!(bare[1].guardrails[2].value, Value::Null);
        assert_eq!(bare[1].guardrails[3].value, Value::Null);
        // The performance line without its figures: nothing collected.
        assert_eq!(view["lines"][0]["north_star"]["state"], "not_collected");
        assert_eq!(
            view["lines"][0]["guardrails"].as_array().map(Vec::len),
            Some(4)
        );
        // The other north stars are not collected.
        for index in [2, 3] {
            assert_eq!(view["lines"][index]["north_star"]["state"], "not_collected");
        }
        assert_eq!(view["lines"][3]["guardrails"], json!([]));
        assert_eq!(view["ranges_error"], Value::Null);
        assert_eq!(view["ignored_ranges"], json!([]));
    }

    /// Objective 086: the performance line from a scorecard and a cost
    /// answer, as the pages give them.
    #[test]
    fn the_performance_line_reads_the_scorecards_fleet_and_the_cost_window() {
        let scorecard = json!({"scopes": [
            {"scope": "mission", "mission": 1, "dimensions": {
                "report_to_live": {"value": 1}, "gate_time": {"value": 1}}},
            {"scope": "fleet", "mission": null, "dimensions": {
                "report_to_live": {"value": 5_520_000, "n": 4},
                "gate_time": {"value": 600_000, "total_ms": 5_100_000, "undecided_ms": 300_000},
                "first_pass": {"value": 0.749},
            }},
        ]});
        let cost = json!({"usd_per_landing": 8.08});
        let values = performance(&scorecard, &cost);
        assert_eq!(values["report_to_live"], json!(5_520_000));
        assert_eq!(values["gate_time"], json!(600_000));
        let share = values["decided_nothing"].as_f64().expect("a share");
        assert!((share - 300_000.0 / 5_100_000.0).abs() < 1e-12, "{share}");
        assert_eq!(values["first_pass"], json!(0.749));
        assert_eq!(values["usd_per_landing"], json!(8.08));
        let line = &lines(&json!({}), &values, &Observed::default(), Vec::new())[0];
        let keys: Vec<&str> = std::iter::once(&line.north_star)
            .chain(&line.guardrails)
            .map(|figure| figure.key)
            .collect();
        assert_eq!(
            keys,
            [
                "performance.report_to_live",
                "performance.gate_time",
                "performance.decided_nothing",
                "performance.first_pass",
                "performance.usd_per_landing",
            ]
        );
        let units: Vec<&str> = line.guardrails.iter().map(|figure| figure.unit).collect();
        assert_eq!(units, ["ms", "share", "share", "usd"]);
        // A gate that spent nothing: no share, so not collected.
        let idle = json!({"scopes": [{"scope": "fleet", "dimensions": {
            "report_to_live": {"value": null},
            "gate_time": {"value": null, "total_ms": 0, "undecided_ms": 0},
            "first_pass": {"value": null}}}]});
        let none = performance(&idle, &json!({"usd_per_landing": null}));
        assert_eq!(
            none,
            json!({"report_to_live": null, "gate_time": null, "decided_nothing": null,
                   "first_pass": null, "usd_per_landing": null})
        );
        let line = &lines(&json!({}), &none, &Observed::default(), Vec::new())[0];
        assert!(
            std::iter::once(&line.north_star)
                .chain(&line.guardrails)
                .all(|figure| figure_json(figure, None)["state"] == "not_collected")
        );
        // Neither answer read: every value null.
        assert_eq!(performance(&Value::Null, &Value::Null), none);
    }

    fn values(figures: &[Figure]) -> Vec<(&'static str, Value)> {
        figures
            .iter()
            .map(|figure| (figure.key, figure.value.clone()))
            .collect()
    }

    #[test]
    fn the_backup_ages_are_the_newest_records_ages_or_null_without_one() {
        let none = recoverability(&json!({"dump": null, "bundle": null}), None, 10_000);
        assert_eq!(
            values(&none),
            [
                ("recoverability.cloud_dump_age", Value::Null),
                ("recoverability.cloud_bundle_age", Value::Null),
                ("recoverability.nightly_age", Value::Null),
                ("recoverability.nightly_red", Value::Null),
            ]
        );
        assert!(none[..3].iter().all(|figure| figure.unit == "ms"));
        assert_eq!(none[3].unit, "count");
        let backups = json!({
            "dump": {"at_ms": 8_200, "key": "postgres/hourly/a.dump.gpg", "bytes": 9},
            "bundle": {"at_ms": 1_000, "key": "bundles/incr/b.tar.zst.gpg", "bytes": 5},
        });
        let both = recoverability(&backups, None, 10_000);
        assert_eq!(both[0].value, json!(1_800));
        assert_eq!(both[1].value, json!(9_000));
        // Only a bundle: the copy's age stays null.
        let bundle_only = recoverability(
            &json!({"dump": null, "bundle": {"at_ms": 4_000}}),
            None,
            10_000,
        );
        assert_eq!(bundle_only[0].value, Value::Null);
        assert_eq!(bundle_only[1].value, json!(6_000));
        // The line carries them as its guardrails, in order.
        let line = &lines(&json!({}), &Value::Null, &Observed::default(), both)[3];
        assert_eq!(line.name, "recoverability");
        assert_eq!(
            line.guardrails
                .iter()
                .map(|figure| figure.key)
                .collect::<Vec<_>>(),
            [
                "recoverability.cloud_dump_age",
                "recoverability.cloud_bundle_age",
                "recoverability.nightly_age",
                "recoverability.nightly_red",
            ]
        );
    }

    #[test]
    fn the_nightly_figures_read_only_the_fact_files() {
        let dir = std::env::temp_dir().join(format!(
            "spawn-core-lines-nightly-{}-{}",
            std::process::id(),
            super::super::now_ms()
        ));
        let backups = json!({"dump": null, "bundle": null});
        // No directory: no figure.
        assert_eq!(nightly_facts(Some(&dir.join("absent"))), None);
        assert_eq!(nightly_facts(None), None);
        std::fs::create_dir_all(&dir).expect("a directory");
        // An empty directory: no fact, no figure.
        let empty = nightly_facts(Some(&dir));
        assert_eq!(empty, Some(Vec::new()));
        let figures = recoverability(&backups, empty.as_deref(), 100_000);
        assert_eq!(figures[2].value, Value::Null);
        assert_eq!(figures[3].value, Value::Null);
        // A green fact, with a scenario log, a lock and a run file beside it.
        std::fs::write(
            dir.join("spawn.json"),
            r#"{"state": "green", "red_since": null, "judged_at": 90000}"#,
        )
        .expect("a fact");
        std::fs::write(dir.join("spawn.040.sh.log"), "a log").expect("a log");
        std::fs::write(dir.join(".spawn.lock"), "").expect("a lock");
        std::fs::write(
            dir.join(".run-1.json"),
            r#"{"red_since": "x", "judged_at": 1}"#,
        )
        .expect("a run file");
        let facts = nightly_facts(Some(&dir));
        assert_eq!(facts.as_ref().map(Vec::len), Some(1), "the log is no fact");
        let figures = recoverability(&backups, facts.as_deref(), 100_000);
        assert_eq!(figures[2].value, json!(10_000));
        assert_eq!(figures[3].value, json!(0));
        // A fact with no judged_at, one that is not JSON, and a red one whose
        // name holds dots.
        std::fs::write(
            dir.join("never.json"),
            r#"{"state": "skipped", "red_since": null, "judged_at": null}"#,
        )
        .expect("a fact");
        std::fs::write(dir.join("broken.json"), "{\"state\": ").expect("a broken fact");
        std::fs::write(
            dir.join("sup.ply-2.json"),
            r#"{"state": "red", "red_since": "bbbb", "judged_at": 40000}"#,
        )
        .expect("a fact");
        let facts = nightly_facts(Some(&dir));
        assert_eq!(facts.as_ref().map(Vec::len), Some(3));
        let figures = recoverability(&backups, facts.as_deref(), 100_000);
        assert_eq!(figures[2].value, json!(60_000), "the oldest judging");
        assert_eq!(figures[3].value, json!(1), "one source held red");
        // Only facts with no judged_at: no age, but the count stands.
        let unjudged = [json!({"red_since": "c", "judged_at": null})];
        let figures = recoverability(&backups, Some(&unjudged), 100_000);
        assert_eq!(figures[2].value, Value::Null);
        assert_eq!(figures[3].value, json!(1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Objective 090: a figure's trend is one point per UTC day of the last
    /// 7 that has a sample carrying it, that day's last; a day without one
    /// has no point; a sample outside the window is left out.
    #[test]
    fn a_trend_is_each_days_last_sample_of_the_last_seven() {
        const D: i64 = super::DAY_MS;
        const H: i64 = 3_600_000;
        // 2026-11-12, 13:30 UTC.
        let midnight = 20_769 * D;
        let now = midnight + 13 * H + 30 * 60_000;
        assert_eq!(super::trend_since(now), midnight - 6 * D);
        let samples = json!([
            {"at_ms": midnight - 8 * D + H, "values": {"a.one": 1}},
            {"at_ms": midnight - 7 * D + 23 * H, "values": {"a.one": 2}},
            {"at_ms": midnight - 6 * D, "values": {"a.one": 100, "a.two": 7}},
            {"at_ms": midnight - 3 * D + H, "values": {"a.one": 110, "a.two": null}},
            {"at_ms": midnight - D + H, "values": {"a.one": 999, "a.two": 8}},
            {"at_ms": midnight - D + 5 * H, "values": {"a.one": 120.5}},
            {"at_ms": now, "values": {"a.one": 121}},
            {"at_ms": midnight + D + H, "values": {"a.one": 5}},
        ]);
        let trends = super::trends(&samples, now);
        assert_eq!(
            json!(trends["a.one"]),
            json!([
                {"day": "2026-11-06", "value": 100},
                {"day": "2026-11-09", "value": 110},
                {"day": "2026-11-11", "value": 120.5},
                {"day": "2026-11-12", "value": 121},
            ])
        );
        // A day whose last sample does not carry the figure keeps the
        // day's last sample that does; a null is no point.
        assert_eq!(
            json!(trends["a.two"]),
            json!([{"day": "2026-11-06", "value": 7}, {"day": "2026-11-11", "value": 8}])
        );
        assert!(super::trends(&json!([]), now).is_empty());

        // In the answer: every figure carries a trend, empty without one.
        let mut trends = trends;
        trends.insert(
            "storage.free_bytes".to_string(),
            vec![json!({"day": "2026-11-12", "value": 3})],
        );
        let view = answer(
            &lines(
                &json!({"free_bytes": 3}),
                &Value::Null,
                &Observed::default(),
                Vec::new(),
            ),
            &Value::Null,
            Ok(None),
            &trends,
        );
        for line in view["lines"].as_array().expect("lines") {
            for figure in std::iter::once(&line["north_star"])
                .chain(line["guardrails"].as_array().expect("guardrails"))
            {
                let want = if figure["key"] == "storage.free_bytes" {
                    json!([{"day": "2026-11-12", "value": 3}])
                } else {
                    json!([])
                };
                assert_eq!(figure["trend"], want, "{figure}");
            }
        }
    }

    /// Objective 090: a sample holds every figure that has a value, and
    /// none without.
    #[test]
    fn a_sample_holds_every_figure_with_a_value() {
        let figures = lines(
            &json!({"free_bytes": 3, "days_left": 9}),
            &Value::Null,
            &Observed::default(),
            Vec::new(),
        );
        let values = super::values_of(&figures);
        // The count not measured always has a value (objective 088): the
        // other 13 figures that have none.
        assert_eq!(
            Value::Object(values),
            json!({"storage.free_bytes": 3, "storage.days_left": 9,
                   "observability.not_measured": 13})
        );
    }

    /// Objective 088: the observability line's guardrails.
    #[test]
    fn the_observability_line_gives_unpriced_use_interventions_and_the_count_not_measured() {
        let storage = json!({
            "days_left": 1234, "free_bytes": 104, "growth_bytes_per_day": 10,
            "database": {"bytes": 300, "log_bytes": 100},
            "test_databases": [{"bytes": 7, "schemas": 3}],
        });
        let tokens = |input: u64, cache_read: u64, unpriced: u64| {
            json!({"usd": 0.9, "tokens": {"input": input, "output": 0, "cache_write": 0,
                   "cache_read": cache_read}, "unpriced_tokens": unpriced})
        };
        // No tokens: no share.
        assert_eq!(unpriced_share(&tokens(0, 0, 0)), Value::Null);
        assert_eq!(unpriced_share(&Value::Null), Value::Null);
        // All priced: none unpriced.
        assert_eq!(unpriced_share(&tokens(300, 100, 0)), json!(0.0));
        // Every count is in the whole, cache reads too.
        assert_eq!(unpriced_share(&tokens(300, 100, 100)), json!(0.25));

        let observed = Observed {
            cost_total: tokens(400_000, 0, 100_000),
            human_per_agent_hour: json!(0.125),
        };
        let all = lines(&storage, &Value::Null, &observed, Vec::new());
        let observability = &all[2];
        assert_eq!(
            observability.north_star.key,
            "observability.needs_you_right"
        );
        assert_eq!(observability.north_star.value, Value::Null);
        let guardrails: Vec<(&str, &str, Value)> = observability
            .guardrails
            .iter()
            .map(|figure| (figure.key, figure.unit, figure.value.clone()))
            .collect();
        // Not measured: the five performance figures (none read) and the
        // two other north stars not collected, every storage figure
        // measured, itself left out though it starts with no value.
        assert_eq!(
            guardrails,
            [
                ("observability.unpriced_share", "share", json!(0.25)),
                (
                    "observability.interventions_per_agent_hour",
                    "ratio",
                    json!(0.125)
                ),
                ("observability.not_measured", "count", json!(7)),
            ]
        );
        // Nothing read: the two guardrails count too.
        let bare = lines(&storage, &Value::Null, &Observed::default(), Vec::new());
        assert_eq!(bare[2].guardrails[0].value, Value::Null);
        assert_eq!(bare[2].guardrails[1].value, Value::Null);
        assert_eq!(bare[2].guardrails[2].value, json!(9));
        // Its range and state as for every figure.
        let view = answer(
            &all,
            &Value::Null,
            Ok(Some(
                r#"{"ranges": {"observability.unpriced_share": {"max": 0.05},
                    "observability.interventions_per_agent_hour": {"max": 0.05},
                    "observability.not_measured": {"max": 0}}}"#
                    .to_string(),
            )),
            &std::collections::BTreeMap::new(),
        );
        let states: Vec<&str> = view["lines"][2]["guardrails"]
            .as_array()
            .expect("guardrails")
            .iter()
            .map(|figure| figure["state"].as_str().expect("a state"))
            .collect();
        assert_eq!(states, ["out", "out", "out"]);
    }
}
