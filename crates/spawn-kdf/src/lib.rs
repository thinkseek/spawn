//! PBKDF2-HMAC-SHA256, one 32-byte block (RFC 8018 §5.2 with dkLen = 32):
//! the derivation of Spawn's restricted login passwords
//! (spawn-postgres `role_password`).
//!
//! **Its output must never change.** Every install's restricted logins
//! (`spawn_supervisor`, `spawn_reader`, `spawn_human` of each database)
//! have passwords derived by this function from the owner's password and
//! each login's salt, and every process of the install derives them again
//! to connect. A derivation that differed by one bit would set different
//! passwords at the next migration and lock every other process of the
//! fleet out of its own database. The known-answer tests here and in
//! spawn-postgres pin it byte for byte to what the code produced before
//! this crate existed (554a4bd4).
//!
//! Why a crate of its own: HMAC and SHA-256 are generic, so their rounds
//! compile in the crate that calls them, at that crate's opt-level. Called
//! from spawn-postgres in a debug build (opt-level 0), the three logins'
//! 600 000 rounds took about 3 s per process, the floor of every database
//! test binary. Here, with `[profile.dev.package.spawn-kdf] opt-level = 3`
//! (the workspace's Cargo.toml), they take a fraction of a second. Release
//! builds were optimised already, and the output is the same.

use hmac::{Hmac, KeyInit, Mac};

/// The first 32-byte block of PBKDF2-HMAC-SHA256 of `password` over
/// `salt`, `rounds` rounds (0 is taken as 1).
#[must_use]
pub fn pbkdf2_sha256(password: &[u8], salt: &[u8], rounds: u32) -> [u8; 32] {
    let keyed =
        Hmac::<sha2::Sha256>::new_from_slice(password).expect("HMAC takes a key of any length");
    let mut round = keyed.clone();
    round.update(salt);
    round.update(&1u32.to_be_bytes());
    let mut previous: [u8; 32] = round.finalize().into_bytes().into();
    let mut output = previous;
    for _ in 1..rounds.max(1) {
        let mut round = keyed.clone();
        round.update(&previous);
        previous = round.finalize().into_bytes().into();
        for (out, byte) in output.iter_mut().zip(previous.iter()) {
            *out ^= byte;
        }
    }
    output
}

#[cfg(test)]
mod tests {
    /// RFC 7914 §11's PBKDF2-HMAC-SHA256 vector (P = "passwd", S = "salt",
    /// c = 1): the first 32 bytes of its 64.
    #[test]
    fn matches_the_rfc_7914_vector() {
        let out = super::pbkdf2_sha256(b"passwd", b"salt", 1);
        assert_eq!(
            out,
            [
                0x55, 0xac, 0x04, 0x6e, 0x56, 0xe3, 0x08, 0x9f, 0xec, 0x16, 0x91, 0xc2, 0x25, 0x44,
                0xb6, 0x05, 0xf9, 0x41, 0x85, 0x21, 0x6d, 0xde, 0x04, 0x65, 0xe6, 0x8b, 0x9d, 0x57,
                0xc2, 0x0d, 0xac, 0xbc,
            ]
        );
    }

    /// Zero rounds is one round, as the code before this crate behaved
    /// (its loop ran `1..rounds`).
    #[test]
    fn zero_rounds_is_one() {
        assert_eq!(
            super::pbkdf2_sha256(b"k", b"s", 0),
            super::pbkdf2_sha256(b"k", b"s", 1)
        );
    }
}
