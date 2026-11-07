//! Disk room on this host, watched by `spawn loop` (docs/storage.md, "Disk
//! room").
//!
//! Once a minute the loop reads the free space of the workspace root and of
//! `TMPDIR` (the gate's scratch). A path whose free space falls under
//! `SPAWN_DISK_WARN_GB` (default 20 GiB) is recorded once as a `host.disk`
//! record with state `low`, and once more as `recovered` when it is back
//! [`RECOVERY_MARGIN`] above that, so a path hovering at the threshold does
//! not flap. The latest record per path is read back from the log on start,
//! so a restart records nothing twice. The supervisor's digest carries each
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
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use spawn_core::command::CommandAction;
use spawn_postgres::PostgresStore;

use crate::config::env_value;
use crate::dispatch::{Escalation, Order, Why};

const GIB: u64 = 1024 * 1024 * 1024;

/// How often the loop reads free space.
pub(crate) const CHECK_EVERY: Duration = Duration::from_mins(1);

/// How far above the warning threshold a low path must come back before it
/// is recorded as recovered.
pub(crate) const RECOVERY_MARGIN: u64 = GIB;

/// How long a `recovered` record is news for the supervisor's digest: one
/// nobody was told of within a day is stale. A `low` record has no window:
/// it holds until its path recovers.
const RECOVERY_NEWS_MS: i64 = 86_400_000;

/// The thresholds, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Thresholds {
    /// Under this a path is recorded `low`.
    pub(crate) warn: u64,
    /// Under this no new execution starts.
    pub(crate) floor: u64,
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

/// Free bytes on the filesystem that holds `path`, as `df -P` reports them
/// to an unprivileged user.
pub(crate) fn free_bytes(path: &Path) -> Result<u64, String> {
    let output = Command::new("df")
        .arg("-Pk")
        .arg("--")
        .arg(path)
        .output()
        .map_err(|error| format!("df {}: {error}", path.display()))?;
    if !output.status.success() {
        return Err(format!(
            "df {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    parse_df(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| format!("df {}: no free space in its output", path.display()))
}

/// The available KiB column of `df -Pk`'s one line, in bytes.
fn parse_df(text: &str) -> Option<u64> {
    let line = text.lines().nth(1)?;
    let available: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
    Some(available.saturating_mul(1024))
}

/// One reading of one place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reading {
    pub(crate) place: Place,
    pub(crate) free: u64,
}

/// The free space of every place, now; a place that cannot be read is left
/// out and named in the error lines.
pub(crate) fn read(places: &[Place]) -> (Vec<Reading>, Vec<String>) {
    let mut readings = Vec::new();
    let mut errors = Vec::new();
    for place in places {
        match free_bytes(&place.path) {
            Ok(free) => readings.push(Reading {
                place: place.clone(),
                free,
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
/// low: `low` when it falls under the warning threshold, `recovered` when
/// a low path is back [`RECOVERY_MARGIN`] above it, else none.
pub(crate) fn crossing(was_low: bool, reading: &Reading, thresholds: Thresholds) -> Option<Value> {
    let state = if !was_low && reading.free < thresholds.warn {
        "low"
    } else if was_low && reading.free >= thresholds.warn.saturating_add(RECOVERY_MARGIN) {
        "recovered"
    } else {
        return None;
    };
    Some(json!({
        "type": "host.disk",
        "label": reading.place.label,
        "path": reading.place.path.display().to_string(),
        "state": state,
        "free_bytes": reading.free,
        "warn_bytes": thresholds.warn,
        "floor_bytes": thresholds.floor,
    }))
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
/// news: once told, [`crate::dispatch::decide_with`] drops it).
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
/// `SPAWN_WORKSPACE_ROOT`, and `TMPDIR`) its free space now (`null` when it
/// cannot be read), the thresholds, and the state of its latest `host.disk`
/// record (`null` when none).
pub(crate) fn report(records: &Value) -> Value {
    let root = env_value("SPAWN_WORKSPACE_ROOT").map(PathBuf::from);
    report_of(&places(root.as_deref()), Thresholds::from_env(), records)
}

fn report_of(places: &[Place], thresholds: Thresholds, records: &Value) -> Value {
    let places: Vec<Value> = places
        .iter()
        .map(|place| {
            let path = place.path.display().to_string();
            let recorded = records
                .as_array()
                .into_iter()
                .flatten()
                .find(|record| record["path"].as_str() == Some(path.as_str()));
            json!({
                "label": place.label,
                "path": path,
                "free_bytes": free_bytes(&place.path).ok(),
                "warn_bytes": thresholds.warn,
                "floor_bytes": thresholds.floor,
                "recorded": recorded.map(|record| json!({
                    "state": record["state"],
                    "free_bytes": record["free_bytes"],
                    "at_ms": record["at_ms"],
                })),
            })
        })
        .collect();
    Value::Array(places)
}

/// What the loop knows about disk room: the latest record per path (read
/// back from the log once, then kept as it records) and the latest reading.
pub(crate) struct Monitor {
    places: Vec<Place>,
    thresholds: Thresholds,
    checked: Option<Instant>,
    /// The latest `host.disk` record per path, `None` until read from the log.
    records: Option<Vec<Value>>,
    readings: Vec<Reading>,
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
        let (readings, mut lines) = read(&self.places);
        let now = spawn_core::time::Clock::now(&spawn_core::time::SystemClock).0;
        for record in self.observe(readings) {
            let id = store
                .host_record(&record)
                .map_err(|error| format!("disk record: {error}"))?;
            lines.push(format!(
                "disk: {} {} {} free",
                record["path"].as_str().unwrap_or_default(),
                record["state"].as_str().unwrap_or_default(),
                gib(record["free_bytes"].as_u64().unwrap_or_default())
            ));
            self.remember(record, id, now);
        }
        Ok(lines)
    }

    /// Take `readings` as the latest and return the records they make.
    pub(crate) fn observe(&mut self, readings: Vec<Reading>) -> Vec<Value> {
        let records: Vec<Value> = readings
            .iter()
            .filter_map(|reading| {
                crossing(self.was_low(&reading.place.path), reading, self.thresholds)
            })
            .collect();
        self.readings = readings;
        records
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

    /// Whether any place was under the floor at the latest reading: no new
    /// execution starts.
    pub(crate) fn below_floor(&self) -> bool {
        self.readings
            .iter()
            .any(|reading| reading.free < self.thresholds.floor)
    }

    /// The digest items for the latest records.
    pub(crate) fn escalations(&self, now_ms: i64) -> Vec<Escalation> {
        escalations(self.records.as_deref().unwrap_or_default(), now_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        GIB, Monitor, Place, RECOVERY_MARGIN, Reading, Thresholds, escalations,
        gate_room_escalations, hold_new_executions, parse_df, places,
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
    fn df_output_is_read_as_bytes() {
        let text = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                    /dev/sda2 1000 400 600 40% /data\n";
        assert_eq!(parse_df(text), Some(600 * 1024));
        assert_eq!(parse_df("Filesystem\n"), None);
        assert!(super::free_bytes(Path::new("/")).is_ok_and(|free| free > 0));
        assert!(super::free_bytes(Path::new("/no/such/path/at/all")).is_err());
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
        assert_eq!(
            report[0]["recorded"],
            json!({"state": "low", "free_bytes": 5, "at_ms": 9})
        );
        assert_eq!(report[0]["warn_bytes"], json!(20 * GIB));
        assert_eq!(report[1]["free_bytes"], json!(null), "unreadable: null");
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
}
