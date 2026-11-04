//! `hook` and `run`: the two commands that run inside an engine's terminal
//! and turn what the engine does into events.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

use spawn_core::event::{EndCause, EventKind, NewEvent, Source};
use spawn_core::id::{AgentId, ExecutionId, SessionId};
use spawn_core::ingest::{claude_hook, codex_line};
use spawn_core::port::EventLog;

use crate::config::{env_or, env_value, open_store, required_env};

/// The session a hook or runner inside a terminal reports for, read from the
/// environment the Bridge gave that terminal.
struct TerminalTarget {
    agent: AgentId,
    session: SessionId,
    execution: Option<ExecutionId>,
}

impl TerminalTarget {
    fn from_env() -> Result<Self, String> {
        Ok(Self {
            agent: AgentId::new(required_env("SPAWN_AGENT_ID")?)
                .map_err(|error| error.to_string())?,
            session: SessionId::new(required_env("SPAWN_SESSION_ID")?)
                .map_err(|error| error.to_string())?,
            execution: env_value("SPAWN_EXECUTION_ID")
                .map(|text| ExecutionId::new(text).map_err(|error| error.to_string()))
                .transpose()?,
        })
    }

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

/// `spawn hook`: ingest one Claude Code hook payload from standard input.
pub(crate) fn hook() -> Result<i32, String> {
    let mut payload = String::new();
    std::io::stdin()
        .read_to_string(&mut payload)
        .map_err(|error| format!("read hook payload: {error}"))?;
    if payload.trim().is_empty() {
        return Ok(0);
    }
    let store = open_store()?;
    let target = TerminalTarget::from_env()?;

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
    if let Err(error) =
        store.append(target.event(Source::Claude, normalized.source_event_id, normalized.kind))
    {
        eprintln!("spawn hook: {error}");
    }
    Ok(0)
}

/// `spawn run -- <program> [args]`: start the engine, record its exit, and
/// forward its exit code.
pub(crate) fn run(rest: &[String]) -> Result<i32, String> {
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
    let target = open_store().ok().zip(TerminalTarget::from_env().ok());

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
