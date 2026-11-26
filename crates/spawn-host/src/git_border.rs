//! D6 Phase 4 Step 2 (`#113` Fine Item 7.3): `GitBorderAdapter` — lossless
//! bidirectional border translation between `CasMerkleStore` + `RefLedger` and
//! external Git bare repositories (`git fast-import` export + history import)
//! with deterministic 40-hex OID verification.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, BufReader, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use spawn_cas::{
    Digest, GIT_POINTER_THRESHOLD, GitOid, JournalEntry, NS_BLOB, Snapshot, Store, Tree, TreeEntry,
    TreeEntryKind, git_pointer_bytes,
};

use crate::cas_merkle::{
    CasMerkleStore, D6RefKind, D6StoreError, LedgerRef, RefLedger, is_pointer_sized,
};
use crate::git::git_command;
use crate::tree_path::nul_fields;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Ref target to materialize on a Git boundary mirror.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BorderRefTarget {
    /// Full Git reference name (e.g. `refs/heads/main`, `refs/tags/v1.0`).
    pub git_ref: String,
    /// Target `spawn-snapshot-v1` digest.
    pub snapshot: Digest,
}

/// Verification & throughput report produced by [`GitBorderAdapter::export_refs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportReport {
    /// Target bare mirror directory.
    pub mirror_dir: PathBuf,
    /// Verified mapping of exported Git ref -> 40-hex commit OID.
    pub refs: BTreeMap<String, GitOid>,
    /// Total snapshots exported in topological order.
    pub snapshots_exported: usize,
    /// Distinct CAS blobs streamed to `git fast-import`.
    pub blobs_streamed: usize,
}

/// One Git reference imported into [`CasMerkleStore`] + [`RefLedger`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedRef {
    /// Source full Git ref (e.g. `refs/heads/main`).
    pub git_ref: String,
    /// Ref name written to `spawn_refs` (`main`, `feat/x`, `tags/v1`).
    pub ledger_name: String,
    /// Classification in `spawn_refs`.
    pub kind: D6RefKind,
    /// Imported `spawn-snapshot-v1` digest.
    pub snapshot: Digest,
    /// Verified 40-hex Git commit OID (`== git rev-parse`).
    pub git_oid: GitOid,
    /// Committed `spawn_ref_log.seq`.
    pub ref_seq: u64,
}

/// Verification report produced by [`GitBorderAdapter::import_bare_repo`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportReport {
    /// Source bare repository path.
    pub source_repo: PathBuf,
    /// Number of commits imported as `spawn-snapshot-v1` objects.
    pub snapshots_imported: usize,
    /// All imported branch/tag refs with verified OIDs.
    pub refs: Vec<ImportedRef>,
}

/// Bidirectional border translator between internal D6 CAS/PG state and external Git bare repos.
#[derive(Debug, Clone)]
pub struct GitBorderAdapter {
    git_program: OsString,
}

impl Default for GitBorderAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl GitBorderAdapter {
    /// Construct a border adapter using the default `git` binary under `LC_ALL=C`.
    pub fn new() -> Self {
        Self {
            git_program: OsString::from("git"),
        }
    }

    /// Construct a border adapter invoking a custom `git` executable path.
    pub fn with_program(program: impl Into<OsString>) -> Self {
        Self {
            git_program: program.into(),
        }
    }

    fn git_bare(&self, bare_dir: &Path) -> Command {
        let mut cmd = git_command(&self.git_program);
        cmd.arg("--git-dir")
            .arg(bare_dir)
            .arg("-c")
            .arg("core.quotepath=false");
        cmd
    }

    /// Initialize an empty read-only-intended bare boundary mirror repository.
    fn init_bare_mirror(&self, mirror_dir: &Path) -> Result<(), D6StoreError> {
        fs::create_dir_all(mirror_dir)?;
        let out = git_command(&self.git_program)
            .arg("init")
            .arg("--bare")
            .arg(mirror_dir)
            .output()?;
        ensure_git_ok("init --bare", &out)?;
        self.configure_readonly_receive(mirror_dir)
    }

    /// Configure a bare mirror to reject external pushes directly (`receive.denyCurrentBranch`,
    /// `receive.denyNonFastForwards`, `receive.denyDeletes`, plus an executable `hooks/pre-receive`).
    pub(crate) fn configure_readonly_receive(&self, mirror_dir: &Path) -> Result<(), D6StoreError> {
        for (key, val) in [
            ("receive.denyNonFastForwards", "true"),
            ("receive.denyDeletes", "true"),
            ("receive.denyCurrentBranch", "refuse"),
            ("gc.auto", "0"),
        ] {
            let out = self
                .git_bare(mirror_dir)
                .args(["config", key, val])
                .output()?;
            ensure_git_ok("config", &out)?;
        }
        let hooks_dir = mirror_dir.join("hooks");
        match fs::symlink_metadata(&hooks_dir) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                return Err(D6StoreError::InvalidInput(format!(
                    "{} must be a real directory",
                    hooks_dir.display()
                )));
            }
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => fs::create_dir_all(&hooks_dir)?,
            Err(err) => return Err(err.into()),
        }
        let hook_path = hooks_dir.join("pre-receive");
        match fs::symlink_metadata(&hook_path) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
                return Err(D6StoreError::InvalidInput(format!(
                    "{} cannot be a directory",
                    hook_path.display()
                )));
            }
            Ok(_) => fs::remove_file(&hook_path)?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
        let mut hook = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&hook_path)?;
        hook.write_all(
            b"#!/bin/sh\necho \"read-only D6 border mirror: direct pushes are disabled\" >&2\nexit 1\n",
        )?;
        hook.flush()?;
        hook.set_permissions(fs::Permissions::from_mode(0o755))?;
        Ok(())
    }

    /// Export all `main`, `branch`, and `tag` refs for `project` from `ledger` into `mirror_dir`.
    pub fn export_project(
        &self,
        store: &CasMerkleStore,
        ledger: &dyn RefLedger,
        project: &str,
        mirror_dir: &Path,
    ) -> Result<ExportReport, D6StoreError> {
        let rows = ledger.list_refs(Some(project))?;
        let mut targets = Vec::new();
        for row in rows {
            let git_ref = match row.kind {
                D6RefKind::Main | D6RefKind::Branch => {
                    if row.name.starts_with("refs/") {
                        row.name
                    } else {
                        format!("refs/heads/{}", row.name)
                    }
                }
                D6RefKind::Tag => {
                    if row.name.starts_with("refs/") {
                        row.name
                    } else if let Some(stripped) = row.name.strip_prefix("tags/") {
                        format!("refs/tags/{stripped}")
                    } else {
                        format!("refs/tags/{}", row.name)
                    }
                }
                D6RefKind::Wip => continue,
            };
            targets.push(BorderRefTarget {
                git_ref,
                snapshot: row.snapshot,
            });
        }
        self.export_refs(store, Some(ledger), mirror_dir, &targets)
    }

    /// Export a slice of [`BorderRefTarget`] DAGs via `git fast-import` into `mirror_dir`,
    /// asserting real-time equality between Git's generated tree/commit OIDs and D6 deterministic
    /// projections (`project_tree_oid` / `project_snapshot_oid`).
    pub fn export_refs(
        &self,
        store: &CasMerkleStore,
        ledger: Option<&dyn RefLedger>,
        mirror_dir: &Path,
        targets: &[BorderRefTarget],
    ) -> Result<ExportReport, D6StoreError> {
        for target in targets {
            validate_git_ref(target.git_ref.as_str())?;
        }
        if !mirror_dir.join("HEAD").is_file() {
            self.init_bare_mirror(mirror_dir)?;
        }

        let plan = build_export_plan(store, ledger, targets)?;
        let marks_path = temp_marks_path();
        let run_id = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let stage_prefix = format!("refs/d6-stage/run-{}-{run_id}", std::process::id());
        let mut commit_oids: HashMap<Digest, GitOid> = HashMap::new();
        let mut blob_marks: HashMap<(Digest, bool), usize> = HashMap::new();
        let mut exported_blobs = BTreeSet::new();
        let mut staging_refs = Vec::new();

        let run_result = self.execute_export_runs(
            store,
            mirror_dir,
            &plan,
            &marks_path,
            &stage_prefix,
            &mut commit_oids,
            &mut blob_marks,
            &mut exported_blobs,
            &mut staging_refs,
        );
        let _ = fs::remove_file(&marks_path);
        let commit_oids = match run_result {
            Ok(oids) => oids,
            Err(err) => {
                self.cleanup_stage_prefix(mirror_dir, &stage_prefix);
                return Err(err);
            }
        };

        let mut ref_map = BTreeMap::new();
        let mut ref_script = String::new();
        for target in targets {
            let oid = commit_oids.get(&target.snapshot).ok_or_else(|| {
                D6StoreError::InvalidInput(format!(
                    "missing exported commit for {}",
                    target.snapshot
                ))
            })?;
            let _ = writeln!(ref_script, "update {} {oid}", target.git_ref);
            ref_map.insert(target.git_ref.clone(), *oid);
        }
        for stage in &staging_refs {
            let _ = writeln!(ref_script, "delete {stage}");
        }
        if !ref_script.is_empty() {
            let out = run_with_stdin(
                self.git_bare(mirror_dir).args(["update-ref", "--stdin"]),
                ref_script.as_bytes(),
            )?;
            ensure_git_ok("update-ref --stdin", &out)?;
        }

        if let Some(head_target) = default_head_ref(&ref_map) {
            let out = self
                .git_bare(mirror_dir)
                .args(["symbolic-ref", "HEAD", head_target])
                .output()?;
            ensure_git_ok("symbolic-ref HEAD", &out)?;
        }

        for (git_ref, expected_oid) in &ref_map {
            let rev = self.rev_parse_commit(mirror_dir, git_ref)?;
            if rev != *expected_oid {
                return Err(D6StoreError::OidMismatch {
                    what: "ref",
                    expected: expected_oid.to_hex(),
                    actual: rev.to_hex(),
                });
            }
        }

        Ok(ExportReport {
            mirror_dir: mirror_dir.to_path_buf(),
            refs: ref_map,
            snapshots_exported: plan.topo.len(),
            blobs_streamed: exported_blobs.len(),
        })
    }

    #[expect(clippy::too_many_arguments)]
    fn execute_export_runs(
        &self,
        store: &CasMerkleStore,
        mirror_dir: &Path,
        plan: &ExportPlan,
        marks_path: &Path,
        stage_prefix: &str,
        commit_oids: &mut HashMap<Digest, GitOid>,
        _blob_marks: &mut HashMap<(Digest, bool), usize>,
        exported_blobs: &mut BTreeSet<(Digest, bool)>,
        staging_refs: &mut Vec<String>,
    ) -> Result<HashMap<Digest, GitOid>, D6StoreError> {
        let mut idx = 0_usize;
        let mut stage_counter = 0_usize;

        while idx < plan.topo.len() {
            // Group contiguous runs of standard commits into one `git fast-import` pass.
            let start = idx;
            while idx < plan.topo.len()
                && parse_standard_commit_body(&plan.snapshots[&plan.topo[idx]].commit_body)
                    .is_some()
            {
                idx += 1;
            }
            if idx > start {
                let slice = &plan.topo[start..idx];
                self.run_fast_import_batch(
                    store,
                    mirror_dir,
                    plan,
                    slice,
                    marks_path,
                    stage_prefix,
                    &mut stage_counter,
                    commit_oids,
                    exported_blobs,
                    staging_refs,
                )?;
            }

            // Emit any non-standard commit (`gpgsig` / custom headers) via staging tree + hash-object.
            if idx < plan.topo.len() {
                let snap_id = &plan.topo[idx];
                let snap = &plan.snapshots[snap_id];
                let tree_oid = self.materialize_git_tree_via_fast_import(
                    store,
                    mirror_dir,
                    plan,
                    &snap.tree,
                    marks_path,
                    stage_prefix,
                    &mut stage_counter,
                    exported_blobs,
                    staging_refs,
                )?;
                let expected_tree = plan.expected_trees[snap_id];
                if tree_oid != expected_tree {
                    return Err(D6StoreError::OidMismatch {
                        what: "tree",
                        expected: expected_tree.to_hex(),
                        actual: tree_oid.to_hex(),
                    });
                }
                let oid = self.write_verbatim_commit(mirror_dir, snap, tree_oid, commit_oids)?;
                let expected_commit = plan.expected_commits[snap_id];
                if oid != expected_commit {
                    return Err(D6StoreError::OidMismatch {
                        what: "commit",
                        expected: expected_commit.to_hex(),
                        actual: oid.to_hex(),
                    });
                }
                commit_oids.insert(snap_id.clone(), oid);
                idx += 1;
            }
        }

        Ok(commit_oids.clone())
    }

    fn cleanup_stage_prefix(&self, mirror_dir: &Path, stage_prefix: &str) {
        let Ok(out) = self
            .git_bare(mirror_dir)
            .args(["for-each-ref", "--format=%(refname)", stage_prefix])
            .output()
        else {
            return;
        };
        if !out.status.success() {
            return;
        }
        let mut script = String::new();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                let _ = writeln!(script, "delete {trimmed}");
            }
        }
        if !script.is_empty() {
            let _ = run_with_stdin(
                self.git_bare(mirror_dir).args(["update-ref", "--stdin"]),
                script.as_bytes(),
            );
        }
    }

    #[expect(clippy::too_many_arguments)]
    fn run_fast_import_batch(
        &self,
        store: &CasMerkleStore,
        mirror_dir: &Path,
        plan: &ExportPlan,
        slice: &[Digest],
        marks_path: &Path,
        stage_prefix: &str,
        stage_counter: &mut usize,
        commit_oids: &mut HashMap<Digest, GitOid>,
        exported_blobs: &mut BTreeSet<(Digest, bool)>,
        staging_refs: &mut Vec<String>,
    ) -> Result<(), D6StoreError> {
        let mut cmd = self.git_bare(mirror_dir);
        cmd.args(["fast-import", "--quiet", "--force", "--done"])
            .arg(format!("--export-marks={}", marks_path.display()))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| D6StoreError::InvalidInput("missing fast-import stdin".to_string()))?;

        let stream_res = write_fast_import_batch_stream(
            &mut stdin,
            store,
            plan,
            slice,
            stage_prefix,
            stage_counter,
            commit_oids,
            exported_blobs,
            staging_refs,
        );

        drop(stdin);
        let out = child.wait_with_output()?;
        let commit_list = stream_res?;
        ensure_git_ok("fast-import", &out)?;

        let marks = parse_marks_file(marks_path)?;
        for (snap_id, mark) in commit_list {
            let actual_oid = marks
                .get(&mark)
                .copied()
                .ok_or_else(|| D6StoreError::GitBorder {
                    command: "fast-import marks".to_string(),
                    detail: format!("missing exported mark :{mark}"),
                })?;
            let expected_commit = plan.expected_commits[&snap_id];
            if actual_oid != expected_commit {
                return Err(D6StoreError::OidMismatch {
                    what: "commit",
                    expected: expected_commit.to_hex(),
                    actual: actual_oid.to_hex(),
                });
            }
            let actual_tree = self.rev_parse_tree(mirror_dir, actual_oid)?;
            let expected_tree = plan.expected_trees[&snap_id];
            if actual_tree != expected_tree {
                return Err(D6StoreError::OidMismatch {
                    what: "tree",
                    expected: expected_tree.to_hex(),
                    actual: actual_tree.to_hex(),
                });
            }
            commit_oids.insert(snap_id, actual_oid);
        }
        Ok(())
    }

    #[expect(clippy::too_many_arguments)]
    fn materialize_git_tree_via_fast_import(
        &self,
        store: &CasMerkleStore,
        mirror_dir: &Path,
        plan: &ExportPlan,
        tree_digest: &Digest,
        marks_path: &Path,
        stage_prefix: &str,
        stage_counter: &mut usize,
        exported_blobs: &mut BTreeSet<(Digest, bool)>,
        staging_refs: &mut Vec<String>,
    ) -> Result<GitOid, D6StoreError> {
        *stage_counter += 1;
        let stage_ref = format!("{stage_prefix}/tree-{}", *stage_counter);
        let leaves = &plan.tree_leaves[tree_digest];

        let mut cmd = self.git_bare(mirror_dir);
        cmd.args(["fast-import", "--quiet", "--force", "--done"])
            .arg(format!("--export-marks={}", marks_path.display()))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| D6StoreError::InvalidInput("missing fast-import stdin".to_string()))?;

        let commit_mark = {
            let mut batch_blob_marks: HashMap<(Digest, bool), usize> = HashMap::new();
            let mut next_mark = 1_usize;
            for entry in leaves.values() {
                if entry.kind == TreeEntryKind::Gitlink {
                    continue;
                }
                let Some(digest) = entry.digest() else {
                    continue;
                };
                let key = (digest.clone(), is_pointer_sized(entry));
                if batch_blob_marks.contains_key(&key) {
                    continue;
                }
                let mark = next_mark;
                next_mark += 1;
                write_blob_command(&mut stdin, store, mark, &key.0, entry.size, key.1)?;
                batch_blob_marks.insert(key.clone(), mark);
                exported_blobs.insert(key);
            }
            let cmark = next_mark;
            writeln!(stdin, "commit {stage_ref}")?;
            writeln!(stdin, "mark :{cmark}")?;
            writeln!(stdin, "committer d6 <d6@localhost> 0 +0000")?;
            writeln!(stdin, "data 0")?;
            writeln!(stdin)?;
            write_tree_leaves(&mut stdin, leaves, &batch_blob_marks)?;
            writeln!(stdin, "done")?;
            stdin.flush()?;
            cmark
        };

        drop(stdin);
        let out = child.wait_with_output()?;
        ensure_git_ok("fast-import (tree staging)", &out)?;
        staging_refs.push(stage_ref);

        let marks = parse_marks_file(marks_path)?;
        let stage_commit_oid =
            marks
                .get(&commit_mark)
                .copied()
                .ok_or_else(|| D6StoreError::GitBorder {
                    command: "fast-import tree staging".to_string(),
                    detail: format!("missing mark :{commit_mark}"),
                })?;
        self.rev_parse_tree(mirror_dir, stage_commit_oid)
    }

    fn write_verbatim_commit(
        &self,
        mirror_dir: &Path,
        snap: &Snapshot,
        tree_oid: GitOid,
        commit_oids: &HashMap<Digest, GitOid>,
    ) -> Result<GitOid, D6StoreError> {
        let mut raw = Vec::with_capacity(64 + snap.parents.len() * 48 + snap.commit_body.len());
        let _ = writeln!(raw, "tree {tree_oid}");
        for p in &snap.parents {
            let p_oid = commit_oids.get(p).ok_or_else(|| {
                D6StoreError::InvalidInput(format!("missing exported parent {p}"))
            })?;
            let _ = writeln!(raw, "parent {p_oid}");
        }
        raw.extend_from_slice(&snap.commit_body);

        let out = run_with_stdin(
            self.git_bare(mirror_dir)
                .args(["hash-object", "-t", "commit", "-w", "--stdin"]),
            &raw,
        )?;
        ensure_git_ok("hash-object -t commit", &out)?;
        let hex = String::from_utf8_lossy(&out.stdout).trim().to_string();
        GitOid::parse_hex(&hex).map_err(D6StoreError::from)
    }

    fn rev_parse_commit(&self, mirror_dir: &Path, rev: &str) -> Result<GitOid, D6StoreError> {
        let arg = format!("{rev}^{{commit}}");
        let out = self
            .git_bare(mirror_dir)
            .args(["rev-parse", "--verify", &arg])
            .output()?;
        ensure_git_ok("rev-parse commit", &out)?;
        let hex = String::from_utf8_lossy(&out.stdout).trim().to_string();
        GitOid::parse_hex(&hex).map_err(D6StoreError::from)
    }

    fn rev_parse_tree(
        &self,
        mirror_dir: &Path,
        commit_oid: GitOid,
    ) -> Result<GitOid, D6StoreError> {
        let arg = format!("{commit_oid}^{{tree}}");
        let out = self
            .git_bare(mirror_dir)
            .args(["rev-parse", "--verify", &arg])
            .output()?;
        ensure_git_ok("rev-parse tree", &out)?;
        let hex = String::from_utf8_lossy(&out.stdout).trim().to_string();
        GitOid::parse_hex(&hex).map_err(D6StoreError::from)
    }

    /// Import all branch and tag history from an existing bare Git repository `source_repo`
    /// into [`CasMerkleStore`] (`NS_BLOB` + `NS_TREE`), sync CAS, record snapshots/refs in
    /// `ledger`, append `refs.journal`, and verify every ref `git_oid == git rev-parse`.
    pub fn import_bare_repo(
        &self,
        store: &CasMerkleStore,
        ledger: &dyn RefLedger,
        project: &str,
        source_repo: &Path,
        reason: &str,
    ) -> Result<ImportReport, D6StoreError> {
        let raw_refs = self.list_bare_commit_refs(source_repo)?;
        if raw_refs.is_empty() {
            return Ok(ImportReport {
                source_repo: source_repo.to_path_buf(),
                snapshots_imported: 0,
                refs: Vec::new(),
            });
        }

        let commit_order = self.topo_rev_list(source_repo, &raw_refs)?;
        let mut blob_cache: HashMap<(GitOid, bool), (Digest, u64)> = HashMap::new();
        let mut tree_cache: HashMap<GitOid, Digest> = HashMap::new();
        let mut commit_map: HashMap<GitOid, (Digest, Digest, Vec<Digest>, i64)> = HashMap::new();

        for commit_oid in &commit_order {
            let parsed = self.read_git_commit(source_repo, *commit_oid)?;
            let tree_digest = self.import_git_tree_recursive(
                store,
                source_repo,
                parsed.tree_oid,
                &mut blob_cache,
                &mut tree_cache,
            )?;

            let expected_tree_oid = store.project_tree_oid(&tree_digest)?;
            if expected_tree_oid != parsed.tree_oid {
                return Err(D6StoreError::OidMismatch {
                    what: "tree",
                    expected: parsed.tree_oid.to_hex(),
                    actual: expected_tree_oid.to_hex(),
                });
            }

            let mut parent_digests = Vec::with_capacity(parsed.parent_oids.len());
            for p_oid in &parsed.parent_oids {
                let (p_digest, ..) = commit_map.get(p_oid).ok_or_else(|| {
                    D6StoreError::InvalidInput(format!("missing imported parent commit {p_oid}"))
                })?;
                parent_digests.push(p_digest.clone());
            }

            let snapshot = Snapshot {
                tree: tree_digest.clone(),
                parents: parent_digests.clone(),
                commit_body: parsed.commit_body,
            };
            let snap_id = store.cas().put_merkle_snapshot(&snapshot)?;
            let projected_oid = store.project_snapshot_oid(&snapshot, &parsed.parent_oids)?;
            if projected_oid != *commit_oid {
                return Err(D6StoreError::OidMismatch {
                    what: "commit",
                    expected: commit_oid.to_hex(),
                    actual: projected_oid.to_hex(),
                });
            }

            commit_map.insert(
                *commit_oid,
                (snap_id, tree_digest, parent_digests, parsed.committer_ms),
            );
        }

        // Durability barrier before publishing snapshots/refs to PG ledger & refs.journal.
        store.cas().sync_store()?;

        for commit_oid in &commit_order {
            let (snap_id, tree_digest, parents, at_ms) = &commit_map[commit_oid];
            let _ = ledger.record_snapshot(
                snap_id,
                commit_oid,
                tree_digest,
                parents,
                project,
                *at_ms,
            )?;
        }

        let imported_refs = self.publish_imported_refs(
            store,
            ledger,
            project,
            source_repo,
            reason,
            &raw_refs,
            &commit_map,
        )?;

        Ok(ImportReport {
            source_repo: source_repo.to_path_buf(),
            snapshots_imported: commit_order.len(),
            refs: imported_refs,
        })
    }

    #[expect(clippy::too_many_arguments)]
    fn publish_imported_refs(
        &self,
        store: &CasMerkleStore,
        ledger: &dyn RefLedger,
        project: &str,
        source_repo: &Path,
        reason: &str,
        raw_refs: &[SourceRef],
        commit_map: &HashMap<GitOid, (Digest, Digest, Vec<Digest>, i64)>,
    ) -> Result<Vec<ImportedRef>, D6StoreError> {
        let existing_refs: HashMap<String, LedgerRef> = ledger
            .list_refs(Some(project))?
            .into_iter()
            .map(|r| (r.name.clone(), r))
            .collect();

        let mut imported_refs = Vec::with_capacity(raw_refs.len());
        for raw in raw_refs {
            let (snap_id, _, _, at_ms) = &commit_map[&raw.commit_oid];
            let expected = existing_refs
                .get(&raw.ledger_name)
                .map(|r| r.snapshot.clone());
            if expected.as_ref() == Some(snap_id) {
                let verified_oid = self.rev_parse_commit(source_repo, &raw.git_ref)?;
                if verified_oid != raw.commit_oid {
                    return Err(D6StoreError::OidMismatch {
                        what: "ref",
                        expected: raw.commit_oid.to_hex(),
                        actual: verified_oid.to_hex(),
                    });
                }
                imported_refs.push(ImportedRef {
                    git_ref: raw.git_ref.clone(),
                    ledger_name: raw.ledger_name.clone(),
                    kind: raw.kind,
                    snapshot: snap_id.clone(),
                    git_oid: verified_oid,
                    ref_seq: 0,
                });
                continue;
            }

            let seq = match raw.kind {
                D6RefKind::Main => ledger.land_main_ref(
                    project,
                    &raw.ledger_name,
                    expected.as_ref(),
                    snap_id,
                    None,
                    reason,
                )?,
                D6RefKind::Branch | D6RefKind::Tag | D6RefKind::Wip => ledger.cas_ref(
                    project,
                    &raw.ledger_name,
                    raw.kind,
                    expected.as_ref(),
                    snap_id,
                    reason,
                )?,
            };

            store.cas().append_ref_journal(&JournalEntry {
                seq,
                project: project.to_string(),
                name: raw.ledger_name.clone(),
                kind: raw.kind.as_str().to_string(),
                old_snapshot: expected,
                new_snapshot: snap_id.clone(),
                git_oid: raw.commit_oid,
                at_ms: *at_ms,
            })?;

            let verified_oid = self.rev_parse_commit(source_repo, &raw.git_ref)?;
            if verified_oid != raw.commit_oid {
                return Err(D6StoreError::OidMismatch {
                    what: "ref",
                    expected: raw.commit_oid.to_hex(),
                    actual: verified_oid.to_hex(),
                });
            }

            imported_refs.push(ImportedRef {
                git_ref: raw.git_ref.clone(),
                ledger_name: raw.ledger_name.clone(),
                kind: raw.kind,
                snapshot: snap_id.clone(),
                git_oid: verified_oid,
                ref_seq: seq,
            });
        }
        Ok(imported_refs)
    }

    fn list_bare_commit_refs(&self, repo: &Path) -> Result<Vec<SourceRef>, D6StoreError> {
        let out = self
            .git_bare(repo)
            .args([
                "for-each-ref",
                "--format=%(refname)%09%(objecttype)%09%(*objectname)%09%(objectname)%00",
                "refs/heads",
                "refs/tags",
            ])
            .output()?;
        ensure_git_ok("for-each-ref", &out)?;
        let mut items = Vec::new();
        for raw_rec in out.stdout.split(|&b| b == 0) {
            let rec = raw_rec.strip_prefix(b"\n").unwrap_or(raw_rec);
            if rec.is_empty() {
                continue;
            }
            let fields: Vec<&[u8]> = rec.splitn(4, |&b| b == b'\t').collect();
            if fields.len() != 4 {
                return Err(D6StoreError::InvalidInput(
                    "malformed for-each-ref record".to_string(),
                ));
            }
            let git_ref = std::str::from_utf8(fields[0])
                .map_err(|_| D6StoreError::InvalidInput("non-utf8 git refname".to_string()))?
                .trim()
                .to_string();
            let obj_type = std::str::from_utf8(fields[1]).unwrap_or("").trim();
            let peeled = std::str::from_utf8(fields[2]).unwrap_or("").trim();
            let direct = std::str::from_utf8(fields[3]).unwrap_or("").trim();

            let commit_hex = if obj_type == "tag" {
                let peeled_oid = GitOid::parse_hex(peeled)?;
                let kind_out = self
                    .git_bare(repo)
                    .args(["cat-file", "-t", peeled])
                    .output()?;
                ensure_git_ok("cat-file -t", &kind_out)?;
                if String::from_utf8_lossy(&kind_out.stdout).trim() != "commit" {
                    continue;
                }
                peeled_oid
            } else if obj_type == "commit" {
                GitOid::parse_hex(direct)?
            } else {
                continue;
            };

            let (ledger_name, kind) = if let Some(branch) = git_ref.strip_prefix("refs/heads/") {
                if branch == "main" {
                    ("main".to_string(), D6RefKind::Main)
                } else {
                    (branch.to_string(), D6RefKind::Branch)
                }
            } else if let Some(tag) = git_ref.strip_prefix("refs/tags/") {
                (format!("tags/{tag}"), D6RefKind::Tag)
            } else {
                continue;
            };

            items.push(SourceRef {
                git_ref,
                ledger_name,
                kind,
                commit_oid: commit_hex,
            });
        }
        Ok(items)
    }

    fn topo_rev_list(&self, repo: &Path, refs: &[SourceRef]) -> Result<Vec<GitOid>, D6StoreError> {
        let mut unique_tips = BTreeSet::new();
        for r in refs {
            unique_tips.insert(r.commit_oid.to_hex());
        }
        let mut cmd = self.git_bare(repo);
        cmd.args(["rev-list", "--topo-order", "--reverse"]);
        for tip in &unique_tips {
            cmd.arg(tip);
        }
        let out = cmd.output()?;
        ensure_git_ok("rev-list", &out)?;
        let mut commits = Vec::new();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                commits.push(GitOid::parse_hex(trimmed)?);
            }
        }
        Ok(commits)
    }

    fn read_git_commit(
        &self,
        repo: &Path,
        commit_oid: GitOid,
    ) -> Result<ParsedGitCommit, D6StoreError> {
        let out = self
            .git_bare(repo)
            .args(["cat-file", "commit", &commit_oid.to_hex()])
            .output()?;
        ensure_git_ok("cat-file commit", &out)?;
        let raw = out.stdout;
        let mut pos = 0_usize;

        let tree_line = next_line(&raw, &mut pos).ok_or_else(|| {
            D6StoreError::InvalidInput("missing tree line in git commit".to_string())
        })?;
        let tree_hex = std::str::from_utf8(tree_line.strip_prefix(b"tree ").ok_or_else(|| {
            D6StoreError::InvalidInput("expected tree header in git commit".to_string())
        })?)
        .map_err(|_| D6StoreError::InvalidInput("non-utf8 tree header".to_string()))?;
        let tree_oid = GitOid::parse_hex(tree_hex)?;

        let mut parent_oids = Vec::new();
        loop {
            let line_start = pos;
            let Some(line) = next_line(&raw, &mut pos) else {
                break;
            };
            if let Some(p_bytes) = line.strip_prefix(b"parent ") {
                let p_hex = std::str::from_utf8(p_bytes).map_err(|_| {
                    D6StoreError::InvalidInput("non-utf8 parent header".to_string())
                })?;
                parent_oids.push(GitOid::parse_hex(p_hex)?);
            } else {
                pos = line_start;
                break;
            }
        }

        let commit_body = raw[pos..].to_vec();
        let committer_ms = extract_committer_ms(&commit_body);
        Ok(ParsedGitCommit {
            tree_oid,
            parent_oids,
            commit_body,
            committer_ms,
        })
    }

    fn import_git_tree_recursive(
        &self,
        store: &CasMerkleStore,
        repo: &Path,
        tree_oid: GitOid,
        blob_cache: &mut HashMap<(GitOid, bool), (Digest, u64)>,
        tree_cache: &mut HashMap<GitOid, Digest>,
    ) -> Result<Digest, D6StoreError> {
        if let Some(cached) = tree_cache.get(&tree_oid) {
            return Ok(cached.clone());
        }
        let out = self
            .git_bare(repo)
            .args(["ls-tree", "-z", &tree_oid.to_hex()])
            .output()?;
        ensure_git_ok("ls-tree -z", &out)?;

        let mut entries = Vec::new();
        for record in nul_fields(&out.stdout) {
            let tab_pos = record.iter().position(|&b| b == b'\t').ok_or_else(|| {
                D6StoreError::InvalidInput("malformed ls-tree record missing tab".to_string())
            })?;
            let meta = std::str::from_utf8(&record[..tab_pos])
                .map_err(|_| D6StoreError::InvalidInput("non-utf8 ls-tree header".to_string()))?;
            let name = record[tab_pos + 1..].to_vec();
            let parts: Vec<&str> = meta.split(' ').collect();
            if parts.len() != 3 {
                return Err(D6StoreError::InvalidInput(format!(
                    "unexpected ls-tree metadata: {meta:?}"
                )));
            }
            let (mode, obj_type, oid_hex) = (parts[0], parts[1], parts[2]);
            let obj_oid = GitOid::parse_hex(oid_hex)?;

            match (mode, obj_type) {
                ("040000", "tree") => {
                    let sub_digest = self
                        .import_git_tree_recursive(store, repo, obj_oid, blob_cache, tree_cache)?;
                    entries.push(TreeEntry::dir(name, &sub_digest));
                }
                ("160000", "commit") => {
                    entries.push(TreeEntry::gitlink(name, obj_oid.as_bytes())?);
                }
                ("100644" | "100755" | "120000", "blob") => {
                    let allow_pointer = mode != "120000";
                    let cache_key = (obj_oid, allow_pointer);
                    let (digest, size) = if let Some(hit) = blob_cache.get(&cache_key) {
                        hit.clone()
                    } else {
                        let imported =
                            self.import_git_blob(store.cas(), repo, obj_oid, allow_pointer)?;
                        blob_cache.insert(cache_key, imported.clone());
                        imported
                    };
                    let entry = match mode {
                        "100644" => TreeEntry::file(name, &digest, size),
                        "100755" => TreeEntry::executable(name, &digest, size),
                        "120000" => TreeEntry::symlink(name, &digest, size),
                        _ => unreachable!(),
                    };
                    entries.push(entry);
                }
                _ => {
                    return Err(D6StoreError::InvalidInput(format!(
                        "unsupported ls-tree entry mode={mode} type={obj_type}"
                    )));
                }
            }
        }

        let tree = Tree::from_entries(entries)?;
        let digest = store.put_tree_cached(&tree)?;
        tree_cache.insert(tree_oid, digest.clone());
        Ok(digest)
    }

    fn import_git_blob(
        &self,
        cas: &Store,
        repo: &Path,
        blob_oid: GitOid,
        allow_pointer_resolution: bool,
    ) -> Result<(Digest, u64), D6StoreError> {
        let hex = blob_oid.to_hex();
        let size_out = self
            .git_bare(repo)
            .args(["cat-file", "-s", &hex])
            .output()?;
        ensure_git_ok("cat-file -s", &size_out)?;
        let git_size: u64 = String::from_utf8_lossy(&size_out.stdout)
            .trim()
            .parse()
            .map_err(|_| D6StoreError::InvalidInput("invalid git blob size".to_string()))?;

        let mut child = self
            .git_bare(repo)
            .args(["cat-file", "blob", &hex])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| D6StoreError::InvalidInput("missing cat-file stdout".to_string()))?;

        if allow_pointer_resolution && git_size <= 256 {
            let mut small = Vec::new();
            stdout.read_to_end(&mut small)?;
            let out = child.wait_with_output()?;
            ensure_git_ok("cat-file blob", &out)?;
            if let Some((ptr_digest, ptr_size)) = parse_pointer_bytes(&small)
                && ptr_size > GIT_POINTER_THRESHOLD
                && cas.has_blob(&ptr_digest)
            {
                let verified_len = cas.stream_blob(&ptr_digest, &mut io::sink())?;
                if verified_len == ptr_size {
                    cas.touch(NS_BLOB, &ptr_digest)?;
                    return Ok((ptr_digest, ptr_size));
                }
            }
            let stored = cas.put_blob(NS_BLOB, &mut small.as_slice())?;
            return Ok((stored.digest, stored.size));
        }

        let mut reader = BufReader::new(stdout);
        let stored_res = cas.put_blob(NS_BLOB, &mut reader);
        drop(reader);
        let out = child.wait_with_output()?;
        ensure_git_ok("cat-file blob", &out)?;
        let stored = stored_res?;
        if stored.size != git_size {
            return Err(D6StoreError::InvalidInput(format!(
                "blob {hex} streamed {} bytes vs git size {git_size}",
                stored.size
            )));
        }
        Ok((stored.digest, stored.size))
    }
}

struct ExportPlan {
    topo: Vec<Digest>,
    snapshots: HashMap<Digest, Snapshot>,
    expected_trees: HashMap<Digest, GitOid>,
    expected_commits: HashMap<Digest, GitOid>,
    tree_leaves: HashMap<Digest, BTreeMap<Vec<u8>, TreeEntry>>,
}

fn build_export_plan(
    store: &CasMerkleStore,
    ledger: Option<&dyn RefLedger>,
    targets: &[BorderRefTarget],
) -> Result<ExportPlan, D6StoreError> {
    let mut visited = HashMap::<Digest, Snapshot>::new();
    let mut stack = targets
        .iter()
        .map(|t| t.snapshot.clone())
        .collect::<VecDeque<_>>();

    while let Some(id) = stack.pop_front() {
        if visited.contains_key(&id) {
            continue;
        }
        let snap = store.cas().read_merkle_snapshot(&id)?;
        for parent in &snap.parents {
            if !visited.contains_key(parent) {
                stack.push_back(parent.clone());
            }
        }
        visited.insert(id, snap);
    }

    let mut topo = Vec::with_capacity(visited.len());
    let mut perm = HashMap::<Digest, u8>::new();
    for t in targets {
        dfs_topo(&t.snapshot, &visited, &mut perm, &mut topo)?;
    }

    let mut expected_trees = HashMap::new();
    let mut expected_commits = HashMap::new();
    let mut tree_leaves = HashMap::new();
    for id in &topo {
        let snap = &visited[id];
        if !tree_leaves.contains_key(&snap.tree) {
            let leaves = store.flatten_tree(&snap.tree)?;
            tree_leaves.insert(snap.tree.clone(), leaves);
        }
        let tree_oid = store.project_tree_oid(&snap.tree)?;
        let mut p_oids = Vec::with_capacity(snap.parents.len());
        for p in &snap.parents {
            p_oids.push(expected_commits[p]);
        }
        let commit_oid = store.project_snapshot_oid(snap, &p_oids)?;

        if let Some(ledger_ref) = ledger
            && let Some(row) = ledger_ref.get_snapshot(id)?
            && (row.tree != snap.tree || row.git_oid != commit_oid)
        {
            return Err(D6StoreError::OidMismatch {
                what: "ledger_snapshot",
                expected: commit_oid.to_hex(),
                actual: row.git_oid.to_hex(),
            });
        }
        expected_trees.insert(id.clone(), tree_oid);
        expected_commits.insert(id.clone(), commit_oid);
    }

    Ok(ExportPlan {
        topo,
        snapshots: visited,
        expected_trees,
        expected_commits,
        tree_leaves,
    })
}

fn dfs_topo(
    id: &Digest,
    snapshots: &HashMap<Digest, Snapshot>,
    state: &mut HashMap<Digest, u8>,
    out: &mut Vec<Digest>,
) -> Result<(), D6StoreError> {
    match state.get(id).copied() {
        Some(2) => return Ok(()),
        Some(1) => {
            return Err(D6StoreError::InvalidInput(format!(
                "cycle detected in snapshot ancestry at {id}"
            )));
        }
        _ => {}
    }
    state.insert(id.clone(), 1);
    let snap = snapshots
        .get(id)
        .ok_or_else(|| D6StoreError::InvalidInput(format!("missing snapshot {id}")))?;
    for parent in &snap.parents {
        dfs_topo(parent, snapshots, state, out)?;
    }
    state.insert(id.clone(), 2);
    out.push(id.clone());
    Ok(())
}

struct StandardCommit<'a> {
    author: &'a str,
    committer: &'a str,
    encoding: Option<&'a str>,
    message: &'a [u8],
}

fn parse_standard_commit_body(body: &[u8]) -> Option<StandardCommit<'_>> {
    let sep = body.windows(2).position(|w| w == b"\n\n")?;
    let headers_raw = &body[..sep];
    let message = &body[sep + 2..];
    let headers_str = std::str::from_utf8(headers_raw).ok()?;
    let mut lines = headers_str.split('\n');
    let author_line = lines.next()?.strip_prefix("author ")?;
    let committer_line = lines.next()?.strip_prefix("committer ")?;
    if !is_fast_import_ident(author_line) || !is_fast_import_ident(committer_line) {
        return None;
    }
    let encoding = match lines.next() {
        Some(line) => Some(line.strip_prefix("encoding ")?),
        None => None,
    };
    if lines.next().is_some() {
        return None;
    }
    if encoding.is_some_and(|e| {
        e.is_empty()
            || e.contains('\r')
            || e.contains('\n')
            || e.starts_with(' ')
            || e.ends_with(' ')
    }) {
        return None;
    }
    Some(StandardCommit {
        author: author_line,
        committer: committer_line,
        encoding,
        message,
    })
}

fn is_fast_import_ident(ident: &str) -> bool {
    if ident.contains('\r')
        || ident.contains('\n')
        || ident.starts_with(' ')
        || ident.ends_with(' ')
    {
        return false;
    }
    let Some(lt) = ident.find('<') else {
        return false;
    };
    let Some(gt) = ident.rfind('>') else {
        return false;
    };
    if lt >= gt {
        return false;
    }
    let email = &ident[lt + 1..gt];
    if email.contains('<') || email.contains('>') {
        return false;
    }
    let Some(after_gt) = ident.get(gt + 1..).and_then(|s| s.strip_prefix(' ')) else {
        return false;
    };
    let Some((secs, tz)) = after_gt.split_once(' ') else {
        return false;
    };
    !secs.is_empty()
        && secs.bytes().all(|b| b.is_ascii_digit())
        && tz.len() == 5
        && matches!(tz.as_bytes()[0], b'+' | b'-')
        && tz.as_bytes()[1..].iter().all(u8::is_ascii_digit)
}

fn write_blob_command(
    stdin: &mut impl Write,
    store: &CasMerkleStore,
    mark: usize,
    digest: &Digest,
    size: u64,
    pointer: bool,
) -> Result<(), D6StoreError> {
    writeln!(stdin, "blob")?;
    writeln!(stdin, "mark :{mark}")?;
    if pointer {
        let ptr = git_pointer_bytes(digest, size);
        writeln!(stdin, "data {}", ptr.len())?;
        stdin.write_all(&ptr)?;
    } else {
        writeln!(stdin, "data {size}")?;
        let streamed = store.cas().stream_blob(digest, stdin)?;
        if streamed != size {
            return Err(D6StoreError::InvalidInput(format!(
                "blob {digest} size {streamed} != tree entry size {size}"
            )));
        }
    }
    stdin.write_all(b"\n")?;
    Ok(())
}

fn write_parent_ref(
    stdin: &mut impl Write,
    keyword: &str,
    parent: &Digest,
    batch_marks: &HashMap<Digest, usize>,
    resolved_oids: &HashMap<Digest, GitOid>,
) -> Result<(), D6StoreError> {
    if let Some(mark) = batch_marks.get(parent) {
        writeln!(stdin, "{keyword} :{mark}")?;
    } else if let Some(oid) = resolved_oids.get(parent) {
        writeln!(stdin, "{keyword} {oid}")?;
    } else {
        return Err(D6StoreError::InvalidInput(format!(
            "parent {parent} missing from topological export"
        )));
    }
    Ok(())
}

fn write_tree_leaves(
    stdin: &mut impl Write,
    leaves: &BTreeMap<Vec<u8>, TreeEntry>,
    blob_marks: &HashMap<(Digest, bool), usize>,
) -> Result<(), D6StoreError> {
    writeln!(stdin, "deleteall")?;
    for (path_bytes, entry) in leaves {
        let quoted_path = format_fast_import_path(path_bytes);
        match entry.kind {
            TreeEntryKind::Gitlink => {
                let raw_oid = entry.gitlink_oid().ok_or_else(|| {
                    D6StoreError::InvalidInput("gitlink entry missing OID".to_string())
                })?;
                if raw_oid.len() != 20 {
                    return Err(D6StoreError::InvalidInput(
                        "git border requires 20-byte SHA-1 gitlink OID".to_string(),
                    ));
                }
                let mut arr = [0_u8; 20];
                arr.copy_from_slice(raw_oid);
                let oid = GitOid::from_bytes(arr);
                writeln!(stdin, "M 160000 {oid} {quoted_path}")?;
            }
            TreeEntryKind::File | TreeEntryKind::Executable | TreeEntryKind::Symlink => {
                let mode = match entry.kind {
                    TreeEntryKind::File => "100644",
                    TreeEntryKind::Executable => "100755",
                    TreeEntryKind::Symlink => "120000",
                    TreeEntryKind::Dir | TreeEntryKind::Gitlink => unreachable!(),
                };
                let digest = entry.digest().ok_or_else(|| {
                    D6StoreError::InvalidInput("leaf entry missing digest".to_string())
                })?;
                let mark = blob_marks
                    .get(&(digest, is_pointer_sized(entry)))
                    .ok_or_else(|| D6StoreError::InvalidInput("missing blob mark".to_string()))?;
                writeln!(stdin, "M {mode} :{mark} {quoted_path}")?;
            }
            TreeEntryKind::Dir => {}
        }
    }
    Ok(())
}

fn format_fast_import_path(bytes: &[u8]) -> String {
    let needs_quote = bytes.starts_with(b"\"")
        || bytes
            .iter()
            .any(|&b| !(0x20..0x7f).contains(&b) || b == b'\\' || b == b'"');
    if !needs_quote {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut out = String::with_capacity(bytes.len() + 8);
    out.push('"');
    for &b in bytes {
        match b {
            b'"' => out.push_str("\\\""),
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            0x00..=0x1f | 0x7f..=0xff => {
                let _ = write!(out, "\\{b:03o}");
            }
            _ => out.push(char::from(b)),
        }
    }
    out.push('"');
    out
}

#[expect(clippy::too_many_arguments)]
fn write_fast_import_batch_stream(
    stdin: &mut impl Write,
    store: &CasMerkleStore,
    plan: &ExportPlan,
    slice: &[Digest],
    stage_prefix: &str,
    stage_counter: &mut usize,
    commit_oids: &HashMap<Digest, GitOid>,
    exported_blobs: &mut BTreeSet<(Digest, bool)>,
    staging_refs: &mut Vec<String>,
) -> Result<Vec<(Digest, usize)>, D6StoreError> {
    let mut batch_blob_marks: HashMap<(Digest, bool), usize> = HashMap::new();
    let mut next_mark = 1_usize;

    for snap_id in slice {
        let snap = &plan.snapshots[snap_id];
        let leaves = &plan.tree_leaves[&snap.tree];
        for entry in leaves.values() {
            if entry.kind == TreeEntryKind::Gitlink {
                continue;
            }
            let Some(digest) = entry.digest() else {
                continue;
            };
            let key = (digest.clone(), is_pointer_sized(entry));
            if batch_blob_marks.contains_key(&key) {
                continue;
            }
            let mark = next_mark;
            next_mark += 1;
            write_blob_command(stdin, store, mark, &key.0, entry.size, key.1)?;
            batch_blob_marks.insert(key.clone(), mark);
            exported_blobs.insert(key);
        }
    }

    let mut batch_commit_marks: HashMap<Digest, usize> = HashMap::new();
    let mut commit_list = Vec::with_capacity(slice.len());
    for snap_id in slice {
        let snap = &plan.snapshots[snap_id];
        let parsed = parse_standard_commit_body(&snap.commit_body).ok_or_else(|| {
            D6StoreError::InvalidInput("non-standard commit in fast-import batch".to_string())
        })?;

        *stage_counter += 1;
        let stage_ref = format!("{stage_prefix}/{}", *stage_counter);
        let mark = next_mark;
        next_mark += 1;

        writeln!(stdin, "commit {stage_ref}")?;
        writeln!(stdin, "mark :{mark}")?;
        writeln!(stdin, "author {}", parsed.author)?;
        writeln!(stdin, "committer {}", parsed.committer)?;
        if let Some(enc) = parsed.encoding {
            writeln!(stdin, "encoding {enc}")?;
        }
        writeln!(stdin, "data {}", parsed.message.len())?;
        stdin.write_all(parsed.message)?;
        stdin.write_all(b"\n")?;

        if let Some((first_parent, rest_parents)) = snap.parents.split_first() {
            write_parent_ref(
                stdin,
                "from",
                first_parent,
                &batch_commit_marks,
                commit_oids,
            )?;
            for merge_parent in rest_parents {
                write_parent_ref(
                    stdin,
                    "merge",
                    merge_parent,
                    &batch_commit_marks,
                    commit_oids,
                )?;
            }
        }

        let same_tree_as_first_parent = snap
            .parents
            .first()
            .and_then(|p| plan.snapshots.get(p))
            .is_some_and(|p_snap| p_snap.tree == snap.tree);

        if !same_tree_as_first_parent {
            let leaves = &plan.tree_leaves[&snap.tree];
            write_tree_leaves(stdin, leaves, &batch_blob_marks)?;
        }
        stdin.write_all(b"\n")?;

        batch_commit_marks.insert(snap_id.clone(), mark);
        staging_refs.push(stage_ref);
        commit_list.push((snap_id.clone(), mark));
    }
    writeln!(stdin, "done")?;
    stdin.flush()?;
    Ok(commit_list)
}

fn temp_marks_path() -> PathBuf {
    let n = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("spawn-d6-marks-{}-{n}.marks", std::process::id()))
}

fn parse_marks_file(path: &Path) -> Result<HashMap<usize, GitOid>, D6StoreError> {
    let text = fs::read_to_string(path)?;
    let mut out = HashMap::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some((mark_str, oid_str)) = trimmed.split_once(' ') else {
            return Err(D6StoreError::InvalidInput(format!(
                "malformed fast-import mark line: {trimmed:?}"
            )));
        };
        let mark_num: usize = mark_str
            .strip_prefix(':')
            .ok_or_else(|| D6StoreError::InvalidInput(format!("bad mark token {mark_str:?}")))?
            .parse()
            .map_err(|_| D6StoreError::InvalidInput(format!("bad mark int {mark_str:?}")))?;
        let oid = GitOid::parse_hex(oid_str)?;
        out.insert(mark_num, oid);
    }
    Ok(out)
}

fn default_head_ref(refs: &BTreeMap<String, GitOid>) -> Option<&str> {
    if refs.contains_key("refs/heads/main") {
        return Some("refs/heads/main");
    }
    refs.keys()
        .find(|name| name.starts_with("refs/heads/"))
        .map(String::as_str)
}

fn validate_git_ref(git_ref: &str) -> Result<(), D6StoreError> {
    if !git_ref.starts_with("refs/")
        || git_ref.contains('\n')
        || git_ref.contains('\r')
        || git_ref.contains('\0')
        || git_ref.contains(' ')
        || git_ref.contains("..")
    {
        return Err(D6StoreError::InvalidInput(format!(
            "invalid git border ref {git_ref:?}"
        )));
    }
    Ok(())
}

fn parse_pointer_bytes(bytes: &[u8]) -> Option<(Digest, u64)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let rest = text.strip_prefix("spawn-pointer-v1\n")?;
    let mut lines = rest.split('\n');
    let digest_line = lines.next()?;
    let size_line = lines.next()?;
    let trailing = lines.next()?;
    if !trailing.is_empty() || lines.next().is_some() {
        return None;
    }
    let digest = Digest::parse(digest_line).ok()?;
    let size_str = size_line.strip_prefix("size ")?;
    let size: u64 = size_str.parse().ok()?;
    if git_pointer_bytes(&digest, size) != bytes {
        return None;
    }
    Some((digest, size))
}

fn next_line<'a>(bytes: &'a [u8], pos: &mut usize) -> Option<&'a [u8]> {
    let rest = bytes.get(*pos..)?;
    if rest.is_empty() {
        return None;
    }
    let end = rest.iter().position(|&b| b == b'\n').unwrap_or(rest.len());
    let line = &rest[..end];
    *pos += if *pos + end < bytes.len() {
        end + 1
    } else {
        end
    };
    Some(line)
}

fn extract_committer_ms(commit_body: &[u8]) -> i64 {
    let text = String::from_utf8_lossy(commit_body);
    let headers = text.split("\n\n").next().unwrap_or("");
    for line in headers.lines() {
        if let Some(rest) = line.strip_prefix("committer ")
            && let Some(gt) = rest.rfind('>')
        {
            let mut parts = rest[gt + 1..].split_ascii_whitespace();
            if let Some(secs_str) = parts.next()
                && let Ok(secs) = secs_str.parse::<i64>()
            {
                return secs.saturating_mul(1000);
            }
        }
    }
    0
}

fn run_with_stdin(
    cmd: &mut Command,
    stdin_bytes: &[u8],
) -> Result<std::process::Output, D6StoreError> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(stdin_bytes)?;
        stdin.flush()?;
    }
    Ok(child.wait_with_output()?)
}

fn ensure_git_ok(action: &str, out: &std::process::Output) -> Result<(), D6StoreError> {
    if out.status.success() {
        return Ok(());
    }
    Err(D6StoreError::GitBorder {
        command: action.to_string(),
        detail: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    })
}

struct SourceRef {
    git_ref: String,
    ledger_name: String,
    kind: D6RefKind,
    commit_oid: GitOid,
}

struct ParsedGitCommit {
    tree_oid: GitOid,
    parent_oids: Vec<GitOid>,
    commit_body: Vec<u8>,
    committer_ms: i64,
}
