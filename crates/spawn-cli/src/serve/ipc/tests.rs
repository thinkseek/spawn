use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;

use serde_json::{Value, json};
use spawn_cas::{Digest, NS_ARTIFACTS, Store};
use spawn_core::event::{EventKind, NewEvent};
use spawn_core::id::{AgentId, SessionId};
use spawn_core::port::EventLog;
use spawn_postgres::PostgresStore;

use super::routes::{PgRole, PoolKind, canonical_ipc_routes};
use super::{IpcState, RoleUrls, bind_uds, spawn_ipc_server, verify_peer_uid};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "spawn-ipc-test-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn ipc_fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../spawn-core/testdata/spawn-ipc-v1")
}

fn exchange_raw(sock: &std::path::Path, raw: &[u8]) -> (u16, String, Vec<u8>) {
    let mut stream = UnixStream::connect(sock).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    stream.write_all(raw).unwrap();
    stream.flush().unwrap();

    let mut reader = BufReader::new(stream);
    let mut head = String::new();
    let mut status = 0_u16;
    let mut content_len = 0_usize;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).unwrap();
        assert!(n > 0, "EOF before end of headers: {head}");
        if line == "\r\n" {
            break;
        }
        if status == 0 {
            status = line
                .split_whitespace()
                .nth(1)
                .unwrap()
                .parse::<u16>()
                .unwrap();
        } else if let Some((k, v)) = line.split_once(':')
            && k.trim().eq_ignore_ascii_case("content-length")
        {
            content_len = v.trim().parse::<usize>().unwrap();
        }
        head.push_str(&line);
    }
    let mut body = vec![0_u8; content_len];
    if content_len > 0 {
        reader.read_exact(&mut body).unwrap();
    }
    (status, head, body)
}

#[test]
fn canonical_ipc_routes_match_golden_routes_json_and_never_allow_privileged_roles() {
    let routes_json: Value =
        serde_json::from_slice(&std::fs::read(ipc_fixture_dir().join("routes.json")).unwrap())
            .unwrap();
    let golden = routes_json["routes"].as_array().unwrap();
    let impl_routes = canonical_ipc_routes();
    assert_eq!(impl_routes.len(), golden.len());

    let mut expected_set = BTreeSet::new();
    for entry in golden {
        let method = entry["method"].as_str().unwrap();
        let pattern = entry["pattern"].as_str().unwrap();
        let pool = entry["pool"].as_str().unwrap();
        let role = entry["pg_role"].as_str().unwrap();
        let idem = entry["idempotency_key"].as_bool().unwrap();
        assert!(role != "owner" && role != "spawn_supervisor");
        expected_set.insert((method, pattern, pool, role, idem));
    }
    let actual_set: BTreeSet<_> = impl_routes
        .iter()
        .map(|r| {
            (
                r.method,
                r.pattern,
                r.pool.as_str(),
                r.role.as_str(),
                r.idempotency_required,
            )
        })
        .collect();
    assert_eq!(actual_set, expected_set);
    assert_ne!(PgRole::Reader.as_str(), "owner");
    assert_ne!(PgRole::Human.as_str(), "spawn_supervisor");
    assert_eq!(PoolKind::Node.as_str(), "node");
}

#[test]
fn bind_uds_atomic_sets_0600_and_verifies_peercred() {
    let dir = Scratch::new("bind");
    let sock_path = dir.0.join("nested").join("core.sock");
    let (listener, _lock) = bind_uds(&sock_path).unwrap();
    let mode = std::fs::metadata(&sock_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert!(dir.0.join("nested").join("core.sock.lock").exists());

    let dir_mode = std::fs::metadata(dir.0.join("nested"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(dir_mode, 0o700);

    let client = UnixStream::connect(&sock_path).unwrap();
    let (server_stream, _) = listener.accept().unwrap();
    verify_peer_uid(&server_stream).unwrap();
    drop(client);
}

#[test]
fn ipc_cas_bridge_pins_stats_and_reports_missing_under_flock() {
    let dir = Scratch::new("cas");
    let cas_root = dir.0.join("cas");
    let store = Store::open(&cas_root).unwrap();
    let payload = b"spawn-ipc-v1 cas test bytes";
    let digest = store.put_bytes(NS_ARTIFACTS, payload).unwrap();

    let stat_req = json!({
        "action": "stat",
        "digest": digest.to_string(),
    });
    let stat_resp = super::node::handle_node_cas(stat_req.to_string().as_bytes(), &cas_root);
    assert_eq!(stat_resp.status, 200);
    let stat_val: Value = serde_json::from_slice(&stat_resp.body).unwrap();
    assert_eq!(stat_val["status"], "ok");
    assert_eq!(stat_val["digest"], digest.to_string());
    assert_eq!(stat_val["size_bytes"], payload.len());

    let pin_req = json!({
        "action": "pin",
        "digest": digest.to_string(),
        "namespace": "artifacts",
        "ref": "gate-test-ref"
    });
    let pin_resp = super::node::handle_node_cas(pin_req.to_string().as_bytes(), &cas_root);
    assert_eq!(pin_resp.status, 200);
    assert!(store.pinned().unwrap().contains(&digest));

    let missing_digest = Digest::from_raw_sha256([0xee; 32]);
    let missing_req = json!({
        "action": "stat",
        "digest": missing_digest.to_string(),
    });
    let missing_resp = super::node::handle_node_cas(missing_req.to_string().as_bytes(), &cas_root);
    assert_eq!(missing_resp.status, 200);
    let missing_val: Value = serde_json::from_slice(&missing_resp.body).unwrap();
    assert_eq!(missing_val["status"], "missing");
}

#[test]
fn ipc_framing_enforces_reject_fixture_and_end_to_end_pg_routes() {
    let Ok(base_url) = std::env::var("SPAWN_TEST_DATABASE_URL") else {
        eprintln!("SPAWN_TEST_DATABASE_URL unset; skipping PG IPC end-to-end assertions");
        return;
    };
    let dir = Scratch::new("pg-e2e");
    let sock_path = dir.0.join("core.sock");
    let cas_root = dir.0.join("cas");
    let _cas = Store::open(&cas_root).unwrap();

    let schema = spawn_postgres::testing::TestSchema::new("serve_ipc");
    let store = PostgresStore::connect_in_schema(&base_url, &schema).unwrap();

    let agent = AgentId::new("agent_ipc_1").unwrap();
    let session = SessionId::new("session_ipc_1").unwrap();
    EventLog::append(
        &store,
        NewEvent {
            agent_id: agent,
            session_id: session,
            execution_id: None,
            source: spawn_core::event::Source::Bridge,
            source_event_id: Some("ipc-seed-1".to_string()),
            kind: EventKind::MessageReceived,
        },
    )
    .unwrap();

    let sep = if base_url.contains('?') { '&' } else { '?' };
    let owner_in_schema = format!("{base_url}{sep}options=-csearch_path%3D{schema}");
    let urls = RoleUrls::from_owner(&owner_in_schema).unwrap();
    assert_ne!(urls.reader.as_str(), base_url);
    assert_ne!(urls.reader.as_str(), owner_in_schema);
    let server = spawn_ipc_server(sock_path.clone(), urls, cas_root, None).unwrap();

    verify_reject_fixture(&sock_path);
    verify_e2e_routes_and_events(&sock_path, &store);

    let state_only = IpcState::new(
        RoleUrls::from_explicit(
            owner_in_schema.clone(),
            owner_in_schema.clone(),
            owner_in_schema,
        ),
        dir.0.join("cas2"),
        None,
    );
    let mut permits = Vec::new();
    for _ in 0..super::WRITES_POOL_SIZE {
        permits.push(state_only.pools.writes_gate.try_acquire().unwrap());
    }
    assert!(state_only.pools.writes_gate.try_acquire().is_none());
    drop(permits);

    drop(state_only);
    drop(server);
    drop(store);
}

fn verify_reject_fixture(sock_path: &std::path::Path) {
    let rejects: Value =
        serde_json::from_slice(&std::fs::read(ipc_fixture_dir().join("reject.json")).unwrap())
            .unwrap();
    for item in rejects.as_array().unwrap() {
        let Some(path) = item["path"].as_str() else {
            continue;
        };
        let case = item["case"].as_str().unwrap();
        let expected_status = u16::try_from(item["status"].as_u64().unwrap()).unwrap();
        let headers = item["headers"].as_object().unwrap();
        let mut raw = format!("POST {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
        let declared_len = headers
            .get("Content-Length")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<usize>().ok());
        let body_str = if let Some(obj) = item.get("body_json").filter(|obj| !obj.is_null()) {
            obj.to_string()
        } else if matches!(declared_len, Some(len) if len <= super::http::MAX_IPC_BODY_BYTES) {
            "{}".repeat(declared_len.unwrap() / 2)
                + if declared_len.unwrap() % 2 == 1 {
                    " "
                } else {
                    ""
                }
        } else {
            String::new()
        };
        for (k, v) in headers {
            if k == "Idempotency-Key" && v.as_str().unwrap().contains('\n') {
                raw.push_str("Idempotency-Key: bad\x07key\r\n");
            } else if k == "Content-Length"
                && !body_str.is_empty()
                && declared_len.is_some_and(|l| l <= super::http::MAX_IPC_BODY_BYTES)
            {
                let _ = write!(raw, "Content-Length: {}\r\n", body_str.len());
            } else {
                let _ = write!(raw, "{k}: {}\r\n", v.as_str().unwrap());
            }
        }
        raw.push_str("\r\n");
        raw.push_str(&body_str);
        let (st, _, resp_body) = exchange_raw(sock_path, raw.as_bytes());
        assert_eq!(
            st,
            expected_status,
            "reject case {case}: {}",
            String::from_utf8_lossy(&resp_body)
        );
    }
}

fn verify_e2e_routes_and_events(sock_path: &std::path::Path, store: &PostgresStore) {
    let (st, _, body) = exchange_raw(
        sock_path,
        b"GET /ipc/v1/hello HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert_eq!(st, 200);
    let hello: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(hello["proto"], "spawn-ipc-v1");
    assert_eq!(hello["schema"], 1);
    assert!(hello["latest_id"].as_u64().unwrap() >= 1);

    let (st, head, _) = exchange_raw(
        sock_path,
        b"GET /api/overview HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert_eq!(st, 200);
    let etag = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("etag")
                .then(|| v.trim().to_string())
        })
        .expect("weak ETag on /api/overview");
    let conditional =
        format!("GET /api/overview HTTP/1.1\r\nHost: localhost\r\nIf-None-Match: {etag}\r\n\r\n");
    let (st304, _, body304) = exchange_raw(sock_path, conditional.as_bytes());
    assert_eq!(st304, 304);
    assert!(body304.is_empty());

    let vectors: Value = serde_json::from_slice(
        &std::fs::read(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../spawn-node-proto/testdata/node-protocol-v1/vectors.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let tok_hex = vectors["enroll_token_sha256"].as_str().unwrap();
    let token_bytes: [u8; 32] = spawn_core::node_proto::codec::unhex(tok_hex, 64)
        .unwrap()
        .try_into()
        .unwrap();
    store
        .node_enroll_token("n252", Some("c0"), &token_bytes, 600_000)
        .unwrap();

    let enroll_req = std::fs::read(ipc_fixture_dir().join("node.enroll.req.json")).unwrap();
    let mut full_enroll = format!(
        "POST /ipc/v1/node/enroll HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nIdempotency-Key: enroll-1\r\n\r\n",
        enroll_req.len()
    )
    .into_bytes();
    full_enroll.extend_from_slice(&enroll_req);
    let (st, _, body) = exchange_raw(sock_path, &full_enroll);
    assert_eq!(st, 200);
    let enroll_ans: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(enroll_ans["ok"], true, "{enroll_ans}");

    let key_req = std::fs::read(ipc_fixture_dir().join("node.key.req.json")).unwrap();
    let mut full_key = format!(
        "POST /ipc/v1/node/key HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nIdempotency-Key: key-1\r\n\r\n",
        key_req.len()
    )
    .into_bytes();
    full_key.extend_from_slice(&key_req);
    let (st, _, body) = exchange_raw(sock_path, &full_key);
    assert_eq!(st, 200);
    let key_ans: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(key_ans["node_id"], vectors["node_id"]);
    assert_eq!(key_ans["revoked"], false);
    assert!(key_ans["pubkey_b64"].as_str().unwrap().len() >= 43);

    let mut ev_stream = UnixStream::connect(sock_path).unwrap();
    ev_stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    ev_stream
        .write_all(b"GET /ipc/v1/events?after=0 HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let mut ev_reader = BufReader::new(ev_stream);
    loop {
        let mut h = String::new();
        ev_reader.read_line(&mut h).unwrap();
        if h == "\r\n" {
            break;
        }
    }
    let mut ready_line = String::new();
    ev_reader.read_line(&mut ready_line).unwrap();
    let ready_val: Value = serde_json::from_str(&ready_line).unwrap();
    assert_eq!(ready_val["type"], "ready");
    assert_eq!(ready_val["replayed"], 2);
    let mut event_line = String::new();
    ev_reader.read_line(&mut event_line).unwrap();
    let event_val: Value = serde_json::from_str(&event_line).unwrap();
    assert_eq!(event_val["type"], "event");
    assert_eq!(event_val["session_id"], "session_ipc_1");

    verify_e2e_nodes_and_episodes(sock_path);
}

fn verify_e2e_nodes_and_episodes(sock_path: &std::path::Path) {
    // Record a beat carrying build_sha, node_version, and power over IPC, then
    // confirm GET /api/nodes surfaces enrolled v1 node n252 without pubkey and
    // GET /api/episodes?limit=5 returns the bounded contract JSON payload.
    let mut beat_req: Value = serde_json::from_slice(
        &std::fs::read(ipc_fixture_dir().join("node.beat.req.json")).unwrap(),
    )
    .unwrap();
    let beat_sha = "0123456789abcdef0123456789abcdef01234567";
    beat_req["beat"]["build_sha"] = json!(beat_sha);
    beat_req["beat"]["node_version"] = json!("0.1.0");
    beat_req["beat"]["power"] = json!({
        "on_ac": true,
        "battery_pct": 100,
        "charging": false,
        "remaining_mins": null
    });
    let beat_bytes = beat_req.to_string();
    let full_beat = format!(
        "POST /ipc/v1/node/beat HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nIdempotency-Key: beat-1\r\n\r\n{}",
        beat_bytes.len(),
        beat_bytes
    );
    let (st, _, body) = exchange_raw(sock_path, full_beat.as_bytes());
    assert_eq!(st, 200);
    let beat_ans: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(beat_ans["ok"], true, "{beat_ans}");

    let (st, _, body) = exchange_raw(
        sock_path,
        b"GET /api/nodes HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert_eq!(st, 200);
    let nodes_ans: Value = serde_json::from_slice(&body).unwrap();
    let n252 = nodes_ans["nodes"]
        .as_array()
        .expect("nodes array")
        .iter()
        .find(|n| n["name"] == "n252")
        .expect("n252 surfaced in /api/nodes");
    assert_eq!(n252["proto"], "node-v1");
    assert_eq!(n252["build_sha"], beat_sha);
    assert_eq!(n252["node_version"], "0.1.0");
    assert_eq!(n252["state"], "active");
    assert_eq!(n252["lost"], false);
    assert_eq!(n252["schedulable"], true);
    assert_eq!(n252["host_fault_strikes"], 0);
    assert!(n252["last_beat_ms"].as_i64().is_some_and(|ms| ms > 0));
    assert_eq!(n252["power"]["on_ac"], true);
    assert!(n252.get("pubkey").is_none());

    let (st, _, body) = exchange_raw(
        sock_path,
        b"GET /api/episodes?limit=5 HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert_eq!(st, 200);
    let ep_ans: Value = serde_json::from_slice(&body).unwrap();
    assert!(ep_ans["mission"].is_null());
    assert!(ep_ans["episodes"].is_array());
    assert!(ep_ans["summary"].is_object());
    assert_eq!(ep_ans["parity_ok"], true);
    assert_eq!(ep_ans["warnings"], 0);
    assert_eq!(ep_ans["drift_count"], 0);
}
