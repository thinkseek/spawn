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
//! `unit` is one of bytes, bytes_per_day, days, count, ratio, ms, share.
//! `range` is the file's entry for the figure (`{min?, max?}`, numbers) or
//! null; `state` is `in` when the value lies within it (bounds included),
//! `out` when it does not, `no_range` without a range and `not_collected`
//! without a value.
//!
//! The storage figures are health.storage's on `/api/fleet`, read the same
//! way ([`super::storage::report`]). The other lines' north stars are not
//! collected yet; later objectives add their figures to this frame.
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
    pub(super) line: &'static str,
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
    pub(super) ranges: BTreeMap<String, Range>,
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
            ranges.ignored.push((key.clone(), "needs a number min or max"));
            continue;
        }
        ranges.ranges.insert(key.clone(), range);
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

/// The four lines, the storage line's figures from `storage` (health.storage
/// as [`super::storage::report`] gives it).
pub(super) fn lines(storage: &Value) -> Vec<Line> {
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
    vec![
        Line {
            line: "performance",
            north_star: Figure::not_collected(
                "performance.report_to_live",
                "Time from report to live",
                "ms",
            ),
            guardrails: Vec::new(),
        },
        Line {
            line: "storage",
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
        Line {
            line: "observability",
            north_star: Figure::not_collected(
                "observability.needs_you_right",
                "Needs you is right",
                "share",
            ),
            guardrails: Vec::new(),
        },
        Line {
            line: "recoverability",
            north_star: Figure::not_collected(
                "recoverability.restores_passed",
                "Critical resources with a restore passed in 7 days",
                "count",
            ),
            guardrails: Vec::new(),
        },
    ]
}

/// The answer from the lines, the ranges file's path (null when there is
/// no workspace root to find it under) and its contents
/// (`Err` when it could not be read for another reason than not being
/// there; `Ok(None)` when it is not there).
pub(super) fn answer(
    lines: &[Line],
    ranges_file: Value,
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
    let with_range = |figure: &Figure| figure_json(figure, ranges.ranges.get(figure.key).copied());
    json!({
        "lines": lines
            .iter()
            .map(|line| json!({
                "line": line.line,
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
        .or_else(|| crate::env_value("SPAWN_WORKSPACE_ROOT").map(|root| PathBuf::from(root).join(RANGES_FILE)))
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
    core_lines_at(&super::open_store()?, ranges_path())
}

/// The answer with the ranges file at `path`, read now.
pub(super) fn core_lines_at(
    store: &spawn_postgres::PostgresStore,
    path: Option<PathBuf>,
) -> Result<Value, String> {
    let storage = super::storage::report(store, super::now_ms())?;
    let file = path
        .as_ref()
        .map_or(Value::Null, |path| json!(path.display().to_string()));
    Ok(answer(&lines(&storage), file, read_ranges(path.as_deref())))
}

#[cfg(test)]
mod tests {
    use super::{Figure, answer, figure_json, lines, parse_ranges, read_ranges};
    use serde_json::{Value, json};

    const KNOWN: [&str; 3] = ["a.one", "a.two", "a.three"];

    fn state(value: Value, range: &str) -> String {
        let ranges = parse_ranges(Some(&format!(r#"{{"ranges": {{"a.one": {range}}}}}"#)), &KNOWN);
        let figure = Figure::new("a.one", "One", "count", value);
        figure_json(&figure, ranges.ranges.get("a.one").copied())["state"]
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
            figure_json(&figure, ranges.ranges.get("a.one").copied())["range"],
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
        assert_eq!(ranges.ranges.len(), 1);
        assert!(ranges.ranges.contains_key("a.one"));
    }

    #[test]
    fn a_missing_file_is_no_ranges_and_a_broken_one_says_why() {
        assert_eq!(parse_ranges(None, &KNOWN), super::Ranges::default());
        for broken in ["{\"ranges\": ", "[1, 2]", r#"{"ranges": [1]}"#, r#"{"other": {}}"#] {
            let ranges = parse_ranges(Some(broken), &KNOWN);
            assert!(ranges.error.is_some(), "{broken}");
            assert!(ranges.ranges.is_empty() && ranges.ignored.is_empty(), "{broken}");
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
        let view = answer(&lines(&storage), json!("/ws/core-lines.json"), Ok(None));
        let names: Vec<&str> = view["lines"]
            .as_array()
            .expect("lines")
            .iter()
            .map(|line| line["line"].as_str().expect("a name"))
            .collect();
        assert_eq!(names, ["performance", "storage", "observability", "recoverability"]);
        let storage_line = &view["lines"][1];
        let values: Vec<(&str, Value)> = std::iter::once(&storage_line["north_star"])
            .chain(storage_line["guardrails"].as_array().expect("guardrails"))
            .map(|figure| (figure["key"].as_str().expect("a key"), figure["value"].clone()))
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
        let bare = lines(&json!({"database": {"bytes": 300, "log_bytes": 0}, "test_databases": []}));
        assert_eq!(bare[1].guardrails[2].value, Value::Null);
        assert_eq!(bare[1].guardrails[3].value, Value::Null);
        // The other north stars are not collected, with no guardrails.
        for index in [0, 2, 3] {
            assert_eq!(view["lines"][index]["north_star"]["state"], "not_collected");
            assert_eq!(view["lines"][index]["guardrails"], json!([]));
        }
        assert_eq!(view["ranges_error"], Value::Null);
        assert_eq!(view["ignored_ranges"], json!([]));
    }
}
