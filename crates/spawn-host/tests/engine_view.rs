//! The filesystem view of an engine without deny rules (decision C2, N6),
//! against a real `bwrap`.
//!
//! Each test builds a made-up home in a scratch outside `/tmp` (under the
//! test target's own directory: `/tmp` is masked whole in the view, so a
//! home below it would hide the masks this test checks), with a canary in
//! every place the view hides, then runs benign shell commands inside the
//! view. **No engine is ever run here**: never `agy`, on any host (a host
//! with `/run/keep` holds live credentials, decision C3). The Herdr test's
//! agent is a shell stub named `claude`.
//!
//! A host with no working `bwrap` skips these tests (and says so), unless
//! `SPAWN_REQUIRE_HERDR=1`, which makes a missing `bwrap` (or `herdr`, for
//! the detection test) a failure.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use spawn_core::id::{TerminalName, WorkspaceId};
use spawn_core::port::{EngineView, Launch, ServerSeen, TerminalHost, ViewedLaunch};
use spawn_host::engine_view::{HOST_DIRECTORIES, Mask, TMP_FALLBACK};
use spawn_host::{BwrapView, HerdrHost, Scratch, ViewConfig};

fn required() -> bool {
    std::env::var("SPAWN_REQUIRE_HERDR").is_ok_and(|value| value == "1")
}

/// One test at a time: the Herdr one starts a server of its own.
fn exclusive() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("parent");
    std::fs::write(path, text).expect("write");
}

/// A made-up home with a canary in every masked place, a workspace, a
/// `TMPDIR` and agy's home below the masked `~/.gemini`.
struct Fixture {
    scratch: Scratch,
}

impl Fixture {
    fn new() -> Self {
        let parent = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        std::fs::create_dir_all(&parent).expect("target tmp");
        let scratch = Scratch::new_in(&parent, "spawn-view");
        let fixture = Self { scratch };
        let home = fixture.home();
        for dir in [
            ".claude",
            ".gemini",
            ".config/antigravity",
            ".codex",
            ".hermes",
            ".ssh",
            ".gnupg",
            ".config/gh",
            ".config/anthropic",
            ".config/spawn",
            ".config/cloudflare",
            ".config/observe3-private",
            ".config/herdr/sessions/spawn",
            "cfg/herdr",
        ] {
            write(&home.join(dir).join("canary"), "secret");
        }
        write(&home.join(".gemini/oauth_creds.json"), "secret");
        write(&home.join(".gemini/antigravity-cli/brain/old"), "mine");
        for file in [".claude.json", ".pgpass", ".git-credentials"] {
            write(&home.join(file), "secret");
        }
        let live = home.join(".local/share/spawn-rsi");
        for file in ["spawn.env", "spawn.env.bak-relay", "env.sh", "typesafe.env"] {
            write(&live.join(file), "SPAWN_DATABASE_URL=secret");
        }
        write(&live.join("bin/kept"), "visible");
        // Canaries behind symbolic links: the view masks what they resolve
        // to (a tmpfs over a link aborts bwrap).
        write(&fixture.scratch.join("real-aws/credentials"), "secret");
        std::os::unix::fs::symlink(fixture.scratch.join("real-aws"), home.join(".aws"))
            .expect("link");
        write(&fixture.scratch.join("real-netrc"), "secret");
        std::os::unix::fs::symlink(fixture.scratch.join("real-netrc"), home.join(".netrc"))
            .expect("link");
        write(&fixture.runtime().join("credentials/keep"), "secret");
        write(&fixture.keep().join("api-key"), "secret");
        std::fs::create_dir_all(fixture.workspace()).expect("ws");
        std::fs::create_dir_all(fixture.tmpdir()).expect("tmp");
        fixture
    }

    fn home(&self) -> PathBuf {
        self.scratch.join("home")
    }

    fn runtime(&self) -> PathBuf {
        self.scratch.join("run")
    }

    /// A stand-in for `/run/keep`, beside the real one (masked too, when
    /// the host has it: never read here).
    fn keep(&self) -> PathBuf {
        self.scratch.join("keep")
    }

    fn workspace(&self) -> PathBuf {
        self.scratch.join("ws")
    }

    fn tmpdir(&self) -> PathBuf {
        self.scratch.join("tmp")
    }

    fn engine_home(&self) -> PathBuf {
        self.home().join(".gemini/antigravity-cli")
    }

    fn config(&self) -> ViewConfig {
        let mut host_directories: Vec<PathBuf> =
            HOST_DIRECTORIES.iter().map(PathBuf::from).collect();
        host_directories.push(self.keep());
        ViewConfig {
            home: self.home(),
            xdg_config_home: Some(self.home().join("cfg")),
            runtime_dir: self.runtime(),
            host_directories,
            loop_dirs: vec![self.home().join(".local/share/spawn-rsi")],
            extra_files: Vec::new(),
            tmpdir: None,
            private_dir: self.scratch.join("private"),
        }
    }

    fn launch(&self) -> (String, String, String) {
        (
            self.workspace().display().to_string(),
            self.engine_home().display().to_string(),
            self.tmpdir().display().to_string(),
        )
    }
}

/// The view, or `None` (said) on a host with no working `bwrap`.
fn view(config: ViewConfig) -> Option<BwrapView> {
    match BwrapView::new("bwrap", config) {
        Ok(view) => Some(view),
        Err(why) => {
            assert!(!required(), "SPAWN_REQUIRE_HERDR=1 and no bwrap: {why}");
            eprintln!("skipped: {why}");
            None
        }
    }
}

/// `script` run by `/bin/sh` inside the view `words` make.
fn inside(words: &[String], script: &str) -> Output {
    Command::new(&words[0])
        .args(&words[1..])
        .args(["/bin/sh", "-c", script])
        .stdin(Stdio::null())
        .output()
        .expect("bwrap")
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// `script` inside the view: it must exit 0; its standard output.
fn ok(words: &[String], script: &str) -> String {
    let output = inside(words, script);
    assert!(output.status.success(), "{script}: {}", text(&output));
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// `script` inside the view must fail; what it said.
fn fails(words: &[String], script: &str) -> String {
    let output = inside(words, script);
    assert!(!output.status.success(), "{script} succeeded: {}", text(&output));
    text(&output)
}

/// Every masked place reads empty, every masked file reads 0 bytes and
/// refuses a write (read-only); `/` and the home are read-only; the
/// workspace, agy's home (re-bound inside the masked `~/.gemini`) and
/// `TMPDIR` are writable and written through to the host; no host process
/// can be signalled, and neither `/tmp`'s sockets nor Herdr's are there.
#[allow(clippy::too_many_lines)] // one view, checked place by place
#[test]
fn inside_the_view_the_masks_are_empty_and_only_the_three_binds_write() {
    let _serial = exclusive();
    let fixture = Fixture::new();
    let Some(view) = view(fixture.config()) else {
        return;
    };
    let (workspace, engine_home, tmpdir) = fixture.launch();
    let words = view
        .wrap(&ViewedLaunch {
            engine: "agy",
            workspace: &workspace,
            engine_home: &engine_home,
            tmpdir: Some(&tmpdir),
        })
        .expect("wrap");
    let home = fixture.home();
    let h = |rest: &str| home.join(rest).display().to_string();

    // Masked directories list empty: the made-up ones, the symlinked one,
    // the runtime directory, the Herdr session directory (both), and the
    // stand-in for /run/keep.
    for dir in [
        h(".claude"),
        h(".config/antigravity"),
        h(".codex"),
        h(".hermes"),
        h(".ssh"),
        h(".gnupg"),
        h(".config/gh"),
        h(".aws"),
        h(".config/anthropic"),
        h(".config/spawn"),
        h(".config/cloudflare"),
        h(".config/observe3-private"),
        h(".config/herdr"),
        h("cfg/herdr"),
        fixture.runtime().display().to_string(),
        fixture.keep().display().to_string(),
    ] {
        assert_eq!(ok(&words, &format!("ls -A '{dir}'")), "", "{dir}");
    }
    // ~/.gemini holds only agy's own home, which keeps its contents.
    assert_eq!(ok(&words, &format!("ls -A '{}'", h(".gemini"))), "antigravity-cli");
    assert_eq!(ok(&words, &format!("cat '{engine_home}/brain/old'")), "mine");
    fails(&words, &format!("cat '{}'", h(".gemini/oauth_creds.json")));
    // The real /run/keep and /run/keep-test, when the host has them, are
    // empty inside (they are never read outside).
    for dir in ["/run/keep", "/run/keep-test"] {
        assert_eq!(
            ok(&words, &format!("if [ -d {dir} ]; then ls -A {dir}; fi")),
            "",
            "{dir}"
        );
    }
    // /tmp is an empty tmpfs: no tmux socket of any user, nothing of the
    // host's.
    let uid = ok(&words, "id -u");
    fails(&words, &format!("ls -A /tmp/tmux-{uid}"));
    assert_eq!(ok(&words, "ls -A /tmp"), "");
    // Herdr's session socket place is gone.
    fails(&words, &format!("ls '{}'", h(".config/herdr/sessions/spawn")));

    // Masked regular files read 0 bytes and refuse a write: the loop's
    // environment files, the home's credential files, the symlinked one.
    for file in [
        h(".local/share/spawn-rsi/spawn.env"),
        h(".local/share/spawn-rsi/spawn.env.bak-relay"),
        h(".local/share/spawn-rsi/env.sh"),
        h(".local/share/spawn-rsi/typesafe.env"),
        h(".claude.json"),
        h(".pgpass"),
        h(".netrc"),
        h(".git-credentials"),
    ] {
        assert_eq!(ok(&words, &format!("wc -c < '{file}'")), "0", "{file}");
        let said = fails(&words, &format!("echo x > '{file}'"));
        assert!(said.contains("Read-only file system"), "{file}: {said}");
    }
    assert_eq!(
        ok(&words, &format!("cat '{}'", h(".local/share/spawn-rsi/bin/kept"))),
        "visible"
    );

    // / and the home are read-only.
    for path in ["/spawn-view-probe".to_string(), h("probe")] {
        let said = fails(&words, &format!("touch '{path}'"));
        assert!(said.contains("Read-only file system"), "{path}: {said}");
    }
    // The workspace, agy's home and TMPDIR write, through to the host.
    ok(
        &words,
        &format!(
            "echo ws > '{workspace}/w' && echo home > '{engine_home}/h' && \
             echo tmp > \"$TMPDIR/t\" && [ \"$TMPDIR\" = '{tmpdir}' ] && [ \"$PWD\" = '{workspace}' ]"
        ),
    );
    assert_eq!(std::fs::read_to_string(fixture.workspace().join("w")).expect("w"), "ws\n");
    assert_eq!(std::fs::read_to_string(fixture.engine_home().join("h")).expect("h"), "home\n");
    assert_eq!(std::fs::read_to_string(fixture.tmpdir().join("t")).expect("t"), "tmp\n");
    // Nothing reached the host's masked places.
    assert!(!home.join(".claude/w").exists());
    assert_eq!(std::fs::read_to_string(home.join(".claude.json")).expect("json"), "secret");

    // No host process can be signalled: its own pid namespace.
    let mut sleeper = Command::new("sleep")
        .arg("30")
        .stdin(Stdio::null())
        .spawn()
        .expect("sleep");
    let said = fails(&words, &format!("kill -0 {}", sleeper.id()));
    let _ = sleeper.kill();
    let _ = sleeper.wait();
    assert!(said.contains("No such process"), "{said}");
}

/// A target that is missing, or neither a directory nor a regular file (a
/// socket), is skipped and said, and the view still starts.
#[test]
fn missing_and_odd_targets_are_skipped_and_said_without_aborting() {
    let _serial = exclusive();
    let fixture = Fixture::new();
    let socket_path = fixture.scratch.join("odd.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket_path).expect("socket");
    let missing = fixture.scratch.join("missing.env");
    let mut config = fixture.config();
    config.extra_files = vec![missing.clone(), socket_path.clone()];
    config.host_directories.push(fixture.scratch.join("no-such-dir"));
    let Some(view) = view(config) else {
        return;
    };
    let (workspace, engine_home, tmpdir) = fixture.launch();
    let plan = view
        .plan(&ViewedLaunch {
            engine: "agy",
            workspace: &workspace,
            engine_home: &engine_home,
            tmpdir: Some(&tmpdir),
        })
        .expect("plan");
    let why = |path: &Path| {
        plan.skipped
            .iter()
            .find(|(skipped, _)| skipped == path)
            .map(|(_, why)| why.clone())
    };
    assert_eq!(why(&missing).as_deref(), Some("missing"));
    assert!(
        why(&socket_path).is_some_and(|why| why.contains("neither a directory nor a regular file")),
        "{:?}",
        plan.skipped
    );
    assert_eq!(
        why(&fixture.scratch.join("no-such-dir")).as_deref(),
        Some("missing")
    );
    assert_eq!(ok(&plan.words, "echo started"), "started");
}

/// The argv invariants: the pid namespace, never a new session, never the
/// network or every namespace unshared nor the program as pid 1; every
/// mask before the first writable bind, and the binds in order
/// (workspace, engine home, TMPDIR), each at its own path; resolved paths
/// only (a link is masked by its target); `--` last.
#[test]
fn the_argv_keeps_its_invariants() {
    let _serial = exclusive();
    let fixture = Fixture::new();
    let Some(view) = view(fixture.config()) else {
        return;
    };
    let (workspace, engine_home, tmpdir) = fixture.launch();
    let plan = view
        .plan(&ViewedLaunch {
            engine: "agy",
            workspace: &workspace,
            engine_home: &engine_home,
            tmpdir: Some(&tmpdir),
        })
        .expect("plan");
    let words = &plan.words;
    assert_eq!(words.last().map(String::as_str), Some("--"));
    assert!(words[0].ends_with("bwrap"), "{words:?}");
    assert_eq!(
        words[1..10],
        [
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--die-with-parent",
            "--unshare-pid"
        ]
    );
    for never in ["--new-session", "--unshare-net", "--unshare-all", "--as-pid-1"] {
        assert!(!words.iter().any(|word| word == never), "{never}: {words:?}");
    }
    let first_bind = words.iter().position(|word| word == "--bind").expect("bind");
    let last_mask = words
        .iter()
        .rposition(|word| word == "--tmpfs" || word == "--ro-bind")
        .expect("mask");
    assert!(last_mask < first_bind, "{words:?}");
    let binds: Vec<(&str, &str)> = words
        .windows(3)
        .filter(|three| three[0] == "--bind")
        .map(|three| (three[1].as_str(), three[2].as_str()))
        .collect();
    assert_eq!(
        binds,
        vec![
            (workspace.as_str(), workspace.as_str()),
            (engine_home.as_str(), engine_home.as_str()),
            (tmpdir.as_str(), tmpdir.as_str()),
        ]
    );
    // The symlinked canaries are masked at what they resolve to.
    let aws = fixture.scratch.join("real-aws");
    let netrc = fixture.scratch.join("real-netrc");
    assert!(plan.masks.contains(&Mask::Directory(aws)), "{:?}", plan.masks);
    assert!(plan.masks.contains(&Mask::File(netrc)), "{:?}", plan.masks);
    assert!(!plan.masks.iter().any(|mask| matches!(
        mask,
        Mask::Directory(path) | Mask::File(path) if path.ends_with(".aws") || path.ends_with(".netrc")
    )));
    // agy's own home is never masked; ~/.gemini is.
    assert!(!plan.masks.contains(&Mask::Directory(fixture.engine_home())));
    assert!(plan.masks.contains(&Mask::Directory(fixture.home().join(".gemini"))));
    // Every regular-file mask binds the one private empty 0444 file, never
    // /dev/null.
    let sources: Vec<&str> = words
        .windows(3)
        .skip(2)
        .filter(|three| three[0] == "--ro-bind")
        .map(|three| three[1].as_str())
        .collect();
    assert!(!sources.is_empty());
    assert!(sources.iter().all(|source| *source == sources[0]), "{sources:?}");
    assert_ne!(sources[0], "/dev/null");
    let meta = std::fs::metadata(sources[0]).expect("empty");
    assert_eq!(meta.len(), 0);
    assert_eq!(meta.permissions().mode() & 0o777, 0o444);
}

/// A workspace or an engine home whose bind would uncover a mask is
/// refused; a TMPDIR that would is passed over for one below the engine's
/// home (said).
#[test]
fn a_bind_that_would_uncover_a_mask_is_refused_or_passed_over() {
    let _serial = exclusive();
    let fixture = Fixture::new();
    let Some(view) = view(fixture.config()) else {
        return;
    };
    let (workspace, engine_home, _) = fixture.launch();
    let home = fixture.home().display().to_string();
    let refused = view
        .plan(&ViewedLaunch {
            engine: "agy",
            workspace: &home,
            engine_home: &engine_home,
            tmpdir: None,
        })
        .expect_err("a workspace holding masks");
    assert!(refused.contains("which the view masks"), "{refused}");
    let refused = view
        .plan(&ViewedLaunch {
            engine: "agy",
            workspace: &workspace,
            engine_home: &home,
            tmpdir: None,
        })
        .expect_err("an engine home holding the user's home");
    assert!(refused.contains("holds the user's home"), "{refused}");
    let plan = view
        .plan(&ViewedLaunch {
            engine: "agy",
            workspace: &workspace,
            engine_home: &engine_home,
            tmpdir: Some("/tmp"),
        })
        .expect("plan");
    assert_eq!(plan.tmpdir, fixture.engine_home().join(TMP_FALLBACK));
    assert!(
        plan.skipped
            .iter()
            .any(|(path, why)| path == Path::new("/tmp") && why.contains("TMPDIR holds")),
        "{:?}",
        plan.skipped
    );
    ok(&plan.words, "touch \"$TMPDIR/x\" && [ ! -e /tmp/x ]");
}

/// A stand-in Claude Code: a shell script named `claude`, which Herdr
/// detects by the process's name; it shows a prompt and waits.
const STUB_CLAUDE: &str = "#!/bin/sh\nprintf '> \\r\\n'\nexec sleep 60\n";

/// The installed `herdr`, if any answers `--version`.
fn herdr_program() -> Option<String> {
    let home = std::env::var_os("HOME")
        .map(|home| Path::new(&home).join(".local/bin/herdr").display().to_string());
    home.into_iter().chain(["herdr".to_string()]).find(|program| {
        Command::new(program)
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .is_ok_and(|output| output.status.success())
    })
}

/// A Herdr session of the test's own, stopped and deleted on drop.
struct HerdrSession {
    program: String,
    name: String,
    runtime: PathBuf,
}

impl HerdrSession {
    fn host(&self) -> HerdrHost {
        HerdrHost::on_session(&self.program, &self.name)
            .expect("a valid session")
            .with_runtime_under(self.runtime.clone())
            .with_terminate_waits(Duration::from_secs(3), Duration::from_secs(2))
    }

    fn herdr(&self, arguments: &[&str]) {
        let mut command = Command::new(&self.program);
        command
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

impl Drop for HerdrSession {
    fn drop(&mut self) {
        self.herdr(&["--session", &self.name, "server", "stop"]);
        let deadline = Instant::now() + Duration::from_secs(5);
        while matches!(self.host().server(), ServerSeen::Answers(_)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        self.herdr(&["session", "delete", &self.name]);
    }
}

/// Herdr still detects an agent started inside the view (no new session,
/// its own pid namespace), and the view hides Herdr's own session socket:
/// a client inside cannot reach Herdr to open a pane outside the view.
#[test]
fn herdr_detects_an_agent_inside_the_view_that_cannot_reach_its_socket() {
    let _serial = exclusive();
    let Some(program) = herdr_program() else {
        assert!(!required(), "SPAWN_REQUIRE_HERDR=1 and no herdr is installed");
        eprintln!("skipped: no herdr installed");
        return;
    };
    let fixture = Fixture::new();
    // The real Herdr session directory of this user is masked too.
    let real_home = PathBuf::from(std::env::var("HOME").expect("HOME"));
    let mut config = fixture.config();
    config.host_directories.push(real_home.join(".config/herdr"));
    let Some(view) = view(config) else {
        return;
    };
    let runtime = fixture.scratch.join("herdr-run");
    std::fs::create_dir(&runtime).expect("runtime");
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).expect("0700");
    let session = HerdrSession {
        program,
        name: format!("spawn-view-test-{}", std::process::id()),
        runtime,
    };
    let host = session.host();
    let bin = fixture.scratch.join("bin");
    std::fs::create_dir_all(&bin).expect("bin");
    let stub = bin.join("claude");
    std::fs::write(&stub, STUB_CLAUDE).expect("stub");
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("0755");

    let (workspace, engine_home, tmpdir) = fixture.launch();
    let mut words = view
        .wrap(&ViewedLaunch {
            engine: "agy",
            workspace: &workspace,
            engine_home: &engine_home,
            tmpdir: Some(&tmpdir),
        })
        .expect("wrap");
    assert!(!words.iter().any(|word| word == "--new-session"));
    let program = words.remove(0);
    words.push(stub.display().to_string());
    let terminal = TerminalName::new("spawn-exec_v1").expect("terminal");
    let id = WorkspaceId::new(format!("agent:{workspace}")).expect("workspace");
    host.start(&Launch {
        terminal: terminal.clone(),
        workspace: id.clone(),
        cwd: workspace.clone(),
        program,
        arguments: words,
        environment: vec![("TERM".to_string(), "xterm-256color".to_string())],
    })
    .expect("start");
    let deadline = Instant::now() + Duration::from_secs(15);
    let reading = loop {
        let reading = host.agent(&terminal, &id).expect("agent").expect("herdr reads agents");
        if reading.agent.is_some() || Instant::now() >= deadline {
            break reading;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(reading.agent.as_deref(), Some("claude"), "{reading:?}");
    // The session's socket, as the host found it, is not there inside.
    let socket = real_home.join(format!(".config/herdr/sessions/{}/herdr.sock", session.name));
    assert!(socket.exists(), "the host's socket: {}", socket.display());
    let check = view
        .wrap(&ViewedLaunch {
            engine: "agy",
            workspace: &workspace,
            engine_home: &engine_home,
            tmpdir: Some(&tmpdir),
        })
        .expect("wrap");
    fails(&check, &format!("ls '{}'", socket.display()));
    host.terminate(&terminal, &id).expect("terminate");
}
