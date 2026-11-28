//! Disk room on this host, watched by `spawn loop` (docs/storage.md, "Disk
//! room").
//!
//! Once a minute the loop reads the free space of the workspace root and of
//! `TMPDIR` (the gate's scratch). A path whose free space falls under
//! `SPAWN_DISK_WARN_GB` (default 20 GiB) is recorded once as a `host.disk`
//! record with state `low`, and once more as `recovered` when it is back
//! [`RECOVERY_MARGIN`] above that, so a path hovering at the threshold does
//! not flap. The latest record per path is read back from the log on start,
//! so a restart records nothing twice. A path that is no longer one of the
//! two (the root or `TMPDIR` changed) is never read again: a `low` record
//! it left is closed at the first check with a `recovered` record whose
//! `reason` is `unwatched` ([`Monitor::unwatched`]), which is no news. The
//! supervisor's digest carries each
//! path's latest record once ([`escalations`]): a `low` one is a condition
//! ([`Why::Disk`]) that holds until a `recovered` record replaces it, told
//! or not; a `recovered` one is news ([`Why::DiskRecovered`]) for a day, and
//! told, it is done. The fleet view's health shows the records and the free
//! space now.
//!
//! Under `SPAWN_DISK_FLOOR_GB` (default 2 GiB) on any of the paths, the loop
//! starts no new executions: the dispatcher's `start` and `resume` orders
//! are held (they are decided again on every pass, so nothing is lost) and
//! residents are not revived. Running executions continue, and the
//! supervisor is still kept alive, so someone can act on the warning.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use spawn_core::command::CommandAction;
use spawn_postgres::PostgresStore;

use crate::config::env_value;
use crate::dispatch::{Escalation, Order, Why};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

/// Minimum effective warning threshold on small volumes (64 MiB when the
/// filesystem can hold at least 128 MiB).
const MIN_WARN_BYTES: u64 = 64 * MIB;

/// Minimum effective execution-hold floor on small volumes (16 MiB when the
/// filesystem can hold at least 64 MiB).
const MIN_FLOOR_BYTES: u64 = 16 * MIB;

/// How often the loop reads free space.
pub(crate) const CHECK_EVERY: Duration = Duration::from_mins(1);

/// How far above the warning threshold a low path must come back before it
/// is recorded as recovered.
pub(crate) const RECOVERY_MARGIN: u64 = GIB;

/// How long a `recovered` record is news for the supervisor's digest: one
/// nobody was told of within a day is stale. A `low` record has no window:
/// it holds until its path recovers.
const RECOVERY_NEWS_MS: i64 = 86_400_000;

/// The `reason` of a `recovered` record that closes the `low` record of a
/// path the loop no longer watches ([`Monitor::unwatched`]): nothing was
/// read there, so it is not news of room ([`escalations`]).
const UNWATCHED: &str = "unwatched";

/// The thresholds, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Thresholds {
    /// Under this a path is recorded `low`.
    pub(crate) warn: u64,
    /// Under this no new execution starts.
    pub(crate) floor: u64,
}

fn cap_to_volume(
    configured: u64,
    total: u64,
    ratio_divisor: u64,
    min_bytes: u64,
    max_share_divisor: u64,
) -> u64 {
    if configured == 0 || total == 0 {
        return configured;
    }
    let proportional = total / ratio_divisor;
    let floor = min_bytes.min((total / max_share_divisor).max(1));
    configured.min(proportional.max(floor))
}

impl Thresholds {
    /// `SPAWN_DISK_WARN_GB` (default 20) and `SPAWN_DISK_FLOOR_GB` (default
    /// 2), whole GiB.
    pub(crate) fn from_env() -> Self {
        let gib = |name: &str, fallback: u64| {
            env_value(name)
                .and_then(|text| text.trim().parse::<u64>().ok())
                .unwrap_or(fallback)
                .saturating_mul(GIB)
        };
        Self {
            warn: gib("SPAWN_DISK_WARN_GB", 20),
            floor: gib("SPAWN_DISK_FLOOR_GB", 2),
        }
    }

    /// Effective thresholds for a filesystem of `total_bytes`: preserves
    /// configured `warn` / `floor` on large volumes (`>= 100 GiB` for the
    /// 20 GiB / 2 GiB defaults) or when `total_bytes` is unknown/zero, and
    /// proportionally caps them on small volumes (`warn <= total / 5`,
    /// `floor <= total / 10`, with guarded minimums that never exceed the
    /// volume or invert `floor <= warn`, and never raise a `0` threshold).
    pub(crate) fn effective_for(self, total_bytes: Option<u64>) -> Self {
        let Some(total) = total_bytes.filter(|&bytes| bytes > 0) else {
            return self;
        };
        Self {
            warn: cap_to_volume(self.warn, total, 5, MIN_WARN_BYTES, 2),
            floor: cap_to_volume(self.floor, total, 10, MIN_FLOOR_BYTES, 4),
        }
    }
}

/// Recovery hysteresis above `warn` for a filesystem of `total_bytes`:
/// [`RECOVERY_MARGIN`] (1 GiB) on volumes `>= 20 GiB` or unknown capacity,
/// scaled to 5% of `total_bytes` (at least 1 byte) on smaller volumes so a
/// small `tmpfs` (e.g., 480 MiB `/tmp`) can always recover cleanly.
pub(crate) fn recovery_margin_for(total_bytes: Option<u64>) -> u64 {
    let Some(total) = total_bytes.filter(|&bytes| bytes > 0) else {
        return RECOVERY_MARGIN;
    };
    (total / 20).clamp(1, RECOVERY_MARGIN)
}

/// A path the fleet writes to, and what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Place {
    pub(crate) label: &'static str,
    pub(crate) path: PathBuf,
}

/// The workspace root (when there is one) and `TMPDIR` (else `/tmp`).
pub(crate) fn places(root: Option<&Path>) -> Vec<Place> {
    let mut places: Vec<Place> = root
        .map(|root| Place {
            label: "workspace",
            path: root.to_path_buf(),
        })
        .into_iter()
        .collect();
    places.push(Place {
        label: "tmpdir",
        path: env_value("TMPDIR").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from),
    });
    places
}

/// Free (unprivileged available) and total bytes of a filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Space {
    pub(crate) free: u64,
    pub(crate) total: u64,
}

/// Free and total bytes on the filesystem that holds `path`, via `statvfs(2)`.
pub(crate) fn space(path: &Path) -> Result<Space, String> {
    let stat = rustix::fs::statvfs(path)
        .map_err(|error| format!("statvfs {}: {error}", path.display()))?;
    let unit = if stat.f_frsize > 0 {
        stat.f_frsize
    } else {
        stat.f_bsize
    };
    Ok(Space {
        free: stat.f_bavail.saturating_mul(unit),
        total: stat.f_blocks.saturating_mul(unit),
    })
}

/// Free bytes on the filesystem that holds `path`, as `statvfs(2)` reports
/// them to an unprivileged user.
pub(crate) fn free_bytes(path: &Path) -> Result<u64, String> {
    space(path).map(|measured| measured.free)
}

/// One reading of one place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reading {
    pub(crate) place: Place,
    pub(crate) free: u64,
}

/// Internal reading paired with the filesystem's total capacity when known.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Measured {
    reading: Reading,
    total: Option<u64>,
}

/// The free and total space of every place, now; a place that cannot be read
/// is left out and named in the error lines.
fn read_measured(places: &[Place]) -> (Vec<Measured>, Vec<String>) {
    let mut readings = Vec::new();
    let mut errors = Vec::new();
    for place in places {
        match space(&place.path) {
            Ok(measured) => readings.push(Measured {
                reading: Reading {
                    place: place.clone(),
                    free: measured.free,
                },
                total: Some(measured.total),
            }),
            Err(error) => errors.push(format!("disk: {error}")),
        }
    }
    (readings, errors)
}

/// Bytes as GiB with one decimal.
pub(crate) fn gib(bytes: u64) -> String {
    // Display only: the precision lost converting to f64 is far below 0.1 GiB.
    #[allow(clippy::cast_precision_loss)]
    let value = bytes as f64 / GIB as f64;
    format!("{value:.1} GiB")
}

/// The record a reading makes, given whether its path was last recorded
/// low: `low` when it falls under the effective warning threshold, `recovered`
/// when a low path is back [`recovery_margin_for`] above it, else none.
fn crossing(
    was_low: bool,
    reading: &Reading,
    total: Option<u64>,
    thresholds: Thresholds,
) -> Option<Value> {
    let effective = thresholds.effective_for(total);
    let margin = recovery_margin_for(total);
    let state = if !was_low && reading.free < effective.warn {
        "low"
    } else if was_low && reading.free >= effective.warn.saturating_add(margin) {
        "recovered"
    } else {
        return None;
    };
    let mut record = json!({
        "type": "host.disk",
        "label": reading.place.label,
        "path": reading.place.path.display().to_string(),
        "state": state,
        "free_bytes": reading.free,
        "warn_bytes": effective.warn,
        "floor_bytes": effective.floor,
    });
    if let Some(total) = total {
        record["total_bytes"] = json!(total);
    }
    Some(record)
}

/// Keep `orders` that start no new execution; return how many were held.
pub(crate) fn hold_new_executions(orders: &mut Vec<Order>) -> usize {
    let before = orders.len();
    orders.retain(|order| {
        !matches!(
            order.action,
            CommandAction::Start { .. } | CommandAction::Resume { .. }
        )
    });
    before - orders.len()
}

/// The digest items for `records`, the latest per path (as
/// `PostgresStore::host_disk` returns them): a `low` record, whatever its
/// age, since its path has not recovered ([`Why::Disk`], a condition); a
/// `recovered` one of the last [`RECOVERY_NEWS_MS`] ([`Why::DiskRecovered`],
/// news: once told, [`crate::dispatch::decide_with`] drops it). A `low`
/// record whose path the loop stopped watching is closed by the loop
/// ([`Monitor::unwatched`]), and that closing record is no item.
pub(crate) fn escalations(records: &[Value], now_ms: i64) -> Vec<Escalation> {
    records
        .iter()
        .filter_map(|record| {
            let id = record["event_id"].as_i64()?;
            let at_ms = record["at_ms"].as_i64()?;
            let free = gib(record["free_bytes"].as_u64()?);
            let path = record["path"].as_str()?;
            let label = record["label"].as_str().unwrap_or("disk");
            let (why, line) = match record["state"].as_str()? {
                "low" => (
                    Why::Disk,
                    format!(
                        "{label} {path} has {free} free, under {} (`SPAWN_DISK_WARN_GB`). Under {} \
no new executions start; free space (old workspaces, build output) or tell the owner.",
                        gib(record["warn_bytes"].as_u64().unwrap_or_default()),
                        gib(record["floor_bytes"].as_u64().unwrap_or_default()),
                    ),
                ),
                // Closed because its path is no longer watched: no room
                // was read there, so there is nothing to tell.
                "recovered" if record["reason"] == UNWATCHED => return None,
                "recovered" if now_ms - at_ms < RECOVERY_NEWS_MS => (
                    Why::DiskRecovered,
                    format!("{label} {path} has {free} free again"),
                ),
                _ => return None,
            };
            Some(Escalation {
                session_id: "host".to_string(),
                id: format!("disk-{id}"),
                why,
                line,
                since_ms: Some(at_ms),
            })
        })
        .collect()
}

/// The digest items for `records`, the integrator's `no_room` gate records
/// that carry `escalate` (`PostgresStore::gate_room_escalations`): a
/// mission's gate has had no room to judge for more than two hours. Each
/// names the check that fell short (`disk`, `memory` or `database`; a record
/// that names none is the gate's own disk floor), its measure and its
/// threshold. No time window: a record stays an item until a digest carrying
/// its tag reaches the supervisor, and after that while the run of `no_room`
/// it escalated goes on (the mission's `gate_room_*`), so the plan holds it
/// as told and unresolved until the gate gets room or the task leaves the
/// gate. The integrator writes one per run, so the supervisor is told once.
pub(crate) fn gate_room_escalations(records: &[Value]) -> Vec<Escalation> {
    records
        .iter()
        .filter_map(|record| {
            let id = record["event_id"].as_i64()?;
            let mission = record["mission"]
                .as_i64()
                .map_or_else(|| "a mission".to_string(), |id| format!("mission {id}"));
            let check = record["check"].as_str().unwrap_or("disk");
            let text = |name: &str| match &record[name] {
                Value::String(text) => text.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            let (what, advice) = match check {
                "memory" => (
                    format!(
                        "{} MiB of memory available, under SPAWN_GATE_MIN_FREE_MEMORY_MB={}",
                        text("measured"),
                        text("threshold")
                    ),
                    "free memory on this host",
                ),
                "database" => (
                    format!(
                        "the test database{} failed the probe \"{}\": {}",
                        record["server"]
                            .as_str()
                            .map_or_else(String::new, |server| format!(" ({server})")),
                        text("threshold"),
                        text("measured")
                    ),
                    "repair the test database server (its /dev/shm, its connections)",
                ),
                // A scenario run that waited past its bound for one of the
                // host's slots: the same room statement, about the fifth
                // resource — and the advice has to name what the owner can
                // act on, or the escalation only repeats the symptom.
                "host scenario slot" => (
                    format!(
                        "a scenario run waited {} for one of SPAWN_GATE_HOST_JOBS={} scenario \
                         slots (other gates on this host were running, or a hand-run gate was \
                         left waiting)",
                        crate::gate::spoken_ms(record["measured"].as_u64().unwrap_or_default()),
                        text("threshold")
                    ),
                    "find what holds the slots (another mission's gate, an owner gate left \
                     waiting in its log) and finish or stop it, or raise SPAWN_GATE_HOST_JOBS if \
                     this host can serve more scenarios",
                ),
                _ => (
                    format!(
                        "{} free in TMPDIR {}, under SPAWN_GATE_MIN_FREE_GB={}",
                        gib(record["free_kb"].as_u64()?.saturating_mul(1024)),
                        record["tmpdir"].as_str()?,
                        record["min_gb"].as_u64().unwrap_or_default()
                    ),
                    "free space there (old gate scratch, build output)",
                ),
            };
            Some(Escalation {
                session_id: "host".to_string(),
                id: format!("gate-room-{id}"),
                why: Why::GateRoom,
                line: format!(
                    "{check} check: the gate of {mission} has had no room to judge for over two \
hours: {what}. Its work waits (no halt counts); {advice} or tell the owner."
                ),
                since_ms: record["at_ms"].as_i64(),
            })
        })
        .collect()
}

/// The size report the fleet view shows: per place (the workspace root of
/// `SPAWN_WORKSPACE_ROOT`, and `TMPDIR`) its free and total space now (`null`
/// when it cannot be read), the effective thresholds for that filesystem's
/// capacity, and the state of its latest `host.disk` record (`null` when none).
pub(crate) fn report(records: &Value) -> Value {
    let root = env_value("SPAWN_WORKSPACE_ROOT").map(PathBuf::from);
    report_of(&places(root.as_deref()), Thresholds::from_env(), records)
}

fn report_of(places: &[Place], thresholds: Thresholds, records: &Value) -> Value {
    let places: Vec<Value> = places
        .iter()
        .map(|place| report_place(place, space(&place.path).ok(), thresholds, records))
        .collect();
    Value::Array(places)
}

fn report_place(
    place: &Place,
    measured: Option<Space>,
    thresholds: Thresholds,
    records: &Value,
) -> Value {
    let path = place.path.display().to_string();
    let recorded = records
        .as_array()
        .into_iter()
        .flatten()
        .find(|record| record["path"].as_str() == Some(path.as_str()));
    let total = measured.map(|space| space.total);
    let effective = thresholds.effective_for(total);
    json!({
        "label": place.label,
        "path": path,
        "free_bytes": measured.map(|space| space.free),
        "total_bytes": total,
        "warn_bytes": effective.warn,
        "floor_bytes": effective.floor,
        "recorded": recorded.map(|record| json!({
            "state": record["state"],
            "free_bytes": record["free_bytes"],
            "at_ms": record["at_ms"],
        })),
    })
}

/// What the loop knows about disk room: the latest record per path (read
/// back from the log once, then kept as it records) and the latest reading.
pub(crate) struct Monitor {
    places: Vec<Place>,
    thresholds: Thresholds,
    checked: Option<Instant>,
    /// The latest `host.disk` record per path, `None` until read from the log.
    records: Option<Vec<Value>>,
    readings: Vec<Measured>,
}

impl Monitor {
    pub(crate) fn new(root: Option<&Path>) -> Self {
        Self::with(places(root), Thresholds::from_env())
    }

    pub(crate) fn with(places: Vec<Place>, thresholds: Thresholds) -> Self {
        Self {
            places,
            thresholds,
            checked: None,
            records: None,
            readings: Vec::new(),
        }
    }

    /// Read free space when [`CHECK_EVERY`] has passed, and record each
    /// crossing. Returns lines for the loop's log.
    pub(crate) fn check(&mut self, store: &PostgresStore) -> Result<Vec<String>, String> {
        if self
            .checked
            .is_some_and(|checked| checked.elapsed() < CHECK_EVERY)
        {
            return Ok(Vec::new());
        }
        // A failure below is tried again at the next check, not every pass.
        self.checked = Some(Instant::now());
        if self.records.is_none() {
            let records = store.host_disk().map_err(|error| error.to_string())?;
            self.records = Some(records.as_array().cloned().unwrap_or_default());
        }
        let (readings, mut lines) = read_measured(&self.places);
        let now = spawn_core::time::Clock::now(&spawn_core::time::SystemClock).0;
        // A low record of a path no longer watched is closed; one that
        // could not be written is made again at the next check.
        let mut records = self.unwatched();
        records.extend(self.observe_measured(readings));
        for record in records {
            let id = store
                .host_record(&record)
                .map_err(|error| format!("disk record: {error}"))?;
            let path = record["path"].as_str().unwrap_or_default();
            lines.push(if record["reason"] == UNWATCHED {
                format!("disk: {path} is no longer watched: its low record is closed")
            } else {
                format!(
                    "disk: {path} {} {} free",
                    record["state"].as_str().unwrap_or_default(),
                    gib(record["free_bytes"].as_u64().unwrap_or_default())
                )
            });
            self.remember(record, id, now);
        }
        Ok(lines)
    }

    /// Take `readings` as the latest and return the records they make.
    fn observe_measured(&mut self, readings: Vec<Measured>) -> Vec<Value> {
        let records: Vec<Value> = readings
            .iter()
            .filter_map(|measured| {
                crossing(
                    self.was_low(&measured.reading.place.path),
                    &measured.reading,
                    measured.total,
                    self.thresholds,
                )
            })
            .collect();
        self.readings = readings;
        records
    }

    /// Take `readings` without capacity metadata as the latest and return the
    /// records they make.
    #[cfg(test)]
    pub(crate) fn observe(&mut self, readings: Vec<Reading>) -> Vec<Value> {
        self.observe_measured(
            readings
                .into_iter()
                .map(|reading| Measured {
                    reading,
                    total: None,
                })
                .collect(),
        )
    }

    /// The records that close the `low` records of paths this loop does not
    /// watch (the workspace root or `TMPDIR` changed since): no reading of
    /// such a path is ever taken again, so nothing else would replace its
    /// `low` record and its item would stand forever. Each is a `recovered`
    /// record with `reason` [`UNWATCHED`] and the old record's numbers,
    /// which nothing read anew. Once written and remembered, none is made
    /// again.
    pub(crate) fn unwatched(&self) -> Vec<Value> {
        self.records
            .iter()
            .flatten()
            .filter(|record| record["state"] == "low")
            .filter(|record| {
                !self.places.iter().any(|place| {
                    record["path"].as_str() == Some(place.path.display().to_string().as_str())
                })
            })
            .map(|record| {
                json!({
                    "type": "host.disk",
                    "label": record["label"],
                    "path": record["path"],
                    "state": "recovered",
                    "reason": UNWATCHED,
                    "free_bytes": record["free_bytes"].as_u64().unwrap_or_default(),
                    "warn_bytes": record["warn_bytes"],
                    "floor_bytes": record["floor_bytes"],
                })
            })
            .collect()
    }

    /// Whether the latest record of `path` says it is low.
    fn was_low(&self, path: &Path) -> bool {
        let path = path.display().to_string();
        self.records.iter().flatten().any(|record| {
            record["path"].as_str() == Some(path.as_str()) && record["state"] == "low"
        })
    }

    /// Keep `record`, written as event `id` at `at_ms`, as its path's latest.
    pub(crate) fn remember(&mut self, record: Value, id: i64, at_ms: i64) {
        let mut record = record;
        record["event_id"] = json!(id);
        record["at_ms"] = json!(at_ms);
        let records = self.records.get_or_insert_with(Vec::new);
        records.retain(|kept| kept["path"] != record["path"]);
        records.push(record);
    }

    /// Whether any place was under its effective floor at the latest reading:
    /// no new execution starts.
    pub(crate) fn below_floor(&self) -> bool {
        self.readings.iter().any(|measured| {
            measured.reading.free < self.thresholds.effective_for(measured.total).floor
        })
    }

    /// The digest items for the latest records.
    pub(crate) fn escalations(&self, now_ms: i64) -> Vec<Escalation> {
        escalations(self.records.as_deref().unwrap_or_default(), now_ms)
    }

    /// How long until these items change with no new record: a `recovered`
    /// one is news for [`RECOVERY_NEWS_MS`] from its record, and stops being
    /// an item then ([`escalations`]). A `low` one holds until its path
    /// recovers, which writes a record. `None` when nothing here waits on a
    /// clock.
    pub(crate) fn due_in(&self, now_ms: i64) -> Option<i64> {
        self.records
            .iter()
            .flatten()
            .filter(|record| record["state"] == "recovered")
            .filter_map(|record| record["at_ms"].as_i64())
            .map(|at| at.saturating_add(RECOVERY_NEWS_MS).saturating_sub(now_ms))
            .filter(|left| *left > 0)
            .min()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]
    use super::{
        GIB, MIB, Measured, Monitor, Place, RECOVERY_MARGIN, Reading, Space, Thresholds,
        escalations, free_bytes, gate_room_escalations, hold_new_executions, places,
        recovery_margin_for, report_place, space,
    };
    use crate::dispatch::{Order, Why};
    use serde_json::json;
    use spawn_core::command::CommandAction;
    use std::path::{Path, PathBuf};

    const THRESHOLDS: Thresholds = Thresholds {
        warn: 20 * GIB,
        floor: 2 * GIB,
    };

    fn place(path: &str) -> Place {
        Place {
            label: "workspace",
            path: PathBuf::from(path),
        }
    }

    fn reading(path: &str, free: u64) -> Reading {
        Reading {
            place: place(path),
            free,
        }
    }

    fn measured(label: &'static str, path: &str, free: u64, total: u64) -> Measured {
        Measured {
            reading: Reading {
                place: Place {
                    label,
                    path: PathBuf::from(path),
                },
                free,
            },
            total: Some(total),
        }
    }

    fn order(action: CommandAction) -> Order {
        Order {
            agent_id: "agent_worker_a".to_string(),
            session_id: "session_worker_a".to_string(),
            key: "k".to_string(),
            action,
            task: None,
        }
    }

    #[test]
    fn statvfs_space_reads_free_and_total_bytes() {
        let root = space(Path::new("/")).expect("root filesystem");
        assert!(root.free > 0);
        assert!(root.total >= root.free);
        assert!(free_bytes(Path::new("/")).is_ok_and(|free| free > 0));
        assert!(space(Path::new("/no/such/path/at/all")).is_err());
        assert!(free_bytes(Path::new("/no/such/path/at/all")).is_err());
    }

    #[test]
    fn the_places_are_the_workspace_root_and_tmpdir() {
        let labels: Vec<&str> = places(Some(Path::new("/ws")))
            .iter()
            .map(|place| place.label)
            .collect();
        assert_eq!(labels, ["workspace", "tmpdir"]);
        assert_eq!(places(None).len(), 1);
    }

    #[test]
    fn a_crossing_is_recorded_once_and_its_recovery_once_past_the_margin() {
        let mut monitor = Monitor::with(vec![place("/ws")], THRESHOLDS);
        let mut states = Vec::new();
        let mut at = 0;
        for free in [
            25 * GIB,                       // fine: nothing
            19 * GIB,                       // low
            10 * GIB,                       // still low: nothing
            20 * GIB,                       // back at the threshold: nothing yet
            20 * GIB + RECOVERY_MARGIN - 1, // inside the margin: nothing
            20 * GIB + RECOVERY_MARGIN,     // recovered
            21 * GIB + RECOVERY_MARGIN,     // fine: nothing
            19 * GIB,                       // low again
        ] {
            at += 1;
            for record in monitor.observe(vec![reading("/ws", free)]) {
                states.push(record["state"].as_str().unwrap_or_default().to_string());
                monitor.remember(record, at, at);
            }
        }
        assert_eq!(states, ["low", "recovered", "low"]);
    }

    #[test]
    fn a_path_the_log_last_saw_low_is_not_recorded_low_again_after_a_restart() {
        let mut monitor = Monitor::with(vec![place("/ws")], THRESHOLDS);
        monitor.remember(json!({"path": "/ws", "state": "low"}), 1, 1);
        assert!(monitor.observe(vec![reading("/ws", GIB)]).is_empty());
        let recovered = monitor.observe(vec![reading("/ws", 30 * GIB)]);
        assert_eq!(recovered[0]["state"], "recovered");
        assert_eq!(recovered[0]["free_bytes"], json!(30 * GIB));
        assert_eq!(recovered[0]["warn_bytes"], json!(20 * GIB));
    }

    /// GitHub #16 item 7: the workspace root or `TMPDIR` changed while a
    /// path was low. Nothing reads the old path again, so no `recovered`
    /// record would ever replace its `low` one: the monitor closes it.
    #[test]
    fn a_low_record_of_a_path_no_longer_watched_is_closed_and_is_no_item() {
        let low = |path: &str| {
            json!({"type": "host.disk", "label": "workspace", "path": path, "state": "low",
                   "free_bytes": 5 * GIB, "warn_bytes": 20 * GIB, "floor_bytes": 2 * GIB})
        };
        let ids = |monitor: &Monitor| -> Vec<String> {
            monitor
                .escalations(1000)
                .into_iter()
                .map(|item| item.id)
                .collect()
        };
        let mut monitor = Monitor::with(vec![place("/ws")], THRESHOLDS);
        monitor.remember(low("/old"), 3, 10);
        monitor.remember(low("/ws"), 4, 20);
        monitor.remember(
            json!({"type": "host.disk", "label": "tmpdir", "path": "/gone", "state": "recovered",
                   "free_bytes": 30 * GIB}),
            5,
            30,
        );

        let closing = monitor.unwatched();
        assert_eq!(
            closing,
            [
                json!({"type": "host.disk", "label": "workspace", "path": "/old",
                       "state": "recovered", "reason": "unwatched", "free_bytes": 5 * GIB,
                       "warn_bytes": 20 * GIB, "floor_bytes": 2 * GIB})
            ],
            "only the low record of the path not watched"
        );
        for record in closing {
            monitor.remember(record, 6, 40);
        }
        assert_eq!(
            ids(&monitor),
            ["disk-4", "disk-5"],
            "the watched path is still low; a recovery that was read stays news"
        );
        assert!(monitor.unwatched().is_empty(), "closed once");
        // Whoever reads the log (the attention queue) sees it closed too,
        // and not as a recovery to tell: nothing was read there.
        assert!(
            escalations(
                &[
                    json!({"type": "host.disk", "label": "workspace", "path": "/old",
                         "state": "recovered", "reason": "unwatched", "free_bytes": 5 * GIB,
                         "event_id": 6, "at_ms": 40})
                ],
                1000
            )
            .is_empty()
        );
    }

    /// A path closed as unwatched and watched again later starts over: low
    /// is recorded again when it is.
    #[test]
    fn a_path_watched_again_after_it_was_closed_is_recorded_low_again() {
        let mut monitor = Monitor::with(vec![place("/old")], THRESHOLDS);
        monitor.remember(
            json!({"type": "host.disk", "label": "workspace", "path": "/old",
                   "state": "recovered", "reason": "unwatched", "free_bytes": GIB}),
            6,
            40,
        );
        assert!(monitor.unwatched().is_empty());
        let records = monitor.observe(vec![reading("/old", GIB)]);
        assert_eq!(records[0]["state"], "low");
        assert_eq!(records[0].get("reason"), None);
    }

    /// The same against the real store, as the loop's check runs it: the
    /// closing record is written, so the latest record of the removed path
    /// in the log is no longer `low` for any reader.
    #[test]
    fn the_check_closes_a_removed_paths_low_record_in_the_log() {
        let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
            "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
        );
        let schema = spawn_postgres::testing::TestSchema::new("disk_unwatched");
        let store = spawn_postgres::PostgresStore::connect_in_schema_with_options(
            &url,
            &schema,
            &spawn_postgres::testing::test_connect_options(),
        )
        .expect("a schema of its own, migrated");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let low = store
                .host_record(&json!({"type": "host.disk", "label": "workspace",
                    "path": "/removed/workspaces", "state": "low", "free_bytes": 5 * GIB,
                    "warn_bytes": 20 * GIB, "floor_bytes": 2 * GIB}))
                .expect("the old root was low");
            // The loop restarts with another root, which has room.
            let roomy = Thresholds { warn: 0, floor: 0 };
            let mut monitor = Monitor::with(vec![place("/")], roomy);
            let lines = monitor.check(&store).expect("checked");
            assert_eq!(
                lines,
                ["disk: /removed/workspaces is no longer watched: its low record is closed"]
            );
            assert!(monitor.escalations(i64::MAX).is_empty());
            let latest = store.host_disk().expect("the log");
            assert_eq!(latest.as_array().map(Vec::len), Some(1));
            assert_eq!(latest[0]["path"], "/removed/workspaces");
            assert_eq!(latest[0]["state"], "recovered");
            assert_eq!(latest[0]["reason"], "unwatched");
            assert!(latest[0]["event_id"].as_i64().is_some_and(|id| id > low));
            assert!(
                escalations(latest.as_array().expect("records"), i64::MAX).is_empty(),
                "no item for a reader of the log"
            );
            // The next loop finds it closed and writes nothing more.
            let mut next = Monitor::with(vec![place("/")], roomy);
            assert!(next.check(&store).expect("checked").is_empty());
            assert_eq!(store.host_disk().expect("the log"), latest);
        }));
        drop(store);
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }

    #[test]
    fn under_the_floor_new_executions_are_held_and_running_ones_are_not_touched() {
        let mut monitor = Monitor::with(vec![place("/ws"), place("/tmp")], THRESHOLDS);
        assert!(!monitor.below_floor(), "nothing read yet: nothing held");
        monitor.observe(vec![reading("/ws", 30 * GIB), reading("/tmp", 2 * GIB)]);
        assert!(!monitor.below_floor(), "at the floor is not under it");
        monitor.observe(vec![reading("/ws", 30 * GIB), reading("/tmp", 2 * GIB - 1)]);
        assert!(monitor.below_floor(), "one place under the floor is enough");

        let mut orders = vec![
            order(CommandAction::Start {
                engine: "claude".to_string(),
                prompt: "go".to_string(),
                parent_session_id: None,
                from: None,
                model: None,
                charter: None,
                model_label: None,
            }),
            order(CommandAction::Input {
                text: "next".to_string(),
                tags: Vec::new(),
            }),
            order(CommandAction::Resume {
                prompt: None,
                model: None,
            }),
            order(CommandAction::Stop),
        ];
        assert_eq!(hold_new_executions(&mut orders), 2);
        let kept: Vec<&CommandAction> = orders.iter().map(|order| &order.action).collect();
        assert_eq!(
            kept,
            [
                &CommandAction::Input {
                    text: "next".to_string(),
                    tags: Vec::new(),
                },
                &CommandAction::Stop
            ]
        );
    }

    #[test]
    fn the_report_shows_free_space_now_and_the_latest_record() {
        let records = json!([{"path": "/", "state": "low", "free_bytes": 5, "at_ms": 9}]);
        let report = super::report_of(
            &[place("/"), place("/no/such/path/at/all")],
            THRESHOLDS,
            &records,
        );
        assert!(
            report[0]["free_bytes"]
                .as_u64()
                .is_some_and(|free| free > 0)
        );
        let root_total = report[0]["total_bytes"].as_u64();
        assert!(root_total.is_some_and(|total| total > 0));
        let expected_root = THRESHOLDS.effective_for(root_total);
        assert_eq!(
            report[0]["recorded"],
            json!({"state": "low", "free_bytes": 5, "at_ms": 9})
        );
        assert_eq!(report[0]["warn_bytes"], json!(expected_root.warn));
        assert_eq!(report[0]["floor_bytes"], json!(expected_root.floor));
        assert_eq!(report[1]["free_bytes"], json!(null), "unreadable: null");
        assert_eq!(report[1]["total_bytes"], json!(null), "unreadable: null");
        assert_eq!(report[1]["warn_bytes"], json!(20 * GIB));
        assert_eq!(report[1]["floor_bytes"], json!(2 * GIB));
        assert_eq!(report[1]["recorded"], json!(null));
    }

    /// A path's latest record is an item: `low` until the path recovers,
    /// whatever its age (a condition); `recovered` for a day (news).
    #[test]
    fn a_low_path_is_an_item_until_it_recovers_and_a_recovery_is_news_for_a_day() {
        let day = 86_400_000;
        let records = [
            json!({"type": "host.disk", "label": "tmpdir", "path": "/tmp", "state": "low",
                   "free_bytes": 5 * GIB, "warn_bytes": 20 * GIB, "floor_bytes": 2 * GIB,
                   "event_id": 7, "at_ms": 10 * day}),
            json!({"type": "host.disk", "label": "workspace", "path": "/ws", "state": "recovered",
                   "free_bytes": 30 * GIB, "event_id": 9, "at_ms": 10 * day + 5}),
            json!({"type": "host.disk", "label": "workspace", "path": "/old", "state": "low",
                   "free_bytes": 1, "event_id": 3, "at_ms": 8 * day}),
            json!({"type": "host.disk", "label": "workspace", "path": "/back", "state": "recovered",
                   "free_bytes": 40 * GIB, "event_id": 2, "at_ms": 8 * day}),
        ];
        let items = escalations(&records, 11 * day - 1);
        let summary: Vec<(String, Why, String)> = items
            .iter()
            .map(|item| (item.id.clone(), item.why, item.tag()))
            .collect();
        assert_eq!(
            summary,
            [
                ("disk-7".to_string(), Why::Disk, "host disk-7".to_string()),
                (
                    "disk-9".to_string(),
                    Why::DiskRecovered,
                    "host disk-9".to_string()
                ),
                ("disk-3".to_string(), Why::Disk, "host disk-3".to_string()),
            ],
            "a recovery older than a day is stale news; a path low for three days still is"
        );
        assert!(
            items[0]
                .line
                .contains("/tmp has 5.0 GiB free, under 20.0 GiB")
        );
        assert!(
            items[0]
                .line
                .contains("Under 2.0 GiB no new executions start")
        );
        assert_eq!(items[1].line, "workspace /ws has 30.0 GiB free again");
        assert_eq!(items[2].since_ms, Some(8 * day));
    }

    #[test]
    fn a_gate_without_room_for_hours_is_told_with_the_free_space_and_tmpdir() {
        let day: i64 = 86_400_000;
        let records = [
            json!({"type": "task.gated", "verdict": "no_room", "escalate": true, "id": 4,
                   "free_kb": 3 * 1_048_576, "min_gb": 10, "tmpdir": "/data/tmp",
                   "event_id": 12, "at_ms": 10 * day, "mission": 7}),
            json!({"type": "task.gated", "verdict": "no_room", "escalate": true, "id": 5,
                   "free_kb": 1, "min_gb": 10, "tmpdir": "/old", "event_id": 2,
                   "at_ms": 8 * day, "mission": 3}),
        ];
        // The store returns only the untold ones; their age does not matter.
        let items = gate_room_escalations(&records);
        assert_eq!(items.len(), 2, "a record older than a day is still told");
        assert_eq!(items[1].tag(), "host gate-room-2");
        assert_eq!(items[0].why, Why::GateRoom);
        assert_eq!(items[0].tag(), "host gate-room-12");
        assert!(
            items[0].line.contains(
                "the gate of mission 7 has had no room to judge for over two hours: 3.0 GiB \
free in TMPDIR /data/tmp, under SPAWN_GATE_MIN_FREE_GB=10"
            ),
            "{}",
            items[0].line
        );
        assert!(
            items[0].line.starts_with("disk check: "),
            "{}",
            items[0].line
        );
    }

    /// Objective 018: the item names the check that fell short, its measure
    /// and its threshold: memory and the test database too.
    #[test]
    fn a_gate_without_room_is_told_with_its_check_measure_and_threshold() {
        let records = [
            json!({"type": "task.gated", "verdict": "no_room", "escalate": true, "id": 4,
                   "check": "memory", "measured": 1500, "threshold": 2048,
                   "event_id": 21, "mission": 7}),
            json!({"type": "task.gated", "verdict": "no_room", "escalate": true, "id": 5,
                   "check": "database",
                   "measured": "ERROR: could not resize shared memory segment (53100)",
                   "threshold": "a parallel query within 10 s",
                   "server": "/run/test-pg:5432/spawn_test", "event_id": 22, "mission": 8}),
            json!({"type": "task.gated", "verdict": "no_room", "escalate": true, "id": 6,
                   "check": "disk", "measured": 1_048_576, "threshold": 10_485_760,
                   "free_kb": 1_048_576, "min_gb": 10, "tmpdir": "/tmp",
                   "event_id": 23, "mission": 9}),
        ];
        let items = gate_room_escalations(&records);
        let lines: Vec<&str> = items.iter().map(|item| item.line.as_str()).collect();
        assert_eq!(
            lines[0],
            "memory check: the gate of mission 7 has had no room to judge for over two hours: \
1500 MiB of memory available, under SPAWN_GATE_MIN_FREE_MEMORY_MB=2048. Its work waits (no halt \
counts); free memory on this host or tell the owner."
        );
        assert!(
            lines[1].starts_with(
                "database check: the gate of mission 8 has had no room to judge for over two \
hours: the test database (/run/test-pg:5432/spawn_test) failed the probe \"a parallel query \
within 10 s\": ERROR: could not resize shared memory segment (53100)."
            ),
            "{}",
            lines[1]
        );
        assert!(
            lines[2].starts_with(
                "disk check: the gate of mission 9 has had no room to judge for over two hours: \
1.0 GiB free in TMPDIR /tmp, under SPAWN_GATE_MIN_FREE_GB=10."
            ),
            "{}",
            lines[2]
        );
        assert!(items.iter().all(|item| item.why == Why::GateRoom));
        assert_eq!(items[1].tag(), "host gate-room-22");
    }

    /// A gate that gave up waiting for a host scenario slot is short of
    /// room too, and the escalation says what the owner can act on: the
    /// wait, the budget, and what holds the slots.
    #[test]
    fn a_gate_without_a_host_scenario_slot_is_told_with_its_wait_and_the_budget() {
        let records = [
            json!({"type": "task.gated", "verdict": "no_room", "escalate": true,
                              "id": 7, "check": "host scenario slot", "measured": 1_800_000,
                              "threshold": 2, "event_id": 24, "mission": 10}),
        ];
        let items = gate_room_escalations(&records);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].line,
            "host scenario slot check: the gate of mission 10 has had no room to judge for \
over two hours: a scenario run waited 30.0 min for one of SPAWN_GATE_HOST_JOBS=2 scenario \
slots (other gates on this host were running, or a hand-run gate was left waiting). Its work \
waits (no halt counts); find what holds the slots (another mission's gate, an owner gate left \
waiting in its log) and finish or stop it, or raise SPAWN_GATE_HOST_JOBS if this host can \
serve more scenarios or tell the owner."
        );
        assert_eq!(items[0].why, Why::GateRoom);
        assert_eq!(items[0].tag(), "host gate-room-24");
    }

    #[test]
    fn small_480_mib_tmpfs_caps_thresholds_and_recovers_cleanly() {
        let tmp_total = 480 * MIB;
        let effective = THRESHOLDS.effective_for(Some(tmp_total));
        assert_eq!(effective.warn, 96 * MIB);
        assert_eq!(effective.floor, 48 * MIB);
        assert_eq!(recovery_margin_for(Some(tmp_total)), 24 * MIB);

        let tmp = Place {
            label: "tmpdir",
            path: PathBuf::from("/tmp"),
        };
        let healthy = report_place(
            &tmp,
            Some(Space {
                free: 470 * MIB,
                total: tmp_total,
            }),
            THRESHOLDS,
            &json!([]),
        );
        assert_eq!(healthy["free_bytes"], json!(470 * MIB));
        assert_eq!(healthy["total_bytes"], json!(480 * MIB));
        assert_eq!(healthy["warn_bytes"], json!(96 * MIB));
        assert_eq!(healthy["floor_bytes"], json!(48 * MIB));

        let mut monitor = Monitor::with(vec![tmp], THRESHOLDS);
        assert!(
            monitor
                .observe_measured(vec![measured("tmpdir", "/tmp", 470 * MIB, tmp_total)])
                .is_empty(),
            "470 MiB free on a 480 MiB tmpfs must not false-alarm"
        );
        assert!(!monitor.below_floor());

        let low = monitor.observe_measured(vec![measured("tmpdir", "/tmp", 80 * MIB, tmp_total)]);
        assert_eq!(low.len(), 1);
        assert_eq!(low[0]["state"], "low");
        assert_eq!(low[0]["warn_bytes"], json!(96 * MIB));
        assert_eq!(low[0]["floor_bytes"], json!(48 * MIB));
        assert_eq!(low[0]["total_bytes"], json!(480 * MIB));
        assert!(!monitor.below_floor());
        monitor.remember(low[0].clone(), 1, 1);

        assert!(
            monitor
                .observe_measured(vec![measured("tmpdir", "/tmp", 40 * MIB, tmp_total)])
                .is_empty()
        );
        assert!(monitor.below_floor());

        assert!(
            monitor
                .observe_measured(vec![measured("tmpdir", "/tmp", 119 * MIB, tmp_total)])
                .is_empty(),
            "inside the 24 MiB recovery margin above 96 MiB"
        );
        let recovered =
            monitor.observe_measured(vec![measured("tmpdir", "/tmp", 120 * MIB, tmp_total)]);
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0]["state"], "recovered");
        assert_eq!(recovered[0]["warn_bytes"], json!(96 * MIB));
        assert_eq!(recovered[0]["floor_bytes"], json!(48 * MIB));
        assert_eq!(recovered[0]["total_bytes"], json!(480 * MIB));
    }

    #[test]
    fn large_500_gib_disk_keeps_configured_thresholds_unchanged() {
        let large_total = 500 * GIB;
        let effective = THRESHOLDS.effective_for(Some(large_total));
        assert_eq!(effective, THRESHOLDS);
        assert_eq!(recovery_margin_for(Some(large_total)), RECOVERY_MARGIN);

        let ws = place("/ws");
        let report = report_place(
            &ws,
            Some(Space {
                free: 300 * GIB,
                total: large_total,
            }),
            THRESHOLDS,
            &json!([]),
        );
        assert_eq!(report["warn_bytes"], json!(20 * GIB));
        assert_eq!(report["floor_bytes"], json!(2 * GIB));
        assert_eq!(report["total_bytes"], json!(500 * GIB));

        let mut monitor = Monitor::with(vec![ws], THRESHOLDS);
        let low =
            monitor.observe_measured(vec![measured("workspace", "/ws", 19 * GIB, large_total)]);
        assert_eq!(low[0]["state"], "low");
        assert_eq!(low[0]["warn_bytes"], json!(20 * GIB));
        assert_eq!(low[0]["floor_bytes"], json!(2 * GIB));
    }

    #[test]
    fn thin_25_gib_disk_and_tiny_or_disabled_thresholds_behave_safely() {
        assert_eq!(
            THRESHOLDS.effective_for(Some(25 * GIB)),
            Thresholds {
                warn: 5 * GIB,
                floor: 2 * GIB,
            }
        );
        assert_eq!(recovery_margin_for(Some(25 * GIB)), RECOVERY_MARGIN);

        for total in [64 * MIB, 10 * MIB, 2] {
            let eff = THRESHOLDS.effective_for(Some(total));
            let margin = recovery_margin_for(Some(total));
            assert!(eff.floor > 0 && eff.floor <= eff.warn && eff.warn < total);
            assert!(eff.warn.saturating_add(margin) <= total);
        }

        assert_eq!(THRESHOLDS.effective_for(Some(0)), THRESHOLDS);
        assert_eq!(THRESHOLDS.effective_for(None), THRESHOLDS);
        assert_eq!(recovery_margin_for(Some(0)), RECOVERY_MARGIN);
        assert_eq!(recovery_margin_for(None), RECOVERY_MARGIN);

        let zero = Thresholds { warn: 0, floor: 0 };
        assert_eq!(zero.effective_for(Some(480 * MIB)), zero);
    }
}
