//! The workspace store of the machine Spawn runs on: one type over the two
//! workspace backends ([`GitWorkspace`] by default and [`D6Workspace`] for
//! opt-in / cutover CAS Merkle workspaces), told apart only here, as
//! [`crate::LocalTerminalHost`] tells the terminal hosts apart.
//!
//! Every backend obeys the same [`WorkspaceStore`] contract (the branch lock
//! under `mount_when`, `unmount_when`, `hold` and `set_aside_when`, the
//! `is_mounted` the Bridge's recovery asks). A backend is chosen in one place
//! ([`WorkspaceBackend`]), and a root runs one backend only ([`claim_root`]).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::path::Path;
use std::sync::Arc;

use spawn_core::id::{BranchName, CommitId, WorkspaceId};
use spawn_core::port::{BaseMerge, Branch, Commit, Hold, SetAside, WorkspaceError, WorkspaceStore};

use crate::cas_merkle::{CasMerkleStore, RefLedger};
use crate::d6_workspace::{D6Workspace, L1TmpfsPolicy};
use crate::git::GitWorkspace;

/// Type-erased thread-safe D6 ledger handle stored inside [`LocalWorkspaceStore`].
pub type DynRefLedger = Arc<dyn RefLedger + Send + Sync>;

/// Construction options for [`LocalWorkspaceStore::d6`].
#[derive(Clone)]
pub struct D6StoreOptions {
    /// Content-addressed Merkle object store and journal root.
    pub cas: Arc<CasMerkleStore>,
    /// Transactional ref and snapshot ledger (`spawn_refs` / `spawn_snapshots`).
    pub ledger: DynRefLedger,
    /// Optional bounded L1 `tmpfs` upperdir policy.
    pub l1_policy: Option<L1TmpfsPolicy>,
    /// Per-file checkpoint byte limit.
    pub max_file_bytes: u64,
}

impl D6StoreOptions {
    /// Create D6 store options with CAS, ledger, and checkpoint file cap (`l1_policy = None`).
    pub fn new(cas: Arc<CasMerkleStore>, ledger: DynRefLedger, max_file_bytes: u64) -> Self {
        Self {
            cas,
            ledger,
            l1_policy: None,
            max_file_bytes,
        }
    }

    /// Enable a bounded L1 `tmpfs` upperdir policy.
    #[must_use]
    pub fn with_l1_policy(mut self, policy: L1TmpfsPolicy) -> Self {
        self.l1_policy = Some(policy);
        self
    }
}

impl std::fmt::Debug for D6StoreOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("D6StoreOptions")
            .field("cas_root", &self.cas.cas().root())
            .field("l1_policy", &self.l1_policy)
            .field("max_file_bytes", &self.max_file_bytes)
            .finish_non_exhaustive()
    }
}

/// The file under a workspace root that names the one backend its
/// workspaces run on ([`claim_root`]).
pub const BACKEND_MARKER: &str = ".spawn-workspace-backend";

/// The two backends a [`LocalWorkspaceStore`] can be: `git` (default)
/// or opt-in pure-Rust `d6`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum WorkspaceBackend {
    /// [`GitWorkspace`].
    #[default]
    Git,
    /// Pure-Rust CAS Merkle + transactional ledger workspace store ([`D6Workspace`]).
    D6,
}

impl WorkspaceBackend {
    /// `"git"` or `"d6"`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::D6 => "d6",
        }
    }

    /// The backend `name` names ([`Self::name`]), exactly.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "git" => Some(Self::Git),
            "d6" => Some(Self::D6),
            _ => None,
        }
    }

    /// Default backend (`Git`) when selecting `"auto"` for a clean root.
    pub const fn auto(_root: &Path) -> Self {
        Self::Git
    }
}

/// Whether `requested` may safely share a root already initialized by `existing`.
pub const fn backends_compatible(existing: WorkspaceBackend, requested: WorkspaceBackend) -> bool {
    matches!(
        (existing, requested),
        (WorkspaceBackend::D6, WorkspaceBackend::D6)
            | (WorkspaceBackend::Git, WorkspaceBackend::Git)
    )
}

const RETIRED_BACKEND_DIRS: &[(&str, &str)] = &[
    (".cow", "cow"),
    (".overlay", "overlay"),
    (".d6-shadow", "d6-shadow"),
];

fn retired_backend_dir_error(root: &Path, entry_path: &Path, backend: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!(
            "workspace root {} contains retired {backend} backend directory {} \
             (removed in S3a; migrate or use a new SPAWN_WORKSPACE_ROOT — see docs/upgrading.md)",
            root.display(),
            entry_path.display()
        ),
    )
}

/// The backend `root`'s workspaces run on, read and never written: the one
/// its [`BACKEND_MARKER`] names, else the one what is there shows (`.d6/` is
/// native D6's, `.sources/` or any workspace directory is Git's), else `None`
/// (a new or empty root). A marker that names no supported backend or an
/// unmarked root containing retired `.cow/`, `.overlay/`, or `.d6-shadow/`
/// directories fails closed.
pub fn root_backend(root: &Path) -> std::io::Result<Option<WorkspaceBackend>> {
    match std::fs::read_to_string(root.join(BACKEND_MARKER)) {
        Ok(text) => {
            let trimmed = text.trim();
            return WorkspaceBackend::parse(trimmed).map(Some).ok_or_else(|| {
                let message = if matches!(trimmed, "cow" | "overlay" | "d6-shadow") {
                    format!(
                        "{} names retired workspace backend {trimmed:?} \
                         (removed in S3a; migrate or use a new SPAWN_WORKSPACE_ROOT — see docs/upgrading.md)",
                        root.join(BACKEND_MARKER).display(),
                    )
                } else {
                    format!(
                        "{} names no workspace backend: {:?}",
                        root.join(BACKEND_MARKER).display(),
                        trimmed
                    )
                };
                std::io::Error::new(std::io::ErrorKind::InvalidData, message)
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut has_d6 = false;
    let mut workspaces = false;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if let Some(name_str) = name.to_str() {
            if let Some((_, retired)) = RETIRED_BACKEND_DIRS
                .iter()
                .find(|(dir_name, _)| *dir_name == name_str)
            {
                return Err(retired_backend_dir_error(root, &entry.path(), retired));
            }
            if name_str == ".d6" {
                has_d6 = true;
                continue;
            }
        }
        let is_git_dir = (name.to_str() == Some(".sources")
            || !name.as_encoded_bytes().starts_with(b"."))
            && entry.file_type()?.is_dir();
        workspaces |= is_git_dir;
    }
    if has_d6 {
        Ok(Some(WorkspaceBackend::D6))
    } else {
        Ok(workspaces.then_some(WorkspaceBackend::Git))
    }
}

/// Claim `root` for `backend`: the first store of any process to use the
/// root writes its [`BACKEND_MARKER`] (made beside it and linked into
/// place, so two processes racing to claim it see one winner), and every
/// later one must ask for a compatible backend ([`backends_compatible`]).
/// Refused, with nothing written, when the root belongs to an incompatible
/// backend ([`root_backend`]): pure `git` and pure `d6` never share a root
/// without an explicit cutover.
pub fn claim_root(root: &Path, backend: WorkspaceBackend) -> Result<(), String> {
    let marker = root.join(BACKEND_MARKER);
    let refused = |there: WorkspaceBackend| {
        let why = if marker.is_file() {
            format!("named by {}", marker.display())
        } else {
            "by what the root holds".to_string()
        };
        format!(
            "workspace root {} is the {} backend's ({why}); refusing to run the {} backend on \
             it: set SPAWN_WORKSPACE_BACKEND={} (or auto, which follows the root) (complete D6 \
             project cutover before switching between git and pure d6, or use another \
             SPAWN_WORKSPACE_ROOT)",
            root.display(),
            there.name(),
            backend.name(),
            there.name()
        )
    };
    let read = |root: &Path| root_backend(root).map_err(|error| error.to_string());
    let existing = match read(root)? {
        Some(there) if !backends_compatible(there, backend) => return Err(refused(there)),
        Some(_) if marker.is_file() => return Ok(()),
        other => other,
    };
    let marker_backend = existing.unwrap_or(backend);
    std::fs::create_dir_all(root).map_err(|error| format!("create {}: {error}", root.display()))?;
    let staged = root.join(format!(
        "{BACKEND_MARKER}.{}",
        crate::reflink::unique_suffix()
    ));
    std::fs::write(&staged, format!("{}\n", marker_backend.name()))
        .and_then(|()| std::fs::File::open(&staged)?.sync_all())
        .map_err(|error| format!("write {}: {error}", staged.display()))?;
    let linked = std::fs::hard_link(&staged, &marker);
    let _ = std::fs::remove_file(&staged);
    match linked {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => match read(root)? {
            Some(there) if backends_compatible(there, backend) => Ok(()),
            Some(there) => Err(refused(there)),
            None => Err(format!("{} vanished while claimed", marker.display())),
        },
        Err(error) => Err(format!("claim {}: {error}", marker.display())),
    }
}

/// The workspace store of this machine: `git` (the default) or opt-in
/// pure-Rust `d6` (`D6Workspace`).
#[derive(Debug)]
#[expect(clippy::large_enum_variant)]
pub enum LocalWorkspaceStore {
    /// Pure-Rust CAS Merkle + transactional ledger workspace store ([`D6Workspace`]).
    D6(D6Workspace<DynRefLedger>),
    /// One `git worktree`-style working copy per branch ([`GitWorkspace`]).
    Git(GitWorkspace),
}

impl LocalWorkspaceStore {
    /// The Git store over the bare repository `repo`, workspaces under
    /// `root`, with checkpoint file size cap `max_file_bytes`.
    pub fn git(repo: impl AsRef<Path>, root: impl AsRef<Path>, max_file_bytes: u64) -> Self {
        Self::Git(GitWorkspace::new(repo, root, max_file_bytes))
    }

    /// Pure-Rust CAS Merkle + ledger workspace store (`D6Workspace`) for `project`
    /// with workspaces under `root`.
    pub fn d6(project: impl Into<String>, root: impl AsRef<Path>, options: D6StoreOptions) -> Self {
        let mut store = D6Workspace::new(
            project,
            root,
            options.cas,
            Arc::new(options.ledger),
            options.max_file_bytes,
        );
        if let Some(policy) = options.l1_policy {
            store = store.with_l1_policy(policy);
        }
        Self::D6(store)
    }

    /// Checkpoint file size cap in bytes across whichever backend this store wraps.
    pub const fn max_file_bytes(&self) -> u64 {
        match self {
            Self::D6(d6) => d6.max_file_bytes(),
            Self::Git(git) => git.max_file_bytes(),
        }
    }

    /// Open `backend` over `repo`, workspaces under `root`, with checkpoint
    /// file size cap `max_file_bytes`. Native `d6` requires CAS + ledger
    /// configuration via [`Self::d6`].
    pub fn open(
        backend: WorkspaceBackend,
        repo: impl AsRef<Path>,
        root: impl AsRef<Path>,
        max_file_bytes: u64,
    ) -> Result<Self, String> {
        match backend {
            WorkspaceBackend::Git => Ok(Self::git(repo, root, max_file_bytes)),
            WorkspaceBackend::D6 => Err(
                "workspace backend d6 requires CAS and RefLedger configuration (use \
                 LocalWorkspaceStore::d6 or WorkspaceSettings)"
                    .to_string(),
            ),
        }
    }

    /// Which backend this is.
    pub fn backend(&self) -> WorkspaceBackend {
        match self {
            Self::D6(_) => WorkspaceBackend::D6,
            Self::Git(_) => WorkspaceBackend::Git,
        }
    }

    /// Which backend this is: `"d6"` or `"git"`.
    pub fn backend_name(&self) -> &'static str {
        self.backend().name()
    }

    /// Reserved L1 `tmpfs` bytes across mounted branches when backed by D6.
    pub fn d6_l1_reserved_bytes(&self) -> Option<u64> {
        match self {
            Self::D6(store) => Some(store.l1_total_reserved_bytes()),
            Self::Git(_) => None,
        }
    }

    /// Borrow the underlying [`D6Workspace`] when backed by [`Self::D6`].
    pub const fn d6_workspace(&self) -> Option<&D6Workspace<DynRefLedger>> {
        match self {
            Self::D6(store) => Some(store),
            Self::Git(_) => None,
        }
    }

    /// Claim this store's root for its backend ([`claim_root`]).
    pub fn claim(&self) -> Result<(), String> {
        claim_root(self.root(), self.backend())
    }

    /// The resolved root workspaces live under.
    pub fn root(&self) -> &Path {
        match self {
            Self::D6(store) => store.root(),
            Self::Git(store) => store.root(),
        }
    }

    /// Checkpoint the mounted `workspace` directory cleanly under `WorkspaceStore::commit`.
    pub fn checkpoint(
        &self,
        workspace: &WorkspaceId,
        message: &str,
    ) -> Result<Commit, WorkspaceError> {
        self.inner().commit(workspace, message)
    }

    /// Discard the mounted `workspace` directory cleanly (and any L1 state).
    pub fn discard(&self, workspace: &WorkspaceId) -> Result<(), WorkspaceError> {
        match self {
            Self::D6(store) => store.discard(workspace),
            Self::Git(_) => self.inner().unmount(workspace),
        }
    }

    /// [`WorkspaceStore::set_aside`], only while `ready` says the agent is
    /// idle, asked under the branch's lock before anything is written.
    pub fn set_aside_when(
        &self,
        branch: &BranchName,
        start: Option<&str>,
        message: &str,
        ready: &dyn Fn() -> Result<(), String>,
    ) -> Result<SetAside, WorkspaceError> {
        match self {
            Self::D6(store) => store.set_aside_when(branch, start, message, ready),
            Self::Git(store) => store.set_aside_when(branch, start, message, ready),
        }
    }

    /// Commit the workspace, asking `take_back` first, under the branch's
    /// lock, when its branch is gone.
    pub fn commit_or_take_back(
        &self,
        workspace: &WorkspaceId,
        message: &str,
        take_back: &dyn Fn() -> Result<(), WorkspaceError>,
    ) -> Result<Commit, WorkspaceError> {
        match self {
            Self::D6(store) => store.commit_or_take_back(workspace, message, take_back),
            Self::Git(store) => store.commit_or_take_back(workspace, message, take_back),
        }
    }

    /// Persists worker workspace changes/HEAD into `refs/spawn/workers/<branch>/head`
    /// and `target_refs` without modifying `refs/heads/*` (`#103`, `#110`).
    pub fn pin_worker_head(
        &self,
        branch: &str,
        directory: Option<&Path>,
        target_refs: &[&str],
    ) -> Result<Option<CommitId>, WorkspaceError> {
        match self {
            Self::D6(store) => store.pin_worker_head(branch, directory, target_refs),
            Self::Git(store) => store.pin_worker_head(branch, directory, target_refs),
        }
    }

    /// Files `commit` changes against `base` that hold an unresolved conflict marker block.
    pub fn conflict_markers(
        &self,
        base: &str,
        commit: &str,
    ) -> Result<Vec<String>, WorkspaceError> {
        match self {
            Self::D6(store) => store.conflict_markers(base, commit),
            Self::Git(store) => store.conflict_markers(base, commit),
        }
    }

    /// Unified patch turning `from` into `to` for Git-backed stores.
    pub fn patch_between(&self, from: &str, to: &str) -> Result<String, WorkspaceError> {
        match self {
            Self::D6(_) => Err(WorkspaceError::Unavailable(
                "D6 workspace does not render textual git patches".to_string(),
            )),
            Self::Git(store) => store.patch_between(from, to),
        }
    }

    /// Non-marker conflict `main` copies recorded for `branch`, if any.
    pub fn main_copies(&self, branch: &str) -> Option<(String, Vec<String>)> {
        match self {
            Self::D6(store) => store.main_copies(branch),
            Self::Git(store) => store.main_copies(branch),
        }
    }

    /// Whether a merge of `main` into `branch` is waiting on conflict marker resolution.
    pub fn has_pending_merge(&self, branch: &str) -> bool {
        match self {
            Self::D6(store) => store.has_pending_merge(branch),
            Self::Git(store) => store.has_pending_merge(branch),
        }
    }

    fn inner(&self) -> &dyn WorkspaceStore {
        match self {
            Self::D6(store) => store,
            Self::Git(store) => store,
        }
    }
}

impl WorkspaceStore for LocalWorkspaceStore {
    fn project(&self) -> &str {
        self.inner().project()
    }

    fn ensure_branch(
        &self,
        branch: &BranchName,
        from: Option<&str>,
    ) -> Result<Branch, WorkspaceError> {
        self.inner().ensure_branch(branch, from)
    }

    fn mount(&self, branch: &BranchName) -> Result<WorkspaceId, WorkspaceError> {
        self.inner().mount(branch)
    }

    fn mount_when(
        &self,
        branch: &BranchName,
        ready: &dyn Fn() -> Result<(), String>,
    ) -> Result<WorkspaceId, WorkspaceError> {
        self.inner().mount_when(branch, ready)
    }

    fn canonical(&self, workspace: &WorkspaceId) -> WorkspaceId {
        self.inner().canonical(workspace)
    }

    fn commit(&self, workspace: &WorkspaceId, message: &str) -> Result<Commit, WorkspaceError> {
        self.inner().commit(workspace, message)
    }

    fn unmount(&self, workspace: &WorkspaceId) -> Result<(), WorkspaceError> {
        self.inner().unmount(workspace)
    }

    fn unmount_when(
        &self,
        workspace: &WorkspaceId,
        ready: &dyn Fn() -> Result<(), WorkspaceError>,
    ) -> Result<(), WorkspaceError> {
        self.inner().unmount_when(workspace, ready)
    }

    fn is_mounted(&self, workspace: &WorkspaceId) -> bool {
        self.inner().is_mounted(workspace)
    }

    fn merge_main_clean(&self, branch: &BranchName) -> Result<BaseMerge, WorkspaceError> {
        self.inner().merge_main_clean(branch)
    }

    fn merge_base(&self, workspace: &WorkspaceId) -> Result<BaseMerge, WorkspaceError> {
        self.inner().merge_base(workspace)
    }

    fn merge_base_when(
        &self,
        workspace: &WorkspaceId,
        ready: &dyn Fn() -> Result<(), String>,
    ) -> Result<BaseMerge, WorkspaceError> {
        self.inner().merge_base_when(workspace, ready)
    }

    fn hold(&self, workspace: &WorkspaceId) -> Option<Hold> {
        self.inner().hold(workspace)
    }

    fn set_aside(
        &self,
        branch: &BranchName,
        start: Option<&str>,
        message: &str,
    ) -> Result<SetAside, WorkspaceError> {
        self.inner().set_aside(branch, start, message)
    }

    fn notices(&self, workspace: &WorkspaceId) -> Vec<String> {
        self.inner().notices(workspace)
    }

    fn notices_told(&self, workspace: &WorkspaceId, notices: &[String]) {
        self.inner().notices_told(workspace, notices);
    }

    fn branch_head(&self, branch: &BranchName) -> Result<Option<CommitId>, WorkspaceError> {
        self.inner().branch_head(branch)
    }
}

impl From<GitWorkspace> for LocalWorkspaceStore {
    fn from(store: GitWorkspace) -> Self {
        Self::Git(store)
    }
}

impl From<D6Workspace<DynRefLedger>> for LocalWorkspaceStore {
    fn from(store: D6Workspace<DynRefLedger>) -> Self {
        Self::D6(store)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "spawn-ws-unit-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn backend_parse_and_default_is_git() {
        for (backend, expected) in [(WorkspaceBackend::Git, "git"), (WorkspaceBackend::D6, "d6")] {
            assert_eq!(backend.name(), expected);
            assert_eq!(WorkspaceBackend::parse(expected), Some(backend));
        }
        assert_eq!(WorkspaceBackend::default(), WorkspaceBackend::Git);
        for retired in ["auto", "cow", "overlay", "d6-shadow", "d6_shadow"] {
            assert_eq!(WorkspaceBackend::parse(retired), None, "{retired}");
        }
        let dir = scratch_dir("auto");
        assert_eq!(WorkspaceBackend::auto(&dir), WorkspaceBackend::Git);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claim_root_refuses_incompatible_d6_and_git_mixes() {
        let base = scratch_dir("claim");

        // 1. Marked Git root refuses D6.
        let git_root = base.join("git");
        claim_root(&git_root, WorkspaceBackend::Git).expect("claim git");
        assert_eq!(
            root_backend(&git_root).expect("read"),
            Some(WorkspaceBackend::Git)
        );
        let err = claim_root(&git_root, WorkspaceBackend::D6).expect_err("git vs d6 refused");
        assert!(err.contains("cutover"), "{err}");

        // 2. Unmarked legacy Git root reads as Git and refuses D6 without writing marker.
        let legacy_git = base.join("legacy-git");
        std::fs::create_dir_all(legacy_git.join("agent-1")).expect("agent dir");
        assert_eq!(
            root_backend(&legacy_git).expect("legacy"),
            Some(WorkspaceBackend::Git)
        );
        assert!(claim_root(&legacy_git, WorkspaceBackend::D6).is_err());
        assert!(!legacy_git.join(BACKEND_MARKER).exists());

        // 2b. Unmarked legacy Git root holding only `.sources/` reads as Git; explicit d6 marker wins.
        let legacy_sources = base.join("legacy-sources");
        std::fs::create_dir_all(legacy_sources.join(".sources")).expect("mkdir .sources");
        assert_eq!(
            root_backend(&legacy_sources).expect("sources"),
            Some(WorkspaceBackend::Git)
        );
        assert!(claim_root(&legacy_sources, WorkspaceBackend::D6).is_err());
        let sources_marked_d6 = base.join("sources-marked-d6");
        std::fs::create_dir_all(sources_marked_d6.join(".sources")).expect("mkdir .sources");
        std::fs::write(sources_marked_d6.join(BACKEND_MARKER), "d6\n").expect("write d6 marker");
        assert_eq!(
            root_backend(&sources_marked_d6).expect("explicit d6 over .sources"),
            Some(WorkspaceBackend::D6)
        );

        // 3. Pure D6 root (marked or unmarked .d6/) refuses Git.
        let d6_root = base.join("d6");
        claim_root(&d6_root, WorkspaceBackend::D6).expect("claim d6");
        assert!(claim_root(&d6_root, WorkspaceBackend::Git).is_err());
        let d6_unmarked = base.join("d6-unmarked");
        std::fs::create_dir_all(d6_unmarked.join(".d6")).expect("mkdir .d6");
        assert_eq!(
            root_backend(&d6_unmarked).expect("unmarked d6"),
            Some(WorkspaceBackend::D6)
        );
        assert!(claim_root(&d6_unmarked, WorkspaceBackend::Git).is_err());

        // 4. Retired and unknown markers fail closed.
        for legacy_marker in ["cow", "overlay", "d6-shadow"] {
            std::fs::write(
                base.join("d6").join(BACKEND_MARKER),
                format!("{legacy_marker}\n"),
            )
            .expect("write retired marker");
            let err = root_backend(&base.join("d6"))
                .expect_err("retired marker must fail closed")
                .to_string();
            assert!(
                err.contains("retired") && err.contains(legacy_marker),
                "{err}"
            );
        }
        std::fs::write(base.join("d6").join(BACKEND_MARKER), "zfs\n")
            .expect("write unknown marker");
        let unknown_err = root_backend(&base.join("d6"))
            .expect_err("unknown marker must fail closed")
            .to_string();
        assert!(
            unknown_err.contains("names no workspace backend"),
            "{unknown_err}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn unmarked_retired_backend_directories_fail_closed() {
        let base = scratch_dir("unmarked-retired");
        for (dir_name, backend_name) in [
            (".cow", "cow"),
            (".overlay", "overlay"),
            (".d6-shadow", "d6-shadow"),
        ] {
            // Alone: must fail closed and refuse claim_root without writing a marker.
            let alone = base.join(format!("alone-{backend_name}"));
            std::fs::create_dir_all(alone.join(dir_name)).expect("mkdir retired alone");
            let err = root_backend(&alone)
                .expect_err("retired dir alone must fail closed")
                .to_string();
            assert!(
                err.contains("retired") && err.contains(backend_name),
                "{err}"
            );
            assert!(claim_root(&alone, WorkspaceBackend::Git).is_err());
            assert!(claim_root(&alone, WorkspaceBackend::D6).is_err());
            assert!(!alone.join(BACKEND_MARKER).exists());

            // With sibling plain workspace dir: must not fall back to Git.
            let with_ws = base.join(format!("ws-{backend_name}"));
            std::fs::create_dir_all(with_ws.join("agent-1")).expect("mkdir workspace");
            std::fs::create_dir_all(with_ws.join(dir_name)).expect("mkdir retired with ws");
            let err = root_backend(&with_ws)
                .expect_err("retired dir with workspaces must fail closed")
                .to_string();
            assert!(
                err.contains("retired") && err.contains(backend_name),
                "{err}"
            );

            // With sibling `.d6`: must not fall back to D6 regardless of directory iteration order.
            let with_d6 = base.join(format!("d6-{backend_name}"));
            std::fs::create_dir_all(with_d6.join(".d6")).expect("mkdir .d6");
            std::fs::create_dir_all(with_d6.join(dir_name)).expect("mkdir retired with .d6");
            let err = root_backend(&with_d6)
                .expect_err("retired dir with .d6 must fail closed")
                .to_string();
            assert!(
                err.contains("retired") && err.contains(backend_name),
                "{err}"
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn open_requires_d6_options_and_never_falls_back_to_git() {
        let base = scratch_dir("open");
        let err = LocalWorkspaceStore::open(
            WorkspaceBackend::D6,
            base.join("r.git"),
            &base,
            crate::workspace_rules::DEFAULT_MAX_FILE_BYTES,
        )
        .expect_err("requires options");
        assert!(err.contains("d6"), "{err}");
        let git = LocalWorkspaceStore::open(
            WorkspaceBackend::Git,
            base.join("r.git"),
            &base,
            crate::workspace_rules::DEFAULT_MAX_FILE_BYTES,
        )
        .expect("open git");
        assert_eq!(git.backend(), WorkspaceBackend::Git);
        let _ = std::fs::remove_dir_all(&base);
    }
}
