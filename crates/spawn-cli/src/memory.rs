//! Memory on this host, as the fleet view's health shows it: read from
//! `/proc/meminfo` when `GET /api/fleet` is answered, like the disk's free
//! space ([`crate::disk::report`]). Nothing is recorded.

use std::path::Path;

use serde_json::{Value, json};

const MIB: u64 = 1024 * 1024;

/// Under this much available memory the health strip marks the host short on
/// large hosts (MemAvailable under 4 GiB, rsi/scenarios/030.sh), and the
/// fallback threshold when `/proc/meminfo` cannot supply `MemTotal`.
pub(crate) const WARN_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Minimum memory warning threshold on small hosts (128 MiB when total RAM
/// is at least 256 MiB) so genuine OOM risk still triggers an alert.
const MIN_WARN_BYTES: u64 = 128 * MIB;

/// Effective memory warning threshold in bytes for a host with `total_bytes`
/// of RAM (`MemTotal`): 20% of total RAM capped at [`WARN_BYTES`] (4 GiB) on
/// hosts with `>= 20 GiB` RAM, floored at [`MIN_WARN_BYTES`] (128 MiB) on
/// small hosts (`>= 256 MiB`), and capped at `total / 2` (at least 1 byte)
/// on ultra-tiny hosts so `warn_bytes` never exceeds `total_bytes`. Falls
/// back to [`WARN_BYTES`] when `MemTotal` is absent or zero.
pub(crate) fn warn_bytes_for_total(total_bytes: Option<u64>) -> u64 {
    let Some(total) = total_bytes.filter(|&bytes| bytes > 0) else {
        return WARN_BYTES;
    };
    let proportional = total / 5;
    let floor = MIN_WARN_BYTES.min((total / 2).max(1));
    proportional.clamp(floor.min(WARN_BYTES), WARN_BYTES)
}

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
    report_for(available, total)
}

fn report_for(available: Option<u64>, total: Option<u64>) -> Value {
    json!({
        "available_bytes": available,
        "total_bytes": total,
        "warn_bytes": warn_bytes_for_total(total),
    })
}

#[cfg(test)]
mod tests {
    use super::{MIB, WARN_BYTES, parse_meminfo, report_for, report_of, warn_bytes_for_total};
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
            assert!(
                here["available_bytes"]
                    .as_u64()
                    .is_some_and(|bytes| bytes > 0)
            );
            assert!(here["total_bytes"].as_u64().is_some_and(|bytes| bytes > 0));
            assert_eq!(
                here["warn_bytes"],
                json!(warn_bytes_for_total(here["total_bytes"].as_u64()))
            );
        }
        assert_eq!(
            report_of(Path::new("/no/such/meminfo")),
            json!({"available_bytes": null, "total_bytes": null, "warn_bytes": WARN_BYTES})
        );
    }

    #[test]
    fn thin_961_mib_host_scales_warn_bytes_while_64_gib_host_keeps_default() {
        // Real 1C / 961 MiB VPS reading (`MemTotal: 984064 kB`).
        let thin_text = "MemTotal:         984064 kB\nMemAvailable:     524288 kB\n";
        let (thin_avail, thin_total) = parse_meminfo(thin_text);
        let thin_report = report_for(thin_avail, thin_total);
        let thin_warn = thin_report["warn_bytes"].as_u64().expect("warn bytes");
        assert_eq!(thin_total, Some(961 * MIB));
        assert_eq!(thin_warn, (961 * MIB) / 5);
        assert!(
            512 * MIB > thin_warn,
            "healthy 961 MiB host with 512 MiB free must not false-alarm"
        );
        assert!(
            128 * MIB < thin_warn,
            "961 MiB host under ~192 MiB free must still alarm"
        );
        assert_eq!(
            thin_report,
            json!({
                "available_bytes": 512 * MIB,
                "total_bytes": 961 * MIB,
                "warn_bytes": (961 * MIB) / 5,
            })
        );

        // Large 64 GiB host keeps default 4 GiB WARN_BYTES.
        let large_text = "MemTotal:       67108864 kB\nMemAvailable:    8388608 kB\n";
        let (large_avail, large_total) = parse_meminfo(large_text);
        let large_report = report_for(large_avail, large_total);
        assert_eq!(large_total, Some(64 * 1024 * MIB));
        assert_eq!(large_report["warn_bytes"], json!(WARN_BYTES));

        // End-to-end `report_of` with a fixture file on disk.
        let fixture = std::env::temp_dir().join(format!(
            "spawn-memory-test-{}-{:?}.txt",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(&fixture, thin_text).expect("write meminfo fixture");
        let file_report = report_of(&fixture);
        let _ = std::fs::remove_file(&fixture);
        assert_eq!(file_report, thin_report);
    }

    #[test]
    fn warn_bytes_boundaries_clamp_safely() {
        const GIB: u64 = 1024 * MIB;
        assert_eq!(warn_bytes_for_total(None), WARN_BYTES);
        assert_eq!(warn_bytes_for_total(Some(0)), WARN_BYTES);
        assert_eq!(warn_bytes_for_total(Some(20 * GIB)), WARN_BYTES);
        assert_eq!(warn_bytes_for_total(Some(640 * MIB)), 128 * MIB);
        assert_eq!(warn_bytes_for_total(Some(512 * MIB)), 128 * MIB);
        assert_eq!(warn_bytes_for_total(Some(256 * MIB)), 128 * MIB);
        assert_eq!(warn_bytes_for_total(Some(100 * MIB)), 50 * MIB);
        assert_eq!(warn_bytes_for_total(Some(1)), 1);
    }
}
