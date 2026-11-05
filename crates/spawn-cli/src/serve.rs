//! Read-only HTTP API and static files for the observer UI.
//!
//! The UI never writes. Every endpoint reads an existing view through the
//! same store methods the CLI uses, so a defect in the UI cannot change
//! state.
//!
//! Reading is still worth guarding: `/api/pane/<id>` shows any live pane.
//! `serve` binds 127.0.0.1 unless told otherwise, and on any other address
//! it refuses to start without `SPAWN_SERVE_TOKEN`. With a token set, every
//! API request must carry it (`Authorization: Bearer <token>` or
//! `?token=<token>`), except from loopback when `SPAWN_SERVE_LOCAL_OPEN=1`.
//! The built UI's static files are served without it:
//! they are the public build and hold no data, and a browser fetches them
//! without headers.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use spawn_core::event::SessionSeq;
use spawn_core::id::SessionId;
use tiny_http::{Header, Request, Response, Server};

use spawn_core::id::{TerminalName, WorkspaceId};
use spawn_core::port::TerminalHost;

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
    let token = access_token(&address, env_value("SPAWN_SERVE_TOKEN"))?;
    let server = Server::http(&address).map_err(|error| format!("listen {address}: {error}"))?;
    let guard = if token.is_some() { "token" } else { "none" };
    // The bound address, not the requested one: with port 0 the kernel picks.
    println!(
        "serve: http://{} web {} auth {guard}",
        server.server_addr(),
        web.display()
    );
    for request in server.incoming_requests() {
        if let Err(error) = handle(request, &web, token.as_deref()) {
            eprintln!("serve: {error}");
        }
    }
    Ok(0)
}

/// The token every API request must carry, if any: whatever
/// `SPAWN_SERVE_TOKEN` holds. An address that is not loopback needs one.
fn access_token(address: &str, token: Option<String>) -> Result<Option<String>, String> {
    if token.is_none() && !is_loopback(address) {
        return Err(format!(
            "refusing to serve on {address} without SPAWN_SERVE_TOKEN; \
             bind 127.0.0.1 or set a token"
        ));
    }
    Ok(token)
}

/// Whether every address `address` resolves to is a loopback one. An
/// address that does not resolve counts as public.
/// Whether loopback clients may read without the token
/// (`SPAWN_SERVE_LOCAL_OPEN=1`).
fn local_open() -> bool {
    std::env::var("SPAWN_SERVE_LOCAL_OPEN").is_ok_and(|value| value == "1")
}

fn is_loopback(address: &str) -> bool {
    let Ok(resolved) = std::net::ToSocketAddrs::to_socket_addrs(address) else {
        return false;
    };
    let resolved: Vec<std::net::SocketAddr> = resolved.collect();
    !resolved.is_empty() && resolved.iter().all(|socket| socket.ip().is_loopback())
}

/// Whether `request` carries `token`, as a bearer header or `?token=`.
fn authorized(request: &Request, query: &str, token: &str) -> bool {
    let header = request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Authorization"))
        .and_then(|header| {
            let (scheme, rest) = header.value.as_str().split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then(|| rest.trim())
        });
    [header, parameter(query, "token")]
        .into_iter()
        .flatten()
        .any(|given| same_secret(given.as_bytes(), token.as_bytes()))
}

/// Byte equality whose time does not depend on where the inputs differ.
fn same_secret(given: &[u8], expected: &[u8]) -> bool {
    given.len() == expected.len()
        && given
            .iter()
            .zip(expected)
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
}

/// The dashboard's connection: the read-only `spawn_reader` login, never the
/// controller's.
fn open_store() -> Result<spawn_postgres::PostgresStore, String> {
    let controller = env_value("SPAWN_DATABASE_URL")
        .ok_or_else(|| "SPAWN_DATABASE_URL is not set".to_string())?;
    let url = match env_value("SPAWN_SERVE_DATABASE_URL") {
        Some(url) => url,
        None => spawn_postgres::role_url(&controller, "spawn_reader")?,
    };
    spawn_postgres::PostgresStore::connect(&url).map_err(|error| error.to_string())
}

fn handle(request: Request, web: &Path, token: Option<&str>) -> Result<(), String> {
    let url = request.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((path, query)) => (path, query),
        None => (url.as_str(), ""),
    };
    // With SPAWN_SERVE_LOCAL_OPEN=1 a browser on this machine needs no
    // token: anything that reaches loopback here runs as the owner's uid and
    // can read the token file anyway. Off by default.
    let local = local_open()
        && request
            .remote_addr()
            .is_some_and(|address| address.ip().is_loopback());
    if let Some(token) = token
        && path.starts_with("/api/")
        && !local
        && !authorized(&request, query, token)
    {
        let challenge =
            Header::from_bytes("WWW-Authenticate", "Bearer").expect("static header is valid");
        return request
            .respond(json_response(401, &json!({ "error": "unauthorized" })).with_header(challenge))
            .map_err(|error| error.to_string());
    }
    let answer = match path {
        "/api/overview" => overview(),
        "/api/rsi" => rsi_overview(),
        "/api/rsi/pane" => rsi_pane(query),
        "/api/fleet" => fleet(),
        "/api/missions" => missions(),
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

/// `/api/missions`: the board, each task with its stages from the store
/// (`stages`: `[{stage, verdict?, at_ms, took_ms?}]`, oldest first) and its
/// last gate verdict (`last_gate`: `{verdict, reason, metric, candidate,
/// at_ms}` or null).
fn missions() -> Result<Value, String> {
    let store = open_store()?;
    let mut board = store.mission_board(50).map_err(err)?;
    let gates = store.last_gates().map_err(err)?;
    with_last_gates(&mut board, &gates);
    Ok(board)
}

/// Put each task's last gate verdict from `gates` (keyed by task id) on the
/// board.
fn with_last_gates(board: &mut Value, gates: &Value) {
    let Some(missions) = board.get_mut("missions").and_then(Value::as_array_mut) else {
        return;
    };
    for task in missions
        .iter_mut()
        .filter_map(|mission| mission.get_mut("tasks").and_then(Value::as_array_mut))
        .flatten()
    {
        let gate = task["id"]
            .as_i64()
            .and_then(|id| gates.get(id.to_string()))
            .cloned()
            .unwrap_or(Value::Null);
        if let Some(fields) = task.as_object_mut() {
            fields.insert("last_gate".to_string(), gate);
        }
    }
}

/// The RSI page (docs/fleet-architecture.md §10.4): every open objective
/// mission, read from the mission tables, the control stream and the
/// mission's backlog.
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
/// every gate verdict.
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
    let objectives = mission["objectives"]
        .as_array()
        .cloned()
        .unwrap_or_default();
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
/// fleet health and the decision ladder. Computed by SQL from storage,
/// except the health's `disk`: free space read now ([`crate::disk::report`]).
fn fleet() -> Result<Value, String> {
    let store = open_store()?;
    let mut health = store.fleet_health().map_err(err)?;
    health["disk"] = crate::disk::report(&store.host_disk().map_err(err)?);
    Ok(json!({
        "agents": store.fleet_board().map_err(err)?,
        "health": health,
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
    let host = crate::config::terminal_host();
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        access_token, is_loopback, objective_json, rsi_summary, same_secret, with_last_gates,
    };
    use crate::objectives::Task;

    fn objective(id: u32, recurring: bool) -> Task {
        Task {
            id,
            title: format!("objective {id}"),
            prompt: "do it".to_string(),
            recurring,
            metric: recurring.then(|| "ingest_total_ns".to_string()),
            unit: "ns".to_string(),
            lower_is_better: true,
            target: recurring.then_some(1100.0),
        }
    }

    fn task(
        id: i64,
        objective: u32,
        state: &str,
        at: i64,
        metric: Option<&str>,
    ) -> serde_json::Value {
        json!({"id": id, "objective": objective, "state": state, "updated_at_ms": at,
               "metric": metric, "revisions": 0, "gates": []})
    }

    #[test]
    fn objectives_read_their_status_from_landed_tasks() {
        let tasks = [
            task(13, 2, "revise", 40, None),
            task(
                12,
                2,
                "landed",
                30,
                Some("METRIC ingest_total_ns baseline=1200 candidate=1021"),
            ),
            task(
                11,
                2,
                "landed",
                20,
                Some("METRIC ingest_total_ns baseline=1573 candidate=1200"),
            ),
            task(10, 1, "landed", 10, None),
        ];
        let one_shot = objective_json(&objective(1, false), &tasks);
        assert_eq!(one_shot["status"], "solved");
        let recurring = objective_json(&objective(2, true), &tasks);
        assert_eq!(recurring["status"], "met");
        assert_eq!(recurring["current"], 1021.0);
        assert_eq!(recurring["tasks"], json!([13, 12, 11]));
        let landings = recurring["landings"].as_array().expect("landings");
        assert_eq!(landings.len(), 2);
        // Oldest first: the chart reads them in order.
        assert_eq!(landings[0]["task"], 11);
        assert_eq!(landings[0]["baseline"], 1573.0);
        assert_eq!(landings[1]["candidate"], 1021.0);
        let untouched = objective_json(&objective(3, false), &tasks);
        assert_eq!(untouched["status"], "open");
        assert_eq!(untouched["landings"], json!([]));
    }

    #[test]
    fn a_recurring_objective_above_its_target_stays_open() {
        let tasks = [task(
            11,
            2,
            "landed",
            20,
            Some("METRIC ingest_total_ns baseline=1573 candidate=1200"),
        )];
        let recurring = objective_json(&objective(2, true), &tasks);
        assert_eq!(recurring["status"], "open");
        assert_eq!(recurring["current"], 1200.0);
    }

    #[test]
    fn the_board_gets_each_tasks_last_gate_and_keeps_its_stages() {
        let stages = json!([{"stage": "assigned", "at_ms": 1},
                            {"stage": "delivered", "at_ms": 3_385_001, "took_ms": 3_385_000}]);
        let mut board = json!({"missions": [{"tasks": [{"id": 7, "stages": stages}, {"id": 8}]}]});
        with_last_gates(&mut board, &json!({"7": {"verdict": "accepted"}}));
        assert_eq!(
            board["missions"][0]["tasks"][0]["last_gate"]["verdict"],
            "accepted"
        );
        assert_eq!(board["missions"][0]["tasks"][0]["stages"], stages);
        assert!(board["missions"][0]["tasks"][1]["last_gate"].is_null());
    }

    #[test]
    fn only_a_loopback_address_serves_without_a_token() {
        assert!(is_loopback("127.0.0.1:8787"));
        assert!(is_loopback("[::1]:8787"));
        assert!(!is_loopback("0.0.0.0:8787"));
        assert!(!is_loopback("[::]:8787"));
        assert!(!is_loopback("192.168.1.10:8787"));
        assert!(!is_loopback("not an address"));
        assert_eq!(access_token("127.0.0.1:8787", None), Ok(None));
        assert!(access_token("0.0.0.0:8787", None).is_err());
        assert_eq!(
            access_token("0.0.0.0:8787", Some("t".to_string())),
            Ok(Some("t".to_string()))
        );
    }

    #[test]
    fn secrets_match_only_when_equal() {
        assert!(same_secret(b"abc", b"abc"));
        assert!(!same_secret(b"abd", b"abc"));
        assert!(!same_secret(b"ab", b"abc"));
        assert!(!same_secret(b"", b"abc"));
    }

    #[test]
    fn the_summary_names_the_open_task() {
        let mission = json!({
            "id": 4, "halted": null, "baseline": "abc",
            "objectives": [{"status": "open"}, {"status": "met"}, {"status": "solved"}],
            "tasks": [
                {"id": 13, "state": "revise", "objective": 3, "revisions": 1,
                 "session_id": "session_resident_rsi", "gates": [{"verdict": "rejected"}]},
                task(12, 2, "landed", 30, None),
            ],
        });
        let summary = rsi_summary(&mission);
        assert_eq!(summary["mission"], 4);
        assert_eq!(summary["open_task"]["id"], 13);
        assert_eq!(summary["open_task"]["last_verdict"], "rejected");
        assert_eq!(
            summary["objectives"],
            json!({"open": 1, "solved": 1, "met": 1})
        );
    }
}
