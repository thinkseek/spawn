//! Public API tests for spawn-host.
//!
//! The git store runs against a real repository in a scratch directory. tmux
//! is real and runs on a private socket so the test never touches an existing
//! server.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

use spawn_core::contract;
use spawn_core::id::{BranchName, TerminalName, WorkspaceId};
use spawn_core::port::{Commit, Launch, TerminalError, TerminalHost, WorkspaceStore};
use spawn_host::{GitWorkspace, PrefixBuilder, TmuxHost, container_name, docker_prefix};

/// Serialize the external programs of this test binary.
///
/// `Command::output` forks, and a fork inherits the write handle of a script
/// another thread is creating. Linux then reports `Text file busy` on exec.
/// One lock at a time removes that race; the tests are fast enough that it
/// costs nothing.
fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Start a session, retrying while the kernel reports `Text file busy`.
///
/// Under load `exec` of a script written moments earlier can fail with
/// `Text file busy` (os error 26). The cause is not established; it is not
/// reproducible in 20 serial or parallel runs. Only that error is retried, so
/// every real start failure still surfaces immediately.
fn start_session(host: &TmuxHost, launch: &Launch) -> Result<(), TerminalError> {
    let mut last = None;
    for _ in 0..20 {
        match host.start(launch) {
            Err(error) if error.0.contains("Text file busy") => {
                last = Some(error);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            other => return other,
        }
    }
    Err(last.expect("one attempt"))
}

/// Directory that holds the fake executables of this test process.
fn scratch() -> PathBuf {
    let path = std::env::temp_dir().join(format!("spawn-host-test-{}", std::process::id()));
    fs::create_dir_all(&path).expect("create scratch directory");
    path
}

/// Write an executable without ever holding the target open.
fn script(name: &str, body: &str) -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let ordinal = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = format!("{name}-{ordinal}");
    let directory = scratch();
    let target = directory.join(&name);
    let temporary = directory.join(format!("{name}.tmp"));
    {
        let mut file = fs::File::create(&temporary).expect("create temporary script");
        file.write_all(body.as_bytes()).expect("write script");
        file.sync_all().expect("flush script");
    }
    let mut permissions = fs::metadata(&temporary).expect("stat script").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&temporary, permissions).expect("mark script executable");
    fs::rename(&temporary, &target).expect("rename script");
    target.to_string_lossy().into_owned()
}

fn git(directory: &PathBuf, arguments: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(["-c", "user.name=spawn-test"])
        .args(["-c", "user.email=spawn@localhost"])
        .args(arguments)
        .status()
        .expect("git is installed");
    assert!(status.success(), "git {arguments:?} failed");
}

/// Repository with one base commit, and an empty workspace root.
fn repository(tag: &str) -> (PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!("spawn-git-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).expect("create scratch repository");
    let repo = base.join("repo");
    fs::create_dir_all(&repo).expect("create repository directory");
    git(&repo, &["init", "-q", "--initial-branch=main"]);
    fs::write(repo.join("README.md"), "base\n").expect("write base file");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "base"]);
    let root = base.join("work");
    fs::create_dir_all(&root).expect("create workspace root");
    (repo.join(".git"), root)
}

fn branch() -> BranchName {
    BranchName::new("agent_test").expect("valid branch")
}

#[test]
fn mount_of_an_unknown_branch_fails() {
    let _exclusive = exclusive();
    let (repo, root) = repository("unknown");
    let store = GitWorkspace::new(&repo, &root);
    assert!(store.mount(&branch()).is_err(), "no commit, no mount");
}

#[test]
fn ensure_branch_is_idempotent_and_mount_extracts_the_base() {
    let _exclusive = exclusive();
    let (repo, root) = repository("mount");
    let store = GitWorkspace::new(&repo, &root);
    store.ensure_branch(&branch()).expect("create the branch");
    store
        .ensure_branch(&branch())
        .expect("creating twice is harmless");
    let workspace = store.mount(&branch()).expect("mount succeeds");
    let extracted = fs::read_to_string(PathBuf::from(workspace.path()).join("README.md"))
        .expect("the base tree was extracted");
    assert_eq!(extracted, "base\n");
    assert!(
        !PathBuf::from(workspace.path()).join(".git").exists(),
        "the engine must not see the repository"
    );
    assert_eq!(workspace.sandbox(), branch().as_str());
    store.unmount(&workspace).expect("unmount succeeds");
    assert!(!PathBuf::from(workspace.path()).exists());
}

#[test]
fn an_untouched_workspace_produces_no_commit() {
    let _exclusive = exclusive();
    let (repo, root) = repository("unchanged");
    let store = GitWorkspace::new(&repo, &root);
    store.ensure_branch(&branch()).expect("create the branch");
    let workspace = store.mount(&branch()).expect("mount succeeds");
    assert!(matches!(
        store.commit(&workspace, "empty").expect("commit succeeds"),
        Commit::Unchanged
    ));
    store.unmount(&workspace).expect("unmount succeeds");
}

#[test]
fn a_changed_workspace_advances_the_branch() {
    let _exclusive = exclusive();
    let (repo, root) = repository("created");
    let store = GitWorkspace::new(&repo, &root);
    store.ensure_branch(&branch()).expect("create the branch");
    let workspace = store.mount(&branch()).expect("mount succeeds");
    fs::write(PathBuf::from(workspace.path()).join("new.txt"), "work\n")
        .expect("write into the workspace");
    match store.commit(&workspace, "work").expect("commit succeeds") {
        Commit::Created(id) => assert_eq!(id.as_str().len(), 40, "{id}"),
        Commit::Unchanged => panic!("a changed workspace must produce a commit"),
    }
    let head = Command::new("git")
        .arg("--git-dir")
        .arg(&repo)
        .args(["show", &format!("{}:new.txt", branch())])
        .output()
        .expect("read the branch");
    assert_eq!(String::from_utf8_lossy(&head.stdout), "work\n");
    store.unmount(&workspace).expect("unmount succeeds");
}

#[test]
fn a_large_file_stays_out_of_the_checkpoint_and_the_rest_goes_in() {
    let _exclusive = exclusive();
    let (repo, root) = repository("large");
    let store = GitWorkspace::new(&repo, &root);
    store.ensure_branch(&branch()).expect("create the branch");
    let workspace = store.mount(&branch()).expect("mount succeeds");
    let directory = PathBuf::from(workspace.path());
    let size = usize::try_from(spawn_host::LARGE_FILE_BYTES).expect("fits") + 1;
    fs::write(directory.join("model.bin"), vec![7u8; size]).expect("write a large file");
    fs::write(directory.join("code.rs"), "fn main() {}\n").expect("write a small file");
    assert!(matches!(
        store.commit(&workspace, "work").expect("commit succeeds"),
        Commit::Created(_)
    ));
    let listed = Command::new("git")
        .arg("--git-dir")
        .arg(&repo)
        .args(["ls-tree", "-r", "--name-only", &branch().to_string()])
        .output()
        .expect("list the branch");
    let listed = String::from_utf8_lossy(&listed.stdout);
    assert!(listed.lines().any(|line| line == "code.rs"), "{listed}");
    assert!(!listed.lines().any(|line| line == "model.bin"), "{listed}");
    // The kept index sees an unchanged tree as unchanged, large file and all.
    assert_eq!(
        store.commit(&workspace, "again").expect("commit succeeds"),
        Commit::Unchanged
    );
    store.unmount(&workspace).expect("unmount succeeds");
}

fn git_out(repo: &PathBuf, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(arguments)
        .output()
        .expect("git is installed");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Deterministic incompressible bytes (xorshift).
fn noise(seed: u64, length: usize) -> Vec<u8> {
    let mut state = seed | 1;
    (0..length)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        })
        .collect()
}

#[test]
fn a_large_file_is_kept_in_the_chunk_store_across_remounts() {
    let _exclusive = exclusive();
    let (repo, root) = repository("kept");
    let store = GitWorkspace::new(&repo, &root);
    let reference = format!("refs/heads/{}", branch());
    store.ensure_branch(&branch()).expect("create the branch");
    let workspace = store.mount(&branch()).expect("mount succeeds");
    let directory = PathBuf::from(workspace.path());
    let size = usize::try_from(spawn_host::LARGE_FILE_BYTES).expect("fits") + 4096;
    let original = noise(11, size);
    fs::create_dir_all(directory.join("data")).expect("create data");
    fs::write(directory.join("data/model.bin"), &original).expect("write a large file");
    fs::write(directory.join("notes.txt"), "text\n").expect("write a small file");
    assert!(matches!(
        store.commit(&workspace, "work").expect("commit succeeds"),
        Commit::Created(_)
    ));
    let trailer = git_out(
        &repo,
        &["log", "-1", "--format=%(trailers:key=Spawn-Large,valueonly)", &reference],
    );
    assert_eq!(trailer.len(), 64, "{trailer}");
    let tree = git_out(&repo, &["rev-parse", &format!("{reference}^{{tree}}")]);
    store.unmount(&workspace).expect("unmount succeeds");
    assert!(root.join(spawn_host::DEFAULT_CHUNK_DIRECTORY).is_dir());

    // A fresh mount brings the large file back, byte for byte.
    let workspace = store.mount(&branch()).expect("mount succeeds");
    let directory = PathBuf::from(workspace.path());
    assert_eq!(fs::read(directory.join("data/model.bin")).expect("read"), original);
    assert_eq!(
        store.commit(&workspace, "again").expect("commit succeeds"),
        Commit::Unchanged,
        "a fresh mount is not a change"
    );

    // A large-file-only edit: same tree, new trailer.
    let mut edited = original.clone();
    edited[1000..2000].copy_from_slice(&noise(12, 1000));
    fs::write(directory.join("data/model.bin"), &edited).expect("edit the large file");
    assert!(matches!(
        store.commit(&workspace, "edit").expect("commit succeeds"),
        Commit::Created(_)
    ));
    assert_eq!(
        git_out(&repo, &["rev-parse", &format!("{reference}^{{tree}}")]),
        tree
    );
    let edited_trailer = git_out(
        &repo,
        &["log", "-1", "--format=%(trailers:key=Spawn-Large,valueonly)", &reference],
    );
    assert_eq!(edited_trailer.len(), 64);
    assert_ne!(edited_trailer, trailer);
    store.unmount(&workspace).expect("unmount succeeds");

    // Every stored object corrupted: the mount fails rather than hand back
    // wrong bytes.
    let mut pending = vec![root.join(spawn_host::DEFAULT_CHUNK_DIRECTORY)];
    while let Some(path) = pending.pop() {
        if path.is_dir() {
            pending.extend(fs::read_dir(&path).expect("read").flatten().map(|e| e.path()));
        } else {
            let mut bytes = fs::read(&path).expect("read object");
            let middle = bytes.len() / 2;
            bytes[middle] ^= 0xff;
            fs::write(&path, bytes).expect("corrupt object");
        }
    }
    match store.mount(&branch()) {
        Err(_) => {}
        Ok(workspace) => {
            let got = fs::read(PathBuf::from(workspace.path()).join("data/model.bin"));
            assert!(got.is_err() || got.expect("read") == edited, "wrong bytes");
        }
    }
}

#[test]
fn a_commit_records_the_branch_from_the_identifier() {
    let _exclusive = exclusive();
    let (repo, root) = repository("identity");
    let store = GitWorkspace::new(&repo, &root);
    store.ensure_branch(&branch()).expect("create the branch");
    let workspace = store.mount(&branch()).expect("mount succeeds");
    assert_eq!(workspace.sandbox(), "agent_test");
    assert_eq!(
        PathBuf::from(workspace.path()),
        root.join("agent_test"),
        "the identifier carries the workspace directory"
    );
    store.unmount(&workspace).expect("unmount succeeds");
}

#[test]
fn the_container_prefix_runs_a_shell_in_the_named_container() {
    let _exclusive = exclusive();
    let prefix = docker_prefix("docker", "agent_test");
    assert_eq!(prefix[0], "docker");
    assert_eq!(prefix[1], "exec");
    assert_eq!(prefix[4], container_name("agent_test"));
    assert_eq!(prefix[6], "-c");
    assert_eq!(prefix.len(), 7, "{prefix:?}");
}

fn isolated_tmux(tag: &str) -> String {
    let socket = format!("spawn-host-test-{tag}-{}", std::process::id());
    script(
        &format!("fake-tmux-{tag}"),
        &format!("#!/bin/sh\nexec tmux -L {socket} \"$@\"\n"),
    )
}

fn terminal(name: &str) -> TerminalName {
    TerminalName::new(format!("spawn-{name}")).expect("valid terminal")
}

fn workspace() -> WorkspaceId {
    WorkspaceId::new("/:/tmp").expect("valid workspace")
}

fn launch(name: &TerminalName) -> Launch {
    Launch {
        terminal: name.clone(),
        workspace: workspace(),
        cwd: "/tmp".to_string(),
        program: "/bin/cat".to_string(),
        arguments: Vec::new(),
        environment: Vec::new(),
    }
}

#[test]
fn tmux_types_into_a_terminal_and_sees_the_echo() {
    let _exclusive = exclusive();
    let host = TmuxHost::local(&isolated_tmux("echo"));
    let name = terminal("echo");
    start_session(&host, &launch(&name)).expect("start succeeds");

    assert!(
        host.is_alive(&name, &workspace())
            .expect("is_alive succeeds")
    );
    host.submit_text(&name, &workspace(), "hello from spawn")
        .expect("submit succeeds");
    let pane = host
        .capture_tail(&name, &workspace(), 20)
        .expect("capture succeeds");
    assert!(pane.contains("hello from spawn"), "pane: {pane}");

    host.terminate(&name, &workspace())
        .expect("terminate succeeds");
    assert!(
        !host
            .is_alive(&name, &workspace())
            .expect("is_alive after terminate"),
        "the session is gone"
    );
}

#[test]
fn tmux_rejects_a_duplicate_terminal() {
    let _exclusive = exclusive();
    let host = TmuxHost::local(&isolated_tmux("dup"));
    let name = terminal("dup");
    start_session(&host, &launch(&name)).expect("start succeeds");
    let error = host
        .start(&launch(&name))
        .expect_err("a duplicate start fails");
    assert!(error.0.contains("already exists"), "{error:?}");
    host.terminate(&name, &workspace()).expect("clean up");
}

#[test]
fn tmux_matches_the_session_name_exactly() {
    let _exclusive = exclusive();
    let host = TmuxHost::local(&isolated_tmux("exact"));
    let first = terminal("a");
    let second = terminal("ab");
    for name in [&first, &second] {
        start_session(&host, &launch(name)).expect("start succeeds");
    }
    host.terminate(&first, &workspace())
        .expect("terminate the short name");
    assert!(
        host.is_alive(&second, &workspace())
            .expect("is_alive succeeds"),
        "terminating spawn-a must not kill spawn-ab"
    );
    host.terminate(&second, &workspace()).expect("clean up");
}

#[test]
fn tmux_starts_in_the_launch_directory() {
    let _exclusive = exclusive();
    let host = TmuxHost::local(&isolated_tmux("cwd"));
    let name = terminal("cwd");
    let mut started = launch(&name);
    started.program = "/bin/sh".to_string();
    started.arguments = vec!["-c".to_string(), "pwd; exec cat".to_string()];
    started.cwd = "/tmp".to_string();
    start_session(&host, &started).expect("start succeeds");
    // The shell prints after tmux returns; poll instead of racing it.
    let mut pane = String::new();
    for _ in 0..50 {
        pane = host
            .capture_tail(&name, &workspace(), 20)
            .expect("capture succeeds");
        if pane.contains("/tmp") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    assert!(pane.contains("/tmp"), "pane: {pane}");
    host.terminate(&name, &workspace()).expect("clean up");
}

#[test]
fn tmux_calls_go_through_the_container_prefix() {
    let _exclusive = exclusive();
    let calls = scratch().join("tmux-calls.log");
    let _ = fs::remove_file(&calls);
    let socket = format!("spawn-host-test-prefix-{}", std::process::id());
    let inner = script(
        "fake-tmux-prefix",
        &format!(
            "#!/bin/sh\nprintf 'tmux %s\\n' \"$*\" >> '{calls}'\nexec tmux -L {socket} \"$@\"\n",
            calls = calls.display()
        ),
    );
    let prefix_program = script(
        "fake-docker-prefix",
        &format!(
            "#!/bin/sh\nprintf 'docker %s\\n' \"$*\" >> '{calls}'\nfor last; do :; done\nexec /bin/sh -c \"$last\"\n",
            calls = calls.display()
        ),
    );
    let builder: std::sync::Arc<PrefixBuilder> = {
        let words = docker_prefix(&prefix_program, "agent_test");
        std::sync::Arc::new(move |_: &WorkspaceId| words.clone())
    };
    let host = TmuxHost::in_workspaces(&inner, builder.clone(), None);
    let name = terminal("prefix");
    start_session(&host, &launch(&name)).expect("start through the prefix succeeds");
    let later = TmuxHost::in_workspaces(&inner, builder, None);
    assert!(
        later
            .is_alive(&name, &workspace())
            .expect("is_alive in a new host")
    );
    later
        .terminate(&name, &workspace())
        .expect("terminate through the prefix");
    let log = fs::read_to_string(&calls).expect("read the call log");
    assert!(
        log.contains("exec -i -t spawn-exec-agent_test sh -c"),
        "{log}"
    );
    assert!(log.contains("new-session"), "{log}");
    assert!(log.contains("-c /tmp"), "{log}");
}

/// The real adapters obey the same contract as the memory fake the Bridge
/// tests run on.
#[test]
fn the_adapters_pass_the_shared_contract() {
    let _exclusive = exclusive();
    let (repo, root) = repository("contract");
    contract::workspace_store("host_ws", &GitWorkspace::new(&repo, &root));
    let host = TmuxHost::local(&isolated_tmux("contract"));
    // A second session keeps the private server up: killing its last session
    // shuts it down, and a `has-session` racing that shutdown fails with
    // "server exited unexpectedly" instead of reporting the terminal gone.
    let holder = terminal("contract-holder");
    start_session(&host, &launch(&holder)).expect("start the holder");
    contract::terminal("host_tty", &host);
    host.terminate(&holder, &workspace()).expect("clean up");
}

/// tmux must exist: a silent skip would hide a broken host.
#[test]
fn tmux_is_installed() {
    let _exclusive = exclusive();
    let status = Command::new("tmux")
        .arg("-V")
        .status()
        .expect("tmux is installed on the test machine");
    assert!(status.success());
}
