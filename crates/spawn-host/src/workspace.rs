//! The workspace store of the machine Spawn runs on: one type over the
//! five workspace backends ([`GitWorkspace`], [`CowWorkspace`], on Linux
//! [`OverlayWorkspace`], [`ShadowD6Workspace`] and [`D6Workspace`]), told
//! apart only here, as [`crate::LocalTerminalHost`] tells the terminal hosts
//! apart.
//!
//! Every backend obeys the same [`WorkspaceStore`] contract (the branch lock
//! under `mount_when`, `unmount_when`, `hold` and `set_aside_when`, the
//! `is_mounted` the Bridge's recovery asks), so a caller holds a
//! [`LocalWorkspaceStore`] and never asks which backend it is, except to say
//! it ([`LocalWorkspaceStore::backend_name`]) or to launch a process into an
//! overlay ([`LocalWorkspaceStore::launch_arguments`]). A backend is chosen
//! in one place ([`WorkspaceBackend`]), and a root runs one backend only
//! ([`claim_root`]).
//!
//! Build-artifact deduplication ([`crate::cas_dedup`]) is a helper of the
//! store, never part of a checkpoint: [`LocalWorkspaceStore::dedup_artifacts`]
//! clones a Cow workspace's `target/` and `node_modules/` against the CAS
//! at `<root>/.cas` while the workspace is offline, and
//! [`LocalWorkspaceStore::prune_cas`] drops the blobs nothing uses any more.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use spawn_core::id::{BranchName, CommitId, WorkspaceId};
use spawn_core::port::{BaseMerge, Branch, Commit, Hold, SetAside, WorkspaceError, WorkspaceStore};

use crate::cas_dedup::{
    ARTIFACT_DIRECTORIES, ArtifactRoot, CAS_MARKER, DedupConfig, DedupReport, PruneReport,
    dedup_dirs, prune_unused,
};
use crate::cas_merkle::{CasMerkleStore, RefLedger};
use crate::cow::{CowWorkspace, cow_clone_supported};
use crate::d6_shadow::{ShadowD6Workspace, ShadowOracleMetrics, ShadowTrackedSetMode};
use crate::d6_workspace::{D6Workspace, L1TmpfsPolicy};
use crate::git::GitWorkspace;
#[cfg(target_os = "linux")]
use crate::overlay::{OverlayWorkspace, overlay_supported_in};

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
}

impl D6StoreOptions {
    /// Create D6 store options with CAS and ledger handles (`l1_policy = None`).
    pub fn new(cas: Arc<CasMerkleStore>, ledger: DynRefLedger) -> Self {
        Self {
            cas,
            ledger,
            l1_policy: None,
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
            .finish_non_exhaustive()
    }
}

/// Construction options for [`LocalWorkspaceStore::d6_shadow`].
#[derive(Clone)]
pub struct D6ShadowOptions {
    /// Internal shadow `D6Workspace` directory root (typically `<root>/.d6-shadow/work`).
    pub shadow_root: PathBuf,
    /// Content-addressed Merkle object store and journal root.
    pub cas: Arc<CasMerkleStore>,
    /// Transactional ref and snapshot ledger (`spawn_refs` / `spawn_snapshots`).
    pub ledger: DynRefLedger,
    /// Optional bounded L1 `tmpfs` upperdir policy for shadow staging.
    pub l1_policy: Option<L1TmpfsPolicy>,
    /// Fail checkpoints fail-closed when `TrackedSetLeak` triggers.
    pub strict_fail_closed: bool,
    /// Tracked-set scan policy (`Strict` by default).
    pub tracked_set_mode: ShadowTrackedSetMode,
}

impl D6ShadowOptions {
    /// Create dual-track shadow options with strict leak checking and default scan mode.
    pub fn new(
        shadow_root: impl Into<PathBuf>,
        cas: Arc<CasMerkleStore>,
        ledger: DynRefLedger,
    ) -> Self {
        Self {
            shadow_root: shadow_root.into(),
            cas,
            ledger,
            l1_policy: None,
            strict_fail_closed: false,
            tracked_set_mode: ShadowTrackedSetMode::Strict,
        }
    }

    /// Configure an optional L1 `tmpfs` policy on the shadow store.
    #[must_use]
    pub fn with_l1_policy(mut self, policy: L1TmpfsPolicy) -> Self {
        self.l1_policy = Some(policy);
        self
    }

    /// Configure fail-closed enforcement on `TrackedSetLeak`.
    #[must_use]
    pub const fn with_strict_fail_closed(mut self, enabled: bool) -> Self {
        self.strict_fail_closed = enabled;
        self
    }

    /// Override tracked-set scanning mode (for negative leak verification).
    #[must_use]
    pub const fn with_tracked_set_mode(mut self, mode: ShadowTrackedSetMode) -> Self {
        self.tracked_set_mode = mode;
        self
    }
}

impl std::fmt::Debug for D6ShadowOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("D6ShadowOptions")
            .field("shadow_root", &self.shadow_root)
            .field("cas_root", &self.cas.cas().root())
            .field("l1_policy", &self.l1_policy)
            .field("strict_fail_closed", &self.strict_fail_closed)
            .field("tracked_set_mode", &self.tracked_set_mode)
            .finish_non_exhaustive()
    }
}

/// The directory under a store's root that holds the CAS of deduplicated
/// build artifacts ([`LocalWorkspaceStore::cas_root`]).
pub const CAS_DIRECTORY: &str = ".cas";

/// The file under a workspace root that names the one backend its
/// workspaces run on ([`claim_root`]).
pub const BACKEND_MARKER: &str = ".spawn-workspace-backend";

/// The five backends a [`LocalWorkspaceStore`] can be, by name. The one
/// place a backend is chosen ([`WorkspaceBackend::auto_with`]): the
/// constructors here and `spawn`'s `SPAWN_WORKSPACE_BACKEND` both use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkspaceBackend {
    /// [`GitWorkspace`].
    Git,
    /// [`CowWorkspace`].
    Cow,
    /// `OverlayWorkspace` (Linux).
    Overlay,
    /// Dual-track oracle wrapping authoritative [`GitWorkspace`] beside shadow [`D6Workspace`].
    D6Shadow,
    /// Pure-Rust CAS Merkle + transactional ledger workspace store ([`D6Workspace`]).
    D6,
}

impl WorkspaceBackend {
    /// `"git"`, `"cow"`, `"overlay"`, `"d6-shadow"` or `"d6"`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Cow => "cow",
            Self::Overlay => "overlay",
            Self::D6Shadow => "d6-shadow",
            Self::D6 => "d6",
        }
    }

    /// The backend `name` names ([`Self::name`]), exactly.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "git" => Some(Self::Git),
            "cow" => Some(Self::Cow),
            "overlay" => Some(Self::Overlay),
            "d6-shadow" => Some(Self::D6Shadow),
            "d6" => Some(Self::D6),
            _ => None,
        }
    }

    /// The backend "the cheapest supported" means, given whether the root's
    /// file system clones copy-on-write: Cow where it does, else Git. Never
    /// the overlay and never opt-in `d6`/`d6-shadow`.
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

/// Whether `requested` may safely share a root already initialized by `existing`.
///
/// Pure `d6` roots never mix with `git`/`cow`/`overlay`/`d6-shadow` without explicit cutover,
/// while opt-in `d6-shadow` safely wraps an authoritative `git` root (and toggling `d6-shadow`
/// back to `git` preserves the root).
pub const fn backends_compatible(existing: WorkspaceBackend, requested: WorkspaceBackend) -> bool {
    matches!(
        (existing, requested),
        (
            WorkspaceBackend::Git | WorkspaceBackend::D6Shadow,
            WorkspaceBackend::Git | WorkspaceBackend::D6Shadow
        ) | (WorkspaceBackend::Cow, WorkspaceBackend::Cow)
            | (WorkspaceBackend::Overlay, WorkspaceBackend::Overlay)
            | (WorkspaceBackend::D6, WorkspaceBackend::D6)
    )
}

/// The backend `root`'s workspaces run on, read and never written: the one
/// its [`BACKEND_MARKER`] names, else the one what is there shows (a root
/// from before the marker: `.cow/` is Cow's, `.overlay/` the overlay's, `.d6/`
/// native D6's, `.d6-shadow/` shadow D6's, any workspace directory Git's),
/// else `None` (a new or empty root). A marker that names no backend is an error.
pub fn root_backend(root: &Path) -> std::io::Result<Option<WorkspaceBackend>> {
    match std::fs::read_to_string(root.join(BACKEND_MARKER)) {
        Ok(text) => {
            return WorkspaceBackend::parse(text.trim())
                .map(Some)
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "{} names no workspace backend: {:?}",
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
    let mut shadow_dir = false;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        match name.to_str() {
            Some(".cow") => return Ok(Some(WorkspaceBackend::Cow)),
            Some(".overlay") => return Ok(Some(WorkspaceBackend::Overlay)),
            Some(".d6") => return Ok(Some(WorkspaceBackend::D6)),
            Some(".d6-shadow") => shadow_dir = true,
            _ => {}
        }
        workspaces |= !name.as_encoded_bytes().starts_with(b".") && entry.file_type()?.is_dir();
    }
    if shadow_dir {
        return Ok(Some(WorkspaceBackend::D6Shadow));
    }
    Ok(workspaces.then_some(WorkspaceBackend::Git))
}

/// Claim `root` for `backend`: the first store of any process to use the
/// root writes its [`BACKEND_MARKER`] (made beside it and linked into
/// place, so two processes racing to claim it see one winner), and every
/// later one must ask for a compatible backend ([`backends_compatible`]). Refused,
/// with nothing written, when the root belongs to an incompatible backend
/// ([`root_backend`]): pure `git` and pure `d6` never share a root without
/// an explicit cutover, while `d6-shadow` can safely run over a `git` root.
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
        let cutover_hint = if matches!(
            (there, backend),
            (
                WorkspaceBackend::Git | WorkspaceBackend::D6Shadow,
                WorkspaceBackend::D6
            ) | (
                WorkspaceBackend::D6,
                WorkspaceBackend::Git | WorkspaceBackend::D6Shadow
            )
        ) {
            " (complete D6 project cutover before switching between git and pure d6, or use another SPAWN_WORKSPACE_ROOT)"
        } else {
            " or use another SPAWN_WORKSPACE_ROOT"
        };
        format!(
            "workspace root {} is the {} backend's ({why}); refusing to run the {} backend on \
             it: set SPAWN_WORKSPACE_BACKEND={} (or auto, which follows the root){cutover_hint}",
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
    let staged = root.join(format!("{BACKEND_MARKER}.{}", crate::cow::unique_suffix()));
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

/// The workspace store of this machine: Git working copies (the default),
/// copy-on-write clones of a per-commit baseline, on Linux an unprivileged
/// overlay over a per-commit snapshot layer, dual-track `d6-shadow` over Git,
/// or native pure-Rust `d6`.
#[derive(Debug)]
#[expect(clippy::large_enum_variant)]
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
    /// Dual-track oracle running authoritative [`GitWorkspace`] with shadow [`D6Workspace`].
    D6Shadow(ShadowD6Workspace<GitWorkspace, DynRefLedger>),
    /// Pure-Rust CAS Merkle + transactional ledger workspace store ([`D6Workspace`]).
    D6(D6Workspace<DynRefLedger>),
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

    /// Pure-Rust CAS Merkle + ledger workspace store (`D6Workspace`) for `project`
    /// with workspaces under `root`.
    pub fn d6(project: impl Into<String>, root: impl AsRef<Path>, options: D6StoreOptions) -> Self {
        let mut store = D6Workspace::new(project, root, options.cas, Arc::new(options.ledger));
        if let Some(policy) = options.l1_policy {
            store = store.with_l1_policy(policy);
        }
        Self::D6(store)
    }

    /// Dual-track `d6-shadow` store wrapping authoritative [`GitWorkspace`] over
    /// `repo` with workspaces under `root` and shadow state in `options.shadow_root`.
    pub fn d6_shadow(
        repo: impl AsRef<Path>,
        root: impl AsRef<Path>,
        options: D6ShadowOptions,
    ) -> Self {
        let repo = repo.as_ref();
        let primary = GitWorkspace::new(repo, root);
        let mut shadow = D6Workspace::new(
            primary.project().to_string(),
            options.shadow_root,
            options.cas,
            Arc::new(options.ledger),
        );
        if let Some(policy) = options.l1_policy {
            shadow = shadow.with_l1_policy(policy);
        }
        let oracle = ShadowD6Workspace::with_git_repo(primary, shadow, repo)
            .with_strict_fail_closed(options.strict_fail_closed)
            .with_tracked_set_mode(options.tracked_set_mode);
        Self::D6Shadow(oracle)
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
    /// overlay only where the kernel mounts one ([`Self::overlay`]). Opt-in
    /// D6 backends require CAS + ledger configuration via [`Self::d6_shadow`]
    /// or [`Self::d6`].
    pub fn open(
        backend: WorkspaceBackend,
        repo: impl AsRef<Path>,
        root: impl AsRef<Path>,
    ) -> Result<Self, String> {
        match backend {
            WorkspaceBackend::Git => Ok(Self::git(repo, root)),
            WorkspaceBackend::Cow => Ok(Self::cow(repo, root)),
            WorkspaceBackend::Overlay => Self::overlay(repo, root),
            WorkspaceBackend::D6Shadow | WorkspaceBackend::D6 => Err(format!(
                "workspace backend {} requires CAS and RefLedger configuration (use \
                 LocalWorkspaceStore::{} or WorkspaceSettings)",
                backend.name(),
                if backend == WorkspaceBackend::D6 {
                    "d6"
                } else {
                    "d6_shadow"
                }
            )),
        }
    }

    /// Which backend this is.
    pub fn backend(&self) -> WorkspaceBackend {
        match self {
            Self::Git(_) => WorkspaceBackend::Git,
            Self::Cow(_) => WorkspaceBackend::Cow,
            #[cfg(target_os = "linux")]
            Self::Overlay(_) => WorkspaceBackend::Overlay,
            Self::D6Shadow(_) => WorkspaceBackend::D6Shadow,
            Self::D6(_) => WorkspaceBackend::D6,
        }
    }

    /// Which backend this is: `"git"`, `"cow"`, `"overlay"`, `"d6-shadow"` or `"d6"`.
    pub fn backend_name(&self) -> &'static str {
        self.backend().name()
    }

    /// Oracle metrics snapshot when backed by [`Self::D6Shadow`].
    pub fn shadow_metrics(&self) -> Option<ShadowOracleMetrics> {
        match self {
            Self::D6Shadow(oracle) => Some(oracle.metrics()),
            _ => None,
        }
    }

    /// Reserved L1 `tmpfs` bytes across mounted branches when backed by D6 / D6-shadow.
    pub fn d6_l1_reserved_bytes(&self) -> Option<u64> {
        match self {
            Self::D6(store) => Some(store.l1_total_reserved_bytes()),
            Self::D6Shadow(oracle) => Some(oracle.shadow().l1_total_reserved_bytes()),
            _ => None,
        }
    }

    /// Borrow the underlying [`D6Workspace`] when backed by [`Self::D6`].
    pub const fn d6_workspace(&self) -> Option<&D6Workspace<DynRefLedger>> {
        match self {
            Self::D6(store) => Some(store),
            _ => None,
        }
    }

    /// Borrow the underlying [`ShadowD6Workspace`] when backed by [`Self::D6Shadow`].
    pub const fn d6_shadow_workspace(
        &self,
    ) -> Option<&ShadowD6Workspace<GitWorkspace, DynRefLedger>> {
        match self {
            Self::D6Shadow(oracle) => Some(oracle),
            _ => None,
        }
    }

    /// Claim this store's root for its backend ([`claim_root`]).
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
        #[cfg(target_os = "linux")]
        let (swept, warnings) = match crate::overlay::sweep_probes(self.root()) {
            Ok(swept) => (
                swept.removed,
                swept
                    .kept
                    .iter()
                    .map(|(probe, why)| format!("overlay probe {} kept: {why}", probe.display()))
                    .collect(),
            ),
            Err(error) => (0, vec![format!("sweep overlay probes: {error}")]),
        };
        #[cfg(not(target_os = "linux"))]
        let (swept, warnings) = (0, Vec::new());
        let pruned = match self {
            Self::Git(_) | Self::D6Shadow(_) | Self::D6(_) => 0,
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
            Self::D6Shadow(store) => store.primary().root(),
            Self::D6(store) => store.root(),
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

    /// Discard the mounted `workspace` directory cleanly (and any L1/shadow state).
    pub fn discard(&self, workspace: &WorkspaceId) -> Result<(), WorkspaceError> {
        match self {
            Self::D6Shadow(store) => store.discard(workspace),
            Self::D6(store) => store.discard(workspace),
            _ => self.inner().unmount(workspace),
        }
    }

    /// [`WorkspaceStore::set_aside`], only while `ready` says the agent is
    /// idle, asked under the branch's lock before anything is written
    /// ([`GitWorkspace::set_aside_when`], [`CowWorkspace::set_aside_when`],
    /// [`OverlayWorkspace::set_aside_when`], [`D6Workspace::set_aside_when`],
    /// [`ShadowD6Workspace::set_aside_when`]: one rule).
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
            Self::D6Shadow(store) => store.set_aside_when(branch, start, message, ready),
            Self::D6(store) => store.set_aside_when(branch, start, message, ready),
        }
    }

    /// Commit the workspace, asking `take_back` first, under the branch's
    /// lock, when its branch is gone ([`GitWorkspace::commit_or_take_back`]
    /// and its Cow, overlay, `D6Shadow` and `D6` twins).
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
            Self::D6Shadow(store) => store.commit_or_take_back(workspace, message, take_back),
            Self::D6(store) => store.commit_or_take_back(workspace, message, take_back),
        }
    }

    /// Persists worker-authored `.git` commits in `directory` into
    /// `refs/spawn/workers/<branch>/head` and `target_refs` (`#103`, `#110`).
    pub fn pin_worker_head(
        &self,
        branch: &str,
        directory: Option<&Path>,
        target_refs: &[&str],
    ) -> Result<Option<CommitId>, WorkspaceError> {
        match self {
            Self::Git(store) => store.pin_worker_head(branch, directory, target_refs),
            Self::Cow(store) => store.pin_worker_head(branch, directory, target_refs),
            Self::D6Shadow(store) => {
                store
                    .primary()
                    .pin_worker_head(branch, directory, target_refs)
            }
            #[cfg(target_os = "linux")]
            Self::Overlay(_) => Ok(None),
            Self::D6(_) => Ok(None),
        }
    }

    /// Files `commit` changes against `base` that hold an unresolved conflict marker block.
    pub fn conflict_markers(
        &self,
        base: &str,
        commit: &str,
    ) -> Result<Vec<String>, WorkspaceError> {
        match self {
            Self::Git(store) => store.conflict_markers(base, commit),
            Self::Cow(store) => store.git_store().conflict_markers(base, commit),
            #[cfg(target_os = "linux")]
            Self::Overlay(store) => store.git_store().conflict_markers(base, commit),
            Self::D6Shadow(oracle) => oracle.primary().conflict_markers(base, commit),
            Self::D6(store) => store.conflict_markers(base, commit),
        }
    }

    /// Unified patch turning `from` into `to` for Git-backed stores.
    pub fn patch_between(&self, from: &str, to: &str) -> Result<String, WorkspaceError> {
        match self {
            Self::Git(store) => store.patch_between(from, to),
            Self::Cow(store) => store.git_store().patch_between(from, to),
            #[cfg(target_os = "linux")]
            Self::Overlay(store) => store.git_store().patch_between(from, to),
            Self::D6Shadow(oracle) => oracle.primary().patch_between(from, to),
            Self::D6(_) => Err(WorkspaceError::Unavailable(
                "D6 workspace does not render textual git patches".to_string(),
            )),
        }
    }

    /// Non-marker conflict `main` copies recorded for `branch`, if any.
    pub fn main_copies(&self, branch: &str) -> Option<(String, Vec<String>)> {
        match self {
            Self::Git(store) => store.main_copies(branch),
            Self::Cow(store) => store.git_store().main_copies(branch),
            #[cfg(target_os = "linux")]
            Self::Overlay(store) => store.git_store().main_copies(branch),
            Self::D6Shadow(oracle) => oracle.primary().main_copies(branch),
            Self::D6(store) => store.main_copies(branch),
        }
    }

    /// Whether a merge of `main` into `branch` is waiting on conflict marker resolution.
    pub fn has_pending_merge(&self, branch: &str) -> bool {
        match self {
            Self::Git(store) => store.has_pending_merge(branch),
            Self::Cow(store) => store.git_store().has_pending_merge(branch),
            #[cfg(target_os = "linux")]
            Self::Overlay(store) => store.git_store().has_pending_merge(branch),
            Self::D6Shadow(oracle) => oracle.primary().has_pending_merge(branch),
            Self::D6(store) => store.has_pending_merge(branch),
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
    /// `node_modules/`) against [`Self::cas_root`],
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
            Self::D6Shadow(store) => store,
            Self::D6(store) => store,
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

impl From<D6Workspace<DynRefLedger>> for LocalWorkspaceStore {
    fn from(store: D6Workspace<DynRefLedger>) -> Self {
        Self::D6(store)
    }
}

impl From<ShadowD6Workspace<GitWorkspace, DynRefLedger>> for LocalWorkspaceStore {
    fn from(store: ShadowD6Workspace<GitWorkspace, DynRefLedger>) -> Self {
        Self::D6Shadow(store)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn backend_parse_and_auto_stay_opt_in_only() {
        for (backend, expected) in [
            (WorkspaceBackend::Git, "git"),
            (WorkspaceBackend::Cow, "cow"),
            (WorkspaceBackend::Overlay, "overlay"),
            (WorkspaceBackend::D6Shadow, "d6-shadow"),
            (WorkspaceBackend::D6, "d6"),
        ] {
            assert_eq!(backend.name(), expected);
            assert_eq!(WorkspaceBackend::parse(expected), Some(backend));
        }
        assert_eq!(WorkspaceBackend::parse("auto"), None);
        assert_eq!(WorkspaceBackend::parse("d6_shadow"), None);
        assert_eq!(WorkspaceBackend::auto_with(true), WorkspaceBackend::Cow);
        assert_eq!(WorkspaceBackend::auto_with(false), WorkspaceBackend::Git);
        let dir = scratch_dir("auto");
        assert!(matches!(
            WorkspaceBackend::auto(&dir),
            WorkspaceBackend::Cow | WorkspaceBackend::Git
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claim_root_refuses_incompatible_d6_mixes_and_allows_shadow_over_git() {
        let base = scratch_dir("claim");

        // 1. Marked Git root allows d6-shadow (keeping "git" marker), refuses pure d6 & cow.
        let git_root = base.join("git");
        claim_root(&git_root, WorkspaceBackend::Git).expect("claim git");
        claim_root(&git_root, WorkspaceBackend::D6Shadow).expect("d6-shadow compatible with git");
        assert_eq!(
            root_backend(&git_root).expect("read"),
            Some(WorkspaceBackend::Git)
        );
        let err = claim_root(&git_root, WorkspaceBackend::D6).expect_err("git vs d6 refused");
        assert!(err.contains("cutover"), "{err}");
        assert!(claim_root(&git_root, WorkspaceBackend::Cow).is_err());

        // 2. Unmarked legacy Git root allows d6-shadow (claiming as "git") and refuses d6.
        let legacy_git = base.join("legacy-git");
        std::fs::create_dir_all(legacy_git.join("agent-1")).expect("agent dir");
        assert_eq!(
            root_backend(&legacy_git).expect("legacy"),
            Some(WorkspaceBackend::Git)
        );
        assert!(claim_root(&legacy_git, WorkspaceBackend::D6).is_err());
        assert!(!legacy_git.join(BACKEND_MARKER).exists());
        claim_root(&legacy_git, WorkspaceBackend::D6Shadow).expect("shadow over legacy git");
        assert_eq!(
            root_backend(&legacy_git).expect("claimed as git"),
            Some(WorkspaceBackend::Git)
        );

        // 3. Pure D6 root (marked or unmarked .d6/) refuses git, cow, overlay, d6-shadow.
        let d6_root = base.join("d6");
        claim_root(&d6_root, WorkspaceBackend::D6).expect("claim d6");
        for other in [
            WorkspaceBackend::Git,
            WorkspaceBackend::Cow,
            WorkspaceBackend::Overlay,
            WorkspaceBackend::D6Shadow,
        ] {
            assert!(claim_root(&d6_root, other).is_err(), "{other:?} vs d6");
        }
        let d6_unmarked = base.join("d6-unmarked");
        std::fs::create_dir_all(d6_unmarked.join(".d6")).expect("mkdir .d6");
        assert_eq!(
            root_backend(&d6_unmarked).expect("unmarked d6"),
            Some(WorkspaceBackend::D6)
        );
        assert!(claim_root(&d6_unmarked, WorkspaceBackend::Git).is_err());

        // 4. Fresh d6-shadow root permits returning to git, refuses d6 & cow.
        let shadow_root = base.join("d6-shadow");
        claim_root(&shadow_root, WorkspaceBackend::D6Shadow).expect("claim shadow");
        claim_root(&shadow_root, WorkspaceBackend::Git).expect("git compatible with d6-shadow");
        assert!(claim_root(&shadow_root, WorkspaceBackend::D6).is_err());
        assert!(claim_root(&shadow_root, WorkspaceBackend::Cow).is_err());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn open_requires_d6_options_and_never_falls_back_to_git() {
        let base = scratch_dir("open");
        for b in [WorkspaceBackend::D6Shadow, WorkspaceBackend::D6] {
            let err = LocalWorkspaceStore::open(b, base.join("r.git"), &base)
                .expect_err("requires options");
            assert!(err.contains(b.name()), "{err}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
