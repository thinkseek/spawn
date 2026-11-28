//! `spawn backup`: the fleet's record off this host.
//!
//! The owner chose local hot recovery plus a remote backup (2026-11-06). One
//! `pg-backup.sh` run a day left a host lost at noon with a copy twelve hours
//! old, nothing carried the mission sources or the transcript archive, and
//! the restore drill was run by hand and compared with the live database.
//! This module is the binary's part of the replacement (docs/storage.md,
//! "Where state lives"); the timers and the bucket rules are the owner's,
//! in deploy/backup.
//!
//! - `spawn backup dump`, every hour: one `pg_dump -Fc` of the database it
//!   records into, its `spawn_events` rows and highest id counted in the
//!   archive itself, checked for plausibility against the newest good
//!   `backup.dumped` record (fewer events, or under half its size, goes to
//!   `postgres/suspect/` and exits 3), encrypted with the passphrase in
//!   `SPAWN_BACKUP_KEY_FILE` and decrypted back to compare its sha256, then
//!   stored under `postgres/hourly/spawn-<YYYYMMDD-HHMMSS>.dump.gpg`, and
//!   also under `postgres/daily/` (and `postgres/weekly/`) when the records
//!   say this is the UTC day's (or the ISO week's) first good dump. Each
//!   PUT's ETag is checked against the MD5 sent and carries
//!   `x-amz-meta-sha256` and `x-amz-meta-spawn-events`.
//! - `spawn backup bundle`, every day: one encrypted tar holding
//!   `manifest.json` and the bundle's contents, full (the first of an ISO
//!   week, or none full within 7 days, from the records) or incremental (its
//!   base the newest `backup.bundled` record): a git bundle of every mission
//!   source's branches and tags — exactly the refs the record names, never a
//!   remote-tracking ref a mirror clone left (the live `supply-pa.git` has
//!   `refs/remotes/github/main`) nor a pending `refs/spawn-owner/` push; a
//!   source with no branches and no tags is recorded `empty`, and an
//!   increment whose refs all point into its base records the `delta` that
//!   changed (a tag added at a held commit, a branch removed), both with no
//!   file in the archive; an increment's entry also names the refs its
//!   bundle holds (`bundled`), since `--not` leaves out a ref the base has
//!   — whole or `--not` its base's heads, and the transcript archive's
//!   files modified
//!   within `SPAWN_BACKUP_TRANSCRIPT_DAYS` (default 14), whole or the bytes
//!   appended since the base's sizes; the record names every window file's
//!   size (`transcripts.sizes`) and the sha256 of its first size bytes
//!   (`transcripts.sha256`, objective 149). The record is a true description of the
//!   archive: verify refuses a bundle that lists a ref its manifest does not
//!   name, so the bundle may hold only what the manifest holds.
//! - `spawn backup verify`, every day: the newest good dump fetched,
//!   decrypted and checked against its record's sha256, restored into a
//!   database of its own on `SPAWN_BACKUP_DRILL_DATABASE_URL` (never the
//!   source, never one it did not create), its `spawn_events` and highest id
//!   compared with the record's, and the database dropped again; then the
//!   newest `backup.bundled` object fetched, decrypted, decompressed by its
//!   magic and unpacked, its `manifest.json`, transcript files and every
//!   repo's `git bundle` (heads listed; a full one verified alone) compared
//!   with its record, so the sources and transcripts are proven to open too:
//!   exit 0 when both pass, 1 when either does not, 2 when it cannot run.
//! - `spawn backup restore-drill`, by hand until objective 149b (core (ii)):
//!   the newest stored
//!   bundle and its chain back to a full one restored into a scratch
//!   directory of its own (mode 700, never under `$HOME`, `$SPAWN_LIVE` or
//!   the root), the transcript archive and every mission source, judged
//!   against the newest record's sha256 and compared with live, read only
//!   ([`restore_drill`]). `--only missions` (objective 186, daily from
//!   its own timer) drills the mission repositories alone: the bundles
//!   decrypted in a pipe, only their manifests and repository bundles
//!   unpacked, never a transcript, and `backup.drilled.missions` recorded
//!   alone. `--only controllers` (objective 198) checks each live
//!   controller, current and previous, against `git archive` of its commit
//!   from the live source, and records one `backup.drilled.controllers` per
//!   controller. `--only transcripts` (objective 200, daily from its own timer)
//!   drills the transcript archive alone the same way: only the manifests
//!   and the transcript slices unpacked, restored into the drill's own
//!   folder and judged by the newest record's sizes and sha256, nothing
//!   live read, refused under `SPAWN_BACKUP_DRILL_FLOOR_MB` (default 40960)
//!   of free space left, and `backup.drilled.transcripts` recorded alone.
//!
//! The bucket stays inside R2's free tier: `SPAWN_BACKUP_R2_LIMIT_MB`
//! (default 10000) is 10,000,000,000 bytes, and nothing is ever written
//! past it — every run lists the whole bucket under `<root>/.backup.lock`
//! and writes no copy whose bytes would pass the limit, the hourly copy
//! included (a dump whose hourly copy was skipped exits 4, so the timer's
//! unit fails and a person sees it). `SPAWN_BACKUP_R2_ALERT_MB` (default
//! 8000) is the alert: the record says `over_alert` and the run says it on
//! its output; objective 030 raises its own alert from the record.
//!
//! With `SPAWN_BACKUP_SYNC_DIR` set (objective 216; the bundle unit sets
//! `/var/tmp/spawn/sync`), the bundle also takes the fleet's notes folder,
//! read only: every file in a full, the new or changed ones (size, mtime,
//! sha256) in an increment, and a manifest of the whole folder inside the
//! bundle (`sync/files`, `sync/manifest.json`), the part scanned for
//! credential values first; a hit, a place the scan could not search, a
//! failed scan or a file the run cannot read drops that part only
//! ([`sync_dir`]). Its scratch is then a hidden directory in the workspace
//! root, never in TMPDIR. Unset, the bundle is as it was. A bundle's PUT is
//! given 600 s plus its size at 100 KB/s ([`bundle_put_seconds`]).
//!
//! The bundle's tar is compressed before it is encrypted — zstd (`-3`) when
//! the host has it, gzip when it does not, plain otherwise — and the key
//! names what it holds (`<...>.tar.zst.gpg`, `.tar.gz.gpg`, `.tar.gpg`): on
//! 2026-11-08 a real full bundle of 426 MB of transcripts and two sources
//! was 90 MB as zstd and 437 MB as a plain tar. `SPAWN_BACKUP_PREFIX`
//! (empty in the fleet) puts every object under a prefix, so a measurement
//! or a test writes to a scratch place (`locktest/`) and never over a real
//! backup; the records name the stored key, prefix included.
//!
//! Secrets come from files the owner made mode 600 and are never on a
//! command line, in the output or in a record: the passphrase file goes to
//! gpg by path, the R2 credentials to curl on stdin (`--config -`), and the
//! database password to libpq through `PGPASSWORD`. A drop the plausibility
//! check doubts is accepted once by the owner with
//! `SPAWN_BACKUP_ACCEPT_DROP=1`, which still records the drop (`accepted`).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use spawn_postgres::PostgresStore;

use crate::Flags;
use crate::config::{
    CREDENTIAL_EXIT_CODE, database_url, env_or, env_value, open_store, workspace_root,
};

mod recoverable;
mod restore_drill;
mod store;
mod sync_dir;

/// The default limits, in decimal megabytes (1,000,000 bytes each): R2's
/// free tier is 10 GB-month of storage, and the owner's alert is at 8 GB.
const DEFAULT_LIMIT_MB: u64 = 10_000;
const DEFAULT_ALERT_MB: u64 = 8_000;

/// A byte count per `SPAWN_BACKUP_R2_*` megabyte.
const MB: u64 = 1_000_000;

/// Keep `LoadCredential=` filename for R2 bucket endpoint, bucket name, and S3 keys.
const CRED_R2: &str = "r2.credentials";
/// Keep `LoadCredential=` default k1 encryption passphrase filename.
const CRED_BACKUP_KEY: &str = "backup.encryption-key";
/// Keep `LoadCredential=` filename for the disposable restore-drill database URL.
const CRED_DRILL_DB_URL: &str = "spawn.backup-drill-database-url";

/// The usage of `spawn backup`, the only copy (main's help reads it).
pub(crate) const USAGE: &str = "usage: spawn backup dump|bundle|verify [--root <dir>]\n       spawn backup restore-drill [--root <dir>] [--into <dir>] [--keep] [--only missions|controllers|transcripts|sync]\n       spawn backup cas snapshot|restore|verify|key-of [...]";

/// One `postgres://` URL, read the way libpq reads it (scripts/pg-url.sh):
/// user and password percent-decoded, an IPv6 host in brackets, a socket
/// directory percent-encoded, and `host=`, `port=`, `user=`, `password=` and
/// `dbname=` parameters overriding the authority's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PgUrl {
    pub(crate) user: String,
    pub(crate) password: String,
    pub(crate) host: String,
    pub(crate) port: String,
    pub(crate) database: String,
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && let Some(byte) = text
                .get(index + 1..index + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            && byte != 0
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn parse_pg_url(url: &str) -> Result<PgUrl, String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("{url:?} is not a postgres:// URL"))?;
    if scheme != "postgres" && scheme != "postgresql" {
        return Err(format!("{url:?} is not a postgres:// URL"));
    }
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (authority, database) = rest.split_once('/').unwrap_or((rest, ""));
    if authority.contains(',') {
        return Err("a URL of several hosts is not read".to_string());
    }
    let (userinfo, hostport) = authority.rsplit_once('@').unwrap_or(("", authority));
    let (user, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
    let (host, port) = if let Some(rest) = hostport.strip_prefix('[') {
        let (host, rest) = rest
            .split_once(']')
            .ok_or_else(|| "an unterminated IPv6 host".to_string())?;
        (host, rest.strip_prefix(':').unwrap_or(""))
    } else {
        hostport.split_once(':').unwrap_or((hostport, ""))
    };
    let mut parsed = PgUrl {
        user: percent_decode(user),
        password: percent_decode(password),
        host: percent_decode(host),
        port: port.to_string(),
        database: percent_decode(database),
    };
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = percent_decode(value);
        match name {
            "host" => parsed.host = value,
            "port" => parsed.port = value,
            "user" => parsed.user = value,
            "password" => parsed.password = value,
            "dbname" => parsed.database = value,
            _ => {}
        }
    }
    if parsed.port.is_empty() {
        parsed.port = "5432".to_string();
    }
    Ok(parsed)
}

impl PgUrl {
    /// The name of the server, for a record: host:port or a socket
    /// directory, never a password.
    fn server(&self) -> String {
        if self.host.is_empty() {
            "localhost".to_string()
        } else if self.host.starts_with('/') {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// The connection settings libpq reads from the environment, so a
    /// password is never on a command line.
    fn env(&self) -> Vec<(&'static str, String)> {
        let mut env = vec![("PGCONNECT_TIMEOUT", "20".to_string())];
        if !self.host.is_empty() {
            env.push(("PGHOST", self.host.clone()));
        }
        env.push(("PGPORT", self.port.clone()));
        if !self.user.is_empty() {
            env.push(("PGUSER", self.user.clone()));
        }
        if !self.password.is_empty() {
            env.push(("PGPASSWORD", self.password.clone()));
        }
        env
    }
}

/// The settings a run works with: the owner's files, checked (mode 600, not
/// empty), and the numbers parsed.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    pub(crate) root: PathBuf,
    /// The file the store seals with ([`seal`]): the sealing key's when it
    /// is required (the store), else k1's as a stand-in nothing seals with.
    pub(crate) key_file: PathBuf,
    /// The key new dumps and bundles are sealed with, or why there is none
    /// ([`sealing_key`]): a run that seals refuses on the reason; a reader
    /// never needs it.
    pub(crate) seal: Result<Key, String>,
    pub(crate) r2: R2,
    pub(crate) pg_bin: Option<PathBuf>,
    pub(crate) compress: String,
    pub(crate) transcript_days: i64,
    pub(crate) limit_bytes: u64,
    pub(crate) alert_bytes: u64,
    /// `SPAWN_BACKUP_PREFIX`: prepended to every object's key, for a run
    /// that must not touch a real backup (a test writes under `locktest/`).
    /// Empty in the fleet. The records name the stored key — prefix included
    /// — so a restore reads exactly the object the run wrote.
    pub(crate) prefix: String,
    pub(crate) drill_url: Option<String>,
}

/// The bucket and the credentials curl signs with.
#[derive(Debug, Clone)]
pub(crate) struct R2 {
    pub(crate) endpoint: String,
    pub(crate) bucket: String,
    pub(crate) access_key: String,
    pub(crate) secret_key: String,
}

impl R2 {
    fn url(&self, path: &str) -> String {
        format!(
            "{}/{}/{}",
            self.endpoint.trim_end_matches('/'),
            self.bucket,
            path
        )
    }
}

/// The mode of a file, or `None` where the platform has none.
fn mode_of(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(path).ok().map(|meta| meta.mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// A file the owner made mode 600 and did not leave empty: a passphrase or
/// an environment. Its content is never printed.
fn private_file(path: &Path, what: &str) -> Result<(), String> {
    let meta = fs::metadata(path).map_err(|error| format!("{what} {}: {error}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{what} {} is not a file", path.display()));
    }
    if meta.len() == 0 {
        return Err(format!("{what} {} is empty", path.display()));
    }
    if let Some(mode) = mode_of(path)
        && mode & 0o077 != 0
    {
        return Err(format!(
            "{what} {} has mode {mode:o}: it must be 600",
            path.display()
        ));
    }
    Ok(())
}

/// What a value that is not a backup key name is refused with: the rule
/// alone. The value is never echoed: it may hold anything, a key's content
/// included (cc-astra A53). Each caller puts the source before it.
pub(crate) const KEY_NAME_RULE: &str = "is not a backup key name (k and a number from 1, as k2)";

/// The one place that holds the default key (objective 114): `None` gives
/// `k1`; a name `k` and a number from 1 (`^k[1-9][0-9]*$`) gives itself;
/// anything else is refused with [`KEY_NAME_RULE`].
pub(crate) fn key_name(named: Option<&str>) -> Result<String, String> {
    let Some(name) = named else {
        return Ok("k1".to_string());
    };
    let mut digits = name.strip_prefix('k').unwrap_or("").bytes();
    let well_formed = digits
        .next()
        .is_some_and(|first| (b'1'..=b'9').contains(&first))
        && digits.all(|digit| digit.is_ascii_digit());
    if well_formed {
        Ok(name.to_string())
    } else {
        Err(KEY_NAME_RULE.to_string())
    }
}

/// A backup key: its name and the file that holds its passphrase (given to
/// gpg by path; its content is never read here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Key {
    pub(crate) name: String,
    pub(crate) file: PathBuf,
}

/// Where the key files are: `SPAWN_BACKUP_KEY_DIR`, else `~/.config/spawn`.
fn key_dir() -> PathBuf {
    env_value("SPAWN_BACKUP_KEY_DIR").map_or_else(|| home().join(".config/spawn"), PathBuf::from)
}

/// The file of the key `name` (a name [`key_name`] gave) in `dir`, or the
/// path the lookup would use, in words, when there is none. k1's is
/// `explicit` (`SPAWN_BACKUP_KEY_FILE`) when set, else `backup.key.k1`, else
/// the old `backup.key`; every other key's is only `backup.key.<name>`.
fn find_key(name: &str, explicit: Option<PathBuf>, dir: &Path) -> Result<Key, String> {
    let found = |file: PathBuf| Key {
        name: name.to_string(),
        file,
    };
    let named = dir.join(format!("backup.key.{name}"));
    if name != "k1" {
        return if named.exists() {
            Ok(found(named))
        } else {
            Err(named.display().to_string())
        };
    }
    if let Some(file) = explicit {
        return if file.exists() {
            Ok(found(file))
        } else {
            Err(file.display().to_string())
        };
    }
    let old = dir.join("backup.key");
    if named.exists() {
        Ok(found(named))
    } else if old.exists() {
        Ok(found(old))
    } else {
        Err(format!("{} (or backup.key)", named.display()))
    }
}

/// Whether an error came from a Keep `$CREDENTIALS_DIRECTORY/<id>` validation failure.
pub(super) fn is_credential_error(reason: &str) -> bool {
    reason.starts_with("credential ")
}

/// Configuration refusal exit code: `78` (`EX_CONFIG`) on Keep credential errors, `2` otherwise.
pub(super) fn config_exit_code(reason: &str) -> i32 {
    if is_credential_error(reason) {
        CREDENTIAL_EXIT_CODE
    } else {
        2
    }
}

/// Resolve the disposable restore-drill PostgreSQL URL, preferring Keep's
/// `$CREDENTIALS_DIRECTORY/spawn.backup-drill-database-url` when present.
fn drill_database_url() -> Result<Option<String>, String> {
    if let Some(url) = crate::config::credential_value(CRED_DRILL_DB_URL)? {
        return Ok(Some(url));
    }
    Ok(env_value("SPAWN_BACKUP_DRILL_DATABASE_URL"))
}

/// Like [`find_key`], but prefers validated files under `cred_dir` (`$CREDENTIALS_DIRECTORY`)
/// when delivered (`backup.key.<name>`, or `backup.encryption-key` for `k1`). Fails closed on
/// malformed or symlinked credential files in `cred_dir`.
fn find_key_with_credentials(
    name: &str,
    cred_dir: Option<&Path>,
    explicit: Option<PathBuf>,
    dir: &Path,
) -> Result<Key, String> {
    if let Some(cdir) = cred_dir {
        let named = format!("backup.key.{name}");
        if let Some(file) = crate::config::load_credential_file(Some(cdir), &named)? {
            return Ok(Key {
                name: name.to_string(),
                file,
            });
        }
        if name == "k1"
            && let Some(file) = crate::config::load_credential_file(Some(cdir), CRED_BACKUP_KEY)?
        {
            return Ok(Key {
                name: name.to_string(),
                file,
            });
        }
    }
    find_key(name, explicit, dir)
}

/// [`find_key_with_credentials`] in `$CREDENTIALS_DIRECTORY` and this host's key directory.
fn key_on_host(name: &str) -> Result<Key, String> {
    find_key_with_credentials(
        name,
        env_value("CREDENTIALS_DIRECTORY").as_deref().map(Path::new),
        env_value("SPAWN_BACKUP_KEY_FILE").map(PathBuf::from),
        &key_dir(),
    )
}

/// The key that reads what `what` names ("the dump", "the bundle", "the
/// bundle chain"), sealed with `name`: its file on this host, mode 600.
fn reading_key(name: &str, what: &str) -> Result<Key, String> {
    let key = key_on_host(name).map_err(|err| {
        if is_credential_error(&err) {
            err
        } else {
            format!("{what} is sealed with {name}; its key file {err} is not on this host")
        }
    })?;
    private_file(&key.file, "the backup key")?;
    Ok(key)
}

/// The key every new dump and bundle is sealed with: `SPAWN_BACKUP_SEAL_KEY`
/// (unset: k1), its file present and mode 600. Never k1 in its place.
fn sealing_key() -> Result<Key, String> {
    let name = key_name(env_value("SPAWN_BACKUP_SEAL_KEY").as_deref())
        .map_err(|rule| format!("SPAWN_BACKUP_SEAL_KEY {rule}"))?;
    let key = key_on_host(&name).map_err(|err| {
        if is_credential_error(&err) {
            err
        } else {
            format!("the sealing key {name}: its key file {err} is not on this host")
        }
    })?;
    private_file(&key.file, "the backup key")?;
    Ok(key)
}

/// The key a `backup.dumped` or `backup.bundled` record says sealed it: its
/// `sealed_with` (absent: k1, as every record before objective 114).
fn record_key(record_type: &str, event_id: i64, record: &Value) -> Result<String, String> {
    let refused =
        |rule: String| format!("the sealed_with of {record_type} record {event_id} {rule}");
    match &record["sealed_with"] {
        Value::Null => key_name(None),
        Value::String(name) => key_name(Some(name)).map_err(refused),
        _ => Err(refused(KEY_NAME_RULE.to_string())),
    }
}

/// The key an object says sealed it: its `x-amz-meta-spawn-seal` (absent:
/// k1, as every object before objective 114).
fn object_key(object: &str, headers: &str) -> Result<String, String> {
    key_name(header_value(headers, "x-amz-meta-spawn-seal").as_deref())
        .map_err(|rule| format!("the x-amz-meta-spawn-seal of {object} {rule}"))
}

/// The record's key and its object's, which must be one.
fn same_key(record: &str, object: &str) -> Result<(), String> {
    if record == object {
        Ok(())
    } else {
        Err(format!("record says {record}, object says {object}"))
    }
}

/// The store's key file: the sealing key's, which a run that `required` it
/// cannot do without (a bad name or a missing file refuses it, never k1's
/// file in its place); not required, the sealing key's when there is one,
/// else k1's (`stand_in`), which nothing seals with.
fn store_key_file(
    seal: &Result<Key, String>,
    required: bool,
    stand_in: impl FnOnce() -> PathBuf,
) -> Result<PathBuf, String> {
    match seal {
        Ok(key) => Ok(key.file.clone()),
        Err(reason) if required => Err(reason.clone()),
        Err(_) => Ok(stand_in()),
    }
}

/// The `NAME=value` lines of an environment file, parsed, never sourced.
fn parse_env_file(text: &str) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some((name, value)) = line.split_once('=') {
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
                .or_else(|| {
                    value
                        .strip_prefix('\'')
                        .and_then(|rest| rest.strip_suffix('\''))
                })
                .unwrap_or(value);
            vars.insert(name.trim().to_string(), value.to_string());
        }
    }
    vars
}

/// A number of megabytes from the environment: unset is the default, and
/// what does not parse is refused (with the line that says why).
fn megabytes(name: &str, default: u64) -> Result<u64, String> {
    match env_value(name) {
        None => Ok(default.saturating_mul(MB)),
        Some(text) => text
            .trim()
            .parse::<u64>()
            .map(|value| value.saturating_mul(MB))
            .map_err(|_| format!("{name}: {text:?} is not a number of megabytes")),
    }
}

/// `SPAWN_BACKUP_PREFIX` as a key prefix: empty, or letters, digits, `.`,
/// `_`, `-` and `/` ending in `/`; nothing that could leave the bucket's
/// namespace (`..`, a leading `/`).
fn prefix_value(text: Option<String>) -> Result<String, String> {
    let Some(text) = text else {
        return Ok(String::new());
    };
    let text = text.trim();
    if text.is_empty() {
        return Ok(String::new());
    }
    if text.starts_with('/')
        || !text
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '/'))
        || text
            .trim_end_matches('/')
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || !text.ends_with('/')
    {
        return Err(format!(
            "SPAWN_BACKUP_PREFIX: {text:?} is not a key prefix (letters, digits, '.', '_', \
             '-' and '/', ending in '/')"
        ));
    }
    Ok(text.to_string())
}

/// Resolve R2 endpoint, bucket, and S3 keys, preferring Keep's `$CREDENTIALS_DIRECTORY/r2.credentials`
/// (fail-closed on bad mode, symlink, empty file, or missing S3 keys without falling back to env/r2.env)
/// and falling back to `SPAWN_R2_ENV_FILE` / `~/.config/spawn/r2.env` when Keep has not delivered it.
fn resolve_backup_r2(
    cred_dir: Option<&Path>,
    explicit_env_file: Option<PathBuf>,
    home: &Path,
    env_lookup: impl Fn(&str) -> Option<String>,
) -> Result<R2, String> {
    if let Some(cred_file) = crate::config::load_credential_file(cred_dir, CRED_R2)? {
        let vars = parse_env_file(
            &fs::read_to_string(&cred_file)
                .map_err(|error| format!("credential {CRED_R2}: {error}"))?,
        );
        let secret_needed = |name: &str| -> Result<String, String> {
            vars.get(name)
                .filter(|value| !value.trim().is_empty())
                .cloned()
                .ok_or_else(|| format!("credential {CRED_R2} is missing {name}"))
        };
        let setting_needed = |name: &str| -> Result<String, String> {
            vars.get(name)
                .filter(|value| !value.trim().is_empty())
                .cloned()
                .or_else(|| env_lookup(name))
                .ok_or_else(|| format!("credential {CRED_R2} is missing {name}"))
        };
        let r2 = R2 {
            endpoint: setting_needed("SPAWN_R2_ENDPOINT")?,
            bucket: setting_needed("SPAWN_R2_BUCKET")?,
            access_key: secret_needed("SPAWN_R2_ACCESS_KEY_ID")?,
            secret_key: secret_needed("SPAWN_R2_SECRET_ACCESS_KEY")?,
        };
        not_locked_forever(&r2.bucket)?;
        return Ok(r2);
    }
    let env_file = explicit_env_file.unwrap_or_else(|| home.join(".config/spawn/r2.env"));
    private_file(&env_file, "the R2 environment")?;
    let vars = parse_env_file(
        &fs::read_to_string(&env_file)
            .map_err(|error| format!("the R2 environment {}: {error}", env_file.display()))?,
    );
    let needed = |name: &str| -> Result<String, String> {
        vars.get(name)
            .cloned()
            .or_else(|| env_lookup(name))
            .ok_or_else(|| format!("{name} is not set in {}", env_file.display()))
    };
    let r2 = R2 {
        endpoint: needed("SPAWN_R2_ENDPOINT")?,
        bucket: needed("SPAWN_R2_BUCKET")?,
        access_key: needed("SPAWN_R2_ACCESS_KEY_ID")?,
        secret_key: needed("SPAWN_R2_SECRET_ACCESS_KEY")?,
    };
    not_locked_forever(&r2.bucket)?;
    Ok(r2)
}

impl Settings {
    /// `key` as it is stored: under `SPAWN_BACKUP_PREFIX`, or as given.
    fn stored(&self, key: &str) -> String {
        format!("{}{key}", self.prefix)
    }

    /// `spawn backup`'s settings: its bucket and credentials from Keep or r2.env
    /// ([`Self::backup_r2`]), never the keep bucket.
    /// It needs no key file: a reader reads each record with the key it
    /// names, and a run that seals takes [`Self::seal`].
    pub(crate) fn from_env(flags: &Flags) -> Result<Self, String> {
        Self::build(flags, Self::backup_r2, false)
    }

    /// The bucket and credentials `spawn backup` works with: Keep's
    /// `$CREDENTIALS_DIRECTORY/r2.credentials` when present, else r2.env's
    /// (`SPAWN_R2_ENV_FILE`), never allowing the keep bucket.
    fn backup_r2() -> Result<R2, String> {
        resolve_backup_r2(
            env_value("CREDENTIALS_DIRECTORY").as_deref().map(Path::new),
            env_value("SPAWN_R2_ENV_FILE").map(PathBuf::from),
            &home(),
            env_value,
        )
    }

    /// The settings with the bucket and credentials `r2` gives (the store
    /// passes its own credential's, so it never needs r2.env). The store
    /// seals with the sealing key's file, so it requires it.
    pub(crate) fn with_r2(
        flags: &Flags,
        r2: impl FnOnce() -> Result<R2, String>,
    ) -> Result<Self, String> {
        Self::build(flags, r2, true)
    }

    fn build(
        flags: &Flags,
        r2: impl FnOnce() -> Result<R2, String>,
        key_required: bool,
    ) -> Result<Self, String> {
        let root = workspace_root(flags)
            .map(PathBuf::from)
            .ok_or_else(|| "--root or SPAWN_WORKSPACE_ROOT is required".to_string())?;
        let seal = sealing_key();
        let key_file = store_key_file(&seal, key_required, || {
            key_on_host("k1").map_or_else(|_| key_dir().join("backup.key.k1"), |key| key.file)
        })?;
        let r2 = r2()?;
        let prefix = prefix_value(env_value("SPAWN_BACKUP_PREFIX"))?;
        let limit_bytes = megabytes("SPAWN_BACKUP_R2_LIMIT_MB", DEFAULT_LIMIT_MB)?;
        let alert_bytes = megabytes("SPAWN_BACKUP_R2_ALERT_MB", DEFAULT_ALERT_MB)?;
        if alert_bytes > limit_bytes {
            return Err(format!(
                "SPAWN_BACKUP_R2_ALERT_MB ({alert_bytes} bytes) is above \
SPAWN_BACKUP_R2_LIMIT_MB ({limit_bytes} bytes)"
            ));
        }
        let transcript_days = match env_value("SPAWN_BACKUP_TRANSCRIPT_DAYS") {
            None => 14,
            Some(text) => text.trim().parse::<i64>().map_err(|_| {
                format!("SPAWN_BACKUP_TRANSCRIPT_DAYS: {text:?} is not a number of days")
            })?,
        };
        Ok(Self {
            root,
            key_file,
            seal,
            r2,
            pg_bin: env_value("SPAWN_BACKUP_PG_BIN").map(PathBuf::from),
            compress: env_or("SPAWN_BACKUP_DUMP_COMPRESS", "zstd:9"),
            transcript_days,
            limit_bytes,
            alert_bytes,
            prefix,
            drill_url: drill_database_url()?,
        })
    }

    /// A tool's path: `SPAWN_BACKUP_PG_BIN`'s directory for the PostgreSQL
    /// tools, else the name on `PATH`.
    fn pg_tool(&self, name: &str) -> PathBuf {
        match &self.pg_bin {
            Some(directory) => directory.join(name),
            None => PathBuf::from(name),
        }
    }
}

/// Whether `name`, exactly as given (never trimmed or folded), is an R2
/// bucket name: 3 to 63 of lower-case letters, digits and `-`, a letter or
/// digit at each end.
fn r2_bucket_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    let inner = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    (3..=63).contains(&bytes.len())
        && bytes.first().is_some_and(inner)
        && bytes.last().is_some_and(inner)
        && bytes.iter().all(|b| inner(b) || *b == b'-')
}

/// `spawn backup` writes only to a bucket whose name is an R2 bucket name
/// as given (rev-g F5: "X-KEEP" or "x-keep " would pass a suffix test and
/// might still reach the keep bucket), and never to the bucket locked for
/// good (the keep bucket, [`store::locked_forever`]). Both before any
/// request; the name is never in the message.
fn not_locked_forever(bucket: &str) -> Result<(), String> {
    if !r2_bucket_name(bucket) {
        return Err(
            "refused: SPAWN_R2_BUCKET is not an R2 bucket name as given (3 to 63 lower-case \
letters, digits and -, no space): nothing is sent"
                .to_string(),
        );
    }
    if store::locked_forever(bucket) {
        return Err(
            "refused: SPAWN_R2_BUCKET names the keep bucket (locked for good); spawn backup \
never writes there, only spawn store does"
                .to_string(),
        );
    }
    Ok(())
}

fn home() -> PathBuf {
    env_value("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

/// An exclusive lock held for a run's whole life: `<root>/.backup.lock`.
struct Lock {
    file: File,
}

impl Lock {
    fn acquire(root: &Path) -> Result<Self, String> {
        let path = root.join(".backup.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|error| format!("the lock {}: {error}", path.display()))?;
        file.lock()
            .map_err(|error| format!("the lock {}: {error}", path.display()))?;
        Ok(Self { file })
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// A scratch directory removed when the run ends.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(what: &str) -> Result<Self, String> {
        Self::new_in(&std::env::temp_dir(), &format!("spawn-{what}"))
    }

    /// A scratch directory `<parent>/<name>-<pid>-<random>`.
    fn new_in(parent: &Path, name: &str) -> Result<Self, String> {
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        for attempt in 0..10u32 {
            let path = parent.join(format!(
                "{name}-{}-{}{attempt}",
                std::process::id(),
                random_hex(4)?
            ));
            match builder.create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(format!("{}: {error}", path.display())),
            }
        }
        Err("cannot make a scratch directory".to_string())
    }

    fn join(&self, name: impl AsRef<Path>) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Bytes as lower-case hex digits.
#[allow(clippy::format_collect)]
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Hex digits from the system's random device.
fn random_hex(bytes: usize) -> Result<String, String> {
    let mut buffer = vec![0u8; bytes];
    File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut buffer))
        .map_err(|error| format!("/dev/urandom: {error}"))?;
    Ok(hex(&buffer))
}

/// sha256 of a file.
fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

/// The MD5 of a file, as S3's ETag spells it: computed here so no tool or
/// crate is needed for it.
fn md5_file(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(md5_hex(&bytes))
}

#[allow(clippy::unreadable_literal, clippy::many_single_char_names)]
pub(crate) fn md5_hex(bytes: &[u8]) -> String {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];
    let mut message = bytes.to_vec();
    let bits = (bytes.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bits.to_le_bytes());
    let (mut a0, mut b0, mut c0, mut d0) =
        (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
    for chunk in message.chunks(64) {
        let mut m = [0u32; 16];
        for (index, word) in chunk.chunks(4).enumerate() {
            m[index] = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
        }
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for index in 0..64 {
            let (f, g) = match index / 16 {
                0 => ((b & c) | (!b & d), index),
                1 => ((d & b) | (!d & c), (5 * index + 1) % 16),
                2 => (b ^ c ^ d, (3 * index + 5) % 16),
                _ => (c ^ (b | !d), (7 * index) % 16),
            };
            let temp = d;
            d = c;
            c = b;
            b = b.wrapping_add(
                a.wrapping_add(f)
                    .wrapping_add(K[index])
                    .wrapping_add(m[g])
                    .rotate_left(S[index]),
            );
            a = temp;
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    let mut bytes = [0u8; 16];
    for (index, word) in [a0, b0, c0, d0].into_iter().enumerate() {
        bytes[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    hex(&bytes)
}

/// A UTC time as `YYYYMMDD-HHMMSS` and the fields a tier decision needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stamp {
    pub(crate) day: i64,
    pub(crate) monday: i64,
    pub(crate) text: [char; 15],
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

impl Stamp {
    /// The UTC stamp of an epoch in ms.
    pub(crate) fn at(ms: i64) -> Self {
        let seconds = ms.div_euclid(1000);
        let day = seconds.div_euclid(86_400);
        let rest = seconds.rem_euclid(86_400);
        let (year, month, date) = civil_from_days(day);
        let text = format!(
            "{year:04}{month:02}{date:02}-{:02}{:02}{:02}",
            rest / 3600,
            rest % 3600 / 60,
            rest % 60
        );
        let mut chars = ['0'; 15];
        for (at, c) in text.chars().enumerate() {
            if at < 15 {
                chars[at] = c;
            }
        }
        Self {
            day,
            // The Monday of the ISO week: 1970-01-01 was a Thursday.
            monday: day - (day + 3).rem_euclid(7),
            text: chars,
        }
    }

    pub(crate) fn text(&self) -> String {
        self.text.iter().collect()
    }

    /// The stamp a key or a name spells, `YYYYMMDD-HHMMSS` anywhere inside.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        for start in 0..bytes.len().saturating_sub(14) {
            let candidate = &text[start..start + 15];
            let bytes = candidate.as_bytes();
            if bytes[8] != b'-' || !bytes[..8].iter().chain(&bytes[9..]).all(u8::is_ascii_digit) {
                continue;
            }
            let number = |a: usize, b: usize| -> i64 {
                candidate
                    .get(a..b)
                    .and_then(|part| part.parse().ok())
                    .unwrap_or(0)
            };
            let day = days_from_civil(number(0, 4), number(4, 6), number(6, 8));
            let seconds = number(9, 11) * 3600 + number(11, 13) * 60 + number(13, 15);
            let ms = (day * 86_400 + seconds) * 1000;
            if ms >= 0 {
                return Some(Self::at(ms));
            }
        }
        None
    }
}

/// A bundle's object name (objective 167):
/// `bundles/<tier>/spawn-bundle-YYYYMMDD-HHMMSS-mmm-<random>.<extension>.gpg`,
/// the UTC second, the millisecond in three digits and `random` (4 bytes of
/// /dev/urandom as hex). Two bundles in one second (a by-hand one beside the
/// timer's) no longer name one object, which the bucket's lock refuses
/// (409); the names still sort, by byte, as the bundles ran (the second,
/// then the fixed-width millisecond; the random part only parts two of one
/// millisecond), and [`Stamp::parse`] still reads the second from them.
pub(crate) fn bundle_name(tier: &str, now_ms: i64, extension: &str, random: &str) -> String {
    format!(
        "bundles/{tier}/spawn-bundle-{}-{:03}-{random}.{extension}.gpg",
        Stamp::at(now_ms).text(),
        now_ms.rem_euclid(1000)
    )
}

/// Whether a dump's copy may be written: the bucket's bytes, the copies this
/// run already wrote and this one together stay within the limit.
pub(crate) fn within_limit(held: u64, written: u64, copy: u64, limit: u64) -> bool {
    held.saturating_add(written).saturating_add(copy) <= limit
}

/// The tier of a dump besides hourly: the UTC day's first good dump goes to
/// `daily/`, the ISO week's first to `weekly/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tier {
    Hourly,
    Daily,
    Weekly,
}

impl Tier {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Hourly => "hourly",
            Self::Daily => "daily",
            Self::Weekly => "weekly",
        }
    }

    pub(crate) fn key(self, stamp: &str) -> String {
        format!("postgres/{}/spawn-{stamp}.dump.gpg", self.name())
    }
}

/// The tiers a new dump gets, from the earlier good dumps' records: hourly
/// always; daily when no earlier good dump shares the UTC day; weekly when
/// none shares the ISO week.
pub(crate) fn tiers_for(now: Stamp, earlier: &[Stamp]) -> Vec<Tier> {
    let mut tiers = vec![Tier::Hourly];
    if !earlier.iter().any(|stamp| stamp.day == now.day) {
        tiers.push(Tier::Daily);
    }
    if !earlier.iter().any(|stamp| stamp.monday == now.monday) {
        tiers.push(Tier::Weekly);
    }
    tiers
}

/// Whether a bundle is a full one: the first of an ISO week, or no full
/// bundle recorded within the last seven days.
pub(crate) fn full_bundle(now: Stamp, earlier: &[(Stamp, bool, i64)], now_ms: i64) -> bool {
    let fresh = earlier
        .iter()
        .any(|(stamp, full, _)| *full && stamp.monday == now.monday);
    let recent_full = earlier
        .iter()
        .any(|(_, full, at)| *full && now_ms.saturating_sub(*at) < 7 * 86_400_000);
    !fresh || !recent_full
}

/// Where an increment writes a transcript file: from the base record's size
/// when it names the file and the file did not shrink, else whole.
pub(crate) fn transcript_offset(base: Option<u64>, size: u64) -> u64 {
    match base {
        Some(base) if size >= base => base,
        _ => 0,
    }
}

/// The event count and the highest id in a `pg_restore -a -t spawn_events`
/// output (`COPY ... (id, ...) FROM stdin;` and its tab-separated rows).
pub(crate) fn copy_events(text: &str) -> (i64, i64) {
    let mut inside = false;
    let mut column = 0usize;
    let mut rows = 0i64;
    let mut max = 0i64;
    for line in text.lines() {
        if !inside {
            if line.starts_with("COPY ") && line.contains("spawn_events") {
                let columns = line
                    .split_once('(')
                    .and_then(|(_, rest)| rest.split_once(')'))
                    .map_or("", |(inner, _)| inner);
                column = columns
                    .split(',')
                    .position(|name| name.trim() == "id")
                    .unwrap_or(0);
                inside = true;
            }
            continue;
        }
        if line == "\\." {
            break;
        }
        rows += 1;
        if let Some(field) = line.split('\t').nth(column)
            && let Ok(id) = field.trim().parse::<i64>()
            && id > max
        {
            max = id;
        }
    }
    (rows, max)
}

/// The plausibility check: a dump holding fewer events than the newest good
/// one, or under half its size, is suspect.
pub(crate) fn suspect(events: i64, bytes: u64, previous: Option<(i64, u64)>) -> Option<String> {
    let (events_before, bytes_before) = previous?;
    if events < events_before {
        return Some(format!(
            "it holds {events} events, fewer than the newest good dump's {events_before}"
        ));
    }
    if bytes.saturating_mul(2) < bytes_before {
        return Some(format!(
            "it is {bytes} bytes, under half the newest good dump's {bytes_before}"
        ));
    }
    None
}

/// The keys and sizes of one ListObjectsV2 page, and whether more follow.
pub(crate) fn parse_listing(xml: &str) -> (Vec<(String, u64)>, Option<String>) {
    let (entries, token) = parse_listing_tagged(xml);
    (
        entries
            .into_iter()
            .map(|listed| (listed.key, listed.size))
            .collect(),
        token,
    )
}

/// A listing page's keys with their size and ETag (unquoted, lower case:
/// the XML spells the quotes `&quot;` or as they are).
pub(crate) fn parse_listing_tagged(xml: &str) -> (Vec<store::Listed>, Option<String>) {
    let mut entries = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<Contents>") {
        let block = &rest[start + "<Contents>".len()..];
        let Some(end) = block.find("</Contents>") else {
            break;
        };
        let body = &block[..end];
        let key = between(body, "<Key>", "</Key>").unwrap_or_default();
        let size = between(body, "<Size>", "</Size>")
            .and_then(|text| text.trim().parse::<u64>().ok())
            .unwrap_or(0);
        let etag = between(body, "<ETag>", "</ETag>")
            .unwrap_or_default()
            .replace("&quot;", "")
            .replace('"', "")
            .trim()
            .to_ascii_lowercase();
        if !key.is_empty() {
            entries.push(store::Listed { key, size, etag });
        }
        rest = &block[end + "</Contents>".len()..];
    }
    let truncated = between(xml, "<IsTruncated>", "</IsTruncated>").as_deref() == Some("true");
    let token = if truncated {
        between(xml, "<NextContinuationToken>", "</NextContinuationToken>")
    } else {
        None
    };
    (entries, token)
}

fn between(text: &str, open: &str, close: &str) -> Option<String> {
    let start = text.find(open)? + open.len();
    let rest = &text[start..];
    let end = rest.find(close)?;
    Some(rest[..end].to_string())
}

/// Run a command, returning its stdout and status; stderr is kept for the
/// error.
fn output(mut command: Command, what: &str) -> Result<(bool, String, String), String> {
    let out = command
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("{what}: {error}"))?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// The usage of `spawn backup cas` ([`recoverable`]), for main's help.
pub(crate) const CAS_USAGE: &str = recoverable::CAS_USAGE;

/// The usage of `spawn store` ([`store`]), for main's help.
pub(crate) const STORE_USAGE: &str = store::USAGE;

/// `spawn store put|plan|get|list|verify` (the off-host copy's layer,
/// [`store`]).
pub(crate) fn store_command(arguments: &[String]) -> Result<i32, String> {
    store::command(arguments).or_else(|error| {
        if is_credential_error(&error) {
            eprintln!("spawn store: {error}");
            Ok(CREDENTIAL_EXIT_CODE)
        } else {
            Err(error)
        }
    })
}

/// The entry point: `spawn backup dump|bundle|verify|restore-drill|cas [...]`.
pub(crate) fn command(arguments: &[String]) -> Result<i32, String> {
    let Some(verb) = arguments.first() else {
        return Err(USAGE.to_string());
    };
    if verb == "cas" {
        return recoverable::command(&arguments[1..]);
    }
    let rest: Vec<String> = arguments[1..].to_vec();
    let switches: &[&str] = if verb == "restore-drill" {
        &["keep"]
    } else {
        &[]
    };
    let flags =
        Flags::parse_with_switches(&rest, switches).map_err(|error| format!("{error}\n{USAGE}"))?;
    match verb.as_str() {
        "dump" => dump(&flags),
        "bundle" => bundle(&flags),
        "verify" => verify(&flags),
        "restore-drill" => restore_drill::restore_drill(&flags),
        other => return Err(format!("unknown backup command {other}\n{USAGE}")),
    }
    .or_else(|error| {
        if is_credential_error(&error) {
            eprintln!("spawn backup {verb}: {error}");
            Ok(CREDENTIAL_EXIT_CODE)
        } else {
            Err(error)
        }
    })
}

/// `spawn backup dump`: one encrypted copy of the log, now.
#[allow(clippy::too_many_lines)]
fn dump(flags: &Flags) -> Result<i32, String> {
    let started = Instant::now();
    let settings = match Settings::from_env(flags) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("spawn backup dump: {error}");
            record_refusal("dump", &error);
            return Ok(config_exit_code(&error));
        }
    };
    // Sealed with SPAWN_BACKUP_SEAL_KEY's key, never k1 in its place.
    let seal = match settings.seal.clone() {
        Ok(seal) => seal,
        Err(reason) => {
            eprintln!("spawn backup dump: {reason}");
            record_refusal("dump", &reason);
            return Ok(config_exit_code(&reason));
        }
    };
    let _lock = Lock::acquire(&settings.root)?;
    let store = open_store()?;
    let url = parse_pg_url(&database_url()?)?;
    let now_ms = store.now_ms().map_err(|error| error.to_string())?;
    let now = Stamp::at(now_ms);
    let previous = dump_records(&store, &settings.prefix, now_ms)?;
    let prior: Vec<(Stamp, i64, u64, bool)> = previous
        .iter()
        .filter_map(|entry| {
            let record = &entry.record;
            let key = record["keys"].as_array()?.first()?.as_str()?;
            let stamp = Stamp::parse(key)?;
            Some((
                stamp,
                record["events"].as_i64()?,
                record["bytes"].as_u64()?,
                record["suspect"].as_bool().unwrap_or(false),
            ))
        })
        .collect();
    let earlier: Vec<Stamp> = prior
        .iter()
        .filter(|(_, _, _, suspect)| !suspect)
        .map(|(stamp, _, _, _)| *stamp)
        .collect();
    let newest_good = prior
        .iter()
        .rfind(|(_, _, _, suspect)| !suspect)
        .map(|(_, events, bytes, _)| (*events, *bytes));

    let scratch = Scratch::new("backup-dump")?;
    let raw = scratch.join("dump");
    let dump_started = Instant::now();
    pg_dump(&settings, &url, &raw)?;
    let dump_ms = i64::try_from(dump_started.elapsed().as_millis()).unwrap_or(i64::MAX);
    let bytes = fs::metadata(&raw)
        .map_err(|error| format!("{}: {error}", raw.display()))?
        .len();
    let (events, max_event_id) = count_events(&settings, &scratch, &raw)?;
    let sha256 = sha256_file(&raw)?;

    let mut keys: Vec<String> = Vec::new();
    let mut tiers: Vec<String> = Vec::new();
    let mut skipped: Vec<Value> = Vec::new();
    let mut suspect_reason: Option<String> = None;
    let encrypted = scratch.join("dump.gpg");
    encrypt_with(&seal, &raw, &encrypted)?;
    let stored_bytes = fs::metadata(&encrypted)
        .map_err(|error| format!("{}: {error}", encrypted.display()))?
        .len();
    // The bytes leave the host encrypted, and decrypt back to what was
    // counted: a copy that does not is not a copy.
    let back = scratch.join("dump.back");
    decrypt_with(&seal, &encrypted, &back)?;
    if sha256_file(&back)? != sha256 {
        return Err("the encrypted copy does not decrypt to the dump".to_string());
    }
    drop(back);

    let held = list_bucket(&settings)?.0;
    let mut written = 0u64;
    let upload_started = Instant::now();
    // An intended drop (data deliberately removed) is accepted once by the
    // owner, as `pg-backup.sh --accept-drop` did: the record still names the
    // drop, under `accepted`, and the dump is stored as a good one.
    let dropped = suspect(events, bytes, newest_good);
    let accepted = dropped.is_some() && env_or("SPAWN_BACKUP_ACCEPT_DROP", "0") == "1";
    match dropped.clone().filter(|_| !accepted) {
        Some(reason) => {
            let key = settings.stored(&format!("postgres/suspect/spawn-{}.dump.gpg", now.text()));
            if within_limit(held, written, stored_bytes, settings.limit_bytes) {
                put_object(&settings, &key, &encrypted, &sha256, events, &seal.name)?;
                written = written.saturating_add(stored_bytes);
                keys.push(key);
            } else {
                skipped.push(skip("suspect", held, written, stored_bytes, &settings));
            }
            suspect_reason = Some(reason);
        }
        None => {
            for tier in tiers_for(now, &earlier) {
                let key = settings.stored(&tier.key(&now.text()));
                if !within_limit(held, written, stored_bytes, settings.limit_bytes) {
                    skipped.push(skip(tier.name(), held, written, stored_bytes, &settings));
                    continue;
                }
                put_object(&settings, &key, &encrypted, &sha256, events, &seal.name)?;
                written = written.saturating_add(stored_bytes);
                keys.push(key);
                tiers.push(tier.name().to_string());
            }
        }
    }
    let upload_ms = i64::try_from(upload_started.elapsed().as_millis()).unwrap_or(i64::MAX);
    let over_alert = held.saturating_add(written) > settings.alert_bytes;
    let mut record = json!({
        "type": "backup.dumped",
        "sealed_with": seal.name,
        "keys": keys,
        "tiers": tiers,
        "skipped": skipped,
        "sha256": sha256,
        "bytes": bytes,
        "stored_bytes": stored_bytes,
        "events": events,
        "max_event_id": max_event_id,
        "held_bytes": held,
        "written_bytes": written,
        "alert_bytes": settings.alert_bytes,
        "limit_bytes": settings.limit_bytes,
        "over_alert": over_alert,
        "dump_ms": dump_ms,
        "upload_ms": upload_ms,
        "duration_ms": i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
    });
    if accepted {
        eprintln!(
            "spawn backup dump: accepted by SPAWN_BACKUP_ACCEPT_DROP: {}",
            dropped.as_deref().unwrap_or_default()
        );
        suspect_reason = None;
    }
    if let Some(reason) = &suspect_reason {
        record["suspect"] = json!(reason);
    }
    if accepted && let Some(target) = record.as_object_mut() {
        target.insert(
            "accepted".to_string(),
            dropped.clone().map_or(Value::Null, Value::String),
        );
    }
    store
        .backup_record(&record)
        .map_err(|error| format!("backup.dumped: {error}"))?;
    if over_alert {
        println!(
            "R2 holds {} bytes, over the alert at {}",
            held.saturating_add(written),
            settings.alert_bytes
        );
    }
    println!(
        "spawn backup dump: {events} events (max id {max_event_id}), {bytes} bytes, {} stored",
        if keys.is_empty() {
            "nothing".to_string()
        } else {
            keys.join(", ")
        }
    );
    if suspect_reason.is_some() {
        return Ok(3);
    }
    if !keys
        .iter()
        .any(|key| key.starts_with(&settings.stored("postgres/hourly/")))
    {
        return Ok(4);
    }
    Ok(0)
}

/// Why a copy was not written.
fn skip(tier: &str, held: u64, written: u64, copy: u64, settings: &Settings) -> Value {
    json!({
        "tier": tier,
        "reason": format!(
            "the limit: {} held, {} written by this run and {} for this copy pass {} bytes",
            held, written, copy, settings.limit_bytes
        ),
    })
}

/// Record a refusal with its reason, so the failure is a record and not an
/// absence (best effort: the store may be what is broken). A refused dump is
/// recorded as one that stored nothing, its hourly tier skipped for the
/// reason; a refused bundle likewise.
fn record_refusal(verb: &str, reason: &str) {
    let Ok(store) = open_store() else {
        return;
    };
    let record = if verb == "bundle" {
        json!({
            "type": "backup.bundled", "kind": "incremental", "base_event_id": null,
            "keys": [], "tiers": [], "skipped": [{"tier": "incr", "reason": reason}],
            "sha256": "", "bytes": 0, "stored_bytes": 0, "repos": [], "missing": [],
            "transcripts": {"files": 0, "bytes": 0, "sizes": {}},
            "held_bytes": 0, "written_bytes": 0,
            "alert_bytes": 0, "limit_bytes": 0, "over_alert": false, "duration_ms": 0,
        })
    } else {
        json!({
            "type": "backup.dumped", "keys": [], "tiers": [],
            "skipped": [{"tier": "hourly", "reason": reason}],
            "sha256": "", "bytes": 0, "stored_bytes": 0, "events": 0, "max_event_id": 0,
            "held_bytes": 0, "written_bytes": 0, "alert_bytes": 0, "limit_bytes": 0,
            "over_alert": false, "dump_ms": 0, "upload_ms": 0,
        })
    };
    let _ = store.backup_record(&record);
}

/// Whether `record` (a `backup.dumped` or `backup.bundled`) is this run's
/// to read: every key it names, with this run's `SPAWN_BACKUP_PREFIX`
/// stripped, is in the backup namespace (`postgres/` or `bundles/`). A
/// record from another prefix is not — a measurement's `locktest/...` never
/// becomes the fleet's bundle base or the drill's subject, and the fleet's
/// records never chain onto one. With no prefix, a record whose keys are
/// not in the namespace is another prefix's and is left out.
fn our_record(prefix: &str, record: &Value) -> bool {
    let Some(keys) = record["keys"].as_array() else {
        return false;
    };
    !keys.is_empty()
        && keys.iter().all(|key| {
            key.as_str()
                .and_then(|key| key.strip_prefix(prefix))
                .is_some_and(|key| key.starts_with("postgres/") || key.starts_with("bundles/"))
        })
}

/// The earlier `backup.dumped` records, oldest first, with their event ids.
struct DumpRecord {
    event_id: i64,
    record: Value,
}

fn dump_records(
    store: &PostgresStore,
    prefix: &str,
    now_ms: i64,
) -> Result<Vec<DumpRecord>, String> {
    let records = store
        .backup_records(
            "backup.dumped",
            now_ms.saturating_sub(400 * 86_400_000),
            10_000,
        )
        .map_err(|error| error.to_string())?;
    Ok(read_dump_records(prefix, &records))
}

/// The store's answer as this run reads it: its own prefix's records only,
/// oldest first. Excludes `{event_id, record}` rows of other prefixes.
fn read_dump_records(prefix: &str, records: &Value) -> Vec<DumpRecord> {
    records
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| our_record(prefix, &entry["record"]))
        .map(|entry| DumpRecord {
            event_id: entry["event_id"].as_i64().unwrap_or_default(),
            record: entry["record"].clone(),
        })
        .collect()
}

/// `pg_dump -Fc` of the database, its password in the environment.
fn pg_dump(settings: &Settings, url: &PgUrl, out: &Path) -> Result<(), String> {
    let mut command = Command::new(settings.pg_tool("pg_dump"));
    command.arg("-Fc").arg("--no-owner").arg("--no-acl");
    if !settings.compress.trim().is_empty() {
        command.arg("-Z").arg(&settings.compress);
    }
    command
        .arg("-d")
        .arg(if url.database.is_empty() {
            "postgres"
        } else {
            &url.database
        })
        .arg("-f")
        .arg(out);
    for (name, value) in url.env() {
        command.env(name, value);
    }
    let (ok, _stdout, stderr) = output(command, "pg_dump")?;
    if !ok {
        return Err(format!("pg_dump: {}", stderr.trim()));
    }
    Ok(())
}

/// The events and highest id of the dump's `spawn_events`, counted in the
/// archive itself: `pg_restore -a -t spawn_events -f -`.
fn count_events(settings: &Settings, scratch: &Scratch, dump: &Path) -> Result<(i64, i64), String> {
    let mut command = Command::new(settings.pg_tool("pg_restore"));
    command
        .arg("-a")
        .arg("-t")
        .arg("spawn_events")
        .arg("-f")
        .arg(scratch.join("events.sql"))
        .arg(dump);
    let (ok, _stdout, stderr) = output(command, "pg_restore")?;
    if !ok {
        return Err(format!(
            "pg_restore (to count the archive): {}",
            stderr.trim()
        ));
    }
    let text = fs::read_to_string(scratch.join("events.sql"))
        .map_err(|error| format!("the archive's events: {error}"))?;
    Ok(copy_events(&text))
}

/// gpg: AES256, no compression (the archive is compressed), the passphrase
/// from its file.
fn gpg(settings: &Settings, decrypting: bool, input: &Path, out: &Path) -> Result<(), String> {
    gpg_with(&settings.key_file, decrypting, "none", input, out)
}

/// gpg with the backup key; `compress` is its --compress-algo when it
/// encrypts (the dumps and bundles are compressed before: `none`).
fn gpg_with(
    key_file: &Path,
    decrypting: bool,
    compress: &str,
    input: &Path,
    out: &Path,
) -> Result<(), String> {
    let mut command = Command::new("gpg");
    command
        .args([
            "--batch",
            "--yes",
            "--quiet",
            "--no-tty",
            "--pinentry-mode",
            "loopback",
            "--no-symkey-cache",
        ])
        .arg("--passphrase-file")
        .arg(key_file);
    if decrypting {
        command.arg("--output").arg(out).arg("--decrypt").arg(input);
    } else {
        command
            .args([
                "--symmetric",
                "--cipher-algo",
                "AES256",
                "--compress-algo",
                compress,
            ])
            .arg("--output")
            .arg(out)
            .arg(input);
    }
    let (ok, _stdout, stderr) = output(command, "gpg")?;
    if !ok {
        return Err(format!("gpg: {}", stderr.trim()));
    }
    Ok(())
}

/// An object opened with the store's key file (`spawn store`, which seals
/// with the sealing key's file and reads with it until objective 116).
fn decrypt(settings: &Settings, input: &Path, out: &Path) -> Result<(), String> {
    gpg(settings, true, input, out)
}

/// A dump or a bundle sealed with `key` (objective 114).
fn encrypt_with(key: &Key, input: &Path, out: &Path) -> Result<(), String> {
    gpg_with(&key.file, false, "none", input, out)
}

/// An object opened with `key`, the one its record names.
fn decrypt_with(key: &Key, input: &Path, out: &Path) -> Result<(), String> {
    gpg_with(&key.file, true, "none", input, out)
}

/// A store object sealed: compressed (zlib) and encrypted with the backup
/// key, as one gpg step (`spawn backup store`).
fn seal(settings: &Settings, input: &Path, out: &Path) -> Result<(), String> {
    gpg_with(&settings.key_file, false, "zlib", input, out)
}

/// One exchange with the bucket. `options` is curl config (headers, the
/// file to upload); the credentials are added here, on stdin, never argv.
struct Response {
    status: u16,
    body: Vec<u8>,
    headers: String,
}

/// The payload hash every request names: R2 refuses an s3 request without
/// `x-amz-content-sha256`, and a curl older than 8.x (7.88, Debian 12's)
/// does not add it itself; a curl that does takes this one. Unsigned: an
/// upload is streamed, never read twice to hash it, and R2 and S3 accept it
/// for GET, HEAD, ListObjectsV2 and PUT (a conditional one included).
const PAYLOAD_HASH_HEADER: &str = "x-amz-content-sha256: UNSIGNED-PAYLOAD";

/// The whole curl config of one request, as curl reads it on stdin: the
/// credentials and SigV4, the payload hash next to it (before the call's
/// own headers), then the URL, the files and `options`.
fn curl_config(
    settings: &Settings,
    url: &str,
    options: &str,
    body_file: &Path,
    headers_file: &Path,
) -> String {
    format!(
        "silent\nshow-error\nuser = \"{}:{}\"\naws-sigv4 = \"aws:amz:auto:s3\"\n\
header = \"{PAYLOAD_HASH_HEADER}\"\nmax-time = \"600\"\nconnect-timeout = \"20\"\n\
url = \"{}\"\noutput = \"{}\"\ndump-header = \"{}\"\nwrite-out = \"%{{http_code}}\"\n{}",
        escape_config(&settings.r2.access_key),
        escape_config(&settings.r2.secret_key),
        escape_config(url),
        body_file.display(),
        headers_file.display(),
        options
    )
}

fn bucket_request(settings: &Settings, url: &str, options: &str) -> Result<Response, String> {
    let scratch = Scratch::new("curl")?;
    let body_file = scratch.join("body");
    let headers_file = scratch.join("headers");
    let status = run_curl(settings, url, options, &body_file, &headers_file)?;
    Ok(Response {
        status,
        body: fs::read(&body_file).unwrap_or_default(),
        headers: fs::read_to_string(&headers_file).unwrap_or_default(),
    })
}

/// One curl run for `url` with the signed config on its stdin (never on its
/// command line), the body to `body_file` and the headers to `headers_file`:
/// the HTTP status, or curl's own error (its exit and message).
fn run_curl(
    settings: &Settings,
    url: &str,
    options: &str,
    body_file: &Path,
    headers_file: &Path,
) -> Result<u16, String> {
    let secret = curl_config(settings, url, options, body_file, headers_file);
    let mut command = Command::new("curl");
    command
        .arg("--config")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| format!("curl: {error}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(secret.as_bytes())
            .map_err(|error| format!("curl: {error}"))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|error| format!("curl: {error}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "curl: exit {}: {}",
            out.status.code().unwrap_or(-1),
            stderr.trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .trim()
        .lines()
        .last()
        .and_then(|line| line.trim().parse::<u16>().ok())
        .ok_or_else(|| format!("curl: no status in its output ({})", stdout.trim()))
}

fn escape_config(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

/// A response header's value, lower-cased name, trimmed.
fn header_value(headers: &str, name: &str) -> Option<String> {
    headers.lines().find_map(|line| {
        let (header, value) = line.split_once(':')?;
        (header.trim().eq_ignore_ascii_case(name)).then(|| value.trim().to_string())
    })
}

/// The whole bucket: every key with its size (every listing page), and the
/// bytes they add up to.
fn list_bucket(settings: &Settings) -> Result<(u64, Vec<(String, u64)>), String> {
    let mut total = 0u64;
    let mut keys = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..10_000 {
        let url = settings.r2.url(&listing_query(None, token.as_deref()));
        let response = bucket_request(settings, &url, "")?;
        if response.status != 200 {
            return Err(format!(
                "ListObjectsV2 answered {}: {}",
                response.status,
                first_line(&String::from_utf8_lossy(&response.body))
            ));
        }
        let text = String::from_utf8_lossy(&response.body).into_owned();
        let (page, next) = parse_listing(&text);
        for (key, size) in &page {
            total = total.saturating_add(*size);
            keys.push((key.clone(), *size));
        }
        match next {
            Some(next) => token = Some(next),
            None => return Ok((total, keys)),
        }
    }
    Err("ListObjectsV2 did not end".to_string())
}

/// A ListObjectsV2 query, its parameters sorted by name as SigV4 signs
/// them (a curl that signs the query as written, 7.88, signs what the
/// bucket checks only so): the token on a later page only, the prefix when
/// there is one, each value percent-encoded.
fn listing_query(prefix: Option<&str>, token: Option<&str>) -> String {
    let mut query = String::from("?");
    if let Some(token) = token {
        let _ = write!(query, "continuation-token={}&", percent_encode(token));
    }
    query.push_str("list-type=2&max-keys=1000");
    if let Some(prefix) = prefix {
        let _ = write!(query, "&prefix={}", percent_encode(prefix));
    }
    query
}

fn first_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(160)
        .collect()
}

fn percent_encode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        let c = byte as char;
        if c.is_ascii_alphanumeric() || "-_.~".contains(c) {
            out.push(c);
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// Store one object, its ETag checked against the MD5 sent and its metadata
/// naming the plaintext's sha256 and event count.
/// The tar as it is stored: compressed with zstd when the host has it,
/// else gzip, else as it is; and the extension that says which.
fn compress_bundle(scratch: &Path, tar: &Path) -> Result<(PathBuf, &'static str), String> {
    for (program, arguments, extension) in [
        ("zstd", vec!["-q", "-3", "-f", "-o"], "tar.zst"),
        ("gzip", vec!["-6", "-c", "-n"], "tar.gz"),
    ] {
        let out = scratch.join(extension);
        let mut command = Command::new(program);
        if program == "gzip" {
            // gzip writes to stdout.
            let file = File::create(&out).map_err(|error| format!("{}: {error}", out.display()))?;
            command.args(&arguments).arg(tar).stdout(Stdio::from(file));
        } else {
            command.args(&arguments).arg(&out).arg(tar);
        }
        match command.output() {
            Ok(run) if run.status.success() => return Ok((out, extension)),
            // Not installed: the next one.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(run) => {
                return Err(format!(
                    "{program}: {}",
                    String::from_utf8_lossy(&run.stderr).trim()
                ));
            }
            Err(error) => return Err(format!("{program}: {error}")),
        }
    }
    let plain = scratch.join("tar");
    fs::copy(tar, &plain).map_err(|error| format!("{}: {error}", plain.display()))?;
    Ok((plain, "tar"))
}

/// What the first bytes of a stored bundle say it is: `gzip`, `zstd`, or
/// `plain` (the tar itself). The key's extension says the same; a restore
/// trusts the magic.
fn bundle_compression(magic: &[u8]) -> &'static str {
    match magic {
        [0x1f, 0x8b, ..] => "gzip",
        [0x28, 0xb5, 0x2f, 0xfd, ..] => "zstd",
        _ => "plain",
    }
}

/// `archive` decompressed back to the tar it was made from, by its magic:
/// what `compress_bundle` did, undone.
fn decompress_bundle(scratch: &Path, archive: &Path) -> Result<(PathBuf, &'static str), String> {
    let mut magic = Vec::with_capacity(4);
    let file = File::open(archive).map_err(|error| format!("{}: {error}", archive.display()))?;
    file.take(4)
        .read_to_end(&mut magic)
        .map_err(|error| format!("{}: {error}", archive.display()))?;
    let compression = bundle_compression(&magic);
    let tar = scratch.join("bundle.tar");
    let program = match compression {
        "gzip" => Some(("gzip", vec!["-dc"])),
        "zstd" => Some(("zstd", vec!["-q", "-dc"])),
        _ => None,
    };
    let Some((program, arguments)) = program else {
        fs::copy(archive, &tar).map_err(|error| format!("{}: {error}", tar.display()))?;
        return Ok((tar, compression));
    };
    let out = File::create(&tar).map_err(|error| format!("{}: {error}", tar.display()))?;
    let run = Command::new(program)
        .args(&arguments)
        .arg(archive)
        .stdout(Stdio::from(out))
        .output()
        .map_err(|error| format!("{program}: {error}"))?;
    if !run.status.success() {
        return Err(format!(
            "{program}: {}",
            String::from_utf8_lossy(&run.stderr).trim()
        ));
    }
    Ok((tar, compression))
}

fn put_object(
    settings: &Settings,
    key: &str,
    file: &Path,
    sha256: &str,
    events: i64,
    sealed_with: &str,
) -> Result<(), String> {
    put_object_with(settings, key, file, sha256, (events, sealed_with), "")
}

/// The scratch directories of the bundle in the workspace root, when it
/// takes the notes folder: `<root>/.spawn-backup-bundle-<pid>-<random>`.
const BUNDLE_SCRATCH: &str = ".spawn-backup-bundle";

/// Remove the bundle scratch directories a killed run left in `root`
/// (SIGKILL runs no cleanup). Called under the backup lock, so no other
/// run is using one.
fn sweep_bundle_scratch(root: &Path) {
    let Ok(listing) = fs::read_dir(root) else {
        return;
    };
    for entry in listing.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(&format!("{BUNDLE_SCRATCH}-"))
            && entry.file_type().is_ok_and(|kind| kind.is_dir())
        {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// A bundle's PUT may take 600 s plus its size at 100 KB/s (objective 216):
/// a backstop sized to the object, not a speed relied on. Given as the last
/// `max-time` of its curl config, which curl takes.
pub(crate) fn bundle_put_seconds(bytes: u64) -> u64 {
    600 + bytes / 100_000
}

/// [`put_object`] with `extra` appended to its curl config (`events` and
/// the sealing key's name as its metadata).
fn put_object_with(
    settings: &Settings,
    key: &str,
    file: &Path,
    sha256: &str,
    (events, sealed_with): (i64, &str),
    extra: &str,
) -> Result<(), String> {
    // Again at the PUT itself, before any request (settings are checked
    // when read; this holds whatever built them).
    not_locked_forever(&settings.r2.bucket)?;
    let md5 = md5_file(file)?;
    let options = format!(
        "upload-file = \"{}\"\nheader = \"Content-Type: application/octet-stream\"\n\
header = \"x-amz-meta-sha256: {}\"\nheader = \"x-amz-meta-spawn-events: {}\"\n\
header = \"x-amz-meta-spawn-seal: {}\"\n{extra}",
        escape_config(&file.display().to_string()),
        sha256,
        events,
        sealed_with
    );
    let response = bucket_request(settings, &settings.r2.url(key), &options)?;
    if response.status != 200 {
        return Err(format!(
            "PUT {key} answered {}: {}",
            response.status,
            first_line(&String::from_utf8_lossy(&response.body))
        ));
    }
    let etag = header_value(&response.headers, "etag").unwrap_or_default();
    let etag = etag.trim_matches('"').to_ascii_lowercase();
    if etag != md5 {
        return Err(format!(
            "PUT {key}: the ETag {etag:?} is not the MD5 sent {md5:?}"
        ));
    }
    Ok(())
}

/// Write `file` at `key` only when no object is there (`If-None-Match: *`:
/// the bucket answers 412 to a key that exists), with `sha256` (the sealed
/// file's, never a plaintext's) as metadata (`spawn backup store`). Never an
/// overwrite.
fn put_new_object(
    settings: &Settings,
    key: &str,
    file: &Path,
    sha256: &str,
) -> Result<store::Put, String> {
    let options = put_new_options(settings, file, sha256);
    let response = bucket_request(settings, &settings.r2.url(key), &options)?;
    match response.status {
        200 => Ok(store::Put::Written),
        412 => Ok(store::Put::Existed),
        status => Err(format!(
            "PUT {key} answered {status}: {}",
            first_line(&String::from_utf8_lossy(&response.body))
        )),
    }
}

/// A conditional PUT's own curl config: the file and its headers.
fn put_new_options(settings: &Settings, file: &Path, sha256: &str) -> String {
    format!(
        "upload-file = \"{}\"\nheader = \"Content-Type: application/octet-stream\"\n\
header = \"If-None-Match: *\"\nheader = \"x-amz-meta-sha256: {}\"\n{}",
        escape_config(&file.display().to_string()),
        sha256,
        // The sealing key's name, the key the store's seal is made with: an
        // object without it reads as k1's everywhere.
        settings.seal.as_ref().map_or_else(
            |_| String::new(),
            |key| format!("header = \"x-amz-meta-spawn-seal: {}\"\n", key.name)
        )
    )
}

/// An object's length, ETag (lower case, unquoted) and sha256 metadata, as
/// HEAD reads them; `None` when there is no such key.
fn head_object(settings: &Settings, key: &str) -> Result<Option<store::Head>, String> {
    let response = bucket_request(settings, &settings.r2.url(key), "head\n")?;
    match response.status {
        200 => Ok(Some(store::Head {
            length: header_value(&response.headers, "content-length")
                .and_then(|text| text.parse().ok())
                .unwrap_or(0),
            etag: header_value(&response.headers, "etag")
                .unwrap_or_default()
                .trim_matches('"')
                .to_ascii_lowercase(),
            sha256: header_value(&response.headers, "x-amz-meta-sha256").unwrap_or_default(),
        })),
        404 => Ok(None),
        status => Err(format!("HEAD {key} answered {status}")),
    }
}

/// Every key under `prefix`, with its size and ETag (every listing page).
fn list_prefix(settings: &Settings, prefix: &str) -> Result<Vec<store::Listed>, String> {
    let mut keys = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..100_000 {
        let query = listing_query(Some(prefix), token.as_deref());
        let response = bucket_request(settings, &settings.r2.url(&query), "")?;
        if response.status != 200 {
            return Err(format!(
                "ListObjectsV2 {prefix} answered {}: {}",
                response.status,
                first_line(&String::from_utf8_lossy(&response.body))
            ));
        }
        let (page, next) = parse_listing_tagged(&String::from_utf8_lossy(&response.body));
        keys.extend(
            page.into_iter()
                .filter(|listed| listed.key.starts_with(prefix)),
        );
        match next {
            Some(next) => token = Some(next),
            None => return Ok(keys),
        }
    }
    Err("ListObjectsV2 did not end".to_string())
}

/// `spawn backup dump`'s body is above; `bundle` and `verify` follow.
///
/// One `backup.bundled` record, as the run reads the earlier ones.
struct BundleRecord {
    event_id: i64,
    at_ms: i64,
    record: Value,
}

fn bundle_records(
    store: &PostgresStore,
    prefix: &str,
    now_ms: i64,
) -> Result<Vec<BundleRecord>, String> {
    let records = store
        .backup_records(
            "backup.bundled",
            now_ms.saturating_sub(400 * 86_400_000),
            10_000,
        )
        .map_err(|error| error.to_string())?;
    Ok(read_bundle_records(prefix, &records))
}

/// The store's answer as this run reads it: its own prefix's records only,
/// oldest first — so a measurement's full bundle under `SPAWN_BACKUP_PREFIX`
/// is never the fleet's base, and the fleet's never a measurement's.
fn read_bundle_records(prefix: &str, records: &Value) -> Vec<BundleRecord> {
    records
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| our_record(prefix, &entry["record"]))
        .map(|entry| BundleRecord {
            event_id: entry["event_id"].as_i64().unwrap_or_default(),
            at_ms: entry["at_ms"].as_i64().unwrap_or_default(),
            record: entry["record"].clone(),
        })
        .collect()
}

/// The earlier bundles a new bundle sealed with `seal` builds on: those
/// sealed with it (objective 114; a record without `sealed_with` is k1's, a
/// record whose name is refused is no one's). With none yet, the bundle is
/// full and has no base, whatever bundle of another key this week holds.
/// Here and not in [`bundle_records`], which verify and the drill share.
fn sealed_alike(earlier: Vec<BundleRecord>, seal: &str) -> Vec<BundleRecord> {
    earlier
        .into_iter()
        .filter(|entry| {
            record_key("backup.bundled", entry.event_id, &entry.record)
                .is_ok_and(|name| name == seal)
        })
        .collect()
}

/// The earlier bundles as the full/increment decision reads them: when each
/// ran, whether it was full, and its week's stamp from its own key.
fn bundle_stamps(earlier: &[BundleRecord]) -> Vec<(Stamp, bool, i64)> {
    earlier
        .iter()
        .filter_map(|entry| {
            let key = entry.record["keys"].as_array()?.first()?.as_str()?;
            let kind = entry.record["kind"].as_str()?;
            Some((Stamp::parse(key)?, kind == "full", entry.at_ms))
        })
        .collect()
}

/// The files of the transcript archive modified within `days` days, relative
/// to the archive root, with their sizes, sorted.
fn window_files(archive: &Path, now_ms: i64, days: i64) -> Result<Vec<(String, u64)>, String> {
    let cutoff = now_ms.saturating_sub(days.saturating_mul(86_400_000));
    let mut files = Vec::new();
    let mut stack = vec![archive.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("{}: {error}", directory.display())),
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            let Ok(modified) = meta.modified() else {
                continue;
            };
            let at_ms = modified
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| {
                    i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
                });
            if at_ms < cutoff {
                continue;
            }
            let Ok(relative) = path.strip_prefix(archive) else {
                continue;
            };
            files.push((
                relative.display().to_string().replace('\\', "/"),
                meta.len(),
            ));
        }
    }
    files.sort();
    Ok(files)
}

/// A source's refs and their commits (`refs/heads` and `refs/tags`).
fn heads_of(source: &str) -> Result<BTreeMap<String, String>, String> {
    let (ok, stdout, stderr) = output(
        {
            let mut command = crate::sources::controller_git();
            command.args([
                "-C",
                source,
                "for-each-ref",
                "--format=%(objectname) %(refname)",
                "refs/heads",
                "refs/tags",
            ]);
            command
        },
        "git for-each-ref",
    )?;
    if !ok {
        return Err(stderr.trim().to_string());
    }
    let mut heads = BTreeMap::new();
    for line in stdout.lines() {
        if let Some((commit, name)) = line.split_once(' ') {
            heads.insert(name.to_string(), commit.to_string());
        }
    }
    Ok(heads)
}

/// Whether the source still holds a commit (`git cat-file -e`).
fn holds(source: &str, commit: &str) -> bool {
    crate::sources::controller_git()
        .args(["-C", source, "cat-file", "-e", commit])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// A file name for a source: its last path component, safe for a tar.
fn source_name(source: &str, taken: &mut BTreeSet<String>, index: usize) -> String {
    let base = source
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("source")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    let base = if base.is_empty() {
        format!("source-{index}")
    } else {
        base
    };
    let mut name = base.clone();
    let mut suffix = 1;
    while !taken.insert(name.clone()) {
        name = format!("{base}-{suffix}");
        suffix += 1;
    }
    name
}

/// `spawn backup bundle`: the mission sources and the transcript archive, a
/// full bundle or an increment of its base.
#[allow(clippy::too_many_lines)]
fn bundle(flags: &Flags) -> Result<i32, String> {
    let started = Instant::now();
    let settings = match Settings::from_env(flags) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("spawn backup bundle: {error}");
            record_refusal("bundle", &error);
            return Ok(config_exit_code(&error));
        }
    };
    let seal = match settings.seal.clone() {
        Ok(seal) => seal,
        Err(reason) => {
            eprintln!("spawn backup bundle: {reason}");
            record_refusal("bundle", &reason);
            return Ok(config_exit_code(&reason));
        }
    };
    let _lock = Lock::acquire(&settings.root)?;
    let store = open_store()?;
    let now_ms = store.now_ms().map_err(|error| error.to_string())?;
    let now = Stamp::at(now_ms);
    let earlier = sealed_alike(
        bundle_records(&store, &settings.prefix, now_ms)?,
        &seal.name,
    );
    let base = earlier.last();
    let stamps = bundle_stamps(&earlier);
    let full = full_bundle(now, &stamps, now_ms);
    let sources = store
        .mission_sources()
        .map_err(|error| error.to_string())?
        .as_array()
        .cloned()
        .unwrap_or_default();
    let archive = settings.root.join(".transcripts");
    let window = window_files(&archive, now_ms, settings.transcript_days)?;
    // With the notes folder (objective 216) the scratch (its staged files,
    // the tar and its compressed, encrypted and decrypted-back copies) is on
    // the workspace's disk beside the lock, never in TMPDIR, which may be
    // memory; a scratch a killed run left there goes first (the lock says no
    // other run is using one). Unset, it stays where it was.
    let sync_dir = env_value("SPAWN_BACKUP_SYNC_DIR").filter(|dir| !dir.trim().is_empty());
    let scratch = if sync_dir.is_some() {
        sweep_bundle_scratch(&settings.root);
        Scratch::new_in(&settings.root, BUNDLE_SCRATCH)?
    } else {
        Scratch::new("backup-bundle")?
    };
    let staging = scratch.join("stage");
    fs::create_dir_all(staging.join("repos"))
        .map_err(|error| format!("{}: {error}", staging.display()))?;
    fs::create_dir_all(staging.join("transcripts"))
        .map_err(|error| format!("{}: {error}", staging.display()))?;

    let mut transcript_entries: Vec<Value> = Vec::new();
    let mut transp_bytes = 0u64;
    let mut sizes = serde_json::Map::new();
    // Each file's sha256 as bundled (objective 149): of its first `size`
    // bytes, unchanged files too, so the newest record alone names every
    // file a restore must give back, and a restore drill judges it.
    let mut hashes = serde_json::Map::new();
    for (path, size) in &window {
        sizes.insert(path.clone(), json!(size));
        hashes.insert(
            path.clone(),
            json!(restore_drill::sha256_prefix(&archive.join(path), *size)?),
        );
        let base_size = base
            .and_then(|base| base.record["transcripts"]["sizes"].get(path))
            .and_then(Value::as_u64);
        let offset = if full {
            0
        } else {
            transcript_offset(base_size, *size)
        };
        if !full && offset >= *size {
            continue;
        }
        let target = staging.join("transcripts").join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
        }
        append_slice(&archive.join(path), &target, offset, *size)?;
        transcript_entries.push(json!({
            "path": path, "offset": offset, "bytes": size - offset, "size": size,
        }));
        transp_bytes = transp_bytes.saturating_add(size - offset);
    }

    let mut repos_record: Vec<Value> = Vec::new();
    let mut repos_manifest: Vec<Value> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    let mut taken = BTreeSet::new();
    for (index, source) in sources.iter().enumerate() {
        let Some(source) = source.as_str() else {
            continue;
        };
        let prepared = recoverable::prepare_source_for_bundle(&store, &settings.root, source)?;
        let git_dir = prepared.git_dir();
        let heads = match heads_of(git_dir) {
            Ok(heads) => heads,
            Err(error) => {
                if prepared.is_d6() {
                    return Err(format!("read exported D6 refs for {source}: {error}"));
                }
                missing.push(source.to_string());
                continue;
            }
        };
        // A source with no branches and no tags has nothing of the mission in
        // the archive, and `git bundle create` refuses to make an empty one:
        // the record says so by name (`empty`) and the manifest names no file,
        // so nothing is left to a git error, and a restore of it makes an
        // empty repository.
        if heads.is_empty() {
            repos_record.push(json!({
                "source": source, "file": Value::Null, "heads": {}, "refs": 0, "bytes": 0,
                "unchanged": false, "empty": true,
            }));
            repos_manifest.push(json!({
                "source": source, "file": Value::Null, "heads": {},
                "unchanged": false, "empty": true,
            }));
            continue;
        }
        let refs = heads.len();
        let base_repos = base
            .and_then(|base| base.record["repos"].as_array().cloned())
            .unwrap_or_default();
        let base_heads = base_repos
            .iter()
            .find(|repo| repo["source"].as_str() == Some(source))
            .and_then(|repo| repo["heads"].as_object().cloned())
            .unwrap_or_default();
        let not: Vec<String> = if full {
            Vec::new()
        } else {
            base_heads
                .values()
                .filter_map(Value::as_str)
                .filter(|commit| holds(git_dir, commit))
                .map(str::to_string)
                .collect()
        };
        // What changed against the base, by ref name: `added` (a ref the
        // base did not have), `moved` (one both have at different commits)
        // and `removed` (one the base had). An increment's record says it, so
        // a reader need not diff two records to see what the day did; its
        // `bundled` list says which of them the archive actually holds.
        let delta = json!({
            "added": heads
                .keys()
                .filter(|name| !base_heads.contains_key(name.as_str()))
                .cloned()
                .collect::<Vec<String>>(),
            "moved": heads
                .iter()
                .filter(|(name, commit)| {
                    base_heads
                        .get(name.as_str())
                        .and_then(Value::as_str)
                        .is_some_and(|held| held != commit.as_str())
                })
                .map(|(name, _)| name.clone())
                .collect::<Vec<String>>(),
            "removed": base_heads
                .keys()
                .filter(|name| !heads.contains_key(name.as_str()))
                .cloned()
                .collect::<Vec<String>>(),
        });
        let unchanged = !full
            && base.is_some()
            && base_heads.len() == heads.len()
            && heads.iter().all(|(name, commit)| {
                base_heads.get(name).and_then(Value::as_str) == Some(commit.as_str())
            });
        if unchanged {
            repos_record.push(json!({
                "source": source, "file": Value::Null, "heads": heads, "refs": refs,
                "bytes": 0, "unchanged": true,
            }));
            repos_manifest.push(json!({
                "source": source, "file": Value::Null, "heads": heads, "unchanged": true,
            }));
            continue;
        }
        // An increment whose refs all point into the base — a tag added at a
        // commit the base already holds, a branch removed — would write no
        // bytes, and `git bundle create` refuses to make an empty one. The
        // run says it by name instead: the entry names the refs that changed
        // (the delta) and no file, so a reader can tell "the source moved in
        // a way that needs no bytes" from "the bundle failed".
        if !full && !bundle_would_write(git_dir, &not)? {
            repos_record.push(json!({
                "source": source, "file": Value::Null, "heads": heads, "refs": refs,
                "bytes": 0, "unchanged": false, "delta": delta, "bundled": [],
            }));
            repos_manifest.push(json!({
                "source": source, "file": Value::Null, "heads": heads, "unchanged": false,
                "delta": delta, "bundled": [],
            }));
            continue;
        }
        let name = source_name(source, &mut taken, index);
        let file = scratch.join(format!("{name}.bundle"));
        git_bundle(git_dir, &file, &not)?;
        // What the bundle actually holds. An increment says it by name:
        // `--not` leaves out a ref whose tip the base has, so the manifest's
        // `heads` is not the same as its ref list, and the drill compares the
        // bundle with this rather than inferring it from the base.
        let bundled: Vec<Value> = if full {
            Vec::new()
        } else {
            bundle_refs(&file)?
                .into_iter()
                .map(|(commit, name)| json!({"commit": commit, "ref": name}))
                .collect()
        };
        let bytes = fs::metadata(&file)
            .map_err(|error| format!("{}: {error}", file.display()))?
            .len();
        let relative = format!("repos/{name}.bundle");
        fs::copy(&file, staging.join(&relative))
            .map_err(|error| format!("{}: {error}", file.display()))?;
        // A full bundle holds every head, so its entry needs no `bundled`;
        // an increment's holds a subset and says which.
        if full {
            repos_record.push(json!({
                "source": source, "file": relative, "heads": heads, "refs": refs,
                "bytes": bytes, "unchanged": false,
            }));
            repos_manifest.push(json!({
                "source": source, "file": relative, "heads": heads, "unchanged": false,
            }));
        } else {
            repos_record.push(json!({
                "source": source, "file": relative, "heads": heads, "refs": refs,
                "bytes": bytes, "unchanged": false, "delta": delta, "bundled": bundled,
            }));
            repos_manifest.push(json!({
                "source": source, "file": relative, "heads": heads, "unchanged": false,
                "delta": delta, "bundled": bundled,
            }));
        }
    }

    let sync = sync_dir.as_deref().map(|dir| {
        let scan = env_value("SPAWN_BACKUP_SYNC_SCAN")
            .filter(|tool| !tool.trim().is_empty())
            .unwrap_or_else(|| sync_dir::DEFAULT_SCAN.to_string());
        sync_dir::take(&sync_dir::Run {
            dir: Path::new(dir),
            stage: &staging,
            full,
            base: base.map(|base| &base.record),
            root: &settings.root,
            scan: &scan,
        })
    });
    let mut manifest = json!({
        "kind": if full { "full" } else { "incremental" },
        "base_event_id": base.map(|base| base.event_id),
        "transcripts": transcript_entries,
        "repos": repos_manifest,
        "created_at_ms": now_ms,
    });
    if let Some(sync) = &sync {
        manifest["sync"] = sync.manifest.clone();
    }
    fs::write(
        staging.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("manifest.json: {error}"))?;
    let tar = scratch.join("bundle.tar");
    let (ok, _stdout, stderr) = output(
        {
            let mut command = Command::new("tar");
            command
                .arg("-C")
                .arg(&staging)
                .arg("-cf")
                .arg(&tar)
                .arg(".");
            command
        },
        "tar",
    )?;
    if !ok {
        return Err(format!("tar: {}", stderr.trim()));
    }
    // A tar of transcripts is mostly text: zstd (fast, and what the drill's
    // other copies use) or gzip when there is no zstd; plain when neither.
    // The stored key names what it holds, and a restore sniffs the magic
    // anyway (objective 031's contract: plain, gzip or zstd).
    let (archive, extension) = compress_bundle(&scratch.join(""), &tar)?;
    let bytes = fs::metadata(&archive)
        .map_err(|error| format!("{}: {error}", archive.display()))?
        .len();
    let sha256 = sha256_file(&archive)?;
    let encrypted = scratch.join("bundle.gpg");
    encrypt_with(&seal, &archive, &encrypted)?;
    let stored_bytes = fs::metadata(&encrypted)
        .map_err(|error| format!("{}: {error}", encrypted.display()))?
        .len();
    let back = scratch.join("bundle.back");
    decrypt_with(&seal, &encrypted, &back)?;
    if sha256_file(&back)? != sha256 {
        return Err("the encrypted bundle does not decrypt to the archive".to_string());
    }

    let held = list_bucket(&settings)?.0;
    let tier = if full { "full" } else { "incr" };
    let key = settings.stored(&bundle_name(tier, now_ms, extension, &random_hex(4)?));
    let mut keys: Vec<String> = Vec::new();
    let mut skipped: Vec<Value> = Vec::new();
    let mut written = 0u64;
    if within_limit(held, written, stored_bytes, settings.limit_bytes) {
        put_object_with(
            &settings,
            &key,
            &encrypted,
            &sha256,
            (0, &seal.name),
            &format!("max-time = \"{}\"\n", bundle_put_seconds(stored_bytes)),
        )?;
        written = written.saturating_add(stored_bytes);
        keys.push(key);
    } else {
        skipped.push(skip(tier, held, written, stored_bytes, &settings));
    }
    let over_alert = held.saturating_add(written) > settings.alert_bytes;
    let mut record = json!({
        "type": "backup.bundled",
        "sealed_with": seal.name,
        "kind": if full { "full" } else { "incremental" },
        "base_event_id": base.map(|base| base.event_id),
        "keys": keys,
        "tiers": [tier],
        "skipped": skipped,
        "sha256": sha256,
        "bytes": bytes,
        "stored_bytes": stored_bytes,
        "repos": repos_record,
        "missing": missing,
        "transcripts": {
            "files": transcript_entries.len(),
            "bytes": transp_bytes,
            "sizes": sizes,
            "sha256": hashes,
        },
        "held_bytes": held,
        "written_bytes": written,
        "alert_bytes": settings.alert_bytes,
        "limit_bytes": settings.limit_bytes,
        "over_alert": over_alert,
        "duration_ms": i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
    });
    if let Some(sync) = &sync {
        record["sync"] = sync.record.clone();
    }
    store
        .backup_record(&record)
        .map_err(|error| format!("backup.bundled: {error}"))?;
    // The next increment's base: the manifest of a stored bundle that holds
    // the notes folder, vouched for by this record's manifest_sha256.
    if let Some(local) = sync.as_ref().and_then(|sync| sync.local.as_deref())
        && !keys.is_empty()
        && let Err(error) = sync_dir::write_local(&settings.root, local)
    {
        eprintln!("spawn backup bundle: the notes manifest's local copy: {error}");
    }
    if let Some(sync) = &sync {
        match sync.record["refused"].as_str() {
            Some(reason) => {
                println!("spawn backup bundle: the notes folder was left out: {reason}");
            }
            None => println!(
                "spawn backup bundle: the notes folder: {} files, {} taken ({} bytes)",
                sync.record["files"], sync.record["taken"], sync.record["taken_bytes"]
            ),
        }
    }
    if over_alert {
        println!(
            "R2 holds {} bytes, over the alert at {}",
            held.saturating_add(written),
            settings.alert_bytes
        );
    }
    println!(
        "spawn backup bundle: {} {} bytes for {} transcript files ({} bytes), {} stored",
        if full { "full" } else { "incremental" },
        bytes,
        transcript_entries.len(),
        transp_bytes,
        if keys.is_empty() {
            "nothing".to_string()
        } else {
            keys.join(", ")
        }
    );
    Ok(0)
}

/// Copy `size - offset` bytes from `offset` of `source` to `target`.
fn append_slice(source: &Path, target: &Path, offset: u64, size: u64) -> Result<(), String> {
    let mut file = File::open(source).map_err(|error| format!("{}: {error}", source.display()))?;
    let mut out = File::create(target).map_err(|error| format!("{}: {error}", target.display()))?;
    if offset > 0 {
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| format!("{}: {error}", source.display()))?;
    }
    let mut left = size.saturating_sub(offset);
    let mut buffer = vec![0u8; 64 * 1024];
    while left > 0 {
        let want = usize::try_from(left.min(buffer.len() as u64)).unwrap_or(buffer.len());
        let read = file
            .read(&mut buffer[..want])
            .map_err(|error| format!("{}: {error}", source.display()))?;
        if read == 0 {
            break;
        }
        out.write_all(&buffer[..read])
            .map_err(|error| format!("{}: {error}", target.display()))?;
        left -= read as u64;
    }
    Ok(())
}

/// A git bundle of `source`'s branches and tags, but not `not`'s commits:
/// exactly the refs `heads_of` puts in the record, so the record describes
/// everything the archive holds (and nothing else, which `verify` refuses).
/// `--all` would also carry refs/remotes/* (live `supply-pa.git` has
/// `refs/remotes/github/main` from the mirror clone it was seeded as) and any
/// `refs/spawn-owner/*` request: neither is the mission's history, and a
/// record that names neither could not describe a bundle that holds them.
fn git_bundle(source: &str, file: &Path, not: &[String]) -> Result<(), String> {
    let mut command = crate::sources::controller_git();
    command
        .args(["-C", source, "bundle", "create"])
        .arg(file)
        .args(["--branches", "--tags"]);
    if !not.is_empty() {
        command.arg("--not");
        for commit in not {
            command.arg(commit);
        }
    }
    let (ok, _stdout, stderr) = output(command, "git bundle create")?;
    if !ok {
        return Err(format!("git bundle create {source}: {}", stderr.trim()));
    }
    Ok(())
}

/// The refs a bundle file lists, `(commit, ref)` and no `HEAD`: what an
/// increment's manifest records as `bundled`, so the archive says which refs
/// it holds instead of a reader inferring them from the base.
fn bundle_refs(file: &Path) -> Result<Vec<(String, String)>, String> {
    let mut command = crate::sources::controller_git();
    command.args(["bundle", "list-heads"]).arg(file);
    let (ok, stdout, stderr) = output(command, "git bundle list-heads")?;
    if !ok {
        return Err(format!(
            "git bundle list-heads {}: {}",
            file.display(),
            stderr.trim()
        ));
    }
    Ok(stdout
        .lines()
        .filter_map(|line| line.split_once(' '))
        .filter(|(_, name)| *name != "HEAD")
        .map(|(commit, name)| (commit.to_string(), name.to_string()))
        .collect())
}

/// Whether a bundle of `source`'s branches and tags, but not `not`'s, would
/// carry anything at all: when it would not (every ref points into the base)
/// `git bundle create` refuses to make an empty bundle, and an increment says
/// so by name before it asks (`bundle`). The object listing, not a commit
/// count: an annotated tag at a commit the base already holds is an object
/// outside it, and a bundle of just that tag is not empty.
fn bundle_would_write(source: &str, not: &[String]) -> Result<bool, String> {
    let mut command = crate::sources::controller_git();
    command.args([
        "-C",
        source,
        "rev-list",
        "--objects",
        "--max-count=1",
        "--branches",
        "--tags",
    ]);
    if !not.is_empty() {
        command.arg("--not");
        for commit in not {
            command.arg(commit);
        }
    }
    let (ok, stdout, stderr) = output(command, "git rev-list --objects")?;
    if !ok {
        return Err(format!(
            "git rev-list --objects {source}: {}",
            stderr.trim()
        ));
    }
    Ok(stdout.lines().next().is_some())
}

/// What the drill restored and where, for its record.
#[derive(Debug, Default, Clone)]
struct Drilled {
    database: Option<String>,
    events: Option<i64>,
    max_event_id: Option<i64>,
}

/// `spawn backup verify`: the newest good dump restored into a database of
/// its own and compared with its record, then the newest bundle opened and
/// compared with its own.
#[allow(clippy::too_many_lines)]
fn verify(flags: &Flags) -> Result<i32, String> {
    let started = Instant::now();
    let settings = match Settings::from_env(flags) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("spawn backup verify: {error}");
            record_verified(
                None,
                false,
                &error,
                &json!({}),
                &Drilled::default(),
                started,
            );
            return Ok(config_exit_code(&error));
        }
    };
    let _lock = Lock::acquire(&settings.root)?;
    let store = open_store()?;
    let Some(drill_url) = settings.drill_url.clone() else {
        let reason =
            "SPAWN_BACKUP_DRILL_DATABASE_URL is not set: the drill has no server to restore into";
        record_verified(
            Some(&store),
            false,
            reason,
            &json!({}),
            &Drilled::default(),
            started,
        );
        return Ok(2);
    };
    let now_ms = store.now_ms().map_err(|error| error.to_string())?;
    let dumps = dump_records(&store, &settings.prefix, now_ms)?;
    let latest = dumps.iter().rev().find(|entry| {
        entry.record["suspect"].is_null()
            && entry.record["keys"]
                .as_array()
                .is_some_and(|keys| !keys.is_empty())
    });
    let Some(latest) = latest else {
        record_verified(
            Some(&store),
            false,
            "no backup.dumped record with a key to verify",
            &json!({}),
            &Drilled::default(),
            started,
        );
        return Ok(2);
    };
    let key = latest.record["keys"][0]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let expected_sha = latest.record["sha256"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let expected = json!({
        "events": latest.record["events"],
        "max_event_id": latest.record["max_event_id"],
    });
    let naming = json!({
        "key": key,
        "sha256": expected_sha,
        "dumped_event_id": latest.event_id,
        "expected": expected,
    });
    // Read with the key its record names (objective 114).
    let sealed_with = record_key("backup.dumped", latest.event_id, &latest.record);
    let outcome = match &sealed_with {
        Ok(name) => restore_and_compare(&settings, &drill_url, latest, name),
        Err(reason) => Err(RestoreError {
            reason: reason.clone(),
            drilled: Drilled::default(),
        }),
    };
    let (mut ok, mut reason, drilled) = match outcome {
        Ok(drilled) => (true, String::new(), drilled),
        Err(RestoreError { reason, drilled }) => (false, reason, drilled),
    };
    let mut record = naming;
    if let Ok(name) = &sealed_with {
        record["sealed_with"] = json!(name);
    }
    record["events"] = json!(drilled.events);
    record["max_event_id"] = json!(drilled.max_event_id);
    record["server"] = json!(
        parse_pg_url(&drill_url)
            .map(|url| url.server())
            .unwrap_or_default()
    );
    // The dump proves the database restores. The bundle — the sources and the
    // transcripts — is the other half of what leaves the host, and the newest
    // one is opened the same way: fetched, decrypted, decompressed by its
    // magic, unpacked, its manifest and every repo's bundle opened, every
    // transcript file it names counted. Checked only when the dump restored:
    // a failed drill is already the day's verdict, and the reason names it.
    if ok {
        match verify_bundle(&settings, &store, now_ms) {
            Ok(check) => record["bundle"] = check,
            Err(error) => {
                // A failed check says so itself: a reader that greps `.bundle.checked` alone must not
                // read this as a pass (the record's top-level ok is false too).
                record["bundle"] = json!({"checked": true, "ok": false, "reason": error.clone()});
                ok = false;
                reason = error;
            }
        }
    }
    let code = if ok {
        record_verified(Some(&store), true, "", &record, &drilled, started);
        0
    } else {
        record_verified(Some(&store), false, &reason, &record, &drilled, started);
        1
    };
    if ok {
        println!(
            "spawn backup verify: {} restored to its record ({} events, max id {}) into {}; {}",
            key,
            record["events"],
            record["max_event_id"],
            drilled.database.as_deref().unwrap_or("a scratch database"),
            match record["bundle"]["checked"].as_bool() {
                Some(true) => format!(
                    "the bundle {} opens ({} repos, {} transcript files)",
                    record["bundle"]["key"].as_str().unwrap_or_default(),
                    record["bundle"]["repos"].as_i64().unwrap_or_default(),
                    record["bundle"]["transcripts"]["files"]
                        .as_i64()
                        .unwrap_or_default()
                ),
                _ => String::from("no bundle to open yet"),
            }
        );
    } else {
        eprintln!("spawn backup verify: {reason}");
    }
    Ok(code)
}

/// The bundle half of the drill: the newest `backup.bundled` record with a
/// key, fetched, checked against its record's sha256, decrypted,
/// decompressed by its magic and unpacked; its `manifest.json` compared with
/// the record (kind, transcript file count and bytes), every transcript file
/// it names found at its recorded size, and every repo's `git bundle`
/// opened — its heads listed, and for a full bundle verified alone (an
/// increment's prerequisites are the base's commits, which a drill does not
/// fetch). A store with no bundle yet says so and passes: there is nothing
/// to open.
#[allow(clippy::too_many_lines)]
fn verify_bundle(settings: &Settings, store: &PostgresStore, now_ms: i64) -> Result<Value, String> {
    let earlier = bundle_records(store, &settings.prefix, now_ms)?;
    let Some(latest) = earlier.iter().rev().find(|entry| {
        entry.record["keys"]
            .as_array()
            .is_some_and(|keys| !keys.is_empty())
    }) else {
        return Ok(json!({"checked": false, "reason": "no backup.bundled record with a key"}));
    };
    let key = latest.record["keys"][0].as_str().unwrap_or_default();
    let expected_sha = latest.record["sha256"].as_str().unwrap_or_default();
    let kind = latest.record["kind"].as_str().unwrap_or_default();
    let Some((bytes, headers)) = get_object_bytes(settings, key)? else {
        return Err(format!("the bucket does not hold the bundle {key}"));
    };
    if let Some(meta) = header_value(&headers, "x-amz-meta-sha256")
        && meta != expected_sha
    {
        return Err(format!(
            "the bundle's x-amz-meta-sha256 {meta} is not the record's {expected_sha}"
        ));
    }
    // Read with the key its record names, which its object must say too.
    let sealed_with = record_key("backup.bundled", latest.event_id, &latest.record)?;
    same_key(&sealed_with, &object_key(key, &headers)?)?;
    let seal = reading_key(&sealed_with, "the bundle")?;
    let scratch = Scratch::new("verify-bundle")?;
    let encrypted = scratch.join("bundle.gpg");
    fs::write(&encrypted, &bytes).map_err(|error| format!("{}: {error}", encrypted.display()))?;
    let raw = scratch.join("bundle.raw");
    decrypt_with(&seal, &encrypted, &raw)?;
    let found = sha256_file(&raw)?;
    if found != expected_sha {
        return Err(format!(
            "the bundle's sha256 {found} is not the record's {expected_sha}"
        ));
    }
    let (tar, compression) = decompress_bundle(&scratch.join(""), &raw)?;
    let unpacked = scratch.join("unpacked");
    fs::create_dir_all(&unpacked).map_err(|error| format!("{}: {error}", unpacked.display()))?;
    let (ok, _stdout, stderr) = output(
        {
            let mut command = Command::new("tar");
            command.arg("-xf").arg(&tar).arg("-C").arg(&unpacked);
            command
        },
        "tar -xf",
    )?;
    if !ok {
        return Err(format!(
            "the bundle {key} is no tar archive ({compression}): {}",
            stderr.trim()
        ));
    }
    let manifest_path = unpacked.join("manifest.json");
    let manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).map_err(|error| {
        format!("the bundle {key} holds no manifest.json ({compression}): {error}")
    })?)
    .map_err(|error| format!("the bundle's manifest.json: {error}"))?;
    let manifest_kind = manifest["kind"].as_str().unwrap_or_default();
    if manifest_kind != kind {
        return Err(format!(
            "the bundle's manifest says {manifest_kind:?}; its record says {kind:?}"
        ));
    }
    let entries = manifest["transcripts"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut files = 0i64;
    let mut held = 0u64;
    for entry in &entries {
        let path = entry["path"].as_str().unwrap_or_default();
        let want = entry["bytes"].as_u64().unwrap_or(0);
        let size = fs::metadata(unpacked.join("transcripts").join(path))
            .map_err(|_| format!("the bundle holds no transcripts/{path}"))?
            .len();
        if size != want {
            return Err(format!(
                "the bundle's transcripts/{path} is {size} bytes; its manifest says {want}"
            ));
        }
        files += 1;
        held = held.saturating_add(size);
    }
    let want_files = latest.record["transcripts"]["files"].as_i64().unwrap_or(-1);
    let want_bytes = latest.record["transcripts"]["bytes"].as_u64().unwrap_or(0);
    if files != want_files || held != want_bytes {
        return Err(format!(
            "the bundle holds {files} transcript files ({held} bytes); its record says {want_files} ({want_bytes})"
        ));
    }
    let mut repos = 0i64;
    let mut alone: Vec<String> = Vec::new();
    // A full bundle carries everything it needs: an empty repository verifies
    // it. An increment's prerequisites are its base's commits, which the
    // drill does not fetch, so only its heads are opened.
    let empty = scratch.join("empty.git");
    if kind == "full" {
        let (ok, _stdout, stderr) = output(
            {
                let mut command = crate::sources::controller_git();
                command.args(["init", "-q", "--bare"]).arg(&empty);
                command
            },
            "git init --bare",
        )?;
        if !ok {
            return Err(format!("git init --bare: {}", stderr.trim()));
        }
    }
    for repo in manifest["repos"].as_array().cloned().unwrap_or_default() {
        let Some(file) = repo["file"].as_str() else {
            continue; // unchanged since the base: nothing of it is in this bundle
        };
        let source = repo["source"].as_str().unwrap_or_default();
        let named: Vec<(String, String)> = repo["heads"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(name, commit)| {
                (
                    commit.as_str().unwrap_or_default().to_string(),
                    name.clone(),
                )
            })
            .collect();
        let bundle = unpacked.join(file);
        let (ok, stdout, stderr) = output(
            {
                let mut command = crate::sources::controller_git();
                command.args(["bundle", "list-heads"]).arg(&bundle);
                command
            },
            "git bundle list-heads",
        )?;
        if !ok {
            return Err(format!(
                "{source}'s bundle in {key} does not open: {}",
                stderr.trim()
            ));
        }
        // `git bundle list-heads` prints the full refname, as the manifest
        // records it (`refs/heads/...`, `refs/tags/...`), and HEAD beside it.
        let listed: BTreeSet<(String, String)> = stdout
            .lines()
            .filter_map(|line| line.split_once(' '))
            .filter(|(_, name)| *name != "HEAD")
            .map(|(commit, name)| (commit.to_string(), name.to_string()))
            .collect();
        let unknown: Vec<String> = listed
            .iter()
            .filter(|pair| !named.contains(*pair))
            .map(|(commit, name)| format!("{name} at {commit}"))
            .collect();
        if !unknown.is_empty() {
            return Err(format!(
                "{source}'s bundle in {key} lists {} the manifest does not name",
                unknown.join(", ")
            ));
        }
        // A full bundle names every head. An increment's bundle lists only
        // the refs whose tips the base does not hold (`--not` drops a ref
        // that did not move, and one whose new commit the base already has),
        // so its manifest says which refs it holds (`bundled`) and the bundle
        // must list exactly those — not what a reader, or this drill, would
        // infer from the base.
        let mut required: Vec<(String, String)> = Vec::new();
        if kind == "full" {
            required.clone_from(&named);
        } else {
            required.extend(
                repo["bundled"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|entry| {
                        Some((
                            entry["commit"].as_str()?.to_string(),
                            entry["ref"].as_str()?.to_string(),
                        ))
                    }),
            );
            // `bundled` is a subset of `heads`: a ref the bundle holds is one
            // the source has, at the same commit.
            let inconsistent: Vec<String> = required
                .iter()
                .filter(|pair| !named.contains(*pair))
                .map(|(commit, name)| format!("{name} at {commit}"))
                .collect();
            if !inconsistent.is_empty() {
                return Err(format!(
                    "{source}'s manifest records as bundled {} not among its heads",
                    inconsistent.join(", ")
                ));
            }
            if required.is_empty() {
                return Err(format!(
                    "{source}'s bundle in {key} is an increment that holds no ref"
                ));
            }
        }
        let missing: Vec<String> = required
            .iter()
            .filter(|pair| !listed.contains(*pair))
            .map(|(commit, name)| format!("{name} at {commit}"))
            .collect();
        if !missing.is_empty() || (listed.is_empty() && kind != "full") {
            return Err(format!(
                "{source}'s bundle in {key} does not list {}",
                if missing.is_empty() {
                    String::from("any head (an empty bundle)")
                } else {
                    missing.join(", ")
                }
            ));
        }
        if kind == "full" {
            let (ok, _stdout, stderr) = output(
                {
                    let mut command = crate::sources::controller_git();
                    command
                        .arg("--git-dir")
                        .arg(&empty)
                        .args(["bundle", "verify", "-q"])
                        .arg(&bundle);
                    command
                },
                "git bundle verify",
            )?;
            if !ok {
                return Err(format!(
                    "{source}'s full bundle in {key} does not verify alone: {}",
                    stderr.trim()
                ));
            }
            alone.push(source.to_string());
        }
        repos += 1;
    }
    Ok(json!({
        "checked": true,
        "ok": true,
        "key": key,
        "sealed_with": sealed_with,
        "bundled_event_id": latest.event_id,
        "kind": kind,
        "sha256": expected_sha,
        "compression": compression,
        "repos": repos,
        "verified_alone": alone,
        "transcripts": {"files": files, "bytes": held},
    }))
}

/// A drill that failed, with what it had made when it did.
struct RestoreError {
    reason: String,
    drilled: Drilled,
}

/// Fetch, check, restore, compare; the scratch database is dropped on every
/// path after it was made.
#[allow(clippy::too_many_lines)]
fn restore_and_compare(
    settings: &Settings,
    drill_url: &str,
    latest: &DumpRecord,
    sealed_with: &str,
) -> Result<Drilled, RestoreError> {
    let mut drilled = Drilled::default();
    let fail = |reason: String, drilled: Drilled| RestoreError { reason, drilled };
    let key = latest.record["keys"][0].as_str().unwrap_or_default();
    let expected_sha = latest.record["sha256"].as_str().unwrap_or_default();
    let Some((bytes, headers)) =
        get_object_bytes(settings, key).map_err(|error| fail(error, drilled.clone()))?
    else {
        return Err(RestoreError {
            reason: format!("the bucket does not hold {key}"),
            drilled,
        });
    };
    if let Some(meta) = header_value(&headers, "x-amz-meta-sha256")
        && meta != expected_sha
    {
        return Err(RestoreError {
            reason: format!(
                "the object's x-amz-meta-sha256 {meta} is not the record's {expected_sha}"
            ),
            drilled,
        });
    }
    // The object's own word on its key must be its record's, and the key's
    // file must be on this host.
    let seal = object_key(key, &headers)
        .and_then(|said| same_key(sealed_with, &said))
        .and_then(|()| reading_key(sealed_with, "the dump"))
        .map_err(|reason| fail(reason, drilled.clone()))?;
    let scratch = Scratch::new("drill").map_err(|error| fail(error, drilled.clone()))?;
    let encrypted = scratch.join("dump.gpg");
    fs::write(&encrypted, &bytes)
        .map_err(|error| fail(format!("{}: {error}", encrypted.display()), drilled.clone()))?;
    let raw = scratch.join("dump");
    if let Err(error) = decrypt_with(&seal, &encrypted, &raw) {
        return Err(RestoreError {
            reason: format!("the object does not decrypt: {error}"),
            drilled,
        });
    }
    match sha256_file(&raw) {
        Ok(found) if found == expected_sha => {}
        Ok(found) => {
            return Err(RestoreError {
                reason: format!("the dump's sha256 {found} is not the record's {expected_sha}"),
                drilled,
            });
        }
        Err(error) => {
            return Err(RestoreError {
                reason: error,
                drilled,
            });
        }
    }
    let url = parse_pg_url(drill_url).map_err(|error| fail(error, drilled.clone()))?;
    let base = if url.database.is_empty() {
        "postgres".to_string()
    } else {
        url.database.clone()
    };
    let database = format!(
        "spawn_drill_{}_{}",
        spawn_unix_seconds(),
        random_hex(4).unwrap_or_default()
    );
    if let Err(error) = create_database(settings, &url, &base, &database) {
        return Err(RestoreError {
            reason: error,
            drilled,
        });
    }
    drilled.database = Some(database.clone());
    let outcome = restore_into(settings, &url, &base, &database, &raw, &scratch).and_then(
        |(events, max_event_id)| {
            verify_restored_events(drill_url, &database)?;
            Ok((events, max_event_id))
        },
    );
    // The database the drill made goes on every path.
    let dropped = drop_database(settings, &url, &base, &database);
    match outcome {
        Ok((events, max_event_id)) => {
            drilled.events = Some(events);
            drilled.max_event_id = Some(max_event_id);
            let want_events = latest.record["events"].as_i64().unwrap_or(-1);
            let want_max = latest.record["max_event_id"].as_i64().unwrap_or(-1);
            if events != want_events || max_event_id != want_max {
                return Err(RestoreError {
                    reason: format!(
                        "the restore holds {events} events (max id {max_event_id}); its record says \
{want_events} (max id {want_max})"
                    ),
                    drilled,
                });
            }
            if let Err(error) = dropped {
                return Err(RestoreError {
                    reason: format!(
                        "the scratch database {database} could not be dropped: {error}"
                    ),
                    drilled,
                });
            }
            Ok(drilled)
        }
        Err(error) => Err(RestoreError {
            reason: match dropped {
                Ok(()) => error,
                Err(drop) => format!("{error}; and {drop}"),
            },
            drilled,
        }),
    }
}

/// Attempts of one whole-object GET before it fails (t494u).
const GET_ATTEMPTS: u32 = 6;
/// A transfer slower than this many bytes a second for [`GET_STALL_SECS`]
/// is cut and resumed, not waited out.
const GET_STALL_BYTES: u64 = 4096;
const GET_STALL_SECS: u64 = 60;
/// The per-attempt time cap is a backstop only: 600 s plus the bytes still
/// to come at this rate.
const GET_FLOOR_BYTES_PER_SEC: u64 = 64 * 1024;

/// The object's bytes, as they are (not through a String), and its headers.
///
/// Robust to a slow or cut transfer (t494u: the controllers drill of 11-17
/// 16:31 lost a 146 MB bundle to one 600 s attempt at ~155 KB/s): the body
/// goes to a scratch file, and each attempt resumes from the bytes already
/// there (`continue-at`, an HTTP Range), cuts a stalled transfer
/// ([`GET_STALL_BYTES`] for [`GET_STALL_SECS`]) instead of waiting for a
/// fixed cap, and has a cap that grows with the bytes still to come
/// ([`get_options`]). Up to [`GET_ATTEMPTS`] attempts, apart by
/// [`get_pause_secs`]. It returns only a body whose length is the object's
/// (Content-Range or Content-Length) and, when it was joined from more than
/// one answer and the ETag is a plain MD5 (one PUT), whose MD5 is the ETag.
/// A body from one whole answer is what the server sent; callers check it
/// as before: the `x-amz-meta-sha256` and, after decryption, the record's
/// sha256 (scenario 186 (7) asserts that last check names the failure).
fn get_object_bytes(settings: &Settings, key: &str) -> Result<Option<(Vec<u8>, String)>, String> {
    get_object_with(settings, key, get_pause_secs)
}

/// [`get_object_bytes`] with its pause schedule given (tests pass none).
fn get_object_with(
    settings: &Settings,
    key: &str,
    pause: fn(u32) -> u64,
) -> Result<Option<(Vec<u8>, String)>, String> {
    let url = settings.r2.url(key);
    let scratch = Scratch::new("get")?;
    let body = scratch.join("body");
    let headers_file = scratch.join("headers");
    let size = |path: &Path| fs::metadata(path).map_or(0, |meta| meta.len());
    let mut total: Option<u64> = None;
    let mut headers = String::new();
    let mut errors: Vec<String> = Vec::new();
    // Whether the body was joined from more than one answer (an attempt
    // that resumed after bytes already there): only such a join is checked
    // against the ETag, the one thing a splice can get wrong.
    let mut joined = false;
    for attempt in 0..GET_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_secs(pause(attempt)));
        }
        let have = size(&body);
        if total == Some(have) {
            break;
        }
        if have > 0 {
            joined = true;
        }
        let mut options = get_options(have, total.map(|total| total.saturating_sub(have)));
        // A resumed attempt asks for the same object only (rev-b n3): an
        // object rewritten between attempts answers 412, never a splice.
        if have > 0
            && let Some(etag) = header_value(&headers, "etag")
        {
            let _ = writeln!(options, "header = \"If-Match: {}\"", escape_config(&etag));
        }
        let _ = fs::remove_file(&headers_file);
        let result = run_curl(settings, &url, &options, &body, &headers_file);
        let these = fs::read_to_string(&headers_file).unwrap_or_default();
        // The answer's head is read even when curl failed part way (a cut
        // or a stall after the headers came): its length is learned then
        // too, so the next attempt's cap is the object's (rev-b n1, n2).
        let status = match &result {
            Ok(status) => Some(*status),
            Err(_) => last_status(&these),
        };
        match status {
            Some(200 | 206) => {
                if let Some(length) = object_length(status.unwrap_or(0), &these) {
                    total = Some(length);
                }
                headers = these;
            }
            Some(416) => {
                if let Some(length) = unsatisfiable_length(&these) {
                    total = Some(length);
                }
            }
            _ => {}
        }
        match result {
            Ok(200 | 206) => {}
            Ok(404) => return Ok(None),
            // Asked from the end of a body already whole.
            Ok(416) if total == Some(have) => break,
            Ok(412) => {
                return Err(format!(
                    "GET {key}: the object changed between attempts (If-Match answered 412)"
                ));
            }
            Ok(status) => return Err(format!("GET {key} answered {status}")),
            Err(error) => {
                // curl 33: the server ignored the Range; start again whole.
                if error.contains("exit 33:") {
                    let _ = fs::remove_file(&body);
                }
                errors.push(format!("attempt {}: {error}", attempt + 1));
            }
        }
        if total == Some(size(&body)) {
            break;
        }
    }
    let have = size(&body);
    let Some(total) = total.filter(|total| *total == have) else {
        return Err(format!(
            "GET {key}: {have} of {} bytes after {GET_ATTEMPTS} attempts ({})",
            total.map_or_else(
                || "an unknown number of".to_string(),
                |total| total.to_string()
            ),
            errors.join("; ")
        ));
    };
    let etag = header_value(&headers, "etag")
        .unwrap_or_default()
        .trim_matches('"')
        .to_ascii_lowercase();
    if joined && is_plain_md5(&etag) {
        let found = md5_file(&body)?;
        if found != etag {
            return Err(format!(
                "GET {key}: the {total} bytes have MD5 {found}, not the ETag {etag}"
            ));
        }
    }
    let bytes = fs::read(&body).map_err(|error| format!("{}: {error}", body.display()))?;
    Ok(Some((bytes, headers)))
}

/// The curl options of one GET attempt: resume after `have` bytes, cut a
/// stall, and a time cap of 600 s plus the bytes still to come at
/// [`GET_FLOOR_BYTES_PER_SEC`] (unknown before the first answer: 600 s plus
/// 256 MiB at that rate). The last `max-time` of a config wins.
pub(crate) fn get_options(have: u64, remaining: Option<u64>) -> String {
    let remaining = remaining.unwrap_or(256 * 1024 * 1024);
    let cap = 600 + remaining / GET_FLOOR_BYTES_PER_SEC;
    let mut options = format!(
        "max-time = \"{cap}\"\nspeed-limit = \"{GET_STALL_BYTES}\"\nspeed-time = \"{GET_STALL_SECS}\"\n"
    );
    if have > 0 {
        let _ = writeln!(options, "continue-at = \"{have}\"");
    }
    options
}

/// Seconds before attempt `attempt` (1-based retries): 5, 15, 30, 60, 120.
pub(crate) fn get_pause_secs(attempt: u32) -> u64 {
    match attempt {
        0 => 0,
        1 => 5,
        2 => 15,
        3 => 30,
        4 => 60,
        _ => 120,
    }
}

/// The whole object's length from one answer: a 206's Content-Range
/// `bytes a-b/total`, or a 200's Content-Length.
pub(crate) fn object_length(status: u16, headers: &str) -> Option<u64> {
    match status {
        206 => header_value(headers, "content-range")
            .and_then(|range| {
                range
                    .rsplit_once('/')
                    .map(|(_, total)| total.trim().to_string())
            })
            .and_then(|total| total.parse().ok()),
        200 => header_value(headers, "content-length").and_then(|text| text.parse().ok()),
        _ => None,
    }
}

/// An ETag that is one PUT's MD5 (32 hex), not a multipart one (`…-n`).
pub(crate) fn is_plain_md5(etag: &str) -> bool {
    etag.len() == 32 && etag.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// The status of the last HTTP head in a dumped header file, if any.
pub(crate) fn last_status(headers: &str) -> Option<u16> {
    headers
        .lines()
        .rev()
        .find(|line| line.starts_with("HTTP/"))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
}

/// A 416's whole length: `Content-Range: bytes */<total>`.
pub(crate) fn unsatisfiable_length(headers: &str) -> Option<u64> {
    header_value(headers, "content-range")
        .and_then(|range| {
            range
                .strip_prefix("bytes */")
                .map(|total| total.trim().to_string())
        })
        .and_then(|total| total.parse().ok())
}

fn spawn_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(0))
}

/// A psql run: `-X -q -tA`, its connection settings in the environment, and
/// `ON_ERROR_STOP` when it is given SQL to run.
fn psql(
    settings: &Settings,
    url: &PgUrl,
    database: &str,
    sql: Option<&str>,
    file: Option<&Path>,
) -> Result<(bool, String, String), String> {
    let mut command = Command::new(settings.pg_tool("psql"));
    command.args(["-X", "-q", "-tA", "-d", database]);
    if file.is_some() {
        command.args(["-v", "ON_ERROR_STOP=1", "--single-transaction"]);
    }
    if let Some(sql) = sql {
        command.args(["-v", "ON_ERROR_STOP=1", "-c", sql]);
    }
    if let Some(file) = file {
        command.arg("-f").arg(file);
    }
    for (name, value) in url.env() {
        command.env(name, value);
    }
    output(command, "psql")
}

fn create_database(
    settings: &Settings,
    url: &PgUrl,
    base: &str,
    database: &str,
) -> Result<(), String> {
    if !database
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(format!(
            "the scratch database name {database:?} is not a plain identifier"
        ));
    }
    let (ok, found, stderr) = psql(
        settings,
        url,
        base,
        Some(&format!(
            "select 1 from pg_database where datname = '{database}'"
        )),
        None,
    )?;
    if !ok {
        return Err(format!("the drill server: {}", stderr.trim()));
    }
    if !found.trim().is_empty() {
        return Err(format!(
            "the database {database} already exists: the drill makes its own"
        ));
    }
    let (ok, _stdout, stderr) = psql(
        settings,
        url,
        base,
        Some(&format!("create database \"{database}\"")),
        None,
    )?;
    if !ok {
        return Err(format!("create database {database}: {}", stderr.trim()));
    }
    Ok(())
}

fn drop_database(
    settings: &Settings,
    url: &PgUrl,
    base: &str,
    database: &str,
) -> Result<(), String> {
    let (ok, _stdout, stderr) = psql(
        settings,
        url,
        base,
        Some(&format!(
            "drop database if exists \"{database}\" with (force)"
        )),
        None,
    )?;
    if ok {
        Ok(())
    } else {
        Err(format!("drop database {database}: {}", stderr.trim()))
    }
}

/// The server's major version, so a newer `pg_restore`'s `SET
/// transaction_timeout = 0;` is left out for a server that predates it.
pub(crate) fn server_major(version: &str) -> u32 {
    version
        .trim()
        .split(['.', ' '])
        .next()
        .and_then(|major| major.parse().ok())
        .unwrap_or(0)
}

/// Drop `SET transaction_timeout = 0;` from a restore script, for a server
/// older than 17 that does not know the parameter.
pub(crate) fn without_transaction_timeout(sql: &str, major: u32) -> String {
    if major >= 17 {
        return sql.to_string();
    }
    let mut kept = String::with_capacity(sql.len());
    for line in sql.lines() {
        if line.trim() != "SET transaction_timeout = 0;" {
            let _ = writeln!(kept, "{line}");
        }
    }
    kept
}

fn restore_into(
    settings: &Settings,
    url: &PgUrl,
    base: &str,
    database: &str,
    dump: &Path,
    scratch: &Scratch,
) -> Result<(i64, i64), String> {
    let (ok, version, stderr) = psql(settings, url, base, Some("show server_version"), None)?;
    if !ok {
        return Err(format!("the drill server: {}", stderr.trim()));
    }
    let major = server_major(&version);
    let script = scratch.join("restore.sql");
    let (ok, _stdout, stderr) = output(
        {
            let mut command = Command::new(settings.pg_tool("pg_restore"));
            command
                .args(["--no-owner", "--no-acl", "-f"])
                .arg(&script)
                .arg(dump);
            command
        },
        "pg_restore",
    )?;
    if !ok {
        return Err(format!("pg_restore: {}", stderr.trim()));
    }
    let sql =
        fs::read_to_string(&script).map_err(|error| format!("{}: {error}", script.display()))?;
    let filtered = scratch.join("restore.filtered.sql");
    fs::write(&filtered, without_transaction_timeout(&sql, major))
        .map_err(|error| format!("{}: {error}", filtered.display()))?;
    let (ok, _stdout, stderr) = psql(settings, url, database, None, Some(&filtered))?;
    if !ok {
        return Err(format!("the restore failed: {}", stderr.trim()));
    }
    let (ok, counts, stderr) = psql(
        settings,
        url,
        database,
        Some("select count(*) || ' ' || coalesce(max(id), 0) from spawn_events"),
        None,
    )?;
    if !ok {
        return Err(format!("the restored database: {}", stderr.trim()));
    }
    let mut parts = counts.split_whitespace();
    let events = parts
        .next()
        .and_then(|part| part.parse().ok())
        .unwrap_or(-1);
    let max = parts
        .next()
        .and_then(|part| part.parse().ok())
        .unwrap_or(-1);
    Ok((events, max))
}

/// Record one drill, ok or not, with what it restored and where.
fn record_verified(
    store: Option<&PostgresStore>,
    ok: bool,
    reason: &str,
    naming: &Value,
    drilled: &Drilled,
    started: Instant,
) {
    let Some(store) = store else {
        return;
    };
    let mut record = json!({
        "type": "backup.verified",
        "ok": ok,
        "reason": reason,
        "key": naming.get("key").cloned().unwrap_or(Value::Null),
        "sha256": naming.get("sha256").cloned().unwrap_or(Value::Null),
        "dumped_event_id": naming.get("dumped_event_id").cloned().unwrap_or(Value::Null),
        // The key the dump's record names (objective 114); the bundle's is
        // under `bundle`.
        "sealed_with": naming.get("sealed_with").cloned().unwrap_or(Value::Null),
        "expected": naming.get("expected").cloned().unwrap_or(Value::Null),
        "events": drilled.events,
        "max_event_id": drilled.max_event_id,
        "database": drilled.database,
        "server": naming.get("server").cloned().unwrap_or(Value::Null),
        "bundle": naming.get("bundle").cloned().unwrap_or(Value::Null),
        "duration_ms": i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
    });
    if record["server"].is_null()
        && let Ok(Some(drilled_url)) = drill_database_url()
        && let Ok(url) = parse_pg_url(&drilled_url)
    {
        record["server"] = json!(url.server());
    }
    let _ = store.backup_record(&record);
}

/// Rewrite `drill_url` (`postgres://...` or `postgresql://...`) so its target
/// database is `database` while keeping host, port, credentials and other query
/// options intact. Error text never echoes the URL because it carries the
/// password.
pub(crate) fn url_for_database(drill_url: &str, database: &str) -> Result<String, String> {
    let (scheme, rest) = drill_url
        .split_once("://")
        .ok_or_else(|| "the drill database URL is not a postgres:// URL".to_string())?;
    if scheme != "postgres" && scheme != "postgresql" {
        return Err("the drill database URL is not a postgres:// URL".to_string());
    }
    let (head, query) = rest.split_once('?').unwrap_or((rest, ""));
    let authority = head.split_once('/').map_or(head, |(auth, _)| auth);
    let kept_query: Vec<&str> = query
        .split('&')
        .filter(|pair| !pair.is_empty() && !pair.starts_with("dbname="))
        .collect();
    if kept_query.is_empty() {
        Ok(format!("{scheme}://{authority}/{database}"))
    } else {
        Ok(format!(
            "{scheme}://{authority}/{database}?{}",
            kept_query.join("&")
        ))
    }
}

/// Decode every row of `spawn_events` in the restored scratch database through
/// `PostgresStore` so schema or payload corruption is caught before the scratch
/// database is dropped.
pub(crate) fn verify_restored_events(drill_url: &str, database: &str) -> Result<usize, String> {
    let url = url_for_database(drill_url, database)?;
    let store = PostgresStore::connect_with_options(&url, &crate::config::pg_connect_options(None))
        .map_err(|error| format!("connect restored database {database}: {error}"))?;
    verify_store_events(&store, 1000)
}

/// Read every global event batch from `store`, failing on the first corrupt row.
pub(crate) fn verify_store_events(
    store: &dyn spawn_core::port::EventLog,
    batch_size: usize,
) -> Result<usize, String> {
    use spawn_core::event::GlobalPosition;

    let mut after = GlobalPosition(0);
    let mut decoded = 0usize;
    loop {
        let batch = store
            .read_global(after, batch_size)
            .map_err(|error| format!("decode restored spawn_events: {error}"))?;
        decoded += batch.len();
        if batch.len() < batch_size {
            return Ok(decoded);
        }
        let Some(last) = batch.last() else {
            return Ok(decoded);
        };
        if last.position <= after {
            return Err("decode restored spawn_events: cursor did not advance".to_string());
        }
        after = last.position;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        R2, Settings, Stamp, Tier, bundle_compression, bundle_stamps, copy_events, full_bundle,
        get_options, get_pause_secs, is_plain_md5, md5_hex, not_locked_forever, object_length,
        parse_env_file, parse_listing, parse_listing_tagged, parse_pg_url, put_object,
        read_bundle_records, read_dump_records, server_major, suspect, tiers_for,
        transcript_offset, window_files, within_limit, without_transaction_timeout,
    };

    /// `SPAWN_BACKUP_PREFIX` is a bucket prefix and nothing else: unset or
    /// empty keeps the fleet's keys; a test's `locktest/` puts every object
    /// under it; anything that could leave the namespace is refused.
    #[test]
    fn the_prefix_is_a_key_prefix_or_nothing() {
        assert_eq!(super::prefix_value(None), Ok(String::new()));
        assert_eq!(super::prefix_value(Some(String::new())), Ok(String::new()));
        assert_eq!(super::prefix_value(Some("  ".into())), Ok(String::new()));
        assert_eq!(
            super::prefix_value(Some("locktest/".into())),
            Ok("locktest/".into())
        );
        for bad in [
            "locktest",
            "/locktest/",
            "a//b/",
            "../x/",
            "a/../b/",
            "s3://x/",
            "a b/",
        ] {
            assert!(
                super::prefix_value(Some(bad.into())).is_err(),
                "{bad:?} was accepted"
            );
        }
    }

    #[test]
    fn url_for_database_rewrites_target_without_leaking_credentials() {
        assert_eq!(
            super::url_for_database(
                "postgres://spawn:secret@127.0.0.1:65432/postgres?sslmode=disable&dbname=old",
                "spawn_drill_42"
            )
            .expect("valid url"),
            "postgres://spawn:secret@127.0.0.1:65432/spawn_drill_42?sslmode=disable"
        );
        assert_eq!(
            super::url_for_database("postgresql://127.0.0.1:65432", "spawn_drill_42")
                .expect("url without path"),
            "postgresql://127.0.0.1:65432/spawn_drill_42"
        );
        let err = super::url_for_database("https://secret_pass@example.com/db", "x").unwrap_err();
        assert!(
            !err.contains("secret_pass"),
            "error leaked URL secret: {err}"
        );
    }

    fn assert_verify_rejects_corrupt_rows(
        base_url: &str,
        scratch_db: &str,
        db_client: &mut postgres::Client,
        agent: &str,
        session: &str,
    ) -> Result<(), String> {
        // Truly corrupt rows fail closed: (a) known EventKind with corrupt detail,
        // (b) invalid-format name, and (c) non-object JSON kind.
        for (seq, kind_sql, label) in [
            (
                4_i64,
                "{\"name\":\"message.typed\",\"detail\":{\"command_id\":\"not-a-number\"}}",
                "known event with corrupt detail",
            ),
            (
                5_i64,
                "{\"name\":\"BAD NAME!\",\"detail\":{}}",
                "invalid event name",
            ),
            (6_i64, "42", "non-object JSON kind"),
        ] {
            db_client
                .execute(
                    "insert into spawn_events (agent_id, session_id, source, kind, session_seq, recorded_at_ms)
                     values ($1, $2, 'claude', $3::text::jsonb, $4, 2000)",
                    &[&agent, &session, &kind_sql, &seq],
                )
                .map_err(|e| format!("insert corrupt ({label}): {e}"))?;
            let check = super::verify_restored_events(base_url, scratch_db);
            db_client
                .execute(
                    "delete from spawn_events where session_id = $1 and session_seq = $2",
                    &[&session, &seq],
                )
                .map_err(|e| format!("cleanup corrupt ({label}): {e}"))?;
            match check {
                Ok(n) => {
                    return Err(format!("unexpected success decoding {label} row ({n})"));
                }
                Err(msg) if msg.contains("decode restored spawn_events") => {}
                Err(msg) => return Err(format!("unexpected error for {label}: {msg}")),
            }
        }
        Ok(())
    }

    #[test]
    fn verify_restored_events_allows_retired_rows_and_rejects_corrupt_rows() {
        let Ok(base_url) = std::env::var("SPAWN_TEST_DATABASE_URL") else {
            return;
        };
        let scratch_db = format!(
            "spawn_backup_verify_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let url = parse_pg_url(&base_url).expect("valid SPAWN_TEST_DATABASE_URL");
        let base_db = if url.database.is_empty() {
            "postgres".to_string()
        } else {
            url.database.clone()
        };
        let mut admin = postgres::Client::connect(&base_url, postgres::NoTls).expect("admin pg");
        admin
            .batch_execute(&format!("create database \"{scratch_db}\""))
            .expect("create scratch db");
        let scratch_url = super::url_for_database(&base_url, &scratch_db).expect("scratch url");
        let scoped = super::PostgresStore::connect(&scratch_url).expect("scoped connect");
        let setup = (|| -> Result<(), String> {
            scoped.migrate().map_err(|e| format!("migrate: {e}"))?;
            let agent = spawn_core::id::AgentId::new("agent_b").expect("agent");
            let session = spawn_core::id::SessionId::new("session_b").expect("session");
            spawn_core::port::EventLog::append(
                &scoped,
                spawn_core::event::NewEvent {
                    agent_id: agent.clone(),
                    session_id: session.clone(),
                    execution_id: None,
                    source: spawn_core::event::Source::Bridge,
                    source_event_id: Some("k1".to_string()),
                    kind: spawn_core::event::EventKind::MessageReceived,
                },
            )
            .map_err(|e| format!("append: {e}"))?;
            let mut db_client = postgres::Client::connect(&scratch_url, postgres::NoTls)
                .map_err(|e| e.to_string())?;
            for (seq, kind_sql, label) in [
                (2_i64, "{\"name\":\"notice.received\"}", "retired"),
                (
                    3_i64,
                    "{\"name\":\"future.v99_event\",\"detail\":{}}",
                    "future unknown",
                ),
            ] {
                db_client
                    .execute(
                        "insert into spawn_events (agent_id, session_id, source, kind, session_seq, recorded_at_ms)
                         values ($1, $2, 'claude', $3::text::jsonb, $4, 2000)",
                        &[&agent.as_str(), &session.as_str(), &kind_sql, &seq],
                    )
                    .map_err(|e| format!("insert {label}: {e}"))?;
            }
            let decoded = super::verify_restored_events(&base_url, &scratch_db)?;
            if decoded != 1 {
                return Err(format!("expected 1 active decoded row, got {decoded}"));
            }
            assert_verify_rejects_corrupt_rows(
                &base_url,
                &scratch_db,
                &mut db_client,
                agent.as_str(),
                session.as_str(),
            )
        })();
        drop(scoped);
        let _ = admin.batch_execute(&format!(
            "select pg_terminate_backend(pid) from pg_stat_activity where datname = '{scratch_db}' and pid <> pg_backend_pid()"
        ));
        admin
            .batch_execute(&format!("drop database if exists \"{scratch_db}\""))
            .expect("drop scratch db");
        let _ = base_db;
        setup.expect("backup verify full-table check");
    }
    use serde_json::json;
    use std::fs;

    /// A stored bundle is read by its magic, not by its name: gzip's `1f 8b`,
    /// zstd's `28 b5 2f fd`, and anything else is the tar itself.
    #[test]
    fn a_stored_bundle_is_read_by_its_magic() {
        assert_eq!(bundle_compression(&[0x1f, 0x8b, 0x08, 0x00]), "gzip");
        assert_eq!(bundle_compression(&[0x28, 0xb5, 0x2f, 0xfd]), "zstd");
        assert_eq!(bundle_compression(b"repo/"), "plain");
        assert_eq!(bundle_compression(&[0x28, 0xb5]), "plain");
        assert_eq!(bundle_compression(&[]), "plain");
    }

    /// A full bundle under a measurement's `SPAWN_BACKUP_PREFIX` is not the
    /// fleet's to read: with yesterday's scratch-prefix full in the store,
    /// today's fleet run is still a *full* (there is no recent full of its
    /// own to be an increment of) and its base names no key of the
    /// measurement's. A run under `locktest/` sees only its own, the other
    /// way round. The dump pick the drill makes is isolated the same way.
    #[test]
    fn a_scratch_prefix_full_is_never_anothers_base() {
        let now_ms = 1_794_110_400_000i64; // 2026-11-08 04:00 UTC
        let scratch_ms = now_ms - 86_400_000; // yesterday: the same ISO week
        let fleet_ms = now_ms - 21 * 86_400_000;
        let key = |prefix: &str, stamp: &str, full: bool| {
            if full {
                format!("{prefix}bundles/full/spawn-bundle-{stamp}.tar.zst.gpg")
            } else {
                format!("{prefix}postgres/hourly/spawn-{stamp}.dump.gpg")
            }
        };
        let store_answer = json!([
            {"event_id": 1, "at_ms": fleet_ms, "record": {
                "type": "backup.bundled", "kind": "full",
                "keys": [key("", "20261018-040000", true)],
                "transcripts": {"sizes": {"a.jsonl": 5_000_000}},
            }},
            {"event_id": 2, "at_ms": scratch_ms, "record": {
                "type": "backup.bundled", "kind": "full",
                "keys": [key("locktest/", "20261107-040000", true)],
                "transcripts": {"sizes": {"a.jsonl": 9_000_000}},
            }},
        ]);
        let fleet = read_bundle_records("", &store_answer);
        assert_eq!(fleet.len(), 1, "the scratch-prefix full was read");
        assert_eq!(fleet[0].event_id, 1);
        assert_eq!(
            fleet
                .last()
                .and_then(|base| base.record["transcripts"]["sizes"]["a.jsonl"].as_u64()),
            Some(5_000_000),
            "the base is the fleet's own full"
        );
        assert!(
            full_bundle(stamp("20261108-040000"), &bundle_stamps(&fleet), now_ms),
            "today's fleet run chained onto the measurement's full"
        );
        // Unfiltered, the same store answer would have made it an increment.
        let all: Vec<super::BundleRecord> = store_answer
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| super::BundleRecord {
                event_id: entry["event_id"].as_i64().unwrap(),
                at_ms: entry["at_ms"].as_i64().unwrap(),
                record: entry["record"].clone(),
            })
            .collect();
        assert!(
            !full_bundle(stamp("20261108-040000"), &bundle_stamps(&all), now_ms),
            "the fixture no longer shows what the filter prevents"
        );

        let scratch = read_bundle_records("locktest/", &store_answer);
        assert_eq!(scratch.len(), 1);
        assert_eq!(scratch[0].event_id, 2);
        assert!(!full_bundle(
            stamp("20261108-040000"),
            &bundle_stamps(&scratch),
            now_ms
        ));

        let dumps = json!([
            {"event_id": 3, "at_ms": fleet_ms, "record": {
                "type": "backup.dumped", "keys": [key("", "20261018-040000", false)],
                "suspect": null,
            }},
            {"event_id": 4, "at_ms": scratch_ms, "record": {
                "type": "backup.dumped", "keys": [key("locktest/", "20261107-040000", false)],
                "suspect": null,
            }},
        ]);
        let ids = |prefix: &str| {
            read_dump_records(prefix, &dumps)
                .into_iter()
                .map(|entry| entry.event_id)
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(""), vec![3]);
        assert_eq!(ids("locktest/"), vec![4]);
    }

    fn stamp(text: &str) -> Stamp {
        Stamp::parse(text).expect("a stamp")
    }

    #[test]
    fn md5_matches_its_known_answers() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            md5_hex(b"The quick brown fox jumps over the lazy dog"),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
    }

    #[test]
    fn a_url_is_read_the_way_libpq_reads_it() {
        let url = parse_pg_url("postgres://u:p%40ss@db.example:6543/spawn?sslmode=disable")
            .expect("a URL");
        assert_eq!(url.user, "u");
        assert_eq!(url.password, "p@ss");
        assert_eq!(url.host, "db.example");
        assert_eq!(url.port, "6543");
        assert_eq!(url.database, "spawn");
        assert_eq!(url.server(), "db.example:6543");
        let socket = parse_pg_url("postgres://u@%2Frun%2Fpostgresql/x").expect("a socket URL");
        assert_eq!(socket.host, "/run/postgresql");
        assert_eq!(socket.port, "5432");
        assert_eq!(socket.server(), "/run/postgresql");
        let v6 = parse_pg_url("postgres://u@[::1]:5433/x").expect("an IPv6 URL");
        assert_eq!(v6.host, "::1");
        assert_eq!(v6.port, "5433");
        let overridden = parse_pg_url("postgres://u@a/b?host=/tmp&port=5555").expect("a URL");
        assert_eq!(overridden.host, "/tmp");
        assert_eq!(overridden.port, "5555");
        assert!(parse_pg_url("mysql://x").is_err());
        assert!(parse_pg_url("postgres://a,b/x").is_err());
    }

    #[test]
    fn an_environment_file_is_parsed_never_sourced() {
        let vars = parse_env_file(
            "# the owner's\nSPAWN_R2_ENDPOINT=\"http://127.0.0.1:1\"\nexport SPAWN_R2_BUCKET=b\n\
SPAWN_R2_SECRET_ACCESS_KEY=s3cr3t\n\n",
        );
        assert_eq!(vars["SPAWN_R2_ENDPOINT"], "http://127.0.0.1:1");
        assert_eq!(vars["SPAWN_R2_BUCKET"], "b");
        assert_eq!(vars["SPAWN_R2_SECRET_ACCESS_KEY"], "s3cr3t");
    }

    #[test]
    fn the_tiers_are_the_days_first_and_the_weeks_first() {
        let now = stamp("20261108-071500");
        assert_eq!(
            tiers_for(now, &[]),
            [Tier::Hourly, Tier::Daily, Tier::Weekly],
            "the first dump of the day and of the week"
        );
        let same_day = [stamp("20261108-031500")];
        assert_eq!(tiers_for(now, &same_day), [Tier::Hourly]);
        // The day before, in the same ISO week (2026-11-08 is a Sunday, so
        // the 2nd is the same week's Monday).
        let same_week = [stamp("20261102-031500")];
        assert_eq!(tiers_for(now, &same_week), [Tier::Hourly, Tier::Daily]);
        let last_week = [stamp("20261101-031500")];
        assert_eq!(
            tiers_for(now, &last_week),
            [Tier::Hourly, Tier::Daily, Tier::Weekly]
        );
        assert_eq!(
            Tier::Daily.key("20261108-071500"),
            "postgres/daily/spawn-20261108-071500.dump.gpg"
        );
    }

    #[test]
    fn a_copy_past_the_limit_is_never_written() {
        assert!(within_limit(0, 0, 1_700_000, 10_000_000_000));
        assert!(within_limit(9_998_300_000, 0, 1_700_000, 10_000_000_000));
        assert!(
            !within_limit(9_998_300_001, 0, 1_700_000, 10_000_000_000),
            "one byte past the limit is past it"
        );
        assert!(
            !within_limit(8_000_000_000, 1_999_999_999, 2, 10_000_000_000),
            "the copies this run wrote count too"
        );
        // The hourly copy is subject to it like every other.
        assert!(!within_limit(1_000_000, 0, 2, 1_000_000));
    }

    #[test]
    fn a_bundle_is_full_on_the_weeks_first_or_after_seven_days_without_one() {
        let now = stamp("20261108-071500");
        let now_ms = 1_760_000_000_000i64;
        // No bundle before it: full.
        assert!(full_bundle(now, &[], now_ms));
        // A full one earlier the same week: an increment.
        assert!(!full_bundle(
            now,
            &[(stamp("20261102-031500"), true, now_ms - 86_400_000)],
            now_ms
        ));
        // Only increments this week, but a full one eight days back: full.
        assert!(full_bundle(
            now,
            &[
                (stamp("20261031-031500"), true, now_ms - 8 * 86_400_000),
                (stamp("20261107-031500"), false, now_ms - 86_400_000),
            ],
            now_ms
        ));
        // A full one yesterday: an increment.
        assert!(!full_bundle(
            now,
            &[(stamp("20261107-031500"), true, now_ms - 86_400_000)],
            now_ms
        ));
    }

    #[test]
    fn an_increment_takes_its_offset_from_its_base() {
        assert_eq!(transcript_offset(Some(100), 250), 100, "a grown file");
        assert_eq!(
            transcript_offset(None, 250),
            0,
            "a file the base did not name"
        );
        assert_eq!(
            transcript_offset(Some(300), 250),
            0,
            "a smaller file, whole"
        );
        assert_eq!(transcript_offset(Some(250), 250), 250, "unchanged: nothing");
    }

    #[test]
    fn the_archive_counts_its_events_and_its_watermark_with_a_gap() {
        let text = "\
--
-- Data for Name: spawn_events; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.spawn_events (id, agent_id, session_id) FROM stdin;
1\ta\ts
2\ta\ts
1002\ta\ts
\\.
";
        assert_eq!(
            copy_events(text),
            (3, 1002),
            "the ids skip; the count is not the watermark"
        );
        assert_eq!(copy_events("no copy block\n"), (0, 0));
    }

    #[test]
    fn a_dump_smaller_than_the_newest_good_one_is_suspect() {
        assert_eq!(suspect(100, 2000, None), None);
        assert_eq!(suspect(100, 2000, Some((100, 4000))), None, "at the line");
        assert!(suspect(99, 4000, Some((100, 4000))).is_some());
        assert!(
            suspect(100, 1999, Some((100, 4000))).is_some(),
            "under half its size"
        );
        assert_eq!(suspect(100, 2000, Some((100, 4000))), None);
    }

    #[test]
    fn a_listing_of_several_pages_adds_up() {
        let page = "<?xml version=\"1.0\"?><ListBucketResult><Contents><Key>a</Key><Size>10</Size>\
</Contents><Contents><Key>b</Key><Size>25</Size></Contents><IsTruncated>true</IsTruncated>\
<NextContinuationToken>dG9rZW4=</NextContinuationToken></ListBucketResult>";
        let (entries, token) = parse_listing(page);
        assert_eq!(entries, [("a".to_string(), 10), ("b".to_string(), 25)]);
        assert_eq!(token.as_deref(), Some("dG9rZW4="));
        let last = "<ListBucketResult><Contents><Key>c</Key><Size>5</Size></Contents>\
<IsTruncated>false</IsTruncated></ListBucketResult>";
        let (entries, token) = parse_listing(last);
        assert_eq!(entries, [("c".to_string(), 5)]);
        assert_eq!(token, None, "the last page names no token");
    }

    /// `spawn backup` never writes to the keep bucket (the lead,
    /// 2026-11-13): refused at the PUT before any request (the endpoint is
    /// a closed port: a request would fail otherwise), and when its
    /// settings are read.
    #[test]
    fn spawn_backup_never_writes_to_the_keep_bucket() {
        let dir = std::env::temp_dir().join(format!(
            "spawn-backup-keep-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |time| time.subsec_nanos())
        ));
        std::fs::create_dir_all(&dir).expect("scratch");
        let file = dir.join("object");
        std::fs::write(&file, b"bytes").expect("object");
        let settings = |bucket: &str| Settings {
            root: dir.clone(),
            key_file: dir.join("key"),
            seal: Err("no sealing key in this test".to_string()),
            r2: R2 {
                endpoint: "http://127.0.0.1:1".to_string(),
                bucket: bucket.to_string(),
                access_key: "made-up-id".to_string(),
                secret_key: "made-up-secret".to_string(),
            },
            pg_bin: None,
            compress: "zstd:9".to_string(),
            transcript_days: 14,
            limit_bytes: 1,
            alert_bytes: 1,
            prefix: "locktest/x/".to_string(),
            drill_url: None,
        };
        let error = put_object(&settings("made-up-keep"), "k", &file, "s", 0, "k1").unwrap_err();
        assert!(error.contains("never writes there"), "{error}");
        assert!(!error.contains("made-up-keep"), "{error}");
        // Not an R2 name as given (rev-g F5): refused before the keep test
        // and before any request, untrimmed and unfolded.
        for bucket in [
            "MADE-UP-KEEP",
            "made-up-keep ",
            " made-up-keep",
            "made-up-Keep",
            "x",
        ] {
            let error = put_object(&settings(bucket), "k", &file, "s", 0, "k1").unwrap_err();
            assert!(
                error.contains("not an R2 bucket name"),
                "{bucket:?}: {error}"
            );
            assert!(
                bucket.trim().len() < 3 || !error.contains(bucket.trim()),
                "{error}"
            );
        }
        // Another bucket gets as far as the request (a closed port).
        let error = put_object(&settings("made-up-store"), "k", &file, "s", 0, "k1").unwrap_err();
        assert!(!error.contains("never writes there"), "{error}");
        assert!(not_locked_forever("made-up-keep").is_err());
        assert!(not_locked_forever("made-up-store").is_ok());
        std::fs::remove_dir_all(&dir).expect("clean up");
    }

    #[test]
    fn a_listing_gives_each_keys_etag_unquoted() {
        let page = "<ListBucketResult><Contents><Key>a</Key><Size>3</Size>\
<ETag>&quot;0CC175B9C0F1B6A831C399E269772661&quot;</ETag></Contents><Contents><Key>b</Key>\
<Size>4</Size><ETag>\"92eb5ffee6ae2fec3ad71c777531578f\"</ETag></Contents>\
<IsTruncated>false</IsTruncated></ListBucketResult>";
        let (entries, _) = parse_listing_tagged(page);
        let tags: Vec<(&str, u64, &str)> = entries
            .iter()
            .map(|listed| (listed.key.as_str(), listed.size, listed.etag.as_str()))
            .collect();
        assert_eq!(
            tags,
            [
                ("a", 3, "0cc175b9c0f1b6a831c399e269772661"),
                ("b", 4, "92eb5ffee6ae2fec3ad71c777531578f")
            ]
        );
    }

    #[test]
    fn the_transaction_timeout_is_left_out_only_for_a_server_older_than_17() {
        let sql =
            "SET statement_timeout = 0;\nSET transaction_timeout = 0;\nSET lock_timeout = 0;\n";
        assert_eq!(server_major("16.15"), 16);
        assert_eq!(server_major("18.6 (Ubuntu)"), 18);
        assert!(!without_transaction_timeout(sql, 16).contains("transaction_timeout"));
        assert!(without_transaction_timeout(sql, 16).contains("statement_timeout"));
        assert!(without_transaction_timeout(sql, 17).contains("transaction_timeout"));
        assert!(without_transaction_timeout(sql, 18).contains("transaction_timeout"));
    }

    #[test]
    fn the_transcript_window_is_the_last_days() {
        let root = std::env::temp_dir().join(format!(
            "spawn-backup-window-{}-{}",
            std::process::id(),
            super::random_hex(4).expect("random")
        ));
        fs::create_dir_all(root.join("agent_a")).expect("mkdir");
        fs::write(root.join("agent_a/new.jsonl"), b"x").expect("write");
        fs::write(root.join("agent_a/old.jsonl"), b"y").expect("write");
        let old = std::time::SystemTime::now() - std::time::Duration::from_hours(20 * 24);
        let file = fs::File::options()
            .write(true)
            .open(root.join("agent_a/old.jsonl"))
            .expect("open");
        file.set_modified(old).expect("touch");
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(0));
        let files = window_files(&root, now_ms, 14).expect("window");
        let names: Vec<&str> = files.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(
            names,
            ["agent_a/new.jsonl"],
            "the 20-day-old file is outside"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_records_shape_a_manifest_the_restore_can_read() {
        // The manifest's shape, as the scenario reads it: paths and offsets
        // for the transcripts, a file and the heads for each source.
        let manifest = json!({
            "kind": "incremental",
            "base_event_id": 7,
            "transcripts": [{"path": "a/x.jsonl", "offset": 100, "bytes": 5, "size": 105}],
            "repos": [{"source": "/s.git", "file": "repos/s.git.bundle",
                       "heads": {"refs/heads/main": "abc"}, "unchanged": false}],
        });
        assert_eq!(manifest["transcripts"][0]["offset"], 100);
        assert_eq!(manifest["repos"][0]["file"], "repos/s.git.bundle");
        assert_eq!(manifest["repos"][0]["heads"]["refs/heads/main"], "abc");
    }

    /// A made-up key's content: what a misplaced value could hold.
    const MADE_UP_SECRET: &str = "bb-made-up-passphrase-0123456789";

    /// Objective 114: None is k1; `k` and a number from 1 is itself;
    /// anything else is the rule alone, never the value.
    #[test]
    fn a_key_name_is_k_and_a_number_from_one_and_absent_is_k1() {
        use super::{KEY_NAME_RULE, key_name};
        assert_eq!(key_name(None).as_deref(), Ok("k1"));
        assert_eq!(key_name(Some("k2")).as_deref(), Ok("k2"));
        assert_eq!(key_name(Some("k12")).as_deref(), Ok("k12"));
        for bad in [
            "k0",
            "K2",
            "k2/..",
            "",
            "k",
            "k01",
            " k2",
            "k2 ",
            MADE_UP_SECRET,
        ] {
            let error = key_name(Some(bad)).expect_err(bad);
            assert_eq!(error, KEY_NAME_RULE, "{bad:?}");
        }
    }

    /// k1's file: SPAWN_BACKUP_KEY_FILE when set (missing: that path), else
    /// backup.key.k1, else the old backup.key; another key only by its name.
    #[test]
    fn k1_is_the_key_file_then_its_new_name_then_the_old_and_others_only_their_own() {
        use super::find_key;
        let dir = crate::storage::tests::scratch("backup-keys");
        let old = dir.join("backup.key");
        let new = dir.join("backup.key.k1");
        let given = dir.join("given");
        let named = |result: Result<super::Key, String>| result.map(|key| key.file);
        assert_eq!(
            named(find_key("k1", None, &dir)),
            Err(format!("{} (or backup.key)", new.display()))
        );
        std::fs::write(&old, MADE_UP_SECRET).expect("old");
        assert_eq!(named(find_key("k1", None, &dir)), Ok(old.clone()));
        std::fs::write(&new, MADE_UP_SECRET).expect("new");
        assert_eq!(named(find_key("k1", None, &dir)), Ok(new.clone()));
        assert_eq!(
            named(find_key("k1", Some(given.clone()), &dir)),
            Err(given.display().to_string()),
            "a set SPAWN_BACKUP_KEY_FILE is the only place looked"
        );
        std::fs::write(&given, MADE_UP_SECRET).expect("given");
        assert_eq!(named(find_key("k1", Some(given.clone()), &dir)), Ok(given));
        let k2 = dir.join("backup.key.k2");
        assert_eq!(
            named(find_key("k2", None, &dir)),
            Err(k2.display().to_string()),
            "k2 is never k1's file"
        );
        std::fs::write(&k2, MADE_UP_SECRET).expect("k2");
        assert_eq!(named(find_key("k2", None, &dir)), Ok(k2));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A record and its object name their key; absent is k1 on both; two
    /// names are refused, and a value that is no name is refused by where it
    /// is, never echoed.
    #[test]
    fn a_record_and_an_object_that_name_two_keys_are_refused() {
        use super::{object_key, record_key, same_key};
        let record = serde_json::json!({"sealed_with": "k2"});
        assert_eq!(record_key("backup.dumped", 7, &record).as_deref(), Ok("k2"));
        assert_eq!(
            record_key("backup.dumped", 7, &serde_json::json!({})).as_deref(),
            Ok("k1")
        );
        assert_eq!(object_key("o", "etag: x\r\n").as_deref(), Ok("k1"));
        assert_eq!(
            object_key("o", "x-amz-meta-spawn-seal: k2\r\n").as_deref(),
            Ok("k2")
        );
        assert_eq!(same_key("k2", "k2"), Ok(()));
        assert_eq!(
            same_key("k2", "k1"),
            Err("record says k2, object says k1".to_string())
        );
        let planted = serde_json::json!({"sealed_with": MADE_UP_SECRET});
        assert_eq!(
            record_key("backup.bundled", 41, &planted),
            Err("the sealed_with of backup.bundled record 41 is not a backup key name (k and a number from 1, as k2)".to_string())
        );
        let header = format!("x-amz-meta-spawn-seal: {MADE_UP_SECRET}\r\n");
        let error = object_key("postgres/hourly/x.dump.gpg", &header).expect_err("refused");
        assert_eq!(
            error,
            "the x-amz-meta-spawn-seal of postgres/hourly/x.dump.gpg is not a backup key name (k and a number from 1, as k2)"
        );
    }

    /// After the switch, a bundle sealed with k2 builds on k2's records
    /// only: with none yet its earlier records are empty, so it is full,
    /// whatever k1 full this week holds; then the next builds on it.
    #[test]
    fn the_first_bundle_under_a_new_key_builds_on_nothing() {
        use super::sealed_alike;
        let entry = |id: i64, record: serde_json::Value| super::BundleRecord {
            event_id: id,
            at_ms: id,
            record,
        };
        let k1_full = || {
            entry(
                1,
                serde_json::json!({"kind": "full", "keys": ["bundles/full/a"]}),
            )
        };
        let k1_incr = || {
            entry(
                2,
                serde_json::json!({"kind": "incremental", "sealed_with": "k1", "keys": ["b"]}),
            )
        };
        assert!(sealed_alike(vec![k1_full(), k1_incr()], "k2").is_empty());
        assert_eq!(sealed_alike(vec![k1_full(), k1_incr()], "k1").len(), 2);
        let k2_full = entry(
            3,
            serde_json::json!({"kind": "full", "sealed_with": "k2", "keys": ["c"]}),
        );
        let earlier = sealed_alike(vec![k1_full(), k1_incr(), k2_full], "k2");
        assert_eq!(earlier.iter().map(|e| e.event_id).collect::<Vec<_>>(), [3]);
        let planted = entry(
            4,
            serde_json::json!({"sealed_with": MADE_UP_SECRET, "keys": ["d"]}),
        );
        assert!(
            sealed_alike(vec![planted], "k1").is_empty(),
            "a refused name is no one's"
        );
    }

    /// The store seals with the sealing key's file: required, a bad name or
    /// a missing file refuses it, never k1's file in its place; not
    /// required (a reader), k1's file stands in.
    #[test]
    fn the_stores_sealing_key_is_required_and_never_k1_in_its_place() {
        use super::{Key, store_key_file};
        let k1 = std::path::PathBuf::from("/made-up/backup.key.k1");
        let k2 = Key {
            name: "k2".to_string(),
            file: "/made-up/backup.key.k2".into(),
        };
        let refused: Result<Key, String> =
            Err(format!("SPAWN_BACKUP_SEAL_KEY {}", super::KEY_NAME_RULE));
        assert_eq!(
            store_key_file(&Ok(k2.clone()), true, || k1.clone()),
            Ok(k2.file.clone())
        );
        assert_eq!(
            store_key_file(&refused, true, || k1.clone()),
            Err(refused.clone().unwrap_err()),
            "required: refused, never k1"
        );
        assert_eq!(
            store_key_file(&refused, false, || k1.clone()),
            Ok(k1.clone())
        );
        assert_eq!(
            store_key_file(&Ok(k2.clone()), false, || k1.clone()),
            Ok(k2.file)
        );
    }

    /// A one-object HTTP stand-in on 127.0.0.1 for the GET tests (t494u):
    /// `behaviour` decides each request's answer from its index and its
    /// Range; every request's Range (or "-") is returned through the log.
    fn serve_object(
        body: Vec<u8>,
        etag: String,
        behaviour: fn(usize, Option<u64>) -> Answer,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use std::fmt::Write as _;
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = log.clone();
        std::thread::spawn(move || {
            for (index, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                let mut range: Option<u64> = None;
                let mut if_match: Option<String> = None;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("if-match")
                    {
                        if_match = Some(value.trim().trim_matches('"').to_string());
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("range")
                    {
                        range = value
                            .trim()
                            .trim_start_matches("bytes=")
                            .trim_end_matches('-')
                            .parse()
                            .ok();
                    }
                }
                let mark = match &if_match {
                    None => "",
                    Some(tag) if *tag == etag => "+if-match",
                    Some(_) => "+if-match-other",
                };
                seen.lock().expect("log").push(format!(
                    "{}{mark}",
                    range.map_or("-".to_string(), |at| at.to_string())
                ));
                let total = body.len();
                let from_range = || usize::try_from(range.unwrap_or(0)).expect("range");
                let (status, from, send) = match behaviour(index, range) {
                    Answer::Whole => (200, 0, total),
                    Answer::WholeCutAt(cut) => (200, 0, cut),
                    Answer::Ranged => (206, from_range(), total - from_range()),
                    Answer::RangedCutAfter(bytes) => {
                        (206, from_range(), bytes.min(total - from_range()))
                    }
                    Answer::NotFound => {
                        let _ = stream.write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                        continue;
                    }
                };
                let mut head = format!(
                    "HTTP/1.1 {status} {}\r\nETag: \"{etag}\"\r\nx-amz-meta-sha256: made-up\r\nContent-Length: {}\r\nConnection: close\r\n",
                    if status == 206 {
                        "Partial Content"
                    } else {
                        "OK"
                    },
                    total - from
                );
                if status == 206 {
                    let _ = write!(
                        head,
                        "Content-Range: bytes {from}-{}/{total}\r\n",
                        total - 1
                    );
                }
                head.push_str("\r\n");
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body[from..from + send]);
                let _ = stream.flush();
            }
        });
        (format!("http://{address}"), log)
    }

    enum Answer {
        Whole,
        WholeCutAt(usize),
        Ranged,
        RangedCutAfter(usize),
        NotFound,
    }

    fn no_pause(_attempt: u32) -> u64 {
        0
    }

    fn made_up_object() -> Vec<u8> {
        (0..300_000u32)
            .map(|index| (index * 7 % 251) as u8)
            .collect()
    }

    #[test]
    fn a_cut_get_resumes_from_its_bytes_and_checks_the_whole() {
        // The first answer is cut after 100,000 of 300,000 bytes, as the
        // 16:31 drill's was; the second asks for the rest with a Range.
        let body = made_up_object();
        let etag = super::md5_hex(&body);
        let (endpoint, log) = serve_object(body.clone(), etag, |index, _| {
            if index == 0 {
                Answer::WholeCutAt(100_000)
            } else {
                Answer::Ranged
            }
        });
        let mut settings = made_up_r2_settings(None);
        settings.r2.endpoint = endpoint;
        let (got, headers) =
            super::get_object_with(&settings, "bundles/made-up.tar.zst.gpg", no_pause)
                .expect("the GET")
                .expect("the object");
        assert_eq!(got, body);
        assert!(
            headers.to_ascii_lowercase().contains("x-amz-meta-sha256"),
            "{headers}"
        );
        let requests = log.lock().expect("log").clone();
        // The resumed request asks for the same object (If-Match, rev-b n3).
        assert_eq!(requests, ["-", "100000+if-match"]);
    }

    #[test]
    fn a_joined_get_whose_bytes_are_not_its_etag_fails() {
        // A cut, then the rest by Range: the joined body is checked against
        // the ETag, and a mismatch is an error, never the body.
        let body = made_up_object();
        let (endpoint, log) = serve_object(body, "0".repeat(32), |index, _| {
            if index == 0 {
                Answer::WholeCutAt(100_000)
            } else {
                Answer::Ranged
            }
        });
        let mut settings = made_up_r2_settings(None);
        settings.r2.endpoint = endpoint;
        let error = super::get_object_with(&settings, "bundles/made-up.tar.zst.gpg", no_pause)
            .expect_err("a wrong MD5");
        assert!(error.contains("not the ETag"), "{error}");
        assert_eq!(log.lock().expect("log").len(), 2);
    }

    #[test]
    fn a_whole_answer_is_left_to_the_callers_sha256_checks() {
        // One whole answer whose ETag is not its MD5 (scenario 186 (7):
        // the stored bytes replaced, the object's metadata left): the GET
        // returns what the server sent, so the caller's decrypted-sha256
        // check names the failure, as it did before resumable GETs.
        let body = made_up_object();
        let (endpoint, log) = serve_object(body.clone(), "0".repeat(32), |_, _| Answer::Whole);
        let mut settings = made_up_r2_settings(None);
        settings.r2.endpoint = endpoint;
        let (got, _) = super::get_object_with(&settings, "bundles/made-up.tar.zst.gpg", no_pause)
            .expect("the GET")
            .expect("the object");
        assert_eq!(got, body);
        assert_eq!(log.lock().expect("log").len(), 1);
    }

    #[test]
    fn a_server_that_ignores_the_range_gets_the_whole_again() {
        // A cut, then a 200 to the Range (curl exit 33): the partial body is
        // dropped and the next attempt takes the whole object.
        let body = made_up_object();
        let etag = super::md5_hex(&body);
        let (endpoint, log) = serve_object(body.clone(), etag, |index, _| match index {
            0 => Answer::WholeCutAt(50_000),
            _ => Answer::Whole,
        });
        let mut settings = made_up_r2_settings(None);
        settings.r2.endpoint = endpoint;
        let (got, _) = super::get_object_with(&settings, "bundles/made-up.tar.zst.gpg", no_pause)
            .expect("the GET")
            .expect("the object");
        assert_eq!(got, body);
        let requests = log.lock().expect("log").clone();
        assert_eq!(requests.first().map(String::as_str), Some("-"));
        assert_eq!(requests.last().map(String::as_str), Some("-"));
    }

    #[test]
    fn a_get_that_never_completes_fails_and_returns_no_part() {
        // rev-b F1: every answer is cut, and the ETag is multipart (no MD5
        // to catch a short body): six attempts, then an error naming the
        // bytes, never the partial body as the object.
        let body = made_up_object();
        let etag = "0123456789abcdef0123456789abcdef-2".to_string();
        let (endpoint, log) = serve_object(body, etag, |index, _| {
            if index == 0 {
                Answer::WholeCutAt(10_000)
            } else {
                Answer::RangedCutAfter(10_000)
            }
        });
        let mut settings = made_up_r2_settings(None);
        settings.r2.endpoint = endpoint;
        let error = super::get_object_with(&settings, "bundles/made-up.tar.zst.gpg", no_pause)
            .expect_err("an incomplete object");
        assert!(
            error.contains("60000 of 300000 bytes after 6 attempts"),
            "{error}"
        );
        assert_eq!(log.lock().expect("log").len(), 6);
    }

    #[test]
    fn a_missing_object_is_none_after_one_request() {
        // rev-b F2: a 404 is "no such object" (None), asked once.
        let (endpoint, log) =
            serve_object(made_up_object(), "x".to_string(), |_, _| Answer::NotFound);
        let mut settings = made_up_r2_settings(None);
        settings.r2.endpoint = endpoint;
        let found = super::get_object_with(&settings, "bundles/none.tar.zst.gpg", no_pause)
            .expect("a 404 is no error");
        assert!(found.is_none());
        assert_eq!(log.lock().expect("log").len(), 1);
    }

    /// Objective 136: a made-up bucket's settings, for a config read and
    /// never sent.
    fn made_up_r2_settings(seal: Option<&str>) -> Settings {
        Settings {
            root: "/made-up".into(),
            key_file: "/made-up/backup.key.k1".into(),
            seal: seal.map_or_else(
                || Err("no sealing key in this test".to_string()),
                |name| {
                    Ok(super::Key {
                        name: name.to_string(),
                        file: format!("/made-up/backup.key.{name}").into(),
                    })
                },
            ),
            r2: R2 {
                endpoint: "https://made-up.r2.example".to_string(),
                bucket: "made-up-store".to_string(),
                access_key: "made-up-access-id-136".to_string(),
                secret_key: MADE_UP_SECRET.to_string(),
            },
            pg_bin: None,
            compress: "zstd:9".to_string(),
            transcript_days: 14,
            limit_bytes: 1,
            alert_bytes: 1,
            prefix: String::new(),
            drill_url: None,
        }
    }

    /// A config's `header = "Name: value"` lines, as (lower-cased name,
    /// value), in order.
    fn config_headers(config: &str) -> Vec<(String, String)> {
        config
            .lines()
            .filter_map(|line| {
                let quoted = line.strip_prefix("header = \"")?.strip_suffix('"')?;
                let (name, value) = quoted.split_once(':')?;
                Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
            })
            .collect()
    }

    /// SigV4's canonical request for a GET of `url` with the config's
    /// headers: the query's parameters sorted by name, every header signed,
    /// and the payload hash `x-amz-content-sha256` names (as an s3 signer
    /// takes it). Returns it with the query as sent.
    fn canonical_request(url: &str, config: &str) -> (String, String) {
        use std::fmt::Write as _;
        let rest = url.split_once("://").expect("scheme").1;
        let (host, path_and_query) = rest.split_once('/').expect("path");
        let (path, sent) = path_and_query
            .split_once('?')
            .unwrap_or((path_and_query, ""));
        let mut params: Vec<(&str, &str)> = sent
            .split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| pair.split_once('=').unwrap_or((pair, "")))
            .collect();
        params.sort_unstable();
        let canonical_query = params
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("&");
        let mut headers = config_headers(config);
        headers.push(("host".to_string(), host.to_string()));
        headers.sort();
        let payload = headers
            .iter()
            .find(|(name, _)| name == "x-amz-content-sha256")
            .map_or("(no payload hash)", |(_, value)| value.as_str())
            .to_string();
        let canonical_headers = headers
            .iter()
            .fold(String::new(), |mut out, (name, value)| {
                let _ = writeln!(out, "{name}:{value}");
                out
            });
        let signed = headers
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(";");
        (
            format!("GET\n/{path}\n{canonical_query}\n{canonical_headers}\n{signed}\n{payload}"),
            sent.to_string(),
        )
    }

    /// Objective 136: both listings' queries, first page and later, with
    /// and without a prefix, are already in SigV4's sorted order (a curl
    /// that signs the query as written signs what the bucket checks), and
    /// the payload hash signed is UNSIGNED-PAYLOAD, from the config's header.
    #[test]
    fn a_listing_query_is_sent_sorted_and_signs_an_unsigned_payload() {
        use super::{curl_config, listing_query};
        let settings = made_up_r2_settings(None);
        let token = "1/made up+token=";
        let cases = [
            (None, None, "?list-type=2&max-keys=1000"),
            (
                None,
                Some(token),
                "?continuation-token=1%2Fmade%20up%2Btoken%3D&list-type=2&max-keys=1000",
            ),
            (
                Some("store/manifests/"),
                None,
                "?list-type=2&max-keys=1000&prefix=store%2Fmanifests%2F",
            ),
            (
                Some("store/manifests/"),
                Some(token),
                "?continuation-token=1%2Fmade%20up%2Btoken%3D&list-type=2&max-keys=1000\
&prefix=store%2Fmanifests%2F",
            ),
        ];
        for (prefix, token, expected) in cases {
            let query = listing_query(prefix, token);
            assert_eq!(query, expected, "{prefix:?} {token:?}");
            let url = settings.r2.url(&query);
            let config = curl_config(
                &settings,
                &url,
                "",
                std::path::Path::new("/made-up/body"),
                std::path::Path::new("/made-up/headers"),
            );
            let (canonical, sent) = canonical_request(&url, &config);
            let canonical_query = canonical.lines().nth(2).unwrap_or_default();
            assert_eq!(
                canonical_query, sent,
                "sorting changed the query of {prefix:?} {token:?}"
            );
            if token.is_some() {
                assert!(sent.starts_with("continuation-token="), "{sent}");
            }
            assert!(
                canonical.contains("\nx-amz-content-sha256:UNSIGNED-PAYLOAD\n"),
                "the payload hash header is not signed"
            );
            assert!(
                canonical.ends_with("\nUNSIGNED-PAYLOAD"),
                "the payload hash signed is not UNSIGNED-PAYLOAD"
            );
        }
    }

    /// Objective 136: every bucket request's config (a GET, a HEAD, a
    /// conditional PUT) names x-amz-content-sha256: UNSIGNED-PAYLOAD next to
    /// SigV4 and before the call's own headers, and no credential is in a
    /// header.
    #[test]
    fn every_bucket_request_names_its_payload_hash_and_no_credential_in_a_header() {
        use super::{PAYLOAD_HASH_HEADER, curl_config, put_new_options};
        assert_eq!(
            PAYLOAD_HASH_HEADER,
            "x-amz-content-sha256: UNSIGNED-PAYLOAD"
        );
        let settings = made_up_r2_settings(Some("k2"));
        let put = put_new_options(
            &settings,
            std::path::Path::new("/made-up/sealed.gpg"),
            "ab12",
        );
        assert!(
            put.contains("header = \"If-None-Match: *\"\n"),
            "not conditional"
        );
        for (what, options) in [("GET", ""), ("HEAD", "head\n"), ("PUT", put.as_str())] {
            let config = curl_config(
                &settings,
                &settings.r2.url("store/manifests/x.json.gpg"),
                options,
                std::path::Path::new("/made-up/body"),
                std::path::Path::new("/made-up/headers"),
            );
            let headers = config_headers(&config);
            let hashes: Vec<&str> = headers
                .iter()
                .filter(|(name, _)| name == "x-amz-content-sha256")
                .map(|(_, value)| value.as_str())
                .collect();
            assert_eq!(hashes, ["UNSIGNED-PAYLOAD"], "{what}");
            let lines: Vec<&str> = config.lines().collect();
            let sigv4 = lines
                .iter()
                .position(|line| line.starts_with("aws-sigv4 = "))
                .expect("aws-sigv4");
            assert_eq!(
                lines.get(sigv4 + 1).copied(),
                Some("header = \"x-amz-content-sha256: UNSIGNED-PAYLOAD\""),
                "{what}: not next to aws-sigv4"
            );
            let first_header = lines
                .iter()
                .position(|line| line.starts_with("header = "))
                .expect("a header");
            assert_eq!(
                first_header,
                sigv4 + 1,
                "{what}: after the call's own headers"
            );
            for (name, value) in &headers {
                for credential in [&settings.r2.access_key, &settings.r2.secret_key] {
                    assert!(
                        !value.contains(credential.as_str()) && !name.contains(credential.as_str()),
                        "{what}: a credential is in the {name} header"
                    );
                }
            }
        }
    }

    #[test]
    fn a_get_resumes_cuts_stalls_and_caps_by_what_is_left() {
        let first = get_options(0, None);
        assert!(first.contains("max-time = \"4696\""), "{first}");
        assert!(first.contains("speed-limit = \"4096\"") && first.contains("speed-time = \"60\""));
        assert!(!first.contains("continue-at"), "{first}");
        // 146,466,171 bytes, 92,749,617 already there: resume, cap by the rest.
        let rest = get_options(92_749_617, Some(146_466_171 - 92_749_617));
        assert!(rest.contains("continue-at = \"92749617\""), "{rest}");
        assert!(rest.contains("max-time = \"1419\""), "{rest}");
        assert_eq!(
            (1..=5).map(get_pause_secs).collect::<Vec<_>>(),
            [5, 15, 30, 60, 120]
        );
    }

    #[test]
    fn an_answer_says_the_whole_length() {
        let range = "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 92749617-146466170/146466171\r\nContent-Length: 53716554\r\n";
        assert_eq!(object_length(206, range), Some(146_466_171));
        let whole = "HTTP/1.1 200 OK\r\nContent-Length: 146466171\r\n";
        assert_eq!(object_length(200, whole), Some(146_466_171));
        assert_eq!(object_length(206, whole), None);
        assert_eq!(object_length(404, whole), None);
        assert!(is_plain_md5("0123456789abcdef0123456789abcdef"));
        assert!(!is_plain_md5("0123456789abcdef0123456789abcdef-3"));
        assert!(!is_plain_md5(""));
        let cut = "HTTP/1.1 200 OK\r\nContent-Length: 146466171\r\n\r\n";
        assert_eq!(super::last_status(cut), Some(200));
        assert_eq!(super::last_status(""), None);
        let late = "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */146466171\r\n";
        assert_eq!(super::last_status(late), Some(416));
        assert_eq!(super::unsatisfiable_length(late), Some(146_466_171));
    }

    /// Objective 167: two bundles in one second (or one millisecond) name
    /// two objects, and the names sort, by byte, as the bundles ran.
    #[test]
    fn bundles_in_one_second_get_names_that_sort_as_they_ran() {
        use super::bundle_name;
        // 2026-11-14T03:05:09.042Z
        let at = 1_763_089_509_042_i64;
        assert_eq!(
            bundle_name("full", at, "tar.zst", "0a1b2c3d"),
            "bundles/full/spawn-bundle-20251114-030509-042-0a1b2c3d.tar.zst.gpg"
        );
        let first = bundle_name("incr", at, "tar.zst", "ffffffff");
        let later_ms = bundle_name("incr", at + 1, "tar.zst", "00000000");
        let same_ms = bundle_name("incr", at, "tar.zst", "00000001");
        assert_ne!(first, later_ms, "one second, two names");
        assert_ne!(
            first, same_ms,
            "one millisecond, named apart by the random part"
        );
        // Times that cross millisecond widths, a second, an hour and a day,
        // their random parts descending: the names sort as the times.
        let times = [
            at - 42,
            at - 41,
            at - 33,
            at,
            at + 57,
            at + 958,
            at + 1_000,
            at + 3_600_000,
            at + 86_400_000,
        ];
        let randoms = [
            "ffffffff", "eeeeeeee", "dddddddd", "cccccccc", "bbbbbbbb", "aaaaaaaa", "99999999",
            "88888888", "77777777",
        ];
        let names: Vec<String> = times
            .iter()
            .zip(randoms)
            .map(|(time, random)| bundle_name("incr", *time, "tar.zst", random))
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(sorted, names, "by byte, as they ran");
        // An earlier second's one-second name (before objective 167) sorts
        // first.
        let old = format!(
            "bundles/incr/spawn-bundle-{}.tar.zst.gpg",
            Stamp::at(at - 1_000).text()
        );
        assert!(old < first, "{old} {first}");
        // The second is read from the name, under a prefix too.
        assert_eq!(
            Stamp::parse(&first).map(|stamp| stamp.text()),
            Some(Stamp::at(at).text())
        );
        assert_eq!(
            Stamp::parse(&format!("locktest/{first}")).map(|stamp| stamp.text()),
            Some("20251114-030509".to_string())
        );
    }

    /// Objective 216 (9): a bundle's PUT is given 600 s plus its size at
    /// 100 KB/s, sized to the object; a small one keeps the 600 s every
    /// request has.
    #[test]
    fn a_bundle_put_is_given_600_s_plus_its_size_at_100_kb_s() {
        assert_eq!(super::bundle_put_seconds(0), 600);
        assert_eq!(super::bundle_put_seconds(99_999), 600);
        assert_eq!(super::bundle_put_seconds(229_000_000), 600 + 2290);
    }

    /// Objective 216 (8): a bundle scratch a killed run left in the
    /// workspace root goes, and nothing else there.
    #[test]
    fn a_bundle_scratch_a_killed_run_left_is_swept() {
        let root = std::env::temp_dir().join(format!("spawn-bundle-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".spawn-backup-bundle-41-0a1b2c3d0/stage/sync"))
            .expect("left");
        std::fs::create_dir_all(root.join(".transcripts")).expect("transcripts");
        std::fs::write(root.join(".backup.lock"), "").expect("lock");
        super::sweep_bundle_scratch(&root);
        assert!(!root.join(".spawn-backup-bundle-41-0a1b2c3d0").exists());
        assert!(root.join(".transcripts").is_dir());
        assert!(root.join(".backup.lock").is_file());
        let made = super::Scratch::new_in(&root, super::BUNDLE_SCRATCH).expect("scratch");
        assert!(made.path.starts_with(&root));
        assert!(
            made.path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(".spawn-backup-bundle-"))
        );
        drop(made);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn wave_k3_r2_prefers_keep_credentials_and_fails_closed() {
        use super::resolve_backup_r2;
        use std::os::unix::fs::PermissionsExt;

        let dir = crate::storage::tests::scratch("backup-wave-k3-r2");
        let home = dir.join("home");
        let spawn_cfg = home.join(".config/spawn");
        let cred_dir = dir.join("cred");
        std::fs::create_dir_all(&spawn_cfg).expect("cfg");
        std::fs::create_dir_all(&cred_dir).expect("cred_dir");

        let legacy_key_id = "legacy_id_221";
        let legacy_secret = "legacy_secret_221";
        let r2_env = spawn_cfg.join("r2.env");
        std::fs::write(
            &r2_env,
            format!(
                "SPAWN_R2_ENDPOINT=https://legacy.r2.example\nSPAWN_R2_BUCKET=spawn-store\n\
                 SPAWN_R2_ACCESS_KEY_ID={legacy_key_id}\nSPAWN_R2_SECRET_ACCESS_KEY={legacy_secret}\n"
            ),
        )
        .expect("r2.env");
        std::fs::set_permissions(&r2_env, std::fs::Permissions::from_mode(0o600)).expect("mode");

        // No r2.credentials in cred_dir -> smooth fallback to r2.env.
        let legacy = resolve_backup_r2(Some(&cred_dir), None, &home, |_| None).expect("fallback");
        assert_eq!(legacy.access_key, legacy_key_id);
        assert_eq!(legacy.bucket, "spawn-store");

        // Keep r2.credentials (0400) overrides legacy r2.env + env.
        let keep_id = "keep_id_994";
        let keep_secret = "keep_secret_994";
        let cred_r2 = cred_dir.join("r2.credentials");
        std::fs::write(
            &cred_r2,
            format!(
                "SPAWN_R2_ENDPOINT=https://keep.r2.example\nSPAWN_R2_BUCKET=keep-bkt\n\
                 SPAWN_R2_ACCESS_KEY_ID={keep_id}\nSPAWN_R2_SECRET_ACCESS_KEY={keep_secret}\n"
            ),
        )
        .expect("cred r2");
        std::fs::set_permissions(&cred_r2, std::fs::Permissions::from_mode(0o400)).expect("mode");
        let keep = resolve_backup_r2(Some(&cred_dir), Some(r2_env.clone()), &home, |_| {
            Some("ignored-env".into())
        })
        .expect("keep r2");
        assert_eq!(keep.access_key, keep_id);
        assert_eq!(keep.secret_key, keep_secret);
        assert_eq!(keep.bucket, "keep-bkt");

        // Missing secret key inside r2.credentials fails closed (never falls back to env/r2.env).
        std::fs::set_permissions(&cred_r2, std::fs::Permissions::from_mode(0o600)).expect("600");
        std::fs::write(
            &cred_r2,
            format!("SPAWN_R2_ENDPOINT=https://keep.r2.example\nSPAWN_R2_BUCKET=keep-bkt\nSPAWN_R2_ACCESS_KEY_ID={keep_id}\n"),
        )
        .expect("partial r2");
        std::fs::set_permissions(&cred_r2, std::fs::Permissions::from_mode(0o400)).expect("400");
        let err_missing = resolve_backup_r2(Some(&cred_dir), Some(r2_env), &home, |_| {
            Some("env-secret-must-not-rescue".into())
        })
        .unwrap_err();
        assert!(
            err_missing.starts_with("credential r2.credentials"),
            "{err_missing}"
        );
        assert!(!err_missing.contains(keep_id));

        // Bad permissions on r2.credentials fail closed without leaking secrets.
        std::fs::set_permissions(&cred_r2, std::fs::Permissions::from_mode(0o600)).expect("600");
        std::fs::write(
            &cred_r2,
            format!("SPAWN_R2_ACCESS_KEY_ID={keep_id}\nSPAWN_R2_SECRET_ACCESS_KEY={keep_secret}\n"),
        )
        .expect("restore");
        std::fs::set_permissions(&cred_r2, std::fs::Permissions::from_mode(0o644)).expect("644");
        let err_mode = resolve_backup_r2(Some(&cred_dir), None, &home, |_| None).unwrap_err();
        assert!(
            err_mode.starts_with("credential r2.credentials"),
            "{err_mode}"
        );
        assert!(!err_mode.contains(keep_secret));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wave_k3_backup_key_prefers_keep_credentials_with_fallback() {
        use super::find_key_with_credentials;
        use std::os::unix::fs::PermissionsExt;

        let dir = crate::storage::tests::scratch("backup-wave-k3-key");
        let key_dir = dir.join("keys");
        let cred_dir = dir.join("cred");
        std::fs::create_dir_all(&key_dir).expect("key_dir");
        std::fs::create_dir_all(&cred_dir).expect("cred_dir");

        // Encryption key lookup: backup.key.k1 > backup.encryption-key > fallback.
        let disk_k1 = key_dir.join("backup.key.k1");
        std::fs::write(&disk_k1, MADE_UP_SECRET).expect("disk k1");
        std::fs::set_permissions(&disk_k1, std::fs::Permissions::from_mode(0o600)).expect("mode");
        assert_eq!(
            find_key_with_credentials("k1", Some(&cred_dir), None, &key_dir)
                .unwrap()
                .file,
            disk_k1
        );

        let cred_alias = cred_dir.join("backup.encryption-key");
        std::fs::write(&cred_alias, MADE_UP_SECRET).expect("alias");
        std::fs::set_permissions(&cred_alias, std::fs::Permissions::from_mode(0o400)).expect("400");
        assert_eq!(
            find_key_with_credentials("k1", Some(&cred_dir), None, &key_dir)
                .unwrap()
                .file,
            cred_alias
        );

        let primary_cred = cred_dir.join("backup.key.k1");
        std::fs::write(&primary_cred, MADE_UP_SECRET).expect("cred k1");
        std::fs::set_permissions(&primary_cred, std::fs::Permissions::from_mode(0o400))
            .expect("400");
        assert_eq!(
            find_key_with_credentials("k1", Some(&cred_dir), Some(disk_k1), &key_dir)
                .unwrap()
                .file,
            primary_cred
        );
        // k2 never consumes k1's alias backup.encryption-key.
        assert!(find_key_with_credentials("k2", Some(&cred_dir), None, &key_dir).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
