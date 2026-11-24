//! A node's own end of the tcp and udp transports (issue #85): `spawn node
//! serve` answers the primary's probes, relays `forward`s along a chain of
//! nodes (tcp and udp mixed as each hop's scheme says), and, with
//! `--register-store`, takes the `register` of a node that joins itself
//! from behind a NAT or a firewall (`spawn node register`), directly or
//! relayed. The wire and its bounds are super::transport's.
//!
//! The trust boundary:
//!
//! - With `SPAWN_NODE_TOKEN` set, every request needs its `auth`
//!   (super::auth: HMAC-SHA256 over what it acts on, never the token
//!   itself, within two minutes of this node's clock); a `register` always
//!   does, so `--register-store` without a token is refused at start.
//! - Each nonce is handled once ([`Relay::admit`], before a request takes
//!   any slot): a request sent again while the first is in flight is
//!   dropped (udp sends again; a tcp peer is told), and one sent again
//!   after is answered from the cache, for [`SEEN_TTL`] (at least the
//!   auth's skew), so a udp resend never runs a probe, a relay loop or a
//!   registration twice.
//! - A probe runs [`super::INVENTORY`] or [`super::BEAT`] and nothing else,
//!   whatever the message says. A relay sends on a request rebuilt from its
//!   allowed fields only (a probe or a `register`; any other key refused),
//!   to hops that are tcp or udp endpoints (at most [`MAX_HOPS`], none
//!   twice, less time than it was given), and connects only to addresses
//!   it resolved and checked: never a loopback, unspecified, link-local,
//!   broadcast or multicast one (IPv4-mapped included) unless
//!   `--allow-loopback-forward`. Every failure to reach the next hop is
//!   said in the same words, so a forward is no port scanner.
//! - A `register` is checked field by field as `spawn_node_record` checks
//!   it, its note and the node's output masked.
//! - At most [`MAX_ACTIVE`] requests at once per listener, [`PER_PEER`] of
//!   them from one address; a tcp peer has [`REQUEST_TIMEOUT`] to send its
//!   request.
//! - Issue #87: a sealed request (`op: "sealed"`, super::crypto's wire
//!   seal) is opened with the token, or the one before it
//!   (`SPAWN_NODE_TOKEN_PREV`, [`Relay::with_previous_token`]), answered
//!   as its inner request would be, and its answer sealed back
//!   (`sealed_reply`, its own salt and nonce each); a forward's sealed
//!   inner request is passed on as it came, unread. The seal is the
//!   fleet's shared token's: it keeps a passive reader of the path out,
//!   not a relay that holds the token (a fleet member could open what it
//!   forwards). Zero-trust relaying is the coordinators' `spawn-e2e-v1`
//!   alone (super::coord). An answer is kept against a resend until the request's
//!   own `at_ms` can no longer pass the auth ([`keep_for`]), however fast
//!   its sender's clock. A store's refusal of a registration is said in
//!   its own words only when they are the writer's, never SQL
//!   ([`store_refusal`]).

use std::collections::HashMap;
use std::net::{IpAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use super::auth;
use super::transport::{
    Endpoint, MAX_HOPS, Mode, TCP_FRAME, Transport, UDP_FRAME, exchange_at, exchange_sealed,
    forbidden, forward, hop_names, local_command, parse, read_line, valid_nonce,
};
use super::{JOIN_TIMEOUT, OUTPUT_LIMIT, PROBE_TIMEOUT, bounded, inventory, valid_name};

/// What a node runs for a probe op (`inventory` or `beat`): its output.
pub(crate) type Runner = Arc<dyn Fn(&str) -> Result<String, String> + Send + Sync>;
/// What records a `node.joined` a `register` makes: its event's id.
pub(crate) type Registrar = Arc<dyn Fn(&Value) -> Result<i64, String> + Send + Sync>;

/// The most requests handled at once, per listener.
pub(crate) const MAX_ACTIVE: usize = 64;
/// The most of them from one address.
pub(crate) const PER_PEER: usize = 16;
/// The nonces remembered (in flight, or answered).
const MAX_SEEN: usize = 1024;
/// How long an answer is kept for its nonce sent again: past the auth's
/// skew, so a request is never run twice while its auth holds.
pub(crate) const SEEN_TTL: Duration = Duration::from_secs(125);
/// How long a tcp peer has to send its request.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
/// The longest time a `forward` may ask.
const FORWARD_MAX: Duration = Duration::from_mins(1);
/// The longest note.
const NOTE_CHARS: usize = 500;
/// What a relay says of any failure to reach its next hop, whatever it was.
pub(crate) const NO_ANSWER: &str = "the next hop gave no valid answer";

/// The probes [`super::INVENTORY`] and [`super::BEAT`], run on this host.
pub(crate) fn local_runner() -> Runner {
    Arc::new(|op: &str| {
        let (script, timeout) = match op {
            "inventory" => (super::INVENTORY, JOIN_TIMEOUT),
            "beat" => (super::BEAT, PROBE_TIMEOUT),
            _ => return Err(format!("no probe {op}")),
        };
        bounded(local_command(script), timeout)
    })
}

/// A nonce's state: being answered, or answered (until when, and how).
enum Seen {
    InFlight,
    Done(Instant, Value),
}

/// How long the answer to `message` is kept against a resend: at least
/// [`SEEN_TTL`], and until its `at_ms` plus the skew (and 5 s) is past,
/// so a request from a clock running fast is never run twice while its
/// auth holds; never more than twice the skew and 10 s.
pub(crate) fn keep_for(message: &Value, now_ms: i64) -> Duration {
    let at = message["at_ms"].as_i64().unwrap_or(now_ms);
    let until = super::crypto::replay_until(at, now_ms);
    let millis = u64::try_from(until.saturating_sub(now_ms)).unwrap_or(0);
    Duration::from_millis(millis).max(SEEN_TTL)
}

/// What [`Relay::admit`] makes of a request.
pub(crate) enum Admit {
    /// Answer this at once (a cached answer, or a refusal).
    Reply(Value),
    /// Its nonce is being answered: drop it.
    Drop,
    /// Answer it ([`Relay::run`]).
    Run(Ticket),
}

/// A nonce being answered: its answer kept when [`Ticket::finish`]ed, its
/// mark cleared when dropped unfinished (an error, a panic, no slot), so
/// the next send of it runs again.
pub(crate) struct Ticket {
    relay: Arc<Relay>,
    nonce: String,
    keep: Duration,
    done: bool,
}

impl Ticket {
    fn finish(mut self, reply: &Value) {
        self.done = true;
        let mut seen = self
            .relay
            .seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        seen.insert(
            std::mem::take(&mut self.nonce),
            Seen::Done(Instant::now() + self.keep, reply.clone()),
        );
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        if !self.done {
            let mut seen = self
                .relay
                .seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if matches!(seen.get(&self.nonce), Some(Seen::InFlight)) {
                seen.remove(&self.nonce);
            }
        }
    }
}

/// A node's answers to the messages it takes.
pub(crate) struct Relay {
    runner: Runner,
    registrar: Option<Registrar>,
    token: Option<String>,
    /// The token before it, still taken while a fleet rotates.
    previous: Option<String>,
    allow_loopback: bool,
    seen: Mutex<HashMap<String, Seen>>,
}

/// A refusal of the request with `nonce`.
fn refusal(nonce: Option<&str>, error: &str) -> Value {
    json!({"nonce": nonce, "ok": false, "error": spawn_core::redact::redact(error)})
}

impl Relay {
    /// `token`: `SPAWN_NODE_TOKEN` (every request then needs its auth, and
    /// a `register` is taken only with one); `allow_loopback`: whether a
    /// forward may reach a loopback or link-local address (tests on one
    /// host, `--allow-loopback-forward`).
    pub(crate) fn new(
        runner: Runner,
        registrar: Option<Registrar>,
        token: Option<String>,
        allow_loopback: bool,
    ) -> Self {
        Self {
            runner,
            registrar,
            token: token.filter(|token| !token.is_empty()),
            previous: None,
            allow_loopback,
            seen: Mutex::new(HashMap::new()),
        }
    }

    /// Also take requests signed or sealed with `previous`
    /// (`SPAWN_NODE_TOKEN_PREV`), while this node has a token of its own.
    #[must_use]
    pub(crate) fn with_previous_token(mut self, previous: Option<String>) -> Self {
        self.previous = previous.filter(|token| !token.is_empty() && self.token.is_some());
        self
    }

    /// The tokens a request may be signed or sealed with.
    fn tokens(&self) -> Vec<&str> {
        [&self.token, &self.previous]
            .into_iter()
            .flatten()
            .map(String::as_str)
            .collect()
    }

    /// Before any slot is taken: refuse a request without a nonce or its
    /// auth, answer one already answered from the cache, drop one in
    /// flight, and mark the rest in flight.
    pub(crate) fn admit(self: &Arc<Self>, message: &Value) -> Admit {
        let Some(nonce) = message["nonce"].as_str().filter(|nonce| valid_nonce(nonce)) else {
            return Admit::Reply(refusal(None, "a message without a nonce"));
        };
        if let Err(why) = self.authorized(message) {
            return Admit::Reply(refusal(Some(nonce), &why));
        }
        let keep = keep_for(message, crate::verify::now_ms());
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        seen.retain(|_, state| match state {
            Seen::Done(until, _) => *until > now,
            Seen::InFlight => true,
        });
        match seen.get(nonce) {
            Some(Seen::Done(_, reply)) => return Admit::Reply(reply.clone()),
            Some(Seen::InFlight) => return Admit::Drop,
            None => {}
        }
        if seen.len() >= MAX_SEEN {
            let oldest = seen
                .iter()
                .filter_map(|(nonce, state)| match state {
                    Seen::Done(at, _) => Some((*at, nonce.clone())),
                    Seen::InFlight => None,
                })
                .min();
            match oldest {
                Some((_, nonce)) => {
                    seen.remove(&nonce);
                }
                None => return Admit::Reply(refusal(Some(nonce), "this node is busy")),
            }
        }
        seen.insert(nonce.to_string(), Seen::InFlight);
        Admit::Run(Ticket {
            relay: Arc::clone(self),
            nonce: nonce.to_string(),
            keep,
            done: false,
        })
    }

    /// Answer an admitted request, the answer kept for its nonce.
    pub(crate) fn run(&self, ticket: Ticket, message: &Value) -> Value {
        let reply = self.reply(message);
        ticket.finish(&reply);
        reply
    }

    /// Admit and answer at once (a tcp request, a test): a request whose
    /// nonce is in flight is told so.
    pub(crate) fn handle(self: &Arc<Self>, message: &Value) -> Value {
        match self.admit(message) {
            Admit::Reply(reply) => reply,
            Admit::Drop => refusal(
                message["nonce"].as_str(),
                "a request with this nonce is in flight",
            ),
            Admit::Run(ticket) => self.run(ticket, message),
        }
    }

    /// Whether the request may be answered: its auth, when this node has a
    /// token or it is a `register`.
    fn authorized(&self, message: &Value) -> Result<(), String> {
        match (&self.token, message["op"].as_str()) {
            (Some(token), _) => {
                let first = auth::verify(message, token);
                if first.is_ok() {
                    return first;
                }
                match &self.previous {
                    Some(previous) => auth::verify(message, previous).or(first),
                    None => first,
                }
            }
            (None, Some("register")) => {
                Err("this node takes no registration without SPAWN_NODE_TOKEN".to_string())
            }
            (None, Some("sealed")) => {
                Err("this node opens no sealed request without SPAWN_NODE_TOKEN".to_string())
            }
            (None, _) => Ok(()),
        }
    }

    fn reply(&self, message: &Value) -> Value {
        if message["op"] == "sealed" {
            return self.sealed_reply(message);
        }
        self.plain_reply(message)
    }

    /// A sealed request opened, answered as its inner request, the answer
    /// sealed back; one that does not open is refused in the clear (it
    /// says nothing the request did not).
    fn sealed_reply(&self, sealed: &Value) -> Value {
        let nonce = sealed["nonce"].as_str();
        let (inner, key) = match super::crypto::open_wire(sealed, &self.tokens()) {
            Ok(opened) => opened,
            Err(why) => return refusal(nonce, &why),
        };
        let reply = if inner["op"] == "sealed" {
            refusal(nonce, "a sealed request inside another")
        } else {
            self.plain_reply(&inner)
        };
        key.seal_reply(&reply)
            .unwrap_or_else(|why| refusal(nonce, &why))
    }

    fn plain_reply(&self, message: &Value) -> Value {
        let nonce = message["nonce"].as_str();
        match self.answer(message) {
            Ok(Value::Object(mut fields)) => {
                fields.insert("nonce".into(), json!(nonce));
                fields.insert("ok".into(), json!(true));
                Value::Object(fields)
            }
            Ok(_) => json!({"nonce": nonce, "ok": true}),
            Err(error) => refusal(nonce, &error),
        }
    }

    fn answer(&self, message: &Value) -> Result<Value, String> {
        match message["op"].as_str() {
            Some(op @ ("inventory" | "beat")) => {
                let output = (self.runner)(op)?;
                if output.len() > usize::try_from(OUTPUT_LIMIT).unwrap_or(usize::MAX) {
                    return Err("the probe's output is too long".to_string());
                }
                Ok(json!({"output": output}))
            }
            Some("register") => {
                let registrar = self
                    .registrar
                    .as_ref()
                    .ok_or("this node takes no registration (spawn node serve --register-store)")?;
                let record = registration(message)?;
                let event = registrar(&record).map_err(|error| store_refusal(&error))?;
                Ok(json!({"event": event}))
            }
            Some("forward") => self.relay(message),
            _ => Err("no such op".to_string()),
        }
    }

    /// A `forward`'s inner request, rebuilt, sent on; its reply (nonce
    /// checked) back.
    fn relay(&self, message: &Value) -> Result<Value, String> {
        only_keys(
            message,
            &[
                "nonce",
                "op",
                "ttl",
                "hops",
                "timeout_ms",
                "inner",
                "at_ms",
                "auth",
            ],
            "a forward",
        )?;
        let hops = message["hops"].as_array().ok_or("a forward without hops")?;
        let ttl = message["ttl"].as_u64().unwrap_or(0);
        if hops.is_empty()
            || hops.len() > MAX_HOPS
            || ttl < u64::try_from(hops.len()).unwrap_or(u64::MAX)
            || ttl > u64::try_from(MAX_HOPS).unwrap_or(u64::MAX)
        {
            return Err(format!("a forward has 1 to {MAX_HOPS} hops within its ttl"));
        }
        let mut path = Vec::new();
        for hop in hops {
            let endpoint = Endpoint::parse(hop.as_str().unwrap_or_default(), None)
                .map_err(|error| format!("a hop of the forward: {error}"))?;
            if path.contains(&endpoint) {
                return Err("a forward names a hop twice".to_string());
            }
            path.push(endpoint);
        }
        let inner = inner_request(&message["inner"])?;
        let inner_nonce = inner["nonce"].as_str().unwrap_or_default().to_string();
        let timeout = message["timeout_ms"]
            .as_u64()
            .map(Duration::from_millis)
            .filter(|timeout| *timeout <= FORWARD_MAX)
            .ok_or("a forward's timeout_ms is at most 60000")?;
        let (next, rest) = path.split_first().ok_or("a forward without hops")?;
        let addresses = self.addresses(next)?;
        let reply = if rest.is_empty() {
            exchange_at(next, &addresses, &inner, timeout).map_err(|_| NO_ANSWER.to_string())?
        } else {
            let envelope = forward(rest, &inner, timeout, self.token.as_deref())?;
            // With a token, the forward this relay sends on is sealed too.
            let reply = match self.token.as_deref() {
                Some(token) => exchange_sealed(next, Some(&addresses), &envelope, timeout, token),
                None => exchange_at(next, &addresses, &envelope, timeout),
            }
            .map_err(|_| NO_ANSWER.to_string())?;
            if reply["ok"] != true {
                return Err(format!(
                    "{next}: {}",
                    reply["error"].as_str().unwrap_or("refused")
                ));
            }
            reply["reply"].clone()
        };
        if reply["nonce"] != inner_nonce.as_str() {
            return Err(NO_ANSWER.to_string());
        }
        Ok(json!({"reply": reply}))
    }

    /// The addresses `next` resolves to, each checked ([`forbidden`] unless
    /// loopback forwards are allowed): one refused refuses the hop.
    fn addresses(&self, next: &Endpoint) -> Result<Vec<std::net::SocketAddr>, String> {
        let addresses = next.addresses().map_err(|_| NO_ANSWER.to_string())?;
        if !self.allow_loopback && addresses.iter().any(|address| forbidden(address.ip())) {
            return Err(format!(
                "{next}: a relay forwards to no loopback, unspecified, link-local, broadcast \
                 or multicast address"
            ));
        }
        Ok(addresses)
    }
}

/// Refuse `message` when it has a key outside `allowed`.
fn only_keys(message: &Value, allowed: &[&str], what: &str) -> Result<(), String> {
    let fields = message
        .as_object()
        .ok_or_else(|| format!("{what} is no object"))?;
    match fields.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(key) => Err(format!("{what} has no field {key:?}")),
        None => Ok(()),
    }
}

/// A forward's inner request rebuilt from its allowed fields alone, each of
/// its type: a probe `{op, nonce, at_ms, auth}`; a `register` with its
/// `name, transport, endpoint, via, mode, note, inventory` too. Any other
/// op, or key, is refused.
pub(crate) fn inner_request(inner: &Value) -> Result<Value, String> {
    const PROBE: &[&str] = &["op", "nonce", "at_ms", "auth"];
    // Issue #87: an inner request sealed for the last node, passed on
    // unread.
    const SEALED: &[&str] = &["op", "nonce", "at_ms", "auth", "salt", "ct"];
    const REGISTER: &[&str] = &[
        "op",
        "nonce",
        "at_ms",
        "auth",
        "name",
        "transport",
        "endpoint",
        "via",
        "mode",
        "note",
        "inventory",
    ];
    let allowed = match inner["op"].as_str() {
        Some("inventory" | "beat") => PROBE,
        Some("register") => REGISTER,
        Some("sealed") => SEALED,
        _ => return Err("a forward carries a probe or a register only".to_string()),
    };
    only_keys(inner, allowed, "a forward's inner request")?;
    inner["nonce"]
        .as_str()
        .filter(|nonce| valid_nonce(nonce))
        .ok_or("a forward's inner request has no nonce")?;
    let mut rebuilt = Map::new();
    for key in allowed {
        let value = &inner[*key];
        let typed = match *key {
            _ if value.is_null() => true,
            "at_ms" => value.is_i64(),
            "via" => value
                .as_array()
                .is_some_and(|hops| hops.iter().all(Value::is_string)),
            _ => value.is_string(),
        };
        if !typed {
            return Err(format!("a forward's inner {key} is not of its type"));
        }
        if !value.is_null() {
            rebuilt.insert((*key).to_string(), value.clone());
        }
    }
    Ok(Value::Object(rebuilt))
}

/// What a registration's refusal by the store says back: the writer's own
/// words (`spawn_node_record`'s: a name joined already, a hop not
/// joined, a field), its first line only, without the driver's prefix;
/// anything else (SQL, a schema, a connection) as one line that says
/// nothing of it.
pub(crate) fn store_refusal(error: &str) -> String {
    const GENERIC: &str = "the primary's store refused the registration";
    const SAID: &[&str] = &[
        "joined already",
        "is not joined",
        "makes a loop",
        "a node's ",
        "a coord node's ",
        "an ssh node's ",
        "a direct node",
        "is reached over another transport",
        "cannot be a hop",
        "names a hop twice",
        "record's ",
    ];
    const INTERNAL: &[&str] = &[
        "sql",
        "relation",
        "column",
        "function",
        "schema",
        "syntax",
        "permission",
        "pl/pgsql",
        "context",
        "spawn_",
        "connection",
        "statement",
        "transaction",
        "postgres",
        "db error",
    ];
    let line = error.lines().next().unwrap_or_default().trim();
    let line = ["db error: ", "ERROR: ", "error: "]
        .iter()
        .fold(line, |line, prefix| {
            line.strip_prefix(prefix).unwrap_or(line)
        })
        .trim();
    let lower = line.to_ascii_lowercase();
    if line.is_empty()
        || line.chars().count() > 300
        || !SAID.iter().any(|said| line.contains(said))
        || INTERNAL.iter().any(|word| lower.contains(word))
    {
        return GENERIC.to_string();
    }
    spawn_core::redact::redact(line)
}

/// The `node.joined` a `register` asks for, each field checked as
/// `spawn_node_record` checks it, its inventory read as `spawn node add`
/// reads one, every text masked.
pub(crate) fn registration(message: &Value) -> Result<Value, String> {
    let name = message["name"]
        .as_str()
        .filter(|name| valid_name(name))
        .ok_or("a registration's name is 1 to 63 letters, digits, dots, dashes or underscores")?;
    let transport = message["transport"]
        .as_str()
        .and_then(Transport::parse)
        .filter(|transport| transport.wire())
        .ok_or("a registration's transport is tcp or udp")?;
    let endpoint = Endpoint::parse(
        message["endpoint"].as_str().unwrap_or_default(),
        Some(transport),
    )?;
    if endpoint.unspecified() {
        return Err(
            "a registration's endpoint names an address to reach, not 0.0.0.0 or [::]".to_string(),
        );
    }
    let via: Vec<String> = match &message["via"] {
        Value::Null => Vec::new(),
        Value::Array(hops) => hops
            .iter()
            .map(|hop| {
                hop.as_str()
                    .map(str::to_string)
                    .ok_or("a hop is a node's name")
            })
            .collect::<Result<_, _>>()?,
        _ => return Err("a registration's via is a list of node names".to_string()),
    };
    hop_names(&via, name)?;
    let mode = Mode::or_default(message["mode"].as_str(), via.len())?;
    let note = match &message["note"] {
        Value::Null => None,
        Value::String(note)
            if note.chars().count() <= NOTE_CHARS && !note.chars().any(char::is_control) =>
        {
            Some(spawn_core::redact::redact(note))
        }
        _ => {
            return Err(format!(
                "a registration's note is at most {NOTE_CHARS} printable characters"
            ));
        }
    };
    let output = message["inventory"]
        .as_str()
        .filter(|output| output.len() <= usize::try_from(OUTPUT_LIMIT).unwrap_or(usize::MAX))
        .ok_or("a registration without its inventory")?;
    let mut record = inventory(&spawn_core::redact::redact(output))?;
    let fields: Map<String, Value> = [
        ("type", json!("node.joined")),
        ("name", json!(name)),
        ("ssh", Value::Null),
        ("transport", json!(transport.name())),
        ("endpoint", json!(endpoint.url())),
        ("via", json!(via)),
        ("mode", json!(mode.name())),
        ("note", json!(note)),
        ("exit_country", Value::Null),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value))
    .collect();
    if let Some(record) = record.as_object_mut() {
        record.extend(fields);
    }
    Ok(record)
}

/// The listeners of a `spawn node serve`, until they are stopped (or
/// dropped).
pub(crate) struct Listening {
    /// Each bound endpoint (its port the system's when asked for 0).
    pub(crate) endpoints: Vec<Endpoint>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl Listening {
    /// Wait for the listeners (they end only when stopped).
    pub(crate) fn wait(mut self) {
        for thread in std::mem::take(&mut self.threads) {
            let _ = thread.join();
        }
    }

    /// Stop the listeners, and wait for them.
    #[cfg(test)]
    pub(crate) fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        self.wait();
    }
}

impl Drop for Listening {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// A listener's requests at once: at most [`MAX_ACTIVE`], [`PER_PEER`] of
/// them from one address.
#[derive(Default)]
pub(crate) struct Slots(Mutex<(usize, HashMap<IpAddr, usize>)>);

/// One of [`Slots`], given back when dropped.
pub(crate) struct Slot {
    slots: Arc<Slots>,
    peer: IpAddr,
}

impl Slots {
    pub(crate) fn take(self: &Arc<Self>, peer: IpAddr) -> Option<Slot> {
        let mut held = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let (total, peers) = &mut *held;
        let of_peer = peers.entry(peer).or_insert(0);
        if *total >= MAX_ACTIVE || *of_peer >= PER_PEER {
            if *of_peer == 0 {
                peers.remove(&peer);
            }
            return None;
        }
        *of_peer += 1;
        *total += 1;
        Some(Slot {
            slots: Arc::clone(self),
            peer,
        })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut held = self.slots.0.lock().unwrap_or_else(PoisonError::into_inner);
        let (total, peers) = &mut *held;
        *total = total.saturating_sub(1);
        if let Some(of_peer) = peers.get_mut(&self.peer) {
            *of_peer = of_peer.saturating_sub(1);
            if *of_peer == 0 {
                peers.remove(&self.peer);
            }
        }
    }
}

/// Listen on each of `binds` (tcp, udp, or both), answering through
/// `relay`, each request on a thread of its own within [`Slots`].
pub(crate) fn listen(relay: &Arc<Relay>, binds: &[Endpoint]) -> Result<Listening, String> {
    let stop = Arc::new(AtomicBool::new(false));
    let mut endpoints = Vec::new();
    let mut threads = Vec::new();
    for bind in binds {
        let address = bind.addresses()?[0];
        let io = |error: std::io::Error| format!("{bind}: {error}");
        let (bound, thread) = if bind.transport == Transport::Udp {
            let socket = UdpSocket::bind(address).map_err(io)?;
            let bound = socket.local_addr().map_err(io)?;
            (
                bound,
                serve_udp(socket, Arc::clone(relay), Arc::clone(&stop)),
            )
        } else {
            let listener = TcpListener::bind(address).map_err(io)?;
            listener.set_nonblocking(true).map_err(io)?;
            let bound = listener.local_addr().map_err(io)?;
            (
                bound,
                serve_tcp(listener, Arc::clone(relay), Arc::clone(&stop)),
            )
        };
        endpoints.push(Endpoint::parse(
            &format!("{}://{bound}", bind.transport.name()),
            Some(bind.transport),
        )?);
        threads.push(thread.map_err(|error| format!("{bind}: {error}"))?);
    }
    Ok(Listening {
        endpoints,
        stop,
        threads,
    })
}

fn serve_tcp(
    listener: TcpListener,
    relay: Arc<Relay>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<JoinHandle<()>> {
    let slots = Arc::new(Slots::default());
    std::thread::Builder::new()
        .name("node-serve-tcp".to_string())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, peer)) => {
                        // No slot: the connection is closed at once.
                        let Some(slot) = slots.take(peer.ip()) else {
                            continue;
                        };
                        let relay = Arc::clone(&relay);
                        let _ = std::thread::Builder::new()
                            .name("node-serve-conn".to_string())
                            .spawn(move || {
                                let _slot = slot;
                                answer_tcp(stream, &relay);
                            });
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(20)),
                }
            }
        })
}

/// One tcp exchange: a request line in (within [`REQUEST_TIMEOUT`]), its
/// reply line out.
fn answer_tcp(mut stream: TcpStream, relay: &Arc<Relay>) {
    use std::io::Write as _;
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let reply = match read_line(&mut stream, Instant::now() + REQUEST_TIMEOUT, TCP_FRAME)
        .and_then(|line| parse(&line))
    {
        Ok(message) => relay.handle(&message),
        Err(error) => refusal(None, &error),
    };
    let mut frame = serde_json::to_vec(&reply).unwrap_or_default();
    frame.push(b'\n');
    let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
    let _ = stream.write_all(&frame);
}

/// A reply as one datagram: one too long becomes a refusal.
fn datagram(reply: &Value) -> Vec<u8> {
    let frame = serde_json::to_vec(reply).unwrap_or_default();
    if frame.len() <= UDP_FRAME {
        return frame;
    }
    serde_json::to_vec(&refusal(
        reply["nonce"].as_str(),
        &format!("the reply is over {UDP_FRAME} bytes, too long for udp"),
    ))
    .unwrap_or_default()
}

fn serve_udp(
    socket: UdpSocket,
    relay: Arc<Relay>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<JoinHandle<()>> {
    let slots = Arc::new(Slots::default());
    socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    std::thread::Builder::new()
        .name("node-serve-udp".to_string())
        .spawn(move || {
            let mut buffer = vec![0_u8; UDP_FRAME + 1];
            while !stop.load(Ordering::Relaxed) {
                let Ok((read, peer)) = socket.recv_from(&mut buffer) else {
                    continue;
                };
                if read > UDP_FRAME {
                    continue;
                }
                let Ok(message) = parse(&buffer[..read]) else {
                    continue;
                };
                // Before any slot: a resend in flight costs nothing, one
                // answered is answered from the cache.
                let ticket = match relay.admit(&message) {
                    Admit::Reply(reply) => {
                        let _ = socket.send_to(&datagram(&reply), peer);
                        continue;
                    }
                    Admit::Drop => continue,
                    Admit::Run(ticket) => ticket,
                };
                // No slot or no thread: the ticket dropped, the next send
                // of the request runs again.
                let (Some(slot), Ok(reply_to)) = (slots.take(peer.ip()), socket.try_clone()) else {
                    continue;
                };
                let relay = Arc::clone(&relay);
                let _ = std::thread::Builder::new()
                    .name("node-serve-datagram".to_string())
                    .spawn(move || {
                        let _slot = slot;
                        let reply = relay.run(ticket, &message);
                        let _ = reply_to.send_to(&datagram(&reply), peer);
                    });
            }
        })
}

#[cfg(test)]
mod tests;
