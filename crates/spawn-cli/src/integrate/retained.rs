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
