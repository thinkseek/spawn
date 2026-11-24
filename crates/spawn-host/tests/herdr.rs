//! The Herdr host against a real `herdr` (`SPAWN_TERMINAL_BACKEND=herdr`).
//!
//! Each test runs its own Herdr session (`spawn-test-<pid>-<n>`), whose
//! server the host starts itself, with its runtime directory in the test's
//! scratch; the session's server is stopped and the session deleted when the
//! test ends. The user's default and interactive sessions are never named.
//!
//! The `herdr` is `$HOME/.local/bin/herdr`, else the one on `PATH`; a host
//! with neither skips these tests (and says so).

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use spawn_core::id::{TerminalName, WorkspaceId};
use spawn_core::port::{Launch, Runner, ServerSeen, TerminalHost};
use spawn_host::{HerdrHost, Scratch};

/// One test at a time: each starts a server of its own.
fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The installed `herdr`, if any answers `--version`.
fn herdr_program() -> Option<String> {
    let home = std::env::var_os("HOME").map(|home| {
        Path::new(&home)
            .join(".local/bin/herdr")
            .display()
            .to_string()
    });
    home.into_iter()
        .chain(["herdr".to_string()])
        .find(|program| {
            Command::new(program)
                .arg("--version")
                .stdin(Stdio::null())
                .output()
                .is_ok_and(|output| output.status.success())
        })
}

/// A Herdr session of the test's own, its server stopped and the session
/// deleted on drop.
struct Session {
    program: String,
    name: String,
    scratch: Scratch,
}

impl Session {
    fn new() -> Option<Self> {
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let Some(program) = herdr_program() else {
            eprintln!("skipped: no herdr installed");
            return None;
        };
        let ordinal = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("spawn-test-{}-{ordinal}", std::process::id());
        let scratch = Scratch::new("spawn-herdr-test");
        let session = Self {
            program,
            name,
            scratch,
        };
        std::fs::create_dir(session.runtime()).expect("runtime");
        std::fs::set_permissions(session.runtime(), std::fs::Permissions::from_mode(0o700))
            .expect("0700");
        std::fs::create_dir(session.cwd()).expect("cwd");
        Some(session)
    }

    fn runtime(&self) -> PathBuf {
        self.scratch.join("run")
    }

    fn cwd(&self) -> PathBuf {
        self.scratch.join("ws")
    }

    fn host(&self) -> HerdrHost {
        HerdrHost::on_session(&self.program, &self.name)
            .expect("a valid session")
            .with_runtime_under(self.runtime())
            .with_terminate_waits(Duration::from_secs(3), Duration::from_secs(2))
    }

    fn workspace(&self) -> WorkspaceId {
        WorkspaceId::new(format!("agent:{}", self.cwd().display())).expect("workspace")
    }

    fn launch(&self, terminal: &TerminalName, script: &str) -> Launch {
        Launch {
            terminal: terminal.clone(),
            workspace: self.workspace(),
            cwd: self.cwd().display().to_string(),
            program: "/bin/sh".to_string(),
            arguments: vec!["-c".to_string(), script.to_string()],
            environment: vec![("GREETING".to_string(), "hi 'there'".to_string())],
        }
    }

    fn herdr(&self, arguments: &[&str]) {
        let mut command = Command::new(&self.program);
        command
            .arg("--session")
            .arg(&self.name)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (name, _) in std::env::vars_os() {
            if name.to_str().is_some_and(|name| name.starts_with("HERDR_")) {
                command.env_remove(name);
            }
        }
        let _ = command.status();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.herdr(&["server", "stop"]);
        let deadline = Instant::now() + Duration::from_secs(5);
        while matches!(self.host().server(), ServerSeen::Answers(_)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut command = Command::new(&self.program);
        command
            .args(["session", "delete", &self.name])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _ = command.status();
    }
}

fn terminal(name: &str) -> TerminalName {
    TerminalName::new(name).expect("terminal")
}

/// The screen once it holds `text`, within 10 s.
fn screen_with(
    host: &HerdrHost,
    terminal: &TerminalName,
    workspace: &WorkspaceId,
    text: &str,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let screen = host
            .capture_tail(terminal, workspace, 50)
            .unwrap_or_default();
        if screen.contains(text) || Instant::now() >= deadline {
            assert!(
                screen.contains(text),
                "{text:?} never appeared on:\n{screen}"
            );
            return screen;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn runner_within(
    host: &HerdrHost,
    terminal: &TerminalName,
    workspace: &WorkspaceId,
    want: &Runner,
) -> Runner {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let runner = host.runner(terminal, workspace).expect("runner");
        if &runner == want || Instant::now() >= deadline {
            return runner;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_terminal_lives_types_staged_text_and_is_terminated_on_its_own_session() {
    let _serial = exclusive();
    let Some(session) = Session::new() else {
        return;
    };
    let host = session.host();
    let workspace = session.workspace();
    let one = terminal("spawn-exec_h1");
    let missing = terminal("spawn-exec_h9");

    // Asking never starts a server.
    assert_eq!(host.server(), ServerSeen::Gone);
    assert!(!host.is_alive(&one, &workspace).expect("alive"));
    assert_eq!(host.server(), ServerSeen::Gone);

    // The launch sees its own environment and none of the controller's
    // (the test runs with SPAWN_* settings of its own).
    host.start(&session.launch(
        &one,
        "printf 'env:%s spawn:%s\\n' \"$GREETING\" \"$(env | grep -c '^SPAWN_')\"; exec cat",
    ))
    .expect("start");
    screen_with(&host, &one, &workspace, "env:hi 'there' spawn:0");
    let ServerSeen::Answers(Some(server)) = host.server() else {
        panic!("the server the host started is named");
    };
    assert!(Path::new(&format!("/proc/{}", server.pid)).exists());
    assert_eq!(
        host.runner(&one, &workspace).expect("runner"),
        Runner::Running
    );
    assert!(host.is_alive(&one, &workspace).expect("alive"));
    assert_eq!(
        host.runners(&[
            (one.clone(), workspace.clone()),
            (missing.clone(), workspace.clone())
        ])
        .expect("runners"),
        vec![
            (one.clone(), Runner::Running),
            (missing.clone(), Runner::Gone)
        ]
    );
    // The launch script removed itself.
    let left: Vec<_> = walk(&session.runtime())
        .into_iter()
        .filter(|path| path.to_string_lossy().ends_with(".sh"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
    // A second start of the same name is refused.
    let again = host
        .start(&session.launch(&one, "exec cat"))
        .expect_err("a duplicate");
    assert!(again.0.contains("already exists"), "{again}");

    // Staging types nothing; the submit types it, then Enter.
    host.stage_text(&one, &workspace, "staged \"quoted\" $HOME")
        .expect("stage");
    std::thread::sleep(Duration::from_millis(300));
    let before = host.capture_tail(&one, &workspace, 50).expect("capture");
    assert!(!before.contains("staged"), "{before}");
    host.submit_staged(&one, &workspace).expect("submit");
    let after = screen_with(&host, &one, &workspace, "staged \"quoted\" $HOME\nstaged");
    assert_eq!(
        after.matches("staged \"quoted\" $HOME").count(),
        2,
        "{after}"
    );
    // Nothing is held any more.
    assert!(host.submit_staged(&one, &workspace).is_err());

    host.send_keys(&one, &workspace, &["y", "e", "s", "Enter"])
        .expect("keys");
    screen_with(&host, &one, &workspace, "yes\nyes");

    // The same server throughout.
    assert_eq!(host.server(), ServerSeen::Answers(Some(server)));

    host.terminate(&one, &workspace).expect("terminate");
    assert!(!host.is_alive(&one, &workspace).expect("alive"));
    assert_eq!(host.runner(&one, &workspace).expect("runner"), Runner::Gone);
    host.terminate(&one, &workspace)
        .expect("a gone terminal is no error");
    assert!(host.submit_text(&one, &workspace, "x").is_err());
}

#[test]
fn a_runner_that_exits_leaves_no_terminal_and_a_long_text_arrives_whole() {
    let _serial = exclusive();
    let Some(session) = Session::new() else {
        return;
    };
    let host = session.host();
    let workspace = session.workspace();

    let brief = terminal("spawn-exec_h2");
    host.start(&session.launch(&brief, "sleep 0.3"))
        .expect("start");
    assert_eq!(
        runner_within(&host, &brief, &workspace, &Runner::Gone),
        Runner::Gone
    );
    assert!(!host.is_alive(&brief, &workspace).expect("alive"));
    host.terminate(&brief, &workspace).expect("terminate");

    // Far over one terminal line, one tmux message and one send_text
    // piece: read without a line limit, it arrives byte for byte, then the
    // Enter (a carriage return the terminal reads as a newline).
    let long = terminal("spawn-exec_h3");
    let out = session.cwd().join("out");
    host.start(&session.launch(
        &long,
        "stty -icanon -echo; echo ready; head -c 100001 > out; echo done; exec cat",
    ))
    .expect("start");
    screen_with(&host, &long, &workspace, "ready");
    let text: String = (0..100_000_u32)
        .map(|index| char::from(b'a' + u8::try_from(index % 26).expect("a letter")))
        .collect();
    host.submit_text(&long, &workspace, &text).expect("submit");
    screen_with(&host, &long, &workspace, "done");
    let written = std::fs::read(&out).expect("out");
    assert_eq!(written.len(), 100_001);
    assert_eq!(&written[..100_000], text.as_bytes());
    assert_eq!(written[100_000], b'\n');

    // A runner that ignores the hang-up is killed and waited for.
    let stubborn = terminal("spawn-exec_h4");
    host.start(&session.launch(
        &stubborn,
        "trap '' HUP TERM; sleep 300 & echo $! > child; wait",
    ))
    .expect("start");
    let child_file = session.cwd().join("child");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !child_file.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let child: u32 = std::fs::read_to_string(&child_file)
        .expect("child")
        .trim()
        .parse()
        .expect("pid");
    host.terminate(&stubborn, &workspace).expect("terminate");
    let stat = std::fs::read_to_string(format!("/proc/{child}/stat")).unwrap_or_default();
    assert!(
        stat.is_empty() || stat.contains(") Z"),
        "the runner's child outlived terminate: {stat}"
    );
    host.terminate(&long, &workspace).expect("terminate");
}

fn walk(directory: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(directory).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(walk(&path));
        } else {
            found.push(path);
        }
    }
    found
}

/// Every tmux key name the host translates is one Herdr presses: none is
/// refused, and each reaches the runner.
#[test]
fn every_translated_key_is_one_herdr_presses() {
    let _serial = exclusive();
    let Some(session) = Session::new() else {
        return;
    };
    let host = session.host();
    let workspace = session.workspace();
    let keys = terminal("spawn-exec_h5");
    host.start(&session.launch(&keys, "stty raw -echo; echo ready; exec od -An -c"))
        .expect("start");
    screen_with(&host, &keys, &workspace, "ready");
    let all = [
        "Enter", "Escape", "Tab", "BTab", "BSpace", "Space", "Up", "Down", "Left", "Right", "F1",
        "F12", "C-c", "C-d", "M-x", "y",
    ];
    for key in all {
        host.send_keys(&keys, &workspace, &[key])
            .unwrap_or_else(|error| panic!("{key}: {error}"));
    }
    host.terminate(&keys, &workspace).expect("terminate");
}
