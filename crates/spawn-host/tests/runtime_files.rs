//! The engine's runtime files and checkpoints (docs/storage.md, "The
//! engine's runtime files"): what Claude Code writes for itself under
//! `.claude/` never enters a checkpoint, whatever the project's
//! `.gitignore` says, while the `.claude/` files a project commits do, and
//! a runtime file the project already tracks is never dropped by Spawn.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use spawn_core::id::{BranchName, WorkspaceId};
use spawn_core::port::{BaseMerge, Commit, SetAside, WorkspaceStore};
use spawn_host::GitWorkspace;

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

/// A project whose `main` holds `files` (no `.gitignore` unless one is
/// among them), checked out in `<base>/repo`; the store over its `.git`;
/// the workspace root.
fn project(tag: &str, files: &[(&str, &str)]) -> (PathBuf, GitWorkspace, PathBuf) {
    let base = std::env::temp_dir().join(format!("spawn-runtime-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    let checkout = base.join("repo");
    fs::create_dir_all(&checkout).expect("create the repository");
    git(&checkout, &["init", "-q", "--initial-branch=main"]);
    for (path, text) in files {
        write(&checkout, path, text);
    }
    git(&checkout, &["add", "-A"]);
    git(&checkout, &["commit", "-qm", "base"]);
    let root = base.join("work");
    fs::create_dir_all(&root).expect("create the workspace root");
    let store = GitWorkspace::new(checkout.join(".git"), &root);
    (checkout, store, root)
}

fn write(directory: &Path, path: &str, text: &str) {
    let path = directory.join(path);
    fs::create_dir_all(path.parent().expect("a parent")).expect("create the parent");
    fs::write(path, text).expect("write a file");
}

fn agent() -> BranchName {
    BranchName::new("agent_engine").expect("valid branch")
}

fn mount(store: &GitWorkspace) -> (WorkspaceId, PathBuf) {
    store.ensure_branch(&agent(), None).expect("create the branch");
    let workspace = store.mount(&agent()).expect("mount");
    let directory = PathBuf::from(workspace.path());
    (workspace, directory)
}

/// Every path `reference` holds, one per line.
fn files_of(checkout: &Path, reference: &str) -> Vec<String> {
    git(checkout, &["ls-tree", "-r", "--name-only", reference])
        .lines()
        .map(str::to_string)
        .collect()
}

/// The files the engine writes for itself, the ones the task names first:
/// its scheduler lock and a file in one of its worktrees.
const LOCK: &str = ".claude/scheduled_tasks.lock";
const WORKTREE_FILE: &str = ".claude/worktrees/fix-login/src/main.rs";
const LOCK_TEXT: &str = r#"{"sessionId":"55eb6112","pid":4008474,"acquiredAt":1793939007182}"#;

#[test]
fn a_checkpoint_commits_none_of_the_engines_runtime_files_and_the_projects_claude_files() {
    // The project ignores its build output only: nothing names `.claude/`.
    let (checkout, store, _) = project(
        "untracked",
        &[("README.md", "base\n"), (".gitignore", "/build\n")],
    );
    let (workspace, directory) = mount(&store);
    write(&directory, LOCK, LOCK_TEXT);
    write(&directory, WORKTREE_FILE, "fn main() {}\n");
    // The rest of the engine's own list, and its lock at any depth.
    write(&directory, ".claude/scheduled_tasks.json", "{\"tasks\":[]}\n");
    write(&directory, ".claude/checkpoints/one/state", "x\n");
    write(&directory, ".claude/agent-memory-local/notes.md", "x\n");
    write(&directory, "tools/.claude/scheduled_tasks.lock", LOCK_TEXT);
    // What a project commits under `.claude/` on purpose.
    write(&directory, ".claude/settings.json", "{\"model\":\"opus\"}\n");
    write(&directory, ".claude/agents/reviewer.md", "Review.\n");
    write(&directory, ".claude/worktrees.md", "Not a worktree.\n");
    write(&directory, "src/lib.rs", "pub fn f() {}\n");

    assert!(matches!(
        store.commit(&workspace, "work").expect("commit"),
        Commit::Created(_)
    ));
    let files = files_of(&checkout, &agent().to_string());
    for committed in [
        ".claude/settings.json",
        ".claude/agents/reviewer.md",
        ".claude/worktrees.md",
        "src/lib.rs",
    ] {
        assert!(files.iter().any(|file| file == committed), "{committed}: {files:?}");
    }
    for runtime in [
        LOCK,
        WORKTREE_FILE,
        ".claude/scheduled_tasks.json",
        ".claude/checkpoints/one/state",
        ".claude/agent-memory-local/notes.md",
        "tools/.claude/scheduled_tasks.lock",
    ] {
        assert!(!files.iter().any(|file| file == runtime), "{runtime}: {files:?}");
    }
    // The engine rewrites its lock and works in its worktree: no change.
    write(&directory, LOCK, "{\"sessionId\":\"another\"}");
    write(&directory, WORKTREE_FILE, "fn main() { todo!() }\n");
    assert_eq!(
        store.commit(&workspace, "again").expect("commit"),
        Commit::Unchanged
    );
    // The files stay in the workspace: the engine still holds them.
    assert!(directory.join(LOCK).is_file());
    assert!(directory.join(WORKTREE_FILE).is_file());
    store.unmount(&workspace).expect("unmount");
}

/// A runtime file the project already tracks (Supply PA's `main` held
/// `.claude/scheduled_tasks.lock`) is the owner's to remove: a checkpoint
/// treats it as any tracked file, and an index that does not know it (a
/// fresh one after a failure) never drops it. An untracked runtime file
/// beside it still stays out.
#[test]
fn a_runtime_file_the_project_tracks_is_committed_like_any_tracked_file() {
    let (checkout, store, root) = project(
        "tracked",
        &[("README.md", "base\n"), (LOCK, "{\"sessionId\":\"committed\"}")],
    );
    let (workspace, directory) = mount(&store);
    assert!(directory.join(LOCK).is_file(), "the mount exports it");
    write(&directory, LOCK, LOCK_TEXT);
    write(&directory, WORKTREE_FILE, "fn main() {}\n");
    assert!(matches!(
        store.commit(&workspace, "work").expect("commit"),
        Commit::Created(_)
    ));
    let reference = agent().to_string();
    assert_eq!(git(&checkout, &["show", &format!("{reference}:{LOCK}")]), LOCK_TEXT);
    assert!(!files_of(&checkout, &reference).contains(&WORKTREE_FILE.to_string()));

    // A fresh index knows nothing: the tracked file stays all the same.
    fs::remove_file(root.join(".index").join(agent().as_str())).expect("drop the index");
    assert_eq!(
        store.commit(&workspace, "fresh index").expect("commit"),
        Commit::Unchanged
    );

    // The engine releases its lock: the deletion is the workspace's, and a
    // later lock of a new session is untracked, so it stays out.
    fs::remove_file(directory.join(LOCK)).expect("release the lock");
    assert!(matches!(
        store.commit(&workspace, "released").expect("commit"),
        Commit::Created(_)
    ));
    assert!(!files_of(&checkout, &reference).contains(&LOCK.to_string()));
    write(&directory, LOCK, "{\"sessionId\":\"next\"}");
    assert_eq!(
        store.commit(&workspace, "relocked").expect("commit"),
        Commit::Unchanged
    );
    store.unmount(&workspace).expect("unmount");
}

/// A set-aside commits the workspace as it was against a fresh index: the
/// engine's runtime files stay out of that commit too, and a tracked one
/// stays in.
#[test]
fn a_set_aside_keeps_the_work_but_not_the_engines_runtime_files() {
    let (checkout, store, _) = project(
        "set-aside",
        &[("README.md", "base\n"), (LOCK, "{\"sessionId\":\"committed\"}")],
    );
    let (_workspace, directory) = mount(&store);
    write(&directory, "work.txt", "uncommitted work\n");
    write(&directory, WORKTREE_FILE, "fn main() {}\n");
    let SetAside::Created { kept, .. } = store
        .set_aside(&agent(), None, "set aside")
        .expect("set aside")
    else {
        panic!("the work is set aside");
    };
    let files = files_of(&checkout, kept.as_str());
    assert!(files.contains(&"work.txt".to_string()), "{files:?}");
    assert!(files.contains(&LOCK.to_string()), "{files:?}");
    assert!(!files.contains(&WORKTREE_FILE.to_string()), "{files:?}");
}

/// The owner removes a tracked runtime file from `main` while the agent's
/// merge of `main` waits on its markers: the merge took the file out of the
/// workspace, the engine writes its lock again, and neither the checkpoint
/// with the markers in nor the one that resolves them puts it back (which
/// would undo the owner's removal when the branch lands).
#[test]
fn a_runtime_file_main_removes_during_a_pending_merge_stays_removed() {
    let (checkout, store, _) = project(
        "pending",
        &[("notes.txt", "base\n"), (LOCK, "{\"sessionId\":\"committed\"}")],
    );
    let (workspace, directory) = mount(&store);
    write(&directory, "notes.txt", "agent\n");
    assert!(matches!(
        store.commit(&workspace, "agent's notes").expect("commit"),
        Commit::Created(_)
    ));
    write(&checkout, "notes.txt", "main\n");
    git(&checkout, &["rm", "-q", "--cached", LOCK]);
    git(&checkout, &["commit", "-qam", "notes; the lock is not the project's"]);
    let main = git(&checkout, &["rev-parse", "main"]);
    assert!(matches!(
        store.merge_base(&workspace).expect("merge main"),
        BaseMerge::Conflicted { marked: true, .. }
    ));
    assert!(!directory.join(LOCK).exists(), "main's removal is in the workspace");

    write(&directory, LOCK, LOCK_TEXT);
    let reference = agent().to_string();
    assert!(matches!(
        store.commit(&workspace, "markers still in").expect("commit"),
        Commit::Created(_)
    ));
    assert!(!files_of(&checkout, &reference).contains(&LOCK.to_string()));

    write(&directory, "notes.txt", "agent and main\n");
    assert!(matches!(
        store.commit(&workspace, "resolved").expect("commit"),
        Commit::Created(_)
    ));
    let parents = git(&checkout, &["rev-list", "--parents", "-n", "1", &reference]);
    assert!(parents.ends_with(&main), "the resolution is the merge: {parents}");
    assert!(!files_of(&checkout, &reference).contains(&LOCK.to_string()));
    store.unmount(&workspace).expect("unmount");
}

/// git with the store's work tree and index, as the Bridge runs it.
fn git_in(checkout: &Path, directory: &Path, index: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .arg(format!("--git-dir={}", checkout.join(".git").display()))
        .arg(format!("--work-tree={}", directory.display()))
        .env("GIT_INDEX_FILE", index)
        .args(arguments)
        .output()
        .expect("git is installed");
    assert!(output.status.success(), "git {arguments:?}: {output:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A workspace mounted and checkpointed before this version (Supply PA's
/// `spawn checkpoint exec_212`): the branch head and the kept index hold
/// the engine's lock and worktree. `main` never held them, so the next
/// checkpoint takes them out of the branch; the engine keeps its files.
#[test]
fn a_runtime_file_only_a_checkpoint_from_before_put_in_the_branch_leaves_it() {
    let (checkout, store, root) = project("before", &[("README.md", "base\n")]);
    let (workspace, directory) = mount(&store);
    write(&directory, LOCK, LOCK_TEXT);
    write(&directory, WORKTREE_FILE, "fn main() {}\n");
    write(&directory, "src/lib.rs", "pub fn f() {}\n");
    // The checkpoint of the version before: `add -A` against the kept
    // index, whatever the engine wrote.
    let index = root.join(".index").join(agent().as_str());
    fs::create_dir_all(root.join(".index")).expect("create the index directory");
    git_in(&checkout, &directory, &index, &["add", "-A"]);
    let tree = git_in(&checkout, &directory, &index, &["write-tree"]);
    let reference = agent().to_string();
    let head = git(&checkout, &["rev-parse", &reference]);
    let before = git(
        &checkout,
        &["commit-tree", &tree, "-p", &head, "-m", "spawn checkpoint exec_212"],
    );
    git(&checkout, &["update-ref", &format!("refs/heads/{reference}"), &before, &head]);
    assert!(files_of(&checkout, &reference).contains(&LOCK.to_string()));

    assert!(matches!(
        store.commit(&workspace, "after the deploy").expect("commit"),
        Commit::Created(_)
    ));
    let files = files_of(&checkout, &reference);
    assert!(files.contains(&"src/lib.rs".to_string()), "{files:?}");
    assert!(!files.contains(&LOCK.to_string()), "{files:?}");
    assert!(!files.contains(&WORKTREE_FILE.to_string()), "{files:?}");
    assert!(directory.join(LOCK).is_file() && directory.join(WORKTREE_FILE).is_file());
    assert_eq!(
        store.commit(&workspace, "again").expect("commit"),
        Commit::Unchanged
    );
    store.unmount(&workspace).expect("unmount");
}

/// The owner removes a tracked runtime file from `main` (Supply PA's
/// 37641e9) while a branch holds the engine's own version of it: the
/// branch's next checkpoint lets it go as well, so the landing is clean
/// instead of a modify/delete conflict.
#[test]
fn a_runtime_file_the_owner_removes_from_main_leaves_the_branch_and_lands_clean() {
    let (checkout, store, _) = project(
        "removed",
        &[("README.md", "base\n"), (LOCK, "{\"sessionId\":\"committed\"}")],
    );
    let (workspace, directory) = mount(&store);
    write(&directory, LOCK, LOCK_TEXT);
    write(&directory, "work.txt", "work\n");
    assert!(matches!(
        store.commit(&workspace, "work").expect("commit"),
        Commit::Created(_)
    ));
    let reference = agent().to_string();
    assert_eq!(git(&checkout, &["show", &format!("{reference}:{LOCK}")]), LOCK_TEXT);

    git(&checkout, &["rm", "-q", "--cached", LOCK]);
    git(&checkout, &["commit", "-qm", "the lock is not the project's"]);
    assert!(matches!(
        store.commit(&workspace, "after the owner's removal").expect("commit"),
        Commit::Created(_)
    ));
    let files = files_of(&checkout, &reference);
    assert!(!files.contains(&LOCK.to_string()), "{files:?}");
    assert!(files.contains(&"work.txt".to_string()), "{files:?}");
    assert!(directory.join(LOCK).is_file(), "the engine keeps its lock");
    // The landing: `main` first, as the integrator merges.
    let landed = Command::new("git")
        .arg("-C")
        .arg(&checkout)
        .args(["merge-tree", "--write-tree", "main", &reference])
        .output()
        .expect("git is installed");
    assert!(landed.status.success(), "the landing is clean: {landed:?}");
    store.unmount(&workspace).expect("unmount");
}

/// `main` adds a runtime file on purpose (a project that commits its
/// durable schedule) while the agent's merge of it waits on its markers:
/// the merge wrote it into the workspace, and it is the project's, so
/// neither checkpoint drops it (which would delete it from `main` when the
/// branch lands).
#[test]
fn a_runtime_file_main_adds_during_a_pending_merge_stays() {
    const SCHEDULE: &str = ".claude/scheduled_tasks.json";
    let (checkout, store, _) = project("added", &[("notes.txt", "base\n")]);
    let (workspace, directory) = mount(&store);
    write(&directory, "notes.txt", "agent\n");
    assert!(matches!(
        store.commit(&workspace, "agent's notes").expect("commit"),
        Commit::Created(_)
    ));
    write(&checkout, "notes.txt", "main\n");
    write(&checkout, SCHEDULE, "{\"tasks\":[]}\n");
    git(&checkout, &["add", "-A"]);
    git(&checkout, &["commit", "-qm", "notes, and the project's schedule"]);
    assert!(matches!(
        store.merge_base(&workspace).expect("merge main"),
        BaseMerge::Conflicted { marked: true, .. }
    ));
    assert!(directory.join(SCHEDULE).is_file(), "the merge wrote it");

    let reference = agent().to_string();
    assert!(matches!(
        store.commit(&workspace, "markers still in").expect("commit"),
        Commit::Created(_)
    ));
    assert!(files_of(&checkout, &reference).contains(&SCHEDULE.to_string()));
    write(&directory, "notes.txt", "agent and main\n");
    assert!(matches!(
        store.commit(&workspace, "resolved").expect("commit"),
        Commit::Created(_)
    ));
    assert!(files_of(&checkout, &reference).contains(&SCHEDULE.to_string()));
    store.unmount(&workspace).expect("unmount");
}
