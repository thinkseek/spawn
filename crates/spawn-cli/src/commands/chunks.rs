//! `spawn chunks gc`: remove chunk-store objects that no reachable commit
//! names (docs/storage.md, "The chunk store").
//!
//! The store is `SPAWN_CHUNK_STORE`, else `<root>/.chunks`; the root is
//! `--root`, else `SPAWN_WORKSPACE_ROOT`. The repositories whose commits keep
//! objects alive are the default repository (`--repo`, else `SPAWN_GIT_REPO`)
//! and every bare repository directly under `<root>/.sources/`. An object
//! goes when it is unreferenced and older than `--grace` minutes (60 by
//! default); any read error deletes nothing and exits non-zero.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::Flags;
use crate::config::{chunk_store, env_value, workspace_root};

const USAGE: &str = "usage: spawn chunks gc [--grace <minutes>] [--root <dir>] [--repo <path>]";

/// Grace period when `--grace` is not given.
const DEFAULT_GRACE_MINUTES: u64 = 60;

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
    let store = chunk_store(&root);
    let repositories = spawn_host::gc::repositories(&repo, Path::new(&root))?;
    let collected = spawn_host::gc::collect(store.as_ref(), &repositories, grace)
        .map_err(|error| format!("chunks gc: nothing deleted: {error}"))?;
    println!(
        "chunks gc: kept {} objects, removed {} objects, freed {} bytes",
        collected.kept, collected.removed, collected.freed
    );
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::grace;
    use std::time::Duration;

    #[test]
    fn the_grace_period_is_in_minutes_and_defaults_to_an_hour() {
        assert_eq!(grace(None), Ok(Duration::from_secs(3600)));
        assert_eq!(grace(Some("30".to_string())), Ok(Duration::from_secs(1800)));
        assert_eq!(grace(Some("0".to_string())), Ok(Duration::ZERO));
        assert!(grace(Some("-1".to_string())).is_err());
        assert!(grace(Some("soon".to_string())).is_err());
    }
}
