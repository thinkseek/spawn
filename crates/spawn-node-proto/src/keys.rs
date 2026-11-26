//! Ed25519 identities (ring): a node's or the primary's key pair from its
//! 32-byte seed, a raw 32-byte public key, and the key id the headers carry
//! (`Spawn-Node-Id`, `Spawn-Primary-Key`): the lowercase hex SHA-256 of the
//! raw public key.

use ring::signature::{ED25519, Ed25519KeyPair, KeyPair as _, UnparsedPublicKey};

use crate::Error;
use crate::codec::{b64u, sha256, sha256_hex, unb64u};

/// The length of an Ed25519 signature.
pub const SIGNATURE_LEN: usize = 64;

/// A key pair that signs: the node's own (`state/node.key`) or, on the
/// gateway, the primary's.
pub struct SigningKey {
    pair: Ed25519KeyPair,
}

impl SigningKey {
    /// The key pair of a 32-byte RFC 8032 seed.
    pub fn from_seed(seed: &[u8; 32]) -> Result<Self, Error> {
        Ed25519KeyPair::from_seed_unchecked(seed)
            .map(|pair| Self { pair })
            .map_err(|_| Error::Key)
    }

    /// Returns the Ed25519 verifying public key for this key pair.
    pub fn public_key(&self) -> PublicKey {
        let mut raw = [0; 32];
        raw.copy_from_slice(self.pair.public_key().as_ref());
        PublicKey(raw)
    }

    /// The raw 64-byte signature of `message`.
    pub fn sign(&self, message: &[u8]) -> [u8; SIGNATURE_LEN] {
        let mut out = [0; SIGNATURE_LEN];
        out.copy_from_slice(self.pair.sign(message).as_ref());
        out
    }

    /// The base64url signature of `message`, as the headers carry it.
    pub fn sign_b64u(&self, message: &[u8]) -> String {
        b64u(&self.sign(message))
    }
}

impl std::fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SigningKey")
            .field("key_id", &self.public_key().key_id())
            .finish_non_exhaustive()
    }
}

/// A raw Ed25519 public key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PublicKey(pub [u8; 32]);

impl PublicKey {
    /// The key from its base64url spelling (`pubkey` of enroll,
    /// `primary_pubkey`).
    pub fn from_b64u(text: &str) -> Result<Self, Error> {
        let bytes = unb64u(text, 32).map_err(|_| Error::Key)?;
        let mut raw = [0; 32];
        raw.copy_from_slice(&bytes);
        Ok(Self(raw))
    }

    /// Encodes the raw public key as unpadded base64url.
    pub fn to_b64u(&self) -> String {
        b64u(&self.0)
    }

    /// The raw SHA-256 of the key; its first 16 bytes are an enroll
    /// token's pin.
    pub fn fingerprint(&self) -> [u8; 32] {
        sha256(&self.0)
    }

    /// The key id: lowercase hex SHA-256 of the raw key (a node's
    /// `node_id`, the primary's `Spawn-Primary-Key`).
    pub fn key_id(&self) -> String {
        sha256_hex(&self.0)
    }

    /// Verifies the raw signature `sig` of `message`.
    pub fn verify(&self, message: &[u8], sig: &[u8]) -> Result<(), Error> {
        if sig.len() != SIGNATURE_LEN {
            return Err(Error::Signature);
        }
        UnparsedPublicKey::new(&ED25519, &self.0)
            .verify(message, sig)
            .map_err(|_| Error::Signature)
    }

    /// Verifies a base64url signature; a signature that is not strict
    /// unpadded base64url of 64 bytes is a malformed header, not a wrong
    /// signature.
    pub fn verify_b64u(&self, message: &[u8], sig: &str) -> Result<(), Error> {
        let raw = decode_signature(sig)?;
        self.verify(message, &raw)
    }
}

/// The 64 bytes of a base64url signature header or field.
pub(crate) fn decode_signature(sig: &str) -> Result<Vec<u8>, Error> {
    unb64u(sig, SIGNATURE_LEN).map_err(|_| Error::Header("signature"))
}
