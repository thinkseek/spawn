//! `run`, the command an engine's terminal starts.
//!
//! The pane holds no database login: the runner writes what the Bridge
//! cannot see for itself, the exit status, to a file the Bridge reads.
//! Claude Code's own transcript and screen tell the Bridge the rest.

use std::process::Command;

use crate::config::env_value;

/// `spawn run -- <program> [args]`: start the engine, write its exit status
/// to `SPAWN_EXIT_FILE`, and forward its exit code. The engine's output
/// goes to the pane untouched.
pub(crate) fn run(rest: &[String]) -> Result<i32, String> {
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
    if let Some(path) = env_value("SPAWN_EXIT_FILE") {
        // Written whole, then renamed, so the Bridge never reads half a code.
        let text = code.map_or_else(String::new, |code| code.to_string());
        let partial = format!("{path}.partial");
        if let Some(parent) = std::path::Path::new(&path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if std::fs::write(&partial, text).is_ok() {
            let _ = std::fs::rename(&partial, &path);
        }
    }
    Ok(code.unwrap_or(1))
}
