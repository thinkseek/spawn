//! The observability line.

use serde_json::{Value, json};

use super::{Figure, Line};

/// What the observability line reads besides the answer itself: the 7-day
/// cost window's `total`, the Home card's human interventions per
/// agent-hour (`human_per_agent_hour`: `/api/interventions?window=24h`'s
/// `per_agent_hour`, objective 100), and the north star "Needs you is right"
/// and what it is read from (`needs_judged`, `{fired, judged, unjudged}`;
/// `None` when not read), both from [`judged_share`] (objective 148).
#[derive(Debug, Clone, Default, PartialEq)]
pub(in crate::serve) struct Observed {
    pub(in crate::serve) cost_total: Value,
    pub(in crate::serve) human_per_agent_hour: Value,
    pub(in crate::serve) needs_you_right: Value,
    pub(in crate::serve) needs_judged: Option<Value>,
}

/// The fewest needs judged for "Needs you is right" to have a value.
const FEWEST_JUDGED: u32 = 5;

/// "Needs you is right" (objective 148) from the store's count of the needs
/// fired in the last 7 days, `{fired, judged, right}` (`None` when not
/// read): right ÷ judged, null while fewer than [`FEWEST_JUDGED`] are
/// judged; and what it is read from, `{fired, judged, unjudged}` (unjudged
/// = fired − judged), `None` when not read. A need nobody judged counts
/// neither way, and a clearing is never evidence.
pub(in crate::serve) fn judged_share(counts: Option<&Value>) -> (Value, Option<Value>) {
    let Some(counts) = counts else {
        return (Value::Null, None);
    };
    let count = |name: &str| {
        counts[name]
            .as_u64()
            .map_or(0, |n| u32::try_from(n).unwrap_or(u32::MAX))
    };
    let (fired, judged, right) = (count("fired"), count("judged"), count("right"));
    let said = json!({
        "fired": fired,
        "judged": judged,
        "unjudged": fired.saturating_sub(judged),
    });
    let value = if judged >= FEWEST_JUDGED {
        json!(f64::from(right) / f64::from(judged))
    } else {
        Value::Null
    };
    (value, Some(said))
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

/// The observability line (objective 088): its count not measured is set by
/// [`count_not_measured`] once every line is built.
pub(super) fn observability_line(observed: &Observed) -> Line {
    Line {
        name: "observability",
        north_star: Figure {
            // What the share is read from, said beside it (objective 148).
            judged: observed.needs_judged.clone(),
            ..Figure::new(
                "observability.needs_you_right",
                "Needs you is right",
                "share",
                observed.needs_you_right.clone(),
            )
        },
        guardrails: vec![
            Figure::new(
                "observability.unpriced_share",
                "Use with no price",
                "share",
                unpriced_share(&observed.cost_total),
            ),
            Figure {
                // The Home card's figure over its 24 h, said beside it.
                window: Some("24h"),
                ..Figure::new(
                    "observability.interventions_per_agent_hour",
                    "Human interventions per agent-hour",
                    "ratio",
                    if observed.human_per_agent_hour.is_number() {
                        observed.human_per_agent_hour.clone()
                    } else {
                        Value::Null
                    },
                )
            },
            Figure::not_collected(NOT_MEASURED, "Figures not measured", "count"),
        ],
    }
}

/// Sets the count of figures not measured: those of `lines` with no value,
/// the count itself left out.
pub(super) fn count_not_measured(lines: &mut [Line]) {
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

/// The window of the observability north star: the needs fired in the last
/// 7 days.
pub(super) const WEEK_MS: i64 = 7 * 24 * 3_600_000;

/// "Needs you is right" (objective 092): of the alerts (not notices) that
/// ended in the 7 days up to `now_ms` (`cleared_at_ms` in the window), the
/// share whose outcome is "cleared", their condition cleared them rather
/// than the owner judging them false, stale or a duplicate; null with none.
/// `alerts` are rows as `/api/alerts` gives them. Not read live until A23
/// replaces it: a condition clearing is not evidence the alert was right.
#[cfg(test)]
pub(super) fn needs_you_right(alerts: &Value, now_ms: i64) -> Value {
    let since = now_ms.saturating_sub(WEEK_MS);
    let (mut ended, mut right) = (0_u32, 0_u32);
    for alert in alerts.as_array().into_iter().flatten() {
        if alert["state"] == "notice" || alert["notice"] == true {
            continue;
        }
        let Some(at) = alert["cleared_at_ms"].as_i64() else {
            continue;
        };
        if at < since || at > now_ms {
            continue;
        }
        ended += 1;
        if alert["outcome"] == "cleared" {
            right += 1;
        }
    }
    if ended == 0 {
        Value::Null
    } else {
        json!(f64::from(right) / f64::from(ended))
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;
    use serde_json::{Value, json};

    /// Objective 100: interventions per agent-hour is the Home card's 24 h
    /// figure and says its window; no other figure carries one.
    #[test]
    fn interventions_per_agent_hour_says_its_24_hour_window_and_no_other_figure_does() {
        let observed = Observed {
            human_per_agent_hour: json!(0.25),
            ..Observed::default()
        };
        let all = lines(&json!({}), &Value::Null, &observed, Value::Null, Vec::new());
        let figures: Vec<&Figure> = all
            .iter()
            .flat_map(|line| std::iter::once(&line.north_star).chain(&line.guardrails))
            .collect();
        let windowed: Vec<(&str, Option<&str>)> = figures
            .iter()
            .filter(|figure| figure.window.is_some())
            .map(|figure| (figure.key, figure.window))
            .collect();
        assert_eq!(
            windowed,
            [("observability.interventions_per_agent_hour", Some("24h"))]
        );
        let view = figure_json(
            figures
                .iter()
                .find(|figure| figure.key == "observability.interventions_per_agent_hour")
                .expect("the figure"),
            None,
        );
        assert_eq!(view["window"], "24h");
        assert_eq!(view["value"], json!(0.25));
        assert_eq!(view["name"], "Human interventions per agent-hour");
        // Null when the card's figure is (no agent time).
        let idle = lines(
            &json!({}),
            &Value::Null,
            &Observed::default(),
            Value::Null,
            Vec::new(),
        );
        let figure = idle[2]
            .guardrails
            .iter()
            .find(|figure| figure.key == "observability.interventions_per_agent_hour")
            .expect("the figure");
        assert_eq!(figure.value, Value::Null);
        assert!(
            figure_json(figure, None).get("window").is_some(),
            "said even when null"
        );
        assert!(
            figure_json(&idle[2].guardrails[0], None)
                .get("window")
                .is_none(),
            "no other figure says a window"
        );
    }

    /// Objective 092: the share of the alerts that ended in the last 7
    /// days whose condition cleared them.
    #[test]
    fn needs_you_is_right_is_the_share_of_the_ended_alerts_their_condition_cleared() {
        use super::needs_you_right;
        const DAY: i64 = 86_400_000;
        let now = 30 * DAY;
        let row = |state: &str, cleared: Option<i64>, outcome: Option<&str>| json!({"key": "k", "state": state, "cleared_at_ms": cleared, "outcome": outcome});
        // None ended: none open, none at all.
        assert_eq!(needs_you_right(&json!([]), now), Value::Null);
        assert_eq!(
            needs_you_right(&json!([row("open", None, None)]), now),
            Value::Null
        );
        // A notice is left out; one ended outside the window too.
        let rows = json!([
            row("cleared", Some(now - DAY), Some("cleared")),
            row("cleared", Some(now - 2 * DAY), Some("false")),
            row("cleared", Some(now - 3 * DAY), Some("duplicate")),
            row("cleared", Some(now - 6 * DAY), Some("cleared")),
            row("open", None, None),
            row("notice", Some(now - DAY), None),
            row("cleared", Some(now - 8 * DAY), Some("stale")),
            row("cleared", Some(now - 9 * DAY), Some("cleared")),
        ]);
        let share = needs_you_right(&rows, now).as_f64().expect("a share");
        assert!((share - 0.5).abs() < 1e-12, "{share}");
        // Only the ones outside the window: none.
        let old = json!([row("cleared", Some(now - 8 * DAY), Some("cleared"))]);
        assert_eq!(needs_you_right(&old, now), Value::Null);
        // The line carries it.
        let observed = Observed {
            needs_you_right: json!(0.5),
            ..Observed::default()
        };
        let line = &lines(&json!({}), &Value::Null, &observed, Value::Null, Vec::new())[2];
        assert_eq!(line.north_star.key, "observability.needs_you_right");
        assert_eq!(line.north_star.unit, "share");
        assert_eq!(line.north_star.value, json!(0.5));
    }

    /// Objective 148: "Needs you is right" is right ÷ judged of the needs
    /// fired in the 7 days, null under 5 judged, said with what it is read
    /// from; nothing said when not read.
    #[test]
    fn needs_you_is_right_is_the_share_judged_right_of_the_judged() {
        let counts = |fired: u64, judged: u64, right: u64| json!({"fired": fired, "judged": judged, "right": right});
        let said = |fired: u64, judged: u64, unjudged: u64| {
            Some(json!({"fired": fired, "judged": judged, "unjudged": unjudged}))
        };
        // Under 5 judged: no value, the counts said.
        assert_eq!(
            judged_share(Some(&counts(6, 4, 4))),
            (Value::Null, said(6, 4, 2))
        );
        assert_eq!(
            judged_share(Some(&counts(0, 0, 0))),
            (Value::Null, said(0, 0, 0))
        );
        // 5 judged: right over judged, never over fired.
        assert_eq!(
            judged_share(Some(&counts(9, 5, 3))),
            (json!(0.6), said(9, 5, 4))
        );
        assert_eq!(
            judged_share(Some(&counts(5, 5, 0))),
            (json!(0.0), said(5, 5, 0))
        );
        assert_eq!(
            judged_share(Some(&counts(7, 5, 5))),
            (json!(1.0), said(7, 5, 2))
        );
        // Not read: nothing said.
        assert_eq!(judged_share(None), (Value::Null, None));

        // The line carries it, and the answer says `judged` beside it.
        let (needs_you_right, needs_judged) = judged_share(Some(&counts(6, 5, 3)));
        let observed = Observed {
            needs_you_right,
            needs_judged,
            ..Observed::default()
        };
        let line = &lines(&json!({}), &Value::Null, &observed, Value::Null, Vec::new())[2];
        let view = figure_json(&line.north_star, None);
        assert_eq!(view["key"], "observability.needs_you_right");
        assert_eq!(view["name"], "Needs you is right");
        assert_eq!(view["value"], json!(0.6));
        assert_eq!(
            view["judged"],
            json!({"fired": 6, "judged": 5, "unjudged": 1})
        );
        // Not read: no `judged`, and no other figure says one.
        let bare = lines(
            &json!({}),
            &Value::Null,
            &Observed::default(),
            Value::Null,
            Vec::new(),
        );
        for figure in bare
            .iter()
            .flat_map(|line| std::iter::once(&line.north_star).chain(&line.guardrails))
        {
            assert!(
                figure_json(figure, None).get("judged").is_none(),
                "{}",
                figure.key
            );
        }
    }

    /// Objective 088: the observability line's guardrails.
    #[test]
    fn the_observability_line_gives_unpriced_use_interventions_and_the_count_not_measured() {
        let storage = json!({
            "days_left": 1234, "free_bytes": 104, "growth_bytes_per_day": 10,
            "database": {"bytes": 300, "log_bytes": 100},
            "test_databases": [{"bytes": 7, "schemas": 3}],
            "unowned_scratch_bytes": 0, "leaked_test_schemas": 0,
            // Two storage readings a day apart with a landing between them
            // (objective 185): the storage north star and S1 are measured.
            "readings": {"newest": {"at_ms": 2 * 86_400_000, "sources": {"scratch": {"bytes": 9},
                                    "build_cache": {"bytes": 1}, "test_databases": {"bytes": 0}}},
                         "older": {"at_ms": 0, "sources": {"scratch": {"bytes": 3}}},
                         "landings": 2},
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
            ..Observed::default()
        };
        let all = lines(&storage, &Value::Null, &observed, Value::Null, Vec::new());
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
        // two other north stars not collected, the disk's fall and the
        // store that grew most (objective 141: no readings here), every
        // other storage figure measured, itself left out though it starts
        // with no value.
        assert_eq!(
            guardrails,
            [
                ("observability.unpriced_share", "share", json!(0.25)),
                (
                    "observability.interventions_per_agent_hour",
                    "ratio",
                    json!(0.125)
                ),
                ("observability.not_measured", "count", json!(9)),
            ]
        );
        // Nothing read: the two guardrails count too.
        let bare = lines(
            &storage,
            &Value::Null,
            &Observed::default(),
            Value::Null,
            Vec::new(),
        );
        assert_eq!(bare[2].guardrails[0].value, Value::Null);
        assert_eq!(bare[2].guardrails[1].value, Value::Null);
        assert_eq!(bare[2].guardrails[2].value, json!(11));
        // Its range and state as for every figure.
        // Its range and state as for every figure, where the owner's set
        // (objective 176) puts it: the interventions are the sustainability
        // north star, unpriced use its third guardrail; the count not
        // measured is retired.
        let view = answer(
            &adopted(&all),
            &Value::Null,
            Ok(Some(
                r#"{"ranges": {"observability.unpriced_share": {"max": 0.05},
                    "observability.interventions_per_agent_hour": {"max": 0.05},
                    "observability.not_measured": {"max": 0}}}"#
                    .to_string(),
            )),
            &std::collections::BTreeMap::new(),
        );
        let sustainability = &view["lines"][2];
        assert_eq!(
            sustainability["north_star"]["key"],
            "observability.interventions_per_agent_hour"
        );
        assert_eq!(sustainability["north_star"]["state"], "out");
        assert_eq!(sustainability["north_star"]["window"], "24h");
        assert_eq!(
            sustainability["guardrails"][2]["key"],
            "observability.unpriced_share"
        );
        assert_eq!(sustainability["guardrails"][2]["state"], "out");
        assert_eq!(
            view["ignored_ranges"],
            json!([{"key": "observability.not_measured", "reason": RETIRED_REASON}])
        );
    }
}
