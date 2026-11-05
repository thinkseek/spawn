//! The audit findings of PR #2 (`/tmp/spawn-review/audit.md`) that still
//! apply to this store, each pinned by the behaviour the code now has:
//! files an agent can swap are opened the safe way, and a git that died
//! mid-checkpoint wedges nothing.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use spawn_core::id::BranchName;
use spawn_core::port::{BaseMerge, Commit, WorkspaceStore};

use crate::git::GitWorkspace;
fn scratch(tag: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!("spawn-audit-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("scratch");
    directory
}

fn git(directory: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(["-c", "user.name=t", "-c", "user.email=t@localhost"])
        .args(arguments)
        .output()
        .expect("git is installed");
    assert!(output.status.success(), "git {arguments:?}: {output:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A repository with `main`, and `agent_a` mounted under `<base>/work`.
fn mounted(base: &Path) -> (PathBuf, GitWorkspace, spawn_core::id::WorkspaceId) {
    let checkout = base.join("repo");
    std::fs::create_dir_all(&checkout).expect("repository");
    git(&checkout, &["init", "-q", "--initial-branch=main"]);
    std::fs::write(checkout.join("README.md"), "base\n").expect("write");
    git(&checkout, &["add", "-A"]);
    git(&checkout, &["commit", "-qm", "base"]);
    let store = GitWorkspace::new(checkout.join(".git"), base.join("work"));
    let branch = BranchName::new("agent_a").expect("branch");
    store.ensure_branch(&branch, None).expect("ensure");
    let workspace = store.mount(&branch).expect("mount");
    (checkout, store, workspace)
}

fn mkfifo(path: &Path) {
    assert!(
        Command::new("mkfifo")
            .arg(path)
            .status()
            .expect("mkfifo")
            .success()
    );
}

/// Run `work` on a thread; whether it returned within `limit`. A thread
/// still blocked is released by `unblock` before this returns.
fn finishes_within(
    limit: Duration,
    work: impl FnOnce() + Send + 'static,
    unblock: impl FnOnce(),
) -> bool {
    let (done, finished) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        work();
        let _ = done.send(());
    });
    let returned = finished.recv_timeout(limit).is_ok();
    if !returned {
        unblock();
    }
    let _ = worker.join();
    returned
}

/// Open the FIFO for writing and close it: a reader blocked in its open
/// gets end of file.
fn release_fifo(path: &Path) {
    let _ = std::fs::OpenOptions::new().write(true).open(path);
}

/// M14: with a merge of main pending (conflict markers in README.md), the
/// agent puts a FIFO at README.md; the next merge of main (the Bridge's
/// idle offer, under the branch lock) used to read it with `std::fs::read`
/// in `unresolved` and block until something wrote to the FIFO. It opens it
/// the safe way now, and returns.
#[test]
fn audit_m14_a_fifo_at_a_conflicted_path_does_not_block_the_merge() {
    let base = scratch("m14");
    let (checkout, store, workspace) = mounted(&base);
    let directory = PathBuf::from(workspace.path());
    std::fs::write(directory.join("README.md"), "mine\n").expect("write");
    assert!(matches!(
        store.commit(&workspace, "mine").expect("commit"),
        Commit::Created(_)
    ));
    std::fs::write(checkout.join("README.md"), "theirs\n").expect("write");
    git(&checkout, &["commit", "-qam", "theirs"]);
    assert!(matches!(
        store.merge_base(&workspace).expect("merge"),
        BaseMerge::Conflicted { marked: true, .. }
    ));
    let readme = directory.join("README.md");
    std::fs::remove_file(&readme).expect("remove");
    mkfifo(&readme);
    let returned = {
        let (store, workspace) = (store.clone(), workspace.clone());
        finishes_within(
            Duration::from_secs(3),
            move || {
                let _ = store.merge_base(&workspace);
            },
            || release_fifo(&readme),
        )
    };
    let _ = std::fs::remove_dir_all(&base);
    assert!(
        returned,
        "merge_base blocked on a FIFO at a conflicted path"
    );
}

/// Sibling (found while reproducing H4): a `git add` killed mid-checkpoint
/// (seen with SIGBUS, status 135, while the agent swapped the files it was
/// hashing) leaves `<root>/.index/<branch>.lock`, and a git killed in a
/// merge leaves `<branch>.merge.lock`. Nothing removed them, so every later
/// checkpoint and merge of the branch failed with "unable to create ...
/// .lock: file exists": the end stalled, and only a discard (which loses
/// the work) got out. Under the branch's lock they are removed now. The
/// dead git is simulated by the lock files it leaves.
#[test]
fn audit_sibling_a_dead_gits_index_lock_fails_no_checkpoint_or_merge() {
    let base = scratch("index-lock");
    let (checkout, store, workspace) = mounted(&base);
    std::fs::create_dir_all(base.join("work/.index")).expect("the index directory");
    std::fs::write(base.join("work/.index/agent_a.lock"), "").expect("the dead git's lock");
    std::fs::write(PathBuf::from(workspace.path()).join("work.txt"), "mine\n").expect("write");
    let first = store.commit(&workspace, "one");
    std::fs::write(base.join("work/.index/agent_a.merge.lock"), "").expect("the merge's lock");
    std::fs::write(checkout.join("other.txt"), "theirs\n").expect("write");
    git(&checkout, &["add", "other.txt"]);
    git(&checkout, &["commit", "-qm", "theirs"]);
    let merged = store.merge_base(&workspace);
    let _ = std::fs::remove_dir_all(&base);
    assert!(
        matches!(first, Ok(Commit::Created(_))),
        "a dead git's index lock fails the checkpoint: {first:?}"
    );
    assert!(
        matches!(merged, Ok(BaseMerge::Merged { .. })),
        "a dead git's merge lock fails the merge: {merged:?}"
    );
}

/// D: an agent that puts a FIFO (or a socket) where a tracked file was used
/// to fail every checkpoint (`git add`: "can only add regular files,
/// symbolic links or git-directories"). The special file is left out of
/// the checkpoint now, and everything else is committed; a FIFO is never
/// opened, so nothing waits on it.
#[test]
fn a_special_file_in_the_workspace_is_left_out_and_the_rest_committed() {
    let base = scratch("special");
    let (checkout, store, workspace) = mounted(&base);
    let directory = PathBuf::from(workspace.path());
    let readme = directory.join("README.md");
    std::fs::remove_file(&readme).expect("remove");
    mkfifo(&readme);
    mkfifo(&directory.join("new.fifo"));
    let socket = std::os::unix::net::UnixListener::bind(directory.join("agent.sock"))
        .expect("a socket in the workspace");
    std::fs::write(directory.join("work.txt"), "mine\n").expect("write");
    let committed = {
        let (store, workspace) = (store.clone(), workspace.clone());
        let fifo = readme.clone();
        let (done, finished) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = done.send(store.commit(&workspace, "with special files"));
        });
        let committed = finished.recv_timeout(Duration::from_secs(10));
        if committed.is_err() {
            release_fifo(&fifo);
        }
        let _ = worker.join();
        committed
    };
    drop(socket);
    let files = git(&checkout, &["ls-tree", "--name-only", "agent_a"]);
    let _ = std::fs::remove_dir_all(&base);
    assert!(
        matches!(committed, Ok(Ok(Commit::Created(_)))),
        "{committed:?}"
    );
    assert_eq!(
        files, "work.txt",
        "the special files are left out, the rest is in"
    );
}
