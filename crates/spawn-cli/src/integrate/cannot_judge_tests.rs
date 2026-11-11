use super::{CANNOT_JUDGE, cannot_judge_fields, halt_reason, host_fault_backoff, host_fault_base};
use serde_json::json;

fn fields(faults: u32) -> serde_json::Value {
    cannot_judge_fields(
        "029.sh",
        faults,
        host_fault_backoff(host_fault_base(None), faults),
        "the gate's log",
    )
}

/// The record: the evidence, the scenario, the shared host-fault count,
/// the backoff and escalation (the second in a row, as any host fault's).
/// Never a rejection, never an abort.
#[test]
fn the_record_names_the_scenario_and_the_host_fault() {
    let record = fields(1);
    assert_eq!(record["evidence"], CANNOT_JUDGE);
    assert_eq!(record["evidence"], "cannot_judge");
    assert_eq!(record["scenario"], "029.sh");
    assert_eq!(record["host_faults"], 1);
    assert_eq!(record["backoff_ms"], 300_000);
    assert_eq!(record["escalate"], false);
    assert_eq!(record["log"], "the gate's log");
    assert!(record.get("due_ms").is_none(), "the store writes it");
    assert_eq!(
        record["reason"],
        "host fault: this host cannot judge 029.sh (the gate's log gives no cause); not a \
             verdict on the work, judged again in 5 min",
        "a log with no line of the scenario's names no cause"
    );
    assert_eq!(record["cause"], serde_json::Value::Null);
    assert!(record.get("candidate").is_none(), "the caller adds it");
    let second = fields(2);
    assert_eq!(second["escalate"], true, "the second in a row, as any");
    assert_eq!(second["backoff_ms"], 600_000);
}

/// The cause is the scenario's own line, quoted from its block in the
/// gate's log: 029's systemd user manager, or Supply PA 073's
/// PostgreSQL server — never one scenario's cause for another.
#[test]
fn the_reason_quotes_the_scenario_s_own_cannot_judge_line() {
    let line_073 = "073: cannot judge: SPAWN_TEST_DATABASE_URL names no PostgreSQL server, \
                        and the race needs two connections (the gate starts one from \
                        SPAWN_GATE_PG_BIN or SPAWN_GATE_PG_IMAGE)";
    let log = format!(
        "| server: none\n\
             | == candidate: 044.sh must pass\n\
             | 044: ok\n\
             | == candidate: 073.sh must pass\n\
             | 073: starting\n\
             | {line_073}\n\
             | == candidate: 075.sh must pass\n\
             | 075: cannot judge: something else entirely\n"
    );
    let record = cannot_judge_fields("073.sh", 1, std::time::Duration::from_mins(5), &log);
    assert_eq!(record["cause"], line_073);
    let reason = record["reason"].as_str().expect("a reason");
    assert!(reason.contains(&format!("it says: {line_073}")), "{reason}");
    assert!(!reason.contains("systemd"), "never 029's cause: {reason}");
    assert!(
        !reason.contains("something else"),
        "never another block's: {reason}"
    );

    // 029's own line, on the gate's stderr (the integrator's `! `).
    let log = "| == candidate: 029.sh must pass\n\
                   ! 029: cannot be judged here: no systemd user manager is reachable\n";
    let record = cannot_judge_fields("029.sh", 1, std::time::Duration::from_mins(5), log);
    assert_eq!(
        record["cause"],
        "029: cannot be judged here: no systemd user manager is reachable"
    );

    // The last line of the block wins; the log's `| ` is not quoted.
    assert_eq!(
        super::cannot_judge_cause(
            "073.sh",
            "| == base: 073.sh\n| 073: cannot judge: first\n| 073: cannot judge: last\n"
        )
        .as_deref(),
        Some("073: cannot judge: last")
    );
    // The gate's own stage line in the scenario's base block is no
    // cause: a candidate that exited 6 saying nothing has none.
    let log = "| == base: 073.sh\n\
                   | stage gate cannot judge 120ms base: 073.sh\n\
                   | == candidate: 073.sh must pass\n";
    let record = cannot_judge_fields("073.sh", 1, std::time::Duration::from_mins(5), log);
    assert_eq!(record["cause"], serde_json::Value::Null, "{record}");
    assert!(
        record["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("the gate's log gives no cause")),
        "{record}"
    );
    // Another scenario's line is no cause of this one's.
    assert_eq!(
        super::cannot_judge_cause(
            "073.sh",
            "| == base: 029.sh\n| 029: cannot be judged here: x\n"
        ),
        None
    );
}

/// No halt rule counts it, however many in a row.
#[test]
fn no_number_of_cannot_judge_host_faults_halts_a_mission() {
    for count in [1, 2, 3, 10, 1000, u32::MAX] {
        let mission: super::Mission = serde_json::from_value(json!({
            "id": 1, "gate": "/g", "accept": "gate", "gate_rejections": 2,
            "gate_aborts": 2, "gate_host_faults": count,
        }))
        .expect("a mission");
        assert_eq!(halt_reason(&mission, 7), None, "{count}");
    }
}
