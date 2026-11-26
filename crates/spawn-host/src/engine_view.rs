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
//! where `<TMPDIR>` is the engine's own, `<engine home>/.spawn-tmp`
//! ([`TMP_FALLBACK`], 0700): never the loop's shared `TMPDIR`, which other
//! host jobs write to and which is masked whole.
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
//!   workspace, the engine's home and its `TMPDIR` bound writable, each at
//!   its own exact path, so the Bridge reads the transcripts from the host
//!   where the engine writes them.
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

use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
            Err(format!(
                "{} is not a private empty 0444 file",
                file.display()
            ))
        }
    }

    /// Every target resolved now: the directories and the regular files to
    /// mask, and what was skipped and why. All of them, a target inside
    /// another masked directory included: the writable binds are checked
    /// against every one ([`BwrapView::refuse_binds`]), and only the words
    /// leave the nested ones out ([`prune_nested`]). Nothing is dropped for
    /// the engine's sake: an engine home equal to, or holding, a target is
    /// refused.
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
        (directories, files, skipped)
    }

    /// Why a writable bind of `workspace` or `engine_home` is refused:
    /// it would uncover a mask, lie in one, or reach the view's `private`
    /// directory (see the module's documentation).
    fn refuse_binds(
        &self,
        workspace: &Path,
        engine_home: &Path,
        private: &Path,
        masked: &[&PathBuf],
    ) -> Result<(), String> {
        let home = std::fs::canonicalize(&self.config.home).unwrap_or(self.config.home.clone());
        // A writable bind may hold no mask (its bind would uncover it) and
        // lie in none (it would open a hole in it), nor reach the view's
        // private directory either way.
        let overlap = |bound: &Path| {
            masked
                .iter()
                .find(|target| target.starts_with(bound) || bound.starts_with(target))
        };
        let reaches_private =
            |bound: &Path| private.starts_with(bound) || bound.starts_with(private);
        if reaches_private(workspace) {
            return Err(format!(
                "the workspace {} overlaps the view's private directory {}",
                workspace.display(),
                private.display()
            ));
        }
        if let Some(target) = overlap(workspace) {
            return Err(format!(
                "the workspace {} overlaps {}, which the view masks",
                workspace.display(),
                target.display()
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
            target.starts_with(engine_home)
                || (engine_home.starts_with(target)
                    && !engine_homes.iter().any(|dir| dir == **target))
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

    /// The view of one launch: every target resolved now, the masks first
    /// and the writable binds last. Refused (an error) when the workspace
    /// or the engine's home cannot be bound without uncovering a mask, or
    /// cannot be made.
    pub fn plan(&self, launch: &ViewedLaunch<'_>) -> Result<Plan, String> {
        let workspace = canonical_directory(Path::new(launch.workspace), "the workspace")?;
        let private = self.private_dir()?;
        let (all_directories, all_files, skipped) = self.targets();
        let masked: Vec<&PathBuf> = all_directories.iter().chain(all_files.iter()).collect();
        // The engine's home is checked where it would resolve before
        // anything is made on disk: a refused one is never created.
        let wanted = Path::new(launch.engine_home);
        let prospective = prospective_canonical(wanted)
            .map_err(|why| format!("the engine's home {}: {why}", wanted.display()))?;
        self.refuse_binds(&workspace, &prospective, &private, &masked)?;
        let engine_home = make_engine_home(wanted, &prospective)?;
        // Made: checked again where it does resolve.
        self.refuse_binds(&workspace, &engine_home, &private, &masked)?;
        let tmpdir = engine_tmpdir(&engine_home)?;
        let (directories, files) = prune_nested(&all_directories, &all_files);

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
        // The TMPDIR below the engine's home is already writable through
        // that bind: bound again, its path would be resolved anew when
        // `bwrap` runs, after a writer outside the view could swap it.
        let mut bound: Vec<&Path> = Vec::new();
        for path in [workspace.as_path(), engine_home.as_path(), tmpdir.as_path()] {
            if bound.iter().any(|done| path.starts_with(done)) {
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
            runtime_dir: runtime.clone(),
            host_directories: Vec::new(),
            loop_dirs: Vec::new(),
            extra_files: Vec::new(),
            host_tmpdir: None,
            private_dir: runtime.join("spawn-engine-view-test"),
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
        assert!(
            planned
                .masks
                .iter()
                .any(|mask| *mask == super::Mask::Directory(engine_home.parent().expect("gemini").to_path_buf()))
        );
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
    }
}
