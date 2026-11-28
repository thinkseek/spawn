//! D6 Phase 4 Step 4 end-to-end integration tests (`#113` Comment #5 & `#95`):
//! - Pure-Rust Layer 0 / workspace materialization (`0644`/`0755`, symlinks, gitlinks,
//!   incremental hard-links from `nearest_dir`, strict non-CAS inode isolation, atomic rename)
//! - `.gitattributes` `export-ignore` / `export-subst` / `eol` pre-cutover audit
//! - Live Temp PG 16 7-step lossless project cutover, `.merge/*` fail-closed gate, parity check,
//!   read-only border mirror, post-cutover `spawn_ref_land_main`, and lossless rollback to Git
//! - Live Temp PG 16 `refs.journal` crash-recovery fast-forward replay (covering both `spawn_ref_cas`
//!   and `spawn_ref_land_main`), GC Fence mismatch/lagging rejections, and 2-round `gc_persistent()`
//! - End-to-end R2 `git bundle` export -> blank-store import roundtrip & `> 100 MiB` local-only reporting

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde_json::json;
use spawn_cas::{
    CommitActor, CommitIntent, CommitObject, CommitOrigin, CommitV2, DEFAULT_PERSISTENT_GC_GRACE,
    Digest, GIT_POINTER_THRESHOLD, GcFenceBinding, GitOid, JournalEntry, NS_BLOB, Snapshot, Store,
    Tree, TreeEntry, TreeEntryKind, TreeMutation, WorkBoundary, WorkChange,
};
use spawn_core::id::BranchName;
use spawn_core::port::{BaseMerge, Commit, SetAside, WorkspaceStore};
use spawn_host::cas_merkle::PublishCommitV2Request;
use spawn_host::d6_workspace::{
    D6Workspace, HostMemorySample, L1Admission, L1FallbackReason, L1FlushReason, L1TmpfsPolicy,
};
use spawn_host::workspace::{D6StoreOptions, DynRefLedger};
use spawn_host::{
    CasMerkleStore, CutoverPhase, D6RefKind, D6StoreError, ExistingDestPolicy, GitBorderAdapter,
    Layer0Options, LedgerRef, LedgerSnapshot, LocalWorkspaceStore, ProjectCutover,
    ProjectStorageMode, PublishRequest, RefLedger, assert_zero_pending_merges,
    audit_export_attributes, bind_store_gc_fence, canonical_root, check_gc_fence,
    format_commit_body, materialize_layer0, materialize_workspace_from_tree,
    reconcile_refs_journal, run_persistent_gc_for_ledger, test_git_command,
    verify_bundle_snapshot_roundtrip, verify_cutover_parity, verify_zero_pending_merges,
};
use spawn_postgres::testing::TestSchema;
use spawn_postgres::{PostgresStore, RefKind, SnapshotState};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "spawn-host-step4-{tag}-{}-{}",
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
    fn connect_isolated(case: &str) -> Self {
        let schema = Arc::new(TestSchema::new(case));
        let pg = schema.store();
        Self {
            pg,
            _schema: schema,
        }
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
        .mission_add("D6 step4 gate", &json!([source]), "human")
        .expect("mission_add");
    store
        .mission_configure(mission, Some("/scenarios"), "gate", None, None)
        .expect("mission_configure");
    let task = store
        .task_assign(
            mission,
            "ephemeral",
            "w_d6_step4",
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

fn git_in(dir: &Path, args: &[&str]) -> Vec<u8> {
    let mut cmd = test_git_command();
    let out = cmd.current_dir(dir).args(args).output().expect("run git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

struct RepeatPatternReader {
    remaining: u64,
    byte: u8,
}

impl Read for RepeatPatternReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 || buf.is_empty() {
            return Ok(0);
        }
        let n = usize::try_from(
            self.remaining
                .min(u64::try_from(buf.len()).unwrap_or(u64::MAX)),
        )
        .expect("bounded by buf.len");
        buf[..n].fill(self.byte);
        self.remaining -= u64::try_from(n).expect("fits u64");
        Ok(n)
    }
}

#[derive(Clone)]
struct ManualClock(Arc<Mutex<SystemTime>>);

impl ManualClock {
    fn new(start: SystemTime) -> Self {
        Self(Arc::new(Mutex::new(start)))
    }

    fn advance(&self, by: Duration) {
        *self.0.lock().expect("clock") += by;
    }

    fn now_ms(&self) -> i64 {
        let dur = self
            .0
            .lock()
            .expect("clock")
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("after epoch");
        i64::try_from(dur.as_millis()).expect("ms fits i64")
    }

    fn cas_store(&self, root: &Path) -> CasMerkleStore {
        let clock = self.0.clone();
        let store = Store::open(root)
            .expect("open cas")
            .with_clock(Arc::new(move || *clock.lock().expect("clock")));
        CasMerkleStore::from_store(store)
    }
}

#[test]
#[expect(clippy::too_many_lines)]
fn layer0_incremental_materialization_enforces_modes_atomic_rename_and_never_hardlinks_cas() {
    let scratch = Scratch::new("layer0_mat");
    let store = CasMerkleStore::open(scratch.join("cas")).expect("open cas");

    let (readme_v1_id, readme_v1_sz) = store.put_bytes_blob(b"# Layer 0 v1\n").expect("readme v1");
    let (script_id, script_sz) = store
        .put_bytes_blob(b"#!/bin/sh\necho ok\n")
        .expect("script");
    let (link_target_id, link_target_sz) = store.put_bytes_blob(b"bin/run.sh").expect("link");

    let mut big_payload = vec![0x5Au8; 4_500_000];
    for (idx, byte) in big_payload.iter_mut().enumerate().step_by(4096) {
        *byte = u8::try_from(idx % 251).expect("byte");
    }
    let (big_id, big_sz) = store
        .put_bytes_blob(&big_payload)
        .expect("big chunked blob");
    assert!(store.cas().chunk_index_path(&big_id).is_file());

    let gitlink_oid = [0x42u8; 20];
    let bin_tree_v1 = store
        .put_tree_cached(
            &Tree::from_entries(vec![TreeEntry::executable(
                b"run.sh".to_vec(),
                &script_id,
                script_sz,
            )])
            .expect("bin tree"),
        )
        .expect("store bin tree");
    let assets_tree = store
        .put_tree_cached(
            &Tree::from_entries(vec![TreeEntry::file(b"big.bin".to_vec(), &big_id, big_sz)])
                .expect("assets tree"),
        )
        .expect("store assets");
    let deps_tree = store
        .put_tree_cached(
            &Tree::from_entries(vec![
                TreeEntry::gitlink(b"submod".to_vec(), &gitlink_oid).expect("gitlink"),
            ])
            .expect("deps tree"),
        )
        .expect("store deps");

    let tree_v1 = store
        .put_tree_cached(
            &Tree::from_entries(vec![
                TreeEntry::file(b"README.md".to_vec(), &readme_v1_id, readme_v1_sz),
                TreeEntry::dir(b"assets".to_vec(), &assets_tree),
                TreeEntry::dir(b"bin".to_vec(), &bin_tree_v1),
                TreeEntry::symlink(b"current.link".to_vec(), &link_target_id, link_target_sz),
                TreeEntry::dir(b"deps".to_vec(), &deps_tree),
            ])
            .expect("root tree v1"),
        )
        .expect("store tree v1");

    let layers_dir = scratch.join("layers");
    let layer1 = layers_dir.join("l1");
    let rep1 = materialize_workspace_from_tree(&store, &tree_v1, &layer1).expect("materialize l1");
    assert_eq!(rep1.files_written, 3);
    assert_eq!(rep1.hardlinked_from_nearest, 0);
    assert_eq!(rep1.symlinks_written, 1);
    assert_eq!(rep1.gitlinks_materialized, 1);
    assert_eq!(rep1.dirs_created, 3);
    assert!(!rep1.reused_existing_dest);

    let readme_l1_meta = fs::symlink_metadata(layer1.join("README.md")).expect("readme l1");
    let script_l1_meta = fs::symlink_metadata(layer1.join("bin/run.sh")).expect("script l1");
    let big_l1_meta = fs::symlink_metadata(layer1.join("assets/big.bin")).expect("big l1");
    assert_eq!(readme_l1_meta.permissions().mode() & 0o777, 0o644);
    assert_eq!(script_l1_meta.permissions().mode() & 0o777, 0o755);
    assert_eq!(big_l1_meta.permissions().mode() & 0o777, 0o644);

    assert_eq!(
        fs::read_link(layer1.join("current.link")).expect("readlink"),
        Path::new("bin/run.sh")
    );
    let gitlink_meta = fs::symlink_metadata(layer1.join("deps/submod")).expect("gitlink dir");
    assert!(gitlink_meta.is_dir());
    assert_eq!(
        fs::read_dir(layer1.join("deps/submod"))
            .expect("read gitlink")
            .count(),
        0
    );

    // Every CAS object stays 0444 with link count 1 and distinct inode from Layer 0 files.
    for (digest, layer_meta) in [
        (&readme_v1_id, &readme_l1_meta),
        (&script_id, &script_l1_meta),
    ] {
        let cas_meta = fs::symlink_metadata(store.cas().object_path(digest)).expect("cas object");
        assert_eq!(cas_meta.permissions().mode() & 0o777, 0o444);
        assert_eq!(cas_meta.nlink(), 1);
        assert_ne!(layer_meta.ino(), cas_meta.ino());
    }

    // No staging temp dirs remain under layers/.
    for entry in fs::read_dir(&layers_dir).expect("read layers dir") {
        let name = entry.expect("entry").file_name();
        assert!(
            !name.to_string_lossy().starts_with(".d6-layer0-stage"),
            "leftover stage dir: {name:?}"
        );
    }

    // Build tree v2 modifying README.md and adding CHANGELOG.md while keeping bin/run.sh & assets/big.bin.
    let (readme_v2_id, readme_v2_sz) = store.put_bytes_blob(b"# Layer 0 v2\n").expect("readme v2");
    let (chg_id, chg_sz) = store.put_bytes_blob(b"- updated\n").expect("changelog");
    let tree_v2 = store
        .put_tree_cached(
            &Tree::from_entries(vec![
                TreeEntry::file(b"CHANGELOG.md".to_vec(), &chg_id, chg_sz),
                TreeEntry::file(b"README.md".to_vec(), &readme_v2_id, readme_v2_sz),
                TreeEntry::dir(b"assets".to_vec(), &assets_tree),
                TreeEntry::dir(b"bin".to_vec(), &bin_tree_v1),
                TreeEntry::symlink(b"current.link".to_vec(), &link_target_id, link_target_sz),
                TreeEntry::dir(b"deps".to_vec(), &deps_tree),
            ])
            .expect("root tree v2"),
        )
        .expect("store tree v2");

    let layer2 = layers_dir.join("l2");
    let rep2 = materialize_layer0(
        &store,
        &tree_v2,
        &layer2,
        &Layer0Options {
            nearest: Some((&tree_v1, &layer1)),
            if_exists: ExistingDestPolicy::Fail,
        },
    )
    .expect("materialize l2 incremental");
    assert_eq!(rep2.hardlinked_from_nearest, 2);
    assert_eq!(rep2.files_written, 2);

    let script_l2_meta = fs::symlink_metadata(layer2.join("bin/run.sh")).expect("script l2");
    let big_l2_meta = fs::symlink_metadata(layer2.join("assets/big.bin")).expect("big l2");
    let readme_l2_meta = fs::symlink_metadata(layer2.join("README.md")).expect("readme l2");
    let script_cas_meta =
        fs::symlink_metadata(store.cas().object_path(&script_id)).expect("cas script");
    assert_eq!(script_l2_meta.permissions().mode() & 0o777, 0o755);
    assert_eq!(big_l2_meta.permissions().mode() & 0o777, 0o644);
    assert_eq!(readme_l2_meta.permissions().mode() & 0o777, 0o644);
    assert_eq!(
        (script_l2_meta.dev(), script_l2_meta.ino()),
        (script_l1_meta.dev(), script_l1_meta.ino())
    );
    assert_ne!(script_l2_meta.ino(), script_cas_meta.ino());
    assert_eq!(
        (big_l2_meta.dev(), big_l2_meta.ino()),
        (big_l1_meta.dev(), big_l1_meta.ino())
    );
    assert_ne!(readme_l2_meta.ino(), readme_l1_meta.ino());
    assert_eq!(
        fs::read_to_string(layer2.join("README.md")).expect("read v2"),
        "# Layer 0 v2\n"
    );

    // ReuseIfPresent is idempotent; Replace atomically swaps destination.
    let reused = materialize_layer0(
        &store,
        &tree_v2,
        &layer2,
        &Layer0Options {
            nearest: None,
            if_exists: ExistingDestPolicy::ReuseIfPresent,
        },
    )
    .expect("reuse l2");
    assert!(reused.reused_existing_dest);

    let replaced = materialize_layer0(
        &store,
        &tree_v1,
        &layer2,
        &Layer0Options {
            nearest: None,
            if_exists: ExistingDestPolicy::Replace,
        },
    )
    .expect("replace l2");
    assert!(!replaced.reused_existing_dest);
    assert!(!layer2.join("CHANGELOG.md").exists());

    // Passing nearest_dir inside CAS root is refused fail-closed.
    let cas_err = materialize_layer0(
        &store,
        &tree_v1,
        &layers_dir.join("l_bad"),
        &Layer0Options {
            nearest: Some((&tree_v1, store.cas().root())),
            if_exists: ExistingDestPolicy::Fail,
        },
    )
    .expect_err("refuse nearest_dir inside CAS root");
    assert!(matches!(cas_err, D6StoreError::InvalidInput(_)));

    // Even if an external dir contains a hard-link to a CAS object, it is NEVER hard-linked.
    let poisoned = layers_dir.join("poisoned_nearest");
    fs::create_dir_all(&poisoned).expect("mkdir poisoned");
    fs::hard_link(store.cas().object_path(&script_id), poisoned.join("run.sh"))
        .expect("seed poison hardlink");
    let clean_dest = layers_dir.join("clean_from_poison");
    let rep_clean = materialize_layer0(
        &store,
        &bin_tree_v1,
        &clean_dest,
        &Layer0Options {
            nearest: Some((&bin_tree_v1, &poisoned)),
            if_exists: ExistingDestPolicy::Fail,
        },
    )
    .expect("skip poisoned hardlink");
    assert_eq!(rep_clean.hardlinked_from_nearest, 0);
    assert_eq!(rep_clean.files_written, 1);
    let clean_script_meta = fs::symlink_metadata(clean_dest.join("run.sh")).expect("clean run.sh");
    assert_eq!(clean_script_meta.permissions().mode() & 0o777, 0o755);
    assert_ne!(clean_script_meta.ino(), script_cas_meta.ino());
}

#[test]
fn audit_export_attributes_detects_export_ignore_export_subst_and_eol() {
    let scratch = Scratch::new("layer0_attrs");
    let store = CasMerkleStore::open(scratch.join("cas")).expect("open cas");

    let root_attrs = b"[attr]dist export-ignore\nBUILD_INFO.txt export-subst\n*.sh eol=lf\nwin/*.ps1 eol=crlf\ndocs/*.md text\n*.generated dist\n";
    let sub_attrs = b"keep.generated -export-ignore\nrun.sh text=auto\n";
    let body = b"content\n";

    let (ra_id, ra_sz) = store.put_bytes_blob(root_attrs).expect("ra");
    let (sa_id, sa_sz) = store.put_bytes_blob(sub_attrs).expect("sa");
    let (b_id, b_sz) = store.put_bytes_blob(body).expect("body");

    let scripts_tree = store
        .put_tree_cached(
            &Tree::from_entries(vec![
                TreeEntry::file(b".gitattributes".to_vec(), &sa_id, sa_sz),
                TreeEntry::file(b"keep.generated".to_vec(), &b_id, b_sz),
                TreeEntry::executable(b"run.sh".to_vec(), &b_id, b_sz),
            ])
            .expect("scripts tree"),
        )
        .expect("store scripts");
    let win_tree = store
        .put_tree_cached(
            &Tree::from_entries(vec![TreeEntry::file(b"deploy.ps1".to_vec(), &b_id, b_sz)])
                .expect("win"),
        )
        .expect("store win");
    let docs_tree = store
        .put_tree_cached(
            &Tree::from_entries(vec![TreeEntry::file(b"guide.md".to_vec(), &b_id, b_sz)])
                .expect("docs"),
        )
        .expect("store docs");

    let root = store
        .put_tree_cached(
            &Tree::from_entries(vec![
                TreeEntry::file(b".gitattributes".to_vec(), &ra_id, ra_sz),
                TreeEntry::file(b"BUILD_INFO.txt".to_vec(), &b_id, b_sz),
                TreeEntry::dir(b"docs".to_vec(), &docs_tree),
                TreeEntry::file(b"foo.generated".to_vec(), &b_id, b_sz),
                TreeEntry::dir(b"scripts".to_vec(), &scripts_tree),
                TreeEntry::dir(b"win".to_vec(), &win_tree),
            ])
            .expect("root"),
        )
        .expect("store root");

    let audit = audit_export_attributes(&store, &root).expect("audit");
    assert!(audit.has_findings());
    assert!(audit.has_export_divergence_risk());
    assert!(audit.has_eol_rules());
    assert_eq!(audit.gitattributes_files.len(), 2);
    assert_eq!(audit.export_ignore_paths.len(), 1);
    assert_eq!(audit.export_ignore_paths[0].shown(), "foo.generated");
    assert_eq!(audit.export_subst_paths.len(), 1);
    assert_eq!(audit.export_subst_paths[0].shown(), "BUILD_INFO.txt");

    let eol_map: HashMap<String, (&str, Option<&str>)> = audit
        .eol_paths
        .iter()
        .map(|f| (f.path.shown(), (f.eol.as_str(), f.text.as_deref())))
        .collect();
    assert_eq!(eol_map.get("scripts/run.sh"), Some(&("lf", Some("auto"))));
    assert_eq!(eol_map.get("win/deploy.ps1"), Some(&("crlf", None)));
    assert_eq!(
        eol_map.get("docs/guide.md"),
        Some(&("unspecified", Some("text")))
    );
    assert!(audit.rules.len() >= 6);
}

#[test]
#[expect(clippy::too_many_lines)]
fn pg_live_cutover_7_steps_pending_merge_guard_parity_and_lossless_rollback() {
    let scratch = Scratch::new("pg_cutover");
    let cas_store = CasMerkleStore::open(scratch.join("cas")).expect("open cas");
    let ledger = PgRefLedger::connect_isolated("d6_step4_cutover");
    let adapter = GitBorderAdapter::new();

    let work = scratch.join("work");
    fs::create_dir_all(&work).expect("mkdir work");
    git_in(&work, &["init", "-b", "main"]);
    fs::write(work.join("README.md"), "# Cutover Project\n").expect("write readme");
    git_in(&work, &["add", "README.md"]);
    git_in(&work, &["commit", "-m", "initial main"]);

    git_in(&work, &["checkout", "-b", "feat/alpha"]);
    fs::write(work.join("alpha.txt"), "alpha work\n").expect("write alpha");
    git_in(&work, &["add", "alpha.txt"]);
    git_in(&work, &["commit", "-m", "feature alpha"]);

    git_in(&work, &["checkout", "main"]);
    git_in(&work, &["tag", "v0.1"]);

    let bare_repo = scratch.join("origin.git");
    git_in(
        &scratch.0,
        &[
            "clone",
            "--bare",
            work.to_str().expect("utf8"),
            bare_repo.to_str().expect("utf8"),
        ],
    );
    git_in(
        &work,
        &["remote", "add", "origin", bare_repo.to_str().expect("utf8")],
    );

    let ws_root = scratch.join("workspaces");
    fs::create_dir_all(ws_root.join(".merge/feat")).expect("mkdir .merge/feat");
    fs::write(
        ws_root.join(".merge/feat/alpha"),
        b"pending-merge\0deadbeef",
    )
    .expect("write pending");
    assert!(
        !verify_zero_pending_merges(&ws_root)
            .expect("audit")
            .is_clean()
    );
    assert!(assert_zero_pending_merges(&ws_root).is_err());

    let mut cutover = ProjectCutover::new("cutover-proj", &bare_repo, &ws_root);
    assert_eq!(cutover.phase(), CutoverPhase::NotStarted);
    assert_eq!(cutover.storage_mode(), ProjectStorageMode::Git);

    // Step 1: Historical backfill into CAS + spawn_snapshots only (refs stay untouched yet).
    let backfill = cutover
        .step1_backfill_history(&cas_store, &ledger)
        .expect("step1 backfill");
    assert_eq!(backfill.commits_scanned, 2);
    assert_eq!(backfill.snapshots_newly_imported, 2);
    assert!(
        ledger
            .list_refs(Some("cutover-proj"))
            .expect("list refs")
            .is_empty()
    );

    // Simulate tail commits arriving between Step 1 and Step 2 pause (including main + new branch).
    fs::write(
        work.join(".gitattributes"),
        "*.generated export-ignore\nREADME.md eol=lf\n",
    )
    .expect("write attrs");
    fs::write(work.join("tmp.generated"), "ignored on archive\n").expect("write gen");
    git_in(&work, &["add", ".gitattributes", "tmp.generated"]);
    git_in(&work, &["commit", "-m", "tail commit on main"]);
    git_in(&work, &["checkout", "-b", "feat/tail"]);
    fs::write(work.join("tail.rs"), "pub fn tail() {}\n").expect("write tail");
    git_in(&work, &["add", "tail.rs"]);
    git_in(&work, &["commit", "-m", "tail branch commit"]);
    git_in(&work, &["push", "--all", "origin"]);

    // Step 2 must fail closed while .merge/feat/alpha exists, leaving dispatch paused for cleanup.
    let blocked = cutover.step2_pause_dispatch_and_verify_clean(&cas_store);
    assert!(matches!(blocked, Err(D6StoreError::InvalidInput(_))));
    assert_eq!(cutover.phase(), CutoverPhase::DispatchPaused);
    assert!(cutover.dispatch_paused());
    assert_eq!(cutover.storage_mode(), ProjectStorageMode::Git);

    // Clear .merge/* and complete Steps 2 -> 7.
    fs::remove_dir_all(ws_root.join(".merge")).expect("clear .merge");
    let step2 = cutover
        .step2_pause_dispatch_and_verify_clean(&cas_store)
        .expect("step2 clean");
    assert!(step2.pending_audit.is_clean());
    assert!(cutover.dispatch_paused());
    assert_eq!(cutover.phase(), CutoverPhase::DispatchPausedAndDrained);

    let step3 = cutover
        .step3_import_tail_commits(&cas_store, &ledger)
        .expect("step3 tail");
    assert_eq!(step3.commits_scanned, 4);
    assert_eq!(step3.snapshots_newly_imported, 2);
    assert_eq!(cutover.phase(), CutoverPhase::TailImported);

    let step4 = cutover
        .step4_commit_refs_and_flip_mode(&cas_store, &ledger, &adapter, "cutover")
        .expect("step4 commit refs");
    assert_eq!(step4.refs.len(), 4);
    assert_eq!(cutover.storage_mode(), ProjectStorageMode::D6Cas);
    assert_eq!(cutover.phase(), CutoverPhase::RefsCommitted);
    assert!(
        cutover
            .export_attributes_audit()
            .expect("audit")
            .has_export_divergence_risk()
    );

    let parity = cutover
        .step5_verify_parity(&cas_store, &ledger)
        .expect("step5 parity");
    assert_eq!(parity.refs.len(), 4);
    assert_eq!(cutover.phase(), CutoverPhase::ParityVerified);

    cutover
        .step6_configure_readonly_border(&adapter)
        .expect("step6 readonly");
    assert_eq!(cutover.phase(), CutoverPhase::BorderReadOnly);

    // Direct push to bare_repo is now rejected by pre-receive hook.
    let push_while_locked = test_git_command()
        .current_dir(&work)
        .args(["push", "origin", "HEAD:refs/heads/bypass"])
        .output()
        .expect("push");
    assert!(!push_while_locked.status.success());

    cutover.step7_resume_dispatch().expect("step7 resume");
    assert_eq!(cutover.phase(), CutoverPhase::Resumed);
    assert!(!cutover.dispatch_paused());
    assert_eq!(cutover.events().len(), 7);

    // Perform a live D6 commit advancing main via PG spawn_ref_land_main!
    let main_ref = parity.refs.get("refs/heads/main").expect("main parity");
    let main_snap = cas_store
        .cas()
        .get_commit_object(&main_ref.snapshot)
        .expect("read main snap");
    let (post_id, post_sz) = cas_store
        .put_bytes_blob(b"post-cutover D6 file\n")
        .expect("put post file");
    let post_tree = cas_store
        .apply_mutations(
            main_snap.tree(),
            &[spawn_cas::TreeMutation::Upsert {
                path: vec![b"POST_CUTOVER.txt".to_vec()],
                kind: spawn_cas::TreeEntryKind::File,
                id: post_id.as_bytes(),
                size: post_sz,
            }],
        )
        .expect("mutate tree");
    let body_draft = format_commit_body(
        "D6 Bot <bot@spawn.local> 1700001000 +0000",
        "D6 Bot <bot@spawn.local> 1700001000 +0000",
        "Post-cutover D6 landing",
    );
    let cand_snap_draft = Snapshot {
        tree: post_tree.clone(),
        parents: vec![main_ref.snapshot.clone()],
        commit_body: body_draft,
    };
    let cand_oid_draft = cas_store
        .project_snapshot_oid(&cand_snap_draft, &[main_ref.git_oid])
        .expect("project cand");
    let task_id = setup_accepted_task(
        &ledger.pg,
        bare_repo.to_str().expect("utf8"),
        &main_ref.git_oid,
        &cand_oid_draft,
    );
    let published = cas_store
        .publish_snapshot(
            &ledger,
            &PublishRequest {
                project: "cutover-proj",
                ref_name: "main",
                kind: D6RefKind::Main,
                expected_ref: Some(&main_ref.snapshot),
                tree: &post_tree,
                parents: std::slice::from_ref(&main_ref.snapshot),
                commit_body: &cand_snap_draft.commit_body,
                created_at_ms: 1_700_001_000_000,
                reason: "post-cutover live land",
                landing_task_id: Some(task_id),
            },
        )
        .expect("publish live main");

    // Parity check detects bare_repo main is behind D6 RefLedger until border/rollback exports.
    let stale_parity = verify_cutover_parity(&cas_store, &ledger, "cutover-proj", &bare_repo);
    assert!(matches!(
        stale_parity,
        Err(D6StoreError::OidMismatch { .. })
    ));

    // Lossless rollback exports post-cutover main to bare_repo, lifts readonly lock, verifies parity!
    let rollback = cutover
        .rollback_to_git(&cas_store, &ledger, &adapter)
        .expect("rollback");
    assert_eq!(cutover.phase(), CutoverPhase::RolledBack);
    assert_eq!(cutover.storage_mode(), ProjectStorageMode::Git);
    assert!(!cutover.dispatch_paused());
    assert_eq!(rollback.exported_refs, 4);
    let rb_parity = rollback.parity.expect("rollback parity");
    assert_eq!(
        rb_parity.refs.get("refs/heads/main").expect("main").git_oid,
        published.git_oid
    );

    // Direct push works again after rollback & fsck passes!
    git_in(&work, &["fetch", "origin", "main"]);
    git_in(&work, &["checkout", "-B", "rollback-check", "FETCH_HEAD"]);
    assert_eq!(
        fs::read_to_string(work.join("POST_CUTOVER.txt")).expect("post cutover file"),
        "post-cutover D6 file\n"
    );
    fs::write(work.join("POST_ROLLBACK.txt"), "after rollback\n").expect("write");
    git_in(&work, &["add", "POST_ROLLBACK.txt"]);
    git_in(&work, &["commit", "-m", "push after rollback"]);
    git_in(&work, &["push", "origin", "HEAD:refs/heads/rollback-check"]);

    let mut fsck_cmd = test_git_command();
    let fsck = fsck_cmd
        .arg("--git-dir")
        .arg(&bare_repo)
        .args(["fsck", "--full", "--strict"])
        .output()
        .expect("fsck");
    assert!(
        fsck.status.success(),
        "fsck stderr: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}

#[test]
#[expect(clippy::too_many_lines)]
fn pg_refs_journal_crash_recovery_fast_forward_and_gc_fence_two_round_gc() {
    let scratch = Scratch::new("pg_journal_gc");
    let start_clock = SystemTime::now() + Duration::from_mins(1);
    let clock = ManualClock::new(start_clock);
    let cas_root = scratch.join("cas");
    let store = clock.cas_store(&cas_root);

    let live_ledger = PgRefLedger::connect_isolated("d6_step4_live");
    let drill_ledger = PgRefLedger::connect_isolated("d6_step4_drill");
    let live_ident = live_ledger.pg.database_identity().expect("live ident");
    let drill_ident = drill_ledger.pg.database_identity().expect("drill ident");

    let live_binding = spawn_cas::GcFenceBinding {
        db_system_id: live_ident.db_system_id,
        db_oid: live_ident.db_oid,
        schema_name: live_ident.schema_name,
    };
    let drill_binding = spawn_cas::GcFenceBinding {
        db_system_id: drill_ident.db_system_id,
        db_oid: drill_ident.db_oid,
        schema_name: drill_ident.schema_name,
    };
    bind_store_gc_fence(&store, &live_binding).expect("bind gc fence");

    // 1. Drill/restore schema is refused cleanly by GC Fence!
    let drill_err = run_persistent_gc_for_ledger(
        &store,
        &drill_ledger,
        &drill_binding,
        0,
        DEFAULT_PERSISTENT_GC_GRACE,
    )
    .expect_err("drill DB must be rejected by GC fence");
    assert!(matches!(
        drill_err,
        D6StoreError::Cas(spawn_cas::CasError::GcFenceMismatch { .. })
    ));

    // 2. Publish 4 commits in `live_ledger`: main bootstrap (seq 1), branch create (seq 2),
    // branch advance (seq 3), main land via accepted task (seq 4), plus an orphan unreferenced snapshot.
    let (b1, sz1) = store.put_bytes_blob(b"base file\n").expect("b1");
    let (b2, sz2) = store.put_bytes_blob(b"branch work 1\n").expect("b2");
    let (b3, sz3) = store.put_bytes_blob(b"branch work 2\n").expect("b3");
    let (b_orp, sz_orp) = store
        .put_bytes_blob(b"orphan unreachable content\n")
        .expect("b_orp");

    let t1 = store
        .put_tree_cached(
            &Tree::from_entries(vec![TreeEntry::file(b"base.txt".to_vec(), &b1, sz1)]).expect("t1"),
        )
        .expect("store t1");
    let t2 = store
        .put_tree_cached(
            &Tree::from_entries(vec![
                TreeEntry::file(b"base.txt".to_vec(), &b1, sz1),
                TreeEntry::file(b"feat.txt".to_vec(), &b2, sz2),
            ])
            .expect("t2"),
        )
        .expect("store t2");
    let t3 = store
        .put_tree_cached(
            &Tree::from_entries(vec![
                TreeEntry::file(b"base.txt".to_vec(), &b1, sz1),
                TreeEntry::file(b"feat.txt".to_vec(), &b3, sz3),
            ])
            .expect("t3"),
        )
        .expect("store t3");
    let t_orp = store
        .put_tree_cached(
            &Tree::from_entries(vec![TreeEntry::file(
                b"orphan.txt".to_vec(),
                &b_orp,
                sz_orp,
            )])
            .expect("t_orp"),
        )
        .expect("store t_orp");

    let empty_body = format_commit_body(
        "Alice <alice@spawn.local> 1700000100 +0000",
        "Alice <alice@spawn.local> 1700000100 +0000",
        "Bootstrap main",
    );
    let ts0 = clock.now_ms();
    let p_main1 = store
        .publish_snapshot(
            &live_ledger,
            &PublishRequest {
                project: "rec-proj",
                ref_name: "main",
                kind: D6RefKind::Main,
                expected_ref: None,
                tree: &t1,
                parents: &[],
                commit_body: &empty_body,
                created_at_ms: ts0,
                reason: "bootstrap main",
                landing_task_id: None,
            },
        )
        .expect("pub main 1");
    assert_eq!(p_main1.ref_seq, 1);

    let br_body1 = format_commit_body(
        "Alice <alice@spawn.local> 1700000200 +0000",
        "Alice <alice@spawn.local> 1700000200 +0000",
        "Branch checkpoint 1",
    );
    let p_br1 = store
        .publish_snapshot(
            &live_ledger,
            &PublishRequest {
                project: "rec-proj",
                ref_name: "feat/x",
                kind: D6RefKind::Branch,
                expected_ref: None,
                tree: &t2,
                parents: std::slice::from_ref(&p_main1.snapshot),
                commit_body: &br_body1,
                created_at_ms: ts0 + 1,
                reason: "branch 1",
                landing_task_id: None,
            },
        )
        .expect("pub br 1");
    assert_eq!(p_br1.ref_seq, 2);

    let br_body2 = format_commit_body(
        "Alice <alice@spawn.local> 1700000300 +0000",
        "Alice <alice@spawn.local> 1700000300 +0000",
        "Branch checkpoint 2",
    );
    let p_br2 = store
        .publish_snapshot(
            &live_ledger,
            &PublishRequest {
                project: "rec-proj",
                ref_name: "feat/x",
                kind: D6RefKind::Branch,
                expected_ref: Some(&p_br1.snapshot),
                tree: &t3,
                parents: std::slice::from_ref(&p_br1.snapshot),
                commit_body: &br_body2,
                created_at_ms: ts0 + 2,
                reason: "branch 2",
                landing_task_id: None,
            },
        )
        .expect("pub br 2");
    assert_eq!(p_br2.ref_seq, 3);

    // Prepare accepted landing task on BOTH `live_ledger` and a simulated backup-restored schema!
    let live_task = setup_accepted_task(
        &live_ledger.pg,
        "/tmp/rec-proj.git",
        &p_main1.git_oid,
        &store
            .project_snapshot_oid(
                &Snapshot {
                    tree: t3.clone(),
                    parents: vec![p_main1.snapshot.clone(), p_br2.snapshot.clone()],
                    commit_body: format_commit_body(
                        "Gate <gate@spawn.local> 1700000400 +0000",
                        "Gate <gate@spawn.local> 1700000400 +0000",
                        "Land feat/x\n\nSpawn-Task: 1\n",
                    ),
                },
                &[p_main1.git_oid, p_br2.git_oid],
            )
            .expect("proj"),
    );
    assert_eq!(live_task, 1);
    let land_body = format_commit_body(
        "Gate <gate@spawn.local> 1700000400 +0000",
        "Gate <gate@spawn.local> 1700000400 +0000",
        &format!("Land feat/x\n\nSpawn-Task: {live_task}\n"),
    );
    let p_main2 = store
        .publish_snapshot(
            &live_ledger,
            &PublishRequest {
                project: "rec-proj",
                ref_name: "main",
                kind: D6RefKind::Main,
                expected_ref: Some(&p_main1.snapshot),
                tree: &t3,
                parents: &[p_main1.snapshot.clone(), p_br2.snapshot.clone()],
                commit_body: &land_body,
                created_at_ms: ts0 + 3,
                reason: "land feat/x",
                landing_task_id: Some(live_task),
            },
        )
        .expect("pub main 2");
    assert_eq!(p_main2.ref_seq, 4);

    // Append an unreferenced orphan snapshot (published only to CAS + spawn_snapshots, no ref!).
    let orp_body = format_commit_body(
        "Alice <alice@spawn.local> 1700000500 +0000",
        "Alice <alice@spawn.local> 1700000500 +0000",
        "Orphan WIP snapshot",
    );
    let orp_snap = Snapshot {
        tree: t_orp.clone(),
        parents: vec![p_main1.snapshot.clone()],
        commit_body: orp_body,
    };
    let orp_snap_id = store.cas().put_merkle_snapshot(&orp_snap).expect("put orp");
    let orp_oid = store
        .project_snapshot_oid(&orp_snap, &[p_main1.git_oid])
        .expect("orp oid");
    store.cas().sync_store().expect("sync orp");
    live_ledger
        .record_snapshot(
            &orp_snap_id,
            &orp_oid,
            &t_orp,
            std::slice::from_ref(&p_main1.snapshot),
            "rec-proj",
            ts0 + 4,
        )
        .expect("record orphan metadata");

    // 3. Verify GC Fence behind journal refuses if pg_latest_ref_seq < journal latest seq (4),
    // and succeeds once pg_latest_ref_seq == 4!
    let lag_err = check_gc_fence(&store, &live_binding, 1).expect_err("lagging PG refused");
    assert!(matches!(
        lag_err,
        D6StoreError::Cas(spawn_cas::CasError::GcFenceBehindJournal {
            pg_latest_ref_seq: 1,
            journal_latest_seq: 4,
        })
    ));
    let ok_fence = check_gc_fence(
        &store,
        &live_binding,
        live_ledger.pg.latest_ref_log_seq().expect("latest seq"),
    )
    .expect("live fence ok");
    assert_eq!(ok_fence.pg_latest_ref_seq, 4);
    assert_eq!(ok_fence.journal_latest_seq, 4);

    // 4. Two-round persistent GC over `live_ledger`:
    // Round 1 (within 24h grace and round=1) MUST defer orphan and retain live roots.
    let r1 = run_persistent_gc_for_ledger(
        &store,
        &live_ledger,
        &live_binding,
        live_ledger.pg.latest_ref_log_seq().expect("seq"),
        DEFAULT_PERSISTENT_GC_GRACE,
    )
    .expect("gc round 1");
    assert!(r1.deleted_objects.is_empty());
    assert!(r1.deferred_objects >= 3);
    assert!(store.cas().has(&orp_snap_id));
    assert!(store.cas().has(&b_orp));

    // Advance injected clock past 24h grace -> Round 2 deletes orphan objects while preserving live!
    clock.advance(Duration::from_hours(25));
    let r2 = run_persistent_gc_for_ledger(
        &store,
        &live_ledger,
        &live_binding,
        live_ledger.pg.latest_ref_log_seq().expect("seq"),
        DEFAULT_PERSISTENT_GC_GRACE,
    )
    .expect("gc round 2");
    assert!(r2.deleted_objects.contains(&orp_snap_id));
    assert!(r2.deleted_objects.contains(&t_orp));
    assert!(r2.deleted_objects.contains(&b_orp));
    assert!(!store.cas().has(&orp_snap_id));
    assert!(!store.cas().has(&b_orp));
    assert!(store.cas().has(&p_main2.snapshot));
    assert!(store.cas().has(&b3));

    // Mark orphan snapshot row as 'collected' in PG while live snapshot stays 'live'.
    let marked = live_ledger
        .pg
        .mark_snapshots_collected(&[p_main2.snapshot.to_string(), orp_snap_id.to_string()])
        .expect("mark collected");
    assert_eq!(marked, 1);
    assert_eq!(
        live_ledger
            .pg
            .get_snapshot(&orp_snap_id.to_string())
            .expect("get")
            .expect("row")
            .state,
        SnapshotState::Collected
    );
    assert_eq!(
        live_ledger
            .pg
            .get_snapshot(&p_main2.snapshot.to_string())
            .expect("get")
            .expect("row")
            .state,
        SnapshotState::Live
    );

    // 5. Simulate crash + PG restore lagging at seq=1, with a torn partial tail line in refs.journal!
    let backup_ledger = PgRefLedger::connect_isolated("d6_step4_restored");
    let backup_ident = backup_ledger.pg.database_identity().expect("backup ident");
    let backup_binding = spawn_cas::GcFenceBinding {
        db_system_id: backup_ident.db_system_id,
        db_oid: backup_ident.db_oid,
        schema_name: backup_ident.schema_name,
    };

    // Restore CAS dir bound to `backup_binding` and copy `refs.journal` + torn tail line!
    let rec_cas_dir = scratch.join("cas_recovered");
    let rec_store = CasMerkleStore::open(&rec_cas_dir).expect("open rec cas");
    for (digest, bytes) in [
        (&b1, b"base file\n".as_slice()),
        (&b2, b"branch work 1\n".as_slice()),
        (&b3, b"branch work 2\n".as_slice()),
    ] {
        let (stored_id, _) = rec_store.put_bytes_blob(bytes).expect("seed blob");
        assert_eq!(&stored_id, digest);
    }
    for tree_id in [&t1, &t2, &t3] {
        let tree_obj = store.read_tree_cached(tree_id).expect("read src tree");
        let put_id = rec_store.put_tree_cached(&tree_obj).expect("put rec tree");
        assert_eq!(&put_id, tree_id);
    }
    for snap_id in [
        &p_main1.snapshot,
        &p_br1.snapshot,
        &p_br2.snapshot,
        &p_main2.snapshot,
    ] {
        let put_snap = match store.cas().get_commit_object(snap_id).expect("src snap") {
            spawn_cas::CommitObject::V1(s) => rec_store
                .cas()
                .put_merkle_snapshot(&s)
                .expect("put rec snap"),
            spawn_cas::CommitObject::V2(v2) => rec_store
                .cas()
                .put_commit_v2(&v2)
                .expect("put rec commit v2"),
        };
        assert_eq!(&put_snap, snap_id);
    }

    // Seed backup DB ONLY up to seq=1 (missing seq=2, 3, 4!), re-recording matching task gate verdict.
    let backup_task = setup_accepted_task(
        &backup_ledger.pg,
        "/tmp/rec-proj.git",
        &p_main1.git_oid,
        &p_main2.git_oid,
    );
    assert_eq!(backup_task, live_task);
    backup_ledger
        .record_snapshot(
            &p_main1.snapshot,
            &p_main1.git_oid,
            &t1,
            &[],
            "rec-proj",
            ts0,
        )
        .expect("backup snap 1");
    let seq1 = backup_ledger
        .land_main_ref(
            "rec-proj",
            "main",
            None,
            &p_main1.snapshot,
            None,
            "bootstrap backup",
        )
        .expect("backup ref 1");
    assert_eq!(seq1, 1);

    let mut journal_bytes = fs::read(store.cas().refs_journal_path()).expect("read live journal");
    journal_bytes.extend_from_slice(b"{\"seq\":5,\"torn\":");
    fs::write(rec_store.cas().refs_journal_path(), &journal_bytes).expect("write rec journal");
    bind_store_gc_fence(&rec_store, &backup_binding).expect("bind backup fence");

    let backup_pg_seq = backup_ledger.pg.latest_ref_log_seq().expect("backup seq");
    assert_eq!(backup_pg_seq, 1);

    // Before reconciliation, GC fence rejects because PG (1) < journal (4).
    assert!(matches!(
        check_gc_fence(&rec_store, &backup_binding, backup_pg_seq),
        Err(D6StoreError::Cas(
            spawn_cas::CasError::GcFenceBehindJournal { .. }
        ))
    ));

    // Reconcile fast-forwards seq 2, 3, 4 cleanly (ignoring torn trailing line).
    let rec_rep = reconcile_refs_journal(&rec_store, &backup_ledger, backup_pg_seq, "crash_replay")
        .expect("reconcile");
    assert_eq!(rec_rep.entries_considered, 3);
    assert_eq!(rec_rep.entries_replayed, 3);
    assert_eq!(rec_rep.already_applied, 0);
    assert_eq!(rec_rep.last_journal_seq, 4);
    assert_eq!(rec_rep.last_ledger_seq, 4);

    // Idempotent second reconcile run replays 0 more entries cleanly.
    let new_pg_seq = backup_ledger.pg.latest_ref_log_seq().expect("new pg seq");
    assert_eq!(new_pg_seq, 4);
    check_gc_fence(&rec_store, &backup_binding, new_pg_seq).expect("fence passes after replay");
    let rec_rep2 = reconcile_refs_journal(&rec_store, &backup_ledger, new_pg_seq, "crash_replay")
        .expect("reconcile idempotent");
    assert_eq!(rec_rep2.entries_considered, 0);
    assert_eq!(rec_rep2.entries_replayed, 0);

    // Recovered refs in backup PG match live PG exactly!
    let backup_refs: HashMap<String, Digest> = backup_ledger
        .list_refs(Some("rec-proj"))
        .expect("backup refs")
        .into_iter()
        .map(|r| (r.name, r.snapshot))
        .collect();
    assert_eq!(backup_refs.get("main"), Some(&p_main2.snapshot));
    assert_eq!(backup_refs.get("feat/x"), Some(&p_br2.snapshot));
}

#[test]
#[expect(clippy::too_many_lines)]
fn pg_r2_git_bundle_drill_recomputes_all_snapshots_and_reports_local_only_large_files() {
    let scratch = Scratch::new("pg_bundle_drill");
    let store = CasMerkleStore::open(scratch.join("cas")).expect("open cas");
    let ledger = PgRefLedger::connect_isolated("d6_step4_bundle");
    let adapter = GitBorderAdapter::new();

    // Project A: includes a > 100 MiB (GIT_POINTER_THRESHOLD + 1) local-only FastCDC file.
    let (readme_id, readme_sz) = store.put_bytes_blob(b"# Bundle Drill\n").expect("readme");
    let (tool_id, tool_sz) = store.put_bytes_blob(b"#!/bin/sh\necho ok\n").expect("tool");
    let (link_id, link_sz) = store.put_bytes_blob(b"README.md").expect("symlink");

    let large_size = GIT_POINTER_THRESHOLD + 1;
    let large_stored = store
        .cas()
        .put_blob(
            NS_BLOB,
            &mut RepeatPatternReader {
                remaining: large_size,
                byte: 0x3C,
            },
        )
        .expect("store 100 MiB + 1 payload");
    assert_eq!(large_stored.size, large_size);

    let bin_tree = store
        .put_tree_cached(
            &Tree::from_entries(vec![TreeEntry::executable(
                b"tool.sh".to_vec(),
                &tool_id,
                tool_sz,
            )])
            .expect("bin"),
        )
        .expect("store bin");
    let base_tree = store
        .put_tree_cached(
            &Tree::from_entries(vec![
                TreeEntry::file(b"README.md".to_vec(), &readme_id, readme_sz),
                TreeEntry::dir(b"bin".to_vec(), &bin_tree),
            ])
            .expect("base tree"),
        )
        .expect("store base");

    let assets_tree = store
        .put_tree_cached(
            &Tree::from_entries(vec![TreeEntry::file(
                b"huge.bin".to_vec(),
                &large_stored.digest,
                large_stored.size,
            )])
            .expect("assets"),
        )
        .expect("store assets");
    let dataset_tree = store
        .put_tree_cached(
            &Tree::from_entries(vec![
                TreeEntry::file(b"README.md".to_vec(), &readme_id, readme_sz),
                TreeEntry::dir(b"assets".to_vec(), &assets_tree),
                TreeEntry::dir(b"bin".to_vec(), &bin_tree),
                TreeEntry::symlink(b"README.link".to_vec(), &link_id, link_sz),
            ])
            .expect("big tree"),
        )
        .expect("store big");

    let body1 = format_commit_body(
        "Alice <alice@spawn.local> 1700001000 +0000",
        "Alice <alice@spawn.local> 1700001000 +0000",
        "Bootstrap main",
    );
    let c1 = store
        .publish_snapshot(
            &ledger,
            &PublishRequest {
                project: "bundle-proj",
                ref_name: "main",
                kind: D6RefKind::Main,
                expected_ref: None,
                tree: &base_tree,
                parents: &[],
                commit_body: &body1,
                created_at_ms: 1_700_001_000_000,
                reason: "init",
                landing_task_id: None,
            },
        )
        .expect("c1");

    let body2 = format_commit_body(
        "Bob <bob@spawn.local> 1700001100 +0000",
        "Bob <bob@spawn.local> 1700001100 +0000",
        "Add >100 MiB local dataset\n\nSpawn-Task: 77\nSpawn-Head: 0123456789abcdef0123456789abcdef01234567\n",
    );
    let c2 = store
        .publish_snapshot(
            &ledger,
            &PublishRequest {
                project: "bundle-proj",
                ref_name: "feat/big",
                kind: D6RefKind::Branch,
                expected_ref: None,
                tree: &dataset_tree,
                parents: std::slice::from_ref(&c1.snapshot),
                commit_body: &body2,
                created_at_ms: 1_700_001_100_000,
                reason: "feat/big",
                landing_task_id: None,
            },
        )
        .expect("c2");

    // Point tag v1.0 cleanly to c2.
    let _tag_seq = ledger
        .cas_ref(
            "bundle-proj",
            "tags/v1.0",
            D6RefKind::Tag,
            None,
            &c2.snapshot,
            "tag v1.0",
        )
        .expect("tag v1.0");
    store
        .cas()
        .append_ref_journal(&JournalEntry {
            seq: c2.ref_seq + 1,
            project: "bundle-proj".to_string(),
            name: "tags/v1.0".to_string(),
            kind: "tag".to_string(),
            old_snapshot: None,
            new_snapshot: c2.snapshot.clone(),
            git_oid: c2.git_oid,
            at_ms: 1_700_001_150_000,
        })
        .expect("journal tag");

    // Land c2 onto main via accepted gate verdict.
    let land_body_preview = format_commit_body(
        "Gate <gate@spawn.local> 1700001300 +0000",
        "Gate <gate@spawn.local> 1700001300 +0000",
        "Merge feat/big\n\nSpawn-Task: 88\nSpawn-Head: deadbeefdeadbeefdeadbeefdeadbeefdeadbeef\n",
    );
    let cand_oid = store
        .project_snapshot_oid(
            &Snapshot {
                tree: dataset_tree.clone(),
                parents: vec![c1.snapshot.clone(), c2.snapshot.clone()],
                commit_body: land_body_preview.clone(),
            },
            &[c1.git_oid, c2.git_oid],
        )
        .expect("cand oid");
    let task_id = setup_accepted_task(&ledger.pg, "/tmp/bundle-proj.git", &c1.git_oid, &cand_oid);
    let c3 = store
        .publish_snapshot(
            &ledger,
            &PublishRequest {
                project: "bundle-proj",
                ref_name: "main",
                kind: D6RefKind::Main,
                expected_ref: Some(&c1.snapshot),
                tree: &dataset_tree,
                parents: &[c1.snapshot.clone(), c2.snapshot.clone()],
                commit_body: &land_body_preview,
                created_at_ms: 1_700_001_300_000,
                reason: "land big merge",
                landing_task_id: Some(task_id),
            },
        )
        .expect("c3");

    let bundle_path = scratch.join("r2-backup/bundle-proj.bundle");
    let drill =
        verify_bundle_snapshot_roundtrip(&store, &ledger, &adapter, "bundle-proj", &bundle_path)
            .expect("bundle drill with large local-only file");

    // Bundle contains only compact spawn-pointer-v1 objects, never the 100 MiB payload!
    assert!(drill.bundle_bytes > 0 && drill.bundle_bytes < 1_048_576);
    assert_eq!(drill.snapshots_verified, 3);
    assert_eq!(drill.refs_verified.len(), 3);
    assert_eq!(
        drill.refs_verified.get("refs/heads/main"),
        Some(&(c3.snapshot.clone(), c3.git_oid))
    );
    assert_eq!(
        drill.refs_verified.get("refs/heads/feat/big"),
        Some(&(c2.snapshot.clone(), c2.git_oid))
    );
    assert_eq!(
        drill.refs_verified.get("refs/tags/v1.0"),
        Some(&(c2.snapshot.clone(), c2.git_oid))
    );
    assert_eq!(drill.local_only_large_files.len(), 2);
    assert_eq!(
        drill.missing_local_only_large_files,
        drill.local_only_large_files
    );
    for entry in &drill.missing_local_only_large_files {
        assert_eq!(entry.path.shown(), "assets/huge.bin");
        assert_eq!(entry.digest, large_stored.digest);
        assert_eq!(entry.size, large_size);
    }

    // Project B: normal repository with zero > 100 MiB blobs.
    let body_clean = format_commit_body(
        "Alice <alice@spawn.local> 1700002000 +0000",
        "Alice <alice@spawn.local> 1700002000 +0000",
        "Clean bootstrap main",
    );
    let c_clean = store
        .publish_snapshot(
            &ledger,
            &PublishRequest {
                project: "clean-proj",
                ref_name: "main",
                kind: D6RefKind::Main,
                expected_ref: None,
                tree: &base_tree,
                parents: &[],
                commit_body: &body_clean,
                created_at_ms: 1_700_002_000_000,
                reason: "clean init",
                landing_task_id: None,
            },
        )
        .expect("clean main");
    let clean_bundle = scratch.join("r2-backup/clean-proj.bundle");
    let clean_drill =
        verify_bundle_snapshot_roundtrip(&store, &ledger, &adapter, "clean-proj", &clean_bundle)
            .expect("clean bundle drill");
    assert_eq!(clean_drill.snapshots_verified, 1);
    assert_eq!(
        clean_drill.refs_verified.get("refs/heads/main"),
        Some(&(c_clean.snapshot, c_clean.git_oid))
    );
    assert!(clean_drill.local_only_large_files.is_empty());
    assert!(clean_drill.missing_local_only_large_files.is_empty());
}

struct DevShmGuard(PathBuf);

impl DevShmGuard {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = PathBuf::from("/dev/shm").join(format!(
            "spawn-d6-step8-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create /dev/shm dir");
        Self(dir)
    }
}

impl Drop for DevShmGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
#[expect(clippy::too_many_lines)]
fn pg_reversible_cutover_d6_l1_and_border_roundtrip_drill() {
    let scratch = Scratch::new("reversible_drill");
    let adapter = GitBorderAdapter::new();

    let mut big_payload = vec![0x5Au8; 4_500_000];
    for (idx, byte) in big_payload.iter_mut().enumerate().step_by(4096) {
        *byte = u8::try_from(idx % 251).expect("byte");
    }

    let work = scratch.join("work");
    fs::create_dir_all(work.join("src")).expect("mkdir src");
    fs::create_dir_all(work.join("bin")).expect("mkdir bin");
    fs::create_dir_all(work.join("assets")).expect("mkdir assets");

    git_in(&work, &["init", "-b", "main"]);
    fs::write(work.join(".gitignore"), "*.local.log\nbuild_tmp/\n").expect("write gitignore");
    fs::write(work.join("README.md"), "# Reversible Cutover Drill\n").expect("write readme");
    fs::write(work.join("src/lib.rs"), "pub fn drill() -> u32 { 7 }\n").expect("write lib");
    fs::write(work.join("bin/check.sh"), "#!/bin/sh\nexit 0\n").expect("write script");
    fs::set_permissions(work.join("bin/check.sh"), fs::Permissions::from_mode(0o755))
        .expect("chmod script");
    std::os::unix::fs::symlink("README.md", work.join("README.link")).expect("symlink");
    fs::write(work.join("assets/model_weights.bin"), &big_payload).expect("write big bin");
    git_in(&work, &["add", "-A"]);
    git_in(&work, &["commit", "-m", "initial main"]);
    git_in(&work, &["tag", "v0.1.0"]);

    git_in(&work, &["checkout", "-b", "agent_alpha"]);
    fs::write(
        work.join("README.md"),
        "# Reversible Cutover Drill\nalpha work\n",
    )
    .expect("edit alpha readme");
    fs::write(work.join("alpha.txt"), "alpha branch\n").expect("write alpha");
    git_in(&work, &["add", "-A"]);
    git_in(&work, &["commit", "-m", "alpha work"]);

    git_in(&work, &["checkout", "-b", "agent_beta", "main"]);
    fs::write(work.join("beta.txt"), "beta branch\n").expect("write beta");
    git_in(&work, &["add", "-A"]);
    git_in(&work, &["commit", "-m", "beta work"]);
    git_in(&work, &["checkout", "main"]);

    let bare_repo = scratch.join("origin.git");
    git_in(
        &scratch.0,
        &[
            "clone",
            "--bare",
            work.to_str().expect("utf8"),
            bare_repo.to_str().expect("utf8"),
        ],
    );
    git_in(
        &work,
        &["remote", "add", "origin", bare_repo.to_str().expect("utf8")],
    );

    let branch_alpha = BranchName::new("agent_alpha").expect("branch_alpha");
    let branch_beta = BranchName::new("agent_beta").expect("branch_beta");

    git_in(&work, &["checkout", "main"]);
    fs::write(
        work.join("upstream_pre_cutover.txt"),
        "upstream main before cutover\n",
    )
    .expect("upstream pre");
    git_in(&work, &["add", "upstream_pre_cutover.txt"]);
    git_in(&work, &["commit", "-m", "upstream pre-cutover"]);
    git_in(&work, &["push", "origin", "main"]);

    // Stage 2: 7-step ProjectCutover to D6Cas on isolated production CAS + PG.
    let prod_cas = Arc::new(CasMerkleStore::open(scratch.join("prod-cas")).expect("prod cas"));
    let prod_pg = Arc::new(PgRefLedger::connect_isolated("drill_cutover_prod"));
    let cutover_ws_root = scratch.join("cutover-workspaces");
    fs::create_dir_all(&cutover_ws_root).expect("mkdir cutover root");
    let mut cutover = ProjectCutover::new("cutover-drill", &bare_repo, &cutover_ws_root);
    cutover
        .step1_backfill_history(&prod_cas, prod_pg.as_ref())
        .expect("step1");
    cutover
        .step2_pause_dispatch_and_verify_clean(&prod_cas)
        .expect("step2");
    cutover
        .step3_import_tail_commits(&prod_cas, prod_pg.as_ref())
        .expect("step3");
    cutover
        .step4_commit_refs_and_flip_mode(&prod_cas, prod_pg.as_ref(), &adapter, "cutover-drill")
        .expect("step4");
    cutover
        .step5_verify_parity(&prod_cas, prod_pg.as_ref())
        .expect("step5");
    cutover
        .step6_configure_readonly_border(&adapter)
        .expect("step6");
    cutover.step7_resume_dispatch().expect("step7");
    assert_eq!(cutover.storage_mode(), ProjectStorageMode::D6Cas);
    assert_eq!(cutover.phase(), CutoverPhase::Resumed);
    let main_ref = prod_pg
        .list_refs(Some("cutover-drill"))
        .expect("list cutover refs")
        .into_iter()
        .find(|r| r.name == "main")
        .expect("main ledger ref after cutover");
    let main_snap = prod_cas
        .cas()
        .read_merkle_snapshot(&main_ref.snapshot)
        .expect("read main snapshot");
    let main_flat = prod_cas
        .flatten_tree(&main_snap.tree)
        .expect("flatten main");
    let big_digest = main_flat
        .get(b"assets/model_weights.bin".as_slice())
        .expect("big asset in main tree")
        .digest()
        .expect("big digest");
    assert!(
        prod_cas.cas().chunk_index_path(&big_digest).is_file(),
        ">4 MiB asset must be chunked during backfill into prod CAS"
    );

    let push_while_locked = test_git_command()
        .current_dir(&work)
        .args(["push", "origin", "HEAD:refs/heads/bypass"])
        .output()
        .expect("push while locked");
    assert!(!push_while_locked.status.success());

    // Stage 3: Native LocalWorkspaceStore::D6 full lifecycle on L1 tmpfs (zero git subprocesses).
    let tmpfs_guard = DevShmGuard::new("cutover-drill");
    let prod_dyn: DynRefLedger = prod_pg.clone();
    let d6_opts = D6StoreOptions::new(
        prod_cas.clone(),
        prod_dyn,
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    )
    .with_l1_policy(L1TmpfsPolicy::new(&tmpfs_guard.0, 128));
    let d6_store = LocalWorkspaceStore::d6("cutover-drill", scratch.join("d6-disk"), d6_opts);
    assert_eq!(d6_store.backend_name(), "d6");

    let ws_alpha = d6_store.mount(&branch_alpha).expect("d6 mount alpha");
    let ws_alpha_dir = PathBuf::from(ws_alpha.path());
    assert!(ws_alpha_dir.starts_with(canonical_root(&tmpfs_guard.0)));
    assert!(!ws_alpha_dir.join(".git").exists());
    assert_eq!(
        fs::read(ws_alpha_dir.join("assets/model_weights.bin")).expect("read big asset"),
        big_payload
    );
    assert_eq!(
        fs::symlink_metadata(ws_alpha_dir.join("bin/check.sh"))
            .expect("script meta")
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert_eq!(
        fs::read_link(ws_alpha_dir.join("README.link")).expect("readlink"),
        Path::new("README.md")
    );

    fs::write(ws_alpha_dir.join("ignored.local.log"), "never tracked\n").expect("write ignored");
    fs::write(
        ws_alpha_dir.join("d6_edit.rs"),
        "pub fn after_cutover() -> bool { true }\n",
    )
    .expect("write d6_edit");
    assert!(matches!(
        d6_store
            .checkpoint(&ws_alpha, "native d6 edit")
            .expect("d6 checkpoint"),
        Commit::Created(_)
    ));
    assert_eq!(
        d6_store.checkpoint(&ws_alpha, "d6 noop").expect("d6 noop"),
        Commit::Unchanged
    );

    let d6_inner = d6_store.d6_workspace().expect("d6 inner");
    let alpha_after_ckpt = d6_inner
        .branch_snapshot("agent_alpha")
        .expect("alpha snap")
        .expect("present");
    let alpha_flat = prod_cas
        .flatten_tree(&alpha_after_ckpt.tree)
        .expect("flatten alpha");
    assert!(alpha_flat.contains_key(b"d6_edit.rs".as_slice()));
    assert!(!alpha_flat.contains_key(b"ignored.local.log".as_slice()));

    // Land a native commit on main via setup_accepted_task + land_main_snapshot.
    let main_now = d6_inner
        .main_snapshot()
        .expect("main snap")
        .expect("present");
    let (land_blob_id, land_blob_sz) = prod_cas
        .put_bytes_blob(b"landed natively in D6\n")
        .expect("land blob");
    let next_main_tree = prod_cas
        .apply_mutations(
            &main_now.tree,
            &[TreeMutation::Upsert {
                path: vec![b"LANDED_IN_D6.md".to_vec()],
                kind: TreeEntryKind::File,
                id: land_blob_id.as_bytes(),
                size: land_blob_sz,
            }],
        )
        .expect("next main tree");
    let land_draft = Snapshot {
        tree: next_main_tree.clone(),
        parents: vec![main_now.id.clone()],
        commit_body: format_commit_body(
            "D6 Gate <gate@spawn.local> 1700003000 +0000",
            "D6 Gate <gate@spawn.local> 1700003000 +0000",
            "Land native D6 update\n\nSpawn-Task: 1\n",
        ),
    };
    let cand_oid = prod_cas
        .project_snapshot_oid(&land_draft, &[main_now.git_oid])
        .expect("cand oid");
    let task_id = setup_accepted_task(
        &prod_pg.pg,
        bare_repo.to_str().expect("utf8"),
        &main_now.git_oid,
        &cand_oid,
    );
    let land_body = format_commit_body(
        "D6 Gate <gate@spawn.local> 1700003000 +0000",
        "D6 Gate <gate@spawn.local> 1700003000 +0000",
        &format!("Land native D6 update\n\nSpawn-Task: {task_id}\n"),
    );
    let next_main_snap = Snapshot {
        tree: next_main_tree.clone(),
        parents: vec![main_now.id.clone()],
        commit_body: land_body,
    };
    let next_main_digest = prod_cas
        .cas()
        .put_merkle_snapshot(&next_main_snap)
        .expect("put main snap");
    let next_main_git_oid = prod_cas
        .project_snapshot_oid(&next_main_snap, &[main_now.git_oid])
        .expect("next main oid");
    prod_pg
        .pg
        .record_snapshot(
            &next_main_digest.to_string(),
            &next_main_git_oid.to_hex(),
            &next_main_tree.to_string(),
            &[main_now.id.to_string()],
            "cutover-drill",
            1_700_003_000_000,
        )
        .expect("record next main");
    d6_inner
        .land_main_snapshot(
            Some(&main_now.id),
            &next_main_digest,
            Some(task_id),
            "drill land main",
        )
        .expect("land main snapshot");

    assert!(matches!(
        d6_store.merge_base(&ws_alpha).expect("d6 merge_base"),
        BaseMerge::Merged { .. }
    ));
    assert_eq!(
        fs::read_to_string(ws_alpha_dir.join("LANDED_IN_D6.md")).expect("landed file"),
        "landed natively in D6\n"
    );
    d6_store.unmount(&ws_alpha).expect("unmount alpha");
    assert_eq!(d6_store.d6_l1_reserved_bytes(), Some(0));

    let ws_beta = d6_store.mount(&branch_beta).expect("mount beta");
    fs::write(Path::new(ws_beta.path()).join("loose_beta.txt"), "loose\n").expect("loose beta");
    assert!(matches!(
        d6_store
            .set_aside_when(&branch_beta, None, "d6 pause beta", &|| Ok(()))
            .expect("set_aside beta"),
        SetAside::Created { .. }
    ));
    d6_store.unmount(&ws_beta).expect("unmount beta");
    assert!(!d6_store.is_mounted(&ws_beta));
    assert_eq!(d6_store.d6_l1_reserved_bytes(), Some(0));

    // Stage 4: Export via GitBorderAdapter into clean external bare Git repo + verify parity + fsck + rollback.
    let external_bare = scratch.join("external-export.git");
    let export = adapter
        .export_project(&prod_cas, prod_pg.as_ref(), "cutover-drill", &external_bare)
        .expect("export to external bare");
    assert_eq!(export.refs.len(), 4);

    let ext_parity =
        verify_cutover_parity(&prod_cas, prod_pg.as_ref(), "cutover-drill", &external_bare)
            .expect("verify external border parity");
    assert_eq!(ext_parity.refs.len(), 4);

    for (git_ref, entry) in &ext_parity.refs {
        let rev_commit = String::from_utf8(git_in(
            &scratch.0,
            &[
                "--git-dir",
                external_bare.to_str().expect("utf8"),
                "rev-parse",
                &format!("{git_ref}^{{commit}}"),
            ],
        ))
        .expect("utf8")
        .trim()
        .to_string();
        assert_eq!(
            rev_commit,
            entry.git_oid.to_hex(),
            "commit OID for {git_ref}"
        );

        let rev_tree = String::from_utf8(git_in(
            &scratch.0,
            &[
                "--git-dir",
                external_bare.to_str().expect("utf8"),
                "rev-parse",
                &format!("{git_ref}^{{tree}}"),
            ],
        ))
        .expect("utf8")
        .trim()
        .to_string();
        let snap = prod_cas
            .cas()
            .get_commit_object(&entry.snapshot)
            .expect("read snap");
        let expected_tree = prod_cas
            .project_tree_oid(snap.tree())
            .expect("project tree oid")
            .to_hex();
        assert_eq!(rev_tree, expected_tree, "tree OID for {git_ref}");
    }

    let verify_checkout = scratch.join("verify-checkout");
    git_in(
        &scratch.0,
        &[
            "clone",
            "--branch",
            "agent_alpha",
            external_bare.to_str().expect("utf8"),
            verify_checkout.to_str().expect("utf8"),
        ],
    );
    assert_eq!(
        fs::read(verify_checkout.join("assets/model_weights.bin")).expect("read ext big"),
        big_payload
    );
    assert_eq!(
        fs::read_to_string(verify_checkout.join("d6_edit.rs")).expect("read ext d6_edit"),
        "pub fn after_cutover() -> bool { true }\n"
    );
    assert_eq!(
        fs::read_to_string(verify_checkout.join("LANDED_IN_D6.md")).expect("read ext landed"),
        "landed natively in D6\n"
    );
    assert_eq!(
        fs::symlink_metadata(verify_checkout.join("bin/check.sh"))
            .expect("ext script meta")
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    assert_eq!(
        fs::read_link(verify_checkout.join("README.link")).expect("ext symlink"),
        Path::new("README.md")
    );
    assert!(!verify_checkout.join("ignored.local.log").exists());

    let fsck_ext = test_git_command()
        .arg("--git-dir")
        .arg(&external_bare)
        .args(["fsck", "--full", "--strict"])
        .output()
        .expect("ext fsck");
    assert!(
        fsck_ext.status.success(),
        "ext fsck stderr: {}",
        String::from_utf8_lossy(&fsck_ext.stderr)
    );

    // Lossless rollback restores bare_repo + writable push capability and passes fsck.
    let rollback = cutover
        .rollback_to_git(&prod_cas, prod_pg.as_ref(), &adapter)
        .expect("lossless rollback");
    assert_eq!(cutover.phase(), CutoverPhase::RolledBack);
    assert_eq!(cutover.storage_mode(), ProjectStorageMode::Git);
    assert_eq!(rollback.exported_refs, 4);

    let fsck_bare = test_git_command()
        .arg("--git-dir")
        .arg(&bare_repo)
        .args(["fsck", "--full", "--strict"])
        .output()
        .expect("bare fsck");
    assert!(
        fsck_bare.status.success(),
        "bare fsck stderr: {}",
        String::from_utf8_lossy(&fsck_bare.stderr)
    );

    git_in(&work, &["fetch", "origin", "main"]);
    git_in(&work, &["checkout", "-B", "post-rollback", "FETCH_HEAD"]);
    assert_eq!(
        fs::read_to_string(work.join("LANDED_IN_D6.md")).expect("landed after rollback"),
        "landed natively in D6\n"
    );
    fs::write(work.join("AFTER_ROLLBACK.txt"), "ok\n").expect("after rollback");
    git_in(&work, &["add", "AFTER_ROLLBACK.txt"]);
    git_in(&work, &["commit", "-m", "post rollback commit"]);
    git_in(&work, &["push", "origin", "HEAD:refs/heads/post-rollback"]);
}

fn summarize_us(samples_us: &[u64]) -> (f64, f64, f64) {
    if samples_us.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let mut sorted = samples_us.to_vec();
    sorted.sort_unstable();
    let n = sorted.len();
    let idx = |pct: usize| -> usize { ((n - 1) * pct) / 100 };
    let to_ms = |us: u64| f64::from(u32::try_from(us).unwrap_or(u32::MAX)) / 1000.0;
    (
        to_ms(sorted[idx(50)]),
        to_ms(sorted[idx(95)]),
        to_ms(sorted[n - 1]),
    )
}

fn seed_bare_repo_for_storm(scratch: &Scratch, tag: &str) -> PathBuf {
    let work = scratch.join(&format!("seed-work-{tag}"));
    fs::create_dir_all(work.join("src")).expect("mkdir src");
    git_in(&work, &["init", "-b", "main"]);
    fs::write(work.join(".gitignore"), "*.tmp.log\n").expect("gitignore");
    fs::write(work.join("README.md"), "# Chaos Lifecycle Repo\n").expect("readme");
    fs::write(work.join("src/lib.rs"), "pub fn v() -> u32 { 1 }\n").expect("lib");
    git_in(&work, &["add", "-A"]);
    git_in(&work, &["commit", "-m", "initial main"]);

    let bare = scratch.join(&format!("bare-{tag}.git"));
    git_in(
        &scratch.0,
        &[
            "clone",
            "--bare",
            work.to_str().expect("utf8"),
            bare.to_str().expect("utf8"),
        ],
    );
    bare
}

fn assert_clean_workspace_root(root: &Path) {
    if !root.exists() {
        return;
    }
    for entry in fs::read_dir(root).expect("read store root") {
        let entry = entry.expect("dir entry");
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        assert!(
            name_str.starts_with('.'),
            "orphan workspace entry left in {}: {name_str}",
            root.display()
        );
    }
    let layers = root.join(".d6/layers");
    if layers.is_dir() {
        for entry in fs::read_dir(&layers).expect("read layers") {
            let name = entry.expect("layer entry").file_name();
            let name_str = name.to_string_lossy();
            assert!(
                !name_str.starts_with(".d6-layer0-stage"),
                "leftover stage dir in {}: {name_str}",
                layers.display()
            );
        }
    }
}

fn assert_git_fsck_clean(bare_repo: &Path) {
    let fsck = test_git_command()
        .arg("--git-dir")
        .arg(bare_repo)
        .args(["fsck", "--full", "--strict"])
        .output()
        .expect("run git fsck");
    assert!(
        fsck.status.success(),
        "git fsck --full --strict failed on {}: {}",
        bare_repo.display(),
        String::from_utf8_lossy(&fsck.stderr)
    );
}

fn run_slot_worker_rounds(store: &LocalWorkspaceStore, slot_id: usize, rounds: usize) -> Vec<u64> {
    let branch = BranchName::new(format!("worker_slot_{slot_id}")).expect("branch");
    let mut cycle_us = Vec::with_capacity(rounds);
    for round in 0..rounds {
        let started = Instant::now();
        store.ensure_branch(&branch, None).expect("ensure_branch");
        let ws = store.mount_when(&branch, &|| Ok(())).expect("mount_when");
        let ws_dir = PathBuf::from(ws.path());
        fs::write(
            ws_dir.join(format!("slot_{slot_id}_r{round}.txt")),
            format!("slot={slot_id} round={round}\n"),
        )
        .expect("write slot file");
        assert!(matches!(
            store
                .checkpoint(&ws, &format!("slot {slot_id} round {round}"))
                .expect("checkpoint"),
            Commit::Created(_)
        ));
        fs::write(ws_dir.join(format!("dirty_{round}.txt")), "unsaved\n").expect("write dirty");
        store
            .set_aside_when(&branch, None, &format!("aside-{slot_id}-{round}"), &|| {
                Ok(())
            })
            .expect("set_aside_when");
        fs::write(
            ws_dir.join(format!("after_{slot_id}_r{round}.txt")),
            format!("after={slot_id} round={round}\n"),
        )
        .expect("write after file");
        assert!(matches!(
            store
                .checkpoint(&ws, &format!("post-aside {slot_id} round {round}"))
                .expect("post-aside checkpoint"),
            Commit::Created(_)
        ));
        if round % 2 == 0 {
            store.unmount_when(&ws, &|| Ok(())).expect("unmount_when");
        } else {
            store.discard(&ws).expect("discard");
        }
        assert!(!store.is_mounted(&ws));
        cycle_us.push(u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX));
    }
    cycle_us
}

fn run_destructive_contention_round(
    store: &LocalWorkspaceStore,
    thread_id: usize,
    rounds: usize,
) -> Vec<u64> {
    let shared = BranchName::new("worker_race_shared").expect("shared branch");
    let mut lat_us = Vec::with_capacity(rounds);
    for round in 0..rounds {
        let started = Instant::now();
        let _ = store.ensure_branch(&shared, None);
        if let Ok(ws) = store.mount_when(&shared, &|| Ok(())) {
            let ws_dir = PathBuf::from(ws.path());
            let _ = fs::write(
                ws_dir.join(format!("race_t{thread_id}_r{round}.txt")),
                format!("race {thread_id}-{round}\n"),
            );
            let _ = store.checkpoint(&ws, &format!("race {thread_id}-{round}"));
            let _ = store.set_aside_when(
                &shared,
                None,
                &format!("race-aside-{thread_id}-{round}"),
                &|| Ok(()),
            );
            if round % 2 == 0 {
                let _ = store.unmount(&ws);
            } else {
                let _ = store.discard(&ws);
            }
        }
        lat_us.push(u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX));
    }
    lat_us
}

fn run_store_storm_harness(
    store: &LocalWorkspaceStore,
    backend_label: &str,
) -> (f64, f64, f64, usize) {
    let sentinel = BranchName::new("worker_sentinel").expect("sentinel branch");
    store
        .ensure_branch(&sentinel, None)
        .expect("ensure sentinel");
    let sentinel_ws = store.mount(&sentinel).expect("mount sentinel");
    let sentinel_file = PathBuf::from(sentinel_ws.path()).join("sentinel_alive.txt");
    fs::write(&sentinel_file, "do-not-delete\n").expect("write sentinel");

    let threads = 8usize;
    let barrier = Arc::new(Barrier::new(threads));
    let samples = Arc::new(Mutex::new(Vec::new()));

    std::thread::scope(|scope| {
        for tid in 0..threads {
            let barrier = Arc::clone(&barrier);
            let samples = Arc::clone(&samples);
            scope.spawn(move || {
                barrier.wait();
                let local = if tid < 6 {
                    run_slot_worker_rounds(store, tid, 3)
                } else {
                    run_destructive_contention_round(store, tid, 4)
                };
                samples.lock().expect("samples").extend(local);
            });
        }
    });

    let shared = BranchName::new("worker_race_shared").expect("shared");
    if store.ensure_branch(&shared, None).is_ok()
        && let Ok(ws) = store.mount(&shared)
    {
        store.discard(&ws).expect("cleanup shared ws");
    }

    assert!(
        store.is_mounted(&sentinel_ws),
        "{backend_label}: sibling sentinel workspace was wrongly removed!"
    );
    assert_eq!(
        fs::read_to_string(&sentinel_file).expect("read sentinel"),
        "do-not-delete\n",
        "{backend_label}: sibling sentinel content corrupted!"
    );
    assert!(matches!(
        store
            .checkpoint(&sentinel_ws, "sentinel survives storm")
            .expect("sentinel ckpt"),
        Commit::Created(_)
    ));
    store.unmount(&sentinel_ws).expect("unmount sentinel");
    assert_clean_workspace_root(store.root());

    let collected = samples.lock().expect("samples").clone();
    let (p50, p95, max) = summarize_us(&collected);
    eprintln!(
        "N252_METRIC ws_lifecycle_storm backend={backend_label} threads={threads} cycles={} \
         p50_ms={p50:.2} p95_ms={p95:.2} max_ms={max:.2}",
        collected.len()
    );
    (p50, p95, max, collected.len())
}

#[test]
fn concurrent_workspace_lifecycle_races_git_and_d6_with_border_fsck() {
    let scratch = Scratch::new("ws_lifecycle_chaos");
    let adapter = GitBorderAdapter::new();

    // 1. Git backend concurrent storm.
    let git_bare = seed_bare_repo_for_storm(&scratch, "git");
    let git_store = LocalWorkspaceStore::git(
        &git_bare,
        scratch.join("ws-git"),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    );
    git_store.claim().expect("claim git");
    let _git_m = run_store_storm_harness(&git_store, "git");
    assert_git_fsck_clean(&git_bare);

    // 2. Pure D6 backend concurrent storm + .gc-fence + refs.journal + border git fsck.
    let d6_bare = seed_bare_repo_for_storm(&scratch, "d6");
    let d6_cas = Arc::new(CasMerkleStore::open(scratch.join("d6-cas")).expect("d6 cas"));
    let d6_ledger = Arc::new(PgRefLedger::connect_isolated("chaos_ws_d6"));
    let ident = d6_ledger.pg.database_identity().expect("db ident");
    let binding = GcFenceBinding {
        db_system_id: ident.db_system_id,
        db_oid: ident.db_oid,
        schema_name: ident.schema_name,
    };
    bind_store_gc_fence(&d6_cas, &binding).expect("bind gc fence");

    let cutover_root = scratch.join("d6-cutover-staging");
    fs::create_dir_all(&cutover_root).expect("mkdir cutover staging");
    let mut cutover = ProjectCutover::new("chaos-d6-proj", &d6_bare, &cutover_root);
    cutover
        .step1_backfill_history(&d6_cas, d6_ledger.as_ref())
        .expect("step1");
    cutover
        .step2_pause_dispatch_and_verify_clean(&d6_cas)
        .expect("step2");
    cutover
        .step3_import_tail_commits(&d6_cas, d6_ledger.as_ref())
        .expect("step3");
    cutover
        .step4_commit_refs_and_flip_mode(&d6_cas, d6_ledger.as_ref(), &adapter, "chaos")
        .expect("step4");
    cutover
        .step5_verify_parity(&d6_cas, d6_ledger.as_ref())
        .expect("step5");
    cutover
        .step6_configure_readonly_border(&adapter)
        .expect("step6");
    cutover.step7_resume_dispatch().expect("step7");

    let d6_dyn: DynRefLedger = d6_ledger.clone();
    let d6_opts = D6StoreOptions::new(d6_cas.clone(), d6_dyn, spawn_host::DEFAULT_MAX_FILE_BYTES);
    let d6_store = LocalWorkspaceStore::d6("chaos-d6-proj", scratch.join("ws-d6"), d6_opts);
    d6_store.claim().expect("claim d6");
    let _d6_m = run_store_storm_harness(&d6_store, "d6");

    let pg_seq = d6_ledger.pg.latest_ref_log_seq().expect("pg seq");
    let fence = check_gc_fence(&d6_cas, &binding, pg_seq).expect("gc fence intact");
    assert_eq!(fence.pg_latest_ref_seq, fence.journal_latest_seq);
    let rec = reconcile_refs_journal(&d6_cas, d6_ledger.as_ref(), pg_seq, "chaos")
        .expect("reconcile refs.journal clean");
    assert_eq!(rec.entries_replayed, 0);

    let exported = scratch.join("d6-exported-storm.git");
    adapter
        .export_project(&d6_cas, d6_ledger.as_ref(), "chaos-d6-proj", &exported)
        .expect("export storm project");
    verify_cutover_parity(&d6_cas, d6_ledger.as_ref(), "chaos-d6-proj", &exported)
        .expect("border parity");
    assert_git_fsck_clean(&exported);
}

#[test]
#[expect(clippy::too_many_lines)]
fn d6_l1_tmpfs_budget_exhaustion_backpressure_fallback_and_recovery() {
    let scratch = Scratch::new("d6_l1_backpressure");
    let tmpfs_guard = DevShmGuard::new("chaos_l1_budget");
    let cas = Arc::new(CasMerkleStore::open(scratch.join("cas")).expect("cas"));
    let ledger = Arc::new(PgRefLedger::connect_isolated("chaos_l1_budget"));
    let adapter = GitBorderAdapter::new();
    let disk_root = scratch.join("d6-disk");

    let gib = 1024_u64 * 1024 * 1024;
    let tmpfs_avail_bytes = Arc::new(AtomicU64::new(64 * 1024 * 1024));
    let avail_probe = tmpfs_avail_bytes.clone();

    let policy = L1TmpfsPolicy::new(&tmpfs_guard.0, 1)
        .with_per_workspace_quota_bytes(800 * 1024)
        .with_flush_threshold_pct(80);
    let d6 = D6Workspace::new(
        "l1-chaos-proj",
        &disk_root,
        cas.clone(),
        ledger.clone(),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    )
    .with_l1_policy(policy.clone())
    .with_l1_memory_probe(Arc::new(move |_| {
        Some(HostMemorySample {
            total_bytes: 512 * gib,
            available_bytes: 400 * gib,
            tmpfs_available_bytes: Some(avail_probe.load(Ordering::Relaxed)),
        })
    }));

    // Seed 600 KiB main snapshot in CAS + PG ledger.
    let seed_payload = vec![0x4B_u8; 600 * 1024];
    let (seed_id, seed_sz) = cas.put_bytes_blob(&seed_payload).expect("put seed");
    let seed_tree = cas
        .put_tree_cached(
            &Tree::from_entries(vec![TreeEntry::file(
                b"seed.bin".to_vec(),
                &seed_id,
                seed_sz,
            )])
            .expect("seed tree"),
        )
        .expect("store seed tree");
    let body = format_commit_body(
        "Alice <alice@spawn.local> 1700010000 +0000",
        "Alice <alice@spawn.local> 1700010000 +0000",
        "Seed L1 backpressure main",
    );
    cas.publish_snapshot(
        ledger.as_ref(),
        &PublishRequest {
            project: "l1-chaos-proj",
            ref_name: "main",
            kind: D6RefKind::Main,
            expected_ref: None,
            tree: &seed_tree,
            parents: &[],
            commit_body: &body,
            created_at_ms: 1_700_010_000_000,
            reason: "seed l1 main",
            landing_task_id: None,
        },
    )
    .expect("publish seed main");

    let ba = BranchName::new("agent_l1_a").expect("ba");
    let bb = BranchName::new("agent_l1_b").expect("bb");
    let bc = BranchName::new("agent_l1_c").expect("bc");
    let bd = BranchName::new("agent_l1_d").expect("bd");
    for b in [&ba, &bb, &bc, &bd] {
        d6.ensure_branch(b, None).expect("ensure");
    }

    // Phase 1: Admits branch A on tmpfs; branch B exceeds 1 MiB global budget -> clean disk fallback.
    let ws_a = d6.mount(&ba).expect("mount a");
    assert!(matches!(
        d6.l1_admission("agent_l1_a"),
        Some(L1Admission::AdmittedTmpfs { .. })
    ));
    let ws_a_dir = PathBuf::from(ws_a.path());
    assert!(ws_a_dir.starts_with(&policy.tmpfs_root));
    assert!(d6.l1_total_reserved_bytes() >= 600 * 1024);

    let fb_started = Instant::now();
    let ws_b = d6.mount(&bb).expect("mount b fallback");
    let fallback_ms =
        f64::from(u32::try_from(fb_started.elapsed().as_micros()).unwrap_or(u32::MAX)) / 1000.0;
    assert!(matches!(
        d6.l1_admission("agent_l1_b"),
        Some(L1Admission::FallbackDisk {
            reason: L1FallbackReason::BudgetExceeded { .. },
            ..
        })
    ));
    assert!(Path::new(ws_b.path()).starts_with(d6.root()));
    assert!(!policy.tmpfs_root.join("agent_l1_b").exists());
    fs::write(Path::new(ws_b.path()).join("fallback_ok.txt"), "disk ok\n").expect("write fallback");
    assert!(matches!(
        d6.checkpoint(&ws_b, "fallback ckpt")
            .expect("fallback ckpt"),
        Commit::Created(_)
    ));
    d6.unmount(&ws_b).expect("unmount b");

    // Phase 2: tmpfs free space drops below workspace size -> clean TmpfsFreeSpaceInsufficient fallback.
    tmpfs_avail_bytes.store(64 * 1024, Ordering::Relaxed);
    let ws_c = d6.mount(&bc).expect("mount c low tmpfs");
    assert!(matches!(
        d6.l1_admission("agent_l1_c"),
        Some(L1Admission::FallbackDisk {
            reason: L1FallbackReason::TmpfsFreeSpaceInsufficient { .. },
            ..
        })
    ));
    fs::write(Path::new(ws_c.path()).join("low_space.txt"), "safe disk\n").expect("write c");
    assert!(matches!(
        d6.checkpoint(&ws_c, "low space ckpt").expect("c ckpt"),
        Commit::Created(_)
    ));
    d6.unmount(&ws_c).expect("unmount c");

    // Phase 3: Active tmpfs branch exceeds 80% quota -> flush + disk demotion + zero L1 leftover.
    let demote_started = Instant::now();
    let pressure_bytes = vec![0x50_u8; 300 * 1024];
    fs::write(ws_a_dir.join("pressure_300k.bin"), &pressure_bytes).expect("write pressure");
    assert!(matches!(
        d6.l1_needs_flush(&ws_a).expect("needs flush"),
        Some(L1FlushReason::UsageThreshold { .. })
    ));
    let outcome = d6
        .flush_l1_if_needed(&ws_a, "pressure flush and demote")
        .expect("flush")
        .expect("triggered");
    let demote_ms =
        f64::from(u32::try_from(demote_started.elapsed().as_micros()).unwrap_or(u32::MAX)) / 1000.0;
    assert!(matches!(outcome.commit, Commit::Created(_)));
    assert!(outcome.reclaimed_bytes >= 900 * 1024);
    let demoted_ws = outcome.demoted_to_disk.expect("demoted to disk");
    assert!(!policy.tmpfs_root.join("agent_l1_a").exists());
    assert_eq!(d6.l1_total_reserved_bytes(), 0);
    assert_eq!(
        fs::read(Path::new(demoted_ws.path()).join("pressure_300k.bin")).expect("read demoted"),
        pressure_bytes
    );
    d6.unmount(&demoted_ws).expect("unmount demoted");

    // Phase 4: Space recovers -> branch D mounts onto tmpfs immediately, zero hanging locks.
    tmpfs_avail_bytes.store(64 * 1024 * 1024, Ordering::Relaxed);
    let rec_started = Instant::now();
    let ws_d = d6.mount(&bd).expect("mount d recovered");
    assert!(matches!(
        d6.l1_admission("agent_l1_d"),
        Some(L1Admission::AdmittedTmpfs { .. })
    ));
    fs::write(
        Path::new(ws_d.path()).join("recovered.txt"),
        "l1 restored\n",
    )
    .expect("write d");
    assert!(matches!(
        d6.checkpoint(&ws_d, "recovered l1").expect("d ckpt"),
        Commit::Created(_)
    ));
    d6.unmount(&ws_d).expect("unmount d");
    let recover_ms =
        f64::from(u32::try_from(rec_started.elapsed().as_micros()).unwrap_or(u32::MAX)) / 1000.0;
    assert_eq!(d6.l1_total_reserved_bytes(), 0);

    // Immediate re-mount confirms zero stuck lock files; border export passes git fsck --full --strict.
    let remount_d = d6.mount(&bd).expect("remount d cleanly");
    d6.discard(&remount_d).expect("discard remount d");
    assert_clean_workspace_root(&disk_root);

    let exported = scratch.join("l1-exported.git");
    adapter
        .export_project(&cas, ledger.as_ref(), "l1-chaos-proj", &exported)
        .expect("export l1 project");
    assert_git_fsck_clean(&exported);

    eprintln!(
        "N252_METRIC d6_l1_backpressure fallback_ms={fallback_ms:.2} demote_ms={demote_ms:.2} \
         recover_ms={recover_ms:.2} reclaimed_bytes={}",
        outcome.reclaimed_bytes
    );
}

struct RecutoverCtx<'a> {
    project: &'a str,
    bare_repo: &'a Path,
    ws_root: &'a Path,
    cas_store: &'a CasMerkleStore,
    ledger: &'a PgRefLedger,
    adapter: &'a GitBorderAdapter,
}

fn complete_cutover_steps(
    ctx: &RecutoverCtx<'_>,
    reason: &str,
) -> (ProjectCutover, spawn_host::CutoverParityReport) {
    let mut cutover = ProjectCutover::new(ctx.project, ctx.bare_repo, ctx.ws_root);
    cutover
        .step1_backfill_history(ctx.cas_store, ctx.ledger)
        .expect("step1 backfill");
    cutover
        .step2_pause_dispatch_and_verify_clean(ctx.cas_store)
        .expect("step2 clean");
    cutover
        .step3_import_tail_commits(ctx.cas_store, ctx.ledger)
        .expect("step3 tail");
    cutover
        .step4_commit_refs_and_flip_mode(ctx.cas_store, ctx.ledger, ctx.adapter, reason)
        .expect("step4 commit refs");
    let parity = cutover
        .step5_verify_parity(ctx.cas_store, ctx.ledger)
        .expect("step5 parity");
    cutover
        .step6_configure_readonly_border(ctx.adapter)
        .expect("step6 readonly");
    cutover.step7_resume_dispatch().expect("step7 resume");
    (cutover, parity)
}

fn publish_main_change_via_gate(
    ctx: &RecutoverCtx<'_>,
    base_snap_id: &Digest,
    base_oid: &GitOid,
    file_name: &str,
    content: &[u8],
    reason: &str,
) -> spawn_host::PublishedSnapshot {
    let base_snap = ctx
        .cas_store
        .cas()
        .get_commit_object(base_snap_id)
        .expect("read base snapshot");
    let (blob_id, blob_sz) = ctx.cas_store.put_bytes_blob(content).expect("put blob");
    let next_tree = ctx
        .cas_store
        .apply_mutations(
            base_snap.tree(),
            &[TreeMutation::Upsert {
                path: vec![file_name.as_bytes().to_vec()],
                kind: TreeEntryKind::File,
                id: blob_id.as_bytes(),
                size: blob_sz,
            }],
        )
        .expect("apply mutation");
    let commit_body = format_commit_body(
        "D6 Gate <gate@spawn.local> 1700002000 +0000",
        "D6 Gate <gate@spawn.local> 1700002000 +0000",
        reason,
    );
    let draft = Snapshot {
        tree: next_tree.clone(),
        parents: vec![base_snap_id.clone()],
        commit_body: commit_body.clone(),
    };
    let cand_oid = ctx
        .cas_store
        .project_snapshot_oid(&draft, &[*base_oid])
        .expect("project candidate oid");
    let task_id = setup_accepted_task(
        &ctx.ledger.pg,
        ctx.bare_repo.to_str().expect("utf8"),
        base_oid,
        &cand_oid,
    );
    ctx.cas_store
        .publish_snapshot(
            ctx.ledger,
            &PublishRequest {
                project: ctx.project,
                ref_name: "main",
                kind: D6RefKind::Main,
                expected_ref: Some(base_snap_id),
                tree: &next_tree,
                parents: std::slice::from_ref(base_snap_id),
                commit_body: &commit_body,
                created_at_ms: 1_700_002_000_000,
                reason,
                landing_task_id: Some(task_id),
            },
        )
        .expect("publish accepted main snapshot")
}

fn seed_recutover_repo(scratch: &Scratch) -> (PathBuf, PathBuf, PathBuf) {
    let work = scratch.join("work");
    let bare = scratch.join("origin.git");
    let ws_root = scratch.join("workspaces");
    fs::create_dir_all(&work).expect("mkdir work");
    fs::create_dir_all(&ws_root).expect("mkdir ws_root");

    git_in(&work, &["init", "-b", "main"]);
    fs::write(work.join("README.md"), "# Blocker 10 Test\n").expect("write readme");
    git_in(&work, &["add", "README.md"]);
    git_in(&work, &["commit", "-m", "initial main"]);

    for (branch, file) in [("feat/keep", "keep.txt"), ("feat/drop", "drop.txt")] {
        git_in(&work, &["checkout", "-b", branch, "main"]);
        fs::write(work.join(file), format!("{branch}\n")).expect("write branch file");
        git_in(&work, &["add", file]);
        git_in(&work, &["commit", "-m", branch]);
    }
    git_in(&work, &["checkout", "main"]);
    git_in(&work, &["tag", "v1.0"]);
    git_in(
        &scratch.0,
        &[
            "clone",
            "--bare",
            work.to_str().expect("utf8"),
            bare.to_str().expect("utf8"),
        ],
    );
    git_in(
        &work,
        &["remote", "add", "origin", bare.to_str().expect("utf8")],
    );
    (work, bare, ws_root)
}

fn advance_git_after_rollback(work: &Path) -> GitOid {
    git_in(work, &["fetch", "origin"]);
    git_in(work, &["reset", "--hard", "origin/main"]);
    assert_eq!(
        fs::read_to_string(work.join("PRE_ROLLBACK_D6.txt")).expect("read d6 commit"),
        "landed in d6 before rollback\n"
    );

    fs::write(work.join("POST_ROLLBACK_GIT_1.txt"), "git commit 1\n").expect("write git 1");
    git_in(work, &["add", "POST_ROLLBACK_GIT_1.txt"]);
    git_in(work, &["commit", "-m", "post-rollback main 1"]);

    fs::write(work.join("POST_ROLLBACK_GIT_2.txt"), "git commit 2\n").expect("write git 2");
    git_in(work, &["add", "POST_ROLLBACK_GIT_2.txt"]);
    git_in(work, &["commit", "-m", "post-rollback main 2"]);
    git_in(work, &["push", "origin", "main"]);

    git_in(work, &["checkout", "-b", "feat/post-rollback", "main"]);
    fs::write(work.join("post_branch.txt"), "after rollback\n").expect("write post branch");
    git_in(work, &["add", "post_branch.txt"]);
    git_in(work, &["commit", "-m", "post rollback branch"]);
    git_in(work, &["push", "origin", "feat/post-rollback"]);
    git_in(work, &["checkout", "main"]);
    git_in(work, &["tag", "v2.0"]);
    git_in(work, &["push", "origin", "refs/tags/v2.0"]);
    git_in(work, &["push", "origin", ":refs/heads/feat/drop"]);

    let rev = git_in(work, &["rev-parse", "HEAD"]);
    GitOid::parse_hex(String::from_utf8_lossy(&rev).trim()).expect("git oid")
}

fn verify_recutover_state_and_guard(
    ctx: &RecutoverCtx<'_>,
    binding: &GcFenceBinding,
    advanced_git_main: GitOid,
    stale_snap: &Digest,
) {
    let (_recutover, second_parity) = complete_cutover_steps(ctx, "re-cutover after git advance");
    assert_eq!(second_parity.refs.len(), 5);
    assert_eq!(
        second_parity.refs["refs/heads/main"].git_oid,
        advanced_git_main
    );
    assert!(!second_parity.refs.contains_key("refs/heads/feat/drop"));
    assert!(
        second_parity
            .refs
            .contains_key("refs/heads/feat/post-rollback")
    );
    assert!(second_parity.refs.contains_key("refs/tags/v2.0"));
    assert!(spawn_host::is_readonly_border(ctx.bare_repo).expect("readonly"));

    let verified = verify_cutover_parity(ctx.cas_store, ctx.ledger, ctx.project, ctx.bare_repo)
        .expect("verify parity");
    assert_eq!(verified.refs["refs/heads/main"].git_oid, advanced_git_main);
    let pg_seq = ctx.ledger.pg.latest_ref_log_seq().expect("pg seq");
    check_gc_fence(ctx.cas_store, binding, pg_seq).expect("gc fence ok");

    // Runtime main-ref gate guard must remain enforced after re-cutover.
    let re_main = &verified.refs["refs/heads/main"];
    assert!(
        ctx.ledger
            .land_main_ref(
                ctx.project,
                "main",
                Some(&re_main.snapshot),
                stale_snap,
                None,
                "rogue"
            )
            .is_err(),
        "advancing main without task_id must stay forbidden after re-cutover"
    );
    let post = publish_main_change_via_gate(
        ctx,
        &re_main.snapshot,
        &re_main.git_oid,
        "AFTER_RECUTOVER.txt",
        b"gated after re-cutover\n",
        "post-recutover d6 land",
    );
    assert_ne!(post.git_oid, re_main.git_oid);
}

#[test]
fn pg_cutover_rollback_git_advance_main_recutover_and_verify() {
    let scratch = Scratch::new("pg_recutover_b10");
    let cas_store = CasMerkleStore::open(scratch.join("cas")).expect("open cas");
    let ledger = PgRefLedger::connect_isolated("d6_b10_recutover");
    let adapter = GitBorderAdapter::new();
    let ident = ledger.pg.database_identity().expect("db identity");
    let binding = GcFenceBinding {
        db_system_id: ident.db_system_id,
        db_oid: ident.db_oid,
        schema_name: ident.schema_name,
    };
    bind_store_gc_fence(&cas_store, &binding).expect("bind gc fence");

    let (work, bare_repo, ws_root) = seed_recutover_repo(&scratch);
    let ctx = RecutoverCtx {
        project: "recutover-b10-proj",
        bare_repo: &bare_repo,
        ws_root: &ws_root,
        cas_store: &cas_store,
        ledger: &ledger,
        adapter: &adapter,
    };
    let unrelated_snap = Digest::parse(&format!("sha256:{}", "d".repeat(64))).unwrap();
    let unrelated_git_oid = GitOid::parse_hex(&"e".repeat(40)).unwrap();
    let unrelated_tree = Digest::parse(&format!("sha256:{}", "c".repeat(64))).unwrap();
    ledger
        .record_snapshot(
            &unrelated_snap,
            &unrelated_git_oid,
            &unrelated_tree,
            &[],
            "unrelated-proj",
            1,
        )
        .expect("unrelated snap");
    ledger
        .land_main_ref(
            "unrelated-proj",
            "main",
            None,
            &unrelated_snap,
            None,
            "init unrelated",
        )
        .expect("unrelated main");

    let (mut cutover, initial_parity) = complete_cutover_steps(&ctx, "initial cutover");
    let init_main = &initial_parity.refs["refs/heads/main"];
    let landed = publish_main_change_via_gate(
        &ctx,
        &init_main.snapshot,
        &init_main.git_oid,
        "PRE_ROLLBACK_D6.txt",
        b"landed in d6 before rollback\n",
        "pre-rollback d6 land",
    );
    assert!(
        ledger
            .land_main_ref(
                ctx.project,
                "main",
                Some(&landed.snapshot),
                &init_main.snapshot,
                None,
                "no task"
            )
            .is_err()
    );

    let rollback = cutover
        .rollback_to_git(&cas_store, &ledger, &adapter)
        .expect("rollback to git");
    assert_eq!(rollback.exported_refs, 4);
    assert!(
        ledger
            .list_refs(Some(ctx.project))
            .expect("retired")
            .is_empty()
    );
    assert_eq!(
        ledger
            .list_refs(Some("unrelated-proj"))
            .expect("unrelated")
            .len(),
        1
    );
    assert!(!spawn_host::is_readonly_border(&bare_repo).expect("writable"));

    let advanced_git_main = advance_git_after_rollback(&work);
    assert_git_fsck_clean(&bare_repo);
    verify_recutover_state_and_guard(&ctx, &binding, advanced_git_main, &init_main.snapshot);
}

struct SeededV2Cutover {
    t1: Digest,
    t2: Digest,
    b2: Digest,
    ts0: i64,
    p_main1: spawn_host::PublishedSnapshot,
    p_br: spawn_host::PublishedSnapshot,
    p_main2: spawn_host::PublishedSnapshot,
    live_task: i64,
    agent_actor: CommitActor,
}

fn land_v2_branch_onto_main(
    store: &CasMerkleStore,
    live_ledger: &PgRefLedger,
    p_main1: &spawn_host::PublishedSnapshot,
    p_br: &spawn_host::PublishedSnapshot,
    t2: &Digest,
    ts0: i64,
) -> (i64, spawn_host::PublishedSnapshot) {
    let land_actor = CommitActor::System {
        id: "d6-integrator".to_string(),
    };
    let land_intent = CommitIntent {
        origin: CommitOrigin::Land,
        task_id: Some(1),
        transcript_ref: Some("transcript://land-1".to_string()),
        message: "Land feat/v2 via structured CommitV2 task_id".to_string(),
    };
    let cand_oid = store
        .project_commit_object_oid(
            &CommitObject::V2(CommitV2 {
                tree: t2.clone(),
                parents: vec![p_main1.snapshot.clone(), p_br.snapshot.clone()],
                at_ms: ts0 + 2_000,
                actor: land_actor.clone(),
                intent: land_intent.clone(),
                git_compat_body: None,
            }),
            &[p_main1.git_oid, p_br.git_oid],
        )
        .expect("project land oid");
    let live_task = setup_accepted_task(
        &live_ledger.pg,
        "/tmp/v2-cut-proj.git",
        &p_main1.git_oid,
        &cand_oid,
    );
    assert_eq!(live_task, 1);

    let p_main2 = store
        .publish_commit_v2(
            live_ledger,
            &PublishCommitV2Request {
                project: "v2-cut-proj",
                ref_name: "main",
                kind: D6RefKind::Main,
                expected_ref: Some(&p_main1.snapshot),
                tree: t2,
                parents: &[p_main1.snapshot.clone(), p_br.snapshot.clone()],
                created_at_ms: ts0 + 2_000,
                actor: &land_actor,
                intent: &land_intent,
                git_compat_body: None,
                reason: "v2 land",
                landing_task_id: Some(live_task),
            },
        )
        .expect("pub v2 land");
    assert_eq!(p_main2.git_oid, cand_oid);
    (live_task, p_main2)
}

fn seed_v2_cutover_dag(
    store: &CasMerkleStore,
    live_ledger: &PgRefLedger,
    ts0: i64,
) -> SeededV2Cutover {
    let (b1, sz1) = store.put_bytes_blob(b"v1 root content\n").expect("b1");
    let (b2, sz2) = store.put_bytes_blob(b"v2 branch content\n").expect("b2");
    let t1 = store
        .put_tree_cached(
            &Tree::from_entries(vec![TreeEntry::file(b"root.txt".to_vec(), &b1, sz1)]).expect("t1"),
        )
        .expect("store t1");
    let t2 = store
        .put_tree_cached(
            &Tree::from_entries(vec![
                TreeEntry::file(b"root.txt".to_vec(), &b1, sz1),
                TreeEntry::file(b"v2.txt".to_vec(), &b2, sz2),
            ])
            .expect("t2"),
        )
        .expect("store t2");

    let v1_body = format_commit_body(
        "Alice <alice@spawn.local> 1700000100 +0000",
        "Alice <alice@spawn.local> 1700000100 +0000",
        "Legacy v1 root",
    );
    let p_main1 = store
        .publish_snapshot(
            live_ledger,
            &PublishRequest {
                project: "v2-cut-proj",
                ref_name: "main",
                kind: D6RefKind::Main,
                expected_ref: None,
                tree: &t1,
                parents: &[],
                commit_body: &v1_body,
                created_at_ms: ts0,
                reason: "v1 seed",
                landing_task_id: None,
            },
        )
        .expect("pub v1 main");

    let agent_actor = CommitActor::Agent {
        id: "v2-worker".to_string(),
        execution_id: Some("exec-v2-cut".to_string()),
        model_label: Some("claude-sonnet".to_string()),
        preset_id: Some("coder".to_string()),
    };
    let br_intent = CommitIntent {
        origin: CommitOrigin::Work {
            change: WorkChange::Edit,
            boundary: WorkBoundary::Submit,
        },
        task_id: Some(1),
        transcript_ref: Some("transcript://1".to_string()),
        message: "CommitV2 branch work (no text trailer)".to_string(),
    };
    let p_br = store
        .publish_commit_v2(
            live_ledger,
            &PublishCommitV2Request {
                project: "v2-cut-proj",
                ref_name: "feat/v2",
                kind: D6RefKind::Branch,
                expected_ref: None,
                tree: &t2,
                parents: std::slice::from_ref(&p_main1.snapshot),
                created_at_ms: ts0 + 1_000,
                actor: &agent_actor,
                intent: &br_intent,
                git_compat_body: None,
                reason: "v2 branch commit",
                landing_task_id: None,
            },
        )
        .expect("pub v2 branch");

    let (live_task, p_main2) =
        land_v2_branch_onto_main(store, live_ledger, &p_main1, &p_br, &t2, ts0);
    SeededV2Cutover {
        t1,
        t2,
        b2,
        ts0,
        p_main1,
        p_br,
        p_main2,
        live_task,
        agent_actor,
    }
}

fn verify_v2_orphan_gc(
    store: &CasMerkleStore,
    clock: &ManualClock,
    live_ledger: &PgRefLedger,
    live_binding: &GcFenceBinding,
    seeded: &SeededV2Cutover,
) {
    let (blob_orp, sz_orp) = store.put_bytes_blob(b"orphan v2 content\n").expect("b_orp");
    let tree_orp = store
        .put_tree_cached(
            &Tree::from_entries(vec![TreeEntry::file(
                b"orp.txt".to_vec(),
                &blob_orp,
                sz_orp,
            )])
            .expect("tree_orp"),
        )
        .expect("store tree_orp");
    let orphan_v2 = CommitV2 {
        tree: tree_orp.clone(),
        parents: vec![seeded.p_main1.snapshot.clone()],
        at_ms: seeded.ts0 + 3_000,
        actor: seeded.agent_actor.clone(),
        intent: CommitIntent {
            origin: CommitOrigin::Work {
                change: WorkChange::Edit,
                boundary: WorkBoundary::Checkpoint,
            },
            task_id: None,
            transcript_ref: None,
            message: "orphan v2 checkpoint".to_string(),
        },
        git_compat_body: None,
    };
    let orphan_digest = store.cas().put_commit_v2(&orphan_v2).expect("put orp v2");
    let orphan_git_oid = store
        .project_commit_object_oid(&CommitObject::V2(orphan_v2), &[seeded.p_main1.git_oid])
        .expect("orphan oid");
    store.cas().sync_store().expect("sync orp");
    live_ledger
        .record_snapshot(
            &orphan_digest,
            &orphan_git_oid,
            &tree_orp,
            std::slice::from_ref(&seeded.p_main1.snapshot),
            "v2-cut-proj",
            seeded.ts0 + 3_000,
        )
        .expect("record orphan v2");

    let r1 = run_persistent_gc_for_ledger(
        store,
        live_ledger,
        live_binding,
        live_ledger.pg.latest_ref_log_seq().expect("seq"),
        DEFAULT_PERSISTENT_GC_GRACE,
    )
    .expect("gc round 1");
    assert!(r1.deleted_objects.is_empty());
    clock.advance(Duration::from_hours(25));
    let r2 = run_persistent_gc_for_ledger(
        store,
        live_ledger,
        live_binding,
        live_ledger.pg.latest_ref_log_seq().expect("seq"),
        DEFAULT_PERSISTENT_GC_GRACE,
    )
    .expect("gc round 2");
    assert!(r2.deleted_objects.contains(&orphan_digest));
    assert!(r2.deleted_objects.contains(&tree_orp));
    assert!(r2.deleted_objects.contains(&blob_orp));
    assert!(store.cas().has(&seeded.p_main2.snapshot));
    assert!(store.cas().has(&seeded.p_br.snapshot));
    assert!(store.cas().has(&seeded.p_main1.snapshot));
    assert!(store.cas().has(&seeded.b2));
}

#[test]
fn pg_commit_v2_reconcile_bundle_roundtrip_and_persistent_gc() {
    let scratch = Scratch::new("pg_v2_cutover");
    let clock = ManualClock::new(SystemTime::now() + Duration::from_mins(1));
    let cas_root = scratch.join("cas");
    let store = clock.cas_store(&cas_root);
    let live_ledger = PgRefLedger::connect_isolated("d6_v2_cutover_live");
    let live_ident = live_ledger.pg.database_identity().expect("live ident");
    let live_binding = GcFenceBinding {
        db_system_id: live_ident.db_system_id,
        db_oid: live_ident.db_oid,
        schema_name: live_ident.schema_name,
    };
    bind_store_gc_fence(&store, &live_binding).expect("bind live fence");

    let seeded = seed_v2_cutover_dag(&store, &live_ledger, clock.now_ms());
    verify_v2_orphan_gc(&store, &clock, &live_ledger, &live_binding, &seeded);

    let adapter = GitBorderAdapter::new();
    let bundle_path = scratch.join("v2-mixed.bundle");
    let drill = verify_bundle_snapshot_roundtrip(
        &store,
        &live_ledger,
        &adapter,
        "v2-cut-proj",
        &bundle_path,
    )
    .expect("v2 bundle roundtrip");
    assert_eq!(drill.snapshots_verified, 3);
    assert_eq!(drill.refs_verified.len(), 2);
    assert!(store.read_tree_cached(&seeded.t2).is_ok());

    let backup_ledger = PgRefLedger::connect_isolated("d6_v2_cutover_backup");
    let backup_task = setup_accepted_task(
        &backup_ledger.pg,
        "/tmp/v2-cut-proj.git",
        &seeded.p_main1.git_oid,
        &seeded.p_main2.git_oid,
    );
    assert_eq!(backup_task, seeded.live_task);
    backup_ledger
        .record_snapshot(
            &seeded.p_main1.snapshot,
            &seeded.p_main1.git_oid,
            &seeded.t1,
            &[],
            "v2-cut-proj",
            seeded.ts0,
        )
        .expect("backup seed snap");
    let seq1 = backup_ledger
        .land_main_ref(
            "v2-cut-proj",
            "main",
            None,
            &seeded.p_main1.snapshot,
            None,
            "seed",
        )
        .expect("backup seed main");
    assert_eq!(seq1, 1);

    let rep = reconcile_refs_journal(&store, &backup_ledger, seq1, "v2_replay")
        .expect("reconcile v2 journal");
    assert_eq!(rep.entries_replayed, 2);
    assert_eq!(
        backup_ledger
            .list_refs(Some("v2-cut-proj"))
            .expect("list backup refs")
            .into_iter()
            .map(|r| (r.name, r.snapshot))
            .collect::<HashMap<_, _>>(),
        HashMap::from([
            ("main".to_string(), seeded.p_main2.snapshot),
            ("feat/v2".to_string(), seeded.p_br.snapshot),
        ])
    );
}
