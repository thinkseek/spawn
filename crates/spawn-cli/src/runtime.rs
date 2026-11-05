//! Assembly of the Bridge and the Observer over one store and the host
//! adapters (git workspaces, tmux terminals, judge).

use std::sync::Arc;

use spawn_core::bridge::{Bridge, BridgeConfig, ModelRelay};
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
    use super::resolve_claude_projects;

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
}
