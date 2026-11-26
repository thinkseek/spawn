//! What this host can run: the capability probe (decision C2, N5) and
//! `GET /api/capabilities` (design PR 4).
//!
//! The probe asks each registered engine's program for its version
//! (`<program> --version`) and, for an engine `SPAWN_ENGINES` enables that
//! is told its model at launch (agy), for the models it knows (`agy
//! models`, read by [`AgyDriver::known_models`]). Each run is bounded
//! ([`VERSION_LIMIT`], [`MODELS_LIMIT`]), runs the program itself (never a
//! shell) with nothing on its standard input, and is killed with its whole
//! process group when its bound passes. The engines' login files are
//! `stat`ed, never opened ([`login_files`]). The answer is cached for the
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
use spawn_core::driver::{AgyDriver, DEFAULT};

use crate::deliver::{EngineReady, Engines};

/// The longest a `<program> --version` may take.
const VERSION_LIMIT: Duration = Duration::from_secs(10);

/// The longest `agy models` may take (it asks agy's service).
const MODELS_LIMIT: Duration = Duration::from_secs(30);

/// How long a probe stays the answer: an hour.
const PROBE_AGE: Duration = Duration::from_secs(3600);

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
    /// Its program answered `--version`.
    pub(crate) installed: bool,
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

/// One probe of every registered engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Probe {
    /// When it ran, on this host's clock.
    pub(crate) at_ms: i64,
    pub(crate) engines: Vec<EngineProbe>,
}

/// What a probe reads from its host: the environment, a bounded run of a
/// program (its standard output, or why not), and whether a path exists
/// (`stat`, never `open`).
pub(crate) struct Host<'a> {
    pub(crate) lookup: &'a dyn Fn(&str) -> Option<String>,
    pub(crate) run: &'a dyn Fn(&str, &[&str], Duration) -> Result<String, String>,
    pub(crate) exists: &'a dyn Fn(&Path) -> bool,
}

/// The login files of `engine` this host would hold: Claude Code's
/// `.credentials.json` in its configuration directory
/// (`SPAWN_CLAUDE_CONFIG_DIR`, else `~/.claude`); agy's Google login
/// (`~/.gemini/oauth_creds.json`) and its home (`SPAWN_AGY_HOME`, else
/// `~/.gemini/antigravity-cli`). Only their presence is ever read.
fn login_files(engine: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let home = PathBuf::from(lookup("HOME").unwrap_or_else(|| "/root".to_string()));
    match engine {
        DEFAULT => {
            let dir = lookup("SPAWN_CLAUDE_CONFIG_DIR")
                .filter(|dir| dir.starts_with('/'))
                .map_or_else(|| home.join(".claude"), PathBuf::from);
            vec![dir.join(".credentials.json")]
        }
        "agy" => vec![
            home.join(".gemini/oauth_creds.json"),
            lookup("SPAWN_AGY_HOME")
                .map_or_else(|| home.join(spawn_core::driver::AGY_HOME), PathBuf::from),
        ],
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
            (DEFAULT, _) => (host.lookup)("SPAWN_CLAUDE_PROGRAM").unwrap_or_else(|| "claude".into()),
            (_, Some((_, home))) => home.program.clone(),
            _ => (host.lookup)(&format!("SPAWN_{}_PROGRAM", id.to_ascii_uppercase()))
                .unwrap_or_else(|| id.to_string()),
        };
        let model = configured.and_then(|(_, home)| home.model.clone());
        let is_enabled = id == DEFAULT || configured.is_some();
        let answer = (host.run)(&program, &["--version"], VERSION_LIMIT);
        let version = answer.as_ref().ok().and_then(|text| AgyDriver::version(text));
        let mut unavailable = match &answer {
            Ok(_) => None,
            Err(why) => Some(format!("{program} --version: {why}")),
        };
        let version_tested = (id == "agy")
            .then(|| version.as_deref().map(AgyDriver::version_tested))
            .flatten();
        let mut known_models = None;
        if id == "agy" && is_enabled && unavailable.is_none() {
            match (host.run)(&program, &["models"], MODELS_LIMIT) {
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
        let login = login_files(id, host.lookup)
            .into_iter()
            .map(|path| LoginFile {
                present: (host.exists)(&path),
                path: path.display().to_string(),
            })
            .collect();
        engines.push(EngineProbe {
            engine: id.to_string(),
            title: driver.title().to_string(),
            program,
            enabled: is_enabled,
            installed: answer.is_ok(),
            version,
            version_tested,
            login,
            known_models,
            model,
            // Claude Code is never taken down by its probe: the Bridge finds
            // its program at launch, as it always did.
            unavailable: unavailable.filter(|_| id != DEFAULT),
        });
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
/// `limit`: its standard output when it exits 0, else why not. The program
/// leads a process group of its own, killed whole when the run ends,
/// however it ends.
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
    let reader = std::thread::spawn(move || {
        let mut text = Vec::new();
        if let Some(out) = stdout {
            let _ = out.take(OUTPUT_LIMIT).read_to_end(&mut text);
        }
        text
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
    let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    let _ = child.kill();
    let _ = child.wait();
    let status = ended?;
    let text = reader.join().unwrap_or_default();
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
pub(crate) fn capabilities() -> Result<Value, String> {
    let probe = cached();
    Ok(json!({
        "probed_at_ms": probe.at_ms,
        "engines": probe.engines,
        "terminal": terminal_backend(),
    }))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::Path;
    use std::time::Duration;

    use super::{Host, engines, probe_with, untested_notice};
    use crate::deliver::Runs;

    struct Fake {
        env: Vec<(&'static str, &'static str)>,
        answers: Vec<(&'static str, Result<&'static str, &'static str>)>,
        runs: RefCell<Vec<String>>,
        stat: RefCell<Vec<String>>,
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
            probe_with(
                &Host {
                    lookup: &lookup,
                    run: &run,
                    exists: &exists,
                },
                7,
            )
        }
    }

    fn fake(
        env: Vec<(&'static str, &'static str)>,
        answers: Vec<(&'static str, Result<&'static str, &'static str>)>,
    ) -> Fake {
        Fake {
            env,
            answers,
            runs: RefCell::new(Vec::new()),
            stat: RefCell::new(Vec::new()),
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
                "/home/a/.gemini/oauth_creds.json",
                "/home/a/.gemini/antigravity-cli",
            ]
        );
        let agy = &probe.engines[1];
        assert_eq!(agy.engine, "agy");
        assert!(agy.enabled && agy.installed);
        assert_eq!(agy.version.as_deref(), Some("1.5.17"));
        assert_eq!(agy.version_tested, Some(true));
        assert_eq!(
            agy.known_models.as_deref(),
            Some(&["gemini-3.8-flash-low".to_string(), "gemini-3.8-pro".to_string()][..])
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
        assert!(matches!(ready.runs("agy", Some("gemini-9")), Runs::Unavailable(why) if why.contains("gemini-9")));
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
                    ("agy models", Ok("Error: Please sign in to view available models.")),
                ],
                "listed no model",
            ),
        ] {
            let host = fake(vec![("SPAWN_ENGINES", "agy")], answers);
            let probe = host.probe();
            assert_eq!(probe.engines[0].unavailable, None, "Claude is never down");
            let ready = engines(&probe);
            assert_eq!(ready.runs("claude", None), Runs::On("claude".to_string(), None));
            match ready.runs("agy", Some("gemini-3.8-pro")) {
                Runs::Unavailable(why) => assert!(why.contains(said), "{why}"),
                other => panic!("{said}: {other:?}"),
            }
        }
        let host = fake(Vec::new(), vec![("agy --version", Ok("1.6.2"))]);
        let probe = host.probe();
        assert!(!host.runs.borrow().iter().any(|run| run.ends_with("models")));
        assert!(!probe.engines[1].enabled);
        assert!(matches!(
            engines(&probe).runs("agy", Some("x-1")),
            Runs::Unavailable(why) if why.contains("not enabled")
        ));
        let notice = untested_notice(&probe).expect("1.6 is untested");
        assert_eq!(notice["key"], "agy_version_untested:1.6.2");
        assert_eq!(notice["kind"], "agy_version_untested");
        assert_eq!(notice["notice"], true);
    }
}
