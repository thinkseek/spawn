//! Cross-language verification of `docs/spawn-e2e-v2.md` and the golden
//! fixtures in `testdata/spawn-e2e-v2/` (AES-256-GCM + HKDF-SHA256 + X25519 +
//! Ed25519), cross-verified against Node.js / WebCrypto `crypto.subtle`.

use std::collections::BTreeSet;
use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::agreement::{self, EphemeralPrivateKey, X25519};
use ring::hkdf::{HKDF_SHA256, KeyType, Salt};
use ring::rand::SystemRandom;
use serde_json::Value;
use spawn_node_proto::codec::{b64u, hex, sha256_hex, unb64u, unhex};
use spawn_node_proto::{PublicKey, SigningKey};

struct KeyLen(usize);

impl KeyType for KeyLen {
    fn len(&self) -> usize {
        self.0
    }
}

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/spawn-e2e-v2")
}

fn read(name: &str) -> Vec<u8> {
    std::fs::read(dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn read_json(name: &str) -> Value {
    serde_json::from_slice(&read(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn hkdf_derive_32(ikm: &[u8], salt: &[u8], info: &[u8]) -> [u8; 32] {
    let salt = Salt::new(HKDF_SHA256, salt);
    let prk = salt.extract(ikm);
    let infos = [info];
    let okm = prk.expand(&infos, KeyLen(32)).expect("hkdf expand 32");
    let mut out = [0_u8; 32];
    okm.fill(&mut out).expect("hkdf fill 32");
    out
}

fn aes_gcm_seal(key: &[u8; 32], nonce: &[u8; 12], aad: &[u8], plain: &[u8]) -> Vec<u8> {
    let unbound = UnboundKey::new(&AES_256_GCM, key).expect("unbound key");
    let less_safe = LessSafeKey::new(unbound);
    let mut buf = plain.to_vec();
    less_safe
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(*nonce),
            Aad::from(aad),
            &mut buf,
        )
        .expect("aes-256-gcm seal");
    buf
}

fn aes_gcm_open(
    key: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
    ct_and_tag: &[u8],
) -> Result<Vec<u8>, ring::error::Unspecified> {
    let unbound = UnboundKey::new(&AES_256_GCM, key)?;
    let less_safe = LessSafeKey::new(unbound);
    let mut buf = ct_and_tag.to_vec();
    let plain = less_safe.open_in_place(
        Nonce::assume_unique_for_key(*nonce),
        Aad::from(aad),
        &mut buf,
    )?;
    Ok(plain.to_vec())
}

fn format_request_header(env: &Value) -> String {
    [
        "spawn-e2e-req-v2",
        env["from"].as_str().unwrap(),
        env["to"].as_str().unwrap(),
        &env["epoch"].as_u64().unwrap().to_string(),
        env["to_x25519"].as_str().unwrap(),
        env["channel_id"].as_str().unwrap(),
        &env["at_ms"].as_i64().unwrap().to_string(),
        env["eph"].as_str().unwrap(),
        env["req_nonce"].as_str().unwrap(),
        env["salt"].as_str().unwrap(),
    ]
    .join("\n")
}

fn format_reply_aad(reply: &Value) -> String {
    [
        "spawn-e2e-reply-v2",
        reply["from"].as_str().unwrap(),
        reply["to"].as_str().unwrap(),
        reply["channel_id"].as_str().unwrap(),
        reply["req_nonce"].as_str().unwrap(),
        reply["reply_nonce"].as_str().unwrap(),
        reply["reply_salt"].as_str().unwrap(),
    ]
    .join("\n")
}

#[test]
fn spawn_e2e_v2_manifest_matches_all_fixtures() {
    let manifest = std::fs::read_to_string(dir().join("MANIFEST.sha256")).unwrap();
    let mut listed = BTreeSet::new();
    for line in manifest.lines() {
        let (expected, name) = line.split_once("  ").expect("manifest line");
        assert!(listed.insert(name.to_string()), "duplicate {name}");
        assert_eq!(
            sha256_hex(&read(name)),
            expected,
            "hash mismatch for {name}"
        );
    }
    let mut actual = BTreeSet::new();
    for entry in std::fs::read_dir(dir()).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        if name != "MANIFEST.sha256" {
            actual.insert(name);
        }
    }
    assert_eq!(listed, actual);
}

#[test]
fn spawn_e2e_v2_golden_request_and_reply_match_webcrypto_bytes() {
    let v = read_json("vectors.json");
    assert_eq!(v["v"], "spawn-e2e-v2");

    let sender_seed: [u8; 32] = unhex(v["sender_ed25519_seed_hex"].as_str().unwrap(), 64)
        .unwrap()
        .try_into()
        .unwrap();
    let responder_seed: [u8; 32] = unhex(v["responder_ed25519_seed_hex"].as_str().unwrap(), 64)
        .unwrap()
        .try_into()
        .unwrap();
    let sender_key = SigningKey::from_seed(&sender_seed).unwrap();
    let responder_key = SigningKey::from_seed(&responder_seed).unwrap();

    assert_eq!(
        sender_key.public_key().to_b64u(),
        v["sender_ed25519_pub_b64u"].as_str().unwrap()
    );
    assert_eq!(
        sender_key.public_key().key_id(),
        v["sender_id"].as_str().unwrap()
    );
    assert_eq!(
        responder_key.public_key().to_b64u(),
        v["responder_ed25519_pub_b64u"].as_str().unwrap()
    );
    assert_eq!(
        responder_key.public_key().key_id(),
        v["responder_id"].as_str().unwrap()
    );

    let req = &v["request_envelope"];
    let req_header = format_request_header(req);
    assert_eq!(req_header, v["request_signing_input"].as_str().unwrap());

    assert_eq!(
        sender_key.sign_b64u(req_header.as_bytes()),
        req["sig"].as_str().unwrap()
    );
    let from_pub = PublicKey::from_b64u(req["from_pub"].as_str().unwrap()).unwrap();
    assert_eq!(from_pub.key_id(), req["from"].as_str().unwrap());
    from_pub
        .verify_b64u(req_header.as_bytes(), req["sig"].as_str().unwrap())
        .unwrap();

    let shared = unhex(v["shared_secret_hex"].as_str().unwrap(), 64).unwrap();
    let req_salt = unb64u(req["salt"].as_str().unwrap(), 32).unwrap();
    let req_nonce: [u8; 12] = unb64u(req["req_nonce"].as_str().unwrap(), 12)
        .unwrap()
        .try_into()
        .unwrap();
    let req_info = format!("spawn-e2e-v2:request\n{req_header}");
    let req_key = hkdf_derive_32(&shared, &req_salt, req_info.as_bytes());
    assert_eq!(hex(&req_key), v["request_key_hex"].as_str().unwrap());

    let plain_bytes = v["request_plaintext"].as_str().unwrap().as_bytes();
    let sealed_ct = aes_gcm_seal(&req_key, &req_nonce, req_header.as_bytes(), plain_bytes);
    assert_eq!(b64u(&sealed_ct), req["ct"].as_str().unwrap());
    let opened_req = aes_gcm_open(&req_key, &req_nonce, req_header.as_bytes(), &sealed_ct).unwrap();
    assert_eq!(opened_req, plain_bytes);

    let reply = &v["reply_envelope"];
    let reply_salt = unb64u(reply["reply_salt"].as_str().unwrap(), 32).unwrap();
    let reply_nonce: [u8; 12] = unb64u(reply["reply_nonce"].as_str().unwrap(), 12)
        .unwrap()
        .try_into()
        .unwrap();
    let reply_info = [
        "spawn-e2e-v2:reply",
        req["from"].as_str().unwrap(),
        req["to"].as_str().unwrap(),
        req["channel_id"].as_str().unwrap(),
        req["req_nonce"].as_str().unwrap(),
        reply["reply_nonce"].as_str().unwrap(),
    ]
    .join("\n");
    assert_eq!(reply_info, v["reply_info"].as_str().unwrap());
    let reply_aad = format_reply_aad(reply);
    assert_eq!(reply_aad, v["reply_aad"].as_str().unwrap());

    let reply_key = hkdf_derive_32(&shared, &reply_salt, reply_info.as_bytes());
    assert_eq!(hex(&reply_key), v["reply_key_hex"].as_str().unwrap());

    let reply_plain = v["reply_plaintext"].as_str().unwrap().as_bytes();
    let reply_ct = aes_gcm_seal(&reply_key, &reply_nonce, reply_aad.as_bytes(), reply_plain);
    assert_eq!(b64u(&reply_ct), reply["ct"].as_str().unwrap());
    let opened_reply =
        aes_gcm_open(&reply_key, &reply_nonce, reply_aad.as_bytes(), &reply_ct).unwrap();
    assert_eq!(opened_reply, reply_plain);
}

#[test]
fn spawn_e2e_v2_rejects_low_order_points_tampering_and_impostors() {
    let v = read_json("vectors.json");
    let shared = unhex(v["shared_secret_hex"].as_str().unwrap(), 64).unwrap();
    let req = &v["request_envelope"];
    let req_header = v["request_signing_input"].as_str().unwrap();
    let req_salt = unb64u(req["salt"].as_str().unwrap(), 32).unwrap();
    let req_nonce: [u8; 12] = unb64u(req["req_nonce"].as_str().unwrap(), 12)
        .unwrap()
        .try_into()
        .unwrap();
    let req_key = hkdf_derive_32(
        &shared,
        &req_salt,
        format!("spawn-e2e-v2:request\n{req_header}").as_bytes(),
    );

    let rng = SystemRandom::new();
    let rejects = read_json("reject.json");
    for item in rejects.as_array().unwrap() {
        let case = item["case"].as_str().unwrap();
        if let Some(pt_b64u) = item["x25519_pub_b64u"].as_str() {
            let pt = URL_SAFE_NO_PAD.decode(pt_b64u).unwrap();
            let eph = EphemeralPrivateKey::generate(&X25519, &rng).unwrap();
            let peer = agreement::UnparsedPublicKey::new(&X25519, &pt);
            assert!(
                agreement::agree_ephemeral(eph, &peer, <[u8]>::to_vec).is_err(),
                "{case}: low-order X25519 point must fail"
            );
            continue;
        }
        let env = &item["envelope"];
        match case {
            "tampered_ct_tag" => {
                let ct = URL_SAFE_NO_PAD.decode(env["ct"].as_str().unwrap()).unwrap();
                assert!(aes_gcm_open(&req_key, &req_nonce, req_header.as_bytes(), &ct).is_err());
            }
            "tampered_sig" => {
                let pubkey = PublicKey::from_b64u(env["from_pub"].as_str().unwrap()).unwrap();
                assert!(
                    pubkey
                        .verify_b64u(req_header.as_bytes(), env["sig"].as_str().unwrap())
                        .is_err()
                );
            }
            "channel_mismatch_aad" => {
                let bad_header = format_request_header(env);
                let pubkey = PublicKey::from_b64u(env["from_pub"].as_str().unwrap()).unwrap();
                assert!(
                    pubkey
                        .verify_b64u(bad_header.as_bytes(), env["sig"].as_str().unwrap())
                        .is_err()
                );
                let ct = URL_SAFE_NO_PAD.decode(env["ct"].as_str().unwrap()).unwrap();
                assert!(aes_gcm_open(&req_key, &req_nonce, bad_header.as_bytes(), &ct).is_err());
            }
            "impostor_from_pub" => {
                let pubkey = PublicKey::from_b64u(env["from_pub"].as_str().unwrap()).unwrap();
                assert_ne!(pubkey.key_id(), env["from"].as_str().unwrap());
                assert!(
                    pubkey
                        .verify_b64u(req_header.as_bytes(), env["sig"].as_str().unwrap())
                        .is_err()
                );
            }
            other => panic!("unexpected reject case: {other}"),
        }
    }

    // Live X25519 + HKDF-SHA256 + AES-256-GCM ephemeral agreement round-trip in ring
    let alice = EphemeralPrivateKey::generate(&X25519, &rng).unwrap();
    let bob = EphemeralPrivateKey::generate(&X25519, &rng).unwrap();
    let alice_pub = alice.compute_public_key().unwrap();
    let bob_pub = bob.compute_public_key().unwrap();
    let alice_shared = agreement::agree_ephemeral(
        alice,
        &agreement::UnparsedPublicKey::new(&X25519, bob_pub.as_ref()),
        <[u8]>::to_vec,
    )
    .unwrap();
    let bob_shared = agreement::agree_ephemeral(
        bob,
        &agreement::UnparsedPublicKey::new(&X25519, alice_pub.as_ref()),
        <[u8]>::to_vec,
    )
    .unwrap();
    assert_eq!(alice_shared, bob_shared);
    let key = hkdf_derive_32(&alice_shared, &req_salt, b"spawn-e2e-v2:live-check");
    let ct = aes_gcm_seal(&key, &req_nonce, b"aad", b"{\"live\":true}");
    let pt = aes_gcm_open(&key, &req_nonce, b"aad", &ct).unwrap();
    assert_eq!(pt, b"{\"live\":true}");
}
