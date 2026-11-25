//! The workspace store of the machine Spawn runs on: one type over the
//! three engines ([`GitWorkspace`], [`CowWorkspace`] and, on Linux,
//! [`OverlayWorkspace`]), told apart only here, as [`crate::LocalTerminalHost`]
//! tells the terminal hosts apart.
//!
//! Every engine obeys the same [`WorkspaceStore`] contract (the branch lock
//! under `mount_when`, `unmount_when`, `hold` and `set_aside_when`, the
//! `is_mounted` the Bridge's recovery asks), so a caller holds a
//! [`LocalWorkspaceStore`] and never asks which engine it is, except to say
//! it ([`LocalWorkspaceStore::backend_name`]) or to launch a process into an
//! overlay ([`LocalWorkspaceStore::launch_arguments`]). An engine is chosen
//! in one place ([`WorkspaceBackend`]), and a root runs one engine only
//! ([`claim_root`]).
//!
//! Build-artifact deduplication ([`crate::cas_dedup`]) is a helper of the
//! store, never part of a checkpoint: [`LocalWorkspaceStore::dedup_artifacts`]
//! clones a Cow workspace's `target/` and `node_modules/` against the CAS
//! at `<root>/.cas` while the workspace is offline, and
//! [`LocalWorkspaceStore::prune_cas`] drops the blobs nothing uses any more.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use spawn_core::id::{BranchName, CommitId, WorkspaceId};
use spawn_core::port::{BaseMerge, Branch, Commit, Hold, SetAside, WorkspaceError, WorkspaceStore};

use crate::cas_dedup::{
    ARTIFACT_DIRECTORIES, ArtifactRoot, CAS_MARKER, DedupConfig, DedupReport, PruneReport,
    dedup_dirs, prune_unused,
};
use crate::cow::{CowWorkspace, cow_clone_supported};
use crate::git::GitWorkspace;
#[cfg(target_os = "linux")]
use crate::overlay::{OverlayWorkspace, overlay_supported_in};

/// The directory under a store's root that holds the CAS of deduplicated
/// build artifacts ([`LocalWorkspaceStore::cas_root`]).
pub const CAS_DIRECTORY: &str = ".cas";

/// The file under a workspace root that names the one engine its
/// workspaces run on ([`claim_root`]).
pub const BACKEND_MARKER: &str = ".spawn-workspace-backend";

/// The three engines a [`LocalWorkspaceStore`] can be, by name. The one
/// place an engine is chosen ([`WorkspaceBackend::auto_with`]): the
/// constructors here and `spawn`'s `SPAWN_WORKSPACE_BACKEND` both use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkspaceBackend {
    /// [`GitWorkspace`].
    Git,
    /// [`CowWorkspace`].
    Cow,
    /// `OverlayWorkspace` (Linux).
    Overlay,
}

impl WorkspaceBackend {
    /// `"git"`, `"cow"` or `"overlay"`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Cow => "cow",
            Self::Overlay => "overlay",
        }
    }

    /// The engine `name` names ([`Self::name`]), exactly.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "git" => Some(Self::Git),
            "cow" => Some(Self::Cow),
            "overlay" => Some(Self::Overlay),
            _ => None,
        }
    }

    /// The engine "the cheapest supported" means, given whether the root's
    /// file system clones copy-on-write: Cow where it does, else Git. Never
    /// the overlay: an engine started by `spawn loop` would not see an
    /// overlay workspace's files (they are inside its holder's namespace),
    /// so the overlay is chosen only by name, until the loop serves it.
    pub fn auto_with(clones: bool) -> Self {
        if clones { Self::Cow } else { Self::Git }
    }

    /// [`Self::auto_with`] for `root`, asked of its file system
    /// ([`cow_clone_supported`]: a probe file is cloned there; the root is
    /// made if missing).
    pub fn auto(root: &Path) -> Self {
        let _ = std::fs::create_dir_all(root);
        Self::auto_with(cow_clone_supported(root))
    }
}

/// The engine `root`'s workspaces run on, read and never written: the one
/// its [`BACKEND_MARKER`] names, else the one what is there shows (a root
/// from before the marker: `.cow/` is Cow's, `.overlay/` the overlay's, any
/// workspace directory Git's), else `None` (a new or empty root). A marker
/// that names no engine is an error.
pub fn root_backend(root: &Path) -> std::io::Result<Option<WorkspaceBackend>> {
    match std::fs::read_to_string(root.join(BACKEND_MARKER)) {
        Ok(text) => {
            return WorkspaceBackend::parse(text.trim())
                .map(Some)
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "{} names no workspace engine: {:?}",
                            root.join(BACKEND_MARKER).display(),
                            text.trim()
                        ),
                    )
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
    let mut workspaces = false;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        match name.to_str() {
            Some(".cow") => return Ok(Some(WorkspaceBackend::Cow)),
            Some(".overlay") => return Ok(Some(WorkspaceBackend::Overlay)),
            _ => {}
        }
        workspaces |= !name.as_encoded_bytes().starts_with(b".") && entry.file_type()?.is_dir();
    }
    Ok(workspaces.then_some(WorkspaceBackend::Git))
}

/// Claim `root` for `backend`: the first store of any process to use the
/// root writes its [`BACKEND_MARKER`] (made beside it and linked into
/// place, so two processes racing to claim it see one winner), and every
/// later one must ask for the same engine. Refused, with nothing written,
/// when the root is another engine's ([`root_backend`]): two engines never
/// share a root, whatever process asks (`spawn loop`, a set-aside, a merge
/// of main).
pub fn claim_root(root: &Path, backend: WorkspaceBackend) -> Result<(), String> {
    let marker = root.join(BACKEND_MARKER);
    let refused = |there: WorkspaceBackend| {
        // Name the marker only when there is one: a root from before it
        // was read by what it holds.
        let why = if marker.is_file() {
            format!("named by {}", marker.display())
        } else {
            "by what the root holds".to_string()
        };
        format!(
            "workspace root {} is the {} engine's ({why}); refusing to run the {} engine on \
             it: set SPAWN_WORKSPACE_BACKEND={} (or auto, which follows the root) or use \
             another SPAWN_WORKSPACE_ROOT",
            root.display(),
            there.name(),
            backend.name(),
            there.name()
        )
    };
    let read = |root: &Path| root_backend(root).map_err(|error| error.to_string());
    match read(root)? {
        Some(there) if there != backend => return Err(refused(there)),
        Some(_) if marker.is_file() => return Ok(()),
        _ => {}
    }
    std::fs::create_dir_all(root).map_err(|error| format!("create {}: {error}", root.display()))?;
    let staged = root.join(format!("{BACKEND_MARKER}.{}", crate::cow::unique_suffix()));
    std::fs::write(&staged, format!("{}\n", backend.name()))
        .and_then(|()| std::fs::File::open(&staged)?.sync_all())
        .map_err(|error| format!("write {}: {error}", staged.display()))?;
    let linked = std::fs::hard_link(&staged, &marker);
    let _ = std::fs::remove_file(&staged);
    match linked {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => match read(root)? {
            Some(there) if there == backend => Ok(()),
            Some(there) => Err(refused(there)),
            None => Err(format!("{} vanished while claimed", marker.display())),
        },
        Err(error) => Err(format!("claim {}: {error}", marker.display())),
    }
}

/// The workspace store of this machine: Git working copies (the default),
/// copy-on-write clones of a per-commit baseline, or, on Linux, an
/// unprivileged overlay over a per-commit snapshot layer.
#[derive(Debug)]
pub enum LocalWorkspaceStore {
    /// One `git worktree`-style working copy per branch ([`GitWorkspace`]).
    Git(GitWorkspace),
    /// One plain directory per branch, cloned extent by extent from a
    /// read-only baseline of its commit ([`CowWorkspace`]).
    Cow(CowWorkspace),
    /// One unprivileged overlay per branch over a shared snapshot of its
    /// commit ([`OverlayWorkspace`]).
    #[cfg(target_os = "linux")]
    Overlay(OverlayWorkspace),
}

impl LocalWorkspaceStore {
    /// The Git store over the bare repository `repo`, workspaces under
    /// `root`.
    pub fn git(repo: impl AsRef<Path>, root: impl AsRef<Path>) -> Self {
        Self::Git(GitWorkspace::new(repo, root))
    }

    /// The copy-on-write store over `repo`, workspaces under `root`. It
    /// works on any file system (a mount copies where it cannot clone);
    /// [`Self::auto`] chooses it only where clones work.
    pub fn cow(repo: impl AsRef<Path>, root: impl AsRef<Path>) -> Self {
        Self::Cow(CowWorkspace::new(repo, root))
    }

    /// The overlay store over `repo`, workspaces under `root`, once the
    /// kernel showed an unprivileged overlay mounts under `root`
    /// ([`overlay_supported_in`]); refused, with the kernel's reason,
    /// otherwise.
    #[cfg(target_os = "linux")]
    pub fn overlay(repo: impl AsRef<Path>, root: impl AsRef<Path>) -> Result<Self, String> {
        let root = root.as_ref();
        std::fs::create_dir_all(root)
            .map_err(|error| format!("create {}: {error}", root.display()))?;
        overlay_supported_in(root)
            .map_err(|why| format!("no unprivileged overlay under {}: {why}", root.display()))?;
        Ok(Self::Overlay(OverlayWorkspace::new(repo, root)))
    }

    /// The overlay store: never on a system other than Linux.
    #[cfg(not(target_os = "linux"))]
    pub fn overlay(repo: impl AsRef<Path>, root: impl AsRef<Path>) -> Result<Self, String> {
        let _ = (repo, root);
        Err("the overlay workspace store needs Linux".to_string())
    }

    /// The cheapest store `root` supports that every caller can serve
    /// ([`WorkspaceBackend::auto`]): copy-on-write clones when its file
    /// system clones ([`cow_clone_supported`]: btrfs, XFS with reflink,
    /// APFS), else Git working copies. Never the overlay, which is chosen
    /// by name alone ([`Self::overlay`]).
    pub fn auto(repo: impl AsRef<Path>, root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        match WorkspaceBackend::auto(root) {
            WorkspaceBackend::Cow => Self::cow(repo, root),
            _ => Self::git(repo, root),
        }
    }

    /// The store of `backend` over `repo`, workspaces under `root`; the
    /// overlay only where the kernel mounts one ([`Self::overlay`]).
    pub fn open(
        backend: WorkspaceBackend,
        repo: impl AsRef<Path>,
        root: impl AsRef<Path>,
    ) -> Result<Self, String> {
        match backend {
            WorkspaceBackend::Git => Ok(Self::git(repo, root)),
            WorkspaceBackend::Cow => Ok(Self::cow(repo, root)),
            WorkspaceBackend::Overlay => Self::overlay(repo, root),
        }
    }

    /// Which engine this is.
    pub fn backend(&self) -> WorkspaceBackend {
        match self {
            Self::Git(_) => WorkspaceBackend::Git,
            Self::Cow(_) => WorkspaceBackend::Cow,
            #[cfg(target_os = "linux")]
            Self::Overlay(_) => WorkspaceBackend::Overlay,
        }
    }

    /// Which engine this is: `"git"`, `"cow"` or `"overlay"`.
    pub fn backend_name(&self) -> &'static str {
        self.backend().name()
    }

    /// Claim this store's root for its engine ([`claim_root`]).
    pub fn claim(&self) -> Result<(), String> {
        claim_root(self.root(), self.backend())
    }

    /// Remove what no mounted workspace uses any more: a Cow store's
    /// baselines ([`CowWorkspace::prune_baselines`]), an overlay store's
    /// snapshot layers ([`OverlayWorkspace::prune_layers`]), and, on Linux,
    /// the overlay probes a killed process left under the root
    /// ([`crate::sweep_probes`]). Each step runs whatever the others found:
    /// a probe that cannot be removed (it holds a `*.jsonl`) or a root that
    /// cannot be listed is a warning, and the baselines and layers are still
    /// pruned. Returns how many trees went and the warnings; an error only
    /// when the baselines or layers could not be pruned (a state that cannot
    /// be read prunes nothing).
    pub fn prune_unused(&self) -> Result<(usize, Vec<String>), String> {
        let mut warnings = Vec::new();
        #[cfg(target_os = "linux")]
        let swept = match crate::overlay::sweep_probes(self.root()) {
            Ok(swept) => {
                for (probe, why) in &swept.kept {
                    warnings.push(format!("overlay probe {} kept: {why}", probe.display()));
                }
                swept.removed
            }
            Err(error) => {
                warnings.push(format!("sweep overlay probes: {error}"));
                0
            }
        };
        #[cfg(not(target_os = "linux"))]
        let swept = 0;
        let pruned = match self {
            Self::Git(_) => 0,
            Self::Cow(store) => store
                .prune_baselines()
                .map_err(|error| format!("prune baselines: {error}"))?,
            #[cfg(target_os = "linux")]
            Self::Overlay(store) => store
                .prune_layers()
                .map_err(|error| format!("prune layers: {error}"))?,
        };
        Ok((swept + pruned, warnings))
    }

    /// The resolved root workspaces live under.
    pub fn root(&self) -> &Path {
        match self {
            Self::Git(store) => store.root(),
            Self::Cow(store) => store.root(),
            #[cfg(target_os = "linux")]
            Self::Overlay(store) => store.root(),
        }
    }

    /// [`WorkspaceStore::set_aside`], only while `ready` says the agent is
    /// idle, asked under the branch's lock before anything is written
    /// ([`GitWorkspace::set_aside_when`], [`CowWorkspace::set_aside_when`],
    /// [`OverlayWorkspace::set_aside_when`]: one rule).
    pub fn set_aside_when(
        &self,
        branch: &BranchName,
        start: Option<&str>,
        message: &str,
        ready: &dyn Fn() -> Result<(), String>,
    ) -> Result<SetAside, WorkspaceError> {
        match self {
            Self::Git(store) => store.set_aside_when(branch, start, message, ready),
            Self::Cow(store) => store.set_aside_when(branch, start, message, ready),
            #[cfg(target_os = "linux")]
            Self::Overlay(store) => store.set_aside_when(branch, start, message, ready),
        }
    }

    /// Commit the workspace, asking `take_back` first, under the branch's
    /// lock, when its branch is gone ([`GitWorkspace::commit_or_take_back`]
    /// and its Cow and overlay twins).
    pub fn commit_or_take_back(
        &self,
        workspace: &WorkspaceId,
        message: &str,
        take_back: &dyn Fn() -> Result<(), WorkspaceError>,
    ) -> Result<Commit, WorkspaceError> {
        match self {
            Self::Git(store) => store.commit_or_take_back(workspace, message, take_back),
            Self::Cow(store) => store.commit_or_take_back(workspace, message, take_back),
            #[cfg(target_os = "linux")]
            Self::Overlay(store) => store.commit_or_take_back(workspace, message, take_back),
        }
    }

    /// The directory where the host reads and writes `workspace`'s files:
    /// the workspace's own path for Git and Cow; for an overlay, its merged
    /// view through the holder ([`OverlayWorkspace::host_path`]).
    pub fn host_path(&self, workspace: &WorkspaceId) -> Result<PathBuf, WorkspaceError> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Overlay(store) => store.host_path(workspace),
            _ => Ok(PathBuf::from(workspace.path())),
        }
    }

    /// The launch prefix a process must run under to see `workspace`
    /// ([`OverlayWorkspace::launch_arguments`]); `None` for Git and Cow,
    /// whose workspaces are plain directories.
    pub fn launch_arguments(
        &self,
        workspace: &WorkspaceId,
    ) -> Result<Option<Vec<OsString>>, WorkspaceError> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Overlay(store) => store.launch_arguments(workspace).map(Some),
            _ => {
                let _ = workspace;
                Ok(None)
            }
        }
    }

    /// The CAS of deduplicated build artifacts: `<root>/.cas`.
    pub fn cas_root(&self) -> PathBuf {
        self.root().join(CAS_DIRECTORY)
    }

    /// Deduplicate `workspace`'s build artifacts (`target/`,
    /// `node_modules/`: [`ARTIFACT_DIRECTORIES`]) against [`Self::cas_root`],
    /// with the workspace taken offline for the pass
    /// ([`CowWorkspace::offline`]: no worker holds its lease, none starts,
    /// no process is seen in it). Transcripts are never touched: a `*.jsonl`
    /// file and anything under a `.transcripts/` directory are skipped
    /// file by file ([`dedup_dirs`]), and only artifact directories are
    /// roots.
    ///
    /// `Ok(None)` when there is nothing to do: a Git store (a working copy
    /// has no offline lease), an overlay (its artifacts are in its upperdir,
    /// behind the holder's namespace; share them through pre-warmed lower
    /// layers, [`OverlayWorkspace::with_lower_layers`]), or a workspace with
    /// no artifact directory.
    pub fn dedup_artifacts(&self, workspace: &WorkspaceId) -> Result<Option<DedupReport>, String> {
        let Self::Cow(store) = self else {
            return Ok(None);
        };
        let directory = PathBuf::from(workspace.path());
        let present: Vec<PathBuf> = ARTIFACT_DIRECTORIES
            .iter()
            .map(|name| directory.join(name))
            .filter(|path| std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir()))
            .collect();
        if present.is_empty() {
            return Ok(None);
        }
        let offline = store
            .offline(workspace)
            .map_err(|error| format!("take {workspace} offline: {error}"))?;
        let roots = present
            .iter()
            .map(|path| ArtifactRoot::new(&offline, path))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        dedup_dirs(&DedupConfig::new(self.cas_root()), &roots)
            .map(Some)
            .map_err(|error| error.to_string())
    }

    /// Drop the CAS blobs no deduplicated file uses any more and that are
    /// older than `grace` ([`prune_unused`]). `Ok(None)` when no pass made
    /// a CAS here yet.
    pub fn prune_cas(&self, grace: Duration) -> std::io::Result<Option<PruneReport>> {
        let cas = self.cas_root();
        if !cas.join(CAS_MARKER).is_file() {
            return Ok(None);
        }
        prune_unused(&cas, grace).map(Some)
    }

    fn inner(&self) -> &dyn WorkspaceStore {
        match self {
            Self::Git(store) => store,
            Self::Cow(store) => store,
            #[cfg(target_os = "linux")]
            Self::Overlay(store) => store,
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

impl From<CowWorkspace> for LocalWorkspaceStore {
    fn from(store: CowWorkspace) -> Self {
        Self::Cow(store)
    }
}

#[cfg(target_os = "linux")]
impl From<OverlayWorkspace> for LocalWorkspaceStore {
    fn from(store: OverlayWorkspace) -> Self {
        Self::Overlay(store)
    }
}
