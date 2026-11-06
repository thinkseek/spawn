//! Times of the scorecard: RFC 3339 instants and spans back in time, read
//! and written without a date library, and UTC days.

/// One day in milliseconds.
pub(crate) const DAY_MS: i64 = 86_400_000;

/// One hour in milliseconds.
pub(crate) const HOUR_MS: i64 = 3_600_000;

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The date of a day since 1970-01-01 (`civil_from_days`).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// A run of exactly `width` ASCII digits at the start of `text`, and the rest.
fn digits(text: &str, width: usize) -> Option<(i64, &str)> {
    let head = text.get(..width)?;
    if !head.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some((head.parse().ok()?, &text[width..]))
}

/// The epoch milliseconds of an RFC 3339 time (`2026-10-02T00:00:00Z`,
/// `2026-10-02T02:00:00.123+02:00`, `T` or a space between date and time),
/// or of a full date alone (`2026-10-02`, its UTC midnight). A fraction
/// finer than a millisecond is cut.
pub(crate) fn parse_rfc3339(text: &str) -> Option<i64> {
    let (year, rest) = digits(text, 4)?;
    let (month, rest) = digits(rest.strip_prefix('-')?, 2)?;
    let (day, rest) = digits(rest.strip_prefix('-')?, 2)?;
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return None;
    }
    let date_ms = days_from_civil(year, month, day) * DAY_MS;
    if rest.is_empty() {
        return Some(date_ms);
    }
    let rest = rest
        .strip_prefix('T')
        .or_else(|| rest.strip_prefix('t'))
        .or_else(|| rest.strip_prefix(' '))?;
    let (hour, rest) = digits(rest, 2)?;
    let (minute, rest) = digits(rest.strip_prefix(':')?, 2)?;
    let (second, mut rest) = digits(rest.strip_prefix(':')?, 2)?;
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut millis = 0;
    if let Some(fraction) = rest.strip_prefix('.') {
        let length = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if length == 0 {
            return None;
        }
        let mut scale = 100;
        for byte in fraction.bytes().take(3) {
            millis += i64::from(byte - b'0') * scale;
            scale /= 10;
        }
        rest = &fraction[length..];
    }
    let offset_minutes = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let (offset_hour, tail) = digits(&rest[1..], 2)?;
            let (offset_minute, tail) = digits(tail.strip_prefix(':')?, 2)?;
            if !tail.is_empty() || offset_hour > 23 || offset_minute > 59 {
                return None;
            }
            sign * (offset_hour * 60 + offset_minute)
        }
    };
    // A leap second is read as the last second of its minute.
    let second = second.min(59);
    Some(
        date_ms + ((hour * 60 + minute - offset_minutes) * 60 + second) * 1000 + millis,
    )
}

/// A span back in time (`90m`, `36h`, `7d`, `2w`, `30s`), in milliseconds.
pub(crate) fn parse_span(text: &str) -> Option<i64> {
    let unit = text.chars().last()?;
    let count: i64 = text[..text.len() - unit.len_utf8()].parse().ok()?;
    if count <= 0 || !text[..text.len() - unit.len_utf8()]
        .bytes()
        .all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let unit_ms = match unit {
        's' => 1000,
        'm' => 60_000,
        'h' => HOUR_MS,
        'd' => DAY_MS,
        'w' => 7 * DAY_MS,
        _ => return None,
    };
    count.checked_mul(unit_ms)
}

/// `ms` as RFC 3339 in UTC: `2026-10-02T00:00:00Z`, with `.mmm` when it is
/// not a whole second.
pub(crate) fn format_rfc3339(ms: i64) -> String {
    let days = ms.div_euclid(DAY_MS);
    let in_day = ms.rem_euclid(DAY_MS);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second, millis) = (
        in_day / HOUR_MS,
        in_day / 60_000 % 60,
        in_day / 1000 % 60,
        in_day % 1000,
    );
    if millis == 0 {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
    } else {
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
    }
}

/// The UTC date of `ms`, `YYYY-MM-DD`.
pub(crate) fn day_of(ms: i64) -> String {
    let (year, month, day) = civil_from_days(ms.div_euclid(DAY_MS));
    format!("{year:04}-{month:02}-{day:02}")
}

/// Every UTC day `[since, until)` covers, oldest first, each as its date and
/// its part of the window: `[max(midnight, since), min(next midnight,
/// until))`.
pub(crate) fn days(since: i64, until: i64) -> Vec<(String, i64, i64)> {
    let mut days = Vec::new();
    let mut start = since;
    while start < until {
        let next_midnight = (start.div_euclid(DAY_MS) + 1) * DAY_MS;
        let end = next_midnight.min(until);
        days.push((day_of(start), start, end));
        start = end;
    }
    days
}
