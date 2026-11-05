//! `spawn chunks gc`: remove chunk-store objects that no reachable commit
//! names (docs/storage.md, "The chunk store").
//!
//! The store is `SPAWN_CHUNK_STORE`, else `<root>/.chunks`; the root is
//! `--root`, else `SPAWN_WORKSPACE_ROOT`. The repositories whose commits keep
//! objects alive are the default repository (`--repo`, else `SPAWN_GIT_REPO`)
//! and every bare repository directly under `<root>/.sources/`. An object
//! goes when it is unreferenced and older than `--grace` minutes (60 by
//! default, at least [`MIN_GRACE_MINUTES`]); any read error deletes nothing
//! and exits non-zero. A default repository without any ref is refused:
//! nothing in it would keep an object alive.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::Flags;
use crate::config::{chunk_store, env_value, workspace_root};

const USAGE: &str = "usage: spawn chunks gc [--grace <minutes>] [--root <dir>] [--repo <path>]";

/// Grace period when `--grace` is not given.
const DEFAULT_GRACE_MINUTES: u64 = 60;

/// The shortest grace period: chunks are written before the commit that
/// names them, so a shorter one could take the chunks and manifest of a
/// checkpoint still in flight.
const MIN_GRACE_MINUTES: u64 = 10;

/// `spawn chunks <gc>`.
pub(crate) fn command(rest: &[String]) -> Result<i32, String> {
    let Some((word, flags)) = rest.split_first() else {
        return Err(USAGE.to_string());
    };
    if word != "gc" {
        return Err(format!("unknown chunks command {word}\n{USAGE}"));
    }
    let flags = Flags::parse(flags).map_err(|error| format!("{error}\n{USAGE}"))?;
    gc(&flags)
}

/// The grace period of `--grace <minutes>`.
fn grace(flag: Option<String>) -> Result<Duration, String> {
    let minutes = match flag {
        None => DEFAULT_GRACE_MINUTES,
        Some(text) => text
            .trim()
            .parse::<u64>()
            .map_err(|_| format!("--grace takes whole minutes, not {text}\n{USAGE}"))?,
    };
    if minutes < MIN_GRACE_MINUTES {
        return Err(format!(
            "--grace {minutes} is under {MIN_GRACE_MINUTES} minutes: it could remove the chunks \
             of a checkpoint still being written\n{USAGE}"
        ));
    }
    Ok(Duration::from_secs(minutes.saturating_mul(60)))
}

fn gc(flags: &Flags) -> Result<i32, String> {
    let grace = grace(flags.get("grace"))?;
    let root = workspace_root(flags)
        .ok_or_else(|| format!("--root or SPAWN_WORKSPACE_ROOT is required\n{USAGE}"))?;
    let repo = flags
        .get("repo")
        .or_else(|| env_value("SPAWN_GIT_REPO"))
        .map(PathBuf::from)
        .ok_or_else(|| format!("--repo or SPAWN_GIT_REPO is required\n{USAGE}"))?;
    refuse_empty(&repo)?;
    let store = chunk_store(&root);
    let repositories = spawn_host::gc::repositories(&repo, Path::new(&root))?;
    let collected = spawn_host::gc::collect(store.as_ref(), &repositories, grace)
        .map_err(|error| format!("chunks gc: nothing deleted: {error}"))?;
    for name in &collected.skipped {
        eprintln!(
            "chunks gc: warning: manifest {name} is not in this store and only refs outside \
             refs/heads name it (another host's branch); skipped"
        );
    }
    println!(
        "chunks gc: kept {} objects, removed {} objects, freed {} bytes",
        collected.kept, collected.removed, collected.freed
    );
    Ok(0)
}

/// Refuse a default repository without any ref: a wrong `--repo` would make
/// every object of the store collectable.
fn refuse_empty(repo: &Path) -> Result<(), String> {
    if spawn_host::gc::has_refs(repo)? {
        return Ok(());
    }
    Err(format!(
        "chunks gc: nothing deleted: the repository {} has no refs, so nothing in it keeps an \
         object alive; is --repo (or SPAWN_GIT_REPO) right?",
        repo.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::{grace, refuse_empty};
    use std::time::Duration;

    #[test]
    fn the_grace_period_is_in_minutes_and_defaults_to_an_hour() {
        assert_eq!(grace(None), Ok(Duration::from_hours(1)));
        assert_eq!(grace(Some("30".to_string())), Ok(Duration::from_mins(30)));
        assert_eq!(grace(Some("10".to_string())), Ok(Duration::from_mins(10)));
        assert!(grace(Some("-1".to_string())).is_err());
        assert!(grace(Some("soon".to_string())).is_err());
    }

    #[test]
    fn a_grace_period_that_could_take_a_checkpoint_in_flight_is_refused() {
        for short in ["0", "9"] {
            let refused = grace(Some(short.to_string()));
            assert!(
                refused
                    .as_ref()
                    .is_err_and(|error| error.contains("under 10 minutes")),
                "{short}: {refused:?}"
            );
        }
    }

    #[test]
    fn a_default_repository_without_refs_is_refused() {
        let directory =
            std::env::temp_dir().join(format!("spawn-chunks-cli-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let repo = directory.join("empty.git");
        let git = |arguments: &[&str]| {
            let status = std::process::Command::new("git")
                .arg("--git-dir")
                .arg(&repo)
                .args(arguments)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@localhost")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@localhost")
                .output()
                .expect("git is installed");
            assert!(status.status.success(), "{status:?}");
            String::from_utf8_lossy(&status.stdout).trim().to_string()
        };
        std::fs::create_dir_all(&directory).expect("scratch");
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q", "--bare"])
                .arg(&repo)
                .status()
                .expect("git")
                .success()
        );
        let refused = refuse_empty(&repo);
        assert!(
            refused
                .as_ref()
                .is_err_and(|error| error.contains("has no refs")),
            "{refused:?}"
        );
        let tree = git(&["mktree"]);
        let commit = git(&["commit-tree", &tree, "-m", "base"]);
        git(&["update-ref", "refs/heads/main", &commit]);
        assert_eq!(refuse_empty(&repo), Ok(()));
        assert!(refuse_empty(&directory.join("absent.git")).is_err());
        let _ = std::fs::remove_dir_all(&directory);
    }
}
