//! Assembly of the Bridge and the Observer over one store and the host
//! adapters (git workspaces, Herdr terminals, judge).

use std::path::{Component, Path, PathBuf};
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
    let mut host = crate::config::terminal_host()?;
    let herdr_backend = true;
    let mut config = bridge_config_for(herdr_backend, std::path::Path::new(&root))?;
    let judge_url = resolve_judge_url(flags, env_value);
    let preflight = preflight_model_endpoints_with(
        config.model_relay.take(),
        judge_url.as_deref(),
        &env_value,
        &spawn_jev::probe_http_endpoint,
    )?;
    for warning in &preflight.warnings {
        eprintln!("{warning}");
    }
    config.model_relay = preflight.model_relay;
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
    for (_, home) in &config.engines {
        for path in home.transcript_roots(None) {
            if !roots.contains(&path) {
                roots.push(path);
            }
        }
    }
    let (archive_mask, stand_in) = spawn_host::archive_mask::CollectorMask::from_env(&roots);
    if let Some(stand_in) = &stand_in {
        eprintln!("{}", stand_in.said());
    }
    if let Err(why) = archive_mask.preflight() {
        if crate::config::env_value("SPAWN_ARCHIVE_MASK_STRICT").as_deref() == Some("1") {
            return Err(format!("archive mask preflight failed: {why}"));
        }
        eprintln!("spawn loop WARNING: archive mask preflight: {why}");
    }
    let pinned = spawn_host::archive_mask::loop_roots(
        stand_in.as_ref(),
        &roots[0],
        &roots[1],
        &spawn_host::home::Home::of_account(),
    )?;
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

/// Whether the Bridge answers an engine's startup trust dialog itself
/// under `mode` (`SPAWN_AUTO_APPROVE`): under every mode but `off`.
fn auto_trust(mode: crate::resolve::Mode) -> bool {
    mode != crate::resolve::Mode::Off
}

fn parse_bounded_usize(
    lookup: &dyn Fn(&str) -> Option<String>,
    var: &str,
    default: usize,
    min: usize,
    max: usize,
) -> Result<usize, String> {
    let Some(raw) = lookup(var).filter(|text| !text.trim().is_empty()) else {
        return Ok(default);
    };
    let trimmed = raw.trim();
    match trimmed.parse::<usize>() {
        Ok(value) if (min..=max).contains(&value) => Ok(value),
        _ => Err(format!(
            "{var}={trimmed:?} must be a whole number from {min} to {max}"
        )),
    }
}

/// Build `BwrapView` config, enabling v2 sandbox features (`canonical_workspace`,
/// whitelist `$HOME`, per-worker `brain/`) strictly on the Herdr backend, and
/// mounting an opt-in shared host build cache directory read-write when enabled.
pub(crate) fn view_config_for_backend(
    lookup: &dyn Fn(&str) -> Option<String>,
    herdr_backend: bool,
) -> spawn_host::ViewConfig {
    let ws_root = lookup("SPAWN_WORKSPACE_ROOT")
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty())
        .map(PathBuf::from);
    let shared_cache = resolve_shared_build_cache(lookup, ws_root.as_deref())
        .ok()
        .flatten();
    view_config_for_backend_with_cache(lookup, herdr_backend, shared_cache.as_deref())
}

/// Like [`view_config_for_backend`], binding `shared_cache_root` read-write
/// when `Some`.
pub(crate) fn view_config_for_backend_with_cache(
    lookup: &dyn Fn(&str) -> Option<String>,
    herdr_backend: bool,
    shared_cache_root: Option<&Path>,
) -> spawn_host::ViewConfig {
    let mut cfg = spawn_host::ViewConfig::from_env(lookup);
    if !herdr_backend {
        cfg.canonical_workspace = None;
        cfg.home_isolation = spawn_host::HomeIsolation::Blacklist;
        cfg.isolate_brain = false;
    }
    if let Some(cache_root) = shared_cache_root {
        apply_shared_build_cache_mount(&mut cfg, cache_root);
    }
    cfg
}

/// Programs and environment the Bridge puts into every terminal it starts.
fn bridge_config_for(herdr_backend: bool, workspace_root: &Path) -> Result<BridgeConfig, String> {
    let mut config = BridgeConfig::new(&env_or("SPAWN_WORKER", "spawn-worker"));
    config.spawn_program = spawn_program();
    config.claude_program = env_or("SPAWN_CLAUDE_PROGRAM", "claude");
    config.claude_projects = claude_projects();
    config.engines = engines_for(env_value, herdr_backend)?;
    config.max_commands_per_tick =
        parse_bounded_usize(&env_value, "SPAWN_BRIDGE_MAX_COMMANDS", 16, 1, 512)?;
    config.ready_attempts =
        parse_bounded_usize(&env_value, "SPAWN_BRIDGE_READY_ATTEMPTS", 40, 1, 240)?;
    config.max_concurrent_mounts =
        parse_bounded_usize(&env_value, "SPAWN_MAX_CONCURRENT_MOUNTS", 8, 1, 128)?;
    config.max_concurrent_checkpoints =
        parse_bounded_usize(&env_value, "SPAWN_MAX_CONCURRENT_CHECKPOINTS", 8, 1, 128)?;
    config.backstops_per_pass =
        parse_bounded_usize(&env_value, "SPAWN_BRIDGE_BACKSTOPS_PER_PASS", 2, 1, 64)?;
    config.session_scan_limit =
        parse_bounded_usize(&env_value, "SPAWN_LIST_DIRS_LIMIT", 64, 64, 4096)?;
    let shared_cache = prepare_shared_build_caches(&env_value, workspace_root)?;
    config.claude_sandbox = claude_sandbox_enabled(&env_value);
    let (policy, said) = bridge_engine_view(
        &env_value,
        &config.engines,
        herdr_backend,
        shared_cache.root.as_deref(),
    );
    if let Some(said) = said {
        eprintln!("{said}");
    }
    config.engine_view = policy;
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
    config.environment.extend(path_env(env_value));
    config.environment.extend(supply_pa_env(env_value));
    // Opt-in shared host toolchain build caches (issue #105): deterministic
    // non-secret subdirectories under the cache root with caller overrides preserved.
    config.environment.extend(shared_cache.env);
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

/// Zero-secret toolchain build cache env variables mapped to subdirectories under
/// the shared host cache root (`#105`).
pub(crate) const SHARED_BUILD_CACHE_VARS: &[(&str, &str)] = &[
    ("ZIG_GLOBAL_CACHE_DIR", "zig"),
    ("CCACHE_DIR", "ccache"),
    ("UV_CACHE_DIR", "uv"),
    ("SCCACHE_DIR", "sccache"),
    ("PIP_CACHE_DIR", "pip"),
    ("NPM_CONFIG_CACHE", "npm"),
    ("GOMODCACHE", "go-mod"),
    ("GOCACHE", "go-build"),
];

const SHARED_CACHE_BARE_SYSTEM_DIRS: &[&str] = &[
    "/",
    "/bin",
    "/boot",
    "/dev",
    "/etc",
    "/home",
    "/lib",
    "/lib64",
    "/media",
    "/mnt",
    "/opt",
    "/proc",
    "/root",
    "/run",
    "/sbin",
    "/srv",
    "/sys",
    "/tmp",
    "/usr",
    "/usr/local",
    "/var",
    "/var/cache",
    "/var/lib",
    "/var/run",
    "/var/tmp",
];

const SHARED_CACHE_SENSITIVE_SUBTREES: &[&str] = &[
    "/etc",
    "/proc",
    "/sys",
    "/dev",
    "/boot",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/usr",
    "/run/keep",
    "/run/keep-test",
    "/var/run/keep",
    "/var/run/keep-test",
];

fn bridge_engine_view(
    lookup: &dyn Fn(&str) -> Option<String>,
    engines: &[(String, spawn_core::driver::EngineHome)],
    herdr_backend: bool,
    shared_cache_mount: Option<&Path>,
) -> (spawn_core::bridge::EngineViewPolicy, Option<String>) {
    engine_view_with(
        lookup,
        engines,
        |l| {
            spawn_host::BwrapView::new(
                "bwrap",
                view_config_for_backend_with_cache(l, herdr_backend, shared_cache_mount),
            )
            .map(|view| Arc::new(view) as Arc<dyn spawn_core::port::EngineView>)
        },
        |l| {
            spawn_host::ContainerView::new(view_config_for_backend_with_cache(
                l,
                herdr_backend,
                shared_cache_mount,
            ))
            .map(|view| Arc::new(view) as Arc<dyn spawn_core::port::EngineView>)
        },
    )
}

fn normalize_cache_path(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        if !matches!(component, Component::CurDir) {
            normal.push(component.as_os_str());
        }
    }
    if normal.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normal
    }
}

fn reject_forbidden_cache_path(
    path: &Path,
    var_name: &str,
    home: Option<&str>,
) -> Result<(), String> {
    let text = path.to_string_lossy();
    if SHARED_CACHE_BARE_SYSTEM_DIRS.contains(&text.as_ref()) {
        return Err(format!(
            "{var_name} cannot target bare system directory {text:?}"
        ));
    }
    for prefix in SHARED_CACHE_SENSITIVE_SUBTREES {
        let prefix_path = Path::new(prefix);
        if path == prefix_path || path.starts_with(prefix_path) {
            return Err(format!(
                "{var_name} cannot target sensitive system directory {text:?}"
            ));
        }
    }
    if let Some(home_raw) = home.map(str::trim).filter(|raw| !raw.is_empty()) {
        let home_path = Path::new(home_raw);
        if home_path.is_absolute() {
            let home_norm = normalize_cache_path(home_path);
            if path == home_norm
                || std::fs::canonicalize(&home_norm)
                    .ok()
                    .is_some_and(|canon| path == canon)
            {
                return Err(format!(
                    "{var_name} cannot target $HOME directly ({text:?}); use a subdirectory"
                ));
            }
        }
    }
    Ok(())
}

fn validate_shared_build_cache_dir(
    raw: &str,
    var_name: &str,
    home: Option<&str>,
) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.chars().any(char::is_control) {
        return Err(format!(
            "{var_name} must be a non-empty clean path, not {raw:?}"
        ));
    }
    let path = Path::new(trimmed);
    if !path.is_absolute() {
        return Err(format!(
            "{var_name} must be an absolute path, not {trimmed:?}"
        ));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!(
            "{var_name} must not contain '..' segments, got {trimmed:?}"
        ));
    }
    let normalized = normalize_cache_path(path);
    reject_forbidden_cache_path(&normalized, var_name, home)?;
    if let Ok(canonical) = std::fs::canonicalize(&normalized) {
        reject_forbidden_cache_path(&canonical, var_name, home)?;
    }
    Ok(normalized)
}

fn parse_shared_cache_toggle(raw: Option<&str>) -> Result<Option<bool>, String> {
    let Some(trimmed) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    match trimmed.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(Some(true)),
        "0" | "false" | "no" | "off" => Ok(Some(false)),
        _ => Err(format!(
            "SPAWN_SHARED_BUILD_CACHES={trimmed:?} must be 1|true|yes|on or 0|false|no|off"
        )),
    }
}

/// Resolve opt-in shared host build cache directory (`SPAWN_SHARED_BUILD_CACHES`
/// / `SPAWN_SHARED_BUILD_CACHE_DIR`), returning `Ok(None)` when disabled by default
/// or explicitly turned off, or validating paths when enabled (`#105`).
pub(crate) fn resolve_shared_build_cache(
    lookup: &dyn Fn(&str) -> Option<String>,
    workspace_root: Option<&Path>,
) -> Result<Option<PathBuf>, String> {
    let raw_toggle = lookup("SPAWN_SHARED_BUILD_CACHES");
    let toggle = parse_shared_cache_toggle(raw_toggle.as_deref())?;
    if toggle == Some(false) {
        return Ok(None);
    }
    let home = lookup("HOME");
    if let Some(dir) = lookup("SPAWN_SHARED_BUILD_CACHE_DIR")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return validate_shared_build_cache_dir(
            &dir,
            "SPAWN_SHARED_BUILD_CACHE_DIR",
            home.as_deref(),
        )
        .map(Some);
    }
    if toggle == Some(true) {
        let Some(ws) = workspace_root else {
            return Err(
                "SPAWN_SHARED_BUILD_CACHES=1 requires a workspace root (set --root / SPAWN_WORKSPACE_ROOT or SPAWN_SHARED_BUILD_CACHE_DIR)"
                    .to_string(),
            );
        };
        if !ws.is_absolute()
            || ws
                .components()
                .any(|component| matches!(component, Component::ParentDir))
            || normalize_cache_path(ws) == Path::new("/")
        {
            return Err(format!(
                "SPAWN_SHARED_BUILD_CACHES requires a valid non-root workspace path, not {:?}",
                ws.display()
            ));
        }
        let default_dir = normalize_cache_path(ws).join(".caches");
        return validate_shared_build_cache_dir(
            &default_dir.display().to_string(),
            "SPAWN_SHARED_BUILD_CACHES",
            home.as_deref(),
        )
        .map(Some);
    }
    Ok(None)
}

/// Compute the 8 zero-secret build cache environment variables under `cache_root`,
/// preserving any non-empty caller-supplied single-tool override.
pub(crate) fn shared_build_cache_env(
    lookup: &dyn Fn(&str) -> Option<String>,
    cache_root: &Path,
) -> Vec<(String, String)> {
    let mut entries = Vec::with_capacity(SHARED_BUILD_CACHE_VARS.len());
    for (var, subdir) in SHARED_BUILD_CACHE_VARS {
        let value = lookup(var)
            .filter(|raw| !raw.trim().is_empty())
            .unwrap_or_else(|| cache_root.join(subdir).display().to_string());
        entries.push(((*var).to_string(), value));
    }
    entries
}

fn ensure_shared_build_cache_dirs(cache_root: &Path) -> Result<(), String> {
    std::fs::create_dir_all(cache_root).map_err(|error| {
        format!(
            "create shared build cache directory {}: {error}",
            cache_root.display()
        )
    })?;
    for (_, subdir) in SHARED_BUILD_CACHE_VARS {
        let dir = cache_root.join(subdir);
        std::fs::create_dir_all(&dir).map_err(|error| {
            format!(
                "create shared build cache subdirectory {}: {error}",
                dir.display()
            )
        })?;
    }
    Ok(())
}

fn apply_shared_build_cache_mount(cfg: &mut spawn_host::ViewConfig, cache_root: &Path) {
    let target_norm = normalize_cache_path(cache_root);
    let target_canon = std::fs::canonicalize(&target_norm).ok();
    if let Some(existing) = cfg.extra_mounts.iter_mut().find(|mount| {
        let mount_norm = normalize_cache_path(&mount.path);
        mount_norm == target_norm
            || target_canon.as_ref().is_some_and(|canon| {
                std::fs::canonicalize(&mount_norm).ok().as_ref() == Some(canon)
            })
    }) {
        existing.writable = true;
        return;
    }
    cfg.extra_mounts.push(spawn_host::ExtraMount {
        path: target_norm,
        writable: true,
    });
}

struct SharedBuildCachePlan {
    root: Option<PathBuf>,
    env: Vec<(String, String)>,
}

fn prepare_shared_build_caches(
    lookup: &dyn Fn(&str) -> Option<String>,
    workspace_root: &Path,
) -> Result<SharedBuildCachePlan, String> {
    let Some(cache_root) = resolve_shared_build_cache(lookup, Some(workspace_root))? else {
        return Ok(SharedBuildCachePlan {
            root: None,
            env: Vec::new(),
        });
    };
    ensure_shared_build_cache_dirs(&cache_root)?;
    let env = shared_build_cache_env(lookup, &cache_root);
    Ok(SharedBuildCachePlan {
        root: Some(cache_root),
        env,
    })
}

/// Forward a non-empty environment variable verbatim to engine panes.
fn passthrough_env(lookup: impl Fn(&str) -> Option<String>, name: &str) -> Vec<(String, String)> {
    lookup(name)
        .filter(|value| !value.is_empty())
        .map(|value| vec![(name.to_string(), value)])
        .unwrap_or_default()
}

/// The caller's `PATH` so engines keep toolchain and scenario wrapper directories
/// even when the terminal daemon spawns clean or login shells.
fn path_env(lookup: impl Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    passthrough_env(lookup, "PATH")
}

/// The deps store's path for engines that build a tree's dependencies
/// (SUPPLY_PA_DEPS, `gate/scenarios/build`): a path, never a login, and only
/// what the integrator has.
fn supply_pa_env(lookup: impl Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    passthrough_env(lookup, "SUPPLY_PA_DEPS")
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

/// Returns `true` when an endpoint configuration value is intentionally disabled
/// (empty/whitespace, `"off"`, or `"stub"` case-insensitively).
fn is_disabled_endpoint(raw: &str) -> bool {
    let trimmed = raw.trim();
    trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("off")
        || trimmed.eq_ignore_ascii_case("stub")
}

/// Startup preflight mode controlled by `SPAWN_MODEL_PREFLIGHT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelPreflightMode {
    /// Log actionable warnings and apply safe degradation when unreachable (default).
    Warn,
    /// Abort startup immediately when any configured endpoint fails health preflight.
    Strict,
    /// Skip startup health probes altogether.
    Off,
}

impl ModelPreflightMode {
    fn parse(raw: Option<&str>) -> (Self, Option<String>) {
        let trimmed = raw.map(str::trim).filter(|value| !value.is_empty());
        match trimmed.map(str::to_ascii_lowercase).as_deref() {
            None | Some("warn") => (Self::Warn, None),
            Some("strict") => (Self::Strict, None),
            Some("off") => (Self::Off, None),
            Some(_) => (
                Self::Warn,
                Some(format!(
                    "spawn: SPAWN_MODEL_PREFLIGHT={} is neither warn, strict, nor off: read as warn",
                    trimmed.unwrap_or_default()
                )),
            ),
        }
    }
}

/// Whether `SPAWN_CHEAP_MODEL` should safely fall back to the default/direct path when
/// `SPAWN_MODEL_RELAY` fails startup health preflight (`warn` mode).
fn cheap_fallback_enabled(lookup: &dyn Fn(&str) -> Option<String>) -> bool {
    !matches!(
        lookup("SPAWN_CHEAP_FALLBACK")
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("off" | "0" | "false" | "no" | "keep-relay")
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelPreflightOutcome {
    model_relay: Option<ModelRelay>,
    cheap_fell_back_to_default: bool,
    fallback_target_model: Option<String>,
    warnings: Vec<String>,
}

/// [`model_relay`] over the environment `lookup` reads (an empty value,
/// `"off"`, or `"stub"` reads as unset).
fn resolve_model_relay(lookup: impl Fn(&str) -> Option<String>) -> Option<ModelRelay> {
    let value = |name: &str| {
        lookup(name)
            .map(|raw| raw.trim().to_string())
            .filter(|trimmed| !trimmed.is_empty())
    };
    let url = value("SPAWN_MODEL_RELAY").filter(|url| !is_disabled_endpoint(url))?;
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
pub(crate) fn engines(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Vec<(String, spawn_core::driver::EngineHome)>, String> {
    engines_for(lookup, true)
}

fn engines_for(
    lookup: impl Fn(&str) -> Option<String>,
    herdr_backend: bool,
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
        let data_dir = match lookup("SPAWN_AGY_DATA_DIR") {
            Some(dir) if !dir.trim().is_empty() => {
                let trimmed = dir.trim();
                if !trimmed.starts_with('/') {
                    return Err(format!(
                        "SPAWN_AGY_DATA_DIR must be an absolute path, not {dir:?}"
                    ));
                }
                let cleaned = trimmed.trim_end_matches('/');
                Some(if cleaned.is_empty() {
                    trimmed.to_string()
                } else {
                    cleaned.to_string()
                })
            }
            _ => None,
        };
        let isolate_brain = herdr_backend
            && data_dir.is_some()
            && matches!(
                lookup("SPAWN_AGY_ISOLATE_BRAIN")
                    .as_deref()
                    .map(str::trim)
                    .map(str::to_ascii_lowercase)
                    .as_deref(),
                Some("1" | "true" | "yes" | "on")
            );
        engines.push((
            name.to_string(),
            spawn_core::driver::EngineHome {
                program: lookup("SPAWN_AGY_PROGRAM").unwrap_or_else(|| "agy".to_string()),
                sessions: home,
                model: lookup("SPAWN_AGY_MODEL"),
                data_dir,
                isolate_brain,
            },
        ));
    }
    Ok(engines)
}

/// The filesystem view policy (`SPAWN_ENGINE_VIEW`) for the engines
/// `enabled`, and one line to say, if any.
///
/// - `required` (also unset or empty, the default): an engine with no deny
///   rules of its own ([`spawn_core::driver::Guard::Sandbox`], agy) runs
///   only inside the view `make` builds (`bwrap`, checked to work on this
///   host). When it cannot be built, such an engine is refused
///   (`engine_unavailable`) and Claude Code runs on unaffected. The view
///   is built only when such an engine is enabled: a Claude-only host
///   (the default) never runs `bwrap`.
/// - `off`: the owner's explicit choice, such an engine runs with no view,
///   as before N6; said once.
/// - Anything else: read as `required` (the safe side), said once, as an
///   invalid `SPAWN_AUTO_APPROVE` is read as its default.
pub(crate) fn engine_view(
    lookup: &dyn Fn(&str) -> Option<String>,
    enabled: &[(String, spawn_core::driver::EngineHome)],
    make: impl FnOnce(
        &dyn Fn(&str) -> Option<String>,
    ) -> Result<Arc<dyn spawn_core::port::EngineView>, String>,
) -> (spawn_core::bridge::EngineViewPolicy, Option<String>) {
    engine_view_with(lookup, enabled, make, |l| {
        spawn_host::ContainerView::new(spawn_host::ViewConfig::from_env(l))
            .map(|view| Arc::new(view) as Arc<dyn spawn_core::port::EngineView>)
    })
}

/// Whether Claude Code runs inside the host filesystem view (`SPAWN_CLAUDE_SANDBOX=1`).
pub(crate) fn claude_sandbox_enabled(lookup: &dyn Fn(&str) -> Option<String>) -> bool {
    matches!(
        lookup("SPAWN_CLAUDE_SANDBOX")
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

pub(crate) fn engine_view_with(
    lookup: &dyn Fn(&str) -> Option<String>,
    enabled: &[(String, spawn_core::driver::EngineHome)],
    make: impl FnOnce(
        &dyn Fn(&str) -> Option<String>,
    ) -> Result<Arc<dyn spawn_core::port::EngineView>, String>,
    make_container: impl FnOnce(
        &dyn Fn(&str) -> Option<String>,
    ) -> Result<Arc<dyn spawn_core::port::EngineView>, String>,
) -> (spawn_core::bridge::EngineViewPolicy, Option<String>) {
    use spawn_core::bridge::EngineViewPolicy;
    let claude_sandbox = claude_sandbox_enabled(lookup);
    let value = lookup("SPAWN_ENGINE_VIEW");
    let trimmed = value.as_deref().map(str::trim);
    let container_mode = trimmed == Some("container");
    let mut said = match trimmed {
        None | Some("" | "required" | "container") => None,
        Some("off") => {
            return (
                EngineViewPolicy::Off,
                Some(
                    "spawn: SPAWN_ENGINE_VIEW=off: engines with no deny rules of their own run \
                     with no filesystem view, and can read whatever this user can"
                        .to_string(),
                ),
            );
        }
        Some(other) => Some(format!(
            "spawn: SPAWN_ENGINE_VIEW={other} is neither required, container, nor off: read as required"
        )),
    };
    let mut sandboxed: Vec<&str> = enabled
        .iter()
        .filter(|(id, _)| {
            spawn_core::driver::driver(id)
                .is_some_and(|driver| driver.traits().guard == spawn_core::driver::Guard::Sandbox)
        })
        .map(|(id, _)| id.as_str())
        .collect();
    if claude_sandbox && !sandboxed.contains(&spawn_core::driver::DEFAULT) {
        sandboxed.push(spawn_core::driver::DEFAULT);
    }
    if sandboxed.is_empty() {
        return (
            EngineViewPolicy::Required(Err(
                "no enabled engine needs one, so none was made".to_string()
            )),
            said,
        );
    }
    let view = match make(lookup) {
        Ok(view) => Ok(view),
        Err(why) if container_mode && spawn_host::container::is_container_namespace_block(&why) => {
            match make_container(lookup) {
                Ok(container_view) => {
                    let note = format!(
                        "spawn: bwrap blocked by outer container ({why}); falling back to \
                         container view (no inner mount isolation) for {}",
                        sandboxed.join(", ")
                    );
                    said = Some(said.map_or(note.clone(), |before| format!("{before}\n{note}")));
                    Ok(container_view)
                }
                Err(container_err) => Err(format!("{why}; {container_err}")),
            }
        }
        Err(why) => Err(why),
    };
    if let Err(why) = &view {
        let suffix = if sandboxed.contains(&spawn_core::driver::DEFAULT) {
            ""
        } else {
            "; Claude Code is unaffected"
        };
        let line = format!(
            "spawn: no filesystem view on this host ({why}): {} will not start \
             (engine_unavailable){suffix}",
            sandboxed.join(", ")
        );
        said = Some(said.map_or(line.clone(), |before| format!("{before}\n{line}")));
    }
    (EngineViewPolicy::Required(view), said)
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

/// Resolve the configured judge endpoint URL from `--judge-url` or
/// `SPAWN_JUDGE_URL`, returning `None` when unset, empty, `"off"`, or `"stub"`.
fn resolve_judge_url(flags: &Flags, lookup: impl Fn(&str) -> Option<String>) -> Option<String> {
    flags
        .get("judge-url")
        .or_else(|| lookup("SPAWN_JUDGE_URL"))
        .map(|raw| raw.trim().to_string())
        .filter(|url| !is_disabled_endpoint(url))
}

/// Probe explicitly configured `model_relay` and `judge` endpoints at startup.
///
/// Honors `SPAWN_MODEL_PREFLIGHT=warn|strict|off` (default `warn`), logs/returns
/// only sanitized `host:port` or `unix://<socket>` targets without credentials
/// or query strings, and safely clears an unreachable `model_relay` so
/// `SPAWN_CHEAP_MODEL` falls back to the main/direct model path instead of
/// wedging sessions on startup.
fn preflight_model_endpoints_with(
    relay: Option<ModelRelay>,
    judge_url: Option<&str>,
    lookup: &dyn Fn(&str) -> Option<String>,
    probe: &dyn Fn(&str, std::time::Duration) -> spawn_jev::EndpointHealth,
) -> Result<ModelPreflightOutcome, String> {
    let raw_mode = lookup("SPAWN_MODEL_PREFLIGHT");
    let (mode, mode_notice) = ModelPreflightMode::parse(raw_mode.as_deref());
    let mut warnings = Vec::new();
    if let Some(notice) = mode_notice {
        warnings.push(notice);
    }

    if mode == ModelPreflightMode::Off {
        return Ok(ModelPreflightOutcome {
            model_relay: relay,
            cheap_fell_back_to_default: false,
            fallback_target_model: None,
            warnings,
        });
    }

    let mut effective_relay = relay;
    let mut cheap_fell_back_to_default = false;
    let mut fallback_target_model = None;
    let mut strict_errors = Vec::new();

    if let Some(relay_cfg) = effective_relay.clone() {
        let health = probe(&relay_cfg.url, spawn_jev::DEFAULT_PROBE_TIMEOUT);
        if !health.status.is_ok() {
            let detail = health.detail.as_deref().unwrap_or("endpoint unreachable");
            let msg = format!(
                "spawn preflight: slot=model_relay target={} status={} latency_ms={} detail=\"{}\" \
                 hint=\"verify spawn-model-relay is running (or set SPAWN_MODEL_RELAY=off / SPAWN_MODEL_PREFLIGHT=off)\"",
                health.target,
                health.status.as_str(),
                health.latency_ms,
                detail,
            );
            match mode {
                ModelPreflightMode::Strict => strict_errors.push(msg),
                ModelPreflightMode::Warn => {
                    warnings.push(msg);
                    if cheap_fallback_enabled(lookup) {
                        let fallback_display = relay_cfg
                            .default_model
                            .clone()
                            .unwrap_or_else(|| "claude-default".to_string());
                        fallback_target_model.clone_from(&relay_cfg.default_model);
                        cheap_fell_back_to_default = true;
                        effective_relay = None;
                        warnings.push(format!(
                            "spawn preflight warning: slot=model_relay action=cheap_fallback_to_default \
                             cheap_model={} fallback_model={} target={} \
                             hint=\"unreachable model relay cleared at startup so sessions fall back to the direct/default path (set SPAWN_CHEAP_FALLBACK=off to keep relay or SPAWN_MODEL_PREFLIGHT=strict to abort)\"",
                            relay_cfg.cheap_model,
                            fallback_display,
                            health.target,
                        ));
                    }
                }
                ModelPreflightMode::Off => {}
            }
        }
    }

    if let Some(url) = judge_url
        .map(str::trim)
        .filter(|u| !is_disabled_endpoint(u))
    {
        let health = probe(url, spawn_jev::DEFAULT_PROBE_TIMEOUT);
        if !health.status.is_ok() {
            let detail = health.detail.as_deref().unwrap_or("endpoint unreachable");
            let msg = format!(
                "spawn preflight: slot=judge target={} status={} latency_ms={} detail=\"{}\" \
                 hint=\"verify spawn-jev-relay is running (or set SPAWN_JUDGE_URL=off to rely on deterministic Observer rules)\"",
                health.target,
                health.status.as_str(),
                health.latency_ms,
                detail,
            );
            match mode {
                ModelPreflightMode::Strict => strict_errors.push(msg),
                ModelPreflightMode::Warn => warnings.push(msg),
                ModelPreflightMode::Off => {}
            }
        }
    }

    if !strict_errors.is_empty() {
        return Err(format!(
            "model endpoint preflight failed (SPAWN_MODEL_PREFLIGHT=strict):\n{}",
            strict_errors.join("\n")
        ));
    }

    Ok(ModelPreflightOutcome {
        model_relay: effective_relay,
        cheap_fell_back_to_default,
        fallback_target_model,
        warnings,
    })
}

/// The model judge at `--judge-url` or `SPAWN_JUDGE_URL`. Without one the
/// Observer judges by the rules alone, and a case they cannot settle asks
/// for attention.
fn judge(flags: &Flags) -> Option<Arc<dyn StateJudge>> {
    let url = resolve_judge_url(flags, env_value)?;
    let mut config = JevConfig::new(&url);
    if let Some(model) = env_value("SPAWN_JUDGE_MODEL").filter(|m| !m.trim().is_empty()) {
        config.model = model;
    }
    Some(Arc::new(JevJudge::new(config)))
}

#[cfg(test)]
mod tests {
    use super::{
        FleetTestDatabase, auto_trust, claude_config_env, engine_view, engines, engines_for,
        path_env, resolve_claude_projects, resolve_model_relay, supply_pa_env,
    };
    use std::sync::Arc;

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
        assert!(
            spawn_core::bridge::BridgeConfig::new("w").auto_trust,
            "on by default"
        );
    }
    use spawn_core::bridge::LaunchEnvironment;

    fn environment(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
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
        assert_eq!(
            path_env(environment(&[("PATH", "/work/bin:/usr/bin")])),
            [("PATH".to_string(), "/work/bin:/usr/bin".to_string())]
        );
        assert!(path_env(environment(&[("PATH", "")])).is_empty());
        assert!(path_env(environment(&[])).is_empty());
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

    #[derive(Debug)]
    struct NoView;

    impl spawn_core::port::EngineView for NoView {
        fn name(&self) -> &'static str {
            "bwrap"
        }

        fn wrap(
            &self,
            _launch: &spawn_core::port::ViewedLaunch<'_>,
        ) -> Result<Vec<String>, String> {
            Ok(vec!["bwrap".to_string(), "--".to_string()])
        }
    }

    /// SPAWN_ENGINE_VIEW: required by default (and for an invalid value,
    /// said), off only when named; the view is made only when an enabled
    /// engine has no deny rules of its own, and a host that cannot make
    /// one refuses that engine, saying so.
    #[test]
    fn the_engine_view_is_required_by_default_and_made_only_for_sandboxed_engines() {
        use spawn_core::bridge::EngineViewPolicy;
        let claude = engines(environment(&[("HOME", "/home/a")])).expect("claude");
        let agy = engines(environment(&[
            ("HOME", "/home/a"),
            ("SPAWN_ENGINES", "claude,agy"),
        ]))
        .expect("agy");
        let made = std::cell::Cell::new(0);
        let works = |_: &dyn Fn(&str) -> Option<String>| {
            made.set(made.get() + 1);
            Ok(Arc::new(NoView) as Arc<dyn spawn_core::port::EngineView>)
        };
        let fails = |_: &dyn Fn(&str) -> Option<String>| -> Result<
            Arc<dyn spawn_core::port::EngineView>,
            String,
        > { Err("bwrap is not installed (not found on PATH)".to_string()) };

        let (policy, said) = engine_view(&environment(&[]), &claude, works);
        assert!(
            matches!(policy, EngineViewPolicy::Required(Err(_))),
            "{policy:?}"
        );
        assert_eq!(said, None);
        assert_eq!(made.get(), 0, "a Claude-only host never runs bwrap");

        let (policy, said) = engine_view(&environment(&[]), &agy, works);
        assert!(
            matches!(policy, EngineViewPolicy::Required(Ok(_))),
            "{policy:?}"
        );
        assert_eq!(said, None);
        assert_eq!(made.get(), 1);

        let (policy, said) = engine_view(
            &environment(&[("SPAWN_ENGINE_VIEW", "required")]),
            &agy,
            fails,
        );
        assert!(
            matches!(&policy, EngineViewPolicy::Required(Err(why)) if why.contains("bwrap")),
            "{policy:?}"
        );
        let said = said.expect("said");
        assert!(
            said.contains("agy will not start (engine_unavailable)"),
            "{said}"
        );

        let (policy, said) =
            engine_view(&environment(&[("SPAWN_ENGINE_VIEW", "off")]), &agy, fails);
        assert!(matches!(policy, EngineViewPolicy::Off), "{policy:?}");
        assert!(said.expect("said").contains("SPAWN_ENGINE_VIEW=off"));

        let (policy, said) =
            engine_view(&environment(&[("SPAWN_ENGINE_VIEW", "maybe")]), &agy, fails);
        assert!(
            matches!(policy, EngineViewPolicy::Required(Err(_))),
            "{policy:?}"
        );
        assert!(
            said.expect("said")
                .contains("SPAWN_ENGINE_VIEW=maybe is neither")
        );
    }

    #[test]
    fn claude_sandbox_enables_engine_view_for_claude_only_host() {
        use super::engine_view_with;
        use spawn_core::bridge::EngineViewPolicy;
        let claude_only = engines(environment(&[("HOME", "/home/a")])).expect("claude only");
        let made = std::cell::Cell::new(0);
        let works = |_: &dyn Fn(&str) -> Option<String>| {
            made.set(made.get() + 1);
            Ok(Arc::new(NoView) as Arc<dyn spawn_core::port::EngineView>)
        };
        let no_ctr = |_: &dyn Fn(&str) -> Option<String>| Err("no container".to_string());
        let (policy, said) = engine_view_with(
            &environment(&[("SPAWN_CLAUDE_SANDBOX", "1")]),
            &claude_only,
            works,
            no_ctr,
        );
        assert!(
            matches!(policy, EngineViewPolicy::Required(Ok(_))),
            "{policy:?}"
        );
        assert_eq!(said, None);
        assert_eq!(made.get(), 1);
    }

    #[test]
    fn container_engine_view_falls_back_only_on_container_namespace_block_and_fails_closed_otherwise()
     {
        use super::engine_view_with;
        use spawn_core::bridge::EngineViewPolicy;
        let agy = engines(environment(&[
            ("HOME", "/home/a"),
            ("SPAWN_ENGINES", "claude,agy"),
        ]))
        .expect("agy");
        let perm_err = "/usr/bin/bwrap does not work on this host (exit status: 1): bwrap: No permissions to create a new namespace";
        let missing_err = "bwrap is not installed (not found on PATH)";

        // Missing bwrap must NEVER fall back to ContainerView, even with SPAWN_ENGINE_VIEW=container.
        let ctr_called = std::cell::Cell::new(0);
        let (policy_missing, _) = engine_view_with(
            &environment(&[("SPAWN_ENGINE_VIEW", "container")]),
            &agy,
            |_| Err(missing_err.to_string()),
            |_| {
                ctr_called.set(ctr_called.get() + 1);
                Ok(Arc::new(NoView) as Arc<dyn spawn_core::port::EngineView>)
            },
        );
        assert!(matches!(policy_missing, EngineViewPolicy::Required(Err(_))));
        assert_eq!(ctr_called.get(), 0);

        // Bare host with EPERM: make_container fails closed.
        let (policy_bare, _) = engine_view_with(
            &environment(&[("SPAWN_ENGINE_VIEW", "container")]),
            &agy,
            |_| Err(perm_err.to_string()),
            |_| Err("refusing ContainerView on a bare host".to_string()),
        );
        assert!(
            matches!(&policy_bare, EngineViewPolicy::Required(Err(why)) if why.contains("bare host")),
            "{policy_bare:?}"
        );

        // Container namespace block inside container: falls back with clear notice.
        let (policy_ok, said_ok) = engine_view_with(
            &environment(&[("SPAWN_ENGINE_VIEW", "container")]),
            &agy,
            |_| Err(perm_err.to_string()),
            |_| Ok(Arc::new(NoView) as Arc<dyn spawn_core::port::EngineView>),
        );
        assert!(matches!(policy_ok, EngineViewPolicy::Required(Ok(_))));
        assert!(said_ok.expect("notice").contains("container view"));
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
        assert_eq!(named[0].1.data_dir, None);
        let with_data = engines(lookup(&[
            ("SPAWN_ENGINES", "agy"),
            ("SPAWN_AGY_DATA_DIR", " /data01/agy-data/ "),
        ]))
        .expect("agy data dir");
        assert_eq!(with_data[0].1.data_dir.as_deref(), Some("/data01/agy-data"));
        assert!(!with_data[0].1.isolate_brain);
        let isolated = engines_for(
            lookup(&[
                ("SPAWN_ENGINES", "agy"),
                ("SPAWN_AGY_DATA_DIR", "/data01/agy-data"),
                ("SPAWN_AGY_ISOLATE_BRAIN", "1"),
            ]),
            true,
        )
        .expect("isolated agy");
        assert!(isolated[0].1.isolate_brain);
        assert!(
            engines(lookup(&[
                ("SPAWN_ENGINES", "agy"),
                ("SPAWN_AGY_DATA_DIR", "relative/dir"),
            ]))
            .is_err()
        );
        let refused = engines(lookup(&[("SPAWN_ENGINES", "claude,codex")])).expect_err("codex");
        assert!(refused.contains("codex"), "{refused}");
    }

    #[test]
    fn bwrap_v2_features_gate_strictly_on_herdr_backend() {
        use super::view_config_for_backend;
        let env = environment(&[
            ("HOME", "/home/u"),
            ("SPAWN_ENGINES", "agy"),
            ("SPAWN_AGY_DATA_DIR", "/data01/agy"),
            ("SPAWN_AGY_ISOLATE_BRAIN", "1"),
            ("SPAWN_SANDBOX_CANONICAL_WORKSPACE", "/workspace"),
            ("SPAWN_SANDBOX_HOME_ISOLATION", "whitelist"),
        ]);

        let herdr_view = view_config_for_backend(&env, true);
        assert_eq!(
            herdr_view.canonical_workspace,
            Some(std::path::PathBuf::from("/workspace"))
        );
        assert_eq!(
            herdr_view.home_isolation,
            spawn_host::HomeIsolation::Whitelist
        );
        assert!(herdr_view.isolate_brain);
        let herdr_engines = engines_for(&env, true).expect("herdr engines");
        assert!(herdr_engines[0].1.isolate_brain);

        let tmux_view = view_config_for_backend(&env, false);
        assert_eq!(tmux_view.canonical_workspace, None);
        assert_eq!(
            tmux_view.home_isolation,
            spawn_host::HomeIsolation::Blacklist
        );
        assert!(!tmux_view.isolate_brain);
        let tmux_engines = engines_for(&env, false).expect("tmux engines");
        assert!(!tmux_engines[0].1.isolate_brain);
    }

    #[test]
    fn parse_bounded_usize_handles_defaults_and_bounds() {
        use super::parse_bounded_usize;
        let none = |_: &str| None;
        assert_eq!(parse_bounded_usize(&none, "V", 8, 1, 128), Ok(8));
        let empty = |_: &str| Some("  ".to_string());
        assert_eq!(parse_bounded_usize(&empty, "V", 8, 1, 128), Ok(8));
        let valid = |_: &str| Some(" 32 ".to_string());
        assert_eq!(parse_bounded_usize(&valid, "V", 8, 1, 128), Ok(32));
        let zero = |_: &str| Some("0".to_string());
        assert!(parse_bounded_usize(&zero, "V", 8, 1, 128).is_err());
        let high = |_: &str| Some("129".to_string());
        assert!(parse_bounded_usize(&high, "V", 8, 1, 128).is_err());
    }

    #[test]
    fn model_preflight_skips_unset_off_and_stub_endpoints() {
        use super::{preflight_model_endpoints_with, resolve_judge_url, resolve_model_relay};
        use crate::Flags;
        use std::cell::Cell;

        let calls = Cell::new(0usize);
        let counting_probe = |url: &str, _timeout: std::time::Duration| {
            calls.set(calls.get() + 1);
            spawn_jev::EndpointHealth {
                status: spawn_jev::EndpointStatus::Ok,
                target: spawn_jev::sanitize_endpoint_target(url),
                http_status: Some(200),
                latency_ms: 1,
                detail: None,
            }
        };

        let empty_flags = Flags::default();
        for disabled in ["", "off", "OFF", "stub", "Stub"] {
            let pairs = [
                ("SPAWN_MODEL_RELAY", disabled),
                ("SPAWN_JUDGE_URL", disabled),
            ];
            let lookup = environment(&pairs);
            assert_eq!(resolve_model_relay(&lookup), None);
            assert_eq!(resolve_judge_url(&empty_flags, &lookup), None);
            let outcome = preflight_model_endpoints_with(
                resolve_model_relay(&lookup),
                resolve_judge_url(&empty_flags, &lookup).as_deref(),
                &lookup,
                &counting_probe,
            )
            .expect("disabled endpoints stay Ok");
            assert_eq!(outcome.model_relay, None);
            assert!(outcome.warnings.is_empty());
        }
        assert_eq!(calls.get(), 0, "disabled endpoints must never probe");

        let preflight_off = environment(&[
            ("SPAWN_MODEL_RELAY", "http://127.0.0.1:8791"),
            ("SPAWN_JUDGE_URL", "http://127.0.0.1:8790"),
            ("SPAWN_MODEL_PREFLIGHT", "off"),
        ]);
        let relay = resolve_model_relay(&preflight_off).expect("relay");
        let outcome = preflight_model_endpoints_with(
            Some(relay.clone()),
            Some("http://127.0.0.1:8790"),
            &preflight_off,
            &counting_probe,
        )
        .expect("preflight off succeeds");
        assert_eq!(outcome.model_relay, Some(relay));
        assert!(!outcome.cheap_fell_back_to_default);
        assert_eq!(calls.get(), 0, "SPAWN_MODEL_PREFLIGHT=off must skip probes");
    }

    #[test]
    fn model_preflight_healthy_endpoints_keep_relay_without_warnings() {
        use super::{preflight_model_endpoints_with, resolve_model_relay};

        let env = environment(&[
            ("SPAWN_MODEL_RELAY", "http://127.0.0.1:8791"),
            ("SPAWN_CHEAP_MODEL", "deepseek-flash"),
            ("SPAWN_DEFAULT_MODEL", "claude-opus-5-5"),
        ]);
        let relay = resolve_model_relay(&env).expect("relay");
        let ok_probe = |url: &str, _timeout: std::time::Duration| spawn_jev::EndpointHealth {
            status: spawn_jev::EndpointStatus::Ok,
            target: spawn_jev::sanitize_endpoint_target(url),
            http_status: Some(200),
            latency_ms: 2,
            detail: None,
        };
        let outcome = preflight_model_endpoints_with(
            Some(relay.clone()),
            Some("http://127.0.0.1:8790"),
            &env,
            &ok_probe,
        )
        .expect("healthy endpoints succeed");
        assert_eq!(outcome.model_relay, Some(relay));
        assert!(!outcome.cheap_fell_back_to_default);
        assert_eq!(outcome.fallback_target_model, None);
        assert!(outcome.warnings.is_empty());
    }

    #[test]
    fn model_preflight_unreachable_warn_degrades_cheap_to_default_and_redacts_secrets() {
        use super::{preflight_model_endpoints_with, resolve_model_relay};

        let env_default_warn = environment(&[
            (
                "SPAWN_MODEL_RELAY",
                "http://relayuser:RelaySecretPass99@127.0.0.1:8791?api_key=RelayQueryTok88",
            ),
            ("SPAWN_CHEAP_MODEL", "deepseek-v4-pro"),
            ("SPAWN_DEFAULT_MODEL", "claude-opus-5-5"),
        ]);
        let relay = resolve_model_relay(&env_default_warn).expect("relay");
        let down_probe = |url: &str, _timeout: std::time::Duration| spawn_jev::EndpointHealth {
            status: spawn_jev::EndpointStatus::Unreachable,
            target: spawn_jev::sanitize_endpoint_target(url),
            http_status: None,
            latency_ms: 4,
            detail: Some(format!(
                "connect {}: connection refused",
                spawn_jev::sanitize_endpoint_target(url)
            )),
        };

        let outcome = preflight_model_endpoints_with(
            Some(relay.clone()),
            Some("http://judgeuser:JudgeSecretPass77@127.0.0.1:8790?token=JudgeQueryTok66"),
            &env_default_warn,
            &down_probe,
        )
        .expect("default warn mode allows startup with fallback");

        assert!(
            outcome.model_relay.is_none(),
            "unreachable relay must be cleared under safe cheap fallback"
        );
        assert!(outcome.cheap_fell_back_to_default);
        assert_eq!(
            outcome.fallback_target_model.as_deref(),
            Some("claude-opus-5-5")
        );
        let joined = outcome.warnings.join("\n");
        assert!(joined.contains("slot=model_relay"), "{joined}");
        assert!(joined.contains("slot=judge"), "{joined}");
        assert!(joined.contains("127.0.0.1:8791"), "{joined}");
        assert!(joined.contains("127.0.0.1:8790"), "{joined}");
        assert!(joined.contains("cheap_fallback_to_default"), "{joined}");
        assert!(joined.contains("deepseek-v4-pro"), "{joined}");
        assert!(joined.contains("claude-opus-5-5"), "{joined}");
        for secret in [
            "RelaySecretPass99",
            "RelayQueryTok88",
            "relayuser",
            "JudgeSecretPass77",
            "JudgeQueryTok66",
            "judgeuser",
        ] {
            assert!(!joined.contains(secret), "secret {secret} leaked: {joined}");
        }

        // Opting out of cheap fallback keeps relay config attached while still warning.
        let keep_relay_env = environment(&[
            ("SPAWN_MODEL_RELAY", "http://127.0.0.1:8791"),
            ("SPAWN_CHEAP_FALLBACK", "off"),
        ]);
        let relay_keep = resolve_model_relay(&keep_relay_env).expect("relay");
        let kept = preflight_model_endpoints_with(
            Some(relay_keep.clone()),
            None,
            &keep_relay_env,
            &down_probe,
        )
        .expect("warn with keep-relay succeeds");
        assert_eq!(kept.model_relay, Some(relay_keep));
        assert!(!kept.cheap_fell_back_to_default);
    }

    #[test]
    fn model_preflight_unreachable_strict_fails_fast_with_actionable_error() {
        use super::{preflight_model_endpoints_with, resolve_model_relay};

        let env = environment(&[
            ("SPAWN_MODEL_PREFLIGHT", "strict"),
            (
                "SPAWN_MODEL_RELAY",
                "http://strictuser:StrictSecret11@127.0.0.1:8791?api_key=StrictQuery22",
            ),
        ]);
        let relay = resolve_model_relay(&env).expect("relay");
        let down_probe = |url: &str, _timeout: std::time::Duration| spawn_jev::EndpointHealth {
            status: spawn_jev::EndpointStatus::Unreachable,
            target: spawn_jev::sanitize_endpoint_target(url),
            http_status: Some(503),
            latency_ms: 9,
            detail: Some("endpoint returned HTTP 503".to_string()),
        };

        let err = preflight_model_endpoints_with(
            Some(relay),
            Some("http://j:JudgeSec33@127.0.0.1:8790?token=JTok44"),
            &env,
            &down_probe,
        )
        .expect_err("strict mode must abort startup");
        assert!(err.contains("SPAWN_MODEL_PREFLIGHT=strict"), "{err}");
        assert!(err.contains("slot=model_relay"), "{err}");
        assert!(err.contains("slot=judge"), "{err}");
        assert!(err.contains("127.0.0.1:8791"), "{err}");
        assert!(err.contains("127.0.0.1:8790"), "{err}");
        for secret in ["StrictSecret11", "StrictQuery22", "JudgeSec33", "JTok44"] {
            assert!(!err.contains(secret), "secret {secret} leaked: {err}");
        }
    }

    #[test]
    fn shared_build_caches_default_off_and_explicit_toggle_off() {
        use super::{resolve_shared_build_cache, view_config_for_backend};
        use std::path::Path;

        let ws = Path::new("/data01/spawn/workspaces");
        for off in ["0", "false", "FALSE", "no", "off", "Off"] {
            let env = environment(&[
                ("HOME", "/home/owner"),
                ("SPAWN_WORKSPACE_ROOT", "/data01/spawn/workspaces"),
                ("SPAWN_SHARED_BUILD_CACHES", off),
                ("SPAWN_SHARED_BUILD_CACHE_DIR", "/data01/custom-cache"),
            ]);
            assert_eq!(
                resolve_shared_build_cache(&env, Some(ws)),
                Ok(None),
                "{off}"
            );
            let view = view_config_for_backend(&env, true);
            assert!(view.extra_mounts.is_empty(), "{off}");
        }

        let unset = environment(&[
            ("HOME", "/home/owner"),
            ("SPAWN_WORKSPACE_ROOT", "/data01/spawn/workspaces"),
        ]);
        assert_eq!(resolve_shared_build_cache(&unset, Some(ws)), Ok(None));
        assert!(
            view_config_for_backend(&unset, true)
                .extra_mounts
                .is_empty()
        );

        let invalid_toggle = environment(&[("SPAWN_SHARED_BUILD_CACHES", "maybe")]);
        assert!(resolve_shared_build_cache(&invalid_toggle, Some(ws)).is_err());
    }

    #[test]
    fn shared_build_caches_opt_in_creates_subdirs_injects_vars_and_mounts_rw() {
        use super::{
            SHARED_BUILD_CACHE_VARS, prepare_shared_build_caches, view_config_for_backend,
            view_config_for_backend_with_cache,
        };
        use std::path::PathBuf;

        let base = std::env::temp_dir().join(format!("spawn-rt-caches-{}", std::process::id()));
        let ws = base.join("ws");
        std::fs::create_dir_all(&ws).expect("mkdir ws");
        let expected_root = ws.join(".caches");

        for val in ["1", "true", "YES", "on"] {
            let env = environment(&[("HOME", "/home/owner"), ("SPAWN_SHARED_BUILD_CACHES", val)]);
            let plan = prepare_shared_build_caches(&env, &ws).expect(val);
            assert_eq!(plan.root.as_ref(), Some(&expected_root), "{val}");
            assert_eq!(plan.env.len(), 8);
            for (var, subdir) in SHARED_BUILD_CACHE_VARS {
                let sub_path = expected_root.join(subdir);
                assert!(sub_path.is_dir(), "missing {}", sub_path.display());
                assert!(
                    plan.env
                        .contains(&((*var).to_string(), sub_path.display().to_string())),
                    "missing {var} in {:?}",
                    plan.env
                );
            }
        }

        // Sandbox ViewConfig keeps whitelist home isolation and adds a single RW ExtraMount.
        let ro_existing = expected_root.display().to_string();
        let mount_spec = format!("{ro_existing}:ro,/opt/tools:ro");
        let env_mount = environment(&[
            ("HOME", "/home/owner"),
            ("SPAWN_WORKSPACE_ROOT", ws.to_str().expect("utf8")),
            ("SPAWN_SHARED_BUILD_CACHES", "1"),
            ("SPAWN_SANDBOX_HOME_ISOLATION", "whitelist"),
            ("SPAWN_ENGINE_VIEW_WRITABLE", &mount_spec),
        ]);
        let view = view_config_for_backend(&env_mount, true);
        assert_eq!(view.home_isolation, spawn_host::HomeIsolation::Whitelist);
        let cache_mounts: Vec<&spawn_host::ExtraMount> = view
            .extra_mounts
            .iter()
            .filter(|m| m.path == expected_root)
            .collect();
        assert_eq!(cache_mounts.len(), 1, "cache mount must be deduplicated");
        assert!(cache_mounts[0].writable, "cache mount must be writable");
        assert_eq!(view.extra_mounts.len(), 2);

        let direct = view_config_for_backend_with_cache(
            &environment(&[("HOME", "/home/owner")]),
            true,
            Some(&expected_root),
        );
        assert_eq!(
            direct.extra_mounts,
            vec![spawn_host::ExtraMount {
                path: PathBuf::from(&expected_root),
                writable: true,
            }]
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn shared_build_cache_dir_and_single_tool_overrides_preserve_caller_vars() {
        use super::{prepare_shared_build_caches, shared_build_cache_env};
        use std::path::PathBuf;

        let base = std::env::temp_dir().join(format!("spawn-rt-override-{}", std::process::id()));
        let ws = base.join("ws");
        let custom = base.join("shared-custom");
        std::fs::create_dir_all(&ws).expect("mkdir ws");
        let custom_with_slash = format!("{}/", custom.display());

        // `SPAWN_SHARED_BUILD_CACHE_DIR` alone enables caching at that custom path
        // and keeps caller-set `CCACHE_DIR` and `GOCACHE` untouched.
        let env = environment(&[
            ("HOME", "/home/owner"),
            ("SPAWN_SHARED_BUILD_CACHE_DIR", &custom_with_slash),
            ("CCACHE_DIR", "/data01/caller/ccache"),
            ("GOCACHE", "/data01/caller/go-build"),
            ("PIP_CACHE_DIR", "  "),
        ]);
        let plan = prepare_shared_build_caches(&env, &ws).expect("custom cache");
        assert_eq!(plan.root, Some(custom.clone()));
        let expected: Vec<(String, String)> = vec![
            ("ZIG_GLOBAL_CACHE_DIR", custom.join("zig")),
            ("CCACHE_DIR", PathBuf::from("/data01/caller/ccache")),
            ("UV_CACHE_DIR", custom.join("uv")),
            ("SCCACHE_DIR", custom.join("sccache")),
            ("PIP_CACHE_DIR", custom.join("pip")),
            ("NPM_CONFIG_CACHE", custom.join("npm")),
            ("GOMODCACHE", custom.join("go-mod")),
            ("GOCACHE", PathBuf::from("/data01/caller/go-build")),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.display().to_string()))
        .collect();
        assert_eq!(plan.env, expected);

        let rechecked = shared_build_cache_env(&env, &custom);
        assert_eq!(rechecked, expected);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn shared_build_caches_reject_relative_traversal_and_sensitive_dirs() {
        use super::resolve_shared_build_cache;
        use std::path::Path;

        let ws = Path::new("/data01/spawn/workspaces");
        for bad in [
            "caches",
            "./caches",
            "~/caches",
            "/data01/caches/../etc",
            "/data01/../caches",
            "/",
            "/tmp",
            "/etc",
            "/etc/spawn-cache",
            "/run",
            "/run/keep",
            "/run/keep/secrets",
            "/run/keep-test",
            "/run/keep-test/sub",
            "/var/run/keep",
            "/var/tmp",
            "/home",
            "/home/owner",
        ] {
            let env = environment(&[
                ("HOME", "/home/owner"),
                ("SPAWN_SHARED_BUILD_CACHE_DIR", bad),
            ]);
            assert!(
                resolve_shared_build_cache(&env, Some(ws)).is_err(),
                "expected rejection for {bad:?}"
            );
        }

        // Symlink resolving into a sensitive directory is also rejected.
        let base = std::env::temp_dir().join(format!("spawn-rt-symlink-{}", std::process::id()));
        std::fs::create_dir_all(&base).expect("mkdir base");
        let sneak = base.join("sneaky-etc");
        std::os::unix::fs::symlink("/etc", &sneak).expect("symlink");
        let env = environment(&[
            ("HOME", "/home/owner"),
            (
                "SPAWN_SHARED_BUILD_CACHE_DIR",
                sneak.to_str().expect("utf8"),
            ),
        ]);
        assert!(resolve_shared_build_cache(&env, Some(ws)).is_err());

        // `SPAWN_SHARED_BUILD_CACHES=1` without any workspace root refuses cleanly.
        let no_ws = environment(&[("SPAWN_SHARED_BUILD_CACHES", "1")]);
        assert!(resolve_shared_build_cache(&no_ws, None).is_err());

        let _ = std::fs::remove_dir_all(&base);
    }
}
