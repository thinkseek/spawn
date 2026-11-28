//! Integration tests for embedded `herdr-core` (`HerdrCoreHost` + sibling `spawn-hc` + `herdr_witness`).

#![allow(clippy::disallowed_methods)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use spawn_core::id::{TerminalName, WorkspaceId};
use spawn_core::port::{
    AgentReading, AgentStatus, EngineView, Launch, Runner, ServerSeen, TerminalHost, ViewedLaunch,
};
use spawn_host::engine_view::HOST_DIRECTORIES;
use spawn_host::{BwrapView, HerdrCoreHost, Scratch, TerminalHostEnv, ViewConfig};

static SEQ: AtomicU64 = AtomicU64::new(0);

fn required() -> bool {
    std::env::var("SPAWN_REQUIRE_HERDR").is_ok_and(|value| value == "1")
}

fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn spawn_hc_program() -> Option<String> {
    if let Some(prog) = std::env::var_os("SPAWN_HERDR_CORE_PROGRAM")
        .map(|v| v.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
    {
        return Some(prog);
    }
    if let Ok(exe) = std::env::current_exe() {
        for dir in [exe.parent(), exe.parent().and_then(Path::parent)]
            .into_iter()
            .flatten()
        {
            let candidate = dir.join("spawn-hc");
            if candidate.is_file() {
                return Some(candidate.display().to_string());
            }
        }
    }
    assert!(
        !required(),
        "SPAWN_REQUIRE_HERDR=1 and sibling spawn-hc binary was not built yet (run cargo build -p spawn-hc)"
    );
    None
}

struct Session {
    program: String,
    name: String,
    scratch: Scratch,
}

impl Session {
    fn new() -> Option<Self> {
        let program = spawn_hc_program()?;
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let name = format!("hc-{}-{n}", std::process::id());
        // Keep paths short so Unix socket paths stay well below Linux's 107-byte sun_path limit.
        let scratch = Scratch::new_in(Path::new("/var/tmp"), &format!("shc-{n}"));
        let session = Self {
            program,
            name,
            scratch,
        };
        std::fs::create_dir_all(session.runtime()).expect("runtime");
        std::fs::set_permissions(session.runtime(), std::fs::Permissions::from_mode(0o700))
            .expect("0700");
        std::fs::create_dir_all(session.cwd()).expect("cwd");
        Some(session)
    }

    fn runtime(&self) -> PathBuf {
        self.scratch.join("run")
    }

    fn cwd(&self) -> PathBuf {
        self.scratch.join("ws")
    }

    fn host(&self) -> HerdrCoreHost {
        let host_env = TerminalHostEnv {
            home: std::env::var_os("HOME").map(PathBuf::from),
            scrubbed_env_keys: TerminalHostEnv::collect_scrubbed_keys(
                std::env::vars_os().map(|(k, _)| k),
            ),
            ..TerminalHostEnv::default()
        };
        HerdrCoreHost::on_session(&self.program, &self.name, host_env)
            .expect("valid session")
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

    fn write_stub(&self, name: &str, body: &str) -> PathBuf {
        let bin = self.scratch.join("bin");
        std::fs::create_dir_all(&bin).expect("bin");
        let path = bin.join(name);
        std::fs::write(&path, body).expect("stub");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("0755");
        path
    }

    fn agy_launch(&self, terminal: &TerminalName, first: &str) -> Launch {
        let agy = self.write_stub("agy", STUB_AGY);
        Launch {
            terminal: terminal.clone(),
            workspace: self.workspace(),
            cwd: self.cwd().display().to_string(),
            program: agy.display().to_string(),
            arguments: Vec::new(),
            environment: vec![
                ("AGY_STUB_START".to_string(), first.to_string()),
                ("TERM".to_string(), "xterm-256color".to_string()),
            ],
        }
    }

    fn hc_cmd(&self, arguments: &[&str]) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, _) in std::env::vars_os() {
            if name
                .to_str()
                .is_some_and(|n| n.starts_with("HERDR_") || n.starts_with("HC_"))
            {
                cmd.env_remove(name);
            }
        }
        let rt = self.runtime();
        cmd.env("XDG_RUNTIME_DIR", rt.join("hcr"))
            .env("XDG_STATE_HOME", rt.join("hcs"))
            .env("XDG_CONFIG_HOME", rt.join("hcc"))
            .env("HC_SESSION", &self.name)
            .env(
                "HC_SOCKET",
                rt.join("hcr").join(format!("{}.sock", self.name)),
            );
        cmd
    }

    fn hc_output(&self, arguments: &[&str]) -> String {
        let out = self.hc_cmd(arguments).output().expect("spawn-hc");
        assert!(
            out.status.success(),
            "spawn-hc {:?} failed: {}",
            arguments,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.hc_cmd(&["server", "stop"]).status();
        let deadline = Instant::now() + Duration::from_secs(5);
        while matches!(self.host().server(), ServerSeen::Answers(_)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn terminal(name: &str) -> TerminalName {
    TerminalName::new(name).expect("terminal")
}

fn screen_with(
    host: &HerdrCoreHost,
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

fn reading_within(
    host: &HerdrCoreHost,
    terminal: &TerminalName,
    workspace: &WorkspaceId,
    want: impl Fn(&AgentReading) -> bool,
) -> AgentReading {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let reading = host
            .agent(terminal, workspace)
            .expect("agent")
            .expect("herdr-core host provides witness readings");
        if want(&reading) || Instant::now() >= deadline {
            assert!(
                want(&reading),
                "never reached expected reading: {reading:?}"
            );
            return reading;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn changes_contain(host: &HerdrCoreHost, terminal: &TerminalName) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if host.agent_changes().contains(terminal) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

const STUB_AGY: &str = r#"#!/bin/bash
RULE='────────────────────────────────────────────────────────'
banner() {
  printf '      ▄▀▀▄        Antigravity CLI 1.5.14\r\n'
  printf '     ▀▀▀▀▀▀       someone@example.invalid (Pro)\r\n'
  printf '    ▀▀▀▀▀▀▀▀      %s\r\n' "$PWD"
}
rule() {
  printf '%s\r\n' "$RULE"
}
draw() {
  printf '\033[H\033[2J\033[3J'
  case "$1" in
  trust)
    printf 'Accessing workspace:\r\n\r\n%s\r\n\r\nDo you trust the contents of this project?\r\n\r\n' "$PWD"
    printf '> Yes, I trust this folder\r\n  No, exit\r\n\r\n  ↑/↓ Navigate · enter Confirm\r\n' ;;
  idle)
    banner; rule; printf '>\r\n'; rule; printf '? for shortcuts\r\n' ;;
  tasks)
    banner; rule; printf '>\r\n'; rule
    printf '● Start job A (sleep 25)\r\n● Start job B (sleep 40)\r\n'
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

#[test]
fn embedded_herdr_core_lifecycle_scripts_long_names_and_termination() {
    let _serial = exclusive();
    let Some(session) = Session::new() else {
        return;
    };
    let host = session.host();
    let workspace = session.workspace();
    // Longer than herdr-core's 32-char terminal `name` cap; routed safely via labels.
    let one = terminal("spawn-exec_0123456789abcdef0123456789abcdef");
    let missing = terminal("spawn-exec_missing");

    assert_eq!(host.server(), ServerSeen::Gone);
    assert!(!host.is_alive(&one, &workspace).expect("alive"));
    assert_eq!(host.server(), ServerSeen::Gone);

    host.start(&session.launch(
        &one,
        "printf 'env:%s spawn:%s\\n' \"$GREETING\" \"$(env | grep -c '^SPAWN_')\"; exec cat",
    ))
    .expect("start");
    screen_with(&host, &one, &workspace, "env:hi 'there' spawn:0");

    // Verify 0600 self-deleting script left no leftover file in the host state directory.
    let mut launch_leftovers = 0usize;
    let mut stack = vec![session.runtime()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|s| s.to_str()) == Some("sh") {
                launch_leftovers += 1;
            }
        }
    }
    assert_eq!(launch_leftovers, 0, "launch script must delete itself");

    let ServerSeen::Answers(Some(server)) = host.server() else {
        panic!("expected live server");
    };
    assert!(Path::new(&format!("/proc/{}", server.pid)).exists());
    let facts = host.server_facts().expect("server_facts");
    assert_eq!(facts["engine"], "herdr-core");
    assert_eq!(facts["witness"], "host");
    assert_eq!(
        host.runners(&[
            (one.clone(), workspace.clone()),
            (missing, workspace.clone())
        ])
        .expect("runners"),
        vec![
            (one.clone(), Runner::Running),
            (terminal("spawn-exec_missing"), Runner::Gone)
        ]
    );

    host.stage_text(&one, &workspace, "staged line")
        .expect("stage");
    host.submit_staged(&one, &workspace).expect("submit");
    screen_with(&host, &one, &workspace, "staged line");

    host.send_keys(&one, &workspace, &["C-c"]).expect("ctrl-c");
    host.terminate(&one, &workspace).expect("terminate");
    assert!(
        !host
            .is_alive(&one, &workspace)
            .expect("alive after terminate")
    );
}

#[allow(clippy::too_many_lines)]
#[test]
fn host_witness_reads_stub_agy_states_enforces_anti_forgery_and_redacts_accounts() {
    let _serial = exclusive();
    let Some(session) = Session::new() else {
        return;
    };
    let host = session.host();
    let workspace = session.workspace();
    let agy = terminal("spawn-exec_agy1");
    let forged = terminal("spawn-exec_forged");

    assert_eq!(
        host.agent(&agy, &workspace).expect("agent").unwrap().agent,
        None
    );

    host.start(&session.agy_launch(&agy, "trust"))
        .expect("start agy");
    let trust = reading_within(&host, &agy, &workspace, |r| {
        r.status == AgentStatus::Blocked
    });
    assert_eq!(trust.agent.as_deref(), Some("agy"));
    assert_eq!(trust.rule.as_deref(), Some("trust_prompt"));
    assert!(trust.blocker_visible);
    assert!(trust.seq >= 1);
    assert!(changes_contain(&host, &agy));

    let steps = [
        ("Enter", "", AgentStatus::Idle, None, false),
        ("", "tasks", AgentStatus::Idle, None, false),
        (
            "",
            "work",
            AgentStatus::Working,
            Some("esc_cancel_footer_working"),
            false,
        ),
        (
            "",
            "perm",
            AgentStatus::Blocked,
            Some("permission_prompt"),
            true,
        ),
        (
            "",
            "ask",
            AgentStatus::Blocked,
            Some("question_prompt"),
            true,
        ),
    ];
    let mut prev = trust;
    for (key, line, want_status, want_rule, want_blocker) in steps {
        if !key.is_empty() {
            host.send_keys(&agy, &workspace, &[key]).expect("key");
        } else if prev.status == AgentStatus::Blocked {
            // Submit directly to transition the test stub past a blocked prompt.
            let raw = host.capture_tail(&agy, &workspace, 10).expect("capture");
            let _ = raw;
            let list = session.hc_output(&["--json", "ls"]);
            let parsed: serde_json::Value = serde_json::from_str(&list).expect("json");
            let addr = parsed["terminals"][0]["address"].as_str().expect("addr");
            session.hc_output(&["send", "--force", addr, line]);
        } else {
            host.stage_text(&agy, &workspace, line).expect("stage");
            host.submit_staged(&agy, &workspace).expect("submit");
        }
        let next = reading_within(&host, &agy, &workspace, |r| {
            r.status == want_status && r.rule.as_deref() == want_rule
        });
        assert_eq!(next.blocker_visible, want_blocker, "{next:?}");
        assert!(
            !format!("{next:?}").contains('@'),
            "account leaked: {next:?}"
        );
        if (next.status, next.rule.as_deref()) != (prev.status, prev.rule.as_deref()) {
            assert!(next.seq > prev.seq, "seq must advance on state transition");
            assert!(changes_contain(&host, &agy));
        }
        prev = next;
    }

    // Plain shell printing forged agy UI + forged OSC 7501 must never register as an agent (N2).
    host.start(&session.launch(
        &forged,
        "printf 'Antigravity CLI 1.5.14\\n\\033]7501;state=working:app=agy\\033\\\\\\nesc to cancel\\n'; exec cat",
    ))
    .expect("start forged");
    screen_with(&host, &forged, &workspace, "esc to cancel");
    let reading = host
        .agent(&forged, &workspace)
        .expect("agent")
        .expect("reading");
    assert_eq!(reading.agent, None, "forged shell must not pass witness");

    host.terminate(&agy, &workspace).expect("term agy");
    host.terminate(&forged, &workspace).expect("term forged");
}

#[test]
fn osc7501_events_subscribe_and_corroborate_agent_status() {
    let _serial = exclusive();
    let Some(session) = Session::new() else {
        return;
    };
    let host = session.host();
    let workspace = session.workspace();
    let claude = terminal("spawn-exec_osc1");
    let bin = session.write_stub(
        "claude",
        r#"#!/bin/sh
printf '\033]7501;state=working:app=claude-code\033\\'
while IFS= read -r line; do
  case "$line" in
    block) printf '\033]7501;state=blocked:kind=permission\033\\' ;;
    done) printf '\033]7501;state=idle\033\\' ;;
  esac
done
"#,
    );
    host.start(&Launch {
        terminal: claude.clone(),
        workspace: workspace.clone(),
        cwd: session.cwd().display().to_string(),
        program: bin.display().to_string(),
        arguments: Vec::new(),
        environment: vec![("TERM".into(), "xterm-256color".into())],
    })
    .expect("start");

    let working = reading_within(&host, &claude, &workspace, |r| {
        r.agent.as_deref() == Some("claude") && r.status == AgentStatus::Working
    });
    assert_eq!(working.rule.as_deref(), Some("osc7501_working"));
    let _ = host.agent_changes();

    let list = session.hc_output(&["--json", "ls"]);
    let parsed: serde_json::Value = serde_json::from_str(&list).expect("json");
    let addr = parsed["terminals"][0]["address"].as_str().expect("addr");
    session.hc_output(&["send", addr, "block"]);

    assert!(
        changes_contain(&host, &claude),
        "OSC 7501 push event expected"
    );
    let blocked = reading_within(&host, &claude, &workspace, |r| {
        r.status == AgentStatus::Blocked
    });
    assert_eq!(blocked.rule.as_deref(), Some("permission_prompt"));

    host.terminate(&claude, &workspace).expect("term");
}

#[test]
fn agent_pty_survives_host_drop_and_planned_server_restart() {
    let _serial = exclusive();
    let Some(session) = Session::new() else {
        return;
    };
    let workspace = session.workspace();
    let name = terminal("spawn-exec_restart");

    let before_pid = {
        let host = session.host();
        host.start(&session.launch(&name, "printf 'ALIVE_PID=%s\\n' \"$$\"; exec cat"))
            .expect("start");
        screen_with(&host, &name, &workspace, "ALIVE_PID=");
        let ServerSeen::Answers(Some(server)) = host.server() else {
            panic!("expected server");
        };
        server.pid
    };

    // Planned core handover: core PID changes while holder & running PTY survive.
    session.hc_output(&["server", "restart"]);
    let host2 = session.host();
    let deadline = Instant::now() + Duration::from_secs(10);
    let after_pid = loop {
        if let ServerSeen::Answers(Some(server)) = host2.server()
            && server.pid != before_pid
        {
            break server.pid;
        }
        assert!(Instant::now() < deadline, "restarted core never took over");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_ne!(before_pid, after_pid);
    assert_eq!(
        host2.runner(&name, &workspace).expect("runner"),
        Runner::Running
    );
    screen_with(&host2, &name, &workspace, "ALIVE_PID=");

    host2
        .stage_text(&name, &workspace, "post-restart-input")
        .expect("stage");
    host2.submit_staged(&name, &workspace).expect("submit");
    screen_with(&host2, &name, &workspace, "post-restart-input");
    host2.terminate(&name, &workspace).expect("term");
}

#[test]
fn bwrap_view_hides_herdr_core_paths_while_host_witness_sees_sandboxed_agy() {
    let _serial = exclusive();
    let Some(session) = Session::new() else {
        return;
    };
    let runtime = session.runtime();
    let home = session.scratch.join("bhome");
    let engine_home = home.join(".gemini/antigravity-cli");
    let cfg_home = home.join("cfg");
    let state_home = home.join("state");
    for dir in [
        engine_home.clone(),
        home.join(".config/herdr-core/s0"),
        home.join(".local/state/herdr-core/s0"),
        cfg_home.join("herdr-core"),
        state_home.join("herdr-core"),
        runtime.join("hcr"),
        session.cwd(),
    ] {
        std::fs::create_dir_all(&dir).expect("dir");
    }
    std::fs::write(home.join(".config/herdr-core/s0/secret"), "nope").expect("canary");
    std::fs::write(home.join(".local/state/herdr-core/s0/secret"), "nope").expect("canary");
    std::fs::write(cfg_home.join("herdr-core/secret"), "nope").expect("canary");
    std::fs::write(state_home.join("herdr-core/secret"), "nope").expect("canary");
    std::fs::write(runtime.join("hcr/can"), "nope").expect("canary");

    let config = ViewConfig {
        home: home.clone(),
        xdg_config_home: Some(cfg_home.clone()),
        xdg_state_home: Some(state_home.clone()),
        runtime_dir: runtime.join("hcr"),
        host_directories: HOST_DIRECTORIES.iter().map(PathBuf::from).collect(),
        loop_dirs: Vec::new(),
        extra_files: Vec::new(),
        host_tmpdir: None,
        private_dir: runtime.join("hcr/spawn-engine-view-test"),
        canonical_workspace: None,
        home_isolation: spawn_host::HomeIsolation::Blacklist,
        extra_mounts: Vec::new(),
        inherit_host: spawn_host::HostInheritMode::Off,
        inherit_host_paths: Vec::new(),
        engines: spawn_host::engine_view::EngineViews::default(),
        path_env: std::env::var("PATH").ok(),
        container_env: std::env::var("container").ok(),
    };
    let Ok(view) = BwrapView::new("bwrap", config) else {
        assert!(!required(), "SPAWN_REQUIRE_HERDR=1 and bwrap unavailable");
        return;
    };

    let host = session.host();
    let workspace = session.workspace();
    let name = terminal("spawn-exec_bwrap_agy");
    let agy = session.write_stub("agy", STUB_AGY);
    let wrapped = view
        .wrap(&ViewedLaunch {
            engine: "agy",
            workspace: &session.cwd().display().to_string(),
            engine_home: &engine_home.display().to_string(),
            worker_id: None,
            exit_file: None,
        })
        .expect("wrap");

    let check_cmd = format!(
        "for d in '{}' '{}' '{}' '{}' '{}'; do \
           if [ -n \"$(ls -A \"$d\" 2>/dev/null)\" ]; then echo \"LEAK:$d\"; exit 1; fi; \
         done; exec '{}'",
        home.join(".config/herdr-core").display(),
        home.join(".local/state/herdr-core").display(),
        cfg_home.join("herdr-core").display(),
        state_home.join("herdr-core").display(),
        runtime.join("hcr").display(),
        agy.display()
    );
    let mut args = wrapped[1..].to_vec();
    args.extend(["/bin/sh".into(), "-c".into(), check_cmd]);

    host.start(&Launch {
        terminal: name.clone(),
        workspace: workspace.clone(),
        cwd: session.cwd().display().to_string(),
        program: wrapped[0].clone(),
        arguments: args,
        environment: vec![
            ("AGY_STUB_START".to_string(), "idle".to_string()),
            ("TERM".to_string(), "xterm-256color".to_string()),
        ],
    })
    .expect("start bwrap agy");

    let reading = reading_within(&host, &name, &workspace, |r| {
        r.agent.as_deref() == Some("agy") && r.status == AgentStatus::Idle
    });
    assert_eq!(reading.agent.as_deref(), Some("agy"));
    host.terminate(&name, &workspace).expect("term");
}

struct PoolCleanup {
    program: String,
    sessions: Vec<String>,
    runtime: PathBuf,
}

impl Drop for PoolCleanup {
    fn drop(&mut self) {
        let xdg_runtime = self.runtime.join("hcr");
        let xdg_state = self.runtime.join("hcs");
        let xdg_config = self.runtime.join("hcc");
        for session in &self.sessions {
            let mut cmd = Command::new(&self.program);
            cmd.args(["server", "stop"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            for (name, _) in std::env::vars_os() {
                if name
                    .to_str()
                    .is_some_and(|n| n.starts_with("HERDR_") || n.starts_with("HC_"))
                {
                    cmd.env_remove(name);
                }
            }
            let _ = cmd
                .env("XDG_RUNTIME_DIR", &xdg_runtime)
                .env("XDG_STATE_HOME", &xdg_state)
                .env("XDG_CONFIG_HOME", &xdg_config)
                .env("HC_SESSION", session)
                .env("HC_SOCKET", xdg_runtime.join(format!("{session}.sock")))
                .status();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        for session in &self.sessions {
            if let Ok(host) =
                HerdrCoreHost::on_session(&self.program, session, TerminalHostEnv::default())
            {
                let host = host.with_runtime_under(self.runtime.clone());
                while matches!(host.server(), ServerSeen::Answers(_)) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }
}

fn pick_pool_terminals(pool: &spawn_host::HerdrPool, shards: usize) -> Vec<TerminalName> {
    let mut chosen = Vec::new();
    let mut per_shard = vec![0usize; shards];
    for i in 0..200 {
        let cand = TerminalName::new(format!("spawn-pool_{i}")).expect("terminal name");
        let idx = pool.shard_index(&cand);
        if per_shard[idx] < 2 {
            per_shard[idx] += 1;
            chosen.push(cand);
            if chosen.len() == shards * 2 {
                break;
            }
        }
    }
    assert!(
        per_shard.iter().all(|&count| count == 2),
        "all {shards} shards hit twice: {per_shard:?}"
    );
    chosen
}

fn scrubbed_host_env() -> TerminalHostEnv {
    TerminalHostEnv {
        scrubbed_env_keys: TerminalHostEnv::collect_scrubbed_keys(
            std::env::vars_os().map(|(k, _)| k),
        ),
        ..TerminalHostEnv::default()
    }
}

#[test]
fn herdr_pool_with_embedded_herdr_core_shards_concurrently() {
    let _serial = exclusive();
    let Some(program) = spawn_hc_program() else {
        return;
    };
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let base = format!("p{n}");
    // Keep base scratch path very short so `{base}-sN.holder.<8>.sock` stays well below 107 bytes.
    let scratch = Scratch::new_in(Path::new("/var/tmp"), &format!("shp-{n}"));
    let runtime = scratch.join("rt");
    let cwd = scratch.join("ws");
    std::fs::create_dir_all(&runtime).expect("runtime");
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).expect("0700");
    std::fs::create_dir_all(&cwd).expect("cwd");

    let stub = scratch.join("stub-shard");
    std::fs::write(
        &stub,
        "#!/bin/sh\nprintf 'SHARD-READY\\n'\nwhile IFS= read -r line; do\n  printf 'ECHO:%s\\n' \"$line\"\ndone\n",
    )
    .expect("write stub");
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("chmod stub");

    let pool = spawn_host::HerdrPool::on_core_session(&program, &base, 4, &scrubbed_host_env())
        .expect("core pool")
        .with_runtime_under(&runtime);
    assert!(pool.is_core());
    assert_eq!(pool.shard_count(), 4);
    let sessions: Vec<String> = pool
        .shard_sessions()
        .into_iter()
        .map(str::to_string)
        .collect();
    let _cleanup = PoolCleanup {
        program,
        sessions,
        runtime,
    };

    let chosen = pick_pool_terminals(&pool, 4);
    let workspace = WorkspaceId::new(format!("pool:{}", cwd.display())).expect("workspace");
    let query: Vec<(TerminalName, WorkspaceId)> = chosen
        .iter()
        .map(|t| (t.clone(), workspace.clone()))
        .collect();

    for term in &chosen {
        pool.start(&Launch {
            terminal: term.clone(),
            workspace: workspace.clone(),
            cwd: cwd.display().to_string(),
            program: stub.display().to_string(),
            arguments: Vec::new(),
            environment: vec![("TERM".to_string(), "xterm-256color".to_string())],
        })
        .expect("start on shard");
    }

    // Wait for every sharded terminal via parallel pool.runners().
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let statuses = pool.runners(&query).expect("pool runners");
        assert_eq!(statuses.len(), chosen.len());
        if statuses
            .iter()
            .zip(chosen.iter())
            .all(|((name, r), expected)| name == expected && matches!(r, Runner::Running))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for pool shards"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Verify 4 independent spawn-hc daemons exist and accept isolated writes + reads.
    assert!(matches!(pool.server(), ServerSeen::Answers(None)));
    for term in &chosen {
        let msg = format!("msg-for-{term}");
        pool.submit_text(term, &workspace, &msg)
            .expect("submit text");
    }
    for term in &chosen {
        let want = format!("ECHO:msg-for-{term}");
        let read_deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let tail = pool.capture_tail(term, &workspace, 40).expect("tail");
            if tail.contains(&want) {
                break;
            }
            assert!(
                Instant::now() < read_deadline,
                "shard terminal {term} did not echo {want:?}: {tail:?}"
            );
            std::thread::sleep(Duration::from_millis(40));
        }
    }

    for term in &chosen {
        pool.terminate(term, &workspace).expect("terminate");
    }
}
