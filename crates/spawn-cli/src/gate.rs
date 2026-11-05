//! Running the gate (`scripts/rsi/gate.sh`, docs/fleet-architecture.md
//! §10.2): one black-box judge for every landing, with a wall clock.
//!
//! The verdict is the file the gate writes at `SPAWN_GATE_VERDICT`, in a
//! private directory this process creates and the gate's sandbox hides, and
//! it counts only when the gate exits 0. Stdout is the record, not the
//! verdict: candidate code once wrote an accepted line to the gate's stdout
//! through /proc and killed the gate. A gate that exits non-zero, or dies
//! of a signal, is an abort whatever it printed, with one exception: exit
//! [`NO_ROOM_EXIT`] *and* a no-room fact in the verdict file
//! (`{"no_room":true,"free_kb":n,"min_gb":n,"tmpdir":"…"}`) means TMPDIR had
//! no room to judge ([`GateFailure::NoRoom`]). The gate writes that file
//! before any candidate code runs (scripts/disk.sh `gate_room`). Either half
//! alone is an abort.
//!
//! The gate runs with a cleared environment: [`GATE_ENV`] and nothing else.
//! The integrator's own database URL never reaches candidate code.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// The integrator's variables the gate receives (scripts/rsi/gate.sh lists
/// what it reads). Everything else, the owner's SPAWN_DATABASE_URL first,
/// stays out.
pub(crate) const GATE_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "LANG",
    "TMPDIR",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "SPAWN_TEST_DATABASE_URL",
    // The free space the gate wants in TMPDIR before it starts
    // (scripts/disk.sh); read outside the sandbox only.
    "SPAWN_GATE_MIN_FREE_GB",
];

/// The gate's exit status when TMPDIR has no room to judge now
/// (scripts/rsi/gate.sh). It counts only with the no-room fact in the
/// verdict file.
pub(crate) const NO_ROOM_EXIT: i32 = 5;

/// One gate verdict.
#[derive(Debug, Clone)]
pub(crate) struct Verdict {
    pub(crate) accepted: bool,
    pub(crate) reason: String,
    /// The candidate's task metric, from the verdict only.
    pub(crate) metric: Option<String>,
    /// Everything the gate printed, stderr marked `! `, the verdict last.
    pub(crate) output: String,
}

/// Why the gate produced no verdict.
#[derive(Debug)]
pub(crate) enum GateFailure {
    /// The gate ran past its wall clock. Infrastructure, not the candidate.
    Timeout,
    /// The gate could not start.
    Failed(String),
    /// The gate ran but gave no verdict to believe: a non-zero or signalled
    /// exit, or no verdict file. `log` is what it printed.
    Aborted { reason: String, log: String },
    /// TMPDIR had less than SPAWN_GATE_MIN_FREE_GB free, so the gate did not
    /// start. A fact about the host, not the work. It names what the gate
    /// saw: free KiB, the floor in GiB, and the directory.
    NoRoom {
        free_kb: u64,
        min_gb: u64,
        tmpdir: String,
        log: String,
    },
}

/// The gate's environment, read through `lookup`: the [`GATE_ENV`]
/// variables that are set. A SPAWN_TEST_DATABASE_URL whose text names the
/// owner's cluster (same host and port, or same socket directory) is
/// withheld here, whatever database it names: the test login is a
/// superuser, and a superuser on the owner's cluster reaches the owner's
/// database. Text is only the cheap first pass; [`withhold_owners_cluster`]
/// decides by the servers' identity. (Not an error: the e2e runs an
/// integrator whose own database is the test database.)
pub(crate) fn gate_env(lookup: impl Fn(&str) -> Option<String>) -> Vec<(String, String)> {
    let owner = lookup("SPAWN_DATABASE_URL");
    GATE_ENV
        .iter()
        .filter_map(|name| {
            lookup(name)
                .filter(|value| !value.is_empty())
                .map(|value| ((*name).to_string(), value))
        })
        .filter(|(name, value)| {
            name != "SPAWN_TEST_DATABASE_URL"
                || owner
                    .as_deref()
                    .is_none_or(|owner| cluster_of(owner) != cluster_of(value))
        })
        .collect()
}

/// The decisive pass over [`gate_env`]'s result: SPAWN_TEST_DATABASE_URL
/// stays only when the owner's server and the test server both answer with
/// their identity (`identity`, `spawn_postgres::cluster_identity` in the
/// integrator) and the two differ. One server spelled two ways (its socket,
/// a hostname, 127.0.0.2, a LAN address) has one identity. Fails closed: no
/// owner URL, or a connection or query that fails, withholds the test URL
/// too. Returns the environment and, when the URL was withheld, why.
pub(crate) fn withhold_owners_cluster(
    env: &[(String, String)],
    owner: Option<&str>,
    identity: impl Fn(&str) -> Result<String, String>,
) -> (Vec<(String, String)>, Option<String>) {
    let Some((_, test)) = env
        .iter()
        .find(|(name, _)| name == "SPAWN_TEST_DATABASE_URL")
    else {
        return (env.to_vec(), None);
    };
    let reason = match owner {
        None => Some(
            "SPAWN_DATABASE_URL is unset, so the test server cannot be told apart from the owner's"
                .to_string(),
        ),
        Some(owner) => match (identity(owner), identity(test)) {
            (Ok(owner), Ok(test)) if owner != test => None,
            (Ok(_), Ok(_)) => {
                Some("SPAWN_TEST_DATABASE_URL reaches the owner's server".to_string())
            }
            (Err(error), _) => Some(format!(
                "the owner's server has no readable identity: {error}"
            )),
            (_, Err(error)) => Some(format!("the test server has no readable identity: {error}")),
        },
    };
    match reason {
        None => (env.to_vec(), None),
        Some(reason) => (
            env.iter()
                .filter(|(name, _)| name != "SPAWN_TEST_DATABASE_URL")
                .cloned()
                .collect(),
            Some(format!(
                "{reason}; the gate does not get SPAWN_TEST_DATABASE_URL"
            )),
        ),
    }
}

/// The cluster a postgres URL names: its host, with a percent-encoded
/// socket directory decoded and `localhost`/`::1` read as 127.0.0.1, and
/// its port (5432 when absent).
fn cluster_of(url: &str) -> (String, String) {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?']).next().unwrap_or(rest);
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port))
            if !port.is_empty()
                && port.bytes().all(|byte| byte.is_ascii_digit())
                && (!host.starts_with('[') || host.ends_with(']')) =>
        {
            (host, port)
        }
        _ => (authority, "5432"),
    };
    let host = host.replace("%2F", "/").replace("%2f", "/");
    let host = match host.trim_start_matches('[').trim_end_matches(']') {
        "localhost" | "::1" | "127.0.0.1" => "127.0.0.1".to_string(),
        _ => host.trim_end_matches('/').to_string(),
    };
    (host, port.to_string())
}

/// Judge `candidate` against `base` in `repo` with the scenarios in
/// `scenarios`, and the task scenario when there is one. `env` is the
/// gate's whole environment ([`gate_env`]).
#[allow(clippy::too_many_arguments)]
pub(crate) fn judge(
    gate: &Path,
    repo: &Path,
    base: &str,
    candidate: &str,
    scenarios: &Path,
    scenario: Option<&Path>,
    env: &[(String, String)],
    limit: Duration,
) -> Result<Verdict, GateFailure> {
    let directory = PrivateDirectory::create().map_err(|error| failed(&error))?;
    let verdict_path = directory.path().join("verdict");
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
    command
        .env_clear()
        .envs(env.iter().map(|(name, value)| (name, value)))
        .env("SPAWN_GATE_VERDICT", &verdict_path);
    let run = run_bounded(command, limit, directory.path())?;
    let verdict = std::fs::read_to_string(&verdict_path).ok();
    decide(&run, verdict.as_deref())
}

/// The verdict of a finished gate run: the verdict file's, only after a
/// clean exit.
fn decide(run: &Run, verdict: Option<&str>) -> Result<Verdict, GateFailure> {
    // The record: stdout, then stderr marked `! ` so no line of it can pass
    // for a verdict or a section header. An abort keeps all of it.
    let record = |stdout: &str| {
        let mut output = stdout.to_string();
        for text in run.stderr.lines() {
            let _ = writeln!(output, "! {text}");
        }
        output
    };
    let aborted = |reason: String| GateFailure::Aborted {
        reason,
        log: record(&run.stdout),
    };
    if run.status.code() == Some(NO_ROOM_EXIT)
        && let Some((free_kb, min_gb, tmpdir)) = verdict.and_then(no_room)
    {
        return Err(GateFailure::NoRoom {
            free_kb,
            min_gb,
            tmpdir,
            log: record(&run.stdout),
        });
    }
    if !run.status.success() {
        return Err(aborted(format!("the gate ended with {}", run.status)));
    }
    let Some(line) = verdict
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.contains('\n'))
    else {
        return Err(aborted("the gate wrote no verdict".to_string()));
    };
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|error| aborted(format!("gate verdict: {error}")))?;
    let text = |name: &str| value.get(name).and_then(|v| v.as_str()).map(str::to_string);
    let accepted = match text("verdict").as_deref() {
        Some("accepted") => true,
        Some("rejected") => false,
        _ => return Err(aborted(format!("gate verdict: {line}"))),
    };
    // A believed verdict is recorded last, and once: stdout's copy goes.
    let stdout = run
        .stdout
        .trim_end()
        .strip_suffix(line)
        .filter(|head| head.is_empty() || head.ends_with('\n'))
        .unwrap_or(&run.stdout);
    let mut output = record(stdout);
    let _ = writeln!(output, "{line}");
    Ok(Verdict {
        accepted,
        reason: text("reason").unwrap_or_else(|| "gate rejected the candidate".to_string()),
        metric: text("metric"),
        output,
    })
}

/// The no-room fact of a verdict file, `(free_kb, min_gb, tmpdir)`: one
/// JSON line with `no_room` true, whole numbers and a directory.
fn no_room(file: &str) -> Option<(u64, u64, String)> {
    let line = file.trim();
    if line.contains('\n') {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("no_room")?.as_bool() != Some(true) || value.get("verdict").is_some() {
        return None;
    }
    let free_kb = value.get("free_kb")?.as_u64()?;
    let min_gb = value.get("min_gb")?.as_u64()?;
    let tmpdir = value.get("tmpdir")?.as_str()?;
    (!tmpdir.is_empty()).then(|| (free_kb, min_gb, tmpdir.to_string()))
}

fn failed(error: &std::io::Error) -> GateFailure {
    GateFailure::Failed(error.to_string())
}

/// A new directory, mode 0700, under the temporary directory; removed on
/// drop. The gate's stdout, stderr and verdict file live in it.
struct PrivateDirectory(PathBuf);

impl PrivateDirectory {
    fn create() -> std::io::Result<Self> {
        use std::os::unix::fs::DirBuilderExt as _;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let path =
            std::env::temp_dir().join(format!("spawn-gate-{}-{nonce:x}", std::process::id()));
        std::fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for PrivateDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A finished command.
#[derive(Debug)]
pub(crate) struct Run {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

/// Run a command with a wall clock and no stdin, its stdout and stderr in
/// files in `directory`.
///
/// The child writes to files, not to pipes: a pipe that fills up would block
/// the child while this loop waits for it to exit. The child leads its own
/// process group, and the whole group is killed when the gate ends or times
/// out, so nothing the candidate started outlives the verdict (the gate's
/// sandbox also dies with its parent).
pub(crate) fn run_bounded(
    mut command: Command,
    limit: Duration,
    directory: &Path,
) -> Result<Run, GateFailure> {
    use std::os::unix::process::CommandExt as _;

    let out_path = directory.join("stdout");
    let err_path = directory.join("stderr");
    let stdout = std::fs::File::create(&out_path).map_err(|error| failed(&error))?;
    let stderr = std::fs::File::create(&err_path).map_err(|error| failed(&error))?;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .process_group(0)
        .spawn()
        .map_err(|error| failed(&error))?;
    let group = child.id();
    let kill_group = || {
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{group}")])
            .stderr(Stdio::null())
            .status();
    };
    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                kill_group();
                let _ = child.kill();
                let _ = child.wait();
                return Err(GateFailure::Timeout);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(error) => {
                kill_group();
                return Err(failed(&error));
            }
        }
    };
    kill_group();
    Ok(Run {
        status,
        stdout: std::fs::read_to_string(&out_path).unwrap_or_default(),
        stderr: std::fs::read_to_string(&err_path).unwrap_or_default(),
    })
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
    use super::{
        GateFailure, PrivateDirectory, gate_env, judge, run_bounded, withhold_owners_cluster,
    };
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn shell(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        command
    }

    fn bounded(script: &str, limit: Duration) -> Result<super::Run, GateFailure> {
        let directory = PrivateDirectory::create().expect("directory");
        run_bounded(shell(script), limit, directory.path())
    }

    #[test]
    fn a_command_past_its_deadline_is_killed() {
        let started = Instant::now();
        let killed = bounded("exec sleep 30", Duration::from_millis(300));
        assert!(matches!(killed, Err(GateFailure::Timeout)));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_command_inside_its_deadline_returns_its_output() {
        let run = bounded(
            "echo accepted; echo forged >&2; exit 3",
            Duration::from_secs(10),
        )
        .expect("output");
        assert!(run.stdout.contains("accepted"));
        assert!(!run.stdout.contains("forged"), "stderr stays out of stdout");
        assert!(run.stderr.contains("forged"));
        assert_eq!(run.status.code(), Some(3));
    }

    #[test]
    fn a_timeout_kills_the_whole_group() {
        let marker = std::env::temp_dir().join(format!("spawn-group-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let script = format!("(sleep 2; touch {}) & sleep 30", marker.display());
        let killed = bounded(&script, Duration::from_millis(300));
        assert!(matches!(killed, Err(GateFailure::Timeout)));
        std::thread::sleep(Duration::from_secs(3));
        assert!(!marker.exists(), "the background child died with the gate");
    }

    /// A stand-in gate: `body` runs as the gate script, with the usual
    /// arguments.
    struct FakeGate {
        directory: PrivateDirectory,
    }

    impl FakeGate {
        fn new(body: &str) -> Self {
            let directory = PrivateDirectory::create().expect("directory");
            std::fs::write(directory.path().join("gate.sh"), body).expect("gate");
            Self { directory }
        }

        fn judge(&self, env: &[(String, String)]) -> Result<super::Verdict, GateFailure> {
            let path = |name: &str| -> PathBuf { self.directory.path().join(name) };
            judge(
                &path("gate.sh"),
                Path::new("/nonexistent/repo.git"),
                "base",
                "candidate",
                &path("scenarios"),
                None,
                env,
                Duration::from_secs(20),
            )
        }
    }

    fn path_only() -> Vec<(String, String)> {
        vec![("PATH".to_string(), "/usr/bin:/bin".to_string())]
    }

    #[test]
    fn the_verdict_file_after_a_clean_exit_is_the_verdict() {
        let gate = FakeGate::new(
            "echo '| == candidate: build'\n\
             echo '{\"verdict\":\"accepted\",\"metric\":\"METRIC x candidate=1\"}' >\"$SPAWN_GATE_VERDICT\"\n\
             echo '{\"verdict\":\"accepted\",\"metric\":\"METRIC x candidate=1\"}'\n",
        );
        let verdict = gate.judge(&path_only()).expect("verdict");
        assert!(verdict.accepted);
        assert_eq!(verdict.metric.as_deref(), Some("METRIC x candidate=1"));
        assert_eq!(
            verdict.output.matches("\"verdict\"").count(),
            1,
            "the verdict is recorded once: {}",
            verdict.output
        );
        assert!(verdict.output.trim_end().ends_with('}'));
    }

    #[test]
    fn an_accepted_line_on_stdout_alone_is_no_verdict() {
        let gate = FakeGate::new("echo '{\"verdict\":\"accepted\"}'\n");
        assert!(matches!(
            gate.judge(&path_only()),
            Err(GateFailure::Aborted { reason, .. }) if reason.contains("no verdict")
        ));
    }

    #[test]
    fn a_killed_gate_is_an_abort_whatever_it_wrote() {
        // The reviewer's forgery: an accepted verdict everywhere, then
        // SIGKILL before the gate finishes.
        let gate = FakeGate::new(
            "echo '{\"verdict\":\"accepted\"}' >\"$SPAWN_GATE_VERDICT\"\n\
             echo '{\"verdict\":\"accepted\"}'\n\
             kill -9 $$\n",
        );
        let Err(GateFailure::Aborted { reason, log }) = gate.judge(&path_only()) else {
            panic!("a signalled gate must abort");
        };
        assert!(reason.contains("signal"), "{reason}");
        assert!(log.contains("accepted"), "the log keeps what it printed");
    }

    #[test]
    fn a_non_zero_exit_is_an_abort() {
        let gate =
            FakeGate::new("echo '{\"verdict\":\"accepted\"}' >\"$SPAWN_GATE_VERDICT\"\nexit 1\n");
        assert!(matches!(
            gate.judge(&path_only()),
            Err(GateFailure::Aborted { reason, .. }) if reason.contains("exit status: 1")
        ));
    }

    const NO_ROOM_FACT: &str =
        r#"{"no_room":true,"free_kb":2097152,"min_gb":10,"tmpdir":"/data/tmp"}"#;

    #[test]
    fn exit_5_with_the_no_room_fact_is_no_room() {
        let gate = FakeGate::new(&format!(
            "echo 'gate: 2.0 GiB free' >&2\necho '{NO_ROOM_FACT}' >\"$SPAWN_GATE_VERDICT\"\nexit 5\n"
        ));
        let Err(GateFailure::NoRoom {
            free_kb,
            min_gb,
            tmpdir,
            log,
        }) = gate.judge(&path_only())
        else {
            panic!("no room is its own outcome");
        };
        assert_eq!(
            (free_kb, min_gb, tmpdir.as_str()),
            (2_097_152, 10, "/data/tmp")
        );
        assert!(log.contains("! gate: 2.0 GiB free"), "{log}");
    }

    #[test]
    fn exit_5_without_the_fact_is_an_abort() {
        // A bare status, a status with a verdict instead, or a fact that
        // names no directory: nothing recorded says "no room".
        for body in [
            "exit 5\n".to_string(),
            "echo '{\"verdict\":\"accepted\"}' >\"$SPAWN_GATE_VERDICT\"\nexit 5\n".to_string(),
            "echo '{\"no_room\":true,\"free_kb\":1,\"min_gb\":10}' >\"$SPAWN_GATE_VERDICT\"\nexit 5\n"
                .to_string(),
            "echo '{\"no_room\":true,\"free_kb\":\"lots\",\"min_gb\":10,\"tmpdir\":\"/t\"}' \
             >\"$SPAWN_GATE_VERDICT\"\nexit 5\n"
                .to_string(),
        ] {
            let gate = FakeGate::new(&body);
            assert!(
                matches!(
                    gate.judge(&path_only()),
                    Err(GateFailure::Aborted { ref reason, .. }) if reason.contains("exit status: 5")
                ),
                "{body}"
            );
        }
    }

    #[test]
    fn the_no_room_fact_with_another_status_is_an_abort() {
        // Exit 3 is every other refusal to start (a missing bind directory,
        // a bad floor): it counts, whatever the file says.
        for status in [0, 3] {
            let gate = FakeGate::new(&format!(
                "echo '{NO_ROOM_FACT}' >\"$SPAWN_GATE_VERDICT\"\nexit {status}\n"
            ));
            assert!(
                matches!(gate.judge(&path_only()), Err(GateFailure::Aborted { .. })),
                "exit {status}"
            );
        }
    }

    #[test]
    fn the_file_outranks_stdout() {
        let gate = FakeGate::new(
            "echo '{\"verdict\":\"rejected\",\"reason\":\"a \\\"quoted\\\" reason\"}' >\"$SPAWN_GATE_VERDICT\"\n\
             echo '{\"verdict\":\"accepted\"}'\n",
        );
        let verdict = gate.judge(&path_only()).expect("verdict");
        assert!(!verdict.accepted);
        assert_eq!(verdict.reason, "a \"quoted\" reason");
    }

    #[test]
    fn the_gate_sees_only_the_environment_it_is_given() {
        // Cargo sets CARGO_PKG_NAME for this test process; the gate must not
        // inherit it, nor anything else.
        assert!(std::env::var_os("CARGO_PKG_NAME").is_some());
        let gate = FakeGate::new(
            "if [ -n \"${CARGO_PKG_NAME-}\" ] || [ -n \"${SPAWN_DATABASE_URL-}\" ] \
                 || [ \"$SPAWN_TEST_DATABASE_URL\" != postgres://t@127.0.0.1:15434/spawn_test ]; then\n\
                 echo '{\"verdict\":\"rejected\",\"reason\":\"leak\"}' >\"$SPAWN_GATE_VERDICT\"\n\
             else\n\
                 echo '{\"verdict\":\"accepted\"}' >\"$SPAWN_GATE_VERDICT\"\n\
             fi\n",
        );
        let lookup = |name: &str| match name {
            "PATH" => Some("/usr/bin:/bin".to_string()),
            "SPAWN_TEST_DATABASE_URL" => {
                Some("postgres://t@127.0.0.1:15434/spawn_test".to_string())
            }
            "SPAWN_DATABASE_URL" => Some("postgres://o@127.0.0.1:15433/spawn".to_string()),
            _ => None,
        };
        let env = gate_env(lookup);
        let verdict = gate.judge(&env).expect("verdict");
        assert!(verdict.accepted, "{}", verdict.reason);
    }

    #[test]
    fn the_allowlist_passes_what_gate_sh_reads_and_nothing_else() {
        let lookup = |name: &str| match name {
            "PATH" | "HOME" | "LANG" | "TMPDIR" | "CARGO_HOME" | "RUSTUP_HOME" => {
                Some(format!("{name}-value"))
            }
            "SPAWN_TEST_DATABASE_URL" => Some("postgres://t:p@127.0.0.1:15434/spawn_test".into()),
            "SPAWN_GATE_MIN_FREE_GB" => Some("10".into()),
            "SPAWN_DATABASE_URL" => Some("postgres://o:p@127.0.0.1:15433/spawn".into()),
            "SPAWN_JUDGE_URL" | "http_proxy" | "SSH_AUTH_SOCK" => Some("secret".into()),
            _ => None,
        };
        let env = gate_env(lookup);
        let names: Vec<&str> = env.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            [
                "PATH",
                "HOME",
                "LANG",
                "TMPDIR",
                "CARGO_HOME",
                "RUSTUP_HOME",
                "SPAWN_TEST_DATABASE_URL",
                "SPAWN_GATE_MIN_FREE_GB"
            ]
        );
        let unset = |name: &str| (name == "PATH").then(|| "/usr/bin".to_string());
        assert_eq!(gate_env(unset).len(), 1, "unset names are left out");
    }

    #[test]
    fn a_test_database_that_is_the_owners_is_withheld() {
        let lookup = |name: &str| match name {
            "SPAWN_TEST_DATABASE_URL" => Some("postgres://other:pw@127.0.0.1:15433/spawn".into()),
            "SPAWN_DATABASE_URL" => {
                Some("postgres://owner:pw@127.0.0.1:15433/spawn?sslmode=disable".into())
            }
            _ => None,
        };
        let env = gate_env(lookup);
        assert!(
            env.iter()
                .all(|(name, _)| name != "SPAWN_TEST_DATABASE_URL")
        );
    }

    /// Whether the test URL reaches the gate, next to this owner URL.
    fn passed(owner: &str, test: &str) -> bool {
        let lookup = |name: &str| match name {
            "SPAWN_TEST_DATABASE_URL" => Some(test.to_string()),
            "SPAWN_DATABASE_URL" => Some(owner.to_string()),
            _ => None,
        };
        gate_env(lookup)
            .iter()
            .any(|(name, _)| name == "SPAWN_TEST_DATABASE_URL")
    }

    #[test]
    fn a_test_database_on_the_owners_cluster_is_withheld_whatever_its_name() {
        let owner = "postgres://owner:pw@127.0.0.1:15433/spawn";
        // The reviewer's case: another database on the same cluster.
        assert!(!passed(
            owner,
            "postgres://postgres:pw@127.0.0.1:15433/spawn_test"
        ));
        assert!(!passed(
            owner,
            "postgres://postgres:pw@localhost:15433/other"
        ));
        assert!(!passed(
            "postgres://owner:pw@[::1]:15433/spawn",
            "postgres://t:pw@127.0.0.1:15433/x"
        ));
        assert!(passed(
            owner,
            "postgres://postgres:pw@127.0.0.1:15434/spawn_test"
        ));
        assert!(passed(
            owner,
            "postgres://postgres:pw@%2Fhome%2Fu%2Fsock/spawn_test"
        ));
        // Default ports compare equal.
        assert!(!passed(
            "postgres://o:pw@db.local/spawn",
            "postgres://t:pw@db.local:5432/spawn_test"
        ));
    }

    #[test]
    fn a_test_database_on_the_owners_socket_is_withheld() {
        let owner = "postgres://owner:pw@%2Fhome%2Fu%2Fsock/spawn";
        assert!(!passed(
            owner,
            "postgres://postgres:pw@%2Fhome%2Fu%2Fsock/spawn_test"
        ));
        assert!(!passed(
            owner,
            "postgres://postgres:pw@%2fhome%2fu%2fsock%2F/x"
        ));
        assert!(passed(
            owner,
            "postgres://postgres:pw@%2Fhome%2Fu%2Fother/spawn_test"
        ));
    }

    fn with_test_url(test: &str) -> Vec<(String, String)> {
        vec![
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("SPAWN_TEST_DATABASE_URL".to_string(), test.to_string()),
        ]
    }

    /// Identities by URL, as two servers would answer: the live server
    /// under four spellings, the test server, and one that is down.
    fn servers(url: &str) -> Result<String, String> {
        match url {
            "postgres://o:pw@127.0.0.1:15433/spawn"
            | "postgres://t:pw@%2Fvar%2Frun%2Flive/spawn_test"
            | "postgres://t:pw@127.0.0.2:15433/spawn_test"
            | "postgres://t:pw@myhost:15433/spawn_test" => Ok("7703".to_string()),
            "postgres://t:pw@%2Fhome%2Fu%2Ftest-sock/spawn_test" => Ok("7704".to_string()),
            _ => Err("connection refused".to_string()),
        }
    }

    fn kept(owner: Option<&str>, test: &str) -> (bool, Option<String>) {
        let (env, warning) = withhold_owners_cluster(&with_test_url(test), owner, servers);
        assert!(env.iter().any(|(name, _)| name == "PATH"), "the rest stays");
        (
            env.iter()
                .any(|(name, _)| name == "SPAWN_TEST_DATABASE_URL"),
            warning,
        )
    }

    #[test]
    fn the_test_url_stays_only_for_another_server() {
        let owner = Some("postgres://o:pw@127.0.0.1:15433/spawn");
        assert_eq!(
            kept(owner, "postgres://t:pw@%2Fhome%2Fu%2Ftest-sock/spawn_test"),
            (true, None)
        );
        // The live server over its socket, 127.0.0.2, a hostname: one
        // identity, so withheld, though the text names another cluster.
        for spelling in [
            "postgres://t:pw@%2Fvar%2Frun%2Flive/spawn_test",
            "postgres://t:pw@127.0.0.2:15433/spawn_test",
            "postgres://t:pw@myhost:15433/spawn_test",
        ] {
            let (passed, warning) = kept(owner, spelling);
            assert!(!passed, "{spelling} reaches the owner's server");
            assert!(warning.is_some_and(|w| w.contains("owner's server")));
        }
    }

    #[test]
    fn an_unreadable_identity_withholds_the_test_url() {
        let owner = Some("postgres://o:pw@127.0.0.1:15433/spawn");
        let (passed, warning) = kept(owner, "postgres://t:pw@%2Fdown/spawn_test");
        assert!(!passed);
        assert!(warning.is_some_and(|w| w.contains("test server")));
        let (passed, warning) = kept(
            Some("postgres://o:pw@%2Fdown/spawn"),
            "postgres://t:pw@%2Fhome%2Fu%2Ftest-sock/spawn_test",
        );
        assert!(!passed);
        assert!(warning.is_some_and(|w| w.contains("owner's server")));
        let (passed, _) = kept(None, "postgres://t:pw@%2Fhome%2Fu%2Ftest-sock/spawn_test");
        assert!(!passed, "no owner URL to compare with");
    }

    #[test]
    fn no_test_url_asks_no_server() {
        let env = vec![("PATH".to_string(), "/usr/bin".to_string())];
        let (kept, warning) = withhold_owners_cluster(&env, None, |_| panic!("no connection"));
        assert_eq!(kept, env);
        assert!(warning.is_none());
    }
}
