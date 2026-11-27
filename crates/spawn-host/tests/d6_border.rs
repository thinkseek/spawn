//! D6 Phase 4 Step 2 integration suite (`#113` Fine Items 7.1–7.3 & Blind-Spot
//! Safeguards #1, #2, #4):
//! - Fail-closed tracked-set scanner (`.gitignore`, `.git/info/exclude`,
//!   `BUILD_OUTPUT`, `ENGINE_RUNTIME_FILES`)
//! - `CasMerkleStore` `O(|diff| + depth)` mutations, `sync_store`, live PG 16
//!   `spawn_snapshots`/`spawn_refs`/`spawn_ref_log`, `refs.journal` & `.gc-fence`
//! - Layer 0 materialization without `0444` CAS hard-links
//! - `GitBorderAdapter` bidirectional `git fast-import` export & bare-repo
//!   history import (`0644`, `0755`, `120000`, `160000`, `> 100 MiB` `spawn-pointer-v1`,
//!   `Spawn-Head` trailers, custom commit headers, `git fsck --full --strict`)

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::json;
use spawn_cas::{
    Digest, GIT_POINTER_THRESHOLD, GitOid, NS_BLOB, Snapshot, Tree, TreeEntry, TreeEntryKind,
    bind_gc_fence, empty_tree_digest, git_pointer_bytes, verify_gc_fence,
};
use spawn_host::{
    CasMerkleStore, D6RefKind, D6StoreError, GitBorderAdapter, InMemoryRefLedger, LedgerRef,
    LedgerSnapshot, PublishRequest, RefLedger, TrackedSetPolicy, format_commit_body,
    test_git_command,
};
use spawn_postgres::testing::TestSchema;
use spawn_postgres::{PostgresStore, RefKind};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "spawn-host-d6-{tag}-{}-{}",
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
        .mission_add("D6 border landing", &json!([source]), "human")
        .expect("mission_add");
    store
        .mission_configure(mission, Some("/scenarios"), "gate", None, None)
        .expect("mission_configure");
    let task = store
        .task_assign(
            mission,
            "ephemeral",
            "w_d6_step2",
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

fn git_bare(dir: &Path, args: &[&str]) -> String {
    let mut cmd = test_git_command();
    let out = cmd
        .arg("--git-dir")
        .arg(dir)
        .args(args)
        .output()
        .expect("run git bare");
    assert!(
        out.status.success(),
        "git --git-dir {} {:?} failed: {}",
        dir.display(),
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
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

#[test]
#[expect(clippy::too_many_lines)]
fn tracked_set_scanner_is_fail_closed_and_matches_gitignore_and_runtime_exclusions() {
    let scratch = Scratch::new("scanner");
    let cas_store = CasMerkleStore::open(scratch.join("cas")).expect("open cas");
    let ws = scratch.join("ws");
    fs::create_dir_all(ws.join(".git/info")).expect("mkdir .git/info");
    fs::create_dir_all(ws.join("sub")).expect("mkdir sub");
    fs::create_dir_all(ws.join("ignored_dir/nested")).expect("mkdir ignored_dir");
    fs::create_dir_all(ws.join("target/debug")).expect("mkdir target");
    fs::create_dir_all(ws.join(".spawn")).expect("mkdir .spawn");
    fs::create_dir_all(ws.join(".claude/agents")).expect("mkdir .claude/agents");
    fs::create_dir_all(ws.join(".claude/worktrees/w1")).expect("mkdir .claude/worktrees");

    fs::write(
        ws.join(".gitignore"),
        "*.log\n!keep.log\n/rootonly.txt\nignored_dir/\n.env\n",
    )
    .expect("write root gitignore");
    fs::write(ws.join(".git/info/exclude"), "local.priv\n").expect("write info/exclude");
    fs::write(ws.join("sub/.gitignore"), "*.secret\n").expect("write sub gitignore");

    // Files expected to be excluded:
    fs::write(ws.join(".env"), "SECRET=1\n").expect("write .env");
    fs::write(ws.join("app.log"), "noise\n").expect("write app.log");
    fs::write(ws.join("rootonly.txt"), "root ignored\n").expect("write rootonly");
    fs::write(ws.join("ignored_dir/nested/x.txt"), "deep ignored\n").expect("write ignored_dir");
    fs::write(ws.join("local.priv"), "private\n").expect("write local.priv");
    fs::write(ws.join("sub/key.secret"), "secret\n").expect("write sub secret");
    fs::write(ws.join("target/debug/bin"), "bin\n").expect("write target");
    fs::write(ws.join(".spawn/state"), "tmp\n").expect("write .spawn");
    fs::write(ws.join(".claude/settings.local.json"), "{}\n").expect("write runtime settings");
    fs::write(ws.join(".claude/worktrees/w1/tmp.txt"), "rt\n").expect("write runtime worktree");

    // Files expected to be admitted:
    fs::write(ws.join("keep.log"), "keep me\n").expect("write keep.log");
    fs::write(ws.join("sub/rootonly.txt"), "nested ok\n").expect("write sub rootonly");
    fs::write(ws.join("sub/main.rs"), "fn main() {}\n").expect("write main.rs");
    fs::write(ws.join(".claude/agents/reviewer.md"), "# reviewer\n").expect("write agent md");

    let empty = empty_tree_digest();
    let policy = TrackedSetPolicy {
        base_tree: Some(&empty),
        main_tree: None,
        extra_exclude_files: Vec::new(),
    };
    let root_tree = cas_store
        .snapshot_workspace(&ws, &policy)
        .expect("snapshot clean workspace");
    let flat = cas_store.flatten_tree(&root_tree).expect("flatten");
    let paths: Vec<String> = flat
        .keys()
        .map(|k| String::from_utf8_lossy(k).into_owned())
        .collect();
    assert_eq!(
        paths,
        vec![
            ".claude/agents/reviewer.md",
            ".gitignore",
            "keep.log",
            "sub/.gitignore",
            "sub/main.rs",
            "sub/rootonly.txt",
        ]
    );

    // Fail-closed leak guard: manually injecting `.env` into candidate tree triggers IgnoredLeak.
    let (env_digest, env_size) = cas_store
        .put_bytes_blob(b"SECRET=1\n")
        .expect("put .env blob");
    let leaked_tree = cas_store
        .apply_mutations(
            &root_tree,
            &[spawn_cas::TreeMutation::Upsert {
                path: vec![b".env".to_vec()],
                kind: TreeEntryKind::File,
                id: env_digest.as_bytes(),
                size: env_size,
            }],
        )
        .expect("inject .env");
    let err = cas_store
        .verify_no_ignored_untracked_leaks(&ws, &leaked_tree, &policy)
        .expect_err("must reject untracked ignored .env leak");
    assert!(matches!(err, D6StoreError::IgnoredLeak { ref path, .. } if path == ".env"));

    // Runtime file tracked in BOTH `base_tree` and `main_tree` stays tracked, whereas tracked in
    // only one is dropped.
    let (rt_digest, rt_size) = cas_store.put_bytes_blob(b"{}\n").expect("put rt blob");
    let tracked_rt_tree = cas_store
        .apply_mutations(
            &root_tree,
            &[spawn_cas::TreeMutation::Upsert {
                path: vec![b".claude".to_vec(), b"settings.local.json".to_vec()],
                kind: TreeEntryKind::File,
                id: rt_digest.as_bytes(),
                size: rt_size,
            }],
        )
        .expect("rt tree");
    let both_policy = TrackedSetPolicy {
        base_tree: Some(&tracked_rt_tree),
        main_tree: Some(&tracked_rt_tree),
        extra_exclude_files: Vec::new(),
    };
    let kept_tree = cas_store
        .snapshot_workspace(&ws, &both_policy)
        .expect("keep tracked runtime");
    assert!(
        cas_store
            .flatten_tree(&kept_tree)
            .expect("flatten")
            .contains_key(b".claude/settings.local.json".as_slice())
    );

    // Fail-closed malformed `.gitignore` (`[unclosed`) refuses scanning.
    let bad_ws = scratch.join("bad_ws");
    fs::create_dir_all(&bad_ws).expect("mkdir bad_ws");
    fs::write(bad_ws.join(".gitignore"), "[unclosed\n").expect("write malformed gitignore");
    let bad_err = cas_store
        .scan_workspace_tree(&bad_ws, &TrackedSetPolicy::default())
        .expect_err("malformed glob must fail closed");
    assert!(matches!(bad_err, D6StoreError::InvalidInput(_)));

    // Fail-closed symlink `.gitignore` refuses scanning.
    let sym_ws = scratch.join("sym_ws");
    fs::create_dir_all(&sym_ws).expect("mkdir sym_ws");
    fs::write(scratch.join("real_ig"), "*.tmp\n").expect("write real_ig");
    symlink(scratch.join("real_ig"), sym_ws.join(".gitignore")).expect("symlink gitignore");
    let sym_err = cas_store
        .scan_workspace_tree(&sym_ws, &TrackedSetPolicy::default())
        .expect_err("symlink gitignore must fail closed");
    assert!(matches!(sym_err, D6StoreError::InvalidInput(_)));
}

#[test]
#[expect(clippy::too_many_lines)]
fn merkle_mutations_and_atomic_publish_roundtrip_with_live_pg16_and_gc_fence() {
    let scratch = Scratch::new("pg_publish");
    let cas_store = CasMerkleStore::open(scratch.join("cas")).expect("open cas");
    let ledger = PgRefLedger::connect_isolated("d6_step2_pub");
    let identity = ledger.pg.database_identity().expect("db identity");
    bind_gc_fence(
        cas_store.cas(),
        &spawn_cas::GcFenceBinding {
            db_system_id: identity.db_system_id,
            db_oid: identity.db_oid,
            schema_name: identity.schema_name,
        },
    )
    .expect("bind gc fence");

    let ws = scratch.join("ws");
    fs::create_dir_all(ws.join("alpha/deep")).expect("mkdir alpha");
    fs::create_dir_all(ws.join("beta")).expect("mkdir beta");
    fs::write(ws.join("alpha/deep/a.txt"), "hello v1\n").expect("write a.txt");
    fs::write(ws.join("beta/b.txt"), "unchanged sibling\n").expect("write b.txt");

    let empty = empty_tree_digest();
    let policy1 = TrackedSetPolicy {
        base_tree: Some(&empty),
        ..Default::default()
    };
    let tree1 = cas_store
        .snapshot_workspace(&ws, &policy1)
        .expect("snap ws 1");
    let beta_entry_1 = cas_store
        .read_tree_cached(&tree1)
        .expect("read tree1")
        .entries
        .into_iter()
        .find(|e| e.name == b"beta")
        .expect("beta subtree");

    let body1 = format_commit_body(
        "Alice <alice@spawn.local> 1700000100 +0000",
        "Alice <alice@spawn.local> 1700000100 +0000",
        "Initial main snapshot\n",
    );
    let pub1 = cas_store
        .publish_snapshot(
            &ledger,
            &PublishRequest {
                project: "proj-step2",
                ref_name: "main",
                kind: D6RefKind::Main,
                expected_ref: None,
                tree: &tree1,
                parents: &[],
                commit_body: &body1,
                created_at_ms: 1_700_000_100_000,
                reason: "bootstrap main",
                landing_task_id: None,
            },
        )
        .expect("publish main v1");
    assert_eq!(pub1.ref_seq, 1);

    // Edit only alpha/deep/a.txt; beta subtree digest must remain identical.
    fs::write(ws.join("alpha/deep/a.txt"), "hello v2\n").expect("edit a.txt");
    let policy2 = TrackedSetPolicy {
        base_tree: Some(&tree1),
        main_tree: Some(&tree1),
        ..Default::default()
    };
    let mutations = cas_store
        .diff_workspace_mutations(&ws, &policy2)
        .expect("diff mutations");
    assert_eq!(mutations.len(), 1);
    let tree2 = cas_store
        .snapshot_workspace(&ws, &policy2)
        .expect("snap ws 2");
    let beta_entry_2 = cas_store
        .read_tree_cached(&tree2)
        .expect("read tree2")
        .entries
        .into_iter()
        .find(|e| e.name == b"beta")
        .expect("beta subtree");
    assert_eq!(beta_entry_1.id, beta_entry_2.id);

    let body2 = format_commit_body(
        "Bob <bob@spawn.local> 1700000200 +0000",
        "Bob <bob@spawn.local> 1700000200 +0000",
        "Task work\n\nSpawn-Task: 77\n",
    );
    let pub2 = cas_store
        .publish_snapshot(
            &ledger,
            &PublishRequest {
                project: "proj-step2",
                ref_name: "feat/work",
                kind: D6RefKind::Branch,
                expected_ref: None,
                tree: &tree2,
                parents: std::slice::from_ref(&pub1.snapshot),
                commit_body: &body2,
                created_at_ms: 1_700_000_200_000,
                reason: "checkpoint branch",
                landing_task_id: None,
            },
        )
        .expect("publish branch v2");
    assert_eq!(pub2.ref_seq, 2);

    // Advance main through real mission_tasks accepted task.gated verdict.
    let body3 = format_commit_body(
        "Integrator <int@spawn.local> 1700000300 +0000",
        "Integrator <int@spawn.local> 1700000300 +0000",
        "Land feat/work\n\nSpawn-Task: 1\n",
    );
    // Re-publish same tree on landed commit for main advance
    let snap3_candidate = Snapshot {
        tree: tree2.clone(),
        parents: vec![pub1.snapshot.clone(), pub2.snapshot.clone()],
        commit_body: body3.clone(),
    };
    let oid3 = cas_store
        .project_snapshot_oid(&snap3_candidate, &[pub1.git_oid, pub2.git_oid])
        .expect("project merge oid");
    let task_id = setup_accepted_task(&ledger.pg, "/tmp/proj-step2-land.git", &pub1.git_oid, &oid3);
    let pub3 = cas_store
        .publish_snapshot(
            &ledger,
            &PublishRequest {
                project: "proj-step2",
                ref_name: "main",
                kind: D6RefKind::Main,
                expected_ref: Some(&pub1.snapshot),
                tree: &tree2,
                parents: &[pub1.snapshot.clone(), pub2.snapshot.clone()],
                commit_body: &body3,
                created_at_ms: 1_700_000_300_000,
                reason: &format!("land task {task_id}"),
                landing_task_id: Some(task_id),
            },
        )
        .expect("land main v3");
    assert_eq!(pub3.ref_seq, 3);
    assert_eq!(pub3.git_oid, oid3);

    // Verify PG + refs.journal + gc_fence lockstep agreement.
    let pg_latest = ledger.pg.latest_ref_log_seq().expect("pg seq");
    assert_eq!(pg_latest, 3);
    let journal = cas_store.cas().read_ref_journal().expect("read journal");
    assert_eq!(journal.len(), 3);
    assert_eq!(journal[2].seq, 3);
    assert_eq!(journal[2].git_oid, pub3.git_oid);
    let identity_check = ledger.pg.database_identity().expect("db id 2");
    let journal_latest = cas_store.cas().latest_seq().expect("journal seq");
    assert_eq!(journal_latest, 3);
    verify_gc_fence(
        cas_store.cas(),
        &spawn_cas::GcFenceBinding {
            db_system_id: identity_check.db_system_id,
            db_oid: identity_check.db_oid,
            schema_name: identity_check.schema_name,
        },
        pg_latest,
        journal_latest,
    )
    .expect("gc fence passes");

    // Verify dir -> file (`beta/` -> `beta`) and file -> dir (`beta` -> `beta/new.txt`) roundtrip.
    fs::remove_dir_all(ws.join("beta")).expect("remove beta dir");
    fs::write(ws.join("beta"), "beta became a file\n").expect("write beta file");
    let policy_dir_to_file = TrackedSetPolicy {
        base_tree: Some(&tree2),
        main_tree: Some(&tree1),
        ..Default::default()
    };
    let tree_beta_file = cas_store
        .snapshot_workspace(&ws, &policy_dir_to_file)
        .expect("dir -> file snapshot");
    let flat_dir_to_file = cas_store
        .flatten_tree(&tree_beta_file)
        .expect("flatten dir->file");
    assert!(flat_dir_to_file.contains_key(b"beta".as_slice()));
    assert!(!flat_dir_to_file.contains_key(b"beta/b.txt".as_slice()));

    fs::remove_file(ws.join("beta")).expect("remove beta file");
    fs::create_dir_all(ws.join("beta")).expect("recreate beta dir");
    fs::write(ws.join("beta/new.txt"), "beta became a dir\n").expect("write beta/new.txt");
    let policy_file_to_dir = TrackedSetPolicy {
        base_tree: Some(&tree_beta_file),
        main_tree: Some(&tree1),
        ..Default::default()
    };
    let tree_beta_dir = cas_store
        .snapshot_workspace(&ws, &policy_file_to_dir)
        .expect("file -> dir snapshot");
    let flat_file_to_dir = cas_store
        .flatten_tree(&tree_beta_dir)
        .expect("flatten file->dir");
    assert!(!flat_file_to_dir.contains_key(b"beta".as_slice()));
    assert!(flat_file_to_dir.contains_key(b"beta/new.txt".as_slice()));
}

#[test]
fn layer0_materialization_preserves_modes_reuses_prev_layer_and_never_hardlinks_cas() {
    let scratch = Scratch::new("materialize");
    let cas_store = CasMerkleStore::open(scratch.join("cas")).expect("open cas");

    let (d_plain, sz_plain) = cas_store.put_bytes_blob(b"plain text\n").expect("plain");
    let (d_exec, sz_exec) = cas_store
        .put_bytes_blob(b"#!/bin/sh\nexit 0\n")
        .expect("exec");
    let (d_sym, sz_sym) = cas_store.put_bytes_blob(b"plain.txt").expect("sym");
    let sub_oid = [0x42_u8; 20];

    let tree1 = cas_store
        .put_tree_cached(
            &Tree::from_entries(vec![
                TreeEntry::file(b"plain.txt".to_vec(), &d_plain, sz_plain),
                TreeEntry::executable(b"run.sh".to_vec(), &d_exec, sz_exec),
                TreeEntry::symlink(b"link.txt".to_vec(), &d_sym, sz_sym),
                TreeEntry::gitlink(b"submod".to_vec(), &sub_oid).expect("gitlink"),
            ])
            .expect("tree1"),
        )
        .expect("put tree1");

    let layer1 = scratch.join("layer1");
    let rep1 = cas_store
        .materialize_tree(&tree1, &layer1, None, None)
        .expect("materialize layer1");
    assert_eq!(rep1.files_written, 2);
    assert_eq!(rep1.hardlinked_from_prev, 0);
    assert_eq!(rep1.symlinks_written, 1);
    assert_eq!(rep1.gitlinks_skipped, 1);

    // Check modes and NEVER hardlinked against 0444 CAS objects.
    let m_plain1 = fs::symlink_metadata(layer1.join("plain.txt")).expect("stat plain");
    let m_exec1 = fs::symlink_metadata(layer1.join("run.sh")).expect("stat exec");
    let m_cas_plain =
        fs::symlink_metadata(cas_store.cas().object_path(&d_plain)).expect("stat cas");
    let m_cas_exec = fs::symlink_metadata(cas_store.cas().object_path(&d_exec)).expect("stat cas");
    assert_eq!(m_plain1.permissions().mode() & 0o777, 0o644);
    assert_eq!(m_exec1.permissions().mode() & 0o777, 0o755);
    assert_ne!(m_plain1.ino(), m_cas_plain.ino());
    assert_ne!(m_exec1.ino(), m_cas_exec.ino());
    assert_eq!(m_cas_plain.nlink(), 1);
    assert_eq!(
        fs::read_link(layer1.join("link.txt")).expect("readlink"),
        Path::new("plain.txt")
    );
    assert!(
        fs::symlink_metadata(layer1.join("submod"))
            .expect("submod")
            .is_dir()
    );

    // Materialize layer2 updating only plain.txt: run.sh hardlinks from layer1, never CAS!
    let (d_plain2, sz_plain2) = cas_store.put_bytes_blob(b"updated text\n").expect("plain2");
    let tree2 = cas_store
        .apply_mutations(
            &tree1,
            &[spawn_cas::TreeMutation::Upsert {
                path: vec![b"plain.txt".to_vec()],
                kind: TreeEntryKind::File,
                id: d_plain2.as_bytes(),
                size: sz_plain2,
            }],
        )
        .expect("mutate tree2");
    let layer2 = scratch.join("layer2");
    let rep2 = cas_store
        .materialize_tree(&tree2, &layer2, Some(&layer1), Some(&tree1))
        .expect("materialize layer2");
    assert_eq!(rep2.hardlinked_from_prev, 1);
    assert_eq!(rep2.files_written, 1);

    let m_exec2 = fs::symlink_metadata(layer2.join("run.sh")).expect("stat exec2");
    assert_eq!(m_exec2.ino(), m_exec1.ino());
    assert_ne!(m_exec2.ino(), m_cas_exec.ino());
    assert_eq!(m_cas_exec.nlink(), 1);
    assert_eq!(
        fs::read_to_string(layer2.join("plain.txt")).expect("read plain2"),
        "updated text\n"
    );

    // Passing prev_layer inside cas_root is strictly refused.
    let err = cas_store
        .materialize_tree(
            &tree1,
            &scratch.join("layer3"),
            Some(cas_store.cas().root()),
            None,
        )
        .expect_err("prev_layer inside CAS root must fail closed");
    assert!(matches!(err, D6StoreError::InvalidInput(_)));
}

#[test]
#[expect(clippy::too_many_lines)]
fn border_export_fast_import_matches_all_oids_modes_pointer_and_merge() {
    let scratch = Scratch::new("border_export");
    let cas_store = CasMerkleStore::open(scratch.join("cas")).expect("open cas");
    let ledger = InMemoryRefLedger::new();
    let adapter = GitBorderAdapter::new();

    let (d_readme, s_readme) = cas_store
        .put_bytes_blob(b"# Border Demo\n")
        .expect("readme");
    let (d_script, s_script) = cas_store
        .put_bytes_blob(b"#!/bin/sh\necho ok\n")
        .expect("script");
    let (d_sym, s_sym) = cas_store.put_bytes_blob(b"README.md").expect("sym");
    let (d_quoted, s_quoted) = cas_store.put_bytes_blob(b"quoted ok\n").expect("quoted");

    // > 100 MiB pointer threshold test blob (100 MiB + 1 byte streamed cleanly in 4 MiB chunks)
    let large_size = GIT_POINTER_THRESHOLD + 1;
    let mut rdr = RepeatPatternReader {
        remaining: large_size,
        byte: b'Z',
    };
    let stored_large = cas_store
        .cas()
        .put_blob(NS_BLOB, &mut rdr)
        .expect("put >100 MiB blob");
    assert_eq!(stored_large.size, large_size);
    assert!(stored_large.chunked);

    let gitlink_oid = [0xAB_u8; 20];
    let sub_tree = cas_store
        .put_tree_cached(
            &Tree::from_entries(vec![TreeEntry::file(
                b"\"quoted\" path.txt".to_vec(),
                &d_quoted,
                s_quoted,
            )])
            .expect("sub_tree"),
        )
        .expect("put sub_tree");

    let base_tree = cas_store
        .put_tree_cached(
            &Tree::from_entries(vec![TreeEntry::file(
                b"README.md".to_vec(),
                &d_readme,
                s_readme,
            )])
            .expect("base_tree"),
        )
        .expect("put base_tree");

    // Nested gitlink sits inside `vendor` subtree (`vendor/mod`).
    let vendor_dir = cas_store
        .put_tree_cached(
            &Tree::from_entries(vec![
                TreeEntry::gitlink(b"mod".to_vec(), &gitlink_oid).expect("gitlink"),
            ])
            .expect("vendor"),
        )
        .expect("put vendor");
    let feat_tree = cas_store
        .put_tree_cached(
            &Tree::from_entries(vec![
                TreeEntry::file(b"README.md".to_vec(), &d_readme, s_readme),
                TreeEntry::dir(b"dir with spaces".to_vec(), &sub_tree),
                TreeEntry::file(
                    b"huge-dataset.bin".to_vec(),
                    &stored_large.digest,
                    stored_large.size,
                ),
                TreeEntry::symlink(b"latest.md".to_vec(), &d_sym, s_sym),
                TreeEntry::executable(b"run.sh".to_vec(), &d_script, s_script),
                TreeEntry::dir(b"vendor".to_vec(), &vendor_dir),
            ])
            .expect("feat_tree"),
        )
        .expect("put feat_tree");

    let c1_body = format_commit_body(
        "Alice <alice@spawn.local> 1700001000 +0000",
        "Alice <alice@spawn.local> 1700001000 +0000",
        "Root main\n",
    );
    let c1 = cas_store
        .publish_snapshot(
            &ledger,
            &PublishRequest {
                project: "border-p",
                ref_name: "main",
                kind: D6RefKind::Main,
                expected_ref: None,
                tree: &base_tree,
                parents: &[],
                commit_body: &c1_body,
                created_at_ms: 1_700_001_000_000,
                reason: "init",
                landing_task_id: None,
            },
        )
        .expect("pub c1");

    let c2_body = format_commit_body(
        "Bob <bob@spawn.local> 1700001100 +0000",
        "Bob <bob@spawn.local> 1700001100 +0000",
        &format!(
            "Feature work\n\nSpawn-Task: 88\nSpawn-Head: {}\n",
            c1.git_oid
        ),
    );
    let c2 = cas_store
        .publish_snapshot(
            &ledger,
            &PublishRequest {
                project: "border-p",
                ref_name: "feat/all-types",
                kind: D6RefKind::Branch,
                expected_ref: None,
                tree: &feat_tree,
                parents: std::slice::from_ref(&c1.snapshot),
                commit_body: &c2_body,
                created_at_ms: 1_700_001_100_000,
                reason: "feat",
                landing_task_id: None,
            },
        )
        .expect("pub c2");

    // Non-standard extra header commit (`gpgsig`-like continuation header)
    let signed_body = b"author Carol <carol@spawn.local> 1700001200 +0000\n\
committer Carol <carol@spawn.local> 1700001200 +0000\n\
gpgsig -----BEGIN MOCK SIG-----\n \n line2\n -----END MOCK SIG-----\n\n\
Signed merge\n\nSpawn-Task: 99\n"
        .to_vec();
    let c3 = cas_store
        .publish_snapshot(
            &ledger,
            &PublishRequest {
                project: "border-p",
                ref_name: "feat/signed",
                kind: D6RefKind::Branch,
                expected_ref: None,
                tree: &feat_tree,
                parents: &[c1.snapshot.clone(), c2.snapshot.clone()],
                commit_body: &signed_body,
                created_at_ms: 1_700_001_200_000,
                reason: "merge signed",
                landing_task_id: None,
            },
        )
        .expect("pub c3");

    let wip_body = format_commit_body(
        "Bob <bob@spawn.local> 1700001250 +0000",
        "Bob <bob@spawn.local> 1700001250 +0000",
        "Volatile WIP flush\n",
    );
    cas_store
        .publish_snapshot(
            &ledger,
            &PublishRequest {
                project: "border-p",
                ref_name: "wip/scratch",
                kind: D6RefKind::Wip,
                expected_ref: None,
                tree: &base_tree,
                parents: std::slice::from_ref(&c1.snapshot),
                commit_body: &wip_body,
                created_at_ms: 1_700_001_250_000,
                reason: "wip flush",
                landing_task_id: None,
            },
        )
        .expect("pub wip");

    let mirror = scratch.join("mirror.git");
    let report = adapter
        .export_project(&cas_store, &ledger, "border-p", &mirror)
        .expect("export mirror");
    assert_eq!(report.snapshots_exported, 3);
    assert!(!report.refs.contains_key("refs/heads/wip/scratch"));
    assert!(!git_bare(&mirror, &["show-ref"]).contains("wip/scratch"));
    assert_eq!(report.refs.get("refs/heads/main"), Some(&c1.git_oid));
    assert_eq!(
        report.refs.get("refs/heads/feat/all-types"),
        Some(&c2.git_oid)
    );
    assert_eq!(report.refs.get("refs/heads/feat/signed"), Some(&c3.git_oid));

    // Verify > 100 MiB boundary file became deterministic `spawn-pointer-v1` blob in Git!
    let ptr_text = git_bare(
        &mirror,
        &[
            "cat-file",
            "blob",
            "refs/heads/feat/signed:huge-dataset.bin",
        ],
    );
    assert_eq!(
        ptr_text.into_bytes(),
        git_pointer_bytes(&stored_large.digest, stored_large.size)
    );
    let ls = git_bare(&mirror, &["ls-tree", "-r", "refs/heads/feat/signed"]);
    assert!(ls.contains("100755 blob"));
    assert!(ls.contains("120000 blob"));
    assert!(ls.contains(&format!(
        "160000 commit {}\tvendor/mod",
        GitOid::from_bytes(gitlink_oid)
    )));
    let commit_dump = git_bare(&mirror, &["cat-file", "commit", &c3.git_oid.to_hex()]);
    assert!(commit_dump.contains("gpgsig -----BEGIN MOCK SIG-----"));
    assert!(commit_dump.contains(&format!("parent {}", c1.git_oid)));
    assert!(commit_dump.contains(&format!("parent {}", c2.git_oid)));

    let fsck = test_git_command()
        .arg("--git-dir")
        .arg(&mirror)
        .args(["fsck", "--full", "--strict"])
        .output()
        .expect("fsck");
    assert!(
        fsck.status.success(),
        "{}",
        String::from_utf8_lossy(&fsck.stderr)
    );

    // Direct pushes against the read-only mirror must be rejected cleanly.
    let push_clone = scratch.join("push_clone");
    git_in(
        scratch.0.as_path(),
        &[
            "clone",
            mirror.to_str().expect("utf8"),
            push_clone.to_str().expect("utf8"),
        ],
    );
    fs::write(push_clone.join("PUSH.txt"), "unauth\n").expect("write push test");
    git_in(&push_clone, &["add", "PUSH.txt"]);
    git_in(&push_clone, &["commit", "-m", "try direct push"]);
    let push_out = test_git_command()
        .current_dir(&push_clone)
        .args(["push", "origin", "HEAD:refs/heads/direct-push"])
        .output()
        .expect("run git push");
    assert!(
        !push_out.status.success(),
        "direct push to readonly mirror must fail"
    );
    let push_stderr = String::from_utf8_lossy(&push_out.stderr);
    assert!(
        push_stderr.contains("read-only D6 border mirror"),
        "unexpected push stderr: {push_stderr}"
    );
}

#[test]
#[expect(clippy::too_many_lines)]
fn full_history_import_from_bare_repo_roundtrips_pg_and_mirror_bit_for_bit() {
    let scratch = Scratch::new("full_import");
    let work = scratch.join("work");
    let sub_work = scratch.join("submod_src");
    fs::create_dir_all(&work).expect("mkdir work");
    fs::create_dir_all(&sub_work).expect("mkdir submod_src");

    // Seed submodule commit so 160000 gitlink passes strict git fsck too
    git_in(&sub_work, &["init"]);
    fs::write(sub_work.join("lib.rs"), "pub fn sub() {}\n").expect("write sub lib");
    git_in(&sub_work, &["add", "lib.rs"]);
    git_in(&sub_work, &["commit", "-m", "sub init"]);
    let sub_oid_hex = String::from_utf8(git_in(&sub_work, &["rev-parse", "HEAD"]))
        .expect("utf8")
        .trim()
        .to_string();

    git_in(&work, &["init"]);
    git_in(&work, &["checkout", "-b", "main"]);
    fs::write(work.join("README.md"), "# Import Test\n").expect("write readme");
    git_in(&work, &["add", "README.md"]);
    git_in(&work, &["commit", "-m", "Initial commit on main"]);

    // Feature branch with 0755, symlink, gitlink, and Spawn trailers
    git_in(&work, &["checkout", "-b", "feat/roundtrip"]);
    fs::write(work.join("tool.sh"), "#!/bin/sh\necho hi\n").expect("write tool.sh");
    fs::set_permissions(work.join("tool.sh"), fs::Permissions::from_mode(0o755)).expect("chmod");
    symlink("README.md", work.join("README.link")).expect("symlink");
    git_in(&work, &["add", "tool.sh", "README.link"]);
    git_in(
        &work,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000",
            &sub_oid_hex,
            "deps/submod",
        ],
    );
    git_in(
        &work,
        &[
            "commit",
            "-m",
            "Feature commit\n\nSpawn-Task: 101\nSpawn-Head: deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        ],
    );
    let feat_oid = String::from_utf8(git_in(&work, &["rev-parse", "feat/roundtrip"]))
        .expect("utf8")
        .trim()
        .to_string();

    // Advance main and create merge commit + tags
    git_in(&work, &["checkout", "main"]);
    fs::write(work.join("OTHER.txt"), "side change\n").expect("write OTHER");
    git_in(&work, &["add", "OTHER.txt"]);
    git_in(&work, &["commit", "-m", "Main progress"]);
    git_in(&work, &["tag", "v0.1"]);
    git_in(
        &work,
        &[
            "merge",
            "--no-ff",
            "feat/roundtrip",
            "-m",
            "Merge feat/roundtrip",
        ],
    );
    git_in(&work, &["tag", "-a", "v0.2", "-m", "Annotated release"]);

    // Clone to bare repo
    let src_bare = scratch.join("source.git");
    git_in(
        scratch.0.as_path(),
        &[
            "clone",
            "--bare",
            work.to_str().expect("utf8"),
            src_bare.to_str().expect("utf8"),
        ],
    );

    let cas_store = CasMerkleStore::open(scratch.join("cas")).expect("open cas");
    let ledger = PgRefLedger::connect_isolated("d6_step2_import");
    let adapter = GitBorderAdapter::new();

    let import_report = adapter
        .import_bare_repo(&cas_store, &ledger, "roundtrip-proj", &src_bare, "backfill")
        .expect("import bare repo");
    assert_eq!(import_report.snapshots_imported, 4);
    assert_eq!(import_report.refs.len(), 4);

    let refs_map: HashMap<String, GitOid> = import_report
        .refs
        .iter()
        .map(|r| (r.git_ref.clone(), r.git_oid))
        .collect();
    assert_eq!(refs_map["refs/heads/feat/roundtrip"].to_hex(), feat_oid);

    // Every PG snapshot/ref exists cleanly
    let pg_refs = ledger
        .list_refs(Some("roundtrip-proj"))
        .expect("list pg refs");
    assert_eq!(pg_refs.len(), 4);

    // Re-export full project to fresh bare mirror and compare bit-for-bit OIDs!
    let dst_mirror = scratch.join("dst_mirror.git");
    let export_report = adapter
        .export_project(&cas_store, &ledger, "roundtrip-proj", &dst_mirror)
        .expect("re-export project");
    for imported in &import_report.refs {
        assert_eq!(
            export_report.refs.get(&imported.git_ref),
            Some(&imported.git_oid),
            "ref {} OID mismatch after import->export roundtrip",
            imported.git_ref
        );
    }

    let fsck = test_git_command()
        .arg("--git-dir")
        .arg(&dst_mirror)
        .args(["fsck", "--full", "--strict"])
        .output()
        .expect("fsck");
    assert!(
        fsck.status.success(),
        "{}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}

#[test]
fn tombstoned_spawn_large_trailer_is_rejected_on_publish() {
    let scratch = Scratch::new("tombstone");
    let cas_store = CasMerkleStore::open(scratch.join("cas")).expect("open cas");
    let ledger = InMemoryRefLedger::new();
    let empty = cas_store.cas().put_empty_tree().expect("empty tree");

    let body = format_commit_body(
        "Alice <alice@spawn.local> 1700002000 +0000",
        "Alice <alice@spawn.local> 1700002000 +0000",
        "Bad commit\n\nSpawn-Large: payload.bin\n",
    );
    let err = cas_store
        .publish_snapshot(
            &ledger,
            &PublishRequest {
                project: "tombstone-proj",
                ref_name: "main",
                kind: D6RefKind::Main,
                expected_ref: None,
                tree: &empty,
                parents: &[],
                commit_body: &body,
                created_at_ms: 1_700_002_000_000,
                reason: "forbidden trailer",
                landing_task_id: None,
            },
        )
        .expect_err("Spawn-Large trailer must be rejected");
    assert!(matches!(err, D6StoreError::InvalidInput(_)));
    assert!(
        ledger
            .list_refs(Some("tombstone-proj"))
            .expect("list")
            .is_empty()
    );
}
