//! Memory on this host, as the fleet view's health shows it: read from
//! `/proc/meminfo` when `GET /api/fleet` is answered, like the disk's free
//! space ([`crate::disk::report`]). Nothing is recorded.

use std::path::Path;

use serde_json::{Value, json};

/// Under this much available memory the health strip marks the host short:
/// the line the supervisor's hand-written monitor watched (MemAvailable
/// under 4 GB, rsi/scenarios/030.sh).
pub(crate) const WARN_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// `MemAvailable` and `MemTotal` of `/proc/meminfo` text, in bytes; each
/// `None` when the text does not carry it in kB.
fn parse_meminfo(text: &str) -> (Option<u64>, Option<u64>) {
    let field = |name: &str| {
        text.lines().find_map(|line| {
            let rest = line.strip_prefix(name)?.strip_prefix(':')?;
            let mut words = rest.split_whitespace();
            let kib: u64 = words.next()?.parse().ok()?;
            (words.next()? == "kB").then(|| kib.saturating_mul(1024))
        })
    };
    (field("MemAvailable"), field("MemTotal"))
}

/// The memory report the fleet view shows: what is available now, the
/// total, and the warning line; a value that cannot be read is `null`, never
/// a guess.
pub(crate) fn report() -> Value {
    report_of(Path::new("/proc/meminfo"))
}

fn report_of(path: &Path) -> Value {
    let (available, total) = std::fs::read_to_string(path)
        .map(|text| parse_meminfo(&text))
        .unwrap_or_default();
    json!({
        "available_bytes": available,
        "total_bytes": total,
        "warn_bytes": WARN_BYTES,
    })
}

#[cfg(test)]
mod tests {
    use super::{WARN_BYTES, parse_meminfo, report_of};
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn meminfo_is_read_in_bytes_and_a_missing_field_is_none() {
        let text = "MemTotal:       65536000 kB\nMemFree:         1000 kB\n\
                    MemAvailable:    8388608 kB\nBuffers:          12 kB\n";
        assert_eq!(
            parse_meminfo(text),
            (Some(8_388_608 * 1024), Some(65_536_000 * 1024))
        );
        assert_eq!(parse_meminfo("MemTotal: 10 kB\n"), (None, Some(10_240)));
        assert_eq!(
            parse_meminfo("MemAvailableX: 5 kB\nMemTotal: 7 MB\n"),
            (None, None),
            "another field, or another unit, is not read"
        );
    }

    #[test]
    fn the_report_reads_this_host_or_says_null() {
        let here = report_of(Path::new("/proc/meminfo"));
        if Path::new("/proc/meminfo").exists() {
            assert!(here["available_bytes"].as_u64().is_some_and(|bytes| bytes > 0));
            assert!(here["total_bytes"].as_u64().is_some_and(|bytes| bytes > 0));
        }
        assert_eq!(
            report_of(Path::new("/no/such/meminfo")),
            json!({"available_bytes": null, "total_bytes": null, "warn_bytes": WARN_BYTES})
        );
    }
}
