//! The 7-day trends.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::DAY_MS;

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

#[cfg(test)]
mod tests {
    use super::super::*;
    use serde_json::{Value, json};

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
            &adopted(&lines(
                &json!({"free_bytes": 3}),
                &Value::Null,
                &Observed::default(),
                Value::Null,
                Vec::new(),
            )),
            &Value::Null,
            Ok(None),
            &trends,
        );
        // Every list carries trends (objective 176): free disk is an alert
        // figure now.
        let mut seen = 0;
        for line in view["lines"].as_array().expect("lines") {
            for figure in std::iter::once(&line["north_star"])
                .chain(line["guardrails"].as_array().expect("guardrails"))
                .chain(view["alert_figures"].as_array().expect("alert figures"))
                .chain(view["diagnostics"].as_array().expect("diagnostics"))
            {
                seen += usize::from(figure["key"] == "storage.free_bytes");
                let want = if figure["key"] == "storage.free_bytes" {
                    json!([{"day": "2026-11-12", "value": 3}])
                } else {
                    json!([])
                };
                assert_eq!(figure["trend"], want, "{figure}");
            }
        }
        assert!(seen > 0, "free disk's trend is read");
    }
}
