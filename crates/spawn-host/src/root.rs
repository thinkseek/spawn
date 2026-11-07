//! The workspace root as the engines see it.
//!
//! An engine runs in its workspace, and whatever names the workspace by
//! path must name it as the engine's process does. tmux starts the pane in
//! the directory it is given; the kernel resolves every symlink on the way,
//! so the process's working directory (`getcwd`, `pwd -P`) is the resolved
//! path, and Claude Code files its transcript under that path (every
//! character but ASCII letters and digits replaced by `-`). A root reached
//! through a symlink (`/home` -> `/data00/home`; on macOS `/tmp` and `/var`
//! are links to `/private/...`) would otherwise give each workspace two
//! names: the Bridge would watch the transcript of the one no engine writes.
//!
//! So the root is resolved once, where it is configured ([`canonical_root`]),
//! and every workspace id is minted under the resolved root: the mount, the
//! pane's working directory, the runner's exit file and the transcript path
//! all come from that one id. Ids recorded under the root as it was
//! configured, by a version that did not resolve it, still name the same
//! directory; [`legacy_workspace`] says where it is now.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// `path` with every symlink in it resolved, as `realpath` gives it, also
/// when it does not exist yet: the longest prefix that exists is resolved
/// and the rest appended as given. A relative path is taken from the
/// current directory. Called where a root is configured, never per use.
pub fn canonical_root(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| path.to_path_buf(), |here| here.join(path))
    };
    let mut existing = absolute.as_path();
    let mut missing: Vec<&OsStr> = Vec::new();
    loop {
        if let Ok(mut resolved) = std::fs::canonicalize(existing) {
            resolved.extend(missing.iter().rev());
            return resolved;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name);
                existing = parent;
            }
            _ => return absolute,
        }
    }
}

/// Where the workspace directory `directory` of `branch` is under the
/// resolved `root`, when it was named under another spelling of that root
/// (`<old root>/<branch>`, recorded before the root was resolved through
/// its symlinks): the old root resolves to `root`. `None` when it names a
/// directory under no spelling of `root`.
pub fn legacy_workspace(root: &Path, branch: &str, directory: &Path) -> Option<PathBuf> {
    if branch.is_empty() {
        return None;
    }
    let mut old_root = directory;
    for _ in Path::new(branch).components() {
        old_root = old_root.parent()?;
    }
    (old_root.join(branch) == directory && canonical_root(old_root) == root)
        .then(|| root.join(branch))
}

#[cfg(test)]
mod tests {
    use super::{canonical_root, legacy_workspace};

    fn scratch(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("spawn-root-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("real/ws")).expect("scratch");
        std::os::unix::fs::symlink(base.join("real"), base.join("link")).expect("link");
        std::fs::canonicalize(&base).expect("resolve the scratch directory")
    }

    #[test]
    fn a_root_reached_through_a_symlink_resolves_to_the_real_directory() {
        let base = scratch("resolve");
        assert_eq!(canonical_root(&base.join("link/ws")), base.join("real/ws"));
        // A root that does not exist yet resolves as far as it exists.
        assert_eq!(
            canonical_root(&base.join("link/ws/later/deeper")),
            base.join("real/ws/later/deeper")
        );
        // A resolved root is its own resolution.
        assert_eq!(canonical_root(&base.join("real/ws")), base.join("real/ws"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_workspace_named_under_the_configured_root_is_found_under_the_resolved_one() {
        let base = scratch("legacy");
        let root = canonical_root(&base.join("link/ws"));
        let old = base.join("link/ws/agent_one");
        assert_eq!(
            legacy_workspace(&root, "agent_one", &old),
            Some(root.join("agent_one"))
        );
        // Not a workspace of that branch, or not under the root at all.
        assert_eq!(legacy_workspace(&root, "agent_two", &old), None);
        assert_eq!(
            legacy_workspace(&root, "agent_one", &base.join("real/agent_one")),
            None
        );
        assert_eq!(legacy_workspace(&root, "", &old), None);
        let _ = std::fs::remove_dir_all(&base);
    }
}
