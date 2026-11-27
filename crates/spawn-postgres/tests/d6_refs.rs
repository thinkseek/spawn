//! D6 Phase 4 Step 1 (`#113` Fine Item 7.2): `spawn_snapshots`, `spawn_refs`,
//! `spawn_ref_log`, owner-only CAS / landing functions, GC collection marking,
//! and least-privilege role isolation.
//!
//! A real database is required (`SPAWN_TEST_DATABASE_URL`); every case runs
//! in a schema of its own.

use serde_json::json;
use spawn_core::port::StoreError;
use spawn_postgres::{PostgresStore, RefKind, SnapshotState};

mod common;
use common::{admin, as_login, migrate_warnings, store};

fn sha256_of_hex(fill: &str) -> String {
    format!("sha256:{}", fill.repeat(32))
}

fn oid_of_hex(fill: &str) -> String {
    fill.repeat(20)
}

fn setup_accepted_task(
    store: &PostgresStore,
    source: &str,
    base_oid: &str,
    candidate_oid: &str,
) -> i64 {
    let mission = store
        .mission_add("D6 landing test", &json!([source]), "human")
        .expect("add mission");
    store
        .mission_configure(mission, Some("/scenarios"), "gate", None, None)
        .expect("configure mission");
    let task = store
        .task_assign(mission, "ephemeral", "w_d6", "land work", "default")
        .expect("assign task");
    store.task_delivered(task, 1).expect("delivered");
    assert!(store.task_reported(task, "done").expect("reported"));
    store
        .integrator_record(&json!({
            "type": "task.gated",
            "id": task,
            "revision": 0,
            "verdict": "accepted",
            "base": base_oid,
            "candidate": candidate_oid,
        }))
        .expect("accepted verdict");
    task
}

fn assert_forbidden_sqlstate(res: Result<u64, postgres::Error>) {
    let err = res.expect_err("expected 42501 refusal");
    assert_eq!(err.as_db_error().map(|db| db.code().code()), Some("42501"));
}

#[test]
fn snapshot_record_is_idempotent_and_rejects_conflicts_or_direct_mutations() {
    let (store, schema) = store("d6_snapshots");
    let snap1 = sha256_of_hex("11");
    let oid1 = oid_of_hex("aa");
    let tree1 = sha256_of_hex("22");
    let parents = vec![sha256_of_hex("00")];

    assert_eq!(
        store
            .record_snapshot(&snap1, &oid1, &tree1, &parents, "proj-a", 1_700_000_000_000)
            .expect("record snapshot"),
        snap1
    );
    assert_eq!(
        store
            .record_snapshot(&snap1, &oid1, &tree1, &parents, "proj-a", 1_700_000_000_000)
            .expect("idempotent replay"),
        snap1
    );
    let err = store
        .record_snapshot(
            &snap1,
            &oid1,
            &sha256_of_hex("33"),
            &parents,
            "proj-a",
            1_700_000_000_000,
        )
        .expect_err("conflicting tree rejected");
    assert!(matches!(err, StoreError::Backend(ref msg) if msg.contains("23505")));

    let snap2 = sha256_of_hex("44");
    let err = store
        .record_snapshot(&snap2, &oid1, &tree1, &[], "proj-a", 1_700_000_000_001)
        .expect_err("duplicate git_oid rejected");
    assert!(matches!(err, StoreError::Backend(ref msg) if msg.contains("23505")));
    assert!(
        store
            .record_snapshot("bad", &oid_of_hex("bb"), &tree1, &[], "proj-a", 1)
            .is_err()
    );
    assert!(
        store
            .record_snapshot(
                &snap2,
                &oid_of_hex("bb"),
                &tree1,
                &["not-a-digest".to_string()],
                "proj-a",
                1
            )
            .is_err()
    );

    let got = store
        .get_snapshot(&snap1)
        .expect("get_snapshot")
        .expect("row present");
    assert_eq!(
        (&got.id, &got.git_oid, &got.tree, &got.parents, got.state),
        (&snap1, &oid1, &tree1, &parents, SnapshotState::Live)
    );
    assert_eq!(
        store
            .get_snapshot_by_oid(&oid1)
            .expect("get_snapshot_by_oid")
            .as_ref(),
        Some(&got)
    );

    let mut client = admin(&schema);
    assert_forbidden_sqlstate(client.execute(
        "update spawn_snapshots set project = 'evil' where id = $1",
        &[&snap1],
    ));
    assert_forbidden_sqlstate(
        client.execute("delete from spawn_snapshots where id = $1", &[&snap1]),
    );
    assert!(
        client.batch_execute("truncate spawn_snapshots").is_err(),
        "truncate without cascade refused"
    );
    assert_forbidden_sqlstate(
        client
            .batch_execute("truncate spawn_snapshots cascade")
            .map(|()| 0),
    );

    assert!(migrate_warnings(&schema).is_empty());
    store.migrate().expect("idempotent migrate");
    assert_eq!(store.get_snapshot(&snap1).expect("read"), Some(got));
}

/// Regression: concurrent workers commit identical synthetic trees within the
/// same 1 s Git timestamp, so they project the same `git_oid` under distinct
/// ids / `created_at_ms`. `spawn_snapshot_record` used `on conflict (id)`, so
/// the `spawn_snapshots_git_oid_key` collision surfaced as a raw `23505`.
/// Now every writer gets the canonical id back and can move a ref through it.
#[test]
fn snapshot_record_returns_canonical_id_for_concurrent_same_git_oid() {
    let schema = common::Schema::new("d6_snap_race");
    let tree = sha256_of_hex("2a");
    let parent = sha256_of_hex("2b");
    let oid = oid_of_hex("c7");
    let first = schema.store();
    let parent_oid = oid_of_hex("c6");
    first
        .record_snapshot(&parent, &parent_oid, &tree, &[], "proj-race", 1)
        .expect("parent");

    let writers = 8_u8;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(usize::from(writers)));
    let handles: Vec<_> = (0..writers)
        .map(|i| {
            let store = schema.store();
            let barrier = barrier.clone();
            let (tree, parent, oid) = (tree.clone(), parent.clone(), oid.clone());
            std::thread::spawn(move || {
                let id = format!("sha256:{:02x}{}", i, "e".repeat(62));
                barrier.wait();
                let got = store
                    .record_snapshot(
                        &id,
                        &oid,
                        &tree,
                        std::slice::from_ref(&parent),
                        "proj-race",
                        1_700_000_000_000 + i64::from(i),
                    )
                    .expect("same commit recorded concurrently returns canonical id");
                store
                    .cas_ref(
                        "proj-race",
                        &format!("w/{i}"),
                        RefKind::Branch,
                        None,
                        &got,
                        "race",
                    )
                    .expect("canonical id satisfies the snapshot FK");
                got
            })
        })
        .collect();
    let ids: Vec<String> = handles
        .into_iter()
        .map(|h| h.join().expect("writer thread"))
        .collect();
    assert!(
        ids.windows(2).all(|w| w[0] == w[1]),
        "one canonical id: {ids:?}"
    );
    let row = first
        .get_snapshot_by_oid(&oid)
        .expect("by oid")
        .expect("row present");
    assert_eq!(row.id, ids[0]);

    // Sequential replay under yet another id and timestamp is idempotent too.
    let late = sha256_of_hex("9f");
    assert_eq!(
        first
            .record_snapshot(
                &late,
                &oid,
                &tree,
                std::slice::from_ref(&parent),
                "proj-race",
                7
            )
            .expect("late replay"),
        ids[0]
    );
    // Same id, other timestamp: first writer's created_at_ms wins.
    assert_eq!(
        first
            .record_snapshot(
                &ids[0],
                &oid,
                &tree,
                std::slice::from_ref(&parent),
                "proj-race",
                42
            )
            .expect("same id, other timestamp"),
        ids[0]
    );
    assert_eq!(
        first.get_snapshot(&ids[0]).expect("read").expect("row"),
        row
    );
    assert!(first.get_snapshot(&late).expect("read").is_none());
}

#[test]
fn snapshot_record_still_refuses_conflicting_git_oid_content() {
    let (store, _schema) = store("d6_snap_conflict");
    let first = sha256_of_hex("51");
    let other = sha256_of_hex("52");
    let tree = sha256_of_hex("53");
    let oid = oid_of_hex("d1");
    store
        .record_snapshot(&first, &oid, &tree, &[], "proj-c", 10)
        .expect("first");

    let mismatches: [(String, Vec<String>, &str); 3] = [
        (sha256_of_hex("54"), vec![], "proj-c"),
        (tree.clone(), vec![], "proj-other"),
        (tree.clone(), vec![first.clone()], "proj-c"),
    ];
    for (t, parents, project) in &mismatches {
        let err = store
            .record_snapshot(&other, &oid, t, parents, project, 11)
            .expect_err("conflicting git_oid content refused");
        assert!(
            matches!(err, StoreError::Backend(ref msg) if msg.contains("23505")),
            "{err:?}"
        );
    }
    // Same id, different git_oid: still refused.
    let err = store
        .record_snapshot(&first, &oid_of_hex("d2"), &tree, &[], "proj-c", 10)
        .expect_err("same id, other git_oid refused");
    assert!(matches!(err, StoreError::Backend(ref msg) if msg.contains("23505")));
    assert!(store.get_snapshot(&other).expect("read").is_none());
}

fn record_three_snapshots(store: &PostgresStore, project: &str) -> (String, String, String) {
    let tree = sha256_of_hex("aa");
    let snap1 = sha256_of_hex("11");
    let snap2 = sha256_of_hex("22");
    let snap3 = sha256_of_hex("33");
    store
        .record_snapshot(&snap1, &oid_of_hex("11"), &tree, &[], project, 100)
        .expect("snap1");
    store
        .record_snapshot(
            &snap2,
            &oid_of_hex("22"),
            &tree,
            std::slice::from_ref(&snap1),
            project,
            200,
        )
        .expect("snap2");
    store
        .record_snapshot(
            &snap3,
            &oid_of_hex("33"),
            &tree,
            std::slice::from_ref(&snap2),
            project,
            300,
        )
        .expect("snap3");
    (snap1, snap2, snap3)
}

#[test]
fn cas_ref_blocks_main_and_enforces_cas_and_append_only_log() {
    let (store, schema) = store("d6_cas_refs");
    let (snap1, snap2, snap3) = record_three_snapshots(&store, "proj-b");

    assert!(matches!(
        store.cas_ref("proj-b", "main", RefKind::Main, None, &snap1, "illegal"),
        Err(StoreError::Rejected(_))
    ));
    assert_eq!(
        store
            .cas_ref(
                "proj-b",
                "feat/x",
                RefKind::Branch,
                None,
                &snap1,
                "create branch"
            )
            .expect("create branch"),
        1
    );
    assert!(matches!(
        store.cas_ref("proj-b", "feat/x", RefKind::Branch, None, &snap2, "dup"),
        Err(StoreError::Rejected(_))
    ));
    assert!(matches!(
        store.cas_ref(
            "proj-b",
            "feat/x",
            RefKind::Branch,
            Some(&snap3),
            &snap2,
            "stale cas",
        ),
        Err(StoreError::Rejected(_))
    ));
    assert_eq!(
        store
            .cas_ref(
                "proj-b",
                "feat/x",
                RefKind::Branch,
                Some(&snap1),
                &snap2,
                "checkpoint",
            )
            .expect("advance branch"),
        2
    );
    assert_eq!(
        store
            .cas_ref(
                "proj-b",
                "wip/feat/x",
                RefKind::Wip,
                None,
                &snap3,
                "tmpfs flush"
            )
            .expect("create wip"),
        3
    );
    assert_eq!(store.latest_ref_log_seq().expect("latest seq"), 3);

    let refs = store.list_refs(Some("proj-b")).expect("list_refs");
    assert_eq!(refs.len(), 2);
    assert_eq!(
        (&refs[0].name, refs[0].kind, &refs[0].snapshot),
        (&"feat/x".to_string(), RefKind::Branch, &snap2)
    );
    assert_eq!(
        (&refs[1].name, refs[1].kind, &refs[1].snapshot),
        (&"wip/feat/x".to_string(), RefKind::Wip, &snap3)
    );

    let log = store.list_ref_log_after(1).expect("log after 1");
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].old_snapshot.as_deref(), Some(snap1.as_str()));
    assert_eq!(log[0].new_snapshot, snap2);
    assert_eq!(log[1].old_snapshot, None);
    assert_eq!(log[1].new_snapshot, snap3);

    let mut client = admin(&schema);
    assert_forbidden_sqlstate(client.execute(
        "update spawn_ref_log set reason = 'tampered' where seq = 1",
        &[],
    ));
    assert_forbidden_sqlstate(client.execute("delete from spawn_ref_log where seq = 1", &[]));
    assert_forbidden_sqlstate(client.batch_execute("truncate spawn_ref_log").map(|()| 0));
}

#[test]
fn land_main_ref_requires_exact_accepted_gate_verdict_and_blocks_cas_ref() {
    let (store, _schema) = store("d6_land_main");
    let tree = sha256_of_hex("bb");
    let (base_snap, base_oid) = (sha256_of_hex("10"), oid_of_hex("1a"));
    let (cand_snap, cand_oid) = (sha256_of_hex("20"), oid_of_hex("2b"));
    let (wrong_snap, wrong_oid) = (sha256_of_hex("30"), oid_of_hex("3c"));

    store
        .record_snapshot(&base_snap, &base_oid, &tree, &[], "proj-main", 100)
        .expect("base_snap");
    store
        .record_snapshot(
            &cand_snap,
            &cand_oid,
            &tree,
            std::slice::from_ref(&base_snap),
            "proj-main",
            200,
        )
        .expect("cand_snap");
    store
        .record_snapshot(
            &wrong_snap,
            &wrong_oid,
            &tree,
            std::slice::from_ref(&base_snap),
            "proj-main",
            300,
        )
        .expect("wrong_snap");

    assert_eq!(
        store
            .land_main_ref("proj-main", "main", None, &base_snap, None, "bootstrap")
            .expect("bootstrap main"),
        1
    );
    assert!(matches!(
        store.land_main_ref("proj-main", "main", None, &cand_snap, None, "re-bootstrap"),
        Err(StoreError::Rejected(_))
    ));
    assert!(matches!(
        store.cas_ref(
            "proj-main",
            "main",
            RefKind::Branch,
            Some(&base_snap),
            &cand_snap,
            "bypass",
        ),
        Err(StoreError::Rejected(_))
    ));
    assert!(matches!(
        store.land_main_ref(
            "proj-main",
            "main",
            Some(&base_snap),
            &cand_snap,
            None,
            "no-task"
        ),
        Err(StoreError::Rejected(_))
    ));

    let task = setup_accepted_task(&store, "/tmp/proj-main.git", &base_oid, &cand_oid);
    assert!(matches!(
        store.land_main_ref(
            "proj-main",
            "main",
            Some(&base_snap),
            &wrong_snap,
            Some(task),
            "wrong-cand",
        ),
        Err(StoreError::Rejected(_))
    ));
    assert_eq!(
        store
            .land_main_ref(
                "proj-main",
                "main",
                Some(&base_snap),
                &cand_snap,
                Some(task),
                "gate accepted",
            )
            .expect("land main"),
        2
    );

    let refs = store.list_refs(Some("proj-main")).expect("list_refs");
    assert_eq!(refs.len(), 1);
    assert_eq!(
        (refs[0].kind, &refs[0].snapshot),
        (RefKind::Main, &cand_snap)
    );
}

#[test]
fn retire_project_refs_allows_rebootstrap_while_preserving_main_gate_guard() {
    let (store, _schema) = store("d6_retire_project");
    let (s1, s2, s3) = record_three_snapshots(&store, "proj-rb");
    let other_snap = sha256_of_hex("99");
    store
        .record_snapshot(
            &other_snap,
            &oid_of_hex("99"),
            &sha256_of_hex("aa"),
            &[],
            "proj-keep",
            100,
        )
        .expect("other snapshot");
    store
        .land_main_ref("proj-keep", "main", None, &other_snap, None, "keep main")
        .expect("bootstrap other project");

    store
        .land_main_ref("proj-rb", "main", None, &s1, None, "initial cutover")
        .expect("bootstrap proj-rb main");
    store
        .cas_ref("proj-rb", "feat/a", RefKind::Branch, None, &s2, "branch")
        .expect("cas branch");
    store
        .cas_ref("proj-rb", "wip/a", RefKind::Wip, None, &s2, "wip")
        .expect("cas wip");
    store
        .cas_ref("proj-rb", "tags/v1", RefKind::Tag, None, &s1, "tag")
        .expect("cas tag");

    assert!(matches!(
        store.land_main_ref("proj-rb", "main", Some(&s1), &s2, None, "no task"),
        Err(StoreError::Rejected(_))
    ));
    assert!(store.retire_project_refs("   ", "rollback").is_err());

    let seq_before = store.latest_ref_log_seq().expect("seq before");
    let retired = store
        .retire_project_refs("proj-rb", "rollback to git")
        .expect("retire project refs");
    assert_eq!(retired, 4);
    assert!(store.list_refs(Some("proj-rb")).expect("empty").is_empty());
    assert_eq!(store.list_refs(Some("proj-keep")).expect("kept").len(), 1);
    for id in [&s1, &s2, &s3] {
        assert_eq!(
            store.get_snapshot(id).expect("snap").expect("exists").state,
            SnapshotState::Live
        );
    }

    let audit = store
        .list_ref_log_after(i64::try_from(seq_before).expect("seq fits i64"))
        .expect("audit log");
    assert_eq!(audit.len(), 4);
    assert!(
        audit
            .iter()
            .all(|row| row.project == "proj-rb" && row.reason == "rollback to git")
    );
    assert_eq!(
        store
            .retire_project_refs("proj-rb", "repeat")
            .expect("repeat"),
        0
    );

    // Re-cutover re-bootstraps `main` at the newer Git commit (`s2`) while
    // retaining full `spawn_ref_land_main` gate protection thereafter.
    store
        .land_main_ref("proj-rb", "main", None, &s2, None, "re-cutover")
        .expect("re-bootstrap main");
    assert!(matches!(
        store.land_main_ref("proj-rb", "main", None, &s3, None, "dup bootstrap"),
        Err(StoreError::Rejected(_))
    ));
    assert!(matches!(
        store.land_main_ref(
            "proj-rb",
            "main",
            Some(&s2),
            &s3,
            None,
            "unapproved advance"
        ),
        Err(StoreError::Rejected(_))
    ));

    let task = setup_accepted_task(
        &store,
        "/tmp/proj-rb.git",
        &oid_of_hex("22"),
        &oid_of_hex("33"),
    );
    store
        .land_main_ref("proj-rb", "main", Some(&s2), &s3, Some(task), "gated land")
        .expect("gated advance succeeds after re-cutover");
}

#[test]
fn mark_snapshots_collected_skips_referenced_snapshots_and_forbids_reuse() {
    let (store, _schema) = store("d6_collect");
    let tree = sha256_of_hex("cc");
    let live_snap = sha256_of_hex("a1");
    let unref_snap = sha256_of_hex("a2");

    store
        .record_snapshot(&live_snap, &oid_of_hex("a1"), &tree, &[], "proj-gc", 10)
        .expect("live_snap");
    store
        .record_snapshot(&unref_snap, &oid_of_hex("a2"), &tree, &[], "proj-gc", 20)
        .expect("unref_snap");
    store
        .cas_ref(
            "proj-gc",
            "branch/live",
            RefKind::Branch,
            None,
            &live_snap,
            "pin via ref",
        )
        .expect("create ref");

    // Passing both IDs collects only the unreferenced snapshot.
    let marked = store
        .mark_snapshots_collected(&[live_snap.clone(), unref_snap.clone()])
        .expect("mark_snapshots_collected");
    assert_eq!(marked, 1);

    assert_eq!(
        store
            .get_snapshot(&live_snap)
            .expect("get live")
            .expect("exists")
            .state,
        SnapshotState::Live
    );
    assert_eq!(
        store
            .get_snapshot(&unref_snap)
            .expect("get collected")
            .expect("exists")
            .state,
        SnapshotState::Collected
    );

    // Repeating is a no-op (returns 0).
    let again = store
        .mark_snapshots_collected(&[live_snap.clone(), unref_snap.clone()])
        .expect("mark again");
    assert_eq!(again, 0);

    // Pointing a ref to a collected snapshot is refused.
    let err = store
        .cas_ref(
            "proj-gc",
            "branch/revive",
            RefKind::Branch,
            None,
            &unref_snap,
            "revive collected",
        )
        .expect_err("cannot point ref at collected snapshot");
    assert!(matches!(err, StoreError::Backend(ref msg) if msg.contains("55000")));
}

#[test]
fn restricted_roles_and_database_identity_obey_least_privilege() {
    let (store, schema) = store("d6_roles");
    let snap = sha256_of_hex("f1");
    let oid = oid_of_hex("f1");
    let tree = sha256_of_hex("f2");
    store
        .record_snapshot(&snap, &oid, &tree, &[], "proj-r", 1)
        .expect("record");
    store
        .cas_ref("proj-r", "branch/r", RefKind::Branch, None, &snap, "init")
        .expect("cas_ref");

    let ident = store.database_identity().expect("database_identity");
    assert!(!ident.db_system_id.trim().is_empty());
    assert!(ident.db_oid > 0);
    assert_eq!(ident.schema_name, schema.as_ref());

    // Reader and supervisor may SELECT all three tables; neither may mutate or call writers.
    for role in ["spawn_reader", "spawn_supervisor"] {
        let mut conn = as_login(&schema, role);
        let snap_count: i64 = conn
            .query_one("select count(*) from spawn_snapshots", &[])
            .expect("select spawn_snapshots")
            .get(0);
        let ref_count: i64 = conn
            .query_one("select count(*) from spawn_refs", &[])
            .expect("select spawn_refs")
            .get(0);
        let log_count: i64 = conn
            .query_one("select count(*) from spawn_ref_log", &[])
            .expect("select spawn_ref_log")
            .get(0);
        assert_eq!((snap_count, ref_count, log_count), (1, 1, 1));

        for sql in [
            "select spawn_snapshot_record($1, $2, $3, '{}'::text[], 'p', 1)",
            "select spawn_ref_cas('p', 'b', 'branch', null, $1, 'r')",
            "select spawn_ref_land_main('p', 'main', null, $1, null, 'r')",
            "select spawn_ref_retire_project('p', 'rollback')",
            "select spawn_snapshot_mark_collected(array[$1])",
        ] {
            let err = match sql {
                s if s.contains("spawn_snapshot_record") => {
                    conn.query_one(s, &[&sha256_of_hex("f3"), &oid_of_hex("f3"), &tree])
                }
                s if s.contains("spawn_ref_retire_project") => conn.query_one(s, &[]),
                _ => conn.query_one(sql, &[&snap]),
            }
            .expect_err("restricted login must not execute owner writer");
            assert_eq!(
                err.as_db_error().map(|db| db.code().code()),
                Some("42501"),
                "{role} calling {sql}"
            );
        }
    }

    // Human and node gateway logins have zero access to the three tables and writers.
    for role in ["spawn_human", "spawn_node_gateway"] {
        let mut conn = as_login(&schema, role);
        for table in ["spawn_snapshots", "spawn_refs", "spawn_ref_log"] {
            let err = conn
                .query_one(&format!("select count(*) from {table}"), &[])
                .expect_err("human/node_gateway must not select D6 tables");
            assert_eq!(
                err.as_db_error().map(|db| db.code().code()),
                Some("42501"),
                "{role} selecting {table}"
            );
        }
        let err = conn
            .query_one(
                "select spawn_ref_cas('p', 'b', 'branch', null, $1, 'r')",
                &[&snap],
            )
            .expect_err("human/node_gateway must not execute owner writer");
        assert_eq!(err.as_db_error().map(|db| db.code().code()), Some("42501"));
        let err = conn
            .query_one("select spawn_ref_retire_project('p', 'rollback')", &[])
            .expect_err("human/node_gateway must not retire refs");
        assert_eq!(err.as_db_error().map(|db| db.code().code()), Some("42501"));
    }
}
