//! Paths in a git tree, as bytes.
//!
//! git keeps a file name as the bytes the file system gave it, which need
//! not be UTF-8 and may hold a newline. A path read from git stays bytes on
//! its way back to git (a pathspec, `<tree>:<path>`) and to the file system
//! (a workspace file); it is converted, lossily, only to be shown.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

/// A `/`-separated path relative to the top of a tree, exactly as git holds
/// it.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TreePath(Vec<u8>);

impl TreePath {
    /// The path of these bytes.
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    /// The bytes git holds.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// The path for the file system, relative to a workspace.
    pub fn as_path(&self) -> &Path {
        Path::new(OsStr::from_bytes(&self.0))
    }

    /// The path for display only: bytes that are not UTF-8 are replaced.
    pub fn shown(&self) -> String {
        String::from_utf8_lossy(&self.0).into_owned()
    }

    /// The path as a UTF-8 string, when it is one.
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }

    /// A pathspec that matches this path and nothing else: `:(<magic>)`
    /// followed by the bytes, `magic` holding `literal` (a file named `*`
    /// or `:/` must not match others).
    pub fn pathspec(&self, magic: &str) -> OsString {
        let mut spec = format!(":({magic})").into_bytes();
        spec.extend_from_slice(&self.0);
        OsString::from_vec(spec)
    }

    /// `<tree>:<path>`, the object at this path of a tree or commit.
    pub fn in_tree(&self, tree: &str) -> OsString {
        let mut name = format!("{tree}:").into_bytes();
        name.extend_from_slice(&self.0);
        OsString::from_vec(name)
    }

    /// The path's components, split at `/`.
    pub fn components(&self) -> impl DoubleEndedIterator<Item = &[u8]> {
        self.0.split(|byte| *byte == b'/')
    }

    /// The file name: the last component.
    pub fn file_name(&self) -> &[u8] {
        self.components().next_back().unwrap_or_default()
    }

    /// Every leading path of this one, then the path itself: `a`, `a/b`,
    /// `a/b/c` for `a/b/c`.
    pub fn prefixes(&self) -> impl Iterator<Item = TreePath> + '_ {
        self.0
            .iter()
            .enumerate()
            .filter(|(_, byte)| **byte == b'/')
            .map(|(at, _)| at)
            .chain([self.0.len()])
            .map(|end| Self(self.0[..end].to_vec()))
    }

    /// A relative path of real names only: not empty, no NUL, no empty
    /// component (a leading, doubled or trailing `/`), no `.`, `..` or
    /// `.git` component. Anything else never reaches a workspace.
    pub fn is_safe(&self) -> bool {
        !self.0.is_empty()
            && !self.0.contains(&0)
            && self
                .components()
                .all(|name| !matches!(name, b"" | b"." | b".." | b".git"))
    }
}

impl From<&str> for TreePath {
    fn from(path: &str) -> Self {
        Self(path.as_bytes().to_vec())
    }
}

impl fmt::Debug for TreePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.shown(), formatter)
    }
}

impl fmt::Display for TreePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.shown())
    }
}

/// The fields of a NUL-separated (`-z`) listing, the empty end dropped.
pub fn nul_fields(listing: &[u8]) -> impl Iterator<Item = &[u8]> {
    listing
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
}

#[cfg(test)]
mod tests {
    use super::TreePath;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn a_path_keeps_its_bytes_and_is_only_shown_lossily() {
        let path = TreePath::from_bytes(b"dir/bad\xffname\n".to_vec());
        assert_eq!(path.as_bytes(), b"dir/bad\xffname\n");
        assert_eq!(path.shown(), "dir/bad\u{fffd}name\n");
        assert_eq!(path.as_str(), None);
        assert_eq!(path.file_name(), b"bad\xffname\n");
        assert_eq!(
            path.prefixes().collect::<Vec<_>>(),
            vec![TreePath::from("dir"), path.clone()]
        );
        assert_eq!(
            path.pathspec("literal").as_bytes(),
            b":(literal)dir/bad\xffname\n"
        );
        assert_eq!(path.in_tree("HEAD").as_bytes(), b"HEAD:dir/bad\xffname\n");
        assert!(path.is_safe());
        for unsafe_path in [
            "",
            "/etc/passwd",
            "a//b",
            "a/",
            "./a",
            "a/../../x",
            ".git/config",
        ] {
            assert!(!TreePath::from(unsafe_path).is_safe(), "{unsafe_path}");
        }
        assert!(!TreePath::from_bytes(b"a\0b".to_vec()).is_safe());
    }
}
