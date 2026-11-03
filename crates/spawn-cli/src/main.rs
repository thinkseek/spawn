//! The `spawn` binary: assembles stores, host adapters and the Bridge.
//!
//! Subcommands:
//!
//! - `init-db` installs the SQL schema.
//! - `submit` writes one command to the queue.
//! - `tick` runs one Bridge and Observer pass.
//! - `evolve` advances the recursive self-improvement loop by one step.
//! - `status` prints the latest observation per session.
//! - `hook` ingests one Claude Code hook payload from standard input.
//! - `run` starts the engine inside the terminal, records its exit, and
//!   forwards its exit code.
//! - `version` prints `spawn <crate version>`.
//!
//! Configuration is environment first, so a terminal started by the Bridge
//! carries everything its hook and runner need.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;

use spawn_core::bridge::{Bridge, BridgeConfig};
use spawn_core::command::{CommandAction, Submission};
use spawn_core::event::{EndCause, EventKind, NewEvent, Source};
use spawn_core::id::{AgentId, ExecutionId, IdempotencyKey, Principal, SessionId, WorkspaceId};
use spawn_core::ingest::{claude_hook, codex_line};
use spawn_core::observe::{HeuristicJudge, Observer, StateJudge};
use spawn_core::port::{CommandQueue, EventLog, TerminalHost, WorkspaceStore};
use spawn_host::{GitWorkspace, TmuxHost, docker_prefix};
use spawn_jev::{JevConfig, JevJudge};
use spawn_postgres::PostgresStore;

mod context;
mod evolve;
mod serve;
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
pub(crate) struct Flags {
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

    pub(crate) fn get(&self, name: &str) -> Option<String> {
        self.values.get(name).cloned()
    }

    fn required(&self, name: &str) -> Result<String, String> {
        self.get(name)
            .ok_or_else(|| format!("--{name} is required"))
    }
}

pub(crate) fn dispatch() -> Result<i32, String> {
    let mut arguments = std::env::args().skip(1);
    let Some(command) = arguments.next() else {
        return Err(
            "usage: spawn <init-db|submit|tick|loop|evolve|serve|status|hook|run|version>"
                .to_string(),
        );
    };
    let rest: Vec<String> = arguments.collect();
    match command.as_str() {
        "init-db" => cmd_init_db(&Flags::parse(&rest)?),
        "submit" => cmd_submit(&Flags::parse(&rest)?),
        "tick" => cmd_tick(&Flags::parse(&rest)?),
        "loop" => cmd_loop(&Flags::parse(&rest)?),
        "serve" => serve::serve(&Flags::parse(&rest)?),
        "evolve" => evolve::evolve(&Flags::parse(&rest)?),
        "status" => cmd_status(&Flags::parse(&rest)?),
        "hook" => cmd_hook(),
        "run" => cmd_run(&rest),
        "version" => cmd_version(&rest),
        other => Err(format!("unknown command {other}")),
    }
}

pub(crate) fn env_value(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

pub(crate) fn env_or(name: &str, fallback: &str) -> String {
    env_value(name).unwrap_or_else(|| fallback.to_string())
}

fn required_env(name: &str) -> Result<String, String> {
    env_value(name).ok_or_else(|| format!("{name} is not set"))
}

pub(crate) fn open_store() -> Result<PostgresStore, String> {
    let url = required_env("SPAWN_DATABASE_URL")?;
    trace::time("pg.connect", || PostgresStore::connect(&url)).map_err(|error| error.to_string())
}

fn cmd_version(rest: &[String]) -> Result<i32, String> {
    if let Some(word) = rest.first() {
        return Err(format!("unexpected argument {word}"));
    }
    println!("spawn {}", env!("CARGO_PKG_VERSION"));
    Ok(0)
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

/// Build the Bridge and the Observer over one store.
///
/// Both keep the event-log cursor in memory, so the resident loop builds
/// them once. Rebuilding them per step would replay the whole log on every
/// tick, and that cost grows with the log.
fn runtime(flags: &Flags) -> Result<(Bridge, Observer), String> {
    let store = Arc::new(open_store()?);
    let repo = flags
        .get("repo")
        .or_else(|| env_value("SPAWN_GIT_REPO"))
        .ok_or_else(|| "--repo or SPAWN_GIT_REPO is required".to_string())?;
    let root = flags
        .get("root")
        .or_else(|| env_value("SPAWN_WORKSPACE_ROOT"))
        .ok_or_else(|| "--root or SPAWN_WORKSPACE_ROOT is required".to_string())?;
    let tmux = env_or("SPAWN_TMUX_PROGRAM", "tmux");
    let socket = env_value("SPAWN_TMUX_SOCKET");
    let docker = env_or("SPAWN_DOCKER_PROGRAM", "docker");
    let image = env_value("SPAWN_EXEC_IMAGE");

    // The engine runs in a container when an image is configured. That
    // container also hosts tmux, so the terminal host needs the same image.
    let workspaces: Arc<dyn WorkspaceStore> = Arc::new(match image.as_ref() {
        Some(image) => GitWorkspace::with_container(&repo, &root, &docker, image),
        None => GitWorkspace::new(&repo, &root),
    });
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

    let mut config = BridgeConfig::new(&env_or("SPAWN_WORKER", "spawn-worker"));
    config.spawn_program = env_or(
        "SPAWN_PROGRAM",
        &std::env::current_exe()
            .map_or_else(|_| "spawn".to_string(), |path| path.display().to_string()),
    );
    config.claude_program = env_or("SPAWN_CLAUDE_PROGRAM", "claude");
    config.codex_program = env_or("SPAWN_CODEX_PROGRAM", "codex");
    // The hook and the runner inside the terminal write to the same log, but
    // the agent reads their environment. The pane therefore carries the
    // restricted login, which can only append engine events.
    config.environment.push((
        "SPAWN_DATABASE_URL".to_string(),
        spawn_postgres::terminal_url(&required_env("SPAWN_DATABASE_URL")?),
    ));

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

    let mut judge: Arc<dyn StateJudge> = match flags
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
    };
    if tracing {
        judge = Arc::new(Timed(judge));
    }
    let context = context::HostContext::new(open_store()?, terminals);
    let mut observer = Observer::new(log, judge).with_context(Arc::new(context));
    trace::time("observer.recover", || observer.recover()).map_err(|error| error.to_string())?;
    Ok((bridge, observer))
}

fn cmd_tick(flags: &Flags) -> Result<i32, String> {
    let (mut bridge, mut observer) = runtime(flags)?;
    let report = trace::time("bridge.tick", || bridge.tick()).map_err(|error| error.to_string())?;
    for warning in &report.warnings {
        eprintln!("spawn tick warning: {warning}");
    }
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

/// Run the Bridge, the Observer and the RSI controller until killed.
///
/// The loop holds one connection and one in-memory cursor, so its cost does
/// not grow with the event log. A transient store error is logged and
/// retried with a growing pause. The one exit is a halt from the RSI
/// controller, which means three rejections in a row; that is a decision for
/// a human, so the process stops instead of spinning.
fn cmd_loop(flags: &Flags) -> Result<i32, String> {
    let interval = std::time::Duration::from_millis(
        env_value("SPAWN_LOOP_INTERVAL_MS")
            .and_then(|text| text.parse().ok())
            .unwrap_or(2000),
    );
    let (mut bridge, mut observer) = runtime(flags)?;
    let mut failures: u32 = 0;
    loop {
        match bridge.tick() {
            Ok(report) => {
                failures = 0;
                for warning in &report.warnings {
                    eprintln!("spawn loop warning: {warning}");
                }
            }
            Err(error) => {
                failures = failures.saturating_add(1);
                eprintln!("spawn loop: bridge: {error}");
            }
        }
        if let Err(error) = observer.tick() {
            eprintln!("spawn loop: observer: {error}");
        }
        match evolve::evolve(flags) {
            Ok(2) => {
                eprintln!("spawn loop: RSI halted after repeated rejections");
                return Ok(2);
            }
            Ok(_) => {}
            Err(error) => {
                failures = failures.saturating_add(1);
                eprintln!("spawn loop: evolve: {error}");
            }
        }
        // ponytail: fixed backoff, cap at 30 intervals. A per-error-kind
        // schedule only matters once a failure class is slow to clear.
        let pause = interval.saturating_mul(failures.clamp(1, 30));
        std::thread::sleep(pause);
    }
}

fn cmd_status(_flags: &Flags) -> Result<i32, String> {
    let store = open_store()?;
    for state in store.states().map_err(|error| error.to_string())? {
        let kind = EventKind::ObservationRecorded {
            observation: state.observation,
        };
        let line = serde_json::to_string(&kind).unwrap_or_else(|_| "{}".to_string());
        println!("{}\t{line}", state.session_id);
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

    // Only Claude Code has hooks; Codex events come through `spawn run`.
    let normalized = match claude_hook(&payload) {
        Ok(Some(normalized)) => normalized,
        Ok(None) => return Ok(0),
        Err(error) => {
            // A hook must never block the engine. The engine shows this line,
            // so a lost event is visible instead of silent.
            eprintln!("spawn hook: {error}");
            return Ok(0);
        }
    };
    if let Err(error) = store.append(NewEvent {
        agent_id: agent,
        session_id: session,
        execution_id: execution,
        source: Source::Claude,
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
    let codex = env_or("SPAWN_ENGINE", "claude") == "codex";
    let mut command = Command::new(program);
    command.args(arguments);
    if codex {
        command.stdout(Stdio::piped());
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("start {program}: {error}"))?;
    // The runner must not fail the engine: without a store it only runs it.
    let target = open_store().ok().zip(run_target().ok());

    if let Some(stdout) = child.stdout.take() {
        // Codex prints one JSON object per line. Record what matters and
        // echo every line so the pane still shows the engine output.
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            println!("{line}");
            let _ = std::io::stdout().flush();
            if let (Some((store, target)), Ok(Some(normalized))) = (&target, codex_line(&line)) {
                let _ = store.append(target.event(
                    Source::Codex,
                    normalized.source_event_id,
                    normalized.kind,
                ));
            }
        }
    }
    let status = child.wait().map_err(|error| error.to_string())?;
    let code = status.code();
    if let Some((store, target)) = &target
        && let Some(execution) = &target.execution
    {
        // The Bridge sees this on its next tick and does the cleanup; the
        // engine source keeps it apart from the Bridge's own end event.
        let source = if codex { Source::Codex } else { Source::Claude };
        let _ = store.append(target.event(
            source,
            Some(format!("{execution}:exited")),
            EventKind::ExecutionEnded {
                cause: EndCause::Exited,
                exit_code: code,
            },
        ));
    }
    Ok(code.unwrap_or(1))
}

/// Session the runner reports for, read from the terminal environment.
struct RunTarget {
    agent: AgentId,
    session: SessionId,
    execution: Option<ExecutionId>,
}

impl RunTarget {
    fn event(&self, source: Source, key: Option<String>, kind: EventKind) -> NewEvent {
        NewEvent {
            agent_id: self.agent.clone(),
            session_id: self.session.clone(),
            execution_id: self.execution.clone(),
            source,
            source_event_id: key,
            kind,
        }
    }
}

fn run_target() -> Result<RunTarget, String> {
    Ok(RunTarget {
        agent: AgentId::new(required_env("SPAWN_AGENT_ID")?).map_err(|error| error.to_string())?,
        session: SessionId::new(required_env("SPAWN_SESSION_ID")?)
            .map_err(|error| error.to_string())?,
        execution: env_value("SPAWN_EXECUTION_ID")
            .map(|text| ExecutionId::new(text).map_err(|error| error.to_string()))
            .transpose()?,
    })
}
