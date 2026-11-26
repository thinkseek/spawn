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
//!     --bind <TMPDIR> <TMPDIR> --setenv TMPDIR <TMPDIR> --chdir <ws> -- <runner> <engine> <args>
//! ```
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
//!   workspace, the engine's home and `TMPDIR` bound writable, each at its
//!   own exact path, so the Bridge reads the transcripts from the host where
//!   the engine writes them. An engine's home inside a masked directory
//!   (`~/.gemini/antigravity-cli` under `~/.gemini`) is bound back into the
//!   mask's empty tmpfs.
//!
//! The masks, resolved again at every launch ([`BwrapView::plan`]):
//!
//! - directories, each an empty `--tmpfs`: `/run/keep`, `/run/keep-test`,
//!   the whole `$XDG_RUNTIME_DIR`, the whole `/tmp` (the fleet's tmux
//!   sockets), the Herdr session directory (`$XDG_CONFIG_HOME/herdr`, by
//!   default `~/.config/herdr`, which holds its socket: a client in the view
//!   could otherwise ask Herdr for a pane outside it), `~/.claude`, every
//!   other engine's home (`~/.gemini` but for the engine's own home below
//!   it, `~/.config/antigravity`, `~/.codex`, `~/.hermes`), `~/.ssh`,
//!   `~/.gnupg`, `~/.config/gh`, `~/.aws`, `~/.config/anthropic`,
//!   `~/.config/spawn`, `~/.config/cloudflare` and
//!   `~/.config/observe3-private`;
//! - regular files, each read-only bound over by a private empty 0444 file
//!   (never `/dev/null`, whose bind answers `EACCES` instead of reading
//!   empty): the loop's `spawn.env` and every other `*.env*` and `env.sh` in
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

use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use spawn_core::port::{EngineView, ViewedLaunch};

/// The longest the functional check of `bwrap` may take.
const CHECK_LIMIT: Duration = Duration::from_secs(10);

/// The directory below the engine's home its `TMPDIR` falls back on when
/// no usable one is given.
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

/// What a view hides and where it keeps its own empty file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewConfig {
    /// The user's home (`$HOME`).
    pub home: PathBuf,
    /// `$XDG_CONFIG_HOME`, when set: its `herdr` is masked too.
    pub xdg_config_home: Option<PathBuf>,
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
        Self {
            xdg_config_home: lookup("XDG_CONFIG_HOME")
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
        targets.push(self.home.join(".config/herdr"));
        if let Some(config) = &self.xdg_config_home {
            targets.push(config.join("herdr"));
        }
        for dir in HOME_DIRECTORIES {
            targets.extend(self.below_home(dir));
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
            Err(format!("{} is not a private empty 0444 file", file.display()))
        }
    }

    /// Every target resolved now: the directories and the regular files to
    /// mask, and what was skipped and why. A target inside a masked
    /// directory goes with it. Nothing is dropped for the engine's sake: an
    /// engine home equal to, or holding, a target is refused
    /// ([`BwrapView::plan`]).
    fn targets(&self) -> (Vec<PathBuf>, Vec<PathBuf>, Vec<(PathBuf, String)>) {
        let home = std::fs::canonicalize(&self.config.home).unwrap_or(self.config.home.clone());
        let mut skipped = Vec::new();
        let mut directories: Vec<PathBuf> = Vec::new();
        let mut files: Vec<PathBuf> = Vec::new();
        let targets = self
            .config
            .directory_targets()
            .into_iter()
            .chain(self.config.file_targets());
        for target in targets {
            match resolve(&target) {
                // A directory holding the home (a host TMPDIR of `/`) would
                // hide everything: said, not masked.
                Ok(Resolved::Directory(path)) if home.starts_with(&path) => {
                    skipped.push((target, "holds the user's home".to_string()));
                }
                Ok(Resolved::Directory(path)) => {
                    if !directories.contains(&path) {
                        directories.push(path);
                    }
                }
                Ok(Resolved::File(path)) => {
                    if !files.contains(&path) {
                        files.push(path);
                    }
                }
                Err(why) => skipped.push((target, why)),
            }
        }
        let inside_other = |path: &Path, all: &[PathBuf]| {
            all.iter()
                .any(|dir| dir.as_path() != path && path.starts_with(dir))
        };
        let all_directories = directories.clone();
        directories.retain(|dir| !inside_other(dir, &all_directories));
        files.retain(|file| !inside_other(file, &directories));

        (directories, files, skipped)
    }

    /// The view of one launch: every target resolved now, the masks first
    /// and the writable binds last. Refused (an error) when the workspace
    /// or the engine's home cannot be bound without uncovering a mask, or
    /// cannot be made.
    pub fn plan(&self, launch: &ViewedLaunch<'_>) -> Result<Plan, String> {
        let workspace = canonical_directory(Path::new(launch.workspace), "the workspace", false)?;
        let engine_home =
            canonical_directory(Path::new(launch.engine_home), "the engine's home", true)?;
        let home = std::fs::canonicalize(&self.config.home).unwrap_or(self.config.home.clone());
        let private = self.private_dir()?;

        let (directories, files, skipped) = self.targets();
        let masked: Vec<&PathBuf> = directories.iter().chain(files.iter()).collect();
        // A writable bind may hold no mask (its bind would uncover it) and
        // lie in none (it would open a hole in it), nor reach the view's
        // private directory either way.
        let overlap = |bound: &Path| {
            masked
                .iter()
                .find(|target| target.starts_with(bound) || bound.starts_with(target))
        };
        let reaches_private =
            |bound: &Path| private.starts_with(bound) || bound.starts_with(&private);
        if reaches_private(&workspace) {
            return Err(format!(
                "the workspace {} overlaps the view's private directory {}",
                workspace.display(),
                private.display()
            ));
        }
        if let Some(target) = overlap(&workspace) {
            return Err(format!(
                "the workspace {} overlaps {}, which the view masks",
                workspace.display(),
                target.display()
            ));
        }
        if engine_home == Path::new("/") || home.starts_with(&engine_home) {
            return Err(format!(
                "the engine's home {} holds the user's home",
                engine_home.display()
            ));
        }
        if reaches_private(&engine_home) {
            return Err(format!(
                "the engine's home {} overlaps the view's private directory {}",
                engine_home.display(),
                private.display()
            ));
        }
        // The engine's home may lie below another engine's masked home
        // (`~/.gemini/antigravity-cli` below `~/.gemini`), and nowhere else
        // a mask covers; it may never be a masked path, nor hold one.
        let engine_homes: Vec<PathBuf> = ENGINE_HOMES
            .iter()
            .flat_map(|dir| self.config.below_home(dir))
            .filter_map(|dir| std::fs::canonicalize(dir).ok())
            .collect();
        let refused = masked.iter().find(|target| {
            target.starts_with(&engine_home)
                || (engine_home.starts_with(target) && !engine_homes.iter().any(|dir| dir == **target))
        });
        if let Some(target) = refused {
            return Err(format!(
                "the engine's home {} overlaps {}, which the view masks",
                engine_home.display(),
                target.display()
            ));
        }
        let tmpdir = engine_tmpdir(&engine_home)?;

        let mut words = vec![
            self.program.display().to_string(),
            "--ro-bind".to_string(),
            "/".to_string(),
            "/".to_string(),
            "--dev".to_string(),
            "/dev".to_string(),
            "--proc".to_string(),
            "/proc".to_string(),
            "--die-with-parent".to_string(),
            "--unshare-pid".to_string(),
        ];
        let mut masks = Vec::new();
        for dir in &directories {
            words.extend(["--tmpfs".to_string(), path_word(dir)]);
            masks.push(Mask::Directory(dir.clone()));
        }
        if !files.is_empty() {
            let empty = self.empty_file()?;
            for file in &files {
                words.extend(["--ro-bind".to_string(), path_word(&empty), path_word(file)]);
                masks.push(Mask::File(file.clone()));
            }
        }
        let mut bound: Vec<&Path> = Vec::new();
        for path in [workspace.as_path(), engine_home.as_path(), tmpdir.as_path()] {
            if bound.contains(&path) {
                continue;
            }
            bound.push(path);
            words.extend(["--bind".to_string(), path_word(path), path_word(path)]);
        }
        words.extend([
            "--setenv".to_string(),
            "TMPDIR".to_string(),
            path_word(&tmpdir),
            "--chdir".to_string(),
            path_word(&workspace),
            "--".to_string(),
        ]);
        Ok(Plan {
            words,
            masks,
            tmpdir,
            skipped,
        })
    }
}

impl EngineView for BwrapView {
    fn name(&self) -> &'static str {
        "bwrap"
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
fn canonical_directory(path: &Path, what: &str, create: bool) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("{what} {} is not absolute", path.display()));
    }
    if create && !path.exists() {
        std::fs::create_dir_all(path)
            .map_err(|error| format!("create {what} {}: {error}", path.display()))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("chmod {what} {}: {error}", path.display()))?;
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

/// `program` as a path: itself when it names one, else the first
/// executable of that name on `PATH`, canonical.
fn find_program(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let path = PathBuf::from(program);
        return is_executable(&path).then(|| std::fs::canonicalize(&path).unwrap_or(path));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
        .map(|found| std::fs::canonicalize(&found).unwrap_or(found))
}

#[cfg(test)]
mod tests {
    use super::ViewConfig;
    use std::path::PathBuf;

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
            ("TMPDIR", "/var/tmp/spawn"),
        ]));
        assert_eq!(config.home, PathBuf::from("/home/u"));
        assert_eq!(config.runtime_dir, PathBuf::from("/run/user/7"));
        assert_eq!(config.tmpdir, Some(PathBuf::from("/var/tmp/spawn")));
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
            "/home/u/cfg/herdr",
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
    }
}
