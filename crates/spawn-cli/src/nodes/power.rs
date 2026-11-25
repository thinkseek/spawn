//! A node's power (issue #91 P1a): whether it runs on AC, its battery's
//! charge, whether it charges, and how long it has left, read by the
//! probes' `@power` section ([`super::INVENTORY`], [`super::BEAT`]) from
//! `pmset -g batt` on macOS and `/sys/class/power_supply/*` on Linux; and
//! the low-battery soft drain built on it.
//!
//! A node with no battery and no AC sensor (a desktop, a VM, a container)
//! answers no power at all: `null`, never a guess. An older node's own copy
//! of the probes says no `@power` either, and its records hold no `power`:
//! every reader takes its absence as `null`.
//!
//! The soft drain: a node on battery (`!on_ac`) at or under
//! `SPAWN_NODE_LOW_BATTERY_PCT` (default [`LOW_BATTERY_PCT`]; `0` turns it
//! off) is `low_battery_drain`, and so `effective_drained`, and
//! [`schedulable`] (the eligibility a scheduler reads; P1 schedules nothing
//! yet) is false for it until it is plugged in or charged above the
//! threshold. It is read, never recorded: the owner's `drain` and
//! `undrain` (`node.drained`, `node.undrained`) are untouched, and a node
//! plugged in again is eligible at once.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// `SPAWN_NODE_LOW_BATTERY_PCT`'s default.
pub(crate) const LOW_BATTERY_PCT: u8 = 20;

/// What a node's power says: on AC or not, its battery's charge (none
/// without a battery), charging or not, and the minutes its battery has
/// left (to empty while discharging, to full while charging; none when
/// the sensor gives no estimate).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PowerTelemetry {
    pub(crate) on_ac: bool,
    pub(crate) battery_pct: Option<u8>,
    pub(crate) charging: bool,
    pub(crate) remaining_mins: Option<u32>,
}

impl PowerTelemetry {
    /// As a record holds it.
    pub(crate) fn json(self) -> Value {
        json!({
            "on_ac": self.on_ac, "battery_pct": self.battery_pct,
            "charging": self.charging, "remaining_mins": self.remaining_mins,
        })
    }

    /// Whether this differs from `last` enough to record a heartbeat before
    /// the ten-minute one is due: AC plugged or unplugged, charging started
    /// or stopped, a battery seen or gone, or the charge moved by
    /// [`MOVED_PCT`] points or more. A soft drain then takes effect within
    /// a probe's interval, not ten minutes later.
    pub(crate) fn moved_from(self, last: Option<Self>) -> bool {
        let Some(last) = last else {
            return true;
        };
        self.on_ac != last.on_ac
            || self.charging != last.charging
            || match (self.battery_pct, last.battery_pct) {
                (Some(now), Some(then)) => now.abs_diff(then) >= MOVED_PCT,
                (None, None) => false,
                _ => true,
            }
    }
}

/// The most minutes a record holds (`spawn_node_record`: at most 9
/// digits): a sensor's larger estimate is clamped to it.
pub(crate) const REMAINING_MINS_MAX: u32 = 999_999_999;

/// Minutes as a record holds them, clamped to [`REMAINING_MINS_MAX`].
fn minutes(mins: u64) -> u32 {
    u32::try_from(mins.min(u64::from(REMAINING_MINS_MAX))).unwrap_or(REMAINING_MINS_MAX)
}

/// The charge, in points, that records a heartbeat early ([`PowerTelemetry::moved_from`]).
const MOVED_PCT: u8 = 5;

/// `pmset -g batt`'s output (macOS):
///
/// ```text
/// Now drawing from 'Battery Power'
///  -InternalBattery-0 (id=4653155)    85%; discharging; 4:32 remaining present: true
/// ```
///
/// `AC Power` on the first line is on AC. A battery line gives its charge,
/// its state (`charging`, `discharging`, `charged`, `finishing charge`,
/// `AC attached; not charging`) and `H:MM remaining` (none for `(no
/// estimate)`). A Mac with no battery (a mini, a Studio) says only the
/// first line: on AC, no battery. No first line at all (no `pmset`) is no
/// power.
pub(crate) fn pmset(lines: &[&str]) -> Option<PowerTelemetry> {
    let source = lines
        .iter()
        .find_map(|line| line.trim().strip_prefix("Now drawing from"))?;
    let on_ac_source = source.contains("AC Power");
    let battery = lines.iter().find(|line| line.contains("InternalBattery"));
    let Some(battery) = battery else {
        return Some(PowerTelemetry {
            on_ac: on_ac_source,
            battery_pct: None,
            charging: false,
            remaining_mins: None,
        });
    };
    // The fields after the battery's name: `85%; discharging; 4:32 remaining`.
    let fields = battery
        .split_once(')')
        .map_or(*battery, |(_, rest)| rest)
        .trim();
    let mut parts = fields.split(';').map(str::trim);
    let battery_pct = parts
        .next()
        .and_then(|pct| pct.strip_suffix('%'))
        .and_then(|pct| pct.trim().parse::<u8>().ok())
        .filter(|pct| *pct <= 100);
    let state = parts.next().unwrap_or_default().to_ascii_lowercase();
    let rest = parts.collect::<Vec<_>>().join(";");
    let charging = state == "charging" || state == "finishing charge";
    let remaining_mins = rest.split_whitespace().find_map(|word| {
        let (hours, minutes) = word.split_once(':')?;
        let hours: u32 = hours.parse().ok()?;
        let minutes: u32 = minutes.parse().ok()?;
        (minutes < 60).then(|| hours.saturating_mul(60).saturating_add(minutes))
    });
    let remaining_mins = remaining_mins.map(|mins| mins.min(REMAINING_MINS_MAX));
    // `0:00 remaining` of a charged battery on AC is no estimate.
    let remaining_mins = remaining_mins.filter(|mins| *mins > 0 || state == "discharging");
    Some(PowerTelemetry {
        on_ac: on_ac_source,
        battery_pct,
        charging,
        remaining_mins,
    })
}

/// One supply of `/sys/class/power_supply` as the probe prints it: a line
/// `supply <name>`, then `<attribute>=<value>` lines.
#[derive(Debug, Default)]
struct Supply<'a> {
    kind: Option<&'a str>,
    online: Option<bool>,
    status: Option<&'a str>,
    capacity: Option<u8>,
    scope: Option<&'a str>,
    time_to_empty_s: Option<u64>,
    time_to_full_s: Option<u64>,
    energy_now: Option<u64>,
    power_now: Option<u64>,
    charge_now: Option<u64>,
    current_now: Option<u64>,
}

/// The Linux probe's `@power` lines (`/sys/class/power_supply/*`):
///
/// ```text
/// supply AC
/// type=Mains
/// online=1
/// supply BAT0
/// type=Battery
/// status=Discharging
/// capacity=57
/// energy_now=24140000
/// power_now=8610000
/// ```
///
/// On AC when a `Mains` or `USB` supply is online; with no such supply,
/// when the battery does not say `Discharging`. The battery is the first
/// system battery (`type=Battery`, not `scope=Device`, a mouse's or a
/// headset's); `remaining_mins` its `time_to_empty_now` (seconds) while
/// discharging, else energy or charge over power or current, and its
/// `time_to_full_now` while charging. No supply but a peripheral's is no
/// power.
pub(crate) fn sysfs(lines: &[&str]) -> Option<PowerTelemetry> {
    let mut supplies: Vec<Supply> = Vec::new();
    for line in lines {
        let line = line.trim();
        if line.strip_prefix("supply ").is_some() {
            supplies.push(Supply::default());
            continue;
        }
        let (Some(supply), Some((name, value))) = (supplies.last_mut(), line.split_once('='))
        else {
            continue;
        };
        let value = value.trim();
        let number = || value.parse::<u64>().ok();
        match name.trim() {
            "type" => supply.kind = Some(value),
            "online" => supply.online = Some(value == "1"),
            "status" => supply.status = Some(value),
            "capacity" => supply.capacity = value.parse::<u8>().ok().filter(|pct| *pct <= 100),
            "scope" => supply.scope = Some(value),
            "time_to_empty_now" => supply.time_to_empty_s = number(),
            "time_to_full_now" => supply.time_to_full_s = number(),
            "energy_now" => supply.energy_now = number(),
            "power_now" => supply.power_now = number(),
            "charge_now" => supply.charge_now = number(),
            "current_now" => supply.current_now = number(),
            _ => {}
        }
    }
    let mains: Vec<&Supply> = supplies
        .iter()
        .filter(|supply| matches!(supply.kind, Some("Mains" | "USB" | "USB_C" | "USB_PD")))
        .collect();
    let battery = supplies
        .iter()
        .find(|supply| supply.kind == Some("Battery") && supply.scope != Some("Device"));
    if mains.is_empty() && battery.is_none() {
        return None;
    }
    let status = battery
        .and_then(|battery| battery.status)
        .unwrap_or_default();
    let discharging = status.eq_ignore_ascii_case("discharging");
    let charging = status.eq_ignore_ascii_case("charging");
    let on_ac = if mains.is_empty() {
        !discharging
    } else {
        mains.iter().any(|supply| supply.online == Some(true))
    };
    let remaining_mins = battery.and_then(|battery| {
        let over = |amount: Option<u64>, rate: Option<u64>| {
            let (amount, rate) = (amount?, rate.filter(|rate| *rate > 0)?);
            Some(minutes(amount.saturating_mul(60) / rate))
        };
        if discharging {
            battery
                .time_to_empty_s
                .filter(|secs| *secs > 0)
                .map(|secs| minutes(secs / 60))
                .or_else(|| over(battery.energy_now, battery.power_now))
                .or_else(|| over(battery.charge_now, battery.current_now))
        } else if charging {
            battery
                .time_to_full_s
                .filter(|secs| *secs > 0)
                .map(|secs| minutes(secs / 60))
        } else {
            None
        }
    });
    Some(PowerTelemetry {
        on_ac,
        battery_pct: battery.and_then(|battery| battery.capacity),
        charging,
        remaining_mins,
    })
}

/// `SPAWN_NODE_LOW_BATTERY_PCT`: the charge at or under which a node on
/// battery is soft-drained, `0` to `100` (`0` turns it off); anything else
/// keeps [`LOW_BATTERY_PCT`] and says why.
pub(crate) fn low_battery_pct(text: Option<&str>) -> (u8, Option<String>) {
    let Some(text) = text else {
        return (LOW_BATTERY_PCT, None);
    };
    match text.trim().parse::<u8>() {
        Ok(pct) if pct <= 100 => (pct, None),
        _ => (
            LOW_BATTERY_PCT,
            Some(format!(
                "SPAWN_NODE_LOW_BATTERY_PCT: {text:?} is not a whole percentage from 0 (off) \
                 to 100; using {LOW_BATTERY_PCT}"
            )),
        ),
    }
}

/// The threshold this process reads (`SPAWN_NODE_LOW_BATTERY_PCT`); a bad
/// value keeps the default and is said on stderr, once per process.
pub(crate) fn low_battery_from_env() -> u8 {
    static SAID: std::sync::Once = std::sync::Once::new();
    let (pct, warning) =
        low_battery_pct(crate::config::env_value("SPAWN_NODE_LOW_BATTERY_PCT").as_deref());
    if let Some(warning) = warning {
        SAID.call_once(|| eprintln!("{warning}"));
    }
    pct
}

/// Whether a node's `power` (as a record or a read holds it, `null` or
/// absent for none) puts it in the soft drain at `threshold`: on battery
/// and at or under it. `0` never does; no power, or no charge read, never
/// does.
pub(crate) fn low_battery(power: &Value, threshold: u8) -> bool {
    threshold > 0
        && power["on_ac"] == false
        && power["battery_pct"]
            .as_u64()
            .is_some_and(|pct| pct <= u64::from(threshold))
}

/// Whether work may be scheduled on `node` (one of the store's node list,
/// [`with_soft_drain`] or not): it answers (`live`, so neither lost nor
/// drained by the owner) and is not in the low-battery soft drain.
pub(crate) fn schedulable(node: &Value, threshold: u8) -> bool {
    node["state"] == "live" && !low_battery(&node["power"], threshold)
}

/// Each node of `nodes` (the store's list) with `low_battery_drain` (in
/// the soft drain at `threshold`), `effective_drained` (drained by the
/// owner, or soft-drained) and `schedulable` ([`schedulable`]); its
/// `state` and the owner's drain are left as they are.
pub(crate) fn with_soft_drain(nodes: &mut Value, threshold: u8) {
    for node in nodes.as_array_mut().into_iter().flatten() {
        let low = low_battery(&node["power"], threshold);
        let drained = node["state"] == "drained";
        let eligible = schedulable(node, threshold);
        if let Some(fields) = node.as_object_mut() {
            fields.insert("low_battery_drain".to_string(), json!(low));
            fields.insert("effective_drained".to_string(), json!(drained || low));
            fields.insert("schedulable".to_string(), json!(eligible));
        }
    }
}

#[cfg(test)]
mod tests;
