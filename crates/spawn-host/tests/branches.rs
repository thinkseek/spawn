//! Branching and merging in the git store (docs/storage.md, "Branching and
//! merging"): fork from an agent or a commit, and the source's `main` merged
//! into an idle workspace.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use spawn_core::id::{BranchName, WorkspaceId};
use spawn_core::port::{BaseMerge, Branch, Commit, WorkspaceStore};
use spawn_host::{GitWorkspace, has_conflict_block};

/// Output of git in `directory`, which must succeed.
fn git(directory: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(["-c", "user.name=spawn-test"])
        .args(["-c", "user.email=spawn@localhost"])
        .args(arguments)
        .output()
        .expect("git is installed");
    assert!(
        output.status.success(),
        "git {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A repository whose `main` is checked out in `<base>/repo` (so a test
/// moves `main` with ordinary commits there, as the gate would), the store
/// over its `.git`, and the workspace root.
struct Fixture {
    checkout: PathBuf,
    store: GitWorkspace,
}

impl Fixture {
    fn new(tag: &str, files: &[(&str, &str)]) -> Self {
        let base =
            std::env::temp_dir().join(format!("spawn-branches-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let checkout = base.join("repo");
        fs::create_dir_all(&checkout).expect("create the repository");
        git(&checkout, &["init", "-q", "--initial-branch=main"]);
        for (path, text) in files {
            fs::write(checkout.join(path), text).expect("write a base file");
        }
        git(&checkout, &["add", "-A"]);
        git(&checkout, &["commit", "-qm", "base"]);
        let root = base.join("work");
        fs::create_dir_all(&root).expect("create the workspace root");
        Self {
            store: GitWorkspace::new(checkout.join(".git"), &root),
            checkout,
        }
    }

    /// Commit `files` on `main`, as a landing would.
    fn land(&self, files: &[(&str, &str)]) -> String {
        for (path, text) in files {
            fs::write(self.checkout.join(path), text).expect("write on main");
        }
        git(&self.checkout, &["add", "-A"]);
        git(&self.checkout, &["commit", "-qm", "landed"]);
        self.head("main")
    }

    fn head(&self, branch: &str) -> String {
        git(
            &self.checkout,
            &["rev-parse", &format!("refs/heads/{branch}")],
        )
    }

    /// A mounted workspace of a new agent branch.
    fn mount(&self, agent: &str) -> (BranchName, WorkspaceId, PathBuf) {
        let branch = BranchName::new(agent).expect("valid branch");
        self.store
            .ensure_branch(&branch, None)
            .expect("create the branch");
        let workspace = self.store.mount(&branch).expect("mount");
        let directory = PathBuf::from(workspace.path());
        (branch, workspace, directory)
    }

    /// Write files into a workspace and checkpoint them.
    fn work(&self, workspace: &WorkspaceId, files: &[(&str, &str)]) -> String {
        let directory = PathBuf::from(workspace.path());
        for (path, text) in files {
            fs::write(directory.join(path), text).expect("write in the workspace");
        }
        match self.store.commit(workspace, "work").expect("commit") {
            Commit::Created(commit) => commit.as_str().to_string(),
            Commit::Unchanged => panic!("the work changed something"),
        }
    }
}

fn read(directory: &Path, path: &str) -> String {
    fs::read_to_string(directory.join(path)).expect("read a workspace file")
}

#[test]
fn a_branch_forks_from_another_agent_or_from_a_commit() {
    let fixture = Fixture::new("fork", &[("README.md", "base\n")]);
    let base = fixture.head("main");
    let (_, source, _) = fixture.mount("agent_source");
    let head = fixture.work(&source, &[("idea.txt", "half done\n")]);

    let forked = BranchName::new("agent_fork").expect("valid branch");
    let made = fixture
        .store
        .ensure_branch(&forked, Some("agent_source"))
        .expect("fork from an agent");
    let Branch::Created(fork) = made else {
        panic!("a new branch: {made:?}");
    };
    assert_eq!(
        (fork.from.as_str(), fork.commit.as_str()),
        ("agent_source", head.as_str())
    );
    let workspace = fixture.store.mount(&forked).expect("mount the fork");
    assert_eq!(read(Path::new(workspace.path()), "idea.txt"), "half done\n");

    let pinned = BranchName::new("agent_pinned").expect("valid branch");
    fixture
        .store
        .ensure_branch(&pinned, Some(&base))
        .expect("fork from a commit");
    assert_eq!(fixture.head("agent_pinned"), base);
    let workspace = fixture.store.mount(&pinned).expect("mount the pin");
    assert!(!Path::new(workspace.path()).join("idea.txt").exists());

    // An existing branch stays where it is.
    assert_eq!(
        fixture
            .store
            .ensure_branch(&forked, Some(&base))
            .expect("existing"),
        Branch::Existing
    );
    assert_eq!(fixture.head("agent_fork"), head);

    let fresh = BranchName::new("agent_fresh").expect("valid branch");
    let unknown = "0123456789abcdef0123456789abcdef01234567";
    for bad in [
        "-rf",
        "--all",
        "HEAD",
        "main",
        "agent_nobody",
        &base[..12],
        unknown,
    ] {
        assert!(
            fixture.store.ensure_branch(&fresh, Some(bad)).is_err(),
            "from {bad} is refused"
        );
    }
    let tree = git(&fixture.checkout, &["rev-parse", "main^{tree}"]);
    assert!(
        fixture.store.ensure_branch(&fresh, Some(&tree)).is_err(),
        "a tree is not a commit"
    );
    assert!(
        git(&fixture.checkout, &["branch", "--list", "agent_fresh"]).is_empty(),
        "a refused from creates nothing"
    );
}

#[test]
fn a_branch_that_contains_main_is_current() {
    let fixture = Fixture::new("current", &[("README.md", "base\n")]);
    let (_, workspace, _) = fixture.mount("agent_a");
    fixture.work(&workspace, &[("work.txt", "mine\n")]);
    assert_eq!(
        fixture.store.merge_base(&workspace).expect("merge"),
        BaseMerge::Current
    );
}

#[test]
fn a_clean_merge_moves_the_directory_then_the_branch_and_leaves_build_output() {
    let fixture = Fixture::new("clean", &[("README.md", "base\n"), ("shared.txt", "one\n")]);
    let (_, workspace, directory) = fixture.mount("agent_a");
    fs::create_dir_all(directory.join("target")).expect("create build output");
    fs::write(directory.join("target/cache"), "rebuildable\n").expect("write build output");
    let head = fixture.work(&workspace, &[("work.txt", "mine\n")]);
    let main = fixture.land(&[("README.md", "landed\n"), ("new.txt", "theirs\n")]);

    let BaseMerge::Merged { commit, mut files } =
        fixture.store.merge_base(&workspace).expect("merge")
    else {
        panic!("main moved: a merge");
    };
    files.sort();
    assert_eq!(files, vec!["README.md", "new.txt"]);
    assert_eq!(read(&directory, "README.md"), "landed\n");
    assert_eq!(read(&directory, "new.txt"), "theirs\n");
    assert_eq!(read(&directory, "work.txt"), "mine\n");
    assert_eq!(read(&directory, "target/cache"), "rebuildable\n");
    assert_eq!(fixture.head("agent_a"), commit.as_str());
    let parents = git(
        &fixture.checkout,
        &["rev-list", "--parents", "-n1", commit.as_str()],
    );
    assert_eq!(parents, format!("{} {head} {main}", commit.as_str()));
    // The directory is the branch: the next checkpoint has nothing to add.
    assert_eq!(
        fixture.store.commit(&workspace, "after").expect("commit"),
        Commit::Unchanged
    );
    assert_eq!(
        fixture.store.merge_base(&workspace).expect("again"),
        BaseMerge::Current
    );
}

/// Where the store keeps a branch's pending merge.
fn pending(directory: &Path, branch: &str) -> PathBuf {
    directory
        .parent()
        .expect("the workspace root")
        .join(".merge")
        .join(branch)
}

fn parents(fixture: &Fixture, commit: &str) -> Vec<String> {
    git(&fixture.checkout, &["rev-list", "--parents", "-n1", commit])
        .split_whitespace()
        .skip(1)
        .map(str::to_string)
        .collect()
}

#[test]
fn a_content_conflict_writes_markers_and_the_resolution_becomes_the_merge() {
    let fixture = Fixture::new("conflict", &[("README.md", "base\n")]);
    let (_, workspace, directory) = fixture.mount("agent_a");
    let head = fixture.work(&workspace, &[("README.md", "mine\n")]);
    let main = fixture.land(&[("README.md", "theirs\n"), ("new.txt", "theirs\n")]);
    let conflicted = BaseMerge::Conflicted {
        files: vec!["README.md".to_string()],
        marked: true,
    };
    assert_eq!(
        fixture.store.merge_base(&workspace).expect("merge"),
        conflicted
    );
    // Main's side is in the workspace, the conflict as markers.
    let marked = read(&directory, "README.md");
    assert!(
        marked.contains("<<<<<<< ") && marked.contains("theirs") && marked.contains("mine"),
        "{marked}"
    );
    assert_eq!(read(&directory, "new.txt"), "theirs\n");
    assert_eq!(fixture.head("agent_a"), head, "the branch did not move");
    let waiting = fs::read_to_string(pending(&directory, "agent_a")).expect("a pending merge");
    assert!(waiting.starts_with(&main), "{waiting:?}");
    // Asked again, nothing is written twice.
    assert_eq!(
        fixture.store.merge_base(&workspace).expect("again"),
        conflicted
    );
    assert_eq!(read(&directory, "README.md"), marked);

    // A checkpoint with the markers still there is an ordinary commit.
    let Commit::Created(unresolved) = fixture.store.commit(&workspace, "wip").expect("commit")
    else {
        panic!("the merge changed the workspace");
    };
    assert_eq!(parents(&fixture, unresolved.as_str()), vec![head.clone()]);
    assert!(pending(&directory, "agent_a").exists(), "still pending");

    // Resolved: the next checkpoint is the merge, and it lands clean.
    let resolved = fixture.work(&workspace, &[("README.md", "mine and theirs\n")]);
    assert_eq!(
        parents(&fixture, &resolved),
        vec![unresolved.as_str().to_string(), main.clone()]
    );
    assert!(!pending(&directory, "agent_a").exists());
    assert_eq!(
        fixture.store.merge_base(&workspace).expect("current"),
        BaseMerge::Current
    );
    git(
        &fixture.checkout,
        &["merge-base", "--is-ancestor", &main, &resolved],
    );
    git(
        &fixture.checkout,
        &["merge-tree", "--write-tree", &main, &resolved],
    );
}

#[test]
fn a_newer_main_waits_until_the_markers_are_resolved() {
    let fixture = Fixture::new("stacked", &[("README.md", "base\n"), ("other.txt", "x\n")]);
    let (_, workspace, directory) = fixture.mount("agent_a");
    fixture.work(&workspace, &[("README.md", "mine\n")]);
    let first = fixture.land(&[("README.md", "theirs\n")]);
    assert!(matches!(
        fixture.store.merge_base(&workspace).expect("merge"),
        BaseMerge::Conflicted { marked: true, .. }
    ));
    let newer = fixture.land(&[("other.txt", "y\n")]);
    assert!(matches!(
        fixture.store.merge_base(&workspace).expect("merge"),
        BaseMerge::Conflicted { marked: true, .. }
    ));
    assert_eq!(
        read(&directory, "other.txt"),
        "x\n",
        "no second merge on top"
    );

    let resolved = fixture.work(&workspace, &[("README.md", "both\n")]);
    assert_eq!(parents(&fixture, &resolved)[1], first);
    let BaseMerge::Merged { commit, .. } = fixture.store.merge_base(&workspace).expect("merge")
    else {
        panic!("the newer main merges once resolved");
    };
    assert_eq!(parents(&fixture, commit.as_str()), vec![resolved, newer]);
    assert_eq!(read(&directory, "other.txt"), "y\n");
    assert_eq!(read(&directory, "README.md"), "both\n");
}

#[test]
fn a_conflict_in_a_directory_that_differs_from_its_branch_changes_nothing() {
    let fixture = Fixture::new("dirty-conflict", &[("README.md", "base\n")]);
    let (_, workspace, directory) = fixture.mount("agent_a");
    let head = fixture.work(&workspace, &[("README.md", "mine\n")]);
    fixture.land(&[("README.md", "theirs\n")]);
    fs::write(directory.join("README.md"), "unsaved\n").expect("edit");
    assert!(fixture.store.merge_base(&workspace).is_err());
    assert_eq!(read(&directory, "README.md"), "unsaved\n");
    assert!(!pending(&directory, "agent_a").exists());
    assert_eq!(fixture.head("agent_a"), head);
}

#[test]
fn a_conflict_markers_cannot_show_changes_nothing() {
    let fixture = Fixture::new(
        "modify-delete",
        &[("README.md", "base\n"), ("keep.txt", "k\n")],
    );
    let (_, workspace, directory) = fixture.mount("agent_a");
    fs::remove_file(directory.join("README.md")).expect("delete");
    let head = fixture.work(&workspace, &[("work.txt", "mine\n")]);
    fixture.land(&[("README.md", "changed\n"), ("new.txt", "theirs\n")]);
    assert_eq!(
        fixture.store.merge_base(&workspace).expect("merge"),
        BaseMerge::Conflicted {
            files: vec!["README.md".to_string()],
            marked: false,
        }
    );
    assert!(!directory.join("README.md").exists());
    assert!(!directory.join("new.txt").exists());
    assert!(!pending(&directory, "agent_a").exists());
    assert_eq!(fixture.head("agent_a"), head);
    assert_eq!(
        fixture.store.commit(&workspace, "nothing").expect("commit"),
        Commit::Unchanged
    );
}

#[test]
fn a_main_with_no_shared_history_is_nothing_to_merge() {
    let fixture = Fixture::new("unrelated", &[("README.md", "base\n")]);
    let (_, workspace, directory) = fixture.mount("agent_a");
    let head = fixture.work(&workspace, &[("work.txt", "mine\n")]);
    // Main reseeded: an orphan commit with nothing in common with the branch.
    git(&fixture.checkout, &["checkout", "-q", "--orphan", "fresh"]);
    git(&fixture.checkout, &["rm", "-rqf", "."]);
    fs::write(fixture.checkout.join("other.txt"), "x\n").expect("write");
    git(&fixture.checkout, &["add", "-A"]);
    git(&fixture.checkout, &["commit", "-qm", "reseeded"]);
    git(&fixture.checkout, &["branch", "-f", "main", "fresh"]);
    assert_eq!(
        fixture.store.merge_base(&workspace).expect("no error"),
        BaseMerge::Current
    );
    assert!(!directory.join("other.txt").exists());
    assert_eq!(fixture.head("agent_a"), head);
}

#[test]
fn a_directory_that_differs_from_its_branch_refuses_the_merge() {
    let fixture = Fixture::new("dirty", &[("README.md", "base\n")]);
    let (_, workspace, directory) = fixture.mount("agent_a");
    let head = fixture.work(&workspace, &[("work.txt", "mine\n")]);
    fixture.land(&[("README.md", "landed\n"), ("new.txt", "theirs\n")]);

    // A file the merge changes, edited after the checkpoint.
    fs::write(directory.join("README.md"), "unsaved\n").expect("edit");
    assert!(fixture.store.merge_base(&workspace).is_err());
    assert_eq!(read(&directory, "README.md"), "unsaved\n");
    assert!(!directory.join("new.txt").exists(), "nothing was written");
    assert_eq!(fixture.head("agent_a"), head);

    // An untracked file where the merge would write one.
    fs::write(directory.join("README.md"), "base\n").expect("restore");
    fs::write(directory.join("new.txt"), "mine too\n").expect("in the way");
    assert!(fixture.store.merge_base(&workspace).is_err());
    assert_eq!(read(&directory, "new.txt"), "mine too\n");
    assert_eq!(fixture.head("agent_a"), head);
}

#[test]
fn the_merge_rules_come_from_mains_gitattributes() {
    let fixture = Fixture::new("union", &[("notes.txt", "start\n")]);
    let (_, workspace, directory) = fixture.mount("agent_a");
    fixture.work(&workspace, &[("notes.txt", "start\nmine\n")]);
    fixture.land(&[
        (".gitattributes", "notes.txt merge=union\n"),
        ("notes.txt", "start\ntheirs\n"),
    ]);
    assert!(matches!(
        fixture.store.merge_base(&workspace).expect("merge"),
        BaseMerge::Merged { .. }
    ));
    assert_eq!(read(&directory, "notes.txt"), "start\ntheirs\nmine\n");

    // A branch's own .gitattributes does not decide: main has none here.
    let fixture = Fixture::new("own-rules", &[("notes.txt", "start\n")]);
    let (_, workspace, _) = fixture.mount("agent_a");
    fixture.work(
        &workspace,
        &[
            (".gitattributes", "notes.txt merge=union\n"),
            ("notes.txt", "start\nmine\n"),
        ],
    );
    fixture.land(&[("notes.txt", "start\ntheirs\n")]);
    assert!(matches!(
        fixture.store.merge_base(&workspace).expect("merge"),
        BaseMerge::Conflicted { .. }
    ));
}

#[test]
fn a_merge_keeps_the_branchs_large_file_manifest() {
    let fixture = Fixture::new("trailer", &[("README.md", "base\n")]);
    let (_, workspace, _) = fixture.mount("agent_a");
    let head = fixture.work(&workspace, &[("work.txt", "mine\n")]);
    // A checkpoint that names a large-file manifest (storage phase 1).
    let tree = git(
        &fixture.checkout,
        &["rev-parse", &format!("{head}^{{tree}}")],
    );
    let named = git(
        &fixture.checkout,
        &[
            "commit-tree",
            &tree,
            "-p",
            &head,
            "-m",
            "spawn checkpoint\n\nSpawn-Large: abc123",
        ],
    );
    git(
        &fixture.checkout,
        &["update-ref", "refs/heads/agent_a", &named, &head],
    );
    fixture.land(&[("new.txt", "theirs\n")]);
    let BaseMerge::Merged { commit, .. } = fixture.store.merge_base(&workspace).expect("merge")
    else {
        panic!("a merge");
    };
    let message = git(
        &fixture.checkout,
        &["log", "-1", "--format=%B", commit.as_str()],
    );
    assert!(message.contains("Spawn-Large: abc123"), "{message}");
}

#[test]
fn only_a_whole_marker_block_is_a_conflict() {
    assert!(has_conflict_block(
        b"a\n<<<<<<< main\nx\n=======\ny\n>>>>>>> agent\nb\n"
    ));
    assert!(has_conflict_block(
        b"<<<<<<< main\r\nx\r\n=======\r\ny\r\n>>>>>>> agent\r\n"
    ));
    assert!(
        !has_conflict_block(b"Title\n=======\n\ntext\n"),
        "a Setext heading"
    );
    assert!(
        !has_conflict_block(b"<<<<<<< main\nx\n>>>>>>> agent\n"),
        "no middle"
    );
    assert!(
        !has_conflict_block(b"=======\n<<<<<<< a\n>>>>>>> b\n"),
        "out of order"
    );
    assert!(
        !has_conflict_block(b"  <<<<<<< a\n=======\n>>>>>>> b\n"),
        "not at line start"
    );
}

#[test]
fn a_resolution_with_a_heading_underline_is_resolved() {
    let fixture = Fixture::new("setext", &[("README.md", "Title\n=======\n\nbase\n")]);
    let (_, workspace, directory) = fixture.mount("agent_a");
    fixture.work(&workspace, &[("README.md", "Title\n=======\n\nmine\n")]);
    let main = fixture.land(&[("README.md", "Title\n=======\n\ntheirs\n")]);
    assert!(matches!(
        fixture.store.merge_base(&workspace).expect("merge"),
        BaseMerge::Conflicted { marked: true, .. }
    ));
    let resolved = fixture.work(&workspace, &[("README.md", "Title\n=======\n\nboth\n")]);
    assert_eq!(parents(&fixture, &resolved)[1], main);
    assert!(!pending(&directory, "agent_a").exists());
}

#[test]
fn a_pending_merge_lives_only_in_its_directory() {
    let fixture = Fixture::new("pending-mount", &[("README.md", "base\n")]);
    let (branch, workspace, directory) = fixture.mount("agent_a");
    let head = fixture.work(&workspace, &[("README.md", "mine\n")]);
    fixture.land(&[("README.md", "theirs\n")]);
    assert!(matches!(
        fixture.store.merge_base(&workspace).expect("merge"),
        BaseMerge::Conflicted { marked: true, .. }
    ));
    assert!(pending(&directory, "agent_a").exists());
    // The directory goes (say the final checkpoint failed): so does the merge.
    fixture.store.unmount(&workspace).expect("unmount");
    assert!(!pending(&directory, "agent_a").exists());

    // A fresh mount of the pre-merge head never takes main as a parent.
    let workspace = fixture.store.mount(&branch).expect("mount");
    assert!(matches!(
        fixture.store.merge_base(&workspace).expect("merge"),
        BaseMerge::Conflicted { marked: true, .. }
    ));
    fs::write(pending(&directory, "agent_a"), "stale").expect("a stale file");
    let workspace = fixture.store.mount(&branch).expect("mount again");
    assert!(!pending(&directory, "agent_a").exists(), "mount clears it");
    let after = fixture.work(&workspace, &[("work.txt", "more\n")]);
    assert_eq!(parents(&fixture, &after), vec![head]);
}

#[test]
fn committed_markers_are_found_against_main() {
    let fixture = Fixture::new("markers", &[("README.md", "base\n"), ("notes.md", "n\n")]);
    let (_, workspace, _) = fixture.mount("agent_a");
    let main = fixture.head("main");
    let clean = fixture.work(&workspace, &[("notes.md", "Title\n=======\n")]);
    assert!(
        fixture
            .store
            .conflict_markers(&main, &clean)
            .expect("scan")
            .is_empty()
    );
    let marked = fixture.work(
        &workspace,
        &[(
            "README.md",
            "<<<<<<< main\nbase\n=======\nmine\n>>>>>>> agent_a\n",
        )],
    );
    assert_eq!(
        fixture
            .store
            .conflict_markers(&main, &marked)
            .expect("scan"),
        vec!["README.md"]
    );
    assert!(!fixture.store.has_pending_merge("agent_a"));
}
