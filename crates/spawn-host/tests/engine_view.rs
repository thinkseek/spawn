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

#![allow(clippy::disallowed_methods)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use spawn_core::id::{TerminalName, WorkspaceId};
use spawn_core::port::{EngineView, Launch, ServerSeen, TerminalHost, ViewedLaunch};
use spawn_host::engine_view::{
    AgyBrain, AgyDataDir, AgyWorkerHome, EnginePaths, EngineViews, HOST_DIRECTORIES, Mask,
    TMP_FALLBACK,
};
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
            ".config/herdr-core/spawn",
            ".local/state/herdr-core/spawn",
            "cfg/herdr",
            "cfg/herdr-core",
            "state/herdr-core",
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
        // The loop's shared TMPDIR (as `/var/tmp/spawn` is on a fleet
        // host), holding another host job's scratch.
        write(&fixture.host_tmp().join("spawn-backup-1/dump"), "secret");
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

    /// The engine's own TMPDIR, which the view makes below its home.
    fn tmpdir(&self) -> PathBuf {
        self.engine_home().join(TMP_FALLBACK)
    }

    /// The loop's shared TMPDIR.
    fn host_tmp(&self) -> PathBuf {
        self.scratch.join("vartmp")
    }

    /// The view's private directory, inside the masked runtime directory.
    fn private(&self) -> PathBuf {
        self.runtime().join("spawn-engine-view-test")
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
            xdg_state_home: Some(self.home().join("state")),
            runtime_dir: self.runtime(),
            host_directories,
            loop_dirs: vec![self.home().join(".local/share/spawn-rsi")],
            extra_files: Vec::new(),
            host_tmpdir: Some(self.host_tmp()),
            private_dir: self.private(),
            canonical_workspace: None,
            home_isolation: spawn_host::HomeIsolation::Blacklist,
            extra_mounts: Vec::new(),
            inherit_host: spawn_host::HostInheritMode::Off,
            inherit_host_paths: Vec::new(),
            engines: EngineViews::default(),
            path_env: std::env::var("PATH").ok(),
            container_env: std::env::var("container").ok(),
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
    assert!(
        !output.status.success(),
        "{script} succeeded: {}",
        text(&output)
    );
    text(&output)
}

/// The two words after each `flag` in `words`.
fn triples<'a>(words: &'a [String], flag: &str) -> Vec<(&'a str, &'a str)> {
    words
        .windows(3)
        .filter(|three| three[0] == flag)
        .map(|three| (three[1].as_str(), three[2].as_str()))
        .collect()
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
            worker_id: None,
            exit_file: None,
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
        h(".config/herdr-core"),
        h(".local/state/herdr-core"),
        h("cfg/herdr"),
        h("cfg/herdr-core"),
        h("state/herdr-core"),
        fixture.runtime().display().to_string(),
        fixture.keep().display().to_string(),
    ] {
        assert_eq!(ok(&words, &format!("ls -A '{dir}'")), "", "{dir}");
    }
    // ~/.gemini holds only agy's own home, which keeps its contents.
    assert_eq!(
        ok(&words, &format!("ls -A '{}'", h(".gemini"))),
        "antigravity-cli"
    );
    assert_eq!(
        ok(&words, &format!("cat '{engine_home}/brain/old'")),
        "mine"
    );
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
    fails(
        &words,
        &format!("ls '{}'", h(".config/herdr/sessions/spawn")),
    );

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
        ok(
            &words,
            &format!("cat '{}'", h(".local/share/spawn-rsi/bin/kept"))
        ),
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
    assert_eq!(
        std::fs::read_to_string(fixture.workspace().join("w")).expect("w"),
        "ws\n"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.engine_home().join("h")).expect("h"),
        "home\n"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.tmpdir().join("t")).expect("t"),
        "tmp\n"
    );
    // Nothing reached the host's masked places.
    assert!(!home.join(".claude/w").exists());
    assert_eq!(
        std::fs::read_to_string(home.join(".claude.json")).expect("json"),
        "secret"
    );

    // The view's private directory (and its empty file) is not there
    // inside, so it can be neither written nor swapped; the loop's shared
    // TMPDIR shows nothing of the host's jobs.
    let empty = fixture.private().join("empty");
    assert!(empty.is_file(), "the empty file is made");
    fails(&words, &format!("ls -A '{}'", fixture.private().display()));
    fails(&words, &format!("echo x > '{}'", empty.display()));
    fails(&words, &format!("ln -sf /etc/passwd '{}'", empty.display()));
    assert_eq!(
        ok(&words, &format!("ls -A '{}'", fixture.host_tmp().display())),
        ""
    );
    fails(
        &words,
        &format!(
            "cat '{}'",
            fixture.host_tmp().join("spawn-backup-1/dump").display()
        ),
    );
    let meta = std::fs::symlink_metadata(&empty).expect("empty");
    assert!(meta.is_file() && meta.len() == 0);
    assert_eq!(meta.permissions().mode() & 0o7777, 0o444);

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
/// fifo, as a socket or a device), is skipped and said, and the view still starts.
#[test]
fn missing_and_odd_targets_are_skipped_and_said_without_aborting() {
    let _serial = exclusive();
    let fixture = Fixture::new();
    // A fifo: neither a directory nor a regular file (a socket's path
    // would be too long here).
    let socket_path = fixture.scratch.join("odd.fifo");
    let made = Command::new("mkfifo")
        .arg(&socket_path)
        .status()
        .expect("mkfifo");
    assert!(made.success());
    let missing = fixture.scratch.join("missing.env");
    let mut config = fixture.config();
    config.extra_files = vec![missing.clone(), socket_path.clone()];
    config
        .host_directories
        .push(fixture.scratch.join("no-such-dir"));
    let Some(view) = view(config) else {
        return;
    };
    let (workspace, engine_home, _) = fixture.launch();
    let plan = view
        .plan(&ViewedLaunch {
            engine: "agy",
            workspace: &workspace,
            engine_home: &engine_home,
            worker_id: None,
            exit_file: None,
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
/// (workspace, engine home; the TMPDIR below the home is only named, never
/// bound again), each at its own path; resolved paths
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
            worker_id: None,
            exit_file: None,
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
    for never in [
        "--new-session",
        "--unshare-net",
        "--unshare-all",
        "--as-pid-1",
    ] {
        assert!(
            !words.iter().any(|word| word == never),
            "{never}: {words:?}"
        );
    }
    let first_bind = words
        .iter()
        .position(|word| word == "--bind")
        .expect("bind");
    let last_mask = words
        .iter()
        .rposition(|word| word == "--tmpfs" || word == "--ro-bind")
        .expect("mask");
    assert!(last_mask < first_bind, "{words:?}");
    let binds = triples(words, "--bind");
    let home_bind = (engine_home.as_str(), engine_home.as_str());
    assert_eq!(
        binds,
        vec![(workspace.as_str(), workspace.as_str()), home_bind]
    );
    assert!(tmpdir.starts_with(&engine_home), "{tmpdir}");
    assert!(triples(words, "--setenv").contains(&("TMPDIR", tmpdir.as_str())));
    // The symlinked canaries are masked at what they resolve to.
    let aws = fixture.scratch.join("real-aws");
    let netrc = fixture.scratch.join("real-netrc");
    assert!(
        plan.masks.contains(&Mask::Directory(aws)),
        "{:?}",
        plan.masks
    );
    assert!(plan.masks.contains(&Mask::File(netrc)), "{:?}", plan.masks);
    assert!(!plan.masks.iter().any(|mask| matches!(
        mask,
        Mask::Directory(path) | Mask::File(path) if path.ends_with(".aws") || path.ends_with(".netrc")
    )));
    // agy's own home is never masked; ~/.gemini is.
    assert!(!plan.masks.contains(&Mask::Directory(fixture.engine_home())));
    assert!(
        plan.masks
            .contains(&Mask::Directory(fixture.home().join(".gemini")))
    );
    // Every regular-file mask binds the one private empty 0444 file, never
    // /dev/null.
    let sources: Vec<&str> = words
        .windows(3)
        .skip(2)
        .filter(|three| three[0] == "--ro-bind")
        .map(|three| three[1].as_str())
        .collect();
    assert!(!sources.is_empty());
    assert!(
        sources.iter().all(|source| *source == sources[0]),
        "{sources:?}"
    );
    assert_ne!(sources[0], "/dev/null");
    let meta = std::fs::metadata(sources[0]).expect("empty");
    assert_eq!(meta.len(), 0);
    assert_eq!(meta.permissions().mode() & 0o777, 0o444);
}

/// A writable bind that would uncover a mask, lie in one, or reach the
/// view's private directory is refused, whatever it is: a misconfigured
/// engine home (`SPAWN_AGY_HOME`) equal to or holding a masked path is
/// refused, never unmasked. Only a home below another engine's masked
/// home is re-bound. The engine's TMPDIR is its own, never the loop's.
#[test]
fn a_bind_that_would_uncover_a_mask_is_refused() {
    let _serial = exclusive();
    let fixture = Fixture::new();
    let Some(view) = view(fixture.config()) else {
        return;
    };
    let (workspace, engine_home, tmpdir) = fixture.launch();
    let home = fixture.home();
    let plan_for = |workspace: &str, engine_home: &str| {
        view.plan(&ViewedLaunch {
            engine: "agy",
            workspace,
            engine_home,
            worker_id: None,
            exit_file: None,
        })
    };
    let shown = |path: PathBuf| path.display().to_string();

    for bad in [
        shown(home.clone()),
        shown(home.join(".claude")),
        shown(fixture.runtime()),
        shown(fixture.private()),
        shown(fixture.host_tmp()),
    ] {
        std::fs::create_dir_all(&bad).expect("dir");
        let refused = plan_for(&bad, &engine_home).expect_err(&bad);
        assert!(refused.contains("overlaps"), "{bad}: {refused}");
    }
    let refused = plan_for(&workspace, &shown(home.clone())).expect_err("home");
    assert!(refused.contains("holds the user's home"), "{refused}");
    for bad in [
        shown(home.join(".gemini")),
        shown(home.join(".claude")),
        shown(home.join(".claude/projects")),
        shown(home.join(".ssh")),
        shown(home.join(".config/herdr")),
        shown(home.join("cfg/herdr")),
        shown(fixture.keep()),
        shown(fixture.runtime()),
        shown(fixture.private()),
        shown(fixture.host_tmp()),
        "/tmp".to_string(),
    ] {
        let refused = plan_for(&workspace, &bad).expect_err(&bad);
        assert!(refused.contains("overlaps"), "{bad}: {refused}");
    }
    // The real /run/keep, when the host has it, is refused the same way
    // (only canonicalized, never read).
    if Path::new("/run/keep").is_dir() {
        assert!(plan_for(&workspace, "/run/keep").is_err());
    }
    // Below another engine's masked home: re-bound.
    let codex = shown(home.join(".codex/agy"));
    let plan = plan_for(&workspace, &codex).expect("below ~/.codex");
    assert_eq!(plan.tmpdir, home.join(".codex/agy").join(TMP_FALLBACK));
    // The engine's own TMPDIR, made 0700 below its home.
    let plan = plan_for(&workspace, &engine_home).expect("plan");
    assert_eq!(shown(plan.tmpdir.clone()), tmpdir);
    let meta = std::fs::metadata(&plan.tmpdir).expect("tmpdir");
    assert_eq!(meta.permissions().mode() & 0o777, 0o700);
    assert!(
        plan.masks.contains(&Mask::Directory(fixture.host_tmp())),
        "{:?}",
        plan.masks
    );
    ok(&plan.words, "touch \"$TMPDIR/x\" && [ ! -e /tmp/x ]");
}

/// A stand-in Claude Code: a shell script named `claude`, which Herdr
/// detects by the process's name; it shows a prompt and reads its input
/// (a builtin: no child takes its place in the pane).
const STUB_CLAUDE: &str = "#!/bin/sh\nprintf '> \\r\\n'\nwhile IFS= read -r line; do :; done\n";

/// The installed `herdr`, checking `SPAWN_HERDR_PROGRAM` first, if any
/// answers `--version`.
fn herdr_program() -> Option<String> {
    let env_program = std::env::var("SPAWN_HERDR_PROGRAM")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let home = std::env::var_os("HOME").map(|home| {
        Path::new(&home)
            .join(".local/bin/herdr")
            .display()
            .to_string()
    });
    env_program
        .into_iter()
        .chain(home)
        .chain(["herdr".to_string()])
        .find(|program| {
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
        let host_env = spawn_host::TerminalHostEnv {
            home: std::env::var_os("HOME").map(PathBuf::from),
            scrubbed_env_keys: spawn_host::TerminalHostEnv::collect_scrubbed_keys(
                std::env::vars_os().map(|(k, _)| k),
            ),
            ..spawn_host::TerminalHostEnv::default()
        };
        HerdrHost::on_session(&self.program, &self.name, host_env)
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
        assert!(
            !required(),
            "SPAWN_REQUIRE_HERDR=1 and no herdr is installed"
        );
        eprintln!("skipped: no herdr installed");
        return;
    };
    let fixture = Fixture::new();
    // The real Herdr session directory of this user is masked too.
    let real_home = PathBuf::from(std::env::var("HOME").expect("HOME"));
    let real_config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map_or_else(|| real_home.join(".config"), PathBuf::from);
    let mut config = fixture.config();
    config.host_directories.push(real_config.join("herdr"));
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

    let (workspace, engine_home, _) = fixture.launch();
    let mut words = view
        .wrap(&ViewedLaunch {
            engine: "agy",
            workspace: &workspace,
            engine_home: &engine_home,
            worker_id: None,
            exit_file: None,
        })
        .expect("wrap");
    assert!(!words.iter().any(|word| word == "--new-session"));
    let program = words.remove(0);
    // A parent between the view and the agent, as the runner (`spawn run`)
    // is in a real launch: the agent is not the view's own child.
    words.extend([
        "/bin/sh".to_string(),
        "-c".to_string(),
        "\"$0\"; :".to_string(),
    ]);
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
        let reading = host
            .agent(&terminal, &id)
            .expect("agent")
            .expect("herdr reads agents");
        if reading.agent.is_some() || Instant::now() >= deadline {
            break reading;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(reading.agent.as_deref(), Some("claude"), "{reading:?}");
    // The session's socket, as the host found it, is not there inside.
    let socket = real_config.join(format!("herdr/sessions/{}/herdr.sock", session.name));
    assert!(socket.exists(), "the host's socket: {}", socket.display());
    let check = view
        .wrap(&ViewedLaunch {
            engine: "agy",
            workspace: &workspace,
            engine_home: &engine_home,
            worker_id: None,
            exit_file: None,
        })
        .expect("wrap");
    fails(&check, &format!("ls '{}'", socket.display()));
    host.terminate(&terminal, &id).expect("terminate");
}

#[test]
fn canonical_workspace_mounts_at_target_sets_env_and_keeps_root_readonly() {
    let _serial = exclusive();
    let fixture = Fixture::new();
    let mut config = fixture.config();
    config.canonical_workspace = Some(PathBuf::from("/workspace"));
    let Some(view) = view(config) else {
        return;
    };
    let (workspace, engine_home, _) = fixture.launch();
    std::fs::write(fixture.workspace().join("in-workspace"), "hello").expect("ws file");
    let exit_file = fixture.workspace().join(".spawn/exit/exec_canon");
    std::fs::create_dir_all(exit_file.parent().expect("parent")).expect("exit dir");

    let plan = view
        .plan(&ViewedLaunch {
            engine: "agy",
            workspace: &workspace,
            engine_home: &engine_home,
            worker_id: Some("agent_canon"),
            exit_file: Some(&exit_file.display().to_string()),
        })
        .expect("plan");

    let canon_ws = std::fs::canonicalize(&workspace).expect("canon ws");
    let canon_ws_str = canon_ws.display().to_string();
    assert_eq!(
        triples(&plan.words, "--bind"),
        vec![
            (canon_ws_str.as_str(), "/workspace"),
            (engine_home.as_str(), engine_home.as_str()),
        ]
    );
    assert!(
        plan.words
            .windows(2)
            .any(|w| w == ["--chdir", "/workspace"])
    );
    let setenvs = triples(&plan.words, "--setenv");
    assert!(setenvs.contains(&("SPAWN_WORKSPACE", "/workspace")));
    assert!(setenvs.contains(&("SPAWN_SANDBOX_CANONICAL_WORKSPACE", "/workspace")));
    assert!(setenvs.contains(&("PWD", "/workspace")));
    assert!(setenvs.contains(&("SPAWN_EXIT_FILE", "/workspace/.spawn/exit/exec_canon")));

    assert_eq!(
        ok(
            &plan.words,
            "printf '%s|%s|%s|%s|%s' \"$(pwd)\" \"$PWD\" \"$SPAWN_WORKSPACE\" \"$SPAWN_SANDBOX_CANONICAL_WORKSPACE\" \"$SPAWN_EXIT_FILE\""
        ),
        "/workspace|/workspace|/workspace|/workspace|/workspace/.spawn/exit/exec_canon"
    );
    assert_eq!(
        ok(
            &plan.words,
            "printf '0\\n' > \"$SPAWN_EXIT_FILE\" && cat /workspace/in-workspace"
        ),
        "hello"
    );
    assert_eq!(
        std::fs::read_to_string(&exit_file).expect("host exit"),
        "0\n"
    );
    let err = fails(&plan.words, "touch /root-write-probe");
    assert!(err.contains("Read-only file system"), "{err}");
}

#[test]
fn whitelist_home_hides_existing_and_dynamic_secrets_while_keeping_workspace_and_engine_home() {
    let _serial = exclusive();
    let fixture = Fixture::new();
    let kube = fixture.home().join(".kube");
    std::fs::create_dir_all(&kube).expect("kube");
    std::fs::write(kube.join("config"), "kube-secret").expect("kube config");

    let mut config = fixture.config();
    config.home_isolation = spawn_host::HomeIsolation::Whitelist;
    config.canonical_workspace = Some(PathBuf::from("/workspace"));
    let Some(view) = view(config) else {
        return;
    };
    let (workspace, engine_home, _) = fixture.launch();
    let words = view
        .wrap(&ViewedLaunch {
            engine: "agy",
            workspace: &workspace,
            engine_home: &engine_home,
            worker_id: Some("agent_wl"),
            exit_file: None,
        })
        .expect("wrap");

    // Simulate a sensitive file appearing in $HOME after wrap() was constructed.
    let dynamic_dir = fixture.home().join(".dynamic-secret");
    std::fs::create_dir_all(&dynamic_dir).expect("dynamic dir");
    std::fs::write(dynamic_dir.join("token"), "late-secret").expect("dynamic token");

    let h = |rel: &str| fixture.home().join(rel).display().to_string();
    for hidden in [
        h(".ssh/id_ed25519"),
        h(".kube/config"),
        h(".dynamic-secret/token"),
        h(".netrc"),
    ] {
        fails(&words, &format!("cat '{hidden}'"));
    }
    assert_eq!(
        ok(&words, &format!("cat '{engine_home}/brain/old'")),
        "mine"
    );
    assert_eq!(
        ok(
            &words,
            &format!(
                "printf 'written' > '{engine_home}/brain/wl-note' && printf 'ws-ok' > /workspace/wl-ws"
            )
        ),
        ""
    );
    assert_eq!(
        std::fs::read_to_string(fixture.engine_home().join("brain/wl-note")).expect("host eh"),
        "written"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.workspace().join("wl-ws")).expect("host ws"),
        "ws-ok"
    );
    assert_eq!(
        ok(&words, &format!("ls -A '{}'", fixture.keep().display())),
        ""
    );
    assert_eq!(ok(&words, "ls -A /tmp"), "");
}

fn seed_agy_host_fixture(host_eh: &Path) {
    std::fs::create_dir_all(host_eh.join("cache")).expect("cache");
    std::fs::create_dir_all(host_eh.join("bin")).expect("bin");
    std::fs::create_dir_all(host_eh.join("builtin")).expect("builtin");
    std::fs::write(host_eh.join("installation_id"), "inst-1").expect("inst");
    std::fs::write(host_eh.join("jetski_state.pbtxt"), "state-1").expect("state");
    std::fs::write(host_eh.join("settings.json"), "host-v1").expect("settings");
    std::fs::write(host_eh.join("cache/onboarding.json"), "{\"done\":true}").expect("onboard");
    std::fs::write(host_eh.join("bin/tool"), "bin-v1").expect("bin tool");
    std::fs::set_permissions(
        host_eh.join("bin/tool"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("bin mode");
    std::fs::write(host_eh.join("builtin/skill.md"), "builtin-v1").expect("builtin skill");
    std::fs::write(
        host_eh.join("antigravity-oauth-token"),
        "SECRET_OAUTH_TOKEN",
    )
    .expect("oauth");
}

fn assert_isolated_worker_permissions(worker_home: &Path) {
    let settings_mode = std::fs::metadata(worker_home.join("settings.json"))
        .expect("w1 settings meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(settings_mode, 0o600, "seed files must be 0600");
    let tool_mode = std::fs::metadata(worker_home.join("bin/tool"))
        .expect("w1 tool meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(tool_mode, 0o700, "worker bin executables must be 0700");
    let oauth_meta =
        std::fs::metadata(worker_home.join("antigravity-oauth-token")).expect("oauth mountpoint");
    assert_eq!(oauth_meta.permissions().mode() & 0o777, 0o600);
    assert_eq!(oauth_meta.len(), 0);
}

#[test]
fn agy_data_dir_isolates_worker_home_tmp_ro_creds_and_brain() {
    let _serial = exclusive();
    let fixture = Fixture::new();
    let host_eh = fixture.engine_home();
    seed_agy_host_fixture(&host_eh);

    let data_dir = fixture.scratch.join("agy-data");
    let mut config = fixture.config();
    config.home_isolation = spawn_host::HomeIsolation::Whitelist;
    config.canonical_workspace = Some(PathBuf::from("/workspace"));
    config.engines.agy = Some(EnginePaths::Data(AgyDataDir {
        root: data_dir.clone(),
        home: AgyWorkerHome::PerWorker,
        brain: AgyBrain::PerWorker,
    }));

    let Some(view_isolated) = view(config) else {
        return;
    };
    let (workspace, engine_home, _) = fixture.launch();
    let launch_worker = |worker_id: &str| {
        view_isolated
            .plan(&ViewedLaunch {
                engine: "agy",
                workspace: &workspace,
                engine_home: &engine_home,
                worker_id: Some(worker_id),
                exit_file: None,
            })
            .expect("plan")
    };
    let plan_w1 = launch_worker("agent_w1");
    let w1_home = std::fs::canonicalize(data_dir.join("workers/agent_w1/home")).expect("w1 home");
    let w1_tmp = std::fs::canonicalize(data_dir.join("workers/agent_w1/tmp")).expect("w1 tmp");
    assert_eq!(plan_w1.tmpdir, w1_tmp);

    // Seeded non-secret files + RO static dirs + RO oauth token work inside bwrap.
    let probe_cmd = format!(
        "printf '%s|%s|%s|%s|%s|%s|%s' \"$(cat '{engine_home}/installation_id')\" \"$(cat '{engine_home}/jetski_state.pbtxt')\" \"$(cat '{engine_home}/settings.json')\" \"$(cat '{engine_home}/cache/onboarding.json')\" \"$(cat '{engine_home}/bin/tool')\" \"$(cat '{engine_home}/builtin/skill.md')\" \"$(cat '{engine_home}/antigravity-oauth-token')\""
    );
    assert_eq!(
        ok(&plan_w1.words, &probe_cmd),
        "inst-1|state-1|host-v1|{\"done\":true}|bin-v1|builtin-v1|SECRET_OAUTH_TOKEN"
    );
    for ro_path in ["antigravity-oauth-token", "builtin/new"] {
        let err = fails(
            &plan_w1.words,
            &format!("echo x > '{engine_home}/{ro_path}'"),
        );
        assert!(err.contains("Read-only file system"), "{ro_path}: {err}");
    }
    assert_isolated_worker_permissions(&w1_home);

    // Volatile writes + TMPDIR + brain/ stay 100% in worker data_dir, not host engine_home.
    ok(
        &plan_w1.words,
        &format!(
            "printf 'w1-edited' > '{engine_home}/settings.json' && \
             mkdir -p '{engine_home}/brain/conv-1' && \
             printf 'w1-brain' > '{engine_home}/brain/conv-1/log.txt' && \
             printf 'w1-tmp' > \"$TMPDIR/scratch\""
        ),
    );
    assert_eq!(
        std::fs::read_to_string(w1_home.join("brain/conv-1/log.txt")).expect("w1 brain"),
        "w1-brain"
    );
    assert_eq!(
        std::fs::read_to_string(w1_tmp.join("scratch")).expect("w1 tmp"),
        "w1-tmp"
    );
    assert_eq!(
        std::fs::read_to_string(host_eh.join("settings.json")).expect("host untouched"),
        "host-v1"
    );
    assert!(!host_eh.join("brain/conv-1").exists());

    // Seeding happens only once for w1, while w2 gets fresh host state and its own brain/.
    std::fs::write(host_eh.join("settings.json"), "host-v2").expect("settings v2");
    assert_eq!(
        ok(
            &launch_worker("agent_w1").words,
            &format!("cat '{engine_home}/settings.json'")
        ),
        "w1-edited"
    );
    let plan_w2 = launch_worker("agent_w2");
    assert_eq!(
        ok(
            &plan_w2.words,
            &format!("cat '{engine_home}/settings.json'")
        ),
        "host-v2"
    );
    fails(
        &plan_w2.words,
        &format!("cat '{engine_home}/brain/conv-1/log.txt'"),
    );
}

#[test]
fn agy_data_dir_keeps_bin_and_xdg_cache_writable_per_worker() {
    let _serial = exclusive();
    let fixture = Fixture::new();
    let host_eh = fixture.engine_home();
    seed_agy_host_fixture(&host_eh);

    let data_dir = fixture.scratch.join("agy-data");
    let mut config = fixture.config();
    config.home_isolation = spawn_host::HomeIsolation::Whitelist;
    config.canonical_workspace = Some(PathBuf::from("/workspace"));
    config.engines.agy = Some(EnginePaths::Data(AgyDataDir {
        root: data_dir.clone(),
        home: AgyWorkerHome::PerWorker,
        brain: AgyBrain::PerWorker,
    }));

    let Some(view_isolated) = view(config) else {
        return;
    };
    let (workspace, engine_home, _) = fixture.launch();
    let plan_w1 = view_isolated
        .plan(&ViewedLaunch {
            engine: "agy",
            workspace: &workspace,
            engine_home: &engine_home,
            worker_id: Some("agent_w1"),
            exit_file: None,
        })
        .expect("plan");
    let w1_home = std::fs::canonicalize(data_dir.join("workers/agent_w1/home")).expect("w1 home");

    ok(
        &plan_w1.words,
        &format!(
            "printf 'agentapi-v1' > '{engine_home}/bin/agentapi' && \
             mkdir -p \"$XDG_CACHE_HOME/antigravity\" && \
             printf 'ok' > \"$XDG_CACHE_HOME/antigravity/probe\""
        ),
    );
    assert_eq!(
        std::fs::read_to_string(w1_home.join("bin/agentapi")).expect("w1 agentapi"),
        "agentapi-v1"
    );
    assert!(!host_eh.join("bin/agentapi").exists());
    assert_eq!(
        std::fs::read_to_string(w1_home.join("xdg-cache/antigravity/probe")).expect("w1 xdg-cache"),
        "ok"
    );
}

#[test]
fn agy_data_dir_shares_brain_when_isolate_brain_is_off() {
    let _serial = exclusive();
    let fixture = Fixture::new();
    let host_eh = fixture.engine_home();
    seed_agy_host_fixture(&host_eh);

    let data_dir = fixture.scratch.join("agy-data");
    let mut config = fixture.config();
    config.home_isolation = spawn_host::HomeIsolation::Whitelist;
    config.canonical_workspace = Some(PathBuf::from("/workspace"));
    config.engines.agy = Some(EnginePaths::Data(AgyDataDir::per_worker(data_dir.clone())));

    let Some(view_shared) = view(config) else {
        return;
    };
    let (workspace, engine_home, _) = fixture.launch();
    let plan_shared_w1 = view_shared
        .plan(&ViewedLaunch {
            engine: "agy",
            workspace: &workspace,
            engine_home: &engine_home,
            worker_id: Some("agent_w1"),
            exit_file: None,
        })
        .expect("shared w1");
    let plan_shared_w2 = view_shared
        .plan(&ViewedLaunch {
            engine: "agy",
            workspace: &workspace,
            engine_home: &engine_home,
            worker_id: Some("agent_w2"),
            exit_file: None,
        })
        .expect("shared w2");
    ok(
        &plan_shared_w1.words,
        &format!("printf 'shared-ok' > '{engine_home}/brain/shared.txt'"),
    );
    assert_eq!(
        ok(
            &plan_shared_w2.words,
            &format!("cat '{engine_home}/brain/shared.txt'")
        ),
        "shared-ok"
    );
    assert_eq!(
        std::fs::read_to_string(data_dir.join("brain/shared.txt")).expect("host shared brain"),
        "shared-ok"
    );
}

#[test]
fn extra_mounts_enforces_ro_and_rw_permissions() {
    let _serial = exclusive();
    let fixture = Fixture::new();
    let readonly_dir = fixture.scratch.join("extra-ro");
    let writable_dir = fixture.scratch.join("extra-rw");
    std::fs::create_dir_all(&readonly_dir).expect("ro dir");
    std::fs::create_dir_all(&writable_dir).expect("rw dir");
    std::fs::write(readonly_dir.join("data"), "ro-data").expect("ro file");
    std::fs::write(writable_dir.join("data"), "rw-data").expect("rw file");

    let mut config = fixture.config();
    config.extra_mounts = spawn_host::engine_view::parse_extra_mounts(&format!(
        "{}:ro,{}",
        readonly_dir.display(),
        writable_dir.display()
    ));
    let Some(view) = view(config) else {
        return;
    };
    let (workspace, engine_home, _) = fixture.launch();
    let words = view
        .wrap(&ViewedLaunch {
            engine: "agy",
            workspace: &workspace,
            engine_home: &engine_home,
            worker_id: None,
            exit_file: None,
        })
        .expect("wrap");

    assert_eq!(
        ok(&words, &format!("cat '{}/data'", readonly_dir.display())),
        "ro-data"
    );
    let ro_err = fails(
        &words,
        &format!("echo x > '{}/written'", readonly_dir.display()),
    );
    assert!(ro_err.contains("Read-only file system"), "{ro_err}");

    assert_eq!(
        ok(&words, &format!("cat '{}/data'", writable_dir.display())),
        "rw-data"
    );
    ok(
        &words,
        &format!("printf 'persisted' > '{}/written'", writable_dir.display()),
    );
    assert_eq!(
        std::fs::read_to_string(writable_dir.join("written")).expect("rw persisted"),
        "persisted"
    );
}

#[test]
fn whitelist_ro_binds_agy_binary_inside_home_without_exposing_siblings() {
    let _serial = exclusive();
    let fixture = Fixture::new();
    let bin_dir = fixture.home().join(".local/bin");
    let agy_bin = bin_dir.join("agy");
    let sibling_secret = bin_dir.join("secret-tool");
    write(&agy_bin, "#!/bin/sh\necho agy-ok\n");
    write(&sibling_secret, "#!/bin/sh\necho secret\n");
    std::fs::set_permissions(&agy_bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    std::fs::set_permissions(&sibling_secret, std::fs::Permissions::from_mode(0o755))
        .expect("chmod sibling");

    let mut config = fixture.config();
    config.home_isolation = spawn_host::HomeIsolation::Whitelist;
    config.engines.agy = Some(EnginePaths::Program(agy_bin.clone()));
    let Some(view) = view(config) else {
        return;
    };
    let (workspace, engine_home, _) = fixture.launch();
    let words = view
        .wrap(&ViewedLaunch {
            engine: "agy",
            workspace: &workspace,
            engine_home: &engine_home,
            worker_id: Some("w-bin"),
            exit_file: None,
        })
        .expect("wrap");

    assert_eq!(ok(&words, &agy_bin.display().to_string()), "agy-ok");
    fails(&words, &format!("cat '{}'", sibling_secret.display()));
}

#[test]
fn bwrap_v2_preserves_host_netns_loopback() {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    let _serial = exclusive();
    let fixture = Fixture::new();
    let mut config = fixture.config();
    config.home_isolation = spawn_host::HomeIsolation::Whitelist;
    config.canonical_workspace = Some(PathBuf::from("/workspace"));
    let Some(view) = view(config) else {
        return;
    };

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut line = String::new();
        reader.read_line(&mut line).expect("read ping");
        assert_eq!(line, "ping\n");
        stream.write_all(b"pong\n").expect("write pong");
    });

    let (workspace, engine_home, _) = fixture.launch();
    let words = view
        .wrap(&ViewedLaunch {
            engine: "agy",
            workspace: &workspace,
            engine_home: &engine_home,
            worker_id: Some("w-netns"),
            exit_file: None,
        })
        .expect("wrap");

    let script = format!(
        "/bin/bash -c 'exec 3<>/dev/tcp/127.0.0.1/{port} && printf \"ping\\n\" >&3 && head -n 1 <&3'"
    );
    assert_eq!(ok(&words, &script), "pong");
    handle.join().expect("server thread");
}

#[test]
fn whitelist_inherit_host_auto_binds_safe_toolchains_and_skips_secret_gitconfig() {
    let _serial = exclusive();
    let fixture = Fixture::new();
    let rustup = fixture.home().join(".rustup/toolchains/stable");
    let cargo_bin = fixture.home().join(".cargo/bin");
    let cargo_cfg = fixture.home().join(".cargo/config.toml");
    let cargo_creds = fixture.home().join(".cargo/credentials.toml");
    let gitconfig = fixture.home().join(".gitconfig");
    write(&rustup.join("marker"), "rustup-ok");
    write(&cargo_bin.join("cargo-ok"), "cargo-bin-ok");
    write(&cargo_cfg, "[net]\noffline = true\n");
    write(&cargo_creds, "[registry]\ntoken = \"cio-secret-123\"\n");
    write(
        &gitconfig,
        "[user]\n\tname = Tester\n\temail = tester@example.com\n",
    );

    let mut config = fixture.config();
    config.home_isolation = spawn_host::HomeIsolation::Whitelist;
    config.canonical_workspace = Some(PathBuf::from("/workspace"));
    config.inherit_host = spawn_host::HostInheritMode::Auto;
    let Some(view) = view(config) else {
        return;
    };
    let (workspace, engine_home, _) = fixture.launch();
    let words = view
        .wrap(&ViewedLaunch {
            engine: "agy",
            workspace: &workspace,
            engine_home: &engine_home,
            worker_id: Some("w-inherit"),
            exit_file: None,
        })
        .expect("wrap");

    assert_eq!(
        ok(
            &words,
            &format!("cat '{}'", rustup.join("marker").display())
        ),
        "rustup-ok"
    );
    assert_eq!(
        ok(
            &words,
            &format!("cat '{}'", cargo_bin.join("cargo-ok").display())
        ),
        "cargo-bin-ok"
    );
    assert_eq!(
        ok(&words, &format!("cat '{}'", cargo_cfg.display())),
        "[net]\noffline = true"
    );
    assert_eq!(
        ok(
            &words,
            &format!("git config --file '{}' user.name", gitconfig.display())
        ),
        "Tester"
    );
    fails(&words, &format!("cat '{}'", cargo_creds.display()));
    // Inherited paths must remain read-only inside the sandbox.
    fails(
        &words,
        &format!("touch '{}'", cargo_bin.join("mutated").display()),
    );

    // Inline secret in .gitconfig must cause it to be skipped cleanly.
    write(
        &gitconfig,
        "[url \"https://x-access-token:ghp_secret@github.com/\"]\n\tinsteadOf = https://github.com/\n",
    );
    let plan = view
        .plan(&ViewedLaunch {
            engine: "agy",
            workspace: &workspace,
            engine_home: &engine_home,
            worker_id: Some("w-inherit-secret"),
            exit_file: None,
        })
        .expect("plan skips secret gitconfig");
    assert!(
        plan.skipped.iter().any(|(p, _)| p.ends_with(".gitconfig")),
        "expected .gitconfig skipped: {:?}",
        plan.skipped
    );
    let words_secret = view
        .wrap(&ViewedLaunch {
            engine: "agy",
            workspace: &workspace,
            engine_home: &engine_home,
            worker_id: Some("w-inherit-secret"),
            exit_file: None,
        })
        .expect("wrap");
    fails(&words_secret, &format!("cat '{}'", gitconfig.display()));
}

#[test]
fn explicit_inherit_host_paths_refuses_secrets_and_allows_safe_ro_paths() {
    let _serial = exclusive();
    let fixture = Fixture::new();
    let safe_dir = fixture.home().join(".local/share/custom-tools");
    let cargo_dir = fixture.home().join(".cargo");
    write(&safe_dir.join("tool"), "tool-ok");
    write(&cargo_dir.join("credentials.toml"), "token = 'secret'\n");
    write(&fixture.home().join(".ssh/id_ed25519"), "PRIVATE KEY");

    let mut config = fixture.config();
    config.home_isolation = spawn_host::HomeIsolation::Whitelist;
    config.canonical_workspace = Some(PathBuf::from("/workspace"));
    config.inherit_host_paths = vec![fixture.home().join(".ssh")];
    let Some(view_bad_ssh) = view(config) else {
        return;
    };
    let (workspace, engine_home, _) = fixture.launch();
    assert!(
        view_bad_ssh
            .plan(&ViewedLaunch {
                engine: "agy",
                workspace: &workspace,
                engine_home: &engine_home,
                worker_id: Some("w-bad-ssh"),
                exit_file: None,
            })
            .is_err()
    );

    let mut config_bad_dir = fixture.config();
    config_bad_dir.home_isolation = spawn_host::HomeIsolation::Whitelist;
    config_bad_dir.inherit_host_paths = vec![cargo_dir];
    let view_bad_dir = BwrapView::new("bwrap", config_bad_dir).expect("bwrap");
    assert!(
        view_bad_dir
            .plan(&ViewedLaunch {
                engine: "agy",
                workspace: &workspace,
                engine_home: &engine_home,
                worker_id: Some("w-bad-cargo"),
                exit_file: None,
            })
            .is_err()
    );

    let mut config_ok = fixture.config();
    config_ok.home_isolation = spawn_host::HomeIsolation::Whitelist;
    config_ok.canonical_workspace = Some(PathBuf::from("/workspace"));
    config_ok.inherit_host_paths = vec![safe_dir.clone()];
    let view_ok = BwrapView::new("bwrap", config_ok).expect("bwrap");
    let words = view_ok
        .wrap(&ViewedLaunch {
            engine: "agy",
            workspace: &workspace,
            engine_home: &engine_home,
            worker_id: Some("w-safe"),
            exit_file: None,
        })
        .expect("wrap");
    assert_eq!(
        ok(
            &words,
            &format!("cat '{}'", safe_dir.join("tool").display())
        ),
        "tool-ok"
    );
}

#[test]
fn claude_bwrap_sandbox_isolates_home_and_shares_projects_with_host() {
    let _serial = exclusive();
    let fixture = Fixture::new();
    let claude_home = fixture.home().join(".claude");
    let host_projects = claude_home.join("projects/-workspace");
    write(&claude_home.join("settings.json"), "{\"sandbox\":true}\n");
    write(
        &claude_home.join(".credentials.json"),
        "{\"claudeAiOauth\":\"tok\"}\n",
    );
    write(&claude_home.join("history.jsonl"), "host-history-line\n");
    write(
        &fixture.home().join(".claude.json"),
        "{\"hostSecret\":\"nope\"}\n",
    );
    write(&fixture.home().join(".ssh/id_rsa"), "SSH_SECRET");
    std::fs::create_dir_all(&host_projects).expect("host projects");

    let claude_data = fixture.scratch.join("claude-data");
    let mut config = fixture.config();
    config.home_isolation = spawn_host::HomeIsolation::Whitelist;
    config.canonical_workspace = Some(PathBuf::from("/workspace"));
    config.engines.claude = Some(EnginePaths::Data(claude_data.clone()));
    let Some(view) = view(config) else {
        return;
    };
    let (workspace, _, _) = fixture.launch();
    let words = view
        .wrap(&ViewedLaunch {
            engine: "claude",
            workspace: &workspace,
            engine_home: &claude_home.display().to_string(),
            worker_id: Some("claude-w1"),
            exit_file: None,
        })
        .expect("wrap claude");

    // Host secrets & sibling history stay hidden.
    fails(
        &words,
        &format!("cat '{}'", fixture.home().join(".ssh/id_rsa").display()),
    );
    fails(
        &words,
        &format!("cat '{}'", claude_home.join("history.jsonl").display()),
    );
    assert_eq!(
        ok(
            &words,
            &format!("cat '{}'", fixture.home().join(".claude.json").display())
        ),
        "{\"hasCompletedOnboarding\":true,\"theme\":\"dark\"}"
    );
    assert_eq!(
        ok(
            &words,
            &format!("cat '{}'", claude_home.join(".claude.json").display())
        ),
        "{\"hasCompletedOnboarding\":true,\"theme\":\"dark\"}"
    );
    // Settings & credentials are readable but read-only.
    assert_eq!(
        ok(
            &words,
            &format!("cat '{}'", claude_home.join("settings.json").display())
        ),
        "{\"sandbox\":true}"
    );
    fails(
        &words,
        &format!(
            "sh -c 'echo x > \"{}\"'",
            claude_home.join("settings.json").display()
        ),
    );
    // Writing a transcript inside <claude_home>/projects persists to host projects!
    let transcript = claude_home.join("projects/-workspace/sess-1.jsonl");
    assert_eq!(
        ok(
            &words,
            &format!(
                "printf 'ok\\n' > '{}' && cat '{}'",
                transcript.display(),
                transcript.display()
            ),
        ),
        "ok"
    );
    assert_eq!(
        std::fs::read_to_string(host_projects.join("sess-1.jsonl")).expect("host transcript"),
        "ok\n"
    );
    // Double check host ~/.claude.json was never modified.
    assert_eq!(
        std::fs::read_to_string(fixture.home().join(".claude.json")).expect("host claude json"),
        "{\"hostSecret\":\"nope\"}\n"
    );
}

fn seed_managed_claude_dir(dir: &Path, token: &str, oauth: &str) {
    std::fs::create_dir_all(dir).expect("mkdir managed");
    std::fs::write(
        dir.join("settings.json"),
        format!("{{\"env\":{{\"ANTHROPIC_AUTH_TOKEN\":\"{token}\"}}}}\n"),
    )
    .expect("write settings");
    std::fs::write(
        dir.join(".credentials.json"),
        format!("{{\"oauth\":\"{oauth}\"}}\n"),
    )
    .expect("write creds");
}

#[test]
fn managed_claude_config_is_masked_from_agy() {
    let fixture = Fixture::new();
    let default_managed = fixture.home().join(".local/state/spawn/claude-config");
    let xdg_state = fixture.scratch.join("xdg-state");
    let xdg_managed = xdg_state.join("spawn/claude-config");
    seed_managed_claude_dir(&default_managed, "managed-secret-1", "managed-cred-1");
    seed_managed_claude_dir(&xdg_managed, "xdg-secret-2", "xdg-cred-2");

    let mut config = fixture.config();
    config.xdg_state_home = Some(xdg_state);
    let Some(view) = view(config) else {
        return;
    };
    let (workspace, _, agy_home) = fixture.launch();
    let agy_words = view
        .wrap(&ViewedLaunch {
            engine: "agy",
            workspace: &workspace,
            engine_home: &agy_home,
            worker_id: Some("agy-w1"),
            exit_file: None,
        })
        .expect("wrap agy");
    for hidden in [
        default_managed.join("settings.json"),
        default_managed.join(".credentials.json"),
        xdg_managed.join("settings.json"),
        xdg_managed.join(".credentials.json"),
    ] {
        fails(&agy_words, &format!("cat '{}'", hidden.display()));
    }
}

#[test]
fn managed_claude_config_works_under_claude_sandbox() {
    let fixture = Fixture::new();
    let default_managed = fixture.home().join(".local/state/spawn/claude-config");
    seed_managed_claude_dir(&default_managed, "managed-secret-1", "managed-cred-1");

    let claude_data = fixture.scratch.join("claude-data");
    let data_managed = claude_data.join("config");
    seed_managed_claude_dir(&data_managed, "data-dir-secret-3", "data-dir-cred-3");

    let mut config = fixture.config();
    config.engines.claude = Some(EnginePaths::Data(claude_data));
    let Some(view) = view(config) else {
        return;
    };
    let (workspace, _, _) = fixture.launch();

    for (idx, managed) in [(&data_managed), (&default_managed)].iter().enumerate() {
        let worker_id = format!("claude-m{idx}");
        let claude_words = view
            .wrap(&ViewedLaunch {
                engine: "claude",
                workspace: &workspace,
                engine_home: &managed.display().to_string(),
                worker_id: Some(&worker_id),
                exit_file: None,
            })
            .expect("wrap claude");
        let settings = ok(
            &claude_words,
            &format!("cat '{}'", managed.join("settings.json").display()),
        );
        assert!(settings.contains("ANTHROPIC_AUTH_TOKEN"), "{settings}");
        fails(
            &claude_words,
            &format!(
                "sh -c 'echo x > \"{}\"'",
                managed.join("settings.json").display()
            ),
        );
        let creds = ok(
            &claude_words,
            &format!("cat '{}'", managed.join(".credentials.json").display()),
        );
        assert!(creds.contains("oauth"), "{creds}");
        let transcript = managed.join("projects/sess-managed.jsonl");
        assert_eq!(
            ok(
                &claude_words,
                &format!(
                    "printf 'ok\\n' > '{}' && cat '{}'",
                    transcript.display(),
                    transcript.display()
                ),
            ),
            "ok"
        );
        assert_eq!(
            std::fs::read_to_string(transcript).expect("persisted transcript"),
            "ok\n"
        );
    }
}

#[test]
fn container_view_fails_closed_on_bare_host_and_sets_isolated_tmpdir_in_container() {
    let fixture = Fixture::new();
    let cfg = fixture.config();
    let err =
        spawn_host::ContainerView::new_with_detector(cfg.clone(), &|| spawn_host::ContainerEnv {
            in_container: false,
            runtime: None,
            signals: Vec::new(),
        })
        .expect_err("must fail closed on bare host");
    assert!(err.contains("bare host"), "{err}");

    let view = spawn_host::ContainerView::new_with_detector(cfg, &|| spawn_host::ContainerEnv {
        in_container: true,
        runtime: Some("docker".to_string()),
        signals: vec!["/.dockerenv".to_string()],
    })
    .expect("container view allowed when container detected");
    assert_eq!(view.name(), "container");
    assert_eq!(view.canonical_workspace(), None);
    let (workspace, engine_home, expected_tmp) = fixture.launch();
    let words = view
        .wrap(&ViewedLaunch {
            engine: "agy",
            workspace: &workspace,
            engine_home: &engine_home,
            worker_id: Some("w-ctr"),
            exit_file: None,
        })
        .expect("wrap container");
    assert_eq!(words.last().map(String::as_str), Some("--"));
    let mut cmd = std::process::Command::new(&words[0]);
    cmd.args(&words[1..]);
    cmd.args(["/bin/sh", "-c", "printf '%s' \"$TMPDIR\""]);
    let out = cmd.output().expect("run wrapper");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), expected_tmp);
}

fn inside_with_env(words: &[String], envs: &[(&str, &str)], script: &str) -> Output {
    let mut command = Command::new(&words[0]);
    command.args(&words[1..]);
    for &(key, val) in envs {
        command.env(key, val);
    }
    command
        .args(["/bin/sh", "-c", script])
        .stdin(Stdio::null())
        .output()
        .expect("bwrap")
}

fn ok_with_env(words: &[String], envs: &[(&str, &str)], script: &str) -> String {
    let out = inside_with_env(words, envs, script);
    assert!(
        out.status.success(),
        "{script}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    text(&out)
}

fn bind_host_socket_if_fits(path: &Path) -> Option<std::os::unix::net::UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("socket parent");
    }
    if path.as_os_str().len() < 100 {
        Some(std::os::unix::net::UnixListener::bind(path).expect("bind host socket"))
    } else {
        None
    }
}

fn run_socket_isolation_check(
    view: &BwrapView,
    fixture: &Fixture,
    engine: &str,
    engine_home: &Path,
) {
    let ws = fixture.workspace().display().to_string();
    let eh = engine_home.display().to_string();
    let words_a = view
        .wrap(&ViewedLaunch {
            engine,
            workspace: &ws,
            engine_home: &eh,
            worker_id: Some("worker-a"),
            exit_file: None,
        })
        .expect("wrap worker-a");
    let words_b = view
        .wrap(&ViewedLaunch {
            engine,
            workspace: &ws,
            engine_home: &eh,
            worker_id: Some("worker-b"),
            exit_file: None,
        })
        .expect("wrap worker-b");

    let ready_file = fixture.workspace().join(format!("ready-{engine}"));
    let _ = std::fs::remove_file(&ready_file);
    let cc_socks = engine_home.join("cc-socks");
    let script_a = format!(
        "echo a > \"$XDG_RUNTIME_DIR/worker-a.sock\" && \
         echo a > /tmp/worker-a.sock && \
         mkdir -p '{}' && echo a > '{}/worker-a.sock' && \
         echo ok > '{}' && sleep 2",
        cc_socks.display(),
        cc_socks.display(),
        ready_file.display()
    );
    let child_a = Command::new(&words_a[0])
        .args(&words_a[1..])
        .args(["/bin/sh", "-c", &script_a])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn worker-a");

    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready_file.is_file() {
        assert!(
            Instant::now() < deadline,
            "{engine} worker-a never signaled ready"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    assert_eq!(ok(&words_b, "ls -A \"$XDG_RUNTIME_DIR\""), "", "{engine}");
    assert_eq!(ok(&words_b, "ls -A /tmp"), "", "{engine}");
    assert_eq!(
        ok(
            &words_b,
            &format!("ls -A '{}'", fixture.home().join(".cc-socks").display())
        ),
        "",
        "{engine}"
    );
    fails(&words_b, "cat \"$XDG_RUNTIME_DIR/worker-a.sock\"");
    fails(&words_b, "cat /tmp/worker-a.sock");
    fails(
        &words_b,
        &format!(
            "test -e '{}'",
            fixture.runtime().join("supervisor.sock").display()
        ),
    );
    if engine == "claude" {
        assert_eq!(ok(&words_b, &format!("ls -A '{}'", cc_socks.display())), "");
        ok(
            &words_b,
            &format!("echo b > '{}/probe-b'", cc_socks.display()),
        );
        assert!(
            !cc_socks.join("probe-b").exists(),
            "cc-socks leaked onto host"
        );
    }

    let out_a = child_a.wait_with_output().expect("wait worker-a");
    assert!(out_a.status.success(), "{out_a:?}");
    let _ = std::fs::remove_file(&ready_file);
}

#[test]
fn runtime_and_socket_dirs_are_isolated_across_workers_for_all_three_engines() {
    let _serial = exclusive();
    let fixture = Fixture::new();
    let claude_managed = fixture.home().join(".local/state/spawn/claude-config");
    let custom_codex = fixture.scratch.join("codex-home");
    write(&fixture.home().join(".cc-socks/peer.canary"), "host-sock");
    write(&claude_managed.join("cc-socks/host.canary"), "claude-sock");
    std::fs::create_dir_all(&custom_codex).expect("codex home");
    let _host_sock = bind_host_socket_if_fits(&fixture.runtime().join("supervisor.sock"));
    let _peer_sock = bind_host_socket_if_fits(&fixture.home().join(".cc-socks/peer.sock"));
    let _claude_sock = bind_host_socket_if_fits(&claude_managed.join("cc-socks/claude.sock"));

    let mut cfg = fixture.config();
    cfg.extra_mounts = Vec::new();
    let Some(view) = view(cfg) else {
        return;
    };
    let agy_home = fixture.engine_home();
    for (engine, engine_home) in [
        ("claude", claude_managed.as_path()),
        ("agy", agy_home.as_path()),
        ("codex", custom_codex.as_path()),
    ] {
        run_socket_isolation_check(&view, &fixture, engine, engine_home);
    }
}

fn seed_codex_whitelist_fixture(fixture: &Fixture) -> (PathBuf, PathBuf, PathBuf) {
    let codex_home = fixture.home().join(".codex");
    write(&codex_home.join("config.toml"), "model = \"gpt-5.4\"\n");
    write(&codex_home.join("auth.json"), "{\"token\":\"codex-tok\"}\n");
    write(&codex_home.join("history.jsonl"), "host-codex-history\n");
    std::fs::create_dir_all(codex_home.join("sessions")).expect("sessions");
    write(
        &fixture.home().join(".claude/settings.json"),
        "{\"sec\":1}\n",
    );
    write(&fixture.home().join(".ssh/id_rsa"), "SSH_KEY");

    let real_bin = fixture.home().join(".local/opt/codex/bin/codex-real");
    let link_bin = fixture.home().join(".local/bin/codex");
    let sibling_secret = fixture.home().join(".local/bin/sibling-secret");
    write(&real_bin, "#!/bin/sh\necho codex-ok\n");
    write(&sibling_secret, "#!/bin/sh\necho sibling\n");
    std::fs::set_permissions(&real_bin, std::fs::Permissions::from_mode(0o755))
        .expect("chmod real");
    std::fs::set_permissions(&sibling_secret, std::fs::Permissions::from_mode(0o755))
        .expect("chmod sib");
    std::fs::create_dir_all(link_bin.parent().unwrap()).expect("bin parent");
    std::os::unix::fs::symlink(&real_bin, &link_bin).expect("symlink");
    (codex_home, link_bin, sibling_secret)
}

#[test]
fn codex_data_dir_isolates_workers_protects_host_and_allows_whitelist_binary() {
    let _serial = exclusive();
    let fixture = Fixture::new();
    let (codex_home, link_bin, sibling_secret) = seed_codex_whitelist_fixture(&fixture);

    let mut cfg = fixture.config();
    cfg.home_isolation = spawn_host::HomeIsolation::Whitelist;
    cfg.canonical_workspace = Some(PathBuf::from("/workspace"));
    cfg.engines.codex = Some(EnginePaths::Both {
        program: link_bin.clone(),
        data: fixture.scratch.join("codex-data"),
    });
    let Some(view) = view(cfg) else {
        return;
    };
    let ws = fixture.workspace().display().to_string();
    let wrap = |worker: &str| {
        view.wrap(&ViewedLaunch {
            engine: "codex",
            workspace: &ws,
            engine_home: &codex_home.display().to_string(),
            worker_id: Some(worker),
            exit_file: None,
        })
        .expect("wrap codex")
    };
    let (w1, w2) = (wrap("codex-w1"), wrap("codex-w2"));

    assert_eq!(ok(&w1, &link_bin.display().to_string()), "codex-ok");
    for secret in [
        sibling_secret,
        fixture.home().join(".ssh/id_rsa"),
        fixture.home().join(".claude/settings.json"),
        codex_home.join("history.jsonl"),
    ] {
        fails(&w1, &format!("cat '{}'", secret.display()));
    }
    assert_eq!(
        ok(
            &w1,
            &format!("cat '{}'", codex_home.join("config.toml").display())
        ),
        "model = \"gpt-5.4\""
    );
    assert_eq!(
        ok(
            &w1,
            &format!("cat '{}'", codex_home.join("auth.json").display())
        ),
        "{\"token\":\"codex-tok\"}"
    );
    fails(
        &w1,
        &format!(
            "sh -c 'echo x > \"{}\"'",
            codex_home.join("auth.json").display()
        ),
    );
    assert_eq!(
        ok(
            &w1,
            "printf '%s|%s' \"$CODEX_HOME\" \"${CLAUDE_CONFIG_DIR-unset}\""
        ),
        format!("{}|unset", codex_home.display())
    );

    ok(
        &w1,
        &format!("echo w1-private > '{}/log/w1.log'", codex_home.display()),
    );
    fails(&w2, &format!("cat '{}/log/w1.log'", codex_home.display()));
    assert!(!codex_home.join("log/w1.log").exists());

    let rollout = codex_home.join("sessions/rollout-1.jsonl");
    assert_eq!(
        ok(
            &w1,
            &format!(
                "printf 'ev\\n' > '{}' && cat '{}'",
                rollout.display(),
                rollout.display()
            )
        ),
        "ev"
    );
    assert_eq!(
        std::fs::read_to_string(&rollout).expect("persisted"),
        "ev\n"
    );
}

#[test]
fn all_three_engines_isolate_sibling_configs_and_scrub_cross_engine_env() {
    let _serial = exclusive();
    let fixture = Fixture::new();
    let home = fixture.home();
    let host_claude = home.join(".claude/settings.json");
    let managed_claude = home.join(".local/state/spawn/claude-config");
    let managed_settings = managed_claude.join("settings.json");
    let host_codex_auth = home.join(".codex/auth.json");
    let host_agy_oauth = home.join(".gemini/antigravity-cli/antigravity-oauth-token");
    write(&host_claude, "{\"hostClaudeSecret\":true}\n");
    write(&managed_settings, "{\"managed\":true}\n");
    write(&host_codex_auth, "{\"codexSecret\":true}\n");
    write(&host_agy_oauth, "agy-oauth-secret\n");
    let custom_codex = fixture.scratch.join("custom-codex");
    std::fs::create_dir_all(&custom_codex).expect("custom codex");

    let Some(view) = view(fixture.config()) else {
        return;
    };
    let ws = fixture.workspace().display().to_string();
    let dirty_env = [
        ("CLAUDE_CONFIG_DIR", "/polluted/claude"),
        ("CODEX_HOME", "/polluted/codex"),
    ];
    let agy_home = fixture.engine_home();
    let cases: [(&str, &Path, &str, Vec<&Path>, String); 3] = [
        (
            "claude",
            &managed_claude,
            "w-cl",
            vec![&host_claude, &host_codex_auth, &host_agy_oauth],
            format!("{}|unset", managed_claude.display()),
        ),
        (
            "agy",
            &agy_home,
            "w-ag",
            vec![&host_claude, &managed_settings, &host_codex_auth],
            "unset|unset".to_string(),
        ),
        (
            "codex",
            &custom_codex,
            "w-cx",
            vec![
                &host_codex_auth,
                &host_claude,
                &managed_settings,
                &host_agy_oauth,
            ],
            format!("unset|{}", custom_codex.display()),
        ),
    ];
    for (engine, engine_home, worker, hidden_files, expected_env) in cases {
        let words = view
            .wrap(&ViewedLaunch {
                engine,
                workspace: &ws,
                engine_home: &engine_home.display().to_string(),
                worker_id: Some(worker),
                exit_file: None,
            })
            .expect(engine);
        assert_eq!(
            ok_with_env(
                &words,
                &dirty_env,
                "printf '%s|%s' \"${CLAUDE_CONFIG_DIR-unset}\" \"${CODEX_HOME-unset}\"",
            ),
            expected_env
        );
        if engine == "claude" {
            assert_eq!(
                ok(&words, &format!("cat '{}'", managed_settings.display())),
                "{\"managed\":true}"
            );
        }
        for hidden in hidden_files {
            fails(&words, &format!("cat '{}'", hidden.display()));
        }
    }
}

#[test]
fn container_view_isolates_per_worker_runtime_and_cross_engine_env_across_all_three_engines() {
    let fixture = Fixture::new();
    let mut cfg = fixture.config();
    let codex_data = fixture.scratch.join("ctr-codex-data");
    cfg.engines.codex = Some(EnginePaths::Data(codex_data.clone()));
    let view = spawn_host::ContainerView::new_with_detector(cfg, &|| spawn_host::ContainerEnv {
        in_container: true,
        runtime: Some("docker".to_string()),
        signals: vec!["/.dockerenv".to_string()],
    })
    .expect("container view");

    let (workspace, agy_home, _) = fixture.launch();
    let claude_home = fixture.scratch.join("ctr-claude");
    let codex_home = fixture.scratch.join("ctr-codex");
    for (engine, eh, worker, expect_cfg) in [
        ("claude", claude_home.as_path(), "w-c", "claude|unset"),
        ("agy", Path::new(&agy_home), "w-a", "unset|unset"),
        ("codex", codex_home.as_path(), "w-x", "unset|codex"),
    ] {
        let words = view
            .wrap(&ViewedLaunch {
                engine,
                workspace: &workspace,
                engine_home: &eh.display().to_string(),
                worker_id: Some(worker),
                exit_file: None,
            })
            .expect("wrap");
        let out = ok_with_env(
            &words,
            &[
                ("CLAUDE_CONFIG_DIR", "/dirty/claude"),
                ("CODEX_HOME", "/dirty/codex"),
            ],
            "[ -d \"$XDG_RUNTIME_DIR\" ] && [ -d \"$TMPDIR\" ] && \
             printf '%s|%s|%s' \"$XDG_RUNTIME_DIR\" \"${CLAUDE_CONFIG_DIR-unset}\" \"${CODEX_HOME-unset}\"",
        );
        let mut parts = out.split('|');
        let rt = parts.next().expect("rt");
        let claude_env = parts.next().expect("claude env");
        let codex_env = parts.next().expect("codex env");
        assert_ne!(rt, fixture.runtime().display().to_string());
        match expect_cfg {
            "claude|unset" => {
                assert_eq!(claude_env, claude_home.display().to_string());
                assert_eq!(codex_env, "unset");
            }
            "unset|unset" => assert_eq!((claude_env, codex_env), ("unset", "unset")),
            "unset|codex" => {
                assert!(
                    rt.starts_with(&codex_data.join("workers/w-x").display().to_string()),
                    "{rt}"
                );
                assert_eq!(claude_env, "unset");
                assert_eq!(codex_env, codex_home.display().to_string());
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn whitelist_program_symlink_into_masked_dir_binds_nothing_for_plain_and_symlinked_home() {
    let _serial = exclusive();
    for symlinked_home in [false, true] {
        let fixture = Fixture::new();
        write(&fixture.workspace().join("ok.txt"), "ws-ok");
        let home = if symlinked_home {
            let link_home = fixture.scratch.join("link-home");
            std::os::unix::fs::symlink(fixture.home(), &link_home).expect("symlink home");
            link_home
        } else {
            fixture.home()
        };
        let canon_home = std::fs::canonicalize(&home).expect("canon home");

        for (label, canary) in [
            ("ssh", home.join(".ssh/exec-canary")),
            ("keep", fixture.keep().join("keep-canary")),
        ] {
            write(&canary, "#!/bin/sh\necho leaked\n");
            std::fs::set_permissions(&canary, std::fs::Permissions::from_mode(0o755))
                .expect("chmod canary");
            let canon_canary = std::fs::canonicalize(&canary).expect("canon canary");

            let link_bin = home.join(format!(".local/bin/claude-{label}-canary"));
            std::fs::create_dir_all(link_bin.parent().expect("bin parent")).expect("mkdir bin");
            std::os::unix::fs::symlink(&canary, &link_bin).expect("symlink bin");

            let mut cfg = fixture.config();
            cfg.home = home.clone();
            cfg.home_isolation = spawn_host::HomeIsolation::Whitelist;
            cfg.canonical_workspace = Some(PathBuf::from("/workspace"));
            cfg.engines.claude = Some(EnginePaths::Program(link_bin.clone()));
            let Some(view) = view(cfg) else {
                return;
            };
            let ws = fixture.workspace().display().to_string();
            let claude_home = canon_home.join(".claude").display().to_string();
            let words = view
                .wrap(&ViewedLaunch {
                    engine: "claude",
                    workspace: &ws,
                    engine_home: &claude_home,
                    worker_id: Some("w-masked-prog"),
                    exit_file: None,
                })
                .expect("wrap");

            let canary_name = format!("{label}-canary");
            assert!(
                !triples(&words, "--ro-bind").iter().any(|(src, dst)| {
                    src.contains(&canary_name)
                        || dst.contains(&canary_name)
                        || *src == canary.to_string_lossy()
                        || *src == canon_canary.to_string_lossy()
                        || *dst == link_bin.to_string_lossy()
                }),
                "unexpected ro-bind for masked program target (symlinked_home={symlinked_home}, label={label}): {words:?}"
            );
            assert!(
                !words.iter().any(|w| w.contains(&canary_name)),
                "unexpected bwrap word exposing masked program target (symlinked_home={symlinked_home}, label={label}): {words:?}"
            );

            assert_eq!(ok(&words, "cat /workspace/ok.txt"), "ws-ok");
            let canon_link = canon_home.join(format!(".local/bin/claude-{label}-canary"));
            for blocked in [&link_bin, &canon_link, &canary, &canon_canary] {
                fails(&words, &format!("cat '{}'", blocked.display()));
                fails(&words, &format!("'{}'", blocked.display()));
            }
        }
    }
}
