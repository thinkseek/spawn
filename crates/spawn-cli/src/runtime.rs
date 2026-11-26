//! Assembly of the Bridge and the Observer over one store and the host
//! adapters (git workspaces, tmux terminals, judge).

use std::sync::Arc;

use spawn_core::bridge::{Bridge, BridgeConfig, LaunchEnvironment, ModelRelay};
use spawn_core::observe::{Observer, StateJudge};
use spawn_core::port::{CommandQueue, EventLog, TerminalHost, WorkspaceStore};
use spawn_jev::{JevConfig, JevJudge};

use crate::Flags;
use crate::config::{
    WorkspaceSettings, env_or, env_value, open_store, required_env, spawn_program, workspace_root,
};
use crate::{context, sources, supervise};

/// Build the Bridge and the Observer over one store.
///
/// Both keep the event-log cursor in memory, so the resident loop builds
/// them once. Rebuilding them per step would replay the whole log on every
/// tick, and that cost grows with the log.
///
/// The resident loop's Bridge owns its worker and makes its previous
/// process's unfinished leases claimable at once
/// ([`BridgeConfig::recover_worker_leases`]).
pub(crate) fn runtime(flags: &Flags) -> Result<(Bridge, Observer), String> {
    runtime_with(flags, true)
}

/// The runtime of one `spawn tick`: its Bridge takes no ownership of the
/// worker and expires no lease, so a tick beside a live holder (a loop, or a
/// claim made without the ownership protocol) leaves that lease to run out.
pub(crate) fn tick_runtime(flags: &Flags) -> Result<(Bridge, Observer), String> {
    runtime_with(flags, false)
}

fn runtime_with(flags: &Flags, recover_worker_leases: bool) -> Result<(Bridge, Observer), String> {
    // The Bridge merges main into branches with `git --attr-source`.
    crate::config::require_git()?;
    let store = Arc::new(open_store()?);
    let repo = flags
        .get("repo")
        .or_else(|| env_value("SPAWN_GIT_REPO"))
        .ok_or_else(|| "--repo or SPAWN_GIT_REPO is required".to_string())?;
    let root = workspace_root(flags)
        .ok_or_else(|| "--root or SPAWN_WORKSPACE_ROOT is required".to_string())?;
    // Every agent's workspace comes from its mission's source; agents no
    // mission owns use the default repository. Every store is the one
    // engine `SPAWN_WORKSPACE_BACKEND` chose for the root.
    let settings = WorkspaceSettings::from_env(std::path::Path::new(&root))?;
    settings.servable()?;
    let factory = settings.factory(std::path::Path::new(&root));
    let missions = sources::MissionWorkspaces::new(
        factory(std::path::Path::new(&repo)),
        std::path::Path::new(&root),
        store.clone(),
        factory,
    )
    .with_cas_dedup(settings.cas_dedup);
    // A push queued by the last process and lost with it is made now.
    if let Err(error) = missions.republish() {
        eprintln!("sources: {error}; a push lost with the last process waits for its next");
    }
    let workspaces: Arc<dyn WorkspaceStore> = Arc::new(missions);
    let mut config = bridge_config()?;
    config.recover_worker_leases = recover_worker_leases;
    // The transcript archive's credential mask (objective 175): the
    // installed collector's places, or a stand-in honoured only in a gate's
    // sandbox and only for roots outside the account's home, said in one
    // line. With a stand-in both roots are pinned, and every file below them
    // is reached without following a link. An enabled engine's own home
    // (agy's, `SPAWN_ENGINES`) holds transcripts the archive copies too, so
    // the mask covers it; Claude Code alone (the default) adds none.
    let mut roots = vec![
        std::path::PathBuf::from(&root),
        std::path::PathBuf::from(&config.claude_projects),
    ];
    roots.extend(
        config
            .engines
            .iter()
            .map(|(_, home)| std::path::PathBuf::from(&home.sessions)),
    );
    let (archive_mask, stand_in) = spawn_host::archive_mask::CollectorMask::from_env(&roots);
    if let Some(stand_in) = &stand_in {
        eprintln!("{}", stand_in.said());
    }
    let pinned = spawn_host::archive_mask::loop_roots(
        stand_in.as_ref(),
        &roots[0],
        &roots[1],
        &spawn_host::home::Home::of_account(),
    )?;
    let mut host = crate::config::terminal_host()?;
    if let Some(projects) = pinned.projects {
        host = host.with_files_beneath(projects);
    }
    let terminals: Arc<dyn TerminalHost> = Arc::new(host);

    let log: Arc<dyn EventLog> = store.clone();
    let queue: Arc<dyn CommandQueue> = store.clone();
    // Beside the workspaces: the transcripts are the agents' history,
    // reached from the workspace root (pinned with a stand-in).
    let transcripts = spawn_host::LocalTranscriptArchive::for_loop(pinned.workspace);
    // This Bridge types each message's marker: the first loop that did
    // is recorded once (`host.messages_marked`), from when a command never
    // typed is a message whose taking is tracked (crate::alerts,
    // `message_not_taken`). A login that may not record it types the
    // markers all the same.
    let marked = serde_json::json!({
        "type": "host.messages_marked",
        "started_at_ms": store.now_ms().unwrap_or_default(),
    });
    if let Err(error) = store.host_record(&marked) {
        eprintln!("spawn: host.messages_marked: {error}");
    }
    let mut bridge = Bridge::new(
        Arc::clone(&log),
        queue,
        workspaces,
        Arc::clone(&terminals),
        config,
    )
    .with_transcript_archive(Arc::new(transcripts))
    .with_archive_mask(Arc::new(archive_mask));
    bridge.recover().map_err(|error| error.to_string())?;

    let context = context::HostContext::new(open_store()?, terminals);
    let mut observer = Observer::new(log).with_context(Arc::new(context));
    if let Some(judge) = judge(flags) {
        // Every call watched: its crossings between answering and failing
        // are recorded (`host.judge`), what the `judge_down` alert reads.
        let watched = crate::judge_watch::WatchedJudge::new(judge, store.clone());
        observer = observer.with_judge(Arc::new(watched));
    }
    observer.recover().map_err(|error| error.to_string())?;
    Ok((bridge, observer))
}

/// Programs and environment the Bridge puts into every terminal it starts.
/// Whether the Bridge answers an engine's startup trust dialog itself
/// under `mode` (`SPAWN_AUTO_APPROVE`): under every mode but `off`.
fn auto_trust(mode: crate::resolve::Mode) -> bool {
    mode != crate::resolve::Mode::Off
}

fn bridge_config() -> Result<BridgeConfig, String> {
    let mut config = BridgeConfig::new(&env_or("SPAWN_WORKER", "spawn-worker"));
    config.spawn_program = spawn_program();
    config.claude_program = env_or("SPAWN_CLAUDE_PROGRAM", "claude");
    config.claude_projects = claude_projects();
    config.engines = engines(env_value)?;
    config.model_relay = model_relay();
    // SPAWN_AUTO_APPROVE=off: not even the startup trust dialog is answered
    // automatically (`crate::resolve`).
    config.auto_trust = auto_trust(crate::resolve::Mode::from_env().0);
    config.screen_quiet = std::time::Duration::from_millis(
        env_value("SPAWN_SCREEN_QUIET_MS")
            .and_then(|text| text.parse().ok())
            .unwrap_or(500),
    );
    // The engines' files are watched, so a pass reads a transcript or an
    // exit status when it changed and not on every pass. A host with no
    // watch reads them every pass, as before, and says so once.
    match spawn_host::InotifyWatch::new() {
        Ok(watch) => config.watch = Some(Arc::new(watch)),
        Err(error) => eprintln!(
            "spawn: {error}; every live execution's transcript and exit status are read on \
             every pass"
        ),
    }
    // A workspace builds against the deps store at SUPPLY_PA_DEPS
    // (`gate/scenarios/build`); the pane's engine runs the build, so it gets
    // the path. Unset, the store's default under $HOME stands.
    config.environment.extend(supply_pa_env(env_value));
    // Engines on a Claude Code directory of their own, when the loop names
    // one (SPAWN_CLAUDE_CONFIG_DIR, issue #79); else the user's own.
    config.environment.extend(claude_config_env(env_value)?);
    // A worker's pane gets no database login: the Bridge reads what the
    // engine does from its files and screen. The supervisor's pane gets its
    // own login: it reads the fleet and commands workers through the
    // gateway, nothing more.
    // Without an owner password there is no supervisor login to hand out:
    // fatal on a supervised host, irrelevant elsewhere.
    match spawn_postgres::role_url(
        &required_env("SPAWN_DATABASE_URL")?,
        crate::supervise::SUPERVISOR_PRINCIPAL,
    ) {
        Ok(url) => config.session_environment.push((
            supervise::SUPERVISOR_SESSION.to_string(),
            vec![
                ("SPAWN_DATABASE_URL".to_string(), url),
                ("SPAWN_GATEWAY".to_string(), "1".to_string()),
            ],
        )),
        Err(error) if crate::config::supervised() => return Err(error),
        Err(_) => {}
    }
    if let Some(fleet) = FleetTestDatabase::from_env(env_value, |url| {
        spawn_postgres::cluster_identity(url).map_err(|error| error.to_string())
    }) {
        config.launch_environment = Some(Arc::new(fleet));
    }
    Ok(config)
}

/// The deps store's path for engines that build a tree's dependencies
/// (SUPPLY_PA_DEPS, `gate/scenarios/build`): a path, never a login, and only
/// what the integrator has.
fn supply_pa_env(lookup: impl Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    lookup("SUPPLY_PA_DEPS")
        .filter(|value| !value.is_empty())
        .map(|value| vec![("SUPPLY_PA_DEPS".to_string(), value)])
        .unwrap_or_default()
}

/// The variable a test reads its database from.
const TEST_DATABASE: &str = "SPAWN_TEST_DATABASE_URL";

/// How a server's identity is read: [`spawn_postgres::cluster_identity`].
type Identity = fn(&str) -> Result<String, String>;

/// The fleet's own test server, which engines get as
/// `SPAWN_TEST_DATABASE_URL`: `SPAWN_FLEET_TEST_DATABASE_URL`
/// (`deploy/fleet-test-db.sh` sets one up), or none.
///
/// Never the gates' shared server (this process's own
/// `SPAWN_TEST_DATABASE_URL`) nor the live database: a worker's tests
/// there slow a gate's or touch the fleet's record. Fails closed, like the
/// gates' [`crate::gate::withhold_owners_cluster`]: an engine gets the URL
/// only once the fleet's server and every other one configured answer with
/// their identity (`system_identifier`) and the fleet's differs from each.
/// Until then every launch goes without it and checks again, so a fleet
/// server that comes up later is given from then on; the reason it is
/// withheld goes to the loop's output once per reason. Once given, it is
/// not checked again: the URLs do not change while the loop runs.
struct FleetTestDatabase {
    fleet: String,
    /// The servers it must not be: (variable, what it is, URL).
    others: Vec<(&'static str, &'static str, String)>,
    identity: Identity,
    state: std::sync::Mutex<FleetCheck>,
}

#[derive(Default)]
struct FleetCheck {
    given: bool,
    /// The last reason said, so a reason is said once.
    told: Option<String>,
}

impl std::fmt::Debug for FleetTestDatabase {
    // Never the URLs: they hold passwords.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FleetTestDatabase")
            .field(
                "others",
                &self
                    .others
                    .iter()
                    .map(|(name, ..)| *name)
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl FleetTestDatabase {
    /// The fleet's test server named in the environment `lookup` reads,
    /// or none when `SPAWN_FLEET_TEST_DATABASE_URL` is unset.
    fn from_env(lookup: impl Fn(&str) -> Option<String>, identity: Identity) -> Option<Self> {
        let fleet = lookup("SPAWN_FLEET_TEST_DATABASE_URL")?;
        let others = [
            (TEST_DATABASE, "the gates' shared test server"),
            ("SPAWN_DATABASE_URL", "the live database"),
        ]
        .into_iter()
        .filter_map(|(name, what)| lookup(name).map(|url| (name, what, url)))
        .collect();
        Some(Self {
            fleet,
            others,
            identity,
            state: std::sync::Mutex::new(FleetCheck::default()),
        })
    }

    /// Why the URL may not be given now, or nothing when it may.
    fn withheld(&self) -> Option<String> {
        if self.others.is_empty() {
            return Some(
                "SPAWN_DATABASE_URL is unset, so the fleet's test server cannot be told apart \
                 from the live one"
                    .to_string(),
            );
        }
        let fleet = match (self.identity)(&self.fleet) {
            Ok(fleet) => fleet,
            Err(error) => {
                return Some(format!(
                    "the fleet's test server has no readable identity: {error}"
                ));
            }
        };
        for (name, what, url) in &self.others {
            if *url == self.fleet {
                return Some(format!("SPAWN_FLEET_TEST_DATABASE_URL is {what} ({name})"));
            }
            match (self.identity)(url) {
                Ok(other) if other != fleet => {}
                Ok(_) => {
                    return Some(format!(
                        "SPAWN_FLEET_TEST_DATABASE_URL reaches {what} ({name})"
                    ));
                }
                Err(error) => {
                    return Some(format!("{what} ({name}) has no readable identity: {error}"));
                }
            }
        }
        None
    }
}

impl LaunchEnvironment for FleetTestDatabase {
    fn environment(&self) -> Vec<(String, String)> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.given {
            match self.withheld() {
                None => {
                    state.given = true;
                    if state.told.take().is_some() {
                        eprintln!(
                            "bridge: the fleet's test server checks out; engines started from \
                             now on get SPAWN_TEST_DATABASE_URL"
                        );
                    }
                }
                Some(reason) => {
                    if state.told.as_ref() != Some(&reason) {
                        eprintln!(
                            "bridge: engines get no SPAWN_TEST_DATABASE_URL: {reason} \
                             (checked again at each launch)"
                        );
                        state.told = Some(reason);
                    }
                    return Vec::new();
                }
            }
        }
        vec![(TEST_DATABASE.to_string(), self.fleet.clone())]
    }
}

/// The model relay (`scripts/model-relay.py`) Claude Code engines reach
/// their models through: `SPAWN_MODEL_RELAY` (`http://127.0.0.1:8791` on a
/// host that runs `spawn-model-relay`; unset or `off` for none), with the
/// cheap profile's main model `SPAWN_CHEAP_MODEL` and every profile's small
/// model `SPAWN_SMALL_MODEL`. The default profile's main model
/// `SPAWN_DEFAULT_MODEL` and its slots `SPAWN_SONNET_MODEL`,
/// `SPAWN_OPUS_MODEL` and `SPAWN_SUBAGENT_MODEL` are each optional: unset
/// or empty keeps Claude Code's own (issue #79). Names only: the relay
/// holds the keys.
///
/// Opt-in, so a host or test without a relay never probes one, and a
/// configured relay that is down is a warning worth reading.
fn model_relay() -> Option<ModelRelay> {
    resolve_model_relay(env_value)
}

/// [`model_relay`] over the environment `lookup` reads (an empty value
/// reads as unset).
fn resolve_model_relay(lookup: impl Fn(&str) -> Option<String>) -> Option<ModelRelay> {
    let value = |name: &str| lookup(name).filter(|value| !value.trim().is_empty());
    let url = value("SPAWN_MODEL_RELAY").filter(|url| url != "off")?;
    let or = |name: &str, fallback: &str| value(name).unwrap_or_else(|| fallback.to_string());
    Some(ModelRelay {
        url: url.trim_end_matches('/').to_string(),
        cheap_model: or("SPAWN_CHEAP_MODEL", "deepseek-flash"),
        small_model: or("SPAWN_SMALL_MODEL", "deepseek-flash"),
        default_model: value("SPAWN_DEFAULT_MODEL"),
        sonnet_model: value("SPAWN_SONNET_MODEL"),
        opus_model: value("SPAWN_OPUS_MODEL"),
        subagent_model: value("SPAWN_SUBAGENT_MODEL"),
    })
}

/// The Claude Code directory engines run with instead of the user's own
/// `~/.claude` (issue #79): `SPAWN_CLAUDE_CONFIG_DIR`, opt-in. Set, the
/// engines get it as `CLAUDE_CONFIG_DIR`, so their settings, login and
/// transcripts are that directory's: its `settings.json` must give them
/// the gateway's token, as `~/.claude/settings.json` does otherwise (the
/// relay holds none). Unset or empty, nothing changes: the engines inherit
/// the user's own settings, token included.
///
/// An absolute path only: a relative one (or `~/…`, which no shell expands
/// here) would name another directory in each engine's workspace, and the
/// engines' deny rules name it by its path. Refused, `spawn loop` does not
/// start.
pub(crate) fn claude_config_dir(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Option<String>, String> {
    let Some(dir) = lookup("SPAWN_CLAUDE_CONFIG_DIR").filter(|dir| !dir.trim().is_empty()) else {
        return Ok(None);
    };
    if !dir.starts_with('/') {
        return Err(format!(
            "SPAWN_CLAUDE_CONFIG_DIR must be an absolute path, not {dir:?}"
        ));
    }
    let trimmed = dir.trim_end_matches('/');
    Ok(Some(if trimmed.is_empty() {
        dir
    } else {
        trimmed.to_string()
    }))
}

/// The engine environment [`claude_config_dir`] adds, or why it refuses.
fn claude_config_env(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Vec<(String, String)>, String> {
    Ok(claude_config_dir(lookup)?
        .map(|dir| ("CLAUDE_CONFIG_DIR".to_string(), dir))
        .into_iter()
        .collect())
}

/// The engines this host runs besides Claude Code, and where each lives:
/// `SPAWN_ENGINES`, a comma-separated list of registered engines
/// (`claude`, `agy`), `claude` alone when unset, so that landing an engine
/// enables nothing until the owner names it. agy's home is `SPAWN_AGY_HOME`
/// (else `$HOME/.gemini/antigravity-cli`), its program `SPAWN_AGY_PROGRAM`
/// (else `agy`) and its model `SPAWN_AGY_MODEL` (unset, a start of agy is
/// refused: agy left to pick falls back to its costliest model). An
/// unknown name stops `spawn loop`, said.
fn engines(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Vec<(String, spawn_core::driver::EngineHome)>, String> {
    let named = lookup("SPAWN_ENGINES").unwrap_or_else(|| spawn_core::driver::DEFAULT.to_string());
    let mut engines = Vec::new();
    for name in named
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        if spawn_core::driver::driver(name).is_none() {
            let known: Vec<&str> = spawn_core::driver::drivers()
                .iter()
                .map(|driver| driver.id())
                .collect();
            return Err(format!(
                "SPAWN_ENGINES names {name}, which is no engine: the engines are {}",
                known.join(", ")
            ));
        }
        if name == spawn_core::driver::DEFAULT || engines.iter().any(|(known, _)| known == name) {
            continue;
        }
        let home = lookup("SPAWN_AGY_HOME").unwrap_or_else(|| {
            let user = lookup("HOME").unwrap_or_else(|| "/root".to_string());
            format!(
                "{}/{}",
                user.trim_end_matches('/'),
                spawn_core::driver::AGY_HOME
            )
        });
        engines.push((
            name.to_string(),
            spawn_core::driver::EngineHome {
                program: lookup("SPAWN_AGY_PROGRAM").unwrap_or_else(|| "agy".to_string()),
                sessions: home,
                model: lookup("SPAWN_AGY_MODEL"),
            },
        ));
    }
    Ok(engines)
}

/// Claude Code's transcript directory, which is where the Bridge reads it.
pub(crate) fn claude_projects() -> String {
    resolve_claude_projects(env_value)
}

/// [`claude_projects`] over the environment `lookup` reads:
/// `SPAWN_CLAUDE_PROJECTS` always wins; else the engines' own directory's
/// (`$SPAWN_CLAUDE_CONFIG_DIR/projects`), else
/// `$CLAUDE_CONFIG_DIR/projects`, else `$HOME/.claude/projects`.
fn resolve_claude_projects(lookup: impl Fn(&str) -> Option<String>) -> String {
    if let Some(projects) = lookup("SPAWN_CLAUDE_PROJECTS") {
        return projects;
    }
    // An invalid one stops `spawn loop` (`bridge_config`); a reader here
    // goes on as without it.
    if let Ok(Some(dir)) = claude_config_dir(&lookup) {
        return format!("{}/projects", dir.trim_end_matches('/'));
    }
    lookup("CLAUDE_CONFIG_DIR").map_or_else(
        || {
            let home = lookup("HOME").unwrap_or_else(|| "/root".to_string());
            format!("{home}/.claude/projects")
        },
        |dir| format!("{dir}/projects"),
    )
}

/// The model judge at `--judge-url` or `SPAWN_JUDGE_URL`. Without one the
/// Observer judges by the rules alone, and a case they cannot settle asks
/// for attention.
fn judge(flags: &Flags) -> Option<Arc<dyn StateJudge>> {
    let url = flags
        .get("judge-url")
        .or_else(|| env_value("SPAWN_JUDGE_URL"))?;
    let mut config = JevConfig::new(&url);
    if let Some(model) = env_value("SPAWN_JUDGE_MODEL") {
        config.model = model;
    }
    Some(Arc::new(JevJudge::new(config)))
}

#[cfg(test)]
mod tests {
    use super::{
        FleetTestDatabase, auto_trust, claude_config_env, engines, resolve_claude_projects,
        resolve_model_relay, supply_pa_env,
    };

    /// `SPAWN_AUTO_APPROVE=off` turns the Bridge's startup trust answer off;
    /// unset, `startup` and `all` keep it.
    #[test]
    fn only_auto_approve_off_turns_the_startup_trust_answer_off() {
        use crate::resolve::Mode;
        for (value, answers) in [
            (None, true),
            (Some("startup"), true),
            (Some("all"), true),
            (Some("off"), false),
        ] {
            let mode = Mode::parse(value).expect("mode");
            assert_eq!(auto_trust(mode), answers, "{value:?}");
        }
        assert!(spawn_core::bridge::BridgeConfig::new("w").auto_trust, "on by default");
    }
    use spawn_core::bridge::LaunchEnvironment;

    fn environment(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(known, _)| known == name)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn on_the_host_the_transcripts_are_under_the_hosts_home() {
        let host = environment(&[("HOME", "/home/owner")]);
        assert_eq!(
            resolve_claude_projects(&host),
            "/home/owner/.claude/projects"
        );
        let configured = environment(&[("HOME", "/home/owner"), ("CLAUDE_CONFIG_DIR", "/cc")]);
        assert_eq!(resolve_claude_projects(configured), "/cc/projects");
    }

    #[test]
    fn a_deps_store_reaches_the_pane_and_nothing_else_does() {
        let store = environment(&[(
            "SUPPLY_PA_DEPS",
            "/home/o/.local/share/spawn-rsi/deps/supply-pa/current",
        )]);
        assert_eq!(
            supply_pa_env(store),
            [(
                "SUPPLY_PA_DEPS".to_string(),
                "/home/o/.local/share/spawn-rsi/deps/supply-pa/current".to_string()
            )]
        );
        assert!(supply_pa_env(environment(&[("SUPPLY_PA_DEPS", "")])).is_empty());
        assert!(supply_pa_env(environment(&[])).is_empty());
    }

    #[test]
    fn an_explicit_transcript_directory_wins() {
        let explicit = environment(&[
            ("HOME", "/home/owner"),
            ("CLAUDE_CONFIG_DIR", "/cc"),
            ("SPAWN_CLAUDE_PROJECTS", "/mnt/transcripts"),
        ]);
        assert_eq!(resolve_claude_projects(&explicit), "/mnt/transcripts");
    }

    #[test]
    fn the_engines_own_claude_directory_comes_before_claude_s_own() {
        let dedicated = environment(&[
            ("HOME", "/home/owner"),
            ("CLAUDE_CONFIG_DIR", "/cc"),
            ("SPAWN_CLAUDE_CONFIG_DIR", "/data/fleet-claude/"),
        ]);
        assert_eq!(
            resolve_claude_projects(&dedicated),
            "/data/fleet-claude/projects"
        );
        let explicit = environment(&[
            ("SPAWN_CLAUDE_CONFIG_DIR", "/data/fleet-claude"),
            ("SPAWN_CLAUDE_PROJECTS", "/mnt/transcripts"),
        ]);
        assert_eq!(resolve_claude_projects(&explicit), "/mnt/transcripts");
        let empty = environment(&[
            ("HOME", "/home/owner"),
            ("CLAUDE_CONFIG_DIR", "/cc"),
            ("SPAWN_CLAUDE_CONFIG_DIR", ""),
        ]);
        assert_eq!(resolve_claude_projects(&empty), "/cc/projects");
        let home = environment(&[("HOME", "/home/owner"), ("SPAWN_CLAUDE_CONFIG_DIR", "  ")]);
        assert_eq!(
            resolve_claude_projects(&home),
            "/home/owner/.claude/projects"
        );
    }

    /// Opt-in: only a named directory reaches the engines, as
    /// `CLAUDE_CONFIG_DIR`. Unset, the engines get nothing that would move
    /// them off the user's own settings, whose token they inherit.
    #[test]
    fn engines_get_a_claude_directory_only_when_the_loop_names_one() {
        assert_eq!(
            claude_config_env(environment(&[(
                "SPAWN_CLAUDE_CONFIG_DIR",
                "/data/fleet-claude/"
            )])),
            Ok(vec![(
                "CLAUDE_CONFIG_DIR".to_string(),
                "/data/fleet-claude".to_string()
            )])
        );
        assert_eq!(claude_config_env(environment(&[])), Ok(Vec::new()));
        assert_eq!(
            claude_config_env(environment(&[("SPAWN_CLAUDE_CONFIG_DIR", "")])),
            Ok(Vec::new())
        );
        // Claude Code's own variable, set for the loop, is not handed on.
        assert_eq!(
            claude_config_env(environment(&[("CLAUDE_CONFIG_DIR", "/cc")])),
            Ok(Vec::new())
        );
    }

    /// Issue #79's review (F4): only an absolute directory is taken; a
    /// relative one, or one under `~/` (no shell expands it here), stops the
    /// loop, and the transcripts are read as without it.
    #[test]
    fn a_claude_directory_that_is_not_absolute_is_refused() {
        for dir in ["fleet-claude", "./fleet-claude", "~/.claude-fleet", "~"] {
            let pairs = [("HOME", "/home/owner"), ("SPAWN_CLAUDE_CONFIG_DIR", dir)];
            let lookup = environment(&pairs);
            let refused = claude_config_env(&lookup).expect_err(dir);
            assert!(refused.contains("absolute"), "{refused}");
            assert_eq!(
                resolve_claude_projects(&lookup),
                "/home/owner/.claude/projects",
                "{dir}"
            );
        }
    }

    #[test]
    fn the_relay_reads_its_optional_default_profile_slots() {
        assert_eq!(resolve_model_relay(environment(&[])), None);
        assert_eq!(
            resolve_model_relay(environment(&[("SPAWN_MODEL_RELAY", "off")])),
            None
        );
        let plain = resolve_model_relay(environment(&[(
            "SPAWN_MODEL_RELAY",
            "http://127.0.0.1:8791/",
        )]))
        .expect("a relay");
        assert_eq!(plain.url, "http://127.0.0.1:8791");
        assert_eq!(plain.cheap_model, "deepseek-flash");
        assert_eq!(plain.small_model, "deepseek-flash");
        assert_eq!(plain.default_model, None);
        assert_eq!(plain.sonnet_model, None);
        assert_eq!(plain.opus_model, None);
        assert_eq!(plain.subagent_model, None);
        let slots = resolve_model_relay(environment(&[
            ("SPAWN_MODEL_RELAY", "http://127.0.0.1:8791"),
            ("SPAWN_CHEAP_MODEL", "deepseek-v4-pro"),
            ("SPAWN_SMALL_MODEL", "deepseek-flash"),
            ("SPAWN_DEFAULT_MODEL", "claude-opus-5-5"),
            ("SPAWN_SONNET_MODEL", "claude-sonnet-5-5"),
            ("SPAWN_OPUS_MODEL", ""),
            ("SPAWN_SUBAGENT_MODEL", "claude-haiku-5-5"),
        ]))
        .expect("a relay");
        assert_eq!(slots.cheap_model, "deepseek-v4-pro");
        assert_eq!(slots.default_model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(slots.sonnet_model.as_deref(), Some("claude-sonnet-5-5"));
        assert_eq!(slots.opus_model, None, "empty reads as unset");
        assert_eq!(slots.subagent_model.as_deref(), Some("claude-haiku-5-5"));
    }

    const FLEET: &str = "postgres://fleet:pw@%2Fhome%2Fo%2Ffleet-pg%2Fsock:5442/spawn_fleet_test";
    const GATES: &str = "postgres://gate:pw@127.0.0.1:5433/spawn_test";
    const LIVE: &str = "postgres://spawn:pw@127.0.0.1:5432/spawn";
    /// The gates' server, spelt another way.
    const GATES_SOCKET: &str = "postgres://gate:pw@%2Fvar%2Frun%2Fpostgresql:5433/spawn_test";
    /// The fleet's server while it is down (see [`FLEET_UP`]).
    const FLEET_LATER: &str = "postgres://fleet:pw@%2Ftmp%2Flater:5442/spawn_fleet_test";

    static FLEET_UP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    fn identity(url: &str) -> Result<String, String> {
        match url {
            FLEET => Ok("fleet".to_string()),
            FLEET_LATER if FLEET_UP.load(std::sync::atomic::Ordering::SeqCst) => {
                Ok("later".to_string())
            }
            FLEET_LATER => Err("connection refused".to_string()),
            GATES | GATES_SOCKET => Ok("gates".to_string()),
            LIVE => Ok("live".to_string()),
            _ => Err("no such server".to_string()),
        }
    }

    fn given(fleet: &FleetTestDatabase) -> Vec<String> {
        fleet
            .environment()
            .into_iter()
            .map(|(name, value)| {
                assert_eq!(name, "SPAWN_TEST_DATABASE_URL");
                value
            })
            .collect()
    }

    fn loop_env(fleet: &str, gates: &str) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = [
            ("SPAWN_FLEET_TEST_DATABASE_URL", fleet),
            ("SPAWN_TEST_DATABASE_URL", gates),
            ("SPAWN_DATABASE_URL", LIVE),
        ]
        .iter()
        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
        .collect();
        move |name| {
            pairs
                .iter()
                .find(|(known, _)| known == name)
                .map(|(_, value)| value.clone())
        }
    }

    #[test]
    fn engines_get_the_fleet_test_server_once_it_is_known_to_be_its_own() {
        let fleet = FleetTestDatabase::from_env(loop_env(FLEET, GATES), identity).expect("set");
        assert_eq!(given(&fleet), [FLEET]);
        assert_eq!(given(&fleet), [FLEET]);
        // Unset: engines get no test database at all, not the gates'.
        let unset = environment(&[("SPAWN_TEST_DATABASE_URL", GATES)]);
        assert!(FleetTestDatabase::from_env(unset, identity).is_none());
        // Never the URLs in what the loop may print.
        assert!(!format!("{fleet:?}").contains("pw"));
    }

    #[test]
    fn a_fleet_test_server_whose_identity_cannot_be_read_is_withheld_until_it_can() {
        let fleet =
            FleetTestDatabase::from_env(loop_env(FLEET_LATER, GATES), identity).expect("set");
        assert!(given(&fleet).is_empty());
        assert!(given(&fleet).is_empty());
        let reason = fleet.withheld().expect("withheld");
        assert!(reason.contains("no readable identity"), "{reason}");
        assert!(!reason.contains("pw"), "no password in {reason}");
        // Up later: the next launch gets it.
        FLEET_UP.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(given(&fleet), [FLEET_LATER]);
        // An unreadable other server withholds it too.
        let unknown = FleetTestDatabase::from_env(
            loop_env(FLEET, "postgres://gate:pw@%2Fgone:5433/spawn_test"),
            identity,
        )
        .expect("set");
        assert!(given(&unknown).is_empty());
        let reason = unknown.withheld().expect("withheld");
        assert!(reason.contains("the gates' shared test server"), "{reason}");
        // Without the live URL it cannot be told apart: withheld.
        let alone = environment(&[("SPAWN_FLEET_TEST_DATABASE_URL", FLEET)]);
        let alone = FleetTestDatabase::from_env(alone, identity).expect("set");
        assert!(given(&alone).is_empty());
    }

    #[test]
    fn a_fleet_test_server_that_is_the_gates_or_the_live_one_is_withheld() {
        for (fleet, gates, what) in [
            (GATES, GATES, "the gates' shared test server"),
            (GATES_SOCKET, GATES, "the gates' shared test server"),
            (LIVE, GATES, "the live database"),
        ] {
            let fleet = FleetTestDatabase::from_env(loop_env(fleet, gates), identity).expect("set");
            assert!(given(&fleet).is_empty());
            let reason = fleet.withheld().expect("withheld");
            assert!(reason.contains(what), "{reason}");
            assert!(!reason.contains("pw"), "no password in {reason}");
        }
    }

    /// Claude Code alone unless `SPAWN_ENGINES` names agy; agy's home,
    /// program and model from their own variables; an unknown engine is
    /// refused.
    #[test]
    fn engines_are_claude_alone_until_spawn_engines_names_agy() {
        let lookup = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(known, _)| *known == name)
                    .map(|(_, value)| (*value).to_string())
            }
        };
        assert!(
            engines(lookup(&[("HOME", "/home/a")]))
                .expect("default")
                .is_empty()
        );
        assert!(
            engines(lookup(&[("SPAWN_ENGINES", "claude")]))
                .expect("claude")
                .is_empty()
        );
        let agy = engines(lookup(&[
            ("SPAWN_ENGINES", "claude, agy,agy"),
            ("HOME", "/home/a/"),
        ]))
        .expect("agy");
        assert_eq!(agy.len(), 1);
        assert_eq!(agy[0].0, "agy");
        assert_eq!(agy[0].1.program, "agy");
        assert_eq!(agy[0].1.sessions, "/home/a/.gemini/antigravity-cli");
        assert_eq!(agy[0].1.model, None);
        let named = engines(lookup(&[
            ("SPAWN_ENGINES", "agy"),
            ("SPAWN_AGY_HOME", "/srv/agy"),
            ("SPAWN_AGY_PROGRAM", "/opt/agy/bin/agy"),
            ("SPAWN_AGY_MODEL", "gemini-3.8-flash-low"),
        ]))
        .expect("agy");
        assert_eq!(named[0].1.sessions, "/srv/agy");
        assert_eq!(named[0].1.program, "/opt/agy/bin/agy");
        assert_eq!(named[0].1.model.as_deref(), Some("gemini-3.8-flash-low"));
        let refused = engines(lookup(&[("SPAWN_ENGINES", "claude,codex")])).expect_err("codex");
        assert!(refused.contains("codex"), "{refused}");
    }
}
