//! The gateway against the shared golden fixtures of `spawn-node-proto`
//! (`testdata/node-protocol-v1/`), with no database: every `reject/` request
//! is a signed `400 bad_request`; every row of `refusals.json` is the
//! gateway's own refusal byte for byte, and the rows the gateway reaches
//! without the ledger are reproduced end to end over HTTP at the row's
//! `server_time_ms`; plus artifacts and their ranges, the public prefix,
//! and malformed HTTP.

mod common;

use common::{Harness, boot_id, no_database, node_id, read, signer, stranger, v};
use serde_json::Value;
use spawn_node_gateway::Rid;
use spawn_node_proto::response::{HEADER_PRIMARY_KEY, HEADER_PRIMARY_SIG};
use spawn_node_proto::{Limit, Method, Refusal};

/// A request fixture: request line, `Name: value` lines, an empty line, the
/// body.
fn parse_fixture(bytes: &[u8]) -> (String, String, Vec<(String, String)>, Vec<u8>) {
    let split = bytes.windows(2).position(|w| w == b"\n\n").unwrap();
    let head = std::str::from_utf8(&bytes[..split]).unwrap();
    let body = bytes[split + 2..].to_vec();
    let mut lines = head.split('\n');
    let (method, target) = lines.next().unwrap().split_once(' ').unwrap();
    let headers = lines
        .map(|line| {
            let (name, value) = line.split_once(": ").unwrap();
            (name.to_string(), value.to_string())
        })
        .collect();
    (method.into(), target.into(), headers, body)
}

#[test]
fn every_reject_fixture_is_a_signed_bad_request_with_no_database_call() {
    let harness = Harness::start("reject", no_database());
    harness.set_clock(1_795_600_000_000);
    let mut names: Vec<String> = std::fs::read_dir(common::fixtures().join("reject"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names.len(), 9, "{names:?}");
    for name in names {
        let (method, target, headers, body) = parse_fixture(&read(&format!("reject/{name}")));
        let body = (!body.is_empty() || method == "POST").then_some(body.as_slice());
        let reply = harness.send(&method, &target, &headers, body);
        let seq = headers
            .iter()
            .find(|(n, _)| n == "Spawn-Node-Seq")
            .and_then(|(_, value)| spawn_node_proto::codec::parse_decimal(value).ok())
            .unwrap_or(0);
        assert_eq!(reply.status, 400, "{name}");
        assert_eq!(reply.json()["error"], "bad_request", "{name}");
        assert_eq!(reply.verify(&node_id(), &boot_id(), seq), 1_795_600_000_000, "{name}");
    }
    assert_eq!(harness.gateway.db_calls(), 0);
}

fn rows() -> Vec<Value> {
    serde_json::from_slice::<Vec<Value>>(&read("refusals.json")).unwrap()
}

fn row(error: &str, status: u16) -> Value {
    rows()
        .into_iter()
        .find(|row| row["error"] == error && row["status"] == status)
        .unwrap()
}

/// The reply is the row's refusal byte for byte: body and both signature
/// headers.
fn assert_row(reply: &common::Reply, row: &Value) {
    assert_eq!(u64::from(reply.status), row["status"].as_u64().unwrap(), "{row}");
    assert_eq!(
        String::from_utf8(reply.body.clone()).unwrap(),
        row["body"].as_str().unwrap(),
        "{row}"
    );
    assert_eq!(reply.header(HEADER_PRIMARY_KEY), row["primary_key"].as_str());
    assert_eq!(reply.header(HEADER_PRIMARY_SIG), row["primary_sig"].as_str(), "{row}");
}

#[test]
fn every_refusal_row_is_the_gateway_s_own_refusal_byte_for_byte() {
    let harness = Harness::start("refusal_rows", no_database());
    let rows = rows();
    assert_eq!(rows.len(), Refusal::ALL.len());
    for row in rows {
        let status = u16::try_from(row["status"].as_u64().unwrap()).unwrap();
        let refusal = Refusal::from_wire(status, row["error"].as_str().unwrap()).unwrap();
        let rid = Rid {
            node_id: row["node_id"].as_str().unwrap().to_string(),
            boot_id: row["boot_id"].as_str().unwrap().to_string(),
            seq: row["seq"].as_u64().unwrap(),
        };
        let response = harness
            .gateway
            .refusal(refusal, &rid, row["server_time_ms"].as_u64().unwrap());
        let spawn_node_gateway::http::Body::Bytes(body) = &response.body else {
            panic!("a refusal's body is bytes");
        };
        assert_eq!(body.as_slice(), row["body"].as_str().unwrap().as_bytes());
        assert_eq!(response.header_value(HEADER_PRIMARY_KEY), row["primary_key"].as_str());
        assert_eq!(response.header_value(HEADER_PRIMARY_SIG), row["primary_sig"].as_str(), "{row}");
    }
}

#[test]
fn bad_request_row_end_to_end() {
    let harness = Harness::start("row_400", no_database());
    let row = row("bad_request", 400);
    let time = row["server_time_ms"].as_u64().unwrap();
    harness.set_clock(time);
    // Signed, well-headed, but not JSON.
    let body = b"{\"v\":1,";
    let reply = harness.signed(&signer(), Method::Post, "/node/v1/heartbeat", 10, time, body, Some(body), &[]);
    assert_row(&reply, &row);
}

#[test]
fn bad_signature_row_on_a_cache_hit_with_no_database_call() {
    let harness = Harness::start("row_401", no_database());
    harness.remember_node();
    let row = row("bad_signature", 401);
    let time = row["server_time_ms"].as_u64().unwrap();
    harness.set_clock(time);
    let beat = v("beat_body");
    // Signed by another key under the node's id.
    let envelope = stranger()
        .sign(Method::Post, "/node/v1/heartbeat", 11, time, beat.as_bytes())
        .unwrap();
    let mut headers: Vec<(String, String)> = envelope
        .to_headers()
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect();
    headers[0].1 = node_id();
    let reply = harness.send("POST", "/node/v1/heartbeat", &headers, Some(beat.as_bytes()));
    assert_row(&reply, &row);
    assert_eq!(harness.gateway.db_calls(), 0, "a cache hit refuses with no database call");
}

#[test]
fn clock_skew_row_end_to_end() {
    let harness = Harness::start("row_skew", no_database());
    let row = row("clock_skew", 409);
    harness.set_clock(row["server_time_ms"].as_u64().unwrap());
    let beat = v("beat_body");
    let reply = harness.signed(
        &signer(),
        Method::Post,
        "/node/v1/heartbeat",
        14,
        1_795_600_000_000,
        beat.as_bytes(),
        Some(beat.as_bytes()),
        &[],
    );
    assert_row(&reply, &row);
}

#[test]
fn beat_first_row_for_an_artifact_without_a_cached_key() {
    let harness = Harness::start("row_beat_first", no_database());
    let row = row("beat_first", 409);
    let time = row["server_time_ms"].as_u64().unwrap();
    harness.set_clock(time);
    let reply = harness.signed(&signer(), Method::Get, &v("artifact_path"), 17, time, b"", None, &[]);
    // The signature binds (node, boot, seq), status, time and body, not the
    // path: the row's bytes exactly.
    assert_row(&reply, &row);
    assert_eq!(harness.gateway.db_calls(), 0);
}

#[test]
fn too_large_row_for_a_result_over_its_caps_before_any_body_byte() {
    let harness = Harness::start("row_413", no_database());
    let row = row("too_large", 413);
    let time = row["server_time_ms"].as_u64().unwrap();
    harness.set_clock(time);
    let target = "/node/v1/jobs/gate-0123456789ab-k3x9q2ma/result";
    let envelope = signer().sign(Method::Post, target, 19, time, b"{}").unwrap();
    let mut head = format!("POST {target} HTTP/1.1\r\n");
    for (name, value) in envelope.to_headers() {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    let over = Limit::ResultMeta.bytes() + 1 + Limit::ResultLog.bytes() + 1;
    head.push_str(&format!("Content-Length: {over}\r\n\r\n"));
    // Only the head is sent: the refusal comes before any body byte.
    let reply = harness.raw(head.as_bytes());
    assert_row(&reply, &row);
}

#[test]
fn busy_429_row_after_twenty_requests_in_ten_seconds() {
    let harness = Harness::start("row_429", no_database());
    let row = row("busy", 429);
    let time = row["server_time_ms"].as_u64().unwrap();
    harness.set_clock(time);
    let body = b"{\"v\":1,";
    for seq in 0..20 {
        let reply = harness.signed(&signer(), Method::Post, "/node/v1/heartbeat", seq + 100, time, body, Some(body), &[]);
        assert_eq!(reply.status, 400);
    }
    let reply = harness.signed(&signer(), Method::Post, "/node/v1/heartbeat", 20, time, body, Some(body), &[]);
    assert_row(&reply, &row);
}

#[test]
fn busy_503_row_when_the_ledger_is_unreachable() {
    let harness = Harness::start("row_503", no_database());
    harness.remember_node();
    let row = rows()
        .into_iter()
        .find(|row| row["status"] == 503)
        .unwrap();
    let time = row["server_time_ms"].as_u64().unwrap();
    harness.set_clock(time);
    let beat = v("beat_body");
    let reply = harness.signed(&signer(), Method::Post, "/node/v1/heartbeat", 21, time, beat.as_bytes(), Some(beat.as_bytes()), &[]);
    assert_row(&reply, &row);
    assert_eq!(harness.gateway.db_calls(), 1, "one connect attempt");
}

#[test]
fn the_artifact_fixture_is_served_whole_and_by_range_unsigned() {
    let harness = Harness::start("artifact", no_database());
    harness.remember_node();
    let (_, target, headers, _) = parse_fixture(&read("artifact.get.txt"));
    let hex = target.rsplit(':').next().unwrap();
    let dir = harness.artifacts.join("sha256").join(&hex[..2]);
    std::fs::create_dir_all(&dir).unwrap();
    let content: Vec<u8> = (0..100_000_u32).map(|n| (n % 251) as u8).collect();
    std::fs::write(dir.join(hex), &content).unwrap();
    harness.set_clock(1_795_600_005_000);

    let whole = harness.send("GET", &target, &headers, None);
    assert_eq!(whole.status, 200);
    assert_eq!(whole.body, content);
    assert_eq!(whole.header("Accept-Ranges"), Some("bytes"));
    assert_eq!(whole.header("Content-Length"), Some("100000"));
    assert!(whole.header(HEADER_PRIMARY_SIG).is_none(), "artifact bodies are not signed");

    let mut ranged = headers.clone();
    ranged.push(("Range".into(), "bytes=10-19".into()));
    let part = harness.send("GET", &target, &ranged, None);
    assert_eq!(part.status, 206);
    assert_eq!(part.body, &content[10..20]);
    assert_eq!(part.header("Content-Range"), Some("bytes 10-19/100000"));

    let mut tail = headers.clone();
    tail.push(("Range".into(), "bytes=-5".into()));
    assert_eq!(harness.send("GET", &target, &tail, None).body, &content[99_995..]);

    let mut past = headers.clone();
    past.push(("Range".into(), "bytes=100000-".into()));
    let unsatisfiable = harness.send("GET", &target, &past, None);
    assert_eq!(unsatisfiable.status, 416);
    assert_eq!(unsatisfiable.header("Content-Range"), Some("bytes */100000"));
    assert_eq!(harness.gateway.db_calls(), 0);
}

#[test]
fn artifact_misses_and_the_registry_manifest() {
    let harness = Harness::start("artifact_miss", no_database());
    harness.remember_node();
    let signer = signer();
    let missing = format!("/node/v1/artifacts/sha256:{}", "a".repeat(64));
    harness.get(&signer, &missing, 1, &[]).assert_refused_status(404, "not_found", 1);
    let blake = format!("/node/v1/artifacts/blake3:{}", "a".repeat(64));
    harness.get(&signer, &blake, 2, &[]).assert_refused(Refusal::BadRequest, 2);
    harness
        .get(&signer, "/node/v1/registry-manifest", 3, &[])
        .assert_refused_status(404, "not_found", 3);
    std::fs::write(harness.artifacts.join("registry-manifest.json"), b"{\"crates\":[]}").unwrap();
    let manifest = harness.get(&signer, "/node/v1/registry-manifest", 4, &[]);
    assert_eq!((manifest.status, manifest.body.as_slice()), (200, &b"{\"crates\":[]}"[..]));
    // A boot the cache does not hold is not served.
    let other_boot = spawn_node_proto::NodeSigner::new(common::node_key(), &"f".repeat(32)).unwrap();
    let reply = harness.get(&other_boot, "/node/v1/registry-manifest", 5, &[]);
    assert_eq!(reply.status, 409);
    reply.verify(&node_id(), &"f".repeat(32), 5);
    // A forged GET under the node's id, on a cache hit.
    let envelope = stranger()
        .sign(Method::Get, "/node/v1/registry-manifest", 6, common::now_ms(), b"")
        .unwrap();
    let mut headers: Vec<(String, String)> = envelope
        .to_headers()
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect();
    headers[0].1 = node_id();
    harness
        .send("GET", "/node/v1/registry-manifest", &headers, None)
        .assert_refused(Refusal::BadSignature, 6);
    assert_eq!(harness.gateway.db_calls(), 0);
}

#[test]
fn the_public_prefix_is_stripped_and_required() {
    let harness = Harness::start_with_prefix("prefix", no_database(), "/gw");
    harness.remember_node();
    std::fs::write(harness.artifacts.join("registry-manifest.json"), b"m").unwrap();
    let signer = signer();
    // The signature covers the target without the prefix.
    let envelope = signer
        .sign(Method::Get, "/node/v1/registry-manifest", 1, common::now_ms(), b"")
        .unwrap();
    let headers: Vec<(String, String)> = envelope
        .to_headers()
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect();
    let reply = harness.send("GET", "/gw/node/v1/registry-manifest", &headers, None);
    assert_eq!((reply.status, reply.body.as_slice()), (200, &b"m"[..]));
    let bare = harness.send("GET", "/node/v1/registry-manifest", &headers, None);
    bare.assert_refused(Refusal::BadRequest, 1);
}

#[test]
fn malformed_http_and_content_length_are_signed_bad_requests() {
    let harness = Harness::start("malformed", no_database());
    let reply = harness.raw(b"NOT A REQUEST\r\n\r\n");
    assert_eq!(reply.status, 400);
    reply.verify("", "", 0);

    let beat = v("beat_body");
    let envelope = signer()
        .sign(Method::Post, "/node/v1/heartbeat", 1, common::now_ms(), beat.as_bytes())
        .unwrap();
    let mut headers: Vec<(String, String)> = envelope
        .to_headers()
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect();
    for length in ["+324", "0324", "324, 324", "huge"] {
        let mut with = headers.clone();
        with.push(("Content-Length".into(), length.into()));
        harness
            .send("POST", "/node/v1/heartbeat", &with, None)
            .assert_refused(Refusal::BadRequest, 1);
    }
    // Twice the same Spawn-Node-* header.
    headers.push(headers[2].clone());
    harness
        .send("POST", "/node/v1/heartbeat", &headers, Some(beat.as_bytes()))
        .assert_refused(Refusal::BadRequest, 1);
    // A POST with no Content-Length.
    headers.pop();
    harness
        .send("POST", "/node/v1/heartbeat", &headers, None)
        .assert_refused(Refusal::BadRequest, 1);
    assert_eq!(harness.gateway.db_calls(), 0);
}

trait RefusedStatus {
    fn assert_refused_status(&self, status: u16, error: &str, seq: u64);
}

impl RefusedStatus for common::Reply {
    fn assert_refused_status(&self, status: u16, error: &str, seq: u64) {
        assert_eq!((self.status, self.json()["error"].as_str().unwrap()), (status, error));
        self.verify(&node_id(), &boot_id(), seq);
    }
}
