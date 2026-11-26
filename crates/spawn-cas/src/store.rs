//! The store: layout, objects, access records and pins.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use crate::digest::{Digest, Verified, verify_reader};
use crate::{Algo, CasError};

/// The file that marks a directory as a store (the existing `CAS_MARKER`
/// of `spawn-host`'s `cas_dedup`).
pub const CAS_MARKER: &str = ".spawn-cas";

const MARKER_TEXT: &str = "spawn-cas v1\n";

/// The directories of the layout, under the root (`chunk-index/` frozen in D6 Phase 0).
const LAYOUT: [&str; 5] = ["objects", "chunk-index", "pins", "access", "tmp"];

/// The lock that orders [`Store::pin`] against [`Store::gc`]. Its name
/// starts with a dot, which no pin name can (it is never read as a pin).
const GC_LOCK: &str = ".lock";

/// The clock of access records and of temporary-file ages.
pub type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// The system clock.
pub fn system_clock() -> Clock {
    Arc::new(SystemTime::now)
}

/// Whether `name` may be a pin owner, a pin name or a namespace:
/// `[A-Za-z0-9_][A-Za-z0-9._:-]{0,127}`.
pub fn is_valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    let Some((first, rest)) = bytes.split_first() else {
        return false;
    };
    bytes.len() <= 128
        && (first.is_ascii_alphanumeric() || *first == b'_')
        && rest
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(byte))
}

pub(crate) fn check_name(what: &'static str, name: &str) -> Result<(), CasError> {
    if is_valid_name(name) {
        Ok(())
    } else {
        Err(CasError::BadName {
            what,
            name: name.to_string(),
        })
    }
}

/// A name no other call of this process uses: `<pid>-<n>`.
pub(crate) fn unique_suffix() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// A file opened for reading without following a symlink at its name and
/// without blocking on a FIFO.
pub(crate) fn open_nofollow(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
}

/// A new file at `path` (never an existing one, never through a symlink),
/// mode 0600.
fn create_new(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

/// `path` as a plain directory (made 0700 when missing); a symlink or a
/// file there is refused.
pub(crate) fn plain_dir(path: &Path) -> Result<(), CasError> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    if fs::symlink_metadata(path)?.is_dir() {
        Ok(())
    } else {
        Err(CasError::NotPlain(path.to_path_buf()))
    }
}

/// What [`Store::publish`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Publish {
    /// The object was named from the staged file.
    Created,
    /// An object of that digest was already there; it was kept.
    Exists,
}

/// An object found under `objects/` ([`Store::objects`]).
#[derive(Debug, Clone)]
pub struct ObjectEntry {
    /// Its address.
    pub digest: Digest,
    /// Where it is.
    pub path: PathBuf,
    /// Its metadata (not following links: it is a regular file).
    pub meta: fs::Metadata,
}

/// The exclusive side of the pin/GC lock ([`Store::lock_gc`]), released on
/// drop.
#[derive(Debug)]
pub struct GcLock {
    _file: fs::File,
}

/// One spawn-cas root.
pub struct Store {
    root: PathBuf,
    clock: Clock,
    /// Runs in GC right after the pins were read (tests of the pin race).
    #[cfg(test)]
    pub(crate) after_pinned: Option<Box<dyn Fn() + Send + Sync>>,
}

impl fmt::Debug for Store {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Store")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl Store {
    /// The store at `root`: made (marker first) when `root` is missing or
    /// an empty directory, opened when it carries [`CAS_MARKER`], refused
    /// otherwise ([`CasError::NotCas`]). The layout's directories are made
    /// when missing (0700). Safe from several processes at once.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, CasError> {
        let root = root.into();
        claim(&root)?;
        for directory in LAYOUT {
            plain_dir(&root.join(directory))?;
        }
        Ok(Self {
            root,
            clock: system_clock(),
            #[cfg(test)]
            after_pinned: None,
        })
    }

    /// The same store on another clock (tests replace it).
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// The root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Now, by the store's clock.
    pub fn now(&self) -> SystemTime {
        (self.clock)()
    }

    /// `tmp/`: where writes land before they are renamed into place.
    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// Where the object `digest` lives (whether or not it exists).
    pub fn object_path(&self, digest: &Digest) -> PathBuf {
        self.root
            .join("objects")
            .join(digest.algo().name())
            .join(digest.shard())
            .join(digest.hex())
    }

    /// Where the chunk index for a chunked blob `file_digest` lives.
    pub fn chunk_index_path(&self, file_digest: &Digest) -> PathBuf {
        self.root
            .join("chunk-index")
            .join(file_digest.algo().name())
            .join(file_digest.shard())
            .join(file_digest.hex())
    }

    /// The access record of `digest` in namespace `namespace`.
    pub fn access_path(&self, namespace: &str, digest: &Digest) -> PathBuf {
        self.root
            .join("access")
            .join(namespace)
            .join(digest.algo().name())
            .join(digest.hex())
    }

    /// The resumable download of `digest` in `tmp/`.
    pub fn partial_path(&self, digest: &Digest) -> PathBuf {
        self.tmp_dir()
            .join(format!("{}-{}.part", digest.algo(), digest.hex()))
    }

    /// Whether the object exists as a regular file (not that it verifies).
    pub fn has(&self, digest: &Digest) -> bool {
        fs::symlink_metadata(self.object_path(digest)).is_ok_and(|meta| meta.is_file())
    }

    /// Whether the blob `digest` is present either as a single object or as a chunk index.
    pub fn has_blob(&self, digest: &Digest) -> bool {
        self.has(digest)
            || fs::symlink_metadata(self.chunk_index_path(digest)).is_ok_and(|meta| meta.is_file())
    }

    /// Flush buffered object and index writes in this store's filesystem before
    /// publishing root references to PostgreSQL (D6 Phase 0 durability barrier).
    pub fn sync_store(&self) -> Result<(), CasError> {
        let dir = open_nofollow(&self.root)?;
        #[cfg(target_os = "linux")]
        {
            rustix::fs::syncfs(&dir).map_err(io::Error::from)?;
        }
        #[cfg(not(target_os = "linux"))]
        {
            dir.sync_all()?;
        }
        Ok(())
    }

    /// The object, opened for reading: never through a symlink, never a
    /// special file ([`CasError::NotPlain`]); a missing object is an
    /// [`io::ErrorKind::NotFound`] ([`CasError::is_not_found`]).
    pub fn open_object(&self, digest: &Digest) -> Result<fs::File, CasError> {
        let path = self.object_path(digest);
        let file = open_nofollow(&path).map_err(|error| {
            if error.raw_os_error() == Some(libc::ELOOP) {
                CasError::NotPlain(path.clone())
            } else {
                error.into()
            }
        })?;
        if file.metadata()?.is_file() {
            Ok(file)
        } else {
            Err(CasError::NotPlain(path))
        }
    }

    /// Re-hash the stored object ([`CasError::Mismatch`] when it does not
    /// hash to its address). Nothing is removed.
    pub fn verify(&self, digest: &Digest) -> Result<(), CasError> {
        verify_reader(&mut self.open_object(digest)?, digest).map(drop)
    }

    /// The object's bytes, verified. Damaged bytes are refused
    /// ([`CasError::Mismatch`]) and the object removed: the next use is a
    /// miss and fetches it again.
    pub fn read(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        let mut bytes = Vec::new();
        self.open_object(digest)?.read_to_end(&mut bytes)?;
        match verify_reader(&mut bytes.as_slice(), digest) {
            Ok(_) => Ok(bytes),
            Err(error) => {
                if error.is_mismatch() {
                    remove_missing_ok(&self.object_path(digest))?;
                }
                Err(error)
            }
        }
    }

    /// Record a use of `digest` in `namespace`, by the store's clock (not
    /// `atime`).
    pub fn touch(&self, namespace: &str, digest: &Digest) -> Result<(), CasError> {
        check_name("namespace", namespace)?;
        let path = self.access_path(namespace, digest);
        let open = || {
            fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&path)
        };
        let file = match open() {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let namespace_dir = self.root.join("access").join(namespace);
                plain_dir(&namespace_dir)?;
                plain_dir(&namespace_dir.join(digest.algo().name()))?;
                open()?
            }
            Err(error) => return Err(error.into()),
        };
        let now = self.now();
        file.set_times(fs::FileTimes::new().set_accessed(now).set_modified(now))?;
        Ok(())
    }

    /// A new, empty file in `tmp/` named `<prefix>-<pid>-<n>`, mode 0600.
    pub fn temp_file(&self, prefix: &str) -> Result<(fs::File, PathBuf), CasError> {
        loop {
            let path = self.tmp_dir().join(format!("{prefix}-{}", unique_suffix()));
            match create_new(&path) {
                Ok(file) => return Ok((file, path)),
                // A dead process of the same pid left it: another name.
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Verify the file at `staged` against `want` and move it into
    /// `objects/` (mode 0444, `rename`, no fsync), recording a use in
    /// `namespace`. A file that does not hash to `want`, or cannot be
    /// verified, is removed and the error returned.
    pub fn commit(
        &self,
        namespace: &str,
        staged: &Path,
        want: &Digest,
    ) -> Result<PathBuf, CasError> {
        check_name("namespace", namespace)?;
        let verified = (|| {
            let mut file = open_nofollow(staged)?;
            if !file.metadata()?.is_file() {
                return Err(CasError::NotPlain(staged.to_path_buf()));
            }
            verify_reader(&mut file, want)?;
            file.set_permissions(fs::Permissions::from_mode(0o444))?;
            Ok(())
        })();
        if let Err(error) = verified {
            remove_missing_ok(staged)?;
            return Err(error);
        }
        let target = self.object_path(want);
        self.object_dir(want)?;
        fs::rename(staged, &target)?;
        self.touch(namespace, want)?;
        Ok(target)
    }

    /// Store the bytes of `reader` under `want`, verified on the way in,
    /// and record a use in `namespace`. An object already there is kept
    /// (and its use recorded).
    pub fn put(
        &self,
        namespace: &str,
        reader: &mut impl Read,
        want: &Digest,
    ) -> Result<PathBuf, CasError> {
        check_name("namespace", namespace)?;
        if self.has(want) {
            self.touch(namespace, want)?;
            return Ok(self.object_path(want));
        }
        let (mut file, staged) = self.temp_file("put")?;
        let written = io::copy(reader, &mut file).and_then(|_| file.flush());
        drop(file);
        if let Err(error) = written {
            remove_missing_ok(&staged)?;
            return Err(error.into());
        }
        self.commit(namespace, &staged, want)
    }

    /// Store `bytes` under their SHA-256 digest.
    pub fn put_bytes(&self, namespace: &str, bytes: &[u8]) -> Result<Digest, CasError> {
        let digest = crate::sha256_of(bytes);
        self.put(namespace, &mut &bytes[..], &digest)?;
        Ok(digest)
    }

    /// Name the object `verified` from `staged`, the file `file` is open
    /// on, whose bytes the caller verified ([`verify_reader`]) without
    /// copying them again: `staged` is made 0444 and hard-linked as the
    /// object, which never replaces one already there
    /// ([`Publish::Exists`]). `staged` stays; the caller removes it.
    /// No access record is written (the caller's lifecycle owns the object
    /// until it records a use).
    pub fn publish(
        &self,
        staged: &Path,
        file: &fs::File,
        verified: &Verified,
    ) -> Result<Publish, CasError> {
        let opened = file.metadata()?;
        let named = fs::symlink_metadata(staged)?;
        if !opened.is_file() || (opened.dev(), opened.ino()) != (named.dev(), named.ino()) {
            return Err(CasError::NotPlain(staged.to_path_buf()));
        }
        file.set_permissions(fs::Permissions::from_mode(0o444))?;
        let digest = verified.digest();
        self.object_dir(digest)?;
        match fs::hard_link(staged, self.object_path(digest)) {
            Ok(()) => Ok(Publish::Created),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(Publish::Exists),
            Err(error) => Err(error.into()),
        }
    }

    /// Make `objects/<algo>/<2 hex>/` for `digest`.
    pub(crate) fn object_dir(&self, digest: &Digest) -> Result<(), CasError> {
        let algo = self.root.join("objects").join(digest.algo().name());
        plain_dir(&algo)?;
        plain_dir(&algo.join(digest.shard()))
    }

    /// Make `chunk-index/<algo>/<2 hex>/` for `digest`.
    pub(crate) fn chunk_index_dir(&self, digest: &Digest) -> Result<(), CasError> {
        let algo = self.root.join("chunk-index").join(digest.algo().name());
        plain_dir(&algo)?;
        plain_dir(&algo.join(digest.shard()))
    }

    /// Store pre-hashed in-memory `bytes` (`<= 4 MiB` by callers of D6 objects)
    /// into `objects/`, recording `namespace` use.
    pub(crate) fn put_hashed_bytes(
        &self,
        namespace: &str,
        bytes: &[u8],
        digest: &Digest,
    ) -> Result<PathBuf, CasError> {
        check_name("namespace", namespace)?;
        if self.has(digest) {
            self.touch(namespace, digest)?;
            return Ok(self.object_path(digest));
        }
        let (mut file, staged) = self.temp_file("obj")?;
        let write_result = file
            .write_all(bytes)
            .and_then(|()| file.flush())
            .and_then(|()| file.set_permissions(fs::Permissions::from_mode(0o444)));
        drop(file);
        if let Err(error) = write_result {
            remove_missing_ok(&staged)?;
            return Err(error.into());
        }
        let target = self.object_path(digest);
        self.object_dir(digest)?;
        let link_result = fs::hard_link(&staged, &target);
        let _ = remove_missing_ok(&staged);
        match link_result {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        self.touch(namespace, digest)?;
        Ok(target)
    }

    /// Read a file safely from `path` without following symlinks and verifying
    /// it is a regular file.
    pub(crate) fn read_plain_bytes(&self, path: &Path) -> Result<Vec<u8>, CasError> {
        let _ = &self.root;
        let mut file = open_nofollow(path).map_err(|error| {
            if error.raw_os_error() == Some(libc::ELOOP) {
                CasError::NotPlain(path.to_path_buf())
            } else {
                error.into()
            }
        })?;
        if !file.metadata()?.is_file() {
            return Err(CasError::NotPlain(path.to_path_buf()));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    /// The namespaces holding an access record of `digest`, sorted: the
    /// users that store it (`artifacts`, `base-cache`, `backup`, `data`,
    /// ...). An object with none belongs to no namespace's lifecycle.
    pub fn namespaces_of(&self, digest: &Digest) -> Result<Vec<String>, CasError> {
        let access = self.root.join("access");
        let namespaces = match fs::read_dir(&access) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut found = Vec::new();
        for namespace in namespaces {
            let namespace = namespace?;
            if !namespace.file_type()?.is_dir() {
                continue;
            }
            let record = namespace
                .path()
                .join(digest.algo().name())
                .join(digest.hex());
            match fs::symlink_metadata(&record) {
                Ok(_) => found.push(namespace.file_name().to_string_lossy().into_owned()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        found.sort();
        Ok(found)
    }

    /// Remove only the object file, never an access record (for an object
    /// no namespace records). Whether it was there.
    pub fn remove_object_file(&self, digest: &Digest) -> Result<bool, CasError> {
        Ok(remove_missing_ok(&self.object_path(digest))?)
    }

    /// Remove the object, then its access record in every namespace
    /// (crash-only: a record left behind names nothing and goes in the next
    /// GC). Whether the object was there.
    pub fn remove_object(&self, digest: &Digest) -> Result<bool, CasError> {
        let removed = remove_missing_ok(&self.object_path(digest))?;
        let access = self.root.join("access");
        let namespaces = match fs::read_dir(&access) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(removed),
            Err(error) => return Err(error.into()),
        };
        for namespace in namespaces {
            let namespace = namespace?;
            if namespace.file_type()?.is_dir() {
                remove_missing_ok(
                    &namespace
                        .path()
                        .join(digest.algo().name())
                        .join(digest.hex()),
                )?;
            }
        }
        Ok(removed)
    }

    /// Every object of `algo`: only regular files named `<64 hex>` in their
    /// own `<2 hex>` shard (anything else under `objects/` is left alone).
    pub fn objects(&self, algo: Algo) -> Result<Vec<ObjectEntry>, CasError> {
        let directory = self.root.join("objects").join(algo.name());
        let shards = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut found = Vec::new();
        for shard in shards {
            let shard = shard?;
            let shard_name = shard.file_name().to_string_lossy().into_owned();
            if !is_shard(&shard_name) || !shard.file_type()?.is_dir() {
                continue;
            }
            for object in fs::read_dir(shard.path())? {
                let object = object?;
                let name = object.file_name().to_string_lossy().into_owned();
                if !crate::is_hex64(&name) || !name.starts_with(&shard_name) {
                    continue;
                }
                let path = object.path();
                let meta = match fs::symlink_metadata(&path) {
                    Ok(meta) => meta,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                if meta.is_file() {
                    found.push(ObjectEntry {
                        digest: Digest::from_parts(algo, &name)?,
                        path,
                        meta,
                    });
                }
            }
        }
        found.sort_by(|left, right| left.digest.cmp(&right.digest));
        Ok(found)
    }

    /// The exclusive side of the pin/GC lock: held by [`Store::pin`] and
    /// [`Store::gc`], and by any other caller that deletes objects by what
    /// is pinned (read the pins after taking it).
    pub fn lock_gc(&self) -> Result<GcLock, CasError> {
        let pins = self.root.join("pins");
        plain_dir(&pins)?;
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(pins.join(GC_LOCK))?;
        file.lock()?;
        Ok(GcLock { _file: file })
    }

    /// Write `pins/<owner>/<name>`: one digest per line, replaced whole
    /// (temporary file and `rename`). It waits for a GC pass that is
    /// running, and never interleaves with one.
    pub fn pin(&self, owner: &str, name: &str, digests: &[Digest]) -> Result<(), CasError> {
        check_name("pin owner", owner)?;
        check_name("pin name", name)?;
        let text = digests.iter().fold(String::new(), |mut text, digest| {
            text.push_str(&digest.to_string());
            text.push('\n');
            text
        });
        let _lock = self.lock_gc()?;
        let directory = self.root.join("pins").join(owner);
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
        let placed = written.and_then(|()| fs::rename(&staged, directory.join(name)));
        if let Err(error) = placed {
            remove_missing_ok(&staged)?;
            return Err(error.into());
        }
        Ok(())
    }

    /// Remove `pins/<owner>/<name>`; a missing pin is no error.
    pub fn unpin(&self, owner: &str, name: &str) -> Result<(), CasError> {
        check_name("pin owner", owner)?;
        check_name("pin name", name)?;
        remove_missing_ok(&self.root.join("pins").join(owner).join(name))?;
        Ok(())
    }

    /// The pin names of `owner`, sorted.
    pub fn pin_names(&self, owner: &str) -> Result<Vec<String>, CasError> {
        check_name("pin owner", owner)?;
        let entries = match fs::read_dir(self.root.join("pins").join(owner)) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let mut names = Vec::new();
        for entry in entries {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if !name.starts_with('.') {
                names.push(name);
            }
        }
        names.sort();
        Ok(names)
    }

    /// Every digest named by any pin file under `pins/`. A line that does
    /// not parse is ignored (a torn pin file pins less than it says, never
    /// more); a pin file that cannot be read is an error, so a pass never
    /// evicts on a guess.
    pub fn pinned(&self) -> Result<BTreeSet<Digest>, CasError> {
        let mut pinned = BTreeSet::new();
        collect_pins(&self.root.join("pins"), &mut pinned)?;
        Ok(pinned)
    }
}

/// Remove the file at `path`; whether it was there.
pub(crate) fn remove_missing_ok(path: &Path) -> io::Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Whether `name` is a shard: two lowercase hex digits.
pub(crate) fn is_shard(name: &str) -> bool {
    name.len() == 2
        && name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn collect_pins(directory: &Path, pinned: &mut BTreeSet<Digest>) -> Result<(), CasError> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let kind = entry.file_type()?;
        let name = entry.file_name();
        if kind.is_dir() {
            collect_pins(&entry.path(), pinned)?;
            continue;
        }
        if !kind.is_file() || name.to_string_lossy().starts_with('.') {
            continue;
        }
        let mut text = String::new();
        match open_nofollow(&entry.path()).and_then(|mut file| file.read_to_string(&mut text)) {
            Ok(_) => {}
            // Unpinned meanwhile.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            // Not UTF-8: no line of it is a digest.
            Err(error) if error.kind() == io::ErrorKind::InvalidData => continue,
            Err(error) => return Err(error.into()),
        }
        pinned.extend(
            text.lines()
                .filter_map(|line| Digest::parse(line.trim()).ok()),
        );
    }
    Ok(())
}

/// Make `root` a store (marker included), or refuse a non-empty directory
/// that is not one.
fn claim(root: &Path) -> Result<(), CasError> {
    let marker = root.join(CAS_MARKER);
    let claimed = || fs::symlink_metadata(&marker).is_ok_and(|meta| meta.is_file());
    if claimed() {
        return Ok(());
    }
    match fs::symlink_metadata(root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            // Built beside its name with the marker inside, then renamed
            // into place: a store is never seen without its marker.
            let parent = root.parent().unwrap_or(Path::new("."));
            fs::create_dir_all(parent)?;
            let name = root.file_name().unwrap_or_default().to_string_lossy();
            let staged = parent.join(format!(".{name}.claim-{}", unique_suffix()));
            fs::DirBuilder::new().mode(0o700).create(&staged)?;
            fs::write(staged.join(CAS_MARKER), MARKER_TEXT)?;
            if fs::rename(&staged, root).is_err() {
                // Another opener made it first.
                fs::remove_file(staged.join(CAS_MARKER))?;
                fs::remove_dir(&staged)?;
            }
        }
        Err(error) => return Err(error.into()),
        Ok(meta) if meta.is_dir() && fs::read_dir(root)?.next().is_none() => {
            // An empty directory: the marker is the first thing in it (an
            // opener that loses the race finds the winner's).
            match create_new(&marker) {
                Ok(mut file) => file.write_all(MARKER_TEXT.as_bytes())?,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(_) => {}
    }
    if claimed() {
        Ok(())
    } else {
        Err(CasError::NotCas(root.to_path_buf()))
    }
}
