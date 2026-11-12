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
//!     "north_star": {key, name, unit, value, range, state},
//!     "guardrails": [{key, name, unit, value, range, state}, ...]
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
//! without a value.
//!
<<<<<<< 87db307185771b52d50aac9203f76b8aa7467099
//! The performance figures (objective 086) are the pages': the Scorecard's
//! fleet scope over its default 7 days (report to live, the gate time's
//! median, the share of gate time that decided nothing, first pass) and
//! `/api/cost?window=7d`'s dollars per landing ([`performance`]), kept for a
//! minute so a Home poll does not compute them again at every request. The
//! storage figures are health.storage's on `/api/fleet`, read the same way
//! ([`super::storage::report`]). The other lines' north stars are not
//! collected yet; later objectives add their figures to this frame.
=======
//! The storage figures are health.storage's on `/api/fleet`, read the same
//! way ([`super::storage::report`]). The observability line's guardrails
//! (objective 088) are the share of the 7-day cost window's tokens no price
//! covers (`/api/cost?window=7d`'s total: unpriced_tokens over its input,
//! output, cache write and cache read tokens; null with none), health's
//! `human_per_agent_hour` on `/api/fleet`, and how many figures of this
//! same answer have no value, itself left out. The other lines' north stars
//! are not collected yet; later objectives add their figures to this frame.
>>>>>>> 0c9ab77233354544a58812b663d20217c80ce798
//!
//! No range lives in code: they come only from the ranges file,
//! `SPAWN_CORE_LINES_FILE`, by default `<workspace root>/core-lines.json`,
//! shaped `{"ranges": {<figure key>: {"min"?: number, "max"?: number}}}`,
//! read at every request, so an edit shows without a restart. An entry whose
//! key names no figure, or with no number min or max, is left out and listed
//! in `ignored_ranges`. A missing file is no ranges and no error; a file that
//! is not such JSON is no ranges and a `ranges_error`. Nothing but `spawn
//! serve` reads the file, and no gate, integrator or deploy step reads the
//! answer: monitoring only.

use std::collections::BTreeMap;
use std::path::PathBuf;

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

<<<<<<< 87db307185771b52d50aac9203f76b8aa7467099
/// How long the performance figures are kept: a Home poll must not compute
/// the 7-day scorecard and cost window again at every request.
const PERFORMANCE_KEPT_MS: i64 = 60_000;

/// The performance figures of the last read, and when.
static PERFORMANCE: std::sync::Mutex<Option<(i64, Value)>> = std::sync::Mutex::new(None);

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

/// The performance values, kept for [`PERFORMANCE_KEPT_MS`]: computed from
/// the 7-day scorecard and cost window as the pages compute them. A part
/// that cannot be read counts as not collected.
fn performance_now(store: &spawn_postgres::PostgresStore, now_ms: i64) -> Value {
    let mut kept = PERFORMANCE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((_, values)) = kept
        .as_ref()
        .filter(|(at, _)| (0..PERFORMANCE_KEPT_MS).contains(&now_ms.saturating_sub(*at)))
    {
        return values.clone();
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
    *kept = Some((now_ms, values.clone()));
    values
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

/// The storage line, from health.storage.
fn storage_line(storage: &Value) -> Line {
=======
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

/// The four lines, the storage line's figures from `storage` (health.storage
/// as [`super::storage::report`] gives it), the observability line's from
/// `observed`.
pub(super) fn lines(storage: &Value, observed: &Observed) -> Vec<Line> {
>>>>>>> 0c9ab77233354544a58812b663d20217c80ce798
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
<<<<<<< 87db307185771b52d50aac9203f76b8aa7467099
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
/// [`super::storage::report`] gives it).
pub(super) fn lines(storage: &Value, performance: &Value) -> Vec<Line> {
    vec![
        performance_line(performance),
        storage_line(storage),
        Line {
            name: "observability",
            north_star: Figure::not_collected(
                "observability.needs_you_right",
                "Needs you is right",
                "share",
            ),
            guardrails: Vec::new(),
        },
=======
    let mut lines = vec![
        Line {
            name: "performance",
            north_star: Figure::not_collected(
                "performance.report_to_live",
                "Time from report to live",
                "ms",
            ),
            guardrails: Vec::new(),
        },
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
        },
        observability_line(observed),
>>>>>>> 0c9ab77233354544a58812b663d20217c80ce798
        Line {
            name: "recoverability",
            north_star: Figure::not_collected(
                "recoverability.restores_passed",
                "Critical resources with a restore passed in 7 days",
                "count",
            ),
            guardrails: Vec::new(),
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
/// there; `Ok(None)` when it is not there).
pub(super) fn answer(
    lines: &[Line],
    ranges_file: &Value,
    contents: Result<Option<String>, String>,
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
    let with_range = |figure: &Figure| figure_json(figure, ranges.bounds.get(figure.key).copied());
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

/// `GET /api/core-lines`.
pub(super) fn core_lines() -> Result<Value, String> {
    core_lines_at(&super::open_store()?, ranges_path().as_deref())
}

/// The answer with the ranges file at `path`, read now.
pub(super) fn core_lines_at(
    store: &spawn_postgres::PostgresStore,
    path: Option<&std::path::Path>,
) -> Result<Value, String> {
<<<<<<< 87db307185771b52d50aac9203f76b8aa7467099
    let now_ms = super::now_ms();
    let storage = super::storage::report(store, now_ms)?;
    let performance = performance_now(store, now_ms);
    let file = path.map_or(Value::Null, |path| json!(path.display().to_string()));
    Ok(answer(
        &lines(&storage, &performance),
=======
    let storage = super::storage::report(store, super::now_ms())?;
    let (name, length_ms) = crate::cost::window_of(Some("7d"))?;
    let cost = crate::cost::window(store, name, length_ms)?;
    let health = store.fleet_health().map_err(super::err)?;
    let observed = Observed {
        cost_total: cost["total"].clone(),
        human_per_agent_hour: health["human_per_agent_hour"].clone(),
    };
    let file = path.map_or(Value::Null, |path| json!(path.display().to_string()));
    Ok(answer(
        &lines(&storage, &observed),
>>>>>>> 0c9ab77233354544a58812b663d20217c80ce798
        &file,
        read_ranges(path),
    ))
}

#[cfg(test)]
mod tests {
<<<<<<< 87db307185771b52d50aac9203f76b8aa7467099
    use super::{Figure, answer, figure_json, lines, parse_ranges, performance, read_ranges};
=======
    use super::{
        Figure, Observed, answer, figure_json, lines, parse_ranges, read_ranges, unpriced_share,
    };
>>>>>>> 0c9ab77233354544a58812b663d20217c80ce798
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
<<<<<<< 87db307185771b52d50aac9203f76b8aa7467099
            &lines(&storage, &Value::Null),
=======
            &lines(&storage, &Observed::default()),
>>>>>>> 0c9ab77233354544a58812b663d20217c80ce798
            &json!("/ws/core-lines.json"),
            Ok(None),
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
<<<<<<< 87db307185771b52d50aac9203f76b8aa7467099
            &Value::Null,
        );
        assert_eq!(bare[1].guardrails[2].value, Value::Null);
        assert_eq!(bare[1].guardrails[3].value, Value::Null);
        // The performance line without its figures: nothing collected.
        assert_eq!(view["lines"][0]["north_star"]["state"], "not_collected");
        assert_eq!(
            view["lines"][0]["guardrails"].as_array().map(Vec::len),
            Some(4)
        );
        // The other north stars are not collected, with no guardrails.
        for index in [2, 3] {
=======
            &Observed::default(),
        );
        assert_eq!(bare[1].guardrails[2].value, Value::Null);
        assert_eq!(bare[1].guardrails[3].value, Value::Null);
        // The other north stars are not collected.
        for index in [0, 2, 3] {
>>>>>>> 0c9ab77233354544a58812b663d20217c80ce798
            assert_eq!(view["lines"][index]["north_star"]["state"], "not_collected");
        }
        for index in [0, 3] {
            assert_eq!(view["lines"][index]["guardrails"], json!([]));
        }
        assert_eq!(view["ranges_error"], Value::Null);
        assert_eq!(view["ignored_ranges"], json!([]));
    }

<<<<<<< 87db307185771b52d50aac9203f76b8aa7467099
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
        let line = &lines(&json!({}), &values)[0];
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
        let line = &lines(&json!({}), &none)[0];
        assert!(
            std::iter::once(&line.north_star)
                .chain(&line.guardrails)
                .all(|figure| figure_json(figure, None)["state"] == "not_collected")
        );
        // Neither answer read: every value null.
        assert_eq!(performance(&Value::Null, &Value::Null), none);
=======
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
        let all = lines(&storage, &observed);
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
        // Not measured: the three other north stars, every storage figure
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
                ("observability.not_measured", "count", json!(3)),
            ]
        );
        // Nothing read: the two guardrails count too.
        let bare = lines(&storage, &Observed::default());
        assert_eq!(bare[2].guardrails[0].value, Value::Null);
        assert_eq!(bare[2].guardrails[1].value, Value::Null);
        assert_eq!(bare[2].guardrails[2].value, json!(5));
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
        );
        let states: Vec<&str> = view["lines"][2]["guardrails"]
            .as_array()
            .expect("guardrails")
            .iter()
            .map(|figure| figure["state"].as_str().expect("a state"))
            .collect();
        assert_eq!(states, ["out", "out", "out"]);
>>>>>>> 0c9ab77233354544a58812b663d20217c80ce798
    }
}
