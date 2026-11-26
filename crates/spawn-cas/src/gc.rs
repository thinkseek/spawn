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
/// Persistent Merkle tree & snapshot metadata objects (D6, never LRU-evicted).
pub const NS_TREE: &str = "tree";
/// Persistent file & chunk blobs referenced by trees (D6, never LRU-evicted).
pub const NS_BLOB: &str = "blob";

fn is_persistent_namespace(name: &str) -> bool {
    name == NS_TREE || name == NS_BLOB
}

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
pub const TMP_MAX_AGE: Duration = Duration::from_hours(7 * 24);

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
            if let Some(&quota) = quotas.get(&name)
                && !is_persistent_namespace(&name)
            {
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    /// A fresh directory under the temporary directory, removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("spawn-cas-{tag}-{}", crate::store::unique_suffix()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("scratch");
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// NB5 (Go `cas/gcrace_test.go`): a pin written while a GC pass runs
    /// (between its read of the pins and its evictions) never returns with
    /// its object present and then loses it to that pass. Pin waits for the
    /// pass: when it returns, the object is either kept or already gone
    /// (and the caller fetches it), and every later pass keeps it.
    #[test]
    fn pin_during_gc_never_loses_a_present_object() {
        let scratch = Scratch::new("gcrace");
        let mut store = Store::open(scratch.0.join("cas")).expect("open");
        let content = b"a baseline bundle";
        let digest = store.put_bytes(NS_ARTIFACTS, content).expect("put");
        let (read_tx, read_rx) = mpsc::channel::<()>();
        let (resume_tx, resume_rx) = mpsc::channel::<()>();
        let resume_rx = Mutex::new(resume_rx);
        let fired = AtomicBool::new(false);
        store.after_pinned = Some(Box::new(move || {
            if !fired.swap(true, Ordering::SeqCst) {
                read_tx.send(()).expect("signal");
                resume_rx.lock().expect("lock").recv().expect("resume");
            }
        }));
        let quotas = Quotas::from([(NS_ARTIFACTS.to_string(), 0)]);
        let store = &store;
        let (result_tx, result_rx) = mpsc::channel();
        std::thread::scope(|scope| {
            let pass = scope.spawn(|| store.gc(&quotas).expect("gc"));
            read_rx.recv().expect("the pass read the pins");
            // A grant accepted right now pins its inputs.
            let digest = digest.clone();
            let result_tx = result_tx.clone();
            scope.spawn(move || {
                let pinned = store.pin("lease", "gate-race.1", std::slice::from_ref(&digest));
                result_tx.send((
                    pinned.map_err(|error| error.to_string()),
                    store.has(&digest),
                ))
            });
            assert!(
                result_rx.recv_timeout(Duration::from_millis(300)).is_err(),
                "Pin returned while a GC pass was between reading the pins and evicting"
            );
            resume_tx.send(()).expect("resume");
            pass.join().expect("gc thread");
        });
        let (pinned, present) = result_rx.recv().expect("pin result");
        pinned.expect("pin");
        assert!(
            !present || store.has(&digest),
            "present when Pin returned, then evicted by the pass"
        );
        // The job finds it missing and fetches it again; from now on it stays.
        if !store.has(&digest) {
            store.put_bytes(NS_ARTIFACTS, content).expect("put again");
        }
        store.gc(&quotas).expect("gc");
        assert!(store.has(&digest), "a pinned object was evicted");
    }
}
