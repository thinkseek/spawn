//! Deterministic 40-hex Git OID projection over `spawn-cas` snapshots, trees,
//! and blobs (`D6 Fine Item 7.3`).
//!
//! `sha256:<64 hex>` remains the single canonical CAS storage address; 40-hex
//! Git OIDs exist strictly as deterministic boundary projections for compatibility
//! with `spawn_commit_map`, `Spawn-Head`, and external Git remotes.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;
use std::io::{self, Write};
use std::str::FromStr;

use sha1::{Digest as _, Sha1};

use crate::merkle::{Snapshot, TreeEntry, TreeEntryKind};
use crate::{CasError, Digest, Store};

/// Immutable format constant threshold above which Git blob projection uses a
/// deterministic pointer blob instead of hashing multi-hundred-MB file contents.
pub const GIT_POINTER_THRESHOLD: u64 = 100 * 1024 * 1024;

/// A 20-byte (40 lowercase hex) SHA-1 Git object identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GitOid([u8; 20]);

impl GitOid {
    /// Construct from raw 20 bytes.
    pub const fn from_bytes(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    /// Raw 20-byte OID.
    pub const fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    /// Parse exact 40 lowercase hex digits.
    pub fn parse_hex(text: &str) -> Result<Self, CasError> {
        let bytes = text.as_bytes();
        if bytes.len() != 40 {
            return Err(CasError::BadGitOid("want 40 lowercase hex digits"));
        }
        let mut out = [0_u8; 20];
        for (idx, pair) in bytes.chunks_exact(2).enumerate() {
            let high = crate::digest::decode_hex_nibble(pair[0])
                .ok_or(CasError::BadGitOid("invalid hex digit"))?;
            let low = crate::digest::decode_hex_nibble(pair[1])
                .ok_or(CasError::BadGitOid("invalid hex digit"))?;
            out[idx] = (high << 4) | low;
        }
        Ok(Self(out))
    }

    /// Lowercase 40-hex formatting.
    pub fn to_hex(&self) -> String {
        crate::digest::to_hex(&self.0)
    }
}

impl fmt::Display for GitOid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

impl FromStr for GitOid {
    type Err = CasError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse_hex(text)
    }
}

impl serde::Serialize for GitOid {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for GitOid {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse_hex(&text).map_err(serde::de::Error::custom)
    }
}

/// Deterministic boundary pointer content for files strictly larger than
/// [`GIT_POINTER_THRESHOLD`] (`100 MiB`).
pub fn git_pointer_bytes(digest: &Digest, size: u64) -> Vec<u8> {
    format!("spawn-pointer-v1\n{digest}\nsize {size}\n").into_bytes()
}

/// Git blob OID of an exact byte slice (`sha1("blob <len>\0" || bytes)`).
pub fn git_blob_oid_of_bytes(bytes: &[u8]) -> GitOid {
    let mut hasher = Sha1::new();
    let header = format!("blob {}\0", bytes.len());
    hasher.update(header.as_bytes());
    hasher.update(bytes);
    GitOid(hasher.finalize().into())
}

struct Sha1Writer {
    hasher: Sha1,
    written: u64,
}

impl Write for Sha1Writer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.hasher.update(buf);
        self.written = self
            .written
            .checked_add(u64::try_from(buf.len()).map_err(|_| io::Error::other("overflow"))?)
            .ok_or_else(|| io::Error::other("byte count overflow"))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Compute the deterministic Git blob OID for a CAS blob (`digest`, `size`).
pub fn project_blob_oid(store: &Store, digest: &Digest, size: u64) -> Result<GitOid, CasError> {
    if size > GIT_POINTER_THRESHOLD {
        return Ok(git_blob_oid_of_bytes(&git_pointer_bytes(digest, size)));
    }
    let mut sink = Sha1Writer {
        hasher: Sha1::new(),
        written: 0,
    };
    let header = format!("blob {size}\0");
    sink.hasher.update(header.as_bytes());
    let streamed = store.stream_blob(digest, &mut sink)?;
    if streamed != size || sink.written != size {
        return Err(CasError::CorruptIndex {
            want: digest.clone(),
            reason: "blob byte size mismatches tree entry size",
        });
    }
    Ok(GitOid(sink.hasher.finalize().into()))
}

fn git_tree_entry_cmp(left: &TreeEntry, right: &TreeEntry) -> Ordering {
    let left_tail = if left.kind == TreeEntryKind::Dir {
        Some(b'/')
    } else {
        None
    };
    let right_tail = if right.kind == TreeEntryKind::Dir {
        Some(b'/')
    } else {
        None
    };
    left.name
        .iter()
        .copied()
        .chain(left_tail)
        .cmp(right.name.iter().copied().chain(right_tail))
}

const fn git_mode_str(kind: TreeEntryKind) -> &'static [u8] {
    match kind {
        TreeEntryKind::File => b"100644",
        TreeEntryKind::Executable => b"100755",
        TreeEntryKind::Symlink => b"120000",
        TreeEntryKind::Dir => b"40000",
        TreeEntryKind::Gitlink => b"160000",
    }
}

/// Domain-separated memoization cache for Git OID projection.
///
/// Trees (`sha1("tree ...")`) and blobs (`sha1("blob ...")`) occupy distinct
/// maps so a regular file whose raw bytes happen to equal a `spawn-tree-v1`
/// object can never collide with that directory's projected Git tree OID.
#[derive(Debug, Default, Clone)]
pub struct GitOidCache {
    /// Cached `spawn-tree-v1` digest -> Git tree OID.
    pub trees: HashMap<Digest, GitOid>,
    /// Cached CAS blob digest (`<= GIT_POINTER_THRESHOLD`) -> Git blob OID.
    pub blobs: HashMap<Digest, GitOid>,
}

impl GitOidCache {
    /// Create an empty domain-separated Git OID cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Project a canonical `spawn-tree-v1` DAG to its exact Git tree 40-hex OID,
/// memoizing subtree and leaf blob OIDs in `cache`.
pub fn project_tree_oid(
    store: &Store,
    tree_digest: &Digest,
    cache: &mut GitOidCache,
) -> Result<GitOid, CasError> {
    if let Some(&cached) = cache.trees.get(tree_digest) {
        return Ok(cached);
    }
    let tree = store.read_tree(tree_digest)?;
    let mut entries: Vec<&TreeEntry> = tree.entries.iter().collect();
    entries.sort_by(|left, right| git_tree_entry_cmp(left, right));

    let mut body = Vec::new();
    for entry in entries {
        let oid = match entry.kind {
            TreeEntryKind::Dir => {
                let child = Digest::from_raw_sha256(entry.id);
                project_tree_oid(store, &child, cache)?
            }
            TreeEntryKind::File | TreeEntryKind::Executable | TreeEntryKind::Symlink => {
                let blob = Digest::from_raw_sha256(entry.id);
                if entry.size <= GIT_POINTER_THRESHOLD {
                    if let Some(&cached) = cache.blobs.get(&blob) {
                        cached
                    } else {
                        let computed = project_blob_oid(store, &blob, entry.size)?;
                        cache.blobs.insert(blob, computed);
                        computed
                    }
                } else {
                    project_blob_oid(store, &blob, entry.size)?
                }
            }
            TreeEntryKind::Gitlink => {
                if entry.size != 20 {
                    return Err(CasError::BadGitOid(
                        "cannot project 32-byte gitlink into 20-byte SHA-1 Git tree",
                    ));
                }
                let raw: [u8; 20] = entry.id[..20]
                    .try_into()
                    .map_err(|_| CasError::BadGitOid("invalid 20-byte gitlink"))?;
                GitOid::from_bytes(raw)
            }
        };
        body.extend_from_slice(git_mode_str(entry.kind));
        body.push(b' ');
        body.extend_from_slice(&entry.name);
        body.push(0);
        body.extend_from_slice(oid.as_bytes());
    }

    let mut hasher = Sha1::new();
    let header = format!("tree {}\0", body.len());
    hasher.update(header.as_bytes());
    hasher.update(&body);
    let oid = GitOid(hasher.finalize().into());
    cache.trees.insert(tree_digest.clone(), oid);
    Ok(oid)
}

/// Project a canonical `spawn-snapshot-v1` commit envelope into its exact
/// deterministic 40-hex Git commit OID, given its projected `parent_oids` in
/// first-parent order.
pub fn project_snapshot_oid(
    store: &Store,
    snapshot: &Snapshot,
    parent_oids: &[GitOid],
    cache: &mut GitOidCache,
) -> Result<GitOid, CasError> {
    if parent_oids.len() != snapshot.parents.len() {
        return Err(CasError::BadFormat(
            "parent_oids count must match snapshot parents count",
        ));
    }
    let tree_oid = project_tree_oid(store, &snapshot.tree, cache)?;
    let mut payload = Vec::with_capacity(64 + parent_oids.len() * 48 + snapshot.commit_body.len());
    payload.extend_from_slice(format!("tree {tree_oid}\n").as_bytes());
    for parent in parent_oids {
        payload.extend_from_slice(format!("parent {parent}\n").as_bytes());
    }
    payload.extend_from_slice(&snapshot.commit_body);

    let mut hasher = Sha1::new();
    hasher.update(format!("commit {}\0", payload.len()).as_bytes());
    hasher.update(&payload);
    Ok(GitOid(hasher.finalize().into()))
}
