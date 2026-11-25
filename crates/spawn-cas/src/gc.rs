//! Crash-only LRU garbage collection by namespace (design §10.3).

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::store::remove_missing_ok;
use crate::{Algo, CasError, Digest, Store, is_hex64};

/// Bundles, controller packs, registry packs (`artifact_urls`).
pub const NS_ARTIFACTS: &str = "artifacts";
/// Files of a Layer 0 `base/<commit>/target`, `node_modules`.
pub const NS_BASE_CACHE: &str = "base-cache";
/// design-143 backup chunks (the local store is a cache of the target).
pub const NS_BACKUP: &str = "backup";
/// Data snapshots.
pub const NS_DATA: &str = "data";

/// Byte quotas by namespace; a namespace without one is never evicted.
pub type Quotas = BTreeMap<String, u64>;

/// `artifacts` 20 GiB and `base-cache` 50 GiB; `backup` and `data` have no
/// default quota.
pub fn default_quotas() -> Quotas {
    Quotas::from([
        (NS_ARTIFACTS.to_string(), 20 << 30),
        (NS_BASE_CACHE.to_string(), 50 << 30),
    ])
}

/// How long an abandoned file in `tmp/` (an interrupted write, a partial
/// download nobody resumed) is kept.
pub const TMP_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// What one [`Store::gc`] pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcReport {
    /// Objects evicted, oldest use first.
    pub evicted: Vec<Digest>,
    /// Their bytes.
    pub freed_bytes: u64,
    /// Times a pinned object was passed over while its namespace was over
    /// quota.
    pub kept_pinned: usize,
    /// Stale entries removed from `tmp/` (and stale pin temporaries).
    pub stale_tmp: usize,
    /// Bytes recorded in each namespace after the pass.
    pub namespace_use: BTreeMap<String, u64>,
    /// What could not be removed, one line each; the pass went on.
    pub errors: Vec<String>,
}

struct AccessRecord {
    digest: Digest,
    path: PathBuf,
    used: SystemTime,
    size: u64,
}

impl Store {
    /// One pass: within each namespace over its quota, the least recently
    /// used unpinned objects are deleted, object first and access record
    /// second, until the namespace fits. A pinned object is never deleted,
    /// including one pinned while the pass runs ([`Store::pin`] waits for
    /// it). Access records whose object is gone are dropped. Entries of
    /// `tmp/` older than [`TMP_MAX_AGE`] go too.
    pub fn gc(&self, quotas: &Quotas) -> Result<GcReport, CasError> {
        let mut report = GcReport::default();
        self.evict(quotas, &mut report)?;
        let now = self.now();
        let stale = |meta: &fs::Metadata| {
            meta.modified()
                .is_ok_and(|at| now.duration_since(at).is_ok_and(|age| age > TMP_MAX_AGE))
        };
        for entry in read_dir_or_empty(&self.tmp_dir())? {
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !stale(&meta) {
                continue;
            }
            let removed = if meta.is_dir() {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
            match removed {
                Ok(()) => report.stale_tmp += 1,
                Err(error) => report
                    .errors
                    .push(format!("remove {}: {error}", path.display())),
            }
        }
        // A pin temporary a crash left behind (`pins/<owner>/.pin-*`).
        for owner in read_dir_or_empty(&self.root().join("pins"))? {
            if !owner.file_type()?.is_dir() {
                continue;
            }
            for entry in read_dir_or_empty(&owner.path())? {
                let path = entry.path();
                let temporary = entry.file_name().to_string_lossy().starts_with(".pin-");
                if temporary
                    && fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file() && stale(&meta))
                {
                    match fs::remove_file(&path) {
                        Ok(()) => report.stale_tmp += 1,
                        Err(error) => report
                            .errors
                            .push(format!("remove {}: {error}", path.display())),
                    }
                }
            }
        }
        Ok(report)
    }

    /// The LRU part of [`Store::gc`], under the pin/GC lock from the read of
    /// the pins to the last removal.
    fn evict(&self, quotas: &Quotas, report: &mut GcReport) -> Result<(), CasError> {
        let _lock = self.lock_gc()?;
        let pinned = self.pinned()?;
        #[cfg(test)]
        if let Some(hook) = &self.after_pinned {
            hook();
        }
        for namespace in read_dir_or_empty(&self.root().join("access"))? {
            if !namespace.file_type()?.is_dir() {
                continue;
            }
            let name = namespace.file_name().to_string_lossy().into_owned();
            let mut records = self.records(&namespace.path())?;
            let mut total: u64 = records.iter().map(|record| record.size).sum();
            if let Some(&quota) = quotas.get(&name) {
                records.sort_by(|left, right| {
                    (left.used, &left.digest).cmp(&(right.used, &right.digest))
                });
                for record in records {
                    if total <= quota {
                        break;
                    }
                    if pinned.contains(&record.digest) {
                        report.kept_pinned += 1;
                        continue;
                    }
                    let object = self.object_path(&record.digest);
                    if let Err(error) = remove_missing_ok(&object) {
                        report
                            .errors
                            .push(format!("remove {}: {error}", object.display()));
                        continue;
                    }
                    if let Err(error) = remove_missing_ok(&record.path) {
                        report
                            .errors
                            .push(format!("remove {}: {error}", record.path.display()));
                    }
                    total -= record.size;
                    report.freed_bytes += record.size;
                    report.evicted.push(record.digest);
                }
            }
            report.namespace_use.insert(name, total);
        }
        Ok(())
    }

    /// The access records of one namespace directory whose object exists;
    /// a record whose object is gone is removed (it names nothing).
    fn records(&self, namespace: &Path) -> Result<Vec<AccessRecord>, CasError> {
        let mut records = Vec::new();
        for algo_dir in read_dir_or_empty(namespace)? {
            let Some(algo) = Algo::from_name(&algo_dir.file_name().to_string_lossy()) else {
                continue;
            };
            if !algo_dir.file_type()?.is_dir() {
                continue;
            }
            for record in read_dir_or_empty(&algo_dir.path())? {
                let hex = record.file_name().to_string_lossy().into_owned();
                if !is_hex64(&hex) {
                    continue;
                }
                let path = record.path();
                let Ok(meta) = fs::symlink_metadata(&path) else {
                    continue;
                };
                if !meta.is_file() {
                    continue;
                }
                let digest = Digest::from_parts(algo, &hex)?;
                match fs::symlink_metadata(self.object_path(&digest)) {
                    Ok(object) if object.is_file() => records.push(AccessRecord {
                        digest,
                        path,
                        used: meta.modified()?,
                        size: object.len(),
                    }),
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        remove_missing_ok(&path)?;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Ok(records)
    }
}

fn read_dir_or_empty(directory: &Path) -> io::Result<Vec<fs::DirEntry>> {
    match fs::read_dir(directory) {
        Ok(entries) => entries.collect(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}
