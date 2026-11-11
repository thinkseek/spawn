//! `/api/interventions?window=24h|7d[&until=<RFC 3339>]` (objective 049;
//! docs/fleet-architecture.md §0): the north star, human interventions per
//! agent-hour, over `[until − window, until)`, with the interventions behind
//! it. A person's command refused and sent again is one intervention (an
//! intent, `spawn_human_intents`); the commands beyond each intent's first
//! are `retries`, left out of the count. The interventions are the
//! experience views' (`spawn_interventions`), which the scorecard's autonomy
//! counts the same; each command's outcome is as it stood before `until`.
//! Hours and the ratio to 3 decimals, rounded as the scorecard rounds.

use serde_json::{Map, Value, json};
use spawn_postgres::PostgresStore;

use super::Failure;
use super::measure::{Number, Window, ratio_milli};
use super::time::{DAY_MS, HOUR_MS, parse_rfc3339};

/// The windows this answer knows, by name.
pub(crate) const WINDOWS: [(&str, i64); 2] = [("24h", DAY_MS), ("7d", 7 * DAY_MS)];

/// The kinds of intervention, as the scorecard names them.
const KINDS: [&str; 4] = ["command", "resume", "owner_task", "cancel_reassign"];

/// The window `window` (24h when absent) ending at `until` (an RFC 3339
/// instant; `now` when absent). Any other window, or an `until` that is no
/// time, is the caller's mistake.
pub(crate) fn window_of(
    window: Option<&str>,
    until: Option<&str>,
    now: impl FnOnce() -> Result<i64, Failure>,
) -> Result<(&'static str, Window), Failure> {
    let asked = window.unwrap_or("24h");
    let (name, length) = WINDOWS
        .iter()
        .find(|(name, _)| *name == asked)
        .copied()
        .ok_or_else(|| Failure::Request(format!("window {asked}: not one of 24h, 7d")))?;
    let until = match until {
        Some(text) => parse_rfc3339(text).ok_or_else(|| {
            Failure::Request(format!(
                "until {text}: not an RFC 3339 time (2026-10-31T00:00:00Z)"
            ))
        })?,
        None => now()?,
    };
    Ok((
        name,
        Window {
            since: until.saturating_sub(length),
            until,
        },
    ))
}

fn milli(numerator: i64, denominator: i64) -> Value {
    ratio_milli(i128::from(numerator), i128::from(denominator))
        .map_or(Value::Null, |value| Number::Milli(value).json())
}

/// The answer for `window` (named `name`) from the store.
pub(crate) fn measure(store: &PostgresStore, name: &str, window: Window) -> Result<Value, Failure> {
    let read = store
        .interventions_window(window.since, window.until)
        .map_err(|error| Failure::Record(error.to_string()))?;
    Ok(answer(&read, name, window))
}

/// The answer from the store's read of `window`: `{agent_ms, items}`.
pub(crate) fn answer(read: &Value, name: &str, window: Window) -> Value {
    let items: Vec<Value> = read["items"].as_array().cloned().unwrap_or_default();
    let mut by_kind: Map<String, Value> = KINDS
        .iter()
        .map(|kind| ((*kind).to_string(), json!(0)))
        .collect();
    let mut retries = 0_i64;
    for item in &items {
        let kind = item["kind"].as_str().unwrap_or_default();
        let entry = by_kind.entry(kind.to_string()).or_insert(json!(0));
        *entry = json!(entry.as_i64().unwrap_or(0) + 1);
        if kind == "command" {
            retries += item["attempts"].as_i64().unwrap_or(1).saturating_sub(1);
        }
    }
    let total = i64::try_from(items.len()).unwrap_or(i64::MAX);
    let agent_ms = read["agent_ms"].as_i64().unwrap_or(0);
    json!({
        "window": name,
        "since_ms": window.since,
        "until_ms": window.until,
        "agent_hours": milli(agent_ms, HOUR_MS),
        "total": total,
        "by_kind": by_kind,
        "retries": retries,
        "per_agent_hour": if agent_ms > 0 {
            milli(total.saturating_mul(HOUR_MS), agent_ms)
        } else {
            Value::Null
        },
        "items": items,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Failure, Window, answer, window_of};
    use crate::scorecard::time::{DAY_MS, HOUR_MS};

    #[test]
    fn the_window_is_24h_or_7d_ending_at_until_or_now() {
        let (name, window) = window_of(None, None, || Ok(10 * DAY_MS)).expect("24h by default");
        assert_eq!(name, "24h");
        assert_eq!(
            window,
            Window {
                since: 9 * DAY_MS,
                until: 10 * DAY_MS
            }
        );
        let (name, window) = window_of(Some("7d"), Some("1970-01-11T00:00:00Z"), || {
            panic!("an until needs no now")
        })
        .expect("7d");
        assert_eq!((name, window.since, window.until), ("7d", 3 * DAY_MS, 10 * DAY_MS));
        for (window, until) in [(Some("2d"), None), (Some("30d"), None), (None, Some("yesterday"))]
        {
            assert!(
                matches!(window_of(window, until, || Ok(0)), Err(Failure::Request(_))),
                "{window:?} {until:?}"
            );
        }
    }

    #[test]
    fn the_answer_counts_one_per_intent_and_says_the_retries() {
        let read = json!({
            "agent_ms": 20 * HOUR_MS,
            "items": [
                {"kind": "command", "attempts": 2, "commands": [3, 4]},
                {"kind": "resume", "attempts": null, "commands": []},
                {"kind": "command", "attempts": 5, "commands": [1, 2, 5, 6, 7]},
            ]
        });
        let window = Window {
            since: 0,
            until: DAY_MS,
        };
        let answer = answer(&read, "24h", window);
        assert_eq!(answer["total"], 3);
        assert_eq!(
            answer["by_kind"],
            json!({"command": 2, "resume": 1, "owner_task": 0, "cancel_reassign": 0})
        );
        assert_eq!(answer["retries"], 5);
        assert_eq!(answer["agent_hours"], json!(20.0));
        assert_eq!(answer["per_agent_hour"], json!(0.15));
        assert_eq!(answer["items"].as_array().map(Vec::len), Some(3));
        let idle = super::answer(&json!({"agent_ms": 0, "items": []}), "24h", window);
        assert_eq!(idle["per_agent_hour"], json!(null));
        assert_eq!(idle["total"], 0);
    }
}
