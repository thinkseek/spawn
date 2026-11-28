//! Real end-to-end integration tests for `spawn agent`: spawns an un-owned
//! external (`Origin::External`) interactive Python PTY under a real
//! `spawn-hc` daemon, auto-building `spawn-hc` when absent, and exercises the
//! compiled `spawn` binary across `agent ls`, `agent snapshot`, `agent keys`,
//! `agent send`, `agent wait`, `agent interrupt`, and fail-closed flag/guard
//! validations.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use spawn_core::tui::{
    SnapshotFidelity, TerminalLiveState, TerminalOrigin, TuiActionOutcome, TuiSnapshot, WaitReason,
};
use spawn_host::herdr_global::HerdrGlobalClient;

static SEQ: AtomicU64 = AtomicU64::new(1);

fn run_spawn(args: &[&str]) -> Output {
    let path_env = std::env::var("PATH").unwrap_or_default();
    Command::new(env!("CARGO_BIN_EXE_spawn"))
        .args(args)
        .env_clear()
        .envs([("PATH", path_env.as_str()), ("TMPDIR", "/tmp")])
        .output()
        .expect("execute spawn binary")
}

fn find_sibling_spawn_hc() -> Option<String> {
    let spawn_bin = Path::new(env!("CARGO_BIN_EXE_spawn"));
    let exe = std::env::current_exe().ok();
    for dir in [
        spawn_bin.parent(),
        exe.as_ref().and_then(|p| p.parent()),
        exe.as_ref().and_then(|p| p.parent()).and_then(Path::parent),
    ]
    .into_iter()
    .flatten()
    {
        let candidate = dir.join("spawn-hc");
        if candidate.is_file() {
            return Some(candidate.display().to_string());
        }
    }
    None
}

fn require_spawn_hc_binary() -> String {
    if let Some(prog) = std::env::var_os("SPAWN_HERDR_CORE_PROGRAM")
        .map(|v| v.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
    {
        if prog.contains(std::path::MAIN_SEPARATOR) {
            assert!(
                Path::new(&prog).is_file(),
                "SPAWN_HERDR_CORE_PROGRAM={prog:?} does not exist"
            );
        }
        return prog;
    }
    if let Some(found) = find_sibling_spawn_hc() {
        return found;
    }

    let spawn_bin = Path::new(env!("CARGO_BIN_EXE_spawn"));
    let profile_dir = spawn_bin.parent();
    let is_release = profile_dir
        .and_then(Path::file_name)
        .and_then(|s| s.to_str())
        == Some("release");
    let target_dir = profile_dir.and_then(Path::parent);
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root");

    let mut build = Command::new(env!("CARGO"));
    build
        .current_dir(&workspace_root)
        .args(["build", "--locked", "-p", "spawn-hc"]);
    if std::env::var("CARGO_NET_OFFLINE").is_ok_and(|v| v.eq_ignore_ascii_case("true")) {
        build.arg("--offline");
    }
    if is_release {
        build.arg("--release");
    }
    if let Some(dir) = target_dir {
        build.env("CARGO_TARGET_DIR", dir);
    }
    let output = build
        .output()
        .expect("invoke cargo build --locked -p spawn-hc");
    assert!(
        output.status.success(),
        "cargo build --locked -p spawn-hc failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    find_sibling_spawn_hc().unwrap_or_else(|| {
        panic!(
            "spawn-hc binary still missing after cargo build in {}",
            workspace_root.display()
        )
    })
}

struct HcEnvGuard {
    root: PathBuf,
    socket: PathBuf,
    state_home: PathBuf,
    config_home: PathBuf,
    runtime_dir: PathBuf,
    program: String,
}

impl HcEnvGuard {
    fn new(program: String) -> Self {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let root =
            Path::new("/tmp").join(format!("spawn-agent-cli-e2e-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let state_home = root.join("st");
        let config_home = root.join("cfg");
        let runtime_dir = root.join("run");
        for d in [&root, &state_home, &config_home, &runtime_dir] {
            std::fs::create_dir_all(d).expect("mkdir");
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700)).expect("0700");
        }
        let socket = runtime_dir.join("hc.sock");
        Self {
            root,
            socket,
            state_home,
            config_home,
            runtime_dir,
            program,
        }
    }
}

impl Drop for HcEnvGuard {
    fn drop(&mut self) {
        let client = HerdrGlobalClient::at_socket(&self.socket);
        let _ = client.execute(
            "t1",
            &spawn_core::tui::TuiAction::Interrupt {
                timeout_ms: Some(500),
            },
        );
        let child = Command::new(&self.program)
            .stdin(std::process::Stdio::null())
            .env("HC_SOCKET", &self.socket)
            .env("XDG_STATE_HOME", &self.state_home)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .args(["server", "stop"])
            .output();
        let _ = child;
        std::thread::sleep(Duration::from_millis(120));
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

// Phase 1: reverse-video-only menu (no text glyph marker), Down moves selection 1->2,
// Enter prints PICKED_<n> + READY_FOR_INPUT. Phase 2: echoes submitted input as RECEIVED:<line>.
const INTERACTIVE_PTY_SCRIPT: &str = r#"import sys, termios, tty, time, signal

def on_sigint(_signum, _frame):
    sys.stdout.write("\r\nINTERRUPTED_OK\r\n")
    sys.stdout.flush()
    sys.exit(0)

signal.signal(signal.SIGINT, on_sigint)
fd = sys.stdin.fileno()
old = termios.tcgetattr(fd)
sel = 0
items = ["Allow action", "Deny action"]

def draw():
    sys.stdout.write("\x1b[2J\x1b[1;1HSelect action:\r\n")
    for idx, label in enumerate(items):
        if idx == sel:
            sys.stdout.write(f"  \x1b[7m{idx + 1}. {label}\x1b[0m\r\n")
        else:
            sys.stdout.write(f"  {idx + 1}. {label}\r\n")
    sys.stdout.flush()

try:
    tty.setraw(fd)
    draw()
    while True:
        ch = sys.stdin.read(1)
        if ch == "\x1b":
            seq = sys.stdin.read(2)
            if seq == "[B":
                sel = min(1, sel + 1)
                draw()
            elif seq == "[A":
                sel = max(0, sel - 1)
                draw()
        elif ch in ("\r", "\n"):
            time.sleep(0.15)
            sys.stdout.write(f"\r\nPICKED_{sel + 1}\r\nREADY_FOR_INPUT\r\n")
            sys.stdout.flush()
            break
finally:
    termios.tcsetattr(fd, termios.TCSADRAIN, old)

line = sys.stdin.readline().rstrip("\r\n")
line = line.replace("\x1b[200~", "").replace("\x1b[201~", "")
sys.stdout.write(f"\r\nRECEIVED:{line}\r\n")
sys.stdout.flush()
time.sleep(30)
"#;

fn launch_external_terminal(env: &HcEnvGuard) -> String {
    let script_path = env.root.join("pty_agent.py");
    std::fs::write(&script_path, INTERACTIVE_PTY_SCRIPT).expect("write script");
    let mut cmd = Command::new(&env.program);
    for (name, _) in std::env::vars_os() {
        if name
            .to_str()
            .is_some_and(|n| n.starts_with("HERDR_") || n.starts_with("HC_"))
        {
            cmd.env_remove(name);
        }
    }
    let output = cmd
        .stdin(std::process::Stdio::null())
        .env("HC_SOCKET", &env.socket)
        .env("HC_SESSION", "ar2cli")
        .env("XDG_STATE_HOME", &env.state_home)
        .env("XDG_CONFIG_HOME", &env.config_home)
        .env("XDG_RUNTIME_DIR", &env.runtime_dir)
        .args([
            "--json",
            "run",
            "--bg",
            "--",
            "python3",
            &script_path.to_string_lossy(),
        ])
        .output()
        .expect("launch external terminal via spawn-hc");
    assert!(
        output.status.success(),
        "spawn-hc run failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let run_json: Value = serde_json::from_slice(&output.stdout).expect("parse run --json");
    run_json["address"]
        .as_str()
        .expect("terminal address")
        .to_string()
}

fn await_snapshot_choice(socket: &str, addr: &str, selected_idx: usize) -> TuiSnapshot {
    for _ in 0..50 {
        let out = run_spawn(&["agent", "snapshot", addr, "--socket", socket, "--json"]);
        if out.status.success()
            && let Ok(snap) = serde_json::from_slice::<TuiSnapshot>(&out.stdout)
        {
            let choices = snap.choices();
            if choices.len() == 2
                && choices[selected_idx].selected
                && !choices[1 - selected_idx].selected
            {
                return snap;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("reverse-video choice {selected_idx} never became visible on {addr}");
}

fn await_screen_text(socket: &str, addr: &str, needle: &str) -> TuiSnapshot {
    for _ in 0..100 {
        let out = run_spawn(&["agent", "snapshot", addr, "--socket", socket, "--json"]);
        if out.status.success()
            && let Ok(snap) = serde_json::from_slice::<TuiSnapshot>(&out.stdout)
            && snap.plain_lines().iter().any(|l| l.contains(needle))
        {
            return snap;
        }
        std::thread::sleep(Duration::from_millis(80));
    }
    panic!("screen text {needle:?} never appeared on {addr}");
}

#[test]
fn agent_cli_validates_flags_fail_closed_without_herdr() {
    let help = run_spawn(&["agent", "--help"]);
    assert_eq!(help.status.code(), Some(0));
    let help_out = String::from_utf8_lossy(&help.stdout);
    for sub in ["ls", "snapshot", "send", "keys", "interrupt", "wait"] {
        assert!(
            help_out.contains(&format!("spawn agent {sub}")),
            "{help_out}"
        );
    }

    for bad in [
        &[
            "agent",
            "send",
            "t1",
            "hi",
            "--if-guard",
            "0x1234",
            "--force",
        ][..],
        &[
            "agent",
            "send",
            "t1",
            "hi",
            "--force",
            "--if-guard",
            "0x1234",
        ],
        &["agent", "send", "t1", "hi", "--submit"],
        &["agent", "keys", "t1", "not_a_key"],
        &["agent", "keys", "t1"],
        &["agent", "keys", "t1", "enter", "--if-guard", "not-hex"],
        &["agent", "wait", "t1", "--timeout", "0s"],
        &["agent", "snapshot"],
        &["agent", "snapshot", "a/b/c"],
        &["agent", "snapshot", ".."],
    ] {
        let out = run_spawn(bad);
        assert_ne!(out.status.code(), Some(0), "must fail closed for {bad:?}");
    }
}

fn verify_ls_and_menu_selection_guard(sock: &str, addr: &str) -> (String, String) {
    let ls_out = run_spawn(&["agent", "ls", "--socket", sock, "--json"]);
    assert_eq!(
        ls_out.status.code(),
        Some(0),
        "ls stderr: {}",
        String::from_utf8_lossy(&ls_out.stderr)
    );
    let ls_json: Value = serde_json::from_slice(&ls_out.stdout).expect("ls json");
    let listed = ls_json["agents"].as_array().expect("agents array");
    let target = listed
        .iter()
        .find(|item| item["address"].as_str() == Some(addr))
        .expect("external terminal in ls --json");
    assert_eq!(
        serde_json::from_value::<TerminalOrigin>(target["origin"].clone()).expect("origin"),
        TerminalOrigin::External
    );
    assert!(matches!(
        serde_json::from_value::<TerminalLiveState>(target["state"].clone()).expect("state"),
        TerminalLiveState::Live { pid: Some(_) }
    ));

    let snap1 = await_snapshot_choice(sock, addr, 0);
    assert!(matches!(
        snap1.fidelity(),
        SnapshotFidelity::StyledVt {
            alt_screen: false,
            ..
        }
    ));
    let guard1 = snap1.guard().to_string();

    let down_out = run_spawn(&[
        "agent",
        "keys",
        addr,
        "down",
        "--if-guard",
        &guard1,
        "--socket",
        sock,
        "--json",
    ]);
    assert_eq!(
        down_out.status.code(),
        Some(0),
        "keys down stderr: {}",
        String::from_utf8_lossy(&down_out.stderr)
    );
    let down_outcome: TuiActionOutcome =
        serde_json::from_slice(&down_out.stdout).expect("keys outcome");
    assert!(matches!(down_outcome, TuiActionOutcome::KeysSent { .. }));

    let snap2 = await_snapshot_choice(sock, addr, 1);
    assert_eq!(snap2.plain_lines(), snap1.plain_lines());
    let guard2 = snap2.guard().to_string();
    assert_ne!(
        guard2, guard1,
        "reverse-video selection shift changed guard"
    );

    let stale_keys = run_spawn(&[
        "agent",
        "keys",
        addr,
        "enter",
        "--if-guard",
        &guard1,
        "--socket",
        sock,
        "--json",
    ]);
    assert_ne!(
        stale_keys.status.code(),
        Some(0),
        "stale guard on keys must fail closed"
    );
    assert!(
        String::from_utf8_lossy(&stale_keys.stderr).contains("guard_mismatch"),
        "{}",
        String::from_utf8_lossy(&stale_keys.stderr)
    );
    (guard1, guard2)
}

fn verify_wait_and_enter_completion(sock: &str, addr: &str, guard2: &str) {
    let sock_w = sock.to_string();
    let addr_w = addr.to_string();
    let wait_thread = std::thread::spawn(move || {
        run_spawn(&[
            "agent",
            "wait",
            &addr_w,
            "--output",
            "PICKED_2",
            "--timeout",
            "8s",
            "--socket",
            &sock_w,
            "--json",
        ])
    });
    std::thread::sleep(Duration::from_millis(120));

    let enter_out = run_spawn(&[
        "agent",
        "keys",
        addr,
        "enter",
        "--if-guard",
        guard2,
        "--socket",
        sock,
        "--json",
    ]);
    assert_eq!(
        enter_out.status.code(),
        Some(0),
        "enter stderr: {}",
        String::from_utf8_lossy(&enter_out.stderr)
    );

    let wait_out = wait_thread.join().expect("join wait");
    assert_eq!(
        wait_out.status.code(),
        Some(0),
        "wait stderr: {}",
        String::from_utf8_lossy(&wait_out.stderr)
    );
    let wait_outcome: TuiActionOutcome =
        serde_json::from_slice(&wait_out.stdout).expect("wait outcome");
    assert!(
        matches!(
            wait_outcome,
            TuiActionOutcome::Waited(ref w) if w.ended() && w.reason == WaitReason::Output
        ),
        "{wait_outcome:?}"
    );
    let _ready = await_screen_text(sock, addr, "READY_FOR_INPUT");
}

fn verify_send_interrupt_and_wait_turn(sock: &str, addr: &str, stale_guard: &str) {
    let stale_send = run_spawn(&[
        "agent",
        "send",
        addr,
        "should-not-send",
        "--if-guard",
        stale_guard,
        "--socket",
        sock,
        "--json",
    ]);
    assert_ne!(stale_send.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&stale_send.stderr).contains("guard_mismatch"),
        "{}",
        String::from_utf8_lossy(&stale_send.stderr)
    );

    let send_out = run_spawn(&[
        "agent",
        "send",
        addr,
        "hello-ar2",
        "--socket",
        sock,
        "--json",
    ]);
    assert_eq!(
        send_out.status.code(),
        Some(0),
        "send stderr: {}",
        String::from_utf8_lossy(&send_out.stderr)
    );
    let send_outcome: TuiActionOutcome =
        serde_json::from_slice(&send_out.stdout).expect("send outcome");
    assert!(matches!(send_outcome, TuiActionOutcome::TextSent { .. }));

    let after_send = await_screen_text(sock, addr, "RECEIVED:hello-ar2");
    assert!(
        after_send
            .plain_lines()
            .iter()
            .any(|line| line.contains("RECEIVED:hello-ar2"))
    );

    let int_out = run_spawn(&[
        "agent",
        "interrupt",
        addr,
        "--timeout",
        "5s",
        "--socket",
        sock,
        "--json",
    ]);
    assert_eq!(
        int_out.status.code(),
        Some(0),
        "interrupt stderr: {}",
        String::from_utf8_lossy(&int_out.stderr)
    );
    let int_outcome: TuiActionOutcome =
        serde_json::from_slice(&int_out.stdout).expect("interrupt outcome");
    assert!(matches!(int_outcome, TuiActionOutcome::Interrupted(_)));

    let wait_turn_out = run_spawn(&[
        "agent",
        "wait",
        addr,
        "--after-turn",
        "0",
        "--timeout",
        "500ms",
        "--socket",
        sock,
        "--json",
    ]);
    assert_eq!(
        wait_turn_out.status.code(),
        Some(0),
        "wait after-turn stderr: {}",
        String::from_utf8_lossy(&wait_turn_out.stderr)
    );
    let wait_turn: TuiActionOutcome =
        serde_json::from_slice(&wait_turn_out.stdout).expect("wait turn outcome");
    assert!(matches!(wait_turn, TuiActionOutcome::Waited(_)));
}

#[test]
fn real_spawn_hc_agent_cli_end_to_end_discovery_guard_send_interrupt_wait() {
    let program = require_spawn_hc_binary();
    let env = HcEnvGuard::new(program);
    let addr = launch_external_terminal(&env);
    let sock = env.socket.to_string_lossy().into_owned();

    let (guard1, guard2) = verify_ls_and_menu_selection_guard(&sock, &addr);
    verify_wait_and_enter_completion(&sock, &addr, &guard2);
    verify_send_interrupt_and_wait_turn(&sock, &addr, &guard1);

    eprintln!("REAL_SPAWN_AGENT_CLI_E2E_VERIFIED address={addr} guard1={guard1} guard2={guard2}");
}
