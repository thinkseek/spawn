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
//! without a value. A figure still measuring its window carries
//! `measuring_since_ms`, when its measure starts (only
//! `performance.report_to_live`, below). `trend` (objective 090) is the
//! figure's last 7 days:
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
//! minute so a Home poll does not compute them again at every request.
//! Report to live (objective 095's (2), by hand for the owner, lead-mac
//! 2026-11-12 18:15): the live record starts at the first `deploy.finished`
//! of any outcome, and a landing before it reads as going live with that
//! first deploy, so a 7-day window reaching back before it measures the
//! record's start, not the wait (106 h on Home). Until the record covers the
//! 7 days the figure has no value (`not_collected`, and no sample) and
//! carries `measuring_since_ms`, the record's start (none without a
//! record) ([`measured`]). The
//! storage figures are health.storage's on `/api/fleet`, read the same way
//! ([`super::storage::report`]). The observability line's guardrails
//! (objective 088) are the share of the 7-day cost window's tokens no price
//! covers (`/api/cost?window=7d`'s total, kept with the performance figures:
//! unpriced_tokens over its input, output, cache write and cache read
//! tokens; null with none), the Home card's human interventions per
//! agent-hour (`/api/interventions?window=24h`'s `per_agent_hour`, its last
//! 24 h, objective 100; the figure says `"window": "24h"`) on
//! `/api/fleet`, and how many figures of this same answer have no value,
//! itself left out. The recoverability guardrails (objective 087,
//! [`recoverability`]) are the ages of the newest database copy and bundle
//! (health.backups on `/api/fleet`), the age of the oldest nightly judging
//! and how many sources the nightly holds red (its fact files,
//! [`nightly_facts`]). The recoverability north star and its two ages
//! (objective 093) are the recovery table's drills, the rows the same file
//! names under `recoverability` ([`drills`]); the answer lists them as
//! `drills`. The storage line's last two guardrails (objective 094) are the
//! scratch nobody owns and the leaked test schemas, read in the background
//! and kept ([`unowned`], [`scratch`], [`schemas`]); the answer lists
//! `scratch` and `test_schemas`. Later objectives add their figures to this
//! frame.
//!
//! No range lives in code: they come only from the ranges file,
//! `SPAWN_CORE_LINES_FILE`, by default `<workspace root>/core-lines.json`,
//! shaped `{"ranges": {<figure key>: {"min"?: number, "max"?: number}}}`,
//! read at every request, so an edit shows without a restart. An entry whose
//! key names no figure, or with no number min or max, is left out and listed
//! in `ignored_ranges`. A missing file is no ranges and no error; a file that
//! is not such JSON is no ranges and a `ranges_error`. Nothing but `spawn
//! serve` and the loop's alert step ([`reading`], `guardrail_out_of_range`,
//! on a thread of its own) read the file, and no gate, integrator or deploy step reads the answer:
//! monitoring only.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

mod drills;
mod observability;
mod performance;
mod recoverability;
mod schemas;
mod scratch;
mod storage;
mod trends;
mod unowned;

use observability::{Observed, count_not_measured, observability_line};
use performance::{measured_samples, performance_line, performance_now};
use recoverability::{nightly_facts, recoverability};
use storage::storage_line;
use trends::{trend_since, trends};

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
    /// When a figure still measuring its window started measuring (null
    /// otherwise): said as `measuring_since_ms`.
    pub(super) measuring_since_ms: Value,
    /// The window a figure is read over, when it says one (objective 100:
    /// interventions per agent-hour, "24h"): said as `window`.
    pub(super) window: Option<&'static str>,
}

impl Figure {
    fn new(key: &'static str, name: &'static str, unit: &'static str, value: Value) -> Self {
        Figure {
            key,
            name,
            unit,
            value,
            measuring_since_ms: Value::Null,
            window: None,
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

/// A figure as the answer gives it: with its range and its state.
pub(super) fn figure_json(figure: &Figure, range: Option<Range>) -> Value {
    let state = match (figure.value.as_f64(), range) {
        (None, _) => "not_collected",
        (Some(_), None) => "no_range",
        (Some(value), Some(range)) if range.holds(value) => "in",
        (Some(_), Some(_)) => "out",
    };
    let mut answer = json!({
        "key": figure.key,
        "name": figure.name,
        "unit": figure.unit,
        "value": figure.value,
        "range": range.map_or(Value::Null, Range::json),
        "state": state,
    });
    if !figure.measuring_since_ms.is_null() {
        answer["measuring_since_ms"] = figure.measuring_since_ms.clone();
    }
    if let Some(window) = figure.window {
        answer["window"] = json!(window);
    }
    answer
}

/// The four lines: the performance line's figures from `performance`
/// ([`performance`]), the storage line's from `storage` (health.storage as
/// [`super::storage::report`] gives it), the observability line's from
/// `observed`, the recoverability north star `restores_passed` (objective
/// 093, [`drills::figures`]) and its guardrails `recoverability`
/// ([`recoverability`], then the drills' two ages).
pub(super) fn lines(
    storage: &Value,
    performance: &Value,
    observed: &Observed,
    restores_passed: Value,
    recoverability: Vec<Figure>,
) -> Vec<Line> {
    let mut lines = vec![
        performance_line(performance),
        storage_line(storage),
        observability_line(observed),
        Line {
            name: "recoverability",
            north_star: Figure::new(
                "recoverability.restores_passed",
                "Critical resources with a restore passed in 7 days",
                "count",
                restores_passed,
            ),
            guardrails: recoverability,
        },
    ];
    count_not_measured(&mut lines);
    lines
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

/// How long each part of one reading took, said on the alert step's
/// `core lines read:` line beside its whole compute time (the owner's
/// per-part timing). `None` is a part that did not run: the archive and the
/// cost are kept by [`performance_now`] for a minute after they are
/// computed, and a reading that failed ends before its later parts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Parts {
    /// The dashboard's connection opened and the ranges file read
    /// (`reading` alone: the API's answer is given its store).
    pub(crate) open: Option<Duration>,
    /// The 7-day samples the trends are drawn from (`host.core_lines`)
    /// and the first deploy that holds report to live back.
    pub(crate) trends: Option<Duration>,
    /// The recoverability line's facts: the newest backups, the nightly's
    /// fact files and the drills.
    pub(crate) facts: Option<Duration>,
    /// The 7-day Scorecard (`scorecard::measure`, which reads the
    /// transcript archive) and the two reads that place report to live
    /// (the first deploy and the database's clock).
    pub(crate) archive: Option<Duration>,
    /// The 7-day cost window (`cost::summary`).
    pub(crate) cost: Option<Duration>,
    /// The storage report, with the scratch nobody owns and each test
    /// server's schemas.
    pub(crate) storage: Option<Duration>,
    /// Human interventions per agent-hour, the Home card's 24 h answer
    /// (`/api/interventions`, objective 100).
    pub(crate) health: Option<Duration>,
    /// The wait for the kept performance figures' lock, behind another
    /// reader computing them (cc-astra A35).
    pub(crate) lock_wait: Option<Duration>,
    /// Whether the performance figures were `measured` by this reading or
    /// `kept` from another's within the minute (cc-astra A35): a kept one
    /// times neither archive nor cost.
    pub(crate) performance: Option<&'static str>,
}

impl Parts {
    /// Every part's time added up: never more than the whole reading's.
    #[cfg(test)]
    pub(crate) fn sum(&self) -> Duration {
        [
            self.open,
            self.trends,
            self.facts,
            self.archive,
            self.cost,
            self.storage,
            self.health,
            self.lock_wait,
        ]
        .into_iter()
        .flatten()
        .sum()
    }

    /// `open_ms=<n|none> trends_ms=… facts_ms=… archive_ms=… cost_ms=…
    /// storage_ms=… health_ms=… lock_wait_ms=… performance=<measured|kept|none>`.
    pub(crate) fn text(&self) -> String {
        let ms = |part: Option<Duration>| {
            part.map_or_else(|| "none".to_string(), |took| took.as_millis().to_string())
        };
        format!(
            "open_ms={} trends_ms={} facts_ms={} archive_ms={} cost_ms={} storage_ms={} \
health_ms={} lock_wait_ms={} performance={}",
            ms(self.open),
            ms(self.trends),
            ms(self.facts),
            ms(self.archive),
            ms(self.cost),
            ms(self.storage),
            ms(self.health),
            ms(self.lock_wait),
            self.performance.unwrap_or("none")
        )
    }
}

/// `work`'s result, its time added to `part`.
fn timed<T>(part: &mut Option<Duration>, work: impl FnOnce() -> T) -> T {
    let begun = Instant::now();
    let result = work();
    *part = Some(part.unwrap_or_default() + begun.elapsed());
    result
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
/// file and the nightly's fact files read now, through the dashboard's own
/// read-only connection and its statement timeout. It is the whole
/// Scorecard and more (cc-astra A18: a minute's cache has always expired
/// at the step's 300 s), so the step runs it on a thread of its own and
/// never waits for it (crate alerts/guardrails.rs). It comes with how long
/// each of its parts took.
pub(crate) fn reading() -> Result<(Value, Parts), String> {
    let mut parts = Parts::default();
    let store = timed(&mut parts.open, super::open_store)?;
    let view = core_lines_timed(
        &store,
        ranges_path().as_deref(),
        nightly_dir().as_deref(),
        &mut parts,
    )?;
    Ok((view, parts))
}

/// The answer with the ranges file at `path` and the nightly's fact files
/// in `nightly`, read now, with the trends of the last 7 days' samples.
pub(super) fn core_lines_at(
    store: &spawn_postgres::PostgresStore,
    path: Option<&Path>,
    nightly: Option<&Path>,
) -> Result<Value, String> {
    core_lines_timed(store, path, nightly, &mut Parts::default())
}

/// [`core_lines_at`], each part's time put in `parts`.
pub(super) fn core_lines_timed(
    store: &spawn_postgres::PostgresStore,
    path: Option<&Path>,
    nightly: Option<&Path>,
    parts: &mut Parts,
) -> Result<Value, String> {
    let now_ms = super::now_ms();
    let contents = timed(&mut parts.open, || read_ranges(path));
    let text = contents.as_ref().ok().and_then(Option::as_deref);
    let (figures, extras) = lines_at(store, nightly, text, now_ms, parts)?;
    let samples = timed(&mut parts.trends, || {
        store
            .host_core_lines(trend_since(now_ms))
            .map(|samples| (samples, store.first_deploy_finished_ms().ok().flatten()))
    })
    .map_err(super::err)?;
    let samples = measured_samples(samples.0, samples.1);
    let file = path.map_or(Value::Null, |path| json!(path.display().to_string()));
    let mut view = answer(&figures, &file, contents, &trends(&samples, now_ms));
    view["drills"] = extras.drills;
    view["scratch"] = extras.scratch;
    view["test_schemas"] = extras.test_schemas;
    // A URL file refused (objective 094) is said where a range left out is.
    if let Some(ignored) = view["ignored_ranges"].as_array_mut() {
        for (name, why) in extras.refused {
            ignored.push(json!({
                "key": format!("storage.test_database_url_files.{name}"),
                "reason": why,
            }));
        }
    }
    Ok(view)
}

/// What the answer gains beside the lines: the drills (objective 093), the
/// scratch nobody owns, each test server's schemas and the URL files
/// refused (objective 094).
struct Extras {
    drills: Value,
    scratch: Value,
    test_schemas: Value,
    refused: Vec<(String, String)>,
}

/// The four lines' figures, read at `now_ms` with the nightly's fact files
/// in `nightly` and the Core lines file's contents `file` (its
/// `recoverability` drills, objective 093; its `storage` scratch roots and
/// test servers, objective 094), and what the answer gains beside them;
/// each part's time put in `parts`.
fn lines_at(
    store: &spawn_postgres::PostgresStore,
    nightly: Option<&Path>,
    file: Option<&str>,
    now_ms: i64,
    parts: &mut Parts,
) -> Result<(Vec<Line>, Extras), String> {
    let (mut storage, unowned) = timed(&mut parts.storage, || {
        super::storage::report(store, now_ms).map(|storage| (storage, unowned::now(file, now_ms)))
    })?;
    let reading = unowned.reading.as_deref();
    storage["unowned_scratch_bytes"] = reading.map_or(Value::Null, unowned::Reading::unowned_bytes);
    storage["leaked_test_schemas"] = reading.map_or(Value::Null, unowned::Reading::leaked);
    let (performance, cost_total) = performance_now(store, now_ms, parts);
    // The Home card's figure (objective 100): `/api/interventions?window=24h`
    // over the 24 h ending at this reading's now, through the same answer.
    let interventions = timed(&mut parts.health, || {
        let (name, window) =
            crate::scorecard::interventions::window_of(Some("24h"), None, || Ok(now_ms))?;
        crate::scorecard::interventions::measure(store, name, window)
    })
    .map_err(|failure| match failure {
        crate::scorecard::Failure::Request(error) | crate::scorecard::Failure::Record(error) => {
            error
        }
    })?;
    let observed = Observed {
        cost_total,
        human_per_agent_hour: interventions["per_agent_hour"].clone(),
        // Not collected until A22/A23 land (owner, 2026-11-12): the share of
        // alerts whose condition cleared them is not whether they were right
        // (sync/astra/findings.md A23), so it is not shown as the north star.
        needs_you_right: Value::Null,
    };
    let (backups, facts, drills) = timed(&mut parts.facts, || {
        store.newest_backups().map(|backups| {
            (
                backups,
                nightly_facts(nightly),
                drills::read(store, file, now_ms),
            )
        })
    })
    .map_err(super::err)?;
    let mut guardrails = recoverability(&backups, facts.as_deref(), now_ms);
    guardrails.extend(drills.guardrails);
    Ok((
        lines(
            &storage,
            &performance,
            &observed,
            drills.north_star.value,
            guardrails,
        ),
        Extras {
            drills: drills.list,
            scratch: unowned::scratch_json(&unowned.plan, reading),
            test_schemas: unowned::test_schemas_json(reading),
            refused: unowned.refused,
        },
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
    let contents = read_ranges(ranges_path().as_deref());
    let text = contents.as_ref().ok().and_then(Option::as_deref);
    let (lines, _) = lines_at(
        &store,
        nightly_dir().as_deref(),
        text,
        super::now_ms(),
        &mut Parts::default(),
    )?;
    Ok(values_of(&lines))
}

#[cfg(test)]
mod tests {
    use super::{Figure, Observed, figure_json, lines, parse_ranges, read_ranges};
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

    /// Objective 090: a sample holds every figure that has a value, and
    /// none without.
    #[test]
    fn a_sample_holds_every_figure_with_a_value() {
        let figures = lines(
            &json!({"free_bytes": 3, "days_left": 9}),
            &Value::Null,
            &Observed::default(),
            Value::Null,
            Vec::new(),
        );
        let values = super::values_of(&figures);
        // The count not measured always has a value (objective 088): the
        // other 15 figures that have none (objective 094's two among them).
        assert_eq!(
            Value::Object(values),
            json!({"storage.free_bytes": 3, "storage.days_left": 9,
                   "observability.not_measured": 15})
        );
    }
}
