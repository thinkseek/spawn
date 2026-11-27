//! `spawn node add --ssh` and `spawn node probe`: SSH-only P1 node commands
//! (the superseded Issue #85/#87 sync mesh commands were retired in Phase 2d
//! in favor of Go `spawn-node`).

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::transport::{self, Op};
use super::{Flags, JOIN_TIMEOUT, USAGE, inventory, name_word, open_store, valid_ssh};

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

/// What `spawn node add` was told of an SSH node: its SSH destination and
/// its jump-host hops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AddTarget {
    pub(crate) destination: String,
    pub(crate) via: Vec<String>,
}

/// `spawn node add`'s flags: `--ssh`, optional `--via`, optional `--note`.
/// Retired Issue #85/#87 mesh flags (`--tcp`, `--udp`, `--coord`,
/// `--node-id`, `--peer`) fail with a clear retirement message pointing to
/// Go `spawn-node`.
pub(crate) fn add_target(name: &str, flags: &Flags) -> Result<AddTarget, String> {
    for retired in ["tcp", "udp", "coord", "node-id", "peer"] {
        if flags.get(retired).is_some() {
            return Err(format!(
                "--{retired} has been retired in favor of Go `spawn-node` (use --ssh or `spawn node enroll-token`)\n{USAGE}"
            ));
        }
    }
    if let Some(unknown) = flags
        .names()
        .find(|flag| !matches!(*flag, "ssh" | "via" | "note"))
    {
        return Err(format!("spawn node add has no --{unknown}\n{USAGE}"));
    }
    let Some(destination) = flags.get("ssh") else {
        return Err(format!("spawn node add requires --ssh\n{USAGE}"));
    };
    if !valid_ssh(&destination) {
        return Err(
            "--ssh is [user@]host or ssh://[user@]host[:port], the port 1 to 65535".to_string(),
        );
    }
    let via = via_names(flags.get("via").as_deref(), name)?;
    Ok(AddTarget { destination, via })
}

/// The `node.joined` of `name` reached over SSH as `target`, from its inventory.
pub(crate) fn joined_record(
    name: &str,
    target: &AddTarget,
    mut facts: Value,
    note: Option<&str>,
) -> Value {
    facts["type"] = json!("node.joined");
    facts["name"] = json!(name);
    facts["ssh"] = json!(target.destination);
    facts["transport"] = json!("ssh");
    facts["endpoint"] = Value::Null;
    facts["via"] = json!(target.via);
    facts["mode"] = json!(if target.via.is_empty() {
        "direct"
    } else {
        "relay"
    });
    facts["note"] = json!(note);
    facts["exit_country"] = Value::Null;
    facts
}

/// `spawn node add <name> --ssh <dest> [--via <hop>[,<hop>...]] [--note <text>]`:
/// check hops against the membership (no loop), run the inventory over SSH,
/// and record `node.joined`.
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
    let failed = |error: String| format!("the preflight of {name} over ssh failed: {error}");
    let route = transport::route(&target.destination, &target.via, &members)
        .map_err(|error| format!("--via: {error}"))?;
    let output = transport::probe_route(&route, Op::Inventory, JOIN_TIMEOUT).map_err(failed)?;
    let facts = inventory(&output).map_err(failed)?;
    let record = joined_record(name, &target, facts, flags.get("note").as_deref());
    let event = store
        .node_record(&record)
        .map_err(|error| error.to_string())?;
    println!("node {name} joined (event {event})");
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

/// `spawn node probe <name>`: one probe of a joined SSH node over its route,
/// its output said, or read as JSON; nothing recorded.
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
        .route(&members)
        .map_err(|error| format!("node {name}: {error}"))?;
    let begun = Instant::now();
    let output = transport::probe_route(&route, op, JOIN_TIMEOUT)
        .map_err(|error| format!("node {name}: {error}"))?;
    if json_out {
        let out = probe_json(name, op, &member["transport"], &output, begun.elapsed())?;
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
    } else {
        print!("{output}");
    }
    Ok(0)
}
