//! The dashboard's reads (`spawn serve`): the fleet board's times and
//! current tool, the fleet's health over its windows, and one task's texts
//! in full. The board and the health read through indexes of their own
//! (sql/schema.sql, "The dashboard's reads"); these cases pin what they
//! answer.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`).

use serde_json::{Value, json};
use spawn_core::event::{ENGINE, EventKind, ModelProfile, NewEvent, SessionSeq, Source};
use spawn_core::id::{AgentId, CommandId, ExecutionId, SessionId, WorkspaceId};
use spawn_core::observation::{Confidence, Observation, SemanticState};
use spawn_core::port::EventLog;

mod common;
use common::{admin, store};

fn agent() -> AgentId {
    AgentId::new("agent_board").expect("valid agent")
}

fn session() -> SessionId {
    SessionId::new("session_board").expect("valid session")
}

fn event(
    execution: Option<&ExecutionId>,
    source: Source,
    key: Option<&str>,
    kind: EventKind,
) -> NewEvent {
    NewEvent {
        agent_id: agent(),
        session_id: session(),
        execution_id: execution.cloned(),
        source,
        source_event_id: key.map(str::to_string),
        kind,
    }
}

fn judged(state: SemanticState, seq: u64) -> NewEvent {
    event(
        None,
        Source::Observer,
        None,
        EventKind::ObservationRecorded {
            observation: Observation::basic(
                state,
                None,
                SessionSeq(seq),
                Confidence::new(0.5).expect("valid confidence"),
                "heuristic",
            ),
        },
    )
}

/// The board's row of the one session.
fn row(board: &Value) -> Value {
    board
        .as_array()
        .and_then(|rows| rows.iter().find(|row| row["session_id"] == "session_board"))
        .cloned()
        .expect("the session's row")
}

#[test]
fn the_board_dates_the_current_state_from_its_first_judgement_and_names_the_open_tool() {
    let (store, schema) = store("board_since");
    // Working, working, blocked, then working twice: the state began with
    // the fourth judgement. Each a few milliseconds apart, so times differ.
    for (seq, state) in [
        SemanticState::Working,
        SemanticState::Working,
        SemanticState::Blocked,
        SemanticState::Working,
        SemanticState::Working,
    ]
    .into_iter()
    .enumerate()
    {
        store
            .append(judged(state, seq as u64 + 1))
            .expect("append succeeds");
        std::thread::sleep(std::time::Duration::from_millis(3));
    }
    let times: Vec<i64> = admin(&schema)
        .query(
            "select recorded_at_ms from spawn_events
              where event_name = 'observation.recorded' order by session_seq",
            &[],
        )
        .expect("the judgements")
        .iter()
        .map(|row| row.get(0))
        .collect();
    assert_eq!(times.len(), 5);
    assert!(times[2] < times[3], "the blocked judgement is earlier");

    // A live execution with two tools started and the later one done: the
    // open one is the earlier.
    let execution = ExecutionId::new("exec_board").expect("valid execution");
    store
        .append(event(
            Some(&execution),
            Source::Bridge,
            Some("exec_board:started"),
            EventKind::ExecutionStarted {
                engine: ENGINE.to_string(),
                workspace: WorkspaceId::new("1:/workspaces/1").expect("valid workspace"),
                command_id: CommandId(1),
                fork: None,
                model: ModelProfile::Default,
                transcript_from: None,
            },
        ))
        .expect("append succeeds");
    for (key, kind) in [
        (
            "a:start",
            EventKind::ToolStarted {
                tool_name: "Bash".to_string(),
                tool_use_id: "a".to_string(),
                summary: Some("cargo test -p spawn-cli".to_string()),
            },
        ),
        (
            "b:start",
            EventKind::ToolStarted {
                tool_name: "Read".to_string(),
                tool_use_id: "b".to_string(),
                summary: None,
            },
        ),
        (
            "b:done",
            EventKind::ToolCompleted {
                tool_use_id: "b".to_string(),
                duration_ms: Some(1),
            },
        ),
    ] {
        store
            .append(event(Some(&execution), Source::Claude, Some(key), kind))
            .expect("append succeeds");
    }

    let board = row(&store.fleet_board().expect("the board reads"));
    assert_eq!(board["state"], "WORKING");
    assert_eq!(board["state_since_ms"], json!(times[3]));
    assert_eq!(board["live"], true);
    assert_eq!(board["activity"]["current_tool"], "Bash");
    assert_eq!(
        board["activity"]["current_tool_summary"], "cargo test -p spawn-cli",
        "the open call's key input, from its tool.started"
    );
    assert_eq!(board["activity"]["tool_calls"], 2);
    assert!(
        board.get("last_input").is_none(),
        "the board sends what the page shows: {board}"
    );

    // The health reads its windows: five judgements by one model judge and
    // two tool calls in the last hour.
    let health = store.fleet_health().expect("the health reads");
    assert_eq!(health["last_hour"]["judgements"], 5);
    assert_eq!(health["last_hour"]["by_judge"], json!({"heuristic": 5}));
    assert_eq!(health["last_hour"]["tool_calls"], 2);
    assert_eq!(health["ladder_24h"]["rules"], 0);
    assert_eq!(health["ladder_24h"]["model"], 5);
    assert_eq!(health["live"], 1);
}

#[test]
fn a_session_judged_one_state_only_is_in_it_since_its_first_judgement() {
    let (store, schema) = store("board_one_state");
    for seq in 1..=3 {
        store
            .append(judged(SemanticState::Waiting, seq))
            .expect("append succeeds");
    }
    let first: i64 = admin(&schema)
        .query_one(
            "select min(recorded_at_ms) from spawn_events where event_name = 'observation.recorded'",
            &[],
        )
        .expect("the first judgement")
        .get(0);
    let board = row(&store.fleet_board().expect("the board reads"));
    assert_eq!(board["state"], "WAITING");
    assert_eq!(board["state_since_ms"], json!(first));
    assert_eq!(board["activity"]["current_tool"], json!(null));
    assert_eq!(board["activity"]["current_tool_summary"], json!(null));
}

#[test]
fn a_tasks_texts_read_in_full_and_a_missing_task_reads_none() {
    let (store, _schema) = store("task_texts");
    let mission = store
        .mission_add("Texts", &json!([]), "human")
        .expect("add");
    let brief = format!("Title\n{}", "brief line\n".repeat(200));
    let task = store
        .task_assign(mission, "ephemeral", "w", &brief, "default")
        .expect("assign");
    store.task_delivered(task, 7).expect("delivered");
    let report = "report line\n".repeat(500);
    assert!(store.task_reported(task, &report).expect("reported"));

    let texts = store
        .task_texts(task)
        .expect("the texts read")
        .expect("the task");
    assert_eq!(texts["id"], task);
    assert_eq!(texts["mission_id"], mission);
    assert_eq!(texts["brief"], brief.as_str(), "the whole brief");
    assert_eq!(texts["report"], report.as_str(), "the whole report");
    assert_eq!(texts["review_note"], json!(null));
    assert!(texts["updated_at_ms"].as_i64().is_some());
    assert_eq!(store.task_texts(task + 1000).expect("reads"), None);
}

#[test]
fn the_label_facts_carry_each_sessions_latest_task_and_the_open_ones() {
    let (store, _schema) = store("label_facts");
    let mission = store
        .mission_add(
            "Owner work on Spawn: changes the owner reviews.",
            &json!(["/x/spawn.git"]),
            "human",
        )
        .expect("add");
    let brief = format!(
        "Owner work on Spawn: a report that is still a draft is not taken.\n{}",
        "more\n".repeat(300)
    );
    // A worker's one task, closed: still its session's latest.
    let closed = store
        .task_assign(mission, "ephemeral", "draft_report", &brief, "default")
        .expect("assign");
    store.task_cancel(closed).expect("cancel");
    // A resident's old task, closed, and its newer one, open: only the newer.
    store.resident_propose("core", "Owns it").expect("propose");
    store.resident_approve("core").expect("approve");
    let old = store
        .task_assign(mission, "resident", "core", "Old work.", "default")
        .expect("assign");
    store.task_cancel(old).expect("cancel");
    let open = store
        .task_assign(mission, "resident", "core", "New work.", "default")
        .expect("assign");

    let facts = store.label_facts().expect("the facts read");
    let row = facts["missions"]
        .as_array()
        .expect("missions")
        .iter()
        .find(|row| row["id"] == mission)
        .expect("the mission")
        .clone();
    assert_eq!(
        row["text"],
        "Owner work on Spawn: changes the owner reviews."
    );
    assert_eq!(row["sources"], json!(["/x/spawn.git"]));
    assert_eq!(row["gate"], json!(null));
    let ids: Vec<i64> = row["tasks"]
        .as_array()
        .expect("tasks")
        .iter()
        .filter_map(|task| task["id"].as_i64())
        .collect();
    assert_eq!(
        ids,
        [closed, open],
        "the latest of each session, and the open one"
    );
    let task = &row["tasks"][0];
    assert_eq!(task["state"], "cancelled");
    let start = task["brief"].as_str().expect("a brief");
    assert!(brief.starts_with(start), "the brief's start");
    assert_eq!(start.chars().count(), 600, "only its start");
    assert!(
        task.get("report").is_none(),
        "no report: a label never reads it"
    );
    let residents = facts["residents"].as_array().expect("residents");
    assert!(residents.iter().any(|resident| resident["name"] == "core"));
    assert!(facts["inputs"].is_array(), "the latest inputs");
}
