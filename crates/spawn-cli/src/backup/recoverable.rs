//! `spawn backup cas`: recoverable content-addressed storage (`fm-cas/v1`).
//!
//! Strictly additive, safe-Rust content-addressed storage engine that preserves
//! full restore metadata and feature-map/file/feature relationships without
//! touching existing `spawn backup` (`postgres/`, `bundles/`, `transcripts/`,
//! `sessions/`) or `spawn store` v2 layouts.
//!
//! Layout strictly under `<store_root>/fm-cas/v1/`:
//! - `blobs/<aa>/<digest>` (regular file blobs `<= chunk_size`)
//! - `blobs/sym/<aa>/<digest>` (UTF-8 symlink target blobs)
//! - `chunks/<aa>/<digest>` (fixed-size chunks for files `> chunk_size`)
//! - `graphs/<aa>/<digest>.json` (canonical `FmGraphSnapshot` objects)
//! - `snapshots/<aa>/<digest>.json` (immutable `SnapshotManifest` objects)
//! - `staging/.tmp-<pid>-<seq>` (transient write staging, ignored by readers)
//! - `.cas.lock` (advisory lock for `snapshots.json` updates)
//! - `snapshots.json` (append-only snapshot index, updated via atomic rename)

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::Flags;

/// Explicit schema and algorithm version embedded in every key and manifest.
pub(crate) const SCHEMA_VERSION: &str = "fm-cas/v1";

/// Default fixed chunk size for files larger than 4 MiB.
pub(crate) const DEFAULT_CHUNK_SIZE: usize = 4 * 1024 * 1024;

const DOMAIN_BLOB: &[u8] = b"spawn.cas.v1.blob\0";
const DOMAIN_CHUNK: &[u8] = b"spawn.cas.v1.chunk\0";
const DOMAIN_SYMLINK: &[u8] = b"spawn.cas.v1.symlink\0";
const DOMAIN_LOGICAL: &[u8] = b"spawn.cas.v1.logical\0";
const DOMAIN_FM_GRAPH: &[u8] = b"spawn.cas.v1.fm-graph\0";
const DOMAIN_SNAPSHOT: &[u8] = b"spawn.cas.v1.snapshot\0";
#[allow(dead_code)]
const DOMAIN_TRANSPORT: &[u8] = b"spawn.cas.v1.transport\0";

const PREFIX_BLOB: &str = "fm-cas/v1/blob/sha256/";
const PREFIX_CHUNK: &str = "fm-cas/v1/chunk/sha256/";
const PREFIX_SYMLINK: &str = "fm-cas/v1/symlink/sha256/";
const PREFIX_LOGICAL: &str = "fm-cas/v1/logical/sha256/";
const PREFIX_FM_GRAPH: &str = "fm-cas/v1/fm-graph/sha256/";
const PREFIX_SNAPSHOT: &str = "fm-cas/v1/snapshot/sha256/";

const CAS_USAGE: &str = "\
usage: spawn backup cas snapshot --source <dir> --store <dir> [--fm-graph <file>] [--parent <snapshot-key>] [--json]
       spawn backup cas restore --store <dir> --snapshot <snapshot-key> --into <dir> [--restore-fm <file>] [--json]
       spawn backup cas verify --store <dir> --snapshot <snapshot-key> [--json]
       spawn backup cas key-of --file <path> [--logical-path <rel-path>] [--json]";

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Structured errors returned by the recoverable CAS engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CasError {
    InvalidPath {
        path: String,
        reason: String,
    },
    InvalidKey {
        key: String,
        reason: String,
    },
    MissingObject {
        key: String,
        affected_paths: Vec<String>,
    },
    CorruptedObject {
        key: String,
        expected_digest: String,
        actual_digest: String,
        affected_paths: Vec<String>,
    },
    SizeMismatch {
        key: String,
        expected: u64,
        actual: u64,
        affected_paths: Vec<String>,
    },
    ManifestIntegrity {
        snapshot_key: String,
        reason: String,
    },
    FmGraphIntegrity {
        fm_graph_key: String,
        reason: String,
    },
    FmGraphConsistency {
        reason: String,
    },
    UnsafeRestoreTarget {
        path: String,
        reason: String,
    },
    Io {
        context: String,
        message: String,
    },
    Json {
        context: String,
        message: String,
    },
}

impl fmt::Display for CasError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPath { path, reason } => {
                write!(f, "invalid path {path:?}: {reason}")
            }
            Self::InvalidKey { key, reason } => {
                write!(f, "invalid key {key:?}: {reason}")
            }
            Self::MissingObject {
                key,
                affected_paths,
            } => write!(
                f,
                "missing object {key} (affected paths: {})",
                format_affected(affected_paths)
            ),
            Self::CorruptedObject {
                key,
                expected_digest,
                actual_digest,
                affected_paths,
            } => write!(
                f,
                "corrupted object {key}: expected digest {expected_digest}, actual {actual_digest} (affected paths: {})",
                format_affected(affected_paths)
            ),
            Self::SizeMismatch {
                key,
                expected,
                actual,
                affected_paths,
            } => write!(
                f,
                "size mismatch for {key}: expected {expected} bytes, actual {actual} bytes (affected paths: {})",
                format_affected(affected_paths)
            ),
            Self::ManifestIntegrity {
                snapshot_key,
                reason,
            } => write!(f, "manifest integrity failure for {snapshot_key}: {reason}"),
            Self::FmGraphIntegrity {
                fm_graph_key,
                reason,
            } => write!(f, "fm-graph integrity failure for {fm_graph_key}: {reason}"),
            Self::FmGraphConsistency { reason } => {
                write!(f, "fm-graph consistency failure: {reason}")
            }
            Self::UnsafeRestoreTarget { path, reason } => {
                write!(f, "unsafe restore target {path:?}: {reason}")
            }
            Self::Io { context, message } => write!(f, "io error ({context}): {message}"),
            Self::Json { context, message } => write!(f, "json error ({context}): {message}"),
        }
    }
}

impl std::error::Error for CasError {}

fn format_affected(paths: &[String]) -> String {
    if paths.is_empty() {
        "<none>".to_string()
    } else {
        paths.join(", ")
    }
}

fn io_err(context: impl Into<String>, error: &io::Error) -> CasError {
    CasError::Io {
        context: context.into(),
        message: error.to_string(),
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn is_valid_sha256_hex(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn parse_key_digest<'a>(key: &'a str, prefix: &str) -> Result<&'a str, CasError> {
    let Some(digest) = key.strip_prefix(prefix) else {
        return Err(CasError::InvalidKey {
            key: key.to_string(),
            reason: format!("expected prefix {prefix}"),
        });
    };
    if !is_valid_sha256_hex(digest) {
        return Err(CasError::InvalidKey {
            key: key.to_string(),
            reason: "digest must be 64 lowercase hex characters".to_string(),
        });
    }
    Ok(digest)
}

fn sha256_len_prefixed(domain: &[u8], bytes: &[u8]) -> String {
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(len.to_be_bytes());
    hasher.update(bytes);
    encode_hex(&hasher.finalize())
}

fn sha256_domain(domain: &[u8], bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(bytes);
    encode_hex(&hasher.finalize())
}

/// Deterministic content key for a regular file's full raw byte payload.
pub(crate) fn blob_key(raw_bytes: &[u8]) -> String {
    format!(
        "{PREFIX_BLOB}{}",
        sha256_len_prefixed(DOMAIN_BLOB, raw_bytes)
    )
}

/// Deterministic content key for a fixed-size file chunk.
pub(crate) fn chunk_key(chunk_bytes: &[u8]) -> String {
    format!(
        "{PREFIX_CHUNK}{}",
        sha256_len_prefixed(DOMAIN_CHUNK, chunk_bytes)
    )
}

/// Deterministic content key for a UTF-8 symlink target string.
pub(crate) fn symlink_blob_key(target: &str) -> String {
    format!(
        "{PREFIX_SYMLINK}{}",
        sha256_len_prefixed(DOMAIN_SYMLINK, target.as_bytes())
    )
}

/// Validate and canonicalize a repo-relative POSIX path.
///
/// Rejects empty paths, leading/trailing slashes, empty segments (`a//b`),
/// `.` / `..` components, NUL bytes, and `.git` components.
pub(crate) fn canonicalize_posix_path(raw: &str) -> Result<String, CasError> {
    if raw.is_empty() {
        return Err(CasError::InvalidPath {
            path: raw.to_string(),
            reason: "path must not be empty".to_string(),
        });
    }
    if raw.contains('\0') {
        return Err(CasError::InvalidPath {
            path: raw.to_string(),
            reason: "path must not contain NUL bytes".to_string(),
        });
    }
    if raw.starts_with('/') || raw.ends_with('/') {
        return Err(CasError::InvalidPath {
            path: raw.to_string(),
            reason: "path must be relative with no leading or trailing '/'".to_string(),
        });
    }
    let mut segments = Vec::new();
    for part in raw.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(CasError::InvalidPath {
                path: raw.to_string(),
                reason: format!("disallowed path component {part:?}"),
            });
        }
        if part == ".git" {
            return Err(CasError::InvalidPath {
                path: raw.to_string(),
                reason: "disallowed '.git' path component".to_string(),
            });
        }
        segments.push(part);
    }
    Ok(segments.join("/"))
}

/// Deterministic logical path/file identity, distinct from blob identity.
pub(crate) fn logical_file_id(canonical_posix_path: &str) -> Result<String, CasError> {
    let canonical = canonicalize_posix_path(canonical_posix_path)?;
    Ok(format!(
        "{PREFIX_LOGICAL}{}",
        sha256_domain(DOMAIN_LOGICAL, canonical.as_bytes())
    ))
}

/// Optional blinded remote transport key:
/// `HMAC-SHA256(master_key, "spawn.cas.v1.transport\0" || canonical_key)`.
///
/// Bridges `fm-cas/v1` deterministic inner keys with `spawn store`'s
/// zero-plaintext-oracle R2 naming.
#[allow(dead_code)]
pub(crate) fn transport_key(master_key: &[u8], canonical_key: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(master_key).expect("HMAC-SHA256 accepts any key length");
    mac.update(DOMAIN_TRANSPORT);
    mac.update(canonical_key.as_bytes());
    encode_hex(&mac.finalize().into_bytes())
}

/// Recursively serialize any serde value to canonical compact UTF-8 JSON bytes
/// with lexicographically sorted object keys.
pub(crate) fn canonical_json_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, CasError> {
    let raw = serde_json::to_value(value).map_err(|error| CasError::Json {
        context: "convert to json value".to_string(),
        message: error.to_string(),
    })?;
    let mut buf = Vec::new();
    write_canonical_value(&raw, &mut buf)?;
    Ok(buf)
}

fn write_canonical_value(value: &Value, out: &mut Vec<u8>) -> Result<(), CasError> {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(flag) => {
            if *flag {
                out.extend_from_slice(b"true");
            } else {
                out.extend_from_slice(b"false");
            }
        }
        Value::Number(num) => {
            out.extend_from_slice(num.to_string().as_bytes());
        }
        Value::String(text) => {
            serde_json::to_writer(&mut *out, text).map_err(|error| CasError::Json {
                context: "encode json string".to_string(),
                message: error.to_string(),
            })?;
        }
        Value::Array(items) => {
            out.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                write_canonical_value(item, out)?;
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let sorted: BTreeMap<&String, &Value> = map.iter().collect();
            out.push(b'{');
            for (index, (k, v)) in sorted.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                serde_json::to_writer(&mut *out, k).map_err(|error| CasError::Json {
                    context: "encode json object key".to_string(),
                    message: error.to_string(),
                })?;
                out.push(b':');
                write_canonical_value(v, out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}

/// Entry kind preserved in a snapshot manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EntryKind {
    Regular,
    Directory,
    Symlink,
}

/// Ordered fixed-size chunk reference for files exceeding `chunk_size`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ChunkRef {
    pub(crate) index: u32,
    pub(crate) offset: u64,
    pub(crate) length: u64,
    pub(crate) chunk_key: String,
}

/// Canonical entry record binding logical identity, content identity, restore
/// metadata, rename lineage, and feature-map associations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SnapshotEntry {
    pub(crate) path: String,
    pub(crate) logical_file_id: String,
    pub(crate) kind: EntryKind,
    pub(crate) mode: u32,
    pub(crate) mtime_ns: i128,
    pub(crate) size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) blob_key: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) chunks: Vec<ChunkRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) symlink_blob_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) symlink_target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) renamed_from: Option<String>,
    #[serde(default)]
    pub(crate) feature_ids: Vec<String>,
}

/// Special filesystem entry explicitly skipped during snapshot capture.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SkippedSpecial {
    pub(crate) path: String,
    pub(crate) kind: String,
}

/// Explicitly declared unsupported POSIX/filesystem semantics recorded in
/// every snapshot manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UnsupportedSemantics {
    pub(crate) posix_acls: String,
    pub(crate) extended_attributes: String,
    pub(crate) hardlink_inodes: String,
    pub(crate) special_files: String,
    pub(crate) uid_gid_ownership: String,
    pub(crate) ctime_and_birthtime: String,
}

impl UnsupportedSemantics {
    pub(crate) fn v1_defaults() -> Self {
        Self {
            posix_acls: "not_preserved".to_string(),
            extended_attributes: "not_preserved".to_string(),
            hardlink_inodes: "restored_as_independent_files_sharing_blob_key".to_string(),
            special_files: "skipped_and_recorded_in_skipped_specials".to_string(),
            uid_gid_ownership: "not_preserved_non_root_restore".to_string(),
            ctime_and_birthtime: "not_preserved".to_string(),
        }
    }
}

/// Compatibility declaration binding `fm-cas/v1` as strictly additive alongside
/// legacy backup/store areas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompatibilityMetadata {
    pub(crate) namespace: String,
    pub(crate) legacy_areas_untouched: Vec<String>,
}

impl CompatibilityMetadata {
    pub(crate) fn v1_defaults() -> Self {
        Self {
            namespace: SCHEMA_VERSION.to_string(),
            legacy_areas_untouched: vec![
                ".backup-sync-manifest.json".to_string(),
                "bundles/".to_string(),
                "postgres/".to_string(),
                "sessions/".to_string(),
                "transcripts/".to_string(),
            ],
        }
    }
}

fn default_schema_version() -> String {
    SCHEMA_VERSION.to_string()
}

/// Status of a file referenced by a feature in the feature-map graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FileAssocStatus {
    Present,
    External,
    Missing,
}

fn default_file_assoc_status() -> FileAssocStatus {
    FileAssocStatus::Present
}

/// Bidirectional file-to-feature association entry inside `FmFeature`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FmFileAssoc {
    pub(crate) path: String,
    #[serde(default = "default_file_assoc_status")]
    pub(crate) status: FileAssocStatus,
}

/// Typed directed relation between two features in the FM graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RelationKind {
    ParentOf,
    DependsOn,
    Implements,
    Verifies,
    RelatedTo,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FmRelation {
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) kind: RelationKind,
}

/// Boundary / exclusion rules captured alongside the FM graph.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FmBoundaries {
    #[serde(default)]
    pub(crate) include_roots: Vec<String>,
    #[serde(default)]
    pub(crate) exclude_globs: Vec<String>,
}

/// Provenance reference linking a feature to objectives, PRs, issues, or requests.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FmProvenanceRef {
    pub(crate) kind: String,
    pub(crate) reference: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) url: Option<String>,
}

/// Single feature node in the content-addressed FM graph snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FmFeature {
    pub(crate) id: String,
    pub(crate) title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) parent_id: Option<String>,
    #[serde(default)]
    pub(crate) files: Vec<FmFileAssoc>,
    #[serde(default)]
    pub(crate) provenance: Vec<FmProvenanceRef>,
}

/// Content-addressed Feature-Map graph snapshot object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FmGraphSnapshot {
    #[serde(default = "default_schema_version")]
    pub(crate) schema_version: String,
    pub(crate) features: Vec<FmFeature>,
    #[serde(default)]
    pub(crate) relations: Vec<FmRelation>,
    #[serde(default)]
    pub(crate) boundaries: FmBoundaries,
}

impl FmGraphSnapshot {
    /// Normalize paths and deterministically sort all nested collections so
    /// identical graphs always yield identical canonical JSON bytes.
    pub(crate) fn canonicalized(&self) -> Result<Self, CasError> {
        let mut graph = self.clone();
        if graph.schema_version != SCHEMA_VERSION {
            return Err(CasError::FmGraphConsistency {
                reason: format!(
                    "unsupported fm-graph schema_version {:?} (expected {SCHEMA_VERSION:?})",
                    graph.schema_version
                ),
            });
        }
        for feature in &mut graph.features {
            for assoc in &mut feature.files {
                match assoc.status {
                    FileAssocStatus::Present => {
                        assoc.path = canonicalize_posix_path(&assoc.path)?;
                    }
                    FileAssocStatus::External | FileAssocStatus::Missing => {
                        if assoc.path.is_empty() || assoc.path.contains('\0') {
                            return Err(CasError::InvalidPath {
                                path: assoc.path.clone(),
                                reason: "file association path must not be empty or contain NUL"
                                    .to_string(),
                            });
                        }
                    }
                }
            }
            feature.files.sort();
            feature.files.dedup();
            feature.provenance.sort();
            feature.provenance.dedup();
        }
        graph.features.sort_by(|a, b| a.id.cmp(&b.id));
        graph.relations.sort();
        graph.relations.dedup();
        graph.boundaries.include_roots.sort();
        graph.boundaries.include_roots.dedup();
        graph.boundaries.exclude_globs.sort();
        graph.boundaries.exclude_globs.dedup();
        Ok(graph)
    }

    /// Validate structural invariants of the FM graph (unique ids, no dangling
    /// relations, no self-loops, no parent/`depends_on` cycles, and no
    /// conflicting file association statuses).
    pub(crate) fn validate_structure(&self) -> Result<(), CasError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(CasError::FmGraphConsistency {
                reason: format!(
                    "unsupported fm-graph schema_version {:?} (expected {SCHEMA_VERSION:?})",
                    self.schema_version
                ),
            });
        }
        let mut feature_ids = BTreeSet::new();
        for feature in &self.features {
            if feature.id.trim().is_empty() {
                return Err(CasError::FmGraphConsistency {
                    reason: "feature id must not be empty".to_string(),
                });
            }
            if !feature_ids.insert(feature.id.clone()) {
                return Err(CasError::FmGraphConsistency {
                    reason: format!("duplicate feature id {:?}", feature.id),
                });
            }
            validate_feature_files(feature)?;
        }

        let mut parent_edges: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
        let mut depends_edges: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();

        for feature in &self.features {
            if let Some(parent) = &feature.parent_id {
                if parent == &feature.id {
                    return Err(CasError::FmGraphConsistency {
                        reason: format!("feature {:?} cannot be its own parent", feature.id),
                    });
                }
                if !feature_ids.contains(parent) {
                    return Err(CasError::FmGraphConsistency {
                        reason: format!(
                            "feature {:?} references missing parent {:?}",
                            feature.id, parent
                        ),
                    });
                }
                parent_edges
                    .entry(parent.as_str())
                    .or_default()
                    .insert(feature.id.as_str());
            }
        }

        for relation in &self.relations {
            if !feature_ids.contains(&relation.from) {
                return Err(CasError::FmGraphConsistency {
                    reason: format!(
                        "dangling relation source {:?} (target {:?})",
                        relation.from, relation.to
                    ),
                });
            }
            if !feature_ids.contains(&relation.to) {
                return Err(CasError::FmGraphConsistency {
                    reason: format!(
                        "dangling relation target {:?} (source {:?})",
                        relation.to, relation.from
                    ),
                });
            }
            if relation.from == relation.to {
                return Err(CasError::FmGraphConsistency {
                    reason: format!(
                        "self-loop relation on feature {:?} ({:?})",
                        relation.from, relation.kind
                    ),
                });
            }
            match relation.kind {
                RelationKind::ParentOf => {
                    parent_edges
                        .entry(relation.from.as_str())
                        .or_default()
                        .insert(relation.to.as_str());
                }
                RelationKind::DependsOn => {
                    depends_edges
                        .entry(relation.from.as_str())
                        .or_default()
                        .insert(relation.to.as_str());
                }
                RelationKind::Implements | RelationKind::Verifies | RelationKind::RelatedTo => {}
            }
        }

        check_acyclic("parent", &feature_ids, &parent_edges)?;
        check_acyclic("depends_on", &feature_ids, &depends_edges)?;
        Ok(())
    }
}

fn validate_feature_files(feature: &FmFeature) -> Result<(), CasError> {
    let mut seen_file_statuses: BTreeMap<&str, FileAssocStatus> = BTreeMap::new();
    for assoc in &feature.files {
        match assoc.status {
            FileAssocStatus::Present => {
                let canonical = canonicalize_posix_path(&assoc.path)?;
                if canonical != assoc.path {
                    return Err(CasError::FmGraphConsistency {
                        reason: format!(
                            "feature {:?} has non-canonical present path {:?}",
                            feature.id, assoc.path
                        ),
                    });
                }
            }
            FileAssocStatus::External | FileAssocStatus::Missing => {
                if assoc.path.is_empty() || assoc.path.contains('\0') {
                    return Err(CasError::InvalidPath {
                        path: assoc.path.clone(),
                        reason: "file association path must not be empty or contain NUL"
                            .to_string(),
                    });
                }
            }
        }
        if let Some(prev_status) = seen_file_statuses.insert(assoc.path.as_str(), assoc.status)
            && prev_status != assoc.status
        {
            return Err(CasError::FmGraphConsistency {
                reason: format!(
                    "feature {:?} lists path {:?} with conflicting statuses ({prev_status:?} vs {:?})",
                    feature.id, assoc.path, assoc.status
                ),
            });
        }
    }
    Ok(())
}

fn check_acyclic(
    label: &str,
    nodes: &BTreeSet<String>,
    edges: &BTreeMap<&str, BTreeSet<&str>>,
) -> Result<(), CasError> {
    // 0 = unvisited, 1 = visiting, 2 = visited
    let mut state: BTreeMap<&str, u8> = BTreeMap::new();
    for node in nodes {
        let key = node.as_str();
        if state.get(key).copied().unwrap_or(0) == 0 {
            dfs_cycle(label, key, edges, &mut state)?;
        }
    }
    Ok(())
}

fn dfs_cycle<'a>(
    label: &str,
    node: &'a str,
    edges: &BTreeMap<&'a str, BTreeSet<&'a str>>,
    state: &mut BTreeMap<&'a str, u8>,
) -> Result<(), CasError> {
    state.insert(node, 1);
    if let Some(neighbors) = edges.get(node) {
        for &next in neighbors {
            match state.get(next).copied().unwrap_or(0) {
                1 => {
                    return Err(CasError::FmGraphConsistency {
                        reason: format!(
                            "cycle detected in {label} edges involving {node:?} -> {next:?}"
                        ),
                    });
                }
                0 => dfs_cycle(label, next, edges, state)?,
                _ => {}
            }
        }
    }
    state.insert(node, 2);
    Ok(())
}

/// Deterministic canonical key for an `FmGraphSnapshot`.
pub(crate) fn fm_graph_key(graph: &FmGraphSnapshot) -> Result<String, CasError> {
    let canonical = graph.canonicalized()?;
    let bytes = canonical_json_bytes(&canonical)?;
    Ok(format!(
        "{PREFIX_FM_GRAPH}{}",
        sha256_domain(DOMAIN_FM_GRAPH, &bytes)
    ))
}

/// Deterministic manifest body hashed to produce `snapshot_key`.
///
/// Intentionally excludes wall-clock timestamps so identical source trees,
/// metadata, parent, and FM graphs always hash to the exact same `snapshot_key`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SnapshotManifestBody {
    pub(crate) schema_version: String,
    pub(crate) chunk_size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) parent_snapshot_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) fm_graph_key: Option<String>,
    pub(crate) entries: Vec<SnapshotEntry>,
    #[serde(default)]
    pub(crate) skipped_specials: Vec<SkippedSpecial>,
    pub(crate) unsupported_semantics: UnsupportedSemantics,
    pub(crate) compatibility: CompatibilityMetadata,
}

impl SnapshotManifestBody {
    pub(crate) fn canonicalized(&self) -> Self {
        let mut body = self.clone();
        for entry in &mut body.entries {
            entry.feature_ids.sort();
            entry.feature_ids.dedup();
        }
        body.entries.sort_by(|a, b| a.path.cmp(&b.path));
        body.skipped_specials.sort();
        body.skipped_specials.dedup();
        body.compatibility.legacy_areas_untouched.sort();
        body.compatibility.legacy_areas_untouched.dedup();
        body
    }
}

/// Deterministic key for a `SnapshotManifestBody`.
pub(crate) fn snapshot_key(body: &SnapshotManifestBody) -> Result<String, CasError> {
    let canonical = body.canonicalized();
    let bytes = canonical_json_bytes(&canonical)?;
    Ok(format!(
        "{PREFIX_SNAPSHOT}{}",
        sha256_domain(DOMAIN_SNAPSHOT, &bytes)
    ))
}

/// Stored immutable snapshot manifest object.
///
/// Uses explicit fields (rather than `#[serde(flatten)]`) so `i128`
/// nanosecond timestamps round-trip directly through `serde_json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SnapshotManifest {
    pub(crate) snapshot_key: String,
    pub(crate) schema_version: String,
    pub(crate) chunk_size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) parent_snapshot_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) fm_graph_key: Option<String>,
    pub(crate) entries: Vec<SnapshotEntry>,
    #[serde(default)]
    pub(crate) skipped_specials: Vec<SkippedSpecial>,
    pub(crate) unsupported_semantics: UnsupportedSemantics,
    pub(crate) compatibility: CompatibilityMetadata,
}

impl SnapshotManifest {
    pub(crate) fn from_body(snapshot_key: String, body: SnapshotManifestBody) -> Self {
        Self {
            snapshot_key,
            schema_version: body.schema_version,
            chunk_size: body.chunk_size,
            parent_snapshot_key: body.parent_snapshot_key,
            fm_graph_key: body.fm_graph_key,
            entries: body.entries,
            skipped_specials: body.skipped_specials,
            unsupported_semantics: body.unsupported_semantics,
            compatibility: body.compatibility,
        }
    }

    pub(crate) fn body(&self) -> SnapshotManifestBody {
        SnapshotManifestBody {
            schema_version: self.schema_version.clone(),
            chunk_size: self.chunk_size,
            parent_snapshot_key: self.parent_snapshot_key.clone(),
            fm_graph_key: self.fm_graph_key.clone(),
            entries: self.entries.clone(),
            skipped_specials: self.skipped_specials.clone(),
            unsupported_semantics: self.unsupported_semantics.clone(),
            compatibility: self.compatibility.clone(),
        }
    }
}

/// Mutable index entry stored in `<store>/fm-cas/v1/snapshots.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SnapshotIndexEntry {
    pub(crate) snapshot_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) parent_snapshot_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) fm_graph_key: Option<String>,
    pub(crate) entry_count: usize,
    pub(crate) total_bytes: u64,
    pub(crate) recorded_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SnapshotIndex {
    pub(crate) schema_version: String,
    pub(crate) snapshots: Vec<SnapshotIndexEntry>,
}

impl Default for SnapshotIndex {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION.to_string(),
            snapshots: Vec::new(),
        }
    }
}

/// Summary returned by [`CasStore::create_snapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SnapshotSummary {
    pub(crate) schema_version: String,
    pub(crate) snapshot_key: String,
    pub(crate) parent_snapshot_key: Option<String>,
    pub(crate) fm_graph_key: Option<String>,
    pub(crate) entry_count: usize,
    pub(crate) total_bytes: u64,
    pub(crate) blobs_written: usize,
    pub(crate) blobs_deduplicated: usize,
    pub(crate) chunks_written: usize,
    pub(crate) chunks_deduplicated: usize,
    pub(crate) renamed_count: usize,
    pub(crate) skipped_specials: Vec<SkippedSpecial>,
}

/// Summary returned by [`CasStore::verify_snapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct VerifySummary {
    pub(crate) schema_version: String,
    pub(crate) snapshot_key: String,
    pub(crate) fm_graph_key: Option<String>,
    pub(crate) entries_verified: usize,
    pub(crate) blobs_verified: usize,
    pub(crate) chunks_verified: usize,
    pub(crate) symlinks_verified: usize,
    pub(crate) features_verified: usize,
}

/// Summary returned by [`CasStore::restore_snapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RestoreSummary {
    pub(crate) schema_version: String,
    pub(crate) snapshot_key: String,
    pub(crate) fm_graph_key: Option<String>,
    pub(crate) into: String,
    pub(crate) entries_restored: usize,
    pub(crate) fm_graph_restored_to: Option<String>,
}

/// Result returned by [`inspect_key_of`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct KeyOfReport {
    pub(crate) schema_version: String,
    pub(crate) kind: EntryKind,
    pub(crate) size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) blob_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) symlink_blob_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) symlink_target: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) chunks: Vec<ChunkRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) logical_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) logical_file_id: Option<String>,
}

/// Write outcome for idempotent content-addressed objects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteOutcome {
    Written,
    Deduplicated,
}

/// Guard that cleans up a staging temp file on error/drop unless committed.
struct TempFileGuard {
    path: PathBuf,
    armed: bool,
}

impl TempFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Exclusive advisory lock on `<store>/fm-cas/v1/.cas.lock`.
struct CasLock {
    file: File,
}

impl CasLock {
    fn acquire(cas_root: &Path) -> Result<Self, CasError> {
        let lock_path = cas_root.join(".cas.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|error| io_err(format!("open {}", lock_path.display()), &error))?;
        file.lock()
            .map_err(|error| io_err(format!("lock {}", lock_path.display()), &error))?;
        Ok(Self { file })
    }
}

impl Drop for CasLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Content-addressed recoverable store rooted at `<store_root>/fm-cas/v1/`.
#[derive(Debug, Clone)]
pub(crate) struct CasStore {
    cas_root: PathBuf,
    chunk_size: usize,
}

impl CasStore {
    /// Open or initialize a `fm-cas/v1` store with the default 4 MiB chunk size.
    pub(crate) fn open(store_root: &Path) -> Result<Self, CasError> {
        Self::open_with_chunk_size(store_root, DEFAULT_CHUNK_SIZE)
    }

    /// Open an existing `fm-cas/v1` store in strictly read-only mode without
    /// creating any directories or mutating `store_root`.
    ///
    /// Suitable for `verify` and `restore` against read-only backup mounts or
    /// immutable storage directories.
    pub(crate) fn open_read_only(store_root: &Path) -> Self {
        Self {
            cas_root: store_root.join("fm-cas").join("v1"),
            chunk_size: DEFAULT_CHUNK_SIZE,
        }
    }

    /// Open or initialize a `fm-cas/v1` store with a custom chunk size (useful
    /// in tests to exercise multi-chunk logic cleanly on small payloads).
    pub(crate) fn open_with_chunk_size(
        store_root: &Path,
        chunk_size: usize,
    ) -> Result<Self, CasError> {
        if chunk_size == 0 {
            return Err(CasError::InvalidKey {
                key: "chunk_size".to_string(),
                reason: "chunk_size must be greater than zero".to_string(),
            });
        }
        let cas_root = store_root.join("fm-cas").join("v1");
        for sub in ["blobs", "chunks", "graphs", "snapshots", "staging"] {
            let dir = cas_root.join(sub);
            fs::create_dir_all(&dir)
                .map_err(|error| io_err(format!("create {}", dir.display()), &error))?;
        }
        Ok(Self {
            cas_root,
            chunk_size,
        })
    }

    pub(crate) fn blob_path(&self, key: &str) -> Result<PathBuf, CasError> {
        let digest = parse_key_digest(key, PREFIX_BLOB)?;
        Ok(self.cas_root.join("blobs").join(&digest[..2]).join(digest))
    }

    pub(crate) fn symlink_blob_path(&self, key: &str) -> Result<PathBuf, CasError> {
        let digest = parse_key_digest(key, PREFIX_SYMLINK)?;
        Ok(self
            .cas_root
            .join("blobs")
            .join("sym")
            .join(&digest[..2])
            .join(digest))
    }

    pub(crate) fn chunk_path(&self, key: &str) -> Result<PathBuf, CasError> {
        let digest = parse_key_digest(key, PREFIX_CHUNK)?;
        Ok(self.cas_root.join("chunks").join(&digest[..2]).join(digest))
    }

    pub(crate) fn fm_graph_path(&self, key: &str) -> Result<PathBuf, CasError> {
        let digest = parse_key_digest(key, PREFIX_FM_GRAPH)?;
        Ok(self
            .cas_root
            .join("graphs")
            .join(&digest[..2])
            .join(format!("{digest}.json")))
    }

    pub(crate) fn snapshot_path(&self, key: &str) -> Result<PathBuf, CasError> {
        let digest = parse_key_digest(key, PREFIX_SNAPSHOT)?;
        Ok(self
            .cas_root
            .join("snapshots")
            .join(&digest[..2])
            .join(format!("{digest}.json")))
    }

    pub(crate) fn index_path(&self) -> PathBuf {
        self.cas_root.join("snapshots.json")
    }

    pub(crate) fn staging_dir(&self) -> PathBuf {
        self.cas_root.join("staging")
    }

    /// Safely sweep stray `.tmp-*` files only inside `fm-cas/v1/staging/`.
    ///
    /// Never follows symlinks and skips staging files whose owning `<pid>`
    /// is still running on this host so concurrent active writers are never
    /// disturbed.
    #[allow(dead_code)]
    pub(crate) fn sweep_staging(&self) -> Result<usize, CasError> {
        let staging = self.staging_dir();
        match fs::symlink_metadata(&staging) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Ok(0),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(io_err(format!("lstat {}", staging.display()), &error)),
        }
        let mut removed = 0usize;
        let entries = fs::read_dir(&staging)
            .map_err(|error| io_err(format!("read_dir {}", staging.display()), &error))?;
        for entry in entries {
            let entry = entry
                .map_err(|error| io_err(format!("dir entry {}", staging.display()), &error))?;
            let file_type = match entry.file_type() {
                Ok(ft) => ft,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(io_err(
                        format!("file_type {}", entry.path().display()),
                        &error,
                    ));
                }
            };
            if !file_type.is_file() {
                continue;
            }
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();
            let Some(rest) = name.strip_prefix(".tmp-") else {
                continue;
            };
            if let Some((pid_str, _seq)) = rest.split_once('-')
                && let Ok(pid) = pid_str.parse::<u32>()
                && is_process_alive(pid)
            {
                continue;
            }
            match fs::remove_file(entry.path()) {
                Ok(()) => removed += 1,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(io_err(format!("remove {}", entry.path().display()), &error));
                }
            }
        }
        Ok(removed)
    }

    /// Create an immutable content-addressed snapshot of `source_dir`.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn create_snapshot(
        &self,
        source_dir: &Path,
        fm_graph: Option<&FmGraphSnapshot>,
        parent_snapshot_key: Option<&str>,
    ) -> Result<SnapshotSummary, CasError> {
        let source_meta = fs::symlink_metadata(source_dir)
            .map_err(|error| io_err(format!("stat {}", source_dir.display()), &error))?;
        if !source_meta.is_dir() {
            return Err(CasError::InvalidPath {
                path: source_dir.display().to_string(),
                reason: "source must be a directory".to_string(),
            });
        }

        let parent_manifest = match parent_snapshot_key {
            Some(parent_key) => Some(self.load_and_verify_manifest(parent_key)?),
            None => None,
        };

        let canonical_graph = match fm_graph {
            Some(graph) => {
                let canonical = graph.canonicalized()?;
                canonical.validate_structure()?;
                Some(canonical)
            }
            None => None,
        };

        let mut file_to_features: BTreeMap<String, Vec<String>> = BTreeMap::new();
        if let Some(graph) = &canonical_graph {
            for feature in &graph.features {
                for assoc in &feature.files {
                    if assoc.status == FileAssocStatus::Present {
                        file_to_features
                            .entry(assoc.path.clone())
                            .or_default()
                            .push(feature.id.clone());
                    }
                }
            }
            for ids in file_to_features.values_mut() {
                ids.sort();
                ids.dedup();
            }
        }

        let mut entries = Vec::new();
        let mut skipped_specials = Vec::new();
        let mut stats = SnapshotWriteStats::default();

        self.walk_source(
            source_dir,
            source_dir,
            &file_to_features,
            &mut entries,
            &mut skipped_specials,
            &mut stats,
        )?;

        entries.sort_by(|a, b| a.path.cmp(&b.path));
        skipped_specials.sort();
        skipped_specials.dedup();

        // Detect renames against parent snapshot when logical_file_id changes
        // while content identity (blob_key / symlink_blob_key) stays identical.
        let renamed_count = if let Some(parent) = &parent_manifest {
            apply_rename_detection(&parent.entries, &mut entries)
        } else {
            0
        };

        // Verify FM graph consistency against the collected entries BEFORE
        // writing the manifest so an inconsistent graph never commits.
        if let Some(graph) = &canonical_graph {
            validate_fm_graph_against_entries(graph, &entries)?;
        }

        let stored_fm_graph_key = if let Some(graph) = &canonical_graph {
            let key = fm_graph_key(graph)?;
            let bytes = canonical_json_bytes(graph)?;
            let dest = self.fm_graph_path(&key)?;
            let digest = parse_key_digest(&key, PREFIX_FM_GRAPH)?;
            self.write_object_once(&dest, &bytes, &key, digest, |disk_bytes| {
                sha256_domain(DOMAIN_FM_GRAPH, disk_bytes)
            })?;
            Some(key)
        } else {
            None
        };

        let body = SnapshotManifestBody {
            schema_version: SCHEMA_VERSION.to_string(),
            chunk_size: u64::try_from(self.chunk_size).unwrap_or(u64::MAX),
            parent_snapshot_key: parent_snapshot_key.map(str::to_string),
            fm_graph_key: stored_fm_graph_key.clone(),
            entries,
            skipped_specials: skipped_specials.clone(),
            unsupported_semantics: UnsupportedSemantics::v1_defaults(),
            compatibility: CompatibilityMetadata::v1_defaults(),
        }
        .canonicalized();

        let key = snapshot_key(&body)?;
        let manifest = SnapshotManifest::from_body(key.clone(), body);
        let manifest_bytes = canonical_json_bytes(&manifest)?;
        let manifest_dest = self.snapshot_path(&key)?;
        let expected_manifest_bytes_digest = sha256_domain(DOMAIN_SNAPSHOT, &manifest_bytes);
        self.write_object_once(
            &manifest_dest,
            &manifest_bytes,
            &key,
            &expected_manifest_bytes_digest,
            |disk_bytes| sha256_domain(DOMAIN_SNAPSHOT, disk_bytes),
        )?;

        let total_bytes: u64 = manifest
            .entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::Regular)
            .map(|entry| entry.size)
            .sum();

        self.record_snapshot_index(&SnapshotIndexEntry {
            snapshot_key: key.clone(),
            parent_snapshot_key: manifest.parent_snapshot_key.clone(),
            fm_graph_key: stored_fm_graph_key.clone(),
            entry_count: manifest.entries.len(),
            total_bytes,
            recorded_at_ms: now_unix_ms(),
        })?;

        Ok(SnapshotSummary {
            schema_version: SCHEMA_VERSION.to_string(),
            snapshot_key: key,
            parent_snapshot_key: manifest.parent_snapshot_key,
            fm_graph_key: stored_fm_graph_key,
            entry_count: manifest.entries.len(),
            total_bytes,
            blobs_written: stats.blobs_written,
            blobs_deduplicated: stats.blobs_deduplicated,
            chunks_written: stats.chunks_written,
            chunks_deduplicated: stats.chunks_deduplicated,
            renamed_count,
            skipped_specials,
        })
    }

    /// Load and verify a snapshot manifest, its FM graph (if present), and
    /// every referenced blob, chunk, and symlink payload.
    pub(crate) fn verify_snapshot(&self, snap_key: &str) -> Result<VerifySummary, CasError> {
        let (manifest, graph, summary) = self.verify_snapshot_internal(snap_key)?;
        let _ = (manifest, graph);
        Ok(summary)
    }

    /// Restore a verified snapshot into an isolated empty directory `into_dir`.
    ///
    /// Performs full pre-flight verification (manifest, FM graph, every blob
    /// and chunk, every path / symlink-chain safety rule, and `--restore-fm`
    /// target checks) before writing any output files. If any mid-restore
    /// error occurs, rolls back partial output so no half-restored tree is
    /// left behind.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn restore_snapshot(
        &self,
        snap_key: &str,
        into_dir: &Path,
        restore_fm_path: Option<&Path>,
    ) -> Result<RestoreSummary, CasError> {
        let (manifest, fm_graph, _verify_summary) = self.verify_snapshot_internal(snap_key)?;

        // Pre-flight safety validation of every entry path and symlink chain.
        for entry in &manifest.entries {
            let _ = canonicalize_posix_path(&entry.path)?;
        }
        validate_all_symlink_targets(&manifest.entries)?;

        // Pre-flight validation of optional `--restore-fm` output path BEFORE
        // creating `into_dir` or writing any files.
        if let Some(fm_out) = restore_fm_path {
            if fm_graph.is_none() {
                return Err(CasError::FmGraphConsistency {
                    reason: format!(
                        "--restore-fm requested ({}) but snapshot {snap_key} has no fm_graph_key",
                        fm_out.display()
                    ),
                });
            }
            match fs::symlink_metadata(fm_out) {
                Ok(_) => {
                    return Err(CasError::UnsafeRestoreTarget {
                        path: fm_out.display().to_string(),
                        reason: "refusing to overwrite existing FM graph output file".to_string(),
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(io_err(format!("lstat {}", fm_out.display()), &error));
                }
            }
            if paths_overlap(into_dir, fm_out) {
                return Err(CasError::UnsafeRestoreTarget {
                    path: fm_out.display().to_string(),
                    reason: "refusing to place --restore-fm output inside --into restore directory"
                        .to_string(),
                });
            }
        }

        // Prepare and verify isolated empty destination directory.
        let created_root = prepare_empty_restore_dir(into_dir)?;
        let mut rollback = RestoreRollbackGuard::new(into_dir.to_path_buf(), created_root);

        // 1. Create all directories (sorted shallow-to-deep).
        let mut dirs: Vec<&SnapshotEntry> = manifest
            .entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::Directory)
            .collect();
        dirs.sort_by(|a, b| {
            a.path
                .matches('/')
                .count()
                .cmp(&b.path.matches('/').count())
                .then_with(|| a.path.cmp(&b.path))
        });
        for dir_entry in &dirs {
            let dest = safe_join(into_dir, &dir_entry.path)?;
            fs::create_dir_all(&dest)
                .map_err(|error| io_err(format!("mkdir {}", dest.display()), &error))?;
        }

        // 2. Restore regular files (single-blob or multi-chunk reassembly).
        for entry in manifest
            .entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::Regular)
        {
            let dest = safe_join(into_dir, &entry.path)?;
            if let Some(parent) = dest.parent() {
                ensure_no_symlink_components(into_dir, parent)?;
                fs::create_dir_all(parent)
                    .map_err(|error| io_err(format!("mkdir {}", parent.display()), &error))?;
            }
            let mut out_file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&dest)
                .map_err(|error| io_err(format!("create {}", dest.display()), &error))?;

            if entry.chunks.is_empty() {
                let key = entry
                    .blob_key
                    .as_deref()
                    .ok_or_else(|| CasError::ManifestIntegrity {
                        snapshot_key: snap_key.to_string(),
                        reason: format!("regular entry {:?} missing blob_key", entry.path),
                    })?;
                let bytes =
                    self.read_verified_blob(key, entry.size, std::slice::from_ref(&entry.path))?;
                out_file
                    .write_all(&bytes)
                    .map_err(|error| io_err(format!("write {}", dest.display()), &error))?;
            } else {
                let whole_key =
                    entry
                        .blob_key
                        .as_deref()
                        .ok_or_else(|| CasError::ManifestIntegrity {
                            snapshot_key: snap_key.to_string(),
                            reason: format!("chunked entry {:?} missing blob_key", entry.path),
                        })?;
                let expected_whole_digest = parse_key_digest(whole_key, PREFIX_BLOB)?;
                let mut whole_hasher = Sha256::new();
                whole_hasher.update(DOMAIN_BLOB);
                whole_hasher.update(entry.size.to_be_bytes());
                let mut written = 0u64;
                for chunk in &entry.chunks {
                    let chunk_bytes = self.read_verified_chunk(
                        &chunk.chunk_key,
                        chunk.length,
                        std::slice::from_ref(&entry.path),
                    )?;
                    whole_hasher.update(&chunk_bytes);
                    out_file
                        .write_all(&chunk_bytes)
                        .map_err(|error| io_err(format!("write {}", dest.display()), &error))?;
                    written = written.saturating_add(chunk.length);
                }
                if written != entry.size {
                    return Err(CasError::SizeMismatch {
                        key: whole_key.to_string(),
                        expected: entry.size,
                        actual: written,
                        affected_paths: vec![entry.path.clone()],
                    });
                }
                let actual_whole_digest = encode_hex(&whole_hasher.finalize());
                if actual_whole_digest != expected_whole_digest {
                    return Err(CasError::CorruptedObject {
                        key: whole_key.to_string(),
                        expected_digest: expected_whole_digest.to_string(),
                        actual_digest: actual_whole_digest,
                        affected_paths: vec![entry.path.clone()],
                    });
                }
            }

            out_file
                .sync_all()
                .map_err(|error| io_err(format!("sync {}", dest.display()), &error))?;
            // Set mtime before restricting permissions (e.g. 0o444).
            set_file_mtime(&out_file, entry.mtime_ns, &dest)?;
            set_file_mode(&dest, entry.mode)?;
        }

        // 3. Restore symlinks after regular files so no file write can ever
        // traverse a newly created symlink.
        for entry in manifest
            .entries
            .iter()
            .filter(|entry| entry.kind == EntryKind::Symlink)
        {
            let dest = safe_join(into_dir, &entry.path)?;
            if let Some(parent) = dest.parent() {
                ensure_no_symlink_components(into_dir, parent)?;
                fs::create_dir_all(parent)
                    .map_err(|error| io_err(format!("mkdir {}", parent.display()), &error))?;
            }
            let target =
                entry
                    .symlink_target
                    .as_deref()
                    .ok_or_else(|| CasError::ManifestIntegrity {
                        snapshot_key: snap_key.to_string(),
                        reason: format!("symlink entry {:?} missing target", entry.path),
                    })?;
            create_symlink(target, &dest)?;
            set_path_mtime_nofollow(&dest, entry.mtime_ns)?;
        }

        // 4. Apply directory mtimes and modes in reverse depth order so child
        // creation does not clobber mtimes and restrictive modes (e.g. 0o555)
        // do not block creating children or updating timestamps.
        dirs.sort_by(|a, b| {
            b.path
                .matches('/')
                .count()
                .cmp(&a.path.matches('/').count())
                .then_with(|| b.path.cmp(&a.path))
        });
        for dir_entry in &dirs {
            let dest = safe_join(into_dir, &dir_entry.path)?;
            set_path_mtime_nofollow(&dest, dir_entry.mtime_ns)?;
            set_file_mode(&dest, dir_entry.mode)?;
        }

        // 5. Optionally restore the FM graph snapshot JSON alongside files.
        let fm_graph_restored_to = if let Some(fm_out) = restore_fm_path {
            let Some(graph) = fm_graph else {
                return Err(CasError::FmGraphConsistency {
                    reason: format!(
                        "--restore-fm requested ({}) but snapshot {snap_key} has no fm_graph_key",
                        fm_out.display()
                    ),
                });
            };
            if let Some(parent) = fm_out.parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent)
                    .map_err(|error| io_err(format!("mkdir {}", parent.display()), &error))?;
            }
            let bytes = canonical_json_bytes(&graph)?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(fm_out)
                .map_err(|error| io_err(format!("create {}", fm_out.display()), &error))?;
            rollback.track_fm_file(fm_out.to_path_buf());
            file.write_all(&bytes)
                .map_err(|error| io_err(format!("write {}", fm_out.display()), &error))?;
            file.sync_all()
                .map_err(|error| io_err(format!("sync {}", fm_out.display()), &error))?;
            Some(fm_out.display().to_string())
        } else {
            None
        };

        rollback.disarm();

        Ok(RestoreSummary {
            schema_version: SCHEMA_VERSION.to_string(),
            snapshot_key: manifest.snapshot_key,
            fm_graph_key: manifest.fm_graph_key,
            into: into_dir.display().to_string(),
            entries_restored: manifest.entries.len(),
            fm_graph_restored_to,
        })
    }

    /// Read the snapshot index (`fm-cas/v1/snapshots.json`).
    pub(crate) fn read_index(&self) -> Result<SnapshotIndex, CasError> {
        let path = self.index_path();
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_file() => {}
            Ok(_) => {
                return Err(CasError::ManifestIntegrity {
                    snapshot_key: "snapshots.json".to_string(),
                    reason: format!("index path {} is not a regular file", path.display()),
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(SnapshotIndex::default());
            }
            Err(error) => return Err(io_err(format!("lstat {}", path.display()), &error)),
        }
        let bytes =
            fs::read(&path).map_err(|error| io_err(format!("read {}", path.display()), &error))?;
        let index: SnapshotIndex =
            serde_json::from_slice(&bytes).map_err(|error| CasError::Json {
                context: format!("parse {}", path.display()),
                message: error.to_string(),
            })?;
        if index.schema_version != SCHEMA_VERSION {
            return Err(CasError::ManifestIntegrity {
                snapshot_key: "snapshots.json".to_string(),
                reason: format!(
                    "unexpected index schema_version {:?} (expected {SCHEMA_VERSION:?})",
                    index.schema_version
                ),
            });
        }
        Ok(index)
    }

    /// Load a manifest and verify its self-hash and exact canonical bytes
    /// against `snap_key`.
    pub(crate) fn load_and_verify_manifest(
        &self,
        snap_key: &str,
    ) -> Result<SnapshotManifest, CasError> {
        let expected_digest = parse_key_digest(snap_key, PREFIX_SNAPSHOT)?;
        let path = self.snapshot_path(snap_key)?;
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_file() => {}
            Ok(_) => {
                return Err(CasError::ManifestIntegrity {
                    snapshot_key: snap_key.to_string(),
                    reason: format!("manifest path {} is not a regular file", path.display()),
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(CasError::MissingObject {
                    key: snap_key.to_string(),
                    affected_paths: Vec::new(),
                });
            }
            Err(error) => return Err(io_err(format!("lstat {}", path.display()), &error)),
        }
        let bytes =
            fs::read(&path).map_err(|error| io_err(format!("read {}", path.display()), &error))?;
        let manifest: SnapshotManifest =
            serde_json::from_slice(&bytes).map_err(|error| CasError::ManifestIntegrity {
                snapshot_key: snap_key.to_string(),
                reason: format!("invalid manifest json: {error}"),
            })?;
        if manifest.schema_version != SCHEMA_VERSION {
            return Err(CasError::ManifestIntegrity {
                snapshot_key: snap_key.to_string(),
                reason: format!("unexpected schema_version {:?}", manifest.schema_version),
            });
        }
        if manifest.chunk_size == 0 {
            return Err(CasError::ManifestIntegrity {
                snapshot_key: snap_key.to_string(),
                reason: "manifest chunk_size must be greater than zero".to_string(),
            });
        }
        if manifest.snapshot_key != snap_key {
            return Err(CasError::ManifestIntegrity {
                snapshot_key: snap_key.to_string(),
                reason: format!(
                    "manifest snapshot_key field {:?} does not match requested key",
                    manifest.snapshot_key
                ),
            });
        }
        let canonical_body = manifest.body().canonicalized();
        let recomputed = snapshot_key(&canonical_body)?;
        let actual_digest = parse_key_digest(&recomputed, PREFIX_SNAPSHOT)?;
        if actual_digest != expected_digest {
            return Err(CasError::CorruptedObject {
                key: snap_key.to_string(),
                expected_digest: expected_digest.to_string(),
                actual_digest: actual_digest.to_string(),
                affected_paths: Vec::new(),
            });
        }
        let canonical_manifest = SnapshotManifest::from_body(snap_key.to_string(), canonical_body);
        let expected_canonical_bytes = canonical_json_bytes(&canonical_manifest)?;
        if bytes != expected_canonical_bytes {
            return Err(CasError::ManifestIntegrity {
                snapshot_key: snap_key.to_string(),
                reason: "manifest bytes on disk are not canonical (tampered formatting, unsorted collections, or duplicate entries)".to_string(),
            });
        }
        Ok(manifest)
    }

    /// Load an FM graph snapshot and verify its self-hash and exact canonical
    /// bytes against `graph_key`.
    pub(crate) fn load_and_verify_fm_graph(
        &self,
        graph_key: &str,
    ) -> Result<FmGraphSnapshot, CasError> {
        let expected_digest = parse_key_digest(graph_key, PREFIX_FM_GRAPH)?;
        let path = self.fm_graph_path(graph_key)?;
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_file() => {}
            Ok(_) => {
                return Err(CasError::FmGraphIntegrity {
                    fm_graph_key: graph_key.to_string(),
                    reason: format!("fm-graph path {} is not a regular file", path.display()),
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(CasError::MissingObject {
                    key: graph_key.to_string(),
                    affected_paths: Vec::new(),
                });
            }
            Err(error) => return Err(io_err(format!("lstat {}", path.display()), &error)),
        }
        let bytes =
            fs::read(&path).map_err(|error| io_err(format!("read {}", path.display()), &error))?;
        let raw_digest = sha256_domain(DOMAIN_FM_GRAPH, &bytes);
        if raw_digest != expected_digest {
            return Err(CasError::CorruptedObject {
                key: graph_key.to_string(),
                expected_digest: expected_digest.to_string(),
                actual_digest: raw_digest,
                affected_paths: Vec::new(),
            });
        }
        let graph: FmGraphSnapshot =
            serde_json::from_slice(&bytes).map_err(|error| CasError::FmGraphIntegrity {
                fm_graph_key: graph_key.to_string(),
                reason: format!("invalid fm-graph json: {error}"),
            })?;
        let canonical = graph.canonicalized()?;
        let expected_canonical_bytes = canonical_json_bytes(&canonical)?;
        if bytes != expected_canonical_bytes {
            return Err(CasError::FmGraphIntegrity {
                fm_graph_key: graph_key.to_string(),
                reason: "fm-graph bytes on disk are not canonical".to_string(),
            });
        }
        let recomputed = fm_graph_key(&canonical)?;
        if recomputed != graph_key {
            return Err(CasError::FmGraphIntegrity {
                fm_graph_key: graph_key.to_string(),
                reason: format!("recomputed canonical key {recomputed} differs from {graph_key}"),
            });
        }
        canonical.validate_structure()?;
        Ok(canonical)
    }

    #[allow(clippy::too_many_lines)]
    fn verify_snapshot_internal(
        &self,
        snap_key: &str,
    ) -> Result<(SnapshotManifest, Option<FmGraphSnapshot>, VerifySummary), CasError> {
        let manifest = self.load_and_verify_manifest(snap_key)?;
        if let Some(parent_key) = &manifest.parent_snapshot_key {
            let _ = parse_key_digest(parent_key, PREFIX_SNAPSHOT)?;
            if parent_key == snap_key {
                return Err(CasError::ManifestIntegrity {
                    snapshot_key: snap_key.to_string(),
                    reason: "snapshot cannot declare itself as parent_snapshot_key".to_string(),
                });
            }
        }
        let fm_graph = match &manifest.fm_graph_key {
            Some(graph_key) => Some(self.load_and_verify_fm_graph(graph_key)?),
            None => None,
        };

        if let Some(graph) = &fm_graph {
            validate_fm_graph_against_entries(graph, &manifest.entries)?;
        } else {
            for entry in &manifest.entries {
                if !entry.feature_ids.is_empty() {
                    return Err(CasError::FmGraphConsistency {
                        reason: format!(
                            "entry {:?} references feature_ids {:?} without an fm_graph_key",
                            entry.path, entry.feature_ids
                        ),
                    });
                }
            }
        }

        // Map each blob/chunk key to all affected logical paths so any missing
        // or corrupted object error names every affected logical path.
        let mut blob_paths: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut chunk_paths: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut entry_kinds: BTreeMap<&str, EntryKind> = BTreeMap::new();

        for entry in &manifest.entries {
            let canonical = canonicalize_posix_path(&entry.path)?;
            if canonical != entry.path {
                return Err(CasError::ManifestIntegrity {
                    snapshot_key: snap_key.to_string(),
                    reason: format!("non-canonical path {:?}", entry.path),
                });
            }
            if entry_kinds
                .insert(entry.path.as_str(), entry.kind)
                .is_some()
            {
                return Err(CasError::ManifestIntegrity {
                    snapshot_key: snap_key.to_string(),
                    reason: format!("duplicate entry path {:?}", entry.path),
                });
            }
            if entry.mode & !0o777 != 0 {
                return Err(CasError::ManifestIntegrity {
                    snapshot_key: snap_key.to_string(),
                    reason: format!(
                        "entry {:?} has mode {:#o} outside allowed 0o777 mask",
                        entry.path, entry.mode
                    ),
                });
            }
            let expected_logical = logical_file_id(&entry.path)?;
            if entry.logical_file_id != expected_logical {
                return Err(CasError::ManifestIntegrity {
                    snapshot_key: snap_key.to_string(),
                    reason: format!(
                        "logical_file_id mismatch for {:?}: expected {expected_logical}, found {}",
                        entry.path, entry.logical_file_id
                    ),
                });
            }
            if let Some(renamed_from) = &entry.renamed_from {
                if entry.kind == EntryKind::Directory {
                    return Err(CasError::ManifestIntegrity {
                        snapshot_key: snap_key.to_string(),
                        reason: format!(
                            "directory entry {:?} must not carry renamed_from",
                            entry.path
                        ),
                    });
                }
                let canonical_old = canonicalize_posix_path(renamed_from)?;
                if canonical_old != *renamed_from || renamed_from == &entry.path {
                    return Err(CasError::ManifestIntegrity {
                        snapshot_key: snap_key.to_string(),
                        reason: format!(
                            "invalid renamed_from {:?} for entry {:?}",
                            renamed_from, entry.path
                        ),
                    });
                }
            }
            if entry.kind == EntryKind::Regular {
                if let Some(key) = &entry.blob_key {
                    blob_paths
                        .entry(key.clone())
                        .or_default()
                        .push(entry.path.clone());
                }
                for chunk in &entry.chunks {
                    chunk_paths
                        .entry(chunk.chunk_key.clone())
                        .or_default()
                        .push(entry.path.clone());
                }
            }
        }

        // Reject path hierarchy collisions (e.g. regular file or symlink at "a"
        // alongside a descendant entry at "a/b").
        for entry in &manifest.entries {
            let mut prefix_end = 0usize;
            while let Some(rel_slash) = entry.path[prefix_end..].find('/') {
                let slash_idx = prefix_end + rel_slash;
                let ancestor = &entry.path[..slash_idx];
                if let Some(kind) = entry_kinds.get(ancestor)
                    && *kind != EntryKind::Directory
                {
                    return Err(CasError::ManifestIntegrity {
                        snapshot_key: snap_key.to_string(),
                        reason: format!(
                            "path hierarchy conflict: {:?} has non-directory ancestor {:?} ({kind:?})",
                            entry.path, ancestor
                        ),
                    });
                }
                prefix_end = slash_idx + 1;
            }
        }

        let mut verified_blobs: BTreeMap<String, u64> = BTreeMap::new();
        let mut verified_chunks = BTreeSet::new();
        let mut symlinks_verified = 0usize;

        for entry in &manifest.entries {
            match entry.kind {
                EntryKind::Directory => {
                    if entry.size != 0
                        || entry.blob_key.is_some()
                        || !entry.chunks.is_empty()
                        || entry.symlink_blob_key.is_some()
                        || entry.symlink_target.is_some()
                    {
                        return Err(CasError::ManifestIntegrity {
                            snapshot_key: snap_key.to_string(),
                            reason: format!(
                                "directory entry {:?} has unexpected payload fields",
                                entry.path
                            ),
                        });
                    }
                }
                EntryKind::Symlink => {
                    if entry.blob_key.is_some() || !entry.chunks.is_empty() {
                        return Err(CasError::ManifestIntegrity {
                            snapshot_key: snap_key.to_string(),
                            reason: format!(
                                "symlink entry {:?} must not carry blob_key or chunks",
                                entry.path
                            ),
                        });
                    }
                    let sym_key = entry.symlink_blob_key.as_deref().ok_or_else(|| {
                        CasError::ManifestIntegrity {
                            snapshot_key: snap_key.to_string(),
                            reason: format!(
                                "symlink entry {:?} missing symlink_blob_key",
                                entry.path
                            ),
                        }
                    })?;
                    let target = entry.symlink_target.as_deref().ok_or_else(|| {
                        CasError::ManifestIntegrity {
                            snapshot_key: snap_key.to_string(),
                            reason: format!(
                                "symlink entry {:?} missing symlink_target",
                                entry.path
                            ),
                        }
                    })?;
                    let target_len = u64::try_from(target.len()).unwrap_or(u64::MAX);
                    if entry.size != target_len {
                        return Err(CasError::SizeMismatch {
                            key: sym_key.to_string(),
                            expected: entry.size,
                            actual: target_len,
                            affected_paths: vec![entry.path.clone()],
                        });
                    }
                    let raw = self.read_verified_symlink_blob(
                        sym_key,
                        entry.size,
                        std::slice::from_ref(&entry.path),
                    )?;
                    let disk_target =
                        String::from_utf8(raw).map_err(|_| CasError::ManifestIntegrity {
                            snapshot_key: snap_key.to_string(),
                            reason: format!("symlink blob {sym_key} is not valid UTF-8"),
                        })?;
                    if disk_target != target {
                        return Err(CasError::ManifestIntegrity {
                            snapshot_key: snap_key.to_string(),
                            reason: format!(
                                "symlink target mismatch for {:?}: manifest has {target:?}, blob has {disk_target:?}",
                                entry.path
                            ),
                        });
                    }
                    symlinks_verified += 1;
                }
                EntryKind::Regular => {
                    if entry.symlink_blob_key.is_some() || entry.symlink_target.is_some() {
                        return Err(CasError::ManifestIntegrity {
                            snapshot_key: snap_key.to_string(),
                            reason: format!(
                                "regular entry {:?} must not carry symlink fields",
                                entry.path
                            ),
                        });
                    }
                    let whole_key =
                        entry
                            .blob_key
                            .as_deref()
                            .ok_or_else(|| CasError::ManifestIntegrity {
                                snapshot_key: snap_key.to_string(),
                                reason: format!("regular entry {:?} missing blob_key", entry.path),
                            })?;
                    let affected = blob_paths
                        .get(whole_key)
                        .cloned()
                        .unwrap_or_else(|| vec![entry.path.clone()]);
                    if entry.chunks.is_empty() {
                        if entry.size > manifest.chunk_size {
                            return Err(CasError::ManifestIntegrity {
                                snapshot_key: snap_key.to_string(),
                                reason: format!(
                                    "regular entry {:?} exceeds manifest chunk_size ({}) without chunks",
                                    entry.path, manifest.chunk_size
                                ),
                            });
                        }
                        if let Some(&verified_size) = verified_blobs.get(whole_key) {
                            if verified_size != entry.size {
                                return Err(CasError::SizeMismatch {
                                    key: whole_key.to_string(),
                                    expected: entry.size,
                                    actual: verified_size,
                                    affected_paths: affected,
                                });
                            }
                        } else {
                            let _ = self.read_verified_blob(whole_key, entry.size, &affected)?;
                            verified_blobs.insert(whole_key.to_string(), entry.size);
                        }
                    } else {
                        if entry.size == 0 {
                            return Err(CasError::ManifestIntegrity {
                                snapshot_key: snap_key.to_string(),
                                reason: format!(
                                    "zero-byte entry {:?} must not carry chunks",
                                    entry.path
                                ),
                            });
                        }
                        let expected_whole_digest = parse_key_digest(whole_key, PREFIX_BLOB)?;
                        let mut whole_hasher = Sha256::new();
                        whole_hasher.update(DOMAIN_BLOB);
                        whole_hasher.update(entry.size.to_be_bytes());
                        let mut expected_offset = 0u64;
                        for (idx, chunk) in entry.chunks.iter().enumerate() {
                            let expected_idx = u32::try_from(idx).unwrap_or(u32::MAX);
                            if chunk.index != expected_idx || chunk.offset != expected_offset {
                                return Err(CasError::ManifestIntegrity {
                                    snapshot_key: snap_key.to_string(),
                                    reason: format!(
                                        "non-contiguous chunk sequence for {:?} at index {}",
                                        entry.path, chunk.index
                                    ),
                                });
                            }
                            if chunk.length == 0 || chunk.length > manifest.chunk_size {
                                return Err(CasError::ManifestIntegrity {
                                    snapshot_key: snap_key.to_string(),
                                    reason: format!(
                                        "invalid chunk length {} for {:?} at index {} (chunk_size {})",
                                        chunk.length, entry.path, chunk.index, manifest.chunk_size
                                    ),
                                });
                            }
                            let c_affected = chunk_paths
                                .get(&chunk.chunk_key)
                                .cloned()
                                .unwrap_or_else(|| vec![entry.path.clone()]);
                            let bytes = self.read_verified_chunk(
                                &chunk.chunk_key,
                                chunk.length,
                                &c_affected,
                            )?;
                            verified_chunks.insert(chunk.chunk_key.clone());
                            whole_hasher.update(&bytes);
                            expected_offset = expected_offset.saturating_add(chunk.length);
                        }
                        if expected_offset != entry.size {
                            return Err(CasError::SizeMismatch {
                                key: whole_key.to_string(),
                                expected: entry.size,
                                actual: expected_offset,
                                affected_paths: affected,
                            });
                        }
                        let actual_whole_digest = encode_hex(&whole_hasher.finalize());
                        if actual_whole_digest != expected_whole_digest {
                            return Err(CasError::CorruptedObject {
                                key: whole_key.to_string(),
                                expected_digest: expected_whole_digest.to_string(),
                                actual_digest: actual_whole_digest,
                                affected_paths: affected,
                            });
                        }
                    }
                }
            }
        }

        let summary = VerifySummary {
            schema_version: SCHEMA_VERSION.to_string(),
            snapshot_key: manifest.snapshot_key.clone(),
            fm_graph_key: manifest.fm_graph_key.clone(),
            entries_verified: manifest.entries.len(),
            blobs_verified: verified_blobs.len(),
            chunks_verified: verified_chunks.len(),
            symlinks_verified,
            features_verified: fm_graph.as_ref().map_or(0, |graph| graph.features.len()),
        };

        Ok((manifest, fm_graph, summary))
    }

    #[allow(clippy::too_many_lines)]
    fn walk_source(
        &self,
        root: &Path,
        dir: &Path,
        file_to_features: &BTreeMap<String, Vec<String>>,
        entries: &mut Vec<SnapshotEntry>,
        skipped_specials: &mut Vec<SkippedSpecial>,
        stats: &mut SnapshotWriteStats,
    ) -> Result<(), CasError> {
        let mut children: Vec<PathBuf> = fs::read_dir(dir)
            .map_err(|error| io_err(format!("read_dir {}", dir.display()), &error))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| io_err(format!("dir entry {}", dir.display()), &error))?
            .into_iter()
            .map(|entry| entry.path())
            .collect();
        children.sort();

        for child in children {
            let rel = child
                .strip_prefix(root)
                .map_err(|_| CasError::InvalidPath {
                    path: child.display().to_string(),
                    reason: "failed to compute relative path".to_string(),
                })?;
            // Skip `.git` directory if snapshotting inside a git repository.
            if rel.components().any(|comp| comp.as_os_str() == ".git") {
                continue;
            }
            let rel_posix = path_to_canonical_posix(rel)?;
            let meta = fs::symlink_metadata(&child)
                .map_err(|error| io_err(format!("lstat {}", child.display()), &error))?;
            let file_type = meta.file_type();

            if let Some(special_kind) = detect_special_file(file_type) {
                skipped_specials.push(SkippedSpecial {
                    path: rel_posix,
                    kind: special_kind.to_string(),
                });
                continue;
            }

            let logical_id = logical_file_id(&rel_posix)?;
            let feature_ids = file_to_features
                .get(&rel_posix)
                .cloned()
                .unwrap_or_default();
            let mode = extract_mode(&meta);
            let mtime_ns = extract_mtime_ns(&meta)?;

            if file_type.is_symlink() {
                let target_os = fs::read_link(&child)
                    .map_err(|error| io_err(format!("read_link {}", child.display()), &error))?;
                let target = target_os
                    .to_str()
                    .ok_or_else(|| CasError::InvalidPath {
                        path: rel_posix.clone(),
                        reason: "symlink target is not valid UTF-8".to_string(),
                    })?
                    .to_string();
                let sym_key = symlink_blob_key(&target);
                let digest = parse_key_digest(&sym_key, PREFIX_SYMLINK)?;
                let dest = self.symlink_blob_path(&sym_key)?;
                let outcome = self.write_object_once(
                    &dest,
                    target.as_bytes(),
                    &sym_key,
                    digest,
                    |disk_bytes| sha256_len_prefixed(DOMAIN_SYMLINK, disk_bytes),
                )?;
                match outcome {
                    WriteOutcome::Written => stats.blobs_written += 1,
                    WriteOutcome::Deduplicated => stats.blobs_deduplicated += 1,
                }
                entries.push(SnapshotEntry {
                    path: rel_posix,
                    logical_file_id: logical_id,
                    kind: EntryKind::Symlink,
                    mode,
                    mtime_ns,
                    size: u64::try_from(target.len()).unwrap_or(u64::MAX),
                    blob_key: None,
                    chunks: Vec::new(),
                    symlink_blob_key: Some(sym_key),
                    symlink_target: Some(target),
                    renamed_from: None,
                    feature_ids,
                });
            } else if file_type.is_dir() {
                entries.push(SnapshotEntry {
                    path: rel_posix,
                    logical_file_id: logical_id,
                    kind: EntryKind::Directory,
                    mode,
                    mtime_ns,
                    size: 0,
                    blob_key: None,
                    chunks: Vec::new(),
                    symlink_blob_key: None,
                    symlink_target: None,
                    renamed_from: None,
                    feature_ids,
                });
                self.walk_source(
                    root,
                    &child,
                    file_to_features,
                    entries,
                    skipped_specials,
                    stats,
                )?;
            } else if file_type.is_file() {
                let stored = self.store_regular_file(&child, stats)?;
                entries.push(SnapshotEntry {
                    path: rel_posix,
                    logical_file_id: logical_id,
                    kind: EntryKind::Regular,
                    mode: stored.mode,
                    mtime_ns: stored.mtime_ns,
                    size: stored.size,
                    blob_key: Some(stored.whole_key),
                    chunks: stored.chunks,
                    symlink_blob_key: None,
                    symlink_target: None,
                    renamed_from: None,
                    feature_ids,
                });
            } else {
                skipped_specials.push(SkippedSpecial {
                    path: rel_posix,
                    kind: "other".to_string(),
                });
            }
        }
        Ok(())
    }

    fn store_regular_file(
        &self,
        path: &Path,
        stats: &mut SnapshotWriteStats,
    ) -> Result<StoredRegularFile, CasError> {
        let mut file = open_regular_nofollow(path)?;
        let meta = file
            .metadata()
            .map_err(|error| io_err(format!("fstat {}", path.display()), &error))?;
        if !meta.file_type().is_file() {
            return Err(CasError::InvalidPath {
                path: path.display().to_string(),
                reason: "path changed to non-regular file during snapshot".to_string(),
            });
        }
        let expected_size = meta.len();
        let mode = extract_mode(&meta);
        let mtime_ns = extract_mtime_ns(&meta)?;
        let chunk_limit = u64::try_from(self.chunk_size).unwrap_or(u64::MAX);

        if expected_size <= chunk_limit {
            let mut bytes = Vec::with_capacity(usize::try_from(expected_size).unwrap_or(0));
            file.read_to_end(&mut bytes)
                .map_err(|error| io_err(format!("read {}", path.display()), &error))?;
            let actual_size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
            if actual_size != expected_size {
                return Err(CasError::SizeMismatch {
                    key: path.display().to_string(),
                    expected: expected_size,
                    actual: actual_size,
                    affected_paths: vec![path.display().to_string()],
                });
            }
            let key = blob_key(&bytes);
            let digest = parse_key_digest(&key, PREFIX_BLOB)?;
            let dest = self.blob_path(&key)?;
            let outcome = self.write_object_once(&dest, &bytes, &key, digest, |disk_bytes| {
                sha256_len_prefixed(DOMAIN_BLOB, disk_bytes)
            })?;
            match outcome {
                WriteOutcome::Written => stats.blobs_written += 1,
                WriteOutcome::Deduplicated => stats.blobs_deduplicated += 1,
            }
            return Ok(StoredRegularFile {
                whole_key: key,
                size: actual_size,
                mode,
                mtime_ns,
                chunks: Vec::new(),
            });
        }

        // Multi-chunk deterministic streaming path for files > chunk_size.
        let mut whole_hasher = Sha256::new();
        whole_hasher.update(DOMAIN_BLOB);
        whole_hasher.update(expected_size.to_be_bytes());

        let mut chunks = Vec::new();
        let mut offset = 0u64;
        let mut index = 0u32;
        let mut buf = vec![0u8; self.chunk_size];

        loop {
            let read_len = read_full_chunk(&mut file, &mut buf, path)?;
            if read_len == 0 {
                break;
            }
            let slice = &buf[..read_len];
            whole_hasher.update(slice);
            let c_key = chunk_key(slice);
            let c_digest = parse_key_digest(&c_key, PREFIX_CHUNK)?;
            let c_dest = self.chunk_path(&c_key)?;
            let outcome =
                self.write_object_once(&c_dest, slice, &c_key, c_digest, |disk_bytes| {
                    sha256_len_prefixed(DOMAIN_CHUNK, disk_bytes)
                })?;
            match outcome {
                WriteOutcome::Written => stats.chunks_written += 1,
                WriteOutcome::Deduplicated => stats.chunks_deduplicated += 1,
            }
            let len_u64 = u64::try_from(read_len).unwrap_or(u64::MAX);
            chunks.push(ChunkRef {
                index,
                offset,
                length: len_u64,
                chunk_key: c_key,
            });
            offset = offset.saturating_add(len_u64);
            index = index.saturating_add(1);
        }

        if offset != expected_size {
            return Err(CasError::SizeMismatch {
                key: path.display().to_string(),
                expected: expected_size,
                actual: offset,
                affected_paths: vec![path.display().to_string()],
            });
        }

        let whole_key = format!("{PREFIX_BLOB}{}", encode_hex(&whole_hasher.finalize()));
        Ok(StoredRegularFile {
            whole_key,
            size: offset,
            mode,
            mtime_ns,
            chunks,
        })
    }

    fn read_verified_blob(
        &self,
        key: &str,
        expected_size: u64,
        affected_paths: &[String],
    ) -> Result<Vec<u8>, CasError> {
        let expected_digest = parse_key_digest(key, PREFIX_BLOB)?;
        let path = self.blob_path(key)?;
        read_and_verify_object(
            &path,
            key,
            expected_digest,
            expected_size,
            affected_paths,
            |bytes| sha256_len_prefixed(DOMAIN_BLOB, bytes),
        )
    }

    fn read_verified_chunk(
        &self,
        key: &str,
        expected_size: u64,
        affected_paths: &[String],
    ) -> Result<Vec<u8>, CasError> {
        let expected_digest = parse_key_digest(key, PREFIX_CHUNK)?;
        let path = self.chunk_path(key)?;
        read_and_verify_object(
            &path,
            key,
            expected_digest,
            expected_size,
            affected_paths,
            |bytes| sha256_len_prefixed(DOMAIN_CHUNK, bytes),
        )
    }

    fn read_verified_symlink_blob(
        &self,
        key: &str,
        expected_size: u64,
        affected_paths: &[String],
    ) -> Result<Vec<u8>, CasError> {
        let expected_digest = parse_key_digest(key, PREFIX_SYMLINK)?;
        let path = self.symlink_blob_path(key)?;
        read_and_verify_object(
            &path,
            key,
            expected_digest,
            expected_size,
            affected_paths,
            |bytes| sha256_len_prefixed(DOMAIN_SYMLINK, bytes),
        )
    }

    /// Atomic, idempotent write-once commit for immutable CAS objects.
    ///
    /// Writes to `<store>/fm-cas/v1/staging/.tmp-<pid>-<seq>`, fsyncs, and
    /// commits with hard-link write-once semantics followed by a parent
    /// directory fsync. If the destination already exists, verifies the
    /// on-disk object's recomputed hash rather than blindly trusting it.
    fn write_object_once<F>(
        &self,
        dest: &Path,
        bytes: &[u8],
        object_key: &str,
        expected_digest: &str,
        digest_fn: F,
    ) -> Result<WriteOutcome, CasError>
    where
        F: Fn(&[u8]) -> String,
    {
        match fs::symlink_metadata(dest) {
            Ok(meta) if meta.file_type().is_file() => {
                verify_existing_on_disk(dest, object_key, expected_digest, &digest_fn)?;
                return Ok(WriteOutcome::Deduplicated);
            }
            Ok(_) => {
                return Err(CasError::CorruptedObject {
                    key: object_key.to_string(),
                    expected_digest: expected_digest.to_string(),
                    actual_digest: "<non-regular-file>".to_string(),
                    affected_paths: Vec::new(),
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_err(format!("lstat {}", dest.display()), &error)),
        }

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| io_err(format!("create {}", parent.display()), &error))?;
        }

        let (tmp_path, mut tmp_file) = self.create_staging_file()?;
        let mut guard = TempFileGuard::new(tmp_path.clone());
        tmp_file
            .write_all(bytes)
            .map_err(|error| io_err(format!("write {}", tmp_path.display()), &error))?;
        tmp_file
            .sync_all()
            .map_err(|error| io_err(format!("sync {}", tmp_path.display()), &error))?;
        drop(tmp_file);

        match fs::hard_link(&tmp_path, dest) {
            Ok(()) => {
                sync_parent_dir(dest)?;
                // Keep guard armed so the staging hard-link is removed cleanly.
                Ok(WriteOutcome::Written)
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                verify_existing_on_disk(dest, object_key, expected_digest, &digest_fn)?;
                Ok(WriteOutcome::Deduplicated)
            }
            Err(error) if is_hardlink_unsupported(&error) => {
                // Fallback for filesystems without hard links: re-check for
                // existing object before atomic rename so we never silently
                // overwrite corrupt on-disk data.
                if let Ok(meta) = fs::symlink_metadata(dest) {
                    if !meta.file_type().is_file() {
                        return Err(CasError::CorruptedObject {
                            key: object_key.to_string(),
                            expected_digest: expected_digest.to_string(),
                            actual_digest: "<non-regular-file>".to_string(),
                            affected_paths: Vec::new(),
                        });
                    }
                    verify_existing_on_disk(dest, object_key, expected_digest, &digest_fn)?;
                    return Ok(WriteOutcome::Deduplicated);
                }
                fs::rename(&tmp_path, dest).map_err(|rename_err| {
                    io_err(
                        format!("rename {} -> {}", tmp_path.display(), dest.display()),
                        &rename_err,
                    )
                })?;
                guard.disarm();
                sync_parent_dir(dest)?;
                Ok(WriteOutcome::Written)
            }
            Err(error) => Err(io_err(
                format!("hard_link {} -> {}", tmp_path.display(), dest.display()),
                &error,
            )),
        }
    }

    fn create_staging_file(&self) -> Result<(PathBuf, File), CasError> {
        let staging = self.staging_dir();
        fs::create_dir_all(&staging)
            .map_err(|error| io_err(format!("create {}", staging.display()), &error))?;
        let pid = std::process::id();
        for _ in 0..64 {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let candidate = staging.join(format!(".tmp-{pid}-{seq}"));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(file) => return Ok((candidate, file)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(io_err(
                        format!("create staging {}", candidate.display()),
                        &error,
                    ));
                }
            }
        }
        Err(CasError::Io {
            context: "create staging file".to_string(),
            message: "exhausted unique staging file attempts".to_string(),
        })
    }

    fn record_snapshot_index(&self, entry: &SnapshotIndexEntry) -> Result<(), CasError> {
        let _lock = CasLock::acquire(&self.cas_root)?;
        let mut index = self.read_index()?;
        if !index
            .snapshots
            .iter()
            .any(|existing| existing.snapshot_key == entry.snapshot_key)
        {
            index.snapshots.push(entry.clone());
        }
        let bytes = canonical_json_bytes(&index)?;
        let (tmp_path, mut tmp_file) = self.create_staging_file()?;
        let mut guard = TempFileGuard::new(tmp_path.clone());
        tmp_file
            .write_all(&bytes)
            .map_err(|error| io_err(format!("write {}", tmp_path.display()), &error))?;
        tmp_file
            .sync_all()
            .map_err(|error| io_err(format!("sync {}", tmp_path.display()), &error))?;
        drop(tmp_file);
        let dest = self.index_path();
        fs::rename(&tmp_path, &dest).map_err(|error| {
            io_err(
                format!("rename {} -> {}", tmp_path.display(), dest.display()),
                &error,
            )
        })?;
        guard.disarm();
        sync_parent_dir(&dest)?;
        Ok(())
    }
}

struct StoredRegularFile {
    whole_key: String,
    size: u64,
    mode: u32,
    mtime_ns: i128,
    chunks: Vec<ChunkRef>,
}

#[derive(Default)]
struct SnapshotWriteStats {
    blobs_written: usize,
    blobs_deduplicated: usize,
    chunks_written: usize,
    chunks_deduplicated: usize,
}

fn verify_existing_on_disk<F>(
    path: &Path,
    object_key: &str,
    expected_digest: &str,
    digest_fn: &F,
) -> Result<(), CasError>
where
    F: Fn(&[u8]) -> String,
{
    let disk_bytes =
        fs::read(path).map_err(|error| io_err(format!("read {}", path.display()), &error))?;
    let actual_digest = digest_fn(&disk_bytes);
    if actual_digest != expected_digest {
        return Err(CasError::CorruptedObject {
            key: object_key.to_string(),
            expected_digest: expected_digest.to_string(),
            actual_digest,
            affected_paths: Vec::new(),
        });
    }
    Ok(())
}

fn read_and_verify_object<F>(
    path: &Path,
    key: &str,
    expected_digest: &str,
    expected_size: u64,
    affected_paths: &[String],
    digest_fn: F,
) -> Result<Vec<u8>, CasError>
where
    F: Fn(&[u8]) -> String,
{
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() => {}
        Ok(_) => {
            return Err(CasError::CorruptedObject {
                key: key.to_string(),
                expected_digest: expected_digest.to_string(),
                actual_digest: "<non-regular-file>".to_string(),
                affected_paths: affected_paths.to_vec(),
            });
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(CasError::MissingObject {
                key: key.to_string(),
                affected_paths: affected_paths.to_vec(),
            });
        }
        Err(error) => return Err(io_err(format!("lstat {}", path.display()), &error)),
    }
    let bytes =
        fs::read(path).map_err(|error| io_err(format!("read {}", path.display()), &error))?;
    let actual_size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if actual_size != expected_size {
        return Err(CasError::SizeMismatch {
            key: key.to_string(),
            expected: expected_size,
            actual: actual_size,
            affected_paths: affected_paths.to_vec(),
        });
    }
    let actual_digest = digest_fn(&bytes);
    if actual_digest != expected_digest {
        return Err(CasError::CorruptedObject {
            key: key.to_string(),
            expected_digest: expected_digest.to_string(),
            actual_digest,
            affected_paths: affected_paths.to_vec(),
        });
    }
    Ok(bytes)
}

fn read_full_chunk(file: &mut File, buf: &mut [u8], path: &Path) -> Result<usize, CasError> {
    let mut filled = 0usize;
    while filled < buf.len() {
        let n = file
            .read(&mut buf[filled..])
            .map_err(|error| io_err(format!("read {}", path.display()), &error))?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

fn apply_rename_detection(
    parent_entries: &[SnapshotEntry],
    current_entries: &mut [SnapshotEntry],
) -> usize {
    let parent_logical_ids: BTreeSet<&str> = parent_entries
        .iter()
        .map(|entry| entry.logical_file_id.as_str())
        .collect();
    let current_logical_ids: BTreeSet<String> = current_entries
        .iter()
        .map(|entry| entry.logical_file_id.clone())
        .collect();

    let mut disappeared_by_content: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for entry in parent_entries {
        if !current_logical_ids.contains(&entry.logical_file_id)
            && let Some(content_key) = entry
                .blob_key
                .as_deref()
                .or(entry.symlink_blob_key.as_deref())
        {
            disappeared_by_content
                .entry(content_key)
                .or_default()
                .push(entry.path.as_str());
        }
    }
    for paths in disappeared_by_content.values_mut() {
        paths.sort_unstable();
        paths.reverse(); // so `.pop()` yields lexicographically smallest path first
    }

    let mut renamed = 0usize;
    for entry in current_entries.iter_mut() {
        if parent_logical_ids.contains(entry.logical_file_id.as_str()) {
            continue;
        }
        let content_key = entry
            .blob_key
            .as_deref()
            .or(entry.symlink_blob_key.as_deref());
        if let Some(key) = content_key
            && let Some(candidates) = disappeared_by_content.get_mut(key)
            && let Some(old_path) = candidates.pop()
        {
            entry.renamed_from = Some(old_path.to_string());
            renamed += 1;
        }
    }
    renamed
}

fn validate_fm_graph_against_entries(
    graph: &FmGraphSnapshot,
    entries: &[SnapshotEntry],
) -> Result<(), CasError> {
    graph.validate_structure()?;

    let entry_map: BTreeMap<&str, &SnapshotEntry> = entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let mut feature_files: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();

    for feature in &graph.features {
        let files_set = feature_files.entry(feature.id.as_str()).or_default();
        for assoc in &feature.files {
            match assoc.status {
                FileAssocStatus::Present => {
                    let Some(entry) = entry_map.get(assoc.path.as_str()) else {
                        return Err(CasError::FmGraphConsistency {
                            reason: format!(
                                "feature {:?} references missing file {:?} (not marked external or missing)",
                                feature.id, assoc.path
                            ),
                        });
                    };
                    if !entry.feature_ids.iter().any(|id| id == &feature.id) {
                        return Err(CasError::FmGraphConsistency {
                            reason: format!(
                                "bidirectional mismatch: feature {:?} references {:?}, but entry does not list feature",
                                feature.id, assoc.path
                            ),
                        });
                    }
                    files_set.insert(assoc.path.as_str());
                }
                FileAssocStatus::External | FileAssocStatus::Missing => {}
            }
        }
    }

    for entry in entries {
        for fid in &entry.feature_ids {
            let Some(files_set) = feature_files.get(fid.as_str()) else {
                return Err(CasError::FmGraphConsistency {
                    reason: format!(
                        "entry {:?} references unknown feature {:?}",
                        entry.path, fid
                    ),
                });
            };
            if !files_set.contains(entry.path.as_str()) {
                return Err(CasError::FmGraphConsistency {
                    reason: format!(
                        "bidirectional mismatch: entry {:?} lists feature {:?}, but feature does not reference path",
                        entry.path, fid
                    ),
                });
            }
        }
    }

    Ok(())
}

fn path_to_canonical_posix(rel: &Path) -> Result<String, CasError> {
    let mut parts = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(seg) => {
                let text = seg.to_str().ok_or_else(|| CasError::InvalidPath {
                    path: rel.display().to_string(),
                    reason: "non-UTF-8 path component".to_string(),
                })?;
                parts.push(text);
            }
            _ => {
                return Err(CasError::InvalidPath {
                    path: rel.display().to_string(),
                    reason: "non-normal path component in relative path".to_string(),
                });
            }
        }
    }
    canonicalize_posix_path(&parts.join("/"))
}

fn safe_join(root: &Path, canonical_rel: &str) -> Result<PathBuf, CasError> {
    let verified = canonicalize_posix_path(canonical_rel)?;
    let mut out = root.to_path_buf();
    for seg in verified.split('/') {
        out.push(seg);
    }
    Ok(out)
}

const MAX_SYMLINK_HOPS: usize = 40;

fn validate_symlink_target(entry_path: &str, target: &str) -> Result<(), CasError> {
    if target.is_empty() {
        return Err(CasError::UnsafeRestoreTarget {
            path: entry_path.to_string(),
            reason: "symlink target must not be empty".to_string(),
        });
    }
    if target.contains('\0') {
        return Err(CasError::UnsafeRestoreTarget {
            path: entry_path.to_string(),
            reason: "symlink target contains NUL byte".to_string(),
        });
    }
    if target.starts_with('/') {
        return Err(CasError::UnsafeRestoreTarget {
            path: entry_path.to_string(),
            reason: format!("absolute symlink target {target:?} is refused on restore"),
        });
    }
    // Simulate resolving `target` relative to `entry_path`'s parent directory
    // and ensure depth never drops below the restore root (0).
    let mut depth: i64 =
        i64::try_from(entry_path.split('/').count().saturating_sub(1)).unwrap_or(0);
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return Err(CasError::UnsafeRestoreTarget {
                        path: entry_path.to_string(),
                        reason: format!("symlink target {target:?} escapes restore root via '..'"),
                    });
                }
            }
            ".git" => {
                return Err(CasError::UnsafeRestoreTarget {
                    path: entry_path.to_string(),
                    reason: format!("symlink target {target:?} references '.git'"),
                });
            }
            _ => depth += 1,
        }
    }
    Ok(())
}

/// Validate all symlinks in a manifest, including multi-hop symlink chains
/// (e.g. `a/b/jump -> ../..` followed by `a/b/escape -> jump/../../etc/passwd`)
/// and symlink loops (> `MAX_SYMLINK_HOPS`).
fn validate_all_symlink_targets(entries: &[SnapshotEntry]) -> Result<(), CasError> {
    let mut symlinks: BTreeMap<&str, &str> = BTreeMap::new();
    for entry in entries {
        if entry.kind == EntryKind::Symlink {
            let target =
                entry
                    .symlink_target
                    .as_deref()
                    .ok_or_else(|| CasError::UnsafeRestoreTarget {
                        path: entry.path.clone(),
                        reason: "symlink entry missing target".to_string(),
                    })?;
            validate_symlink_target(&entry.path, target)?;
            symlinks.insert(entry.path.as_str(), target);
        }
    }

    for (&sym_path, &target) in &symlinks {
        let mut cwd: Vec<String> = sym_path.split('/').map(str::to_string).collect();
        let _ = cwd.pop(); // parent directory of `sym_path`
        let mut queue: Vec<String> = target
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .map(str::to_string)
            .rev()
            .collect();
        let mut hops = 0usize;

        while let Some(part) = queue.pop() {
            if part == ".." {
                if cwd.pop().is_none() {
                    return Err(CasError::UnsafeRestoreTarget {
                        path: sym_path.to_string(),
                        reason: format!(
                            "symlink chain from {target:?} escapes restore root via '..'"
                        ),
                    });
                }
                continue;
            }
            if part == ".git" {
                return Err(CasError::UnsafeRestoreTarget {
                    path: sym_path.to_string(),
                    reason: format!("symlink chain from {target:?} references '.git'"),
                });
            }
            cwd.push(part);
            let candidate = cwd.join("/");
            if let Some(&next_target) = symlinks.get(candidate.as_str()) {
                hops += 1;
                if hops > MAX_SYMLINK_HOPS {
                    return Err(CasError::UnsafeRestoreTarget {
                        path: sym_path.to_string(),
                        reason: format!(
                            "symlink chain from {target:?} exceeds maximum hop depth ({MAX_SYMLINK_HOPS}; possible symlink loop)"
                        ),
                    });
                }
                // Resolve `next_target` relative to `candidate`'s parent directory.
                let _ = cwd.pop();
                let next_parts: Vec<String> = next_target
                    .split('/')
                    .filter(|seg| !seg.is_empty() && *seg != ".")
                    .map(str::to_string)
                    .collect();
                for seg in next_parts.into_iter().rev() {
                    queue.push(seg);
                }
            }
        }
    }
    Ok(())
}

/// Returns `true` if `candidate` equals or lies inside `root` (comparing
/// lexically normalized paths and, where parents exist, canonicalized paths).
fn paths_overlap(root: &Path, candidate: &Path) -> bool {
    let norm_root = normalize_lexical_path(root);
    let norm_cand = normalize_lexical_path(candidate);
    if norm_cand == norm_root || norm_cand.starts_with(&norm_root) {
        return true;
    }
    if let (Ok(can_root), Some(cand_parent)) = (fs::canonicalize(root), candidate.parent())
        && let Ok(can_parent) = fs::canonicalize(cand_parent)
    {
        let can_cand = match candidate.file_name() {
            Some(name) => can_parent.join(name),
            None => can_parent,
        };
        if can_cand == can_root || can_cand.starts_with(&can_root) {
            return true;
        }
    }
    false
}

fn normalize_lexical_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Prepares an empty restore directory and returns `true` if this call created
/// `into_dir` (so rollback knows whether to remove the root directory itself or
/// only its contents).
fn prepare_empty_restore_dir(into_dir: &Path) -> Result<bool, CasError> {
    match fs::symlink_metadata(into_dir) {
        Ok(meta) => {
            if !meta.file_type().is_dir() {
                return Err(CasError::UnsafeRestoreTarget {
                    path: into_dir.display().to_string(),
                    reason: "restore destination exists and is not a directory".to_string(),
                });
            }
            let mut iter = fs::read_dir(into_dir)
                .map_err(|error| io_err(format!("read_dir {}", into_dir.display()), &error))?;
            if iter.next().is_some() {
                return Err(CasError::UnsafeRestoreTarget {
                    path: into_dir.display().to_string(),
                    reason: "refusing to restore into non-empty directory".to_string(),
                });
            }
            Ok(false)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(into_dir).map_err(|create_err| {
                io_err(format!("create {}", into_dir.display()), &create_err)
            })?;
            Ok(true)
        }
        Err(error) => Err(io_err(format!("lstat {}", into_dir.display()), &error)),
    }
}

/// Cleans up partial restore output if `restore_snapshot` fails mid-flight.
struct RestoreRollbackGuard {
    into_dir: PathBuf,
    created_root: bool,
    fm_file: Option<PathBuf>,
    armed: bool,
}

impl RestoreRollbackGuard {
    fn new(into_dir: PathBuf, created_root: bool) -> Self {
        Self {
            into_dir,
            created_root,
            fm_file: None,
            armed: true,
        }
    }

    fn track_fm_file(&mut self, path: PathBuf) {
        self.fm_file = Some(path);
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RestoreRollbackGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(fm) = &self.fm_file {
            let _ = fs::remove_file(fm);
        }
        make_tree_writable_best_effort(&self.into_dir);
        if self.created_root {
            let _ = fs::remove_dir_all(&self.into_dir);
        } else if let Ok(entries) = fs::read_dir(&self.into_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                    let _ = fs::remove_dir_all(&path);
                } else {
                    let _ = fs::remove_file(&path);
                }
            }
        }
    }
}

fn make_tree_writable_best_effort(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::symlink_metadata(dir)
            && meta.file_type().is_dir()
        {
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                        make_tree_writable_best_effort(&entry.path());
                    }
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
}

fn ensure_no_symlink_components(root: &Path, candidate: &Path) -> Result<(), CasError> {
    let rel = candidate
        .strip_prefix(root)
        .map_err(|_| CasError::UnsafeRestoreTarget {
            path: candidate.display().to_string(),
            reason: "path escapes restore destination root".to_string(),
        })?;
    let mut current = root.to_path_buf();
    for component in rel.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(CasError::UnsafeRestoreTarget {
                        path: current.display().to_string(),
                        reason: "refusing to traverse symlink component inside restore target"
                            .to_string(),
                    });
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_err(format!("lstat {}", current.display()), &error)),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn open_regular_nofollow(path: &Path) -> Result<File, CasError> {
    use rustix::fs::{Mode, OFlags};
    let fd = rustix::fs::open(path, OFlags::RDONLY | OFlags::NOFOLLOW, Mode::empty()).map_err(
        |errno| {
            io_err(
                format!("open(O_NOFOLLOW) {}", path.display()),
                &errno.into(),
            )
        },
    )?;
    Ok(File::from(fd))
}

#[cfg(not(unix))]
fn open_regular_nofollow(path: &Path) -> Result<File, CasError> {
    File::open(path).map_err(|error| io_err(format!("open {}", path.display()), &error))
}

#[cfg(unix)]
fn sync_parent_dir(path: &Path) -> Result<(), CasError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() {
        return Ok(());
    }
    let dir = File::open(parent)
        .map_err(|error| io_err(format!("open dir {}", parent.display()), &error))?;
    dir.sync_all()
        .map_err(|error| io_err(format!("sync dir {}", parent.display()), &error))
}

#[cfg(not(unix))]
fn sync_parent_dir(_path: &Path) -> Result<(), CasError> {
    Ok(())
}

fn is_hardlink_unsupported(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::Unsupported | io::ErrorKind::CrossesDevices
    ) {
        return true;
    }
    #[cfg(unix)]
    {
        error.raw_os_error().is_some_and(|code| {
            matches!(
                rustix::io::Errno::from_raw_os_error(code),
                rustix::io::Errno::OPNOTSUPP
                    | rustix::io::Errno::NOSYS
                    | rustix::io::Errno::XDEV
                    | rustix::io::Errno::PERM
            )
        })
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(unix)]
fn is_process_alive(pid: u32) -> bool {
    let Ok(raw_pid) = i32::try_from(pid) else {
        return false;
    };
    let Some(typed_pid) = rustix::process::Pid::from_raw(raw_pid) else {
        return false;
    };
    match rustix::process::test_kill_process(typed_pid) {
        Ok(()) => true,
        Err(errno) => errno == rustix::io::Errno::PERM,
    }
}

#[cfg(not(unix))]
fn is_process_alive(_pid: u32) -> bool {
    false
}

#[cfg(unix)]
fn detect_special_file(file_type: fs::FileType) -> Option<&'static str> {
    use std::os::unix::fs::FileTypeExt;
    if file_type.is_fifo() {
        Some("fifo")
    } else if file_type.is_socket() {
        Some("socket")
    } else if file_type.is_block_device() {
        Some("block_device")
    } else if file_type.is_char_device() {
        Some("char_device")
    } else {
        None
    }
}

#[cfg(not(unix))]
fn detect_special_file(_file_type: fs::FileType) -> Option<&'static str> {
    None
}

#[cfg(unix)]
fn extract_mode(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn extract_mode(_meta: &fs::Metadata) -> u32 {
    0o644
}

#[cfg(unix)]
fn set_file_mode(path: &Path, mode: u32) -> Result<(), CasError> {
    use std::os::unix::fs::PermissionsExt;
    let masked = mode & 0o777;
    fs::set_permissions(path, fs::Permissions::from_mode(masked))
        .map_err(|error| io_err(format!("chmod {masked:o} {}", path.display()), &error))
}

#[cfg(not(unix))]
fn set_file_mode(_path: &Path, _mode: u32) -> Result<(), CasError> {
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &str, dest: &Path) -> Result<(), CasError> {
    std::os::unix::fs::symlink(target, dest)
        .map_err(|error| io_err(format!("symlink {} -> {target}", dest.display()), &error))
}

#[cfg(not(unix))]
fn create_symlink(_target: &str, dest: &Path) -> Result<(), CasError> {
    Err(CasError::UnsafeRestoreTarget {
        path: dest.display().to_string(),
        reason: "symlinks are not supported on this platform".to_string(),
    })
}

fn extract_mtime_ns(meta: &fs::Metadata) -> Result<i128, CasError> {
    let modified = meta
        .modified()
        .map_err(|error| io_err("read mtime", &error))?;
    match modified.duration_since(UNIX_EPOCH) {
        Ok(dur) => Ok(i128::from(dur.as_secs()) * 1_000_000_000 + i128::from(dur.subsec_nanos())),
        Err(err) => {
            let dur = err.duration();
            Ok(-(i128::from(dur.as_secs()) * 1_000_000_000 + i128::from(dur.subsec_nanos())))
        }
    }
}

fn mtime_ns_to_system_time(mtime_ns: i128) -> Result<SystemTime, CasError> {
    let abs = mtime_ns.unsigned_abs();
    let secs = u64::try_from(abs / 1_000_000_000).map_err(|_| CasError::InvalidKey {
        key: mtime_ns.to_string(),
        reason: "mtime seconds out of range".to_string(),
    })?;
    let nanos = u32::try_from(abs % 1_000_000_000).unwrap_or(0);
    let dur = Duration::new(secs, nanos);
    let time = if mtime_ns >= 0 {
        UNIX_EPOCH.checked_add(dur)
    } else {
        UNIX_EPOCH.checked_sub(dur)
    };
    time.ok_or_else(|| CasError::InvalidKey {
        key: mtime_ns.to_string(),
        reason: "mtime out of SystemTime range".to_string(),
    })
}

fn set_file_mtime(file: &File, mtime_ns: i128, path: &Path) -> Result<(), CasError> {
    let time = mtime_ns_to_system_time(mtime_ns)?;
    file.set_modified(time)
        .map_err(|error| io_err(format!("set_modified {}", path.display()), &error))
}

#[cfg(unix)]
fn set_path_mtime_nofollow(path: &Path, mtime_ns: i128) -> Result<(), CasError> {
    use rustix::fs::{AtFlags, CWD, Nsecs, Secs, Timespec, Timestamps, UTIME_OMIT, utimensat};
    let secs =
        Secs::try_from(mtime_ns.div_euclid(1_000_000_000)).map_err(|_| CasError::InvalidKey {
            key: mtime_ns.to_string(),
            reason: "mtime seconds out of POSIX timespec range".to_string(),
        })?;
    let nsecs =
        Nsecs::try_from(mtime_ns.rem_euclid(1_000_000_000)).map_err(|_| CasError::InvalidKey {
            key: mtime_ns.to_string(),
            reason: "mtime nanoseconds out of POSIX timespec range".to_string(),
        })?;
    let times = Timestamps {
        last_access: Timespec {
            tv_sec: 0,
            tv_nsec: UTIME_OMIT,
        },
        last_modification: Timespec {
            tv_sec: secs,
            tv_nsec: nsecs,
        },
    };
    utimensat(CWD, path, &times, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|errno| io_err(format!("utimensat {}", path.display()), &errno.into()))
}

#[cfg(not(unix))]
fn set_path_mtime_nofollow(_path: &Path, _mtime_ns: i128) -> Result<(), CasError> {
    Ok(())
}

fn now_unix_ms() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    i64::try_from(millis).unwrap_or(i64::MAX)
}

/// Inspect deterministic keys for a single file or symlink on disk.
pub(crate) fn inspect_key_of(
    path: &Path,
    logical_path: Option<&str>,
    chunk_size: usize,
) -> Result<KeyOfReport, CasError> {
    if chunk_size == 0 {
        return Err(CasError::InvalidKey {
            key: "chunk_size".to_string(),
            reason: "chunk_size must be greater than zero".to_string(),
        });
    }
    let meta = fs::symlink_metadata(path)
        .map_err(|error| io_err(format!("lstat {}", path.display()), &error))?;
    let (canonical_logical, logical_id) = match logical_path {
        Some(rel) => {
            let canonical = canonicalize_posix_path(rel)?;
            let id = logical_file_id(&canonical)?;
            (Some(canonical), Some(id))
        }
        None => (None, None),
    };

    if meta.file_type().is_symlink() {
        let target_os = fs::read_link(path)
            .map_err(|error| io_err(format!("read_link {}", path.display()), &error))?;
        let target = target_os
            .to_str()
            .ok_or_else(|| CasError::InvalidPath {
                path: path.display().to_string(),
                reason: "symlink target is not valid UTF-8".to_string(),
            })?
            .to_string();
        let sym_key = symlink_blob_key(&target);
        return Ok(KeyOfReport {
            schema_version: SCHEMA_VERSION.to_string(),
            kind: EntryKind::Symlink,
            size: u64::try_from(target.len()).unwrap_or(u64::MAX),
            blob_key: None,
            symlink_blob_key: Some(sym_key),
            symlink_target: Some(target),
            chunks: Vec::new(),
            logical_path: canonical_logical,
            logical_file_id: logical_id,
        });
    }

    if !meta.file_type().is_file() {
        return Err(CasError::InvalidPath {
            path: path.display().to_string(),
            reason: "key-of requires a regular file or symlink".to_string(),
        });
    }

    inspect_regular_file_keys(path, chunk_size, canonical_logical, logical_id)
}

fn inspect_regular_file_keys(
    path: &Path,
    chunk_size: usize,
    canonical_logical: Option<String>,
    logical_id: Option<String>,
) -> Result<KeyOfReport, CasError> {
    let mut file = open_regular_nofollow(path)?;
    let fd_meta = file
        .metadata()
        .map_err(|error| io_err(format!("fstat {}", path.display()), &error))?;
    if !fd_meta.file_type().is_file() {
        return Err(CasError::InvalidPath {
            path: path.display().to_string(),
            reason: "key-of requires a regular file or symlink".to_string(),
        });
    }
    let expected_size = fd_meta.len();
    let chunk_limit = u64::try_from(chunk_size).unwrap_or(u64::MAX);
    if expected_size <= chunk_limit {
        let mut bytes = Vec::with_capacity(usize::try_from(expected_size).unwrap_or(0));
        file.read_to_end(&mut bytes)
            .map_err(|error| io_err(format!("read {}", path.display()), &error))?;
        let actual_size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if actual_size != expected_size {
            return Err(CasError::SizeMismatch {
                key: path.display().to_string(),
                expected: expected_size,
                actual: actual_size,
                affected_paths: vec![path.display().to_string()],
            });
        }
        let key = blob_key(&bytes);
        return Ok(KeyOfReport {
            schema_version: SCHEMA_VERSION.to_string(),
            kind: EntryKind::Regular,
            size: actual_size,
            blob_key: Some(key),
            symlink_blob_key: None,
            symlink_target: None,
            chunks: Vec::new(),
            logical_path: canonical_logical,
            logical_file_id: logical_id,
        });
    }

    let mut whole_hasher = Sha256::new();
    whole_hasher.update(DOMAIN_BLOB);
    whole_hasher.update(expected_size.to_be_bytes());
    let mut chunks = Vec::new();
    let mut offset = 0u64;
    let mut index = 0u32;
    let mut buf = vec![0u8; chunk_size];

    loop {
        let n = read_full_chunk(&mut file, &mut buf, path)?;
        if n == 0 {
            break;
        }
        let slice = &buf[..n];
        whole_hasher.update(slice);
        let c_key = chunk_key(slice);
        let len_u64 = u64::try_from(n).unwrap_or(u64::MAX);
        chunks.push(ChunkRef {
            index,
            offset,
            length: len_u64,
            chunk_key: c_key,
        });
        offset = offset.saturating_add(len_u64);
        index = index.saturating_add(1);
    }

    if offset != expected_size {
        return Err(CasError::SizeMismatch {
            key: path.display().to_string(),
            expected: expected_size,
            actual: offset,
            affected_paths: vec![path.display().to_string()],
        });
    }

    let whole_key = format!("{PREFIX_BLOB}{}", encode_hex(&whole_hasher.finalize()));
    Ok(KeyOfReport {
        schema_version: SCHEMA_VERSION.to_string(),
        kind: EntryKind::Regular,
        size: offset,
        blob_key: Some(whole_key),
        symlink_blob_key: None,
        symlink_target: None,
        chunks,
        logical_path: canonical_logical,
        logical_file_id: logical_id,
    })
}

/// CLI entrypoint for `spawn backup cas <snapshot|restore|verify|key-of>`.
pub(crate) fn command(arguments: &[String]) -> Result<i32, String> {
    let Some(subcommand) = arguments.first() else {
        return Err(CAS_USAGE.to_string());
    };
    let flags = Flags::parse_with_switches(&arguments[1..], &["json"])
        .map_err(|error| format!("{error}\n{CAS_USAGE}"))?;

    match subcommand.as_str() {
        "snapshot" => cmd_snapshot(&flags),
        "restore" => cmd_restore(&flags),
        "verify" => cmd_verify(&flags),
        "key-of" => cmd_key_of(&flags),
        other => Err(format!("unknown backup cas command {other}\n{CAS_USAGE}")),
    }
}

fn reject_unknown_flags(flags: &Flags, allowed: &[&str]) -> Result<(), String> {
    for name in flags.names() {
        if !allowed.contains(&name) {
            return Err(format!("unknown flag --{name}\n{CAS_USAGE}"));
        }
    }
    Ok(())
}

fn cmd_snapshot(flags: &Flags) -> Result<i32, String> {
    reject_unknown_flags(flags, &["source", "store", "fm-graph", "parent"])?;
    let source = PathBuf::from(flags.required("source")?);
    let store_root = PathBuf::from(flags.required("store")?);
    let fm_graph_opt = match flags.get("fm-graph") {
        Some(path) => {
            let bytes = fs::read(&path).map_err(|error| format!("read {path}: {error}"))?;
            let graph: FmGraphSnapshot = serde_json::from_slice(&bytes)
                .map_err(|error| format!("parse fm-graph {path}: {error}"))?;
            Some(graph)
        }
        None => None,
    };
    let parent = flags.get("parent");

    let store = CasStore::open(&store_root).map_err(|error| error.to_string())?;
    let summary = store
        .create_snapshot(&source, fm_graph_opt.as_ref(), parent.as_deref())
        .map_err(|error| error.to_string())?;

    if flags.switch("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&summary).map_err(|error| error.to_string())?
        );
    } else {
        println!("snapshot_key: {}", summary.snapshot_key);
        if let Some(fg) = &summary.fm_graph_key {
            println!("fm_graph_key: {fg}");
        }
        println!(
            "entries: {} bytes: {} blobs: +{} ({} dedup) chunks: +{} ({} dedup) renamed: {}",
            summary.entry_count,
            summary.total_bytes,
            summary.blobs_written,
            summary.blobs_deduplicated,
            summary.chunks_written,
            summary.chunks_deduplicated,
            summary.renamed_count
        );
    }
    Ok(0)
}

fn cmd_restore(flags: &Flags) -> Result<i32, String> {
    reject_unknown_flags(flags, &["store", "snapshot", "into", "restore-fm"])?;
    let store_root = PathBuf::from(flags.required("store")?);
    let snap_key = flags.required("snapshot")?;
    let into_dir = PathBuf::from(flags.required("into")?);
    let restore_fm = flags.get("restore-fm").map(PathBuf::from);

    let store = CasStore::open_read_only(&store_root);
    let summary = store
        .restore_snapshot(&snap_key, &into_dir, restore_fm.as_deref())
        .map_err(|error| error.to_string())?;

    if flags.switch("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&summary).map_err(|error| error.to_string())?
        );
    } else {
        println!(
            "restored {} ({} entries) into {}",
            summary.snapshot_key, summary.entries_restored, summary.into
        );
        if let Some(fm_path) = &summary.fm_graph_restored_to {
            println!("fm_graph_restored_to: {fm_path}");
        }
    }
    Ok(0)
}

fn cmd_verify(flags: &Flags) -> Result<i32, String> {
    reject_unknown_flags(flags, &["store", "snapshot"])?;
    let store_root = PathBuf::from(flags.required("store")?);
    let snap_key = flags.required("snapshot")?;

    let store = CasStore::open_read_only(&store_root);
    let summary = store
        .verify_snapshot(&snap_key)
        .map_err(|error| error.to_string())?;

    if flags.switch("json") {
        let payload = json!({
            "verified": true,
            "schema_version": summary.schema_version,
            "snapshot_key": summary.snapshot_key,
            "fm_graph_key": summary.fm_graph_key,
            "entries_verified": summary.entries_verified,
            "blobs_verified": summary.blobs_verified,
            "chunks_verified": summary.chunks_verified,
            "symlinks_verified": summary.symlinks_verified,
            "features_verified": summary.features_verified,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).map_err(|error| error.to_string())?
        );
    } else {
        println!(
            "verified {} (entries: {}, blobs: {}, chunks: {}, symlinks: {}, features: {})",
            summary.snapshot_key,
            summary.entries_verified,
            summary.blobs_verified,
            summary.chunks_verified,
            summary.symlinks_verified,
            summary.features_verified
        );
    }
    Ok(0)
}

fn cmd_key_of(flags: &Flags) -> Result<i32, String> {
    reject_unknown_flags(flags, &["file", "logical-path"])?;
    let file_path = PathBuf::from(flags.required("file")?);
    let logical_path = flags.get("logical-path");

    let report = inspect_key_of(&file_path, logical_path.as_deref(), DEFAULT_CHUNK_SIZE)
        .map_err(|error| error.to_string())?;

    if flags.switch("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?
        );
    } else {
        if let Some(key) = &report.blob_key {
            println!("blob_key: {key}");
        }
        if let Some(key) = &report.symlink_blob_key {
            println!("symlink_blob_key: {key}");
        }
        if let Some(id) = &report.logical_file_id {
            println!("logical_file_id: {id}");
        }
        println!("size: {}", report.size);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "spawn-cas-unit-{label}-{}-{seq}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            make_tree_writable_best_effort(&self.path);
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn write_file_with_meta(path: &Path, data: &[u8], mode: u32, mtime_ns: i128) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        let mut file = File::create(path).expect("create file");
        file.write_all(data).expect("write data");
        file.sync_all().expect("sync");
        set_file_mode(path, mode).expect("set mode");
        set_file_mtime(&file, mtime_ns, path).expect("set mtime");
    }

    fn sample_fm_graph() -> FmGraphSnapshot {
        FmGraphSnapshot {
            schema_version: SCHEMA_VERSION.to_string(),
            features: vec![
                FmFeature {
                    id: "storage-core".to_string(),
                    title: "Recoverable CAS Core".to_string(),
                    summary: Some("Deterministic keys and blobs".to_string()),
                    parent_id: None,
                    files: vec![
                        FmFileAssoc {
                            path: "src/lib.rs".to_string(),
                            status: FileAssocStatus::Present,
                        },
                        FmFileAssoc {
                            path: "docs/external.md".to_string(),
                            status: FileAssocStatus::External,
                        },
                    ],
                    provenance: vec![FmProvenanceRef {
                        kind: "objective".to_string(),
                        reference: "objective:4/559".to_string(),
                        title: Some("Recoverable storage keys".to_string()),
                        url: None,
                    }],
                },
                FmFeature {
                    id: "cli-layer".to_string(),
                    title: "CLI Subcommand".to_string(),
                    summary: None,
                    parent_id: Some("storage-core".to_string()),
                    files: vec![FmFileAssoc {
                        path: "bin/run.sh".to_string(),
                        status: FileAssocStatus::Present,
                    }],
                    provenance: Vec::new(),
                },
            ],
            relations: vec![FmRelation {
                from: "cli-layer".to_string(),
                to: "storage-core".to_string(),
                kind: RelationKind::DependsOn,
            }],
            boundaries: FmBoundaries {
                include_roots: vec!["src".to_string(), "bin".to_string()],
                exclude_globs: vec!["target/**".to_string()],
            },
        }
    }

    #[test]
    fn deterministic_keys_and_domain_separation() {
        let data = b"hello recoverable storage";
        let b_key = blob_key(data);
        let c_key = chunk_key(data);
        let s_key = symlink_blob_key("hello recoverable storage");

        assert!(b_key.starts_with(PREFIX_BLOB));
        assert!(c_key.starts_with(PREFIX_CHUNK));
        assert!(s_key.starts_with(PREFIX_SYMLINK));
        // Same payload under different domains MUST yield distinct digests.
        assert_ne!(
            b_key.strip_prefix(PREFIX_BLOB),
            c_key.strip_prefix(PREFIX_CHUNK)
        );
        assert_ne!(
            b_key.strip_prefix(PREFIX_BLOB),
            s_key.strip_prefix(PREFIX_SYMLINK)
        );

        let id1 = logical_file_id("src/lib.rs").expect("logical id");
        let id2 = logical_file_id("src/other.rs").expect("logical id");
        assert!(id1.starts_with(PREFIX_LOGICAL));
        assert_ne!(id1, id2);
        assert_eq!(id1, logical_file_id("src/lib.rs").expect("deterministic"));

        // Transport key is deterministic per master key and distinct across keys.
        let t1 = transport_key(b"master-key-alpha", &b_key);
        let t2 = transport_key(b"master-key-alpha", &b_key);
        let t3 = transport_key(b"master-key-beta", &b_key);
        assert_eq!(t1.len(), 64);
        assert_eq!(t1, t2);
        assert_ne!(t1, t3);
        // Must not reveal inner digest.
        assert!(!b_key.ends_with(&t1));
    }

    #[test]
    fn deterministic_snapshot_across_independent_stores() {
        let src = TempDir::new("det-src");
        let store_a = TempDir::new("det-store-a");
        let store_b = TempDir::new("det-store-b");

        let fixed_mtime = 1_750_000_000_123_456_789_i128;
        write_file_with_meta(
            &src.path().join("src/lib.rs"),
            b"pub fn a() {}",
            0o644,
            fixed_mtime,
        );
        write_file_with_meta(
            &src.path().join("bin/run.sh"),
            b"#!/bin/sh\necho hi\n",
            0o755,
            fixed_mtime,
        );
        let graph = sample_fm_graph();

        let cas_a = CasStore::open(store_a.path()).expect("open a");
        let cas_b = CasStore::open(store_b.path()).expect("open b");
        let snap_a = cas_a
            .create_snapshot(src.path(), Some(&graph), None)
            .expect("snap a");
        let snap_b = cas_b
            .create_snapshot(src.path(), Some(&graph), None)
            .expect("snap b");

        assert_eq!(snap_a.snapshot_key, snap_b.snapshot_key);
        assert_eq!(snap_a.fm_graph_key, snap_b.fm_graph_key);

        // Permuting feature/file ordering in the FM graph produces the same key.
        let mut permuted = graph.clone();
        permuted.features.reverse();
        permuted.features[1].files.reverse();
        assert_eq!(
            fm_graph_key(&graph).expect("k1"),
            fm_graph_key(&permuted).expect("k2")
        );
    }

    #[test]
    fn duplicates_and_renames_distinguish_blob_from_logical_identity() {
        let src = TempDir::new("dup-src");
        let store_dir = TempDir::new("dup-store");
        let cas = CasStore::open(store_dir.path()).expect("open");

        let mtime = 1_750_000_100_000_000_000_i128;
        write_file_with_meta(
            &src.path().join("dup_a.txt"),
            b"identical payload",
            0o644,
            mtime,
        );
        write_file_with_meta(
            &src.path().join("dup_b.txt"),
            b"identical payload",
            0o644,
            mtime,
        );
        write_file_with_meta(
            &src.path().join("original.txt"),
            b"will be renamed",
            0o644,
            mtime,
        );

        let snap1 = cas.create_snapshot(src.path(), None, None).expect("snap1");
        assert_eq!(snap1.blobs_written, 2);
        assert_eq!(snap1.blobs_deduplicated, 1);

        let m1 = cas
            .load_and_verify_manifest(&snap1.snapshot_key)
            .expect("m1");
        let entry_a = m1.entries.iter().find(|e| e.path == "dup_a.txt").unwrap();
        let entry_b = m1.entries.iter().find(|e| e.path == "dup_b.txt").unwrap();
        assert_eq!(entry_a.blob_key, entry_b.blob_key);
        assert_ne!(entry_a.logical_file_id, entry_b.logical_file_id);

        // Rename `original.txt` -> `moved/renamed.txt`
        fs::create_dir_all(src.path().join("moved")).expect("mkdir");
        fs::rename(
            src.path().join("original.txt"),
            src.path().join("moved/renamed.txt"),
        )
        .expect("rename");

        let snap2 = cas
            .create_snapshot(src.path(), None, Some(&snap1.snapshot_key))
            .expect("snap2");
        assert_eq!(snap2.renamed_count, 1);
        assert_eq!(snap2.blobs_written, 0);

        let m2 = cas
            .load_and_verify_manifest(&snap2.snapshot_key)
            .expect("m2");
        let old_entry = m1
            .entries
            .iter()
            .find(|e| e.path == "original.txt")
            .unwrap();
        let new_entry = m2
            .entries
            .iter()
            .find(|e| e.path == "moved/renamed.txt")
            .unwrap();
        assert_eq!(old_entry.blob_key, new_entry.blob_key);
        assert_ne!(old_entry.logical_file_id, new_entry.logical_file_id);
        assert_eq!(new_entry.renamed_from.as_deref(), Some("original.txt"));
    }

    #[test]
    fn multi_chunk_empty_binary_symlink_and_full_round_trip() {
        let src = TempDir::new("rt-src");
        let store_dir = TempDir::new("rt-store");
        let restore_dir = TempDir::new("rt-out");
        let fm_dir = TempDir::new("rt-fm");
        let restored_fm = fm_dir.path().join("restored-fm.json");

        // Use a small chunk size (1024 bytes) to exercise multi-chunk storage
        // and reassembly without allocating huge files.
        let cas = CasStore::open_with_chunk_size(store_dir.path(), 1024).expect("open");
        let mtime = 1_750_000_222_333_444_000_i128;

        // Empty file
        write_file_with_meta(&src.path().join("empty.bin"), b"", 0o644, mtime);
        // Binary file with all 256 byte values
        let all_bytes: Vec<u8> = (0u8..=255).collect();
        write_file_with_meta(&src.path().join("src/lib.rs"), &all_bytes, 0o644, mtime);
        // Executable multi-chunk file (3500 bytes -> 4 chunks: 1024, 1024, 1024, 428)
        let large: Vec<u8> = (0..3500).map(|i| u8::try_from(i % 251).unwrap()).collect();
        write_file_with_meta(&src.path().join("bin/run.sh"), &large, 0o755, mtime);
        // Empty nested directory
        fs::create_dir_all(src.path().join("empty_dir/nested")).expect("empty dir");
        // Relative symlink
        create_symlink("../src/lib.rs", &src.path().join("bin/link_to_lib")).expect("symlink");

        let graph = sample_fm_graph();
        let snap = cas
            .create_snapshot(src.path(), Some(&graph), None)
            .expect("snapshot");
        assert_eq!(snap.chunks_written, 4);

        let verified = cas.verify_snapshot(&snap.snapshot_key).expect("verify");
        assert_eq!(verified.chunks_verified, 4);
        assert_eq!(verified.symlinks_verified, 1);
        assert_eq!(verified.features_verified, 2);

        let out_target = restore_dir.path().join("restored-tree");
        let restored = cas
            .restore_snapshot(&snap.snapshot_key, &out_target, Some(&restored_fm))
            .expect("restore");
        assert_eq!(restored.entries_restored, snap.entry_count);

        assert_eq!(fs::read(out_target.join("empty.bin")).unwrap(), b"");
        assert_eq!(fs::read(out_target.join("src/lib.rs")).unwrap(), all_bytes);
        assert_eq!(fs::read(out_target.join("bin/run.sh")).unwrap(), large);
        assert!(out_target.join("empty_dir/nested").is_dir());
        assert_eq!(
            fs::read_link(out_target.join("bin/link_to_lib")).unwrap(),
            PathBuf::from("../src/lib.rs")
        );

        let run_meta = fs::symlink_metadata(out_target.join("bin/run.sh")).unwrap();
        assert_eq!(extract_mode(&run_meta), 0o755);
        assert_eq!(extract_mtime_ns(&run_meta).unwrap(), mtime);

        let lib_meta = fs::symlink_metadata(out_target.join("src/lib.rs")).unwrap();
        assert_eq!(extract_mode(&lib_meta), 0o644);
        assert_eq!(extract_mtime_ns(&lib_meta).unwrap(), mtime);

        let restored_graph: FmGraphSnapshot =
            serde_json::from_slice(&fs::read(&restored_fm).unwrap()).unwrap();
        assert_eq!(
            fm_graph_key(&restored_graph).unwrap(),
            snap.fm_graph_key.unwrap()
        );
    }

    #[test]
    fn detects_single_bit_corruption_and_missing_objects() {
        let src = TempDir::new("corrupt-src");
        let store_dir = TempDir::new("corrupt-store");
        let cas = CasStore::open_with_chunk_size(store_dir.path(), 512).expect("open");

        let mtime = 1_750_000_300_000_000_000_i128;
        write_file_with_meta(
            &src.path().join("small.txt"),
            b"integrity test",
            0o644,
            mtime,
        );
        let big = vec![0x5Au8; 1200];
        write_file_with_meta(&src.path().join("big.bin"), &big, 0o644, mtime);

        let snap = cas.create_snapshot(src.path(), None, None).expect("snap");
        let manifest = cas.load_and_verify_manifest(&snap.snapshot_key).unwrap();
        let small_key = manifest
            .entries
            .iter()
            .find(|e| e.path == "small.txt")
            .unwrap()
            .blob_key
            .clone()
            .unwrap();
        let chunk0_key = manifest
            .entries
            .iter()
            .find(|e| e.path == "big.bin")
            .unwrap()
            .chunks[0]
            .chunk_key
            .clone();

        // 1. Flip 1 bit in chunk 0 -> detected with exact key & path
        let chunk_path = cas.chunk_path(&chunk0_key).unwrap();
        let orig_chunk = fs::read(&chunk_path).unwrap();
        let mut flipped = orig_chunk.clone();
        flipped[17] ^= 0x01;
        fs::write(&chunk_path, &flipped).unwrap();
        let err = cas
            .verify_snapshot(&snap.snapshot_key)
            .expect_err("must detect chunk bit flip");
        assert!(matches!(
            err,
            CasError::CorruptedObject { ref key, ref affected_paths, .. }
                if key == &chunk0_key && affected_paths.contains(&"big.bin".to_string())
        ));
        fs::write(&chunk_path, &orig_chunk).unwrap();

        // 2. Flip 1 bit in regular blob -> detected on verify AND on re-snapshot write-once check
        let blob_path = cas.blob_path(&small_key).unwrap();
        let orig_blob = fs::read(&blob_path).unwrap();
        let mut flipped_blob = orig_blob.clone();
        flipped_blob[0] ^= 0x80;
        fs::write(&blob_path, &flipped_blob).unwrap();
        let err = cas
            .verify_snapshot(&snap.snapshot_key)
            .expect_err("must detect blob bit flip");
        assert!(matches!(
            err,
            CasError::CorruptedObject { ref key, ref affected_paths, .. }
                if key == &small_key && affected_paths.contains(&"small.txt".to_string())
        ));
        // Re-snapshotting must NOT blindly trust corrupt existing blob!
        let err_resnap = cas
            .create_snapshot(src.path(), None, None)
            .expect_err("write-once must detect existing on-disk corruption");
        assert!(matches!(err_resnap, CasError::CorruptedObject { .. }));

        // 3. Delete blob -> MissingObject naming exact key and path
        fs::remove_file(&blob_path).unwrap();
        let err_missing = cas
            .verify_snapshot(&snap.snapshot_key)
            .expect_err("must detect missing blob");
        assert!(matches!(
            err_missing,
            CasError::MissingObject { ref key, ref affected_paths }
                if key == &small_key && affected_paths.contains(&"small.txt".to_string())
        ));
        fs::write(&blob_path, &orig_blob).unwrap();
        cas.verify_snapshot(&snap.snapshot_key)
            .expect("restored blob verifies cleanly");
    }

    #[test]
    fn fm_graph_consistency_rejects_dangling_cycles_and_missing_files() {
        let src = TempDir::new("fmc-src");
        let store_dir = TempDir::new("fmc-store");
        let cas = CasStore::open(store_dir.path()).expect("open");
        write_file_with_meta(
            &src.path().join("a.rs"),
            b"fn a(){}",
            0o644,
            1_750_000_000_000_000_000,
        );

        // Dangling relation
        let dangling = FmGraphSnapshot {
            schema_version: SCHEMA_VERSION.to_string(),
            features: vec![FmFeature {
                id: "f1".to_string(),
                title: "F1".to_string(),
                summary: None,
                parent_id: None,
                files: vec![FmFileAssoc {
                    path: "a.rs".to_string(),
                    status: FileAssocStatus::Present,
                }],
                provenance: Vec::new(),
            }],
            relations: vec![FmRelation {
                from: "f1".to_string(),
                to: "nonexistent".to_string(),
                kind: RelationKind::DependsOn,
            }],
            boundaries: FmBoundaries::default(),
        };
        assert!(matches!(
            cas.create_snapshot(src.path(), Some(&dangling), None),
            Err(CasError::FmGraphConsistency { .. })
        ));

        // DependsOn cycle
        let cyclic = FmGraphSnapshot {
            schema_version: SCHEMA_VERSION.to_string(),
            features: vec![
                FmFeature {
                    id: "f1".to_string(),
                    title: "F1".to_string(),
                    summary: None,
                    parent_id: None,
                    files: Vec::new(),
                    provenance: Vec::new(),
                },
                FmFeature {
                    id: "f2".to_string(),
                    title: "F2".to_string(),
                    summary: None,
                    parent_id: None,
                    files: Vec::new(),
                    provenance: Vec::new(),
                },
            ],
            relations: vec![
                FmRelation {
                    from: "f1".to_string(),
                    to: "f2".to_string(),
                    kind: RelationKind::DependsOn,
                },
                FmRelation {
                    from: "f2".to_string(),
                    to: "f1".to_string(),
                    kind: RelationKind::DependsOn,
                },
            ],
            boundaries: FmBoundaries::default(),
        };
        assert!(matches!(
            cas.create_snapshot(src.path(), Some(&cyclic), None),
            Err(CasError::FmGraphConsistency { .. })
        ));

        // Missing file not marked external/missing
        let missing_file = FmGraphSnapshot {
            schema_version: SCHEMA_VERSION.to_string(),
            features: vec![FmFeature {
                id: "f1".to_string(),
                title: "F1".to_string(),
                summary: None,
                parent_id: None,
                files: vec![FmFileAssoc {
                    path: "does_not_exist.rs".to_string(),
                    status: FileAssocStatus::Present,
                }],
                provenance: Vec::new(),
            }],
            relations: Vec::new(),
            boundaries: FmBoundaries::default(),
        };
        assert!(matches!(
            cas.create_snapshot(src.path(), Some(&missing_file), None),
            Err(CasError::FmGraphConsistency { .. })
        ));
    }

    #[test]
    fn concurrent_snapshots_and_interrupted_staging_files_are_safe() {
        let store_dir = TempDir::new("conc-store");
        let cas = Arc::new(CasStore::open(store_dir.path()).expect("open"));

        // Plant a simulated crashed/interrupted staging temp file.
        let stray = cas.staging_dir().join(".tmp-999999-42");
        fs::write(&stray, b"partial crashed payload").expect("plant stray");

        let mut handles = Vec::new();
        for worker in 0..6u32 {
            let cas_clone = Arc::clone(&cas);
            handles.push(thread::spawn(move || {
                let src = TempDir::new(&format!("conc-src-{worker}"));
                let mtime = 1_750_000_500_000_000_000_i128;
                // Shared content across all threads (exercises concurrent dedup race)
                write_file_with_meta(
                    &src.path().join("shared.txt"),
                    b"shared across workers",
                    0o644,
                    mtime,
                );
                // Unique content per thread
                write_file_with_meta(
                    &src.path().join(format!("worker_{worker}.txt")),
                    format!("payload from worker {worker}").as_bytes(),
                    0o644,
                    mtime,
                );
                let snap = cas_clone
                    .create_snapshot(src.path(), None, None)
                    .expect("concurrent snapshot");
                cas_clone
                    .verify_snapshot(&snap.snapshot_key)
                    .expect("verify concurrent snapshot");
                snap.snapshot_key
            }));
        }

        let keys: Vec<String> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let index = cas.read_index().expect("read index");
        for key in &keys {
            assert!(
                index
                    .snapshots
                    .iter()
                    .any(|entry| &entry.snapshot_key == key),
                "missing {key} in snapshot index"
            );
        }

        // Sweep cleans only staging temp files and leaves all snapshots valid.
        let swept = cas.sweep_staging().expect("sweep");
        assert_eq!(swept, 1);
        assert!(!stray.exists());
        for key in &keys {
            cas.verify_snapshot(key).expect("still valid after sweep");
        }
    }

    #[test]
    fn refuses_unsafe_paths_and_escaping_symlinks() {
        assert!(canonicalize_posix_path("../escape").is_err());
        assert!(canonicalize_posix_path("/etc/passwd").is_err());
        assert!(canonicalize_posix_path("a/.git/config").is_err());
        assert!(canonicalize_posix_path("a//b").is_err());
        assert!(canonicalize_posix_path("a/./b").is_err());

        assert!(validate_symlink_target("bin/link", "../src/lib.rs").is_ok());
        assert!(validate_symlink_target("bin/link", "../../escape").is_err());
        assert!(validate_symlink_target("bin/link", "/etc/passwd").is_err());
        assert!(validate_symlink_target("bin/link", "../.git/HEAD").is_err());
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn round_trip_restore_preserves_dir_and_symlink_mtimes_and_snapshot_key_determinism() {
        let src = TempDir::new("det-rt-src");
        let store1 = TempDir::new("det-rt-store1");
        let store2 = TempDir::new("det-rt-store2");
        let restore_parent = TempDir::new("det-rt-out");
        let restored_tree = restore_parent.path().join("tree");

        let cas1 = CasStore::open_with_chunk_size(store1.path(), 512).expect("open 1");
        let cas2 = CasStore::open_with_chunk_size(store2.path(), 512).expect("open 2");

        let file_mtime = 1_750_000_111_222_333_444_i128;
        let pre_epoch_mtime = -86_400_500_000_000_i128; // 1969-12-30T23:59:59.500Z
        let sym_mtime = 1_750_000_222_111_000_999_i128;
        let dir_mtime = 1_750_000_333_444_555_666_i128;

        write_file_with_meta(
            &src.path().join("src/lib.rs"),
            b"pub fn ro() {}\n",
            0o644,
            file_mtime,
        );
        write_file_with_meta(
            &src.path().join("bin/run.sh"),
            b"#!/bin/sh\nexit 0\n",
            0o755,
            pre_epoch_mtime,
        );
        write_file_with_meta(
            &src.path().join("ro_dir/secret.txt"),
            b"readonly data",
            0o444,
            file_mtime,
        );
        let big = vec![0x33u8; 1300];
        write_file_with_meta(
            &src.path().join("data/chunked.bin"),
            &big,
            0o644,
            file_mtime,
        );
        fs::create_dir_all(src.path().join("empty/deep")).expect("empty dir");
        create_symlink("../src/lib.rs", &src.path().join("bin/lib_link")).expect("symlink");
        set_path_mtime_nofollow(&src.path().join("bin/lib_link"), sym_mtime).expect("sym mtime");

        // Set explicit deterministic mtimes on all directories in reverse depth order,
        // and apply restrictive 0o555 mode on `ro_dir`.
        for rel in ["empty/deep", "empty", "src", "bin", "data", "ro_dir"] {
            set_path_mtime_nofollow(&src.path().join(rel), dir_mtime).expect("dir mtime");
        }
        set_file_mode(&src.path().join("ro_dir"), 0o555).expect("chmod ro_dir");

        let graph = sample_fm_graph();
        let snap1 = cas1
            .create_snapshot(src.path(), Some(&graph), None)
            .expect("snap1");

        // Make source writable again before TempDir drop can remove it.
        set_file_mode(&src.path().join("ro_dir"), 0o755).expect("restore ro_dir mode");

        cas1.restore_snapshot(&snap1.snapshot_key, &restored_tree, None)
            .expect("restore");

        // Verify restored modes & mtimes including pre-1970, symlink, and dirs.
        assert_eq!(
            extract_mode(&fs::symlink_metadata(restored_tree.join("ro_dir")).unwrap()),
            0o555
        );
        assert_eq!(
            extract_mode(&fs::symlink_metadata(restored_tree.join("ro_dir/secret.txt")).unwrap()),
            0o444
        );
        assert_eq!(
            extract_mtime_ns(&fs::symlink_metadata(restored_tree.join("bin/run.sh")).unwrap())
                .unwrap(),
            pre_epoch_mtime
        );
        assert_eq!(
            extract_mtime_ns(&fs::symlink_metadata(restored_tree.join("bin/lib_link")).unwrap())
                .unwrap(),
            sym_mtime
        );
        for rel in ["empty/deep", "empty", "src", "bin", "data", "ro_dir"] {
            assert_eq!(
                extract_mtime_ns(&fs::symlink_metadata(restored_tree.join(rel)).unwrap()).unwrap(),
                dir_mtime,
                "directory mtime mismatch for {rel}"
            );
        }

        // Re-snapshotting the restored tree yields the EXACT same snapshot_key!
        let snap2 = cas2
            .create_snapshot(&restored_tree, Some(&graph), None)
            .expect("snap2");
        // Restore writable mode so TempDir drops cleanly.
        set_file_mode(&restored_tree.join("ro_dir"), 0o755).expect("cleanup chmod");
        assert_eq!(snap1.snapshot_key, snap2.snapshot_key);
        assert_eq!(snap1.fm_graph_key, snap2.fm_graph_key);
    }

    #[test]
    fn detects_manifest_raw_byte_tampering_and_unknown_fields() {
        let src = TempDir::new("tamper-src");
        let store_dir = TempDir::new("tamper-store");
        let cas = CasStore::open(store_dir.path()).expect("open");
        let mtime = 1_750_000_000_000_000_000_i128;
        write_file_with_meta(&src.path().join("a.txt"), b"alpha", 0o644, mtime);
        write_file_with_meta(&src.path().join("b.txt"), b"beta", 0o644, mtime);

        let snap = cas.create_snapshot(src.path(), None, None).expect("snap");
        let manifest_path = cas.snapshot_path(&snap.snapshot_key).unwrap();
        let orig = fs::read(&manifest_path).unwrap();

        // 1. Trailing whitespace tampering -> rejected on verify AND re-snapshot!
        let mut with_ws = orig.clone();
        with_ws.push(b'\n');
        fs::write(&manifest_path, &with_ws).unwrap();
        assert!(matches!(
            cas.verify_snapshot(&snap.snapshot_key),
            Err(CasError::ManifestIntegrity { .. })
        ));
        assert!(matches!(
            cas.create_snapshot(src.path(), None, None),
            Err(CasError::CorruptedObject { .. })
        ));

        // 2. Unknown injected JSON field -> rejected via deny_unknown_fields!
        let mut val: Value = serde_json::from_slice(&orig).unwrap();
        val.as_object_mut()
            .unwrap()
            .insert("injected_field".to_string(), json!("evil"));
        fs::write(&manifest_path, serde_json::to_vec(&val).unwrap()).unwrap();
        assert!(matches!(
            cas.verify_snapshot(&snap.snapshot_key),
            Err(CasError::ManifestIntegrity { .. })
        ));

        // 3. Reordered entries array (non-canonical on disk) -> rejected!
        let mut reordered: Value = serde_json::from_slice(&orig).unwrap();
        reordered["entries"].as_array_mut().unwrap().reverse();
        fs::write(&manifest_path, serde_json::to_vec(&reordered).unwrap()).unwrap();
        assert!(matches!(
            cas.verify_snapshot(&snap.snapshot_key),
            Err(CasError::ManifestIntegrity { .. })
        ));

        // Restore original bytes -> verifies cleanly again.
        fs::write(&manifest_path, &orig).unwrap();
        cas.verify_snapshot(&snap.snapshot_key).expect("clean");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn rejects_crafted_manifest_prefix_collision_dup_size_mismatch_and_bad_parent() {
        let store_dir = TempDir::new("crafted-store");
        let cas = CasStore::open(store_dir.path()).expect("open");

        // Store one valid 5-byte blob ("hello")
        let hello = b"hello";
        let hello_key = blob_key(hello);
        let hello_digest = parse_key_digest(&hello_key, PREFIX_BLOB).unwrap();
        let hello_dest = cas.blob_path(&hello_key).unwrap();
        cas.write_object_once(&hello_dest, hello, &hello_key, hello_digest, |b| {
            sha256_len_prefixed(DOMAIN_BLOB, b)
        })
        .unwrap();

        let write_crafted_manifest = |body: SnapshotManifestBody| -> String {
            let canonical = body.canonicalized();
            let key = snapshot_key(&canonical).unwrap();
            let manifest = SnapshotManifest::from_body(key.clone(), canonical);
            let bytes = canonical_json_bytes(&manifest).unwrap();
            let dest = cas.snapshot_path(&key).unwrap();
            fs::create_dir_all(dest.parent().unwrap()).unwrap();
            fs::write(&dest, &bytes).unwrap();
            key
        };

        let make_reg = |path: &str, size: u64| SnapshotEntry {
            path: path.to_string(),
            logical_file_id: logical_file_id(path).unwrap(),
            kind: EntryKind::Regular,
            mode: 0o644,
            mtime_ns: 1_750_000_000_000_000_000,
            size,
            blob_key: Some(hello_key.clone()),
            chunks: Vec::new(),
            symlink_blob_key: None,
            symlink_target: None,
            renamed_from: None,
            feature_ids: Vec::new(),
        };

        // 1. Path hierarchy collision: regular file at "a" AND regular file at "a/b"
        let collision_key = write_crafted_manifest(SnapshotManifestBody {
            schema_version: SCHEMA_VERSION.to_string(),
            chunk_size: DEFAULT_CHUNK_SIZE as u64,
            parent_snapshot_key: None,
            fm_graph_key: None,
            entries: vec![make_reg("a", 5), make_reg("a/b", 5)],
            skipped_specials: Vec::new(),
            unsupported_semantics: UnsupportedSemantics::v1_defaults(),
            compatibility: CompatibilityMetadata::v1_defaults(),
        });
        assert!(matches!(
            cas.verify_snapshot(&collision_key),
            Err(CasError::ManifestIntegrity { ref reason, .. }) if reason.contains("hierarchy conflict")
        ));

        // 2. Duplicate blob_key where 2nd entry lies about entry.size
        let dup_size_key = write_crafted_manifest(SnapshotManifestBody {
            schema_version: SCHEMA_VERSION.to_string(),
            chunk_size: DEFAULT_CHUNK_SIZE as u64,
            parent_snapshot_key: None,
            fm_graph_key: None,
            entries: vec![make_reg("first.txt", 5), make_reg("second.txt", 999)],
            skipped_specials: Vec::new(),
            unsupported_semantics: UnsupportedSemantics::v1_defaults(),
            compatibility: CompatibilityMetadata::v1_defaults(),
        });
        assert!(matches!(
            cas.verify_snapshot(&dup_size_key),
            Err(CasError::SizeMismatch {
                expected: 999,
                actual: 5,
                ..
            })
        ));

        // 3. Invalid parent_snapshot_key syntax
        let bad_parent_key = write_crafted_manifest(SnapshotManifestBody {
            schema_version: SCHEMA_VERSION.to_string(),
            chunk_size: DEFAULT_CHUNK_SIZE as u64,
            parent_snapshot_key: Some("not-a-snapshot-key".to_string()),
            fm_graph_key: None,
            entries: vec![make_reg("ok.txt", 5)],
            skipped_specials: Vec::new(),
            unsupported_semantics: UnsupportedSemantics::v1_defaults(),
            compatibility: CompatibilityMetadata::v1_defaults(),
        });
        assert!(matches!(
            cas.verify_snapshot(&bad_parent_key),
            Err(CasError::InvalidKey { .. })
        ));

        // 4. Conflicting file statuses in FM graph
        let conflicting_fm = FmGraphSnapshot {
            schema_version: SCHEMA_VERSION.to_string(),
            features: vec![FmFeature {
                id: "f1".to_string(),
                title: "F1".to_string(),
                summary: None,
                parent_id: None,
                files: vec![
                    FmFileAssoc {
                        path: "ok.txt".to_string(),
                        status: FileAssocStatus::Present,
                    },
                    FmFileAssoc {
                        path: "ok.txt".to_string(),
                        status: FileAssocStatus::Missing,
                    },
                ],
                provenance: Vec::new(),
            }],
            relations: Vec::new(),
            boundaries: FmBoundaries::default(),
        };
        assert!(matches!(
            conflicting_fm.validate_structure(),
            Err(CasError::FmGraphConsistency { ref reason }) if reason.contains("conflicting statuses")
        ));

        // 5. Incompatible index schema_version rejected
        fs::write(
            cas.index_path(),
            br#"{"schema_version":"fm-cas/v99","snapshots":[]}"#,
        )
        .unwrap();
        assert!(matches!(
            cas.read_index(),
            Err(CasError::ManifestIntegrity { .. })
        ));
    }

    #[test]
    fn rejects_symlink_chain_root_escape_and_loops_on_restore() {
        let src = TempDir::new("symchain-src");
        let store_dir = TempDir::new("symchain-store");
        let cas = CasStore::open(store_dir.path()).expect("open");

        // 2-hop escape: a/b/jump -> ../.. (resolves to root), a/b/escape -> jump/../../etc/passwd
        fs::create_dir_all(src.path().join("a/b")).unwrap();
        create_symlink("../..", &src.path().join("a/b/jump")).unwrap();
        create_symlink("jump/../../etc/passwd", &src.path().join("a/b/escape")).unwrap();

        let snap = cas.create_snapshot(src.path(), None, None).expect("snap");
        cas.verify_snapshot(&snap.snapshot_key)
            .expect("verify validates content integrity");

        let out_parent = TempDir::new("symchain-out");
        let out_dir = out_parent.path().join("target");
        let err = cas
            .restore_snapshot(&snap.snapshot_key, &out_dir, None)
            .expect_err("must refuse symlink chain escape");
        assert!(matches!(err, CasError::UnsafeRestoreTarget { .. }));
        assert!(
            !out_dir.exists(),
            "must not create restore dir on pre-flight failure"
        );

        // Symlink loop: loop/x -> y, loop/y -> x
        let loop_src = TempDir::new("symloop-src");
        fs::create_dir_all(loop_src.path().join("loop")).unwrap();
        create_symlink("y", &loop_src.path().join("loop/x")).unwrap();
        create_symlink("x", &loop_src.path().join("loop/y")).unwrap();

        let loop_snap = cas
            .create_snapshot(loop_src.path(), None, None)
            .expect("loop snap");
        let loop_out = out_parent.path().join("loop_target");
        let loop_err = cas
            .restore_snapshot(&loop_snap.snapshot_key, &loop_out, None)
            .expect_err("must refuse symlink loop");
        assert!(matches!(loop_err, CasError::UnsafeRestoreTarget { .. }));
        assert!(!loop_out.exists());
    }

    #[test]
    fn preflight_restore_fm_and_dangling_symlink_target_checks_leave_no_output() {
        let src = TempDir::new("pf-src");
        let store_dir = TempDir::new("pf-store");
        let cas = CasStore::open(store_dir.path()).expect("open");
        let mtime = 1_750_000_000_000_000_000_i128;
        write_file_with_meta(&src.path().join("src/lib.rs"), b"fn x() {}", 0o644, mtime);
        write_file_with_meta(&src.path().join("bin/run.sh"), b"#!/bin/sh\n", 0o755, mtime);

        let snap_no_fm = cas.create_snapshot(src.path(), None, None).expect("no fm");
        let snap_with_fm = cas
            .create_snapshot(src.path(), Some(&sample_fm_graph()), None)
            .expect("with fm");

        let scratch = TempDir::new("pf-out");

        // 1. --restore-fm when snapshot has no fm_graph_key -> fails BEFORE creating into_dir
        let target1 = scratch.path().join("t1");
        let err1 = cas
            .restore_snapshot(
                &snap_no_fm.snapshot_key,
                &target1,
                Some(&scratch.path().join("fm1.json")),
            )
            .expect_err("must refuse missing fm_graph");
        assert!(matches!(err1, CasError::FmGraphConsistency { .. }));
        assert!(
            !target1.exists(),
            "into_dir must not be created when --restore-fm fails pre-flight"
        );

        // 2. --restore-fm pointing to a dangling symlink -> fails BEFORE creating into_dir
        let dangling_fm = scratch.path().join("dangling_fm.json");
        create_symlink("does-not-exist", &dangling_fm).unwrap();
        let target2 = scratch.path().join("t2");
        let err2 = cas
            .restore_snapshot(&snap_with_fm.snapshot_key, &target2, Some(&dangling_fm))
            .expect_err("must refuse dangling symlink fm_out");
        assert!(matches!(err2, CasError::UnsafeRestoreTarget { .. }));
        assert!(!target2.exists());

        // 3. --restore-fm inside into_dir -> fails BEFORE creating into_dir
        let target3 = scratch.path().join("t3");
        let err3 = cas
            .restore_snapshot(
                &snap_with_fm.snapshot_key,
                &target3,
                Some(&target3.join("fm.json")),
            )
            .expect_err("must refuse fm_out inside into_dir");
        assert!(matches!(err3, CasError::UnsafeRestoreTarget { .. }));
        assert!(!target3.exists());

        // 4. --into pointing to a dangling symlink -> refused cleanly
        let dangling_into = scratch.path().join("dangling_into");
        create_symlink("nowhere", &dangling_into).unwrap();
        let err4 = cas
            .restore_snapshot(&snap_no_fm.snapshot_key, &dangling_into, None)
            .expect_err("must refuse dangling symlink into_dir");
        assert!(matches!(err4, CasError::UnsafeRestoreTarget { .. }));
    }

    #[test]
    fn sweep_staging_preserves_live_pid_and_symlinks_and_read_only_store_works() {
        let src = TempDir::new("ro-src");
        let store_dir = TempDir::new("ro-store");
        let cas = CasStore::open(store_dir.path()).expect("open");
        let mtime = 1_750_000_000_000_000_000_i128;
        write_file_with_meta(
            &src.path().join("hello.txt"),
            b"readonly store test",
            0o644,
            mtime,
        );
        let snap = cas.create_snapshot(src.path(), None, None).expect("snap");

        // Plant live-PID temp file, dead-PID temp file, and symlink inside staging.
        let live_tmp = cas
            .staging_dir()
            .join(format!(".tmp-{}-9999", std::process::id()));
        let dead_tmp = cas.staging_dir().join(".tmp-999999-1");
        let sym_tmp = cas.staging_dir().join(".tmp-999999-sym");
        fs::write(&live_tmp, b"active writer").unwrap();
        fs::write(&dead_tmp, b"crashed writer").unwrap();
        create_symlink("hello", &sym_tmp).unwrap();

        let swept = cas.sweep_staging().expect("sweep");
        assert_eq!(swept, 1, "only dead-PID regular temp file should be swept");
        assert!(live_tmp.exists(), "live writer temp file must be preserved");
        assert!(!dead_tmp.exists(), "dead PID temp file should be removed");
        assert!(
            fs::symlink_metadata(&sym_tmp).is_ok(),
            "symlink in staging must not be followed/removed"
        );
        fs::remove_file(&live_tmp).unwrap();
        fs::remove_file(&sym_tmp).unwrap();

        // Read-only open on an empty/missing dir MUST NOT create fm-cas/v1/
        let untouched_dir = TempDir::new("untouched-store");
        let ro_empty = CasStore::open_read_only(untouched_dir.path());
        assert!(matches!(
            ro_empty.verify_snapshot(&snap.snapshot_key),
            Err(CasError::MissingObject { .. })
        ));
        assert!(
            !untouched_dir.path().join("fm-cas").exists(),
            "open_read_only must not mutate store directory"
        );

        // inspect_key_of rejects zero chunk size
        assert!(matches!(
            inspect_key_of(&src.path().join("hello.txt"), None, 0),
            Err(CasError::InvalidKey { .. })
        ));
    }

    #[test]
    fn skips_unix_socket_special_file_supports_parentless_delta_restore_and_rolls_back_cleanly() {
        let src = TempDir::new("sock-src");
        let store1_dir = TempDir::new("sock-store1");
        let store2_dir = TempDir::new("sock-store2");
        let cas1 = CasStore::open(store1_dir.path()).expect("open 1");
        let cas2 = CasStore::open(store2_dir.path()).expect("open 2");

        let mtime = 1_750_000_000_000_000_000_i128;
        write_file_with_meta(
            &src.path().join("before.txt"),
            b"socket test payload",
            0o644,
            mtime,
        );
        #[cfg(unix)]
        let _listener = std::os::unix::net::UnixListener::bind(src.path().join("ctrl.sock"))
            .expect("bind unix socket");

        let snap1 = cas1.create_snapshot(src.path(), None, None).expect("snap1");
        #[cfg(unix)]
        assert_eq!(
            snap1.skipped_specials,
            vec![SkippedSpecial {
                path: "ctrl.sock".to_string(),
                kind: "socket".to_string()
            }]
        );

        // Rename file and create snap2 with snap1 as parent.
        fs::rename(src.path().join("before.txt"), src.path().join("after.txt")).unwrap();
        let snap2 = cas1
            .create_snapshot(src.path(), None, Some(&snap1.snapshot_key))
            .expect("snap2");
        assert_eq!(snap2.renamed_count, 1);

        // Delta-replicate ONLY snap2's manifest and its blob into store2 (WITHOUT snap1!).
        let m2 = cas1.load_and_verify_manifest(&snap2.snapshot_key).unwrap();
        let m2_src = cas1.snapshot_path(&snap2.snapshot_key).unwrap();
        let m2_dst = cas2.snapshot_path(&snap2.snapshot_key).unwrap();
        fs::create_dir_all(m2_dst.parent().unwrap()).unwrap();
        fs::copy(&m2_src, &m2_dst).unwrap();
        for entry in &m2.entries {
            if let Some(bk) = &entry.blob_key {
                let b_src = cas1.blob_path(bk).unwrap();
                let b_dst = cas2.blob_path(bk).unwrap();
                fs::create_dir_all(b_dst.parent().unwrap()).unwrap();
                fs::copy(&b_src, &b_dst).unwrap();
            }
        }

        // Single-snapshot verify & restore in store2 succeed without snap1 present!
        cas2.verify_snapshot(&snap2.snapshot_key)
            .expect("independent delta snapshot verifies without parent");
        let restore_dir = TempDir::new("sock-restore");
        let restore_out = restore_dir.path().join("tree");
        cas2.restore_snapshot(&snap2.snapshot_key, &restore_out, None)
            .expect("independent delta snapshot restores");
        assert_eq!(
            fs::read(restore_out.join("after.txt")).unwrap(),
            b"socket test payload"
        );
        assert!(!restore_out.join("ctrl.sock").exists());

        // Rollback guard cleans up partial files & 0o555 dirs for both created_root=true and false.
        let rb_scratch = TempDir::new("rb-test");
        let created_dir = rb_scratch.path().join("created");
        fs::create_dir_all(created_dir.join("ro")).unwrap();
        fs::write(created_dir.join("ro/part.txt"), b"partial").unwrap();
        set_file_mode(&created_dir.join("ro"), 0o555).unwrap();
        {
            let _guard = RestoreRollbackGuard::new(created_dir.clone(), true);
        }
        assert!(!created_dir.exists(), "created_root rollback removes dir");

        let pre_empty = rb_scratch.path().join("pre_empty");
        fs::create_dir_all(pre_empty.join("ro")).unwrap();
        fs::write(pre_empty.join("ro/part.txt"), b"partial").unwrap();
        set_file_mode(&pre_empty.join("ro"), 0o555).unwrap();
        {
            let _guard = RestoreRollbackGuard::new(pre_empty.clone(), false);
        }
        assert!(pre_empty.is_dir(), "pre-existing empty dir itself remains");
        assert!(
            fs::read_dir(&pre_empty).unwrap().next().is_none(),
            "pre-existing dir contents are cleaned on rollback"
        );
    }
}
