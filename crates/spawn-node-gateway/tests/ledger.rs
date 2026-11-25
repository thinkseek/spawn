//! The gateway over a real ledger (PostgreSQL, `SPAWN_TEST_DATABASE_URL`, a
//! schema per test), logged in as the schema's restricted
//! `spawn_node_gateway` login (`spawn_postgres::role_url`), with the
//! fixtures' node and primary keys: enroll with the fixture token, beats,
//! a signed grant, a streamed result and its `lease_fenced` twin (whose
//! files are removed), spec, replay, verify-then-commit's rollback, a
//! cache hit's zero database calls, range GETs, revoke and an unknown node.

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::{
    Harness, Reply, Req, boot_id, node_id, now_ms, primary_key, read, signer, stranger, v,
};
use serde_json::json;
use spawn_node_gateway::{ResultAnswer, SpecAnswer};
use spawn_node_proto::enroll::{EnrollAnswer, EnrollToken};
use spawn_node_proto::grant::JobKind;
use spawn_node_proto::heartbeat::Ack;
use spawn_node_proto::result::ResultMeta;
use spawn_node_proto::{Method, NodeSigner, Refusal, SigningKey};
use spawn_postgres::testing::TestSchema;
use spawn_postgres::{NodeAction, PostgresStore, RemoteJobKind, RemoteJobSpec, RemoteJobState};

const JOB: &str = "gate-0123456789ab-k3x9q2ma";

/// The test URL with `schema` as its search path.
fn schema_url(schema: &TestSchema) -> String {
    let url = schema.url();
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}options=-csearch_path%3D{}", schema.name())
}

/// The owner's store in a fresh schema and a gateway logged in as the
/// schema's `spawn_node_gateway` login.
fn setup(case: &str) -> (TestSchema, PostgresStore, Harness) {
    let schema = TestSchema::new(case);
    let owner = schema.store();
    let gateway_url = spawn_postgres::role_url(&schema_url(&schema), "spawn_node_gateway")
        .expect("the login's URL");
    let harness = Harness::start(
        case,
        Arc::new(move || PostgresStore::connect(&gateway_url).map_err(|error| error.to_string())),
    );
    (schema, owner, harness)
}

fn token() -> EnrollToken {
    EnrollToken::from_file_contents(&String::from_utf8(read("enroll.token.txt")).unwrap()).unwrap()
}

fn enroll_body(spec: &[u8]) -> Vec<u8> {
    let mut body = format!(
        "{{\"v\":1,\"token\":\"{}\",\"pubkey\":\"{}\",\"spec\":",
        token().as_str(),
        v("node_pubkey_b64u")
    )
    .into_bytes();
    body.extend_from_slice(spec);
    body.push(b'}');
    body
}

/// Enroll the fixture node as `n252`.
fn enroll(owner: &PostgresStore, harness: &Harness) -> Reply {
    let token_sha: [u8; 32] = spawn_node_proto::codec::sha256(token().as_str().as_bytes());
    owner
        .node_enroll_token("n252", None, &token_sha, 600_000)
        .expect("a token");
    harness.post(
        &signer(),
        "/node/v1/enroll",
        0,
        &enroll_body(&read("spec.json")),
    )
}

fn beat(harness: &Harness, signer: &NodeSigner, seq: u64) -> Reply {
    harness.post(signer, "/node/v1/heartbeat", seq, v("beat_body").as_bytes())
}

fn job(id: &str) -> RemoteJobSpec {
    RemoteJobSpec {
        job_id: id.to_string(),
        kind: RemoteJobKind::Gate,
        cell_id: None,
        priority: None,
        judging_id: None,
        mission_id: None,
        task_id: None,
        spec: json!({"timeout_s": 600}),
        requires: BTreeMap::from([("pg".to_string(), "16".to_string())]),
        cost: BTreeMap::from([("gate_sandbox".to_string(), 1)]),
        base_commit: None,
        max_attempts: None,
        queue_wait_ms: None,
    }
}

fn result_body(job_id: &str, epoch: u64, log: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let meta = ResultMeta {
        v: 1,
        job_id: job_id.to_string(),
        lease_epoch: epoch,
        exit_code: Some(0),
        signal: None,
        timed_out: false,
        self_fenced: false,
        seconds: 12,
        verdict_raw: None,
        stdout_bytes: 0,
        log_bytes: log.len() as u64,
        log_sha256: spawn_node_proto::codec::sha256_hex(log),
        truncated: None,
        inputs: None,
        env: None,
        wrapper_error: None,
    };
    let line = meta.to_bytes();
    let body = spawn_node_proto::result::build_body(&line, log);
    (line, body)
}

fn post_result(harness: &Harness, seq: u64, job_id: &str, line: &[u8], body: &[u8]) -> Reply {
    harness.signed(
        &signer(),
        &Req {
            method: Method::Post,
            target: &format!("/node/v1/jobs/{job_id}/result"),
            seq,
            ts_ms: now_ms(),
            signed_body: line,
            body: Some(body),
        },
        &[],
    )
}

#[test]
fn enroll_beat_grant_result_and_their_refusals() {
    let (_schema, owner, harness) = setup("gw_flow");
    enroll_and_first_beats(&owner, &harness);
    granted(&owner, &harness);
    results(&owner, &harness);
    let digest = spec_and_artifacts(&harness);
    replay_rollback_and_revoke(&owner, &harness, &digest);
}

/// Enroll (proof of possession, the token consumed, the primary pinned),
/// a first beat, and a cache hit's forged beat.
fn enroll_and_first_beats(owner: &PostgresStore, harness: &Harness) {
    let node = signer();
    // Enroll: proof of possession, the token consumed, the primary pinned.
    let enrolled = enroll(owner, harness);
    assert_eq!(enrolled.status, 200, "{}", enrolled.json());
    enrolled.verify(&node_id(), &boot_id(), 0);
    let answer: EnrollAnswer = serde_json::from_slice(&enrolled.body).unwrap();
    assert_eq!(
        token().accept_primary(&answer.primary_pubkey).unwrap(),
        primary_key().public_key()
    );
    let row = owner.node_v1("n252").unwrap().unwrap();
    assert_eq!(row.node_id.as_deref(), Some(node_id().as_str()));
    assert_eq!(
        row.spec_sha256.as_deref(),
        Some(v("spec_sha256").as_str()),
        "the spec's bytes as received are hashed"
    );
    // The token is good once.
    let again = harness.post(
        &node,
        "/node/v1/enroll",
        0,
        &enroll_body(&read("spec.json")),
    );
    again.assert_refused(Refusal::UnknownNode, 0);

    // A first beat: nothing queued, the spec held.
    let first = beat(harness, &node, 1);
    assert_eq!(first.status, 200, "{}", first.json());
    first.verify(&node_id(), &boot_id(), 1);
    let ack = Ack::parse(&first.body).unwrap();
    assert!(!ack.need_spec && ack.grants.is_empty());

    // A cache hit refuses a bad signature with no database call.
    let calls = harness.gateway.db_calls();
    let forged = forged_beat(harness, 2);
    forged.assert_refused(Refusal::BadSignature, 2);
    assert_eq!(harness.gateway.db_calls(), calls, "zero database calls");
}

/// A queued job, granted with its exact bytes signed.
fn granted(owner: &PostgresStore, harness: &Harness) {
    let node = signer();
    // A job: granted, its exact bytes signed.
    owner.remote_job_enqueue(&job(JOB)).unwrap();
    let granted = beat(harness, &node, 3);
    assert_eq!(granted.status, 200);
    granted.verify(&node_id(), &boot_id(), 3);
    let ack = Ack::parse(&granted.body).unwrap();
    assert_eq!(ack.grants.len(), 1);
    assert_eq!(ack.next_beat_ms, 5_000);
    let (_, grant) = ack.grants[0]
        .open(&primary_key().public_key(), &node_id(), now_ms())
        .expect("a grant signed by the primary, for this node, not expired");
    assert_eq!(
        (grant.job_id.as_str(), grant.lease_epoch, grant.kind),
        (JOB, 1, JobKind::Gate)
    );
    assert_eq!(grant.timeout_s, Some(600));
    assert_eq!(
        grant
            .cost
            .as_ref()
            .and_then(|cost| cost.get("gate_sandbox")),
        Some(&1)
    );
    assert!(grant.expires_ms > now_ms() + 60_000);
}

/// The result streamed, stored and completed; again; fenced; mismatched.
fn results(owner: &PostgresStore, harness: &Harness) {
    // The result: 3 MiB streamed to disk, checked, then completed.
    let log: Vec<u8> = (0..3 * 1024 * 1024_u32)
        .map(|n| (n.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    let (line, body) = result_body(JOB, 1, &log);
    let done = post_result(harness, 4, JOB, &line, &body);
    assert_eq!(done.status, 200, "{}", done.json());
    done.verify(&node_id(), &boot_id(), 4);
    let answer: ResultAnswer = serde_json::from_slice(&done.body).unwrap();
    assert_eq!((answer.job_id.as_str(), answer.lease_epoch), (JOB, 1));
    let dir = harness.results.join(format!("{JOB}.1"));
    assert_eq!(std::fs::read(dir.join("log")).unwrap(), log);
    assert_eq!(std::fs::read(dir.join("meta.json")).unwrap(), line);
    assert!(!dir.join("log.part").exists());
    let stored = owner.remote_job(JOB).unwrap().unwrap();
    assert_eq!(stored.state, RemoteJobState::Completed);
    assert_eq!(stored.log_bytes, Some(i64::try_from(log.len()).unwrap()));
    assert_eq!(
        stored.result_sha256.as_deref(),
        Some(spawn_node_proto::codec::sha256_hex(&line).as_str())
    );
    // The same result again: accepted, nothing rewritten.
    let twice = post_result(harness, 5, JOB, &line, &body);
    assert_eq!(twice.status, 200);

    // A result at an epoch the job never had: the files go again.
    let (line2, body2) = result_body(JOB, 2, b"other log");
    let fenced = post_result(harness, 6, JOB, &line2, &body2);
    fenced.assert_refused(Refusal::LeaseFenced, 6);
    assert!(
        !harness.results.join(format!("{JOB}.2")).exists(),
        "partial files removed"
    );
    // A log that does not match its metadata.
    let (line3, mut body3) = result_body(JOB, 1, b"abc");
    *body3.last_mut().unwrap() = b'x';
    post_result(harness, 7, JOB, &line3, &body3).assert_refused(Refusal::BadRequest, 7);
    assert_eq!(
        std::fs::read(dir.join("log")).unwrap(),
        log,
        "the stored result is untouched"
    );
}

/// Spec, and ranged artifact GETs on the cached key; the artifact's
/// address.
fn spec_and_artifacts(harness: &Harness) -> spawn_node_proto::ContentDigest {
    let node = signer();
    // Spec: the same bytes change nothing.
    let spec = harness.post(&node, "/node/v1/spec", 8, &read("spec.json"));
    assert_eq!(spec.status, 200);
    spec.verify(&node_id(), &boot_id(), 8);
    let spec: SpecAnswer = serde_json::from_slice(&spec.body).unwrap();
    assert!(!spec.changed);

    // Range GET of an artifact on the cached key.
    let content = b"0123456789abcdef".repeat(1000);
    let digest = spawn_node_proto::ContentDigest::sha256_of(&content);
    let path = harness.artifacts.join("sha256").join(&digest.hex[..2]);
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join(&digest.hex), &content).unwrap();
    let calls = harness.gateway.db_calls();
    let part = harness.get(
        &node,
        &digest.artifact_path(),
        9,
        &[("Range".into(), "bytes=15990-".into())],
    );
    assert_eq!(part.status, 206);
    assert_eq!(part.body, &content[15_990..]);
    assert_eq!(
        part.header("Content-Range"),
        Some("bytes 15990-15999/16000")
    );
    let whole = harness.get(&node, &digest.artifact_path(), 10, &[]);
    digest.verify(&whole.body).unwrap();
    assert_eq!(
        harness.gateway.db_calls(),
        calls,
        "artifacts never reach the ledger"
    );
    digest
}

/// Replay, verify-then-commit's rollback, revoke.
fn replay_rollback_and_revoke(
    owner: &PostgresStore,
    harness: &Harness,
    digest: &spawn_node_proto::ContentDigest,
) {
    let node = signer();
    // Replay: a beat seq at or below the stored one.
    beat(harness, &node, 3).assert_refused(Refusal::Replay, 3);

    // Verify-then-commit: on a cache miss the forged beat reaches the
    // ledger, whose work is rolled back.
    harness.gateway.keys().evict(&node_id());
    let before = owner.node_v1("n252").unwrap().unwrap();
    let calls = harness.gateway.db_calls();
    forged_beat(harness, 50).assert_refused(Refusal::BadSignature, 50);
    assert!(harness.gateway.db_calls() > calls, "a miss asks the ledger");
    let after = owner.node_v1("n252").unwrap().unwrap();
    assert_eq!(after.beat_seq, before.beat_seq, "rolled back");
    assert_eq!(after.last_beat_ms, before.last_beat_ms, "rolled back");
    assert!(
        harness.gateway.keys().get(&node_id()).is_none(),
        "a forgery is not cached"
    );

    // Revoked: refused, and the cache forgets the key.
    assert_eq!(beat(harness, &node, 51).status, 200, "a good beat");
    assert!(harness.gateway.keys().get(&node_id()).is_some());
    assert!(owner.node_admin("n252", NodeAction::Revoke).unwrap());
    beat(harness, &node, 52).assert_refused(Refusal::Revoked, 52);
    assert!(harness.gateway.keys().get(&node_id()).is_none(), "evicted");
    harness
        .get(&node, &digest.artifact_path(), 53, &[])
        .assert_refused(Refusal::BeatFirst, 53);
}

/// A beat under the node's id signed by another key.
fn forged_beat(harness: &Harness, seq: u64) -> Reply {
    let beat = v("beat_body");
    let envelope = stranger()
        .sign(
            Method::Post,
            "/node/v1/heartbeat",
            seq,
            now_ms(),
            beat.as_bytes(),
        )
        .unwrap();
    let mut headers: Vec<(String, String)> = envelope
        .to_headers()
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect();
    headers[0].1 = node_id();
    harness.send(
        "POST",
        "/node/v1/heartbeat",
        &headers,
        Some(beat.as_bytes()),
    )
}

#[test]
fn unknown_nodes_bad_tokens_and_a_result_before_any_beat() {
    let (_schema, _owner, harness) = setup("gw_unknown");
    let unknown = NodeSigner::new(SigningKey::from_seed(&[5; 32]).unwrap(), &boot_id()).unwrap();
    let reply = beat(&harness, &unknown, 1);
    assert_eq!(
        (reply.status, reply.json()["error"].as_str().unwrap()),
        (401, "unknown_node")
    );
    reply.verify(&unknown.node_id, &boot_id(), 1);

    // A token the ledger never stored.
    harness
        .post(
            &signer(),
            "/node/v1/enroll",
            0,
            &enroll_body(&read("spec.json")),
        )
        .assert_refused(Refusal::UnknownNode, 0);

    // Proof of possession: the body's key must sign the request.
    let body = enroll_body(&read("spec.json"));
    let envelope = stranger()
        .sign(Method::Post, "/node/v1/enroll", 0, now_ms(), &body)
        .unwrap();
    let mut headers: Vec<(String, String)> = envelope
        .to_headers()
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect();
    headers[0].1 = node_id();
    let calls = harness.gateway.db_calls();
    harness
        .send("POST", "/node/v1/enroll", &headers, Some(&body))
        .assert_refused(Refusal::BadSignature, 0);
    assert_eq!(harness.gateway.db_calls(), calls);

    // A result from a node whose key the gateway does not hold.
    let (line, body) = result_body(JOB, 1, b"log");
    post_result(&harness, 2, JOB, &line, &body).assert_refused(Refusal::BeatFirst, 2);
    assert_eq!(harness.gateway.db_calls(), calls);
}
