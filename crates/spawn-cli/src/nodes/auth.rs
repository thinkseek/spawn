//! The tcp and udp wire's authentication (issue #85): with
//! `SPAWN_NODE_TOKEN` set, every request carries `at_ms` (its sender's
//! wall clock) and `auth`, HMAC-SHA256 keyed by the token over a
//! domain-separated list of the request's fields ([`canonical`]): its op,
//! nonce and time always, and what the op acts on. The token itself never
//! goes on the wire. A request older or newer than [`MAX_SKEW_MS`] is
//! refused, and one sent again within it is answered from the relay's
//! cache (super::relay), so a captured request does nothing twice.

use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};

use crate::verify::now_ms;

/// What separates these MACs from every other use of the token.
const DOMAIN: &str = "spawn-node-auth-v1";
/// How far a request's `at_ms` may be from the receiver's clock.
pub(crate) const MAX_SKEW_MS: i64 = 120_000;

/// The fields a request's MAC covers, in order: `[domain, op, nonce,
/// at_ms, ...]`, then for `register` its `name, transport, endpoint, via,
/// mode, note, inventory`, for `sealed` its `salt, ct`, for `forward` its
/// `hops, ttl, timeout_ms` and
/// its inner request's `op` and `nonce`; `inventory` and `beat` nothing
/// more. None for another op.
fn canonical(message: &Value) -> Option<Vec<u8>> {
    let op = message["op"].as_str()?;
    let mut fields = vec![
        json!(DOMAIN),
        json!(op),
        message["nonce"].clone(),
        message["at_ms"].clone(),
    ];
    match op {
        "inventory" | "beat" => {}
        "register" => {
            for key in [
                "name",
                "transport",
                "endpoint",
                "via",
                "mode",
                "note",
                "inventory",
            ] {
                fields.push(message[key].clone());
            }
        }
        // Issue #87: a sealed request (super::crypto's wire seal), its
        // salt and ciphertext: checked before anything is opened.
        "sealed" => {
            for key in ["salt", "ct"] {
                fields.push(message[key].clone());
            }
        }
        "forward" => {
            for key in ["hops", "ttl", "timeout_ms"] {
                fields.push(message[key].clone());
            }
            fields.push(message["inner"]["op"].clone());
            fields.push(message["inner"]["nonce"].clone());
        }
        _ => return None,
    }
    serde_json::to_vec(&Value::Array(fields)).ok()
}

/// The MAC of `message` under `token`, in hex.
fn mac(token: &str, message: &Value) -> Option<String> {
    use std::fmt::Write as _;
    let bytes = canonical(message)?;
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(token.as_bytes()).ok()?;
    mac.update(&bytes);
    Some(
        mac.finalize()
            .into_bytes()
            .iter()
            .fold(String::new(), |mut hex, byte| {
                let _ = write!(hex, "{byte:02x}");
                hex
            }),
    )
}

/// Stamp `message` with the time and, with a token, its `auth`.
pub(crate) fn sign(message: &mut Value, token: Option<&str>) {
    if message.get("at_ms").is_none_or(Value::is_null) {
        message["at_ms"] = json!(now_ms());
    }
    if let Some(token) = token
        && let Some(auth) = mac(token, message)
    {
        message["auth"] = json!(auth);
    }
}

/// Whether `message` carries the MAC `token` gives it and was sent within
/// [`MAX_SKEW_MS`] of now; why not, in words that say nothing of the MAC.
pub(crate) fn verify(message: &Value, token: &str) -> Result<(), String> {
    let given = message["auth"].as_str().unwrap_or_default();
    let wanted = mac(token, message).unwrap_or_default();
    if wanted.is_empty() || !same(given, &wanted) {
        return Err("the request's auth is not this node's token's".to_string());
    }
    let at = message["at_ms"].as_i64().unwrap_or(i64::MIN);
    if now_ms().saturating_sub(at).abs() > MAX_SKEW_MS {
        return Err("the request's at_ms is too far from this node's clock".to_string());
    }
    Ok(())
}

/// Whether two texts are the same, in a time that does not say where they
/// differ.
pub(crate) fn same(given: &str, wanted: &str) -> bool {
    given.len() == wanted.len()
        && given
            .bytes()
            .zip(wanted.bytes())
            .fold(0_u8, |differ, (a, b)| differ | (a ^ b))
            == 0
}

#[cfg(test)]
mod tests {
    use super::{MAX_SKEW_MS, sign, verify};
    use crate::verify::now_ms;
    use serde_json::json;

    #[test]
    fn a_signed_request_verifies_and_any_change_to_what_it_covers_does_not() {
        let mut register = json!({"op": "register", "nonce": "n0123456789abcdef", "name": "edge",
                                  "transport": "udp", "endpoint": "udp://e:1", "via": ["gw"],
                                  "mode": "relay", "note": null, "inventory": "@end\n"});
        sign(&mut register, Some("k3y"));
        let auth = register["auth"].as_str().expect("an auth").to_string();
        assert_eq!(auth.len(), 64);
        assert!(
            !register.to_string().contains("k3y"),
            "the token never travels"
        );
        assert_eq!(verify(&register, "k3y"), Ok(()));
        assert!(verify(&register, "other").is_err());
        for (key, value) in [
            ("name", json!("evil")),
            ("endpoint", json!("udp://evil:1")),
            ("via", json!([])),
            ("mode", json!("direct")),
            ("note", json!("x")),
            ("inventory", json!("@x\n@end\n")),
            ("nonce", json!("n0123456789abcdeX")),
            ("op", json!("beat")),
            ("auth", json!("00")),
        ] {
            let mut tampered = register.clone();
            tampered[key] = value;
            assert!(verify(&tampered, "k3y").is_err(), "{key} tampered");
        }
        let mut stale = json!({"op": "beat", "nonce": "n0123456789abcdef",
                               "at_ms": now_ms() - MAX_SKEW_MS - 1000});
        sign(&mut stale, Some("k3y"));
        assert!(verify(&stale, "k3y").unwrap_err().contains("at_ms"));
        let mut forward = json!({"op": "forward", "nonce": "n0123456789abcdef", "ttl": 1,
                                 "hops": ["tcp://h:1"], "timeout_ms": 1000,
                                 "inner": {"op": "beat", "nonce": "m0123456789abcdef"}});
        sign(&mut forward, Some("k3y"));
        assert_eq!(verify(&forward, "k3y"), Ok(()));
        forward["hops"] = json!(["tcp://evil:1"]);
        assert!(verify(&forward, "k3y").is_err());
        let mut unsigned = json!({"op": "beat", "nonce": "n0123456789abcdef"});
        sign(&mut unsigned, None);
        assert!(unsigned.get("auth").is_none() && unsigned["at_ms"].is_i64());
        assert!(verify(&unsigned, "k3y").is_err());
    }
}
