//! `run`, the command an engine's terminal starts.
//!
//! The pane holds no database login: the runner writes what the Bridge
//! cannot see for itself, the exit status, to a file the Bridge reads.
//! Claude Code's own transcript and screen tell the Bridge the rest.
//!
//! **An engine that runs in a resource envelope** (objective 029: the pane
//! runs `spawn run` through `systemd-run --scope` inside Spawn's slice) also
//! has the runner note its cgroup's memory peak (`SPAWN_ENVELOPE_FILE`) while
//! it runs: the scope's cgroup is gone by the time the Bridge reads the end,
//! so the last reading the runner wrote is what an end's evidence can say of
//! how high it went. The engine and everything it starts are in the runner's
//! cgroup, so `memory.peak` covers them all.

use std::process::Command;
use std::time::Duration;

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
    if let Some(path) = env_value("SPAWN_ENVELOPE_FILE") {
        note_memory_peak(path);
    }
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

/// Note this process's cgroup `memory.peak` in `path` while the process runs:
/// a thread reads it every half second and writes the value when it grew,
/// whole then renamed so the Bridge never reads half a number. A process
/// outside an envelope (the run's cgroup is the pane's) notes nothing it can
/// be judged by, but writes nothing either.
fn note_memory_peak(path: String) {
    let Ok(cgroups) = std::fs::read_to_string("/proc/self/cgroup") else {
        return;
    };
    let Some(cgroup) = cgroups
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(|rest| rest.trim().to_string())
    else {
        return;
    };
    let peak = format!("/sys/fs/cgroup{cgroup}/memory.peak");
    if !std::path::Path::new(&peak).exists() {
        return;
    }
    std::thread::spawn(move || {
        let mut last: Option<u64> = None;
        loop {
            if let Ok(text) = std::fs::read_to_string(&peak)
                && let Ok(value) = text.trim().parse::<u64>()
                && last != Some(value)
            {
                write_peak(&path, value);
                last = Some(value);
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });
}

/// `value` in `path`, whole: written beside it and renamed, so a reader
/// never sees half a number.
fn write_peak(path: &str, value: u64) {
    let partial = format!("{path}.partial");
    if let Some(parent) = std::path::Path::new(path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::write(&partial, value.to_string()).is_ok() {
        let _ = std::fs::rename(&partial, path);
    }
}

#[cfg(test)]
mod tests {
    use super::write_peak;

    #[test]
    fn the_peak_file_is_written_whole() {
        let directory = std::env::temp_dir().join(format!(
            "spawn-peak-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&directory).expect("directory");
        let path = directory.join("envelope-exec_1");
        write_peak(path.to_str().unwrap(), 96 * 1024 * 1024);
        assert_eq!(
            std::fs::read_to_string(&path).expect("the peak"),
            (96 * 1024 * 1024).to_string()
        );
        assert!(!directory.join("envelope-exec_1.partial").exists());
        let _ = std::fs::remove_dir_all(&directory);
    }
}
