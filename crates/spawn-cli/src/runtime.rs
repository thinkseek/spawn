//! Assembly of the Bridge and the Observer over one store and the host
//! adapters (git workspaces, tmux terminals, optional container, judge).

use std::sync::Arc;

use spawn_core::bridge::{Bridge, BridgeConfig, ModelRelay};
use spawn_core::observe::{HeuristicJudge, Observer, StateJudge};
use spawn_core::port::{CommandQueue, EventLog, TerminalHost, WorkspaceStore};
use spawn_host::GitWorkspace;
use spawn_jev::{JevConfig, JevJudge};

use crate::Flags;
use crate::config::{
    chunk_store, env_or, env_value, open_store, required_env, spawn_program, workspace_root,
};
use crate::{context, sources, supervise};

/// Build the Bridge and the Observer over one store.
///
/// Both keep the event-log cursor in memory, so the resident loop builds
/// them once. Rebuilding them per step would replay the whole log on every
/// tick, and that cost grows with the log.
pub(crate) fn runtime(flags: &Flags) -> Result<(Bridge, Observer), String> {
    let store = Arc::new(open_store()?);
    let repo = flags
        .get("repo")
        .or_else(|| env_value("SPAWN_GIT_REPO"))
        .ok_or_else(|| "--repo or SPAWN_GIT_REPO is required".to_string())?;
    let root = workspace_root(flags)
        .ok_or_else(|| "--root or SPAWN_WORKSPACE_ROOT is required".to_string())?;
    let docker = env_or("SPAWN_DOCKER_PROGRAM", "docker");
    let image = env_value("SPAWN_EXEC_IMAGE");

    // The engine runs in a container when an image is configured. That
    // container also hosts tmux, so the terminal host needs the same image.
    // Every agent's workspace comes from its mission's source; agents no
    // mission owns use the default repository.
    let factory: sources::Factory = {
        let (root, docker, image) = (root.clone(), docker.clone(), image.clone());
        let chunks = chunk_store(&root);
        Arc::new(move |repo: &std::path::Path| {
            match image.as_ref() {
                Some(image) => GitWorkspace::with_container(repo, &root, &docker, image),
                None => GitWorkspace::new(repo, &root),
            }
            .with_chunks(Arc::clone(&chunks))
        })
    };
    let workspaces: Arc<dyn WorkspaceStore> = Arc::new(sources::MissionWorkspaces::new(
        factory(std::path::Path::new(&repo)),
        std::path::Path::new(&root),
        store.clone(),
        factory,
    ));
    let terminals: Arc<dyn TerminalHost> = Arc::new(crate::config::terminal_host());

    let config = bridge_config()?;
    let log: Arc<dyn EventLog> = store.clone();
    let queue: Arc<dyn CommandQueue> = store.clone();
    let mut bridge = Bridge::new(
        Arc::clone(&log),
        queue,
        workspaces,
        Arc::clone(&terminals),
        config,
    );
    bridge.recover().map_err(|error| error.to_string())?;

    let context = context::HostContext::new(open_store()?, terminals);
    let mut observer = Observer::new(log, judge(flags)).with_context(Arc::new(context));
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
    Ok(config)
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
        cheap_model: env_or("SPAWN_CHEAP_MODEL", "deepseek-v4-pro"),
        small_model: env_or("SPAWN_SMALL_MODEL", "deepseek-flash"),
    })
}

/// Claude Code's transcript directory as the engine sees it:
/// `SPAWN_CLAUDE_PROJECTS`, else `$CLAUDE_CONFIG_DIR/projects`, else
/// `$HOME/.claude/projects`.
fn claude_projects() -> String {
    env_value("SPAWN_CLAUDE_PROJECTS")
        .or_else(|| env_value("CLAUDE_CONFIG_DIR").map(|dir| format!("{dir}/projects")))
        .unwrap_or_else(|| format!("{}/.claude/projects", env_or("HOME", "/root")))
}

/// The model judge at `--judge-url` or `SPAWN_JUDGE_URL`, else the heuristic.
fn judge(flags: &Flags) -> Arc<dyn StateJudge> {
    match flags
        .get("judge-url")
        .or_else(|| env_value("SPAWN_JUDGE_URL"))
    {
        Some(url) => {
            let mut config = JevConfig::new(&url);
            if let Some(model) = env_value("SPAWN_JUDGE_MODEL") {
                config.model = model;
            }
            Arc::new(JevJudge::new(config))
        }
        None => Arc::new(HeuristicJudge),
    }
}
