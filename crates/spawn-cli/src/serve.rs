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

use crate::objectives::{Landing, Task, landed_value, load_backlog, metric_lines};
use crate::{Flags, env_or, env_value};

/// Prefix of an error that answers 400.
const BAD_REQUEST: &str = "bad request: ";

/// A session id from a URL, or a 400.
fn session_id(id: &str) -> Result<SessionId, String> {
    SessionId::new(id).map_err(|error| format!("{BAD_REQUEST}{error}"))
}

/// Gate rejections (or aborts) in a row that halt a mission, mirrored from
/// the integrator for display.
const HALT_AFTER: u32 = 3;

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
        "/api/rsi/pane" => rsi_pane(query),
        "/api/fleet" => fleet(),
        "/api/missions" => open_store().and_then(|store| store.mission_board(50).map_err(err)),
        _ => {
            if let Some(id) = path.strip_prefix("/api/session/") {
                session(id, query)
            } else if let Some(task) = path.strip_prefix("/api/rsi/task/") {
                rsi_task(task)
            } else if let Some(id) = path.strip_prefix("/api/pane/") {
                pane(id, None)
            } else if let Some(rest) = path.strip_prefix("/api/fleet/") {
                fleet_session(rest, query)
            } else {
                return static_file(request, web, path);
            }
        }
    };
    let (status, body) = match answer {
        Ok(body) => (200, body),
        // A malformed identifier is the caller's mistake, not the server's.
        Err(error) if error.starts_with(BAD_REQUEST) => (400, json!({ "error": error })),
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
    let rsi = objective_missions(&store)?.first().map(rsi_summary);
    Ok(json!({ "sessions": sessions, "rsi": rsi }))
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

/// The RSI page (docs/fleet-architecture.md §10.4): every open objective
/// mission, read from the mission tables, the control stream and the
/// mission's backlog. Nothing here reads the retired controller's `rsi_*`
/// tables, which stay in the database as history.
///
/// `GET /api/rsi` answers:
///
/// ```text
/// {
///   "generated_at_ms": 1793766328633,
///   "halt_after": 3,                      // rejections (or aborts) in a row that halt
///   "missions": [{                        // open objective missions, by id
///     "id": 4, "text": "...", "state": "open", "sources": ["..."],
///     "gate": "/.../rsi/scenarios", "accept": "gate",
///     "budget": {"minutes": 45, "tool_calls": 60} | null,
///     "baseline": "<commit>" | null,      // where the last landing left main
///     "halted": "<reason>" | null,        // set until a human resumes
///     "scenario_floor": 7, "gate_rejections": 0, "gate_aborts": 0,
///     "backlog": "/.../rsi/backlog" | null,
///     "backlog_error": "<why it could not be read>" | null,
///     "resident": {"name", "agent_id", "session_id", "state"} | null,
///     "next_objective": 3 | null,         // what the integrator assigns next
///     "objectives": [{                    // the backlog, by id
///       "id": 2, "title": "...", "prompt": "...", "recurring": true,
///       "metric": "ingest_total_ns" | null, "unit": "ns",
///       "direction": "lower" | "higher", "target": 1100 | null,
///       "status": "open" | "solved" | "met",
///       "current": 1021 | null,           // latest landed metric value
///       "tasks": [10, 14],                // task ids for this objective
///       "landings": [{                    // landed tasks, oldest first
///         "task": 10, "at_ms": 1793724858211,
///         "metric": "METRIC ingest_total_ns baseline=1573 candidate=1021" | null,
///         "baseline": 1573 | null, "candidate": 1021 | null
///       }]
///     }],
///     "tasks": [{                         // newest first
///       "id", "kind", "agent_id", "session_id", "state", "revisions",
///       "objective", "candidate", "base", "metric", "scenario", "brief",
///       "review_note", "created_by", "created_at_ms", "delivered_at_ms",
///       "updated_at_ms",
///       "gates": [{                       // task.gated records, oldest first
///         "verdict": "accepted" | "rejected" | "aborted" | "conflict" | "stale",
///         "reason": "..." | null, "metric": "METRIC ..." | null,
///         "candidate": "<commit>" | null, "at_ms": 1793724858000
///       }]
///     }]
///   }]
/// }
/// ```
///
/// `GET /api/rsi/task/<id>` answers one task: the fields above plus the full
/// `brief`, `report`, `gate_log` (the latest), `mission_id`, and `log` on
/// every gate verdict. It replaces the retired `/api/rsi/iteration/<n>`.
///
/// `GET /api/rsi/pane[?mission=<id>]` answers the live pane of the mission's
/// resident (the first open objective mission by default), in the shape of
/// `/api/pane/<session>` plus `session_id`.
fn rsi_overview() -> Result<Value, String> {
    let store = open_store()?;
    Ok(json!({
        "generated_at_ms": now_ms(),
        "halt_after": HALT_AFTER,
        "missions": objective_missions(&store)?,
    }))
}

/// Open objective missions with their backlog's objectives filled in.
fn objective_missions(store: &spawn_postgres::PostgresStore) -> Result<Vec<Value>, String> {
    let missions = store.objective_missions().map_err(err)?;
    let Value::Array(missions) = missions else {
        return Ok(Vec::new());
    };
    Ok(missions.into_iter().map(with_objectives).collect())
}

/// One mission row with `backlog`, `backlog_error`, `objectives` and
/// `next_objective` added.
fn with_objectives(mut mission: Value) -> Value {
    let backlog = mission
        .pointer("/objectives/backlog")
        .and_then(Value::as_str)
        .map(str::to_string);
    let (objectives, error) = match &backlog {
        Some(path) => match load_backlog(Path::new(path)) {
            Ok(objectives) => (objectives, None),
            Err(error) => (Vec::new(), Some(error)),
        },
        None => (Vec::new(), Some("the mission names no backlog".to_string())),
    };
    let tasks = mission
        .get("tasks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let rows: Vec<Value> = objectives
        .iter()
        .map(|objective| objective_json(objective, &tasks))
        .collect();
    let next = rows
        .iter()
        .find(|row| row["status"] == "open")
        .map(|row| row["id"].clone());
    if let Some(fields) = mission.as_object_mut() {
        fields.insert("backlog".to_string(), json!(backlog));
        fields.insert("backlog_error".to_string(), json!(error));
        fields.insert("next_objective".to_string(), next.unwrap_or(Value::Null));
        fields.insert("objectives".to_string(), Value::Array(rows));
    }
    mission
}

/// One backlog objective with its status and landings, from the mission's
/// tasks (newest first).
fn objective_json(objective: &Task, tasks: &[Value]) -> Value {
    let mine: Vec<&Value> = tasks
        .iter()
        .filter(|task| task["objective"].as_u64() == Some(u64::from(objective.id)))
        .collect();
    let mut landed: Vec<&Value> = mine
        .iter()
        .copied()
        .filter(|task| task["state"] == "landed")
        .collect();
    // Newest first, as `landed_value` reads them.
    landed.sort_by_key(|task| {
        std::cmp::Reverse((task["updated_at_ms"].as_i64(), task["id"].as_i64()))
    });
    let landings: Vec<Landing> = landed
        .iter()
        .map(|task| Landing {
            objective: objective.id,
            metric: task["metric"].as_str().map(str::to_string),
        })
        .collect();
    let current = landed_value(objective, &landings);
    let status = if objective.recurring {
        if current.is_some_and(|value| objective.meets(value)) {
            "met"
        } else {
            "open"
        }
    } else if landed.is_empty() {
        "open"
    } else {
        "solved"
    };
    let history: Vec<Value> = landed
        .iter()
        .rev()
        .map(|task| {
            let line = task["metric"].as_str();
            let metric = line.and_then(|line| {
                let mut metrics = metric_lines(line);
                match &objective.metric {
                    Some(name) => metrics.into_iter().find(|metric| &metric.name == name),
                    None => metrics.pop(),
                }
            });
            json!({
                "task": task["id"],
                "at_ms": task["updated_at_ms"],
                "metric": line,
                "baseline": metric.as_ref().and_then(|metric| metric.baseline),
                "candidate": metric.as_ref().and_then(|metric| metric.candidate),
            })
        })
        .collect();
    json!({
        "id": objective.id,
        "title": objective.title,
        "prompt": objective.prompt,
        "recurring": objective.recurring,
        "metric": objective.metric,
        "unit": objective.unit,
        "direction": if objective.lower_is_better { "lower" } else { "higher" },
        "target": objective.target,
        "status": status,
        "current": current,
        "tasks": mine.iter().map(|task| task["id"].clone()).collect::<Vec<_>>(),
        "landings": history,
    })
}

/// The RSI line of `/api/overview`: the first open objective mission, its
/// open task and how many objectives are settled.
fn rsi_summary(mission: &Value) -> Value {
    let objectives = mission["objectives"].as_array().cloned().unwrap_or_default();
    let count = |status: &str| {
        objectives
            .iter()
            .filter(|objective| objective["status"] == status)
            .count()
    };
    let open_task = mission["tasks"].as_array().and_then(|tasks| {
        tasks
            .iter()
            .find(|task| {
                !matches!(
                    task["state"].as_str(),
                    Some("landed" | "cancelled" | "accepted") | None
                )
            })
            .map(|task| {
                json!({
                    "id": task["id"],
                    "state": task["state"],
                    "objective": task["objective"],
                    "revisions": task["revisions"],
                    "session_id": task["session_id"],
                    "last_verdict": task["gates"]
                        .as_array()
                        .and_then(|gates| gates.last())
                        .map(|gate| gate["verdict"].clone()),
                })
            })
    });
    json!({
        "mission": mission["id"],
        "halted": mission["halted"],
        "baseline": mission["baseline"],
        "resident": mission["resident"],
        "next_objective": mission["next_objective"],
        "open_task": open_task,
        "objectives": {
            "open": count("open"),
            "solved": count("solved"),
            "met": count("met"),
        },
    })
}

/// One objective task, with its full brief, report and every gate verdict
/// with its log.
fn rsi_task(task: &str) -> Result<Value, String> {
    let id: i64 = task
        .parse()
        .map_err(|_| format!("{BAD_REQUEST}bad task {task}"))?;
    let store = open_store()?;
    store
        .objective_task(id)
        .map_err(err)?
        .ok_or_else(|| format!("task {id} does not exist"))
}

/// The live pane of an objective mission's resident.
fn rsi_pane(query: &str) -> Result<Value, String> {
    let wanted = parameter(query, "mission")
        .map(|text| {
            text.parse::<i64>()
                .map_err(|_| format!("{BAD_REQUEST}bad mission {text}"))
        })
        .transpose()?;
    let store = open_store()?;
    let missions = store.objective_missions().map_err(err)?;
    let resident = missions
        .as_array()
        .into_iter()
        .flatten()
        .find(|mission| wanted.is_none_or(|id| mission["id"].as_i64() == Some(id)))
        .and_then(|mission| mission.get("resident"))
        .filter(|resident| resident.is_object())
        .cloned();
    let Some(resident) = resident else {
        return Ok(json!({ "live": false, "terminal": null, "session_id": null,
                          "captured_at_ms": now_ms(), "text": "" }));
    };
    let session = resident["session_id"].as_str().unwrap_or_default();
    let mut body = pane(session, resident["agent_id"].as_str())?;
    body["session_id"] = json!(session);
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
        .ok_or_else(|| format!("{BAD_REQUEST}unknown fleet path {rest}"))?;
    let session = session_id(id)?;
    let limit = parameter(query, "limit")
        .and_then(|text| text.parse::<u32>().ok())
        .unwrap_or(50)
        .min(500);
    let store = open_store()?;
    match what {
        "judgements" => store.judgements_json(session.as_str(), limit).map_err(err),
        "commands" => store.commands_json(session.as_str(), limit).map_err(err),
        other => Err(format!("{BAD_REQUEST}unknown fleet view {other}")),
    }
}

/// The tail of a session's terminal, read from tmux: a live peek at compute,
/// not storage. Capturing a pane changes nothing, so the API stays read-only.
/// `owner` names the session's agent when the session has no state yet (a
/// resident before its first event).
fn pane(id: &str, owner: Option<&str>) -> Result<Value, String> {
    let store = open_store()?;
    let session = session_id(id)?;
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
        .or_else(|| owner.map(str::to_string))
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
    let session = session_id(id)?;
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
