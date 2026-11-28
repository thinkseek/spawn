//! LayerFS v2 Track A (docs/layerfs-v2-overlay.md): workspaces served by an
//! unprivileged overlayfs over one shared, read-only snapshot layer, with
//! checkpoints taken from the upperdir alone and teardown that drops the
//! upperdir while git commits and transcripts outside it stay.
//!
//! Every test runs in its own scratch directory and skips, saying why, on a
//! host that cannot mount an overlay without privileges (not Linux, no user
//! namespaces).
//!
//! Teardown asks the kernel whether the overlay is still alive (a probe
//! overlay over its workdir), never `/proc`: on a supported host every
//! step of every test runs, and an unexpected refusal (`InUseUnproven`
//! included) fails it ([`unmounted`], [`remounted`]). Unrelated processes,
//! dumpable or not, and the pid namespace the tests run in never matter.
//!
//! Linux only: the store itself is compiled on Linux alone.
#![cfg(target_os = "linux")]
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::Instant;

use spawn_core::id::{BranchName, WorkspaceId};
use spawn_core::port::{Branch, Commit, WORKSPACE_STALE, WorkspaceError, WorkspaceStore};
use spawn_host::Scratch;
use spawn_host::overlay::{
    IN_USE, OverlayWorkspace, STATE_UNREADABLE, TRANSCRIPT_IN_LAYER, TRANSCRIPT_IN_UPPER,
    UPPER_UNREADABLE, overlay_supported_in,
};

/// Stdout of git, which must succeed.
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

/// The scratch for a test, or `None` (the test skips) when this host
/// cannot mount an overlay without privileges.
fn scratch(name: &str) -> Option<Scratch> {
    let scratch = Scratch::new(name);
    match overlay_supported_in(&scratch) {
        Ok(()) => Some(scratch),
        Err(why) => {
            eprintln!("skipped: no unprivileged overlayfs here: {why}");
            None
        }
    }
}

/// Unmount `workspace`, which must succeed: any refusal, `InUseUnproven`
/// included, fails the test.
fn unmounted(store: &OverlayWorkspace, workspace: &WorkspaceId) {
    if let Err(error) = store.unmount(workspace) {
        panic!("unmount failed: {error:?}");
    }
}

/// Mount `name` again over its existing upperdir, which must succeed: any
/// refusal fails the test.
fn remounted(store: &OverlayWorkspace, name: &str) -> WorkspaceId {
    store
        .mount(&branch(name))
        .unwrap_or_else(|error| panic!("mount failed: {error:?}"))
}

/// What a finished unmount leaves of `workspace`: no holder (the store is
/// dropped) and no state. For the prune tests, which need a layer no
/// workspace uses whether or not this host lets an unmount prove itself.
fn forget(store: OverlayWorkspace, workspace: &WorkspaceId) {
    let state = store
        .upper_dir(workspace)
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    drop(store);
    spawn_host::scratch::remove_tree(&state).unwrap();
}

/// A bare repository whose `main` holds `files` (path, content, executable),
/// plus a symlink `link -> README`.
fn seed(base: &Path, files: &[(&str, &[u8], bool)]) -> PathBuf {
    let checkout = base.join("seed");
    let repo = base.join("spawn.git");
    fs::create_dir_all(&checkout).unwrap();
    git(&["init", "--quiet", "-b", "main", checkout.to_str().unwrap()]);
    for (path, content, executable) in files {
        let file = checkout.join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, content).unwrap();
        if *executable {
            fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    std::os::unix::fs::symlink("README", checkout.join("link")).unwrap();
    let c = checkout.to_str().unwrap();
    git(&["-C", c, "add", "-A"]);
    git(&["-C", c, "commit", "--quiet", "-m", "seed"]);
    git(&["init", "--quiet", "--bare", repo.to_str().unwrap()]);
    git(&["-C", c, "push", "--quiet", repo.to_str().unwrap(), "main"]);
    fs::remove_dir_all(&checkout).unwrap();
    repo
}

const SEED: &[(&str, &[u8], bool)] = &[
    ("README", b"base readme\n", false),
    ("src/lib.rs", b"pub fn lib() {}\n", false),
    ("src/old.rs", b"// to be deleted\n", false),
    ("dir/a", b"a\n", false),
    ("dir/b", b"b\n", false),
    ("docs/guide.md", b"# guide\n", false),
    ("bin/run.sh", b"#!/bin/sh\necho run\n", true),
    ("tool.sh", b"#!/bin/sh\n", false),
];

fn branch(name: &str) -> BranchName {
    BranchName::new(name.to_string()).unwrap()
}

/// Run `script` with `sh -c` inside the workspace, which must succeed.
fn sh(store: &OverlayWorkspace, workspace: &WorkspaceId, script: &str) -> String {
    let output = store
        .command(workspace, "sh")
        .unwrap()
        .args(["-c", script])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{script}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Every entry under `directory` that is not a directory (files, symlinks,
/// whiteouts), and the bytes of its regular files.
fn census(directory: &Path) -> (usize, u64) {
    let Ok(entries) = fs::read_dir(directory) else {
        return (0, 0);
    };
    let mut total = (0, 0);
    for entry in entries.flatten() {
        let metadata = fs::symlink_metadata(entry.path()).unwrap();
        if metadata.is_dir() {
            let (count, bytes) = census(&entry.path());
            total.0 += count;
            total.1 += bytes;
        } else {
            total.0 += 1;
            if metadata.is_file() {
                total.1 += metadata.len();
            }
        }
    }
    total
}

/// The tree of `commit`: path to (mode, content).
fn tree(repo: &Path, commit: &str) -> BTreeMap<String, (String, String)> {
    git_in(repo, &["ls-tree", "-r", commit])
        .lines()
        .map(|line| {
            let (meta, path) = line.split_once('\t').unwrap();
            let fields: Vec<&str> = meta.split(' ').collect();
            let content = git_in(repo, &["cat-file", "blob", fields[2]]);
            (path.to_string(), (fields[0].to_string(), content))
        })
        .collect()
}

#[test]
fn probes_unprivileged_overlayfs() {
    let Some(scratch) = scratch("overlay-probe") else {
        return;
    };
    // `scratch` already probed; a second probe in the same place must agree
    // and leave nothing behind.
    overlay_supported_in(&scratch).unwrap();
    assert_eq!(fs::read_dir(&*scratch).unwrap().count(), 0);
}

#[test]
#[allow(clippy::too_many_lines)] // one scenario, told in order
fn five_agents_share_one_lower_layer_and_commit_their_own_diff() {
    let Some(scratch) = scratch("overlay-five") else {
        return;
    };
    let repo = seed(&scratch, SEED);
    let transcripts = scratch.join(".transcripts");
    fs::create_dir_all(&transcripts).unwrap();
    fs::write(transcripts.join("agent-0.jsonl"), b"{\"turn\":1}\n").unwrap();
    let main = git_in(&repo, &["rev-parse", "main"]);
    let store = OverlayWorkspace::new(
        &repo,
        scratch.join("ws"),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    );

    let names: Vec<String> = (0..5).map(|n| format!("agent-{n}")).collect();
    for name in &names {
        assert!(matches!(
            store.ensure_branch(&branch(name), None).unwrap(),
            Branch::Created(_)
        ));
    }
    let workspaces: Vec<WorkspaceId> = std::thread::scope(|scope| {
        let handles: Vec<_> = names
            .iter()
            .map(|name| scope.spawn(|| store.mount(&branch(name)).unwrap()))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // One snapshot layer under all five.
    let layers: Vec<_> = fs::read_dir(store.root().join(".layers"))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(layers.len(), 1, "one shared lower layer");
    let layer = store.layer_path(&main);
    for workspace in &workspaces {
        assert_eq!(store.lower_commit(workspace).unwrap(), main);
        assert!(store.holder_pid(workspace).is_some());
    }

    // Reading every file, from the host and from inside, copies nothing up.
    for workspace in &workspaces {
        let merged = store.host_path(workspace).unwrap();
        assert_eq!(fs::read(merged.join("README")).unwrap(), b"base readme\n");
        assert_eq!(
            fs::read(merged.join("src/lib.rs")).unwrap(),
            b"pub fn lib() {}\n"
        );
        assert_eq!(sh(&store, workspace, "cat dir/a link"), "a\nbase readme\n");
        assert_eq!(sh(&store, workspace, "pwd").trim(), workspace.path());
        // The store sets nothing in a client's environment.
        let output = store
            .command(workspace, "sh")
            .unwrap()
            .env_remove("CARGO_INCREMENTAL")
            .args(["-c", "echo ${CARGO_INCREMENTAL-unset}"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "unset");
        assert_eq!(census(&store.upper_dir(workspace).unwrap()), (0, 0));
    }

    // Conflicting edits of the same file, one per agent, plus one change of
    // each kind.
    for (n, workspace) in workspaces.iter().enumerate() {
        let merged = store.host_path(workspace).unwrap();
        fs::write(merged.join("README"), format!("agent {n}\n")).unwrap();
        sh(
            &store,
            workspace,
            &format!("echo 'pub fn a{n}() {{}}' > src/agent_{n}.rs"),
        );
    }
    sh(&store, &workspaces[0], "rm src/old.rs");
    sh(
        &store,
        &workspaces[1],
        "rm -r dir && mkdir dir && echo c > dir/c",
    );
    sh(
        &store,
        &workspaces[2],
        "mkdir -p target/debug .spawn && echo junk > target/debug/x && echo m > .spawn/m && ln -sfn src/lib.rs link",
    );
    sh(&store, &workspaces[3], "chmod +x tool.sh && rm bin/run.sh");

    // Each agent sees its own edit; the shared layer is untouched.
    for (n, workspace) in workspaces.iter().enumerate() {
        let merged = store.host_path(workspace).unwrap();
        assert_eq!(
            fs::read_to_string(merged.join("README")).unwrap(),
            format!("agent {n}\n")
        );
    }
    assert_eq!(fs::read(layer.join("README")).unwrap(), b"base readme\n");
    assert!(layer.join("src/old.rs").exists() && layer.join("dir/a").exists());
    assert!(
        !store
            .host_path(&workspaces[0])
            .unwrap()
            .join("src/old.rs")
            .exists()
    );
    assert!(
        store
            .host_path(&workspaces[1])
            .unwrap()
            .join("src/old.rs")
            .exists()
    );

    // An agent that changed two files holds two files: nothing unmodified
    // is duplicated.
    assert_eq!(census(&store.upper_dir(&workspaces[4]).unwrap()).0, 2);
    // README, agent_0.rs and the whiteout of old.rs.
    assert_eq!(census(&store.upper_dir(&workspaces[0]).unwrap()).0, 3);

    // Commit all five at once.
    let commits: Vec<Commit> = std::thread::scope(|scope| {
        let handles: Vec<_> = workspaces
            .iter()
            .map(|workspace| scope.spawn(|| store.commit(workspace, "checkpoint").unwrap()))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let base = tree(&repo, &main);
    for (n, (commit, name)) in commits.iter().zip(&names).enumerate() {
        let Commit::Created(id) = commit else {
            panic!("{name}: {commit:?}");
        };
        assert_eq!(git_in(&repo, &["rev-parse", name]), id.as_str());
        assert_eq!(git_in(&repo, &["rev-parse", &format!("{name}^")]), main);
        let mut expected = base.clone();
        expected.insert("README".into(), ("100644".into(), format!("agent {n}")));
        expected.insert(
            format!("src/agent_{n}.rs"),
            ("100644".into(), format!("pub fn a{n}() {{}}")),
        );
        match n {
            0 => {
                expected.remove("src/old.rs");
            }
            1 => {
                expected.remove("dir/a");
                expected.remove("dir/b");
                expected.insert("dir/c".into(), ("100644".into(), "c".into()));
            }
            2 => {
                expected.insert("link".into(), ("120000".into(), "src/lib.rs".into()));
            }
            3 => {
                expected.insert("tool.sh".into(), ("100755".into(), "#!/bin/sh".into()));
                expected.remove("bin/run.sh");
            }
            _ => {}
        }
        assert_eq!(tree(&repo, id.as_str()), expected, "{name}");
    }
    // Nothing new: unchanged.
    for workspace in &workspaces {
        assert_eq!(store.commit(workspace, "again").unwrap(), Commit::Unchanged);
    }

    // Teardown reclaims every upperdir byte and keeps commits and
    // transcripts.
    let heads: Vec<String> = names
        .iter()
        .map(|name| git_in(&repo, &["rev-parse", name]))
        .collect();
    for workspace in &workspaces {
        let upper = store.upper_dir(workspace).unwrap();
        assert!(census(&upper).1 > 0);
        let pid = store.holder_pid(workspace).unwrap();
        unmounted(&store, workspace);
        assert!(!upper.exists(), "upperdir reclaimed");
        assert!(!upper.parent().unwrap().exists(), "state reclaimed");
        assert!(!Path::new(workspace.path()).exists(), "mount point removed");
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "holder stopped"
        );
        assert!(store.holder_pid(workspace).is_none());
    }
    let overlay = store.root().join(".overlay");
    assert_eq!(census(&overlay), (0, 0));
    for (name, head) in names.iter().zip(&heads) {
        assert_eq!(&git_in(&repo, &["rev-parse", name]), head, "commit kept");
    }
    assert_eq!(
        fs::read(transcripts.join("agent-0.jsonl")).unwrap(),
        b"{\"turn\":1}\n"
    );
    // The shared layer outlives its users until pruned.
    assert!(layer.is_dir());
    assert_eq!(store.prune_layers().unwrap(), 1);
    assert!(!layer.exists());
}

#[test]
fn unmount_refuses_while_a_transcript_is_in_the_upperdir() {
    let Some(scratch) = scratch("overlay-transcript") else {
        return;
    };
    let repo = seed(&scratch, SEED);
    let store = OverlayWorkspace::new(
        &repo,
        scratch.join("ws"),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    );
    store.ensure_branch(&branch("agent"), None).unwrap();
    let workspace = store.mount(&branch("agent")).unwrap();
    sh(
        &store,
        &workspace,
        "mkdir -p logs && echo '{}' > logs/session.jsonl",
    );
    let error = store.unmount(&workspace).unwrap_err();
    assert!(
        matches!(&error, WorkspaceError::Refused { code, .. } if code == TRANSCRIPT_IN_UPPER),
        "{error}"
    );
    // Nothing changed: still mounted, the file still there.
    let upper = store.upper_dir(&workspace).unwrap();
    assert!(upper.join("logs/session.jsonl").is_file());
    assert!(store.holder_pid(&workspace).is_some());
    // Moved out of the way (never deleted), the unmount goes on.
    sh(
        &store,
        &workspace,
        "mv logs/session.jsonl logs/session.kept",
    );
    unmounted(&store, &workspace);
    assert!(!upper.exists());
}

#[test]
fn an_upperdir_outlives_its_holder_and_is_mounted_again() {
    let Some(scratch) = scratch("overlay-remount") else {
        return;
    };
    let repo = seed(&scratch, SEED);
    let root = scratch.join("ws");
    let first = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    first.ensure_branch(&branch("agent"), None).unwrap();
    let workspace = first.mount(&branch("agent")).unwrap();
    sh(&first, &workspace, "echo pending > pending.txt");
    let pid = first.holder_pid(&workspace).unwrap();
    // The Bridge goes away: its holders stop with it.
    drop(first);
    assert!(!Path::new(&format!("/proc/{pid}")).exists());

    let second = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    assert!(second.holder_pid(&workspace).is_none());
    let again = remounted(&second, "agent");
    assert_eq!(again, workspace);
    assert_eq!(sh(&second, &workspace, "cat pending.txt"), "pending\n");
    let Commit::Created(id) = second.commit(&workspace, "after restart").unwrap() else {
        panic!("expected a commit");
    };
    assert_eq!(
        git_in(&repo, &["show", &format!("{}:pending.txt", id.as_str())]),
        "pending"
    );
    unmounted(&second, &workspace);
}

#[test]
fn an_overlay_workspace_is_mounted_only_while_its_holder_serves_overlay() {
    let Some(scratch) = scratch("overlay-is-mounted") else {
        return;
    };
    let repo = seed(&scratch, SEED);
    let root = scratch.join("ws");
    let first = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    first.ensure_branch(&branch("agent"), None).unwrap();
    let workspace = first.mount(&branch("agent")).unwrap();
    assert!(first.is_mounted(&workspace));
    drop(first);

    let second = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    assert!(second.holder_pid(&workspace).is_none());
    assert!(!second.is_mounted(&workspace));
    let again = remounted(&second, "agent");
    assert_eq!(again, workspace);
    assert!(second.is_mounted(&workspace));
    unmounted(&second, &workspace);
    assert!(!second.is_mounted(&workspace));
}

#[test]
fn is_mounted_is_false_for_detached_overlay_directories_without_holder() {
    let scratch = Scratch::new("overlay-detached-dir");
    let repo = seed(&scratch, SEED);
    let root = scratch.join("ws");
    let store = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    let merged = store.root().join("agent");
    let workspace = WorkspaceId::new(format!("agent:{}", merged.display())).unwrap();
    let state = store
        .upper_dir(&workspace)
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    fs::create_dir_all(&merged).unwrap();
    fs::create_dir_all(state.join("upper")).unwrap();
    fs::create_dir_all(state.join("work")).unwrap();
    fs::write(state.join("lower"), "deadbeef\n").unwrap();
    assert!(!store.is_mounted(&workspace));
}

/// A process entered into the workspace with `sh -c "<script>"`, once it
/// has printed its first line (`in`): it is inside by then. It lives until
/// [`release`] closes its stdin, when `script` ends in a `read`.
fn enter(store: &OverlayWorkspace, workspace: &WorkspaceId, script: &str) -> Child {
    let mut child = store
        .command(workspace, "sh")
        .unwrap()
        .args(["-c", script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "in", "the process did not get inside");
    child
}

/// Let a process from [`enter`] end, and reap it.
fn release(mut child: Child) {
    drop(child.stdin.take());
    child.wait().unwrap();
}

fn refused_with(result: Result<impl std::fmt::Debug, WorkspaceError>, wanted: &str) -> String {
    match result {
        Err(WorkspaceError::Refused { code, message }) if code == wanted => message,
        other => panic!("expected a {wanted} refusal, got {other:?}"),
    }
}

/// Review #512: a `chmod 000` directory hid a transcript from the count, and
/// the delete then made it readable and removed it. Now what cannot be read
/// refuses the unmount, with no permission changed and nothing deleted.
#[test]
fn unmount_refuses_an_upperdir_it_cannot_read_and_deletes_nothing() {
    let Some(scratch) = scratch("overlay-hidden-transcript") else {
        return;
    };
    let repo = seed(&scratch, SEED);
    let store = OverlayWorkspace::new(
        &repo,
        scratch.join("ws"),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    );
    store.ensure_branch(&branch("agent"), None).unwrap();
    let workspace = store.mount(&branch("agent")).unwrap();
    sh(
        &store,
        &workspace,
        "mkdir hidden && echo '{\"turn\":1}' > hidden/session.jsonl",
    );
    let upper = store.upper_dir(&workspace).unwrap();
    let hidden = upper.join("hidden");
    fs::set_permissions(&hidden, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&hidden).is_ok() {
        fs::set_permissions(&hidden, fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("skipped: mode 000 does not stop this user reading (root?)");
        store.unmount(&workspace).unwrap_err();
        return;
    }
    refused_with(store.unmount(&workspace), UPPER_UNREADABLE);
    // Nothing changed: the mode is still 000, the workspace still mounted.
    let mode = fs::symlink_metadata(&hidden).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o000);
    assert!(store.holder_pid(&workspace).is_some());
    fs::set_permissions(&hidden, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        fs::read(hidden.join("session.jsonl")).unwrap(),
        b"{\"turn\":1}\n"
    );
    // Readable now, the transcript is counted and still kept.
    refused_with(store.unmount(&workspace), TRANSCRIPT_IN_UPPER);
    assert!(hidden.join("session.jsonl").is_file());
    sh(
        &store,
        &workspace,
        "mv hidden/session.jsonl hidden/session.kept",
    );
    unmounted(&store, &workspace);
    assert!(!upper.exists());
}

/// Review #512: a process inside the workspace's namespace outlived the
/// holder, and the unmount deleted the upperdir under its feet. Now the
/// unmount refuses while any process besides the holder is inside.
#[test]
fn unmount_refuses_while_a_process_is_still_inside_the_workspace() {
    let Some(scratch) = scratch("overlay-in-use") else {
        return;
    };
    let repo = seed(&scratch, SEED);
    let store = OverlayWorkspace::new(
        &repo,
        scratch.join("ws"),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    );
    store.ensure_branch(&branch("agent"), None).unwrap();
    let workspace = store.mount(&branch("agent")).unwrap();
    sh(&store, &workspace, "echo pending > pending.txt");
    let child = enter(&store, &workspace, "echo in; read line");
    let message = refused_with(store.unmount(&workspace), IN_USE);
    assert!(message.contains("lease"), "{message}");
    // Nothing changed: still mounted, the edit still there.
    let upper = store.upper_dir(&workspace).unwrap();
    assert!(upper.join("pending.txt").is_file());
    assert!(store.holder_pid(&workspace).is_some());
    assert_eq!(sh(&store, &workspace, "cat pending.txt"), "pending\n");

    release(child);
    unmounted(&store, &workspace);
    assert!(!upper.exists());
}

/// A namespace made inside the workspace inherits the overlay mount
/// without sharing the holder's namespace: its mount table still counts.
#[test]
fn unmount_refuses_while_a_nested_namespace_still_shows_the_overlay() {
    let Some(scratch) = scratch("overlay-nested") else {
        return;
    };
    let repo = seed(&scratch, SEED);
    let store = OverlayWorkspace::new(
        &repo,
        scratch.join("ws"),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    );
    store.ensure_branch(&branch("agent"), None).unwrap();
    let workspace = store.mount(&branch("agent")).unwrap();
    let child = enter(
        &store,
        &workspace,
        "exec unshare --mount sh -c 'echo in; read line'",
    );
    refused_with(store.unmount(&workspace), IN_USE);
    assert!(store.holder_pid(&workspace).is_some());
    release(child);
    unmounted(&store, &workspace);
}

/// Review #512: a process inside the namespace outlived a dropped store,
/// keeping the old overlay alive, and the next mount put a second overlay
/// over the same upperdir. Now a launched client keeps its holder across the
/// drop (its lease), the next store finds that holder, and no second overlay
/// is mounted.
#[test]
fn a_process_outliving_a_dropped_store_blocks_a_second_mount() {
    let Some(scratch) = scratch("overlay-double-mount") else {
        return;
    };
    let repo = seed(&scratch, SEED);
    let root = scratch.join("ws");
    let first = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    first.ensure_branch(&branch("agent"), None).unwrap();
    let workspace = first.mount(&branch("agent")).unwrap();
    sh(&first, &workspace, "echo pending > pending.txt");
    let upper = first.upper_dir(&workspace).unwrap();
    let holder = first.holder_pid(&workspace).unwrap();
    let child = enter(&first, &workspace, "echo in; read line");
    // Review of #556, finding 3: the store's drop honors admission. The
    // client holds the lease, so its holder is left running.
    drop(first);
    assert!(Path::new(&format!("/proc/{holder}/ns/mnt")).exists());

    // The second store finds that live holder and mounts nothing: one
    // overlay over the upperdir.
    let second = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    assert_eq!(second.mount(&branch("agent")).unwrap(), workspace);
    assert_eq!(second.holder_pid(&workspace), Some(holder));
    assert_eq!(namespaces_showing(&upper).len(), 1);
    // The unmount refuses: the client still holds the lease.
    refused_with(second.unmount(&workspace), IN_USE);
    assert!(upper.join("pending.txt").is_file());
    assert_eq!(sh(&second, &workspace, "cat pending.txt"), "pending\n");

    release(child);
    unmounted(&second, &workspace);
    assert!(!upper.exists());
}

#[test]
fn a_branch_moved_under_the_workspace_is_stale_and_a_deleted_one_is_recreated() {
    let Some(scratch) = scratch("overlay-stale") else {
        return;
    };
    let repo = seed(&scratch, SEED);
    let store = OverlayWorkspace::new(
        &repo,
        scratch.join("ws"),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    );
    store.ensure_branch(&branch("agent"), None).unwrap();
    let workspace = store.mount(&branch("agent")).unwrap();
    sh(&store, &workspace, "echo one > one.txt");
    let Commit::Created(first) = store.commit(&workspace, "one").unwrap() else {
        panic!("expected a commit");
    };

    // Deleted under the workspace: made again, on what this host published.
    git_in(&repo, &["update-ref", "-d", "refs/heads/agent"]);
    sh(&store, &workspace, "echo two > two.txt");
    let Commit::Recreated { commit, parent } = store.commit(&workspace, "two").unwrap() else {
        panic!("expected a recreated branch");
    };
    assert_eq!(
        parent.as_ref().map(spawn_core::id::CommitId::as_str),
        Some(first.as_str())
    );
    assert_eq!(git_in(&repo, &["rev-parse", "agent"]), commit.as_str());

    // Moved by another writer: refused, the upperdir kept.
    let main = git_in(&repo, &["rev-parse", "main"]);
    git_in(&repo, &["update-ref", "refs/heads/agent", &main]);
    sh(&store, &workspace, "echo three > three.txt");
    let error = store.commit(&workspace, "three").unwrap_err();
    assert!(
        matches!(&error, WorkspaceError::Refused { code, .. } if code == WORKSPACE_STALE),
        "{error}"
    );
    assert!(
        store
            .upper_dir(&workspace)
            .unwrap()
            .join("three.txt")
            .is_file()
    );
    assert_eq!(git_in(&repo, &["rev-parse", "agent"]), main);
    unmounted(&store, &workspace);
}

#[test]
fn prewarmed_lower_layers_are_shared_and_never_committed() {
    let Some(scratch) = scratch("overlay-deps") else {
        return;
    };
    let repo = seed(&scratch, SEED);
    let deps = scratch.join("deps");
    fs::create_dir_all(deps.join("node_modules/pkg")).unwrap();
    fs::write(
        deps.join("node_modules/pkg/index.js"),
        b"module.exports = 1;\n",
    )
    .unwrap();
    fs::create_dir_all(deps.join("target/debug")).unwrap();
    fs::write(deps.join("target/debug/libdep.rlib"), b"rlib").unwrap();
    let store = OverlayWorkspace::new(
        &repo,
        scratch.join("ws"),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    )
    .with_lower_layers([deps.clone()]);
    let mut workspaces = Vec::new();
    for name in ["a", "b"] {
        store.ensure_branch(&branch(name), None).unwrap();
        workspaces.push(store.mount(&branch(name)).unwrap());
    }
    for workspace in &workspaces {
        assert_eq!(
            sh(
                &store,
                workspace,
                "cat node_modules/pkg/index.js target/debug/libdep.rlib"
            ),
            "module.exports = 1;\nrlib"
        );
        // A build that writes into the pre-warmed target copies up only
        // what it writes, and the checkpoint leaves it out.
        sh(&store, workspace, "echo new > target/debug/incremental");
        assert_eq!(store.commit(workspace, "build").unwrap(), Commit::Unchanged);
        assert_eq!(census(&store.upper_dir(workspace).unwrap()).0, 1);
    }
    assert!(
        !deps.join("target/debug/incremental").exists(),
        "lower untouched"
    );
    for workspace in &workspaces {
        unmounted(&store, workspace);
    }
    assert!(deps.join("node_modules/pkg/index.js").is_file());
}

/// A host process started with `sh -c "<script>"` in `directory`, once it
/// has printed its first line (`in`).
fn start_in(directory: &Path, script: &str) -> Child {
    let mut child = std::process::Command::new("sh")
        .args(["-c", script])
        .current_dir(directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "in", "the process did not start");
    child
}

/// Let a process end by closing its stdin; it must succeed.
fn finish(mut child: Child) {
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success(), "the process failed");
}

/// Whether `python3` runs here (two tests use it to make a process shape
/// `sh` cannot).
fn python3() -> bool {
    let found = std::process::Command::new("python3")
        .arg("-V")
        .output()
        .is_ok_and(|output| output.status.success());
    if !found {
        eprintln!("skipped: no python3");
    }
    found
}

/// The mount namespaces whose table shows an overlay over `upper`.
fn namespaces_showing(upper: &Path) -> std::collections::BTreeSet<PathBuf> {
    let wanted = format!("upperdir={}", upper.display());
    let mut namespaces = std::collections::BTreeSet::new();
    for entry in fs::read_dir("/proc").unwrap().flatten() {
        let process = entry.path();
        if let (Ok(namespace), Ok(table)) = (
            fs::read_link(process.join("ns/mnt")),
            fs::read_to_string(process.join("mountinfo")),
        ) && table
            .lines()
            .any(|line| line.contains(" - overlay ") && line.contains(&wanted))
        {
            namespaces.insert(namespace);
        }
    }
    namespaces
}

/// Review 516 (a) F2, (b) F1: a host process whose working directory is
/// inside `host_path` is in the host's mount namespace, whose table shows
/// no overlay, but it holds the overlay all the same. The unmount refuses
/// while it is there, and its late write lands.
#[test]
fn a_host_process_inside_host_path_blocks_the_unmount() {
    let Some(scratch) = scratch("overlay-host-cwd") else {
        return;
    };
    let repo = seed(&scratch, SEED);
    let store = OverlayWorkspace::new(
        &repo,
        scratch.join("ws"),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    );
    store.ensure_branch(&branch("agent"), None).unwrap();
    let workspace = store.mount(&branch("agent")).unwrap();
    sh(&store, &workspace, "echo pending > pending.txt");
    let merged = store.host_path(&workspace).unwrap();
    let child = start_in(&merged, "echo in; read line; echo late >> pending.txt");
    let message = refused_with(store.unmount(&workspace), IN_USE);
    assert!(message.contains("workdir busy"), "{message}");
    // The host-path endpoint is retired; the overlay it holds, and every
    // byte, stay.
    let upper = store.upper_dir(&workspace).unwrap();
    assert!(upper.join("pending.txt").is_file());
    assert!(store.holder_pid(&workspace).is_none());
    refused_with(store.mount(&branch("agent")), IN_USE);

    finish(child);
    assert_eq!(
        fs::read(upper.join("pending.txt")).unwrap(),
        b"pending\nlate\n"
    );
    remounted(&store, "agent");
    assert_eq!(sh(&store, &workspace, "cat pending.txt"), "pending\nlate\n");
    unmounted(&store, &workspace);
    assert!(!upper.exists());
}

/// Review 516 (a) F2: a file held open through `host_path`, by this very
/// process, blocks the unmount, and, held across a dropped store, the next
/// mount: never two live overlays over one upperdir.
#[test]
fn a_file_held_open_through_host_path_blocks_unmount_and_a_second_mount() {
    use std::io::Write;
    let Some(scratch) = scratch("overlay-host-fd") else {
        return;
    };
    let repo = seed(&scratch, SEED);
    let root = scratch.join("ws");
    let first = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    first.ensure_branch(&branch("agent"), None).unwrap();
    let workspace = first.mount(&branch("agent")).unwrap();
    sh(&first, &workspace, "echo pending > pending.txt");
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(first.host_path(&workspace).unwrap().join("pending.txt"))
        .unwrap();
    refused_with(first.unmount(&workspace), IN_USE);
    let upper = first.upper_dir(&workspace).unwrap();
    drop(first);

    let second = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    refused_with(second.mount(&branch("agent")), IN_USE);
    assert!(second.holder_pid(&workspace).is_none());
    assert!(namespaces_showing(&upper).is_empty());
    refused_with(second.unmount(&workspace), IN_USE);
    file.write_all(b"late\n").unwrap();
    drop(file);

    let again = remounted(&second, "agent");
    assert_eq!(again, workspace);
    assert_eq!(
        sh(&second, &workspace, "cat pending.txt"),
        "pending\nlate\n"
    );
    unmounted(&second, &workspace);
    assert!(!upper.exists());
}

/// Review 516 (a) F2: a file mapped into memory holds the overlay after its
/// descriptor is closed and the process has left the directory, and writes
/// through the mapping reach the upperdir. The unmount refuses while the
/// mapping lives.
#[test]
fn a_file_mapped_through_host_path_blocks_the_unmount() {
    let Some(scratch) = scratch("overlay-host-mmap") else {
        return;
    };
    if !python3() {
        return;
    }
    let repo = seed(&scratch, SEED);
    let store = OverlayWorkspace::new(
        &repo,
        scratch.join("ws"),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    );
    store.ensure_branch(&branch("agent"), None).unwrap();
    let workspace = store.mount(&branch("agent")).unwrap();
    sh(&store, &workspace, "echo pending > pending.txt");
    let merged = store.host_path(&workspace).unwrap();
    // `mmap.mmap` keeps a duplicate of the descriptor: map through libc
    // instead, so the mapping alone holds the file.
    let script = "import ctypes, os, sys\n\
                  libc = ctypes.CDLL(None, use_errno=True)\n\
                  libc.mmap.restype = ctypes.c_void_p\n\
                  libc.mmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int, \
                  ctypes.c_int, ctypes.c_int, ctypes.c_long]\n\
                  fd = os.open('pending.txt', os.O_RDWR)\n\
                  address = libc.mmap(None, 8, 3, 1, fd, 0)\n\
                  assert address not in (None, ctypes.c_void_p(-1).value)\n\
                  os.close(fd)\n\
                  os.chdir('/')\n\
                  print('in', flush=True)\n\
                  sys.stdin.readline()\n\
                  ctypes.memmove(address, b'P', 1)\n\
                  libc.msync(ctypes.c_void_p(address), ctypes.c_size_t(8), 4)\n";
    let mut child = std::process::Command::new("python3")
        .args(["-I", "-c", script])
        .current_dir(&merged)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "in");
    refused_with(store.unmount(&workspace), IN_USE);
    finish(child);
    remounted(&store, "agent");
    assert_eq!(sh(&store, &workspace, "cat pending.txt"), "Pending\n");
    unmounted(&store, &workspace);
}

/// Review 516 (b) F2: a namespace made inside the workspace that lazily
/// unmounts the overlay no longer shows it in its table, but its working
/// directory still holds it. The unmount refuses, the holder alive.
#[test]
fn a_nested_namespace_that_lazily_unmounted_the_overlay_blocks_the_unmount() {
    let Some(scratch) = scratch("overlay-nested-lazy") else {
        return;
    };
    let repo = seed(&scratch, SEED);
    let store = OverlayWorkspace::new(
        &repo,
        scratch.join("ws"),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    );
    store.ensure_branch(&branch("agent"), None).unwrap();
    let workspace = store.mount(&branch("agent")).unwrap();
    let child = enter(
        &store,
        &workspace,
        "exec unshare --mount sh -c 'umount -l \"$PWD\" && echo in; read line; echo late > late.txt'",
    );
    refused_with(store.unmount(&workspace), IN_USE);
    assert!(store.holder_pid(&workspace).is_some());
    finish(child);
    assert_eq!(sh(&store, &workspace, "cat late.txt"), "late\n");
    unmounted(&store, &workspace);
}

/// Review 516 (b) F3: after a dropped store, a process left inside lazily
/// unmounts the overlay and keeps its working directory. The next mount
/// refuses: no second live overlay over the same upperdir.
#[test]
fn a_process_that_lazily_unmounted_after_a_drop_blocks_a_second_mount() {
    use std::io::Write;
    let Some(scratch) = scratch("overlay-drop-lazy") else {
        return;
    };
    let repo = seed(&scratch, SEED);
    let root = scratch.join("ws");
    let first = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    first.ensure_branch(&branch("agent"), None).unwrap();
    let workspace = first.mount(&branch("agent")).unwrap();
    sh(&first, &workspace, "echo pending > pending.txt");
    let upper = first.upper_dir(&workspace).unwrap();
    let mut child = first
        .command(&workspace, "sh")
        .unwrap()
        .args([
            "-c",
            "echo in; read line; umount -l \"$PWD\" && echo umounted; read line; \
             echo late >> pending.txt",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "in");
    drop(first);
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"go\n").unwrap();
    line.clear();
    out.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "umounted");
    assert!(namespaces_showing(&upper).is_empty());

    // The holder lives on (the client holds the lease), but shows no
    // overlay any more: the mount would retire it, and refuses while the
    // client holds the lease; so does the unmount.
    let second = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    refused_with(second.mount(&branch("agent")), IN_USE);
    refused_with(second.unmount(&workspace), IN_USE);
    drop(stdin);
    assert!(child.wait().unwrap().success(), "the late writer failed");
    let again = remounted(&second, "agent");
    assert_eq!(again, workspace);
    assert_eq!(
        sh(&second, &workspace, "cat pending.txt"),
        "pending\nlate\n"
    );
    unmounted(&second, &workspace);
}

/// Review 516 (a) F3: a process whose main thread (the thread-group
/// leader) has exited while another thread goes on inside the workspace:
/// `/proc/<pid>/ns/mnt` and `/proc/<pid>/cwd` are gone (and
/// `/proc/<pid>/mountinfo` answers `EINVAL`), and only
/// `/proc/<pid>/task/<tid>` shows it. The unmount refuses, and after a
/// dropped store so does the next mount.
#[test]
fn a_thread_group_whose_leader_exited_still_holds_the_workspace() {
    let Some(scratch) = scratch("overlay-leader-exit") else {
        return;
    };
    if !python3() {
        return;
    }
    let repo = seed(&scratch, SEED);
    let root = scratch.join("ws");
    let first = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    first.ensure_branch(&branch("agent"), None).unwrap();
    let workspace = first.mount(&branch("agent")).unwrap();
    let script = "import ctypes, os, sys, threading\n\
                  def worker():\n    \
                      sys.stdin.readline()\n    \
                      with open('late.txt', 'w') as f:\n        \
                          f.write('late\\n')\n    \
                      os._exit(0)\n\
                  threading.Thread(target=worker).start()\n\
                  print('in', os.getpid(), flush=True)\n\
                  ctypes.CDLL(None).pthread_exit(None)\n";
    let mut child = first
        .command(&workspace, "python3")
        .unwrap()
        .args(["-I", "-c", script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    // The python process (the launch's supervisor holds the lease).
    let pid = line.trim().strip_prefix("in ").unwrap().to_string();
    // Wait until the leader has left its namespace.
    let leader = PathBuf::from(format!("/proc/{pid}/ns/mnt"));
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    while fs::read_link(&leader).is_ok() && Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(
        fs::read_link(&leader).is_err(),
        "the main thread did not exit"
    );

    refused_with(first.unmount(&workspace), IN_USE);
    let upper = first.upper_dir(&workspace).unwrap();
    drop(first);
    let second = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    assert_eq!(second.mount(&branch("agent")).unwrap(), workspace);
    refused_with(second.unmount(&workspace), IN_USE);
    finish(child);
    assert!(upper.join("late.txt").is_file());
    let again = remounted(&second, "agent");
    assert_eq!(again, workspace);
    unmounted(&second, &workspace);
}

/// Review 516 (a) F1: a layer whose commit tracks a `*.jsonl` fixture is
/// pruned whole (the fixture is the commit's own file), never left half
/// deleted under its commit's name for the next mount to serve. A
/// `*.jsonl` that is not the commit's file refuses the prune of that layer
/// and keeps it whole, set aside under a `.kept-` name (review 539 (a) N5):
/// the next mount builds the commit's own layer, without the stray.
#[test]
fn prune_never_leaves_a_half_deleted_layer() {
    let Some(scratch) = scratch("overlay-prune") else {
        return;
    };
    let files: &[(&str, &[u8], bool)] = &[
        ("README", b"readme\n", false),
        ("a/one.txt", b"1\n", false),
        ("b/fixture.jsonl", b"{}\n", false),
        ("c/three.txt", b"3\n", false),
    ];
    let repo = seed(&scratch, files);
    let main = git_in(&repo, &["rev-parse", "main"]);
    let root = scratch.join("ws");
    let all = "README a/one.txt b/fixture.jsonl c/three.txt link";
    let listing = "for f in README a/one.txt b/fixture.jsonl c/three.txt c/stray.jsonl link; \
                   do [ -e \"$f\" ] && printf '%s ' \"$f\"; done";
    let store = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    store.ensure_branch(&branch("agent"), None).unwrap();
    let layer = store.layer_path(&main);
    let layers = layer.parent().unwrap().to_path_buf();
    let names = || -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&layers)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };

    let workspace = store.mount(&branch("agent")).unwrap();
    forget(store, &workspace);
    let store = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    assert_eq!(store.prune_layers().unwrap(), 1);
    assert!(!layer.exists());
    assert!(names().is_empty(), "{:?}", names());
    let workspace = store.mount(&branch("agent")).unwrap();
    assert_eq!(sh(&store, &workspace, listing).trim(), all);
    forget(store, &workspace);
    let store = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);

    // A `*.jsonl` the commit does not hold: refused, the layer kept whole
    // under a name no mount uses, and the next mount builds a clean one.
    fs::write(layer.join("c/stray.jsonl"), b"{\"turn\":1}\n").unwrap();
    refused_with(store.prune_layers(), TRANSCRIPT_IN_LAYER);
    assert!(
        !layer.exists(),
        "a layer that is not its commit is still served"
    );
    let kept: Vec<String> = names()
        .into_iter()
        .filter(|name| name.starts_with(&format!(".kept-{main}-")))
        .collect();
    assert_eq!(kept.len(), 1, "{:?}", names());
    assert_eq!(
        fs::read(layers.join(&kept[0]).join("c/stray.jsonl")).unwrap(),
        b"{\"turn\":1}\n"
    );
    let workspace = store.mount(&branch("agent")).unwrap();
    assert_eq!(sh(&store, &workspace, listing).trim(), all);
    forget(store, &workspace);
    let store = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    // The commit's fixture, changed: refused and set aside too.
    fs::write(layer.join("b/fixture.jsonl"), b"{\"changed\":1}\n").unwrap();
    refused_with(store.prune_layers(), TRANSCRIPT_IN_LAYER);
    assert!(!layer.exists());
    let kept: Vec<String> = names()
        .into_iter()
        .filter(|name| name.starts_with(".kept-"))
        .collect();
    assert_eq!(kept.len(), 2, "{:?}", names());
    // What is kept is never pruned: it is for a person to look at.
    assert_eq!(store.prune_layers().unwrap(), 0);
    assert_eq!(names().len(), 2);
}

/// Review 539 (a) N3: a workspace state prune cannot read (`chmod 000` on
/// it) refuses the whole prune, before anything is removed: the layer it
/// uses is not taken for unused.
#[test]
fn prune_refuses_a_state_it_cannot_read() {
    let Some(scratch) = scratch("overlay-prune-unreadable") else {
        return;
    };
    let repo = seed(&scratch, SEED);
    let store = OverlayWorkspace::new(
        &repo,
        scratch.join("ws"),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    );
    store.ensure_branch(&branch("agent"), None).unwrap();
    let workspace = store.mount(&branch("agent")).unwrap();
    let main = git_in(&repo, &["rev-parse", "main"]);
    let state = store
        .upper_dir(&workspace)
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&state).is_ok() {
        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("skipped: mode 000 does not stop this user reading (root?)");
        return;
    }
    let refused = store.prune_layers();
    fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
    refused_with(refused, STATE_UNREADABLE);
    assert!(store.layer_path(&main).is_dir(), "a live layer was pruned");
    assert_eq!(sh(&store, &workspace, "cat README"), "base readme\n");
}

/// Review 539 (a) N4: a branch `a/b` once lived inside `a`'s state, and
/// unmounting `a` deleted `a/b`'s upperdir under its live holder. Each
/// state is now one flat name: neither holds the other, and `a`'s unmount
/// leaves `a/b` whole.
#[test]
fn nested_branch_names_never_share_state() {
    let Some(scratch) = scratch("overlay-nested-names") else {
        return;
    };
    let repo = seed(&scratch, SEED);
    let store = OverlayWorkspace::new(
        &repo,
        scratch.join("ws"),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    );
    store.ensure_branch(&branch("a"), None).unwrap();
    let a = store.mount(&branch("a")).unwrap();
    git_in(&repo, &["update-ref", "-d", "refs/heads/a"]);
    store.ensure_branch(&branch("a/b"), None).unwrap();
    let b = store.mount(&branch("a/b")).unwrap();
    sh(&store, &b, "echo mine > mine.txt");
    let (upper_a, upper_b) = (store.upper_dir(&a).unwrap(), store.upper_dir(&b).unwrap());
    let (state_a, state_b) = (upper_a.parent().unwrap(), upper_b.parent().unwrap());
    assert!(
        !state_b.starts_with(state_a) && !state_a.starts_with(state_b),
        "{} and {} nest",
        state_a.display(),
        state_b.display()
    );
    let holder_b = store.holder_pid(&b).unwrap();
    unmounted(&store, &a);
    assert!(upper_b.join("mine.txt").is_file(), "a/b lost its edit");
    assert_eq!(store.holder_pid(&b), Some(holder_b));
    assert_eq!(sh(&store, &b, "cat mine.txt"), "mine\n");
    unmounted(&store, &b);
}

/// Review 516 (a) F5: only an untracked directory with a derived name is
/// left out. A tracked file named `target`, or a tracked directory named
/// `node_modules`, is committed like any other path.
#[test]
fn derived_names_leave_out_only_untracked_directories() {
    let Some(scratch) = scratch("overlay-derived-names") else {
        return;
    };
    let files: &[(&str, &[u8], bool)] = &[
        ("README", b"readme\n", false),
        ("src/target", b"tracked\n", false),
        ("fixtures/node_modules/a.txt", b"a\n", false),
    ];
    let repo = seed(&scratch, files);
    let store = OverlayWorkspace::new(
        &repo,
        scratch.join("ws"),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    );
    store.ensure_branch(&branch("agent"), None).unwrap();
    let workspace = store.mount(&branch("agent")).unwrap();
    sh(
        &store,
        &workspace,
        "echo edited > src/target && echo b > fixtures/node_modules/a.txt \
         && mkdir -p target/debug && echo out > target/debug/bin",
    );
    let Commit::Created(edit) = store.commit(&workspace, "edit").unwrap() else {
        panic!("expected a commit");
    };
    let files = tree(&repo, edit.as_str());
    assert_eq!(files["src/target"].1, "edited");
    assert_eq!(files["fixtures/node_modules/a.txt"].1, "b");
    assert!(!files.contains_key("target/debug/bin"));
    sh(&store, &workspace, "rm src/target");
    let Commit::Created(removal) = store.commit(&workspace, "rm").unwrap() else {
        panic!("expected a commit");
    };
    assert!(!tree(&repo, removal.as_str()).contains_key("src/target"));
    unmounted(&store, &workspace);
}

/// Review 516 (a) F6: two stores mounting one branch at once, with no
/// holder alive, make one overlay: the second finds the first's holder.
#[test]
fn two_stores_mounting_one_branch_at_once_make_one_overlay() {
    let Some(scratch) = scratch("overlay-concurrent-mount") else {
        return;
    };
    let repo = seed(&scratch, SEED);
    let root = scratch.join("ws");
    let a = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    let b = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    a.ensure_branch(&branch("agent"), None).unwrap();
    for _ in 0..5 {
        let barrier = std::sync::Barrier::new(2);
        let (first, second) = std::thread::scope(|scope| {
            let one = scope.spawn(|| {
                barrier.wait();
                a.mount(&branch("agent"))
            });
            let two = scope.spawn(|| {
                barrier.wait();
                b.mount(&branch("agent"))
            });
            (one.join().unwrap(), two.join().unwrap())
        });
        let workspace = first.unwrap();
        assert_eq!(second.unwrap(), workspace);
        let upper = a.upper_dir(&workspace).unwrap();
        assert_eq!(namespaces_showing(&upper).len(), 1);
        unmounted(&a, &workspace);
        assert!(!upper.exists());
    }
}

/// Review 516 (a) R10, (b) F4: a recorded holder pid that now names a live
/// stranger (pid reuse), with the rest of the record (namespace, device,
/// start time) the old holder's, is neither taken for the holder nor
/// killed by the unmount. Review 539 (a) N2: this test flaked when the
/// kernel gave the stranger's new namespace the old one's freed inode; the
/// start time now tells them apart whatever the inode (pinned without any
/// reuse by the unit test
/// `a_stranger_with_the_recorded_pid_and_namespace_is_not_the_holder`), and
/// after the stop the kernel's workdir lock, not a device number that may
/// have been handed on, decides.
#[test]
fn a_reused_holder_pid_is_neither_trusted_nor_killed() {
    let Some(scratch) = scratch("overlay-pid-reuse") else {
        return;
    };
    let repo = seed(&scratch, SEED);
    let root = scratch.join("ws");
    let first = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    first.ensure_branch(&branch("agent"), None).unwrap();
    let workspace = first.mount(&branch("agent")).unwrap();
    let state = first
        .upper_dir(&workspace)
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    drop(first);
    let recorded = fs::read_to_string(state.join("holder")).unwrap();
    let mut stranger = std::process::Command::new("unshare")
        .args([
            "--user",
            "--map-root-user",
            "--mount",
            "sh",
            "-c",
            "echo in; read line",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(stranger.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "in");
    let mut forged = vec![stranger.id().to_string()];
    forged.extend(recorded.split_whitespace().skip(1).map(str::to_string));
    fs::write(state.join("holder"), format!("{}\n", forged.join(" "))).unwrap();

    let second = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    assert!(second.holder_pid(&workspace).is_none());
    assert!(second.command(&workspace, "true").is_err());
    // Done or refused as unproven, the stranger lives on.
    unmounted(&second, &workspace);
    assert!(
        matches!(stranger.try_wait(), Ok(None)),
        "the stranger was killed"
    );
    drop(stranger.stdin.take());
    stranger.wait().unwrap();
}

/// Review 516 (a) R19: deleting a tracked `*.jsonl` leaves a whiteout of
/// that name in the upperdir. A whiteout is no transcript: the deletion is
/// committed and the unmount goes on.
#[test]
fn a_whiteout_named_like_a_transcript_does_not_block_the_unmount() {
    let Some(scratch) = scratch("overlay-jsonl-whiteout") else {
        return;
    };
    let files: &[(&str, &[u8], bool)] = &[
        ("README", b"readme\n", false),
        ("fixture.jsonl", b"{}\n", false),
    ];
    let repo = seed(&scratch, files);
    let store = OverlayWorkspace::new(
        &repo,
        scratch.join("ws"),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    );
    store.ensure_branch(&branch("agent"), None).unwrap();
    let workspace = store.mount(&branch("agent")).unwrap();
    sh(&store, &workspace, "rm fixture.jsonl");
    let Commit::Created(id) = store.commit(&workspace, "rm").unwrap() else {
        panic!("expected a commit");
    };
    assert!(!tree(&repo, id.as_str()).contains_key("fixture.jsonl"));
    let upper = store.upper_dir(&workspace).unwrap();
    assert!(fs::symlink_metadata(upper.join("fixture.jsonl")).is_ok());
    unmounted(&store, &workspace);
    assert!(!upper.exists());
}

/// Review 516 (b) F4: a tracked directory replaced by a file, and a
/// tracked file replaced by a directory, commit as such (`--replace`).
#[test]
fn a_directory_replaced_by_a_file_and_a_file_by_a_directory_commit() {
    let Some(scratch) = scratch("overlay-replace") else {
        return;
    };
    let repo = seed(&scratch, SEED);
    let store = OverlayWorkspace::new(
        &repo,
        scratch.join("ws"),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    );
    store.ensure_branch(&branch("agent"), None).unwrap();
    let workspace = store.mount(&branch("agent")).unwrap();
    sh(
        &store,
        &workspace,
        "rm -r dir && echo now-a-file > dir && rm tool.sh && mkdir tool.sh \
         && echo inner > tool.sh/inner",
    );
    let Commit::Created(id) = store.commit(&workspace, "replace").unwrap() else {
        panic!("expected a commit");
    };
    let files = tree(&repo, id.as_str());
    assert_eq!(files["dir"].1, "now-a-file");
    assert!(!files.contains_key("dir/a") && !files.contains_key("dir/b"));
    assert_eq!(files["tool.sh/inner"].1, "inner");
    unmounted(&store, &workspace);
}

/// A file held open from a pre-warmed layer on another filesystem (here
/// `/dev/shm`) is seen too: `xino=on` makes it report the overlay's device,
/// where without it it reports a per-layer pseudo device. Skips where
/// `/dev/shm` is missing or on the scratch's own filesystem.
#[test]
fn a_file_held_open_from_a_lower_layer_on_another_filesystem_blocks_the_unmount() {
    use std::os::unix::fs::MetadataExt;
    let Some(scratch) = scratch("overlay-xino") else {
        return;
    };
    let shm = Path::new("/dev/shm");
    if !shm.is_dir() || fs::metadata(shm).unwrap().dev() == fs::metadata(&*scratch).unwrap().dev() {
        eprintln!("skipped: no /dev/shm on a filesystem of its own");
        return;
    }
    let deps = Scratch::new_in(shm, "overlay-xino-deps");
    fs::create_dir_all(deps.join("node_modules/pkg")).unwrap();
    fs::write(deps.join("node_modules/pkg/index.js"), b"1;\n").unwrap();
    let repo = seed(&scratch, SEED);
    let store = OverlayWorkspace::new(
        &repo,
        scratch.join("ws"),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    )
    .with_lower_layers([deps.to_path_buf()]);
    store.ensure_branch(&branch("agent"), None).unwrap();
    let workspace = store.mount(&branch("agent")).unwrap();
    let file = fs::File::open(
        store
            .host_path(&workspace)
            .unwrap()
            .join("node_modules/pkg/index.js"),
    )
    .unwrap();
    refused_with(store.unmount(&workspace), IN_USE);
    drop(file);
    remounted(&store, "agent");
    unmounted(&store, &workspace);
}

/// A python3 host process in `directory` that makes itself not dumpable
/// (`PR_SET_DUMPABLE` 0, as `spawn loop`, `serve` and `integrate` do,
/// `crates/spawn-cli/src/main.rs`), once it says `in`. On a line on stdin
/// it appends `late` to `pending.txt` and exits.
fn undumpable_in(directory: &Path) -> Child {
    let script = "import ctypes, sys\n\
                  assert ctypes.CDLL(None).prctl(4, 0, 0, 0, 0) == 0\n\
                  print('in', flush=True)\n\
                  sys.stdin.readline()\n\
                  open('pending.txt', 'a').write('late\\n')\n";
    let mut child = std::process::Command::new("python3")
        .args(["-I", "-c", script])
        .current_dir(directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "in", "the process did not start");
    child
}

/// Review 539 (a) N1, (b) F1, decided by #556: a host process of this uid
/// inside `host_path` that is not dumpable (its working directory cannot
/// be read through `/proc`) is a live user of the overlay all the same.
/// The kernel says so after the holder stops: the unmount refuses `InUse`
/// (never `InUseUnproven`) and deletes nothing; its late write lands, and
/// once it is gone the workspace is mounted again and torn down.
#[test]
fn a_non_dumpable_host_process_inside_host_path_blocks_the_unmount() {
    let Some(scratch) = scratch("overlay-undumpable-cwd") else {
        return;
    };
    if !python3() {
        return;
    }
    let repo = seed(&scratch, SEED);
    let store = OverlayWorkspace::new(
        &repo,
        scratch.join("ws"),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    );
    store.ensure_branch(&branch("agent"), None).unwrap();
    let workspace = store.mount(&branch("agent")).unwrap();
    sh(&store, &workspace, "echo pending > pending.txt");
    let child = undumpable_in(&store.host_path(&workspace).unwrap());
    let message = refused_with(store.unmount(&workspace), IN_USE);
    assert!(message.contains("workdir busy"), "{message}");
    let upper = store.upper_dir(&workspace).unwrap();
    assert!(upper.join("pending.txt").is_file());
    refused_with(store.mount(&branch("agent")), IN_USE);
    finish(child);
    assert_eq!(
        fs::read(upper.join("pending.txt")).unwrap(),
        b"pending\nlate\n"
    );
    remounted(&store, "agent");
    unmounted(&store, &workspace);
    assert!(!upper.exists());
}

/// Review 539 (a) N1, (b) F1: the same process held across a dropped
/// store. The next mount refuses `InUse`: the kernel holds the old
/// overlay's workdir busy, whatever `/proc` can read. No second overlay is
/// mounted over the upperdir, and the late write lands in it.
#[test]
fn a_non_dumpable_host_process_blocks_a_second_mount_after_a_drop() {
    let Some(scratch) = scratch("overlay-undumpable-drop") else {
        return;
    };
    if !python3() {
        return;
    }
    let repo = seed(&scratch, SEED);
    let root = scratch.join("ws");
    let first = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    first.ensure_branch(&branch("agent"), None).unwrap();
    let workspace = first.mount(&branch("agent")).unwrap();
    sh(&first, &workspace, "echo pending > pending.txt");
    let child = undumpable_in(&first.host_path(&workspace).unwrap());
    let upper = first.upper_dir(&workspace).unwrap();
    drop(first);

    let second = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    let message = refused_with(second.mount(&branch("agent")), IN_USE);
    assert!(message.contains("workdir busy"), "{message}");
    assert!(second.holder_pid(&workspace).is_none());
    refused_with(second.unmount(&workspace), IN_USE);
    assert!(namespaces_showing(&upper).is_empty(), "a second overlay");
    finish(child);
    assert_eq!(
        fs::read(upper.join("pending.txt")).unwrap(),
        b"pending\nlate\n"
    );
    let again = remounted(&second, "agent");
    assert_eq!(again, workspace);
    assert_eq!(
        sh(&second, &workspace, "cat pending.txt"),
        "pending\nlate\n"
    );
}

/// Review 539 (b) F1: a binary that may be run but not read (mode 0111)
/// makes its process non-dumpable without any `prctl`. Started inside
/// `host_path`, it blocks the unmount (`InUse`, from the kernel) until it
/// is gone.
#[test]
fn an_execute_only_binary_inside_host_path_blocks_the_unmount() {
    let Some(scratch) = scratch("overlay-exec-only") else {
        return;
    };
    let sleep = scratch.join("sleep");
    fs::copy("/bin/sleep", &sleep).unwrap();
    fs::set_permissions(&sleep, fs::Permissions::from_mode(0o111)).unwrap();
    let repo = seed(&scratch, SEED);
    let store = OverlayWorkspace::new(
        &repo,
        scratch.join("ws"),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    );
    store.ensure_branch(&branch("agent"), None).unwrap();
    let workspace = store.mount(&branch("agent")).unwrap();
    let mut child = std::process::Command::new(&sleep)
        .arg("30")
        .current_dir(store.host_path(&workspace).unwrap())
        .spawn()
        .unwrap();
    // Running the binary (no longer the forked copy of this process).
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    while fs::read_link(format!("/proc/{}/cwd", child.id())).is_ok() && Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    refused_with(store.unmount(&workspace), IN_USE);
    child.kill().unwrap();
    child.wait().unwrap();
    remounted(&store, "agent");
    unmounted(&store, &workspace);
}

/// Owner question 1 of task 539 (a reference held by no process), the case
/// of a descriptor in flight: a file opened through `host_path` is sent
/// over a unix socket (`SCM_RIGHTS`) and closed, and never received. No
/// process has it open, so `/proc` shows nothing, but the kernel holds the
/// old overlay's workdir busy: after a dropped store the next mount refuses
/// `InUse`.
#[test]
fn a_descriptor_in_flight_in_a_unix_socket_blocks_a_second_mount() {
    let Some(scratch) = scratch("overlay-scm-rights") else {
        return;
    };
    if !python3() {
        return;
    }
    let repo = seed(&scratch, SEED);
    let root = scratch.join("ws");
    let first = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    first.ensure_branch(&branch("agent"), None).unwrap();
    let workspace = first.mount(&branch("agent")).unwrap();
    sh(&first, &workspace, "echo pending > pending.txt");
    let script = "import os, socket, sys\n\
                  a, b = socket.socketpair()\n\
                  fd = os.open('pending.txt', os.O_RDONLY)\n\
                  socket.send_fds(a, [b'x'], [fd])\n\
                  os.close(fd)\n\
                  os.chdir('/')\n\
                  print('in', flush=True)\n\
                  sys.stdin.readline()\n";
    let mut child = std::process::Command::new("python3")
        .args(["-I", "-c", script])
        .current_dir(first.host_path(&workspace).unwrap())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "in");
    drop(first);

    let second = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    let message = refused_with(second.mount(&branch("agent")), IN_USE);
    assert!(message.contains("workdir busy"), "{message}");
    assert!(
        !message.contains(&format!("process(es) {}", child.id())),
        "the scan saw the descriptor in flight after all: {message}"
    );
    finish(child);
    let again = remounted(&second, "agent");
    assert_eq!(again, workspace);
}

/// Review 539 (b) N1: a holder record in an older format (no start time)
/// names no live holder. It is not trusted (no `host_path`), its holder is
/// not killed by another store, and that store's unmount refuses `InUse`:
/// the kernel still sees the overlay the untrusted holder keeps.
#[test]
fn a_holder_record_without_its_fields_is_not_trusted() {
    let Some(scratch) = scratch("overlay-old-record") else {
        return;
    };
    let repo = seed(&scratch, SEED);
    let root = scratch.join("ws");
    let first = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    first.ensure_branch(&branch("agent"), None).unwrap();
    let workspace = first.mount(&branch("agent")).unwrap();
    let state = first
        .upper_dir(&workspace)
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let holder = first.holder_pid(&workspace).unwrap();
    let recorded = fs::read_to_string(state.join("holder")).unwrap();
    let old: Vec<&str> = recorded.split_whitespace().take(2).collect();
    fs::write(state.join("holder"), format!("{}\n", old.join(" "))).unwrap();

    let second = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    assert!(second.holder_pid(&workspace).is_none());
    assert!(second.host_path(&workspace).is_err());
    refused_with(second.unmount(&workspace), IN_USE);
    assert!(
        Path::new(&format!("/proc/{holder}")).exists(),
        "the holder was killed"
    );
    assert!(state.join("upper").is_dir() && state.join("work").is_dir());
    fs::write(state.join("holder"), recorded).unwrap();
    assert_eq!(first.holder_pid(&workspace), Some(holder));
    unmounted(&first, &workspace);
}

/// `cargo test -p spawn-host --test overlay -- --ignored --nocapture`:
/// disk and wall time of five overlay workspaces against five full working
/// copies of the same tree, each agent editing one file.
#[test]
#[ignore = "benchmark"]
fn benchmark_five_overlays_against_five_full_copies() {
    let Some(scratch) = scratch("overlay-bench") else {
        return;
    };
    let files: usize = std::env::var("SPAWN_OVERLAY_BENCH_FILES")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(4000);
    let content: Vec<u8> = (0..4096u32)
        .map(|i| b'a' + u8::try_from(i % 26).unwrap())
        .collect();
    let owned: Vec<(String, Vec<u8>, bool)> = (0..files)
        .map(|n| (format!("src/m{}/f{n}.rs", n % 64), content.clone(), false))
        .collect();
    let seeded: Vec<(&str, &[u8], bool)> = owned
        .iter()
        .map(|(p, c, x)| (p.as_str(), c.as_slice(), *x))
        .collect();
    let mut all = vec![("README", &b"bench\n"[..], false)];
    all.extend(seeded);
    let repo = seed(&scratch, &all);

    // Five full working copies.
    let started = Instant::now();
    let mut full_bytes = 0;
    for n in 0..5 {
        let copy = scratch.join(format!("copy-{n}"));
        fs::create_dir_all(&copy).unwrap();
        let index = scratch.join(format!("copy-{n}.index"));
        let checkout = |arguments: &[&str]| {
            let status = spawn_host::test_git_command()
                .env("GIT_INDEX_FILE", &index)
                .arg(format!("--git-dir={}", repo.display()))
                .arg(format!("--work-tree={}", copy.display()))
                .args(arguments)
                .status()
                .unwrap();
            assert!(status.success(), "git {arguments:?}");
        };
        checkout(&["read-tree", "main"]);
        checkout(&["checkout-index", "-a"]);
        fs::write(copy.join("README"), format!("agent {n}\n")).unwrap();
        full_bytes += census(&copy).1;
    }
    let full_time = started.elapsed();

    // Five overlays over one layer.
    let store = OverlayWorkspace::new(
        &repo,
        scratch.join("ws"),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    );
    let started = Instant::now();
    let mut workspaces = Vec::new();
    for n in 0..5 {
        let name = branch(&format!("agent-{n}"));
        store.ensure_branch(&name, None).unwrap();
        let workspace = store.mount(&name).unwrap();
        fs::write(
            store.host_path(&workspace).unwrap().join("README"),
            format!("agent {n}\n"),
        )
        .unwrap();
        workspaces.push(workspace);
    }
    let overlay_time = started.elapsed();
    let main = git_in(&repo, &["rev-parse", "main"]);
    let layer_bytes = census(&store.layer_path(&main)).1;
    let upper_bytes: u64 = workspaces
        .iter()
        .map(|w| census(&store.upper_dir(w).unwrap()).1)
        .sum();
    let started = Instant::now();
    for workspace in &workspaces {
        assert!(matches!(
            store.commit(workspace, "bench").unwrap(),
            Commit::Created(_)
        ));
    }
    let commit_time = started.elapsed();
    let started = Instant::now();
    for workspace in &workspaces {
        unmounted(&store, workspace);
    }
    let unmount_time = started.elapsed();

    println!("files per tree:       {}", files + 1);
    println!("5 full copies:        {full_bytes} bytes in {full_time:?}");
    println!(
        "5 overlays:           {} bytes ({layer_bytes} shared layer + {upper_bytes} upper) in {overlay_time:?}",
        layer_bytes + upper_bytes
    );
    println!("5 overlay commits:    {commit_time:?}");
    println!("5 overlay unmounts:   {unmount_time:?}");
    assert!(layer_bytes + upper_bytes < full_bytes / 4);
}

/// A `lower` record that exists but cannot be read is an error, never a
/// fresh start: a fresh start would write the branch head over it, and the
/// upperdir (a diff against the old lower) would then be served over the
/// wrong commit.
#[test]
fn an_unreadable_lower_record_is_never_taken_for_a_fresh_workspace() {
    let Some(scratch) = scratch("overlay-unreadable-lower") else {
        return;
    };
    let repo = seed(&scratch, SEED);
    let root = scratch.join("ws");
    let first = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    first.ensure_branch(&branch("agent"), None).unwrap();
    let workspace = first.mount(&branch("agent")).unwrap();
    let lower = first
        .upper_dir(&workspace)
        .unwrap()
        .parent()
        .unwrap()
        .join("lower");
    drop(first);
    let before = fs::read(&lower).unwrap();
    fs::set_permissions(&lower, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(&lower).is_ok() {
        fs::set_permissions(&lower, fs::Permissions::from_mode(0o644)).unwrap();
        eprintln!("skipped: mode 000 does not stop this user reading (root?)");
        return;
    }
    let second = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    let result = second.mount(&branch("agent"));
    let mode = fs::metadata(&lower).unwrap().permissions().mode();
    fs::set_permissions(&lower, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        matches!(&result, Err(WorkspaceError::Unavailable(_))),
        "{result:?}"
    );
    assert_eq!(mode & 0o777, 0, "the lower record was replaced");
    assert_eq!(fs::read(&lower).unwrap(), before);
}

/// #556 (owner decision 2026-11-23): an unrelated process of this uid that
/// is not dumpable, outside every workspace (as `spawn loop`, `serve` or
/// `integrate` run on a fleet host), never blocks a teardown or a recovery.
/// Unmount, mount over an existing upperdir after a dropped store, and
/// unmount again all succeed while it lives.
#[test]
fn an_unrelated_non_dumpable_host_process_never_blocks_teardown() {
    let Some(scratch) = scratch("overlay-unrelated-undumpable") else {
        return;
    };
    if !python3() {
        return;
    }
    let elsewhere = scratch.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    let bystander = undumpable_in(&elsewhere);
    let repo = seed(&scratch, SEED);
    let root = scratch.join("ws");
    let first = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    first.ensure_branch(&branch("agent"), None).unwrap();
    let workspace = first.mount(&branch("agent")).unwrap();
    sh(&first, &workspace, "echo pending > pending.txt");
    let upper = first.upper_dir(&workspace).unwrap();
    drop(first);

    let second = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    assert_eq!(remounted(&second, "agent"), workspace);
    assert_eq!(sh(&second, &workspace, "cat pending.txt"), "pending\n");
    unmounted(&second, &workspace);
    assert!(!upper.exists());
    // A fresh workspace too, start to end.
    let again = remounted(&second, "agent");
    unmounted(&second, &again);
    finish(bystander);
    assert_eq!(fs::read(elsewhere.join("pending.txt")).unwrap(), b"late\n");
}

/// The environment variable that tells
/// [`a_whole_lifecycle_runs_inside_a_nested_pid_namespace`] it already runs
/// inside the nested namespace; it holds the outer pid namespace.
const NESTED: &str = "SPAWN_OVERLAY_TEST_NESTED_PIDNS";

/// The whole lifecycle (mount, commit, a holder lost with its store, the
/// recovery mount over the old upperdir, a refused and then a clean
/// unmount) inside a pid namespace nested in the one the tests run in,
/// where `/proc` lists only that namespace's processes. The test runs
/// itself again under `unshare --pid` and requires the nested run to reach
/// its end.
#[test]
fn a_whole_lifecycle_runs_inside_a_nested_pid_namespace() {
    let Some(scratch) = scratch("overlay-nested-pidns") else {
        return;
    };
    let own = fs::read_link("/proc/self/ns/pid").unwrap();
    let Ok(outer) = std::env::var(NESTED) else {
        let output = std::process::Command::new("unshare")
            .args([
                "--user",
                "--map-current-user",
                "--pid",
                "--fork",
                "--mount-proc",
            ])
            .arg(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "a_whole_lifecycle_runs_inside_a_nested_pid_namespace",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(NESTED, &own)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stdout}\n{stderr}");
        assert!(
            stderr.contains("nested lifecycle done"),
            "the nested run did not reach its end: {stdout}\n{stderr}"
        );
        return;
    };
    assert_ne!(own, Path::new(&outer), "not in a nested pid namespace");
    let repo = seed(&scratch, SEED);
    let root = scratch.join("ws");
    let first = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    first.ensure_branch(&branch("agent"), None).unwrap();
    let workspace = first.mount(&branch("agent")).unwrap();
    sh(&first, &workspace, "echo one > one.txt");
    assert!(matches!(
        first.commit(&workspace, "one").unwrap(),
        Commit::Created(_)
    ));
    sh(&first, &workspace, "echo pending > pending.txt");
    let upper = first.upper_dir(&workspace).unwrap();
    drop(first);
    let second = OverlayWorkspace::new(&repo, &root, spawn_host::DEFAULT_MAX_FILE_BYTES);
    assert_eq!(remounted(&second, "agent"), workspace);
    assert_eq!(sh(&second, &workspace, "cat pending.txt"), "pending\n");
    let child = enter(&second, &workspace, "echo in; read line");
    refused_with(second.unmount(&workspace), IN_USE);
    release(child);
    unmounted(&second, &workspace);
    assert!(!upper.exists());
    eprintln!("nested lifecycle done");
}

/// Review of #556, finding 3: the lease is held by the launch's supervisor
/// (`flock`), not by a descriptor the program must keep. A client that
/// closes every descriptor it inherited still blocks the unmount while it
/// runs, and its workspace stays mounted.
#[test]
fn a_client_that_closes_its_descriptors_keeps_the_lease() {
    let Some(scratch) = scratch("overlay-closed-fds") else {
        return;
    };
    if !python3() {
        return;
    }
    let repo = seed(&scratch, SEED);
    let store = OverlayWorkspace::new(
        &repo,
        scratch.join("ws"),
        spawn_host::DEFAULT_MAX_FILE_BYTES,
    );
    store.ensure_branch(&branch("agent"), None).unwrap();
    let workspace = store.mount(&branch("agent")).unwrap();
    let script = "import os, sys\n\
                  os.closerange(3, 65536)\n\
                  print('in', flush=True)\n\
                  sys.stdin.readline()\n\
                  open('late.txt', 'w').write('late\\n')\n";
    let mut child = store
        .command(&workspace, "python3")
        .unwrap()
        .args(["-I", "-c", script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line.trim(), "in");
    let message = refused_with(store.unmount(&workspace), IN_USE);
    assert!(message.contains("lease"), "{message}");
    assert!(store.holder_pid(&workspace).is_some(), "still mounted");
    finish(child);
    assert_eq!(sh(&store, &workspace, "cat late.txt"), "late\n");
    unmounted(&store, &workspace);
}
