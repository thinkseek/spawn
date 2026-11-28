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
use std::time::Duration;

use serde_json::json;
use spawn_cas::{
    CasError, CommitActor, CommitIntent, CommitObject, CommitOrigin, DEFAULT_PERSISTENT_GC_GRACE,
    Digest, GcFenceBinding, GitOid, Snapshot, Tree, TreeEntry, TreeEntryKind, TreeMutation,
    WorkBoundary, WorkChange,
};
use spawn_core::id::{BranchName, CommitId, WorkspaceId};
use spawn_core::port::{
    BaseMerge, Branch, Commit, MERGE_WAITS, MOUNT_CANCELLED, SetAside, UNMOUNT_IN_USE,
    WorkspaceError, WorkspaceStore,
};
use spawn_host::d6_workspace::{
    HostMemorySample, L1Admission, L1FallbackReason, L1FlushReason, L1TmpfsPolicy,
};
use spawn_host::workspace::{D6ShadowOptions, D6StoreOptions, DynRefLedger};
use spawn_host::{
    CasMerkleStore, D6RefKind, D6StoreError, D6Workspace, GitWorkspace, LedgerRef, LedgerSnapshot,
    LocalWorkspaceStore, RefLedger, SET_ASIDE_WAITS, ShadowCheckpointOutcome, ShadowD6Workspace,
    ShadowTrackedSetMode, bind_store_gc_fence, check_gc_fence, format_commit_body,
    reconcile_refs_journal, run_persistent_gc_for_ledger, test_git_command,
};
use spawn_postgres::testing::TestSchema;
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
    _schema: Arc<TestSchema>,
}

impl PgRefLedger {
    fn connect_in_schema(schema: Arc<TestSchema>) -> Self {
        let pg = schema.store();
        Self {
            pg,
            _schema: schema,
        }
    }

    fn connect_isolated(case: &str) -> Self {
        Self::connect_in_schema(Arc::new(TestSchema::new(case)))
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

    fn retire_project_refs(&self, project: &str, reason: &str) -> Result<usize, D6StoreError> {
        let count = self
            .pg
            .retire_project_refs(project, reason)
            .map_err(|e| D6StoreError::Ledger(e.to_string()))?;
        usize::try_from(count)
            .map_err(|_| D6StoreError::Ledger(format!("negative retired ref count: {count}")))
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

    let store = D6Workspace::new("proj-d6", &d6_root, cas.clone(), ledger.clone(), 1024)
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
    let primary = GitWorkspace::new(
        &bare_repo,
        scratch.join("git-work"),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    );
    let cas = Arc::new(CasMerkleStore::open(scratch.join("cas")).expect("cas"));
    let ledger = Arc::new(PgRefLedger::connect_isolated("shadow_parity"));
    let d6 = D6Workspace::new(
        "proj-shadow",
        scratch.join("d6-work"),
        cas.clone(),
        ledger.clone(),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
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

    let cap = spawn_host::DEFAULT_MAX_FILE_BYTES;
    let bare_repo = checkout.join(".git");
    let primary = GitWorkspace::new(&bare_repo, scratch.join("git-work"), cap);
    let cas = Arc::new(CasMerkleStore::open(scratch.join("cas")).expect("cas"));
    let ledger = Arc::new(PgRefLedger::connect_isolated("shadow_leak"));
    let leak_d6 = || {
        D6Workspace::new(
            "proj-leak",
            scratch.join("d6-work"),
            cas.clone(),
            ledger.clone(),
            cap,
        )
    };

    // Positive strict run: zero leaks with .env & sub/secret.local present
    let strict_oracle = ShadowD6Workspace::new(primary.clone(), leak_d6());
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
    let leaky_oracle = ShadowD6Workspace::new(primary.clone(), leak_d6())
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
    let nonblocking_leaky_oracle = ShadowD6Workspace::new(primary, leak_d6())
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

fn seed_l1_main_with_blob(
    store: &D6Workspace<PgRefLedger>,
    ledger: &PgRefLedger,
    cas: &CasMerkleStore,
    project: &str,
    source: &str,
    blob_size: usize,
) {
    let base_branch = BranchName::new("seed_bootstrap").expect("branch");
    store.ensure_branch(&base_branch, None).expect("bootstrap");
    let main0 = store.main_snapshot().expect("main0").expect("present");
    let (blob_d, blob_len) = cas
        .put_bytes_blob(&vec![b'k'; blob_size])
        .expect("put blob");
    let tree = cas
        .put_tree_cached(
            &Tree::from_entries(vec![TreeEntry::file(
                b"base.bin".to_vec(),
                &blob_d,
                blob_len,
            )])
            .expect("tree"),
        )
        .expect("put tree");
    let ts_ms = 1_700_000_005_000_i64;
    let body = format_commit_body(
        "spawn <s@l> 1700000005 +0000",
        "spawn <s@l> 1700000005 +0000",
        "seed l1 main\n",
    );
    let snap = Snapshot {
        tree: tree.clone(),
        parents: vec![main0.id.clone()],
        commit_body: body,
    };
    let snap_id = cas.cas().put_merkle_snapshot(&snap).expect("put snap");
    let git_oid = cas
        .project_snapshot_oid(&snap, &[main0.git_oid])
        .expect("project");
    ledger
        .record_snapshot(&snap_id, &git_oid, &tree, &snap.parents, project, ts_ms)
        .expect("record snap");
    let task_id = setup_accepted_task(&ledger.pg, source, &main0.git_oid, &git_oid);
    store
        .land_main_snapshot(Some(&main0.id), &snap_id, Some(task_id), "seed l1 main")
        .expect("land main");
}

fn verify_l1_host_memory_reserve_fallbacks(
    cas: &Arc<CasMerkleStore>,
    ledger: &Arc<PgRefLedger>,
    disk_root: &Path,
    tmpfs_root: &Path,
) {
    let gib = 1024_u64 * 1024 * 1024;
    // 1a) 128 GiB host with only 50 GiB available (< 64 GiB absolute floor) -> fallback to disk
    let low_abs_store = D6Workspace::new(
        "proj-l1",
        disk_root,
        cas.clone(),
        ledger.clone(),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    )
    .with_l1_policy(L1TmpfsPolicy::new(tmpfs_root, 16))
    .with_l1_memory_probe(Arc::new(move |_| {
        Some(HostMemorySample {
            total_bytes: 128 * gib,
            available_bytes: 50 * gib,
            tmpfs_available_bytes: Some(gib),
        })
    }));
    let b_low_abs = BranchName::new("agent_low64").expect("branch");
    low_abs_store
        .ensure_branch(&b_low_abs, None)
        .expect("ensure");
    let ws_low_abs = low_abs_store.mount(&b_low_abs).expect("mount fallback");
    assert!(matches!(
        low_abs_store.l1_admission("agent_low64"),
        Some(L1Admission::FallbackDisk {
            reason: L1FallbackReason::HostMemoryBelowReserve { .. },
            ..
        })
    ));
    assert!(Path::new(ws_low_abs.path()).starts_with(low_abs_store.root()));
    low_abs_store.discard(&ws_low_abs).expect("discard");

    // 1b) 1 TiB host with 100 GiB available (> 64 GiB, but < 15% of 1 TiB = 153.6 GiB) -> fallback
    let low_pct_store = D6Workspace::new(
        "proj-l1",
        disk_root,
        cas.clone(),
        ledger.clone(),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    )
    .with_l1_policy(L1TmpfsPolicy::new(tmpfs_root, 16))
    .with_l1_memory_probe(Arc::new(move |_| {
        Some(HostMemorySample {
            total_bytes: 1024 * gib,
            available_bytes: 100 * gib,
            tmpfs_available_bytes: Some(gib),
        })
    }));
    let b_low_pct = BranchName::new("agent_low15pct").expect("branch");
    low_pct_store
        .ensure_branch(&b_low_pct, None)
        .expect("ensure");
    let ws_low_pct = low_pct_store.mount(&b_low_pct).expect("mount fallback");
    assert!(matches!(
        low_pct_store.l1_admission("agent_low15pct"),
        Some(L1Admission::FallbackDisk {
            reason: L1FallbackReason::HostMemoryBelowReserve { .. },
            ..
        })
    ));
    assert_eq!(low_pct_store.l1_total_reserved_bytes(), 0);
    low_pct_store.discard(&ws_low_pct).expect("discard");
}

fn verify_l1_interval_and_pressure_flush(
    store: &D6Workspace<PgRefLedger>,
    cas: &CasMerkleStore,
    ws1: &WorkspaceId,
    ws1_dir: &Path,
    clock_ms: &AtomicI64,
) {
    // Dirty 20 KiB (< 80%), advance 65s -> triggers 60s interval flush into PG while staying in L1
    fs::write(ws1_dir.join("dirty1.bin"), vec![b'd'; 20 * 1024]).expect("write dirty1");
    assert_eq!(store.l1_needs_flush(ws1).expect("under 80%"), None);
    clock_ms.fetch_add(65_000, Ordering::Relaxed);
    assert!(matches!(
        store.l1_needs_flush(ws1).expect("interval elapsed"),
        Some(L1FlushReason::IntervalElapsed { .. })
    ));
    let interval_out = store
        .flush_l1_if_needed(ws1, "l1 interval flush")
        .expect("flush interval")
        .expect("triggered");
    assert!(matches!(interval_out.commit, Commit::Created(_)));
    assert_eq!(interval_out.demoted_to_disk, None);
    assert_eq!(interval_out.reclaimed_bytes, 0);
    assert_eq!(
        store
            .l1_accounting("agent_l1_one")
            .expect("acc")
            .flush_count,
        1
    );

    // Write another 60 KiB -> >= 560 KiB (80% threshold) -> pressure flush + demote to disk + reclaim L1
    fs::write(ws1_dir.join("pressure.bin"), vec![b'p'; 60 * 1024]).expect("write pressure");
    assert!(matches!(
        store.l1_needs_flush(ws1).expect("pressure threshold"),
        Some(L1FlushReason::UsageThreshold {
            threshold_pct: 80,
            ..
        })
    ));
    let pressure_out = store
        .flush_l1_if_needed(ws1, "l1 pressure reclaim")
        .expect("flush pressure")
        .expect("triggered");
    assert!(matches!(pressure_out.commit, Commit::Created(_)));
    assert!(pressure_out.reclaimed_bytes >= 600 * 1024);
    let disk_ws = pressure_out.demoted_to_disk.expect("demoted to disk");
    assert!(!store.is_mounted(ws1));
    assert!(store.is_mounted(&disk_ws));
    assert_eq!(store.l1_total_reserved_bytes(), 0);
    assert!(Path::new(disk_ws.path()).join("dirty1.bin").is_file());
    assert!(Path::new(disk_ws.path()).join("pressure.bin").is_file());
    let pg_head = store
        .branch_snapshot("agent_l1_one")
        .expect("head")
        .expect("some");
    let pg_flat = cas.flatten_tree(&pg_head.tree).expect("flatten");
    assert!(pg_flat.contains_key(b"pressure.bin".as_slice()));

    store.discard(&disk_ws).expect("discard demoted");
    assert_eq!(store.l1_total_reserved_bytes(), 0);
}

#[test]
fn d6_workspace_pg_l1_tmpfs_admission_fallback_pressure_flush_and_reclaim() {
    let scratch = Scratch::new("pg_l1_tmpfs");
    let cas = Arc::new(CasMerkleStore::open(scratch.join("cas")).expect("cas"));
    let ledger = Arc::new(PgRefLedger::connect_isolated("l1_tmpfs"));
    let disk_root = scratch.join("disk-work");
    let tmpfs_root = scratch.join("tmpfs-l1");
    let gib = 1024_u64 * 1024 * 1024;

    let clock_ms = Arc::new(AtomicI64::new(1_700_000_010_000));
    let clk = clock_ms.clone();
    let policy = L1TmpfsPolicy::new(&tmpfs_root, 1).with_per_workspace_quota_bytes(700 * 1024);
    let store = D6Workspace::new(
        "proj-l1",
        &disk_root,
        cas.clone(),
        ledger.clone(),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    )
    .with_clock_ms(Arc::new(move || clk.fetch_add(1_000, Ordering::Relaxed)))
    .with_l1_policy(policy)
    .with_l1_memory_probe(Arc::new(move |_| {
        Some(HostMemorySample {
            total_bytes: 512 * gib,
            available_bytes: 400 * gib,
            tmpfs_available_bytes: Some(gib),
        })
    }));

    // Seed 520 KiB baseline file onto PG main (within 700 KiB quota, < 80% = 560 KiB)
    seed_l1_main_with_blob(
        &store,
        &ledger,
        &cas,
        "proj-l1",
        "/tmp/proj-l1-seed.git",
        520 * 1024,
    );

    verify_l1_host_memory_reserve_fallbacks(&cas, &ledger, &disk_root, &tmpfs_root);

    // Admit branch 1 onto L1 tmpfs
    let b1 = BranchName::new("agent_l1_one").expect("b1");
    store.ensure_branch(&b1, None).expect("ensure b1");
    let ws1 = store.mount(&b1).expect("mount b1 on L1");
    assert!(matches!(
        store.l1_admission("agent_l1_one"),
        Some(L1Admission::AdmittedTmpfs { .. })
    ));
    let ws1_dir = PathBuf::from(ws1.path());
    assert!(ws1_dir.starts_with(store.l1_policy().expect("policy").tmpfs_root.clone()));
    assert!(store.l1_total_reserved_bytes() >= 520 * 1024);
    assert_eq!(store.l1_needs_flush(&ws1).expect("probe"), None);

    // Branch 2 would push total past 1 MiB budget (520 + 520 > 1024 KiB) -> falls back to disk
    let b2 = BranchName::new("agent_l1_two").expect("b2");
    store.ensure_branch(&b2, None).expect("ensure b2");
    let ws2 = store.mount(&b2).expect("mount b2 fallback");
    assert!(matches!(
        store.l1_admission("agent_l1_two"),
        Some(L1Admission::FallbackDisk {
            reason: L1FallbackReason::BudgetExceeded { .. },
            ..
        })
    ));
    assert!(Path::new(ws2.path()).starts_with(store.root()));
    store.discard(&ws2).expect("discard b2");

    verify_l1_interval_and_pressure_flush(&store, &cas, &ws1, &ws1_dir, &clock_ms);

    // Subsequent mount after reclaim succeeds on L1 and discard frees quota cleanly
    let b3 = BranchName::new("agent_l1_three").expect("b3");
    store.ensure_branch(&b3, None).expect("ensure b3");
    let ws3 = store.mount(&b3).expect("mount b3 on L1");
    assert!(matches!(
        store.l1_admission("agent_l1_three"),
        Some(L1Admission::AdmittedTmpfs { .. })
    ));
    store.discard(&ws3).expect("discard b3");
    assert_eq!(store.l1_total_reserved_bytes(), 0);
}

fn advance_concurrent_main_steps(
    coord_store: &D6Workspace<PgRefLedger>,
    coord_ledger: &PgRefLedger,
    cas: &CasMerkleStore,
) {
    for step in 0..3_usize {
        let main_now = coord_store.main_snapshot().expect("main").expect("some");
        let (bd, blen) = cas
            .put_bytes_blob(format!("main-step-{step}\n").as_bytes())
            .expect("blob");
        let next_tree = cas
            .apply_mutations(
                &main_now.tree,
                &[TreeMutation::Upsert {
                    path: vec![
                        b"main_updates".to_vec(),
                        format!("m{step}.txt").into_bytes(),
                    ],
                    kind: TreeEntryKind::File,
                    id: bd.as_bytes(),
                    size: blen,
                }],
            )
            .expect("mutate");
        let ts_ms = 1_700_000_100_000_i64 + i64::try_from(step).unwrap() * 1_000;
        let body = format_commit_body(
            "spawn <s@l> 1700000100 +0000",
            "spawn <s@l> 1700000100 +0000",
            &format!("main advance {step}\n"),
        );
        let snap = Snapshot {
            tree: next_tree.clone(),
            parents: vec![main_now.id.clone()],
            commit_body: body,
        };
        let sid = cas.cas().put_merkle_snapshot(&snap).expect("snap");
        let goid = cas
            .project_snapshot_oid(&snap, &[main_now.git_oid])
            .expect("oid");
        coord_ledger
            .record_snapshot(&sid, &goid, &next_tree, &snap.parents, "proj-conc", ts_ms)
            .expect("record");
        let task_id = setup_accepted_task(
            &coord_ledger.pg,
            &format!("/tmp/proj-conc-{step}.git"),
            &main_now.git_oid,
            &goid,
        );
        coord_store
            .land_main_snapshot(Some(&main_now.id), &sid, Some(task_id), "concurrent main")
            .expect("land main");
        std::thread::sleep(Duration::from_millis(15));
    }
}

fn verify_concurrent_pg_branches(
    coord_store: &D6Workspace<PgRefLedger>,
    cas: &CasMerkleStore,
    disk_root: &Path,
    mounted_branches: &[BranchName],
    unmounted_branches: &[BranchName],
) {
    for b in mounted_branches {
        let dir = disk_root.join(b.as_str());
        let ws = spawn_core::id::WorkspaceId::new(format!("{b}:{}", dir.display())).expect("id");
        assert!(!matches!(
            coord_store.merge_base(&ws).expect("final merge_base"),
            BaseMerge::Conflicted { .. }
        ));
    }
    for b in unmounted_branches {
        assert!(!matches!(
            coord_store
                .merge_main_clean(b)
                .expect("final merge_main_clean"),
            BaseMerge::Conflicted { .. }
        ));
        assert!(!disk_root.join(b.as_str()).exists());
    }
    for (idx, b) in mounted_branches.iter().enumerate() {
        let head = coord_store
            .branch_snapshot(b.as_str())
            .expect("get")
            .expect("head");
        let flat = cas.flatten_tree(&head.tree).expect("flatten");
        for step in 0..3_usize {
            assert!(flat.contains_key(format!("main_updates/m{step}.txt").as_bytes()));
        }
        for round in 0..4_usize {
            assert!(flat.contains_key(format!("workers/w{idx}/r{round}.txt").as_bytes()));
        }
    }
    for b in unmounted_branches {
        let head = coord_store
            .branch_snapshot(b.as_str())
            .expect("get")
            .expect("head");
        let flat = cas.flatten_tree(&head.tree).expect("flatten");
        for step in 0..3_usize {
            assert!(flat.contains_key(format!("main_updates/m{step}.txt").as_bytes()));
        }
    }
    let journal = cas.cas().read_ref_journal().expect("read refs.journal");
    assert!(
        journal.len() >= 20,
        "expected >= 20 ref journal transitions, got {}",
        journal.len()
    );
}

#[test]
fn d6_workspace_pg_concurrent_8_branches_checkpoint_and_merge_main_clean() {
    use std::sync::Barrier;

    let scratch = Scratch::new("pg_concurrent_8");
    let cas = Arc::new(CasMerkleStore::open(scratch.join("cas")).expect("cas"));
    let disk_root = scratch.join("work");
    let schema = Arc::new(TestSchema::new("concurrent8"));
    let cap = spawn_host::DEFAULT_MAX_FILE_BYTES;
    let open_conc = || {
        let l = Arc::new(PgRefLedger::connect_in_schema(Arc::clone(&schema)));
        let ws = D6Workspace::new("proj-conc", &disk_root, cas.clone(), l.clone(), cap);
        (l, ws)
    };
    let (coord_ledger, coord_store) = open_conc();

    seed_l1_main_with_blob(
        &coord_store,
        &coord_ledger,
        &cas,
        "proj-conc",
        "/tmp/proj-conc-init.git",
        1024,
    );

    let mounted_branches: Vec<BranchName> = (0..8)
        .map(|i| BranchName::new(format!("agent_c{i}")).expect("branch"))
        .collect();
    let unmounted_branches: Vec<BranchName> = (0..4)
        .map(|i| BranchName::new(format!("unmounted_c{i}")).expect("branch"))
        .collect();
    for b in mounted_branches.iter().chain(&unmounted_branches) {
        coord_store.ensure_branch(b, None).expect("ensure branch");
    }

    let worker_stores: Vec<_> = (0..8).map(|_| open_conc().1).collect();
    let merger_stores: Vec<_> = (0..4).map(|_| open_conc().1).collect();

    let barrier = Arc::new(Barrier::new(13));
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        let b_main = barrier.clone();
        let cs = &coord_store;
        let cl = &coord_ledger;
        let cas_ref = &cas;
        handles.push(scope.spawn(move || {
            b_main.wait();
            advance_concurrent_main_steps(cs, cl, cas_ref);
        }));

        for (idx, (store, branch)) in worker_stores.into_iter().zip(&mounted_branches).enumerate() {
            let b_w = barrier.clone();
            handles.push(scope.spawn(move || {
                b_w.wait();
                let ws = store.mount(branch).expect("mount");
                let dir = PathBuf::from(ws.path());
                fs::create_dir_all(dir.join(format!("workers/w{idx}"))).expect("mkdir");
                for round in 0..4_usize {
                    fs::write(
                        dir.join(format!("workers/w{idx}/r{round}.txt")),
                        format!("worker={idx} round={round}\n"),
                    )
                    .expect("write");
                    assert!(matches!(
                        store
                            .checkpoint(&ws, &format!("w{idx} r{round}"))
                            .expect("ckpt"),
                        Commit::Created(_)
                    ));
                    assert_eq!(
                        store.checkpoint(&ws, "noop").expect("noop"),
                        Commit::Unchanged
                    );
                    let m = store.merge_base(&ws).expect("merge_base");
                    assert!(!matches!(m, BaseMerge::Conflicted { .. }));
                }
            }));
        }

        for (store, branch) in merger_stores.into_iter().zip(&unmounted_branches) {
            let b_u = barrier.clone();
            handles.push(scope.spawn(move || {
                b_u.wait();
                for _ in 0..6 {
                    let res = store.merge_main_clean(branch).expect("merge_main_clean");
                    assert!(!matches!(res, BaseMerge::Conflicted { .. }));
                    std::thread::sleep(Duration::from_millis(8));
                }
            }));
        }
        for h in handles {
            h.join().expect("thread succeeded cleanly");
        }
    });

    verify_concurrent_pg_branches(
        &coord_store,
        &cas,
        &disk_root,
        &mounted_branches,
        &unmounted_branches,
    );
}

#[test]
fn d6_workspace_pg_same_second_identical_commits_are_idempotent() {
    let scratch = Scratch::new("pg_same_second_idempotent");
    let cas = Arc::new(CasMerkleStore::open(scratch.join("cas")).expect("cas"));
    let ledger = Arc::new(PgRefLedger::connect_isolated("same_sec_idempotent"));
    let disk_root = scratch.join("disk-work");

    let store_a = D6Workspace::new(
        "proj-sec",
        &disk_root,
        cas.clone(),
        ledger.clone(),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    )
    .with_clock_ms(Arc::new(|| 1_700_000_050_111));
    let store_b = D6Workspace::new(
        "proj-sec",
        &disk_root,
        cas,
        ledger,
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    )
    .with_clock_ms(Arc::new(|| 1_700_000_050_987));

    let b1 = BranchName::new("agent_sec_a").expect("b1");
    let b2 = BranchName::new("agent_sec_b").expect("b2");
    store_a.ensure_branch(&b1, None).expect("ensure b1");
    store_b.ensure_branch(&b2, None).expect("ensure b2");

    let ws1 = store_a.mount(&b1).expect("mount b1");
    let ws2 = store_b.mount(&b2).expect("mount b2");
    fs::write(Path::new(ws1.path()).join("same.txt"), b"identical\n").expect("write w1");
    fs::write(Path::new(ws2.path()).join("same.txt"), b"identical\n").expect("write w2");

    let c1 = store_a
        .checkpoint(&ws1, "identical commit")
        .expect("checkpoint w1");
    let c2 = store_b
        .checkpoint(&ws2, "identical commit")
        .expect("checkpoint w2 same second must not hit 23505");
    assert_eq!(c1, c2);
    assert!(matches!(c1, Commit::Created(_)));

    let s1 = store_a
        .branch_snapshot("agent_sec_a")
        .expect("s1")
        .expect("some s1");
    let s2 = store_b
        .branch_snapshot("agent_sec_b")
        .expect("s2")
        .expect("some s2");
    assert_eq!(s1.id, s2.id);
    assert_eq!(s1.created_at_ms, 1_700_000_050_000);
    assert_eq!(s2.created_at_ms, 1_700_000_050_000);
}

fn setup_local_bare_repo(seed: &Path, bare: &Path) {
    fs::create_dir_all(seed).expect("mkdir seed");
    git_in(seed, &["init", "-q", "--initial-branch=main"]);
    fs::write(seed.join(".gitignore"), "*.secret\n").expect("gi");
    fs::write(seed.join("README.md"), "base line\n").expect("readme");
    git_in(seed, &["add", "-A"]);
    git_in(seed, &["commit", "-qm", "base"]);
    git_in(
        seed,
        &[
            "clone",
            "-q",
            "--bare",
            &seed.to_string_lossy(),
            &bare.to_string_lossy(),
        ],
    );
}

fn verify_local_store_d6_shadow_strict_leak(cas: Arc<CasMerkleStore>) {
    let scratch = Scratch::new("local_d6_shadow_strict");
    let seed = scratch.join("seed");
    let bare = scratch.join("bare.git");
    setup_local_bare_repo(&seed, &bare);
    let ledger: DynRefLedger = Arc::new(PgRefLedger::connect_isolated("local_d6_shadow_strict"));
    let opts = D6ShadowOptions::new(
        scratch.join("shadow-work"),
        cas,
        ledger,
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    )
    .with_tracked_set_mode(ShadowTrackedSetMode::SimulateUnfilteredScan)
    .with_strict_fail_closed(true);
    let store = LocalWorkspaceStore::d6_shadow(&bare, scratch.join("git-work"), opts);
    let branch = BranchName::new("agent_strict_leak").expect("branch");
    store.ensure_branch(&branch, None).expect("ensure_branch");
    let ws = store.mount(&branch).expect("mount");
    let dir = PathBuf::from(ws.path());
    fs::write(dir.join("tracked.txt"), "ok\n").expect("write tracked");
    fs::write(dir.join("leak.secret"), "TOKEN=topsecret\n").expect("write secret");

    let err = store
        .checkpoint(&ws, "must fail closed on leak")
        .expect_err("strict_fail_closed must refuse TrackedSetLeak");
    assert!(
        matches!(&err, WorkspaceError::Refused { code, .. } if code == "TrackedSetLeak"),
        "unexpected error: {err:?}"
    );
    let metrics = store.shadow_metrics().expect("shadow metrics");
    assert!(metrics.tracked_set_leaks >= 1, "{metrics:?}");
    assert_eq!(metrics.shadow_errors, 0, "{metrics:?}");
    store.discard(&ws).expect("discard");
}

#[test]
fn local_workspace_store_d6_shadow_parity_and_strict_fail_closed_leak() {
    let scratch = Scratch::new("local_d6_shadow");
    let seed = scratch.join("seed");
    let bare = scratch.join("bare.git");
    setup_local_bare_repo(&seed, &bare);
    let cas = Arc::new(CasMerkleStore::open(scratch.join("cas")).expect("cas"));
    let ledger: DynRefLedger = Arc::new(PgRefLedger::connect_isolated("local_d6_shadow"));

    let mut opts = D6ShadowOptions::new(
        scratch.join("shadow-work"),
        cas.clone(),
        ledger,
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    )
    .with_strict_fail_closed(true);
    opts = opts.with_l1_policy(L1TmpfsPolicy::new(scratch.join("l1-tmpfs"), 16));
    let store = LocalWorkspaceStore::d6_shadow(&bare, scratch.join("git-work"), opts);
    assert_eq!(store.backend_name(), "d6-shadow");

    let branch = BranchName::new("agent_local_shadow").expect("branch");
    store.ensure_branch(&branch, None).expect("ensure_branch");
    let ws = store.mount(&branch).expect("mount");
    assert!(store.is_mounted(&ws));
    let held = store.hold(&ws).expect("hold");
    drop(held);

    let dir = PathBuf::from(ws.path());
    fs::write(dir.join("app.txt"), "hello d6 shadow\n").expect("write app");
    fs::write(dir.join("ignored.secret"), "SECRET=1\n").expect("write ignored");
    assert!(matches!(
        store.checkpoint(&ws, "work commit").expect("checkpoint"),
        Commit::Created(_)
    ));

    // Advance main in git and verify clean merge parity
    fs::write(seed.join("upstream.txt"), "from main\n").expect("write upstream");
    git_in(&seed, &["add", "-A"]);
    git_in(&seed, &["commit", "-qm", "upstream"]);
    git_in(&seed, &["push", "-q", &bare.to_string_lossy(), "main"]);

    assert!(matches!(
        store.merge_base(&ws).expect("merge_base"),
        BaseMerge::Merged { .. }
    ));
    let aside = store
        .set_aside_when(&branch, None, "pause work", &|| Ok(()))
        .expect("set_aside_when");
    assert!(matches!(aside, SetAside::Created { .. }));
    assert!(store.notices(&ws).is_empty());
    store.notices_told(&ws, &[]);
    store.unmount(&ws).expect("unmount");
    assert!(!store.is_mounted(&ws));

    let metrics = store.shadow_metrics().expect("shadow metrics");
    assert!(metrics.checkpoints_agreed >= 1, "{metrics:?}");
    assert_eq!(metrics.tree_mismatches, 0, "{metrics:?}");
    assert_eq!(metrics.tracked_set_leaks, 0, "{metrics:?}");
    assert_eq!(metrics.merge_summary.agree_clean, 1, "{metrics:?}");
    assert!(!metrics.has_divergences(), "{metrics:?}");

    verify_local_store_d6_shadow_strict_leak(cas);
}

#[test]
fn local_workspace_store_d6_native_lifecycle_and_l1_tmpfs() {
    let scratch = Scratch::new("local_d6_native");
    let cas = Arc::new(CasMerkleStore::open(scratch.join("cas")).expect("cas"));
    let ledger_pg = Arc::new(PgRefLedger::connect_isolated("local_d6_native"));
    let ledger: DynRefLedger = ledger_pg.clone();
    let tmpfs_root = scratch.join("l1-tmpfs");

    let opts = D6StoreOptions::new(cas.clone(), ledger, spawn_host::DEFAULT_MAX_FILE_BYTES)
        .with_l1_policy(L1TmpfsPolicy::new(&tmpfs_root, 32));
    let store = LocalWorkspaceStore::d6("proj-local-d6", scratch.join("disk-work"), opts);
    assert_eq!(store.backend_name(), "d6");

    let branch = BranchName::new("agent_local_d6").expect("branch");
    store.ensure_branch(&branch, None).expect("ensure_branch");
    let ws = store.mount(&branch).expect("mount");
    assert!(store.is_mounted(&ws));
    assert!(Path::new(ws.path()).starts_with(spawn_host::canonical_root(&tmpfs_root)));
    assert!(store.d6_l1_reserved_bytes().is_some());

    let dir = PathBuf::from(ws.path());
    fs::write(dir.join("main.rs"), "fn main() {}\n").expect("write");
    assert!(matches!(
        store.checkpoint(&ws, "first d6 commit").expect("ckpt"),
        Commit::Created(_)
    ));
    assert_eq!(
        store.checkpoint(&ws, "noop").expect("noop"),
        Commit::Unchanged
    );

    // Advance PG main independently and test merge_base through LocalWorkspaceStore::D6
    let d6 = store.d6_workspace().expect("d6_workspace");
    let main_now = d6.main_snapshot().expect("main").expect("seeded");
    let (bd, blen) = cas.put_bytes_blob(b"upstream change\n").expect("blob");
    let next_tree = cas
        .apply_mutations(
            &main_now.tree,
            &[TreeMutation::Upsert {
                path: vec![b"UPSTREAM.md".to_vec()],
                kind: TreeEntryKind::File,
                id: bd.as_bytes(),
                size: blen,
            }],
        )
        .expect("tree");
    let snap = Snapshot {
        tree: next_tree.clone(),
        parents: vec![main_now.id.clone()],
        commit_body: format_commit_body(
            "spawn <t@localhost> 1700000100 +0000",
            "spawn <t@localhost> 1700000100 +0000",
            "upstream\n",
        ),
    };
    let snap_id = cas.cas().put_merkle_snapshot(&snap).expect("put snap");
    let oid = cas
        .project_snapshot_oid(&snap, &[main_now.git_oid])
        .expect("oid");
    ledger_pg
        .pg
        .record_snapshot(
            &snap_id.to_string(),
            &oid.to_hex(),
            &next_tree.to_string(),
            &[main_now.id.to_string()],
            "proj-local-d6",
            1_700_000_100_000,
        )
        .expect("record");
    let task_id = setup_accepted_task(
        &ledger_pg.pg,
        "/tmp/local-d6-main.git",
        &main_now.git_oid,
        &oid,
    );
    let _ = d6
        .land_main_snapshot(Some(&main_now.id), &snap_id, Some(task_id), "land")
        .expect("land main");

    assert!(matches!(
        store.merge_base(&ws).expect("merge_base"),
        BaseMerge::Merged { .. }
    ));
    assert!(Path::new(ws.path()).join("UPSTREAM.md").is_file());
    assert!(matches!(
        store
            .set_aside_when(&branch, None, "pause d6", &|| Ok(()))
            .expect("set_aside"),
        SetAside::Created { .. }
    ));
    store.unmount(&ws).expect("unmount");
    assert!(!store.is_mounted(&ws));
    assert_eq!(store.d6_l1_reserved_bytes(), Some(0));
    let refs = ledger_pg
        .list_refs(Some("proj-local-d6"))
        .expect("list_refs");
    assert!(refs.iter().any(|r| r.name == "agent_local_d6"));
}

fn copy_cas_dir_excluding_fence_and_journal(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("mkdir dst");
    for entry in fs::read_dir(src).expect("read src") {
        let entry = entry.expect("dir entry");
        let name = entry.file_name();
        if name == ".gc-fence" || name == "refs.journal" || name == ".lock" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        let ft = entry.file_type().expect("file type");
        if ft.is_dir() {
            copy_cas_dir_excluding_fence_and_journal(&from, &to);
        } else if ft.is_file() {
            fs::copy(&from, &to).expect("copy cas entry");
            let mode = fs::metadata(&from).expect("meta").permissions().mode() & 0o777;
            fs::set_permissions(&to, fs::Permissions::from_mode(mode)).expect("chmod copy");
        }
    }
}

fn binding_from_pg(pg: &PostgresStore) -> GcFenceBinding {
    let ident = pg.database_identity().expect("database_identity");
    GcFenceBinding {
        db_system_id: ident.db_system_id,
        db_oid: ident.db_oid,
        schema_name: ident.schema_name,
    }
}

#[test]
#[expect(clippy::too_many_lines)]
fn d6_workspace_pg_host_and_pg_restart_recovery_with_gc_fence_and_journal() {
    let scratch = Scratch::new("host_pg_restart");
    let cas_dir = scratch.join("cas");
    let cas = Arc::new(CasMerkleStore::open(&cas_dir).expect("open cas"));
    let live_pg = Arc::new(PgRefLedger::connect_isolated("host_pg_restart_live"));
    let live_binding = binding_from_pg(&live_pg.pg);
    bind_store_gc_fence(&cas, &live_binding).expect("bind live gc fence");

    let live_dyn: DynRefLedger = live_pg.clone();
    let store = LocalWorkspaceStore::d6(
        "restart-proj",
        scratch.join("disk-work"),
        D6StoreOptions::new(cas.clone(), live_dyn, spawn_host::DEFAULT_MAX_FILE_BYTES),
    );
    let d6 = store.d6_workspace().expect("d6 inner");

    // Seed initial main (seq=1) deterministically so backup restore drill can match seq=1.
    let (base_id, base_sz) = cas.put_bytes_blob(b"base v0\n").expect("base blob");
    let main0_tree = cas
        .put_tree_cached(
            &Tree::from_entries(vec![TreeEntry::file(
                b"README.md".to_vec(),
                &base_id,
                base_sz,
            )])
            .expect("main0 tree"),
        )
        .expect("store main0 tree");
    let main0_snap = Snapshot {
        tree: main0_tree.clone(),
        parents: Vec::new(),
        commit_body: format_commit_body(
            "Spawn <spawn@localhost> 1700000100 +0000",
            "Spawn <spawn@localhost> 1700000100 +0000",
            "Bootstrap main\n",
        ),
    };
    let main0_digest = cas
        .cas()
        .put_merkle_snapshot(&main0_snap)
        .expect("put main0");
    let main0_git_oid = cas
        .project_snapshot_oid(&main0_snap, &[])
        .expect("main0 oid");
    cas.cas().sync_store().expect("sync main0");
    let seq1 = d6
        .land_main_snapshot(None, &main0_digest, None, "bootstrap main")
        .expect("bootstrap main");
    assert_eq!(seq1, 1);

    let branch = BranchName::new("agent_rec").expect("branch");
    store.ensure_branch(&branch, None).expect("ensure_branch");
    let ws = store.mount(&branch).expect("mount");
    let dir = PathBuf::from(ws.path());
    fs::write(dir.join("state.txt"), "v1\n").expect("write v1");
    assert!(matches!(
        store.checkpoint(&ws, "ckpt v1").expect("ckpt v1"),
        Commit::Created(_)
    ));
    fs::write(dir.join("state.txt"), "v2\n").expect("write v2");
    fs::write(dir.join("extra.txt"), "persisted\n").expect("write extra");
    assert!(matches!(
        store.checkpoint(&ws, "ckpt v2").expect("ckpt v2"),
        Commit::Created(_)
    ));

    // Advance main cleanly (with Spawn-Task trailer for journal replay verification).
    let (land_blob_id, land_blob_sz) = cas.put_bytes_blob(b"upstream restart\n").expect("land");
    let main1_tree = cas
        .apply_mutations(
            &main0_tree,
            &[TreeMutation::Upsert {
                path: vec![b"UPSTREAM.md".to_vec()],
                kind: TreeEntryKind::File,
                id: land_blob_id.as_bytes(),
                size: land_blob_sz,
            }],
        )
        .expect("main1 tree");
    let draft_body = format_commit_body(
        "Gate <gate@spawn.local> 1700000200 +0000",
        "Gate <gate@spawn.local> 1700000200 +0000",
        "Land upstream\n\nSpawn-Task: 1\n",
    );
    let main1_draft = Snapshot {
        tree: main1_tree.clone(),
        parents: vec![main0_digest.clone()],
        commit_body: draft_body,
    };
    let main1_git_oid = cas
        .project_snapshot_oid(&main1_draft, &[main0_git_oid])
        .expect("main1 oid");
    let live_task = setup_accepted_task(
        &live_pg.pg,
        "/tmp/restart-proj.git",
        &main0_git_oid,
        &main1_git_oid,
    );
    assert_eq!(live_task, 1);
    let main1_snap = Snapshot {
        tree: main1_tree.clone(),
        parents: vec![main0_digest.clone()],
        commit_body: format_commit_body(
            "Gate <gate@spawn.local> 1700000200 +0000",
            "Gate <gate@spawn.local> 1700000200 +0000",
            &format!("Land upstream\n\nSpawn-Task: {live_task}\n"),
        ),
    };
    let main1_digest = cas
        .cas()
        .put_merkle_snapshot(&main1_snap)
        .expect("put main1");
    cas.cas().sync_store().expect("sync main1");
    d6.land_main_snapshot(
        Some(&main0_digest),
        &main1_digest,
        Some(live_task),
        "land upstream",
    )
    .expect("land upstream");
    store.unmount(&ws).expect("unmount");

    let live_seq = live_pg.pg.latest_ref_log_seq().expect("live seq");
    assert!(live_seq >= 5);

    // Host restart: drop handles, reopen CAS + store, verify GC Fence & mounted workspace contents.
    drop(store);
    drop(cas);
    let reopened_cas = Arc::new(CasMerkleStore::open(&cas_dir).expect("reopen cas"));
    let status =
        check_gc_fence(&reopened_cas, &live_binding, live_seq).expect("gc fence post-restart");
    assert_eq!(status.pg_latest_ref_seq, live_seq);
    assert_eq!(status.journal_latest_seq, live_seq);

    let reopened_store = LocalWorkspaceStore::d6(
        "restart-proj",
        scratch.join("disk-work"),
        D6StoreOptions::new(
            reopened_cas.clone(),
            live_pg.clone(),
            spawn_host::DEFAULT_MAX_FILE_BYTES,
        ),
    );
    let ws2 = reopened_store.mount(&branch).expect("remount");
    assert_eq!(
        fs::read_to_string(Path::new(ws2.path()).join("state.txt")).expect("state v2"),
        "v2\n"
    );
    assert_eq!(
        fs::read_to_string(Path::new(ws2.path()).join("extra.txt")).expect("extra"),
        "persisted\n"
    );
    reopened_store.unmount(&ws2).expect("unmount reopened");

    // Drill schema must fail closed against live CAS .gc-fence on both check and GC.
    let drill_pg = Arc::new(PgRefLedger::connect_isolated("host_pg_restart_drill"));
    let drill_binding = binding_from_pg(&drill_pg.pg);
    assert!(matches!(
        check_gc_fence(&reopened_cas, &drill_binding, 0),
        Err(D6StoreError::Cas(CasError::GcFenceMismatch { .. }))
    ));
    assert!(matches!(
        run_persistent_gc_for_ledger(
            &reopened_cas,
            drill_pg.as_ref(),
            &drill_binding,
            0,
            DEFAULT_PERSISTENT_GC_GRACE,
        ),
        Err(D6StoreError::Cas(CasError::GcFenceMismatch { .. }))
    ));

    // Simulated PG crash/restore lag at seq=1 + torn partial tail line in refs.journal.
    let restored_cas_dir = scratch.join("cas-restored");
    copy_cas_dir_excluding_fence_and_journal(&cas_dir, &restored_cas_dir);
    let mut journal_bytes =
        fs::read(reopened_cas.cas().refs_journal_path()).expect("read refs.journal");
    journal_bytes.extend_from_slice(b"{\"seq\":999,\"torn\":");
    fs::write(restored_cas_dir.join("refs.journal"), &journal_bytes)
        .expect("write restored journal");
    let restored_cas =
        Arc::new(CasMerkleStore::open(&restored_cas_dir).expect("open restored cas"));

    let restored_pg = Arc::new(PgRefLedger::connect_isolated("host_pg_restart_backup"));
    let restored_binding = binding_from_pg(&restored_pg.pg);
    bind_store_gc_fence(&restored_cas, &restored_binding).expect("bind restored fence");

    let backup_task = setup_accepted_task(
        &restored_pg.pg,
        "/tmp/restart-proj.git",
        &main0_git_oid,
        &main1_git_oid,
    );
    assert_eq!(backup_task, live_task);
    restored_pg
        .record_snapshot(
            &main0_digest,
            &main0_git_oid,
            &main0_tree,
            &[],
            "restart-proj",
            1_700_000_100_000,
        )
        .expect("record backup main0");
    assert_eq!(
        restored_pg
            .land_main_ref(
                "restart-proj",
                "main",
                None,
                &main0_digest,
                None,
                "backup init"
            )
            .expect("land backup main0"),
        1
    );

    assert!(matches!(
        check_gc_fence(&restored_cas, &restored_binding, 1),
        Err(D6StoreError::Cas(CasError::GcFenceBehindJournal {
            pg_latest_ref_seq: 1,
            ..
        }))
    ));

    let rep1 = reconcile_refs_journal(&restored_cas, restored_pg.as_ref(), 1, "host_pg_restart")
        .expect("reconcile suffix");
    assert!(rep1.entries_replayed >= 4, "{rep1:?}");
    assert_eq!(rep1.last_ledger_seq, live_seq);

    let rep2 = reconcile_refs_journal(
        &restored_cas,
        restored_pg.as_ref(),
        live_seq,
        "host_pg_restart",
    )
    .expect("idempotent second reconcile");
    assert_eq!(rep2.entries_considered, 0);
    assert_eq!(rep2.entries_replayed, 0);

    let new_seq = restored_pg.pg.latest_ref_log_seq().expect("restored seq");
    assert_eq!(new_seq, live_seq);
    check_gc_fence(&restored_cas, &restored_binding, new_seq)
        .expect("gc fence passes after reconcile");

    let restored_store = LocalWorkspaceStore::d6(
        "restart-proj",
        scratch.join("restored-disk-work"),
        D6StoreOptions::new(
            restored_cas,
            restored_pg,
            spawn_host::DEFAULT_MAX_FILE_BYTES,
        ),
    );
    let ws3 = restored_store.mount(&branch).expect("mount restored");
    assert_eq!(
        fs::read_to_string(Path::new(ws3.path()).join("state.txt")).expect("restored state v2"),
        "v2\n"
    );
    assert_eq!(
        fs::read_to_string(Path::new(ws3.path()).join("extra.txt")).expect("restored extra"),
        "persisted\n"
    );
    restored_store.unmount(&ws3).expect("unmount restored");
}

struct PinTestFixture {
    scratch: Scratch,
    cas_dir: PathBuf,
    cas: Arc<CasMerkleStore>,
    ledger_pg: Arc<PgRefLedger>,
    store: LocalWorkspaceStore,
    main0_snap: Snapshot,
    main0_digest: Digest,
    branch: BranchName,
    project: String,
}

fn seed_bootstrap_main_and_agent(
    scratch_prefix: &str,
    schema_name: &str,
    project: &str,
    branch_name: &str,
) -> PinTestFixture {
    let scratch = Scratch::new(scratch_prefix);
    let cas_dir = scratch.join("cas");
    let cas = Arc::new(CasMerkleStore::open(&cas_dir).expect("open cas"));
    let ledger_pg = Arc::new(PgRefLedger::connect_isolated(schema_name));
    let binding = binding_from_pg(&ledger_pg.pg);
    bind_store_gc_fence(&cas, &binding).expect("bind gc fence");

    let dyn_ledger: DynRefLedger = ledger_pg.clone();
    let store = LocalWorkspaceStore::d6(
        project,
        scratch.join("work"),
        D6StoreOptions::new(cas.clone(), dyn_ledger, spawn_host::DEFAULT_MAX_FILE_BYTES),
    );
    let d6 = store.d6_workspace().expect("d6 inner");

    let (base_id, base_sz) = cas.put_bytes_blob(b"base v0\n").expect("base blob");
    let main0_tree = cas
        .put_tree_cached(
            &Tree::from_entries(vec![TreeEntry::file(
                b"README.md".to_vec(),
                &base_id,
                base_sz,
            )])
            .expect("main0 tree"),
        )
        .expect("store main0 tree");
    let main0_snap = Snapshot {
        tree: main0_tree,
        parents: Vec::new(),
        commit_body: format_commit_body(
            "Spawn <spawn@localhost> 1700000100 +0000",
            "Spawn <spawn@localhost> 1700000100 +0000",
            "Bootstrap main\n",
        ),
    };
    let main0_digest = cas
        .cas()
        .put_merkle_snapshot(&main0_snap)
        .expect("put main0");
    cas.cas().sync_store().expect("sync main0");
    let seq1 = d6
        .land_main_snapshot(None, &main0_digest, None, "bootstrap main")
        .expect("bootstrap main");
    assert_eq!(seq1, 1);

    let branch = BranchName::new(branch_name).expect("branch");
    store.ensure_branch(&branch, None).expect("ensure_branch");
    PinTestFixture {
        scratch,
        cas_dir,
        cas,
        ledger_pg,
        store,
        main0_snap,
        main0_digest,
        branch,
        project: project.to_string(),
    }
}

fn replay_journal_into_fresh_pg(
    fx: &PinTestFixture,
    schema_name: &str,
) -> Vec<spawn_host::LedgerRef> {
    let restored_cas_dir = fx.scratch.join("cas-restored");
    copy_cas_dir_excluding_fence_and_journal(&fx.cas_dir, &restored_cas_dir);
    let journal_bytes = fs::read(fx.cas.cas().refs_journal_path()).expect("read journal");
    fs::write(restored_cas_dir.join("refs.journal"), journal_bytes).expect("copy journal");
    let restored_cas =
        Arc::new(CasMerkleStore::open(&restored_cas_dir).expect("open restored cas"));
    let restored_pg = Arc::new(PgRefLedger::connect_isolated(schema_name));
    let restored_binding = binding_from_pg(&restored_pg.pg);
    bind_store_gc_fence(&restored_cas, &restored_binding).expect("bind restored fence");
    restored_pg
        .record_snapshot(
            &fx.main0_digest,
            &main0_snap_oid(&fx.cas, &fx.main0_snap),
            &fx.main0_snap.tree,
            &[],
            &fx.project,
            1_700_000_100_000,
        )
        .expect("record restored main0");
    restored_pg
        .land_main_ref(
            &fx.project,
            "main",
            None,
            &fx.main0_digest,
            None,
            "bootstrap",
        )
        .expect("seed restored main");

    reconcile_refs_journal(&restored_cas, restored_pg.as_ref(), 1, "test_pin_reconcile")
        .expect("reconcile pinned refs");
    restored_pg
        .list_refs(Some(&fx.project))
        .expect("restored refs")
}

fn assert_pinned_refs_survive_gc_and_reconcile(
    fx: &PinTestFixture,
    pinned_digest: &Digest,
    accepted_ref: &str,
    committed_digest: &Digest,
) {
    let binding = binding_from_pg(&fx.ledger_pg.pg);
    let seq_now = fx.ledger_pg.pg.latest_ref_log_seq().expect("seq now");
    let gc1 = run_persistent_gc_for_ledger(
        &fx.cas,
        fx.ledger_pg.as_ref(),
        &binding,
        seq_now,
        Duration::ZERO,
    )
    .expect("gc sweep 1");
    let gc2 = run_persistent_gc_for_ledger(
        &fx.cas,
        fx.ledger_pg.as_ref(),
        &binding,
        seq_now,
        Duration::ZERO,
    )
    .expect("gc sweep 2");
    assert_eq!(gc1.roots, 3);
    assert_eq!(gc2.roots, 3);
    assert!(fx.cas.cas().has(pinned_digest));

    let restored_refs = replay_journal_into_fresh_pg(fx, "d6_pin_worker_reconcile");
    assert!(
        restored_refs
            .iter()
            .any(|r| r.name == accepted_ref && r.snapshot == *committed_digest)
    );
}

#[test]
fn pg_d6_pin_worker_head_unmount_remount_roundtrip() {
    let fx = seed_bootstrap_main_and_agent(
        "pin_worker_roundtrip",
        "d6_pin_worker_roundtrip",
        "proj-pin-rt",
        "agent_worker_rt",
    );
    let binding = binding_from_pg(&fx.ledger_pg.pg);
    let d6 = fx.store.d6_workspace().expect("d6 inner");

    let branch_before = fx
        .store
        .branch_head(&fx.branch)
        .expect("branch_head before")
        .expect("seeded branch head");

    let ws = fx.store.mount(&fx.branch).expect("mount");
    let ws_dir = PathBuf::from(ws.path());
    fs::write(ws_dir.join("README.md"), "worker modified\n").expect("modify README");
    fs::write(ws_dir.join("feature.rs"), "pub fn work() -> i32 { 42 }\n").expect("write feature");

    let head_ref = spawn_host::workspace_rules::task_head_ref(77, "head");
    let worker_ref = spawn_host::workspace_rules::worker_head_ref(fx.branch.as_str());

    let pinned1 = fx
        .store
        .pin_worker_head(fx.branch.as_str(), Some(&ws_dir), &[&head_ref])
        .expect("pin dirty workspace")
        .expect("some commit id");
    assert_eq!(
        fx.store
            .branch_head(&fx.branch)
            .expect("branch_head after")
            .expect("branch head"),
        branch_before
    );
    let worker_snap = d6
        .branch_snapshot(&worker_ref)
        .expect("worker_ref lookup")
        .expect("worker_ref exists");
    let task_snap = d6
        .branch_snapshot(&head_ref)
        .expect("task_ref lookup")
        .expect("task_ref exists");
    assert_eq!(worker_snap.git_oid.to_hex(), pinned1.as_str());
    assert_eq!(task_snap.git_oid.to_hex(), pinned1.as_str());

    let seq_after_pin = fx.ledger_pg.pg.latest_ref_log_seq().expect("seq after pin");
    let fence_status =
        check_gc_fence(&fx.cas, &binding, seq_after_pin).expect("gc fence aligned after pin");
    assert_eq!(
        fence_status.pg_latest_ref_seq,
        fence_status.journal_latest_seq
    );

    let pinned_repeat = fx
        .store
        .pin_worker_head(fx.branch.as_str(), Some(&ws_dir), &[&head_ref])
        .expect("repeat pin")
        .expect("some commit id");
    assert_eq!(pinned_repeat, pinned1);
    assert_eq!(
        fx.ledger_pg.pg.latest_ref_log_seq().expect("seq unchanged"),
        seq_after_pin
    );

    let Commit::Created(committed_oid) = fx
        .store
        .checkpoint(&ws, "worker commit checkpoint")
        .expect("checkpoint")
    else {
        panic!("expected new checkpoint commit");
    };
    fx.store.unmount(&ws).expect("unmount");
    assert!(!ws_dir.exists());

    let accepted_ref = spawn_host::workspace_rules::task_head_ref(77, "accepted");
    let pinned_post_unmount = fx
        .store
        .pin_worker_head(fx.branch.as_str(), None, &[&accepted_ref])
        .expect("post-unmount pin matches branch head")
        .expect("branch head retained after unmount");
    assert_eq!(pinned_post_unmount, committed_oid);

    let reviewer = BranchName::new("agent_reviewer").expect("reviewer branch");
    fx.store
        .ensure_branch(&reviewer, Some(pinned1.as_str()))
        .expect("fork from earlier worker CommitId");
    let review_ws = fx.store.mount(&reviewer).expect("mount reviewer");
    let review_dir = PathBuf::from(review_ws.path());
    assert_eq!(
        fs::read_to_string(review_dir.join("README.md")).expect("read remounted README"),
        "worker modified\n"
    );
    assert_eq!(
        fs::read_to_string(review_dir.join("feature.rs")).expect("read remounted feature"),
        "pub fn work() -> i32 { 42 }\n"
    );
    fx.store.unmount(&review_ws).expect("unmount reviewer");

    let committed_snap = d6
        .branch_snapshot(fx.branch.as_str())
        .expect("branch snap")
        .expect("branch exists");
    assert_pinned_refs_survive_gc_and_reconcile(
        &fx,
        &worker_snap.id,
        &accepted_ref,
        &committed_snap.id,
    );
}

fn main0_snap_oid(cas: &CasMerkleStore, snap: &Snapshot) -> GitOid {
    cas.project_snapshot_oid(snap, &[]).expect("main0 oid")
}

fn seed_inner_git_repo(ws_dir: &Path) {
    let run_git = |args: &[&str]| {
        let out = test_git_command()
            .current_dir(ws_dir)
            .args(args)
            .output()
            .expect("run test_git_command");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run_git(&["init", "-q", "-b", "work"]);
    run_git(&["config", "user.name", "Worker"]);
    run_git(&["config", "user.email", "worker@spawn.local"]);
    fs::write(ws_dir.join("committed.rs"), "pub fn a() -> i32 { 1 }\n").expect("write committed");
    run_git(&["add", "committed.rs"]);
    run_git(&["commit", "-qm", "worker inner commit"]);
    fs::write(ws_dir.join("uncommitted.rs"), "pub fn b() -> i32 { 2 }\n").expect("write dirty");
    fs::write(ws_dir.join(".git/info/exclude"), "scratch.tmp\n").expect("write info exclude");
    fs::write(ws_dir.join("scratch.tmp"), "ignore me\n").expect("write scratch.tmp");
}

#[test]
fn pg_d6_pin_worker_head_local_git_boundary() {
    let scratch = Scratch::new("pin_worker_git_boundary");
    let cas = Arc::new(CasMerkleStore::open(scratch.join("cas")).expect("open cas"));
    let ledger_pg = Arc::new(PgRefLedger::connect_isolated("d6_pin_worker_git_boundary"));
    let binding = binding_from_pg(&ledger_pg.pg);
    bind_store_gc_fence(&cas, &binding).expect("bind gc fence");

    let dyn_ledger: DynRefLedger = ledger_pg;
    let store = LocalWorkspaceStore::d6(
        "proj-pin-boundary",
        scratch.join("work"),
        D6StoreOptions::new(cas.clone(), dyn_ledger, spawn_host::DEFAULT_MAX_FILE_BYTES),
    );
    let d6 = store.d6_workspace().expect("d6 inner");

    let branch = BranchName::new("agent_git_bound").expect("branch");
    store.ensure_branch(&branch, None).expect("ensure_branch");
    let ws = store.mount(&branch).expect("mount");
    let ws_dir = PathBuf::from(ws.path());

    let outside = scratch.join("outside.git");
    fs::create_dir_all(&outside).expect("mkdir outside");
    fs::write(outside.join("secret.txt"), "do not leak into CAS\n").expect("write outside secret");
    seed_inner_git_repo(&ws_dir);

    let task_ref = spawn_host::workspace_rules::task_head_ref(88, "head");
    let commit = store
        .pin_worker_head(branch.as_str(), Some(&ws_dir), &[&task_ref])
        .expect("pin with inner .git dir")
        .expect("pinned commit");
    let worker_snap = d6
        .branch_snapshot(&spawn_host::workspace_rules::worker_head_ref(
            branch.as_str(),
        ))
        .expect("worker snap")
        .expect("present");
    assert_eq!(worker_snap.git_oid.to_hex(), commit.as_str());

    let flat = cas.flatten_tree(&worker_snap.tree).expect("flatten tree");
    assert!(flat.contains_key(b"committed.rs".as_slice()));
    assert!(flat.contains_key(b"uncommitted.rs".as_slice()));
    assert!(!flat.contains_key(b"scratch.tmp".as_slice()));
    assert!(
        flat.keys()
            .all(|p| p != b".git" && !p.starts_with(b".git/"))
    );

    // Replace inner `.git` with an arbitrary `gitdir:` file: D6 never follows `.git` pointers,
    // so both checkpoint and pin_worker_head treat workspace files identically without leaking `.git`.
    fs::remove_dir_all(ws_dir.join(".git")).expect("remove inner .git dir");
    fs::write(
        ws_dir.join(".git"),
        format!("gitdir: {}\n", outside.display()),
    )
    .expect("gitdir");
    fs::write(ws_dir.join("after_gitfile.rs"), "pub fn c() -> i32 { 3 }\n")
        .expect("write after gitdir");
    store
        .checkpoint(&ws, "checkpoint ignores gitdir file")
        .expect("checkpoint succeeds with gitdir pointer file");
    let pin_with_gitfile = store
        .pin_worker_head(branch.as_str(), Some(&ws_dir), &[&task_ref])
        .expect("pin_worker_head matches checkpoint behavior on gitdir file")
        .expect("pinned commit");
    let after_snap = d6
        .branch_snapshot(&task_ref)
        .expect("task snap")
        .expect("present");
    assert_eq!(after_snap.git_oid.to_hex(), pin_with_gitfile.as_str());
    let flat_after = cas.flatten_tree(&after_snap.tree).expect("flatten after");
    assert!(flat_after.contains_key(b"after_gitfile.rs".as_slice()));
    assert!(!flat_after.contains_key(b"secret.txt".as_slice()));
    assert!(
        flat_after
            .keys()
            .all(|p| p != b".git" && !p.starts_with(b".git/"))
    );

    // Symlink `.git` is still refused fail-closed by the workspace scanner, while unmount cleans up.
    fs::remove_file(ws_dir.join(".git")).expect("remove gitdir file");
    std::os::unix::fs::symlink(&outside, ws_dir.join(".git")).expect("symlink .git");
    let sym_err = store
        .pin_worker_head(branch.as_str(), Some(&ws_dir), &[])
        .unwrap_err();
    assert!(
        sym_err.to_string().contains(".git") && sym_err.to_string().contains("symlink"),
        "{sym_err}"
    );
    store
        .unmount(&ws)
        .expect("unmount tolerates symlink .git cleanup");
}

#[test]
fn pg_d6_pin_worker_head_discard_does_not_leak_previous_task_into_new_task() {
    let fx = seed_bootstrap_main_and_agent(
        "pin_worker_discard",
        "d6_pin_worker_discard",
        "proj-pin-discard",
        "agent_reuse",
    );
    let d6 = fx.store.d6_workspace().expect("d6 inner");
    let main_snap = d6.main_snapshot().expect("main").expect("main seeded");
    assert_eq!(main_snap.id, fx.main0_digest);

    // Task 601 runs on agent_reuse and pins old_task.rs without landing to branch.
    let ws1 = fx.store.mount(&fx.branch).expect("mount task 601");
    let dir1 = PathBuf::from(ws1.path());
    fs::write(dir1.join("old_task.rs"), "pub fn old_work() -> i32 { 1 }\n")
        .expect("write old_task.rs");
    let old_ref = spawn_host::workspace_rules::task_head_ref(601, "head");
    let old_pin = fx
        .store
        .pin_worker_head(fx.branch.as_str(), Some(&dir1), &[&old_ref])
        .expect("pin task 601")
        .expect("old pin commit");
    let worker_ref = spawn_host::workspace_rules::worker_head_ref(fx.branch.as_str());
    let worker_after_601 = d6
        .branch_snapshot(&worker_ref)
        .expect("worker snap")
        .expect("exists");
    assert_eq!(worker_after_601.git_oid.to_hex(), old_pin.as_str());

    // Discard resets worker ref back to branch head and drops the old workspace.
    d6.discard(&ws1).expect("discard task 601");
    assert_eq!(
        d6.branch_snapshot(&worker_ref)
            .expect("worker after discard")
            .expect("present")
            .id,
        fx.main0_digest
    );

    // Even with a stale worker ref left behind, `directory=None` MUST pin branch head (`main0`).
    fx.ledger_pg
        .cas_ref(
            &fx.project,
            &worker_ref,
            spawn_host::D6RefKind::Branch,
            Some(&fx.main0_digest),
            &worker_after_601.id,
            "simulate stale worker ref",
        )
        .expect("set stale worker ref");
    let stale_acc_ref = spawn_host::workspace_rules::task_head_ref(602, "accepted");
    let no_dir_pin = fx
        .store
        .pin_worker_head(fx.branch.as_str(), None, &[&stale_acc_ref])
        .expect("pin 602 without directory")
        .expect("commit");
    assert_eq!(no_dir_pin.as_str(), main_snap.git_oid.to_hex());
    let snap_602_acc = d6
        .branch_snapshot(&stale_acc_ref)
        .expect("602 acc")
        .expect("exists");
    assert_eq!(snap_602_acc.id, fx.main0_digest);
    assert!(
        !fx.cas
            .flatten_tree(&snap_602_acc.tree)
            .expect("flatten 602")
            .contains_key(b"old_task.rs".as_slice())
    );

    // With a live directory too, the new commit must parent from `branch` (`main0`), never task 601.
    fx.ledger_pg
        .cas_ref(
            &fx.project,
            &worker_ref,
            spawn_host::D6RefKind::Branch,
            Some(&fx.main0_digest),
            &worker_after_601.id,
            "simulate stale worker ref before mounted task",
        )
        .expect("set stale worker ref 2");

    let ws2 = fx.store.mount(&fx.branch).expect("mount task 603");
    let dir2 = PathBuf::from(ws2.path());
    assert!(!dir2.join("old_task.rs").exists());
    fs::write(dir2.join("new_task.rs"), "pub fn new_work() -> i32 { 2 }\n")
        .expect("write new_task.rs");
    let ref_603 = spawn_host::workspace_rules::task_head_ref(603, "head");
    let pin_603 = fx
        .store
        .pin_worker_head(fx.branch.as_str(), Some(&dir2), &[&ref_603])
        .expect("pin task 603")
        .expect("603 commit");
    let snap_603 = d6
        .branch_snapshot(&ref_603)
        .expect("603 snap")
        .expect("exists");
    assert_eq!(snap_603.git_oid.to_hex(), pin_603.as_str());
    assert_eq!(snap_603.parents, vec![fx.main0_digest.clone()]);
    let flat_603 = fx.cas.flatten_tree(&snap_603.tree).expect("flatten 603");
    assert!(flat_603.contains_key(b"new_task.rs".as_slice()));
    assert!(!flat_603.contains_key(b"old_task.rs".as_slice()));
    fx.store.unmount(&ws2).expect("unmount task 603");
}

fn run_with_blocked_journal_once<F: FnOnce()>(journal_path: &Path, backup_path: &Path, op: F) {
    fs::copy(journal_path, backup_path).expect("backup refs.journal");
    fs::remove_file(journal_path).expect("rm refs.journal");
    fs::create_dir_all(journal_path).expect("inject directory at refs.journal");
    op();
    fs::remove_dir_all(journal_path).expect("remove blocker dir");
    fs::copy(backup_path, journal_path).expect("restore refs.journal");
}

#[test]
fn pg_d6_pin_worker_head_retries_heal_missing_refs_journal_entry() {
    let fx = seed_bootstrap_main_and_agent(
        "pin_worker_half_write",
        "d6_pin_worker_half_write",
        "proj-pin-heal",
        "agent_heal",
    );
    let binding = binding_from_pg(&fx.ledger_pg.pg);
    let d6 = fx.store.d6_workspace().expect("d6 inner");
    let journal_path = fx.cas.cas().refs_journal_path();
    let journal_backup = fx.scratch.join("refs.journal.bak");

    let task_ref_none = spawn_host::workspace_rules::task_head_ref(901, "accepted");
    run_with_blocked_journal_once(&journal_path, &journal_backup, || {
        let half_err = fx
            .store
            .pin_worker_head(fx.branch.as_str(), None, &[&task_ref_none])
            .unwrap_err();
        assert!(!half_err.to_string().is_empty());
    });

    let worker_ref = spawn_host::workspace_rules::worker_head_ref(fx.branch.as_str());
    assert!(
        d6.branch_snapshot(&worker_ref)
            .expect("pg worker")
            .is_some()
    );
    assert!(
        !fx.cas
            .cas()
            .read_ref_journal()
            .expect("read journal")
            .iter()
            .any(|e| e.name == worker_ref)
    );

    let healed_commit = fx
        .store
        .pin_worker_head(fx.branch.as_str(), None, &[&task_ref_none])
        .expect("retry heals refs.journal")
        .expect("commit");
    assert_eq!(
        healed_commit.as_str(),
        main0_snap_oid(&fx.cas, &fx.main0_snap).to_hex()
    );
    let seq_healed = fx.ledger_pg.pg.latest_ref_log_seq().expect("seq healed");
    check_gc_fence(&fx.cas, &binding, seq_healed).expect("gc fence aligned after heal");

    fx.store
        .pin_worker_head(fx.branch.as_str(), None, &[&task_ref_none])
        .expect("repeat after heal");
    assert_eq!(
        fx.ledger_pg.pg.latest_ref_log_seq().expect("seq unchanged"),
        seq_healed
    );

    // Dirty publish half-write retry also heals worker_ref + task_ref_dirty cleanly.
    let ws = fx.store.mount(&fx.branch).expect("mount heal dirty");
    let ws_dir = PathBuf::from(ws.path());
    fs::write(ws_dir.join("heal_dirty.rs"), "pub fn heal() -> i32 { 7 }\n").expect("write dirty");
    let task_ref_dirty = spawn_host::workspace_rules::task_head_ref(902, "head");
    run_with_blocked_journal_once(&journal_path, &journal_backup, || {
        let _err = fx
            .store
            .pin_worker_head(fx.branch.as_str(), Some(&ws_dir), &[&task_ref_dirty])
            .unwrap_err();
    });

    let dirty_commit = fx
        .store
        .pin_worker_head(fx.branch.as_str(), Some(&ws_dir), &[&task_ref_dirty])
        .expect("dirty retry heals refs.journal")
        .expect("dirty commit");
    let dirty_snap = d6
        .branch_snapshot(&worker_ref)
        .expect("worker dirty snap")
        .expect("present");
    assert_eq!(dirty_snap.git_oid.to_hex(), dirty_commit.as_str());
    check_gc_fence(
        &fx.cas,
        &binding,
        fx.ledger_pg.pg.latest_ref_log_seq().expect("seq healed 2"),
    )
    .expect("gc fence aligned after dirty heal");
    fx.store.unmount(&ws).expect("unmount heal dirty");

    let restored_refs = replay_journal_into_fresh_pg(&fx, "d6_pin_worker_heal_restore");
    for (ref_name, expected_snap) in [
        (worker_ref.as_str(), &dirty_snap.id),
        (task_ref_none.as_str(), &fx.main0_digest),
        (task_ref_dirty.as_str(), &dirty_snap.id),
    ] {
        assert!(
            restored_refs
                .iter()
                .any(|r| r.name == ref_name && r.snapshot == *expected_snap),
            "missing healed {ref_name} after reconcile"
        );
    }
}

#[test]
fn pg_d6_pin_worker_head_half_write_then_branch_move_keeps_journal_chain_valid() {
    let fx = seed_bootstrap_main_and_agent(
        "pin_worker_half_write_move",
        "d6_pin_worker_half_write_move",
        "proj-pin-move",
        "agent_move",
    );
    let d6 = fx.store.d6_workspace().expect("d6 inner");
    let ws = fx.store.mount(&fx.branch).expect("mount agent_move");
    let ws_dir = PathBuf::from(ws.path());
    let worker_ref = spawn_host::workspace_rules::worker_head_ref(fx.branch.as_str());
    let task_ref = spawn_host::workspace_rules::task_head_ref(910, "accepted");

    fs::write(ws_dir.join("step.txt"), "b0\n").expect("write b0");
    let Commit::Created(b0) = fx.store.checkpoint(&ws, "ckpt b0").expect("ckpt b0") else {
        panic!("expected Created b0");
    };
    let b0_snap = d6
        .branch_snapshot(fx.branch.as_str())
        .expect("b0 snap")
        .expect("present");
    let p0 = fx
        .store
        .pin_worker_head(fx.branch.as_str(), None, &[&task_ref])
        .expect("pin b0")
        .expect("commit b0");
    assert_eq!(p0, b0);

    fs::write(ws_dir.join("step.txt"), "b1\n").expect("write b1");
    let Commit::Created(b1) = fx.store.checkpoint(&ws, "ckpt b1").expect("ckpt b1") else {
        panic!("expected Created b1");
    };
    let b1_snap = d6
        .branch_snapshot(fx.branch.as_str())
        .expect("b1 snap")
        .expect("present");
    // Simulate a half-write crash between PG `cas_ref` (B0 -> B1) and `refs.journal` append.
    for ref_name in [&worker_ref, &task_ref] {
        fx.ledger_pg
            .cas_ref(
                &fx.project,
                ref_name,
                D6RefKind::Branch,
                Some(&b0_snap.id),
                &b1_snap.id,
                "d6:pin_worker_head",
            )
            .expect("simulate pg half-write B0 -> B1");
        assert_eq!(
            d6.branch_snapshot(ref_name)
                .expect("snap after half write")
                .expect("present")
                .id,
            b1_snap.id
        );
    }

    fs::write(ws_dir.join("step.txt"), "b2\n").expect("write b2");
    let Commit::Created(b2) = fx.store.checkpoint(&ws, "ckpt b2").expect("ckpt b2") else {
        panic!("expected Created b2");
    };
    let b2_snap = d6
        .branch_snapshot(fx.branch.as_str())
        .expect("b2 snap")
        .expect("present");
    assert_ne!(b1, b2);

    let p2 = fx
        .store
        .pin_worker_head(fx.branch.as_str(), None, &[&task_ref])
        .expect("retry pin at b2 heals journal before advancing")
        .expect("commit b2");
    assert_eq!(p2, b2);
    fx.store.unmount(&ws).expect("unmount agent_move");

    let suffix = fx
        .cas
        .cas()
        .reconcile_entries_after(1)
        .expect("reconcile_entries_after(1) must keep valid fast-forward chain");
    assert!(!suffix.is_empty());

    let restored_refs = replay_journal_into_fresh_pg(&fx, "d6_pin_worker_move_restore");
    for expected_ref in [worker_ref.as_str(), task_ref.as_str()] {
        assert!(
            restored_refs
                .iter()
                .any(|r| r.name == expected_ref && r.snapshot == b2_snap.id),
            "expected {expected_ref} at B2 in replayed PG ledger"
        );
    }
}

#[test]
fn pg_d6_concurrent_checkpoint_pin_and_discard_keep_refs_journal_strictly_monotonic() {
    let fx = seed_bootstrap_main_and_agent(
        "pin_worker_concurrent_seq",
        "d6_pin_worker_concurrent_seq",
        "proj-pin-conc",
        "agent_conc_base",
    );
    let threads = 6usize;
    let branches: Vec<BranchName> = (0..threads)
        .map(|idx| BranchName::new(format!("agent_conc_{idx}")).expect("branch"))
        .collect();
    for branch in &branches {
        fx.store.ensure_branch(branch, None).expect("ensure_branch");
    }

    let barrier = std::sync::Barrier::new(threads);
    std::thread::scope(|scope| {
        for (idx, branch) in branches.iter().enumerate() {
            let fx = &fx;
            let barrier = &barrier;
            scope.spawn(move || {
                barrier.wait();
                let task_id = 950 + i64::try_from(idx).expect("thread index");
                let task_ref = spawn_host::workspace_rules::task_head_ref(task_id, "head");
                for round in 0..3 {
                    let ws = fx.store.mount(branch).expect("mount");
                    let ws_dir = PathBuf::from(ws.path());
                    fs::write(ws_dir.join("work.txt"), format!("t{idx}-r{round}-a\n"))
                        .expect("write a");
                    assert!(matches!(
                        fx.store
                            .checkpoint(&ws, &format!("ckpt {idx}/{round}"))
                            .expect("ckpt"),
                        Commit::Created(_)
                    ));
                    fs::write(ws_dir.join("work.txt"), format!("t{idx}-r{round}-b\n"))
                        .expect("write b");
                    fx.store
                        .pin_worker_head(branch.as_str(), Some(&ws_dir), &[&task_ref])
                        .expect("pin dirty")
                        .expect("dirty commit");
                    fx.store
                        .pin_worker_head(branch.as_str(), None, &[&task_ref])
                        .expect("pin clean")
                        .expect("clean commit");
                    fx.store.discard(&ws).expect("discard");
                }
            });
        }
    });

    let entries = fx.cas.cas().read_ref_journal().expect("read journal");
    assert!(
        entries.len() > 20,
        "expected >= 20 entries, got {}",
        entries.len()
    );
    assert!(
        entries.windows(2).all(|w| w[1].seq > w[0].seq),
        "non-monotonic seq in refs.journal"
    );
    let suffix = fx
        .cas
        .cas()
        .reconcile_entries_after(1)
        .expect("reconcile_entries_after(1) succeeds");
    assert!(!suffix.is_empty());

    let binding = binding_from_pg(&fx.ledger_pg.pg);
    let pg_seq = fx.ledger_pg.pg.latest_ref_log_seq().expect("pg seq");
    let fence = check_gc_fence(&fx.cas, &binding, pg_seq).expect("gc fence aligned");
    assert_eq!(fence.pg_latest_ref_seq, fence.journal_latest_seq);

    let live: std::collections::BTreeMap<String, Digest> = fx
        .ledger_pg
        .list_refs(Some(&fx.project))
        .expect("list live refs")
        .into_iter()
        .map(|r| (r.name, r.snapshot))
        .collect();
    let restored: std::collections::BTreeMap<String, Digest> =
        replay_journal_into_fresh_pg(&fx, "d6_pin_worker_concurrent_restore")
            .into_iter()
            .map(|r| (r.name, r.snapshot))
            .collect();
    assert_eq!(restored, live);
}

#[test]
fn d6_workspace_native_writes_produce_commit_v2_without_compat_body() {
    let scratch = Scratch::new("native_v2");
    let cas = Arc::new(CasMerkleStore::open(scratch.join("cas")).expect("open cas"));
    let ledger = Arc::new(PgRefLedger::connect_isolated("d6_native_commit_v2"));
    let clock_ms = Arc::new(AtomicI64::new(1_700_200_000_000));
    let clk = clock_ms.clone();
    let d6_root = scratch.join("ws");

    let ws_store = D6Workspace::new(
        "proj-native-v2",
        &d6_root,
        cas.clone(),
        ledger.clone(),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    )
    .with_clock_ms(Arc::new(move || clk.fetch_add(1_000, Ordering::Relaxed)));
    let branch = BranchName::new("agent_v2").expect("branch name");
    let (main0, ws) = verify_native_v2_branch_commits(&cas, &ws_store, &branch);
    verify_native_v2_land_merge_set_aside_and_compat(
        &cas, &ledger, &ws_store, &branch, &ws, &main0, &clock_ms,
    );
}

fn verify_native_v2_branch_commits(
    cas: &CasMerkleStore,
    ws_store: &D6Workspace<PgRefLedger>,
    branch: &BranchName,
) -> (LedgerSnapshot, WorkspaceId) {
    ws_store.ensure_branch(branch, None).expect("ensure branch");
    let main0 = ws_store.main_snapshot().expect("main").expect("seeded");
    assert!(matches!(
        cas.cas().get_commit_object(&main0.id).expect("read seed"),
        CommitObject::V2(ref c)
            if c.git_compat_body.is_none()
                && c.intent.origin == CommitOrigin::Import
                && matches!(c.actor, CommitActor::System { .. })
    ));

    let ws = ws_store.mount(branch).expect("mount");
    let ws_dir = PathBuf::from(ws.path());
    fs::write(ws_dir.join("alpha.txt"), "hello v2\n").expect("write alpha");
    let ckpt = ws_store
        .checkpoint(&ws, "native checkpoint\r\n")
        .expect("checkpoint");
    assert!(matches!(ckpt, Commit::Created(_)));
    let head1 = ws_store
        .branch_snapshot(branch.as_str())
        .expect("snap")
        .expect("present");
    assert!(matches!(
        cas.cas().get_commit_object(&head1.id).expect("read head1"),
        CommitObject::V2(ref c)
            if c.git_compat_body.is_none()
                && c.intent.origin
                    == CommitOrigin::Work {
                        change: WorkChange::Edit,
                        boundary: WorkBoundary::Checkpoint,
                    }
    ));

    fs::write(ws_dir.join("beta.txt"), "typed commit v2\n").expect("write beta");
    let typed_actor = CommitActor::Agent {
        id: "agent_v2".to_string(),
        execution_id: Some("exec-v2-1".to_string()),
        model_label: Some("gpt-test".to_string()),
        preset_id: Some("coder".to_string()),
    };
    let typed_intent = CommitIntent {
        origin: CommitOrigin::Work {
            change: WorkChange::Edit,
            boundary: WorkBoundary::Submit,
        },
        task_id: Some(42),
        transcript_ref: Some("transcript://42".to_string()),
        message: "typed submit commit".to_string(),
    };
    let c_v2 = ws_store
        .commit_v2(&ws, &typed_actor, &typed_intent, "d6:commit_v2")
        .expect("commit_v2");
    assert!(matches!(c_v2, Commit::Created(_)));
    let head2 = ws_store
        .branch_snapshot(branch.as_str())
        .expect("head2")
        .expect("present");
    let obj2 = cas.cas().get_commit_object(&head2.id).expect("read head2");
    assert!(matches!(
        obj2,
        CommitObject::V2(ref c)
            if c.git_compat_body.is_none()
                && c.actor == typed_actor
                && c.intent == typed_intent
    ));

    fs::write(ws_dir.join("dirty.txt"), "dirty pin\n").expect("write dirty");
    let pinned = ws_store
        .pin_worker_head(branch.as_str(), Some(&ws_dir), &[])
        .expect("pin")
        .expect("pinned");
    let worker_ref = spawn_host::workspace_rules::worker_head_ref(branch.as_str());
    let worker_snap = ws_store
        .branch_snapshot(&worker_ref)
        .expect("worker snap")
        .expect("present");
    assert_eq!(worker_snap.git_oid.to_hex(), pinned.as_str());
    assert!(matches!(
        cas.cas()
            .get_commit_object(&worker_snap.id)
            .expect("read worker snap"),
        CommitObject::V2(ref c)
            if c.git_compat_body.is_none()
                && c.intent.origin
                    == CommitOrigin::Work {
                        change: WorkChange::Edit,
                        boundary: WorkBoundary::Submit,
                    }
    ));
    (main0, ws)
}

fn publish_test_v2_main(
    cas: &CasMerkleStore,
    ledger: &PgRefLedger,
    main0: &LedgerSnapshot,
    clock_ms: &AtomicI64,
) {
    let (main_blob, main_sz) = cas.put_bytes_blob(b"main update\n").expect("main blob");
    let next_main_tree = cas
        .apply_mutations(
            &main0.tree,
            &[TreeMutation::Upsert {
                path: vec![b"MAIN.md".to_vec()],
                kind: TreeEntryKind::File,
                id: main_blob.as_bytes(),
                size: main_sz,
            }],
        )
        .expect("next main tree");
    let main_actor = CommitActor::System {
        id: "integrator".to_string(),
    };
    let main_intent = CommitIntent {
        origin: CommitOrigin::Land,
        task_id: None,
        transcript_ref: None,
        message: "land main doc".to_string(),
    };
    let pre_main_oid = cas
        .project_commit_object_oid(
            &CommitObject::V2(spawn_cas::CommitV2 {
                tree: next_main_tree.clone(),
                parents: vec![main0.id.clone()],
                at_ms: clock_ms.load(Ordering::Relaxed),
                actor: main_actor.clone(),
                intent: main_intent.clone(),
                git_compat_body: None,
            }),
            &[main0.git_oid],
        )
        .expect("project oid");
    let task_id = setup_accepted_task(
        &ledger.pg,
        "/tmp/proj-native-v2.git",
        &main0.git_oid,
        &pre_main_oid,
    );
    let main1 = cas
        .publish_commit_v2(
            ledger,
            &spawn_host::cas_merkle::PublishCommitV2Request {
                project: "proj-native-v2",
                ref_name: "main",
                kind: D6RefKind::Main,
                expected_ref: Some(&main0.id),
                tree: &next_main_tree,
                parents: std::slice::from_ref(&main0.id),
                created_at_ms: clock_ms.fetch_add(1_000, Ordering::Relaxed),
                actor: &main_actor,
                intent: &main_intent,
                git_compat_body: None,
                reason: "d6:test_land",
                landing_task_id: Some(task_id),
            },
        )
        .expect("publish v2 main");
    assert_eq!(main1.git_oid, pre_main_oid);
}

fn verify_native_v2_land_merge_set_aside_and_compat(
    cas: &CasMerkleStore,
    ledger: &PgRefLedger,
    ws_store: &D6Workspace<PgRefLedger>,
    branch: &BranchName,
    ws: &WorkspaceId,
    main0: &LedgerSnapshot,
    clock_ms: &AtomicI64,
) {
    let ws_dir = PathBuf::from(ws.path());
    publish_test_v2_main(cas, ledger, main0, clock_ms);

    let merged = ws_store.merge_base(ws).expect("merge_base");
    assert!(matches!(merged, BaseMerge::Merged { .. }));
    let head3 = ws_store
        .branch_snapshot(branch.as_str())
        .expect("head3")
        .expect("present");
    assert!(matches!(
        cas.cas().get_commit_object(&head3.id).expect("read head3"),
        CommitObject::V2(ref c)
            if c.git_compat_body.is_none()
                && c.intent.origin
                    == CommitOrigin::Work {
                        change: WorkChange::Derive,
                        boundary: WorkBoundary::Checkpoint,
                    }
    ));

    let sa = ws_store
        .set_aside(branch, None, "set aside v2 work")
        .expect("set_aside");
    assert!(matches!(sa, SetAside::Created { .. }));
    let head4 = ws_store
        .branch_snapshot(branch.as_str())
        .expect("head4")
        .expect("present");
    assert!(matches!(
        cas.cas().get_commit_object(&head4.id).expect("read head4"),
        CommitObject::V2(ref c)
            if c.git_compat_body.is_none()
                && c.intent.origin
                    == CommitOrigin::Work {
                        change: WorkChange::Revert,
                        boundary: WorkBoundary::Checkpoint,
                    }
    ));

    fs::write(ws_dir.join("compat.txt"), "legacy body\n").expect("write compat");
    let compat_body = format_commit_body(
        "Compat <c@spawn.local> 1700200100 +0000",
        "Compat <c@spawn.local> 1700200100 +0000",
        "compat commit\n",
    );
    let compat = ws_store
        .commit_with_body(ws, &compat_body, 1_700_200_100_000, "d6:compat")
        .expect("commit_with_body");
    assert!(matches!(compat, Commit::Created(_)));
    let head5 = ws_store
        .branch_snapshot(branch.as_str())
        .expect("head5")
        .expect("present");
    assert!(matches!(
        cas.cas().get_commit_object(&head5.id).expect("read head5"),
        CommitObject::V1(_)
    ));
}
