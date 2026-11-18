//! Days until the disk is full from the disk itself (objective 141).
//!
//! The storage line's north star was the free disk over the growth of the
//! stores that persist (objective 085); builds, reviewer scratch and
//! workspaces are no store, so it said months where the disk had days. It
//! is now the free disk over the disk's own fall: the least-squares line
//! through its daily readings.
//!
//! - The readings: for each UTC day of the trend's window ([`trend_since`],
//!   today and the 6 before) before today, that day's last
//!   `host.core_lines` sample carrying a number for `storage.free_bytes`;
//!   then the free disk now at now. Today's samples are left out: the
//!   reading now stands for today.
//! - The fall ([`fall`]): the negated slope of that line, in bytes a day,
//!   rounded; below zero when the disk gains; none until the readings span
//!   a day.
//! - `days_left`: the free disk over the fall, rounded down; none when
//!   either is unknown or the disk does not fall. While the fall has none,
//!   the figure says `measuring_since_ms`, a day after the first reading.
//! - The store that grew most ([`grew_most`]): of health.storage's
//!   `growth_parts` that have a value and `unattributed` (the fall less
//!   their sum), the one that grew most; a tie goes to the one named first.
//!   It is judged on the fit's fall, the week's: the growth parts grow over
//!   the same week, and a day's fall less a week's growth would call a
//!   store's sudden day unattributed.
//!
//! The fall said is the worse of the fit's and the last day's (objective
//! 222, [`worse`]), with its basis (`fit` or `day`). A disk swept by hand
//! rises and falls in steps: on 2026-11-18 df's free disk fell 77 GB in a
//! day while the week's sawtooth (sweeps lifting it between) fitted a line
//! that gained 4.7 GB a day, and a robust line reads the week's gain too.
//! The day's fall ([`day_fall`]) is from the latest sample a day old or
//! more, no older than 36 h, to the free disk now, over the days between:
//! from the same samples, no read of its own.

use serde_json::{Value, json};

use super::DAY_MS;
use super::trends::trend_since;

/// The figure the readings are drawn from.
const FREE: &str = "storage.free_bytes";

/// The named stores, in the order a tie is settled by: health.storage's
/// `growth_parts`, then what none of them accounts for.
const STORES: [&str; 4] = ["archive", "projects", "database", "session_backups"];

/// What no named store accounts for.
const UNATTRIBUTED: &str = "unattributed";

/// The readings `(at_ms, free bytes)`, oldest first: each UTC day of the
/// window before today's, its last sample (`[{at_ms, values}]`, as the
/// store reads them) carrying a number for the free disk, then `free_now`
/// at `now_ms` (none when it is unknown).
pub(super) fn readings(samples: &Value, free_now: Option<u64>, now_ms: i64) -> Vec<(i64, f64)> {
    let since = trend_since(now_ms);
    let today = now_ms.div_euclid(DAY_MS) * DAY_MS;
    // day -> (at_ms, bytes); a later sample replaces an earlier.
    let mut days = std::collections::BTreeMap::<i64, (i64, f64)>::new();
    for sample in samples.as_array().into_iter().flatten() {
        let (Some(at), Some(free)) = (sample["at_ms"].as_i64(), sample["values"][FREE].as_f64())
        else {
            continue;
        };
        if at < since || at >= today {
            continue;
        }
        let day = at.div_euclid(DAY_MS);
        match days.get(&day) {
            Some((kept, _)) if *kept > at => {}
            _ => {
                days.insert(day, (at, free));
            }
        }
    }
    let mut readings: Vec<(i64, f64)> = days.into_values().collect();
    if let Some(free) = free_now {
        #[allow(clippy::cast_precision_loss)]
        readings.push((now_ms, free as f64));
    }
    readings
}

/// The fall of `readings` (oldest first): the negated slope of their
/// least-squares line (time in days, bytes), rounded, in bytes a day;
/// below zero when the disk gains. None until they span a day.
pub(super) fn fall(readings: &[(i64, f64)]) -> Option<i64> {
    let first = readings.iter().map(|(at, _)| *at).min()?;
    let last = readings.iter().map(|(at, _)| *at).max()?;
    if last - first < DAY_MS {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    let points: Vec<(f64, f64)> = readings
        .iter()
        .map(|(at, bytes)| ((at - first) as f64 / DAY_MS as f64, *bytes))
        .collect();
    #[allow(clippy::cast_precision_loss)]
    let count = points.len() as f64;
    let mean_x = points.iter().map(|(x, _)| x).sum::<f64>() / count;
    let mean_y = points.iter().map(|(_, y)| y).sum::<f64>() / count;
    let (mut across, mut spread) = (0.0, 0.0);
    for (x, y) in &points {
        across += (x - mean_x) * (y - mean_y);
        spread += (x - mean_x) * (x - mean_x);
    }
    if spread <= 0.0 {
        return None;
    }
    let slope = across / spread;
    // A disk's bytes a day fit an i64 many times over.
    #[allow(clippy::cast_possible_truncation)]
    Some((-slope).round() as i64)
}

/// The oldest a day-old sample the day's fall is read from may be.
const DAY_FALL_STALE_MS: i64 = 36 * 3_600_000;

/// The free disk lost over the last day, in bytes a day, rounded; below
/// zero when the disk gained. From the latest of `samples` carrying a
/// number for the free disk at or before `now_ms` less a day, when it is at
/// or after `now_ms` less 36 h, to `free_now` at `now_ms`, over the days
/// between them. None without such a sample or a free disk now.
pub(super) fn day_fall(samples: &Value, free_now: Option<u64>, now_ms: i64) -> Option<i64> {
    let free_now = free_now?;
    let (at, then) = samples
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|sample| Some((sample["at_ms"].as_i64()?, sample["values"][FREE].as_f64()?)))
        .filter(|(at, _)| *at <= now_ms - DAY_MS && *at >= now_ms - DAY_FALL_STALE_MS)
        .max_by_key(|(at, _)| *at)?;
    #[allow(clippy::cast_precision_loss)]
    let days = (now_ms - at) as f64 / DAY_MS as f64;
    #[allow(clippy::cast_precision_loss)]
    let lost = then - free_now as f64;
    // A disk's bytes a day fit an i64 many times over.
    #[allow(clippy::cast_possible_truncation)]
    Some((lost / days).round() as i64)
}

/// The worse of the fit's fall and the day's, with its basis (`fit` or
/// `day`): the larger; a tie is the fit's; either alone when the other is
/// none; none when both are.
pub(super) fn worse(fit: Option<i64>, day: Option<i64>) -> Option<(i64, &'static str)> {
    match (fit, day) {
        (Some(fit), Some(day)) if day > fit => Some((day, "day")),
        (Some(fit), _) => Some((fit, "fit")),
        (None, Some(day)) => Some((day, "day")),
        (None, None) => None,
    }
}

/// How many days `free` lasts at `fall` a day, rounded down; none when
/// either is unknown or the disk does not fall.
pub(super) fn days_left(free: Option<u64>, fall: Option<i64>) -> Option<u64> {
    let fall = u64::try_from(fall?).ok().filter(|fall| *fall > 0)?;
    Some(free? / fall)
}

/// The store that grew most and its bytes a day: of `parts`
/// (health.storage's `growth_parts`) those with a value, and what no
/// named store accounts for (`fall` less their sum). A tie goes to the
/// store named first. None without a fall.
pub(super) fn grew_most(parts: &Value, fall: Option<i64>) -> Option<(&'static str, i64)> {
    let fall = fall?;
    let named: Vec<(&'static str, i64)> = STORES
        .iter()
        .filter_map(|store| Some((*store, parts[*store].as_i64()?)))
        .collect();
    let accounted = named
        .iter()
        .fold(0_i64, |sum, (_, grew)| sum.saturating_add(*grew));
    let mut most: Option<(&'static str, i64)> = None;
    for (store, grew) in named.into_iter().chain(std::iter::once((
        UNATTRIBUTED,
        fall.saturating_sub(accounted),
    ))) {
        if most.is_none_or(|(_, kept)| grew > kept) {
            most = Some((store, grew));
        }
    }
    most
}

/// `storage` (health.storage as the line reads it) with the disk's
/// figures from `samples` (the window's `host.core_lines` samples) and its
/// own `free_bytes` now, at `now_ms`: `disk_fall_bytes_per_day`, the worse
/// of the fit's and the day's ([`worse`]), and its `disk_fall_basis`;
/// `days_left` from that fall (and `days_left_measuring_since_ms` while it
/// has no value); `grew_most` (`{store, bytes_per_day}`) from the fit's.
pub(super) fn with_disk(storage: &mut Value, samples: &Value, now_ms: i64) {
    let free = storage["free_bytes"].as_u64();
    let readings = readings(samples, free, now_ms);
    let fit = fall(&readings);
    let said = worse(fit, day_fall(samples, free, now_ms));
    let fall = said.map(|(fall, _)| fall);
    let measuring_since = match fall {
        Some(_) => None,
        None => readings.first().map(|(at, _)| at + DAY_MS),
    };
    let most = grew_most(&storage["growth_parts"], fit);
    storage["days_left"] = json!(days_left(free, fall));
    storage["days_left_measuring_since_ms"] = json!(measuring_since);
    storage["disk_fall_bytes_per_day"] = json!(fall);
    storage["disk_fall_basis"] = json!(said.map(|(_, basis)| basis));
    storage["grew_most"] = most.map_or(
        Value::Null,
        |(store, grew)| json!({"store": store, "bytes_per_day": grew}),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: i64 = DAY_MS;
    const H: i64 = 3_600_000;
    const GIB: u64 = 1 << 30;
    // 2026-11-14, 02:39 UTC.
    const MIDNIGHT: i64 = 20_771 * D;
    const NOW: i64 = MIDNIGHT + 2 * H + 39 * 60_000;

    fn sample(at_ms: i64, free: &Value) -> Value {
        json!({"at_ms": at_ms, "values": {FREE: free, "storage.days_left": 9}})
    }

    #[test]
    fn the_readings_are_each_earlier_days_last_sample_and_now() {
        let samples = json!([
            // Outside the window: the 7th day before today.
            sample(MIDNIGHT - 7 * D + 23 * H, &json!(1)),
            sample(MIDNIGHT - 6 * D + H, &json!(600)),
            sample(MIDNIGHT - 6 * D + 5 * H, &json!(590)),
            // The day's last sample without the figure: the one before it.
            sample(MIDNIGHT - 3 * D + H, &json!(300)),
            json!({"at_ms": MIDNIGHT - 3 * D + 9 * H, "values": {"storage.days_left": 4}}),
            sample(MIDNIGHT - 2 * D + 9 * H, &Value::Null),
            sample(MIDNIGHT - D + 23 * H, &json!(120)),
            // Today's: the reading now stands for it.
            sample(MIDNIGHT + H, &json!(50)),
        ]);
        assert_eq!(
            readings(&samples, Some(100), NOW),
            [
                (MIDNIGHT - 6 * D + 5 * H, 590.0),
                (MIDNIGHT - 3 * D + H, 300.0),
                (MIDNIGHT - D + 23 * H, 120.0),
                (NOW, 100.0),
            ]
        );
        assert_eq!(
            readings(&samples, None, NOW).len(),
            3,
            "no free disk now: no reading now"
        );
        assert_eq!(readings(&json!([]), Some(7), NOW), [(NOW, 7.0)]);
    }

    #[test]
    fn the_fall_is_the_fits_and_has_no_value_under_a_day() {
        // Exactly on a line falling 10 GiB a day.
        #[allow(clippy::cast_precision_loss)]
        let free = |at: i64| (100 * GIB) as f64 + (10 * GIB) as f64 * (NOW - at) as f64 / D as f64;
        let on_line: Vec<(i64, f64)> = [MIDNIGHT - 6 * D + 2 * H, MIDNIGHT - 3 * D, NOW]
            .into_iter()
            .map(|at| (at, free(at)))
            .collect();
        let fall_on_line = fall(&on_line).expect("a fall");
        assert!(
            (fall_on_line - i64::try_from(10 * GIB).expect("fits")).abs() <= 1,
            "{fall_on_line}"
        );
        // The least-squares line, not the first and last: 0, 2 and 0 a day
        // apart gives a slope of 0; 10, 4, 4 over days 0, 1, 2: -3.
        assert_eq!(fall(&[(0, 0.0), (D, 2.0), (2 * D, 0.0)]), Some(0));
        assert_eq!(fall(&[(0, 10.0), (D, 4.0), (2 * D, 4.0)]), Some(3));
        // A disk that gains: below zero.
        assert_eq!(fall(&[(0, 100.0), (2 * D, 150.0)]), Some(-25));
        // Under a day of span: none.
        assert_eq!(fall(&[(0, 100.0), (D - 1, 10.0)]), None);
        assert_eq!(fall(&[(0, 100.0)]), None);
        assert_eq!(fall(&[]), None);
        assert_eq!(
            fall(&[(0, 100.0), (D, 90.0)]),
            Some(10),
            "a day exactly is enough"
        );
    }

    #[test]
    fn days_left_are_rounded_down_and_none_without_a_fall() {
        assert_eq!(days_left(Some(29), Some(10)), Some(2));
        assert_eq!(days_left(Some(30), Some(10)), Some(3));
        assert_eq!(days_left(Some(30), Some(0)), None);
        assert_eq!(days_left(Some(30), Some(-4)), None, "a disk that gains");
        assert_eq!(days_left(Some(30), None), None);
        assert_eq!(days_left(None, Some(10)), None);
    }

    #[test]
    fn the_store_that_grew_most_counts_what_no_store_accounts_for() {
        let parts = json!({"archive": 30, "projects": 5, "database": null, "session_backups": 10});
        // 100 a day: 45 named, 55 unattributed.
        assert_eq!(grew_most(&parts, Some(100)), Some(("unattributed", 55)));
        // 60 a day: 15 unattributed, the archive grew most.
        assert_eq!(grew_most(&parts, Some(60)), Some(("archive", 30)));
        // A tie goes to the store named first.
        let tied = json!({"archive": 5, "projects": 20, "database": 20});
        assert_eq!(grew_most(&tied, Some(45)), Some(("projects", 20)));
        assert_eq!(
            grew_most(&tied, Some(65)),
            Some(("projects", 20)),
            "a tie with unattributed"
        );
        // No store measured: all of the fall.
        assert_eq!(grew_most(&Value::Null, Some(7)), Some(("unattributed", 7)));
        assert_eq!(grew_most(&parts, None), None, "no fall: none");
    }

    #[test]
    fn with_disk_sets_the_days_from_the_fit_and_the_measuring_start_while_measuring() {
        // The stores' growth says 136 days; the disk falls 10 GiB a day.
        let mut storage = json!({
            "free_bytes": 79 * GIB, "days_left": 136,
            "growth_parts": {"archive": 1000, "projects": 2000, "database": null},
        });
        let samples = json!([
            sample(MIDNIGHT - 2 * D + 12 * H, &json!(99 * GIB)),
            sample(MIDNIGHT - D + 12 * H, &json!(89 * GIB)),
        ]);
        let now = MIDNIGHT + 12 * H;
        with_disk(&mut storage, &samples, now);
        let ten = 10 * GIB;
        assert_eq!(storage["disk_fall_bytes_per_day"], ten);
        assert_eq!(storage["days_left"], 7, "79 / 10, rounded down");
        assert_eq!(storage["days_left_measuring_since_ms"], Value::Null);
        assert_eq!(
            storage["grew_most"],
            json!({"store": "unattributed", "bytes_per_day": ten - 3000})
        );

        // Only the reading now: measuring, from a day after it.
        let mut storage = json!({"free_bytes": 79 * GIB, "days_left": 136});
        with_disk(&mut storage, &json!([]), now);
        assert_eq!(storage["days_left"], Value::Null);
        assert_eq!(storage["days_left_measuring_since_ms"], now + D);
        assert_eq!(storage["disk_fall_bytes_per_day"], Value::Null);
        assert_eq!(storage["grew_most"], Value::Null);

        // Measuring from the first reading, a day before the fall.
        let mut storage = json!({"free_bytes": 79 * GIB});
        let first = MIDNIGHT - D + 20 * H;
        with_disk(&mut storage, &json!([sample(first, &json!(80 * GIB))]), now);
        assert_eq!(storage["days_left_measuring_since_ms"], first + D);
    }

    #[test]
    fn the_days_fall_is_from_the_latest_sample_a_day_old_not_the_oldest() {
        let samples = json!([
            sample(NOW - 35 * H, &json!(500 * GIB)),
            sample(NOW - 30 * H, &json!(400 * GIB)),
            // The latest a day old: 100 GiB lost over the day.
            sample(NOW - D, &json!(200 * GIB)),
            // Under a day old: not taken.
            sample(NOW - D + 1, &json!(900 * GIB)),
            sample(NOW - H, &json!(105 * GIB)),
        ]);
        assert_eq!(
            day_fall(&samples, Some(100 * GIB), NOW),
            Some(i64::try_from(100 * GIB).expect("fits"))
        );
        // Scaled by the days between: 30 h old, 90 GiB lost.
        let samples = json!([sample(NOW - 30 * H, &json!(190 * GIB))]);
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
        let scaled = ((90 * GIB) as f64 / 1.25).round() as i64;
        assert_eq!(day_fall(&samples, Some(100 * GIB), NOW), Some(scaled));
    }

    #[test]
    fn the_days_fall_takes_36_hours_exactly_and_nothing_older() {
        let taken = json!([sample(NOW - 36 * H, &json!(250))]);
        assert_eq!(
            day_fall(&taken, Some(100), NOW),
            Some(100),
            "150 over 1.5 days"
        );
        let older = json!([sample(NOW - 36 * H - 1, &json!(250))]);
        assert_eq!(
            day_fall(&older, Some(100), NOW),
            None,
            "a millisecond older"
        );
    }

    #[test]
    fn the_days_fall_takes_no_sample_under_a_day_old() {
        let samples = json!([
            sample(NOW - D + 1, &json!(250)),
            sample(NOW - H, &json!(200)),
        ]);
        assert_eq!(day_fall(&samples, Some(100), NOW), None);
    }

    #[test]
    fn the_days_fall_is_none_without_a_sample_or_a_free_disk_now() {
        let samples = json!([sample(NOW - D, &json!(200))]);
        assert_eq!(day_fall(&samples, None, NOW), None, "no free disk now");
        assert_eq!(day_fall(&json!([]), Some(100), NOW), None, "no sample");
        // A sample without a number for the free disk is no sample.
        let without = json!([
            sample(NOW - D, &Value::Null),
            json!({"at_ms": NOW - D - H, "values": {"storage.days_left": 4}}),
        ]);
        assert_eq!(day_fall(&without, Some(100), NOW), None);
    }

    #[test]
    fn a_day_that_gained_falls_below_zero() {
        let samples = json!([sample(NOW - D, &json!(100))]);
        assert_eq!(day_fall(&samples, Some(130), NOW), Some(-30));
    }

    #[test]
    fn the_worse_of_the_fit_and_the_day_with_its_basis() {
        assert_eq!(
            worse(Some(10), Some(80)),
            Some((80, "day")),
            "the day larger"
        );
        assert_eq!(
            worse(Some(80), Some(10)),
            Some((80, "fit")),
            "the fit larger"
        );
        assert_eq!(
            worse(Some(40), Some(40)),
            Some((40, "fit")),
            "a tie is the fit's"
        );
        assert_eq!(
            worse(Some(-5), Some(-20)),
            Some((-5, "fit")),
            "both gaining"
        );
        assert_eq!(
            worse(Some(-20), Some(-5)),
            Some((-5, "day")),
            "both gaining"
        );
        assert_eq!(worse(Some(-3), None), Some((-3, "fit")), "the fit alone");
        assert_eq!(worse(None, Some(-3)), Some((-3, "day")), "the day alone");
        assert_eq!(worse(None, None), None);
    }

    /// The week of 2026-11-18: each UTC day's last sample (hourly, at
    /// hh:54:59 UTC) and df's reading a day before now (179.0 GB at
    /// 14:54:59 +08), now 101.9 GB at 14:55:50 +08. Bytes that round to
    /// the GB said.
    fn replay_of_2026_11_18(day_old_free: u64) -> (Value, i64) {
        let last = 23 * H + 54 * 60_000 + 59_000;
        let now = MIDNIGHT + 6 * H + 55 * 60_000 + 50_000;
        let week: [u64; 6] = [
            129_140_000_000,
            61_630_000_000,
            163_710_000_000,
            158_590_000_000,
            171_370_000_000,
            144_946_804_614,
        ];
        let mut samples: Vec<Value> = (0_i64..)
            .zip(week)
            .map(|(day, free)| sample(MIDNIGHT - (6 - day) * D + last, &json!(free)))
            .collect();
        samples.push(sample(now - D - 51_000, &json!(day_old_free)));
        samples.push(sample(
            now - D + 59 * 60_000 + 9_000,
            &json!(170_000_000_000_u64),
        ));
        (Value::Array(samples), now)
    }

    #[test]
    fn the_replay_of_2026_11_18_says_the_days_fall() {
        let free_now = 101_900_000_000_u64;
        let (samples, now) = replay_of_2026_11_18(179_049_896_704);
        let fit = fall(&readings(&samples, Some(free_now), now));
        assert_eq!(fit, Some(-4_739_443_506), "the fit: gaining");
        let day = day_fall(&samples, Some(free_now), now);
        assert_eq!(day, Some(77_104_383_700), "the day: 77 GB lost");
        let mut storage = json!({"free_bytes": free_now});
        with_disk(&mut storage, &samples, now);
        assert_eq!(storage["disk_fall_bytes_per_day"], 77_104_383_700_i64);
        assert_eq!(storage["disk_fall_basis"], "day");
        assert_eq!(storage["days_left"], 1);
        assert_eq!(storage["days_left_measuring_since_ms"], Value::Null);

        // The same week with a day that gained 10 GB: the fit's.
        let (samples, now) = replay_of_2026_11_18(free_now - 10_000_000_000);
        assert!(day_fall(&samples, Some(free_now), now).expect("a day") < 0);
        let mut storage = json!({"free_bytes": free_now});
        with_disk(&mut storage, &samples, now);
        assert_eq!(storage["disk_fall_bytes_per_day"], -4_739_443_506_i64);
        assert_eq!(storage["disk_fall_basis"], "fit");
        assert_eq!(storage["days_left"], Value::Null, "gaining: no days");
    }

    #[test]
    fn a_days_fall_with_no_fit_is_said() {
        // Both samples yesterday's: the readings span under a day.
        let samples = json!([
            sample(NOW - D - 60_000, &json!(125 * GIB)),
            sample(MIDNIGHT - 60_000, &json!(110 * GIB)),
        ]);
        let free = 100 * GIB;
        assert_eq!(fall(&readings(&samples, Some(free), NOW)), None);
        let mut storage = json!({"free_bytes": free});
        with_disk(&mut storage, &samples, NOW);
        let day = day_fall(&samples, Some(free), NOW).expect("a day");
        assert!((day - 25 * i64::try_from(GIB).expect("fits")).abs() < 20_000_000);
        assert_eq!(storage["disk_fall_bytes_per_day"], day);
        assert_eq!(storage["disk_fall_basis"], "day");
        assert_eq!(storage["days_left"], 4);
        assert_eq!(storage["days_left_measuring_since_ms"], Value::Null);
        assert_eq!(storage["grew_most"], Value::Null, "no fit: none");
    }

    #[test]
    fn the_store_that_grew_most_is_judged_over_the_week_not_the_day() {
        // A flat week at 100 GiB, then 20 GiB into the archive in the last
        // day; the archive's growth over the week, a seventh of it.
        let archive = 20 * GIB / 7;
        let mut samples: Vec<Value> = (1..=6)
            .map(|days| sample(MIDNIGHT - days * D + 12 * H, &json!(100 * GIB)))
            .collect();
        samples.push(sample(NOW - D, &json!(100 * GIB)));
        let samples = Value::Array(samples);
        let free = 80 * GIB;
        let mut storage = json!({"free_bytes": free, "growth_parts": {"archive": archive}});
        with_disk(&mut storage, &samples, NOW);
        assert_eq!(storage["disk_fall_basis"], "day");
        assert_eq!(storage["disk_fall_bytes_per_day"], 20 * GIB);
        let fit = fall(&readings(&samples, Some(free), NOW)).expect("a fit");
        let (store, grew) = grew_most(&storage["growth_parts"], Some(fit)).expect("a store");
        assert_eq!(
            storage["grew_most"],
            json!({"store": store, "bytes_per_day": grew})
        );
        // The day's would call the archive's day unattributed.
        let unattributed = i64::try_from(20 * GIB - archive).expect("fits");
        assert_ne!(storage["grew_most"]["bytes_per_day"], unattributed);
        assert!(grew < unattributed, "{grew}");
    }
}
