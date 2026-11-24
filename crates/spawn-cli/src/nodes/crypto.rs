//! The node mesh's cryptography (issue #87): who a node is, whom it
//! trusts, and the two seals its messages travel under.
//!
//! - [`NodeIdentity`]: an Ed25519 key pair (PKCS#8 v2), its `node_id`
//!   (SHA-256 of the 32-byte public key, lowercase hex) and a rotating
//!   X25519 prekey signed by it (`prekey_sig` over `spawn-prekey-v1 ||
//!   node_id || x25519_pub || epoch_be64`). Loaded from `--key-file` or
//!   `SPAWN_NODE_KEY_FILE` (made, mode 0600, when missing), else from
//!   `SPAWN_NODE_KEY` (base64 PKCS#8), else made for this run only.
//! - [`TrustPolicy`]: the pre-shared token (`SPAWN_NODE_TOKEN`, and the
//!   one it replaces, `SPAWN_NODE_TOKEN_PREV`, still taken while a fleet
//!   rotates: sealed with the first, opened with either) and the revoked
//!   node ids (`SPAWN_NODE_REVOKED`).
//! - `spawn-e2e-v1` ([`seal_request`], [`Responder`]): a request sealed
//!   to one node's prekey through any number of blind relays (a
//!   coordinator, a tree of them, a Cloudflare Durable Object). A fresh
//!   ephemeral X25519 key per request; HKDF-SHA256 over the shared secret
//!   (and the token when there is one) gives the request's and the reply's
//!   ChaCha20-Poly1305 keys, bound to both node ids, the prekey's epoch,
//!   the channel and the time. The header is signed by the sender's
//!   Ed25519 key and is the AEAD's associated data: a relay sees who
//!   talks to whom, never what (no op, inventory, token or argument).
//! - The tcp and udp wire's seal (`op: "sealed"`, [`seal_wire`],
//!   [`open_wire`]): with a token, each request (and a forward's inner
//!   request) is ChaCha20-Poly1305-sealed under a key HKDF derives from
//!   the token, a random salt, the nonce and the time; its reply comes
//!   back sealed under a key of its own (`op: "sealed_reply"`).
//!
//! X25519 is computed here ([`x25519`], RFC 7748's ladder over 5 limbs
//! of 51 bits, no secret-dependent branch or index): `ring` offers only
//! single-use ephemeral keys, and a prekey answers many requests.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use base64::Engine as _;
use ring::rand::SecureRandom as _;
use ring::signature::KeyPair as _;
use ring::{aead, digest, hkdf, rand, signature};
use serde_json::{Value, json};
use zeroize::Zeroizing;

use super::auth::MAX_SKEW_MS;
use crate::verify::now_ms;

/// The sealed envelope's domain, and its HKDF's.
pub(crate) const E2E_DOMAIN: &str = "spawn-e2e-v1";
/// A sealed reply's domain.
pub(crate) const E2E_REPLY_DOMAIN: &str = "spawn-e2e-v1-reply";
/// What a prekey's signature starts with.
const PREKEY_DOMAIN: &[u8] = b"spawn-prekey-v1";
/// The wire seal's domain.
const WIRE_DOMAIN: &str = "spawn-wire-v1";
/// The most envelopes remembered against a replay.
const MAX_REPLAYS: usize = 8192;
/// The longest sealed payload (a probe's output and its frame fit).
pub(crate) const MAX_SEALED: usize = 400 * 1024;

/// Bytes as standard base64.
pub(crate) fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Base64, standard or URL-safe, padded or not.
pub(crate) fn unb64(text: &str) -> Result<Vec<u8>, String> {
    let normal: String = text
        .trim()
        .trim_end_matches('=')
        .chars()
        .map(|c| match c {
            '-' => '+',
            '_' => '/',
            c => c,
        })
        .collect();
    base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(normal.as_bytes())
        .map_err(|_| "not base64".to_string())
}

/// Bytes as lowercase hex.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// A field of `value` as exactly `N` bytes of base64.
fn bytes_of<const N: usize>(value: &Value, what: &str) -> Result<[u8; N], String> {
    value
        .as_str()
        .ok_or_else(|| format!("no {what}"))
        .and_then(unb64)
        .and_then(|bytes| {
            <[u8; N]>::try_from(bytes.as_slice()).map_err(|_| format!("{what} is not {N} bytes"))
        })
        .map_err(|_| format!("a bad {what}"))
}

/// `N` bytes of the system's random device.
pub(crate) fn random<const N: usize>() -> Result<[u8; N], String> {
    let mut bytes = [0_u8; N];
    rand::SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "the system's random device failed".to_string())?;
    Ok(bytes)
}

/// SHA-256 of `bytes`.
pub(crate) fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut out = [0_u8; 32];
    out.copy_from_slice(digest::digest(&digest::SHA256, bytes).as_ref());
    out
}

/// A node's id: SHA-256 of its raw Ed25519 public key, in hex.
pub(crate) fn node_id_of(pubkey: &[u8]) -> String {
    hex(&sha256(pubkey))
}

/// Whether `text` is a node id: 64 lowercase hex digits.
pub(crate) fn valid_node_id(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Whether two byte strings are equal, in a time that does not say where
/// they differ.
pub(crate) fn same_bytes(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0_u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// X25519 (RFC 7748) over GF(2^255 - 19), five 51-bit limbs (the RFC's
/// names: x2, z2, x3, z3, aa, bb, da, cb).
#[allow(clippy::similar_names, clippy::many_single_char_names)]
pub(crate) mod x25519 {
    const MASK: u64 = (1 << 51) - 1;

    #[derive(Clone, Copy)]
    struct Fe([u64; 5]);

    fn word(bytes: &[u8; 32], at: usize) -> u64 {
        let mut word = [0_u8; 8];
        word.copy_from_slice(&bytes[at..at + 8]);
        u64::from_le_bytes(word)
    }

    impl Fe {
        const ZERO: Self = Self([0; 5]);
        const ONE: Self = Self([1, 0, 0, 0, 0]);

        fn load(bytes: &[u8; 32]) -> Self {
            let w = [
                word(bytes, 0),
                word(bytes, 8),
                word(bytes, 16),
                word(bytes, 24),
            ];
            Self([
                w[0] & MASK,
                ((w[0] >> 51) | (w[1] << 13)) & MASK,
                ((w[1] >> 38) | (w[2] << 26)) & MASK,
                ((w[2] >> 25) | (w[3] << 39)) & MASK,
                (w[3] >> 12) & MASK,
            ])
        }

        /// Limbs back under 2^51 (the top carry folded in times 19).
        fn carry(mut self) -> Self {
            let l = &mut self.0;
            for _ in 0..2 {
                let mut c;
                c = l[0] >> 51;
                l[0] &= MASK;
                l[1] += c;
                c = l[1] >> 51;
                l[1] &= MASK;
                l[2] += c;
                c = l[2] >> 51;
                l[2] &= MASK;
                l[3] += c;
                c = l[3] >> 51;
                l[3] &= MASK;
                l[4] += c;
                c = l[4] >> 51;
                l[4] &= MASK;
                l[0] += c * 19;
            }
            self
        }

        fn store(self) -> [u8; 32] {
            let mut l = self.carry().0;
            // q is 1 when the value is p or more, which takes p off.
            let mut q = (l[0] + 19) >> 51;
            q = (l[1] + q) >> 51;
            q = (l[2] + q) >> 51;
            q = (l[3] + q) >> 51;
            q = (l[4] + q) >> 51;
            l[0] += 19 * q;
            l[1] += l[0] >> 51;
            l[0] &= MASK;
            l[2] += l[1] >> 51;
            l[1] &= MASK;
            l[3] += l[2] >> 51;
            l[2] &= MASK;
            l[4] += l[3] >> 51;
            l[3] &= MASK;
            l[4] &= MASK;
            let words = [
                l[0] | (l[1] << 51),
                (l[1] >> 13) | (l[2] << 38),
                (l[2] >> 26) | (l[3] << 25),
                (l[3] >> 39) | (l[4] << 12),
            ];
            let mut out = [0_u8; 32];
            for (index, word) in words.iter().enumerate() {
                out[index * 8..index * 8 + 8].copy_from_slice(&word.to_le_bytes());
            }
            out
        }

        fn add(self, other: Self) -> Self {
            let mut out = self.0;
            for (limb, add) in out.iter_mut().zip(other.0) {
                *limb += add;
            }
            Self(out).carry()
        }

        fn sub(self, other: Self) -> Self {
            // Plus 4p, so no limb goes below zero.
            const FOUR_P: [u64; 5] = [
                0x1F_FFFF_FFFF_FFB4,
                0x1F_FFFF_FFFF_FFFC,
                0x1F_FFFF_FFFF_FFFC,
                0x1F_FFFF_FFFF_FFFC,
                0x1F_FFFF_FFFF_FFFC,
            ];
            let mut out = [0_u64; 5];
            for index in 0..5 {
                out[index] = self.0[index] + FOUR_P[index] - other.0[index];
            }
            Self(out).carry()
        }

        fn mul(self, other: Self) -> Self {
            let a = self.0.map(u128::from);
            let b = other.0.map(u128::from);
            let b19 = b.map(|limb| limb * 19);
            let r = [
                a[0] * b[0] + a[1] * b19[4] + a[2] * b19[3] + a[3] * b19[2] + a[4] * b19[1],
                a[0] * b[1] + a[1] * b[0] + a[2] * b19[4] + a[3] * b19[3] + a[4] * b19[2],
                a[0] * b[2] + a[1] * b[1] + a[2] * b[0] + a[3] * b19[4] + a[4] * b19[3],
                a[0] * b[3] + a[1] * b[2] + a[2] * b[1] + a[3] * b[0] + a[4] * b19[4],
                a[0] * b[4] + a[1] * b[3] + a[2] * b[2] + a[3] * b[1] + a[4] * b[0],
            ];
            Self::reduce_wide(r)
        }

        fn reduce_wide(mut r: [u128; 5]) -> Self {
            let mask = u128::from(MASK);
            for index in 0..4 {
                r[index + 1] += r[index] >> 51;
                r[index] &= mask;
            }
            let top = r[4] >> 51;
            r[4] &= mask;
            r[0] += top * 19;
            r[1] += r[0] >> 51;
            r[0] &= mask;
            // Every limb is under 2^52 now.
            Self(r.map(|wide| u64::try_from(wide).unwrap_or(u64::MAX))).carry()
        }

        fn square(self) -> Self {
            self.mul(self)
        }

        fn square_times(self, times: u32) -> Self {
            (0..times).fold(self, |fe, _| fe.square())
        }

        fn mul_small(self, small: u64) -> Self {
            let wide = self.0.map(|limb| u128::from(limb) * u128::from(small));
            Self::reduce_wide(wide)
        }

        /// self^(p-2): the inverse (0 for 0).
        fn invert(self) -> Self {
            let z2 = self.square();
            let z9 = z2.square_times(2).mul(self);
            let z11 = z9.mul(z2);
            let z2_5_0 = z11.square().mul(z9);
            let z2_10_0 = z2_5_0.square_times(5).mul(z2_5_0);
            let z2_20_0 = z2_10_0.square_times(10).mul(z2_10_0);
            let z2_40_0 = z2_20_0.square_times(20).mul(z2_20_0);
            let z2_50_0 = z2_40_0.square_times(10).mul(z2_10_0);
            let z2_100_0 = z2_50_0.square_times(50).mul(z2_50_0);
            let z2_200_0 = z2_100_0.square_times(100).mul(z2_100_0);
            let z2_250_0 = z2_200_0.square_times(50).mul(z2_50_0);
            z2_250_0.square_times(5).mul(z11)
        }

        /// Swap `a` and `b` when `swap` is 1, with no branch on it.
        fn swap(swap: u64, a: &mut Self, b: &mut Self) {
            let mask = swap.wrapping_neg();
            for index in 0..5 {
                let differ = mask & (a.0[index] ^ b.0[index]);
                a.0[index] ^= differ;
                b.0[index] ^= differ;
            }
        }
    }

    /// The public key of `scalar`: `scalar` times the base point 9.
    pub(crate) fn public(scalar: &[u8; 32]) -> [u8; 32] {
        let mut nine = [0_u8; 32];
        nine[0] = 9;
        scalar_mult(scalar, &nine)
    }

    /// RFC 7748 section 5's `X25519(k, u)`.
    /// The clamped scalar and the ladder's state are wiped when it ends
    /// (`zeroize`): no copy of a secret is left in them. Temporaries the
    /// compiler keeps in registers or on the stack are out of its reach.
    pub(crate) fn scalar_mult(scalar: &[u8; 32], point: &[u8; 32]) -> [u8; 32] {
        use zeroize::Zeroize as _;
        let mut k = zeroize::Zeroizing::new(*scalar);
        k[0] &= 0xF8;
        k[31] &= 0x7F;
        k[31] |= 0x40;
        let x1 = Fe::load(point);
        let (mut x2, mut z2, mut x3, mut z3) = (Fe::ONE, Fe::ZERO, x1, Fe::ONE);
        let mut swap = 0_u64;
        for bit in (0..255).rev() {
            let k_t = u64::from((k[bit / 8] >> (bit % 8)) & 1);
            swap ^= k_t;
            Fe::swap(swap, &mut x2, &mut x3);
            Fe::swap(swap, &mut z2, &mut z3);
            swap = k_t;
            let a = x2.add(z2);
            let aa = a.square();
            let b = x2.sub(z2);
            let bb = b.square();
            let e = aa.sub(bb);
            let c = x3.add(z3);
            let d = x3.sub(z3);
            let da = d.mul(a);
            let cb = c.mul(b);
            x3 = da.add(cb).square();
            z3 = x1.mul(da.sub(cb).square());
            x2 = aa.mul(bb);
            z2 = e.mul(aa.add(e.mul_small(121_665)));
        }
        Fe::swap(swap, &mut x2, &mut x3);
        Fe::swap(swap, &mut z2, &mut z3);
        let out = x2.mul(z2.invert()).store();
        for fe in [&mut x2, &mut z2, &mut x3, &mut z3] {
            fe.0.zeroize();
        }
        swap.zeroize();
        out
    }
}

/// The shared secret of `secret` and `public`, refused when it is all
/// zeros (a low-order point: no secret at all).
fn agree(secret: &[u8; 32], public: &[u8; 32]) -> Result<Zeroizing<[u8; 32]>, String> {
    let shared = Zeroizing::new(x25519::scalar_mult(secret, public));
    if same_bytes(shared.as_ref(), &[0_u8; 32]) {
        return Err("a peer key of low order".to_string());
    }
    Ok(shared)
}

/// HKDF-SHA256's output length, for `ring`.
struct Len(usize);

impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

/// Two 32-byte keys from HKDF-SHA256 (`salt`, `ikm`), one per `info`.
fn derive(salt: &[u8], ikm: &[u8], infos: [&[u8]; 2]) -> Result<[Zeroizing<[u8; 32]>; 2], String> {
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(ikm);
    let one = |info: &[u8]| -> Result<Zeroizing<[u8; 32]>, String> {
        let mut key = Zeroizing::new([0_u8; 32]);
        prk.expand(&[info], Len(32))
            .and_then(|okm| okm.fill(key.as_mut()))
            .map_err(|_| "the key derivation failed".to_string())?;
        Ok(key)
    };
    Ok([one(infos[0])?, one(infos[1])?])
}

/// ChaCha20-Poly1305 of `plain` under `key` and `nonce`, `aad` bound.
fn seal_aead(key: &[u8; 32], nonce: [u8; 12], aad: &[u8], plain: &[u8]) -> Result<Vec<u8>, String> {
    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::CHACHA20_POLY1305, key).map_err(|_| "a bad key")?,
    );
    let mut sealed = plain.to_vec();
    key.seal_in_place_append_tag(
        aead::Nonce::assume_unique_for_key(nonce),
        aead::Aad::from(aad),
        &mut sealed,
    )
    .map_err(|_| "the seal failed".to_string())?;
    Ok(sealed)
}

/// The plaintext of `sealed`, or none when it, its key, nonce or `aad`
/// is not the one it was sealed with.
fn open_aead(
    key: &[u8; 32],
    nonce: [u8; 12],
    aad: &[u8],
    sealed: &[u8],
) -> Option<Zeroizing<Vec<u8>>> {
    let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::CHACHA20_POLY1305, key).ok()?);
    let mut buffer = Zeroizing::new(sealed.to_vec());
    let plain = key
        .open_in_place(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(aad),
            buffer.as_mut_slice(),
        )
        .ok()?
        .len();
    buffer.truncate(plain);
    Some(buffer)
}

/// A JSON list as the bytes a signature or an AEAD binds.
fn canonical(fields: &[Value]) -> Vec<u8> {
    serde_json::to_vec(fields).unwrap_or_default()
}

/// One X25519 prekey of a node, and its signed bundle.
struct Prekey {
    secret: Zeroizing<[u8; 32]>,
    public: [u8; 32],
    epoch: u64,
    sig: Vec<u8>,
}

/// What a node publishes of its current prekey.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PrekeyBundle {
    pub(crate) x25519_pub: String,
    pub(crate) epoch: u64,
    pub(crate) prekey_sig: String,
}

/// What a prekey's signature covers.
fn prekey_message(node_id: &str, x25519_pub: &[u8; 32], epoch: u64) -> Vec<u8> {
    let mut message = PREKEY_DOMAIN.to_vec();
    message.extend_from_slice(node_id.as_bytes());
    message.extend_from_slice(x25519_pub);
    message.extend_from_slice(&epoch.to_be_bytes());
    message
}

/// Where a node's identity came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeySource {
    /// A key file, read or made now.
    File(PathBuf, bool),
    /// `SPAWN_NODE_KEY`.
    Env,
    /// Made for this run, kept nowhere.
    Ephemeral,
}

/// A node's identity: its Ed25519 key pair, its id, its prekeys (the
/// current one last, the one before it still opened).
pub(crate) struct NodeIdentity {
    pair: signature::Ed25519KeyPair,
    node_id: String,
    prekeys: Mutex<Vec<Prekey>>,
}

impl std::fmt::Debug for NodeIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeIdentity")
            .field("node_id", &self.node_id)
            .finish_non_exhaustive()
    }
}

impl NodeIdentity {
    /// The identity of a PKCS#8 Ed25519 key (v2, or v1 without its public
    /// key), with a first prekey.
    pub(crate) fn from_pkcs8(pkcs8: &[u8]) -> Result<Self, String> {
        let pair = signature::Ed25519KeyPair::from_pkcs8(pkcs8)
            .or_else(|_| signature::Ed25519KeyPair::from_pkcs8_maybe_unchecked(pkcs8))
            .map_err(|_| "not a PKCS#8 Ed25519 key".to_string())?;
        let node_id = node_id_of(pair.public_key().as_ref());
        let identity = Self {
            pair,
            node_id,
            prekeys: Mutex::new(Vec::new()),
        };
        identity.rotate()?;
        Ok(identity)
    }

    /// A new key, and its PKCS#8 v2 document (to keep, or not).
    pub(crate) fn generate() -> Result<(Self, Zeroizing<Vec<u8>>), String> {
        let document = signature::Ed25519KeyPair::generate_pkcs8(&rand::SystemRandom::new())
            .map_err(|_| "no key could be made".to_string())?;
        let pkcs8 = Zeroizing::new(document.as_ref().to_vec());
        Ok((Self::from_pkcs8(&pkcs8)?, pkcs8))
    }

    /// The identity `--key-file` (`key_file`), else `SPAWN_NODE_KEY_FILE`,
    /// else `SPAWN_NODE_KEY`, else one made for this run, says.
    pub(crate) fn load(key_file: Option<&Path>) -> Result<(Self, KeySource), String> {
        let file = key_file
            .map(Path::to_path_buf)
            .or_else(|| crate::config::env_value("SPAWN_NODE_KEY_FILE").map(PathBuf::from));
        let key = crate::config::env_value("SPAWN_NODE_KEY").map(Zeroizing::new);
        Self::load_from(file.as_deref(), key.as_ref().map(|key| key.as_str()))
    }

    /// As [`Self::load`], from what it read: a key file (read, or made
    /// with mode 0600 in directories made 0700 when missing), else a
    /// base64 PKCS#8 key, else a key for this run.
    pub(crate) fn load_from(
        file: Option<&Path>,
        key_b64: Option<&str>,
    ) -> Result<(Self, KeySource), String> {
        if let Some(file) = file {
            return match std::fs::read_to_string(file) {
                Ok(text) => {
                    let text = Zeroizing::new(text);
                    let pkcs8 = Zeroizing::new(
                        unb64(&text)
                            .map_err(|_| format!("{}: not a base64 key", file.display()))?,
                    );
                    let identity = Self::from_pkcs8(&pkcs8)
                        .map_err(|error| format!("{}: {error}", file.display()))?;
                    Ok((identity, KeySource::File(file.to_path_buf(), false)))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    let (identity, pkcs8) = Self::generate()?;
                    write_key_file(file, &pkcs8)?;
                    Ok((identity, KeySource::File(file.to_path_buf(), true)))
                }
                Err(error) => Err(format!("{}: {error}", file.display())),
            };
        }
        if let Some(key) = key_b64 {
            let pkcs8 = Zeroizing::new(unb64(key).map_err(|_| "SPAWN_NODE_KEY: not base64")?);
            let identity =
                Self::from_pkcs8(&pkcs8).map_err(|error| format!("SPAWN_NODE_KEY: {error}"))?;
            return Ok((identity, KeySource::Env));
        }
        Ok((Self::generate()?.0, KeySource::Ephemeral))
    }

    pub(crate) fn node_id(&self) -> &str {
        &self.node_id
    }

    /// The raw Ed25519 public key.
    pub(crate) fn pubkey(&self) -> [u8; 32] {
        let mut out = [0_u8; 32];
        out.copy_from_slice(self.pair.public_key().as_ref());
        out
    }

    /// The public key in standard base64.
    pub(crate) fn pubkey_b64(&self) -> String {
        b64(&self.pubkey())
    }

    /// An Ed25519 signature of `message`.
    pub(crate) fn sign(&self, message: &[u8]) -> Vec<u8> {
        self.pair.sign(message).as_ref().to_vec()
    }

    fn prekeys(&self) -> std::sync::MutexGuard<'_, Vec<Prekey>> {
        self.prekeys.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The current prekey's bundle.
    pub(crate) fn bundle(&self) -> PrekeyBundle {
        let prekeys = self.prekeys();
        let current = prekeys.last();
        PrekeyBundle {
            x25519_pub: current.map(|key| b64(&key.public)).unwrap_or_default(),
            epoch: current.map_or(0, |key| key.epoch),
            prekey_sig: current.map(|key| b64(&key.sig)).unwrap_or_default(),
        }
    }

    /// A new prekey, its epoch past the last one's and at least the
    /// time in seconds; the one before it is still opened, older ones
    /// are dropped (wiped).
    pub(crate) fn rotate(&self) -> Result<PrekeyBundle, String> {
        let secret = Zeroizing::new(random::<32>()?);
        let public = x25519::public(&secret);
        let mut prekeys = self.prekeys();
        let seconds = u64::try_from(now_ms() / 1000).unwrap_or(0);
        let epoch = prekeys
            .last()
            .map_or(seconds, |last| (last.epoch + 1).max(seconds));
        let sig = self.sign(&prekey_message(&self.node_id, &public, epoch));
        prekeys.push(Prekey {
            secret,
            public,
            epoch,
            sig,
        });
        while prekeys.len() > 2 {
            prekeys.remove(0);
        }
        drop(prekeys);
        Ok(self.bundle())
    }

    /// The secret of the prekey `public` at `epoch`, when it is current or
    /// the one before.
    fn prekey_secret(&self, public: &[u8; 32], epoch: u64) -> Option<Zeroizing<[u8; 32]>> {
        self.prekeys()
            .iter()
            .find(|key| key.epoch == epoch && same_bytes(&key.public, public))
            .map(|key| key.secret.clone())
    }

    /// This node's public keys as a coordinator's `peer_info` gives
    /// them.
    pub(crate) fn public_info(&self) -> Value {
        let bundle = self.bundle();
        json!({
            "node_id": self.node_id, "pubkey": self.pubkey_b64(),
            "x25519_pub": bundle.x25519_pub, "epoch": bundle.epoch,
            "prekey_sig": bundle.prekey_sig,
        })
    }
}

/// Write a new key file, mode 0600, its missing directories 0700; never
/// over a file there already.
fn write_key_file(file: &Path, pkcs8: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
    let io = |error: std::io::Error| format!("{}: {error}", file.display());
    if let Some(parent) = file
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(io)?;
    }
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(file)
        .map_err(io)?;
    let text = Zeroizing::new(format!("{}\n", b64(pkcs8)));
    out.write_all(text.as_bytes()).map_err(io)?;
    out.sync_all().map_err(io)
}

/// A peer's public keys, checked: its id is its key's hash, its key the
/// one pinned (when one is), its prekey signed by it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PeerKeys {
    pub(crate) node_id: String,
    pub(crate) pubkey: [u8; 32],
    pub(crate) x25519: [u8; 32],
    pub(crate) epoch: u64,
}

impl PeerKeys {
    /// `info`'s `node_id`, `pubkey`, `x25519_pub`, `epoch` and
    /// `prekey_sig`, checked; `wanted`, the node id asked for; `pinned`,
    /// the public key the membership holds for it.
    pub(crate) fn verify(
        info: &Value,
        wanted: Option<&str>,
        pinned: Option<&str>,
    ) -> Result<Self, String> {
        let node_id = info["node_id"]
            .as_str()
            .filter(|id| valid_node_id(id))
            .ok_or("a peer without a node id")?;
        let pubkey: [u8; 32] = bytes_of(&info["pubkey"], "public key")?;
        if node_id_of(&pubkey) != node_id {
            return Err("the peer's node id is not its key's".to_string());
        }
        if wanted.is_some_and(|wanted| wanted != node_id) {
            return Err("the coordinator answered for another node".to_string());
        }
        if let Some(pinned) = pinned {
            let pinned = unb64(pinned).map_err(|_| "the pinned public key is not base64")?;
            if !same_bytes(&pinned, &pubkey) {
                return Err("the peer's key is not the one pinned at its join".to_string());
            }
        }
        let x25519: [u8; 32] = bytes_of(&info["x25519_pub"], "prekey")?;
        let epoch = info["epoch"].as_u64().ok_or("a prekey without its epoch")?;
        let sig = info["prekey_sig"]
            .as_str()
            .and_then(|sig| unb64(sig).ok())
            .ok_or("a prekey without its signature")?;
        signature::UnparsedPublicKey::new(&signature::ED25519, pubkey)
            .verify(&prekey_message(node_id, &x25519, epoch), &sig)
            .map_err(|_| "the prekey's signature is not the peer's".to_string())?;
        Ok(Self {
            node_id: node_id.to_string(),
            pubkey,
            x25519,
            epoch,
        })
    }
}

/// Whom a node trusts: the token (and the one before it) and the revoked
/// node ids.
#[derive(Clone, Default)]
pub(crate) struct TrustPolicy {
    token: Option<Zeroizing<String>>,
    previous: Option<Zeroizing<String>>,
    revoked: BTreeSet<String>,
}

impl std::fmt::Debug for TrustPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrustPolicy")
            .field("token", &self.token.is_some())
            .field("previous", &self.previous.is_some())
            .field("revoked", &self.revoked)
            .finish()
    }
}

impl TrustPolicy {
    /// `token`, `previous` (only with a token) and the revoked ids, a
    /// comma-separated list (each a node id; anything else ignored).
    pub(crate) fn new(token: Option<&str>, previous: Option<&str>, revoked: Option<&str>) -> Self {
        let token = token
            .filter(|t| !t.is_empty())
            .map(|t| Zeroizing::new(t.to_string()));
        let previous = previous
            .filter(|t| !t.is_empty() && token.is_some())
            .map(|t| Zeroizing::new(t.to_string()));
        let revoked = revoked
            .unwrap_or_default()
            .split(',')
            .map(|id| id.trim().to_ascii_lowercase())
            .filter(|id| valid_node_id(id))
            .collect();
        Self {
            token,
            previous,
            revoked,
        }
    }

    /// `SPAWN_NODE_TOKEN`, `SPAWN_NODE_TOKEN_PREV`, `SPAWN_NODE_REVOKED`.
    pub(crate) fn from_env() -> Self {
        let env = crate::config::env_value;
        Self::new(
            env("SPAWN_NODE_TOKEN").as_deref(),
            env("SPAWN_NODE_TOKEN_PREV").as_deref(),
            env("SPAWN_NODE_REVOKED").as_deref(),
        )
    }

    /// The token a node seals and signs with.
    pub(crate) fn token(&self) -> Option<&str> {
        self.token.as_deref().map(String::as_str)
    }

    /// The tokens a node opens and verifies with: the current, then the
    /// one before.
    pub(crate) fn tokens(&self) -> Vec<&str> {
        [&self.token, &self.previous]
            .into_iter()
            .flatten()
            .map(|token| token.as_str())
            .collect()
    }

    /// What an envelope's keys mix in: each token, or nothing without one.
    fn psks(&self) -> Vec<Option<&str>> {
        let tokens = self.tokens();
        if tokens.is_empty() {
            vec![None]
        } else {
            tokens.into_iter().map(Some).collect()
        }
    }

    pub(crate) fn revoked(&self, node_id: &str) -> bool {
        self.revoked.contains(node_id)
    }
}

/// What an envelope's two keys are bound to.
fn e2e_keys(
    shared: &[u8; 32],
    psk: Option<&str>,
    header: &EnvelopeHeader,
) -> Result<[Zeroizing<[u8; 32]>; 2], String> {
    let mut salt = Zeroizing::new(E2E_DOMAIN.as_bytes().to_vec());
    if let Some(psk) = psk {
        salt.extend_from_slice(&sha256(psk.as_bytes()));
    }
    let info = |direction: &str| {
        canonical(&[
            json!(E2E_DOMAIN),
            json!(header.from),
            json!(header.to),
            json!(header.epoch),
            json!(header.channel_id),
            json!(header.at_ms),
            json!(direction),
        ])
    };
    derive(&salt, shared, [&info("request"), &info("reply")])
}

/// The plaintext header of a sealed request: its signature and its
/// AEAD's associated data.
struct EnvelopeHeader {
    from: String,
    to: String,
    from_pub: String,
    eph: String,
    to_xpub: String,
    epoch: u64,
    channel_id: String,
    at_ms: i64,
    req_nonce: String,
}

impl EnvelopeHeader {
    fn canonical(&self) -> Vec<u8> {
        canonical(&[
            json!(E2E_DOMAIN),
            json!(self.from),
            json!(self.to),
            json!(self.from_pub),
            json!(self.eph),
            json!(self.to_xpub),
            json!(self.epoch),
            json!(self.channel_id),
            json!(self.at_ms),
            json!(self.req_nonce),
        ])
    }

    fn of(envelope: &Value) -> Result<Self, String> {
        let text = |key: &str| {
            envelope[key]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("an envelope without {key}"))
        };
        if envelope["v"] != E2E_DOMAIN {
            return Err(format!("not a {E2E_DOMAIN} envelope"));
        }
        let channel_id = text("channel_id")?;
        if channel_id.len() > 128 {
            return Err("an envelope's channel is too long".to_string());
        }
        Ok(Self {
            from: text("from")?,
            to: text("to")?,
            from_pub: text("from_pub")?,
            eph: text("eph")?,
            to_xpub: text("to_xpub")?,
            epoch: envelope["epoch"]
                .as_u64()
                .ok_or("an envelope without epoch")?,
            channel_id,
            at_ms: envelope["at_ms"]
                .as_i64()
                .ok_or("an envelope without at_ms")?,
            req_nonce: text("req_nonce")?,
        })
    }
}

/// What opens a sealed reply (the requester's) or seals one (the
/// responder's): the reply key and what the reply's header binds.
pub(crate) struct ReplyKey {
    key: Zeroizing<[u8; 32]>,
    /// The requester.
    pub(crate) requester: String,
    /// The responder.
    pub(crate) responder: String,
    channel_id: String,
    at_ms: i64,
    req_nonce: String,
}

impl ReplyKey {
    fn aad(&self, rep_nonce: &str) -> Vec<u8> {
        canonical(&[
            json!(E2E_REPLY_DOMAIN),
            json!(self.responder),
            json!(self.requester),
            json!(self.channel_id),
            json!(self.at_ms),
            json!(self.req_nonce),
            json!(rep_nonce),
        ])
    }

    /// `reply` sealed for the requester.
    pub(crate) fn seal(&self, reply: &Value) -> Result<Value, String> {
        let nonce = random::<12>()?;
        let rep_nonce = b64(&nonce);
        let plain = Zeroizing::new(serde_json::to_vec(reply).map_err(|e| e.to_string())?);
        let ct = seal_aead(&self.key, nonce, &self.aad(&rep_nonce), &plain)?;
        Ok(json!({
            "v": E2E_REPLY_DOMAIN, "from": self.responder, "to": self.requester,
            "channel_id": self.channel_id, "req_nonce": self.req_nonce,
            "rep_nonce": rep_nonce, "ct": b64(&ct),
        }))
    }

    /// The reply `envelope` seals: refused when it is not this request's,
    /// or was changed on its way.
    pub(crate) fn open(&self, envelope: &Value) -> Result<Value, String> {
        if envelope["v"] == "spawn-e2e-v1-error" {
            return Err(format!(
                "the node refused the envelope: {}",
                said(&envelope["error"])
            ));
        }
        if envelope["v"] != E2E_REPLY_DOMAIN
            || envelope["from"] != self.responder.as_str()
            || envelope["to"] != self.requester.as_str()
            || envelope["req_nonce"] != self.req_nonce.as_str()
        {
            return Err("a reply that is not this request's".to_string());
        }
        let rep_nonce = envelope["rep_nonce"]
            .as_str()
            .ok_or("a reply without rep_nonce")?;
        let nonce: [u8; 12] = bytes_of(&envelope["rep_nonce"], "reply nonce")?;
        let ct = envelope["ct"]
            .as_str()
            .and_then(|ct| unb64(ct).ok())
            .ok_or("a reply without ct")?;
        let plain = open_aead(&self.key, nonce, &self.aad(rep_nonce), &ct)
            .ok_or("the reply does not open: changed on its way, or not this node's")?;
        serde_json::from_slice(&plain).map_err(|_| "the reply is no JSON".to_string())
    }
}

/// Seal `payload` from `me` to `peer` on `channel_id`: the envelope, and
/// the key its reply opens with.
pub(crate) fn seal_request(
    me: &NodeIdentity,
    peer: &PeerKeys,
    policy: &TrustPolicy,
    channel_id: &str,
    payload: &Value,
) -> Result<(Value, ReplyKey), String> {
    if policy.revoked(&peer.node_id) {
        return Err(format!("node {} is revoked", peer.node_id));
    }
    let eph_secret = Zeroizing::new(random::<32>()?);
    let eph_public = x25519::public(&eph_secret);
    let shared = agree(&eph_secret, &peer.x25519)?;
    let req_nonce = random::<12>()?;
    let header = EnvelopeHeader {
        from: me.node_id().to_string(),
        to: peer.node_id.clone(),
        from_pub: me.pubkey_b64(),
        eph: b64(&eph_public),
        to_xpub: b64(&peer.x25519),
        epoch: peer.epoch,
        channel_id: channel_id.to_string(),
        at_ms: now_ms(),
        req_nonce: b64(&req_nonce),
    };
    let [request_key, reply_key] = e2e_keys(&shared, policy.token(), &header)?;
    let aad = header.canonical();
    let sig = me.sign(&aad);
    let plain = Zeroizing::new(serde_json::to_vec(payload).map_err(|e| e.to_string())?);
    let ct = seal_aead(&request_key, req_nonce, &aad, &plain)?;
    let envelope = json!({
        "v": E2E_DOMAIN, "from": header.from, "to": header.to, "from_pub": header.from_pub,
        "eph": header.eph, "to_xpub": header.to_xpub, "epoch": header.epoch,
        "channel_id": header.channel_id, "at_ms": header.at_ms,
        "req_nonce": header.req_nonce, "sig": b64(&sig), "ct": b64(&ct),
    });
    let reply = ReplyKey {
        key: reply_key,
        requester: header.from,
        responder: header.to,
        channel_id: header.channel_id,
        at_ms: header.at_ms,
        req_nonce: header.req_nonce,
    };
    Ok((envelope, reply))
}

/// A request a [`Responder`] opened: what it asks, who sent it, and the
/// key its reply is sealed with.
pub(crate) struct Opened {
    pub(crate) request: Value,
    pub(crate) from: String,
    pub(crate) from_pub: String,
    pub(crate) reply: ReplyKey,
}

/// How long a request's `(from, req_nonce)` is remembered: until its
/// `at_ms` can no longer pass the skew check, with a margin, never longer
/// than twice the skew.
pub(crate) fn replay_until(at_ms: i64, now: i64) -> i64 {
    at_ms
        .saturating_add(MAX_SKEW_MS + 5_000)
        .min(now.saturating_add(2 * MAX_SKEW_MS + 10_000))
}

/// A node's end of `spawn-e2e-v1`: it opens the envelopes sealed to it
/// (each once), and seals their replies.
pub(crate) struct Responder {
    identity: Arc<NodeIdentity>,
    policy: TrustPolicy,
    seen: Mutex<HashMap<String, i64>>,
}

impl Responder {
    pub(crate) fn new(identity: Arc<NodeIdentity>, policy: TrustPolicy) -> Self {
        Self {
            identity,
            policy,
            seen: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn policy(&self) -> &TrustPolicy {
        &self.policy
    }

    /// Open `envelope`: refused when its sender is revoked, its time is
    /// past the skew, its signature or id is not its sender's, it was
    /// opened before, it is sealed to no current prekey of this node, or
    /// it does not open under any token this node takes.
    pub(crate) fn open(&self, envelope: &Value) -> Result<Opened, String> {
        let header = EnvelopeHeader::of(envelope)?;
        if header.to != self.identity.node_id() {
            return Err("an envelope sealed to another node".to_string());
        }
        if self.policy.revoked(&header.from) {
            return Err("the sender is revoked".to_string());
        }
        let now = now_ms();
        if now.saturating_sub(header.at_ms).abs() > MAX_SKEW_MS {
            return Err("the envelope's at_ms is too far from this node's clock".to_string());
        }
        let from_pub: [u8; 32] = bytes_of(&envelope["from_pub"], "sender key")?;
        if node_id_of(&from_pub) != header.from {
            return Err("the sender's id is not its key's".to_string());
        }
        let aad = header.canonical();
        let sig = envelope["sig"]
            .as_str()
            .and_then(|sig| unb64(sig).ok())
            .ok_or("an envelope without its signature")?;
        signature::UnparsedPublicKey::new(&signature::ED25519, from_pub)
            .verify(&aad, &sig)
            .map_err(|_| "the envelope's signature is not its sender's".to_string())?;
        let to_xpub: [u8; 32] = bytes_of(&envelope["to_xpub"], "prekey")?;
        let secret = self
            .identity
            .prekey_secret(&to_xpub, header.epoch)
            .ok_or("an envelope sealed to a prekey this node no longer holds")?;
        let eph: [u8; 32] = bytes_of(&envelope["eph"], "ephemeral key")?;
        let shared = agree(&secret, &eph)?;
        let req_nonce: [u8; 12] = bytes_of(&envelope["req_nonce"], "request nonce")?;
        let ct = envelope["ct"]
            .as_str()
            .and_then(|ct| unb64(ct).ok())
            .filter(|ct| ct.len() <= MAX_SEALED)
            .ok_or("an envelope without ct")?;
        let seen_key = format!("{}:{}", header.from, header.req_nonce);
        self.remember(&seen_key, header.at_ms, now)?;
        for psk in self.policy.psks() {
            let [request_key, reply_key] = e2e_keys(&shared, psk, &header)?;
            if let Some(plain) = open_aead(&request_key, req_nonce, &aad, &ct) {
                let request =
                    serde_json::from_slice(&plain).map_err(|_| "the request is no JSON")?;
                return Ok(Opened {
                    request,
                    from: header.from.clone(),
                    from_pub: header.from_pub.clone(),
                    reply: ReplyKey {
                        key: reply_key,
                        requester: header.from,
                        responder: header.to,
                        channel_id: header.channel_id,
                        at_ms: header.at_ms,
                        req_nonce: header.req_nonce,
                    },
                });
            }
        }
        self.forget(&seen_key);
        Err(
            "the envelope does not open: changed on its way, or sealed without this node's token"
                .to_string(),
        )
    }

    /// Mark `key` seen until [`replay_until`]; refused when it was.
    fn remember(&self, key: &str, at_ms: i64, now: i64) -> Result<(), String> {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        seen.retain(|_, until| *until > now);
        if seen.contains_key(key) {
            return Err("a replayed envelope".to_string());
        }
        if seen.len() >= MAX_REPLAYS {
            return Err("this node is busy".to_string());
        }
        seen.insert(key.to_string(), replay_until(at_ms, now));
        Ok(())
    }

    fn forget(&self, key: &str) {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(key);
    }
}

impl std::fmt::Debug for Opened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Opened")
            .field("from", &self.from)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ReplyKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplyKey")
            .field("responder", &self.responder)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for WireKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WireKey")
            .field("nonce", &self.nonce)
            .finish_non_exhaustive()
    }
}

/// A peer's or a coordinator's words, as this node says them: one line of
/// at most 200 printable characters (super::clean_text), masked; a value
/// that is no text, or nothing printable, is "refused".
pub(crate) fn said(error: &Value) -> String {
    error.as_str().and_then(super::clean_text).map_or_else(
        || "refused".to_string(),
        |text| spawn_core::redact::redact(&text),
    )
}

/// An unauthenticated refusal of `envelope`, which did not open (it says
/// nothing a relay could not see), naming the request it refuses.
pub(crate) fn envelope_error(why: &str, envelope: &Value) -> Value {
    json!({"v": "spawn-e2e-v1-error", "error": said(&json!(why)),
           "from": envelope["to"], "to": envelope["from"], "req_nonce": envelope["req_nonce"]})
}

/// What a wire seal's reply is sealed and opened with: the request's key
/// (to seal the request), and the token, salt, nonce and time each reply
/// key is derived from, with a salt of the reply's own.
pub(crate) struct WireKey {
    request: Zeroizing<[u8; 32]>,
    token: Zeroizing<String>,
    salt: String,
    nonce: String,
    at_ms: i64,
}

fn wire_key(
    token: &str,
    salt: &[u8],
    salt_b64: &str,
    nonce: &str,
    at_ms: i64,
) -> Result<WireKey, String> {
    let info = canonical(&[
        json!(WIRE_DOMAIN),
        json!(nonce),
        json!(at_ms),
        json!("request"),
    ]);
    let [request, _] = derive(salt, token.as_bytes(), [&info, &info])?;
    Ok(WireKey {
        request,
        token: Zeroizing::new(token.to_string()),
        salt: salt_b64.to_string(),
        nonce: nonce.to_string(),
        at_ms,
    })
}

fn wire_aad(op: &str, nonce: &str, at_ms: i64, salt: &str) -> Vec<u8> {
    canonical(&[
        json!(WIRE_DOMAIN),
        json!(op),
        json!(nonce),
        json!(at_ms),
        json!(salt),
    ])
}

/// `message` (a request with its nonce) sealed under `token`: `{op:
/// "sealed", nonce, at_ms, salt, ct, auth}` (the auth: super::auth's MAC
/// of it, checked before anything is opened), and the key its reply opens
/// with. The request's key is its own (a fresh salt each), so its zero
/// nonce is never used twice under one key.
pub(crate) fn seal_wire(message: &Value, token: &str) -> Result<(Value, WireKey), String> {
    let nonce = message["nonce"]
        .as_str()
        .ok_or("a request without a nonce")?;
    let at_ms = now_ms();
    let salt = random::<32>()?;
    let salt_b64 = b64(&salt);
    let key = wire_key(token, &salt, &salt_b64, nonce, at_ms)?;
    let plain = Zeroizing::new(serde_json::to_vec(message).map_err(|e| e.to_string())?);
    let ct = seal_aead(
        &key.request,
        [0; 12],
        &wire_aad("sealed", nonce, at_ms, &salt_b64),
        &plain,
    )?;
    let mut sealed = json!({"op": "sealed", "nonce": nonce, "at_ms": at_ms, "salt": salt_b64,
                            "ct": b64(&ct)});
    super::auth::sign(&mut sealed, Some(token));
    Ok((sealed, key))
}

/// A sealed request opened with the first of `tokens` its auth is
/// signed with: the request (its nonce the seal's), and the key its reply
/// is sealed with.
pub(crate) fn open_wire(sealed: &Value, tokens: &[&str]) -> Result<(Value, WireKey), String> {
    let nonce = sealed["nonce"]
        .as_str()
        .ok_or("a sealed request without a nonce")?;
    let at_ms = sealed["at_ms"]
        .as_i64()
        .ok_or("a sealed request without at_ms")?;
    let salt_b64 = sealed["salt"]
        .as_str()
        .ok_or("a sealed request without salt")?;
    let salt = unb64(salt_b64).map_err(|_| "a sealed request's salt is not base64")?;
    let ct = sealed["ct"]
        .as_str()
        .and_then(|ct| unb64(ct).ok())
        .filter(|ct| ct.len() <= MAX_SEALED)
        .ok_or("a sealed request without ct")?;
    for token in tokens {
        if super::auth::verify(sealed, token).is_err() {
            continue;
        }
        let key = wire_key(token, &salt, salt_b64, nonce, at_ms)?;
        let Some(plain) = open_aead(
            &key.request,
            [0; 12],
            &wire_aad("sealed", nonce, at_ms, salt_b64),
            &ct,
        ) else {
            continue;
        };
        let inner: Value =
            serde_json::from_slice(&plain).map_err(|_| "a sealed request that is no JSON")?;
        if !inner.is_object() || inner["nonce"] != nonce {
            return Err("a sealed request whose inner nonce is not its own".to_string());
        }
        return Ok((inner, key));
    }
    Err("the sealed request's auth is not this node's token's".to_string())
}

impl WireKey {
    /// The key of one reply: HKDF of the token under the reply's own
    /// salt, bound to the request's nonce, time and salt.
    fn reply_key(&self, reply_salt: &[u8]) -> Result<Zeroizing<[u8; 32]>, String> {
        let info = canonical(&[
            json!(WIRE_DOMAIN),
            json!(self.nonce),
            json!(self.at_ms),
            json!(self.salt),
            json!("reply"),
        ]);
        let [key, _] = derive(reply_salt, self.token.as_bytes(), [&info, &info])?;
        Ok(key)
    }

    fn reply_aad(&self, reply_salt: &str, reply_nonce: &str) -> Vec<u8> {
        canonical(&[
            json!(WIRE_DOMAIN),
            json!("sealed_reply"),
            json!(self.nonce),
            json!(self.at_ms),
            json!(self.salt),
            json!(reply_salt),
            json!(reply_nonce),
        ])
    }

    /// `reply` sealed: `{op: "sealed_reply", nonce, ok: true, reply_salt,
    /// reply_nonce, ct}` (the reply's own `ok` is inside). Each reply has
    /// a fresh salt (its key) and nonce, so two replies to one request
    /// (two nodes sharing the token, a resend answered again) never share
    /// a keystream.
    pub(crate) fn seal_reply(&self, reply: &Value) -> Result<Value, String> {
        let reply_salt = random::<32>()?;
        let reply_nonce = random::<12>()?;
        let (salt_b64, nonce_b64) = (b64(&reply_salt), b64(&reply_nonce));
        let key = self.reply_key(&reply_salt)?;
        let plain = Zeroizing::new(serde_json::to_vec(reply).map_err(|e| e.to_string())?);
        let ct = seal_aead(
            &key,
            reply_nonce,
            &self.reply_aad(&salt_b64, &nonce_b64),
            &plain,
        )?;
        Ok(
            json!({"op": "sealed_reply", "nonce": self.nonce, "ok": true,
                  "reply_salt": salt_b64, "reply_nonce": nonce_b64, "ct": b64(&ct)}),
        )
    }

    /// The reply a `sealed_reply` seals (its nonce this request's).
    pub(crate) fn open_reply(&self, sealed: &Value) -> Result<Value, String> {
        if sealed["op"] != "sealed_reply" || sealed["nonce"] != self.nonce.as_str() {
            return Err("an unsealed reply to a sealed request".to_string());
        }
        let salt_b64 = sealed["reply_salt"]
            .as_str()
            .ok_or("a sealed reply without reply_salt")?;
        let nonce_b64 = sealed["reply_nonce"]
            .as_str()
            .ok_or("a sealed reply without reply_nonce")?;
        let reply_salt: [u8; 32] = bytes_of(&sealed["reply_salt"], "reply salt")?;
        let reply_nonce: [u8; 12] = bytes_of(&sealed["reply_nonce"], "reply nonce")?;
        let ct = sealed["ct"]
            .as_str()
            .and_then(|ct| unb64(ct).ok())
            .filter(|ct| ct.len() <= MAX_SEALED)
            .ok_or("a sealed reply without ct")?;
        let key = self.reply_key(&reply_salt)?;
        let plain = open_aead(&key, reply_nonce, &self.reply_aad(salt_b64, nonce_b64), &ct)
            .ok_or("the sealed reply does not open: changed on its way")?;
        let reply: Value =
            serde_json::from_slice(&plain).map_err(|_| "a sealed reply that is no JSON")?;
        if reply["nonce"] != self.nonce.as_str() {
            return Err("a sealed reply naming another request".to_string());
        }
        Ok(reply)
    }
}

#[cfg(test)]
mod tests;
