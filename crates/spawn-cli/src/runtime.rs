//! Assembly of the Bridge and the Observer over one store and the host
//! adapters (git workspaces, tmux terminals, optional container, judge).

use std::sync::Arc;

use spawn_core::bridge::{Bridge, BridgeConfig};
use spawn_core::id::WorkspaceId;
use spawn_core::observe::{HeuristicJudge, Observer, StateJudge};
use spawn_core::port::{CommandQueue, EventLog, TerminalHost, WorkspaceStore};
use spawn_host::{GitWorkspace, TmuxHost, docker_prefix};
use spawn_jev::{JevConfig, JevJudge};

use crate::config::{
    env_or, env_value, open_store, required_env, spawn_program, workspace_root,
};
use crate::flags::Flags;
use crate::trace::{self, Timed};
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
    let tmux = env_or("SPAWN_TMUX_PROGRAM", "tmux");
    let socket = env_value("SPAWN_TMUX_SOCKET");
    let docker = env_or("SPAWN_DOCKER_PROGRAM", "docker");
    let image = env_value("SPAWN_EXEC_IMAGE");

    // The engine runs in a container when an image is configured. That
    // container also hosts tmux, so the terminal host needs the same image.
    // Every agent's workspace comes from its mission's source; agents no
    // mission owns use the default repository.
    let factory: sources::Factory = {
        let (root, docker, image) = (root.clone(), docker.clone(), image.clone());
        Arc::new(move |repo: &std::path::Path| match image.as_ref() {
            Some(image) => GitWorkspace::with_container(repo, &root, &docker, image),
            None => GitWorkspace::new(repo, &root),
        })
    };
    let workspaces: Arc<dyn WorkspaceStore> = Arc::new(sources::MissionWorkspaces::new(
        factory(std::path::Path::new(&repo)),
        std::path::Path::new(&root),
        store.clone(),
        factory,
    ));
    let terminals: Arc<dyn TerminalHost> = match image.as_ref() {
        Some(_) => Arc::new(TmuxHost::in_workspaces(
            &tmux,
            Arc::new(move |workspace: &WorkspaceId| docker_prefix(&docker, workspace.sandbox())),
            socket.clone(),
        )),
        None => Arc::new(TmuxHost::in_workspaces(
            &tmux,
            Arc::new(|_: &WorkspaceId| Vec::new()),
            socket,
        )),
    };

    let config = bridge_config()?;

    let mut log: Arc<dyn EventLog> = store.clone();
    let mut queue: Arc<dyn CommandQueue> = store.clone();
    let (mut workspaces, mut terminals) = (workspaces, terminals);
    let tracing = trace::enabled();
    if tracing {
        log = Arc::new(Timed(log));
        queue = Arc::new(Timed(queue));
        workspaces = Arc::new(Timed(workspaces));
        terminals = Arc::new(Timed(terminals));
    }
    let mut bridge = Bridge::new(
        Arc::clone(&log),
        queue,
        workspaces,
        Arc::clone(&terminals),
        config,
    );
    trace::time("bridge.recover", || bridge.recover()).map_err(|error| error.to_string())?;

    let mut judge = judge(flags);
    if tracing {
        judge = Arc::new(Timed(judge));
    }
    let context = context::HostContext::new(open_store()?, terminals);
    let mut observer = Observer::new(log, judge).with_context(Arc::new(context));
    trace::time("observer.recover", || observer.recover()).map_err(|error| error.to_string())?;
    Ok((bridge, observer))
}

/// Programs and environment the Bridge puts into every terminal it starts.
fn bridge_config() -> Result<BridgeConfig, String> {
    let mut config = BridgeConfig::new(&env_or("SPAWN_WORKER", "spawn-worker"));
    config.spawn_program = spawn_program();
    config.claude_program = env_or("SPAWN_CLAUDE_PROGRAM", "claude");
    config.codex_program = env_or("SPAWN_CODEX_PROGRAM", "codex");
    // The hook and the runner inside the terminal write to the same log, but
    // the agent reads their environment. The pane therefore carries the
    // restricted login, which can only append engine events.
    config.environment.push((
        "SPAWN_DATABASE_URL".to_string(),
        spawn_postgres::terminal_url(&required_env("SPAWN_DATABASE_URL")?),
    ));
    // The supervisor's pane gets its own login: it reads the fleet and
    // commands workers through the gateway, nothing more.
    config.session_environment.push((
        supervise::SUPERVISOR_SESSION.to_string(),
        vec![
            (
                "SPAWN_DATABASE_URL".to_string(),
                spawn_postgres::role_url(&required_env("SPAWN_DATABASE_URL")?, "spawn_supervisor"),
            ),
            ("SPAWN_GATEWAY".to_string(), "1".to_string()),
        ],
    ));
    Ok(config)
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
