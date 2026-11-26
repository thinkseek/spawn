//! `spawn-cas`: the unified content-addressed store (design §10, D6 Phase 0),
//! on-disk compatible with `spawn-node`'s Go `cas` package:
//!
//! ```text
//! <cas>/.spawn-cas                      marker (CAS_MARKER)
//! <cas>/objects/<algo>/<2 hex>/<hex>    one immutable object, mode 0444
//! <cas>/chunk-index/<algo>/<2 hex>/<hex>  spawn-chunks-v1 index for > 4 MiB blobs
//! <cas>/pins/<owner>/<name>             a text file of digests something holds
//! <cas>/access/<ns>/<algo>/<hex>        LRU record: its mtime is the last use
//! <cas>/tmp/                            writes land here, then rename
//! ```
//!
//! # Class B & Persistent D6 Namespaces
//!
//! Ephemeral namespaces (`artifacts`, `base-cache`, `backup`, `data`) are
//! crash-only unless pinned; D6 persistent namespaces (`tree`, `blob`) are never
//! LRU-evicted and support filesystem-level [`Store::sync_store`] before
//! publishing PostgreSQL snapshot roots.
//!
//! # Digests
//!
//! Every content address uses `sha256:<64 hex>`, lowercase; unprefixed digests
//! and legacy `blake3:` addresses are refused everywhere (`D6 Fine Item 7.1`).
//!
//! # Safety rules
//!
//! - Bytes are verified on the way in ([`Store::commit`], [`Store::put`],
//!   [`Store::publish`], [`Store::put_blob`]) and on [`Store::read`] /
//!   [`Store::stream_blob`]; bytes that do not hash to their address are
//!   refused and removed.
//! - Objects and chunk indices are opened `O_NOFOLLOW | O_NONBLOCK`: symlinks
//!   and special files are refused, never followed.
//! - A non-empty directory without [`CAS_MARKER`] is never taken for a
//!   store ([`CasError::NotCas`]).
//! - [`Store::pin`] and [`Store::gc`] exclude each other (an `flock` on
//!   `pins/.lock`, across threads and processes).
//!
//! # Data snapshots
//!
//! [`Manifest`] is manifest-v1, a data snapshot's manifest: the
//! `tree_sha256_v1` line list of a directory; the snapshot's identity is
//! `sha256:<tree_sha256_v1>`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod chunker;
mod digest;
mod gc;
mod gc_fence;
mod git_oid;
mod journal;
mod legacy;
mod manifest;
mod merkle;
mod store;
mod tree;

use std::io;
use std::path::PathBuf;

pub use chunker::{
    CHUNK_AVG, CHUNK_MAX, CHUNK_MIN, CHUNKER_FASTCDC_V1, ChunkIndex, ChunkRef, StoredBlob,
    fastcdc_gear_table, next_chunk_len,
};
pub use digest::{
    Algo, Digest, Hasher, Verified, is_hex64, sha256_of, sha256_reader, verify_reader,
};
pub use gc::{
    GcReport, NS_ARTIFACTS, NS_BACKUP, NS_BASE_CACHE, NS_BLOB, NS_DATA, NS_TREE, Quotas,
    TMP_MAX_AGE, default_quotas,
};
pub use gc_fence::{
    DEFAULT_PERSISTENT_GC_GRACE, GC_UNREACH_STATE_FILE, GcFenceBinding, PersistentGcReport,
    bind_gc_fence, verify_gc_fence,
};
pub use git_oid::{
    GIT_POINTER_THRESHOLD, GitOid, GitOidCache, git_blob_oid_of_bytes, git_pointer_bytes,
    project_blob_oid, project_snapshot_oid, project_tree_oid,
};
pub use journal::{JournalEntry, RefJournal};
pub use legacy::AdoptReport;
pub use manifest::{KEEP_SNAPSHOTS, KeepReport, Manifest, ManifestEntry, PROV_PIN_OWNER};
pub use merkle::{
    Snapshot, Tree, TreeEntry, TreeEntryKind, TreeMutation, empty_tree_digest, is_valid_entry_name,
};
pub use store::{
    CAS_MARKER, Clock, GcLock, GcSharedLock, ObjectEntry, Publish, Store, is_valid_name,
    system_clock,
};
pub use tree::{TREE_SKIP_REGISTRY, registry_sha256, scenarios_sha256, tree_sha256_v1};

/// What a store operation refused, or the I/O error that stopped it.
#[derive(Debug, thiserror::Error)]
pub enum CasError {
    /// A digest that is not `sha256:<64 hex>`.
    #[error("digest {input:?}: {reason}")]
    BadDigest {
        /// The text given.
        input: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// The content does not hash to its address.
    #[error("content hashes to {got}, not {want}")]
    Mismatch {
        /// The address.
        want: Digest,
        /// What the content hashes to (`sha256:<64 hex>`).
        got: String,
    },
    /// An owner, pin name or namespace outside
    /// `[A-Za-z0-9_][A-Za-z0-9._:-]{0,127}`.
    #[error("{what} {name:?}: not [A-Za-z0-9_][A-Za-z0-9._:-]{{0,127}}")]
    BadName {
        /// Which kind of name.
        what: &'static str,
        /// The name given.
        name: String,
    },
    /// A non-empty directory without the marker.
    #[error("{} is not a CAS root (no {CAS_MARKER}, and not empty)", .0.display())]
    NotCas(PathBuf),
    /// A symlink, or not the kind of file the layout puts there.
    #[error("{} is a symlink or not a plain file or directory: refused", .0.display())]
    NotPlain(PathBuf),
    /// A symbolic link under a snapshot's root that points outside it.
    #[error("{} points outside the snapshot's root ({}): refused", .link.display(), .target.display())]
    SymlinkEscape {
        /// The link, relative to the root.
        link: PathBuf,
        /// Where it points, as it says.
        target: PathBuf,
    },
    /// Bytes that are not a manifest-v1.
    #[error("not a manifest-v1, line {line}: {reason}")]
    BadManifest {
        /// The line (from 1).
        line: usize,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// Canonical `spawn-tree-v1`, `spawn-snapshot-v1`, or `spawn-chunks-v1` encoding violation.
    #[error("malformed CAS object format: {0}")]
    BadFormat(&'static str),
    /// Object exceeded the write-side 4 MiB single-object hard cap.
    #[error("{kind} object size {size} exceeds {max}-byte cap")]
    TooLarge {
        /// Object kind (`tree`, `snapshot`, `chunk index`).
        kind: &'static str,
        /// Actual encoded byte size.
        size: usize,
        /// Maximum allowed bytes.
        max: usize,
    },
    /// Invalid or non-SHA1 Git OID projection input.
    #[error("invalid Git OID: {0}")]
    BadGitOid(&'static str),
    /// Chunk index did not match the requested blob digest or byte total.
    #[error("corrupt chunk index for {want}: {reason}")]
    CorruptIndex {
        /// Target file digest.
        want: Digest,
        /// Reason for rejection.
        reason: &'static str,
    },
    /// `.gc-fence` is missing from the CAS root, refusing persistent GC.
    #[error("missing GC fence at {}: bind CAS root before running persistent GC", .0.display())]
    GcFenceMissing(PathBuf),
    /// `.gc-fence` binding does not match the live PostgreSQL cluster/db/schema.
    #[error("GC fence mismatch: CAS bound to {expected}, live DB is {actual}")]
    GcFenceMismatch {
        /// Expected binding from `.gc-fence`.
        expected: String,
        /// Live PostgreSQL binding.
        actual: String,
    },
    /// Live PostgreSQL `spawn_ref_log` trails `refs.journal`; reconcile before GC.
    #[error(
        "PG ref log ({pg_latest_ref_seq}) trails refs.journal ({journal_latest_seq}): reconcile before GC"
    )]
    GcFenceBehindJournal {
        /// Latest `seq` in PostgreSQL `spawn_ref_log`.
        pg_latest_ref_seq: u64,
        /// Latest `seq` in `refs.journal`.
        journal_latest_seq: u64,
    },
    /// The file system said no.
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl CasError {
    /// Whether this is a missing file or object.
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::Io(error) if error.kind() == io::ErrorKind::NotFound)
    }

    /// Whether this is a digest mismatch.
    pub fn is_mismatch(&self) -> bool {
        matches!(self, Self::Mismatch { .. })
    }
}

impl From<CasError> for io::Error {
    fn from(error: CasError) -> Self {
        match error {
            CasError::Io(error) => error,
            CasError::NotCas(_)
            | CasError::BadDigest { .. }
            | CasError::BadName { .. }
            | CasError::BadGitOid(_)
            | CasError::TooLarge { .. } => io::Error::new(io::ErrorKind::InvalidInput, error),
            CasError::Mismatch { .. }
            | CasError::BadManifest { .. }
            | CasError::BadFormat(_)
            | CasError::CorruptIndex { .. } => io::Error::new(io::ErrorKind::InvalidData, error),
            CasError::NotPlain(_)
            | CasError::SymlinkEscape { .. }
            | CasError::GcFenceMissing(_)
            | CasError::GcFenceMismatch { .. }
            | CasError::GcFenceBehindJournal { .. } => io::Error::other(error),
        }
    }
}
