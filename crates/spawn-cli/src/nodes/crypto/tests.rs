//! The mesh's cryptography: X25519 against RFC 7748 and `ring`, the
//! identities and their key files, the sealed envelope (opened once, by
//! its node only, under the token or the one before, never from a revoked
//! sender, never changed), and the wire seal.

use super::{
    KeySource, MAX_SEALED, NodeIdentity, PeerKeys, Responder, TrustPolicy, b64, envelope_error,
    node_id_of, open_wire, replay_until, seal_request, seal_wire, unb64, valid_node_id, x25519,
};
use crate::nodes::auth::MAX_SKEW_MS;
use crate::verify::now_ms;
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::Arc;

fn unhex(text: &str) -> [u8; 32] {
    let mut out = [0_u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).expect("hex");
    }
    out
}

#[test]
fn x25519_matches_rfc_7748_and_ring() {
    // RFC 7748 section 5.2.
    assert_eq!(
        x25519::scalar_mult(
            &unhex("a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4"),
            &unhex("e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c"),
        ),
        unhex("c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552")
    );
    assert_eq!(
        x25519::scalar_mult(
            &unhex("4b66e9d4d1b4673c5ad22691957d6af5c11b6421e0ea01d42ca4169e7918ba0d"),
            &unhex("e5210f12786811d3f4b7959d0538ae2c31dbe7106fc03c3efc4cd549c715a493"),
        ),
        unhex("95cbde9476e8907d7aade45cb4b873f88b595a68799fa152e6f8f7647aac7957")
    );
    // Section 6.1: Alice and Bob.
    let alice = unhex("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
    let bob = unhex("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb");
    assert_eq!(
        x25519::public(&alice),
        unhex("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a")
    );
    assert_eq!(
        x25519::public(&bob),
        unhex("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f")
    );
    let shared = unhex("4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742");
    assert_eq!(x25519::scalar_mult(&alice, &x25519::public(&bob)), shared);
    assert_eq!(x25519::scalar_mult(&bob, &x25519::public(&alice)), shared);
    // Section 5.2's iteration, 1000 times.
    let (mut k, mut u) = (
        unhex("0900000000000000000000000000000000000000000000000000000000000000"),
        unhex("0900000000000000000000000000000000000000000000000000000000000000"),
    );
    for round in 1..=1000 {
        let next = x25519::scalar_mult(&k, &u);
        u = k;
        k = next;
        if round == 1 {
            assert_eq!(
                k,
                unhex("422c8e7a6227d7bca1350b3e2bb7279f7897b87bb6854b783c60e80311ae3079")
            );
        }
    }
    assert_eq!(
        k,
        unhex("684cf59ba83309552800ef566f2f4d3c1c3887c49360e3875f2eb94d99532c51")
    );
    // Against ring's own X25519, both ways, for random keys.
    let rng = ring::rand::SystemRandom::new();
    for _ in 0..16 {
        let theirs = ring::agreement::EphemeralPrivateKey::generate(&ring::agreement::X25519, &rng)
            .expect("a key");
        let their_public = theirs.compute_public_key().expect("its public key");
        let ours = super::random::<32>().expect("random");
        let our_public = x25519::public(&ours);
        let by_ring = ring::agreement::agree_ephemeral(
            theirs,
            &ring::agreement::UnparsedPublicKey::new(&ring::agreement::X25519, our_public),
            <[u8]>::to_vec,
        )
        .expect("agreed");
        let mut point = [0_u8; 32];
        point.copy_from_slice(their_public.as_ref());
        assert_eq!(by_ring, x25519::scalar_mult(&ours, &point).to_vec());
    }
}

/// A directory under `TMPDIR`, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "spawn-node-key-{tag}-{}-{}",
            std::process::id(),
            crate::nodes::transport::nonce()
        ));
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn an_identity_is_made_once_in_its_key_file_mode_0600_and_read_back() {
    let scratch = Scratch::new("file");
    let file = scratch.0.join("deep/er/node.key");
    let (made, source) = NodeIdentity::load_from(Some(&file), None).expect("made");
    assert_eq!(source, KeySource::File(file.clone(), true));
    let file_mode = std::fs::metadata(&file)
        .expect("the file")
        .permissions()
        .mode();
    assert_eq!(file_mode & 0o777, 0o600, "{file_mode:o}");
    let dir_mode = std::fs::metadata(file.parent().expect("a parent"))
        .expect("the directory")
        .permissions()
        .mode();
    assert_eq!(dir_mode & 0o777, 0o700, "{dir_mode:o}");
    let (read, source) = NodeIdentity::load_from(Some(&file), Some("ignored")).expect("read");
    assert_eq!(source, KeySource::File(file.clone(), false));
    assert_eq!(read.node_id(), made.node_id());
    assert!(valid_node_id(made.node_id()));
    assert_eq!(made.node_id(), node_id_of(&made.pubkey()));
    assert_eq!(unb64(&made.pubkey_b64()).expect("base64").len(), 32);
    // SPAWN_NODE_KEY: the same key, from its text.
    let text = std::fs::read_to_string(&file).expect("the key's text");
    let (env, source) = NodeIdentity::load_from(None, Some(text.trim())).expect("from text");
    assert_eq!((env.node_id(), source), (made.node_id(), KeySource::Env));
    assert!(NodeIdentity::load_from(None, Some("bm90IGEga2V5")).is_err());
    // Neither: a key for this run, a new one each time.
    let (one, source) = NodeIdentity::load_from(None, None).expect("ephemeral");
    assert_eq!(source, KeySource::Ephemeral);
    assert_ne!(
        one.node_id(),
        NodeIdentity::load_from(None, None)
            .expect("another")
            .0
            .node_id()
    );
    // A file that is not a key is refused, never overwritten.
    let bad = scratch.0.join("bad.key");
    std::fs::write(&bad, "garbage\n").expect("written");
    assert!(NodeIdentity::load_from(Some(&bad), None).is_err());
    assert_eq!(std::fs::read_to_string(&bad).expect("kept"), "garbage\n");
}

#[test]
fn a_prekey_is_signed_by_its_node_and_checked_against_the_id_and_pin() {
    let (node, _) = NodeIdentity::generate().expect("a node");
    let info = node.public_info();
    let peer =
        PeerKeys::verify(&info, Some(node.node_id()), Some(&node.pubkey_b64())).expect("verified");
    assert_eq!(peer.node_id, node.node_id());
    let (other, _) = NodeIdentity::generate().expect("another");
    // Another node asked for; another key pinned; an id not its key's; a
    // prekey signed by another; a prekey's epoch changed.
    assert!(PeerKeys::verify(&info, Some(other.node_id()), None).is_err());
    assert!(PeerKeys::verify(&info, None, Some(&other.pubkey_b64())).is_err());
    let mut forged = info.clone();
    forged["node_id"] = json!(other.node_id());
    assert!(PeerKeys::verify(&forged, None, None).is_err());
    let mut swapped = info.clone();
    swapped["x25519_pub"] = other.public_info()["x25519_pub"].clone();
    assert!(PeerKeys::verify(&swapped, None, None).is_err());
    let mut epoch = info.clone();
    epoch["epoch"] = json!(info["epoch"].as_u64().expect("an epoch") + 1);
    assert!(PeerKeys::verify(&epoch, None, None).is_err());
    // A rotation: a later epoch, still verified.
    let before = node.bundle();
    let after = node.rotate().expect("rotated");
    assert!(after.epoch > before.epoch);
    PeerKeys::verify(&node.public_info(), None, None).expect("the new prekey verifies");
}

fn pair(policy: &TrustPolicy) -> (Arc<NodeIdentity>, Responder, PeerKeys) {
    let (me, _) = NodeIdentity::generate().expect("me");
    let (node, _) = NodeIdentity::generate().expect("node");
    let node = Arc::new(node);
    let peer = PeerKeys::verify(&node.public_info(), None, None).expect("peer");
    (Arc::new(me), Responder::new(node, policy.clone()), peer)
}

#[test]
fn an_envelope_opens_once_at_its_node_and_its_reply_only_for_its_sender() {
    let policy = TrustPolicy::new(Some("fleet-token"), None, None);
    let (me, node, peer) = pair(&policy);
    let request = json!({"op": "inventory", "secret_arg": "sk-ant-api03-hidden"});
    let (envelope, reply_key) =
        seal_request(&me, &peer, &policy, "probe-1", &request).expect("sealed");
    // What a relay sees: ids and keys, no op, argument or token.
    let seen = envelope.to_string();
    for hidden in ["inventory", "secret_arg", "sk-ant", "fleet-token"] {
        assert!(!seen.contains(hidden), "{hidden} in {seen}");
    }
    let opened = node.open(&envelope).expect("opened");
    assert_eq!(opened.request, request);
    assert_eq!(opened.from, me.node_id());
    // Opened once: a replay is refused.
    assert!(
        node.open(&envelope)
            .expect_err("a replay")
            .contains("replayed")
    );
    let reply = opened
        .reply
        .seal(&json!({"ok": true, "output": "@end\n"}))
        .expect("sealed reply");
    assert!(!reply.to_string().contains("@end"));
    assert_eq!(reply_key.open(&reply).expect("opened")["output"], "@end\n");
    // A reply changed on its way, or for another request, does not open.
    let mut changed = reply.clone();
    let mut ct = unb64(changed["ct"].as_str().expect("ct")).expect("b64");
    ct[0] ^= 1;
    changed["ct"] = json!(b64(&ct));
    assert!(reply_key.open(&changed).is_err());
    let (_, other_key) = seal_request(&me, &peer, &policy, "probe-2", &request).expect("sealed");
    assert!(other_key.open(&reply).is_err());
    // A refusal says why.
    assert!(
        reply_key
            .open(&envelope_error("revoked", &envelope))
            .expect_err("refused")
            .contains("revoked")
    );
}

#[test]
fn a_tampered_stale_misaddressed_or_unsigned_envelope_is_refused() {
    let policy = TrustPolicy::new(Some("fleet-token"), None, None);
    let (me, node, peer) = pair(&policy);
    let seal = || {
        seal_request(&me, &peer, &policy, "c", &json!({"op": "beat"}))
            .expect("sealed")
            .0
    };
    for (key, value) in [
        ("channel_id", json!("other")),
        ("at_ms", json!(now_ms() - 1)),
        ("epoch", json!(1)),
        ("from", json!("0".repeat(64))),
        ("to", json!("0".repeat(64))),
        ("sig", json!(b64(&[0; 64]))),
        ("eph", json!(b64(&x25519::public(&[7; 32])))),
        ("req_nonce", json!(b64(&[1; 12]))),
        ("v", json!("spawn-e2e-v0")),
    ] {
        let mut tampered = seal();
        tampered[key] = value;
        assert!(node.open(&tampered).is_err(), "{key} tampered");
    }
    let mut flipped = seal();
    let mut ct = unb64(flipped["ct"].as_str().expect("ct")).expect("b64");
    let last = ct.len() - 1;
    ct[last] ^= 0x80;
    flipped["ct"] = json!(b64(&ct));
    assert!(
        node.open(&flipped)
            .expect_err("tampered")
            .contains("does not open")
    );
    // The sender re-signs a forged header with its own key: still bound to
    // the keys the AEAD was sealed under, so it does not open.
    let mut resigned = seal();
    resigned["channel_id"] = json!("forged");
    let header = super::EnvelopeHeader::of(&resigned).expect("a header");
    resigned["sig"] = json!(b64(&me.sign(&header.canonical())));
    assert!(node.open(&resigned).is_err());
    // A sender pretending to be another: its key is not that id's.
    let (mallory, _) = NodeIdentity::generate().expect("mallory");
    let mut impostor = seal();
    impostor["from_pub"] = json!(mallory.pubkey_b64());
    assert!(
        node.open(&impostor)
            .expect_err("impostor")
            .contains("id is not its key")
    );
    // Stale: sealed past the skew.
    let mut stale = seal();
    stale["at_ms"] = json!(now_ms() - MAX_SKEW_MS - 1000);
    assert!(node.open(&stale).expect_err("stale").contains("at_ms"));
    // An envelope for another node.
    let (_, other, other_peer) = pair(&policy);
    let to_other = seal_request(&me, &other_peer, &policy, "c", &json!({"op": "beat"}))
        .expect("sealed")
        .0;
    assert!(
        node.open(&to_other)
            .expect_err("not mine")
            .contains("another node")
    );
    assert!(other.open(&to_other).is_ok());
    // Without the token, or with another: it does not open.
    for wrong in [
        TrustPolicy::new(None, None, None),
        TrustPolicy::new(Some("other"), None, None),
    ] {
        let envelope = seal_request(&me, &peer, &wrong, "c", &json!({"op": "beat"}))
            .expect("sealed")
            .0;
        assert!(
            node.open(&envelope)
                .expect_err("no token")
                .contains("does not open")
        );
    }
    // An oversized ciphertext is refused before anything is derived.
    let mut huge = seal();
    huge["ct"] = json!(b64(&vec![0; MAX_SEALED + 1]));
    assert!(node.open(&huge).is_err());
}

#[test]
fn a_revoked_sender_or_node_is_refused_both_ways() {
    let (me, _) = NodeIdentity::generate().expect("me");
    let (node, _) = NodeIdentity::generate().expect("node");
    let node = Arc::new(node);
    let peer = PeerKeys::verify(&node.public_info(), None, None).expect("peer");
    let revoking_me = TrustPolicy::new(
        Some("t"),
        None,
        Some(&format!(" {} ,junk", me.node_id().to_uppercase())),
    );
    assert!(revoking_me.revoked(me.node_id()));
    let open = TrustPolicy::new(Some("t"), None, None);
    let envelope = seal_request(&me, &peer, &open, "c", &json!({"op": "beat"}))
        .expect("sealed")
        .0;
    let responder = Responder::new(Arc::clone(&node), revoking_me);
    assert!(
        responder
            .open(&envelope)
            .expect_err("revoked")
            .contains("revoked")
    );
    // A sender refuses to seal to a revoked node.
    let revoking_node = TrustPolicy::new(Some("t"), None, Some(node.node_id()));
    assert!(seal_request(&me, &peer, &revoking_node, "c", &json!({})).is_err());
}

#[test]
fn a_token_rotation_takes_the_previous_token_and_a_prekey_rotation_the_previous_prekey() {
    let (me, _) = NodeIdentity::generate().expect("me");
    let (node, _) = NodeIdentity::generate().expect("node");
    let node = Arc::new(node);
    let peer = PeerKeys::verify(&node.public_info(), None, None).expect("peer");
    let rotated = Responder::new(
        Arc::clone(&node),
        TrustPolicy::new(Some("new"), Some("old"), None),
    );
    for token in ["new", "old"] {
        let sender = TrustPolicy::new(Some(token), None, None);
        let (envelope, key) =
            seal_request(&me, &peer, &sender, "c", &json!({"op": "beat"})).expect("sealed");
        let opened = rotated.open(&envelope).expect("opened under either token");
        let reply = opened.reply.seal(&json!({"ok": true})).expect("sealed");
        assert_eq!(key.open(&reply).expect("the sender opens it")["ok"], true);
    }
    let sender = TrustPolicy::new(Some("older"), None, None);
    let envelope = seal_request(&me, &peer, &sender, "c", &json!({}))
        .expect("sealed")
        .0;
    assert!(rotated.open(&envelope).is_err());
    // A previous token alone is no token.
    assert!(
        TrustPolicy::new(None, Some("old"), None)
            .tokens()
            .is_empty()
    );
    // An envelope to the prekey before the current one still opens; to
    // the one before that, not.
    let old = seal_request(&me, &peer, &sender, "c", &json!({}))
        .expect("sealed")
        .0;
    node.rotate().expect("rotated");
    let policy = TrustPolicy::new(Some("older"), None, None);
    let responder = Responder::new(Arc::clone(&node), policy);
    assert!(responder.open(&old).is_ok());
    let older = seal_request(&me, &peer, &sender, "c", &json!({}))
        .expect("sealed")
        .0;
    node.rotate().expect("rotated again");
    assert!(
        responder
            .open(&older)
            .expect_err("dropped")
            .contains("no longer holds")
    );
}

#[test]
fn a_replay_is_remembered_until_its_time_can_no_longer_pass() {
    let now = now_ms();
    // A request from a clock running fast is kept past now + the skew.
    assert_eq!(
        replay_until(now + 60_000, now),
        now + 60_000 + MAX_SKEW_MS + 5_000
    );
    assert_eq!(replay_until(now, now), now + MAX_SKEW_MS + 5_000);
    // Never more than twice the skew, whatever its at_ms says.
    assert_eq!(replay_until(i64::MAX, now), now + 2 * MAX_SKEW_MS + 10_000);
}

#[test]
fn the_wire_seal_opens_under_either_token_and_hides_the_request() {
    let message = json!({"nonce": "n0123456789abcdef", "op": "register", "name": "edge",
                         "inventory": "@arch\nx86_64\n@end\n", "at_ms": now_ms()});
    let (sealed, key) = seal_wire(&message, "new").expect("sealed");
    assert_eq!(sealed["op"], "sealed");
    assert_eq!(sealed["nonce"], message["nonce"]);
    assert_eq!(sealed["auth"].as_str().map(str::len), Some(64));
    for hidden in ["register", "edge", "x86_64", "new"] {
        assert!(!sealed.to_string().contains(hidden), "{hidden}: {sealed}");
    }
    let (opened, reply_key) = open_wire(&sealed, &["new", "old"]).expect("opened");
    assert_eq!(opened, message);
    let reply = reply_key
        .seal_reply(&json!({"nonce": "n0123456789abcdef", "ok": true, "event": 7}))
        .expect("sealed");
    assert!(!reply.to_string().contains("event"));
    assert_eq!(key.open_reply(&reply).expect("opened")["event"], 7);
    // The previous token opens one sealed with it; another does not.
    let (by_old, _) = seal_wire(&message, "old").expect("sealed");
    assert!(open_wire(&by_old, &["new", "old"]).is_ok());
    assert!(
        open_wire(&by_old, &["new"])
            .expect_err("not taken")
            .contains("auth")
    );
    // Changed on its way: refused (its auth, or its AEAD).
    for (field, value) in [
        ("ct", json!(b64(&[0; 40]))),
        ("salt", json!(b64(&[1; 32]))),
        ("nonce", json!("n0123456789abcdeX")),
    ] {
        let mut tampered = sealed.clone();
        tampered[field] = value;
        assert!(open_wire(&tampered, &["new"]).is_err(), "{field}");
    }
    // A plaintext answer to a sealed request is no answer.
    assert!(
        key.open_reply(&json!({"nonce": "n0123456789abcdef", "ok": true}))
            .is_err()
    );
    let mut wrong: Value = reply.clone();
    wrong["nonce"] = json!("n0123456789abcdeX");
    assert!(key.open_reply(&wrong).is_err());
}

/// RFC 7748 section 6.1: a peer key of low order (0, 1, the points of
/// order 8, and the same u past p) gives the all-zero secret, refused.
#[test]
fn a_peer_key_of_low_order_is_refused() {
    let secret = super::random::<32>().expect("random");
    for (what, point) in [
        (
            "u = 0",
            "0000000000000000000000000000000000000000000000000000000000000000",
        ),
        (
            "u = 1",
            "0100000000000000000000000000000000000000000000000000000000000000",
        ),
        (
            "order 8",
            "e0eb7a7c3b41b8ae1656e3faf19fc46ada098deb9c32b1fd866205165f49b800",
        ),
        (
            "order 8",
            "5f9c95bca3508c24b1d0b1559c83ef5b04445cc4581c8e86d8224eddd09f1157",
        ),
        (
            "p - 1",
            "ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
        ),
        (
            "p",
            "edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
        ),
        (
            "p + 1",
            "eeffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
        ),
    ] {
        assert!(super::agree(&secret, &unhex(point)).is_err(), "{what}");
    }
    // A key of full order agrees.
    assert!(super::agree(&secret, &x25519::public(&[9; 32])).is_ok());
    // Sealed to a node whose prekey is of low order: refused, nothing sent.
    let (me, _) = NodeIdentity::generate().expect("me");
    let (node, _) = NodeIdentity::generate().expect("node");
    let mut peer = PeerKeys::verify(&node.public_info(), None, None).expect("peer");
    peer.x25519 = unhex("0100000000000000000000000000000000000000000000000000000000000000");
    let policy = TrustPolicy::new(Some("t"), None, None);
    assert!(seal_request(&me, &peer, &policy, "c", &json!({})).is_err());
}
