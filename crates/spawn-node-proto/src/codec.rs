//! Wire scalar encodings (`base64url` here; pure hex/SHA-256/decimals in
//! `spawn_core::node_proto::codec`).

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

pub use spawn_core::node_proto::codec::{
    EMPTY_SHA256, MAX_BUILD_SHA_LEN, MAX_NODE_VERSION_LEN, MAX_SAFE, MIN_BUILD_SHA_LEN, hex,
    is_build_sha, is_lower_hex, is_node_version, parse_decimal, sha256, sha256_hex, unhex,
};

use crate::Error;

/// Base64url without padding.
pub fn b64u(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Strict base64url without padding: only `A-Z a-z 0-9 - _`, no `=`, the
/// canonical spelling (re-encoding gives the same text), and exactly `len`
/// decoded bytes.
pub fn unb64u(text: &str, len: usize) -> Result<Vec<u8>, Error> {
    if !text
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(Error::Base64);
    }
    let bytes = URL_SAFE_NO_PAD.decode(text).map_err(|_| Error::Base64)?;
    if bytes.len() != len || URL_SAFE_NO_PAD.encode(&bytes) != text {
        return Err(Error::Base64);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_is_strict_canonical_and_unpadded() {
        let text = b64u(&[0xff; 32]);
        assert_eq!(unb64u(&text, 32).unwrap(), vec![0xff; 32]);
        assert!(unb64u(&format!("{text}="), 32).is_err());
        assert!(unb64u(&text.replace('_', "/"), 32).is_err());
        assert!(unb64u(&text, 31).is_err());
        // Non-zero trailing bits: decodes leniently elsewhere, not here.
        assert!(unb64u("AB", 1).is_err());
        assert_eq!(unb64u("AA", 1).unwrap(), vec![0]);
    }
}
