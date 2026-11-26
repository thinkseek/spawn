//! Enrollment and key pinning (section 2.5). The owner's token is
//! `sne1.<base64url of 32 random bytes>.<base64url of the first 16 bytes of
//! SHA-256(primary public key)>`; the store keeps only the SHA-256 of the
//! whole token string. The daemon accepts the `primary_pubkey` of the
//! enroll answer only if its SHA-256 starts with the token's 16 bytes.

use serde::{Deserialize, Serialize};

use crate::codec::{b64u, sha256_hex, unb64u};
use crate::keys::PublicKey;
use crate::spec::NodeSpec;
use crate::{Error, VERSION};

/// The token's first part.
pub const TOKEN_PREFIX: &str = "sne1";
/// How many bytes of the primary key's SHA-256 a token pins.
pub const PIN_LEN: usize = 16;

/// An enroll token.
#[derive(Clone, PartialEq, Eq)]
pub struct EnrollToken {
    text: String,
    secret: [u8; 32],
    pin: [u8; PIN_LEN],
}

impl EnrollToken {
    /// The token of a 32-byte secret, pinning `primary`.
    pub fn new(secret: [u8; 32], primary: &PublicKey) -> Self {
        let mut pin = [0; PIN_LEN];
        pin.copy_from_slice(&primary.fingerprint()[..PIN_LEN]);
        Self {
            text: format!("{TOKEN_PREFIX}.{}.{}", b64u(&secret), b64u(&pin)),
            secret,
            pin,
        }
    }

    /// A token line, exactly (no whitespace).
    pub fn parse(text: &str) -> Result<Self, Error> {
        let mut parts = text.split('.');
        let (Some(TOKEN_PREFIX), Some(secret), Some(pin), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(Error::Token);
        };
        let secret = unb64u(secret, 32).map_err(|_| Error::Token)?;
        let pin = unb64u(pin, PIN_LEN).map_err(|_| Error::Token)?;
        Ok(Self {
            text: text.to_string(),
            secret: secret.try_into().map_err(|_| Error::Token)?,
            pin: pin.try_into().map_err(|_| Error::Token)?,
        })
    }

    /// A token file's contents: one line, with or without its final `\n`.
    pub fn from_file_contents(contents: &str) -> Result<Self, Error> {
        Self::parse(contents.strip_suffix('\n').unwrap_or(contents))
    }

    /// Returns the full serialized token string (`sne1.<secret>.<pin>`).
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Returns the 32-byte raw enrollment secret.
    pub fn secret(&self) -> &[u8; 32] {
        &self.secret
    }

    /// Returns the 16-byte primary key fingerprint pin.
    pub fn pin(&self) -> &[u8; PIN_LEN] {
        &self.pin
    }

    /// What the store keeps: the lowercase hex SHA-256 of the token string.
    pub fn sha256(&self) -> String {
        sha256_hex(self.text.as_bytes())
    }

    /// True when `primary`'s SHA-256 starts with the token's pin.
    pub fn pins(&self, primary: &PublicKey) -> bool {
        primary.fingerprint()[..PIN_LEN] == self.pin
    }

    /// The primary key of an enroll answer, accepted only if the token
    /// pins it.
    pub fn accept_primary(&self, primary_pubkey_b64u: &str) -> Result<PublicKey, Error> {
        let key = PublicKey::from_b64u(primary_pubkey_b64u)?;
        if self.pins(&key) {
            Ok(key)
        } else {
            Err(Error::PinMismatch)
        }
    }
}

impl std::fmt::Debug for EnrollToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the secret.
        f.debug_struct("EnrollToken")
            .field("sha256", &self.sha256())
            .finish_non_exhaustive()
    }
}

/// `POST /node/v1/enroll`'s body, signed by the new key itself with seq 0.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollRequest {
    /// Protocol version (must equal [`VERSION`]).
    pub v: u64,
    /// Raw serialized enrollment token string (`sne1.<secret>.<pin>`).
    pub token: String,
    /// Base64url-encoded 32-byte Ed25519 public key of the enrolling node.
    pub pubkey: String,
    /// Node specification advertised during enrollment.
    pub spec: NodeSpec,
}

impl EnrollRequest {
    /// The new node's key, which must also sign the request (proof of
    /// possession).
    pub fn pubkey(&self) -> Result<PublicKey, Error> {
        PublicKey::from_b64u(&self.pubkey)
    }

    /// Parses and validates the request's enrollment token.
    pub fn token(&self) -> Result<EnrollToken, Error> {
        EnrollToken::parse(&self.token)
    }

    /// Validates the version, node public key, enrollment token, and spec.
    pub fn validate(&self) -> Result<(), Error> {
        if self.v != VERSION {
            return Err(Error::Body("v"));
        }
        self.pubkey()?;
        self.token()?;
        self.spec.validate()
    }
}

/// The part of the enroll answer the daemon pins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrollAnswer {
    /// Protocol version (`1`).
    pub v: u64,
    /// Primary gateway clock timestamp in Unix milliseconds.
    pub server_time_ms: u64,
    /// Base64url-encoded 32-byte Ed25519 verifying key of the primary gateway.
    pub primary_pubkey: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SigningKey;

    #[test]
    fn a_token_pins_only_its_primary_and_never_prints_its_secret() {
        let primary = SigningKey::from_seed(&[1; 32]).unwrap().public_key();
        let other = SigningKey::from_seed(&[2; 32]).unwrap().public_key();
        let token = EnrollToken::new([9; 32], &primary);
        let parsed = EnrollToken::parse(token.as_str()).unwrap();
        assert_eq!(parsed, token);
        assert!(parsed.pins(&primary));
        assert!(!parsed.pins(&other));
        assert_eq!(parsed.accept_primary(&primary.to_b64u()).unwrap(), primary);
        assert_eq!(
            parsed.accept_primary(&other.to_b64u()),
            Err(Error::PinMismatch)
        );
        assert!(!format!("{parsed:?}").contains(&b64u(&[9; 32])));
        for bad in [
            "",
            "sne1",
            "sne2.AAAA.AAAA",
            &format!("{} ", token.as_str()),
            &format!("{}.x", token.as_str()),
            &token.as_str().replace("sne1.", "sne1.="),
        ] {
            assert_eq!(EnrollToken::parse(bad), Err(Error::Token), "{bad:?}");
        }
    }
}
