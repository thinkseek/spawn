//! Canonical Merkle `Tree` (`spawn-tree-v1`), `Snapshot` (`spawn-snapshot-v1`),
//! and in-memory persistent `O(|diff| + depth)` tree editor (`D6 Phase 0`).

use std::collections::{BTreeMap, BTreeSet};

use crate::chunker::CHUNK_MAX;
use crate::{CasError, Digest, NS_TREE, Store, sha256_of};

/// Magic header for `spawn-tree-v1` canonical tree objects.
pub const TREE_MAGIC: &[u8] = b"spawn-tree-v1\n";

/// Magic header for `spawn-snapshot-v1` canonical commit-envelope objects.
pub const SNAPSHOT_MAGIC: &[u8] = b"spawn-snapshot-v1\n";

const GIT_OID_SHA1_LEN: u64 = 20;
const GIT_OID_SHA256_LEN: u64 = 32;

/// Entry kind inside a canonical `spawn-tree-v1` node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TreeEntryKind {
    /// Regular non-executable file (`f`, Git mode `100644`).
    File,
    /// Regular executable file (`x`, Git mode `100755`).
    Executable,
    /// Symbolic link (`l`, Git mode `120000`, `id = sha256(target_bytes)`).
    Symlink,
    /// Non-empty subdirectory (`d`, Git mode `40000`, `id = child tree sha256`, `size = 0`).
    Dir,
    /// Submodule gitlink (`g`, Git mode `160000`, left-aligned zero-padded commit OID, `size = oid len`).
    Gitlink,
}

impl TreeEntryKind {
    /// Wire byte code in `spawn-tree-v1`.
    pub const fn code(self) -> u8 {
        match self {
            Self::File => b'f',
            Self::Executable => b'x',
            Self::Symlink => b'l',
            Self::Dir => b'd',
            Self::Gitlink => b'g',
        }
    }

    /// Decode from wire byte code.
    pub const fn from_code(byte: u8) -> Option<Self> {
        match byte {
            b'f' => Some(Self::File),
            b'x' => Some(Self::Executable),
            b'l' => Some(Self::Symlink),
            b'd' => Some(Self::Dir),
            b'g' => Some(Self::Gitlink),
            _ => None,
        }
    }
}

/// One entry in a canonical `spawn-tree-v1` object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// Kind tag (`f`, `x`, `l`, `d`, `g`).
    pub kind: TreeEntryKind,
    /// Single path component (no `/`, NUL, `.`, `..`, or case-insensitive `.git`).
    pub name: Vec<u8>,
    /// Content SHA-256 bytes (or zero-padded commit OID for `Gitlink`).
    pub id: [u8; 32],
    /// Byte size (`0` for `Dir`, OID byte length `20`/`32` for `Gitlink`).
    pub size: u64,
}

impl TreeEntry {
    /// Construct a regular file entry (`100644`).
    pub fn file(name: impl Into<Vec<u8>>, digest: &Digest, size: u64) -> Self {
        Self {
            kind: TreeEntryKind::File,
            name: name.into(),
            id: digest.as_bytes(),
            size,
        }
    }

    /// Construct an executable file entry (`100755`).
    pub fn executable(name: impl Into<Vec<u8>>, digest: &Digest, size: u64) -> Self {
        Self {
            kind: TreeEntryKind::Executable,
            name: name.into(),
            id: digest.as_bytes(),
            size,
        }
    }

    /// Construct a symlink entry (`120000`, `size` = target bytes length).
    pub fn symlink(name: impl Into<Vec<u8>>, target_digest: &Digest, target_len: u64) -> Self {
        Self {
            kind: TreeEntryKind::Symlink,
            name: name.into(),
            id: target_digest.as_bytes(),
            size: target_len,
        }
    }

    /// Construct a non-empty directory entry (`40000`, `size = 0`).
    pub fn dir(name: impl Into<Vec<u8>>, tree_digest: &Digest) -> Self {
        Self {
            kind: TreeEntryKind::Dir,
            name: name.into(),
            id: tree_digest.as_bytes(),
            size: 0,
        }
    }

    /// Construct a gitlink submodule entry (`160000`), accepting 20-byte (SHA-1)
    /// or 32-byte (SHA-256) raw commit OIDs.
    pub fn gitlink(name: impl Into<Vec<u8>>, oid: &[u8]) -> Result<Self, CasError> {
        if oid.len() != 20 && oid.len() != 32 {
            return Err(CasError::BadFormat("gitlink oid must be 20 or 32 bytes"));
        }
        let mut id = [0_u8; 32];
        id[..oid.len()].copy_from_slice(oid);
        Ok(Self {
            kind: TreeEntryKind::Gitlink,
            name: name.into(),
            id,
            size: u64::try_from(oid.len()).expect("20 or 32 fits in u64"),
        })
    }

    /// Return the SHA-256 CAS digest if this entry is `File`, `Executable`, `Symlink`, or `Dir`.
    pub fn digest(&self) -> Option<Digest> {
        match self.kind {
            TreeEntryKind::Gitlink => None,
            TreeEntryKind::File
            | TreeEntryKind::Executable
            | TreeEntryKind::Symlink
            | TreeEntryKind::Dir => Some(Digest::from_raw_sha256(self.id)),
        }
    }

    /// Return raw commit OID slice if this entry is a `Gitlink`.
    pub fn gitlink_oid(&self) -> Option<&[u8]> {
        if self.kind != TreeEntryKind::Gitlink {
            return None;
        }
        let len = usize::try_from(self.size).ok()?;
        self.id.get(..len)
    }
}

/// Validate a single `spawn-tree-v1` entry component name.
pub fn is_valid_entry_name(name: &[u8]) -> bool {
    if name.is_empty() || name.len() > usize::from(u16::MAX) {
        return false;
    }
    if name.contains(&b'/') || name.contains(&0) {
        return false;
    }
    if name == b"." || name == b".." {
        return false;
    }
    let trimmed_len = name
        .iter()
        .rposition(|&b| b != b'.' && b != b' ')
        .map_or(0, |last| last + 1);
    if name[..trimmed_len].eq_ignore_ascii_case(b".git") {
        return false;
    }
    true
}

/// SHA-256 digest of the canonical empty `spawn-tree-v1` node (`count = 0`).
pub fn empty_tree_digest() -> Digest {
    static EMPTY: std::sync::OnceLock<Digest> = std::sync::OnceLock::new();
    EMPTY
        .get_or_init(|| {
            let mut bytes = Vec::with_capacity(TREE_MAGIC.len() + 4);
            bytes.extend_from_slice(TREE_MAGIC);
            bytes.extend_from_slice(&0_u32.to_be_bytes());
            sha256_of(&bytes)
        })
        .clone()
}

fn validate_entry(entry: &TreeEntry, empty_tree_raw: &[u8; 32]) -> Result<(), CasError> {
    if !is_valid_entry_name(&entry.name) {
        return Err(CasError::BadFormat("invalid tree entry name"));
    }
    match entry.kind {
        TreeEntryKind::Dir => {
            if entry.size != 0 {
                return Err(CasError::BadFormat("directory tree entry size must be 0"));
            }
            if &entry.id == empty_tree_raw {
                return Err(CasError::BadFormat("empty subdirectory entry is forbidden"));
            }
        }
        TreeEntryKind::Gitlink => {
            if entry.size != GIT_OID_SHA1_LEN && entry.size != GIT_OID_SHA256_LEN {
                return Err(CasError::BadFormat("gitlink size must be 20 or 32 bytes"));
            }
            let oid_len = usize::try_from(entry.size).expect("20 or 32 fits in usize");
            if entry.id[oid_len..].iter().any(|&b| b != 0) {
                return Err(CasError::BadFormat("gitlink zero padding corrupted"));
            }
        }
        TreeEntryKind::File | TreeEntryKind::Executable | TreeEntryKind::Symlink => {}
    }
    Ok(())
}

/// Canonical `spawn-tree-v1` directory node.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tree {
    /// Sorted unique directory entries (`name` bytewise ascending).
    pub entries: Vec<TreeEntry>,
}

impl Tree {
    /// Validate entries, sort ascending by `name`, and reject duplicates.
    pub fn from_entries(mut entries: Vec<TreeEntry>) -> Result<Self, CasError> {
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        let empty = empty_tree_digest().as_bytes();
        for (index, entry) in entries.iter().enumerate() {
            validate_entry(entry, &empty)?;
            if index > 0 && entries[index - 1].name == entry.name {
                return Err(CasError::BadFormat("duplicate tree entry name"));
            }
        }
        Ok(Self { entries })
    }

    /// Encode this tree in canonical `spawn-tree-v1` byte order.
    pub fn encode(&self) -> Result<Vec<u8>, CasError> {
        let count = u32::try_from(self.entries.len())
            .map_err(|_| CasError::BadFormat("too many entries"))?;
        let empty = empty_tree_digest().as_bytes();
        let mut total = TREE_MAGIC.len() + 4;
        for (index, entry) in self.entries.iter().enumerate() {
            validate_entry(entry, &empty)?;
            if index > 0 && self.entries[index - 1].name >= entry.name {
                return Err(CasError::BadFormat("tree entries must be strictly sorted"));
            }
            total = total
                .checked_add(1 + 2 + entry.name.len() + 32 + 8)
                .ok_or(CasError::BadFormat("tree size overflow"))?;
        }
        if total > CHUNK_MAX {
            return Err(CasError::TooLarge {
                kind: "tree",
                size: total,
                max: CHUNK_MAX,
            });
        }

        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(TREE_MAGIC);
        out.extend_from_slice(&count.to_be_bytes());
        for entry in &self.entries {
            out.push(entry.kind.code());
            let name_len = u16::try_from(entry.name.len())
                .map_err(|_| CasError::BadFormat("tree entry name too long"))?;
            out.extend_from_slice(&name_len.to_be_bytes());
            out.extend_from_slice(&entry.name);
            out.extend_from_slice(&entry.id);
            out.extend_from_slice(&entry.size.to_be_bytes());
        }
        Ok(out)
    }

    /// Strictly decode canonical `spawn-tree-v1` bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, CasError> {
        if bytes.len() < TREE_MAGIC.len() + 4 || &bytes[..TREE_MAGIC.len()] != TREE_MAGIC {
            return Err(CasError::BadFormat("missing spawn-tree-v1 header"));
        }
        let mut pos = TREE_MAGIC.len();
        let count = usize::try_from(u32::from_be_bytes(
            bytes[pos..pos + 4]
                .try_into()
                .map_err(|_| CasError::BadFormat("truncated tree entry count"))?,
        ))
        .map_err(|_| CasError::BadFormat("tree entry count overflow"))?;
        pos += 4;
        let empty = empty_tree_digest().as_bytes();
        let mut entries: Vec<TreeEntry> = Vec::with_capacity(count.min(4096));
        for index in 0..count {
            let kind_byte = *bytes
                .get(pos)
                .ok_or(CasError::BadFormat("truncated tree entry kind"))?;
            pos += 1;
            let kind = TreeEntryKind::from_code(kind_byte)
                .ok_or(CasError::BadFormat("unknown tree entry kind"))?;
            let name_len = usize::from(u16::from_be_bytes(
                bytes
                    .get(pos..pos + 2)
                    .ok_or(CasError::BadFormat("truncated entry name length"))?
                    .try_into()
                    .map_err(|_| CasError::BadFormat("truncated entry name length"))?,
            ));
            pos += 2;
            let name = bytes
                .get(pos..pos + name_len)
                .ok_or(CasError::BadFormat("truncated entry name"))?
                .to_vec();
            pos += name_len;
            let id: [u8; 32] = bytes
                .get(pos..pos + 32)
                .ok_or(CasError::BadFormat("truncated entry id"))?
                .try_into()
                .map_err(|_| CasError::BadFormat("truncated entry id"))?;
            pos += 32;
            let size = u64::from_be_bytes(
                bytes
                    .get(pos..pos + 8)
                    .ok_or(CasError::BadFormat("truncated entry size"))?
                    .try_into()
                    .map_err(|_| CasError::BadFormat("truncated entry size"))?,
            );
            pos += 8;
            let entry = TreeEntry {
                kind,
                name,
                id,
                size,
            };
            validate_entry(&entry, &empty)?;
            if index > 0 && entries[index - 1].name >= entry.name {
                return Err(CasError::BadFormat(
                    "tree entries are unsorted or duplicate",
                ));
            }
            entries.push(entry);
        }
        if pos != bytes.len() {
            return Err(CasError::BadFormat("trailing bytes in tree object"));
        }
        Ok(Self { entries })
    }
}

/// Canonical `spawn-snapshot-v1` commit envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Root `spawn-tree-v1` SHA-256 digest.
    pub tree: Digest,
    /// Parent snapshot digests in Git first-parent order.
    pub parents: Vec<Digest>,
    /// Verbatim raw Git commit body (everything after `tree`/`parent` header lines,
    /// preserving `author`, `committer`, `gpgsig`, custom headers, and message).
    pub commit_body: Vec<u8>,
}

fn parse_strict_decimal(bytes: &[u8]) -> Option<usize> {
    if bytes.is_empty() {
        return None;
    }
    if bytes == b"0" {
        return Some(0);
    }
    if !matches!(bytes[0], b'1'..=b'9') || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse::<usize>().ok()
}

fn read_line<'a>(bytes: &'a [u8], pos: &mut usize) -> Result<&'a [u8], CasError> {
    let rest = bytes.get(*pos..).unwrap_or(&[]);
    let rel_end = rest
        .iter()
        .position(|&b| b == b'\n')
        .ok_or(CasError::BadFormat("unterminated snapshot header line"))?;
    let line = &rest[..rel_end];
    *pos += rel_end + 1;
    Ok(line)
}

impl Snapshot {
    /// Encode into canonical `spawn-snapshot-v1` bytes.
    pub fn encode(&self) -> Result<Vec<u8>, CasError> {
        let mut header = String::with_capacity(96 + self.parents.len() * 80);
        header.push_str("spawn-snapshot-v1\n");
        header.push_str("tree ");
        header.push_str(&self.tree.to_string());
        header.push('\n');
        for parent in &self.parents {
            header.push_str("parent ");
            header.push_str(&parent.to_string());
            header.push('\n');
        }
        header.push_str("commit ");
        header.push_str(&self.commit_body.len().to_string());
        header.push('\n');

        let total = header.len().saturating_add(self.commit_body.len());
        if total > CHUNK_MAX {
            return Err(CasError::TooLarge {
                kind: "snapshot",
                size: total,
                max: CHUNK_MAX,
            });
        }
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(&self.commit_body);
        Ok(out)
    }

    /// Decode and strictly validate `spawn-snapshot-v1` bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, CasError> {
        if !bytes.starts_with(SNAPSHOT_MAGIC) {
            return Err(CasError::BadFormat("missing spawn-snapshot-v1 header"));
        }
        let mut pos = SNAPSHOT_MAGIC.len();
        let tree_line = read_line(bytes, &mut pos)?;
        let tree_raw = tree_line
            .strip_prefix(b"tree ")
            .ok_or(CasError::BadFormat("expected tree line in snapshot"))?;
        let tree_str =
            std::str::from_utf8(tree_raw).map_err(|_| CasError::BadFormat("non-utf8 tree line"))?;
        let tree = Digest::parse(tree_str)?;

        let mut parents = Vec::new();
        let commit_len: usize;
        loop {
            let line = read_line(bytes, &mut pos)?;
            if let Some(parent_raw) = line.strip_prefix(b"parent ") {
                let parent_str = std::str::from_utf8(parent_raw)
                    .map_err(|_| CasError::BadFormat("non-utf8 parent line"))?;
                parents.push(Digest::parse(parent_str)?);
            } else if let Some(len_raw) = line.strip_prefix(b"commit ") {
                commit_len = parse_strict_decimal(len_raw)
                    .ok_or(CasError::BadFormat("invalid commit byte length"))?;
                break;
            } else {
                return Err(CasError::BadFormat("unexpected snapshot header line"));
            }
        }
        if bytes.len().saturating_sub(pos) != commit_len {
            return Err(CasError::BadFormat("snapshot commit body length mismatch"));
        }
        Ok(Self {
            tree,
            parents,
            commit_body: bytes[pos..].to_vec(),
        })
    }
}

/// Leaf mutation fed to [`Store::apply_tree_mutations`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeMutation {
    /// Insert or replace a non-directory entry at `path` (creates parent dirs).
    Upsert {
        /// Non-empty path segments relative to the tree root.
        path: Vec<Vec<u8>>,
        /// Must be `File`, `Executable`, `Symlink`, or `Gitlink` (never `Dir`).
        kind: TreeEntryKind,
        /// Blob digest bytes (or padded gitlink OID).
        id: [u8; 32],
        /// Entry size (`oid.len()` for gitlink).
        size: u64,
    },
    /// Remove an entry or subtree at `path` (automatically prunes emptied parents).
    Remove {
        /// Non-empty path segments relative to the tree root.
        path: Vec<Vec<u8>>,
    },
}

type TreePayload = (TreeEntryKind, [u8; 32], u64);
type MutationOp<'a> = (&'a [Vec<u8>], Option<TreePayload>);

enum MutationView<'a> {
    Upsert {
        kind: TreeEntryKind,
        id: [u8; 32],
        size: u64,
    },
    Remove,
    Nested(&'a [Vec<u8>], Option<TreePayload>),
}

fn validate_mutations(mutations: &[TreeMutation]) -> Result<(), CasError> {
    let mut leaf_upserts: BTreeSet<&[Vec<u8>]> = BTreeSet::new();
    let mut exact_paths: BTreeSet<(&[Vec<u8>], bool)> = BTreeSet::new();
    for mutation in mutations {
        let (path, is_upsert) = match mutation {
            TreeMutation::Upsert {
                path,
                kind,
                id,
                size,
            } => {
                if *kind == TreeEntryKind::Dir {
                    return Err(CasError::BadFormat("Upsert leaf kind cannot be Dir"));
                }
                let dummy = TreeEntry {
                    kind: *kind,
                    name: b"ok".to_vec(),
                    id: *id,
                    size: *size,
                };
                validate_entry(&dummy, &[0_u8; 32])?;
                (path.as_slice(), true)
            }
            TreeMutation::Remove { path } => (path.as_slice(), false),
        };
        if path.is_empty() || path.iter().any(|seg| !is_valid_entry_name(seg)) {
            return Err(CasError::BadFormat("invalid mutation path segment"));
        }
        if !exact_paths.insert((path, is_upsert)) {
            return Err(CasError::BadFormat("duplicate mutation for same path"));
        }
        if is_upsert {
            leaf_upserts.insert(path);
        }
    }
    for mutation in mutations {
        let (path, is_upsert) = match mutation {
            TreeMutation::Upsert { path, .. } => (path.as_slice(), true),
            TreeMutation::Remove { path } => (path.as_slice(), false),
        };
        for cut in 1..path.len() {
            if leaf_upserts.contains(&path[..cut]) {
                return Err(CasError::BadFormat("conflicting file and child mutations"));
            }
        }
        if !is_upsert && leaf_upserts.contains(path) {
            return Err(CasError::BadFormat(
                "conflicting upsert and remove for exact path",
            ));
        }
    }
    Ok(())
}

impl Store {
    /// Store a canonical `spawn-tree-v1` node in [`NS_TREE`], returning its SHA-256 digest.
    pub fn put_tree(&self, tree: &Tree) -> Result<Digest, CasError> {
        let bytes = tree.encode()?;
        let digest = sha256_of(&bytes);
        self.put_hashed_bytes(NS_TREE, &bytes, &digest)?;
        Ok(digest)
    }

    /// Read, verify, and decode a canonical `spawn-tree-v1` node.
    pub fn read_tree(&self, digest: &Digest) -> Result<Tree, CasError> {
        let bytes = self.read(digest)?;
        Tree::decode(&bytes)
    }

    /// Store a canonical `spawn-snapshot-v1` commit envelope in [`NS_TREE`],
    /// returning its SHA-256 digest.
    pub fn put_merkle_snapshot(&self, snapshot: &Snapshot) -> Result<Digest, CasError> {
        let bytes = snapshot.encode()?;
        let digest = sha256_of(&bytes);
        self.put_hashed_bytes(NS_TREE, &bytes, &digest)?;
        Ok(digest)
    }

    /// Read, verify, and decode a canonical `spawn-snapshot-v1` commit envelope.
    pub fn read_merkle_snapshot(&self, digest: &Digest) -> Result<Snapshot, CasError> {
        let bytes = self.read(digest)?;
        Snapshot::decode(&bytes)
    }

    /// Ensure the canonical empty tree node exists in [`NS_TREE`] and return its digest.
    pub fn put_empty_tree(&self) -> Result<Digest, CasError> {
        self.put_tree(&Tree::default())
    }

    /// Apply a batch of path-level mutations to `base_tree` in `O(|diff| + depth)`,
    /// only reading and writing trees along modified paths.
    pub fn apply_tree_mutations(
        &self,
        base_tree: &Digest,
        mutations: &[TreeMutation],
    ) -> Result<Digest, CasError> {
        validate_mutations(mutations)?;
        if mutations.is_empty() {
            if *base_tree == empty_tree_digest() && !self.has(base_tree) {
                return self.put_empty_tree();
            }
            return Ok(base_tree.clone());
        }
        let empty_digest = empty_tree_digest();
        let initial = if *base_tree == empty_digest {
            None
        } else {
            Some(base_tree.clone())
        };
        let items: Vec<MutationOp<'_>> = mutations
            .iter()
            .map(|m| match m {
                TreeMutation::Upsert {
                    path,
                    kind,
                    id,
                    size,
                } => (path.as_slice(), Some((*kind, *id, *size))),
                TreeMutation::Remove { path } => (path.as_slice(), None),
            })
            .collect();
        match self.mutate_subtree(initial.as_ref(), &items)? {
            Some(digest) => Ok(digest),
            None => self.put_empty_tree(),
        }
    }

    fn mutate_subtree(
        &self,
        current_digest: Option<&Digest>,
        ops: &[MutationOp<'_>],
    ) -> Result<Option<Digest>, CasError> {
        let mut entries: BTreeMap<Vec<u8>, TreeEntry> = match current_digest {
            Some(digest) => self
                .read_tree(digest)?
                .entries
                .into_iter()
                .map(|entry| (entry.name.clone(), entry))
                .collect(),
            None => BTreeMap::new(),
        };

        let mut grouped: BTreeMap<&[u8], Vec<MutationView<'_>>> = BTreeMap::new();
        for &(path, payload) in ops {
            let head = path[0].as_slice();
            let entry = grouped.entry(head).or_default();
            if path.len() == 1 {
                match payload {
                    Some((kind, id, size)) => entry.push(MutationView::Upsert { kind, id, size }),
                    None => entry.push(MutationView::Remove),
                }
            } else {
                entry.push(MutationView::Nested(&path[1..], payload));
            }
        }

        for (name, group) in grouped {
            let mut has_leaf_remove = false;
            let mut leaf_upsert: Option<TreePayload> = None;
            let mut child_ops: Vec<MutationOp<'_>> = Vec::new();
            for item in group {
                match item {
                    MutationView::Remove => has_leaf_remove = true,
                    MutationView::Upsert { kind, id, size } => {
                        leaf_upsert = Some((kind, id, size));
                    }
                    MutationView::Nested(rest, payload) => child_ops.push((rest, payload)),
                }
            }

            if has_leaf_remove {
                entries.remove(name);
            }
            if !child_ops.is_empty() {
                let child_base = match entries.get(name) {
                    Some(existing) if existing.kind == TreeEntryKind::Dir => {
                        Some(Digest::from_raw_sha256(existing.id))
                    }
                    Some(_) => {
                        // A non-Dir entry cannot absorb nested inserts unless explicitly removed,
                        // while nested removes under a missing/non-dir path are safe no-ops.
                        let has_insert = child_ops.iter().any(|(_, payload)| payload.is_some());
                        if has_insert {
                            return Err(CasError::BadFormat(
                                "cannot descend into non-directory entry without removing it",
                            ));
                        }
                        None
                    }
                    None => None,
                };
                if child_base.is_some() || child_ops.iter().any(|(_, payload)| payload.is_some()) {
                    match self.mutate_subtree(child_base.as_ref(), &child_ops)? {
                        Some(new_child) => {
                            entries.insert(name.to_vec(), TreeEntry::dir(name, &new_child));
                        }
                        None => {
                            entries.remove(name);
                        }
                    }
                }
            }
            if let Some((kind, id, size)) = leaf_upsert {
                entries.insert(
                    name.to_vec(),
                    TreeEntry {
                        kind,
                        name: name.to_vec(),
                        id,
                        size,
                    },
                );
            }
        }

        if entries.is_empty() {
            return Ok(None);
        }
        let tree = Tree {
            entries: entries.into_values().collect(),
        };
        let digest = self.put_tree(&tree)?;
        Ok(Some(digest))
    }
}
