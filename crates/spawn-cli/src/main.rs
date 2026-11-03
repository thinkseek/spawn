//! Spawn command-line entry point.
//!
//! The binary only wires library ports. Business rules live in the library
//! crates and are covered there through public API tests.

// Pedantic doc-bookkeeping lints: public items carry purposeful docs, but
// mandatory Errors/Panics sections and identifier backticks add noise.
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::doc_markdown
)]
#![allow(clippy::module_name_repetitions, clippy::must_use_candidate)]
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use spawn_engine::{ClaudeHookInput, normalize_claude_hook};
use spawn_judge_jev::{JevConfig, JevJudge};
use spawn_observer::{HeuristicJudge, Observer};
use spawn_projector::{InMemoryViewStore, Projector};
use spawn_store::EventLog;
use spawn_store_postgres::PostgresStore;
use spawn_types::clock::SystemClock;
use spawn_types::event::{EventSource, NewEvent};
use spawn_types::identifiers::{AgentId, ExecutionId, SessionId};

mod config;

use config::EnvConfig;

fn main() {
    let result = run();
    if let Err(error) = result {
        eprintln!("spawn: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let command = std::env::args().nth(1).unwrap_or_default();
    match command.as_str() {
        "migrate" => cmd_migrate(),
        "hook" => cmd_hook(),
        "observe" => cmd_observe(),
        "project" => cmd_project(),
        other => Err(format!(
            "unknown subcommand '{other}'; expected migrate|hook|observe|project"
        )),
    }
}

fn connect(config: &EnvConfig) -> Result<PostgresStore, String> {
    PostgresStore::connect(&config.database_url, Arc::new(SystemClock))
        .map_err(|error| error.to_string())
}

fn cmd_migrate() -> Result<(), String> {
    let config = EnvConfig::from_env()?;
    let _store = connect(&config)?;
    println!("migrations applied");
    Ok(())
}

fn cmd_hook() -> Result<(), String> {
    let config = EnvConfig::from_env()?;
    let mut body = String::new();
    std::io::stdin()
        .read_to_string(&mut body)
        .map_err(|error| error.to_string())?;
    let input: ClaudeHookInput =
        serde_json::from_str(&body).map_err(|error| format!("invalid hook payload: {error}"))?;
    let normalized = normalize_claude_hook(&input).map_err(|error| error.to_string())?;
    let store = connect(&config)?;
    let outcome = store
        .append(NewEvent {
            agent_id: AgentId::from_prefixed(config.require("SPAWN_AGENT_ID")?)
                .map_err(|error| error.to_string())?,
            session_id: SessionId::from_prefixed(config.require("SPAWN_SESSION_ID")?)
                .map_err(|error| error.to_string())?,
            execution_id: Some(
                ExecutionId::from_prefixed(config.require("SPAWN_EXECUTION_ID")?)
                    .map_err(|error| error.to_string())?,
            ),
            source: EventSource::ClaudeHooks,
            source_event_id: normalized.source_event_id,
            kind: normalized.kind,
            artifact: None,
            occurred_at: None,
        })
        .map_err(|error| error.to_string())?;
    println!(
        "{}",
        serde_json::to_string(outcome.record()).map_err(|e| e.to_string())?
    );
    Ok(())
}

fn cmd_observe() -> Result<(), String> {
    let config = EnvConfig::from_env()?;
    let store = Arc::new(connect(&config)?);
    let judge: Arc<dyn spawn_observer::StateJudge> = match std::env::var("SPAWN_DECISION_URL") {
        Ok(url) if !url.is_empty() => Arc::new(JevJudge::new(JevConfig::new(url))),
        _ => Arc::new(HeuristicJudge::new()),
    };
    // Observation heartbeat timeout: 60 seconds by design, not a minute-named value.
    #[allow(clippy::duration_suboptimal_units)]
    let mut observer = Observer::new(store, judge, Arc::new(SystemClock), Duration::from_secs(60));
    loop {
        let emitted = observer.tick().map_err(|error| error.to_string())?;
        if !emitted.is_empty() {
            println!("recorded {} observation(s)", emitted.len());
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn cmd_project() -> Result<(), String> {
    let config = EnvConfig::from_env()?;
    let store = Arc::new(connect(&config)?);
    let views = Arc::new(InMemoryViewStore::new());
    let projector = Projector::new(store, views.clone(), spawn_types::event::GlobalPosition(0));
    loop {
        let cursor = projector
            .process_available()
            .map_err(|error| error.to_string())?;
        println!("projector at global position {}", cursor.0);
        std::thread::sleep(Duration::from_secs(1));
    }
}
