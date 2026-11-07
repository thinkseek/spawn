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
//!   source, whole or `--not` its base's heads, and the transcript archive's
//!   files modified within `SPAWN_BACKUP_TRANSCRIPT_DAYS` (default 14),
//!   whole or the bytes appended since the base's sizes.
//! - `spawn backup verify`, every day: the newest good dump fetched,
//!   decrypted and checked against its record's sha256, restored into a
//!   database of its own on `SPAWN_BACKUP_DRILL_DATABASE_URL` (never the
//!   source, never one it did not create), its `spawn_events` and highest id
//!   compared with the record's, and the database dropped again: exit 0 when
//!   it restores to its record, 1 when it does not, 2 when it cannot run.
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
//! Secrets come from files the owner made mode 600 and are never on a
//! command line, in the output or in a record: the passphrase file goes to
//! gpg by path, the R2 credentials to curl on stdin (`--config -`), and the
//! database password to libpq through `PGPASSWORD`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use spawn_postgres::PostgresStore;

use crate::config::{env_or, env_value, open_store, workspace_root};
use crate::Flags;

/// The default limits, in decimal megabytes (1,000,000 bytes each): R2's
/// free tier is 10 GB-month of storage, and the owner's alert is at 8 GB.
const DEFAULT_LIMIT_MB: u64 = 10_000;
const DEFAULT_ALERT_MB: u64 = 8_000;

/// A byte count per `SPAWN_BACKUP_R2_*` megabyte.
const MB: u64 = 1_000_000;

const USAGE: &str = "usage: spawn backup dump|bundle|verify [--root <dir>]";

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
    pub(crate) key_file: PathBuf,
    pub(crate) r2: R2,
    pub(crate) pg_bin: Option<PathBuf>,
    pub(crate) compress: String,
    pub(crate) transcript_days: i64,
    pub(crate) limit_bytes: u64,
    pub(crate) alert_bytes: u64,
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
    let meta = fs::metadata(path)
        .map_err(|error| format!("{what} {}: {error}", path.display()))?;
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

impl Settings {
    pub(crate) fn from_env(flags: &Flags) -> Result<Self, String> {
        let root = workspace_root(flags)
            .map(PathBuf::from)
            .ok_or_else(|| "--root or SPAWN_WORKSPACE_ROOT is required".to_string())?;
        let key_file = env_value("SPAWN_BACKUP_KEY_FILE")
            .map_or_else(|| home().join(".config/spawn/backup.key"), PathBuf::from);
        private_file(&key_file, "the backup key")?;
        let env_file = env_value("SPAWN_R2_ENV_FILE")
            .map_or_else(|| home().join(".config/spawn/r2.env"), PathBuf::from);
        private_file(&env_file, "the R2 environment")?;
        let vars = parse_env_file(
            &fs::read_to_string(&env_file)
                .map_err(|error| format!("the R2 environment {}: {error}", env_file.display()))?,
        );
        let needed = |name: &str| -> Result<String, String> {
            vars.get(name)
                .cloned()
                .or_else(|| env_value(name))
                .ok_or_else(|| format!("{name} is not set in {}", env_file.display()))
        };
        let r2 = R2 {
            endpoint: needed("SPAWN_R2_ENDPOINT")?,
            bucket: needed("SPAWN_R2_BUCKET")?,
            access_key: needed("SPAWN_R2_ACCESS_KEY_ID")?,
            secret_key: needed("SPAWN_R2_SECRET_ACCESS_KEY")?,
        };
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
            r2,
            pg_bin: env_value("SPAWN_BACKUP_PG_BIN").map(PathBuf::from),
            compress: env_or("SPAWN_BACKUP_DUMP_COMPRESS", "zstd:9"),
            transcript_days,
            limit_bytes,
            alert_bytes,
            drill_url: env_value("SPAWN_BACKUP_DRILL_DATABASE_URL"),
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
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        for attempt in 0..10u32 {
            let path = std::env::temp_dir().join(format!(
                "spawn-{what}-{}-{}{attempt}",
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
    let mut file =
        File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
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
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6,
        10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
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
    let (mut a0, mut b0, mut c0, mut d0) = (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
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
            if bytes[8] != b'-'
                || !bytes[..8].iter().chain(&bytes[9..]).all(u8::is_ascii_digit)
            {
                continue;
            }
            let number = |a: usize, b: usize| -> i64 {
                candidate.get(a..b).and_then(|part| part.parse().ok()).unwrap_or(0)
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
        if !key.is_empty() {
            entries.push((key, size));
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

/// The entry point: `spawn backup dump|bundle|verify [--root <dir>]`.
pub(crate) fn command(arguments: &[String]) -> Result<i32, String> {
    let Some(verb) = arguments.first() else {
        return Err(USAGE.to_string());
    };
    let rest: Vec<String> = arguments[1..].to_vec();
    let flags = Flags::parse(&rest).map_err(|error| format!("{error}\n{USAGE}"))?;
    match verb.as_str() {
        "dump" => dump(&flags),
        "bundle" => bundle(&flags),
        "verify" => verify(&flags),
        other => Err(format!("unknown backup command {other}\n{USAGE}")),
    }
}

/// `spawn backup dump`: one encrypted copy of the log, now.
#[allow(clippy::too_many_lines)]
fn dump(flags: &Flags) -> Result<i32, String> {
    let started = Instant::now();
    let settings = match Settings::from_env(flags) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("spawn backup dump: {error}");
            record_refusal(&error);
            return Ok(2);
        }
    };
    let _lock = Lock::acquire(&settings.root)?;
    let store = open_store()?;
    let url = parse_pg_url(&crate::config::required_env("SPAWN_DATABASE_URL")?)?;
    let now_ms = store.now_ms().map_err(|error| error.to_string())?;
    let now = Stamp::at(now_ms);
    let previous = dump_records(&store, now_ms)?;
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
    encrypt(&settings, &raw, &encrypted)?;
    let stored_bytes = fs::metadata(&encrypted)
        .map_err(|error| format!("{}: {error}", encrypted.display()))?
        .len();
    // The bytes leave the host encrypted, and decrypt back to what was
    // counted: a copy that does not is not a copy.
    let back = scratch.join("dump.back");
    decrypt(&settings, &encrypted, &back)?;
    if sha256_file(&back)? != sha256 {
        return Err("the encrypted copy does not decrypt to the dump".to_string());
    }
    drop(back);

    let held = list_bucket(&settings)?.0;
    let mut written = 0u64;
    let upload_started = Instant::now();
    match suspect(events, bytes, newest_good) {
        Some(reason) => {
            let key = format!("postgres/suspect/spawn-{}.dump.gpg", now.text());
            if within_limit(held, written, stored_bytes, settings.limit_bytes) {
                put_object(&settings, &key, &encrypted, &sha256, events)?;
                written = written.saturating_add(stored_bytes);
                keys.push(key);
            } else {
                skipped.push(skip("suspect", held, written, stored_bytes, &settings));
            }
            suspect_reason = Some(reason);
        }
        None => {
            for tier in tiers_for(now, &earlier) {
                let key = tier.key(&now.text());
                if !within_limit(held, written, stored_bytes, settings.limit_bytes) {
                    skipped.push(skip(tier.name(), held, written, stored_bytes, &settings));
                    continue;
                }
                put_object(&settings, &key, &encrypted, &sha256, events)?;
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
    if let Some(reason) = &suspect_reason {
        record["suspect"] = json!(reason);
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
    if !keys.iter().any(|key| key.starts_with("postgres/hourly/")) {
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
/// absence (best effort: the store may be what is broken).
fn record_refusal(reason: &str) {
    let Ok(store) = open_store() else {
        return;
    };
    let _ = store.backup_record(&json!({
        "type": "backup.verified", "ok": false, "reason": reason,
    }));
}

/// The earlier `backup.dumped` records, oldest first, with their event ids.
struct DumpRecord {
    event_id: i64,
    record: Value,
}

fn dump_records(store: &PostgresStore, now_ms: i64) -> Result<Vec<DumpRecord>, String> {
    let records = store
        .backup_records("backup.dumped", now_ms.saturating_sub(400 * 86_400_000), 10_000)
        .map_err(|error| error.to_string())?;
    Ok(records
        .as_array()
        .into_iter()
        .flatten()
        .map(|entry| DumpRecord {
            event_id: entry["event_id"].as_i64().unwrap_or_default(),
            record: entry["record"].clone(),
        })
        .collect())
}

/// `pg_dump -Fc` of the database, its password in the environment.
fn pg_dump(settings: &Settings, url: &PgUrl, out: &Path) -> Result<(), String> {
    let mut command = Command::new(settings.pg_tool("pg_dump"));
    command
        .arg("-Fc")
        .arg("--no-owner")
        .arg("--no-acl");
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
        return Err(format!("pg_restore (to count the archive): {}", stderr.trim()));
    }
    let text = fs::read_to_string(scratch.join("events.sql"))
        .map_err(|error| format!("the archive's events: {error}"))?;
    Ok(copy_events(&text))
}

/// gpg: AES256, no compression (the archive is compressed), the passphrase
/// from its file.
fn gpg(settings: &Settings, decrypting: bool, input: &Path, out: &Path) -> Result<(), String> {
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
        .arg(&settings.key_file);
    if decrypting {
        command.arg("--output").arg(out).arg("--decrypt").arg(input);
    } else {
        command
            .args([
                "--symmetric",
                "--cipher-algo",
                "AES256",
                "--compress-algo",
                "none",
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

fn encrypt(settings: &Settings, input: &Path, out: &Path) -> Result<(), String> {
    gpg(settings, false, input, out)
}

fn decrypt(settings: &Settings, input: &Path, out: &Path) -> Result<(), String> {
    gpg(settings, true, input, out)
}

/// One exchange with the bucket. `options` is curl config (headers, the
/// file to upload); the credentials are added here, on stdin, never argv.
struct Response {
    status: u16,
    body: String,
    headers: String,
}

fn bucket_request(settings: &Settings, url: &str, options: &str) -> Result<Response, String> {
    let scratch = Scratch::new("curl")?;
    let body_file = scratch.join("body");
    let headers_file = scratch.join("headers");
    let config = format!(
        "url = \"{}\"\noutput = \"{}\"\ndump-header = \"{}\"\nwrite-out = \"%{{http_code}}\"\n{}",
        escape_config(url),
        body_file.display(),
        headers_file.display(),
        options
    );
    let secret = format!(
        "silent\nshow-error\nuser = \"{}:{}\"\naws-sigv4 = \"aws:amz:auto:s3\"\n\
max-time = \"600\"\nconnect-timeout = \"20\"\n{config}",
        escape_config(&settings.r2.access_key),
        escape_config(&settings.r2.secret_key)
    );
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
        return Err(format!("curl: {}", stderr.trim()));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let status = stdout
        .trim()
        .lines()
        .last()
        .and_then(|line| line.trim().parse::<u16>().ok())
        .ok_or_else(|| format!("curl: no status in its output ({})", stdout.trim()))?;
    Ok(Response {
        status,
        body: fs::read_to_string(&body_file).unwrap_or_default(),
        headers: fs::read_to_string(&headers_file).unwrap_or_default(),
    })
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
        let url = match &token {
            None => settings.r2.url("?list-type=2&max-keys=1000"),
            Some(token) => settings.r2.url(&format!(
                "?list-type=2&max-keys=1000&continuation-token={}",
                percent_encode(token)
            )),
        };
        let response = bucket_request(settings, &url, "")?;
        if response.status != 200 {
            return Err(format!(
                "ListObjectsV2 answered {}: {}",
                response.status,
                first_line(&response.body)
            ));
        }
        let (page, next) = parse_listing(&response.body);
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

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or("").chars().take(160).collect()
}

fn percent_encode(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        let c = byte as char;
        if c.is_ascii_alphanumeric() || "-_.~".contains(c) {
            out.push(c);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Store one object, its ETag checked against the MD5 sent and its metadata
/// naming the plaintext's sha256 and event count.
fn put_object(
    settings: &Settings,
    key: &str,
    file: &Path,
    sha256: &str,
    events: i64,
) -> Result<(), String> {
    let md5 = md5_file(file)?;
    let options = format!(
        "upload-file = \"{}\"\nheader = \"Content-Type: application/octet-stream\"\n\
header = \"x-amz-meta-sha256: {}\"\nheader = \"x-amz-meta-spawn-events: {}\"\n",
        escape_config(&file.display().to_string()),
        sha256,
        events
    );
    let response = bucket_request(settings, &settings.r2.url(key), &options)?;
    if response.status != 200 {
        return Err(format!(
            "PUT {key} answered {}: {}",
            response.status,
            first_line(&response.body)
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

/// `spawn backup dump`'s body is above; `bundle` and `verify` follow.
///
/// One `backup.bundled` record, as the run reads the earlier ones.
struct BundleRecord {
    event_id: i64,
    at_ms: i64,
    record: Value,
}

fn bundle_records(store: &PostgresStore, now_ms: i64) -> Result<Vec<BundleRecord>, String> {
    let records = store
        .backup_records("backup.bundled", now_ms.saturating_sub(400 * 86_400_000), 10_000)
        .map_err(|error| error.to_string())?;
    Ok(records
        .as_array()
        .into_iter()
        .flatten()
        .map(|entry| BundleRecord {
            event_id: entry["event_id"].as_i64().unwrap_or_default(),
            at_ms: entry["at_ms"].as_i64().unwrap_or_default(),
            record: entry["record"].clone(),
        })
        .collect())
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
                .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
                .unwrap_or(0);
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
            let mut command = Command::new("git");
            command.args(["-C", source, "for-each-ref", "--format=%(objectname) %(refname)",
                          "refs/heads", "refs/tags"]);
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
    Command::new("git")
        .args(["-C", source, "cat-file", "-e", commit])
        .stdin(Stdio::null())
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
        .map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' })
        .collect::<String>();
    let base = if base.is_empty() { format!("source-{index}") } else { base };
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
            return Ok(2);
        }
    };
    let _lock = Lock::acquire(&settings.root)?;
    let store = open_store()?;
    let now_ms = store.now_ms().map_err(|error| error.to_string())?;
    let now = Stamp::at(now_ms);
    let earlier = bundle_records(&store, now_ms)?;
    let base = earlier.last();
    let stamps: Vec<(Stamp, bool, i64)> = earlier
        .iter()
        .filter_map(|entry| {
            let key = entry.record["keys"].as_array()?.first()?.as_str()?;
            let kind = entry.record["kind"].as_str()?;
            Some((Stamp::parse(key)?, kind == "full", entry.at_ms))
        })
        .collect();
    let full = full_bundle(now, &stamps, now_ms);
    let sources = store
        .mission_sources()
        .map_err(|error| error.to_string())?
        .as_array()
        .cloned()
        .unwrap_or_default();
    let archive = settings.root.join(".transcripts");
    let window = window_files(&archive, now_ms, settings.transcript_days)?;
    let scratch = Scratch::new("backup-bundle")?;
    let staging = scratch.join("stage");
    fs::create_dir_all(staging.join("repos"))
        .map_err(|error| format!("{}: {error}", staging.display()))?;
    fs::create_dir_all(staging.join("transcripts"))
        .map_err(|error| format!("{}: {error}", staging.display()))?;

    let mut transcript_entries: Vec<Value> = Vec::new();
    let mut transp_bytes = 0u64;
    let mut sizes = serde_json::Map::new();
    for (path, size) in &window {
        sizes.insert(path.clone(), json!(size));
        let base_size = base
            .and_then(|base| base.record["transcripts"]["sizes"].get(path))
            .and_then(Value::as_u64);
        let offset = if full { 0 } else { transcript_offset(base_size, *size) };
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
        let Some(source) = source.as_str() else { continue };
        let heads = match heads_of(source) {
            Ok(heads) => heads,
            Err(_) => {
                missing.push(source.to_string());
                continue;
            }
        };
        let refs = heads.len();
        let base_repos = base
            .and_then(|base| base.record["repos"].as_array().cloned())
            .unwrap_or_default();
        let base_heads = base_repos
            .iter()
            .find(|repo| repo["source"].as_str() == Some(source))
            .and_then(|repo| repo["heads"].as_object().cloned())
            .unwrap_or_default();
        let unchanged = !full
            && base.is_some()
            && base_heads.len() == heads.len()
            && heads
                .iter()
                .all(|(name, commit)| base_heads.get(name).and_then(Value::as_str) == Some(commit.as_str()));
        if unchanged {
            repos_record.push(json!({
                "source": source, "heads": heads, "refs": refs, "bytes": 0, "unchanged": true,
            }));
            repos_manifest.push(json!({
                "source": source, "file": Value::Null, "heads": heads, "unchanged": true,
            }));
            continue;
        }
        let name = source_name(source, &mut taken, index);
        let file = scratch.join(format!("{name}.bundle"));
        let not: Vec<String> = if full {
            Vec::new()
        } else {
            base_heads
                .values()
                .filter_map(Value::as_str)
                .filter(|commit| holds(source, commit))
                .map(str::to_string)
                .collect()
        };
        git_bundle(source, &file, &not)?;
        let bytes = fs::metadata(&file)
            .map_err(|error| format!("{}: {error}", file.display()))?
            .len();
        let relative = format!("repos/{name}.bundle");
        fs::copy(&file, staging.join(&relative))
            .map_err(|error| format!("{}: {error}", file.display()))?;
        repos_record.push(json!({
            "source": source, "heads": heads, "refs": refs, "bytes": bytes,
            "unchanged": false, "file": relative,
        }));
        repos_manifest.push(json!({
            "source": source, "file": relative, "heads": heads, "unchanged": false,
        }));
    }

    let manifest = json!({
        "kind": if full { "full" } else { "incremental" },
        "base_event_id": base.map(|base| base.event_id),
        "transcripts": transcript_entries,
        "repos": repos_manifest,
        "created_at_ms": now_ms,
    });
    fs::write(
        staging.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("manifest.json: {error}"))?;
    let tar = scratch.join("bundle.tar");
    let (ok, _stdout, stderr) = output(
        {
            let mut command = Command::new("tar");
            command.arg("-C").arg(&staging).arg("-cf").arg(&tar).arg(".");
            command
        },
        "tar",
    )?;
    if !ok {
        return Err(format!("tar: {}", stderr.trim()));
    }
    let bytes = fs::metadata(&tar)
        .map_err(|error| format!("{}: {error}", tar.display()))?
        .len();
    let sha256 = sha256_file(&tar)?;
    let encrypted = scratch.join("bundle.gpg");
    encrypt(&settings, &tar, &encrypted)?;
    let stored_bytes = fs::metadata(&encrypted)
        .map_err(|error| format!("{}: {error}", encrypted.display()))?
        .len();
    let back = scratch.join("bundle.back");
    decrypt(&settings, &encrypted, &back)?;
    if sha256_file(&back)? != sha256 {
        return Err("the encrypted bundle does not decrypt to the tar".to_string());
    }

    let held = list_bucket(&settings)?.0;
    let tier = if full { "full" } else { "incr" };
    let key = format!(
        "bundles/{}/spawn-bundle-{}.tar.gpg",
        tier,
        now.text()
    );
    let mut keys: Vec<String> = Vec::new();
    let mut skipped: Vec<Value> = Vec::new();
    let mut written = 0u64;
    if within_limit(held, written, stored_bytes, settings.limit_bytes) {
        put_object(&settings, &key, &encrypted, &sha256, 0)?;
        written = written.saturating_add(stored_bytes);
        keys.push(key);
    } else {
        skipped.push(skip(tier, held, written, stored_bytes, &settings));
    }
    let over_alert = held.saturating_add(written) > settings.alert_bytes;
    let record = json!({
        "type": "backup.bundled",
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
        },
        "held_bytes": held,
        "written_bytes": written,
        "alert_bytes": settings.alert_bytes,
        "limit_bytes": settings.limit_bytes,
        "over_alert": over_alert,
        "duration_ms": i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
    });
    store
        .backup_record(&record)
        .map_err(|error| format!("backup.bundled: {error}"))?;
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
        if keys.is_empty() { "nothing".to_string() } else { keys.join(", ") }
    );
    Ok(0)
}

/// Copy `size - offset` bytes from `offset` of `source` to `target`.
fn append_slice(source: &Path, target: &Path, offset: u64, size: u64) -> Result<(), String> {
    let mut file =
        File::open(source).map_err(|error| format!("{}: {error}", source.display()))?;
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

/// A git bundle of every ref of `source`, but not `not`'s commits.
fn git_bundle(source: &str, file: &Path, not: &[String]) -> Result<(), String> {
    let mut command = Command::new("git");
    command.args(["-C", source, "bundle", "create"]).arg(file).arg("--all");
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

/// What the drill restored and where, for its record.
#[derive(Debug, Default, Clone)]
struct Drilled {
    database: Option<String>,
    events: Option<i64>,
    max_event_id: Option<i64>,
}

/// `spawn backup verify`: the newest good dump restored into a database of
/// its own and compared with its record.
fn verify(flags: &Flags) -> Result<i32, String> {
    let started = Instant::now();
    let settings = match Settings::from_env(flags) {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("spawn backup verify: {error}");
            record_verified(None, false, &error, &json!({}), &Drilled::default(), started);
            return Ok(2);
        }
    };
    let _lock = Lock::acquire(&settings.root)?;
    let store = open_store()?;
    let Some(drill_url) = settings.drill_url.clone() else {
        let reason = "SPAWN_BACKUP_DRILL_DATABASE_URL is not set: the drill has no server to restore into";
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
    let dumps = dump_records(&store, now_ms)?;
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
    let expected_sha = latest.record["sha256"].as_str().unwrap_or_default().to_string();
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
    let outcome = restore_and_compare(&settings, &drill_url, latest);
    let (ok, reason, drilled) = match outcome {
        Ok(drilled) => (true, String::new(), drilled),
        Err(RestoreError { reason, drilled }) => (false, reason, drilled),
    };
    let mut record = naming;
    record["events"] = json!(drilled.events);
    record["max_event_id"] = json!(drilled.max_event_id);
    record["server"] = json!(parse_pg_url(&drill_url).map(|url| url.server()).unwrap_or_default());
    let code = if ok {
        record_verified(Some(&store), true, "", &record, &drilled, started);
        0
    } else {
        record_verified(Some(&store), false, &reason, &record, &drilled, started);
        1
    };
    if ok {
        println!(
            "spawn backup verify: {} restored to its record ({} events, max id {}) into {}",
            key,
            record["events"],
            record["max_event_id"],
            drilled.database.as_deref().unwrap_or("a scratch database")
        );
    } else {
        eprintln!("spawn backup verify: {reason}");
    }
    Ok(code)
}

/// A drill that failed, with what it had made when it did.
struct RestoreError {
    reason: String,
    drilled: Drilled,
}

/// Fetch, check, restore, compare; the scratch database is dropped on every
/// path after it was made.
fn restore_and_compare(
    settings: &Settings,
    drill_url: &str,
    latest: &DumpRecord,
) -> Result<Drilled, RestoreError> {
    let mut drilled = Drilled::default();
    let fail = |reason: String, drilled: Drilled| RestoreError { reason, drilled };
    let key = latest.record["keys"][0].as_str().unwrap_or_default();
    let expected_sha = latest.record["sha256"].as_str().unwrap_or_default();
    let Some((bytes, headers)) = get_object_bytes(settings, key)
        .map_err(|error| fail(error, drilled.clone()))?
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
            reason: format!("the object's x-amz-meta-sha256 {meta} is not the record's {expected_sha}"),
            drilled,
        });
    }
    let scratch = Scratch::new("drill").map_err(|error| fail(error, drilled.clone()))?;
    let encrypted = scratch.join("dump.gpg");
    fs::write(&encrypted, &bytes)
        .map_err(|error| fail(format!("{}: {error}", encrypted.display()), drilled.clone()))?;
    let raw = scratch.join("dump");
    if let Err(error) = decrypt(settings, &encrypted, &raw) {
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
        Err(error) => return Err(RestoreError { reason: error, drilled }),
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
        return Err(RestoreError { reason: error, drilled });
    }
    drilled.database = Some(database.clone());
    let outcome = restore_into(settings, &url, &base, &database, &raw, &scratch);
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
                    reason: format!("the scratch database {database} could not be dropped: {error}"),
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

/// The object's bytes, as they are (not through a String), and its headers.
fn get_object_bytes(settings: &Settings, key: &str) -> Result<Option<(Vec<u8>, String)>, String> {
    let scratch = Scratch::new("fetch")?;
    let output = scratch.join("object");
    let options = format!(
        "output = \"{}\"\n",
        escape_config(&output.display().to_string())
    );
    let response = bucket_request(settings, &settings.r2.url(key), &options)?;
    match response.status {
        200 => Ok(Some((
            fs::read(&output).map_err(|error| format!("{}: {error}", output.display()))?,
            response.headers,
        ))),
        404 => Ok(None),
        status => Err(format!("GET {key} answered {status}")),
    }
}

fn spawn_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(0))
        .unwrap_or(0)
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
        return Err(format!("the scratch database name {database:?} is not a plain identifier"));
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
        return Err(format!("the database {database} already exists: the drill makes its own"));
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
        Some(&format!("drop database if exists \"{database}\" with (force)")),
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
    sql.lines()
        .filter(|line| line.trim() != "SET transaction_timeout = 0;")
        .map(|line| format!("{line}\n"))
        .collect()
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
    let sql = fs::read_to_string(&script).map_err(|error| format!("{}: {error}", script.display()))?;
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
        Some("select count(*), coalesce(max(id), 0) from spawn_events"),
        None,
    )?;
    if !ok {
        return Err(format!("the restored database: {}", stderr.trim()));
    }
    let mut parts = counts.split_whitespace();
    let events = parts.next().and_then(|part| part.parse().ok()).unwrap_or(-1);
    let max = parts.next().and_then(|part| part.parse().ok()).unwrap_or(-1);
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
        "expected": naming.get("expected").cloned().unwrap_or(Value::Null),
        "events": drilled.events,
        "max_event_id": drilled.max_event_id,
        "database": drilled.database,
        "server": naming.get("server").cloned().unwrap_or(Value::Null),
        "duration_ms": i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
    });
    if record["server"].is_null()
        && let Some(drilled_url) = env_value("SPAWN_BACKUP_DRILL_DATABASE_URL")
        && let Ok(url) = parse_pg_url(&drilled_url)
    {
        record["server"] = json!(url.server());
    }
    let _ = store.backup_record(&record);
}
