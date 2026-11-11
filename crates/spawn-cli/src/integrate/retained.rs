//! The gate's log of an owner judging that ended with no verdict (P26).
//!
//! A judging's directory, which holds the gate's stdout and stderr, is
//! removed when its thread ends ([`super::judging`]). A verdict keeps its
//! log on its record, but an owner judging with no verdict records no
//! outcome, so without this its log went with the directory: the host's
//! fault, a base that disagreed with itself, a scenario the host cannot
//! judge, an abort (a harness exit 2). Here it is kept as
//! `<root>/no-verdict-logs/<ms>-request-<request>-<judging>.log`: everything
//! the gate printed (stdout, then stderr marked `! `, as
//! [`crate::gate::GateFailure`] holds it), cut to its last [`LOG_BYTES`].
//! The directory keeps the newest [`KEEP`] files and removes the older ones
//! each time it writes one, so it stays at most `KEEP` × `LOG_BYTES` (about
//! 128 MB). The path goes in the journal's no-verdict line and on the
//! request's `source.push_no_verdict` record.

//!
//! Every judging's gates, whatever the outcome (lead-mac, 2026-11-11: the
//! per-scenario lines at the end of a gate's stdout are the measurement's
//! source): when a gate's private directory in a judging's directory is
//! dropped, its `stdout`, `stderr`, `verdict` and `status` are copied first
//! to `<root>/gate-logs/<judging>/<gate directory>/` (each cut to its last
//! [`LOG_BYTES`]; [`keep_gate`]). `<root>/gate-logs` keeps the newest
//! [`GATE_KEEP`] judgings (by id, which sorts by start) and at most
//! [`GATE_TOTAL_BYTES`] in all, the oldest removed first. A judging's
//! outcome records name the directory (`gate_logs`, [`super::judging`]'s
//! `named`) once a gate of it has been kept.

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// The directory under the integrator's root that holds the kept logs.
pub(crate) const DIRECTORY: &str = "no-verdict-logs";

/// How many logs the directory keeps: the newest, by name (each starts with
/// its time in ms, zero-padded).
pub(crate) const KEEP: usize = 32;

/// The most of one log kept: its end, where the gate says why it stopped.
pub(crate) const LOG_BYTES: usize = 4 * 1024 * 1024;

/// Keep `log` as `<root>/no-verdict-logs/<now ms>-request-<request>-<judging>.log`
/// (0600, in a 0700 directory, written whole: a temporary file renamed over
/// it), then remove all but the newest `keep` `.log` files there. Returns the
/// kept file's path.
pub(crate) fn keep(
    root: &Path,
    request: i64,
    judging: Option<&str>,
    log: &str,
    keep: usize,
) -> Result<PathBuf, String> {
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
    let directory = root.join(DIRECTORY);
    match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("create {}: {error}", directory.display())),
    }
    let name = format!(
        "{:013}-request-{request}-{}.log",
        super::now_ms(),
        judging.unwrap_or("none")
    );
    let path = directory.join(&name);
    let temporary = directory.join(format!(".{name}.tmp"));
    let write = || -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(tail(log, LOG_BYTES).as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, &path)
    };
    if let Err(error) = write() {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("write {}: {error}", path.display()));
    }
    prune(&directory, keep);
    Ok(path)
}

/// Remove all but the newest `keep` `.log` files in `directory` (by name),
/// and any temporary file a stopped writer left.
fn prune(directory: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut logs: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let extension = Path::new(&name)
            .extension()
            .map(|extension| extension.to_string_lossy().to_string());
        if name.starts_with('.') && extension.as_deref() == Some("tmp") {
            let old = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|at| at.elapsed().ok())
                .is_some_and(|age| age.as_secs() > 3600);
            if old {
                let _ = std::fs::remove_file(entry.path());
            }
        } else if !name.starts_with('.') && extension.as_deref() == Some("log") {
            logs.push(name);
        }
    }
    logs.sort_unstable();
    let over = logs.len().saturating_sub(keep);
    for name in &logs[..over] {
        let _ = std::fs::remove_file(directory.join(name));
    }
}

/// The directory under the integrator's root that holds every judging's
/// gate files.
pub(crate) const GATE_LOGS: &str = "gate-logs";

/// How many judgings' gate files `gate-logs` keeps: about a day and a half of
/// a busy fleet's gates.
pub(crate) const GATE_KEEP: usize = 200;

/// The most `gate-logs` holds in all; the oldest judgings go first past it.
pub(crate) const GATE_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// The files of a gate's private directory that are kept.
const GATE_FILES: [&str; 4] = ["stdout", "stderr", "verdict", "status"];

/// Copy the gate files of `gate` (a gate's private directory) to
/// `<root>/gate-logs/<judging>/<gate's name>/`, each cut to its last
/// [`LOG_BYTES`] (0600, directories 0700), then prune `gate-logs` to `keep`
/// judgings and `total` bytes. Returns the judging's directory there.
pub(crate) fn keep_gate(
    root: &Path,
    judging: &str,
    gate: &Path,
    keep: usize,
    total: u64,
) -> Result<PathBuf, String> {
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
    let logs = root.join(GATE_LOGS);
    let judging_dir = logs.join(judging);
    let name = gate
        .file_name()
        .ok_or_else(|| format!("{} names no gate", gate.display()))?;
    let into = judging_dir.join(name);
    for directory in [&logs, &judging_dir, &into] {
        match std::fs::DirBuilder::new().mode(0o700).create(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("create {}: {error}", directory.display())),
        }
    }
    for file in GATE_FILES {
        let Ok(bytes) = std::fs::read(gate.join(file)) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        let path = into.join(file);
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut out| out.write_all(tail(&text, LOG_BYTES).as_bytes()))
            .map_err(|error| format!("write {}: {error}", path.display()))?;
    }
    prune_gates(&logs, keep, total);
    Ok(judging_dir)
}

/// Remove the oldest judgings' directories in `logs` (by name) past `keep`,
/// and then past `total` bytes in all.
fn prune_gates(logs: &Path, keep: usize, total: u64) {
    let Ok(entries) = std::fs::read_dir(logs) else {
        return;
    };
    let mut judgings: Vec<(String, u64)> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            (name, size(&entry.path()))
        })
        .collect();
    judgings.sort_unstable();
    let mut held: u64 = judgings.iter().map(|(_, bytes)| bytes).sum();
    let mut left = judgings.len();
    for (name, bytes) in &judgings {
        if left <= keep && held <= total {
            break;
        }
        // The newest is never removed: it is the one just kept.
        if left == 1 {
            break;
        }
        let _ = std::fs::remove_dir_all(logs.join(name));
        held = held.saturating_sub(*bytes);
        left -= 1;
    }
}

/// The bytes of the files under `path`.
fn size(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path) else {
        return std::fs::metadata(path).map_or(0, |meta| meta.len());
    };
    entries
        .flatten()
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => size(&entry.path()),
            _ => entry.metadata().map_or(0, |meta| meta.len()),
        })
        .sum()
}

/// The last `limit` bytes of `text`, at a character boundary.
fn tail(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut start = text.len() - limit;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("spawn-retained-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("root");
        root
    }

    /// A gate directory with its files, as gate.rs leaves it.
    fn gate(parent: &Path, name: &str, stdout: &str) -> PathBuf {
        let gate = parent.join(name);
        std::fs::create_dir_all(&gate).expect("gate");
        std::fs::write(gate.join("stdout"), stdout).expect("stdout");
        std::fs::write(gate.join("stderr"), "warn\n").expect("stderr");
        std::fs::write(gate.join("verdict"), "{}\n").expect("verdict");
        std::fs::write(gate.join("env"), "SECRET=x\n").expect("env");
        gate
    }

    #[test]
    fn a_gates_files_are_kept_by_judging_and_the_env_is_not() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = root("gates");
        let first = gate(
            &root,
            "spawn-gate-1-a-0",
            "| stage gate ok 6095ms check-web.sh\n",
        );
        let second = gate(&root, "spawn-gate-1-b-1", "the second gate\n");
        let kept = keep_gate(&root, "0001aa", &first, GATE_KEEP, GATE_TOTAL_BYTES).expect("kept");
        assert_eq!(kept, root.join(GATE_LOGS).join("0001aa"));
        keep_gate(&root, "0001aa", &second, GATE_KEEP, GATE_TOTAL_BYTES).expect("kept");
        let read = |gate: &str, file: &str| std::fs::read_to_string(kept.join(gate).join(file));
        assert_eq!(
            read("spawn-gate-1-a-0", "stdout").expect("stdout"),
            "| stage gate ok 6095ms check-web.sh\n"
        );
        assert_eq!(
            read("spawn-gate-1-a-0", "stderr").expect("stderr"),
            "warn\n"
        );
        assert_eq!(
            read("spawn-gate-1-b-1", "stdout").expect("stdout"),
            "the second gate\n"
        );
        assert!(
            read("spawn-gate-1-a-0", "env").is_err(),
            "the environment file is not kept"
        );
        let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(&kept.join("spawn-gate-1-a-0").join("stdout")), 0o600);
        assert_eq!(mode(&kept), 0o700);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn gate_logs_keep_the_newest_judgings_by_count_and_by_bytes() {
        let root = root("gate-cap");
        let source = gate(&root, "spawn-gate-1-c-0", &"x".repeat(1000));
        for judging in ["0001", "0002", "0003", "0004", "0005"] {
            keep_gate(&root, judging, &source, 3, u64::MAX).expect("kept");
        }
        let left = |root: &Path| {
            let mut left: Vec<String> = std::fs::read_dir(root.join(GATE_LOGS))
                .expect("logs")
                .map(|entry| {
                    entry
                        .expect("entry")
                        .file_name()
                        .to_string_lossy()
                        .to_string()
                })
                .collect();
            left.sort();
            left
        };
        assert_eq!(
            left(&root),
            ["0003", "0004", "0005"],
            "the newest 3 by count"
        );
        // Each judging holds about 1 kB: a 2.5 kB total keeps the newest two.
        keep_gate(&root, "0006", &source, 10, 2500).expect("kept");
        assert_eq!(left(&root), ["0005", "0006"], "the newest within the bytes");
        // The newest is kept even alone over the total.
        keep_gate(&root, "0007", &source, 10, 10).expect("kept");
        assert_eq!(left(&root), ["0007"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_kept_log_is_readable_whole_and_private() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = root("whole");
        let path =
            keep(&root, 7, Some("abc"), "| == base\n! a harness line\n", KEEP).expect("kept");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "| == base\n! a harness line\n"
        );
        let name = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .to_string();
        assert!(name.ends_with("-request-7-abc.log"), "{name}");
        let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&root.join(DIRECTORY)), 0o700);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_directory_keeps_only_the_newest_and_each_log_its_end() {
        let root = root("cap");
        let mut kept = Vec::new();
        for request in 0..7 {
            kept.push(keep(&root, request, None, &format!("log {request}\n"), 3).expect("kept"));
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let mut left: Vec<_> = std::fs::read_dir(root.join(DIRECTORY))
            .expect("dir")
            .map(|entry| entry.expect("entry").path())
            .collect();
        left.sort();
        assert_eq!(left, kept[4..].to_vec(), "the newest 3, nothing else");
        // One byte over, the cut falls inside the two-byte é: it moves past it.
        let long = format!("é{}the end\n", "b".repeat(LOG_BYTES - 9));
        assert_eq!(long.len(), LOG_BYTES + 1);
        let path = keep(&root, 9, None, &long, 3).expect("kept");
        let text = std::fs::read_to_string(&path).expect("read, as UTF-8");
        assert_eq!(text.len(), LOG_BYTES - 1, "cut at the character boundary");
        assert!(text.ends_with("the end\n"), "its end kept");
        assert_eq!(
            std::fs::read_dir(root.join(DIRECTORY))
                .expect("dir")
                .count(),
            3
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
