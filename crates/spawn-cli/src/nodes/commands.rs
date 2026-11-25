//! `spawn node add`, `spawn node serve` and `spawn node register` (issue
//! #85), and `spawn node coord`, `spawn node connect` and `spawn node
//! probe` (issue #87): each command's flags read into what it does (tested
//! without a node), then done.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::coord::{self, CoordUrl};
use super::crypto::{NodeIdentity, TrustPolicy, valid_node_id};
use super::transport::{self, Endpoint, Mode, Op, Transport};
use super::{
    Flags, INVENTORY, JOIN_TIMEOUT, USAGE, bounded, inventory, name_word, open_store, relay,
    valid_name, valid_ssh,
};

/// A `--via` of `spawn node add`: comma-separated node names, at most
/// [`transport::MAX_HOPS`], none `name`, none twice.
fn via_names(text: Option<&str>, name: &str) -> Result<Vec<String>, String> {
    let hops: Vec<String> = match text {
        None => Vec::new(),
        Some(text) => text.split(',').map(|hop| hop.trim().to_string()).collect(),
    };
    transport::hop_names(&hops, name).map_err(|error| format!("--via: {error}"))?;
    Ok(hops)
}

/// What `spawn node add` was told of a node: how it is reached, at what
/// destination or endpoint, through which hops, in which mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AddTarget {
    pub(crate) transport: Transport,
    /// The ssh destination, or the tcp or udp endpoint's URL, or the
    /// coordinator's.
    pub(crate) endpoint: String,
    pub(crate) via: Vec<String>,
    pub(crate) mode: Mode,
    /// A coord node's id (`--node-id`) or, to look up, its name at the
    /// coordinator (`--peer`).
    pub(crate) node_id: Option<String>,
    pub(crate) peer: Option<String>,
}

/// `spawn node add`'s flags: exactly one of `--ssh`, `--tcp`, `--udp`,
/// `--coord`; `--via`; `--mode` (relay with hops, else direct, by
/// default; a coord node's relay or hole_punch); `--note`; for `--coord`
/// exactly one of `--node-id` and `--peer`, and no `--via` (the
/// coordinator is its way).
pub(crate) fn add_target(name: &str, flags: &Flags) -> Result<AddTarget, String> {
    if let Some(unknown) = flags.names().find(|flag| {
        !matches!(
            *flag,
            "ssh" | "tcp" | "udp" | "coord" | "node-id" | "peer" | "via" | "mode" | "note"
        )
    }) {
        return Err(format!("spawn node add has no --{unknown}\n{USAGE}"));
    }
    let given: Vec<(Transport, String)> = [
        Transport::Ssh,
        Transport::Tcp,
        Transport::Udp,
        Transport::Coord,
    ]
    .into_iter()
    .filter_map(|transport| flags.get(transport.name()).map(|value| (transport, value)))
    .collect();
    let Ok([(transport, value)]) = <[_; 1]>::try_from(given) else {
        return Err(format!(
            "spawn node add takes exactly one of --ssh, --tcp, --udp, --coord\n{USAGE}"
        ));
    };
    if transport == Transport::Coord {
        return coord_target(name, &value, flags);
    }
    if flags.get("node-id").is_some() || flags.get("peer").is_some() {
        return Err("--node-id and --peer name a node behind a coordinator (--coord)".to_string());
    }
    let endpoint = if transport == Transport::Ssh {
        if !valid_ssh(&value) {
            return Err(
                "--ssh is [user@]host or ssh://[user@]host[:port], the port 1 to 65535".to_string(),
            );
        }
        value
    } else {
        let endpoint = Endpoint::parse(&value, Some(transport))
            .map_err(|error| format!("--{}: {error}", transport.name()))?;
        if endpoint.unspecified() {
            return Err(format!(
                "--{}: 0.0.0.0 and [::] name no node to reach",
                transport.name()
            ));
        }
        endpoint.url()
    };
    let via = via_names(flags.get("via").as_deref(), name)?;
    let mode = Mode::or_default(flags.get("mode").as_deref(), via.len())
        .map_err(|error| format!("--mode: {error}"))?;
    Ok(AddTarget {
        transport,
        endpoint,
        via,
        mode,
        node_id: None,
        peer: None,
    })
}

/// `spawn node add <name> --coord <url> --node-id <hex> | --peer <name>`.
fn coord_target(name: &str, url: &str, flags: &Flags) -> Result<AddTarget, String> {
    let url = CoordUrl::parse(url).map_err(|error| format!("--coord: {error}"))?;
    if flags.get("via").is_some() {
        return Err("--via: a coord node is reached through its coordinator".to_string());
    }
    let (node_id, peer) = match (flags.get("node-id"), flags.get("peer")) {
        (Some(id), None) if valid_node_id(&id) => (Some(id), None),
        (Some(_), None) => return Err("--node-id is 64 lowercase hex digits".to_string()),
        (None, Some(peer)) if valid_name(&peer) => (None, Some(peer)),
        (None, Some(_)) => return Err("--peer is a node's name".to_string()),
        _ => {
            return Err(format!(
                "spawn node add {name} --coord takes exactly one of --node-id, --peer"
            ));
        }
    };
    let mode = match flags.get("mode").as_deref() {
        None | Some("relay") => Mode::Relay,
        Some("hole_punch") => Mode::HolePunch,
        Some(_) => return Err("--mode: a coord node's mode is relay or hole_punch".to_string()),
    };
    Ok(AddTarget {
        transport: Transport::Coord,
        endpoint: url.url(),
        via: Vec::new(),
        mode,
        node_id,
        peer,
    })
}

/// The `node.joined` of `name` reached as `target`, from its inventory.
pub(crate) fn joined_record(
    name: &str,
    target: &AddTarget,
    mut facts: Value,
    note: Option<&str>,
) -> Value {
    let ssh = target.transport == Transport::Ssh;
    facts["type"] = json!("node.joined");
    facts["name"] = json!(name);
    facts["ssh"] = if ssh {
        json!(target.endpoint)
    } else {
        Value::Null
    };
    facts["transport"] = json!(target.transport.name());
    facts["endpoint"] = if ssh {
        Value::Null
    } else {
        json!(target.endpoint)
    };
    facts["via"] = json!(target.via);
    facts["mode"] = json!(target.mode.name());
    facts["note"] = json!(note);
    facts["exit_country"] = Value::Null;
    facts
}

/// `spawn node add <name> ...`: the hops resolved from the membership (no
/// loop), the inventory over the route, `node.joined` recorded.
pub(crate) fn add(words: &[String]) -> Result<i32, String> {
    let (name, flags) = name_word(words)?;
    let flags = Flags::parse(flags)?;
    let target = add_target(name, &flags)?;
    let store = open_store()?;
    // Refused before the preflight, so a second add reaches no node.
    let members = store.node_members().map_err(|error| error.to_string())?;
    if members
        .as_array()
        .into_iter()
        .flatten()
        .any(|node| node["name"] == name)
    {
        return Err(format!(
            "node {name} is joined already (spawn node remove {name} first)"
        ));
    }
    if transport::loops_back(&members, &target.via, name) {
        return Err(format!(
            "--via: a hop is reached through {name} itself (a loop)"
        ));
    }
    let over = target.transport.name();
    let token = crate::config::env_value("SPAWN_NODE_TOKEN");
    let failed = |error: String| format!("the preflight of {name} over {over} failed: {error}");
    let record = if target.transport == Transport::Coord {
        let url = CoordUrl::parse(&target.endpoint)?;
        let context = coord::ProbeContext::from_env(token.as_deref())?;
        let probed = coord::ask(
            &url,
            target.node_id.as_deref(),
            target.peer.as_deref(),
            None,
            &json!({"op": "inventory"}),
            JOIN_TIMEOUT,
            &context,
        )
        .map_err(failed)?;
        let output = transport::accepted(probed.reply)
            .and_then(|reply| transport::output_of(&reply))
            .map_err(failed)?;
        let facts = inventory(&output).map_err(failed)?;
        let mut record = coord_joined(
            joined_record(name, &target, facts, flags.get("note").as_deref()),
            &probed.peer,
        );
        // No --mode said: the way the preflight took.
        if flags.get("mode").is_none() {
            record["mode"] = json!(probed.mode);
        }
        record
    } else {
        let route = transport::route(target.transport, &target.endpoint, &target.via, &members)
            .map_err(|error| format!("--via: {error}"))?;
        let output = transport::probe_route(&route, Op::Inventory, JOIN_TIMEOUT, token.as_deref())
            .map_err(failed)?;
        let facts = inventory(&output).map_err(failed)?;
        joined_record(name, &target, facts, flags.get("note").as_deref())
    };
    let event = store
        .node_record(&record)
        .map_err(|error| error.to_string())?;
    println!("node {name} joined (event {event})");
    Ok(0)
}

/// A coord node's `node.joined`: `record` with the identity, role and
/// zone of `peer` (its record at the coordinator, keys checked).
pub(crate) fn coord_joined(mut record: Value, peer: &Value) -> Value {
    record["node_id"] = peer["node_id"].clone();
    record["pubkey"] = peer["pubkey"].clone();
    record["role"] = json!(if peer["role"] == "peer" {
        "peer"
    } else {
        "worker"
    });
    record["zone"] = match peer["zone"].as_str() {
        Some(zone) if coord::valid_zone(zone) => json!(zone),
        _ => Value::Null,
    };
    record
}

/// `spawn node serve`'s endpoints to listen on: `--listen` (one or more,
/// comma-separated, each with its scheme), `--tcp`, `--udp` (tcp and udp
/// on the same port is two listeners).
fn serve_binds(flags: &Flags) -> Result<Vec<Endpoint>, String> {
    if let Some(unknown) = flags
        .names()
        .find(|flag| !matches!(*flag, "listen" | "tcp" | "udp"))
    {
        return Err(format!("spawn node serve has no --{unknown}\n{USAGE}"));
    }
    let mut binds = Vec::new();
    for text in flags.get("listen").iter().flat_map(|text| text.split(',')) {
        binds
            .push(Endpoint::bind(text.trim(), None).map_err(|error| format!("--listen: {error}"))?);
    }
    for transport in [Transport::Tcp, Transport::Udp] {
        if let Some(text) = flags.get(transport.name()) {
            binds.push(
                Endpoint::bind(&text, Some(transport))
                    .map_err(|error| format!("--{}: {error}", transport.name()))?,
            );
        }
    }
    if binds.is_empty() {
        return Err(format!(
            "spawn node serve needs --listen, --tcp or --udp\n{USAGE}"
        ));
    }
    Ok(binds)
}

/// What `spawn node serve` was told: where to listen, whether to record
/// registrations, whether a forward may reach a loopback or link-local
/// address, and the token every request is checked against.
#[derive(Debug)]
pub(crate) struct ServeConfig {
    pub(crate) binds: Vec<Endpoint>,
    pub(crate) register_store: bool,
    pub(crate) allow_loopback_forward: bool,
    pub(crate) token: Option<String>,
    /// `--allow-unauthenticated`: listen beyond loopback without a token.
    pub(crate) allow_unauthenticated: bool,
}

/// `spawn node serve`'s flags, with `token` (`SPAWN_NODE_TOKEN`):
/// `--register-store` without a token is refused (fail closed: no
/// registration is ever taken unauthenticated).
pub(crate) fn serve_config(words: &[String], token: Option<String>) -> Result<ServeConfig, String> {
    let flags = Flags::parse_with_switches(
        words,
        &[
            "register-store",
            "allow-loopback-forward",
            "allow-unauthenticated",
        ],
    )?;
    let binds = serve_binds(&flags)?;
    let token = token.filter(|token| !token.is_empty());
    let register_store = flags.switch("register-store");
    if register_store && token.is_none() {
        return Err("--register-store requires SPAWN_NODE_TOKEN".to_string());
    }
    Ok(ServeConfig {
        binds,
        register_store,
        allow_loopback_forward: flags.switch("allow-loopback-forward"),
        token,
        allow_unauthenticated: flags.switch("allow-unauthenticated"),
    })
}

/// Issue #87: a `spawn node serve` (or `spawn node coord`) listening on
/// anything but loopback answers anyone who reaches it, so it needs its
/// token (`what`) unless `--allow-unauthenticated` says otherwise; one on
/// loopback only (a test, a local relay) does not.
pub(crate) fn require_auth(
    exposed: bool,
    token: bool,
    allow_unauthenticated: bool,
    what: &str,
) -> Result<(), String> {
    if exposed && !token && !allow_unauthenticated {
        return Err(format!(
            "listening beyond loopback without {what} answers anyone: set {what}, or pass \
             --allow-unauthenticated"
        ));
    }
    Ok(())
}

/// `spawn node serve`: answer probes, relay forwards and (with
/// `--register-store`, owner only) record registrations, until killed.
pub(crate) fn serve(words: &[String]) -> Result<i32, String> {
    let config = serve_config(words, crate::config::env_value("SPAWN_NODE_TOKEN"))?;
    require_auth(
        config.binds.iter().any(|bind| !bind.loopback()),
        config.token.is_some(),
        config.allow_unauthenticated,
        "SPAWN_NODE_TOKEN",
    )?;
    let registrar: Option<relay::Registrar> = if config.register_store {
        let store = Mutex::new(open_store()?);
        Some(Arc::new(move |record: &Value| {
            store
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .node_record(record)
                .map_err(|error| error.to_string())
        }))
    } else {
        None
    };
    let relay = Arc::new(
        relay::Relay::new(
            relay::local_runner(),
            registrar,
            config.token,
            config.allow_loopback_forward,
        )
        .with_previous_token(crate::config::env_value("SPAWN_NODE_TOKEN_PREV")),
    );
    let listening = relay::listen(&relay, &config.binds)?;
    for endpoint in &listening.endpoints {
        println!("node serve: listening on {endpoint}");
    }
    listening.wait();
    Ok(0)
}

/// What `spawn node register` sends and where: the `register` message
/// (without its nonce and inventory) and the path up to the primary.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Registration {
    pub(crate) message: Value,
    pub(crate) path: Vec<Endpoint>,
}

/// `spawn node register`'s `--via`: the hops from the primary to this
/// node, each a name or `name=<endpoint>`; with endpoints (every hop's or
/// none) the registration goes up through them, the nearest first.
fn upward(text: Option<&str>, name: &str) -> Result<(Vec<String>, Vec<Endpoint>), String> {
    let mut hops = Vec::new();
    let mut up = Vec::new();
    for entry in text.iter().flat_map(|text| text.split(',')) {
        match entry.trim().split_once('=') {
            Some((hop, endpoint)) => {
                hops.push(hop.to_string());
                up.push(
                    Endpoint::parse(endpoint, None)
                        .map_err(|error| format!("--via {hop}: {error}"))?,
                );
            }
            None => hops.push(entry.trim().to_string()),
        }
    }
    transport::hop_names(&hops, name).map_err(|error| format!("--via: {error}"))?;
    if !up.is_empty() && up.len() != hops.len() {
        return Err(
            "--via: every hop names its endpoint (name=tcp://host:port), or none".to_string(),
        );
    }
    up.reverse();
    Ok((hops, up))
}

/// `spawn node register`'s flags as its message and path: `--name`,
/// `--listen` (where the primary will reach this node's `spawn node
/// serve`), `--to` (the primary's, its scheme `--listen`'s when it says
/// none), `--via`, `--mode`, `--note`.
pub(crate) fn registration(flags: &Flags) -> Result<Registration, String> {
    if let Some(unknown) = flags
        .names()
        .find(|flag| !matches!(*flag, "to" | "name" | "listen" | "via" | "mode" | "note"))
    {
        return Err(format!("spawn node register has no --{unknown}\n{USAGE}"));
    }
    let name = flags.required("name")?;
    if !valid_name(&name) {
        return Err(
            "--name: a node's name is 1 to 63 letters, digits, dots, dashes or \
                    underscores, first a letter or digit"
                .to_string(),
        );
    }
    let listen = Endpoint::parse(&flags.required("listen")?, None)
        .map_err(|error| format!("--listen: {error}"))?;
    if listen.unspecified() {
        return Err(
            "--listen: the endpoint the primary reaches this node at, not 0.0.0.0 or [::]"
                .to_string(),
        );
    }
    let to = flags.required("to")?;
    let to = Endpoint::parse(&to, Some(listen.transport))
        .or_else(|_| Endpoint::parse(&to, None))
        .map_err(|error| format!("--to: {error}"))?;
    let (hops, mut path) = upward(flags.get("via").as_deref(), &name)?;
    let mode = Mode::or_default(flags.get("mode").as_deref(), hops.len())
        .map_err(|error| format!("--mode: {error}"))?;
    path.push(to);
    let message = json!({
        "op": "register", "name": name, "transport": listen.transport.name(),
        "endpoint": listen.url(), "via": hops, "mode": mode.name(), "note": flags.get("note"),
    });
    Ok(Registration { message, path })
}

/// Send `registration` with this node's `output` (its inventory), signed
/// with `token` (never sent itself), bounded by `timeout`, sent again as it
/// was (one nonce, one auth: taken once) while it fails short of a refusal,
/// or is still in flight, and time is left. The `node.joined` event's id.
pub(crate) fn register_once(
    registration: &Registration,
    output: &str,
    timeout: Duration,
    token: &str,
) -> Result<i64, String> {
    let mut message = registration.message.clone();
    message["nonce"] = json!(transport::nonce());
    message["inventory"] = json!(output);
    super::auth::sign(&mut message, Some(token));
    let deadline = Instant::now() + timeout;
    let mut why = "no time to send".to_string();
    for _ in 0..5 {
        let left = deadline.saturating_duration_since(Instant::now());
        if left < Duration::from_millis(500) {
            break;
        }
        match transport::send(&registration.path, &message, left, Some(token))
            .and_then(transport::accepted)
        {
            Ok(reply) => {
                return reply["event"]
                    .as_i64()
                    .ok_or_else(|| "no event in the reply".to_string());
            }
            // Still being answered (a tcp try cut short): ask again.
            Err(error) if error.contains("in flight") => {
                std::thread::sleep(Duration::from_millis(500));
                why = error;
            }
            // A refusal is the answer: no second try changes it.
            Err(error) if error.starts_with("the node refused") => return Err(error),
            Err(error) => why = error,
        }
    }
    Err(why)
}

/// `spawn node register`: this node's inventory sent up to the primary.
pub(crate) fn register(words: &[String]) -> Result<i32, String> {
    let flags = Flags::parse(words)?;
    let registration = registration(&flags)?;
    let token = crate::config::env_value("SPAWN_NODE_TOKEN")
        .ok_or("spawn node register requires SPAWN_NODE_TOKEN (the primary's, to sign with)")?;
    let output = bounded(transport::local_command(INVENTORY), JOIN_TIMEOUT)
        .map_err(|error| format!("this node's inventory: {error}"))?;
    inventory(&output).map_err(|error| format!("this node's inventory: {error}"))?;
    let event = register_once(&registration, &output, JOIN_TIMEOUT, &token)?;
    println!(
        "node {} registered (event {event})",
        registration.message["name"].as_str().unwrap_or("?")
    );
    Ok(0)
}

/// `spawn node coord`'s flags, with `token` (`SPAWN_COORD_TOKEN`): `--ws`,
/// `--tcp` (one at least), `--udp`, `--zone`, `--parent`, `--key-file`,
/// `--lease-secs`; a ws or tcp bind beyond loopback needs the token or
/// `--allow-unauthenticated`.
pub(crate) fn coord_config(
    words: &[String],
    token: Option<String>,
) -> Result<coord::CoordConfig, String> {
    let flags = Flags::parse_with_switches(words, &["allow-unauthenticated"])?;
    if let Some(unknown) = flags.names().find(|flag| {
        !matches!(
            *flag,
            "ws" | "tcp" | "udp" | "zone" | "parent" | "key-file" | "lease-secs"
        )
    }) {
        return Err(format!("spawn node coord has no --{unknown}\n{USAGE}"));
    }
    let (ws, tcp) = (flags.get("ws"), flags.get("tcp"));
    if ws.is_none() && tcp.is_none() {
        return Err(format!("spawn node coord needs --ws or --tcp\n{USAGE}"));
    }
    let token = token.filter(|token| !token.is_empty());
    let mut exposed = false;
    for bind in ws.iter().chain(tcp.iter()) {
        let address = bind
            .parse::<std::net::SocketAddr>()
            .map_err(|_| format!("{bind}: a bind is ip:port (port 0 for any)"))?;
        exposed |= !address.ip().is_loopback();
    }
    if let Some(udp) = flags.get("udp") {
        udp.parse::<std::net::SocketAddr>()
            .map_err(|_| format!("--udp {udp}: a bind is ip:port"))?;
    }
    require_auth(
        exposed,
        token.is_some(),
        flags.switch("allow-unauthenticated"),
        "SPAWN_COORD_TOKEN",
    )?;
    let zone = flags.get("zone");
    if zone.as_deref().is_some_and(|zone| !coord::valid_zone(zone)) {
        return Err("--zone is 1 to 63 letters, digits, dots, dashes or underscores".to_string());
    }
    let parent = flags
        .get("parent")
        .map(|url| CoordUrl::parse(&url).map_err(|error| format!("--parent: {error}")))
        .transpose()?;
    let lease = flags
        .get("lease-secs")
        .map(|secs| {
            secs.parse::<u64>()
                .ok()
                .filter(|secs| (1..=600).contains(secs))
                .map(Duration::from_secs)
                .ok_or("--lease-secs is 1 to 600")
        })
        .transpose()?;
    Ok(coord::CoordConfig {
        ws,
        tcp,
        udp: flags.get("udp"),
        zone,
        parent,
        token,
        key_file: flags.get("key-file").map(std::path::PathBuf::from),
        lease,
    })
}

/// `spawn node coord`: a coordinator, until killed.
pub(crate) fn coord(words: &[String]) -> Result<i32, String> {
    let config = coord_config(words, crate::config::env_value("SPAWN_COORD_TOKEN"))?;
    let running = coord::start(&config)?;
    println!(
        "node coord: {} (zone {})",
        running.coordinator.node_id(),
        config.zone.as_deref().unwrap_or("-")
    );
    for (what, address) in [
        ("ws", running.ws),
        ("tcp", running.tcp),
        ("udp", running.udp),
    ] {
        if let Some(address) = address {
            println!("node coord: listening on {what}://{address}");
        }
    }
    if let Some(parent) = &config.parent {
        println!(
            "node coord: zone {} under {parent}",
            config.zone.as_deref().unwrap_or("-")
        );
    }
    running.wait();
    Ok(0)
}

/// `spawn node connect`'s flags, with `node_token` (`SPAWN_NODE_TOKEN`)
/// and `coord_token` (`SPAWN_COORD_TOKEN`): `--coord`, `--name`, `--role`
/// (worker by default, or peer), `--zone`, `--key-file`, `--udp-stun`,
/// `--register-to`, `--register-store`, `--note`, `--allow-unauthenticated`.
/// The node token is needed (it is mixed into the envelope's keys, so only
/// the fleet can seal to this node) unless `--allow-unauthenticated`; a
/// registration, sent or taken, always needs it.
pub(crate) fn connect_config(
    words: &[String],
    node_token: Option<&str>,
    coord_token: Option<String>,
) -> Result<coord::ConnectConfig, String> {
    let flags = Flags::parse_with_switches(words, &["register-store", "allow-unauthenticated"])?;
    if let Some(unknown) = flags.names().find(|flag| {
        !matches!(
            *flag,
            "coord" | "name" | "role" | "zone" | "key-file" | "udp-stun" | "register-to" | "note"
        )
    }) {
        return Err(format!("spawn node connect has no --{unknown}\n{USAGE}"));
    }
    let url =
        CoordUrl::parse(&flags.required("coord")?).map_err(|error| format!("--coord: {error}"))?;
    let name = flags.required("name")?;
    if !valid_name(&name) {
        return Err(
            "--name: a node's name is 1 to 63 letters, digits, dots, dashes or underscores, \
             first a letter or digit"
                .to_string(),
        );
    }
    let role = flags.get("role").unwrap_or_else(|| "worker".to_string());
    if !matches!(role.as_str(), "worker" | "peer") {
        return Err("--role is worker or peer".to_string());
    }
    let zone = flags.get("zone");
    if zone.as_deref().is_some_and(|zone| !coord::valid_zone(zone)) {
        return Err("--zone is 1 to 63 letters, digits, dots, dashes or underscores".to_string());
    }
    let register_to = flags.get("register-to");
    if register_to
        .as_deref()
        .is_some_and(|to| !valid_name(to) && !valid_node_id(to))
    {
        return Err("--register-to is the primary's name or node id".to_string());
    }
    let register_store = flags.switch("register-store");
    let token = node_token.is_some_and(|token| !token.is_empty());
    if register_store && !token {
        return Err("--register-store requires SPAWN_NODE_TOKEN".to_string());
    }
    if register_to.is_some() && !token {
        return Err("--register-to requires SPAWN_NODE_TOKEN (the primary's)".to_string());
    }
    // Fail closed, as serve and coord do: without the token, any client of
    // the coordinator may seal a probe to this node.
    if !token && !flags.switch("allow-unauthenticated") {
        return Err(
            "spawn node connect without SPAWN_NODE_TOKEN answers any client of the \
             coordinator: set SPAWN_NODE_TOKEN, or pass --allow-unauthenticated"
                .to_string(),
        );
    }
    let note = flags.get("note");
    if note
        .as_deref()
        .is_some_and(|note| note.chars().count() > 500 || note.chars().any(char::is_control))
    {
        return Err("--note is at most 500 printable characters".to_string());
    }
    if let Some(stun) = flags.get("udp-stun") {
        stun.parse::<std::net::SocketAddr>()
            .map_err(|_| format!("--udp-stun {stun}: ip:port of a coordinator's --udp"))?;
    }
    Ok(coord::ConnectConfig {
        url,
        name,
        role,
        zone,
        key_file: flags.get("key-file").map(std::path::PathBuf::from),
        udp_stun: flags.get("udp-stun"),
        register_to,
        register_store,
        note,
        coord_token: coord_token.filter(|token| !token.is_empty()),
        punch_loopback: false,
    })
}

/// `spawn node connect`: this node reached through a coordinator, until
/// killed.
pub(crate) fn connect(words: &[String]) -> Result<i32, String> {
    let policy = TrustPolicy::from_env();
    let config = connect_config(
        words,
        policy.token(),
        crate::config::env_value("SPAWN_COORD_TOKEN"),
    )?;
    let (identity, source) = NodeIdentity::load(config.key_file.as_deref())?;
    match source {
        super::crypto::KeySource::Ephemeral => {
            println!("node connect: a key for this run only (--key-file keeps one)");
        }
        super::crypto::KeySource::File(file, true) => {
            println!("node connect: a new key in {}", file.display());
        }
        _ => {}
    }
    let registrar: Option<relay::Registrar> = if config.register_store {
        let store = Mutex::new(open_store()?);
        Some(Arc::new(move |record: &Value| {
            store
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .node_record(record)
                .map_err(|error| error.to_string())
        }))
    } else {
        None
    };
    let identity = Arc::new(identity);
    let service = Arc::new(coord::NodeService {
        responder: super::crypto::Responder::new(Arc::clone(&identity), policy),
        runner: relay::local_runner(),
        registrar,
        in_flight: Arc::default(),
    });
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let say: coord::Say = Arc::new(|line: String| println!("{line}"));
    coord::run_connector(&config, &identity, &service, &stop, None, &say)?;
    Ok(0)
}

/// `spawn node probe`'s flags: `--op` (inventory or beat, beat by
/// default) and `--json`.
pub(crate) fn probe_flags(words: &[String]) -> Result<(Op, bool), String> {
    let flags = Flags::parse_with_switches(words, &["json"])?;
    if let Some(unknown) = flags.names().find(|flag| *flag != "op") {
        return Err(format!("spawn node probe has no --{unknown}\n{USAGE}"));
    }
    let op = match flags.get("op").as_deref() {
        None | Some("beat") => Op::Beat,
        Some("inventory") => Op::Inventory,
        Some(other) => return Err(format!("--op is inventory or beat, not {other:?}")),
    };
    Ok((op, flags.switch("json")))
}

/// What `spawn node probe --json` says of one probe's `output`.
pub(crate) fn probe_json(
    name: &str,
    op: Op,
    transport: &Value,
    output: &str,
    elapsed: Duration,
) -> Result<Value, String> {
    let facts = match op {
        Op::Inventory => inventory(output)?,
        Op::Beat => super::beat(output)?.facts(),
    };
    Ok(json!({
        "name": name, "op": op.name(), "transport": transport,
        "elapsed_ms": u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        "facts": facts,
    }))
}

/// `spawn node probe <name>`: one probe of a joined node over its route
/// (ssh, tcp, udp, or its coordinator), its output said, or read as
/// JSON; nothing recorded.
pub(crate) fn probe(words: &[String]) -> Result<i32, String> {
    let (name, rest) = name_word(words)?;
    let (op, json_out) = probe_flags(rest)?;
    let members = open_store()?
        .node_members()
        .map_err(|error| error.to_string())?;
    let member = members
        .as_array()
        .into_iter()
        .flatten()
        .find(|member| member["name"] == name)
        .ok_or_else(|| format!("node {name} is not joined"))?;
    let route = transport::Member::of(member)
        .ok_or_else(|| format!("node {name} has no route"))?
        .route(&members)?;
    let token = crate::config::env_value("SPAWN_NODE_TOKEN");
    let begun = Instant::now();
    let output = transport::probe_route(&route, op, JOIN_TIMEOUT, token.as_deref())
        .map_err(|error| format!("node {name}: {error}"))?;
    if json_out {
        let out = probe_json(name, op, &member["transport"], &output, begun.elapsed())?;
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
    } else {
        print!("{output}");
    }
    Ok(0)
}
