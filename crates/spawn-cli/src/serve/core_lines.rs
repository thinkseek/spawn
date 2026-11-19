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
//! `measuring_since_ms`, when its measure starts
//! (`performance.report_to_live`, below, and `storage.days_left`, [`disk`]).
//! A figure that names a store (`storage.grew_most`) says it as `store`.
//! `trend` (objective 090) is the
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
//! ([`super::storage::report`]), but for days left (objective 141): the
//! free disk over its own fall, a 7-day fit of its daily readings, beside
//! which the line gives that fall and the store that grew most ([`disk`]). The observability line's guardrails
//! (objective 088) are the share of the 7-day cost window's tokens no price
//! covers (`/api/cost?window=7d`'s total, kept with the performance figures:
//! unpriced_tokens over its input, output, cache write and cache read
//! tokens; null with none), the Home card's human interventions per
//! agent-hour (`/api/interventions?window=24h`'s `per_agent_hour`, its last
//! 24 h, objective 100; the figure says `"window": "24h"`) on
//! `/api/fleet`, and how many figures of this same answer have no value,
//! itself left out. Its north star, "Needs you is right" (objective 148),
//! is the owner's judgements (`spawn alert judge`) of the needs (alerts,
//! never notices) fired in the last 7 days: right ÷ judged, no value under
//! 5 judged, said with `judged: {fired, judged, unjudged}` (none when not
//! read); a clearing is never evidence. The recoverability guardrails (objective 087,
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
//! shaped `{"ranges": {<figure key>: {"min"?: number, "max"?: number,
//! "target"?: number}}}`, read at every request, so an edit shows without a
//! restart. A `target` (objective 181) is said as the figure's `target`,
//! apart from its range, and never judged against: an entry with a target
//! alone gives no range. An entry whose key names no figure, or with
//! neither a number min or max nor a target, is left out and listed in
//! `ignored_ranges`. A missing file is no ranges and no error; a file that
//! is not such JSON is no ranges and a `ranges_error`. Nothing but `spawn
//! serve` and the loop's alert step ([`reading`], `guardrail_out_of_range`,
//! on a thread of its own) read the file, and no gate, integrator or deploy step reads the answer:
//! monitoring only.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

mod disk;
mod drills;
// The groups are used by objectives 2 and 3 (performance.rs, storage.rs;
// sync/design-core-lines-kept-figures.md), which land after this one: until
// then nothing but the tests makes a group. `expect`, so the first of them
// to use it must take this line out.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "kept groups land before the groups that use them (objectives 2 and 3)"
    )
)]
pub(crate) mod kept;
mod observability;
mod performance;
mod quiet;
mod recoverability;
mod schemas;
mod scratch;
mod storage;
mod trends;
mod unowned;

use observability::{Observed, count_not_measured, observability_line};
use performance::{measured_samples, performance_line, performance_now};
use quiet::{samples_since, zero_for_a_week};
use recoverability::{nightly_facts, recoverability};
use storage::storage_line;
use trends::trends;

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
    /// When a figure of a kept group ([`kept`]) was read: said as
    /// `as_of_ms`; `None` for a figure read by the reading itself.
    pub(super) as_of_ms: Option<i64>,
    /// Whether its group is past its stale line: said as `stale`, shown as
    /// stale, never as current, and left out of the hourly sample.
    pub(super) stale: bool,
    /// The store a figure names (objective 141: the store that grew most):
    /// said as `store`.
    pub(super) store: Option<String>,
    /// What a judged share is read from (objective 148: "Needs you is
    /// right"), `{fired, judged, unjudged}`: said as `judged`; `None` when
    /// not read.
    pub(super) judged: Option<Value>,
    /// Each mission's figure beside the fleet's (objective 159: the
    /// performance north star's report to live by mission): said as
    /// `by_mission`; `None` on every other figure.
    pub(super) by_mission: Option<Value>,
    /// A figure of the adopted set no code computes yet (objective 176: the
    /// storage north star, S1 and S2): its value is null and its state
    /// `not_measured_yet`, never a made-up value.
    pub(super) not_measured_yet: bool,
    /// Why a figure is not measured, or what it was read from (objective
    /// 185: the storage north star and S1): said as `note`.
    pub(super) note: Option<String>,
    /// The figure split by source (objective 185): said as `by_source`.
    pub(super) by_source: Option<Value>,
    /// The figure split by agent and objective (objective 232: the scratch
    /// of landed objectives): said as `by_owner`.
    pub(super) by_owner: Option<Value>,
    /// What a count is out of (objective 181: U1 out of the recovery
    /// table's rows): said as `of`.
    pub(super) of: Option<usize>,
    /// Why the ranges file's range is not applied yet (objective 181: U2
    /// in its first week of judged data): said as `range_note`; while it is
    /// set the figure has no range, so it is never out.
    pub(super) range_note: Option<String>,
    /// What a figure's value is read from, when it says (objective 222:
    /// storage.disk_fall's `fit` or `day`): said as `basis`.
    pub(super) basis: Option<String>,
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
            as_of_ms: None,
            stale: false,
            store: None,
            judged: None,
            by_mission: None,
            not_measured_yet: false,
            note: None,
            by_source: None,
            by_owner: None,
            of: None,
            range_note: None,
            basis: None,
        }
    }

    /// A figure of the adopted set no code computes yet (objective 176).
    fn not_measured_yet(key: &'static str, name: &'static str, unit: &'static str) -> Self {
        Figure {
            not_measured_yet: true,
            ..Figure::new(key, name, unit, Value::Null)
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

/// What the ranges file gave: the ranges by key, the targets by key
/// (objective 181: said beside the figure, never judged against), the
/// entries left out with why, and why the file could not be read, if it
/// could not.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Ranges {
    pub(super) bounds: BTreeMap<String, Range>,
    pub(super) targets: BTreeMap<String, f64>,
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
        let target = bound("target");
        if let Some(target) = target {
            ranges.targets.insert(key.clone(), target);
        }
        if range.min.is_none() && range.max.is_none() {
            // A target alone gives no range, and is not left out.
            if target.is_none() {
                ranges
                    .ignored
                    .push((key.clone(), "needs a number min or max"));
            }
            continue;
        }
        ranges.bounds.insert(key.clone(), range);
    }
    ranges.ignored.sort();
    ranges
}

/// One UTC day, in milliseconds.
const DAY_MS: i64 = 86_400_000;

/// A figure as the answer gives it: with its range and its state. A
/// figure with a `range_note` takes no range (objective 181).
pub(super) fn figure_json(figure: &Figure, range: Option<Range>) -> Value {
    let range = range.filter(|_| figure.range_note.is_none());
    let state = match (figure.value.as_f64(), range) {
        (None, _) if figure.not_measured_yet => "not_measured_yet",
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
    if let Some(as_of) = figure.as_of_ms {
        answer["as_of_ms"] = json!(as_of);
    }
    if figure.stale {
        answer["stale"] = json!(true);
    }
    if let Some(store) = &figure.store {
        answer["store"] = json!(store);
    }
    if let Some(judged) = &figure.judged {
        answer["judged"] = judged.clone();
    }
    if let Some(by_mission) = &figure.by_mission {
        answer["by_mission"] = by_mission.clone();
    }
    if let Some(note) = &figure.note {
        answer["note"] = json!(note);
    }
    if let Some(by_source) = &figure.by_source {
        answer["by_source"] = by_source.clone();
    }
    if let Some(by_owner) = &figure.by_owner {
        answer["by_owner"] = by_owner.clone();
    }
    if let Some(of) = figure.of {
        answer["of"] = json!(of);
    }
    if let Some(range_note) = &figure.range_note {
        answer["range_note"] = json!(range_note);
    }
    if let Some(basis) = &figure.basis {
        answer["basis"] = json!(basis);
    }
    answer
}

/// The kept group a figure is read from (sync/design-core-lines-kept-figures.md):
/// the performance figures and the cost window's unpriced share are the
/// `performance` group's; the storage figures from the sizes (the free
/// space aside, read at each reading; the scratch and test schemas aside,
/// kept by [`unowned`] with their own age) are the `storage` group's.
pub(super) fn group_of(key: &str) -> Option<&'static str> {
    match key {
        "observability.unpriced_share" => Some("performance"),
        "storage.days_left"
        | "storage.archive_growth"
        | "storage.grew_most"
        | "storage.database_ratio"
        | "storage.test_database_bytes"
        | "storage.test_database_schemas" => Some("storage"),
        _ if key.starts_with("performance.") => Some("performance"),
        _ => None,
    }
}

/// Each figure of a kept group in `statuses` said with when its group was
/// read and whether it is stale at `now_ms`; a group not in use leaves its
/// figures as the reading read them.
pub(super) fn aged(lines: &mut [Line], statuses: &[kept::Status], now_ms: i64) {
    for line in lines {
        for figure in std::iter::once(&mut line.north_star).chain(&mut line.guardrails) {
            let Some(status) = group_of(figure.key)
                .and_then(|group| statuses.iter().find(|status| status.group == group))
            else {
                continue;
            };
            figure.as_of_ms = status.as_of_ms;
            figure.stale = status.stale(now_ms);
        }
    }
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

/// The figures the owner's core metric set dropped (objective 176): still
/// read (other pages read them), in none of the answer's lists, and a range
/// the file gives one is listed in `ignored_ranges` with [`RETIRED_REASON`].
pub(super) const RETIRED: [&str; 5] = [
    "storage.days_left",
    "storage.database_ratio",
    "storage.test_database_bytes",
    "storage.test_database_schemas",
    "observability.not_measured",
];

/// Why a dropped figure's range is ignored.
pub(super) const RETIRED_REASON: &str = "retired by the owner's core metric set (2026-11-14)";

/// The figures demoted to an alert only, each with its line, in order;
/// one the reading did not make (the drills' ages without a recovery table)
/// is left out. Leaked test schemas joins them, last, once it has read 0
/// for a week ([`LEAKED_SCHEMAS`]).
const ALERT_FIGURES: [(&str, &str); 5] = [
    ("sustainability", "recoverability.cloud_bundle_age"),
    ("sustainability", "recoverability.nightly_age"),
    ("sustainability", "recoverability.nightly_red"),
    ("sustainability", "recoverability.session_backup_age"),
    ("sustainability", "recoverability.oldest_drill_age"),
];

/// The diagnostics on the storage page, in order; none raises an alert but
/// those in [`ALERTING_DIAGNOSTICS`]. Free disk (objective 182) is first,
/// with no range: its alert is disk_low's (objective 204), and its fall is
/// how a sweep's effect is read. Leaked test schemas leaves them once it has
/// read 0 for a week ([`LEAKED_SCHEMAS`]). The scratch of landed objectives
/// (objective 232) is last, with no range.
const DIAGNOSTICS: [&str; 7] = [
    "storage.free_bytes",
    "storage.unowned_scratch_bytes",
    "storage.disk_fall",
    "storage.grew_most",
    "storage.archive_growth",
    "storage.leaked_test_schemas",
    "storage.landed_scratch_bytes",
];

/// Leaked test schemas (objective 182): "stays visible until it has read 0
/// for 7 days, then an alert only" ([`zero_for_a_week`]).
const LEAKED_SCHEMAS: &str = "storage.leaked_test_schemas";

/// Each line's plain readings (objective 182), with the line: never a
/// north star, guardrail, alert figure or diagnostic, so raising nothing.
/// Dollars per landing is "shown under sustainability as a plain reading",
/// with "no range until use with no price is under 10%": it takes its
/// configured range while the 7-day cost window is priced ([`priced`],
/// objective 207), and has none otherwise.
const READINGS: [(&str, &str); 1] = [("sustainability", USD_PER_LANDING)];

/// Dollars per landing.
const USD_PER_LANDING: &str = "performance.usd_per_landing";

/// Use with no price: the share of the 7-day cost window's tokens no price
/// covers, read from the same cost summary as dollars per landing.
const UNPRICED_SHARE: &str = "observability.unpriced_share";

/// Under this share of unpriced use, dollars per landing takes its range.
const PRICED_UNDER: f64 = 0.10;

/// What a reading says where its range would be.
const READING_NOTE: &str = "no range until use with no price is under 10%";

/// Whether the 7-day cost window is priced enough for dollars per landing
/// to take its configured range (objective 207): use with no price is a
/// fresh number under 10%. Missing (no tokens in the window), not a number,
/// stale, or 10% or more, it is not.
fn priced(share: Option<&Figure>) -> bool {
    share.is_some_and(|share| {
        !share.stale
            && share
                .value
                .as_f64()
                .is_some_and(|value| value < PRICED_UNDER)
    })
}

/// The figures a ranges file entry is never applied to, with why: said in
/// `ignored_ranges` (objective 182).
const NO_RANGE: [(&str, &str); 3] = [
    (
        "storage.free_bytes",
        "an alert of its own (disk_low, objective 204)",
    ),
    (
        "performance.usd_per_landing",
        "shown with no range until use with no price is under 10% (the owner's core metric set)",
    ),
    (
        "storage.landed_scratch_bytes",
        "a diagnostic with no range: whose scratch it is, never out (objective 232)",
    ),
];

/// The diagnostics that still raise their out-of-range alert, each with its
/// line (rev-e's 176 R1 F1): leaked test schemas "stays visible until it has
/// read 0 for 7 days, then an alert only" (objective 182); until 182 it
/// raises while it is shown.
const ALERTING_DIAGNOSTICS: [(&str, &str); 1] = [("storage.leaked_test_schemas", "storage")];

/// The line a diagnostic raises its alert under, if it raises one.
pub(super) fn alerting_line(key: &str) -> Option<&'static str> {
    ALERTING_DIAGNOSTICS
        .iter()
        .find(|(diagnostic, _)| *diagnostic == key)
        .map(|(_, line)| *line)
}

/// The core metric set the owner adopted on 2026-11-14 (objective 176):
/// three lines (storage, performance, sustainability), each line's plain
/// readings (objective 182), the figures demoted to an alert with their
/// lines, and the diagnostics.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Set {
    pub(super) lines: Vec<Line>,
    /// The lines' plain readings, each with its line, in order.
    pub(super) readings: Vec<(&'static str, Figure)>,
    pub(super) alert_figures: Vec<(&'static str, Figure)>,
    pub(super) diagnostics: Vec<Figure>,
}

impl Set {
    /// Every figure of the set: the lines', their readings, the alert
    /// figures and the diagnostics.
    pub(super) fn figures(&self) -> impl Iterator<Item = &Figure> {
        self.lines
            .iter()
            .flat_map(|line| std::iter::once(&line.north_star).chain(&line.guardrails))
            .chain(self.readings.iter().map(|(_, figure)| figure))
            .chain(self.alert_figures.iter().map(|(_, figure)| figure))
            .chain(&self.diagnostics)
    }
}

/// The adopted set from `readings` ([`adopted`]), leaked test schemas an
/// alert figure only when it has read 0 for a week by the `host.core_lines`
/// `samples` at `now_ms` ([`zero_for_a_week`]).
pub(super) fn adopted_at(readings: &[Line], samples: &Value, now_ms: i64) -> Set {
    let now = readings
        .iter()
        .flat_map(|line| std::iter::once(&line.north_star).chain(&line.guardrails))
        .find(|figure| figure.key == LEAKED_SCHEMAS)
        .map_or(Value::Null, |figure| figure.value.clone());
    adopted(
        readings,
        zero_for_a_week(LEAKED_SCHEMAS, &now, samples, now_ms),
    )
}

/// The adopted set from `readings`, the figures [`lines`] reads, picked by
/// key: keys stay as they were (trends and ranges files keep working). A
/// line's figure the readings lack is not collected; a reading, an alert
/// figure or a diagnostic they lack is left out. Leaked test schemas is the
/// last alert figure when `leaked_quiet` (it has read 0 for a week), else a
/// diagnostic.
pub(super) fn adopted(readings: &[Line], leaked_quiet: bool) -> Set {
    let find = |key: &str| {
        readings
            .iter()
            .flat_map(|line| std::iter::once(&line.north_star).chain(&line.guardrails))
            .find(|figure| figure.key == key)
            .cloned()
    };
    let card = |key: &'static str, name: &'static str, unit: &'static str| {
        find(key).unwrap_or_else(|| Figure::not_collected(key, name, unit))
    };
    // The storage north star and S1 are measured from the storage readings
    // (objective 185); S2 is not measured yet (objective 184).
    let measured = |key: &'static str, name: &'static str, unit: &'static str| {
        find(key).unwrap_or_else(|| Figure::not_measured_yet(key, name, unit))
    };
    let lines = vec![
        Line {
            name: "storage",
            north_star: measured(
                "storage.bytes_per_landing",
                "Bytes a landing leaves behind",
                "bytes",
            ),
            guardrails: vec![
                measured("storage.rebuildable_bytes", "Rebuildable bytes", "bytes"),
                Figure::not_measured_yet("storage.kept_share", "Share kept on purpose", "share"),
            ],
        },
        Line {
            name: "performance",
            north_star: card(
                "performance.report_to_live",
                "Time from report to live",
                "ms",
            ),
            guardrails: vec![
                card("performance.gate_time", "Gate time per run", "ms"),
                card(
                    "performance.decided_nothing",
                    "Gate time that decided nothing",
                    "share",
                ),
                card(
                    "performance.first_pass",
                    "Landings that pass first time",
                    "share",
                ),
            ],
        },
        Line {
            name: "sustainability",
            north_star: card(
                "observability.interventions_per_agent_hour",
                "Human interventions per agent-hour",
                "ratio",
            ),
            guardrails: vec![
                card(
                    "recoverability.restores_passed",
                    "Critical resources with a restore passed in 7 days",
                    "count",
                ),
                card(
                    "observability.needs_you_right",
                    "Needs you is right",
                    "share",
                ),
                card("observability.unpriced_share", "Use with no price", "share"),
                card(
                    "recoverability.cloud_dump_age",
                    "Age of the newest database copy",
                    "ms",
                ),
            ],
        },
    ];
    let leaked = leaked_quiet.then_some(("storage", LEAKED_SCHEMAS));
    let priced = priced(find(UNPRICED_SHARE).as_ref());
    Set {
        lines,
        readings: READINGS
            .iter()
            .filter_map(|(line, key)| {
                find(key).map(|figure| (*line, plain_reading(figure, priced)))
            })
            .collect(),
        alert_figures: ALERT_FIGURES
            .iter()
            .chain(&leaked)
            .filter_map(|(line, key)| find(key).map(|figure| (*line, figure)))
            .collect(),
        diagnostics: DIAGNOSTICS
            .iter()
            .filter(|key| !(leaked_quiet && **key == LEAKED_SCHEMAS))
            .filter_map(|key| find(key))
            .collect(),
    }
}

/// `figure` as a line's plain reading: with its configured range while
/// `priced` ([`priced`]), else no range and its note where the range would
/// be.
fn plain_reading(figure: Figure, priced: bool) -> Figure {
    if priced {
        return figure;
    }
    Figure {
        range_note: Some(READING_NOTE.to_string()),
        ..figure
    }
}

/// The file's entry for a figure in [`NO_RANGE`], whatever it gives (a
/// bound, a target, or neither), not applied: listed in `ignored` with why.
/// Dollars per landing's is applied as configured while `priced`.
fn no_range(ranges: &mut Ranges, priced: bool) {
    for (key, reason) in NO_RANGE {
        if priced && key == USD_PER_LANDING {
            continue;
        }
        let bound = ranges.bounds.remove(key).is_some();
        let target = ranges.targets.remove(key).is_some();
        let listed = ranges.ignored.iter().any(|(ignored, _)| ignored == key);
        if bound || target || listed {
            ranges.ignored.retain(|(ignored, _)| ignored != key);
            ranges.ignored.push((key.to_string(), reason));
        }
    }
    ranges.ignored.sort();
}

/// The answer from the lines, the ranges file's path (null when there is
/// no workspace root to find it under) and its contents
/// (`Err` when it could not be read for another reason than not being
/// there; `Ok(None)` when it is not there), every figure with its trend
/// from `trends` ([`trends`]; empty without one).
pub(super) fn answer(
    set: &Set,
    ranges_file: &Value,
    contents: Result<Option<String>, String>,
    trends: &BTreeMap<String, Vec<Value>>,
) -> Value {
    let known: Vec<&str> = set.figures().map(|figure| figure.key).collect();
    let mut ranges = match contents {
        Ok(text) => {
            let mut ranges = parse_ranges(text.as_deref(), &known);
            let share = set.figures().find(|figure| figure.key == UNPRICED_SHARE);
            no_range(&mut ranges, priced(share));
            ranges
        }
        Err(error) => Ranges {
            error: Some(error),
            ..Ranges::default()
        },
    };
    // A range of a figure the owner's set dropped is said so, not unknown.
    for (key, reason) in &mut ranges.ignored {
        if RETIRED.contains(&key.as_str()) && *reason == "unknown figure" {
            *reason = RETIRED_REASON;
        }
    }
    let lines = &set.lines;
    let with_range = |figure: &Figure| {
        let mut view = figure_json(figure, ranges.bounds.get(figure.key).copied());
        if let Some(target) = ranges.targets.get(figure.key) {
            view["target"] = number(*target);
        }
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
                "readings": set
                    .readings
                    .iter()
                    .filter(|(name, _)| *name == line.name)
                    .map(|(_, figure)| with_range(figure))
                    .collect::<Vec<_>>(),
            }))
            .collect::<Vec<_>>(),
        "alert_figures": set
            .alert_figures
            .iter()
            .map(|(line, figure)| {
                let mut view = with_range(figure);
                view["line"] = json!(line);
                view
            })
            .collect::<Vec<_>>(),
        "diagnostics": set
            .diagnostics
            .iter()
            .map(|figure| {
                let mut view = with_range(figure);
                if let Some(line) = alerting_line(figure.key) {
                    view["line"] = json!(line);
                }
                view
            })
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

/// The queue the ranges file names for the owner's interventions
/// (objective 156): none without a ranges file, one that cannot be read,
/// or no absolute `observability.queue_file`.
pub(crate) fn queue_settings() -> Option<crate::scorecard::queue_posts::Settings> {
    let text = read_ranges(ranges_path().as_deref()).ok().flatten();
    crate::scorecard::queue_posts::settings(text.as_deref())
}

/// What the daily storage reading (objective 180, crate storage_bytes.rs)
/// reads from the Core lines file, re-read at each reading.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct StorageBytesSettings {
    /// `storage.scratch_roots`, less what `storage.scratch_exclude` names.
    pub(crate) scratch_roots: Vec<PathBuf>,
    pub(crate) scratch_exclude: Vec<PathBuf>,
    /// `storage.quiet_hold_file`, else `SPAWN_QUIET_HOLD_FILE`; `None`: no
    /// such check.
    pub(crate) quiet_hold_file: Option<PathBuf>,
    /// The configured test databases by role or name, with their URLs (the
    /// env's, then the file's `storage.test_database_url_files` taken).
    /// Never printed.
    pub(crate) test_databases: Vec<(String, String)>,
}

/// The Core lines file's settings for the daily storage reading: an error
/// when the file is there but cannot be read (the quiet-hold file it may
/// name is then unknown).
pub(crate) fn storage_bytes_settings() -> Result<StorageBytesSettings, String> {
    let text = read_ranges(ranges_path().as_deref())?;
    Ok(storage_bytes_settings_of(
        text.as_deref(),
        crate::env_value("SPAWN_QUIET_HOLD_FILE"),
    ))
}

/// [`storage_bytes_settings`] of the file's `text` (`None`: no file), with
/// `SPAWN_QUIET_HOLD_FILE`'s value `quiet_env`.
pub(crate) fn storage_bytes_settings_of(
    text: Option<&str>,
    quiet_env: Option<String>,
) -> StorageBytesSettings {
    let settings = unowned::settings(text);
    let quiet_hold_file = text
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .and_then(|value| {
            value["storage"]["quiet_hold_file"]
                .as_str()
                .map(str::to_string)
        })
        .filter(|path| !path.trim().is_empty())
        .or_else(|| quiet_env.filter(|path| !path.trim().is_empty()))
        .map(PathBuf::from);
    let (plan, _) = unowned::plan(&settings);
    StorageBytesSettings {
        scratch_roots: settings.roots,
        scratch_exclude: settings.excludes,
        quiet_hold_file,
        test_databases: plan.servers,
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
    /// The storage report (`serve/storage.rs`: its sizes, kept 10 min,
    /// and the free space).
    pub(crate) storage: Option<Duration>,
    /// The scratch nobody owns and each test server's schemas
    /// ([`unowned`]: kept, refreshed beside the reading; only a process's
    /// first reading waits). Apart from `storage` (cc-astra), so a slow
    /// reading says which of the two it was.
    pub(crate) unowned: Option<Duration>,
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
    /// How old the kept `performance` group served was (none while no
    /// group of that name is in use): with the refresh's own log line, what
    /// the acceptance reads beside the reading's time.
    pub(crate) performance_age_ms: Option<i64>,
    /// How old the kept `storage` group served was.
    pub(crate) storage_age_ms: Option<i64>,
    /// Whether any kept group is in use: only then does the line say the
    /// two ages (until objectives 2 and 3 register theirs, it is as it was).
    pub(crate) aged: bool,
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
            self.unowned,
            self.health,
            self.lock_wait,
        ]
        .into_iter()
        .flatten()
        .sum()
    }

    /// Each kept group's age at `now_ms`, from `statuses`.
    pub(crate) fn ages(&mut self, statuses: &[kept::Status], now_ms: i64) {
        let age = |group: &str| {
            statuses
                .iter()
                .find(|status| status.group == group)
                .and_then(|status| status.age_ms(now_ms))
        };
        self.performance_age_ms = age("performance");
        self.storage_age_ms = age("storage");
        self.aged = !statuses.is_empty();
    }

    /// `open_ms=<n|none> trends_ms=… facts_ms=… archive_ms=… cost_ms=…
    /// storage_ms=… unowned_ms=… health_ms=… lock_wait_ms=… performance=<measured|kept|none>`,
    /// then, while a kept group is in use, ` performance_age_ms=<n|none>
    /// storage_age_ms=<n|none>`.
    pub(crate) fn text(&self) -> String {
        let ms = |part: Option<Duration>| {
            part.map_or_else(|| "none".to_string(), |took| took.as_millis().to_string())
        };
        let age = |age: Option<i64>| age.map_or_else(|| "none".to_string(), |age| age.to_string());
        let ages = if self.aged {
            format!(
                " performance_age_ms={} storage_age_ms={}",
                age(self.performance_age_ms),
                age(self.storage_age_ms)
            )
        } else {
            String::new()
        };
        format!(
            "open_ms={} trends_ms={} facts_ms={} archive_ms={} cost_ms={} storage_ms={} \
unowned_ms={} health_ms={} lock_wait_ms={} performance={}{ages}",
            ms(self.open),
            ms(self.trends),
            ms(self.facts),
            ms(self.archive),
            ms(self.cost),
            ms(self.storage),
            ms(self.unowned),
            ms(self.health),
            ms(self.lock_wait),
            self.performance.unwrap_or("none"),
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

/// The loop's first guardrail reading (objective 192): [`reading`], plus
/// the `host.core_lines` samples recorded at or after `since` on the same
/// connection, as `recent_samples` (`[{at_ms, values}]`, oldest first), or
/// why they could not be read, as `recent_samples_error`. A figure out at
/// that reading takes its out-since from them (crate alerts/guardrails.rs),
/// so a restart does not restart the raise hold. Never part of `GET
/// /api/core-lines`; later readings are [`reading`]'s.
pub(crate) fn reading_with_samples(since: i64) -> Result<(Value, Parts), String> {
    let mut parts = Parts::default();
    let store = timed(&mut parts.open, super::open_store)?;
    let mut view = core_lines_timed(
        &store,
        ranges_path().as_deref(),
        nightly_dir().as_deref(),
        &mut parts,
    )?;
    match timed(&mut parts.trends, || store.host_core_lines(since)) {
        Ok(samples) => view["recent_samples"] = samples,
        Err(error) => view["recent_samples_error"] = json!(super::err(error)),
    }
    Ok((view, parts))
}

/// Warm the reading at serve's start (the lead, 2026-11-13: the first GET
/// after each restart took about 3 s, later ones 0.05 s): one reading run
/// now, as a request would run it, so the kept performance figures, the
/// storage sizes and the archive's parsed files are there for the first
/// person. Its line says each part's time, so the first deploy shows which
/// part the cold start costs. No store read beyond the request's own, and
/// not on the alert step's path.
pub(super) fn warm() -> String {
    let begun = Instant::now();
    let warmed = super::open_store()
        .and_then(|store| warm_at(&store, ranges_path().as_deref(), nightly_dir().as_deref()));
    let took = begun.elapsed().as_millis();
    match warmed {
        Ok(parts) => format!("core lines warmed in {took} ms: {}", parts.text()),
        Err(error) => format!("core lines not warmed ({took} ms): {error}"),
    }
}

/// [`warm`]'s reading through `store`, its parts.
pub(super) fn warm_at(
    store: &spawn_postgres::PostgresStore,
    path: Option<&Path>,
    nightly: Option<&Path>,
) -> Result<Parts, String> {
    let mut parts = Parts::default();
    core_lines_timed(store, path, nightly, &mut parts)?;
    Ok(parts)
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
    // The week's samples, read once: the trends and the disk's readings
    // (objective 141) both come from them, each filtering its own window,
    // and whether leaked test schemas has read 0 for the week (objective
    // 182).
    let samples = timed(&mut parts.trends, || {
        store
            .host_core_lines(samples_since(now_ms))
            .map(|samples| (samples, store.first_deploy_finished_ms().ok().flatten()))
    })
    .map_err(super::err)?;
    let (figures, extras) = lines_at(store, nightly, text, &samples.0, now_ms, parts)?;
    let set = adopted_at(&figures, &samples.0, now_ms);
    let samples = measured_samples(samples.0, samples.1);
    let file = path.map_or(Value::Null, |path| json!(path.display().to_string()));
    let mut view = answer(&set, &file, contents, &trends(&samples, now_ms));
    view["drills"] = extras.drills;
    with_groups(&mut view, extras.groups);
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

/// `groups` added to the answer only when a kept group is in use: with none
/// (until objectives 2 and 3 register theirs) the answer is as it was.
pub(super) fn with_groups(view: &mut Value, groups: Vec<Value>) {
    if !groups.is_empty() {
        view["groups"] = Value::Array(groups);
    }
}

/// What the answer gains beside the lines: the drills (objective 093), the
/// scratch nobody owns, each test server's schemas and the URL files
/// refused (objective 094).
struct Extras {
    drills: Value,
    /// Each kept group in use: its age, stale line and last error.
    groups: Vec<Value>,
    scratch: Value,
    test_schemas: Value,
    refused: Vec<(String, String)>,
}

/// The four lines' figures, read at `now_ms` with the nightly's fact files
/// in `nightly` and the Core lines file's contents `file` (its
/// `recoverability` drills, objective 093; its `storage` scratch roots and
/// test servers, objective 094), and what the answer gains beside them;
/// the storage line's days left from the disk's own fall over the window's
/// `host.core_lines` `samples` ([`disk`], objective 141); each part's time
/// put in `parts`.
fn lines_at(
    store: &spawn_postgres::PostgresStore,
    nightly: Option<&Path>,
    file: Option<&str>,
    samples: &Value,
    now_ms: i64,
    parts: &mut Parts,
) -> Result<(Vec<Line>, Extras), String> {
    let mut storage = timed(&mut parts.storage, || super::storage::report(store, now_ms))?;
    // The storage readings (objective 180), the newest and the newest a day
    // older with the landings between them, which the storage north star
    // and S1 are measured from (objective 185). A failed read is no figure,
    // not a failed answer.
    storage["readings"] = timed(&mut parts.storage, || {
        store
            .host_storage_bytes()
            .map_err(|error| eprintln!("core-lines: the storage readings: {error}"))
            .unwrap_or(Value::Null)
    });
    let unowned = timed(&mut parts.unowned, || unowned::now(file, now_ms));
    let reading = unowned.reading.as_deref();
    // Whether each claimed folder's objective landed (objective 140): read
    // only when a folder is claimed, once for all of them; None when the
    // store cannot say.
    let pairs = unowned::claimed_pairs(reading);
    let landed = if pairs.is_empty() {
        Some(Vec::new())
    } else {
        store
            .claimed_landed(&pairs)
            .map_err(|error| eprintln!("core-lines: the claimed folders' landings: {error}"))
            .ok()
    };
    storage["unowned_scratch_bytes"] = reading.map_or(Value::Null, unowned::Reading::unowned_bytes);
    storage["leaked_test_schemas"] = reading.map_or(Value::Null, unowned::Reading::leaked);
    // The scratch landed objectives left behind (objective 232), from the
    // same reading and landings: no read of its own.
    storage["landed_scratch"] = unowned::landed_scratch(reading, landed.as_deref());
    disk::with_disk(&mut storage, samples, now_ms);
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
    // "Needs you is right" (objective 148): of the needs fired in the last
    // 7 days, the owner's judgements; a clearing is never evidence (A23).
    // A failed read is no figure, not a failed answer.
    let counts = timed(&mut parts.health, || {
        store
            .needs_judged(now_ms - observability::WEEK_MS)
            .map_err(|error| eprintln!("core-lines: the needs judged: {error}"))
            .ok()
    });
    let (needs_you_right, needs_judged) = observability::judged_share(counts.as_ref());
    // Its week of judged data starts at the first hourly sample carrying it
    // (objective 181); a failed read is read as no sample yet.
    let first = timed(&mut parts.health, || {
        store
            .first_core_lines_value(observability::NEEDS_YOU_RIGHT)
            .map_err(|error| eprintln!("core-lines: the first judged value: {error}"))
            .ok()
            .flatten()
    });
    let needs_range_note = observability::range_note(first, &needs_you_right, now_ms);
    let observed = Observed {
        cost_total,
        human_per_agent_hour: interventions["per_agent_hour"].clone(),
        needs_you_right,
        needs_judged,
        needs_range_note,
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
    let mut figures = lines(
        &storage,
        &performance,
        &observed,
        drills.north_star.value,
        guardrails,
    );
    // U1 out of the recovery table's rows (objective 181).
    for star in figures.iter_mut().map(|line| &mut line.north_star) {
        if star.key == "recoverability.restores_passed" {
            star.of = drills.north_star.of;
            star.note.clone_from(&drills.north_star.note);
        }
    }
    let statuses = kept::statuses();
    aged(&mut figures, &statuses, now_ms);
    parts.ages(&statuses, now_ms);
    Ok((
        figures,
        Extras {
            groups: statuses.iter().map(|status| status.json(now_ms)).collect(),
            drills: drills.list,
            scratch: unowned::scratch_json(&unowned.plan, reading, landed.as_deref()),
            test_schemas: unowned::test_schemas_json(reading),
            refused: unowned.refused,
        },
    ))
}

/// How far a sweep's walk of one root may go (objective 145): it reads the
/// whole of the scratch once, not every ten minutes.
const SWEEP_BOUND: scratch::Bound = scratch::Bound {
    entries: 50_000_000,
    time: Duration::from_mins(10),
};

/// What `spawn scratch sweep` decides from (objective 145): the claimed
/// folders the page's walk finds (the Core lines file's scratch roots, its
/// exclusions and refused roots), each rechecked against every name and
/// place the walk never counts, $HOME too ([`scratch::sweepable`]), and
/// which of their objectives landed, read once through the dashboard's
/// read-only login, only when a folder is claimed.
pub(crate) struct ClaimedScratch {
    pub(crate) folders: Vec<(PathBuf, crate::scratch_owner::Claim)>,
    pub(crate) landed: Vec<(i64, i64)>,
}

/// [`ClaimedScratch`] at `now_ms`; why not (never with a URL) when the file
/// names no root, a walk goes past its bound, or the store cannot be opened
/// or its landings read.
pub(crate) fn claimed_scratch(now_ms: i64) -> Result<ClaimedScratch, String> {
    let text = read_ranges(ranges_path().as_deref())?;
    let settings = unowned::settings(text.as_deref());
    if settings.roots.is_empty() {
        return Err("the Core lines file names no storage.scratch_roots".to_string());
    }
    let never = scratch::never_counted(&crate::storage::projects_path());
    let walk = scratch::walk(
        &settings.roots,
        &settings.excludes,
        &never,
        now_ms,
        &crate::integrate::still_runs,
        SWEEP_BOUND,
    )?;
    let mut places = never;
    places.extend(crate::env_value("HOME").map(PathBuf::from));
    let folders: Vec<_> = walk
        .claimed
        .into_iter()
        .filter(|claimed| {
            scratch::sweepable(&claimed.path, &settings.roots, &settings.excludes, &places)
        })
        .map(|claimed| (claimed.path, claimed.claim))
        .collect();
    let pairs: Vec<(i64, i64)> = folders
        .iter()
        .filter_map(|(_, claim)| claim.pair())
        .collect();
    if pairs.is_empty() {
        return Ok(ClaimedScratch {
            folders,
            landed: Vec::new(),
        });
    }
    let no_url = |why: String| {
        if why.contains("://") {
            "(its error names a URL, not shown)".to_string()
        } else {
            why
        }
    };
    let landed = super::open_store()
        .map_err(|why| format!("the store cannot be opened: {}", no_url(why)))?
        .claimed_landed(&pairs)
        .map_err(|error| {
            format!(
                "the store's landings cannot be read: {}",
                no_url(error.to_string())
            )
        })?;
    Ok(ClaimedScratch { folders, landed })
}

/// The figures that have a value, by key: what a `host.core_lines` sample
/// holds.
pub(super) fn values_of(set: &Set) -> Map<String, Value> {
    set.figures()
        .filter(|figure| !figure.value.is_null() && !figure.stale)
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
    let now_ms = super::now_ms();
    // The week's samples, for the disk's fall (objective 141) and whether
    // leaked test schemas has read 0 for the week (objective 182; it is
    // sampled either way): one read an hour.
    let samples = store
        .host_core_lines(samples_since(now_ms))
        .map_err(super::err)?;
    let (lines, _) = lines_at(
        &store,
        nightly_dir().as_deref(),
        text,
        &samples,
        now_ms,
        &mut Parts::default(),
    )?;
    Ok(values_of(&adopted_at(&lines, &samples, now_ms)))
}

#[cfg(test)]
mod tests {
    use super::{Figure, Observed, figure_json, lines, parse_ranges, read_ranges};
    use serde_json::{Value, json};
    use std::collections::BTreeMap;

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

    /// Objective 181: a ranges file entry's `target` is kept apart from its
    /// range and said as the figure's `target`, never judged against; an
    /// entry with a target alone gives no range and is not left out; one
    /// with neither is left out as before.
    #[test]
    fn a_target_is_said_beside_the_figure_apart_from_its_range() {
        let ranges = parse_ranges(
            Some(
                r#"{"ranges": {
                    "a.one": {"max": 0.1, "target": 0.1},
                    "a.two": {"target": 52428800},
                    "a.three": {"target": "many"}
                }}"#,
            ),
            &KNOWN,
        );
        assert_eq!(ranges.bounds.len(), 1);
        assert_eq!(
            ranges.bounds.get("a.one").copied().map(super::Range::json),
            Some(json!({"max": 0.1}))
        );
        assert_eq!(
            ranges
                .targets
                .iter()
                .map(|(k, v)| (k.as_str(), *v))
                .collect::<Vec<_>>(),
            [("a.one", 0.1), ("a.two", 52_428_800.0)]
        );
        assert_eq!(
            ranges.ignored,
            [("a.three".to_string(), "needs a number min or max")]
        );

        let star = |file: &str| {
            let observed = Observed {
                human_per_agent_hour: json!(0.5),
                ..Observed::default()
            };
            let figures = lines(&json!({}), &Value::Null, &observed, Value::Null, Vec::new());
            let view = super::answer(
                &super::adopted(&figures, false),
                &Value::Null,
                Ok(Some(file.to_string())),
                &BTreeMap::new(),
            );
            let all: Vec<Value> = view["lines"]
                .as_array()
                .expect("lines")
                .iter()
                .flat_map(|line| {
                    std::iter::once(line["north_star"].clone())
                        .chain(line["guardrails"].as_array().expect("guardrails").clone())
                })
                .collect();
            let find = |key: &str| {
                all.iter()
                    .find(|figure| figure["key"] == key)
                    .cloned()
                    .expect(key)
            };
            (
                find("observability.interventions_per_agent_hour"),
                find("storage.bytes_per_landing"),
                view["ignored_ranges"].clone(),
            )
        };
        let (sustainability, storage, ignored) = star(
            r#"{"ranges": {
                "observability.interventions_per_agent_hour": {"max": 0.1, "target": 0.1},
                "storage.bytes_per_landing": {"target": 52428800}}}"#,
        );
        assert_eq!(sustainability["target"], json!(0.1));
        assert_eq!(sustainability["range"], json!({"max": 0.1}));
        assert_eq!(sustainability["state"], "out", "from the range alone");
        assert_eq!(storage["target"], json!(52_428_800), "whole, as written");
        assert_eq!(storage["range"], Value::Null);
        assert_eq!(ignored, json!([]));
        // A target alone, the value past it: no range, never judged.
        let (sustainability, _, _) =
            star(r#"{"ranges": {"observability.interventions_per_agent_hour": {"target": 0.1}}}"#);
        assert_eq!(sustainability["target"], json!(0.1));
        assert_eq!(
            (&sustainability["range"], &sustainability["state"]),
            (&Value::Null, &json!("no_range"))
        );
        // No target: none said.
        let (sustainability, storage, _) = star(r#"{"ranges": {}}"#);
        assert!(sustainability.get("target").is_none() && storage.get("target").is_none());
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

    /// A kept group's figures say when the group was read and, past its
    /// stale line, `stale`; a stale figure is left out of the sample; the
    /// parts line says each group's age (sync/design-core-lines-kept-figures.md).
    #[test]
    fn a_kept_groups_figures_say_their_age_and_a_stale_one_is_not_sampled() {
        use super::kept::Status;
        let status = |group, as_of_ms| Status {
            group,
            as_of_ms: Some(as_of_ms),
            first_tried_ms: Some(as_of_ms),
            every_ms: 60_000,
            stale_ms: 900_000,
            last_error: None,
            refresh_ms: Some(700),
        };
        let now = 2_000_000;
        // Performance read 2 min ago (current); storage 20 min ago, past its
        // 15 here (stale).
        let statuses = [
            status("performance", now - 120_000),
            status("storage", now - 1_200_000),
        ];
        let mut figures = lines(
            &json!({"free_bytes": 3, "days_left": 9, "growth_bytes_per_day": 7}),
            &json!({"gate_time": 5}),
            &Observed::default(),
            Value::Null,
            Vec::new(),
        );
        super::aged(&mut figures, &statuses, now);
        let find = |key: &str| {
            figures
                .iter()
                .flat_map(|line| std::iter::once(&line.north_star).chain(&line.guardrails))
                .find(|figure| figure.key == key)
                .expect("the figure")
                .clone()
        };
        let gate = figure_json(&find("performance.gate_time"), None);
        assert_eq!(gate["as_of_ms"], now - 120_000);
        assert_eq!(gate.get("stale"), None, "current: no stale field");
        let days = figure_json(&find("storage.days_left"), None);
        assert_eq!(days["as_of_ms"], now - 1_200_000);
        assert_eq!(days["stale"], true);
        assert_eq!(days["value"], 9, "a stale figure keeps its value");
        // The free space is read at each reading: no group, no age.
        let free = figure_json(&find("storage.free_bytes"), None);
        assert_eq!((free.get("as_of_ms"), free.get("stale")), (None, None));
        let values = super::values_of(&super::adopted(&figures, false));
        assert_eq!(
            values.get("storage.archive_growth"),
            None,
            "stale: not sampled"
        );
        assert_eq!(values["storage.free_bytes"], 3);
        assert_eq!(values["performance.gate_time"], 5);
        let mut parts = super::Parts::default();
        parts.ages(&statuses, now);
        assert!(
            parts
                .text()
                .ends_with("performance_age_ms=120000 storage_age_ms=1200000"),
            "{}",
            parts.text()
        );
    }

    /// Inert until a group is in use (the lead: by test, not by reading):
    /// with no kept group, the figures, the answer and the parts line are
    /// exactly what they were before kept groups (the line's one change is
    /// `unowned_ms`, split out of `storage_ms` at the lead's ask).
    #[test]
    fn with_no_kept_group_the_answer_and_the_parts_line_are_as_before() {
        let before = lines(
            &json!({"free_bytes": 3, "days_left": 9}),
            &json!({"gate_time": 5}),
            &Observed::default(),
            Value::Null,
            Vec::new(),
        );
        let mut figures = before.clone();
        super::aged(&mut figures, &[], 2_000_000);
        assert_eq!(figures, before);
        let trends = std::collections::BTreeMap::new();
        let view = super::answer(
            &super::adopted(&figures, false),
            &Value::Null,
            Ok(None),
            &trends,
        );
        let mut grouped = view.clone();
        super::with_groups(&mut grouped, Vec::new());
        assert_eq!(grouped, view, "no `groups` key");
        let text = serde_json::to_string(&view).expect("text");
        assert!(
            !text.contains("as_of_ms") && !text.contains("stale") && !text.contains("groups"),
            "{text}"
        );
        let mut parts = super::Parts {
            open: Some(std::time::Duration::from_millis(20)),
            performance: Some("measured"),
            ..super::Parts::default()
        };
        parts.ages(&[], 2_000_000);
        assert_eq!(
            parts.text(),
            "open_ms=20 trends_ms=none facts_ms=none archive_ms=none cost_ms=none \
storage_ms=none unowned_ms=none health_ms=none lock_wait_ms=none performance=measured"
        );
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
        let values = super::values_of(&super::adopted(&figures, false));
        // The owner's set (objective 176): the figures dropped from it (days
        // left, the count not measured) are no longer sampled.
        assert_eq!(Value::Object(values), json!({"storage.free_bytes": 3}));
    }

    /// The answer of the owner's set (objective 176) over readings with
    /// every storage figure, the performance figures, the interventions and
    /// the recoverability figures (the drills' ages when `drills`), and the
    /// ranges file `ranges`.
    fn adopted_view(drills: bool, ranges: &str) -> Value {
        super::answer(
            &super::adopted(&adopted_readings(drills, 1), false),
            &Value::Null,
            Ok(Some(ranges.to_string())),
            &std::collections::BTreeMap::new(),
        )
    }

    /// [`adopted_view`]'s readings, leaked test schemas reading `leaked`.
    fn adopted_readings(drills: bool, leaked: u64) -> Vec<super::Line> {
        let storage = json!({
            "days_left": 1234, "free_bytes": 104, "growth_bytes_per_day": 10,
            "database": {"bytes": 300, "log_bytes": 100},
            "test_databases": [{"bytes": 7, "schemas": 3}],
            "unowned_scratch_bytes": 400_000, "leaked_test_schemas": leaked,
            "disk_fall_bytes_per_day": 5, "grew_most": {"store": "archive", "bytes_per_day": 6},
        });
        let performance = json!({"report_to_live": 60_000, "gate_time": 5, "decided_nothing": 0.1,
                                 "first_pass": 0.9, "usd_per_landing": 2.5});
        let observed = Observed {
            human_per_agent_hour: json!(0.125),
            ..Observed::default()
        };
        let mut guardrails = vec![
            Figure::new(
                "recoverability.cloud_dump_age",
                "Age of the newest database copy",
                "ms",
                json!(1),
            ),
            Figure::new("recoverability.cloud_bundle_age", "Bundle", "ms", json!(2)),
            Figure::new("recoverability.nightly_age", "Nightly", "ms", json!(3)),
            Figure::new("recoverability.nightly_red", "Reds", "count", json!(1)),
        ];
        if drills {
            guardrails.push(Figure::new(
                "recoverability.session_backup_age",
                "Sessions",
                "ms",
                json!(4),
            ));
            guardrails.push(Figure::new(
                "recoverability.oldest_drill_age",
                "Drill",
                "ms",
                json!(5),
            ));
        }
        lines(&storage, &performance, &observed, json!(2), guardrails)
    }

    fn keys(figures: &Value) -> Vec<String> {
        figures
            .as_array()
            .expect("figures")
            .iter()
            .map(|figure| figure["key"].as_str().expect("a key").to_string())
            .collect()
    }

    /// Objective 176: the three lines in the owner's order, each figure by
    /// key; the storage figures no code computes yet are not measured yet
    /// (with their range), and a figure with a value is never that.
    #[test]
    fn the_answer_is_the_adopted_core_metric_set() {
        let view = adopted_view(
            false,
            r#"{"ranges": {"storage.bytes_per_landing": {"max": 52428800},
                "observability.interventions_per_agent_hour": {"max": 0.1}}}"#,
        );
        let shape: Vec<(String, String, Vec<String>)> = view["lines"]
            .as_array()
            .expect("lines")
            .iter()
            .map(|line| {
                (
                    line["line"].as_str().expect("a name").to_string(),
                    line["north_star"]["key"]
                        .as_str()
                        .expect("a key")
                        .to_string(),
                    keys(&line["guardrails"]),
                )
            })
            .collect();
        let owned = |list: &[&str]| list.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(
            shape,
            [
                (
                    "storage".to_string(),
                    "storage.bytes_per_landing".to_string(),
                    owned(&["storage.rebuildable_bytes", "storage.kept_share"])
                ),
                (
                    "performance".to_string(),
                    "performance.report_to_live".to_string(),
                    owned(&[
                        "performance.gate_time",
                        "performance.decided_nothing",
                        "performance.first_pass"
                    ])
                ),
                (
                    "sustainability".to_string(),
                    "observability.interventions_per_agent_hour".to_string(),
                    owned(&[
                        "recoverability.restores_passed",
                        "observability.needs_you_right",
                        "observability.unpriced_share",
                        "recoverability.cloud_dump_age"
                    ])
                ),
            ]
        );
        let storage = &view["lines"][0];
        assert_eq!(
            [
                &storage["north_star"]["value"],
                &storage["north_star"]["state"],
                &storage["north_star"]["range"]
            ],
            [
                &Value::Null,
                &json!("not_measured_yet"),
                &json!({"max": 52_428_800})
            ]
        );
        for guardrail in storage["guardrails"].as_array().expect("guardrails") {
            assert_eq!(guardrail["state"], "not_measured_yet");
            assert_eq!(guardrail["value"], Value::Null);
        }
        let star = &view["lines"][2]["north_star"];
        assert_eq!(
            [&star["value"], &star["window"], &star["state"]],
            [&json!(0.125), &json!("24h"), &json!("out")]
        );
        assert_eq!(
            view["lines"][2]["guardrails"][0]["value"], 2,
            "restores passed"
        );
        for line in view["lines"].as_array().expect("lines").iter().skip(1) {
            for figure in std::iter::once(&line["north_star"])
                .chain(line["guardrails"].as_array().expect("guardrails"))
            {
                assert_ne!(figure["state"], "not_measured_yet", "{figure}");
            }
        }
    }

    /// Objective 176: the demoted figures are the alert figures, each with
    /// its line (the drills' ages only with a recovery table), or the
    /// diagnostics, in order; leaked test schemas alone carries its line.
    #[test]
    fn the_demoted_figures_are_alert_figures_with_their_lines_or_diagnostics() {
        let view = adopted_view(
            false,
            r#"{"ranges": {"recoverability.nightly_red": {"max": 0}}}"#,
        );
        let lined = |figures: &Value| -> Vec<(String, String)> {
            figures
                .as_array()
                .expect("figures")
                .iter()
                .filter(|figure| figure["line"].is_string())
                .map(|figure| {
                    (
                        figure["line"].as_str().unwrap_or_default().to_string(),
                        figure["key"].as_str().unwrap_or_default().to_string(),
                    )
                })
                .collect()
        };
        let pair = |line: &str, key: &str| (line.to_string(), key.to_string());
        // Objective 182: free disk is no alert figure, but the first
        // diagnostic; dollars per landing is no diagnostic, but a reading.
        assert_eq!(
            lined(&view["alert_figures"]),
            [
                pair("sustainability", "recoverability.cloud_bundle_age"),
                pair("sustainability", "recoverability.nightly_age"),
                pair("sustainability", "recoverability.nightly_red"),
            ]
        );
        assert_eq!(view["alert_figures"][2]["state"], "out");
        assert_eq!(
            keys(&view["diagnostics"]),
            [
                "storage.free_bytes",
                "storage.unowned_scratch_bytes",
                "storage.disk_fall",
                "storage.grew_most",
                "storage.archive_growth",
                "storage.leaked_test_schemas",
                "storage.landed_scratch_bytes",
            ]
        );
        assert_eq!(
            lined(&view["diagnostics"]),
            [pair("storage", "storage.leaked_test_schemas")]
        );
        assert_eq!(view["diagnostics"][3]["store"], "archive");
        let with_drills = adopted_view(true, r#"{"ranges": {}}"#);
        assert_eq!(
            keys(&with_drills["alert_figures"])[3..],
            [
                "recoverability.session_backup_age",
                "recoverability.oldest_drill_age"
            ]
        );
        assert_eq!(with_drills["alert_figures"][4]["line"], "sustainability");
    }

    /// Objective 182: free disk is a diagnostic and dollars per landing the
    /// sustainability line's plain reading, neither with a range nor a
    /// target, each file entry ignored with its reason; another diagnostic
    /// keeps its range; the reading says its note where the range would be.
    #[test]
    fn free_disk_is_a_diagnostic_and_dollars_per_landing_a_reading_with_no_range() {
        let view = adopted_view(
            false,
            r#"{"ranges": {"storage.free_bytes": {"min": 1e18, "target": 5},
                "performance.usd_per_landing": {"max": 1, "target": 0.5},
                "storage.disk_fall": {"max": 1}}}"#,
        );
        let free = &view["diagnostics"][0];
        assert_eq!(free["key"], "storage.free_bytes");
        assert_eq!(
            (&free["value"], &free["range"], &free["state"]),
            (&json!(104), &Value::Null, &json!("no_range"))
        );
        assert_eq!(free.get("target"), None);
        assert_eq!(free.get("line"), None, "free disk raises nothing");
        // Another diagnostic keeps its range.
        let fall = &view["diagnostics"][2];
        assert_eq!(fall["key"], "storage.disk_fall");
        assert_eq!(
            (&fall["range"], &fall["state"]),
            (&json!({"max": 1}), &json!("out"))
        );
        // The readings of each line.
        let readings: Vec<(String, Vec<String>)> = view["lines"]
            .as_array()
            .expect("lines")
            .iter()
            .map(|line| {
                (
                    line["line"].as_str().expect("a name").to_string(),
                    keys(&line["readings"]),
                )
            })
            .collect();
        assert_eq!(
            readings,
            [
                ("storage".to_string(), vec![]),
                ("performance".to_string(), vec![]),
                (
                    "sustainability".to_string(),
                    vec!["performance.usd_per_landing".to_string()]
                ),
            ]
        );
        let reading = &view["lines"][2]["readings"][0];
        assert_eq!(
            (&reading["value"], &reading["range"], &reading["state"]),
            (&json!(2.5), &Value::Null, &json!("no_range"))
        );
        assert_eq!(reading.get("target"), None);
        assert_eq!(
            reading["range_note"],
            "no range until use with no price is under 10%"
        );
        for list in ["alert_figures", "diagnostics"] {
            assert!(
                !keys(&view[list]).contains(&"performance.usd_per_landing".to_string()),
                "{list}"
            );
        }
        assert_eq!(
            view["ignored_ranges"],
            json!([
                {"key": "performance.usd_per_landing", "reason":
                 "shown with no range until use with no price is under 10% (the owner's core metric set)"},
                {"key": "storage.free_bytes",
                 "reason": "an alert of its own (disk_low, objective 204)"},
            ])
        );
        // Both are still sampled.
        let values = super::values_of(&super::adopted(&adopted_readings(false, 1), false));
        assert_eq!(values["storage.free_bytes"], 104);
        assert_eq!(values["performance.usd_per_landing"], 2.5);
    }

    /// Objective 207: dollars per landing takes its configured range and
    /// target while use with no price is a fresh number under 10%, and is a
    /// reading with no range, its note and its entry ignored, when the share
    /// is 10% or more, missing, not a number or stale; a reading either way,
    /// in no other list.
    #[test]
    fn dollars_per_landing_takes_its_range_while_use_with_no_price_is_under_ten_percent() {
        let ranges = r#"{"ranges": {"performance.usd_per_landing": {"max": 10, "target": 5}}}"#;
        let view_with = |share: Value, stale: bool| {
            let mut readings = adopted_readings(false, 1);
            for line in &mut readings {
                for figure in std::iter::once(&mut line.north_star).chain(&mut line.guardrails) {
                    if figure.key == "observability.unpriced_share" {
                        figure.value = share.clone();
                        figure.stale = stale;
                    }
                }
            }
            super::answer(
                &super::adopted(&readings, false),
                &Value::Null,
                Ok(Some(ranges.to_string())),
                &std::collections::BTreeMap::new(),
            )
        };
        let reason = "shown with no range until use with no price is under 10% (the owner's core metric set)";
        for (share, stale, priced) in [
            (Value::Null, false, false),
            (json!(0.10), false, false),
            (json!(0.048), false, true),
            (json!(0.2), false, false),
            (json!(0.048), true, false),
            (json!("0.01"), false, false),
        ] {
            let view = view_with(share.clone(), stale);
            let what = format!("share {share} stale {stale}");
            let reading = &view["lines"][2]["readings"][0];
            assert_eq!(reading["key"], "performance.usd_per_landing", "{what}");
            assert_eq!(reading["value"], 2.5, "{what}");
            let ignored: Vec<&Value> = view["ignored_ranges"]
                .as_array()
                .expect("ignored")
                .iter()
                .filter(|entry| entry["key"] == "performance.usd_per_landing")
                .collect();
            if priced {
                assert_eq!(reading["range"], json!({"max": 10}), "{what}");
                assert_eq!(reading["target"], 5, "{what}");
                assert_eq!(reading["state"], "in", "{what}");
                assert_eq!(reading.get("range_note"), None, "{what}");
                assert!(ignored.is_empty(), "{what}");
            } else {
                assert_eq!(reading["range"], Value::Null, "{what}");
                assert_eq!(reading.get("target"), None, "{what}");
                assert_eq!(reading["state"], "no_range", "{what}");
                assert_eq!(
                    reading["range_note"], "no range until use with no price is under 10%",
                    "{what}"
                );
                assert_eq!(
                    ignored,
                    [&json!({"key": "performance.usd_per_landing", "reason": reason})],
                    "{what}"
                );
            }
            // A reading either way: in no other list.
            for line in view["lines"].as_array().expect("lines") {
                assert_ne!(line["north_star"]["key"], "performance.usd_per_landing");
                assert!(
                    !keys(&line["guardrails"]).contains(&"performance.usd_per_landing".to_string())
                );
            }
            for list in ["alert_figures", "diagnostics"] {
                assert!(
                    !keys(&view[list]).contains(&"performance.usd_per_landing".to_string()),
                    "{what}: {list}"
                );
            }
        }
        // Out of its range while priced: out on the card, still a reading.
        let tight = r#"{"ranges": {"performance.usd_per_landing": {"max": 1}}}"#;
        let mut readings = adopted_readings(false, 1);
        for line in &mut readings {
            for figure in std::iter::once(&mut line.north_star).chain(&mut line.guardrails) {
                if figure.key == "observability.unpriced_share" {
                    figure.value = json!(0.01);
                }
            }
        }
        let view = super::answer(
            &super::adopted(&readings, false),
            &Value::Null,
            Ok(Some(tight.to_string())),
            &std::collections::BTreeMap::new(),
        );
        assert_eq!(view["lines"][2]["readings"][0]["state"], "out");
    }

    /// Objective 182: leaked test schemas read 0 for a week (hourly samples
    /// across it) is the last alert figure, of line storage, and no
    /// diagnostic, its range applied and still sampled; one sample in the
    /// week at 2, or a value now of 1, makes it a diagnostic again.
    #[test]
    fn leaked_schemas_quiet_for_a_week_is_an_alert_figure_only() {
        const H: i64 = 3_600_000;
        let now = 1_800_000_000_000_i64;
        let week = |odd: Option<i64>| {
            let samples: Vec<Value> = (0..=(7 * 24 + 2))
                .map(|hours| {
                    let leaked = if odd == Some(hours) { 2 } else { 0 };
                    json!({"at_ms": now - hours * H,
                           "values": {"storage.leaked_test_schemas": leaked}})
                })
                .rev()
                .collect();
            Value::Array(samples)
        };
        let ranges = r#"{"ranges": {"storage.leaked_test_schemas": {"max": 0}}}"#;
        let view_of = |set: &super::Set| {
            super::answer(
                set,
                &Value::Null,
                Ok(Some(ranges.to_string())),
                &std::collections::BTreeMap::new(),
            )
        };
        let quiet = super::adopted_at(&adopted_readings(true, 0), &week(None), now);
        let view = view_of(&quiet);
        let last = view["alert_figures"]
            .as_array()
            .and_then(|figures| figures.last())
            .expect("an alert figure");
        assert_eq!(last["key"], "storage.leaked_test_schemas");
        assert_eq!(last["line"], "storage");
        assert_eq!(
            (&last["range"], &last["state"]),
            (&json!({"max": 0}), &json!("in"))
        );
        assert!(!keys(&view["diagnostics"]).contains(&"storage.leaked_test_schemas".to_string()));
        assert_eq!(
            super::values_of(&quiet)["storage.leaked_test_schemas"],
            0,
            "an alert figure stays sampled"
        );

        // A sample in the week at 2, or 1 now: a diagnostic again.
        for set in [
            super::adopted_at(&adopted_readings(true, 0), &week(Some(3 * 24)), now),
            super::adopted_at(&adopted_readings(true, 1), &week(None), now),
        ] {
            let view = view_of(&set);
            assert!(
                !keys(&view["alert_figures"]).contains(&"storage.leaked_test_schemas".to_string())
            );
            let diagnostic = view["diagnostics"]
                .as_array()
                .expect("diagnostics")
                .iter()
                .find(|figure| figure["key"] == "storage.leaked_test_schemas")
                .expect("a diagnostic");
            assert_eq!(diagnostic["line"], "storage");
        }
    }

    /// Objective 176: a dropped figure is in none of the lists, and a range
    /// the file still gives one is retired by name; an unknown key stays
    /// unknown.
    #[test]
    fn a_dropped_figure_is_nowhere_and_its_range_is_retired() {
        let view = adopted_view(
            true,
            r#"{"ranges": {"storage.days_left": {"min": 180}, "storage.database_ratio": {"max": 10},
                "observability.not_measured": {"max": 0}, "storage.free_bites": {"min": 1}}}"#,
        );
        let mut every: Vec<String> = Vec::new();
        for line in view["lines"].as_array().expect("lines") {
            every.push(
                line["north_star"]["key"]
                    .as_str()
                    .expect("a key")
                    .to_string(),
            );
            every.extend(keys(&line["guardrails"]));
        }
        every.extend(keys(&view["alert_figures"]));
        every.extend(keys(&view["diagnostics"]));
        for dropped in super::RETIRED {
            assert!(!every.iter().any(|key| key == dropped), "{dropped}");
        }
        assert_eq!(
            view["ignored_ranges"],
            json!([
                {"key": "observability.not_measured", "reason": super::RETIRED_REASON},
                {"key": "storage.database_ratio", "reason": super::RETIRED_REASON},
                {"key": "storage.days_left", "reason": super::RETIRED_REASON},
                {"key": "storage.free_bites", "reason": "unknown figure"},
            ])
        );
    }
}
