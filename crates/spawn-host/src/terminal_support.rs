//! Shared OS-level terminal host support: environment scrubbing, private
//! launch files, process-tree cleanup, and workspace/transcript file I/O.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use spawn_core::id::{TerminalName, WorkspaceId};
use spawn_core::port::{LIST_DIRS_LIMIT, READ_LIMIT, TerminalError};

/// Environment inputs for terminal hosts ([`crate::HerdrHost`], [`crate::HerdrCoreHost`],
/// [`crate::HerdrPool`]), injected at startup from the host configuration boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalHostEnv {
    /// User home directory (`$HOME`), used for XDG defaults and straggler-sweep safety.
    pub home: Option<PathBuf>,
    /// `$XDG_STATE_HOME` override for `herdr-core` state directories.
    pub xdg_state_home: Option<PathBuf>,
    /// `$XDG_CONFIG_HOME` override for `herdr-core` config directories.
    pub xdg_config_home: Option<PathBuf>,
    /// `$XDG_RUNTIME_DIR` override for sockets and terminal state records.
    pub xdg_runtime_dir: Option<PathBuf>,
    /// Override for legacy `herdr` launch-clear timeout (`SPAWN_HERDR_LAUNCH_CLEAR_MS`).
    pub launch_clear_limit: Option<Duration>,
    /// Parent environment variable keys matching controller scrub rules to strip from
    /// spawned terminal daemon child processes.
    pub scrubbed_env_keys: Vec<OsString>,
}

impl TerminalHostEnv {
    /// Filter `keys` down to those matching [`crate::herdr::scrub`] rules so spawned
    /// terminal daemons never inherit controller secrets, `SPAWN_*`, `HERDR_*`, or `HC_*`.
    #[must_use]
    pub fn collect_scrubbed_keys(keys: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
        let mut out: Vec<OsString> = keys
            .into_iter()
            .filter(|name| crate::herdr::scrub(name))
            .collect();
        out.sort();
        out.dedup();
        out
    }
}

/// Variables kept out of spawned terminal server environments, besides every
/// `SPAWN_*` one (see [`scrub`]).
const SCRUBBED: [&str; 2] = ["PGPASSWORD", "TYPESAFE_API_KEY"];

const STATIC_HERDR_SCRUBBED: [&str; 6] = [
    "PGPASSWORD",
    "TYPESAFE_API_KEY",
    "HERDR_SOCKET_PATH",
    "HERDR_SESSION",
    "HC_SOCKET",
    "HC_SESSION",
];

pub(crate) fn remove_scrubbed_env(command: &mut Command, host_env: &TerminalHostEnv) {
    for key in STATIC_HERDR_SCRUBBED {
        command.env_remove(key);
    }
    for key in &host_env.scrubbed_env_keys {
        command.env_remove(key);
    }
}

/// Whether a variable of the controller's stays out of terminal hosts: every
/// `SPAWN_*` setting (database logins, the serve token, repository and
/// workspace root) and the secrets in [`SCRUBBED`]. A pane gets only what its
/// launch passes explicitly; an agent that runs the black-box or e2e scripts
/// in its pane must not reach the live fleet.
pub(crate) fn scrub(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| name.starts_with("SPAWN_") || SCRUBBED.contains(&name))
}

/// How long [`spawn_core::port::TerminalHost::terminate`] waits for a killed
/// terminal's processes to exit after closing hangs their terminal up.
pub(crate) const HANGUP_WAIT: Duration = Duration::from_secs(5);
/// How long [`spawn_core::port::TerminalHost::terminate`] waits after sending survivors SIGKILL.
pub(crate) const KILL_WAIT: Duration = Duration::from_secs(2);

/// How often that wait looks.
const GONE_POLL: Duration = Duration::from_millis(20);

/// The kind of [`write_private`] file a launch's environment goes through.
pub(crate) const LAUNCH: &str = "launch";

/// Send `signal` to every live process in the sessions led by `leaders`.
pub(crate) fn signal_sessions(leaders: &[u32], signal: rustix::process::Signal) {
    for pid in session_members(leaders) {
        if let Some(pid) = rustix::process::Pid::from_raw(pid.cast_signed()) {
            let _ = rustix::process::kill_process(pid, signal);
        }
    }
}

/// Wait for every process in the sessions led by `leaders` to exit after
/// `closed_by` (for `waits.0`), then SIGKILL survivors and wait `waits.1`.
pub(crate) fn wait_sessions_gone(
    terminal: &TerminalName,
    leaders: &[u32],
    waits: (Duration, Duration),
    closed_by: &str,
) -> Result<(), TerminalError> {
    let (hangup, kill) = waits;
    if until_gone(leaders, hangup).is_empty() {
        return Ok(());
    }
    signal_sessions(leaders, rustix::process::Signal::KILL);
    let left = until_gone(leaders, kill);
    if left.is_empty() {
        return Ok(());
    }
    Err(TerminalError(format!(
        "{} process(es) of {terminal}'s runner outlived {closed_by} and SIGKILL: {left:?}",
        left.len()
    )))
}

/// End the processes left beneath a terminated terminal's workspace (a
/// daemon the engine detached: agy's `--bg-updater`, a `nohup` server) by
/// pid, before the caller's final checkpoint (design section 5.4): every
/// live process of this user whose working directory is `directory` or
/// below it, read from `/proc/<pid>/cwd` (an exact link, never an argument
/// pattern), gets SIGTERM, and SIGKILL after `waits.0`; one left after
/// `waits.1` more is an error naming it. A signal goes through a pidfd
/// opened while the process's directory still matched, so a reused pid is
/// never signalled. Never this process; never a directory too broad to be
/// one workspace ([`sweepable`]).
pub(crate) fn end_stragglers(
    directory: &str,
    waits: (Duration, Duration),
    home_dir: Option<&Path>,
) -> Result<(), TerminalError> {
    let directory = std::path::Path::new(directory);
    if !sweepable(directory, home_dir) {
        return Ok(());
    }
    for (signal, wait) in [
        (rustix::process::Signal::TERM, waits.0),
        (rustix::process::Signal::KILL, waits.1),
    ] {
        let found = stragglers_beneath(directory);
        if found.is_empty() {
            return Ok(());
        }
        for pid in &found {
            signal_beneath(*pid, directory, signal);
        }
        let deadline = Instant::now() + wait;
        while !stragglers_beneath(directory).is_empty() && Instant::now() < deadline {
            std::thread::sleep(GONE_POLL);
        }
    }
    let left = stragglers_beneath(directory);
    if left.is_empty() {
        return Ok(());
    }
    Err(TerminalError(format!(
        "{} process(es) left beneath {} outlived SIGTERM and SIGKILL: {left:?}",
        left.len(),
        directory.display()
    )))
}

/// Whether `directory` may be swept: an absolute path of at least three
/// parts (`/tmp/a` and `/home/user` are never one workspace), that is not
/// this process's working directory, the home directory or the temporary
/// directory, nor above any of them.
fn sweepable(directory: &std::path::Path, home_dir: Option<&Path>) -> bool {
    use std::path::Component;
    if !directory.is_absolute()
        || directory
            .components()
            .filter(|part| matches!(part, Component::Normal(_)))
            .count()
            < 3
        || directory
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return false;
    }
    let shared = [
        std::env::current_dir().ok(),
        home_dir.map(Path::to_path_buf),
        crate::home::Home::of_account().path,
        Some(std::env::temp_dir()),
    ];
    !shared
        .into_iter()
        .flatten()
        .any(|shared| shared.starts_with(directory))
}

/// The live processes of this user, this one excepted, whose working
/// directory is `directory` or below it, by `/proc/<pid>/cwd`.
fn stragglers_beneath(directory: &std::path::Path) -> Vec<u32> {
    use std::os::unix::fs::MetadataExt;
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let uid = crate::process::current_uid();
    let own = std::process::id();
    let mut found: Vec<u32> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| *pid != own)
        .filter(|pid| {
            std::fs::metadata(format!("/proc/{pid}")).is_ok_and(|meta| meta.uid() == uid)
                && std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    .ok()
                    .and_then(|stat| session_of(&stat))
                    .is_some_and(|(state, _)| state != 'Z')
                && cwd_beneath(*pid, directory)
        })
        .collect();
    found.sort_unstable();
    found
}

/// Whether `/proc/<pid>/cwd` is `directory` or below it.
fn cwd_beneath(pid: u32, directory: &std::path::Path) -> bool {
    std::fs::read_link(format!("/proc/{pid}/cwd")).is_ok_and(|cwd| cwd.starts_with(directory))
}

/// `signal` to `pid` through a pidfd, only while its working directory is
/// still beneath `directory` once the pidfd holds it.
#[cfg(target_os = "linux")]
fn signal_beneath(pid: u32, directory: &std::path::Path, signal: rustix::process::Signal) {
    let Some(raw) = rustix::process::Pid::from_raw(pid.cast_signed()) else {
        return;
    };
    let Ok(pidfd) = rustix::process::pidfd_open(raw, rustix::process::PidfdFlags::empty()) else {
        return;
    };
    if cwd_beneath(pid, directory) {
        let _ = rustix::process::pidfd_send_signal(&pidfd, signal);
    }
}

/// Without pidfds (every target but Linux) no signal is sent: a signal by
/// pid after a check of its directory could reach a process that reused the
/// pid in between, which [`end_stragglers`] promises never happens. Nor is
/// one needed: stragglers are found through `/proc/<pid>/cwd`, so where
/// there is no pidfd there is no `/proc` either and none is ever found. Were
/// one found, it would outlive both rounds and [`end_stragglers`] would name
/// it in its error.
#[cfg(not(target_os = "linux"))]
fn signal_beneath(_pid: u32, _directory: &std::path::Path, _signal: rustix::process::Signal) {}

/// The processes still in the sessions `leaders` lead once none is left or
/// `limit` passes.
fn until_gone(leaders: &[u32], limit: Duration) -> Vec<u32> {
    let deadline = Instant::now() + limit;
    loop {
        let left = session_members(leaders);
        if left.is_empty() || Instant::now() >= deadline {
            return left;
        }
        std::thread::sleep(GONE_POLL);
    }
}

/// The live processes (a zombie writes nothing) whose session is one of
/// `leaders`, by `/proc/<pid>/stat`. A process that left the session
/// (`setsid`) is not among them.
fn session_members(leaders: &[u32]) -> Vec<u32> {
    if leaders.is_empty() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut members: Vec<u32> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| session_of(&stat))
                .is_some_and(|(state, session)| state != 'Z' && leaders.contains(&session))
        })
        .collect();
    members.sort_unstable();
    members
}

/// A `/proc/<pid>/stat` line's state and session: the fields after the
/// command's closing parenthesis (a command can hold spaces and parentheses)
/// are state, ppid, pgrp, session.
fn session_of(stat: &str) -> Option<(char, u32)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let session = fields.nth(2)?.parse().ok()?;
    Some((state, session))
}

/// `environment` as `NAME='value'` lines for a shell to source.
pub(crate) fn environment_text(environment: &[(String, String)]) -> Result<String, TerminalError> {
    let mut text = String::new();
    for (key, value) in environment {
        let name_ok = key
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !name_ok {
            return Err(TerminalError(format!(
                "{key} is not an environment variable name"
            )));
        }
        text.push_str(key);
        text.push('=');
        text.push_str(&spawn_core::shell::shell_quote(value));
        text.push('\n');
    }
    Ok(text)
}

/// `text` in a new file only this uid reads (mode 600, created exclusively)
/// in `directory`: what must reach a pane launch without appearing on any
/// command line in `/proc`. The caller removes it.
pub(crate) fn write_private(
    directory: &std::path::Path,
    terminal: &TerminalName,
    kind: &str,
    extension: &str,
    what: &str,
    text: &str,
) -> Result<std::path::PathBuf, TerminalError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let path = directory.join(format!(
        "spawn-{kind}-{}-{}-{nanos}.{extension}",
        terminal.as_str(),
        std::process::id()
    ));
    let failed =
        |error: std::io::Error| TerminalError(format!("write {what} of {terminal}: {error}"));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(failed)?;
    if let Err(error) = file.write_all(text.as_bytes()) {
        let _ = std::fs::remove_file(&path);
        return Err(failed(error));
    }
    Ok(path)
}

/// [`spawn_core::port::TerminalHost::read_from`] on this machine's files:
/// below `beneath` when present, through the workspace for workspace files,
/// or safely via `O_NOFOLLOW | O_NONBLOCK` otherwise.
pub(crate) fn read_from_files(
    beneath: Option<&crate::beneath::Beneath>,
    workspace: &WorkspaceId,
    path: &str,
    offset: u64,
) -> Result<Option<Vec<u8>>, TerminalError> {
    if let Some(beneath) = beneath
        && let Some(relative) = beneath.relative(std::path::Path::new(path))
    {
        return read_beneath(beneath, relative, offset);
    }
    // A file in the workspace (the engine's exit status) is the agent's:
    // opened the safe way, from the workspace through held directories.
    let top = workspace.path().trim_end_matches('/');
    match path
        .strip_prefix(top)
        .and_then(|rest| rest.strip_prefix('/'))
    {
        Some(relative) if !top.is_empty() => {
            read_in_workspace(std::path::Path::new(top), relative, offset)
        }
        _ => read_local(path, offset),
    }
}

/// [`spawn_core::port::TerminalHost::write_new`] on this machine's files,
/// below `beneath` through it ([`read_from_files`]).
pub(crate) fn write_new_file(
    beneath: Option<&crate::beneath::Beneath>,
    path: &str,
    bytes: &[u8],
) -> Result<bool, TerminalError> {
    if let Some(beneath) = beneath
        && let Some(relative) = beneath.relative(std::path::Path::new(path))
    {
        return beneath.write_new(relative, bytes).map_err(TerminalError);
    }
    write_local(path, bytes)
}

/// [`spawn_core::port::TerminalHost::replace_grown`] on this machine's files,
/// below `beneath` through it ([`read_from_files`]).
pub(crate) fn replace_grown_file(
    beneath: Option<&crate::beneath::Beneath>,
    path: &str,
    bytes: &[u8],
    current: &[u8],
) -> Result<bool, TerminalError> {
    if let Some(beneath) = beneath
        && let Some(relative) = beneath.relative(std::path::Path::new(path))
    {
        return beneath
            .replace_grown(relative, bytes, current)
            .map_err(TerminalError);
    }
    replace_local(path, bytes, current)
}

/// Replace the regular file `path` with `bytes` if it holds exactly
/// `current`, never with fewer bytes: whether it was replaced. The bytes go
/// to a temporary file beside it first, made durable, with the file's mode,
/// and a rename puts it in place, so the file is whole, old or new, at every
/// moment.
fn replace_local(path: &str, bytes: &[u8], current: &[u8]) -> Result<bool, TerminalError> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let failed =
        |what: &str, error: std::io::Error| TerminalError(format!("{what} {path}: {error}"));
    if bytes.len() < current.len() {
        return Err(TerminalError(format!(
            "replacing {path} would shorten it: {} bytes for {}",
            bytes.len(),
            current.len()
        )));
    }
    let target = std::path::Path::new(path);
    let metadata = match target.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(failed("inspect", error)),
    };
    if !metadata.is_file() {
        return Err(TerminalError(format!("{path} is not a regular file")));
    }
    if std::fs::read(target).map_err(|error| failed("read", error))? != current {
        return Ok(false);
    }
    let (Some(parent), Some(name)) = (target.parent(), target.file_name()) else {
        return Err(TerminalError(format!("{path} names no file")));
    };
    let temporary = parent.join(format!(
        ".{}.spawn-replace-{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&temporary);
    // Made with the file's mode, so it is never more open than the file,
    // even before the mode is set exactly (the umask only narrows it).
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(metadata.permissions().mode() & 0o7777)
        .open(&temporary)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.set_permissions(metadata.permissions())?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&temporary, target))
        // The rename is durable once the directory is.
        .and_then(|()| std::fs::File::open(parent)?.sync_all());
    match written {
        Ok(()) => Ok(true),
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(failed("replace", error))
        }
    }
}

/// Create `path` holding `bytes` unless something is there: whether it was
/// written. The bytes go to a temporary file beside it first, made durable,
/// and a hard link puts it in place, which never replaces an existing file:
/// the file appears whole or not at all.
fn write_local(path: &str, bytes: &[u8]) -> Result<bool, TerminalError> {
    use std::io::Write;
    let target = std::path::Path::new(path);
    if target.symlink_metadata().is_ok() {
        return Ok(false);
    }
    let failed =
        |what: &str, error: std::io::Error| TerminalError(format!("{what} {path}: {error}"));
    let (Some(parent), Some(name)) = (target.parent(), target.file_name()) else {
        return Err(TerminalError(format!("{path} names no file")));
    };
    std::fs::create_dir_all(parent).map_err(|error| failed("create the directory of", error))?;
    let temporary = parent.join(format!(
        ".{}.spawn-write-{}",
        name.to_string_lossy(),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&temporary);
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        });
    let linked = written.and_then(|()| std::fs::hard_link(&temporary, target));
    let _ = std::fs::remove_file(&temporary);
    match linked {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(failed("write", error)),
    }
}

/// At most [`READ_LIMIT`] bytes from `offset` of the file `relative` in
/// the workspace at `top`, through [`crate::agent_files::open_regular`];
/// `None` when it does not exist.
fn read_in_workspace(
    top: &std::path::Path,
    relative: &str,
    offset: u64,
) -> Result<Option<Vec<u8>>, TerminalError> {
    use std::io::{Read, Seek, SeekFrom};
    let shown = || format!("{}/{relative}", top.display());
    let opened = crate::agent_files::open_regular(top, &crate::tree_path::TreePath::from(relative))
        .map_err(|error| TerminalError(format!("open {}: {error}", shown())))?;
    let Some(crate::agent_files::Opened { file, .. }) = opened else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    let mut file = file;
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| (&mut file).take(READ_LIMIT as u64).read_to_end(&mut bytes))
        .map_err(|error| TerminalError(format!("read {}: {error}", shown())))?;
    Ok(Some(bytes))
}

/// At most [`READ_LIMIT`] bytes from `offset` of the file `relative` below
/// `beneath`; `None` when it does not exist.
fn read_beneath(
    beneath: &crate::beneath::Beneath,
    relative: &std::path::Path,
    offset: u64,
) -> Result<Option<Vec<u8>>, TerminalError> {
    use std::io::{Read, Seek, SeekFrom};
    let Some(mut file) = beneath.open_read(relative).map_err(TerminalError)? else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| (&mut file).take(READ_LIMIT as u64).read_to_end(&mut bytes))
        .map_err(|error| {
            TerminalError(format!(
                "read {}: {error}",
                beneath.root().join(relative).display()
            ))
        })?;
    Ok(Some(bytes))
}

/// [`spawn_core::port::TerminalHost::list_dirs`] on this machine's files: below
/// `beneath` through it ([`read_from_files`]), else the local directory, opened
/// without following a symlink at its last component. Each entry is stated
/// without following it: only directories count, newest modification first
/// (then by name), at most [`LIST_DIRS_LIMIT`].
pub(crate) fn list_dirs_files(
    beneath: Option<&crate::beneath::Beneath>,
    dir: &str,
) -> Result<Vec<String>, TerminalError> {
    list_dirs_files_up_to(beneath, dir, LIST_DIRS_LIMIT)
}

/// [`spawn_core::port::TerminalHost::list_dirs_up_to`] on this machine's files:
/// identical symlink-safe traversal and newest-first ordering as [`list_dirs_files`],
/// truncated to `limit` entries rather than [`LIST_DIRS_LIMIT`].
pub(crate) fn list_dirs_files_up_to(
    beneath: Option<&crate::beneath::Beneath>,
    dir: &str,
    limit: usize,
) -> Result<Vec<String>, TerminalError> {
    use rustix::fs::{Mode, OFlags};
    if let Some(beneath) = beneath
        && let Some(relative) = beneath.relative(std::path::Path::new(dir))
    {
        let Some(opened) = beneath.open_dir(relative).map_err(TerminalError)? else {
            return Ok(Vec::new());
        };
        return newest_dirs(opened.descriptor(), dir, limit);
    }
    let fd = match rustix::fs::open(
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(Vec::new()),
        Err(errno) => return Err(TerminalError(format!("open {dir}: {errno}"))),
    };
    newest_dirs(&fd, dir, limit)
}

/// The subdirectories of the open directory `fd` (`shown` in errors), as
/// [`list_dirs_files_up_to`] gives them.
fn newest_dirs(
    fd: &std::os::fd::OwnedFd,
    shown: &str,
    limit: usize,
) -> Result<Vec<String>, TerminalError> {
    use rustix::fs::{AtFlags, FileType};
    let mut entries = rustix::fs::Dir::read_from(fd)
        .map_err(|errno| TerminalError(format!("list {shown}: {errno}")))?;
    let mut found: Vec<((i64, i64), String)> = Vec::new();
    while let Some(entry) = entries.read() {
        let entry = entry.map_err(|errno| TerminalError(format!("list {shown}: {errno}")))?;
        let Ok(name) = entry.file_name().to_str() else {
            continue;
        };
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            continue;
        }
        // Not followed: a symlink is a symlink, never the directory it names.
        let Ok(stat) = rustix::fs::statat(fd, name, AtFlags::SYMLINK_NOFOLLOW) else {
            continue;
        };
        if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
            continue;
        }
        #[allow(clippy::useless_conversion)] // the fields' widths differ by platform
        let modified = (
            i64::from(stat.st_mtime),
            i64::try_from(stat.st_mtime_nsec).unwrap_or(0),
        );
        found.push((modified, name.to_string()));
    }
    found.sort_by(|(a, a_name), (b, b_name)| b.cmp(a).then_with(|| a_name.cmp(b_name)));
    found.truncate(limit);
    Ok(found.into_iter().map(|(_, name)| name).collect())
}

/// At most [`READ_LIMIT`] bytes of a local file from `offset`; `None` when
/// it does not exist. The agent controls the path: a symlink is not followed
/// and a FIFO does not block the open (`O_NOFOLLOW | O_NONBLOCK`), and
/// anything but a regular file is refused before a byte is read.
fn read_local(path: &str, offset: u64) -> Result<Option<Vec<u8>>, TerminalError> {
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::fs::OpenOptionsExt;
    let opened = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path);
    let file = match opened {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(TerminalError(format!("open {path}: {error}"))),
    };
    let regular = file
        .metadata()
        .map_err(|error| TerminalError(format!("stat {path}: {error}")))?
        .is_file();
    if !regular {
        return Err(TerminalError(format!("{path} is not a regular file")));
    }
    let mut bytes = Vec::new();
    let mut file = file;
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| (&mut file).take(READ_LIMIT as u64).read_to_end(&mut bytes))
        .map_err(|error| TerminalError(format!("read {path}: {error}")))?;
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        READ_LIMIT, end_stragglers, list_dirs_files, list_dirs_files_up_to, read_in_workspace,
        read_local, replace_local, stragglers_beneath, sweepable,
    };

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("spawn-read-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch_dir(dir: &std::path::Path, seconds: u64) {
        let opened = std::fs::File::open(dir).unwrap();
        opened
            .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(seconds))
            .unwrap();
    }

    /// F5: a directory's subdirectories, newest first, never a file, a
    /// symlink to a directory, or anything through a symlinked directory;
    /// at most 64; the same below a `Beneath`.
    #[test]
    fn list_dirs_gives_plain_subdirectories_newest_first() {
        let dir = scratch("list-dirs");
        let brain = dir.join("brain");
        std::fs::create_dir_all(&brain).unwrap();
        for (name, seconds) in [("old", 1_000), ("newest", 3_000), ("middle", 2_000)] {
            std::fs::create_dir(brain.join(name)).unwrap();
            touch_dir(&brain.join(name), seconds);
        }
        std::fs::write(brain.join("a-file"), b"x").unwrap();
        let elsewhere = dir.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        touch_dir(&elsewhere, 9_000);
        std::os::unix::fs::symlink(&elsewhere, brain.join("linked")).unwrap();
        let path = brain.to_str().unwrap();
        assert_eq!(
            list_dirs_files(None, path).unwrap(),
            vec!["newest", "middle", "old"]
        );
        let beneath = crate::beneath::Beneath::new(&dir);
        assert_eq!(
            list_dirs_files(Some(&beneath), path).unwrap(),
            vec!["newest", "middle", "old"]
        );
        // Missing: empty. A symlinked directory is never listed through.
        let missing = dir.join("missing");
        assert!(
            list_dirs_files(None, missing.to_str().unwrap())
                .unwrap()
                .is_empty()
        );
        assert!(
            list_dirs_files(Some(&beneath), missing.to_str().unwrap())
                .unwrap()
                .is_empty()
        );
        let link = dir.join("brain-link");
        std::os::unix::fs::symlink(&brain, &link).unwrap();
        assert!(list_dirs_files(None, link.to_str().unwrap()).is_err());
        assert!(list_dirs_files(Some(&beneath), link.to_str().unwrap()).is_err());
        // Bounded.
        let many = dir.join("many");
        std::fs::create_dir(&many).unwrap();
        for at in 0..70_u64 {
            let sub = many.join(format!("c{at:02}"));
            std::fs::create_dir(&sub).unwrap();
            touch_dir(&sub, 10_000 + at);
        }
        let listed = list_dirs_files(None, many.to_str().unwrap()).unwrap();
        assert_eq!(listed.len(), spawn_core::port::LIST_DIRS_LIMIT);
        assert_eq!(listed.first().map(String::as_str), Some("c69"));
        assert_eq!(listed.last().map(String::as_str), Some("c06"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_dirs_up_to_scales_past_64_preserving_newest_first_and_security() {
        let dir = scratch("list-dirs-up-to");
        let brain = dir.join("brain");
        std::fs::create_dir_all(&brain).unwrap();
        for at in 0..150_u64 {
            let sub = brain.join(format!("conv-{at:03}"));
            std::fs::create_dir(&sub).unwrap();
            touch_dir(&sub, 20_000 + at);
        }
        std::fs::write(brain.join("ignored-file"), b"data").unwrap();
        let outside = dir.join("outside");
        std::fs::create_dir(&outside).unwrap();
        touch_dir(&outside, 99_999);
        std::os::unix::fs::symlink(&outside, brain.join("ignored-symlink")).unwrap();

        let path = brain.to_str().unwrap();
        let all = list_dirs_files_up_to(None, path, 4096).unwrap();
        assert_eq!(all.len(), 150);
        assert_eq!(all.first().map(String::as_str), Some("conv-149"));
        assert_eq!(all.last().map(String::as_str), Some("conv-000"));
        assert!(!all.contains(&"ignored-file".to_string()));
        assert!(!all.contains(&"ignored-symlink".to_string()));

        let beneath = crate::beneath::Beneath::new(&dir);
        let top100 = list_dirs_files_up_to(Some(&beneath), path, 100).unwrap();
        assert_eq!(top100.len(), 100);
        assert_eq!(top100.first().map(String::as_str), Some("conv-149"));
        assert_eq!(top100.last().map(String::as_str), Some("conv-050"));

        assert!(list_dirs_files_up_to(None, path, 0).unwrap().is_empty());
        assert_eq!(
            list_dirs_files(None, path).unwrap().len(),
            spawn_core::port::LIST_DIRS_LIMIT
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// S16: a transcript is replaced only by a longer file, only while it
    /// still holds what was read, through a rename, with its mode.
    #[test]
    fn a_file_is_replaced_whole_only_by_more_and_only_as_it_was_read() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("replace-grown");
        let file = dir.join("n1.jsonl");
        let path = file.to_str().unwrap();
        std::fs::write(&file, b"c\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        // Shorter: refused, nothing written.
        assert!(replace_local(path, b"", b"c\n").is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"c\n");
        // Changed since it was read: not replaced.
        assert!(matches!(
            replace_local(path, b"a\nb\nc\nd\n", b"d\n"),
            Ok(false)
        ));
        assert_eq!(std::fs::read(&file).unwrap(), b"c\n");
        // As read: replaced, mode kept, no temporary left.
        assert!(matches!(
            replace_local(path, b"a\nb\nc\n", b"c\n"),
            Ok(true)
        ));
        assert_eq!(std::fs::read(&file).unwrap(), b"a\nb\nc\n");
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        // Gone: not replaced, and not made.
        std::fs::remove_file(&file).unwrap();
        assert!(matches!(replace_local(path, b"x\n", b""), Ok(false)));
        assert!(!file.exists());
        // A symlink is no regular file.
        std::os::unix::fs::symlink(dir.join("elsewhere"), &file).unwrap();
        assert!(replace_local(path, b"x\n", b"").is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn environment_text_quotes_values_and_rejects_invalid_names() {
        let valid = super::environment_text(&[
            ("OK_NAME".to_string(), "a 'quoted' val".to_string()),
            ("_EMPTY".to_string(), String::new()),
        ])
        .expect("valid names");
        assert!(valid.contains("OK_NAME="), "{valid}");
        assert!(valid.contains("_EMPTY="), "{valid}");
        assert!(super::environment_text(&[("1BAD".to_string(), "x".to_string())]).is_err());
        assert!(super::environment_text(&[("BAD=NAME".to_string(), "x".to_string())]).is_err());
    }

    #[test]
    fn write_private_sets_0600_mode_and_preserves_contents() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("write-private");
        let term = spawn_core::id::TerminalName::new("spawn-exec_1").unwrap();
        let path = super::write_private(
            &dir,
            &term,
            super::LAUNCH,
            "sh",
            "launch",
            "#!/bin/sh\necho ok\n",
        )
        .expect("write_private");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "#!/bin/sh\necho ok\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn write_new_and_read_from_files_respect_offsets_and_avoid_overwrites() {
        let dir = scratch("write-new-read");
        let file = dir.join("nested/log.jsonl");
        let path = file.to_str().unwrap();
        let ws = spawn_core::id::WorkspaceId::new(format!("w:{}", dir.display())).unwrap();
        assert!(super::write_new_file(None, path, b"abcdef").unwrap());
        assert!(!super::write_new_file(None, path, b"zzzzzz").unwrap());
        assert_eq!(
            super::read_from_files(None, &ws, path, 2).unwrap(),
            Some(b"cdef".to_vec())
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_controllers_settings_stay_out_of_terminal_hosts() {
        use std::ffi::OsStr;
        for name in [
            "SPAWN_DATABASE_URL",
            "SPAWN_SERVE_TOKEN",
            "SPAWN_HERDR_SESSION",
            "PGPASSWORD",
        ] {
            assert!(super::scrub(OsStr::new(name)), "{name}");
        }
        for name in ["PATH", "HOME", "https_proxy", "LANG"] {
            assert!(!super::scrub(OsStr::new(name)), "{name}");
        }
    }

    #[test]
    fn an_agent_planted_fifo_or_symlink_is_refused_without_blocking() {
        let dir = scratch("special");
        let fifo = dir.join("exit-fifo");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        let started = Instant::now();
        assert!(read_local(fifo.to_str().unwrap(), 0).is_err());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the open blocked"
        );

        let link = dir.join("exit-link");
        std::os::unix::fs::symlink("/etc/hostname", &link).unwrap();
        assert!(read_local(link.to_str().unwrap(), 0).is_err());

        assert!(
            read_local(dir.join("absent").to_str().unwrap(), 0)
                .unwrap()
                .is_none()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_read_returns_at_most_the_limit_from_the_offset() {
        let dir = scratch("limit");
        let file = dir.join("big");
        std::fs::write(&file, vec![b'x'; READ_LIMIT + 10]).unwrap();
        let path = file.to_str().unwrap();
        assert_eq!(read_local(path, 0).unwrap().unwrap().len(), READ_LIMIT);
        assert_eq!(
            read_local(path, READ_LIMIT as u64).unwrap().unwrap().len(),
            10
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A workspace file is read from the workspace through held
    /// directories: a parent directory the agent swapped for a symlink is
    /// refused, not followed out of the workspace.
    #[test]
    fn a_workspace_file_behind_a_symlinked_directory_is_refused() {
        let dir = scratch("workspace");
        let workspace = dir.join("ws");
        let outside = dir.join("outside");
        std::fs::create_dir_all(workspace.join(".spawn")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(workspace.join(".spawn/exit-1"), b"0\n").unwrap();
        std::fs::write(outside.join("exit-2"), b"secret").unwrap();
        std::os::unix::fs::symlink(&outside, workspace.join("link")).unwrap();
        assert_eq!(
            read_in_workspace(&workspace, ".spawn/exit-1", 0).unwrap(),
            Some(b"0\n".to_vec())
        );
        assert_eq!(
            read_in_workspace(&workspace, ".spawn/exit-3", 0).unwrap(),
            None
        );
        assert!(read_in_workspace(&workspace, "link/exit-2", 0).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Never a directory too broad to be one workspace.
    #[test]
    fn only_a_narrow_directory_is_swept() {
        let home = crate::home::Home::of_account().path;
        for broad in [
            "/",
            "/tmp",
            "/tmp/a",
            "/home/someone",
            "relative/a/b",
            "/a/../b/c",
        ] {
            assert!(
                !sweepable(std::path::Path::new(broad), home.as_deref()),
                "{broad}"
            );
        }
        if let Some(home_dir) = &home {
            assert!(!sweepable(home_dir, home.as_deref()));
            assert!(
                home_dir
                    .parent()
                    .is_none_or(|parent| !sweepable(parent, home.as_deref()))
            );
        }
        let configured_home = std::path::Path::new("/var/tmp/custom-home/user");
        assert!(!sweepable(configured_home, Some(configured_home)));
        let here = std::env::current_dir().expect("cwd");
        assert!(!sweepable(&here, home.as_deref()));
        assert!(sweepable(
            std::path::Path::new("/workspaces/agents/agent_a"),
            home.as_deref(),
        ));
    }

    /// Without pidfds and `/proc` (every target but Linux) the sweep finds
    /// nothing and signals nothing: a process beneath the workspace is left
    /// alone rather than signalled by a pid that could have been reused.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn without_pidfds_nothing_beneath_the_workspace_is_signalled() {
        let scratch = crate::Scratch::new("spawn-straggler-test");
        let workspace = scratch.join("ws");
        std::fs::create_dir_all(&workspace).expect("dirs");
        if !sweepable(&workspace, None) {
            eprintln!(
                "skipped: the scratch {} is not sweepable",
                workspace.display()
            );
            return;
        }
        let mut inside = std::process::Command::new("sleep")
            .arg("30")
            .current_dir(&workspace)
            .spawn()
            .expect("sleep");
        let ended = end_stragglers(
            &workspace.display().to_string(),
            (Duration::from_millis(50), Duration::from_millis(50)),
            None,
        );
        let alive = inside.try_wait().expect("try_wait").is_none();
        let _ = inside.kill();
        let _ = inside.wait();
        assert!(ended.is_ok(), "{ended:?}");
        assert!(stragglers_beneath(&workspace).is_empty());
        assert!(alive, "never signalled by pid");
    }

    /// A process the test starts with its working directory below a
    /// workspace, detached from any session, is found by pid and ended;
    /// one elsewhere is left alone. The sweep reads `/proc` and signals
    /// through pidfds: Linux only.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_detached_process_beneath_the_workspace_is_ended_by_pid() {
        let scratch = crate::Scratch::new("spawn-straggler-test");
        let workspace = scratch.join("ws");
        let nested = workspace.join("deep");
        std::fs::create_dir_all(&nested).expect("dirs");
        let elsewhere = scratch.join("other");
        std::fs::create_dir_all(&elsewhere).expect("dirs");
        if !sweepable(&workspace, None) {
            eprintln!(
                "skipped: the scratch {} is not sweepable",
                workspace.display()
            );
            return;
        }
        let mut inside = std::process::Command::new("sleep")
            .arg("30")
            .current_dir(&nested)
            .spawn()
            .expect("sleep");
        let mut outside = std::process::Command::new("sleep")
            .arg("30")
            .current_dir(&elsewhere)
            .spawn()
            .expect("sleep");
        assert_eq!(stragglers_beneath(&workspace), vec![inside.id()]);
        // The child is reaped here so that it is gone, not a zombie.
        let reaper = std::thread::spawn(move || inside.wait());
        end_stragglers(
            &workspace.display().to_string(),
            (Duration::from_secs(2), Duration::from_secs(2)),
            None,
        )
        .expect("ended");
        let status = reaper.join().expect("joined").expect("waited");
        assert!(!status.success(), "ended by a signal: {status:?}");
        assert!(stragglers_beneath(&workspace).is_empty());
        assert_eq!(outside.try_wait().expect("try_wait"), None, "left alone");
        let _ = outside.kill();
        let _ = outside.wait();
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline && !stragglers_beneath(&elsewhere).is_empty() {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
