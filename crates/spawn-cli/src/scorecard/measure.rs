//! The eight dimensions of one scope over one window, from a [`History`]
//! (docs/design.md §15).

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use super::history::{History, Kind};
use super::time::HOUR_MS;

/// Below this many landings (gate runs, for gate time) a dimension has no
/// value: `not enough data`.
pub(crate) const MIN_N: usize = 3;

/// Whom a measure covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    Fleet,
    Mission(i64),
}

impl Scope {
    fn holds(self, mission: Option<i64>) -> bool {
        match self {
            Self::Fleet => true,
            Self::Mission(id) => mission == Some(id),
        }
    }

    fn holds_any(self, missions: &[i64]) -> bool {
        match self {
            Self::Fleet => true,
            Self::Mission(id) => missions.contains(&id),
        }
    }

    fn mission(self) -> Option<i64> {
        match self {
            Self::Fleet => None,
            Self::Mission(id) => Some(id),
        }
    }
}

/// `[since, until)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Window {
    pub(crate) since: i64,
    pub(crate) until: i64,
}

impl Window {
    fn holds(self, at: i64) -> bool {
        self.since <= at && at < self.until
    }
}

/// The raw numbers of one scope over one window.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    pub(crate) landed: usize,
    pub(crate) open_ms: i64,
    /// Lead times of the landings, sorted.
    pub(crate) lead_ms: Vec<i64>,
    pub(crate) first_pass: usize,
    pub(crate) marks: BTreeMap<Kind, usize>,
    pub(crate) tokens: i64,
    pub(crate) by_model: BTreeMap<String, i64>,
    /// Wall clocks of the gate runs, sorted.
    pub(crate) gate_runs: Vec<i64>,
}

impl Counts {
    fn count(&self, kind: Kind) -> usize {
        self.marks.get(&kind).copied().unwrap_or(0)
    }

    fn sum(&self, kinds: &[Kind]) -> usize {
        kinds.iter().map(|kind| self.count(*kind)).sum()
    }

    fn gate_ms(&self) -> i64 {
        self.gate_runs.iter().sum()
    }
}

/// The length of the union of `intervals` (an open end runs on), clipped to
/// `window`.
pub(crate) fn union_ms(intervals: &[(i64, Option<i64>)], window: Window) -> i64 {
    let mut clipped: Vec<(i64, i64)> = intervals
        .iter()
        .map(|(start, end)| {
            (
                (*start).max(window.since),
                end.unwrap_or(i64::MAX).min(window.until),
            )
        })
        .filter(|(start, end)| start < end)
        .collect();
    clipped.sort_unstable();
    let mut total = 0;
    let mut current: Option<(i64, i64)> = None;
    for (start, end) in clipped {
        current = match current {
            Some((open, close)) if start <= close => Some((open, close.max(end))),
            Some((open, close)) => {
                total += close - open;
                Some((start, end))
            }
            None => Some((start, end)),
        };
    }
    if let Some((open, close)) = current {
        total += close - open;
    }
    total
}

/// The counts of `scope` over `window`.
pub(crate) fn counts(history: &History, scope: Scope, window: Window) -> Counts {
    let mut counts = Counts {
        open_ms: union_ms(&history.open_intervals(scope.mission()), window),
        ..Counts::default()
    };
    for landing in &history.landings {
        if scope.holds(Some(landing.mission)) && window.holds(landing.at) {
            counts.landed += 1;
            counts.lead_ms.push(landing.lead_ms);
            counts.first_pass += usize::from(landing.first_pass);
        }
    }
    counts.lead_ms.sort_unstable();
    for mark in &history.marks {
        if scope.holds(mark.mission) && window.holds(mark.at) {
            *counts.marks.entry(mark.kind).or_insert(0) += 1;
        }
    }
    for tokens in &history.tokens {
        if scope.holds_any(&tokens.missions) && window.holds(tokens.at) {
            counts.tokens = counts.tokens.saturating_add(tokens.tokens);
            let model = counts.by_model.entry(tokens.model.clone()).or_insert(0);
            *model = model.saturating_add(tokens.tokens);
        }
    }
    for run in &history.gate_runs {
        if scope.holds(run.mission) && window.holds(run.at) {
            counts.gate_runs.push(run.ms);
        }
    }
    counts.gate_runs.sort_unstable();
    counts
}

/// Whether `scope` has anything counted over `window`: a task open in it,
/// a landing, a mark, tokens or a gate run.
pub(crate) fn active(history: &History, scope: Scope, window: Window) -> bool {
    let counts = counts(history, scope, window);
    counts.open_ms > 0
        || counts.landed > 0
        || !counts.marks.is_empty()
        || counts.tokens > 0
        || !counts.gate_runs.is_empty()
        || history.tasks.values().any(|task| {
            scope.holds(Some(task.mission))
                && task.assigned_at < window.until
                && task.closed_at.is_none_or(|closed| closed > window.since)
        })
}

/// `numerator / denominator` in thousandths, rounded half away from zero;
/// `None` for a zero denominator. Exact: no float is rounded.
pub(crate) fn ratio_milli(numerator: i128, denominator: i128) -> Option<i64> {
    if denominator == 0 {
        return None;
    }
    let negative = (numerator < 0) != (denominator < 0);
    let (numerator, denominator) = (numerator.abs(), denominator.abs());
    let rounded = (numerator * 2000 + denominator) / (2 * denominator);
    let rounded = i64::try_from(rounded).ok()?;
    Some(if negative { -rounded } else { rounded })
}

/// The value at rank ceil(p/100 × n) of `sorted` (nearest rank).
pub(crate) fn nearest_rank(sorted: &[i64], percent: usize) -> Option<i64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (percent * sorted.len()).div_ceil(100).max(1);
    sorted.get(rank - 1).copied()
}

/// A value as the JSON says it: thousandths as a ratio with 3 decimals, or
/// a whole number (ms, tokens).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Number {
    Milli(i64),
    Whole(i64),
}

impl Number {
    pub(crate) fn json(self) -> Value {
        match self {
            #[allow(clippy::cast_precision_loss)]
            Self::Milli(milli) => json!(milli as f64 / 1000.0),
            Self::Whole(whole) => json!(whole),
        }
    }

    fn key(self) -> i64 {
        match self {
            Self::Milli(value) | Self::Whole(value) => value,
        }
    }
}

fn number(value: Option<Number>) -> Value {
    value.map_or(Value::Null, Number::json)
}

fn milli(numerator: impl Into<i128>, denominator: impl Into<i128>) -> Option<Number> {
    ratio_milli(numerator.into(), denominator.into()).map(Number::Milli)
}

fn whole(value: Option<i64>) -> Option<Number> {
    value.map(Number::Whole)
}

/// Which way a dimension is better.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Better {
    Higher,
    Lower,
}

impl Better {
    fn word(self) -> &'static str {
        match self {
            Self::Higher => "higher",
            Self::Lower => "lower",
        }
    }
}

/// `better`, `worse` or `same`: the rounded values compared by the
/// direction; `None` when either is missing.
pub(crate) fn trend(value: Option<Number>, previous: Option<Number>, better: Better) -> Option<&'static str> {
    let (value, previous) = (value?.key(), previous?.key());
    Some(match (value.cmp(&previous), better) {
        (std::cmp::Ordering::Equal, _) => "same",
        (std::cmp::Ordering::Greater, Better::Higher)
        | (std::cmp::Ordering::Less, Better::Lower) => "better",
        _ => "worse",
    })
}

/// One dimension before it is written: its value (none below the minimum
/// n), its n, its direction and its parts.
struct Dimension {
    value: Option<Number>,
    n: usize,
    better: Better,
    parts: Map<String, Value>,
}

fn usize_json(value: usize) -> Value {
    json!(value)
}

fn per_landed(counts: &Counts, enough: bool, numerator: impl Into<i128>) -> Option<Number> {
    if enough {
        milli(numerator, i128::try_from(counts.landed).unwrap_or(i128::MAX))
    } else {
        None
    }
}

const NAMES: [&str; 8] = [
    "throughput",
    "lead_time",
    "first_pass",
    "rework",
    "autonomy",
    "reliability",
    "cost",
    "gate_time",
];

const SEND_BACKS: [(Kind, &str); 4] = [
    (Kind::Rejected, "rejected"),
    (Kind::Conflict, "conflict"),
    (Kind::Incompatible, "incompatible"),
    (Kind::Reviewed, "reviewed"),
];

const INTERVENTIONS: [(Kind, &str); 4] = [
    (Kind::Command, "command"),
    (Kind::Resume, "resume"),
    (Kind::OwnerTask, "owner_task"),
    (Kind::CancelReassign, "cancel_reassign"),
];

const FAILURES: [(Kind, &str); 4] = [
    (Kind::FailedCommand, "failed_command"),
    (Kind::SpentDelivery, "spent_delivery"),
    (Kind::Stall, "stall"),
    (Kind::GateInfrastructure, "gate_infrastructure"),
];

fn by_kind(counts: &Counts, kinds: &[(Kind, &str); 4]) -> Value {
    Value::Object(
        kinds
            .iter()
            .map(|(kind, name)| ((*name).to_string(), usize_json(counts.count(*kind))))
            .collect(),
    )
}

fn kinds(table: &[(Kind, &str); 4]) -> Vec<Kind> {
    table.iter().map(|(kind, _)| *kind).collect()
}

/// The eight dimensions of `counts`, by name, in [`NAMES`] order.
fn dimensions_of(counts: &Counts) -> Vec<Dimension> {
    let landed = counts.landed;
    let enough = landed >= MIN_N;
    let landed_i = i128::try_from(landed).unwrap_or(i128::MAX);
    let mut out = Vec::with_capacity(NAMES.len());

    let mut parts = Map::new();
    parts.insert("landed".into(), usize_json(landed));
    parts.insert(
        "open_hours".into(),
        number(milli(counts.open_ms, HOUR_MS)),
    );
    out.push(Dimension {
        value: if enough {
            milli(landed_i * i128::from(HOUR_MS), counts.open_ms)
        } else {
            None
        },
        n: landed,
        better: Better::Higher,
        parts,
    });

    let mut parts = Map::new();
    let p90 = if enough {
        whole(nearest_rank(&counts.lead_ms, 90))
    } else {
        None
    };
    parts.insert("p90".into(), number(p90));
    out.push(Dimension {
        value: if enough {
            whole(nearest_rank(&counts.lead_ms, 50))
        } else {
            None
        },
        n: landed,
        better: Better::Lower,
        parts,
    });

    let mut parts = Map::new();
    parts.insert("first_pass".into(), usize_json(counts.first_pass));
    out.push(Dimension {
        value: per_landed(counts, enough, i128::try_from(counts.first_pass).unwrap_or(0)),
        n: landed,
        better: Better::Higher,
        parts,
    });

    for (table, total, breakdown) in [
        (&SEND_BACKS, "send_backs", "by_verdict"),
        (&INTERVENTIONS, "interventions", "by_kind"),
        (&FAILURES, "events", "by_kind"),
    ] {
        let sum = counts.sum(&kinds(table));
        let mut parts = Map::new();
        parts.insert(total.into(), usize_json(sum));
        parts.insert(breakdown.into(), by_kind(counts, table));
        out.push(Dimension {
            value: per_landed(counts, enough, i128::try_from(sum).unwrap_or(0)),
            n: landed,
            better: Better::Lower,
            parts,
        });
    }

    let mut parts = Map::new();
    parts.insert("tokens".into(), json!(counts.tokens));
    parts.insert(
        "by_model".into(),
        Value::Object(
            counts
                .by_model
                .iter()
                .filter(|(_, tokens)| **tokens > 0)
                .map(|(model, tokens)| {
                    (
                        model.clone(),
                        json!({
                            "tokens": tokens,
                            "per_landed": number(per_landed(counts, enough, *tokens)),
                        }),
                    )
                })
                .collect(),
        ),
    );
    parts.insert("gate_ms".into(), json!(counts.gate_ms()));
    parts.insert(
        "gate_ms_per_landed".into(),
        number(per_landed(counts, enough, counts.gate_ms())),
    );
    out.push(Dimension {
        value: per_landed(counts, enough, counts.tokens),
        n: landed,
        better: Better::Lower,
        parts,
    });

    let runs = counts.gate_runs.len();
    let mut parts = Map::new();
    parts.insert("runs".into(), usize_json(runs));
    out.push(Dimension {
        value: if runs >= MIN_N {
            whole(nearest_rank(&counts.gate_runs, 50))
        } else {
            None
        },
        n: runs,
        better: Better::Lower,
        parts,
    });
    out
}

/// The eight dimensions of `counts` as JSON, each with `value`, `n`,
/// `enough`, `better`, `previous`, `trend` and its parts; `previous` and
/// `trend` from `previous` (the previous window's counts), null without it.
pub(crate) fn dimensions(counts: &Counts, previous: Option<&Counts>) -> Value {
    let now = dimensions_of(counts);
    let before = previous.map(dimensions_of);
    let mut out = Map::new();
    for (index, (name, dimension)) in NAMES.iter().zip(now).enumerate() {
        let previous = before
            .as_ref()
            .and_then(|before| before.get(index))
            .and_then(|before| before.value);
        let mut object = Map::new();
        object.insert("value".into(), number(dimension.value));
        object.insert("n".into(), usize_json(dimension.n));
        object.insert("enough".into(), json!(dimension.n >= MIN_N));
        object.insert("better".into(), json!(dimension.better.word()));
        object.insert("previous".into(), number(previous));
        object.insert(
            "trend".into(),
            json!(trend(dimension.value, previous, dimension.better)),
        );
        object.extend(dimension.parts);
        out.insert((*name).to_string(), Value::Object(object));
    }
    Value::Object(out)
}
