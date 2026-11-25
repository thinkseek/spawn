//! Content addresses (`sha256:<hex>`; `blake3:<hex>` is parsed but has no
//! verifier in v1) and the artifact GET's target. Artifact bodies are not
//! signed: the bytes are believed only if they hash to the digest.

use std::fmt;

use crate::codec::{is_lower_hex, sha256_hex};
use crate::envelope::PATH_PREFIX;
use crate::Error;

/// A digest's algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Algo {
    Sha256,
    Blake3,
}

impl Algo {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sha256 => "sha256",
            Self::Blake3 => "blake3",
        }
    }
}

/// `<algo>:<64 lowercase hex>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContentDigest {
    pub algo: Algo,
    pub hex: String,
}

impl ContentDigest {
    pub fn parse(text: &str) -> Result<Self, Error> {
        let (algo, hex) = text.split_once(':').ok_or(Error::Digest)?;
        let algo = match algo {
            "sha256" => Algo::Sha256,
            "blake3" => Algo::Blake3,
            _ => return Err(Error::Digest),
        };
        if !is_lower_hex(hex, 64) {
            return Err(Error::Digest);
        }
        Ok(Self {
            algo,
            hex: hex.to_string(),
        })
    }

    /// The `sha256:` digest of `bytes`.
    pub fn sha256_of(bytes: &[u8]) -> Self {
        Self {
            algo: Algo::Sha256,
            hex: sha256_hex(bytes),
        }
    }

    /// The gateway-relative URL of the artifact (`/node/v1/artifacts/…`).
    pub fn artifact_path(&self) -> String {
        format!("{PATH_PREFIX}artifacts/{self}")
    }

    /// Checks fetched bytes against the digest.
    pub fn verify(&self, bytes: &[u8]) -> Result<(), Error> {
        match self.algo {
            Algo::Sha256 if sha256_hex(bytes) == self.hex => Ok(()),
            Algo::Sha256 => Err(Error::DigestMismatch),
            // v1 emits sha256 only and has no blake3 crate.
            Algo::Blake3 => Err(Error::Digest),
        }
    }
}

impl fmt::Display for ContentDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.algo.as_str(), self.hex)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests_parse_strictly_and_verify_sha256() {
        let digest = ContentDigest::sha256_of(b"abc");
        let text = digest.to_string();
        assert_eq!(ContentDigest::parse(&text).unwrap(), digest);
        digest.verify(b"abc").unwrap();
        assert_eq!(digest.verify(b"abd"), Err(Error::DigestMismatch));
        let blake = ContentDigest::parse(&format!("blake3:{}", digest.hex)).unwrap();
        assert_eq!(blake.algo, Algo::Blake3);
        assert_eq!(blake.verify(b"abc"), Err(Error::Digest));
        for bad in [
            "sha256",
            "md5:00",
            &format!("sha256:{}", digest.hex.to_uppercase()),
            &format!("sha256:{}", &digest.hex[1..]),
            &format!("SHA256:{}", digest.hex),
        ] {
            assert_eq!(ContentDigest::parse(bad), Err(Error::Digest), "{bad}");
        }
    }
}
