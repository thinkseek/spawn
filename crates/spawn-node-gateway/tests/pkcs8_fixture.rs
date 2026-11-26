//! Verifies `spawn_node_gateway::primary::from_pkcs8` accepts all three frozen
//! Ed25519 PKCS#8 formats (`v1` 48 B, `v2_ring` 83 B, `v2_rfc8410` 85 B) from
//! `spawn-node-proto/testdata/pkcs8-ed25519-v2/` and rejects every malformed
//! vector.

use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../spawn-node-proto/testdata/pkcs8-ed25519-v2")
}

#[test]
fn gateway_from_pkcs8_accepts_v1_v2_ring_and_v2_rfc8410_fixtures() {
    let dir = fixture_dir();
    let vectors: Value =
        serde_json::from_slice(&std::fs::read(dir.join("vectors.json")).unwrap()).unwrap();
    let msg = vectors["message"].as_str().unwrap().as_bytes();
    let expected_pub = vectors["pubkey_b64u"].as_str().unwrap();
    let expected_key_id = vectors["key_id"].as_str().unwrap();
    let expected_sig = vectors["signature_b64u"].as_str().unwrap();

    for label in ["v1", "v2_ring", "v2_rfc8410"] {
        let file = vectors["formats"][label]["file"].as_str().unwrap();
        let der = std::fs::read(dir.join(file)).unwrap();
        let signing = spawn_node_gateway::primary::from_pkcs8(&der)
            .unwrap_or_else(|e| panic!("{label} ({file}) failed to parse: {e}"));
        assert_eq!(signing.public_key().to_b64u(), expected_pub, "{label}");
        assert_eq!(signing.public_key().key_id(), expected_key_id, "{label}");
        assert_eq!(signing.sign_b64u(msg), expected_sig, "{label}");
    }

    let rejects: Value =
        serde_json::from_slice(&std::fs::read(dir.join("reject.json")).unwrap()).unwrap();
    for item in rejects.as_array().unwrap() {
        let case = item["case"].as_str().unwrap();
        let der = URL_SAFE_NO_PAD
            .decode(item["der_b64u"].as_str().unwrap())
            .unwrap();
        assert!(
            spawn_node_gateway::primary::from_pkcs8(&der).is_err(),
            "reject case {case} should fail from_pkcs8"
        );
    }
}
