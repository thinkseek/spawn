//! [`LocalWorkspaceStore`] contract tests for the remaining workspace backends
//! (`Git` and root claim compatibility with `D6`; pure `D6` PG lifecycle tests
//! live in `tests/d6_workspace.rs` and `tests/d6_layer0_cutover.rs`).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use spawn_core::contract;
use spawn_core::id::BranchName;
use spawn_core::port::{
    Branch, Commit, MOUNT_CANCELLED, SetAside, UNMOUNT_IN_USE, WorkspaceError, WorkspaceStore,
};
use spawn_host::{
    BACKEND_MARKER, LocalWorkspaceStore, SET_ASIDE_WAITS, Scratch, WorkspaceBackend, claim_root,
    root_backend,
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
fn constructors_name_their_engine_and_auto_picks_git() {
    let scratch = Scratch::new("local-store-names");
    let repo = seed(&scratch);
    let git_store =
        LocalWorkspaceStore::git(&repo, scratch.join("g"), spawn_host::DEFAULT_MAX_FILE_BYTES);
    assert_eq!(git_store.backend_name(), "git");
    assert_eq!(git_store.backend(), WorkspaceBackend::Git);

    let root = scratch.join("auto");
    assert_eq!(WorkspaceBackend::default(), WorkspaceBackend::Git);
    assert_eq!(WorkspaceBackend::auto(&root), WorkspaceBackend::Git);
    for backend in [WorkspaceBackend::D6, WorkspaceBackend::Git] {
        assert_eq!(WorkspaceBackend::parse(backend.name()), Some(backend));
    }
    for retired in ["auto", "cow", "overlay", "d6-shadow"] {
        assert_eq!(WorkspaceBackend::parse(retired), None, "{retired}");
    }

    let opened_git = LocalWorkspaceStore::open(
        WorkspaceBackend::Git,
        &repo,
        scratch.join("g2"),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    )
    .expect("open git");
    assert_eq!(opened_git.backend(), WorkspaceBackend::Git);
    assert!(
        LocalWorkspaceStore::open(
            WorkspaceBackend::D6,
            &repo,
            scratch.join("d6"),
            spawn_host::DEFAULT_MAX_FILE_BYTES,
        )
        .is_err()
    );
}

#[test]
fn git_engine_mounts_sets_aside_and_unmounts() {
    let scratch = Scratch::new("local-store-git-lifecycle");
    let repo = seed(&scratch);
    let main = git_in(&repo, &["rev-parse", "refs/heads/main"]);
    let main_tree = tree_of(&repo, &main);
    let store = LocalWorkspaceStore::git(
        &repo,
        scratch.join("git-root"),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    );
    let agent = "agent-git";
    let b = branch(agent);
    assert!(matches!(
        store.ensure_branch(&b, None).unwrap(),
        Branch::Created(_)
    ));

    let cancelled = store.mount_when(&b, &|| Err("not wanted".to_string()));
    assert_eq!(code(cancelled), MOUNT_CANCELLED);

    let workspace = store.mount_when(&b, &|| Ok(())).unwrap();
    assert!(store.is_mounted(&workspace));
    assert!(store.hold(&workspace).is_some());
    let view = PathBuf::from(workspace.path());
    let fresh = files(&view);
    assert_eq!(fresh.get("README").unwrap(), b"base readme\n");

    fs::write(view.join("README"), "changed\n").unwrap();
    fs::write(view.join("new.txt"), "new\n").unwrap();
    fs::remove_file(view.join("src/old.rs")).unwrap();
    let first = store.commit(&workspace, "work").unwrap();
    assert!(matches!(first, Commit::Created(_)));
    fs::create_dir_all(view.join("deep/er")).unwrap();
    fs::write(view.join("deep/er/more.txt"), "more\n").unwrap();
    let worked = git_in(&repo, &["rev-parse", &format!("refs/heads/{agent}")]);

    let waits = store.set_aside_when(&b, None, "spawn: set aside #1", &|| {
        Err("a turn runs".to_string())
    });
    assert_eq!(code(waits), SET_ASIDE_WAITS);
    assert_eq!(
        git_in(&repo, &["rev-parse", &format!("refs/heads/{agent}")]),
        worked
    );
    assert!(view.join("deep/er/more.txt").is_file());

    let made = store
        .set_aside_when(&b, None, "spawn: set aside #1", &|| Ok(()))
        .unwrap();
    let SetAside::Created {
        commit,
        kept,
        files: changed,
    } = made.clone()
    else {
        panic!("{made:?}");
    };
    let head = git_in(&repo, &["rev-parse", &format!("refs/heads/{agent}")]);
    assert_eq!(head, commit.to_string());
    assert_eq!(tree_of(&repo, &head), main_tree);
    assert_eq!(git_in(&repo, &["rev-parse", &format!("{kept}^")]), worked);
    assert_eq!(
        git_in(&repo, &["show", &format!("{kept}:deep/er/more.txt")]),
        "more"
    );
    let mut changed = changed;
    changed.sort();
    assert_eq!(
        changed,
        ["README", "deep/er/more.txt", "new.txt", "src/old.rs"]
    );
    assert_eq!(files(&view), fresh);
    assert!(!view.join("deep").exists());
    assert!(store.is_mounted(&workspace));

    let again = store
        .set_aside_when(&b, None, "spawn: set aside #1", &|| Ok(()))
        .unwrap();
    assert_eq!(again, made);
    assert_eq!(
        store
            .set_aside_when(&b, None, "spawn: set aside #2", &|| Ok(()))
            .unwrap(),
        SetAside::Unchanged
    );
    assert_eq!(store.commit(&workspace, "idle").unwrap(), Commit::Unchanged);
    fs::write(view.join("after.txt"), "after\n").unwrap();
    let Commit::Created(after) = store.commit(&workspace, "after").unwrap() else {
        panic!("no commit after the set-aside");
    };
    assert_eq!(
        git_in(&repo, &["rev-parse", &format!("{after}^")]),
        commit.to_string()
    );

    let refused = store.unmount_when(&workspace, &|| {
        Err(WorkspaceError::Refused {
            code: UNMOUNT_IN_USE.to_string(),
            message: "in use".to_string(),
        })
    });
    assert_eq!(code(refused), UNMOUNT_IN_USE);
    assert!(store.is_mounted(&workspace));
    store.unmount_when(&workspace, &|| Ok(())).unwrap();
    assert!(!store.is_mounted(&workspace));
}

#[test]
fn git_engine_sets_aside_an_unmounted_branch() {
    let scratch = Scratch::new("local-store-unmounted");
    let repo = seed(&scratch);
    let main = git_in(&repo, &["rev-parse", "refs/heads/main"]);
    let store = LocalWorkspaceStore::git(
        &repo,
        scratch.join("git-root"),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    );
    let b = branch("idle-git");
    store.ensure_branch(&b, None).unwrap();
    let workspace = store.mount(&b).unwrap();
    let view = PathBuf::from(workspace.path());
    fs::write(view.join("work.txt"), "work\n").unwrap();
    let Commit::Created(work) = store.commit(&workspace, "work").unwrap() else {
        panic!("no commit");
    };
    store.unmount(&workspace).unwrap();

    let made = store
        .set_aside(&b, Some(&main), "spawn: set aside #9")
        .unwrap();
    let SetAside::Created { kept, files, .. } = made else {
        panic!("{made:?}");
    };
    assert_eq!(kept, work);
    assert_eq!(files, ["work.txt"]);
    let workspace = store.mount(&b).unwrap();
    let view = PathBuf::from(workspace.path());
    assert!(!view.join("work.txt").exists());
    assert!(view.join("README").is_file());
    store.unmount(&workspace).unwrap();
}

#[test]
fn git_engine_passes_the_shared_workspace_contract() {
    let scratch = Scratch::new("local-store-contract");
    let repo = seed(&scratch);
    contract::workspace_store(
        "local_git",
        &LocalWorkspaceStore::git(
            &repo,
            scratch.join("git"),
            spawn_host::DEFAULT_MAX_FILE_BYTES,
        ),
    );
}

#[test]
fn a_root_is_claimed_by_one_engine_and_refuses_another() {
    let scratch = Scratch::new("local-store-claim");
    let repo = seed(&scratch);
    let root = scratch.join("root");
    assert_eq!(root_backend(&root).unwrap(), None);
    let git_store = LocalWorkspaceStore::git(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    git_store.claim().unwrap();
    assert_eq!(
        fs::read_to_string(root.join(BACKEND_MARKER)).unwrap(),
        "git\n"
    );
    git_store.claim().unwrap();
    let error = claim_root(&root, WorkspaceBackend::D6).expect_err("another backend");
    assert!(error.contains("git backend"), "{error}");
    assert!(error.contains("SPAWN_WORKSPACE_BACKEND=git"), "{error}");
    assert!(
        error.contains(BACKEND_MARKER),
        "a marker there is named: {error}"
    );
    assert_eq!(root_backend(&root).unwrap(), Some(WorkspaceBackend::Git));
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
    let git = LocalWorkspaceStore::git(&repo, &legacy, spawn_host::DEFAULT_MAX_FILE_BYTES);
    let b = branch("old");
    git.ensure_branch(&b, None).unwrap();
    git.mount(&b).unwrap();
    assert_eq!(root_backend(&legacy).unwrap(), Some(WorkspaceBackend::Git));
    let error = claim_root(&legacy, WorkspaceBackend::D6).expect_err("Git's root");
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

    fs::write(root.join(BACKEND_MARKER), "cow\n").unwrap();
    assert!(
        root_backend(&root).is_err(),
        "a marker that names a retired backend"
    );
    fs::write(root.join(BACKEND_MARKER), "zfs\n").unwrap();
    assert!(root_backend(&root).is_err(), "a marker that names nothing");
}
