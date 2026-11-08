//! `run`, the command an engine's terminal starts.
//!
//! The pane holds no database login: the runner writes what the Bridge
//! cannot see for itself, the exit status, to a file the Bridge reads.
//! Claude Code's own transcript and screen tell the Bridge the rest.
//!
//! A program the OS killed has no exit status. The runner records the
//! signal that killed it beside the status (`<SPAWN_EXIT_FILE>.signal`,
//! `9` for a SIGKILL), leaving the status file empty as before: the
//! Bridge's `execution.ended` then names the signal, and a killed engine
//! is no longer indistinguishable from one that exited 0.

use std::process::Command;

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

use crate::config::env_value;

/// `spawn run -- <program> [args]`: start the engine, write its exit status
/// to `SPAWN_EXIT_FILE` (and the signal that killed it to
/// `SPAWN_EXIT_FILE.signal`), and forward its exit code. The engine's
/// output goes to the pane untouched.
pub(crate) fn run(rest: &[String]) -> Result<i32, String> {
    run_to(rest, env_value("SPAWN_EXIT_FILE").as_deref())
}

/// [`run`], with the exit file named rather than read from the environment
/// (the test of a signal death needs no process-wide variable).
fn run_to(rest: &[String], exit_file: Option<&str>) -> Result<i32, String> {
    let words: &[String] = match rest.split_first() {
        Some((first, tail)) if first == "--" => tail,
        Some(_) => rest,
        None => return Err("run needs a program after --".to_string()),
    };
    let (program, arguments) = words
        .split_first()
        .ok_or_else(|| "run needs a program after --".to_string())?;
    let mut child = Command::new(program)
        .args(arguments)
        .spawn()
        .map_err(|error| format!("start {program}: {error}"))?;
    let status = child.wait().map_err(|error| error.to_string())?;
    let code = status.code();
    if let Some(path) = exit_file {
        // Written whole, then renamed, so the Bridge never reads half a code.
        let text = code.map_or_else(String::new, |code| code.to_string());
        let partial = format!("{path}.partial");
        if let Some(parent) = std::path::Path::new(&path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&partial, text).is_ok() {
            let _ = std::fs::rename(&partial, path);
        }
        // The signal comes from the same wait, and only when there is one:
        // an exit with a code leaves no signal file, and a stale one from
        // an earlier run of the same path is removed first.
        let signal_path = format!("{path}.signal");
        match status.signal() {
            Some(signal) => {
                let partial = format!("{signal_path}.partial");
                if std::fs::write(&partial, signal.to_string()).is_ok() {
                    let _ = std::fs::rename(&partial, &signal_path);
                }
            }
            None => {
                let _ = std::fs::remove_file(&signal_path);
            }
        }
    }
    Ok(code.unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::run_to;

    fn words(script: &str) -> Vec<String> {
        vec![
            "--".to_string(),
            "sh".to_string(),
            "-c".to_string(),
            script.to_string(),
        ]
    }

    #[test]
    fn the_runner_writes_the_signal_of_a_killed_engine_beside_the_status() {
        let dir = std::env::temp_dir().join(format!("spawn-exit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let exit = dir.join("exit-exec_1");
        let exit = exit.display().to_string();
        // A program the OS kills: the status file stays empty, and the
        // signal that killed it is beside it (the Bridge records it).
        let code = run_to(&words("kill -9 $$"), Some(&exit)).expect("run");
        assert_eq!(code, 1, "no exit code: run forwards 1");
        assert_eq!(std::fs::read_to_string(&exit).expect("status"), "");
        assert_eq!(
            std::fs::read_to_string(format!("{exit}.signal")).expect("signal"),
            "9"
        );
        // An exit with a code leaves no signal file, and one left by an
        // earlier run of the same path goes.
        let code = run_to(&words("exit 7"), Some(&exit)).expect("run");
        assert_eq!(code, 7);
        assert_eq!(std::fs::read_to_string(&exit).expect("status"), "7");
        assert!(!std::path::Path::new(&format!("{exit}.signal")).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
