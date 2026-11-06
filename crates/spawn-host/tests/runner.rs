//! Issue #15, item 2: a terminal of an execution's name is not its runner.
//!
//! tmux keeps a session after its runner is gone when the pane stays dead
//! (`remain-on-exit`), and a terminal manager (tmux-resurrect, continuum)
//! restores a session of the same name after a restart, at a shell prompt.
//! `has-session` says yes to both; `runner` tells them from the runner this
//! host launched. tmux is real, on a server of the test's own that reads no
//! configuration.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use spawn_core::id::{TerminalName, WorkspaceId};
use spawn_core::port::{Launch, Runner, TerminalHost};
use spawn_host::TmuxHost;

struct Server {
    directory: PathBuf,
    socket: String,
}

impl Server {
    fn new() -> Self {
        let socket = format!("spawn-runner-test-{}", std::process::id());
        let directory = std::env::temp_dir().join(&socket);
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).expect("scratch");
        Self { directory, socket }
    }

    /// The host, through a `tmux` that runs on this server.
    fn host(&self) -> TmuxHost {
        let target = self.directory.join("tmux");
        let temporary = self.directory.join("tmux.tmp");
        {
            let mut file = fs::File::create(&temporary).expect("create");
            write!(
                file,
                "#!/bin/sh\nexec tmux -L {} -f /dev/null \"$@\"\n",
                self.socket
            )
            .expect("write");
            file.sync_all().expect("sync");
        }
        let mut permissions = fs::metadata(&temporary).expect("stat").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&temporary, permissions).expect("chmod");
        fs::rename(&temporary, &target).expect("rename");
        TmuxHost::local(&target.display().to_string())
    }

    /// tmux on this server, directly: what a terminal manager does.
    fn tmux(&self, arguments: &[&str]) {
        let status = Command::new("tmux")
            .args(["-L", &self.socket, "-f", "/dev/null"])
            .args(arguments)
            .status()
            .expect("tmux is installed");
        assert!(status.success(), "tmux {arguments:?}");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.socket, "kill-server"])
            .stderr(std::process::Stdio::null())
            .status();
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn terminal() -> TerminalName {
    TerminalName::new("spawn-exec_1").expect("valid terminal")
}

fn workspace() -> WorkspaceId {
    WorkspaceId::new("agent_one:/tmp").expect("valid workspace")
}

fn launch(program: &str, arguments: &[&str], environment: &[(&str, &str)]) -> Launch {
    Launch {
        terminal: terminal(),
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

/// What `runner` answers once it stops answering `Running` or `limit`
/// passes.
fn settled(host: &TmuxHost, limit: Duration) -> Runner {
    let deadline = Instant::now() + limit;
    loop {
        let runner = host.runner(&terminal(), &workspace()).expect("runner");
        if runner != Runner::Running || Instant::now() > deadline {
            return runner;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_terminal_hosts_its_runner_only_while_the_launch_lives_in_it() {
    let server = Server::new();
    let host = server.host();
    assert_eq!(
        host.runner(&terminal(), &workspace()).expect("runner"),
        Runner::Gone,
        "no server, no terminal"
    );

    // Launched by this host, with an environment and without one.
    for environment in [&[("SPAWN_EXECUTION_ID", "exec_1")][..], &[]] {
        host.start(&launch("/bin/cat", &[], environment))
            .expect("start");
        assert_eq!(
            host.runner(&terminal(), &workspace()).expect("runner"),
            Runner::Running
        );
        assert!(host.is_alive(&terminal(), &workspace()).expect("alive"));
        host.terminate(&terminal(), &workspace()).expect("kill");
        assert_eq!(
            host.runner(&terminal(), &workspace()).expect("runner"),
            Runner::Gone
        );
    }

    // A session of the same name restored at a shell prompt: tmux has it,
    // and it hosts no runner.
    server.tmux(&[
        "new-session",
        "-d",
        "-s",
        "spawn-exec_1",
        "-x",
        "80",
        "-y",
        "20",
    ]);
    assert!(host.is_alive(&terminal(), &workspace()).expect("alive"));
    let Runner::Untracked(seen) = host.runner(&terminal(), &workspace()).expect("runner") else {
        panic!("a restored shell hosts no runner");
    };
    assert!(seen.contains("no command"), "{seen}");
    host.terminate(&terminal(), &workspace()).expect("kill");

    // A dead pane kept by remain-on-exit: the runner exited (3) and the
    // session stays.
    server.tmux(&["new-session", "-d", "-s", "keep"]);
    server.tmux(&["set-option", "-g", "remain-on-exit", "on"]);
    host.start(&launch("/bin/sh", &["-c", "exit 3"], &[("A", "1")]))
        .expect("start");
    let Runner::Untracked(seen) = settled(&host, Duration::from_secs(5)) else {
        panic!("a dead pane hosts no runner");
    };
    assert!(seen.contains("dead (exit status 3)"), "{seen}");
    assert!(host.is_alive(&terminal(), &workspace()).expect("alive"));
}
