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
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde_json::json;
use spawn_cas::{
    DEFAULT_PERSISTENT_GC_GRACE, Digest, GIT_POINTER_THRESHOLD, GitOid, JournalEntry, NS_BLOB,
    Snapshot, Store, Tree, TreeEntry,
};
use spawn_host::{
    CasMerkleStore, CutoverPhase, D6RefKind, D6StoreError, ExistingDestPolicy, GitBorderAdapter,
    Layer0Options, LedgerRef, LedgerSnapshot, ProjectCutover, ProjectStorageMode, PublishRequest,
    RefLedger, assert_zero_pending_merges, audit_export_attributes, bind_store_gc_fence,
    check_gc_fence, format_commit_body, materialize_layer0, materialize_workspace_from_tree,
    reconcile_refs_journal, run_persistent_gc_for_ledger, test_git_command,
    verify_bundle_snapshot_roundtrip, verify_cutover_parity, verify_zero_pending_merges,
};
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

    // Step 2 must fail closed while .merge/feat/alpha exists, leaving phase unchanged.
    let blocked = cutover.step2_pause_dispatch_and_verify_clean(&cas_store);
    assert!(matches!(blocked, Err(D6StoreError::InvalidInput(_))));
    assert_eq!(cutover.phase(), CutoverPhase::BackfillImported);

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
        .read_merkle_snapshot(&main_ref.snapshot)
        .expect("read main snap");
    let (post_id, post_sz) = cas_store
        .put_bytes_blob(b"post-cutover D6 file\n")
        .expect("put post file");
    let post_tree = cas_store
        .apply_mutations(
            &main_snap.tree,
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
        let snap_obj = store.cas().read_merkle_snapshot(snap_id).expect("src snap");
        let put_snap = rec_store
            .cas()
            .put_merkle_snapshot(&snap_obj)
            .expect("put rec snap");
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
