//! `run`, the command an engine's terminal starts, and `hook`.
//!
//! The pane holds no database login: the runner writes what the Bridge
//! cannot see for itself (the exit status, a copy of Codex output) to files
//! the Bridge reads. Claude Code's own transcript and screen tell the Bridge
//! the rest.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};

use crate::config::{env_or, env_value};

/// `spawn hook`: accepted and ignored. Engines started by an older build
/// still call it from their hook settings; their events come from the
/// transcript now.
// ponytail: delete once no execution started before the transcript reader
// is still running.
pub(crate) fn hook() -> i32 {
    let mut payload = String::new();
    let _ = std::io::stdin().read_to_string(&mut payload);
    0
}

/// `spawn run -- <program> [args]`: start the engine, write its exit status
/// to `SPAWN_EXIT_FILE`, and forward its exit code. With
/// `SPAWN_ENGINE_OUTPUT` set (Codex), every output line is also appended
/// there.
pub(crate) fn run(rest: &[String]) -> Result<i32, String> {
    let words: &[String] = match rest.split_first() {
        Some((first, tail)) if first == "--" => tail,
        Some(_) => rest,
        None => return Err("run needs a program after --".to_string()),
    };
    let (program, arguments) = words
        .split_first()
        .ok_or_else(|| "run needs a program after --".to_string())?;
    let copy =
        env_value("SPAWN_ENGINE_OUTPUT").filter(|_| env_or("SPAWN_ENGINE", "claude") == "codex");
    let mut command = Command::new(program);
    command.args(arguments);
    if copy.is_some() {
        command.stdout(Stdio::piped());
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("start {program}: {error}"))?;
    if let (Some(stdout), Some(path)) = (child.stdout.take(), copy.as_deref()) {
        // Echo every line so the pane still shows the engine output; the
        // copy is what the Bridge reads.
        let mut file = open_append(path);
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            println!("{line}");
            let _ = std::io::stdout().flush();
            if let Some(file) = file.as_mut() {
                let _ = writeln!(file, "{line}");
                let _ = file.flush();
            }
        }
    }
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

fn open_append(path: &str) -> Option<std::fs::File> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}
