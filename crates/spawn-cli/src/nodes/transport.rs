//! How the primary reaches a P1 node: over SSH (directly, or through up to
//! [`MAX_HOPS`] joined SSH jump hosts via nested `ProxyCommand`).
//!
//! Issue #113 Phase 2d retired the Issue #85/#87 Rust sync TCP/UDP/coordinator
//! mesh in favor of Go `spawn-node` (`nodes/v1.rs`). Historical non-SSH node
//! records that remain in `spawn_events` are recognized cleanly as retired
//! without panicking or opening network sockets.

use std::time::Duration;

use serde_json::Value;

use super::{BEAT, INVENTORY, bounded, ssh_command_via, valid_name};

/// The most jump-host hops between the primary and an SSH node.
pub(crate) const MAX_HOPS: usize = 8;

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

/// The way to a node: SSH with its jump hosts (a nested `ProxyCommand`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Route {
    Ssh {
        destination: String,
        jumps: Vec<String>,
    },
}

/// One joined node as the store's membership says it (`node_members`):
/// its transport name (`"ssh"` for SSH nodes and pre-transport joins, or a
/// retired transport name from historical records), its destination, and its
/// hops.
pub(crate) struct Member<'a> {
    pub(crate) transport: &'a str,
    pub(crate) endpoint: Option<&'a str>,
    pub(crate) via: Vec<String>,
}

impl<'a> Member<'a> {
    pub(crate) fn of(member: &'a Value) -> Self {
        let transport = member["transport"].as_str().unwrap_or("ssh");
        let endpoint = member["endpoint"].as_str().or(member["ssh"].as_str());
        let via = member["via"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|hop| hop.as_str().map(str::to_string))
            .collect();
        Self {
            transport,
            endpoint,
            via,
        }
    }

    /// The route to this member over SSH, or why it cannot be probed
    /// (missing destination, missing/invalid jump host, or a retired non-SSH
    /// transport from historical records).
    pub(crate) fn route(&self, members: &Value) -> Result<Route, String> {
        if self.transport != "ssh" {
            return Err(format!(
                "transport {:?} is retired in favor of Go spawn-node (re-join with --ssh or use spawn-node v1)",
                self.transport
            ));
        }
        let destination = self
            .endpoint
            .ok_or_else(|| "an ssh node without its ssh destination".to_string())?;
        route(destination, &self.via, members)
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
                .map(|m| Member::of(m).via)
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

/// The SSH route to a node at `destination` through the joined jump-host
/// hops `via` (each resolved from the membership).
pub(crate) fn route(destination: &str, via: &[String], members: &Value) -> Result<Route, String> {
    hop_names(via, "")?;
    if !super::valid_ssh(destination) {
        return Err("not an ssh destination".to_string());
    }
    let mut jumps = Vec::new();
    for hop in via {
        let member = find(members, hop)
            .map(Member::of)
            .ok_or_else(|| format!("hop {hop} is not joined"))?;
        if member.transport != "ssh" {
            return Err(format!(
                "hop {hop} is reached over {}: the hops of an ssh node are ssh nodes",
                member.transport
            ));
        }
        let hop_dest = member
            .endpoint
            .filter(|dest| super::valid_ssh(dest))
            .ok_or_else(|| format!("hop {hop} has no ssh destination"))?;
        jumps.push(hop_dest.to_string());
    }
    Ok(Route::Ssh {
        destination: destination.to_string(),
        jumps,
    })
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

/// Ask `op` of the node at the end of `route`, bounded by `timeout`: its
/// output, or why not.
pub(crate) fn probe_route(route: &Route, op: Op, timeout: Duration) -> Result<String, String> {
    match route {
        Route::Ssh { destination, jumps } => {
            if !super::valid_ssh(destination) || !jumps.iter().all(|jump| super::valid_ssh(jump)) {
                return Err("not an ssh destination".to_string());
            }
            bounded(ssh_command_via(destination, jumps, op.script()), timeout)
        }
    }
}

#[cfg(test)]
mod tests;
