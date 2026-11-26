//! `spawn-cas`: the unified content-addressed store (design §10), the same
//! layout as `spawn-node`'s Go `cas` package, byte for byte:
//!
//! ```text
//! <cas>/.spawn-cas                      marker (CAS_MARKER)
//! <cas>/objects/<algo>/<2 hex>/<hex>    one immutable object, mode 0444
//! <cas>/pins/<owner>/<name>             a text file of digests something holds
//! <cas>/access/<ns>/<algo>/<hex>        LRU record: its mtime is the last use
//! <cas>/tmp/                            writes land here, then rename
//! ```
//!
//! # Class B: crash-only
//!
//! Nothing is fsynced. Any of the store may be removed at any instant,
//! mid-write included, and the next [`Store::open`] reads what is left
//! correctly with no repair step: an object is renamed into place whole
//! (after it was verified), a pin file is replaced whole (temporary file
//! and `rename`), GC deletes an object before its access record (a record
//! that names nothing is dropped by the next pass), and abandoned
//! temporaries are purged after [`TMP_MAX_AGE`].
//!
//! # Digests
//!
//! Every address carries its algorithm ([`Digest`]): `sha256:<64 hex>` or
//! `blake3:<64 hex>`, lowercase; anything else is refused everywhere.
//! BLAKE3 parses, but cannot be computed until its own PR (ST2): hashing or
//! verifying it is [`CasError::AlgoUnavailable`], and unverifiable bytes
//! are never stored.
//!
//! # Safety rules
//!
//! - Bytes are verified on the way in ([`Store::commit`], [`Store::put`],
//!   [`Store::publish`]) and on [`Store::read`]; bytes that do not hash to
//!   their address are refused and removed.
//! - Objects are opened `O_NOFOLLOW | O_NONBLOCK`: a symlink or a special
//!   file planted as an object is refused, never followed.
//! - A non-empty directory without [`CAS_MARKER`] is never taken for a
//!   store ([`CasError::NotCas`]).
//! - [`Store::pin`] and [`Store::gc`] exclude each other (an `flock` on
//!   `pins/.lock`, so across threads and processes): a pin either is read
//!   by a pass or is written after it, so a pin that returned with its
//!   object present never loses it to that pass, or to any later one.
//!
//! # Data snapshots
//!
//! [`Manifest`] is manifest-v1, a data snapshot's manifest: the
//! `tree_sha256_v1` line list of a directory; the snapshot's identity is
//! `sha256:<tree_sha256_v1>`. [`Store::put_snapshot`] stores one in the
//! `data` namespace and [`Store::keep_snapshots`] keeps the
//! [`KEEP_SNAPSHOTS`] newest of a data ref pinned (`pins/prov/<ref>`),
//! crash-only like the rest (`spawn prov snapshot`, docs/provenance.md).
//!
//! The store's previous incarnation (`spawn-host`'s `cas_dedup`, objects
//! at `<cas>/<2 hex>/<64 hex>`) is adopted in place by
//! [`Store::adopt_legacy_shards`]: a rename, nothing rehashed.

mod digest;
mod gc;
mod legacy;
mod manifest;
mod store;

use std::io;
use std::path::PathBuf;

pub use digest::{
    Algo, Digest, Hasher, Verified, is_hex64, sha256_of, sha256_reader, verify_reader,
};
pub use gc::{
    GcReport, NS_ARTIFACTS, NS_BACKUP, NS_BASE_CACHE, NS_DATA, Quotas, TMP_MAX_AGE, default_quotas,
};
pub use legacy::AdoptReport;
pub use manifest::{
    KEEP_SNAPSHOTS, KeepReport, MANIFEST_V1, Manifest, ManifestEntry, PROV_PIN_OWNER,
};
pub use store::{
    CAS_MARKER, Clock, GcLock, ObjectEntry, Publish, Store, is_valid_name, system_clock,
};

/// What a store operation refused, or the I/O error that stopped it.
#[derive(Debug, thiserror::Error)]
pub enum CasError {
    /// A digest that is not `sha256:<64 hex>` or `blake3:<64 hex>`.
    #[error("digest {input:?}: {reason}")]
    BadDigest {
        /// The text given.
        input: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// The algorithm parses but cannot be computed in this build.
    #[error("{0} verification is not available in this build (ST2)")]
    AlgoUnavailable(Algo),
    /// The content does not hash to its address.
    #[error("content hashes to {got}, not {want}")]
    Mismatch {
        /// The address.
        want: Digest,
        /// What the content hashes to (algorithm-prefixed).
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
            CasError::NotCas(_) | CasError::BadDigest { .. } | CasError::BadName { .. } => {
                io::Error::new(io::ErrorKind::InvalidInput, error)
            }
            CasError::Mismatch { .. } | CasError::BadManifest { .. } => {
                io::Error::new(io::ErrorKind::InvalidData, error)
            }
            CasError::AlgoUnavailable(_) => io::Error::new(io::ErrorKind::Unsupported, error),
            CasError::NotPlain(_) | CasError::SymlinkEscape { .. } => io::Error::other(error),
        }
    }
}
