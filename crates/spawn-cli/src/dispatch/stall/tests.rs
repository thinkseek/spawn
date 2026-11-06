//! Each step from task rows, records and facts; queues; the bound's edges;
//! what moves a tag and what does not; the line.

use std::collections::BTreeSet;

use serde_json::json;
use spawn_core::event::{ModelProfile, SessionSeq};
use spawn_core::observation::{Ambiguity, Confidence, Observation, SemanticState};

use super::{Bounds, Stalls, items};
use crate::deliver::Work;
use crate::dispatch::{Escalation, Facts, Sent, Why, decide_with};

/// The database's clock in every case.
const NOW: i64 = 50_000_000;

const SESSION: &str = "session_resident_core";

fn bounds() -> Bounds {
    Bounds::default()
}

fn observation(state: SemanticState, idle: bool, seq: u64) -> Observation {
    let mut observation = Observation::basic(
        state,
        None,
        SessionSeq(seq),
        Confidence::new(0.9).expect("valid"),
        "rules",
    );
    if idle {
        observation.ambiguity = Some(Ambiguity::TurnStoppedIdle);
    }
    observation
}

/// A live resident, judged on its latest fact (`seq` 20).
fn worker(state: SemanticState, idle: bool) -> Facts {
    Facts {
        session_id: SESSION.to_string(),
        agent_id: "agent_resident_core".to_string(),
        live: true,
        execution_id: Some("exec_3".to_string()),
        end_cause: None,
        observation: Some(observation(state, idle, 20)),
        observation_event_id: Some(9),
        observation_at_ms: Some(NOW - 1_000),
        evidence_seq: Some(20),
        turn_ended: idle,
        checkpoint: None,
        episode: Some(4),
        stops: Vec::new(),
        open_command: false,
        dispatched: Vec::new(),
        told: BTreeSet::new(),
        last_delivery: None,
        model: ModelProfile::Default,
        resumed_young: 0,
    }
}

fn supervisor(told: &[String]) -> Facts {
    Facts {
        session_id: "session_supervisor".to_string(),
        agent_id: "agent_supervisor".to_string(),
        told: told.iter().cloned().collect(),
        ..worker(SemanticState::Waiting, true)
    }
}

/// A task row as `delivery_rows` returns it.
fn row(id: i64, state: &str, session: &str) -> serde_json::Value {
    json!({
        "id": id, "mission_id": 1, "kind": "resident",
        "agent_id": session.replace("session_", "agent_"), "session_id": session,
        "brief": format!("brief {id}"), "state": state, "revisions": 0,
        "mission_text": "m", "resident": "core", "charter": "c",
    })
}

fn work(rows: &[serde_json::Value]) -> Work {
    Work {
        tasks: serde_json::from_value(json!(rows)).expect("task rows"),
        ..Work::default()
    }
}

/// An open task as `stall_facts` returns it, its step's record `ago_ms`
/// before [`NOW`].
fn open(id: i64, state: &str, record: (i64, &str, i64)) -> serde_json::Value {
    let (record_id, kind, ago_ms) = record;
    json!({
        "id": id, "mission_id": 1, "kind": "resident", "session_id": SESSION,
        "state": state, "revisions": 0, "gated": false, "accept": "review", "halted": false,
        "record": {"id": record_id, "type": kind, "at_ms": NOW - ago_ms, "age_ms": ago_ms},
    })
}

fn stalls(value: serde_json::Value) -> Stalls {
    let mut value = value;
    value["now_ms"] = json!(NOW);
    serde_json::from_value(value).expect("stall facts")
}

fn failed(id: i64, key: &str, ago_ms: i64, error: &str) -> Sent {
    Sent {
        id: Some(id),
        key: key.to_string(),
        text: None,
        created_at_ms: NOW - ago_ms - 500,
        finished_at_ms: Some(NOW - ago_ms),
        succeeded: Some(false),
        error: Some(error.to_string()),
    }
}

const REFUSED: &str = "submit text: paste-buffer failed: paste-buffer refused by the scenario; \
                       the text may have been typed and the log cannot tell; it is not typed again";

const BUSY: &str = "pane state Busy does not accept input";

fn only(items: &[Escalation]) -> &Escalation {
    assert_eq!(items.len(), 1, "{items:#?}");
    &items[0]
}

// --- each step -------------------------------------------------------------

#[test]
fn an_assigned_task_its_sessions_turn_is_at_delivery_from_its_assignment() {
    let facts = [worker(SemanticState::Waiting, true)];
    let tasks = work(&[row(7, "assigned", SESSION)]);
    let found = stalls(json!({"tasks": [open(7, "assigned", (120, "task.assigned", 600_000))]}));
    let item = only(&items(&facts, &tasks, &found, &bounds())).clone();
    assert_eq!(item.why, Why::Stalled);
    assert_eq!(item.tag(), format!("{SESSION} stall-7-delivery-120"));
    assert!(
        item.line
            .starts_with("task #7 (mission #1) has not moved from step delivery for 10m00s")
    );
    assert!(
        item.line.contains("task.assigned (record 120, 10m00s ago)"),
        "{}",
        item.line
    );
}

#[test]
fn a_task_sent_back_is_at_delivery_from_the_review_that_sent_it_back() {
    let facts = [worker(SemanticState::Waiting, true)];
    let tasks = work(&[row(7, "revise", SESSION)]);
    let found = stalls(json!({"tasks": [open(7, "revise", (130, "task.reviewed", 600_000))]}));
    let item = only(&items(&facts, &tasks, &found, &bounds())).clone();
    assert_eq!(item.id, "stall-7-delivery-130");
}

#[test]
fn a_delivered_task_is_at_activity_from_the_later_of_its_delivery_and_the_latest_fact() {
    let facts = [worker(SemanticState::Working, false)];
    let tasks = work(&[row(7, "delivered", SESSION)]);
    // The latest fact came after the delivery record: measured from it.
    let found = stalls(json!({
        "tasks": [open(7, "delivered", (140, "task.delivered", 1_000_000))],
        "sessions": [{"session_id": SESSION, "seq": 20, "event_name": "tool.completed",
                      "at_ms": NOW - 900_000, "screen": "✽ Working…"}],
    }));
    let item = only(&items(&facts, &tasks, &found, &bounds())).clone();
    assert_eq!(item.id, "stall-7-activity-20");
    assert!(item.line.contains("for 15m00s"), "{}", item.line);
    assert!(
        item.line.contains("tool.completed (seq 20"),
        "{}",
        item.line
    );
    // No fact after the delivery record: measured from the record.
    let found = stalls(json!({
        "tasks": [open(7, "delivered", (140, "task.delivered", 900_000))],
        "sessions": [{"session_id": SESSION, "seq": 20, "event_name": "message.received",
                      "at_ms": NOW - 901_000}],
    }));
    assert_eq!(
        only(&items(&facts, &tasks, &found, &bounds())).id,
        "stall-7-activity-140"
    );
}

#[test]
fn a_worker_idle_at_its_prompt_with_no_report_is_at_activity_too() {
    let facts = [worker(SemanticState::Waiting, true)];
    let tasks = work(&[row(7, "delivered", SESSION)]);
    let found = stalls(json!({
        "tasks": [open(7, "delivered", (140, "task.delivered", 950_000))],
        "sessions": [{"session_id": SESSION, "seq": 20, "event_name": "turn.stopped",
                      "at_ms": NOW - 900_000, "screen": "❯"}],
    }));
    assert_eq!(
        only(&items(&facts, &tasks, &found, &bounds())).id,
        "stall-7-activity-20"
    );
}

/// Task 33: a resident ran clippy, the tests and the e2e in background
/// shells for over 15 minutes, at its prompt, with no events. Its screen
/// says the shells still run: that is activity.
#[test]
fn a_worker_whose_screen_shows_background_shells_running_is_not_stalled() {
    let facts = [worker(SemanticState::Waiting, true)];
    let tasks = work(&[row(7, "delivered", SESSION)]);
    let found = stalls(json!({
        "tasks": [open(7, "delivered", (140, "task.delivered", 2_000_000))],
        "sessions": [{"session_id": SESSION, "seq": 20, "event_name": "screen.observed",
                      "at_ms": NOW - 1_900_000,
                      "screen": "✻ Sautéed for 6s · done 11:27 PM · 3 shells still running"}],
    }));
    assert!(items(&facts, &tasks, &found, &bounds()).is_empty());
}

#[test]
fn a_report_under_review_is_at_judgement_from_its_report() {
    let facts = [worker(SemanticState::Waiting, true)];
    let tasks = work(&[row(7, "reported", SESSION)]);
    let found = stalls(json!({"tasks": [open(7, "reported", (150, "task.reported", 1_800_000))]}));
    let item = only(&items(&facts, &tasks, &found, &bounds())).clone();
    assert_eq!(item.id, "stall-7-judgement-150");
    assert!(
        item.line.contains("the report item is not sent yet"),
        "{}",
        item.line
    );
    // Told: the item says so.
    let told = [
        worker(SemanticState::Waiting, true),
        supervisor(&[format!("{SESSION} report-7-0")]),
    ];
    let item = only(&items(&told, &tasks, &found, &bounds())).clone();
    assert!(
        item.line.contains("the report item was sent"),
        "{}",
        item.line
    );
}

/// A gated task the integrator judges next waits from the later of its
/// readiness and the mission's latest verdict that is not `stale`; a stale
/// verdict is evidence and moves nothing. The others are queued behind it.
#[test]
fn only_the_task_the_gate_judges_next_is_at_judgement_and_a_stale_verdict_moves_nothing() {
    let gated = |id: i64, record: (i64, &str, i64)| {
        let mut task = open(id, "accepted", record);
        task["gated"] = json!(true);
        task["stale"] = json!({"id": 400, "reason": "the push lags", "at_ms": NOW - 60_000});
        task
    };
    let facts = [worker(SemanticState::Waiting, true)];
    let tasks = work(&[]);
    let found = stalls(json!({
        "tasks": [gated(8, (160, "task.reviewed", 4_000_000)),
                  gated(9, (170, "task.reviewed", 3_900_000))],
        "verdicts": [{"mission_id": 1, "id": 180, "verdict": "accepted", "task": "5",
                      "at_ms": NOW - 3_700_000}],
    }));
    let item = only(&items(&facts, &tasks, &found, &bounds())).clone();
    assert_eq!(
        item.id, "stall-8-judgement-180",
        "measured from the mission's verdict"
    );
    assert!(item.line.contains("the push lags"), "{}", item.line);
    assert!(item.line.contains("accepted on task #5"), "{}", item.line);
    // Under the gate's bound since that verdict: nothing.
    let found = stalls(json!({
        "tasks": [gated(8, (160, "task.reviewed", 4_000_000))],
        "verdicts": [{"mission_id": 1, "id": 180, "verdict": "accepted", "task": "5",
                      "at_ms": NOW - 3_000_000}],
    }));
    assert!(items(&facts, &tasks, &found, &bounds()).is_empty());
}

#[test]
fn a_landing_task_is_at_landing_and_a_closed_ephemeral_worker_still_running_at_stop() {
    let facts = [worker(SemanticState::Waiting, true)];
    let tasks = work(&[]);
    let found = stalls(json!({"tasks": [open(7, "landing", (190, "task.gated", 600_000))]}));
    assert_eq!(
        only(&items(&facts, &tasks, &found, &bounds())).id,
        "stall-7-landing-190"
    );

    let mut closed = open(7, "cancelled", (200, "task.cancelled", 300_000));
    closed["kind"] = json!("ephemeral");
    let found = stalls(json!({"tasks": [closed.clone()]}));
    let mut running = worker(SemanticState::Working, false);
    running
        .dispatched
        .push(failed(41, "retire-task-7", 200_000, "kill-session refused"));
    let item = only(&items(&[running.clone()], &tasks, &found, &bounds())).clone();
    assert_eq!(item.id, "stall-7-stop-200");
    assert!(
        item.line
            .contains("`retire-task-7` failed (kill-session refused)"),
        "{}",
        item.line
    );
    // Its worker gone: nothing to stop.
    running.live = false;
    assert!(items(&[running], &tasks, &found, &bounds()).is_empty());
}

/// One escalation per halt, about the mission, naming the reason and its
/// open tasks; a new halt is a new record and a new tag.
#[test]
fn a_halted_mission_is_escalated_once_per_halt_record() {
    let halted = |record: i64| {
        stalls(
            json!({"halted": [{"mission_id": 3, "reason": "3 gate rejections in a row",
                                   "record": {"id": record, "type": "mission.halted",
                                              "at_ms": NOW - 1_000},
                                   "tasks": [11, 12]}]}),
        )
    };
    let item = only(&items(&[], &work(&[]), &halted(500), &bounds())).clone();
    assert_eq!(item.tag(), "missions stall-mission-3-halted-500");
    assert!(
        item.line.contains("3 gate rejections in a row"),
        "{}",
        item.line
    );
    assert!(item.line.contains("#11, #12"), "{}", item.line);
    // Told once, it is not pending again; a new halt is.
    let host = items(&[], &work(&[]), &halted(500), &bounds());
    let rows = [supervisor(&[item.tag()])];
    let plan = decide_with(&rows, &Work::default(), &host, false, NOW, "spawn");
    assert!(plan.pending.is_empty(), "{:?}", plan.pending);
    let host = items(&[], &work(&[]), &halted(530), &bounds());
    let plan = decide_with(&rows, &Work::default(), &host, false, NOW, "spawn");
    assert_eq!(plan.pending.len(), 1);
    assert_eq!(plan.pending[0].id, "stall-mission-3-halted-530");
}

// --- queues ------------------------------------------------------------------

#[test]
fn a_task_queued_behind_another_of_its_session_is_not_escalated() {
    let facts = [worker(SemanticState::Working, false)];
    // Task 8 waits behind delivered task 7; task 9 behind task 8.
    let tasks = work(&[
        row(7, "delivered", SESSION),
        row(8, "assigned", SESSION),
        row(9, "assigned", SESSION),
    ]);
    let found = stalls(json!({
        "tasks": [open(7, "delivered", (100, "task.delivered", 60_000)),
                  open(8, "assigned", (110, "task.assigned", 9_000_000)),
                  open(9, "assigned", (120, "task.assigned", 9_000_000))],
        "sessions": [{"session_id": SESSION, "seq": 20, "event_name": "tool.started",
                      "at_ms": NOW - 5_000}],
    }));
    assert!(items(&facts, &tasks, &found, &bounds()).is_empty());
    // With 7 closed, 8 is the session's turn and 9 still queued behind it.
    let tasks = work(&[row(8, "assigned", SESSION), row(9, "assigned", SESSION)]);
    let found = stalls(json!({
        "tasks": [open(8, "assigned", (110, "task.assigned", 9_000_000)),
                  open(9, "assigned", (120, "task.assigned", 9_000_000))],
    }));
    let facts = [worker(SemanticState::Waiting, true)];
    assert_eq!(
        only(&items(&facts, &tasks, &found, &bounds())).id,
        "stall-8-delivery-110"
    );
}

// --- the bound's edges -------------------------------------------------------

#[test]
fn a_step_is_escalated_at_its_bound_and_not_a_second_before() {
    let facts = [worker(SemanticState::Waiting, true)];
    let tasks = work(&[row(7, "assigned", SESSION)]);
    let at = |ago_ms: i64| {
        stalls(json!({"tasks": [open(7, "assigned", (120, "task.assigned", ago_ms))]}))
    };
    assert!(items(&facts, &tasks, &at(599_000), &bounds()).is_empty());
    assert_eq!(items(&facts, &tasks, &at(600_000), &bounds()).len(), 1);
    // The bounds come from the environment, whole seconds.
    let (short, warnings) =
        Bounds::read(|name| (name == "SPAWN_STALL_DELIVERY_SECS").then(|| "90".to_string()));
    assert!(warnings.is_empty());
    assert!(items(&facts, &tasks, &at(89_000), &short).is_empty());
    assert_eq!(items(&facts, &tasks, &at(90_000), &short).len(), 1);
}

#[test]
fn a_bound_that_is_not_a_whole_number_of_seconds_warns_and_keeps_its_default() {
    for bad in ["-5", "1.5", "soon"] {
        let (read, warnings) =
            Bounds::read(|name| (name == "SPAWN_STALL_ACTIVITY_SECS").then(|| bad.to_string()));
        assert_eq!(read, Bounds::default());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("SPAWN_STALL_ACTIVITY_SECS"));
    }
    let defaults = Bounds::default();
    assert_eq!(
        [
            defaults.delivery,
            defaults.activity,
            defaults.review,
            defaults.gate
        ],
        [600_000, 900_000, 1_800_000, 3_600_000]
    );
    assert_eq!(
        [defaults.landing, defaults.stop, defaults.halted],
        [600_000, 300_000, 0]
    );
}

// --- what moves a tag --------------------------------------------------------

#[test]
fn a_new_fact_changes_the_activity_tag() {
    let facts = [worker(SemanticState::Working, false)];
    let tasks = work(&[row(7, "delivered", SESSION)]);
    let latest = |seq: i64, ago_ms: i64| {
        stalls(json!({
            "tasks": [open(7, "delivered", (140, "task.delivered", 3_000_000))],
            "sessions": [{"session_id": SESSION, "seq": seq, "event_name": "tool.completed",
                          "at_ms": NOW - ago_ms}],
        }))
    };
    let first = only(&items(&facts, &tasks, &latest(20, 1_000_000), &bounds())).tag();
    // One tool call: the step moved, and is under its bound again.
    assert!(items(&facts, &tasks, &latest(22, 100_000), &bounds()).is_empty());
    let second = only(&items(&facts, &tasks, &latest(22, 900_000), &bounds())).tag();
    assert_ne!(first, second);
    assert_eq!(second, format!("{SESSION} stall-7-activity-22"));
}

#[test]
fn a_failed_retry_does_not_change_the_delivery_tag() {
    let tasks = work(&[row(7, "assigned", SESSION)]);
    let found = stalls(json!({"tasks": [open(7, "assigned", (120, "task.assigned", 700_000))]}));
    let mut facts = worker(SemanticState::Waiting, true);
    facts
        .dispatched
        .push(failed(30, "deliver-7-0", 690_000, "terminal gone: no pane"));
    let before = only(&items(&[facts.clone()], &tasks, &found, &bounds())).clone();
    facts.dispatched.push(failed(
        31,
        "deliver-7-0-retry-1",
        60_000,
        "terminal gone: no pane",
    ));
    let after = only(&items(&[facts], &tasks, &found, &bounds())).clone();
    assert_eq!(before.tag(), after.tag());
    assert!(
        after
            .line
            .contains("`deliver-7-0-retry-1` failed (the same error)"),
        "{}",
        after.line
    );
    assert!(
        after.line.contains("retry `deliver-7-0-retry-2` due in"),
        "{}",
        after.line
    );
}

/// Retries spent: `DeliveryFailed` was the stall's escalation. A delivery
/// that may have typed is not tried again, and its stall still is.
#[test]
fn a_delivery_given_up_gets_no_stall_item() {
    // A re-issue: its spent delivery is escalated as delivery failed.
    let mut reissue = row(7, "assigned", SESSION);
    reissue["reissue_of"] = json!(6);
    let tasks = work(&[reissue]);
    let found = stalls(json!({"tasks": [open(7, "assigned", (120, "task.assigned", 2_000_000))]}));
    let mut facts = worker(SemanticState::Waiting, true);
    let error = "terminal gone: no pane";
    facts
        .dispatched
        .push(failed(30, "deliver-7-0", 1_900_000, error));
    for retry in 1..=crate::deliver::DELIVERY_RETRIES {
        facts.dispatched.push(failed(
            30 + i64::try_from(retry).expect("small"),
            &format!("deliver-7-0-retry-{retry}"),
            100_000,
            error,
        ));
    }
    let plan = decide_with(
        &[facts.clone()],
        &tasks,
        &items(&[facts.clone()], &tasks, &found, &bounds()),
        false,
        NOW,
        "spawn",
    );
    let whys: Vec<Why> = plan.pending.iter().map(|item| item.why).collect();
    assert_eq!(whys, [Why::DeliveryFailed], "{:?}", plan.pending);
    // A first delivery spent the same way is re-issued: neither escalated
    // nor stalled.
    let first = work(&[row(7, "assigned", SESSION)]);
    let plan = decide_with(
        &[facts.clone()],
        &first,
        &items(&[facts.clone()], &first, &found, &bounds()),
        false,
        NOW,
        "spawn",
    );
    assert!(plan.pending.is_empty(), "{:?}", plan.pending);
    assert_eq!(plan.reissues.len(), 1, "due: its last attempt is long past");
    assert_eq!(plan.reissues[0].task, 7);
    // One that may have typed: not retried, and its stall is escalated.
    let mut unsure = worker(SemanticState::Waiting, true);
    unsure
        .dispatched
        .push(failed(30, "deliver-7-0", 1_900_000, REFUSED));
    let item = only(&items(&[unsure], &tasks, &found, &bounds())).clone();
    assert!(item.line.contains("not tried again"), "{}", item.line);
}

// --- the line ------------------------------------------------------------------

/// The scenario's stuck delivery: the line names the task, the step, every
/// failed attempt's key and error, the judgement, and the retry due and
/// held, with why.
#[test]
fn the_line_names_the_task_the_step_the_failed_attempts_and_the_judgement() {
    let tasks = work(&[row(7, "assigned", SESSION)]);
    let found = stalls(json!({"tasks": [open(7, "assigned", (120, "task.assigned", 95_000))]}));
    let mut facts = worker(SemanticState::Waiting, true);
    facts
        .dispatched
        .push(failed(30, "deliver-7-0", 94_000, "terminal gone: no pane"));
    facts
        .dispatched
        .push(failed(31, "deliver-7-0-retry-1", 30_000, BUSY));
    // Its latest judgement is older than the facts its own refusals wrote.
    facts.evidence_seq = Some(24);
    let (short, _) =
        Bounds::read(|name| (name == "SPAWN_STALL_DELIVERY_SECS").then(|| "90".to_string()));
    let item = only(&items(&[facts], &tasks, &found, &short)).clone();
    for word in [
        "task #7",
        "step delivery",
        "for 1m35s",
        "`deliver-7-0` failed (terminal gone: no pane)",
        "`deliver-7-0-retry-1` failed (pane state Busy does not accept input)",
        "latest judgement WAITING by rules (90%), older than the latest fact (seq 24)",
        "retry `deliver-7-0-retry-2` due and held: its worker has no judgement covering its \
         latest fact",
    ] {
        assert!(item.line.contains(word), "{word}: {}", item.line);
    }
    assert!(!item.line.contains('\n'));
}

#[test]
fn the_activity_line_names_the_latest_fact_and_the_judgement_state() {
    let facts = [worker(SemanticState::Working, false)];
    let tasks = work(&[row(7, "delivered", SESSION)]);
    let found = stalls(json!({
        "tasks": [open(7, "delivered", (140, "task.delivered", 40_000))],
        "sessions": [{"session_id": SESSION, "seq": 20, "event_name": "message.received",
                      "at_ms": NOW - 41_000, "screen": "you: BB:SILENT quiet"}],
    }));
    let (short, _) =
        Bounds::read(|name| (name == "SPAWN_STALL_ACTIVITY_SECS").then(|| "30".to_string()));
    let item = only(&items(&facts, &tasks, &found, &short)).clone();
    assert_eq!(item.id, "stall-7-activity-140");
    for word in [
        "task #7",
        "step activity",
        "message.received",
        "WORKING",
        "covers the latest fact",
    ] {
        assert!(item.line.contains(word), "{word}: {}", item.line);
    }
}

/// Through the whole pass: the stall reaches an idle supervisor once, under
/// the heading `stalled`.
#[test]
fn a_stall_reaches_the_supervisor_once_under_its_heading() {
    let tasks = work(&[row(7, "assigned", SESSION)]);
    let found = stalls(json!({"tasks": [open(7, "assigned", (120, "task.assigned", 600_000))]}));
    let resident = worker(SemanticState::Working, false);
    let rows = [resident.clone(), supervisor(&[])];
    let host = items(&rows, &tasks, &found, &bounds());
    let plan = decide_with(&rows, &tasks, &host, true, NOW, "spawn");
    let digest = plan
        .orders
        .iter()
        .find(|order| order.session_id == "session_supervisor")
        .expect("a digest");
    let spawn_core::command::CommandAction::Input { text, tags } = &digest.action else {
        panic!("a digest is typed input");
    };
    assert_eq!(tags, &[format!("{SESSION} stall-7-delivery-120")]);
    assert!(
        text.contains(&format!("- {SESSION} — stalled: task #7")),
        "{text}"
    );
    let rows = [resident, supervisor(tags)];
    let host = items(&rows, &tasks, &found, &bounds());
    let plan = decide_with(&rows, &tasks, &host, true, NOW, "spawn");
    assert!(plan.pending.is_empty(), "{:?}", plan.pending);
}
