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
//! `?token=<token>`), except, with `SPAWN_SERVE_LOCAL_OPEN=1`, from a loopback
//! client that runs as the owner's uid (Linux: the uid of its socket in
//! `/proc/net/tcp{,6}`; without `/proc`, no client is exempt).
//! An API read served without the token (no token set, or the local-open
//! exemption) also needs a `Host` naming this server (127.0.0.1, localhost,
//! [::1] or the bound address, with its port) and no cross-origin `Origin`:
//! a page that rebinds its own name to 127.0.0.1, or a proxy on this machine
//! that forwards another name here, reads nothing without the token.
//! The built UI's static files are served without it:
//! they are the public build and hold no data, and a browser fetches them
//! without headers.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

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
    let port = server.server_addr().to_ip().map(|address| address.port());
    let hosts = port
        .map(|port| server_hosts(&address, port))
        .unwrap_or_default();
    let shared = Arc::new(Shared {
        web,
        token,
        port,
        hosts,
    });
    serve_requests(&server, &shared, MAX_IN_FLIGHT);
    Ok(0)
}

/// Requests served at once. Each runs on its own thread, so a client that
/// stops reading, a slow query or a slow pane holds one of these, never the
/// server; one past them is answered 503 at once.
const MAX_IN_FLIGHT: usize = 16;

/// Longest one dashboard statement runs on the reader's connection.
const STATEMENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// What every request's handler reads.
struct Shared {
    web: PathBuf,
    token: Option<String>,
    port: Option<u16>,
    hosts: Vec<String>,
}

/// Requests in flight, at most `limit`.
struct Slots {
    taken: AtomicUsize,
    limit: usize,
}

/// One request's place among [`Slots`], given back when it is dropped.
struct Slot(Arc<Slots>);

impl Slots {
    fn take(slots: &Arc<Self>) -> Option<Slot> {
        slots
            .taken
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |taken| {
                (taken < slots.limit).then_some(taken + 1)
            })
            .ok()
            .map(|_| Slot(Arc::clone(slots)))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.taken.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Serve `server`'s requests until it closes: each on a thread of its own,
/// at most `limit` at once, and 503 past them.
fn serve_requests(server: &Server, shared: &Arc<Shared>, limit: usize) {
    let slots = Arc::new(Slots {
        taken: AtomicUsize::new(0),
        limit,
    });
    for request in server.incoming_requests() {
        let Some(slot) = Slots::take(&slots) else {
            let busy = json_response(503, &json!({ "error": "busy: too many requests at once" }));
            if let Err(error) = request.respond(busy) {
                eprintln!("serve: {error}");
            }
            continue;
        };
        let shared = Arc::clone(shared);
        let spawned = std::thread::Builder::new()
            .name("serve-request".to_string())
            .spawn(move || {
                let _slot = slot;
                if let Err(error) = handle(
                    request,
                    &shared.web,
                    shared.token.as_deref(),
                    shared.port,
                    &shared.hosts,
                ) {
                    eprintln!("serve: {error}");
                }
            });
        if let Err(error) = spawned {
            eprintln!("serve: no thread for a request: {error}");
        }
    }
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

/// Whether the owner's own loopback clients may read without the token
/// (`SPAWN_SERVE_LOCAL_OPEN=1`).
fn local_open() -> bool {
    std::env::var("SPAWN_SERVE_LOCAL_OPEN").is_ok_and(|value| value == "1")
}

/// `address` with an IPv4-mapped IPv6 address written as the IPv4 one, so
/// a dual-stack listener's peer compares with the kernel's IPv4 tables.
fn plain(address: SocketAddr) -> SocketAddr {
    match address {
        SocketAddr::V6(v6) => v6.ip().to_ipv4_mapped().map_or(address, |v4| {
            SocketAddr::new(std::net::IpAddr::V4(v4), v6.port())
        }),
        SocketAddr::V4(_) => address,
    }
}

/// One TCP socket of `/proc/net/tcp` or `/proc/net/tcp6`: its local and
/// remote address and the uid that owns it.
#[derive(Debug, PartialEq, Eq)]
struct ProcSocket {
    local: SocketAddr,
    remote: SocketAddr,
    uid: u32,
}

/// An address as `/proc/net/tcp{,6}` writes it: the address in hex, each
/// 32-bit word in the host's byte order, a colon, the port in hex.
fn proc_address(text: &str) -> Option<SocketAddr> {
    let (address, port) = text.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    let word = |hex: &str| u32::from_str_radix(hex, 16).ok().map(u32::to_ne_bytes);
    let ip = match address.len() {
        8 => std::net::IpAddr::from(word(address)?),
        32 => {
            let mut bytes = [0u8; 16];
            for (index, chunk) in bytes.chunks_mut(4).enumerate() {
                chunk.copy_from_slice(&word(address.get(index * 8..index * 8 + 8)?)?);
            }
            std::net::IpAddr::from(bytes)
        }
        _ => return None,
    };
    Some(plain(SocketAddr::new(ip, port)))
}

/// The sockets of one `/proc/net/tcp{,6}` table: past the header line,
/// fields `sl local_address rem_address st tx:rx tr:when retrnsmt uid ...`.
/// A line that does not parse is skipped.
fn proc_sockets(table: &str) -> Vec<ProcSocket> {
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            Some(ProcSocket {
                local: proc_address(fields.get(1)?)?,
                remote: proc_address(fields.get(2)?)?,
                uid: fields.get(7)?.parse().ok()?,
            })
        })
        .collect()
}

/// The uid that owns the client end of a connection from `peer` to this
/// server's `port`: the socket whose local address is `peer` and whose
/// remote port is `port`, in the kernel's `tables`.
fn peer_uid(peer: SocketAddr, port: u16, tables: &[String]) -> Option<u32> {
    let peer = plain(peer);
    tables
        .iter()
        .flat_map(|table| proc_sockets(table))
        .find(|socket| socket.local == peer && socket.remote.port() == port)
        .map(|socket| socket.uid)
}

/// The uid this server runs as: the owner of `/proc/self`. Without `/proc`,
/// none.
fn owner_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").ok().map(|meta| meta.uid())
}

/// Whether a request from `peer` reads without the token: only with
/// `SPAWN_SERVE_LOCAL_OPEN=1` (`enabled`), from loopback, and from a socket
/// the owner's uid holds. Another user on this machine reaches loopback too;
/// a peer whose uid is unknown (no `/proc`, no matching socket) is not
/// exempt.
fn local_exemption(
    enabled: bool,
    peer: Option<SocketAddr>,
    peer_uid: Option<u32>,
    owner: Option<u32>,
) -> bool {
    enabled
        && peer.is_some_and(|peer| plain(peer).ip().is_loopback())
        && owner.is_some()
        && peer_uid == owner
}

/// [`local_exemption`] for a request from `peer` to this server's `port`,
/// with the uids read from `/proc`.
fn owners_own_request(peer: Option<SocketAddr>, port: Option<u16>) -> bool {
    if !local_open() {
        return false;
    }
    let tables: Vec<String> = ["/proc/net/tcp", "/proc/net/tcp6"]
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .collect();
    let uid = peer
        .zip(port)
        .and_then(|(peer, port)| peer_uid(peer, port, &tables));
    local_exemption(true, peer, uid, owner_uid())
}

/// Whether every address `address` resolves to is a loopback one. An
/// address that does not resolve counts as public.
fn is_loopback(address: &str) -> bool {
    let Ok(resolved) = std::net::ToSocketAddrs::to_socket_addrs(address) else {
        return false;
    };
    let resolved: Vec<std::net::SocketAddr> = resolved.collect();
    !resolved.is_empty() && resolved.iter().all(|socket| socket.ip().is_loopback())
}

/// The `Host` values that name this server on `port`: the loopback names,
/// and the address it was told to bind unless that is a wildcard. Each is
/// lowercase `host:port`.
fn server_hosts(address: &str, port: u16) -> Vec<String> {
    let mut hosts: Vec<String> = ["127.0.0.1", "localhost", "[::1]"]
        .iter()
        .map(|host| format!("{host}:{port}"))
        .collect();
    let bound = address
        .rsplit_once(':')
        .map_or(address, |(host, _)| host)
        .to_ascii_lowercase();
    let wildcard = bound
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_unspecified());
    if !bound.is_empty() && !wildcard {
        let named = format!("{bound}:{port}");
        if !hosts.contains(&named) {
            hosts.push(named);
        }
    }
    hosts
}

/// `authority` (a `Host` value, or an origin without its scheme) as
/// lowercase `host:port`, port 80 when it names none.
fn authority(text: &str) -> String {
    let text = text.trim().to_ascii_lowercase();
    let has_port = match text.rfind(']') {
        Some(close) => text[close..].contains(':'),
        None => text.contains(':'),
    };
    if has_port { text } else { format!("{text}:80") }
}

/// Whether a request with these `Host` and `Origin` headers may be served
/// without the token: its `Host` names this server (`hosts`, from
/// [`server_hosts`]) and its `Origin`, if any, is that same host over http.
/// A page that rebinds its own name to 127.0.0.1 sends its own name as the
/// `Host`; a page on another origin sends that origin.
fn tokenless_request(host: Option<&str>, origin: Option<&str>, hosts: &[String]) -> bool {
    let Some(host) = host.map(authority) else {
        return false;
    };
    if !hosts.contains(&host) {
        return false;
    }
    origin.is_none_or(|origin| {
        origin
            .trim()
            .strip_prefix("http://")
            .is_some_and(|rest| authority(rest) == host)
    })
}

/// The value of `request`'s header `name`, if it has one.
fn header<'a>(request: &'a Request, name: &'static str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv(name))
        .map(|header| header.value.as_str())
}

/// Whether `request` carries `token`, as a bearer header or `?token=`.
fn authorized(request: &Request, query: &str, token: &str) -> bool {
    let bearer = header(request, "Authorization").and_then(|value| {
        let (scheme, rest) = value.split_once(' ')?;
        scheme.eq_ignore_ascii_case("bearer").then(|| rest.trim())
    });
    [bearer, parameter(query, "token")]
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
    spawn_postgres::PostgresStore::connect_with_statement_timeout(&url, STATEMENT_TIMEOUT)
        .map_err(|error| error.to_string())
}

fn handle(
    request: Request,
    web: &Path,
    token: Option<&str>,
    port: Option<u16>,
    hosts: &[String],
) -> Result<(), String> {
    let url = request.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((path, query)) => (path, query),
        None => (url.as_str(), ""),
    };
    if path.starts_with("/api/") {
        // Without the token a read needs a Host that names this server and
        // no foreign Origin (DNS rebinding, a local proxy); with it, any.
        // With SPAWN_SERVE_LOCAL_OPEN=1 the owner's browser on this machine
        // needs no token: a client running as the owner's uid can read the
        // token file anyway. Another user's loopback client still needs it.
        // Off by default.
        let named = tokenless_request(header(&request, "Host"), header(&request, "Origin"), hosts);
        let open = match token {
            None => named,
            Some(token) => {
                authorized(&request, query, token)
                    || (named && owners_own_request(request.remote_addr().copied(), port))
            }
        };
        if !open {
            return refuse(request, token.is_some());
        }
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

/// Answer a refused API request: 401 with a bearer challenge when a token
/// would let it in, 403 when no token is set (its Host or Origin is not this
/// server's).
fn refuse(request: Request, token_set: bool) -> Result<(), String> {
    let response = if token_set {
        let challenge =
            Header::from_bytes("WWW-Authenticate", "Bearer").expect("static header is valid");
        json_response(401, &json!({ "error": "unauthorized" })).with_header(challenge)
    } else {
        json_response(
            403,
            &json!({ "error": "forbidden: the Host or Origin does not name this server" }),
        )
    };
    request.respond(response).map_err(|error| error.to_string())
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
    // In a mission with a gate, accepted work still waits for the gate and
    // then lands: the task is open until it landed (or was cancelled).
    let gated = !mission["gate"].is_null();
    let open_task = mission["tasks"].as_array().and_then(|tasks| {
        tasks
            .iter()
            .find(|task| match task["state"].as_str() {
                Some("landed" | "cancelled") | None => false,
                Some("accepted") => gated,
                Some(_) => true,
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
        Arc, ProcSocket, Shared, access_token, handle, is_loopback, local_exemption,
        objective_json, owner_uid, peer_uid, proc_sockets, rsi_summary, same_secret,
        serve_requests, server_hosts, tokenless_request, with_last_gates,
    };
    use crate::objectives::Task;
    use std::net::SocketAddr;

    fn address(text: &str) -> SocketAddr {
        text.parse().expect("a socket address")
    }

    /// `/proc/net/tcp` on a little-endian host: a listener on
    /// 127.0.0.1:8787, another user's client (uid 1001) at port 54321 and
    /// the server's end of that connection (uid 1000), and the owner's own
    /// client at port 54322.
    const TCP: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:2253 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 1313053 1 0000000000000000 100 0 0 10 0
   1: 0100007F:D431 0100007F:2253 01 00000000:00000000 00:00000000 00000000  1001        0 1313100 1 0000000000000000 20 4 30 10 -1
   2: 0100007F:2253 0100007F:D431 01 00000000:00000000 00:00000000 00000000  1000        0 1313101 1 0000000000000000 20 4 30 10 -1
   3: 0100007F:D432 0100007F:2253 01 00000000:00000000 00:00000000 00000000  1000        0 1313102 1 0000000000000000 20 4 30 10 -1
   4: garbage
";

    /// `/proc/net/tcp6`: a client at [::1]:54323 of uid 1001 and one at
    /// [::ffff:127.0.0.1]:54324 of uid 1000, both to port 8787.
    const TCP6: &str = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000001000000:D433 00000000000000000000000001000000:2253 01 00000000:00000000 00:00000000 00000000  1001        0 77 1 0000000000000000 20 4 30 10 -1
   1: 0000000000000000FFFF00000100007F:D434 0000000000000000FFFF00000100007F:2253 01 00000000:00000000 00:00000000 00000000  1000        0 78 1 0000000000000000 20 4 30 10 -1
";

    #[cfg(target_endian = "little")]
    #[test]
    fn proc_net_tcp_rows_parse_into_addresses_and_uids() {
        let sockets = proc_sockets(TCP);
        assert_eq!(
            sockets.len(),
            4,
            "the header and a garbage line are skipped"
        );
        assert_eq!(
            sockets[1],
            ProcSocket {
                local: address("127.0.0.1:54321"),
                remote: address("127.0.0.1:8787"),
                uid: 1001,
            }
        );
        let six = proc_sockets(TCP6);
        assert_eq!(six[0].local, address("[::1]:54323"));
        // An IPv4-mapped address reads as the IPv4 one.
        assert_eq!(six[1].local, address("127.0.0.1:54324"));
        assert_eq!(six[1].uid, 1000);
    }

    #[cfg(target_endian = "little")]
    #[test]
    fn a_peers_uid_is_the_uid_of_its_own_end_of_the_connection() {
        let tables = [TCP.to_string(), TCP6.to_string()];
        assert_eq!(
            peer_uid(address("127.0.0.1:54321"), 8787, &tables),
            Some(1001)
        );
        assert_eq!(
            peer_uid(address("127.0.0.1:54322"), 8787, &tables),
            Some(1000)
        );
        assert_eq!(peer_uid(address("[::1]:54323"), 8787, &tables), Some(1001));
        assert_eq!(
            peer_uid(address("[::ffff:127.0.0.1]:54324"), 8787, &tables),
            Some(1000),
            "a dual-stack listener's IPv4 peer"
        );
        assert_eq!(
            peer_uid(address("127.0.0.1:54321"), 9999, &tables),
            None,
            "a connection to another port"
        );
        assert_eq!(peer_uid(address("127.0.0.1:1"), 8787, &tables), None);
        assert_eq!(
            peer_uid(address("127.0.0.1:54322"), 8787, &[]),
            None,
            "no /proc"
        );
    }

    #[test]
    fn only_the_owners_loopback_client_reads_without_the_token() {
        let loopback = Some(address("127.0.0.1:54322"));
        assert!(local_exemption(true, loopback, Some(1000), Some(1000)));
        assert!(
            !local_exemption(true, loopback, Some(1001), Some(1000)),
            "another user on this machine"
        );
        assert!(
            !local_exemption(true, loopback, None, Some(1000)),
            "uid unknown"
        );
        assert!(
            !local_exemption(true, loopback, None, None),
            "no /proc at all"
        );
        assert!(
            !local_exemption(false, loopback, Some(1000), Some(1000)),
            "off"
        );
        assert!(
            !local_exemption(
                true,
                Some(address("192.168.1.10:5000")),
                Some(1000),
                Some(1000)
            ),
            "not loopback"
        );
        assert!(!local_exemption(true, None, Some(1000), Some(1000)));
    }

    #[test]
    fn this_processs_own_connection_is_found_as_the_owners() {
        let Some(owner) = owner_uid() else {
            return; // No /proc: nothing is exempt, and nothing to find.
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listen");
        let port = listener.local_addr().expect("bound").port();
        let client = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let (_server_end, peer) = listener.accept().expect("accept");
        assert_eq!(peer, client.local_addr().expect("client address"));
        let tables: Vec<String> = ["/proc/net/tcp", "/proc/net/tcp6"]
            .iter()
            .filter_map(|path| std::fs::read_to_string(path).ok())
            .collect();
        assert_eq!(peer_uid(peer, port, &tables), Some(owner));
    }

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
    fn the_server_is_named_by_loopback_and_its_bound_address() {
        let loopback = ["127.0.0.1:8787", "localhost:8787", "[::1]:8787"];
        assert_eq!(server_hosts("127.0.0.1:8787", 8787), loopback);
        assert_eq!(
            server_hosts("0.0.0.0:8787", 8787),
            loopback,
            "a wildcard names nothing"
        );
        assert_eq!(server_hosts("[::]:8787", 8787), loopback);
        assert_eq!(
            server_hosts("192.168.1.10:0", 41000)
                .last()
                .map(String::as_str),
            Some("192.168.1.10:41000"),
            "the bound port, not the requested 0"
        );
        assert_eq!(
            server_hosts("Spawn-Box:8787", 8787)
                .last()
                .map(String::as_str),
            Some("spawn-box:8787")
        );
    }

    #[test]
    fn only_this_servers_host_and_origin_read_without_the_token() {
        let hosts = server_hosts("192.168.1.10:8787", 8787);
        for host in [
            "127.0.0.1:8787",
            "localhost:8787",
            "LOCALHOST:8787",
            "[::1]:8787",
            "192.168.1.10:8787",
        ] {
            assert!(tokenless_request(Some(host), None, &hosts), "{host}");
        }
        // DNS rebinding: the page's own name reaches 127.0.0.1.
        assert!(!tokenless_request(
            Some("attacker.example:8787"),
            None,
            &hosts
        ));
        assert!(!tokenless_request(Some("attacker.example"), None, &hosts));
        // Another port is another origin; no Host at all is refused.
        assert!(!tokenless_request(Some("127.0.0.1:9999"), None, &hosts));
        assert!(!tokenless_request(Some("localhost"), None, &hosts));
        assert!(!tokenless_request(None, None, &hosts));
        // Origin: the same host over http, or none; anything else is foreign.
        let same = Some("http://127.0.0.1:8787");
        assert!(tokenless_request(Some("127.0.0.1:8787"), same, &hosts));
        assert!(tokenless_request(
            Some("localhost:8787"),
            Some("http://localhost:8787"),
            &hosts
        ));
        for origin in [
            "http://attacker.example",
            "http://attacker.example:8787",
            "http://localhost:8787",
            "https://127.0.0.1:8787",
            "null",
        ] {
            assert!(
                !tokenless_request(Some("127.0.0.1:8787"), Some(origin), &hosts),
                "{origin}"
            );
        }
        // Port 80 is implied on both sides.
        let web = server_hosts("127.0.0.1:80", 80);
        assert!(tokenless_request(
            Some("localhost"),
            Some("http://localhost"),
            &web
        ));
    }

    /// Status of one GET of `path` from `serve`'s request handler with no
    /// token set, sending `headers` (each line ending in CRLF, `PORT` replaced
    /// by the server's port) and no other.
    fn tokenless_status(path: &str, headers: &str) -> u16 {
        use std::io::{Read, Write};
        let server = tiny_http::Server::http("127.0.0.1:0").expect("listen");
        let port = server.server_addr().to_ip().expect("ip").port();
        let hosts = server_hosts("127.0.0.1:0", port);
        let web = std::env::temp_dir();
        let worker = std::thread::spawn(move || {
            let request = server.recv().expect("request");
            handle(request, &web, None, Some(port), &hosts).expect("answered");
        });
        let headers = headers.replace("PORT", &port.to_string());
        let mut client = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
        write!(
            client,
            "GET {path} HTTP/1.1\r\n{headers}Connection: close\r\n\r\n"
        )
        .expect("send");
        let mut answer = String::new();
        client.read_to_string(&mut answer).expect("read");
        worker.join().expect("handler");
        answer
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .expect("status line")
    }

    /// A server running `serve`'s own loop ([`serve_requests`]) with at most
    /// `limit` requests at once, over a web directory holding a 64 MiB
    /// `big.bin`. Returns the port and the directory.
    fn serving(tag: &str, limit: usize) -> (u16, std::path::PathBuf) {
        let web = std::env::temp_dir().join(format!("spawn-serve-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&web).expect("web");
        std::fs::write(web.join("big.bin"), vec![0u8; 64 << 20]).expect("big file");
        let server = tiny_http::Server::http("127.0.0.1:0").expect("listen");
        let port = server.server_addr().to_ip().expect("ip").port();
        let shared = Arc::new(Shared {
            web: web.clone(),
            token: None,
            port: Some(port),
            hosts: server_hosts("127.0.0.1:0", port),
        });
        std::thread::spawn(move || serve_requests(&server, &shared, limit));
        (port, web)
    }

    /// A client that asks for `big.bin` and never reads the answer.
    fn stalled_client(port: u16) -> std::net::TcpStream {
        use std::io::Write;
        let mut stalled = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
        write!(
            stalled,
            "GET /big.bin HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n"
        )
        .expect("send");
        std::thread::sleep(std::time::Duration::from_millis(500));
        stalled
    }

    /// The status of a request answered without the store (a malformed
    /// session id is 400), or `None` when no answer came within 3 s.
    fn quick_status(port: u16) -> Option<u16> {
        use std::io::{Read, Write};
        let mut client = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .expect("timeout");
        write!(
            client,
            "GET /api/session/not%20a%20session HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\
             Connection: close\r\n\r\n"
        )
        .expect("send");
        let mut answer = [0u8; 12];
        let read = client.read(&mut answer).ok()?;
        std::str::from_utf8(&answer[..read])
            .ok()?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    }

    /// Audit M2: a client that asks for a large file and never reads it
    /// holds one request's thread, not the server.
    #[test]
    fn a_client_that_stops_reading_does_not_stall_the_others() {
        let (port, web) = serving("stall", 2);
        let stalled = stalled_client(port);
        let status = quick_status(port);
        let _ = std::fs::remove_dir_all(&web);
        drop(stalled);
        assert_eq!(
            status,
            Some(400),
            "a second client is answered while the first does not read"
        );
    }

    /// With every slot held, the next request is told 503 at once, not
    /// left waiting.
    #[test]
    fn a_request_past_the_limit_is_answered_busy() {
        let (port, web) = serving("busy", 1);
        let stalled = stalled_client(port);
        let status = quick_status(port);
        drop(stalled);
        // The slot comes back once the stalled request ends.
        let mut freed = None;
        for _ in 0..50 {
            freed = quick_status(port);
            if freed == Some(400) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let _ = std::fs::remove_dir_all(&web);
        assert_eq!(status, Some(503));
        assert_eq!(freed, Some(400), "a freed slot serves again");
    }

    #[test]
    fn a_rebound_or_cross_origin_api_read_is_refused_before_it_reaches_the_store() {
        // A malformed session id answers 400 without a store: the request
        // passed the guard.
        let path = "/api/session/not%20a%20session";
        assert_eq!(tokenless_status(path, "Host: 127.0.0.1:PORT\r\n"), 400);
        assert_eq!(
            tokenless_status(
                path,
                "Host: localhost:PORT\r\nOrigin: http://localhost:PORT\r\n"
            ),
            400
        );
        assert_eq!(
            tokenless_status(path, "Host: attacker.example:PORT\r\n"),
            403
        );
        assert_eq!(
            tokenless_status(path, "Host: 127.0.0.1\r\n"),
            403,
            "port missing"
        );
        assert_eq!(tokenless_status(path, ""), 403, "no Host");
        assert_eq!(
            tokenless_status(
                path,
                "Host: 127.0.0.1:PORT\r\nOrigin: http://attacker.example\r\n"
            ),
            403
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
        assert_eq!(summary["open_task"]["session_id"], "session_resident_rsi");
        assert_eq!(summary["open_task"]["last_verdict"], "rejected");
        assert_eq!(
            summary["objectives"],
            json!({"open": 1, "solved": 1, "met": 1})
        );
    }

    #[test]
    fn accepted_work_waiting_for_the_gate_is_the_open_task() {
        let mission = |gate: serde_json::Value, state: &str| {
            json!({
                "id": 4, "halted": null, "gate": gate, "objectives": [],
                "tasks": [task(14, 3, state, 50, None), task(12, 2, "landed", 30, None)],
            })
        };
        for state in ["accepted", "landing"] {
            let summary = rsi_summary(&mission(json!("/srv/rsi/scenarios"), state));
            assert_eq!(summary["open_task"]["id"], 14, "{state}");
            assert_eq!(summary["open_task"]["state"], state);
        }
        // Without a gate, acceptance is the end.
        let summary = rsi_summary(&mission(serde_json::Value::Null, "accepted"));
        assert!(summary["open_task"].is_null());
        let summary = rsi_summary(&mission(json!("/srv/rsi/scenarios"), "cancelled"));
        assert!(summary["open_task"].is_null());
    }
}
