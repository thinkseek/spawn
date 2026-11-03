//! Read-only HTTP API and static files for the observer UI.
//!
//! The UI never writes. Every endpoint reads an existing view through the
//! same store methods the CLI uses, so a defect in the UI cannot change
//! state, and the API needs no authentication beyond binding to a private
//! address.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use spawn_core::event::SessionSeq;
use spawn_core::id::SessionId;
use tiny_http::{Header, Request, Response, Server};

use spawn_core::id::{TerminalName, WorkspaceId};
use spawn_core::port::TerminalHost;
use spawn_host::{TmuxHost, docker_prefix};
use spawn_postgres::{RSI_AGENT, RSI_SESSION, RsiIterationRow};

use crate::evolve::{Task, landed_metric, load_backlog, parse_metrics};
use crate::{Flags, env_or, env_value};

/// Halt threshold of the controller, mirrored for display.
const HALT_AFTER: u32 = 3;

/// Iterations the RSI page lists.
const RSI_ITERATIONS: u32 = 50;

/// Attempts the RSI page reads for its table and chart.
const RSI_ATTEMPTS: u32 = 500;

/// Pane lines the RSI page shows.
const PANE_LINES: usize = 120;

/// Default page size of the timeline.
const DEFAULT_LIMIT: usize = 200;

/// Largest page the timeline returns.
const MAX_LIMIT: usize = 1000;

/// Serve the API and the built UI until the process is killed.
pub fn serve(flags: &Flags) -> Result<i32, String> {
    let address = flags
        .get("address")
        .or_else(|| env_value("SPAWN_SERVE_ADDRESS"))
        .unwrap_or_else(|| "127.0.0.1:8787".to_string());
    let web = PathBuf::from(
        flags
            .get("web")
            .unwrap_or_else(|| env_or("SPAWN_WEB_DIR", "web/dist")),
    );
    let server = Server::http(&address).map_err(|error| format!("listen {address}: {error}"))?;
    println!("serve: http://{address} web {}", web.display());
    for request in server.incoming_requests() {
        if let Err(error) = handle(request, &web) {
            eprintln!("serve: {error}");
        }
    }
    Ok(0)
}

/// The dashboard's connection: the read-only `spawn_reader` login, never the
/// controller's. `serve` answers anyone on the LAN.
fn open_store() -> Result<spawn_postgres::PostgresStore, String> {
    let controller = env_value("SPAWN_DATABASE_URL")
        .ok_or_else(|| "SPAWN_DATABASE_URL is not set".to_string())?;
    let url = env_value("SPAWN_SERVE_DATABASE_URL")
        .unwrap_or_else(|| spawn_postgres::role_url(&controller, "spawn_reader"));
    spawn_postgres::PostgresStore::connect(&url).map_err(|error| error.to_string())
}

fn handle(request: Request, web: &Path) -> Result<(), String> {
    let url = request.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((path, query)) => (path, query),
        None => (url.as_str(), ""),
    };
    let answer = match path {
        "/api/overview" => overview(),
        "/api/rsi" => rsi_overview(),
        "/api/rsi/pane" => pane(RSI_SESSION),
        "/api/fleet" => fleet(),
        _ => {
            if let Some(id) = path.strip_prefix("/api/session/") {
                session(id, query)
            } else if let Some(number) = path.strip_prefix("/api/rsi/iteration/") {
                rsi_iteration(number)
            } else if let Some(id) = path.strip_prefix("/api/pane/") {
                pane(id)
            } else if let Some(rest) = path.strip_prefix("/api/fleet/") {
                fleet_session(rest, query)
            } else {
                return static_file(request, web, path);
            }
        }
    };
    let (status, body) = match answer {
        Ok(body) => (200, body),
        Err(error) => (500, json!({ "error": error })),
    };
    request
        .respond(json_response(status, &body))
        .map_err(|error| error.to_string())
}

fn json_response(status: u16, body: &Value) -> Response<std::io::Cursor<Vec<u8>>> {
    let header = Header::from_bytes("Content-Type", "application/json; charset=utf-8")
        .expect("static header is valid");
    Response::from_string(body.to_string())
        .with_status_code(status)
        .with_header(header)
}

fn overview() -> Result<Value, String> {
    let store = open_store()?;
    let sessions: Vec<Value> = store
        .states()
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|state| {
            json!({
                "session_id": state.session_id.as_str(),
                "agent_id": state.agent_id.as_str(),
                "observation": state.observation,
                "session_seq": state.session_seq.0,
                "recorded_at_ms": state.recorded_at.0,
            })
        })
        .collect();
    let rsi = store.rsi_state().map_err(|error| error.to_string())?;
    let open = store
        .rsi_open_iteration()
        .map_err(|error| error.to_string())?;
    let solved = store
        .rsi_solved_tasks()
        .map_err(|error| error.to_string())?;
    Ok(json!({
        "sessions": sessions,
        "rsi": {
            "baseline": rsi.as_ref().map(|state| state.baseline.clone()),
            "iteration": rsi.as_ref().map(|state| state.iteration),
            "consecutive_rejected": rsi.as_ref().map(|state| state.consecutive_rejected),
            "solved_tasks": solved,
            "open_iteration": open.map(|iteration| json!({
                "iteration": iteration.iteration,
                "task_id": iteration.task_id,
                "phase": iteration.phase,
                "agent_id": iteration.agent_id,
                "session_id": iteration.session_id,
                "candidate": iteration.candidate,
                "note": iteration.note,
                "patch_bytes": iteration.patch.as_ref().map_or(0, String::len),
            })),
        },
    }))
}

fn err(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| {
            i64::try_from(time.as_millis()).unwrap_or(i64::MAX)
        })
}

/// The objective the loop pursues: the first recurring task in the
/// controller's backlog that has not met its target, else the last one.
fn objective(store: &spawn_postgres::PostgresStore) -> Result<Option<Task>, String> {
    let backlog = PathBuf::from(env_or("SPAWN_RSI_BACKLOG", "rsi/backlog"));
    let recurring: Vec<Task> = load_backlog(&backlog)?
        .into_iter()
        .filter(|task| task.recurring)
        .collect();
    for task in &recurring {
        let met = match (landed_metric(store, task)?, task.target) {
            (Some(value), Some(target)) => {
                if task.lower_is_better {
                    value <= target
                } else {
                    value >= target
                }
            }
            _ => false,
        };
        if !met {
            return Ok(Some(task.clone()));
        }
    }
    Ok(recurring.last().cloned())
}

fn iteration_json(row: &RsiIterationRow) -> Value {
    let iteration = &row.iteration;
    json!({
        "iteration": iteration.iteration,
        "task_id": iteration.task_id,
        "phase": iteration.phase,
        "started_at_ms": row.started_at_ms,
        "updated_at_ms": row.updated_at_ms,
        "note": iteration.note,
        "candidate": iteration.candidate,
        "patch_bytes": iteration.patch.as_ref().map_or(0, String::len),
    })
}

/// Everything the RSI page shows, in one read.
fn rsi_overview() -> Result<Value, String> {
    let store = open_store()?;
    let state = store.rsi_state().map_err(err)?;
    let objective = objective(&store)?;
    let iterations: Vec<Value> = store
        .rsi_iterations(RSI_ITERATIONS)
        .map_err(err)?
        .iter()
        .map(iteration_json)
        .collect();
    let attempts = store.rsi_recent_attempts(RSI_ATTEMPTS).map_err(err)?;
    let unit = objective
        .as_ref()
        .map_or(String::new(), |task| task.unit.clone());
    let attempt_rows: Vec<Value> = attempts
        .iter()
        .map(|attempt| {
            let metrics: serde_json::Map<String, Value> = parse_metrics(&attempt.gate_log)
                .into_iter()
                .map(|metric| {
                    (
                        metric.name,
                        json!({ "baseline": metric.baseline, "candidate": metric.candidate, "unit": unit }),
                    )
                })
                .collect();
            json!({
                "task_id": attempt.task_id,
                "attempt": attempt.attempt,
                "iteration": attempt.iteration,
                "accepted": attempt.accepted,
                "created_at_ms": attempt.created_at_ms,
                "metrics": metrics,
            })
        })
        .collect();
    let history: Vec<Value> = attempts
        .iter()
        .rev()
        .flat_map(|attempt| {
            parse_metrics(&attempt.gate_log)
                .into_iter()
                .map(move |metric| {
                    json!({
                        "iteration": attempt.iteration,
                        "created_at_ms": attempt.created_at_ms,
                        "metric": metric.name,
                        "baseline": metric.baseline,
                        "candidate": metric.candidate,
                        "accepted": attempt.accepted,
                    })
                })
        })
        .collect();
    let state_json = state.map(|state| {
        let halt_reason = if state.consecutive_rejected >= HALT_AFTER {
            Some(format!(
                "{} rejections in a row",
                state.consecutive_rejected
            ))
        } else if state.infra_failures >= HALT_AFTER {
            Some(format!("{} aborts in a row", state.infra_failures))
        } else {
            None
        };
        json!({
            "baseline": state.baseline,
            "iteration": state.iteration,
            "consecutive_rejected": state.consecutive_rejected,
            "infra_failures": state.infra_failures,
            "scenario_count": state.scenario_count,
            "halted": halt_reason.is_some(),
            "halt_reason": halt_reason,
        })
    });
    Ok(json!({
        "generated_at_ms": now_ms(),
        "state": state_json,
        "objective": objective.map(|task| json!({
            "task_id": task.id,
            "title": task.title,
            "prompt": task.prompt,
            "metric": task.metric,
            "unit": task.unit,
            "direction": if task.lower_is_better { "lower" } else { "higher" },
            "target": task.target,
        })),
        "iterations": iterations,
        "attempts": attempt_rows,
        "metric_history": history,
    }))
}

/// One iteration with its patch, gate log and prompt.
fn rsi_iteration(number: &str) -> Result<Value, String> {
    let number: u32 = number
        .parse()
        .map_err(|_| format!("bad iteration {number}"))?;
    let store = open_store()?;
    let Some((row, prompt)) = store.rsi_iteration(number).map_err(err)? else {
        return Err(format!("iteration {number} does not exist"));
    };
    let gate_log = store
        .rsi_attempts_of(row.iteration.task_id, RSI_ATTEMPTS)
        .map_err(err)?
        .into_iter()
        .find(|attempt| attempt.iteration == number)
        .map(|attempt| attempt.gate_log)
        .unwrap_or_default();
    let mut body = iteration_json(&row);
    body["patch"] = json!(row.iteration.patch.clone().unwrap_or_default());
    body["gate_log"] = json!(gate_log);
    body["prompt"] = json!(prompt);
    Ok(body)
}

/// The fleet board: every session with its judgement and activity, plus
/// fleet health and the decision ladder. Computed by SQL from storage.
fn fleet() -> Result<Value, String> {
    let store = open_store()?;
    Ok(json!({
        "agents": store.fleet_board().map_err(err)?,
        "health": store.fleet_health().map_err(err)?,
    }))
}

/// `/api/fleet/<session>/judgements` and `/api/fleet/<session>/commands`.
fn fleet_session(rest: &str, query: &str) -> Result<Value, String> {
    let (id, what) = rest
        .split_once('/')
        .ok_or_else(|| format!("unknown fleet path {rest}"))?;
    let session = SessionId::new(id).map_err(err)?;
    let limit = parameter(query, "limit")
        .and_then(|text| text.parse::<u32>().ok())
        .unwrap_or(50)
        .min(500);
    let store = open_store()?;
    match what {
        "judgements" => store.judgements_json(session.as_str(), limit).map_err(err),
        "commands" => store.commands_json(session.as_str(), limit).map_err(err),
        other => Err(format!("unknown fleet view {other}")),
    }
}

/// The tail of a session's terminal, read from tmux: a live peek at compute,
/// not storage. Capturing a pane changes nothing, so the API stays read-only.
fn pane(id: &str) -> Result<Value, String> {
    let store = open_store()?;
    let session = SessionId::new(id).map_err(err)?;
    let latest = store.executions(&session).map_err(err)?.into_iter().next();
    let Some(execution) = latest else {
        return Ok(
            json!({ "live": false, "terminal": null, "captured_at_ms": now_ms(), "text": "" }),
        );
    };
    let Some(agent) = store
        .state(&session)
        .map_err(err)?
        .map(|state| state.agent_id.to_string())
        .or_else(|| (id == RSI_SESSION).then(|| RSI_AGENT.to_string()))
    else {
        return Err(format!("{id} has no owner"));
    };
    let live = execution.ended_at.is_none();
    let terminal = TerminalName::for_execution(&execution.execution_id);
    let root = env_or("SPAWN_WORKSPACE_ROOT", "/tmp");
    let workspace = WorkspaceId::new(format!("{agent}:{root}/{agent}")).map_err(err)?;
    let program = env_or("SPAWN_TMUX_PROGRAM", "tmux");
    let socket = env_value("SPAWN_TMUX_SOCKET");
    let host = match env_value("SPAWN_EXEC_IMAGE") {
        Some(_) => {
            let docker = env_or("SPAWN_DOCKER_PROGRAM", "docker");
            TmuxHost::in_workspaces(
                &program,
                std::sync::Arc::new(move |workspace: &WorkspaceId| {
                    docker_prefix(&docker, workspace.sandbox())
                }),
                socket,
            )
        }
        None => TmuxHost::in_workspaces(
            &program,
            std::sync::Arc::new(|_: &WorkspaceId| Vec::new()),
            socket,
        ),
    };
    let text = if live {
        host.capture_tail(&terminal, &workspace, PANE_LINES)
            .unwrap_or_default()
    } else {
        String::new()
    };
    let mut lines: Vec<&str> = text.lines().collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    Ok(json!({
        "live": live && !lines.is_empty(),
        "terminal": terminal.as_str(),
        "captured_at_ms": now_ms(),
        "text": lines.join("\n"),
    }))
}

fn session(id: &str, query: &str) -> Result<Value, String> {
    let session = SessionId::new(id).map_err(|error| error.to_string())?;
    let after = parameter(query, "after")
        .and_then(|text| text.parse::<u64>().ok())
        .unwrap_or(0);
    let limit = parameter(query, "limit")
        .and_then(|text| text.parse::<usize>().ok())
        .unwrap_or(DEFAULT_LIMIT)
        .min(MAX_LIMIT);
    let store = open_store()?;
    let state = store.state(&session).map_err(|error| error.to_string())?;
    let executions: Vec<Value> = store
        .executions(&session)
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|row| {
            json!({
                "execution_id": row.execution_id.as_str(),
                "command_id": row.command_id.map(|id| id.0),
                "engine": row.engine,
                "started_at_ms": row.started_at.0,
                "ended_at_ms": row.ended_at.map(|time| time.0),
                "cause": row.cause,
                "exit_code": row.exit_code,
                "exit_reported": row.exit_reported,
                "duration_ms": row.duration_ms,
            })
        })
        .collect();
    let open_tools: Vec<Value> = store
        .open_tools(&session)
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|tool| {
            json!({
                "execution_id": tool.execution_id.map(|id| id.as_str().to_string()),
                "tool_use_id": tool.tool_use_id,
                "tool_name": tool.tool_name,
                "session_seq": tool.session_seq.0,
            })
        })
        .collect();
    let freshness = store
        .freshness(&session)
        .map_err(|error| error.to_string())?
        .map(|fresh| {
            json!({
                "last_session_seq": fresh.last_session_seq.0,
                "last_event_id": fresh.last_event_id.0,
                "last_event_at_ms": fresh.last_event_at.0,
                "event_count": fresh.event_count,
            })
        });
    let timeline: Vec<Value> = store
        .timeline(&session, SessionSeq(after), limit)
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|entry| {
            json!({
                "session_seq": entry.session_seq.0,
                "event_id": entry.event_id.0,
                "execution_id": entry.execution_id.map(|id| id.as_str().to_string()),
                "source": entry.source.as_str(),
                "event_name": entry.event_name,
                "detail": entry.detail,
                "recorded_at_ms": entry.recorded_at.0,
            })
        })
        .collect();
    Ok(json!({
        "session_id": session.as_str(),
        "state": state.map(|state| json!({
            "agent_id": state.agent_id.as_str(),
            "observation": state.observation,
            "session_seq": state.session_seq.0,
            "recorded_at_ms": state.recorded_at.0,
        })),
        "executions": executions,
        "open_tools": open_tools,
        "freshness": freshness,
        "timeline": timeline,
    }))
}

fn parameter<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name).then_some(value)
    })
}

fn static_file(request: Request, web: &Path, path: &str) -> Result<(), String> {
    let relative = match path {
        "/" => "index.html".to_string(),
        other => other.trim_start_matches('/').to_string(),
    };
    // A request path never escapes the built directory.
    if relative.contains("..") {
        return request
            .respond(Response::empty(404))
            .map_err(|error| error.to_string());
    }
    let file = web.join(&relative);
    // `..` is refused above; a symlink inside the built directory could still
    // point outside it, so the resolved path must stay under the root.
    let inside = match (std::fs::canonicalize(&file), std::fs::canonicalize(web)) {
        (Ok(file), Ok(root)) => file.starts_with(root),
        _ => false,
    };
    if !inside {
        return request
            .respond(Response::empty(404))
            .map_err(|error| error.to_string());
    }
    let Ok(bytes) = std::fs::read(&file) else {
        return request
            .respond(Response::empty(404))
            .map_err(|error| error.to_string());
    };
    let header = Header::from_bytes("Content-Type", content_type(&relative))
        .expect("static header is valid");
    request
        .respond(Response::from_data(bytes).with_header(header))
        .map_err(|error| error.to_string())
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("json") => "application/json; charset=utf-8",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}
