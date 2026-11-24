//! The copy-on-write workspace store and the CAS deduplicator, on the file
//! system the build directory is on (`CARGO_TARGET_TMPDIR`): btrfs or XFS
//! on Linux, APFS on macOS. Nothing here needs root, a VM or a mount; on a
//! file system without clones the store is checked in its copy fallback and
//! the extent checks say they were skipped.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use spawn_core::id::{BranchName, WorkspaceId};
use spawn_core::port::{Commit, WORKSPACE_STALE, WorkspaceError, WorkspaceStore};
use spawn_host::cas_dedup::{CAS_MARKER, sha256_file};
use spawn_host::cow::{
    UNSAFE_TREE, WORKSPACE_BUSY, WORKSPACE_DIRTY, WORKSPACE_HOLDS_TRANSCRIPTS, WORKSPACE_NOT_OWNED,
    WORKSPACE_UNREADABLE,
};
use spawn_host::{
    ArtifactRoot, CloneKind, CowWorkspace, DedupConfig, DedupError, DedupReport, Scratch,
    WorkspaceLease, clone_or_copy, cow_clone_supported, current_uid, dedup_dirs, live_users,
    prune_unused, reflink_file, test_git_command,
};

fn scratch(tag: &str) -> Scratch {
    Scratch::new_in(Path::new(env!("CARGO_TARGET_TMPDIR")), tag)
}

fn git(directory: &Path, arguments: &[&str]) -> String {
    let output = test_git_command()
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()
        .expect("git is installed");
    assert!(output.status.success(), "git {arguments:?}: {output:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Deterministic bytes no compression or dedup inside the file system can
/// shrink.
fn noise(seed: u64, length: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..length)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        })
        .collect()
}

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
}

/// A repository whose `main` holds a small tree: text, a 256 KiB source, an
/// executable, a symlink and `bulk` files.
fn seeded(base: &Path, bulk: usize) -> PathBuf {
    let checkout = base.join("seed");
    fs::create_dir_all(checkout.join("src")).expect("mkdir");
    fs::create_dir_all(checkout.join("bin")).expect("mkdir");
    fs::create_dir_all(checkout.join("bulk")).expect("mkdir");
    git(&checkout, &["init", "-q", "--initial-branch=main"]);
    fs::write(checkout.join("README.md"), "base\n").expect("write");
    fs::write(checkout.join("src/lib.rs"), noise(1, 256 * 1024)).expect("write");
    fs::write(checkout.join("bin/run.sh"), "#!/bin/sh\necho run\n").expect("write");
    set_mode(&checkout.join("bin/run.sh"), 0o755);
    fs::write(checkout.join("bin/tool.txt"), "tool\n").expect("write");
    std::os::unix::fs::symlink("README.md", checkout.join("link")).expect("symlink");
    for at in 0..bulk {
        fs::write(
            checkout.join(format!("bulk/f{at}.txt")),
            format!("file {at}\n"),
        )
        .expect("write");
    }
    git(&checkout, &["add", "-A"]);
    git(&checkout, &["commit", "-qm", "base"]);
    checkout.join(".git")
}

/// The workspace a test's artifact root `<w>/target` belongs to: `<w>`,
/// whose admission lock holds it offline.
fn admission_lock(root: &Path) -> PathBuf {
    root.parent().expect("a workspace").to_path_buf()
}

/// [`dedup_dirs`] over `roots`, each held offline by its own admission
/// lock for the pass.
fn dedup(config: &DedupConfig, roots: &[&Path]) -> Result<DedupReport, DedupError> {
    let offline: Vec<WorkspaceLease> = roots
        .iter()
        .map(|root| {
            WorkspaceLease::exclusive(&admission_lock(root))
                .expect("lock")
                .expect("no lease is held")
        })
        .collect();
    let roots: Vec<ArtifactRoot<'_>> = roots
        .iter()
        .zip(&offline)
        .map(|(root, admission)| ArtifactRoot::new(admission, *root).expect("artifact root"))
        .collect();
    dedup_dirs(config, &roots)
}

fn branch(name: &str) -> BranchName {
    BranchName::new(name).expect("branch")
}

fn ls_tree(repo: &Path, reference: &str) -> Vec<(String, String)> {
    git(repo, &["ls-tree", "-r", "--full-tree", reference])
        .lines()
        .map(|line| {
            let (head, path) = line.split_once('\t').expect("tab");
            (
                path.to_string(),
                head.split(' ').next().unwrap_or("").to_string(),
            )
        })
        .collect()
}

/// The refs a stale workspace of `branch` was kept on.
fn stale_refs(repo: &Path, branch: &str) -> Vec<String> {
    git(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname)",
            &format!("refs/spawn-cow/stale/{branch}/"),
        ],
    )
    .lines()
    .map(str::to_string)
    .collect()
}

fn show(repo: &Path, spec: &str) -> String {
    git(repo, &["show", spec])
}

/// `(logical, physical, length)` of each extent of a file, in blocks.
type Extents = Vec<(u64, u64, u64)>;

/// `filefrag -v` extents of a file as `(logical, physical, length)` blocks
/// and whether every extent is flagged shared; `None` without `filefrag`.
fn extents(path: &Path) -> Option<(Extents, bool)> {
    let output = Command::new("filefrag")
        .args(["-s", "-v"])
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut found = Vec::new();
    let mut all_shared = true;
    for line in text.lines() {
        let fields: Vec<&str> = line.split(':').map(str::trim).collect();
        if fields.len() < 4 || fields[0].parse::<u64>().is_err() {
            continue;
        }
        let start = |field: &str| field.split("..").next().and_then(|n| n.trim().parse().ok());
        let (Some(logical), Some(physical), Ok(length)) =
            (start(fields[1]), start(fields[2]), fields[3].parse::<u64>())
        else {
            continue;
        };
        all_shared &= fields.last().is_some_and(|flags| flags.contains("shared"));
        found.push((logical, physical, length));
    }
    (!found.is_empty()).then_some((found, all_shared))
}

#[test]
#[allow(clippy::too_many_lines)] // one scenario, step by step
fn workspaces_are_isolated_cow_clones_committed_by_manifest() {
    let base = scratch("cow-store");
    let repo = seeded(&base, 200);
    let store = CowWorkspace::new(&repo, base.join("work"));
    let cow = store.cow_supported();
    eprintln!("copy-on-write clones on this file system: {cow}");

    let mut mounted = Vec::new();
    for name in ["agent_a", "agent_b", "agent_c"] {
        store.ensure_branch(&branch(name), None).expect("ensure");
        let workspace = store.mount(&branch(name)).expect("mount");
        let stats = store.last_stats();
        assert_eq!(stats.reflinked + stats.copied, 204, "{stats:?}");
        if cow {
            assert_eq!(stats.copied, 0, "{stats:?}");
        }
        mounted.push(workspace);
    }
    // One baseline for the one commit all three are at.
    let head = git(&repo, &["rev-parse", "main"]);
    let baselines: Vec<_> = fs::read_dir(store.root().join(".cow/baselines"))
        .expect("baselines")
        .flatten()
        .collect();
    assert_eq!(baselines.len(), 1);
    let baseline = store.baseline_path(&head);
    assert!(
        fs::metadata(baseline.join("README.md"))
            .expect("baseline file")
            .permissions()
            .readonly(),
        "Layer 0 is read-only"
    );

    let path = |workspace: &WorkspaceId| PathBuf::from(workspace.path());
    let (a, b) = (path(&mounted[0]), path(&mounted[1]));
    assert_eq!(a, store.root().join("agent_a"));
    // Plain directories with the committed tree, modes and links.
    assert_eq!(
        fs::read_to_string(a.join("README.md")).expect("read"),
        "base\n"
    );
    assert_eq!(
        fs::read(a.join("src/lib.rs")).expect("read"),
        noise(1, 256 * 1024)
    );
    assert_eq!(
        fs::metadata(a.join("bin/run.sh")).expect("stat").mode() & 0o777,
        0o755
    );
    assert_eq!(
        fs::metadata(a.join("README.md")).expect("stat").mode() & 0o777,
        0o644
    );
    assert_eq!(
        fs::read_link(a.join("link")).expect("link"),
        Path::new("README.md")
    );

    if cow && cfg!(target_os = "linux") {
        match (
            extents(&baseline.join("src/lib.rs")),
            extents(&a.join("src/lib.rs")),
        ) {
            (Some((layer0, _)), Some((clone, shared))) => {
                assert_eq!(layer0, clone, "the clone shares Layer 0's extents");
                assert!(shared, "extents are flagged shared");
            }
            _ => eprintln!("filefrag unavailable: extent check skipped"),
        }
    }

    // A checkpoint after edits, additions, deletions and exec-bit changes
    // reads only what changed.
    fs::write(a.join("README.md"), "edited by a\n").expect("write");
    fs::create_dir_all(a.join("docs")).expect("mkdir");
    fs::write(a.join("docs/new.md"), "new\n").expect("write");
    fs::remove_file(a.join("bulk/f0.txt")).expect("remove");
    set_mode(&a.join("bin/tool.txt"), 0o755);
    set_mode(&a.join("bin/run.sh"), 0o644);
    fs::create_dir_all(a.join("target/debug")).expect("mkdir");
    fs::write(a.join("target/debug/app"), "build output\n").expect("write");

    // Isolation: neither the neighbour nor Layer 0 saw any of it.
    assert_eq!(
        fs::read_to_string(b.join("README.md")).expect("read"),
        "base\n"
    );
    assert!(b.join("bulk/f0.txt").exists());
    assert_eq!(
        fs::read_to_string(baseline.join("README.md")).expect("read"),
        "base\n"
    );

    let commit = store.commit(&mounted[0], "a's work").expect("commit");
    let Commit::Created(created) = commit else {
        panic!("expected a commit, got {commit:?}");
    };
    let stats = store.last_stats();
    assert_eq!(
        stats.hashed, 4,
        "only the four changed files are read: {stats:?}"
    );
    assert_eq!(stats.reused, 201, "{stats:?}");
    assert_eq!(git(&repo, &["rev-parse", "agent_a"]), created.as_str());
    assert_eq!(git(&repo, &["rev-parse", "agent_a^"]), head);
    let tree = ls_tree(&repo, "agent_a");
    let mode_of = |name: &str| {
        tree.iter()
            .find(|(path, _)| path == name)
            .map(|(_, mode)| mode.clone())
    };
    assert_eq!(show(&repo, "agent_a:README.md"), "edited by a");
    assert_eq!(show(&repo, "agent_a:docs/new.md"), "new");
    assert_eq!(mode_of("bulk/f0.txt"), None);
    assert_eq!(mode_of("bin/tool.txt").as_deref(), Some("100755"));
    assert_eq!(mode_of("bin/run.sh").as_deref(), Some("100644"));
    assert_eq!(mode_of("link").as_deref(), Some("120000"));
    assert_eq!(
        mode_of("target/debug/app"),
        None,
        "build output is never committed"
    );
    assert_eq!(tree.len(), 205);

    // Nothing new: unchanged, with every file taken from the manifest.
    assert_eq!(
        store.commit(&mounted[0], "again").expect("commit"),
        Commit::Unchanged
    );
    assert_eq!(
        store.commit(&mounted[1], "b idle").expect("commit"),
        Commit::Unchanged
    );
    assert_eq!(git(&repo, &["rev-parse", "agent_b"]), head);

    // Same size, mtime put back: the ctime still gives the edit away.
    let readme = b.join("README.md");
    let before = fs::metadata(&readme).expect("stat");
    fs::write(&readme, "BASE\n").expect("write");
    fs::OpenOptions::new()
        .write(true)
        .open(&readme)
        .expect("open")
        .set_times(fs::FileTimes::new().set_modified(before.modified().expect("mtime")))
        .expect("set times");
    assert!(matches!(
        store.commit(&mounted[1], "b").expect("commit"),
        Commit::Created(_)
    ));
    assert_eq!(show(&repo, "agent_b:README.md"), "BASE");

    // A clean workspace at its head mounts again as it is.
    let inode = fs::metadata(a.join("src/lib.rs")).expect("stat").ino();
    assert_eq!(
        store.mount(&branch("agent_a")).expect("remount"),
        mounted[0]
    );
    assert_eq!(
        fs::metadata(a.join("src/lib.rs")).expect("stat").ino(),
        inode
    );
    // One with uncommitted work is refused, and kept.
    fs::write(a.join("README.md"), "uncommitted\n").expect("write");
    match store.mount(&branch("agent_a")) {
        Err(WorkspaceError::Refused { code, .. }) => assert_eq!(code, WORKSPACE_DIRTY),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert_eq!(
        fs::read_to_string(a.join("README.md")).expect("read"),
        "uncommitted\n"
    );

    // A branch moved under the workspace: refused, the state kept aside.
    let c = path(&mounted[2]);
    git(
        &repo,
        &["update-ref", "refs/heads/agent_c", created.as_str()],
    );
    fs::write(c.join("README.md"), "c's work\n").expect("write");
    match store.commit(&mounted[2], "c") {
        Err(WorkspaceError::Refused { code, .. }) => assert_eq!(code, WORKSPACE_STALE),
        other => panic!("expected stale, got {other:?}"),
    }
    assert_eq!(git(&repo, &["rev-parse", "agent_c"]), created.as_str());
    let kept = stale_refs(&repo, "agent_c");
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(show(&repo, &format!("{}:README.md", kept[0])), "c's work");
    // A second stale state gets a ref of its own; the first stays (F9b).
    fs::write(c.join("README.md"), "c's second work\n").expect("write");
    assert!(store.commit(&mounted[2], "c again").is_err());
    let kept = stale_refs(&repo, "agent_c");
    assert_eq!(kept.len(), 2, "{kept:?}");
    let contents: Vec<String> = kept
        .iter()
        .map(|reference| show(&repo, &format!("{reference}:README.md")))
        .collect();
    assert!(contents.contains(&"c's work".to_string()), "{contents:?}");
    assert!(
        contents.contains(&"c's second work".to_string()),
        "{contents:?}"
    );

    // A branch deleted under the workspace is made again from it.
    git(&repo, &["update-ref", "-d", "refs/heads/agent_b"]);
    fs::write(b.join("docs.txt"), "more\n").expect("write");
    match store.commit(&mounted[1], "b again").expect("commit") {
        Commit::Recreated { commit, parent } => {
            assert_eq!(git(&repo, &["rev-parse", "agent_b"]), commit.as_str());
            assert!(parent.is_some());
        }
        other => panic!("expected recreated, got {other:?}"),
    }

    // A foreign or unsafe workspace id is refused.
    let foreign =
        WorkspaceId::new(format!("agent_a:{}", base.join("elsewhere").display())).expect("id");
    assert!(store.commit(&foreign, "x").is_err());
    let escape = WorkspaceId::new(format!(
        ".transcripts:{}",
        store.root().join(".transcripts").display()
    ))
    .expect("id");
    assert!(store.unmount(&escape).is_err());
}

#[test]
fn unmount_keeps_transcripts_and_never_follows_links() {
    let base = scratch("cow-unmount");
    let repo = seeded(&base, 2);
    let store = CowWorkspace::new(&repo, base.join("work"));
    store
        .ensure_branch(&branch("agent_a"), None)
        .expect("ensure");
    let workspace = store.mount(&branch("agent_a")).expect("mount");
    let directory = PathBuf::from(workspace.path());

    // The archive beside the workspaces, and a directory outside the store
    // that a link in the workspace points at.
    let archive = store
        .root()
        .join(spawn_host::TRANSCRIPT_DIRECTORY)
        .join("agent_a");
    fs::create_dir_all(&archive).expect("mkdir");
    fs::write(archive.join("s1.jsonl"), "{}\n").expect("write");
    let outside = base.join("outside");
    fs::create_dir_all(&outside).expect("mkdir");
    fs::write(outside.join("keep.txt"), "keep\n").expect("write");
    std::os::unix::fs::symlink(&outside, directory.join("outside-link")).expect("symlink");
    // A transcript directory inside the workspace, and a stray transcript.
    fs::create_dir_all(directory.join(".transcripts")).expect("mkdir");
    fs::write(directory.join(".transcripts/t1.jsonl"), "{\"t\":1}\n").expect("write");
    fs::write(directory.join("notes.jsonl"), "{}\n").expect("write");

    match store.unmount(&workspace) {
        Err(WorkspaceError::Refused { code, .. }) => assert_eq!(code, WORKSPACE_HOLDS_TRANSCRIPTS),
        other => panic!("expected the 0 *.jsonl guard to refuse, got {other:?}"),
    }
    assert!(
        directory.join("notes.jsonl").exists(),
        "nothing was deleted"
    );
    assert!(directory.join(".transcripts/t1.jsonl").exists());

    // Moved out of the way (not deleted), the unmount goes through.
    fs::rename(directory.join("notes.jsonl"), base.join("notes.jsonl")).expect("move");
    store.unmount(&workspace).expect("unmount");
    assert!(!directory.exists());
    let kept = store.kept_transcripts("agent_a");
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(
        fs::read_to_string(kept[0].join("t1.jsonl")).expect("kept transcript"),
        "{\"t\":1}\n"
    );
    assert!(
        archive.join("s1.jsonl").exists(),
        "the archive is never touched"
    );
    assert!(outside.join("keep.txt").exists(), "links are not followed");
    assert!(base.join("notes.jsonl").exists());
    // Unmounting again is a no-op; the branch keeps what was committed.
    store.unmount(&workspace).expect("unmount again");
    assert_eq!(show(&repo, "agent_a:README.md"), "base");
    assert_eq!(store.prune_baselines().expect("prune"), 1);
}

#[test]
fn reflink_refuses_an_existing_target() {
    let base = scratch("cow-reflink");
    let source = base.join("a");
    let target = base.join("b");
    fs::write(&source, noise(2, 70_000)).expect("write");
    fs::write(&target, "taken").expect("write");
    assert!(reflink_file(&source, &target).is_err());
    assert_eq!(fs::read_to_string(&target).expect("read"), "taken");
    if cow_clone_supported(&base) {
        fs::remove_file(&target).expect("remove");
        reflink_file(&source, &target).expect("clone");
        assert_eq!(fs::read(&target).expect("read"), noise(2, 70_000));
    }
}

/// Three workers' `target/` directories as separate builds leave them:
/// the same large artifacts written separately, plus what must be left
/// alone.
fn build_outputs(base: &Path) -> Vec<PathBuf> {
    (0..3)
        .map(|worker| {
            let target = base.join(format!("w{worker}/target"));
            let debug = target.join("debug");
            fs::create_dir_all(debug.join("deps")).expect("mkdir");
            fs::create_dir_all(debug.join(".transcripts")).expect("mkdir");
            fs::write(debug.join("app"), noise(10, 1 << 20)).expect("write");
            set_mode(&debug.join("app"), 0o755);
            fs::write(debug.join("deps/libfoo.rlib"), noise(11, 256 * 1024)).expect("write");
            fs::write(debug.join("deps/own.rlib"), noise(100 + worker, 128 * 1024)).expect("write");
            fs::write(debug.join("small.d"), noise(12, 1024)).expect("write");
            fs::write(debug.join("run.jsonl"), noise(13, 128 * 1024)).expect("write");
            fs::write(debug.join(".transcripts/t"), noise(14, 128 * 1024)).expect("write");
            std::os::unix::fs::symlink("app", debug.join("app-link")).expect("symlink");
            target
        })
        .collect()
}

#[test]
fn cas_dedup_converges_three_workers_on_one_extent() {
    let base = scratch("cow-dedup");
    if !cow_clone_supported(&base) {
        eprintln!("no copy-on-write clones on this file system: dedup test skipped");
        return;
    }
    let targets = build_outputs(&base);
    let apps: Vec<PathBuf> = targets.iter().map(|t| t.join("debug/app")).collect();
    let mtimes: Vec<_> = apps
        .iter()
        .map(|app| fs::metadata(app).expect("stat").modified().expect("mtime"))
        .collect();
    let linux_extents = cfg!(target_os = "linux") && extents(&apps[0]).is_some();
    if linux_extents {
        let first = extents(&apps[0]).expect("extents").0;
        let second = extents(&apps[1]).expect("extents").0;
        assert_ne!(first, second, "separate builds start on separate extents");
    }

    let mut config = DedupConfig::new(base.join("cas"));
    config.min_age = Duration::ZERO;
    let reports: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = targets
            .iter()
            .map(|target| {
                let config = &config;
                scope.spawn(move || dedup(config, &[target.as_path()]).expect("dedup"))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("thread"))
            .collect()
    });
    for report in &reports {
        assert!(report.cow_supported);
        assert!(report.errors.is_empty(), "{report:?}");
        assert_eq!(report.skipped_symlinks, 1, "{report:?}");
        assert_eq!(report.skipped_small, 1, "{report:?}");
        assert_eq!(report.skipped_transcripts, 2, "{report:?}");
        assert_eq!(report.eligible, 3, "{report:?}");
    }
    let sum = |field: fn(&spawn_host::DedupReport) -> u64| reports.iter().map(field).sum::<u64>();
    assert_eq!(sum(|r| r.blobs_created as u64), 2 + 3, "{reports:?}");
    assert_eq!(sum(|r| r.deduplicated as u64), 4, "{reports:?}");
    assert_eq!(sum(|r| r.reclaimed_bytes), 2 * ((1 << 20) + 256 * 1024));

    // Paths, contents, modes and mtimes are as the builds left them.
    for (app, mtime) in apps.iter().zip(&mtimes) {
        assert_eq!(fs::read(app).expect("read"), noise(10, 1 << 20));
        let meta = fs::metadata(app).expect("stat");
        assert_eq!(meta.mode() & 0o777, 0o755);
        assert_eq!(meta.modified().expect("mtime"), *mtime);
        assert!(
            fs::symlink_metadata(app.with_file_name("app-link"))
                .expect("link")
                .is_symlink()
        );
        assert!(app.with_file_name("run.jsonl").exists());
    }
    if linux_extents {
        let blob = fs::read_dir(base.join("cas"))
            .expect("cas")
            .flatten()
            .filter(|shard| shard.file_name().len() == 2)
            .flat_map(|shard| fs::read_dir(shard.path()).expect("shard").flatten())
            .map(|blob| blob.path())
            .find(|blob| fs::read(blob).is_ok_and(|bytes| bytes == noise(10, 1 << 20)))
            .expect("the app's blob");
        let (canonical, _) = extents(&blob).expect("extents");
        for app in &apps {
            let (clone, shared) = extents(app).expect("extents");
            assert_eq!(clone, canonical, "{} shares the CAS extent", app.display());
            assert!(shared);
        }
        // The jsonl transcripts were left on their own extents.
        let jsonl: Vec<_> = targets
            .iter()
            .map(|t| extents(&t.join("debug/run.jsonl")).expect("extents").0)
            .collect();
        assert_ne!(jsonl[0], jsonl[1]);
    } else {
        eprintln!("filefrag unavailable: extent check skipped");
    }

    // A second pass knows every file and reads none.
    let again = dedup(&config, &[targets[0].as_path()]).expect("dedup");
    assert_eq!(again.already_shared, 3, "{again:?}");
    assert_eq!(again.deduplicated + again.blobs_created, 0, "{again:?}");

    // Writing one clone leaves the others as they were.
    fs::write(&apps[1], b"rebuilt").expect("write");
    assert_eq!(fs::read(&apps[0]).expect("read"), noise(10, 1 << 20));

    // Prune: the rewritten file's entry goes, every blob still used stays.
    let pruned = prune_unused(&base.join("cas"), Duration::ZERO).expect("prune");
    assert_eq!(pruned.dropped_entries, 1, "{pruned:?}");
    assert_eq!(pruned.blobs_removed, 0, "{pruned:?}");
    // With two builds gone, the app's blob is still w2's; w0's own is not.
    fs::remove_dir_all(base.join("w0")).expect("remove");
    let pruned = prune_unused(&base.join("cas"), Duration::ZERO).expect("prune");
    assert_eq!(pruned.blobs_removed, 1, "{pruned:?}");
    assert_eq!(fs::read(&apps[2]).expect("read"), noise(10, 1 << 20));
}

#[test]
fn cas_dedup_leaves_young_files() {
    let base = scratch("cow-dedup-young");
    if !cow_clone_supported(&base) {
        eprintln!("no copy-on-write clones on this file system: test skipped");
        return;
    }
    let dirs: Vec<PathBuf> = (0..2)
        .map(|at| base.join(format!("w{at}/target")))
        .collect();
    for dir in &dirs {
        fs::create_dir_all(dir).expect("mkdir");
        fs::write(dir.join("big.bin"), noise(5, 200_000)).expect("write");
    }
    // Default config: two seconds of quiet are required.
    let report = dedup(&DedupConfig::new(base.join("cas")), &[&dirs[0], &dirs[1]]).expect("dedup");
    assert_eq!(report.skipped_busy, 2, "{report:?}");
    assert_eq!(report.deduplicated + report.blobs_created, 0, "{report:?}");
}

/// Review #517 F2 (probes p10, p10b): a writer holding the file open, with
/// or without a quiet period, would lose its writes to a rename; the whole
/// directory is refused instead, and the late write lands at the path.
#[test]
fn cas_dedup_refuses_a_live_workspace_and_loses_no_write() {
    let base = scratch("cow-dedup-live");
    if !cow_clone_supported(&base) {
        eprintln!("no copy-on-write clones on this file system: test skipped");
        return;
    }
    let (a, b) = (base.join("w0/target"), base.join("w1/target"));
    for dir in [&a, &b] {
        fs::create_dir_all(dir).expect("mkdir");
        fs::write(dir.join("log.bin"), vec![7_u8; 200_000]).expect("write");
    }
    let mut config = DedupConfig::new(base.join("cas"));
    config.min_age = Duration::ZERO;
    let mut writer = fs::OpenOptions::new()
        .append(true)
        .open(b.join("log.bin"))
        .expect("open");
    for config in [config.clone(), DedupConfig::new(base.join("cas"))] {
        match dedup(&config, &[&a, &b]) {
            Err(DedupError::Live { directory, pids }) => {
                assert_eq!(directory, b);
                assert!(pids.contains(&std::process::id()), "{pids:?}");
            }
            other => panic!("a live workspace must be refused, got {other:?}"),
        }
    }
    writer.write_all(b"LATE-WRITE").expect("write");
    drop(writer);
    assert!(
        fs::read(b.join("log.bin"))
            .expect("read")
            .ends_with(b"LATE-WRITE")
    );
    assert!(
        !base.join("cas").exists(),
        "nothing was written before the refusal"
    );

    // A process whose working directory is inside counts as well.
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("cd \"$1\" && exec sleep 30")
        .arg("sh")
        .arg(&a)
        .spawn()
        .expect("sh");
    wait_until_live(&a, child.id());
    assert!(matches!(
        dedup(&config, &[&a]),
        Err(DedupError::Live { .. })
    ));
    let _ = child.kill();
    let _ = child.wait();
    // A worker's lease keeps the root online: no exclusive admission, and
    // a lease is never taken for one.
    let lease = WorkspaceLease::lease(&admission_lock(&a)).expect("lease");
    assert!(
        WorkspaceLease::exclusive(&admission_lock(&a))
            .expect("lock")
            .is_none()
    );
    assert!(matches!(
        ArtifactRoot::new(&lease, &a),
        Err(DedupError::NotOffline(_))
    ));
    drop(lease);
    // Only a build-artifact directory is ever a root.
    let offline = WorkspaceLease::exclusive(&base.join("w0"))
        .expect("lock")
        .expect("free");
    assert!(matches!(
        ArtifactRoot::new(&offline, base.join("w0")),
        Err(DedupError::NotArtifact(_))
    ));
    drop(offline);
    // Quiescent now: the pass goes through.
    let report = dedup(&config, &[&a, &b]).expect("dedup");
    assert_eq!(report.blobs_created, 2, "{report:?}");
}

/// Wait until `pid` is seen working in `directory`.
fn wait_until_live(directory: &Path, pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !live_users(directory).expect("process table").contains(&pid) {
        assert!(Instant::now() < deadline, "{pid} never showed up");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Review #517 F3 (probe p11): a blob of the right size but other bytes is
/// never cloned over a file.
#[test]
fn cas_dedup_refuses_a_tampered_blob() {
    let base = scratch("cow-dedup-tampered");
    if !cow_clone_supported(&base) {
        eprintln!("no copy-on-write clones on this file system: test skipped");
        return;
    }
    let work = base.join("w0/target");
    fs::create_dir_all(&work).expect("mkdir");
    let content = vec![1_u8; 100_000];
    fs::write(work.join("f.bin"), &content).expect("write");
    let hash = sha256_file(&work.join("f.bin")).expect("hash");
    let mut config = DedupConfig::new(base.join("cas"));
    config.min_age = Duration::ZERO;

    // Planted before any pass: the root is not a CAS, nothing is used.
    let shard = base.join("cas").join(&hash[..2]);
    fs::create_dir_all(&shard).expect("mkdir");
    fs::write(shard.join(&hash), vec![2_u8; 100_000]).expect("plant");
    assert!(matches!(
        dedup(&config, &[&work]),
        Err(DedupError::NotCas(_))
    ));
    assert_eq!(fs::read(work.join("f.bin")).expect("read"), content);

    // Planted in a real CAS: refused by its content.
    fs::write(base.join("cas").join(CAS_MARKER), "spawn\n").expect("marker");
    let report = dedup(&config, &[&work]).expect("dedup");
    assert_eq!(report.deduplicated, 0, "{report:?}");
    assert_eq!(report.errors.len(), 1, "{report:?}");
    assert!(
        report.errors[0].contains("does not hold the content"),
        "{report:?}"
    );
    assert_eq!(fs::read(work.join("f.bin")).expect("read"), content);
    let leftovers = fs::read_dir(&work).expect("dir").count();
    assert_eq!(leftovers, 1, "no staged clone is left behind");

    // A symlink planted as the blob is never followed.
    fs::remove_file(shard.join(&hash)).expect("remove");
    let outside = base.join("outside.bin");
    fs::write(&outside, vec![3_u8; 100_000]).expect("write");
    std::os::unix::fs::symlink(&outside, shard.join(&hash)).expect("symlink");
    let report = dedup(&config, &[&work]).expect("dedup");
    assert_eq!(report.deduplicated, 0, "{report:?}");
    assert_eq!(report.errors.len(), 1, "{report:?}");
    assert_eq!(fs::read(work.join("f.bin")).expect("read"), content);
}

/// Review #517 F4 (probe p12): a root that is not a CAS is never pruned, and
/// inside one only the names a pass makes are removed.
#[test]
fn prune_refuses_a_foreign_root_and_keeps_foreign_names() {
    let base = scratch("cow-prune");
    let root = base.join("project");
    fs::create_dir_all(root.join("ab")).expect("mkdir");
    fs::create_dir_all(root.join("tmp")).expect("mkdir");
    fs::write(root.join("ab/session.jsonl"), "{}\n").expect("write");
    fs::write(root.join("tmp/notes.txt"), "keep\n").expect("write");
    let refused = prune_unused(&root, Duration::ZERO).expect_err("no marker");
    assert_eq!(refused.kind(), std::io::ErrorKind::InvalidInput);
    assert!(root.join("ab/session.jsonl").exists());
    assert!(root.join("tmp/notes.txt").exists());

    // Marked: still only `<hh>/<hash>` and `tmp/<hash>.<pid>-<n>` go.
    fs::write(root.join(CAS_MARKER), "spawn\n").expect("marker");
    let hash = format!("ab{}", "0".repeat(62));
    fs::write(root.join("ab").join(&hash), "blob").expect("write");
    fs::write(root.join("ab").join(format!("{hash}.jsonl")), "{}\n").expect("write");
    fs::write(root.join("cd").with_extension("jsonl"), "{}\n").expect("write");
    fs::create_dir_all(root.join("cd")).expect("mkdir");
    fs::write(root.join("cd").join(&hash), "wrong shard").expect("write");
    fs::write(root.join("tmp").join(format!("{hash}.12-3")), "tmp").expect("write");
    let report = prune_unused(&root, Duration::ZERO).expect("prune");
    assert_eq!(report.blobs_removed, 1, "{report:?}");
    assert_eq!(report.temporaries_removed, 1, "{report:?}");
    assert!(!root.join("ab").join(&hash).exists());
    for kept in [
        root.join("ab/session.jsonl"),
        root.join("ab").join(format!("{hash}.jsonl")),
        root.join("cd.jsonl"),
        root.join("cd").join(&hash),
        root.join("tmp/notes.txt"),
    ] {
        assert!(kept.exists(), "{} was deleted", kept.display());
    }
}

/// A tree object made by `git mktree` from `listing`.
fn mktree(repo: &Path, listing: &str) -> String {
    let mut child = test_git_command()
        .arg("-C")
        .arg(repo)
        .args(["mktree", "--missing"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("git");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(listing.as_bytes())
        .expect("feed");
    let output = child.wait_with_output().expect("mktree");
    assert!(output.status.success(), "mktree {listing:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A blob of `content`.
fn blob(repo: &Path, content: &str) -> String {
    let holder = repo.join("blob-input");
    fs::write(&holder, content).expect("write");
    let id = git(
        repo,
        &["hash-object", "-w", holder.to_str().expect("utf-8 path")],
    );
    fs::remove_file(&holder).expect("remove");
    id
}

/// Point `branch` at a commit of `tree`.
fn branch_at(repo: &Path, branch: &str, tree: &str) {
    let commit = git(repo, &["commit-tree", tree, "-m", branch]);
    git(
        repo,
        &["update-ref", &format!("refs/heads/{branch}"), &commit],
    );
}

/// Review #517 F1 (probe p5): trees that would write outside the store
/// (a symlink with an entry below it, `..`, `.git` in any spelling) are
/// refused before a byte is written, and nothing appears outside.
#[test]
fn a_tree_that_escapes_is_refused_and_writes_nothing_outside() {
    let base = scratch("cow-escape");
    let repo = seeded(&base, 1);
    let outside = base.join("outside");
    fs::create_dir_all(&outside).expect("mkdir");
    let pwned = blob(&repo, "pwned\n");
    let link = blob(&repo, outside.to_str().expect("utf-8 path"));
    let sub = mktree(&repo, &format!("100644 blob {pwned}\tpwn\n"));
    branch_at(
        &repo,
        "evil_dup",
        &mktree(
            &repo,
            &format!("120000 blob {link}\tevil\n040000 tree {sub}\tevil\n"),
        ),
    );
    let dotdot = mktree(&repo, &format!("040000 tree {sub}\t..\n"));
    branch_at(
        &repo,
        "evil_dotdot",
        &mktree(&repo, &format!("040000 tree {dotdot}\t..\n")),
    );
    let hook = mktree(&repo, &format!("100755 blob {pwned}\tpost-checkout\n"));
    let hooks = mktree(&repo, &format!("040000 tree {hook}\thooks\n"));
    branch_at(
        &repo,
        "evil_dotgit",
        &mktree(&repo, &format!("040000 tree {hooks}\t.GIT\n")),
    );
    let store = CowWorkspace::new(&repo, base.join("work"));
    for name in ["evil_dup", "evil_dotdot", "evil_dotgit"] {
        match store.mount(&branch(name)) {
            Err(WorkspaceError::Refused { code, .. }) => assert_eq!(code, UNSAFE_TREE, "{name}"),
            other => panic!("{name}: expected a refusal, got {other:?}"),
        }
        assert!(!store.root().join(name).exists(), "{name}");
    }
    assert!(!outside.join("pwn").exists());
    for escaped in [
        store.root().join(".cow/baselines/pwn"),
        store.root().join(".cow/pwn"),
        store.root().join(".cow/stage/pwn"),
        store.root().join("pwn"),
        base.join("pwn"),
    ] {
        assert!(!escaped.exists(), "{}", escaped.display());
    }
    let stage = fs::read_dir(store.root().join(".cow/stage")).map_or(0, Iterator::count);
    assert_eq!(stage, 0, "no staging is left behind");
}

/// Review #517 F5 (probe p1): a directory the unmount cannot read is never
/// taken for "no transcript"; nothing at all is deleted.
#[test]
fn unmount_refuses_an_unreadable_workspace_and_deletes_nothing() {
    if current_uid() == 0 {
        eprintln!("root reads every directory: test skipped");
        return;
    }
    let base = scratch("cow-unreadable");
    let repo = seeded(&base, 1);
    let store = CowWorkspace::new(&repo, base.join("work"));
    store
        .ensure_branch(&branch("agent_a"), None)
        .expect("ensure");
    let workspace = store.mount(&branch("agent_a")).expect("mount");
    let directory = PathBuf::from(workspace.path());
    let hidden = directory.join("zz_hidden");
    fs::create_dir_all(&hidden).expect("mkdir");
    fs::write(hidden.join("t.jsonl"), "{}\n").expect("write");
    set_mode(&hidden, 0o000);
    let result = store.unmount(&workspace);
    set_mode(&hidden, 0o755);
    match result {
        Err(WorkspaceError::Refused { code, .. }) => assert_eq!(code, WORKSPACE_UNREADABLE),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(hidden.join("t.jsonl").exists());
    assert!(directory.join("README.md").exists());
    assert!(directory.join("src/lib.rs").exists());
}

/// Review #517 F6 (probe p4): a submodule gitlink stays on the branch
/// through a checkpoint, and the untouched workspace mounts again.
#[test]
fn submodule_gitlinks_survive_commit_and_remount() {
    let base = scratch("cow-gitlink");
    let repo = seeded(&base, 1);
    let main = git(&repo, &["rev-parse", "main"]);
    let listing = git(&repo, &["ls-tree", "main"]);
    let vendor = mktree(&repo, &format!("160000 commit {main}\tsub\n"));
    let top = mktree(&repo, &format!("{listing}\n040000 tree {vendor}\tvendor\n"));
    let commit = git(&repo, &["commit-tree", &top, "-p", "main", "-m", "sub"]);
    git(&repo, &["update-ref", "refs/heads/main", &commit]);
    let store = CowWorkspace::new(&repo, base.join("work"));
    store
        .ensure_branch(&branch("agent_a"), None)
        .expect("ensure");
    let workspace = store.mount(&branch("agent_a")).expect("mount");
    let directory = PathBuf::from(workspace.path());
    assert!(directory.join("vendor/sub").is_dir());
    assert_eq!(
        store.commit(&workspace, "noop").expect("commit"),
        Commit::Unchanged
    );
    assert_eq!(store.mount(&branch("agent_a")).expect("remount"), workspace);
    fs::write(directory.join("README.md"), "edited\n").expect("write");
    assert!(matches!(
        store.commit(&workspace, "edit"),
        Ok(Commit::Created(_))
    ));
    let tree = ls_tree(&repo, "agent_a");
    assert!(
        tree.iter()
            .any(|(path, mode)| path == "vendor/sub" && mode == "160000"),
        "{tree:?}"
    );
    // A file inside the submodule's directory is not the superproject's.
    fs::write(directory.join("vendor/sub/inner.txt"), "x\n").expect("write");
    assert_eq!(
        store.commit(&workspace, "inner").expect("commit"),
        Commit::Unchanged
    );
}

/// Review #517 F7 (probe p7): two stores mounting one branch at once take
/// turns on the branch's flock; both succeed and no staging leaks.
#[test]
fn two_stores_mounting_one_branch_at_once_both_succeed() {
    let base = scratch("cow-race");
    let repo = seeded(&base, 20);
    let first = CowWorkspace::new(&repo, base.join("work"));
    let second = CowWorkspace::new(&repo, base.join("work"));
    first
        .ensure_branch(&branch("agent_a"), None)
        .expect("ensure");
    let workspace = WorkspaceId::new(format!(
        "agent_a:{}",
        first.root().join("agent_a").display()
    ))
    .expect("id");
    for round in 0..10 {
        first.unmount(&workspace).expect("unmount");
        let (one, two) = std::thread::scope(|scope| {
            let one = scope.spawn(|| first.mount(&branch("agent_a")));
            let two = scope.spawn(|| second.mount(&branch("agent_a")));
            (one.join().expect("thread"), two.join().expect("thread"))
        });
        assert_eq!(one.expect("first mount"), workspace, "round {round}");
        assert_eq!(two.expect("second mount"), workspace, "round {round}");
    }
    let leaked = fs::read_dir(first.root().join(".cow/stage")).map_or(0, Iterator::count);
    assert_eq!(leaked, 0);
}

/// Review #517 F8 (probe p9): a workspace a process still works in is not
/// unmounted.
#[test]
fn unmount_refuses_while_a_process_works_inside() {
    let base = scratch("cow-busy");
    let repo = seeded(&base, 1);
    let store = CowWorkspace::new(&repo, base.join("work"));
    store
        .ensure_branch(&branch("agent_a"), None)
        .expect("ensure");
    let workspace = store.mount(&branch("agent_a")).expect("mount");
    let directory = PathBuf::from(workspace.path());
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("cd \"$1\" && exec sleep 30")
        .arg("sh")
        .arg(&directory)
        .spawn()
        .expect("sh");
    wait_until_live(&directory, child.id());
    let result = store.unmount(&workspace);
    let _ = child.kill();
    let _ = child.wait();
    match result {
        Err(WorkspaceError::Refused { code, .. }) => assert_eq!(code, WORKSPACE_BUSY),
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(directory.join("README.md").exists());
    // An open file counts as well.
    let held = fs::File::open(directory.join("README.md")).expect("open");
    assert!(matches!(
        store.unmount(&workspace),
        Err(WorkspaceError::Refused { code, .. }) if code == WORKSPACE_BUSY
    ));
    drop(held);
    store.unmount(&workspace).expect("unmount");
    assert!(!directory.exists());
}

/// A descriptor opened by a non-leader thread still keeps its workspace
/// busy; sharing the process-level maps must not skip per-thread checks.
#[test]
fn unmount_refuses_while_a_worker_thread_holds_a_file_inside() {
    let base = scratch("cow-busy-thread");
    let repo = seeded(&base, 1);
    let store = CowWorkspace::new(&repo, base.join("work"));
    store
        .ensure_branch(&branch("agent_a"), None)
        .expect("ensure");
    let workspace = store.mount(&branch("agent_a")).expect("mount");
    let directory = PathBuf::from(workspace.path());
    let (opened, opened_rx) = std::sync::mpsc::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let worker_directory = directory.clone();
    let result = std::thread::scope(|scope| {
        scope.spawn(move || {
            let held = fs::File::open(worker_directory.join("README.md")).expect("open in worker");
            opened.send(()).expect("opened");
            release_rx.recv().expect("release");
            drop(held);
        });
        opened_rx.recv().expect("worker opened the file");
        let result = store.unmount(&workspace);
        release.send(()).expect("release worker");
        result
    });
    refused_with(result, WORKSPACE_BUSY);
    assert!(directory.join("README.md").exists());
    store.unmount(&workspace).expect("unmount after worker");
    assert!(!directory.exists());
}

/// Review #517 F9a (probe p8): the transcripts kept for `a` are only
/// `a`'s, never `a-b`'s.
#[test]
fn kept_transcripts_tell_branches_apart() {
    let base = scratch("cow-kept");
    let repo = seeded(&base, 1);
    let store = CowWorkspace::new(&repo, base.join("work"));
    for name in ["a", "a-b"] {
        store.ensure_branch(&branch(name), None).expect("ensure");
        let workspace = store.mount(&branch(name)).expect("mount");
        let directory = PathBuf::from(workspace.path());
        fs::create_dir_all(directory.join(".transcripts")).expect("mkdir");
        fs::write(directory.join(".transcripts/t.jsonl"), name).expect("write");
        store.unmount(&workspace).expect("unmount");
    }
    for name in ["a", "a-b"] {
        let kept = store.kept_transcripts(name);
        assert_eq!(kept.len(), 1, "{name}: {kept:?}");
        assert_eq!(
            fs::read_to_string(kept[0].join("t.jsonl")).expect("read"),
            name
        );
    }
}

fn refused_with(result: Result<impl std::fmt::Debug, WorkspaceError>, expected: &str) {
    match result {
        Err(WorkspaceError::Refused { code, .. }) => assert_eq!(code, expected),
        other => panic!("expected {expected}, got {other:?}"),
    }
}

/// Review #546/#509 FIX: a directory this store did not mount there (no
/// state, or another directory under the mounted name) is never checked,
/// replaced or deleted.
#[test]
fn a_directory_the_store_did_not_mount_is_never_touched() {
    let base = scratch("cow-owned");
    let repo = seeded(&base, 1);
    let store = CowWorkspace::new(&repo, base.join("work"));
    // An ordinary workspace of the same name, made by someone else.
    let foreign = store.root().join("agent_x");
    fs::create_dir_all(&foreign).expect("mkdir");
    fs::write(foreign.join("work.txt"), "theirs\n").expect("write");
    store
        .ensure_branch(&branch("agent_x"), None)
        .expect("ensure");
    let id = WorkspaceId::new(format!("agent_x:{}", foreign.display())).expect("id");
    refused_with(store.unmount(&id), WORKSPACE_NOT_OWNED);
    assert_eq!(
        fs::read_to_string(foreign.join("work.txt")).expect("kept"),
        "theirs\n"
    );

    // A mounted workspace put aside and another directory put in its
    // place: refused by every operation, both kept.
    store
        .ensure_branch(&branch("agent_a"), None)
        .expect("ensure");
    let workspace = store.mount(&branch("agent_a")).expect("mount");
    let directory = PathBuf::from(workspace.path());
    let aside = base.join("aside");
    fs::rename(&directory, &aside).expect("move");
    fs::create_dir_all(&directory).expect("mkdir");
    fs::write(directory.join("README.md"), "base\n").expect("write");
    refused_with(store.unmount(&workspace), WORKSPACE_NOT_OWNED);
    refused_with(store.commit(&workspace, "x"), WORKSPACE_NOT_OWNED);
    refused_with(store.mount(&branch("agent_a")), WORKSPACE_NOT_OWNED);
    assert!(directory.join("README.md").exists());
    // The mounted generation back in place: owned again.
    fs::remove_dir_all(&directory).expect("remove");
    fs::rename(&aside, &directory).expect("move back");
    store.unmount(&workspace).expect("unmount");
    assert!(!directory.exists());
}

/// A worker's admission lease keeps its workspace from being replaced,
/// unmounted or deduplicated; while it is offline no worker is admitted.
#[test]
fn a_worker_lease_blocks_replacement_unmount_and_dedup() {
    let base = scratch("cow-lease");
    let repo = seeded(&base, 1);
    let store = CowWorkspace::new(&repo, base.join("work"));
    store
        .ensure_branch(&branch("agent_a"), None)
        .expect("ensure");
    let workspace = store.mount(&branch("agent_a")).expect("mount");
    let directory = PathBuf::from(workspace.path());
    let lease = store.lease(&branch("agent_a")).expect("lease");
    refused_with(store.unmount(&workspace), WORKSPACE_BUSY);
    refused_with(store.offline(&workspace), WORKSPACE_BUSY);
    // The branch moves on: replacing the workspace is refused as well.
    let tree = git(&repo, &["rev-parse", "agent_a^{tree}"]);
    let next = git(
        &repo,
        &["commit-tree", &tree, "-p", "agent_a", "-m", "next"],
    );
    git(&repo, &["update-ref", "refs/heads/agent_a", &next]);
    refused_with(store.mount(&branch("agent_a")), WORKSPACE_BUSY);
    assert!(directory.join("README.md").exists());
    drop(lease);

    // Offline: no worker starts, and its build output is deduplicated.
    store.mount(&branch("agent_a")).expect("replace");
    fs::create_dir_all(directory.join("target")).expect("mkdir");
    fs::write(directory.join("target/a.bin"), noise(3, 100_000)).expect("write");
    fs::write(directory.join("target/b.bin"), noise(3, 100_000)).expect("write");
    let offline = store.offline(&workspace).expect("offline");
    refused_with(store.lease(&branch("agent_a")), WORKSPACE_BUSY);
    let mut config = DedupConfig::new(base.join("cas"));
    config.min_age = Duration::ZERO;
    let root = ArtifactRoot::new(&offline, directory.join("target")).expect("root");
    let report = dedup_dirs(&config, &[root]).expect("dedup");
    assert!(report.errors.is_empty(), "{report:?}");
    if report.cow_supported {
        assert_eq!(report.blobs_created + report.deduplicated, 2, "{report:?}");
    }
    drop(offline);
    assert_eq!(
        fs::read(directory.join("target/b.bin")).expect("read"),
        noise(3, 100_000)
    );
    let lease = store.lease(&branch("agent_a")).expect("admitted again");
    drop(lease);
    store.unmount(&workspace).expect("unmount");
}

/// A pass that cannot read part of a root says so: the error is in the
/// report and the root is marked incomplete.
#[test]
fn cas_dedup_reports_what_it_cannot_read() {
    if current_uid() == 0 {
        eprintln!("root reads every directory: test skipped");
        return;
    }
    let base = scratch("cow-dedup-unreadable");
    let target = base.join("w0/target");
    let hidden = target.join("hidden");
    fs::create_dir_all(&hidden).expect("mkdir");
    fs::write(hidden.join("big.bin"), noise(4, 100_000)).expect("write");
    set_mode(&hidden, 0o000);
    let mut config = DedupConfig::new(base.join("cas"));
    config.min_age = Duration::ZERO;
    let report = dedup(&config, &[&target]);
    set_mode(&hidden, 0o755);
    let report = report.expect("dedup");
    if report.cow_supported {
        assert_eq!(report.errors.len(), 1, "{report:?}");
        assert_eq!(report.incomplete, vec![target.clone()], "{report:?}");
    }
    assert_eq!(
        fs::read(hidden.join("big.bin")).expect("read"),
        noise(4, 100_000)
    );
}

/// A read-only (0444) source, as a CAS blob is, clones (or copies) into a
/// new file whose mode and times can then be set; an existing target is
/// never replaced.
#[test]
fn a_read_only_source_clones_and_takes_new_metadata() {
    let base = scratch("cow-readonly");
    let source = base.join("blob");
    let target = base.join("file");
    fs::write(&source, noise(6, 70_000)).expect("write");
    set_mode(&source, 0o444);
    let kind = clone_or_copy(&source, &target).expect("clone");
    if cow_clone_supported(&base) {
        assert_eq!(kind, CloneKind::Reflinked);
    }
    assert_eq!(fs::read(&target).expect("read"), noise(6, 70_000));
    set_mode(&target, 0o755);
    let then = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    fs::File::open(&target)
        .expect("open")
        .set_times(fs::FileTimes::new().set_modified(then))
        .expect("set times");
    let meta = fs::metadata(&target).expect("stat");
    assert_eq!(meta.mode() & 0o777, 0o755);
    assert_eq!(meta.modified().expect("mtime"), then);
    let existing = clone_or_copy(&source, &target).expect_err("the target exists");
    assert_eq!(existing.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read(&target).expect("read"), noise(6, 70_000));
}

/// Review #557 F1: the offline guard of one workspace never admits another
/// workspace's artifacts (named directly, through a link, or through a
/// linked ancestor), nor a directory put in its own place since.
#[test]
fn an_offline_guard_never_admits_another_workspaces_artifacts() {
    let base = scratch("cow-dedup-owner");
    let (a, b, c) = (base.join("ws/a"), base.join("ws/b"), base.join("ws/c"));
    for workspace in [&a, &b] {
        fs::create_dir_all(workspace.join("target")).expect("mkdir");
        fs::write(workspace.join("target/big.bin"), noise(7, 100_000)).expect("write");
    }
    fs::create_dir_all(&c).expect("mkdir");
    std::os::unix::fs::symlink(b.join("target"), c.join("target")).expect("symlink");
    std::os::unix::fs::symlink(&b, base.join("alias")).expect("symlink");
    // B's worker is admitted.
    let lease = WorkspaceLease::lease(&b).expect("lease");
    assert!(WorkspaceLease::exclusive(&b).expect("lock").is_none());
    let offline_a = WorkspaceLease::exclusive(&a).expect("lock").expect("free");
    let offline_c = WorkspaceLease::exclusive(&c).expect("lock").expect("free");
    for (guard, root) in [
        (&offline_a, b.join("target")),
        (&offline_a, base.join("alias/target")),
        (&offline_c, c.join("target")),
    ] {
        match ArtifactRoot::new(guard, &root) {
            Err(DedupError::NotArtifact(_)) => {}
            other => panic!("{} must be refused, got {other:?}", root.display()),
        }
    }
    // A's own root is A's.
    let own = ArtifactRoot::new(&offline_a, a.join("target")).expect("own root");
    assert_eq!(
        own.path(),
        fs::canonicalize(a.join("target")).expect("resolve")
    );
    drop(own);
    // A directory put in A's place is not the generation A's guard holds.
    fs::rename(&a, base.join("ws/a-old")).expect("move");
    fs::create_dir_all(a.join("target")).expect("mkdir");
    assert!(matches!(
        ArtifactRoot::new(&offline_a, a.join("target")),
        Err(DedupError::NotOffline(_))
    ));
    drop(lease);
    assert_eq!(
        fs::read(b.join("target/big.bin")).expect("read"),
        noise(7, 100_000)
    );
}

/// Review #557 F3: a path the branch tracks under a build-output name
/// (`target/config`) is never dropped by a checkpoint; a new build output
/// there is never added.
#[test]
fn a_tracked_path_under_a_build_directory_survives_checkpoints() {
    let base = scratch("cow-tracked-target");
    let repo = seeded(&base, 1);
    let checkout = repo.parent().expect("checkout").to_path_buf();
    fs::create_dir_all(checkout.join("target")).expect("mkdir");
    fs::write(checkout.join("target/config"), "tracked\n").expect("write");
    git(&checkout, &["add", "-f", "target/config"]);
    git(&checkout, &["commit", "-qm", "config"]);
    let store = CowWorkspace::new(&repo, base.join("work"));
    store
        .ensure_branch(&branch("agent_a"), None)
        .expect("ensure");
    let workspace = store.mount(&branch("agent_a")).expect("mount");
    let directory = PathBuf::from(workspace.path());
    assert_eq!(
        store.commit(&workspace, "noop").expect("commit"),
        Commit::Unchanged
    );
    fs::create_dir_all(directory.join("target/debug")).expect("mkdir");
    fs::write(directory.join("target/debug/app"), "output\n").expect("write");
    assert_eq!(
        store.commit(&workspace, "build").expect("commit"),
        Commit::Unchanged
    );
    assert_eq!(show(&repo, "agent_a:target/config"), "tracked");
    fs::write(directory.join("target/config"), "edited\n").expect("write");
    assert!(matches!(
        store.commit(&workspace, "edit"),
        Ok(Commit::Created(_))
    ));
    assert_eq!(show(&repo, "agent_a:target/config"), "edited");
    let tree = ls_tree(&repo, "agent_a");
    assert!(
        !tree.iter().any(|(path, _)| path == "target/debug/app"),
        "{tree:?}"
    );
    // A clean remount at the same head reuses the workspace as it is.
    assert_eq!(store.mount(&branch("agent_a")).expect("remount"), workspace);
}

/// Review #557 F4: a replaced duplicate keeps its group.
#[test]
fn cas_dedup_keeps_a_files_group() {
    let base = scratch("cow-dedup-group");
    if !cow_clone_supported(&base) {
        eprintln!("no copy-on-write clones on this file system: test skipped");
        return;
    }
    let target = base.join("w0/target");
    fs::create_dir_all(&target).expect("mkdir");
    fs::write(target.join("a.bin"), noise(8, 100_000)).expect("write");
    fs::write(target.join("b.bin"), noise(8, 100_000)).expect("write");
    // The first supplementary group this process can give `b.bin`. In a
    // user namespace (bubblewrap) `getgroups` lists groups that are not
    // mapped there, and `chgrp` to one fails with `EINVAL` (or `EPERM`).
    let own = rustix::process::getegid();
    let mut other = None;
    for group in rustix::process::getgroups().expect("groups") {
        if group == own {
            continue;
        }
        match std::os::unix::fs::chown(target.join("b.bin"), None, Some(group.as_raw())) {
            Ok(()) => {
                other = Some(group);
                break;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::InvalidInput | std::io::ErrorKind::PermissionDenied
                ) => {}
            Err(error) => panic!("chgrp: {error}"),
        }
    }
    let Some(other) = other else {
        eprintln!("no usable second group in this sandbox: test skipped");
        return;
    };
    let mut config = DedupConfig::new(base.join("cas"));
    config.min_age = Duration::ZERO;
    let report = dedup(&config, &[&target]).expect("dedup");
    assert!(report.errors.is_empty(), "{report:?}");
    assert_eq!(report.blobs_created + report.deduplicated, 2, "{report:?}");
    let meta = fs::metadata(target.join("b.bin")).expect("stat");
    assert_eq!(meta.gid(), other.as_raw());
    assert_eq!(
        fs::read(target.join("b.bin")).expect("read"),
        noise(8, 100_000)
    );
    // The index was replaced whole: no staged index is left beside it.
    let staged: Vec<_> = fs::read_dir(base.join("cas"))
        .expect("cas")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("index.") && name != "index.lock")
        .collect();
    assert!(staged.is_empty(), "{staged:?}");
    let again = dedup(&config, &[&target]).expect("dedup");
    assert_eq!(again.already_shared, 2, "{again:?}");
}

/// A repository whose `main` ignores `.env` and `dist/` (`.gitignore`),
/// with `agent_a` mounted from it by a store with a 1 MiB file cap.
fn ignoring(base: &Path) -> (PathBuf, CowWorkspace, WorkspaceId) {
    let repo = seeded(base, 1);
    let checkout = repo.parent().expect("checkout").to_path_buf();
    fs::write(checkout.join(".gitignore"), ".env\ndist/\n").expect("write");
    git(&checkout, &["add", ".gitignore"]);
    git(&checkout, &["commit", "-qm", "ignore"]);
    let store = CowWorkspace::new(&repo, base.join("work")).with_max_file_bytes(1 << 20);
    store
        .ensure_branch(&branch("agent_a"), None)
        .expect("ensure");
    let workspace = store.mount(&branch("agent_a")).expect("mount");
    (repo, store, workspace)
}

fn tree_paths(repo: &Path) -> Vec<String> {
    ls_tree(repo, "agent_a")
        .into_iter()
        .map(|(path, _)| path)
        .collect()
}

/// Review #557 r1 P1-3 (a): a file the project's `.gitignore` ignores
/// (`.env`, a `dist/` bundle) never enters a checkpoint, as with the Git
/// store; a tracked file is still taken.
#[test]
fn a_checkpoint_never_takes_an_ignored_file() {
    let base = scratch("cow-ignored");
    let (repo, store, workspace) = ignoring(&base);
    let directory = PathBuf::from(workspace.path());
    fs::write(directory.join(".env"), "SECRET=1\n").expect("write");
    fs::create_dir_all(directory.join("dist")).expect("mkdir");
    fs::write(directory.join("dist/bundle.js"), noise(9, 300_000)).expect("write");
    fs::create_dir_all(directory.join("web/dist")).expect("mkdir");
    fs::write(directory.join("web/dist/app.js"), "x\n").expect("write");
    assert_eq!(
        store.commit(&workspace, "ignored").expect("commit"),
        Commit::Unchanged
    );
    fs::write(directory.join("README.md"), "edited\n").expect("write");
    assert!(matches!(
        store.commit(&workspace, "edit"),
        Ok(Commit::Created(_))
    ));
    let paths = tree_paths(&repo);
    for ignored in [".env", "dist/bundle.js", "web/dist/app.js"] {
        assert!(
            !paths.iter().any(|path| path == ignored),
            "{ignored}: {paths:?}"
        );
    }
    assert_eq!(show(&repo, "agent_a:README.md"), "edited");
}

/// Review #557 r1 P1-3 (b): the engine's runtime files
/// (`ENGINE_RUNTIME_FILES`) never enter a checkpoint; the rest of
/// `.claude/` does.
#[test]
fn a_checkpoint_never_takes_an_engine_runtime_file() {
    let base = scratch("cow-runtime");
    let (repo, store, workspace) = ignoring(&base);
    let directory = PathBuf::from(workspace.path());
    fs::create_dir_all(directory.join(".claude/worktrees/w1")).expect("mkdir");
    fs::write(directory.join(".claude/scheduled_tasks.lock"), "1\n").expect("write");
    fs::write(directory.join(".claude/settings.local.json"), "{}\n").expect("write");
    fs::write(directory.join(".claude/worktrees/w1/file"), "x\n").expect("write");
    fs::create_dir_all(directory.join("sub/.claude")).expect("mkdir");
    fs::write(directory.join("sub/.claude/scheduled_tasks.lock"), "1\n").expect("write");
    fs::write(directory.join(".claude/settings.json"), "{}\n").expect("write");
    assert!(matches!(
        store.commit(&workspace, "claude"),
        Ok(Commit::Created(_))
    ));
    let paths = tree_paths(&repo);
    assert!(
        paths.iter().any(|path| path == ".claude/settings.json"),
        "{paths:?}"
    );
    for runtime in spawn_host::ENGINE_RUNTIME_FILES {
        assert!(
            !paths
                .iter()
                .any(|path| path.contains(runtime.trim_end_matches('/'))),
            "{runtime}: {paths:?}"
        );
    }
}

/// Review #557 r1 P1-3 (c): a file over the size cap (`max_file_bytes`) is
/// left out of the checkpoint, and the agent is told once, as by the Git
/// store.
#[test]
fn a_checkpoint_leaves_out_a_file_over_the_size_cap_and_says_so() {
    let base = scratch("cow-cap");
    let (repo, store, workspace) = ignoring(&base);
    let directory = PathBuf::from(workspace.path());
    fs::write(directory.join("huge.bin"), noise(10, 2 << 20)).expect("write");
    fs::write(directory.join("small.bin"), noise(11, 1000)).expect("write");
    assert!(matches!(
        store.commit(&workspace, "files"),
        Ok(Commit::Created(_))
    ));
    let paths = tree_paths(&repo);
    assert!(paths.iter().any(|path| path == "small.bin"), "{paths:?}");
    assert!(!paths.iter().any(|path| path == "huge.bin"), "{paths:?}");
    let notices = store.notices(&workspace);
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("huge.bin"), "{notices:?}");
    store.notices_told(&workspace, &notices);
    assert!(store.notices(&workspace).is_empty());
    assert!(directory.join("huge.bin").exists(), "left in the workspace");
}

/// Review #557 r1 P2-4: a file the pass cannot open (`EACCES`) is an error
/// that marks its root incomplete, never a "busy" skip; prune keeps an
/// index row whose file it cannot look at (and its blob), and says so.
#[test]
fn cas_dedup_reports_access_errors_and_prune_keeps_what_it_cannot_see() {
    if current_uid() == 0 {
        eprintln!("root reads every file: test skipped");
        return;
    }
    let base = scratch("cow-dedup-eacces");
    if !cow_clone_supported(&base) {
        eprintln!("no copy-on-write clones on this file system: test skipped");
        return;
    }
    let target = base.join("w0/target");
    let sub = target.join("sub");
    fs::create_dir_all(&sub).expect("mkdir");
    fs::write(sub.join("a.bin"), noise(12, 100_000)).expect("write");
    fs::write(sub.join("b.bin"), noise(12, 100_000)).expect("write");
    fs::write(target.join("locked.bin"), noise(13, 100_000)).expect("write");
    set_mode(&target.join("locked.bin"), 0o000);
    let mut config = DedupConfig::new(base.join("cas"));
    config.min_age = Duration::ZERO;
    let report = dedup(&config, &[&target]);
    set_mode(&target.join("locked.bin"), 0o644);
    let report = report.expect("dedup");
    assert_eq!(report.errors.len(), 1, "{report:?}");
    assert!(report.errors[0].contains("locked.bin"), "{report:?}");
    assert_eq!(report.skipped_busy, 0, "{report:?}");
    assert_eq!(report.incomplete.len(), 1, "{report:?}");
    assert_eq!(report.blobs_created + report.deduplicated, 2, "{report:?}");

    set_mode(&sub, 0o000);
    let pruned = prune_unused(&base.join("cas"), Duration::ZERO);
    set_mode(&sub, 0o755);
    let pruned = pruned.expect("prune");
    assert_eq!(pruned.live_entries, 2, "{pruned:?}");
    assert_eq!(pruned.dropped_entries, 0, "{pruned:?}");
    assert_eq!(pruned.blobs_removed, 0, "{pruned:?}");
    assert_eq!(pruned.errors.len(), 2, "{pruned:?}");
    let again = dedup(&config, &[&target]).expect("dedup");
    assert_eq!(again.already_shared, 2, "{again:?}");
}

/// Review #557 r2 P1: a tracked directory replaced by a file of the same
/// name (`ENOTDIR` on the paths git still lists under it) is a deletion:
/// the checkpoint and the unmount both go through.
#[test]
fn a_tracked_directory_replaced_by_a_file_is_checkpointed_and_unmounted() {
    let base = scratch("cow-notdir");
    let (repo, store, workspace) = ignoring(&base);
    let directory = PathBuf::from(workspace.path());
    fs::remove_dir_all(directory.join("src")).expect("remove");
    fs::write(directory.join("src"), "now a file\n").expect("write");
    assert!(matches!(
        store.commit(&workspace, "src is a file"),
        Ok(Commit::Created(_))
    ));
    let paths = tree_paths(&repo);
    assert!(paths.iter().any(|path| path == "src"), "{paths:?}");
    assert!(
        !paths.iter().any(|path| path.starts_with("src/")),
        "{paths:?}"
    );
    assert_eq!(show(&repo, "agent_a:src"), "now a file");
    store.unmount(&workspace).expect("unmount");
    assert!(!directory.exists());
}

/// Review #557 r2 P3: an untracked file this store never takes (a large
/// transcript under `.transcripts/`) is never told of as left out for its
/// size.
#[test]
fn a_large_untracked_transcript_is_no_size_notice() {
    let base = scratch("cow-cap-transcript");
    let (_repo, store, workspace) = ignoring(&base);
    let directory = PathBuf::from(workspace.path());
    fs::create_dir_all(directory.join("logs/.transcripts")).expect("mkdir");
    fs::write(
        directory.join("logs/.transcripts/big.bin"),
        noise(14, 2 << 20),
    )
    .expect("write");
    assert_eq!(
        store.commit(&workspace, "nothing").expect("commit"),
        Commit::Unchanged
    );
    assert!(store.notices(&workspace).is_empty());
}

/// `cargo test -p spawn-host --test cow -- --ignored --nocapture`
#[test]
#[ignore = "micro-benchmark: prints timings and reclaimed bytes"]
fn bench_cow_mount_commit_and_dedup() {
    let base = scratch("cow-bench");
    let repo = seeded(&base, 2000);
    let store = CowWorkspace::new(&repo, base.join("work"));
    println!("copy-on-write supported: {}", store.cow_supported());
    for at in 0..4 {
        let name = format!("agent_{at}");
        store.ensure_branch(&branch(&name), None).expect("ensure");
        let started = Instant::now();
        let workspace = store.mount(&branch(&name)).expect("mount");
        let mounted = started.elapsed();
        let mount_stats = store.last_stats();
        let started = Instant::now();
        let commit = store.commit(&workspace, "noop").expect("commit");
        println!(
            "{name}: mount {mounted:?} ({mount_stats:?}), unchanged commit {:?} ({commit:?}, {:?})",
            started.elapsed(),
            store.last_stats()
        );
    }

    if !cow_clone_supported(&base) {
        println!("no clones: dedup benchmark skipped");
        return;
    }
    let workers = 4;
    let artifacts = 24;
    let targets: Vec<PathBuf> = (0..workers)
        .map(|worker| {
            let target = base.join(format!("bw{worker}/target"));
            let deps = target.join("debug/deps");
            fs::create_dir_all(&deps).expect("mkdir");
            for at in 0..artifacts {
                fs::write(deps.join(format!("lib{at}.rlib")), noise(at, 1 << 20)).expect("write");
            }
            target
        })
        .collect();
    let mut config = DedupConfig::new(base.join("cas"));
    config.min_age = Duration::ZERO;
    let refs: Vec<&Path> = targets.iter().map(PathBuf::as_path).collect();
    let started = Instant::now();
    let report = dedup(&config, &refs).expect("dedup");
    let first = started.elapsed();
    let started = Instant::now();
    let again = dedup(&config, &refs).expect("dedup");
    println!(
        "dedup {workers}x{artifacts} MiB: {first:?}, reclaimed {} bytes ({} files, {} blobs); \
         second pass {:?} ({} already shared)",
        report.reclaimed_bytes,
        report.deduplicated,
        report.blobs_created,
        started.elapsed(),
        again.already_shared
    );
    assert_eq!(
        report.reclaimed_bytes,
        (workers - 1) * artifacts * (1 << 20)
    );
}
