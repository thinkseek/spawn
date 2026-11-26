//! `GET /node/v1/artifacts/<algo:hex>` (section 3.6): files of
//! `SPAWN_NODE_ARTIFACTS`, `<algo>/<2 hex>/<hex>`, streamed with
//! `Content-Length`, `Accept-Ranges: bytes` and one-range `206` answers.
//! Their bodies are not signed: a node believes an artifact only if it
//! hashes to its address. `GET /node/v1/registry-manifest` is not an
//! artifact: [`manifest_body`] builds its JSON, which the gateway signs as
//! every other answer (design §2.3).

use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use spawn_cas::{Algo, Digest};

use crate::http::{Body, Response};

/// The manifest's file name under `SPAWN_NODE_ARTIFACTS`.
pub const REGISTRY_MANIFEST: &str = "registry-manifest.json";
/// The largest manifest file served.
pub const MAX_MANIFEST: u64 = 1024 * 1024;

/// Why no manifest is served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    /// No such regular file: `404`.
    Missing,
    /// The file is not a manifest: the operator's fault, `503 busy`.
    Invalid(String),
}

/// The registry manifest's answer body: the file (at most 1 MiB, strict
/// JSON) must be an object with `registry` (64 lowercase hex, the cargo
/// registry's `tree_sha256_v1`) and `pack` (`{digest, size}`: the packed
/// registry's content address and length); its members are kept, with
/// `"v": 1` and `"server_time_ms": now` set.
pub fn manifest_body(path: &Path, now: u64) -> Result<Vec<u8>, ManifestError> {
    let invalid = |why: &str| ManifestError::Invalid(why.to_string());
    let metadata = std::fs::symlink_metadata(path).map_err(|_| ManifestError::Missing)?;
    if !metadata.file_type().is_file() {
        return Err(ManifestError::Missing);
    }
    if metadata.len() > MAX_MANIFEST {
        return Err(invalid("over 1 MiB"));
    }
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(MAX_MANIFEST + 1).read_to_end(&mut bytes))
        .map_err(|error| ManifestError::Invalid(error.to_string()))?;
    if bytes.len() as u64 > MAX_MANIFEST {
        return Err(invalid("over 1 MiB"));
    }
    let mut manifest: serde_json::Map<String, serde_json::Value> =
        spawn_node_proto::json::parse(&bytes)
            .map_err(|error| ManifestError::Invalid(error.to_string()))?;
    let registry_ok = manifest
        .get("registry")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|hex| spawn_node_proto::codec::is_lower_hex(hex, 64));
    if !registry_ok {
        return Err(invalid("registry is not 64 lowercase hex"));
    }
    let pack = manifest.get("pack").ok_or_else(|| invalid("no pack"))?;
    let digest_ok = pack
        .get("digest")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|digest| Digest::parse(digest).is_ok());
    if !digest_ok
        || pack
            .get("size")
            .and_then(serde_json::Value::as_u64)
            .is_none()
    {
        return Err(invalid("pack is not {digest, size}"));
    }
    manifest.insert("v".to_string(), spawn_node_proto::VERSION.into());
    manifest.insert("server_time_ms".to_string(), now.into());
    Ok(spawn_node_proto::json::to_vec(&manifest))
}

/// Why no file is served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Miss {
    /// No such regular file: `404`.
    NotFound,
}

/// The file of a content address (`sha256` only in D6 v1).
pub fn artifact_path(root: &Path, digest: &Digest) -> PathBuf {
    match digest.algo() {
        // The hex is 64 lowercase hex digits (Digest::parse): no
        // separator, no dot, nothing to escape.
        Algo::Sha256 => root
            .join(digest.algo().name())
            .join(digest.shard())
            .join(digest.hex()),
    }
}

/// One range of a file of `len` bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    /// The whole file (no `Range`, or one this server ignores: several
    /// ranges, another unit, a malformed one).
    Whole,
    /// `[start, end]`, inclusive.
    Part {
        /// Inclusive start byte offset.
        start: u64,
        /// Inclusive end byte offset.
        end: u64,
    },
    /// Starts past the end: `416`.
    Unsatisfiable,
}

/// The range a `Range` header asks of a file of `len` bytes (RFC 9110
/// §14.1.2, one `bytes` range).
pub fn parse_range(header: Option<&str>, len: u64) -> Range {
    let Some(spec) = header.and_then(|value| value.trim().strip_prefix("bytes=")) else {
        return Range::Whole;
    };
    if spec.contains(',') {
        return Range::Whole;
    }
    let Some((first, last)) = spec.trim().split_once('-') else {
        return Range::Whole;
    };
    let number = |text: &str| -> Option<u64> {
        (!text.is_empty() && text.len() <= 19 && text.bytes().all(|b| b.is_ascii_digit()))
            .then(|| text.parse().ok())
            .flatten()
    };
    match (first.is_empty(), number(first), number(last)) {
        // bytes=-N: the last N bytes.
        (true, _, Some(suffix)) => {
            if suffix == 0 || len == 0 {
                Range::Unsatisfiable
            } else {
                Range::Part {
                    start: len.saturating_sub(suffix),
                    end: len - 1,
                }
            }
        }
        (false, Some(start), last) if last.is_some() || last_is_empty(spec) => {
            if start >= len {
                return Range::Unsatisfiable;
            }
            let end = last.map_or(len - 1, |last| last.min(len - 1));
            if end < start {
                Range::Whole
            } else {
                Range::Part { start, end }
            }
        }
        _ => Range::Whole,
    }
}

fn last_is_empty(spec: &str) -> bool {
    spec.trim().ends_with('-')
}

/// The answer serving `path` with `range`: `200`, `206` or `416`.
pub fn serve(path: &Path, range: Option<&str>) -> Result<Response, Miss> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| Miss::NotFound)?;
    if !metadata.file_type().is_file() {
        return Err(Miss::NotFound);
    }
    let file = File::open(path).map_err(|_| Miss::NotFound)?;
    // The open file's length, not the path's (a rename in between).
    let len = file.metadata().map_err(|_| Miss::NotFound)?.len();
    let response = match parse_range(range, len) {
        Range::Whole => Response {
            status: 200,
            headers: Vec::new(),
            body: Body::File {
                file,
                offset: 0,
                len,
            },
        },
        Range::Part { start, end } => Response {
            status: 206,
            headers: Vec::new(),
            body: Body::File {
                file,
                offset: start,
                len: end - start + 1,
            },
        }
        .header("Content-Range", format!("bytes {start}-{end}/{len}")),
        Range::Unsatisfiable => {
            Response::bytes(416, Vec::new()).header("Content-Range", format!("bytes */{len}"))
        }
    };
    Ok(response
        .header("Accept-Ranges", "bytes")
        .header("Content-Type", "application/octet-stream"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range(None, 10), Range::Whole);
        assert_eq!(
            parse_range(Some("bytes=2-5"), 10),
            Range::Part { start: 2, end: 5 }
        );
        assert_eq!(
            parse_range(Some("bytes=2-"), 10),
            Range::Part { start: 2, end: 9 }
        );
        assert_eq!(
            parse_range(Some("bytes=2-99"), 10),
            Range::Part { start: 2, end: 9 }
        );
        assert_eq!(
            parse_range(Some("bytes=-3"), 10),
            Range::Part { start: 7, end: 9 }
        );
        assert_eq!(
            parse_range(Some("bytes=-30"), 10),
            Range::Part { start: 0, end: 9 }
        );
        assert_eq!(parse_range(Some("bytes=10-"), 10), Range::Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=-0"), 10), Range::Unsatisfiable);
        for ignored in [
            "bytes=1-2,4-5",
            "items=1-2",
            "bytes=x-2",
            "bytes=5-2",
            "bytes=-",
        ] {
            assert_eq!(parse_range(Some(ignored), 10), Range::Whole, "{ignored}");
        }
    }

    #[test]
    fn only_sha256_has_files() {
        let digest = spawn_cas::sha256_of(b"abc");
        let path = artifact_path(Path::new("/a"), &digest);
        assert_eq!(
            path,
            Path::new("/a/sha256")
                .join(digest.shard())
                .join(digest.hex())
        );
        assert!(
            Digest::parse(&format!("blake3:{}", digest.hex())).is_err(),
            "blake3 digests must be refused at parse time"
        );
    }

    #[test]
    fn a_manifest_is_checked_and_stamped() {
        let dir = std::env::temp_dir().join(format!(
            "spawn-node-gateway-manifest-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(REGISTRY_MANIFEST);
        assert_eq!(manifest_body(&path, 7), Err(ManifestError::Missing));
        let digest = spawn_cas::sha256_of(b"pack");
        let registry = "a".repeat(64);
        std::fs::write(
            &path,
            format!(r#"{{"registry":"{registry}","pack":{{"digest":"{digest}","size":4}},"v":9}}"#),
        )
        .unwrap();
        let body: serde_json::Value =
            serde_json::from_slice(&manifest_body(&path, 7).unwrap()).unwrap();
        assert_eq!(body["v"], 1);
        assert_eq!(body["server_time_ms"], 7);
        assert_eq!(body["registry"], registry.as_str());
        assert_eq!(body["pack"]["size"], 4);
        for bad in [
            r#"{"registry":"x","pack":{"digest":"sha256:00","size":1}}"#.to_string(),
            format!(r#"{{"registry":"{registry}"}}"#),
            format!(r#"{{"registry":"{registry}","pack":{{"digest":"{digest}","size":-1}}}}"#),
            format!(
                r#"{{"registry":"{registry}","registry":"{registry}","pack":{{"digest":"{digest}","size":1}}}}"#
            ),
            "[]".to_string(),
        ] {
            std::fs::write(&path, &bad).unwrap();
            assert!(
                matches!(manifest_body(&path, 7), Err(ManifestError::Invalid(_))),
                "{bad}"
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
