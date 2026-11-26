//! Runner identity and process-lifecycle tests for `HerdrCoreHost` (`spawn-hc`).
//!
//! Covers:
//! - A terminal hosts its runner only while the launch owned by this host lives;
//!   an un-owned terminal or exited launch reports [`Runner::Gone`].
//! - Repeated start/terminate cycles across daemon restarts never leak launch
//!   environment files.
//! - `terminate` waits for HUP-trapping runners to finish writing before
//!   returning, and sends `SIGKILL` after the hangup timeout to HUP-ignoring ones.
//! - Terminating a gone terminal never touches another whose name it prefixes
//!   (`spawn-exec_1` vs `spawn-exec_12`).
//! - When the daemon stops on its own, `terminate` still waits for and kills
//!   the recorded session leader.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use spawn_core::id::{TerminalName, WorkspaceId};
use spawn_core::port::{Launch, Runner, ServerSeen, TerminalHost};
use spawn_host::{HerdrCoreHost, Scratch};

/// Serialize external `spawn-hc` invocations in this test binary so concurrent
/// forks never race on freshly written temporary scripts (`Text file busy`).
fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn spawn_hc_program() -> String {
    if let Some(prog) = std::env::var_os("SPAWN_HERDR_CORE_PROGRAM")
        .map(|v| v.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
    {
        return prog;
    }
    if let Ok(exe) = std::env::current_exe() {
        for dir in [exe.parent(), exe.parent().and_then(Path::parent)]
            .into_iter()
            .flatten()
        {
            let candidate = dir.join("spawn-hc");
            if candidate.is_file() {
                return candidate.display().to_string();
            }
        }
    }
    "spawn-hc".to_string()
}

struct Server {
    program: String,
    session: String,
    scratch: Scratch,
}

impl Server {
    /// An isolated `herdr-core` server with short `/var/tmp` runtime paths so
    /// Unix socket paths remain well below Linux's 107-byte `sun_path` limit.
    fn new(tag: &str) -> Self {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let session = format!("rn-{tag}-{}-{n}", std::process::id());
        let scratch = Scratch::new_in(Path::new("/var/tmp"), &format!("shcr-{n}"));
        let rt = scratch.join("run");
        for sub in ["", "hcr", "hcs", "hcc"] {
            let dir = if sub.is_empty() {
                rt.clone()
            } else {
                rt.join(sub)
            };
            fs::create_dir_all(&dir).expect("runtime");
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).expect("0700");
        }
        Self {
            program: spawn_hc_program(),
            session,
            scratch,
        }
    }

    fn runtime(&self) -> PathBuf {
        self.scratch.join("run")
    }

    fn host(&self) -> HerdrCoreHost {
        self.host_with(&self.program)
    }

    fn host_with(&self, program: &str) -> HerdrCoreHost {
        HerdrCoreHost::on_session(program, &self.session)
            .expect("valid session")
            .with_runtime_under(self.runtime())
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
            .env("HC_SESSION", &self.session)
            .env(
                "HC_SOCKET",
                rt.join("hcr").join(format!("{}.sock", self.session)),
            );
        cmd
    }

    fn stop_daemon(&self) {
        let _ = self.hc_cmd(&["server", "stop"]).status();
        let deadline = Instant::now() + Duration::from_secs(5);
        while matches!(self.host().server(), ServerSeen::Answers(_)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Abruptly SIGKILLs the `herdr-core` server and its PTY holder(s), closing
    /// the master PTYs cleanly (sending kernel SIGHUP to live PTY sessions)
    /// without invoking graceful `server stop` child teardown.
    fn crash_daemon(&self) {
        if let Ok(output) = self.hc_cmd(&["server", "status", "--json"]).output()
            && output.status.success()
            && let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&output.stdout)
        {
            let mut pids = Vec::new();
            if let Some(pid) = doc["pid"].as_u64().and_then(|p| u32::try_from(p).ok()) {
                pids.push(pid);
            }
            for holder in doc["holders"].as_array().into_iter().flatten() {
                if let Some(pid) = holder["pid"].as_u64().and_then(|p| u32::try_from(p).ok()) {
                    pids.push(pid);
                }
            }
            for pid in pids {
                if let Some(raw) = rustix::process::Pid::from_raw(pid.cast_signed()) {
                    let _ = rustix::process::kill_process(raw, rustix::process::Signal::KILL);
                }
            }
        }
        let sock = self
            .runtime()
            .join("hcr")
            .join(format!("{}.sock", self.session));
        let _ = fs::remove_file(sock);
        let deadline = Instant::now() + Duration::from_secs(5);
        while matches!(self.host().server(), ServerSeen::Answers(_)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn write_script(&self, name: &str, body: &str) -> String {
        let target = self.scratch.join(name);
        let temporary = self.scratch.join(format!("{name}.tmp"));
        {
            let mut file = fs::File::create(&temporary).expect("create");
            file.write_all(body.as_bytes()).expect("write");
            file.sync_all().expect("sync");
        }
        let mut permissions = fs::metadata(&temporary).expect("stat").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&temporary, permissions).expect("chmod");
        fs::rename(&temporary, &target).expect("rename");
        target.display().to_string()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop_daemon();
    }
}

fn terminal() -> TerminalName {
    TerminalName::new("spawn-exec_1").expect("valid terminal")
}

fn workspace() -> WorkspaceId {
    WorkspaceId::new("agent_one:/tmp").expect("valid workspace")
}

fn launch_named(
    terminal: TerminalName,
    program: &str,
    arguments: &[&str],
    environment: &[(&str, &str)],
) -> Launch {
    Launch {
        terminal,
        workspace: workspace(),
        cwd: "/tmp".to_string(),
        program: program.to_string(),
        arguments: arguments.iter().map(|word| (*word).to_string()).collect(),
        environment: environment
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect(),
    }
}

fn launch(program: &str, arguments: &[&str], environment: &[(&str, &str)]) -> Launch {
    launch_named(terminal(), program, arguments, environment)
}

fn settled(host: &HerdrCoreHost, limit: Duration) -> Runner {
    let deadline = Instant::now() + limit;
    loop {
        let runner = host.runner(&terminal(), &workspace()).expect("runner");
        if runner != Runner::Running || Instant::now() > deadline {
            return runner;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn environment_files(server: &Server) -> Vec<String> {
    fn collect(dir: &Path, prefix: &str, found: &mut std::collections::BTreeSet<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                collect(&path, prefix, found);
            } else if let Some(name) = entry.file_name().to_str()
                && name.starts_with(prefix)
            {
                found.insert(name.to_string());
            }
        }
    }
    let prefix = format!("spawn-launch-spawn-exec_1-{}-", std::process::id());
    let mut found = std::collections::BTreeSet::new();
    collect(&server.runtime(), &prefix, &mut found);
    found.into_iter().collect()
}

fn until_running(marker: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !running_with(marker) {
        assert!(Instant::now() < deadline, "{marker} never ran");
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(Duration::from_millis(200));
}

fn running_with(marker: &str) -> bool {
    fs::read_dir("/proc")
        .expect("/proc")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().parse::<u32>().is_ok())
        .any(|entry| {
            let stat = fs::read_to_string(entry.path().join("stat")).unwrap_or_default();
            let zombie = stat
                .rfind(')')
                .is_some_and(|at| stat[at + 1..].trim_start().starts_with('Z'));
            !zombie
                && fs::read(entry.path().join("cmdline"))
                    .is_ok_and(|line| String::from_utf8_lossy(&line).contains(marker))
        })
}

#[test]
fn a_terminal_hosts_its_runner_only_while_the_launch_lives_in_it() {
    let _exclusive = exclusive();
    let server = Server::new("runner");
    let host = server.host();
    assert_eq!(
        host.runner(&terminal(), &workspace()).expect("runner"),
        Runner::Gone,
        "no server, no terminal"
    );

    // Launched by this host, with an environment and without one.
    let marker = format!("spawn-runner-live-{}", std::process::id());
    for environment in [&[("SPAWN_EXECUTION_ID", "exec_1")][..], &[]] {
        host.start(&launch(
            "/bin/sh",
            &["-c", "while :; do sleep 0.05; done", &marker],
            environment,
        ))
        .expect("start");
        assert_eq!(
            host.runner(&terminal(), &workspace()).expect("runner"),
            Runner::Running
        );
        assert!(host.is_alive(&terminal(), &workspace()).expect("alive"));
        until_running(&marker);
        host.terminate(&terminal(), &workspace()).expect("kill");
        assert_eq!(
            host.runner(&terminal(), &workspace()).expect("runner"),
            Runner::Gone
        );
    }

    // A terminal of the same label started outside `HerdrCoreHost` has no host
    // record, so it is not treated as a running Spawn runner and `terminate`
    // closes it by label.
    let status = server
        .hc_cmd(&[
            "run",
            "--bg",
            "--no-notify",
            "--label",
            &format!("spawn_owner={}", server.session),
            "--label",
            "spawn_terminal=spawn-exec_1",
            "--",
            "/bin/cat",
        ])
        .status()
        .expect("spawn-hc run");
    assert!(status.success(), "external spawn-hc run failed");
    assert_eq!(
        host.runner(&terminal(), &workspace()).expect("runner"),
        Runner::Gone
    );
    // Clean up the externally launched terminal by stopping or killing via spawn-hc.
    if let Ok(output) = server
        .hc_cmd(&["ls", "--label", "spawn_terminal=spawn-exec_1", "--json"])
        .output()
        && output.status.success()
        && let Ok(doc) = serde_json::from_slice::<serde_json::Value>(&output.stdout)
        && let Some(addr) = doc["terminals"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|item| item["address"].as_str())
    {
        let _ = server.hc_cmd(&["kill", addr]).status();
    }

    // Once its launched command exits (`exit 3`), the runner is gone.
    host.start(&launch("/bin/sh", &["-c", "exit 3"], &[("A", "1")]))
        .expect("start short exit");
    assert_eq!(settled(&host, Duration::from_secs(5)), Runner::Gone);
    assert!(
        !host.is_alive(&terminal(), &workspace()).expect("is_alive"),
        "exited launch is no longer live"
    );
}

#[test]
fn a_launch_starts_while_the_server_shuts_down_after_its_last_terminal() {
    let _exclusive = exclusive();
    let server = Server::new("restart");
    let host = server.host();
    let before = environment_files(&server);
    for round in 0..12 {
        host.start(&launch("/bin/cat", &[], &[]))
            .unwrap_or_else(|error| panic!("start {round}: {error:?}"));
        assert_eq!(
            host.runner(&terminal(), &workspace()).expect("runner"),
            Runner::Running
        );
        host.terminate(&terminal(), &workspace()).expect("kill");
        if round % 4 == 3 {
            server.stop_daemon();
        }
    }
    assert_eq!(
        environment_files(&server),
        before,
        "launch environment files leaked after repeated restarts"
    );
}

#[test]
fn a_launch_recovers_after_stale_socket_and_cleans_up_on_daemon_failure() {
    let _exclusive = exclusive();
    let server = Server::new("stale");
    let host = server.host();
    let before = environment_files(&server);

    // Leave a stale Unix socket file where no daemon listens; starting a
    // launch removes it and brings up the daemon cleanly.
    let sock = server
        .runtime()
        .join("hcr")
        .join(format!("{}.sock", server.session));
    let _ = fs::remove_file(&sock);
    fs::write(&sock, b"").expect("stale socket placeholder");
    host.start(&launch("/bin/cat", &[], &[("A", "1")]))
        .expect("start recovers past stale socket");
    assert_eq!(
        host.runner(&terminal(), &workspace()).expect("runner"),
        Runner::Running
    );
    host.terminate(&terminal(), &workspace()).expect("kill");
    assert_eq!(environment_files(&server), before);

    // A broken `spawn-hc` binary fails loudly and cleans up the launch script.
    let broken_prog = server.write_script(
        "broken-hc",
        "#!/bin/sh\necho 'synthetic herdr-core refusal' >&2\nexit 1\n",
    );
    let broken_host = server.host_with(&broken_prog);
    server.stop_daemon();
    let err = broken_host
        .start(&launch("/bin/cat", &[], &[("SECRET", "x")]))
        .expect_err("broken daemon refuses start");
    assert!(!err.0.is_empty());
    assert_eq!(
        environment_files(&server),
        before,
        "launch environment file leaked after failed start"
    );
}

#[test]
fn terminate_returns_once_the_runner_and_what_it_started_are_gone() {
    let _exclusive = exclusive();
    let server = Server::new("terminate");
    let host = server
        .host()
        .with_terminate_waits(Duration::from_secs(5), Duration::from_secs(2));
    let late = server.scratch.join("late");
    let marker = format!("spawn-dies-writing-{}", std::process::id());
    let script = format!(
        "trap 'sleep 0.3; echo late >> {}; exit 0' TERM HUP; while :; do sleep 0.05; done",
        late.display()
    );
    host.start(&launch("/bin/sh", &["-c", &script, &marker], &[]))
        .expect("start");
    until_running(&marker);
    host.terminate(&terminal(), &workspace()).expect("kill");
    assert_eq!(
        fs::read_to_string(&late).unwrap_or_default(),
        "late\n",
        "terminate returned before the runner wrote as it died"
    );
    assert!(!running_with(&marker), "the runner is gone");

    // One that ignores hangup/term: `wait_sessions_gone` escalates to SIGKILL.
    let host = host.with_terminate_waits(Duration::from_millis(300), Duration::from_secs(2));
    let marker = format!("spawn-ignores-hangup-{}", std::process::id());
    host.start(&launch(
        "/bin/sh",
        &[
            "-c",
            "trap '' TERM HUP; while :; do sleep 0.05; done",
            &marker,
        ],
        &[],
    ))
    .expect("start");
    until_running(&marker);
    let started = Instant::now();
    host.terminate(&terminal(), &workspace()).expect("killed");
    assert!(started.elapsed() >= Duration::from_millis(300), "it waited");
    assert!(!running_with(&marker), "killed, and gone");
    assert_eq!(
        host.runner(&terminal(), &workspace()).expect("runner"),
        Runner::Gone
    );
}

#[test]
fn terminating_a_gone_terminal_leaves_one_whose_name_it_prefixes() {
    let _exclusive = exclusive();
    let server = Server::new("prefix");
    let exec_12 = TerminalName::new("spawn-exec_12").expect("valid");
    let marker = format!("spawn-exec-12-lives-{}", std::process::id());
    let host = server
        .host()
        .with_terminate_waits(Duration::from_millis(300), Duration::from_millis(300));
    host.start(&launch_named(
        exec_12.clone(),
        "/bin/sh",
        &["-c", "while :; do sleep 0.05; done", &marker],
        &[],
    ))
    .expect("start spawn-exec_12");
    until_running(&marker);
    assert_eq!(
        host.runner(&terminal(), &workspace()).expect("runner"),
        Runner::Gone,
        "spawn-exec_1 is not there"
    );
    host.terminate(&terminal(), &workspace())
        .expect("terminating a gone terminal is harmless");
    std::thread::sleep(Duration::from_millis(700));
    assert!(running_with(&marker), "spawn-exec_12's runner still runs");
    host.terminate(&exec_12, &workspace())
        .expect("clean up spawn-exec_12");
}

#[test]
fn terminate_waits_for_a_runner_whose_session_vanished_on_its_own() {
    let _exclusive = exclusive();
    let server = Server::new("vanished");
    let host = server
        .host()
        .with_terminate_waits(Duration::from_secs(5), Duration::from_secs(2));

    // Daemon & holder vanish (`herdr-core` starts children with SIGHUP ignored):
    // `terminate` sends SIGTERM to the recorded session leader and waits while
    // the runner writes as it dies 0.4 s later.
    let late = server.scratch.join("late");
    let marker = format!("spawn-vanished-writing-{}", std::process::id());
    let script = format!(
        "trap 'sleep 0.4; echo late >> {}; exit 0' HUP TERM; while :; do sleep 0.05; done",
        late.display()
    );
    host.start(&launch("/bin/sh", &["-c", &script, &marker], &[]))
        .expect("start");
    until_running(&marker);
    server.crash_daemon();
    assert_eq!(
        host.runner(&terminal(), &workspace()).expect("runner"),
        Runner::Gone,
        "the daemon vanished"
    );
    host.terminate(&terminal(), &workspace())
        .expect("terminate");
    assert_eq!(
        fs::read_to_string(&late).unwrap_or_default(),
        "late\n",
        "terminate returned before the runner of a vanished session wrote as it died"
    );
    assert!(!running_with(&marker), "the runner is gone");

    // Daemon & holder vanish while the runner ignores HUP/TERM: `terminate`
    // waits 300 ms and SIGKILLs the recorded session leader.
    let host = host.with_terminate_waits(Duration::from_millis(300), Duration::from_secs(2));
    let marker = format!("spawn-vanished-ignores-{}", std::process::id());
    host.start(&launch(
        "/bin/sh",
        &[
            "-c",
            "trap '' TERM HUP; while :; do sleep 0.05; done",
            &marker,
        ],
        &[],
    ))
    .expect("start");
    until_running(&marker);
    server.crash_daemon();
    std::thread::sleep(Duration::from_millis(200));
    assert!(running_with(&marker), "it outlived the daemon crash");
    let started = Instant::now();
    host.terminate(&terminal(), &workspace()).expect("killed");
    assert!(started.elapsed() >= Duration::from_millis(300), "it waited");
    assert!(!running_with(&marker), "killed, and gone");
    let started = Instant::now();
    host.terminate(&terminal(), &workspace()).expect("again");
    assert!(
        started.elapsed() < Duration::from_millis(300),
        "nothing to wait for"
    );
}
