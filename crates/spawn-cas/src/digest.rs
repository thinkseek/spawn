//! Algorithm-prefixed content addresses: `sha256:<64 hex>`, lowercase.
//! An unprefixed digest or any algorithm other than SHA-256 is refused
//! everywhere (D6 Phase 0 freeze).

use std::fmt;
use std::io::{self, Read};
use std::str::FromStr;

use sha2::{Digest as _, Sha256};

use crate::CasError;

/// The single frozen CAS content-address algorithm (`sha256`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Algo {
    /// Hardware-accelerated SHA-256 (`sha256:<64 hex>`).
    Sha256,
}

impl Algo {
    /// Every supported algorithm, in directory order.
    pub const ALL: [Self; 1] = [Self::Sha256];

    /// The prefix and the directory name under `objects/` and `chunk-index/`.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Sha256 => "sha256",
        }
    }

    /// The algorithm of that exact (lowercase) name.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|algo| algo.name() == name)
    }

    /// An incremental hasher for this algorithm.
    pub fn hasher(self) -> Hasher {
        match self {
            Self::Sha256 => Hasher(Sha256::new()),
        }
    }
}

impl fmt::Display for Algo {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// An incremental SHA-256 hasher.
#[derive(Debug, Clone)]
pub struct Hasher(Sha256);

impl Hasher {
    /// Feed `bytes`.
    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    /// The digest bytes of everything fed.
    pub fn finish_bytes(self) -> [u8; 32] {
        self.0.finalize().into()
    }

    /// The digest of everything fed, in lowercase hex.
    pub fn finish_hex(self) -> String {
        to_hex(&self.0.finalize())
    }

    /// Finalize into a [`Digest`].
    pub fn finish(self) -> Digest {
        Digest::from_raw_sha256(self.finish_bytes())
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

pub(crate) fn decode_hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

pub(crate) fn decode_hex_32(hex: &str) -> Option<[u8; 32]> {
    let bytes = hex.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0_u8; 32];
    for (index, pair) in bytes.chunks_exact(2).enumerate() {
        let high = decode_hex_nibble(pair[0])?;
        let low = decode_hex_nibble(pair[1])?;
        out[index] = (high << 4) | low;
    }
    Some(out)
}

/// An algorithm-prefixed content address (`sha256:<64 hex>`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest {
    algo: Algo,
    hex: String,
}

impl Digest {
    /// Exactly `sha256:<64 hex>`, lowercase.
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

    /// Construct a SHA-256 [`Digest`] from raw 32 bytes.
    pub fn from_raw_sha256(bytes: [u8; 32]) -> Self {
        Self {
            algo: Algo::Sha256,
            hex: to_hex(&bytes),
        }
    }

    /// Raw 32 digest bytes.
    pub fn as_bytes(&self) -> [u8; 32] {
        decode_hex_32(&self.hex).expect("Digest always stores 64 lowercase hex digits")
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

    /// An incremental hasher for this digest's algorithm.
    pub fn hasher(&self) -> Hasher {
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

impl serde::Serialize for Digest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Digest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

/// The SHA-256 digest of `bytes`.
pub fn sha256_of(bytes: &[u8]) -> Digest {
    Digest::from_raw_sha256(Sha256::digest(bytes).into())
}

/// The SHA-256 digest of everything `reader` yields.
pub fn sha256_reader(reader: &mut impl Read) -> io::Result<Digest> {
    let mut hasher = Hasher(Sha256::new());
    feed(&mut hasher, reader)?;
    Ok(hasher.finish())
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
/// [`CasError::Mismatch`] when it differs.
pub fn verify_reader(reader: &mut impl Read, want: &Digest) -> Result<Verified, CasError> {
    let mut hasher = want.hasher();
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
