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
use spawn_host::{GitWorkspace, TmuxHost};

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
    store
        .ensure_branch(&branch(), None)
        .expect("create the branch");
    store
        .ensure_branch(&branch(), None)
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
    store
        .ensure_branch(&branch(), None)
        .expect("create the branch");
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
    store
        .ensure_branch(&branch(), None)
        .expect("create the branch");
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

/// The cap the size tests run under: 1 MiB, so a file over it is small.
const CAP: u64 = 1 << 20;

#[test]
fn a_file_over_the_cap_stays_out_of_the_checkpoint_and_the_rest_goes_in() {
    let _exclusive = exclusive();
    let (repo, root) = repository("large");
    let store = GitWorkspace::new(&repo, &root).with_max_file_bytes(CAP);
    store
        .ensure_branch(&branch(), None)
        .expect("create the branch");
    let workspace = store.mount(&branch()).expect("mount succeeds");
    let directory = PathBuf::from(workspace.path());
    let size = usize::try_from(CAP).expect("fits") + 1;
    fs::write(directory.join("model.bin"), vec![7u8; size]).expect("write a large file");
    fs::write(directory.join("code.rs"), "fn main() {}\n").expect("write a small file");
    assert!(matches!(
        store.commit(&workspace, "work").expect("commit succeeds"),
        Commit::Created(_)
    ));
    let listed = git_out(
        &repo,
        &["ls-tree", "-r", "--name-only", &branch().to_string()],
    );
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

/// A file under the default cap, however large for a source file, is an
/// ordinary blob of the checkpoint: a fresh mount brings it back byte for
/// byte, and an edit to it alone is a new checkpoint.
#[test]
fn a_file_under_the_cap_is_a_blob_across_remounts() {
    let _exclusive = exclusive();
    let (repo, root) = repository("kept");
    let store = GitWorkspace::new(&repo, &root);
    let reference = format!("refs/heads/{}", branch());
    store
        .ensure_branch(&branch(), None)
        .expect("create the branch");
    let workspace = store.mount(&branch()).expect("mount succeeds");
    let directory = PathBuf::from(workspace.path());
    let original = noise(11, 9 << 20);
    fs::create_dir_all(directory.join("data")).expect("create data");
    fs::write(directory.join("data/model.bin"), &original).expect("write a large file");
    fs::write(directory.join("notes.txt"), "text\n").expect("write a small file");
    assert!(matches!(
        store.commit(&workspace, "work").expect("commit succeeds"),
        Commit::Created(_)
    ));
    assert_eq!(
        git_out(
            &repo,
            &["cat-file", "-s", &format!("{reference}:data/model.bin")]
        ),
        (9u64 << 20).to_string()
    );
    store.unmount(&workspace).expect("unmount succeeds");

    let workspace = store.mount(&branch()).expect("mount succeeds");
    let directory = PathBuf::from(workspace.path());
    assert_eq!(
        fs::read(directory.join("data/model.bin")).expect("read"),
        original
    );
    assert_eq!(
        store.commit(&workspace, "again").expect("commit succeeds"),
        Commit::Unchanged,
        "a fresh mount is not a change"
    );
    let mut edited = original;
    edited[1000..2000].copy_from_slice(&noise(12, 1000));
    fs::write(directory.join("data/model.bin"), &edited).expect("edit the large file");
    assert!(matches!(
        store.commit(&workspace, "edit").expect("commit succeeds"),
        Commit::Created(_)
    ));
    store.unmount(&workspace).expect("unmount succeeds");
    let workspace = store.mount(&branch()).expect("mount succeeds");
    assert_eq!(
        fs::read(PathBuf::from(workspace.path()).join("data/model.bin")).expect("read"),
        edited
    );
    store.unmount(&workspace).expect("unmount succeeds");
}

#[test]
fn a_file_over_the_cap_named_like_a_pattern_drops_nothing_else() {
    let _exclusive = exclusive();
    let (repo, root) = repository("pattern");
    let store = GitWorkspace::new(&repo, &root).with_max_file_bytes(CAP);
    store
        .ensure_branch(&branch(), None)
        .expect("create the branch");
    let workspace = store.mount(&branch()).expect("mount succeeds");
    let directory = PathBuf::from(workspace.path());
    let size = usize::try_from(CAP).expect("fits") + 1;
    fs::write(directory.join("a.txt"), "a\n").expect("write");
    fs::write(directory.join("b.txt"), "b\n").expect("write");
    for name in ["*", "*.txt", ":"] {
        fs::write(directory.join(name), vec![1u8; size]).expect("write a large file");
    }
    assert!(matches!(
        store.commit(&workspace, "work").expect("commit succeeds"),
        Commit::Created(_)
    ));
    let listed = git_out(
        &repo,
        &["ls-tree", "-r", "--name-only", &branch().to_string()],
    );
    for kept in ["a.txt", "b.txt", "README.md"] {
        assert!(
            listed.lines().any(|line| line == kept),
            "{kept} kept: {listed}"
        );
    }
    for large in ["*", "*.txt", ":"] {
        assert!(
            !listed.lines().any(|line| line == large),
            "{large} out: {listed}"
        );
    }
    store.unmount(&workspace).expect("unmount succeeds");
}

/// Names of files over the cap that are not plain: bytes that are not
/// UTF-8, in a file and in a directory, and newlines.
const ODD_LARGE_NAMES: [&[u8]; 4] = [
    b"bad\xffname.bin",
    b"dir\xfe/model.bin",
    b"line\nbreak.bin",
    b"new\nline/model.bin",
];

#[test]
fn a_file_over_the_cap_of_any_name_is_left_out_and_told_of() {
    use std::os::unix::ffi::OsStrExt;
    let _exclusive = exclusive();
    let (repo, root) = repository("odd-names");
    let store = GitWorkspace::new(&repo, &root).with_max_file_bytes(CAP);
    store
        .ensure_branch(&branch(), None)
        .expect("create the branch");
    let workspace = store.mount(&branch()).expect("mount succeeds");
    let directory = PathBuf::from(workspace.path());
    let size = usize::try_from(CAP).expect("fits") + 1;
    for name in ODD_LARGE_NAMES {
        let path = directory.join(std::ffi::OsStr::from_bytes(name));
        fs::create_dir_all(path.parent().expect("a parent")).expect("create the directory");
        fs::write(&path, vec![9u8; size]).expect("write a large file");
    }
    fs::write(directory.join("kept.txt"), "kept\n").expect("write");
    assert!(matches!(
        store.commit(&workspace, "work").expect("commit succeeds"),
        Commit::Created(_)
    ));
    let listed = Command::new("git")
        .arg("--git-dir")
        .arg(&repo)
        .args(["ls-tree", "-r", "-z", "--name-only", &branch().to_string()])
        .output()
        .expect("list the branch")
        .stdout;
    let listed: Vec<&[u8]> = listed.split(|byte| *byte == 0).collect();
    for name in ODD_LARGE_NAMES {
        assert!(!listed.contains(&name), "out of git: {listed:?}");
    }
    assert!(listed.contains(&b"kept.txt".as_slice()), "{listed:?}");
    let notices = store.notices(&workspace);
    assert_eq!(notices.len(), ODD_LARGE_NAMES.len());
    assert_eq!(store.notices(&workspace), notices, "untold until carried");
    store.notices_told(&workspace, &notices);
    assert!(store.notices(&workspace).is_empty(), "told once");
    assert_eq!(
        store.commit(&workspace, "again").expect("commit succeeds"),
        Commit::Unchanged
    );
    store.unmount(&workspace).expect("unmount succeeds");
}

#[test]
fn a_commit_records_the_branch_from_the_identifier() {
    let _exclusive = exclusive();
    let (repo, root) = repository("identity");
    let store = GitWorkspace::new(&repo, &root);
    store
        .ensure_branch(&branch(), None)
        .expect("create the branch");
    let workspace = store.mount(&branch()).expect("mount succeeds");
    assert_eq!(workspace.sandbox(), "agent_test");
    assert_eq!(
        PathBuf::from(workspace.path()),
        root.join("agent_test"),
        "the identifier carries the workspace directory"
    );
    store.unmount(&workspace).expect("unmount succeeds");
}

/// A tmux program pinned to a private server, which goes when the test does,
/// even when it panics: no server or socket outlives a test run.
struct IsolatedTmux {
    program: String,
    socket: String,
}

impl Drop for IsolatedTmux {
    fn drop(&mut self) {
        let _ = std::process::Command::new("tmux")
            .args(["-L", &self.socket, "kill-server"])
            .stderr(std::process::Stdio::null())
            .status();
        // tmux leaves the socket file behind; it is this test's own.
        if let Ok(meta) = fs::metadata("/proc/self") {
            use std::os::unix::fs::MetadataExt;
            let dir = std::env::var_os("TMUX_TMPDIR").unwrap_or_else(|| "/tmp".into());
            let _ = fs::remove_file(
                std::path::Path::new(&dir)
                    .join(format!("tmux-{}", meta.uid()))
                    .join(&self.socket),
            );
        }
    }
}

fn isolated_tmux(tag: &str) -> IsolatedTmux {
    let socket = format!("spawn-host-test-{tag}-{}", std::process::id());
    let program = script(
        &format!("fake-tmux-{tag}"),
        &format!("#!/bin/sh\nexec tmux -L {socket} \"$@\"\n"),
    );
    IsolatedTmux { program, socket }
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
    let tmux = isolated_tmux("echo");
    let host = TmuxHost::local(&tmux.program);
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

/// A prompt longer than tmux's message size (16 KiB; an objective's brief
/// can be) is staged and pasted whole: `set-buffer <text>` refused it as
/// "command too long", and the dispatcher retried that refusal forever.
#[test]
fn tmux_stages_a_text_longer_than_its_message_size() {
    let _exclusive = exclusive();
    let tmux = isolated_tmux("long");
    let host = TmuxHost::local(&tmux.program);
    let name = terminal("long");
    start_session(&host, &launch(&name)).expect("start succeeds");

    // Short lines: the test's /bin/cat reads a terminal in canonical mode,
    // which caps one line at 4 KiB; the engine reads raw, as one paste.
    let text = format!(
        "{}LONG-TEXT-END",
        format!("{}\n", "x".repeat(99)).repeat(410)
    );
    host.stage_text(&name, &workspace(), &text)
        .expect("a 40 KiB text stages");
    host.submit_staged(&name, &workspace())
        .expect("the staged text is pasted");
    // The pane echoes 40 KiB: wait for its end to show, at most 10 s.
    let mut pane = String::new();
    for _ in 0..100 {
        pane = host
            .capture_tail(&name, &workspace(), 20)
            .expect("capture succeeds");
        if pane.contains("LONG-TEXT-END") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(pane.contains("LONG-TEXT-END"), "pane tail: {pane}");

    host.terminate(&name, &workspace())
        .expect("terminate succeeds");
}

/// Audit G: a launch's environment (a database login among it) never goes
/// on tmux's command line, which every local user reads in /proc. The pane
/// still gets every value exactly, and the file that carried them is gone.
#[test]
fn a_launchs_environment_never_reaches_a_command_line() {
    let _exclusive = exclusive();
    let tmux = isolated_tmux("environment");
    let argv = scratch().join("environment-argv.log");
    let out = scratch().join("environment-out.txt");
    let _ = fs::remove_file(&argv);
    let _ = fs::remove_file(&out);
    // A tmux that records every command line it is given.
    let logging = script(
        "logging-tmux-environment",
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >>'{}'\nexec '{}' \"$@\"\n",
            argv.display(),
            tmux.program
        ),
    );
    let host = TmuxHost::local(&logging);
    let name = terminal("environment");
    let mut launched = launch(&name);
    let secret = "postgres://spawn_supervisor_x:s3cret-0f9a@127.0.0.1:5432/spawn";
    let awkward = "a b 'quoted' $HOME `x`\nsecond line";
    launched.environment = vec![
        ("SPAWN_DATABASE_URL".to_string(), secret.to_string()),
        ("AWKWARD".to_string(), awkward.to_string()),
    ];
    launched.program = "/bin/sh".to_string();
    launched.arguments = vec![
        "-c".to_string(),
        format!(
            "printf '%s|%s' \"$SPAWN_DATABASE_URL\" \"$AWKWARD\" >'{}'; exec cat",
            out.display()
        ),
    ];
    start_session(&host, &launched).expect("start succeeds");
    let mut seen = String::new();
    for _ in 0..100 {
        seen = fs::read_to_string(&out).unwrap_or_default();
        if !seen.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    host.terminate(&name, &workspace()).expect("clean up");
    assert_eq!(
        seen,
        format!("{secret}|{awkward}"),
        "the pane got every value"
    );
    let lines = fs::read_to_string(&argv).expect("the command lines");
    assert!(
        !lines.contains("s3cret-0f9a"),
        "a secret on tmux's command line"
    );
    assert!(
        !lines.contains("second line"),
        "a value on tmux's command line"
    );
    let directory = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let left: Vec<_> = fs::read_dir(&directory)
        .expect("read the directory")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(&format!("spawn-launch-{}-", name.as_str()))
        })
        .collect();
    assert!(left.is_empty(), "the environment file outlived the start");
}

#[test]
fn tmux_rejects_a_duplicate_terminal() {
    let _exclusive = exclusive();
    let tmux = isolated_tmux("dup");
    let host = TmuxHost::local(&tmux.program);
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
    let tmux = isolated_tmux("exact");
    let host = TmuxHost::local(&tmux.program);
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
    let tmux = isolated_tmux("cwd");
    let host = TmuxHost::local(&tmux.program);
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

/// `write_new` writes on this host: bytes that are not text, and more than
/// a pipe holds, arrive whole, and no temporary file is left.
#[test]
fn files_are_written_new_on_the_host() {
    let directory = scratch().join("write-new");
    let _ = fs::remove_dir_all(&directory);
    let local = TmuxHost::local("tmux");
    contract::terminal_files("local", &local, &directory.to_string_lossy());
    let big: Vec<u8> = (0..=255u8).cycle().take(300_000).collect();
    let path = directory.join("local/big.jsonl");
    assert!(
        local
            .write_new(&workspace(), &path.to_string_lossy(), &big)
            .expect("write")
    );
    assert_eq!(fs::read(&path).expect("read back"), big);
    let left: Vec<_> = fs::read_dir(directory.join("local"))
        .expect("list")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert!(
        !left
            .iter()
            .any(|name| name.to_string_lossy().contains("spawn-write")),
        "no temporary file is left: {left:?}"
    );
    fs::remove_dir_all(&directory).expect("clean up");
}

/// A host on a socket runs every tmux call on that server (`tmux -L`).
#[test]
fn a_host_on_a_socket_runs_tmux_on_that_server() {
    let _exclusive = exclusive();
    let calls = scratch().join("tmux-socket-calls.log");
    let _ = fs::remove_file(&calls);
    let socket = format!("spawn-host-test-socket-{}", std::process::id());
    let _server = IsolatedTmux {
        program: String::new(),
        socket: socket.clone(),
    };
    let logging = script(
        "fake-tmux-socket",
        &format!(
            "#!/bin/sh\nprintf 'tmux %s\\n' \"$*\" >> '{calls}'\nexec tmux \"$@\"\n",
            calls = calls.display()
        ),
    );
    let host = TmuxHost::on_socket(&logging, Some(socket.clone()));
    let name = terminal("socket");
    start_session(&host, &launch(&name)).expect("start on the socket");
    assert!(host.is_alive(&name, &workspace()).expect("is_alive"));
    host.terminate(&name, &workspace()).expect("terminate");
    let log = fs::read_to_string(&calls).expect("read the call log");
    assert_eq!(
        log.lines()
            .filter(|line| !line.starts_with(&format!("tmux -L {socket} ")))
            .count(),
        0,
        "{log}"
    );
    assert!(log.contains("new-session"), "{log}");
}

/// A capture from a tmux that never answers fails within its bound
/// instead of holding the caller.
#[test]
fn a_capture_from_a_tmux_that_never_answers_fails_in_time() {
    let _exclusive = exclusive();
    let hung = script("fake-tmux-hung", "#!/bin/sh\nexec sleep 60\n");
    let host = TmuxHost::local(&hung);
    let started = std::time::Instant::now();
    let captured = host.capture_tail(&terminal("hung"), &workspace(), 20);
    let took = started.elapsed();
    assert!(
        matches!(&captured, Err(TerminalError(text)) if text.contains("did not answer")),
        "{captured:?}"
    );
    assert!(took < std::time::Duration::from_secs(10), "took {took:?}");
}

/// The real adapters obey the same contract as the memory fake the Bridge
/// tests run on.
#[test]
fn the_adapters_pass_the_shared_contract() {
    let _exclusive = exclusive();
    let (repo, root) = repository("contract");
    contract::workspace_store("host_ws", &GitWorkspace::new(&repo, &root));
    let tmux = isolated_tmux("contract");
    let host = TmuxHost::local(&tmux.program);
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
