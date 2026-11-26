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

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read_repo_file(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn parse_docs_api_routes(doc: &str) -> Vec<(String, String)> {
    let mut routes = Vec::new();
    for line in doc.lines() {
        for method in ["GET", "POST"] {
            if line.starts_with(&format!("| {method} | `")) {
                let path = line
                    .split('`')
                    .nth(1)
                    .unwrap_or_else(|| panic!("missing backtick path in docs/api.md line: {line}"));
                routes.push((method.to_string(), path.to_string()));
            }
        }
    }
    routes
}

fn extract_quoted_tokens(block: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for line in block.lines() {
        let code = line.split("//").next().unwrap_or("");
        let mut parts = code.split('"');
        let _ = parts.next();
        while let Some(quoted) = parts.next() {
            tokens.push(quoted.to_string());
            if parts.next().is_none() {
                break;
            }
        }
    }
    tokens
}

fn parse_serve_routes(serve_rs: &str) -> Vec<(String, String)> {
    let start = serve_rs
        .find("pub(super) const ROUTES: &[(&str, &str)] = &[")
        .expect("serve.rs must define ROUTES");
    let tail = &serve_rs[start..];
    let end = tail.find("];").expect("serve.rs ROUTES must terminate");
    let tokens = extract_quoted_tokens(&tail[..end]);
    assert!(
        tokens.len().is_multiple_of(2) && !tokens.is_empty(),
        "ROUTES tokens must come in (method, path) pairs"
    );
    tokens
        .chunks_exact(2)
        .map(|pair| (pair[0].clone(), pair[1].clone()))
        .collect()
}

fn parse_static_ipc_routes(routes_rs: &str) -> BTreeSet<(String, String)> {
    let start = routes_rs
        .find("const STATIC_IPC_ROUTES: &[IpcRoute] = &[")
        .expect("ipc/routes.rs must define STATIC_IPC_ROUTES");
    let tail = &routes_rs[start..];
    let end = tail
        .find("];")
        .expect("ipc/routes.rs STATIC_IPC_ROUTES must terminate");
    let mut out = BTreeSet::new();
    let mut method: Option<String> = None;
    for line in tail[..end].lines() {
        let trimmed = line.trim();
        if let Some(m) = trimmed
            .strip_prefix("method: \"")
            .and_then(|s| s.strip_suffix("\","))
        {
            method = Some(m.to_string());
        } else if let Some(p) = trimmed
            .strip_prefix("pattern: \"")
            .and_then(|s| s.strip_suffix("\","))
        {
            let m = method.take().expect("method before pattern");
            assert!(
                out.insert((m.clone(), p.to_string())),
                "duplicate STATIC_IPC_ROUTES entry {m} {p}"
            );
        }
    }
    out
}

fn collapse_rust_line_continuations(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(pos) = rest.find("\\\n") {
        out.push_str(&rest[..pos]);
        rest = rest[pos + 2..].trim_start_matches([' ', '\t']);
    }
    out.push_str(rest);
    out
}

/// Issue #98 Item 2: cross-check HTTP `/api/*` canonical route inventory and
/// `/ipc/v1/*` routes across `routes.json`, `docs/api.md`, `serve.rs`, and
/// `serve/ipc/routes.rs`, plus the read-only out-of-table endpoints
/// `/api/features/truth` and `/api/episodes`.
#[test]
fn http_api_routes_docs_serve_and_readonly_out_of_table_endpoints_stay_locked() {
    let table = read_json("routes.json");
    let docs_api = read_repo_file("docs/api.md");
    let serve_rs = read_repo_file("crates/spawn-cli/src/serve.rs");
    let ipc_routes_rs = read_repo_file("crates/spawn-cli/src/serve/ipc/routes.rs");

    let serve_routes = parse_serve_routes(&serve_rs);
    let docs_routes = parse_docs_api_routes(&docs_api);
    assert_eq!(
        serve_routes, docs_routes,
        "serve.rs::ROUTES must stay synchronized with docs/api.md tables"
    );

    let mut fixture_api = BTreeSet::new();
    let mut fixture_ipc = BTreeSet::new();
    for route in table["routes"].as_array().expect("routes array") {
        let method = route["method"].as_str().unwrap().to_string();
        let pattern = route["pattern"].as_str().unwrap().to_string();
        if pattern.starts_with("/api/") {
            fixture_api.insert((method, pattern));
        } else if pattern.starts_with("/ipc/v1/") {
            fixture_ipc.insert((method, pattern));
        } else {
            panic!("unexpected route namespace {method} {pattern}");
        }
    }

    let serve_api: BTreeSet<(String, String)> = serve_routes.iter().cloned().collect();
    assert_eq!(
        serve_api.len(),
        serve_routes.len(),
        "no duplicate serve ROUTES"
    );
    assert_eq!(
        serve_api, fixture_api,
        "routes.json /api/* routes must match serve.rs::ROUTES and docs/api.md"
    );

    let static_ipc = parse_static_ipc_routes(&ipc_routes_rs);
    assert_eq!(
        static_ipc, fixture_ipc,
        "ipc/routes.rs STATIC_IPC_ROUTES must match routes.json /ipc/v1/* routes"
    );

    // Out-of-table read-only endpoints `/api/features/truth` and `/api/episodes`:
    // both exist in serve.rs and ipc/routes.rs, reject non-GET/HEAD with 405
    // Allow: GET, and dispatch solely on read-only handlers/reads_gate.
    for path in ["/api/features/truth", "/api/episodes"] {
        assert!(
            !serve_api.contains(&("POST".to_string(), path.to_string())),
            "{path} must never become a POST route in ROUTES"
        );
    }

    let truth_serve_idx = serve_rs
        .find("if path == \"/api/features/truth\"")
        .expect("serve.rs handles /api/features/truth");
    let truth_serve_block = &serve_rs[truth_serve_idx..truth_serve_idx + 420];
    assert!(
        truth_serve_block.contains("Method::Get | Method::Head")
            && truth_serve_block.contains("not_allowed(\"GET\")")
            && truth_serve_block.contains("feature_map::feature_truth_answer"),
        "serve.rs /api/features/truth must remain strictly GET|HEAD read-only"
    );

    let episodes_serve_idx = serve_rs
        .find("if path == \"/api/episodes\"")
        .expect("serve.rs handles /api/episodes");
    let episodes_serve_block = &serve_rs[episodes_serve_idx..episodes_serve_idx + 420];
    assert!(
        episodes_serve_block.contains("Method::Get | Method::Head")
            && episodes_serve_block.contains("not_allowed(\"GET\")")
            && episodes_serve_block.contains("episodes_answer"),
        "serve.rs /api/episodes must remain strictly GET|HEAD read-only"
    );

    for (path, handler_call) in [
        (
            "/api/features/truth",
            "handle_api_get(\"/api/features/truth\"",
        ),
        ("/api/episodes", "handle_api_get(\"/api/episodes\""),
    ] {
        let needle = format!("if req.path == \"{path}\"");
        let idx = ipc_routes_rs
            .find(&needle)
            .unwrap_or_else(|| panic!("ipc/routes.rs must handle {path}"));
        let block = &ipc_routes_rs[idx..idx + 420];
        assert!(
            block.contains("\"GET\" | \"HEAD\"")
                && block.contains("405")
                && block.contains("\"Allow\", \"GET\"")
                && block.contains(handler_call),
            "ipc/routes.rs {path} must reject non-GET|HEAD and bind to handle_api_get"
        );
    }
    let handle_get_idx = ipc_routes_rs
        .find("fn handle_api_get(")
        .expect("ipc/routes.rs handle_api_get");
    let handle_get_block = &ipc_routes_rs[handle_get_idx..handle_get_idx + 900];
    assert!(
        handle_get_block.contains("state.pools.reads_gate.try_acquire()")
            && handle_get_block.contains("serve::with_reader_url(reader_url"),
        "ipc handle_api_get must enforce reads_gate pool and spawn_reader role binding"
    );
}

/// Issue #98 Item 2: lock SSE wire event names, framing, and HTTP response headers
/// in `serve/stream.rs` and `serve/ipc/events.rs`.
#[test]
fn sse_stream_wire_events_and_response_headers_stay_locked() {
    let stream_raw = read_repo_file("crates/spawn-cli/src/serve/stream.rs");
    let stream_rs = collapse_rust_line_continuations(&stream_raw);
    let ipc_events_raw = read_repo_file("crates/spawn-cli/src/serve/ipc/events.rs");
    let ipc_events_rs = collapse_rust_line_continuations(&ipc_events_raw);

    for required in [
        "HTTP/1.1 200 OK\\r\\nContent-Type: text/event-stream; charset=utf-8\\r\\n",
        "Cache-Control: no-cache, no-store\\r\\n",
        "Connection: close\\r\\n",
        "X-Accel-Buffering: no\\r\\n\\r\\nretry: 3000\\n\\n",
        "pub(super) const PROBE_FRAME: &str = \":\\n\\n\";",
        "\": keepalive\\n\\n\"",
        "format!(\"id: {id}\\n\")",
        "format!(\"{id}event: {}\\ndata: {}\\n\\n\"",
        "\"message\".to_string()",
        "\"ready\"",
        "\"resync\"",
        "\"stream_reset\"",
        "\"slow_consumer\"",
        "\"cursor_ahead\"",
        "\"behind\"",
        "\"latest_event_id\"",
        "\"after_event_id\"",
        "\"replayed\"",
    ] {
        assert!(
            stream_rs.contains(required),
            "serve/stream.rs SSE wire contract missing token: {required}"
        );
    }

    for required in [
        "HTTP/1.1 200 OK\\r\\nContent-Type: text/event-stream; charset=utf-8\\r\\n",
        "HTTP/1.1 200 OK\\r\\nContent-Type: application/x-ndjson; charset=utf-8\\r\\n",
        "Cache-Control: no-cache, no-store\\r\\nConnection: close\\r\\n\\r\\nretry: 3000\\n\\n",
        "\"type\": \"ready\"",
        "\"type\": \"event\"",
        "\"type\": \"hb\"",
        "\"type\": \"resync\"",
    ] {
        assert!(
            ipc_events_rs.contains(required),
            "serve/ipc/events.rs wire contract missing token: {required}"
        );
    }
}
