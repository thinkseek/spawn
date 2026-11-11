use std::time::Duration;

use serde_json::json;

use super::{Mission, halt_reason, host_fault_backoff, host_fault_base, host_wait};

fn fields(
    scenario: &str,
    setup_s: Option<u64>,
    faults: u32,
    disagreements: u32,
) -> serde_json::Value {
    super::base_disagrees_fields(
        scenario,
        [1, 0],
        setup_s,
        faults,
        disagreements,
        host_fault_backoff(host_fault_base(None), faults),
        "the gate's log",
    )
}

/// The record: the evidence, the scenario, both exits, the setup's
/// seconds when the gate printed a timing line (left out when it did
/// not), the shared count, its own count, its backoff and its alert.
#[test]
fn the_record_names_the_scenario_its_exits_and_the_setup() {
    let record = fields("011.sh", Some(244), 1, 1);
    assert_eq!(record["evidence"], "base_disagrees");
    assert_eq!(record["scenario"], "011.sh");
    assert_eq!(record["exits"], json!([1, 0]));
    assert_eq!(record["setup_s"], 244);
    assert_eq!(record["host_faults"], 1);
    assert_eq!(record["base_disagrees"], 1);
    assert_eq!(record["backoff_ms"], 300_000);
    assert_eq!(record["escalate"], false);
    assert_eq!(record["alert"], false);
    assert_eq!(record["log"], "the gate's log");
    assert!(record.get("due_ms").is_none(), "the store writes it");
    assert_eq!(
        record["reason"],
        "host fault: the base disagrees with itself: 011.sh exited 1, then 0 on the base, \
             after a setup of 244 s; not a verdict, judged again later"
    );

    // No timing line: the field is left out, not null.
    let record = fields("011.sh", None, 1, 1);
    assert!(record.get("setup_s").is_none(), "{record}");
    assert_eq!(
        record["reason"],
        "host fault: the base disagrees with itself: 011.sh exited 1, then 0 on the base; \
             not a verdict, judged again later"
    );

    // The record keeps no `candidate` of its own: the caller adds it.
    assert!(record.get("candidate").is_none());
}

/// A base disagreement never escalates, at any count: the record that
/// makes the mission's run 3 carries the alert, and every other one
/// (the later ones of the same run too) does not.
#[test]
fn a_base_disagrees_never_escalates_and_alerts_at_the_third() {
    let alerts: Vec<(bool, bool)> = (1..=6)
        .map(|disagreements| {
            let record = fields("011.sh", None, disagreements, disagreements);
            (
                record["escalate"].as_bool().expect("escalate"),
                record["alert"].as_bool().expect("alert"),
            )
        })
        .collect();
    assert_eq!(
        alerts,
        [
            (false, false),
            (false, false),
            (false, true),
            (false, false),
            (false, false),
            (false, false),
        ]
    );
    // 020's evidence still escalates at the second in a row.
    let fault = super::host::host_fault(
        super::host::CHECK_TESTS_REGRESSION,
        "| == regression: check-tests.sh must still pass\n\
             | FAILED api::tests::one: error connecting to server: Connection refused (os error \
             111)\n",
        Vec::new,
    )
    .expect("a host fault by its failures");
    let escalations: Vec<bool> = (1..=4)
        .map(|faults| {
            super::host_fault_fields(&fault, faults, Duration::from_secs(5), "")["escalate"]
                .as_bool()
                .expect("escalate")
        })
        .collect();
    assert_eq!(escalations, [false, true, false, false]);
}

/// Both evidences share 020's run and its backoff: five minutes,
/// doubling with each host fault in a row whatever its evidence, at most
/// an hour.
#[test]
fn both_evidences_share_the_run_and_its_backoff() {
    let base = host_fault_base(None);
    let minutes: Vec<u64> = (0..=7)
        .map(|faults| host_fault_backoff(base, faults).as_secs() / 60)
        .collect();
    assert_eq!(minutes, [0, 5, 10, 20, 40, 60, 60, 60]);
    assert_eq!(
        host_fault_backoff(Duration::from_secs(4), 3),
        Duration::from_secs(16),
        "the scenario's four seconds, doubled twice"
    );
    assert_eq!(
        host_fault_backoff(base, u32::MAX),
        Duration::from_hours(1),
        "no overflow"
    );
}

/// The hold until the recorded `due_ms` is 020's: a base disagreement
/// folds into the same run (`gate_host_faults`, `gate_host_task`,
/// `gate_host_due_ms`), so the task waits as behind any host fault.
#[test]
fn a_base_disagreement_holds_its_task_until_its_due_time() {
    let due = 10_000_000;
    let waiting: Mission = serde_json::from_value(json!({
        "id": 1, "gate": "/g", "accept": "gate",
        "gate_host_faults": 2, "gate_host_task": 4, "gate_host_due_ms": due,
        "gate_base_disagrees": 2,
    }))
    .expect("a mission");
    assert_eq!(
        host_wait(&waiting, 4, due - 8_000),
        Some(Duration::from_secs(8))
    );
    assert_eq!(host_wait(&waiting, 4, due), None, "due: judged again");
    assert_eq!(host_wait(&waiting, 5, 0), None, "another task's");
}

/// No halt rule counts a base disagreement, however many in a row.
#[test]
fn no_number_of_base_disagreements_halts_a_mission() {
    for count in [1, 2, 3, 10, 1000, u32::MAX] {
        let mission: Mission = serde_json::from_value(json!({
            "id": 1, "gate": "/g", "accept": "gate", "gate_rejections": 2,
            "gate_aborts": 2, "gate_host_faults": count, "gate_base_disagrees": count,
        }))
        .expect("a mission");
        assert_eq!(halt_reason(&mission, 7), None, "{count}");
    }
}
