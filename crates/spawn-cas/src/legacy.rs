//! The one-time adoption of the store's previous layout: `spawn-host`'s
//! `cas_dedup` kept SHA-256 blobs unprefixed at `<cas>/<2 hex>/<64 hex>`.
//! They become `objects/sha256/<2 hex>/<64 hex>` by a rename, never
//! rehashed (design §10.1, F14).

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use crate::store::{is_shard, open_nofollow, plain_dir, remove_missing_ok};
use crate::{Algo, CasError, Digest, Store, is_hex64};

/// What [`Store::adopt_legacy_shards`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdoptReport {
    /// Blobs moved to `objects/sha256/`.
    pub adopted: usize,
    /// Legacy names removed because an object of that digest already
    /// existed (a crash leftover of an earlier adoption, or a duplicate).
    pub duplicates: Vec<PathBuf>,
    /// Legacy shard directories removed once empty.
    pub shards_removed: usize,
}

impl Store {
    /// Move every legacy blob, `<root>/<2 hex>/<64 hex>` (a regular file
    /// named by its lowercase digest, in its own shard), to
    /// `objects/sha256/<2 hex>/<64 hex>`, mode 0444. Nothing is read or
    /// rehashed: the name is the address the old store gave it, by the
    /// same algorithm. Anything else is left exactly as it is: other names
    /// (a `*.jsonl` file among them), entries in the wrong shard,
    /// symlinks, directories that are not shards. A legacy name whose
    /// object already exists is removed (the object stays), and a legacy
    /// shard left empty is removed.
    ///
    /// Idempotent and safe from several processes at once: each blob is
    /// linked at its new name (never replacing an object) before its old
    /// name is removed, so it is never absent from both.
    pub fn adopt_legacy_shards(&self) -> Result<AdoptReport, CasError> {
        let mut report = AdoptReport::default();
        let mut shards: Vec<(String, PathBuf)> = Vec::new();
        for entry in fs::read_dir(self.root())? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_shard(&name) && entry.file_type()?.is_dir() {
                shards.push((name, entry.path()));
            }
        }
        shards.sort();
        for (shard, directory) in shards {
            let mut blobs = Vec::new();
            for entry in fs::read_dir(&directory)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if is_hex64(&name) && name.starts_with(&shard) && entry.file_type()?.is_file() {
                    blobs.push(name);
                }
            }
            blobs.sort();
            for hex in blobs {
                let digest = Digest::from_parts(Algo::Sha256, &hex)?;
                let legacy = directory.join(&hex);
                let target = self.object_path(&digest);
                let algo = self.root().join("objects").join(Algo::Sha256.name());
                plain_dir(&algo)?;
                plain_dir(&algo.join(&shard))?;
                match fs::hard_link(&legacy, &target) {
                    Ok(()) => {
                        open_nofollow(&target)?
                            .set_permissions(fs::Permissions::from_mode(0o444))?;
                        remove_missing_ok(&legacy)?;
                        report.adopted += 1;
                    }
                    // The object is there already: a crash between the link
                    // and the unlink left the same inode at both names, or
                    // the old store and a new writer both hold the address.
                    // By the no-rehash rule the name is the content, so the
                    // legacy name is redundant either way and goes.
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        if remove_missing_ok(&legacy)? {
                            report.duplicates.push(legacy);
                        }
                    }
                    // Adopted by another opener meanwhile.
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            if fs::remove_dir(&directory).is_ok() {
                report.shards_removed += 1;
            }
        }
        Ok(report)
    }
}
