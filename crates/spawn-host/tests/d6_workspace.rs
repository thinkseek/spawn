//! D6 Phase 4 Step 5 live PostgreSQL + CAS end-to-end integration tests (`#113` & `#95`):
//! - Full `WorkspaceStore` contract on pure-Rust [`D6Workspace<PgRefLedger>`]: `ensure_branch`,
//!   `mount` / `mount_when`, warm Layer 0 cache reuse & incremental `nearest` hard-linking without
//!   linking `<cas>/objects`, `canonical`, `is_mounted`, executables/symlinks/nested `.gitignore`,
//!   oversized file notices, `checkpoint` / `commit`, `land_main_snapshot`, clean & conflicting
//!   `merge_base` (`diff3` + `.merge/*`), `hold` locking, `set_aside` / `set_aside_when`, `unmount`,
//!   and `discard`.
//! - Live dual-track oracle [`ShadowD6Workspace<GitWorkspace, PgRefLedger>`] achieving 100% tree
//!   OID & commit OID parity across checkpoints, clean merges, conflicting merges + resolution,
//!   `hold`, `set_aside`, and `discard` while verifying PostgreSQL `spawn_refs`/`spawn_snapshots`.
//! - Positive (0 leaks) and explicit negative (`SimulateUnfilteredScan`) fail-closed
//!   `TrackedSetLeak` verification ensuring ignored files never enter PG `spawn_refs`/`spawn_snapshots`.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use serde_json::json;
use spawn_cas::{Digest, GitOid, Snapshot, Tree, TreeEntry, TreeEntryKind, TreeMutation};
use spawn_core::id::{BranchName, CommitId};
use spawn_core::port::{
    BaseMerge, Branch, Commit, MERGE_WAITS, MOUNT_CANCELLED, SetAside, UNMOUNT_IN_USE,
    WorkspaceError, WorkspaceStore,
};
use spawn_host::{
    CasMerkleStore, D6RefKind, D6StoreError, D6Workspace, GitWorkspace, LedgerRef, LedgerSnapshot,
    RefLedger, SET_ASIDE_WAITS, ShadowCheckpointOutcome, ShadowD6Workspace, ShadowTrackedSetMode,
    format_commit_body, test_git_command,
};
use spawn_postgres::{PostgresStore, RefKind};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "spawn-host-step5-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create scratch dir");
        Self(path)
    }

    fn join(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct PgRefLedger {
    pg: PostgresStore,
}

impl PgRefLedger {
    fn connect_isolated(case: &str) -> Self {
        static SCHEMA_SEQ: AtomicU64 = AtomicU64::new(0);
        let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
            "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
        );
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let seq = SCHEMA_SEQ.fetch_add(1, Ordering::Relaxed);
        let schema = format!("spawn_test_{}_{nanos:x}_{seq}_{case}", std::process::id());
        let pg = PostgresStore::connect_in_schema(&url, &schema).expect("connect_in_schema");
        Self { pg }
    }
}

fn to_pg_kind(kind: D6RefKind) -> RefKind {
    match kind {
        D6RefKind::Main => RefKind::Main,
        D6RefKind::Branch => RefKind::Branch,
        D6RefKind::Wip => RefKind::Wip,
        D6RefKind::Tag => RefKind::Tag,
    }
}

fn from_pg_kind(kind: RefKind) -> D6RefKind {
    match kind {
        RefKind::Main => D6RefKind::Main,
        RefKind::Branch => D6RefKind::Branch,
        RefKind::Wip => D6RefKind::Wip,
        RefKind::Tag => D6RefKind::Tag,
    }
}

impl RefLedger for PgRefLedger {
    fn record_snapshot(
        &self,
        id: &Digest,
        git_oid: &GitOid,
        tree: &Digest,
        parents: &[Digest],
        project: &str,
        created_at_ms: i64,
    ) -> Result<Digest, D6StoreError> {
        let parent_strs: Vec<String> = parents.iter().map(Digest::to_string).collect();
        let out = self
            .pg
            .record_snapshot(
                &id.to_string(),
                &git_oid.to_hex(),
                &tree.to_string(),
                &parent_strs,
                project,
                created_at_ms,
            )
            .map_err(|e| D6StoreError::Ledger(e.to_string()))?;
        Ok(Digest::parse(&out)?)
    }

    fn get_snapshot(&self, id: &Digest) -> Result<Option<LedgerSnapshot>, D6StoreError> {
        let row = self
            .pg
            .get_snapshot(&id.to_string())
            .map_err(|e| D6StoreError::Ledger(e.to_string()))?;
        row.map(|r| {
            let parents = r
                .parents
                .iter()
                .map(|p| Digest::parse(p))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(LedgerSnapshot {
                id: Digest::parse(&r.id)?,
                git_oid: GitOid::parse_hex(&r.git_oid)?,
                tree: Digest::parse(&r.tree)?,
                parents,
                project: r.project,
                created_at_ms: r.created_at_ms,
            })
        })
        .transpose()
    }

    fn get_snapshot_by_oid(
        &self,
        git_oid: &GitOid,
    ) -> Result<Option<LedgerSnapshot>, D6StoreError> {
        let row = self
            .pg
            .get_snapshot_by_oid(&git_oid.to_hex())
            .map_err(|e| D6StoreError::Ledger(e.to_string()))?;
        row.map(|r| {
            let parents = r
                .parents
                .iter()
                .map(|p| Digest::parse(p))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(LedgerSnapshot {
                id: Digest::parse(&r.id)?,
                git_oid: GitOid::parse_hex(&r.git_oid)?,
                tree: Digest::parse(&r.tree)?,
                parents,
                project: r.project,
                created_at_ms: r.created_at_ms,
            })
        })
        .transpose()
    }

    fn cas_ref(
        &self,
        project: &str,
        name: &str,
        kind: D6RefKind,
        expected: Option<&Digest>,
        new_snapshot: &Digest,
        reason: &str,
    ) -> Result<u64, D6StoreError> {
        let exp_str = expected.map(Digest::to_string);
        let new_str = new_snapshot.to_string();
        let seq = self
            .pg
            .cas_ref(
                project,
                name,
                to_pg_kind(kind),
                exp_str.as_deref(),
                &new_str,
                reason,
            )
            .map_err(|e| D6StoreError::Ledger(e.to_string()))?;
        u64::try_from(seq).map_err(|_| D6StoreError::Ledger("negative seq".to_string()))
    }

    fn land_main_ref(
        &self,
        project: &str,
        name: &str,
        expected_base: Option<&Digest>,
        new_snapshot: &Digest,
        task_id: Option<i64>,
        reason: &str,
    ) -> Result<u64, D6StoreError> {
        let exp_str = expected_base.map(Digest::to_string);
        let new_str = new_snapshot.to_string();
        let seq = self
            .pg
            .land_main_ref(project, name, exp_str.as_deref(), &new_str, task_id, reason)
            .map_err(|e| D6StoreError::Ledger(e.to_string()))?;
        u64::try_from(seq).map_err(|_| D6StoreError::Ledger("negative seq".to_string()))
    }

    fn list_refs(&self, project: Option<&str>) -> Result<Vec<LedgerRef>, D6StoreError> {
        let rows = self
            .pg
            .list_refs(project)
            .map_err(|e| D6StoreError::Ledger(e.to_string()))?;
        rows.into_iter()
            .map(|r| {
                Ok(LedgerRef {
                    project: r.project,
                    name: r.name,
                    kind: from_pg_kind(r.kind),
                    snapshot: Digest::parse(&r.snapshot)?,
                    updated_at_ms: r.updated_at_ms,
                })
            })
            .collect()
    }
}

fn setup_accepted_task(
    store: &PostgresStore,
    source: &str,
    base_oid: &GitOid,
    candidate_oid: &GitOid,
) -> i64 {
    let mission = store
        .mission_add("D6 step5 gate", &json!([source]), "human")
        .expect("mission_add");
    store
        .mission_configure(mission, Some("/scenarios"), "gate", None, None)
        .expect("mission_configure");
    let task = store
        .task_assign(
            mission,
            "ephemeral",
            "w_d6_step5",
            "land D6 commit",
            "default",
        )
        .expect("task_assign");
    store.task_delivered(task, 1).expect("task_delivered");
    assert!(store.task_reported(task, "done").expect("task_reported"));
    store
        .integrator_record(&json!({
            "type": "task.gated",
            "id": task,
            "revision": 0,
            "verdict": "accepted",
            "base": base_oid.to_hex(),
            "candidate": candidate_oid.to_hex(),
        }))
        .expect("integrator_record accepted");
    task
}

fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = test_git_command()
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
#[expect(clippy::too_many_lines)]
fn d6_workspace_pg_full_lifecycle_layer0_hold_merge_set_aside_and_discard() {
    let scratch = Scratch::new("pg_d6_ws");
    let cas = Arc::new(CasMerkleStore::open(scratch.join("cas")).expect("open cas"));
    let ledger = Arc::new(PgRefLedger::connect_isolated("d6_ws_lifecycle"));
    let clock_ms = Arc::new(AtomicI64::new(1_700_100_000_000));
    let c = clock_ms.clone();
    let d6_root = scratch.join("d6-work");

    let store = D6Workspace::new("proj-d6", &d6_root, cas.clone(), ledger.clone())
        .with_max_file_bytes(1024)
        .with_clock_ms(Arc::new(move || c.fetch_add(1_000, Ordering::Relaxed)));
    assert_eq!(store.project(), "proj-d6");

    // 1. ensure_branch bootstraps main & handles agent / commit forks & validation
    let b1 = BranchName::new("agent_1").expect("b1");
    let created = store.ensure_branch(&b1, None).expect("ensure_branch b1");
    assert!(matches!(created, Branch::Created(_)));
    assert_eq!(
        store.ensure_branch(&b1, None).expect("idempotent"),
        Branch::Existing
    );

    let main0 = store.main_snapshot().expect("main0").expect("seeded");
    assert_eq!(
        store
            .branch_head(&b1)
            .expect("head")
            .as_ref()
            .map(CommitId::as_str),
        Some(main0.git_oid.to_hex().as_str())
    );

    // Invalid from inputs are refused cleanly
    let b_bad = BranchName::new("agent_bad").expect("bad");
    for bad in ["-rf", "agent_missing", &main0.git_oid.to_hex()[..12]] {
        assert!(store.ensure_branch(&b_bad, Some(bad)).is_err());
    }

    // 2. Land seed tree onto main in PG via accepted landing task
    let (readme_d, readme_len) = cas.put_bytes_blob(b"line1\nline2\nline3\n").expect("blob");
    let (gi_root_d, gi_root_len) = cas.put_bytes_blob(b".env\n").expect("gi");
    let (gi_sub_d, gi_sub_len) = cas.put_bytes_blob(b"*.local\n").expect("sub gi");
    let (sub_keep_d, sub_keep_len) = cas.put_bytes_blob(b"keep\n").expect("keep");
    let sub_tree = cas
        .put_tree_cached(
            &Tree::from_entries(vec![
                TreeEntry::file(b".gitignore".to_vec(), &gi_sub_d, gi_sub_len),
                TreeEntry::file(b"keep.txt".to_vec(), &sub_keep_d, sub_keep_len),
            ])
            .expect("sub tree"),
        )
        .expect("put sub tree");
    let t_seed = cas
        .put_tree_cached(
            &Tree::from_entries(vec![
                TreeEntry::file(b".gitignore".to_vec(), &gi_root_d, gi_root_len),
                TreeEntry::file(b"README.md".to_vec(), &readme_d, readme_len),
                TreeEntry::dir(b"sub".to_vec(), &sub_tree),
            ])
            .expect("seed tree"),
        )
        .expect("put seed tree");

    let seed_ms = clock_ms.fetch_add(1_000, Ordering::Relaxed);
    let seed_body = format_commit_body(
        &format!("spawn <s@l> {} +0000", seed_ms / 1000),
        &format!("spawn <s@l> {} +0000", seed_ms / 1000),
        "seed repo\n",
    );
    let snap_seed = cas
        .cas()
        .put_merkle_snapshot(&Snapshot {
            tree: t_seed.clone(),
            parents: vec![main0.id.clone()],
            commit_body: seed_body,
        })
        .expect("put seed snap");
    let parent_oids = cas
        .resolve_parent_oids(ledger.as_ref(), std::slice::from_ref(&main0.id))
        .expect("parent oids");
    let seed_git_oid = cas
        .project_snapshot_oid(
            &cas.cas().read_merkle_snapshot(&snap_seed).expect("read"),
            &parent_oids,
        )
        .expect("oid");
    cas.cas().sync_store().expect("sync");
    ledger
        .record_snapshot(
            &snap_seed,
            &seed_git_oid,
            &t_seed,
            std::slice::from_ref(&main0.id),
            "proj-d6",
            seed_ms,
        )
        .expect("record seed");
    let task1 = setup_accepted_task(
        &ledger.pg,
        "/tmp/proj-d6-seed.git",
        &main0.git_oid,
        &seed_git_oid,
    );
    store
        .land_main_snapshot(Some(&main0.id), &snap_seed, Some(task1), "seed main")
        .expect("land main seed");

    // 3. mount_when cancellation & mount + warm Layer 0 isolation check
    let b2 = BranchName::new("agent_2").expect("b2");
    store.ensure_branch(&b2, None).expect("b2 created");
    let cancelled = store.mount_when(&b2, &|| Err("cancelled before install".to_string()));
    assert!(matches!(
        cancelled,
        Err(WorkspaceError::Refused { ref code, .. }) if code == MOUNT_CANCELLED
    ));
    assert!(!d6_root.join("agent_2").exists());

    let ws2 = store.mount(&b2).expect("mount b2");
    assert!(store.is_mounted(&ws2));
    assert_eq!(store.canonical(&ws2), ws2);
    let ws2_dir = PathBuf::from(ws2.path());

    // Warm Layer 0 materialized under <root>/.d6/layers/<tree_hex>, distinct inode from CAS object
    let warm_readme = store.layer_path(&t_seed).join("README.md");
    let cas_readme = cas.cas().object_path(&readme_d);
    assert_ne!(
        fs::symlink_metadata(&warm_readme).expect("warm").ino(),
        fs::symlink_metadata(&cas_readme).expect("cas").ino()
    );
    // Writable workspace file is isolated from warm Layer 0 (editing workspace doesn't alter warm layer)
    fs::write(ws2_dir.join("README.md"), "line1\nbranch line\nline3\n").expect("edit");
    assert_eq!(
        fs::read_to_string(&warm_readme).expect("warm untouched"),
        "line1\nline2\nline3\n"
    );

    // 4. Executable (0755), symlink, nested ignored files, runtime files, and oversized cap notices
    fs::write(ws2_dir.join("run.sh"), "#!/bin/sh\necho d6\n").expect("sh");
    fs::set_permissions(ws2_dir.join("run.sh"), fs::Permissions::from_mode(0o755)).expect("chmod");
    std::os::unix::fs::symlink("README.md", ws2_dir.join("readme.link")).expect("symlink");
    fs::write(ws2_dir.join(".env"), "IGNORED=yes\n").expect("env");
    fs::write(ws2_dir.join("sub/ignored.local"), "IGNORED_LOCAL=1\n").expect("local");
    fs::create_dir_all(ws2_dir.join("target")).expect("target");
    fs::write(ws2_dir.join("target/artifact.o"), "bin").expect("target file");
    fs::create_dir_all(ws2_dir.join(".claude")).expect("claude");
    fs::write(ws2_dir.join(".claude/scheduled_tasks.lock"), "123\n").expect("runtime");
    fs::write(ws2_dir.join("big.bin"), vec![9_u8; 2048]).expect("oversized");

    let Commit::Created(c1) = store
        .checkpoint(&ws2, "work checkpoint")
        .expect("checkpoint")
    else {
        panic!("expected Created");
    };
    assert_eq!(store.commit(&ws2, "noop").expect("noop"), Commit::Unchanged);

    // Notices surfaced for oversized file & marked told once
    let notices = store.notices(&ws2);
    assert_eq!(notices.len(), 1);
    assert!(notices[0].starts_with("big.bin"));
    store.notices_told(&ws2, &notices);
    assert!(store.notices(&ws2).is_empty());

    // Inspect PG spawn_snapshots & spawn_refs directly
    let pg_ref = ledger
        .pg
        .list_refs(Some("proj-d6"))
        .expect("list refs")
        .into_iter()
        .find(|r| r.name == "agent_2")
        .expect("agent_2 ref");
    let pg_snap = ledger
        .pg
        .get_snapshot(&pg_ref.snapshot)
        .expect("get_snapshot")
        .expect("snap");
    assert_eq!(pg_snap.git_oid, c1.as_str());
    let flat_b2 = cas
        .flatten_tree(&Digest::parse(&pg_snap.tree).expect("tree"))
        .expect("flatten");
    assert!(!flat_b2.contains_key(b".env".as_slice()));
    assert!(!flat_b2.contains_key(b"sub/ignored.local".as_slice()));
    assert!(!flat_b2.contains_key(b"target/artifact.o".as_slice()));
    assert!(!flat_b2.contains_key(b".claude/scheduled_tasks.lock".as_slice()));
    assert!(!flat_b2.contains_key(b"big.bin".as_slice()));
    assert_eq!(
        flat_b2[b"run.sh".as_slice()].kind,
        TreeEntryKind::Executable
    );
    assert_eq!(
        flat_b2[b"readme.link".as_slice()].kind,
        TreeEntryKind::Symlink
    );

    // Fork agent_3 from agent_2's commit OID & verify incremental Layer 0 hardlink reuse from nearest
    let b3 = BranchName::new("agent_3").expect("b3");
    store
        .ensure_branch(&b3, Some(c1.as_str()))
        .expect("fork from commit");
    let ws3 = store.mount(&b3).expect("mount b3");
    let layer_report = store.last_layer0_report().expect("layer0 report");
    assert!(
        layer_report.hardlinked_from_nearest >= 1 || layer_report.reused_existing_dest,
        "expected warm layer incremental reuse: {layer_report:?}"
    );
    assert_eq!(
        fs::read_to_string(Path::new(ws3.path()).join("run.sh")).expect("sh"),
        "#!/bin/sh\necho d6\n"
    );
    assert_eq!(
        fs::symlink_metadata(Path::new(ws3.path()).join("run.sh"))
            .expect("mode")
            .permissions()
            .mode()
            & 0o111,
        0o111
    );

    // Test discard on ws3
    store.discard(&ws3).expect("discard ws3");
    assert!(!store.is_mounted(&ws3));

    // 5. Land main edit (conflict on README.md) and test merge_base_when + hold + diff3 conflict
    let (main_conf_d, main_conf_len) = cas
        .put_bytes_blob(b"line1\nmain line\nline3\n")
        .expect("main blob");
    let t_main2 = cas
        .apply_mutations(
            &t_seed,
            &[TreeMutation::Upsert {
                path: vec![b"README.md".to_vec()],
                kind: TreeEntryKind::File,
                id: main_conf_d.as_bytes(),
                size: main_conf_len,
            }],
        )
        .expect("mutate");
    let main2_ms = clock_ms.fetch_add(1_000, Ordering::Relaxed);
    let main2_body = format_commit_body(
        &format!("spawn <s@l> {} +0000", main2_ms / 1000),
        &format!("spawn <s@l> {} +0000", main2_ms / 1000),
        "main conflict commit\n",
    );
    let snap_main2 = cas
        .cas()
        .put_merkle_snapshot(&Snapshot {
            tree: t_main2.clone(),
            parents: vec![snap_seed.clone()],
            commit_body: main2_body,
        })
        .expect("put snap_main2");
    let main2_oid = cas
        .project_snapshot_oid(
            &cas.cas().read_merkle_snapshot(&snap_main2).expect("read"),
            &[seed_git_oid],
        )
        .expect("oid");
    cas.cas().sync_store().expect("sync");
    ledger
        .record_snapshot(
            &snap_main2,
            &main2_oid,
            &t_main2,
            std::slice::from_ref(&snap_seed),
            "proj-d6",
            main2_ms,
        )
        .expect("record main2");
    let task2 = setup_accepted_task(
        &ledger.pg,
        "/tmp/proj-d6-main2.git",
        &seed_git_oid,
        &main2_oid,
    );
    store
        .land_main_snapshot(Some(&snap_seed), &snap_main2, Some(task2), "main2 conflict")
        .expect("land main2");

    // merge_base_when refused when ready returns Err -> MERGE_WAITS
    let waits = store.merge_base_when(&ws2, &|| Err("engine busy".to_string()));
    assert!(matches!(
        waits,
        Err(WorkspaceError::Refused { ref code, .. }) if code == MERGE_WAITS
    ));

    // Hold lock blocks cross-thread merge_base until dropped
    let held = store.hold(&ws2).expect("hold acquired");
    let store_ref = Arc::new(store);
    let ws2_clone = ws2.clone();
    let store_bg = Arc::clone(&store_ref);
    let (tx, rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let res = store_bg.merge_base(&ws2_clone);
        let _ = tx.send(res);
    });
    assert!(
        rx.recv_timeout(Duration::from_millis(80)).is_err(),
        "merge_base must wait while hold guard is active"
    );
    drop(held);
    let merge_res = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("merge_base finishes after hold drop")
        .expect("merge_base ok");
    handle.join().expect("join");

    assert!(matches!(
        merge_res,
        BaseMerge::Conflicted {
            ref files,
            marked: true
        } if files == &vec!["README.md".to_string()]
    ));
    assert!(store_ref.has_pending_merge("agent_2"));
    let conflict_text = fs::read_to_string(ws2_dir.join("README.md")).expect("read marked");
    assert!(conflict_text.contains("<<<<<<<") && conflict_text.contains(">>>>>>>"));

    // Resolve markers & checkpoint -> 2-parent merge commit in PG spawn_snapshots
    fs::write(ws2_dir.join("README.md"), "line1\nresolved line\nline3\n").expect("resolve");
    let Commit::Created(resolved_oid) = store_ref
        .checkpoint(&ws2, "resolved conflict")
        .expect("commit resolution")
    else {
        panic!("expected Created merge commit");
    };
    assert!(!store_ref.has_pending_merge("agent_2"));
    let resolved_row = ledger
        .get_snapshot_by_oid(&GitOid::parse_hex(resolved_oid.as_str()).expect("oid"))
        .expect("get")
        .expect("row");
    assert_eq!(resolved_row.parents.len(), 2);
    assert_eq!(resolved_row.parents[1], snap_main2);

    // 6. set_aside_when refusal & success + unmount_when refusal & unmount
    let sa_waits = store_ref.set_aside_when(&b2, None, "cancel", &|| Err("typing".to_string()));
    assert!(matches!(
        sa_waits,
        Err(WorkspaceError::Refused { ref code, .. }) if code == SET_ASIDE_WAITS
    ));
    let sa = store_ref
        .set_aside_when(&b2, Some(&seed_git_oid.to_hex()), "cancel task", &|| Ok(()))
        .expect("set_aside");
    assert!(matches!(sa, SetAside::Created { .. }));
    assert_eq!(
        store_ref
            .set_aside_when(&b2, Some(&seed_git_oid.to_hex()), "cancel task", &|| Ok(()))
            .expect("retry"),
        sa
    );

    let unmount_denied = store_ref.unmount_when(&ws2, &|| {
        Err(WorkspaceError::Refused {
            code: UNMOUNT_IN_USE.to_string(),
            message: "live pane".to_string(),
        })
    });
    assert!(matches!(
        unmount_denied,
        Err(WorkspaceError::Refused { ref code, .. }) if code == UNMOUNT_IN_USE
    ));
    assert!(store_ref.is_mounted(&ws2));

    store_ref.unmount(&ws2).expect("unmount");
    assert!(!store_ref.is_mounted(&ws2));
}

#[test]
#[expect(clippy::too_many_lines)]
fn shadow_d6_workspace_pg_dual_track_oracle_parity() {
    let scratch = Scratch::new("pg_shadow_oracle");
    let checkout = scratch.join("repo");
    fs::create_dir_all(checkout.join("sub")).expect("create repo");
    git_in(&checkout, &["init", "-q", "--initial-branch=main"]);
    fs::write(checkout.join(".gitignore"), ".env\n*.tmp\n").expect("gi");
    fs::write(checkout.join("sub/.gitignore"), "*.local\n").expect("sub gi");
    fs::write(checkout.join("README.md"), "alpha\nbeta\ngamma\n").expect("readme");
    fs::write(checkout.join("sub/clean.txt"), "initial\n").expect("clean");
    git_in(&checkout, &["add", "-A"]);
    git_in(&checkout, &["commit", "-qm", "initial base"]);
    let init_main_oid = GitOid::parse_hex(&git_in(&checkout, &["rev-parse", "main"])).expect("oid");

    let bare_repo = checkout.join(".git");
    let primary = GitWorkspace::new(&bare_repo, scratch.join("git-work"));
    let cas = Arc::new(CasMerkleStore::open(scratch.join("cas")).expect("cas"));
    let ledger = Arc::new(PgRefLedger::connect_isolated("shadow_parity"));
    let d6 = D6Workspace::new(
        "proj-shadow",
        scratch.join("d6-work"),
        cas.clone(),
        ledger.clone(),
    );
    let oracle = ShadowD6Workspace::new(primary, d6);

    // ensure_branch + mount + mount_when
    let b1 = BranchName::new("agent_alpha").expect("branch");
    oracle.ensure_branch(&b1, None).expect("ensure_branch");
    let ws = oracle.mount(&b1).expect("mount");
    assert!(oracle.is_mounted(&ws));
    let ws_dir = PathBuf::from(ws.path());

    // Write regular file, executable (0755), symlink, and ignored files (.env, sub/*.local, target, .claude lock)
    fs::write(ws_dir.join("README.md"), "alpha\nbranch change\ngamma\n").expect("edit");
    fs::write(ws_dir.join("tool.sh"), "#!/bin/sh\necho parity\n").expect("sh");
    fs::set_permissions(ws_dir.join("tool.sh"), fs::Permissions::from_mode(0o755)).expect("chmod");
    std::os::unix::fs::symlink("README.md", ws_dir.join("README.link")).expect("symlink");
    fs::write(ws_dir.join(".env"), "API_KEY=secret\n").expect("env");
    fs::write(ws_dir.join("sub/cache.local"), "ignored local\n").expect("local");
    fs::create_dir_all(ws_dir.join("target")).expect("target");
    fs::write(ws_dir.join("target/debug.out"), "build").expect("build");
    fs::create_dir_all(ws_dir.join(".claude")).expect("claude");
    fs::write(ws_dir.join(".claude/scheduled_tasks.lock"), "pid=99\n").expect("lock");

    // 1. Checkpoint asserts 100% tree OID & commit OID equivalence + 0 TrackedSetLeak
    let (c1_res, out1) = oracle
        .checkpoint_with_outcome(&ws, "feat: executable + symlink + edit")
        .expect("checkpoint 1");
    let Commit::Created(c1) = c1_res else {
        panic!("expected Created");
    };
    let expected_tree_oid = GitOid::parse_hex(&git_in(
        &checkout,
        &["rev-parse", &format!("{}^{{tree}}", c1.as_str())],
    ))
    .expect("tree oid");
    assert_eq!(
        out1,
        ShadowCheckpointOutcome::Agree {
            tree_oid: Some(expected_tree_oid),
            primary_commit: Some(c1.clone()),
            shadow_commit: Some(c1.clone()),
        }
    );

    // No-op checkpoint stays Agree cleanly
    let (c_noop, out_noop) = oracle.checkpoint_with_outcome(&ws, "noop").expect("noop");
    assert_eq!(c_noop, Commit::Unchanged);
    assert!(matches!(
        out_noop,
        ShadowCheckpointOutcome::Agree {
            primary_commit: None,
            shadow_commit: None,
            ..
        }
    ));

    // Verify PG rows reflect exact commit OID & omit ignored files
    let pg_ref = ledger
        .pg
        .list_refs(Some("proj-shadow"))
        .expect("list")
        .into_iter()
        .find(|r| r.name == "agent_alpha")
        .expect("ref");
    let pg_snap = ledger
        .pg
        .get_snapshot(&pg_ref.snapshot)
        .expect("get")
        .expect("snap");
    assert_eq!(pg_snap.git_oid, c1.as_str());
    let flat = cas
        .flatten_tree(&Digest::parse(&pg_snap.tree).expect("digest"))
        .expect("flatten");
    assert!(!flat.contains_key(b".env".as_slice()));
    assert!(!flat.contains_key(b"sub/cache.local".as_slice()));
    assert!(!flat.contains_key(b"target/debug.out".as_slice()));
    assert!(!flat.contains_key(b".claude/scheduled_tasks.lock".as_slice()));
    assert_eq!(flat[b"tool.sh".as_slice()].kind, TreeEntryKind::Executable);
    assert_eq!(flat[b"README.link".as_slice()].kind, TreeEntryKind::Symlink);

    // 2. Advance main cleanly (`sub/clean.txt`), sync to PG via accepted task, run merge_base
    fs::write(checkout.join("sub/clean.txt"), "main clean addition\n").expect("write clean");
    git_in(&checkout, &["add", "-A"]);
    git_in(&checkout, &["commit", "-qm", "main clean update"]);
    let clean_main_oid =
        GitOid::parse_hex(&git_in(&checkout, &["rev-parse", "main"])).expect("clean oid");
    let task_clean = setup_accepted_task(
        &ledger.pg,
        "/tmp/proj-shadow-clean.git",
        &init_main_oid,
        &clean_main_oid,
    );
    let synced_clean = oracle
        .sync_main_from_git(Some(task_clean))
        .expect("sync clean main")
        .expect("present");
    assert_eq!(synced_clean.git_oid, clean_main_oid);

    let hold_guard = oracle.hold(&ws).expect("hold");
    drop(hold_guard);

    let BaseMerge::Merged {
        commit: merged_clean_oid,
        files,
    } = oracle.merge_base(&ws).expect("clean merge_base")
    else {
        panic!("expected clean Merged");
    };
    assert_eq!(files, vec!["sub/clean.txt".to_string()]);
    assert_eq!(
        oracle
            .shadow()
            .branch_head(&b1)
            .expect("shadow head")
            .as_ref()
            .map(CommitId::as_str),
        Some(merged_clean_oid.as_str())
    );
    let m1 = oracle.metrics();
    assert_eq!(m1.merge_summary.agree_clean, 1);
    assert_eq!(m1.merge_summary.blockers, 0);

    // Edit post-merge and verify next checkpoint agrees
    fs::write(ws_dir.join("sub/clean.txt"), "combined\n").expect("edit after merge");
    let (c2_res, out2) = oracle
        .checkpoint_with_outcome(&ws, "post-merge edit")
        .expect("post-merge checkpoint");
    let Commit::Created(c2) = c2_res else {
        panic!("expected Created");
    };
    assert!(matches!(
        out2,
        ShadowCheckpointOutcome::Agree {
            primary_commit: Some(ref p),
            shadow_commit: Some(ref s),
            ..
        } if p == &c2 && s == &c2
    ));

    // 3. Conflicting main advance -> merge_base conflicts with diff3 markers -> resolution checkpoint
    fs::write(
        checkout.join("README.md"),
        "alpha\nmain conflict line\ngamma\n",
    )
    .expect("conflict");
    git_in(&checkout, &["add", "-A"]);
    git_in(&checkout, &["commit", "-qm", "main conflicting edit"]);
    let conf_main_oid =
        GitOid::parse_hex(&git_in(&checkout, &["rev-parse", "main"])).expect("conf oid");
    let task_conf = setup_accepted_task(
        &ledger.pg,
        "/tmp/proj-shadow-conf.git",
        &clean_main_oid,
        &conf_main_oid,
    );
    oracle
        .sync_main_from_git(Some(task_conf))
        .expect("sync conflict main");

    let BaseMerge::Conflicted {
        files: conf_files,
        marked: true,
    } = oracle.merge_base(&ws).expect("conflicting merge_base")
    else {
        panic!("expected Conflicted marked: true");
    };
    assert_eq!(conf_files, vec!["README.md".to_string()]);
    assert!(oracle.shadow().has_pending_merge("agent_alpha"));
    let m2 = oracle.metrics();
    assert_eq!(m2.merge_summary.agree_conflict, 1);
    assert_eq!(m2.merge_summary.blockers, 0);

    // Resolve markers & checkpoint -> exact 2-parent merge commit parity
    fs::write(ws_dir.join("README.md"), "alpha\nresolved both\ngamma\n").expect("resolve");
    let (c3_res, out3) = oracle
        .checkpoint_with_outcome(&ws, "resolve merge")
        .expect("resolve checkpoint");
    let Commit::Created(c3) = c3_res else {
        panic!("expected Created resolved commit");
    };
    assert!(!oracle.shadow().has_pending_merge("agent_alpha"));
    assert!(matches!(
        out3,
        ShadowCheckpointOutcome::Agree {
            primary_commit: Some(ref p),
            shadow_commit: Some(ref s),
            ..
        } if p == &c3 && s == &c3
    ));

    // 4. set_aside & set_aside_when + unmount + discard
    fs::write(ws_dir.join("uncommitted.txt"), "keep before set_aside\n").expect("dirty");
    let sa = oracle
        .set_aside_when(
            &b1,
            Some(&init_main_oid.to_hex()),
            "cancel work",
            &|| Ok(()),
        )
        .expect("set_aside_when");
    let SetAside::Created {
        commit: sa_commit, ..
    } = sa
    else {
        panic!("expected SetAside::Created");
    };
    assert_eq!(
        oracle
            .shadow()
            .branch_head(&b1)
            .expect("head")
            .as_ref()
            .map(CommitId::as_str),
        Some(sa_commit.as_str())
    );

    let m_final = oracle.metrics();
    assert!(
        !m_final.has_divergences(),
        "unexpected divergence: {m_final:?}"
    );
    assert_eq!(m_final.checkpoints_agreed, 4);
    assert_eq!(m_final.tracked_set_leaks, 0);
    assert_eq!(m_final.tree_mismatches, 0);
    assert_eq!(m_final.shadow_errors, 0);
    assert_eq!(m_final.merge_summary.blockers, 0);

    oracle.discard(&ws).expect("discard");
    assert!(!oracle.is_mounted(&ws));

    // Fork agent_beta and exercise unmount too
    let b2 = BranchName::new("agent_beta").expect("b2");
    oracle.ensure_branch(&b2, None).expect("ensure b2");
    let ws2 = oracle.mount(&b2).expect("mount b2");
    oracle.unmount(&ws2).expect("unmount b2");
    assert!(!oracle.is_mounted(&ws2));
}

fn pg_ref_snapshot(ledger: &PgRefLedger, project: &str, branch: &str) -> String {
    ledger
        .pg
        .list_refs(Some(project))
        .expect("list")
        .into_iter()
        .find(|r| r.name == branch)
        .expect("ref")
        .snapshot
}

#[test]
fn shadow_d6_workspace_pg_negative_tracked_set_leak_fails_closed() {
    let scratch = Scratch::new("pg_shadow_leak");
    let checkout = scratch.join("repo");
    fs::create_dir_all(checkout.join("sub")).expect("repo");
    git_in(&checkout, &["init", "-q", "--initial-branch=main"]);
    fs::write(checkout.join(".gitignore"), ".env\n").expect("gi");
    fs::write(checkout.join("sub/.gitignore"), "*.local\n").expect("sub gi");
    fs::write(checkout.join("README.md"), "base\n").expect("readme");
    git_in(&checkout, &["add", "-A"]);
    git_in(&checkout, &["commit", "-qm", "base"]);

    let bare_repo = checkout.join(".git");
    let primary = GitWorkspace::new(&bare_repo, scratch.join("git-work"));
    let cas = Arc::new(CasMerkleStore::open(scratch.join("cas")).expect("cas"));
    let ledger = Arc::new(PgRefLedger::connect_isolated("shadow_leak"));

    // Positive strict run: zero leaks with .env & sub/secret.local present
    let strict_oracle = ShadowD6Workspace::new(
        primary.clone(),
        D6Workspace::new(
            "proj-leak",
            scratch.join("d6-work"),
            cas.clone(),
            ledger.clone(),
        ),
    );
    let b = BranchName::new("agent_sec").expect("branch");
    strict_oracle.ensure_branch(&b, None).expect("ensure");
    let ws = strict_oracle.mount(&b).expect("mount");
    let ws_dir = PathBuf::from(ws.path());

    fs::write(ws_dir.join(".env"), "DB_SECRET=topsecret\n").expect("write env");
    fs::write(ws_dir.join("sub/secret.local"), "LOCAL_KEY=123\n").expect("write local");
    fs::write(ws_dir.join("work.txt"), "clean code\n").expect("write clean");

    let (_, ok_outcome) = strict_oracle
        .checkpoint_with_outcome(&ws, "strict clean")
        .expect("strict checkpoint");
    assert!(matches!(ok_outcome, ShadowCheckpointOutcome::Agree { .. }));
    assert_eq!(strict_oracle.metrics().tracked_set_leaks, 0);

    let head_before = strict_oracle
        .shadow()
        .branch_snapshot("agent_sec")
        .expect("get")
        .expect("head")
        .id;

    // Negative Case 1: policy bypass with a normal edit (`Commit::Created`) must fail-closed
    // on `.env` and `sub/secret.local`, reporting `TrackedSetLeak` (not `TreeOidMismatch`)
    // and leaving PG `spawn_refs` untouched.
    let leaky_oracle = ShadowD6Workspace::new(
        primary.clone(),
        D6Workspace::new(
            "proj-leak",
            scratch.join("d6-work"),
            cas.clone(),
            ledger.clone(),
        ),
    )
    .with_tracked_set_mode(ShadowTrackedSetMode::SimulateUnfilteredScan)
    .with_strict_fail_closed(true);

    fs::write(ws_dir.join("work.txt"), "clean code v2\n").expect("edit work");
    let err1 = leaky_oracle
        .checkpoint_with_outcome(&ws, "should block leak")
        .expect_err("must refuse TrackedSetLeak");
    assert!(matches!(err1, WorkspaceError::Refused { ref code, .. } if code == "TrackedSetLeak"));
    let m1 = leaky_oracle.metrics();
    assert_eq!(m1.tracked_set_leaks, 1);
    assert_eq!(m1.tree_mismatches, 0);
    assert!(matches!(
        m1.last_checkpoint,
        Some(ShadowCheckpointOutcome::TrackedSetLeak { ref leaked_paths, .. })
            if leaked_paths.contains(&".env".to_string())
                && leaked_paths.contains(&"sub/secret.local".to_string())
    ));
    assert_eq!(
        pg_ref_snapshot(&ledger, "proj-leak", "agent_sec"),
        head_before.to_string()
    );

    // Negative Case 2: primary returns Commit::Unchanged (only an ignored file added),
    // yet unfiltered shadow scan STILL catches `TrackedSetLeak` fail-closed.
    let nonblocking_leaky_oracle = ShadowD6Workspace::new(
        primary,
        D6Workspace::new("proj-leak", scratch.join("d6-work"), cas, ledger.clone()),
    )
    .with_tracked_set_mode(ShadowTrackedSetMode::SimulateUnfilteredScan)
    .with_strict_fail_closed(false);

    fs::write(ws_dir.join("sub/only_ignored.local"), "ONLY_IGNORED=1\n").expect("write local2");
    let (res2, out2) = nonblocking_leaky_oracle
        .checkpoint_with_outcome(&ws, "ignored-only leak check")
        .expect("non-blocking oracle returns primary Commit::Unchanged");
    assert_eq!(res2, Commit::Unchanged);
    assert!(matches!(
        out2,
        ShadowCheckpointOutcome::TrackedSetLeak { ref leaked_paths, .. }
            if leaked_paths.contains(&".env".to_string())
                && leaked_paths.contains(&"sub/only_ignored.local".to_string())
    ));
    let m2 = nonblocking_leaky_oracle.metrics();
    assert_eq!(m2.tracked_set_leaks, 1);
    assert_eq!(m2.tree_mismatches, 0);

    // Ensure PG spawn_refs did NOT advance to any snapshot containing leaked paths
    assert_eq!(
        pg_ref_snapshot(&ledger, "proj-leak", "agent_sec"),
        head_before.to_string()
    );
}
