//! The filesystem view an engine without deny rules of its own runs in
//! (decision C2, N6; `Guard::Sandbox`): `bwrap`.
//!
//! Claude Code is kept off the host's credentials by the deny rules it is
//! handed at launch. agy (and any other `Guard::Sandbox` engine) has none,
//! so the Bridge starts it inside a view of the host's filesystem
//! ([`BwrapView`]) where the credentials, the other engines' homes, the
//! loop's environment files and every terminal server's socket are not
//! there. The launch script Herdr's pane runs ends in
//!
//! ```text
//! exec bwrap --ro-bind / / --dev /dev --proc /proc --die-with-parent --unshare-pid \
//!     <masks> --bind <ws> <ws> --bind <engine home> <engine home> \
//!     --setenv TMPDIR <TMPDIR> --chdir <ws> -- <runner> <engine> <args>
//! ```
//!
//! where `<TMPDIR>` is the engine's own, `<engine home>/.spawn-tmp`
//! ([`TMP_FALLBACK`], 0700): never the loop's shared `TMPDIR`, which other
//! host jobs write to and which is masked whole. It is writable through
//! the engine home's bind and never bound again: a second bind would
//! resolve its path anew when `bwrap` runs, after a writer outside the view
//! could swap it.
//!
//! where `<runner> <engine> <args>` is the usual `spawn run -- <engine>
//! <args>`: the runner stays the engine's parent, and writes the exit
//! status into the workspace, which is writable.
//!
//! Binding rules (probe under herdr 0.9.5 and bwrap 0.9.0):
//!
//! - **Never `--new-session`**: `setsid` takes the engine out of the pane's
//!   foreground process group, and Herdr's `agent.get` answers
//!   `agent_not_found`.
//! - **Always `--unshare-pid`**: the engine cannot `kill(2)` a host
//!   process; Herdr still detects it.
//! - Never `--unshare-net`, `--unshare-all` nor `--as-pid-1`.
//! - **Masks first, writable binds last**: everything is read-only
//!   (`--ro-bind / /`), each mask hides one path, and only then are the
//!   workspace and the engine's home (its `TMPDIR` inside) bound writable,
//!   each at its own exact path, so the Bridge reads the transcripts from
//!   the host where the engine writes them. Every writable bind is checked
//!   against every mask target, a nested one included; only the words leave
//!   out a mask inside another.
//! - **No writable bind overlaps a mask or the view's private directory**:
//!   a workspace that holds or lies in a masked path is refused, and so is
//!   an engine home that is or holds one (a misconfigured `SPAWN_AGY_HOME`
//!   of `~/.gemini`, `~/.claude` or `/run/keep` is refused, never
//!   unmasked). The one exception is an engine home below another engine's
//!   masked home ([`ENGINE_HOMES`]: `~/.gemini/antigravity-cli` under
//!   `~/.gemini`), bound back into the mask's empty tmpfs.
//!
//! The masks, resolved again at every launch ([`BwrapView::plan`]):
//!
//! - directories, each an empty `--tmpfs`: `/run/keep`, `/run/keep-test`,
//!   the whole `$XDG_RUNTIME_DIR`, the whole `/tmp` (the fleet's tmux
//!   sockets), the loop's shared `TMPDIR`, the Herdr session directory (`$XDG_CONFIG_HOME/herdr`, by
//!   default `~/.config/herdr`, which holds its socket: a client in the view
//!   could otherwise ask Herdr for a pane outside it), `~/.claude`, every
//!   other engine's home (`~/.gemini` but for the engine's own home below
//!   it, `~/.config/antigravity`, `~/.codex`, `~/.hermes`), `~/.ssh`,
//!   `~/.gnupg`, `~/.config/gh`, `~/.aws`, `~/.config/anthropic`,
//!   `~/.config/spawn`, `~/.config/cloudflare` and
//!   `~/.config/observe3-private` (each `~/.config/<x>` also below
//!   `$XDG_CONFIG_HOME` when that is set elsewhere);
//! - regular files, each read-only bound over by a private empty 0444 file
//!   (never `/dev/null`, whose bind answers `EACCES` instead of reading
//!   empty), kept in a 0700 directory inside the masked runtime directory
//!   (`$XDG_RUNTIME_DIR/spawn-engine-view-<uid>`), so the engine can
//!   neither reach, rewrite nor swap it: the loop's `spawn.env` and every other `*.env*` and `env.sh` in
//!   the loop's directory (`~/.local/share/spawn-rsi`), `~/.claude.json`,
//!   `~/.pgpass`, `~/.netrc` and `~/.git-credentials`.
//!
//! Each target is canonicalized (`--tmpfs` over a symbolic link aborts
//! `bwrap`) and masked by what it resolves to: a directory by a tmpfs, a
//! regular file by the empty file. A missing target, or one of another
//! type (a socket, a fifo, a device), is skipped and said
//! ([`Plan::skipped`]): one missing target would abort `bwrap`.
//!
//! **Known residual:** a target missing at a launch that appears later is
//! visible to the engine already running (`--ro-bind / /` is live); the
//! next launch masks it.

use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use spawn_core::port::{EngineView, ViewedLaunch};

/// The longest the functional check of `bwrap` may take.
const CHECK_LIMIT: Duration = Duration::from_secs(10);

/// The directory below the engine's home that is its `TMPDIR` in the view.
pub const TMP_FALLBACK: &str = ".spawn-tmp";

/// The engines' homes masked below the user's home: the only masked
/// directories an engine's own home may lie below (re-bound writable into
/// the mask's tmpfs, `~/.gemini/antigravity-cli`), never equal to.
pub const ENGINE_HOMES: &[&str] = &[".gemini", ".config/antigravity", ".codex", ".hermes"];

/// The directories masked below the user's home, as written (a
/// `.config/<x>` one also below `$XDG_CONFIG_HOME` when that is set).
pub const HOME_DIRECTORIES: &[&str] = &[
    ".claude",
    ".gemini",
    ".config/antigravity",
    ".codex",
    ".hermes",
    ".ssh",
    ".gnupg",
    ".config/gh",
    ".aws",
    ".config/anthropic",
    ".config/spawn",
    ".config/cloudflare",
    ".config/observe3-private",
];

/// The regular files masked below the user's home.
pub const HOME_FILES: &[&str] = &[".claude.json", ".pgpass", ".netrc", ".git-credentials"];

/// The directories masked wherever the home is.
pub const HOST_DIRECTORIES: &[&str] = &["/run/keep", "/run/keep-test", "/tmp"];

/// The loop's directory below the user's home, whose environment files
/// (`*.env*`, `env.sh`) are masked.
pub const LOOP_DIRECTORY: &str = ".local/share/spawn-rsi";

/// How `$HOME` is isolated inside the filesystem view
/// (`SPAWN_SANDBOX_HOME_ISOLATION`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HomeIsolation {
    /// Mask known credential and sibling-engine paths below `$HOME` (default).
    #[default]
    Blacklist,
    /// Mask the entire `$HOME` with an empty tmpfs and re-bind only allowed
    /// paths (`SPAWN_SANDBOX_HOME_ISOLATION=whitelist`).
    Whitelist,
}

/// How host user configuration and toolchains are inherited inside the view
/// (`SPAWN_SANDBOX_INHERIT_HOST`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HostInheritMode {
    /// Do not automatically expose `$HOME` toolchains or git config (default).
    #[default]
    Off,
    /// Automatically read-only bind safe host toolchains and secret-free
    /// `.gitconfig` (`SPAWN_SANDBOX_INHERIT_HOST=auto`).
    Auto,
}

/// An extra host path bound into the filesystem view
/// (`SPAWN_ENGINE_VIEW_WRITABLE`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtraMount {
    /// Host path to bind at the same path inside the view.
    pub path: PathBuf,
    /// `true` for `--bind` (`:rw`, the default); `false` for `--ro-bind` (`:ro`).
    pub writable: bool,
}

/// What a view hides and where it keeps its own empty file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewConfig {
    /// The user's home (`$HOME`).
    pub home: PathBuf,
    /// `$XDG_CONFIG_HOME`, when set: its `herdr` and `herdr-core` are masked too.
    pub xdg_config_home: Option<PathBuf>,
    /// `$XDG_STATE_HOME`, when set: its `herdr-core` is masked too.
    pub xdg_state_home: Option<PathBuf>,
    /// The runtime directory (`$XDG_RUNTIME_DIR`, else `/run/user/<uid>`).
    pub runtime_dir: PathBuf,
    /// The directories masked wherever the home is ([`HOST_DIRECTORIES`];
    /// a test names its own beside them).
    pub host_directories: Vec<PathBuf>,
    /// The loop's directories, whose `*.env*` and `env.sh` are masked.
    pub loop_dirs: Vec<PathBuf>,
    /// More regular files to mask (the loop's environment file when it
    /// lies elsewhere).
    pub extra_files: Vec<PathBuf>,
    /// The `TMPDIR` the loop runs with (shared host scratch, which other
    /// host jobs write to): masked whole, never bound. The engine gets a
    /// `TMPDIR` of its own ([`TMP_FALLBACK`]).
    pub host_tmpdir: Option<PathBuf>,
    /// A directory only this user enters, inside the masked runtime
    /// directory (`$XDG_RUNTIME_DIR/spawn-engine-view-<uid>`), where the
    /// empty file a regular file is masked by is kept. No writable bind
    /// may reach it.
    pub private_dir: PathBuf,
    /// Optional fixed workspace mount path inside the sandbox (e.g.
    /// `/workspace`, `SPAWN_SANDBOX_CANONICAL_WORKSPACE`).
    pub canonical_workspace: Option<PathBuf>,
    /// `$HOME` isolation policy (`SPAWN_SANDBOX_HOME_ISOLATION`).
    pub home_isolation: HomeIsolation,
    /// Additional mounts from `SPAWN_ENGINE_VIEW_WRITABLE`.
    pub extra_mounts: Vec<ExtraMount>,
    /// Optional off-home data root for `agy` (`SPAWN_AGY_DATA_DIR`).
    pub agy_data_dir: Option<PathBuf>,
    /// Whether per-worker `agy` homes and temp dirs are isolated under
    /// `agy_data_dir` (`SPAWN_AGY_ISOLATE_WORKERS != "0"` when `agy_data_dir`
    /// is set).
    pub isolate_workers: bool,
    /// Whether each worker's `agy` `brain/` directory is isolated per worker
    /// under `agy_data_dir` (`SPAWN_AGY_ISOLATE_BRAIN=1|true|yes|on` when
    /// `agy_data_dir` is set; default `false` so `brain/` is shared).
    pub isolate_brain: bool,
    /// Executable path of `agy` (`SPAWN_AGY_PROGRAM` else `agy` on `PATH`),
    /// RO-bound in whitelist mode if located under `$HOME`.
    pub agy_program: Option<PathBuf>,
    /// Safe host toolchain/config inheritance policy (`SPAWN_SANDBOX_INHERIT_HOST`).
    pub inherit_host: HostInheritMode,
    /// Explicit host paths read-only inherited (`SPAWN_SANDBOX_INHERIT_HOST_PATHS`).
    pub inherit_host_paths: Vec<PathBuf>,
    /// Executable path of `claude` (`SPAWN_CLAUDE_PROGRAM` else `claude` on `PATH`),
    /// RO-bound in whitelist mode if located under `$HOME`.
    pub claude_program: Option<PathBuf>,
    /// Optional off-home data root for `claude` (`SPAWN_CLAUDE_DATA_DIR`).
    pub claude_data_dir: Option<PathBuf>,
}

fn trim_trailing_slash(raw: &str) -> &str {
    let trimmed = raw.trim_end_matches('/');
    if trimmed.is_empty() && raw.starts_with('/') {
        "/"
    } else {
        trimmed
    }
}

/// Parse `SPAWN_ENGINE_VIEW_WRITABLE` entries separated by `,` (or legacy `:`
/// when neither `:ro` nor `:rw` suffix is given), defaulting to `:rw`.
#[must_use]
pub fn parse_extra_mounts(raw: &str) -> Vec<ExtraMount> {
    let mut mounts = Vec::new();
    for entry in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(path) = entry.strip_suffix(":ro") {
            let trimmed = trim_trailing_slash(path.trim());
            if !trimmed.is_empty() {
                mounts.push(ExtraMount {
                    path: PathBuf::from(trimmed),
                    writable: false,
                });
            }
        } else if let Some(path) = entry.strip_suffix(":rw") {
            let trimmed = trim_trailing_slash(path.trim());
            if !trimmed.is_empty() {
                mounts.push(ExtraMount {
                    path: PathBuf::from(trimmed),
                    writable: true,
                });
            }
        } else {
            for part in entry.split(':').map(str::trim).filter(|s| !s.is_empty()) {
                mounts.push(ExtraMount {
                    path: PathBuf::from(trim_trailing_slash(part)),
                    writable: true,
                });
            }
        }
    }
    mounts
}

/// Parse `SPAWN_SANDBOX_INHERIT_HOST_PATHS` entries separated by `,` or `:`.
#[must_use]
pub fn parse_inherit_host_paths(raw: &str) -> Vec<PathBuf> {
    raw.split([',', ':'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(trim_trailing_slash)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect()
}

impl ViewConfig {
    /// The view of this host's user, from the environment `lookup` reads.
    pub fn from_env(lookup: &dyn Fn(&str) -> Option<String>) -> Self {
        let uid = crate::current_uid();
        let home = lookup("HOME")
            .filter(|home| home.starts_with('/'))
            .map(PathBuf::from)
            .or_else(|| crate::home::Home::of_account().path)
            .unwrap_or_else(|| PathBuf::from("/root"));
        let runtime_dir = lookup("XDG_RUNTIME_DIR")
            .filter(|dir| dir.starts_with('/'))
            .map_or_else(|| PathBuf::from(format!("/run/user/{uid}")), PathBuf::from);
        let private_dir = runtime_dir.join(format!("spawn-engine-view-{uid}"));
        let canonical_workspace = lookup("SPAWN_SANDBOX_CANONICAL_WORKSPACE")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("off"))
            .map(|value| PathBuf::from(trim_trailing_slash(&value)));
        let home_isolation = match lookup("SPAWN_SANDBOX_HOME_ISOLATION")
            .as_deref()
            .map(str::trim)
        {
            Some(value) if value.eq_ignore_ascii_case("whitelist") => HomeIsolation::Whitelist,
            _ => HomeIsolation::Blacklist,
        };
        let extra_mounts = lookup("SPAWN_ENGINE_VIEW_WRITABLE")
            .map(|raw| parse_extra_mounts(&raw))
            .unwrap_or_default();
        let agy_data_dir = lookup("SPAWN_AGY_DATA_DIR")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(|value| PathBuf::from(trim_trailing_slash(&value)));
        let isolate_workers = agy_data_dir.is_some()
            && lookup("SPAWN_AGY_ISOLATE_WORKERS")
                .as_deref()
                .map(str::trim)
                != Some("0");
        let isolate_brain = agy_data_dir.is_some()
            && matches!(
                lookup("SPAWN_AGY_ISOLATE_BRAIN")
                    .as_deref()
                    .map(str::trim)
                    .map(str::to_ascii_lowercase)
                    .as_deref(),
                Some("1" | "true" | "yes" | "on")
            );
        let agy_cmd = lookup("SPAWN_AGY_PROGRAM")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "agy".to_string());
        let agy_program = find_program_entry(&agy_cmd, lookup("PATH").as_deref());
        let inherit_host = match lookup("SPAWN_SANDBOX_INHERIT_HOST")
            .as_deref()
            .map(str::trim)
        {
            Some(value) if value.eq_ignore_ascii_case("auto") => HostInheritMode::Auto,
            _ => HostInheritMode::Off,
        };
        let inherit_host_paths = lookup("SPAWN_SANDBOX_INHERIT_HOST_PATHS")
            .map(|raw| parse_inherit_host_paths(&raw))
            .unwrap_or_default();
        let claude_cmd = lookup("SPAWN_CLAUDE_PROGRAM")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "claude".to_string());
        let claude_program = find_program_entry(&claude_cmd, lookup("PATH").as_deref());
        let claude_data_dir = lookup("SPAWN_CLAUDE_DATA_DIR")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(|value| PathBuf::from(trim_trailing_slash(&value)));
        Self {
            xdg_config_home: lookup("XDG_CONFIG_HOME")
                .filter(|dir| dir.starts_with('/'))
                .map(PathBuf::from),
            xdg_state_home: lookup("XDG_STATE_HOME")
                .filter(|dir| dir.starts_with('/'))
                .map(PathBuf::from),
            host_directories: HOST_DIRECTORIES.iter().map(PathBuf::from).collect(),
            loop_dirs: vec![home.join(LOOP_DIRECTORY)],
            extra_files: Vec::new(),
            host_tmpdir: lookup("TMPDIR")
                .filter(|dir| dir.starts_with('/'))
                .map(PathBuf::from),
            private_dir,
            runtime_dir,
            home,
            canonical_workspace,
            home_isolation,
            extra_mounts,
            agy_data_dir,
            isolate_workers,
            isolate_brain,
            agy_program,
            inherit_host,
            inherit_host_paths,
            claude_program,
            claude_data_dir,
        }
    }

    /// The directory targets, as written (not yet resolved).
    fn directory_targets(&self) -> Vec<PathBuf> {
        let mut targets = self.host_directories.clone();
        targets.push(self.runtime_dir.clone());
        targets.push(self.private_dir.clone());
        if let Some(tmpdir) = &self.host_tmpdir {
            targets.push(tmpdir.clone());
        }
        for target in [
            self.home.join(".config/herdr"),
            self.home.join(".config/herdr-core"),
            self.home.join(".local/state/herdr-core"),
        ] {
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
        if let Some(config) = &self.xdg_config_home {
            for name in ["herdr", "herdr-core"] {
                let target = config.join(name);
                if !targets.contains(&target) {
                    targets.push(target);
                }
            }
        }
        if let Some(state) = &self.xdg_state_home {
            let target = state.join("herdr-core");
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
        for dir in HOME_DIRECTORIES {
            for target in self.below_home(dir) {
                if !targets.contains(&target) {
                    targets.push(target);
                }
            }
        }
        targets
    }

    /// `dir` below the home, and below `$XDG_CONFIG_HOME` too for a
    /// `.config/<x>` one when that is set elsewhere.
    fn below_home(&self, dir: &str) -> Vec<PathBuf> {
        let mut paths = vec![self.home.join(dir)];
        if let (Some(config), Some(rest)) = (&self.xdg_config_home, dir.strip_prefix(".config/")) {
            let other = config.join(rest);
            if !paths.contains(&other) {
                paths.push(other);
            }
        }
        paths
    }

    /// The regular-file targets, as written: the fixed ones and every
    /// environment file in the loop's directories now.
    fn file_targets(&self) -> Vec<PathBuf> {
        let mut targets: Vec<PathBuf> =
            HOME_FILES.iter().map(|file| self.home.join(file)).collect();
        for dir in &self.loop_dirs {
            targets.push(dir.join("spawn.env"));
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut found: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_str()
                        .is_some_and(|name| name.contains(".env") || name == "env.sh")
                })
                .map(|entry| entry.path())
                .collect();
            found.sort();
            targets.extend(found);
        }
        targets.extend(self.extra_files.iter().cloned());
        targets
    }
}

/// One mask of a plan, by the path it resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mask {
    /// A directory, hidden by an empty tmpfs.
    Directory(PathBuf),
    /// A regular file, hidden by the private empty file.
    File(PathBuf),
}

/// What one launch's view is: its `bwrap` words and what it skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// `bwrap` and its arguments, up to and including `--`.
    pub words: Vec<String>,
    /// The masks, in order.
    pub masks: Vec<Mask>,
    /// The `TMPDIR` the engine gets.
    pub tmpdir: PathBuf,
    /// Each target not masked, and why (missing, not a directory nor a
    /// regular file, unreadable).
    pub skipped: Vec<(PathBuf, String)>,
}

/// The `bwrap` view (see the module's documentation).
#[derive(Debug, Clone)]
pub struct BwrapView {
    program: PathBuf,
    config: ViewConfig,
}

impl BwrapView {
    /// The view through `program` (`bwrap` is looked up on `PATH`), once
    /// it has shown it works on this host: it starts `/bin/true` in a view
    /// of the same kind (same namespaces, a tmpfs and a bind) within
    /// [`CHECK_LIMIT`].
    pub fn new(program: &str, config: ViewConfig) -> Result<Self, String> {
        let program = find_program(program)
            .ok_or_else(|| format!("{program} is not installed (not found on PATH)"))?;
        let view = Self { program, config };
        view.validate_config()?;
        let private = view.private_dir()?;
        view.check(&private)?;
        Ok(view)
    }

    /// The program the view runs, as found.
    pub fn program(&self) -> &Path {
        &self.program
    }

    /// What the view is configured with.
    pub fn config(&self) -> &ViewConfig {
        &self.config
    }

    /// Run `/bin/true` in a view of the same kind, binding `probe` (the
    /// view's private directory, which exists by now) writable.
    fn check(&self, probe: &Path) -> Result<(), String> {
        let mut command = Command::new(&self.program);
        command
            .args(["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"])
            .args(["--die-with-parent", "--unshare-pid", "--tmpfs", "/tmp"])
            .arg("--bind")
            .arg(probe)
            .arg(probe)
            .args(["--", "/bin/true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| format!("{}: {error}", self.program.display()))?;
        let deadline = Instant::now() + CHECK_LIMIT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(status)) => {
                    let mut said = String::new();
                    if let Some(mut stderr) = child.stderr.take() {
                        let _ = std::io::Read::read_to_string(&mut stderr, &mut said);
                    }
                    return Err(format!(
                        "{} does not work on this host ({status}): {}",
                        self.program.display(),
                        said.lines().next().unwrap_or("no message")
                    ));
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "{} did not finish its check within {} s",
                        self.program.display(),
                        CHECK_LIMIT.as_secs()
                    ));
                }
                Err(error) => return Err(format!("{}: {error}", self.program.display())),
            }
        }
    }

    /// The view's private directory, canonical: made 0700 (never more)
    /// when missing, then checked to be a directory of this user's that no
    /// one else may enter.
    fn private_dir(&self) -> Result<PathBuf, String> {
        let dir = &self.config.private_dir;
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("create {}: {error}", dir.display())),
        }
        if !crate::process::private_to_us(dir) {
            return Err(format!(
                "{} is not a directory only this user may enter",
                dir.display()
            ));
        }
        std::fs::canonicalize(dir).map_err(|error| format!("{}: {error}", dir.display()))
    }

    /// The private empty file a regular file is masked by: 0444, empty,
    /// this user's, in the view's private directory. When it is not so, a
    /// fresh one is made (`create_new`, 0444 from the start) and renamed
    /// over it, never removed first.
    fn empty_file(&self) -> Result<PathBuf, String> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = self.private_dir()?;
        let file = dir.join("empty");
        let sound = |path: &Path| {
            std::fs::symlink_metadata(path).is_ok_and(|meta| {
                meta.is_file()
                    && meta.len() == 0
                    && meta.mode() & 0o7777 == 0o444
                    && meta.uid() == crate::current_uid()
            })
        };
        if sound(&file) {
            return Ok(file);
        }
        let ordinal = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let partial = dir.join(format!("empty.{}.{ordinal}", std::process::id()));
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o444)
            .open(&partial)
            .map_err(|error| format!("create {}: {error}", partial.display()))?;
        if let Err(error) = std::fs::rename(&partial, &file) {
            let _ = std::fs::remove_file(&partial);
            return Err(format!("rename to {}: {error}", file.display()));
        }
        if sound(&file) {
            Ok(file)
        } else {
            Err(format!(
                "{} is not a private empty 0444 file",
                file.display()
            ))
        }
    }

    /// Validate [`ViewConfig`] invariants (`canonical_workspace`,
    /// `extra_mounts`, `agy_data_dir`, `claude_data_dir`, `inherit_host_paths`,
    /// and whitelist `$HOME`).
    fn validate_config(&self) -> Result<(), String> {
        validate_config_for(&self.config)
    }

    /// Every target resolved now: the directories and the regular files to
    /// mask, and what was skipped and why. All of them, a target inside
    /// another masked directory included: the writable binds are checked
    /// against every one ([`BwrapView::refuse_binds_for`]), and only the words
    /// leave the nested ones out ([`prune_nested`]).
    fn targets_for(&self, engine: &str) -> (Vec<PathBuf>, Vec<PathBuf>, Vec<(PathBuf, String)>) {
        let home =
            std::fs::canonicalize(&self.config.home).unwrap_or_else(|_| self.config.home.clone());
        let mut skipped = Vec::new();
        let mut directories: Vec<PathBuf> = Vec::new();
        let mut files: Vec<PathBuf> = Vec::new();
        if self.config.home_isolation == HomeIsolation::Whitelist {
            match resolve(&self.config.home) {
                Ok(Resolved::Directory(path)) if path != Path::new("/") => {
                    directories.push(path);
                }
                Ok(Resolved::Directory(_)) => {
                    skipped.push((
                        self.config.home.clone(),
                        "holds the user's home".to_string(),
                    ));
                }
                Ok(Resolved::File(path)) => {
                    skipped.push((
                        self.config.home.clone(),
                        format!("is a file ({})", path.display()),
                    ));
                }
                Err(why) => skipped.push((self.config.home.clone(), why)),
            }
        }
        let omit_claude_dir = engine == "claude"
            && self.config.home_isolation == HomeIsolation::Blacklist
            && self.config.claude_data_dir.is_none();
        let claude_homes = if omit_claude_dir {
            self.canonical_claude_homes()
        } else {
            Vec::new()
        };
        let targets = self
            .config
            .directory_targets()
            .into_iter()
            .chain(self.config.file_targets());
        for target in targets {
            match resolve(&target) {
                Ok(Resolved::Directory(path))
                    if match self.config.home_isolation {
                        HomeIsolation::Blacklist => home.starts_with(&path),
                        HomeIsolation::Whitelist => home.starts_with(&path) && path != home,
                    } =>
                {
                    skipped.push((target, "holds the user's home".to_string()));
                }
                Ok(Resolved::Directory(path)) => {
                    let skip_dir = directories.contains(&path)
                        || (omit_claude_dir && claude_homes.iter().any(|c| c == &path));
                    if !skip_dir {
                        directories.push(path);
                    }
                }
                Ok(Resolved::File(path)) => {
                    if !files.contains(&path) {
                        files.push(path);
                    }
                }
                Err(why)
                    if self.config.home_isolation == HomeIsolation::Whitelist
                        && why == "missing"
                        && prospective_canonical(&target)
                            .is_ok_and(|path| path.starts_with(&home)) => {}
                Err(why) => skipped.push((target, why)),
            }
        }
        if self.config.inherit_host == HostInheritMode::Auto
            && self.config.home_isolation == HomeIsolation::Blacklist
            && engine != "claude"
        {
            for rel in [".cargo/credentials", ".cargo/credentials.toml"] {
                let candidate = self.config.home.join(rel);
                if let Ok(Resolved::File(path)) = resolve(&candidate)
                    && !files.contains(&path)
                {
                    files.push(path);
                }
            }
        }
        (directories, files, skipped)
    }

    fn canonical_claude_homes(&self) -> Vec<PathBuf> {
        self.config
            .below_home(".claude")
            .into_iter()
            .filter_map(|dir| {
                std::fs::canonicalize(&dir)
                    .ok()
                    .or_else(|| prospective_canonical(&dir).ok())
            })
            .collect()
    }

    fn canonical_engine_homes_for(&self, engine: &str) -> Vec<PathBuf> {
        let mut homes: Vec<PathBuf> = ENGINE_HOMES
            .iter()
            .flat_map(|dir| self.config.below_home(dir))
            .filter_map(|dir| {
                std::fs::canonicalize(&dir)
                    .ok()
                    .or_else(|| prospective_canonical(&dir).ok())
            })
            .collect();
        if engine == "claude" {
            homes.extend(self.canonical_claude_homes());
        }
        homes
    }

    fn forbidden_home_targets_for(&self, engine: &str) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for dir in HOME_DIRECTORIES.iter().copied().filter(|dir| {
            !(ENGINE_HOMES.contains(dir) || (engine == "claude" && *dir == ".claude"))
        }) {
            paths.extend(self.config.below_home(dir));
        }
        paths.extend(self.config.below_home(".config/herdr"));
        paths.extend(self.config.below_home(".config/herdr-core"));
        paths.push(self.config.home.join(".local/state/herdr-core"));
        if let Some(state) = &self.config.xdg_state_home {
            paths.push(state.join("herdr-core"));
        }
        paths.extend(HOME_FILES.iter().map(|file| self.config.home.join(file)));
        paths.extend(self.config.loop_dirs.iter().cloned());
        paths
            .into_iter()
            .filter_map(|path| {
                std::fs::canonicalize(&path)
                    .ok()
                    .or_else(|| prospective_canonical(&path).ok())
            })
            .collect()
    }

    /// Why a writable bind of `workspace` or `engine_home` is refused:
    /// it would uncover a mask, lie in one, or reach the view's `private`
    /// directory (see the module's documentation).
    fn refuse_binds_for(
        &self,
        engine: &str,
        ctx: &BindContext<'_>,
        engine_home: &Path,
    ) -> Result<(), String> {
        let home =
            std::fs::canonicalize(&self.config.home).unwrap_or_else(|_| self.config.home.clone());
        let whitelist = self.config.home_isolation == HomeIsolation::Whitelist;
        let reaches_private = |bound: &Path| paths_overlap(bound, ctx.private);
        if reaches_private(ctx.workspace_src) || reaches_private(ctx.workspace_dest) {
            return Err(format!(
                "the workspace {} overlaps the view's private directory {}",
                ctx.workspace_src.display(),
                ctx.private.display()
            ));
        }
        if let Some(target) = ctx.masked.iter().find(|target| {
            target.starts_with(ctx.workspace_src)
                || paths_overlap(ctx.workspace_dest, target)
                || (ctx.workspace_src.starts_with(target)
                    && (***target != home || self.config.canonical_workspace.is_none()))
        }) {
            return Err(format!(
                "the workspace {} overlaps {}, which the view masks",
                ctx.workspace_src.display(),
                target.display()
            ));
        }
        if self.config.canonical_workspace.is_none()
            && whitelist
            && (ctx.workspace_src.starts_with(&home) || home.starts_with(ctx.workspace_src))
        {
            return Err(format!(
                "the workspace {} overlaps $HOME {}, which the view masks",
                ctx.workspace_src.display(),
                home.display()
            ));
        }
        if engine_home == Path::new("/") || home.starts_with(engine_home) {
            return Err(format!(
                "the engine's home {} holds the user's home",
                engine_home.display()
            ));
        }
        if reaches_private(engine_home) {
            return Err(format!(
                "the engine's home {} overlaps the view's private directory {}",
                engine_home.display(),
                ctx.private.display()
            ));
        }
        let engine_homes = self.canonical_engine_homes_for(engine);
        let claude_homes = if engine == "claude" {
            self.canonical_claude_homes()
        } else {
            Vec::new()
        };
        if let Some(conflict) = engine_homes
            .iter()
            .find(|dir| dir.starts_with(engine_home) && !claude_homes.iter().any(|c| c == *dir))
        {
            return Err(format!(
                "the engine's home {} overlaps {}, which the view masks",
                engine_home.display(),
                conflict.display()
            ));
        }
        for forbidden in self.forbidden_home_targets_for(engine) {
            if (whitelist || engine == "claude") && paths_overlap(engine_home, &forbidden) {
                return Err(format!(
                    "the engine's home {} overlaps {}, which the view masks",
                    engine_home.display(),
                    forbidden.display()
                ));
            }
        }
        let allowed_claude_self =
            |target: &Path| engine == "claude" && claude_homes.iter().any(|dir| dir == target);
        let refused = ctx.masked.iter().find(|target| {
            let target_path = target.as_path();
            if target_path == engine_home && allowed_claude_self(target_path) {
                return false;
            }
            target.starts_with(engine_home)
                || (engine_home.starts_with(target)
                    && !engine_homes.iter().any(|dir| dir == **target)
                    && !(whitelist && target_path == home.as_path()))
        });
        if let Some(target) = refused {
            return Err(format!(
                "the engine's home {} overlaps {}, which the view masks",
                engine_home.display(),
                target.display()
            ));
        }
        Ok(())
    }

    fn refuse_extra_target(
        path: &Path,
        what: &str,
        ctx: &BindContext<'_>,
        home: &Path,
    ) -> Result<(), String> {
        if paths_overlap(path, ctx.private) {
            return Err(format!(
                "{what} {} overlaps the view's private directory {}",
                path.display(),
                ctx.private.display()
            ));
        }
        if let Some(target) = ctx.masked.iter().find(|target| paths_overlap(path, target)) {
            return Err(format!(
                "{what} {} overlaps {}, which the view masks",
                path.display(),
                target.display()
            ));
        }
        if paths_overlap(path, home) {
            return Err(format!(
                "{what} {} lies in or holds the user's home {}",
                path.display(),
                home.display()
            ));
        }
        if paths_overlap(path, ctx.workspace_dest) {
            return Err(format!(
                "{what} {} overlaps workspace mount {}",
                path.display(),
                ctx.workspace_dest.display()
            ));
        }
        Ok(())
    }

    /// Prepare off-home per-worker directories, one-time non-secret seeding,
    /// static read-only overlays, and `brain/` binding plan when
    /// `SPAWN_AGY_DATA_DIR` is enabled for `agy`.
    fn prepare_agy_data(
        &self,
        root: &Path,
        launch: &ViewedLaunch<'_>,
        host_engine_home: &Path,
        ctx: &BindContext<'_>,
        home: &Path,
    ) -> Result<AgyDataPlan, String> {
        let (canonical_root, _worker_base, worker_home, worker_tmp) =
            prepare_worker_dirs(root, "SPAWN_AGY_DATA_DIR", launch, ctx, home)?;
        ensure_dir_0700(&worker_home.join("brain"), "worker brain")?;
        let mut overlays = Vec::new();
        if self.config.isolate_workers {
            for rel in [
                "installation_id",
                "jetski_state.pbtxt",
                "settings.json",
                "cache/onboarding.json",
            ] {
                let src = host_engine_home.join(rel);
                let dst = worker_home.join(rel);
                if std::fs::metadata(&src).is_ok_and(|meta| meta.is_file()) {
                    if let Some(parent) = dst.parent()
                        && parent != worker_home
                    {
                        ensure_dir_0700(parent, "worker cache dir")?;
                    }
                    copy_file_atomic_noclobber(&src, &dst, 0o600)?;
                }
            }
            let worker_bin = ensure_dir_0700(&worker_home.join("bin"), "worker bin")?;
            let host_bin = host_engine_home.join("bin");
            if let Ok(entries) = std::fs::read_dir(&host_bin) {
                for entry in entries.flatten() {
                    let src = entry.path();
                    let Ok(meta) = std::fs::symlink_metadata(&src) else {
                        continue;
                    };
                    if !meta.file_type().is_file() {
                        continue;
                    }
                    let dst = worker_bin.join(entry.file_name());
                    let mode = (meta.permissions().mode() & 0o700) | 0o600;
                    copy_file_atomic_noclobber(&src, &dst, mode)?;
                }
            }
            ensure_dir_0700(&worker_home.join("xdg-cache"), "worker xdg-cache")?;
            let builtin = host_engine_home.join("builtin");
            if std::fs::metadata(&builtin).is_ok_and(|meta| meta.is_dir()) {
                let canonical = std::fs::canonicalize(&builtin)
                    .map_err(|error| format!("{}: {error}", builtin.display()))?;
                overlays.push((canonical, "builtin"));
            }
            let oauth = host_engine_home.join("antigravity-oauth-token");
            if std::fs::metadata(&oauth).is_ok_and(|meta| meta.is_file()) {
                let canonical = std::fs::canonicalize(&oauth)
                    .map_err(|error| format!("{}: {error}", oauth.display()))?;
                ensure_empty_mount_file(&worker_home.join("antigravity-oauth-token"))?;
                overlays.push((canonical, "antigravity-oauth-token"));
            }
        }
        let shared_brain = if self.config.isolate_brain {
            None
        } else {
            let shared = ensure_dir_0700(&canonical_root.join("brain"), "shared brain")?;
            Self::refuse_extra_target(&shared, "shared brain", ctx, home)?;
            Some(shared)
        };
        Ok(AgyDataPlan {
            worker_home,
            worker_tmp,
            overlays,
            shared_brain,
        })
    }

    /// Prepare off-home per-worker directories for Claude Code (`SPAWN_CLAUDE_DATA_DIR`),
    /// sharing only `<host_engine_home>/projects` writable for transcript ingestion
    /// and RO-binding non-mutable settings/plugins/credentials from `host_engine_home`.
    fn prepare_claude_data(
        root: &Path,
        launch: &ViewedLaunch<'_>,
        host_engine_home: &Path,
        ctx: &BindContext<'_>,
        home: &Path,
    ) -> Result<ClaudeDataPlan, String> {
        let (_canonical_root, worker_base, worker_home, worker_tmp) =
            prepare_worker_dirs(root, "SPAWN_CLAUDE_DATA_DIR", launch, ctx, home)?;
        for subdir in [
            "todos",
            "tasks",
            "plans",
            "session-env",
            "debug",
            "telemetry",
            "shell-snapshots",
            "file-history",
            "paste-cache",
            "statsig",
            "cache",
            "backups",
            "sessions",
            "projects",
        ] {
            ensure_dir_0700(&worker_home.join(subdir), "claude worker subdir")?;
        }
        let host_projects_raw = host_engine_home.join("projects");
        let host_projects = ensure_dir_0700(&host_projects_raw, "claude host projects")?;
        let claude_json = worker_base.join("claude.json");
        write_file_atomic_noclobber(&claude_json, b"{}\n", 0o600)?;

        let mut ro_overlays = Vec::new();
        for rel in ["settings.json", "settings.local.json", ".credentials.json"] {
            let src = host_engine_home.join(rel);
            if std::fs::metadata(&src).is_ok_and(|meta| meta.is_file()) {
                let canonical = std::fs::canonicalize(&src)
                    .map_err(|error| format!("{}: {error}", src.display()))?;
                ensure_empty_mount_file(&worker_home.join(rel))?;
                ro_overlays.push((canonical, rel, false));
            }
        }
        let plugins = host_engine_home.join("plugins");
        if std::fs::metadata(&plugins).is_ok_and(|meta| meta.is_dir()) {
            let canonical = std::fs::canonicalize(&plugins)
                .map_err(|error| format!("{}: {error}", plugins.display()))?;
            ensure_dir_0700(&worker_home.join("plugins"), "claude worker plugins")?;
            ro_overlays.push((canonical, "plugins", true));
        }

        Ok(ClaudeDataPlan {
            worker_home,
            worker_tmp,
            host_projects,
            claude_json,
            ro_overlays,
        })
    }

    fn resolve_engine_ro_binaries(
        &self,
        launch: &ViewedLaunch<'_>,
        home: &Path,
        engine_home: &Path,
        ctx: &BindContext<'_>,
    ) -> Vec<(PathBuf, PathBuf)> {
        if self.config.home_isolation != HomeIsolation::Whitelist {
            return Vec::new();
        }
        let prog = match launch.engine {
            "agy" => self.config.agy_program.as_deref(),
            "claude" => self.config.claude_program.as_deref(),
            _ => None,
        };
        let Some(prog) = prog else {
            return Vec::new();
        };
        let Ok(canon) = std::fs::canonicalize(prog) else {
            return Vec::new();
        };
        if !std::fs::symlink_metadata(&canon).is_ok_and(|meta| meta.is_file()) {
            return Vec::new();
        }
        let forbidden = self.forbidden_home_targets_for(launch.engine);
        let safe = |dest: &Path| {
            dest.starts_with(home)
                && dest != home
                && !paths_overlap(dest, ctx.private)
                && !paths_overlap(dest, engine_home)
                && !forbidden.iter().any(|bad| paths_overlap(dest, bad))
        };
        collect_program_ro_binds(prog, &canon, &safe)
    }

    fn check_host_inherit_target(
        resolved: &Path,
        label: &str,
        ctx: &InheritCheckCtx<'_>,
    ) -> Result<(), String> {
        if paths_overlap(resolved, ctx.bind.private) {
            return Err(format!(
                "{label} {} overlaps the view's private directory {}",
                resolved.display(),
                ctx.bind.private.display()
            ));
        }
        if resolved == ctx.home || ctx.home.starts_with(resolved) {
            return Err(format!(
                "{label} {} holds or equals $HOME {}",
                resolved.display(),
                ctx.home.display()
            ));
        }
        if paths_overlap(resolved, ctx.engine_home)
            || ctx
                .engine_homes
                .iter()
                .any(|dir| paths_overlap(resolved, dir))
        {
            return Err(format!(
                "{label} {} overlaps an engine home",
                resolved.display()
            ));
        }
        if ctx.forbidden.iter().any(|bad| paths_overlap(resolved, bad))
            || ctx
                .bind
                .masked
                .iter()
                .any(|mask| **mask != ctx.home && paths_overlap(resolved, mask))
        {
            return Err(format!(
                "{label} {} overlaps a masked path",
                resolved.display()
            ));
        }
        if paths_overlap(resolved, ctx.bind.workspace_src)
            || paths_overlap(resolved, ctx.bind.workspace_dest)
        {
            return Err(format!(
                "{label} {} overlaps the workspace",
                resolved.display()
            ));
        }
        if is_secret_candidate(resolved) {
            return Err(format!(
                "{label} {} looks like a secret file or directory",
                resolved.display()
            ));
        }
        if resolved.is_dir() && dir_has_direct_secret_entry(resolved) {
            return Err(format!(
                "{label} {} directly contains a secret file; specify safe subpaths instead",
                resolved.display()
            ));
        }
        if resolved.file_name().and_then(|s| s.to_str()) == Some(".gitconfig")
            && let Ok(contents) = std::fs::read_to_string(resolved)
            && !gitconfig_has_no_inline_secrets(&contents)
        {
            return Err(format!(
                "{label} {} contains inline credentials or auth headers",
                resolved.display()
            ));
        }
        Ok(())
    }

    fn resolve_auto_inherit_entries<'a>(
        &self,
        rels: impl IntoIterator<Item = &'a str>,
        expect_dir: bool,
        ctx: &InheritCheckCtx<'_>,
        binds: &mut Vec<(PathBuf, PathBuf)>,
        skipped: &mut Vec<(PathBuf, String)>,
    ) {
        let label = if expect_dir {
            "auto-inherit path"
        } else {
            "auto-inherit file"
        };
        for rel in rels {
            let candidate = self.config.home.join(rel);
            if std::fs::symlink_metadata(&candidate).is_err() {
                continue;
            }
            let Ok(resolved) = std::fs::canonicalize(&candidate) else {
                continue;
            };
            if (expect_dir && !resolved.is_dir()) || (!expect_dir && !resolved.is_file()) {
                continue;
            }
            if let Err(why) = Self::check_host_inherit_target(&resolved, label, ctx) {
                skipped.push((candidate, why));
                continue;
            }
            let dest = prospective_canonical(&candidate)
                .ok()
                .filter(|p| p.starts_with(ctx.home) && p != ctx.home)
                .unwrap_or_else(|| resolved.clone());
            if dest != resolved
                && let Err(why) = Self::check_host_inherit_target(&dest, label, ctx)
            {
                skipped.push((candidate, why));
                continue;
            }
            if !binds.iter().any(|(_, existing)| existing == &dest) {
                binds.push((resolved, dest));
            }
        }
    }

    fn resolve_host_inherit_ro_binds(
        &self,
        engine: &str,
        bind: &BindContext<'_>,
        home: &Path,
        engine_home: &Path,
        skipped: &mut Vec<(PathBuf, String)>,
    ) -> Result<Vec<(PathBuf, PathBuf)>, String> {
        let mut binds: Vec<(PathBuf, PathBuf)> = Vec::new();
        let forbidden = self.forbidden_home_targets_for(engine);
        let engine_homes = self.canonical_engine_homes_for(engine);
        let check_ctx = InheritCheckCtx {
            bind,
            home,
            engine_home,
            forbidden: &forbidden,
            engine_homes: &engine_homes,
        };

        if self.config.inherit_host == HostInheritMode::Auto
            && self.config.home_isolation == HomeIsolation::Whitelist
        {
            self.resolve_auto_inherit_entries(
                AUTO_INHERIT_HOME_DIRS.iter().copied(),
                true,
                &check_ctx,
                &mut binds,
                skipped,
            );
            self.resolve_auto_inherit_entries(
                AUTO_INHERIT_HOME_FILES
                    .iter()
                    .copied()
                    .chain([".gitconfig"]),
                false,
                &check_ctx,
                &mut binds,
                skipped,
            );
        }

        for raw in &self.config.inherit_host_paths {
            let resolved = std::fs::canonicalize(raw)
                .map_err(|error| format!("inherit host path {}: {error}", raw.display()))?;
            let meta = std::fs::symlink_metadata(&resolved)
                .map_err(|error| format!("inherit host path {}: {error}", resolved.display()))?;
            if !meta.is_dir() && !meta.is_file() {
                return Err(format!(
                    "inherit host path {} is neither a directory nor a regular file",
                    resolved.display()
                ));
            }
            Self::check_host_inherit_target(&resolved, "inherit host path", &check_ctx)?;
            if let Ok(prosp) = prospective_canonical(raw)
                && prosp != resolved
                && prosp.starts_with(home)
                && prosp != home
            {
                Self::check_host_inherit_target(&prosp, "inherit host path", &check_ctx)?;
                if !binds.iter().any(|(_, existing)| existing == &prosp) {
                    binds.push((resolved.clone(), prosp));
                }
            }
            if !binds.iter().any(|(_, existing)| existing == &resolved) {
                binds.push((resolved.clone(), resolved));
            }
        }

        Ok(binds)
    }

    fn resolve_extra_mounts(
        &self,
        ctx: &BindContext<'_>,
        home: &Path,
        engine_home: &Path,
    ) -> Result<Vec<(PathBuf, bool)>, String> {
        let mut extra_binds = Vec::new();
        for mount in &self.config.extra_mounts {
            let resolved = std::fs::canonicalize(&mount.path)
                .map_err(|error| format!("extra mount {}: {error}", mount.path.display()))?;
            let meta = std::fs::symlink_metadata(&resolved)
                .map_err(|error| format!("extra mount {}: {error}", resolved.display()))?;
            if !meta.is_dir() && !meta.is_file() {
                return Err(format!(
                    "extra mount {} is neither a directory nor a regular file",
                    resolved.display()
                ));
            }
            Self::refuse_extra_target(&resolved, "extra mount", ctx, home)?;
            if paths_overlap(&resolved, engine_home) {
                return Err(format!(
                    "extra mount {} overlaps the engine's home {}",
                    resolved.display(),
                    engine_home.display()
                ));
            }
            extra_binds.push((resolved, mount.writable));
        }
        Ok(extra_binds)
    }

    /// Build the root filesystem prefix and namespace flags.
    ///
    /// Host network namespace MUST remain shared (never pass `--unshare-net`)
    /// so sandboxed agents retain loopback access to `127.0.0.1:8791`
    /// (`model-relay` / local control plane per `#113`).
    fn append_root_prefix(&self, words: &mut Vec<String>) -> Result<(), String> {
        if let Some(target) = &self.config.canonical_workspace {
            match std::fs::symlink_metadata(target) {
                Ok(meta) if meta.is_dir() => {
                    words.extend(["--ro-bind".to_string(), "/".to_string(), "/".to_string()]);
                }
                Ok(_) => {
                    return Err(format!(
                        "canonical workspace {} exists on host and is not a directory",
                        target.display()
                    ));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    emit_root_mirror_for_target(words, target)?;
                }
                Err(error) => {
                    return Err(format!("canonical workspace {}: {error}", target.display()));
                }
            }
        } else {
            words.extend(["--ro-bind".to_string(), "/".to_string(), "/".to_string()]);
        }
        words.extend([
            "--dev".to_string(),
            "/dev".to_string(),
            "--proc".to_string(),
            "/proc".to_string(),
            "--die-with-parent".to_string(),
            "--unshare-pid".to_string(),
        ]);
        Ok(())
    }

    fn append_default_engine_binds<'a>(
        words: &mut Vec<String>,
        bound_dests: &mut Vec<&'a Path>,
        engine_home: &'a Path,
        tmpdir: &'a Path,
    ) {
        // The TMPDIR below the engine's home is already writable through
        // that bind: bound again, its path would be resolved anew when
        // `bwrap` runs, after a writer outside the view could swap it.
        for path in [engine_home, tmpdir] {
            if bound_dests.iter().any(|done| path.starts_with(done)) {
                continue;
            }
            bound_dests.push(path);
            words.extend(["--bind".to_string(), path_word(path), path_word(path)]);
        }
    }

    fn append_canonical_env(
        &self,
        words: &mut Vec<String>,
        launch: &ViewedLaunch<'_>,
        binds: &PlannedBinds<'_>,
    ) {
        let Some(target) = &self.config.canonical_workspace else {
            return;
        };
        words.extend([
            "--setenv".to_string(),
            "SPAWN_WORKSPACE".to_string(),
            path_word(target),
            "--setenv".to_string(),
            "SPAWN_SANDBOX_CANONICAL_WORKSPACE".to_string(),
            path_word(target),
            "--setenv".to_string(),
            "PWD".to_string(),
            path_word(target),
        ]);
        if let Some(ef) = launch.exit_file {
            let ef_path = Path::new(ef);
            if let Some(rel) = ef_path
                .strip_prefix(Path::new(launch.workspace))
                .ok()
                .or_else(|| ef_path.strip_prefix(binds.workspace_src).ok())
                .filter(|rel| !rel.as_os_str().is_empty())
            {
                words.extend([
                    "--setenv".to_string(),
                    "SPAWN_EXIT_FILE".to_string(),
                    path_word(&target.join(rel)),
                ]);
            }
        }
    }

    fn append_agy_binds<'a>(
        &self,
        words: &mut Vec<String>,
        bound_dests: &mut Vec<&'a Path>,
        engine_home: &'a Path,
        tmpdir: &'a Path,
        plan: &AgyDataPlan,
    ) {
        if self.config.isolate_workers {
            bound_dests.push(engine_home);
            words.extend([
                "--bind".to_string(),
                path_word(&plan.worker_home),
                path_word(engine_home),
            ]);
            for (src, rel) in &plan.overlays {
                words.extend([
                    "--ro-bind".to_string(),
                    path_word(src),
                    path_word(&engine_home.join(rel)),
                ]);
            }
            if let Some(shared_brain) = &plan.shared_brain {
                words.extend([
                    "--bind".to_string(),
                    path_word(shared_brain),
                    path_word(&engine_home.join("brain")),
                ]);
            }
            words.extend([
                "--bind".to_string(),
                path_word(tmpdir),
                path_word(tmpdir),
                "--setenv".to_string(),
                "XDG_CACHE_HOME".to_string(),
                path_word(&engine_home.join("xdg-cache")),
            ]);
        } else {
            Self::append_default_engine_binds(words, bound_dests, engine_home, tmpdir);
            let brain_src = plan
                .shared_brain
                .clone()
                .unwrap_or_else(|| plan.worker_home.join("brain"));
            words.extend([
                "--bind".to_string(),
                path_word(&brain_src),
                path_word(&engine_home.join("brain")),
            ]);
        }
    }

    fn append_claude_binds<'a>(
        words: &mut Vec<String>,
        bound_dests: &mut Vec<&'a Path>,
        home: &Path,
        engine_home: &'a Path,
        tmpdir: &Path,
        plan: &ClaudeDataPlan,
    ) {
        bound_dests.push(engine_home);
        words.extend([
            "--bind".to_string(),
            path_word(&plan.worker_home),
            path_word(engine_home),
        ]);
        for (src, rel, _is_dir) in &plan.ro_overlays {
            words.extend([
                "--ro-bind".to_string(),
                path_word(src),
                path_word(&engine_home.join(rel)),
            ]);
        }
        words.extend([
            "--bind".to_string(),
            path_word(&plan.host_projects),
            path_word(&engine_home.join("projects")),
            "--bind".to_string(),
            path_word(&plan.claude_json),
            path_word(&home.join(".claude.json")),
            "--bind".to_string(),
            path_word(tmpdir),
            path_word(tmpdir),
            "--setenv".to_string(),
            "CLAUDE_CONFIG_DIR".to_string(),
            path_word(engine_home),
        ]);
    }

    fn append_binds_and_env(
        &self,
        words: &mut Vec<String>,
        launch: &ViewedLaunch<'_>,
        binds: &PlannedBinds<'_>,
    ) {
        for (src, dest) in binds.host_inherit_binds {
            words.extend(["--ro-bind".to_string(), path_word(src), path_word(dest)]);
        }

        let mut bound_dests: Vec<&Path> = vec![binds.workspace_dest];
        words.extend([
            "--bind".to_string(),
            path_word(binds.workspace_src),
            path_word(binds.workspace_dest),
        ]);

        if let Some(plan) = binds.agy_plan {
            self.append_agy_binds(
                words,
                &mut bound_dests,
                binds.engine_home,
                binds.tmpdir,
                plan,
            );
        } else if let Some(plan) = binds.claude_plan {
            Self::append_claude_binds(
                words,
                &mut bound_dests,
                binds.home,
                binds.engine_home,
                binds.tmpdir,
                plan,
            );
        } else {
            Self::append_default_engine_binds(
                words,
                &mut bound_dests,
                binds.engine_home,
                binds.tmpdir,
            );
            if launch.engine == "claude" {
                words.extend([
                    "--setenv".to_string(),
                    "CLAUDE_CONFIG_DIR".to_string(),
                    path_word(binds.engine_home),
                ]);
            }
        }

        for (resolved, writable) in binds.extra_binds {
            let flag = if *writable { "--bind" } else { "--ro-bind" };
            words.extend([flag.to_string(), path_word(resolved), path_word(resolved)]);
        }
        for (src, dest) in binds.engine_ro_binaries {
            words.extend(["--ro-bind".to_string(), path_word(src), path_word(dest)]);
        }

        words.extend([
            "--setenv".to_string(),
            "TMPDIR".to_string(),
            path_word(binds.tmpdir),
        ]);
        self.append_canonical_env(words, launch, binds);
        words.extend([
            "--chdir".to_string(),
            path_word(binds.workspace_dest),
            "--".to_string(),
        ]);
    }

    fn resolve_engine_home_for_plan(
        &self,
        engine: &str,
        wanted: &Path,
        ctx: &BindContext<'_>,
        directories: &[PathBuf],
        isolate_agy_workers: bool,
    ) -> Result<PathBuf, String> {
        let prospective = prospective_canonical(wanted)
            .map_err(|why| format!("the engine's home {}: {why}", wanted.display()))?;
        self.refuse_binds_for(engine, ctx, &prospective)?;
        let covered_by_tmpfs = directories
            .iter()
            .any(|mask| prospective.starts_with(mask) && prospective != *mask);

        if isolate_agy_workers && std::fs::symlink_metadata(wanted).is_ok() {
            let made = canonical_directory(wanted, "the engine's home")?;
            self.refuse_binds_for(engine, ctx, &made)?;
            Ok(made)
        } else if isolate_agy_workers && covered_by_tmpfs {
            Ok(prospective)
        } else {
            let made = make_engine_home(wanted, &prospective)?;
            self.refuse_binds_for(engine, ctx, &made)?;
            Ok(made)
        }
    }

    /// The view of one launch: every target resolved now, the masks first
    /// and the writable binds last. Refused (an error) when the workspace
    /// or the engine's home cannot be bound without uncovering a mask, or
    /// cannot be made.
    pub fn plan(&self, launch: &ViewedLaunch<'_>) -> Result<Plan, String> {
        self.validate_config()?;
        let workspace = canonical_directory(Path::new(launch.workspace), "the workspace")?;
        let workspace_dest = self
            .config
            .canonical_workspace
            .as_deref()
            .unwrap_or(&workspace);
        let home =
            std::fs::canonicalize(&self.config.home).unwrap_or_else(|_| self.config.home.clone());
        let private = self.private_dir()?;
        let (all_directories, all_files, mut skipped) = self.targets_for(launch.engine);
        let masked: Vec<&PathBuf> = all_directories.iter().chain(all_files.iter()).collect();
        let ctx = BindContext {
            workspace_src: &workspace,
            workspace_dest,
            private: &private,
            masked: &masked,
        };
        let (directories, files) = prune_nested(&all_directories, &all_files);

        let agy_data_root = (launch.engine == "agy")
            .then_some(self.config.agy_data_dir.as_deref())
            .flatten();
        let claude_data_root = (launch.engine == "claude")
            .then_some(self.config.claude_data_dir.as_deref())
            .flatten();
        let isolate_agy_workers = agy_data_root.is_some() && self.config.isolate_workers;
        let engine_home = self.resolve_engine_home_for_plan(
            launch.engine,
            Path::new(launch.engine_home),
            &ctx,
            &directories,
            isolate_agy_workers,
        )?;

        let agy_plan = match agy_data_root {
            Some(root) => Some(self.prepare_agy_data(root, launch, &engine_home, &ctx, &home)?),
            None => None,
        };
        let claude_plan = match claude_data_root {
            Some(root) => Some(Self::prepare_claude_data(
                root,
                launch,
                &engine_home,
                &ctx,
                &home,
            )?),
            None => None,
        };
        let tmpdir = match (&agy_plan, &claude_plan) {
            (Some(plan), _) if self.config.isolate_workers => plan.worker_tmp.clone(),
            (_, Some(plan)) => plan.worker_tmp.clone(),
            _ => engine_tmpdir(&engine_home)?,
        };
        let extra_binds = self.resolve_extra_mounts(&ctx, &home, &engine_home)?;
        let raw_engine_ro_binaries =
            self.resolve_engine_ro_binaries(launch, &home, &engine_home, &ctx);
        let host_inherit_binds = self.resolve_host_inherit_ro_binds(
            launch.engine,
            &ctx,
            &home,
            &engine_home,
            &mut skipped,
        )?;
        let engine_ro_binaries: Vec<(PathBuf, PathBuf)> = raw_engine_ro_binaries
            .into_iter()
            .filter(|(_, dest)| {
                !host_inherit_binds
                    .iter()
                    .any(|(_, d)| d.is_dir() && dest.starts_with(d))
            })
            .collect();

        let mut words = vec![self.program.display().to_string()];
        self.append_root_prefix(&mut words)?;
        let masks = self.emit_mask_words(&directories, &files, &mut words)?;

        self.append_binds_and_env(
            &mut words,
            launch,
            &PlannedBinds {
                home: &home,
                workspace_src: &workspace,
                workspace_dest,
                engine_home: &engine_home,
                tmpdir: &tmpdir,
                agy_plan: agy_plan.as_ref(),
                claude_plan: claude_plan.as_ref(),
                extra_binds: &extra_binds,
                engine_ro_binaries: &engine_ro_binaries,
                host_inherit_binds: &host_inherit_binds,
            },
        );
        Ok(Plan {
            words,
            masks,
            tmpdir,
            skipped,
        })
    }

    fn emit_mask_words(
        &self,
        directories: &[PathBuf],
        files: &[PathBuf],
        words: &mut Vec<String>,
    ) -> Result<Vec<Mask>, String> {
        let mut masks = Vec::new();
        for dir in directories {
            words.extend(["--tmpfs".to_string(), path_word(dir)]);
            masks.push(Mask::Directory(dir.clone()));
        }
        if !files.is_empty() {
            let empty = self.empty_file()?;
            for file in files {
                words.extend(["--ro-bind".to_string(), path_word(&empty), path_word(file)]);
                masks.push(Mask::File(file.clone()));
            }
        }
        Ok(masks)
    }
}

struct PlannedBinds<'a> {
    home: &'a Path,
    workspace_src: &'a Path,
    workspace_dest: &'a Path,
    engine_home: &'a Path,
    tmpdir: &'a Path,
    agy_plan: Option<&'a AgyDataPlan>,
    claude_plan: Option<&'a ClaudeDataPlan>,
    extra_binds: &'a [(PathBuf, bool)],
    engine_ro_binaries: &'a [(PathBuf, PathBuf)],
    host_inherit_binds: &'a [(PathBuf, PathBuf)],
}

struct BindContext<'a> {
    workspace_src: &'a Path,
    workspace_dest: &'a Path,
    private: &'a Path,
    masked: &'a [&'a PathBuf],
}

struct InheritCheckCtx<'a> {
    bind: &'a BindContext<'a>,
    home: &'a Path,
    engine_home: &'a Path,
    forbidden: &'a [PathBuf],
    engine_homes: &'a [PathBuf],
}

struct AgyDataPlan {
    worker_home: PathBuf,
    worker_tmp: PathBuf,
    overlays: Vec<(PathBuf, &'static str)>,
    shared_brain: Option<PathBuf>,
}

struct ClaudeDataPlan {
    worker_home: PathBuf,
    worker_tmp: PathBuf,
    host_projects: PathBuf,
    claude_json: PathBuf,
    ro_overlays: Vec<(PathBuf, &'static str, bool)>,
}

fn paths_overlap(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

fn validate_abs_clean(path: &Path, what: &str) -> Result<(), String> {
    use std::path::Component;
    if !path.is_absolute() {
        return Err(format!("{what} {} is not absolute", path.display()));
    }
    if path == Path::new("/") {
        return Err(format!("{what} cannot be /"));
    }
    if path
        .components()
        .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
    {
        return Err(format!(
            "{what} {} cannot contain `.` or `..`",
            path.display()
        ));
    }
    Ok(())
}

fn ensure_dir_0700(path: &Path, what: &str) -> Result<PathBuf, String> {
    if std::fs::symlink_metadata(path).is_err() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(|error| format!("create {what} {}: {error}", path.display()))?;
    }
    canonical_directory(path, what)
}

static ATOMIC_SEED_SEQ: AtomicU64 = AtomicU64::new(0);

fn copy_file_atomic_noclobber(src: &Path, dst: &Path, mode: u32) -> Result<(), String> {
    if std::fs::symlink_metadata(dst).is_ok() {
        return Ok(());
    }
    let bytes = std::fs::read(src).map_err(|error| format!("read {}: {error}", src.display()))?;
    let parent = dst
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", dst.display()))?;
    let (tmp_path, mut tmp_file) = loop {
        let seq = ATOMIC_SEED_SEQ.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".seed-{}-{seq}.tmp", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&candidate)
        {
            Ok(file) => break (candidate, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(format!("create temp seed {}: {error}", candidate.display()));
            }
        }
    };
    let write_res = tmp_file.write_all(&bytes);
    drop(tmp_file);
    if let Err(error) = write_res {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!("write {}: {error}", dst.display()));
    }
    if let Err(error) = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(mode)) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!("chmod {}: {error}", dst.display()));
    }
    let link_res = std::fs::hard_link(&tmp_path, dst);
    let _ = std::fs::remove_file(&tmp_path);
    match link_res {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(format!("publish {}: {error}", dst.display())),
    }
}

fn ensure_empty_mount_file(path: &Path) -> Result<(), String> {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(file) => {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|error| format!("chmod {}: {error}", path.display()))?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(format!("create {}: {error}", path.display())),
    }
}

fn emit_root_mirror_for_target(words: &mut Vec<String>, target: &Path) -> Result<(), String> {
    use std::path::Component;
    let parts: Vec<std::ffi::OsString> = target
        .components()
        .filter_map(|part| match part {
            Component::Normal(name) => Some(name.to_os_string()),
            _ => None,
        })
        .collect();
    let refs: Vec<&std::ffi::OsStr> = parts.iter().map(std::ffi::OsString::as_os_str).collect();
    mirror_dir_for_target(words, Path::new("/"), &refs)
}

fn mirror_dir_for_target(
    words: &mut Vec<String>,
    dir: &Path,
    remaining: &[&std::ffi::OsStr],
) -> Result<(), String> {
    if remaining.is_empty() {
        words.extend(["--dir".to_string(), path_word(dir)]);
        return Ok(());
    }
    if dir != Path::new("/") {
        words.extend(["--dir".to_string(), path_word(dir)]);
    }
    words.extend(["--tmpfs".to_string(), path_word(dir)]);
    let is_root = dir == Path::new("/");
    let mut entries: Vec<(std::ffi::OsString, PathBuf)> = std::fs::read_dir(dir)
        .map_err(|error| format!("read {}: {error}", dir.display()))?
        .filter_map(Result::ok)
        .map(|entry| (entry.file_name(), entry.path()))
        .collect();
    entries.sort_by(|(a, _), (b, _)| a.cmp(b));
    let head = remaining[0];
    for (name, path) in entries {
        if name.as_os_str() == head || (is_root && (name == "dev" || name == "proc")) {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            if let Ok(link_target) = std::fs::read_link(&path) {
                words.extend([
                    "--symlink".to_string(),
                    path_word(&link_target),
                    path_word(&path),
                ]);
            }
        } else {
            words.extend([
                "--ro-bind-try".to_string(),
                path_word(&path),
                path_word(&path),
            ]);
        }
    }
    let next = dir.join(head);
    if is_root {
        words.extend([
            "--dir".to_string(),
            "/dev".to_string(),
            "--dir".to_string(),
            "/proc".to_string(),
        ]);
    }
    if remaining.len() == 1 {
        words.extend(["--dir".to_string(), path_word(&next)]);
    } else if std::fs::symlink_metadata(&next).is_ok_and(|meta| meta.is_dir()) {
        mirror_dir_for_target(words, &next, &remaining[1..])?;
    } else {
        let mut cur = dir.to_path_buf();
        for comp in remaining {
            cur.push(comp);
            words.extend(["--dir".to_string(), path_word(&cur)]);
        }
    }
    words.extend(["--remount-ro".to_string(), path_word(dir)]);
    Ok(())
}

impl EngineView for BwrapView {
    fn name(&self) -> &'static str {
        "bwrap"
    }

    fn canonical_workspace(&self) -> Option<&Path> {
        self.config.canonical_workspace.as_deref()
    }

    /// [`BwrapView::plan`]'s words; each skipped target said on the
    /// loop's standard error, one line for the launch.
    fn wrap(&self, launch: &ViewedLaunch<'_>) -> Result<Vec<String>, String> {
        if !is_executable(&self.program) {
            return Err(format!("{} is gone", self.program.display()));
        }
        let plan = self.plan(launch)?;
        if !plan.skipped.is_empty() {
            let said: Vec<String> = plan
                .skipped
                .iter()
                .map(|(path, why)| format!("{} ({why})", path.display()))
                .collect();
            eprintln!(
                "engine view: {} in {}: not masked: {}",
                launch.engine,
                launch.workspace,
                said.join(", ")
            );
        }
        Ok(plan.words)
    }
}

/// Host filesystem fallback view used inside unprivileged containers where
/// nested `bwrap` namespaces are blocked by the container runtime.
///
/// Fails closed if constructed on a bare host without container runtime signals
/// ([`crate::container::detect_container`]). Gives each engine its own isolated
/// `TMPDIR` without remapping the workspace or creating nested namespaces.
#[derive(Debug, Clone)]
pub struct ContainerView {
    config: ViewConfig,
}

impl ContainerView {
    /// Construct a [`ContainerView`], failing closed unless container runtime
    /// signals are detected on the host.
    pub fn new(config: ViewConfig) -> Result<Self, String> {
        Self::new_with_detector(config, &crate::container::detect_container)
    }

    /// Testable constructor taking a custom container detector.
    pub fn new_with_detector(
        config: ViewConfig,
        detect: &dyn Fn() -> crate::container::ContainerEnv,
    ) -> Result<Self, String> {
        validate_config_for(&config)?;
        let env = detect();
        if !env.in_container {
            return Err(
                "refusing ContainerView on a bare host: no container runtime detected; \
                 install or fix bwrap instead of disabling namespace isolation"
                    .to_string(),
            );
        }
        let sh = Path::new("/bin/sh");
        if !is_executable(sh) {
            return Err("/bin/sh is not executable".to_string());
        }
        Ok(Self { config })
    }

    /// What the view is configured with.
    #[must_use]
    pub fn config(&self) -> &ViewConfig {
        &self.config
    }
}

impl EngineView for ContainerView {
    fn name(&self) -> &'static str {
        "container"
    }

    fn canonical_workspace(&self) -> Option<&Path> {
        None
    }

    fn wrap(&self, launch: &ViewedLaunch<'_>) -> Result<Vec<String>, String> {
        let workspace = canonical_directory(Path::new(launch.workspace), "the workspace")?;
        let wanted = Path::new(launch.engine_home);
        let prospective = prospective_canonical(wanted)
            .map_err(|why| format!("the engine's home {}: {why}", wanted.display()))?;
        let engine_home = make_engine_home(wanted, &prospective)?;
        let data_root = match launch.engine {
            "agy" => self
                .config
                .isolate_workers
                .then_some(self.config.agy_data_dir.as_deref())
                .flatten(),
            "claude" => self.config.claude_data_dir.as_deref(),
            _ => None,
        };
        let tmpdir = match data_root {
            Some(root) => {
                let canonical_root = ensure_dir_0700(root, "engine data dir")?;
                let worker_key = worker_key_for(launch, &workspace);
                let workers_dir = ensure_dir_0700(&canonical_root.join("workers"), "workers dir")?;
                let worker_base = ensure_dir_0700(&workers_dir.join(&worker_key), "worker dir")?;
                ensure_dir_0700(&worker_base.join("tmp"), "worker tmp")?
            }
            None => engine_tmpdir(&engine_home)?,
        };
        Ok(vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            "TMPDIR=\"$1\"; export TMPDIR; shift 2; exec \"$@\"".to_string(),
            "spawn-container-view".to_string(),
            path_word(&tmpdir),
            "--".to_string(),
        ])
    }
}

const AUTO_INHERIT_HOME_DIRS: &[&str] = &[
    ".rustup",
    ".cargo/bin",
    ".nvm",
    ".pyenv",
    ".asdf",
    ".mise",
    ".local/opt",
    ".local/share/mise",
];

const AUTO_INHERIT_HOME_FILES: &[&str] = &[".cargo/config.toml", ".cargo/config"];

fn validate_managed_root(
    root: &Path,
    label: &str,
    config: &ViewConfig,
    private_norm: &Path,
    home_norm: &Path,
) -> Result<(), String> {
    validate_abs_clean(root, label)?;
    let root_norm = prospective_canonical(root).unwrap_or_else(|_| root.to_path_buf());
    if root_norm == Path::new("/") {
        return Err(format!("{label} cannot resolve to /"));
    }
    if paths_overlap(root, &config.private_dir) || paths_overlap(&root_norm, private_norm) {
        return Err(format!(
            "{label} {} overlaps the view's private directory {}",
            root.display(),
            config.private_dir.display()
        ));
    }
    if config.home_isolation == HomeIsolation::Whitelist
        && (paths_overlap(root, &config.home) || paths_overlap(&root_norm, home_norm))
    {
        return Err(format!(
            "{label} {} overlaps $HOME {} in whitelist mode",
            root.display(),
            config.home.display()
        ));
    }
    Ok(())
}

fn validate_config_for(config: &ViewConfig) -> Result<(), String> {
    let home_norm = prospective_canonical(&config.home).unwrap_or_else(|_| config.home.clone());
    let private_norm =
        prospective_canonical(&config.private_dir).unwrap_or_else(|_| config.private_dir.clone());
    if config.home_isolation == HomeIsolation::Whitelist
        && (config.home == Path::new("/") || home_norm == Path::new("/"))
    {
        return Err("HOME cannot be / in whitelist mode".to_string());
    }
    if let Some(target) = &config.canonical_workspace {
        validate_abs_clean(target, "canonical workspace")?;
        let target_norm = prospective_canonical(target).unwrap_or_else(|_| target.clone());
        if target_norm == Path::new("/") {
            return Err("canonical workspace cannot resolve to /".to_string());
        }
        for (other, label) in [
            (config.home.as_path(), "$HOME"),
            (home_norm.as_path(), "$HOME"),
            (Path::new("/run"), "/run"),
            (Path::new("/tmp"), "/tmp"),
            (config.private_dir.as_path(), "the view's private directory"),
            (private_norm.as_path(), "the view's private directory"),
            (config.runtime_dir.as_path(), "XDG_RUNTIME_DIR"),
        ] {
            if paths_overlap(target, other) || paths_overlap(&target_norm, other) {
                return Err(format!(
                    "canonical workspace {} overlaps {label} ({})",
                    target.display(),
                    other.display()
                ));
            }
        }
    }
    if let Some(root) = &config.agy_data_dir {
        validate_managed_root(
            root,
            "SPAWN_AGY_DATA_DIR",
            config,
            &private_norm,
            &home_norm,
        )?;
    }
    if let Some(root) = &config.claude_data_dir {
        validate_managed_root(
            root,
            "SPAWN_CLAUDE_DATA_DIR",
            config,
            &private_norm,
            &home_norm,
        )?;
    }
    for mount in &config.extra_mounts {
        validate_managed_root(
            &mount.path,
            "extra mount",
            config,
            &private_norm,
            &home_norm,
        )?;
    }
    for inherit_path in &config.inherit_host_paths {
        validate_abs_clean(inherit_path, "SPAWN_SANDBOX_INHERIT_HOST_PATHS entry")?;
        let norm = prospective_canonical(inherit_path).unwrap_or_else(|_| inherit_path.clone());
        if norm == Path::new("/") {
            return Err(format!(
                "SPAWN_SANDBOX_INHERIT_HOST_PATHS entry {} cannot resolve to /",
                inherit_path.display()
            ));
        }
        if paths_overlap(inherit_path, &config.private_dir) || paths_overlap(&norm, &private_norm) {
            return Err(format!(
                "SPAWN_SANDBOX_INHERIT_HOST_PATHS entry {} overlaps the view's private directory {}",
                inherit_path.display(),
                config.private_dir.display()
            ));
        }
        if inherit_path == &config.home
            || norm == home_norm
            || config.home.starts_with(inherit_path)
            || home_norm.starts_with(&norm)
        {
            return Err(format!(
                "SPAWN_SANDBOX_INHERIT_HOST_PATHS entry {} holds or equals $HOME {}",
                inherit_path.display(),
                config.home.display()
            ));
        }
    }
    Ok(())
}

fn worker_key_for(launch: &ViewedLaunch<'_>, workspace_src: &Path) -> String {
    let raw_key = launch
        .worker_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map_or_else(
            || {
                workspace_src
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("worker")
                    .to_string()
            },
            str::to_string,
        );
    spawn_core::driver::sanitize_worker_key(&raw_key)
}

fn prepare_worker_dirs(
    root: &Path,
    label: &str,
    launch: &ViewedLaunch<'_>,
    ctx: &BindContext<'_>,
    home: &Path,
) -> Result<(PathBuf, PathBuf, PathBuf, PathBuf), String> {
    let canonical_root = ensure_dir_0700(root, label)?;
    let worker_key = worker_key_for(launch, ctx.workspace_src);
    let workers_dir = ensure_dir_0700(&canonical_root.join("workers"), "workers dir")?;
    let worker_base = ensure_dir_0700(&workers_dir.join(&worker_key), "worker dir")?;
    let worker_home = ensure_dir_0700(&worker_base.join("home"), "worker home")?;
    let worker_tmp = ensure_dir_0700(&worker_base.join("tmp"), "worker tmp")?;
    for (path, what) in [(&worker_home, "worker home"), (&worker_tmp, "worker tmp")] {
        BwrapView::refuse_extra_target(path, what, ctx, home)?;
    }
    Ok((canonical_root, worker_base, worker_home, worker_tmp))
}

fn write_file_atomic_noclobber(dst: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    if std::fs::symlink_metadata(dst).is_ok() {
        return Ok(());
    }
    let parent = dst
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", dst.display()))?;
    let (tmp_path, mut tmp_file) = loop {
        let seq = ATOMIC_SEED_SEQ.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".seed-{}-{seq}.tmp", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&candidate)
        {
            Ok(file) => break (candidate, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(format!("create temp seed {}: {error}", candidate.display()));
            }
        }
    };
    let write_res = tmp_file.write_all(bytes);
    drop(tmp_file);
    if let Err(error) = write_res {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!("write {}: {error}", dst.display()));
    }
    if let Err(error) = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(mode)) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!("chmod {}: {error}", dst.display()));
    }
    let link_res = std::fs::hard_link(&tmp_path, dst);
    let _ = std::fs::remove_file(&tmp_path);
    match link_res {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(format!("publish {}: {error}", dst.display())),
    }
}

fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() && !path.is_absolute() {
                    out.push("..");
                }
            }
            Component::Normal(name) => out.push(name),
        }
    }
    out
}

fn collect_program_ro_binds(
    prog: &Path,
    canon: &Path,
    safe: &dyn Fn(&Path) -> bool,
) -> Vec<(PathBuf, PathBuf)> {
    let mut binds: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut push_dest = |dest: PathBuf| {
        if safe(&dest) && !binds.iter().any(|(_, existing)| existing == &dest) {
            binds.push((canon.to_path_buf(), dest));
        }
    };
    push_dest(canon.to_path_buf());
    let mut cur = prog.to_path_buf();
    for _ in 0..16 {
        if let Ok(prosp) = prospective_canonical(&cur) {
            push_dest(prosp);
        }
        push_dest(cur.clone());
        let Ok(target) = std::fs::read_link(&cur) else {
            break;
        };
        let next = if target.is_absolute() {
            target
        } else {
            cur.parent().unwrap_or_else(|| Path::new("/")).join(target)
        };
        cur = lexical_normalize(&next);
    }
    binds
}

fn is_secret_candidate(path: &Path) -> bool {
    for comp in path.components() {
        let std::path::Component::Normal(os) = comp else {
            continue;
        };
        let lower = os.to_string_lossy().to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            ".ssh"
                | ".gnupg"
                | ".aws"
                | ".kube"
                | ".docker"
                | ".git-credentials"
                | ".pgpass"
                | ".netrc"
                | ".claude.json"
                | "antigravity-oauth-token"
        ) {
            return true;
        }
    }
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    let has_secret_ext = Path::new(&lower)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| matches!(ext, "env" | "pem" | "key" | "p12" | "pfx" | "kdbx"));
    lower == ".env"
        || lower.starts_with(".env.")
        || has_secret_ext
        || matches!(
            lower.as_str(),
            "env.sh"
                | "credentials"
                | "credentials.toml"
                | "credentials.json"
                | ".credentials.json"
                | "id_rsa"
                | "id_ed25519"
                | "id_ecdsa"
                | "id_dsa"
                | ".npmrc"
                | ".pypirc"
                | "antigravity-oauth-token"
        )
}

fn dir_has_direct_secret_entry(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries
        .flatten()
        .any(|entry| is_secret_candidate(&entry.path()))
}

fn has_url_userinfo(line: &str) -> bool {
    let mut rest = line;
    while let Some(idx) = rest.find("://") {
        let after = &rest[idx + 3..];
        let authority = after
            .split(['/', '?', '#', '"', '\'', ' ', '\t', '\\'])
            .next()
            .unwrap_or("");
        if authority.contains('@') {
            return true;
        }
        rest = after;
    }
    false
}

/// Reject `.gitconfig` files that embed URL credentials, auth headers, or inline
/// passwords/tokens so safe `[user]` / `[alias]` / `[core]` config can be
/// inherited read-only without leaking secrets.
#[must_use]
pub fn gitconfig_has_no_inline_secrets(contents: &str) -> bool {
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if has_url_userinfo(&lower)
            || lower.contains("extraheader")
            || lower.contains("authorization:")
            || lower.contains("bearer ")
            || lower.contains("ghp_")
            || lower.contains("github_pat_")
            || lower.contains("sk-ant-")
            || lower.contains("sk-proj-")
        {
            return false;
        }
        if let Some((key, value)) = lower.split_once('=') {
            let key = key.trim();
            let value = value.trim();
            if ["password", "token", "secret", "apikey", "api_key"]
                .iter()
                .any(|kw| key.contains(kw))
            {
                return false;
            }
            if (key == "helper" || key.ends_with(".helper")) && value.starts_with('!') {
                return false;
            }
        }
    }
    true
}

/// The engine's own `TMPDIR`: [`TMP_FALLBACK`] below its home, made 0700
/// when missing, and still below that home once resolved (a link out of
/// it is refused). Never the loop's shared `TMPDIR`.
fn engine_tmpdir(engine_home: &Path) -> Result<PathBuf, String> {
    let dir = engine_home.join(TMP_FALLBACK);
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("create {}: {error}", dir.display())),
    }
    let canonical =
        std::fs::canonicalize(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    if canonical.is_dir() && canonical.starts_with(engine_home) && canonical != engine_home {
        Ok(canonical)
    } else {
        Err(format!(
            "the engine's TMPDIR {} leaves its home {}",
            dir.display(),
            engine_home.display()
        ))
    }
}

/// The masks the words carry: `directories` and `files` less every one
/// inside another masked directory, which goes with it.
fn prune_nested(directories: &[PathBuf], files: &[PathBuf]) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let inside_other = |path: &Path, all: &[PathBuf]| {
        all.iter()
            .any(|dir| dir.as_path() != path && path.starts_with(dir))
    };
    let kept: Vec<PathBuf> = directories
        .iter()
        .filter(|dir| !inside_other(dir, directories))
        .cloned()
        .collect();
    let files = files
        .iter()
        .filter(|file| !inside_other(file, &kept))
        .cloned()
        .collect();
    (kept, files)
}

/// Where `path` (absolute, no `..`) would resolve: its deepest existing
/// ancestor canonicalized, the rest joined as written. Nothing is made.
fn prospective_canonical(path: &Path) -> Result<PathBuf, String> {
    use std::path::Component;
    if !path.is_absolute() {
        return Err("not absolute".to_string());
    }
    if path.components().any(|part| part == Component::ParentDir) {
        return Err("holds `..`".to_string());
    }
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    while std::fs::symlink_metadata(&existing).is_err() {
        let Some(name) = existing.file_name().map(std::ffi::OsStr::to_os_string) else {
            return Err("no part of it exists".to_string());
        };
        rest.push(name);
        existing.pop();
    }
    let mut resolved = std::fs::canonicalize(&existing).map_err(|error| error.to_string())?;
    for name in rest.into_iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
}

/// The engine's home, made 0700 (its missing parents too) when missing,
/// and canonical; refused unless it resolves where it was checked
/// (`prospective`) and is a directory.
fn make_engine_home(wanted: &Path, prospective: &Path) -> Result<PathBuf, String> {
    if std::fs::symlink_metadata(wanted).is_err() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(wanted)
            .map_err(|error| format!("create the engine's home {}: {error}", wanted.display()))?;
    }
    let made = canonical_directory(wanted, "the engine's home")?;
    if made == prospective {
        Ok(made)
    } else {
        Err(format!(
            "the engine's home {} resolves to {}, not {} where it was checked",
            wanted.display(),
            made.display(),
            prospective.display()
        ))
    }
}

/// What a mask target resolved to.
enum Resolved {
    Directory(PathBuf),
    File(PathBuf),
}

/// `target` canonical, and whether it is a directory or a regular file;
/// why not, else.
fn resolve(target: &Path) -> Result<Resolved, String> {
    let path = std::fs::canonicalize(target).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            "missing".to_string()
        } else {
            error.to_string()
        }
    })?;
    let meta = std::fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
    if meta.is_dir() {
        Ok(Resolved::Directory(path))
    } else if meta.is_file() {
        Ok(Resolved::File(path))
    } else {
        Err(format!(
            "neither a directory nor a regular file ({})",
            path.display()
        ))
    }
}

/// `path` canonical, a directory; made (0700) first when `create`.
fn canonical_directory(path: &Path, what: &str) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("{what} {} is not absolute", path.display()));
    }
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| format!("{what} {}: {error}", path.display()))?;
    if canonical.is_dir() {
        Ok(canonical)
    } else {
        Err(format!("{what} {} is not a directory", path.display()))
    }
}

fn path_word(path: &Path) -> String {
    path.display().to_string()
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.mode() & 0o111 != 0)
}

fn find_program_entry(program: &str, path_env: Option<&str>) -> Option<PathBuf> {
    if program.contains('/') {
        let path = PathBuf::from(program);
        return is_executable(&path).then_some(path);
    }
    let path_os = path_env
        .map(std::ffi::OsString::from)
        .or_else(|| std::env::var_os("PATH"))?;
    std::env::split_paths(&path_os)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

/// `program` as a path: itself when it names one, else the first
/// executable of that name on `PATH`, canonical.
fn find_program(program: &str) -> Option<PathBuf> {
    find_program_entry(program, None).map(|found| std::fs::canonicalize(&found).unwrap_or(found))
}

#[cfg(test)]
mod tests {
    use super::{BwrapView, ViewConfig};
    use spawn_core::port::ViewedLaunch;
    use std::path::{Path, PathBuf};

    /// A view over a made-up home in `scratch` (agy's home below the masked
    /// `~/.gemini`, a `~/.ssh`, a workspace), never run: `plan` only
    /// resolves paths, so these need no `bwrap`.
    fn view_in(scratch: &Path) -> (BwrapView, PathBuf, PathBuf) {
        let home = scratch.join("home");
        let engine_home = home.join(".gemini/antigravity-cli");
        let workspace = scratch.join("ws");
        for dir in [&engine_home, &home.join(".ssh"), &workspace] {
            std::fs::create_dir_all(dir).expect("dir");
        }
        let runtime = scratch.join("run");
        std::fs::create_dir_all(&runtime).expect("run");
        let config = ViewConfig {
            home,
            xdg_config_home: None,
            xdg_state_home: None,
            runtime_dir: runtime.clone(),
            host_directories: Vec::new(),
            loop_dirs: Vec::new(),
            extra_files: Vec::new(),
            host_tmpdir: None,
            private_dir: runtime.join("spawn-engine-view-test"),
            canonical_workspace: None,
            home_isolation: super::HomeIsolation::Blacklist,
            extra_mounts: Vec::new(),
            agy_data_dir: None,
            isolate_workers: false,
            isolate_brain: false,
            agy_program: None,
            inherit_host: super::HostInheritMode::Off,
            inherit_host_paths: Vec::new(),
            claude_program: None,
            claude_data_dir: None,
        };
        let view = BwrapView {
            program: PathBuf::from("/usr/bin/bwrap"),
            config,
        };
        (view, workspace, engine_home)
    }

    fn plan(view: &BwrapView, workspace: &Path, engine_home: &Path) -> Result<super::Plan, String> {
        view.plan(&ViewedLaunch {
            engine: "agy",
            workspace: &workspace.display().to_string(),
            engine_home: &engine_home.display().to_string(),
            worker_id: None,
            exit_file: None,
        })
    }

    /// The re-bound engine home itself is planned (the baseline the next
    /// tests break): agy's home below the masked `~/.gemini`.
    #[test]
    fn an_engine_home_below_another_engines_masked_home_is_re_bound() {
        let scratch = crate::Scratch::new("spawn-view-unit");
        let (view, workspace, engine_home) = view_in(&scratch);
        let planned = plan(&view, &workspace, &engine_home).expect("plan");
        let engine_home = std::fs::canonicalize(&engine_home).expect("home");
        assert!(planned.masks.iter().any(|mask| *mask
            == super::Mask::Directory(engine_home.parent().expect("gemini").to_path_buf())));
    }

    /// A directory mask inside the re-bound engine home (the loop's TMPDIR
    /// there) is not pruned away from the check: its writable bind would
    /// show it, so the plan is refused.
    #[test]
    fn a_directory_mask_inside_a_re_bound_engine_home_is_refused() {
        let scratch = crate::Scratch::new("spawn-view-unit");
        let (mut view, workspace, engine_home) = view_in(&scratch);
        let nested = engine_home.join("shared-tmp");
        std::fs::create_dir_all(&nested).expect("nested");
        view.config.host_tmpdir = Some(nested);
        let refused = plan(&view, &workspace, &engine_home).expect_err("nested directory");
        assert!(refused.contains("overlaps"), "{refused}");
        assert!(refused.contains("shared-tmp"), "{refused}");
    }

    /// The same for a regular-file mask inside the re-bound engine home.
    #[test]
    fn a_file_mask_inside_a_re_bound_engine_home_is_refused() {
        let scratch = crate::Scratch::new("spawn-view-unit");
        let (mut view, workspace, engine_home) = view_in(&scratch);
        let nested = engine_home.join("loop.env");
        std::fs::write(&nested, "secret").expect("nested");
        view.config.extra_files = vec![nested];
        let refused = plan(&view, &workspace, &engine_home).expect_err("nested file");
        assert!(refused.contains("overlaps"), "{refused}");
        assert!(refused.contains("loop.env"), "{refused}");
    }

    /// An engine home refused for lying in a mask (`~/.ssh`) is never made
    /// on disk: it is checked where it would resolve first.
    #[test]
    fn a_refused_engine_home_is_not_created() {
        let scratch = crate::Scratch::new("spawn-view-unit");
        let (view, workspace, _) = view_in(&scratch);
        let inside_ssh = scratch.join("home/.ssh/agy-home/deeper");
        let refused = plan(&view, &workspace, &inside_ssh).expect_err("in ~/.ssh");
        assert!(refused.contains("overlaps"), "{refused}");
        assert!(!scratch.join("home/.ssh/agy-home").exists(), "made on disk");
        // An accepted, missing one is made, 0700.
        let fresh = scratch.join("home/.codex/agy");
        std::fs::create_dir_all(scratch.join("home/.codex")).expect("codex");
        plan(&view, &workspace, &fresh).expect("below ~/.codex");
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(&fresh).expect("made").permissions(),
        );
        assert_eq!(mode & 0o777, 0o700);
    }

    fn lookup(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(known, _)| *known == name)
                .map(|(_, value)| (*value).to_string())
        }
    }

    #[test]
    fn the_config_reads_the_users_home_runtime_and_herdr_directories() {
        let config = ViewConfig::from_env(&lookup(&[
            ("HOME", "/home/u"),
            ("XDG_RUNTIME_DIR", "/run/user/7"),
            ("XDG_CONFIG_HOME", "/home/u/cfg"),
            ("XDG_STATE_HOME", "/home/u/st"),
            ("TMPDIR", "/var/tmp/spawn"),
        ]));
        assert_eq!(config.home, PathBuf::from("/home/u"));
        assert_eq!(config.runtime_dir, PathBuf::from("/run/user/7"));
        assert_eq!(config.host_tmpdir, Some(PathBuf::from("/var/tmp/spawn")));
        // The private directory is inside the masked runtime directory,
        // never in the shared TMPDIR.
        assert_eq!(
            config.private_dir,
            PathBuf::from(format!(
                "/run/user/7/spawn-engine-view-{}",
                crate::current_uid()
            ))
        );
        assert_eq!(
            config.loop_dirs,
            vec![PathBuf::from("/home/u/.local/share/spawn-rsi")]
        );
        let dirs = config.directory_targets();
        for want in [
            "/run/keep",
            "/run/keep-test",
            "/tmp",
            "/run/user/7",
            "/home/u/.config/herdr",
            "/home/u/.config/herdr-core",
            "/home/u/.local/state/herdr-core",
            "/home/u/cfg/herdr",
            "/home/u/cfg/herdr-core",
            "/home/u/st/herdr-core",
            "/home/u/.claude",
            "/home/u/.gemini",
            "/home/u/.config/antigravity",
            "/home/u/.codex",
            "/home/u/.hermes",
            "/home/u/.ssh",
            "/home/u/.gnupg",
            "/home/u/.config/gh",
            "/home/u/.aws",
            "/home/u/.config/anthropic",
            "/home/u/.config/spawn",
            "/home/u/.config/cloudflare",
            "/home/u/.config/observe3-private",
            // $XDG_CONFIG_HOME's own, too.
            "/home/u/cfg/antigravity",
            "/home/u/cfg/gh",
            "/home/u/cfg/anthropic",
            "/home/u/cfg/spawn",
            "/home/u/cfg/cloudflare",
            "/home/u/cfg/observe3-private",
            // The loop's shared TMPDIR is masked, never bound.
            "/var/tmp/spawn",
        ] {
            assert!(dirs.contains(&PathBuf::from(want)), "{want}: {dirs:?}");
        }
        let files = config.file_targets();
        for want in [
            "/home/u/.local/share/spawn-rsi/spawn.env",
            "/home/u/.claude.json",
            "/home/u/.pgpass",
            "/home/u/.netrc",
            "/home/u/.git-credentials",
        ] {
            assert!(files.contains(&PathBuf::from(want)), "{want}: {files:?}");
        }
    }

    #[test]
    fn without_a_runtime_directory_the_users_default_one_is_masked() {
        let config = ViewConfig::from_env(&lookup(&[("HOME", "/home/u")]));
        assert_eq!(
            config.runtime_dir,
            PathBuf::from(format!("/run/user/{}", crate::current_uid()))
        );
        assert_eq!(config.xdg_config_home, None);
        assert_eq!(config.canonical_workspace, None);
        assert_eq!(config.home_isolation, super::HomeIsolation::Blacklist);
        assert!(config.extra_mounts.is_empty());
        assert_eq!(config.agy_data_dir, None);
        assert!(!config.isolate_workers);
        assert!(!config.isolate_brain);
    }

    #[test]
    fn parse_extra_mounts_handles_ro_rw_and_default() {
        use super::{ExtraMount, parse_extra_mounts};
        assert_eq!(
            parse_extra_mounts(" /opt/tools:ro , /data/cache/:rw , /var/lib/ref:/var/lib/two "),
            vec![
                ExtraMount {
                    path: PathBuf::from("/opt/tools"),
                    writable: false,
                },
                ExtraMount {
                    path: PathBuf::from("/data/cache"),
                    writable: true,
                },
                ExtraMount {
                    path: PathBuf::from("/var/lib/ref"),
                    writable: true,
                },
                ExtraMount {
                    path: PathBuf::from("/var/lib/two"),
                    writable: true,
                },
            ]
        );
        assert!(parse_extra_mounts(" , ").is_empty());
    }

    #[test]
    fn from_env_reads_canonical_workspace_whitelist_extra_mounts_and_agy_data_dir() {
        let enabled = ViewConfig::from_env(&lookup(&[
            ("HOME", "/home/u"),
            ("SPAWN_SANDBOX_CANONICAL_WORKSPACE", "/workspace/"),
            ("SPAWN_SANDBOX_HOME_ISOLATION", "whitelist"),
            ("SPAWN_ENGINE_VIEW_WRITABLE", "/opt/a:ro,/opt/b"),
            ("SPAWN_AGY_DATA_DIR", "/data01/agy/"),
        ]));
        assert_eq!(
            enabled.canonical_workspace,
            Some(PathBuf::from("/workspace"))
        );
        assert_eq!(enabled.home_isolation, super::HomeIsolation::Whitelist);
        assert_eq!(enabled.extra_mounts.len(), 2);
        assert!(!enabled.extra_mounts[0].writable);
        assert!(enabled.extra_mounts[1].writable);
        assert_eq!(enabled.agy_data_dir, Some(PathBuf::from("/data01/agy")));
        assert!(enabled.isolate_workers);
        assert!(!enabled.isolate_brain);

        let isolated_brain = ViewConfig::from_env(&lookup(&[
            ("HOME", "/home/u"),
            ("SPAWN_AGY_DATA_DIR", "/data01/agy"),
            ("SPAWN_AGY_ISOLATE_BRAIN", "1"),
        ]));
        assert!(isolated_brain.isolate_workers);
        assert!(isolated_brain.isolate_brain);

        let disabled = ViewConfig::from_env(&lookup(&[
            ("HOME", "/home/u"),
            ("SPAWN_SANDBOX_CANONICAL_WORKSPACE", "OFF"),
            ("SPAWN_AGY_DATA_DIR", "/data01/agy"),
            ("SPAWN_AGY_ISOLATE_WORKERS", "0"),
            ("SPAWN_AGY_ISOLATE_BRAIN", "0"),
        ]));
        assert_eq!(disabled.canonical_workspace, None);
        assert!(!disabled.isolate_workers);
        assert!(!disabled.isolate_brain);
    }

    #[test]
    fn canonical_workspace_validation_rejects_invalid_and_overlapping_paths() {
        let scratch = crate::Scratch::new("spawn-view-unit");
        let (mut view, workspace, engine_home) = view_in(&scratch);
        let canon_home = std::fs::canonicalize(&view.config.home).expect("home");
        for bad in [
            PathBuf::from("relative/ws"),
            PathBuf::from("/"),
            PathBuf::from("/ws/../other"),
            canon_home.join("ws"),
            PathBuf::from("/run/ws"),
            PathBuf::from("/tmp/ws"),
            view.config.private_dir.clone(),
        ] {
            view.config.canonical_workspace = Some(bad.clone());
            let err = plan(&view, &workspace, &engine_home).expect_err(&format!("{bad:?}"));
            assert!(err.contains("canonical workspace"), "{bad:?}: {err}");
        }
    }

    #[test]
    fn whitelist_mode_masks_home_keeps_host_masks_and_refuses_home_binds() {
        let scratch = crate::Scratch::new("spawn-view-unit");
        let (mut view, workspace, engine_home) = view_in(&scratch);
        view.config.home_isolation = super::HomeIsolation::Whitelist;
        let host_tmp = scratch.join("host-tmp");
        std::fs::create_dir_all(&host_tmp).expect("host tmp");
        view.config.host_tmpdir = Some(host_tmp.clone());

        let planned = plan(&view, &workspace, &engine_home).expect("whitelist plan");
        let canon_home = std::fs::canonicalize(&view.config.home).expect("home");
        let canon_host_tmp = std::fs::canonicalize(&host_tmp).expect("host tmp");
        assert!(
            planned
                .masks
                .contains(&super::Mask::Directory(canon_home.clone()))
        );
        assert!(
            planned
                .masks
                .contains(&super::Mask::Directory(canon_host_tmp))
        );
        // Nested ~/.ssh inside $HOME is pruned from emitted masks.
        let canon_ssh = std::fs::canonicalize(view.config.home.join(".ssh")).expect("ssh");
        assert!(!planned.masks.contains(&super::Mask::Directory(canon_ssh)));
        // Missing files inside $HOME are already covered by --tmpfs $HOME, not skipped.
        assert!(
            !planned
                .skipped
                .iter()
                .any(|(path, _)| path.starts_with(&view.config.home)),
            "{:?}",
            planned.skipped
        );

        // Workspace inside $HOME without canonical_workspace is refused.
        let ws_in_home = view.config.home.join("proj");
        std::fs::create_dir_all(&ws_in_home).expect("proj");
        let err = plan(&view, &ws_in_home, &engine_home).expect_err("ws in home");
        assert!(err.contains("overlaps"), "{err}");

        // With canonical_workspace=/workspace, ~/proj mounts outside $HOME cleanly.
        view.config.canonical_workspace = Some(PathBuf::from("/workspace"));
        let err_extra = {
            view.config.extra_mounts = vec![super::ExtraMount {
                path: view.config.home.join("extra"),
                writable: true,
            }];
            plan(&view, &workspace, &engine_home).expect_err("extra in home")
        };
        assert!(err_extra.contains("overlaps $HOME"), "{err_extra}");
        view.config.extra_mounts.clear();

        // Engine home == $HOME or inside ~/.ssh is refused even in whitelist mode.
        assert!(plan(&view, &workspace, &view.config.home).is_err());
        let missing_ssh = view.config.home.join(".ssh/sub");
        assert!(plan(&view, &workspace, &missing_ssh).is_err());
    }

    #[test]
    fn canonical_workspace_rewrites_bind_chdir_pwd_and_exit_file() {
        let scratch = crate::Scratch::new("spawn-view-unit");
        let (mut view, workspace, engine_home) = view_in(&scratch);
        view.config.canonical_workspace = Some(PathBuf::from("/workspace"));
        let canon_ws = std::fs::canonicalize(&workspace).expect("ws");
        let ef = workspace.join(".spawn/exit/exec_1");
        let planned = view
            .plan(&ViewedLaunch {
                engine: "agy",
                workspace: &workspace.display().to_string(),
                engine_home: &engine_home.display().to_string(),
                worker_id: Some("agent_1"),
                exit_file: Some(&ef.display().to_string()),
            })
            .expect("plan");
        let ws_src = canon_ws.display().to_string();
        assert!(
            planned
                .words
                .windows(3)
                .any(|w| w == ["--bind", ws_src.as_str(), "/workspace"])
        );
        assert!(
            !planned
                .words
                .windows(3)
                .any(|w| w == ["--bind", ws_src.as_str(), ws_src.as_str()])
        );
        assert!(
            planned
                .words
                .windows(2)
                .any(|w| w == ["--chdir", "/workspace"])
        );
        assert!(
            planned
                .words
                .windows(3)
                .any(|w| w == ["--setenv", "SPAWN_WORKSPACE", "/workspace"])
        );
        assert!(planned.words.windows(3).any(|w| w
            == [
                "--setenv",
                "SPAWN_SANDBOX_CANONICAL_WORKSPACE",
                "/workspace"
            ]));
        assert!(
            planned
                .words
                .windows(3)
                .any(|w| w == ["--setenv", "PWD", "/workspace"])
        );
        assert!(planned.words.windows(3).any(|w| w
            == [
                "--setenv",
                "SPAWN_EXIT_FILE",
                "/workspace/.spawn/exit/exec_1"
            ]));
        assert!(
            !planned.words.iter().any(|w| w == "--unshare-net"),
            "host netns must stay shared: {:?}",
            planned.words
        );

        view.config.home_isolation = super::HomeIsolation::Whitelist;
        let whitelist_plan = view
            .plan(&ViewedLaunch {
                engine: "agy",
                workspace: &workspace.display().to_string(),
                engine_home: &engine_home.display().to_string(),
                worker_id: Some("agent_1"),
                exit_file: Some(&ef.display().to_string()),
            })
            .expect("whitelist plan");
        assert!(
            !whitelist_plan.words.iter().any(|w| w == "--unshare-net"),
            "host netns must stay shared in whitelist mode"
        );
    }

    #[test]
    fn from_env_and_secret_filters_handle_host_inherit_and_claude_fields() {
        let cfg = ViewConfig::from_env(&lookup(&[
            ("HOME", "/home/u"),
            ("SPAWN_SANDBOX_INHERIT_HOST", "auto"),
            (
                "SPAWN_SANDBOX_INHERIT_HOST_PATHS",
                "/opt/sdk,/home/u/.local/tools:",
            ),
            ("SPAWN_CLAUDE_DATA_DIR", "/data01/claude/"),
        ]));
        assert_eq!(cfg.inherit_host, super::HostInheritMode::Auto);
        assert_eq!(
            cfg.inherit_host_paths,
            vec![
                PathBuf::from("/opt/sdk"),
                PathBuf::from("/home/u/.local/tools"),
            ]
        );
        assert_eq!(cfg.claude_data_dir, Some(PathBuf::from("/data01/claude")));

        assert!(super::gitconfig_has_no_inline_secrets(
            "[user]\n\tname = Ada\n\temail = ada@example.com\n[core]\n\tautocrlf = false\n"
        ));
        assert!(!super::gitconfig_has_no_inline_secrets(
            "[url \"https://user:ghp_tok@github.com/\"]\n\tinsteadOf = https://github.com/\n"
        ));
        assert!(!super::gitconfig_has_no_inline_secrets(
            "[http]\n\textraHeader = Authorization: basic abc\n"
        ));
        assert!(!super::gitconfig_has_no_inline_secrets(
            "[credential]\n\thelper = !sh -c 'echo password=xyz'\n"
        ));
    }

    #[test]
    fn claude_home_allowed_for_claude_only_and_symlink_chain_bound() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = crate::Scratch::new("spawn-view-claude-unit");
        let (mut view, workspace, _) = view_in(&scratch);
        let claude_home = view.config.home.join(".claude");
        std::fs::create_dir_all(&claude_home).expect("claude home");
        let claude_dir_str = claude_home.display().to_string();

        // Still refused for agy.
        let refused_agy = view
            .plan(&ViewedLaunch {
                engine: "agy",
                workspace: &workspace.display().to_string(),
                engine_home: &claude_dir_str,
                worker_id: None,
                exit_file: None,
            })
            .expect_err("agy must refuse .claude");
        assert!(refused_agy.contains("overlaps"), "{refused_agy}");

        // Allowed for claude, plus intermediate symlink hops inside $HOME are bound.
        let real_bin = view.config.home.join(".local/opt/node/lib/claude.exe");
        let link_bin = view.config.home.join(".local/opt/node/bin/claude");
        let entry_dir = scratch.join("toolchain");
        let entry_bin = entry_dir.join("claude");
        std::fs::create_dir_all(real_bin.parent().unwrap()).expect("real parent");
        std::fs::create_dir_all(link_bin.parent().unwrap()).expect("link parent");
        std::fs::create_dir_all(&entry_dir).expect("entry dir");
        std::fs::write(&real_bin, "#!/bin/sh\necho ok\n").expect("real");
        std::fs::set_permissions(&real_bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        std::os::unix::fs::symlink("../lib/claude.exe", &link_bin).expect("sym1");
        std::os::unix::fs::symlink(&link_bin, &entry_bin).expect("sym2");

        view.config.home_isolation = super::HomeIsolation::Whitelist;
        view.config.canonical_workspace = Some(PathBuf::from("/workspace"));
        view.config.claude_program = Some(entry_bin);
        let planned = view
            .plan(&ViewedLaunch {
                engine: "claude",
                workspace: &workspace.display().to_string(),
                engine_home: &claude_dir_str,
                worker_id: Some("w1"),
                exit_file: None,
            })
            .expect("claude plan");
        let canon_real = std::fs::canonicalize(&real_bin)
            .expect("canon real")
            .display()
            .to_string();
        let canon_link_parent = std::fs::canonicalize(link_bin.parent().unwrap()).unwrap();
        let expected_link = canon_link_parent.join("claude").display().to_string();
        assert!(
            planned
                .words
                .windows(3)
                .any(|w| w == ["--ro-bind", canon_real.as_str(), canon_real.as_str()]),
            "{:?}",
            planned.words
        );
        assert!(
            planned
                .words
                .windows(3)
                .any(|w| w == ["--ro-bind", canon_real.as_str(), expected_link.as_str()]),
            "{:?}",
            planned.words
        );
    }
}
