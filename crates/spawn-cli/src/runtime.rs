//! Assembly of the Bridge and the Observer over one store and the host
//! adapters (git workspaces, tmux terminals, judge).

use std::sync::Arc;

use spawn_core::bridge::{Bridge, BridgeConfig, LaunchEnvironment, ModelRelay};
use spawn_core::observe::{Observer, StateJudge};
use spawn_core::port::{CommandQueue, EventLog, TerminalHost, WorkspaceStore};
use spawn_host::GitWorkspace;
use spawn_jev::{JevConfig, JevJudge};

use crate::Flags;
use crate::config::{env_or, env_value, open_store, required_env, spawn_program, workspace_root};
use crate::{context, sources, supervise};

/// Build the Bridge and the Observer over one store.
///
/// Both keep the event-log cursor in memory, so the resident loop builds
/// them once. Rebuilding them per step would replay the whole log on every
/// tick, and that cost grows with the log.
pub(crate) fn runtime(flags: &Flags) -> Result<(Bridge, Observer), String> {
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
    // mission owns use the default repository.
    let factory: sources::Factory = {
        let root = root.clone();
        Arc::new(move |repo: &std::path::Path| GitWorkspace::new(repo, &root))
    };
    let missions = sources::MissionWorkspaces::new(
        factory(std::path::Path::new(&repo)),
        std::path::Path::new(&root),
        store.clone(),
        factory,
    );
    // A push queued by the last process and lost with it is made now.
    if let Err(error) = missions.republish() {
        eprintln!("sources: {error}; a push lost with the last process waits for its next");
    }
    let workspaces: Arc<dyn WorkspaceStore> = Arc::new(missions);
    let terminals: Arc<dyn TerminalHost> = Arc::new(crate::config::terminal_host());

    let config = bridge_config()?;
    let log: Arc<dyn EventLog> = store.clone();
    let queue: Arc<dyn CommandQueue> = store.clone();
    // Beside the workspaces: the transcripts are the agents' history.
    let transcripts = spawn_host::LocalTranscriptArchive::new(
        std::path::Path::new(&root).join(spawn_host::TRANSCRIPT_DIRECTORY),
    );
    let mut bridge = Bridge::new(
        Arc::clone(&log),
        queue,
        workspaces,
        Arc::clone(&terminals),
        config,
    )
    .with_transcript_archive(Arc::new(transcripts));
    bridge.recover().map_err(|error| error.to_string())?;

    let context = context::HostContext::new(open_store()?, terminals);
    let mut observer = Observer::new(log).with_context(Arc::new(context));
    if let Some(judge) = judge(flags) {
        observer = observer.with_judge(judge);
    }
    observer.recover().map_err(|error| error.to_string())?;
    Ok((bridge, observer))
}

/// Programs and environment the Bridge puts into every terminal it starts.
fn bridge_config() -> Result<BridgeConfig, String> {
    let mut config = BridgeConfig::new(&env_or("SPAWN_WORKER", "spawn-worker"));
    config.spawn_program = spawn_program();
    config.claude_program = env_or("SPAWN_CLAUDE_PROGRAM", "claude");
    config.claude_projects = claude_projects();
    config.model_relay = model_relay();
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
/// model `SPAWN_SMALL_MODEL`. Names only: the relay holds the keys.
///
/// Opt-in, so a host or test without a relay never probes one, and a
/// configured relay that is down is a warning worth reading.
fn model_relay() -> Option<ModelRelay> {
    let url = env_value("SPAWN_MODEL_RELAY").filter(|url| url != "off")?;
    Some(ModelRelay {
        url: url.trim_end_matches('/').to_string(),
        cheap_model: env_or("SPAWN_CHEAP_MODEL", "deepseek-flash"),
        small_model: env_or("SPAWN_SMALL_MODEL", "deepseek-flash"),
    })
}

/// Claude Code's transcript directory, which is where the Bridge reads it.
fn claude_projects() -> String {
    resolve_claude_projects(env_value)
}

/// [`claude_projects`] over the environment `lookup` reads:
/// `SPAWN_CLAUDE_PROJECTS` always wins; else `$CLAUDE_CONFIG_DIR/projects`,
/// else `$HOME/.claude/projects`.
fn resolve_claude_projects(lookup: impl Fn(&str) -> Option<String>) -> String {
    if let Some(projects) = lookup("SPAWN_CLAUDE_PROJECTS") {
        return projects;
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
    use super::{FleetTestDatabase, resolve_claude_projects};
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
    fn an_explicit_transcript_directory_wins() {
        let explicit = environment(&[
            ("HOME", "/home/owner"),
            ("CLAUDE_CONFIG_DIR", "/cc"),
            ("SPAWN_CLAUDE_PROJECTS", "/mnt/transcripts"),
        ]);
        assert_eq!(resolve_claude_projects(&explicit), "/mnt/transcripts");
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
}
