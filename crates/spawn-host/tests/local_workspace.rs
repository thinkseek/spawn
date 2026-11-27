//! [`LocalWorkspaceStore`]: the Git, Cow and (on Linux) overlay backends run
//! through one type, and every backend answers the same
//! [`WorkspaceStore`] contract the same way: `mount_when` / `unmount_when`
//! asked under the branch's lock, `is_mounted`, `hold`, and a set-aside
//! (`set_aside_when`) that keeps the work in the branch's history and moves
//! the mounted workspace back to the start's tree, the same commits and the
//! same answers in every store.
//!
//! The overlay backend runs for real (an unprivileged user and mount
//! namespace) wherever the kernel allows it, and is skipped, saying why,
//! anywhere else.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use spawn_core::contract;
use spawn_core::id::BranchName;
use spawn_core::port::{
    Branch, Commit, MOUNT_CANCELLED, SetAside, UNMOUNT_IN_USE, WorkspaceError, WorkspaceStore,
};
use spawn_host::cow::{WORKSPACE_DIFFERS, WORKSPACE_NOT_OWNED};
use spawn_host::{
    BACKEND_MARKER, CowWorkspace, LocalWorkspaceStore, SET_ASIDE_WAITS, Scratch, WorkspaceBackend,
    claim_root, root_backend,
};

fn git(arguments: &[&str]) -> String {
    let output = spawn_host::test_git_command()
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

fn git_in(repo: &Path, arguments: &[&str]) -> String {
    let mut all = vec!["--git-dir", repo.to_str().expect("utf-8 path")];
    all.extend_from_slice(arguments);
    git(&all)
}

/// A bare repository whose `main` holds a few files.
fn seed(base: &Path) -> PathBuf {
    let checkout = base.join("seed");
    let repo = base.join("spawn.git");
    fs::create_dir_all(checkout.join("src")).unwrap();
    git(&["init", "--quiet", "-b", "main", checkout.to_str().unwrap()]);
    fs::write(checkout.join("README"), "base readme\n").unwrap();
    fs::write(checkout.join("src/lib.rs"), "pub fn lib() {}\n").unwrap();
    fs::write(checkout.join("src/old.rs"), "// kept by main\n").unwrap();
    std::os::unix::fs::symlink("README", checkout.join("link")).unwrap();
    let c = checkout.to_str().unwrap();
    git(&["-C", c, "add", "-A"]);
    git(&["-C", c, "commit", "--quiet", "-m", "seed"]);
    git(&["init", "--quiet", "--bare", repo.to_str().unwrap()]);
    git(&["-C", c, "push", "--quiet", repo.to_str().unwrap(), "main"]);
    fs::remove_dir_all(&checkout).unwrap();
    repo
}

fn branch(name: &str) -> BranchName {
    BranchName::new(name.to_string()).unwrap()
}

fn code<T: std::fmt::Debug>(result: Result<T, WorkspaceError>) -> String {
    match result {
        Err(WorkspaceError::Refused { code, .. }) => code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// Every backend this host runs, each over a root of its own under `base`.
fn engines(base: &Path, repo: &Path) -> Vec<LocalWorkspaceStore> {
    let mut stores = vec![
        LocalWorkspaceStore::git(repo, base.join("git-root")),
        LocalWorkspaceStore::cow(repo, base.join("cow-root")),
    ];
    match LocalWorkspaceStore::overlay(repo, base.join("overlay-root")) {
        Ok(store) => stores.push(store),
        Err(why) => eprintln!("overlay backend skipped: {why}"),
    }
    stores
}

/// The files a workspace shows (path to bytes), its own directories
/// (`.git`, `.spawn`) left out.
fn files(directory: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, at: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in fs::read_dir(at).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let name = entry.file_name();
            if at == root && (name == ".git" || name == ".spawn") {
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let kind = entry.file_type().unwrap();
            if kind.is_symlink() {
                let target = fs::read_link(&path).unwrap();
                out.insert(relative, target.as_os_str().as_encoded_bytes().to_vec());
            } else if kind.is_dir() {
                walk(root, &path, out);
            } else {
                out.insert(relative, fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(directory, directory, &mut out);
    out
}

fn tree_of(repo: &Path, commit: &str) -> String {
    git_in(repo, &["rev-parse", &format!("{commit}^{{tree}}")])
}

#[test]
fn constructors_name_their_engine_and_auto_picks_the_cheapest_supported() {
    let scratch = Scratch::new("local-store-names");
    let repo = seed(&scratch);
    assert_eq!(
        LocalWorkspaceStore::git(&repo, scratch.join("g")).backend_name(),
        "git"
    );
    assert_eq!(
        LocalWorkspaceStore::cow(&repo, scratch.join("c")).backend_name(),
        "cow"
    );
    let root = scratch.join("auto");
    let auto = LocalWorkspaceStore::auto(&repo, &root);
    // Never the overlay, even where the kernel mounts one: an engine the
    // loop starts would not see its files.
    let expected = if spawn_host::cow_clone_supported(&root) {
        "cow"
    } else {
        "git"
    };
    assert_eq!(auto.backend_name(), expected);
    assert_eq!(WorkspaceBackend::auto_with(true), WorkspaceBackend::Cow);
    assert_eq!(WorkspaceBackend::auto_with(false), WorkspaceBackend::Git);
    for backend in [
        WorkspaceBackend::Git,
        WorkspaceBackend::Cow,
        WorkspaceBackend::Overlay,
    ] {
        assert_eq!(WorkspaceBackend::parse(backend.name()), Some(backend));
    }
    assert_eq!(WorkspaceBackend::parse("auto"), None);
    let opened = LocalWorkspaceStore::open(WorkspaceBackend::Cow, &repo, scratch.join("c2"));
    assert_eq!(opened.unwrap().backend(), WorkspaceBackend::Cow);
    assert_eq!(auto.root(), spawn_host::canonical_root(&root));
    assert_eq!(auto.cas_root(), auto.root().join(spawn_host::CAS_DIRECTORY));
    match LocalWorkspaceStore::overlay(&repo, scratch.join("o")) {
        Ok(store) => assert_eq!(store.backend_name(), "overlay"),
        Err(why) => assert!(why.contains("overlay"), "{why}"),
    }
}

/// The whole contract, engine by engine: the same calls, the same answers.
#[test]
#[allow(clippy::too_many_lines)] // one backend's whole run, step by step
fn every_engine_mounts_sets_aside_and_unmounts_alike() {
    let scratch = Scratch::new("local-store-parity");
    let repo = seed(&scratch);
    let main = git_in(&repo, &["rev-parse", "refs/heads/main"]);
    let main_tree = tree_of(&repo, &main);
    let mut seen = Vec::new();
    for store in engines(&scratch, &repo) {
        let name = store.backend_name();
        seen.push(name);
        let agent = format!("agent-{name}");
        let b = branch(&agent);
        assert!(matches!(
            store.ensure_branch(&b, None).unwrap(),
            Branch::Created(_)
        ));

        // A mount the caller no longer wants changes nothing.
        let cancelled = store.mount_when(&b, &|| Err("not wanted".to_string()));
        assert_eq!(code(cancelled), MOUNT_CANCELLED, "{name}");

        let workspace = store.mount_when(&b, &|| Ok(())).unwrap();
        assert!(store.is_mounted(&workspace), "{name}");
        assert!(store.hold(&workspace).is_some(), "{name}: hold");
        let launch = store.launch_arguments(&workspace).unwrap();
        assert_eq!(launch.is_some(), name == "overlay", "{name}");
        let view = store.host_path(&workspace).unwrap();
        let fresh = files(&view);
        assert_eq!(fresh.get("README").unwrap(), b"base readme\n", "{name}");

        // Work: one checkpoint, then more nobody checkpointed.
        fs::write(view.join("README"), "changed\n").unwrap();
        fs::write(view.join("new.txt"), "new\n").unwrap();
        fs::remove_file(view.join("src/old.rs")).unwrap();
        let first = store.commit(&workspace, "work").unwrap();
        assert!(matches!(first, Commit::Created(_)), "{name}: {first:?}");
        fs::create_dir_all(view.join("deep/er")).unwrap();
        fs::write(view.join("deep/er/more.txt"), "more\n").unwrap();
        let worked = git_in(&repo, &["rev-parse", &format!("refs/heads/{agent}")]);

        // Waiting on the agent: refused, nothing changes.
        let waits = store.set_aside_when(&b, None, "spawn: set aside #1", &|| {
            Err("a turn runs".to_string())
        });
        assert_eq!(code(waits), SET_ASIDE_WAITS, "{name}");
        assert_eq!(
            git_in(&repo, &["rev-parse", &format!("refs/heads/{agent}")]),
            worked,
            "{name}"
        );
        assert!(view.join("deep/er/more.txt").is_file(), "{name}");

        let made = store
            .set_aside_when(&b, None, "spawn: set aside #1", &|| Ok(()))
            .unwrap();
        let SetAside::Created {
            commit,
            kept,
            files: changed,
        } = made.clone()
        else {
            panic!("{name}: {made:?}");
        };
        let head = git_in(&repo, &["rev-parse", &format!("refs/heads/{agent}")]);
        assert_eq!(head, commit.to_string(), "{name}");
        assert_eq!(tree_of(&repo, &head), main_tree, "{name}");
        // The uncommitted work is kept first, on top of the checkpoint.
        assert_eq!(
            git_in(&repo, &["rev-parse", &format!("{kept}^")]),
            worked,
            "{name}"
        );
        assert_eq!(
            git_in(&repo, &["show", &format!("{kept}:deep/er/more.txt")]),
            "more",
            "{name}"
        );
        let mut changed = changed;
        changed.sort();
        assert_eq!(
            changed,
            ["README", "deep/er/more.txt", "new.txt", "src/old.rs"],
            "{name}"
        );
        // The workspace followed, in place.
        assert_eq!(files(&view), fresh, "{name}");
        assert!(
            !view.join("deep").exists(),
            "{name}: emptied directories go"
        );
        assert!(store.is_mounted(&workspace), "{name}");

        // Asked again with the same message: the same answer, nothing new.
        let again = store
            .set_aside_when(&b, None, "spawn: set aside #1", &|| Ok(()))
            .unwrap();
        assert_eq!(again, made, "{name}");
        // Nothing left to set aside for another task.
        assert_eq!(
            store
                .set_aside_when(&b, None, "spawn: set aside #2", &|| Ok(()))
                .unwrap(),
            SetAside::Unchanged,
            "{name}"
        );
        // The store's record of the workspace is exact: nothing to commit,
        // and new work goes on top of the set-aside.
        assert_eq!(store.commit(&workspace, "idle").unwrap(), Commit::Unchanged);
        fs::write(view.join("after.txt"), "after\n").unwrap();
        let Commit::Created(after) = store.commit(&workspace, "after").unwrap() else {
            panic!("{name}: no commit after the set-aside");
        };
        assert_eq!(
            git_in(&repo, &["rev-parse", &format!("{after}^")]),
            commit.to_string(),
            "{name}"
        );

        // Unmount: refused while the caller says no, then done.
        let refused = store.unmount_when(&workspace, &|| {
            Err(WorkspaceError::Refused {
                code: UNMOUNT_IN_USE.to_string(),
                message: "in use".to_string(),
            })
        });
        assert_eq!(code(refused), UNMOUNT_IN_USE, "{name}");
        assert!(store.is_mounted(&workspace), "{name}");
        store.unmount_when(&workspace, &|| Ok(())).unwrap();
        assert!(!store.is_mounted(&workspace), "{name}");
    }
    assert!(seen.starts_with(&["git", "cow"]), "{seen:?}");
    #[cfg(target_os = "linux")]
    if spawn_host::overlay_supported_in(&scratch).is_ok() {
        assert_eq!(seen, ["git", "cow", "overlay"], "the overlay backend ran");
    }
}

/// A branch with no mounted workspace is set aside in the repository alone,
/// and the next mount shows the start's tree.
#[test]
fn every_engine_sets_aside_an_unmounted_branch_alike() {
    let scratch = Scratch::new("local-store-unmounted");
    let repo = seed(&scratch);
    let main = git_in(&repo, &["rev-parse", "refs/heads/main"]);
    for store in engines(&scratch, &repo) {
        let name = store.backend_name();
        let agent = format!("idle-{name}");
        let b = branch(&agent);
        store.ensure_branch(&b, None).unwrap();
        let workspace = store.mount(&b).unwrap();
        let view = store.host_path(&workspace).unwrap();
        fs::write(view.join("work.txt"), "work\n").unwrap();
        let Commit::Created(work) = store.commit(&workspace, "work").unwrap() else {
            panic!("{name}: no commit");
        };
        store.unmount(&workspace).unwrap();

        let made = store
            .set_aside(&b, Some(&main), "spawn: set aside #9")
            .unwrap();
        let SetAside::Created { kept, files, .. } = made else {
            panic!("{name}: {made:?}");
        };
        assert_eq!(kept, work, "{name}");
        assert_eq!(files, ["work.txt"], "{name}");
        let workspace = store.mount(&b).unwrap();
        let view = store.host_path(&workspace).unwrap();
        assert!(!view.join("work.txt").exists(), "{name}");
        assert!(view.join("README").is_file(), "{name}");
        store.unmount(&workspace).unwrap();
    }
}

/// A file no checkpoint took, at a path the start holds a file at, makes
/// way for it: moved under `.spawn/set-aside/<kept>/`, bytes untouched.
#[test]
fn a_file_in_the_way_of_the_start_is_moved_aside_in_cow_and_overlay() {
    let scratch = Scratch::new("local-store-in-the-way");
    let repo = seed(&scratch);
    for store in engines(&scratch, &repo) {
        let name = store.backend_name();
        if name == "git" {
            // The Git store's own rule (files over the cap) is tested with it.
            continue;
        }
        let agent = format!("way-{name}");
        let b = branch(&agent);
        store.ensure_branch(&b, None).unwrap();
        let workspace = store.mount(&b).unwrap();
        let view = store.host_path(&workspace).unwrap();
        fs::remove_file(view.join("src/old.rs")).unwrap();
        store.commit(&workspace, "drop old").unwrap();
        // A build output (a `target/` directory, which no checkpoint of
        // either engine takes) where the start holds a source file.
        fs::create_dir_all(view.join("src/old.rs/target")).unwrap();
        fs::write(view.join("src/old.rs/target/out.o"), "untracked\n").unwrap();
        let made = store.set_aside(&b, None, "spawn: set aside #3").unwrap();
        let SetAside::Created { kept, .. } = made else {
            panic!("{name}: {made:?}");
        };
        assert_eq!(
            fs::read_to_string(view.join("src/old.rs")).unwrap(),
            "// kept by main\n",
            "{name}"
        );
        let aside = view.join(format!(".spawn/set-aside/{kept}/src/old.rs/target/out.o"));
        assert_eq!(
            fs::read_to_string(&aside).unwrap(),
            "untracked\n",
            "{name}: {}",
            aside.display()
        );
        store.unmount(&workspace).ok();
    }
}

/// The CAS helper: a Cow workspace's artifacts are cloned against
/// `<root>/.cas`, never a transcript; Git and overlay stores have nothing
/// to deduplicate; a prune runs once a CAS exists.
#[test]
fn cas_dedup_shares_artifacts_and_never_touches_transcripts() {
    let scratch = Scratch::new("local-store-cas");
    let repo = seed(&scratch);
    let store = LocalWorkspaceStore::cow(&repo, scratch.join("cow-root"));
    assert!(store.prune_cas(Duration::ZERO).unwrap().is_none());
    let b = branch("builder");
    store.ensure_branch(&b, None).unwrap();
    let workspace = store.mount(&b).unwrap();
    let view = store.host_path(&workspace).unwrap();
    assert!(store.dedup_artifacts(&workspace).unwrap().is_none());

    let blob: Vec<u8> = (0..256 * 1024_u32)
        .map(|at| u8::try_from(at % 251).unwrap())
        .collect();
    for path in [
        "target/debug/a.bin",
        "target/release/a.bin",
        "node_modules/pkg/a.bin",
        "target/.transcripts/session.bin",
        "target/session.jsonl",
    ] {
        let file = view.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, &blob).unwrap();
    }
    let inode = |path: &str| fs::metadata(view.join(path)).unwrap().ino();
    let transcripts = (
        inode("target/.transcripts/session.bin"),
        inode("target/session.jsonl"),
    );
    // Files younger than the pass's quiet time are left alone.
    std::thread::sleep(Duration::from_millis(2_200));
    let report = store
        .dedup_artifacts(&workspace)
        .unwrap()
        .expect("artifacts to look at");
    assert_eq!(
        (
            inode("target/.transcripts/session.bin"),
            inode("target/session.jsonl")
        ),
        transcripts,
        "a transcript is never replaced"
    );
    assert_eq!(fs::read(view.join("target/session.jsonl")).unwrap(), blob);
    if report.cow_supported {
        assert!(report.skipped_transcripts >= 2, "{report:?}");
        assert!(report.deduplicated >= 2, "{report:?}");
        assert!(store.cas_root().is_dir());
        let pruned = store.prune_cas(Duration::ZERO).unwrap().expect("a CAS");
        assert!(pruned.errors.is_empty(), "{pruned:?}");
    } else {
        assert_eq!(report.skipped_transcripts, 0, "{report:?}");
        assert!(
            !store.cas_root().exists(),
            "non-CoW pass must not create CAS root"
        );
        eprintln!(
            "no copy-on-write clones under {}: nothing shared",
            view.display()
        );
    }
    for path in ["target/debug/a.bin", "node_modules/pkg/a.bin"] {
        assert_eq!(fs::read(view.join(path)).unwrap(), blob, "{path}");
    }
    // A checkpoint never takes the artifacts or the CAS.
    store.commit(&workspace, "build").unwrap();
    let listed = git_in(
        &repo,
        &["ls-tree", "-r", "--name-only", "refs/heads/builder"],
    );
    assert!(!listed.contains("target/"), "{listed}");
    assert!(!listed.contains(".cas"), "{listed}");

    let other = LocalWorkspaceStore::git(&repo, scratch.join("git-root"));
    let b = branch("plain");
    other.ensure_branch(&b, None).unwrap();
    let plain = other.mount(&b).unwrap();
    fs::create_dir_all(Path::new(plain.path()).join("target")).unwrap();
    assert!(other.dedup_artifacts(&plain).unwrap().is_none());
}

/// Whether this host must run the overlay backend (the kernel mounts one
/// under `base`): then a test asserts it ran, never skips it.
fn overlay_here(base: &Path) -> bool {
    #[cfg(target_os = "linux")]
    return spawn_host::overlay_supported_in(base).is_ok();
    #[cfg(not(target_os = "linux"))]
    {
        let _ = base;
        false
    }
}

/// The shared `WorkspaceStore` contract the memory fake and the Git store
/// pass, run against the Cow store itself and every [`LocalWorkspaceStore`]
/// backend: the overlay too wherever the kernel mounts one.
#[test]
fn every_engine_passes_the_shared_workspace_contract() {
    let scratch = Scratch::new("local-store-contract");
    let repo = seed(&scratch);
    contract::workspace_store(
        "cow_direct",
        &CowWorkspace::new(&repo, scratch.join("cow-direct")),
    );
    contract::workspace_store(
        "local_git",
        &LocalWorkspaceStore::git(&repo, scratch.join("git")),
    );
    contract::workspace_store(
        "local_cow",
        &LocalWorkspaceStore::cow(&repo, scratch.join("cow")),
    );
    match LocalWorkspaceStore::overlay(&repo, scratch.join("overlay")) {
        Ok(store) => contract::workspace_store("local_overlay", &store),
        Err(why) => assert!(
            !overlay_here(&scratch),
            "the kernel mounts an overlay here, yet the store refused: {why}"
        ),
    }
}

/// A root runs one backend: the first claim writes the marker, the same
/// backend is let in again, another is refused with nothing written; a root
/// from before the marker is read by what is in it.
#[test]
fn a_root_is_claimed_by_one_engine_and_refuses_another() {
    let scratch = Scratch::new("local-store-claim");
    let repo = seed(&scratch);
    let root = scratch.join("root");
    assert_eq!(root_backend(&root).unwrap(), None);
    let cow = LocalWorkspaceStore::cow(&repo, &root);
    cow.claim().unwrap();
    assert_eq!(
        fs::read_to_string(root.join(BACKEND_MARKER)).unwrap(),
        "cow\n"
    );
    cow.claim().unwrap();
    let error = claim_root(&root, WorkspaceBackend::Git).expect_err("another backend");
    assert!(error.contains("cow backend"), "{error}");
    assert!(error.contains("SPAWN_WORKSPACE_BACKEND=cow"), "{error}");
    assert!(
        error.contains(BACKEND_MARKER),
        "a marker there is named: {error}"
    );
    assert_eq!(root_backend(&root).unwrap(), Some(WorkspaceBackend::Cow));
    let leftovers: Vec<_> = fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| {
            name.to_string_lossy()
                .starts_with(&format!("{BACKEND_MARKER}."))
        })
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // Before the marker: a Git workspace in the root makes it Git's.
    let legacy = scratch.join("legacy");
    let git = LocalWorkspaceStore::git(&repo, &legacy);
    let b = branch("old");
    git.ensure_branch(&b, None).unwrap();
    git.mount(&b).unwrap();
    assert_eq!(root_backend(&legacy).unwrap(), Some(WorkspaceBackend::Git));
    let error = claim_root(&legacy, WorkspaceBackend::Cow).expect_err("Git's root");
    assert!(error.contains("git backend"), "{error}");
    assert!(
        error.contains("by what the root holds") && !error.contains(BACKEND_MARKER),
        "no marker is named where there is none: {error}"
    );
    assert!(
        !legacy.join(BACKEND_MARKER).exists(),
        "a refusal writes nothing"
    );
    claim_root(&legacy, WorkspaceBackend::Git).unwrap();

    fs::write(root.join(BACKEND_MARKER), "zfs\n").unwrap();
    assert!(root_backend(&root).is_err(), "a marker that names nothing");
}

/// Check that a set-aside refused because `blocker` (a file no checkpoint
/// takes) sits where the start needs the directory `src/` changed nothing
/// (no kept commit, the branch and the directory as they were), then that
/// the same set-aside asked again once it is gone is made, and asked once
/// more answers the same.
fn refused_then_retried(store: &LocalWorkspaceStore, repo: &Path, agent: &str, view: &Path) {
    let name = store.backend_name();
    let b = branch(agent);
    let head = || git_in(repo, &["rev-parse", &format!("refs/heads/{agent}")]);
    let before = head();
    let refused = store.set_aside(&b, None, "spawn: set aside #5");
    assert_eq!(code(refused), WORKSPACE_DIFFERS, "{name}");
    assert_eq!(head(), before, "{name}: no kept commit, the branch unmoved");
    assert_eq!(
        fs::read_to_string(view.join("README")).unwrap(),
        "uncommitted\n",
        "{name}: the directory did not change"
    );
    fs::remove_file(view.join("src")).unwrap();
    let made = store.set_aside(&b, None, "spawn: set aside #5").unwrap();
    let SetAside::Created { kept, .. } = &made else {
        panic!("{name}: {made:?}");
    };
    assert_eq!(
        git_in(repo, &["show", &format!("{kept}:README")]),
        "uncommitted",
        "{name}: the work is kept"
    );
    assert_eq!(
        store.set_aside(&b, None, "spawn: set aside #5").unwrap(),
        made,
        "{name}: the retry answers the same"
    );
    assert_eq!(
        fs::read_to_string(view.join("src/lib.rs")).unwrap(),
        "pub fn lib() {}\n",
        "{name}"
    );
    assert_eq!(
        fs::read_to_string(view.join("README")).unwrap(),
        "base readme\n"
    );
}

/// A workspace on `store` for `agent` whose branch has no `src/` (the start
/// has one) and whose README holds work no checkpoint took yet.
fn without_src(store: &LocalWorkspaceStore, agent: &str) -> (spawn_core::id::WorkspaceId, PathBuf) {
    let b = branch(agent);
    store.ensure_branch(&b, None).unwrap();
    let workspace = store.mount(&b).unwrap();
    let view = store.host_path(&workspace).unwrap();
    fs::remove_dir_all(view.join("src")).unwrap();
    store.commit(&workspace, "no src").unwrap();
    fs::write(view.join("README"), "uncommitted\n").unwrap();
    (workspace, view)
}

/// A special file (a FIFO, which no backend's checkpoint takes) where the
/// start needs a directory refuses the set-aside before anything moves.
#[test]
fn a_set_aside_blocked_by_a_special_file_changes_nothing_and_is_retried() {
    let scratch = Scratch::new("local-store-differs");
    let repo = seed(&scratch);
    let mut ran = Vec::new();
    for store in engines(&scratch, &repo) {
        let name = store.backend_name();
        if name == "git" {
            continue;
        }
        ran.push(name);
        let agent = format!("fifo-{name}");
        let (workspace, view) = without_src(&store, &agent);
        let made = std::process::Command::new("mkfifo")
            .arg(view.join("src"))
            .status()
            .expect("mkfifo runs");
        assert!(made.success(), "{name}: mkfifo");
        refused_then_retried(&store, &repo, &agent, &view);
        store.unmount(&workspace).unwrap();
    }
    assert!(ran.contains(&"cow"), "{ran:?}");
    if overlay_here(&scratch) {
        assert!(ran.contains(&"overlay"), "{ran:?}");
    }
}

/// A Cow file over the size cap (no checkpoint takes it) where the start
/// needs a directory refuses the set-aside before anything moves.
#[test]
fn a_cow_set_aside_blocked_by_a_file_over_the_cap_changes_nothing_and_is_retried() {
    let scratch = Scratch::new("local-store-over-cap");
    let repo = seed(&scratch);
    let store: LocalWorkspaceStore = CowWorkspace::new(&repo, scratch.join("cow"))
        .with_max_file_bytes(64)
        .into();
    let (workspace, view) = without_src(&store, "capped");
    fs::write(view.join("src"), vec![b'x'; 4096]).unwrap();
    refused_then_retried(&store, &repo, "capped", &view);
    store.unmount(&workspace).unwrap();
}

/// A directory at `<root>/<branch>` this backend did not make (another
/// backend's workspace) is never taken for one it set aside: refused, the
/// branch unmoved.
#[test]
fn a_set_aside_over_another_engines_directory_is_refused() {
    let scratch = Scratch::new("local-store-foreign");
    let repo = seed(&scratch);
    for store in engines(&scratch, &repo) {
        let name = store.backend_name();
        if name == "git" {
            continue;
        }
        let agent = format!("foreign-{name}");
        let b = branch(&agent);
        store.ensure_branch(&b, None).unwrap();
        let workspace = store.mount(&b).unwrap();
        let view = store.host_path(&workspace).unwrap();
        fs::write(view.join("work.txt"), "work\n").unwrap();
        store.commit(&workspace, "work").unwrap();
        store.unmount(&workspace).unwrap();
        let foreign = store.root().join(&agent);
        fs::create_dir_all(&foreign).unwrap();
        fs::write(foreign.join("README"), "a Git checkout\n").unwrap();
        let before = git_in(&repo, &["rev-parse", &format!("refs/heads/{agent}")]);
        let refused = store.set_aside(&b, None, "spawn: set aside #6");
        assert_eq!(code(refused), WORKSPACE_NOT_OWNED, "{name}");
        assert_eq!(
            git_in(&repo, &["rev-parse", &format!("refs/heads/{agent}")]),
            before,
            "{name}"
        );
        assert_eq!(
            fs::read_to_string(foreign.join("README")).unwrap(),
            "a Git checkout\n"
        );
    }
}

/// What no mounted workspace uses goes at a prune: Cow baselines, overlay
/// layers, and the probes a killed process left; what a mounted workspace
/// uses, and a live process's probe, stay.
#[test]
fn a_prune_keeps_only_what_mounted_workspaces_use() {
    let scratch = Scratch::new("local-store-prune");
    let repo = seed(&scratch);
    let main = git_in(&repo, &["rev-parse", "refs/heads/main"]);
    for store in engines(&scratch, &repo) {
        let name = store.backend_name();
        let trees = |store: &LocalWorkspaceStore| -> usize {
            let directory = match name {
                "cow" => store.root().join(".cow/baselines"),
                "overlay" => store.root().join(".layers"),
                _ => return 0,
            };
            fs::read_dir(directory).map_or(0, |entries| {
                entries
                    .filter(|entry| {
                        !entry
                            .as_ref()
                            .unwrap()
                            .file_name()
                            .to_string_lossy()
                            .starts_with('.')
                    })
                    .count()
            })
        };
        let kept = branch(&format!("kept-{name}"));
        let gone = branch(&format!("gone-{name}"));
        store.ensure_branch(&kept, None).unwrap();
        store.ensure_branch(&gone, None).unwrap();
        let kept_ws = store.mount(&kept).unwrap();
        let gone_ws = store.mount(&gone).unwrap();
        // `gone` moves on and is mounted again at its new head.
        fs::write(store.host_path(&gone_ws).unwrap().join("n.txt"), "n\n").unwrap();
        store.commit(&gone_ws, "n").unwrap();
        store.unmount(&gone_ws).unwrap();
        let gone_ws = store.mount(&gone).unwrap();
        let used = trees(&store);
        store.unmount(&gone_ws).unwrap();
        let dead = store.root().join(".spawn-overlay-probe-4294967294-0");
        let live = store
            .root()
            .join(format!(".spawn-overlay-probe-{}-999", std::process::id()));
        fs::create_dir_all(dead.join("upper")).unwrap();
        fs::create_dir_all(&live).unwrap();
        let (pruned, warnings) = store.prune_unused().unwrap();
        assert!(warnings.is_empty(), "{name}: {warnings:?}");
        #[cfg(target_os = "linux")]
        assert!(!dead.exists(), "{name}: a dead probe is swept");
        assert!(live.exists(), "{name}: a live probe stays");
        fs::remove_dir_all(&live).unwrap();
        if name == "git" {
            assert_eq!(pruned, usize::from(cfg!(target_os = "linux")));
        } else {
            assert!(used >= 2, "{name}: {used} trees while both were mounted");
            assert_eq!(trees(&store), 1, "{name}: only `kept`'s head stays");
            assert!(pruned >= 2, "{name}: {pruned}");
        }
        // `kept` still mounts and reads its tree.
        assert!(store.is_mounted(&kept_ws), "{name}");
        assert!(store.host_path(&kept_ws).unwrap().join("README").is_file());
        let _ = &main;
        store.unmount(&kept_ws).unwrap();
    }
}

/// A dead probe that cannot be removed (it holds a transcript, never
/// deleted) is kept and said, and the sweep and the prune go on: the other
/// dead probes go, and so do the baselines no workspace uses.
#[cfg(target_os = "linux")]
#[test]
fn a_probe_that_cannot_be_removed_stops_neither_the_sweep_nor_the_prune() {
    let scratch = Scratch::new("local-store-sweep");
    let repo = seed(&scratch);
    let store = LocalWorkspaceStore::cow(&repo, scratch.join("cow"));
    let b = branch("once");
    store.ensure_branch(&b, None).unwrap();
    let workspace = store.mount(&b).unwrap();
    store.unmount(&workspace).unwrap();
    let baselines = store.root().join(".cow/baselines");
    assert!(fs::read_dir(&baselines).unwrap().next().is_some());
    let kept = store.root().join(".spawn-overlay-probe-4294967293-0");
    let gone = store.root().join(".spawn-overlay-probe-4294967294-1");
    fs::create_dir_all(kept.join("upper")).unwrap();
    fs::write(kept.join("upper/session.jsonl"), "{}\n").unwrap();
    fs::create_dir_all(gone.join("upper")).unwrap();
    let swept = spawn_host::sweep_probes(store.root()).unwrap();
    assert_eq!(swept.removed, 1, "{swept:?}");
    assert_eq!(swept.kept.len(), 1, "{swept:?}");
    assert!(!gone.exists());
    assert!(
        kept.join("upper/session.jsonl").is_file(),
        "a transcript is never deleted"
    );

    let (pruned, warnings) = store.prune_unused().unwrap();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("kept"), "{warnings:?}");
    assert!(pruned >= 1, "the baseline still went: {pruned}");
    let left: Vec<_> = fs::read_dir(&baselines)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| !name.to_string_lossy().starts_with('.'))
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

/// A Cow set-aside reads the start's baseline while a prune runs beside it
/// (another process's unmount): the prune waits for the move, never
/// removes the baseline under it, and every set-aside is made.
#[test]
fn a_cow_set_aside_survives_a_concurrent_prune() {
    let scratch = Scratch::new("local-store-prune-race");
    let repo = seed(&scratch);
    let root = scratch.join("cow");
    let store = LocalWorkspaceStore::cow(&repo, &root);
    let b = branch("racer");
    store.ensure_branch(&b, None).unwrap();
    let workspace = store.mount(&b).unwrap();
    let view = store.host_path(&workspace).unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let pruner = {
        let stop = stop.clone();
        let pruner = CowWorkspace::new(&repo, &root);
        std::thread::spawn(move || {
            let mut runs = 0_u32;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                pruner.prune_baselines().expect("prune");
                runs += 1;
            }
            runs
        })
    };
    for round in 0..12 {
        // Many files, so the move is long enough to meet a prune.
        for file in 0..40 {
            fs::write(view.join(format!("work-{file}.txt")), format!("{round}\n")).unwrap();
        }
        fs::write(view.join("README"), format!("round {round}\n")).unwrap();
        let made = store
            .set_aside(&b, None, &format!("spawn: set aside #{round}"))
            .unwrap_or_else(|error| panic!("round {round}: {error:?}"));
        assert!(matches!(made, SetAside::Created { .. }), "round {round}");
        assert_eq!(
            fs::read_to_string(view.join("README")).unwrap(),
            "base readme\n"
        );
        assert!(!view.join("work-0.txt").exists(), "round {round}");
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(pruner.join().unwrap() > 0, "the prune ran meanwhile");
    store.unmount(&workspace).unwrap();
}
