//! What a gate run judged and where (objective 035's `inputs` and `env`):
//! the exact base and candidate it was given, the controller commit whose
//! scenarios judged, the digest of the scenario set, the task's own
//! scenario; the host, the toolchain the run printed and the runner. A
//! verdict binds them, so two verdicts are compared only when they judged
//! the same immutable inputs (the shadow, [`super::shadow`]).
//!
//! Everything is read just before the run it describes is started, never
//! remembered from an earlier read: a controller deployed between two gates
//! of one integrator is seen by the second.

use std::path::Path;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The prefix of the gate's toolchain line (scripts/rsi/gate.sh prints it
/// during its setup, before any build).
const TOOLCHAIN_LINE: &str = "| base cache: toolchain ";

/// The names of the `inputs` fields, in the record's order.
pub(super) const FIELDS: [&str; 5] = [
    "base",
    "candidate",
    "controller",
    "scenarios_sha256",
    "scenario",
];

/// The controller of a mission's gate directory: the directory two levels
/// above it.
pub(super) fn controller_of(scenarios: &Path) -> Option<std::path::PathBuf> {
    Some(scenarios.parent()?.parent()?.to_path_buf())
}

/// The first line of `<controller>/.controller` when it is 40 hex digits;
/// None when the file is missing, unreadable or says anything else.
pub(super) fn controller_commit(controller: &Path) -> Option<String> {
    let text = std::fs::read_to_string(controller.join(".controller")).ok()?;
    let first = text.lines().next()?.trim();
    (first.len() == 40 && first.chars().all(|c| c.is_ascii_hexdigit())).then(|| first.to_string())
}

/// The scenario set's digest: the sha256 of the lines `<sha256 of the
/// file>  <path>\n` (sha256sum's format) of every regular file under `dir`
/// (symbolic links are not followed), paths relative to it, sorted
/// bytewise. None when the directory cannot be read whole.
pub(super) fn scenarios_sha256(dir: &Path) -> Option<String> {
    let mut files: Vec<(Vec<u8>, std::path::PathBuf)> = Vec::new();
    walk(dir, Path::new(""), &mut files).ok()?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut all = Sha256::new();
    for (relative, path) in files {
        let bytes = std::fs::read(&path).ok()?;
        all.update(hex(&Sha256::digest(&bytes)).as_bytes());
        all.update(b"  ");
        all.update(&relative);
        all.update(b"\n");
    }
    Some(hex(&all.finalize()))
}

fn walk(
    dir: &Path,
    relative: &Path,
    files: &mut Vec<(Vec<u8>, std::path::PathBuf)>,
) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let name = relative.join(entry.file_name());
        if kind.is_dir() {
            walk(&entry.path(), &name, files)?;
        } else if kind.is_file() {
            files.push((name.as_os_str().as_bytes().to_vec(), entry.path()));
        }
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, byte| {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// The text after `| base cache: toolchain ` on the first such line of a
/// gate's output (a stderr line marked `! ` counts too); None when the run
/// printed none (a gate that ended before its setup).
pub(super) fn toolchain_of(log: &str) -> Option<String> {
    log.lines().find_map(|line| {
        let line = line.strip_prefix("! ").unwrap_or(line);
        line.strip_prefix(TOOLCHAIN_LINE)
            .map(|rest| rest.trim_end().to_string())
    })
}

/// This host's name, as `uname -n` gives it.
pub(super) fn host_name() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

/// The `inputs` of a gate run given `base` and `candidate` with the gate
/// directory `scenarios` and the task's `scenario` (its file name), read
/// now.
pub(super) fn read(
    base: &str,
    candidate: &str,
    scenarios: &Path,
    scenario: Option<&Path>,
) -> Value {
    let controller = controller_of(scenarios).and_then(|dir| controller_commit(&dir));
    json!({
        "base": base,
        "candidate": candidate,
        "controller": controller,
        "scenarios_sha256": scenarios_sha256(scenarios),
        "scenario": scenario
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().to_string()),
    })
}

/// The `inputs` of a record no gate ran for (`stale`, a conflict found
/// before the gate, a `no_room` from the integrator's own room check): what
/// is known, `base` and the candidate when there is one, the rest null.
pub(super) fn known(base: Option<&str>, candidate: Option<&str>) -> Value {
    json!({
        "base": base,
        "candidate": candidate,
        "controller": null,
        "scenarios_sha256": null,
        "scenario": null,
    })
}

/// The `env` of a gate run on this host whose output is `log`.
pub(super) fn env_here(log: Option<&str>) -> Value {
    json!({
        "host": host_name(),
        "toolchain": log.and_then(toolchain_of),
        "runner": null,
    })
}

/// The names of the `inputs` fields whose values differ between `one` and
/// `other` (a field absent from one counts as null).
pub(super) fn unequal(one: &Value, other: &Value) -> Vec<&'static str> {
    FIELDS
        .into_iter()
        .filter(|field| {
            one.get(*field).unwrap_or(&Value::Null) != other.get(*field).unwrap_or(&Value::Null)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{controller_commit, scenarios_sha256, toolchain_of, unequal};
    use serde_json::json;
    use std::path::PathBuf;

    struct Scratch(PathBuf);
    impl Scratch {
        fn new(case: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "spawn-inputs-test-{case}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The digest as the scenario's shell computes it, with sha256sum.
    fn shell_digest(dir: &std::path::Path) -> String {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(
                "cd \"$1\" && find . -type f | sed 's|^\\./||' | LC_ALL=C sort | \
                 while IFS= read -r p; do sha256sum \"$p\"; done | sha256sum | cut -d' ' -f1",
            )
            .arg("sh")
            .arg(dir)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn the_scenario_set_digest_counts_every_file_bytewise_and_sees_a_file_added() {
        let dir = Scratch::new("digest");
        std::fs::create_dir_all(dir.0.join("sub")).unwrap();
        std::fs::write(dir.0.join("check-b.sh"), "b\n").unwrap();
        std::fs::write(dir.0.join("B.sh"), "B\n").unwrap();
        std::fs::write(dir.0.join("sub/notes"), "notes\n").unwrap();
        let first = scenarios_sha256(&dir.0).unwrap();
        assert_eq!(first, shell_digest(&dir.0));
        std::fs::write(dir.0.join("sub/more"), "more\n").unwrap();
        let second = scenarios_sha256(&dir.0).unwrap();
        assert_ne!(first, second);
        assert_eq!(second, shell_digest(&dir.0));
        assert_eq!(scenarios_sha256(&dir.0.join("missing")), None);
    }

    #[test]
    fn the_controller_marker_is_forty_hex_digits_or_nothing() {
        let dir = Scratch::new("marker");
        assert_eq!(controller_commit(&dir.0), None);
        let commit = "0123456789abcdef0123456789abcdef01234567";
        std::fs::write(dir.0.join(".controller"), format!("{commit}\nmore\n")).unwrap();
        assert_eq!(controller_commit(&dir.0).as_deref(), Some(commit));
        std::fs::write(dir.0.join(".controller"), "not a commit\n").unwrap();
        assert_eq!(controller_commit(&dir.0), None);
    }

    #[test]
    fn the_toolchain_is_the_first_such_line_or_none() {
        let log = "| == setup\n| base cache: toolchain rustc 1 cargo 1  \n| base cache: toolchain other\n";
        assert_eq!(toolchain_of(log).as_deref(), Some("rustc 1 cargo 1"));
        assert_eq!(
            toolchain_of("! | base cache: toolchain x\n").as_deref(),
            Some("x")
        );
        assert_eq!(toolchain_of("| == setup\nno verdict\n"), None);
    }

    #[test]
    fn unequal_names_the_fields_that_differ() {
        let one = json!({"base": "a", "candidate": "b", "controller": null,
                         "scenarios_sha256": "s", "scenario": null});
        assert!(unequal(&one, &one.clone()).is_empty());
        let mut other = one.clone();
        other["scenarios_sha256"] = json!("t");
        other["controller"] = json!("c");
        assert_eq!(
            unequal(&one, &other),
            vec!["controller", "scenarios_sha256"]
        );
        assert_eq!(
            unequal(&one, &json!({})),
            vec!["base", "candidate", "scenarios_sha256"]
        );
    }
}
