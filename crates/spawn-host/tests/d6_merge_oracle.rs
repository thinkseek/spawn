//! D6 Phase 4 Step 3 test suite & historical merge Oracle (`#113` Comment #5):
//! - Synthetic 3-way Merkle merges (`O(1)` subtree skip, `diff3` byte parity with
//!   `git -c merge.conflictStyle=diff3 merge-tree --write-tree --merge-base=...`,
//!   exact-content rename resolution, `.gitattributes`, modes/symlinks/gitlinks/dir-file,
//!   single merge-base guard, `Spawn-Head` first-parent merge snapshots)
//! - Full repository historical merge replay across `thinkseek/spawn`'s real
//!   two-parent merge commits asserting `BlockerRustCleanGitConflictOrTreeDiff == 0`; on a
//!   `git archive` export with no `.git` (`scripts/rsi/gate.sh`) the same oracle replays a
//!   self-contained synthetic history of >= 12 realistic merges instead.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use spawn_cas::{Digest, GitOid, Snapshot, Tree, TreeEntry, empty_tree_digest};
use spawn_host::{
    BorderRefTarget, CasMerkleStore, ConflictKind, ConflictLabels, D6RefKind, GitBorderAdapter,
    InMemoryRefLedger, MergeBaseError, MergeDriver, MergeOptions, MergeShadowOutcome,
    MergeSnapshotSpec, PublishRequest, ShadowOracleSummary, build_merge_snapshot,
    classify_shadow_merge, format_commit_body, has_conflict_block_sized, merge_snapshots,
    merge_text_3way, merge_trees, test_git_command, validate_single_merge_base,
};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "spawn-d6-oracle-{tag}-{}-{}",
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

fn git_in(dir: &Path, args: &[&str]) -> String {
    let out = test_git_command()
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed in {}: {}",
        args,
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn git_bytes(dir: &Path, args: &[&str]) -> Vec<u8> {
    let out = test_git_command()
        .current_dir(dir)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {:?} failed in {}: {}",
        args,
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

struct GitMergeTreeResult {
    clean: bool,
    tree_oid: GitOid,
    conflict_paths: Vec<String>,
}

fn run_git_merge_tree(
    repo: &Path,
    base_rev: &str,
    main_rev: &str,
    branch_rev: &str,
    diff3: bool,
) -> GitMergeTreeResult {
    let mut cmd = test_git_command();
    cmd.arg("-C").arg(repo);
    if diff3 {
        cmd.args(["-c", "merge.conflictStyle=diff3"]);
    }
    cmd.arg(format!("--attr-source={main_rev}"))
        .args(["merge-tree", "--write-tree", "--name-only", "-z"])
        .arg(format!("--merge-base={base_rev}"))
        .args([main_rev, branch_rev]);
    let out = cmd.output().expect("run git merge-tree");
    let code = out.status.code().unwrap_or(2);
    assert!(
        code == 0 || code == 1,
        "git merge-tree unexpected exit {code}: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let mut fields = out.stdout.split(|&b| b == 0);
    let tree_hex = String::from_utf8_lossy(fields.next().unwrap_or_default())
        .trim()
        .to_string();
    let tree_oid = GitOid::parse_hex(&tree_hex).expect("merge-tree oid");
    let mut conflict_paths = Vec::new();
    if code == 1 {
        for f in fields {
            if f.is_empty() {
                break;
            }
            conflict_paths.push(String::from_utf8_lossy(f).into_owned());
        }
    }
    GitMergeTreeResult {
        clean: code == 0,
        tree_oid,
        conflict_paths,
    }
}

struct BatchGitReader {
    stdin: Option<BufWriter<ChildStdin>>,
    stdout: Option<BufReader<ChildStdout>>,
    child: Child,
}

impl BatchGitReader {
    fn open(repo: &Path) -> Self {
        let mut child = test_git_command()
            .arg("-C")
            .arg(repo)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn git cat-file --batch");
        let stdin = Some(BufWriter::new(child.stdin.take().expect("stdin")));
        let stdout = Some(BufReader::new(child.stdout.take().expect("stdout")));
        Self {
            stdin,
            stdout,
            child,
        }
    }

    fn read_object(&mut self, oid: GitOid) -> (String, Vec<u8>) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        let stdout = self.stdout.as_mut().expect("stdout open");
        writeln!(stdin, "{oid}").expect("write oid");
        stdin.flush().expect("flush batch");
        let mut header = String::new();
        stdout.read_line(&mut header).expect("read header");
        let parts: Vec<&str> = header.trim_end().split(' ').collect();
        assert!(
            parts.len() == 3,
            "unexpected cat-file --batch header for {oid}: {header:?}"
        );
        let kind = parts[1].to_string();
        let size: usize = parts[2].parse().expect("object size");
        let mut data = vec![0_u8; size];
        stdout.read_exact(&mut data).expect("read object body");
        let mut nl = [0_u8; 1];
        stdout.read_exact(&mut nl).expect("read trailing nl");
        (kind, data)
    }

    fn import_tree(
        &mut self,
        store: &CasMerkleStore,
        tree_oid: GitOid,
        tree_cache: &mut HashMap<GitOid, Digest>,
        blob_cache: &mut HashMap<GitOid, (Digest, u64)>,
    ) -> Digest {
        if let Some(hit) = tree_cache.get(&tree_oid) {
            return hit.clone();
        }
        let (kind, raw) = self.read_object(tree_oid);
        assert_eq!(kind, "tree", "expected tree object for {tree_oid}");

        let mut entries = Vec::new();
        let mut pos = 0_usize;
        while pos < raw.len() {
            let sp = raw[pos..]
                .iter()
                .position(|&b| b == b' ')
                .expect("space in tree entry")
                + pos;
            let mode = std::str::from_utf8(&raw[pos..sp]).expect("ascii mode");
            let nul = raw[sp + 1..]
                .iter()
                .position(|&b| b == 0)
                .expect("nul in tree entry")
                + sp
                + 1;
            let name = raw[sp + 1..nul].to_vec();
            let mut oid_bytes = [0_u8; 20];
            oid_bytes.copy_from_slice(&raw[nul + 1..nul + 21]);
            let obj_oid = GitOid::from_bytes(oid_bytes);
            pos = nul + 21;

            match mode {
                "40000" | "040000" => {
                    let sub_digest = self.import_tree(store, obj_oid, tree_cache, blob_cache);
                    entries.push(TreeEntry::dir(name, &sub_digest));
                }
                "160000" => {
                    entries.push(TreeEntry::gitlink(name, obj_oid.as_bytes()).expect("gitlink"));
                }
                "100644" | "100755" | "120000" => {
                    let (digest, size) = if let Some(hit) = blob_cache.get(&obj_oid) {
                        hit.clone()
                    } else {
                        let (b_kind, b_data) = self.read_object(obj_oid);
                        assert_eq!(b_kind, "blob", "expected blob object for {obj_oid}");
                        let stored = store.put_bytes_blob(&b_data).expect("put blob");
                        blob_cache.insert(obj_oid, stored.clone());
                        stored
                    };
                    let entry = match mode {
                        "100644" => TreeEntry::file(name, &digest, size),
                        "100755" => TreeEntry::executable(name, &digest, size),
                        "120000" => TreeEntry::symlink(name, &digest, size),
                        _ => unreachable!(),
                    };
                    entries.push(entry);
                }
                other => panic!("unsupported git tree mode {other:?}"),
            }
        }

        let tree = Tree::from_entries(entries).expect("canonical tree");
        let digest = store.put_tree_cached(&tree).expect("store tree");
        tree_cache.insert(tree_oid, digest.clone());
        digest
    }
}

impl Drop for BatchGitReader {
    fn drop(&mut self) {
        drop(self.stdin.take());
        drop(self.stdout.take());
        let _ = self.child.wait();
    }
}

fn build_synthetic_repo(scratch: &Scratch) -> (PathBuf, CasMerkleStore, GitBorderAdapter) {
    let repo = scratch.join("repo");
    fs::create_dir_all(&repo).expect("mkdir repo");
    git_in(&repo, &["init", "-b", "main"]);
    let cas_store = CasMerkleStore::open(scratch.join("cas")).expect("open cas");
    let adapter = GitBorderAdapter::new();
    (repo, cas_store, adapter)
}

fn import_commits_to_cas(
    repo: &Path,
    cas_store: &CasMerkleStore,
    revs: &[&str],
) -> Vec<(Digest, GitOid, Digest)> {
    let mut reader = BatchGitReader::open(repo);
    let mut tree_cache = HashMap::new();
    let mut blob_cache = HashMap::new();
    revs.iter()
        .map(|rev| {
            let tree_hex = git_in(repo, &["rev-parse", &format!("{rev}^{{tree}}")])
                .trim()
                .to_string();
            let commit_hex = git_in(repo, &["rev-parse", rev]).trim().to_string();
            let tree_oid = GitOid::parse_hex(&tree_hex).expect("tree oid");
            let commit_oid = GitOid::parse_hex(&commit_hex).expect("commit oid");
            let tree_digest =
                reader.import_tree(cas_store, tree_oid, &mut tree_cache, &mut blob_cache);
            (tree_digest, commit_oid, empty_tree_digest())
        })
        .collect()
}

#[test]
fn disjoint_files_and_subtrees_merge_cleanly_with_o1_subtree_reuse() {
    let scratch = Scratch::new("disjoint");
    let (repo, cas_store, _adapter) = build_synthetic_repo(&scratch);

    fs::create_dir_all(repo.join("untouched/nested")).expect("mkdir untouched");
    fs::create_dir_all(repo.join("left")).expect("mkdir left");
    fs::create_dir_all(repo.join("right")).expect("mkdir right");
    fs::write(repo.join("untouched/nested/core.rs"), "pub fn core() {}\n").expect("core");
    fs::write(repo.join("left/a.txt"), "left v1\n").expect("left");
    fs::write(repo.join("right/b.txt"), "right v1\n").expect("right");
    git_in(&repo, &["add", "."]);
    git_in(&repo, &["commit", "-m", "base"]);
    let base_rev = git_in(&repo, &["rev-parse", "HEAD"]).trim().to_string();

    git_in(&repo, &["checkout", "-b", "branch"]);
    fs::write(repo.join("right/b.txt"), "right v2\n").expect("right2");
    fs::write(repo.join("right/added.txt"), "new on branch\n").expect("added");
    git_in(&repo, &["add", "."]);
    git_in(&repo, &["commit", "-m", "branch edit"]);
    let br_rev = git_in(&repo, &["rev-parse", "HEAD"]).trim().to_string();

    git_in(&repo, &["checkout", "main"]);
    fs::write(repo.join("left/a.txt"), "left v2\n").expect("left2");
    fs::remove_file(repo.join("untouched/nested/core.rs")).ok();
    fs::write(repo.join("untouched/nested/core.rs"), "pub fn core() {}\n").expect("restore");
    git_in(&repo, &["add", "."]);
    git_in(&repo, &["commit", "-m", "main edit"]);
    let main_rev = git_in(&repo, &["rev-parse", "HEAD"]).trim().to_string();

    let imported = import_commits_to_cas(&repo, &cas_store, &[&base_rev, &main_rev, &br_rev]);
    let (base_t, main_t, br_t) = (&imported[0].0, &imported[1].0, &imported[2].0);

    let report = merge_trees(&cas_store, base_t, main_t, br_t, &MergeOptions::default())
        .expect("merge disjoint trees");
    assert!(
        report.is_clean(),
        "unexpected conflicts: {:?}",
        report.conflicts
    );
    assert!(
        report.subtrees_reused >= 3,
        "expected O(1) skips for untouched/left/right subtrees, got {}",
        report.subtrees_reused
    );

    // Untouched subtree root entry digest is reused intact.
    let base_untouched = cas_store
        .read_tree_cached(base_t)
        .expect("base")
        .entries
        .into_iter()
        .find(|e| e.name == b"untouched")
        .expect("untouched entry");
    let merged_untouched = cas_store
        .read_tree_cached(&report.tree)
        .expect("merged")
        .entries
        .into_iter()
        .find(|e| e.name == b"untouched")
        .expect("merged untouched entry");
    assert_eq!(base_untouched.id, merged_untouched.id);

    let git_res = run_git_merge_tree(&repo, &base_rev, &main_rev, &br_rev, true);
    assert!(git_res.clean);
    assert_eq!(report.tree_git_oid, git_res.tree_oid);
}

#[test]
#[expect(clippy::too_many_lines)]
fn three_way_text_merge_and_diff3_markers_match_git_byte_for_byte() {
    let scratch = Scratch::new("diff3_parity");
    let (repo, cas_store, _adapter) = build_synthetic_repo(&scratch);

    // Build 9 separate files exercising every text merge & diff3 marker boundary.
    fs::write(repo.join("clean_hunks.txt"), "1\n2\n3\n4\n5\n6\n").unwrap();
    fs::write(repo.join("same_edit.txt"), "1\n2\n3\n4\n").unwrap();
    fs::write(repo.join("same_insert.txt"), "1\n2\n3\n").unwrap();
    fs::write(repo.join("adjacent.txt"), "1\n2\n3\n4\n5\n").unwrap();
    fs::write(repo.join("diff_insert.txt"), "1\n2\n3\n4\n5\n").unwrap();
    fs::write(repo.join("multi_line.txt"), "h\na\nb\nc\nt\n").unwrap();
    fs::write(repo.join("empty_side.txt"), "a\nb\nc\n").unwrap();
    fs::write(repo.join("no_nl.txt"), "a\nb").unwrap();
    git_in(&repo, &["add", "."]);
    git_in(&repo, &["commit", "-m", "base"]);
    let base_rev = git_in(&repo, &["rev-parse", "HEAD"]).trim().to_string();

    git_in(&repo, &["checkout", "-b", "branch"]);
    fs::write(repo.join("clean_hunks.txt"), "1\n2\n3\n4\n5_BR\n6\n").unwrap();
    fs::write(repo.join("same_edit.txt"), "1\n2_BOTH\n3\n4\n").unwrap();
    fs::write(repo.join("same_insert.txt"), "1\n2\nINS\n3\n").unwrap();
    fs::write(repo.join("adjacent.txt"), "1\n2\n3_BR\n4\n5\n").unwrap();
    fs::write(repo.join("diff_insert.txt"), "1\n2\nINS_BR\n3\n4\n5\n").unwrap();
    fs::write(repo.join("multi_line.txt"), "h\nx_br\ny_br\nc\nt\n").unwrap();
    fs::write(repo.join("empty_side.txt"), "a\nc\n").unwrap();
    fs::write(repo.join("no_nl.txt"), "a\nBR").unwrap();
    fs::write(repo.join("add_add.txt"), "from_branch\n").unwrap();
    git_in(&repo, &["add", "."]);
    git_in(&repo, &["commit", "-m", "branch"]);
    let br_rev = git_in(&repo, &["rev-parse", "HEAD"]).trim().to_string();

    git_in(&repo, &["checkout", "main"]);
    fs::write(repo.join("clean_hunks.txt"), "1\n2_MN\n3\n4\n5\n6\n").unwrap();
    fs::write(repo.join("same_edit.txt"), "1\n2_BOTH\n3\n4\n").unwrap();
    fs::write(repo.join("same_insert.txt"), "1\n2\nINS\n3\n").unwrap();
    fs::write(repo.join("adjacent.txt"), "1\n2_MN\n3\n4\n5\n").unwrap();
    fs::write(repo.join("diff_insert.txt"), "1\n2\nINS_MN\n3\n4\n5\n").unwrap();
    fs::write(repo.join("multi_line.txt"), "h\nx_mn\nb\nz_mn\nt\n").unwrap();
    fs::write(repo.join("empty_side.txt"), "a\nM\nc\n").unwrap();
    fs::write(repo.join("no_nl.txt"), "a\nMN").unwrap();
    fs::write(repo.join("add_add.txt"), "from_main\n").unwrap();
    git_in(&repo, &["add", "."]);
    git_in(&repo, &["commit", "-m", "main"]);
    let main_rev = git_in(&repo, &["rev-parse", "HEAD"]).trim().to_string();

    let imported = import_commits_to_cas(&repo, &cas_store, &[&base_rev, &main_rev, &br_rev]);
    let (base_t, main_t, br_t) = (&imported[0].0, &imported[1].0, &imported[2].0);

    let options = MergeOptions {
        labels: ConflictLabels {
            ours: main_rev.clone(),
            base: base_rev.clone(),
            theirs: br_rev.clone(),
        },
        extra_attributes: Vec::new(),
    };
    let report = merge_trees(&cas_store, base_t, main_t, br_t, &options).expect("merge");
    let flat = cas_store.flatten_tree(&report.tree).expect("flatten");

    let git_res = run_git_merge_tree(&repo, &base_rev, &main_rev, &br_rev, true);
    assert!(!git_res.clean);

    // Every single file (clean + diff3 conflicted) must match Git's output byte-for-byte!
    for file in [
        "clean_hunks.txt",
        "same_edit.txt",
        "same_insert.txt",
        "adjacent.txt",
        "diff_insert.txt",
        "multi_line.txt",
        "empty_side.txt",
        "no_nl.txt",
        "add_add.txt",
    ] {
        let git_bytes_blob = git_bytes(
            &repo,
            &["cat-file", "blob", &format!("{}:{file}", git_res.tree_oid)],
        );
        let cas_entry = flat
            .get(file.as_bytes())
            .unwrap_or_else(|| panic!("missing {file} in merged tree"));
        let cas_bytes = cas_store
            .cas()
            .read_blob(&cas_entry.digest().expect("digest"))
            .expect("read merged blob");
        assert_eq!(
            String::from_utf8_lossy(&cas_bytes),
            String::from_utf8_lossy(&git_bytes_blob),
            "byte-level mismatch vs git merge-tree --diff3 on {file}"
        );
    }

    assert!(!has_conflict_block_sized(
        &cas_store
            .cas()
            .read_blob(&flat[b"clean_hunks.txt".as_slice()].digest().unwrap())
            .unwrap(),
        7
    ));
    assert!(has_conflict_block_sized(
        &cas_store
            .cas()
            .read_blob(&flat[b"adjacent.txt".as_slice()].digest().unwrap())
            .unwrap(),
        7
    ));

    let conflict_paths: Vec<String> = report.conflicts.iter().map(|c| c.path.shown()).collect();
    assert_eq!(
        conflict_paths,
        vec![
            "add_add.txt",
            "adjacent.txt",
            "diff_insert.txt",
            "empty_side.txt",
            "multi_line.txt",
            "no_nl.txt",
        ]
    );
    let outcome = classify_shadow_merge(
        &report,
        git_res.clean,
        Some(git_res.tree_oid),
        git_res.conflict_paths.len(),
        false,
    );
    assert!(!outcome.is_blocker());
    assert!(matches!(outcome, MergeShadowOutcome::AgreeConflict { .. }));

    // Unit-check bare marker formatting on merge_text_3way directly too.
    let direct = merge_text_3way(
        b"x\n",
        b"m\n",
        b"t\n",
        &MergeDriver::Text,
        9,
        &ConflictLabels::default(),
    );
    assert!(direct.conflicted);
    assert_eq!(
        String::from_utf8_lossy(&direct.bytes),
        "<<<<<<<<<\nm\n|||||||||\nx\n=========\nt\n>>>>>>>>>\n"
    );
}

#[test]
#[expect(clippy::too_many_lines)]
fn exact_content_rename_merge_and_rename_conflict_variants() {
    let scratch = Scratch::new("renames");
    let (repo, cas_store, _adapter) = build_synthetic_repo(&scratch);

    fs::create_dir_all(repo.join("src")).unwrap();
    fs::create_dir_all(repo.join("lib")).unwrap();
    fs::write(repo.join("src/mod.rs"), "pub fn moved() -> i32 { 1 }\n").unwrap();
    fs::write(repo.join("src/both.rs"), "pub fn both() {}\n").unwrap();
    fs::write(
        repo.join("src/fuzzy.rs"),
        "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\n",
    )
    .unwrap();
    git_in(&repo, &["add", "."]);
    git_in(&repo, &["commit", "-m", "base"]);
    let base_rev = git_in(&repo, &["rev-parse", "HEAD"]).trim().to_string();

    // Branch performs exact renames of `src/mod.rs -> lib/mod.rs` and `src/both.rs -> lib/both.rs`
    // and fuzzy-renames `src/fuzzy.rs -> lib/fuzzy.rs` (small tweak).
    git_in(&repo, &["checkout", "-b", "branch"]);
    fs::rename(repo.join("src/mod.rs"), repo.join("lib/mod.rs")).unwrap();
    fs::rename(repo.join("src/both.rs"), repo.join("lib/both.rs")).unwrap();
    fs::remove_file(repo.join("src/fuzzy.rs")).unwrap();
    fs::write(
        repo.join("lib/fuzzy.rs"),
        "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8 renamed\n",
    )
    .unwrap();
    git_in(&repo, &["add", "-A"]);
    git_in(&repo, &["commit", "-m", "branch renames"]);
    let br_clean_rev = git_in(&repo, &["rev-parse", "HEAD"]).trim().to_string();

    // Main edits `src/mod.rs` + ALSO exact-renames `src/both.rs -> lib/both.rs`.
    git_in(&repo, &["checkout", "main"]);
    fs::create_dir_all(repo.join("lib")).unwrap();
    fs::write(repo.join("src/mod.rs"), "pub fn moved() -> i32 { 42 }\n").unwrap();
    fs::rename(repo.join("src/both.rs"), repo.join("lib/both.rs")).unwrap();
    git_in(&repo, &["add", "-A"]);
    git_in(&repo, &["commit", "-m", "main edits"]);
    let main_exact_rev = git_in(&repo, &["rev-parse", "HEAD"]).trim().to_string();

    // First sub-case: pure exact rename + edit on `src/mod.rs` (`lib/fuzzy.rs` moved cleanly without edit on main):
    let imported = import_commits_to_cas(
        &repo,
        &cas_store,
        &[&base_rev, &main_exact_rev, &br_clean_rev],
    );
    let report = merge_trees(
        &cas_store,
        &imported[0].0,
        &imported[1].0,
        &imported[2].0,
        &MergeOptions::default(),
    )
    .expect("exact rename merge");
    assert!(
        report.is_clean(),
        "unexpected conflicts: {:?}",
        report.conflicts
    );
    assert!(report.exact_renames_applied >= 1);
    let git_res = run_git_merge_tree(&repo, &base_rev, &main_exact_rev, &br_clean_rev, true);
    assert!(git_res.clean);
    assert_eq!(report.tree_git_oid, git_res.tree_oid);

    // Second sub-case: main ALSO modifies line 1 of `src/fuzzy.rs` -> Git merges via fuzzy R80 rename,
    // Rust refuses non-exact fuzzy rename conservatively -> ObservedRustConflictGitCleanNonExactRename!
    fs::write(
        repo.join("src/fuzzy.rs"),
        "line 1 main\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\n",
    )
    .unwrap();
    git_in(&repo, &["add", "-A"]);
    git_in(&repo, &["commit", "-m", "main edits fuzzy"]);
    let main_fuzzy_rev = git_in(&repo, &["rev-parse", "HEAD"]).trim().to_string();

    let imported2 = import_commits_to_cas(&repo, &cas_store, &[&main_fuzzy_rev]);
    let report2 = merge_trees(
        &cas_store,
        &imported[0].0,
        &imported2[0].0,
        &imported[2].0,
        &MergeOptions::default(),
    )
    .expect("fuzzy rename conservative conflict");
    assert!(!report2.is_clean());
    let git_fuzzy = run_git_merge_tree(&repo, &base_rev, &main_fuzzy_rev, &br_clean_rev, true);
    assert!(git_fuzzy.clean);
    let outcome = classify_shadow_merge(
        &report2,
        git_fuzzy.clean,
        Some(git_fuzzy.tree_oid),
        git_fuzzy.conflict_paths.len(),
        true,
    );
    assert!(!outcome.is_blocker());
    assert!(matches!(
        outcome,
        MergeShadowOutcome::ObservedRustConflictGitCleanNonExactRename { .. }
    ));

    // Third sub-case: rename/rename (1to2) conflict (`src/split.rs -> a.rs` vs `b.rs`) + rename/add.
    git_in(&repo, &["checkout", "-B", "rr_base", &base_rev]);
    fs::write(repo.join("src/split.rs"), "pub fn split() {}\n").unwrap();
    git_in(&repo, &["add", "."]);
    git_in(&repo, &["commit", "-m", "split base"]);
    let rr_base = git_in(&repo, &["rev-parse", "HEAD"]).trim().to_string();

    git_in(&repo, &["checkout", "-b", "rr_br"]);
    fs::create_dir_all(repo.join("lib")).unwrap();
    fs::rename(repo.join("src/split.rs"), repo.join("lib/split_b.rs")).unwrap();
    git_in(&repo, &["add", "-A"]);
    git_in(&repo, &["commit", "-m", "rr branch"]);
    let rr_br = git_in(&repo, &["rev-parse", "HEAD"]).trim().to_string();

    git_in(&repo, &["checkout", "-B", "rr_main", &rr_base]);
    fs::create_dir_all(repo.join("lib")).unwrap();
    fs::rename(repo.join("src/split.rs"), repo.join("lib/split_a.rs")).unwrap();
    fs::write(repo.join("lib/split_b.rs"), "// unrelated collision\n").unwrap();
    git_in(&repo, &["add", "-A"]);
    git_in(&repo, &["commit", "-m", "rr main"]);
    let rr_main = git_in(&repo, &["rev-parse", "HEAD"]).trim().to_string();

    let imp_rr = import_commits_to_cas(&repo, &cas_store, &[&rr_base, &rr_main, &rr_br]);
    let rep_rr = merge_trees(
        &cas_store,
        &imp_rr[0].0,
        &imp_rr[1].0,
        &imp_rr[2].0,
        &MergeOptions::default(),
    )
    .expect("1to2 rename conflict");
    assert!(
        rep_rr
            .conflicts
            .iter()
            .any(|c| c.kind == ConflictKind::RenameRename),
        "expected RenameRename conflict, got {:?}",
        rep_rr.conflicts
    );
    assert!(
        rep_rr
            .conflicts
            .iter()
            .any(|c| c.kind == ConflictKind::AddAdd || c.kind == ConflictKind::RenameAdd),
        "expected RenameAdd/AddAdd conflict on lib/split_b.rs, got {:?}",
        rep_rr.conflicts
    );
}

#[test]
fn gitattributes_binary_text_merge_union_and_conflict_marker_size_match_git() {
    let scratch = Scratch::new("attrs");
    let (repo, cas_store, _adapter) = build_synthetic_repo(&scratch);

    fs::create_dir_all(repo.join("sub")).unwrap();
    fs::write(
        repo.join(".gitattributes"),
        "*.bin binary\n*.negtext -text\n*.nomerge -merge\n*.union merge=union\n*.wide conflict-marker-size=10\n",
    )
    .unwrap();
    fs::write(
        repo.join("sub/.gitattributes"),
        "*.custom merge=specialdrv\n",
    )
    .unwrap();

    fs::write(repo.join("a.bin"), "line1\nline2\nline3\n").unwrap();
    fs::write(repo.join("b.negtext"), "line1\nline2\nline3\n").unwrap();
    fs::write(repo.join("c.nomerge"), "line1\nline2\nline3\n").unwrap();
    fs::write(repo.join("d.union"), "head\nmid\ntail\n").unwrap();
    fs::write(repo.join("e.wide"), "head\nbase\ntail\n").unwrap();
    fs::write(repo.join("nul_forced.txt"), b"x\0y\nz\n").unwrap();
    fs::write(repo.join("sub/x.custom"), "a\nb\nc\n").unwrap();
    git_in(&repo, &["add", "."]);
    git_in(&repo, &["commit", "-m", "base"]);
    let base_rev = git_in(&repo, &["rev-parse", "HEAD"]).trim().to_string();

    git_in(&repo, &["checkout", "-b", "branch"]);
    fs::write(repo.join("a.bin"), "line1\nline2\nline3_br\n").unwrap();
    fs::write(repo.join("b.negtext"), "line1\nline2\nline3_br\n").unwrap();
    fs::write(repo.join("c.nomerge"), "line1\nline2\nline3_br\n").unwrap();
    fs::write(repo.join("d.union"), "head\nmid_br\ntail\n").unwrap();
    fs::write(repo.join("e.wide"), "head\nbranch_val\ntail\n").unwrap();
    fs::write(repo.join("nul_forced.txt"), b"x\0y\nz_br\n").unwrap();
    fs::write(repo.join("sub/x.custom"), "a\nb\nc_br\n").unwrap();
    git_in(&repo, &["add", "."]);
    git_in(&repo, &["commit", "-m", "branch"]);
    let br_rev = git_in(&repo, &["rev-parse", "HEAD"]).trim().to_string();

    git_in(&repo, &["checkout", "main"]);
    fs::write(repo.join("a.bin"), "line1_mn\nline2\nline3\n").unwrap();
    fs::write(repo.join("b.negtext"), "line1_mn\nline2\nline3\n").unwrap();
    fs::write(repo.join("c.nomerge"), "line1_mn\nline2\nline3\n").unwrap();
    fs::write(repo.join("d.union"), "head\nmid_mn\ntail\n").unwrap();
    fs::write(repo.join("e.wide"), "head\nmain_val\ntail\n").unwrap();
    fs::write(repo.join("nul_forced.txt"), b"x_mn\0y\nz\n").unwrap();
    fs::write(repo.join("sub/x.custom"), "a_mn\nb\nc\n").unwrap();
    git_in(&repo, &["add", "."]);
    git_in(&repo, &["commit", "-m", "main"]);
    let main_rev = git_in(&repo, &["rev-parse", "HEAD"]).trim().to_string();

    let imp = import_commits_to_cas(&repo, &cas_store, &[&base_rev, &main_rev, &br_rev]);
    let options = MergeOptions {
        labels: ConflictLabels {
            ours: main_rev.clone(),
            base: base_rev.clone(),
            theirs: br_rev.clone(),
        },
        extra_attributes: vec!["nul_forced.txt merge=text".to_string()],
    };
    let report = merge_trees(&cas_store, &imp[0].0, &imp[1].0, &imp[2].0, &options).expect("merge");
    let flat = cas_store.flatten_tree(&report.tree).expect("flatten");

    // Check `-text` auto-merged cleanly, `merge=union` merged cleanly, and conflicts match expected kinds!
    let by_path: HashMap<String, ConflictKind> = report
        .conflicts
        .iter()
        .map(|c| (c.path.shown(), c.kind.clone()))
        .collect();
    assert!(
        !by_path.contains_key("b.negtext"),
        "-text should text-merge"
    );
    assert!(
        !by_path.contains_key("d.union"),
        "merge=union should merge cleanly"
    );
    assert_eq!(by_path.get("a.bin"), Some(&ConflictKind::Binary));
    assert_eq!(by_path.get("c.nomerge"), Some(&ConflictKind::Binary));
    assert_eq!(by_path.get("e.wide"), Some(&ConflictKind::Content));
    assert_eq!(by_path.get("nul_forced.txt"), Some(&ConflictKind::Binary));
    assert_eq!(
        by_path.get("sub/x.custom"),
        Some(&ConflictKind::UnsupportedMergeDriver(
            "specialdrv".to_string()
        ))
    );

    // Verify `b.negtext`, `d.union`, and 10-char markers in `e.wide` match git 2.52.0 byte-for-byte!
    let git_res = run_git_merge_tree(&repo, &base_rev, &main_rev, &br_rev, true);
    for file in ["b.negtext", "d.union", "e.wide"] {
        let git_b = git_bytes(
            &repo,
            &["cat-file", "blob", &format!("{}:{file}", git_res.tree_oid)],
        );
        let cas_b = cas_store
            .cas()
            .read_blob(&flat[file.as_bytes()].digest().unwrap())
            .unwrap();
        assert_eq!(cas_b, git_b, "mismatch on {file}");
    }
    let wide_bytes = cas_store
        .cas()
        .read_blob(&flat[b"e.wide".as_slice()].digest().unwrap())
        .unwrap();
    assert!(has_conflict_block_sized(&wide_bytes, 10));
}

#[test]
#[expect(clippy::too_many_lines)]
fn modes_symlinks_gitlinks_dir_file_single_merge_base_and_spawn_head_snapshot() {
    let scratch = Scratch::new("modes_dag");
    let (repo, cas_store, adapter) = build_synthetic_repo(&scratch);

    let sub1 = [0x11_u8; 20];
    let sub2 = [0x22_u8; 20];
    let sub3 = [0x33_u8; 20];

    fs::write(repo.join("script.sh"), "line1\nline2\nline3\n").unwrap();
    symlink("target_v1", repo.join("sym.lnk")).unwrap();
    git_in(&repo, &["add", "."]);
    git_in(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            "160000",
            &GitOid::from_bytes(sub1).to_hex(),
            "submod",
        ],
    );
    git_in(&repo, &["commit", "-m", "base"]);
    let base_rev = git_in(&repo, &["rev-parse", "HEAD"]).trim().to_string();

    // First test clean `0644 -> 0755` chmod + disjoint edit matching Git tree OID.
    git_in(&repo, &["checkout", "-b", "mode_br"]);
    fs::write(repo.join("script.sh"), "line1\nline2\nline3_br\n").unwrap();
    git_in(&repo, &["add", "script.sh"]);
    git_in(&repo, &["commit", "-m", "mode branch"]);
    let mode_br_rev = git_in(&repo, &["rev-parse", "HEAD"]).trim().to_string();

    git_in(&repo, &["checkout", "main"]);
    fs::write(repo.join("script.sh"), "line1_mn\nline2\nline3\n").unwrap();
    fs::set_permissions(repo.join("script.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    git_in(&repo, &["add", "script.sh"]);
    git_in(&repo, &["commit", "-m", "mode main chmod+edit"]);
    let mode_main_rev = git_in(&repo, &["rev-parse", "HEAD"]).trim().to_string();

    let imp = import_commits_to_cas(
        &repo,
        &cas_store,
        &[&base_rev, &mode_main_rev, &mode_br_rev],
    );
    let rep_mode = merge_trees(
        &cas_store,
        &imp[0].0,
        &imp[1].0,
        &imp[2].0,
        &MergeOptions::default(),
    )
    .expect("mode merge");
    assert!(rep_mode.is_clean());
    let git_mode = run_git_merge_tree(&repo, &base_rev, &mode_main_rev, &mode_br_rev, true);
    assert!(git_mode.clean);
    assert_eq!(rep_mode.tree_git_oid, git_mode.tree_oid);

    // Build real `spawn-snapshot-v1` DAG with first-parent merge + `Spawn-Head` and export via `GitBorderAdapter`.
    let ledger = InMemoryRefLedger::new();
    let body_base = format_commit_body(
        "Alice <alice@spawn.local> 1700010000 +0000",
        "Alice <alice@spawn.local> 1700010000 +0000",
        "Base snap\n",
    );
    let pub_base = cas_store
        .publish_snapshot(
            &ledger,
            &PublishRequest {
                project: "merge-p",
                ref_name: "main",
                kind: D6RefKind::Main,
                expected_ref: None,
                tree: &imp[0].0,
                parents: &[],
                commit_body: &body_base,
                created_at_ms: 1_700_010_000_000,
                reason: "base",
                landing_task_id: None,
            },
        )
        .expect("pub base");
    let body_main = format_commit_body(
        "Alice <alice@spawn.local> 1700010100 +0000",
        "Alice <alice@spawn.local> 1700010100 +0000",
        "Mainline move\n",
    );
    let pub_main = cas_store
        .publish_snapshot(
            &ledger,
            &PublishRequest {
                project: "merge-p",
                ref_name: "main",
                kind: D6RefKind::Main,
                expected_ref: Some(&pub_base.snapshot),
                tree: &imp[1].0,
                parents: std::slice::from_ref(&pub_base.snapshot),
                commit_body: &body_main,
                created_at_ms: 1_700_010_100_000,
                reason: "main move",
                landing_task_id: Some(1),
            },
        )
        .expect("pub main");
    let body_br = format_commit_body(
        "Bob <bob@spawn.local> 1700010200 +0000",
        "Bob <bob@spawn.local> 1700010200 +0000",
        "Branch work\n",
    );
    let pub_br = cas_store
        .publish_snapshot(
            &ledger,
            &PublishRequest {
                project: "merge-p",
                ref_name: "feat/mode",
                kind: D6RefKind::Branch,
                expected_ref: None,
                tree: &imp[2].0,
                parents: std::slice::from_ref(&pub_base.snapshot),
                commit_body: &body_br,
                created_at_ms: 1_700_010_200_000,
                reason: "branch work",
                landing_task_id: None,
            },
        )
        .expect("pub branch");

    let (found_base, snap_rep) =
        merge_snapshots(&cas_store, &pub_main.snapshot, &pub_br.snapshot, None)
            .expect("merge_snapshots single base");
    assert_eq!(found_base, pub_base.snapshot);
    assert!(snap_rep.is_clean());
    assert_eq!(snap_rep.tree_git_oid, git_mode.tree_oid);

    let built = build_merge_snapshot(
        &cas_store,
        &MergeSnapshotSpec {
            main_snapshot: &pub_main.snapshot,
            branch_snapshot: &pub_br.snapshot,
            merged_tree: &snap_rep.tree,
            main_git_oid: Some(pub_main.git_oid),
            branch_git_oid: Some(pub_br.git_oid),
            author_line: "Integrator <int@spawn.local> 1700010300 +0000",
            committer_line: "Integrator <int@spawn.local> 1700010300 +0000",
            message: "Land feat/mode",
            spawn_task: Some(42),
            spawn_objective: Some(7),
            extra_trailers: &[],
        },
    )
    .expect("build_merge_snapshot");

    let mirror = scratch.join("merge_mirror.git");
    let exp = adapter
        .export_refs(
            &cas_store,
            None,
            &mirror,
            &[BorderRefTarget {
                git_ref: "refs/heads/main".to_string(),
                snapshot: built.snapshot_id.clone(),
            }],
        )
        .expect("export merge commit");
    assert_eq!(exp.refs["refs/heads/main"], built.git_oid);
    assert_eq!(
        git_in(&mirror, &["rev-parse", "refs/heads/main^1"]).trim(),
        pub_main.git_oid.to_hex()
    );
    assert_eq!(
        git_in(&mirror, &["rev-parse", "refs/heads/main^2"]).trim(),
        pub_br.git_oid.to_hex()
    );
    let dumped_msg = git_in(&mirror, &["cat-file", "commit", &built.git_oid.to_hex()]);
    assert!(dumped_msg.contains(&format!("Spawn-Head: {}", pub_br.git_oid)));
    assert!(dumped_msg.contains("Spawn-Task: 42"));

    // Verify criss-cross multi-merge-base rejection AND unrelated ancestry rejection.
    let cross_a = cas_store
        .cas()
        .put_merkle_snapshot(&Snapshot {
            tree: imp[0].0.clone(),
            parents: vec![pub_main.snapshot.clone(), pub_br.snapshot.clone()],
            commit_body: body_main.clone(),
        })
        .unwrap();
    let cross_b = cas_store
        .cas()
        .put_merkle_snapshot(&Snapshot {
            tree: imp[0].0.clone(),
            parents: vec![pub_br.snapshot.clone(), pub_main.snapshot.clone()],
            commit_body: body_br.clone(),
        })
        .unwrap();
    let multi_err = merge_snapshots(&cas_store, &cross_a, &cross_b, None)
        .expect_err("must reject criss-cross multi merge-base");
    assert!(matches!(
        multi_err,
        MergeBaseError::MultipleMergeBases { ref bases, .. } if bases.len() == 2
    ));

    let root_unrelated = cas_store
        .cas()
        .put_merkle_snapshot(&Snapshot {
            tree: imp[0].0.clone(),
            parents: Vec::new(),
            commit_body: format_commit_body(
                "Root <r@spawn.local> 1700010400 +0000",
                "Root <r@spawn.local> 1700010400 +0000",
                "Unrelated\n",
            ),
        })
        .unwrap();
    let none_err = merge_snapshots(&cas_store, &pub_main.snapshot, &root_unrelated, None)
        .expect_err("must reject unrelated histories");
    assert!(matches!(none_err, MergeBaseError::NoCommonAncestor { .. }));

    // Verify symlink, gitlink, and dir-file conflicts in `merge_trees`.
    let (d_a, s_a) = cas_store.put_bytes_blob(b"sym_a").unwrap();
    let (d_b, s_b) = cas_store.put_bytes_blob(b"sym_b").unwrap();
    let (d_f, s_f) = cas_store.put_bytes_blob(b"leaf file\n").unwrap();
    let sub_tree = cas_store
        .put_tree_cached(&Tree::from_entries(vec![TreeEntry::file("x", &d_f, s_f)]).unwrap())
        .unwrap();

    let cf_base = cas_store
        .put_tree_cached(
            &Tree::from_entries(vec![
                TreeEntry::symlink("sym", &d_a, s_a),
                TreeEntry::gitlink("sub", &sub1).unwrap(),
                TreeEntry::file("node", &d_f, s_f),
            ])
            .unwrap(),
        )
        .unwrap();
    let cf_main = cas_store
        .put_tree_cached(
            &Tree::from_entries(vec![
                TreeEntry::symlink("sym", &d_b, s_b),
                TreeEntry::gitlink("sub", &sub2).unwrap(),
                TreeEntry::dir("node", &sub_tree),
            ])
            .unwrap(),
        )
        .unwrap();
    let (d_f2, s_f2) = cas_store.put_bytes_blob(b"edited leaf\n").unwrap();
    let cf_branch = cas_store
        .put_tree_cached(
            &Tree::from_entries(vec![
                TreeEntry::symlink("sym", &d_a, s_a),
                TreeEntry::gitlink("sub", &sub3).unwrap(),
                TreeEntry::file("node", &d_f2, s_f2),
            ])
            .unwrap(),
        )
        .unwrap();
    // Make sym also diverge on branch:
    let (d_c, s_c) = cas_store.put_bytes_blob(b"sym_c").unwrap();
    let cf_branch = cas_store
        .apply_mutations(
            &cf_branch,
            &[spawn_cas::TreeMutation::Upsert {
                path: vec![b"sym".to_vec()],
                kind: spawn_cas::TreeEntryKind::Symlink,
                id: d_c.as_bytes(),
                size: s_c,
            }],
        )
        .unwrap();

    let cf_rep = merge_trees(
        &cas_store,
        &cf_base,
        &cf_main,
        &cf_branch,
        &MergeOptions::default(),
    )
    .unwrap();
    let kinds: HashMap<String, ConflictKind> = cf_rep
        .conflicts
        .iter()
        .map(|c| (c.path.shown(), c.kind.clone()))
        .collect();
    assert_eq!(kinds.get("sym"), Some(&ConflictKind::Symlink));
    assert_eq!(kinds.get("sub"), Some(&ConflictKind::Gitlink));
    assert_eq!(kinds.get("node"), Some(&ConflictKind::DirectoryFile));
}

/// Replays every two-parent merge in `merges` (`[merge, p1, p2]`) through `merge_trees` and the
/// `git merge-tree` shadow oracle, asserting no blocker, and returns the oracle summary.
fn replay_merges_through_oracle(
    repo: &Path,
    merges: &[Vec<String>],
    cas_store: &CasMerkleStore,
    batch_reader: &mut BatchGitReader,
    tree_cache: &mut HashMap<GitOid, Digest>,
    blob_cache: &mut HashMap<GitOid, (Digest, u64)>,
) -> ShadowOracleSummary {
    let mut summary = ShadowOracleSummary::default();
    for parts in merges {
        let merge_commit = &parts[0];
        let p1 = &parts[1];
        let p2 = &parts[2];

        let mb_out = git_in(repo, &["merge-base", "--all", p1, p2]);
        let bases: Vec<Digest> = mb_out
            .lines()
            .filter(|l| !l.trim().is_empty())
            .enumerate()
            .map(|(i, _)| {
                let mut b = [0_u8; 32];
                b[0] = u8::try_from(i).unwrap_or(255);
                Digest::from_raw_sha256(b)
            })
            .collect();

        if bases.len() > 1 {
            let p1_d = Digest::from_raw_sha256([0xA1_u8; 32]);
            let p2_d = Digest::from_raw_sha256([0xA2_u8; 32]);
            let err = validate_single_merge_base(&bases, &p1_d, &p2_d)
                .expect_err("multi merge-base must be rejected");
            assert!(matches!(err, MergeBaseError::MultipleMergeBases { .. }));
            summary.rejected_multi_merge_base += 1;
            continue;
        }
        if bases.is_empty() {
            continue;
        }
        let base_commit = mb_out.lines().next().expect("single base").trim();

        let rev_to_tree = |rev: &str| -> GitOid {
            let h = git_in(repo, &["rev-parse", &format!("{rev}^{{tree}}")]);
            GitOid::parse_hex(h.trim()).expect("tree oid")
        };
        let b_oid = rev_to_tree(base_commit);
        let p1_oid = rev_to_tree(p1);
        let p2_oid = rev_to_tree(p2);

        let b_cas = batch_reader.import_tree(cas_store, b_oid, tree_cache, blob_cache);
        let p1_cas = batch_reader.import_tree(cas_store, p1_oid, tree_cache, blob_cache);
        let p2_cas = batch_reader.import_tree(cas_store, p2_oid, tree_cache, blob_cache);

        let options = MergeOptions {
            labels: ConflictLabels {
                ours: p1.clone(),
                base: base_commit.to_string(),
                theirs: p2.clone(),
            },
            extra_attributes: Vec::new(),
        };
        let rust_rep = merge_trees(cas_store, &b_cas, &p1_cas, &p2_cas, &options)
            .unwrap_or_else(|e| panic!("merge_trees failed on {merge_commit}: {e}"));

        let git_res = run_git_merge_tree(repo, base_commit, p1, p2, true);
        let mut had_non_exact_rename = false;
        if !rust_rep.is_clean() && git_res.clean {
            let r1 = git_in(
                repo,
                &[
                    "diff-tree",
                    "-r",
                    "-M",
                    "--diff-filter=R",
                    "--name-status",
                    base_commit,
                    p1,
                ],
            );
            let r2 = git_in(
                repo,
                &[
                    "diff-tree",
                    "-r",
                    "-M",
                    "--diff-filter=R",
                    "--name-status",
                    base_commit,
                    p2,
                ],
            );
            let has_fuzzy = |out: &str| {
                out.lines()
                    .any(|line| !line.starts_with("R100\t") && line.starts_with('R'))
            };
            had_non_exact_rename = has_fuzzy(&r1) || has_fuzzy(&r2);
        }

        let outcome = classify_shadow_merge(
            &rust_rep,
            git_res.clean,
            Some(git_res.tree_oid),
            git_res.conflict_paths.len(),
            had_non_exact_rename,
        );
        assert!(
            !outcome.is_blocker(),
            "blocker on merge {merge_commit} (base={base_commit}, p1={p1}, p2={p2}): {outcome:?}; rust_conflicts={:?}; git_conflicts={:?}",
            rust_rep.conflicts,
            git_res.conflict_paths
        );
        summary.record(&outcome);
    }

    summary
}

/// Two-parent merges reachable from `HEAD` in `repo`, newest first, as `[merge, p1, p2]`.
fn two_parent_merges(repo: &Path) -> Vec<Vec<String>> {
    git_in(repo, &["rev-list", "--merges", "--parents", "HEAD"])
        .lines()
        .map(|line| line.split_whitespace().map(str::to_string).collect())
        .filter(|parts: &Vec<String>| parts.len() == 3)
        .collect()
}

/// True when `repo_root` is the top level of its own git work tree. `scripts/rsi/gate.sh`
/// exports candidate trees with `git archive`, which carries no `.git`.
fn is_own_git_work_tree(repo_root: &Path) -> bool {
    let out = test_git_command()
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "--is-inside-work-tree", "--show-toplevel"])
        .output();
    let Ok(out) = out else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut lines = stdout.lines();
    if lines.next().map(str::trim) != Some("true") {
        return false;
    }
    let Some(top) = lines.next().map(str::trim) else {
        return false;
    };
    match (fs::canonicalize(top), fs::canonicalize(repo_root)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn head_oid(repo: &Path) -> String {
    git_in(repo, &["rev-parse", "HEAD"]).trim().to_string()
}

fn commit_all(repo: &Path, msg: &str) -> String {
    git_in(repo, &["add", "-A"]);
    git_in(repo, &["commit", "-q", "--allow-empty", "-m", msg]);
    head_oid(repo)
}

/// Records a real two-parent merge commit `p1 + p2` on `branch` (checked out afterwards).
/// The merge tree is git's clean result, or `p1`'s tree when git reports a conflict.
fn record_merge(repo: &Path, branch: &str, p1: &str, p2: &str, msg: &str) -> String {
    let out = test_git_command()
        .arg("-C")
        .arg(repo)
        .args(["merge-tree", "--write-tree", p1, p2])
        .output()
        .expect("git merge-tree");
    let tree = if out.status.code() == Some(0) {
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .expect("merge-tree oid")
            .trim()
            .to_string()
    } else {
        git_in(repo, &["rev-parse", &format!("{p1}^{{tree}}")])
            .trim()
            .to_string()
    };
    let merge = git_in(repo, &["commit-tree", &tree, "-p", p1, "-p", p2, "-m", msg])
        .trim()
        .to_string();
    git_in(
        repo,
        &["update-ref", &format!("refs/heads/{branch}"), &merge],
    );
    git_in(repo, &["checkout", "-q", "-f", branch]);
    git_in(repo, &["reset", "-q", "--hard", &merge]);
    merge
}

/// One merge scenario on top of `main`: `side` edits run on a branch, `main_edits` on main,
/// then a two-parent merge `main + side` is recorded on main.
fn merge_scenario(
    repo: &Path,
    name: &str,
    side: impl FnOnce(&Path),
    main_edits: impl FnOnce(&Path),
) -> String {
    let branch = format!("side-{name}");
    git_in(repo, &["checkout", "-q", "-b", &branch, "main"]);
    side(repo);
    let p2 = commit_all(repo, &format!("{name}: side"));
    git_in(repo, &["checkout", "-q", "main"]);
    main_edits(repo);
    let p1 = commit_all(repo, &format!("{name}: main"));
    record_merge(repo, "main", &p1, &p2, &format!("Merge {name}"))
}

fn numbered_lines(prefix: &str, n: usize) -> String {
    let mut out = String::new();
    for i in 1..=n {
        std::fmt::Write::write_fmt(&mut out, format_args!("{prefix} line {i}\n"))
            .expect("write to String");
    }
    out
}

/// Builds a self-contained repository with realistic two-parent merges covering clean disjoint
/// edits, same-file non-overlapping edits, diff3 conflicts, exact-content renames, binary files,
/// executable modes, symlinks, delete/modify conflicts and a criss-cross multi-merge-base merge.
#[expect(clippy::too_many_lines)]
fn build_synthetic_merge_history(repo: &Path) {
    fs::create_dir_all(repo).expect("mkdir synthetic repo");
    git_in(repo, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(repo.join("src/core")).unwrap();
    fs::create_dir_all(repo.join("docs")).unwrap();
    fs::write(repo.join("src/core/lib.rs"), numbered_lines("lib", 40)).unwrap();
    fs::write(repo.join("src/core/util.rs"), numbered_lines("util", 20)).unwrap();
    fs::write(repo.join("docs/guide.md"), numbered_lines("guide", 30)).unwrap();
    fs::write(
        repo.join("src/conflict.rs"),
        "fn value() -> i32 {\n    1\n}\n",
    )
    .unwrap();
    fs::write(
        repo.join("src/moved_src.rs"),
        "pub fn moved() -> u8 { 7 }\n",
    )
    .unwrap();
    fs::write(repo.join("assets.bin"), [0_u8, 1, 2, 3, 0, 255, 254, 0]).unwrap();
    fs::write(repo.join("assets2.bin"), [9_u8, 0, 8, 0, 7, 0]).unwrap();
    fs::write(
        repo.join("run.sh"),
        "#!/bin/sh\necho one\necho two\necho three\n",
    )
    .unwrap();
    fs::write(repo.join("doomed.txt"), numbered_lines("doomed", 5)).unwrap();
    symlink("docs/guide.md", repo.join("link")).unwrap();
    symlink("src/core/lib.rs", repo.join("link2")).unwrap();
    commit_all(repo, "base");

    // 1. Clean disjoint edits in different files and subtrees.
    merge_scenario(
        repo,
        "disjoint",
        |r| fs::write(r.join("docs/guide.md"), numbered_lines("guide v2", 30)).unwrap(),
        |r| fs::write(r.join("src/core/util.rs"), numbered_lines("util v2", 20)).unwrap(),
    );
    // 2. Same file, non-overlapping hunks.
    merge_scenario(
        repo,
        "same_file",
        |r| {
            let s = fs::read_to_string(r.join("src/core/lib.rs")).unwrap();
            fs::write(
                r.join("src/core/lib.rs"),
                s.replace("lib line 35\n", "lib line 35 side\n"),
            )
            .unwrap();
        },
        |r| {
            let s = fs::read_to_string(r.join("src/core/lib.rs")).unwrap();
            fs::write(
                r.join("src/core/lib.rs"),
                s.replace("lib line 3\n", "lib line 3 main\n"),
            )
            .unwrap();
        },
    );
    // 3. Overlapping edits: diff3 content conflict.
    merge_scenario(
        repo,
        "diff3",
        |r| fs::write(r.join("src/conflict.rs"), "fn value() -> i32 {\n    2\n}\n").unwrap(),
        |r| fs::write(r.join("src/conflict.rs"), "fn value() -> i32 {\n    3\n}\n").unwrap(),
    );
    // 4. Exact-content rename on one side, unrelated edit on the other.
    merge_scenario(
        repo,
        "rename",
        |r| fs::rename(r.join("src/moved_src.rs"), r.join("src/core/moved.rs")).unwrap(),
        |r| fs::write(r.join("docs/notes.md"), "notes\n").unwrap(),
    );
    // 5. Binary file changed on one side only: clean.
    merge_scenario(
        repo,
        "binary_one_side",
        |r| fs::write(r.join("assets.bin"), [0_u8, 9, 9, 9, 0, 1]).unwrap(),
        |r| fs::write(r.join("src/core/util.rs"), numbered_lines("util v3", 20)).unwrap(),
    );
    // 6. Binary file changed differently on both sides: binary conflict.
    merge_scenario(
        repo,
        "binary_both",
        |r| fs::write(r.join("assets2.bin"), [1_u8, 0, 1, 0]).unwrap(),
        |r| fs::write(r.join("assets2.bin"), [2_u8, 0, 2, 0, 2]).unwrap(),
    );
    // 7. Executable bit on one side, content edit on the other.
    merge_scenario(
        repo,
        "exec_mode",
        |r| fs::set_permissions(r.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap(),
        |r| {
            fs::write(
                r.join("run.sh"),
                "#!/bin/sh\necho one\necho two\necho THREE\n",
            )
            .unwrap();
        },
    );
    // 8. Symlink retargeted on one side: clean.
    merge_scenario(
        repo,
        "symlink_one_side",
        |r| {
            fs::remove_file(r.join("link")).unwrap();
            symlink("src/core/util.rs", r.join("link")).unwrap();
        },
        |r| fs::write(r.join("docs/notes.md"), "notes v2\n").unwrap(),
    );
    // 9. Symlink retargeted differently on both sides: symlink conflict.
    merge_scenario(
        repo,
        "symlink_both",
        |r| {
            fs::remove_file(r.join("link2")).unwrap();
            symlink("side-target", r.join("link2")).unwrap();
        },
        |r| {
            fs::remove_file(r.join("link2")).unwrap();
            symlink("main-target", r.join("link2")).unwrap();
        },
    );
    // 10. Delete on one side, modify on the other: modify/delete conflict.
    merge_scenario(
        repo,
        "delete_modify",
        |r| fs::remove_file(r.join("doomed.txt")).unwrap(),
        |r| fs::write(r.join("doomed.txt"), numbered_lines("doomed v2", 5)).unwrap(),
    );
    // 11. Identical file added on both sides: clean add/add.
    merge_scenario(
        repo,
        "add_add_same",
        |r| fs::write(r.join("src/shared.rs"), "pub const SHARED: u8 = 1;\n").unwrap(),
        |r| fs::write(r.join("src/shared.rs"), "pub const SHARED: u8 = 1;\n").unwrap(),
    );

    // 12-14. Criss-cross: A and B merge each other, then the two merges are merged, which has
    // two merge bases (A and B) and must be rejected by the single merge-base guard.
    git_in(repo, &["checkout", "-q", "-b", "cross-b", "main"]);
    fs::write(repo.join("src/cross_b.rs"), "pub fn b() {}\n").unwrap();
    let b = commit_all(repo, "criss-cross: B");
    git_in(repo, &["checkout", "-q", "main"]);
    fs::write(repo.join("src/cross_a.rs"), "pub fn a() {}\n").unwrap();
    let a = commit_all(repo, "criss-cross: A");
    let m_ab = record_merge(repo, "main", &a, &b, "criss-cross: merge B into A");
    let m_ba = record_merge(repo, "cross-b", &b, &a, "criss-cross: merge A into B");
    git_in(repo, &["checkout", "-q", "cross-b"]);
    fs::write(repo.join("docs/guide.md"), numbered_lines("guide v3", 30)).unwrap();
    let b2 = commit_all(repo, "criss-cross: B2");
    git_in(repo, &["checkout", "-q", "main"]);
    fs::write(repo.join("src/core/util.rs"), numbered_lines("util v4", 20)).unwrap();
    let a2 = commit_all(repo, "criss-cross: A2");
    assert_ne!(m_ab, m_ba);
    record_merge(repo, "main", &a2, &b2, "criss-cross: final merge");
}

#[test]
#[expect(clippy::too_many_lines)]
fn real_thinkseek_spawn_history_merge_oracle_has_zero_blockers() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf();
    let scratch = Scratch::new("hist_oracle");
    let cas_store = CasMerkleStore::open(scratch.join("cas")).expect("open cas");

    if !is_own_git_work_tree(&repo_root) {
        // `scripts/rsi/gate.sh` runs the suite on a `git archive` export with no `.git`: replay the
        // same oracle over a self-contained synthetic history instead of the real one.
        let synthetic = scratch.join("synthetic-history");
        build_synthetic_merge_history(&synthetic);
        let merges = two_parent_merges(&synthetic);
        assert!(
            merges.len() >= 12,
            "expected >= 12 synthetic two-parent merges, got {}",
            merges.len()
        );
        let mut batch_reader = BatchGitReader::open(&synthetic);
        let mut tree_cache: HashMap<GitOid, Digest> = HashMap::new();
        let mut blob_cache: HashMap<GitOid, (Digest, u64)> = HashMap::new();
        let summary = replay_merges_through_oracle(
            &synthetic,
            &merges,
            &cas_store,
            &mut batch_reader,
            &mut tree_cache,
            &mut blob_cache,
        );
        eprintln!(
            "SYNTHETIC MERGE ORACLE SUMMARY (no .git at {}): merges={} total={} agree_clean={} agree_conflict={} blockers={} observed_non_exact_rename={} rejected_multi_merge_base={}",
            repo_root.display(),
            merges.len(),
            summary.total,
            summary.agree_clean,
            summary.agree_conflict,
            summary.blockers,
            summary.observed_non_exact_rename,
            summary.rejected_multi_merge_base
        );
        assert_eq!(summary.blockers, 0);
        assert!(summary.agree_clean >= 6);
        assert!(summary.agree_conflict >= 4);
        assert!(summary.rejected_multi_merge_base >= 1);
        return;
    }

    let merges = two_parent_merges(&repo_root);
    assert!(
        merges.len() >= 800,
        "expected >= 800 real two-parent merges in thinkseek/spawn history, got {}",
        merges.len()
    );

    let mut batch_reader = BatchGitReader::open(&repo_root);
    let mut tree_cache: HashMap<GitOid, Digest> = HashMap::new();
    let mut blob_cache: HashMap<GitOid, (Digest, u64)> = HashMap::new();
    let summary = replay_merges_through_oracle(
        &repo_root,
        &merges,
        &cas_store,
        &mut batch_reader,
        &mut tree_cache,
        &mut blob_cache,
    );

    eprintln!(
        "HISTORY MERGE ORACLE SUMMARY: total={} agree_clean={} agree_conflict={} blockers={} observed_non_exact_rename={} rejected_multi_merge_base={}",
        summary.total,
        summary.agree_clean,
        summary.agree_conflict,
        summary.blockers,
        summary.observed_non_exact_rename,
        summary.rejected_multi_merge_base
    );

    assert_eq!(summary.blockers, 0);
    assert!(summary.agree_clean >= 650);
    assert!(summary.agree_conflict >= 50);
    assert!(summary.rejected_multi_merge_base >= 3);

    // End-to-end check via `GitBorderAdapter::export_refs` -> `import_bare_repo` -> `merge_snapshots`
    // on the full repository tree from the most recent real single-base merge (`merges[0]`).
    let (_sample_m, sample_p1, sample_p2) = (&merges[0][0], &merges[0][1], &merges[0][2]);
    let sample_base = git_in(&repo_root, &["merge-base", sample_p1, sample_p2])
        .trim()
        .to_string();
    let sample_git = run_git_merge_tree(&repo_root, &sample_base, sample_p1, sample_p2, true);
    let rev_to_tree = |rev: &str| -> GitOid {
        let h = git_in(&repo_root, &["rev-parse", &format!("{rev}^{{tree}}")]);
        GitOid::parse_hex(h.trim()).expect("tree oid")
    };
    let sb_tree = batch_reader.import_tree(
        &cas_store,
        rev_to_tree(&sample_base),
        &mut tree_cache,
        &mut blob_cache,
    );
    let sp1_tree = batch_reader.import_tree(
        &cas_store,
        rev_to_tree(sample_p1),
        &mut tree_cache,
        &mut blob_cache,
    );
    let sp2_tree = batch_reader.import_tree(
        &cas_store,
        rev_to_tree(sample_p2),
        &mut tree_cache,
        &mut blob_cache,
    );
    let sb_snap = cas_store
        .cas()
        .put_merkle_snapshot(&Snapshot {
            tree: sb_tree,
            parents: Vec::new(),
            commit_body: format_commit_body(
                "Oracle <o@spawn.local> 1700020000 +0000",
                "Oracle <o@spawn.local> 1700020000 +0000",
                "base\n",
            ),
        })
        .unwrap();
    let sp1_snap = cas_store
        .cas()
        .put_merkle_snapshot(&Snapshot {
            tree: sp1_tree,
            parents: std::slice::from_ref(&sb_snap).to_vec(),
            commit_body: format_commit_body(
                "Oracle <o@spawn.local> 1700020100 +0000",
                "Oracle <o@spawn.local> 1700020100 +0000",
                "p1\n",
            ),
        })
        .unwrap();
    let sp2_snap = cas_store
        .cas()
        .put_merkle_snapshot(&Snapshot {
            tree: sp2_tree,
            parents: std::slice::from_ref(&sb_snap).to_vec(),
            commit_body: format_commit_body(
                "Oracle <o@spawn.local> 1700020200 +0000",
                "Oracle <o@spawn.local> 1700020200 +0000",
                "p2\n",
            ),
        })
        .unwrap();
    let mini_bare = scratch.join("mini.git");
    let ledger = InMemoryRefLedger::new();
    let adapter = GitBorderAdapter::new();
    adapter
        .export_refs(
            &cas_store,
            None,
            &mini_bare,
            &[
                BorderRefTarget {
                    git_ref: "refs/heads/main".to_string(),
                    snapshot: sp1_snap,
                },
                BorderRefTarget {
                    git_ref: "refs/heads/side".to_string(),
                    snapshot: sp2_snap,
                },
            ],
        )
        .expect("export sample DAG");
    let imp_cas = CasMerkleStore::open(scratch.join("imp_cas")).unwrap();
    let imp_rep = adapter
        .import_bare_repo(&imp_cas, &ledger, "spawn-sample", &mini_bare, "oracle-e2e")
        .expect("import bare repo");
    let main_snap = imp_rep
        .refs
        .iter()
        .find(|r| r.git_ref == "refs/heads/main")
        .unwrap()
        .snapshot
        .clone();
    let side_snap = imp_rep
        .refs
        .iter()
        .find(|r| r.git_ref == "refs/heads/side")
        .unwrap()
        .snapshot
        .clone();
    let (_b_snap, e2e_rep) =
        merge_snapshots(&imp_cas, &main_snap, &side_snap, None).expect("e2e merge_snapshots");
    assert_eq!(e2e_rep.is_clean(), sample_git.clean);
    if e2e_rep.is_clean() {
        assert_eq!(e2e_rep.tree_git_oid, sample_git.tree_oid);
    }
}
