//! Whether a figure has read 0 for a week (objective 182): leaked test
//! schemas "stays visible until it has read 0 for 7 days, then an alert
//! only". A week at 0 is seen, never assumed: the value now is 0, and the
//! hourly `host.core_lines` samples cover the whole week, each carrying the
//! figure as the number 0. A missing sample is not a zero, nor is one that
//! does not carry the figure as a number.

use serde_json::Value;

use super::DAY_MS;

/// The week a figure must have read 0 over.
const WEEK_MS: i64 = 7 * DAY_MS;

/// The longest the samples may leave uncovered: from the week's start to
/// the oldest, between two of them, and from the newest to now. The loop
/// samples hourly, so two hours allow one late sample, never a missing one.
const GAP_MS: i64 = 2 * 3_600_000;

/// Where the samples the answer, the guardrail step's reading and the
/// hourly sample read start at `now_ms`: a week back, so
/// [`zero_for_a_week`] sees the whole week (the trends and the disk's
/// readings filter their own, shorter windows).
pub(crate) fn samples_since(now_ms: i64) -> i64 {
    now_ms - WEEK_MS
}

/// Whether the figure `key` has read 0 for a week at `now_ms`: its value
/// `now` is 0, and the `host.core_lines` `samples` (`[{at_ms, values}]`)
/// since `now_ms` − 7 × 24 h each carry it as 0, the oldest within
/// [`GAP_MS`] of the week's start, no two more than [`GAP_MS`] apart and
/// the newest within [`GAP_MS`] of now. No sample is no week.
pub(super) fn zero_for_a_week(key: &str, now: &Value, samples: &Value, now_ms: i64) -> bool {
    if now.as_f64() != Some(0.0) {
        return false;
    }
    let since = samples_since(now_ms);
    let mut week: Vec<(i64, bool)> = samples
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|sample| {
            let at = sample["at_ms"].as_i64()?;
            (since..=now_ms)
                .contains(&at)
                .then(|| (at, sample["values"][key].as_f64() == Some(0.0)))
        })
        .collect();
    if week.is_empty() || week.iter().any(|(_, zero)| !zero) {
        return false;
    }
    week.sort_unstable();
    let mut covered = since;
    for (at, _) in week {
        if at - covered > GAP_MS {
            return false;
        }
        covered = at;
    }
    now_ms - covered <= GAP_MS
}

#[cfg(test)]
mod tests {
    use super::{GAP_MS, WEEK_MS, zero_for_a_week};
    use serde_json::{Value, json};

    const KEY: &str = "storage.leaked_test_schemas";
    const H: i64 = 3_600_000;
    const NOW: i64 = 1_800_000_000_000;

    /// Hourly samples from `first` ms ago down to now (`NOW - at` for each
    /// in the hour steps), each with `value(ago)`'s values (`None`: the
    /// sample is missing).
    fn hourly(first: i64, value: impl Fn(i64) -> Option<Value>) -> Value {
        let mut samples = Vec::new();
        let mut ago = first;
        while ago >= 0 {
            if let Some(values) = value(ago) {
                samples.push(json!({"at_ms": NOW - ago, "values": values}));
            }
            ago -= H;
        }
        Value::Array(samples)
    }

    fn zeros(_: i64) -> Option<Value> {
        Some(json!({KEY: 0}))
    }

    /// Objective 182: leaked test schemas has read 0 for a week only when it
    /// reads 0 now and hourly samples cover the whole week, each carrying it
    /// as 0; a missing sample, a sample without it, a gap at the week's
    /// start, inside it or before now, or a value now other than 0 is no
    /// week.
    #[test]
    fn leaked_schemas_read_zero_for_a_week_only_with_samples_across_it() {
        let zero = json!(0);
        let week = WEEK_MS + 2 * H;
        let full = hourly(week, zeros);
        assert!(zero_for_a_week(KEY, &zero, &full, NOW), "a full week of zeros");

        // Older non-zero samples outside the week do not count.
        let older = hourly(10 * 24 * H, |ago| {
            Some(json!({KEY: if ago > WEEK_MS { 5 } else { 0 }}))
        });
        assert!(zero_for_a_week(KEY, &zero, &older, NOW), "outside the week");

        // The value now must be 0.
        assert!(!zero_for_a_week(KEY, &json!(1), &full, NOW), "1 now");
        assert!(!zero_for_a_week(KEY, &Value::Null, &full, NOW), "null now");

        // One sample in the week at 3, null, or without the figure.
        for odd in [json!({KEY: 3}), json!({KEY: null}), json!({"storage.free_bytes": 1})] {
            let samples = hourly(week, |ago| {
                Some(if ago == 3 * 24 * H { odd.clone() } else { json!({KEY: 0}) })
            });
            assert!(!zero_for_a_week(KEY, &zero, &samples, NOW), "{odd}");
        }

        // A gap at the week's start: the first sample 3 h after it.
        let late = hourly(WEEK_MS - 3 * H, zeros);
        assert!(!zero_for_a_week(KEY, &zero, &late, NOW), "a late start");
        // Inside it: three hours missing four days ago.
        let gap = hourly(week, |ago| {
            (!(4 * 24 * H..4 * 24 * H + 3 * H).contains(&ago)).then(|| json!({KEY: 0}))
        });
        assert!(!zero_for_a_week(KEY, &zero, &gap, NOW), "a gap inside");
        // Before now: none in the last three hours.
        let stopped = hourly(week, |ago| (ago > 3 * H).then(|| json!({KEY: 0})));
        assert!(!zero_for_a_week(KEY, &zero, &stopped, NOW), "stopped");

        // No samples.
        assert!(!zero_for_a_week(KEY, &zero, &json!([]), NOW), "none");
        assert!(!zero_for_a_week(KEY, &zero, &Value::Null, NOW), "no list");

        // Samples two hours apart cover the week; just over two is a gap.
        let two = hourly(week, |ago| (ago % GAP_MS == 0).then(|| json!({KEY: 0})));
        assert!(zero_for_a_week(KEY, &zero, &two, NOW), "two hours apart");
        let three = hourly(week, |ago| (ago % (3 * H) == 0).then(|| json!({KEY: 0})));
        assert!(!zero_for_a_week(KEY, &zero, &three, NOW), "three hours apart");
    }
}
