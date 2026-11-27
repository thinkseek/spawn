//! End-to-end blackbox CLI and PostgreSQL/CAS tests for `spawn storage cutover` (`#113`).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use spawn_cas::{CAS_MARKER, Digest, GitOid, Snapshot, Tree, TreeEntry};
use spawn_host::{
    BACKEND_MARKER, CasMerkleStore, D6RefKind, D6StoreError, LedgerRef, LedgerSnapshot,
    PublishRequest, RefLedger, format_commit_body,
};
use spawn_postgres::testing::TestSchema;
use spawn_postgres::{PostgresStore, RefKind};

const SPAWN_BIN: &str = env!("CARGO_BIN_EXE_spawn");

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("spawn-e2e-{tag}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).expect("create scratch dir");
        Self(dir)
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

struct TestPgRefLedger(Arc<PostgresStore>);

impl TestPgRefLedger {
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
}

impl RefLedger for TestPgRefLedger {
    fn record_snapshot(
        &self,
        id: &Digest,
        git_oid: &GitOid,
        tree: &Digest,
        parents: &[Digest],
        project: &str,
        created_at_ms: i64,
    ) -> Result<Digest, D6StoreError> {
        let p: Vec<String> = parents.iter().map(Digest::to_string).collect();
        let canonical = self
            .0
            .record_snapshot(
                &id.to_string(),
                &git_oid.to_hex(),
                &tree.to_string(),
                &p,
                project,
                created_at_ms,
            )
            .map_err(|e| D6StoreError::Ledger(e.to_string()))?;
        Ok(Digest::parse(&canonical)?)
    }

    fn get_snapshot(&self, id: &Digest) -> Result<Option<LedgerSnapshot>, D6StoreError> {
        let row = self
            .0
            .get_snapshot(&id.to_string())
            .map_err(|e| D6StoreError::Ledger(e.to_string()))?;
        row.map(|r| {
            let parents = r
                .parents
                .iter()
                .map(|s| Digest::parse(s))
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
            .0
            .get_snapshot_by_oid(&git_oid.to_hex())
            .map_err(|e| D6StoreError::Ledger(e.to_string()))?;
        row.map(|r| {
            let parents = r
                .parents
                .iter()
                .map(|s| Digest::parse(s))
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
        let exp = expected.map(Digest::to_string);
        let seq = self
            .0
            .cas_ref(
                project,
                name,
                Self::to_pg_kind(kind),
                exp.as_deref(),
                &new_snapshot.to_string(),
                reason,
            )
            .map_err(|e| D6StoreError::Ledger(e.to_string()))?;
        u64::try_from(seq).map_err(|_| D6StoreError::Ledger("negative ref seq".to_string()))
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
        let exp = expected_base.map(Digest::to_string);
        let seq = self
            .0
            .land_main_ref(
                project,
                name,
                exp.as_deref(),
                &new_snapshot.to_string(),
                task_id,
                reason,
            )
            .map_err(|e| D6StoreError::Ledger(e.to_string()))?;
        u64::try_from(seq).map_err(|_| D6StoreError::Ledger("negative ref seq".to_string()))
    }

    fn list_refs(&self, project: Option<&str>) -> Result<Vec<LedgerRef>, D6StoreError> {
        let rows = self
            .0
            .list_refs(project)
            .map_err(|e| D6StoreError::Ledger(e.to_string()))?;
        rows.into_iter()
            .map(|r| {
                Ok(LedgerRef {
                    project: r.project,
                    name: r.name,
                    kind: Self::from_pg_kind(r.kind),
                    snapshot: Digest::parse(&r.snapshot)?,
                    updated_at_ms: r.updated_at_ms,
                })
            })
            .collect()
    }
}

fn schema_url(schema: &TestSchema) -> String {
    let url = schema.url();
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}options=-csearch_path%3D{}", schema.name())
}

fn run_cli(args: &[&str], db_url: Option<&str>) -> Output {
    let mut cmd = Command::new(SPAWN_BIN);
    cmd.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .stdin(Stdio::null())
        .args(args);
    if let Some(url) = db_url {
        cmd.env("SPAWN_DATABASE_URL", url);
    }
    cmd.output().expect("execute spawn CLI")
}

fn run_cli_json(args: &[&str], db_url: &str, expected_code: i32) -> Value {
    let out = run_cli(args, Some(db_url));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(expected_code),
        "args={args:?} expected exit {expected_code}, got {:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status.code()
    );
    serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("invalid JSON for {args:?}: {e}\nstdout:\n{stdout}"))
}

fn git_in(dir: &Path, args: &[&str]) -> Output {
    spawn_host::test_git_command()
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run git")
}

fn git_ok(dir: &Path, args: &[&str]) -> String {
    let out = git_in(dir, args);
    assert!(
        out.status.success(),
        "git {:?} failed in {}: {}",
        args,
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_project_repo(scratch: &Scratch) -> (PathBuf, PathBuf, PathBuf) {
    let ws = scratch.join("ws");
    let work = scratch.join("work");
    let bare = scratch.join("origin.git");
    fs::create_dir_all(&ws).expect("mkdir ws");
    fs::create_dir_all(&work).expect("mkdir work");

    git_ok(&work, &["init", "-b", "main"]);
    git_ok(&work, &["config", "user.name", "Cutover Tester"]);
    git_ok(&work, &["config", "user.email", "tester@spawn.local"]);
    fs::write(work.join("README.md"), "# D6 Cutover CLI Test\n").expect("write readme");
    git_ok(&work, &["add", "README.md"]);
    git_ok(&work, &["commit", "-m", "initial main"]);

    git_ok(&work, &["checkout", "-b", "feat/alpha"]);
    fs::write(work.join("alpha.txt"), "alpha branch\n").expect("write alpha");
    git_ok(&work, &["add", "alpha.txt"]);
    git_ok(&work, &["commit", "-m", "feature alpha"]);

    git_ok(&work, &["checkout", "main"]);
    git_ok(&work, &["tag", "v0.1"]);

    git_ok(
        &scratch.0,
        &[
            "clone",
            "--bare",
            work.to_str().expect("utf8 work"),
            bare.to_str().expect("utf8 bare"),
        ],
    );
    git_ok(
        &work,
        &["remote", "add", "origin", bare.to_str().expect("utf8 bare")],
    );
    (ws, work, bare)
}

fn seed_post_cutover_main_landing(schema: &TestSchema, cas_dir: &Path, project: &str) -> String {
    let store = CasMerkleStore::open(cas_dir).expect("open cutover cas");
    let pg = Arc::new(schema.store());
    let ledger = TestPgRefLedger(Arc::clone(&pg));

    let main_ref = ledger
        .list_refs(Some(project))
        .expect("list refs")
        .into_iter()
        .find(|r| r.name == "main" && r.kind == D6RefKind::Main)
        .expect("live main ref");
    let old_main_snap = main_ref.snapshot;
    let old_main_oid = ledger
        .get_snapshot(&old_main_snap)
        .expect("get old main snapshot")
        .expect("old main snapshot exists")
        .git_oid;

    let (blob_id, blob_sz) = store
        .put_bytes_blob(b"landed in d6 mode after cutover\n")
        .expect("put blob");
    let tree = store
        .put_tree_cached(
            &Tree::from_entries(vec![TreeEntry::file(
                b"LIVE_D6.txt".to_vec(),
                &blob_id,
                blob_sz,
            )])
            .expect("build tree"),
        )
        .expect("put tree");

    let mission = pg
        .mission_add("D6 cutover post-switch landing", &json!([project]), "human")
        .expect("mission_add");
    pg.mission_configure(mission, Some("/scenarios"), "gate", None, None)
        .expect("mission_configure");
    let task_id = pg
        .task_assign(
            mission,
            "ephemeral",
            "w_d6_cli",
            "land after cutover",
            "default",
        )
        .expect("task_assign");

    let commit_body = format_commit_body(
        "Gate <gate@spawn.local> 1700001000 +0000",
        "Gate <gate@spawn.local> 1700001000 +0000",
        &format!("Post-cutover live D6 land\n\nSpawn-Task: {task_id}\n"),
    );
    let candidate_oid = store
        .project_snapshot_oid(
            &Snapshot {
                tree: tree.clone(),
                parents: vec![old_main_snap.clone()],
                commit_body: commit_body.clone(),
            },
            &[old_main_oid],
        )
        .expect("project candidate oid");

    pg.task_delivered(task_id, 1).expect("task_delivered");
    assert!(pg.task_reported(task_id, "done").expect("task_reported"));
    pg.integrator_record(&json!({
        "type": "task.gated",
        "id": task_id,
        "revision": 0,
        "verdict": "accepted",
        "base": old_main_oid.to_hex(),
        "candidate": candidate_oid.to_hex(),
    }))
    .expect("integrator_record");

    let parents = [old_main_snap.clone()];
    let published = store
        .publish_snapshot(
            &ledger,
            &PublishRequest {
                project,
                ref_name: "main",
                kind: D6RefKind::Main,
                expected_ref: Some(&old_main_snap),
                tree: &tree,
                parents: &parents,
                commit_body: &commit_body,
                created_at_ms: 1_700_001_000_000,
                reason: "post-cutover live main landing",
                landing_task_id: Some(task_id),
            },
        )
        .expect("publish main landing");
    published.git_oid.to_hex()
}

fn assert_pre_cutover_and_dirty_merge_guard(url: &str, ws: &Path, bare: &Path) -> (String, String) {
    let ws_s = ws.to_string_lossy().into_owned();
    let bare_s = bare.to_string_lossy().into_owned();

    let st = run_cli_json(
        &[
            "storage", "cutover", "--status", "--repo", &bare_s, "--root", &ws_s, "--json",
        ],
        url,
        0,
    );
    assert_eq!(st["cas_initialized"], false);
    assert_eq!(st["workspace_backend"], Value::Null);
    assert_eq!(st["pending_merges_clean"], true);
    assert_eq!(st["readonly_border"], false);
    assert!(!ws.join(".d6/cas").exists());

    let pre_ver = run_cli_json(
        &[
            "storage", "cutover", "--verify", "--repo", &bare_s, "--root", &ws_s, "--json",
        ],
        url,
        1,
    );
    assert_eq!(pre_ver["ok"], false);
    assert_eq!(pre_ver["checks"]["cas_initialized"], false);
    assert!(!ws.join(".d6/cas").exists());

    fs::create_dir_all(ws.join(".merge")).expect("mkdir .merge");
    fs::write(ws.join(".merge/in-flight"), b"dirty").expect("seed .merge marker");
    let dirty_st = run_cli_json(
        &[
            "storage", "cutover", "--status", "--repo", &bare_s, "--root", &ws_s, "--json",
        ],
        url,
        0,
    );
    assert_eq!(dirty_st["pending_merges_clean"], false);

    let blocked = run_cli(
        &[
            "storage", "cutover", "--to", "d6", "--repo", &bare_s, "--root", &ws_s, "--json",
        ],
        Some(url),
    );
    assert!(!blocked.status.success(), ".merge/* must block --to d6");
    assert!(!ws.join(".d6/cas").join(CAS_MARKER).exists());
    fs::remove_dir_all(ws.join(".merge")).expect("clean .merge");
    (ws_s, bare_s)
}

#[test]
fn storage_cutover_help_opens_no_store() {
    let scratch = Scratch::new("cutover-help");
    let clean_root = scratch.join("clean-ws");
    fs::create_dir_all(&clean_root).expect("mkdir clean_root");
    let root_s = clean_root.to_string_lossy().into_owned();

    for args in [
        &["storage", "--help"][..],
        &["storage", "cutover", "--help"][..],
    ] {
        let out = run_cli(args, None);
        assert_eq!(out.status.code(), Some(0), "help failed for {args:?}");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("spawn storage cutover"), "{stdout}");
        assert!(stdout.contains("SPAWN_DATABASE_URL"), "{stdout}");
    }

    let status_no_db = run_cli(&["storage", "cutover", "--status", "--root", &root_s], None);
    assert!(!status_no_db.status.success());
    assert!(!clean_root.join(".d6/cas").exists());
    assert!(!clean_root.join(".d6/cas").join(CAS_MARKER).exists());
}

fn assert_cutover_and_locked_border(url: &str, ws: &Path, work: &Path, ws_s: &str, bare_s: &str) {
    let cut = run_cli_json(
        &[
            "storage",
            "cutover",
            "--to",
            "d6",
            "--repo",
            bare_s,
            "--root",
            ws_s,
            "--reason",
            "e2e cutover",
            "--json",
        ],
        url,
        0,
    );
    assert_eq!(cut["phase"], "resumed");
    assert_eq!(cut["storage_mode"], "d6_cas");
    assert_eq!(cut["readonly_border"], true);
    assert_eq!(cut["workspace_backend"], "d6");
    assert!(cut["refs_verified"].as_u64().unwrap_or(0) >= 3);
    assert_eq!(
        fs::read_to_string(ws.join(BACKEND_MARKER)).unwrap().trim(),
        "d6"
    );
    assert!(ws.join(".d6/cas/.gc-fence").is_file());
    assert!(ws.join(".d6/cas/.cutover-state.json").is_file());

    let ver = run_cli_json(
        &[
            "storage", "cutover", "--verify", "--repo", bare_s, "--root", ws_s, "--json",
        ],
        url,
        0,
    );
    assert_eq!(ver["ok"], true);
    for key in [
        "pending_merges_clean",
        "cas_initialized",
        "gc_fence",
        "ref_parity",
        "readonly_border",
        "workspace_backend_marker",
    ] {
        assert_eq!(ver["checks"][key], true, "check {key} failed: {ver}");
    }

    fs::write(work.join("blocked.txt"), "direct push must fail\n").expect("write blocked");
    git_ok(work, &["add", "blocked.txt"]);
    git_ok(work, &["commit", "-m", "blocked push"]);
    assert!(!git_in(work, &["push", "origin", "main"]).status.success());

    let second = run_cli(
        &[
            "storage", "cutover", "--to", "d6", "--repo", bare_s, "--root", ws_s, "--json",
        ],
        Some(url),
    );
    assert!(!second.status.success(), "duplicate --to d6 must fail");
}

#[test]
fn pg_storage_cutover_status_to_d6_verify_live_land_and_lossless_rollback() {
    let schema = TestSchema::new("storage_cutover_e2e");
    let _pg = schema.store();
    let db_url = schema_url(&schema);
    let scratch = Scratch::new("cutover-e2e");
    let (ws, work, bare) = init_project_repo(&scratch);
    let (ws_s, bare_s) = assert_pre_cutover_and_dirty_merge_guard(&db_url, &ws, &bare);

    assert_cutover_and_locked_border(&db_url, &ws, &work, &ws_s, &bare_s);

    let canonical_project = fs::canonicalize(&bare)
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let landed_oid =
        seed_post_cutover_main_landing(&schema, &ws.join(".d6/cas"), &canonical_project);

    let drifted = run_cli_json(
        &[
            "storage", "cutover", "--verify", "--repo", &bare_s, "--root", &ws_s, "--json",
        ],
        &db_url,
        1,
    );
    assert_eq!(drifted["ok"], false);
    assert_eq!(drifted["checks"]["ref_parity"], false);
    assert_eq!(drifted["checks"]["gc_fence"], true);
    assert_eq!(drifted["checks"]["readonly_border"], true);
    assert_eq!(drifted["checks"]["workspace_backend_marker"], true);

    let rb = run_cli_json(
        &[
            "storage",
            "cutover",
            "--rollback",
            "git",
            "--repo",
            &bare_s,
            "--root",
            &ws_s,
            "--reason",
            "e2e rollback",
            "--json",
        ],
        &db_url,
        0,
    );
    assert_eq!(rb["phase"], "rolled_back");
    assert_eq!(rb["storage_mode"], "git");
    assert_eq!(rb["readonly_border"], false);
    assert_eq!(rb["workspace_backend"], "git");
    assert!(rb["exported_refs"].as_u64().unwrap_or(0) >= 3);

    assert_eq!(git_ok(&bare, &["rev-parse", "refs/heads/main"]), landed_oid);
    git_ok(&bare, &["fsck", "--full", "--strict"]);
    assert_eq!(
        fs::read_to_string(ws.join(BACKEND_MARKER)).unwrap().trim(),
        "git"
    );

    git_ok(&work, &["fetch", "origin"]);
    git_ok(&work, &["reset", "--hard", "origin/main"]);
    fs::write(work.join("after_rb.txt"), "writable again\n").expect("write after_rb");
    git_ok(&work, &["add", "after_rb.txt"]);
    git_ok(&work, &["commit", "-m", "post-rollback git push"]);
    git_ok(&work, &["push", "origin", "main"]);
}

#[test]
fn pg_storage_cutover_gc_fence_mismatch_fails_closed() {
    let schema_a = TestSchema::new("storage_cutover_fence_a");
    let schema_b = TestSchema::new("storage_cutover_fence_b");
    let _store_a = schema_a.store();
    let _store_b = schema_b.store();

    let url_a = schema_url(&schema_a);
    let url_b = schema_url(&schema_b);

    let scratch = Scratch::new("cutover-fence");
    let (ws, _work, bare) = init_project_repo(&scratch);
    let ws_s = ws.to_string_lossy().into_owned();
    let bare_s = bare.to_string_lossy().into_owned();

    run_cli_json(
        &[
            "storage", "cutover", "--to", "d6", "--repo", &bare_s, "--root", &ws_s, "--json",
        ],
        &url_a,
        0,
    );
    let state_before = fs::read(ws.join(".d6/cas/.cutover-state.json")).expect("read state");

    let mismatch_status = run_cli_json(
        &[
            "storage", "cutover", "--status", "--repo", &bare_s, "--root", &ws_s, "--json",
        ],
        &url_b,
        0,
    );
    assert_eq!(mismatch_status["gc_fence"]["status"], "mismatch");

    let mismatch_verify = run_cli_json(
        &[
            "storage", "cutover", "--verify", "--repo", &bare_s, "--root", &ws_s, "--json",
        ],
        &url_b,
        1,
    );
    assert_eq!(mismatch_verify["ok"], false);
    assert_eq!(mismatch_verify["checks"]["gc_fence"], false);
    assert_eq!(mismatch_verify["gc_fence"]["status"], "mismatch");

    let mismatch_cutover = run_cli(
        &[
            "storage", "cutover", "--to", "d6", "--repo", &bare_s, "--root", &ws_s, "--json",
        ],
        Some(&url_b),
    );
    assert!(!mismatch_cutover.status.success());

    let mismatch_rb = run_cli(
        &[
            "storage",
            "cutover",
            "--rollback",
            "git",
            "--repo",
            &bare_s,
            "--root",
            &ws_s,
            "--json",
        ],
        Some(&url_b),
    );
    assert!(!mismatch_rb.status.success());
    assert!(
        spawn_host::is_readonly_border(&bare).expect("check bare border"),
        "bare repo must remain locked read-only when rollback fails GC fence"
    );
    assert_eq!(
        fs::read(ws.join(".d6/cas/.cutover-state.json")).expect("read state after"),
        state_before
    );
    assert_eq!(
        fs::read_to_string(ws.join(BACKEND_MARKER)).unwrap().trim(),
        "d6"
    );

    let ok_a = run_cli_json(
        &[
            "storage", "cutover", "--verify", "--repo", &bare_s, "--root", &ws_s, "--json",
        ],
        &url_a,
        0,
    );
    assert_eq!(ok_a["ok"], true);
}
