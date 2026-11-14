use super::{
    BUDGET_GAP, Freshness, Mission, NO_TEST_DATABASE, aborted, active_ms, budget_gap, floor_gone,
    floor_reason, freshness, gates_at_once, halt_reason, markers_note, merged_tree,
    needs_test_database, no_room, over_budget, pinned, report_freshness, room_backoff, room_wait,
    short_of_room, task_scenario, waits_for_gate, wall_clock_minutes,
    withheld_without_test_database,
};
use crate::gate::{GateFailure, Shortfall};
use serde_json::json;

/// SPAWN_INTEGRATE_DRAIN_SECS: unset is two hours; a whole number of
/// seconds from 1 to a day is taken; anything else refuses to start,
/// naming the variable.
#[test]
fn the_drain_bound_is_a_whole_number_of_seconds_up_to_a_day() {
    assert_eq!(super::drain_secs(None), Ok(7200));
    assert_eq!(super::drain_secs(Some("3")), Ok(3));
    assert_eq!(super::drain_secs(Some(" 600 ")), Ok(600));
    assert_eq!(super::drain_secs(Some("86400")), Ok(86_400));
    for bad in [
        "0",
        "86401",
        "x",
        "1.5",
        "-5",
        "+5",
        "",
        " ",
        "1e3",
        "99999999999999999999",
    ] {
        let error = super::drain_secs(Some(bad)).expect_err(bad);
        assert!(error.contains("SPAWN_INTEGRATE_DRAIN_SECS"), "{error}");
    }
}

/// The integrator's pid file is its pid and start time, nothing else.
#[test]
fn a_pid_file_names_a_pid_and_its_start_time() {
    assert_eq!(super::read_pid_file("4242 991\n"), Some((4242, 991)));
    assert_eq!(super::read_pid_file("4242\n"), None);
    assert_eq!(super::read_pid_file("4242 991 7\n"), None);
    assert_eq!(super::read_pid_file("x 991\n"), None);
    assert_eq!(super::read_pid_file(""), None);
}

/// `spawn integrate --drain` finds the integrator by its pid file: this
/// process runs, a start time that is not its own (a pid used again)
/// is gone, and a root without the file has no integrator.
#[test]
fn a_drain_request_needs_the_running_integrator_of_its_root() {
    let pid = std::process::id();
    let ticks = super::warm::start_ticks(pid).expect("start time");
    assert!(super::still_runs(pid, ticks));
    assert!(!super::still_runs(pid, ticks + 1));
    let root = std::env::temp_dir().join(format!("spawn-drain-root-{pid}"));
    let _ = std::fs::remove_dir_all(&root);
    let error = super::request_drain(&root).expect_err("no pid file");
    assert!(error.contains("no integrator runs"), "{error}");
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(root.join(super::PID_FILE), format!("{pid} {}\n", ticks + 1)).expect("pid file");
    let error = super::request_drain(&root).expect_err("gone");
    assert!(error.contains("is gone"), "{error}");
    super::write_pid_file(&root);
    assert_eq!(
        super::read_pid_file(
            &std::fs::read_to_string(root.join(super::PID_FILE)).expect("written")
        ),
        Some((pid, ticks))
    );
    let _ = std::fs::remove_dir_all(&root);
}

fn disk_short(free_kb: u64, min_gb: u64, tmpdir: &str) -> Shortfall {
    Shortfall::Disk {
        free_kb,
        min_gb,
        tmpdir: tmpdir.to_string(),
    }
}

/// One gate at a time unless the test server is per gate: more is
/// refused without the opt-in, whatever asks for it.
#[test]
fn the_gate_cap_is_one_and_more_needs_a_server_per_gate() {
    use super::{DEFAULT_GATES, MAX_GATES, gate_slots};
    let per_gate = || Some("1");
    assert_eq!(gate_slots(None, None, None), Ok(1));
    assert_eq!(DEFAULT_GATES, 1);
    assert_eq!(gate_slots(Some("1".into()), None, None), Ok(1));
    for (flag, env) in [(Some("2"), None), (None, Some("3")), (Some("8"), Some("1"))] {
        let refused = gate_slots(flag.map(Into::into), env.map(Into::into), None);
        assert!(
            refused
                .as_ref()
                .is_err_and(|error| error.contains("SPAWN_TEST_DATABASE_PER_GATE=1")),
            "{flag:?} {env:?}: {refused:?}"
        );
        assert!(gate_slots(flag.map(Into::into), env.map(Into::into), Some("yes")).is_err());
    }
    assert_eq!(gate_slots(None, Some("3".into()), per_gate()), Ok(3));
    assert_eq!(
        gate_slots(Some("1".into()), Some("3".into()), per_gate()),
        Ok(1),
        "the flag first"
    );
    for wrong in ["0", "-1", "two", ""] {
        assert!(
            gate_slots(Some(wrong.into()), None, per_gate()).is_err(),
            "{wrong}"
        );
    }
    assert!(gate_slots(None, Some((MAX_GATES + 1).to_string()), per_gate()).is_err());
}

/// Only exit 1 is a conflict: a merge-tree git cannot run (an unknown
/// option, a crash, a signal) is an error carrying git's message.
#[test]
fn a_merge_git_cannot_run_is_an_error_not_a_conflict() {
    assert_eq!(
        merged_tree(Some(0), "abc123\n", ""),
        Ok(Some("abc123".to_string()))
    );
    assert_eq!(merged_tree(Some(1), "def456\nCONFLICT", ""), Ok(None));
    let unknown = merged_tree(
        Some(129),
        "",
        "error: unknown option `attr-source=abc'\nusage: git [-v | --version]",
    )
    .expect_err("exit 129 is no conflict");
    assert!(unknown.contains("unknown option"), "{unknown}");
    assert!(unknown.contains("exited 129"), "{unknown}");
    assert!(unknown.contains("not a conflict"), "{unknown}");
    assert!(unknown.contains("2.43"), "{unknown}");
    let crash = merged_tree(None, "", "").expect_err("a signal is no conflict");
    assert!(crash.contains("killed by a signal"), "{crash}");
    assert!(merged_tree(Some(139), "", "Segmentation fault").is_err());
    assert!(merged_tree(Some(128), "", "fatal: bad object").is_err());
    assert!(merged_tree(Some(2), "", "").is_err());
}

#[test]
fn a_task_scenario_is_read_from_the_mission_s_scenarios_as_they_are_now() {
    let now = std::path::Path::new("/live/controllers/spawn-abc/rsi/scenarios");
    assert_eq!(
        task_scenario(now, Some("/home/owner/projects/Spawn/rsi/scenarios/007.sh")),
        Some(now.join("007.sh"))
    );
    assert_eq!(task_scenario(now, None), None);
    assert_eq!(task_scenario(now, Some("/")), None);
}

/// deploy/deploy.sh tells a binary whose gates outlive it by the words
/// it says at start ([`super::UNITS_LINE`]): the two must not drift.
#[test]
fn deploy_reads_the_words_the_integrator_says_of_its_units() {
    let deploy = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/deploy.sh"),
    )
    .expect("deploy/deploy.sh");
    let words = super::UNITS_LINE
        .split(':')
        .next()
        .expect("the words before the colon");
    assert!(
        deploy.contains(&format!("grep -aqF '{words}")),
        "deploy.sh greps for {words:?}"
    );
}

#[test]
fn pinned_resolves_the_controller_symlink_once() {
    let base = std::env::temp_dir().join(format!("spawn-pinned-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("spawn-one/rsi/scenarios")).expect("one");
    std::fs::create_dir_all(base.join("spawn-two/rsi/scenarios")).expect("two");
    std::os::unix::fs::symlink("spawn-one", base.join("spawn")).expect("link");
    let through = base.join("spawn/rsi/scenarios");
    let held = pinned(&through);
    assert!(
        held.ends_with("spawn-one/rsi/scenarios"),
        "{}",
        held.display()
    );
    // The switch: what was pinned stays the first controller's.
    std::fs::remove_file(base.join("spawn")).expect("unlink");
    std::os::unix::fs::symlink("spawn-two", base.join("spawn")).expect("relink");
    assert!(held.ends_with("spawn-one/rsi/scenarios"));
    assert!(pinned(&through).ends_with("spawn-two/rsi/scenarios"));
    // A path that does not resolve stays as named.
    let missing = base.join("missing/scenarios");
    assert_eq!(pinned(&missing), missing);
    std::fs::remove_dir_all(&base).expect("clean");
}

/// A request names its commit by its full hash only (SHA-1 or SHA-256),
/// never by an option or a revision expression.
#[test]
fn an_owner_request_names_its_commit_by_full_hash() {
    use super::owner::{full_hash, owner_ref};
    assert!(full_hash(&"a1".repeat(20)));
    assert!(full_hash(&"0f".repeat(32)));
    for bad in [
        "HEAD",
        "--all",
        "main",
        &"A1".repeat(20),
        &"a1".repeat(19),
        "",
        &"g".repeat(40),
    ] {
        assert!(!full_hash(bad), "{bad}");
    }
    assert_eq!(owner_ref("abc"), "refs/spawn-owner/abc");
}

/// The merge that lands the owner's commit says it is the owner's
/// landing, names the request and the commit's subject (bounded), and
/// carries the request and the commit as trailers.
#[test]
fn an_owner_landing_merge_names_its_request_and_the_commits_subject() {
    let message = super::owner::owner_message(42, "c0ffee", "ba5e", "owner: the contract\n");
    assert!(
        message.starts_with("Owner landing (request 42): owner: the contract\n\n"),
        "{message}"
    );
    assert!(
        message.contains("c0ffee") && message.contains("ba5e"),
        "{message}"
    );
    assert!(
        message.ends_with("\n\nSpawn-Owner-Request: 42\nSpawn-Owner-Commit: c0ffee\n"),
        "{message}"
    );
    let long = super::owner::owner_message(1, "c", "m", &"x".repeat(500));
    let first = long.lines().next().expect("a first line");
    assert!(first.chars().count() < 140, "{first}");
}

/// Commit everything in the checkout `directory`.
pub(super) fn commit_in(directory: &str, message: &str) {
    let git = crate::sources::test_git;
    git(&["-C", directory, "add", "-A"]).expect("add");
    git(&[
        "-C",
        directory,
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "--no-gpg-sign",
        "-qm",
        message,
    ])
    .expect("commit");
}

/// What the integrator does with an agent's push that passed: fetch it
/// into its own ref, build the landing ([`super::landing_candidate`]),
/// move the source's main and the mirror's as `land` does.
fn land_as_the_integrator(mirror: &std::path::Path, source: &str, agent: &str, task: i64) {
    let git = crate::sources::test_git;
    let mirror_path = mirror.to_string_lossy().to_string();
    let tracking = format!("refs/spawn-integrate/{agent}");
    let pushed = crate::sources::fetch_pushed_into(mirror, source, agent, &tracking)
        .expect("fetch")
        .expect("pushed");
    let main = git(&["--git-dir", &mirror_path, "rev-parse", "refs/heads/main"]).expect("main");
    let message = format!("Work (task {task})\n\nThe work.\n");
    let candidate = super::landing_candidate(&mirror_path, &main, &pushed, &message)
        .expect("candidate")
        .expect("no conflict");
    assert_ne!(candidate, pushed, "a landing of its own");
    let to_main = format!("{candidate}:refs/heads/main");
    git(&["--git-dir", &mirror_path, "push", "-q", source, &to_main]).expect("land");
    git(&[
        "--git-dir",
        &mirror_path,
        "update-ref",
        "refs/heads/main",
        &candidate,
    ])
    .expect("mirror main");
}

/// After a landing, the agent's pushed branch is not left behind: the
/// integrator writes no ref of the agent, and the Bridge's next merge of
/// main is a new commit (which `MissionWorkspaces` marks for the
/// pusher), so `spawn/<agent>` contains main again once pushed. Both
/// landing shapes that are not the branch head itself: a commit without
/// the large-file trailer, and a merge with a main that moved.
#[test]
fn a_landing_reaches_the_agents_pushed_branch_through_the_next_merge() {
    use crate::push::{Pushed, push_branch};
    use crate::sources::{ensure_mirror, test_git as git};
    use spawn_core::id::BranchName;
    use spawn_core::port::{BaseMerge, Commit, WorkspaceStore};
    use spawn_host::GitWorkspace;

    // Removed when the test ends (kept when it fails).
    let base = spawn_host::Scratch::new("spawn-land-push");
    let seed = base.join("seed");
    std::fs::create_dir_all(&seed).expect("seed directory");
    let seed_path = seed.to_string_lossy().to_string();
    git(&["init", "-q", "--initial-branch=main", &seed_path]).expect("init");
    std::fs::write(seed.join("README.md"), "base\n").expect("write");
    commit_in(&seed_path, "base");
    let source = base.join("source.git").to_string_lossy().to_string();
    git(&["clone", "-q", "--bare", &seed_path, &source]).expect("bare source");
    let root = base.join("host");
    let mirror = ensure_mirror(&root, Some(&source)).expect("mirror");
    let mirror_path = mirror.to_string_lossy().to_string();
    let store = GitWorkspace::new(&mirror, &root);
    let rev = |reference: &str| {
        git(&["--git-dir", &mirror_path, "rev-parse", reference]).expect("rev-parse")
    };
    let source_rev = |reference: &str| {
        git(&["--git-dir", &source, "rev-parse", reference]).expect("rev-parse on source")
    };

    for (task, agent, main_moves) in [(1, "agent_large", false), (2, "agent_late", true)] {
        let branch = BranchName::new(agent).expect("valid branch");
        store.ensure_branch(&branch, None).expect("branch");
        let workspace = store.mount(&branch).expect("mount");
        let directory = std::path::Path::new(workspace.path());
        std::fs::write(directory.join(format!("{agent}.txt")), "work\n").expect("write");
        assert!(matches!(
            store.commit(&workspace, "work").expect("commit"),
            Commit::Created(_)
        ));
        if !main_moves {
            // A head an earlier version wrote, naming large files kept
            // outside git: the landing is a commit of its own, without
            // the trailer.
            let head = rev(&format!("refs/heads/{agent}"));
            let message = format!("work\n\nSpawn-Large: {}", "a".repeat(64));
            let named = git(&[
                "--git-dir",
                &mirror_path,
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit-tree",
                &format!("{head}^{{tree}}"),
                "-p",
                &head,
                "-m",
                &message,
            ])
            .expect("a head with the trailer");
            let reference = format!("refs/heads/{agent}");
            git(&[
                "--git-dir",
                &mirror_path,
                "update-ref",
                &reference,
                &named,
                &head,
            ])
            .expect("move the branch");
        }
        assert_eq!(push_branch(&mirror, &source, agent), Pushed::Done);
        if main_moves {
            // Main moved since the worker started.
            let clone = base.join(format!("clone-{agent}"));
            let clone_path = clone.to_string_lossy().to_string();
            git(&["clone", "-q", &source, &clone_path]).expect("clone");
            std::fs::write(clone.join("other.txt"), "theirs\n").expect("write");
            commit_in(&clone_path, "elsewhere");
            git(&["-C", &clone_path, "push", "-q", "origin", "main"]).expect("push main");
            crate::sources::refresh(&mirror, &source).expect("refresh");
        }

        let agent_head = rev(&format!("refs/heads/{agent}"));
        land_as_the_integrator(&mirror, &source, agent, task);
        assert_eq!(
            rev(&format!("refs/heads/{agent}")),
            agent_head,
            "the integrator moves no ref of the agent"
        );

        // The Bridge's next merge of main is a new commit, and once
        // pushed the agent's branch on the source holds the landing.
        let BaseMerge::Merged { commit, .. } = store.merge_base(&workspace).expect("merge") else {
            panic!("the landing is new to the branch");
        };
        assert_eq!(push_branch(&mirror, &source, agent), Pushed::Done);
        let pushed_now = source_rev(&format!("refs/heads/spawn/{agent}"));
        assert_eq!(pushed_now, commit.as_str());
        git(&[
            "--git-dir",
            &source,
            "merge-base",
            "--is-ancestor",
            &source_rev("refs/heads/main"),
            &pushed_now,
        ])
        .expect("spawn/<agent> contains main");
        store.unmount(&workspace).expect("unmount");
    }
}

/// A source with `README.md` and `notes.txt` on main, its mirror under a
/// host root, and that host's store: `(base, source, root, mirror, store)`.
fn world(
    tag: &str,
) -> (
    std::path::PathBuf,
    String,
    std::path::PathBuf,
    std::path::PathBuf,
    spawn_host::GitWorkspace,
) {
    use crate::sources::{ensure_mirror, test_git as git};
    let base = std::env::temp_dir().join(format!("spawn-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let seed = base.join("seed");
    std::fs::create_dir_all(&seed).expect("seed directory");
    let seed_path = seed.to_string_lossy().to_string();
    git(&["init", "-q", "--initial-branch=main", &seed_path]).expect("init");
    std::fs::write(seed.join("README.md"), "one\ntwo\nthree\n").expect("write");
    std::fs::write(seed.join("notes.txt"), "notes\n").expect("write");
    commit_in(&seed_path, "base");
    let source = base.join("source.git").to_string_lossy().to_string();
    git(&["clone", "-q", "--bare", &seed_path, &source]).expect("bare source");
    let root = base.join("host");
    let mirror = ensure_mirror(&root, Some(&source)).expect("mirror");
    let store = spawn_host::GitWorkspace::new(&mirror, &root);
    (base, source, root, mirror, store)
}

/// Main moves on the source by `change` in a clone, and the mirror
/// follows; main's new commit.
fn main_moves(
    base: &std::path::Path,
    source: &str,
    mirror: &std::path::Path,
    change: impl Fn(&std::path::Path),
) -> String {
    use crate::sources::test_git as git;
    let clone = base.join(format!("clone-{}", fastrand_name()));
    let clone_path = clone.to_string_lossy().to_string();
    git(&["clone", "-q", source, &clone_path]).expect("clone");
    change(&clone);
    commit_in(&clone_path, "main moves");
    git(&["-C", &clone_path, "push", "-q", "origin", "main"]).expect("push main");
    crate::sources::refresh(mirror, source).expect("refresh");
    git(&[
        "--git-dir",
        &mirror.to_string_lossy(),
        "rev-parse",
        "refs/heads/main",
    ])
    .expect("main")
}

fn fastrand_name() -> String {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed).to_string()
}

/// An agent's branch with one checkpoint by `work`, mounted, and pushed
/// as `spawn/<agent>`: its workspace.
fn agent_with_work(
    store: &spawn_host::GitWorkspace,
    mirror: &std::path::Path,
    source: &str,
    agent: &str,
    work: impl Fn(&std::path::Path),
) -> spawn_core::id::WorkspaceId {
    use crate::push::{Pushed, push_branch};
    use spawn_core::id::BranchName;
    use spawn_core::port::{Commit, WorkspaceStore};
    let branch = BranchName::new(agent).expect("valid branch");
    store.ensure_branch(&branch, None).expect("branch");
    let workspace = store.mount(&branch).expect("mount");
    work(std::path::Path::new(workspace.path()));
    assert!(matches!(
        store.commit(&workspace, "work").expect("commit"),
        Commit::Created(_)
    ));
    assert_eq!(push_branch(mirror, source, agent), Pushed::Done);
    workspace
}

/// What one integrator pass judges of `agent`'s push against the
/// mirror's main: the landing candidate, `None` for a conflict.
fn judged(mirror: &std::path::Path, source: &str, agent: &str) -> Option<String> {
    let mirror_path = mirror.to_string_lossy().to_string();
    let pushed = crate::sources::fetch_pushed_into(
        mirror,
        source,
        agent,
        &format!("refs/spawn-integrate/{agent}"),
    )
    .expect("fetch")
    .expect("pushed");
    let main =
        crate::sources::test_git(&["--git-dir", &mirror_path, "rev-parse", "refs/heads/main"])
            .expect("main");
    super::landing_candidate(&mirror_path, &main, &pushed, "Work (task 1)\n\nThe work.\n")
        .expect("candidate")
}

/// The whole loop of a conflict: the verdict merges main into the idle
/// workspace with markers and leaves the merge pending, its note names
/// the marked file, a report before they are resolved is sent back, the
/// checkpoint that resolves them has main as its second parent, and that
/// work lands.
#[test]
#[allow(clippy::too_many_lines)]
fn a_conflict_verdict_leaves_markers_the_resolution_merges_main_and_lands() {
    use crate::main_merge::{Merge, conflict_note, into_workspace};
    use crate::push::{Pushed, push_branch};
    use crate::sources::test_git as git;
    use spawn_core::port::{Commit, WorkspaceStore};

    let (base, source, root, mirror, store) = world("conflict-loop");
    let mirror_path = mirror.to_string_lossy().to_string();
    let agent = "agent_resident";
    let workspace = agent_with_work(&store, &mirror, &source, agent, |directory| {
        std::fs::write(
            directory.join("README.md"),
            "one\nTWO by the agent\nthree\n",
        )
        .expect("write");
    });
    let main = main_moves(&base, &source, &mirror, |clone| {
        std::fs::write(clone.join("README.md"), "one\nTWO on main\nthree\n").expect("write");
    });
    assert_eq!(judged(&mirror, &source, agent), None, "a conflict verdict");

    // The verdict's merge, before the note is written.
    let merge = into_workspace(&store, &mirror, &root, agent);
    assert_eq!(
        merge,
        Merge::Marked {
            files: vec!["README.md".to_string()]
        }
    );
    assert!(
        store.has_pending_merge(agent),
        "main waits as the second parent"
    );
    let readme = std::fs::read_to_string(root.join(agent).join("README.md")).expect("read");
    assert!(
        readme.contains("<<<<<<<") && readme.contains("TWO on main"),
        "{readme}"
    );
    let note = conflict_note(&main, &merge);
    assert!(
        note.contains("README.md")
            && note.contains("carry conflict markers")
            && note.contains(&main)
            && !note.contains("within about 30 s"),
        "{note}"
    );
    // Asked again (the next delivery), the merge is still the same one.
    assert_eq!(into_workspace(&store, &mirror, &root, agent), merge);

    // A report with the markers in: the judge sends it back unjudged.
    assert!(matches!(
        store.commit(&workspace, "markers").expect("commit"),
        Commit::Created(_)
    ));
    assert_eq!(push_branch(&mirror, &source, agent), Pushed::Done);
    let head = git(&[
        "--git-dir",
        &mirror_path,
        "rev-parse",
        &format!("refs/heads/{agent}"),
    ])
    .expect("head");
    let marked = store.conflict_markers(&main, &head).expect("markers");
    assert!(
        super::markers_note(store.has_pending_merge(agent), &marked).is_some(),
        "unresolved markers are not judged"
    );

    // The agent resolves them; that checkpoint records the merge.
    std::fs::write(
        root.join(agent).join("README.md"),
        "one\nTWO by the agent, on main\nthree\n",
    )
    .expect("resolve");
    let Commit::Created(resolved) = store.commit(&workspace, "resolved").expect("commit") else {
        panic!("the resolution is a commit");
    };
    assert_eq!(
        git(&[
            "--git-dir",
            &mirror_path,
            "rev-parse",
            &format!("{resolved}^2")
        ])
        .expect("second parent"),
        main,
        "main is the resolved checkpoint's second parent"
    );
    assert!(!store.has_pending_merge(agent));
    assert_eq!(push_branch(&mirror, &source, agent), Pushed::Done);
    assert!(
        store
            .conflict_markers(&main, resolved.as_str())
            .expect("markers")
            .is_empty()
    );
    assert_eq!(super::markers_note(false, &[]), None);

    // Judged again: it contains main, so it lands as a commit of its
    // tree on top of main, with the resolution as its second parent.
    let candidate = judged(&mirror, &source, agent).expect("no conflict now");
    assert_ne!(candidate, resolved.as_str(), "never the worker's commit");
    let rev = |reference: String| {
        git(&["--git-dir", &mirror_path, "rev-parse", &reference]).expect("rev-parse")
    };
    assert_eq!(
        rev(format!("{candidate}^1")),
        main,
        "main is the first parent"
    );
    assert_eq!(rev(format!("{candidate}^2")), resolved.as_str());
    assert_eq!(
        rev(format!("{candidate}^{{tree}}")),
        rev(format!("{resolved}^{{tree}}"))
    );
    git(&[
        "--git-dir",
        &mirror_path,
        "push",
        "-q",
        &source,
        &format!("{candidate}:refs/heads/main"),
    ])
    .expect("lands with one commit on top of main");
    assert_eq!(
        git(&["--git-dir", &source, "show", "main:README.md"]).expect("main's README"),
        "one\nTWO by the agent, on main\nthree"
    );
    store.unmount(&workspace).expect("unmount");
    let _ = std::fs::remove_dir_all(&base);
}

/// A conflict markers cannot show: nothing in the workspace changes but
/// main's version of each file, copied aside, and the note says what to
/// do with each one. Done as it says, the work lands.
#[test]
fn a_conflict_without_markers_is_explained_and_resolvable_without_git() {
    use crate::main_merge::{MAIN_COPIES, Merge, conflict_note, into_workspace};
    use crate::push::{Pushed, push_branch};
    use spawn_core::port::{Commit, WorkspaceStore};

    let (base, source, root, mirror, store) = world("conflict-unmarked");
    let agent = "agent_unmarked";
    // The agent deletes notes.txt and changes README.md; main changes
    // notes.txt and deletes README.md.
    let workspace = agent_with_work(&store, &mirror, &source, agent, |directory| {
        std::fs::remove_file(directory.join("notes.txt")).expect("delete");
        std::fs::write(directory.join("README.md"), "one\nmine\nthree\n").expect("write");
        std::fs::write(directory.join("work.txt"), "my work\n").expect("write");
    });
    let main = main_moves(&base, &source, &mirror, |clone| {
        std::fs::write(clone.join("notes.txt"), "notes, revised\n").expect("write");
        std::fs::remove_file(clone.join("README.md")).expect("delete");
    });
    assert_eq!(judged(&mirror, &source, agent), None, "a conflict verdict");

    let merge = into_workspace(&store, &mirror, &root, agent);
    let Merge::Unmarked { files, copied } = &merge else {
        panic!("no markers can show this: {merge:?}");
    };
    assert_eq!(files, &["README.md".to_string(), "notes.txt".to_string()]);
    assert_eq!(copied, &["notes.txt".to_string()]);
    assert!(!store.has_pending_merge(agent));
    let directory = root.join(agent);
    assert_eq!(
        std::fs::read_to_string(directory.join(MAIN_COPIES).join("notes.txt")).expect("copy"),
        "notes, revised\n"
    );
    assert!(
        !directory.join("notes.txt").exists(),
        "the workspace is unchanged"
    );
    let note = conflict_note(&main, &merge);
    assert!(
        note.contains("cannot show")
            && note.contains(".spawn/main")
            && note.contains("make each of notes.txt exactly main's version")
            && note.contains("delete README.md"),
        "{note}"
    );

    // What the note says, done by hand: then it merges and lands.
    std::fs::copy(
        directory.join(MAIN_COPIES).join("notes.txt"),
        directory.join("notes.txt"),
    )
    .expect("copy main's version");
    std::fs::remove_file(directory.join("README.md")).expect("delete");
    assert!(matches!(
        store.commit(&workspace, "resolved").expect("commit"),
        Commit::Created(_)
    ));
    assert_eq!(push_branch(&mirror, &source, agent), Pushed::Done);
    assert!(
        judged(&mirror, &source, agent).is_some(),
        "the resolved work merges with main"
    );
    store.unmount(&workspace).expect("unmount");
    let _ = std::fs::remove_dir_all(&base);
}

/// A task delivered to a worker whose branch does not contain main gets
/// main first: a clean merge is a merge commit (the `base_merged`
/// checkpoint) and the brief names what changed; a conflicting one
/// leaves markers and the brief names the marked files. A worker with
/// no workspace on this host gets nothing.
#[test]
fn a_delivery_to_a_stale_branch_merges_main_first() {
    use crate::main_merge::{Merge, delivery_note, into_workspace};
    use crate::sources::test_git as git;
    use spawn_core::port::WorkspaceStore;

    let (base, source, root, mirror, store) = world("delivery-merge");
    let mirror_path = mirror.to_string_lossy().to_string();
    let clean = agent_with_work(&store, &mirror, &source, "agent_clean", |directory| {
        std::fs::write(directory.join("clean.txt"), "mine\n").expect("write");
    });
    let marked = agent_with_work(&store, &mirror, &source, "agent_marked", |directory| {
        std::fs::write(
            directory.join("README.md"),
            "one\nTWO by the agent\nthree\n",
        )
        .expect("write");
    });
    let main = main_moves(&base, &source, &mirror, |clone| {
        std::fs::write(clone.join("README.md"), "one\nTWO on main\nthree\n").expect("write");
        std::fs::write(clone.join("added.txt"), "from main\n").expect("write");
    });

    let merge = into_workspace(&store, &mirror, &root, "agent_clean");
    let Merge::Merged { commit, files } = &merge else {
        panic!("a clean merge: {merge:?}");
    };
    assert_eq!(files, &["README.md".to_string(), "added.txt".to_string()]);
    assert_eq!(
        git(&[
            "--git-dir",
            &mirror_path,
            "rev-parse",
            "refs/heads/agent_clean"
        ])
        .expect("head"),
        *commit
    );
    assert_eq!(
        git(&[
            "--git-dir",
            &mirror_path,
            "rev-parse",
            &format!("{commit}^2")
        ])
        .expect("second parent"),
        main
    );
    assert!(root.join("agent_clean").join("added.txt").exists());
    let note = delivery_note(&merge, &[]).expect("the brief says so");
    assert!(
        note.contains("added.txt") && note.contains("without a conflict"),
        "{note}"
    );
    assert_eq!(
        into_workspace(&store, &mirror, &root, "agent_clean"),
        Merge::Current,
        "a branch that contains main is left alone"
    );
    // The merge is read back from the branch: a record whose append
    // failed is written at the next merge (causality audit #8).
    assert_eq!(
        crate::main_merge::head_merge(&mirror, "agent_clean"),
        Some(merge.clone())
    );
    assert_eq!(
        crate::main_merge::head_merge(&mirror, "agent_marked"),
        None,
        "a checkpoint is no merge"
    );
    assert_eq!(delivery_note(&Merge::Current, &[]), None);
    // The first delivery was refused before typing (a pane not ready);
    // its retry finds main already in, and still tells what the
    // recorded merge changed (causality audit #7).
    let note = delivery_note(&Merge::Current, &["added.txt".to_string()])
        .expect("the retry's brief says so");
    assert!(
        note.contains("added.txt") && note.contains("without a conflict"),
        "{note}"
    );

    let merge = into_workspace(&store, &mirror, &root, "agent_marked");
    assert_eq!(
        merge,
        Merge::Marked {
            files: vec!["README.md".to_string()]
        }
    );
    assert!(store.has_pending_merge("agent_marked"));
    let note = delivery_note(&merge, &[]).expect("the brief says so");
    assert!(
        note.contains("README.md") && note.contains("conflict markers"),
        "{note}"
    );

    assert_eq!(
        into_workspace(&store, &mirror, &root, "agent_elsewhere"),
        Merge::NotMounted
    );
    store.unmount(&clean).expect("unmount");
    store.unmount(&marked).expect("unmount");
    let _ = std::fs::remove_dir_all(&base);
}

/// History c1 <- c2 <- c3, and d2 forked from c1: `contains(a, b)` when
/// `a` is reachable from `b`.
fn history(ancestor: &str, commit: &str) -> bool {
    let parents = |commit: &str| match commit {
        "c2" | "d2" => Some("c1"),
        "c3" => Some("c2"),
        _ => None,
    };
    let mut at = Some(commit);
    while let Some(current) = at {
        if current == ancestor {
            return true;
        }
        at = parents(current);
    }
    false
}

#[test]
fn a_push_at_the_checkpoint_or_past_it_is_judged() {
    assert_eq!(
        freshness(Some("c2"), Some("c2"), history),
        Freshness::Fresh("c2".to_string()),
        "equal"
    );
    assert_eq!(
        freshness(Some("c2"), Some("c3"), history),
        Freshness::Fresh("c3".to_string()),
        "descendant"
    );
    assert_eq!(
        freshness(None, Some("c1"), history),
        Freshness::Fresh("c1".to_string()),
        "no checkpoint to compare with"
    );
}

#[test]
fn a_push_that_lags_the_checkpoint_is_stale() {
    let behind = freshness(Some("c2"), Some("c1"), history);
    assert!(
        matches!(&behind, Freshness::Stale(reason) if reason.contains("behind")),
        "{behind:?}"
    );
    let diverged = freshness(Some("c2"), Some("d2"), history);
    assert!(
        matches!(&diverged, Freshness::Stale(reason)
            if reason == "the pushed branch (d2) does not contain the latest checkpoint (c2)"),
        "{diverged:?}"
    );
    let unknown = freshness(Some("elsewhere"), Some("c3"), history);
    assert!(
        matches!(unknown, Freshness::Stale(_)),
        "a checkpoint the mirror lacks"
    );
    let absent = freshness(Some("c2"), None, history);
    assert!(
        matches!(&absent, Freshness::Stale(reason) if reason.contains("no spawn/<agent>")),
        "{absent:?}"
    );
}

#[test]
fn a_report_is_judged_against_its_own_checkpoint() {
    assert_eq!(
        report_freshness(Some("c2"), Some("c2"), Some("c2"), history),
        Freshness::Fresh("c2".to_string()),
        "equal"
    );
    assert_eq!(
        report_freshness(Some("c2"), Some("c2"), Some("c3"), history),
        Freshness::Fresh("c3".to_string()),
        "descendant"
    );
    let behind = report_freshness(Some("c2"), None, Some("c1"), history);
    assert!(
        matches!(&behind, Freshness::Stale(reason)
            if reason.contains("behind the report's checkpoint")),
        "the base pushed at the mount, with no checkpoint recorded: {behind:?}"
    );
    let diverged = report_freshness(Some("c2"), Some("c2"), Some("d2"), history);
    assert!(
        matches!(&diverged, Freshness::Stale(reason)
            if reason.contains("does not contain the report's checkpoint")),
        "{diverged:?}"
    );
    let absent = report_freshness(Some("c2"), None, None, history);
    assert!(
        matches!(&absent, Freshness::Stale(reason) if reason.contains("no spawn/<agent>")),
        "no pushed branch: {absent:?}"
    );
    // Past the report's checkpoint but behind a later one: the latest
    // checkpoint still rules.
    let lagging = report_freshness(Some("c1"), Some("c3"), Some("c2"), history);
    assert!(
        matches!(&lagging, Freshness::Stale(reason) if reason.contains("behind")),
        "{lagging:?}"
    );
    // A report that names no commit is judged as before.
    assert_eq!(
        report_freshness(None, None, Some("c1"), history),
        Freshness::Fresh("c1".to_string())
    );
    assert_eq!(
        report_freshness(None, None, None, history),
        Freshness::Nothing
    );
}

#[test]
fn no_checkpoint_and_no_pushed_branch_is_nothing_to_land() {
    assert_eq!(freshness(None, None, history), Freshness::Nothing);
}

/// A scratch repository for the semantic-conflict tests: main M0 with
/// `struct.txt` naming `a b`; a work branch from M0 whose head adds
/// `use.txt` naming `uses`; main then moves to M1 (`struct.txt` names
/// `a b c`, subject `Add field c (objective 1, task 36)`). With
/// `head_on_main`, the work branch starts from M1 instead (its head
/// contains main). `(base, mirror, m0, m1, head)`.
fn semantic_world(
    tag: &str,
    uses: &str,
    head_on_main: bool,
) -> (std::path::PathBuf, String, String, String, String) {
    let git = crate::sources::test_git;
    let base = std::env::temp_dir().join(format!("spawn-semantic-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let seed = base.join("seed");
    std::fs::create_dir_all(&seed).expect("seed directory");
    let seed_path = seed.to_string_lossy().to_string();
    git(&["init", "-q", "--initial-branch=main", &seed_path]).expect("init");
    std::fs::write(seed.join("struct.txt"), "a b\n").expect("write");
    commit_in(&seed_path, "base: Facts {a, b}");
    let m0 = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("m0");
    let work = |from: &str| {
        git(&["-C", &seed_path, "checkout", "-qb", "work", from]).expect("branch");
        std::fs::write(seed.join("use.txt"), format!("{uses}\n")).expect("write");
        commit_in(&seed_path, "spawn checkpoint exec_1");
        let head = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("head");
        git(&["-C", &seed_path, "checkout", "-q", "main"]).expect("main");
        head
    };
    let early = (!head_on_main).then(|| work(&m0));
    std::fs::write(seed.join("struct.txt"), "a b c\n").expect("write");
    commit_in(&seed_path, "Add field c (objective 1, task 36)");
    let m1 = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("m1");
    let head = early.unwrap_or_else(|| work(&m1));
    (base, format!("{seed_path}/.git"), m0, m1, head)
}

/// A fake gate that "builds" a tree: `use.txt`, when there, must name
/// what `struct.txt` names, or it rejects with `candidate tests do not
/// build` and a compiler line. It notes `<base> <candidate>` per run in
/// `<base>/judged`. With `abort_alone`, a candidate that is not a merge
/// (a head judged alone) gets no verdict.
fn semantic_gate(base: &std::path::Path, abort_alone: bool) -> std::path::PathBuf {
    let judged = base.join("judged");
    let abort = if abort_alone {
        "git --git-dir=\"$1\" rev-parse -q --verify \"$3^2\" >/dev/null || { echo 'dies'; exit 3; }\n"
    } else {
        ""
    };
    let script = format!(
        "echo \"$2 $3\" >>'{}'\n{abort}\
             s=$(git --git-dir=\"$1\" show \"$3:struct.txt\")\n\
             u=$(git --git-dir=\"$1\" show \"$3:use.txt\" 2>/dev/null) || u=$s\n\
             if [ \"$s\" = \"$u\" ]; then\n\
               printf '{{\"verdict\":\"accepted\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
               echo '{{\"verdict\":\"accepted\"}}'\n\
             else\n\
               echo 'error[E0063]: missing field `c` in initializer of `Facts`'\n\
               printf '{{\"verdict\":\"rejected\",\"reason\":\"candidate tests do not build\"}}\\n' >\"$SPAWN_GATE_VERDICT\"\n\
               echo '{{\"verdict\":\"rejected\",\"reason\":\"candidate tests do not build\"}}'\n\
             fi\n",
        judged.display()
    );
    let path = base.join("gate.sh");
    std::fs::write(&path, script).expect("gate");
    path
}

/// The runs the semantic gate noted: `(base, candidate)`.
fn semantic_runs(base: &std::path::Path) -> Vec<(String, String)> {
    std::fs::read_to_string(base.join("judged"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            line.split_once(' ')
                .map(|(a, b)| (a.to_string(), b.to_string()))
        })
        .collect()
}

/// Judge the landing of `head` on `main` with the semantic gate, as
/// `judge_pushed` does once the merge is rejected: the rejected merge's
/// verdict (it must be a rejection), the landing candidate and what
/// [`super::rejection`] makes of it.
fn judge_semantic(
    base: &std::path::Path,
    mirror: &str,
    main: &str,
    head: &str,
    abort_alone: bool,
) -> (String, super::Rejection) {
    let gate_path = semantic_gate(base, abort_alone);
    let scenarios = base.join("scenarios");
    std::fs::create_dir_all(&scenarios).expect("scenarios");
    let env: Vec<(String, String)> = std::env::var("PATH")
        .map(|path| vec![("PATH".to_string(), path)])
        .unwrap_or_default();
    let run_gate = |against: &str, judged: &str| {
        crate::gate::judge(
            &gate_path,
            std::path::Path::new(mirror),
            against,
            judged,
            &scenarios,
            None,
            &env,
            std::time::Duration::from_mins(1),
        )
    };
    let candidate = super::landing_candidate(mirror, main, head, "Work (task 48)\n")
        .expect("a candidate")
        .expect("a clean merge");
    let merged = run_gate(main, &candidate).expect("a verdict on the merge");
    assert!(
        !merged.accepted,
        "the merge fails the gate: {}",
        merged.output
    );
    let rejection =
        super::rejection(mirror, main, &candidate, head, &merged, &run_gate).expect("judged");
    (candidate, rejection)
}

/// Objective 16: a head right on its own base, broken only by what
/// landed on main beside it, is `incompatible`: sent back with the
/// gate's output and main's commits, judged by the head's own verdict
/// against its merge-base with main.
#[test]
fn a_rejected_merge_whose_head_passes_alone_is_incompatible() {
    let (base, mirror, m0, m1, head) = semantic_world("incompatible", "a b", false);
    let (candidate, rejection) = judge_semantic(&base, &mirror, &m1, &head, false);
    let super::Rejection::Record(verdict, record) = rejection else {
        panic!("a record: {rejection:?}");
    };
    assert_eq!(verdict, "incompatible", "{record}");
    assert_eq!(record["candidate"], candidate.as_str());
    assert_eq!(record["head"], head.as_str());
    assert_eq!(record["head_base"], m0.as_str());
    assert_eq!(record["head_verdict"], "accepted");
    assert_eq!(record["main_commits"], json!([m1]));
    assert_eq!(record["reason"], "candidate tests do not build");
    assert!(
        record["log"]
            .as_str()
            .is_some_and(|log| log.contains("missing field `c`")),
        "{record}"
    );
    let note = record["note"].as_str().expect("a note");
    let first = note.lines().next().unwrap_or_default();
    assert!(
        first.contains("missing field `c` in initializer of `Facts`") && first.contains(&m1[..12]),
        "the first line says it all: {first}"
    );
    for wanted in [
        "error[E0063]: missing field `c` in initializer of `Facts`",
        &m1[..12],
        "Add field c (objective 1, task 36)",
        "passes the gate on its own base",
        "Main is merged into your workspace with this revision",
    ] {
        assert!(note.contains(wanted), "{wanted:?} in {note}");
    }
    assert_eq!(
        semantic_runs(&base),
        vec![(m1.clone(), candidate), (m0, head)],
        "the merge against main, then the head alone against its base"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// The head fails the gate on its own base too: a rejection, counted,
/// that names the head's verdict.
#[test]
fn a_rejected_merge_whose_head_fails_alone_is_rejected() {
    let (base, mirror, m0, m1, head) = semantic_world("head-fails", "a b z", false);
    let (candidate, rejection) = judge_semantic(&base, &mirror, &m1, &head, false);
    let super::Rejection::Record(verdict, record) = rejection else {
        panic!("a record: {rejection:?}");
    };
    assert_eq!(verdict, "rejected", "{record}");
    assert_eq!(record["head"], head.as_str());
    assert_eq!(record["head_base"], m0.as_str());
    assert_eq!(record["head_verdict"], "rejected");
    assert_eq!(record["candidate"], candidate.as_str());
    assert!(
        record["note"]
            .as_str()
            .is_some_and(|note| note.starts_with("The gate rejected this work")),
        "{record}"
    );
    assert_eq!(semantic_runs(&base).len(), 2, "the head was judged alone");
    let _ = std::fs::remove_dir_all(&base);
}

/// A head that contains main is what the gate judged: a rejection of
/// the candidate is a rejection, with no second run.
#[test]
fn a_rejected_candidate_whose_head_contains_main_is_rejected_without_a_second_run() {
    let (base, mirror, _m0, m1, head) = semantic_world("head-on-main", "a b", true);
    let (candidate, rejection) = judge_semantic(&base, &mirror, &m1, &head, false);
    let super::Rejection::Record(verdict, record) = rejection else {
        panic!("a record: {rejection:?}");
    };
    assert_eq!(verdict, "rejected", "{record}");
    assert!(record.get("head_verdict").is_none(), "{record}");
    assert!(record.get("head").is_none(), "{record}");
    assert_eq!(semantic_runs(&base), vec![(m1, candidate)], "one run");
    let _ = std::fs::remove_dir_all(&base);
}

/// The head's own run gives no verdict: an abort naming the head, never
/// `incompatible` on no evidence.
#[test]
fn a_head_run_without_a_verdict_is_aborted() {
    let (base, mirror, m0, m1, head) = semantic_world("head-aborts", "a b", false);
    let (candidate, rejection) = judge_semantic(&base, &mirror, &m1, &head, true);
    let super::Rejection::Record(verdict, record) = rejection else {
        panic!("a record: {rejection:?}");
    };
    assert_eq!(verdict, "aborted", "{record}");
    assert_eq!(record["head"], head.as_str());
    assert_eq!(record["head_base"], m0.as_str());
    assert_eq!(record["candidate"], candidate.as_str());
    assert!(record.get("head_verdict").is_none(), "{record}");
    assert!(
        record["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains(&head) && reason.contains("gate failed")),
        "{record}"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// The note lists at most [`super::NOTE_COMMITS`] of main's commits and
/// says how many more there are.
#[test]
fn an_incompatible_note_bounds_the_commits_it_lists() {
    let commits: Vec<(String, String)> = (0..super::NOTE_COMMITS + 3)
        .map(|n| {
            (
                format!("{n:012x}{}", "a".repeat(28)),
                format!("landing {n}"),
            )
        })
        .collect();
    let note = super::incompatible_note(
        "candidate tests do not build",
        "error: x\n",
        &"b".repeat(40),
        &"c".repeat(40),
        &commits,
    );
    assert!(note.contains("landing 0\n"), "{note}");
    let first = note.lines().next().unwrap_or_default();
    assert!(first.contains(&format!("{:012x}", 4)), "{first}");
    assert!(!first.contains(&format!("{:012x}", 5)), "{first}");
    assert!(
        first.contains("38 more") && first.ends_with(": error: x"),
        "{first}"
    );
    assert!(
        !note.contains(&format!("landing {}\n", super::NOTE_COMMITS)),
        "{note}"
    );
    assert!(note.contains("and 3 more"), "{note}");
    assert!(note.ends_with("error: x\n"), "{note}");
}

#[test]
fn unresolved_markers_send_work_back_before_the_gate() {
    assert_eq!(markers_note(false, &[]), None);
    let note = markers_note(false, &["README.md".to_string()]).expect("a note");
    assert!(
        note.contains("README.md") && note.contains("do not rewrite"),
        "{note}"
    );
    let note = markers_note(true, &[]).expect("a pending merge is enough");
    assert!(note.contains("the files the merge listed"), "{note}");
}

#[test]
fn a_gate_without_a_believable_verdict_is_recorded_as_an_abort_with_its_log() {
    let record = aborted(&GateFailure::Aborted {
        reason: "the gate ended with signal: 9 (SIGKILL)".to_string(),
        log: "{\"verdict\":\"accepted\"}\n".to_string(),
    });
    assert!(
        record["reason"]
            .as_str()
            .is_some_and(|r| r.contains("SIGKILL"))
    );
    assert!(
        record["log"]
            .as_str()
            .is_some_and(|l| l.contains("accepted"))
    );
    assert!(
        record.get("verdict").is_none(),
        "the caller records it as aborted"
    );
    assert!(
        aborted(&GateFailure::Timeout)["reason"]
            .as_str()
            .is_some_and(|r| r.contains("wall clock"))
    );
}

fn mission(rejections: u32, aborts: u32, floor: u32) -> Mission {
    serde_json::from_value(json!({
        "id": 1, "gate": "/g", "accept": "review",
        "gate_rejections": rejections, "gate_aborts": aborts, "scenario_floor": floor,
    }))
    .expect("a mission")
}

#[test]
fn three_in_a_row_halt_and_the_scenario_floor_holds() {
    assert_eq!(halt_reason(&mission(2, 2, 7), 7), None);
    assert!(halt_reason(&mission(3, 0, 0), 7).is_some_and(|r| r.contains("rejections")));
    assert!(halt_reason(&mission(0, 3, 0), 7).is_some_and(|r| r.contains("aborts")));
    assert!(halt_reason(&mission(0, 0, 7), 6).is_some_and(|r| r.contains("fell")));
    assert_eq!(halt_reason(&mission(0, 0, 0), 0), None, "no floor yet");
    let mut one = mission(3, 0, 0);
    one.gate_rejection_objective = Some(19);
    assert!(
        halt_reason(&one, 7).is_some_and(|r| r.contains("rejections")),
        "no objectives to yield: it halts"
    );
    one.objectives = Some(json!({"backlog": "/b", "resident": "r"}));
    assert_eq!(halt_reason(&one, 7), None, "objective 19 yields instead");
}

/// A halt on the floor names the scenarios the gating that set the
/// floor saw and the directory no longer holds, sorted; nothing without
/// that gating's names.
#[test]
fn a_halt_on_the_floor_names_the_scenarios_gone() {
    let names = |list: &[&str]| list.iter().map(ToString::to_string).collect::<Vec<_>>();
    let mut floor = mission(0, 0, 5);
    assert_eq!(floor_gone(&floor, &names(&["a.sh"])), None, "no names");
    floor.scenario_names = Some(names(&["e.sh", "a.sh", "c.sh", "b.sh", "d.sh"]));
    assert_eq!(
        floor_gone(&floor, &names(&["a.sh", "b.sh", "f.sh"])),
        Some(names(&["c.sh", "d.sh", "e.sh"]))
    );
    assert_eq!(
        floor_reason(&floor, 3).as_deref(),
        Some("the scenarios fell from 5 to 3")
    );
    assert_eq!(floor_reason(&floor, 5), None);
    assert_eq!(
        floor_reason(&mission(3, 0, 5), 3),
        floor_reason(&floor, 3),
        "the same words; halt_reason says the rejections first"
    );
    assert!(halt_reason(&mission(3, 0, 5), 3).is_some_and(|r| r.contains("rejections")));
}

/// The scenario names are the directory's `*.sh` files, sorted; a
/// directory that does not exist holds none.
#[test]
fn the_scenario_names_are_the_gate_directorys_scripts() {
    let base = std::env::temp_dir().join(format!(
        "spawn-scenario-names-{}-{}",
        std::process::id(),
        super::now_ms()
    ));
    std::fs::create_dir_all(base.join("nested.sh")).expect("dir");
    for name in ["b.sh", "a.sh", "notes.md", "c.sh.bak"] {
        std::fs::write(base.join(name), "").expect("file");
    }
    let names = crate::gate::scenario_names(&base).expect("names");
    assert_eq!(names, ["a.sh", "b.sh", "nested.sh"]);
    assert!(
        crate::gate::scenario_names(&base.join("missing"))
            .expect("none")
            .is_empty()
    );
    std::fs::remove_dir_all(&base).expect("clean");
}

const MINUTE: i64 = 60_000;

#[test]
fn no_room_backs_off_doubling_to_a_cap() {
    let minutes: Vec<u64> = (0..=6)
        .map(|waits| room_backoff(waits).as_secs() / 60)
        .collect();
    assert_eq!(minutes, [0, 5, 10, 20, 40, 40, 40]);
    assert_eq!(room_backoff(u32::MAX).as_secs(), 40 * 60, "no overflow");
}

#[test]
fn the_gate_waits_out_its_backoff_from_the_recorded_no_room() {
    let mut waiting = mission(0, 0, 0);
    assert_eq!(
        room_wait(&waiting, 4, 0),
        None,
        "no run of no_room: judge now"
    );
    waiting.gate_room_task = Some(4);
    waiting.gate_room_since_ms = Some(100 * MINUTE);
    waiting.gate_room_last_ms = Some(110 * MINUTE);
    waiting.gate_room_waits = 2;
    assert_eq!(
        room_wait(&waiting, 4, 112 * MINUTE).map(|wait| wait.as_secs()),
        Some(8 * 60)
    );
    assert_eq!(room_wait(&waiting, 4, 120 * MINUTE), None, "backoff over");
    assert_eq!(
        room_wait(&waiting, 5, 112 * MINUTE),
        None,
        "another task does not inherit the backoff"
    );
}

#[test]
fn no_room_names_the_reason_and_the_free_space_and_escalates_once_after_two_hours() {
    let mut waiting = mission(0, 2, 0);
    let first = no_room(
        &waiting,
        4,
        &disk_short(3 * 1_048_576 + 104_858, 10, "/data/tmp"),
        "log",
        0,
    );
    assert_eq!(
        first["reason"],
        "no room to judge now: 3.1 GiB free in /data/tmp (TMPDIR), under \
             SPAWN_GATE_MIN_FREE_GB=10; not a verdict, judged again later"
    );
    assert_eq!(first["free_kb"], 3 * 1_048_576 + 104_858);
    assert_eq!(first["tmpdir"], "/data/tmp");
    assert_eq!(first["min_gb"], 10);
    assert_eq!(first["escalate"], false, "the first of a run");
    waiting.gate_room_task = Some(4);
    waiting.gate_room_since_ms = Some(0);
    let at = |mission: &Mission, minutes: i64| {
        no_room(mission, 4, &disk_short(1, 10, "/t"), "", minutes * MINUTE)["escalate"].clone()
    };
    assert_eq!(
        no_room(&waiting, 5, &disk_short(1, 10, "/t"), "", 500 * MINUTE)["escalate"],
        false,
        "a later task's first no_room starts its own run"
    );
    assert_eq!(
        at(&waiting, 120),
        false,
        "two hours exactly is not more than two"
    );
    assert_eq!(at(&waiting, 121), true);
    waiting.gate_room_escalated = true;
    assert_eq!(at(&waiting, 500), false, "told once per run");
    // And it never counts toward the halt the mission already nears.
    assert_eq!(halt_reason(&waiting, 0), None);
}

/// Objective 018: every `no_room` names its check, what it measured and
/// the threshold, in its fields and in its reason; a disk one keeps the
/// fields and wording it had; the database's names the server, never
/// the URL; and escalation is the run's, whatever the check.
#[test]
fn a_no_room_names_its_check_measure_and_threshold() {
    let waiting = mission(0, 0, 0);
    let disk = no_room(
        &waiting,
        4,
        &disk_short(3 * 1_048_576, 10, "/data/tmp"),
        "log",
        0,
    );
    assert_eq!(
        (&disk["check"], &disk["measured"], &disk["threshold"]),
        (
            &json!("disk"),
            &json!(3 * 1_048_576),
            &json!(10 * 1_048_576)
        )
    );
    assert_eq!(
        (&disk["free_kb"], &disk["min_gb"], &disk["tmpdir"]),
        (&json!(3 * 1_048_576), &json!(10), &json!("/data/tmp"))
    );

    let memory = short_of_room(
        &waiting,
        4,
        &Shortfall::Memory {
            available_mb: 1500,
            min_mb: 2048,
        },
        "memory: 1500 MiB available\n",
        0,
    );
    assert_eq!(
        memory["reason"],
        "no room to judge now: the host's memory has 1500 MiB available (MemAvailable), \
             under SPAWN_GATE_MIN_FREE_MEMORY_MB=2048; not a verdict, judged again later"
    );
    assert_eq!(
        (&memory["check"], &memory["measured"], &memory["threshold"]),
        (&json!("memory"), &json!(1500), &json!(2048))
    );
    assert_eq!(memory["log"], "memory: 1500 MiB available\n");
    assert_eq!(memory["escalate"], false);
    assert!(memory.get("free_kb").is_none() && memory.get("tmpdir").is_none());

    let error = "ERROR: could not resize shared memory segment \"/PostgreSQL.2\" to 8388608 \
                     bytes: No space left on device (53100)";
    let database = Shortfall::Database {
        server: "/var/run/test-pg:5433/spawn_test".to_string(),
        probe: "a parallel query within 10 s".to_string(),
        error: error.to_string(),
    };
    let record = short_of_room(&waiting, 4, &database, "", 0);
    assert_eq!(
        (&record["check"], &record["measured"], &record["threshold"]),
        (
            &json!("database"),
            &json!(error),
            &json!("a parallel query within 10 s")
        )
    );
    assert_eq!(record["server"], "/var/run/test-pg:5433/spawn_test");
    let reason = record["reason"].as_str().expect("a reason");
    assert!(
        reason.starts_with(
            "no room to judge now: the test database (/var/run/test-pg:5433/spawn_test) \
                 failed the probe \"a parallel query within 10 s\": ERROR: could not resize"
        ) && reason.ends_with("not a verdict, judged again later"),
        "{reason}"
    );

    // A scenario run that gave up waiting for a host scenario slot is
    // the host's capacity too: the wait and the budget, the gate's own
    // fact, and no disk or database fields.
    let slot = short_of_room(
        &waiting,
        4,
        &Shortfall::Slot {
            waited_ms: 1_800_000,
            budget: 2,
        },
        "waited 30.0 min for a host scenario slot (all 2 taken)\n",
        0,
    );
    assert_eq!(
        (&slot["check"], &slot["measured"], &slot["threshold"]),
        (&json!("host scenario slot"), &json!(1_800_000), &json!(2))
    );
    assert_eq!(
        slot["reason"],
        "no room to judge now: a scenario run waited 30.0 min for a host scenario slot \
             (SPAWN_GATE_HOST_JOBS=2 in all, shared by every gate on this host); not a verdict, \
             judged again later"
    );
    assert_eq!(
        slot["log"],
        "waited 30.0 min for a host scenario slot (all 2 taken)\n"
    );
    assert!(slot.get("free_kb").is_none() && slot.get("server").is_none());

    // Two hours into the task's run: the first record escalates,
    // whichever check fell short.
    let mut long = mission(0, 0, 0);
    long.gate_room_task = Some(4);
    long.gate_room_since_ms = Some(0);
    assert_eq!(
        short_of_room(&long, 4, &database, "", 121 * MINUTE)["escalate"],
        true
    );
}

#[test]
fn a_gate_without_the_test_database_does_not_wait_for_one_that_has_it() {
    // One shared-server gate allowed: a second one waits, a gate that gets
    // no test database never does, however many share the server.
    assert!(!waits_for_gate(true, 0, 1));
    assert!(waits_for_gate(true, 1, 1));
    assert!(!waits_for_gate(false, 1, 1));
    assert!(!waits_for_gate(false, 8, 1));
    assert!(!waits_for_gate(true, 2, 3));
}

/// The gates that can run at once, which the default jobs divide the
/// cores among: one per source, those on the shared test server within
/// `--gates`.
#[test]
fn gates_at_once_are_one_per_source_within_the_shared_cap() {
    let of = |pairs: &[(&str, bool)], gates| {
        gates_at_once(
            pairs
                .iter()
                .map(|(source, shares)| ((*source).to_string(), *shares)),
            gates,
        )
    };
    // Spawn (shared server) and Supply PA (no test database, two missions).
    assert_eq!(
        of(&[("spawn", true), ("supply", false), ("supply", false)], 1),
        2
    );
    assert_eq!(of(&[("a", true), ("b", true)], 1), 1);
    assert_eq!(of(&[("a", true), ("b", true)], 2), 2);
    assert_eq!(
        of(&[("a", true), ("a", false)], 1),
        1,
        "one gate per source"
    );
    assert_eq!(of(&[], 1), 1);
}

#[test]
fn the_owner_marks_a_mission_whose_gate_needs_no_test_database() {
    let dir = std::env::temp_dir().join(format!("spawn-no-test-db-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    assert!(
        needs_test_database(&dir),
        "the default is the shared server"
    );
    std::fs::write(dir.join(NO_TEST_DATABASE), "").expect("the marker");
    assert!(!needs_test_database(&dir));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_gate_without_the_test_database_gets_no_server_of_its_own_either() {
    for name in [
        "SPAWN_TEST_DATABASE_URL",
        "SPAWN_GATE_PG_IMAGE",
        "SPAWN_GATE_PG_BIN",
    ] {
        assert!(withheld_without_test_database(name), "{name}");
    }
    for name in [
        "PATH",
        "TMPDIR",
        "SPAWN_GATE_JOBS",
        "SPAWN_GATE_MIN_FREE_GB",
    ] {
        assert!(!withheld_without_test_database(name), "{name}");
    }
}

#[test]
fn the_gate_wall_clock_is_a_setting_with_a_default() {
    let minutes = |text: Option<&str>| wall_clock_minutes(text).as_secs() / 60;
    assert_eq!(minutes(None), 120);
    assert_eq!(minutes(Some("90")), 90);
    assert_eq!(minutes(Some(" 240 ")), 240);
    // Nothing that is not a whole number of minutes from 1 up is taken.
    for bad in ["", "0", "-5", "1.5", "two hours"] {
        assert_eq!(minutes(Some(bad)), 120, "{bad:?}");
    }
}

#[test]
fn a_budget_bounds_minutes_and_tool_calls() {
    let budget = json!({"minutes": 45, "tool_calls": 60});
    assert_eq!(over_budget(&budget, 44 * 60_000, 60), None);
    assert!(over_budget(&budget, 46 * 60_000, 0).is_some());
    assert!(over_budget(&budget, 0, 61).is_some());
    assert_eq!(over_budget(&json!({}), i64::MAX, i64::MAX), None);
}

const MIN: i64 = 60_000;
const GAP: i64 = 5 * MIN;

/// A session event at `at_ms` (its id follows its time), with a tool or
/// job id when it has one.
fn event(id: i64, at_ms: i64, name: &str, key: Option<&str>) -> spawn_postgres::ActivityEvent {
    spawn_postgres::ActivityEvent {
        id,
        at_ms,
        name: name.to_string(),
        key: key.map(str::to_string),
    }
}

/// Steady work from `from_ms` to `to_ms`: a tool call every 30 s.
fn steady(from_ms: i64, to_ms: i64, first_id: i64) -> Vec<spawn_postgres::ActivityEvent> {
    let mut events = Vec::new();
    let (mut at, mut id) = (from_ms, first_id);
    while at <= to_ms {
        let key = format!("t{id}");
        events.push(event(id, at, "tool.started", Some(&key)));
        events.push(event(id + 1, at + 5_000, "tool.completed", Some(&key)));
        at += 30_000;
        id += 2;
    }
    events
}

fn never_asked(_: i64, _: i64) -> Result<i64, String> {
    panic!("no gap inside an ended tool is longer than the cap")
}

/// 2026-11-07 23:05:46: the fleet stopped 17:55-23:05, and the
/// integrator's first pass after the restart aborted task 159 (a
/// 90-minute budget, 13 minutes of work before the stop) and task 113
/// (240 minutes; it stopped inside a tool call that never ended). The
/// wall clock (main's rule) aborts both; their active time aborts
/// neither.
#[test]
fn a_fleet_stopped_for_five_hours_puts_no_task_over_its_budget() {
    let delivered = 1_794_044_765_209;
    let first_pass = 1_794_063_946_655;
    let mut events = steady(delivered + 10_000, delivered + 13 * MIN, 1);
    assert!(over_budget(&json!({"minutes": 90}), first_pass - delivered, 0).is_some());
    let active = active_ms(delivered, first_pass, &events, GAP, never_asked).expect("active");
    assert!(active < 13 * MIN + 2 * GAP, "{active}");
    assert_eq!(over_budget(&json!({"minutes": 90}), active, 0), None);

    // Task 113: a tool started before the stop never ends; the restart's
    // first event comes five hours later.
    let last = events.last().expect("events").clone();
    events.push(event(
        last.id + 1,
        last.at_ms + 1_000,
        "tool.started",
        Some("cargo"),
    ));
    events.push(event(
        last.id + 2,
        first_pass - 2_000,
        "command.accepted",
        None,
    ));
    let active = active_ms(delivered, first_pass, &events, GAP, never_asked).expect("active");
    assert!(active < 13 * MIN + 3 * GAP, "{active}");
    assert_eq!(over_budget(&json!({"minutes": 240}), active, 0), None);
}

/// No silence of any length adds more than the cap: an outage alone
/// puts nothing over budget, at delivery, between events, or up to now.
#[test]
fn an_outage_of_any_length_adds_at_most_the_cap() {
    let day = 24 * 60 * MIN;
    assert_eq!(active_ms(0, 30 * day, &[], GAP, never_asked), Ok(GAP));
    let events = [
        event(1, 30 * day, "tool.started", Some("a")),
        event(2, 60 * day, "tool.completed", Some("b")),
    ];
    assert_eq!(
        active_ms(0, 90 * day, &events, GAP, never_asked),
        Ok(3 * GAP)
    );
    // A clock that steps back counts nothing.
    let back = [
        event(1, 10 * MIN, "tool.started", Some("a")),
        event(2, MIN, "turn.stopped", None),
    ];
    assert_eq!(active_ms(0, MIN, &back, GAP, never_asked), Ok(GAP));
}

/// A model API outage (rev-e on 988c85d): Claude Code retries behind a
/// busy pane for 30 minutes and the turn fails; the dispatcher nudges
/// three times an hour and each nudged turn fails too. The readings are
/// not in `events` (the store leaves them out); the failed turns and the
/// commands count nothing, so two hours of it add nothing, and the
/// model's first progress afterwards adds one cap.
#[test]
fn a_model_api_outage_counts_nothing_but_one_cap() {
    let mut events = steady(0, 10 * MIN, 1);
    let mut id = 1_000;
    let mut at = 10 * MIN;
    let mut next = |name: &str, step: i64| {
        id += 1;
        at += step;
        event(id, at, name, None)
    };
    events.push(next("turn.failed", 30 * MIN));
    for _ in 0..6 {
        events.push(next("command.accepted", 20 * MIN));
        events.push(next("command.step", 1_000));
        events.push(next("command.finished", 1_000));
        events.push(next("turn.failed", 9 * MIN));
    }
    let outage_end = events.last().expect("events").at_ms;
    let before = active_ms(0, outage_end, &events, GAP, never_asked).expect("active");
    assert_eq!(before, 10 * MIN + 5_000, "the outage itself counts nothing");
    // A nudge's prompt arriving is no progress: the worker waited for it.
    events.push(next("message.received", 12 * MIN));
    let last = |events: &[spawn_postgres::ActivityEvent]| events.last().expect("events").at_ms;
    let prompted = active_ms(0, last(&events), &events, GAP, never_asked).expect("active");
    assert_eq!(prompted, before, "a prompt arriving counts nothing");
    events.push(next("tool.started", 2 * MIN));
    let after = active_ms(0, last(&events), &events, GAP, never_asked).expect("active");
    assert_eq!(
        after,
        before + 2 * MIN,
        "the model's first progress counts the gap before it, at most a cap"
    );
    assert!(over_budget(&json!({"minutes": 20}), after, 0).is_none());
}

/// The fleet's restart after a stop: the gap ends in the restart's
/// command, which counts nothing, then work goes on and counts.
#[test]
fn a_restart_counts_nothing_for_the_stop() {
    let mut events = steady(0, 5 * MIN, 1);
    let stop = events.last().expect("events").at_ms;
    events.push(event(500, stop + 300 * MIN, "command.accepted", None));
    events.push(event(
        501,
        stop + 300 * MIN + 2_000,
        "session.native_started",
        None,
    ));
    events.extend(steady(stop + 301 * MIN, stop + 311 * MIN, 600));
    let active = active_ms(0, stop + 311 * MIN, &events, GAP, never_asked).expect("active");
    // Five minutes before, ten after, and the first tool call after the
    // engine's start: about a minute.
    assert!(active < 17 * MIN, "{active}");
    assert!(active > 15 * MIN, "{active}");
}

#[test]
fn steady_work_runs_past_the_budget() {
    let events = steady(0, 100 * MIN, 1);
    let active = active_ms(0, 100 * MIN, &events, GAP, never_asked).expect("active");
    // Every gap is short: all of it counts (to its last event, 5 s past now).
    assert_eq!(active, 100 * MIN + 5_000);
    assert!(over_budget(&json!({"minutes": 90}), active, 0).is_some());
}

/// A tool that runs an hour without an event of its session (a build, a
/// gate in the worker) counts while the fleet was alive, at most its
/// gap; a host frozen meanwhile counts only the cap. A job counts the
/// same way.
#[test]
fn a_long_tool_counts_while_the_fleet_was_alive() {
    for (start, end, key) in [
        ("tool.started", "tool.completed", "toolu_1"),
        ("tool.started", "tool.failed", "toolu_1"),
        ("job.started", "job.ended", "job_1"),
    ] {
        let events = [
            event(10, MIN, start, Some(key)),
            event(90, 61 * MIN, end, Some(key)),
        ];
        let mut asked = Vec::new();
        let alive = active_ms(0, 61 * MIN, &events, GAP, |from, to| {
            asked.push((from, to));
            Ok(60 * MIN)
        });
        assert_eq!(alive, Ok(61 * MIN), "{start}");
        assert_eq!(asked, [(10, 90)]);
        let frozen = active_ms(0, 61 * MIN, &events, GAP, |_, _| Ok(MIN));
        assert_eq!(frozen, Ok(MIN + GAP), "{start}");
        // More than the gap is never credited.
        let over = active_ms(0, 61 * MIN, &events, GAP, |_, _| Ok(500 * MIN));
        assert_eq!(over, Ok(61 * MIN), "{start}");
    }
    // A tool still running counts the cap until it ends, and the fleet's
    // answer is never needed for it.
    let running = [event(1, 0, "tool.started", Some("t"))];
    assert_eq!(active_ms(0, 60 * MIN, &running, GAP, never_asked), Ok(GAP));
    // An end with another id, or one without an id, covers nothing.
    let unpaired = [
        event(1, 0, "tool.started", Some("t")),
        event(2, 60 * MIN, "tool.completed", Some("u")),
        event(3, 120 * MIN, "tool.completed", None),
    ];
    assert_eq!(
        active_ms(0, 120 * MIN, &unpaired, GAP, never_asked),
        Ok(2 * GAP)
    );
    // The fleet's error is the pass's.
    let errs = active_ms(
        0,
        61 * MIN,
        &[
            event(1, 0, "tool.started", Some("t")),
            event(2, 60 * MIN, "tool.completed", Some("t")),
        ],
        GAP,
        |_, _| Err("down".to_string()),
    );
    assert_eq!(errs, Err("down".to_string()));
}

#[test]
fn the_budget_gap_is_a_whole_number_of_seconds() {
    assert_eq!(budget_gap(None), BUDGET_GAP);
    assert_eq!(budget_gap(Some(" 90 ")), std::time::Duration::from_secs(90));
    for bad in ["", "0", "-5", "1.5", "2m", "ten"] {
        assert_eq!(budget_gap(Some(bad)), BUDGET_GAP, "{bad:?}");
    }
}

/// Objective 34: a task whose `task.held` names the active hold on its
/// source pushed nothing, so it is no landing in flight: the other
/// missions on the source do not wait for it. A `landing` task without
/// one (not held yet, or held by a hold that ended) still makes them
/// wait, as before.
#[test]
fn landing_elsewhere_ignores_a_task_waiting_behind_the_active_hold() {
    let on_source = |id: i64, hold: Option<i64>, held_by: Option<i64>| {
        let mut found = mission(0, 0, 0);
        found.id = id;
        found.source = Some("/srv/s.git".to_string());
        found.hold = hold;
        found.tasks = serde_json::from_value(json!([
            {"id": id * 10, "agent_id": "agent_a", "session_id": "session_a",
             "state": "landing", "held_by": held_by},
        ]))
        .expect("tasks");
        found
    };
    let waiting = on_source(2, None, None);
    for (hold, held_by, waits) in [
        (Some(7), Some(7), false),
        (Some(7), None, true),
        (Some(9), Some(7), true),
        (None, Some(7), true),
        (None, None, true),
    ] {
        let missions = [waiting.clone(), on_source(1, hold, held_by)];
        assert_eq!(
            super::landing_elsewhere(&missions, &missions[0]),
            waits.then_some((1, 10)),
            "hold {hold:?}, held_by {held_by:?}"
        );
    }
}

#[test]
fn the_ready_task_follows_the_acceptance_rule() {
    let mut gated = mission(0, 0, 0);
    gated.tasks = serde_json::from_value(json!([
        {"id": 1, "agent_id": "agent_a", "session_id": "session_a", "state": "reported"},
        {"id": 2, "agent_id": "agent_b", "session_id": "session_b", "state": "accepted"},
    ]))
    .expect("tasks");
    assert_eq!(gated.ready().map(|task| task.id), Some(2));
    gated.accept = "gate".to_string();
    assert_eq!(gated.ready().map(|task| task.id), Some(1));
    // A report the supervisor accepted anyway is judged too, not left
    // accepted forever.
    gated.tasks.remove(0);
    assert_eq!(gated.ready().map(|task| task.id), Some(2));
}

/// A task the load-sensitive bound parked is not the ready one: the next
/// is (its mission goes on), and it is among the parked ones.
#[test]
fn a_parked_task_is_not_ready_and_the_next_one_is() {
    let mut gated = mission(0, 0, 0);
    gated.accept = "gate".to_string();
    gated.tasks = serde_json::from_value(json!([
        {"id": 1, "agent_id": "agent_a", "session_id": "session_a", "state": "reported",
         "gate_parked_pushed": "a1", "gate_load_sensitive_pushed": "a1",
         "gate_load_sensitive_scenarios": ["099.sh", "046.sh"]},
        {"id": 2, "agent_id": "agent_b", "session_id": "session_b", "state": "reported",
         "gate_load_sensitive_scenarios": null},
    ]))
    .expect("tasks");
    assert_eq!(gated.ready().map(|task| task.id), Some(2));
    assert_eq!(gated.parked().map(|task| task.id).collect::<Vec<_>>(), [1]);
    gated.tasks.remove(1);
    assert_eq!(
        gated.ready().map(|task| task.id),
        None,
        "only the parked one"
    );
}

/// A task as `GATED` reads it, with a brief and a report.
fn task_of(id: i64, objective: Option<u32>, brief: &str, report: Option<&str>) -> super::Task {
    serde_json::from_value(json!({
        "id": id, "agent_id": "agent_a", "session_id": "session_a", "state": "landing",
        "objective": objective, "brief": brief, "report": report,
    }))
    .expect("a task")
}

const REPORT: &str = "## Outcome\ndone\n\n## What changed\nsummary: HARBOUR.txt now holds \
                          kestrel\n\n### Details\nOne file.\n\n## Open issues\nnone\n";

#[test]
fn an_objective_landing_names_its_title_objective_and_task_and_summarises_the_report() {
    let task = task_of(
        42,
        Some(931),
        "Objective 931: Greet the harbour with a kestrel\n\nWrite the file.\n\
             Earlier gate verdicts on this objective, newest first:\n- task 41: rejected\n",
        Some(REPORT),
    );
    let message = super::landing_message(&task, "abc123");
    let mut lines = message.lines();
    assert_eq!(
        lines.next(),
        Some("Greet the harbour with a kestrel (objective 931, task 42)")
    );
    assert_eq!(lines.next(), Some(""));
    let body: String = lines.collect::<Vec<_>>().join("\n");
    assert!(
        body.starts_with("summary: HARBOUR.txt now holds kestrel\n\n### Details\nOne file."),
        "{body}"
    );
    assert!(
        !body.contains("## Outcome") && !body.contains("Open issues"),
        "{body}"
    );
    assert!(!message.contains("task 41"), "no other task: {message}");
    assert!(
        message.ends_with("\n\nSpawn-Task: 42\nSpawn-Objective: 931\nSpawn-Head: abc123\n"),
        "{message}"
    );
    assert!(!spawn_host::names_files_outside_git(&message));
}

#[test]
fn a_landing_without_an_objective_names_its_brief_and_task() {
    let task = task_of(
        5,
        None,
        "\n\n  Fix the parser's error messages  \nMore.\n",
        Some(REPORT),
    );
    let message = super::landing_message(&task, "abc123");
    assert_eq!(
        message.lines().next(),
        Some("Fix the parser's error messages (task 5)")
    );
    assert!(!message.contains("Spawn-Objective"), "{message}");
    assert!(message.contains("\nsummary: HARBOUR.txt now holds kestrel\n"));
    // A long first line is cut; an empty brief still names the task;
    // nothing reads like a checkpoint.
    let long = task_of(6, None, &"word ".repeat(100), None);
    let first = super::landing_message(&long, "h")
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    assert!(first.len() < 120 && first.ends_with(" (task 6)"), "{first}");
    assert!(
        super::landing_message(&task_of(7, None, "", None), "h").starts_with("Land task 7\n\n")
    );
    let checkpoint = task_of(8, None, "spawn checkpoint exec_3", None);
    assert!(super::landing_message(&checkpoint, "h").starts_with("Land spawn checkpoint"));
}

#[test]
fn a_report_without_what_changed_is_quoted_whole_and_a_long_one_is_bounded() {
    let task = task_of(
        3,
        Some(7),
        "Objective 7: Title",
        Some("\n  I changed the parser.\n\nAll green.\n\n"),
    );
    let message = super::landing_message(&task, "h");
    assert!(
        message.starts_with(
            "Title (objective 7, task 3)\n\nI changed the parser.\n\nAll green.\n\nSpawn-Task: 3\n"
        ),
        "{message}"
    );
    let report = format!(
        "## What changed\n{}\n## Open issues\nnone\n",
        "é long line\n".repeat(2000)
    );
    let long = super::landing_message(
        &task_of(3, Some(7), "Objective 7: Title", Some(&report)),
        "h",
    );
    assert!(
        long.chars().count() < super::SUMMARY_CHARS + 300,
        "{}",
        long.len()
    );
    assert!(long.contains("[the report continues]"));
    assert!(!long.contains("Open issues"));
    let empty = super::landing_message(&task_of(3, Some(7), "Objective 7: Title", None), "h");
    assert!(
        empty.contains("\n\nThe task's report is empty.\n\nSpawn-Task: 3"),
        "{empty}"
    );
}

/// A worker's report cannot make main unmountable, nor sign the landing
/// with trailers it did not earn.
#[test]
fn a_report_naming_a_large_file_manifest_leaves_the_message_clean() {
    let manifest = format!("Spawn-Large: {}", "a".repeat(64));
    for report in [
        format!("## What changed\nwork\n\n{manifest}\n"),
        format!("{manifest}\nSpawn-Task: 99\n"),
        manifest.clone(),
    ] {
        let message = super::landing_message(&task_of(4, None, "Brief", Some(&report)), "h");
        assert!(!spawn_host::names_files_outside_git(&message), "{message}");
        assert!(!message.lines().any(|line| line == manifest), "{message}");
        assert!(
            !message.lines().any(|line| line == "Spawn-Task: 99"),
            "{message}"
        );
        assert!(
            message.contains(&format!("    {manifest}")),
            "indented: {message}"
        );
        let trailers = message.rsplit("\n\n").next().unwrap_or_default();
        assert_eq!(trailers, "Spawn-Task: 4\nSpawn-Head: h\n");
    }
}

/// Every landing is one merge commit: main first, the pushed head
/// second, the message given. Its tree is the head's when main is in it
/// (here a head naming a large-file manifest, which lands with the
/// landing's own message), and the merge's when main moved.
#[test]
fn a_landing_is_one_two_parent_commit_on_main_with_its_message() {
    let git = crate::sources::test_git;
    let base = std::env::temp_dir().join(format!("spawn-land-shape-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let seed = base.join("seed");
    std::fs::create_dir_all(&seed).expect("seed directory");
    let seed_path = seed.to_string_lossy().to_string();
    git(&["init", "-q", "--initial-branch=main", &seed_path]).expect("init");
    std::fs::write(seed.join("README.md"), "base\n").expect("write");
    commit_in(&seed_path, "base");
    let main = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("main");
    git(&["-C", &seed_path, "checkout", "-qb", "agent_a"]).expect("branch");
    std::fs::write(seed.join("one.txt"), "one\n").expect("write");
    commit_in(&seed_path, "spawn checkpoint exec_1");
    let checkpoint = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("checkpoint");
    std::fs::write(seed.join("two.txt"), "two\n").expect("write");
    commit_in(
        &seed_path,
        &format!("spawn checkpoint exec_2\n\nSpawn-Large: {}", "b".repeat(64)),
    );
    let head = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("head");
    git(&["-C", &seed_path, "checkout", "-q", "main"]).expect("main");
    std::fs::write(seed.join("other.txt"), "theirs\n").expect("write");
    commit_in(&seed_path, "elsewhere");
    let moved = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("moved");
    let mirror = format!("{seed_path}/.git");
    let rev = |reference: String| {
        git(&["--git-dir", &mirror, "rev-parse", &reference]).expect("rev-parse")
    };
    let task = task_of(
        9,
        Some(12),
        "Objective 12: Count the gulls",
        Some("## What changed\ngulls\n"),
    );
    let message = super::landing_message(&task, &head);

    for (main, merged) in [(main.as_str(), false), (moved.as_str(), true)] {
        let landing = super::landing_candidate(&mirror, main, &head, &message)
            .expect("a landing")
            .expect("no conflict");
        assert_eq!(rev(format!("{landing}^1")), main, "first parent: the base");
        assert_eq!(rev(format!("{landing}^2")), head, "second parent: the head");
        assert!(
            git(&[
                "--git-dir",
                &mirror,
                "rev-parse",
                "--verify",
                "-q",
                &format!("{landing}^3")
            ])
            .is_err(),
            "two parents"
        );
        let said =
            git(&["--git-dir", &mirror, "show", "-s", "--format=%B", &landing]).expect("message");
        assert_eq!(said, message.trim_end());
        assert!(
            said.starts_with("Count the gulls (objective 12, task 9)\n\ngulls\n"),
            "{said}"
        );
        assert!(
            !spawn_host::names_files_outside_git(&said),
            "the manifest stays off main"
        );
        let files = git(&["--git-dir", &mirror, "ls-tree", "--name-only", &landing]).expect("tree");
        if merged {
            assert_eq!(files, "README.md\none.txt\nother.txt\ntwo.txt", "the merge");
        } else {
            assert_eq!(
                rev(format!("{landing}^{{tree}}")),
                rev(format!("{head}^{{tree}}"))
            );
        }
        // One commit on the first-parent line; the checkpoints off it.
        let line = git(&[
            "--git-dir",
            &mirror,
            "rev-list",
            "--first-parent",
            &format!("{main}..{landing}"),
        ])
        .expect("first parents");
        assert_eq!(line, landing);
        git(&[
            "--git-dir",
            &mirror,
            "merge-base",
            "--is-ancestor",
            &checkpoint,
            &landing,
        ])
        .expect("the checkpoints stay reachable");
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// Both landing commits [`super::landing_candidate`] writes (a branch
/// head with the large-file trailer, a merge with a main that moved) are
/// by [`super::LANDING_IDENTITY`], as author and as committer. Run by
/// [`landing_commits_ignore_the_hosts_git_configuration`] under a home
/// without or with a hostile git configuration; alone it checks the same
/// under the caller's.
#[test]
#[ignore = "run by landing_commits_ignore_the_hosts_git_configuration"]
fn landing_commits_are_by_the_integrator() {
    let git = crate::sources::test_git;
    let base = std::env::temp_dir().join(format!(
        "spawn-land-identity-{}-{}",
        std::process::id(),
        std::env::var("SPAWN_TEST_LANDING_CASE").unwrap_or_default()
    ));
    let _ = std::fs::remove_dir_all(&base);
    let seed = base.join("seed");
    std::fs::create_dir_all(&seed).expect("seed directory");
    let seed_path = seed.to_string_lossy().to_string();
    git(&["init", "-q", "--initial-branch=main", &seed_path]).expect("init");
    std::fs::write(seed.join("README.md"), "base\n").expect("write");
    commit_in(&seed_path, "base");
    let main = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("main");
    // A branch head on main that names a large-file manifest.
    git(&["-C", &seed_path, "checkout", "-qb", "agent_large"]).expect("branch");
    std::fs::write(seed.join("work.txt"), "work\n").expect("write");
    let trailer = format!("work\n\nSpawn-Large: {}", "a".repeat(64));
    commit_in(&seed_path, &trailer);
    let large = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("large");
    // A branch beside a main that moved on.
    git(&["-C", &seed_path, "checkout", "-qb", "agent_late", &main]).expect("branch");
    std::fs::write(seed.join("late.txt"), "late\n").expect("write");
    commit_in(&seed_path, "late");
    let late = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("late");
    git(&["-C", &seed_path, "checkout", "-q", "main"]).expect("main");
    std::fs::write(seed.join("other.txt"), "theirs\n").expect("write");
    commit_in(&seed_path, "elsewhere");
    let moved = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("moved");
    let mirror = format!("{seed_path}/.git");

    for (main, branch) in [(main.as_str(), large.as_str()), (&moved, &late)] {
        let message = "Work (task 7)\n\nThe work.\n\nSpawn-Task: 7\n";
        let landing = super::landing_candidate(&mirror, main, branch, message)
            .expect("a landing")
            .expect("no conflict");
        assert_ne!(landing, branch, "a commit of the integrator's own");
        let said =
            git(&["--git-dir", &mirror, "show", "-s", "--format=%B", &landing]).expect("message");
        assert_eq!(said, message.trim_end(), "the integrator's message");
        assert!(!spawn_host::names_files_outside_git(&said));
        let who = git(&[
            "--git-dir",
            &mirror,
            "show",
            "-s",
            "--format=%an <%ae>%n%cn <%ce>",
            &landing,
        ])
        .expect("show");
        let expected = format!(
            "{0} <{1}>",
            super::LANDING_IDENTITY.0,
            super::LANDING_IDENTITY.1
        );
        assert_eq!(who, format!("{expected}\n{expected}"));
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// Under the git configuration of this process: a plain `git commit`
/// with the controller's git (the control: it must not get through
/// when signing is forced), then Spawn's own commits, a checkpoint (the
/// store's `commit-tree`) and a landing (the integrator's), neither of
/// them signed. The seed is made with the test's own git, which that
/// configuration does not reach. Run by
/// [`spawns_own_commits_ignore_forced_signing`].
#[test]
#[ignore = "run by spawns_own_commits_ignore_forced_signing"]
fn own_commits_under_forced_signing() {
    use spawn_core::port::{Commit, WorkspaceStore};
    let git = crate::sources::test_git;
    let base = std::env::temp_dir().join(format!("spawn-forced-signing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let seed = base.join("seed");
    std::fs::create_dir_all(&seed).expect("seed directory");
    let seed_path = seed.to_string_lossy().to_string();
    git(&["init", "-q", "--initial-branch=main", &seed_path]).expect("init");
    std::fs::write(seed.join("README.md"), "base\n").expect("write");
    commit_in(&seed_path, "base");
    let main = git(&["-C", &seed_path, "rev-parse", "HEAD"]).expect("main");

    std::fs::write(seed.join("control.txt"), "control\n").expect("write");
    git(&["-C", &seed_path, "add", "-A"]).expect("add");
    let control = crate::sources::controller_git()
        .args([
            "-C",
            &seed_path,
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
        ])
        .args(["commit", "-qm", "control"])
        .output()
        .expect("git runs");
    assert!(
        !control.status.success(),
        "signing is not forced here: {control:?}"
    );

    let mirror = format!("{seed_path}/.git");
    let store = spawn_host::GitWorkspace::new(&mirror, base.join("work"));
    let branch = spawn_core::id::BranchName::new("agent_signed").expect("valid branch");
    store.ensure_branch(&branch, None).expect("branch");
    let workspace = store.mount(&branch).expect("mount");
    std::fs::write(
        std::path::Path::new(workspace.path()).join("work.txt"),
        "work\n",
    )
    .expect("write");
    let Commit::Created(checkpoint) = store.commit(&workspace, "work").expect("a checkpoint")
    else {
        panic!("a change");
    };
    store.unmount(&workspace).expect("unmount");
    let message = "Work (task 7)\n\nThe work.\n\nSpawn-Task: 7\n";
    let landing = super::landing_candidate(&mirror, &main, checkpoint.as_str(), message)
        .expect("a landing")
        .expect("no conflict");
    for commit in [checkpoint.as_str(), landing.as_str()] {
        let raw = git(&["--git-dir", &mirror, "cat-file", "commit", commit]).expect("commit");
        assert!(!raw.contains("gpgsig"), "{commit} is signed: {raw}");
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// Spawn's own commits never sign, whatever the host's git
/// configuration says: with signing forced through the environment
/// (`GIT_CONFIG_COUNT`: `commit.gpgsign` and `tag.gpgsign` on,
/// `gpg.format=ssh`, a `user.signingkey` that does not exist), a
/// checkpoint commit and a landing commit are both made, unsigned,
/// within a bounded wait, where the controller's `git commit` fails. A
/// host whose `~/.gitconfig` signs with a key no agent holds made every
/// porcelain commit wait forever on `ssh-keygen -Y sign`.
#[test]
fn spawns_own_commits_ignore_forced_signing() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let program = std::env::current_exe().expect("the test binary");
    let missing = std::env::temp_dir().join(format!(
        "spawn-forced-signing-{}-no-such-key.pub",
        std::process::id()
    ));
    let mut child = std::process::Command::new(&program)
        .args([
            "--exact",
            "integrate::tests::own_commits_under_forced_signing",
            "--ignored",
            "--nocapture",
        ])
        .env("GIT_CONFIG_COUNT", "4")
        .env("GIT_CONFIG_KEY_0", "commit.gpgsign")
        .env("GIT_CONFIG_VALUE_0", "true")
        .env("GIT_CONFIG_KEY_1", "tag.gpgsign")
        .env("GIT_CONFIG_VALUE_1", "true")
        .env("GIT_CONFIG_KEY_2", "gpg.format")
        .env("GIT_CONFIG_VALUE_2", "ssh")
        .env("GIT_CONFIG_KEY_3", "user.signingkey")
        .env("GIT_CONFIG_VALUE_3", &missing)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run the test binary");
    let limit = Duration::from_mins(2);
    let started = Instant::now();
    while child
        .try_wait()
        .expect("wait for the test binary")
        .is_none()
    {
        if started.elapsed() >= limit {
            let _ = child.kill();
            let _ = child.wait();
            panic!("a commit under forced signing hung for {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().expect("its output");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("1 passed"),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn landing_commits_ignore_the_hosts_git_configuration() {
    let base = std::env::temp_dir().join(format!("spawn-land-homes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let empty = base.join("empty");
    std::fs::create_dir_all(&empty).expect("an empty home");
    // Another identity, no identity guessed, and every commit signed by
    // a gpg that does not exist.
    let hostile = base.join("hostile");
    std::fs::create_dir_all(&hostile).expect("a hostile home");
    std::fs::write(
        hostile.join(".gitconfig"),
        "[user]\n\tname = Someone Else\n\temail = else@example.com\n\tuseConfigOnly = true\n\
             [commit]\n\tgpgSign = true\n[gpg]\n\tprogram = /nonexistent/gpg\n",
    )
    .expect("write the hostile configuration");
    let program = std::env::current_exe().expect("the test binary");
    for (case, home) in [("empty", &empty), ("hostile", &hostile)] {
        let output = std::process::Command::new(&program)
            .args([
                "--exact",
                "integrate::tests::landing_commits_are_by_the_integrator",
                "--ignored",
                "--nocapture",
            ])
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("SPAWN_TEST_LANDING_CASE", case)
            .env_remove("GIT_CONFIG_GLOBAL")
            .env_remove("GIT_AUTHOR_NAME")
            .env_remove("GIT_AUTHOR_EMAIL")
            .env_remove("GIT_COMMITTER_NAME")
            .env_remove("GIT_COMMITTER_EMAIL")
            .env_remove("EMAIL")
            .output()
            .expect("run the test binary");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "{case} home: {stdout}{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let _ = std::fs::remove_dir_all(&base);
}

/// The controller's commit is read from its directory's name on the
/// scenarios' physical path (through the `controllers/<name>` symlink);
/// main is behind-checked against it: a descendant is ahead, the commit
/// itself, an ancestor or an unrelated commit is not, and scenarios in
/// no controller say nothing. With no commit map, a commit the repository
/// lacks is an unknown commit (objective 178), never behind.
#[test]
fn a_main_ahead_of_its_controller_is_a_descendant_of_its_commit() {
    use super::ControllerLag;
    let git = crate::sources::test_git;
    let base = std::env::temp_dir().join(format!("spawn-controller-commit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let repo = base.join("repo");
    let repo_path = repo.to_string_lossy().to_string();
    std::fs::create_dir_all(&repo).expect("repo");
    git(&["init", "-q", "--initial-branch=main", &repo_path]).expect("init");
    std::fs::write(repo.join("a"), "1\n").expect("write");
    commit_in(&repo_path, "one");
    let c1 = git(&["-C", &repo_path, "rev-parse", "HEAD"]).expect("c1");
    std::fs::write(repo.join("a"), "2\n").expect("write");
    commit_in(&repo_path, "two");
    let c2 = git(&["-C", &repo_path, "rev-parse", "HEAD"]).expect("c2");
    // No commit map: each id as the repository holds it.
    let unmapped = |commit: &str| {
        Ok::<_, String>(crate::commit_now::CommitNow::of(commit, None, || {
            crate::commit_now::held(&repo, commit)
        }))
    };
    let controllers = base.join("controllers");
    std::fs::create_dir_all(controllers.join(format!("spawn-{c1}/rsi/scenarios")))
        .expect("controller");
    std::os::unix::fs::symlink(format!("spawn-{c1}"), controllers.join("spawn")).expect("link");
    let scenarios = controllers.join("spawn/rsi/scenarios");
    assert_eq!(super::controller_commit(&scenarios), Some(c1.clone()));
    assert_eq!(
        super::controller_behind(&repo, &scenarios, &c2, unmapped),
        Ok(ControllerLag::Behind(c1.clone()))
    );
    assert_eq!(
        super::controller_behind(&repo, &scenarios, &c1, unmapped),
        Ok(ControllerLag::No),
        "one commit"
    );
    // The controller ahead of main (an ancestor main): not behind.
    std::fs::create_dir_all(controllers.join(format!("spawn-{c2}/rsi/scenarios")))
        .expect("controller two");
    let ahead = controllers.join(format!("spawn-{c2}/rsi/scenarios"));
    assert_eq!(
        super::controller_behind(&repo, &ahead, &c1, unmapped),
        Ok(ControllerLag::No)
    );
    // A commit this repository does not have (and no map names): an
    // unknown commit, never behind.
    let missing = "e".repeat(40);
    let other = controllers.join(format!("spawn-{missing}/rsi/scenarios"));
    std::fs::create_dir_all(&other).expect("other");
    assert_eq!(
        super::controller_behind(&repo, &other, &c2, unmapped),
        Ok(ControllerLag::Unknown(format!(
            "the controller's commit {missing} is an unknown commit (in neither the commit map \
             nor the source)"
        )))
    );
    // Not a controller: no commit on the path, or not 40 hex digits.
    let plain = base.join("scenarios");
    std::fs::create_dir_all(&plain).expect("plain");
    assert_eq!(super::controller_commit(&plain), None);
    let short = controllers.join("spawn-abc1234/rsi/scenarios");
    std::fs::create_dir_all(&short).expect("short");
    assert_eq!(super::controller_commit(&short), None);
    assert_eq!(
        super::controller_behind(&repo, &plain, &c2, unmapped),
        Ok(ControllerLag::No)
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// Objective 178: a controller built before a history rewrite
/// (`controllers/spawn-<old>`) is compared by the commit map's new id for
/// its commit: behind a descendant of the new id, not behind the new id
/// itself; a dropped commit is unknown with its sentence, and an error of
/// the map's reading is an error, not a verdict.
#[test]
fn a_controller_built_before_the_rewrite_is_compared_by_its_new_commit() {
    use super::ControllerLag;
    use crate::commit_now::CommitNow;
    let git = crate::sources::test_git;
    let base =
        std::env::temp_dir().join(format!("spawn-controller-rewrite-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let repo = base.join("repo");
    let repo_path = repo.to_string_lossy().to_string();
    std::fs::create_dir_all(&repo).expect("repo");
    git(&["init", "-q", "--initial-branch=main", &repo_path]).expect("init");
    std::fs::write(repo.join("a"), "1\n").expect("write");
    commit_in(&repo_path, "new one");
    let new1 = git(&["-C", &repo_path, "rev-parse", "HEAD"]).expect("new1");
    std::fs::write(repo.join("a"), "2\n").expect("write");
    commit_in(&repo_path, "new two");
    let new2 = git(&["-C", &repo_path, "rev-parse", "HEAD"]).expect("new2");
    // The old id is no commit of this repository: the map alone knows it.
    let old = "a".repeat(40);
    let controllers = base.join("controllers");
    let scenarios = controllers.join(format!("spawn-{old}/rsi/scenarios"));
    std::fs::create_dir_all(&scenarios).expect("controller");
    let mapped = |commit: &str| {
        Ok::<_, String>(CommitNow::of(
            commit,
            (commit == old).then_some(&Some(new1.clone())),
            || false,
        ))
    };
    assert_eq!(
        super::controller_behind(&repo, &scenarios, &new2, mapped),
        Ok(ControllerLag::Behind(new1.clone()))
    );
    assert_eq!(
        super::controller_behind(&repo, &scenarios, &new1, mapped),
        Ok(ControllerLag::No)
    );
    let dropped = |commit: &str| Ok::<_, String>(CommitNow::of(commit, Some(&None), || true));
    assert_eq!(
        super::controller_behind(&repo, &scenarios, &new2, dropped),
        Ok(ControllerLag::Unknown(format!(
            "the controller's commit {old} was dropped by the history rewrite (the commit map \
             names no new commit for it)"
        )))
    );
    assert_eq!(
        super::controller_behind(&repo, &scenarios, &new2, |_: &str| Err(
            "the store is gone".to_string()
        )),
        Err("the store is gone".to_string())
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// Gate v2 step 5b: the nightly's red is read from
/// `<root>/nightly/<name>.json`, `<name>` the source's last component
/// without `.git`: its `red_since` while set (a red, or a skipped run
/// during one), with the failed names; nothing for green or flaky (null),
/// no fact, a fact that is no JSON, or a source whose name nightly.sh
/// would refuse.
#[test]
fn the_nightly_red_is_the_facts_red_since() {
    let root = std::env::temp_dir().join(format!("spawn-nightly-fact-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("nightly")).expect("facts");
    let source = "/live/missions/spawn.git";
    let write = |text: &str| std::fs::write(root.join("nightly/spawn.json"), text).expect("fact");
    assert_eq!(super::nightly_red(&root, source), None, "no fact");
    write(r#"{"v":1,"state":"red","failed":["023","025"],"red_since":"abc123"}"#);
    assert_eq!(
        super::nightly_red(&root, source),
        Some(super::NightlyRed {
            since: "abc123".to_string(),
            failed: vec!["023".to_string(), "025".to_string()],
        })
    );
    assert!(
        super::nightly_red(&root, "/elsewhere/spawn").is_some(),
        "the name without .git too"
    );
    write(r#"{"v":1,"state":"skipped","failed":["023"],"red_since":"abc123"}"#);
    assert!(
        super::nightly_red(&root, source).is_some(),
        "a skipped night keeps the red"
    );
    for clear in [
        r#"{"v":1,"state":"green","failed":[],"red_since":null}"#,
        r#"{"v":1,"state":"flaky","failed":[],"red_since":null}"#,
        r#"{"v":1,"state":"red","red_since":""}"#,
        "not json",
    ] {
        write(clear);
        assert_eq!(super::nightly_red(&root, source), None, "{clear}");
    }
    assert_eq!(super::nightly_red(&root, "/live/missions/.git"), None);
    assert_eq!(super::nightly_red(&root, "/live/missions/a b.git"), None);
    let _ = std::fs::remove_dir_all(&root);
}

/// `scenario-owners` under the integrator's root: `<scenario> <owner>` a
/// line, `#` comments and blank lines skipped; a scenario it does not name,
/// or no file at all, has no owner.
#[test]
fn the_scenario_owners_name_each_scenarios_root_fix_owner() {
    let root = std::env::temp_dir().join(format!("spawn-owners-{}", std::process::id()));
    std::fs::create_dir_all(&root).expect("root");
    let named = ["099.sh".to_string(), "046.sh".to_string()];
    assert_eq!(
        super::scenario_owners(&root, &named),
        [("099.sh".to_string(), None), ("046.sh".to_string(), None)],
        "no file"
    );
    std::fs::write(
        root.join(super::SCENARIO_OWNERS),
        "# scenario  owner\n\n099.sh   rev-f  # alerts.rs\n046.sh\n",
    )
    .expect("owners");
    assert_eq!(
        super::scenario_owners(&root, &named),
        [
            ("099.sh".to_string(), Some("rev-f".to_string())),
            ("046.sh".to_string(), None)
        ]
    );
    std::fs::remove_dir_all(&root).expect("clean up");
}
