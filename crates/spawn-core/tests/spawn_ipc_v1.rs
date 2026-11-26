//! Contract validation for `docs/spawn-ipc-v1.md` and the frozen golden
//! fixtures in `testdata/spawn-ipc-v1/`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::PathBuf;

use serde_json::Value;
use sha2::{Digest as _, Sha256};

const MAX_IPC_BODY_BYTES: usize = 4 * 1024 * 1024;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/spawn-ipc-v1")
}

fn read_bytes(name: &str) -> Vec<u8> {
    std::fs::read(fixture_dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn read_json(name: &str) -> Value {
    serde_json::from_slice(&read_bytes(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for b in digest {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[test]
fn spawn_ipc_v1_manifest_matches_all_fixtures() {
    let dir = fixture_dir();
    let manifest = std::fs::read_to_string(dir.join("MANIFEST.sha256")).unwrap();
    let mut listed = BTreeSet::new();
    for line in manifest.lines() {
        let (expected_hash, name) = line
            .split_once("  ")
            .unwrap_or_else(|| panic!("bad manifest line: {line}"));
        assert!(is_lower_hex(expected_hash, 64), "bad sha256 in manifest");
        assert!(listed.insert(name.to_string()), "duplicate entry: {name}");
        let actual_hash = sha256_hex(&read_bytes(name));
        assert_eq!(actual_hash, expected_hash, "sha256 mismatch for {name}");
    }

    let mut actual_files = BTreeSet::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        if name != "MANIFEST.sha256" {
            actual_files.insert(name);
        }
    }
    assert_eq!(listed, actual_files, "manifest must list every fixture");
}

#[test]
fn spawn_ipc_v1_route_table_enforces_roles_pools_and_caps() {
    let table = read_json("routes.json");
    assert_eq!(table["proto"], "spawn-ipc-v1");
    assert_eq!(table["max_body_bytes"], MAX_IPC_BODY_BYTES);
    assert_eq!(table["max_ndjson_line_bytes"], MAX_IPC_BODY_BYTES);
    assert!(
        table["go_idle_conn_timeout_s"].as_u64().unwrap()
            < table["rust_idle_timeout_s"].as_u64().unwrap(),
        "Go IdleConnTimeout must be strictly less than Rust idle timeout"
    );

    let pools = table["pools"].as_object().unwrap();
    assert_eq!(pools["reads"], 16);
    assert_eq!(pools["writes"], 4);
    assert_eq!(pools["node"], 16);
    assert_eq!(pools["events"], 2);

    let routes = table["routes"].as_array().unwrap();
    assert_eq!(routes.len(), 50, "33 GET + 9 POST /api + 8 /ipc/v1 routes");

    let allowed_roles = ["spawn_reader", "spawn_human", "spawn_node_gateway", "none"];
    let mut seen: BTreeMap<(String, String), (String, String, bool)> = BTreeMap::new();
    for route in routes {
        let method = route["method"].as_str().unwrap().to_string();
        let pattern = route["pattern"].as_str().unwrap().to_string();
        let pool = route["pool"].as_str().unwrap().to_string();
        let pg_role = route["pg_role"].as_str().unwrap().to_string();
        let idempotency = route["idempotency_key"].as_bool().unwrap();

        assert!(
            allowed_roles.contains(&pg_role.as_str()),
            "disallowed pg_role {pg_role} on {method} {pattern}"
        );
        assert!(
            pools.contains_key(&pool),
            "unknown pool {pool} on {method} {pattern}"
        );
        if method == "POST" {
            assert!(idempotency, "POST {pattern} must require Idempotency-Key");
        } else {
            assert_eq!(method, "GET");
            assert!(
                !idempotency,
                "GET {pattern} must not require Idempotency-Key"
            );
        }
        assert!(
            seen.insert(
                (method.clone(), pattern.clone()),
                (pool, pg_role, idempotency)
            )
            .is_none(),
            "duplicate route {method} {pattern}"
        );
    }

    let get_api = seen
        .keys()
        .filter(|(m, p)| m == "GET" && p.starts_with("/api/"))
        .count();
    let post_api = seen
        .keys()
        .filter(|(m, p)| m == "POST" && p.starts_with("/api/"))
        .count();
    let ipc_v1 = seen
        .keys()
        .filter(|(_, p)| p.starts_with("/ipc/v1/"))
        .count();
    assert_eq!(get_api, 33);
    assert_eq!(post_api, 9);
    assert_eq!(ipc_v1, 8);
}

#[test]
fn spawn_ipc_v1_payload_and_stream_fixtures_validate() {
    for name in [
        "hello.res.json",
        "node.key.req.json",
        "node.key.res.json",
        "node.enroll.req.json",
        "node.beat.req.json",
        "node.spec.req.json",
        "node.result.req.json",
        "node.cas.req.json",
        "node.cas.res.json",
    ] {
        let raw = read_bytes(name);
        assert!(raw.len() <= MAX_IPC_BODY_BYTES, "{name} exceeds 4 MiB cap");
    }

    let hello = read_json("hello.res.json");
    assert_eq!(hello["proto"], "spawn-ipc-v1");
    assert_eq!(hello["schema"], 1);
    assert!(hello["latest_id"].as_u64().is_some());

    let key_query = read_json("node.key.req.json");
    let key_answer = read_json("node.key.res.json");
    assert!(is_lower_hex(key_query["node_id"].as_str().unwrap(), 64));
    assert_eq!(key_query["node_id"], key_answer["node_id"]);
    assert!(!key_answer["pubkey_b64"].as_str().unwrap().is_empty());

    let enroll = read_json("node.enroll.req.json");
    assert!(is_lower_hex(
        enroll["token_sha256_hex"].as_str().unwrap(),
        64
    ));
    assert!(is_lower_hex(enroll["spec_sha256"].as_str().unwrap(), 64));
    assert!(enroll["spec"].is_object());

    let beat = read_json("node.beat.req.json");
    assert!(is_lower_hex(beat["node_id"].as_str().unwrap(), 64));
    assert!(beat["seq"].as_u64().unwrap() >= 1);
    assert!(beat["beat"].is_object());

    let spec = read_json("node.spec.req.json");
    assert_eq!(spec["spec_sha256"], enroll["spec_sha256"]);
    assert!(spec["spec"].is_object());

    let result = read_json("node.result.req.json");
    assert!(result["lease_epoch"].as_u64().unwrap() >= 1);
    assert!(result["result"].is_object());

    let cas_request = read_json("node.cas.req.json");
    let cas_response = read_json("node.cas.res.json");
    assert_eq!(cas_request["action"], "pin");
    assert_eq!(cas_request["digest"], cas_response["digest"]);
    assert_eq!(cas_response["status"], "ok");

    let ndjson = String::from_utf8(read_bytes("events.ndjson")).unwrap();
    assert!(ndjson.ends_with('\n'));
    let mut kinds = Vec::new();
    let mut prev_id = None;
    let mut prev_ts = None;
    for line in ndjson.lines() {
        assert!(line.len() <= MAX_IPC_BODY_BYTES);
        let obj: Value = serde_json::from_str(line).unwrap();
        let kind = obj["type"].as_str().unwrap().to_string();
        let ts = obj["recorded_at_ms"].as_i64().unwrap();
        if let Some(prev) = prev_ts {
            assert!(ts >= prev, "cursor recorded_at_ms must not go backward");
        }
        prev_ts = Some(ts);
        if kind == "event" {
            let id = obj["id"].as_u64().unwrap();
            if let Some(prev) = prev_id {
                assert!(id > prev, "event ids must increase strictly");
            }
            prev_id = Some(id);
        }
        kinds.push(kind);
    }
    assert_eq!(kinds, vec!["ready", "event", "event", "hb", "resync"]);

    let rejects = read_json("reject.json");
    assert!(rejects.as_array().unwrap().len() >= 8);
}
