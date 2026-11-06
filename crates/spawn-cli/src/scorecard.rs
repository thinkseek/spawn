//! `spawn scorecard`: every dimension of the fleet at once, from the record
//! alone (docs/design.md §15): throughput, lead time, first-pass quality,
//! rework, autonomy, reliability, cost and gate time, each with its n, its
//! direction, the previous window's value and the trend, for the fleet, each
//! mission, and each UTC day of the window. `/api/scorecard` answers the same
//! JSON (crate::serve).
//!
//! Causality (§14): every number is read from a record that names what it
//! covers: a control record's own `recorded_at_ms` and the task or mission it
//! names, a command's key, session and times, a transcript line's message id
//! and timestamp. Nothing is inferred from absence (a task is open until a
//! record closes it; a delivery is spent only when its attempts say so, as
//! `deliver::attempts` judges them) or from the order loops ran in, and no
//! counter is kept anywhere: a reader of the same record gets the same
//! numbers, and once `--until` is given the same flags give the same bytes.
//! The facts are read in one snapshot (spawn-postgres, `scorecard`), every
//! record before the window's end and none after it.
//!
//! [`history`] folds the record into marks, [`measure`] counts a scope over a
//! window, [`transcripts`] reads the archive's tokens and [`time`] the times.

mod history;
mod measure;
mod time;
mod transcripts;

#[cfg(test)]
mod tests;

use std::fmt::Write as _;
use std::path::Path;

use serde_json::{Value, json};
use spawn_postgres::PostgresStore;

use history::{Facts, History};
use measure::{MIN_N, Scope, Window, active, counts, dimensions};
use time::{DAY_MS, format_rfc3339, parse_rfc3339, parse_span};

/// The window when `--since` is not given: the last seven days.
const DEFAULT_SPAN_MS: i64 = 7 * DAY_MS;

/// The longest window a scorecard measures, so one request stays bounded.
const MAX_WINDOW_DAYS: i64 = 400;

const USAGE: &str =
    "usage: spawn scorecard [--mission N] [--since <time>] [--until <time>] [--json] [--root <dir>]";

/// What a scorecard is asked: the flags of `spawn scorecard`, the
/// parameters of `/api/scorecard`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Query {
    pub(crate) mission: Option<String>,
    pub(crate) since: Option<String>,
    pub(crate) until: Option<String>,
}

/// Why no scorecard: the question was wrong (`Request`), or the record could
/// not be read (`Record`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Failure {
    Request(String),
    Record(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request(error) | Self::Record(error) => formatter.write_str(error),
        }
    }
}

/// A time as given: an instant (RFC 3339) or a span back (`36h`, `7d`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum When {
    At(i64),
    Ago(i64),
}

fn when(flag: &str, text: &str) -> Result<When, Failure> {
    if let Some(at) = parse_rfc3339(text) {
        return Ok(When::At(at));
    }
    parse_span(text).map(When::Ago).ok_or_else(|| {
        Failure::Request(format!(
            "--{flag} {text}: not an RFC 3339 time (2026-10-02T00:00:00Z) or a span back \
             (36h, 7d)"
        ))
    })
}

/// The window a query names, `now` being the database's clock: `--until`
/// (default now; a span counts back from now), `--since` (default 7d; a span
/// counts back from the window's end, so a given `--until` fixes the window).
fn window(since: Option<When>, until: Option<When>, now: i64) -> Result<Window, Failure> {
    let until = match until {
        None => now,
        Some(When::At(at)) => at,
        Some(When::Ago(span)) => now.saturating_sub(span),
    };
    let since = match since {
        None => until.saturating_sub(DEFAULT_SPAN_MS),
        Some(When::At(at)) => at,
        Some(When::Ago(span)) => until.saturating_sub(span),
    };
    if since >= until {
        return Err(Failure::Request(format!(
            "the window [{}, {}) is empty: --since must be before --until",
            format_rfc3339(since),
            format_rfc3339(until)
        )));
    }
    if until - since > MAX_WINDOW_DAYS * DAY_MS {
        return Err(Failure::Request(format!(
            "the window is longer than {MAX_WINDOW_DAYS} days"
        )));
    }
    Ok(Window { since, until })
}

fn mission_of(text: &str) -> Result<i64, Failure> {
    text.parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| Failure::Request(format!("--mission {text}: not a mission id")))
}

/// The scorecard a query asks for, read from `store` and the transcript
/// archive of the workspace root `root` (`<root>/.transcripts`).
pub(crate) fn measure(
    store: &PostgresStore,
    root: Option<&Path>,
    query: &Query,
) -> Result<Value, Failure> {
    let mission = query.mission.as_deref().map(mission_of).transpose()?;
    let since = query.since.as_deref().map(|text| when("since", text)).transpose()?;
    let until = query.until.as_deref().map(|text| when("until", text)).transpose()?;
    let root = root.ok_or_else(|| {
        Failure::Record(
            "no transcript archive to read the cost from: SPAWN_WORKSPACE_ROOT is not set \
             (or pass --root)"
                .to_string(),
        )
    })?;
    let now = match (since, until) {
        (Some(When::At(_)), Some(When::At(_))) => 0,
        _ => store
            .database_now_ms()
            .map_err(|error| Failure::Record(error.to_string()))?,
    };
    let window = window(since, until, now)?;
    let facts: Facts = serde_json::from_value(
        store
            .scorecard_facts(window.until)
            .map_err(|error| Failure::Record(error.to_string()))?,
    )
    .map_err(|error| Failure::Record(format!("the record's facts: {error}")))?;
    let previous_since = window.since - (window.until - window.since);
    let messages = transcripts::read_archive(
        &root.join(spawn_host::TRANSCRIPT_DIRECTORY),
        previous_since,
    )
    .map_err(Failure::Record)?;
    let mut history = History::from_record(&facts, &messages);
    history.clip(previous_since, window.until);
    card(&history, window, mission)
}

/// The scorecard of `history` over `window`: the fleet and every mission
/// active in this window or the previous one, or `mission` alone.
fn card(history: &History, window: Window, mission: Option<i64>) -> Result<Value, Failure> {
    let length = window.until - window.since;
    let previous = Window {
        since: window.since - length,
        until: window.since,
    };
    let both = Window {
        since: previous.since,
        until: window.until,
    };
    let scopes: Vec<Scope> = match mission {
        Some(id) if history.missions.contains(&id) => vec![Scope::Mission(id)],
        Some(id) => {
            return Err(Failure::Request(format!(
                "no record names mission {id} before {}",
                format_rfc3339(window.until)
            )));
        }
        None => std::iter::once(Scope::Fleet)
            .chain(
                history
                    .missions
                    .iter()
                    .map(|id| Scope::Mission(*id))
                    .filter(|scope| active(history, *scope, both)),
            )
            .collect(),
    };
    let scopes: Vec<Value> = scopes
        .into_iter()
        .map(|scope| {
            let days: Vec<Value> = time::days(window.since, window.until)
                .into_iter()
                .map(|(day, since, until)| {
                    json!({
                        "day": day,
                        "dimensions": dimensions(&counts(history, scope, Window { since, until }), None),
                    })
                })
                .collect();
            let (kind, id) = match scope {
                Scope::Fleet => ("fleet", None),
                Scope::Mission(id) => ("mission", Some(id)),
            };
            json!({
                "scope": kind,
                "mission": id,
                "dimensions": dimensions(
                    &counts(history, scope, window),
                    Some(&counts(history, scope, previous)),
                ),
                "days": days,
            })
        })
        .collect();
    Ok(json!({
        "since_ms": window.since,
        "until_ms": window.until,
        "previous_since_ms": previous.since,
        "min_n": MIN_N,
        "scopes": scopes,
    }))
}

/// `spawn scorecard [--mission N] [--since <time>] [--until <time>]
/// [--json] [--root <dir>]`.
pub(crate) fn command(arguments: &[String]) -> Result<i32, String> {
    let mut query = Query::default();
    let mut json = false;
    let mut root = None;
    let mut words = arguments.iter();
    while let Some(word) = words.next() {
        if word == "--json" {
            json = true;
            continue;
        }
        let slot = match word.as_str() {
            "--mission" => &mut query.mission,
            "--since" => &mut query.since,
            "--until" => &mut query.until,
            "--root" => &mut root,
            other => return Err(format!("unexpected argument {other}; {USAGE}")),
        };
        let value = words
            .next()
            .filter(|value| !value.starts_with("--"))
            .ok_or_else(|| format!("{word} needs a value; {USAGE}"))?;
        *slot = Some(value.clone());
    }
    let root = root.or_else(|| crate::env_value("SPAWN_WORKSPACE_ROOT"));
    let store = crate::open_store()?;
    let card = measure(&store, root.as_deref().map(Path::new), &query)
        .map_err(|failure| failure.to_string())?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&card).map_err(|error| error.to_string())?
        );
    } else {
        print!("{}", text(&card));
    }
    Ok(0)
}

/// A JSON number as text: a whole ratio without its `.0`.
fn show(value: &Value) -> String {
    let text = value.to_string();
    text.strip_suffix(".0").map_or(text.clone(), str::to_string)
}

/// Milliseconds as a short span (`5h00m`, `12m30s`, `40s`).
fn span(ms: i64) -> String {
    let seconds = ms / 1000;
    if seconds >= 3600 {
        format!("{}h{:02}m", seconds / 3600, seconds / 60 % 60)
    } else if seconds >= 60 {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

/// Milliseconds with their span: `18000000 ms (5h00m)`.
fn ms(value: &Value) -> String {
    value
        .as_i64()
        .map_or_else(|| show(value), |ms| format!("{ms} ms ({})", span(ms)))
}

/// The counts of a breakdown, in the order the definitions name them.
fn counts_of(value: &Value, order: &[&str]) -> String {
    order
        .iter()
        .map(|name| {
            let count = value.get(*name).map_or_else(|| "0".to_string(), show);
            format!("{name} {count}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// A dimension's parts, in words.
fn parts_of(name: &str, field: &dyn Fn(&str) -> Value) -> String {
    match name {
        "throughput" => format!(
            "landed {}, open hours {}",
            show(&field("landed")),
            show(&field("open_hours"))
        ),
        "first_pass" => format!("first pass {}", show(&field("first_pass"))),
        "rework" => format!(
            "send-backs {}: {}",
            show(&field("send_backs")),
            counts_of(
                &field("by_verdict"),
                &["rejected", "conflict", "incompatible", "reviewed"],
            )
        ),
        "autonomy" => format!(
            "interventions {}: {}",
            show(&field("interventions")),
            counts_of(
                &field("by_kind"),
                &["command", "resume", "owner_task", "cancel_reassign"],
            )
        ),
        "reliability" => format!(
            "events {}: {}",
            show(&field("events")),
            counts_of(
                &field("by_kind"),
                &["failed_command", "spent_delivery", "stall", "gate_infrastructure"],
            )
        ),
        "cost" => {
            let models = field("by_model")
                .as_object()
                .map(|models| {
                    models
                        .iter()
                        .map(|(model, part)| {
                            let tokens = part.get("tokens").map(show).unwrap_or_default();
                            match part.get("per_landed").filter(|value| !value.is_null()) {
                                Some(per) => format!("{model} {tokens} ({} per landing)", show(per)),
                                None => format!("{model} {tokens}"),
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            let gate = field("gate_ms");
            let per = field("gate_ms_per_landed");
            format!(
                "tokens {}{}{models}; gate machine time {}{}",
                show(&field("tokens")),
                if models.is_empty() { "" } else { ": " },
                ms(&gate),
                if per.is_null() {
                    String::new()
                } else {
                    format!(", {} ms per landing", show(&per))
                }
            )
        }
        "gate_time" => format!("runs {}", show(&field("runs"))),
        _ => String::new(),
    }
}

/// One dimension's line: its value (or `not enough data`, with n and the
/// minimum), its parts, its direction, the previous value and the trend.
fn line(name: &str, dimension: &Value, min_n: &Value) -> String {
    let field = |key: &str| dimension.get(key).cloned().unwrap_or(Value::Null);
    let enough = field("enough").as_bool().unwrap_or(false);
    let n = show(&field("n"));
    let as_ms = matches!(name, "lead_time" | "gate_time");
    let shown = |value: &Value| if as_ms { ms(value) } else { show(value) };
    let value = if enough && !field("value").is_null() {
        match name {
            "throughput" => format!("{} landings per open hour", show(&field("value"))),
            "lead_time" => format!(
                "median {}, p90 {}, n {n}",
                ms(&field("value")),
                ms(&field("p90"))
            ),
            "first_pass" => format!("{} of landings, n {n}", show(&field("value"))),
            "rework" => format!("{} send-backs per landing, n {n}", show(&field("value"))),
            "autonomy" => format!("{} interventions per landing, n {n}", show(&field("value"))),
            "reliability" => format!("{} failures per landing, n {n}", show(&field("value"))),
            "cost" => format!("{} tokens per landing, n {n}", show(&field("value"))),
            "gate_time" => format!("median {} over {n} runs", ms(&field("value"))),
            _ => show(&field("value")),
        }
    } else {
        format!("not enough data (n {n}, minimum {})", show(min_n))
    };
    let parts = parts_of(name, &field);
    let previous = field("previous");
    let previous = if previous.is_null() {
        "previous none".to_string()
    } else {
        format!("previous {}", shown(&previous))
    };
    let trend = field("trend")
        .as_str()
        .map(|trend| format!(", {trend}"))
        .unwrap_or_default();
    let label = name.replace('_', " ");
    let parts = if parts.is_empty() || name == "lead_time" {
        String::new()
    } else {
        format!(" ({parts})")
    };
    format!(
        "  {label:<12} {value}{parts}  [{} is better; {previous}{trend}]\n",
        field("better").as_str().unwrap_or("?")
    )
}

/// The days of a scope as a table: one row per day, one column per
/// dimension's value, `-` where there is not enough data.
fn days_table(days: &[Value]) -> String {
    const COLUMNS: [(&str, &str); 8] = [
        ("throughput", "throughput"),
        ("lead_time", "lead_time_ms"),
        ("first_pass", "first_pass"),
        ("rework", "rework"),
        ("autonomy", "autonomy"),
        ("reliability", "reliability"),
        ("cost", "cost"),
        ("gate_time", "gate_time_ms"),
    ];
    let mut out = String::from("  days (- not enough data):\n    day       ");
    for (_, header) in COLUMNS {
        let _ = write!(out, "  {header:>12}");
    }
    out.push('\n');
    for day in days {
        let _ = write!(
            out,
            "    {}",
            day.get("day").and_then(Value::as_str).unwrap_or("?")
        );
        for (name, _) in COLUMNS {
            let value = day
                .pointer(&format!("/dimensions/{name}/value"))
                .filter(|value| !value.is_null())
                .map_or_else(|| "-".to_string(), show);
            let _ = write!(out, "  {value:>12}");
        }
        out.push('\n');
    }
    out
}

/// The text form of a scorecard: the same numbers as its JSON, one line per
/// dimension, then the days.
pub(crate) fn text(card: &Value) -> String {
    let at = |key: &str| card.get(key).and_then(Value::as_i64).unwrap_or(0);
    let min_n = card.get("min_n").cloned().unwrap_or(Value::Null);
    let mut out = format!(
        "spawn scorecard {} to {} (previous window from {}); minimum n {}\n",
        format_rfc3339(at("since_ms")),
        format_rfc3339(at("until_ms")),
        format_rfc3339(at("previous_since_ms")),
        show(&min_n)
    );
    for scope in card.get("scopes").and_then(Value::as_array).into_iter().flatten() {
        let title = match scope.get("mission").and_then(Value::as_i64) {
            Some(id) => format!("mission {id}"),
            None => "fleet".to_string(),
        };
        let _ = write!(out, "\n{title}\n");
        for name in [
            "throughput",
            "lead_time",
            "first_pass",
            "rework",
            "autonomy",
            "reliability",
            "cost",
            "gate_time",
        ] {
            if let Some(dimension) = scope.pointer(&format!("/dimensions/{name}")) {
                out.push_str(&line(name, dimension, &min_n));
            }
        }
        if let Some(days) = scope.get("days").and_then(Value::as_array) {
            out.push_str(&days_table(days));
        }
    }
    out
}
