//! `spawn secret set|generate|rotate|list`: the store of secrets by name
//! (sync/design-secret-injection.md, a fleet note, outside this repository:
//! section 5, step B; rotation, section 3.2, objective 123).
//!
//! The store is the directory `SPAWN_SECRET_STORE` names: absolute, existing,
//! owned by the running user, mode 700, not a symbolic link. Each secret is
//! two files: `<store>/<NAME>` holds the value exactly, `<store>/<NAME>.json`
//! its metadata (`name`, `tier`, `created`), never the value; both mode 600.
//!
//! A value is never an argument, a file or `/dev/null`: `set` reads it from a
//! pipe, or from a terminal with echo off; `generate` makes it inside the
//! store. No command prints a value, and there is no `get`. Tier 2 is refused:
//! no secret is injectable (owner decision 2 of 2026-11-13).
//!
//! Each file is written to a staged file (`.<file>.<pid>.new`, created new,
//! 600), fsynced, hard-linked to its name (never over one) and the directory
//! fsynced, so a crash leaves no partial value under a name. A write holds the
//! store's exclusive lock from its last check to its fsync; under it no other
//! write is in progress, so a staged file or a half entry is a killed write's
//! leftover, and is removed (staged files) or replaced (the name written).
//!
//! `rotate NAME` replaces a stored value (read as `set` reads it, or made as
//! `generate` makes it) and keeps the previous one as `<NAME>.retired.<k>`
//! (600, the next number after the highest), served by nothing: transcripts
//! and copies made before still hold it, and a later reader must keep looking
//! for it. A value NAME holds now or held before is refused, compared byte
//! for byte. Under the exclusive lock the new value is staged, the current
//! file hard-linked to its retired name (never over one), the staged value
//! renamed over `<NAME>`, the metadata staged and renamed over `<NAME>.json`,
//! and the directory fsynced: `<NAME>` always holds the old value or the new
//! one whole, and the old value has its retired name before `<NAME>` changes.
//! A retired name that is the current file itself (a rotation killed after
//! its link) is removed by the next rotation, which also rebuilds the
//! metadata of a value rotated without it.
//!
//! The metadata then also holds `rotated`, `fingerprint` and `retired`
//! (`[{n, retired, fingerprint}]`, oldest first). A fingerprint is the first
//! 12 hex characters of HMAC-SHA256 of the value under the store's key,
//! `<store>/.fingerprint.key` (32 random bytes, 600, never printed): keyed, so
//! a short or guessable value cannot be tested against it offline. The store's
//! first rotation makes the key; `set` and `generate` add a fingerprint only
//! once it exists.

use std::fs::{self, File, OpenOptions};
use std::io::{IsTerminal, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// The usage, printed for `get` and every verb that is not one (main's
/// help reads it too).
pub(crate) const USAGE: &str = "usage: spawn secret set NAME [--tier 1]   (the value from a pipe, or typed at a terminal, not echoed)\n       spawn secret generate NAME --bytes N   (N random bytes, 16..1024, made inside the store)\n       spawn secret rotate NAME [--generate --bytes N]   (the new value as set reads it or generate makes it; the previous one is kept, retired)\n       spawn secret list\nThere is no get: no command prints a value, current or retired.";

/// The store's fingerprint key: 32 random bytes, mode 600.
const KEY_FILE: &str = ".fingerprint.key";

/// The largest value, in bytes.
const MAX_VALUE: usize = 64 * 1024;

/// Why a command stops: the usage, or one sentence.
#[derive(Debug, PartialEq, Eq)]
enum Refusal {
    Usage,
    Say(String),
}

fn say(sentence: impl Into<String>) -> Refusal {
    Refusal::Say(sentence.into())
}

/// `spawn secret …`: exit 0 on success, 2 on any refusal.
pub(crate) fn command(words: &[String]) -> i32 {
    match run(words) {
        Ok(()) => 0,
        Err(Refusal::Usage) => {
            eprintln!("{USAGE}");
            2
        }
        Err(Refusal::Say(sentence)) => {
            eprintln!("spawn secret: {sentence}");
            2
        }
    }
}

fn run(words: &[String]) -> Result<(), Refusal> {
    let Some((verb, rest)) = words.split_first() else {
        return Err(Refusal::Usage);
    };
    match verb.as_str() {
        "set" => {
            let name = set_flags(rest)?;
            let store = Store::open()?;
            store.refuse_existing(&name)?;
            let value = read_value(&name)?;
            store.write(&name, &value)
        }
        "generate" => {
            let (name, bytes) = generate_flags(rest)?;
            let store = Store::open()?;
            store.refuse_existing(&name)?;
            let value = generate(bytes)?;
            store.write(&name, &value)
        }
        "rotate" => {
            let (name, generated) = rotate_flags(rest)?;
            let store = Store::open()?;
            store.refuse_missing(&name)?;
            let value = match generated {
                Some(bytes) => generate(bytes)?,
                None => read_value(&name)?,
            };
            store.rotate(&name, &value)
        }
        "list" => {
            if !rest.is_empty() {
                return Err(say("list takes no argument"));
            }
            Store::open()?.list()
        }
        _ => Err(Refusal::Usage),
    }
}

/// A secret's name: `[A-Z][A-Z0-9_]{0,63}`.
fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_uppercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_')
}

fn checked_name(word: Option<&String>) -> Result<String, Refusal> {
    let Some(name) = word else {
        return Err(Refusal::Usage);
    };
    if valid_name(name) {
        Ok(name.clone())
    } else {
        Err(say(
            "a name is a capital letter then up to 63 capitals, digits or underscores ([A-Z][A-Z0-9_]{0,63}); nothing was stored",
        ))
    }
}

/// `set NAME [--tier 1]`: the name. Any other word is refused without being
/// repeated (it may be the value); tier 2 is refused as not injectable.
fn set_flags(words: &[String]) -> Result<String, Refusal> {
    let name = checked_name(words.first())?;
    let mut rest = words[1..].iter();
    while let Some(word) = rest.next() {
        let tier = if word == "--tier" {
            rest.next().map(String::as_str)
        } else if let Some(tier) = word.strip_prefix("--tier=") {
            Some(tier)
        } else {
            return Err(say(
                "set takes only NAME and --tier 1; the value is never an argument, it is read from a pipe or typed at a terminal, and the extra word was not stored",
            ));
        };
        tier_one(tier)?;
    }
    Ok(name)
}

/// A `--tier` word's value: 1 is taken, 2 refused as not injectable.
fn tier_one(tier: Option<&str>) -> Result<(), Refusal> {
    match tier {
        Some("1") => Ok(()),
        Some("2") => Err(say(
            "tier 2 is refused: no secret is injectable (owner decision 2), so nothing was read or stored",
        )),
        _ => Err(say("the only tier is 1 (--tier 1); nothing was stored")),
    }
}

/// A `--bytes` word's value: 16..=1024.
fn byte_count(count: Option<&str>) -> Result<usize, Refusal> {
    match count.and_then(|c| c.parse::<usize>().ok()) {
        Some(n) if (16..=1024).contains(&n) => Ok(n),
        _ => Err(say("--bytes takes a number from 16 to 1024")),
    }
}

/// `rotate NAME [--tier 1] [--generate --bytes N]`: the name, and N when the
/// new value is generated rather than read. Any other word is refused
/// without being repeated (it may be the value); tier 2 is refused.
fn rotate_flags(words: &[String]) -> Result<(String, Option<usize>), Refusal> {
    let name = checked_name(words.first())?;
    let (mut generated, mut bytes) = (false, None);
    let mut rest = words[1..].iter();
    while let Some(word) = rest.next() {
        if word == "--generate" {
            generated = true;
        } else if word == "--tier" {
            tier_one(rest.next().map(String::as_str))?;
        } else if let Some(tier) = word.strip_prefix("--tier=") {
            tier_one(Some(tier))?;
        } else if word == "--bytes" {
            bytes = Some(byte_count(rest.next().map(String::as_str))?);
        } else if let Some(count) = word.strip_prefix("--bytes=") {
            bytes = Some(byte_count(Some(count))?);
        } else {
            return Err(say(
                "rotate takes only NAME, --tier 1 and --generate --bytes N; the value is never an argument, it is read from a pipe or typed at a terminal, and the extra word was not stored",
            ));
        }
    }
    match (generated, bytes) {
        (true, None) => Err(say(
            "rotate --generate needs --bytes N (16 to 1024); nothing was stored",
        )),
        (false, Some(_)) => Err(say("--bytes goes with --generate; nothing was stored")),
        (_, bytes) => Ok((name, bytes)),
    }
}

/// `generate NAME --bytes N`: the name and N (16..=1024).
fn generate_flags(words: &[String]) -> Result<(String, usize), Refusal> {
    let name = checked_name(words.first())?;
    let mut bytes = None;
    let mut rest = words[1..].iter();
    while let Some(word) = rest.next() {
        let count = if word == "--bytes" {
            rest.next().map(String::as_str)
        } else if let Some(count) = word.strip_prefix("--bytes=") {
            Some(count)
        } else {
            return Err(say("generate takes only NAME and --bytes N"));
        };
        bytes = Some(byte_count(count)?);
    }
    let Some(bytes) = bytes else {
        return Err(say("generate needs --bytes N (16 to 1024)"));
    };
    Ok((name, bytes))
}

/// A value in memory, overwritten when dropped.
struct Value(Vec<u8>);

impl Drop for Value {
    fn drop(&mut self) {
        self.0.fill(0);
        std::hint::black_box(&self.0);
    }
}

/// The value of `set`, from a pipe or a terminal only.
fn read_value(name: &str) -> Result<Value, Refusal> {
    let stdin = std::io::stdin();
    let not_from = || {
        say(
            "the value comes from a pipe or a terminal (not an argument, a file or /dev/null); nothing was stored",
        )
    };
    let value = if stdin.is_terminal() {
        from_terminal(name)?
    } else {
        let fd = std::os::fd::AsFd::as_fd(&stdin)
            .try_clone_to_owned()
            .map_err(|_| not_from())?;
        let metadata = File::from(fd).metadata().map_err(|_| not_from())?;
        if !metadata.file_type().is_fifo() {
            return Err(not_from());
        }
        from_pipe(&mut stdin.lock())?
    };
    if value.0.is_empty() {
        return Err(say("the value is empty; nothing was stored"));
    }
    if value.0.len() > MAX_VALUE {
        return Err(say("the value is over 64 KiB; nothing was stored"));
    }
    Ok(value)
}

/// A piped value, without one trailing "\n" or "\r\n".
fn from_pipe(input: &mut impl Read) -> Result<Value, Refusal> {
    let mut value = Value(Vec::new());
    let limit = (MAX_VALUE + 3) as u64;
    input
        .take(limit)
        .read_to_end(&mut value.0)
        .map_err(|_| say("the value could not be read from the pipe; nothing was stored"))?;
    if value.0.ends_with(b"\r\n") {
        value.0.truncate(value.0.len() - 2);
    } else if value.0.ends_with(b"\n") {
        value.0.truncate(value.0.len() - 1);
    }
    Ok(value)
}

/// The terminal's saved settings (`stty -g`), restored when dropped.
struct Echo(String);

fn stty(arguments: &[&str]) -> Option<String> {
    let output = Command::new("stty")
        .args(arguments)
        .stdin(Stdio::inherit())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

impl Drop for Echo {
    fn drop(&mut self) {
        if stty(&[&self.0]).is_none() {
            eprintln!(
                "spawn secret: the terminal's settings could not be restored; run `stty sane`"
            );
        }
    }
}

/// A value typed at the terminal: echo and keyboard signals off, read byte
/// by byte, the settings restored on every path.
fn from_terminal(name: &str) -> Result<Value, Refusal> {
    let Some(saved) = stty(&["-g"]) else {
        return Err(say(
            "the terminal's settings could not be read; nothing was stored",
        ));
    };
    let echo = Echo(saved);
    if stty(&["-echo", "-icanon", "-isig", "min", "1", "time", "0"]).is_none() {
        drop(echo);
        return Err(say(
            "the terminal's echo could not be turned off; nothing was read or stored",
        ));
    }
    eprint!("The value of {name} (not echoed; Enter ends it, Ctrl-C cancels): ");
    let _ = std::io::stderr().flush();
    let line = edit_line(&mut std::io::stdin().lock());
    eprintln!();
    drop(echo);
    match line {
        Line::Value(value) => Ok(value),
        Line::Cancelled => Err(say("cancelled at the prompt (Ctrl-C); nothing was stored")),
        Line::TooLong => Err(say("the value is over 64 KiB; nothing was stored")),
        Line::Failed => Err(say("the terminal could not be read; nothing was stored")),
    }
}

/// What the terminal line gave.
enum Line {
    Value(Value),
    Cancelled,
    TooLong,
    Failed,
}

/// The line editor of the terminal: Enter, CR, Ctrl-D or the end of input
/// end the value; backspace erases one character (a whole UTF-8 sequence);
/// Ctrl-U erases the line; Ctrl-C cancels. Every other byte is the value's.
fn edit_line(input: &mut impl Read) -> Line {
    let mut value = Value(Vec::new());
    let mut byte = [0u8; 1];
    loop {
        match input.read(&mut byte) {
            Ok(0) => return Line::Value(value),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Line::Failed,
        }
        match byte[0] {
            b'\n' | b'\r' | 0x04 => return Line::Value(value),
            0x03 => return Line::Cancelled,
            0x15 => {
                value.0.fill(0);
                value.0.clear();
            }
            0x7f | 0x08 => {
                while let Some(last) = value.0.pop() {
                    if last & 0xC0 != 0x80 {
                        break;
                    }
                }
            }
            other => {
                if value.0.len() >= MAX_VALUE {
                    return Line::TooLong;
                }
                value.0.push(other);
            }
        }
    }
}

/// N random bytes from /dev/urandom, as base64url without padding.
fn generate(bytes: usize) -> Result<Value, Refusal> {
    let mut raw = Value(vec![0u8; bytes]);
    File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut raw.0))
        .map_err(|_| say("/dev/urandom could not be read; nothing was stored"))?;
    Ok(Value(base64url(&raw.0).into_bytes()))
}

/// Base64url (RFC 4648 §5) without padding.
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]));
        }
    }
    out
}

/// The store's fingerprint key in memory, overwritten when dropped.
struct Key([u8; 32]);

impl Drop for Key {
    fn drop(&mut self) {
        self.0.fill(0);
        std::hint::black_box(&self.0);
    }
}

/// A value's fingerprint: HMAC-SHA256 under `key`, its first 12 hex
/// characters.
fn fingerprint(key: &[u8], value: &[u8]) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut mac =
        Hmac::<sha2::Sha256>::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(value);
    let tag = mac.finalize().into_bytes();
    let mut hex = String::with_capacity(12);
    for byte in &tag[..6] {
        hex.push(char::from(HEX[usize::from(byte >> 4)]));
        hex.push(char::from(HEX[usize::from(byte & 15)]));
    }
    hex
}

/// The numbers k of `name`'s retired values (`<name>.retired.<k>`, k a
/// number from 1 written without a leading zero) among `entries`, sorted.
fn retired_numbers(name: &str, entries: &[String]) -> Vec<u64> {
    let prefix = format!("{name}.retired.");
    let mut numbers: Vec<u64> = entries
        .iter()
        .filter_map(|entry| entry.strip_prefix(&prefix))
        .filter(|k| !k.is_empty() && !k.starts_with('0') && k.bytes().all(|b| b.is_ascii_digit()))
        .filter_map(|k| k.parse().ok())
        .collect();
    numbers.sort_unstable();
    numbers.dedup();
    numbers
}

/// A staged file a write makes: `.<NAME>.<pid>.new`, `.<NAME>.json.<pid>.new`
/// or the key's, `..fingerprint.key.<pid>.new`.
fn staged(entry: &str) -> bool {
    let Some(middle) = entry
        .strip_prefix('.')
        .and_then(|rest| rest.strip_suffix(".new"))
    else {
        return false;
    };
    let Some((file, pid)) = middle.rsplit_once('.') else {
        return false;
    };
    let name = file.strip_suffix(".json").unwrap_or(file);
    !pid.is_empty()
        && pid.bytes().all(|b| b.is_ascii_digit())
        && (valid_name(name) || file == KEY_FILE)
}

/// The time now, RFC 3339 in UTC to the second.
fn now_rfc3339() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
    let (days, rest) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// The store directory, checked.
struct Store {
    path: PathBuf,
}

impl Store {
    /// The directory `SPAWN_SECRET_STORE` names, refused unless it is an
    /// absolute path to an existing directory of the running user's, mode
    /// 700, and not a symbolic link (checked without following it).
    fn open() -> Result<Self, Refusal> {
        let Some(path) = std::env::var_os("SPAWN_SECRET_STORE").filter(|p| !p.is_empty()) else {
            return Err(say(
                "SPAWN_SECRET_STORE is not set; it names the store directory",
            ));
        };
        let path = PathBuf::from(path);
        let refused = |why: &str| {
            say(format!(
                "SPAWN_SECRET_STORE ({}) must be an existing directory of yours, mode 700, named by an absolute path and not a link: {why}; nothing was written",
                path.display()
            ))
        };
        if !path.is_absolute() {
            return Err(refused("it is not absolute"));
        }
        let metadata = fs::symlink_metadata(&path).map_err(|_| refused("it does not exist"))?;
        if metadata.file_type().is_symlink() {
            return Err(refused("it is a symbolic link"));
        }
        if !metadata.is_dir() {
            return Err(refused("it is not a directory"));
        }
        if metadata.uid() != spawn_host::current_uid() {
            return Err(refused("another user owns it"));
        }
        if metadata.mode() & 0o7777 != 0o700 {
            return Err(refused(&format!(
                "its mode is {:o}",
                metadata.mode() & 0o7777
            )));
        }
        Ok(Self { path })
    }

    fn file(&self, entry: &str) -> PathBuf {
        self.path.join(entry)
    }

    fn present(&self, entry: &str) -> bool {
        fs::symlink_metadata(self.file(entry)).is_ok()
    }

    /// An existing name (its value and its metadata both present) is refused.
    fn refuse_existing(&self, name: &str) -> Result<(), Refusal> {
        if self.present(name) && self.present(&format!("{name}.json")) {
            return Err(say(format!(
                "{name} already exists and keeps its value; nothing was stored"
            )));
        }
        Ok(())
    }

    /// A name not in the store (its value and its metadata) is refused, before
    /// any value is read.
    fn refuse_missing(&self, name: &str) -> Result<(), Refusal> {
        if self.present(name) && self.present(&format!("{name}.json")) {
            Ok(())
        } else {
            Err(not_in_store(name))
        }
    }

    /// An entry of the store's, a regular file reached without a link, and
    /// its bytes; `None` when it is not one or cannot be read.
    fn read_regular(&self, entry: &str) -> Option<(fs::Metadata, Value)> {
        let path = self.file(entry);
        let named = fs::symlink_metadata(&path).ok()?;
        if !named.file_type().is_file() {
            return None;
        }
        let mut file = File::open(&path).ok()?;
        let held = file.metadata().ok()?;
        if held.dev() != named.dev() || held.ino() != named.ino() {
            return None;
        }
        let mut value = Value(Vec::new());
        file.read_to_end(&mut value.0).ok()?;
        Some((held, value))
    }

    /// The store's fingerprint key, `None` when it has none; refused unless
    /// it is a regular file of 32 bytes, mode 600. Never printed.
    fn key(&self) -> Result<Option<Key>, Refusal> {
        let refused = |why: &str| {
            say(format!(
                "the store's fingerprint key ({KEY_FILE}) must be a regular file of 32 bytes, mode 600: {why}; nothing was written"
            ))
        };
        match fs::symlink_metadata(self.file(KEY_FILE)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(refused("it could not be read")),
            Ok(metadata) if !metadata.file_type().is_file() => {
                return Err(refused("it is not a regular file"));
            }
            Ok(metadata) if metadata.mode() & 0o7777 != 0o600 => {
                return Err(refused(&format!(
                    "its mode is {:o}",
                    metadata.mode() & 0o7777
                )));
            }
            Ok(_) => {}
        }
        let Some((_, bytes)) = self.read_regular(KEY_FILE) else {
            return Err(refused("it could not be read"));
        };
        let mut key = Key([0; 32]);
        if bytes.0.len() != key.0.len() {
            return Err(refused("it is not 32 bytes"));
        }
        key.0.copy_from_slice(&bytes.0);
        Ok(Some(key))
    }

    /// Makes the store's fingerprint key: 32 bytes of /dev/urandom, placed
    /// as any file is (staged, fsynced, linked, never over a name).
    fn make_key(&self) -> Result<Key, Refusal> {
        let mut key = Key([0; 32]);
        File::open("/dev/urandom")
            .and_then(|mut random| random.read_exact(&mut key.0))
            .map_err(|_| say("/dev/urandom could not be read; nothing was stored"))?;
        self.place(KEY_FILE, &key.0)?;
        Ok(key)
    }

    /// Removes every staged file, a killed write's leftovers.
    fn remove_staged(&self) -> Result<(), Refusal> {
        for entry in self.entries()? {
            if staged(&entry) {
                fs::remove_file(self.file(&entry)).map_err(|_| {
                    say("a killed write's staged file could not be removed; nothing was stored")
                })?;
            }
        }
        Ok(())
    }

    /// The store's lock: the directory itself, opened and checked to be the
    /// directory that was checked.
    fn lock(&self, exclusive: bool) -> Result<File, Refusal> {
        let failed = || say("the store could not be locked; nothing was written");
        let directory = File::open(&self.path).map_err(|_| failed())?;
        let (held, named) = (
            directory.metadata().map_err(|_| failed())?,
            fs::symlink_metadata(&self.path).map_err(|_| failed())?,
        );
        if held.dev() != named.dev() || held.ino() != named.ino() {
            return Err(failed());
        }
        if exclusive {
            directory.lock()
        } else {
            directory.lock_shared()
        }
        .map_err(|_| failed())?;
        Ok(directory)
    }

    fn entries(&self) -> Result<Vec<String>, Refusal> {
        let mut names: Vec<String> = fs::read_dir(&self.path)
            .map_err(|_| say("the store could not be read"))?
            .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
            .collect();
        names.sort();
        Ok(names)
    }

    /// Writes a new secret under the exclusive lock, after removing every
    /// staged file and replacing an incomplete entry of this name.
    fn write(&self, name: &str, value: &Value) -> Result<(), Refusal> {
        let directory = self.lock(true)?;
        self.remove_staged()?;
        let meta = format!("{name}.json");
        let (has_value, has_meta) = (self.present(name), self.present(&meta));
        if has_value && has_meta {
            return Err(say(format!(
                "{name} already exists and keeps its value; nothing was stored"
            )));
        }
        let incomplete = has_value || has_meta;
        let key = self.key()?;
        for (present, entry) in [(has_value, name), (has_meta, meta.as_str())] {
            if present {
                fs::remove_file(self.file(entry)).map_err(|_| {
                    say(format!(
                        "the incomplete {name} could not be removed; nothing was stored"
                    ))
                })?;
            }
        }
        let mut metadata = serde_json::json!({"name": name, "tier": 1, "created": now_rfc3339()});
        if let Some(key) = &key {
            metadata["fingerprint"] = fingerprint(&key.0, &value.0).into();
        }
        self.place(name, &value.0)?;
        self.place(&meta, format!("{metadata}\n").as_bytes())?;
        directory
            .sync_all()
            .map_err(|_| say("the store directory could not be synced"))?;
        if incomplete {
            println!(
                "spawn secret: stored {name} (tier 1), replacing the incomplete entry a killed write left"
            );
        } else {
            println!("spawn secret: stored {name} (tier 1)");
        }
        Ok(())
    }

    /// One file: staged ([`Store::stage`]), hard-linked to its name, the
    /// staged file removed.
    fn place(&self, entry: &str, bytes: &[u8]) -> Result<(), Refusal> {
        let stage = self.stage(entry, bytes)?;
        let linked = fs::hard_link(&stage, self.file(entry));
        let _ = fs::remove_file(&stage);
        linked.map_err(|_| say(format!("{entry} could not be written; it was not stored")))
    }

    /// `entry`'s staged file, `.<entry>.<pid>.new`: created new, 600,
    /// written and fsynced. None is left when it fails.
    fn stage(&self, entry: &str, bytes: &[u8]) -> Result<PathBuf, Refusal> {
        let stage = self.file(&format!(".{entry}.{}.new", std::process::id()));
        let result = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&stage)?;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            file.write_all(bytes)?;
            file.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(&stage);
            return Err(say(format!(
                "{entry} could not be written; it was not stored"
            )));
        }
        Ok(stage)
    }

    /// Rotates `name` to `value` under the exclusive lock: refused (and the
    /// store unchanged) when the value is the current one or a retired one;
    /// otherwise the killed rotations' leftovers go, the key is made if the
    /// store has none, and the five steps of the module's doc run in order.
    fn rotate(&self, name: &str, value: &Value) -> Result<(), Refusal> {
        let directory = self.lock(true)?;
        let meta = format!("{name}.json");
        if !(self.present(name) && self.present(&meta)) {
            return Err(not_in_store(name));
        }
        let Some((held, current)) = self.read_regular(name) else {
            return Err(say(format!(
                "{name}'s value could not be read as a regular file; nothing was rotated"
            )));
        };
        let (retired, leftovers) = self.retired(name, &held)?;
        if value.0 == current.0 || retired.iter().any(|(_, old)| old.0 == value.0) {
            return Err(say(format!(
                "the new value of {name} is one it holds now or held before; nothing was rotated"
            )));
        }
        self.remove_staged()?;
        for entry in &leftovers {
            fs::remove_file(self.file(entry)).map_err(|_| {
                say(format!(
                    "{entry}, a killed rotation's leftover, could not be removed; nothing was rotated"
                ))
            })?;
        }
        let key = match self.key()? {
            Some(key) => key,
            None => self.make_key()?,
        };
        let k = retired.last().map_or(1, |(n, _)| n + 1);
        let metadata = self.rotated_metadata(name, &key, value, &current, &retired, k);
        // 1. the new value, staged.
        let stage = self.stage(name, &value.0)?;
        // 2. the old value under its retired name, never over a name.
        let retired_entry = format!("{name}.retired.{k}");
        if fs::hard_link(self.file(name), self.file(&retired_entry)).is_err() {
            let _ = fs::remove_file(&stage);
            return Err(say(format!(
                "{retired_entry} could not be made; nothing was rotated"
            )));
        }
        // 3. the new value under the name.
        if fs::rename(&stage, self.file(name)).is_err() {
            let _ = fs::remove_file(&stage);
            let _ = fs::remove_file(self.file(&retired_entry));
            return Err(say(format!(
                "the new value could not be put under {name}; nothing was rotated"
            )));
        }
        // 4. the metadata.
        let half = || {
            say(format!(
                "{name} holds its new value but its metadata could not be written; the next rotation of {name} repairs it"
            ))
        };
        let meta_stage = self
            .stage(&meta, format!("{metadata}\n").as_bytes())
            .map_err(|_| half())?;
        if fs::rename(&meta_stage, self.file(&meta)).is_err() {
            let _ = fs::remove_file(&meta_stage);
            return Err(half());
        }
        // 5. the directory.
        directory
            .sync_all()
            .map_err(|_| say("the store directory could not be synced"))?;
        println!(
            "spawn secret: rotated {name}; its previous value is kept as {retired_entry}, served by nothing"
        );
        Ok(())
    }

    /// `name`'s retired values (number and bytes, oldest first), and the
    /// retired names that are the current file `held` itself (a rotation
    /// killed between its link and its rename), which are not retired values.
    fn retired(&self, name: &str, held: &fs::Metadata) -> Result<Retired, Refusal> {
        let (mut retired, mut leftovers) = (Vec::new(), Vec::new());
        for n in retired_numbers(name, &self.entries()?) {
            let entry = format!("{name}.retired.{n}");
            let Some((metadata, old)) = self.read_regular(&entry) else {
                return Err(say(format!(
                    "{entry} could not be read as a regular file; nothing was rotated"
                )));
            };
            if metadata.dev() == held.dev() && metadata.ino() == held.ino() {
                leftovers.push(entry);
            } else {
                retired.push((n, old));
            }
        }
        Ok((retired, leftovers))
    }

    /// `name`'s metadata after a rotation to `value`: 122's fields kept,
    /// `rotated` now, the new value's fingerprint, and `retired` rebuilt from
    /// the retired files and `current` as number `k`, each entry's time the
    /// old metadata's ("?" for a file it did not know).
    fn rotated_metadata(
        &self,
        name: &str,
        key: &Key,
        value: &Value,
        current: &Value,
        retired: &[(u64, Value)],
        k: u64,
    ) -> serde_json::Value {
        let old: serde_json::Map<String, serde_json::Value> =
            fs::read(self.file(&format!("{name}.json")))
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_default();
        let known = |n: u64| {
            old.get("retired")
                .and_then(serde_json::Value::as_array)
                .and_then(|entries| {
                    entries
                        .iter()
                        .find(|entry| entry.get("n").and_then(serde_json::Value::as_u64) == Some(n))
                })
                .and_then(|entry| entry.get("retired").cloned())
                .unwrap_or_else(|| "?".into())
        };
        let now = now_rfc3339();
        let mut entries: Vec<serde_json::Value> = retired
            .iter()
            .map(|(n, old)| {
                serde_json::json!({"n": n, "retired": known(*n), "fingerprint": fingerprint(&key.0, &old.0)})
            })
            .collect();
        entries.push(serde_json::json!({"n": k, "retired": now, "fingerprint": fingerprint(&key.0, &current.0)}));
        let mut metadata = old.clone();
        metadata.entry("name").or_insert_with(|| name.into());
        metadata.entry("tier").or_insert_with(|| 1.into());
        metadata
            .entry("created")
            .or_insert_with(|| now.clone().into());
        metadata.insert("rotated".into(), now.into());
        metadata.insert("fingerprint".into(), fingerprint(&key.0, &value.0).into());
        metadata.insert("retired".into(), entries.into());
        serde_json::Value::Object(metadata)
    }

    /// One line per secret, sorted, never a value; a killed write's leftovers
    /// are named as such.
    fn list(&self) -> Result<(), Refusal> {
        let _lock = self.lock(false)?;
        let key = self.key()?;
        let entries = self.entries()?;
        let has = |entry: &str| entries.iter().any(|e| e == entry);
        let mut lines = Vec::new();
        for entry in &entries {
            if staged(entry) {
                lines.push(format!(
                    "{entry}  staged: a killed write's copy; the next set, generate or rotate removes it"
                ));
            } else if valid_name(entry) {
                if has(&format!("{entry}.json")) {
                    lines.push(self.stored_line(entry, key.as_ref()));
                } else {
                    lines.push(format!(
                        "{entry}  incomplete: a value without its metadata (a killed write); a set or generate of {entry} replaces it"
                    ));
                }
            } else if let Some(name) = entry.strip_suffix(".json")
                && valid_name(name)
                && !has(name)
            {
                lines.push(format!(
                    "{name}  incomplete: metadata without its value (a killed write); a set or generate of {name} replaces it"
                ));
            }
        }
        lines.sort();
        let mut out = std::io::stdout().lock();
        for line in lines {
            let _ = writeln!(out, "{line}");
        }
        Ok(())
    }

    /// `NAME  tier=1  created=<time>  rotated=<time|->  retired=<n>  fp=<fp>`:
    /// fp the metadata's fingerprint when it is the value's under `key`, `-`
    /// when the metadata has none, `?` when it is not the value's.
    fn stored_line(&self, name: &str, key: Option<&Key>) -> String {
        let metadata: Option<serde_json::Value> = fs::read(self.file(&format!("{name}.json")))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        let field = |key: &str| {
            metadata
                .as_ref()
                .and_then(|m| m.get(key))
                .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string))
        };
        match (field("tier"), field("created")) {
            (Some(tier), Some(created)) => {
                let rotated = field("rotated").unwrap_or_else(|| "-".into());
                let retired = metadata
                    .as_ref()
                    .and_then(|m| m.get("retired"))
                    .and_then(serde_json::Value::as_array)
                    .map_or(0, Vec::len);
                let recorded = metadata
                    .as_ref()
                    .and_then(|m| m.get("fingerprint"))
                    .and_then(serde_json::Value::as_str);
                let fp = match (recorded, key, self.read_regular(name)) {
                    (None, _, _) => "-".to_string(),
                    (Some(fp), Some(key), Some((_, value)))
                        if fingerprint(&key.0, &value.0) == fp =>
                    {
                        fp.to_string()
                    }
                    _ => "?".to_string(),
                };
                format!(
                    "{name}  tier={tier}  created={created}  rotated={rotated}  retired={retired}  fp={fp}"
                )
            }
            _ => format!("{name}  unreadable metadata"),
        }
    }
}

/// A name's retired values (number and bytes, oldest first) and the retired
/// names that are its current file (a killed rotation's leftovers).
type Retired = (Vec<(u64, Value)>, Vec<String>);

/// The refusal of a name not in the store.
fn not_in_store(name: &str) -> Refusal {
    say(format!(
        "{name} is not in the store (set it first); nothing was read or stored"
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        Line, Refusal, base64url, edit_line, fingerprint, retired_numbers, rotate_flags, set_flags,
        staged, valid_name,
    };

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|w| (*w).to_string()).collect()
    }

    #[test]
    fn base64url_has_no_padding_and_the_url_alphabet() {
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(&[0xfb, 0xff, 0xfe]), "-__-");
        let encoded = base64url(&[0xa5; 32]);
        assert_eq!(encoded.len(), 43);
        assert!(!encoded.contains(['=', '+', '/']));
    }

    #[test]
    fn names_are_capitals_digits_and_underscores() {
        for good in ["A", "DB_PASSWORD", "X9", "A_", &"A".repeat(64)] {
            assert!(valid_name(good), "{good}");
        }
        for bad in [
            "",
            "a",
            "Ab",
            "9A",
            "_A",
            "-",
            "../A",
            "A/B",
            "A.json",
            "A-B",
            &"A".repeat(65),
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn set_refuses_an_extra_word_without_repeating_it_and_tier_two() {
        let Err(Refusal::Say(sentence)) = set_flags(&words(&["NAME", "hunter2-value"])) else {
            panic!("an extra word is refused");
        };
        assert!(!sentence.contains("hunter2"), "{sentence}");
        let Err(Refusal::Say(sentence)) = set_flags(&words(&["NAME", "--tier", "2"])) else {
            panic!("tier 2 is refused");
        };
        assert!(sentence.contains("injectable"), "{sentence}");
        assert!(set_flags(&words(&["NAME", "--tier", "3"])).is_err());
        assert!(set_flags(&words(&["NAME", "--tier"])).is_err());
        assert_eq!(
            set_flags(&words(&["NAME", "--tier", "1"])),
            Ok("NAME".into())
        );
        assert_eq!(set_flags(&words(&["NAME"])), Ok("NAME".into()));
        assert_eq!(set_flags(&words(&[])), Err(Refusal::Usage));
        assert!(set_flags(&words(&["lower"])).is_err());
    }

    fn typed(keys: &[u8]) -> Option<Vec<u8>> {
        match edit_line(&mut &keys[..]) {
            Line::Value(value) => Some(value.0.clone()),
            Line::Cancelled => None,
            Line::TooLong | Line::Failed => panic!("unexpected"),
        }
    }

    #[test]
    fn the_terminal_line_ends_erases_and_cancels() {
        assert_eq!(typed(b"abc\nrest").as_deref(), Some(&b"abc"[..]));
        assert_eq!(typed(b"abc\rrest").as_deref(), Some(&b"abc"[..]));
        assert_eq!(typed(b"abc\x04rest").as_deref(), Some(&b"abc"[..]));
        assert_eq!(typed(b"abc").as_deref(), Some(&b"abc"[..]));
        assert_eq!(typed("aé\x7f\n".as_bytes()).as_deref(), Some(&b"a"[..]));
        assert_eq!(typed(b"abZ\x08\n").as_deref(), Some(&b"ab"[..]));
        assert_eq!(typed(b"\x7f\x7fx\n").as_deref(), Some(&b"x"[..]));
        assert_eq!(typed(b"wrong\x15right\n").as_deref(), Some(&b"right"[..]));
        assert_eq!(typed(b"secret\x03more\n"), None);
    }

    #[test]
    fn staged_files_are_dotted_new_files_of_a_name() {
        assert!(staged(".DB.123.new"));
        assert!(staged(".DB.json.4194301.new"));
        assert!(staged("..fingerprint.key.4194301.new"));
        for not in [
            ".fingerprint.key",
            "DB.123.new",
            ".DB.new",
            ".DB.x1.new",
            ".db.1.new",
            "DB",
            "DB.json",
        ] {
            assert!(!staged(not), "{not}");
        }
    }

    #[test]
    fn the_fingerprint_is_hmac_sha256_cut_to_12_hex() {
        // RFC 4231, test case 2: 5bdcc146bf60754e6a042426089575c7…
        assert_eq!(
            fingerprint(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60"
        );
        assert_ne!(fingerprint(b"Jefe", b"a"), fingerprint(b"Jeff", b"a"));
    }

    #[test]
    fn retired_numbers_are_the_names_own_and_sorted() {
        let entries = words(&[
            "DB",
            "DB.json",
            "DB.retired.10",
            "DB.retired.2",
            "DB.retired.1",
            "DB.retired.",
            "DB.retired.x",
            "DB.retired.3a",
            "DB.retired.03",
            "DB.retired.0",
            "DB.retired.-4",
            "DB_B.retired.5",
            "DBX.retired.6",
            ".DB.retired.7",
            "DB.retired.99999999999999999999999",
        ]);
        assert_eq!(retired_numbers("DB", &entries), [1, 2, 10]);
        assert_eq!(retired_numbers("DB_B", &entries), [5]);
        assert_eq!(retired_numbers("NONE", &entries), Vec::<u64>::new());
    }

    #[test]
    fn rotate_reads_or_generates_and_refuses_an_extra_word_and_tier_two() {
        assert_eq!(rotate_flags(&words(&["NAME"])), Ok(("NAME".into(), None)));
        assert_eq!(
            rotate_flags(&words(&["NAME", "--tier", "1"])),
            Ok(("NAME".into(), None))
        );
        assert_eq!(
            rotate_flags(&words(&["NAME", "--generate", "--bytes", "32"])),
            Ok(("NAME".into(), Some(32)))
        );
        assert_eq!(
            rotate_flags(&words(&["NAME", "--bytes=16", "--generate"])),
            Ok(("NAME".into(), Some(16)))
        );
        let Err(Refusal::Say(sentence)) = rotate_flags(&words(&["NAME", "hunter2-value"])) else {
            panic!("an extra word is refused");
        };
        assert!(!sentence.contains("hunter2"), "{sentence}");
        let Err(Refusal::Say(sentence)) = rotate_flags(&words(&["NAME", "--tier", "2"])) else {
            panic!("tier 2 is refused");
        };
        assert!(sentence.contains("injectable"), "{sentence}");
        assert!(rotate_flags(&words(&["NAME", "--generate"])).is_err());
        assert!(rotate_flags(&words(&["NAME", "--bytes", "32"])).is_err());
        assert!(rotate_flags(&words(&["NAME", "--generate", "--bytes", "8"])).is_err());
        assert_eq!(rotate_flags(&words(&[])), Err(Refusal::Usage));
        assert!(rotate_flags(&words(&["lower"])).is_err());
    }
}
