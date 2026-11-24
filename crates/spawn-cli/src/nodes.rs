//! The nodes (multi-host P1; docs/fleet-architecture.md §1.2): the
//! machines Spawn may use, and whether they answer. A node does no work yet.
//!
//! - `spawn node add|list|drain|undrain|remove` ([`command`]): `add` runs
//!   the read-only [`INVENTORY`] once over the node's transport and records
//!   `node.joined`: over ssh (through jump hosts with `--via`), or over tcp
//!   or udp to the node's `spawn node serve`, directly or relayed by the
//!   nodes `--via` names, tcp and udp mixed (issue #85, [`transport`]).
//! - `spawn node serve` is a node's own end of tcp and udp: it answers the
//!   probes, relays `forward`s, and with `--register-store` takes the
//!   `register` of a node that joins itself (`spawn node register`, from
//!   behind a NAT or a firewall, directly or relayed) ([`relay`]).
//! - `spawn loop` keeps a [`Heartbeat`]: a worker thread with a database
//!   connection of its own (every statement bounded), so a pass never
//!   waits on a node, its records or its alerts. Every [`PROBE_EVERY_MS`]
//!   (monotonic) it runs the small [`BEAT`] on each node (one bounded probe
//!   in flight per node, on a thread of its own), records `node.heartbeat`
//!   every [`RECORD_EVERY_MS`] while the node answers, `node.lost` after
//!   `SPAWN_NODE_LOST_SECS` (60) without an answer, and `node.back` at the
//!   next one. Each names its `join`, the event of the node's current
//!   `node.joined`, which the writer compares under the append lock: a
//!   probe of a join since removed records nothing. At each read of the
//!   membership it fires and clears `node_lost` alerts as each node's
//!   latest `node.lost` asks ([`alert_plan`]).
//!
//! The trust boundary: the primary sends a node nothing but [`INVENTORY`]
//! or [`BEAT`], fixed texts of read-only commands (over tcp and udp, only
//! their names: the node runs its own copy). ssh runs with an
//! environment of `PATH`, `HOME`, `USER`, `LOGNAME` and `SSH_AUTH_SOCK`
//! alone (no `SendEnv` can carry the database URL or a token), in batch
//! mode against an already trusted host key, without a shared connection,
//! forwarding or terminal, in a process group of its own that is killed
//! whole when the probe ends. Every string of a record is masked.

use std::collections::BTreeMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use spawn_postgres::{PostgresStore, StoreConnector};

use crate::verify::now_ms;
use crate::{Flags, open_store};

mod auth;
mod commands;
mod coord;
mod crypto;
mod relay;
mod transport;
mod ws;

use transport::{Op, Route};

/// What `spawn node add` runs on a node: read-only commands only. Each
/// section starts with a line `@<name>`; `@end` says the output is whole.
pub(crate) const INVENTORY: &str = "echo @arch; uname -m; echo @os; uname -o; \
echo @kernel; uname -r; echo @cores; nproc; \
echo @cpu_max; cat /sys/fs/cgroup/cpu.max 2>/dev/null; echo @meminfo; cat /proc/meminfo; \
echo @mem_max; cat /sys/fs/cgroup/memory.max 2>/dev/null; echo @df; df -Pk \"$HOME\"; \
echo @gpu; if command -v nvidia-smi >/dev/null 2>&1; then \
nvidia-smi --query-gpu=name,memory.total --format=csv,noheader,nounits; fi; echo @end";

/// What a heartbeat runs: the load, the available memory, the free disk.
pub(crate) const BEAT: &str = "echo @loadavg; cat /proc/loadavg; \
echo @meminfo; grep '^MemAvailable:' /proc/meminfo; echo @df; df -Pk \"$HOME\"; echo @end";

/// Between two heartbeat probes of a node.
pub(crate) const PROBE_EVERY_MS: i64 = 15_000;
/// One heartbeat probe's bound, and `spawn node add`'s.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
pub(crate) const JOIN_TIMEOUT: Duration = Duration::from_secs(30);
/// How often a node that answers has its heartbeat recorded.
pub(crate) const RECORD_EVERY_MS: i64 = 600_000;
/// `SPAWN_NODE_LOST_SECS`: its default and range (at least a probe
/// interval plus a probe's bound).
const LOST_SECS: u64 = 60;
const LOST_RANGE: std::ops::RangeInclusive<u64> = 30..=86_400;
/// How often the worker reads the membership: while no node is joined,
/// and while one is.
const IDLE_LIST_EVERY_MS: i64 = 60_000;
/// The longest pause after a database error (from [`PROBE_EVERY_MS`],
/// doubling).
const BACKOFF_MAX_MS: i64 = 300_000;
/// The worker's statement bound (its lock wait is half of it).
const STATEMENT_TIMEOUT: Duration = Duration::from_secs(5);
/// The most of a probe's output (and of its errors) read.
const OUTPUT_LIMIT: u64 = 256 * 1024;
/// The longest text of a node (an arch, a GPU's name, an error) kept.
const TEXT_CHARS: usize = 200;

pub(crate) const USAGE: &str = "usage: spawn node add <name> --ssh <[user@]host|ssh://[user@]host[:port]> [--note <text>]   (owner only)\n       spawn node add <name> --ssh <dest>|--tcp <[tcp://]host:port>|--udp <[udp://]host:port> [--via <hop>[,<hop>...]] [--mode direct|relay|hole_punch] [--note <text>]\n       spawn node list [--json]\n       spawn node drain|undrain|remove <name>   (owner only)\n       spawn node serve --listen <tcp://bind|udp://bind>[,...] | --tcp <bind> | --udp <bind> [--register-store] [--allow-loopback-forward] [--allow-unauthenticated]\n       spawn node register --to <[tcp://|udp://]host:port> --name <name> --listen <tcp://host:port|udp://host:port> [--via <hop>[=<endpoint>][,...]] [--mode direct|relay|hole_punch] [--note <text>]\n       spawn node add <name> --coord <ws://|wss://|tcp://...> --node-id <hex>|--peer <name> [--mode relay|hole_punch] [--note <text>]\n       spawn node probe <name> [--op inventory|beat] [--json]\n       spawn node coord --ws <bind>|--tcp <bind> [--udp <bind>] [--zone <name>] [--parent <ws://|wss://|tcp://...>] [--key-file <path>] [--lease-secs <n>] [--allow-unauthenticated]\n       spawn node connect --coord <ws://|wss://|tcp://...> --name <name> [--role worker|peer] [--zone <name>] [--key-file <path>] [--udp-stun <host:port>] [--register-to <primary>] [--register-store] [--note <text>] [--allow-unauthenticated]";

/// One line of text from a node, kept only when printable, and short.
fn clean_text(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty() && !text.chars().any(char::is_control))
        .then(|| text.chars().take(TEXT_CHARS).collect())
}

/// A probe's output by section; refused without `@end` (cut short).
fn sections(output: &str) -> Result<BTreeMap<&str, Vec<&str>>, String> {
    let mut sections: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut current = None;
    for line in output.lines() {
        if let Some(name) = line.strip_prefix('@') {
            current = Some(name.trim());
            sections.entry(name.trim()).or_default();
        } else if let Some(name) = current {
            sections.entry(name).or_default().push(line);
        }
    }
    if sections.contains_key("end") {
        Ok(sections)
    } else {
        Err("the probe's output was cut short (no @end)".to_string())
    }
}

/// A `/proc/meminfo` field in bytes (the file says kB).
fn meminfo(lines: &[&str], field: &str) -> Option<u64> {
    lines.iter().find_map(|line| {
        let mut words = line
            .strip_prefix(field)?
            .strip_prefix(':')?
            .split_whitespace();
        let value: u64 = words.next()?.parse().ok()?;
        matches!(words.next(), Some("kB") | None)
            .then(|| value.checked_mul(1024))
            .flatten()
    })
}

/// The free bytes `df -Pk` says: the column before the `Capacity` one
/// (`NN%`) of its second line, so a mount point with a space still reads.
fn df_free(lines: &[&str]) -> Option<u64> {
    let words: Vec<&str> = lines.get(1)?.split_whitespace().collect();
    let capacity = words.iter().position(|word| {
        word.strip_suffix('%')
            .is_some_and(|n| n.parse::<u64>().is_ok())
    })?;
    let kib: u64 = words.get(capacity.checked_sub(1)?)?.parse().ok()?;
    kib.checked_mul(1024)
}

/// The probe's sections read as one node's facts.
struct Sections<'a>(BTreeMap<&'a str, Vec<&'a str>>);

impl Sections<'_> {
    fn lines(&self, name: &str) -> &[&str] {
        self.0.get(name).map_or(&[][..], Vec::as_slice)
    }

    fn first(&self, name: &str) -> Option<&str> {
        self.lines(name)
            .iter()
            .map(|line| line.trim())
            .find(|line| !line.is_empty())
    }

    fn text(&self, name: &str) -> Result<String, String> {
        self.first(name)
            .and_then(clean_text)
            .ok_or_else(|| format!("the probe's output has no {name}"))
    }

    fn free_disk(&self) -> Result<u64, String> {
        df_free(self.lines("df")).ok_or_else(|| "the probe's output has no free disk".to_string())
    }

    fn memory(&self, field: &str) -> Result<u64, String> {
        meminfo(self.lines("meminfo"), field)
            .ok_or_else(|| format!("the probe's output has no {field}"))
    }
}

/// [`INVENTORY`]'s output as the facts `node.joined` records: `arch`,
/// `os`, `kernel`, `cores`, `cpu_max` (the cgroup's `cpu.max` as written,
/// or null), `mem_total`, `mem_max` (the cgroup's `memory.max` in bytes,
/// null for `max`), `disk_free` (of the node user's `$HOME`) and `gpu`
/// (each card's `name` and `memory_mb`, or null without one). An output
/// cut short, or without what every Linux host answers, is refused.
pub(crate) fn inventory(output: &str) -> Result<Value, String> {
    let sections = Sections(sections(output)?);
    let cores = sections
        .first("cores")
        .and_then(|text| text.parse::<u64>().ok())
        .filter(|cores| *cores > 0)
        .ok_or("the probe's output has no cores")?;
    let cpu_max = sections.first("cpu_max").filter(|text| {
        matches!(text.split_whitespace().collect::<Vec<_>>()[..],
            [quota, period] if (quota == "max" || quota.parse::<u64>().is_ok())
                && period.parse::<u64>().is_ok())
    });
    let gpus: Vec<Value> = sections
        .lines("gpu")
        .iter()
        .filter_map(|line| {
            let (name, memory) = line.rsplit_once(',')?;
            Some(json!({"name": clean_text(name)?, "memory_mb": memory.trim().parse::<u64>().ok()}))
        })
        .collect();
    Ok(json!({
        "arch": sections.text("arch")?,
        "os": sections.text("os")?,
        "kernel": sections.text("kernel")?,
        "cores": cores,
        "cpu_max": cpu_max,
        "mem_total": sections.memory("MemTotal")?,
        "mem_max": sections.first("mem_max").and_then(|text| text.parse::<u64>().ok()),
        "disk_free": sections.free_disk()?,
        "gpu": if gpus.is_empty() { Value::Null } else { Value::Array(gpus) },
    }))
}

/// What a heartbeat reads ([`BEAT`]).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HeartbeatSample {
    pub(crate) load1: f64,
    pub(crate) mem_available: u64,
    pub(crate) disk_free: u64,
}

/// [`BEAT`]'s output; one cut short, or missing a value, is no answer.
pub(crate) fn beat(output: &str) -> Result<HeartbeatSample, String> {
    let sections = Sections(sections(output)?);
    let load1 = sections
        .first("loadavg")
        .and_then(|text| text.split_whitespace().next()?.parse::<f64>().ok())
        .filter(|load| load.is_finite() && *load >= 0.0)
        .ok_or("the probe's output has no load average")?;
    Ok(HeartbeatSample {
        load1,
        mem_available: sections.memory("MemAvailable")?,
        disk_free: sections.free_disk()?,
    })
}

/// Whether `name` is a node's name: 1 to 63 letters, digits, dots, dashes or
/// underscores, first a letter or digit (`spawn_node_record` holds the same).
pub(crate) fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.len() <= 63
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Whether `destination` is one ssh takes as a destination and nothing
/// else: `[user@]host` or `ssh://[user@]host[:port]`, the port 1 to 65535.
/// Nothing ssh would read as an option (a leading `-`), no password, no
/// space or control character (`spawn_node_record` holds the same).
pub(crate) fn valid_ssh(destination: &str) -> bool {
    fn word(text: &str, first: fn(char) -> bool, rest: &[char]) -> bool {
        let mut chars = text.chars();
        chars.next().is_some_and(first)
            && chars.all(|c| c.is_ascii_alphanumeric() || rest.contains(&c))
    }
    fn user_host(text: &str) -> bool {
        let host = |text: &str| word(text, |c| c.is_ascii_alphanumeric(), &['.', '-']);
        match text.split_once('@') {
            Some((user, rest)) => {
                word(
                    user,
                    |c| c.is_ascii_alphanumeric() || c == '_',
                    &['.', '_', '-'],
                ) && host(rest)
            }
            None => host(text),
        }
    }
    if destination.len() > 255 {
        return false;
    }
    match destination.strip_prefix("ssh://") {
        Some(rest) => match rest.rsplit_once(':') {
            Some((head, port)) => {
                port.len() <= 5
                    && port.chars().all(|c| c.is_ascii_digit())
                    && port.parse::<u16>().is_ok_and(|port| port > 0)
                    && user_host(head)
            }
            None => user_host(rest),
        },
        None => user_host(destination),
    }
}

/// The options every ssh of a probe runs with, each jump host's too
/// (module doc).
pub(crate) const SSH_OPTIONS: [&str; 9] = [
    "BatchMode=yes",
    "ConnectTimeout=10",
    "StrictHostKeyChecking=yes",
    "UpdateHostKeys=no",
    "ControlMaster=no",
    "ControlPath=none",
    "ClearAllForwardings=yes",
    "ForwardAgent=no",
    "ForwardX11=no",
];

/// `text` as one POSIX shell word.
pub(crate) fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The `ProxyCommand` that reaches the host after `jumps` through them, in
/// order: an ssh to the last jump with every one of [`SSH_OPTIONS`] and
/// `-W [%h]:%p`, itself reaching that jump through a `ProxyCommand` of the
/// jumps before it, each word shell-quoted, the nested command's `%`
/// doubled (each ssh expands its own `%h` and `%p` once). Not `-J`:
/// OpenSSH gives a jump host none of the command line's `-o` options.
pub(crate) fn proxy_command(jumps: &[String]) -> Option<String> {
    let (last, before) = jumps.split_last()?;
    let mut words = vec!["ssh".to_string()];
    for option in SSH_OPTIONS {
        words.push("-o".to_string());
        words.push(shell_quote(option));
    }
    if let Some(inner) = proxy_command(before) {
        words.push("-o".to_string());
        words.push(shell_quote(&format!(
            "ProxyCommand={}",
            inner.replace('%', "%%")
        )));
    }
    words.extend([
        "-T".to_string(),
        "-W".to_string(),
        shell_quote("[%h]:%p"),
        "--".to_string(),
        shell_quote(last),
    ]);
    Some(words.join(" "))
}

/// The ssh that runs `script` on `destination` (module doc) through the
/// jump hosts `jumps` in order ([`proxy_command`]; each a destination
/// [`valid_ssh`] takes, checked by the caller, so none is read as an
/// option), its options before the destination.
pub(crate) fn ssh_command_via(destination: &str, jumps: &[String], script: &str) -> Command {
    let mut command = Command::new("ssh");
    command.env_clear();
    for name in ["PATH", "HOME", "USER", "LOGNAME", "SSH_AUTH_SOCK"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    for option in SSH_OPTIONS {
        command.args(["-o", option]);
    }
    if let Some(proxy) = proxy_command(jumps) {
        command.args(["-o", &format!("ProxyCommand={proxy}")]);
    }
    command.args(["-T", "-a", "-x", "--", destination, script]);
    command
}

/// Everything of `stream` up to [`OUTPUT_LIMIT`], the rest read and
/// dropped (so the writer never blocks on a full pipe), sent once it closes.
fn drain(stream: Option<impl Read + Send + 'static>) -> Receiver<Vec<u8>> {
    let (send, receive) = std::sync::mpsc::channel();
    let Some(mut stream) = stream else {
        let _ = send.send(Vec::new());
        return receive;
    };
    let _ = std::thread::Builder::new()
        .name("node-probe-pipe".to_string())
        .spawn(move || {
            let mut kept = Vec::new();
            let _ = (&mut stream).take(OUTPUT_LIMIT).read_to_end(&mut kept);
            let _ = std::io::copy(&mut stream, &mut std::io::sink());
            let _ = send.send(kept);
        });
    receive
}

/// Run `command` until one deadline, `timeout` from now: its standard
/// output when it exits 0 and its pipes close in time, otherwise why not.
/// It leads a process group of its own, killed whole when the run ends
/// however it ends, so nothing it started (a proxy, a helper holding a
/// pipe) outlives the probe, nor the threads reading its pipes.
pub(crate) fn bounded(mut command: Command, timeout: Duration) -> Result<String, String> {
    use std::os::unix::process::CommandExt as _;
    let deadline = Instant::now() + timeout;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|error| format!("could not start ssh: {error}"))?;
    let group = rustix::process::Pid::from_child(&child);
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());
    let ended = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() >= deadline => {
                break Err(format!("no answer in {} s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => break Err(format!("ssh: {error}")),
        }
    };
    let pipe = |receiver: &Receiver<Vec<u8>>| {
        receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .ok()
    };
    let (stdout, stderr) = match &ended {
        Ok(_) => (pipe(&out), pipe(&err)),
        Err(_) => (None, None),
    };
    let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    let _ = child.kill();
    let _ = child.wait();
    let status = ended?;
    if !status.success() {
        let why = stderr
            .as_deref()
            .map(String::from_utf8_lossy)
            .and_then(|text| text.lines().rev().find_map(clean_text))
            // Masked here, before any log line or record holds it.
            .map(|line| spawn_core::redact::redact(&line))
            .unwrap_or_default();
        return Err(match status.code() {
            Some(code) if why.is_empty() => format!("ssh exited {code}"),
            Some(code) => format!("ssh exited {code}: {why}"),
            None => "ssh ended by a signal".to_string(),
        });
    }
    stdout
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .ok_or_else(|| format!("its output was not closed in {} s", timeout.as_secs()))
}

/// Bytes as GiB with one decimal, `-` unread.
#[allow(clippy::cast_precision_loss)]
fn gib(bytes: Option<u64>) -> String {
    bytes.map_or_else(
        || "-".to_string(),
        |bytes| format!("{:.1}G", bytes as f64 / 1_073_741_824.0),
    )
}

/// How a node is reached, as `spawn node list` says it: `ssh` (an ssh
/// node, direct), else `<transport>/<mode>` (`tcp/direct`, `udp/relay`,
/// `ssh/relay`, `tcp/hole_punch`); an old join's, ssh and direct.
fn reach(node: &Value) -> String {
    let transport = node["transport"].as_str().unwrap_or("ssh");
    let hops = node["via"].as_array().map_or(0, Vec::len);
    let mode = node["mode"]
        .as_str()
        .unwrap_or(if hops > 0 { "relay" } else { "direct" });
    if transport == "ssh" && mode == "direct" {
        transport.to_string()
    } else {
        format!("{transport}/{mode}")
    }
}

/// A node's hops as `spawn node list` says them, `via a->b`, or nothing.
fn hops_text(node: &Value) -> Option<String> {
    let hops: Vec<&str> = node["via"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    (!hops.is_empty()).then(|| format!("via {}", hops.join("->")))
}

/// One line per node for `spawn node list`, from the store's list.
fn lines(nodes: &Value, now: i64) -> Vec<String> {
    let nodes = nodes.as_array().map_or(&[][..], Vec::as_slice);
    if nodes.is_empty() {
        return vec!["no node joined (spawn node add <name> --ssh <destination>)".to_string()];
    }
    let row = |cells: [&str; 8]| {
        format!(
            "{:<16} {:<8} {:<14} {:<8} {:>5} {:>8} {:>8} {}",
            cells[0], cells[1], cells[2], cells[3], cells[4], cells[5], cells[6], cells[7]
        )
    };
    let mut out = vec![row([
        "NAME",
        "STATE",
        "TRANSPORT",
        "ARCH",
        "CORES",
        "MEMORY",
        "DISK",
        "SAMPLED",
    ])];
    for node in nodes {
        let beat = &node["heartbeat"];
        let sampled = beat["at_ms"].as_i64().map_or_else(
            || "never".to_string(),
            |at| {
                format!(
                    "{} s ago (load {}, {} available)",
                    now.saturating_sub(at) / 1000,
                    beat["load1"],
                    gib(beat["mem_available"].as_u64())
                )
            },
        );
        let sampled = match hops_text(node) {
            Some(hops) => format!("{sampled}  {hops}"),
            None => sampled,
        };
        out.push(row([
            node["name"].as_str().unwrap_or("?"),
            node["state"].as_str().unwrap_or("?"),
            &reach(node),
            node["arch"].as_str().unwrap_or("?"),
            &node["cores"].to_string(),
            &gib(node["mem_total"].as_u64()),
            &gib(beat["disk_free"].as_u64().or(node["disk_free"].as_u64())),
            &sampled,
        ]));
    }
    out
}

/// The node's name, the first word after the verb, and the words after it.
fn name_word(words: &[String]) -> Result<(&str, &[String]), String> {
    let Some((name, rest)) = words.split_first() else {
        return Err(USAGE.to_string());
    };
    if !valid_name(name) {
        return Err(format!(
            "a node's name is 1 to 63 letters, digits, dots, dashes or underscores, \
             first a letter or digit\n{USAGE}"
        ));
    }
    Ok((name, rest))
}

/// `spawn node ...`.
pub(crate) fn command(rest: &[String]) -> Result<i32, String> {
    let Some((verb, words)) = rest.split_first() else {
        return Err(USAGE.to_string());
    };
    match verb.as_str() {
        "add" => commands::add(words),
        "serve" => commands::serve(words),
        "register" => commands::register(words),
        "coord" => commands::coord(words),
        "connect" => commands::connect(words),
        "probe" => commands::probe(words),
        "list" => {
            let json = match words {
                [] => false,
                [flag] if flag == "--json" => true,
                _ => return Err(USAGE.to_string()),
            };
            let nodes = open_store()?.nodes().map_err(|error| error.to_string())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&nodes).unwrap_or_default()
                );
            } else {
                for line in lines(&nodes, now_ms()) {
                    println!("{line}");
                }
            }
            Ok(0)
        }
        "drain" | "undrain" | "remove" => {
            let (name, more) = name_word(words)?;
            if let Some(word) = more.first() {
                return Err(format!("unexpected argument {word}\n{USAGE}"));
            }
            let kind = format!("node.{verb}{}", if verb == "remove" { "d" } else { "ed" });
            let event = open_store()?
                .node_record(&json!({"type": kind, "name": name}))
                .map_err(|error| error.to_string())?;
            println!("node {name}: {kind} (event {event})");
            Ok(0)
        }
        _ => Err(USAGE.to_string()),
    }
}

/// `SPAWN_NODE_LOST_SECS` in ms, with a warning when it is not whole
/// seconds in [`LOST_RANGE`] (the default is kept).
pub(crate) fn lost_ms(text: Option<&str>) -> (i64, Option<String>) {
    let default = i64::try_from(LOST_SECS * 1000).unwrap_or(60_000);
    let Some(text) = text else {
        return (default, None);
    };
    match text.trim().parse::<u64>() {
        Ok(secs) if LOST_RANGE.contains(&secs) => {
            (i64::try_from(secs * 1000).unwrap_or(default), None)
        }
        _ => (
            default,
            Some(format!(
                "SPAWN_NODE_LOST_SECS: {text:?} is not whole seconds from {} to {}; using {LOST_SECS}",
                LOST_RANGE.start(),
                LOST_RANGE.end()
            )),
        ),
    }
}

/// A moment as the worker reads it: `mono`, ms on a monotonic clock (no
/// change of the wall clock moves it), for every bound and cadence; `wall`,
/// ms since the epoch, for the times records hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProbeClock {
    pub(crate) mono: i64,
    pub(crate) wall: i64,
}

impl ProbeClock {
    /// Now, on a monotonic clock started at `origin`.
    fn now(origin: Instant) -> Self {
        Self {
            mono: i64::try_from(origin.elapsed().as_millis()).unwrap_or(i64::MAX),
            wall: now_ms(),
        }
    }

    /// A wall time the store gave, placed on this moment's monotonic clock.
    fn of_wall(self, wall: i64) -> Self {
        Self {
            mono: self.mono.saturating_sub(self.wall.saturating_sub(wall)),
            wall,
        }
    }
}

/// A probe to start: the node, the join it was started for, and where.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProbeJob {
    pub(crate) name: String,
    pub(crate) join: i64,
    /// Its way (ssh, ssh through jump hosts, tcp or udp through relays),
    /// or why the membership gives none (a hop not joined): a failed probe.
    pub(crate) route: Result<Route, String>,
}

/// A probe's end, stamped on its own thread when it ended.
#[derive(Debug)]
pub(crate) struct ProbeOutcome {
    pub(crate) name: String,
    pub(crate) join: i64,
    pub(crate) at: ProbeClock,
    pub(crate) answer: Result<HeartbeatSample, String>,
}

/// What a record changes in the watch, once the store took it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum NodeTransition {
    Lost,
    Back,
    Beat(ProbeClock),
}

/// A record the watch decided: it changes nothing until it is written
/// ([`HeartbeatScheduler::applied`]), so one the store did not take is decided again.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NodeRecordIntent {
    pub(crate) name: String,
    pub(crate) join: i64,
    pub(crate) change: NodeTransition,
    pub(crate) record: Value,
}

/// One node as the worker keeps it: one join of it.
struct Node {
    route: Result<Route, String>,
    join: i64,
    lost: bool,
    /// Its last answer known: one this worker took, or one the store holds
    /// (the open `node.lost`'s `last_seen_ms`, the latest heartbeat). The
    /// only time a record calls last seen.
    seen: Option<ProbeClock>,
    /// The last answer before its open loss: what `down_ms` counts from,
    /// however many tries its `node.back` takes.
    lost_seen: Option<ProbeClock>,
    /// When this worker began watching this join (monotonic): the lost
    /// bound is not counted from before it (a grace, not an answer).
    watched: i64,
    /// Its latest heartbeat recorded (monotonic).
    beat: Option<i64>,
    due: i64,
    probing: bool,
    /// Why its last probe failed, said once per streak.
    failing: Option<String>,
}

/// The heartbeat's decisions over the joined nodes, without I/O: the
/// worker ([`Heartbeat`]) reads the membership into it, hands it each
/// probe's end, writes what it proposes and tells it what was written.
pub(crate) struct HeartbeatScheduler {
    lost_ms: i64,
    nodes: BTreeMap<String, Node>,
    proposals: Vec<NodeRecordIntent>,
    lines: Vec<String>,
    resync: bool,
}

impl HeartbeatScheduler {
    pub(crate) fn new(lost_ms: i64) -> Self {
        Self {
            lost_ms,
            nodes: BTreeMap::new(),
            proposals: Vec::new(),
            lines: Vec::new(),
            resync: false,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// After a write that failed (it may have been taken all the same):
    /// the next [`Self::sync`] takes each node's lost state and heartbeat
    /// from the store.
    pub(crate) fn resync(&mut self) {
        self.resync = true;
    }

    /// Take the store's membership (`node_members`) at `now`. A node new to
    /// the worker, or joined again since (another `join`), starts afresh
    /// from what the store holds, its probe in flight disowned; a node no
    /// longer listed is dropped. Otherwise the worker's own state stands,
    /// but after [`Self::resync`].
    pub(crate) fn sync(&mut self, members: &Value, now: ProbeClock) {
        let mut nodes = BTreeMap::new();
        for member in members.as_array().into_iter().flatten() {
            let (Some(name), Some(reached), Some(join)) = (
                member["name"].as_str(),
                transport::Member::of(member),
                member["join"].as_i64(),
            ) else {
                continue;
            };
            // Resolved again at each read: a hop removed, or joined again
            // elsewhere, changes the way, never the join.
            let route = reached.route(members);
            let open = member["lost"]["open"] == true;
            let last_seen = member["lost"]["last_seen_ms"]
                .as_i64()
                .filter(|_| open)
                .map(|wall| now.of_wall(wall));
            let beat = member["beat_ms"].as_i64().map(|wall| now.of_wall(wall));
            let evidence = [last_seen, beat]
                .into_iter()
                .flatten()
                .max_by_key(|at| at.wall);
            let mut node = match self.nodes.remove(name) {
                Some(node) if node.join == join && !self.resync => node,
                Some(mut node) if node.join == join => {
                    node.lost = open;
                    node.lost_seen = if open {
                        node.lost_seen.or(last_seen)
                    } else {
                        None
                    };
                    node.beat = beat.map(|at| at.mono);
                    node.seen = node.seen.or(evidence);
                    node
                }
                _ => Node {
                    route: Err(String::new()),
                    join,
                    lost: open,
                    seen: evidence,
                    lost_seen: last_seen,
                    watched: now.mono,
                    beat: beat.map(|at| at.mono),
                    due: now.mono,
                    probing: false,
                    failing: None,
                },
            };
            node.route = route;
            nodes.insert(name.to_string(), node);
        }
        self.nodes = nodes;
        self.resync = false;
    }

    /// Take a probe's end. One for a join no longer current (the node
    /// removed, or removed and joined again) changes nothing.
    pub(crate) fn take(&mut self, done: ProbeOutcome) {
        let Some(node) = self
            .nodes
            .get_mut(&done.name)
            .filter(|node| node.join == done.join)
        else {
            return;
        };
        node.probing = false;
        let (name, join, at) = (done.name, node.join, done.at);
        match done.answer {
            Ok(beat) => {
                node.seen = Some(at);
                node.failing = None;
                if node.lost {
                    let down = node.lost_seen.map(|seen| at.mono.saturating_sub(seen.mono));
                    self.proposals.push(NodeRecordIntent {
                        record: json!({"type": "node.back", "name": name, "join": join,
                                       "down_ms": down}),
                        name: name.clone(),
                        join,
                        change: NodeTransition::Back,
                    });
                }
                if node
                    .beat
                    .is_none_or(|last| at.mono.saturating_sub(last) >= RECORD_EVERY_MS)
                {
                    self.proposals.push(NodeRecordIntent {
                        record: json!({
                            "type": "node.heartbeat", "name": name, "join": join,
                            "load1": beat.load1, "mem_available": beat.mem_available,
                            "disk_free": beat.disk_free,
                        }),
                        name,
                        join,
                        change: NodeTransition::Beat(at),
                    });
                }
            }
            Err(why) => {
                if node.failing.as_deref() != Some(why.as_str()) {
                    self.lines.push(format!("node {name}: probe failed: {why}"));
                }
                node.failing = Some(why);
            }
        }
    }

    /// Decide at `now`: each node silent past the lost bound is to be
    /// lost, and the probes due start. Returns the proposals (with those
    /// the ends taken made), the probes to start, and lines for the log.
    pub(crate) fn decide(
        &mut self,
        now: ProbeClock,
    ) -> (Vec<NodeRecordIntent>, Vec<ProbeJob>, Vec<String>) {
        let mut work = Vec::new();
        for (name, node) in &mut self.nodes {
            let from = node
                .seen
                .map_or(node.watched, |seen| seen.mono.max(node.watched));
            let silent = now.mono.saturating_sub(from);
            if !node.lost && silent >= self.lost_ms {
                self.proposals.push(NodeRecordIntent {
                    record: json!({
                        "type": "node.lost", "name": name, "join": node.join,
                        "last_seen_ms": node.seen.map(|seen| seen.wall), "silent_ms": silent,
                        "error": node.failing,
                    }),
                    name: name.clone(),
                    join: node.join,
                    change: NodeTransition::Lost,
                });
            }
            if !node.probing && now.mono >= node.due {
                node.probing = true;
                node.due = now.mono.saturating_add(PROBE_EVERY_MS);
                work.push(ProbeJob {
                    name: name.clone(),
                    join: node.join,
                    route: node.route.clone(),
                });
            }
        }
        (
            std::mem::take(&mut self.proposals),
            work,
            std::mem::take(&mut self.lines),
        )
    }

    /// The store took `proposal`: what it changes, and a line for the
    /// loop's log when it is a crossing.
    pub(crate) fn applied(&mut self, proposal: &NodeRecordIntent) -> Option<String> {
        let node = self
            .nodes
            .get_mut(&proposal.name)
            .filter(|node| node.join == proposal.join)?;
        let name = &proposal.name;
        match proposal.change {
            NodeTransition::Lost => {
                node.lost = true;
                node.lost_seen = node.seen;
                let silent = proposal.record["silent_ms"].as_i64().unwrap_or_default();
                Some(format!(
                    "node {name}: lost, no answer for {} s",
                    silent / 1000
                ))
            }
            NodeTransition::Back => {
                node.lost = false;
                node.lost_seen = None;
                Some(format!("node {name}: answers again"))
            }
            NodeTransition::Beat(at) => {
                node.beat = Some(at.mono);
                None
            }
        }
    }
}

/// The `node_lost` alert records the nodes' state asks for, in order,
/// given the open ones (`{key: node name}`): for each node, its latest
/// `node.lost` (`node_members`' `lost`) fired when it never was, and
/// cleared, after it, when a `node.back` followed; then every other open
/// one cleared (its node back, removed or joined again). A loss and its
/// recovery between two reads are both told, in order; a write that
/// failed is asked for again at the next read.
pub(crate) fn alert_plan(members: &Value, open: &BTreeMap<String, String>) -> Vec<Value> {
    let mut out = Vec::new();
    let mut open = open.clone();
    let mut lost_now = BTreeMap::new();
    for member in members.as_array().into_iter().flatten() {
        let lost = &member["lost"];
        let (Some(name), Some(event_id)) = (member["name"].as_str(), lost["event_id"].as_i64())
        else {
            continue;
        };
        let key = format!("node_lost:{name}:{event_id}");
        if lost["alerted"] != true && !open.contains_key(&key) {
            out.push(json!({
                "type": "alert.fired", "key": key, "kind": "node_lost",
                "subject": format!("node {name}"),
                "evidence": {
                    "name": name, "event_id": event_id, "last_seen_ms": lost["last_seen_ms"],
                    "silent_ms": lost["silent_ms"], "error": lost["error"],
                },
                "text": format!(
                    "node {name} lost: no answer for {} s",
                    lost["silent_ms"].as_i64().unwrap_or_default() / 1000
                ),
                "notice": false,
            }));
            open.insert(key.clone(), name.to_string());
        }
        lost_now.insert(key, lost["open"] == true);
    }
    for (key, name) in open {
        let ended = match lost_now.get(&key) {
            Some(true) => continue,
            Some(false) => "back",
            None => "gone",
        };
        out.push(json!({
            "type": "alert.cleared", "key": key, "kind": "node_lost",
            "subject": format!("node {name}"),
            "evidence": {"name": name, "lost": false, "ended": ended},
        }));
    }
    out
}

/// The loop's side of the heartbeat: it starts the worker at its first
/// look and then only takes the worker's lines. No I/O.
pub(crate) struct Heartbeat {
    lost_ms: i64,
    lines: Option<Receiver<String>>,
}

impl Heartbeat {
    pub(crate) fn from_env() -> (Self, Vec<String>) {
        let (lost_ms, warning) =
            lost_ms(crate::config::env_value("SPAWN_NODE_LOST_SECS").as_deref());
        (
            Self {
                lost_ms,
                lines: None,
            },
            warning.into_iter().collect(),
        )
    }

    pub(crate) fn check(&mut self, open: &PostgresStore) -> Vec<String> {
        if let Some(lines) = &self.lines {
            return lines.try_iter().collect();
        }
        let (say, lines) = std::sync::mpsc::channel();
        self.lines = Some(lines);
        let connector = open.connector(STATEMENT_TIMEOUT);
        let lost_ms = self.lost_ms;
        match std::thread::Builder::new()
            .name("node-heartbeat".to_string())
            .spawn(move || work(&connector, lost_ms, &say))
        {
            Ok(_) => Vec::new(),
            Err(error) => vec![format!("nodes: the heartbeat did not start: {error}")],
        }
    }
}

/// Start `item`'s probe on a thread of its own; its end comes on `done`.
fn start(item: ProbeJob, done: &Sender<ProbeOutcome>, origin: Instant) {
    let (name, join) = (item.name.clone(), item.join);
    let send = done.clone();
    let spawned = std::thread::Builder::new()
        .name("node-probe".to_string())
        .spawn(move || {
            let answer = item
                .route
                .and_then(|route| {
                    let token = crate::config::env_value("SPAWN_NODE_TOKEN");
                    transport::probe_route(&route, Op::Beat, PROBE_TIMEOUT, token.as_deref())
                })
                .and_then(|output| beat(&output));
            let _ = send.send(ProbeOutcome {
                name: item.name,
                join: item.join,
                at: ProbeClock::now(origin),
                answer,
            });
        });
    if let Err(error) = spawned {
        let _ = done.send(ProbeOutcome {
            name,
            join,
            at: ProbeClock::now(origin),
            answer: Err(format!("the probe did not start: {error}")),
        });
    }
}

/// The worker's connection, and the open `node_lost` alerts it knows
/// (read once when it connects, then kept by its own writes).
struct NodeStoreHandle {
    store: PostgresStore,
    alerts: BTreeMap<String, String>,
}

impl NodeStoreHandle {
    fn connect(connector: &StoreConnector) -> Result<Self, String> {
        let store = connector
            .connect()
            .map_err(|error| format!("nodes: {error}"))?;
        let open = store
            .node_alerts()
            .map_err(|error| format!("nodes: the open alerts: {error}"))?;
        let alerts = open
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(key, subject)| {
                Some((
                    key.clone(),
                    subject.as_str()?.strip_prefix("node ")?.to_string(),
                ))
            })
            .collect();
        Ok(Self { store, alerts })
    }

    /// Write the proposals, in order, then the alerts the membership just
    /// read asks for ([`alert_plan`]). The first write that fails stops
    /// them; an alert the store holds already (fired, or not open to clear)
    /// is no failure.
    fn write(
        &mut self,
        watch: &mut HeartbeatScheduler,
        proposals: Vec<NodeRecordIntent>,
        members: Option<&Value>,
        lines: &mut Vec<String>,
    ) -> Result<(), String> {
        for proposal in proposals {
            self.store.node_record(&proposal.record).map_err(|error| {
                format!("nodes: {} not recorded: {error}", proposal.record["type"])
            })?;
            lines.extend(watch.applied(&proposal));
        }
        for record in members
            .map(|members| alert_plan(members, &self.alerts))
            .unwrap_or_default()
        {
            let key = record["key"].as_str().unwrap_or_default().to_string();
            let fired = record["type"] == "alert.fired";
            match self.store.alert_record(&record) {
                Ok(_) => lines.push(format!("nodes: {} {key}", record["type"])),
                Err(error) => {
                    let why = error.to_string();
                    if !["already fired", "not open", "was not fired"]
                        .iter()
                        .any(|settled| why.contains(settled))
                    {
                        return Err(format!("nodes: {key} not recorded: {why}"));
                    }
                }
            }
            if fired {
                let name = record["evidence"]["name"].as_str().unwrap_or_default();
                self.alerts.insert(key, name.to_string());
            } else {
                self.alerts.remove(&key);
            }
        }
        Ok(())
    }
}

/// The worker: read the membership, take the probes' ends, write what the
/// [`HeartbeatScheduler`] proposes and the alerts, start its probes. After a database
/// error it connects again after a pause, from [`PROBE_EVERY_MS`] doubling
/// to [`BACKOFF_MAX_MS`], said once. It ends when the loop does.
fn work(connector: &StoreConnector, lost_ms: i64, say: &Sender<String>) {
    let origin = Instant::now();
    let (done, ends) = std::sync::mpsc::channel();
    let mut watch = HeartbeatScheduler::new(lost_ms);
    let mut db: Option<NodeStoreHandle> = None;
    let (mut list_due, mut backoff) = (0_i64, 0_i64);
    let mut said: Option<String> = None;
    let mut waited: Option<ProbeOutcome> = None;
    loop {
        let now = ProbeClock::now(origin);
        let mut failed = None;
        let mut members = None;
        if now.mono >= list_due {
            if db.is_none() {
                match NodeStoreHandle::connect(connector) {
                    Ok(connected) => db = Some(connected),
                    Err(error) => failed = Some(error),
                }
            }
            if let Some(open) = &db {
                match open.store.node_members() {
                    Ok(read) => {
                        watch.sync(&read, now);
                        (backoff, said) = (0, None);
                        let every = if watch.is_empty() {
                            IDLE_LIST_EVERY_MS
                        } else {
                            PROBE_EVERY_MS
                        };
                        list_due = now.mono.saturating_add(every);
                        members = Some(read);
                    }
                    Err(error) => failed = Some(format!("nodes: {error}")),
                }
            }
        }
        // The ends after the read, so what they decide starts from it.
        for end in waited.take().into_iter().chain(ends.try_iter()) {
            watch.take(end);
        }
        let (proposals, probes, mut lines) = watch.decide(now);
        if failed.is_none()
            && let Some(open) = &mut db
            && let Err(error) = open.write(&mut watch, proposals, members.as_ref(), &mut lines)
        {
            failed = Some(error);
        }
        if let Some(error) = failed {
            db = None;
            watch.resync();
            backoff = (backoff * 2).clamp(PROBE_EVERY_MS, BACKOFF_MAX_MS);
            list_due = now.mono.saturating_add(backoff);
            if said.as_ref() != Some(&error) {
                lines.push(error.clone());
            }
            said = Some(error);
        }
        for item in probes {
            start(item, &done, origin);
        }
        for line in lines {
            if say.send(line).is_err() {
                return;
            }
        }
        let wait = if watch.is_empty() {
            list_due
                .saturating_sub(ProbeClock::now(origin).mono)
                .clamp(100, IDLE_LIST_EVERY_MS)
        } else {
            1000
        };
        waited = ends
            .recv_timeout(Duration::from_millis(wait.unsigned_abs()))
            .ok();
    }
}

#[cfg(test)]
mod hardening_tests;
#[cfg(test)]
mod tests;
