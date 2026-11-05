//! Reproductions of audit findings (PR #2, `/tmp/spawn-review/audit.md`)
//! that are still real at this commit. Each is ignored so the suite stays
//! green; run one with `cargo test -p spawn-host --lib <name> -- --ignored`.
//! Each asserts the behaviour the code should have, so it fails today.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use spawn_core::id::BranchName;
use spawn_core::port::{BaseMerge, ChunkError, ChunkStore, Commit, StoredObject, WorkspaceStore};

use crate::chunks::{LocalChunkStore, Manifest, hash_of, store_file};
use crate::git::{GitWorkspace, LARGE_FILE_BYTES};

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

/// H4, symlink: an engine that swaps its large file for a symlink between
/// the controller's lstat (`stat_of`) and its open (`store_file`) gets a
/// host file the controller can read into the chunk store, named by the
/// checkpoint's manifest. The swap is `renameat2(RENAME_EXCHANGE)` there
/// and back, in a loop with a short pause, as a background process in the
/// workspace could run it. (A swap with no pause hides the file from `git
/// ls-files --others`, which lists nothing while the directory keeps
/// changing, so it is never chunked at all.)
#[test]
#[ignore = "audit H4 repro"]
fn audit_h4_a_symlink_swapped_in_during_a_checkpoint_is_chunked_repro() {
    let base = scratch("h4-symlink");
    let (_checkout, store, workspace) = mounted(&base);
    let directory = PathBuf::from(workspace.path());
    let secret = base.join("owner-secret.env");
    std::fs::write(
        &secret,
        "SPAWN_DATABASE_URL=postgres://owner:hunter2@db/spawn\n",
    )
    .expect("secret");
    let secret_chunk = hash_of(&std::fs::read(&secret).expect("read"));
    let big = directory.join("big.bin");
    let link = directory.join("big.lnk");
    std::os::unix::fs::symlink(&secret, &link).expect("symlink");
    let size = usize::try_from(LARGE_FILE_BYTES).expect("fits") + 4096;
    std::fs::write(&big, vec![0u8; size]).expect("the large file");
    // Held open: its content changes in place each attempt (a new stat, so
    // the stat cache never answers), whichever name it has at the time.
    let real = std::fs::OpenOptions::new()
        .write(true)
        .open(&big)
        .expect("open the large file");
    let stop = Arc::new(AtomicBool::new(false));
    let swapper = {
        let (stop, big, link) = (Arc::clone(&stop), big.clone(), link.clone());
        std::thread::spawn(move || {
            let exchange = || {
                let _ = rustix::fs::renameat_with(
                    rustix::fs::CWD,
                    &big,
                    rustix::fs::CWD,
                    &link,
                    rustix::fs::RenameFlags::EXCHANGE,
                );
            };
            // The symlink is at big.bin for one syscall's time, then the
            // file is back: listings and stats mostly see the file.
            while !stop.load(Ordering::Relaxed) {
                exchange();
                exchange();
                std::thread::sleep(Duration::from_micros(100));
            }
        })
    };
    let mut leaked = false;
    for attempt in 0..1000u32 {
        std::os::unix::fs::FileExt::write_all_at(&real, &attempt.to_le_bytes(), 0)
            .expect("rewrite");
        let _ = store.commit(&workspace, "checkpoint");
        // A git killed mid-checkpoint leaves its index lock behind, which
        // fails every later checkpoint (a separate finding, pinned by
        // `audit_sibling_a_dead_gits_index_lock_fails_every_checkpoint_repro`);
        // cleared here so this test sees only the symlink.
        let _ = std::fs::remove_file(base.join("work/.index/agent_a.lock"));
        let chunks = LocalChunkStore::new(base.join("work").join(crate::DEFAULT_CHUNK_DIRECTORY));
        if chunks.contains(&secret_chunk).expect("contains") {
            leaked = true;
            break;
        }
    }
    stop.store(true, Ordering::Relaxed);
    swapper.join().expect("swapper");
    let _ = std::fs::remove_dir_all(&base);
    assert!(
        !leaked,
        "the owner's file was read through a symlink into the chunk store"
    );
}

/// H4, the primitive: `store_file` (and `chunk_names_of`) open the path
/// with a plain open, so a symlink is followed and a FIFO with no writer
/// blocks the caller (the Bridge's tick, holding the branch lock).
#[test]
#[ignore = "audit H4 repro"]
fn audit_h4_store_file_follows_symlinks_and_blocks_on_a_fifo_repro() {
    let base = scratch("h4-primitive");
    let store = LocalChunkStore::new(base.join("store"));
    let secret = base.join("secret");
    std::fs::write(&secret, "owner only\n").expect("secret");
    let link = base.join("link");
    std::os::unix::fs::symlink(&secret, &link).expect("symlink");
    let followed = store_file(&store, &link).is_ok();

    let fifo = base.join("fifo");
    mkfifo(&fifo);
    let returned = {
        let (store, reader) = (store.clone(), fifo.clone());
        finishes_within(
            Duration::from_secs(3),
            move || {
                let _ = store_file(&store, &reader);
            },
            || release_fifo(&fifo),
        )
    };
    let _ = std::fs::remove_dir_all(&base);
    assert!(
        !followed && returned,
        "store_file read a symlink's target: {followed}; returned from a FIFO with no \
         writer: {returned}"
    );
}

/// M14: with a merge of main pending (conflict markers in README.md), the
/// agent puts a FIFO at README.md; the next merge of main (the Bridge's
/// idle offer, under the branch lock) reads it with `std::fs::read` in
/// `unresolved` and blocks until something writes to the FIFO.
#[test]
#[ignore = "audit M14 repro"]
fn audit_m14_a_fifo_at_a_conflicted_path_blocks_the_merge_repro() {
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

/// A chunk store that, when the checkpoint stores its manifest (the last
/// object it writes), first lets an hour and a half pass for every object
/// and runs a collection with the default grace: a checkpoint that took
/// longer than the grace period, with `spawn chunks gc` running meanwhile.
#[derive(Debug)]
struct SlowCheckpoint {
    inner: LocalChunkStore,
    repo: PathBuf,
    collected: AtomicBool,
}

impl SlowCheckpoint {
    fn age_everything(&self) {
        for fan_out in std::fs::read_dir(self.inner.directory())
            .expect("store")
            .flatten()
        {
            if !fan_out.path().is_dir() {
                continue;
            }
            for object in std::fs::read_dir(fan_out.path())
                .expect("fan-out")
                .flatten()
            {
                let _ = std::fs::File::open(object.path()).and_then(|file| {
                    file.set_modified(SystemTime::now() - Duration::from_mins(90))
                });
            }
        }
    }
}

impl ChunkStore for SlowCheckpoint {
    fn put(&self, bytes: &[u8]) -> Result<String, ChunkError> {
        if Manifest::from_bytes(bytes).is_ok() && !self.collected.swap(true, Ordering::Relaxed) {
            self.age_everything();
            crate::gc::collect(
                &self.inner,
                std::slice::from_ref(&self.repo),
                Duration::from_hours(1),
            )
            .expect("the collection runs");
        }
        self.inner.put(bytes)
    }
    fn get(&self, name: &str) -> Result<Vec<u8>, ChunkError> {
        self.inner.get(name)
    }
    fn contains(&self, name: &str) -> Result<bool, ChunkError> {
        self.inner.contains(name)
    }
    fn touch(&self, name: &str) -> Result<bool, ChunkError> {
        self.inner.touch(name)
    }
    fn list(&self) -> Result<Vec<StoredObject>, ChunkError> {
        self.inner.list()
    }
    fn remove_if_older(&self, name: &str, older_than: Duration) -> Result<Option<u64>, ChunkError> {
        self.inner.remove_if_older(name, older_than)
    }
}

/// M13: liveness is what commits name when the collection runs, and a
/// checkpoint's chunks are written before its commit. One that runs past
/// the grace period loses its first chunks to a concurrent collection, then
/// commits a manifest naming them, and every later mount fails.
#[test]
#[ignore = "audit M13 repro"]
fn audit_m13_a_collection_during_a_long_checkpoint_loses_its_chunks_repro() {
    let base = scratch("m13");
    let (checkout, plain, workspace) = mounted(&base);
    let slow = Arc::new(SlowCheckpoint {
        inner: LocalChunkStore::new(base.join("chunks")),
        repo: checkout.join(".git"),
        collected: AtomicBool::new(false),
    });
    let store = plain.with_chunks(slow.clone());
    let size = usize::try_from(LARGE_FILE_BYTES).expect("fits") * 2;
    let bytes: Vec<u8> = (0..size as u64)
        .map(|i| (i.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 29).to_le_bytes()[0])
        .collect();
    std::fs::write(PathBuf::from(workspace.path()).join("model.bin"), &bytes).expect("write");
    let committed = store.commit(&workspace, "large");
    assert!(slow.collected.load(Ordering::Relaxed), "the collection ran");
    let branch = BranchName::new("agent_a").expect("branch");
    let remounted = store.mount(&branch);
    let _ = std::fs::remove_dir_all(&base);
    assert!(matches!(committed, Ok(Commit::Created(_))), "{committed:?}");
    assert!(
        remounted.is_ok(),
        "the checkpoint's own chunks were collected before its commit: {remounted:?}"
    );
}

/// Sibling (found while reproducing H4): a `git add` killed mid-checkpoint
/// (here by SIGBUS, status 135, while the agent swapped the files it was
/// hashing) leaves `<root>/.index/<branch>.lock`. Nothing removes it, so
/// every later checkpoint and merge of the branch fails with "unable to
/// create ... .lock: file exists": the end stalls, and only a discard
/// (which loses the work) gets out. The dead git is simulated by the lock
/// file it leaves.
#[test]
#[ignore = "audit sibling repro"]
fn audit_sibling_a_dead_gits_index_lock_fails_every_checkpoint_repro() {
    let base = scratch("index-lock");
    let (_checkout, store, workspace) = mounted(&base);
    std::fs::create_dir_all(base.join("work/.index")).expect("the index directory");
    std::fs::write(base.join("work/.index/agent_a.lock"), "").expect("the dead git's lock");
    std::fs::write(PathBuf::from(workspace.path()).join("work.txt"), "mine\n").expect("write");
    let first = store.commit(&workspace, "one");
    let second = store.commit(&workspace, "two");
    let _ = std::fs::remove_dir_all(&base);
    assert!(
        matches!(second, Ok(Commit::Created(_))),
        "a dead git's index lock fails every checkpoint: {first:?} then {second:?}"
    );
}
