//! Public API tests for spawn-host.
//!
//! LayerFS is replaced by small shell scripts that reproduce its exit codes,
//! because the adapter's contract is exactly those three codes. tmux is real
//! and runs on a private socket so the test never touches an existing server.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

use spawn_core::id::{BranchName, TerminalName, WorkspaceId};
use spawn_core::port::{Commit, Launch, TerminalHost, WorkspaceError, WorkspaceStore};
use spawn_host::{LayerFsCli, PrefixBuilder, TmuxHost};

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

/// Directory that holds the fake executables of this test process.
fn scratch() -> PathBuf {
    let path = std::env::temp_dir().join(format!("spawn-host-test-{}", std::process::id()));
    fs::create_dir_all(&path).expect("create scratch directory");
    path
}

/// Write an executable without ever holding the target open, so a concurrent
/// `exec` cannot see a half-written file. Every call gets its own path: two
/// tests that share a name would otherwise race on the same inode and fail
/// with `Text file busy`.
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

/// A CLI that answers every call with one fixed reply and exit code.
fn fixed(name: &str, code: i32, stdout: &str, stderr: &str) -> String {
    script(
        name,
        &format!(
            "#!/bin/sh\nprintf '%s' '{}'\nprintf '%s' '{}' >&2\nexit {code}\n",
            stdout.replace('\'', "'\\''"),
            stderr.replace('\'', "'\\''")
        ),
    )
}

/// A CLI that answers by subcommand: `sandbox create`, `workspace mount`,
/// `workspace commit` and `branch fork` each get their own reply.
fn scripted(name: &str, commit_code: i32, commit_out: &str) -> String {
    script(
        name,
        &format!(
            r#"#!/bin/sh
case "$*" in
  *"sandbox create"*) printf '%s' '{{"sandbox":"9"}}' ;;
  *"workspace mount"*) printf '%s' '{{"ws":"9:/workspaces/9"}}' ;;
  *"workspace commit"*) printf '%s' '{commit_out}'; exit {commit_code} ;;
  *"branch fork"*) exit 0 ;;
  *) exit 0 ;;
esac
"#
        ),
    )
}

fn branch() -> BranchName {
    BranchName::new("cart").expect("valid branch")
}

fn workspace() -> WorkspaceId {
    WorkspaceId::new("9:/workspaces/9").expect("valid workspace")
}

#[test]
fn mount_reads_the_sandbox_and_the_workspace_from_the_cli() {
    let _exclusive = exclusive();
    let cli = LayerFsCli::new(
        &scripted("fake-layerfs-ok", 0, r#"{"commit":{"id":"c_9"}}"#),
        "shop",
    );
    let mounted = cli.mount(&branch()).expect("mount succeeds");
    assert_eq!(mounted.as_str(), "9:/workspaces/9");
    assert_eq!(cli.project(), "shop");
}

#[test]
fn commit_without_changes_is_unchanged() {
    let _exclusive = exclusive();
    let cli = LayerFsCli::new(
        &fixed(
            "fake-layerfs-nocommit",
            125,
            "",
            r#"{"code":"NoCommit","message":"nothing to commit"}"#,
        ),
        "shop",
    );
    assert!(matches!(
        cli.commit(&workspace(), "empty").expect("commit succeeds"),
        Commit::Unchanged
    ));
}

#[test]
fn commit_with_changes_returns_the_commit_id() {
    let _exclusive = exclusive();
    let cli = LayerFsCli::new(
        &scripted("fake-layerfs-ok-2", 0, r#"{"commit":{"id":"c_9"}}"#),
        "shop",
    );
    match cli.commit(&workspace(), "work").expect("commit succeeds") {
        Commit::Created(id) => assert_eq!(id.as_str(), "c_9"),
        Commit::Unchanged => panic!("a created commit must not be reported as unchanged"),
    }
}

#[test]
fn up_to_date_flag_wins_over_a_commit_id() {
    let _exclusive = exclusive();
    let cli = LayerFsCli::new(
        &fixed(
            "fake-layerfs-uptodate",
            0,
            r#"{"upToDate":true,"commit":"c_9"}"#,
            "",
        ),
        "shop",
    );
    assert!(matches!(
        cli.commit(&workspace(), "empty").expect("commit succeeds"),
        Commit::Unchanged
    ));
}

#[test]
fn usage_error_is_unavailable() {
    let _exclusive = exclusive();
    let cli = LayerFsCli::new(&fixed("fake-layerfs-usage", 2, "", "bad arguments"), "shop");
    match cli.mount(&branch()).expect_err("a usage error fails") {
        WorkspaceError::Unavailable(message) => {
            assert!(message.contains("usage error"), "{message}");
            assert!(message.contains("bad arguments"), "{message}");
        }
        WorkspaceError::Refused { code, .. } => panic!("unexpected refusal: {code}"),
    }
}

#[test]
fn refusal_carries_the_code_and_the_message() {
    let _exclusive = exclusive();
    let cli = LayerFsCli::new(
        &fixed(
            "fake-layerfs-refused",
            125,
            "",
            r#"{"code":"InvalidInput","message":"colon must be followed by /"}"#,
        ),
        "shop",
    );
    match cli.mount(&branch()).expect_err("a refusal fails") {
        WorkspaceError::Refused { code, message } => {
            assert_eq!(code, "InvalidInput");
            assert!(message.contains("colon"), "{message}");
        }
        WorkspaceError::Unavailable(message) => panic!("unexpected unavailability: {message}"),
    }
}

#[test]
fn ensure_branch_accepts_an_existing_name() {
    let _exclusive = exclusive();
    let cli = LayerFsCli::new(
        &fixed(
            "fake-layerfs-exists",
            125,
            "",
            r#"{"code":"NameTaken","message":"branch exists"}"#,
        ),
        "shop",
    );
    cli.ensure_branch(&branch())
        .expect("an existing branch is the desired state");
}

#[test]
fn a_missing_program_is_unavailable() {
    let _exclusive = exclusive();
    let cli = LayerFsCli::new("/nonexistent/layerfs", "shop");
    match cli.mount(&branch()).expect_err("a missing program fails") {
        WorkspaceError::Unavailable(message) => assert!(message.contains("layerfs"), "{message}"),
        WorkspaceError::Refused { code, .. } => panic!("unexpected refusal: {code}"),
    }
}

#[test]
fn exec_prefix_marks_the_stream_and_the_project() {
    let _exclusive = exclusive();
    let cli = LayerFsCli::new("layerfs", "shop");
    let prefix = cli.exec_prefix(&workspace());
    assert_eq!(prefix[0], "layerfs");
    assert_eq!(prefix[1], "--project");
    assert_eq!(prefix[3], "workspace");
    assert_eq!(prefix[4], "exec");
    assert_eq!(prefix[6], "--stream");
    assert_eq!(prefix[7], "--");
    assert_eq!(prefix.len(), 8, "{prefix:?}");
}

/// tmux on a socket private to one test, so no shared server, no user
/// sessions and no name collision between parallel tests.
fn isolated_tmux(tag: &str) -> String {
    let socket = format!("spawn-host-test-{tag}-{}", std::process::id());
    script(
        &format!("fake-tmux-{tag}"),
        &format!("#!/bin/sh\nexec /usr/bin/tmux -L {socket} \"$@\"\n"),
    )
}

fn terminal(name: &str) -> TerminalName {
    TerminalName::new(format!("spawn-{name}")).expect("valid terminal")
}

#[test]
fn tmux_types_into_a_terminal_and_sees_the_echo() {
    let _exclusive = exclusive();
    let host = TmuxHost::local(&isolated_tmux("echo"));
    let name = terminal("echo");
    host.start(&Launch {
        terminal: name.clone(),
        workspace: workspace(),
        program: "/bin/cat".to_string(),
        arguments: Vec::new(),
        environment: Vec::new(),
    })
    .expect("start succeeds");

    assert!(host.is_alive(&name).expect("is_alive succeeds"));
    host.submit_text(&name, "hello from spawn")
        .expect("submit succeeds");
    let pane = host.capture_tail(&name, 20).expect("capture succeeds");
    assert!(pane.contains("hello from spawn"), "pane: {pane}");

    host.terminate(&name).expect("terminate succeeds");
    assert!(
        !host.is_alive(&name).expect("is_alive after terminate"),
        "the session is gone"
    );
}

#[test]
fn tmux_rejects_a_duplicate_terminal() {
    let _exclusive = exclusive();
    let host = TmuxHost::local(&isolated_tmux("dup"));
    let name = terminal("dup");
    host.start(&Launch {
        terminal: name.clone(),
        workspace: workspace(),
        program: "/bin/cat".to_string(),
        arguments: Vec::new(),
        environment: Vec::new(),
    })
    .expect("first start succeeds");
    let error = host
        .start(&Launch {
            terminal: name.clone(),
            workspace: workspace(),
            program: "/bin/cat".to_string(),
            arguments: Vec::new(),
            environment: Vec::new(),
        })
        .expect_err("a duplicate start fails");
    assert!(error.0.contains("already exists"), "{error:?}");
    host.terminate(&name).expect("clean up");
}

#[test]
fn tmux_matches_the_session_name_exactly() {
    let _exclusive = exclusive();
    let host = TmuxHost::local(&isolated_tmux("exact"));
    let first = terminal("a");
    let second = terminal("ab");
    for name in [&first, &second] {
        host.start(&Launch {
            terminal: name.clone(),
            workspace: workspace(),
            program: "/bin/cat".to_string(),
            arguments: Vec::new(),
            environment: Vec::new(),
        })
        .expect("start succeeds");
    }
    host.terminate(&first).expect("terminate the short name");
    assert!(
        host.is_alive(&second).expect("is_alive succeeds"),
        "terminating spawn-a must not kill spawn-ab"
    );
    host.terminate(&second).expect("clean up");
}

#[test]
fn tmux_calls_go_through_the_workspace_prefix() {
    let _exclusive = exclusive();
    let calls = scratch().join("tmux-calls.log");
    let _ = fs::remove_file(&calls);
    let socket = format!("spawn-host-test-prefix-{}", std::process::id());
    // Inside the sandbox tmux is tmux; it only logs the call.
    let inner = script(
        "fake-tmux-prefix",
        &format!(
            "#!/bin/sh\nprintf 'tmux %s\\n' \"$*\" >> '{calls}'\nexec /usr/bin/tmux -L {socket} \"$@\"\n",
            calls = calls.display()
        ),
    );
    // The prefix program logs its own argv and runs the command it is given.
    let prefix_program = script(
        "fake-layerfs-prefix",
        &format!(
            "#!/bin/sh\nprintf 'layerfs %s\\n' \"$*\" >> '{calls}'\nfor last; do :; done\nexec /bin/sh -c \"$last\"\n",
            calls = calls.display()
        ),
    );
    let builder: std::sync::Arc<PrefixBuilder> = {
        let cli = LayerFsCli::new(&prefix_program, "shop");
        let prefix = cli.exec_prefix(&workspace());
        std::sync::Arc::new(move |_: &WorkspaceId| prefix.clone())
    };
    let host = TmuxHost::in_workspaces(&inner, builder, None);
    let name = terminal("prefix");
    host.start(&Launch {
        terminal: name.clone(),
        workspace: workspace(),
        program: "/bin/cat".to_string(),
        arguments: Vec::new(),
        environment: Vec::new(),
    })
    .expect("start through the prefix succeeds");
    host.terminate(&name).expect("terminate through the prefix");
    let log = fs::read_to_string(&calls).expect("read the call log");
    assert!(
        log.contains("layerfs --project shop workspace exec 9:/workspaces/9 --stream --"),
        "{log}"
    );
    assert!(log.contains("tmux new-session"), "{log}");
    assert!(log.contains("new-session"), "{log}");
}

#[test]
fn a_killed_process_is_unavailable() {
    let _exclusive = exclusive();
    // `layerfs` exits 125 without JSON, so the adapter reports it as
    // unstructured unavailability instead of inventing a refusal code.
    let cli = LayerFsCli::new(&fixed("fake-layerfs-bare", 125, "", "boom"), "shop");
    match cli.mount(&branch()).expect_err("bare failure") {
        WorkspaceError::Unavailable(message) => assert!(message.contains("boom"), "{message}"),
        WorkspaceError::Refused { code, .. } => panic!("unexpected refusal: {code}"),
    }
}

#[test]
fn unmount_closes_the_sandbox() {
    let _exclusive = exclusive();
    let cli = LayerFsCli::new(&scripted("fake-layerfs-unmount", 0, ""), "shop");
    cli.unmount(&workspace()).expect("unmount succeeds");
}

/// tmux must exist: a silent skip would hide a broken host.
#[test]
fn tmux_is_installed() {
    let _exclusive = exclusive();
    let status = Command::new("/usr/bin/tmux")
        .arg("-V")
        .status()
        .expect("tmux is installed on the test machine");
    assert!(status.success());
}
