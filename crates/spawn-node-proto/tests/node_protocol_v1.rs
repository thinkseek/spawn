//! Probe P6 on the Rust side (design §7, §14): every value of the golden
//! fixtures in `testdata/node-protocol-v1/` reproduced byte for byte with
//! ring, all six signatures of `vectors.json` and every refusal of
//! `refusals.json` verified, every target and header of `reject/` refused
//! as `400 bad_request`, `tree_sha256_v1` of `tree/`, and the directory's
//! `MANIFEST.sha256`.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde_json::Value;
use spawn_node_proto::codec::{EMPTY_SHA256, hex, sha256_hex, unhex};
use spawn_node_proto::enroll::EnrollToken;
use spawn_node_proto::envelope::{NodeSigner, request_signing_input};
use spawn_node_proto::grant::{Grant, SignedGrant, grant_signing_input};
use spawn_node_proto::heartbeat::{Ack, Beat, NEXT_BEAT_IDLE_MS};
use spawn_node_proto::response::{RefusalBody, RequestId, sign_response, verify_response};
use spawn_node_proto::result::{ResultMeta, build_body};
use spawn_node_proto::spec::{NodeSpec, spec_sha256};
use spawn_node_proto::{
    ContentDigest, Error, Limit, Method, PublicKey, Reaction, Refusal, SigningKey, Verb,
    check_request, scenarios_sha256, tree_sha256_v1,
};

const BEAT_PATH: &str = "/node/v1/heartbeat";
const SPEC_PATH: &str = "/node/v1/spec";
const RESULT_PATH: &str = "/node/v1/jobs/gate-0123456789ab-k3x9q2ma/result";
/// The 18-byte log of section 7 (`result.log`; kept here too because the
/// repository's `.gitignore` names `*.log`).
const RESULT_LOG: &[u8] = b"| gate ok\n\n! warn\n";

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/node-protocol-v1")
}

fn read(name: &str) -> Vec<u8> {
    std::fs::read(dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn json_file(name: &str) -> Value {
    serde_json::from_slice(&read(name)).unwrap()
}

fn vectors() -> Value {
    json_file("vectors.json")
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} is not a string"))
}

fn seed(v: &Value, key: &str) -> [u8; 32] {
    unhex(s(v, key), 64).unwrap().try_into().unwrap()
}

fn node() -> SigningKey {
    SigningKey::from_seed(&seed(&vectors(), "node_seed_hex")).unwrap()
}

fn primary() -> SigningKey {
    SigningKey::from_seed(&seed(&vectors(), "primary_seed_hex")).unwrap()
}

fn signer() -> NodeSigner {
    NodeSigner::new(node(), s(&vectors(), "boot_id")).unwrap()
}

/// A request fixture: the request line, `Name: value` headers, an empty
/// line, the body (no newline added).
struct RawRequest {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl RawRequest {
    fn parse(bytes: &[u8]) -> Self {
        let split = bytes
            .windows(2)
            .position(|w| w == b"\n\n")
            .expect("an empty line");
        let head = std::str::from_utf8(&bytes[..split]).unwrap();
        let body = bytes[split + 2..].to_vec();
        let mut lines = head.split('\n');
        let (method, target) = lines.next().unwrap().split_once(' ').unwrap();
        let headers: Vec<(String, String)> = lines
            .map(|line| {
                let (name, value) = line.split_once(": ").unwrap();
                (name.to_string(), value.to_string())
            })
            .collect();
        Self {
            method: method.into(),
            target: target.into(),
            headers,
            body,
        }
    }

    fn header(&self, name: &str) -> Option<&str> {
        let mut found = self
            .headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name));
        let first = found.next().map(|(_, value)| value.as_str());
        assert!(found.next().is_none(), "{name} twice");
        first
    }

    fn check(&self) -> Result<spawn_node_proto::Checked<'_>, Error> {
        check_request(
            &self.method,
            &self.target,
            |name| self.header(name),
            &self.body,
        )
    }
}

/// The keys of `vectors.json` each test reproduces; together they must be
/// every key the file has.
const VECTOR_KEYS: [&str; 30] = [
    "node_seed_hex",
    "node_pubkey_b64u",
    "node_id",
    "primary_seed_hex",
    "primary_pubkey_b64u",
    "primary_key_id",
    "boot_id",
    "spec_body",
    "spec_sha256",
    "spec_sig",
    "beat_body",
    "beat_bytes",
    "beat_sha256",
    "beat_signing_input_sha256",
    "beat_sig",
    "ack_body",
    "ack_sha256",
    "ack_sig",
    "artifact_path",
    "empty_sha256",
    "artifact_get_sig",
    "grant_bytes",
    "grant_sha256",
    "grant_sig",
    "result_meta",
    "result_meta_sha256",
    "result_sig",
    "tree_sha256_v1",
    "enroll_token",
    "enroll_token_sha256",
];

#[test]
fn p6_every_key_of_vectors_json_is_reproduced_by_a_test() {
    let v = vectors();
    let have: BTreeSet<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    let covered: BTreeSet<&str> = VECTOR_KEYS.into_iter().collect();
    assert_eq!(have, covered);
}

#[test]
fn p6_keys_derive_from_the_seeds() {
    let v = vectors();
    let node = node().public_key();
    let primary = primary().public_key();
    assert_eq!(node.to_b64u(), s(&v, "node_pubkey_b64u"));
    assert_eq!(node.key_id(), s(&v, "node_id"));
    assert_eq!(primary.to_b64u(), s(&v, "primary_pubkey_b64u"));
    assert_eq!(primary.key_id(), s(&v, "primary_key_id"));
    assert_eq!(
        PublicKey::from_b64u(s(&v, "node_pubkey_b64u")).unwrap(),
        node
    );
    // keys.json carries the same six values.
    let keys = json_file("keys.json");
    for key in keys.as_object().unwrap().keys() {
        assert_eq!(keys[key], v[key], "{key}");
    }
    assert_eq!(keys.as_object().unwrap().len(), 6);
    // The seeds are the published test seeds, bytes 00..1f and 20..3f.
    assert_eq!(
        seed(&v, "node_seed_hex"),
        std::array::from_fn::<u8, 32, _>(|i| i as u8)
    );
    assert_eq!(
        seed(&v, "primary_seed_hex"),
        std::array::from_fn::<u8, 32, _>(|i| 0x20 + i as u8)
    );
}

#[test]
fn p6_boot_id_file_and_counter() {
    let v = vectors();
    assert_eq!(
        read("boot_id.txt"),
        format!("{}\n", s(&v, "boot_id")).into_bytes()
    );
    let boot = s(&v, "boot_id");
    assert_eq!(u64::from_str_radix(&boot[..16], 16).unwrap(), 1);
    assert!(boot < "00000000000000020000000000000000");
}

#[test]
fn p6_spec_bytes_digest_signature_and_model() {
    let v = vectors();
    let body = read("spec.json");
    assert_eq!(body, s(&v, "spec_body").as_bytes());
    assert_eq!(spec_sha256(&body), s(&v, "spec_sha256"));
    let envelope = signer()
        .sign(Method::Post, SPEC_PATH, 2, 1_795_600_000_500, &body)
        .unwrap();
    let sig = envelope.to_headers()[4].1.clone();
    assert_eq!(sig, s(&v, "spec_sig"));
    envelope
        .verify(&node().public_key(), Method::Post, SPEC_PATH, &body)
        .unwrap();

    let spec = NodeSpec::parse(&body).unwrap();
    assert_eq!(spec.to_bytes(), body);
    assert_eq!(spec.name, "n252");
    assert_eq!(
        spec.capacities_total.keys().collect::<Vec<_>>(),
        ["gate_sandbox", "batch_cpu"]
    );
    assert_eq!(spec.holds.pg.as_deref(), Some("16"));
}

#[test]
fn p6_heartbeat_bytes_signing_input_signature_and_model() {
    let v = vectors();
    let body = read("heartbeat.req.json");
    assert_eq!(body, s(&v, "beat_body").as_bytes());
    assert_eq!(body.len() as u64, v["beat_bytes"].as_u64().unwrap());
    assert_eq!(body.len(), 324);
    assert_eq!(sha256_hex(&body), s(&v, "beat_sha256"));
    let signer = signer();
    let input = request_signing_input(
        Method::Post,
        BEAT_PATH,
        &signer.node_id,
        &signer.boot_id,
        1,
        1_795_600_000_000,
        &body,
    );
    assert_eq!(sha256_hex(&input), s(&v, "beat_signing_input_sha256"));
    assert_eq!(
        input,
        format!(
            "spawn-node-req-v1\nPOST\n{BEAT_PATH}\n{}\n{}\n1\n1795600000000\n{}\n",
            s(&v, "node_id"),
            s(&v, "boot_id"),
            s(&v, "beat_sha256")
        )
        .into_bytes()
    );
    assert_eq!(node().sign_b64u(&input), s(&v, "beat_sig"));
    node()
        .public_key()
        .verify_b64u(&input, s(&v, "beat_sig"))
        .unwrap();

    let beat = Beat::parse(&body).unwrap();
    assert_eq!(beat.to_bytes().unwrap(), body);
    assert_eq!(beat.spec_sha256, s(&v, "spec_sha256"));

    // The whole request through the gateway's checks, then its signature.
    let envelope = signer
        .sign(Method::Post, BEAT_PATH, 1, 1_795_600_000_000, &body)
        .unwrap();
    let headers = envelope.to_headers();
    let header = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, value)| value.as_str())
    };
    let checked = check_request("POST", BEAT_PATH, header, &body).unwrap();
    assert_eq!(checked.verb, Verb::Heartbeat);
    checked.verify(&node().public_key()).unwrap();
    // Another key, or one changed byte: 401 bad_signature.
    let err = checked.verify(&primary().public_key()).unwrap_err();
    assert_eq!(err.refusal(), Refusal::BadSignature);
    let mut flipped = body.clone();
    flipped[10] ^= 1;
    let checked = check_request("POST", BEAT_PATH, header, &flipped).unwrap();
    assert_eq!(
        checked.verify(&node().public_key()).unwrap_err().refusal(),
        Refusal::BadSignature
    );
}

#[test]
fn p6_heartbeat_answer_bytes_and_response_signature() {
    let v = vectors();
    let body = read("heartbeat.resp.json");
    assert_eq!(body, s(&v, "ack_body").as_bytes());
    assert_eq!(sha256_hex(&body), s(&v, "ack_sha256"));
    let request = RequestId {
        node_id: s(&v, "node_id"),
        boot_id: s(&v, "boot_id"),
        seq: 1,
    };
    let signature = sign_response(&primary(), 200, request, 1_795_600_000_120, &body);
    assert_eq!(signature.sig, s(&v, "ack_sig"));
    assert_eq!(signature.primary_key, s(&v, "primary_key_id"));
    let primary = primary().public_key();
    assert_eq!(
        verify_response(
            &primary,
            200,
            request,
            &signature.primary_key,
            &signature.sig,
            &body
        ),
        Ok(1_795_600_000_120)
    );
    // Bound to the status and to the request: another seq, another boot or
    // another status is not this answer.
    let replayed = RequestId { seq: 2, ..request };
    assert_eq!(
        verify_response(
            &primary,
            200,
            replayed,
            &signature.primary_key,
            &signature.sig,
            &body
        ),
        Err(Error::Signature)
    );
    assert_eq!(
        verify_response(
            &primary,
            409,
            request,
            &signature.primary_key,
            &signature.sig,
            &body
        ),
        Err(Error::Signature)
    );
    // Signed by anyone but the pinned primary: a transport failure.
    let node = node().public_key();
    assert_eq!(
        verify_response(
            &node,
            200,
            request,
            &signature.primary_key,
            &signature.sig,
            &body
        ),
        Err(Error::Signature)
    );

    let ack = Ack::parse(&body).unwrap();
    assert_eq!(ack.to_bytes(), body);
    assert_eq!(ack.next_beat_ms, NEXT_BEAT_IDLE_MS);
    assert!(ack.need_sync.is_empty() && ack.grants.is_empty() && !ack.drain);
}

#[test]
fn p6_artifact_get_request_and_signature() {
    let v = vectors();
    let raw = RawRequest::parse(&read("artifact.get.txt"));
    assert_eq!(raw.method, "GET");
    assert_eq!(raw.target, s(&v, "artifact_path"));
    assert!(raw.body.is_empty());
    assert_eq!(sha256_hex(&raw.body), s(&v, "empty_sha256"));
    assert_eq!(EMPTY_SHA256, s(&v, "empty_sha256"));
    let digest = ContentDigest::parse(s(&v, "artifact_path").rsplit('/').next().unwrap()).unwrap();
    assert_eq!(digest.artifact_path(), s(&v, "artifact_path"));

    let envelope = signer()
        .sign(
            Method::Get,
            s(&v, "artifact_path"),
            3,
            1_795_600_005_000,
            b"",
        )
        .unwrap();
    let sent: Vec<(String, String)> = envelope
        .to_headers()
        .into_iter()
        .map(|(name, value)| (name.to_string(), value))
        .collect();
    assert_eq!(sent, raw.headers, "the fixture's headers, in order");
    assert_eq!(
        raw.header("Spawn-Node-Sig"),
        Some(s(&v, "artifact_get_sig"))
    );

    let checked = raw.check().unwrap();
    assert_eq!(checked.verb, Verb::Artifact(digest));
    checked.verify(&node().public_key()).unwrap();
}

#[test]
fn p6_grant_bytes_signature_and_model() {
    let v = vectors();
    let bytes = read("grant.json");
    assert_eq!(bytes, s(&v, "grant_bytes").as_bytes());
    assert_eq!(sha256_hex(&bytes), s(&v, "grant_sha256"));
    assert_eq!(
        grant_signing_input(&bytes),
        format!("spawn-node-grant-v1\n{}\n", s(&v, "grant_sha256")).into_bytes()
    );
    let signed = SignedGrant::sign(&primary(), &bytes);
    assert_eq!(signed.sig, s(&v, "grant_sig"));

    let primary = primary().public_key();
    let (opened, grant) = signed
        .open(&primary, s(&v, "node_id"), 1_795_600_000_120)
        .unwrap();
    assert_eq!(opened, bytes);
    assert_eq!(grant.to_bytes(), bytes);
    assert_eq!(grant.job_id, "gate-0123456789ab-k3x9q2ma");
    assert_eq!(grant.expires_ms, 1_795_600_090_120);
    let as_stored = SignedGrant {
        grant: signed.grant.clone(),
        sig: s(&v, "grant_sig").into(),
    };
    as_stored
        .open(&primary, s(&v, "node_id"), 1_795_600_090_119)
        .unwrap();
    assert_eq!(
        as_stored
            .open(&primary, s(&v, "node_id"), 1_795_600_090_120)
            .unwrap_err(),
        Error::Grant("expired")
    );
    assert_eq!(
        as_stored.open(&primary, &"0".repeat(64), 0).unwrap_err(),
        Error::Grant("node_id is not this node's")
    );
    let _: Grant = serde_json::from_slice(&bytes).unwrap();
}

#[test]
fn p6_result_metadata_log_and_signature() {
    let v = vectors();
    let meta_line = read("result.meta.json");
    assert_eq!(meta_line, s(&v, "result_meta").as_bytes());
    assert_eq!(sha256_hex(&meta_line), s(&v, "result_meta_sha256"));

    let meta = ResultMeta::parse(&meta_line).unwrap();
    assert_eq!(meta.to_bytes(), meta_line);
    assert_eq!(meta.log_bytes, 18);
    assert_eq!(meta.stdout_bytes, 11);
    meta.check_log(RESULT_LOG).unwrap();
    let (stdout, stderr) = meta.streams(RESULT_LOG).unwrap();
    assert_eq!((stdout, stderr), (&b"| gate ok\n\n"[..], &b"! warn\n"[..]));
    assert!(
        meta.verdict_raw
            .as_deref()
            .unwrap()
            .contains("\"accepted\"")
    );

    // The signed body is the metadata line alone (rule 5).
    let envelope = signer()
        .sign(Method::Post, RESULT_PATH, 4, 1_795_600_300_000, &meta_line)
        .unwrap();
    assert_eq!(envelope.to_headers()[4].1, s(&v, "result_sig"));

    let body = build_body(&meta_line, RESULT_LOG);
    let headers = envelope.to_headers();
    let header = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, value)| value.as_str())
    };
    let checked = check_request("POST", RESULT_PATH, header, &body).unwrap();
    assert_eq!(
        checked.verb,
        Verb::Result {
            job_id: meta.job_id.clone()
        }
    );
    assert_eq!(checked.signed_body, &meta_line[..]);
    assert_eq!(checked.log, RESULT_LOG);
    checked.verify(&node().public_key()).unwrap();
    // A changed log byte leaves the signature good and fails the log check.
    let mut log = RESULT_LOG.to_vec();
    log[0] = b'!';
    assert_eq!(meta.check_log(&log), Err(Error::LogMismatch));
}

#[test]
fn p6_result_log_fixture_is_the_eighteen_bytes_of_section_7() {
    // `*.log` is in the repository's .gitignore: a checkout without the
    // file (not force-added) still runs every other check on the constant.
    match std::fs::read(dir().join("result.log")) {
        Ok(bytes) => assert_eq!(bytes, RESULT_LOG),
        Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound, "{e}"),
    }
    assert_eq!(
        sha256_hex(RESULT_LOG),
        "b1828b9641adff7294821e40ef30e6e73c644d8fa89e9765445a76c82d2161da"
    );
}

#[test]
fn p6_tree_sha256_v1_of_the_tree_fixture() {
    let v = vectors();
    let tree = dir().join("tree");
    assert_eq!(tree_sha256_v1(&tree, &[]).unwrap(), s(&v, "tree_sha256_v1"));
    assert_eq!(
        scenarios_sha256(&tree).as_deref(),
        Some(s(&v, "tree_sha256_v1"))
    );
    // The same digest spelled out: sha256sum's lines, sorted bytewise.
    let mut lines = String::new();
    for path in ["build", "check-tests.sh", "sub/001.sh"] {
        lines.push_str(&format!(
            "{}  {path}\n",
            sha256_hex(&read(&format!("tree/{path}")))
        ));
    }
    assert_eq!(sha256_hex(lines.as_bytes()), s(&v, "tree_sha256_v1"));
}

#[test]
fn p6_enroll_token_digest_and_pin() {
    let v = vectors();
    let file = String::from_utf8(read("enroll.token.txt")).unwrap();
    assert_eq!(file, format!("{}\n", s(&v, "enroll_token")));
    let token = EnrollToken::from_file_contents(&file).unwrap();
    assert_eq!(token.as_str(), s(&v, "enroll_token"));
    assert_eq!(token.sha256(), s(&v, "enroll_token_sha256"));
    // Its last part is the first 16 bytes of the primary key id.
    assert_eq!(hex(token.pin()), s(&v, "primary_key_id")[..32]);
    assert_eq!(
        token.secret(),
        &std::array::from_fn::<u8, 32, _>(|i| 0x40 + i as u8)
    );
    let primary = primary().public_key();
    assert!(token.pins(&primary));
    assert_eq!(
        token.accept_primary(s(&v, "primary_pubkey_b64u")).unwrap(),
        primary
    );
    assert_eq!(
        token.accept_primary(s(&v, "node_pubkey_b64u")),
        Err(Error::PinMismatch)
    );
    assert_eq!(EnrollToken::new(*token.secret(), &primary), token);
}

fn reaction_of(text: &str) -> Reaction {
    match text {
        "log; do not resend the same bytes" => Reaction::DropRequest,
        "full jitter backoff" => Reaction::Backoff,
        "stop every job; exit 78; no restart loop" => Reaction::ExitRevoked,
        "offset_ms = server_time_ms - local_now_ms; retry at once, once" => {
            Reaction::AdoptServerClock
        }
        "seq = max(seq, local) + 1; retry" => Reaction::BumpSeq,
        "exit non-zero: another process owns the identity" => Reaction::ExitStaleBoot,
        "send a heartbeat now, then retry" => Reaction::BeatThenRetry,
        "keep results/<job>.<epoch>/ until GC; stop advertising it" => Reaction::KeepUntilGc,
        "truncate per section 3.5 and resend" => Reaction::TruncateAndResend,
        other => panic!("unknown reaction {other:?}"),
    }
}

#[test]
fn p6_every_refusal_body_and_signature() {
    let rows = json_file("refusals.json");
    let rows = rows.as_array().unwrap();
    let primary_key = primary();
    let primary = primary_key.public_key();
    let mut seen = BTreeSet::new();
    for row in rows {
        let status = u16::try_from(row["status"].as_u64().unwrap()).unwrap();
        let error = s(row, "error");
        let refusal =
            Refusal::from_wire(status, error).unwrap_or_else(|| panic!("{status} {error}"));
        seen.insert(format!("{refusal:?}"));
        assert_eq!(
            refusal.reaction(),
            reaction_of(s(row, "reaction")),
            "{error}"
        );

        let server_time_ms = row["server_time_ms"].as_u64().unwrap();
        let body = RefusalBody::new(refusal, server_time_ms).to_bytes();
        assert_eq!(body, s(row, "body").as_bytes(), "{error}");
        assert_eq!(sha256_hex(&body), s(row, "body_sha256"));
        assert_eq!(
            serde_json::from_slice::<RefusalBody>(&body)
                .unwrap()
                .refusal(status),
            Some(refusal)
        );

        // The request it answers is a legal one.
        let method = Method::parse(s(row, "method")).unwrap();
        Verb::route(method, s(row, "path")).unwrap();
        let request = RequestId {
            node_id: s(row, "node_id"),
            boot_id: s(row, "boot_id"),
            seq: row["seq"].as_u64().unwrap(),
        };
        let signature = sign_response(&primary_key, status, request, server_time_ms, &body);
        assert_eq!(signature.primary_key, s(row, "primary_key"));
        assert_eq!(signature.sig, s(row, "primary_sig"), "{error}");
        assert_eq!(
            verify_response(
                &primary,
                status,
                request,
                s(row, "primary_key"),
                s(row, "primary_sig"),
                &body
            ),
            Ok(server_time_ms),
            "{error}"
        );
        // Under another status the same signature does not verify.
        assert_eq!(
            verify_response(
                &primary,
                200,
                request,
                s(row, "primary_key"),
                s(row, "primary_sig"),
                &body
            ),
            Err(Error::Signature)
        );
    }
    // One row per refusal of section 2.4.
    assert_eq!(rows.len(), Refusal::ALL.len());
    assert_eq!(seen.len(), Refusal::ALL.len());
}

/// Every reject fixture and the error it must be refused with; all are
/// `400 bad_request`.
fn expected_reject(name: &str) -> Error {
    match name {
        "beat-4097-bytes.txt" => Error::TooLarge {
            limit: Limit::Beat,
            size: 4097,
        },
        "dot-segment.txt" | "dotdot-segment.txt" | "double-slash.txt" | "pct-in-path.txt" => {
            Error::Target
        }
        "duplicate-json-key.txt" => Error::Json("duplicate key"),
        "float.txt" => Error::Json("float"),
        "padded-signature.txt" => Error::Header("signature"),
        "seq-leading-zero.txt" => Error::Header("Spawn-Node-Seq"),
        other => panic!("reject fixture {other} has no expectation"),
    }
}

#[test]
fn p6_every_reject_fixture_is_a_bad_request() {
    let mut names: Vec<String> = std::fs::read_dir(dir().join("reject"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names.len(), 9, "{names:?}");
    for name in names {
        let raw = RawRequest::parse(&read(&format!("reject/{name}")));
        let err = raw.check().expect_err(&name);
        assert_eq!(err, expected_reject(&name), "{name}");
        assert_eq!(err.refusal(), Refusal::BadRequest, "{name}");
        assert_eq!(err.refusal().status(), 400);
    }
}

#[test]
fn p6_reject_fixtures_are_refused_for_their_rule_not_their_signature() {
    // Each fixture is signed correctly over its own bytes, so the refusal
    // comes from the rule it breaks; a node never produces any of them.
    let key = node().public_key();
    let signer = signer();
    for name in [
        "beat-4097-bytes.txt",
        "dot-segment.txt",
        "dotdot-segment.txt",
        "double-slash.txt",
        "duplicate-json-key.txt",
        "float.txt",
        "pct-in-path.txt",
        "padded-signature.txt",
        "seq-leading-zero.txt",
    ] {
        let raw = RawRequest::parse(&read(&format!("reject/{name}")));
        let method = Method::parse(&raw.method).unwrap();
        let seq: u64 = raw.header("Spawn-Node-Seq").unwrap().parse().unwrap();
        let ts: u64 = raw.header("Spawn-Node-Ts").unwrap().parse().unwrap();
        let input = request_signing_input(
            method,
            &raw.target,
            &signer.node_id,
            &signer.boot_id,
            seq,
            ts,
            &raw.body,
        );
        let sig = raw.header("Spawn-Node-Sig").unwrap().trim_end_matches('=');
        key.verify_b64u(&input, sig)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        // The signer itself refuses to make a target that breaks rule 3.
        if spawn_node_proto::check_target(&raw.target).is_err() {
            assert_eq!(
                signer.sign(method, &raw.target, seq, ts, &raw.body),
                Err(Error::Target),
                "{name}"
            );
        }
    }
    let beat = RawRequest::parse(&read("reject/beat-4097-bytes.txt"));
    assert_eq!(beat.body.len(), 4097);
    assert!(Beat::parse(&beat.body).is_err());
}

#[test]
fn p6_seq_zero_is_for_enroll_only() {
    let body = read("heartbeat.req.json");
    let envelope = signer()
        .sign(Method::Post, BEAT_PATH, 0, 1_795_600_000_000, &body)
        .unwrap();
    let headers = envelope.to_headers();
    let header = |name: &str| {
        headers
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, value)| value.as_str())
    };
    assert_eq!(
        check_request("POST", BEAT_PATH, header, &body).unwrap_err(),
        Error::Header("Spawn-Node-Seq")
    );
    let missing = |name: &str| (name != "Spawn-Node-Ts").then(|| header(name)).flatten();
    assert_eq!(
        check_request("POST", BEAT_PATH, missing, &body).unwrap_err(),
        Error::MissingHeader("Spawn-Node-Ts")
    );
}

#[test]
fn p6_manifest_lists_every_fixture_file_with_its_sha256() {
    let manifest = String::from_utf8(read("MANIFEST.sha256")).unwrap();
    let mut files = Vec::new();
    collect(&dir(), "", &mut files);
    files.retain(|path| path != "MANIFEST.sha256");
    files.sort();
    let mut expected = String::new();
    for path in &files {
        expected.push_str(&format!("{}  {path}\n", sha256_hex(&read(path))));
    }
    // `result.log` may be absent from a checkout (`.gitignore`'s `*.log`).
    let manifest: String = manifest
        .lines()
        .filter(|line| !line.ends_with("  result.log") || files.iter().any(|f| f == "result.log"))
        .map(|line| format!("{line}\n"))
        .collect();
    assert_eq!(manifest, expected);
}

fn collect(root: &std::path::Path, prefix: &str, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(root.join(prefix)).unwrap() {
        let entry = entry.unwrap();
        let name = format!("{prefix}{}", entry.file_name().into_string().unwrap());
        if entry.file_type().unwrap().is_dir() {
            collect(root, &format!("{name}/"), out);
        } else {
            out.push(name);
        }
    }
}
