//! The primary's Ed25519 key: a PKCS#8 file only its owner may read
//! (mode 0600), made on first start when absent. Every response, refusal
//! and grant the gateway sends is signed with it; enroll tokens pin the
//! first 16 bytes of the SHA-256 of its public key.

use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;

use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair as _};
use spawn_node_proto::SigningKey;
use zeroize::Zeroizing;

/// The DER before the 32-byte seed of an RFC 8410 (v1) PKCS#8 Ed25519
/// key: `SEQUENCE { INTEGER 0, AlgorithmIdentifier { 1.3.101.112 },
/// OCTET STRING { OCTET STRING (32) } }`.
const PKCS8_V1_PREFIX: [u8; 16] = [
    0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];
/// The same for a v2 document (version 1), the public key after the seed:
/// ring writes it as `[1] IMPLICIT BIT STRING` (`81 21 00`, 83 bytes in
/// all), RFC 8410's example as `[1] EXPLICIT` (`a1 23 03 21 00`, 85).
const PKCS8_V2_RING: ([u8; 2], &[u8], usize) = ([0x30, 0x51], &[0x81, 0x21, 0x00], 83);
const PKCS8_V2_RFC: ([u8; 2], &[u8], usize) = ([0x30, 0x53], &[0xa1, 0x23, 0x03, 0x21, 0x00], 85);
/// What follows the outer `SEQUENCE` header in every v2 document, up to
/// the seed.
const PKCS8_V2_BODY: [u8; 14] = [
    0x02, 0x01, 0x01, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];

/// The key in `path`, made (mode 0600, written whole and synced) when the
/// file does not exist. A file that others may read or write, or that is
/// not an Ed25519 PKCS#8 document, is refused.
pub fn load_or_create(path: &Path) -> Result<SigningKey, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => load(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create(path)?;
            load(path)
        }
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

fn create(path: &Path) -> Result<(), String> {
    let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
        .map_err(|_| "cannot generate an Ed25519 key".to_string())?;
    let document = Zeroizing::new(document.as_ref().to_vec());
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("create {}: {error}", path.display()))?;
    file.write_all(&document)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(|error| format!("sync {}: {error}", parent.display()))?;
    }
    Ok(())
}

fn load(path: &Path) -> Result<SigningKey, String> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    if !metadata.file_type().is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let mode = metadata.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(format!(
            "{} is mode {:04o}; the primary key must be 0600",
            path.display(),
            mode & 0o7777
        ));
    }
    let mut document = Zeroizing::new(Vec::new());
    File::open(path)
        .and_then(|file| file.take(4096).read_to_end(&mut document))
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    from_pkcs8(&document).map_err(|error| format!("{}: {error}", path.display()))
}

/// The signing key of a PKCS#8 Ed25519 document (v1 or v2).
pub fn from_pkcs8(document: &[u8]) -> Result<SigningKey, String> {
    // ring parses (and for v2 checks the public key against the seed).
    let pair = Ed25519KeyPair::from_pkcs8_maybe_unchecked(document)
        .map_err(|_| "not an Ed25519 PKCS#8 key".to_string())?;
    let v1 = document.len() == 48 && document[..16] == PKCS8_V1_PREFIX;
    let v2 = [PKCS8_V2_RING, PKCS8_V2_RFC]
        .iter()
        .any(|(head, public, len)| {
            document.len() == *len
                && document[..2] == *head
                && document[2..16] == PKCS8_V2_BODY
                && document[48..48 + public.len()] == **public
        });
    if !v1 && !v2 {
        return Err("not a plain Ed25519 PKCS#8 key".to_string());
    }
    let mut seed = Zeroizing::new([0_u8; 32]);
    seed.copy_from_slice(&document[16..48]);
    let key = SigningKey::from_seed(&seed).map_err(|error| error.to_string())?;
    if key.public_key().0.as_slice() != pair.public_key().as_ref() {
        return Err("the key's seed and public key disagree".to_string());
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "spawn-node-gateway-primary-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_missing_key_is_made_0600_and_loads_the_same_next_time() {
        let dir = scratch("make");
        let path = dir.join("primary.pk8");
        let first = load_or_create(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let again = load_or_create(&path).unwrap();
        assert_eq!(first.public_key(), again.public_key());

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let refused = load_or_create(&path).unwrap_err();
        assert!(refused.contains("0600"), "{refused}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_v1_document_gives_the_seed_s_key() {
        let seed = [0x20_u8; 32];
        let mut document = PKCS8_V1_PREFIX.to_vec();
        document.extend_from_slice(&seed);
        let key = from_pkcs8(&document).unwrap();
        assert_eq!(
            key.public_key(),
            SigningKey::from_seed(&seed).unwrap().public_key()
        );
        assert!(from_pkcs8(&document[..40]).is_err());
        assert!(from_pkcs8(b"not a key").is_err());
    }
}
