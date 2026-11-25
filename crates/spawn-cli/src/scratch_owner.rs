//! Scratch with an owner, an objective and a lease (objective 140):
//! `spawn scratch claim|renew|release` writes a folder's `.spawn-owner` as
//! one JSON line, exactly
//! `{"agent", "objective", "created_ms", "lease_until_ms", "build"}`:
//! - `agent`: 1 to 64 letters, digits, `.`, `_` and `-`;
//! - `objective`: `<mission>/<NNN>`, as `spawn verify` names it;
//! - `created_ms`, `lease_until_ms`: when it was claimed and when its lease
//!   ends (24 h by default, 1 to 168 h);
//! - `build`: the folder's build output, relative paths of plain parts whose
//!   last part is `target`: the only paths a later sweep (objective 145)
//!   may ever remove.
//!
//! The storage line's scratch walk (serve/core_lines/scratch.rs) reads it:
//! a claimed folder is owned while its lease runs, and nobody's ("lease
//! ended") after. Nothing here removes, moves or renames anything but the
//! owner file it writes (through a temporary file renamed over it), and no
//! symlink is followed: a folder named through one, or a build path
//! through one, is refused. `spawn scratch sweep` (crate::scratch_sweep,
//! objective 145) is the one that removes build paths.
//!
//! A folder removed whole (`rm -rf`, the usual end of a round) took its
//! claim with it: `release` of one that is gone prints `{"gone":"<dir>"}`
//! and exits 0, writing nothing (objective 220, [`gone`]). Gone is narrow:
//! an absolute name with no `.` or `..` part, every part of it that exists
//! a real directory (never followed), up to one that does not exist. Any
//! other name is released, or refused, as before; `renew` of a gone folder
//! is refused (a lease cannot run on nothing).

use std::path::{Component, Path, PathBuf};

use serde_json::{Value, json};

/// The owner file a folder holds (094's pid marker's name too).
pub(crate) const OWNER_FILE: &str = ".spawn-owner";

const HOUR_MS: i64 = 3_600_000;

/// The lease by default, and its bounds, in hours.
const LEASE_HOURS: i64 = 24;
const MOST_HOURS: i64 = 168;

/// The usage of `spawn scratch`, the only copy (main's help reads it).
pub(crate) const USAGE: &str = "usage: spawn scratch claim <dir> --agent <name> --objective <mission>/<objective> \
                     [--build <path>]... [--lease-hours <h>]\n       \
                     spawn scratch renew <dir> [--lease-hours <h>]\n       \
                     spawn scratch release <dir>\n       \
                     spawn scratch sweep [--excuse <pid>:<start ticks>]... \
                     [--apply --expect-digest <digest>]";

/// A folder's claim, as its owner file holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Claim {
    pub(crate) agent: String,
    /// `<mission>/<NNN>`.
    pub(crate) objective: String,
    pub(crate) created_ms: i64,
    pub(crate) lease_until_ms: i64,
    pub(crate) build: Vec<String>,
}

impl Claim {
    /// The owner file's one line.
    pub(crate) fn line(&self) -> String {
        json!({
            "agent": self.agent, "objective": self.objective, "created_ms": self.created_ms,
            "lease_until_ms": self.lease_until_ms, "build": self.build,
        })
        .to_string()
    }

    /// The claim an owner file's `text` holds: its five fields exactly,
    /// each as the rules above say; `None` for anything else (094's
    /// `<pid> <start ticks>` marker too).
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let Value::Object(fields) = serde_json::from_str::<Value>(text.trim_end()).ok()? else {
            return None;
        };
        let mut keys: Vec<&str> = fields.keys().map(String::as_str).collect();
        keys.sort_unstable();
        if keys
            != [
                "agent",
                "build",
                "created_ms",
                "lease_until_ms",
                "objective",
            ]
        {
            return None;
        }
        let agent = fields["agent"].as_str().filter(|agent| agent_ok(agent))?;
        let objective = fields["objective"].as_str()?;
        let (mission, number) = crate::verify::parse_name(objective).ok()?;
        if crate::verify::name(mission, number) != objective {
            return None;
        }
        let build = fields["build"]
            .as_array()?
            .iter()
            .map(|path| {
                path.as_str()
                    .filter(|path| build_path_ok(path))
                    .map(str::to_string)
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Claim {
            agent: agent.to_string(),
            objective: objective.to_string(),
            created_ms: fields["created_ms"].as_i64()?,
            lease_until_ms: fields["lease_until_ms"].as_i64()?,
            build,
        })
    }

    /// The `(mission, objective)` it names.
    pub(crate) fn pair(&self) -> Option<(i64, i64)> {
        crate::verify::parse_name(&self.objective).ok()
    }
}

/// An agent's name: 1 to 64 letters, digits, `.`, `_` and `-`.
fn agent_ok(agent: &str) -> bool {
    (1..=64).contains(&agent.len())
        && agent
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// A build path: relative, of plain parts (no `.` or `..`), its last part
/// `target`.
pub(crate) fn build_path_ok(path: &str) -> bool {
    let path = Path::new(path);
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        && path.file_name().is_some_and(|name| name == "target")
        && !path.to_string_lossy().contains("//")
}

/// The claim `dir`'s owner file holds, read without following a symlink:
/// `None` when it has none, or a file that is no claim (094's marker).
pub(crate) fn claim_in(dir: &Path) -> Option<Claim> {
    let file = dir.join(OWNER_FILE);
    let metadata = std::fs::symlink_metadata(&file).ok()?;
    if !metadata.is_file() {
        return None;
    }
    Claim::parse(&std::fs::read_to_string(&file).ok()?)
}

/// `spawn scratch claim|renew|release`: prints the owner file's line.
pub(crate) fn command(rest: &[String]) -> Result<i32, String> {
    // The sweep (objective 145) takes no folder: it reads the claimed ones.
    if rest.first().is_some_and(|verb| verb == "sweep") {
        return crate::scratch_sweep::command(&rest[1..]);
    }
    let (Some(verb), Some(dir)) = (rest.first(), rest.get(1)) else {
        return Err(USAGE.to_string());
    };
    let flags = Parsed::of(&rest[2..])?;
    let now = crate::verify::now_ms();
    let claim = match verb.as_str() {
        "claim" => claim(Path::new(dir), &flags, now)?,
        "renew" => {
            flags.only(&["lease-hours"])?;
            let hours = flags.lease_hours()?;
            change(Path::new(dir), |claim| {
                claim.lease_until_ms = now + hours * HOUR_MS;
            })?
        }
        "release" => {
            flags.only(&[])?;
            if gone(Path::new(dir)) {
                println!("{}", json!({ "gone": dir }));
                return Ok(0);
            }
            change(Path::new(dir), |claim| claim.lease_until_ms = now)?
        }
        other => return Err(format!("unknown scratch command {other}\n{USAGE}")),
    };
    println!("{}", claim.line());
    Ok(0)
}

/// The flags after the folder: each `--name value`, `--build` repeatable.
#[derive(Debug, Default)]
struct Parsed {
    values: Vec<(String, String)>,
}

impl Parsed {
    fn of(words: &[String]) -> Result<Self, String> {
        let mut parsed = Parsed::default();
        let mut words = words.iter();
        while let Some(word) = words.next() {
            let Some(name) = word.strip_prefix("--") else {
                return Err(format!("unexpected argument {word}\n{USAGE}"));
            };
            let value = words
                .next()
                .ok_or_else(|| format!("--{name} needs a value\n{USAGE}"))?;
            parsed.values.push((name.to_string(), value.clone()));
        }
        Ok(parsed)
    }

    /// Refuses any flag but `allowed`, and any but `--build` given twice.
    fn only(&self, allowed: &[&str]) -> Result<(), String> {
        for (index, (name, _)) in self.values.iter().enumerate() {
            if !allowed.contains(&name.as_str()) {
                return Err(format!("unknown flag --{name}\n{USAGE}"));
            }
            if name != "build" && self.values[..index].iter().any(|(seen, _)| seen == name) {
                return Err(format!("--{name} is given twice"));
            }
        }
        Ok(())
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|(seen, _)| seen == name)
            .map(|(_, value)| value.as_str())
    }

    fn all(&self, name: &str) -> Vec<&str> {
        self.values
            .iter()
            .filter(|(seen, _)| seen == name)
            .map(|(_, value)| value.as_str())
            .collect()
    }

    /// `--lease-hours`, 24 by default, 1 to 168.
    fn lease_hours(&self) -> Result<i64, String> {
        let Some(text) = self.get("lease-hours") else {
            return Ok(LEASE_HOURS);
        };
        text.parse::<i64>()
            .ok()
            .filter(|hours| (1..=MOST_HOURS).contains(hours))
            .ok_or_else(|| {
                format!("--lease-hours is a whole number from 1 to {MOST_HOURS}, not '{text}'")
            })
    }
}

/// Why `dir` cannot be claimed as named: not absolute, not its resolved
/// path, a symlink at it or above it, or not a directory.
fn folder_refused(dir: &Path) -> Option<String> {
    if !dir.is_absolute() {
        return Some(format!(
            "{} is not an absolute path: name the folder by its absolute, resolved path",
            dir.display()
        ));
    }
    for part in dir.ancestors() {
        match std::fs::symlink_metadata(part) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Some(format!(
                    "{} is a symlink: a folder is never claimed through one",
                    part.display()
                ));
            }
            Ok(_) => {}
            Err(error) => return Some(format!("{}: {error}", part.display())),
        }
    }
    match std::fs::canonicalize(dir) {
        Ok(resolved) if resolved == dir && resolved.is_dir() => None,
        Ok(resolved) if resolved == dir => Some(format!("{} is not a directory", dir.display())),
        Ok(resolved) => Some(format!(
            "{} resolves to {}: name the folder by its absolute, resolved path",
            dir.display(),
            resolved.display()
        )),
        Err(error) => Some(format!("{}: {error}", dir.display())),
    }
}

/// Why the build path `path` under `dir` is refused: not relative plain
/// parts ending in `target`, or a part that exists and is a symlink or not
/// a directory (one not made yet is taken).
fn build_refused(dir: &Path, path: &str) -> Option<String> {
    if !build_path_ok(path) {
        return Some(format!(
            "--build {path}: a build path is relative, of plain parts, ending in target"
        ));
    }
    let mut at = dir.to_path_buf();
    for part in Path::new(path).components() {
        at.push(part);
        match std::fs::symlink_metadata(&at) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Some(format!("--build {path}: {} is a symlink", at.display()));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Some(format!(
                    "--build {path}: {} is not a directory",
                    at.display()
                ));
            }
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

/// What `dir`'s owner file is now: `Ok(None)` none, `Ok(Some)` a claim;
/// refused when it is not a regular file or holds no claim.
fn owner_now(dir: &Path) -> Result<Option<Claim>, String> {
    let file = dir.join(OWNER_FILE);
    let Ok(metadata) = std::fs::symlink_metadata(&file) else {
        return Ok(None);
    };
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", file.display()));
    }
    let text =
        std::fs::read_to_string(&file).map_err(|error| format!("{}: {error}", file.display()))?;
    Claim::parse(&text)
        .map(Some)
        .ok_or_else(|| format!("{} does not hold a claim", file.display()))
}

/// `claim`: checked, then written.
fn claim(dir: &Path, flags: &Parsed, now: i64) -> Result<Claim, String> {
    flags.only(&["agent", "objective", "build", "lease-hours"])?;
    let hours = flags.lease_hours()?;
    let agent = flags
        .get("agent")
        .ok_or_else(|| format!("--agent is required\n{USAGE}"))?;
    if !agent_ok(agent) {
        return Err(format!(
            "--agent is 1 to 64 letters, digits, '.', '_' and '-', not '{agent}'"
        ));
    }
    let objective = flags
        .get("objective")
        .ok_or_else(|| format!("--objective is required\n{USAGE}"))?;
    let (mission, number) = crate::verify::parse_name(objective)?;
    if let Some(why) = folder_refused(dir) {
        return Err(why);
    }
    let mut build: Vec<String> = Vec::new();
    for path in flags.all("build") {
        if let Some(why) = build_refused(dir, path) {
            return Err(why);
        }
        if !build.iter().any(|seen| seen == path) {
            build.push(path.to_string());
        }
    }
    if let Some(running) = owner_now(dir)?.filter(|claim| claim.lease_until_ms > now) {
        return Err(format!(
            "{} is claimed by {} for {} until {}: renew or release it",
            dir.display(),
            running.agent,
            running.objective,
            running.lease_until_ms
        ));
    }
    let claim = Claim {
        agent: agent.to_string(),
        objective: crate::verify::name(mission, number),
        created_ms: now,
        lease_until_ms: now + hours * HOUR_MS,
        build,
    };
    write(dir, &claim)?;
    Ok(claim)
}

/// Whether `dir` is gone, its claim with it (objective 220): absolute, no
/// `.` or `..` part, and walking it from `/`, every part that exists is a
/// real directory (lstat, never followed), until one part does not exist
/// (`NotFound`). A symlink or a file on the way, a folder still there, or
/// any other error (a directory that cannot be searched) is not gone.
pub(crate) fn gone(dir: &Path) -> bool {
    let Some(text) = dir.to_str() else {
        return false;
    };
    if !dir.is_absolute() || text.split('/').any(|part| part == "." || part == "..") {
        return false;
    }
    let mut at = PathBuf::from("/");
    for part in dir.components() {
        match part {
            Component::RootDir => continue,
            Component::Normal(name) => at.push(name),
            _ => return false,
        }
        match std::fs::symlink_metadata(&at) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => return false,
            Err(error) => return error.kind() == std::io::ErrorKind::NotFound,
        }
    }
    false
}

/// `renew` and `release`: `dir`'s claim, changed and written again.
fn change(dir: &Path, apply: impl FnOnce(&mut Claim)) -> Result<Claim, String> {
    if let Some(why) = folder_refused(dir) {
        return Err(why);
    }
    let mut claim = owner_now(dir)?.ok_or_else(|| format!("{} holds no claim", dir.display()))?;
    apply(&mut claim);
    write(dir, &claim)?;
    Ok(claim)
}

/// The owner file, written through a temporary file renamed over it. The
/// temporary file is made new (objective 145's (7)): a symlink or a file
/// already at its name is never followed or overwritten.
fn write(dir: &Path, claim: &Claim) -> Result<(), String> {
    use std::io::Write;
    let file = dir.join(OWNER_FILE);
    let temporary: PathBuf = dir.join(format!("{OWNER_FILE}.{}.tmp", std::process::id()));
    let mut made = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| format!("{}: {error}", temporary.display()))?;
    made.write_all(format!("{}\n", claim.line()).as_bytes())
        .and_then(|()| std::fs::rename(&temporary, &file))
        .map_err(|error| {
            let _ = std::fs::remove_file(&temporary);
            format!("{}: {error}", file.display())
        })
}

#[cfg(test)]
mod tests;
