//! How the primary reaches a node (issue #85): over ssh as before, or over
//! tcp or udp to the node's own `spawn node serve`, directly or through up
//! to [`MAX_HOPS`] other nodes (`via`), each a relay of the next.
//!
//! The wire: one JSON object a message, a line on tcp (one exchange a
//! connection), a datagram on udp. A request is `{nonce, op, at_ms,
//! auth?, ...}` (the auth: super::auth), its reply `{nonce, ok, ...}`
//! naming the request's nonce: a reply naming
//! another is never taken (on udp it is dropped and the wait goes on), so
//! a reply spoofed or late from an earlier request is no answer. Every
//! nonce is 16 bytes of the system's random device and a counter
//! ([`nonce`]). The ops:
//!
//! - `inventory`, `beat`: the node runs [`super::INVENTORY`] or
//!   [`super::BEAT`] and answers `{output}`;
//! - `register`: a node joins itself (`spawn node register`), answered
//!   `{event}` (super::relay);
//! - `forward`: `{ttl, hops, timeout_ms, inner}`, a relay sends `inner`,
//!   rebuilt from its allowed fields (super::relay), or, while hops remain
//!   after the next, a `forward` of its own with a nonce of its own, to
//!   `hops[0]`, and answers `{reply}`, the inner
//!   request's reply as it came back, whose nonce the sender checks again.
//!
//! Every exchange is bounded: by its deadline (each relay passes on less
//! time than it took, [`HOP_MARGIN`]), by its frame ([`TCP_FRAME`],
//! [`UDP_FRAME`]), and by [`MAX_HOPS`]. Udp sends again while no reply
//! comes, at most until the deadline.

use std::io::{Read, Write};
use std::net::{Ipv6Addr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{BEAT, INVENTORY, OUTPUT_LIMIT, bounded, clean_text, ssh_command_via, valid_name};

/// The most hops between the primary and a node, and of a `forward`.
pub(crate) const MAX_HOPS: usize = 8;
/// The longest message on tcp (a probe's output, bounded, and its frame).
pub(crate) const TCP_FRAME: usize = 320 * 1024;
/// The longest datagram on udp.
pub(crate) const UDP_FRAME: usize = 65_000;
/// What each relay keeps of the time it was given, for its own reply.
pub(crate) const HOP_MARGIN: Duration = Duration::from_millis(400);
/// The least time a relay passes on.
const HOP_LEAST: Duration = Duration::from_millis(300);
/// Udp's first wait before it sends again (doubling, up to a second).
const UDP_RETRY: Duration = Duration::from_millis(250);

/// How a node is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transport {
    Ssh,
    Tcp,
    Udp,
}

impl Transport {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "ssh" => Some(Self::Ssh),
            "tcp" => Some(Self::Tcp),
            "udp" => Some(Self::Udp),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Ssh => "ssh",
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}

/// How the path to a node was made: straight, through relays, or a hole
/// a node punched in its NAT by speaking first (a label the record keeps;
/// the probe goes the same way).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Direct,
    Relay,
    HolePunch,
}

impl Mode {
    pub(crate) fn parse(text: &str) -> Option<Self> {
        match text {
            "direct" => Some(Self::Direct),
            "relay" => Some(Self::Relay),
            "hole_punch" => Some(Self::HolePunch),
            _ => None,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Relay => "relay",
            Self::HolePunch => "hole_punch",
        }
    }

    /// The mode a join says, or its default: relay with hops, else direct.
    /// Direct with hops is refused (`spawn_node_record` holds the same).
    pub(crate) fn or_default(mode: Option<&str>, hops: usize) -> Result<Self, String> {
        let mode = match mode {
            None if hops > 0 => Self::Relay,
            None => Self::Direct,
            Some(text) => Self::parse(text)
                .ok_or_else(|| format!("a mode is direct, relay or hole_punch, not {text:?}"))?,
        };
        if mode == Self::Direct && hops > 0 {
            return Err("a direct node has no --via (its mode is relay or hole_punch)".to_string());
        }
        Ok(mode)
    }
}

/// A tcp or udp endpoint: `host:port`, the host a name, an IPv4 or a
/// bracketed IPv6 address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Endpoint {
    pub(crate) transport: Transport,
    host: String,
    port: u16,
}

impl Endpoint {
    /// `[tcp://|udp://]host:port`, the scheme `transport`'s when both are
    /// said, one of them at least. No user or password (`@`), nothing an
    /// argument parser reads as an option (a leading `-`), no space or
    /// control character, at most 255 characters, the port 1 to 65535
    /// (`spawn_node_record` holds the same).
    pub(crate) fn parse(text: &str, transport: Option<Transport>) -> Result<Self, String> {
        Self::parse_port(text, transport, 1)
    }

    /// An endpoint to listen on: as [`Self::parse`], the port 0 too (the
    /// system's choice).
    pub(crate) fn bind(text: &str, transport: Option<Transport>) -> Result<Self, String> {
        Self::parse_port(text, transport, 0)
    }

    fn parse_port(text: &str, transport: Option<Transport>, least: u16) -> Result<Self, String> {
        let wanted = transport.map_or("tcp|udp", Transport::name);
        let bad = || format!("an endpoint is [{wanted}://]host:port, the port 1 to 65535");
        if text.len() > 255
            || text.starts_with('-')
            || text.contains('@')
            || text.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(bad());
        }
        let (scheme, rest) = match text.split_once("://") {
            Some((scheme, rest)) => (
                Some(
                    Transport::parse(scheme)
                        .filter(|t| *t != Transport::Ssh)
                        .ok_or_else(bad)?,
                ),
                rest,
            ),
            None => (None, text),
        };
        let transport = match (scheme, transport) {
            (Some(said), Some(wanted)) if said != wanted => return Err(bad()),
            (Some(said), _) => said,
            (None, Some(wanted)) if wanted != Transport::Ssh => wanted,
            _ => return Err(bad()),
        };
        let (host, port) = rest.rsplit_once(':').ok_or_else(bad)?;
        let valid_host = if let Some(v6) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']'))
        {
            v6.parse::<Ipv6Addr>().is_ok()
        } else {
            let mut chars = host.chars();
            chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
                && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
        };
        let port = (port.len() <= 5 && port.chars().all(|c| c.is_ascii_digit()))
            .then(|| port.parse::<u16>().ok())
            .flatten()
            .filter(|port| *port >= least);
        match port {
            Some(port) if valid_host => Ok(Self {
                transport,
                host: host.to_string(),
                port,
            }),
            _ => Err(bad()),
        }
    }

    /// `tcp://host:port` or `udp://host:port`.
    pub(crate) fn url(&self) -> String {
        format!("{}://{}:{}", self.transport.name(), self.host, self.port)
    }

    /// Whether the host is the unspecified address, which listens on every
    /// interface but names no node to reach.
    pub(crate) fn unspecified(&self) -> bool {
        let host = self.host.trim_start_matches('[').trim_end_matches(']');
        host.parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_unspecified())
    }

    /// The addresses the host resolves to.
    pub(crate) fn addresses(&self) -> Result<Vec<SocketAddr>, String> {
        let addresses: Vec<SocketAddr> = (self.host.as_str(), self.port)
            .to_socket_addrs()
            .or_else(|_| format!("{}:{}", self.host, self.port).to_socket_addrs())
            .map_err(|error| format!("{}: {error}", self.url()))?
            .collect();
        if addresses.is_empty() {
            return Err(format!("{}: no address", self.url()));
        }
        Ok(addresses)
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.url())
    }
}

/// A `--via`, or a join's `via`: at most [`MAX_HOPS`] node names, none
/// `name` itself, none twice.
pub(crate) fn hop_names(hops: &[String], name: &str) -> Result<(), String> {
    if hops.len() > MAX_HOPS {
        return Err(format!("at most {MAX_HOPS} hops, not {}", hops.len()));
    }
    for (index, hop) in hops.iter().enumerate() {
        if !valid_name(hop) {
            return Err(format!("hop {hop:?} is no node's name"));
        }
        if hop == name {
            return Err(format!("node {name} cannot be a hop of its own via"));
        }
        if hops[..index].contains(hop) {
            return Err(format!("hop {hop} is named twice"));
        }
    }
    Ok(())
}

/// The way to a node: ssh with its jump hosts (a nested `ProxyCommand`), or a path of tcp and
/// udp endpoints, the relays in order and the node last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Route {
    Ssh {
        destination: String,
        jumps: Vec<String>,
    },
    Net(Vec<Endpoint>),
}

/// One joined node as the store's membership says it (`node_members`):
/// its transport and endpoint (the ssh destination for ssh; an old join
/// without them, ssh and its `ssh`) and its hops.
pub(crate) struct Member<'a> {
    pub(crate) transport: Transport,
    pub(crate) endpoint: &'a str,
    pub(crate) via: Vec<String>,
}

impl<'a> Member<'a> {
    pub(crate) fn of(member: &'a Value) -> Option<Self> {
        let transport = Transport::parse(member["transport"].as_str().unwrap_or("ssh"))?;
        let endpoint = member["endpoint"].as_str().or(member["ssh"].as_str())?;
        let via = member["via"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|hop| hop.as_str().map(str::to_string))
            .collect();
        Some(Self {
            transport,
            endpoint,
            via,
        })
    }
}

/// `name`'s entry of the membership.
fn find<'a>(members: &'a Value, name: &str) -> Option<&'a Value> {
    members
        .as_array()
        .into_iter()
        .flatten()
        .find(|member| member["name"] == name)
}

/// Whether the hops of `via`, followed through each hop's own `via` in the
/// membership, ever reach `name` (`spawn_node_record` checks the same
/// under the append lock).
pub(crate) fn loops_back(members: &Value, via: &[String], name: &str) -> bool {
    let mut seen: Vec<String> = via.to_vec();
    let mut front = seen.clone();
    while !front.is_empty() {
        let mut next = Vec::new();
        for hop in &front {
            for further in find(members, hop)
                .and_then(Member::of)
                .map(|m| m.via)
                .unwrap_or_default()
            {
                if further == name {
                    return true;
                }
                if !seen.contains(&further) {
                    seen.push(further.clone());
                    next.push(further);
                }
            }
        }
        front = next;
    }
    false
}

/// The route to a node reached over `transport` at `endpoint` through the
/// joined hops `via` (each resolved from the membership): an ssh node's
/// hops are ssh nodes, its jump hosts; a tcp or udp node's are tcp or udp
/// nodes (mixed as they are), its relays.
pub(crate) fn route(
    transport: Transport,
    endpoint: &str,
    via: &[String],
    members: &Value,
) -> Result<Route, String> {
    let mut hops = Vec::new();
    for hop in via {
        let member = find(members, hop)
            .and_then(Member::of)
            .ok_or_else(|| format!("hop {hop} is not joined"))?;
        let over_ssh = member.transport == Transport::Ssh;
        if over_ssh != (transport == Transport::Ssh) {
            return Err(format!(
                "hop {hop} is reached over {}: the hops of a node over {} are {}",
                member.transport.name(),
                transport.name(),
                if transport == Transport::Ssh {
                    "ssh nodes"
                } else {
                    "tcp or udp nodes"
                }
            ));
        }
        hops.push((hop, member));
    }
    if transport == Transport::Ssh {
        if !super::valid_ssh(endpoint) {
            return Err("not an ssh destination".to_string());
        }
        let mut jumps = Vec::new();
        for (hop, member) in hops {
            if !super::valid_ssh(member.endpoint) {
                return Err(format!("hop {hop} has no ssh destination"));
            }
            jumps.push(member.endpoint.to_string());
        }
        return Ok(Route::Ssh {
            destination: endpoint.to_string(),
            jumps,
        });
    }
    let mut path = Vec::new();
    for (hop, member) in hops {
        path.push(
            Endpoint::parse(member.endpoint, Some(member.transport))
                .map_err(|error| format!("hop {hop}: {error}"))?,
        );
    }
    path.push(Endpoint::parse(endpoint, Some(transport))?);
    Ok(Route::Net(path))
}

/// What a probe asks of a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    Inventory,
    Beat,
}

impl Op {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Inventory => "inventory",
            Self::Beat => "beat",
        }
    }

    pub(crate) fn script(self) -> &'static str {
        match self {
            Self::Inventory => INVENTORY,
            Self::Beat => BEAT,
        }
    }
}

/// A probe request for `op`, signed with `token` when there is one.
pub(crate) fn request(op: Op, token: Option<&str>) -> Value {
    let mut message = json!({"nonce": nonce(), "op": op.name()});
    super::auth::sign(&mut message, token);
    message
}

/// Ask `op` of the node at the end of `route`, bounded by `timeout`, every
/// tcp or udp request signed with `token` (`SPAWN_NODE_TOKEN`) when there
/// is one: its output, masked, or why not.
pub(crate) fn probe_route(
    route: &Route,
    op: Op,
    timeout: Duration,
    token: Option<&str>,
) -> Result<String, String> {
    match route {
        Route::Ssh { destination, jumps } => {
            if !super::valid_ssh(destination) || !jumps.iter().all(|jump| super::valid_ssh(jump)) {
                return Err("not an ssh destination".to_string());
            }
            bounded(ssh_command_via(destination, jumps, op.script()), timeout)
        }
        Route::Net(path) => {
            let reply = accepted(send(path, &request(op, token), timeout, token)?)?;
            let output = reply["output"]
                .as_str()
                .filter(|output| {
                    output.len() <= usize::try_from(OUTPUT_LIMIT).unwrap_or(usize::MAX)
                })
                .ok_or("the node's answer has no output")?;
            Ok(spawn_core::redact::redact(output))
        }
    }
}

/// A new nonce: 16 bytes of `/dev/urandom` in hex, then a counter of this
/// process (so two are never the same even were the device to repeat),
/// the time and the process's id standing in for the bytes when it cannot
/// be read.
pub(crate) fn nonce() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut bytes = [0_u8; 16];
    let random = std::fs::File::open("/dev/urandom")
        .and_then(|mut device| device.read_exact(&mut bytes))
        .is_ok();
    let head = if random {
        bytes.iter().fold(String::new(), |mut hex, byte| {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
            hex
        })
    } else {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        format!("{nanos:x}{:x}", std::process::id())
    };
    format!("{head}-{count:x}")
}

/// Whether `nonce` is one a message may carry: 16 to 80 letters, digits,
/// dashes or underscores.
pub(crate) fn valid_nonce(nonce: &str) -> bool {
    (16..=80).contains(&nonce.len())
        && nonce
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

/// A reply that says `ok`, or the error it says (one line, masked).
pub(crate) fn accepted(reply: Value) -> Result<Value, String> {
    if reply["ok"] == true {
        return Ok(reply);
    }
    let why = reply["error"].as_str().and_then(clean_text).map_or_else(
        || "no reason said".to_string(),
        |text| spawn_core::redact::redact(&text),
    );
    Err(format!("the node refused: {why}"))
}

/// Send `inner` along `path` (relays, then its node), bounded by
/// `timeout`: straight to the node without a relay, else wrapped in a
/// `forward` to the first relay. The inner request's reply, its nonce
/// checked, `ok` or not. The `forward` is signed with `token`.
pub(crate) fn send(
    path: &[Endpoint],
    inner: &Value,
    timeout: Duration,
    token: Option<&str>,
) -> Result<Value, String> {
    let (first, rest) = path.split_first().ok_or("no endpoint to send to")?;
    let inner_nonce = inner["nonce"].as_str().unwrap_or_default().to_string();
    if rest.is_empty() {
        return exchange(first, inner, timeout);
    }
    let envelope = forward(rest, inner, timeout, token)?;
    let reply = accepted(exchange(first, &envelope, timeout)?)?;
    let inner = reply["reply"].clone();
    if inner["nonce"] != inner_nonce.as_str() {
        return Err("the relayed reply names another request".to_string());
    }
    Ok(inner)
}

/// The `forward` that has a relay send `inner` along `hops` (at most
/// [`MAX_HOPS`]), passing on `timeout` less [`HOP_MARGIN`], signed with
/// `token` when there is one.
pub(crate) fn forward(
    hops: &[Endpoint],
    inner: &Value,
    timeout: Duration,
    token: Option<&str>,
) -> Result<Value, String> {
    if hops.is_empty() || hops.len() > MAX_HOPS {
        return Err(format!(
            "a forward has 1 to {MAX_HOPS} hops, not {}",
            hops.len()
        ));
    }
    let passed = timeout.saturating_sub(HOP_MARGIN);
    if passed < HOP_LEAST {
        return Err("no time left to pass on".to_string());
    }
    let mut envelope = json!({
        "nonce": nonce(), "op": "forward", "ttl": hops.len(),
        "hops": hops.iter().map(Endpoint::url).collect::<Vec<_>>(),
        "timeout_ms": u64::try_from(passed.as_millis()).unwrap_or(u64::MAX),
        "inner": inner,
    });
    super::auth::sign(&mut envelope, token);
    Ok(envelope)
}

/// Whether a relay must never connect to `ip` on a peer's word: a loopback,
/// unspecified, link-local, broadcast or multicast address, or one of them
/// mapped into IPv6 (so a forward cannot reach the relay's own host
/// services nor a cloud's metadata address).
pub(crate) fn forbidden(ip: std::net::IpAddr) -> bool {
    fn v4(ip: std::net::Ipv4Addr) -> bool {
        ip.is_loopback()
            || ip.is_unspecified()
            || ip.is_link_local()
            || ip.is_broadcast()
            || ip.is_multicast()
    }
    match ip {
        std::net::IpAddr::V4(ip) => v4(ip),
        std::net::IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || (ip.segments()[0] & 0xffc0) == 0xfe80
                || ip.to_ipv4_mapped().is_some_and(v4)
        }
    }
}

/// One request to `to` at `addresses` (resolved and checked already) and
/// its reply naming the request's nonce, as [`exchange`].
pub(crate) fn exchange_at(
    to: &Endpoint,
    addresses: &[SocketAddr],
    message: &Value,
    timeout: Duration,
) -> Result<Value, String> {
    exchange_with(to, Some(addresses), message, timeout)
}

/// One request to `to` and its reply naming the request's nonce, bounded
/// by `timeout` (on udp, sent again while no such reply comes).
pub(crate) fn exchange(to: &Endpoint, message: &Value, timeout: Duration) -> Result<Value, String> {
    exchange_with(to, None, message, timeout)
}

fn exchange_with(
    to: &Endpoint,
    addresses: Option<&[SocketAddr]>,
    message: &Value,
    timeout: Duration,
) -> Result<Value, String> {
    let addresses = match addresses {
        Some(addresses) if !addresses.is_empty() => addresses.to_vec(),
        Some(_) => return Err(format!("{to}: no address")),
        None => to.addresses()?,
    };
    let nonce = message["nonce"]
        .as_str()
        .filter(|nonce| valid_nonce(nonce))
        .ok_or("a request without a nonce")?;
    let deadline = Instant::now() + timeout;
    let mut frame = serde_json::to_vec(message).map_err(|error| error.to_string())?;
    if to.transport == Transport::Udp {
        if frame.len() > UDP_FRAME {
            return Err(format!(
                "the request is over {UDP_FRAME} bytes, too long for udp"
            ));
        }
        return udp_exchange(to, addresses[0], &frame, nonce, deadline);
    }
    frame.push(b'\n');
    if frame.len() > TCP_FRAME {
        return Err(format!("the request is over {TCP_FRAME} bytes"));
    }
    let reply = tcp_exchange(to, &addresses, &frame, deadline)?;
    if reply["nonce"] != nonce {
        return Err(format!("{to} answered another request"));
    }
    Ok(reply)
}

/// The time left before `deadline`, or an error naming `to`.
fn left(deadline: Instant, to: &Endpoint) -> Result<Duration, String> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        Err(format!("{to}: no answer in time"))
    } else {
        Ok(left)
    }
}

fn tcp_exchange(
    to: &Endpoint,
    addresses: &[SocketAddr],
    frame: &[u8],
    deadline: Instant,
) -> Result<Value, String> {
    let mut why = String::new();
    let mut stream = None;
    for address in addresses {
        match TcpStream::connect_timeout(address, left(deadline, to)?) {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(error) => why = format!("{to}: {error}"),
        }
    }
    let mut stream = stream.ok_or(why)?;
    let io = |error: std::io::Error| format!("{to}: {error}");
    stream
        .set_write_timeout(Some(left(deadline, to)?))
        .map_err(io)?;
    stream.write_all(frame).map_err(io)?;
    read_line(&mut stream, deadline, TCP_FRAME)
        .map_err(|error| format!("{to}: {error}"))
        .and_then(|line| parse(&line))
}

/// One line of `stream` (up to `\n` or its end), at most `limit` bytes,
/// by `deadline`.
pub(crate) fn read_line(
    stream: &mut TcpStream,
    deadline: Instant,
    limit: usize,
) -> Result<Vec<u8>, String> {
    let mut line = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("no whole message in time".to_string());
        }
        stream
            .set_read_timeout(Some(left))
            .map_err(|error| error.to_string())?;
        let read = match stream.read(&mut chunk) {
            Ok(read) => read,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err("no whole message in time".to_string());
            }
            Err(error) => return Err(error.to_string()),
        };
        if read == 0 {
            return Ok(line);
        }
        let got = &chunk[..read];
        if let Some(end) = got.iter().position(|byte| *byte == b'\n') {
            line.extend_from_slice(&got[..end]);
            if line.len() > limit {
                return Err(format!("a message over {limit} bytes"));
            }
            return Ok(line);
        }
        line.extend_from_slice(got);
        if line.len() > limit {
            return Err(format!("a message over {limit} bytes"));
        }
    }
}

/// A message's bytes as a JSON object.
pub(crate) fn parse(bytes: &[u8]) -> Result<Value, String> {
    match serde_json::from_slice::<Value>(bytes) {
        Ok(value) if value.is_object() => Ok(value),
        _ => Err("a message that is no JSON object".to_string()),
    }
}

fn udp_exchange(
    to: &Endpoint,
    address: SocketAddr,
    frame: &[u8],
    nonce: &str,
    deadline: Instant,
) -> Result<Value, String> {
    let local = if address.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let io = |error: std::io::Error| format!("{to}: {error}");
    let socket = UdpSocket::bind(local).map_err(io)?;
    // Connected: the kernel drops every datagram from another address.
    socket.connect(address).map_err(io)?;
    let mut buffer = vec![0_u8; UDP_FRAME + 1];
    let mut wait = UDP_RETRY;
    let mut why = format!("{to}: no answer in time");
    loop {
        left(deadline, to).map_err(|_| why.clone())?;
        if let Err(error) = socket.send(frame) {
            // A port that refused an earlier datagram says so here.
            why = format!("{to}: {error}");
        }
        let resend = Instant::now() + wait;
        wait = (wait * 2).min(Duration::from_secs(1));
        loop {
            let until = resend
                .min(deadline)
                .saturating_duration_since(Instant::now());
            if until.is_zero() {
                break;
            }
            socket.set_read_timeout(Some(until)).map_err(io)?;
            match socket.recv(&mut buffer) {
                Ok(read) if read <= UDP_FRAME => {
                    if let Ok(reply) = parse(&buffer[..read])
                        && reply["nonce"] == nonce
                    {
                        return Ok(reply);
                    }
                    // Another request's reply, or none: dropped.
                }
                Ok(_) => why = format!("{to}: a reply over {UDP_FRAME} bytes"),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => {
                    why = format!("{to}: {error}");
                    std::thread::sleep(until.min(Duration::from_millis(50)));
                }
            }
        }
    }
}

/// `sh -c script` on this host, with the environment ssh would give it:
/// what `spawn node serve` runs for a probe.
pub(crate) fn local_command(script: &str) -> Command {
    let mut command = Command::new("sh");
    command.env_clear();
    for name in ["PATH", "HOME", "USER", "LOGNAME"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.args(["-c", script]);
    command
}

#[cfg(test)]
mod tests;
