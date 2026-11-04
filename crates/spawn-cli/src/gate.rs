//! Running the gate (`scripts/rsi/gate.sh`, docs/fleet-architecture.md
//! §10.2): one black-box judge for every landing, with a wall clock.
//!
//! The verdict is the gate's last line on stdout and nothing else: scenario
//! output reaches stdout only through the gate's `| ` prefix, and stderr is
//! open to whatever the candidate's processes print.

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

/// One gate verdict.
#[derive(Debug, Clone)]
pub(crate) struct Verdict {
    pub(crate) accepted: bool,
    pub(crate) reason: String,
    /// The candidate's task metric, from the verdict line only.
    pub(crate) metric: Option<String>,
    /// Everything the gate printed, stderr marked `! `, the verdict last.
    pub(crate) output: String,
}

/// Why the gate produced no verdict.
#[derive(Debug)]
pub(crate) enum GateFailure {
    /// The gate ran past its wall clock. Infrastructure, not the candidate.
    Timeout,
    /// The gate could not start or printed no verdict.
    Failed(String),
}

/// Judge `candidate` against `base` in `repo` with the scenarios in
/// `scenarios`, and the task scenario when there is one.
pub(crate) fn judge(
    gate: &Path,
    repo: &Path,
    base: &str,
    candidate: &str,
    scenarios: &Path,
    scenario: Option<&Path>,
    limit: Duration,
) -> Result<Verdict, GateFailure> {
    let mut command = Command::new("sh");
    command
        .arg(gate)
        .arg(repo)
        .arg(base)
        .arg(candidate)
        .arg(scenarios);
    if let Some(scenario) = scenario {
        command.arg(scenario);
    }
    let (stdout, stderr) = run_bounded(command, limit)?;
    let line = stdout
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .filter(|line| line.starts_with('{'))
        .ok_or_else(|| GateFailure::Failed("gate produced no verdict".to_string()))?;
    // The record keeps stderr, marked `! ` so no line of it can pass for a
    // verdict or a section header, and the verdict stays last.
    let mut output = stdout[..stdout.rfind(line).unwrap_or(stdout.len())].to_string();
    for text in stderr.lines() {
        let _ = writeln!(output, "! {text}");
    }
    let _ = writeln!(output, "{line}");
    let verdict: serde_json::Value = serde_json::from_str(line)
        .map_err(|error| GateFailure::Failed(format!("gate verdict: {error}")))?;
    let text = |name: &str| {
        verdict
            .get(name)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    Ok(Verdict {
        accepted: text("verdict").as_deref() == Some("accepted"),
        reason: text("reason").unwrap_or_else(|| "gate rejected the candidate".to_string()),
        metric: text("metric"),
        output,
    })
}

/// Run a command with a wall clock, returning its stdout and stderr.
///
/// The child writes to files, not to pipes: a pipe that fills up would block
/// the child while this loop waits for it to exit. The files live in a new
/// private directory per run. The child leads its own process group, and the
/// whole group is killed when the gate ends or times out, so nothing the
/// candidate started outlives the verdict.
///
/// ponytail: same-uid code can still reach the files through /proc; the real
/// boundary is running the gate in a container.
pub(crate) fn run_bounded(
    mut command: Command,
    limit: Duration,
) -> Result<(String, String), GateFailure> {
    use std::os::unix::fs::DirBuilderExt as _;
    use std::os::unix::process::CommandExt as _;

    let failed = |error: std::io::Error| GateFailure::Failed(error.to_string());
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let directory =
        std::env::temp_dir().join(format!("spawn-gate-{}-{nonce:x}", std::process::id()));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&directory)
        .map_err(failed)?;
    let result = (|| {
        let out_path = directory.join("stdout");
        let err_path = directory.join("stderr");
        let stdout = std::fs::File::create(&out_path).map_err(failed)?;
        let stderr = std::fs::File::create(&err_path).map_err(failed)?;
        let mut child = command
            .stdout(stdout)
            .stderr(stderr)
            .process_group(0)
            .spawn()
            .map_err(failed)?;
        let group = child.id();
        let kill_group = || {
            let _ = Command::new("kill")
                .args(["-KILL", "--", &format!("-{group}")])
                .stderr(std::process::Stdio::null())
                .status();
        };
        let deadline = Instant::now() + limit;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() >= deadline => {
                    kill_group();
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(GateFailure::Timeout);
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(200)),
                Err(error) => {
                    kill_group();
                    return Err(failed(error));
                }
            }
        }
        kill_group();
        let stdout = std::fs::read_to_string(&out_path).unwrap_or_default();
        let stderr = std::fs::read_to_string(&err_path).unwrap_or_default();
        Ok((stdout, stderr))
    })();
    let _ = std::fs::remove_dir_all(&directory);
    result
}

/// Scenario files in the controller's checkout.
pub(crate) fn count_scenarios(directory: &Path) -> Result<u32, String> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(format!("read {}: {error}", directory.display())),
    };
    let mut count = 0;
    for entry in entries {
        let path = entry
            .map_err(|error| format!("read {}: {error}", directory.display()))?
            .path();
        if path.extension().and_then(|text| text.to_str()) == Some("sh") {
            count += 1;
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::{GateFailure, run_bounded};
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn shell(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        command
    }

    #[test]
    fn a_command_past_its_deadline_is_killed() {
        let started = Instant::now();
        let killed = run_bounded(shell("exec sleep 30"), Duration::from_millis(300));
        assert!(matches!(killed, Err(GateFailure::Timeout)));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_command_inside_its_deadline_returns_its_output() {
        let (stdout, stderr) = run_bounded(
            shell("echo accepted; echo forged >&2"),
            Duration::from_secs(10),
        )
        .expect("output");
        assert!(stdout.contains("accepted"));
        assert!(!stdout.contains("forged"), "stderr stays out of stdout");
        assert!(stderr.contains("forged"));
    }

    #[test]
    fn a_timeout_kills_the_whole_group() {
        let marker = std::env::temp_dir().join(format!("spawn-group-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let script = format!("(sleep 2; touch {}) & sleep 30", marker.display());
        let killed = run_bounded(shell(&script), Duration::from_millis(300));
        assert!(matches!(killed, Err(GateFailure::Timeout)));
        std::thread::sleep(Duration::from_secs(3));
        assert!(!marker.exists(), "the background child died with the gate");
    }
}
