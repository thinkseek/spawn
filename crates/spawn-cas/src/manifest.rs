//! manifest-v1: a data snapshot's manifest, and the snapshot pins of
//! `spawn prov snapshot` (PV3b, docs/provenance.md).
//!
//! # The format
//!
//! manifest-v1 **is** the `tree_sha256_v1` line list (spawn-node v1,
//! section 7): one line per regular file under the snapshot's root,
//!
//! ```text
//! <64 lowercase hex: SHA-256 of the file>  <path relative to the root>\n
//! ```
//!
//! sorted bytewise by path, each path once. Only regular files are listed:
//! a directory is its files, a symbolic link is never followed (one that
//! points outside the root is refused, [`CasError::SymlinkEscape`]), and
//! other special files are left out. The snapshot's identity is the SHA-256
//! of those bytes, algorithm-prefixed: `sha256:<tree_sha256_v1>`. File sizes
//! are not part of it (they go in provenance entries).
//!
//! # Snapshot pins
//!
//! `pins/prov/<ref>` keeps the file bytes of the [`KEEP_SNAPSHOTS`] newest
//! snapshots of a data ref: for each, newest first, a `# snapshot <digest>`
//! line (a comment: [`Store::pinned`] reads only lines that are digests)
//! followed by the manifest's digest and every file's. Then a `# manifests`
//! line and the manifest of every snapshot the ref ever had: manifests are
//! pinned forever (decision D3, class A), only file bytes are capped. A pin
//! file is replaced whole (temporary file and `rename`, no fsync:
//! crash-only), under the pin/GC lock. The file objects of a snapshot that
//! leaves the newest three, which no pin names any more and which no
//! namespace but `data` records, are removed right away
//! ([`Store::keep_snapshots`]); its manifest stays.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

use crate::store::{check_name, open_nofollow, plain_dir, remove_missing_ok, unique_suffix};
use crate::{Algo, CasError, Digest, NS_DATA, Store, is_hex64, sha256_of, sha256_reader};

/// The format's name.
pub const MANIFEST_V1: &str = "manifest-v1";

/// The pin owner of the data snapshots: `pins/prov/<ref>`.
pub const PROV_PIN_OWNER: &str = "prov";

/// How many snapshots of a data ref its pin keeps, newest first.
pub const KEEP_SNAPSHOTS: usize = 3;

/// The comment line that opens a snapshot in a pin file.
const SNAPSHOT_LINE: &str = "# snapshot ";

/// The comment line that opens the manifests section of a pin file.
const MANIFESTS_LINE: &str = "# manifests";

/// A data ref's pin, as read back.
#[derive(Debug, Default)]
struct SnapshotPin {
    /// The kept snapshots, newest first, each with what is pinned for it.
    snapshots: Vec<(Digest, Vec<Digest>)>,
    /// Every manifest pinned forever.
    manifests: Vec<Digest>,
}

/// One regular file of a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    /// Its path relative to the root, as the file system spells it.
    pub path: Vec<u8>,
    /// The SHA-256 of its bytes.
    pub digest: Digest,
    /// Its size when it was read ([`Manifest::build`]); `None` from a
    /// parsed manifest (sizes are not part of the format).
    pub size: Option<u64>,
}

/// A manifest-v1: the files of a snapshot, sorted bytewise, each once.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    entries: Vec<ManifestEntry>,
}

fn bad(line: usize, reason: &'static str) -> CasError {
    CasError::BadManifest { line, reason }
}

/// Whether `path` may be a manifest path: not empty, relative, no empty,
/// `.` or `..` component, and no newline (one line per file).
fn valid_path(path: &[u8]) -> Result<(), &'static str> {
    if path.is_empty() {
        return Err("an empty path");
    }
    if path.contains(&b'\n') {
        return Err("a path with a newline");
    }
    if path.contains(&0) {
        return Err("a path with a NUL byte");
    }
    if path.split(|&byte| byte == b'/').any(|part| {
        let part: &[u8] = part;
        part.is_empty() || part == b"." || part == b".."
    }) {
        return Err("not a plain relative path");
    }
    Ok(())
}

impl Manifest {
    /// The manifest of `entries`, sorted; a path twice, or one that is not
    /// a plain relative path, is refused.
    pub fn from_entries(mut entries: Vec<ManifestEntry>) -> Result<Self, CasError> {
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        for (at, entry) in entries.iter().enumerate() {
            valid_path(&entry.path).map_err(|reason| bad(at + 1, reason))?;
            if entry.digest.algo() != Algo::Sha256 {
                return Err(bad(at + 1, "a manifest-v1 digest is SHA-256"));
            }
            if at > 0 && entries[at - 1].path == entry.path {
                return Err(bad(at + 1, "a path twice"));
            }
        }
        Ok(Self { entries })
    }

    /// The manifest of the directory `root` (module doc): every regular file
    /// under it, hashed, with its size. Symbolic links are never followed;
    /// one whose target (read as a path, whether or not it exists) is
    /// outside `root` is refused ([`CasError::SymlinkEscape`]). Files are
    /// opened `O_NOFOLLOW | O_NONBLOCK`, so a file swapped for a link or a
    /// FIFO while the walk runs is refused, never followed or waited on.
    pub fn build(root: &Path) -> Result<Self, CasError> {
        let root = fs::canonicalize(root)?;
        if !fs::symlink_metadata(&root)?.is_dir() {
            return Err(CasError::NotPlain(root));
        }
        let mut entries = Vec::new();
        walk(&root, &root, &mut Vec::new(), &mut entries)?;
        Self::from_entries(entries)
    }

    /// Parse manifest-v1 bytes: every line `<64 hex>  <path>\n`, sorted
    /// strictly bytewise, the last one ended too. Anything else is refused
    /// ([`CasError::BadManifest`], with the line).
    pub fn parse(bytes: &[u8]) -> Result<Self, CasError> {
        let mut entries: Vec<ManifestEntry> = Vec::new();
        if bytes.is_empty() {
            return Ok(Self { entries });
        }
        let Some(body) = bytes.strip_suffix(b"\n") else {
            return Err(bad(
                bytes.split(|&byte| byte == b'\n').count(),
                "the last line is not ended",
            ));
        };
        for (at, line) in body.split(|&byte| byte == b'\n').enumerate() {
            let number = at + 1;
            if line.len() < 67 || &line[64..66] != b"  " {
                return Err(bad(number, "not <64 hex>  <path>"));
            }
            let hex = std::str::from_utf8(&line[..64]).map_err(|_| bad(number, "not hex"))?;
            if !is_hex64(hex) {
                return Err(bad(number, "not 64 lowercase hex digits"));
            }
            let path = line[66..].to_vec();
            valid_path(&path).map_err(|reason| bad(number, reason))?;
            if let Some(last) = entries.last()
                && last.path >= path
            {
                return Err(bad(number, "not sorted bytewise, or a path twice"));
            }
            entries.push(ManifestEntry {
                path,
                digest: Digest::from_parts(Algo::Sha256, hex)?,
                size: None,
            });
        }
        Ok(Self { entries })
    }

    /// The files, sorted bytewise.
    pub fn entries(&self) -> &[ManifestEntry] {
        &self.entries
    }

    /// How many files.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether it lists no file.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The bytes: the `tree_sha256_v1` line list.
    pub fn text(&self) -> Vec<u8> {
        let mut text = Vec::with_capacity(self.entries.len() * 100);
        for entry in &self.entries {
            text.extend_from_slice(entry.digest.hex().as_bytes());
            text.extend_from_slice(b"  ");
            text.extend_from_slice(&entry.path);
            text.push(b'\n');
        }
        text
    }

    /// The snapshot's identity: `sha256:<tree_sha256_v1>`.
    pub fn digest(&self) -> Digest {
        sha256_of(&self.text())
    }

    /// The bytes the files hold, by their sizes (0 for a parsed manifest).
    pub fn bytes(&self) -> u64 {
        self.entries.iter().filter_map(|entry| entry.size).sum()
    }

    /// The files under the top-level directory `top`, their paths relative
    /// to it (a directory's own manifest).
    #[must_use]
    pub fn subtree(&self, top: &[u8]) -> Self {
        let mut prefix = top.to_vec();
        prefix.push(b'/');
        // Sorted bytewise, the paths under `top/` are one run: found by a
        // binary search, so every directory's subtree together costs one
        // pass over the manifest (O(log N + k) each).
        let from = self
            .entries
            .partition_point(|entry| entry.path.as_slice() < prefix.as_slice());
        Self {
            entries: self.entries[from..]
                .iter()
                .map_while(|entry| {
                    entry
                        .path
                        .strip_prefix(prefix.as_slice())
                        .map(|rest| ManifestEntry {
                            path: rest.to_vec(),
                            digest: entry.digest.clone(),
                            size: entry.size,
                        })
                })
                .collect(),
        }
    }

    /// The distinct digests of the files.
    pub fn object_digests(&self) -> BTreeSet<Digest> {
        self.entries
            .iter()
            .map(|entry| entry.digest.clone())
            .collect()
    }
}

/// `path` with its `.` and `..` components resolved by text alone.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `path` as the kernel resolves it: its longest existing prefix through
/// every link (`canonicalize`), the rest by text. A link loop resolves to
/// nothing and is outside.
fn resolved(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match fs::canonicalize(&existing) {
            Ok(real) => {
                let mut out = real;
                for part in rest.iter().rev() {
                    out.push(part);
                }
                return lexical(&out);
            }
            Err(error) if error.raw_os_error() == Some(libc::ELOOP) => return PathBuf::new(),
            Err(_) => {
                let Some(name) = existing.file_name().map(OsStr::to_os_string) else {
                    // `..` or the root: fall back to the text.
                    return lexical(path);
                };
                rest.push(name);
                if !existing.pop() {
                    return lexical(path);
                }
            }
        }
    }
}

fn walk(
    root: &Path,
    directory: &Path,
    relative: &mut Vec<u8>,
    entries: &mut Vec<ManifestEntry>,
) -> Result<(), CasError> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let kind = entry.file_type()?;
        let path = entry.path();
        let before = relative.len();
        if !relative.is_empty() {
            relative.push(b'/');
        }
        relative.extend_from_slice(name.as_bytes());
        if kind.is_symlink() {
            let target = fs::read_link(&path)?;
            let pointed = directory.join(&target);
            // By text, and as the kernel would resolve it (through the
            // links on the way, `dirlink/../outside` included).
            if !lexical(&pointed).starts_with(root) || !resolved(&pointed).starts_with(root) {
                return Err(CasError::SymlinkEscape {
                    link: PathBuf::from(OsStr::from_bytes(relative)),
                    target,
                });
            }
        } else if kind.is_dir() {
            walk(root, &path, relative, entries)?;
        } else if kind.is_file() {
            let mut file = open_nofollow(&path).map_err(|error| {
                if error.raw_os_error() == Some(libc::ELOOP) {
                    CasError::NotPlain(path.clone())
                } else {
                    error.into()
                }
            })?;
            let meta = file.metadata()?;
            if !meta.is_file() {
                return Err(CasError::NotPlain(path));
            }
            let digest = sha256_reader(&mut file)?;
            entries.push(ManifestEntry {
                path: relative.clone(),
                digest,
                size: Some(meta.len()),
            });
        }
        relative.truncate(before);
    }
    Ok(())
}

/// A new file at `path` (never an existing one, never through a link).
fn create_new(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

/// What [`Store::keep_snapshots`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeepReport {
    /// The snapshots the pin keeps, newest first.
    pub kept: Vec<Digest>,
    /// The snapshots that left the pin.
    pub dropped: Vec<Digest>,
    /// Objects removed: no pin named them and only `data` recorded them.
    pub removed: usize,
}

impl Store {
    /// Store the snapshot `manifest` of `root` in the `data` namespace: its
    /// manifest object, and every file's object unless `digest_only`. Each
    /// file is verified on the way in: one that changed since the manifest
    /// was built is [`CasError::Mismatch`]. An object already there is kept.
    /// Safe to run again (a run killed midway leaves only whole objects).
    /// The snapshot's digest.
    pub fn put_snapshot(
        &self,
        root: &Path,
        manifest: &Manifest,
        digest_only: bool,
    ) -> Result<Digest, CasError> {
        if !digest_only {
            let mut seen = BTreeSet::new();
            for entry in manifest.entries() {
                if !seen.insert(&entry.digest) {
                    continue;
                }
                if self.has(&entry.digest) {
                    self.touch(NS_DATA, &entry.digest)?;
                    continue;
                }
                let path = root.join(OsStr::from_bytes(&entry.path));
                let mut file = open_nofollow(&path)?;
                if !file.metadata()?.is_file() {
                    return Err(CasError::NotPlain(path));
                }
                self.put(NS_DATA, &mut file, &entry.digest)?;
            }
        }
        self.put_bytes(NS_DATA, &manifest.text())
    }

    /// The manifest of the snapshot `snapshot`, read verified from the
    /// store and parsed.
    pub fn snapshot_manifest(&self, snapshot: &Digest) -> Result<Manifest, CasError> {
        Manifest::parse(&self.read(snapshot)?)
    }

    /// The files of `snapshot` its store lacks (none: every byte is here).
    /// Objects are checked for presence, not re-hashed
    /// ([`Store::verify_snapshot`] re-hashes).
    pub fn snapshot_missing(&self, snapshot: &Digest) -> Result<Vec<Digest>, CasError> {
        let manifest = self.snapshot_manifest(snapshot)?;
        Ok(manifest
            .object_digests()
            .into_iter()
            .filter(|digest| !self.has(digest))
            .collect())
    }

    /// Re-hash the snapshot's manifest and every file object.
    pub fn verify_snapshot(&self, snapshot: &Digest) -> Result<Manifest, CasError> {
        let manifest = self.snapshot_manifest(snapshot)?;
        for digest in manifest.object_digests() {
            self.verify(&digest)?;
        }
        Ok(manifest)
    }

    /// The snapshots `pins/prov/<name>` keeps whole (their manifests and
    /// their files' bytes), newest first: at most [`KEEP_SNAPSHOTS`].
    pub fn kept_snapshots(&self, name: &str) -> Result<Vec<Digest>, CasError> {
        Ok(self
            .snapshot_pin(name)?
            .snapshots
            .into_iter()
            .map(|(snapshot, _)| snapshot)
            .collect())
    }

    /// Every manifest `pins/prov/<name>` pins: the manifest of every
    /// snapshot the ref ever kept, pinned forever (D3), sorted.
    pub fn kept_manifests(&self, name: &str) -> Result<Vec<Digest>, CasError> {
        let pin = self.snapshot_pin(name)?;
        let mut manifests: BTreeSet<Digest> = pin.manifests.into_iter().collect();
        manifests.extend(pin.snapshots.into_iter().map(|(snapshot, _)| snapshot));
        Ok(manifests.into_iter().collect())
    }

    /// The pin of `name`: each kept snapshot with every digest pinned for
    /// it, and the manifests section.
    fn snapshot_pin(&self, name: &str) -> Result<SnapshotPin, CasError> {
        check_name("pin name", name)?;
        let path = self.root().join("pins").join(PROV_PIN_OWNER).join(name);
        let mut text = String::new();
        let mut pin = SnapshotPin::default();
        match open_nofollow(&path).and_then(|mut file| file.read_to_string(&mut text)) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(pin),
            Err(error) => return Err(error.into()),
        }
        let mut in_manifests = false;
        for line in text.lines() {
            if line == MANIFESTS_LINE {
                in_manifests = true;
            } else if let Some(snapshot) = line.strip_prefix(SNAPSHOT_LINE) {
                in_manifests = false;
                pin.snapshots
                    .push((Digest::parse(snapshot.trim())?, Vec::new()));
            } else if let Ok(digest) = Digest::parse(line.trim()) {
                if in_manifests {
                    pin.manifests.push(digest);
                } else if let Some((_, digests)) = pin.snapshots.last_mut() {
                    digests.push(digest);
                }
            }
        }
        Ok(pin)
    }

    /// Make `pins/prov/<name>` keep the [`KEEP_SNAPSHOTS`] first of
    /// `snapshots` (newest first; a digest twice counts once) whole: each
    /// one's manifest and, when its manifest is here, its files. Every
    /// manifest the pin ever named, and every one of `snapshots`, stays
    /// pinned forever in its manifests section (D3: a manifest is class A;
    /// only file bytes are capped). The pin is replaced whole under the
    /// pin/GC lock. Then the file objects of the snapshots that left the
    /// newest [`KEEP_SNAPSHOTS`], which no pin names any more and which no
    /// namespace but `data` records, are removed (object first, record
    /// second); their manifests stay. A run killed at any instant leaves
    /// the old pin or the new one, and at worst objects no pin names (a
    /// later run with a `data` quota, [`Store::gc`], collects them).
    pub fn keep_snapshots(&self, name: &str, snapshots: &[Digest]) -> Result<KeepReport, CasError> {
        check_name("pin name", name)?;
        let mut kept: Vec<Digest> = Vec::new();
        for snapshot in snapshots {
            if kept.len() == KEEP_SNAPSHOTS {
                break;
            }
            if !kept.contains(snapshot) {
                kept.push(snapshot.clone());
            }
        }
        let lock = self.lock_gc()?;
        let before = self.snapshot_pin(name)?;
        let mut manifests: BTreeSet<Digest> = before.manifests.iter().cloned().collect();
        manifests.extend(
            before
                .snapshots
                .iter()
                .map(|(snapshot, _)| snapshot.clone()),
        );
        manifests.extend(snapshots.iter().cloned());
        let mut text = format!("# {MANIFEST_V1} snapshots of {name}, newest first\n");
        let mut pinned_now: BTreeSet<Digest> = manifests.clone();
        for snapshot in &kept {
            text.push_str(SNAPSHOT_LINE);
            text.push_str(&snapshot.to_string());
            text.push('\n');
            text.push_str(&snapshot.to_string());
            text.push('\n');
            let objects = match self.snapshot_manifest(snapshot) {
                Ok(manifest) => manifest.object_digests(),
                Err(error) if error.is_not_found() => BTreeSet::new(),
                Err(error) => return Err(error),
            };
            for digest in objects {
                text.push_str(&digest.to_string());
                text.push('\n');
                pinned_now.insert(digest);
            }
        }
        text.push_str(MANIFESTS_LINE);
        text.push('\n');
        for manifest in &manifests {
            text.push_str(&manifest.to_string());
            text.push('\n');
        }
        let directory = self.root().join("pins").join(PROV_PIN_OWNER);
        plain_dir(&directory)?;
        let (mut file, staged) = loop {
            let staged = directory.join(format!(".pin-{}", unique_suffix()));
            match create_new(&staged) {
                Ok(file) => break (file, staged),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        };
        let written = file.write_all(text.as_bytes());
        drop(file);
        if let Err(error) = written.and_then(|()| fs::rename(&staged, directory.join(name))) {
            remove_missing_ok(&staged)?;
            return Err(error.into());
        }
        let mut report = KeepReport {
            kept: kept.clone(),
            ..KeepReport::default()
        };
        // Only file objects are candidates: a manifest is never removed.
        let mut candidates: BTreeSet<Digest> = BTreeSet::new();
        for (snapshot, digests) in before.snapshots {
            if kept.contains(&snapshot) {
                continue;
            }
            report.dropped.push(snapshot.clone());
            candidates.extend(digests.into_iter().filter(|digest| *digest != snapshot));
        }
        if !candidates.is_empty() {
            let pinned = self.pinned()?;
            for digest in candidates {
                if pinned.contains(&digest) || pinned_now.contains(&digest) {
                    continue;
                }
                let namespaces = self.namespaces_of(&digest)?;
                if namespaces.iter().any(|namespace| namespace != NS_DATA) {
                    continue;
                }
                if self.remove_object(&digest)? {
                    report.removed += 1;
                }
            }
        }
        drop(lock);
        Ok(report)
    }
}
