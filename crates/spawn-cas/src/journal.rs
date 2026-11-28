//! Append-only durability journal (`<cas_root>/refs.journal`) for D6 ref
//! transitions (`#113` Fine Item 7.2).
//!
//! Each PostgreSQL ref commit (`spawn_ref_cas` / `spawn_ref_land_main`) locks
//! `refs.journal` exclusively, heals any unterminated trailing crash fragment
//! back to the last committed newline boundary, and appends one JSON line
//! opened with `O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC` followed by `sync_all()`
//! (`fsync`) so PG backup restore windows can fast-forward reconcile recent
//! ref moves.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::store::open_nofollow;
use crate::{CasError, Digest, GitOid, Store};

/// File name of the CAS root ref durability journal.
pub(crate) const REFS_JOURNAL: &str = "refs.journal";

/// One committed ref transition recorded in `refs.journal`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    /// Monotonic `spawn_ref_log.seq` assigned by PostgreSQL (`>= 1`).
    pub seq: u64,
    /// Project name.
    pub project: String,
    /// Reference name (e.g. `main`, `branch/foo`, `wip/foo`).
    pub name: String,
    /// Reference kind (`main`, `branch`, `wip`, `tag`).
    pub kind: String,
    /// Previous snapshot (`None` when the ref was newly created).
    pub old_snapshot: Option<Digest>,
    /// New snapshot (`sha256:<64 hex>`).
    pub new_snapshot: Digest,
    /// Deterministic 40-hex projected Git commit OID of `new_snapshot`.
    pub git_oid: GitOid,
    /// Transition timestamp in Unix epoch milliseconds.
    pub at_ms: i64,
}

fn validate_entry(entry: &JournalEntry) -> Result<(), CasError> {
    if entry.seq == 0 {
        return Err(CasError::BadFormat("journal entry seq must be >= 1"));
    }
    if entry.project.trim().is_empty() || entry.name.trim().is_empty() {
        return Err(CasError::BadFormat(
            "journal entry project and name must be non-empty",
        ));
    }
    if !matches!(entry.kind.as_str(), "main" | "branch" | "wip" | "tag") {
        return Err(CasError::BadFormat("invalid journal entry ref kind"));
    }
    Ok(())
}

const TAIL_SCAN_CHUNK: usize = 8192;
const TAIL_SCAN_CHUNK_U64: u64 = 8192;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TailState {
    /// File is empty or ends cleanly with `b'\n'`; ready to append at `eof`.
    Clean { eof: u64 },
    /// File ends with an unterminated trailing fragment starting at `valid_end`.
    TornTail { valid_end: u64 },
}

fn find_last_committed_end(file: &mut fs::File, file_len: u64) -> io::Result<u64> {
    let mut buf = [0_u8; TAIL_SCAN_CHUNK];
    let mut cursor = file_len;
    while cursor > 0 {
        let step_u64 = cursor.min(TAIL_SCAN_CHUNK_U64);
        let step = usize::try_from(step_u64).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "journal chunk size overflow")
        })?;
        let start = cursor - step_u64;
        file.seek(SeekFrom::Start(start))?;
        file.read_exact(&mut buf[..step])?;
        if let Some(rel) = buf[..step].iter().rposition(|&b| b == b'\n') {
            let rel_u64 = u64::try_from(rel).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "journal offset overflow")
            })?;
            return Ok(start + rel_u64 + 1);
        }
        cursor = start;
    }
    Ok(0)
}

fn inspect_tail(file: &mut fs::File, file_len: u64) -> io::Result<TailState> {
    if file_len == 0 {
        return Ok(TailState::Clean { eof: 0 });
    }
    let mut last = [0_u8; 1];
    file.seek(SeekFrom::Start(file_len - 1))?;
    file.read_exact(&mut last)?;
    if last[0] == b'\n' {
        return Ok(TailState::Clean { eof: file_len });
    }
    let valid_end = find_last_committed_end(file, file_len)?;
    Ok(TailState::TornTail { valid_end })
}

fn heal_torn_tail_and_seek(file: &mut fs::File, file_len: u64) -> io::Result<()> {
    match inspect_tail(file, file_len)? {
        TailState::Clean { eof } => {
            file.seek(SeekFrom::Start(eof))?;
        }
        TailState::TornTail { valid_end } => {
            file.set_len(valid_end)?;
            file.sync_all()?;
            file.seek(SeekFrom::Start(valid_end))?;
        }
    }
    Ok(())
}

/// Crash-safe reader and `flock`-serialized writer (`O_NOFOLLOW`) for `refs.journal`.
#[derive(Debug, Clone)]
pub struct RefJournal {
    path: PathBuf,
}

impl RefJournal {
    /// Handle for the journal file at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Path to `refs.journal`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Acquire an exclusive `flock` on `refs.journal`, truncate any
    /// unterminated torn trailing fragment back to the last committed newline
    /// boundary (and `sync_all()`), then append one committed entry and
    /// `sync_all()`.
    pub fn append(&self, entry: &JournalEntry) -> Result<(), CasError> {
        validate_entry(entry)?;
        let mut line = serde_json::to_string(entry)
            .map_err(|_| CasError::BadFormat("failed to serialize journal entry"))?;
        line.push('\n');

        let existed = match fs::symlink_metadata(&self.path) {
            Ok(meta) => {
                if !meta.is_file() {
                    return Err(CasError::NotPlain(self.path.clone()));
                }
                true
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };

        let mut file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&self.path)
            .map_err(|error| {
                if error.raw_os_error() == Some(libc::ELOOP) {
                    CasError::NotPlain(self.path.clone())
                } else {
                    error.into()
                }
            })?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(CasError::NotPlain(self.path.clone()));
        }
        if meta.permissions().mode() & 0o777 != 0o600 {
            let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
        }

        // Hold exclusive flock across tail inspection, torn-tail truncation, and write
        // so concurrent writers/readers never interleave with healing or partial lines.
        file.lock()?;
        let file_len = file.metadata()?.len();
        heal_torn_tail_and_seek(&mut file, file_len)?;

        file.write_all(line.as_bytes())?;
        file.sync_all()?;

        if !existed
            && let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            open_nofollow(parent)?.sync_all()?;
        }
        Ok(())
    }

    /// Read all newline-committed entries under a shared `flock`, tolerating an
    /// incomplete/torn trailing line without losing earlier committed entries.
    pub fn read_all(&self) -> Result<Vec<JournalEntry>, CasError> {
        let mut file = match open_nofollow(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
                return Err(CasError::NotPlain(self.path.clone()));
            }
            Err(error) => return Err(error.into()),
        };
        if !file.metadata()?.is_file() {
            return Err(CasError::NotPlain(self.path.clone()));
        }

        file.lock_shared()?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let Some(last_newline) = bytes.iter().rposition(|&b| b == b'\n') else {
            return Ok(Vec::new());
        };

        let mut entries = Vec::new();
        for raw_line in bytes[..last_newline].split(|&b| b == b'\n') {
            if raw_line.is_empty() {
                continue;
            }
            let text = std::str::from_utf8(raw_line)
                .map_err(|_| CasError::BadFormat("non-utf8 committed line in refs.journal"))?;
            let entry: JournalEntry = serde_json::from_str(text).map_err(|_| {
                CasError::BadFormat("malformed committed JSON line in refs.journal")
            })?;
            validate_entry(&entry)?;
            entries.push(entry);
        }
        Ok(entries)
    }

    /// Return the highest committed `seq` in `refs.journal`, or `0` if missing/empty.
    pub fn latest_seq(&self) -> Result<u64, CasError> {
        Ok(self
            .read_all()?
            .into_iter()
            .map(|entry| entry.seq)
            .max()
            .unwrap_or(0))
    }

    /// Return entries strictly newer than `pg_latest_seq` (`seq > pg_latest_seq`)
    /// after validating monotonic `seq` ordering and per-ref fast-forward
    /// chaining inside the replay window.
    pub fn reconcile_entries_after(
        &self,
        pg_latest_seq: u64,
    ) -> Result<Vec<JournalEntry>, CasError> {
        let entries = self.read_all()?;
        let mut prev_seq = 0_u64;
        for entry in &entries {
            if entry.seq <= prev_seq {
                return Err(CasError::BadFormat("non-monotonic seq in refs.journal"));
            }
            prev_seq = entry.seq;
        }

        let suffix: Vec<JournalEntry> = entries
            .into_iter()
            .filter(|entry| entry.seq > pg_latest_seq)
            .collect();

        let mut latest_per_ref: BTreeMap<(&str, &str), &Digest> = BTreeMap::new();
        for entry in &suffix {
            let key = (entry.project.as_str(), entry.name.as_str());
            if let Some(previous_new) = latest_per_ref.get(&key)
                && entry.old_snapshot.as_ref() != Some(*previous_new)
            {
                return Err(CasError::BadFormat(
                    "broken fast-forward ref chain in refs.journal",
                ));
            }
            latest_per_ref.insert(key, &entry.new_snapshot);
        }
        Ok(suffix)
    }
}

impl Store {
    /// Path to `<cas_root>/refs.journal`.
    pub fn refs_journal_path(&self) -> PathBuf {
        self.root().join(REFS_JOURNAL)
    }

    /// Return a [`RefJournal`] handle bound to `<cas_root>/refs.journal`.
    pub fn ref_journal(&self) -> RefJournal {
        RefJournal::new(self.refs_journal_path())
    }

    /// Append and `fsync` a committed entry to `<cas_root>/refs.journal`.
    pub fn append_ref_journal(&self, entry: &JournalEntry) -> Result<(), CasError> {
        self.ref_journal().append(entry)
    }

    /// Read all committed lines from `<cas_root>/refs.journal` (crash-safe
    /// against an incomplete trailing line).
    pub fn read_ref_journal(&self) -> Result<Vec<JournalEntry>, CasError> {
        self.ref_journal().read_all()
    }

    /// Return the latest committed `seq` in `<cas_root>/refs.journal` (`0` if empty).
    pub fn latest_seq(&self) -> Result<u64, CasError> {
        self.ref_journal().latest_seq()
    }

    /// Return strictly fast-forward journal entries with `seq > pg_latest_seq`.
    pub fn reconcile_entries_after(
        &self,
        pg_latest_seq: u64,
    ) -> Result<Vec<JournalEntry>, CasError> {
        self.ref_journal().reconcile_entries_after(pg_latest_seq)
    }
}
