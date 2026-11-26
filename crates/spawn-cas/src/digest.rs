//! Algorithm-prefixed content addresses: `sha256:<64 hex>` or
//! `blake3:<64 hex>`, lowercase. An unprefixed digest is refused
//! everywhere.

use std::fmt;
use std::io::{self, Read};
use std::str::FromStr;

use sha2::{Digest as _, Sha256};

use crate::CasError;

/// An algorithm a digest may name. Both parse; only SHA-256 can be
/// computed until the `blake3` crate lands (ST2, design F14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Algo {
    /// SHA-256, the v1 wire's algorithm.
    Sha256,
    /// BLAKE3: parsed, never verified in this build
    /// ([`CasError::AlgoUnavailable`]).
    Blake3,
}

impl Algo {
    /// Every algorithm, in the order of their directory names.
    pub const ALL: [Self; 2] = [Self::Blake3, Self::Sha256];

    /// The prefix and the directory name under `objects/`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Sha256 => "sha256",
            Self::Blake3 => "blake3",
        }
    }

    /// The algorithm of that exact (lowercase) name.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|algo| algo.name() == name)
    }

    /// A hasher of this algorithm, or [`CasError::AlgoUnavailable`].
    pub fn hasher(self) -> Result<Hasher, CasError> {
        match self {
            Self::Sha256 => Ok(Hasher(Sha256::new())),
            Self::Blake3 => Err(CasError::AlgoUnavailable(self)),
        }
    }
}

impl fmt::Display for Algo {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// An incremental hash of an available algorithm.
#[derive(Debug, Clone)]
pub struct Hasher(Sha256);

impl Hasher {
    /// Feed `bytes`.
    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    /// The digest of everything fed, in lowercase hex.
    pub fn finish_hex(self) -> String {
        to_hex(&self.0.finalize())
    }
}

/// Whether `text` is 64 lowercase hex digits.
pub fn is_hex64(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

/// An algorithm-prefixed content address.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest {
    algo: Algo,
    hex: String,
}

impl Digest {
    /// Exactly `sha256:<64 hex>` or `blake3:<64 hex>`, lowercase.
    pub fn parse(text: &str) -> Result<Self, CasError> {
        let bad = |reason| CasError::BadDigest {
            input: text.to_string(),
            reason,
        };
        let (algo, hex) = text
            .split_once(':')
            .ok_or_else(|| bad("no algorithm prefix"))?;
        let algo = Algo::from_name(algo).ok_or_else(|| bad("unknown algorithm"))?;
        if hex.len() != 64 {
            return Err(bad("want 64 hex digits"));
        }
        if !is_hex64(hex) {
            return Err(bad("not lowercase hex"));
        }
        Ok(Self {
            algo,
            hex: hex.to_string(),
        })
    }

    /// The digest of `algo` and `hex` (64 lowercase hex digits).
    pub fn from_parts(algo: Algo, hex: &str) -> Result<Self, CasError> {
        Self::parse(&format!("{algo}:{hex}"))
    }

    /// The algorithm.
    pub fn algo(&self) -> Algo {
        self.algo
    }

    /// The 64 lowercase hex digits.
    pub fn hex(&self) -> &str {
        &self.hex
    }

    /// The shard: the first two hex digits.
    pub fn shard(&self) -> &str {
        &self.hex[..2]
    }

    /// A hasher of the digest's algorithm, or
    /// [`CasError::AlgoUnavailable`].
    pub fn hasher(&self) -> Result<Hasher, CasError> {
        self.algo.hasher()
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.algo, self.hex)
    }
}

impl FromStr for Digest {
    type Err = CasError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

/// The SHA-256 digest of `bytes`.
pub fn sha256_of(bytes: &[u8]) -> Digest {
    Digest {
        algo: Algo::Sha256,
        hex: to_hex(&Sha256::digest(bytes)),
    }
}

/// The SHA-256 digest of everything `reader` yields.
pub fn sha256_reader(reader: &mut impl Read) -> io::Result<Digest> {
    let mut hasher = Hasher(Sha256::new());
    feed(&mut hasher, reader)?;
    Ok(Digest {
        algo: Algo::Sha256,
        hex: hasher.finish_hex(),
    })
}

fn feed(hasher: &mut Hasher, reader: &mut impl Read) -> io::Result<()> {
    let mut buffer = vec![0_u8; 256 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => hasher.update(&buffer[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

/// Proof that some bytes hashed to a digest ([`verify_reader`]): what
/// [`crate::Store::publish`] needs to name an object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified(Digest);

impl Verified {
    /// The digest the bytes hashed to.
    pub fn digest(&self) -> &Digest {
        &self.0
    }
}

/// Hash everything `reader` yields and compare it with `want`:
/// [`CasError::Mismatch`] when it differs, [`CasError::AlgoUnavailable`]
/// when `want`'s algorithm cannot be computed (unverifiable bytes are never
/// used).
pub fn verify_reader(reader: &mut impl Read, want: &Digest) -> Result<Verified, CasError> {
    let mut hasher = want.hasher()?;
    feed(&mut hasher, reader)?;
    let got = hasher.finish_hex();
    if got == want.hex {
        Ok(Verified(want.clone()))
    } else {
        Err(CasError::Mismatch {
            want: want.clone(),
            got: format!("{}:{got}", want.algo),
        })
    }
}
