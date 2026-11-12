//! The performance line and its report-to-live record.

use serde_json::{Value, json};

use super::{DAY_MS, Figure, Line};

/// How long the performance figures are kept: a Home poll must not compute
/// the 7-day scorecard and cost window again at every request.
const PERFORMANCE_KEPT_MS: i64 = 60_000;

/// The performance figures of the last read and the 7-day cost window's
/// `total` (objective 088), and when.
static PERFORMANCE: std::sync::Mutex<Kept> = std::sync::Mutex::new(None);

/// The performance line's values (objective 086), from the Scorecard's
/// answer over its default 7 days and `/api/cost?window=7d`'s: the fleet
/// scope's `report_to_live` value, `gate_time` value (the median),
/// `gate_time`'s `undecided_ms` over its `total_ms` (null when the total is
/// 0), `first_pass` value, and the cost window's `usd_per_landing`. A value
/// the page does not have is null. While the 7 days are not covered by the
/// live record (`report_to_live`'s `covered` false, objective 095), report
/// to live has no value and `measuring_since_ms` is its `measured_from_ms`:
/// the figure would measure the record's start, not the wait. [`measured`]
/// holds it back by the same rule from the record's start alone.
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
    let live = &fleet["report_to_live"];
    let mut values = json!({
        "report_to_live": live["value"],
        "gate_time": gate["value"],
        "decided_nothing": decided_nothing,
        "first_pass": fleet["first_pass"]["value"],
        "usd_per_landing": cost["usd_per_landing"],
    });
    if live["covered"] == false {
        values["report_to_live"] = Value::Null;
        values["measuring_since_ms"] = live["measured_from_ms"].clone();
    }
    values
}

/// The window report to live is measured over: the Scorecard's default.
const REPORT_TO_LIVE_WINDOW_MS: i64 = 7 * DAY_MS;

/// `values` with report to live held back while the live record does not
/// cover its window: with the record starting at `first_deploy_ms` (the
/// first `deploy.finished`; `None` without one or when it cannot be read)
/// later than `now_ms` less the window, `report_to_live` is null and
/// `measuring_since_ms` the record's start (null without a record). Covered,
/// the values are as they were.
pub(super) fn measured(mut values: Value, first_deploy_ms: Option<i64>, now_ms: i64) -> Value {
    let covered = first_deploy_ms
        .is_some_and(|first| first <= now_ms.saturating_sub(REPORT_TO_LIVE_WINDOW_MS));
    if !covered {
        values["report_to_live"] = Value::Null;
        values["measuring_since_ms"] = first_deploy_ms.map_or(Value::Null, |first| json!(first));
    }
    values
}

/// `samples` (`host.core_lines`, `[{at_ms, values}]`) without a report to
/// live taken before the live record covered its window (rev-e F1 on
/// 7b1dd20a): a sample before `first_deploy_ms` plus the window holds the
/// record's start, not the wait (106 h, sampled live at 19:24), so its trend
/// never draws it. Without a record (`None`), none is kept. The other
/// figures stay as they are.
pub(super) fn measured_samples(mut samples: Value, first_deploy_ms: Option<i64>) -> Value {
    let covered_from = first_deploy_ms.map(|first| first.saturating_add(REPORT_TO_LIVE_WINDOW_MS));
    for sample in samples.as_array_mut().into_iter().flatten() {
        let at = sample["at_ms"].as_i64();
        let kept = matches!((at, covered_from), (Some(at), Some(from)) if at >= from);
        if !kept && let Some(values) = sample["values"].as_object_mut() {
            values.remove("performance.report_to_live");
        }
    }
    samples
}

/// The performance values and the 7-day cost window's `total`, kept for
/// [`PERFORMANCE_KEPT_MS`]: computed from the 7-day scorecard and cost
/// window as the pages compute them, report to live held back until the
/// live record covers the window ([`measured`]). A part that cannot be read
/// counts as not collected. The Scorecard's and the cost window's times go
/// in `parts` when they are computed, not when they are kept.
pub(super) fn performance_now(
    store: &spawn_postgres::PostgresStore,
    now_ms: i64,
    parts: &mut super::Parts,
) -> (Value, Value) {
    let (mut kept, held) = kept_now(now_ms, parts);
    if let Some(held) = held {
        return held;
    }
    parts.performance = Some("measured");
    let root = crate::env_value("SPAWN_WORKSPACE_ROOT");
    let scorecard = super::timed(&mut parts.archive, || {
        crate::scorecard::measure(
            store,
            root.as_deref().map(std::path::Path::new),
            &crate::scorecard::Query::default(),
        )
    })
    .unwrap_or(Value::Null);
    let cost = super::timed(&mut parts.cost, || {
        crate::cost::window_of(Some("7d"))
            .and_then(|(name, length_ms)| crate::cost::summary(store, name, length_ms))
    })
    .unwrap_or(Value::Null);
    let (first_deploy_ms, database_now_ms) = super::timed(&mut parts.archive, || {
        (
            store.first_deploy_finished_ms().ok().flatten(),
            store.database_now_ms().unwrap_or(now_ms),
        )
    });
    let values = measured(
        performance(&scorecard, &cost),
        first_deploy_ms,
        database_now_ms,
    );
    let cost_total = cost["total"].clone();
    *kept = Some((now_ms, values.clone(), cost_total.clone()));
    (values, cost_total)
}

/// The kept performance figures: when, the values and the cost total.
type Kept = Option<(i64, Value, Value)>;

/// The kept performance figures' lock, the wait for it timed as
/// `lock_wait` (behind another reader computing them, cc-astra A35), and
/// the figures kept within [`PERFORMANCE_KEPT_MS`] of `now_ms`, if any
/// (then marked `kept`).
fn kept_now(
    now_ms: i64,
    parts: &mut super::Parts,
) -> (std::sync::MutexGuard<'static, Kept>, Option<(Value, Value)>) {
    let kept = super::timed(&mut parts.lock_wait, || {
        PERFORMANCE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    });
    let held = kept
        .as_ref()
        .filter(|(at, _, _)| (0..PERFORMANCE_KEPT_MS).contains(&now_ms.saturating_sub(*at)))
        .map(|(_, values, cost_total)| (values.clone(), cost_total.clone()));
    if held.is_some() {
        parts.performance = Some("kept");
    }
    (kept, held)
}

/// The performance line ([`performance`]'s values).
pub(super) fn performance_line(performance: &Value) -> Line {
    let mut report_to_live = Figure::new(
        "performance.report_to_live",
        "Time from report to live",
        "ms",
        performance["report_to_live"].clone(),
    );
    report_to_live.measuring_since_ms = performance["measuring_since_ms"].clone();
    Line {
        name: "performance",
        north_star: report_to_live,
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
                // The Cost page's priced dollars only: usage on a model no
                // price names is not in it (cc-astra A11).
                "Dollars per landing (priced use only)",
                "usd",
                performance["usd_per_landing"].clone(),
            ),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;
    use serde_json::{Value, json};

    /// cc-astra A35: a reading that waits behind another holding the kept
    /// figures' lock says the wait as `lock_wait`, and the figures it then
    /// finds kept are marked `kept`. Lower bounds only.
    #[test]
    fn a_reading_behind_another_says_its_lock_wait_and_that_it_was_kept() {
        let (held, release) = (
            std::sync::mpsc::channel::<()>(),
            std::sync::mpsc::channel::<()>(),
        );
        let (locked, is_locked) = held;
        let (go, wait) = release;
        let now_ms = 1_000_000;
        let holder = std::thread::spawn(move || {
            let mut kept = PERFORMANCE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *kept = Some((now_ms, json!({"x": 1}), json!(2)));
            locked.send(()).expect("locked");
            let _ = wait.recv();
            std::thread::sleep(std::time::Duration::from_millis(200));
        });
        is_locked.recv().expect("the holder has the lock");
        go.send(()).expect("go");
        let mut parts = Parts::default();
        let (_guard, held) = kept_now(now_ms + 1, &mut parts);
        holder.join().expect("holder");
        assert_eq!(held, Some((json!({"x": 1}), json!(2))));
        assert_eq!(parts.performance, Some("kept"));
        let waited = parts.lock_wait.expect("timed");
        assert!(
            waited >= std::time::Duration::from_millis(150),
            "{waited:?}"
        );
        assert!(
            parts.text().contains(" performance=kept"),
            "{}",
            parts.text()
        );
    }

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
        let line = &lines(
            &json!({}),
            &values,
            &Observed::default(),
            Value::Null,
            Vec::new(),
        )[0];
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
        let line = &lines(
            &json!({}),
            &none,
            &Observed::default(),
            Value::Null,
            Vec::new(),
        )[0];
        assert!(
            std::iter::once(&line.north_star)
                .chain(&line.guardrails)
                .all(|figure| figure_json(figure, None)["state"] == "not_collected")
        );
        // Neither answer read: every value null.
        assert_eq!(performance(&Value::Null, &Value::Null), none);
        assert_eq!(
            lines(
                &json!({}),
                &values,
                &Observed::default(),
                Value::Null,
                Vec::new()
            )[0]
            .guardrails[3]
                .name,
            "Dollars per landing (priced use only)",
            "the Cost page's priced dollars only (cc-astra A11)"
        );
    }

    /// rev-e F1 on 7b1dd20a: a report to live sampled before the record
    /// covered the window (106 h, the record's start) never reaches its
    /// trend; one sampled after does, and the other figures are untouched.
    #[test]
    fn report_to_live_samples_before_the_record_covers_the_window_are_dropped() {
        const DAY: i64 = 86_400_000;
        let now = 200 * DAY + 12 * 3_600_000;
        let first = now - 8 * DAY;
        let sample = |at: i64, rtl: i64| {
            json!({"at_ms": at, "values": {"performance.report_to_live": rtl,
                                           "storage.free_bytes": 7}})
        };
        let samples = json!([
            sample(now - 2 * DAY, 380_287_112),
            sample(first + 7 * DAY - 1, 380_000_000),
            sample(first + 7 * DAY, 5_400_000),
            sample(now, 5_500_000),
        ]);
        let kept = trends(&measured_samples(samples.clone(), Some(first)), now);
        let rtl: Vec<Value> = kept["performance.report_to_live"]
            .iter()
            .map(|point| point["value"].clone())
            .collect();
        assert_eq!(rtl, [json!(5_400_000), json!(5_500_000)], "{kept:?}");
        assert_eq!(kept["storage.free_bytes"].len(), 3, "one a day, untouched");
        // No record: no report to live point at all.
        let none = trends(&measured_samples(samples.clone(), None), now);
        assert!(!none.contains_key("performance.report_to_live"), "{none:?}");
        assert_eq!(none["storage.free_bytes"].len(), 3);
        // The record only 2 days old: none either.
        let young = trends(&measured_samples(samples, Some(now - 2 * DAY)), now);
        assert!(
            !young.contains_key("performance.report_to_live"),
            "{young:?}"
        );
    }

    /// Objective 095's (2), by hand: report to live is held back, measuring
    /// since the first deploy record, until that record covers the 7 days.
    #[test]
    fn report_to_live_waits_until_the_live_record_covers_the_window() {
        const DAY: i64 = 86_400_000;
        let now = 100 * DAY;
        let values = json!({"report_to_live": 383_820_000, "gate_time": 840_000});
        // The record began 2 days ago: no value, measuring since then.
        let young = measured(values.clone(), Some(now - 2 * DAY), now);
        assert_eq!(young["report_to_live"], Value::Null);
        assert_eq!(young["measuring_since_ms"], json!(now - 2 * DAY));
        assert_eq!(
            young["gate_time"],
            json!(840_000),
            "the guardrails are untouched"
        );
        let line = &lines(
            &json!({}),
            &young,
            &Observed::default(),
            Value::Null,
            Vec::new(),
        )[0];
        assert_eq!(
            figure_json(&line.north_star, None),
            json!({"key": "performance.report_to_live", "name": "Time from report to live",
                   "unit": "ms", "value": null, "range": null, "state": "not_collected",
                   "measuring_since_ms": now - 2 * DAY})
        );
        assert!(
            !super::super::values_of(std::slice::from_ref(line))
                .contains_key("performance.report_to_live"),
            "no sample of it while measuring"
        );
        // Just short of 7 days: still measuring.
        let short = measured(values.clone(), Some(now - 7 * DAY + 1), now);
        assert_eq!(short["report_to_live"], Value::Null);
        // No deploy record at all: no value and no start.
        let none = measured(values.clone(), None, now);
        assert_eq!(none["report_to_live"], Value::Null);
        assert_eq!(none["measuring_since_ms"], Value::Null);
        let star = &lines(
            &json!({}),
            &none,
            &Observed::default(),
            Value::Null,
            Vec::new(),
        )[0]
        .north_star;
        assert!(figure_json(star, None).get("measuring_since_ms").is_none());
        // Covered (exactly 7 days, and more): the Scorecard's value, no start.
        for first in [now - 7 * DAY, now - 30 * DAY] {
            let covered = measured(values.clone(), Some(first), now);
            assert_eq!(covered, values, "covered from {first}");
            let star = &lines(
                &json!({}),
                &covered,
                &Observed::default(),
                Value::Null,
                Vec::new(),
            )[0]
            .north_star;
            let shown = figure_json(star, None);
            assert_eq!(shown["value"], json!(383_820_000));
            assert!(shown.get("measuring_since_ms").is_none());
        }
    }

    /// Objective 095: the north star while the Scorecard's 7 days are not
    /// covered by the live record has no value and says where the measuring
    /// began; once covered it is the Scorecard's value, with no such field.
    #[test]
    fn the_performance_north_star_measures_since_the_record_until_covered() {
        let card = |covered: bool, from: Value| {
            json!({"scopes": [{"scope": "fleet", "dimensions": {
                "report_to_live": {"value": 3_600_000, "n": 3, "covered": covered,
                                   "measured_from_ms": from},
                "gate_time": {"value": null}, "first_pass": {"value": null}}}]})
        };
        let star = |scorecard: &Value| {
            let values = performance(scorecard, &Value::Null);
            let line = &lines(
                &json!({}),
                &values,
                &Observed::default(),
                Value::Null,
                Vec::new(),
            )[0];
            figure_json(&line.north_star, None)
        };
        let waiting = star(&card(false, json!(1_700_000_000_000_i64)));
        assert_eq!(waiting["key"], "performance.report_to_live");
        assert_eq!(waiting["value"], Value::Null);
        assert_eq!(waiting["state"], "not_collected");
        assert_eq!(waiting["measuring_since_ms"], json!(1_700_000_000_000_i64));
        let covered = star(&card(true, json!(1_700_000_000_000_i64)));
        assert_eq!(covered["value"], json!(3_600_000));
        assert_eq!(covered["state"], "no_range");
        assert_eq!(covered.get("measuring_since_ms"), None, "{covered}");
        // With no deploy record: nothing measured, nothing to say since when.
        let never = star(&card(false, Value::Null));
        assert_eq!(never["value"], Value::Null);
        assert_eq!(never.get("measuring_since_ms"), None, "{never}");
    }
}
