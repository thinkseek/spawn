//! The coordinators (issue #87): a node behind any NAT or firewall keeps
//! one outbound connection to a coordinator (`spawn node connect`), and
//! is reached through it, its requests and replies sealed end to end
//! (super::crypto's `spawn-e2e-v1`): a coordinator forwards envelopes it
//! cannot open. Coordinators form a tree (`spawn node coord --parent`):
//! each pushes its zone's directory up, and forwards what it cannot
//! deliver to its parent. The same protocol, `spawn-coord-v1`, one JSON
//! object a WebSocket text frame (`ws://`, `wss://`) or a line (`tcp://`),
//! is what a Cloudflare Worker and Durable Object relay on `:443` speaks.
//!
//! The messages:
//!
//! - `hello` (a node, a client, or a child coordinator, first thing),
//!   answered `welcome`: its `node_id`, `pubkey`, prekey (`x25519_pub`,
//!   `epoch`, `prekey_sig`), `name`, `role`, `zone`, `udp_reflexive`,
//!   `kind` (node, client or coord) and `at_ms`, signed (`sig`) by its
//!   Ed25519 key; with `SPAWN_COORD_TOKEN` set, the coordinator also wants
//!   the token: the WebSocket upgrade's bearer, or `coord_auth`
//!   (HMAC-SHA256 of the signed hello under it, the token never sent).
//! - `lookup {id, node_id | name}`, answered `peer_info {id, found,
//!   peer}`: a node's public keys, `name`, `role`, `zone` and
//!   `udp_reflexive` only, from this coordinator, a child's summary or,
//!   asked in turn, its parent.
//! - `relay {from, to, id, envelope}`: `from` must be the session's own
//!   node id (a child coordinator's: one of its summary), `envelope` is
//!   passed on untouched to `to`, here, down a child or up the parent;
//!   `error {id, to, for, error}` when no coordinator knows `to`.
//! - `zone_summary {zone, peers_summary, lease_epoch, lease_ms}`: a child
//!   coordinator's directory, O(K) in its peers, pushed at a third of its
//!   lease and forgotten when the lease runs out (soft state).
//! - `punch {from, to, nonce, reflexive}`: routed as a relay; the target
//!   sends a datagram to `reflexive` to open its NAT for a direct udp
//!   exchange (P4), which falls back at once to the relay (P3).
//! - `ping`, answered `pong`.
//!
//! `spawn node coord --udp` answers `{"op": "stun", "nonce"}` with
//! `{"op": "stun_reply", "nonce", "reflexive": "ip:port"}`, the address
//! the datagram came from (a node's public udp mapping).
//!
//! Bounds: a message is at most [`MAX_FRAME`]; a hello comes within
//! [`HELLO_TIMEOUT`]; at most [`MAX_SESSIONS`] sessions; each session's
//! queue holds [`QUEUE`] messages (more are dropped: a slow reader cannot
//! grow the coordinator); a session silent for [`IDLE`] is closed.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs as _, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use zeroize::Zeroizing;

use super::auth::{MAX_SKEW_MS, same};
use super::crypto::{
    self, NodeIdentity, PeerKeys, Responder, TrustPolicy, envelope_error, seal_request, unb64,
    valid_node_id,
};
use super::relay::{Registrar, Runner, store_refusal};
use super::transport::{Op, forbidden, nonce, parse, valid_nonce};
use super::ws::{Message, WsStream};
use super::{OUTPUT_LIMIT, inventory, valid_name};
use crate::verify::now_ms;

/// The protocol's name, on every hello.
pub(crate) const PROTOCOL: &str = "spawn-coord-v1";
/// The largest message.
pub(crate) const MAX_FRAME: usize = super::ws::MAX_FRAME;
/// How long a new connection has to say its hello.
pub(crate) const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// The most sessions a coordinator holds.
pub(crate) const MAX_SESSIONS: usize = 4096;
/// The most messages queued for one session.
pub(crate) const QUEUE: usize = 256;
/// A session that says nothing this long is closed (clients ping at a
/// third of it).
pub(crate) const IDLE: Duration = Duration::from_secs(75);
/// Between two pings of a client.
const PING_EVERY: Duration = Duration::from_secs(20);
/// A pump's wait for its connection, between two looks at its queue.
const POLL: Duration = Duration::from_millis(15);
/// A child coordinator's lease on its summary, by default.
pub(crate) const LEASE: Duration = Duration::from_secs(30);
/// A client's hello refused: its node id is a connected node's.
pub(crate) const ID_IN_USE: &str = "this node id is a connected node's";
/// The most connections a coordinator holds open before their hello (an
/// upgrade or hello not said yet): more are closed at once.
pub(crate) const MAX_PENDING: usize = 256;
/// The most sealed requests a connected node answers at once.
pub(crate) const MAX_REQUESTS: usize = 32;
/// The most lookups a coordinator asks of its parent at once.
const MAX_UPWARD: usize = 64;
/// The most peers a summary may hold.
const MAX_SUMMARY: usize = 10_000;
/// How long a probe waits for a punched udp path before the relay.
const PUNCH_WAIT: Duration = Duration::from_millis(700);
/// A node's prekey lives this long before it is rotated (the connection
/// made again, its hello with the new one).
const PREKEY_LIFETIME: Duration = Duration::from_hours(1);

/// A coordinator URL's scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scheme {
    Ws,
    Wss,
    Tcp,
}

/// A coordinator's URL: `ws://`, `wss://` or `tcp://`, a host (a name,
/// an IPv4, a bracketed IPv6 address), a port (80 and 443 by default, a
/// tcp one always said), a path. No user, space or control character, at
/// most 255 characters (`spawn_node_record` holds the same).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CoordUrl {
    pub(crate) scheme: Scheme,
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) path: String,
    text: String,
}

impl CoordUrl {
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let bad = || {
            "a coordinator is ws://, wss:// or tcp://host[:port][/path], the port 1 to 65535"
                .to_string()
        };
        if text.len() > 255
            || text.contains('@')
            || text.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(bad());
        }
        let (scheme, rest) = text.split_once("://").ok_or_else(bad)?;
        let scheme = match scheme {
            "ws" => Scheme::Ws,
            "wss" => Scheme::Wss,
            "tcp" => Scheme::Tcp,
            _ => return Err(bad()),
        };
        let (authority, path) = match rest.find('/') {
            Some(at) => rest.split_at(at),
            None => (rest, ""),
        };
        let path_ok = path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._~:/?#[]!$&'()*+,;=%-".contains(c));
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (inside, after) = v6.split_once(']').ok_or_else(bad)?;
            inside.parse::<std::net::Ipv6Addr>().map_err(|_| bad())?;
            (
                format!("[{inside}]"),
                after.strip_prefix(':').map(str::to_string),
            )
        } else {
            match authority.rsplit_once(':') {
                Some((host, port)) => (host.to_string(), Some(port.to_string())),
                None => (authority.to_string(), None),
            }
        };
        let name_ok = host.starts_with('[') || {
            let mut chars = host.chars();
            chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
                && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
        };
        let port = match port {
            Some(port) => Some(
                (port.len() <= 5 && !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()))
                    .then(|| port.parse::<u16>().ok())
                    .flatten()
                    .filter(|port| *port > 0)
                    .ok_or_else(bad)?,
            ),
            None => None,
        };
        let port = match (scheme, port) {
            (_, Some(port)) => port,
            (Scheme::Ws, None) => 80,
            (Scheme::Wss, None) => 443,
            (Scheme::Tcp, None) => {
                return Err(format!("{}: a tcp coordinator says its port", bad()));
            }
        };
        if !name_ok || !path_ok {
            return Err(bad());
        }
        Ok(Self {
            scheme,
            host,
            port,
            path: if path.is_empty() {
                "/".to_string()
            } else {
                path.to_string()
            },
            text: text.to_string(),
        })
    }

    /// The URL as it was given.
    pub(crate) fn url(&self) -> String {
        self.text.clone()
    }

    /// The host without an IPv6 address's brackets.
    pub(crate) fn bare_host(&self) -> &str {
        self.host.trim_start_matches('[').trim_end_matches(']')
    }
}

impl std::fmt::Display for CoordUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// A connection speaking `spawn-coord-v1`: a WebSocket, or lines on tcp.
pub(crate) enum Channel {
    Ws(Box<WsStream>),
    Line { stream: TcpStream, buffer: Vec<u8> },
}

impl Channel {
    /// Connect to `url`, with `bearer` (the coordinator's token) on a
    /// WebSocket's upgrade, by `timeout`.
    pub(crate) fn open(
        url: &CoordUrl,
        bearer: Option<&str>,
        timeout: Duration,
        tls: Option<Arc<rustls::ClientConfig>>,
    ) -> Result<Self, String> {
        match url.scheme {
            Scheme::Ws | Scheme::Wss => Ok(Self::Ws(Box::new(WsStream::connect(
                url, bearer, timeout, tls,
            )?))),
            Scheme::Tcp => {
                let addresses: Vec<SocketAddr> = (url.bare_host(), url.port)
                    .to_socket_addrs()
                    .map_err(|error| format!("{url}: {error}"))?
                    .collect();
                let stream =
                    super::transport::connect_happy(url, &addresses, Instant::now() + timeout)?;
                stream
                    .set_write_timeout(Some(timeout))
                    .map_err(|error| error.to_string())?;
                let _ = stream.set_nodelay(true);
                Ok(Self::Line {
                    stream,
                    buffer: Vec::new(),
                })
            }
        }
    }

    pub(crate) fn send(&mut self, message: &Value) -> Result<(), String> {
        let text = serde_json::to_string(message).map_err(|error| error.to_string())?;
        if text.len() > MAX_FRAME {
            return Err(format!("a message over {MAX_FRAME} bytes"));
        }
        match self {
            Self::Ws(ws) => ws.send_text(&text),
            Self::Line { stream, .. } => {
                use std::io::Write as _;
                let mut line = text.into_bytes();
                line.push(b'\n');
                stream.write_all(&line).map_err(|error| error.to_string())
            }
        }
    }

    /// The next message (a JSON object) within `timeout`, or none.
    pub(crate) fn recv(&mut self, timeout: Duration) -> Result<Option<Value>, String> {
        match self {
            Self::Ws(ws) => match ws.recv(timeout)? {
                None => Ok(None),
                Some(Message::Text(text)) => parse(text.as_bytes()).map(Some),
                Some(Message::Binary(bytes)) => parse(&bytes).map(Some),
                Some(Message::Close) => Err("the coordinator closed".to_string()),
            },
            Self::Line { stream, buffer } => {
                use std::io::Read as _;
                let deadline = Instant::now() + timeout;
                loop {
                    if let Some(end) = buffer.iter().position(|byte| *byte == b'\n') {
                        let line: Vec<u8> = buffer.drain(..=end).collect();
                        return parse(&line[..end]).map(Some);
                    }
                    if buffer.len() > MAX_FRAME {
                        return Err(format!("a message over {MAX_FRAME} bytes"));
                    }
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Ok(None);
                    }
                    stream
                        .set_read_timeout(Some(left))
                        .map_err(|error| error.to_string())?;
                    let mut chunk = [0_u8; 16 * 1024];
                    match stream.read(&mut chunk) {
                        Ok(0) => return Err("the connection closed".to_string()),
                        Ok(count) => buffer.extend_from_slice(&chunk[..count]),
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) =>
                        {
                            return Ok(None);
                        }
                        Err(error) => return Err(error.to_string()),
                    }
                }
            }
        }
    }

    fn peer(&self) -> Option<SocketAddr> {
        match self {
            Self::Ws(ws) => ws.peer(),
            Self::Line { stream, .. } => stream.peer_addr().ok(),
        }
    }
}

/// Who says a hello: a node (reached through the coordinator), a client
/// (it asks, is not listed), or a child coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Node,
    Client,
    Coord,
}

impl Kind {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "node" => Some(Self::Node),
            "client" => Some(Self::Client),
            "coord" => Some(Self::Coord),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Client => "client",
            Self::Coord => "coord",
        }
    }
}

/// What a hello says of its sender beside its keys.
#[derive(Debug, Clone)]
pub(crate) struct HelloFields {
    pub(crate) kind: Kind,
    pub(crate) name: String,
    pub(crate) role: String,
    pub(crate) zone: Option<String>,
    pub(crate) udp_reflexive: Option<String>,
}

/// Whether `zone` is a zone's name (as a node's).
pub(crate) fn valid_zone(zone: &str) -> bool {
    valid_name(zone)
}

/// The fields a hello's signature (and `coord_auth`) covers.
fn hello_canonical(hello: &Value) -> Vec<u8> {
    let fields: Vec<Value> = [
        "type",
        "kind",
        "node_id",
        "pubkey",
        "x25519_pub",
        "epoch",
        "prekey_sig",
        "name",
        "role",
        "zone",
        "udp_reflexive",
        "at_ms",
    ]
    .iter()
    .map(|key| hello[*key].clone())
    .collect();
    serde_json::to_vec(&json!([PROTOCOL, fields])).unwrap_or_default()
}

fn hmac_hex(token: &str, bytes: &[u8]) -> String {
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(token.as_bytes())
        .unwrap_or_else(|_| Hmac::<sha2::Sha256>::new_from_slice(b"-").expect("a key"));
    mac.update(bytes);
    crypto::hex(&mac.finalize().into_bytes())
}

/// A signed hello of `identity` (with `coord_auth` under `token`).
pub(crate) fn hello(identity: &NodeIdentity, fields: &HelloFields, token: Option<&str>) -> Value {
    let mut hello = identity.public_info();
    hello["type"] = json!("hello");
    hello["v"] = json!(PROTOCOL);
    hello["kind"] = json!(fields.kind.name());
    hello["name"] = json!(fields.name);
    hello["role"] = json!(fields.role);
    hello["zone"] = json!(fields.zone);
    hello["udp_reflexive"] = json!(fields.udp_reflexive);
    hello["at_ms"] = json!(now_ms());
    let canonical = hello_canonical(&hello);
    hello["sig"] = json!(crypto::b64(&identity.sign(&canonical)));
    if let Some(token) = token {
        hello["coord_auth"] = json!(hmac_hex(token, &canonical));
    }
    hello
}

/// What a coordinator lists of a peer: its public keys, name, role, zone
/// and udp mapping, nothing else.
fn public_record(hello: &Value) -> Value {
    json!({
        "node_id": hello["node_id"], "pubkey": hello["pubkey"],
        "x25519_pub": hello["x25519_pub"], "epoch": hello["epoch"],
        "prekey_sig": hello["prekey_sig"], "name": hello["name"], "role": hello["role"],
        "zone": hello["zone"], "udp_reflexive": hello["udp_reflexive"],
    })
}

/// A hello checked: its protocol, time, keys and signature, its fields,
/// and, when the coordinator has a `token`, its bearer (`bearer`, the
/// WebSocket upgrade's) or `coord_auth`. Its kind and public record.
pub(crate) fn check_hello(
    hello: &Value,
    token: Option<&str>,
    bearer: bool,
) -> Result<(Kind, Value), String> {
    if hello["type"] != "hello" || hello["v"] != PROTOCOL {
        return Err(format!("the first message is a {PROTOCOL} hello"));
    }
    let at = hello["at_ms"].as_i64().unwrap_or(i64::MIN);
    if now_ms().saturating_sub(at).abs() > MAX_SKEW_MS {
        return Err("the hello's at_ms is too far from the coordinator's clock".to_string());
    }
    let peer = PeerKeys::verify(hello, None, None)?;
    let canonical = hello_canonical(hello);
    let sig = hello["sig"]
        .as_str()
        .and_then(|sig| unb64(sig).ok())
        .ok_or("a hello without its signature")?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, peer.pubkey)
        .verify(&canonical, &sig)
        .map_err(|_| "the hello's signature is not its node's".to_string())?;
    if let Some(token) = token
        && !bearer
        && !hello["coord_auth"]
            .as_str()
            .is_some_and(|auth| same(auth, &hmac_hex(token, &canonical)))
    {
        return Err("the hello carries no SPAWN_COORD_TOKEN of this coordinator".to_string());
    }
    let kind = hello["kind"]
        .as_str()
        .and_then(Kind::parse)
        .ok_or("a hello's kind is node, client or coord")?;
    if !hello["name"].as_str().is_some_and(valid_name) {
        return Err("a hello's name is a node's name".to_string());
    }
    if !matches!(
        hello["role"].as_str(),
        Some("worker" | "peer" | "client" | "coord")
    ) {
        return Err("a hello's role is worker, peer, client or coord".to_string());
    }
    if !(hello["zone"].is_null() || hello["zone"].as_str().is_some_and(valid_zone)) {
        return Err("a hello's zone is a zone's name".to_string());
    }
    if !(hello["udp_reflexive"].is_null()
        || hello["udp_reflexive"]
            .as_str()
            .is_some_and(|text| text.parse::<SocketAddr>().is_ok()))
    {
        return Err("a hello's udp_reflexive is ip:port".to_string());
    }
    Ok((kind, public_record(hello)))
}

/// One connected session of a coordinator.
struct Session {
    kind: Kind,
    record: Value,
    queue: SyncSender<Value>,
    dead: Arc<AtomicBool>,
    /// A child coordinator's summary: its peers, until when, its epoch.
    summary: Vec<Value>,
    lease_until: Instant,
    lease_epoch: u64,
}

/// What a coordinator holds: its sessions by id, its nodes and clients by
/// node id.
#[derive(Default)]
struct Directory {
    sessions: HashMap<u64, Session>,
    by_node: HashMap<String, u64>,
}

/// Where a message to a node id goes.
enum Hop {
    Local(SyncSender<Value>),
    Parent,
    Nowhere,
}

/// A coordinator: its directory, its parent link, its tap (tests).
pub(crate) struct Coordinator {
    zone: Option<String>,
    token: Option<Zeroizing<String>>,
    identity: Arc<NodeIdentity>,
    directory: Mutex<Directory>,
    parent: Mutex<Option<Arc<CoordClient>>>,
    next: AtomicU64,
    /// The epoch of the summary last pushed up.
    epoch: AtomicU64,
    /// Lookups asked of the parent and not answered yet.
    upward: std::sync::atomic::AtomicUsize,
    lease: Duration,
    /// Every message handled, as it came (tests: what a coordinator sees).
    tap: Option<Arc<Mutex<Vec<String>>>>,
}

/// Send `message` on `queue`, dropped when it is full or gone.
fn push(queue: &SyncSender<Value>, message: Value) {
    match queue.try_send(message) {
        Ok(()) | Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {}
    }
}

impl Coordinator {
    pub(crate) fn new(
        identity: Arc<NodeIdentity>,
        zone: Option<String>,
        token: Option<String>,
        lease: Duration,
    ) -> Self {
        Self {
            zone,
            token: token.filter(|t| !t.is_empty()).map(Zeroizing::new),
            identity,
            directory: Mutex::new(Directory::default()),
            parent: Mutex::new(None),
            next: AtomicU64::new(1),
            epoch: AtomicU64::new(0),
            upward: std::sync::atomic::AtomicUsize::new(0),
            lease: lease.clamp(Duration::from_secs(1), Duration::from_mins(10)),
            tap: None,
        }
    }

    fn token(&self) -> Option<&str> {
        self.token.as_deref().map(String::as_str)
    }

    /// This coordinator's own node id (its key's: a child's hello upward).
    pub(crate) fn node_id(&self) -> &str {
        self.identity.node_id()
    }

    fn directory(&self) -> std::sync::MutexGuard<'_, Directory> {
        self.directory
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn parent(&self) -> Option<Arc<CoordClient>> {
        self.parent
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .filter(|client| client.alive())
    }

    /// Take a session that said `hello` (checked), its queue `queue`: its
    /// id, or why not (full; a node id held by a session it may not take
    /// over).
    fn join(
        &self,
        kind: Kind,
        record: Value,
        queue: SyncSender<Value>,
        dead: Arc<AtomicBool>,
    ) -> Result<u64, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let node_id = record["node_id"].as_str().unwrap_or_default().to_string();
        let mut directory = self.directory();
        if directory.sessions.len() >= MAX_SESSIONS {
            return Err("the coordinator is full".to_string());
        }
        // The same node again (it reconnected): the new session takes over.
        // A client's never: a probe with a node's key (a stale key file, a
        // connector's) would drop that node's or coordinator's session and
        // with it every request waiting on it.
        if let Some(old) = directory.by_node.get(&node_id).copied() {
            let old_kind = directory.sessions.get(&old).map(|session| session.kind);
            if kind == Kind::Client && matches!(old_kind, Some(Kind::Node | Kind::Coord)) {
                return Err(ID_IN_USE.to_string());
            }
            if directory.by_node.insert(node_id, id) == Some(old)
                && let Some(old) = directory.sessions.remove(&old)
            {
                old.dead.store(true, Ordering::Relaxed);
            }
        } else {
            directory.by_node.insert(node_id, id);
        }
        directory.sessions.insert(
            id,
            Session {
                kind,
                record,
                queue,
                dead,
                summary: Vec::new(),
                lease_until: Instant::now(),
                lease_epoch: 0,
            },
        );
        Ok(id)
    }

    fn leave(&self, id: u64) {
        let mut directory = self.directory();
        if let Some(session) = directory.sessions.remove(&id) {
            let node_id = session.record["node_id"].as_str().unwrap_or_default();
            if directory.by_node.get(node_id) == Some(&id) {
                directory.by_node.remove(node_id);
            }
        }
    }

    /// Every peer this coordinator can reach downward: its own sessions'
    /// (nodes and clients) and its children's summaries (leases running).
    fn subtree(&self) -> Vec<Value> {
        let now = Instant::now();
        let directory = self.directory();
        let mut peers = Vec::new();
        for session in directory.sessions.values() {
            match session.kind {
                Kind::Node | Kind::Client => peers.push(session.record.clone()),
                Kind::Coord if session.lease_until > now => {
                    peers.extend(session.summary.iter().cloned());
                }
                Kind::Coord => {}
            }
        }
        peers.truncate(MAX_SUMMARY);
        peers
    }

    /// A node (not a client) by `node_id` or `name`: here or in a child's
    /// summary.
    fn find(&self, node_id: Option<&str>, name: Option<&str>) -> Option<Value> {
        let matches = |record: &Value| {
            record["role"] != "client"
                && record["role"] != "coord"
                && (node_id.is_some_and(|id| record["node_id"] == id)
                    || (node_id.is_none() && name.is_some_and(|name| record["name"] == name)))
        };
        let now = Instant::now();
        let directory = self.directory();
        let local = directory
            .sessions
            .values()
            .filter(|session| session.kind == Kind::Node)
            .map(|session| &session.record)
            .find(|record| matches(record))
            .cloned();
        local.or_else(|| {
            directory
                .sessions
                .values()
                .filter(|session| session.kind == Kind::Coord && session.lease_until > now)
                .flat_map(|session| session.summary.iter())
                .find(|record| matches(record))
                .cloned()
        })
    }

    /// Where a message to `to` goes from here.
    fn hop(&self, to: &str) -> Hop {
        let now = Instant::now();
        let directory = self.directory();
        if let Some(session) = directory
            .by_node
            .get(to)
            .and_then(|id| directory.sessions.get(id))
        {
            return Hop::Local(session.queue.clone());
        }
        if let Some(child) = directory.sessions.values().find(|session| {
            session.kind == Kind::Coord
                && session.lease_until > now
                && session.summary.iter().any(|peer| peer["node_id"] == to)
        }) {
            return Hop::Local(child.queue.clone());
        }
        drop(directory);
        if self.parent().is_some() {
            Hop::Parent
        } else {
            Hop::Nowhere
        }
    }

    /// Whether the session `id` may send as `from`: its own node id, or,
    /// a child coordinator's, one of its summary.
    fn may_send_as(&self, id: u64, from: &str) -> bool {
        let now = Instant::now();
        let directory = self.directory();
        directory
            .sessions
            .get(&id)
            .is_some_and(|session| match session.kind {
                Kind::Coord => {
                    session.lease_until > now
                        && session.summary.iter().any(|peer| peer["node_id"] == from)
                }
                _ => session.record["node_id"] == from,
            })
    }

    fn reply_to(&self, id: u64, message: Value) {
        if let Some(session) = self.directory().sessions.get(&id) {
            push(&session.queue, message);
        }
    }

    /// Handle one message of the session `id` (after its hello).
    pub(crate) fn handle(self: &Arc<Self>, id: u64, message: Value) {
        if let Some(tap) = &self.tap {
            tap.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(message.to_string());
        }
        match message["type"].as_str() {
            Some("ping") => self.reply_to(id, json!({"type": "pong"})),
            Some("pong") => {}
            Some("lookup") => self.lookup(id, &message),
            Some(kind @ ("relay" | "punch")) => {
                let (from, to) = (
                    message["from"].as_str().map(str::to_string),
                    message["to"].as_str().map(str::to_string),
                );
                let (Some(from), Some(to)) = (from, to) else {
                    return self.reply_to(id, json!({"type": "error", "id": message["id"],
                                                    "error": format!("a {kind} without from and to")}));
                };
                if !self.may_send_as(id, &from) {
                    return self.reply_to(id, json!({"type": "error", "id": message["id"], "to": to,
                                                    "for": from,
                                                    "error": format!("a {kind} from another node than the session's")}));
                }
                if !valid_node_id(&to) {
                    return self.reply_to(
                        id,
                        json!({"type": "error", "id": message["id"], "to": to,
                                                    "error": "no such peer"}),
                    );
                }
                self.route(id, &to, message);
            }
            Some("error") => {
                // A child coordinator's refusal of a relay, to its sender;
                // from anyone else (a node, a client) dropped: no session
                // may fail another's request.
                if self.kind_of(id) != Some(Kind::Coord) {
                    return;
                }
                if let Some(target) = message["for"].as_str() {
                    match self.hop(target) {
                        Hop::Local(queue) => push(&queue, message),
                        Hop::Parent | Hop::Nowhere => {}
                    }
                }
            }
            Some("zone_summary") => self.summary(id, &message),
            _ => self.reply_to(
                id,
                json!({"type": "error", "id": message["id"],
                                          "error": "no such message type"}),
            ),
        }
    }

    /// Pass `message` (a relay or punch, its sender checked) toward `to`.
    fn route(&self, id: u64, to: &str, message: Value) {
        match self.hop(to) {
            Hop::Local(queue) => push(&queue, message),
            Hop::Parent => {
                if let Some(parent) = self.parent() {
                    parent.send(message);
                }
            }
            Hop::Nowhere => {
                let from = message["from"].clone();
                let error = json!({"type": "error", "id": message["id"], "to": to,
                                   "for": from, "error": "no such peer"});
                self.reply_to(id, error);
            }
        }
    }

    fn lookup(self: &Arc<Self>, id: u64, message: &Value) {
        let lookup_id = message["id"].clone();
        let node_id = message["node_id"].as_str().filter(|id| valid_node_id(id));
        let name = message["name"].as_str().filter(|name| valid_name(name));
        if node_id.is_none() && name.is_none() {
            return self.reply_to(
                id,
                json!({"type": "peer_info", "id": lookup_id, "found": false,
                                            "error": "a lookup names a node id or a name"}),
            );
        }
        if let Some(peer) = self.find(node_id, name) {
            return self.reply_to(
                id,
                json!({"type": "peer_info", "id": lookup_id, "found": true,
                                            "peer": peer}),
            );
        }
        let Some(parent) = self.parent() else {
            return self.reply_to(
                id,
                json!({"type": "peer_info", "id": lookup_id, "found": false}),
            );
        };
        // Asked up the tree on a thread of its own: the session's pump
        // goes on meanwhile; at most MAX_UPWARD at once.
        if self.upward.fetch_add(1, Ordering::AcqRel) >= MAX_UPWARD {
            self.upward.fetch_sub(1, Ordering::AcqRel);
            return self.reply_to(
                id,
                json!({"type": "peer_info", "id": lookup_id, "found": false,
                       "error": "the coordinator is busy"}),
            );
        }
        let coordinator = Arc::clone(self);
        let (node_id, name) = (node_id.map(str::to_string), name.map(str::to_string));
        let spawned = std::thread::Builder::new()
            .name("coord-lookup".to_string())
            .spawn(move || {
                let found = parent.lookup(node_id.as_deref(), name.as_deref(), HELLO_TIMEOUT);
                let answer = match found {
                    Ok(Some(peer)) => json!({"type": "peer_info", "id": lookup_id, "found": true,
                                             "peer": peer}),
                    _ => json!({"type": "peer_info", "id": lookup_id, "found": false}),
                };
                coordinator.reply_to(id, answer);
                coordinator.upward.fetch_sub(1, Ordering::AcqRel);
            });
        if spawned.is_err() {
            self.upward.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// The kind of the session `id`.
    fn kind_of(&self, id: u64) -> Option<Kind> {
        self.directory()
            .sessions
            .get(&id)
            .map(|session| session.kind)
    }

    fn summary(&self, id: u64, message: &Value) {
        let peers = message["peers_summary"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let epoch = message["lease_epoch"].as_u64().unwrap_or(0);
        let lease = message["lease_ms"]
            .as_u64()
            .map_or(LEASE, Duration::from_millis)
            .clamp(Duration::from_secs(1), Duration::from_mins(10));
        let peers: Vec<Value> = peers
            .into_iter()
            .filter(|peer| peer["node_id"].as_str().is_some_and(valid_node_id) && peer.is_object())
            .take(MAX_SUMMARY)
            .map(|peer| {
                json!({"node_id": peer["node_id"], "pubkey": peer["pubkey"],
                       "x25519_pub": peer["x25519_pub"], "epoch": peer["epoch"],
                       "prekey_sig": peer["prekey_sig"], "name": peer["name"],
                       "role": peer["role"], "zone": peer["zone"],
                       "udp_reflexive": peer["udp_reflexive"]})
            })
            .collect();
        let mut directory = self.directory();
        if let Some(session) = directory.sessions.get_mut(&id)
            && session.kind == Kind::Coord
            && epoch >= session.lease_epoch
        {
            session.summary = peers;
            session.lease_epoch = epoch;
            session.lease_until = Instant::now() + lease;
        }
    }

    /// The summary this coordinator pushes up: its subtree, a new epoch.
    fn summary_message(&self) -> Value {
        let epoch = self.epoch.fetch_add(1, Ordering::Relaxed) + 1;
        json!({"type": "zone_summary", "zone": self.zone,
               "peers_summary": fit_summary(self.subtree()),
               "lease_epoch": epoch,
               "lease_ms": u64::try_from(self.lease.as_millis()).unwrap_or(30_000)})
    }

    /// Push the summary up now (a session came or went): queued on the
    /// parent's connection before anything that session sends after, so
    /// the parent knows a sender before its first relay.
    fn announce(&self) {
        if let Some(parent) = self.parent() {
            parent.send(self.summary_message());
        }
    }

    /// A message from the parent: a relay or punch down to its node, an
    /// error to its sender.
    fn take_from_parent(&self, message: Value) {
        if let Some(tap) = &self.tap {
            tap.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(message.to_string());
        }
        let target = match message["type"].as_str() {
            Some("relay" | "punch") => message["to"].as_str(),
            Some("error") => message["for"].as_str(),
            _ => None,
        };
        if let Some(target) = target
            && let Hop::Local(queue) = self.hop(target)
        {
            push(&queue, message);
        }
    }
}

/// The room a summary's peers may take in one frame: [`MAX_FRAME`] less
/// what the rest of the message needs.
const SUMMARY_ROOM: usize = MAX_FRAME - 4096;

/// The first of `peers` that fit, serialized, in [`SUMMARY_ROOM`]: a
/// summary always goes up whole in one frame (those left out are reached
/// as an unknown id is: up the tree, or not at all).
pub(crate) fn fit_summary(peers: Vec<Value>) -> Vec<Value> {
    let mut used = 2;
    peers
        .into_iter()
        .take_while(|peer| {
            used += serde_json::to_vec(peer).map_or(usize::MAX / 2, |bytes| bytes.len()) + 1;
            used <= SUMMARY_ROOM
        })
        .collect()
}

/// One session of a coordinator, on its own thread: its hello, then its
/// messages handled and its queue sent, until it closes, idles past
/// [`IDLE`], is taken over, or the coordinator stops.
fn session(
    mut channel: Channel,
    coordinator: &Arc<Coordinator>,
    bearer: bool,
    stop: &AtomicBool,
    said_hello: impl FnOnce(),
) {
    let reflexive = channel.peer().map(|peer| peer.to_string());
    let Ok(Some(hello)) = channel.recv(HELLO_TIMEOUT) else {
        return;
    };
    said_hello();
    let (kind, record) = match check_hello(&hello, coordinator.token(), bearer) {
        Ok(checked) => checked,
        Err(why) => {
            let _ = channel.send(&json!({"type": "error", "error": why}));
            return;
        }
    };
    let (queue, outbound) = std::sync::mpsc::sync_channel(QUEUE);
    let dead = Arc::new(AtomicBool::new(false));
    let id = match coordinator.join(kind, record, queue, Arc::clone(&dead)) {
        Ok(id) => id,
        Err(why) => {
            let _ = channel.send(&json!({"type": "error", "error": why}));
            return;
        }
    };
    coordinator.announce();
    let welcome = json!({"type": "welcome", "v": PROTOCOL, "zone": coordinator.zone,
                         "coordinator": coordinator.identity.node_id(), "reflexive": reflexive});
    if channel.send(&welcome).is_ok() {
        pump(&mut channel, &outbound, &dead, stop, |message| {
            coordinator.handle(id, message);
        });
    }
    coordinator.leave(id);
    coordinator.announce();
}

/// Send what `outbound` holds, take what comes (to `take`), until the
/// connection fails, `dead` or `stop` is set, or nothing comes for
/// [`IDLE`].
fn pump(
    channel: &mut Channel,
    outbound: &Receiver<Value>,
    dead: &AtomicBool,
    stop: &AtomicBool,
    mut take: impl FnMut(Value),
) {
    let mut heard = Instant::now();
    while !dead.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed) {
        while let Ok(message) = outbound.try_recv() {
            if channel.send(&message).is_err() {
                return;
            }
        }
        match channel.recv(POLL) {
            Ok(Some(message)) => {
                heard = Instant::now();
                take(message);
            }
            Ok(None) if heard.elapsed() > IDLE => return,
            Ok(None) => {}
            Err(_) => return,
        }
    }
}

/// What `spawn node coord` listens on and how it behaves.
#[derive(Debug, Default)]
pub(crate) struct CoordConfig {
    pub(crate) ws: Option<String>,
    pub(crate) tcp: Option<String>,
    pub(crate) udp: Option<String>,
    pub(crate) zone: Option<String>,
    pub(crate) parent: Option<CoordUrl>,
    pub(crate) token: Option<String>,
    pub(crate) key_file: Option<PathBuf>,
    pub(crate) lease: Option<Duration>,
}

/// The listeners and threads of a running coordinator, stopped when
/// dropped.
pub(crate) struct Running {
    pub(crate) coordinator: Arc<Coordinator>,
    pub(crate) ws: Option<SocketAddr>,
    pub(crate) tcp: Option<SocketAddr>,
    pub(crate) udp: Option<SocketAddr>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Running {
    pub(crate) fn wait(mut self) {
        for thread in std::mem::take(&mut self.threads) {
            let _ = thread.join();
        }
    }

    #[cfg(test)]
    pub(crate) fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        self.wait();
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// A bind address (`host:port`, the port 0 for the system's).
fn bind_address(text: &str) -> Result<SocketAddr, String> {
    let text = text
        .strip_prefix("ws://")
        .or_else(|| text.strip_prefix("tcp://"))
        .or_else(|| text.strip_prefix("udp://"))
        .unwrap_or(text);
    text.to_socket_addrs()
        .map_err(|error| format!("{text}: {error}"))?
        .next()
        .ok_or_else(|| format!("{text}: no address"))
}

/// One of a bounded count's slots, given back when dropped.
pub(crate) struct Held(Arc<std::sync::atomic::AtomicUsize>);

impl Held {
    /// A slot of `count`, when fewer than `most` are held.
    pub(crate) fn take(count: &Arc<std::sync::atomic::AtomicUsize>, most: usize) -> Option<Self> {
        count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                (held < most).then_some(held + 1)
            })
            .ok()
            .map(|_| Self(Arc::clone(count)))
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Accept on `listener` until `stop`, each connection's session on a
/// thread of its own (`ws`: a WebSocket upgrade first).
fn accept_loop(
    listener: TcpListener,
    ws: bool,
    coordinator: Arc<Coordinator>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<JoinHandle<()>> {
    listener.set_nonblocking(true)?;
    // Every connection's thread, and those not past their hello yet: both
    // bounded, so a flood of idle connections costs at most MAX_PENDING
    // threads for HELLO_TIMEOUT each.
    let threads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let pending = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    std::thread::Builder::new()
        .name("coord-accept".to_string())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let Ok((stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                };
                // No slot: the connection is closed at once.
                let (Some(thread_slot), Some(pending_slot)) = (
                    Held::take(&threads, MAX_SESSIONS + MAX_PENDING),
                    Held::take(&pending, MAX_PENDING),
                ) else {
                    continue;
                };
                let coordinator = Arc::clone(&coordinator);
                let stop = Arc::clone(&stop);
                let _ = std::thread::Builder::new()
                    .name("coord-session".to_string())
                    .spawn(move || {
                        let _thread_slot = thread_slot;
                        let mut pending_slot = Some(pending_slot);
                        if stream.set_nonblocking(false).is_err() {
                            return;
                        }
                        let (channel, bearer) = if ws {
                            match WsStream::accept(stream, coordinator.token(), HELLO_TIMEOUT) {
                                Ok((ws, accepted)) => (Channel::Ws(Box::new(ws)), accepted.bearer),
                                Err(_) => return,
                            }
                        } else {
                            let _ = stream.set_nodelay(true);
                            (
                                Channel::Line {
                                    stream,
                                    buffer: Vec::new(),
                                },
                                false,
                            )
                        };
                        session(channel, &coordinator, bearer, &stop, || {
                            pending_slot.take();
                        });
                    });
            }
        })
}

/// The STUN-like responder: each `{"op": "stun", "nonce"}` answered with
/// the address it came from.
fn stun_loop(socket: UdpSocket, stop: Arc<AtomicBool>) -> std::io::Result<JoinHandle<()>> {
    socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    std::thread::Builder::new()
        .name("coord-stun".to_string())
        .spawn(move || {
            let mut buffer = [0_u8; 2048];
            while !stop.load(Ordering::Relaxed) {
                let Ok((read, peer)) = socket.recv_from(&mut buffer) else {
                    continue;
                };
                let Ok(message) = parse(&buffer[..read]) else {
                    continue;
                };
                let Some(nonce) = message["nonce"].as_str().filter(|n| n.len() <= 80) else {
                    continue;
                };
                if message["op"] == "stun" {
                    let reply = json!({"op": "stun_reply", "nonce": nonce,
                                       "reflexive": peer.to_string()});
                    let _ = socket.send_to(reply.to_string().as_bytes(), peer);
                }
            }
        })
}

/// Start a coordinator as `config` says: its listeners, its STUN
/// responder, its link to its parent.
pub(crate) fn start(config: &CoordConfig) -> Result<Running, String> {
    start_with(config, None)
}

/// [`start`], every handled message kept in `tap` when given (tests).
pub(crate) fn start_with(
    config: &CoordConfig,
    tap: Option<Arc<Mutex<Vec<String>>>>,
) -> Result<Running, String> {
    let (identity, _) = NodeIdentity::load_from(config.key_file.as_deref(), None)?;
    let mut coordinator = Coordinator::new(
        Arc::new(identity),
        config.zone.clone(),
        config.token.clone(),
        config.lease.unwrap_or(LEASE),
    );
    coordinator.tap = tap;
    let coordinator = Arc::new(coordinator);
    let stop = Arc::new(AtomicBool::new(false));
    let mut threads = Vec::new();
    let mut running = Running {
        coordinator: Arc::clone(&coordinator),
        ws: None,
        tcp: None,
        udp: None,
        stop: Arc::clone(&stop),
        threads: Vec::new(),
    };
    for (bind, ws) in [(&config.ws, true), (&config.tcp, false)] {
        let Some(bind) = bind else { continue };
        let address = bind_address(bind)?;
        let listener = TcpListener::bind(address).map_err(|error| format!("{bind}: {error}"))?;
        let bound = listener.local_addr().map_err(|error| error.to_string())?;
        if ws {
            running.ws = Some(bound);
        } else {
            running.tcp = Some(bound);
        }
        threads.push(
            accept_loop(listener, ws, Arc::clone(&coordinator), Arc::clone(&stop))
                .map_err(|error| error.to_string())?,
        );
    }
    if let Some(bind) = &config.udp {
        let socket =
            UdpSocket::bind(bind_address(bind)?).map_err(|error| format!("{bind}: {error}"))?;
        running.udp = Some(socket.local_addr().map_err(|error| error.to_string())?);
        threads.push(stun_loop(socket, Arc::clone(&stop)).map_err(|error| error.to_string())?);
    }
    if let Some(parent) = &config.parent {
        let parent = parent.clone();
        let coordinator = Arc::clone(&coordinator);
        let stop = Arc::clone(&stop);
        threads.push(
            std::thread::Builder::new()
                .name("coord-uplink".to_string())
                .spawn(move || uplink(&parent, &coordinator, &stop))
                .map_err(|error| error.to_string())?,
        );
    }
    running.threads = threads;
    Ok(running)
}

/// A child coordinator's link to its parent: connected again with a
/// backoff, its summary pushed at a third of its lease, what the parent
/// sends down delivered.
fn uplink(parent: &CoordUrl, coordinator: &Arc<Coordinator>, stop: &AtomicBool) {
    let mut backoff = Duration::from_millis(200);
    while !stop.load(Ordering::Relaxed) {
        let fields = HelloFields {
            kind: Kind::Coord,
            name: format!("coord-{}", coordinator.zone.as_deref().unwrap_or("root")),
            role: "coord".to_string(),
            zone: coordinator.zone.clone(),
            udp_reflexive: None,
        };
        let client = CoordClient::connect(
            parent,
            Arc::clone(&coordinator.identity),
            TrustPolicy::default(),
            &fields,
            coordinator.token(),
            HELLO_TIMEOUT,
            None,
        );
        let Ok(client) = client else {
            sleep_unless(stop, backoff);
            backoff = (backoff * 2).min(Duration::from_secs(30));
            continue;
        };
        let client = Arc::new(client);
        backoff = Duration::from_millis(200);
        *coordinator
            .parent
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&client));
        let refresh = coordinator.lease / 3;
        let mut pushed: Option<Instant> = None;
        while client.alive() && !stop.load(Ordering::Relaxed) {
            if pushed.is_none_or(|at| at.elapsed() >= refresh) {
                client.send(coordinator.summary_message());
                pushed = Some(Instant::now());
            }
            if let Some(message) = client.next_incoming(Duration::from_millis(50)) {
                coordinator.take_from_parent(message);
            }
        }
        *coordinator
            .parent
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        client.close();
    }
}

fn sleep_unless(stop: &AtomicBool, wait: Duration) {
    let until = Instant::now() + wait;
    while Instant::now() < until && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A client of a coordinator: one connection, pumped on a thread of its
/// own; its lookups and sealed requests wait for their answers, anything
/// else (a request to this node, a punch) is incoming.
pub(crate) struct CoordClient {
    identity: Arc<NodeIdentity>,
    policy: TrustPolicy,
    queue: SyncSender<Value>,
    waiters: Arc<Mutex<HashMap<String, Sender<Value>>>>,
    incoming: Mutex<Receiver<Value>>,
    alive: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    /// The coordinator's welcome.
    pub(crate) welcome: Value,
    thread: Mutex<Option<JoinHandle<()>>>,
}

/// What a waiter is keyed by: a lookup's id, or a sealed request's peer
/// and nonce.
fn waiter_key(message: &Value) -> Option<String> {
    match message["type"].as_str()? {
        "peer_info" => Some(format!("lookup:{}", message["id"].as_str()?)),
        "relay" => {
            let envelope = &message["envelope"];
            matches!(
                envelope["v"].as_str(),
                Some(crypto::E2E_REPLY_DOMAIN | "spawn-e2e-v1-error")
            )
            .then(|| {
                Some(format!(
                    "reply:{}:{}",
                    message["from"].as_str()?,
                    envelope["req_nonce"].as_str()?
                ))
            })
            .flatten()
        }
        "error" => Some(format!(
            "reply:{}:{}",
            message["to"].as_str()?,
            message["id"].as_str()?
        )),
        _ => None,
    }
}

impl CoordClient {
    /// Connect to `url`, say a hello of `identity` (`fields`; with
    /// `coord_token` as bearer and `coord_auth`), wait for the welcome,
    /// then pump on a thread of its own.
    pub(crate) fn connect(
        url: &CoordUrl,
        identity: Arc<NodeIdentity>,
        policy: TrustPolicy,
        fields: &HelloFields,
        coord_token: Option<&str>,
        timeout: Duration,
        tls: Option<Arc<rustls::ClientConfig>>,
    ) -> Result<Self, String> {
        let deadline = Instant::now() + timeout;
        let mut channel = Channel::open(url, coord_token, timeout, tls)?;
        channel.send(&hello(&identity, fields, coord_token))?;
        let welcome = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!("{url}: no welcome in time"));
            }
            match channel.recv(left)? {
                Some(message) if message["type"] == "welcome" => break message,
                Some(message) if message["type"] == "error" => {
                    return Err(format!(
                        "{url} refused the hello: {}",
                        crypto::said(&message["error"])
                    ));
                }
                _ => {}
            }
        };
        let (queue, outbound) = std::sync::mpsc::sync_channel(QUEUE);
        // Bounded: a coordinator flooding a client fills its queue, never
        // its memory (more is dropped).
        let (incoming_in, incoming) = std::sync::mpsc::sync_channel(QUEUE);
        let waiters: Arc<Mutex<HashMap<String, Sender<Value>>>> = Arc::default();
        let alive = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (waiters, alive, stop) =
                (Arc::clone(&waiters), Arc::clone(&alive), Arc::clone(&stop));
            std::thread::Builder::new()
                .name("coord-client".to_string())
                .spawn(move || {
                    let dispatch = |message: Value| {
                        let waiter = waiter_key(&message).and_then(|key| {
                            waiters
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .remove(&key)
                        });
                        match waiter {
                            Some(waiter) => {
                                let _ = waiter.send(message);
                            }
                            None => {
                                let _ = incoming_in.try_send(message);
                            }
                        }
                    };
                    client_pump(&mut channel, &outbound, &stop, dispatch);
                    alive.store(false, Ordering::Relaxed);
                    if let Channel::Ws(ws) = &mut channel {
                        ws.close();
                    }
                })
                .map_err(|error| error.to_string())?
        };
        Ok(Self {
            identity,
            policy,
            queue,
            waiters,
            incoming: Mutex::new(incoming),
            alive,
            stop,
            welcome,
            thread: Mutex::new(Some(thread)),
        })
    }

    pub(crate) fn alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    pub(crate) fn identity(&self) -> &Arc<NodeIdentity> {
        &self.identity
    }

    /// Queue `message` for the coordinator.
    pub(crate) fn send(&self, message: Value) {
        push(&self.queue, message);
    }

    /// The next incoming message within `timeout`.
    pub(crate) fn next_incoming(&self, timeout: Duration) -> Option<Value> {
        self.incoming
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .recv_timeout(timeout)
            .ok()
    }

    /// Send `message` and wait for the answer keyed `key`.
    fn ask(&self, key: &str, message: Value, timeout: Duration) -> Result<Value, String> {
        let (answer, wait) = std::sync::mpsc::channel();
        self.waiters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.to_string(), answer);
        self.send(message);
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match wait.recv_timeout(left.min(Duration::from_millis(100))) {
                Ok(answer) => return Ok(answer),
                Err(_) if left.is_zero() => break,
                Err(_) if !self.alive() => break,
                Err(_) => {}
            }
        }
        self.waiters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(key);
        Err(if self.alive() {
            "no answer through the coordinator in time".to_string()
        } else {
            "the coordinator's connection closed".to_string()
        })
    }

    /// A node's public record by `node_id` or `name`, or none.
    pub(crate) fn lookup(
        &self,
        node_id: Option<&str>,
        name: Option<&str>,
        timeout: Duration,
    ) -> Result<Option<Value>, String> {
        let id = nonce();
        let answer = self.ask(
            &format!("lookup:{id}"),
            json!({"type": "lookup", "id": id, "node_id": node_id, "name": name}),
            timeout,
        )?;
        Ok((answer["found"] == true).then(|| answer["peer"].clone()))
    }

    /// `payload` sealed to `peer`, relayed, its sealed reply opened.
    pub(crate) fn request(
        &self,
        peer: &PeerKeys,
        payload: &Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let (envelope, key) = seal_request(&self.identity, peer, &self.policy, &nonce(), payload)?;
        let req_nonce = envelope["req_nonce"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let answer = self.ask(
            &format!("reply:{}:{req_nonce}", peer.node_id),
            json!({"type": "relay", "from": self.identity.node_id(), "to": peer.node_id,
                   "id": req_nonce, "envelope": envelope}),
            timeout,
        )?;
        if answer["type"] == "error" {
            return Err(format!(
                "the coordinator: {}",
                crypto::said(&answer["error"])
            ));
        }
        key.open(&answer["envelope"])
    }

    /// Stop the pump and wait for it.
    pub(crate) fn close(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = thread.join();
        }
    }
}

impl Drop for CoordClient {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// A client's pump: what `outbound` holds sent, a ping each
/// [`PING_EVERY`], what comes handed to `take`, until the connection
/// fails, `stop` is set, or nothing comes for [`IDLE`] and a ping.
fn client_pump(
    channel: &mut Channel,
    outbound: &Receiver<Value>,
    stop: &AtomicBool,
    mut take: impl FnMut(Value),
) {
    let (mut pinged, mut heard) = (Instant::now(), Instant::now());
    while !stop.load(Ordering::Relaxed) {
        while let Ok(message) = outbound.try_recv() {
            if channel.send(&message).is_err() {
                return;
            }
        }
        if pinged.elapsed() >= PING_EVERY {
            if channel.send(&json!({"type": "ping"})).is_err() {
                return;
            }
            pinged = Instant::now();
        }
        match channel.recv(POLL) {
            Ok(Some(message)) => {
                heard = Instant::now();
                take(message);
            }
            Ok(None) if heard.elapsed() > IDLE + PING_EVERY => return,
            Ok(None) => {}
            Err(_) => return,
        }
    }
}

/// A STUN-like ask of `server` on `socket`: this socket's address as the
/// server sees it.
pub(crate) fn stun(
    socket: &UdpSocket,
    server: SocketAddr,
    timeout: Duration,
) -> Result<SocketAddr, String> {
    let nonce = nonce();
    let request = json!({"op": "stun", "nonce": nonce}).to_string();
    let deadline = Instant::now() + timeout;
    let mut buffer = [0_u8; 2048];
    let mut sent: Option<Instant> = None;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(format!("{server}: no stun reply in time"));
        }
        if sent.is_none_or(|at| at.elapsed() >= Duration::from_millis(300)) {
            socket
                .send_to(request.as_bytes(), server)
                .map_err(|error| format!("{server}: {error}"))?;
            sent = Some(Instant::now());
        }
        socket
            .set_read_timeout(Some(left.min(Duration::from_millis(100))))
            .map_err(|error| error.to_string())?;
        if let Ok((read, from)) = socket.recv_from(&mut buffer)
            && from == server
            && let Ok(reply) = parse(&buffer[..read])
            && reply["op"] == "stun_reply"
            && reply["nonce"] == nonce.as_str()
            && let Some(reflexive) = reply["reflexive"].as_str().and_then(|r| r.parse().ok())
        {
            return Ok(reflexive);
        }
    }
}

/// What `spawn node connect` was told.
#[derive(Debug, Clone)]
pub(crate) struct ConnectConfig {
    pub(crate) url: CoordUrl,
    pub(crate) name: String,
    pub(crate) role: String,
    pub(crate) zone: Option<String>,
    pub(crate) key_file: Option<PathBuf>,
    pub(crate) udp_stun: Option<String>,
    pub(crate) register_to: Option<String>,
    pub(crate) register_store: bool,
    pub(crate) note: Option<String>,
    pub(crate) coord_token: Option<String>,
    /// Whether a punch may go to a loopback or link-local mapping (tests
    /// on one host; never a flag of the command).
    pub(crate) punch_loopback: bool,
}

/// A connected node's end: its identity's responder, its probes, its
/// registrar (`--register-store`).
pub(crate) struct NodeService {
    pub(crate) responder: Responder,
    pub(crate) runner: Runner,
    pub(crate) registrar: Option<Registrar>,
    /// The sealed requests being answered ([`MAX_REQUESTS`] at most).
    pub(crate) in_flight: Arc<std::sync::atomic::AtomicUsize>,
}

impl NodeService {
    /// Answer an envelope sealed to this node: its probe run, its
    /// registration recorded; the reply sealed (or a refusal of an
    /// envelope that does not open).
    pub(crate) fn answer(&self, envelope: &Value) -> Value {
        let opened = match self.responder.open(envelope) {
            Ok(opened) => opened,
            Err(why) => return envelope_error(&why, envelope),
        };
        let reply = match self.request(&opened) {
            Ok(Value::Object(mut fields)) => {
                fields.insert("ok".into(), json!(true));
                Value::Object(fields)
            }
            Ok(_) => json!({"ok": true}),
            Err(error) => json!({"ok": false, "error": spawn_core::redact::redact(&error)}),
        };
        opened
            .reply
            .seal(&reply)
            .unwrap_or_else(|why| envelope_error(&why, envelope))
    }

    fn request(&self, opened: &crypto::Opened) -> Result<Value, String> {
        let request = &opened.request;
        match request["op"].as_str() {
            Some(op @ ("inventory" | "beat")) => {
                let output = (self.runner)(op)?;
                if output.len() > usize::try_from(OUTPUT_LIMIT).unwrap_or(usize::MAX) {
                    return Err("the probe's output is too long".to_string());
                }
                Ok(json!({"output": output}))
            }
            Some("register") => {
                let registrar = self.registrar.as_ref().ok_or(
                    "this node takes no registration (spawn node connect --register-store)",
                )?;
                if self.responder.policy().token().is_none() {
                    return Err(
                        "this node takes no registration without SPAWN_NODE_TOKEN".to_string()
                    );
                }
                let record = coord_registration(request, &opened.from, &opened.from_pub)?;
                let event = registrar(&record).map_err(|error| store_refusal(&error))?;
                Ok(json!({"event": event}))
            }
            _ => Err("no such op".to_string()),
        }
    }
}

/// The `node.joined` a `register` through a coordinator asks for: its
/// identity is the envelope's sender's (`from`, `from_pub`), its endpoint
/// a coordinator's URL, every field checked as `spawn_node_record` checks
/// it, its note and inventory masked.
pub(crate) fn coord_registration(
    request: &Value,
    from: &str,
    from_pub: &str,
) -> Result<Value, String> {
    let name = request["name"]
        .as_str()
        .filter(|name| valid_name(name))
        .ok_or("a registration's name is 1 to 63 letters, digits, dots, dashes or underscores")?;
    if request["transport"] != "coord" {
        return Err("a registration through a coordinator is a coord node's".to_string());
    }
    if request["node_id"] != from || request["pubkey"] != from_pub {
        return Err("a registration names another node than its sender".to_string());
    }
    let url = CoordUrl::parse(request["endpoint"].as_str().unwrap_or_default())?;
    let mode = match request["mode"].as_str() {
        None | Some("relay") => "relay",
        Some("hole_punch") => "hole_punch",
        Some(_) => return Err("a coord node's mode is relay or hole_punch".to_string()),
    };
    let role = match request["role"].as_str() {
        None | Some("worker") => "worker",
        Some("peer") => "peer",
        Some(_) => return Err("a node's role is worker or peer".to_string()),
    };
    let zone = match &request["zone"] {
        Value::Null => Value::Null,
        Value::String(zone) if valid_zone(zone) => json!(zone),
        _ => {
            return Err(
                "a node's zone is 1 to 63 letters, digits, dots, dashes or underscores".to_string(),
            );
        }
    };
    let note = match &request["note"] {
        Value::Null => Value::Null,
        Value::String(note)
            if note.chars().count() <= 500 && !note.chars().any(char::is_control) =>
        {
            json!(spawn_core::redact::redact(note))
        }
        _ => return Err("a registration's note is at most 500 printable characters".to_string()),
    };
    let output = request["inventory"]
        .as_str()
        .filter(|output| output.len() <= usize::try_from(OUTPUT_LIMIT).unwrap_or(usize::MAX))
        .ok_or("a registration without its inventory")?;
    let mut record = inventory(&spawn_core::redact::redact(output))?;
    for (key, value) in [
        ("type", json!("node.joined")),
        ("name", json!(name)),
        ("ssh", Value::Null),
        ("transport", json!("coord")),
        ("endpoint", json!(url.url())),
        ("via", json!([])),
        ("mode", json!(mode)),
        ("note", note),
        ("exit_country", Value::Null),
        ("node_id", json!(from)),
        ("pubkey", json!(from_pub)),
        ("role", json!(role)),
        ("zone", zone),
    ] {
        record[key] = value;
    }
    Ok(record)
}

/// A connected node's udp socket: it answers a sealed request sent
/// straight to it (a punched path), and a punch probe.
fn udp_service(
    socket: UdpSocket,
    service: Arc<NodeService>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<JoinHandle<()>> {
    socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    std::thread::Builder::new()
        .name("node-punch".to_string())
        .spawn(move || {
            let mut buffer = vec![0_u8; 65_536];
            while !stop.load(Ordering::Relaxed) {
                let Ok((read, peer)) = socket.recv_from(&mut buffer) else {
                    continue;
                };
                let Ok(message) = parse(&buffer[..read]) else {
                    continue;
                };
                if message["op"] == "e2e" {
                    let reply = json!({"op": "e2e_reply", "envelope": service.answer(&message["envelope"])});
                    let bytes = reply.to_string();
                    if bytes.len() <= super::transport::UDP_FRAME {
                        let _ = socket.send_to(bytes.as_bytes(), peer);
                    }
                }
            }
        })
}

/// What a connector says it did, line by line (the command prints them;
/// tests read them).
pub(crate) type Say = Arc<dyn Fn(String) + Send + Sync>;

/// `spawn node connect`'s loop: connected (again, with a backoff), its
/// hello said, each request answered on a thread of its own, its
/// registration sent once, until `stop`.
pub(crate) fn run_connector(
    config: &ConnectConfig,
    identity: &Arc<NodeIdentity>,
    service: &Arc<NodeService>,
    stop: &Arc<AtomicBool>,
    tls: Option<&Arc<rustls::ClientConfig>>,
    say: &Say,
) -> Result<(), String> {
    let mut udp_reflexive = None;
    let mut udp_thread = None;
    let mut punch_socket = None;
    if let Some(server) = &config.udp_stun {
        let server = bind_address(server)?;
        let local = if server.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };
        let socket = UdpSocket::bind(local).map_err(|error| error.to_string())?;
        let reflexive = stun(&socket, server, Duration::from_secs(5))?;
        say(format!("node connect: udp reflexive {reflexive}"));
        udp_reflexive = Some(reflexive.to_string());
        // The punches go out of the very socket the mapping is of.
        punch_socket = Some(Puncher::new(
            socket.try_clone().map_err(|error| error.to_string())?,
            config.punch_loopback,
        ));
        udp_thread = Some(
            udp_service(socket, Arc::clone(service), Arc::clone(stop))
                .map_err(|error| error.to_string())?,
        );
    }
    let fields = HelloFields {
        kind: Kind::Node,
        name: config.name.clone(),
        role: config.role.clone(),
        zone: config.zone.clone(),
        udp_reflexive: udp_reflexive.clone(),
    };
    let mut registered = config.register_to.is_none();
    let mut backoff = Duration::from_millis(250);
    let mut rotated = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        if rotated.elapsed() >= PREKEY_LIFETIME {
            identity.rotate()?;
            rotated = Instant::now();
        }
        let client = CoordClient::connect(
            &config.url,
            Arc::clone(identity),
            service.responder.policy().clone(),
            &fields,
            config.coord_token.as_deref(),
            HELLO_TIMEOUT,
            tls.cloned(),
        );
        let Ok(client) = client.map_err(|why| {
            say(format!(
                "node connect: {why}; again in {} ms",
                backoff.as_millis()
            ));
        }) else {
            sleep_unless(stop, backoff);
            backoff = (backoff * 2).min(Duration::from_mins(1));
            continue;
        };
        let client = Arc::new(client);
        backoff = Duration::from_millis(250);
        say(format!(
            "node connect: {} connected to {} (zone {}) as {}",
            config.name,
            config.url,
            client.welcome["zone"].as_str().unwrap_or("-"),
            identity.node_id()
        ));
        if !registered {
            let client = Arc::clone(&client);
            let config = config.clone();
            let say = Arc::clone(say);
            let mode = if udp_reflexive.is_some() {
                "hole_punch"
            } else {
                "relay"
            };
            // On a thread of its own: the client's incoming requests are
            // answered meanwhile (the primary's preflight among them).
            std::thread::Builder::new()
                .name("node-register".to_string())
                .spawn(move || register_through(&client, &config, mode, &say))
                .map_err(|error| error.to_string())?;
            registered = true;
        }
        while client.alive() && !stop.load(Ordering::Relaxed) && rotated.elapsed() < PREKEY_LIFETIME
        {
            if let Some(message) = client.next_incoming(Duration::from_millis(100)) {
                take_incoming(message, &client, service, punch_socket.as_ref());
            }
        }
        client.close();
        if !stop.load(Ordering::Relaxed) {
            say("node connect: the coordinator's connection closed; connecting again".to_string());
        }
    }
    if let Some(thread) = udp_thread {
        let _ = thread.join();
    }
    Ok(())
}

/// The most punches a node sends in [`PUNCH_WINDOW`], and the most to
/// one address.
const PUNCH_GLOBAL: usize = 16;
const PUNCH_PER_TARGET: usize = 1;
const PUNCH_WINDOW: Duration = Duration::from_secs(2);

/// A node's answer to `punch`: one fixed, small datagram (`{"op":
/// "punch_probe", "nonce"}`, the nonce a checked one) from its mapped udp
/// socket to the asker's announced mapping, so its NAT lets the asker's
/// datagrams in. Never to a loopback, unspecified, link-local, broadcast
/// or multicast address (unless a test allows loopback), never more than
/// [`PUNCH_PER_TARGET`] to one address nor [`PUNCH_GLOBAL`] in all per
/// [`PUNCH_WINDOW`]: a punch cannot make a node a reflector.
pub(crate) struct Puncher {
    socket: UdpSocket,
    allow_loopback: bool,
    sent: Mutex<Vec<(Instant, IpAddr)>>,
}

impl Puncher {
    pub(crate) fn new(socket: UdpSocket, allow_loopback: bool) -> Self {
        Self {
            socket,
            allow_loopback,
            sent: Mutex::new(Vec::new()),
        }
    }

    /// Answer `message` (a punch): whether a datagram was sent.
    pub(crate) fn punch(&self, message: &Value) -> bool {
        self.admit(message)
            .is_some_and(|(target, probe)| self.socket.send_to(&probe, target).is_ok())
    }

    /// Whether `message` may be answered, and with what to where: its
    /// target and the fixed probe (the send counted against the limits).
    pub(crate) fn admit(&self, message: &Value) -> Option<(SocketAddr, Vec<u8>)> {
        let nonce = message["nonce"]
            .as_str()
            .filter(|nonce| valid_nonce(nonce))?;
        let target = message["reflexive"]
            .as_str()
            .filter(|text| text.len() <= 64)
            .and_then(|text| text.parse::<SocketAddr>().ok())?;
        let ip = target.ip();
        if target.port() == 0 || (forbidden(ip) && !(self.allow_loopback && ip.is_loopback())) {
            return None;
        }
        {
            let mut sent = self.sent.lock().unwrap_or_else(PoisonError::into_inner);
            sent.retain(|(at, _)| at.elapsed() < PUNCH_WINDOW);
            if sent.len() >= PUNCH_GLOBAL
                || sent.iter().filter(|(_, to)| *to == ip).count() >= PUNCH_PER_TARGET
            {
                return None;
            }
            sent.push((Instant::now(), ip));
        }
        let probe = json!({"op": "punch_probe", "nonce": nonce}).to_string();
        Some((target, probe.into_bytes()))
    }
}

/// One message to a connected node: a sealed request answered on a
/// thread of its own; a punch answered by `punch` (the node's mapped
/// socket).
fn take_incoming(
    message: Value,
    client: &Arc<CoordClient>,
    service: &Arc<NodeService>,
    punch: Option<&Puncher>,
) {
    match message["type"].as_str() {
        Some("relay") if message["envelope"]["v"] == crypto::E2E_DOMAIN => {
            let envelope = &message["envelope"];
            // At most MAX_REQUESTS at once: one more is refused (in the
            // clear: it says nothing a relay could not see).
            let Some(slot) = Held::take(&service.in_flight, MAX_REQUESTS) else {
                client.send(json!({
                    "type": "relay", "from": client.identity().node_id(),
                    "to": envelope["from"], "id": envelope["req_nonce"],
                    "envelope": envelope_error("this node is busy", envelope),
                }));
                return;
            };
            let (client, service) = (Arc::clone(client), Arc::clone(service));
            let _ = std::thread::Builder::new()
                .name("node-request".to_string())
                .spawn(move || {
                    let _slot = slot;
                    let envelope = &message["envelope"];
                    let reply = service.answer(envelope);
                    client.send(json!({
                        "type": "relay", "from": client.identity().node_id(),
                        "to": envelope["from"], "id": envelope["req_nonce"], "envelope": reply,
                    }));
                });
        }
        Some("punch") => {
            if let Some(puncher) = punch {
                puncher.punch(&message);
            }
        }
        _ => {}
    }
}

/// Join this node to the primary `config.register_to` (a name or node
/// id) through the coordinator: its record sealed to the primary, which
/// records it (`spawn node connect --register-store` there).
fn register_through(client: &CoordClient, config: &ConnectConfig, mode: &str, say: &Say) {
    let outcome = (|| -> Result<i64, String> {
        let target = config.register_to.as_deref().unwrap_or_default();
        let (by_id, by_name) = if valid_node_id(target) {
            (Some(target), None)
        } else {
            (None, Some(target))
        };
        let info = client
            .lookup(by_id, by_name, HELLO_TIMEOUT)?
            .ok_or_else(|| format!("the coordinator knows no {target}"))?;
        let peer = PeerKeys::verify(&info, by_id, None)?;
        let output = super::bounded(
            super::transport::local_command(super::INVENTORY),
            super::JOIN_TIMEOUT,
        )
        .map_err(|error| format!("this node's inventory: {error}"))?;
        inventory(&output).map_err(|error| format!("this node's inventory: {error}"))?;
        let identity = client.identity();
        let payload = json!({
            "op": "register", "name": config.name, "transport": "coord",
            "endpoint": config.url.url(), "mode": mode, "via": [], "note": config.note,
            "inventory": output, "node_id": identity.node_id(), "pubkey": identity.pubkey_b64(),
            "role": config.role, "zone": config.zone,
        });
        let reply =
            super::transport::accepted(client.request(&peer, &payload, super::JOIN_TIMEOUT)?)?;
        reply["event"]
            .as_i64()
            .ok_or_else(|| "no event in the reply".to_string())
    })();
    match outcome {
        Ok(event) => say(format!("node {} registered (event {event})", config.name)),
        Err(why) => say(format!(
            "node {}: the registration failed: {why}",
            config.name
        )),
    }
}

/// What a probe through a coordinator needs beside its target.
pub(crate) struct ProbeContext {
    pub(crate) identity: Arc<NodeIdentity>,
    pub(crate) policy: TrustPolicy,
    pub(crate) coord_token: Option<String>,
    pub(crate) tls: Option<Arc<rustls::ClientConfig>>,
    /// Whether a punched path may go to a loopback or link-local mapping
    /// (tests on one host).
    pub(crate) allow_loopback: bool,
    /// Whether to try a punched udp path first.
    pub(crate) punch: bool,
}

impl ProbeContext {
    /// This host's: an identity of this probe's own (a fresh key every
    /// time, never `SPAWN_NODE_KEY_FILE`'s or `SPAWN_NODE_KEY`'s: a probe
    /// must never take a connector's session at the coordinator; its
    /// `node_id` names no join, so no key can be pinned to it), `token`,
    /// `SPAWN_NODE_TOKEN_PREV`, `SPAWN_NODE_REVOKED`, `SPAWN_COORD_TOKEN`.
    pub(crate) fn from_env(token: Option<&str>) -> Result<Self, String> {
        let env = crate::config::env_value;
        Ok(Self {
            identity: Arc::new(NodeIdentity::generate()?.0),
            policy: TrustPolicy::new(
                token,
                env("SPAWN_NODE_TOKEN_PREV").as_deref(),
                env("SPAWN_NODE_REVOKED").as_deref(),
            ),
            coord_token: env("SPAWN_COORD_TOKEN"),
            tls: None,
            allow_loopback: false,
            punch: true,
        })
    }
}

/// A probe's answer through a coordinator: the reply, the node's public
/// record, and the way it took (`hole_punch`: straight over udp, or
/// `relay`).
#[derive(Debug)]
pub(crate) struct Probed {
    pub(crate) reply: Value,
    pub(crate) peer: Value,
    pub(crate) mode: &'static str,
}

/// Ask `payload` of the node `node_id` (or, without one, named `name`)
/// through the coordinator at `url`: its key checked against `pinned`
/// (its join's), sealed end to end; over a punched udp path first when
/// the node has a udp mapping (a probe only), else, or when that gives no
/// answer within [`PUNCH_WAIT`], relayed.
pub(crate) fn ask(
    url: &CoordUrl,
    node_id: Option<&str>,
    name: Option<&str>,
    pinned: Option<&str>,
    payload: &Value,
    timeout: Duration,
    context: &ProbeContext,
) -> Result<Probed, String> {
    let deadline = Instant::now() + timeout;
    let fields = HelloFields {
        kind: Kind::Client,
        name: "spawn-probe".to_string(),
        role: "client".to_string(),
        zone: None,
        udp_reflexive: None,
    };
    let connect = |identity: Arc<NodeIdentity>| {
        CoordClient::connect(
            url,
            identity,
            context.policy.clone(),
            &fields,
            context.coord_token.as_deref(),
            timeout.min(HELLO_TIMEOUT),
            context.tls.clone(),
        )
    };
    // A probe's key that is a connected node's (a key file shared with a
    // connector) is refused, never takes that node's session: asked again
    // with a key of this probe's own.
    let client = match connect(Arc::clone(&context.identity)) {
        Err(why) if why.contains(ID_IN_USE) => connect(Arc::new(NodeIdentity::generate()?.0))?,
        other => other?,
    };
    let outcome = (|| {
        let left = || deadline.saturating_duration_since(Instant::now());
        let info = client
            .lookup(node_id, name, left())?
            .ok_or_else(|| format!("{url} knows no such node (not connected)"))?;
        let peer = PeerKeys::verify(&info, node_id, pinned)?;
        let probe = matches!(payload["op"].as_str(), Some("inventory" | "beat"));
        if context.punch
            && probe
            && let Some(reflexive) = info["udp_reflexive"]
                .as_str()
                .and_then(|r| r.parse::<SocketAddr>().ok())
            && (context.allow_loopback || !forbidden(reflexive.ip()))
        {
            // Straight to the node's announced mapping: this reaches a
            // node that is public, or behind a NAT that lets in datagrams
            // to a mapping from anyone (endpoint-independent filtering).
            // No punch is asked: this prober announces no mapping of its
            // own for the node to open toward.
            if let Ok(reply) = punched(&client, &peer, reflexive, payload, PUNCH_WAIT.min(left())) {
                return Ok(Probed {
                    reply,
                    peer: info,
                    mode: "hole_punch",
                });
            }
        }
        let reply = client.request(&peer, payload, left())?;
        Ok(Probed {
            reply,
            peer: info,
            mode: "relay",
        })
    })();
    client.close();
    outcome
}

/// One sealed request straight to `reflexive` over udp, its reply within
/// `wait`.
fn punched(
    client: &CoordClient,
    peer: &PeerKeys,
    reflexive: SocketAddr,
    payload: &Value,
    wait: Duration,
) -> Result<Value, String> {
    let (envelope, key) = seal_request(client.identity(), peer, &client.policy, &nonce(), payload)?;
    let socket = UdpSocket::bind(if reflexive.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    })
    .map_err(|error| error.to_string())?;
    socket
        .connect(reflexive)
        .map_err(|error| error.to_string())?;
    let datagram = json!({"op": "e2e", "envelope": envelope}).to_string();
    if datagram.len() > super::transport::UDP_FRAME {
        return Err("too long for udp".to_string());
    }
    let deadline = Instant::now() + wait;
    let mut buffer = vec![0_u8; 65_536];
    let mut sends = 0;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("no punched answer".to_string());
        }
        if sends < 3 {
            socket
                .send(datagram.as_bytes())
                .map_err(|error| error.to_string())?;
            sends += 1;
        }
        socket
            .set_read_timeout(Some(left.min(Duration::from_millis(200))))
            .map_err(|error| error.to_string())?;
        match socket.recv(&mut buffer) {
            Ok(read) => {
                if let Ok(reply) = parse(&buffer[..read])
                    && reply["op"] == "e2e_reply"
                    && reply["envelope"]["req_nonce"] == envelope["req_nonce"]
                {
                    return key.open(&reply["envelope"]);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                return Err("the punched port refused".to_string());
            }
            Err(_) => {}
        }
    }
}

/// The probe `op` of the coord node `node_id` through `url` (its key
/// pinned to `pubkey` when its join holds one), with this host's identity
/// and `token`: the reply.
pub(crate) fn probe(
    url: &str,
    node_id: &str,
    pubkey: Option<&str>,
    op: Op,
    timeout: Duration,
    token: Option<&str>,
) -> Result<Value, String> {
    let url = CoordUrl::parse(url)?;
    let context = ProbeContext::from_env(token)?;
    let probed = ask(
        &url,
        Some(node_id),
        None,
        pubkey,
        &json!({"op": op.name()}),
        timeout,
        &context,
    )?;
    super::transport::accepted(probed.reply)
}

#[cfg(test)]
mod tests;
