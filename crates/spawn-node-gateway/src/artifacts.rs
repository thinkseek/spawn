//! `GET /node/v1/artifacts/<algo:hex>` and `GET /node/v1/registry-manifest`
//! (section 3.6): files of `SPAWN_NODE_ARTIFACTS`, `<algo>/<2 hex>/<hex>`
//! and `registry-manifest.json`, streamed with `Content-Length`,
//! `Accept-Ranges: bytes` and one-range `206` answers. The bodies are not
//! signed: a node believes an artifact only if it hashes to its address,
//! and the manifest only if the registry it describes hashes to the
//! grant's `requires.registry`.

use std::fs::File;
use std::path::{Path, PathBuf};

use spawn_node_proto::ContentDigest;
use spawn_node_proto::artifact::Algo;

use crate::http::{Body, Response};

/// The manifest's file name under `SPAWN_NODE_ARTIFACTS`.
pub const REGISTRY_MANIFEST: &str = "registry-manifest.json";

/// Why no file is served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Miss {
    /// A digest algorithm v1 serves no file of (`blake3`): `400`.
    Unsupported,
    /// No such regular file: `404`.
    NotFound,
}

/// The file of a content address: `sha256` only.
pub fn artifact_path(root: &Path, digest: &ContentDigest) -> Result<PathBuf, Miss> {
    match digest.algo {
        // The hex is 64 lowercase hex digits (ContentDigest::parse): no
        // separator, no dot, nothing to escape.
        Algo::Sha256 => Ok(root
            .join(digest.algo.as_str())
            .join(&digest.hex[..2])
            .join(&digest.hex)),
        Algo::Blake3 => Err(Miss::Unsupported),
    }
}

/// One range of a file of `len` bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    /// The whole file (no `Range`, or one this server ignores: several
    /// ranges, another unit, a malformed one).
    Whole,
    /// `[start, end]`, inclusive.
    Part { start: u64, end: u64 },
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
        let digest = ContentDigest::sha256_of(b"abc");
        let path = artifact_path(Path::new("/a"), &digest).unwrap();
        assert_eq!(
            path,
            Path::new("/a/sha256")
                .join(&digest.hex[..2])
                .join(&digest.hex)
        );
        let blake = ContentDigest::parse(&format!("blake3:{}", digest.hex)).unwrap();
        assert_eq!(
            artifact_path(Path::new("/a"), &blake),
            Err(Miss::Unsupported)
        );
    }
}
