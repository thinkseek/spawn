//! `tree_sha256_v1` (section 7): the digest of a directory, shared by the
//! integrator's `scenarios_sha256` (`integrate/inputs.rs`) and the node's
//! `registry` key. Every regular file under the directory (symbolic links
//! are not followed), its path relative to the directory, sorted bytewise;
//! the digest is the SHA-256 of the lines `<hex SHA-256 of the file>
//! <path>\n` (sha256sum's format, two spaces).

use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};

use crate::codec::hex;

/// The file names skipped for `registry` only: cargo's lock files, which
/// change while gates run.
pub const TREE_SKIP_REGISTRY: [&str; 2] = [".package-cache", ".package-cache-mutate"];

/// The digest of `dir`, skipping files whose name is one of `skip`.
pub fn tree_sha256_v1(dir: &Path, skip: &[&str]) -> std::io::Result<String> {
    let mut files: Vec<(Vec<u8>, PathBuf)> = Vec::new();
    walk(dir, Path::new(""), skip, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut all = Sha256::new();
    for (relative, path) in files {
        let bytes = std::fs::read(&path)?;
        all.update(hex(&Sha256::digest(&bytes)).as_bytes());
        all.update(b"  ");
        all.update(&relative);
        all.update(b"\n");
    }
    Ok(hex(&all.finalize()))
}

/// The scenario set's digest (`inputs.scenarios_sha256`); `None` when the
/// directory cannot be read whole.
pub fn scenarios_sha256(dir: &Path) -> Option<String> {
    tree_sha256_v1(dir, &[]).ok()
}

/// The `registry` value of a cargo registry directory.
pub fn registry_sha256(dir: &Path) -> std::io::Result<String> {
    tree_sha256_v1(dir, &TREE_SKIP_REGISTRY)
}

fn walk(
    dir: &Path,
    relative: &Path,
    skip: &[&str],
    files: &mut Vec<(Vec<u8>, PathBuf)>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let name = relative.join(entry.file_name());
        if kind.is_dir() {
            walk(&entry.path(), &name, skip, files)?;
        } else if kind.is_file()
            && !skip
                .iter()
                .any(|s| entry.file_name().as_bytes() == s.as_bytes())
        {
            files.push((name.as_os_str().as_bytes().to_vec(), entry.path()));
        }
    }
    Ok(())
}
