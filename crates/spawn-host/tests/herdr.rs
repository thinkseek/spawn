//! The Herdr host against a real `herdr` (`SPAWN_TERMINAL_BACKEND=herdr`).
//!
//! Each test runs its own Herdr session (`spawn-test-<pid>-<n>`), whose
//! server the host starts itself, with its runtime directory in the test's
//! scratch; the session's server is stopped and the session deleted when the
//! test ends. The user's default and interactive sessions are never named.
//!
//! The `herdr` is `$HOME/.local/bin/herdr`, else the one on `PATH`; a host
//! with neither skips these tests (and says so), unless
//! `SPAWN_REQUIRE_HERDR=1`, which makes a missing `herdr` a failure: a
//! developer's or verifier's host with one never passes by skipping.
//!
//! The agent witness (issue #91 P2 PR 2) is tested against a stub `agy`
//! ([`STUB_AGY`]) that draws the states Herdr's agy manifest reads.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use spawn_core::id::{TerminalName, WorkspaceId};
use spawn_core::port::{AgentReading, AgentStatus, Launch, Runner, ServerSeen, TerminalHost};
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
            assert!(
                !herdr_required(),
                "SPAWN_REQUIRE_HERDR=1 and no herdr is installed"
            );
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
    let named: Vec<&str> = spawn_core::command::NamedKey::ALL
        .iter()
        .map(|key| key.tmux())
        .collect();
    for key in all.into_iter().chain(named) {
        host.send_keys(&keys, &workspace, &[key])
            .unwrap_or_else(|error| panic!("{key}: {error}"));
    }
    host.terminate(&keys, &workspace).expect("terminate");
}

/// Whether the suite must run: `SPAWN_REQUIRE_HERDR=1` turns a missing
/// `herdr` into a failure instead of a skip, for a host that has one (the
/// gate's sandbox binds none, and skips).
fn herdr_required() -> bool {
    std::env::var("SPAWN_REQUIRE_HERDR").is_ok_and(|value| value == "1")
}

/// A stand-in for the Antigravity CLI, named `agy` so Herdr detects it as
/// agy (by the process's name) and reads its screen by agy's manifest. It
/// draws each state agy draws (as agy 1.5.14 does, CR LF line ends; open
/// work as its task panel of `●` entries and a closing rule between the
/// prompt box and a footer counting `subagent(s)` and `task(s)`), reads
/// a line per state change (bash `read -t`, Enter as CR or LF, a bracketed
/// paste's markers dropped, as `agent.prompt` writes one), turns
/// bracketed paste on as agy does and off again when it exits (`EXIT`
/// trap), so the pane is left clean. The banner names a made-up account.
const STUB_AGY: &str = r#"#!/bin/bash
trap 'printf "\033[?2004l"' EXIT
printf '\033[?2004h'
rule() { printf '%s\r\n' "────────────────────────────────────────────────"; }
banner() { printf '      ▄▀▀▄        Antigravity CLI 1.5.14\r\n     ▀▀▀▀▀▀       someone@example.invalid (Pro)\r\n\r\n'; }
draw() {
  printf '\033[2J\033[H'
  case "$1" in
  trust)
    printf 'Accessing workspace:\r\n\r\n%s\r\n\r\nDo you trust the contents of this project?\r\n\r\n' "$PWD"
    printf '> Yes, I trust this folder\r\n  No, exit\r\n\r\n  ↑/↓ Navigate · enter Confirm\r\n' ;;
  idle)
    banner; rule; printf '>\r\n'; rule; printf '? for shortcuts\r\n' ;;
  tasks)
    banner; rule; printf '>\r\n'; rule
    printf '● Start job A (sleep 25)\r\n● Start job B (sleep 40)\r\n● File counter (subagent)\r\n'
    rule
    printf '? for shortcuts            auto · Gemini 3.8 Flash · low · 1 subagent(s) · 2 task(s) · /tasks\r\n' ;;
  work)
    banner; printf '⠋ Thinking about it\r\n'; rule; printf '>\r\n'; rule; printf 'esc to cancel\r\n' ;;
  perm)
    printf 'Run command?\r\n  cargo test\r\n> 1. Yes\r\n  2. No\r\n'
    printf '↑/↓ Navigate · tab Amend · ctrl+g edit/expand command\r\nesc to cancel\r\n' ;;
  ask)
    printf 'Which one?\r\n> 1. A\r\n  2. B\r\n↑/↓ Navigate · enter Select · esc Skip\r\nesc to cancel\r\n' ;;
  esac
}
state=${AGY_STUB_START:-idle}
draw "$state"
while true; do
  if IFS= read -r -t 1 line; then
    line=${line%$'\r'}
    # A bracketed paste (agent.prompt's write) arrives between its markers.
    line=${line#$'\e[200~'}; line=${line%$'\e[201~'}
    case "$line" in
      "") [ "$state" = trust ] && state=idle ;;
      exit) exit 0 ;;
      *) state=$line ;;
    esac
    draw "$state"
  fi
done
"#;

impl Session {
    /// The stub `agy` in the scratch's own `bin`.
    fn stub_agy(&self) -> PathBuf {
        let bin = self.scratch.join("bin");
        std::fs::create_dir_all(&bin).expect("bin");
        let agy = bin.join("agy");
        std::fs::write(&agy, STUB_AGY).expect("stub");
        std::fs::set_permissions(&agy, std::fs::Permissions::from_mode(0o755)).expect("0755");
        agy
    }

    /// A launch of the stub agy, drawing `first` first.
    fn agy_launch(&self, terminal: &TerminalName, first: &str) -> Launch {
        Launch {
            terminal: terminal.clone(),
            workspace: self.workspace(),
            cwd: self.cwd().display().to_string(),
            program: self.stub_agy().display().to_string(),
            arguments: Vec::new(),
            environment: vec![
                ("AGY_STUB_START".to_string(), first.to_string()),
                ("TERM".to_string(), "xterm-256color".to_string()),
            ],
        }
    }

    /// The session's API socket, as `herdr status` names it.
    fn socket(&self) -> PathBuf {
        let mut command = Command::new(&self.program);
        command
            .arg("--session")
            .arg(&self.name)
            .arg("status")
            .stdin(Stdio::null());
        for (name, _) in std::env::vars_os() {
            if name.to_str().is_some_and(|name| name.starts_with("HERDR_")) {
                command.env_remove(name);
            }
        }
        let output = command.output().expect("status");
        let text = String::from_utf8_lossy(&output.stdout).to_string();
        let mut in_server = false;
        for line in text.lines() {
            if !line.starts_with(' ') {
                in_server = line.trim_end() == "server:";
            } else if in_server && let Some(path) = line.trim().strip_prefix("socket:") {
                return PathBuf::from(path.trim());
            }
        }
        panic!("no socket in: {text}");
    }

    /// One raw API call (what a person's Herdr, or Herdr's own restore,
    /// does beside Spawn's host): the whole answer.
    fn call(&self, method: &str, params: &serde_json::Value) -> serde_json::Value {
        use std::io::{BufRead, Write};
        let stream = std::os::unix::net::UnixStream::connect(self.socket()).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        let request = serde_json::json!({ "id": "test", "method": method, "params": params });
        (&stream)
            .write_all(format!("{request}\n").as_bytes())
            .expect("write");
        let mut answer = String::new();
        std::io::BufReader::new(&stream)
            .read_line(&mut answer)
            .expect("read");
        serde_json::from_str(&answer).expect("json")
    }

    /// The pane the host recorded for `terminal`.
    fn pane_of(&self, terminal: &TerminalName) -> String {
        let record = walk(&self.runtime())
            .into_iter()
            .find(|path| {
                path.file_name().and_then(|name| name.to_str()) == Some(terminal.as_str())
                    && path
                        .parent()
                        .is_some_and(|parent| parent.ends_with("terminals"))
            })
            .expect("a record");
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(record).expect("record")).expect("json");
        value["pane"].as_str().expect("pane").to_string()
    }

    fn workspace_labels(&self) -> Vec<String> {
        self.call("workspace.list", &serde_json::json!({}))["result"]["workspaces"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|workspace| workspace["label"].as_str().map(str::to_string))
            .collect()
    }
}

/// The host's agent reading once `want` holds, within 15 s.
fn reading_within(
    host: &HerdrHost,
    terminal: &TerminalName,
    workspace: &WorkspaceId,
    want: impl Fn(&AgentReading) -> bool,
) -> AgentReading {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let reading = host
            .agent(terminal, workspace)
            .expect("agent")
            .expect("herdr reads agents");
        if want(&reading) || Instant::now() >= deadline {
            assert!(want(&reading), "never reached: {reading:?}");
            return reading;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Whether the host's change queue names `terminal` within 10 s.
fn changes_name(host: &HerdrHost, terminal: &TerminalName) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if host.agent_changes().contains(terminal) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Herdr detects the stub as agy and reads every state of its manifest
/// (2026.10.05.1): the trust dialog, the prompt (by its idle fallback: no
/// rule), the prompt with open tasks (still idle for Herdr, busy for
/// Spawn's reading), a running turn, a permission and a question. Each
/// change reaches the host's change queue; the pane's account line never
/// reaches Spawn's reading.
#[allow(clippy::too_many_lines)] // one stub session walked through every manifest state, in order
#[test]
fn the_agent_witness_reads_a_stub_agy_through_every_manifest_state() {
    let _serial = exclusive();
    let Some(session) = Session::new() else {
        return;
    };
    let host = session.host();
    let workspace = session.workspace();
    let agy = terminal("spawn-exec_a1");
    let shell = terminal("spawn-exec_a2");
    // No pane: a reading of no agent, from a host that reads agents.
    assert_eq!(
        host.agent(&agy, &workspace).expect("agent"),
        Some(AgentReading {
            agent: None,
            status: AgentStatus::Unknown,
            rule: None,
            blocker_visible: false,
            session: None,
            seq: 0,
            manifest: None,
        })
    );
    host.start(&session.agy_launch(&agy, "trust"))
        .expect("start");
    let trust = reading_within(&host, &agy, &workspace, |reading| {
        reading.status == AgentStatus::Blocked
    });
    assert_eq!(trust.agent.as_deref(), Some("agy"));
    assert_eq!(trust.rule.as_deref(), Some("trust_prompt"));
    assert!(trust.blocker_visible);
    assert!(
        trust
            .manifest
            .as_deref()
            .is_some_and(|manifest| !manifest.is_empty()),
        "{trust:?}"
    );
    let pane = screen_with(&host, &agy, &workspace, "enter Confirm");
    assert!(spawn_core::screen::is_agy_trust_dialog(&pane), "{pane}");
    assert!(changes_name(&host, &agy), "the detection is a change");

    let steps: [(
        &str,
        AgentStatus,
        Option<&str>,
        spawn_core::screen::ScreenState,
    ); 5] = [
        (
            "",
            AgentStatus::Idle,
            None,
            spawn_core::screen::ScreenState::Ready,
        ),
        (
            "tasks",
            AgentStatus::Idle,
            None,
            spawn_core::screen::ScreenState::Busy,
        ),
        (
            "work",
            AgentStatus::Working,
            Some("esc_cancel_footer_working"),
            spawn_core::screen::ScreenState::Busy,
        ),
        (
            "perm",
            AgentStatus::Blocked,
            Some("permission_prompt"),
            spawn_core::screen::ScreenState::Permission,
        ),
        (
            "ask",
            AgentStatus::Blocked,
            Some("question_prompt"),
            spawn_core::screen::ScreenState::Permission,
        ),
    ];
    let mut last = trust;
    for (typed, status, rule, screen) in steps {
        if last.status == AgentStatus::Blocked {
            // Herdr's `agent.prompt` (PR 3's `submit_staged`) may refuse a
            // blocked agent (`agent_blocked`, nothing typed), depending on
            // when its own status catches up: the stub is driven past its
            // dialog through the pane itself, as a person's keyboard would.
            let pane = session.pane_of(&agy);
            session.call(
                "pane.send_text",
                &serde_json::json!({ "pane_id": pane, "text": typed }),
            );
            session.call(
                "pane.send_keys",
                &serde_json::json!({ "pane_id": pane, "keys": ["enter"] }),
            );
        } else {
            host.submit_text(&agy, &workspace, typed).expect("typed");
        }
        let reading = reading_within(&host, &agy, &workspace, |reading| {
            // Herdr counts status changes only: permission to question
            // (blocked to blocked) keeps its seq, and changes its rule.
            reading.status == status && reading.rule.as_deref() == rule
        });
        assert!(
            reading.seq > last.seq || reading.status == last.status,
            "{reading:?} after {last:?}"
        );
        assert_eq!(
            reading.blocker_visible,
            status == AgentStatus::Blocked,
            "{typed}"
        );
        // The stub draws in one write: the screen is whole by now.
        std::thread::sleep(Duration::from_millis(300));
        let pane = host.capture_tail(&agy, &workspace, 50).expect("capture");
        let read = spawn_core::screen::read_agy_screen(&pane);
        assert_eq!(read.state, screen, "{typed}: {pane}");
        assert!(!read.evidence.contains("example.invalid"), "{read:?}");
        last = reading;
    }
    assert!(changes_name(&host, &agy), "status changes are pushed");

    // A plain shell: no agent there.
    host.start(&session.launch(&shell, "exec cat"))
        .expect("start");
    screen_with(&host, &shell, &workspace, "");
    let none = host
        .agent(&shell, &workspace)
        .expect("agent")
        .expect("read");
    assert_eq!(none.agent, None);

    // The stub exits: its EXIT trap turned bracketed paste off again.
    host.submit_text(&agy, &workspace, "exit").expect("typed");
    assert_eq!(
        runner_within(&host, &agy, &workspace, &Runner::Gone),
        Runner::Gone
    );
    host.terminate(&agy, &workspace).expect("terminate");
    host.terminate(&shell, &workspace).expect("terminate");
}

/// An agent Herdr itself starts (`agent.start`) is `launch_pending` with
/// no status for its first seconds, when Herdr answers `agent_not_ready`:
/// the host refuses to stage text for it then (refused before typing,
/// nothing typed), and its reading says nothing.
#[test]
fn an_agent_still_launching_refuses_input_before_anything_is_typed() {
    let _serial = exclusive();
    let Some(session) = Session::new() else {
        return;
    };
    let host = session.host();
    let workspace = session.workspace();
    let one = terminal("spawn-exec_a3");
    let bin = session.stub_agy();
    let bin = bin.parent().expect("bin").display().to_string();
    let mut launch = session.launch(&one, "echo shell-ready; exec /bin/sh -i");
    launch
        .environment
        .push(("PATH".to_string(), format!("{bin}:/usr/bin:/bin")));
    host.start(&launch).expect("start");
    screen_with(&host, &one, &workspace, "shell-ready");
    std::thread::sleep(Duration::from_millis(300));
    let pane = session.pane_of(&one);
    let started = session.call(
        "agent.start",
        &serde_json::json!({ "name": "stub", "kind": "agy", "pane_id": pane }),
    );
    assert_eq!(
        started["result"]["agent"]["launch_pending"],
        serde_json::json!(true),
        "{started}"
    );
    let refused = host
        .stage_text(&one, &workspace, "too soon")
        .expect_err("not ready yet");
    assert!(
        refused
            .0
            .contains("refused before typing (agent_not_ready)"),
        "{refused}"
    );
    let reading = host.agent(&one, &workspace).expect("agent").expect("read");
    assert_eq!(reading.status, AgentStatus::Unknown, "{reading:?}");
    // Once it settles, input is taken again.
    let deadline = Instant::now() + Duration::from_secs(20);
    while host.stage_text(&one, &workspace, "later").is_err() {
        assert!(Instant::now() < deadline, "the agent never settled");
        std::thread::sleep(Duration::from_millis(200));
    }
    host.terminate(&one, &workspace).expect("terminate");
}

/// The first time a host meets a server it closes the workspaces of
/// Spawn's terminals on it that no record of its own names (what a
/// restarted server restored): never a terminal it recorded there, nor a
/// workspace that is not Spawn's.
#[test]
fn restored_spawn_workspaces_are_swept_once_per_server() {
    let _serial = exclusive();
    let Some(session) = Session::new() else {
        return;
    };
    let host = session.host();
    let workspace = session.workspace();
    let kept = terminal("spawn-exec_a4");
    host.start(&session.launch(&kept, "exec cat"))
        .expect("start");
    for label in ["spawn-exec_a99", "someone-elses"] {
        let created = session.call(
            "workspace.create",
            &serde_json::json!({
                "cwd": session.cwd().display().to_string(),
                "label": label,
                "focus": false,
            }),
        );
        assert!(created.get("result").is_some(), "{created}");
    }
    // The same host met this server already: nothing is swept again.
    host.agent(&kept, &workspace).expect("agent");
    assert!(
        session
            .workspace_labels()
            .contains(&"spawn-exec_a99".to_string())
    );
    // A host that never met it (a restarted loop) sweeps.
    let fresh = session.host();
    fresh.agent(&kept, &workspace).expect("agent");
    let labels = session.workspace_labels();
    assert!(
        !labels.contains(&"spawn-exec_a99".to_string()),
        "{labels:?}"
    );
    assert!(labels.contains(&"spawn-exec_a4".to_string()), "{labels:?}");
    assert!(labels.contains(&"someone-elses".to_string()), "{labels:?}");
    assert_eq!(
        fresh.runner(&kept, &workspace).expect("runner"),
        Runner::Running
    );
    host.terminate(&kept, &workspace).expect("terminate");
}

/// A server that speaks another protocol than the host is refused, the
/// error naming both.
#[test]
fn a_server_of_another_protocol_is_refused_naming_both() {
    let _serial = exclusive();
    let Some(session) = Session::new() else {
        return;
    };
    let host = session.host().expecting_protocol(21);
    let one = terminal("spawn-exec_a5");
    let error = host
        .start(&session.launch(&one, "exec cat"))
        .expect_err("another protocol");
    assert!(
        error.0.contains("speaks protocol 22") && error.0.contains("expects protocol 21"),
        "{error}"
    );
    // The host that speaks it starts there.
    let right = session.host();
    assert_eq!(spawn_host::herdr::PROTOCOL, 22);
    right
        .start(&session.launch(&one, "exec cat"))
        .expect("start");
    right
        .terminate(&one, &session.workspace())
        .expect("terminate");
}

/// Carry list N8: a server whose active detection manifests differ from
/// the ones the host is pinned to is refused before anything starts,
/// naming the pinned and the actual version; the host pinned to what the
/// server has (Herdr 0.9.5's bundled manifests) starts there.
#[test]
fn a_server_of_other_detection_manifests_is_refused_naming_both() {
    let _serial = exclusive();
    let Some(session) = Session::new() else {
        return;
    };
    let host = session
        .host()
        .expecting_manifests(&[("agy", "1999.01.01.1"), ("claude", "2026.10.11.1")]);
    let one = terminal("spawn-exec_a6");
    let error = host
        .start(&session.launch(&one, "exec cat"))
        .expect_err("other manifests");
    assert!(
        error.0.contains("agy pinned 1999.01.01.1, actual 2026.10.05.1"),
        "{error}"
    );
    let right = session.host();
    right
        .start(&session.launch(&one, "exec cat"))
        .expect("start");
    right
        .terminate(&one, &session.workspace())
        .expect("terminate");
}

/// A process the engine detached from its session (`setsid`, as agy's
/// background updater does) is still beneath the workspace once the pane
/// is closed: terminate ends it by pid before it returns.
#[test]
fn a_detached_daemon_beneath_the_workspace_is_ended_by_terminate() {
    let _serial = exclusive();
    let Some(session) = Session::new() else {
        return;
    };
    let host = session.host();
    let workspace = session.workspace();
    let one = terminal("spawn-exec_a7");
    let pidfile = session.cwd().join("daemon.pid");
    let script = format!(
        "setsid sh -c 'echo $$ > {}; exec sleep 60' </dev/null >/dev/null 2>&1 & echo started; exec cat",
        pidfile.display()
    );
    host.start(&session.launch(&one, &script)).expect("start");
    screen_with(&host, &one, &workspace, "started");
    let deadline = Instant::now() + Duration::from_secs(10);
    let pid: u32 = loop {
        if let Some(pid) = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|text| text.trim().parse().ok())
        {
            break pid;
        }
        assert!(Instant::now() < deadline, "the daemon never started");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(Path::new(&format!("/proc/{pid}")).exists());
    host.terminate(&one, &workspace).expect("terminate");
    let gone = |pid: u32| {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map_or(true, |stat| stat.split(") ").nth(1).is_some_and(|rest| rest.starts_with('Z')))
    };
    assert!(gone(pid), "the detached daemon {pid} outlived terminate");
}

/// `submit_staged` hands a staged text to the pane's detected agent (the
/// stub agy) through `agent.prompt`, which submits it: the stub reads the
/// line and draws the state it names. A text past the request line cap,
/// and a pane with no agent, still arrive (chunked `pane.send_text`).
#[test]
fn a_staged_text_reaches_a_detected_agent_through_agent_prompt() {
    let _serial = exclusive();
    let Some(session) = Session::new() else {
        return;
    };
    let host = session.host();
    let workspace = session.workspace();
    let agy = terminal("spawn-exec_a8");
    host.start(&session.agy_launch(&agy, "idle")).expect("start");
    reading_within(&host, &agy, &workspace, |reading| {
        reading.agent.as_deref() == Some("agy") && reading.status == AgentStatus::Idle
    });
    host.stage_text(&agy, &workspace, "work").expect("stage");
    host.submit_staged(&agy, &workspace).expect("submit");
    screen_with(&host, &agy, &workspace, "esc to cancel");
    host.terminate(&agy, &workspace).expect("terminate");
}
