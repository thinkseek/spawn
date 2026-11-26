//! A result's files (section 3.5): the log streams from the socket to
//! `SPAWN_NODE_RESULTS/<job>.<epoch>/log.part`, hashed and counted as it
//! goes (never held in memory), and is checked against `log_bytes` and
//! `log_sha256`; then it is synced and renamed to `log`, and the metadata
//! line is written to `meta.json` (synced, renamed) — all before the
//! ledger is told. When the ledger says the lease moved, the files this
//! request wrote are removed again.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sha2::{Digest as _, Sha256};
use spawn_node_proto::Limit;
use spawn_node_proto::codec::hex;

/// The log's file once whole.
pub const LOG: &str = "log";
/// The log while it streams.
pub const LOG_PART: &str = "log.part";
/// The metadata line, as received.
pub const META: &str = "meta.json";
const META_PART: &str = "meta.json.part";

/// What storing did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stored {
    /// This request wrote `log` and `meta.json`.
    Written,
    /// The very same result was stored before; nothing was rewritten.
    Same,
}

/// Why a result was not stored.
#[derive(Debug)]
pub enum Refused {
    /// The log is not `log_bytes` long or does not hash to `log_sha256`.
    Mismatch,
    /// More than 32 MiB of log.
    TooLarge,
    /// Another result for this job and epoch is stored already.
    Conflict,
    Io(io::Error),
}

impl From<io::Error> for Refused {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// The directory of a job's result at an epoch. `job_id` is a checked path
/// segment (`is_job_id`: no `/`, not `.` or `..`).
pub fn result_dir(root: &Path, job_id: &str, lease_epoch: u64) -> PathBuf {
    root.join(format!("{job_id}.{lease_epoch}"))
}

/// Streams `log_bytes` bytes of `body` into `dir` and stores the result
/// (see the module). `meta_line` is the metadata line without its newline.
pub fn store(
    dir: &Path,
    meta_line: &[u8],
    log_bytes: u64,
    log_sha256: &str,
    body: &mut impl Read,
) -> Result<Stored, Refused> {
    std::fs::create_dir_all(dir)?;
    let part = dir.join(LOG_PART);
    let streamed = stream(&part, log_bytes, log_sha256, body);
    if let Err(error) = streamed {
        let _ = std::fs::remove_file(&part);
        return Err(error);
    }
    match std::fs::read(dir.join(META)) {
        Ok(stored) => {
            std::fs::remove_file(&part)?;
            return if stored == meta_line {
                Ok(Stored::Same)
            } else {
                Err(Refused::Conflict)
            };
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    std::fs::rename(&part, dir.join(LOG))?;
    let meta_part = dir.join(META_PART);
    let mut file = File::create(&meta_part)?;
    file.write_all(meta_line)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&meta_part, dir.join(META))?;
    File::open(dir)?.sync_all()?;
    Ok(Stored::Written)
}

/// The log into `part`: at most 32 MiB + 1 bytes read, hashed as written.
fn stream(
    part: &Path,
    log_bytes: u64,
    log_sha256: &str,
    body: &mut impl Read,
) -> Result<(), Refused> {
    let cap = Limit::ResultLog.bytes() as u64;
    if log_bytes > cap {
        return Err(Refused::TooLarge);
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(part)?;
    let mut hasher = Sha256::new();
    let mut limited = body.take(log_bytes.min(cap + 1));
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let read = match limited.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        };
        total += read as u64;
        if total > cap {
            return Err(Refused::TooLarge);
        }
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read])?;
    }
    if total != log_bytes || hex(&hasher.finalize()) != log_sha256 {
        return Err(Refused::Mismatch);
    }
    file.sync_all()?;
    Ok(())
}

/// Removes what [`store`] wrote (the ledger refused the result).
pub fn remove(dir: &Path) {
    for name in [LOG_PART, LOG, META_PART, META] {
        let _ = std::fs::remove_file(dir.join(name));
    }
    let _ = std::fs::remove_dir(dir);
}

/// The `<job>.<epoch>` results being stored right now: one upload at a
/// time per result.
#[derive(Debug, Default)]
pub struct InFlight {
    names: Mutex<HashSet<String>>,
}

/// Holds a result's name until dropped.
#[derive(Debug)]
pub struct Claim<'a> {
    owner: &'a InFlight,
    name: String,
}

impl InFlight {
    /// The claim on `name`; `None` while another request holds it.
    pub fn claim(&self, name: &str) -> Option<Claim<'_>> {
        let mut names = self
            .names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        names.insert(name.to_string()).then(|| Claim {
            owner: self,
            name: name.to_string(),
        })
    }
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        self.owner
            .names
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spawn_node_proto::codec::sha256_hex;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "spawn-node-gateway-results-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_log_is_checked_stored_once_and_a_conflict_is_refused() {
        let root = scratch("store");
        let dir = result_dir(&root, "gate-1", 3);
        let log = b"hello log";
        let sha = sha256_hex(log);
        let stored = store(&dir, b"{\"m\":1}", 9, &sha, &mut &log[..]).unwrap();
        assert_eq!(stored, Stored::Written);
        assert_eq!(std::fs::read(dir.join(LOG)).unwrap(), log);
        assert_eq!(std::fs::read(dir.join(META)).unwrap(), b"{\"m\":1}");
        assert!(!dir.join(LOG_PART).exists());

        let same = store(&dir, b"{\"m\":1}", 9, &sha, &mut &log[..]).unwrap();
        assert_eq!(same, Stored::Same);
        assert!(matches!(
            store(&dir, b"{\"m\":2}", 9, &sha, &mut &log[..]),
            Err(Refused::Conflict)
        ));
        assert_eq!(std::fs::read(dir.join(META)).unwrap(), b"{\"m\":1}");

        let other = result_dir(&root, "gate-2", 1);
        assert!(matches!(
            store(&other, b"{}", 9, &"0".repeat(64), &mut &log[..]),
            Err(Refused::Mismatch)
        ));
        assert!(matches!(
            store(&other, b"{}", 10, &sha, &mut &log[..]),
            Err(Refused::Mismatch)
        ));
        assert!(!other.join(LOG_PART).exists() && !other.join(LOG).exists());
        remove(&dir);
        assert!(!dir.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn one_upload_per_result_at_a_time() {
        let in_flight = InFlight::default();
        let claim = in_flight.claim("a.1").unwrap();
        assert!(in_flight.claim("a.1").is_none());
        assert!(in_flight.claim("a.2").is_some());
        drop(claim);
        assert!(in_flight.claim("a.1").is_some());
    }
}
