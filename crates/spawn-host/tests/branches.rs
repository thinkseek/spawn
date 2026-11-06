//! Branching and merging in the git store (docs/storage.md, "Branching and
//! merging"): fork from an agent or a commit, and the source's `main` merged
//! into a branch (cleanly) or a mounted workspace between turns (with
//! markers on a conflict).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use spawn_core::id::{BranchName, WorkspaceId};
use spawn_core::port::{BaseMerge, Branch, Commit, WorkspaceStore};
use spawn_host::{GitWorkspace, has_conflict_block, has_conflict_block_sized};

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
fn a_branch_head_is_its_last_commit_and_a_missing_branch_has_none() {
    let fixture = Fixture::new("head", &[("README.md", "base\n")]);
    let missing = BranchName::new("agent_none").expect("valid branch");
    assert_eq!(fixture.store.branch_head(&missing).expect("ask"), None);
    let (branch, workspace, _) = fixture.mount("agent_a");
    let commit = fixture.work(&workspace, &[("work.txt", "mine\n")]);
    let head = fixture.store.branch_head(&branch).expect("ask");
    assert_eq!(
        head.as_ref().map(spawn_core::id::CommitId::as_str),
        Some(commit.as_str())
    );
}

#[test]
fn main_merges_into_a_branch_only_when_clean_and_a_conflict_leaves_it() {
    let fixture = Fixture::new("clean-merge", &[("README.md", "base\n"), ("x.rs", "one\n")]);
    let (branch, workspace, directory) = fixture.mount("agent_a");
    let work = fixture.work(&workspace, &[("work.txt", "mine\n")]);
    fixture.store.unmount(&workspace).expect("unmount");
    assert_eq!(
        fixture.store.merge_main_clean(&branch).expect("merge"),
        BaseMerge::Current
    );
    // A clean main: the branch moves to a merge of both, nothing mounted.
    let main = fixture.land(&[("README.md", "main\n")]);
    let BaseMerge::Merged { commit, files } =
        fixture.store.merge_main_clean(&branch).expect("merge")
    else {
        panic!("a clean main merges");
    };
    assert_eq!(files, ["README.md"]);
    assert_eq!(fixture.head("agent_a"), commit.as_str());
    assert_eq!(parents(&fixture, commit.as_str()), [work.clone(), main]);
    assert!(!directory.exists(), "no directory is touched");
    // A conflicting main: the branch stays where it is, marked false.
    let workspace = fixture.store.mount(&branch).expect("mount");
    let mine = fixture.work(&workspace, &[("x.rs", "mine\n")]);
    fixture.store.unmount(&workspace).expect("unmount");
    fixture.land(&[("x.rs", "theirs\n")]);
    assert_eq!(
        fixture.store.merge_main_clean(&branch).expect("merge"),
        BaseMerge::Conflicted {
            files: vec!["x.rs".to_string()],
            marked: false
        }
    );
    assert_eq!(fixture.head("agent_a"), mine, "the branch did not move");
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

#[test]
fn a_repeated_merge_of_the_same_main_never_moves_the_branch_back() {
    let fixture = Fixture::new("repeat", &[("README.md", "base\n")]);
    let (_, workspace, directory) = fixture.mount("agent_a");
    fixture.work(&workspace, &[("work.txt", "one\n")]);
    fixture.land(&[("new.txt", "theirs\n")]);
    assert!(matches!(
        fixture.store.merge_base(&workspace).expect("merge"),
        BaseMerge::Merged { .. }
    ));
    let after = fixture.work(&workspace, &[("work.txt", "two\n")]);
    // Main has not moved: the branch contains it, whatever came after.
    assert_eq!(
        fixture.store.merge_base(&workspace).expect("again"),
        BaseMerge::Current
    );
    assert_eq!(fixture.head("agent_a"), after);
    assert_eq!(read(&directory, "work.txt"), "two\n");

    // The same holds after a merge that a resolved checkpoint recorded.
    let fixture = Fixture::new("repeat-resolved", &[("README.md", "base\n")]);
    let (_, workspace, directory) = fixture.mount("agent_a");
    fixture.work(&workspace, &[("README.md", "mine\n")]);
    fixture.land(&[("README.md", "theirs\n")]);
    assert!(matches!(
        fixture.store.merge_base(&workspace).expect("merge"),
        BaseMerge::Conflicted { marked: true, .. }
    ));
    fixture.work(&workspace, &[("README.md", "both\n")]);
    let after = fixture.work(&workspace, &[("README.md", "both, then more\n")]);
    assert_eq!(
        fixture.store.merge_base(&workspace).expect("again"),
        BaseMerge::Current
    );
    assert_eq!(fixture.head("agent_a"), after);
    assert_eq!(read(&directory, "README.md"), "both, then more\n");
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
    assert!(
        waiting.starts_with(&format!("pending-merge\0{main}\0{head}\0")),
        "{waiting:?}"
    );
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
fn a_large_blob_from_main_stays_a_blob_and_survives_the_landing() {
    let fixture = Fixture::new("large-from-main", &[("README.md", "base\n")]);
    let (_, workspace, directory) = fixture.mount("agent_a");
    fixture.work(&workspace, &[("work.txt", "mine\n")]);
    // Main takes a 9 MiB file as a plain blob, from outside Spawn.
    let large = vec![7_u8; 9 << 20];
    fs::write(fixture.checkout.join("weights.bin"), &large).expect("write on main");
    let main = fixture.land(&[]);
    let blob = git(&fixture.checkout, &["rev-parse", "main:weights.bin"]);
    assert!(matches!(
        fixture.store.merge_base(&workspace).expect("merge"),
        BaseMerge::Merged { .. }
    ));
    assert_eq!(
        fs::read(directory.join("weights.bin")).expect("merged"),
        large
    );

    // A checkpoint keeps it a blob in the branch's tree.
    let head = fixture.work(&workspace, &[("work.txt", "more\n")]);
    assert_eq!(
        git(
            &fixture.checkout,
            &["rev-parse", &format!("{head}:weights.bin")]
        ),
        blob
    );
    let message = git(&fixture.checkout, &["show", "-s", "--format=%B", &head]);
    assert!(!message.contains("Spawn-Large"), "{message}");

    // Landed (main moves to the branch head), main still has it.
    git(&fixture.checkout, &["merge", "-q", "--ff-only", &head]);
    assert_ne!(fixture.head("main"), main);
    assert_eq!(
        git(&fixture.checkout, &["rev-parse", "main:weights.bin"]),
        blob
    );

    // One the agent made itself, under the cap, is a blob too.
    fs::write(directory.join("own.bin"), &large).expect("write a large file");
    let own = fixture.work(&workspace, &[]);
    assert_eq!(
        git(
            &fixture.checkout,
            &["ls-tree", "--name-only", &own, "own.bin"]
        ),
        "own.bin"
    );
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
    assert!(
        has_conflict_block(b"<<<<<<<<<< a\nx\n==========\ny\n>>>>>>>>>> b\n"),
        "conflict-marker-size=10"
    );
    assert!(
        has_conflict_block(b"<<<<<<<\nx\n=======\ny\n>>>>>>>\n"),
        "markers without labels"
    );
    assert!(
        !has_conflict_block(b"<<<<<<<<<< a\nx\n=======\ny\n>>>>>>>>>> b\n"),
        "one length per block"
    );
    assert!(
        !has_conflict_block(b"<<<<<<<< a\nx\n=======\ny\n>>>>>>> b\n"),
        "one length per block"
    );
    assert!(
        has_conflict_block(b"<<<<<<<<< a\n<<<<<<< b\nx\n=======\ny\n>>>>>>> c\n"),
        "a block inside an unfinished one"
    );
    assert!(!has_conflict_block(b"<<< a\nx\n===\ny\n>>> b\n"));
    assert!(
        has_conflict_block_sized(b"<<< a\nx\n===\ny\n>>> b\n", 3),
        "conflict-marker-size=3"
    );
}

#[test]
fn markers_of_the_attributes_size_are_conflicts_until_resolved() {
    let fixture = Fixture::new(
        "marker-size",
        &[
            (
                ".gitattributes",
                "*.txt conflict-marker-size=10\n*.md conflict-marker-size=3\n",
            ),
            ("long.txt", "base\n"),
            ("short.md", "base\n"),
        ],
    );
    let (_, workspace, directory) = fixture.mount("agent_a");
    let head = fixture.work(
        &workspace,
        &[("long.txt", "mine\n"), ("short.md", "mine\n")],
    );
    let main = fixture.land(&[("long.txt", "theirs\n"), ("short.md", "theirs\n")]);
    assert!(matches!(
        fixture.store.merge_base(&workspace).expect("merge"),
        BaseMerge::Conflicted { marked: true, .. }
    ));
    assert!(
        read(&directory, "long.txt").starts_with("<<<<<<<<<< "),
        "{}",
        read(&directory, "long.txt")
    );
    assert!(
        read(&directory, "short.md").starts_with("<<< "),
        "{}",
        read(&directory, "short.md")
    );

    let long_marked = read(&directory, "long.txt");
    let short_marked = read(&directory, "short.md");
    let Commit::Created(wip) = fixture
        .store
        .commit(&workspace, "wip")
        .expect("commit the markers")
    else {
        panic!("the merge changed the workspace");
    };
    assert_eq!(parents(&fixture, wip.as_str()), vec![head.clone()]);

    // Either file's markers left, the other resolved: no resolution yet,
    // and the integrator sees the one left.
    for (long, short, left) in [
        ("both\n", short_marked.as_str(), "short.md"),
        (long_marked.as_str(), "both\n", "long.txt"),
    ] {
        fs::write(directory.join("long.txt"), long).expect("write");
        fs::write(directory.join("short.md"), short).expect("write");
        assert!(matches!(
            fixture.store.merge_base(&workspace).expect("still marked"),
            BaseMerge::Conflicted { marked: true, .. }
        ));
        let Commit::Created(half) = fixture.store.commit(&workspace, "half").expect("commit")
        else {
            panic!("a change");
        };
        assert_eq!(
            parents(&fixture, half.as_str()).len(),
            1,
            "{left} is marked"
        );
        assert_eq!(
            fixture
                .store
                .conflict_markers(&main, half.as_str())
                .expect("scan"),
            vec![left]
        );
    }
    let resolved = fixture.work(
        &workspace,
        &[("long.txt", "both\n"), ("short.md", "both\n")],
    );
    let parents = parents(&fixture, &resolved);
    assert_eq!(parents[1], main);
    git(
        &fixture.checkout,
        &["merge-base", "--is-ancestor", &head, &resolved],
    );
}

#[test]
fn a_conflict_nested_in_a_merge_base_is_one_on_its_own() {
    // A criss-cross history whose two merge bases conflict: in diff3 style
    // the base section shows their merge, with markers two longer.
    let fixture = Fixture::new("nested", &[("f.txt", "1\nx\n2\n")]);
    let checkout = &fixture.checkout;
    let commit = |file: &str| {
        fs::write(checkout.join("f.txt"), file).expect("write");
        git(checkout, &["commit", "-qam", file]);
    };
    git(checkout, &["checkout", "-qb", "a"]);
    commit("1\na\n2\n");
    git(checkout, &["checkout", "-qb", "c", "main"]);
    commit("1\nc\n2\n");
    for (name, from, other) in [("m1", "a", "c"), ("m2", "c", "a")] {
        git(checkout, &["checkout", "-qb", name, from]);
        let _ = Command::new("git")
            .arg("-C")
            .arg(checkout)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "rerere.enabled=false",
            ])
            .args(["merge", "-q", other])
            .output();
        fs::write(checkout.join("f.txt"), format!("1\n{name}\n2\n")).expect("resolve");
        git(checkout, &["commit", "-qam", name]);
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["-c", "merge.conflictStyle=diff3"])
        .args(["merge-tree", "--write-tree", "m1", "m2"])
        .output()
        .expect("merge-tree");
    let tree = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .expect("a tree")
        .to_string();
    let merged = git(checkout, &["cat-file", "-p", &format!("{tree}:f.txt")]);
    assert!(merged.contains("\n<<<<<<<<< "), "nested markers: {merged}");
    assert!(has_conflict_block(merged.as_bytes()));
    // The outer markers resolved, the nested block left in: still marked.
    let outer = |line: &&str| {
        line.starts_with("<<<<<<< ")
            || line.starts_with("||||||| ")
            || *line == "======="
            || line.starts_with(">>>>>>> ")
    };
    let inner: Vec<&str> = merged.lines().filter(|line| !outer(line)).collect();
    let inner = inner.join("\n");
    assert!(!inner.contains("\n<<<<<<< "), "{inner}");
    assert!(has_conflict_block(inner.as_bytes()), "{inner}");
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
fn a_pending_merge_whose_markers_were_committed_survives_the_unmount() {
    let fixture = Fixture::new("pending-committed", &[("README.md", "base\n")]);
    let (branch, workspace, directory) = fixture.mount("agent_a");
    let head = fixture.work(&workspace, &[("README.md", "mine\n")]);
    let main = fixture.land(&[("README.md", "theirs\n"), ("new.txt", "theirs\n")]);
    assert!(matches!(
        fixture.store.merge_base(&workspace).expect("merge"),
        BaseMerge::Conflicted { marked: true, .. }
    ));
    // A checkpoint with the markers in, then the execution ends.
    let Commit::Created(unresolved) = fixture.store.commit(&workspace, "wip").expect("commit")
    else {
        panic!("the merge changed the workspace");
    };
    assert_eq!(parents(&fixture, unresolved.as_str()), vec![head]);
    let marked = read(&directory, "README.md");
    fixture.store.unmount(&workspace).expect("unmount");
    assert!(
        pending(&directory, "agent_a").exists(),
        "the branch carries the markers: main is still owed a parent"
    );

    // The next execution sees the same markers, not a merge of them.
    let workspace = fixture.store.mount(&branch).expect("mount");
    assert!(pending(&directory, "agent_a").exists());
    assert_eq!(read(&directory, "README.md"), marked);
    assert_eq!(read(&directory, "new.txt"), "theirs\n");
    assert_eq!(
        fixture.store.merge_base(&workspace).expect("merge"),
        BaseMerge::Conflicted {
            files: vec!["README.md".to_string()],
            marked: true,
        }
    );
    assert_eq!(read(&directory, "README.md"), marked, "no doubled markers");

    // The resolution is the merge with main, and main is now contained.
    let resolved = fixture.work(&workspace, &[("README.md", "mine and theirs\n")]);
    assert_eq!(
        parents(&fixture, &resolved),
        vec![unresolved.as_str().to_string(), main.clone()]
    );
    assert!(!pending(&directory, "agent_a").exists());
    git(
        &fixture.checkout,
        &["merge-base", "--is-ancestor", &main, &resolved],
    );
    assert_eq!(
        fixture.store.merge_base(&workspace).expect("current"),
        BaseMerge::Current
    );
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

/// A path that begins with a space: listings are read NUL-split, never
/// trimmed, so the first path of one keeps its leading space.
const SPACED: &str = " leading space.txt";

#[test]
fn a_path_with_a_leading_space_survives_a_clean_merge() {
    let fixture = Fixture::new("spaced-clean", &[("README.md", "base\n")]);
    let (_, workspace, directory) = fixture.mount("agent_a");
    fixture.work(&workspace, &[("work.txt", "mine\n")]);
    fixture.land(&[(SPACED, "theirs\n")]);
    let BaseMerge::Merged { files, .. } = fixture.store.merge_base(&workspace).expect("merge")
    else {
        panic!("main moved: a merge");
    };
    assert_eq!(files, vec![SPACED]);
    assert_eq!(read(&directory, SPACED), "theirs\n");
    assert_eq!(
        fixture.store.commit(&workspace, "after").expect("commit"),
        Commit::Unchanged
    );
}

#[test]
fn a_path_with_a_leading_space_survives_a_conflicted_merge() {
    let fixture = Fixture::new("spaced-conflict", &[(SPACED, "base\n")]);
    let (_, workspace, directory) = fixture.mount("agent_a");
    fixture.work(&workspace, &[(SPACED, "mine\n")]);
    let main = fixture.land(&[(SPACED, "theirs\n")]);
    assert_eq!(
        fixture.store.merge_base(&workspace).expect("merge"),
        BaseMerge::Conflicted {
            files: vec![SPACED.to_string()],
            marked: true
        }
    );
    assert!(has_conflict_block(read(&directory, SPACED).as_bytes()));
    // The markers, committed, are found under the path's own name.
    let Commit::Created(marked) = fixture.store.commit(&workspace, "wip").expect("commit") else {
        panic!("the merge changed the workspace");
    };
    assert_eq!(
        fixture
            .store
            .conflict_markers(&main, marked.as_str())
            .expect("scan"),
        vec![SPACED]
    );
    let resolved = fixture.work(&workspace, &[(SPACED, "both\n")]);
    assert_eq!(parents(&fixture, &resolved)[1], main);
}

/// A file name that is not UTF-8.
const NOT_UTF8: &[u8] = b"bad\xffname.txt";

/// Write `text` to the file named [`NOT_UTF8`] in `directory`.
fn write_not_utf8(directory: &Path, text: &str) {
    use std::os::unix::ffi::OsStrExt;
    fs::write(directory.join(std::ffi::OsStr::from_bytes(NOT_UTF8)), text)
        .expect("write the file whose name is not UTF-8");
}

fn read_not_utf8(directory: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    fs::read(directory.join(std::ffi::OsStr::from_bytes(NOT_UTF8))).expect("read it")
}

#[test]
fn a_conflict_in_a_file_whose_name_is_not_utf8_keeps_its_markers_until_resolved() {
    let fixture = Fixture::new("not-utf8", &[("README.md", "base\n")]);
    write_not_utf8(&fixture.checkout, "base\n");
    fixture.land(&[]);
    let (_, workspace, directory) = fixture.mount("agent_a");
    assert_eq!(read_not_utf8(&directory), b"base\n");
    write_not_utf8(&directory, "mine\n");
    let head = fixture.work(&workspace, &[]);
    write_not_utf8(&fixture.checkout, "theirs\n");
    let main = fixture.land(&[]);
    let shown = String::from_utf8_lossy(NOT_UTF8).into_owned();

    assert_eq!(
        fixture.store.merge_base(&workspace).expect("merge"),
        BaseMerge::Conflicted {
            files: vec![shown.clone()],
            marked: true
        }
    );
    assert!(has_conflict_block(&read_not_utf8(&directory)));
    // A checkpoint with the markers in is no resolution: main is no parent.
    let Commit::Created(marked) = fixture.store.commit(&workspace, "wip").expect("commit") else {
        panic!("the merge changed the workspace");
    };
    assert_eq!(parents(&fixture, marked.as_str()), vec![head]);
    assert!(pending(&directory, "agent_a").exists());
    // The integrator finds the committed markers under the file's own bytes.
    assert_eq!(
        fixture
            .store
            .conflict_markers(&main, marked.as_str())
            .expect("scan"),
        vec![shown.clone()]
    );
    assert_eq!(
        fixture.store.merge_base(&workspace).expect("still marked"),
        BaseMerge::Conflicted {
            files: vec![shown],
            marked: true
        }
    );

    write_not_utf8(&directory, "both\n");
    let resolved = fixture.work(&workspace, &[]);
    assert_eq!(parents(&fixture, &resolved)[1], main);
    assert!(!pending(&directory, "agent_a").exists());
}

/// Where the store keeps a branch's lock.
fn branch_lock(directory: &Path, branch: &str) -> PathBuf {
    directory
        .parent()
        .expect("the workspace root")
        .join(".index")
        .join(format!("{branch}.spawn-lock"))
}

/// Issue #7: a merge into a mounted workspace asks whether the agent is
/// idle under the branch's lock, right before it would write, and one that
/// is not keeps its workspace, its branch and its pending state as they
/// were; asked again once the agent is idle, the merge happens.
#[test]
fn a_merge_into_an_agent_that_is_not_idle_waits_and_changes_nothing() {
    use spawn_core::port::{MERGE_WAITS, WorkspaceError};
    let fixture = Fixture::new("not-idle", &[("README.md", "base\n"), ("notes.txt", "n\n")]);
    let (_, workspace, directory) = fixture.mount("agent_a");
    let head = fixture.work(&workspace, &[("work.txt", "mine\n")]);
    fixture.land(&[("README.md", "landed\n"), ("new.txt", "theirs\n")]);

    let asked = std::cell::Cell::new(0);
    let busy = || {
        asked.set(asked.get() + 1);
        // Asked under the branch's lock: a checkpoint (or the Bridge's
        // typing) cannot take it meanwhile.
        let lock = fs::File::open(branch_lock(&directory, "agent_a")).expect("the lock file");
        assert!(lock.try_lock().is_err(), "asked without the branch's lock");
        Err("a turn is running".to_string())
    };
    match fixture.store.merge_base_when(&workspace, &busy) {
        Err(WorkspaceError::Refused { code, message }) => {
            assert_eq!(code, MERGE_WAITS);
            assert_eq!(message, "a turn is running");
        }
        other => panic!("the merge must wait: {other:?}"),
    }
    assert_eq!(asked.get(), 1);
    assert_eq!(fixture.head("agent_a"), head, "the branch did not move");
    assert_eq!(read(&directory, "README.md"), "base\n");
    assert!(
        !directory.join("new.txt").exists(),
        "the workspace is as it was"
    );

    // A conflict waits the same way: no markers, no pending merge.
    let fixture = Fixture::new("not-idle-conflict", &[("README.md", "base\n")]);
    let (_, workspace, directory) = fixture.mount("agent_a");
    fixture.work(&workspace, &[("README.md", "mine\n")]);
    fixture.land(&[("README.md", "theirs\n")]);
    assert!(
        fixture
            .store
            .merge_base_when(&workspace, &|| Err("busy".to_string()))
            .is_err()
    );
    assert_eq!(read(&directory, "README.md"), "mine\n");
    assert!(!pending(&directory, "agent_a").exists());

    // Idle: the merge happens, as `merge_base` does it.
    assert!(matches!(
        fixture.store.merge_base_when(&workspace, &|| Ok(())),
        Ok(BaseMerge::Conflicted { marked: true, .. })
    ));
    assert!(pending(&directory, "agent_a").exists());
}

/// Issue #7: while the Bridge holds a workspace (it types into the agent's
/// engine), no merge writes it: the merge waits for the hold, and only then
/// asks whether the agent is idle.
#[test]
fn a_held_workspace_keeps_a_merge_out_until_the_hold_ends() {
    let fixture = Fixture::new("held", &[("README.md", "base\n")]);
    let (_, workspace, directory) = fixture.mount("agent_a");
    fixture.work(&workspace, &[("work.txt", "mine\n")]);
    fixture.land(&[("new.txt", "theirs\n")]);

    let held = fixture
        .store
        .hold(&workspace)
        .expect("a git workspace is held");
    let typing_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen = std::sync::Arc::clone(&typing_done);
    let store = fixture.store.clone();
    let merging = std::thread::spawn(move || {
        store.merge_base_when(&workspace, &|| {
            // Asked only once the typing is over.
            assert!(seen.load(std::sync::atomic::Ordering::SeqCst));
            Ok(())
        })
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!merging.is_finished(), "the merge waits for the hold");
    assert!(!directory.join("new.txt").exists());
    typing_done.store(true, std::sync::atomic::Ordering::SeqCst);
    drop(held);
    assert!(matches!(
        merging.join().expect("joined"),
        Ok(BaseMerge::Merged { .. })
    ));
    assert_eq!(read(&directory, "new.txt"), "theirs\n");
}

/// Issue #11: main's copies for a conflict markers cannot show live under
/// `.spawn/`, which no commit keeps and every mount replaces; the store
/// records them beside the workspace and every mount puts them back, until
/// the branch takes main in.
#[test]
fn mains_copies_come_back_at_every_mount_until_main_is_in() {
    let fixture = Fixture::new(
        "copies-remount",
        &[("README.md", "base\n"), ("notes.txt", "notes\n")],
    );
    let (branch, workspace, directory) = fixture.mount("agent_a");
    fs::remove_file(directory.join("notes.txt")).expect("delete");
    fixture.work(&workspace, &[("work.txt", "mine\n")]);
    let main = fixture.land(&[("notes.txt", "notes, revised\n"), ("new.txt", "theirs\n")]);
    assert_eq!(
        fixture.store.merge_base(&workspace).expect("merge"),
        BaseMerge::Conflicted {
            files: vec!["notes.txt".to_string()],
            marked: false,
        }
    );
    let copy = format!("{}/notes.txt", spawn_host::MAIN_COPIES);
    assert_eq!(read(&directory, &copy), "notes, revised\n");
    assert_eq!(
        fixture.store.main_copies("agent_a"),
        Some((main.clone(), vec!["notes.txt".to_string()]))
    );
    assert!(
        !directory.join("notes.txt").exists(),
        "the workspace is unchanged"
    );

    // The engine is lost and the workspace goes; a resume merges main only
    // when it is clean (it is not), then mounts the branch afresh.
    fixture.store.unmount(&workspace).expect("unmount");
    assert!(!directory.exists());
    assert!(matches!(
        fixture.store.merge_main_clean(&branch).expect("merge"),
        BaseMerge::Conflicted { marked: false, .. }
    ));
    let workspace = fixture.store.mount(&branch).expect("mount");
    assert_eq!(
        read(&directory, &copy),
        "notes, revised\n",
        "the copy the note names is back"
    );
    // And again at the next mount.
    let workspace = {
        fixture.store.unmount(&workspace).expect("unmount");
        fixture.store.mount(&branch).expect("mount")
    };
    assert_eq!(read(&directory, &copy), "notes, revised\n");

    // Resolved as the note says (main's version); the next merge takes
    // main in cleanly, and no mount brings the copies back after that.
    fs::copy(directory.join(&copy), directory.join("notes.txt")).expect("copy main's version");
    fixture.work(&workspace, &[]);
    assert!(matches!(
        fixture.store.merge_base(&workspace).expect("merge"),
        BaseMerge::Merged { .. }
    ));
    assert_eq!(fixture.store.main_copies("agent_a"), None);
    fixture.store.unmount(&workspace).expect("unmount");
    fixture.store.mount(&branch).expect("mount");
    assert!(!directory.join(&copy).exists());
}
