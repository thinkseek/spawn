//! The power readers and the low-battery soft drain, each without a
//! machine: `pmset -g batt` outputs as macOS prints them, the Linux
//! probe's `/sys/class/power_supply` lines, and the drain's rule over the
//! store's node list.

use super::{
    LOW_BATTERY_PCT, PowerTelemetry, low_battery, low_battery_pct, pmset, schedulable, sysfs,
    with_soft_drain,
};
use serde_json::{Value, json};

fn lines(text: &str) -> Vec<&str> {
    text.lines().collect()
}

fn power(on_ac: bool, pct: Option<u8>, charging: bool, mins: Option<u32>) -> PowerTelemetry {
    PowerTelemetry {
        on_ac,
        battery_pct: pct,
        charging,
        remaining_mins: mins,
    }
}

#[test]
fn pmset_reads_every_state_macos_prints() {
    for (text, expected) in [
        (
            "Now drawing from 'Battery Power'\n -InternalBattery-0 (id=4653155)\t85%; discharging; 4:32 remaining present: true",
            power(false, Some(85), false, Some(272)),
        ),
        (
            "Now drawing from 'AC Power'\n -InternalBattery-0 (id=4653155)\t57%; charging; 1:20 remaining present: true",
            power(true, Some(57), true, Some(80)),
        ),
        (
            "Now drawing from 'AC Power'\n -InternalBattery-0 (id=4653155)\t100%; charged; 0:00 remaining present: true",
            power(true, Some(100), false, None),
        ),
        (
            "Now drawing from 'AC Power'\n -InternalBattery-0 (id=4653155)\t80%; AC attached; not charging present: true",
            power(true, Some(80), false, None),
        ),
        (
            "Now drawing from 'Battery Power'\n -InternalBattery-0 (id=4653155)\t64%; discharging; (no estimate) present: true",
            power(false, Some(64), false, None),
        ),
        (
            "Now drawing from 'AC Power'\n -InternalBattery-0 (id=4653155)\t98%; finishing charge; 0:10 remaining present: true",
            power(true, Some(98), true, Some(10)),
        ),
        (
            "Now drawing from 'AC Power'",
            power(true, None, false, None),
        ),
    ] {
        assert_eq!(pmset(&lines(text)), Some(expected), "{text}");
    }
    assert_eq!(pmset(&[]), None, "no pmset, no power");
    assert_eq!(pmset(&lines("pmset: command not found")), None);
}

#[test]
fn sysfs_reads_mains_and_the_system_battery_only() {
    let laptop =
        "supply AC\ntype=Mains\nonline=1\nsupply BAT0\ntype=Battery\nstatus=Full\ncapacity=100";
    assert_eq!(
        sysfs(&lines(laptop)),
        Some(power(true, Some(100), false, None))
    );
    // No Mains supply: the battery's status says whether it is on AC.
    let bare = "supply BAT1\ntype=Battery\nstatus=Discharging\ncapacity=12\ntime_to_empty_now=1500";
    assert_eq!(
        sysfs(&lines(bare)),
        Some(power(false, Some(12), false, Some(25)))
    );
    // Charge over current when no energy is read.
    let charge = "supply BAT0\ntype=Battery\nstatus=Discharging\ncapacity=40\ncharge_now=2000000\ncurrent_now=1000000";
    assert_eq!(
        sysfs(&lines(charge)),
        Some(power(false, Some(40), false, Some(120)))
    );
    // An AC adapter alone (a mini PC on a brick): on AC, no battery.
    assert_eq!(
        sysfs(&lines("supply ADP1\ntype=Mains\nonline=1")),
        Some(power(true, None, false, None))
    );
    assert_eq!(sysfs(&[]), None, "a desktop or VM has no supply");
    assert_eq!(
        sysfs(&lines(
            "supply hid-mouse\ntype=Battery\nscope=Device\ncapacity=3"
        )),
        None,
        "a mouse's battery is not the node's"
    );
    // A capacity out of range is no charge.
    assert_eq!(
        sysfs(&lines(
            "supply BAT0\ntype=Battery\nstatus=Discharging\ncapacity=250"
        )),
        Some(power(false, None, false, None))
    );
}

#[test]
fn a_moved_power_is_a_plug_a_charge_state_or_five_points() {
    let now = power(false, Some(50), false, None);
    assert!(now.moved_from(None));
    assert!(!now.moved_from(Some(power(false, Some(53), false, Some(9)))));
    assert!(now.moved_from(Some(power(false, Some(55), false, None))));
    assert!(now.moved_from(Some(power(true, Some(50), false, None))));
    assert!(now.moved_from(Some(power(false, Some(50), true, None))));
    assert!(now.moved_from(Some(power(false, None, false, None))));
}

#[test]
fn the_threshold_is_a_whole_percentage_and_zero_turns_it_off() {
    assert_eq!(low_battery_pct(None), (LOW_BATTERY_PCT, None));
    assert_eq!(LOW_BATTERY_PCT, 20);
    assert_eq!(low_battery_pct(Some("0")), (0, None));
    assert_eq!(low_battery_pct(Some(" 35 ")), (35, None));
    assert_eq!(low_battery_pct(Some("100")), (100, None));
    for bad in ["101", "-1", "x", "12.5", ""] {
        let (pct, warning) = low_battery_pct(Some(bad));
        assert_eq!(pct, LOW_BATTERY_PCT, "{bad}");
        assert!(
            warning
                .expect("said")
                .contains("SPAWN_NODE_LOW_BATTERY_PCT"),
            "{bad}"
        );
    }
}

#[test]
fn a_node_on_battery_at_or_under_the_threshold_is_soft_drained() {
    let on_battery = |pct: u64| {
        json!({"on_ac": false, "battery_pct": pct, "charging": false,
                                       "remaining_mins": null})
    };
    assert!(low_battery(&on_battery(20), 20));
    assert!(low_battery(&on_battery(3), 20));
    assert!(!low_battery(&on_battery(21), 20));
    assert!(!low_battery(&on_battery(5), 0), "0 turns it off");
    assert!(!low_battery(&Value::Null, 20), "no power, no drain");
    assert!(!low_battery(
        &json!({"on_ac": true, "battery_pct": 5, "charging": true, "remaining_mins": null}),
        20
    ));
    assert!(!low_battery(
        &json!({"on_ac": false, "battery_pct": null, "charging": false, "remaining_mins": null}),
        20
    ));
}

#[test]
fn the_soft_drain_marks_each_node_and_leaves_the_owners_drain_alone() {
    let mut nodes = json!([
        {"name": "mac24", "state": "live",
         "power": {"on_ac": false, "battery_pct": 15, "charging": false, "remaining_mins": 30}},
        {"name": "laptop", "state": "live",
         "power": {"on_ac": true, "battery_pct": 15, "charging": true, "remaining_mins": 60}},
        {"name": "desktop", "state": "live", "power": null},
        {"name": "held", "state": "drained", "power": null},
        {"name": "gone", "state": "lost",
         "power": {"on_ac": false, "battery_pct": 90, "charging": false, "remaining_mins": null}},
    ]);
    with_soft_drain(&mut nodes, 20);
    let read = |field: &str| -> Vec<Value> {
        nodes
            .as_array()
            .expect("a list")
            .iter()
            .map(|node| node[field].clone())
            .collect()
    };
    assert_eq!(
        read("low_battery_drain"),
        [true, false, false, false, false].map(Value::from)
    );
    assert_eq!(
        read("effective_drained"),
        [true, false, false, true, false].map(Value::from)
    );
    assert_eq!(
        read("schedulable"),
        [false, true, true, false, false].map(Value::from)
    );
    assert_eq!(
        read("state"),
        ["live", "live", "live", "drained", "lost"].map(Value::from),
        "the state is the store's, untouched"
    );
    // Off: nobody is soft-drained.
    let mut off = nodes.clone();
    with_soft_drain(&mut off, 0);
    assert_eq!(off[0]["low_battery_drain"], false);
    assert_eq!(off[0]["effective_drained"], false);
    assert!(schedulable(&off[0], 0));
}

/// Review L5: a sensor's estimate past what a record holds (9 digits of
/// minutes) is clamped, never refused by the store.
#[test]
fn a_huge_remaining_estimate_is_clamped_to_what_a_record_holds() {
    let huge = "Now drawing from 'Battery Power'\n -InternalBattery-0 (id=1)\t50%; discharging; 99999999:00 remaining present: true";
    assert_eq!(
        pmset(&lines(huge)).and_then(|power| power.remaining_mins),
        Some(super::REMAINING_MINS_MAX)
    );
    let sysfs_huge = "supply BAT0\ntype=Battery\nstatus=Discharging\ncapacity=50\ntime_to_empty_now=18446744073709551615";
    assert_eq!(
        sysfs(&lines(sysfs_huge)).and_then(|power| power.remaining_mins),
        Some(super::REMAINING_MINS_MAX)
    );
    let rate = "supply BAT0\ntype=Battery\nstatus=Discharging\ncapacity=50\nenergy_now=18446744073709551615\npower_now=1";
    assert_eq!(
        sysfs(&lines(rate)).and_then(|power| power.remaining_mins),
        Some(super::REMAINING_MINS_MAX)
    );
    assert_eq!(super::REMAINING_MINS_MAX, 999_999_999);
}
