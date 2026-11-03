//! The `spawn` binary: assembles stores, host adapters and the Bridge.
//!
//! Subcommands:
//!
//! - `init-db` installs the SQL schema.
//! - `submit` writes one command to the queue.
//! - `tick` runs one Bridge and Observer pass.
//! - `status` prints the last observation per session.
//! - `hook` ingests one Claude Code hook payload from standard input.
//! - `run` starts the engine inside the terminal and forwards its exit code.
//!
//! Configuration is environment first, so a terminal started by the Bridge
//! carries everything its hook and runner need.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;

use spawn_core::bridge::{Bridge, BridgeConfig};
use spawn_core::command::{CommandAction, Submission};
use spawn_core::event::{GlobalPosition, NewEvent, Source};
use spawn_core::id::{AgentId, ExecutionId, IdempotencyKey, Principal, SessionId};
use spawn_core::ingest::{claude_hook, codex_line};
use spawn_core::observe::{HeuristicJudge, Observer, StateJudge};
use spawn_core::port::{CommandQueue, EventLog, TerminalHost, WorkspaceStore};
use spawn_host::{LayerFsCli, TmuxHost};
use spawn_jev::{JevConfig, JevJudge};
use spawn_postgres::PostgresStore;

mod trace;
use trace::Timed;

fn main() {
    let code = match dispatch() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("spawn: {error}");
            1
        }
    };
    std::process::exit(code);
}

/// Parsed arguments.
#[derive(Debug, Default)]
struct Flags {
    values: HashMap<String, String>,
}

impl Flags {
    fn parse(arguments: &[String]) -> Result<Self, String> {
        let mut flags = Self::default();
        let mut rest = arguments.iter();
        while let Some(word) = rest.next() {
            let Some(name) = word.strip_prefix("--") else {
                return Err(format!("unexpected argument {word}"));
            };
            let Some(next) = rest.next() else {
                return Err(format!("--{name} needs a value"));
            };
            if next.starts_with("--") {
                return Err(format!("--{name} needs a value, found {next}"));
            }
            flags.values.insert(name.to_string(), next.clone());
        }
        Ok(flags)
    }

    fn get(&self, name: &str) -> Option<String> {
        self.values.get(name).cloned()
    }

    fn required(&self, name: &str) -> Result<String, String> {
        self.get(name)
            .ok_or_else(|| format!("--{name} is required"))
    }
}

fn dispatch() -> Result<i32, String> {
    let mut arguments = std::env::args().skip(1);
    let Some(command) = arguments.next() else {
        return Err("usage: spawn <init-db|submit|tick|status|hook|run>".to_string());
    };
    let rest: Vec<String> = arguments.collect();
    match command.as_str() {
        "init-db" => cmd_init_db(&Flags::parse(&rest)?),
        "submit" => cmd_submit(&Flags::parse(&rest)?),
        "tick" => cmd_tick(&Flags::parse(&rest)?),
        "status" => cmd_status(&Flags::parse(&rest)?),
        "hook" => cmd_hook(),
        "run" => cmd_run(&rest),
        other => Err(format!("unknown command {other}")),
    }
}

fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn env_or(name: &str, fallback: &str) -> String {
    env_value(name).unwrap_or_else(|| fallback.to_string())
}

fn required_env(name: &str) -> Result<String, String> {
    env_value(name).ok_or_else(|| format!("{name} is not set"))
}

fn open_store() -> Result<PostgresStore, String> {
    let url = required_env("SPAWN_DATABASE_URL")?;
    trace::time("pg.connect", || PostgresStore::connect(&url)).map_err(|error| error.to_string())
}

fn cmd_init_db(_flags: &Flags) -> Result<i32, String> {
    let store = open_store()?;
    store.migrate().map_err(|error| error.to_string())?;
    println!("spawn: schema installed");
    Ok(0)
}

fn cmd_submit(flags: &Flags) -> Result<i32, String> {
    let store = open_store()?;
    let principal = Principal::new(
        flags
            .get("principal")
            .unwrap_or_else(|| "spawn".to_string()),
    )
    .map_err(|error| error.to_string())?;
    let agent = AgentId::new(flags.required("agent")?).map_err(|error| error.to_string())?;
    let session = SessionId::new(flags.required("session")?).map_err(|error| error.to_string())?;
    let key = IdempotencyKey::new(flags.required("key")?).map_err(|error| error.to_string())?;
    let action: CommandAction = serde_json::from_str(&flags.required("action")?)
        .map_err(|error| format!("--action is not a command action: {error}"))?;
    let submission = Submission {
        agent_id: agent,
        session_id: session,
        idempotency_key: key,
        action,
    };
    let id = trace::time("queue.submit", || store.submit(&principal, &submission))
        .map_err(|error| error.to_string())?;
    println!("command={}", id.0);
    Ok(0)
}

fn cmd_tick(flags: &Flags) -> Result<i32, String> {
    let store = Arc::new(open_store()?);
    let project = flags
        .get("project")
        .or_else(|| env_value("SPAWN_PROJECT"))
        .ok_or_else(|| "--project or SPAWN_PROJECT is required".to_string())?;
    let layerfs = env_or("SPAWN_LAYERFS_PROGRAM", "layerfs");
    let tmux = env_or("SPAWN_TMUX_PROGRAM", "tmux");
    let socket = env_value("SPAWN_TMUX_SOCKET");

    let workspaces: Arc<dyn WorkspaceStore> = Arc::new(LayerFsCli::new(&layerfs, &project));
    let terminals: Arc<dyn TerminalHost> = if env_value("SPAWN_TMUX_IN_WORKSPACE").is_some() {
        let prefix = LayerFsCli::new(&layerfs, &project);
        Arc::new(TmuxHost::in_workspaces(
            &tmux,
            Arc::new(move |workspace| prefix.exec_prefix(workspace)),
            socket.clone(),
        ))
    } else {
        Arc::new(TmuxHost::local(&tmux))
    };

    let mut config = BridgeConfig::new(&env_or("SPAWN_WORKER", "spawn-worker"));
    config.spawn_program = env_or(
        "SPAWN_PROGRAM",
        &std::env::current_exe()
            .map_or_else(|_| "spawn".to_string(), |path| path.display().to_string()),
    );
    config.claude_program = env_or("SPAWN_CLAUDE_PROGRAM", "claude");
    config.codex_program = env_or("SPAWN_CODEX_PROGRAM", "codex");

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
    let mut bridge = Bridge::new(Arc::clone(&log), queue, workspaces, terminals, config);
    trace::time("bridge.recover", || bridge.recover()).map_err(|error| error.to_string())?;
    let report = trace::time("bridge.tick", || bridge.tick()).map_err(|error| error.to_string())?;
    for warning in &report.warnings {
        eprintln!("spawn tick warning: {warning}");
    }

    let mut judge: Arc<dyn StateJudge> = match flags
        .get("judge-url")
        .or_else(|| env_value("SPAWN_JUDGE_URL"))
    {
        Some(url) => Arc::new(JevJudge::new(JevConfig::new(&url))),
        None => Arc::new(HeuristicJudge),
    };
    if tracing {
        judge = Arc::new(Timed(judge));
    }
    let mut observer = Observer::new(log, judge);
    trace::time("observer.recover", || observer.recover()).map_err(|error| error.to_string())?;
    let observations =
        trace::time("observer.tick", || observer.tick()).map_err(|error| error.to_string())?;

    println!(
        "commands={} observations={} sessions={}",
        report.commands,
        observations,
        bridge.session_count()
    );
    Ok(0)
}

fn cmd_status(flags: &Flags) -> Result<i32, String> {
    let store = open_store()?;
    let limit = flags
        .get("limit")
        .and_then(|text| text.parse::<usize>().ok())
        .unwrap_or(1000);
    let events = store
        .read_global(GlobalPosition(0), limit)
        .map_err(|error| error.to_string())?;
    let mut last: HashMap<String, String> = HashMap::new();
    for record in &events {
        if record.event.kind.name() == "observation.recorded" {
            let kind =
                serde_json::to_string(&record.event.kind).unwrap_or_else(|_| "{}".to_string());
            last.insert(record.event.session_id.to_string(), kind);
        }
    }
    for (session, line) in last {
        println!("{session}\t{line}");
    }
    Ok(0)
}

fn cmd_hook() -> Result<i32, String> {
    let mut payload = String::new();
    std::io::stdin()
        .read_to_string(&mut payload)
        .map_err(|error| format!("read hook payload: {error}"))?;
    if payload.trim().is_empty() {
        return Ok(0);
    }
    let store = open_store()?;
    let agent = AgentId::new(required_env("SPAWN_AGENT_ID")?).map_err(|error| error.to_string())?;
    let session =
        SessionId::new(required_env("SPAWN_SESSION_ID")?).map_err(|error| error.to_string())?;
    let execution = env_value("SPAWN_EXECUTION_ID")
        .map(|text| ExecutionId::new(text).map_err(|error| error.to_string()))
        .transpose()?;

    let engine = env_or("SPAWN_ENGINE", "claude");
    let normalized = if engine == "codex" {
        codex_line(&payload)
    } else {
        claude_hook(&payload)
    };
    let normalized = match normalized {
        Ok(Some(normalized)) => normalized,
        Ok(None) => return Ok(0),
        Err(error) => {
            // A hook must never block the engine. The engine shows this line,
            // so a lost event is visible instead of silent.
            eprintln!("spawn hook: {error}");
            return Ok(0);
        }
    };
    let source = if engine == "codex" {
        Source::Codex
    } else {
        Source::Claude
    };
    if let Err(error) = store.append(NewEvent {
        agent_id: agent,
        session_id: session,
        execution_id: execution,
        source,
        source_event_id: normalized.source_event_id,
        kind: normalized.kind,
    }) {
        eprintln!("spawn hook: {error}");
    }
    Ok(0)
}

fn cmd_run(rest: &[String]) -> Result<i32, String> {
    let words: &[String] = match rest.split_first() {
        Some((first, tail)) if first == "--" => tail,
        Some(_) => rest,
        None => return Err("run needs a program after --".to_string()),
    };
    let (program, arguments) = words
        .split_first()
        .ok_or_else(|| "run needs a program after --".to_string())?;

    if env_or("SPAWN_ENGINE", "claude") != "codex" {
        let status = Command::new(program)
            .args(arguments)
            .status()
            .map_err(|error| format!("start {program}: {error}"))?;
        return Ok(status.code().unwrap_or(1));
    }

    // Codex prints one JSON object per line. Read it, record what matters,
    // and echo every line so the pane still shows the engine output.
    let mut child = Command::new(program)
        .args(arguments)
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("start {program}: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "no stdout from the engine".to_string())?;
    let store = open_store()?;
    let agent = AgentId::new(required_env("SPAWN_AGENT_ID")?).map_err(|error| error.to_string())?;
    let session =
        SessionId::new(required_env("SPAWN_SESSION_ID")?).map_err(|error| error.to_string())?;
    let execution = env_value("SPAWN_EXECUTION_ID")
        .map(|text| ExecutionId::new(text).map_err(|error| error.to_string()))
        .transpose()?;
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        println!("{line}");
        let _ = std::io::stdout().flush();
        if let Ok(Some(normalized)) = codex_line(&line) {
            let _ = store.append(NewEvent {
                agent_id: agent.clone(),
                session_id: session.clone(),
                execution_id: execution.clone(),
                source: Source::Codex,
                source_event_id: normalized.source_event_id,
                kind: normalized.kind,
            });
        }
    }
    let status = child.wait().map_err(|error| error.to_string())?;
    Ok(status.code().unwrap_or(1))
}
