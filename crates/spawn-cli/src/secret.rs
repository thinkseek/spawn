//! `spawn secret set|generate|list`: the store of secrets by name
//! (sync/design-secret-injection.md, section 5, step B).
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

use std::fs::{self, File, OpenOptions};
use std::io::{IsTerminal, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// The usage, printed for `get` and every verb that is not one.
const USAGE: &str = "usage: spawn secret set NAME [--tier 1]   (the value from a pipe, or typed at a terminal, not echoed)\n       spawn secret generate NAME --bytes N   (N random bytes, 16..1024, made inside the store)\n       spawn secret list\nThere is no get: no command prints a value.";

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
        match tier {
            Some("1") => {}
            Some("2") => {
                return Err(say(
                    "tier 2 is refused: no secret is injectable (owner decision 2), so nothing was read or stored",
                ));
            }
            _ => return Err(say("the only tier is 1 (--tier 1); nothing was stored")),
        }
    }
    Ok(name)
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
        match count.and_then(|c| c.parse::<usize>().ok()) {
            Some(n) if (16..=1024).contains(&n) => bytes = Some(n),
            _ => return Err(say("--bytes takes a number from 16 to 1024")),
        }
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

/// A staged file a write makes: `.<NAME>.<pid>.new` or `.<NAME>.json.<pid>.new`.
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
    !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()) && valid_name(name)
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
        for entry in self.entries()? {
            if staged(&entry) {
                fs::remove_file(self.file(&entry)).map_err(|_| {
                    say("a killed write's staged file could not be removed; nothing was stored")
                })?;
            }
        }
        let meta = format!("{name}.json");
        let (has_value, has_meta) = (self.present(name), self.present(&meta));
        if has_value && has_meta {
            return Err(say(format!(
                "{name} already exists and keeps its value; nothing was stored"
            )));
        }
        let incomplete = has_value || has_meta;
        for (present, entry) in [(has_value, name), (has_meta, meta.as_str())] {
            if present {
                fs::remove_file(self.file(entry)).map_err(|_| {
                    say(format!(
                        "the incomplete {name} could not be removed; nothing was stored"
                    ))
                })?;
            }
        }
        let metadata = serde_json::json!({"name": name, "tier": 1, "created": now_rfc3339()});
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

    /// One file: staged (created new, 600), fsynced, hard-linked to its name,
    /// the staged file removed.
    fn place(&self, entry: &str, bytes: &[u8]) -> Result<(), Refusal> {
        let stage = self.file(&format!(".{entry}.{}.new", std::process::id()));
        let failed = |_| say(format!("{entry} could not be written; it was not stored"));
        let result = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&stage)?;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            file.write_all(bytes)?;
            file.sync_all()?;
            fs::hard_link(&stage, self.file(entry))
        })();
        let _ = fs::remove_file(&stage);
        result.map_err(failed)
    }

    /// One line per secret, sorted, never a value; a killed write's leftovers
    /// are named as such.
    fn list(&self) -> Result<(), Refusal> {
        let _lock = self.lock(false)?;
        let entries = self.entries()?;
        let has = |entry: &str| entries.iter().any(|e| e == entry);
        let mut lines = Vec::new();
        for entry in &entries {
            if staged(entry) {
                lines.push(format!(
                    "{entry}  staged: a killed write's copy; the next set or generate removes it"
                ));
            } else if valid_name(entry) {
                if has(&format!("{entry}.json")) {
                    lines.push(self.stored_line(entry));
                } else {
                    lines.push(format!(
                        "{entry}  incomplete: a value without its metadata (a killed write); a set or generate of {entry} replaces it"
                    ));
                }
            } else if let Some(name) = entry.strip_suffix(".json") {
                if valid_name(name) && !has(name) {
                    lines.push(format!(
                        "{name}  incomplete: metadata without its value (a killed write); a set or generate of {name} replaces it"
                    ));
                }
            }
        }
        lines.sort();
        let mut out = std::io::stdout().lock();
        for line in lines {
            let _ = writeln!(out, "{line}");
        }
        Ok(())
    }

    fn stored_line(&self, name: &str) -> String {
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
            (Some(tier), Some(created)) => format!("{name}  tier={tier}  created={created}"),
            _ => format!("{name}  unreadable metadata"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Line, Refusal, base64url, edit_line, set_flags, staged, valid_name};

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
}
