//! HTTP API and static files for the observer UI: 33 GET route patterns
//! (`GET /api/stream` among them) and nine POST routes. Seven are a
//! person's writes: an input to a session, a key, a restart or a reassign
//! of a stalled worker, the resume of a halted mission, a message to the
//! supervisor ([`master_message`]) and the answer to an approval prompt
//! (`POST /api/approvals/<session_id>/act`). Two are the feature registry's: a fresh scan, which changes
//! nothing, and a file's association ([`feature_scan_post`],
//! [`feature_associate_post`]). Every route is a row of [`ROUTES`], which
//! the router dispatches on; every route, with its method and CLI twin, is
//! listed in docs/api.md, and a test keeps the two equal. The 11 routes
//! issue 98 renamed answer under their old spellings too, 14 alias patterns
//! ([`ALIASES`]):
//! `handle()` rewrites a legacy path to its canonical one first, and the
//! answer adds `Link: <canonical>; rel="successor-version"` (only when the
//! path is safe in a header: [`linkable`]).
//!
//! Every read goes through the same store methods the CLI uses, as the
//! read-only `spawn_reader` login, so a defect in a read cannot change
//! state. An API path that reads answers 405 to any method but GET and HEAD.
//!
//! The first write is `POST /api/sessions/<id>/input` with `{"text": "...",
//! "key": "<idempotency key>"}` (the key may come in an `Idempotency-Key`
//! header instead): the input `spawn submit --principal human --agent <the
//! session's agent> --session <id> --key <key> --action
//! '{"type":"input","text":...}'` makes, with the same idempotency rule, so
//! a retried POST types once (the same session and text: a key another
//! session's input used, or other text, is 409 and nothing is made, GitHub
//! #16 item 8); like every person's input it waits for the session's turn.
//! It answers 202 with the command, which `/api/sessions/<id>/commands` then
//! follows. A session no agent has used is
//! 404; one that runs no execution (it never ran one, or its latest ended)
//! is 409 with the reason ("<session> runs no execution: an input needs a
//! running engine; resume the session"), and no command is made, so the
//! person learns at once that nothing would read it (objective 040; `spawn
//! submit` still records such an input, which the Bridge fails with the
//! same words). It runs as the `spawn_human`
//! login, whose rights are `spawn_human_input` and `spawn_human_key`
//! (crates/spawn-postgres/sql/schema.sql): it submits no other action, reads
//! nothing and appends no event. `{"probe": true}` in place of a text passes
//! every guard and writes nothing: the page asks it before it offers its
//! input box. The guards, in order: the read guard below (the clients that
//! may read may write, and no other); POST only, 405 otherwise; unless the
//! request carries the token, an `Origin` naming this server as the `Host`
//! does (a page on another site cannot make the owner's browser type), and,
//! when no token is set at all, a client socket the owner's uid holds (as
//! `SPAWN_SERVE_LOCAL_OPEN` checks: anyone on a token-less loopback server
//! reads, only the owner types); `Content-Type: application/json` (no form
//! a page may post without asking first), 415 otherwise; a body of at most
//! [`MAX_INPUT_BODY`] bytes, 413 otherwise; then a key and a text a person
//! types (not blank, at most [`MAX_INPUT_TEXT`] bytes, no control character
//! but tab and line ends), 400 otherwise.
//!
//! The second write is `POST /api/sessions/<id>/key` with `{"name":
//! "escape", "key": "<idempotency key>"}` (backlog P1): the key command
//! `spawn submit --principal human ... --action '{"type":"key","key":...}'`
//! makes, through `spawn_human_key`, with the same guards and idempotency
//! rule. The name is one of escape, up, down, left, right, enter, y, n,
//! digit1, digit2, digit3 (the bare `1` to `3` name the digits too; issue
//! #91) (400 otherwise). A key is its own action, never text, and does not wait for
//! the turn: the Bridge sends it at once. A session no agent has used is
//! 404; one that runs no execution is 409 with the reason, and so is an
//! idempotency key another request used; no command is made.
//!
//! A person may also restart or reassign a stalled worker from its agent
//! page: `POST /api/sessions/<id>/restart` and `POST
//! /api/sessions/<id>/reassign` with `{"reason": "...", "key": "<idempotency key>"}`
//! (the key may come in an `Idempotency-Key` header; optional reason of at
//! most [`MAX_REASON`] bytes, no control character), through `spawn_human_task`
//! (the `spawn_human` login's third function), with the input's guards. The
//! session's delivered task is restarted (`task.restarted`, the dispatcher
//! stops the running execution and resumes its native session) or reassigned
//! (cancelled and assigned again to a fresh worker whose branch starts from
//! the old one's last checkpoint), `by` `human`. It answers 202 with
//! `{"task", "session_id", "agent_id", "principal": "human"}`, the task and
//! the worker that now carry the work, or 409 with the reason for a session
//! that holds no delivered task, and nothing is recorded. `GET
//! /api/sessions/<id>` links old and new: each execution's `restart_of` (the
//! execution of the same session a restart's resume replaced, else null),
//! and the session's `reassigned_to` and `reassigned_from` (`{"task",
//! "session_id"}` or null).
//!
//! A person may also resume a halted mission from Needs you (objective
//! 051): `POST /api/missions/<id>/resume` with `{"halt": <event>, "key":
//! "<idempotency key>"}` (the key may come in an `Idempotency-Key` header),
//! with the input's guards in the same order, `{"probe": true}` passing them
//! and writing nothing (200). It runs through `spawn_human_resume`, the
//! `spawn_human` login's fourth function, which records the owner's
//! `mission.resumed` with `by` from its own `spawn_principal()` (`human`,
//! never from here) and the `halt` it resumes; serve never writes it as the
//! owner. The halt is the mission's `mission.halted` event the page showed
//! (`/api/missions`' `halt.event`): it answers 202 `{"event": <the
//! mission.resumed record>}` for the halt the mission is in, the same event
//! for the same key again, and 409 with the reason, recording nothing, when
//! the mission is not halted or is halted on another halt (a page not
//! refreshed since); 404 for no such mission; 400 without a key or a halt.
//!
//! Master Control (issue #91 P1a): `POST /api/master/message` with
//! `{"text": "...", "mode": "auto" | "steer" | "queue" | "restart", "key":
//! "<idempotency key>"}` (the key may come in an `Idempotency-Key` header;
//! the mode is `auto` when left out), or `{"probe": true}`, with the
//! input's guards in its order, writes to the supervisor through
//! `spawn_human_master_message`, the `spawn_human` login's fifth function.
//! It never answers 409 for a supervisor that runs no execution: it wakes
//! it first (a resume when a native session is bound, else a start; none
//! when one is already in flight) and the input follows; `steer` presses
//! Esc into a running turn first, `restart` stops a running supervisor
//! first. 202 `{command_id, wake_command_id, stop_command_id,
//! steer_command_id, session_id, mode, principal, woke, replayed}`; a
//! retried POST makes nothing; 409 only for a key another request used.
//!
//! Approvals ([`approvals`]): `GET /api/approvals` lists the permission
//! and selection prompts the live panes show, found by fixed text rules (no
//! agent protocol), and `POST /api/approvals/<session_id>/act` answers one with
//! named keys or a text through `spawn_human_key` and `spawn_human_input`,
//! with the input's guards. `GET /api/context/digest` ([`digest`]) is the
//! fleet in one small answer with a bounded Markdown block. `GET
//! /api/stream` ([`stream`]) is the log as Server-Sent Events, on slots of
//! its own beside [`MAX_IN_FLIGHT`], fed by one `LISTEN spawn_events`
//! listener, with `Last-Event-ID` / `?after=` catch-up. Each node
//! of `/api/nodes` carries its `power` and the low-battery soft drain
//! (`low_battery_drain`, `effective_drained`, `schedulable`).
//!
//! Reading is still worth guarding: `/api/sessions/<id>/pane` shows any live pane.
//! `serve` binds 127.0.0.1 unless told otherwise, and on any other address
//! it refuses to start without `SPAWN_SERVE_TOKEN`. With a token set, every
//! API request must carry it (`Authorization: Bearer <token>` or
//! `?token=<token>`), except, with `SPAWN_SERVE_LOCAL_OPEN=1`, from a loopback
//! client that runs as the owner's uid (Linux: the uid of its socket in
//! `/proc/net/tcp{,6}`; without `/proc`, no client is exempt), and, with
//! `SPAWN_SERVE_OPEN_NETWORKS` (comma-separated CIDRs, e.g. the owner's
//! intranet `192.168.31.0/24`), from a client inside one of them that names
//! this server by an IP address inside one of them (an IP literal `Host`:
//! a page that rebinds its own name sends its name, so it reads nothing).
//! Off by default.
//! An API read served without the token (no token set, or the local-open
//! exemption) also needs a `Host` naming this server (127.0.0.1, localhost,
//! [::1] or the bound address, with its port) and no cross-origin `Origin`:
//! a page that rebinds its own name to 127.0.0.1, or a proxy on this machine
//! that forwards another name here, reads nothing without the token.
//! The built UI's static files are served without it:
//! they are the public build and hold no data, and a browser fetches them
//! without headers.
//!
//! How a read is sent (validators and 304, the boards' answers shared for a
//! second among the pages that poll, the lists' first lines) is
//! [`answers`]'s.

mod answers;
pub(crate) mod approvals;
pub(crate) mod attention;
pub(crate) mod capabilities;
pub(crate) mod core_lines;
mod digest;
pub(crate) mod feature_map;
mod ipc;
mod labels;
mod prov;
mod pwa;
mod storage;
mod stream;

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, PoisonError};

use serde_json::{Value, json};
use spawn_core::command::NamedKey;
use spawn_core::event::{GlobalPosition, SessionSeq};
use spawn_core::id::{IdempotencyKey, SessionId};
use tiny_http::{Header, Method, Request, Response, Server};

use spawn_core::id::{TerminalName, WorkspaceId};
use spawn_core::port::TerminalHost;

use crate::objectives::{
    Landing, Outcome, Task, attempts, choose, holds, landed_value, load_backlog, metric_lines,
    overruns,
};
use crate::{Flags, env_or, env_value};

/// Prefix of an error that answers 400.
pub(super) const BAD_REQUEST: &str = "bad request: ";

/// Prefix of an error that answers 503.
pub(super) const BUSY: &str = ipc::POOL_BUSY_PREFIX;

/// A session id from a URL, or a 400.
pub(super) fn session_id(id: &str) -> Result<SessionId, String> {
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

/// Largest body `POST /api/sessions/<id>/input` takes, in bytes.
const MAX_INPUT_BODY: usize = 16 * 1024;

/// Longest text a person types from the page, in bytes. `spawn_human_input`
/// (schema.sql) holds the same bound.
pub(super) const MAX_INPUT_TEXT: usize = 8 * 1024;

/// The usage of `spawn serve`, the only copy (main's help reads it).
pub(crate) const USAGE: &str = "usage: spawn serve [--address <host:port>] [--web <dir>] [--ipc [--ipc-sock <path>] [--cas <dir>]]\n      \
                                (the dashboard and the HTTP API, docs/api.md; --address or\n       \
                                SPAWN_SERVE_ADDRESS, default 127.0.0.1:8787; --web or SPAWN_WEB_DIR,\n       \
                                default web/dist; off loopback it needs SPAWN_SERVE_TOKEN;\n       \
                                --ipc serves HTTP/1.1-over-UDS at $XDG_RUNTIME_DIR/spawn/core.sock)";

/// Serve the API and the built UI until the process is killed.
pub fn serve(flags: &Flags) -> Result<i32, String> {
    if flags.switch("ipc") || flags.get("ipc-sock").is_some() {
        return ipc::run(flags);
    }
    let address = flags
        .get("address")
        .or_else(|| env_value("SPAWN_SERVE_ADDRESS"))
        .unwrap_or_else(|| "127.0.0.1:8787".to_string());
    let web = PathBuf::from(
        flags
            .get("web")
            .unwrap_or_else(|| env_or("SPAWN_WEB_DIR", "web/dist")),
    );
    let raw_token = match serve_token() {
        Ok(token) => token,
        Err(error) => {
            eprintln!("serve: {error}");
            return Ok(crate::config::CREDENTIAL_EXIT_CODE);
        }
    };
    let token = access_token(&address, raw_token)?;
    // The loop records costs at these prices; a page served beside a setting
    // that cannot be read would show figures nobody can trust, so it stops.
    crate::cost::Prices::from_env()?;
    // Read once, here, so a bad value is warned of at start.
    crate::config::message_taken_ms();
    let pool_config = ipc::PoolConfig::from_process_env()?;
    let owner_url = env_value("SPAWN_DATABASE_URL");
    let reader_override = env_value("SPAWN_SERVE_DATABASE_URL");
    let pools = ipc::ServePools::from_serve_env(
        owner_url.as_deref(),
        reader_override.as_deref(),
        pool_config,
    )?;
    drop(owner_url);
    let open = open_networks(env_value("SPAWN_SERVE_OPEN_NETWORKS").as_deref())?;
    let server = Server::http(&address).map_err(|error| format!("listen {address}: {error}"))?;
    let guard = match (&token, open.is_empty()) {
        (None, _) => "none".to_string(),
        (Some(_), true) => "token".to_string(),
        (Some(_), false) => format!(
            "token, none from {}",
            open.iter()
                .map(Network::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    // The bound address, not the requested one: with port 0 the kernel picks.
    println!(
        "serve: http://{} web {} auth {guard} pools reads={} writes={} events={}",
        server.server_addr(),
        web.display(),
        pool_config.reads,
        pool_config.writes,
        pool_config.events,
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
        open,
        pools: Ok(pools),
    });
    // Listen again on the bound address, so a client's port stays right.
    let bound = server
        .server_addr()
        .to_ip()
        .map_or_else(|| address.clone(), |address| address.to_string());
    let streams = Slots::with_per_peer(stream::SSE_SLOTS, Some(stream::STREAMS_PER_PEER));
    let warm_pools = shared.pools.as_ref().ok().and_then(Clone::clone);
    std::thread::spawn(move || {
        let text = match warm_pools {
            Some(pools) => run_with_reader_pool_status(
                &pools.reader,
                false,
                core_lines::warm,
                |_| false,
                |err| format!("core lines not warmed (pool busy): {err}"),
            ),
            None => core_lines::warm(),
        };
        println!("serve: {text}");
    });
    keep_listening(
        server,
        &bound,
        |server| {
            serve_requests_with_queue(server, &shared, MAX_IN_FLIGHT, MAX_REQUEST_QUEUE, &streams)
        },
        |address| Server::http(address).map_err(|error| error.to_string()),
        listen_again_pause,
    )
}

/// Serve from `listener` and, each time it stops, listen again on
/// `address`. Returns only when it cannot listen again.
///
/// One failed accept() (a connection reset before it was taken, too many
/// open files, no memory) ends tiny_http's accept thread for good, and its
/// error goes only to the `log` crate. Its requests then end, and serve
/// once returned Ok(0) and exited without a word: a page and every client
/// saw a refused port and nothing said why. Now the stop is said on stderr
/// (the journal), and the same address is listened on again after a
/// pause. If that fails, every try, serve exits with an error, so its unit
/// restarts it and the reason is in the journal.
fn keep_listening<L>(
    mut listener: L,
    address: &str,
    mut serve: impl FnMut(&L) -> std::io::Error,
    mut listen: impl FnMut(&str) -> Result<L, String>,
    pause: impl Fn(u32) -> std::time::Duration,
) -> Result<i32, String> {
    let mut stops: u32 = 0;
    loop {
        let started = std::time::Instant::now();
        let error = serve(&listener);
        // A listener that served a while starts the pauses over.
        if started.elapsed() >= LISTEN_AGAIN_QUIET {
            stops = 0;
        }
        stops = stops.saturating_add(1);
        eprintln!("serve: the listener stopped: {error}; listening again on {address}");
        // The old listener's socket goes first, so its port can be bound again.
        drop(listener);
        let mut tries = 0;
        let next = loop {
            tries += 1;
            std::thread::sleep(pause(stops));
            match listen(address) {
                Ok(next) => break next,
                Err(error) if tries < LISTEN_AGAIN_TRIES => {
                    eprintln!("serve: cannot listen again on {address} yet: {error}");
                }
                Err(error) => {
                    return Err(format!(
                        "serve: cannot listen again on {address} after the listener stopped \
                         ({tries} tries): {error}"
                    ));
                }
            }
        };
        eprintln!("serve: listening again on http://{address}");
        listener = next;
    }
}

/// Tries to listen again after the listener stopped, before serve gives up.
const LISTEN_AGAIN_TRIES: u32 = 5;

/// A listener that served this long before it stopped starts the pauses
/// over.
const LISTEN_AGAIN_QUIET: std::time::Duration = std::time::Duration::from_mins(1);

/// The pause before listening again after the `stops`-th stop in a row:
/// 200 ms, doubling, at most 5 s.
fn listen_again_pause(stops: u32) -> std::time::Duration {
    let millis = 200u64.saturating_mul(1u64 << stops.saturating_sub(1).min(5));
    std::time::Duration::from_millis(millis.min(5000))
}

/// Concurrent worker threads handling non-stream requests in `spawn serve`.
const MAX_IN_FLIGHT: usize = 16;

/// Bounded waiting queue in front of [`MAX_IN_FLIGHT`] workers for traffic bursts;
/// requests beyond `MAX_IN_FLIGHT + MAX_REQUEST_QUEUE` answer 503 at once.
const MAX_REQUEST_QUEUE: usize = 1024;

/// Longest one dashboard statement runs on the reader's connection.
pub(super) const STATEMENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// What every request's handler reads.
struct Shared {
    web: PathBuf,
    token: Option<String>,
    port: Option<u16>,
    hosts: Vec<String>,
    /// Networks whose clients read without the token
    /// (`SPAWN_SERVE_OPEN_NETWORKS`, [`open_network_request`]).
    open: Vec<Network>,
    /// Unified role connection pools (`reads`, `writes`, `node`, `events`)
    /// derived once at startup; the owner URL is dropped immediately.
    pools: Result<Option<Arc<ipc::ServePools>>, String>,
}

impl Shared {
    #[cfg(test)]
    fn pools_from_owner(owner_url: Option<String>) -> Result<Option<Arc<ipc::ServePools>>, String> {
        let Some(url) = owner_url else {
            return Ok(None);
        };
        ipc::ServePools::from_serve_env(Some(&url), None, ipc::PoolConfig::default())
    }
}

/// Requests in flight, at most `limit`, and, when `per_peer` is set, at
/// most that many from one client address that is not loopback (the
/// streams' [`stream::STREAMS_PER_PEER`], as `nodes/relay.rs`'s
/// `PER_PEER`): one far client cannot take every stream slot. Loopback
/// clients (the owner's own machine, or a local proxy) count only toward
/// `limit` (unless `count_loopback`, which only a test sets).
struct Slots {
    taken: AtomicUsize,
    limit: usize,
    per_peer: Option<usize>,
    count_loopback: bool,
    peers: Mutex<HashMap<IpAddr, usize>>,
}

/// One request's place among [`Slots`] (and its address's, if counted),
/// given back when it is dropped.
struct Slot {
    slots: Arc<Slots>,
    peer: Option<IpAddr>,
}

/// Why [`Slots::take_from`] gave no slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Busy {
    /// Every slot is taken.
    All,
    /// This address holds its `per_peer` already.
    Peer,
}

impl Slots {
    #[cfg(test)]
    fn new(limit: usize) -> Arc<Self> {
        Self::with_per_peer(limit, None)
    }

    fn with_per_peer(limit: usize, per_peer: Option<usize>) -> Arc<Self> {
        Arc::new(Self {
            taken: AtomicUsize::new(0),
            limit,
            per_peer,
            count_loopback: false,
            peers: Mutex::new(HashMap::new()),
        })
    }

    /// As [`Slots::with_per_peer`], loopback clients counted too: a test's
    /// clients are all loopback.
    #[cfg(test)]
    fn counting_loopback(limit: usize, per_peer: usize) -> Arc<Self> {
        Arc::new(Self {
            taken: AtomicUsize::new(0),
            limit,
            per_peer: Some(per_peer),
            count_loopback: true,
            peers: Mutex::new(HashMap::new()),
        })
    }

    #[cfg(test)]
    fn take(slots: &Arc<Self>) -> Option<Slot> {
        Self::take_from(slots, None).ok()
    }

    /// A slot for a request from `peer`: one of `limit`, and one of its
    /// address's `per_peer` if that is counted (a `per_peer` set, an
    /// address that is not loopback).
    fn take_from(slots: &Arc<Self>, peer: Option<IpAddr>) -> Result<Slot, Busy> {
        slots
            .taken
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |taken| {
                (taken < slots.limit).then_some(taken + 1)
            })
            .map_err(|_| Busy::All)?;
        // Dropped on a refusal below, it gives the shared place back.
        let mut slot = Slot {
            slots: Arc::clone(slots),
            peer: None,
        };
        let counted = slots
            .per_peer
            .zip(peer.map(|peer| peer.to_canonical()))
            .filter(|(_, peer)| slots.count_loopback || !peer.is_loopback());
        if let Some((per_peer, peer)) = counted {
            let mut peers = slots.peers.lock().unwrap_or_else(PoisonError::into_inner);
            if peers.get(&peer).copied().unwrap_or(0) >= per_peer {
                return Err(Busy::Peer);
            }
            *peers.entry(peer).or_insert(0) += 1;
            slot.peer = Some(peer);
        }
        Ok(slot)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        if let Some(peer) = self.peer {
            let mut peers = self
                .slots
                .peers
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(held) = peers.get_mut(&peer) {
                *held = held.saturating_sub(1);
                if *held == 0 {
                    peers.remove(&peer);
                }
            }
        }
        self.slots.taken.fetch_sub(1, Ordering::AcqRel);
    }
}

struct RequestQueueState {
    queue: VecDeque<Request>,
    active: usize,
    closed: bool,
}

struct RequestQueue {
    workers: usize,
    queue_cap: usize,
    state: Mutex<RequestQueueState>,
    cond: Condvar,
}

#[cfg(test)]
fn serve_requests(
    server: &Server,
    shared: &Arc<Shared>,
    slots: &Arc<Slots>,
    streams: &Arc<Slots>,
) -> std::io::Error {
    serve_requests_with_queue(server, shared, slots.limit, 0, streams)
}

fn spawn_request_worker(
    idx: usize,
    rq: &Arc<RequestQueue>,
    shared: &Arc<Shared>,
) -> Option<std::thread::JoinHandle<()>> {
    let rq_worker = Arc::clone(rq);
    let shared_worker = Arc::clone(shared);
    match std::thread::Builder::new()
        .name(format!("serve-request-{idx}"))
        .spawn(move || {
            loop {
                let request = {
                    let mut guard = rq_worker
                        .state
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    loop {
                        if let Some(req) = guard.queue.pop_front() {
                            guard.active += 1;
                            break req;
                        }
                        if guard.closed {
                            return;
                        }
                        guard = rq_worker
                            .cond
                            .wait(guard)
                            .unwrap_or_else(PoisonError::into_inner);
                    }
                };
                if let Err(error) = handle(request, &shared_worker) {
                    eprintln!("serve: {error}");
                }
                let mut guard = rq_worker
                    .state
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                guard.active = guard.active.saturating_sub(1);
            }
        }) {
        Ok(h) => Some(h),
        Err(error) => {
            eprintln!("serve: failed to spawn worker {idx}: {error}");
            None
        }
    }
}

/// Serve `server`'s requests until its listener stops: streams run on their own
/// `streams` slots, while regular requests run on `workers` fixed worker threads
/// fed by a bounded queue of capacity `queue_cap` (answering 503 past it).
fn serve_requests_with_queue(
    server: &Server,
    shared: &Arc<Shared>,
    workers: usize,
    queue_cap: usize,
    streams: &Arc<Slots>,
) -> std::io::Error {
    let rq = Arc::new(RequestQueue {
        workers,
        queue_cap,
        state: Mutex::new(RequestQueueState {
            queue: VecDeque::new(),
            active: 0,
            closed: false,
        }),
        cond: Condvar::new(),
    });
    let worker_handles: Vec<_> = (0..workers)
        .filter_map(|idx| spawn_request_worker(idx, &rq, shared))
        .collect();
    loop {
        let request = match server.recv() {
            Ok(request) => request,
            Err(error) => {
                {
                    let mut guard = rq.state.lock().unwrap_or_else(PoisonError::into_inner);
                    guard.closed = true;
                    rq.cond.notify_all();
                }
                for h in worker_handles {
                    let _ = h.join();
                }
                return error;
            }
        };
        if stream::is_stream(&request) {
            let taken = Slots::take_from(streams, request.remote_addr().map(SocketAddr::ip));
            let Ok(slot) = taken else {
                let busy = json_response(
                    503,
                    &json!({ "error": match taken.err() {
                        Some(Busy::Peer) => "busy: too many streams from this address",
                        _ => "busy: too many streams at once",
                    } }),
                );
                if let Err(error) = request.respond(busy) {
                    eprintln!("serve: {error}");
                }
                continue;
            };
            let shared = Arc::clone(shared);
            let spawned = std::thread::Builder::new()
                .name("serve-stream".to_string())
                .spawn(move || {
                    let _slot = slot;
                    if let Err(error) = handle(request, &shared) {
                        eprintln!("serve: {error}");
                    }
                });
            if let Err(error) = spawned {
                eprintln!("serve: no thread for a stream: {error}");
            }
            continue;
        }
        let mut guard = rq.state.lock().unwrap_or_else(PoisonError::into_inner);
        let max_outstanding = rq.workers.saturating_add(rq.queue_cap);
        if rq.workers == 0 || guard.active + guard.queue.len() >= max_outstanding {
            drop(guard);
            let busy = json_response(503, &json!({ "error": "busy: too many requests at once" }));
            if let Err(error) = request.respond(busy) {
                eprintln!("serve: {error}");
            }
            continue;
        }
        guard.queue.push_back(request);
        rq.cond.notify_one();
    }
}

/// Resolve the UI/API bearer token, preferring Keep's `$CREDENTIALS_DIRECTORY/spawn-serve.token`
/// when present and falling back to `SPAWN_SERVE_TOKEN`.
fn serve_token() -> Result<Option<String>, String> {
    serve_token_with(
        env_value("CREDENTIALS_DIRECTORY").as_deref().map(Path::new),
        env_value("SPAWN_SERVE_TOKEN"),
    )
}

fn serve_token_with(
    cred_dir: Option<&Path>,
    env_token: Option<String>,
) -> Result<Option<String>, String> {
    if let Some(token) = crate::config::load_credential_value(cred_dir, "spawn-serve.token")? {
        return Ok(Some(token));
    }
    Ok(env_token)
}

/// The token every API request must carry, if any: Keep's `spawn-serve.token`
/// credential file when delivered, else `SPAWN_SERVE_TOKEN`. An address that
/// is not loopback needs one.
fn access_token(address: &str, token: Option<String>) -> Result<Option<String>, String> {
    if let Some(token) = &token
        && !is_header_safe_token(token)
    {
        return Err(format!(
            "refusing to serve with this SPAWN_SERVE_TOKEN: a token is 1 to {MAX_TOKEN_LEN} \
             visible ASCII characters (no space, newline or other byte), so a browser can send \
             it as a header (web/src/token.ts holds the same rule)"
        ));
    }
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

/// The uid this server runs as; none without `/proc`, where no socket's
/// uid can be read either. Not the owner of `/proc/self`: the server is
/// not dumpable ([`spawn_host::undumpable`]), and that makes root own its
/// `/proc/<pid>`.
fn owner_uid() -> Option<u32> {
    std::path::Path::new("/proc/self")
        .exists()
        .then(spawn_host::current_uid)
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
    local_open() && owners_client(peer, port)
}

/// Whether the client end of a connection from `peer` to this server's
/// `port` is a loopback socket the owner's uid holds, by `/proc` (none
/// without it): [`local_exemption`] whatever `SPAWN_SERVE_LOCAL_OPEN` says.
fn owners_client(peer: Option<SocketAddr>, port: Option<u16>) -> bool {
    let tables: Vec<String> = ["/proc/net/tcp", "/proc/net/tcp6"]
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .collect();
    let uid = peer
        .zip(port)
        .and_then(|(peer, port)| peer_uid(peer, port, &tables));
    local_exemption(true, peer, uid, owner_uid())
}

/// One network of `SPAWN_SERVE_OPEN_NETWORKS`: an address and how many of
/// its leading bits a client's address must share.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Network {
    address: std::net::IpAddr,
    prefix: u8,
}

impl Network {
    /// Whether `ip` lies in this network. An IPv4 address mapped into IPv6
    /// is compared as the IPv4 address it is.
    fn contains(&self, ip: std::net::IpAddr) -> bool {
        use std::net::IpAddr;
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            IpAddr::V4(_) => ip,
        };
        let prefix = u32::from(self.prefix);
        match (self.address, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = u32::MAX.checked_shl(32 - prefix).unwrap_or(0);
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = u128::MAX.checked_shl(128 - prefix).unwrap_or(0);
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

impl std::fmt::Display for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.address, self.prefix)
    }
}

/// `SPAWN_SERVE_OPEN_NETWORKS` (`text`): comma-separated CIDRs, IPv4 or
/// IPv6 (`192.168.31.0/24`, `fd00::/8`); an address without `/` is that one
/// address. Unset or empty: none. A malformed entry stops `serve` from
/// starting rather than opening more or less than the owner wrote. A
/// network as wide as the whole address space (`/0`) is refused: that is
/// no token at all.
fn open_networks(text: Option<&str>) -> Result<Vec<Network>, String> {
    let mut networks = Vec::new();
    for entry in text.unwrap_or_default().split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let refuse = |why: &str| {
            format!("refusing to serve: SPAWN_SERVE_OPEN_NETWORKS entry {entry:?} {why}")
        };
        let (address, prefix) = entry
            .split_once('/')
            .map_or((entry, None), |(address, prefix)| (address, Some(prefix)));
        let address: std::net::IpAddr = address
            .parse()
            .map_err(|_| refuse("is not an IP address or CIDR"))?;
        let width: u8 = if address.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            None => width,
            Some(prefix) => prefix
                .parse::<u8>()
                .ok()
                .filter(|prefix| *prefix <= width)
                .ok_or_else(|| refuse("has no valid prefix length"))?,
        };
        if prefix == 0 {
            return Err(refuse("opens every address: set no token instead of that"));
        }
        networks.push(Network { address, prefix });
    }
    Ok(networks)
}

/// Whether a request from `peer` reads without the token because it comes
/// from an open network (`SPAWN_SERVE_OPEN_NETWORKS`): its address lies in
/// one of `open`, its `Host` is an IP address inside one of them with this
/// server's `port` (an IP literal: a page that rebinds its own name to this
/// host sends that name, so it reads nothing), and its `Origin`, if any, is
/// that same host over http.
fn open_network_request(
    peer: Option<SocketAddr>,
    host: Option<&str>,
    origin: Option<&str>,
    port: Option<u16>,
    open: &[Network],
) -> bool {
    let inside = |ip: std::net::IpAddr| open.iter().any(|network| network.contains(ip));
    let (Some(peer), Some(host), Some(port)) = (peer, host, port) else {
        return false;
    };
    if !inside(plain(peer).ip()) {
        return false;
    }
    let named: Option<SocketAddr> = authority(host).parse().ok();
    if !named.is_some_and(|named| named.port() == port && inside(named.ip())) {
        return false;
    }
    let host = authority(host);
    origin.is_none_or(|origin| {
        origin
            .trim()
            .strip_prefix("http://")
            .is_some_and(|rest| authority(rest) == host)
    })
}

/// Longest token `serve` accepts, and the UI sends.
const MAX_TOKEN_LEN: usize = 512;

/// Whether `token` is one the UI can send as `Authorization: Bearer`:
/// 1 to [`MAX_TOKEN_LEN`] visible ASCII characters (`web/src/token.ts`,
/// `TOKEN_PATTERN`, is the same rule).
fn is_header_safe_token(token: &str) -> bool {
    (1..=MAX_TOKEN_LEN).contains(&token.len()) && token.bytes().all(|byte| byte.is_ascii_graphic())
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

enum ReaderContext {
    Pooled {
        pool: Arc<ipc::RoleConnPool>,
        parked: Option<ipc::PoolSlot>,
        reused_any: bool,
    },
    #[cfg(test)]
    UrlOverride(String),
}

enum OpenMode {
    Pooled {
        pool: Arc<ipc::RoleConnPool>,
        parked: Option<ipc::PoolSlot>,
    },
    #[cfg(test)]
    Override(String),
    Ambient,
}

std::thread_local! {
    static READER_CONTEXT: std::cell::RefCell<Option<ReaderContext>> = const { std::cell::RefCell::new(None) };
}

static AMBIENT_READER_POOLS: LazyLock<Mutex<HashMap<String, Arc<ipc::RoleConnPool>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(super) struct PooledStore {
    slot: Option<ipc::PoolSlot>,
}

impl PooledStore {
    pub(super) fn mark_broken(&self) {
        if let Some(slot) = &self.slot {
            slot.mark_broken();
        }
    }
}

impl std::ops::Deref for PooledStore {
    type Target = spawn_postgres::PostgresStore;

    fn deref(&self) -> &Self::Target {
        self.slot
            .as_ref()
            .expect("pooled store slot present until drop")
            .store()
    }
}

impl Drop for PooledStore {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            if std::thread::panicking() || slot.is_broken() {
                drop(slot);
                return;
            }
            READER_CONTEXT.with(|cell| {
                if let Some(ReaderContext::Pooled { parked, .. }) = cell.borrow_mut().as_mut()
                    && parked.is_none()
                {
                    *parked = Some(slot);
                    return;
                }
                drop(slot);
            });
        }
    }
}

fn reader_pool_for_url(url: &str) -> Arc<ipc::RoleConnPool> {
    let mut map = AMBIENT_READER_POOLS
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if let Some(pool) = map.get(url) {
        return Arc::clone(pool);
    }
    let config = ipc::PoolConfig::from_process_env().unwrap_or_default();
    let pool = ipc::RoleConnPool::new(
        "reads",
        zeroize::Zeroizing::new(url.to_string()),
        config.reads,
        config.wait_timeout,
    );
    if map.len() > 32 {
        map.clear();
    }
    map.insert(url.to_string(), Arc::clone(&pool));
    pool
}

pub(super) fn checkout_pooled(pool: &Arc<ipc::RoleConnPool>) -> Result<PooledStore, String> {
    let mut slot = pool.acquire().map_err(|e| e.to_string())?;
    slot.ensure_connected().map_err(|e| e.to_string())?;
    Ok(PooledStore { slot: Some(slot) })
}

pub(super) fn run_with_reader_pool_status<T>(
    pool: &Arc<ipc::RoleConnPool>,
    acquire_upfront: bool,
    mut f: impl FnMut() -> T,
    is_backend_failure: impl Fn(&T) -> bool,
    busy_fallback: impl FnOnce(ipc::PoolError) -> T,
) -> T {
    struct Reset(Option<ReaderContext>);
    impl Drop for Reset {
        fn drop(&mut self) {
            READER_CONTEXT.with(|cell| *cell.borrow_mut() = self.0.take());
        }
    }
    for attempt in 0..2 {
        let initial_slot = if acquire_upfront {
            match pool.acquire() {
                Ok(slot) => Some(slot),
                Err(err) => return busy_fallback(err),
            }
        } else {
            None
        };
        let prev = READER_CONTEXT.with(|cell| {
            cell.borrow_mut().replace(ReaderContext::Pooled {
                pool: Arc::clone(pool),
                parked: initial_slot,
                reused_any: false,
            })
        });
        let _reset = Reset(prev);
        let result = f();
        let (parked, reused_any) = READER_CONTEXT.with(|cell| match cell.borrow_mut().take() {
            Some(ReaderContext::Pooled {
                parked, reused_any, ..
            }) => (parked, reused_any),
            other => {
                *cell.borrow_mut() = other;
                (None, false)
            }
        });
        if is_backend_failure(&result) {
            let mut can_retry = reused_any;
            if let Some(slot) = parked {
                if slot.was_reused() {
                    can_retry = true;
                }
                if slot.is_connected() {
                    slot.mark_broken();
                }
                drop(slot);
            }
            if attempt == 0 && can_retry {
                continue;
            }
        } else {
            drop(parked);
        }
        return result;
    }
    unreachable!("reader retry loop exits within two attempts")
}

/// Run `f` with an explicit `spawn_reader` database URL override (used by tests
/// so worker threads never read ambient `SPAWN_DATABASE_URL`).
#[cfg(test)]
pub(super) fn with_reader_url<T>(url: &str, f: impl FnOnce() -> T) -> T {
    struct Reset(Option<ReaderContext>);
    impl Drop for Reset {
        fn drop(&mut self) {
            READER_CONTEXT.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let prev = READER_CONTEXT.with(|slot| {
        slot.borrow_mut()
            .replace(ReaderContext::UrlOverride(url.to_string()))
    });
    let _reset = Reset(prev);
    f()
}

/// The dashboard's connection: checked out from the active unified
/// `RoleConnPool` as the read-only `spawn_reader` login, never the controller's.
fn open_store() -> Result<PooledStore, String> {
    let mode = READER_CONTEXT.with(|cell| match cell.borrow_mut().as_mut() {
        Some(ReaderContext::Pooled { pool, parked, .. }) => OpenMode::Pooled {
            pool: Arc::clone(pool),
            parked: parked.take(),
        },
        #[cfg(test)]
        Some(ReaderContext::UrlOverride(url)) => OpenMode::Override(url.clone()),
        None => OpenMode::Ambient,
    });
    match mode {
        OpenMode::Pooled { pool, parked } => {
            let mut slot = match parked {
                Some(slot) => slot,
                None => pool.acquire().map_err(|e| e.to_string())?,
            };
            slot.ensure_connected().map_err(|e| e.to_string())?;
            let reused = slot.was_reused();
            if reused {
                READER_CONTEXT.with(|cell| {
                    if let Some(ReaderContext::Pooled { reused_any, .. }) =
                        cell.borrow_mut().as_mut()
                    {
                        *reused_any = true;
                    }
                });
            }
            Ok(PooledStore { slot: Some(slot) })
        }
        #[cfg(test)]
        OpenMode::Override(url) => {
            if url.trim().is_empty() {
                return Err("SPAWN_DATABASE_URL is not set".to_string());
            }
            checkout_pooled(&reader_pool_for_url(&url))
        }
        OpenMode::Ambient => {
            let controller = env_value("SPAWN_DATABASE_URL")
                .ok_or_else(|| "SPAWN_DATABASE_URL is not set".to_string())?;
            let url = match env_value("SPAWN_SERVE_DATABASE_URL") {
                Some(url) => url,
                None => spawn_postgres::role_url(&controller, "spawn_reader")?,
            };
            checkout_pooled(&reader_pool_for_url(&url))
        }
    }
}

fn stream_pools(shared: &Shared) -> Result<Option<Arc<ipc::ServePools>>, String> {
    shared.pools.clone()
}

fn human_pool(shared: &Shared) -> Result<Arc<ipc::RoleConnPool>, (u16, String)> {
    match &shared.pools {
        Ok(Some(pools)) => pools
            .human
            .clone()
            .ok_or_else(|| (500, "SPAWN_DATABASE_URL is not set".to_string())),
        Ok(None) => Err((500, "SPAWN_DATABASE_URL is not set".to_string())),
        Err(err) => Err((500, err.clone())),
    }
}

fn checkout_human(shared: &Shared) -> Result<ipc::CheckedOutConn, (u16, String)> {
    human_pool(shared)?
        .checkout_verified()
        .map_err(ipc::PoolError::status_and_message)
}

/// What the read guard decided for one API request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Access {
    /// It may read (and so ask to write, [`input`]).
    read: bool,
    /// It carries the token.
    token: bool,
}

/// The read guard. Without the token a read needs a Host that names this
/// server and no foreign Origin (DNS rebinding, a local proxy); with it,
/// any. With SPAWN_SERVE_LOCAL_OPEN=1 the owner's browser on this machine
/// needs no token: a client running as the owner's uid can read the token
/// file anyway. Another user's loopback client still needs it. Off by
/// default. A client of an open network (SPAWN_SERVE_OPEN_NETWORKS) that
/// names this server by its address there needs none either.
fn access(request: &Request, query: &str, shared: &Shared) -> Access {
    let host = header(request, "Host");
    let origin = header(request, "Origin");
    let named = tokenless_request(host, origin, &shared.hosts);
    let peer = request.remote_addr().copied();
    let Some(token) = shared.token.as_deref() else {
        return Access {
            read: named,
            token: false,
        };
    };
    let carries = authorized(request, query, token);
    Access {
        read: carries
            || (named && owners_own_request(peer, shared.port))
            || open_network_request(peer, host, origin, shared.port, &shared.open),
        token: carries,
    }
}

/// Every canonical API route `spawn serve` answers: its method (`GET`
/// takes HEAD too) and its path pattern, where a `<name>` segment is any one
/// segment that is not empty. `handle()` and `reader()` dispatch on these
/// patterns only, and the test `routes_match_docs_api_md` keeps
/// docs/api.md's tables equal to this one. No two patterns fit one path.
pub(super) const ROUTES: &[(&str, &str)] = &[
    ("GET", "/api/overview"),
    ("GET", "/api/fleet"),
    ("GET", "/api/attention"),
    ("GET", "/api/missions"),
    ("GET", "/api/events"),
    ("GET", "/api/sessions/<id>"),
    ("GET", "/api/sessions/<id>/commands"),
    ("GET", "/api/sessions/<id>/judgements"),
    ("GET", "/api/sessions/<id>/pane"),
    ("GET", "/api/tasks/<n>"),
    ("GET", "/api/tasks/<n>/gate"),
    ("GET", "/api/objectives"),
    ("GET", "/api/objectives/<m>/<n>"),
    ("GET", "/api/objectives/pane"),
    ("GET", "/api/scorecard"),
    ("GET", "/api/scorecard/fleet"),
    ("GET", "/api/interventions"),
    ("GET", "/api/alerts"),
    ("GET", "/api/cost"),
    ("GET", "/api/core-lines"),
    ("GET", "/api/verifications"),
    ("GET", "/api/nodes"),
    ("GET", "/api/grading"),
    ("GET", "/api/capabilities"),
    ("GET", "/api/features"),
    ("GET", "/api/features/coverage"),
    ("GET", "/api/features/coverage/projects"),
    ("GET", "/api/stream"),
    ("GET", "/api/approvals"),
    ("GET", "/api/context/digest"),
    ("GET", "/api/prov/who"),
    ("GET", "/api/prov/change/<id>"),
    ("GET", "/api/prov/blast"),
    ("POST", "/api/sessions/<id>/input"),
    ("POST", "/api/sessions/<id>/key"),
    ("POST", "/api/sessions/<id>/restart"),
    ("POST", "/api/sessions/<id>/reassign"),
    ("POST", "/api/missions/<id>/resume"),
    ("POST", "/api/master/message"),
    ("POST", "/api/approvals/<session_id>/act"),
    ("POST", "/api/features/scan"),
    ("POST", "/api/features/associate"),
];

/// The legacy spelling of each renamed route and the canonical pattern it
/// answers as (the same `<name>`s). A request to a legacy path is answered
/// exactly as its canonical path is, plus `Link: <canonical>;
/// rel="successor-version"`; the first one of each spelling is logged.
const ALIASES: &[(&str, &str)] = &[
    ("/api/session/<id>", "/api/sessions/<id>"),
    ("/api/fleet/<id>/commands", "/api/sessions/<id>/commands"),
    (
        "/api/fleet/<id>/judgements",
        "/api/sessions/<id>/judgements",
    ),
    ("/api/pane/<id>", "/api/sessions/<id>/pane"),
    ("/api/session/<id>/input", "/api/sessions/<id>/input"),
    ("/api/session/<id>/key", "/api/sessions/<id>/key"),
    ("/api/session/<id>/restart", "/api/sessions/<id>/restart"),
    ("/api/session/<id>/reassign", "/api/sessions/<id>/reassign"),
    ("/api/mission/<id>/resume", "/api/missions/<id>/resume"),
    ("/api/missions/task/<n>", "/api/tasks/<n>"),
    ("/api/rsi/task/<n>", "/api/tasks/<n>/gate"),
    ("/api/rsi", "/api/objectives"),
    ("/api/rsi/objective/<m>/<n>", "/api/objectives/<m>/<n>"),
    ("/api/rsi/pane", "/api/objectives/pane"),
];

/// The segments of `path` that `pattern`'s `<name>`s stand for, in order,
/// when `path` fits it (as [`ROUTES`] says), else `None`.
fn fit<'a>(pattern: &str, path: &'a str) -> Option<Vec<&'a str>> {
    let mut wanted = pattern.split('/');
    let mut given = path.split('/');
    let mut values = Vec::new();
    loop {
        match (wanted.next(), given.next()) {
            (None, None) => return Some(values),
            (Some(want), Some(segment)) if want.starts_with('<') && want.ends_with('>') => {
                if segment.is_empty() {
                    return None;
                }
                values.push(segment);
            }
            (Some(want), Some(segment)) if want == segment => {}
            _ => return None,
        }
    }
}

/// The route `path` names: its method, pattern and the values of the
/// pattern's `<name>`s.
pub(super) fn route(path: &str) -> Option<(&'static str, &'static str, Vec<&str>)> {
    ROUTES
        .iter()
        .find_map(|(method, pattern)| fit(pattern, path).map(|values| (*method, *pattern, values)))
}

/// The canonical path a legacy `path` answers as, with the legacy pattern
/// it fits, or `None` for a path that is no legacy alias.
fn canonical(path: &str) -> Option<(String, &'static str)> {
    ALIASES.iter().find_map(|(legacy, successor)| {
        let values = fit(legacy, path)?;
        let mut values = values.into_iter();
        let rewritten: Vec<&str> = successor
            .split('/')
            .map(|segment| {
                if segment.starts_with('<') {
                    values.next().unwrap_or(segment)
                } else {
                    segment
                }
            })
            .collect();
        Some((rewritten.join("/"), *legacy))
    })
}

std::thread_local! {
    /// The canonical path of the legacy request this thread is answering,
    /// if it is one: every response sent meanwhile names it ([`Successor`]).
    static SUCCESSOR: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// While alive, this thread's responses carry `Link: <path>;
/// rel="successor-version"` (one request is answered on one thread).
struct Successor;

impl Successor {
    fn new(path: String) -> Self {
        SUCCESSOR.with(|successor| *successor.borrow_mut() = Some(path));
        Successor
    }
}

impl Drop for Successor {
    fn drop(&mut self) {
        SUCCESSOR.with(|successor| successor.borrow_mut().take());
    }
}

/// The `Link` header naming the canonical path of the legacy request this
/// thread answers, if it is answering one whose path is safe in a header
/// ([`linkable`]); otherwise none, though the request is still answered as
/// its canonical path.
fn successor_link() -> Option<Header> {
    SUCCESSOR.with(|successor| {
        successor
            .borrow()
            .as_deref()
            .filter(|path| linkable(path))
            .and_then(|path| {
                Header::from_bytes("Link", format!("<{path}>; rel=\"successor-version\"")).ok()
            })
    })
}

/// Whether `path` may go into a `Link` header as `<path>`: graphic ASCII
/// only (no CR, LF, space or other control byte, which would split the
/// response) and none of `<`, `>` or `"`, which would end the URI
/// reference or open a parameter of the client's choosing.
fn linkable(path: &str) -> bool {
    !path.is_empty()
        && path
            .bytes()
            .all(|byte| (0x21..=0x7E).contains(&byte) && !matches!(byte, b'<' | b'>' | b'"'))
}

/// The legacy spellings already logged by this process: each once.
static LOGGED_ALIASES: std::sync::Mutex<BTreeSet<&'static str>> =
    std::sync::Mutex::new(BTreeSet::new());

/// The log line of the `legacy` spelling: its pattern and its canonical
/// pattern, both from [`ALIASES`], never a byte of the request.
fn alias_log_line(legacy: &'static str) -> String {
    let successor = ALIASES
        .iter()
        .find(|(pattern, _)| *pattern == legacy)
        .map_or("?", |(_, successor)| *successor);
    format!(
        "serve: legacy path {legacy} answered as {successor} (logged once; docs/api.md lists \
         the canonical routes)"
    )
}

/// Log the first request of the `legacy` spelling (its pattern only).
fn log_alias(legacy: &'static str) {
    let first = LOGGED_ALIASES
        .lock()
        .is_ok_and(|mut logged| logged.insert(legacy));
    if first {
        eprintln!("{}", alias_log_line(legacy));
    }
}

fn with_shared_reader<T>(
    shared: &Shared,
    mut f: impl FnMut() -> T,
    is_backend_failure: impl Fn(&T) -> bool,
    busy_fallback: impl FnOnce(ipc::PoolError) -> T,
) -> T {
    match &shared.pools {
        Ok(Some(pools)) => {
            run_with_reader_pool_status(&pools.reader, false, f, is_backend_failure, busy_fallback)
        }
        _ => f(),
    }
}

fn busy_answer(err: ipc::PoolError) -> answers::Answer {
    let (status, msg) = err.status_and_message();
    answers::Answer {
        status,
        body: json!({ "error": msg }).to_string(),
        etag: None,
    }
}

fn handle_api_request(
    request: Request,
    path: &str,
    query: &str,
    shared: &Shared,
) -> Result<Option<Request>, String> {
    let access = access(&request, query, shared);
    if !access.read {
        return refuse(request, shared.token.is_some()).map(|()| None);
    }
    if path == "/api/features/truth" || path == "/api/episodes" {
        if !matches!(request.method(), Method::Get | Method::Head) {
            return request
                .respond(not_allowed("GET"))
                .map(|()| None)
                .map_err(|error| error.to_string());
        }
        let answer = with_shared_reader(
            shared,
            || {
                if path == "/api/features/truth" {
                    feature_map::feature_truth_answer(query)
                } else {
                    episodes_answer(query)
                }
            },
            |a| a.status == 500,
            busy_answer,
        );
        let if_none_match = header(&request, "If-None-Match").map(str::to_string);
        return answers::respond(request, &answer, if_none_match.as_deref()).map(|()| None);
    }
    if is_storage_read_route(path) {
        return respond_storage_read(request, path, query, shared).map(|()| None);
    }
    let Some((method, pattern, values)) = route(path) else {
        return static_file(request, &shared.web, path).map(|()| None);
    };
    let id = values.first().copied().unwrap_or_default();
    match pattern {
        "/api/sessions/<id>/input" => return input(request, id, access, shared).map(|()| None),
        "/api/sessions/<id>/key" => return key(request, id, access, shared).map(|()| None),
        "/api/missions/<id>/resume" => return resume(request, id, access, shared).map(|()| None),
        "/api/sessions/<id>/restart" => {
            return moved(request, id, "restart", access, shared).map(|()| None);
        }
        "/api/sessions/<id>/reassign" => {
            return moved(request, id, "reassign", access, shared).map(|()| None);
        }
        "/api/master/message" => return master_message(request, access, shared).map(|()| None),
        "/api/stream" => return stream::stream(request, query, shared).map(|()| None),
        "/api/approvals/<session_id>/act" => {
            return approvals::act(request, id, access, shared).map(|()| None);
        }
        "/api/features/scan" => {
            return feature_scan_post(request, query, access, shared).map(|()| None);
        }
        "/api/features/associate" => {
            return feature_associate_post(request, query, access, shared).map(|()| None);
        }
        _ => {}
    }
    debug_assert_eq!(method, "GET", "every POST route is answered above");
    if !matches!(request.method(), Method::Get | Method::Head) {
        return request
            .respond(not_allowed("GET"))
            .map(|()| None)
            .map_err(|error| error.to_string());
    }
    if path == "/api/features/coverage" && mission_query(query).is_some() {
        return feature_coverage_mission_get(request, query, shared).map(|()| None);
    }
    Ok(Some(request))
}

fn handle(request: Request, shared: &Shared) -> Result<(), String> {
    let url = request.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((path, query)) => (path, query),
        None => (url.as_str(), ""),
    };
    // A legacy path answers as its canonical one, from the first guard to
    // the shared answer's key, so both spellings share one answer.
    let rewritten = canonical(path);
    let _successor = rewritten.as_ref().map(|(successor, legacy)| {
        log_alias(legacy);
        Successor::new(successor.clone())
    });
    let path = rewritten
        .as_ref()
        .map_or(path, |(successor, _)| successor.as_str());
    let request = if path.starts_with("/api/") {
        let Some(request) = handle_api_request(request, path, query, shared)? else {
            return Ok(());
        };
        request
    } else {
        request
    };
    // The PWA shell is the binary's own (serve/pwa.rs), whatever the
    // built directory holds.
    if let Some(asset) = pwa::asset(path) {
        return pwa::respond(request, asset);
    }
    if reader(path, query).is_none() {
        return static_file(request, &shared.web, path);
    }
    // A board's answer may be one computed for another page this second
    // (answers::SHARE_FOR), when the request accepts one that old; any
    // other is read now. A malformed identifier is the caller's mistake
    // (400), not the server's.
    let compute = || {
        with_shared_reader(
            shared,
            || {
                let read = reader(path, query).expect("checked");
                answers::answer(path, read(), BAD_REQUEST)
            },
            |a| a.status == 500,
            busy_answer,
        )
    };
    let answer = if answers::shared(path) {
        answers::ANSWERS.get(
            &answers::key(path, query),
            answers::accepted_age(header(&request, "Cache-Control")),
            compute,
        )
    } else {
        Arc::new(compute())
    };
    let if_none_match = header(&request, "If-None-Match").map(str::to_string);
    answers::respond(request, &answer, if_none_match.as_deref())
}

/// One API read, not run yet.
type Read<'a> = Box<dyn FnOnce() -> Result<Value, String> + 'a>;

/// The read at canonical `path` with `query`, or `None` when `path` is no
/// API read (a static file, or a 404).
pub(super) fn reader<'a>(path: &'a str, query: &'a str) -> Option<Read<'a>> {
    let (method, pattern, values) = route(path)?;
    if method != "GET" {
        return None;
    }
    let id = values.first().copied().unwrap_or_default();
    Some(match pattern {
        "/api/overview" => Box::new(overview),
        "/api/objectives" => Box::new(rsi_overview),
        "/api/objectives/pane" => Box::new(move || rsi_pane(query)),
        "/api/objectives/<m>/<n>" => {
            let rest = path.strip_prefix("/api/objectives/").unwrap_or_default();
            Box::new(move || rsi_objective(rest))
        }
        "/api/fleet" => Box::new(fleet),
        "/api/attention" => Box::new(attention::attention),
        "/api/missions" => Box::new(missions),
        "/api/events" => Box::new(move || events(query)),
        "/api/scorecard" => Box::new(move || scorecard(query)),
        "/api/scorecard/fleet" => Box::new(move || scorecard_fleet(query)),
        "/api/interventions" => Box::new(move || interventions(query)),
        "/api/alerts" => Box::new(move || alerts(query)),
        "/api/cost" => Box::new(move || cost(query)),
        "/api/core-lines" => Box::new(core_lines::core_lines),
        "/api/verifications" => Box::new(verifications),
        "/api/nodes" => Box::new(nodes),
        "/api/grading" => Box::new(grading),
        "/api/capabilities" => Box::new(|| Ok(capabilities::capabilities())),
        "/api/approvals" => Box::new(approvals::approvals),
        "/api/context/digest" => Box::new(digest::digest),
        "/api/prov/who" => Box::new(move || prov::who(query)),
        "/api/prov/change/<id>" => Box::new(move || prov::change(id)),
        "/api/prov/blast" => Box::new(move || prov::blast(query)),
        "/api/features" => Box::new(move || feature_map::feature_map(query)),
        "/api/features/coverage" => Box::new(feature_coverage_get),
        "/api/features/coverage/projects" => Box::new(feature_coverage_projects),
        "/api/sessions/<id>" => Box::new(move || session(id, query)),
        "/api/sessions/<id>/commands" => Box::new(move || session_list(id, "commands", query)),
        "/api/sessions/<id>/judgements" => Box::new(move || session_list(id, "judgements", query)),
        "/api/sessions/<id>/pane" => Box::new(move || pane(id, None)),
        "/api/tasks/<n>" => Box::new(move || mission_task(id)),
        "/api/tasks/<n>/gate" => Box::new(move || rsi_task(id)),
        _ => return None,
    })
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

/// 405, naming the one method (`allow`) the path takes.
fn not_allowed(allow: &'static str) -> Response<std::io::Cursor<Vec<u8>>> {
    let header = Header::from_bytes("Allow", allow).expect("static header is valid");
    json_response(
        405,
        &json!({ "error": format!("method not allowed: this path takes {allow} only") }),
    )
    .with_header(header)
}

/// `POST /api/sessions/<id>/input`, past the read guard (`access`): a
/// person's input to session `id`, or a probe of whether one would be taken
/// (module doc).
fn input(mut request: Request, id: &str, access: Access, shared: &Shared) -> Result<(), String> {
    if *request.method() != Method::Post {
        return request
            .respond(not_allowed("POST"))
            .map_err(|error| error.to_string());
    }
    let (status, body) = match typed(&mut request, id, access, shared) {
        Ok(answer) => answer,
        Err((status, error)) => (status, json!({ "error": error })),
    };
    request
        .respond(json_response(status, &body))
        .map_err(|error| error.to_string())
}

/// The body of `POST /api/sessions/<id>/input`: a person's text and its
/// idempotency key, or `{"probe": true}` alone. Nothing else: no action
/// type, no agent, no principal.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct InputBody {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    probe: bool,
}

/// The checks of a person's input after its method, then the probe's answer
/// or the submission: `(status, answer)`, or `(status, why)` when refused.
/// The guards every person's write passes after its method (module doc),
/// in order, then its body and its `Idempotency-Key` header. `what` names
/// the write in the refusals ("an input", "a key").
fn write_body(
    request: &mut Request,
    access: Access,
    shared: &Shared,
    what: &str,
) -> Result<(Vec<u8>, Option<String>), (u16, String)> {
    if !access.token {
        if !same_origin(header(request, "Host"), header(request, "Origin")) {
            return Err((
                403,
                format!("forbidden: {what} without the token needs this page's own Origin"),
            ));
        }
        if shared.token.is_none() && !owners_client(request.remote_addr().copied(), shared.port) {
            return Err((
                403,
                "forbidden: with no SPAWN_SERVE_TOKEN set, only the owner's own client on this \
                 machine types"
                    .to_string(),
            ));
        }
    }
    if !is_json(header(request, "Content-Type")) {
        return Err((
            415,
            format!("{what} is a JSON body (Content-Type: application/json)"),
        ));
    }
    let header_key = header(request, "Idempotency-Key").map(str::to_string);
    let body = read_body(request)?;
    Ok((body, header_key))
}

/// A write's idempotency key, from its body or its `Idempotency-Key`
/// header (the same when both), so a retried POST makes one command.
pub(super) fn idempotency(
    body: Option<String>,
    header: Option<String>,
    what: &str,
) -> Result<IdempotencyKey, (u16, String)> {
    match (body, header) {
        (Some(body), Some(header)) if body != header => Err((
            400,
            format!("{BAD_REQUEST}the key and the Idempotency-Key header differ"),
        )),
        (Some(key), _) | (None, Some(key)) => IdempotencyKey::new(key)
            .map_err(|error| (400, format!("{BAD_REQUEST}the key: {error}"))),
        (None, None) => Err((
            400,
            format!(
                "{BAD_REQUEST}{what} needs an idempotency key (\"key\", or an \
                 Idempotency-Key header), so that a retried POST makes one command"
            ),
        )),
    }
}

fn typed(
    request: &mut Request,
    id: &str,
    access: Access,
    shared: &Shared,
) -> Result<(u16, Value), (u16, String)> {
    let (body, header_key) = write_body(request, access, shared, "an input")?;
    let pool = human_pool(shared);
    apply_input_body_checked(&body, id, header_key, false, pool.as_ref())
}

fn with_human_writer<T>(
    human: Result<&Arc<ipc::RoleConnPool>, &(u16, String)>,
    op: impl FnOnce(&spawn_postgres::PostgresStore) -> Result<T, spawn_core::port::StoreError>,
) -> Result<T, (u16, String)> {
    let pool = human.map_err(Clone::clone)?;
    ipc::with_write(pool, op)
}

fn probe_human_writer(
    human: Result<&Arc<ipc::RoleConnPool>, &(u16, String)>,
) -> Result<(), (u16, String)> {
    let pool = human.map_err(Clone::clone)?;
    let _ = pool
        .checkout_verified()
        .map_err(ipc::PoolError::status_and_message)?;
    Ok(())
}

pub(super) fn apply_input_body(
    raw_body: &[u8],
    id: &str,
    header_key: Option<String>,
    allow_header_key_on_probe: bool,
    human: &Arc<ipc::RoleConnPool>,
) -> Result<(u16, Value), (u16, String)> {
    apply_input_body_checked(
        raw_body,
        id,
        header_key,
        allow_header_key_on_probe,
        Ok(human),
    )
}

fn apply_input_body_checked(
    raw_body: &[u8],
    id: &str,
    header_key: Option<String>,
    allow_header_key_on_probe: bool,
    human: Result<&Arc<ipc::RoleConnPool>, &(u16, String)>,
) -> Result<(u16, Value), (u16, String)> {
    let body: InputBody = serde_json::from_slice(raw_body).map_err(|error| {
        (
            400,
            format!("{BAD_REQUEST}the body is not an input: {error}"),
        )
    })?;
    let session = session_id(id).map_err(|error| (400, error))?;
    if body.probe {
        if body.text.is_some()
            || body.key.is_some()
            || (!allow_header_key_on_probe && header_key.is_some())
        {
            return Err((
                400,
                format!("{BAD_REQUEST}a probe carries no text and no key"),
            ));
        }
        probe_human_writer(human)?;
        return Ok((
            200,
            json!({ "writable": true, "session_id": session.as_str(),
                    "max_text_bytes": MAX_INPUT_TEXT }),
        ));
    }
    let key = idempotency(body.key, header_key, "an input")?;
    let text = typed_text(body.text).map_err(|error| (400, format!("{BAD_REQUEST}{error}")))?;
    match with_human_writer(human, |store| {
        store.submit_human_input(&session, &key, &text)
    })? {
        spawn_postgres::HumanInput::Submitted(command) => Ok((
            202,
            json!({ "command_id": command.0, "session_id": session.as_str(),
                    "principal": "human" }),
        )),
        spawn_postgres::HumanInput::NoAgent => Err((
            404,
            format!(
                "no agent has used {}: there is nothing to type into",
                session.as_str()
            ),
        )),
        // Nothing runs: refused with the store's reason, before any command
        // is made (objective 040), so the person learns it at once. A key
        // another request used is refused the same way (#16 item 8).
        spawn_postgres::HumanInput::NotRunning(why) | spawn_postgres::HumanInput::KeyUsed(why) => {
            Err((409, why))
        }
    }
}

/// `POST /api/sessions/<id>/key`, past the read guard (`access`): a
/// person's named key for session `id` (module doc, backlog P1).
fn key(mut request: Request, id: &str, access: Access, shared: &Shared) -> Result<(), String> {
    if *request.method() != Method::Post {
        return request
            .respond(not_allowed("POST"))
            .map_err(|error| error.to_string());
    }
    let (status, body) = match pressed(&mut request, id, access, shared) {
        Ok(answer) => answer,
        Err((status, error)) => (status, json!({ "error": error })),
    };
    request
        .respond(json_response(status, &body))
        .map_err(|error| error.to_string())
}

/// The body of `POST /api/sessions/<id>/key`: the key's name (`escape`,
/// `up`, `down`, `left`, `right`, `enter`, `y`, `n`) and the idempotency
/// key. Nothing else: no text, no action type, no agent, no principal.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    key: Option<String>,
}

/// The checks of a person's key after its method, then the submission:
/// `(status, answer)`, or `(status, why)` when refused. A session no agent
/// has used is 404; one that runs no execution is 409 with the store's
/// reason, and no command is made.
fn pressed(
    request: &mut Request,
    id: &str,
    access: Access,
    shared: &Shared,
) -> Result<(u16, Value), (u16, String)> {
    let (body, header_key) = write_body(request, access, shared, "a key")?;
    let pool = human_pool(shared);
    apply_key_body_checked(&body, id, header_key, pool.as_ref())
}

pub(super) fn apply_key_body(
    raw_body: &[u8],
    id: &str,
    header_key: Option<String>,
    human: &Arc<ipc::RoleConnPool>,
) -> Result<(u16, Value), (u16, String)> {
    apply_key_body_checked(raw_body, id, header_key, Ok(human))
}

fn apply_key_body_checked(
    raw_body: &[u8],
    id: &str,
    header_key: Option<String>,
    human: Result<&Arc<ipc::RoleConnPool>, &(u16, String)>,
) -> Result<(u16, Value), (u16, String)> {
    let body: KeyBody = serde_json::from_slice(raw_body)
        .map_err(|error| (400, format!("{BAD_REQUEST}the body is not a key: {error}")))?;
    let session = session_id(id).map_err(|error| (400, error))?;
    let key = idempotency(body.key, header_key, "a key")?;
    let name = body
        .name
        .ok_or_else(|| (400, format!("{BAD_REQUEST}a key needs a name")))?;
    let name =
        NamedKey::from_name(&name).map_err(|error| (400, format!("{BAD_REQUEST}{error}")))?;
    match with_human_writer(human, |store| store.submit_human_key(&session, &key, name))? {
        spawn_postgres::HumanKey::Submitted(command) => Ok((
            202,
            json!({ "command_id": command.0, "session_id": session.as_str(),
                    "principal": "human", "key": name.name() }),
        )),
        spawn_postgres::HumanKey::NoAgent => Err((
            404,
            format!(
                "no agent has used {}: there is nothing to press keys into",
                session.as_str()
            ),
        )),
        spawn_postgres::HumanKey::NotRunning(why) | spawn_postgres::HumanKey::KeyUsed(why) => {
            Err((409, why))
        }
    }
}

/// `POST /api/sessions/<id>/restart` and `/reassign`, past the read guard
/// (`access`): a person restarts the delivered task of session `id`, or
/// reassigns it to a fresh worker (module doc).
fn moved(
    mut request: Request,
    id: &str,
    action: &str,
    access: Access,
    shared: &Shared,
) -> Result<(), String> {
    if *request.method() != Method::Post {
        return request
            .respond(not_allowed("POST"))
            .map_err(|error| error.to_string());
    }
    let (status, body) = match move_task(&mut request, id, action, access, shared) {
        Ok(answer) => answer,
        Err((status, error)) => (status, json!({ "error": error })),
    };
    request
        .respond(json_response(status, &body))
        .map_err(|error| error.to_string())
}

/// The body of `POST /api/sessions/<id>/restart` and `/reassign`: why and the
/// idempotency key (`key`, or the `Idempotency-Key` header).
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MoveBody {
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    key: Option<String>,
}

/// Longest reason a person gives for a restart or a reassign, in bytes
/// (`spawn_human_task`).
const MAX_REASON: usize = 2000;

/// The checks of a person's restart or reassign after its method, then the
/// record: `(202, {task, session_id, agent_id, principal})` of the task and
/// worker that now carry the work (replaying the same answer when the same
/// key is sent again with the same session, action and reason), or
/// `(status, why)` when refused (409 when no delivered task or the key was
/// already used for another move).
fn move_task(
    request: &mut Request,
    id: &str,
    action: &str,
    access: Access,
    shared: &Shared,
) -> Result<(u16, Value), (u16, String)> {
    let what = if action == "restart" {
        "a restart"
    } else {
        "a reassign"
    };
    let (body, header_key) = write_body(request, access, shared, what)?;
    let pool = human_pool(shared);
    apply_move_body_checked(&body, id, action, header_key, pool.as_ref())
}

pub(super) fn apply_move_body(
    raw_body: &[u8],
    id: &str,
    action: &str,
    header_key: Option<String>,
    human: &Arc<ipc::RoleConnPool>,
) -> Result<(u16, Value), (u16, String)> {
    apply_move_body_checked(raw_body, id, action, header_key, Ok(human))
}

fn apply_move_body_checked(
    raw_body: &[u8],
    id: &str,
    action: &str,
    header_key: Option<String>,
    human: Result<&Arc<ipc::RoleConnPool>, &(u16, String)>,
) -> Result<(u16, Value), (u16, String)> {
    let what = if action == "restart" {
        "a restart"
    } else {
        "a reassign"
    };
    let body: MoveBody = serde_json::from_slice(raw_body)
        .map_err(|error| (400, format!("{BAD_REQUEST}the body is not {what}: {error}")))?;
    let session = session_id(id).map_err(|error| (400, error))?;
    let key = idempotency(body.key, header_key, what)?;
    let reason = body
        .reason
        .map(|reason| reason.trim().to_string())
        .filter(|reason| !reason.is_empty());
    if let Some(reason) = &reason {
        if reason.len() > MAX_REASON {
            return Err((
                400,
                format!(
                    "{BAD_REQUEST}the reason is {} bytes; at most {MAX_REASON}",
                    reason.len()
                ),
            ));
        }
        if reason.chars().any(char::is_control) {
            return Err((
                400,
                format!("{BAD_REQUEST}the reason holds a control character"),
            ));
        }
    }
    match with_human_writer(human, |store| {
        store.human_task(&session, action, reason.as_deref(), &key)
    })? {
        spawn_postgres::HumanTask::Moved(mut moved) => {
            if let Some(fields) = moved.as_object_mut() {
                fields.insert("principal".to_string(), json!("human"));
            }
            Ok((202, moved))
        }
        spawn_postgres::HumanTask::Refused(why) => Err((409, why)),
    }
}

/// `POST /api/master/message`, past the read guard (`access`): a person's
/// message to the supervisor from Master Control (issue #91 P1a; module
/// doc), or a probe of whether one would be taken.
fn master_message(mut request: Request, access: Access, shared: &Shared) -> Result<(), String> {
    if *request.method() != Method::Post {
        return request
            .respond(not_allowed("POST"))
            .map_err(|error| error.to_string());
    }
    let (status, body) = match messaged(&mut request, access, shared) {
        Ok(answer) => answer,
        Err((status, error)) => (status, json!({ "error": error })),
    };
    request
        .respond(json_response(status, &body))
        .map_err(|error| error.to_string())
}

/// The body of `POST /api/master/message`: the text, its mode (`auto`,
/// `steer`, `queue`, `restart`; `auto` when left out) and the idempotency
/// key, or `{"probe": true}` alone. Nothing else: no session (it is the
/// supervisor's), no agent, no principal.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MasterBody {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    probe: bool,
}

/// The checks of a person's message after its method (the input's guards,
/// in its order), then the probe's answer or the submission: `(202,
/// {command_id, wake_command_id, session_id, mode, principal, ...})`, or
/// `(status, why)` when refused. A stopped or idle supervisor is woken,
/// never refused: the only 409 is a key another request used.
fn messaged(
    request: &mut Request,
    access: Access,
    shared: &Shared,
) -> Result<(u16, Value), (u16, String)> {
    let (body, header_key) = write_body(request, access, shared, "a message")?;
    let pool = human_pool(shared);
    apply_master_body_checked(&body, header_key, false, pool.as_ref())
}

pub(super) fn apply_master_body(
    raw_body: &[u8],
    header_key: Option<String>,
    allow_header_key_on_probe: bool,
    human: &Arc<ipc::RoleConnPool>,
) -> Result<(u16, Value), (u16, String)> {
    apply_master_body_checked(raw_body, header_key, allow_header_key_on_probe, Ok(human))
}

fn apply_master_body_checked(
    raw_body: &[u8],
    header_key: Option<String>,
    allow_header_key_on_probe: bool,
    human: Result<&Arc<ipc::RoleConnPool>, &(u16, String)>,
) -> Result<(u16, Value), (u16, String)> {
    let body: MasterBody = serde_json::from_slice(raw_body).map_err(|error| {
        (
            400,
            format!("{BAD_REQUEST}the body is not a message: {error}"),
        )
    })?;
    if body.probe {
        if body.text.is_some()
            || body.key.is_some()
            || body.mode.is_some()
            || (!allow_header_key_on_probe && header_key.is_some())
        {
            return Err((
                400,
                format!("{BAD_REQUEST}a probe carries no text, no mode and no key"),
            ));
        }
        probe_human_writer(human)?;
        return Ok((
            200,
            json!({ "writable": true, "session_id": crate::supervise::SUPERVISOR_SESSION,
                    "max_text_bytes": MAX_INPUT_TEXT,
                    "max_key_bytes": spawn_postgres::MAX_MASTER_KEY_BYTES,
                    "modes": spawn_postgres::MasterMessageMode::ALL
                        .iter()
                        .map(|mode| mode.name())
                        .collect::<Vec<_>>() }),
        ));
    }
    let mode = spawn_postgres::MasterMessageMode::from_name(body.mode.as_deref().unwrap_or("auto"))
        .map_err(|error| (400, format!("{BAD_REQUEST}{error}")))?;
    let key = idempotency(body.key, header_key, "a message")?;
    if key.as_str().len() > spawn_postgres::MAX_MASTER_KEY_BYTES {
        return Err((
            400,
            format!(
                "{BAD_REQUEST}a message's idempotency key is at most {} bytes",
                spawn_postgres::MAX_MASTER_KEY_BYTES
            ),
        ));
    }
    let text = typed_text(body.text).map_err(|error| (400, format!("{BAD_REQUEST}{error}")))?;
    match with_human_writer(human, |store| store.human_master_message(&key, &text, mode))? {
        spawn_postgres::HumanMaster::Submitted(answer) => Ok((202, answer)),
        spawn_postgres::HumanMaster::KeyUsed(why) => Err((409, why)),
        spawn_postgres::HumanMaster::Refused(why) => Err((400, format!("{BAD_REQUEST}{why}"))),
    }
}

/// `POST /api/missions/<id>/resume`, past the read guard (`access`): a
/// person resumes mission `id` on the halt the page showed (module doc).
fn resume(mut request: Request, id: &str, access: Access, shared: &Shared) -> Result<(), String> {
    if *request.method() != Method::Post {
        return request
            .respond(not_allowed("POST"))
            .map_err(|error| error.to_string());
    }
    let (status, body) = match resumed(&mut request, id, access, shared) {
        Ok(answer) => answer,
        Err((status, error)) => (status, json!({ "error": error })),
    };
    request
        .respond(json_response(status, &body))
        .map_err(|error| error.to_string())
}

/// The body of `POST /api/missions/<id>/resume`: the halt it resumes (its
/// `mission.halted` event) and the idempotency key, or `{"probe": true}`
/// alone. Nothing else: no principal, no reason.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ResumeBody {
    #[serde(default)]
    halt: Option<i64>,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    probe: bool,
}

/// The checks of a person's resume after its method, then the probe's
/// answer or the record: `(202, {event})`, or `(status, why)` when refused.
fn resumed(
    request: &mut Request,
    id: &str,
    access: Access,
    shared: &Shared,
) -> Result<(u16, Value), (u16, String)> {
    let (body, header_key) = write_body(request, access, shared, "a resume")?;
    let pool = human_pool(shared);
    apply_resume_body_checked(&body, id, header_key, false, pool.as_ref())
}

pub(super) fn apply_resume_body(
    raw_body: &[u8],
    id: &str,
    header_key: Option<String>,
    allow_header_key_on_probe: bool,
    human: &Arc<ipc::RoleConnPool>,
) -> Result<(u16, Value), (u16, String)> {
    apply_resume_body_checked(
        raw_body,
        id,
        header_key,
        allow_header_key_on_probe,
        Ok(human),
    )
}

fn apply_resume_body_checked(
    raw_body: &[u8],
    id: &str,
    header_key: Option<String>,
    allow_header_key_on_probe: bool,
    human: Result<&Arc<ipc::RoleConnPool>, &(u16, String)>,
) -> Result<(u16, Value), (u16, String)> {
    let body: ResumeBody = serde_json::from_slice(raw_body).map_err(|error| {
        (
            400,
            format!("{BAD_REQUEST}the body is not a resume: {error}"),
        )
    })?;
    let mission: i64 = id
        .parse()
        .ok()
        .filter(|mission| *mission > 0)
        .ok_or_else(|| (400, format!("{BAD_REQUEST}bad mission {id}")))?;
    if body.probe {
        if body.halt.is_some()
            || body.key.is_some()
            || (!allow_header_key_on_probe && header_key.is_some())
        {
            return Err((
                400,
                format!("{BAD_REQUEST}a probe carries no halt and no key"),
            ));
        }
        probe_human_writer(human)?;
        return Ok((200, json!({ "writable": true, "mission": mission })));
    }
    let key = idempotency(body.key, header_key, "a resume")?;
    let halt = body.halt.ok_or_else(|| {
        (
            400,
            format!(
                "{BAD_REQUEST}a resume names the halt it resumes (\"halt\", the mission's \
                 halt.event), so that a page not refreshed resumes no other"
            ),
        )
    })?;
    match with_human_writer(human, |store| store.human_resume(mission, halt, &key))? {
        spawn_postgres::HumanResume::Resumed(event) => Ok((
            202,
            json!({ "event": event, "mission": mission, "halt": halt, "principal": "human" }),
        )),
        spawn_postgres::HumanResume::NoMission(why) => Err((404, why)),
        spawn_postgres::HumanResume::Refused(why) => Err((409, why)),
    }
}

/// `GET /api/features/coverage`: current feature-map file coverage report,
/// boundary policy, exclusions, wild files, stale references, and features.
fn feature_coverage_get() -> Result<Value, String> {
    let root = crate::feature_inventory::default_root();
    let registry = crate::feature_inventory::default_registry_path(&root);
    crate::feature_inventory::api_coverage_payload(&root, &registry)
}

pub(super) fn feature_coverage_for_query(query: &str) -> (u16, Value) {
    match feature_paths(query) {
        Ok((root, registry)) => {
            match crate::feature_inventory::api_coverage_payload(&root, &registry) {
                Ok(payload) => (200, payload),
                Err(error) => (500, json!({ "error": error })),
            }
        }
        Err(refusal) => refusal,
    }
}

/// `GET /api/features/coverage?mission=<id>`: [`feature_coverage_get`] for
/// that mission's project ([`feature_paths`]), its refusals structured.
fn feature_coverage_mission_get(
    request: Request,
    query: &str,
    shared: &Shared,
) -> Result<(), String> {
    let (status, body) = with_shared_reader(
        shared,
        || feature_coverage_for_query(query),
        |(st, _)| *st == 500,
        |err| {
            let (st, msg) = err.status_and_message();
            (st, json!({ "error": msg }))
        },
    );
    request
        .respond(json_response(status, &body))
        .map_err(|error| error.to_string())
}

/// The project root and feature registry a coverage request reads: without
/// `?mission` the server's own (`SPAWN_FEATURE_ROOT`, else the working
/// directory; `SPAWN_FEATURE_REGISTRY`, else the root's registry); with it,
/// the open objective mission's `--features` file and the project root it
/// belongs to ([`mission_feature_paths`]).
pub(super) fn feature_paths(query: &str) -> Result<(PathBuf, PathBuf), (u16, Value)> {
    let Some(text) = mission_query(query) else {
        let root = crate::feature_inventory::default_root();
        let registry = crate::feature_inventory::default_registry_path(&root);
        return Ok((root, registry));
    };
    let Ok(id) = text.parse::<i64>() else {
        return Err((
            400,
            json!({ "error": format!("{BAD_REQUEST}bad mission {text}"), "code": "bad_mission" }),
        ));
    };
    let missions = open_store()
        .and_then(|store| store.objective_missions().map_err(err))
        .map_err(|error| (500, json!({ "error": error })))?;
    let (root, registry) = mission_feature_paths(&missions, id)?;
    coverage_registry_checked(&registry, id)?;
    Ok((root, registry))
}

/// A query parameter as given (`None` when absent, `Some("")` for bare
/// `?name` or `?name=`).
fn query_param<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query
        .split('&')
        .find_map(|pair| match pair.split_once('=') {
            Some((key, value)) => (key == name).then_some(value),
            None => (pair == name).then_some(""),
        })
}

/// The `mission` of a coverage request's query as given: `None` when the
/// query names none, `Some("")` for a bare `mission` or an empty
/// `mission=` (refused as a bad mission, never read as the default
/// project).
pub(super) fn mission_query(query: &str) -> Option<&str> {
    query_param(query, "mission")
}

/// 400 `not_a_coverage_registry` when mission `id`'s `--features` file is
/// not a coverage registry [`crate::feature_inventory::load_registry`]
/// takes (missing, unreadable, or a legacy identity-only feature map).
fn coverage_registry_checked(registry: &Path, id: i64) -> Result<(), (u16, Value)> {
    crate::feature_inventory::load_registry(registry)
        .map(|_| ())
        .map_err(|error| {
            (
                400,
                json!({
                    "error": format!(
                        "{BAD_REQUEST}mission {id}'s --features file is not a coverage \
                         registry: {error}"
                    ),
                    "code": "not_a_coverage_registry",
                    "mission": id,
                }),
            )
        })
}

/// Mission `id`'s registry and project root ([`mission_paths`]), from the
/// open objective missions `missions`: 404 when there is no such mission.
fn mission_feature_paths(missions: &Value, id: i64) -> Result<(PathBuf, PathBuf), (u16, Value)> {
    let mission = missions
        .as_array()
        .into_iter()
        .flatten()
        .find(|mission| mission["id"].as_i64() == Some(id))
        .ok_or_else(|| {
            (
                404,
                json!({ "error": format!("no open objective mission {id}"),
                        "code": "mission_not_found", "mission": id }),
            )
        })?;
    mission_paths(mission, id)
}

/// Mission row `mission`'s (id `id`) registry, its `objectives.features` as
/// configured (a relative path beside its backlog; never rewritten, so it
/// is opened and saved as the OS resolves it, symlinks and `..` included),
/// and the project root it belongs to; 400 when it names no `--features`
/// registry.
fn mission_paths(mission: &Value, id: i64) -> Result<(PathBuf, PathBuf), (u16, Value)> {
    let refused = |status: u16, code: &str, error: String| {
        (
            status,
            json!({ "error": error, "code": code, "mission": id }),
        )
    };
    let given = mission
        .pointer("/objectives/features")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            refused(
                400,
                "no_features_registry",
                format!(
                    "{BAD_REQUEST}mission {id} has no --features registry configured \
                     (spawn mission configure {id} --features <registry.json>)"
                ),
            )
        })?;
    let backlog = json!({ "backlog": mission.pointer("/objectives/backlog") });
    let registry = feature_map::resolve(&backlog, given)
        .map_err(|error| refused(400, "no_features_registry", format!("{BAD_REQUEST}{error}")))?;
    let root = crate::feature_inventory::project_root_for_registry(&registry);
    Ok((root, registry))
}

/// `GET /api/features/coverage/projects`: every project the coverage page
/// can read ([`coverage_projects`]), over all open objective missions (no
/// `/api/features` mission limit).
fn feature_coverage_projects() -> Result<Value, String> {
    let store = open_store()?;
    let missions = store.objective_missions().map_err(err)?;
    Ok(coverage_projects(&missions))
}

/// Resolve `(root, registry)` for one eligible open objective mission (used
/// by [`coverage_projects`] and named `?project=` resolution).
pub(super) fn coverage_mission_paths(mission: &Value, id: i64) -> Option<(PathBuf, PathBuf)> {
    let (root, registry) = mission_paths(mission, id).ok()?;
    coverage_registry_checked(&registry, id).ok()?;
    Some((root, registry))
}

/// `{"projects": [{mission, text, registry_path, root}]}`: each mission of
/// `missions` whose `--features` file is a coverage registry
/// ([`coverage_registry_checked`]), in mission order; a mission without
/// one, or with a legacy identity-only map, is left out.
fn coverage_projects(missions: &Value) -> Value {
    let projects: Vec<Value> = missions
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|mission| {
            let id = mission["id"].as_i64()?;
            let (root, registry) = coverage_mission_paths(mission, id)?;
            Some(json!({
                "mission": id,
                "text": mission["text"],
                "registry_path": registry.display().to_string(),
                "root": root.display().to_string(),
            }))
        })
        .collect();
    json!({ "projects": projects })
}

/// `POST /api/features/scan`: one-click unassociated-file ("野文件") scan.
/// Performs a fresh read-only project inventory scan; never modifies or
/// deletes any file on disk.
fn feature_scan_post(
    mut request: Request,
    query: &str,
    access: Access,
    shared: &Shared,
) -> Result<(), String> {
    if *request.method() != Method::Post {
        return request
            .respond(not_allowed("POST"))
            .map_err(|error| error.to_string());
    }
    if !access.token && !same_origin(header(&request, "Host"), header(&request, "Origin")) {
        return request
            .respond(json_response(
                403,
                &json!({ "error": "forbidden: a scan without the token needs this page's own Origin" }),
            ))
            .map_err(|error| error.to_string());
    }
    let _ = read_body(&mut request);
    let paths = with_shared_reader(
        shared,
        || feature_paths(query),
        |res| matches!(res, Err((500, _))),
        |err| {
            let (st, msg) = err.status_and_message();
            Err((st, json!({ "error": msg })))
        },
    );
    let (status, body) = match paths {
        Ok((root, registry)) => {
            match crate::feature_inventory::api_coverage_payload(&root, &registry) {
                Ok(payload) => (200, payload),
                Err(error) => (500, json!({ "error": error })),
            }
        }
        Err(refusal) => refusal,
    };
    request
        .respond(json_response(status, &body))
        .map_err(|error| error.to_string())
}

/// Body of `POST /api/features/associate`: one repo-relative file path and
/// one or more genuine feature IDs to associate it with (many-to-many).
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FeatureAssociateBody {
    path: String,
    features: Vec<String>,
}

/// `POST /api/features/associate`: validate and atomically persist a
/// many-to-many file-to-feature association, returning the refreshed scan
/// report and association update summary.
fn feature_associate_post(
    mut request: Request,
    query: &str,
    access: Access,
    shared: &Shared,
) -> Result<(), String> {
    if *request.method() != Method::Post {
        return request
            .respond(not_allowed("POST"))
            .map_err(|error| error.to_string());
    }
    let (status, body) = match feature_associate_checked(&mut request, query, access, shared) {
        Ok(answer) | Err(FeatureAssociateRefusal::Structured(answer)) => answer,
        Err(FeatureAssociateRefusal::Plain((status, error))) => (status, json!({ "error": error })),
    };
    request
        .respond(json_response(status, &body))
        .map_err(|error| error.to_string())
}

/// Why `POST /api/features/associate` did not save: a plain `(status,
/// message)` answered as `{"error": message}`, or a complete JSON body (the
/// read-only registry refusal carries its own fields).
#[derive(Debug)]
pub(super) enum FeatureAssociateRefusal {
    Plain((u16, String)),
    Structured((u16, Value)),
}

impl From<(u16, String)> for FeatureAssociateRefusal {
    fn from(plain: (u16, String)) -> Self {
        Self::Plain(plain)
    }
}

fn feature_associate_checked(
    request: &mut Request,
    query: &str,
    access: Access,
    shared: &Shared,
) -> Result<(u16, Value), FeatureAssociateRefusal> {
    let (raw_body, _) = write_body(request, access, shared, "a feature association")?;
    let (root, registry) = with_shared_reader(
        shared,
        || feature_paths(query),
        |res| matches!(res, Err((500, _))),
        |err| {
            let (st, msg) = err.status_and_message();
            Err((st, json!({ "error": msg })))
        },
    )
    .map_err(FeatureAssociateRefusal::Structured)?;
    feature_associate_at(&root, &registry, &raw_body)
}

/// The association itself, against an explicit project root and registry
/// path: parse the body, refuse a read-only registry with a structured 409
/// before anything is attempted, then validate and save.
pub(super) fn feature_associate_at(
    root: &std::path::Path,
    registry: &std::path::Path,
    raw_body: &[u8],
) -> Result<(u16, Value), FeatureAssociateRefusal> {
    let body: FeatureAssociateBody = serde_json::from_slice(raw_body).map_err(|error| {
        (
            400,
            format!(
                "{BAD_REQUEST}the body must be {{\"path\": ..., \"features\": [...]}}: {error}"
            ),
        )
    })?;
    let read_only = |reason: Option<String>| {
        let reason = reason.unwrap_or_else(|| "the feature registry is read-only".to_string());
        FeatureAssociateRefusal::Structured((
            409,
            json!({
                "error": format!(
                    "the feature registry on this deployment is read-only, so the association \
                     was not saved: {reason}"
                ),
                "code": "registry_read_only",
                "registry_writable": false,
                "read_only_reason": reason,
                "registry_path": registry.display().to_string(),
            }),
        ))
    };
    let writability = crate::feature_inventory::probe_registry_writable(registry);
    if !writability.writable {
        return Err(read_only(writability.read_only_reason));
    }
    crate::feature_inventory::api_associate_payload(root, registry, &body.path, &body.features)
        .map(|payload| (200, payload))
        .map_err(|error| {
            // The registry may have turned read-only between the probe and
            // the save: answer that the same way, never as a raw OS error.
            let again = crate::feature_inventory::probe_registry_writable(registry);
            if again.writable {
                FeatureAssociateRefusal::Plain((400, format!("{BAD_REQUEST}{error}")))
            } else {
                read_only(again.read_only_reason)
            }
        })
}

/// Whether `origin` is this page's own: present, and the same host over
/// http as the request's `Host`.
fn same_origin(host: Option<&str>, origin: Option<&str>) -> bool {
    let (Some(host), Some(origin)) = (host, origin) else {
        return false;
    };
    origin
        .trim()
        .strip_prefix("http://")
        .is_some_and(|rest| authority(rest) == authority(host))
}

/// Whether a `Content-Type` value is JSON's (`application/json`, with any
/// parameters).
fn is_json(content_type: Option<&str>) -> bool {
    content_type
        .and_then(|value| value.split(';').next())
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
}

/// A request's body, at most [`MAX_INPUT_BODY`] bytes (413 past it: a
/// declared length is refused unread).
fn read_body(request: &mut Request) -> Result<Vec<u8>, (u16, String)> {
    use std::io::Read;
    let too_big = || {
        (
            413,
            format!("the body is over {MAX_INPUT_BODY} bytes, the most an input takes"),
        )
    };
    if request
        .body_length()
        .is_some_and(|length| length > MAX_INPUT_BODY)
    {
        return Err(too_big());
    }
    let mut body = Vec::new();
    let limit = u64::try_from(MAX_INPUT_BODY).unwrap_or(u64::MAX) + 1;
    Read::take(request.as_reader(), limit)
        .read_to_end(&mut body)
        .map_err(|error| {
            (
                400,
                format!("{BAD_REQUEST}the body could not be read: {error}"),
            )
        })?;
    if body.len() > MAX_INPUT_BODY {
        return Err(too_big());
    }
    Ok(body)
}

/// `text` as a person types it, or why it is not: present, not blank, at
/// most [`MAX_INPUT_TEXT`] bytes, and no control character but tab and line
/// ends (an escape would reach the engine's terminal as keys).
fn typed_text(text: Option<String>) -> Result<String, String> {
    let text = text.ok_or("an input needs a text")?;
    if text.trim().is_empty() {
        return Err("the text is empty".to_string());
    }
    if text.len() > MAX_INPUT_TEXT {
        return Err(format!(
            "the text is {} bytes; an input is at most {MAX_INPUT_TEXT}",
            text.len()
        ));
    }
    if text
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\t' | '\n' | '\r'))
    {
        return Err(
            "the text holds a control character; only tab and line ends are typed".to_string(),
        );
    }
    Ok(text)
}

fn json_response(status: u16, body: &Value) -> Response<std::io::Cursor<Vec<u8>>> {
    let header = Header::from_bytes("Content-Type", "application/json; charset=utf-8")
        .expect("static header is valid");
    let response = Response::from_string(body.to_string())
        .with_status_code(status)
        .with_header(header);
    match successor_link() {
        Some(link) => response.with_header(link),
        None => response,
    }
}

fn overview() -> Result<Value, String> {
    let store = open_store()?;
    let sessions: Vec<Value> = store
        .states()
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|state| {
            // The list shows the judgement's state, reason, confidence and
            // judge; its evidence (the screen it saw, most of the bytes) is
            // the session's own read (`/api/sessions/<id>`, `/api/fleet`).
            let mut observation = json!(state.observation);
            if let Some(fields) = observation.as_object_mut() {
                fields.remove("evidence");
            }
            json!({
                "session_id": state.session_id.as_str(),
                "agent_id": state.agent_id.as_str(),
                "observation": observation,
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
/// at_ms}` or null), each mission with the owner's `hold` on its source (the
/// active hold's event id, or null), and `holds`: every active hold
/// (`{source, hold, main, by, reason, held_at_ms, expires_ms, waiting}`). A task's `brief`, `review_note` and `report` are their
/// first lines, each with its length in bytes (`brief_bytes`, ...); a
/// closed mission's or a cancelled task's report is its length alone
/// ([`answers::slim_board`]). The texts in full are
/// `/api/tasks/<n>`. Each mission carries its `mission_name`, each
/// task its `label`, `task_title` and `mission_name`, each resident its
/// `label` ([`labels`]).
fn missions() -> Result<Value, String> {
    let store = open_store()?;
    let mut board = store.mission_board(50).map_err(err)?;
    let gates = store.last_gates().map_err(err)?;
    with_last_gates(&mut board, &gates);
    let activity = store.task_activity().map_err(err)?;
    with_activity(&mut board, &activity);
    with_task_liveness(&mut board, now_ms());
    labels::label_board(&mut board);
    answers::slim_board(&mut board);
    Ok(board)
}

/// `/api/verifications`: what `spawn verify list` prints (crate::verify),
/// read through the reader login on request.
fn verifications() -> Result<Value, String> {
    let store = open_store()?;
    let facts = store.verification_facts().map_err(err)?;
    Ok(crate::verify::list(&facts, now_ms()))
}

/// `/api/grading`: the grading catalog (docs/agent-grading.md) as the web
/// reads it ([`crate::grading::api_catalog_with_samples`]): every preset as
/// `spawn grade list --json` prints it, with its routing grade, the profile it
/// runs as (its named execution profile, else its model profile), `driver`,
/// `label_only`, `tier_role`, and whether it is due an anti-starvation
/// re-probe now; every profile the catalog names; and strictly additive
/// read-only PV5 28-day calibration window readiness metrics. Read through the
/// reader login like every other read.
fn grading() -> Result<Value, String> {
    let store = open_store()?;
    let catalog = store.grading_catalog().map_err(err)?;
    let activity = store.grading_activity().map_err(err)?;
    let samples = store.grading_pv5_samples().map_err(err)?;
    Ok(crate::grading::api_catalog_with_samples(
        &catalog,
        &activity,
        &samples,
        now_ms(),
        crate::grading::reprobe_interval_ms(),
    ))
}

/// Matches `nodes::v1::LOST_AFTER_MS` (`spawn_node_sweep` lease window).
const NODE_V1_LOST_AFTER_MS: i64 = 90_000;

fn v1_node_is_lost(node: &spawn_postgres::NodeV1, now: i64) -> bool {
    node.status == spawn_postgres::NodeStatus::Active
        && node
            .last_beat_ms
            .is_none_or(|beat| now.saturating_sub(beat) > NODE_V1_LOST_AFTER_MS)
}

fn v1_node_state(node: &spawn_postgres::NodeV1, now: i64) -> String {
    let mut state =
        if node.status == spawn_postgres::NodeStatus::Active && v1_node_is_lost(node, now) {
            "lost".to_string()
        } else {
            node.status.as_str().to_string()
        };
    if node.drained {
        state.push_str(",drained");
    }
    if node.degraded {
        state.push_str(",degraded");
    }
    state
}

fn v1_node_json(node: &spawn_postgres::NodeV1, now: i64) -> Value {
    json!({
        "name": node.name,
        "proto": "node-v1",
        "node_id": node.node_id,
        "cell_id": node.cell_id,
        "status": node.status,
        "state": v1_node_state(node, now),
        "drained": node.drained,
        "degraded": node.degraded,
        "lost": v1_node_is_lost(node, now),
        "host_fault_strikes": node.host_fault_strikes,
        "last_beat_ms": node.last_beat_ms,
        "spec_sha256": node.spec_sha256,
        "warm_bases": node.warm_bases,
        "power": node.power,
        "build_sha": node.build_sha,
        "node_version": node.node_version,
    })
}

fn merge_nodes_with_v1(
    mut p1: Value,
    v1: &[spawn_postgres::NodeV1],
    now: i64,
    threshold: u8,
) -> Value {
    crate::nodes::power::with_soft_drain(&mut p1, threshold);
    let mut all = match p1 {
        Value::Array(items) => items,
        Value::Null => Vec::new(),
        other => vec![other],
    };
    if !v1.is_empty() {
        let originals: Vec<String> = v1.iter().map(|node| v1_node_state(node, now)).collect();
        let mut v1_array = Value::Array(
            v1.iter()
                .zip(&originals)
                .map(|(node, original_state)| {
                    let mut obj = v1_node_json(node, now);
                    let mapped = if node.drained {
                        "drained"
                    } else if node.status == spawn_postgres::NodeStatus::Active
                        && !v1_node_is_lost(node, now)
                        && !node.degraded
                    {
                        "live"
                    } else {
                        original_state.as_str()
                    };
                    if let Some(fields) = obj.as_object_mut() {
                        fields.insert("state".to_string(), Value::String(mapped.to_string()));
                    }
                    obj
                })
                .collect(),
        );
        crate::nodes::power::with_soft_drain(&mut v1_array, threshold);
        if let Value::Array(mut items) = v1_array {
            for (entry, original_state) in items.iter_mut().zip(originals) {
                if let Some(fields) = entry.as_object_mut() {
                    fields.insert("state".to_string(), Value::String(original_state));
                }
            }
            all.extend(items);
        }
    }
    public_nodes(Value::Array(all))
}

/// `/api/nodes`: every legacy P1 node joined and not removed (multi-host P1,
/// crate nodes.rs) plus every enrolled `spawn-node` v1 node (`spawn_node_state`),
/// `{nodes: [...]}`, read through the reader login like every other read. No
/// clock in it, so an unchanged list keeps its ETag. Each node carries its
/// `power` (null without a battery or AC sensor) and the low-battery soft drain
/// at `SPAWN_NODE_LOW_BATTERY_PCT` (issue #91, crate nodes/power.rs):
/// `low_battery_drain`, `effective_drained` and `schedulable`, which leave
/// its `state` and the owner's drain as they are.
fn nodes() -> Result<Value, String> {
    let store = open_store()?;
    let p1 = store.nodes().map_err(|error| error.to_string())?;
    let v1 = match store.nodes_v1() {
        Ok(nodes) => nodes,
        Err(error) => {
            let message = error.to_string();
            if message.contains("spawn_node_state") && message.contains("does not exist") {
                Vec::new()
            } else {
                return Err(message);
            }
        }
    };
    let merged = merge_nodes_with_v1(
        p1,
        &v1,
        now_ms(),
        crate::nodes::power::low_battery_from_env(),
    );
    Ok(json!({ "nodes": merged }))
}

/// The store's nodes as the web reads them: without where each is reached
/// (its ssh destination, its tcp or udp endpoint, its coordinator's URL),
/// its note nor its public key; how it is reached (`transport`, `mode`,
/// its hops' names in `via`), its `state`, `role` and `zone` stay.
fn public_nodes(mut nodes: Value) -> Value {
    for node in nodes.as_array_mut().into_iter().flatten() {
        if let Some(node) = node.as_object_mut() {
            node.remove("ssh");
            node.remove("endpoint");
            node.remove("note");
            node.remove("pubkey");
        }
    }
    nodes
}

/// `/api/tasks/<n>`: one task's brief, report and review note in
/// full, `{id, mission_id, brief, report, review_note, updated_at_ms}`, as
/// the Missions page asks when a task is opened.
fn mission_task(task: &str) -> Result<Value, String> {
    let id: i64 = task
        .parse()
        .map_err(|_| format!("{BAD_REQUEST}bad task {task}"))?;
    let store = open_store()?;
    store
        .task_texts(id)
        .map_err(err)?
        .ok_or_else(|| format!("task {id} does not exist"))
}

const EPISODES_DEFAULT_LIMIT: usize = 50;
const EPISODES_MAX_LIMIT: usize = 200;
/// Safe JSON body ceiling well below `spawn-node-edge`'s 4 MiB (`4_194_304`) IPC cap.
const EPISODES_MAX_BODY_BYTES: usize = 3_500_000;

fn parse_episodes_mission_query(query: &str) -> Result<Option<i64>, String> {
    let Some(raw) = mission_query(query) else {
        return Ok(None);
    };
    raw.parse::<i64>()
        .map(Some)
        .map_err(|_| format!("{BAD_REQUEST}bad mission {raw}"))
}

fn parse_episodes_limit_query(query: &str) -> Result<usize, String> {
    let Some(raw) = query_param(query, "limit") else {
        return Ok(EPISODES_DEFAULT_LIMIT);
    };
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("{BAD_REQUEST}bad limit {raw}"));
    }
    Ok(raw
        .parse::<usize>()
        .unwrap_or(EPISODES_MAX_LIMIT)
        .min(EPISODES_MAX_LIMIT))
}

pub(in crate::serve) fn bound_episodes(
    mut episodes: Vec<spawn_core::control::TaskEpisode>,
    limit: usize,
) -> Vec<spawn_core::control::TaskEpisode> {
    episodes.sort_by_key(|ep| std::cmp::Reverse((ep.updated_at_ms, ep.assigned_seq, ep.task_id)));
    episodes.truncate(limit);
    episodes
}

fn truncate_utf8(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

pub(in crate::serve) fn episodes_payload(
    mission_id: Option<i64>,
    episodes: &[spawn_core::control::TaskEpisode],
    summary: &Value,
    parity_ok: bool,
    drifts: &[Value],
) -> Value {
    json!({
        "mission": mission_id,
        "episodes": episodes,
        "summary": summary,
        "parity_ok": parity_ok,
        "warnings": summary["warnings"].as_u64().unwrap_or(0),
        "drift_count": drifts.len(),
    })
}

fn bounded_episodes_payload_with_cap(
    mission_id: Option<i64>,
    episodes: Vec<spawn_core::control::TaskEpisode>,
    limit: usize,
    summary: &Value,
    parity_ok: bool,
    drifts: &[Value],
    max_bytes: usize,
) -> Value {
    let bounded = bound_episodes(episodes, limit);
    let payload = episodes_payload(mission_id, &bounded, summary, parity_ok, drifts);
    if payload.to_string().len() <= max_bytes {
        return payload;
    }
    let mut lo = 1usize;
    let mut hi = bounded.len();
    let mut best = 0usize;
    while lo <= hi {
        let mid = lo + (hi - lo) / 2;
        let candidate = episodes_payload(mission_id, &bounded[..mid], summary, parity_ok, drifts);
        if candidate.to_string().len() <= max_bytes {
            best = mid;
            lo = mid + 1;
        } else {
            hi = mid - 1;
        }
    }
    if best > 0 {
        return episodes_payload(mission_id, &bounded[..best], summary, parity_ok, drifts);
    }
    if let Some(first) = bounded.first() {
        let mut trimmed = first.clone();
        trimmed.brief = truncate_utf8(&trimmed.brief, 4096);
        trimmed.last_report = trimmed
            .last_report
            .as_deref()
            .map(|text| truncate_utf8(text, 4096));
        trimmed.review_note = trimmed
            .review_note
            .as_deref()
            .map(|text| truncate_utf8(text, 4096));
        let candidate = episodes_payload(
            mission_id,
            std::slice::from_ref(&trimmed),
            summary,
            parity_ok,
            drifts,
        );
        if candidate.to_string().len() <= max_bytes {
            return candidate;
        }
    }
    episodes_payload(mission_id, &[], summary, parity_ok, drifts)
}

pub(in crate::serve) fn bounded_episodes_payload(
    mission_id: Option<i64>,
    episodes: Vec<spawn_core::control::TaskEpisode>,
    limit: usize,
    summary: &Value,
    parity_ok: bool,
    drifts: &[Value],
) -> Value {
    bounded_episodes_payload_with_cap(
        mission_id,
        episodes,
        limit,
        summary,
        parity_ok,
        drifts,
        EPISODES_MAX_BODY_BYTES,
    )
}

/// Read-only `GET|HEAD /api/episodes[?mission=<id>&limit=<n>]` JSON answer with weak ETag
/// validation (no wall clock in the body so identical snapshots yield `304`).
pub(in crate::serve) fn episodes_answer(query: &str) -> answers::Answer {
    let read = (|| -> Result<Value, String> {
        let mission_id = parse_episodes_mission_query(query)?;
        let limit = parse_episodes_limit_query(query)?;
        let store = open_store()?;
        let (episodes, summary, parity_ok, drifts) = store
            .episode_parity_report(mission_id, false)
            .map_err(err)?;
        Ok(bounded_episodes_payload(
            mission_id, episodes, limit, &summary, parity_ok, &drifts,
        ))
    })();
    answers::answer("/api/episodes", read, BAD_REQUEST)
}

const STORAGE_TREE_DEFAULT_LIMIT: usize = 120;
const STORAGE_TREE_MAX_LIMIT: usize = 500;
const STORAGE_PROJECT_MAX_LEN: usize = 256;
const STORAGE_REF_LOG_SCAN_MAX: usize = 500;
const STORAGE_HISTORY_MAX: usize = 80;
const SNAPSHOT_ANCESTOR_MAX_DEPTH: usize = 32;
const SNAPSHOT_CHILDREN_SCAN_MAX: usize = 256;

fn parse_storage_tree_query(query: &str) -> Result<(Option<String>, usize), String> {
    let project = parameter(query, "project")
        .map(|text| percent_decoded(text).ok_or_else(|| format!("{BAD_REQUEST}bad project {text}")))
        .transpose()?
        .filter(|text| !text.is_empty());
    if let Some(ref proj) = project
        && (proj.len() > STORAGE_PROJECT_MAX_LEN || proj.chars().any(char::is_control))
    {
        return Err(format!("{BAD_REQUEST}bad project"));
    }
    let limit = match parameter(query, "limit") {
        None => STORAGE_TREE_DEFAULT_LIMIT,
        Some(raw) => {
            if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(format!("{BAD_REQUEST}bad limit {raw}"));
            }
            let parsed: usize = raw
                .parse()
                .map_err(|_| format!("{BAD_REQUEST}bad limit {raw}"))?;
            if parsed == 0 || parsed > STORAGE_TREE_MAX_LIMIT {
                return Err(format!(
                    "{BAD_REQUEST}limit must be between 1 and {STORAGE_TREE_MAX_LIMIT}, got {parsed}"
                ));
            }
            parsed
        }
    };
    Ok((project, limit))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SnapshotLookup {
    Digest(String),
    GitOid(String),
}

impl SnapshotLookup {
    fn as_str(&self) -> &str {
        match self {
            Self::Digest(id) | Self::GitOid(id) => id.as_str(),
        }
    }
}

fn parse_snapshot_lookup(raw: &str) -> Result<SnapshotLookup, String> {
    let decoded =
        percent_decoded(raw).ok_or_else(|| format!("{BAD_REQUEST}bad snapshot id {raw}"))?;
    let normalized = decoded.to_ascii_lowercase();
    if let Some(hex) = normalized.strip_prefix("sha256:")
        && hex.len() == 64
        && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Ok(SnapshotLookup::Digest(normalized));
    }
    if normalized.len() == 40 && normalized.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(SnapshotLookup::GitOid(normalized));
    }
    Err(format!(
        "{BAD_REQUEST}snapshot id must be sha256:<64 hex> or 40-hex git oid, got {decoded:?}"
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StorageRefKind {
    Main,
    Task,
    Worker,
    Wip,
    Tag,
}

impl StorageRefKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Task => "task",
            Self::Worker => "worker",
            Self::Wip => "wip",
            Self::Tag => "tag",
        }
    }
}

fn classify_ref_kind(name: &str, db_kind: spawn_postgres::RefKind) -> StorageRefKind {
    match db_kind {
        spawn_postgres::RefKind::Main => StorageRefKind::Main,
        spawn_postgres::RefKind::Wip => StorageRefKind::Wip,
        spawn_postgres::RefKind::Tag => StorageRefKind::Tag,
        spawn_postgres::RefKind::Branch => {
            if name == "main" || name == "refs/heads/main" {
                StorageRefKind::Main
            } else if name.starts_with(spawn_host::workspace_rules::TASK_HEAD_REF_PREFIX)
                || name.starts_with("task/")
                || name.starts_with("spawn/task")
                || name.starts_with("refs/heads/spawn/task")
                || name.contains("task-")
            {
                StorageRefKind::Task
            } else if name.starts_with("wip/")
                || name.starts_with(spawn_host::workspace_rules::STALE_REF_PREFIX)
            {
                StorageRefKind::Wip
            } else {
                StorageRefKind::Worker
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefLogAction {
    Created,
    Merged,
    FastForwarded,
    RolledBack,
    Retired,
    Moved,
}

impl RefLogAction {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Merged => "merged",
            Self::FastForwarded => "fast-forwarded",
            Self::RolledBack => "rolled-back",
            Self::Retired => "retired",
            Self::Moved => "moved",
        }
    }
}

fn classify_ref_log_action(old_snapshot: Option<&str>, reason: &str) -> RefLogAction {
    if old_snapshot.is_none() {
        return RefLogAction::Created;
    }
    let lower = reason.to_ascii_lowercase();
    if lower.contains("rollback") || lower.contains("rolled-back") || lower.contains("rolled_back")
    {
        RefLogAction::RolledBack
    } else if lower.contains("retire") {
        RefLogAction::Retired
    } else if lower.contains("land") || lower.contains("merge") {
        RefLogAction::Merged
    } else if lower.contains("fast-forward")
        || lower.contains("fast_forward")
        || lower.contains("fast forward")
        || lower.contains("checkpoint")
        || lower.contains("commit")
    {
        RefLogAction::FastForwarded
    } else {
        RefLogAction::Moved
    }
}

fn is_missing_d6_relation(error: &spawn_core::port::StoreError) -> bool {
    let text = error.to_string();
    text.contains("does not exist") || text.contains("42P01")
}

fn list_all_refs_safe(
    store: &spawn_postgres::PostgresStore,
) -> Result<Vec<spawn_postgres::RefRow>, String> {
    match store.list_refs(None) {
        Ok(rows) => Ok(rows),
        Err(error) if is_missing_d6_relation(&error) => Ok(Vec::new()),
        Err(error) => Err(err(error)),
    }
}

fn list_all_ref_logs_safe(
    store: &spawn_postgres::PostgresStore,
) -> Result<Vec<spawn_postgres::RefLogRow>, String> {
    let latest = match store.latest_ref_log_seq() {
        Ok(seq) => seq,
        Err(error) if is_missing_d6_relation(&error) => return Ok(Vec::new()),
        Err(error) => return Err(err(error)),
    };
    if latest == 0 {
        return Ok(Vec::new());
    }
    let max_scan = u64::try_from(STORAGE_REF_LOG_SCAN_MAX).unwrap_or(500);
    let after_seq = i64::try_from(latest.saturating_sub(max_scan)).unwrap_or(0);
    match store.list_ref_log_after(after_seq) {
        Ok(mut rows) => {
            if rows.len() > STORAGE_REF_LOG_SCAN_MAX {
                let excess = rows.len() - STORAGE_REF_LOG_SCAN_MAX;
                rows.drain(..excess);
            }
            Ok(rows)
        }
        Err(error) if is_missing_d6_relation(&error) => Ok(Vec::new()),
        Err(error) => Err(err(error)),
    }
}

fn get_snapshot_safe(
    store: &spawn_postgres::PostgresStore,
    lookup: &SnapshotLookup,
) -> Result<Option<spawn_postgres::SnapshotRow>, String> {
    let res = match lookup {
        SnapshotLookup::Digest(id) => store.get_snapshot(id),
        SnapshotLookup::GitOid(oid) => store.get_snapshot_by_oid(oid),
    };
    match res {
        Ok(row) => Ok(row),
        Err(error) if is_missing_d6_relation(&error) => Ok(None),
        Err(error) => Err(err(error)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StorageBackend {
    D6,
    Cow,
    Overlay,
    Git,
}

impl StorageBackend {
    const fn as_str(self) -> &'static str {
        match self {
            Self::D6 => "d6",
            Self::Cow => "cow",
            Self::Overlay => "overlay",
            Self::Git => "git",
        }
    }
}

fn detect_storage_backend(has_d6_data: bool) -> (StorageBackend, String) {
    let configured = env_value("SPAWN_WORKSPACE_BACKEND")
        .or_else(|| env_value("SPAWN_STORAGE_BACKEND"))
        .map(|value| value.trim().to_ascii_lowercase());
    let root_kind = env_value("SPAWN_WORKSPACE_ROOT")
        .map(|raw| spawn_host::canonical_root(Path::new(&raw)))
        .and_then(|root| spawn_host::root_backend(&root).ok().flatten());
    if has_d6_data {
        let mode_note = match root_kind {
            Some(spawn_host::WorkspaceBackend::D6Shadow) => {
                "D6 shadow ledger active alongside authoritative Git border.".to_string()
            }
            _ => "D6 persistent CAS Merkle tree and PostgreSQL ref ledger active.".to_string(),
        };
        return (StorageBackend::D6, mode_note);
    }
    if matches!(configured.as_deref(), Some("d6" | "d6-shadow" | "cas"))
        || matches!(
            root_kind,
            Some(spawn_host::WorkspaceBackend::D6 | spawn_host::WorkspaceBackend::D6Shadow)
        )
    {
        return (
            StorageBackend::D6,
            "D6 storage mode configured, but no D6 snapshots or refs have been recorded yet."
                .to_string(),
        );
    }
    let backend = match root_kind {
        Some(spawn_host::WorkspaceBackend::Cow) => StorageBackend::Cow,
        Some(spawn_host::WorkspaceBackend::Overlay) => StorageBackend::Overlay,
        _ => StorageBackend::Git,
    };
    let backend_name = backend.as_str();
    (
        backend,
        format!(
            "Running in {backend_name} workspace mode with no D6 CAS snapshots recorded. Run `spawn storage cutover --to d6` to enable D6 snapshot tracking."
        ),
    )
}

fn shell_token(text: &str) -> String {
    if !text.is_empty()
        && text.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/' | b':')
        })
    {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', "'\"'\"'"))
    }
}

fn short_snapshot_id(id: &str) -> String {
    let hex = id.strip_prefix("sha256:").unwrap_or(id);
    hex.chars().take(12).collect()
}

fn ref_json(row: &spawn_postgres::RefRow) -> Value {
    json!({
        "name": row.name,
        "kind": classify_ref_kind(&row.name, row.kind).as_str(),
        "snapshot_id": row.snapshot,
        "updated_at_ms": row.updated_at_ms,
    })
}

fn ref_badge_json(row: &spawn_postgres::RefRow) -> Value {
    json!({
        "name": row.name,
        "kind": classify_ref_kind(&row.name, row.kind).as_str(),
        "updated_at_ms": row.updated_at_ms,
    })
}

fn ref_log_json(row: &spawn_postgres::RefLogRow) -> Value {
    json!({
        "seq": row.seq,
        "project": row.project,
        "name": row.name,
        "kind": classify_ref_kind(&row.name, row.kind).as_str(),
        "old_snapshot": row.old_snapshot,
        "new_snapshot": row.new_snapshot,
        "reason": row.reason,
        "action": classify_ref_log_action(row.old_snapshot.as_deref(), &row.reason).as_str(),
        "at_ms": row.at_ms,
    })
}

fn enqueue_snapshot_id(
    queue: &mut std::collections::VecDeque<String>,
    queued: &mut std::collections::HashSet<String>,
    id: &str,
) {
    if queued.insert(id.to_string()) {
        queue.push_back(id.to_string());
    }
}

fn collect_project_snapshots(
    store: &spawn_postgres::PostgresStore,
    project: &str,
    project_refs: &[&spawn_postgres::RefRow],
    project_logs: &[&spawn_postgres::RefLogRow],
    limit: usize,
) -> Result<(Vec<spawn_postgres::SnapshotRow>, bool), String> {
    let mut queue = std::collections::VecDeque::new();
    let mut queued = std::collections::HashSet::new();
    for r in project_refs {
        if classify_ref_kind(&r.name, r.kind) == StorageRefKind::Main {
            enqueue_snapshot_id(&mut queue, &mut queued, &r.snapshot);
        }
    }
    for r in project_refs {
        enqueue_snapshot_id(&mut queue, &mut queued, &r.snapshot);
    }
    for entry in project_logs {
        enqueue_snapshot_id(&mut queue, &mut queued, &entry.new_snapshot);
        if let Some(old) = &entry.old_snapshot {
            enqueue_snapshot_id(&mut queue, &mut queued, old);
        }
    }

    let mut nodes = Vec::new();
    let mut seen_nodes = std::collections::HashSet::new();
    let mut truncated = false;
    while let Some(id) = queue.pop_front() {
        if nodes.len() >= limit {
            truncated = true;
            break;
        }
        let lookup = SnapshotLookup::Digest(id);
        let Some(row) = get_snapshot_safe(store, &lookup)? else {
            continue;
        };
        if row.project != project || !seen_nodes.insert(row.id.clone()) {
            continue;
        }
        for parent in &row.parents {
            enqueue_snapshot_id(&mut queue, &mut queued, parent);
        }
        nodes.push(row);
    }
    nodes.sort_by(|a, b| {
        b.created_at_ms
            .cmp(&a.created_at_ms)
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok((nodes, truncated))
}

fn is_storage_read_route(path: &str) -> bool {
    path == "/api/storage/tree"
        || path
            .strip_prefix("/api/storage/snapshot/")
            .is_some_and(|raw_id| !raw_id.is_empty() && !raw_id.contains('/'))
}

fn respond_storage_read(
    request: Request,
    path: &str,
    query: &str,
    shared: &Shared,
) -> Result<(), String> {
    if !matches!(request.method(), Method::Get | Method::Head) {
        return request
            .respond(not_allowed("GET"))
            .map_err(|error| error.to_string());
    }
    let answer = with_shared_reader(
        shared,
        || {
            if let Some(raw_id) = path.strip_prefix("/api/storage/snapshot/") {
                storage_snapshot_answer(raw_id)
            } else {
                storage_tree_answer(query)
            }
        },
        |a| a.status == 500,
        busy_answer,
    );
    let if_none_match = header(&request, "If-None-Match").map(str::to_string);
    answers::respond(request, &answer, if_none_match.as_deref())
}

fn empty_storage_tree_json(projects: &[String], backend: StorageBackend, mode_note: &str) -> Value {
    json!({
        "projects": projects,
        "active_project": Value::Null,
        "summary": {
            "total_snapshots": 0,
            "live_snapshots": 0,
            "collected_snapshots": 0,
            "active_refs": 0,
            "history_entries": 0,
            "truncated": false,
            "backend": backend.as_str(),
            "mode_note": mode_note,
        },
        "nodes": [],
        "refs": [],
        "history": [],
    })
}

fn storage_tree_for_store(
    store: &spawn_postgres::PostgresStore,
    requested_project: Option<&str>,
    limit: usize,
) -> Result<Value, String> {
    let all_refs = list_all_refs_safe(store)?;
    let all_logs = list_all_ref_logs_safe(store)?;
    let has_d6_data = !all_refs.is_empty() || !all_logs.is_empty();
    let (backend, mode_note) = detect_storage_backend(has_d6_data);

    let mut projects_set = BTreeSet::new();
    for r in &all_refs {
        projects_set.insert(r.project.clone());
    }
    for l in &all_logs {
        projects_set.insert(l.project.clone());
    }
    let projects: Vec<String> = projects_set.into_iter().collect();

    let active_project = requested_project.map(str::to_string).or_else(|| {
        all_refs
            .iter()
            .map(|r| r.project.clone())
            .min()
            .or_else(|| projects.first().cloned())
    });

    let Some(project) = active_project else {
        return Ok(empty_storage_tree_json(&projects, backend, &mode_note));
    };

    let mut project_refs: Vec<&spawn_postgres::RefRow> =
        all_refs.iter().filter(|r| r.project == project).collect();
    project_refs.sort_by(|a, b| {
        let a_main = classify_ref_kind(&a.name, a.kind) == StorageRefKind::Main;
        let b_main = classify_ref_kind(&b.name, b.kind) == StorageRefKind::Main;
        b_main.cmp(&a_main).then_with(|| a.name.cmp(&b.name))
    });

    let mut project_logs: Vec<&spawn_postgres::RefLogRow> =
        all_logs.iter().filter(|l| l.project == project).collect();
    project_logs.sort_by_key(|entry| std::cmp::Reverse(entry.seq));

    let (nodes, truncated) =
        collect_project_snapshots(store, &project, &project_refs, &project_logs, limit)?;

    let mut refs_by_snap: HashMap<&str, Vec<Value>> = HashMap::new();
    for r in &project_refs {
        refs_by_snap
            .entry(r.snapshot.as_str())
            .or_default()
            .push(ref_badge_json(r));
    }

    let live_snapshots = nodes
        .iter()
        .filter(|row| row.state == spawn_postgres::SnapshotState::Live)
        .count();
    let collected_snapshots = nodes.len().saturating_sub(live_snapshots);

    let node_values: Vec<Value> = nodes
        .iter()
        .map(|row| {
            let attached = refs_by_snap.remove(row.id.as_str()).unwrap_or_default();
            json!({
                "snapshot_id": row.id,
                "parent_ids": row.parents,
                "tree_oid": row.tree,
                "created_at_ms": row.created_at_ms,
                "git_oid": row.git_oid,
                "state": row.state.as_str(),
                "refs": attached,
            })
        })
        .collect();
    let ref_values: Vec<Value> = project_refs.iter().map(|r| ref_json(r)).collect();
    let history_values: Vec<Value> = project_logs
        .iter()
        .take(STORAGE_HISTORY_MAX)
        .map(|l| ref_log_json(l))
        .collect();

    Ok(json!({
        "projects": projects,
        "active_project": project,
        "summary": {
            "total_snapshots": node_values.len(),
            "live_snapshots": live_snapshots,
            "collected_snapshots": collected_snapshots,
            "active_refs": ref_values.len(),
            "history_entries": project_logs.len(),
            "truncated": truncated,
            "backend": backend.as_str(),
            "mode_note": mode_note,
        },
        "nodes": node_values,
        "refs": ref_values,
        "history": history_values,
    }))
}

fn recovery_commands_for_snapshot(snap: &spawn_postgres::SnapshotRow) -> Vec<Value> {
    let short_id = short_snapshot_id(&snap.id);
    let proj_arg = shell_token(&snap.project);
    let git_arg = shell_token(&snap.git_oid);
    let branch_arg = shell_token(&format!("recover/{short_id}"));
    let rollback_reason = shell_token(&format!("time-machine rollback to {short_id}"));
    vec![
        json!({
            "id": "verify_status",
            "label": "Inspect D6 cutover status",
            "description": "Read-only inspection of current D6/Git cutover phase, CAS directory, and ref state.",
            "command": format!("spawn storage cutover --status --project {proj_arg} --json"),
            "risk": "read_only",
        }),
        json!({
            "id": "verify_parity",
            "label": "Verify CAS parity",
            "description": "Read-only parity check between D6 Merkle snapshots and the projected Git border.",
            "command": format!("spawn storage cutover --verify --project {proj_arg} --json"),
            "risk": "read_only",
        }),
        json!({
            "id": "inspect_commit",
            "label": "Inspect projected commit",
            "description": "Show commit summary and touched paths for this snapshot's projected Git OID.",
            "command": format!("git show --stat {git_arg}"),
            "risk": "read_only",
        }),
        json!({
            "id": "create_branch",
            "label": "Create recovery branch",
            "description": "Non-destructive branch at this point-in-time snapshot to test or cherry-pick forward.",
            "command": format!("git checkout -b {branch_arg} {git_arg}"),
            "risk": "branch",
        }),
        json!({
            "id": "rollback_git",
            "label": "Rollback project to writable Git",
            "description": "Retires live D6 refs while preserving append-only snapshot/ref audit history.",
            "command": format!("spawn storage cutover --rollback git --project {proj_arg} --reason {rollback_reason}"),
            "risk": "rollback",
        }),
    ]
}

fn snapshot_ancestors(
    store: &spawn_postgres::PostgresStore,
    root: &spawn_postgres::SnapshotRow,
) -> Result<Vec<Value>, String> {
    let mut ancestors = Vec::new();
    let mut visited = std::collections::HashSet::from([root.id.clone()]);
    let mut queue = std::collections::VecDeque::new();
    for parent in &root.parents {
        if visited.insert(parent.clone()) {
            queue.push_back((parent.clone(), 1usize));
        }
    }
    while let Some((id, depth)) = queue.pop_front() {
        let lookup = SnapshotLookup::Digest(id);
        let Some(row) = get_snapshot_safe(store, &lookup)? else {
            continue;
        };
        ancestors.push(json!({
            "snapshot_id": row.id,
            "git_oid": row.git_oid,
            "tree_oid": row.tree,
            "parent_ids": row.parents,
            "state": row.state.as_str(),
            "created_at_ms": row.created_at_ms,
            "depth": depth,
        }));
        if depth < SNAPSHOT_ANCESTOR_MAX_DEPTH && ancestors.len() < SNAPSHOT_CHILDREN_SCAN_MAX {
            for next_parent in &row.parents {
                if visited.insert(next_parent.clone()) {
                    queue.push_back((next_parent.clone(), depth + 1));
                }
            }
        }
    }
    Ok(ancestors)
}

fn storage_snapshot_for_store(
    store: &spawn_postgres::PostgresStore,
    lookup: &SnapshotLookup,
) -> Result<Option<Value>, String> {
    let Some(snap) = get_snapshot_safe(store, lookup)? else {
        return Ok(None);
    };

    let project_refs = match store.list_refs(Some(&snap.project)) {
        Ok(rows) => rows,
        Err(error) if is_missing_d6_relation(&error) => Vec::new(),
        Err(error) => return Err(err(error)),
    };
    let all_logs = list_all_ref_logs_safe(store)?;
    let mut project_logs: Vec<&spawn_postgres::RefLogRow> = all_logs
        .iter()
        .filter(|l| l.project == snap.project)
        .collect();
    project_logs.sort_by_key(|entry| std::cmp::Reverse(entry.seq));

    let ref_refs: Vec<&spawn_postgres::RefRow> = project_refs.iter().collect();
    let (project_nodes, _) = collect_project_snapshots(
        store,
        &snap.project,
        &ref_refs,
        &project_logs,
        SNAPSHOT_CHILDREN_SCAN_MAX,
    )?;

    let attached_refs: Vec<Value> = project_refs
        .iter()
        .filter(|r| r.snapshot == snap.id)
        .map(ref_badge_json)
        .collect();
    let children: Vec<Value> = project_nodes
        .iter()
        .filter(|candidate| candidate.parents.iter().any(|p| p == &snap.id))
        .map(|row| {
            json!({
                "snapshot_id": row.id,
                "git_oid": row.git_oid,
                "tree_oid": row.tree,
                "parent_ids": row.parents,
                "state": row.state.as_str(),
                "created_at_ms": row.created_at_ms,
            })
        })
        .collect();
    let scoped_history: Vec<Value> = project_logs
        .into_iter()
        .filter(|l| {
            l.new_snapshot == snap.id || l.old_snapshot.as_deref() == Some(snap.id.as_str())
        })
        .take(50)
        .map(ref_log_json)
        .collect();
    let ancestors = snapshot_ancestors(store, &snap)?;
    let recovery_commands = recovery_commands_for_snapshot(&snap);

    Ok(Some(json!({
        "snapshot": {
            "snapshot_id": snap.id,
            "project": snap.project,
            "parent_ids": snap.parents,
            "tree_oid": snap.tree,
            "git_oid": snap.git_oid,
            "state": snap.state.as_str(),
            "created_at_ms": snap.created_at_ms,
        },
        "refs": attached_refs,
        "ancestors": ancestors,
        "children": children,
        "history": scoped_history,
        "recovery_commands": recovery_commands,
    })))
}

enum OptionalStore {
    Unconfigured,
    Connected(PooledStore),
}

fn open_storage_store() -> Result<OptionalStore, String> {
    let state = READER_CONTEXT.with(|cell| match cell.borrow().as_ref() {
        Some(ReaderContext::Pooled { .. }) => (false, true),
        #[cfg(test)]
        Some(ReaderContext::UrlOverride(url)) => (url.trim().is_empty(), true),
        None => (false, false),
    });
    if state.0 {
        return Ok(OptionalStore::Unconfigured);
    }
    if !state.1
        && env_value("SPAWN_SERVE_DATABASE_URL").is_none()
        && env_value("SPAWN_DATABASE_URL").is_none()
    {
        return Ok(OptionalStore::Unconfigured);
    }
    open_store().map(OptionalStore::Connected)
}

/// Read-only `GET|HEAD /api/storage/tree[?project=<name>&limit=<N>]` answer
/// with weak `ETag` validation (graceful empty response in `git` mode, empty ledger,
/// or when `SPAWN_DATABASE_URL` is unset).
pub(in crate::serve) fn storage_tree_answer(query: &str) -> answers::Answer {
    let read = (|| -> Result<Value, String> {
        let (project, limit) = parse_storage_tree_query(query)?;
        match open_storage_store()? {
            OptionalStore::Unconfigured => {
                let (backend, _) = detect_storage_backend(false);
                Ok(empty_storage_tree_json(
                    &[],
                    backend,
                    "SPAWN_DATABASE_URL is not configured; D6 snapshot ledger is unavailable.",
                ))
            }
            OptionalStore::Connected(store) => {
                storage_tree_for_store(&store, project.as_deref(), limit)
            }
        }
    })();
    answers::answer("/api/storage/tree", read, BAD_REQUEST)
}

/// Read-only `GET|HEAD /api/storage/snapshot/<id>` answer (`400` on malformed id,
/// `404` when unknown or DB unconfigured, weak `ETag` / `304` on hit).
pub(in crate::serve) fn storage_snapshot_answer(raw_id: &str) -> answers::Answer {
    let read = (|| -> Result<(SnapshotLookup, Option<Value>), String> {
        let lookup = parse_snapshot_lookup(raw_id)?;
        let found = match open_storage_store()? {
            OptionalStore::Unconfigured => None,
            OptionalStore::Connected(store) => storage_snapshot_for_store(&store, &lookup)?,
        };
        Ok((lookup, found))
    })();
    match read {
        Ok((_, Some(value))) => answers::answer("/api/storage/snapshot/*", Ok(value), BAD_REQUEST),
        Ok((lookup, None)) => answers::Answer {
            status: 404,
            body: json!({
                "error": format!("snapshot not found: {}", lookup.as_str()),
                "code": "snapshot_not_found",
            })
            .to_string(),
            etag: None,
        },
        Err(error) => answers::answer("/api/storage/snapshot/*", Err(error), BAD_REQUEST),
    }
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

/// Liveness quiet/stalled thresholds (milliseconds), aligned with
/// `spawn-android` `FleetLivenessModel.kt` (`#63`).
const LIVENESS_AGENT_QUIET_MS: i64 = 5 * 60_000;
const LIVENESS_AGENT_STALLED_MS: i64 = 15 * 60_000;
const LIVENESS_TASK_QUIET_MS: i64 = 10 * 60_000;
const LIVENESS_TASK_STALLED_MS: i64 = 20 * 60_000;

/// Enrich each agent row on `/api/fleet` with real execution liveness truth:
/// `last_event_at_ms` (execution/turn/state timestamp, never `judged_at_ms`),
/// `silence_ms`, and `liveness_state` (`active_heartbeat` / `quiet_working` /
/// `loop_stalled` / `waiting_idle` / `blocked` / `offline`).
fn with_fleet_liveness(agents: &mut Value, now_ms: i64) {
    let Some(rows) = agents.as_array_mut() else {
        return;
    };
    for agent in rows {
        let live = agent.get("live").and_then(Value::as_bool).unwrap_or(false);
        let state = agent
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("UNKNOWN")
            .to_ascii_uppercase();
        let last_event_at_ms = agent
            .get("activity")
            .and_then(|activity| {
                activity
                    .get("last_event_at_ms")
                    .and_then(Value::as_i64)
                    .or_else(|| activity.get("turn_at_ms").and_then(Value::as_i64))
            })
            .or_else(|| agent.get("state_since_ms").and_then(Value::as_i64))
            .or_else(|| agent.get("started_at_ms").and_then(Value::as_i64));
        let silence_ms = if live {
            last_event_at_ms.map(|ts| now_ms.saturating_sub(ts).max(0))
        } else {
            None
        };
        let waiting_on_own_job = live
            && agent
                .get("judgement")
                .and_then(|judgement| judgement.get("reason"))
                .and_then(Value::as_str)
                == Some("WAITING_FOR_BACKGROUND_JOB")
            && agent
                .get("activity")
                .and_then(|activity| activity.get("jobs"))
                .and_then(Value::as_array)
                .is_some_and(|jobs| !jobs.is_empty());
        let liveness_state = if !live {
            "offline"
        } else if state == "BLOCKED" {
            "blocked"
        } else if matches!(state.as_str(), "WAITING" | "COMPLETED") {
            "waiting_idle"
        } else if waiting_on_own_job {
            if silence_ms.is_some_and(|silence| silence < LIVENESS_AGENT_QUIET_MS) {
                "active_heartbeat"
            } else {
                "quiet_working"
            }
        } else if silence_ms.is_some_and(|silence| silence >= LIVENESS_AGENT_STALLED_MS) {
            "loop_stalled"
        } else if silence_ms.is_some_and(|silence| silence >= LIVENESS_AGENT_QUIET_MS) {
            "quiet_working"
        } else {
            "active_heartbeat"
        };
        if let Some(fields) = agent.as_object_mut() {
            fields.insert("last_event_at_ms".to_string(), json!(last_event_at_ms));
            fields.insert("silence_ms".to_string(), json!(silence_ms));
            fields.insert("liveness_state".to_string(), json!(liveness_state));
        }
    }
}

/// Enrich `/api/missions` with top-level `generated_at_ms` and each task with
/// `last_event_at_ms`, `silence_ms`, and `liveness_state` (`active_working` /
/// `briefed_quiet` / `stalled` / `waiting_review` / `terminal`).
fn with_task_liveness(board: &mut Value, now_ms: i64) {
    if let Some(root) = board.as_object_mut() {
        root.insert("generated_at_ms".to_string(), json!(now_ms));
    }
    let Some(missions) = board.get_mut("missions").and_then(Value::as_array_mut) else {
        return;
    };
    for mission in missions {
        let gated = mission.get("gate").is_some_and(|gate| !gate.is_null());
        let Some(tasks) = mission.get_mut("tasks").and_then(Value::as_array_mut) else {
            continue;
        };
        for task in tasks {
            let latest_stage_at_ms =
                task.get("stages")
                    .and_then(Value::as_array)
                    .and_then(|stages| {
                        stages
                            .iter()
                            .filter_map(|stage| stage.get("at_ms").and_then(Value::as_i64))
                            .max()
                    });
            let fallback_ts = latest_stage_at_ms
                .or_else(|| task.get("updated_at_ms").and_then(Value::as_i64))
                .or_else(|| task.get("created_at_ms").and_then(Value::as_i64));
            let state = task
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let (last_event_at_ms, silence_ms, liveness_state) = match state {
                "delivered" => {
                    let engine_at_ms = task
                        .get("activity")
                        .and_then(|activity| activity.get("at_ms"))
                        .and_then(Value::as_i64);
                    let delivered_since_ms = task
                        .get("activity")
                        .and_then(|activity| activity.get("since_ms"))
                        .and_then(Value::as_i64);
                    let last = engine_at_ms.or(delivered_since_ms).or(fallback_ts);
                    let silence = last.map(|ts| now_ms.saturating_sub(ts).max(0));
                    let liveness = if silence.is_some_and(|ms| ms >= LIVENESS_TASK_STALLED_MS) {
                        "stalled"
                    } else if engine_at_ms.is_none()
                        || silence.is_some_and(|ms| ms >= LIVENESS_TASK_QUIET_MS)
                    {
                        "briefed_quiet"
                    } else {
                        "active_working"
                    };
                    (last, silence, liveness)
                }
                "reported" | "revise" | "landing" => (fallback_ts, None, "waiting_review"),
                "accepted" => (
                    fallback_ts,
                    None,
                    if gated { "waiting_review" } else { "terminal" },
                ),
                "landed" | "cancelled" => (fallback_ts, None, "terminal"),
                // "assigned" and any unrecognized states read as briefed/quiet until delivered.
                _ => (fallback_ts, None, "briefed_quiet"),
            };
            if let Some(fields) = task.as_object_mut() {
                fields.insert("last_event_at_ms".to_string(), json!(last_event_at_ms));
                fields.insert("silence_ms".to_string(), json!(silence_ms));
                fields.insert("liveness_state".to_string(), json!(liveness_state));
            }
        }
    }
}

/// Put on each task what its worker did since the task reached it
/// (objective 211), from `activity` (keyed by task id, the delivered tasks
/// alone): `{since_ms, at_ms, event, tool_calls}` for a delivered task,
/// null for every other.
fn with_activity(board: &mut Value, activity: &Value) {
    let Some(missions) = board.get_mut("missions").and_then(Value::as_array_mut) else {
        return;
    };
    for task in missions
        .iter_mut()
        .filter_map(|mission| mission.get_mut("tasks").and_then(Value::as_array_mut))
        .flatten()
    {
        let delivered = task["state"] == "delivered";
        let found = task["id"]
            .as_i64()
            .and_then(|id| activity.get(id.to_string()))
            .filter(|_| delivered)
            .cloned()
            .unwrap_or(Value::Null);
        if let Some(fields) = task.as_object_mut() {
            fields.insert("activity".to_string(), found);
        }
    }
}

/// The RSI page (docs/fleet-architecture.md §10.4): every open objective
/// mission, read from the mission tables, the control stream and the
/// mission's backlog.
///
/// `GET /api/objectives` answers:
///
/// ```text
/// {
///   "generated_at_ms": 1793766328633,
///   "halt_after": 3,                      // rejections (or aborts) in a row that halt
///   "missions": [{                        // open objective missions, by id
///     "id": 4, "text": "...", "mission_name": "Spawn RSI",
///     "state": "open", "sources": ["..."],
///     "gate": "/.../rsi/scenarios", "accept": "gate",
///     "budget": {"minutes": 45, "tool_calls": 60} | null,
///     "baseline": "<commit>" | null,      // where the last landing left main
///     "halted": "<reason>" | null,        // set until a human resumes
///     "scenario_floor": 7, "gate_rejections": 0, "gate_aborts": 0,
///     "backlog": "/.../rsi/backlog" | null,
///     "backlog_error": "<why it could not be read>" | null,
///     "resident": {"name", "agent_id", "session_id", "state", "label",
///                  "task_title", "mission_name"} | null,
///     "next_objective": 3 | null,         // what the integrator assigns next:
///                                         // objectives::choose over the landings
///                                         // (and objective.solved records), the
///                                         // tasks in flight, attempts and holds
///     "objectives": [{                    // the backlog, by id
///       "id": 2, "title": "...", "prompt": "...", "recurring": true,
///       "metric": "ingest_total_ns" | null, "unit": "ns",
///       "direction": "lower" | "higher", "target": 1100 | null,
///       "owner_judged": false,            // the owner judges it, not the fleet
///       "status": "open" | "solved" | "met" | "owner",   // "solved" by a landing
///                                         // or by an objective.solved record
///       "solved_on_base": {"event": 109278, "task": 201, "base": "<commit>",
///                          "at_ms": 1793724858211} | null,
///                                         // the integrator's objective.solved record
///                                         // (at_ms: when it was recorded)
///       "current": 1021 | null,           // latest landed metric value
///       "tasks": [10, 14],                // task ids for this objective
///       "landings": [{                    // landed tasks, oldest first
///         "task": 10, "at_ms": 1793724858211,
///         "metric": "METRIC ingest_total_ns baseline=1573 candidate=1021" | null,
///         "baseline": 1573 | null, "candidate": 1021 | null
///       }]
///     }],
///     "tasks": [{                         // newest first
///       "id", "label", "task_title", "mission_name",   // labels.rs
///       "kind", "agent_id", "session_id", "state", "revisions",
///       "objective", "candidate", "base", "metric", "scenario", "brief",
///       "review_note", "created_by", "created_at_ms", "delivered_at_ms",
///       "updated_at_ms",
///       "gates": [{                       // task.gated records, oldest first
///         "verdict": "accepted" | "rejected" | "aborted" | "conflict" | "incompatible"
///                    | "stale" | "no_room" | "host_fault",
///         "reason": "..." | null, "metric": "METRIC ..." | null,
///         "candidate": "<commit>" | null, "at_ms": 1793724858000
///       }]
///     }]
///   }]
/// }
/// ```
///
/// An objective's `prompt` is its first lines (about 1 KB), and a task's
/// `brief` and `review_note` are null, their lengths in bytes beside them
/// ([`answers::slim_rsi`]: `prompt_bytes`, `brief_bytes`,
/// `review_note_bytes`).
///
/// `GET /api/tasks/<n>/gate` answers one task: the fields above plus the full
/// `brief`, `report`, `gate_log` (the latest), `mission_id`, and `log` on
/// every gate verdict.
///
/// `GET /api/objectives/<m>/<n>` answers one objective's whole
/// prompt, `{mission, id, title, prompt, prompt_bytes}`, as the page asks
/// when the objective is pointed at.
///
/// `GET /api/objectives/pane[?mission=<id>]` answers the live pane of the mission's
/// resident (the first open objective mission by default), in the shape of
/// `/api/sessions/<id>/pane` plus `session_id`.
fn rsi_overview() -> Result<Value, String> {
    let store = open_store()?;
    let mut missions = objective_missions(&store)?;
    labels::label_rsi(&mut missions);
    answers::slim_rsi(&mut missions);
    Ok(json!({
        "generated_at_ms": now_ms(),
        "halt_after": HALT_AFTER,
        "missions": missions,
    }))
}

/// Open objective missions with their backlog's objectives filled in, each
/// with its tasks' outcomes (`PostgresStore::objective_outcomes`, the
/// integrator's own read) for [`with_objectives`]'s `next_objective`.
pub(crate) fn objective_missions(
    store: &spawn_postgres::PostgresStore,
) -> Result<Vec<Value>, String> {
    let missions = store.objective_missions().map_err(err)?;
    let Value::Array(missions) = missions else {
        return Ok(Vec::new());
    };
    missions
        .into_iter()
        .map(|mut mission| {
            if let Some(id) = mission["id"].as_i64() {
                let outcomes = store.objective_outcomes(id).map_err(err)?;
                if let Some(fields) = mission.as_object_mut() {
                    fields.insert("outcomes".to_string(), outcomes);
                }
            }
            Ok(with_objectives(mission))
        })
        .collect()
}

/// Whether a task of an objective mission is in flight: from its
/// assignment until it lands or is cancelled. Accepted work of a mission
/// with a gate still waits for the gate and then lands; accepted without
/// one is closed.
fn task_in_flight(task: &Value, gated: bool) -> bool {
    match task["state"].as_str() {
        Some("landed" | "cancelled") | None => false,
        Some("accepted") => gated,
        Some(_) => true,
    }
}

/// One mission row with `backlog`, `backlog_error`, `features_file`,
/// `objectives` and `next_objective` added. A retired objective's row (the
/// row's `retired`, objective 210) has status `retired` and its `retired`
/// row, and `next_objective` never names it. The row's `solved_on_base`
/// (the mission's `objective.solved` records) and `outcomes` (its tasks'
/// outcomes, when read) are taken out: each objective row names its own
/// record, and `next_objective` is what [`crate::objectives::choose`] gives
/// one free slot from the integrator's own inputs (the landings, landed
/// tasks and `objective.solved` records as `OBJECTIVE_LANDINGS`; the
/// objectives with a task in flight; the attempts; the held objectives),
/// null when it would assign nothing. `features_file` is the
/// `objectives.features` path as configured (the feature map's
/// identity-only mapping, [`feature_map`]): null when the mission names
/// none.
pub(crate) fn with_objectives(mut mission: Value) -> Value {
    let backlog = mission
        .pointer("/objectives/backlog")
        .and_then(Value::as_str)
        .map(str::to_string);
    let features_file = mission
        .pointer("/objectives/features")
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
    let (solved, outcomes) = match mission.as_object_mut() {
        Some(fields) => (fields.remove("solved_on_base"), fields.remove("outcomes")),
        None => (None, None),
    };
    let solved: Vec<Value> = solved
        .and_then(|solved| solved.as_array().cloned())
        .unwrap_or_default();
    let outcomes: Vec<Outcome> = outcomes
        .and_then(|outcomes| serde_json::from_value(outcomes).ok())
        .unwrap_or_default();
    let retired: Vec<Value> = mission
        .get("retired")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let record = |id: u32| {
        solved
            .iter()
            .find(|record| record["objective"].as_u64() == Some(u64::from(id)))
    };
    let rows: Vec<Value> = objectives
        .iter()
        .map(|objective| {
            retired_row(
                objective_json(objective, &tasks, record(objective.id)),
                retired_of(&retired, objective.id),
            )
        })
        .collect();
    let closed = Closed {
        solved: &solved,
        retired: &retired,
    };
    let next = next_objective(&mission, &objectives, &tasks, &closed, &outcomes);
    if let Some(fields) = mission.as_object_mut() {
        fields.insert("backlog".to_string(), json!(backlog));
        fields.insert("backlog_error".to_string(), json!(error));
        fields.insert("features_file".to_string(), json!(features_file));
        fields.insert("next_objective".to_string(), json!(next));
        fields.insert("objectives".to_string(), Value::Array(rows));
    }
    mission
}

/// The mission's `retired` row of objective `id` (objective 210), if it was
/// retired by hand.
fn retired_of(retired: &[Value], id: u32) -> Option<&Value> {
    retired
        .iter()
        .find(|row| row["objective"].as_u64() == Some(u64::from(id)))
}

/// An objective row with its `retired` row (`{objective, by, reason,
/// caller, at_ms}`, null when it was not retired): a retired objective's
/// status is `retired`, whatever its kind (objective 210).
fn retired_row(mut row: Value, retired: Option<&Value>) -> Value {
    if let Some(fields) = row.as_object_mut() {
        if retired.is_some() {
            fields.insert("status".to_string(), json!("retired"));
        }
        fields.insert("retired".to_string(), json!(retired));
    }
    row
}

/// The records that close an objective besides a landing: the mission's
/// `objective.solved` records and its objectives retired by hand.
struct Closed<'a> {
    solved: &'a [Value],
    retired: &'a [Value],
}

/// What the integrator assigns next in `mission`: the objective
/// [`crate::objectives::choose`] gives one free slot, from the same inputs
/// as `crate::objectives::assign_next` (a retired objective is never it).
fn next_objective(
    mission: &Value,
    objectives: &[Task],
    tasks: &[Value],
    closed: &Closed<'_>,
    outcomes: &[Outcome],
) -> Option<u32> {
    // Newest first, as `OBJECTIVE_LANDINGS` gives them (`landed_value`
    // reads the first metric); a record solved on the base has none.
    let mut landed: Vec<&Value> = tasks
        .iter()
        .filter(|task| task["state"] == "landed")
        .collect();
    landed.sort_by_key(|task| {
        std::cmp::Reverse((task["updated_at_ms"].as_i64(), task["id"].as_i64()))
    });
    let landings: Vec<Landing> = landed
        .iter()
        .filter_map(|task| {
            Some(Landing {
                objective: u32::try_from(task["objective"].as_u64()?).ok()?,
                metric: task["metric"].as_str().map(str::to_string),
                retired: false,
            })
        })
        .chain(closed.solved.iter().filter_map(|record| {
            Some(Landing {
                objective: u32::try_from(record["objective"].as_u64()?).ok()?,
                metric: None,
                retired: false,
            })
        }))
        .chain(closed.retired.iter().filter_map(|row| {
            Some(Landing {
                objective: u32::try_from(row["objective"].as_u64()?).ok()?,
                metric: None,
                retired: true,
            })
        }))
        .collect();
    let gated = !mission["gate"].is_null();
    let in_flight: BTreeSet<u32> = tasks
        .iter()
        .filter(|task| task_in_flight(task, gated))
        .filter_map(|task| u32::try_from(task["objective"].as_u64()?).ok())
        .collect();
    let configured_ms = mission["configured_ms"].as_i64();
    let held: BTreeSet<u32> = overruns(outcomes)
        .iter()
        .filter(|(_, overrun)| holds(overrun, configured_ms))
        .map(|(objective, _)| *objective)
        .collect();
    choose(
        objectives,
        &landings,
        &in_flight,
        &attempts(outcomes),
        &held,
        1,
    )
    .first()
    .map(|task| task.id)
}

/// One backlog objective with its status and landings, from the mission's
/// tasks (newest first), and `solved`, the mission's `objective.solved`
/// record of it if one: a one-shot objective solved on the base is
/// solved, as a landing makes it, and its row names the record
/// (`solved_on_base`: `{event, task, base, at_ms}`, null for every other
/// objective).
fn objective_json(objective: &Task, tasks: &[Value], solved: Option<&Value>) -> Value {
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
            retired: false,
        })
        .collect();
    let current = landed_value(objective, &landings);
    let status = if objective.owner_judged {
        // The owner's, not the fleet's: it is never assigned, and
        // `next_objective` below (which reads status "open") never names it.
        "owner"
    } else if objective.recurring {
        if current.is_some_and(|value| objective.meets(value)) {
            "met"
        } else {
            "open"
        }
    } else if landed.is_empty() && solved.is_none() {
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
        "owner_judged": objective.owner_judged,
        "status": status,
        "current": current,
        "tasks": mine.iter().map(|task| task["id"].clone()).collect::<Vec<_>>(),
        "landings": history,
        "solved_on_base": solved.map(|record| json!({
            "event": record["event"],
            "task": record["task"],
            "base": record["base"],
            "at_ms": record["at_ms"],
        })),
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
            .find(|task| task_in_flight(task, gated))
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
            // The owner's own objectives, which the fleet never assigns
            // (backlog `owner_judged`).
            "owner_judged": count("owner"),
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

/// `/api/objectives/<m>/<n>`: one objective's whole prompt, read
/// from the backlog of that open objective mission (`/api/objectives` sends the
/// prompt's first lines).
fn rsi_objective(rest: &str) -> Result<Value, String> {
    let bad = || format!("{BAD_REQUEST}bad objective {rest}");
    let (mission, objective) = rest.split_once('/').ok_or_else(bad)?;
    let mission: i64 = mission.parse().map_err(|_| bad())?;
    let objective: u32 = objective.parse().map_err(|_| bad())?;
    let store = open_store()?;
    let missions = store.objective_missions().map_err(err)?;
    objective_prompt(&missions, mission, objective)
}

/// Objective `objective` of mission `mission` among the open objective
/// `missions`, with its whole prompt, from the mission's backlog.
fn objective_prompt(missions: &Value, mission: i64, objective: u32) -> Result<Value, String> {
    let backlog = missions
        .as_array()
        .into_iter()
        .flatten()
        .find(|row| row["id"].as_i64() == Some(mission))
        .ok_or_else(|| format!("mission {mission} is not an open objective mission"))?
        .pointer("/objectives/backlog")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("mission {mission} names no backlog"))?;
    let found = load_backlog(Path::new(backlog))?
        .into_iter()
        .find(|task| task.id == objective)
        .ok_or_else(|| format!("objective {objective} is not in mission {mission}'s backlog"))?;
    Ok(json!({
        "mission": mission,
        "id": found.id,
        "title": found.title,
        "prompt_bytes": found.prompt.len(),
        "prompt": found.prompt,
    }))
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

/// The fleet board: every session with its judgement, activity and label
/// (`label`, `task_title`, `mission_name`: [`labels`]), plus
/// fleet health, the decision ladder and each gated mission's gate-room
/// waits. Computed by SQL from storage, except what the host has now, read
/// at this request: the health's `disk` (free space, [`crate::disk::report`]),
/// `memory` (`/proc/meminfo`, [`crate::memory::report`]) and `storage`
/// (what uses the disk, [`storage::report`]: its sizes kept for ten minutes).
/// Its `backups` are the newest database copy and bundle (objective 82,
/// [`backups`]).
fn fleet() -> Result<Value, String> {
    let store = open_store()?;
    let mut health = store.fleet_health().map_err(err)?;
    let until = health["generated_at_ms"].as_i64().unwrap_or_else(now_ms);
    if health["generated_at_ms"].is_null() {
        health["generated_at_ms"] = json!(until);
    }
    if let Some(posts) = crate::scorecard::interventions::queue_posts_between(
        until.saturating_sub(crate::scorecard::time::DAY_MS),
        until,
    ) {
        add_queue_posts(&mut health, posts.len());
    }
    health["disk"] = crate::disk::report(&store.host_disk().map_err(err)?);
    health["memory"] = crate::memory::report();
    health["storage"] = storage::report(&store, now_ms())?;
    health["backups"] = backups(&store)?;
    let mut agents = store.fleet_board().map_err(err)?;
    labels::label_fleet(&mut agents, &label_context(&store)?);
    with_fleet_liveness(&mut agents, until);
    Ok(json!({
        "generated_at_ms": until,
        "agents": agents,
        "health": health,
    }))
}

/// The owner's `posts` posts in the queue over the strip's day (objective
/// 158), added as the Home card adds them: `interventions_24h` gains them,
/// `queue_posts_24h` says how many, and `human_per_agent_hour` is worked out
/// again from `interventions_24h` and `agent_ms_24h` (the card's one ratio).
/// Only called when the ranges file names the queue: without it the strip
/// is the store's, with no `queue_posts_24h`.
fn add_queue_posts(health: &mut Value, posts: usize) {
    let posts = i64::try_from(posts).unwrap_or(i64::MAX);
    let total = health["interventions_24h"]
        .as_i64()
        .unwrap_or(0)
        .saturating_add(posts);
    let agent_ms = health["agent_ms_24h"].as_i64().unwrap_or(0);
    health["interventions_24h"] = json!(total);
    health["queue_posts_24h"] = json!(posts);
    health["human_per_agent_hour"] =
        crate::scorecard::interventions::per_agent_hour(total, agent_ms);
}

/// The health's `backups`: `{dump, bundle}`, each the newest `backup.dumped`
/// or `backup.bundled` record as `{at_ms, key, bytes}`, or null when there
/// is none.
fn backups(store: &spawn_postgres::PostgresStore) -> Result<Value, String> {
    store.newest_backups().map_err(err)
}

/// What the fleet board and the attention queue label their agents with
/// ([`labels`]): the missions, their tasks and the residents.
fn label_context(store: &spawn_postgres::PostgresStore) -> Result<labels::Context, String> {
    let facts = store.label_facts().map_err(err)?;
    Ok(labels::Context::from_board(&facts, &labels::Titles::new()))
}

/// `/api/sessions/<id>/judgements` (`what` `judgements`) and
/// `/api/sessions/<id>/commands` (any other `what`), `?limit=` (50, at most
/// 500).
fn session_list(id: &str, what: &str, query: &str) -> Result<Value, String> {
    let session = session_id(id)?;
    let limit = parameter(query, "limit")
        .and_then(|text| text.parse::<u32>().ok())
        .unwrap_or(50)
        .min(500);
    let store = open_store()?;
    match what {
        "judgements" => store.judgements_json(session.as_str(), limit).map_err(err),
        _ => store
            .commands_json(session.as_str(), limit, crate::config::message_taken_ms())
            .map_err(err),
    }
}

/// The tail of a session's terminal, read from Herdr: a live peek at compute,
/// not storage. Capturing a pane changes nothing, so the API stays read-only.
/// It leaves the host masked unless the session's newest start is
/// positively Claude Code's (decision C2, N4: a failed lookup masks;
/// `spawn_core::driver::excerpt_for`).
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
    let host = crate::config::terminal_host()?;
    let text = if live {
        let raw = host
            .capture_tail(&terminal, &workspace, PANE_LINES)
            .unwrap_or_default();
        let engine = store.latest_engine(&session).ok().flatten();
        spawn_core::driver::excerpt_for(engine.as_deref(), &raw)
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
    let links = store
        .session_links(session.as_str())
        .map_err(|error| error.to_string())?;
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
                "restart_of": links["restart_of"][row.execution_id.as_str()].clone(),
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
        "reassigned_to": links["reassigned_to"].clone(),
        "reassigned_from": links["reassigned_from"].clone(),
        "open_tools": open_tools,
        "freshness": freshness,
        "timeline": timeline,
    }))
}

/// Where a read of the whole log starts: after a position, or at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogStart {
    After(u64),
    Since(i64),
}

/// The start and page size of `/api/events`: `after=<position>` or
/// `since_ms=<epoch ms>` (not both; `since=` is `since_ms=`'s alias, both
/// spellings at once a 400), and `limit`, capped. A number that does
/// not parse is the caller's mistake.
fn log_window(query: &str) -> Result<(LogStart, usize), String> {
    let number = |name: &str| -> Result<Option<i64>, String> {
        parameter(query, name)
            .map(|text| {
                text.parse::<i64>()
                    .ok()
                    .filter(|value| *value >= 0)
                    .ok_or_else(|| format!("{BAD_REQUEST}bad {name} {text}"))
            })
            .transpose()
    };
    let since = match (number("since_ms")?, number("since")?) {
        (Some(_), Some(_)) => {
            return Err(format!(
                "{BAD_REQUEST}since_ms or its alias since, not both"
            ));
        }
        (since_ms, since) => since_ms.or(since),
    };
    let start = match (number("after")?, since) {
        (Some(_), Some(_)) => return Err(format!("{BAD_REQUEST}after or since_ms, not both")),
        (_, Some(since)) => LogStart::Since(since),
        (after, None) => LogStart::After(after.unwrap_or(0).unsigned_abs()),
    };
    let limit = number("limit")?
        .map_or(DEFAULT_LIMIT, |limit| {
            usize::try_from(limit).unwrap_or(MAX_LIMIT)
        })
        .min(MAX_LIMIT);
    Ok((start, limit))
}

/// Events of the whole log in log order, for the Show view: a page after a
/// position, and `next`, the position to read after next time (the last
/// one returned, or the start when nothing is newer). Read-only, through
/// the reader login like every other answer.
fn events(query: &str) -> Result<Value, String> {
    let (start, limit) = log_window(query)?;
    let store = open_store()?;
    let after = match start {
        LogStart::After(after) => GlobalPosition(after),
        LogStart::Since(since) => store
            .position_before(spawn_core::time::Timestamp(since))
            .map_err(err)?,
    };
    let entries = store.log_after(after, limit).map_err(err)?;
    let next = entries.last().map_or(after, |entry| entry.position);
    let events: Vec<Value> = entries
        .into_iter()
        .map(|entry| {
            json!({
                "id": entry.position.0,
                "agent_id": entry.agent_id,
                "session_id": entry.session_id,
                "execution_id": entry.execution_id,
                "source": entry.source,
                "event_name": entry.event_name,
                "detail": entry.detail,
                "recorded_at_ms": entry.recorded_at.0,
            })
        })
        .collect();
    Ok(json!({ "events": events, "next": next.0, "now_ms": now_ms() }))
}

/// `/api/scorecard?mission=&since=&until=`: exactly the JSON of `spawn
/// scorecard --json` with the same flags (crate::scorecard), read through
/// the reader login and from this server's own transcript archive
/// (`SPAWN_WORKSPACE_ROOT`). An empty parameter is one not given.
fn scorecard(query: &str) -> Result<Value, String> {
    let given = |name: &str| -> Result<Option<String>, String> {
        parameter(query, name)
            .map(|text| {
                percent_decoded(text).ok_or_else(|| format!("{BAD_REQUEST}bad {name} {text}"))
            })
            .transpose()
            .map(|value| value.filter(|text| !text.is_empty()))
    };
    let asked = crate::scorecard::Query {
        mission: given("mission")?,
        since: given("since")?,
        until: given("until")?,
    };
    let store = open_store()?;
    let root = env_value("SPAWN_WORKSPACE_ROOT");
    crate::scorecard::measure(&store, root.as_deref().map(Path::new), &asked).map_err(|failure| {
        match failure {
            crate::scorecard::Failure::Request(error) => format!("{BAD_REQUEST}{error}"),
            crate::scorecard::Failure::Record(error) => error,
        }
    })
}

/// `/api/scorecard/fleet?window=24h|7d|30d|90d&until=<RFC 3339>`: how the
/// fleet decides over `[until − window, until)` (24h when the window is
/// absent, now when `until` is), from the experience views
/// (crate::scorecard::fleet), read through the reader login. Any other
/// window, or an `until` that is not a time, is the caller's mistake: 400.
fn scorecard_fleet(query: &str) -> Result<Value, String> {
    let given = |name: &str| -> Result<Option<String>, String> {
        parameter(query, name)
            .map(|text| {
                percent_decoded(text).ok_or_else(|| format!("{BAD_REQUEST}bad {name} {text}"))
            })
            .transpose()
            .map(|value| value.filter(|text| !text.is_empty()))
    };
    let (window, until) = (given("window")?, given("until")?);
    let failure = |failure: crate::scorecard::Failure| match failure {
        crate::scorecard::Failure::Request(error) => format!("{BAD_REQUEST}{error}"),
        crate::scorecard::Failure::Record(error) => error,
    };
    let mut store = None;
    let (name, span) =
        crate::scorecard::fleet::window_of(window.as_deref(), until.as_deref(), || {
            let opened = open_store().map_err(crate::scorecard::Failure::Record)?;
            let now = opened
                .database_now_ms()
                .map_err(|error| crate::scorecard::Failure::Record(error.to_string()))?;
            store = Some(opened);
            Ok(now)
        })
        .map_err(failure)?;
    let store = match store {
        Some(store) => store,
        None => open_store()?,
    };
    crate::scorecard::fleet::measure(&store, name, span).map_err(failure)
}

/// `/api/interventions?window=24h|7d&until=<RFC 3339>`: the north star
/// over `[until − window, until)` (24h when the window is absent, now when
/// `until` is) and the interventions behind it, one per intent of a person
/// (crate::scorecard::interventions), read through the reader login. Any
/// other window, or an `until` that is not a time, is the caller's
/// mistake: 400.
fn interventions(query: &str) -> Result<Value, String> {
    let given = |name: &str| -> Result<Option<String>, String> {
        parameter(query, name)
            .map(|text| {
                percent_decoded(text).ok_or_else(|| format!("{BAD_REQUEST}bad {name} {text}"))
            })
            .transpose()
            .map(|value| value.filter(|text| !text.is_empty()))
    };
    let (window, until) = (given("window")?, given("until")?);
    let failure = |failure: crate::scorecard::Failure| match failure {
        crate::scorecard::Failure::Request(error) => format!("{BAD_REQUEST}{error}"),
        crate::scorecard::Failure::Record(error) => error,
    };
    let mut store = None;
    let (name, span) =
        crate::scorecard::interventions::window_of(window.as_deref(), until.as_deref(), || {
            let opened = open_store().map_err(crate::scorecard::Failure::Record)?;
            let now = opened
                .database_now_ms()
                .map_err(|error| crate::scorecard::Failure::Record(error.to_string()))?;
            store = Some(opened);
            Ok(now)
        })
        .map_err(failure)?;
    let store = match store {
        Some(store) => store,
        None => open_store()?,
    };
    crate::scorecard::interventions::measure(&store, name, span).map_err(failure)
}

/// `/api/alerts?since=` (or its alias `?window=`, not both): exactly the JSON array of `spawn alerts --once
/// --json` with the same flag (`crate::alerts::window`, notices folded:
/// objectives 151 and 166), from the same store method, read through the
/// reader login. An empty parameter is the configured window.
fn alerts(query: &str) -> Result<Value, String> {
    let store = open_store()?;
    let since = alerts_since(query)?;
    crate::alerts::window(&store, since.as_deref())
}

/// `/api/alerts`' span: `?since=`, or its alias `?window=` (both at once,
/// or one that does not decode, is the caller's mistake).
fn alerts_since(query: &str) -> Result<Option<String>, String> {
    let spelled = |name: &str| {
        parameter(query, name)
            .map(|text| {
                percent_decoded(text).ok_or_else(|| format!("{BAD_REQUEST}bad {name} {text}"))
            })
            .transpose()
    };
    match (spelled("since")?, spelled("window")?) {
        (Some(_), Some(_)) => Err(format!("{BAD_REQUEST}since or its alias window, not both")),
        (since, window) => Ok(since.or(window)),
    }
}

/// `/api/cost?window=24h|7d` (24h when absent; any other window is the
/// caller's mistake, 400): what the model calls timed in [now minus the
/// window, now) cost, per model, mission, task, agent and execution
/// (crate::cost), and what a landing cost in it, for the fleet and each
/// mission (`landed`, `usd_per_landing`), read through the reader login.
/// Each `tasks` row carries its `label`, the task's label as
/// `/api/missions` gives it ([`labels`]), or null for a task the record
/// does not hold; each `missions` row its `name`, the mission's
/// `mission_name` as `/api/missions` gives it, or null likewise.
fn cost(query: &str) -> Result<Value, String> {
    let asked = parameter(query, "window")
        .map(|text| percent_decoded(text).ok_or_else(|| format!("{BAD_REQUEST}bad window {text}")))
        .transpose()?;
    let (name, length_ms) = crate::cost::window_of(asked.as_deref())
        .map_err(|error| format!("{BAD_REQUEST}{error}"))?;
    let store = open_store()?;
    let mut answer = crate::cost::window(&store, name, length_ms)?;
    label_cost_tasks(&store, &mut answer)?;
    Ok(answer)
}

/// A cost answer's `tasks` rows, each with its `label` as `/api/missions`
/// gives it, and its `missions` rows, each with its `name` (null for a
/// task or mission the record does not hold): read for those tasks and
/// missions alone ([`spawn_postgres::PostgresStore::label_facts_of`]).
fn label_cost_tasks(
    store: &spawn_postgres::PostgresStore,
    answer: &mut Value,
) -> Result<(), String> {
    let ids_of = |list: &str, field: &str| -> Vec<i64> {
        answer[list]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|row| row[field].as_i64())
            .collect()
    };
    let tasks = ids_of("tasks", "task");
    let missions = ids_of("missions", "mission");
    let facts = store.label_facts_of(&tasks, &missions).map_err(err)?;
    labels::label_cost(
        answer,
        &labels::Context::from_board(&facts, &labels::Titles::new()),
    );
    Ok(())
}

/// A query parameter's value with its `%XX` escapes decoded (a `+` stays a
/// `+`: a time zone offset is one); `None` when it decodes to no UTF-8.
fn percent_decoded(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(byte) = text
                .get(index + 1..index + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(out).ok()
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
    // A file's tag is its bytes'; the build's assets/ are named by their
    // content and kept by the browser for good, the page asked again.
    let etag = answers::file_etag(&bytes);
    let tag = Header::from_bytes("ETag", etag.as_str()).expect("a hex tag is a valid header");
    let cache = Header::from_bytes("Cache-Control", answers::file_cache_control(&relative))
        .expect("static header is valid");
    if answers::not_modified(header(&request, "If-None-Match"), &etag) {
        let unchanged = Response::empty(304).with_header(tag).with_header(cache);
        return request
            .respond(unchanged)
            .map_err(|error| error.to_string());
    }
    let header = Header::from_bytes("Content-Type", content_type(&relative))
        .expect("static header is valid");
    request
        .respond(
            Response::from_data(bytes)
                .with_header(header)
                .with_header(tag)
                .with_header(cache),
        )
        .map_err(|error| error.to_string())
}

/// The `Content-Type` of a built file: a web app manifest as
/// `application/manifest+json` and icons by their kind among them (the
/// dashboard's own PWA shell is embedded, serve/pwa.rs).
fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("webmanifest") => "application/manifest+json",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
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
        Arc, BAD_REQUEST, DEFAULT_LIMIT, FeatureAssociateRefusal, LogStart, MAX_IN_FLIGHT,
        MAX_INPUT_BODY, MAX_INPUT_TEXT, MAX_LIMIT, ProcSocket, Shared, Slots, access_token,
        backups, content_type, coverage_projects, coverage_registry_checked, feature_associate_at,
        feature_paths, handle, is_json, is_loopback, keep_listening, listen_again_pause,
        local_exemption, log_window, mission_feature_paths, mission_query, objective_json,
        objective_prompt, open_network_request, open_networks, owner_uid, peer_uid, proc_sockets,
        public_nodes, rsi_objective, rsi_summary, same_origin, same_secret, serve_requests,
        server_hosts, stream, tokenless_request, typed_text, with_activity, with_fleet_liveness,
        with_last_gates, with_objectives, with_task_liveness,
    };
    use crate::objectives::Task;
    use std::net::SocketAddr;
    use std::path::PathBuf;

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

    #[test]
    fn an_objective_is_read_whole_from_its_missions_backlog() {
        let dir = std::env::temp_dir().join(format!("spawn-serve-backlog-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        // Longer than the first lines `/api/rsi` sends.
        let prompt = format!(
            "Make the gate faster.\n{}",
            "Every line counts.\n".repeat(200)
        );
        let objective = json!({"id": 4, "title": "Faster gate", "prompt": prompt});
        std::fs::write(dir.join("004.json"), objective.to_string()).expect("write");
        let missions = json!([
            {"id": 3, "objectives": {"backlog": "/nowhere"}},
            {"id": 7, "objectives": {"backlog": dir.to_str()}},
        ]);

        let answer = objective_prompt(&missions, 7, 4).expect("the objective");
        assert_eq!(answer["prompt"], json!(prompt), "the whole prompt");
        assert_eq!(answer["prompt_bytes"], json!(prompt.len()));
        assert_eq!(answer["title"], "Faster gate");
        assert_eq!(answer["mission"], 7);
        assert!(
            objective_prompt(&missions, 7, 5).is_err(),
            "not in the backlog"
        );
        assert!(
            objective_prompt(&missions, 8, 4).is_err(),
            "no such open mission"
        );
        // A malformed path is refused before the store is opened.
        for rest in ["", "7", "7/", "x/4", "7/x", "7/4/1"] {
            let error = rsi_objective(rest).expect_err("malformed");
            assert!(error.starts_with(BAD_REQUEST), "{rest}: {error}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

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
    fn open_networks_are_cidrs_and_a_malformed_one_stops_serve() {
        assert!(open_networks(None).expect("unset").is_empty());
        assert!(open_networks(Some(" , ")).expect("empty").is_empty());
        let open = open_networks(Some("192.168.31.0/24, 100.64.0.0/10,fd00::/8, 10.0.0.5"))
            .expect("four networks");
        let shown: Vec<String> = open.iter().map(ToString::to_string).collect();
        assert_eq!(
            shown,
            [
                "192.168.31.0/24",
                "100.64.0.0/10",
                "fd00::/8",
                "10.0.0.5/32"
            ]
        );
        for wrong in [
            "192.168.31.0/33",
            "intranet",
            "192.168.31.0/x",
            "fd00::/129",
        ] {
            assert!(open_networks(Some(wrong)).is_err(), "{wrong} is refused");
        }
        for everything in ["0.0.0.0/0", "::/0"] {
            assert!(
                open_networks(Some(everything)).is_err(),
                "{everything} would be no token at all"
            );
        }
        let ip = |text: &str| text.parse::<std::net::IpAddr>().expect("ip");
        let lan = open[0];
        assert!(lan.contains(ip("192.168.31.7")));
        assert!(!lan.contains(ip("192.168.32.7")));
        assert!(
            lan.contains(ip("::ffff:192.168.31.7")),
            "IPv4 mapped into IPv6"
        );
        assert!(!lan.contains(ip("fd00::1")));
        assert!(open[2].contains(ip("fd12:3456::1")));
        assert!(open[3].contains(ip("10.0.0.5")) && !open[3].contains(ip("10.0.0.6")));
    }

    /// The owner's intranet reads without the token (2026-11-06, "No token
    /// needed for the intranet"): a client inside an open network that names
    /// this server by its address inside it. Nothing else does.
    #[test]
    fn a_client_on_an_open_network_reads_without_the_token_and_nothing_else_does() {
        let open = open_networks(Some("192.168.31.0/24")).expect("network");
        let peer = Some(address("192.168.31.50:51234"));
        let host = Some("192.168.31.112:8787");
        let port = Some(8787);
        assert!(open_network_request(peer, host, None, port, &open));
        assert!(open_network_request(
            peer,
            host,
            Some("http://192.168.31.112:8787"),
            port,
            &open
        ));
        assert!(
            open_network_request(
                Some(address("[::ffff:192.168.31.50]:51234")),
                host,
                None,
                port,
                &open
            ),
            "an IPv4 client on an IPv6 socket"
        );
        let refused = [
            (
                Some(address("100.126.1.1:51234")),
                host,
                None,
                "a client outside the network",
            ),
            (
                peer,
                Some("evil.example:8787"),
                None,
                "a page that rebinds its own name",
            ),
            (
                peer,
                Some("192.168.31.112:9999"),
                None,
                "a Host naming another port",
            ),
            (
                peer,
                Some("192.168.31.112"),
                None,
                "a Host with no port (port 80)",
            ),
            (
                peer,
                Some("8.8.8.8:8787"),
                None,
                "a Host outside the network",
            ),
            (
                peer,
                host,
                Some("http://evil.example"),
                "a cross-origin page",
            ),
            (
                peer,
                host,
                Some("https://192.168.31.112:8787"),
                "another scheme",
            ),
            (None, host, None, "no peer address"),
            (peer, None, None, "no Host"),
        ];
        for (peer, host, origin, why) in refused {
            assert!(
                !open_network_request(peer, host, origin, port, &open),
                "{why}"
            );
        }
        assert!(
            !open_network_request(peer, host, None, port, &[]),
            "no open network set: the token, as before"
        );
    }

    /// The handler itself lets an open network's client through with a token
    /// set, and only then: here the "intranet" is loopback, the one network
    /// a test can be a client on. A read let through finds no database (no
    /// SPAWN_DATABASE_URL in the test), which is anything but 401.
    #[test]
    fn the_handler_opens_the_api_to_an_open_network_only() {
        let named = "Host: 127.0.0.1:PORT\r\n";
        assert_eq!(
            status_with(Some("secret"), "", "/api/overview", named),
            401,
            "no open network: the token"
        );
        assert_ne!(
            status_with(Some("secret"), "127.0.0.0/8", "/api/overview", named),
            401,
            "a client inside the open network naming this server by its address"
        );
        assert_eq!(
            status_with(
                Some("secret"),
                "127.0.0.0/8",
                "/api/overview",
                "Host: localhost:PORT\r\n"
            ),
            401,
            "a name, not an address: the token"
        );
        assert_eq!(
            status_with(Some("secret"), "10.0.0.0/8", "/api/overview", named),
            401,
            "a client outside the open network: the token"
        );
    }

    /// The body of an input every guard but the one tested lets through.
    const BODY: &str = r#"{"text":"stop objective_87","key":"k-1"}"#;

    /// `answer` refuses with `status`, before the database: these tests set
    /// no SPAWN_DATABASE_URL, so a request past every guard would answer 500
    /// "SPAWN_DATABASE_URL is not set" instead.
    fn refused(answer: &Answer, status: u16, why: &str) {
        assert_eq!(answer.status, status, "{why}: {answer:?}");
        assert!(
            !answer.body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("SPAWN_DATABASE_URL"),
            "{why}: refused before the database"
        );
    }

    /// The read guard refuses an input as it refuses a read: the clients
    /// that may read may write, and no other.
    #[test]
    fn the_read_guard_refuses_an_input_as_it_refuses_a_read() {
        let json = "Content-Type: application/json\r\n";
        for (token, open, headers, status, why) in [
            (
                Some("secret"),
                "",
                "Host: 127.0.0.1:PORT\r\n",
                401,
                "a token set and none carried",
            ),
            (
                Some("secret"),
                "",
                "Host: attacker.example:PORT\r\nOrigin: http://attacker.example:PORT\r\n",
                401,
                "a rebound name",
            ),
            (
                None,
                "",
                "Host: attacker.example:PORT\r\n",
                403,
                "no token set, a Host that does not name this server",
            ),
            (
                None,
                "",
                "Host: 127.0.0.1:PORT\r\nOrigin: http://attacker.example\r\n",
                403,
                "a page on another site",
            ),
            (
                Some("secret"),
                "127.0.0.0/8",
                "Host: 127.0.0.1:PORT\r\nOrigin: http://evil.example\r\n",
                401,
                "an open network's client from another site's page",
            ),
        ] {
            let headers = format!("{headers}{json}");
            refused(&post(token, open, None, INPUT, &headers, BODY), status, why);
        }
    }

    /// Past the read guard, an input without the token needs its page's own
    /// `Origin`, which a browser sends with every POST: a script with no
    /// Origin needs the token.
    #[test]
    fn an_input_without_the_token_needs_its_pages_origin() {
        let named = "Host: 127.0.0.1:PORT\r\nContent-Type: application/json\r\n";
        for (token, open, why) in [
            (None, "", "no token set"),
            (Some("secret"), "127.0.0.0/8", "an open network's client"),
        ] {
            let answer = post(token, open, None, INPUT, named, BODY);
            refused(&answer, 403, why);
            assert!(
                answer.body["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("Origin")),
                "{why}: {answer:?}"
            );
        }
    }

    #[test]
    fn the_input_path_takes_post_only() {
        for method in ["GET", "PUT", "DELETE", "PATCH"] {
            let answer = exchange(
                Some("secret"),
                "",
                None,
                &format!(
                    "{method} {INPUT} HTTP/1.1\r\nHost: 127.0.0.1:PORT\r\n\
                     Authorization: Bearer secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                ),
            );
            refused(&answer, 405, method);
            assert!(answer.head.contains("Allow: POST"), "{method}: {answer:?}");
        }
    }

    /// Only JSON (a form or a text post is one a page may send without
    /// asking first), and at most 16 KiB of it, declared or chunked.
    #[test]
    fn an_input_is_a_json_body_of_at_most_16_kib() {
        for content in [
            "",
            "Content-Type: text/plain\r\n",
            "Content-Type: application/x-www-form-urlencoded\r\n",
            "Content-Type: multipart/form-data; boundary=x\r\n",
        ] {
            let headers =
                format!("Host: 127.0.0.1:PORT\r\nAuthorization: Bearer secret\r\n{content}");
            refused(
                &post(Some("secret"), "", None, INPUT, &headers, BODY),
                415,
                content,
            );
        }
        let big = format!(r#"{{"text":"{}","key":"k-1"}}"#, "a".repeat(MAX_INPUT_BODY));
        refused(
            &post(Some("secret"), "", None, INPUT, SCRIPT, &big),
            413,
            "a body over 16 KiB",
        );
        let chunked = exchange(
            Some("secret"),
            "",
            None,
            &format!(
                "POST {INPUT} HTTP/1.1\r\nHost: 127.0.0.1:PORT\r\nAuthorization: Bearer secret\r\n\
                 Content-Type: application/json\r\nTransfer-Encoding: chunked\r\n\
                 Connection: close\r\n\r\n{:x}\r\n{big}\r\n0\r\n\r\n",
                big.len()
            ),
        );
        refused(&chunked, 413, "a chunked body over 16 KiB");
    }

    /// The body names a text and its key, nothing else: no action type, no
    /// agent; the text is one a person types.
    #[test]
    fn an_inputs_body_is_a_text_and_a_key_and_nothing_else() {
        let long = format!(
            r#"{{"text":"{}","key":"k-1"}}"#,
            "a".repeat(MAX_INPUT_TEXT + 1)
        );
        for (body, why) in [
            ("not json", "not JSON"),
            (r#"{"text":"x","key":"k","type":"stop"}"#, "an action type"),
            (
                r#"{"text":"x","key":"k","agent":"agent_worker_a"}"#,
                "an agent",
            ),
            (r#"{"text":"x"}"#, "no key"),
            (r#"{"key":"k"}"#, "no text"),
            (r#"{"text":"  \n ","key":"k"}"#, "a blank text"),
            (long.as_str(), "a text over 8 KiB"),
            (r#"{"text":"a\u001b[201~b","key":"k"}"#, "an escape"),
            (r#"{"text":"x","key":""}"#, "an empty key"),
            (r#"{"probe":true,"text":"x"}"#, "a probe with a text"),
        ] {
            refused(
                &post(Some("secret"), "", None, INPUT, SCRIPT, body),
                400,
                why,
            );
        }
        let other_key = format!("{SCRIPT}Idempotency-Key: other\r\n");
        refused(
            &post(Some("secret"), "", None, INPUT, &other_key, BODY),
            400,
            "a key in the body and another in the header",
        );
        let malformed = "/api/session/not%20a%20session/input";
        refused(
            &post(Some("secret"), "", None, malformed, SCRIPT, BODY),
            400,
            "a malformed session id",
        );
    }

    const KEY: &str = "/api/session/session_supervisor/key";

    /// The key path has the input's guards (POST only, the page's Origin
    /// without the token, JSON), and its body is a name of the closed set
    /// and an idempotency key, nothing else: no text, no action type.
    /// Past every guard it reaches the database (here none: 500).
    #[test]
    fn a_keys_path_has_the_inputs_guards_and_a_name_of_the_set() {
        let get = exchange(
            Some("secret"),
            "",
            None,
            &format!(
                "GET {KEY} HTTP/1.1\r\nHost: 127.0.0.1:PORT\r\n\
                 Authorization: Bearer secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
        );
        refused(&get, 405, "GET");
        let body = r#"{"name":"escape","key":"k-1"}"#;
        let named = "Host: 127.0.0.1:PORT\r\nContent-Type: application/json\r\n";
        let no_origin = post(None, "", None, KEY, named, body);
        refused(&no_origin, 403, "no token and no Origin");
        let text =
            "Host: 127.0.0.1:PORT\r\nAuthorization: Bearer secret\r\nContent-Type: text/plain\r\n";
        refused(
            &post(Some("secret"), "", None, KEY, text, body),
            415,
            "not JSON",
        );
        for (bad, why) in [
            ("not json", "not JSON"),
            (r#"{"name":"escape"}"#, "no idempotency key"),
            (r#"{"key":"k"}"#, "no name"),
            (r#"{"name":"f1","key":"k"}"#, "a key outside the set"),
            (r#"{"name":"Escape","key":"k"}"#, "names are exact"),
            (r#"{"name":"escape","key":"k","text":"x"}"#, "a text"),
            (
                r#"{"name":"escape","key":"k","type":"stop"}"#,
                "an action type",
            ),
            (r#"{"name":"escape","key":""}"#, "an empty key"),
        ] {
            refused(&post(Some("secret"), "", None, KEY, SCRIPT, bad), 400, why);
        }
        let f1 = post(
            Some("secret"),
            "",
            None,
            KEY,
            SCRIPT,
            r#"{"name":"f1","key":"k"}"#,
        );
        assert!(
            f1.body["error"]
                .as_str()
                .is_some_and(|error| error.contains(r#"key "f1" is not one of escape, up"#)),
            "the refusal names the key: {f1:?}"
        );
        let reached = post(Some("secret"), "", None, KEY, SCRIPT, body);
        assert_eq!(reached.status, 500, "{reached:?}");
        assert_eq!(reached.body["error"], "SPAWN_DATABASE_URL is not set");
    }

    /// The restart and reassign paths have the input's guards (POST only,
    /// the page's Origin without the token, JSON), an idempotency key (via
    /// `"key"` and/or `Idempotency-Key` header, matching when both), and an
    /// optional reason of at most 2000 bytes with no control character. Past
    /// every guard each reaches the database (here none: 500).
    #[test]
    fn the_restart_and_reassign_paths_have_the_inputs_guards_and_a_reason_alone() {
        for path in [
            "/api/session/session_worker_a_7/restart",
            "/api/session/session_worker_a_7/reassign",
        ] {
            let get = exchange(
                Some("secret"),
                "",
                None,
                &format!(
                    "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:PORT\r\n\
                     Authorization: Bearer secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                ),
            );
            refused(&get, 405, "GET");
            let body = r#"{"reason":"stalled on a hung tool","key":"move-1"}"#;
            let named = "Host: 127.0.0.1:PORT\r\nContent-Type: application/json\r\n";
            refused(
                &post(None, "", None, path, named, body),
                403,
                "no token and no Origin",
            );
            let text = "Host: 127.0.0.1:PORT\r\nAuthorization: Bearer secret\r\n\
                        Content-Type: text/plain\r\n";
            refused(
                &post(Some("secret"), "", None, path, text, body),
                415,
                "not JSON",
            );
            let long = format!(r#"{{"reason":"{}","key":"k"}}"#, "x".repeat(2001));
            let long_key = format!(r#"{{"reason":"x","key":"{}"}}"#, "k".repeat(257));
            for (bad, why) in [
                ("not json", "not JSON"),
                (r#"{"reason":"stalled on a hung tool"}"#, "missing key"),
                (r#"{"reason":"x","key":""}"#, "empty key"),
                (r#"{"reason":"x","key":"k\u0007"}"#, "control char in key"),
                (long_key.as_str(), "key too long"),
                (r#"{"reason":"x","key":"k","task":7}"#, "a task"),
                (
                    r#"{"reason":"x","key":"k","type":"stop"}"#,
                    "an action type",
                ),
                (r#"{"reason":"a\u001bb","key":"k"}"#, "a control character"),
                (long.as_str(), "too long"),
            ] {
                refused(&post(Some("secret"), "", None, path, SCRIPT, bad), 400, why);
            }
            let with_other_header = format!("{SCRIPT}Idempotency-Key: other-key\r\n");
            refused(
                &post(Some("secret"), "", None, path, &with_other_header, body),
                400,
                "mismatched body key and Idempotency-Key header",
            );
            let reached = post(Some("secret"), "", None, path, SCRIPT, body);
            assert_eq!(reached.status, 500, "{reached:?}");
            assert_eq!(reached.body["error"], "SPAWN_DATABASE_URL is not set");

            let with_same_header = format!("{SCRIPT}Idempotency-Key: move-1\r\n");
            let reached_both = post(Some("secret"), "", None, path, &with_same_header, body);
            assert_eq!(reached_both.status, 500, "{reached_both:?}");

            let header_only = format!("{SCRIPT}Idempotency-Key: header-move-1\r\n");
            let reached_header = post(
                Some("secret"),
                "",
                None,
                path,
                &header_only,
                r#"{"reason":"stalled"}"#,
            );
            assert_eq!(reached_header.status, 500, "{reached_header:?}");
        }
    }

    const RESUME: &str = "/api/mission/6/resume";

    /// The resume path has the input's guards (POST only, the page's Origin
    /// without the token, JSON), a body of a halt and a key alone (or a
    /// probe), and a mission named by its number. Past every guard it
    /// reaches the database (here none: 500).
    #[test]
    fn a_resumes_path_has_the_inputs_guards_and_a_halt_and_a_key() {
        let get = exchange(
            Some("secret"),
            "",
            None,
            &format!(
                "GET {RESUME} HTTP/1.1\r\nHost: 127.0.0.1:PORT\r\n\
                 Authorization: Bearer secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ),
        );
        refused(&get, 405, "GET");
        let body = r#"{"halt":61,"key":"k-1"}"#;
        let named = "Host: 127.0.0.1:PORT\r\nContent-Type: application/json\r\n";
        refused(
            &post(None, "", None, RESUME, named, body),
            403,
            "no token and no Origin",
        );
        let elsewhere = format!("{named}Origin: http://elsewhere.example\r\n");
        refused(
            &post(None, "", None, RESUME, &elsewhere, body),
            403,
            "another site's page",
        );
        let text = "Host: 127.0.0.1:PORT\r\nAuthorization: Bearer secret\r\n\
                    Content-Type: text/plain\r\n";
        refused(
            &post(Some("secret"), "", None, RESUME, text, body),
            415,
            "not JSON",
        );
        for (bad, why) in [
            ("not json", "not JSON"),
            (r#"{"halt":61}"#, "no idempotency key"),
            (r#"{"key":"k"}"#, "no halt"),
            (r#"{"halt":"61","key":"k"}"#, "a halt that is no event id"),
            (r#"{"halt":61,"key":"k","by":"owner"}"#, "who resumes"),
            (r#"{"halt":61,"key":""}"#, "an empty key"),
            (r#"{"probe":true,"halt":61}"#, "a probe with a halt"),
        ] {
            refused(
                &post(Some("secret"), "", None, RESUME, SCRIPT, bad),
                400,
                why,
            );
        }
        for path in ["/api/mission/six/resume", "/api/mission/-1/resume"] {
            refused(
                &post(Some("secret"), "", None, path, SCRIPT, body),
                400,
                "a mission that is no number",
            );
        }
        let other_key = format!("{SCRIPT}Idempotency-Key: other\r\n");
        refused(
            &post(Some("secret"), "", None, RESUME, &other_key, body),
            400,
            "a key in the body and another in the header",
        );
        for reaching in [body, r#"{"probe":true}"#] {
            let reached = post(Some("secret"), "", None, RESUME, SCRIPT, reaching);
            assert_eq!(reached.status, 500, "{reached:?}");
            assert_eq!(reached.body["error"], "SPAWN_DATABASE_URL is not set");
        }
        let header_key = format!("{SCRIPT}Idempotency-Key: k-1\r\n");
        let reached = post(
            Some("secret"),
            "",
            None,
            RESUME,
            &header_key,
            r#"{"halt":61}"#,
        );
        assert_eq!(reached.status, 500, "the key in its header: {reached:?}");
    }

    /// A person's resume reaches the test database as the `spawn_human`
    /// login: the halt the mission is in, once per key, `by` human; any
    /// other is 409 with the reason and records nothing; no mission is 404.
    #[test]
    fn a_persons_resume_reaches_the_test_database_once_for_the_halt_it_names() {
        in_test_schema(|owner, store| {
            let to = |path: &str, body: &str| {
                post(
                    Some("secret"),
                    "",
                    Some(owner.to_string()),
                    path,
                    SCRIPT,
                    body,
                )
            };
            let mission = store
                .mission_add("Resumed from the page", &json!([]), "human")
                .expect("a mission");
            let path = format!("/api/mission/{mission}/resume");
            let probe = to(&path, r#"{"probe":true}"#);
            assert_eq!(probe.status, 200, "{probe:?}");
            let idle = to(&path, r#"{"halt":1,"key":"r-0"}"#);
            assert_eq!(idle.status, 409, "{idle:?}");
            assert!(
                idle.body["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("not halted")),
                "{idle:?}"
            );
            let halt = store
                .integrator_record(&json!({"type": "mission.halted", "id": mission,
                                           "reason": "the scenarios fell from 5 to 2",
                                           "gone": ["c.sh"]}))
                .expect("halted");
            let stale = to(&path, &format!(r#"{{"halt":{},"key":"r-1"}}"#, halt - 1));
            assert_eq!(stale.status, 409, "{stale:?}");
            let missing = to(
                "/api/mission/999999/resume",
                &format!(r#"{{"halt":{halt},"key":"r-1"}}"#),
            );
            assert_eq!(missing.status, 404, "{missing:?}");
            let body = format!(r#"{{"halt":{halt},"key":"r-2"}}"#);
            let first = to(&path, &body);
            assert_eq!(first.status, 202, "{first:?}");
            let event = first.body["event"].as_i64().expect("the record's event");
            let again = to(&path, &body);
            assert_eq!(again.status, 202, "{again:?}");
            assert_eq!(again.body["event"], event, "a retried POST resumes once");
            let board = store.mission_board(10).expect("the board");
            assert!(board["missions"][0]["halted"].is_null());
            assert!(board["missions"][0]["halt"].is_null());
            let mut client =
                postgres::Client::connect(owner, postgres::NoTls).expect("the owner connects");
            let records: Vec<serde_json::Value> = client
                .query(
                    "select kind -> 'detail' -> 'record' from spawn_events
                      where session_id = 'session_control'
                        and kind -> 'detail' -> 'record' ->> 'type' = 'mission.resumed'",
                    &[],
                )
                .expect("the records")
                .iter()
                .map(|row| row.get(0))
                .collect();
            assert_eq!(records.len(), 1, "{records:?}");
            assert_eq!(records[0]["by"], "human");
            assert_eq!(records[0]["halt"], halt);
            assert_eq!(records[0]["id"], mission);
        });
    }

    /// Who may write past the guards: the token's holder from anywhere, the
    /// page itself on an open network, and, with no token set, the owner's
    /// own client. Each reaches the database (here none: 500).
    #[test]
    fn the_clients_that_read_may_write_from_their_own_page() {
        let body = r#"{"text":"hello","key":"k-1"}"#;
        let reached = |answer: Answer, why: &str| {
            assert_eq!(answer.status, 500, "{why}: {answer:?}");
            assert_eq!(
                answer.body["error"], "SPAWN_DATABASE_URL is not set",
                "{why}: every guard passed"
            );
        };
        reached(
            post(Some("secret"), "", None, INPUT, SCRIPT, body),
            "the token, no Origin",
        );
        reached(
            post(
                Some("secret"),
                "",
                None,
                INPUT,
                &format!("{SCRIPT}Origin: http://elsewhere.example\r\n"),
                body,
            ),
            "the token from another page: it holds the token",
        );
        reached(
            post(Some("secret"), "127.0.0.0/8", None, INPUT, PAGE, body),
            "an open network's page, naming this server by its address",
        );
        reached(
            post(
                Some("secret"),
                "",
                None,
                &format!("{INPUT}?token=secret"),
                PAGE,
                body,
            ),
            "the token as a parameter",
        );
        reached(
            post(
                Some("secret"),
                "",
                None,
                INPUT,
                "Host: 127.0.0.1:PORT\r\nAuthorization: Bearer secret\r\n\
                 Content-Type: Application/JSON; charset=utf-8\r\n",
                body,
            ),
            "JSON with a charset",
        );
        let local = post(None, "", None, INPUT, PAGE, body);
        if owner_known() {
            reached(local, "no token set: the owner's own client (this test)");
        } else {
            assert_eq!(local.status, 403, "no /proc: no client is the owner's");
        }
        let probe = post(None, "", None, INPUT, PAGE, r#"{"probe":true}"#);
        if owner_known() {
            reached(probe, "a probe passes the same guards");
        }
    }

    #[test]
    fn a_read_path_takes_get_and_head_only() {
        for method in ["POST", "PUT", "DELETE"] {
            let answer = exchange(
                None,
                "",
                None,
                &format!(
                    "{method} /api/overview HTTP/1.1\r\nHost: 127.0.0.1:PORT\r\n\
                     Content-Length: 0\r\nConnection: close\r\n\r\n"
                ),
            );
            assert_eq!(answer.status, 405, "{method}");
            assert!(answer.head.contains("Allow: GET"), "{answer:?}");
        }
        // A GET still reads (no database here: anything but 405).
        assert_eq!(
            status_with(
                None,
                "",
                "/api/session/not%20a%20session",
                "Host: 127.0.0.1:PORT\r\n"
            ),
            400
        );
    }

    #[test]
    fn a_persons_text_and_its_page_are_checked_as_typed() {
        assert_eq!(
            typed_text(Some("hi\tthere\r\nok".into())),
            Ok("hi\tthere\r\nok".into())
        );
        assert_eq!(
            typed_text(Some("字".repeat(MAX_INPUT_TEXT / 3))).map(|text| text.len()),
            Ok(MAX_INPUT_TEXT / 3 * 3)
        );
        for bad in [
            None,
            Some(String::new()),
            Some(" \n\t".to_string()),
            Some("a".repeat(MAX_INPUT_TEXT + 1)),
            Some("\u{1b}[A".to_string()),
            Some("\u{9b}A".to_string()),
            Some("a\u{0}b".to_string()),
        ] {
            assert!(typed_text(bad.clone()).is_err(), "{bad:?}");
        }
        assert!(is_json(Some("application/json")));
        assert!(is_json(Some(" application/JSON ; charset=utf-8")));
        assert!(!is_json(Some("text/plain")));
        assert!(!is_json(Some("application/json-seq")));
        assert!(!is_json(None));
        assert!(same_origin(
            Some("127.0.0.1:8787"),
            Some("http://127.0.0.1:8787")
        ));
        assert!(same_origin(
            Some("LOCALHOST:8787"),
            Some("http://localhost:8787")
        ));
        assert!(!same_origin(Some("127.0.0.1:8787"), None), "no Origin");
        assert!(!same_origin(None, Some("http://127.0.0.1:8787")));
        assert!(!same_origin(
            Some("127.0.0.1:8787"),
            Some("https://127.0.0.1:8787")
        ));
        assert!(!same_origin(
            Some("127.0.0.1:8787"),
            Some("http://127.0.0.1:9999")
        ));
        assert!(!same_origin(Some("127.0.0.1:8787"), Some("null")));
    }

    /// Run `case` on a schema of its own in the test database
    /// (`SPAWN_TEST_DATABASE_URL`, never the live one), migrated, its session
    /// `session_supervisor` used by `agent_supervisor`, with its owner's URL
    /// (the schema on its search path, as an install's URL names it) and
    /// store. The schema is dropped afterwards.
    fn in_test_schema(case: impl FnOnce(&str, &spawn_postgres::PostgresStore)) {
        use spawn_core::event::{EventKind, NewEvent, Source};
        use spawn_core::id::AgentId;
        use spawn_core::port::EventLog;
        let url = std::env::var("SPAWN_TEST_DATABASE_URL").expect(
            "SPAWN_TEST_DATABASE_URL must point at a disposable database; tests do not skip",
        );
        let schema = spawn_postgres::testing::TestSchema::new("serve_input");
        let store = spawn_postgres::PostgresStore::connect_in_schema(&url, &schema)
            .expect("a schema of its own, migrated");
        let separator = if url.contains('?') { '&' } else { '?' };
        let owner = format!("{url}{separator}options=-csearch_path%3D{schema}");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            store
                .append(NewEvent {
                    agent_id: AgentId::new("agent_supervisor").expect("valid agent"),
                    session_id: spawn_core::id::SessionId::new("session_supervisor")
                        .expect("valid session"),
                    execution_id: None,
                    source: Source::Claude,
                    source_event_id: Some("message-1".to_string()),
                    kind: EventKind::MessageReceived,
                })
                .expect("the session's first event");
            case(&owner, &store);
        }));
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }

    /// Objective 82, through the real store: the health's `backups` is
    /// null for each kind before any upload, then the newest record of each
    /// kind as `{at_ms, key, bytes}`, whatever other records lie between.
    #[test]
    fn the_health_carries_the_newest_dump_and_bundle() {
        in_test_schema(|owner, store| {
            assert_eq!(
                backups(store).expect("backups"),
                json!({"dump": null, "bundle": null}),
                "no upload yet"
            );
            let mut client =
                postgres::Client::connect(owner, postgres::NoTls).expect("the owner connects");
            let mut upload = |kind: &str, key: &str, bytes: i64, at_ms: i64| {
                let id = store
                    .backup_record(&json!({"type": kind, "keys": [key, "other"], "bytes": bytes}))
                    .expect("a backup record");
                client
                    .execute(
                        "update spawn_events set recorded_at_ms = $1 where id = $2",
                        &[&at_ms, &id],
                    )
                    .expect("placed");
            };
            upload("backup.dumped", "postgres/hourly/a.dump.gpg", 900, 1_000);
            assert_eq!(
                backups(store).expect("backups"),
                json!({"dump": {"at_ms": 1_000, "key": "postgres/hourly/a.dump.gpg", "bytes": 900},
                       "bundle": null}),
                "a dump and no bundle"
            );
            upload("backup.bundled", "bundles/full/b.tar.zst.gpg", 50, 2_000);
            upload("backup.dumped", "postgres/hourly/c.dump.gpg", 940, 3_000);
            store
                .backup_record(&json!({"type": "backup.verified", "ok": true}))
                .expect("a drill record");
            assert_eq!(
                backups(store).expect("backups"),
                json!({"dump": {"at_ms": 3_000, "key": "postgres/hourly/c.dump.gpg", "bytes": 940},
                       "bundle": {"at_ms": 2_000, "key": "bundles/full/b.tar.zst.gpg", "bytes": 50}}),
                "the newest of each kind"
            );
        });
    }

    /// The per-part timing, through the real store: a reading says how
    /// long each of its parts took, and the parts do not overlap (rev-e F1:
    /// every read inside it is in one part). The archive and the cost are the kept
    /// performance values' (another test may have computed them within the
    /// minute), so they are both timed or both not, never one alone.
    #[test]
    fn a_reading_times_each_of_its_parts() {
        in_test_schema(|_owner, store| {
            let dir = std::env::temp_dir().join(format!(
                "spawn-core-lines-parts-{}-{}",
                std::process::id(),
                super::now_ms()
            ));
            std::fs::create_dir_all(&dir).expect("a directory");
            let mut parts = super::core_lines::Parts::default();
            let begun = std::time::Instant::now();
            super::core_lines::core_lines_timed(
                store,
                Some(&dir.join("core-lines.json")),
                Some(&dir.join("nightly")),
                &mut parts,
            )
            .expect("the answer");
            let whole = begun.elapsed();
            assert!(
                [
                    parts.open,
                    parts.trends,
                    parts.facts,
                    parts.storage,
                    parts.health
                ]
                .iter()
                .all(Option::is_some),
                "{parts:?}"
            );
            assert_eq!(parts.archive.is_some(), parts.cost.is_some(), "{parts:?}");
            // Disjoint parts: their sum is never more than the whole.
            assert!(parts.sum() <= whole, "{parts:?} over {whole:?}");
            std::fs::remove_dir_all(&dir).expect("clean up");
        });
    }

    /// The warm-up at serve's start, through the real store: it runs a
    /// reading (its parts say whether it measured or took the kept
    /// figures), and the reading after it, the first person's, takes the
    /// kept performance figures and reads no archive file at all.
    #[test]
    fn a_reading_after_the_warm_up_is_kept_and_reads_no_archive() {
        in_test_schema(|_owner, store| {
            let dir = std::env::temp_dir().join(format!(
                "spawn-core-lines-warm-{}-{}",
                std::process::id(),
                super::now_ms()
            ));
            std::fs::create_dir_all(&dir).expect("a directory");
            let file = dir.join("core-lines.json");
            let nightly = dir.join("nightly");
            let warmed =
                super::core_lines::warm_at(store, Some(&file), Some(&nightly)).expect("warmed");
            assert!(
                warmed.performance.is_some() && warmed.storage.is_some(),
                "the warm-up ran a reading: {warmed:?}"
            );
            let mut parts = super::core_lines::Parts::default();
            super::core_lines::core_lines_timed(store, Some(&file), Some(&nightly), &mut parts)
                .expect("the answer");
            assert_eq!(parts.performance, Some("kept"), "{parts:?}");
            assert_eq!(parts.archive, None, "no archive read: {parts:?}");
            assert_eq!(parts.cost, None, "{parts:?}");
            std::fs::remove_dir_all(&dir).expect("clean up");
        });
    }

    /// Objective 084, through the real store: `/api/core-lines`' answer
    /// with no ranges file, a broken one and one with ranges, read at each
    /// request.
    #[test]
    fn the_core_lines_answer_reads_its_ranges_file_at_each_request() {
        use serde_json::Value;
        in_test_schema(|_owner, store| {
            let dir = std::env::temp_dir().join(format!(
                "spawn-core-lines-serve-{}-{}",
                std::process::id(),
                super::now_ms()
            ));
            std::fs::create_dir_all(&dir).expect("a directory");
            let file = dir.join("core-lines.json");
            let nightly = dir.join("nightly");
            let read = || {
                super::core_lines::core_lines_at(store, Some(&file), Some(&nightly))
                    .expect("the answer")
            };
            let figure = |view: &Value, key: &str| {
                view["lines"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|line| {
                        std::iter::once(line["north_star"].clone())
                            .chain(line["guardrails"].as_array().cloned().unwrap_or_default())
                    })
                    // The owner's set (objective 176): the alert figures and
                    // the diagnostics are figures too.
                    .chain(view["alert_figures"].as_array().cloned().unwrap_or_default())
                    .chain(view["diagnostics"].as_array().cloned().unwrap_or_default())
                    .find(|figure| figure["key"] == key)
                    .unwrap_or_else(|| panic!("no {key}: {view}"))
            };
            // No file: no ranges, no error.
            let none = read();
            assert_eq!(none["ranges_file"], json!(file.display().to_string()));
            assert_eq!(none["ranges_error"], Value::Null);
            assert_eq!(none["ignored_ranges"], json!([]));
            assert_eq!(figure(&none, "storage.free_bytes")["range"], Value::Null);
            assert_eq!(
                figure(&none, "performance.report_to_live")["state"],
                json!("not_collected")
            );
            // Objective 087: no backup record and no nightly directory, no
            // recoverability figure.
            for key in [
                "recoverability.cloud_dump_age",
                "recoverability.cloud_bundle_age",
                "recoverability.nightly_age",
                "recoverability.nightly_red",
            ] {
                assert_eq!(figure(&none, key)["value"], Value::Null, "{key}");
            }
            // A file: its ranges, and the entries it cannot use listed.
            std::fs::write(
                &file,
                r#"{"ranges": {"storage.leaked_test_schemas": {"min": 0, "max": 1000000000000},
                    "storage.free_bites": {"min": 1},
                    "recoverability.restores_passed": {"min": 6},
                    "storage.free_bytes": {"max": "many"},
                    "storage.database_ratio": {"max": 10}}}"#,
            )
            .expect("the file");
            let ranged = read();
            assert_eq!(ranged["ranges_error"], Value::Null);
            assert_eq!(
                ranged["ignored_ranges"],
                json!([{"key": "storage.database_ratio",
                        "reason": super::core_lines::RETIRED_REASON},
                       {"key": "storage.free_bites", "reason": "unknown figure"},
                       // Free disk takes no range at all (objective 182).
                       {"key": "storage.free_bytes",
                        "reason": "an alert of its own (disk_low, objective 204)"}])
            );
            let ratio = figure(&ranged, "storage.leaked_test_schemas");
            assert_eq!(
                ratio["range"],
                json!({"min": 0, "max": 1_000_000_000_000_i64})
            );
            // The test servers' schemas are counted; with none read, none.
            let expected = if ratio["value"].is_null() {
                "not_collected"
            } else {
                "in"
            };
            assert_eq!(ratio["state"], json!(expected), "{ratio}");
            let restores = figure(&ranged, "recoverability.restores_passed");
            assert_eq!(restores["range"], json!({"min": 6}));
            assert_eq!(restores["state"], json!("not_collected"));
            // A broken file: no ranges, and why.
            std::fs::write(&file, "{\"ranges\": ").expect("the file");
            let broken = read();
            assert!(
                broken["ranges_error"]
                    .as_str()
                    .is_some_and(|why| !why.is_empty())
            );
            assert_eq!(broken["ignored_ranges"], json!([]));
            assert_eq!(
                figure(&broken, "storage.leaked_test_schemas")["range"],
                Value::Null
            );
            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    /// Task 545 (reviews 532/533 of 515): a reading through the real store
    /// in a test (the serve and Core lines readings reach
    /// [`super::storage::report`], as does the alerts monitor's guardrail
    /// reading) looks for build directories under the tests' path, never
    /// under the host's `/tmp` or `SPAWN_ALERT_TMPFS_PATH`. A look there
    /// stops the test itself (`storage::tmpfs_build_targets`); this one
    /// fails when a reading's look holds a watched host path.
    #[test]
    fn a_reading_in_a_test_never_looks_at_the_hosts_tmpfs() {
        in_test_schema(|_owner, store| {
            let storage = super::storage::report(store, super::now_ms()).expect("the figures");
            let looked = storage["tmpfs_build_targets"]["root"]
                .as_str()
                .map(std::path::PathBuf::from)
                .expect("the tmpfs looked under");
            assert_eq!(looked, std::path::Path::new(super::storage::TESTS_TMPFS));
            for watched in super::storage::watched_host_paths() {
                assert!(
                    !watched.starts_with(&looked),
                    "a test's reading looked under {}, which holds the host's {}",
                    looked.display(),
                    watched.display()
                );
            }
            assert_eq!(
                storage["tmpfs_build_targets"]["targets"],
                serde_json::json!([])
            );
        });
    }

    /// Objective 064, through the real store: each line for the owner
    /// carries the tasks it names with their states now, and only a review
    /// ask whose tasks have all ended is settled.
    #[test]
    fn the_owners_lines_carry_their_tasks_states_and_whether_they_are_settled() {
        in_test_schema(|owner, store| {
            let mission = store
                .mission_add("064 owner work", &json!([]), "human")
                .expect("a mission");
            let task = |brief: &str| {
                store
                    .task_assign(mission, "ephemeral", "w1", brief, "default")
                    .expect("a task")
            };
            let (a, b, c, d) = (task("a"), task("b"), task("c"), task("d"));
            let mut client =
                postgres::Client::connect(owner, postgres::NoTls).expect("the owner connects");
            let mut record = |value: serde_json::Value| {
                client
                    .query_one("select spawn_control_record($1)", &[&value])
                    .expect("a record");
            };
            for id in [a, b, d] {
                record(json!({"type": "task.reported", "id": id, "commit": "c"}));
            }
            for id in [a, b] {
                record(json!({"type": "task.reviewed", "id": id, "accepted": true}));
            }
            store.task_cancel(c).expect("cancelled");
            let lines: Vec<(String, Option<i64>)> = [
                format!("reports {a}, {b} and {c} are still waiting for your review."),
                format!("task #{d} waits for your review."),
                format!("task {a} landed; deploying it needs spawn init-db."),
                format!("mission #{mission} has no backlog yet; task 999999 neither."),
            ]
            .into_iter()
            .map(|text| (text, Some(5)))
            .collect();
            let mut warnings = Vec::new();
            let notes = super::attention::owner_notes(store, &lines, 2, &mut warnings);
            assert!(warnings.is_empty(), "{warnings:?}");
            let shape = |index: usize| json!({"tasks": notes[index]["tasks"], "settled": notes[index]["settled"]});
            assert_eq!(
                shape(0),
                json!({"tasks": [{"id": a, "state": "accepted"}, {"id": b, "state": "accepted"},
                                 {"id": c, "state": "cancelled"}],
                       "settled": true})
            );
            assert_eq!(
                shape(1),
                json!({"tasks": [{"id": d, "state": "reported"}], "settled": false})
            );
            assert_eq!(
                shape(2),
                json!({"tasks": [{"id": a, "state": "accepted"}], "settled": false}),
                "an ask other than a review stands"
            );
            assert_eq!(shape(3), json!({"tasks": [], "settled": false}));
            assert_eq!(notes[0]["text"], json!(lines[0].0));
            assert_eq!(notes[0]["at_ms"], json!(5));
            let current: Vec<&serde_json::Value> =
                (0..4).map(|index| &notes[index]["current"]).collect();
            assert_eq!(
                current,
                [&json!(true), &json!(true), &json!(false), &json!(false)]
            );
        });
    }

    #[test]
    fn a_cost_task_row_carries_its_label_as_the_missions_board_gives_it() {
        in_test_schema(|_owner, store| {
            let supply = store
                .mission_add(
                    "Supply PA: give every provider an AI business agent.",
                    &json!(["/x/supply.git"]),
                    "human",
                )
                .expect("a mission");
            let other = store
                .mission_add("Work on the docs", &json!([]), "human")
                .expect("a mission");
            let extras = store
                .task_assign(
                    supply,
                    "ephemeral",
                    "w1",
                    "Implement extras in Supply PA: docs/protocol.md, sections Extras",
                    "default",
                )
                .expect("a task");
            let docs = store
                .task_assign(
                    other,
                    "ephemeral",
                    "w2",
                    "Goal: rewrite the guide.",
                    "default",
                )
                .expect("a task");
            let mut board = store.mission_board(50).expect("the board");
            super::labels::label_board(&mut board);
            let board_label = |id: i64| {
                board["missions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|mission| mission["tasks"].as_array().into_iter().flatten())
                    .find(|task| task["id"] == id)
                    .map_or_else(
                        || panic!("no task {id} on the board: {board}"),
                        |task| task["label"].clone(),
                    )
            };
            let mut answer = json!({"tasks": [
                {"task": extras, "usd": 1.0},
                {"task": docs, "usd": 0.5},
                {"task": 999_999, "usd": 0.1},
            ]});
            super::label_cost_tasks(store, &mut answer).expect("labelled");
            let label = |index: usize| answer["tasks"][index]["label"].clone();
            assert!(label(0).is_string(), "{answer}");
            assert_eq!(label(0), board_label(extras), "{answer}");
            assert_eq!(label(1), board_label(docs), "{answer}");
            assert_eq!(label(2), serde_json::Value::Null, "not in the record");
            // No tasks: nothing to label, and no error.
            let mut empty = json!({"tasks": []});
            super::label_cost_tasks(store, &mut empty).expect("labelled");
            assert_eq!(empty, json!({"tasks": []}));
        });
    }

    /// One model call of `worker`'s execution 1, timed `at_ms`, of `input`
    /// tokens on `model-x` (the test's price: $3 a million input tokens).
    fn spend(
        store: &spawn_postgres::PostgresStore,
        agent: &str,
        session: &str,
        id: &str,
        at_ms: i64,
        input: u64,
    ) {
        use spawn_core::event::{EventKind, NewEvent, Source};
        use spawn_core::id::{AgentId, CommandId, ExecutionId, SessionId};
        use spawn_core::port::EventLog;
        store
            .append(NewEvent {
                agent_id: AgentId::new(agent).expect("valid agent"),
                session_id: SessionId::new(session).expect("valid session"),
                execution_id: Some(ExecutionId::for_command(CommandId(1))),
                source: Source::Claude,
                source_event_id: Some(format!("usage:{id}")),
                kind: EventKind::ModelUsage {
                    message_id: id.to_string(),
                    model: "model-x".to_string(),
                    at_ms,
                    input_tokens: input,
                    output_tokens: 0,
                    cache_write_tokens: 0,
                    cache_read_tokens: 0,
                },
            })
            .expect("append");
    }

    /// A worker's task of `mission` and its execution 1 started: the
    /// worker's agent and session.
    fn started_worker(store: &spawn_postgres::PostgresStore, mission: i64) -> (String, String) {
        use spawn_core::event::{ENGINE, EventKind, ModelProfile, NewEvent, Source};
        use spawn_core::id::{AgentId, CommandId, ExecutionId, SessionId, WorkspaceId};
        use spawn_core::port::EventLog;
        let task = store
            .task_assign(mission, "ephemeral", "w1", "brief", "default")
            .expect("a task");
        let (agent, session) = (
            format!("agent_worker_w1_{task}"),
            format!("session_worker_w1_{task}"),
        );
        store
            .append(NewEvent {
                agent_id: AgentId::new(&agent).expect("valid agent"),
                session_id: SessionId::new(&session).expect("valid session"),
                execution_id: Some(ExecutionId::for_command(CommandId(1))),
                source: Source::Claude,
                source_event_id: Some("start-1".to_string()),
                kind: EventKind::ExecutionStarted {
                    engine: ENGINE.to_string(),
                    workspace: WorkspaceId::new("1:/workspaces/1").expect("valid"),
                    command_id: CommandId(1),
                    fork: None,
                    model: ModelProfile::Default,
                    transcript_from: None,
                    launch: None,
                },
            })
            .expect("started");
        (agent, session)
    }

    /// GitHub #64, through the real store: `/api/cost`'s window also says
    /// the window of equal length before it, its trend by the hour (24h) or
    /// the day (7d), and each budgeted mission's spend of the UTC day.
    #[test]
    fn a_cost_window_says_the_window_before_its_trend_and_the_budgets() {
        const HOUR: i64 = 3_600_000;
        const DAY: i64 = 24 * HOUR;
        in_test_schema(|_owner, store| {
            let mission = store
                .mission_add("Budgeted work", &json!([]), "human")
                .expect("a mission");
            store
                .mission_configure(
                    mission,
                    None,
                    "review",
                    Some(&json!({"daily_usd": 2.5})),
                    None,
                )
                .expect("a daily budget");
            let (agent, session) = started_worker(store, mission);
            let now = store.database_now_ms().expect("now");
            // Today (UTC) $3.00; last night, just before the UTC day and
            // inside the last 24 h, $0.60 (not today's budget); 30 h ago
            // $1.50 (the 24h window's previous one); 10 days ago $0.30 (the
            // week's previous window).
            let day_start = now - now.rem_euclid(DAY);
            let calls = [
                ("today", (now - 1000).max(day_start), 1_000_000, 3.0),
                ("last_night", day_start - 1, 200_000, 0.6),
                ("yesterday", now - 30 * HOUR, 500_000, 1.5),
                ("ancient", now - 10 * DAY, 100_000, 0.3),
            ];
            for (id, at_ms, input, _usd) in calls {
                spend(store, &agent, &session, id, at_ms, input);
            }
            let prices = json!({"model-x": {"input": 3, "output": 15, "cache_write": 3.75, "cache_read": 0.3}});
            assert_eq!(store.cost_record(&prices).expect("record"), 4);
            let usd = |value: &serde_json::Value| value.as_f64().unwrap_or(f64::NAN);
            let near = |value: &serde_json::Value, expected: f64, what: &str| {
                assert!(
                    (usd(value) - expected).abs() < 1e-9,
                    "{what}: {value}, expected {expected}"
                );
            };
            for (name, length_ms, steps, step_ms, before) in
                [("24h", DAY, 24, HOUR, 1.5), ("7d", 7 * DAY, 7, DAY, 0.3)]
            {
                let answer = crate::cost::window(store, name, length_ms).expect("the window");
                let since = answer["since_ms"].as_i64().expect("since");
                let previous = &answer["previous"];
                assert_eq!(
                    previous["since_ms"],
                    json!(since - length_ms),
                    "{name}: {answer}"
                );
                assert_eq!(previous["until_ms"], json!(since), "{name}: {answer}");
                near(
                    &previous["total"]["usd"],
                    before,
                    &format!("{name}: the window before"),
                );
                assert_eq!(previous["landed"], json!(0), "{name}: {answer}");
                assert_eq!(answer["trend_step_ms"], json!(step_ms), "{name}: {answer}");
                let trend = answer["trend"].as_array().expect("a trend");
                assert_eq!(trend.len(), steps, "{name}: {answer}");
                for (index, step) in (0_i64..).zip(trend) {
                    assert_eq!(
                        step["since_ms"],
                        json!(since + index * step_ms),
                        "{name}: {answer}"
                    );
                }
                // Each call in the step its time falls in, nothing elsewhere.
                let until = answer["until_ms"].as_i64().expect("until");
                let mut expected = vec![0.0; steps];
                for (_id, at_ms, _input, usd) in calls {
                    if at_ms >= since && at_ms < until {
                        expected
                            [usize::try_from((at_ms - since) / step_ms).expect("in the window")] +=
                            usd;
                    }
                }
                for (index, (step, usd)) in trend.iter().zip(expected).enumerate() {
                    near(&step["usd"], usd, &format!("{name}: step {index}"));
                }
                let budgets = answer["budgets"].as_array().expect("budgets");
                assert_eq!(budgets.len(), 1, "{name}: {answer}");
                assert_eq!(budgets[0]["mission"], json!(mission), "{name}: {answer}");
                near(
                    &budgets[0]["budget_usd"],
                    2.5,
                    &format!("{name}: the budget"),
                );
                near(
                    &budgets[0]["spent_usd"],
                    3.0,
                    &format!("{name}: today's spend alone, from the UTC day's start"),
                );
                // The other sessions' part: there, apart, and empty here.
                assert_eq!(answer["sessions"]["rows"], json!([]), "{name}: {answer}");
                near(
                    &answer["sessions"]["total"]["usd"],
                    0.0,
                    &format!("{name}: sessions"),
                );
            }
        });
    }

    #[test]
    fn a_cost_mission_row_carries_its_name_as_the_missions_board_gives_it() {
        in_test_schema(|_owner, store| {
            let supply = store
                .mission_add(
                    "Owner work on Supply PA: changes the owner reviews.",
                    &json!(["/x/supply.git"]),
                    "human",
                )
                .expect("a mission");
            let docs = store
                .mission_add("Docs: rewrite the guide.", &json!([]), "human")
                .expect("a mission");
            // A task of the first mission alone: the second is named all
            // the same, with none of its tasks in the answer.
            let task = store
                .task_assign(supply, "ephemeral", "w1", "Goal: one change.", "default")
                .expect("a task");
            let mut board = store.mission_board(50).expect("the board");
            super::labels::label_board(&mut board);
            let board_name = |id: i64| {
                board["missions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|mission| mission["id"] == id)
                    .map_or_else(
                        || panic!("no mission {id} on the board: {board}"),
                        |mission| mission["mission_name"].clone(),
                    )
            };
            let mut answer = json!({
                "missions": [
                    {"mission": supply, "usd": 1.0},
                    {"mission": docs, "usd": 0.5},
                    {"mission": 999_999, "usd": 0.1},
                ],
                "tasks": [{"task": task, "mission": supply, "usd": 1.0}],
            });
            super::label_cost_tasks(store, &mut answer).expect("labelled");
            let name = |index: usize| answer["missions"][index]["name"].clone();
            assert_eq!(name(0), json!("Supply PA · owner work"), "{answer}");
            assert_eq!(name(0), board_name(supply), "{answer}");
            assert_eq!(name(1), board_name(docs), "{answer}");
            assert!(name(1).is_string(), "{answer}");
            assert_eq!(name(2), serde_json::Value::Null, "not in the record");
            assert!(answer["tasks"][0]["label"].is_string(), "{answer}");
        });
    }

    /// The supervisor's session starts an execution, as the Bridge records it.
    fn execution_starts(store: &spawn_postgres::PostgresStore) {
        spawn_core::port::EventLog::append(
            store,
            spawn_core::event::NewEvent {
                agent_id: spawn_core::id::AgentId::new("agent_supervisor").expect("valid agent"),
                session_id: spawn_core::id::SessionId::new("session_supervisor")
                    .expect("valid session"),
                execution_id: Some(
                    spawn_core::id::ExecutionId::new("exec_key").expect("valid execution"),
                ),
                source: spawn_core::event::Source::Bridge,
                source_event_id: Some("exec_key:started".to_string()),
                kind: spawn_core::event::EventKind::ExecutionStarted {
                    engine: spawn_core::event::ENGINE.to_string(),
                    workspace: spawn_core::id::WorkspaceId::new("1:/workspaces/1")
                        .expect("valid workspace"),
                    command_id: spawn_core::id::CommandId(1),
                    fork: None,
                    model: spawn_core::event::ModelProfile::Default,
                    transcript_from: None,
                    launch: None,
                },
            },
        )
        .expect("an execution starts");
    }

    /// The accepted case, end to end through the handler into the test
    /// database: the probe writes nothing; an input to a session that runs
    /// no execution is refused 409 with the reason and records nothing
    /// (objective 040); once it runs, an input is the command `spawn submit
    /// --principal human` makes, and a retried POST types once.
    #[test]
    fn a_persons_input_reaches_the_test_database_once() {
        in_test_schema(|owner, store| {
            let commands = || {
                store
                    .commands_json("session_supervisor", 50, 120_000)
                    .expect("commands")
            };
            let to = |token, open, headers: &str, path: &str, body: &str| {
                post(token, open, Some(owner.to_string()), path, headers, body)
            };
            let probe = to(Some("secret"), "", SCRIPT, INPUT, r#"{"probe":true}"#);
            assert_eq!(probe.status, 200, "{probe:?}");
            assert_eq!(probe.body["writable"], true);
            assert_eq!(probe.body["max_text_bytes"], MAX_INPUT_TEXT);
            assert_eq!(commands(), json!([]), "a probe writes nothing");

            // Nothing runs yet: refused with the reason, nothing recorded.
            let stopped = to(
                Some("secret"),
                "",
                SCRIPT,
                INPUT,
                r#"{"text":"anyone there?","key":"page-0"}"#,
            );
            assert_eq!(stopped.status, 409, "{stopped:?}");
            assert_eq!(
                stopped.body["error"],
                "session_supervisor runs no execution: an input needs a running engine; \
                 resume the session"
            );
            assert_eq!(commands(), json!([]), "a refused input records nothing");
            execution_starts(store);

            let body = r#"{"text":"Stop objective_87 and report.\nThanks","key":"page-1"}"#;
            let first = to(Some("secret"), "", SCRIPT, INPUT, body);
            assert_eq!(first.status, 202, "{first:?}");
            assert_eq!(first.body["principal"], "human");
            assert_eq!(first.body["session_id"], "session_supervisor");
            let id = first.body["command_id"].as_u64().expect("a command");
            let again = to(Some("secret"), "", SCRIPT, INPUT, body);
            assert_eq!(again.status, 202);
            assert_eq!(again.body["command_id"], id, "a retried POST types once");
            let listed = commands();
            assert_eq!(listed.as_array().map(Vec::len), Some(1), "{listed}");
            assert_eq!(listed[0]["command_id"], id);
            assert_eq!(listed[0]["principal"], "human");
            assert_eq!(listed[0]["action"], "input");
            assert_eq!(listed[0]["text"], "Stop objective_87 and report.\nThanks");
            assert_eq!(listed[0]["state"], "pending");
            assert_eq!(listed[0]["waiting"], false);

            // The key is that message's: other text under it is refused,
            // not answered with the first command (GitHub #16 item 8).
            let other = to(
                Some("secret"),
                "",
                SCRIPT,
                INPUT,
                r#"{"text":"Something else","key":"page-1"}"#,
            );
            assert_eq!(other.status, 409, "{other:?}");
            assert_eq!(
                other.body["error"],
                "idempotency key 'page-1' was already used for another input or key: send \
                 this one with a new key"
            );
            assert_eq!(commands().as_array().map(Vec::len), Some(1));

            // The key in a header, from the page of an open network.
            let page_with_key = format!("{PAGE}Idempotency-Key: page-2\r\n");
            let header = to(
                Some("secret"),
                "127.0.0.0/8",
                &page_with_key,
                INPUT,
                r#"{"text":"second"}"#,
            );
            assert_eq!(header.status, 202, "{header:?}");
            assert_ne!(header.body["command_id"], id);
            if owner_known() {
                let local = to(None, "", PAGE, INPUT, r#"{"text":"third","key":"page-3"}"#);
                assert_eq!(
                    local.status, 202,
                    "no token set, the owner's client: {local:?}"
                );
            }
            let unknown = to(
                Some("secret"),
                "",
                SCRIPT,
                "/api/session/session_never_seen/input",
                r#"{"text":"hello","key":"page-4"}"#,
            );
            assert_eq!(unknown.status, 404, "{unknown:?}");
            let listed = commands();
            let listed = listed.as_array().expect("a list");
            assert!(listed.iter().all(|command| command["principal"] == "human"));
            assert_eq!(listed.len(), if owner_known() { 3 } else { 2 });
        });
    }

    /// A person's key end to end through the handler into the test
    /// database: refused 409 with the reason while the session runs no
    /// execution (and nothing recorded), then the key command `spawn submit
    /// --principal human` makes, once for a retried POST; 404 for a session
    /// no agent has used.
    #[test]
    fn a_persons_key_reaches_the_test_database_once_and_only_into_a_running_engine() {
        in_test_schema(|owner, store| {
            let commands = || {
                store
                    .commands_json("session_supervisor", 50, 120_000)
                    .expect("commands")
            };
            let to = |path: &str, body: &str| {
                post(
                    Some("secret"),
                    "",
                    Some(owner.to_string()),
                    path,
                    SCRIPT,
                    body,
                )
            };
            let stopped = to(KEY, r#"{"name":"escape","key":"key-1"}"#);
            assert_eq!(stopped.status, 409, "{stopped:?}");
            assert_eq!(
                stopped.body["error"],
                "session_supervisor runs no execution: a key needs a running engine; resume \
                 the session"
            );
            assert_eq!(commands(), json!([]), "a refused key records nothing");

            execution_starts(store);
            let body = r#"{"name":"down","key":"key-2"}"#;
            let first = to(KEY, body);
            assert_eq!(first.status, 202, "{first:?}");
            assert_eq!(first.body["principal"], "human");
            assert_eq!(first.body["key"], "down");
            let id = first.body["command_id"].as_u64().expect("a command");
            let again = to(KEY, body);
            assert_eq!(
                again.body["command_id"], id,
                "a retried POST makes one command"
            );
            let listed = commands();
            assert_eq!(listed.as_array().map(Vec::len), Some(1), "{listed}");
            assert_eq!(listed[0]["action"], "key");
            assert_eq!(listed[0]["text"], "down");
            assert_eq!(listed[0]["principal"], "human");
            let unknown = to(
                "/api/session/session_never_seen/key",
                r#"{"name":"up","key":"key-3"}"#,
            );
            assert_eq!(unknown.status, 404, "{unknown:?}");
        });
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
            after: Vec::new(),
            owner_judged: false,
            model: None,
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
        let one_shot = objective_json(&objective(1, false), &tasks, None);
        assert_eq!(one_shot["status"], "solved");
        let recurring = objective_json(&objective(2, true), &tasks, None);
        assert_eq!(recurring["status"], "met");
        assert_eq!(recurring["current"], 1021.0);
        assert_eq!(recurring["tasks"], json!([13, 12, 11]));
        let landings = recurring["landings"].as_array().expect("landings");
        assert_eq!(landings.len(), 2);
        // Oldest first: the chart reads them in order.
        assert_eq!(landings[0]["task"], 11);
        assert_eq!(landings[0]["baseline"], 1573.0);
        assert_eq!(landings[1]["candidate"], 1021.0);
        let untouched = objective_json(&objective(3, false), &tasks, None);
        assert_eq!(untouched["status"], "open");
        assert_eq!(untouched["landings"], json!([]));
    }

    /// An owner-judged objective is shown as the owner's, and is never what
    /// the mission assigns next.
    #[test]
    fn an_owner_judged_objective_is_the_owners_on_the_board() {
        let mut owner = objective(1, false);
        owner.owner_judged = true;
        // The rows as the board builds them, and then through a real
        // backlog directory below.
        let owner_row = objective_json(&owner, &[], None);
        let fleet_row = objective_json(&objective(2, false), &[], None);
        assert_eq!(
            objective_json(&objective(1, false), &[], None)["status"],
            "open"
        );
        assert_eq!(owner_row["status"], "owner");
        assert_eq!(owner_row["owner_judged"], true);
        assert_eq!(fleet_row["status"], "open");
        assert_eq!(fleet_row["owner_judged"], false);
        // `next_objective` is the first "open" row: the owner's is not it.
        let rows = [owner_row, fleet_row];
        let next = rows
            .iter()
            .find(|row| row["status"] == "open")
            .map(|row| row["id"].clone());
        assert_eq!(next, Some(json!(2)));
        // Through the mission's own backlog: the owner's objective is shown
        // as the owner's and is not what the mission assigns next.
        let dir = std::env::temp_dir().join(format!("spawn-board-owner-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("029.json"),
            r#"{"id": 29, "title": "the owner's", "prompt": "p", "owner_judged": true}"#,
        )
        .expect("write");
        std::fs::write(
            dir.join("030.json"),
            r#"{"id": 30, "title": "the fleet's", "prompt": "p"}"#,
        )
        .expect("write");
        let board = with_objectives(json!({
            "id": 4,
            "objectives": {"backlog": dir.to_str()},
            "tasks": [],
        }));
        assert_eq!(board["backlog_error"], json!(null));
        assert_eq!(board["next_objective"], json!(30), "not the owner's");
        let rows = board["objectives"].as_array().expect("rows");
        assert_eq!(rows[0]["id"], json!(29));
        assert_eq!(rows[0]["status"], "owner");
        assert_eq!(rows[0]["owner_judged"], json!(true));
        assert_eq!(rows[1]["status"], "open");
        assert_eq!(rows[1]["owner_judged"], json!(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A backlog directory that has been moved or deleted is named, not
    /// silently read as an empty backlog: the feature map must not render
    /// four empty columns as "no capabilities" without saying why.
    #[test]
    fn a_backlog_directory_that_is_gone_is_named_on_the_board() {
        let missing = std::env::temp_dir().join(format!(
            "spawn-board-gone-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&missing);
        let board = with_objectives(json!({
            "id": 4,
            "gate": "/g",
            "objectives": {"backlog": missing.display().to_string()},
            "tasks": [],
        }));
        let error = board["backlog_error"].as_str().expect("named");
        assert!(
            error.starts_with(&format!("read {}: ", missing.display())),
            "{error}"
        );
        assert_eq!(board["objectives"], json!([]));
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
        let recurring = objective_json(&objective(2, true), &tasks, None);
        assert_eq!(recurring["status"], "open");
        assert_eq!(recurring["current"], 1200.0);
    }

    /// A backlog of `ids` in a fresh directory, each objective's JSON
    /// extended by `extra` (its id's entry).
    fn backlog_dir(name: &str, ids: &[u32], extra: &[(u32, &str)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("spawn-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        for id in ids {
            let more = extra
                .iter()
                .find(|(of, _)| of == id)
                .map_or("", |(_, more)| *more);
            std::fs::write(
                dir.join(format!("{id:03}.json")),
                format!(r#"{{"id": {id}, "title": "o{id}", "prompt": "p"{more}}}"#),
            )
            .expect("write");
        }
        dir
    }

    /// An objective the integrator recorded solved on the base reads as
    /// solved and names its record; `next_objective` is the integrator's
    /// own choice: not solved, not in flight, yields behind the others.
    #[test]
    fn an_objective_solved_on_the_base_is_solved_and_next_is_the_integrators_choice() {
        let dir = backlog_dir("board-solved", &[1, 2, 3, 4, 5], &[]);
        let board = with_objectives(json!({
            "id": 1,
            "gate": "/g",
            "objectives": {"backlog": dir.to_str()},
            "tasks": [
                task(4, 4, "cancelled", 40, None),
                task(3, 3, "reported", 30, None),
                task(2, 2, "cancelled", 20, None),
                task(1, 1, "landed", 10, None),
            ],
            "solved_on_base": [{"event": 77, "objective": 2, "task": 2, "base": "b0", "at_ms": 5}],
            "outcomes": [
                {"type": "task.gated", "task": 4, "objective": 4, "verdict": "rejected"},
                {"type": "task.gated", "task": 4, "objective": 4, "verdict": "rejected"},
                {"type": "task.gated", "task": 4, "objective": 4, "verdict": "rejected"},
                {"type": "task.cancelled", "task": 4, "objective": 4, "yielded": true},
            ],
        }));
        let rows = board["objectives"].as_array().expect("rows");
        let statuses: Vec<&serde_json::Value> = rows.iter().map(|row| &row["status"]).collect();
        assert_eq!(statuses, ["solved", "solved", "open", "open", "open"]);
        assert_eq!(
            rows[1]["solved_on_base"],
            json!({"event": 77, "task": 2, "base": "b0", "at_ms": 5})
        );
        for at in [0, 2, 3, 4] {
            assert_eq!(
                rows[at]["solved_on_base"],
                serde_json::Value::Null,
                "row {at}"
            );
        }
        // Its landings are a landing's only; the record is not one.
        assert_eq!(rows[1]["landings"], json!([]));
        assert_eq!(board["next_objective"], json!(5));
        // The inputs are taken out of the row.
        assert!(board.get("outcomes").is_none());
        assert!(board.get("solved_on_base").is_none());
        let summary = rsi_summary(&board);
        assert_eq!(summary["objectives"]["open"], 3);
        assert_eq!(summary["objectives"]["solved"], 2);
        assert_eq!(summary["next_objective"], 5);

        // Without objective 5, the yielded 4 is next; with 4 in flight too,
        // nothing is.
        let dir4 = backlog_dir("board-solved-4", &[1, 2, 3, 4], &[]);
        let mut mission = json!({
            "id": 1, "gate": "/g", "objectives": {"backlog": dir4.to_str()},
            "tasks": [task(1, 1, "landed", 10, None)],
            "solved_on_base": [{"event": 77, "objective": 2, "task": 2, "base": "b0"}],
        });
        mission["tasks"]
            .as_array_mut()
            .expect("tasks")
            .push(task(3, 3, "delivered", 30, None));
        assert_eq!(with_objectives(mission.clone())["next_objective"], json!(4));
        mission["tasks"]
            .as_array_mut()
            .expect("tasks")
            .push(task(5, 4, "accepted", 50, None));
        assert_eq!(
            with_objectives(mission)["next_objective"],
            serde_json::Value::Null
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir4);
    }

    /// The row's `solved_on_base` carries `at_ms`, the time of the
    /// integrator's `objective.solved` record, which the RSI page's
    /// Landings card places the objective at (web/src/rsiProgress.ts).
    #[test]
    fn solved_on_base_carries_the_records_time() {
        let dir = backlog_dir("board-solved-at", &[1, 2], &[]);
        let board = with_objectives(json!({
            "id": 1, "gate": "/g", "objectives": {"backlog": dir.to_str()},
            "tasks": [task(1, 1, "landed", 10, None)],
            "solved_on_base": [{"event": 9, "objective": 2, "task": 7, "base": "b",
                                "at_ms": 1_793_724_858_211_i64}],
        }));
        let row = &board["objectives"][1];
        assert_eq!(row["status"], "solved");
        assert_eq!(row["solved_on_base"]["at_ms"], json!(1_793_724_858_211_i64));
        assert_eq!(row["solved_on_base"]["task"], json!(7));
        assert_eq!(
            board["objectives"][0]["solved_on_base"],
            serde_json::Value::Null
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `after` is obeyed against records solved on the base; a held
    /// objective is not next until the mission is configured again.
    #[test]
    fn next_objective_obeys_order_holds_and_only_its_own_records() {
        let dir = backlog_dir("board-order", &[1, 2, 3], &[(1, r#", "after": [2]"#)]);
        // Objective 2 open: 1 waits for it, so 2 is next.
        let open = with_objectives(json!({
            "id": 1, "gate": "/g", "objectives": {"backlog": dir.to_str()}, "tasks": [],
        }));
        assert_eq!(open["next_objective"], json!(2));
        // Objective 2 solved on the base: 1 may go.
        let solved = with_objectives(json!({
            "id": 1, "gate": "/g", "objectives": {"backlog": dir.to_str()}, "tasks": [],
            "solved_on_base": [{"event": 9, "objective": 2, "task": 7, "base": "b"}],
        }));
        assert_eq!(solved["objectives"][1]["status"], "solved");
        assert_eq!(solved["next_objective"], json!(1));
        // Objective 1 held after a second overrun, not configured since:
        // 3 is next; a newer configure ends the hold.
        let held = json!({
            "id": 1, "gate": "/g", "objectives": {"backlog": dir.to_str()}, "tasks": [],
            "configured_ms": 100,
            "solved_on_base": [{"event": 9, "objective": 2, "task": 7, "base": "b"}],
            "outcomes": [{"type": "task.over_budget", "task": 8, "objective": 1,
                          "chain": true, "at_ms": 200}],
        });
        assert_eq!(with_objectives(held.clone())["next_objective"], json!(3));
        let mut configured = held;
        configured["configured_ms"] = json!(300);
        assert_eq!(with_objectives(configured)["next_objective"], json!(1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Objective 210: an objective retired by hand reads as `retired`
    /// whatever its kind (a recurring one with no value too), its row
    /// carries the mission's `retired` row of it, every other row carries
    /// null, and `next_objective` never names it.
    #[test]
    fn a_retired_objective_is_retired_and_never_next() {
        let dir = backlog_dir(
            "board-retired",
            &[1, 2, 3],
            &[(
                2,
                r#", "recurring": true, "metric": "m_ms", "target": 1, "direction": "lower""#,
            )],
        );
        let retired = json!([
            {"objective": 1, "by": "lead", "reason": "renumbered", "caller": "owner", "at_ms": 5},
            {"objective": 2, "by": "lead", "reason": "parked", "caller": "spawn_supervisor",
             "at_ms": 6},
            {"objective": 9, "by": "lead", "reason": "gone", "caller": "owner", "at_ms": 7},
        ]);
        let board = with_objectives(json!({
            "id": 1, "gate": "/g", "objectives": {"backlog": dir.to_str()}, "tasks": [],
            "retired": retired,
        }));
        assert_eq!(board["objectives"][0]["status"], "retired");
        assert_eq!(board["objectives"][0]["retired"], retired[0]);
        assert_eq!(board["objectives"][1]["status"], "retired", "recurring");
        assert_eq!(
            board["objectives"][1]["retired"]["caller"],
            "spawn_supervisor"
        );
        assert_eq!(board["objectives"][2]["status"], "open");
        assert!(board["objectives"][2]["retired"].is_null());
        assert_eq!(board["retired"], retired, "the mission keeps its list");
        assert_eq!(board["next_objective"], json!(3));
        // With 3 in flight, nothing is next.
        let busy = with_objectives(json!({
            "id": 1, "gate": "/g", "objectives": {"backlog": dir.to_str()},
            "tasks": [task(4, 3, "delivered", 10, None)], "retired": retired,
        }));
        assert_eq!(busy["next_objective"], serde_json::Value::Null);
        let _ = std::fs::remove_dir_all(&dir);
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

    /// Objective 211: a delivered task carries its activity; every other
    /// task, and a delivered one the read has none for, carries null.
    #[test]
    fn a_delivered_task_carries_its_activity_and_every_other_null() {
        let mut board = json!({"missions": [{"tasks": [
            {"id": 1, "state": "delivered"},
            {"id": 2, "state": "reported"},
            {"id": 3, "state": "delivered"},
        ]}]});
        let found = json!({"since_ms": 10, "at_ms": 20, "event": "tool.completed",
                           "tool_calls": 2});
        with_activity(
            &mut board,
            &json!({"1": found, "2": {"since_ms": 1, "at_ms": null, "event": null,
                                      "tool_calls": 0}}),
        );
        let tasks = &board["missions"][0]["tasks"];
        assert_eq!(tasks[0]["activity"], found);
        assert!(tasks[1]["activity"].is_null(), "not delivered: null");
        assert!(tasks[2]["activity"].is_null(), "no activity read: null");
        assert!(
            tasks[2]
                .as_object()
                .expect("a task")
                .contains_key("activity"),
            "the key is always there"
        );
    }

    #[test]
    fn fleet_liveness_distinguishes_active_quiet_stalled_idle_blocked_and_offline() {
        let now = 1_000_000_000;
        let min = 60_000;
        let mut agents = json!([
            {
                "session_id": "s_active",
                "live": true,
                "state": "WORKING",
                "judged_at_ms": now - 1000,
                "state_since_ms": now - 10 * min,
                "activity": { "last_event_at_ms": now - 2 * min }
            },
            {
                "session_id": "s_quiet",
                "live": true,
                "state": "WORKING",
                "judged_at_ms": now - 500,
                "state_since_ms": now - 20 * min,
                "activity": { "last_event_at_ms": null, "turn_at_ms": now - 7 * min }
            },
            {
                "session_id": "s_stalled",
                "live": true,
                "state": "WORKING",
                "judged_at_ms": now - 100,
                "state_since_ms": now - 16 * min,
                "activity": {}
            },
            {
                "session_id": "s_waiting",
                "live": true,
                "state": "WAITING",
                "state_since_ms": now - 25 * min,
                "activity": { "last_event_at_ms": now - 25 * min }
            },
            {
                "session_id": "s_blocked",
                "live": true,
                "state": "BLOCKED",
                "state_since_ms": now - 30 * min,
                "activity": { "last_event_at_ms": now - 30 * min }
            },
            {
                "session_id": "s_offline",
                "live": false,
                "state": "COMPLETED",
                "state_since_ms": now - 40 * min,
                "activity": { "last_event_at_ms": now - 40 * min }
            },
            {
                "session_id": "s_future_skew",
                "live": true,
                "state": "WORKING",
                "activity": { "last_event_at_ms": now + 5_000 }
            }
        ]);

        with_fleet_liveness(&mut agents, now);
        let rows = agents.as_array().expect("agents array");
        assert_eq!(rows[0]["liveness_state"], "active_heartbeat");
        assert_eq!(rows[0]["last_event_at_ms"], now - 2 * min);
        assert_eq!(rows[0]["silence_ms"], 2 * min);

        assert_eq!(rows[1]["liveness_state"], "quiet_working");
        assert_eq!(rows[1]["last_event_at_ms"], now - 7 * min);
        assert_eq!(rows[1]["silence_ms"], 7 * min);

        // Falls back to state_since_ms rather than judged_at_ms.
        assert_eq!(rows[2]["liveness_state"], "loop_stalled");
        assert_eq!(rows[2]["last_event_at_ms"], now - 16 * min);
        assert_eq!(rows[2]["silence_ms"], 16 * min);

        assert_eq!(rows[3]["liveness_state"], "waiting_idle");
        assert_eq!(rows[4]["liveness_state"], "blocked");
        assert_eq!(rows[5]["liveness_state"], "offline");
        assert!(rows[5]["silence_ms"].is_null());

        assert_eq!(rows[6]["liveness_state"], "active_heartbeat");
        assert_eq!(rows[6]["silence_ms"], 0);
    }

    #[test]
    fn fleet_liveness_caps_waiting_for_background_job_at_quiet_working() {
        let now = 1_000_000_000;
        let min = 60_000;
        let mut agents = json!([
            {
                "session_id": "s_bg_active",
                "live": true,
                "state": "WORKING",
                "judgement": { "reason": "WAITING_FOR_BACKGROUND_JOB" },
                "activity": {
                    "last_event_at_ms": now - 2 * min,
                    "jobs": [{ "id": "job-1" }]
                }
            },
            {
                "session_id": "s_bg_quiet",
                "live": true,
                "state": "WORKING",
                "judgement": { "reason": "WAITING_FOR_BACKGROUND_JOB" },
                "activity": {
                    "last_event_at_ms": now - 7 * min,
                    "jobs": [{ "id": "job-2" }]
                }
            },
            {
                "session_id": "s_bg_long_wait",
                "live": true,
                "state": "WORKING",
                "judgement": { "reason": "WAITING_FOR_BACKGROUND_JOB" },
                "activity": {
                    "last_event_at_ms": now - 25 * min,
                    "jobs": [{ "id": "job-3" }]
                }
            },
            {
                "session_id": "s_bg_null_ts",
                "live": true,
                "state": "WORKING",
                "judgement": { "reason": "WAITING_FOR_BACKGROUND_JOB" },
                "activity": {
                    "jobs": [{ "id": "job-4" }]
                }
            },
            {
                "session_id": "s_bg_empty_jobs_stalled",
                "live": true,
                "state": "WORKING",
                "judgement": { "reason": "WAITING_FOR_BACKGROUND_JOB" },
                "activity": {
                    "last_event_at_ms": now - 25 * min,
                    "jobs": []
                }
            },
            {
                "session_id": "s_non_bg_stalled",
                "live": true,
                "state": "WORKING",
                "judgement": { "reason": "MODEL_SILENT" },
                "activity": {
                    "last_event_at_ms": now - 25 * min,
                    "jobs": [{ "id": "job-5" }]
                }
            }
        ]);

        with_fleet_liveness(&mut agents, now);
        let rows = agents.as_array().expect("agents array");

        assert_eq!(rows[0]["liveness_state"], "active_heartbeat");
        assert_eq!(rows[0]["last_event_at_ms"], now - 2 * min);
        assert_eq!(rows[0]["silence_ms"], 2 * min);

        assert_eq!(rows[1]["liveness_state"], "quiet_working");
        assert_eq!(rows[1]["last_event_at_ms"], now - 7 * min);
        assert_eq!(rows[1]["silence_ms"], 7 * min);

        assert_eq!(rows[2]["liveness_state"], "quiet_working");
        assert_eq!(rows[2]["last_event_at_ms"], now - 25 * min);
        assert_eq!(rows[2]["silence_ms"], 25 * min);

        assert_eq!(rows[3]["liveness_state"], "quiet_working");
        assert!(rows[3]["last_event_at_ms"].is_null());
        assert!(rows[3]["silence_ms"].is_null());

        assert_eq!(rows[4]["liveness_state"], "loop_stalled");
        assert_eq!(rows[4]["last_event_at_ms"], now - 25 * min);
        assert_eq!(rows[4]["silence_ms"], 25 * min);

        assert_eq!(rows[5]["liveness_state"], "loop_stalled");
        assert_eq!(rows[5]["last_event_at_ms"], now - 25 * min);
        assert_eq!(rows[5]["silence_ms"], 25 * min);
    }

    #[test]
    fn task_liveness_sets_generated_at_ms_and_distinguishes_task_states() {
        let now = 2_000_000_000;
        let min = 60_000;
        let mut board = json!({
            "missions": [
                {
                    "id": 1,
                    "gate": "/repo/rsi/scenarios",
                    "tasks": [
                        {
                            "id": 10,
                            "state": "delivered",
                            "updated_at_ms": now - 3 * min,
                            "activity": { "since_ms": now - 5 * min, "at_ms": now - 3 * min, "tool_calls": 4 }
                        },
                        {
                            "id": 11,
                            "state": "delivered",
                            "updated_at_ms": now - 2 * min,
                            "activity": { "since_ms": now - 2 * min, "at_ms": null, "tool_calls": 0 }
                        },
                        {
                            "id": 12,
                            "state": "delivered",
                            "updated_at_ms": now - 12 * min,
                            "activity": { "since_ms": now - 18 * min, "at_ms": now - 12 * min, "tool_calls": 7 }
                        },
                        {
                            "id": 13,
                            "state": "delivered",
                            "updated_at_ms": now - 22 * min,
                            "activity": { "since_ms": now - 25 * min, "at_ms": now - 22 * min, "tool_calls": 1 }
                        },
                        {
                            "id": 14,
                            "state": "reported",
                            "stages": [{ "stage": "reported", "at_ms": now - min }]
                        },
                        {
                            "id": 15,
                            "state": "accepted",
                            "updated_at_ms": now - min
                        },
                        {
                            "id": 16,
                            "state": "landed",
                            "updated_at_ms": now - min
                        }
                    ]
                },
                {
                    "id": 2,
                    "gate": null,
                    "tasks": [
                        {
                            "id": 20,
                            "state": "accepted",
                            "updated_at_ms": now - min
                        }
                    ]
                }
            ]
        });

        with_task_liveness(&mut board, now);
        assert_eq!(board["generated_at_ms"], now);

        let gated = &board["missions"][0]["tasks"];
        assert_eq!(gated[0]["liveness_state"], "active_working");
        assert_eq!(gated[0]["last_event_at_ms"], now - 3 * min);
        assert_eq!(gated[0]["silence_ms"], 3 * min);

        assert_eq!(gated[1]["liveness_state"], "briefed_quiet");
        assert_eq!(gated[1]["last_event_at_ms"], now - 2 * min);
        assert_eq!(gated[1]["silence_ms"], 2 * min);

        assert_eq!(gated[2]["liveness_state"], "briefed_quiet");
        assert_eq!(gated[2]["silence_ms"], 12 * min);

        assert_eq!(gated[3]["liveness_state"], "stalled");
        assert_eq!(gated[3]["silence_ms"], 22 * min);

        assert_eq!(gated[4]["liveness_state"], "waiting_review");
        assert_eq!(gated[4]["last_event_at_ms"], now - min);
        assert!(gated[4]["silence_ms"].is_null());

        assert_eq!(gated[5]["liveness_state"], "waiting_review");
        assert_eq!(gated[6]["liveness_state"], "terminal");

        let ungated = &board["missions"][1]["tasks"];
        assert_eq!(ungated[0]["liveness_state"], "terminal");
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

    /// Audit L11: a token the browser could never send as a header is
    /// refused at start, the same rule as the UI's (web/src/token.ts).
    #[test]
    fn serve_refuses_a_token_no_browser_can_send() {
        for good in [
            "t",
            "bb-serve-token-1",
            &"a".repeat(512),
            "!~#%&*+-./:=?@^_|",
        ] {
            assert!(
                access_token("0.0.0.0:8787", Some(good.to_string())).is_ok(),
                "{good}"
            );
        }
        for bad in [
            "",
            "a b",
            "a\nb",
            "a\rb",
            "a\u{0}b",
            "a\tb",
            "令牌",
            "café",
            &"a".repeat(513),
        ] {
            let refused =
                access_token("127.0.0.1:8787", Some(bad.to_string())).expect_err("not a token");
            assert!(refused.contains("visible ASCII"), "{bad:?}: {refused}");
        }
    }

    #[test]
    fn serve_token_prefers_keep_credential_over_env_and_fails_closed() {
        use std::os::unix::fs::PermissionsExt;

        let dir = crate::storage::tests::scratch("serve-token-keep");
        assert_eq!(
            super::serve_token_with(None, Some("env-tok-1".to_string())).unwrap(),
            Some("env-tok-1".to_string())
        );
        assert_eq!(
            super::serve_token_with(Some(&dir), Some("env-tok-2".to_string())).unwrap(),
            Some("env-tok-2".to_string())
        );

        let keep_tok = "keep-tok-9941";
        let path = dir.join("spawn-serve.token");
        std::fs::write(&path, format!("{keep_tok}\n")).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).expect("mode");
        assert_eq!(
            super::serve_token_with(Some(&dir), Some("env-tok-ignored".to_string())).unwrap(),
            Some(keep_tok.to_string())
        );

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("bad mode");
        let err = super::serve_token_with(Some(&dir), Some("env-tok-fallback-forbidden".into()))
            .expect_err("bad mode must fail closed");
        assert!(err.starts_with("credential spawn-serve.token"));
        assert!(!err.contains(keep_tok));

        let _ = std::fs::remove_dir_all(&dir);
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
        status_with(None, "", path, headers)
    }

    /// Status of one GET of `path` from `serve`'s request handler with
    /// `token` set (or none) and `open` as `SPAWN_SERVE_OPEN_NETWORKS`,
    /// sending `headers` as [`tokenless_status`] does.
    fn status_with(token: Option<&'static str>, open: &str, path: &str, headers: &str) -> u16 {
        exchange(
            token,
            open,
            None,
            &format!("GET {path} HTTP/1.1\r\n{headers}Connection: close\r\n\r\n"),
        )
        .status
    }

    /// One answer of `serve`'s request handler.
    #[derive(Debug)]
    struct Answer {
        status: u16,
        /// The status line and headers.
        head: String,
        body: serde_json::Value,
    }

    /// One raw exchange with `serve`'s request handler, `token` set (or
    /// none), `open` as `SPAWN_SERVE_OPEN_NETWORKS` and `owner` as
    /// `SPAWN_DATABASE_URL`: `raw` is sent as it is, `PORT` replaced by the
    /// server's port.
    fn exchange(
        token: Option<&'static str>,
        open: &str,
        owner: Option<String>,
        raw: &str,
    ) -> Answer {
        use std::io::{Read, Write};
        let server = tiny_http::Server::http("127.0.0.1:0").expect("listen");
        let port = server.server_addr().to_ip().expect("ip").port();
        let shared = Shared {
            web: std::env::temp_dir(),
            token: token.map(str::to_string),
            port: Some(port),
            hosts: server_hosts("127.0.0.1:0", port),
            open: open_networks(Some(open)).expect("networks"),
            pools: Shared::pools_from_owner(owner),
        };
        let worker = std::thread::spawn(move || {
            let request = server.recv().expect("request");
            handle(request, &shared).expect("answered");
        });
        let mut client = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
        client
            .write_all(raw.replace("PORT", &port.to_string()).as_bytes())
            .expect("send");
        let mut answer = String::new();
        client.read_to_string(&mut answer).expect("read");
        worker.join().expect("handler");
        let (head, body) = answer.split_once("\r\n\r\n").unwrap_or((&answer, ""));
        Answer {
            status: head
                .split_whitespace()
                .nth(1)
                .and_then(|code| code.parse().ok())
                .expect("status line"),
            head: head.to_string(),
            body: serde_json::from_str(body).unwrap_or(serde_json::Value::Null),
        }
    }

    /// A POST of `body` to `path`, sending `headers` (each ending in CRLF,
    /// `PORT` replaced) and its length, to a server set up as [`exchange`]
    /// says.
    fn post(
        token: Option<&'static str>,
        open: &str,
        owner: Option<String>,
        path: &str,
        headers: &str,
        body: &str,
    ) -> Answer {
        exchange(
            token,
            open,
            owner,
            &format!(
                "POST {path} HTTP/1.1\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ),
        )
    }

    /// The headers of the page's own POST: its Host, its Origin, JSON.
    const PAGE: &str = "Host: 127.0.0.1:PORT\r\nOrigin: http://127.0.0.1:PORT\r\n\
                        Content-Type: application/json\r\n";

    /// The headers of a script with the token: no Origin.
    const SCRIPT: &str = "Host: 127.0.0.1:PORT\r\nAuthorization: Bearer secret\r\n\
                          Content-Type: application/json\r\n";

    const INPUT: &str = "/api/session/session_supervisor/input";

    /// Whether this host can tell the owner's sockets (`/proc`): with no
    /// token set, only the owner's client types.
    fn owner_known() -> bool {
        owner_uid().is_some()
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
            open: Vec::new(),
            pools: Ok(None),
        });
        let slots = Slots::new(limit);
        let streams = Slots::new(stream::SSE_SLOTS);
        std::thread::spawn(move || serve_requests(&server, &shared, &slots, &streams));
        (port, web)
    }

    /// A listener that stops (one failed accept ends tiny_http's accept
    /// thread) is listened on again, each stop said; when it cannot be,
    /// every try, serve gives up with an error instead of `Ok(0)`.
    #[test]
    fn a_stopped_listener_is_listened_on_again_then_given_up_with_an_error() {
        let mut served = Vec::new();
        let mut listens = 0;
        let outcome = keep_listening(
            1u32,
            "127.0.0.1:9",
            |listener: &u32| {
                served.push(*listener);
                std::io::Error::other("accept failed")
            },
            |_address: &str| {
                listens += 1;
                if listens == 1 {
                    Ok(2u32)
                } else {
                    Err("address in use".to_string())
                }
            },
            |_stops| std::time::Duration::ZERO,
        );
        assert_eq!(
            served,
            vec![1, 2],
            "the first listener, then the one listened on again"
        );
        assert_eq!(listens, 1 + 5, "one listen that worked, then five tries");
        let error = outcome.expect_err("serve gives up with an error, never Ok(0)");
        assert!(
            error.contains("cannot listen again on 127.0.0.1:9")
                && error.contains("(5 tries)")
                && error.contains("address in use"),
            "{error}"
        );
    }

    #[test]
    fn the_pause_before_listening_again_doubles_from_200_ms_to_at_most_5_s() {
        let millis = |stops| listen_again_pause(stops).as_millis();
        assert_eq!(
            [
                millis(1),
                millis(2),
                millis(3),
                millis(5),
                millis(6),
                millis(40)
            ],
            [200, 400, 800, 3200, 5000, 5000]
        );
    }

    /// The real listener: stopped, it is listened on again on the same
    /// port, and a request there is answered.
    #[test]
    fn a_stopped_tiny_http_listener_answers_again_on_the_same_port() {
        use std::io::{Read, Write};
        let web = std::env::temp_dir().join(format!("spawn-serve-again-{}", std::process::id()));
        std::fs::create_dir_all(&web).expect("web");
        std::fs::write(web.join("index.html"), "<!doctype html>\n").expect("index");
        let server = tiny_http::Server::http("127.0.0.1:0").expect("listen");
        let port = server.server_addr().to_ip().expect("ip").port();
        let shared = Arc::new(Shared {
            web: web.clone(),
            token: None,
            port: Some(port),
            hosts: server_hosts("127.0.0.1:0", port),
            open: Vec::new(),
            pools: Ok(None),
        });
        let address = format!("127.0.0.1:{port}");
        let slots = Slots::new(4);
        let streams = Slots::new(stream::SSE_SLOTS);
        let mut first = true;
        // Asked only once the new listener is bound: a request the stopped
        // one took before it was dropped is answered 500 by tiny_http.
        let (bound, listening) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            keep_listening(
                server,
                &address,
                |server| {
                    if std::mem::take(&mut first) {
                        // As a failed accept ends tiny_http's accept thread.
                        return std::io::Error::other("test: the accept thread stopped");
                    }
                    serve_requests(server, &shared, &slots, &streams)
                },
                |address| {
                    let server =
                        tiny_http::Server::http(address).map_err(|error| error.to_string())?;
                    bound.send(()).ok();
                    Ok(server)
                },
                |_stops| std::time::Duration::from_millis(50),
            )
        });
        listening
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the stopped listener is listened on again within 10 s");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let answer = loop {
            let attempt =
                std::net::TcpStream::connect(("127.0.0.1", port)).and_then(|mut stream| {
                    write!(
                        stream,
                        "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
                    )?;
                    let mut answer = String::new();
                    stream.read_to_string(&mut answer)?;
                    Ok(answer)
                });
            match attempt {
                Ok(answer) if !answer.is_empty() => break answer,
                _ if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                other => panic!("no answer on port {port} after the listener stopped: {other:?}"),
            }
        };
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        std::fs::remove_dir_all(&web).ok();
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

    /// The whole answer to one GET of `path` with `headers` (CRLF-ended).
    fn raw_get(port: u16, path: &str, headers: &str) -> String {
        use std::io::{Read, Write};
        let mut client = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
        write!(
            client,
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{headers}Connection: close\r\n\r\n"
        )
        .expect("send");
        let mut answer = String::new();
        client.read_to_string(&mut answer).expect("read");
        answer
    }

    /// The value of header `name` in a raw answer.
    fn raw_header<'a>(answer: &'a str, name: &str) -> Option<&'a str> {
        answer.split("\r\n\r\n").next()?.lines().find_map(|line| {
            let (field, value) = line.split_once(':')?;
            field.eq_ignore_ascii_case(name).then(|| value.trim())
        })
    }

    /// A file carries its tag; asked again with it, it is not sent again.
    /// The build's assets are kept by the browser for good.
    #[test]
    fn a_file_is_tagged_and_an_unchanged_one_is_not_sent_again() {
        let (port, web) = serving("etag", 4);
        std::fs::write(web.join("index.html"), "<html>spawn</html>").expect("page");
        std::fs::create_dir_all(web.join("assets")).expect("assets");
        std::fs::write(web.join("assets/app-1a2b.js"), "1").expect("asset");
        let first = raw_get(port, "/index.html", "");
        let tag = raw_header(&first, "ETag").expect("a tag").to_string();
        let page_cache = raw_header(&first, "Cache-Control").map(str::to_string);
        let again = raw_get(port, "/index.html", &format!("If-None-Match: {tag}\r\n"));
        let other = raw_get(port, "/index.html", "If-None-Match: W/\"0\"\r\n");
        let asset = raw_get(port, "/assets/app-1a2b.js", "");
        let _ = std::fs::remove_dir_all(&web);
        assert!(first.starts_with("HTTP/1.1 200"), "{first}");
        assert!(first.ends_with("<html>spawn</html>"));
        assert!(tag.starts_with("W/\""), "{tag}");
        assert_eq!(page_cache.as_deref(), Some("no-cache"));
        assert!(again.starts_with("HTTP/1.1 304"), "{again}");
        assert_eq!(raw_header(&again, "ETag"), Some(tag.as_str()));
        assert!(again.ends_with("\r\n\r\n"), "no body: {again}");
        assert!(other.starts_with("HTTP/1.1 200"), "another tag: {other}");
        assert_eq!(
            raw_header(&asset, "Cache-Control"),
            Some("public, max-age=31536000, immutable")
        );
    }

    /// The PWA shell (issue #91 P1b) is the binary's own (serve/pwa.rs):
    /// the manifest as `application/manifest+json`, the service worker as
    /// JavaScript, the icon as SVG, each `no-cache` with a tag that gets
    /// 304, served with no file of the built directory and before one of
    /// the same name. Nothing else is answered by it, nor by the page
    /// (there is no index.html fallback: a missing file is a 404).
    #[test]
    fn the_pwa_shell_is_served_with_its_types_and_never_as_the_page() {
        let (port, web) = serving("pwa", 4);
        std::fs::write(web.join("index.html"), "<html>spawn</html>").expect("page");
        std::fs::write(web.join("sw.js"), "a stale worker").expect("a built worker");
        let manifest = raw_get(port, "/manifest.webmanifest", "");
        let worker = raw_get(port, "/sw.js", "");
        let icon = raw_get(port, "/icon.svg", "");
        let missing = raw_get(port, "/missing.webmanifest", "");
        let nested = raw_get(port, "/assets/sw.js", "");
        let queried = raw_get(port, "/sw.js?v=2", "");
        let tag = raw_header(&worker, "ETag").expect("a tag").to_string();
        let again = raw_get(port, "/sw.js", &format!("If-None-Match: {tag}\r\n"));
        let _ = std::fs::remove_dir_all(&web);
        for (answer, kind, body) in [
            (
                &manifest,
                "application/manifest+json",
                "\"start_url\": \"/\"",
            ),
            (
                &worker,
                "application/javascript; charset=utf-8",
                "addEventListener(\"fetch\"",
            ),
            (&icon, "image/svg+xml", "<svg"),
        ] {
            assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
            assert_eq!(raw_header(answer, "Content-Type"), Some(kind));
            assert_eq!(raw_header(answer, "Cache-Control"), Some("no-cache"));
            assert!(answer.contains(body), "{answer}");
            assert!(
                !answer.contains("<html>spawn</html>"),
                "not the page: {answer}"
            );
        }
        assert!(
            !worker.contains("a stale worker"),
            "the embedded worker wins"
        );
        assert!(queried.contains("addEventListener(\"fetch\""), "{queried}");
        assert!(again.starts_with("HTTP/1.1 304"), "{again}");
        assert_eq!(raw_header(&again, "Cache-Control"), Some("no-cache"));
        for answer in [&missing, &nested] {
            assert!(answer.starts_with("HTTP/1.1 404"), "{answer}");
            assert!(
                !answer.contains("<html>spawn</html>"),
                "not the page: {answer}"
            );
        }
    }

    /// The embedded shell: exactly three paths, the worker never caching
    /// the API, the manifest naming the icon served beside it.
    #[test]
    fn the_embedded_pwa_shell_is_three_files_that_agree() {
        let paths: Vec<&str> = super::pwa::ASSETS.iter().map(|asset| asset.path).collect();
        assert_eq!(paths, ["/sw.js", "/manifest.webmanifest", "/icon.svg"]);
        assert!(super::pwa::asset("/sw.js").is_some());
        assert!(super::pwa::asset("/sw.js/").is_none());
        assert!(super::pwa::asset("/assets/sw.js").is_none());
        let worker = super::pwa::asset("/sw.js").expect("worker").body;
        assert!(worker.contains("url.pathname.startsWith(\"/api/\")"));
        assert!(worker.contains("request.headers.has(\"Authorization\")"));
        let manifest: serde_json::Value = serde_json::from_str(
            super::pwa::asset("/manifest.webmanifest")
                .expect("manifest")
                .body,
        )
        .expect("the manifest is JSON");
        for icon in manifest["icons"].as_array().expect("icons") {
            let src = icon["src"].as_str().expect("src");
            assert!(super::pwa::asset(src).is_some(), "{src} is served");
        }
    }

    /// Each built file's type: a manifest and icons among them.
    #[test]
    fn built_files_are_typed_by_their_names() {
        assert_eq!(
            content_type("manifest.webmanifest"),
            "application/manifest+json"
        );
        assert_eq!(
            content_type("assets/index-1a2b.js"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(content_type("icon.svg"), "image/svg+xml");
        assert_eq!(content_type("icon-192.png"), "image/png");
        assert_eq!(content_type("favicon.ico"), "image/x-icon");
        assert_eq!(content_type("index.html"), "text/html; charset=utf-8");
        assert_eq!(content_type("unknown"), "application/octet-stream");
    }

    /// `/api/features` is a route of its own, behind the same read guard: a
    /// tokenless request naming this server reaches its read (there is no
    /// SPAWN_DATABASE_URL in the test, so 500 says so) rather than a 404. A
    /// mistyped path is still 404.
    #[test]
    fn the_feature_map_route_reaches_its_read() {
        let named = "Host: 127.0.0.1:PORT\r\n";
        let answer = exchange(
            None,
            "",
            None,
            &format!("GET /api/features HTTP/1.1\r\n{named}Connection: close\r\n\r\n"),
        );
        assert_eq!(answer.status, 500, "{answer:?}");
        assert_eq!(answer.body["error"], "SPAWN_DATABASE_URL is not set");
        assert_eq!(tokenless_status("/api/feature", named), 404);
        assert_eq!(tokenless_status("/api/features/", named), 404);
        // The coverage page's project list is a read of its own too.
        assert_eq!(
            tokenless_status("/api/features/coverage/projects", named),
            500
        );
    }

    #[test]
    fn the_feature_coverage_and_scan_routes_enforce_methods_and_guards() {
        let named = "Host: 127.0.0.1:PORT\r\n";
        // GET on POST-only endpoints answers 405.
        assert_eq!(tokenless_status("/api/features/scan", named), 405);
        assert_eq!(tokenless_status("/api/features/associate", named), 405);
        // POST on GET-only coverage endpoint answers 405.
        let post_cov = exchange(
            None,
            "",
            None,
            &format!(
                "POST /api/features/coverage HTTP/1.1\r\n{named}Content-Length: 0\r\nConnection: close\r\n\r\n"
            ),
        );
        assert_eq!(post_cov.status, 405, "{post_cov:?}");
        // POST /api/features/scan without Origin and without token answers 403.
        let scan_no_origin = exchange(
            None,
            "",
            None,
            &format!(
                "POST /api/features/scan HTTP/1.1\r\n{named}Content-Length: 2\r\nConnection: close\r\n\r\n{{}}"
            ),
        );
        assert_eq!(scan_no_origin.status, 403, "{scan_no_origin:?}");
        // POST /api/features/associate with invalid JSON body under token answers 400.
        let bad_assoc = exchange(
            Some("secret-token"),
            "",
            None,
            &format!(
                "POST /api/features/associate HTTP/1.1\r\n{named}Authorization: Bearer secret-token\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
            ),
        );
        assert_eq!(bad_assoc.status, 400, "{bad_assoc:?}");
    }

    /// A deployed controller tree is read-only: an association is refused
    /// with a structured 409 naming why, before anything is attempted, and
    /// the registry is left exactly as it was.
    #[cfg(unix)]
    #[test]
    fn a_feature_association_against_a_read_only_registry_is_refused_with_409() {
        use std::os::unix::fs::PermissionsExt;

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let root = std::env::temp_dir().join(format!(
            "spawn-serve-assoc-read-only-{}-{nanos}",
            std::process::id()
        ));
        let dir = root.join("docs/feature-map");
        std::fs::create_dir_all(root.join("src")).expect("src");
        std::fs::create_dir_all(&dir).expect("registry dir");
        std::fs::write(root.join("src/lib.rs"), "pub fn ok() {}\n").expect("lib.rs");
        std::fs::write(root.join("src/wild.rs"), "pub fn wild() {}\n").expect("wild.rs");
        let registry = dir.join("registry.json");
        let boundary = spawn_core::feature_coverage::BoundaryPolicy::default_spawn_boundary();
        let registry_json = json!({
            "schema_version": spawn_core::feature_coverage::SCHEMA_VERSION,
            "initiative_id": "t559u",
            "boundary": boundary,
            "features": [{
                "id": "domain-core",
                "title": "Core",
                "kind": "domain",
                "summary": "Core files",
                "files": ["src/lib.rs"],
            }],
        });
        let text = serde_json::to_string_pretty(&registry_json).expect("json");
        std::fs::write(&registry, &text).expect("registry");
        crate::feature_inventory::load_registry(&registry).expect("fixture registry parses");

        let body = br#"{"path": "src/wild.rs", "features": ["domain-core"]}"#;
        std::fs::set_permissions(&registry, std::fs::Permissions::from_mode(0o444))
            .expect("chmod file");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).expect("chmod dir");
        let refused = feature_associate_at(&root, &registry, body);
        let unchanged = std::fs::read_to_string(&registry).expect("read back");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("restore");
        std::fs::set_permissions(&registry, std::fs::Permissions::from_mode(0o644))
            .expect("restore file");
        // A malformed body is still a 400, read-only or not.
        let bad = feature_associate_at(&root, &registry, b"{}");
        // Writable again: the same association saves.
        let saved = feature_associate_at(&root, &registry, body);
        let _ = std::fs::remove_dir_all(&root);

        assert!(
            matches!(bad, Err(FeatureAssociateRefusal::Plain((400, _)))),
            "{bad:?}"
        );
        let (status, payload) = saved.expect("a writable registry saves");
        assert_eq!(status, 200);
        assert_eq!(payload["registry_writable"], json!(true));
        assert_eq!(payload["report"]["counts"]["unassociated_wild"], json!(1));
        if refused.is_ok() {
            // Root ignores permission bits: nothing read-only to observe.
            eprintln!("skipping: permission bits are not enforced for this user");
            return;
        }
        let Err(FeatureAssociateRefusal::Structured((status, refusal))) = refused else {
            panic!("expected a structured refusal, got {refused:?}");
        };
        assert_eq!(status, 409);
        assert_eq!(refusal["code"], "registry_read_only");
        assert_eq!(refusal["registry_writable"], json!(false));
        let error = refusal["error"].as_str().expect("error");
        assert!(error.contains("read-only"), "{error}");
        assert!(!error.contains("os error"), "no raw OS error: {error}");
        assert!(
            refusal["read_only_reason"]
                .as_str()
                .is_some_and(|r| !r.is_empty())
        );
        assert_eq!(unchanged, text, "the registry is untouched");
    }

    /// An API answer that failed carries no tag, so it is never "not
    /// modified": a malformed id answers 400 whatever the request names.
    #[test]
    fn an_api_error_is_never_answered_not_modified() {
        let (port, web) = serving("etag-error", 4);
        let answer = raw_get(
            port,
            "/api/session/not%20a%20session",
            "If-None-Match: *\r\n",
        );
        let _ = std::fs::remove_dir_all(&web);
        assert!(answer.starts_with("HTTP/1.1 400"), "{answer}");
        assert_eq!(raw_header(&answer, "ETag"), None);
        assert_eq!(raw_header(&answer, "Cache-Control"), Some("no-cache"));
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
    fn the_log_reads_after_a_position_or_from_a_time_with_a_capped_page() {
        assert_eq!(log_window(""), Ok((LogStart::After(0), DEFAULT_LIMIT)));
        assert_eq!(
            log_window("after=42&limit=10"),
            Ok((LogStart::After(42), 10))
        );
        assert_eq!(
            log_window("since_ms=1700000000000"),
            Ok((LogStart::Since(1_700_000_000_000), DEFAULT_LIMIT))
        );
        assert_eq!(
            log_window("limit=999999999999"),
            Ok((LogStart::After(0), MAX_LIMIT))
        );
        for bad in ["after=-1", "after=x", "limit=ten", "since_ms=1&after=2"] {
            let error = log_window(bad).expect_err(bad);
            assert!(error.starts_with(BAD_REQUEST), "{bad}: {error}");
        }
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
            json!({"open": 1, "solved": 1, "met": 1, "owner_judged": 0})
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

    /// `?mission=<id>` reads that open objective mission's `--features`
    /// registry (relative beside its backlog) and the project root it
    /// belongs to; no such mission is a structured 404, a mission without
    /// a registry a structured 400.
    #[test]
    fn mission_feature_paths_find_any_projects_registry() {
        let missions = json!([
            {"id": 4, "objectives": {"backlog": "/srv/spawn/rsi/backlog",
                                     "features": "/srv/spawn/docs/feature-map/registry.json"}},
            {"id": 9, "objectives": {"backlog": "/srv/acme/backlog",
                                     "features": "../docs/feature-map/registry.json"}},
            {"id": 11, "objectives": {"backlog": "/srv/flake/backlog"}},
        ]);
        let (root, registry) = mission_feature_paths(&missions, 4).expect("spawn");
        assert_eq!(root, PathBuf::from("/srv/spawn"));
        assert_eq!(
            registry,
            PathBuf::from("/srv/spawn/docs/feature-map/registry.json")
        );
        let (root, registry) = mission_feature_paths(&missions, 9).expect("acme");
        // The registry is the path as configured; the root is where it is.
        assert_eq!(
            registry,
            PathBuf::from("/srv/acme/backlog/../docs/feature-map/registry.json")
        );
        assert_eq!(root, PathBuf::from("/srv/acme"));

        let (status, body) = mission_feature_paths(&missions, 11).expect_err("no registry");
        assert_eq!(status, 400);
        assert_eq!(body["code"], "no_features_registry");
        assert_eq!(body["mission"], 11);
        assert!(
            body["error"].as_str().unwrap().starts_with(BAD_REQUEST),
            "{body}"
        );

        let (status, body) = mission_feature_paths(&missions, 12).expect_err("no mission");
        assert_eq!(status, 404);
        assert_eq!(body["code"], "mission_not_found");

        // Without `?mission` the server's own root stays the default.
        let (root, registry) = feature_paths("").expect("default");
        assert_eq!(root, crate::feature_inventory::default_root());
        assert_eq!(
            registry,
            crate::feature_inventory::default_registry_path(&root)
        );
        let (status, body) = feature_paths("mission=abc").expect_err("bad id");
        assert_eq!(status, 400);
        assert_eq!(body["code"], "bad_mission");
    }

    /// A bare `?mission` or an empty `?mission=` is a bad mission (400),
    /// never the default project; a query without it is the default.
    #[test]
    fn a_bare_or_empty_mission_is_a_bad_mission() {
        assert_eq!(mission_query(""), None);
        assert_eq!(mission_query("limit=3&missions=4"), None);
        assert_eq!(mission_query("mission"), Some(""));
        assert_eq!(mission_query("token=x&mission="), Some(""));
        assert_eq!(mission_query("mission=4&limit=3"), Some("4"));
        for query in ["mission", "mission=", "token=x&mission"] {
            let (status, body) = feature_paths(query).expect_err(query);
            assert_eq!(status, 400, "{query}");
            assert_eq!(body["code"], "bad_mission", "{query}");
        }
    }

    /// A mission's `--features` file must be a coverage registry: a legacy
    /// identity-only feature map or a missing file is a structured 400.
    #[test]
    fn a_legacy_feature_map_is_not_a_coverage_registry() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "spawn-serve-coverage-registry-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let legacy = dir.join("legacy.json");
        std::fs::write(
            &legacy,
            r#"{"features": [{"id": "a", "title": "A", "objectives": [1]}]}"#,
        )
        .expect("legacy");
        let registry = dir.join("registry.json");
        std::fs::write(
            &registry,
            r#"{"schema_version": 1, "initiative_id": "x", "features": [],
                "boundary": {"project_name": "Acme", "description": "d",
                             "included_categories": [], "exclusions": []}}"#,
        )
        .expect("registry");
        assert!(coverage_registry_checked(&registry, 9).is_ok());
        for refused in [legacy, dir.join("missing.json")] {
            let (status, body) = coverage_registry_checked(&refused, 9).expect_err("refused");
            assert_eq!(status, 400);
            assert_eq!(body["code"], "not_a_coverage_registry");
            assert_eq!(body["mission"], 9);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A relative registry that walks out of the backlog's repository
    /// (`../../../acme/meta/features.json` from Spawn's backlog) keeps its
    /// configured path (opened and saved as the OS resolves it), and its
    /// root is Acme's directory, not a path through Spawn.
    #[test]
    fn a_relative_registry_is_normalized_out_of_the_backlog() {
        let missions = json!([
            {"id": 9, "objectives": {"backlog": "/srv/spawn/rsi/backlog",
                                     "features": "../../../acme/meta/features.json"}},
        ]);
        let (root, registry) = mission_feature_paths(&missions, 9).expect("acme");
        assert_eq!(
            registry,
            PathBuf::from("/srv/spawn/rsi/backlog/../../../acme/meta/features.json")
        );
        assert_eq!(root, PathBuf::from("/srv/acme/meta"));
    }

    /// The coverage page's projects are every open objective mission with a
    /// coverage registry, however many missions there are: one past
    /// `/api/features`' 50-mission cap is listed; a mission without a
    /// registry, with a legacy map, or with a missing file is not.
    #[test]
    fn coverage_projects_list_every_eligible_mission_past_the_feature_map_cap() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "spawn-serve-coverage-projects-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(dir.join("backlog")).expect("backlog");
        let registry = r#"{"schema_version": 1, "initiative_id": "x", "features": [],
            "boundary": {"project_name": "P", "description": "d",
                         "included_categories": [], "exclusions": []}}"#;
        std::fs::write(dir.join("backlog/registry.json"), registry).expect("registry");
        std::fs::write(
            dir.join("backlog/legacy.json"),
            r#"{"features": [{"id": "a", "title": "A", "objectives": [1]}]}"#,
        )
        .expect("legacy");
        let backlog = dir.join("backlog").display().to_string();
        let missions: Vec<serde_json::Value> = (1..=60)
            .map(|id| {
                let features = match id {
                    3 | 57 => json!("registry.json"),
                    58 => json!("legacy.json"),
                    59 => json!("missing.json"),
                    _ => serde_json::Value::Null,
                };
                json!({"id": id, "text": format!("Mission {id}"),
                       "objectives": {"backlog": backlog, "features": features}})
            })
            .collect();
        let answer = coverage_projects(&serde_json::Value::Array(missions));
        let projects = answer["projects"].as_array().expect("projects");
        let ids: Vec<i64> = projects
            .iter()
            .filter_map(|project| project["mission"].as_i64())
            .collect();
        assert_eq!(ids, [3, 57]);
        assert_eq!(projects[1]["text"], "Mission 57");
        assert_eq!(
            projects[1]["registry_path"],
            dir.join("backlog/registry.json").display().to_string()
        );
        assert!(
            coverage_projects(&json!([]))["projects"]
                .as_array()
                .is_some_and(Vec::is_empty)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `/api/nodes` says how each node is reached, never where.
    #[test]
    fn the_nodes_read_withholds_destinations_endpoints_and_notes() {
        let nodes = public_nodes(json!([
            {"name": "legacy", "ssh": "ckl@legacy", "note": "rack 4", "transport": "ssh",
             "endpoint": null, "via": [], "mode": "direct", "state": "live"},
            {"name": "edge", "ssh": null, "note": null, "transport": "udp",
             "endpoint": "udp://203.0.113.9:7001", "via": ["gw"], "mode": "hole_punch",
             "state": "lost"},
        ]));
        for node in nodes.as_array().expect("a list") {
            for key in ["ssh", "endpoint", "note"] {
                assert!(node.get(key).is_none(), "{key}: {node}");
            }
        }
        assert_eq!(
            json!([
                nodes[1]["transport"],
                nodes[1]["mode"],
                nodes[1]["via"],
                nodes[1]["state"]
            ]),
            json!(["udp", "hole_punch", ["gw"], "lost"])
        );
        assert_eq!(nodes[0]["transport"], "ssh");
    }

    /// Issue #87: a coord node's coordinator URL and public key stay out of
    /// `/api/nodes`; its role, zone and node id stay in.
    #[test]
    fn the_nodes_read_withholds_a_coord_nodes_url_and_public_key() {
        let nodes = public_nodes(json!([
            {"name": "laptop", "ssh": null, "note": "home", "transport": "coord",
             "endpoint": "wss://coord.example/mesh", "via": [], "mode": "relay",
             "state": "live", "node_id": "ab".repeat(32),
             "pubkey": "11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=",
             "role": "peer", "zone": "home"},
        ]));
        let node = &nodes[0];
        for key in ["ssh", "endpoint", "note", "pubkey"] {
            assert!(node.get(key).is_none(), "{key}: {node}");
        }
        assert_eq!(
            json!([
                node["transport"],
                node["mode"],
                node["via"],
                node["state"],
                node["role"],
                node["zone"]
            ]),
            json!(["coord", "relay", [], "live", "peer", "home"])
        );
        assert_eq!(node["node_id"], "ab".repeat(32));
    }

    /// A server running `serve`'s loop with `regular` request slots and
    /// `streams` stream slots, `token` set. Returns its port.
    fn serving_slots(token: Option<&'static str>, regular: usize, streams: usize) -> u16 {
        serving_on(token, Slots::new(regular), Slots::new(streams))
    }

    /// A server with the read guard of `token` on these slots: its port.
    fn serving_on(token: Option<&'static str>, slots: Arc<Slots>, streams: Arc<Slots>) -> u16 {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("listen");
        let port = server.server_addr().to_ip().expect("ip").port();
        let shared = Arc::new(Shared {
            web: std::env::temp_dir(),
            token: token.map(str::to_string),
            port: Some(port),
            hosts: server_hosts("127.0.0.1:0", port),
            open: Vec::new(),
            pools: Ok(None),
        });
        std::thread::spawn(move || serve_requests(&server, &shared, &slots, &streams));
        port
    }

    /// Rev 1: over HTTP, a stream from an address that holds its share of
    /// the stream slots is 503 `too many streams from this address` (not
    /// the all-taken 503), and once one is given back the next reaches its
    /// handler (here the read guard, 401 without the token).
    #[test]
    fn a_stream_past_its_addresss_share_is_503_over_http() {
        use std::net::{IpAddr, Ipv4Addr};
        let streams = Slots::counting_loopback(stream::SSE_SLOTS, 1);
        let port = serving_on(Some("secret"), Slots::new(1), Arc::clone(&streams));
        let held = Slots::take_from(&streams, Some(IpAddr::V4(Ipv4Addr::LOCALHOST)))
            .expect("this address's one stream");
        let (status, body) = raw_status(port, "/api/stream", "Authorization: Bearer secret\r\n");
        assert_eq!(status, 503, "{body}");
        assert!(
            body.contains("too many streams from this address"),
            "{body}"
        );
        assert!(!body.contains("at once"), "{body}");
        drop(held);
        let (status, body) = raw_status(port, "/api/stream", "");
        assert_eq!(status, 401, "the stream reached the read guard: {body}");
    }

    /// One raw GET to `port`: the status and the body.
    fn raw_status(port: u16, path: &str, headers: &str) -> (u16, String) {
        use std::io::{Read, Write};
        let mut client = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .expect("timeout");
        client
            .write_all(
                format!(
                    "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{headers}Connection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .expect("send");
        let mut answer = String::new();
        let _ = client.read_to_string(&mut answer);
        let status = answer
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        (status, answer)
    }

    /// Issue #91: a stream takes one of the stream slots, never one of the
    /// requests' (`MAX_IN_FLIGHT`): with every request slot taken a stream
    /// still reaches its handler (here the read guard, which refuses it
    /// without the token), and with every stream slot taken a stream is
    /// 503 while the other requests are served.
    #[test]
    fn streams_take_their_own_slots_never_the_requests() {
        assert_eq!(stream::SSE_SLOTS, 64);
        assert_eq!(MAX_IN_FLIGHT, 16);
        // Two stream slots: a slot is freed only after its answer is read,
        // so a second stream sent at once may find the first still held.
        let no_requests = serving_slots(Some("secret"), 0, 2);
        let (status, body) = raw_status(no_requests, "/api/overview", "");
        assert_eq!(status, 503, "{body}");
        assert!(body.contains("too many requests"), "{body}");
        let (status, body) = raw_status(no_requests, "/api/stream", "");
        assert_eq!(status, 401, "the stream reached the read guard: {body}");
        let (status, _) = raw_status(no_requests, "/api/stream?after_event_id=3", "");
        assert_eq!(status, 401);
        let no_streams = serving_slots(Some("secret"), 1, 0);
        let (status, body) = raw_status(
            no_streams,
            "/api/stream",
            "Authorization: Bearer secret\r\n",
        );
        assert_eq!(status, 503, "{body}");
        assert!(body.contains("too many streams"), "{body}");
        // Review S1: a bad or doubled cursor is the caller's mistake, said
        // before the stream subscribes.
        // A slot each: one is freed only after its answer is read.
        let one_stream = serving_slots(Some("secret"), 0, 3);
        for path in [
            "/api/stream?after=x",
            "/api/stream?after=-1",
            "/api/stream?after=3&after_event_id=3",
        ] {
            let (status, body) = raw_status(one_stream, path, "Authorization: Bearer secret\r\n");
            assert_eq!(status, 400, "{path}: {body}");
        }
        let (status, body) = raw_status(no_streams, "/api/overview", "");
        assert_eq!(status, 401, "a request is still served: {body}");
    }

    /// Task #574: one client address that is not loopback holds at most
    /// `STREAMS_PER_PEER` stream slots, so it cannot take all 64; its
    /// refusal is its own (`Busy::Peer`), other addresses are served, a
    /// slot given back is its address's again, and loopback (the owner's
    /// own machine: the 64-stream tests) counts only toward the 64.
    #[test]
    fn one_far_address_holds_at_most_its_share_of_the_stream_slots() {
        use super::Busy;
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
        assert_eq!(stream::STREAMS_PER_PEER, 16);
        let streams = Slots::with_per_peer(stream::SSE_SLOTS, Some(stream::STREAMS_PER_PEER));
        let far = IpAddr::V4(Ipv4Addr::new(192, 168, 31, 7));
        let other = IpAddr::V4(Ipv4Addr::new(192, 168, 31, 8));
        let mut held: Vec<_> = (0..stream::STREAMS_PER_PEER)
            .map(|n| Slots::take_from(&streams, Some(far)).unwrap_or_else(|_| panic!("{n}")))
            .collect();
        assert_eq!(
            Slots::take_from(&streams, Some(far)).err(),
            Some(Busy::Peer),
            "the 17th from one address"
        );
        // The same address as an IPv4-mapped IPv6 one is the same address.
        let mapped = IpAddr::V6(Ipv4Addr::new(192, 168, 31, 7).to_ipv6_mapped());
        assert_eq!(
            Slots::take_from(&streams, Some(mapped)).err(),
            Some(Busy::Peer)
        );
        assert_eq!(
            streams.taken.load(std::sync::atomic::Ordering::Acquire),
            stream::STREAMS_PER_PEER,
            "a refusal holds no shared slot"
        );
        let beside = Slots::take_from(&streams, Some(other)).expect("another address");
        held.pop();
        let again = Slots::take_from(&streams, Some(far)).expect("a slot given back");
        // Loopback fills every slot left, uncapped by address.
        let loopback: Vec<_> = (0..stream::SSE_SLOTS - stream::STREAMS_PER_PEER - 1)
            .map(|n| {
                let peer = if n % 2 == 0 {
                    IpAddr::V4(Ipv4Addr::LOCALHOST)
                } else {
                    IpAddr::V6(Ipv6Addr::LOCALHOST)
                };
                Slots::take_from(&streams, Some(peer)).unwrap_or_else(|_| panic!("{n}"))
            })
            .collect();
        assert_eq!(
            Slots::take_from(&streams, Some(IpAddr::V4(Ipv4Addr::LOCALHOST))).err(),
            Some(Busy::All),
            "the 65th stream"
        );
        assert_eq!(
            Slots::take_from(&streams, Some(other)).err(),
            Some(Busy::All)
        );
        drop((held, beside, again, loopback));
        assert_eq!(streams.taken.load(std::sync::atomic::Ordering::Acquire), 0);
        assert!(
            streams
                .peers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "every address's count is given back"
        );
        // The requests' slots count no address.
        let requests = Slots::new(2);
        let _a = Slots::take_from(&requests, Some(far)).expect("one");
        let _b = Slots::take_from(&requests, Some(far)).expect("two");
        assert!(Slots::take(&requests).is_none());
    }

    const MASTER: &str = "/api/master/message";

    /// `POST /api/master/message` has the input's guards in its order, and
    /// its body is a text, a mode and a key, or a probe, nothing else.
    #[test]
    fn the_master_messages_path_has_the_inputs_guards() {
        let answer = exchange(
            Some("secret"),
            "",
            None,
            &format!("GET {MASTER} HTTP/1.1\r\n{SCRIPT}Connection: close\r\n\r\n"),
        );
        assert_eq!(answer.status, 405, "{answer:?}");
        let body = r#"{"text":"hello","key":"m-1"}"#;
        let cross = "Host: 127.0.0.1:PORT\r\nOrigin: http://evil.example\r\n\
                     Content-Type: application/json\r\n";
        assert_eq!(post(None, "", None, MASTER, cross, body).status, 403);
        let plain = "Host: 127.0.0.1:PORT\r\nAuthorization: Bearer secret\r\n\
                     Content-Type: text/plain\r\n";
        assert_eq!(
            post(Some("secret"), "", None, MASTER, plain, body).status,
            415
        );
        let huge = format!(r#"{{"text":"{}","key":"m"}}"#, "x".repeat(MAX_INPUT_BODY));
        assert_eq!(
            post(Some("secret"), "", None, MASTER, SCRIPT, &huge).status,
            413
        );
        for (bad, why) in [
            (r#"{"text":"hi"}"#, "no key"),
            (r#"{"text":"   ","key":"k"}"#, "a blank text"),
            (r#"{"text":"a\u001b[2J","key":"k"}"#, "an escape"),
            (
                r#"{"text":"hi","key":"k","mode":"shout"}"#,
                "an unknown mode",
            ),
            (
                r#"{"text":"hi","key":"k","session":"session_worker_a"}"#,
                "a session",
            ),
            (r#"{"probe":true,"text":"hi"}"#, "a probe with a text"),
        ] {
            let answer = post(Some("secret"), "", None, MASTER, SCRIPT, bad);
            assert_eq!(answer.status, 400, "{why}: {answer:?}");
        }
        let long = format!(
            r#"{{"text":"{}","key":"k"}}"#,
            "x".repeat(MAX_INPUT_TEXT + 1)
        );
        assert_eq!(
            post(Some("secret"), "", None, MASTER, SCRIPT, &long).status,
            400
        );
        let reached = post(Some("secret"), "", None, MASTER, SCRIPT, body);
        assert_eq!(reached.status, 500, "{reached:?}");
        assert_eq!(reached.body["error"], "SPAWN_DATABASE_URL is not set");
    }

    /// Issue #91, through the handler into the test database: a message to
    /// a supervisor that runs no execution wakes it (never 409), a retried
    /// POST makes nothing, every mode is taken, and another request under a
    /// used key is 409.
    #[test]
    fn a_master_message_wakes_a_stopped_supervisor_and_is_typed_once() {
        in_test_schema(|owner, store| {
            let commands = || {
                store
                    .commands_json("session_supervisor", 50, 120_000)
                    .expect("commands")
            };
            let to = |body: &str| {
                post(
                    Some("secret"),
                    "",
                    Some(owner.to_string()),
                    MASTER,
                    SCRIPT,
                    body,
                )
            };
            let probe = to(r#"{"probe":true}"#);
            assert_eq!(probe.status, 200, "{probe:?}");
            assert_eq!(probe.body["session_id"], "session_supervisor");
            assert_eq!(
                probe.body["modes"],
                json!(["auto", "steer", "queue", "restart"])
            );
            assert_eq!(commands(), json!([]), "a probe writes nothing");

            let body = r#"{"text":"Are you there?","key":"mm-1"}"#;
            let woke = to(body);
            assert_eq!(woke.status, 202, "{woke:?}");
            assert_eq!(woke.body["session_id"], "session_supervisor");
            assert_eq!(woke.body["principal"], "human");
            assert_eq!(woke.body["mode"], "auto");
            assert!(woke.body["wake_command_id"].is_i64(), "{woke:?}");
            assert!(woke.body["command_id"].is_i64());
            let again = to(body);
            assert_eq!(again.status, 202);
            assert_eq!(again.body["command_id"], woke.body["command_id"]);
            assert_eq!(again.body["wake_command_id"], woke.body["wake_command_id"]);
            let listed = commands();
            assert_eq!(listed.as_array().map(Vec::len), Some(2), "{listed}");
            // Newest first: the input after its wake.
            assert_eq!(listed[0]["action"], "input");
            assert_eq!(listed[0]["text"], "Are you there?");
            assert_eq!(listed[1]["action"], "start");
            let other = to(r#"{"text":"Something else","key":"mm-1"}"#);
            assert_eq!(other.status, 409, "{other:?}");

            execution_starts(store);
            for (mode, key) in [("queue", "mm-2"), ("steer", "mm-3"), ("restart", "mm-4")] {
                let answer = to(&format!(
                    r#"{{"text":"mode {mode}","key":"{key}","mode":"{mode}"}}"#
                ));
                assert_eq!(answer.status, 202, "{mode}: {answer:?}");
                assert_eq!(answer.body["mode"], mode);
            }
            let header = format!("{SCRIPT}Idempotency-Key: mm-5\r\n");
            let keyed = post(
                Some("secret"),
                "",
                Some(owner.to_string()),
                MASTER,
                &header,
                r#"{"text":"by header"}"#,
            );
            assert_eq!(keyed.status, 202, "{keyed:?}");
            let actions: Vec<String> = commands()
                .as_array()
                .expect("a list")
                .iter()
                .map(|command| command["action"].as_str().unwrap_or("?").to_string())
                .collect();
            // Newest first: by header (woken by restart's start, after its
            // pending stop: no wake of its own); restart's input, its own
            // start (the first start, still in flight, is older than the
            // stop that would end what it begins) and its stop; steer's
            // input and Esc; queue's input; the first input and its start.
            assert_eq!(
                actions,
                [
                    "input", "input", "start", "stop", "input", "key", "input", "input", "start"
                ]
                .map(str::to_string),
                "the newest first"
            );
            let listed = commands();
            let (restart_wake, restart_stop) = (
                listed[2]["command_id"].as_i64().expect("an id"),
                listed[3]["command_id"].as_i64().expect("an id"),
            );
            assert!(
                restart_wake > restart_stop,
                "the wake is newer than the stop"
            );
        });
    }

    /// Issue #91: `POST /api/approvals/<session_id>/act` with `keys` and `text`,
    /// through the handler into the test database: each key its own
    /// command, a digit stored as `digit<n>`; refused 409 while nothing
    /// runs, and nothing recorded.
    #[test]
    fn an_act_presses_keys_and_types_text_into_a_running_engine_once() {
        in_test_schema(|owner, store| {
            const ACT: &str = "/api/approvals/session_supervisor/act";
            let commands = || {
                store
                    .commands_json("session_supervisor", 50, 120_000)
                    .expect("commands")
            };
            let to = |body: &str| {
                post(
                    Some("secret"),
                    "",
                    Some(owner.to_string()),
                    ACT,
                    SCRIPT,
                    body,
                )
            };
            let probe = to(r#"{"probe":true}"#);
            assert_eq!(probe.status, 200, "{probe:?}");
            assert!(
                probe.body["keys"]
                    .as_array()
                    .is_some_and(|keys| keys.contains(&json!("digit1")))
            );
            let stopped = to(r#"{"action":"keys","keys":["2"],"key":"act-1"}"#);
            assert_eq!(stopped.status, 409, "{stopped:?}");
            assert_eq!(commands(), json!([]), "nothing recorded");
            execution_starts(store);
            let body = r#"{"action":"keys","keys":["2","enter"],"key":"act-2"}"#;
            let pressed = to(body);
            assert_eq!(pressed.status, 202, "{pressed:?}");
            assert_eq!(pressed.body["keys"], json!(["digit2", "enter"]));
            assert_eq!(pressed.body["principal"], "human");
            let ids = pressed.body["command_ids"].clone();
            assert_eq!(ids.as_array().map(Vec::len), Some(2));
            assert_eq!(
                to(body).body["command_ids"],
                ids,
                "a retried POST presses once"
            );
            let typed = to(r#"{"action":"text","text":"use main","key":"act-3"}"#);
            assert_eq!(typed.status, 202, "{typed:?}");
            assert_eq!(typed.body["text"], "use main");
            let listed = commands();
            let listed: Vec<(String, String)> = listed
                .as_array()
                .expect("a list")
                .iter()
                .map(|command| {
                    (
                        command["action"].as_str().unwrap_or("?").to_string(),
                        command["text"].as_str().unwrap_or("?").to_string(),
                    )
                })
                .collect();
            assert_eq!(
                listed,
                [("input", "use main"), ("key", "enter"), ("key", "digit2")]
                    .map(|(a, b)| (a.to_string(), b.to_string()))
            );
            let unknown = post(
                Some("secret"),
                "",
                Some(owner.to_string()),
                "/api/approvals/session_never_seen/act",
                SCRIPT,
                r#"{"action":"keys","keys":["y"],"key":"act-4"}"#,
            );
            assert_eq!(unknown.status, 404, "{unknown:?}");
            for (bad, why) in [
                (
                    r#"{"action":"keys","keys":["f1"],"key":"k"}"#,
                    "a key outside the set",
                ),
                (r#"{"action":"approve","key":"k"}"#, "an unknown action"),
                (
                    r#"{"action":"allow","key":"k"}"#,
                    "an answer without its approval_id",
                ),
                (r#"{"keys":["y"],"key":"k"}"#, "no action"),
                (r#"{"action":"keys","keys":["y"]}"#, "no key"),
            ] {
                assert_eq!(to(bad).status, 400, "{why}");
            }
        });
    }

    /// The named-key path takes the digits of a numbered prompt (issue #91).
    #[test]
    fn a_persons_digit_key_reaches_the_test_database() {
        in_test_schema(|owner, store| {
            execution_starts(store);
            let answer = post(
                Some("secret"),
                "",
                Some(owner.to_string()),
                KEY,
                SCRIPT,
                r#"{"name":"3","key":"digit-1"}"#,
            );
            assert_eq!(answer.status, 202, "{answer:?}");
            assert_eq!(answer.body["key"], "digit3");
            let listed = store
                .commands_json("session_supervisor", 50, 120_000)
                .expect("commands");
            assert_eq!(listed[0]["text"], "digit3");
        });
    }

    /// Pairs of strings: (method, path) or (legacy, canonical).
    type Pairs = Vec<(String, String)>;

    /// The canonical routes and legacy aliases of docs/api.md: each table
    /// row `| GET | `<path>` |` or `| POST | `<path>` |`, and each alias row
    /// `| `<legacy>` | `<canonical>` |`.
    fn documented_routes(doc: &str) -> (Pairs, Pairs) {
        let quoted = |line: &str| -> Vec<String> {
            line.split('`')
                .skip(1)
                .step_by(2)
                .map(str::to_string)
                .collect()
        };
        let mut routes = Vec::new();
        let mut aliases = Vec::new();
        for line in doc.lines() {
            for method in ["GET", "POST"] {
                if line.starts_with(&format!("| {method} | `")) {
                    routes.push((method.to_string(), quoted(line)[0].clone()));
                }
            }
            if line.starts_with("| `/api/") {
                let paths = quoted(line);
                aliases.push((paths[0].clone(), paths[1].clone()));
            }
        }
        (routes, aliases)
    }

    #[test]
    fn routes_match_docs_api_md() {
        let doc = include_str!("../../../docs/api.md");
        let (routes, aliases) = documented_routes(doc);
        let table: Vec<(String, String)> = super::ROUTES
            .iter()
            .map(|(method, path)| (method.to_string(), path.to_string()))
            .collect();
        assert_eq!(
            routes, table,
            "docs/api.md's route tables and serve.rs's ROUTES, in the same order"
        );
        let gets = super::ROUTES.iter().filter(|(m, _)| *m == "GET").count();
        let posts = super::ROUTES.iter().filter(|(m, _)| *m == "POST").count();
        assert_eq!((gets, posts), (33, 9));
        assert!(
            doc.contains(&format!(
                "serves {gets} GET route patterns and {posts} POST routes"
            )),
            "docs/api.md states the counts"
        );
        let legacy: Vec<(String, String)> = super::ALIASES
            .iter()
            .map(|(old, new)| (old.to_string(), new.to_string()))
            .collect();
        assert_eq!(
            aliases, legacy,
            "docs/api.md's alias table and serve.rs's ALIASES, in the same order"
        );
    }

    /// Task #574: docs/architecture.md tells the workspace as it is. Every
    /// member of the workspace's Cargo.toml is named there (as `` `name` ``),
    /// the crate count it states is the members' count, `spawn serve` is
    /// said to be a control plane with `docs/api.md`'s POST count, and the
    /// stale claims (five crates, no `LISTEN/NOTIFY`, a read-only serve with
    /// one write, blake3/zstd) never come back.
    #[test]
    fn docs_architecture_md_names_every_workspace_crate_and_no_stale_claim() {
        let doc = include_str!("../../../docs/architecture.md");
        let manifest = include_str!("../../../Cargo.toml");
        let members: Vec<&str> = manifest
            .split_once("members = [")
            .and_then(|(_, rest)| rest.split_once(']'))
            .map(|(list, _)| list)
            .expect("the workspace's members list")
            .split(',')
            .filter_map(|entry| entry.trim().strip_prefix("\"crates/"))
            .filter_map(|entry| entry.strip_suffix('"'))
            .collect();
        assert!(!members.is_empty(), "the workspace's members");
        for member in &members {
            assert!(
                doc.contains(&format!("| `{member}` |")),
                "docs/architecture.md's crate table has no row for {member}"
            );
        }
        assert!(
            doc.contains(&format!("用 {} 个 Rust crate", members.len())),
            "docs/architecture.md states the crate count"
        );
        let posts = super::ROUTES.iter().filter(|(m, _)| *m == "POST").count();
        let gets = super::ROUTES.iter().filter(|(m, _)| *m == "GET").count();
        assert!(
            doc.contains("Thin Web + PWA 控制面"),
            "serve is a control plane"
        );
        // Both places that give serve's route counts (§1 and §7's `serve`
        // row) give ROUTES' counts, and no other count is given.
        let counts = format!("{gets} 个 GET 路由模式和 {posts} 个 POST 路由");
        assert_eq!(
            doc.matches(&counts).count(),
            2,
            "docs/architecture.md states serve's route counts twice as {counts:?}"
        );
        assert_eq!(doc.matches("个 GET 路由模式").count(), 2);
        assert_eq!(doc.matches("个 POST 路由").count(), 2);
        // spawn-host stores no blocks with blake3 or zstd (the backups'
        // zstd is spawn-cli's, and may be said elsewhere).
        let host_row = doc
            .lines()
            .find(|line| line.starts_with("| `spawn-host` |"))
            .expect("spawn-host's row");
        for stale in ["blake3", "zstd"] {
            assert!(!host_row.contains(stale), "spawn-host's row says {stale}");
        }
        assert!(doc.contains("pg_notify('spawn_events'"));
        assert!(doc.contains("deploy/gate-runner.sh"));
        assert!(doc.contains("轻通信多保活 · 事件同步 · 数据库隔离"));
        for stale in [
            "5 个 Rust crate",
            "没有 `LISTEN/NOTIFY`",
            "不是控制面",
            "唯一的写入",
            "只读看板",
            "blake3",
            "全部代码同步阻塞",
        ] {
            assert!(
                !doc.contains(stale),
                "docs/architecture.md says {stale:?} again"
            );
        }
    }

    /// A path that fits `pattern`, each `<name>` filled with `value`.
    fn filled(pattern: &str, value: &str) -> String {
        pattern
            .split('/')
            .map(|segment| {
                if segment.starts_with('<') {
                    value
                } else {
                    segment
                }
            })
            .collect::<Vec<_>>()
            .join("/")
    }

    #[test]
    fn every_route_and_alias_names_one_route_only() {
        for (method, pattern) in super::ROUTES {
            assert!(["GET", "POST"].contains(method), "{pattern}");
            assert!(pattern.starts_with("/api/"), "{pattern}");
            for value in ["7", "session_a", "pane", "task"] {
                let path = filled(pattern, value);
                let matching: Vec<&str> = super::ROUTES
                    .iter()
                    .filter(|(_, other)| super::fit(other, &path).is_some())
                    .map(|(_, other)| *other)
                    .collect();
                assert_eq!(matching, vec![*pattern], "{path}");
                // The one overlap: `/api/missions/task/resume` is the legacy
                // `/api/missions/task/<n>` (as it always was), never mission
                // "task" (a mission id is a number).
                let shadowed = *pattern == "/api/missions/<id>/resume" && value == "task";
                assert_eq!(
                    super::canonical(&path).is_some(),
                    shadowed,
                    "a canonical path is no alias: {path}"
                );
            }
        }
        for (legacy, successor) in super::ALIASES {
            assert!(
                super::ROUTES.iter().any(|(_, route)| route == successor),
                "{legacy} answers as a route: {successor}"
            );
            assert!(
                super::route(legacy).is_none(),
                "a legacy pattern is no canonical route: {legacy}"
            );
            let path = filled(legacy, "7");
            assert_eq!(
                super::canonical(&path),
                Some((filled(successor, "7"), *legacy)),
                "{path}"
            );
        }
    }

    #[test]
    fn a_legacy_path_is_rewritten_to_its_canonical_path() {
        let rewritten = |path: &str| super::canonical(path).map(|(path, _)| path);
        for (legacy, successor) in [
            ("/api/session/session_a", "/api/sessions/session_a"),
            (
                "/api/session/not%20a%20session",
                "/api/sessions/not%20a%20session",
            ),
            (
                "/api/fleet/session_a/commands",
                "/api/sessions/session_a/commands",
            ),
            (
                "/api/fleet/session_a/judgements",
                "/api/sessions/session_a/judgements",
            ),
            ("/api/pane/session_a", "/api/sessions/session_a/pane"),
            (
                "/api/session/session_a/input",
                "/api/sessions/session_a/input",
            ),
            ("/api/session/session_a/key", "/api/sessions/session_a/key"),
            (
                "/api/session/session_a/restart",
                "/api/sessions/session_a/restart",
            ),
            (
                "/api/session/session_a/reassign",
                "/api/sessions/session_a/reassign",
            ),
            ("/api/mission/6/resume", "/api/missions/6/resume"),
            ("/api/missions/task/12", "/api/tasks/12"),
            ("/api/rsi/task/12", "/api/tasks/12/gate"),
            ("/api/rsi", "/api/objectives"),
            ("/api/rsi/objective/3/41", "/api/objectives/3/41"),
            ("/api/rsi/pane", "/api/objectives/pane"),
        ] {
            assert_eq!(rewritten(legacy).as_deref(), Some(successor), "{legacy}");
        }
        for path in [
            "/api/overview",
            "/api/sessions/session_a",
            "/api/session/",
            "/api/fleet/session_a",
            "/api/fleet/session_a/pane",
            "/api/rsi/objective/pane",
            "/api/rsi/",
            "/api/missions",
            "/index.html",
        ] {
            assert_eq!(rewritten(path), None, "{path}");
        }
    }

    /// The successor `Link` header in a response head, if any.
    fn link_of(head: &str) -> Option<String> {
        head.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("link")
                .then(|| value.trim().to_string())
        })
    }

    #[test]
    fn a_legacy_path_answers_as_its_canonical_path_and_links_to_it() {
        let send = |token: Option<&'static str>, method: &str, path: &str| {
            exchange(
                token,
                "",
                None,
                &format!(
                    "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:PORT\r\n\
                     Content-Length: 0\r\nConnection: close\r\n\r\n"
                ),
            )
        };
        let reads = [
            (
                "/api/session/not%20a%20session",
                "/api/sessions/not%20a%20session",
            ),
            (
                "/api/fleet/bad%20id/commands",
                "/api/sessions/bad%20id/commands",
            ),
            (
                "/api/fleet/bad%20id/judgements",
                "/api/sessions/bad%20id/judgements",
            ),
            ("/api/pane/bad%20id", "/api/sessions/bad%20id/pane"),
            ("/api/missions/task/x", "/api/tasks/x"),
            ("/api/rsi/task/x", "/api/tasks/x/gate"),
            ("/api/rsi", "/api/objectives"),
            ("/api/rsi/objective/x/4", "/api/objectives/x/4"),
            ("/api/rsi/pane?mission=x", "/api/objectives/pane?mission=x"),
        ];
        let writes = [
            (
                "/api/session/session_a/input",
                "/api/sessions/session_a/input",
            ),
            ("/api/session/session_a/key", "/api/sessions/session_a/key"),
            (
                "/api/session/session_a/restart",
                "/api/sessions/session_a/restart",
            ),
            (
                "/api/session/session_a/reassign",
                "/api/sessions/session_a/reassign",
            ),
            ("/api/mission/6/resume", "/api/missions/6/resume"),
        ];
        let mut cases = Vec::new();
        for (legacy, successor) in reads {
            // The read itself, a write method (405 GET) and a refusal (401).
            cases.push((None, "GET", legacy, successor));
            cases.push((None, "PUT", legacy, successor));
            cases.push((Some("secret"), "GET", legacy, successor));
        }
        for (legacy, successor) in writes {
            // A write without JSON or Origin, a read method (405 POST) and a
            // refusal (401).
            cases.push((None, "POST", legacy, successor));
            cases.push((None, "GET", legacy, successor));
            cases.push((Some("secret"), "POST", legacy, successor));
        }
        for (token, method, legacy, successor) in cases {
            let old = send(token, method, legacy);
            let new = send(token, method, successor);
            assert_eq!(old.status, new.status, "{method} {legacy}");
            assert_ne!(old.status, 404, "{method} {legacy} is routed");
            if old.status != 200 {
                assert_eq!(old.body, new.body, "{method} {legacy}");
            }
            let path = successor.split('?').next().unwrap_or(successor);
            assert_eq!(
                link_of(&old.head),
                Some(format!("<{path}>; rel=\"successor-version\"")),
                "{method} {legacy}: {}",
                old.head
            );
            assert_eq!(link_of(&new.head), None, "{method} {successor}");
        }
        // The read reached its own handler, not another's.
        let read = |path: &str| send(None, "GET", path);
        let task = read("/api/missions/task/x");
        assert_eq!(task.status, 400);
        assert_eq!(task.body["error"], format!("{BAD_REQUEST}bad task x"));
        let objective = read("/api/rsi/objective/x/4");
        assert_eq!(
            objective.body["error"],
            format!("{BAD_REQUEST}bad objective x/4")
        );
        let pane = read("/api/rsi/pane?mission=x");
        assert_eq!(pane.body["error"], format!("{BAD_REQUEST}bad mission x"));
        // A canonical path under no route is a 404, as before.
        assert_eq!(read("/api/sessions/a/b/c").status, 404);
        assert_eq!(read("/api/session/").status, 404);
    }

    #[test]
    fn the_time_parameters_take_their_aliases() {
        assert_eq!(
            log_window("since=1700000000000"),
            Ok((LogStart::Since(1_700_000_000_000), DEFAULT_LIMIT)),
            "since= is since_ms="
        );
        for bad in ["since=1&since_ms=1", "since=x", "since=1&after=2"] {
            let error = log_window(bad).expect_err(bad);
            assert!(error.starts_with(BAD_REQUEST), "{bad}: {error}");
        }
        // `/api/alerts`: window= is since=; both at once is a 400.
        let since = super::alerts_since;
        assert_eq!(since(""), Ok(None));
        assert_eq!(since("since=1h"), Ok(Some("1h".to_string())));
        assert_eq!(since("window=7d"), Ok(Some("7d".to_string())));
        for bad in ["since=1h&window=1h", "window=7d&since="] {
            let error = since(bad).expect_err(bad);
            assert!(error.starts_with(BAD_REQUEST), "{bad}: {error}");
        }
    }

    #[test]
    fn a_path_unsafe_in_a_header_gets_no_link_and_is_never_logged() {
        for path in [
            "/api/sessions/session_a",
            "/api/objectives/3/41",
            "/api/sessions/a%0Ab",
        ] {
            assert!(super::linkable(path), "{path}");
        }
        for path in [
            "",
            "/api/sessions/x\nX-Injected:1",
            "/api/sessions/x\rX-Injected:1",
            "/api/sessions/x\r\nX-Injected:1",
            "/api/sessions/a\u{1}b",
            "/api/sessions/a\u{7f}b",
            "/api/sessions/a\tb",
            "/api/sessions/a b",
            "/api/sessions/a>;rel=x",
            "/api/sessions/<a",
            "/api/sessions/a\"b",
            "/api/sessions/é",
        ] {
            assert!(!super::linkable(path), "{path:?}");
            let _successor = super::Successor::new(path.to_string());
            assert!(super::successor_link().is_none(), "{path:?}");
        }
        {
            let _successor = super::Successor::new("/api/sessions/session_a".to_string());
            let link = super::successor_link().expect("a safe path links");
            assert_eq!(
                link.value.as_str(),
                "</api/sessions/session_a>; rel=\"successor-version\""
            );
        }
        assert!(super::successor_link().is_none(), "the guard clears it");
        // The log line holds the two patterns and nothing of a request.
        for (legacy, successor) in super::ALIASES {
            assert_eq!(
                super::alias_log_line(legacy),
                format!(
                    "serve: legacy path {legacy} answered as {successor} (logged once; \
                     docs/api.md lists the canonical routes)"
                )
            );
        }
    }

    #[test]
    fn a_hostile_legacy_path_cannot_write_a_header_or_a_log_line() {
        let raw = |token: Option<&'static str>, path: &str| {
            exchange(
                token,
                "",
                None,
                &format!(
                    "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:PORT\r\n\
                     Content-Length: 0\r\nConnection: close\r\n\r\n"
                ),
            )
        };
        for (legacy, successor) in [
            (
                "/api/session/x\nX-Injected:1",
                "/api/sessions/x\nX-Injected:1",
            ),
            (
                "/api/session/x\rX-Injected:1",
                "/api/sessions/x\rX-Injected:1",
            ),
            ("/api/session/a\u{1}b", "/api/sessions/a\u{1}b"),
            ("/api/session/a>;rel=x", "/api/sessions/a>;rel=x"),
            ("/api/session/<a", "/api/sessions/<a"),
            ("/api/session/a\"b", "/api/sessions/a\"b"),
            (
                "/api/pane/x\nX-Injected:1",
                "/api/sessions/x\nX-Injected:1/pane",
            ),
        ] {
            assert!(super::canonical(legacy).is_some(), "{legacy:?} is an alias");
            // Unauthenticated to a token-guarded server, and token-less.
            for token in [Some("secret"), None] {
                let old = raw(token, legacy);
                let new = raw(token, successor);
                assert_eq!(old.status, new.status, "{legacy:?} answers as canonical");
                assert_eq!(old.body, new.body, "{legacy:?}");
                if token.is_some() {
                    assert_eq!(old.status, 401, "{legacy:?}");
                }
                assert_eq!(link_of(&old.head), None, "{legacy:?}: {}", old.head);
                assert!(
                    !old.head.to_ascii_lowercase().contains("x-injected")
                        && !old.head.contains("rel=x"),
                    "{legacy:?}: {}",
                    old.head
                );
                let lines: Vec<&str> = old.head.split("\r\n").collect();
                assert!(
                    lines
                        .iter()
                        .all(|line| !line.contains(['\n', '\r', '\u{1}'])),
                    "{legacy:?}: every header line is whole: {lines:?}"
                );
            }
        }
        // A safe legacy path still links.
        let fine = raw(Some("secret"), "/api/session/session_a");
        assert_eq!(fine.status, 401);
        assert_eq!(
            link_of(&fine.head),
            Some("</api/sessions/session_a>; rel=\"successor-version\"".to_string())
        );
    }

    #[test]
    fn every_post_route_reaches_its_own_handler() {
        let posts: Vec<&str> = super::ROUTES
            .iter()
            .filter(|(method, _)| *method == "POST")
            .map(|(_, pattern)| *pattern)
            .collect();
        assert_eq!(posts.len(), 9);
        for pattern in posts {
            let path = filled(pattern, "7");
            // A read method: the write's own 405, naming POST (a route
            // left to the reads would answer 405 naming GET, or 404).
            let read = exchange(
                None,
                "",
                None,
                &format!(
                    "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:PORT\r\nConnection: close\r\n\r\n"
                ),
            );
            assert_eq!(read.status, 405, "{path}");
            assert!(
                read.head
                    .lines()
                    .any(|line| line.eq_ignore_ascii_case("allow: post")),
                "{path}: {}",
                read.head
            );
            // A POST is taken by it: refused by a write guard, never 404 or
            // the reads' 405.
            let write = post(None, "", None, &path, "Host: 127.0.0.1:PORT\r\n", "{}");
            assert!(
                ![404, 405].contains(&write.status),
                "{path}: {}",
                write.status
            );
        }
    }

    #[test]
    fn ipc_route_table_covers_every_canonical_route() {
        let table: serde_json::Value = serde_json::from_str(include_str!(
            "../../spawn-core/testdata/spawn-ipc-v1/routes.json"
        ))
        .expect("valid spawn-ipc-v1 routes.json");
        let routes = table["routes"].as_array().expect("routes array");
        let mut by_key = std::collections::BTreeMap::new();
        for entry in routes {
            let method = entry["method"].as_str().expect("method");
            let pattern = entry["pattern"].as_str().expect("pattern");
            assert!(
                by_key.insert((method, pattern), entry).is_none(),
                "duplicate route in routes.json: {method} {pattern}"
            );
        }
        assert_eq!(super::ROUTES.len(), 42);
        for &(method, pattern) in super::ROUTES {
            let entry = by_key.get(&(method, pattern)).unwrap_or_else(|| {
                panic!("missing canonical route in routes.json: {method} {pattern}")
            });
            if method == "GET" {
                assert_eq!(entry["pool"], "reads", "{pattern}");
                assert_eq!(entry["pg_role"], "spawn_reader", "{pattern}");
                assert_eq!(entry["idempotency_key"], false, "{pattern}");
            } else {
                assert_eq!(method, "POST", "{pattern}");
                assert_eq!(entry["pool"], "writes", "{pattern}");
                assert_eq!(entry["idempotency_key"], true, "{pattern}");
                let expected_role =
                    if matches!(pattern, "/api/features/scan" | "/api/features/associate") {
                        "none"
                    } else {
                        "spawn_human"
                    };
                assert_eq!(entry["pg_role"], expected_role, "{pattern}");
            }
        }
        let api_count = routes
            .iter()
            .filter(|entry| {
                entry["pattern"]
                    .as_str()
                    .is_some_and(|p| p.starts_with("/api/"))
            })
            .count();
        assert_eq!(
            api_count,
            super::ROUTES.len(),
            "routes.json /api/* count must match serve::ROUTES exactly"
        );
    }

    fn sample_episode(
        task_id: i64,
        mission_id: i64,
        updated_at_ms: i64,
        assigned_seq: i64,
        brief: &str,
    ) -> spawn_core::control::TaskEpisode {
        use spawn_core::control::{EpisodeOutcome, GradeWire, TaskEpisode, TaskKind};
        use spawn_core::mission::TaskState;
        TaskEpisode {
            task_id,
            mission_id,
            kind: TaskKind::Ephemeral,
            agent_id: format!("agent_worker_{task_id}"),
            session_id: format!("session_{task_id}"),
            brief: brief.to_string(),
            created_by: Some("supervisor".to_string()),
            objective: None,
            scenario: None,
            model: "default".to_string(),
            difficulty: GradeWire::L2,
            preset_id: None,
            first_preset_id: None,
            engine: None,
            model_label: None,
            pinned: false,
            reissue_of: None,
            continues: None,
            reassign_of: None,
            state: TaskState::Landed,
            outcome: EpisodeOutcome::Landed,
            rework: 0,
            quality_rejections: 0,
            reroutes: 0,
            restarts: 0,
            steals: 0,
            deliveries: 1,
            reports: 1,
            last_command_id: Some(task_id),
            last_report: Some("done".to_string()),
            last_report_commit: None,
            review_note: None,
            base: None,
            candidate: None,
            metric: None,
            last_gate_verdict: None,
            review_sync_points: Vec::new(),
            gate_verdicts: Vec::new(),
            terminal_by: None,
            terminal_reason: None,
            assigned_seq,
            assigned_at_ms: updated_at_ms - 10,
            delivered_at_ms: Some(updated_at_ms - 5),
            closed_seq: Some(assigned_seq + 1),
            closed_at_ms: Some(updated_at_ms),
            updated_at_ms,
            stages: Vec::new(),
        }
    }

    fn sample_episodes_summary(total: usize) -> serde_json::Value {
        json!({
            "total": total,
            "landed": total,
            "accepted": 0,
            "rejected": 0,
            "cancelled": 0,
            "aborted": 0,
            "over_budget": 0,
            "in_flight": 0,
            "total_rework": 0,
            "total_reroutes": 0,
            "review_sync_points": 0,
            "gate_verdicts": 0,
            "warnings": 0,
        })
    }

    #[test]
    fn episodes_limit_query_parses_defaults_caps_and_rejects_bad_values() {
        assert_eq!(
            super::parse_episodes_limit_query(""),
            Ok(super::EPISODES_DEFAULT_LIMIT)
        );
        assert_eq!(
            super::parse_episodes_limit_query("mission=7"),
            Ok(super::EPISODES_DEFAULT_LIMIT)
        );
        assert_eq!(super::parse_episodes_limit_query("limit=5"), Ok(5));
        assert_eq!(
            super::parse_episodes_limit_query("mission=4&limit=5"),
            Ok(5)
        );
        assert_eq!(super::parse_episodes_limit_query("limit=0"), Ok(0));
        assert_eq!(super::parse_episodes_limit_query("limit=200"), Ok(200));
        assert_eq!(
            super::parse_episodes_limit_query("limit=500"),
            Ok(super::EPISODES_MAX_LIMIT)
        );
        assert_eq!(
            super::parse_episodes_limit_query("limit=99999999999999999999999"),
            Ok(super::EPISODES_MAX_LIMIT)
        );
        for bad in [
            "limit=",
            "limit",
            "limit=-1",
            "limit=abc",
            "limit=+5",
            "mission=4&limit=ten",
        ] {
            let err = super::parse_episodes_limit_query(bad).expect_err(bad);
            assert!(err.starts_with(BAD_REQUEST), "{bad}: {err}");
        }
    }

    #[test]
    fn bounded_episodes_payload_limits_sorts_newest_first_and_preserves_keys() {
        let episodes = vec![
            sample_episode(1, 4, 100, 10, "t1"),
            sample_episode(5, 4, 300, 11, "t5"),
            sample_episode(2, 4, 200, 15, "t2"),
            sample_episode(4, 4, 200, 20, "t4"),
            sample_episode(3, 4, 200, 20, "t3"),
            sample_episode(6, 4, 50, 1, "t6"),
            sample_episode(7, 4, 400, 1, "t7"),
            sample_episode(8, 4, 350, 2, "t8"),
        ];
        let summary = sample_episodes_summary(episodes.len());
        let payload = super::bounded_episodes_payload(Some(4), episodes, 5, &summary, true, &[]);
        let keys: Vec<&str> = payload
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "drift_count",
                "episodes",
                "mission",
                "parity_ok",
                "summary",
                "warnings",
            ]
        );
        assert_eq!(payload["mission"], 4);
        assert_eq!(payload["summary"]["total"], 8);
        assert_eq!(payload["parity_ok"], true);
        assert_eq!(payload["warnings"], 0);
        assert_eq!(payload["drift_count"], 0);
        let ids: Vec<i64> = payload["episodes"]
            .as_array()
            .expect("episodes array")
            .iter()
            .filter_map(|ep| ep["task_id"].as_i64())
            .collect();
        assert_eq!(ids, [7, 8, 5, 4, 3]);

        // Default limit (50) and hard upper cap (200).
        let many: Vec<_> = (1..=250)
            .map(|i| sample_episode(i, 9, i * 10, i, "brief"))
            .collect();
        let many_summary = sample_episodes_summary(many.len());
        let default_limit = super::parse_episodes_limit_query("").unwrap();
        let default_payload = super::bounded_episodes_payload(
            Some(9),
            many.clone(),
            default_limit,
            &many_summary,
            true,
            &[],
        );
        let default_ids: Vec<i64> = default_payload["episodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|ep| ep["task_id"].as_i64())
            .collect();
        assert_eq!(default_ids.len(), 50);
        assert_eq!(default_ids.first(), Some(&250));
        assert_eq!(default_ids.last(), Some(&201));

        let capped_limit = super::parse_episodes_limit_query("limit=500").unwrap();
        let capped_payload =
            super::bounded_episodes_payload(Some(9), many, capped_limit, &many_summary, true, &[]);
        let capped_ids: Vec<i64> = capped_payload["episodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|ep| ep["task_id"].as_i64())
            .collect();
        assert_eq!(capped_ids.len(), 200);
        assert_eq!(capped_ids.first(), Some(&250));
        assert_eq!(capped_ids.last(), Some(&51));
    }

    #[test]
    fn bounded_episodes_payload_respects_byte_budget() {
        const { assert!(super::EPISODES_MAX_BODY_BYTES < 4_194_304) };

        let large_brief = "x".repeat(10_000);
        let episodes: Vec<_> = (1..=10)
            .map(|i| sample_episode(i, 1, i * 100, i, &large_brief))
            .collect();
        let summary = sample_episodes_summary(episodes.len());
        let payload = super::bounded_episodes_payload_with_cap(
            Some(1),
            episodes,
            10,
            &summary,
            true,
            &[],
            35_000,
        );
        let encoded_len = payload.to_string().len();
        assert!(encoded_len <= 35_000, "encoded_len={encoded_len}");
        let kept = payload["episodes"].as_array().unwrap();
        assert!(!kept.is_empty() && kept.len() < 10, "kept={}", kept.len());
        assert_eq!(kept[0]["task_id"], 10);

        let huge_utf8 = "€".repeat(40_000);
        let mut single = sample_episode(99, 2, 500, 99, &huge_utf8);
        single.last_report = Some(huge_utf8.clone());
        let single_summary = sample_episodes_summary(1);
        let trimmed = super::bounded_episodes_payload_with_cap(
            Some(2),
            vec![single],
            5,
            &single_summary,
            true,
            &[],
            20_000,
        );
        assert!(trimmed.to_string().len() <= 20_000);
        assert_eq!(trimmed["episodes"].as_array().unwrap().len(), 1);
        assert!(trimmed["episodes"][0]["brief"].as_str().unwrap().len() <= 4096);
        assert!(super::truncate_utf8(&huge_utf8, 4096).len() <= 4096);
    }

    fn sample_v1_node(name: &str) -> spawn_postgres::NodeV1 {
        spawn_postgres::NodeV1 {
            name: name.to_string(),
            node_id: Some("a".repeat(64)),
            cell_id: "c0".to_string(),
            status: spawn_postgres::NodeStatus::Active,
            drained: false,
            degraded: false,
            lost_recorded: false,
            host_fault_strikes: 0,
            boot_id: Some("0".repeat(32)),
            beat_seq: 1,
            last_beat_ms: Some(1_000_000),
            spec_sha256: Some("b".repeat(64)),
            spec: None,
            warm_bases: Vec::new(),
            power: None,
            build_sha: None,
            node_version: None,
        }
    }

    #[test]
    fn merge_nodes_with_v1_surfaces_enrolled_v1_nodes_with_telemetry_and_no_secrets() {
        let now = 1_000_000;
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let p1 = json!([
            {
                "name": "legacy-ssh",
                "ssh": "owner@legacy.local",
                "endpoint": null,
                "note": "keep private",
                "pubkey": "secret-pubkey",
                "transport": "ssh",
                "mode": "direct",
                "via": [],
                "state": "live",
                "power": null
            }
        ]);

        let mut n252 = sample_v1_node("n252");
        n252.build_sha = Some(sha.to_string());
        n252.node_version = Some("0.1.0".to_string());
        n252.power = Some(json!({
            "on_ac": true,
            "battery_pct": 100,
            "charging": false,
            "remaining_mins": null
        }));

        let mut nlost = sample_v1_node("nlost");
        nlost.last_beat_ms = Some(now - 200_000);

        let mut nbatt = sample_v1_node("nbatt");
        nbatt.power = Some(json!({
            "on_ac": false,
            "battery_pct": 15,
            "charging": false,
            "remaining_mins": 30
        }));

        let mut ndrained = sample_v1_node("ndrained");
        ndrained.drained = true;

        let mut ndegraded = sample_v1_node("ndegraded");
        ndegraded.degraded = true;
        ndegraded.host_fault_strikes = 3;

        let merged =
            super::merge_nodes_with_v1(p1, &[n252, nlost, nbatt, ndrained, ndegraded], now, 20);
        let rows = merged.as_array().expect("merged array");
        assert_eq!(rows.len(), 6);

        let p1_row = &rows[0];
        assert_eq!(p1_row["name"], "legacy-ssh");
        assert_eq!(p1_row["transport"], "ssh");
        for hidden in ["ssh", "endpoint", "note", "pubkey"] {
            assert!(p1_row.get(hidden).is_none(), "{hidden}");
        }

        let r_n252 = &rows[1];
        assert_eq!(r_n252["name"], "n252");
        assert_eq!(r_n252["proto"], "node-v1");
        assert_eq!(r_n252["status"], "active");
        assert_eq!(r_n252["build_sha"], sha);
        assert_eq!(r_n252["node_version"], "0.1.0");
        assert_eq!(r_n252["state"], "active");
        assert_eq!(r_n252["lost"], false);
        assert_eq!(r_n252["schedulable"], true);
        assert_eq!(r_n252["low_battery_drain"], false);
        assert_eq!(r_n252["effective_drained"], false);
        assert_eq!(r_n252["host_fault_strikes"], 0);
        assert_eq!(r_n252["last_beat_ms"], now);
        assert_eq!(r_n252["power"]["on_ac"], true);
        for hidden in ["ssh", "endpoint", "note", "pubkey"] {
            assert!(r_n252.get(hidden).is_none(), "{hidden}");
        }

        let r_lost = &rows[2];
        assert_eq!(r_lost["state"], "lost");
        assert_eq!(r_lost["lost"], true);
        assert_eq!(r_lost["schedulable"], false);

        let r_batt = &rows[3];
        assert_eq!(r_batt["state"], "active");
        assert_eq!(r_batt["low_battery_drain"], true);
        assert_eq!(r_batt["effective_drained"], true);
        assert_eq!(r_batt["schedulable"], false);

        let r_drained = &rows[4];
        assert_eq!(r_drained["state"], "active,drained");
        assert_eq!(r_drained["effective_drained"], true);
        assert_eq!(r_drained["schedulable"], false);

        let r_degraded = &rows[5];
        assert_eq!(r_degraded["state"], "active,degraded");
        assert_eq!(r_degraded["host_fault_strikes"], 3);
        assert_eq!(r_degraded["schedulable"], false);
    }

    #[test]
    fn storage_tree_and_snapshot_validate_queries_and_ids_before_db() {
        let long_proj = format!("project={}", "a".repeat(257));
        for bad_query in [
            "limit=0",
            "limit=501",
            "limit=abc",
            "limit=",
            "project=%ff",
            long_proj.as_str(),
            "project=bad%00proj",
            "project=bad%0aproj",
            "project=bad%7fproj",
        ] {
            let ans = super::storage_tree_answer(bad_query);
            assert_eq!(ans.status, 400, "{bad_query}: {}", ans.body);
            let err = super::parse_storage_tree_query(bad_query).expect_err(bad_query);
            assert!(err.starts_with(super::BAD_REQUEST), "{bad_query}: {err}");
        }
        assert_eq!(
            super::parse_storage_tree_query("project=my%2Dproj&limit=40").expect("valid query"),
            (Some("my-proj".to_string()), 40)
        );
        for bad_id in ["bad", "sha256:1234", "g".repeat(40).as_str(), "%zz"] {
            let ans = super::storage_snapshot_answer(bad_id);
            assert_eq!(ans.status, 400, "{bad_id}: {}", ans.body);
        }
    }

    #[test]
    fn storage_classify_ref_kind_and_ref_log_action() {
        assert_eq!(
            super::classify_ref_kind("main", spawn_postgres::RefKind::Main),
            super::StorageRefKind::Main
        );
        assert_eq!(
            super::classify_ref_kind("refs/spawn/tasks/42/head", spawn_postgres::RefKind::Branch),
            super::StorageRefKind::Task
        );
        assert_eq!(
            super::classify_ref_kind(
                "refs/spawn/tasks/42/accepted",
                spawn_postgres::RefKind::Branch
            ),
            super::StorageRefKind::Task
        );
        assert_eq!(
            super::classify_ref_kind("task/42", spawn_postgres::RefKind::Branch),
            super::StorageRefKind::Task
        );
        assert_eq!(
            super::classify_ref_kind(
                "refs/spawn/workers/w1/head",
                spawn_postgres::RefKind::Branch
            ),
            super::StorageRefKind::Worker
        );
        assert_eq!(
            super::classify_ref_kind("refs/heads/spawn/w1", spawn_postgres::RefKind::Branch),
            super::StorageRefKind::Worker
        );
        assert_eq!(
            super::classify_ref_kind("worker/agent-1", spawn_postgres::RefKind::Branch),
            super::StorageRefKind::Worker
        );
        assert_eq!(
            super::classify_ref_kind("feature/misc", spawn_postgres::RefKind::Branch),
            super::StorageRefKind::Worker
        );
        assert_eq!(
            super::classify_ref_kind("wip/w1", spawn_postgres::RefKind::Wip),
            super::StorageRefKind::Wip
        );
        assert_eq!(
            super::classify_ref_kind("v0.1.0", spawn_postgres::RefKind::Tag),
            super::StorageRefKind::Tag
        );

        assert_eq!(
            super::classify_ref_log_action(None, "bootstrap main"),
            super::RefLogAction::Created
        );
        assert_eq!(
            super::classify_ref_log_action(Some("sha256:a"), "land task 42 merge"),
            super::RefLogAction::Merged
        );
        assert_eq!(
            super::classify_ref_log_action(Some("sha256:a"), "checkpoint fast-forward"),
            super::RefLogAction::FastForwarded
        );
        assert_eq!(
            super::classify_ref_log_action(Some("sha256:a"), "rollback to prior snapshot"),
            super::RefLogAction::RolledBack
        );
        assert_eq!(
            super::classify_ref_log_action(Some("sha256:a"), "retire d6 project refs"),
            super::RefLogAction::Retired
        );
        assert_eq!(
            super::classify_ref_log_action(Some("sha256:a"), "retarget branch"),
            super::RefLogAction::Moved
        );
    }

    #[test]
    fn storage_recovery_commands_escape_shell_tokens_and_utf8_ids() {
        assert_eq!(
            super::short_snapshot_id("sha256:αβγδ1234567890"),
            "αβγδ12345678"
        );
        let injected_snap = spawn_postgres::SnapshotRow {
            id: "sha256:αβ'$(id);abcd".to_string(),
            git_oid: "abc';$(id)".to_string(),
            tree: "sha256:00".to_string(),
            parents: vec![],
            project: "proj'; rm -rf / #".to_string(),
            state: spawn_postgres::SnapshotState::Live,
            created_at_ms: 1000,
        };
        let cmds = super::recovery_commands_for_snapshot(&injected_snap);
        assert_eq!(cmds.len(), 5);
        assert!(
            cmds[0]["command"]
                .as_str()
                .unwrap()
                .contains("'proj'\"'\"'; rm -rf / #'")
        );
        assert!(
            cmds[3]["command"]
                .as_str()
                .unwrap()
                .contains("'recover/αβ'\"'\"'$(id);abc'")
        );
    }

    #[test]
    fn storage_tree_and_snapshot_graceful_when_database_url_unset() {
        let verify_unconfigured = || {
            let tree_ans = super::storage_tree_answer("");
            assert_eq!(tree_ans.status, 200, "{}", tree_ans.body);
            assert!(tree_ans.etag.is_some());
            let tree_body: serde_json::Value =
                serde_json::from_str(&tree_ans.body).expect("json tree");
            assert_eq!(tree_body["nodes"], json!([]));
            assert_eq!(tree_body["refs"], json!([]));
            assert_eq!(tree_body["history"], json!([]));
            assert_eq!(tree_body["summary"]["total_snapshots"], 0);
            assert!(
                tree_body["summary"]["mode_note"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("SPAWN_DATABASE_URL is not configured"),
                "{}",
                tree_ans.body
            );

            let digest = format!("sha256:{}", "aa".repeat(32));
            let snap_ans = super::storage_snapshot_answer(&digest);
            assert_eq!(snap_ans.status, 404, "{}", snap_ans.body);
            let snap_body: serde_json::Value =
                serde_json::from_str(&snap_ans.body).expect("json snap");
            assert_eq!(snap_body["code"], "snapshot_not_found");
        };

        // Explicit unconfigured URL path (covers environments where SPAWN_DATABASE_URL may be exported).
        super::with_reader_url("", verify_unconfigured);
        // Real unset SPAWN_DATABASE_URL path when running cleanly without DB env vars.
        if crate::env_value("SPAWN_DATABASE_URL").is_none()
            && crate::env_value("SPAWN_SERVE_DATABASE_URL").is_none()
        {
            verify_unconfigured();
        }

        // Genuine connection/SQL errors must still report HTTP 500 rather than silently returning 200.
        super::with_reader_url("postgres://invalid:invalid@127.0.0.1:1/nonexistent", || {
            let err_ans = super::storage_tree_answer("");
            assert_eq!(err_ans.status, 500, "{}", err_ans.body);
        });
    }

    struct SeededD6Dag {
        s0: String,
        s1: String,
        s3: String,
        o0: String,
        o3: String,
    }

    fn seed_d6_test_dag(store: &spawn_postgres::PostgresStore) -> SeededD6Dag {
        let hex64 = |b: &str| format!("sha256:{}", b.repeat(32));
        let oid40 = |b: &str| b.repeat(20);
        let tree = hex64("aa");
        let (s0, o0) = (hex64("10"), oid40("10"));
        let (s1, o1) = (hex64("20"), oid40("20"));
        let (s2, o2) = (hex64("30"), oid40("30"));
        let (s3, o3) = (hex64("40"), oid40("40"));
        let (sb, ob) = (hex64("50"), oid40("50"));

        let s0_ref = std::slice::from_ref(&s0);
        let merge_parents = [s0.clone(), s1.clone()];
        let specs: [(&str, &str, &[String], &str, i64); 5] = [
            (&s0, &o0, &[], "proj-alpha", 1_700_000_000_000),
            (&s1, &o1, s0_ref, "proj-alpha", 1_700_000_010_000),
            (&s2, &o2, s0_ref, "proj-alpha", 1_700_000_020_000),
            (&s3, &o3, &merge_parents, "proj-alpha", 1_700_000_030_000),
            (&sb, &ob, &[], "proj-beta", 1_700_000_040_000),
        ];
        for (id, oid, parents, proj, ts) in specs {
            store
                .record_snapshot(id, oid, &tree, parents, proj, ts)
                .expect(id);
        }

        store
            .land_main_ref("proj-alpha", "main", None, &s0, None, "bootstrap main")
            .expect("bootstrap main");
        let mission = store
            .mission_add("D6 serve test", &json!(["/tmp/proj-alpha.git"]), "human")
            .expect("add mission");
        store
            .mission_configure(mission, Some("/scenarios"), "gate", None, None)
            .expect("configure mission");
        let task = store
            .task_assign(mission, "ephemeral", "w_serve", "land d6", "default")
            .expect("assign");
        store.task_delivered(task, 1).expect("delivered");
        assert!(store.task_reported(task, "done").expect("reported"));
        store
            .integrator_record(&json!({
                "type": "task.gated",
                "id": task,
                "revision": 0,
                "verdict": "accepted",
                "base": o0,
                "candidate": o3,
            }))
            .expect("task.gated");
        store
            .land_main_ref(
                "proj-alpha",
                "main",
                Some(&s0),
                &s3,
                Some(task),
                "land merge s3",
            )
            .expect("advance main");

        for (name, target, msg) in [
            ("worker/agent-1", &s1, "fork worker"),
            ("task/42", &s2, "fork task"),
            ("refs/spawn/tasks/99/head", &s1, "checkpoint fast-forward"),
        ] {
            store
                .cas_ref(
                    "proj-alpha",
                    name,
                    spawn_postgres::RefKind::Branch,
                    None,
                    target,
                    msg,
                )
                .expect(name);
        }
        store
            .land_main_ref("proj-beta", "main", None, &sb, None, "bootstrap beta")
            .expect("beta main");
        SeededD6Dag { s0, s1, s3, o0, o3 }
    }

    fn send_storage_test_http(reader_url: &str, raw_req: &str) -> Answer {
        use std::io::{Read, Write};
        let server = tiny_http::Server::http("127.0.0.1:0").expect("listen");
        let port = server.server_addr().to_ip().expect("ip").port();
        let shared = Shared {
            web: std::env::temp_dir(),
            token: None,
            port: Some(port),
            hosts: server_hosts("127.0.0.1:0", port),
            open: open_networks(Some("")).expect("networks"),
            pools: Ok(None),
        };
        let url_for_worker = reader_url.to_string();
        let worker = std::thread::spawn(move || {
            let request = server.recv().expect("request");
            super::with_reader_url(&url_for_worker, || {
                handle(request, &shared).expect("answered");
            });
        });
        let mut client = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
        client
            .write_all(raw_req.replace("PORT", &port.to_string()).as_bytes())
            .expect("send");
        let mut answer = String::new();
        client.read_to_string(&mut answer).expect("read");
        worker.join().expect("handler");
        let (head, body) = answer.split_once("\r\n\r\n").unwrap_or((&answer, ""));
        Answer {
            status: head
                .split_whitespace()
                .nth(1)
                .and_then(|code| code.parse().ok())
                .expect("status line"),
            head: head.to_string(),
            body: serde_json::from_str(body).unwrap_or(serde_json::Value::Null),
        }
    }

    fn assert_storage_store_queries(
        reader_store: &spawn_postgres::PostgresStore,
        dag: &SeededD6Dag,
    ) {
        let tree_json = super::storage_tree_for_store(reader_store, Some("proj-alpha"), 120)
            .expect("proj-alpha tree");
        assert_eq!(tree_json["projects"], json!(["proj-alpha", "proj-beta"]));
        assert_eq!(tree_json["active_project"], "proj-alpha");
        assert_eq!(tree_json["summary"]["backend"], "d6");
        assert_eq!(tree_json["summary"]["total_snapshots"], 4);
        assert_eq!(tree_json["summary"]["active_refs"], 4);
        assert_eq!(tree_json["summary"]["history_entries"], 5);
        assert_eq!(tree_json["summary"]["truncated"], false);
        assert_eq!(tree_json["nodes"][0]["snapshot_id"], dag.s3);
        assert!(tree_json["nodes"][0].get("is_merge").is_none());
        assert_eq!(tree_json["nodes"][0]["parent_ids"], json!([dag.s0, dag.s1]));
        assert_eq!(tree_json["nodes"][0]["refs"][0]["name"], "main");
        assert!(tree_json["nodes"][0]["refs"][0].get("ref_kind").is_none());
        assert_eq!(tree_json["refs"][0].get("ref_kind"), None);
        assert_eq!(tree_json["history"][0]["name"], "refs/spawn/tasks/99/head");
        assert_eq!(tree_json["history"][0]["kind"], "task");
        assert!(tree_json["history"][0].get("ref_kind").is_none());
        assert_eq!(tree_json["history"][3]["action"], "merged");

        let s3_lookup = super::parse_snapshot_lookup(&dag.s3).expect("parse s3");
        let snap_s3 = super::storage_snapshot_for_store(reader_store, &s3_lookup)
            .expect("query s3")
            .expect("s3 exists");
        assert_eq!(snap_s3["snapshot"]["snapshot_id"], dag.s3);
        assert!(snap_s3["snapshot"].get("is_merge").is_none());
        assert_eq!(snap_s3["ancestors"].as_array().unwrap().len(), 2);
        assert_eq!(snap_s3["refs"][0]["name"], "main");
        assert_eq!(snap_s3["recovery_commands"].as_array().unwrap().len(), 5);

        let o0_lookup = super::parse_snapshot_lookup(&dag.o0).expect("parse o0");
        let snap_s0_by_oid = super::storage_snapshot_for_store(reader_store, &o0_lookup)
            .expect("query o0")
            .expect("o0 exists");
        assert_eq!(snap_s0_by_oid["snapshot"]["snapshot_id"], dag.s0);
        assert_eq!(snap_s0_by_oid["children"].as_array().unwrap().len(), 3);
    }

    fn assert_storage_http_roundtrips(reader_url: &str, dag: &SeededD6Dag) {
        let tree_res = send_storage_test_http(
            reader_url,
            "GET /api/storage/tree?project=proj-alpha HTTP/1.1\r\nHost: 127.0.0.1:PORT\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(tree_res.status, 200, "{}", tree_res.head);
        assert_eq!(tree_res.body["summary"]["total_snapshots"], 4);
        let etag = tree_res
            .head
            .lines()
            .find_map(|line| {
                let (k, v) = line.split_once(':')?;
                k.eq_ignore_ascii_case("etag").then(|| v.trim().to_string())
            })
            .expect("ETag header");
        let cached = send_storage_test_http(
            reader_url,
            &format!(
                "GET /api/storage/tree?project=proj-alpha HTTP/1.1\r\nHost: 127.0.0.1:PORT\r\nIf-None-Match: {etag}\r\nConnection: close\r\n\r\n"
            ),
        );
        assert_eq!(cached.status, 304, "{}", cached.head);

        let snap_res = send_storage_test_http(
            reader_url,
            &format!(
                "GET /api/storage/snapshot/{} HTTP/1.1\r\nHost: 127.0.0.1:PORT\r\nConnection: close\r\n\r\n",
                dag.s3
            ),
        );
        assert_eq!(snap_res.status, 200, "{}", snap_res.head);
        assert_eq!(snap_res.body["snapshot"]["git_oid"], dag.o3);

        let missing_id = format!("sha256:{}", "ff".repeat(32));
        let not_found = send_storage_test_http(
            reader_url,
            &format!(
                "GET /api/storage/snapshot/{missing_id} HTTP/1.1\r\nHost: 127.0.0.1:PORT\r\nConnection: close\r\n\r\n"
            ),
        );
        assert_eq!(not_found.status, 404, "{}", not_found.head);

        let bad_res = send_storage_test_http(
            reader_url,
            "GET /api/storage/snapshot/not-valid HTTP/1.1\r\nHost: 127.0.0.1:PORT\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(bad_res.status, 400, "{}", bad_res.head);

        let method_res = send_storage_test_http(
            reader_url,
            "POST /api/storage/tree HTTP/1.1\r\nHost: 127.0.0.1:PORT\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(method_res.status, 405, "{}", method_res.head);
    }

    #[test]
    fn storage_tree_and_snapshot_endpoints_work_empty_and_with_d6_dag() {
        in_test_schema(|owner, store| {
            let reader_url =
                spawn_postgres::role_url(owner, "spawn_reader").expect("reader role url");
            let reader_store =
                spawn_postgres::PostgresStore::connect(&reader_url).expect("connect reader store");

            let empty = super::storage_tree_for_store(&reader_store, None, 120)
                .expect("empty tree succeeds");
            assert_eq!(empty["nodes"].as_array().unwrap().len(), 0);
            assert_eq!(empty["refs"].as_array().unwrap().len(), 0);
            assert_eq!(empty["history"].as_array().unwrap().len(), 0);
            assert_eq!(empty["summary"]["total_snapshots"], 0);
            assert_eq!(empty["summary"]["backend"], "git");
            assert!(
                !empty["summary"]["mode_note"]
                    .as_str()
                    .unwrap_or_default()
                    .is_empty()
            );

            let dag = seed_d6_test_dag(store);
            assert_storage_store_queries(&reader_store, &dag);
            assert_storage_http_roundtrips(&reader_url, &dag);
        });
    }

    fn percentile_ms(sorted_us: &[u128], num: usize, den: usize) -> f64 {
        if sorted_us.is_empty() {
            return 0.0;
        }
        let idx = ((sorted_us.len() - 1) * num) / den;
        std::time::Duration::from_micros(u64::try_from(sorted_us[idx]).unwrap_or(u64::MAX))
            .as_secs_f64()
            * 1000.0
    }

    fn spawn_serve_activity_sampler(
        mut observer: postgres::Client,
        sampling: &Arc<std::sync::atomic::AtomicBool>,
    ) -> std::thread::JoinHandle<(usize, i64, i64)> {
        use std::sync::atomic::Ordering;
        let sampling_flag = Arc::clone(sampling);
        std::thread::spawn(move || {
            let mut samples = 0usize;
            let mut peak_total = 0_i64;
            let mut peak_role = 0_i64;
            loop {
                let row = observer
                    .query_one(
                        "select count(*)::bigint,
                                count(*) filter (
                                    where application_name = 'spawn_serve_stress_1000'
                                      and (usename like 'spawn_reader%' or usename like 'spawn_human%')
                                )::bigint
                           from pg_stat_activity
                          where datname = current_database()",
                        &[],
                    )
                    .expect("sample pg_stat_activity");
                samples += 1;
                peak_total = peak_total.max(row.get::<_, i64>(0));
                peak_role = peak_role.max(row.get::<_, i64>(1));
                if !sampling_flag.load(Ordering::Acquire) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            (samples, peak_total, peak_role)
        })
    }

    fn send_stress_http_request(port: u16, idx: usize) -> (u16, u128, String, bool) {
        use std::io::{Read, Write};
        let t0 = std::time::Instant::now();
        let Ok(mut stream) = std::net::TcpStream::connect(("127.0.0.1", port)) else {
            return (
                0_u16,
                t0.elapsed().as_micros(),
                "connect failed".to_string(),
                false,
            );
        };
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));
        let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(10)));
        let is_write = idx.is_multiple_of(7);
        let req_bytes = if is_write {
            let body = format!(r#"{{"text":"stress {idx}","key":"stress-in-{idx}"}}"#);
            format!(
                "POST /api/sessions/session_supervisor/input HTTP/1.1\r\n\
                 Host: 127.0.0.1:{port}\r\n\
                 Authorization: Bearer stress-secret\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\r\n{}",
                body.len(),
                body
            )
        } else {
            format!(
                "GET /api/events?limit=5 HTTP/1.1\r\n\
                 Host: 127.0.0.1:{port}\r\n\
                 Authorization: Bearer stress-secret\r\n\
                 Connection: close\r\n\r\n"
            )
        };
        if let Err(e) = stream.write_all(req_bytes.as_bytes()) {
            return (0_u16, t0.elapsed().as_micros(), e.to_string(), is_write);
        }
        let mut resp = String::new();
        if let Err(e) = stream.read_to_string(&mut resp) {
            return (0_u16, t0.elapsed().as_micros(), e.to_string(), is_write);
        }
        let status = resp
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse::<u16>().ok())
            .unwrap_or(0);
        (status, t0.elapsed().as_micros(), resp, is_write)
    }

    struct ServeBurstResult {
        latencies_us: Vec<u128>,
        ok_reads: usize,
        ok_writes: usize,
        failures: Vec<(u16, bool, String)>,
        too_many_clients: usize,
        elapsed: std::time::Duration,
    }

    fn run_serve_stress_burst(port: u16, concurrency: usize) -> ServeBurstResult {
        let barrier = Arc::new(std::sync::Barrier::new(concurrency));
        let begun = std::time::Instant::now();
        let handles: Vec<_> = (0..concurrency)
            .map(|idx| {
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    send_stress_http_request(port, idx)
                })
            })
            .collect();

        let mut latencies_us = Vec::with_capacity(concurrency);
        let mut ok_reads = 0usize;
        let mut ok_writes = 0usize;
        let mut failures = Vec::new();
        let mut too_many_clients = 0usize;

        for h in handles {
            let (status, lat_us, resp, is_write) = h.join().expect("client thread");
            latencies_us.push(lat_us);
            if resp
                .to_ascii_lowercase()
                .contains("too many clients already")
            {
                too_many_clients += 1;
            }
            if is_write && status == 202 {
                ok_writes += 1;
            } else if !is_write && status == 200 {
                ok_reads += 1;
            } else if failures.len() < 10 {
                failures.push((status, is_write, resp.chars().take(200).collect::<String>()));
            }
        }
        ServeBurstResult {
            latencies_us,
            ok_reads,
            ok_writes,
            failures,
            too_many_clients,
            elapsed: begun.elapsed(),
        }
    }

    #[test]
    fn thousand_concurrent_http_requests_stay_bounded_under_1c1g_pg() {
        use std::sync::atomic::{AtomicBool, Ordering};

        const CONCURRENCY: usize = 1000;

        in_test_schema(|owner, store| {
            execution_starts(store);
            let observer_url =
                std::env::var("SPAWN_TEST_DATABASE_URL").expect("SPAWN_TEST_DATABASE_URL");
            let observer =
                postgres::Client::connect(&observer_url, postgres::NoTls).expect("observer pg");

            let config = super::ipc::PoolConfig::default();
            assert_eq!(
                (config.reads, config.writes, config.node, config.events),
                (6, 2, 6, 2)
            );

            let sep = if owner.contains('?') { '&' } else { '?' };
            let tagged_owner = format!("{owner}{sep}application_name=spawn_serve_stress_1000");
            let pools = super::ipc::ServePools::from_serve_env(Some(&tagged_owner), None, config)
                .expect("serve pools")
                .expect("configured pools");

            let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").expect("listen"));
            let port = server.server_addr().to_ip().expect("ip").port();
            let shared = Arc::new(Shared {
                web: std::env::temp_dir(),
                token: Some("stress-secret".to_string()),
                port: Some(port),
                hosts: server_hosts("127.0.0.1:0", port),
                open: Vec::new(),
                pools: Ok(Some(pools)),
            });
            let streams = Slots::with_per_peer(stream::SSE_SLOTS, Some(stream::STREAMS_PER_PEER));
            let server_for_loop = Arc::clone(&server);
            let server_shared = Arc::clone(&shared);
            let server_streams = Arc::clone(&streams);
            let server_thread = std::thread::spawn(move || {
                super::serve_requests_with_queue(
                    &server_for_loop,
                    &server_shared,
                    MAX_IN_FLIGHT,
                    super::MAX_REQUEST_QUEUE,
                    &server_streams,
                )
            });

            let (warm_st, warm_body) = raw_status(
                port,
                "/api/events?limit=5",
                "Authorization: Bearer stress-secret\r\n",
            );
            assert_eq!(warm_st, 200, "{warm_body}");

            let sampling = Arc::new(AtomicBool::new(true));
            let monitor = spawn_serve_activity_sampler(observer, &sampling);
            let mut burst = run_serve_stress_burst(port, CONCURRENCY);
            sampling.store(false, Ordering::Release);
            let (sample_count, peak_total, peak_role) = monitor.join().expect("sampler thread");
            server.unblock();
            let _ = server_thread.join();
            drop(shared);

            burst.latencies_us.sort_unstable();
            let p50 = percentile_ms(&burst.latencies_us, 50, 100);
            let p95 = percentile_ms(&burst.latencies_us, 95, 100);
            let p99 = percentile_ms(&burst.latencies_us, 99, 100);
            let rps = f64::from(u32::try_from(CONCURRENCY).unwrap_or(u32::MAX))
                / burst.elapsed.as_secs_f64();

            eprintln!(
                "[stress:spawn-serve-1000] reqs={CONCURRENCY} reads={} writes={} \
                 failures={} too_many_clients={} samples={sample_count} peak_role_conns={peak_role}/8 \
                 peak_pg_stat_activity={peak_total}/22 elapsed_ms={:.1} rps={rps:.1} \
                 p50_ms={p50:.2} p95_ms={p95:.2} p99_ms={p99:.2}",
                burst.ok_reads,
                burst.ok_writes,
                CONCURRENCY - burst.ok_reads - burst.ok_writes,
                burst.too_many_clients,
                burst.elapsed.as_secs_f64() * 1000.0
            );

            assert_eq!(burst.too_many_clients, 0, "must never hit max_connections");
            assert!(
                sample_count > 0,
                "pg_stat_activity sampler recorded 0 samples"
            );
            assert_eq!(
                burst.ok_reads + burst.ok_writes,
                CONCURRENCY,
                "sample failures: {:?}",
                burst.failures
            );
            assert!(
                peak_role > 0 && peak_role <= 8,
                "serve role pool connections must be in 1..=8, got {peak_role}"
            );
            assert!(
                peak_total > 0 && peak_total <= 22,
                "total pg_stat_activity must be in 1..=22, got {peak_total}"
            );
        });
    }

    fn read_tcp_until(client: &mut std::net::TcpStream, needle: &str) -> String {
        use std::io::Read;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut seen = String::new();
        let mut buf = [0u8; 4096];
        while !seen.contains(needle) {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for {needle:?}: {seen}"
            );
            client.set_read_timeout(Some(remaining)).expect("timeout");
            let n = client.read(&mut buf).expect("read stream");
            assert!(n > 0, "EOF before {needle:?}: {seen}");
            seen.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        seen
    }

    fn open_tcp_sse(port: u16, token: &str) -> std::net::TcpStream {
        use std::io::Write;
        let mut client = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
        write!(
            client,
            "GET /api/stream HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\n\r\n"
        )
        .unwrap();
        client.flush().unwrap();
        let _ = read_tcp_until(&mut client, "event: ready\n");
        client
    }

    #[test]
    fn tcp_sse_and_http_reads_succeed_concurrently_under_reads_pool_one() {
        use spawn_core::port::EventLog;

        in_test_schema(|owner, store| {
            execution_starts(store);
            let config = super::ipc::PoolConfig {
                reads: 1,
                writes: 1,
                node: 1,
                events: 2,
                wait_timeout: std::time::Duration::from_millis(400),
            };
            let pools = super::ipc::ServePools::from_serve_env(Some(owner), None, config)
                .expect("pools")
                .expect("configured");
            let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").expect("listen"));
            let port = server.server_addr().to_ip().expect("ip").port();
            let shared = Arc::new(Shared {
                web: std::env::temp_dir(),
                token: Some("reads-one-secret".to_string()),
                port: Some(port),
                hosts: server_hosts("127.0.0.1:0", port),
                open: Vec::new(),
                pools: Ok(Some(Arc::clone(&pools))),
            });
            let streams = Slots::with_per_peer(stream::SSE_SLOTS, Some(stream::STREAMS_PER_PEER));
            let server_for_loop = Arc::clone(&server);
            let server_shared = Arc::clone(&shared);
            let server_streams = Arc::clone(&streams);
            let server_thread = std::thread::spawn(move || {
                super::serve_requests_with_queue(
                    &server_for_loop,
                    &server_shared,
                    MAX_IN_FLIGHT,
                    super::MAX_REQUEST_QUEUE,
                    &server_streams,
                )
            });

            let mut sse1 = open_tcp_sse(port, "reads-one-secret");
            let mut sse2 = open_tcp_sse(port, "reads-one-secret");
            assert_eq!(pools.reader.stats().0, 0);

            for _ in 0..8 {
                let (st, body) = raw_status(
                    port,
                    "/api/events?limit=5",
                    "Authorization: Bearer reads-one-secret\r\n",
                );
                assert_eq!(st, 200, "{body}");
                let (st, body) = raw_status(
                    port,
                    "/api/overview",
                    "Authorization: Bearer reads-one-secret\r\n",
                );
                assert_eq!(st, 200, "{body}");
            }

            let live_id = store
                .append(spawn_core::event::NewEvent {
                    agent_id: spawn_core::id::AgentId::new("agent_tcp_reads1").unwrap(),
                    session_id: spawn_core::id::SessionId::new("session_tcp_reads1").unwrap(),
                    execution_id: None,
                    source: spawn_core::event::Source::Bridge,
                    source_event_id: Some("tcp-reads1-live".to_string()),
                    kind: spawn_core::event::EventKind::MessageReceived,
                })
                .unwrap()
                .record
                .position
                .0;
            for sse in [&mut sse1, &mut sse2] {
                let seen = read_tcp_until(sse, &format!("id: {live_id}\n"));
                assert!(seen.contains("\"session_tcp_reads1\""), "{seen}");
            }

            drop((sse1, sse2));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while pools.events.stats().0 != 0 && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            assert_eq!(pools.reader.stats().0, 0);
            assert_eq!(pools.events.stats().0, 0);

            server.unblock();
            let _ = server_thread.join();
        });
    }
}
