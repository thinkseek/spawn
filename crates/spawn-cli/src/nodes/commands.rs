//! `spawn node add`, `spawn node serve` and `spawn node register` (issue
//! #85): each command's flags read into what it does (tested without a
//! node), then done.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

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
    /// The ssh destination, or the tcp or udp endpoint's URL.
    pub(crate) endpoint: String,
    pub(crate) via: Vec<String>,
    pub(crate) mode: Mode,
}

/// `spawn node add`'s flags: exactly one of `--ssh`, `--tcp`, `--udp`;
/// `--via`; `--mode` (relay with hops, else direct, by default); `--note`.
pub(crate) fn add_target(name: &str, flags: &Flags) -> Result<AddTarget, String> {
    if let Some(unknown) = flags
        .names()
        .find(|flag| !matches!(*flag, "ssh" | "tcp" | "udp" | "via" | "mode" | "note"))
    {
        return Err(format!("spawn node add has no --{unknown}\n{USAGE}"));
    }
    let given: Vec<(Transport, String)> = [Transport::Ssh, Transport::Tcp, Transport::Udp]
        .into_iter()
        .filter_map(|transport| flags.get(transport.name()).map(|value| (transport, value)))
        .collect();
    let Ok([(transport, value)]) = <[_; 1]>::try_from(given) else {
        return Err(format!(
            "spawn node add takes exactly one of --ssh, --tcp, --udp\n{USAGE}"
        ));
    };
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
    let route = transport::route(target.transport, &target.endpoint, &target.via, &members)
        .map_err(|error| format!("--via: {error}"))?;
    let over = target.transport.name();
    let token = crate::config::env_value("SPAWN_NODE_TOKEN");
    let output = transport::probe_route(&route, Op::Inventory, JOIN_TIMEOUT, token.as_deref())
        .map_err(|error| format!("the preflight of {name} over {over} failed: {error}"))?;
    let facts = inventory(&output)
        .map_err(|error| format!("the preflight of {name} over {over} failed: {error}"))?;
    let record = joined_record(name, &target, facts, flags.get("note").as_deref());
    let event = store
        .node_record(&record)
        .map_err(|error| error.to_string())?;
    println!("node {name} joined (event {event})");
    Ok(0)
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
}

/// `spawn node serve`'s flags, with `token` (`SPAWN_NODE_TOKEN`):
/// `--register-store` without a token is refused (fail closed: no
/// registration is ever taken unauthenticated).
pub(crate) fn serve_config(words: &[String], token: Option<String>) -> Result<ServeConfig, String> {
    let flags = Flags::parse_with_switches(words, &["register-store", "allow-loopback-forward"])?;
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
    })
}

/// `spawn node serve`: answer probes, relay forwards and (with
/// `--register-store`, owner only) record registrations, until killed.
pub(crate) fn serve(words: &[String]) -> Result<i32, String> {
    let config = serve_config(words, crate::config::env_value("SPAWN_NODE_TOKEN"))?;
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
    let relay = Arc::new(relay::Relay::new(
        relay::local_runner(),
        registrar,
        config.token,
        config.allow_loopback_forward,
    ));
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
