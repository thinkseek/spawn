//! spawn-node v1's owner commands (PR S4; design §2.5, §8 Stage 1), beside
//! the P1 nodes' and over the ledger of `spawn_node_state`:
//!
//! - `spawn node enroll-token <name> [--cell c0]`: a one-time token,
//!   printed once (`sne1.<base64url of 32 random bytes>.<base64url of the
//!   first 16 bytes of SHA-256(the primary's public key)>`); the store
//!   keeps only the SHA-256 of the whole token and an expiry 10 minutes
//!   ahead (`spawn_node_enroll_token`). The primary's public key is the
//!   gateway's (base64url, 32 bytes): `--primary-pubkey <key>`, else
//!   `--primary-pubkey-file <path>`, else `SPAWN_NODE_PRIMARY_PUBKEY`.
//! - `spawn node revoke <name>`: fences every lease it holds; the name is
//!   never enrolled again (`spawn_node_admin`).
//! - `spawn node list [--json]`: the P1 nodes as before, then the v1 ones
//!   (in JSON, appended to the same array, each with `"proto":"node-v1"`).
//! - `spawn node drain|undrain <name>`: a v1 node's through
//!   `spawn_node_admin` (an undrain also clears `degraded` and its
//!   strikes), a P1 node's as before. A name is never both (both writers
//!   check it).

use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use spawn_node_proto::PublicKey;
use spawn_node_proto::enroll::EnrollToken;
use spawn_postgres::{NodeAction, NodeStatus, NodeV1, PostgresStore};

use super::{USAGE, name_word, now_ms, open_store, power};

/// An enrollment token's life (the store refuses more than 10 minutes).
pub(super) const TOKEN_TTL_MS: i64 = 600_000;

/// The primary's public key when no flag names it (base64url).
pub(super) const PRIMARY_PUBKEY_ENV: &str = "SPAWN_NODE_PRIMARY_PUBKEY";

/// How long a v1 node may be silent before it is lost (the ledger's lease
/// time, `spawn_node_sweep`).
const LOST_AFTER_MS: i64 = 90_000;

/// 32 random bytes from the system's generator.
fn random_secret() -> Result<[u8; 32], String> {
    let mut secret = [0u8; 32];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut secret)
        .map_err(|_| "no random bytes from the system".to_string())?;
    Ok(secret)
}

/// The primary's public key from its base64url text.
fn primary_key(text: &str) -> Result<PublicKey, String> {
    PublicKey::from_b64u(text.trim()).map_err(|error| {
        format!("the primary's public key is not 32 bytes of base64url: {error:?}")
    })
}

/// A fresh token for `primary` and the SHA-256 the store keeps of it.
pub(super) fn new_token(primary: &PublicKey) -> Result<(EnrollToken, [u8; 32]), String> {
    let token = EnrollToken::new(random_secret()?, primary);
    let digest: [u8; 32] = Sha256::digest(token.as_str().as_bytes()).into();
    Ok((token, digest))
}

/// `enroll-token`'s words: the name, the cell, and where the primary's key
/// is (`Err` with the usage for anything else).
#[derive(Debug, PartialEq, Eq)]
pub(super) struct EnrollWords<'a> {
    pub(super) name: &'a str,
    pub(super) cell: Option<&'a str>,
    pub(super) key: Option<&'a str>,
    pub(super) key_file: Option<&'a str>,
}

pub(super) fn enroll_words(words: &[String]) -> Result<EnrollWords<'_>, String> {
    let (name, mut rest) = name_word(words)?;
    let mut parsed = EnrollWords {
        name,
        cell: None,
        key: None,
        key_file: None,
    };
    while let Some((flag, more)) = rest.split_first() {
        let Some((value, more)) = more.split_first() else {
            return Err(format!("{flag} needs a value\n{USAGE}"));
        };
        let slot = match flag.as_str() {
            "--cell" => &mut parsed.cell,
            "--primary-pubkey" => &mut parsed.key,
            "--primary-pubkey-file" => &mut parsed.key_file,
            _ => return Err(format!("unexpected argument {flag}\n{USAGE}")),
        };
        if slot.replace(value.as_str()).is_some() {
            return Err(format!("{flag} given twice\n{USAGE}"));
        }
        rest = more;
    }
    if let Some(cell) = parsed.cell {
        let mut chars = cell.chars();
        let ok = chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            && cell.len() <= 16
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !ok {
            return Err(format!(
                "a cell is 1 to 16 lower-case letters, digits or dashes, first a letter or \
                 digit\n{USAGE}"
            ));
        }
    }
    Ok(parsed)
}

/// Store a fresh token for `name` in `cell` (good [`TOKEN_TTL_MS`]); the
/// token, printed once by the caller, and when it expires.
pub(super) fn enroll_token(
    store: &PostgresStore,
    name: &str,
    cell: Option<&str>,
    primary: &PublicKey,
) -> Result<(EnrollToken, i64), String> {
    let (token, digest) = new_token(primary)?;
    let expires = store
        .node_enroll_token(name, cell, &digest, TOKEN_TTL_MS)
        .map_err(|error| error.to_string())?;
    Ok((token, expires))
}

/// `spawn node enroll-token <name> [--cell c0] [--primary-pubkey <key> |
/// --primary-pubkey-file <path>]`.
pub(super) fn enroll_token_command(words: &[String]) -> Result<i32, String> {
    let parsed = enroll_words(words)?;
    let text = match (parsed.key, parsed.key_file) {
        (Some(_), Some(_)) => {
            return Err(format!(
                "--primary-pubkey and --primary-pubkey-file: one, not both\n{USAGE}"
            ));
        }
        (Some(key), None) => key.to_string(),
        (None, Some(path)) => {
            std::fs::read_to_string(path).map_err(|error| format!("{path}: {error}"))?
        }
        (None, None) => crate::config::env_value(PRIMARY_PUBKEY_ENV).ok_or_else(|| {
            format!(
                "the primary's public key is needed: --primary-pubkey, --primary-pubkey-file \
                 or {PRIMARY_PUBKEY_ENV}"
            )
        })?,
    };
    let primary = primary_key(&text)?;
    let (token, expires) = enroll_token(&open_store()?, parsed.name, parsed.cell, &primary)?;
    // The token, once, alone on stdout; the rest on stderr.
    println!("{}", token.as_str());
    eprintln!(
        "node {}: enrollment token for cell {}, good until {expires} (ms; 10 minutes); \
         shown once: give it to `spawn-node enroll --token-file`",
        parsed.name,
        parsed.cell.unwrap_or("c0")
    );
    Ok(0)
}

/// `spawn node revoke <name>`: what happened, in a line.
pub(super) fn revoke(store: &PostgresStore, name: &str) -> Result<String, String> {
    let changed = store
        .node_admin(name, NodeAction::Revoke)
        .map_err(|error| error.to_string())?;
    Ok(if changed {
        format!("node {name}: revoked (its leases fenced; the name is never enrolled again)")
    } else {
        format!("node {name}: revoked already")
    })
}

pub(super) fn revoke_command(words: &[String]) -> Result<i32, String> {
    let (name, more) = name_word(words)?;
    if let Some(word) = more.first() {
        return Err(format!("unexpected argument {word}\n{USAGE}"));
    }
    println!("{}", revoke(&open_store()?, name)?);
    Ok(0)
}

/// A drain or an undrain of `name` when it is a v1 node: its line; None
/// when no v1 node has that name (a P1 node's, recorded as before).
pub(super) fn drain(
    store: &PostgresStore,
    name: &str,
    drain: bool,
) -> Result<Option<String>, String> {
    if store
        .node_v1(name)
        .map_err(|error| error.to_string())?
        .is_none()
    {
        return Ok(None);
    }
    let (action, verb) = if drain {
        (NodeAction::Drain, "drained")
    } else {
        (NodeAction::Undrain, "undrained")
    };
    let changed = store
        .node_admin(name, action)
        .map_err(|error| error.to_string())?;
    Ok(Some(if changed {
        format!("node {name}: {verb} (spawn-node v1)")
    } else {
        format!("node {name}: {verb} already (spawn-node v1)")
    }))
}

/// A v1 node's state in a word or two: `pending`, `revoked`, `lost`,
/// `active`, with `drained` and `degraded` added.
fn state_of(node: &NodeV1, now: i64) -> String {
    let mut state = if node.status == NodeStatus::Active && is_lost(node, now) {
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

fn is_lost(node: &NodeV1, now: i64) -> bool {
    node.status == NodeStatus::Active
        && node
            .last_beat_ms
            .is_none_or(|beat| now.saturating_sub(beat) > LOST_AFTER_MS)
}

/// A v1 node as `spawn node list --json` gives it.
pub(super) fn json_of(node: &NodeV1, now: i64) -> Value {
    json!({
        "name": node.name,
        "proto": "node-v1",
        "node_id": node.node_id,
        "cell_id": node.cell_id,
        "status": node.status,
        "state": state_of(node, now),
        "drained": node.drained,
        "degraded": node.degraded,
        "lost": is_lost(node, now),
        "host_fault_strikes": node.host_fault_strikes,
        "last_beat_ms": node.last_beat_ms,
        "spec_sha256": node.spec_sha256,
        "warm_bases": node.warm_bases,
        "power": node.power,
        "build_sha": node.build_sha,
        "node_version": node.node_version,
    })
}

/// A v1 node's build in a word: `<node_version>@<first 12 of build_sha>`,
/// either half alone when the other is unknown, none when both are.
fn build_summary(node: &NodeV1) -> Option<String> {
    let sha = node
        .build_sha
        .as_deref()
        .map(|sha| sha.chars().take(12).collect::<String>());
    match (node.node_version.as_deref(), sha) {
        (Some(version), Some(sha)) => Some(format!("{version}@{sha}")),
        (Some(version), None) => Some(version.to_string()),
        (None, Some(sha)) => Some(format!("@{sha}")),
        (None, None) => None,
    }
}

fn power_summary(power: &Value, low_battery_drain: bool) -> Option<String> {
    let map = power.as_object()?;
    let on_ac = map.get("on_ac").and_then(Value::as_bool)?;
    let mut words = vec![if on_ac { "AC" } else { "battery" }.to_string()];
    if let Some(pct) = map.get("battery_pct").and_then(Value::as_u64) {
        words.push(format!("{pct}%"));
    }
    if map.get("charging").and_then(Value::as_bool) == Some(true) {
        words.push("charging".to_string());
    } else if !on_ac {
        words.push("discharging".to_string());
    }
    if let Some(mins) = map.get("remaining_mins").and_then(Value::as_u64) {
        words.push(format!("{}h{:02}m", mins / 60, mins % 60));
    }
    if low_battery_drain {
        words.push("LOW-BATTERY DRAIN".to_string());
    }
    Some(words.join(" "))
}

/// The v1 nodes' table (nothing when there is none).
pub(super) fn lines(nodes: &[NodeV1], now: i64) -> Vec<String> {
    lines_with_threshold(nodes, now, power::low_battery_from_env())
}

fn lines_with_threshold(nodes: &[NodeV1], now: i64, threshold: u8) -> Vec<String> {
    if nodes.is_empty() {
        return Vec::new();
    }
    let row = |cells: [&str; 6]| {
        format!(
            "{:<16} {:<22} {:<6} {:>7} {:<14} {}",
            cells[0], cells[1], cells[2], cells[3], cells[4], cells[5]
        )
    };
    let mut out = vec![
        String::new(),
        "spawn-node v1:".to_string(),
        row(["NAME", "STATE", "CELL", "STRIKES", "NODE", "BEAT"]),
    ];
    for node in nodes {
        let mut beat = node.last_beat_ms.map_or_else(
            || "never".to_string(),
            |at| format!("{} s ago", now.saturating_sub(at) / 1000),
        );
        if let Some(node_power) = &node.power
            && let Some(summary) =
                power_summary(node_power, power::low_battery(node_power, threshold))
        {
            beat.push_str("  ");
            beat.push_str(&summary);
        }
        if let Some(build) = build_summary(node) {
            beat.push_str("  ");
            beat.push_str(&build);
        }
        let id: String = node
            .node_id
            .as_deref()
            .unwrap_or("-")
            .chars()
            .take(12)
            .collect();
        out.push(row([
            &node.name,
            &state_of(node, now),
            &node.cell_id,
            &node.host_fault_strikes.to_string(),
            &id,
            &beat,
        ]));
    }
    out
}

fn v1_json_with_soft_drain(v1: &[NodeV1], now: i64, threshold: u8) -> Vec<Value> {
    let originals: Vec<String> = v1.iter().map(|node| state_of(node, now)).collect();
    let mut array = Value::Array(
        v1.iter()
            .zip(&originals)
            .map(|(node, original_state)| {
                let mut obj = json_of(node, now);
                let mapped = if node.drained {
                    "drained"
                } else if node.status == NodeStatus::Active && !is_lost(node, now) && !node.degraded
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
    power::with_soft_drain(&mut array, threshold);
    let mut items = match array {
        Value::Array(items) => items,
        _ => Vec::new(),
    };
    for (entry, original_state) in items.iter_mut().zip(originals) {
        if let Some(fields) = entry.as_object_mut() {
            fields.insert("state".to_string(), Value::String(original_state));
        }
    }
    items
}

/// The P1 nodes' JSON array with the v1 nodes appended (soft-drained using
/// `SPAWN_NODE_LOW_BATTERY_PCT`).
#[cfg(test)]
pub(super) fn merged_json(p1: Value, v1: &[NodeV1], now: i64) -> Value {
    merged_json_with_threshold(p1, v1, now, power::low_battery_from_env())
}

pub(super) fn merged_json_with_threshold(
    p1: Value,
    v1: &[NodeV1],
    now: i64,
    threshold: u8,
) -> Value {
    let mut all = match p1 {
        Value::Array(nodes) => nodes,
        Value::Null => Vec::new(),
        other => vec![other],
    };
    all.extend(v1_json_with_soft_drain(v1, now, threshold));
    Value::Array(all)
}

/// `spawn node list [--json]`: the P1 nodes as before, then the v1 ones.
pub(super) fn list_command(words: &[String]) -> Result<i32, String> {
    let json = match words {
        [] => false,
        [flag] if flag == "--json" => true,
        _ => return Err(USAGE.to_string()),
    };
    let store = open_store()?;
    let threshold = power::low_battery_from_env();
    let mut nodes = store.nodes().map_err(|error| error.to_string())?;
    power::with_soft_drain(&mut nodes, threshold);
    // A store whose schema has no v1 table yet lists its P1 nodes alone.
    let v1 = store.nodes_v1().unwrap_or_else(|error| {
        eprintln!("spawn node list: the spawn-node v1 nodes not read: {error}");
        Vec::new()
    });
    let now = now_ms();
    if json {
        let nodes = if v1.is_empty() {
            nodes
        } else {
            merged_json_with_threshold(nodes, &v1, now, threshold)
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&nodes).unwrap_or_default()
        );
    } else {
        let p1_lines = super::lines(&nodes, now);
        let p1_empty = nodes.as_array().is_none_or(Vec::is_empty);
        if !p1_empty || v1.is_empty() {
            for line in p1_lines {
                println!("{line}");
            }
        }
        for line in lines(&v1, now) {
            println!("{line}");
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_string).collect()
    }

    fn node(name: &str) -> NodeV1 {
        NodeV1 {
            name: name.to_string(),
            node_id: Some(
                "56475aa75463474c0285df5dbf2bcab73da651358839e9b77481b2eab107708c".into(),
            ),
            cell_id: "c0".into(),
            status: NodeStatus::Active,
            drained: false,
            degraded: false,
            lost_recorded: false,
            host_fault_strikes: 0,
            boot_id: None,
            beat_seq: 0,
            last_beat_ms: Some(1_000_000),
            spec_sha256: None,
            spec: None,
            warm_bases: Vec::new(),
            power: None,
            build_sha: None,
            node_version: None,
        }
    }

    #[test]
    fn a_v1_nodes_build_sha_and_node_version_show_in_the_list_and_json() {
        let now = 1_050_000;
        let mut built = node("n1");
        built.build_sha = Some("e3f8ca5a66214f0a47f80878c2ceb97a76301dbb".into());
        built.node_version = Some("0.1.0".into());
        let mut version_only = node("n2");
        version_only.node_version = Some("0.1.0".into());
        let mut sha_only = node("n3");
        sha_only.build_sha = Some("abc1234".into());
        let legacy = node("n4");
        let v1 = vec![built, version_only, sha_only, legacy];

        let table = lines(&v1, now);
        assert!(
            table[3].ends_with("50 s ago  0.1.0@e3f8ca5a6621"),
            "{}",
            table[3]
        );
        assert!(table[4].ends_with("50 s ago  0.1.0"), "{}", table[4]);
        assert!(table[5].ends_with("50 s ago  @abc1234"), "{}", table[5]);
        assert!(table[6].ends_with("50 s ago"), "{}", table[6]);

        let merged = merged_json(Value::Null, &v1, now);
        let merged = merged.as_array().unwrap();
        assert_eq!(
            merged[0]["build_sha"],
            "e3f8ca5a66214f0a47f80878c2ceb97a76301dbb"
        );
        assert_eq!(merged[0]["node_version"], "0.1.0");
        assert_eq!(merged[1]["build_sha"], Value::Null);
        assert_eq!(merged[2]["node_version"], Value::Null);
        // A node whose daemon sends neither still carries both keys, null.
        let legacy = merged[3].as_object().unwrap();
        assert_eq!(legacy.get("build_sha"), Some(&Value::Null));
        assert_eq!(legacy.get("node_version"), Some(&Value::Null));
    }

    /// The fixture's primary key (design §7, keys.json).
    const PRIMARY: &str = "Kay64UG8yvCyLhqU000LxzYeUm0L_hLIl5S8kyKWbdc";

    #[test]
    fn a_token_is_sne1_32_random_bytes_and_the_primary_keys_pin() {
        let primary = primary_key(PRIMARY).unwrap();
        let (token, digest) = new_token(&primary).unwrap();
        let parts: Vec<&str> = token.as_str().split('.').collect();
        assert_eq!(parts.len(), 3, "{}", token.as_str());
        assert_eq!(parts[0], "sne1");
        // 32 bytes and 16 bytes, base64url without padding.
        assert_eq!(parts[1].len(), 43);
        assert_eq!(parts[2].len(), 22);
        assert!(!token.as_str().contains('='));
        // The pin is the first 16 bytes of the primary's key id (§7:
        // `JPbtasv-EAnAMNfKVnwzyg` for the fixture key).
        assert_eq!(parts[2], "JPbtasv-EAnAMNfKVnwzyg");
        assert!(token.pins(&primary));
        let parsed = EnrollToken::parse(token.as_str()).unwrap();
        assert_eq!(parsed.as_str(), token.as_str());
        // The store keeps the SHA-256 of the whole string.
        let expected: [u8; 32] = Sha256::digest(token.as_str().as_bytes()).into();
        assert_eq!(digest, expected);
        // Fresh every time.
        let (again, _) = new_token(&primary).unwrap();
        assert_ne!(again.as_str(), token.as_str());
        assert!(primary_key("not a key").is_err());
    }

    #[test]
    fn enroll_token_takes_a_name_a_cell_and_one_key_source() {
        let given = words("n252 --cell c1 --primary-pubkey k");
        let parsed = enroll_words(&given).unwrap();
        assert_eq!(
            parsed,
            EnrollWords {
                name: "n252",
                cell: Some("c1"),
                key: Some("k"),
                key_file: None
            }
        );
        assert_eq!(enroll_words(&words("n252")).unwrap().cell, None);
        for bad in [
            "",
            "-bad",
            "n252 --cell",
            "n252 --cell C0",
            "n252 --cell c0 --cell c1",
            "n252 --what x",
        ] {
            assert!(enroll_words(&words(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn v1_nodes_list_beside_the_p1_ones_without_changing_them() {
        let now = 1_050_000;
        let mut drained = node("n2");
        drained.drained = true;
        drained.degraded = true;
        drained.host_fault_strikes = 3;
        let mut silent = node("n3");
        silent.last_beat_ms = Some(100);
        let mut pending = node("n4");
        pending.status = NodeStatus::Pending;
        pending.node_id = None;
        pending.last_beat_ms = None;
        let v1 = vec![node("n1"), drained, silent, pending];
        assert_eq!(state_of(&v1[0], now), "active");
        assert_eq!(state_of(&v1[1], now), "active,drained,degraded");
        assert_eq!(state_of(&v1[2], now), "lost");
        assert_eq!(state_of(&v1[3], now), "pending");
        let table = lines(&v1, now);
        assert_eq!(table[1], "spawn-node v1:");
        assert!(table[2].starts_with("NAME"));
        assert!(table[3].starts_with("n1 "), "{}", table[3]);
        assert!(table[3].contains("50 s ago"), "{}", table[3]);
        assert!(table[3].contains("56475aa75463"));
        assert!(table[6].contains("never"));
        assert!(lines(&[], now).is_empty());

        // JSON: the P1 array kept as it is, the v1 nodes appended.
        let p1 = json!([{"name": "old", "state": "up"}]);
        let merged = merged_json(p1.clone(), &v1, now);
        let merged = merged.as_array().unwrap();
        assert_eq!(merged.len(), 5);
        assert_eq!(merged[0], p1[0]);
        assert_eq!(merged[1]["proto"], "node-v1");
        assert_eq!(merged[1]["status"], "active");
        assert_eq!(merged[2]["degraded"], true);
        assert_eq!(merged[2]["host_fault_strikes"], 3);
        assert_eq!(merged[3]["lost"], true);
        assert_eq!(merged[4]["status"], "pending");
        assert_eq!(merged[4]["node_id"], Value::Null);
        assert_eq!(merged_json(Value::Null, &[], now), json!([]));
    }

    /// The owner's commands on a real ledger: a token stored by digest
    /// (refused for a P1 name), drain and undrain of a v1 node through
    /// `spawn_node_admin` while a P1 name is left to the P1 path, revoke,
    /// and the list of both kinds.
    #[test]
    fn enroll_token_revoke_list_drain_and_undrain_on_the_ledger() {
        let schema = spawn_postgres::testing::TestSchema::new("nodes_v1_cli");
        let store = PostgresStore::connect_in_schema_with_options(
            schema.url(),
            &schema,
            &spawn_postgres::testing::test_connect_options(),
        )
        .expect("connect");
        let primary = primary_key(PRIMARY).unwrap();

        let (token, expires) = enroll_token(&store, "n252", Some("c0"), &primary).unwrap();
        assert!(token.as_str().starts_with("sne1."));
        assert!(expires > now_ms() - 60_000 && expires <= now_ms() + TOKEN_TTL_MS + 60_000);
        let pending = store.node_v1("n252").unwrap().expect("the pending node");
        assert_eq!(pending.status, NodeStatus::Pending);
        assert_eq!(pending.cell_id, "c0");
        // A new token for the same name replaces the old one.
        enroll_token(&store, "n252", None, &primary).unwrap();

        // A P1-joined name is refused by the store.
        let mut joined = super::super::inventory(
            "@arch\nx86_64\n@os\nGNU/Linux\n@kernel\n6.1\n@cores\n4\n@meminfo\n\
             MemTotal: 1024 kB\nMemAvailable: 512 kB\n@df\n\
             Filesystem 1024-blocks Used Available Capacity Mounted on\n\
             /dev/x 100 50 50 50% /\n@gpu\n@end\n",
        )
        .expect("an inventory");
        for (key, value) in [
            ("type", json!("node.joined")),
            ("name", json!("p1node")),
            ("ssh", json!("p1node")),
            ("note", Value::Null),
            ("exit_country", Value::Null),
        ] {
            joined[key] = value;
        }
        store.node_record(&joined).expect("a P1 join");
        assert!(enroll_token(&store, "p1node", None, &primary).is_err());

        // Drain and undrain: a v1 node through the ledger, twice each.
        assert_eq!(
            drain(&store, "n252", true).unwrap().as_deref(),
            Some("node n252: drained (spawn-node v1)")
        );
        assert!(store.node_v1("n252").unwrap().unwrap().drained);
        assert_eq!(
            drain(&store, "n252", true).unwrap().as_deref(),
            Some("node n252: drained already (spawn-node v1)")
        );
        assert_eq!(
            drain(&store, "n252", false).unwrap().as_deref(),
            Some("node n252: undrained (spawn-node v1)")
        );
        assert!(!store.node_v1("n252").unwrap().unwrap().drained);
        // A P1 name is not the v1 path's.
        assert_eq!(drain(&store, "p1node", true).unwrap(), None);
        assert_eq!(drain(&store, "nobody", false).unwrap(), None);

        // Both kinds listed.
        let v1 = store.nodes_v1().unwrap();
        assert_eq!(
            v1.iter().map(|node| node.name.as_str()).collect::<Vec<_>>(),
            ["n252"]
        );
        let p1 = store.nodes().unwrap();
        let merged = merged_json(p1.clone(), &v1, now_ms());
        let names: Vec<&str> = merged
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|node| node["name"].as_str())
            .collect();
        assert!(
            names.contains(&"p1node") && names.contains(&"n252"),
            "{names:?}"
        );
        assert_eq!(
            p1.as_array().unwrap().len() + 1,
            merged.as_array().unwrap().len()
        );

        // Revoke: once, then already; a revoked name gets no token again.
        assert!(
            revoke(&store, "n252")
                .unwrap()
                .contains("revoked (its leases")
        );
        assert_eq!(
            revoke(&store, "n252").unwrap(),
            "node n252: revoked already"
        );
        assert_eq!(
            store.node_v1("n252").unwrap().unwrap().status,
            NodeStatus::Revoked
        );
        assert!(enroll_token(&store, "n252", None, &primary).is_err());
        assert!(revoke(&store, "nobody").is_err());
    }

    #[test]
    fn v1_power_soft_drains_live_battery_nodes_and_renders_summary() {
        let now = 1_050_000;
        let mut low_batt = node("nbatt");
        low_batt.power = Some(json!({
            "on_ac": false,
            "battery_pct": 15,
            "charging": false,
            "remaining_mins": 30
        }));

        let mut on_ac = node("nac");
        on_ac.power = Some(json!({
            "on_ac": true,
            "battery_pct": 10,
            "charging": true,
            "remaining_mins": 45
        }));

        let mut degraded = node("ndeg");
        degraded.degraded = true;
        degraded.power = Some(json!({
            "on_ac": true,
            "battery_pct": 100,
            "charging": false,
            "remaining_mins": null
        }));

        let mut drained = node("ndrain");
        drained.drained = true;
        drained.power = Some(json!({
            "on_ac": true,
            "battery_pct": 100,
            "charging": false,
            "remaining_mins": null
        }));

        let v1 = [low_batt, on_ac, degraded, drained];
        let merged = merged_json_with_threshold(json!([]), &v1, now, 20);
        let rows = merged.as_array().unwrap();
        assert_eq!(rows.len(), 4);

        // On battery <= 20: soft-drained without mutating stored v1 `state`.
        assert_eq!(rows[0]["state"], "active");
        assert_eq!(rows[0]["power"]["battery_pct"], 15);
        assert_eq!(rows[0]["low_battery_drain"], true);
        assert_eq!(rows[0]["effective_drained"], true);
        assert_eq!(rows[0]["schedulable"], false);

        // Plugged in on AC even at 10%: not soft-drained, schedulable.
        assert_eq!(rows[1]["low_battery_drain"], false);
        assert_eq!(rows[1]["effective_drained"], false);
        assert_eq!(rows[1]["schedulable"], true);

        // Degraded node stays non-schedulable even on AC.
        assert_eq!(rows[2]["state"], "active,degraded");
        assert_eq!(rows[2]["low_battery_drain"], false);
        assert_eq!(rows[2]["schedulable"], false);

        // Drained node keeps original v1 state and reports effective_drained=true.
        assert_eq!(rows[3]["state"], "active,drained");
        assert_eq!(rows[3]["effective_drained"], true);
        assert_eq!(rows[3]["schedulable"], false);

        // Threshold 0 disables soft drain.
        let off = merged_json_with_threshold(json!([]), &v1[..1], now, 0);
        assert_eq!(off[0]["low_battery_drain"], false);
        assert_eq!(off[0]["effective_drained"], false);
        assert_eq!(off[0]["schedulable"], true);

        // Text table includes power summary and LOW-BATTERY DRAIN notice.
        let table = lines_with_threshold(&v1, now, 20);
        assert!(
            table[3].contains("battery 15% discharging 0h30m LOW-BATTERY DRAIN"),
            "{}",
            table[3]
        );
        assert!(table[4].contains("AC 10% charging 0h45m"), "{}", table[4]);
    }
}
