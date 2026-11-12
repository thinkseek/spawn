//! `/api/scorecard/fleet?window=24h|7d|30d|90d[&until=<RFC 3339>]`: how the
//! fleet decides over `[until − window, until)` (objective 043; docs/
//! fleet-architecture.md §4 "Experience"): the decision ladder, human
//! decisions per agent-hour, escalations, interventions, outcomes, tasks
//! landed per day, the window's judgements and each open mission's idle
//! hours, from the experience views (spawn-postgres, `experience_window`).
//! Ratios and hours to 3 decimals, rounded as the scorecard rounds
//! ([`ratio_milli`]); a mission's open hours as throughput counts them
//! ([`union_ms`]).

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};
use spawn_postgres::PostgresStore;

use super::Failure;
use super::measure::{Number, Window, ratio_milli, union_ms};
use super::time::{DAY_MS, HOUR_MS, parse_rfc3339};

/// The windows this answer knows, by name.
pub(crate) const WINDOWS: [(&str, i64); 4] = [
    ("24h", DAY_MS),
    ("7d", 7 * DAY_MS),
    ("30d", 30 * DAY_MS),
    ("90d", 90 * DAY_MS),
];

/// The ladder's known tiers, cheapest first; any other is `ladder_extra`.
const TIERS: [&str; 6] = [
    "rules",
    "model",
    "dispatcher",
    "supervisor",
    "human",
    "liveness",
];
const KINDS: [&str; 4] = ["command", "resume", "owner_task", "cancel_reassign"];
const OUTCOMES: [&str; 5] = ["landed", "accepted", "cancelled", "aborted", "over_budget"];

/// The window `window` (24h when absent) ending at `until` (an RFC 3339
/// instant; `now` when absent).
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
        .ok_or_else(|| Failure::Request(format!("window {asked}: not one of 24h, 7d, 30d, 90d")))?;
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

fn count(value: &Value) -> i64 {
    value.as_i64().unwrap_or(0)
}

fn milli(numerator: i64, denominator: i64) -> Value {
    ratio_milli(i128::from(numerator), i128::from(denominator))
        .map_or(Value::Null, |value| Number::Milli(value).json())
}

/// The answer for `window` (named `name`) from the store.
pub(crate) fn measure(store: &PostgresStore, name: &str, window: Window) -> Result<Value, Failure> {
    let counts = store
        .experience_window(window.since, window.until)
        .map_err(|error| Failure::Record(error.to_string()))?;
    Ok(answer(&counts, name, window))
}

/// The answer from the store's counts of `window`.
pub(crate) fn answer(counts: &Value, name: &str, window: Window) -> Value {
    let ladder_counts = counts["ladder"].as_object().cloned().unwrap_or_default();
    let mut ladder = Map::new();
    for tier in TIERS {
        ladder.insert(
            tier.to_string(),
            json!(ladder_counts.get(tier).map_or(0, count)),
        );
    }
    let extra: BTreeMap<&String, i64> = ladder_counts
        .iter()
        .filter(|(tier, _)| !TIERS.contains(&tier.as_str()))
        .map(|(tier, n)| (tier, count(n)))
        .collect();
    let ladder_extra: Vec<Value> = extra
        .into_iter()
        .map(|(tier, n)| json!([tier, n]))
        .collect();
    let agent_ms = count(&counts["agent_ms"]);
    let human = count(&ladder["human"]);
    let by_kind: Map<String, Value> = KINDS
        .iter()
        .map(|kind| {
            (
                (*kind).to_string(),
                json!(count(&counts["interventions"][kind])),
            )
        })
        .collect();
    let interventions: i64 = counts["interventions"]
        .as_object()
        .map_or(0, |kinds| kinds.values().map(count).sum());
    let outcomes: Map<String, Value> = OUTCOMES
        .iter()
        .map(|outcome| {
            (
                (*outcome).to_string(),
                json!(count(&counts["outcomes"][outcome])),
            )
        })
        .collect();
    let landings = count(&outcomes["landed"]) + count(&outcomes["accepted"]);
    let length = window.until - window.since;
    let judgements = &counts["judgements"];
    let missions: Vec<Value> = counts["missions"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|mission| idle(mission, window))
        .collect();
    json!({
        "window": name,
        "since_ms": window.since,
        "until_ms": window.until,
        "ladder": ladder,
        "ladder_extra": ladder_extra,
        "agent_hours": milli(agent_ms, HOUR_MS),
        "human_per_agent_hour": if agent_ms > 0 {
            milli(human.saturating_mul(HOUR_MS), agent_ms)
        } else {
            Value::Null
        },
        "escalations": count(&counts["escalations"]),
        "interventions": { "total": interventions, "by_kind": by_kind },
        "outcomes": outcomes,
        "landed_per_day": milli(landings.saturating_mul(DAY_MS), length),
        "judgements": {
            "total": count(&judgements["total"]),
            "by_judge": judgements["by_judge"].as_object().cloned().unwrap_or_default(),
            "judge_unavailable": count(&judgements["judge_unavailable"]),
            "disagreements": count(&judgements["disagreements"]),
            "model_latency_p50_ms": judgements["model_latency_p50_ms"]
                .as_f64()
                .map_or(Value::Null, |ms| json!(ms.round())),
        },
        "missions": missions,
    })
}

/// The `[start, end]` pairs of `value` (an open end is `None`).
fn intervals(value: &Value) -> Vec<(i64, Option<i64>)> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|pair| Some((pair[0].as_i64()?, pair[1].as_i64())))
        .collect()
}

/// One mission's span in the window, its paused hours (the owner's pauses
/// within the span, objective 099), its idle hours (the span less the union
/// of its tasks' open intervals and its pauses: a paused hour is the
/// owner's choice, not idle), whether it is owner work (no objective
/// backlog) and whether it sat idle more than half of its span less its
/// paused time, which an owner-work mission never is, nor one paused at the
/// window's end.
fn idle(mission: &Value, window: Window) -> Value {
    let opened = count(&mission["opened_at_ms"]);
    let span = Window {
        since: opened.max(window.since),
        until: mission["closed_at_ms"]
            .as_i64()
            .map_or(window.until, |closed| closed.min(window.until)),
    };
    let span_ms = (span.until - span.since).max(0);
    let tasks = intervals(&mission["tasks"]);
    let pauses = intervals(&mission["pauses"]);
    let (busy_ms, paused_ms) = if span_ms > 0 {
        let both: Vec<(i64, Option<i64>)> = tasks.iter().chain(&pauses).copied().collect();
        (union_ms(&both, span), union_ms(&pauses, span))
    } else {
        (0, 0)
    };
    let idle_ms = span_ms - busy_ms;
    // A mission with no objective backlog takes its work from the owner: an
    // hour without a task there is no loss, so it is never flagged idle.
    let owner_work = mission["owner_work"].as_bool().unwrap_or(false);
    // Paused at the window's end: the owner holds it now.
    let paused_now = pauses.iter().any(|(_, end)| end.is_none());
    json!({
        "mission": mission["mission"],
        "text": mission["text"],
        "hours": milli(span_ms, HOUR_MS),
        "idle_hours": milli(idle_ms, HOUR_MS),
        "paused_hours": milli(paused_ms, HOUR_MS),
        "idle": !owner_work && !paused_now && idle_ms * 2 > span_ms - paused_ms,
        "owner_work": owner_work,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const U: i64 = 1_793_404_800_000;

    fn at(hours: f64) -> i64 {
        #[allow(clippy::cast_possible_truncation)]
        let offset = (hours * 3_600_000.0) as i64;
        U + offset
    }

    fn fail(result: Result<(&'static str, Window), Failure>) -> String {
        match result {
            Err(Failure::Request(error)) => error,
            other => panic!("a request failure, not {other:?}"),
        }
    }

    #[test]
    fn a_window_is_one_it_knows_ending_at_until_or_now() {
        let now = || Ok(U);
        assert_eq!(
            window_of(Some("7d"), Some("2026-10-31T00:00:00Z"), || panic!(
                "no clock read"
            ))
            .expect("a window"),
            (
                "7d",
                Window {
                    since: U - 7 * DAY_MS,
                    until: U
                }
            )
        );
        assert_eq!(
            window_of(None, None, now).expect("the default"),
            (
                "24h",
                Window {
                    since: U - DAY_MS,
                    until: U
                }
            )
        );
        assert!(fail(window_of(Some("2d"), None, now)).contains("not one of"));
        assert!(fail(window_of(Some("30d"), Some("yesterday"), now)).contains("RFC 3339"));
    }

    #[test]
    fn the_answer_rounds_as_the_scorecard_and_marks_an_idle_mission() {
        let window = Window {
            since: U - 7 * DAY_MS,
            until: U,
        };
        let counts = json!({
            "ladder": {"rules": 1, "model": 3, "dispatcher": 4, "supervisor": 1, "human": 1,
                       "liveness": 1, "spawn-integrate": 1, "owner_gate": 2},
            "interventions": {"command": 1, "resume": 1, "owner_task": 1},
            "outcomes": {"landed": 2, "accepted": 1, "aborted": 1},
            "escalations": 3,
            "agent_ms": 26 * HOUR_MS,
            "judgements": {"total": 4, "by_judge": {"jev": 3, "rules": 1}, "judge_unavailable": 1,
                           "disagreements": 1, "model_latency_p50_ms": 500.0},
            "missions": [
                {"mission": 1, "text": "busy", "opened_at_ms": at(-720.0), "closed_at_ms": null,
                 "tasks": [[at(-100.0), at(-90.0)], [at(-60.0), at(-50.0)], [at(-20.0), at(-10.0)],
                           [at(-5.0), null]]},
                {"mission": 2, "text": "idle", "opened_at_ms": at(-48.0), "closed_at_ms": null,
                 "tasks": [[at(-40.0), at(-24.0)]]},
                {"mission": 3, "text": "half", "opened_at_ms": at(-10.0), "closed_at_ms": at(-2.0),
                 "tasks": [[at(-6.0), at(-1.0)]]}
            ]
        });
        let answer = answer(&counts, "7d", window);
        assert_eq!(answer["ladder"]["human"], 1);
        assert_eq!(
            answer["ladder_extra"],
            json!([["owner_gate", 2], ["spawn-integrate", 1]])
        );
        assert_eq!(answer["agent_hours"], json!(26.0));
        assert_eq!(answer["human_per_agent_hour"], json!(0.038));
        assert_eq!(answer["landed_per_day"], json!(0.429));
        assert_eq!(answer["interventions"]["total"], 3);
        assert_eq!(answer["interventions"]["by_kind"]["cancel_reassign"], 0);
        assert_eq!(answer["outcomes"]["over_budget"], 0);
        assert_eq!(answer["judgements"]["model_latency_p50_ms"], json!(500.0));
        let missions: Vec<(i64, f64, f64, bool)> = answer["missions"]
            .as_array()
            .expect("missions")
            .iter()
            .map(|m| {
                (
                    m["mission"].as_i64().expect("id"),
                    m["idle_hours"].as_f64().expect("idle"),
                    m["hours"].as_f64().expect("hours"),
                    m["idle"].as_bool().expect("mark"),
                )
            })
            .collect();
        assert_eq!(
            missions,
            vec![
                (1, 133.0, 168.0, true),
                (2, 32.0, 48.0, true),
                (3, 4.0, 8.0, false)
            ],
            "half the span idle is not idle"
        );
    }

    #[test]
    fn an_owner_work_mission_is_never_idle_and_a_backlog_one_is_as_before() {
        let window = Window {
            since: U - DAY_MS,
            until: U,
        };
        let counts = json!({
            "missions": [
                {"mission": 1, "text": "backlog", "opened_at_ms": at(-48.0), "closed_at_ms": null,
                 "owner_work": false, "tasks": []},
                {"mission": 2, "text": "owner", "opened_at_ms": at(-48.0), "closed_at_ms": null,
                 "owner_work": true, "tasks": []},
                {"mission": 3, "text": "busy backlog", "opened_at_ms": at(-48.0), "closed_at_ms": null,
                 "owner_work": false, "tasks": [[at(-30.0), null]]},
                {"mission": 4, "text": "busy owner", "opened_at_ms": at(-48.0), "closed_at_ms": null,
                 "owner_work": true, "tasks": [[at(-30.0), null]]}
            ]
        });
        let answer = answer(&counts, "24h", window);
        let rows: Vec<(i64, f64, bool, bool)> = answer["missions"]
            .as_array()
            .expect("missions")
            .iter()
            .map(|m| {
                (
                    m["mission"].as_i64().expect("id"),
                    m["idle_hours"].as_f64().expect("idle hours"),
                    m["idle"].as_bool().expect("idle"),
                    m["owner_work"].as_bool().expect("owner work"),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                (1, 24.0, true, false),
                (2, 24.0, false, true),
                (3, 0.0, false, false),
                (4, 0.0, false, true)
            ],
            "owner work keeps its idle hours but is never flagged idle"
        );
    }

    #[test]
    fn paused_time_is_not_idle_and_a_mission_paused_at_the_end_is_never_flagged() {
        let window = Window {
            since: U - DAY_MS,
            until: U,
        };
        let counts = json!({
            "missions": [
                // Paused for the last 20 h, still paused: 4 h idle, 20 paused.
                {"mission": 1, "text": "paused now", "opened_at_ms": at(-48.0), "closed_at_ms": null,
                 "owner_work": false, "tasks": [], "pauses": [[at(-20.0), null]]},
                // Paused 12 h and resumed; a task over part of the pause:
                // idle 24 − 12 (paused) − 2 (the task outside it) = 10 > (24 − 12) / 2.
                {"mission": 2, "text": "resumed", "opened_at_ms": at(-48.0), "closed_at_ms": null,
                 "owner_work": false, "tasks": [[at(-14.0), at(-10.0)]],
                 "pauses": [[at(-22.0), at(-10.0)]]},
                // Paused 16 h, resumed, then busy 4 of the 8 h left: idle 4,
                // not more than half of 24 − 16.
                {"mission": 3, "text": "half", "opened_at_ms": at(-48.0), "closed_at_ms": null,
                 "owner_work": false, "tasks": [[at(-4.0), null]],
                 "pauses": [[at(-30.0), at(-8.0)]]},
                // No pause at all: as before.
                {"mission": 4, "text": "idle", "opened_at_ms": at(-48.0), "closed_at_ms": null,
                 "owner_work": false, "tasks": []}
            ]
        });
        let answer = answer(&counts, "24h", window);
        let rows: Vec<(i64, f64, f64, f64, bool)> = answer["missions"]
            .as_array()
            .expect("missions")
            .iter()
            .map(|m| {
                (
                    m["mission"].as_i64().expect("id"),
                    m["hours"].as_f64().expect("hours"),
                    m["idle_hours"].as_f64().expect("idle hours"),
                    m["paused_hours"].as_f64().expect("paused hours"),
                    m["idle"].as_bool().expect("idle"),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                (1, 24.0, 4.0, 20.0, false),
                (2, 24.0, 10.0, 12.0, true),
                (3, 24.0, 4.0, 16.0, false),
                (4, 24.0, 24.0, 0.0, true)
            ],
            "paused hours are left out of idle hours, and a mission paused at the end is not idle"
        );
    }

    #[test]
    fn no_agent_hours_means_no_rate() {
        let window = Window {
            since: U - DAY_MS,
            until: U,
        };
        let answer = answer(&json!({"ladder": {"human": 2}}), "24h", window);
        assert_eq!(answer["human_per_agent_hour"], Value::Null);
        assert_eq!(answer["agent_hours"], json!(0.0));
        assert_eq!(answer["landed_per_day"], json!(0.0));
        assert_eq!(answer["judgements"]["model_latency_p50_ms"], Value::Null);
        assert_eq!(answer["missions"], json!([]));
    }
}
