//! What this host can run: the capability probe (decision C2, N5) and
//! `GET /api/capabilities` (design PR 4).
//!
//! The probe asks Claude Code's program, and the program of each other
//! engine `SPAWN_ENGINES` enables, for its version (`<program> --version`),
//! and an enabled agy for the models it knows (`agy models`, read by
//! [`AgyDriver::known_models`]). **An engine besides Claude Code that
//! `SPAWN_ENGINES` does not name runs no process at all** (decision C3:
//! agy runs nowhere before its enabling step): it is reported not enabled,
//! its install and version unknown. Each run is bounded ([`VERSION_LIMIT`],
//! [`MODELS_LIMIT`]), its output included: a run whose output a detached
//! process still holds open past the bound fails, as one that does not
//! exit does ([`run_bounded`]). It runs the program itself (never a shell)
//! with nothing on its standard input, and its process group is killed
//! when the run ends. An engine with no deny rules of its own
//! ([`Guard::Sandbox`], agy) is probed only inside the same filesystem view
//! the Bridge starts it in (`SPAWN_ENGINE_VIEW`, [`view_with`]): `bwrap
//! --die-with-parent --unshare-pid` in a scratch workspace removed after,
//! so neither its probe nor the background updater it leaves (`setsid`)
//! sees what the view masks, nor outlives `bwrap`. With no view under
//! `required` (the default) such an engine is down and its program never
//! runs; `SPAWN_ENGINE_VIEW=off` probes it on the host as is. The engines'
//! login files are `stat`ed, never opened ([`login_files`]). The answer is cached for the
//! process: `spawn loop` probes at its start and then hourly on a thread
//! of its own ([`start`]), so no pass waits on an engine's program;
//! `spawn serve` probes when the cache is empty or an hour old.
//!
//! What delivery reads ([`engines`]): Claude Code always runs; an enabled
//! engine whose program does not answer, or whose model listing fails, is
//! down, alone (a probe failure never takes Claude Code down). A preset's or
//! `SPAWN_AGY_MODEL`'s label that the listing does not name makes no start
//! (`crate::deliver`, `engine_unavailable`). An agy whose `major.minor` is
//! not one Spawn was checked against ([`spawn_core::driver::AGY_TESTED`])
//! is reported, once per version, by the notice `agy_version_untested`
//! ([`alert_untested`]), and refuses nothing: a screen Spawn cannot read
//! reads `Unknown`, which types nothing.
//!
//! The answer also names the terminal backend: tmux, or Herdr with its
//! version, the protocol its server speaks and the agent detection
//! manifests it has active beside the pinned ones (carry list N8).

use std::collections::BTreeSet;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};
use spawn_core::driver::{AgyDriver, DEFAULT, EngineHome, Guard};

use crate::deliver::{EngineReady, Engines};

/// The longest a `<program> --version` may take.
const VERSION_LIMIT: Duration = Duration::from_secs(10);

/// The longest `agy models` may take (it asks agy's service).
const MODELS_LIMIT: Duration = Duration::from_secs(30);

/// How long a probe stays the answer: an hour.
const PROBE_AGE: Duration = Duration::from_hours(1);

/// The most of a program's output a probe reads.
const OUTPUT_LIMIT: u64 = 64 * 1024;

/// One login file of an engine, as `stat` saw it: never opened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct LoginFile {
    pub(crate) path: String,
    pub(crate) present: bool,
}

/// One registered engine, as the probe found it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct EngineProbe {
    /// The driver's id (`claude`, `agy`).
    pub(crate) engine: String,
    pub(crate) title: String,
    /// The program the Bridge launches.
    pub(crate) program: String,
    /// `SPAWN_ENGINES` names it (Claude Code: always).
    pub(crate) enabled: bool,
    /// Its program answered `--version`; null for an engine not enabled,
    /// whose program is never run.
    pub(crate) installed: Option<bool>,
    pub(crate) version: Option<String>,
    /// Whether its `major.minor` is one Spawn was checked against: agy
    /// only, null for an engine with no such list or no version.
    pub(crate) version_tested: Option<bool>,
    /// Its login files, `stat`ed.
    pub(crate) login: Vec<LoginFile>,
    /// The model labels its listing names (agy, enabled), sorted; null when
    /// not listed.
    pub(crate) known_models: Option<Vec<String>>,
    /// The host's own model label for it (`SPAWN_AGY_MODEL`).
    pub(crate) model: Option<String>,
    /// Why it cannot start here, when it cannot (Claude Code: never set by
    /// the probe; its program is the Bridge's to find).
    pub(crate) unavailable: Option<String>,
}

impl EngineProbe {
    /// An engine whose program the probe never ran (`program`, and whether
    /// it is enabled), with why it cannot start.
    fn not_run(
        driver: &dyn spawn_core::driver::AgentDriver,
        (program, enabled): (String, bool),
        login: Vec<LoginFile>,
        model: Option<String>,
        why: String,
    ) -> Self {
        Self {
            engine: driver.id().to_string(),
            title: driver.title().to_string(),
            program,
            enabled,
            installed: None,
            version: None,
            version_tested: None,
            login,
            known_models: None,
            model,
            unavailable: Some(why),
        }
    }
}

/// One probe of every registered engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Probe {
    /// When it ran, on this host's clock.
    pub(crate) at_ms: i64,
    pub(crate) engines: Vec<EngineProbe>,
}

/// A bounded run of `program` with `arguments` within a limit: its
/// standard output, or why not ([`run_bounded`]).
pub(crate) type Run<'a> = &'a dyn Fn(&str, &[&str], Duration) -> Result<String, String>;

/// How the probe of an engine with no deny rules of its own
/// ([`Guard::Sandbox`]) runs its program ([`view_with`]).
pub(crate) enum Boxed {
    /// `SPAWN_ENGINE_VIEW=off`: on the host, as is (the owner's choice).
    Off,
    /// Inside the view: `words` (the view's program and arguments, ending
    /// in `--`) go before the program; `scratch`, the workspace the view
    /// binds, is removed when this is dropped, after the probe's runs.
    Inside {
        words: Vec<String>,
        _scratch: Option<Scratch>,
    },
    /// No view: the engine is down, and its program is never run.
    Unavailable(String),
}

/// A scratch directory a probe's view binds as its workspace, removed whole
/// on drop.
#[derive(Debug)]
pub(crate) struct Scratch(PathBuf);

impl Scratch {
    /// A new 0700 directory below `base` (made new, never one there).
    fn new(base: &Path) -> Result<Self, String> {
        use std::os::unix::fs::DirBuilderExt as _;
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.subsec_nanos());
        let dir = base.join(format!(
            "spawn-capabilities-{}-{}-{nanos}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|error| format!("create {}: {error}", dir.display()))?;
        Ok(Self(dir))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What a probe reads from its host: the environment, a bounded run of a
/// program (its standard output, or why not), whether a path exists
/// (`stat`, never `open`), and how a [`Guard::Sandbox`] engine's program
/// runs (`view`, given the engine's id and home: [`view_with`]).
pub(crate) struct Host<'a> {
    pub(crate) lookup: &'a dyn Fn(&str) -> Option<String>,
    pub(crate) run: Run<'a>,
    pub(crate) exists: &'a dyn Fn(&Path) -> bool,
    pub(crate) view: &'a dyn Fn(&str, &EngineHome) -> Boxed,
}

/// What builds the host's filesystem view ([`spawn_core::port::EngineView`])
/// from the environment, as [`crate::runtime::engine_view`] takes it.
type MakeView<'a> = Box<
    dyn FnOnce(
            &dyn Fn(&str) -> Option<String>,
        ) -> Result<std::sync::Arc<dyn spawn_core::port::EngineView>, String>
        + 'a,
>;

/// How the probe of the [`Guard::Sandbox`] engine `engine` (home `home`)
/// runs on this host, by `SPAWN_ENGINE_VIEW` as the Bridge reads it
/// ([`crate::runtime::engine_view`]): `off`, on the host; otherwise
/// (`required`, unset, or anything else) inside the view `make` builds,
/// in a scratch workspace of its own ([`Scratch`], below the first of
/// `bases` the view can bind), or not at all.
pub(crate) fn view_with(
    lookup: &dyn Fn(&str) -> Option<String>,
    engine: &str,
    home: &EngineHome,
    bases: &[PathBuf],
    make: MakeView<'_>,
) -> Boxed {
    use spawn_core::bridge::EngineViewPolicy;
    let enabled = [(engine.to_string(), home.clone())];
    let view = match crate::runtime::engine_view(lookup, &enabled, make).0 {
        EngineViewPolicy::Off => return Boxed::Off,
        EngineViewPolicy::Required(Err(why)) => {
            return Boxed::Unavailable(format!("no filesystem view on this host ({why})"));
        }
        EngineViewPolicy::Required(Ok(view)) => view,
    };
    let mut why = "no scratch directory for it".to_string();
    for base in bases.iter().filter(|base| base.is_dir()) {
        let scratch = match Scratch::new(base) {
            Ok(scratch) => scratch,
            Err(error) => {
                why = error;
                continue;
            }
        };
        let Some(workspace) = scratch.path().to_str() else {
            continue;
        };
        match view.wrap(&spawn_core::port::ViewedLaunch {
            engine,
            workspace,
            engine_home: &home.sessions,
        }) {
            Ok(words) if words.len() > 1 && words.last().is_some_and(|word| word == "--") => {
                return Boxed::Inside {
                    words,
                    _scratch: Some(scratch),
                };
            }
            Ok(_) => why = "its words do not end in --".to_string(),
            Err(error) => why = error,
        }
    }
    Boxed::Unavailable(format!(
        "its filesystem view ({}) cannot be made: {why}",
        view.name()
    ))
}

/// [`view_with`] on this host's `bwrap`, its scratch workspace below
/// `/var/tmp`, else `$HOME/.cache` (`/tmp` and the loop's `TMPDIR` are
/// masked inside the view, so never there).
fn view_now(engine: &str, home: &EngineHome) -> Boxed {
    let lookup = |name: &str| crate::env_value(name);
    let mut bases = vec![PathBuf::from("/var/tmp")];
    if let Some(dir) = lookup("HOME").filter(|dir| dir.starts_with('/')) {
        bases.push(Path::new(&dir).join(".cache"));
    }
    view_with(
        &lookup,
        engine,
        home,
        &bases,
        Box::new(|lookup| {
            spawn_host::BwrapView::new("bwrap", spawn_host::ViewConfig::from_env(lookup)).map(
                |view| {
                    std::sync::Arc::new(view) as std::sync::Arc<dyn spawn_core::port::EngineView>
                },
            )
        }),
    )
}

/// The login files of `engine` this host would hold: Claude Code's
/// `.credentials.json` in its configuration directory
/// (`SPAWN_CLAUDE_CONFIG_DIR`, else `~/.claude`); agy's file-based login
/// token (`<agy home>/antigravity-oauth-token`, where agy 1.5.17 keeps it)
/// and its home (`SPAWN_AGY_HOME`, else `~/.gemini/antigravity-cli`). Only
/// their presence is ever read (`symlink_metadata`, never opened).
fn login_files(engine: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let home = PathBuf::from(lookup("HOME").unwrap_or_else(|| "/root".to_string()));
    match engine {
        DEFAULT => {
            let dir = lookup("SPAWN_CLAUDE_CONFIG_DIR")
                .filter(|dir| dir.starts_with('/'))
                .map_or_else(|| home.join(".claude"), PathBuf::from);
            vec![dir.join(".credentials.json")]
        }
        "agy" => {
            let agy_home = lookup("SPAWN_AGY_HOME")
                .map_or_else(|| home.join(spawn_core::driver::AGY_HOME), PathBuf::from);
            vec![agy_home.join("antigravity-oauth-token"), agy_home]
        }
        _ => Vec::new(),
    }
}

/// Probe every registered engine on `host` at `at_ms`. Claude Code's
/// program is `SPAWN_CLAUDE_PROGRAM` (else `claude`); another engine's is
/// as `spawn loop` reads it (`SPAWN_AGY_PROGRAM`, else `agy`). Only an
/// enabled agy has its models listed; any failure is that engine's alone.
pub(crate) fn probe_with(host: &Host<'_>, at_ms: i64) -> Probe {
    let enabled = crate::runtime::engines(host.lookup).unwrap_or_default();
    let mut engines = Vec::new();
    for driver in spawn_core::driver::drivers() {
        let id = driver.id();
        let configured = enabled.iter().find(|(name, _)| name == id);
        let program = match (id, configured) {
            (DEFAULT, _) => {
                (host.lookup)("SPAWN_CLAUDE_PROGRAM").unwrap_or_else(|| "claude".into())
            }
            (_, Some((_, home))) => home.program.clone(),
            _ => (host.lookup)(&format!("SPAWN_{}_PROGRAM", id.to_ascii_uppercase()))
                .unwrap_or_else(|| id.to_string()),
        };
        let model = configured.and_then(|(_, home)| home.model.clone());
        let is_enabled = id == DEFAULT || configured.is_some();
        let login = login_files(id, host.lookup)
            .into_iter()
            .map(|path| LoginFile {
                present: (host.exists)(&path),
                path: path.display().to_string(),
            })
            .collect();
        if !is_enabled {
            // Decision C3: an engine the host does not enable is never run,
            // not even for its version.
            engines.push(EngineProbe::not_run(
                *driver,
                (program, false),
                login,
                model,
                format!("SPAWN_ENGINES does not name {id}: it is not enabled on this host"),
            ));
            continue;
        }
        // An engine with no deny rules of its own runs inside its view, or
        // not at all: with none, it is down and its program never runs.
        let boxed = match configured {
            Some((_, home)) if driver.traits().guard == Guard::Sandbox => (host.view)(id, home),
            _ => Boxed::Off,
        };
        let prefix: Vec<&str> = match &boxed {
            Boxed::Off => Vec::new(),
            Boxed::Inside { words, .. } => words.iter().map(String::as_str).collect(),
            Boxed::Unavailable(why) => {
                engines.push(EngineProbe::not_run(
                    *driver,
                    (program, true),
                    login,
                    model,
                    format!(
                        "{id} is probed and run only inside its filesystem view, and {why}: \
                         it will not start (engine_unavailable)"
                    ),
                ));
                continue;
            }
        };
        let run = |arguments: &[&str], limit: Duration| match prefix.split_first() {
            None => (host.run)(&program, arguments, limit),
            Some((view, words)) => {
                let mut all: Vec<&str> = words.to_vec();
                all.push(&program);
                all.extend_from_slice(arguments);
                (host.run)(view, &all, limit)
            }
        };
        let answer = run(&["--version"], VERSION_LIMIT);
        let version = answer
            .as_ref()
            .ok()
            .and_then(|text| AgyDriver::version(text));
        let mut unavailable = match &answer {
            Ok(_) => None,
            Err(why) => Some(format!("{program} --version: {why}")),
        };
        let version_tested = (id == "agy")
            .then(|| version.as_deref().map(AgyDriver::version_tested))
            .flatten();
        let mut known_models = None;
        if id == "agy" && unavailable.is_none() {
            match run(&["models"], MODELS_LIMIT) {
                Ok(listing) => {
                    let known = AgyDriver::known_models(&listing);
                    if known.is_empty() {
                        unavailable = Some(format!("{program} models listed no model"));
                    }
                    known_models = Some(known);
                }
                Err(why) => unavailable = Some(format!("{program} models: {why}")),
            }
        }
        engines.push(EngineProbe {
            engine: id.to_string(),
            title: driver.title().to_string(),
            program,
            enabled: true,
            installed: Some(answer.is_ok()),
            version,
            version_tested,
            login,
            known_models,
            model,
            // Claude Code is never taken down by its probe: the Bridge finds
            // its program at launch, as it always did.
            unavailable: unavailable.filter(|_| id != DEFAULT),
        });
        // The view's scratch workspace goes once both runs are done.
        drop(boxed);
    }
    Probe { at_ms, engines }
}

/// What delivery may start, from `probe`: every enabled engine besides
/// Claude Code is ready (with its listed labels and the host's label) or
/// down (why).
pub(crate) fn engines(probe: &Probe) -> Engines {
    let mut out = Engines {
        probed: true,
        ..Engines::default()
    };
    for engine in probe
        .engines
        .iter()
        .filter(|engine| engine.enabled && engine.engine != DEFAULT)
    {
        match &engine.unavailable {
            Some(why) => {
                out.down.insert(engine.engine.clone(), why.clone());
            }
            None => {
                out.ready.insert(
                    engine.engine.clone(),
                    EngineReady {
                        known: engine
                            .known_models
                            .iter()
                            .flatten()
                            .cloned()
                            .collect::<BTreeSet<String>>(),
                        default_label: engine.model.clone(),
                    },
                );
            }
        }
    }
    out
}

/// Run `program` with `arguments` (no shell, standard input closed) within
/// `limit`: its standard output when it exits 0 and its output closes in
/// time, else why not. The program leads a process group of its own,
/// killed whole when the run ends, however it ends. Its output is read on a
/// thread and waited for only until the same deadline: a process that left
/// the group (`setsid`, as agy's background updater does) and still holds
/// the output open fails the run at the bound, and the reading thread is
/// left to end when that process closes it.
fn run_bounded(program: &str, arguments: &[&str], limit: Duration) -> Result<String, String> {
    use std::os::unix::process::CommandExt as _;
    let deadline = Instant::now() + limit;
    let mut child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|error| format!("not started: {error}"))?;
    let group = rustix::process::Pid::from_child(&child);
    let stdout = child.stdout.take();
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut text = Vec::new();
        if let Some(out) = stdout {
            let _ = out.take(OUTPUT_LIMIT).read_to_end(&mut text);
        }
        let _ = send.send(text);
    });
    let ended = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() >= deadline => {
                break Err(format!("no answer within {} s", limit.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => break Err(error.to_string()),
        }
    };
    let text = match &ended {
        Ok(_) => receive
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| {
                format!(
                    "its output was still held open {} s on (a detached process keeps it)",
                    limit.as_secs()
                )
            }),
        Err(why) => Err(why.clone()),
    };
    let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    let _ = child.kill();
    let _ = child.wait();
    let status = ended?;
    let text = text?;
    if !status.success() {
        return Err(format!("exited with {status}"));
    }
    Ok(String::from_utf8_lossy(&text).into_owned())
}

/// Probe this host now, as it is.
pub(crate) fn probe_now() -> Probe {
    let lookup = |name: &str| crate::env_value(name);
    let exists = |path: &Path| std::fs::symlink_metadata(path).is_ok();
    probe_with(
        &Host {
            lookup: &lookup,
            run: &run_bounded,
            exists: &exists,
            view: &view_now,
        },
        crate::grading::now_ms(),
    )
}

/// The process's latest probe, and when it ran.
static CACHE: Mutex<Option<(Instant, Probe)>> = Mutex::new(None);

fn keep(probe: &Probe) {
    *CACHE.lock().unwrap_or_else(PoisonError::into_inner) = Some((Instant::now(), probe.clone()));
}

/// The latest probe, if one ran and is younger than an hour.
fn fresh() -> Option<Probe> {
    CACHE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .filter(|(at, _)| at.elapsed() < PROBE_AGE)
        .map(|(_, probe)| probe.clone())
}

/// The latest probe, made now when none is younger than an hour.
pub(crate) fn cached() -> Probe {
    fresh().unwrap_or_else(|| {
        let probe = probe_now();
        keep(&probe);
        probe
    })
}

/// What delivery may start, from the latest probe; before any probe, no
/// engine besides Claude Code is known (a task on another one waits).
pub(crate) fn current_engines() -> Engines {
    CACHE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(|(_, probe)| engines(probe))
        .unwrap_or_default()
}

/// Probe at once and then hourly, on a thread of its own, for as long as
/// the process lives (`spawn loop`): no pass waits on an engine's program.
pub(crate) fn start() {
    let started = std::thread::Builder::new()
        .name("spawn-capabilities".to_string())
        .spawn(|| {
            loop {
                keep(&probe_now());
                std::thread::sleep(PROBE_AGE);
            }
        });
    if let Err(error) = started {
        eprintln!("spawn loop warning: the capability probe did not start: {error}");
    }
}

/// The versions of agy this process already raised `agy_version_untested`
/// for (a restarted process's second record for one is refused by the
/// store, which is the same).
static TOLD: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// The `agy_version_untested` notice for `probe`, when its agy is
/// installed at a version whose `major.minor` Spawn was not checked
/// against: `{type, key, kind, subject, evidence, text, notice}` as
/// `spawn_alert_record` takes it, keyed by the version.
pub(crate) fn untested_notice(probe: &Probe) -> Option<Value> {
    let agy = probe
        .engines
        .iter()
        .find(|engine| engine.engine == "agy" && engine.version_tested == Some(false))?;
    let version = agy.version.as_deref()?;
    Some(json!({
        "type": "alert.fired",
        "key": format!("agy_version_untested:{version}"),
        "kind": "agy_version_untested",
        "subject": format!("agy {version}"),
        "evidence": {
            "version": version,
            "tested": spawn_core::driver::AGY_TESTED,
            "program": agy.program,
        },
        "text": format!(
            "agy {version} is not a version Spawn was checked against ({}): nothing is refused \
             (a screen Spawn cannot read reads Unknown, which types nothing); check its screens \
             and transcript, then add its major.minor to AGY_TESTED",
            spawn_core::driver::AGY_TESTED.join(", ")
        ),
        "notice": true,
    }))
}

/// Record the `agy_version_untested` notice of the latest probe, once per
/// version per process (the store refuses a second one for a key). Never
/// probes: before the first probe it does nothing.
pub(crate) fn alert_untested(store: &spawn_postgres::PostgresStore) -> Option<String> {
    let probe = CACHE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .map(|(_, probe)| probe.clone())?;
    let notice = untested_notice(&probe)?;
    let key = notice["key"].as_str()?.to_string();
    if TOLD
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .contains(&key)
    {
        return None;
    }
    match store.alert_record(&notice) {
        Ok(_) | Err(spawn_core::port::StoreError::Rejected(_)) => {
            TOLD.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(key.clone());
            Some(format!("capabilities: {key}"))
        }
        Err(error) => Some(format!("capabilities: {key} not recorded yet: {error}")),
    }
}

/// The terminal backend's part of the answer: tmux, or Herdr with its
/// program's version (`herdr --version`, bounded) and its running server's
/// protocol and manifests ([`spawn_host::HerdrHost::server_facts`]), or why
/// they could not be read.
fn terminal_backend() -> Value {
    let host = match crate::config::terminal_host() {
        Ok(host) => host,
        Err(error) => return json!({ "backend": null, "error": error }),
    };
    match host {
        spawn_host::LocalTerminalHost::Tmux(_) => json!({ "backend": "tmux" }),
        spawn_host::LocalTerminalHost::Herdr(herdr) => {
            let version = run_bounded(herdr.program(), &["--version"], VERSION_LIMIT);
            let mut answer = json!({
                "backend": "herdr",
                "program": herdr.program(),
                "session": herdr.session(),
                "version": version.as_ref().ok().map(|text| text.trim().to_string()),
                "version_error": version.err(),
                "expected_protocol": spawn_host::herdr::PROTOCOL,
                "pinned_manifests": spawn_host::herdr::MANIFESTS
                    .iter()
                    .map(|(agent, version)| json!({ "agent": agent, "version": version }))
                    .collect::<Vec<_>>(),
            });
            match herdr.server_facts() {
                Ok(facts) => {
                    answer["server"] = facts;
                }
                Err(error) => {
                    answer["server"] = Value::Null;
                    answer["server_error"] = json!(error.0);
                }
            }
            answer
        }
    }
}

/// `GET /api/capabilities`: `{probed_at_ms, engines, terminal}`, each
/// engine as [`EngineProbe`] serializes (installed, version, whether that
/// version is tested, login files present, the cached agy model labels, the
/// host's label, why it is unavailable), and the terminal backend
/// ([`terminal_backend`]). The probe is the cached one, made now when none
/// is younger than an hour.
pub(crate) fn capabilities() -> Value {
    let probe = cached();
    json!({
        "probed_at_ms": probe.at_ms,
        "engines": probe.engines,
        "terminal": terminal_backend(),
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::Path;
    use std::time::Duration;

    use super::{Boxed, Host, engines, probe_with, run_bounded, untested_notice, view_with};
    use crate::deliver::Runs;
    use spawn_core::driver::EngineHome;

    /// How the fake host's filesystem view answers for a sandboxed engine.
    #[derive(Clone, Copy)]
    enum View {
        /// `SPAWN_ENGINE_VIEW=off`: run on the host.
        Off,
        /// Wrapped in a made-up `bwrap`.
        Wrap,
        /// None on this host, for this reason.
        Refuse(&'static str),
    }

    struct Fake {
        env: Vec<(&'static str, &'static str)>,
        answers: Vec<(&'static str, Result<&'static str, &'static str>)>,
        runs: RefCell<Vec<String>>,
        stat: RefCell<Vec<String>>,
        view: View,
        viewed: RefCell<Vec<String>>,
    }

    impl Fake {
        fn probe(&self) -> super::Probe {
            let lookup = |name: &str| {
                self.env
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_string())
            };
            let run = |program: &str, arguments: &[&str], _limit: Duration| {
                let line = format!("{program} {}", arguments.join(" "));
                self.runs.borrow_mut().push(line.clone());
                self.answers
                    .iter()
                    .find(|(asked, _)| *asked == line)
                    .map_or(Err("not found".to_string()), |(_, answer)| {
                        answer.map(str::to_string).map_err(str::to_string)
                    })
            };
            let exists = |path: &Path| {
                self.stat.borrow_mut().push(path.display().to_string());
                path.ends_with(".credentials.json")
            };
            let view = |engine: &str, home: &EngineHome| {
                self.viewed
                    .borrow_mut()
                    .push(format!("{engine} {}", home.sessions));
                match self.view {
                    View::Off => Boxed::Off,
                    View::Wrap => Boxed::Inside {
                        words: ["bwrap", "--die-with-parent", "--unshare-pid", "--"]
                            .map(str::to_string)
                            .to_vec(),
                        _scratch: None,
                    },
                    View::Refuse(why) => Boxed::Unavailable(why.to_string()),
                }
            };
            probe_with(
                &Host {
                    lookup: &lookup,
                    run: &run,
                    exists: &exists,
                    view: &view,
                },
                7,
            )
        }
    }

    /// A fake host whose view is off (`SPAWN_ENGINE_VIEW=off`): the probe's
    /// own logic, its program run as is.
    fn fake(
        env: Vec<(&'static str, &'static str)>,
        answers: Vec<(&'static str, Result<&'static str, &'static str>)>,
    ) -> Fake {
        fake_viewed(env, answers, View::Off)
    }

    fn fake_viewed(
        env: Vec<(&'static str, &'static str)>,
        answers: Vec<(&'static str, Result<&'static str, &'static str>)>,
        view: View,
    ) -> Fake {
        Fake {
            env,
            answers,
            runs: RefCell::new(Vec::new()),
            stat: RefCell::new(Vec::new()),
            view,
            viewed: RefCell::new(Vec::new()),
        }
    }

    /// agy enabled and answering: its version, its listed labels, its login
    /// files stated (never opened), and delivery may start it on a listed
    /// label only.
    #[test]
    fn an_enabled_agy_is_probed_for_version_and_models_and_ready_on_its_listed_labels() {
        let host = fake(
            vec![
                ("HOME", "/home/a"),
                ("SPAWN_ENGINES", "claude,agy"),
                ("SPAWN_AGY_MODEL", "gemini-3.8-flash-low"),
            ],
            vec![
                ("claude --version", Ok("2.1.0 (Claude Code)\n")),
                ("agy --version", Ok("1.5.17\n")),
                (
                    "agy models",
                    Ok("Fetching available models...\ngemini-3.8-flash-low\ngemini-3.8-pro\n"),
                ),
            ],
        );
        let probe = host.probe();
        assert_eq!(
            *host.runs.borrow(),
            ["claude --version", "agy --version", "agy models"]
        );
        assert_eq!(
            *host.stat.borrow(),
            [
                "/home/a/.claude/.credentials.json",
                "/home/a/.gemini/antigravity-cli/antigravity-oauth-token",
                "/home/a/.gemini/antigravity-cli",
            ]
        );
        let agy = &probe.engines[1];
        assert_eq!(agy.engine, "agy");
        assert!(agy.enabled);
        assert_eq!(agy.installed, Some(true));
        assert_eq!(agy.version.as_deref(), Some("1.5.17"));
        assert_eq!(agy.version_tested, Some(true));
        assert_eq!(
            agy.known_models.as_deref(),
            Some(
                &[
                    "gemini-3.8-flash-low".to_string(),
                    "gemini-3.8-pro".to_string()
                ][..]
            )
        );
        assert_eq!(agy.unavailable, None);
        assert!(probe.engines[0].login[0].present);
        let ready = engines(&probe);
        assert_eq!(
            ready.runs("agy", Some("gemini-3.8-pro")),
            Runs::On("agy".to_string(), Some("gemini-3.8-pro".to_string()))
        );
        assert_eq!(
            ready.runs("agy", None),
            Runs::On("agy".to_string(), Some("gemini-3.8-flash-low".to_string()))
        );
        assert!(
            matches!(ready.runs("agy", Some("gemini-9")), Runs::Unavailable(why) if why.contains("gemini-9"))
        );
        assert_eq!(untested_notice(&probe), None, "1.5 is tested");
    }

    /// A probe failure takes agy down alone, never Claude Code; a disabled
    /// agy is never asked for its models.
    #[test]
    fn a_failed_probe_disables_agy_only_and_a_disabled_agy_lists_nothing() {
        for (answers, said) in [
            (
                vec![
                    ("agy --version", Ok("1.5.17")),
                    ("agy models", Err("exited with exit status: 1")),
                ],
                "agy models: exited",
            ),
            (vec![("agy --version", Err("not started"))], "agy --version"),
            (
                vec![
                    ("agy --version", Ok("1.5.17")),
                    (
                        "agy models",
                        Ok("Error: Please sign in to view available models."),
                    ),
                ],
                "listed no model",
            ),
        ] {
            let host = fake(vec![("SPAWN_ENGINES", "agy")], answers);
            let probe = host.probe();
            assert_eq!(probe.engines[0].unavailable, None, "Claude is never down");
            let ready = engines(&probe);
            assert_eq!(
                ready.runs("claude", None),
                Runs::On("claude".to_string(), None)
            );
            match ready.runs("agy", Some("gemini-3.8-pro")) {
                Runs::Unavailable(why) => assert!(why.contains(said), "{why}"),
                other => panic!("{said}: {other:?}"),
            }
        }
        // Decision C3: a disabled agy runs no process at all, not even
        // `--version`; only Claude Code's program is asked.
        let host = fake(
            vec![("HOME", "/home/a")],
            vec![
                ("claude --version", Ok("2.1.0")),
                ("agy --version", Ok("1.6.2")),
            ],
        );
        let probe = host.probe();
        assert_eq!(*host.runs.borrow(), ["claude --version"]);
        let agy = &probe.engines[1];
        assert!(!agy.enabled);
        assert_eq!(agy.installed, None);
        assert_eq!(agy.version, None);
        assert_eq!(agy.known_models, None);
        assert_eq!(agy.login.len(), 2, "its login files are only stated");
        assert!(matches!(
            engines(&probe).runs("agy", Some("x-1")),
            Runs::Unavailable(why) if why.contains("not enabled")
        ));
        assert_eq!(untested_notice(&probe), None, "a version never asked");
        let host = fake(
            vec![("SPAWN_ENGINES", "agy")],
            vec![
                ("agy --version", Ok("1.6.2")),
                ("agy models", Ok("gemini-3.8-pro")),
            ],
        );
        let probe = host.probe();
        let notice = untested_notice(&probe).expect("1.6 is untested");
        assert_eq!(notice["key"], "agy_version_untested:1.6.2");
        assert_eq!(notice["kind"], "agy_version_untested");
        assert_eq!(notice["notice"], true);
    }

    /// Must-fix of review 589: a detached process (`setsid`, as agy's
    /// background updater) that keeps the output open does not hold the
    /// probe past its bound: the run fails near the bound, and agy is
    /// reported down, never left unknown.
    #[test]
    fn a_detached_child_holding_the_output_open_fails_the_run_at_its_bound() {
        let dir = std::env::temp_dir().join(format!("h4r-bounded-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch");
        let pidfile = dir.join("pid");
        let script = format!(
            "echo 1.5.17; setsid sh -c 'echo $$ > {}; exec sleep 30' & sleep 0.2; exit 0",
            pidfile.display()
        );
        let started = std::time::Instant::now();
        let answer = run_bounded("sh", &["-c", &script], Duration::from_secs(1));
        let took = started.elapsed();
        if let Ok(pid) = std::fs::read_to_string(&pidfile) {
            let _ = std::process::Command::new("kill").arg(pid.trim()).status();
        }
        let _ = std::fs::remove_dir_all(&dir);
        let why = answer.expect_err("held open past the bound");
        assert!(why.contains("held open"), "{why}");
        assert!(took < Duration::from_secs(3), "bounded: {took:?}");
        // A run that ends and closes its output answers as before.
        assert_eq!(
            run_bounded("sh", &["-c", "echo 1.5.17"], Duration::from_secs(5)).as_deref(),
            Ok("1.5.17\n")
        );
        assert!(run_bounded("sh", &["-c", "exit 3"], Duration::from_secs(5)).is_err());
    }

    const WRAPPED_VERSION: &str = "bwrap --die-with-parent --unshare-pid -- agy --version";
    const WRAPPED_MODELS: &str = "bwrap --die-with-parent --unshare-pid -- agy models";

    fn agy_answers() -> Vec<(&'static str, Result<&'static str, &'static str>)> {
        vec![
            ("claude --version", Ok("2.1.0 (Claude Code)\n")),
            ("agy --version", Ok("1.5.17\n")),
            ("agy models", Ok("gemini-3.8-pro\n")),
            (WRAPPED_VERSION, Ok("1.5.17\n")),
            (WRAPPED_MODELS, Ok("gemini-3.8-pro\n")),
        ]
    }

    /// F1 (O55): a sandboxed engine (agy, `Guard::Sandbox`) with no view on
    /// this host is down, and its program is never run on the host, not
    /// even for `--version`; Claude Code is probed unwrapped, as before.
    #[test]
    fn a_sandboxed_engine_with_no_view_is_unavailable_and_never_run() {
        let host = fake_viewed(
            vec![("HOME", "/home/a"), ("SPAWN_ENGINES", "claude,agy")],
            agy_answers(),
            View::Refuse("no filesystem view on this host (bwrap is not installed)"),
        );
        let probe = host.probe();
        assert_eq!(*host.runs.borrow(), ["claude --version"], "agy never ran");
        assert_eq!(
            *host.viewed.borrow(),
            ["agy /home/a/.gemini/antigravity-cli"]
        );
        let agy = &probe.engines[1];
        assert!(agy.enabled);
        assert_eq!(agy.installed, None);
        assert_eq!(agy.version, None);
        assert_eq!(agy.known_models, None);
        let why = agy.unavailable.as_deref().expect("down");
        assert!(why.contains("bwrap is not installed"), "{why}");
        assert!(why.contains("engine_unavailable"), "{why}");
        assert_eq!(agy.login.len(), 2, "its login files are still only stated");
        let ready = engines(&probe);
        assert!(matches!(
            ready.runs("agy", Some("gemini-3.8-pro")),
            Runs::Unavailable(why) if why.contains("filesystem view")
        ));
        assert_eq!(probe.engines[0].unavailable, None, "Claude is never down");
        assert_eq!(
            ready.runs("claude", None),
            Runs::On("claude".to_string(), None)
        );
    }

    /// F1: with a view, agy's `--version` and `models` run as the view's
    /// program with agy and its arguments after `--`; Claude Code's
    /// `--version` stays unwrapped, and a Claude-only host asks no view.
    #[test]
    fn a_sandboxed_engines_probes_run_inside_its_view_and_claudes_do_not() {
        let host = fake_viewed(
            vec![("HOME", "/home/a"), ("SPAWN_ENGINES", "claude,agy")],
            agy_answers(),
            View::Wrap,
        );
        let probe = host.probe();
        assert_eq!(
            *host.runs.borrow(),
            ["claude --version", WRAPPED_VERSION, WRAPPED_MODELS]
        );
        assert_eq!(
            *host.viewed.borrow(),
            ["agy /home/a/.gemini/antigravity-cli"]
        );
        let agy = &probe.engines[1];
        assert_eq!(agy.installed, Some(true));
        assert_eq!(agy.version.as_deref(), Some("1.5.17"));
        assert_eq!(agy.unavailable, None);
        assert_eq!(
            engines(&probe).runs("agy", Some("gemini-3.8-pro")),
            Runs::On("agy".to_string(), Some("gemini-3.8-pro".to_string()))
        );
        // A wrapped run that fails takes agy down alone.
        let failing = fake_viewed(
            vec![("SPAWN_ENGINES", "agy")],
            vec![(WRAPPED_VERSION, Err("exited with exit status: 1"))],
            View::Wrap,
        );
        let probe = failing.probe();
        assert!(
            probe.engines[1]
                .unavailable
                .as_deref()
                .is_some_and(|why| why.contains("agy --version: exited")),
            "{:?}",
            probe.engines[1].unavailable
        );
        assert_eq!(probe.engines[0].unavailable, None);
        // Claude Code alone: no view is asked for, nothing is wrapped.
        for view in [View::Wrap, View::Refuse("none")] {
            let claude = fake_viewed(vec![("HOME", "/home/a")], agy_answers(), view);
            let probe = claude.probe();
            assert_eq!(*claude.runs.borrow(), ["claude --version"]);
            assert!(claude.viewed.borrow().is_empty());
            assert_eq!(probe.engines[0].installed, Some(true));
        }
    }

    /// A view that records what it was asked and wraps in a made-up
    /// `bwrap`, or refuses.
    #[derive(Debug, Default)]
    struct FakeEngineView {
        refuse: Option<&'static str>,
        asked: std::sync::Mutex<Vec<(String, String, String)>>,
    }

    impl spawn_core::port::EngineView for FakeEngineView {
        fn name(&self) -> &'static str {
            "bwrap"
        }

        fn wrap(&self, launch: &spawn_core::port::ViewedLaunch<'_>) -> Result<Vec<String>, String> {
            self.asked.lock().expect("asked").push((
                launch.engine.to_string(),
                launch.workspace.to_string(),
                launch.engine_home.to_string(),
            ));
            match self.refuse {
                Some(why) => Err(why.to_string()),
                None => Ok(["/usr/bin/bwrap", "--unshare-pid", "--"]
                    .map(str::to_string)
                    .to_vec()),
            }
        }
    }

    /// F1: `SPAWN_ENGINE_VIEW` as the Bridge reads it. `off` runs on the
    /// host and builds no view; required (unset, or an invalid value) with
    /// no view, or a view that refuses the launch, is unavailable; a view
    /// wraps the probe in a scratch workspace of its own, removed on drop.
    #[test]
    fn the_probes_view_follows_spawn_engine_view_and_cleans_its_scratch() {
        let home = EngineHome {
            program: "agy".to_string(),
            sessions: "/home/a/.gemini/antigravity-cli".to_string(),
            model: None,
        };
        let base = std::env::temp_dir().join(format!("spawn-cap-view-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("base");
        let bases = [base.join("missing"), base.clone()];
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_string())
            }
        };
        let off = env(&[("SPAWN_ENGINE_VIEW", "off")]);
        let boxed = view_with(
            &off,
            "agy",
            &home,
            &bases,
            Box::new(|_| panic!("off builds no view")),
        );
        assert!(matches!(boxed, Boxed::Off));
        for pairs in [
            &[][..],
            &[("SPAWN_ENGINE_VIEW", "required")][..],
            &[("SPAWN_ENGINE_VIEW", "maybe")][..],
        ] {
            let lookup = env(pairs);
            let boxed = view_with(
                &lookup,
                "agy",
                &home,
                &bases,
                Box::new(|_| Err("bwrap is not installed".to_string())),
            );
            assert!(
                matches!(&boxed, Boxed::Unavailable(why) if why.contains("bwrap is not installed")),
                "{pairs:?}"
            );
        }
        let unset = env(&[]);
        let view = std::sync::Arc::new(FakeEngineView::default());
        let made = view.clone();
        let boxed = view_with(
            &unset,
            "agy",
            &home,
            &bases,
            Box::new(move |_| Ok(made as std::sync::Arc<dyn spawn_core::port::EngineView>)),
        );
        let Boxed::Inside {
            words,
            _scratch: scratch,
        } = boxed
        else {
            panic!("inside the view");
        };
        assert_eq!(words, ["/usr/bin/bwrap", "--unshare-pid", "--"]);
        let scratch = scratch.expect("a scratch workspace");
        let workspace = scratch.path().to_path_buf();
        assert!(
            workspace.is_dir() && workspace.starts_with(&base),
            "{workspace:?}"
        );
        assert_eq!(
            *view.asked.lock().expect("asked"),
            [(
                "agy".to_string(),
                workspace.display().to_string(),
                home.sessions.clone()
            )]
        );
        drop(scratch);
        assert!(!workspace.exists(), "removed on drop");
        // A view that refuses the launch: unavailable, nothing left behind.
        let refusing = std::sync::Arc::new(FakeEngineView {
            refuse: Some("the workspace overlaps /tmp, which the view masks"),
            ..FakeEngineView::default()
        });
        let boxed = view_with(
            &unset,
            "agy",
            &home,
            &bases,
            Box::new(move |_| Ok(refusing as std::sync::Arc<dyn spawn_core::port::EngineView>)),
        );
        assert!(
            matches!(&boxed, Boxed::Unavailable(why) if why.contains("overlaps /tmp")),
            "refused"
        );
        assert_eq!(
            std::fs::read_dir(&base).expect("base").count(),
            0,
            "no scratch left"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// F2 (O55): agy 1.5.17 keeps its login token in a file below its home;
    /// that file and the home are stated (`SPAWN_AGY_HOME` too), never a
    /// file agy does not write.
    #[test]
    fn agys_login_file_is_its_token_below_its_home() {
        let lookup = |name: &str| match name {
            "HOME" => Some("/home/a".to_string()),
            "SPAWN_AGY_HOME" => Some("/srv/agy".to_string()),
            _ => None,
        };
        assert_eq!(
            super::login_files("agy", &lookup),
            [
                std::path::PathBuf::from("/srv/agy/antigravity-oauth-token"),
                std::path::PathBuf::from("/srv/agy"),
            ]
        );
    }
}
