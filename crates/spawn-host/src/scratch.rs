//! A test's scratch directory, removed when the test ends.
//!
//! A test makes its scratch under the temporary directory and holds the
//! [`Scratch`] for as long as it needs it. When the test passes, dropping it
//! removes the directory and everything in it; when the test fails (the
//! guard is dropped while the thread panics), the directory is kept for
//! debugging and its path is printed. Only the directory the guard made is
//! ever removed: never anything found by a name or a pattern, since other
//! processes keep their own scratch next to it.

use std::path::{Path, PathBuf};

/// A new, empty directory under [`std::env::temp_dir`], removed on drop
/// unless the thread is panicking (a failing test keeps it, and says where).
#[derive(Debug)]
pub struct Scratch {
    path: PathBuf,
}

impl Scratch {
    /// A new directory `<temp_dir>/<prefix>-<pid>-<n>`, unique within this
    /// process (`n` counts the scratches it made).
    ///
    /// # Panics
    ///
    /// When the directory cannot be created: a test cannot go on without it.
    pub fn new(prefix: &str) -> Self {
        Self::new_in(&std::env::temp_dir(), prefix)
    }

    /// [`Scratch::new`] under `parent` instead of the temporary directory.
    ///
    /// # Panics
    ///
    /// When the directory cannot be created.
    pub fn new_in(parent: &Path, prefix: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let ordinal = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = parent.join(format!("{prefix}-{}-{ordinal}", std::process::id()));
        // A directory a dead process of the same pid left: this process made
        // none of that name yet, so whatever is there is stale.
        let _ = remove_tree(&path);
        std::fs::create_dir_all(&path)
            .unwrap_or_else(|error| panic!("create the scratch {}: {error}", path.display()));
        Self { path }
    }

    /// The directory.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl std::ops::Deref for Scratch {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("the failed test's scratch is kept: {}", self.path.display());
            return;
        }
        if let Err(error) = remove_tree(&self.path) {
            eprintln!("cannot remove the scratch {}: {error}", self.path.display());
        }
    }
}

/// Remove `path` and everything under it; a directory a test made
/// read-only is made writable first. A path already gone is no error.
pub fn remove_tree(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => {
            make_writable(path);
            match std::fs::remove_dir_all(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                other => other,
            }
        }
    }
}

/// Give the owner every right on each directory under `path` (never
/// following a symbolic link), so its entries can be removed.
fn make_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    let mode = meta.permissions().mode() | 0o700;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            make_writable(&entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Scratch;

    #[test]
    fn a_passing_tests_scratch_is_removed() {
        let scratch = Scratch::new("spawn-scratch-pass");
        let path = scratch.path().to_path_buf();
        std::fs::create_dir_all(scratch.join("a/b")).expect("nest");
        std::fs::write(scratch.join("a/b/file"), "x").expect("write");
        drop(scratch);
        assert!(!path.exists(), "removed: {}", path.display());
    }

    #[test]
    fn a_read_only_tree_is_removed_too() {
        use std::os::unix::fs::PermissionsExt as _;
        let scratch = Scratch::new("spawn-scratch-read-only");
        let path = scratch.path().to_path_buf();
        let locked = scratch.join("locked");
        std::fs::create_dir(&locked).expect("dir");
        std::fs::write(locked.join("file"), "x").expect("write");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).expect("lock");
        drop(scratch);
        assert!(!path.exists(), "removed: {}", path.display());
    }

    #[test]
    fn a_failing_tests_scratch_is_kept() {
        let kept = std::sync::Arc::new(std::sync::Mutex::new(None));
        let seen = std::sync::Arc::clone(&kept);
        let failed = std::thread::spawn(move || {
            let scratch = Scratch::new("spawn-scratch-fail");
            *seen.lock().expect("lock") = Some(scratch.path().to_path_buf());
            panic!("the test fails");
        })
        .join();
        assert!(failed.is_err(), "the thread panicked");
        let path = kept.lock().expect("lock").take().expect("a path");
        assert!(path.is_dir(), "kept: {}", path.display());
        // This test passes: what the failing thread kept is its own to remove.
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn two_scratches_of_one_prefix_are_distinct() {
        let (one, two) = (
            Scratch::new("spawn-scratch-twice"),
            Scratch::new("spawn-scratch-twice"),
        );
        assert_ne!(one.path(), two.path());
        assert!(one.path().is_dir() && two.path().is_dir());
    }
}
