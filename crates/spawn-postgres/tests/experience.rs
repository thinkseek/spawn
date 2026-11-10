//! The experience views (`spawn_decisions`, `spawn_interventions`,
//! `spawn_outcomes`, `spawn_escalations`), the window's figures counted over
//! them (`fleet_experience`, `/api/scorecard/fleet`) and `/api/fleet`'s 24 h
//! ladder, which reads the same view (docs/fleet-architecture.md §4).
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use postgres::Client;
use serde_json::{Value, json};

mod common;
use common::{admin, as_login, store};

const HOUR_MS: i64 = 3_600_000;
/// 2023-11-15T00:00:00Z: the history's hour 0 (experience-history.sql).
const B: i64 = 1_700_006_400_000;

fn seed(client: &mut Client) {
    client
        .batch_execute(&format!(
            "begin; {} commit;",
            include_str!("experience-history.sql")
        ))
        .expect("the history seeds");
}

/// `<value>:<n>` pairs of `column` over `view` in `[since, until)`, by value.
fn counts(client: &mut Client, view: &str, column: &str, since: i64, until: i64) -> String {
    client
        .query_one(
            &format!(
                "select coalesce(string_agg(v || ':' || n, ' ' order by v), '') from (
                   select {column}::text as v, count(*) as n from {view}
                    where at_ms >= $1 and at_ms < $2 group by 1) t"
            ),
            &[&since, &until],
        )
        .expect("the view reads")
        .get(0)
}

#[test]
fn the_views_derive_decisions_interventions_outcomes_and_escalations_from_the_log() {
    let (_store, schema) = store("experience_views");
    let mut client = admin(&schema);
    seed(&mut client);
    let (since, until) = (B, B + 100 * HOUR_MS);
    assert_eq!(
        counts(&mut client, "spawn_decisions", "tier", since, until),
        "dispatcher:3 human:2 liveness:1 model:3 rules:1 spawn-integrate:1 supervisor:1"
    );
    assert_eq!(
        counts(&mut client, "spawn_interventions", "kind", since, until),
        "cancel_reassign:1 command:2 owner_task:1 resume:1"
    );
    assert_eq!(
        counts(&mut client, "spawn_outcomes", "outcome", since, until),
        "aborted:1 accepted:1 cancelled:2 landed:1 over_budget:1"
    );
    assert_eq!(
        counts(&mut client, "spawn_escalations", "tag", since, until),
        "s a:1 s b:1 s c:1"
    );

    // Each row at its own time, naming what it is about.
    let rows: Vec<(i64, String, i64, i64, Option<String>)> = client
        .query(
            "select at_ms, outcome, task_id, mission_id, \"by\" from spawn_outcomes order by at_ms",
            &[],
        )
        .expect("the outcomes read")
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4)))
        .collect();
    let hours = |h: i64| B + h * HOUR_MS;
    assert_eq!(
        rows,
        vec![
            (hours(20), "accepted".into(), 1, 1, None),
            (hours(30), "landed".into(), 2, 2, None),
            (
                hours(35),
                "cancelled".into(),
                3,
                1,
                Some("spawn-dispatch".into())
            ),
            (hours(45), "cancelled".into(), 5, 1, None),
            (hours(60), "over_budget".into(), 6, 1, None),
            (hours(62), "aborted".into(), 7, 1, None),
        ],
        "one closing record per task; a record after it closes nothing"
    );
    let reassigned: (i64, i64, i64) = client
        .query_one(
            "select at_ms, mission_id, task_id from spawn_interventions
              where kind = 'cancel_reassign'",
            &[],
        )
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .expect("one cancel-and-reassign");
    assert_eq!(
        reassigned,
        (hours(50), 1, 6),
        "at the reassignment of the objective"
    );
    let escalation: (i64, String) = client
        .query_one(
            "select e.at_ms, c.idempotency_key from spawn_escalations e
               join spawn_commands c on c.id = e.command_id where e.tag = 's a'",
            &[],
        )
        .map(|row| (row.get(0), row.get(1)))
        .expect("tag `s a`");
    assert_eq!(
        escalation,
        (hours(25), "digest-1".to_string()),
        "at the first digest that carried it"
    );
}

#[test]
fn a_windows_figures_count_the_views() {
    let (store, schema) = store("experience_window");
    seed(&mut admin(&schema));
    let answer = store
        .fleet_experience(B, B + 100 * HOUR_MS)
        .expect("the window's figures read");
    assert_eq!(answer["since_ms"], json!(B));
    assert_eq!(answer["until_ms"], json!(B + 100 * HOUR_MS));
    assert_eq!(
        answer["ladder"],
        json!({"rules": 1, "model": 3, "dispatcher": 3, "supervisor": 1, "human": 2,
               "liveness": 1})
    );
    assert_eq!(answer["ladder_extra"], json!([["spawn-integrate", 1]]));
    assert_eq!(answer["agent_hours"], json!(30));
    assert_eq!(answer["human_per_agent_hour"], json!(0.067));
    assert_eq!(answer["escalations"], json!(3));
    assert_eq!(
        answer["interventions"],
        json!({"total": 5, "by_kind": {"command": 2, "resume": 1, "owner_task": 1,
                                       "cancel_reassign": 1}})
    );
    assert_eq!(
        answer["outcomes"],
        json!({"landed": 1, "accepted": 1, "cancelled": 2, "aborted": 1, "over_budget": 1})
    );
    // Two landings over 100 h, 4.1667 days.
    assert_eq!(answer["landed_per_day"], json!(0.48));
    assert_eq!(
        answer["judgements"],
        json!({"total": 4, "by_judge": {"jev": 2, "rules": 1, "unknown": 1},
               "judge_unavailable": 1, "disagreements": 1, "model_latency_p50_ms": 200})
    );
    assert_eq!(
        answer["missions"],
        json!([
            {"mission": 1, "text": "busy", "hours": 99, "idle_hours": 21, "idle": false},
            {"mission": 2, "text": "gated", "hours": 78, "idle_hours": 60, "idle": true},
        ])
    );

    // A window before every record: nothing, no ratio, no mission.
    let empty = store
        .fleet_experience(B - 200 * HOUR_MS, B - 100 * HOUR_MS)
        .expect("an empty window reads");
    assert_eq!(empty["agent_hours"], json!(0));
    assert_eq!(empty["human_per_agent_hour"], Value::Null);
    assert_eq!(empty["judgements"]["model_latency_p50_ms"], Value::Null);
    assert_eq!(empty["interventions"]["total"], json!(0));
    assert_eq!(empty["missions"], json!([]));
    assert_eq!(empty["ladder_extra"], json!([]));

    // A window that ends inside the history clips to it: mission 2 is open
    // from 2 to 24 (22 h) with task 2 from 12 (12 h busy).
    let day = store
        .fleet_experience(B, B + 24 * HOUR_MS)
        .expect("the first day reads");
    assert_eq!(
        day["missions"][1],
        json!({"mission": 2, "text": "gated", "hours": 22, "idle_hours": 10, "idle": false})
    );
    assert_eq!(day["agent_hours"], json!(20));
}

#[test]
fn the_reader_and_the_supervisor_read_the_views() {
    let (store, schema) = store("experience_logins");
    seed(&mut admin(&schema));
    for login in ["spawn_reader", "spawn_supervisor"] {
        let mut client = as_login(&schema, login);
        for view in [
            "spawn_decisions",
            "spawn_interventions",
            "spawn_outcomes",
            "spawn_escalations",
        ] {
            let rows: i64 = client
                .query_one(&format!("select count(*) from {view}"), &[])
                .unwrap_or_else(|error| panic!("{login} reads {view}: {error}"))
                .get(0);
            assert!(rows > 0, "{login} sees the rows of {view}");
        }
    }
    let reader = spawn_postgres::PostgresStore::connect(
        &spawn_postgres::role_url(&common::schema_url(&schema), "spawn_reader")
            .expect("the reader's URL"),
    )
    .expect("the reader connects");
    assert_eq!(
        reader
            .fleet_experience(B, B + 100 * HOUR_MS)
            .expect("the reader reads the window")["interventions"]["total"],
        json!(5)
    );
    drop(store);
}

#[test]
fn the_fleets_24_hour_ladder_reads_the_decisions_view() {
    let (store, schema) = store("experience_ladder");
    let mut client = admin(&schema);
    // Now, an hour ago, and two days ago (outside the day).
    client
        .batch_execute(
            "insert into spawn_commands (principal, idempotency_key, agent_id, session_id, action,
                                         state, succeeded, error, created_at_ms, finished_at_ms)
             select p, k, 'agent_worker_w_1', 'session_worker_w_1', '{\"type\":\"input\",\"text\":\"x\"}',
                    'finished', true, null, spawn_now_ms() - ago, spawn_now_ms() - ago + 10
               from (values ('human', 'h-1', 3600000), ('spawn', 'cli-1', 3600000),
                            ('spawn-integrate', 'i-1', 3600000), ('spawn-dispatch', 'd-1', 3600000),
                            ('human', 'h-old', 172800000)) v(p, k, ago);
             insert into spawn_events (agent_id, session_id, execution_id, source, source_event_id,
                                       kind, session_seq, recorded_at_ms)
             values ('agent_worker_w_1', 'session_worker_w_1', 'exec_l', 'bridge', null,
                     '{\"name\":\"execution.started\",\"detail\":{\"engine\":\"claude\",\"workspace\":\"/w\"}}',
                     1, spawn_now_ms() - 7200000);",
        )
        .expect("seed the day");
    let health = store.fleet_health().expect("the health reads");
    let ladder = &health["ladder_24h"];
    assert_eq!(
        ladder["human"],
        json!(2),
        "the board's and the CLI's: {ladder}"
    );
    assert_eq!(ladder["dispatcher"], json!(1));
    assert_eq!(ladder["rules"], json!(0));
    assert_eq!(
        ladder["spawn-integrate"],
        json!(1),
        "another principal is a tier of its own"
    );
    let rate = health["human_per_agent_hour"]
        .as_f64()
        .expect("a rate with an execution running");
    assert!(
        (rate - 1.0).abs() < 0.01,
        "two in about two agent-hours: {rate}"
    );
}
