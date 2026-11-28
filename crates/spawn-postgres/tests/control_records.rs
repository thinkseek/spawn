//! Bidirectional contract pins between `spawn_core::control::{ControlRecord, GradingRecord}`
//! and `crates/spawn-postgres/sql/schema.sql`, plus real PostgreSQL lifecycle &
//! pure RSI Episode Replay parity verification.
//!
//! Run against PostgreSQL (`SPAWN_TEST_DATABASE_URL`) for the live DB case.

use std::collections::{BTreeMap, BTreeSet};

use postgres::Client;
use serde_json::{Value, json};
use spawn_core::control::{
    ControlRecord, ControlRecordKind, EpisodeOutcome, GateVerdict, GradingRecord,
    replay_task_episodes,
};
use spawn_core::grading::TaskDifficulty;
use spawn_core::mission::TaskState;
use spawn_postgres::{PostgresStore, PushRequest};

mod common;
use common::{admin, store};

const SCHEMA_SQL: &str = include_str!("../sql/schema.sql");

fn extract_pg_function(sql: &str, name: &str) -> String {
    let sig = format!("create or replace function {name}(");
    let start = sql
        .find(&sig)
        .unwrap_or_else(|| panic!("function `{name}` not found in schema.sql"));
    let slice = &sql[start..];
    let end = slice
        .find("$$;\n")
        .unwrap_or_else(|| panic!("end of function `{name}` not found in schema.sql"));
    slice[..end + 4].to_string()
}

fn extract_quoted_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.char_indices();
    while let Some((start, ch)) = chars.next() {
        if ch == '\'' {
            let mut body = String::new();
            for (_, inner) in chars.by_ref() {
                if inner == '\'' {
                    break;
                }
                body.push(inner);
            }
            if !body.is_empty() && start < text.len() {
                out.push(body);
            }
        }
    }
    out
}

fn extract_top_level_case_kinds(fn_sql: &str) -> BTreeSet<String> {
    let lines: Vec<&str> = fn_sql.lines().collect();
    let mut kinds = BTreeSet::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].starts_with("    when ") {
            let mut chunk = lines[i].to_string();
            while !chunk.contains(" then") && i + 1 < lines.len() {
                i += 1;
                chunk.push(' ');
                chunk.push_str(lines[i].trim());
            }
            for tok in extract_quoted_tokens(&chunk) {
                let is_scenario_file = tok
                    .rsplit('.')
                    .next()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("sh"));
                if tok.contains('.') && !tok.contains('%') && !is_scenario_file {
                    kinds.insert(tok);
                }
            }
        }
        i += 1;
    }
    kinds
}

fn extract_dotted_record_literals(fn_sql: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for tok in extract_quoted_tokens(fn_sql) {
        let ignored_ext = tok
            .rsplit('.')
            .next()
            .is_some_and(|ext| matches!(ext, "sh" | "rs" | "md"));
        let is_record_type = tok.contains('.')
            && !tok.contains('%')
            && !tok.contains(' ')
            && !ignored_ext
            && tok != "control.recorded"
            && tok != "grading.recorded"
            && tok
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'_');
        if is_record_type {
            out.insert(tok);
        }
    }
    out
}

#[test]
fn schema_sql_control_and_grading_kinds_match_rust_adt_bidirectionally() {
    let apply_fn = extract_pg_function(SCHEMA_SQL, "spawn_control_apply");
    let sql_control_kinds = extract_top_level_case_kinds(&apply_fn);
    let rust_control_kinds: BTreeSet<String> = ControlRecord::ALL_KINDS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    assert_eq!(
        sql_control_kinds, rust_control_kinds,
        "bidirectional mismatch between `spawn_control_apply` branches and `ControlRecord::ALL_KINDS`"
    );
    assert_eq!(rust_control_kinds.len(), ControlRecordKind::ALL.len());

    let grading_fn = extract_pg_function(SCHEMA_SQL, "spawn_grading_apply");
    let sql_grading_kinds = extract_top_level_case_kinds(&grading_fn);
    let rust_grading_kinds: BTreeSet<String> = GradingRecord::ALL_KINDS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    assert_eq!(
        sql_grading_kinds, rust_grading_kinds,
        "bidirectional mismatch between `spawn_grading_apply` branches and `GradingRecord::ALL_KINDS`"
    );

    // Every allowlist / writer function in schema.sql must remain covered by ControlRecord::ALL_KINDS.
    for writer in [
        "spawn_integrator_record",
        "spawn_host_record",
        "spawn_alert_record",
        "spawn_backup_record",
        "spawn_deploy_record",
        "spawn_verification_record",
        "spawn_objective_retire",
        "spawn_mission_add",
        "spawn_mission_close",
        "spawn_task_assign",
        "spawn_task_delivered",
        "spawn_task_reported",
        "spawn_task_report_unfinished",
        "spawn_task_fork_failed",
        "spawn_task_review",
        "spawn_task_reroute",
        "spawn_task_cancel",
        "spawn_task_reissue",
        "spawn_task_restart_as",
        "spawn_task_reassign_as",
        "spawn_task_steal",
        "spawn_resident_propose",
        "spawn_resident_approve",
        "spawn_resident_retire",
        "spawn_mission_configure",
        "spawn_objective_assign",
        "spawn_mission_resume",
        "spawn_mission_pause",
        "spawn_human_resume",
        "spawn_source_moved",
        "spawn_source_push_request",
        "spawn_source_hold",
        "spawn_source_release",
    ] {
        let body = extract_pg_function(SCHEMA_SQL, writer);
        let literals = extract_dotted_record_literals(&body);
        for lit in literals {
            assert!(
                rust_control_kinds.contains(&lit),
                "writer `{writer}` references control record type `{lit}` not in `ControlRecord::ALL_KINDS`"
            );
        }
    }
}

#[derive(Debug)]
struct SqlTaskRow {
    id: i64,
    mission_id: i64,
    state: String,
    revisions: i32,
    quality_rejections: i32,
    difficulty: i16,
    preset_id: Option<String>,
    first_preset_id: Option<String>,
    model: String,
    agent_id: String,
    session_id: String,
}

fn fetch_sql_tasks(client: &mut Client) -> Vec<SqlTaskRow> {
    client
        .query(
            "select id, mission_id, state, revisions, quality_rejections, difficulty,
                    preset_id, first_preset_id, model, agent_id, session_id
               from mission_tasks
              order by id",
            &[],
        )
        .expect("query mission_tasks")
        .iter()
        .map(|row| SqlTaskRow {
            id: row.get(0),
            mission_id: row.get(1),
            state: row.get(2),
            revisions: row.get(3),
            quality_rejections: row.get(4),
            difficulty: row.get(5),
            preset_id: row.get(6),
            first_preset_id: row.get(7),
            model: row.get(8),
            agent_id: row.get(9),
            session_id: row.get(10),
        })
        .collect()
}

fn fetch_stream(client: &mut Client, event_name: &str) -> Vec<(i64, i64, Value)> {
    client
        .query(
            "select session_seq, recorded_at_ms, kind -> 'detail' -> 'record'
               from spawn_events
              where session_id = 'session_control' and event_name = $1
              order by session_seq",
            &[&event_name],
        )
        .expect("read control session stream")
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .collect()
}

struct LifecycleIds {
    t1: i64,
    t2: i64,
    t3: i64,
    t4: i64,
    t5: i64,
    t6: i64,
    t7: i64,
}

fn seed_grading_and_residents(store: &PostgresStore) {
    for rec in [
        json!({"type": "preset.registered", "preset_id": "cheap-l1", "engine_label": "agy", "model_label": "v4", "model_profile": "cheap", "cost_rank": 0}),
        json!({"type": "preset.onboarded", "preset_id": "cheap-l1", "from_state": "unexamined", "to_state": "active", "to_grade": "l1", "passed": 3, "total": 3, "failed_cases": [], "suite": "evals/l1"}),
        json!({"type": "preset.registered", "preset_id": "neon-l2", "engine_label": "agy", "model_label": "neon", "model_profile": "default", "cost_rank": 1, "execution_profile": "standard"}),
        json!({"type": "preset.onboarded", "preset_id": "neon-l2", "from_state": "unexamined", "to_state": "active", "to_grade": "l3", "passed": 5, "total": 5, "failed_cases": [], "suite": "evals/l3"}),
        json!({"type": "preset.grade_locked", "preset_id": "neon-l2", "from_grade": null, "to_grade": "l2", "from_state": "active", "to_state": "active"}),
        json!({"type": "preset.state_changed", "preset_id": "neon-l2", "from_state": "active", "to_state": "disabled"}),
        json!({"type": "preset.state_changed", "preset_id": "neon-l2", "from_state": "disabled", "to_state": "active"}),
    ] {
        store.grading_record(&rec).expect("grading_record");
    }

    store
        .resident_propose_in("alpha", "Primary resident", Some("workers"))
        .expect("propose alpha");
    store.resident_approve("alpha").expect("approve alpha");
    store
        .resident_propose_in("beta", "Backup resident", Some("workers"))
        .expect("propose beta");
    store.resident_approve("beta").expect("approve beta");
}

fn drive_review_mission(store: &PostgresStore, sha_a: &str, sha_b: &str) -> (i64, i64, i64, i64) {
    let m1 = store
        .mission_add("Review mission lifecycle", &json!([]), "human")
        .expect("add m1");
    let t1 = store
        .task_assign_routed(
            m1,
            "ephemeral",
            "w1",
            "implement parser",
            "cheap",
            false,
            false,
            TaskDifficulty::L1,
            Some("cheap-l1"),
        )
        .expect("assign t1");
    store.task_delivered(t1, 101).expect("deliver t1");
    assert!(
        store
            .task_report_unfinished(t1, "d1", &json!([{"line": 1}]), "finish TODO", 101)
            .expect("unfinished")
    );
    assert!(
        store
            .task_reported_with(t1, "draft v0", Some(sha_a))
            .expect("report t1 v0")
    );
    store
        .task_reject_rerouted(
            t1,
            "add edge test",
            Some(0),
            "neon-l2",
            1,
            TaskDifficulty::L2,
        )
        .expect("reject+reroute t1");
    store.task_delivered(t1, 102).expect("deliver t1 rev 1");
    assert!(
        store
            .task_reported_with(t1, "final v1", Some(sha_b))
            .expect("report t1 v1")
    );
    store.task_review(t1, true, "", Some(1)).expect("accept t1");

    let t2 = store
        .task_assign(m1, "ephemeral", "w2", "wedged task", "default")
        .expect("assign t2");
    store.task_delivered(t2, 103).expect("deliver t2");
    store.task_restart(t2, Some("stalled")).expect("restart t2");
    store.task_cancel(t2).expect("cancel t2");

    let t3 = store
        .task_assign(m1, "resident", "alpha", "resident task 3", "default")
        .expect("assign t3");
    let t4 = store
        .task_assign(m1, "resident", "alpha", "resident task 4", "default")
        .expect("assign t4");
    store
        .task_steal(t4, "beta", "spawn-dispatch", Some("rebalance"))
        .expect("steal t4");
    store.task_cancel(t3).expect("cancel t3");
    store.task_cancel(t4).expect("cancel t4");
    store.resident_retire("beta").expect("retire beta");
    store
        .mission_close(m1, "done", "m1 complete")
        .expect("close m1");
    (t1, t2, t3, t4)
}

fn drive_gated_and_ops_mission(store: &PostgresStore) -> (i64, i64, i64) {
    let source = "/tmp/spawn-control-records-repo";
    let m2 = store
        .mission_add("Gated objective mission", &json!([source]), "human")
        .expect("add m2");
    store
        .mission_configure(
            m2,
            Some("/tmp/gate.sh"),
            "gate",
            Some(&json!({"tool_calls": 5})),
            Some(&json!({"backlog": "/tmp/backlog", "resident": "alpha", "workers": 2})),
        )
        .expect("configure m2");
    store
        .mission_pause(m2, Some("pause check"))
        .expect("pause m2");
    store.mission_resume(m2).expect("unpause m2");

    let t5 = drive_landed_objective(store, m2, source);
    let (t6, t7) = drive_budget_and_retired_objectives(store, m2);
    drive_operational_records(store, m2, source);
    store
        .mission_close(m2, "done", "m2 complete")
        .expect("close m2");
    (t5, t6, t7)
}

fn drive_landed_objective(store: &PostgresStore, m2: i64, source: &str) -> i64 {
    let sha_a = "a".repeat(40);
    let sha_b = "b".repeat(40);
    let sha_c = "c".repeat(40);
    let sha_d = "d".repeat(40);
    let sha_e = "e".repeat(40);

    let t5 = store
        .objective_assign(m2, 1, Some("001.sh"), "Objective 1")
        .expect("assign t5");
    store
        .task_fork_failed(t5, 1, &sha_a, "no prior branch")
        .expect("fork_failed");
    store.task_delivered(t5, 201).expect("deliver t5 rev 0");
    assert!(
        store
            .task_reported_with(t5, "obj1 v0", Some(&sha_c))
            .expect("report t5 v0")
    );
    store
        .integrator_record(&json!({"type": "task.gated", "id": t5, "revision": 0,
                                    "verdict": "rejected", "class": "code",
                                    "base": sha_a, "note": "failed assertion"}))
        .expect("gate reject t5");
    store.task_delivered(t5, 202).expect("deliver t5 rev 1");
    assert!(
        store
            .task_reported_with(t5, "obj1 v1", Some(&sha_d))
            .expect("report t5 v1")
    );
    store
        .integrator_record(&json!({"type": "task.gated", "id": t5, "revision": 1,
                                    "verdict": "conflict", "class": "code",
                                    "base": sha_b, "note": "merge conflict"}))
        .expect("gate conflict t5");
    store.task_delivered(t5, 203).expect("deliver t5 rev 2");
    assert!(
        store
            .task_reported_with(t5, "obj1 v2", Some(&sha_e))
            .expect("report t5 v2")
    );
    store
        .integrator_record(&json!({"type": "task.gated", "id": t5, "revision": 2,
                                    "verdict": "accepted", "class": "code",
                                    "base": sha_b, "candidate": sha_e, "metric": "1.0"}))
        .expect("gate accept t5");
    let hold_id = {
        let _lock = store.source_hold_lock(source).expect("hold lock");
        let held = store
            .source_hold(source, &sha_b, 60, Some("deploy window"))
            .expect("source_hold");
        held["hold"].as_i64().expect("hold id")
    };
    store.task_held(t5, hold_id, source).expect("task.held");
    store.source_release(source).expect("source_release");
    store
        .integrator_record(&json!({"type": "task.landed", "id": t5, "from": sha_b, "to": sha_c}))
        .expect("land t5");
    store
        .verification_record(
            &json!({"type": "objective.verified", "mission": m2, "objective": 1,
                                      "by": "owner", "how": "pg integration", "result": "pass"}),
        )
        .expect("verify obj1");
    t5
}

fn drive_budget_and_retired_objectives(store: &PostgresStore, m2: i64) -> (i64, i64) {
    let t6 = store
        .objective_assign(m2, 2, Some("002.sh"), "Objective 2")
        .expect("assign t6");
    store.task_delivered(t6, 204).expect("deliver t6");
    store
        .integrator_record(
            &json!({"type": "task.over_budget", "id": t6, "reason": "tool cap reached",
                                    "tool_calls": 6, "budget": {"tool_calls": 5}}),
        )
        .expect("over_budget t6");
    let t7 = store
        .objective_assign_continuing(m2, 2, Some("002.sh"), "Objective 2 cont", Some(t6))
        .expect("assign t7");
    store.task_delivered(t7, 205).expect("deliver t7");
    store
        .integrator_record(&json!({"type": "task.aborted", "id": t7, "reason": "gate timeout"}))
        .expect("abort t7");
    store
        .objective_retire(
            &json!({"type": "objective.retired", "id": m2, "objective": 3,
                                   "kind": "obsolete", "by": "owner", "reason": "superseded"}),
        )
        .expect("retire obj3");
    (t6, t7)
}

fn drive_operational_records(store: &PostgresStore, m2: i64, source: &str) {
    let sha_c = "c".repeat(40);
    let sha_d = "d".repeat(40);
    let sha_e = "e".repeat(40);
    store
        .source_moved(source, &sha_c, &sha_d)
        .expect("source_moved");
    let req = store
        .source_push_request(
            source,
            &sha_e,
            &format!("refs/spawn-owner/{sha_e}"),
            &PushRequest::default(),
        )
        .expect("push_req");
    store
        .integrator_record(
            &json!({"type": "source.push_refused", "request": req, "reason": "conflict"}),
        )
        .expect("push_refused");
    store
        .integrator_record(
            &json!({"type": "mission.halted", "id": m2, "reason": "three rejections"}),
        )
        .expect("halt m2");
    store.mission_resume(m2).expect("resume m2");
    store
        .host_record(&json!({"type": "host.loop_started", "boot_id": "b1", "started_at_ms": 1000}))
        .expect("loop_started");
    store
        .host_record(
            &json!({"type": "host.disk", "path": "/tmp", "state": "low", "free_bytes": 1024}),
        )
        .expect("host.disk");
    store
        .alert_record(&json!({"type": "alert.fired", "key": "disk:tmp:1", "kind": "disk_low", "subject": "/tmp", "text": "low"}))
        .expect("alert.fired");
    store
        .alert_record(&json!({"type": "alert.cleared", "key": "disk:tmp:1", "kind": "disk_low", "subject": "/tmp", "evidence": {"free_bytes": 2048}}))
        .expect("alert.cleared");
    store
        .deploy_record(
            &json!({"type": "deploy.finished", "ok": true, "head": sha_d, "by": "owner", "reason": ""}),
        )
        .expect("deploy.finished");
    store
        .backup_record(&json!({
            "type": "backup.stored", "area": "sessions", "host": "h1", "account": "a1",
            "manifest": "m1", "files": 1, "unchanged": 0, "grown": 0, "whole": 1,
            "segments_written": 1, "segments_found": 0, "bytes_written": 10
        }))
        .expect("backup.stored");
}

fn assert_replay_matches_sql(client: &mut Client, ids: &LifecycleIds) {
    let raw_control = fetch_stream(client, "control.recorded");
    assert!(!raw_control.is_empty());

    let mut typed_stream = Vec::with_capacity(raw_control.len());
    let mut distinct_kinds = BTreeSet::new();
    for (seq, at_ms, val) in &raw_control {
        let parsed = ControlRecord::from_value_strict(val)
            .unwrap_or_else(|e| panic!("strict parse failed at seq {seq} for {val}: {e}"));
        let wire = parsed.to_value().expect("reserialize");
        assert_eq!(ControlRecord::from_value(&wire).expect("reparse"), parsed);
        distinct_kinds.insert(parsed.kind());
        typed_stream.push((*seq, *at_ms, parsed));
    }
    assert!(
        distinct_kinds.len() >= 25,
        "expected rich lifecycle coverage (>=25 kinds), got {}",
        distinct_kinds.len()
    );
    assert!(distinct_kinds.contains(&ControlRecordKind::MissionAdded));
    assert!(distinct_kinds.contains(&ControlRecordKind::TaskLanded));
    assert!(distinct_kinds.contains(&ControlRecordKind::ObjectiveVerified));

    let raw_grading = fetch_stream(client, "grading.recorded");
    assert_eq!(raw_grading.len(), 7);
    for (seq, _, val) in &raw_grading {
        GradingRecord::from_value_strict(val)
            .unwrap_or_else(|e| panic!("strict grading parse failed at seq {seq} for {val}: {e}"));
    }

    let episodes = replay_task_episodes(&typed_stream);
    let by_id: BTreeMap<i64, _> = episodes.into_iter().map(|ep| (ep.task_id, ep)).collect();

    let sql_tasks = fetch_sql_tasks(client);
    assert_eq!(by_id.len(), sql_tasks.len());
    for row in &sql_tasks {
        let ep = by_id
            .get(&row.id)
            .unwrap_or_else(|| panic!("replayed task {} missing", row.id));
        let sql_state = row.state.parse::<TaskState>().expect("sql state");
        assert_eq!(ep.mission_id, row.mission_id);
        assert_eq!(ep.state, sql_state, "task {} state mismatch", row.id);
        assert_eq!(
            ep.rework,
            u32::try_from(row.revisions).expect("non-negative revisions"),
            "task {} rework mismatch",
            row.id
        );
        assert_eq!(
            ep.quality_rejections,
            u32::try_from(row.quality_rejections).expect("non-negative quality_rejections"),
            "task {} quality_rejections mismatch",
            row.id
        );
        assert_eq!(ep.difficulty.rank(), row.difficulty);
        assert_eq!(ep.preset_id, row.preset_id);
        assert_eq!(ep.first_preset_id, row.first_preset_id);
        assert_eq!(ep.model, row.model);
        assert_eq!(ep.agent_id, row.agent_id);
        assert_eq!(ep.session_id, row.session_id);
    }

    let ep1 = &by_id[&ids.t1];
    assert_eq!(ep1.outcome, EpisodeOutcome::Accepted);
    assert_eq!(ep1.rework, 1);
    assert_eq!(ep1.reroutes, 1);
    assert_eq!(ep1.first_preset_id.as_deref(), Some("cheap-l1"));
    assert_eq!(ep1.preset_id.as_deref(), Some("neon-l2"));
    assert_eq!(ep1.review_sync_points.len(), 2);

    assert_eq!(by_id[&ids.t2].outcome, EpisodeOutcome::Cancelled);
    assert_eq!(by_id[&ids.t2].restarts, 1);
    assert_eq!(by_id[&ids.t3].outcome, EpisodeOutcome::Cancelled);
    assert_eq!(by_id[&ids.t4].steals, 1);
    assert_eq!(by_id[&ids.t4].agent_id, "agent_resident_beta");

    let ep5 = &by_id[&ids.t5];
    assert_eq!(ep5.outcome, EpisodeOutcome::Landed);
    assert_eq!(ep5.rework, 2);
    assert_eq!(ep5.quality_rejections, 1);
    assert_eq!(ep5.last_gate_verdict, Some(GateVerdict::Accepted));
    assert_eq!(ep5.gate_verdicts.len(), 3);

    assert_eq!(by_id[&ids.t6].outcome, EpisodeOutcome::OverBudget);
    assert_eq!(by_id[&ids.t7].outcome, EpisodeOutcome::Aborted);
    assert_eq!(by_id[&ids.t7].continues, Some(ids.t6));
}

#[test]
fn pg_lifecycle_control_and_grading_records_parse_strictly_and_replay_matches_sql() {
    let (store, schema) = store("control_records_lifecycle");
    seed_grading_and_residents(&store);
    let sha_a = "a".repeat(40);
    let sha_b = "b".repeat(40);
    let (t1, t2, t3, t4) = drive_review_mission(&store, &sha_a, &sha_b);
    let (t5, t6, t7) = drive_gated_and_ops_mission(&store);
    let ids = LifecycleIds {
        t1,
        t2,
        t3,
        t4,
        t5,
        t6,
        t7,
    };

    let mut client = admin(&schema);
    assert_replay_matches_sql(&mut client, &ids);

    // Refolding in SQL must produce the exact same task projections as replay.
    client
        .batch_execute("select spawn_control_refold()")
        .expect("refold");
    assert_replay_matches_sql(&mut client, &ids);
}

fn assert_owner_and_reader_parity(store: &PostgresStore, reader_store: &PostgresStore) {
    let (owner_stream, owner_meta) = store.control_record_stream(true).expect("owner stream");
    let (reader_stream, reader_meta) = reader_store
        .control_record_stream(true)
        .expect("reader stream");
    assert!(!owner_stream.is_empty());
    assert_eq!(owner_stream, reader_stream);
    assert_eq!(owner_meta["warnings"], json!(0));
    assert_eq!(owner_meta, reader_meta);

    let (owner_eps, owner_summary) = store.replay_episodes(None, true).expect("owner replay");
    let (reader_eps, reader_summary) = reader_store
        .replay_episodes(None, true)
        .expect("reader replay");
    assert_eq!(owner_eps.len(), 7);
    assert_eq!(owner_eps, reader_eps);
    assert_eq!(owner_summary, reader_summary);
    assert_eq!(owner_summary["total"], json!(7));
    assert_eq!(owner_summary["landed"], json!(1));
    assert_eq!(owner_summary["accepted"], json!(1));
    assert_eq!(owner_summary["rejected"], json!(0));
    assert_eq!(owner_summary["cancelled"], json!(3));
    assert_eq!(owner_summary["aborted"], json!(1));
    assert_eq!(owner_summary["over_budget"], json!(1));
    assert_eq!(owner_summary["in_flight"], json!(0));
    assert_eq!(owner_summary["total_rework"], json!(3));
    assert_eq!(owner_summary["total_reroutes"], json!(1));
    assert_eq!(owner_summary["review_sync_points"], json!(2));
    assert_eq!(owner_summary["gate_verdicts"], json!(3));
    assert_eq!(owner_summary["warnings"], json!(0));

    let (owner_eps_p, owner_parity, owner_ok, owner_drifts) = store
        .episode_parity_report(None, true)
        .expect("owner parity");
    let (reader_eps_p, reader_parity, reader_ok, reader_drifts) = reader_store
        .episode_parity_report(None, true)
        .expect("reader parity");
    assert!(owner_ok, "owner drifts: {owner_drifts:?}");
    assert!(reader_ok, "reader drifts: {reader_drifts:?}");
    assert!(owner_drifts.is_empty());
    assert!(reader_drifts.is_empty());
    assert_eq!(owner_parity, reader_parity);
    assert_eq!(owner_eps_p, reader_eps_p);

    let mut mission_ids: Vec<i64> = owner_eps.iter().map(|ep| ep.mission_id).collect();
    mission_ids.sort_unstable();
    mission_ids.dedup();
    assert_eq!(mission_ids.len(), 2);
    for (mid, expected_count) in [(mission_ids[0], 4usize), (mission_ids[1], 3usize)] {
        let (eps_o, sum_o, ok_o, drifts_o) = store
            .episode_parity_report(Some(mid), true)
            .expect("mission owner parity");
        let (eps_r, sum_r, ok_r, drifts_r) = reader_store
            .episode_parity_report(Some(mid), true)
            .expect("mission reader parity");
        assert!(ok_o && drifts_o.is_empty(), "{drifts_o:?}");
        assert!(ok_r && drifts_r.is_empty(), "{drifts_r:?}");
        assert_eq!(eps_o.len(), expected_count);
        assert_eq!(eps_o, eps_r);
        assert_eq!(sum_o, sum_r);
    }
}

#[test]
fn pg_owner_and_spawn_reader_replay_episodes_and_match_sql_projections() {
    let (store, schema) = store("control_records_reader_replay");
    seed_grading_and_residents(&store);
    let sha_a = "a".repeat(40);
    let sha_b = "b".repeat(40);
    let _review_ids = drive_review_mission(&store, &sha_a, &sha_b);
    let _gated_ids = drive_gated_and_ops_mission(&store);

    let schema_url = common::schema_url(&schema);
    let reader_url = spawn_postgres::role_url_with_passfile(&schema_url, "spawn_reader", None)
        .expect("reader url");
    let reader_store = PostgresStore::connect_with_options(
        &reader_url,
        &spawn_postgres::ConnectOptions::default(),
    )
    .expect("connect as spawn_reader");

    assert_owner_and_reader_parity(&store, &reader_store);

    // Inject an unknown future control variant into `session_control`:
    // strict mode must fail while compat mode skips with structured warnings
    // and keeps episode parity 100% intact for both owner and spawn_reader.
    admin(&schema)
        .query_one(
            "select * from spawn_append('agent_control', 'session_control', null, 'control', null,
                 jsonb_build_object('name', 'control.recorded',
                                    'detail', jsonb_build_object('record',
                                        jsonb_build_object('type', 'future.experiment.v99', 'id', 999))))",
            &[],
        )
        .expect("append unknown control variant");

    assert!(store.control_record_stream(true).is_err());
    assert!(reader_store.control_record_stream(true).is_err());

    let (compat_eps_o, compat_sum_o, compat_ok_o, compat_drifts_o) = store
        .episode_parity_report(None, false)
        .expect("owner compat parity");
    let (compat_eps_r, compat_sum_r, compat_ok_r, compat_drifts_r) = reader_store
        .episode_parity_report(None, false)
        .expect("reader compat parity");
    assert!(compat_ok_o && compat_drifts_o.is_empty());
    assert!(compat_ok_r && compat_drifts_r.is_empty());
    assert_eq!(compat_eps_o, compat_eps_r);
    assert_eq!(compat_eps_o.len(), 7);
    assert_eq!(compat_sum_o["warnings"], json!(1));
    assert_eq!(
        compat_sum_o["warning_details"]["unknown_variants"],
        json!(1)
    );
    assert_eq!(
        compat_sum_o["warning_details"]["by_kind"]["future.experiment.v99"],
        json!(1)
    );
    assert_eq!(compat_sum_o, compat_sum_r);

    drop(reader_store);
    drop(store);
}
