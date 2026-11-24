//! LayerFS v2 Track A: an opt-in workspace store that serves each agent's
//! files through an unprivileged Linux `overlayfs` instead of a full
//! working copy (see `docs/layerfs-v2-overlay.md`). Nothing selects it by
//! default: it is a [`WorkspaceStore`] of its own, over a root of its own,
//! and `spawn-cli` still builds [`crate::GitWorkspace`].
//!
//! Durable state never lives in an overlay. Refs and commits stay in the
//! backing bare repository, and transcripts are filed outside the store
//! root. An overlay holds only an agent's uncommitted edits, so tearing one
//! down loses nothing a checkpoint took.
//!
//! Layout under the store root (resolved through its symlinks once):
//!
//! ```text
//! <root>/.layers/<commit>/      Layer 0: read-only snapshot of a commit, shared
//! <root>/.overlay/<hex branch>/ Layer 1: one agent's state (the branch name
//!                               in hex, so `a` and `a/b` never nest)
//!     upper/  work/             overlayfs upperdir and workdir
//!     lower                     the commit the snapshot layer is (diff base)
//!     head                      the branch head this workspace last published
//!     pending                   "<old> <new>": a publication in flight
//!     holder                    "<pid> <mnt namespace> <start time>"
//! <root>/.locks/<hex branch>    the workspace lock (mount, commit, unmount)
//! <root>/.locks/<hex>.lease     the admission lease of launched clients
//! <root>/<branch>               the merged mount point (the workspace id)
//! ```
//!
//! The mount is made in a user and mount namespace of its own (`unshare -r
//! -m`), held by a sleeping holder process. Processes enter it through
//! [`OverlayWorkspace::command`], whose `flock` supervisor holds the
//! workspace's admission lease for as long as the launched program lives;
//! the host reads and writes the merged view through
//! `/proc/<holder>/root` ([`OverlayWorkspace::host_path`]).
//! [`WorkspaceStore::commit`] never reads the merged view: it walks the
//! upperdir alone, so a checkpoint costs O(|diff|), not O(|tree|).
//!
//! Teardown and recovery never guess from `/proc`: the kernel itself says
//! whether an overlay over a workdir is still alive (a probe overlay with
//! `index=on` over the same workdir inode is refused `EBUSY` while it is),
//! see `docs/layerfs-v2-overlay.md`, "The in-use proof".

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use spawn_core::id::{BranchName, CommitId, WorkspaceId};
use spawn_core::port::{Branch, Commit, WORKSPACE_STALE, WorkspaceError, WorkspaceStore};

use crate::git::{GitWorkspace, git_command, pathspec_batches, report, report_listing};

/// Directories a checkpoint leaves out by default, at any depth: build
/// output, installed dependencies and Spawn's own runtime files. They may
/// still be pre-warmed as read-only lower layers
/// ([`OverlayWorkspace::with_lower_layers`]).
pub const DEFAULT_DERIVED_DIRS: &[&str] = &["target", "node_modules", ".spawn"];

/// The overlayfs mount options besides the directories. `userxattr` keeps
/// whiteout and opaque markers in `user.overlay.*`, which a user namespace
/// may write. `redirect_dir=nofollow` means a renamed lower directory is
/// copied rather than redirected, so the upperdir holds no redirects to
/// follow. (`redirect_dir=off` is refused inside a user namespace.)
/// `metacopy=off` means a changed file is always copied up whole, and
/// `index=off` means no hard-link index is kept.
pub const MOUNT_OPTIONS: &str = "userxattr,redirect_dir=nofollow,metacopy=off,index=off";

/// The [`WorkspaceError::Refused`] code of an unmount that found a
/// transcript (`*.jsonl`) in the upperdir. Transcripts belong outside the
/// overlay, so the upperdir is kept rather than deleted.
pub const TRANSCRIPT_IN_UPPER: &str = "TranscriptInUpper";

/// The [`WorkspaceError::Refused`] code of an unmount that could not read
/// all of the upperdir (a directory it may not list, an entry it cannot
/// stat), so could not prove it holds no transcript. Nothing is deleted
/// and no permission is touched.
pub const UPPER_UNREADABLE: &str = "UpperUnreadable";

/// The [`WorkspaceError::Refused`] code of an unmount or a mount refused
/// because the workspace is still in use: a client launched through
/// [`OverlayWorkspace::command`] still holds its admission lease (the
/// workspace stays mounted), or, once the holder is stopped, the kernel
/// still holds the overlay over its workdir (a process inside the
/// namespace, a working directory, open file or mapping through
/// [`OverlayWorkspace::host_path`], a descriptor in flight). The upperdir,
/// workdir and state are kept whole, and a later mount refuses too while
/// the old overlay lives.
pub const IN_USE: &str = "InUse";

/// The [`WorkspaceError::Refused`] code of an unmount or a mount refused
/// because the kernel could not be asked whether this workspace's overlay
/// is still alive (the probe mount failed some other way than `EBUSY`).
/// It concerns this workspace only, and nothing is changed.
pub const IN_USE_UNPROVEN: &str = "InUseUnproven";

/// The [`WorkspaceError::Refused`] code of a state that cannot be read
/// whole: a [`OverlayWorkspace::prune_layers`] that cannot tell which layers
/// a workspace uses, or a mount over a state that holds work but lost its
/// `lower` marker (never taken for a fresh start). Nothing is removed.
pub const STATE_UNREADABLE: &str = "StateUnreadable";

/// The [`WorkspaceError::Refused`] code of a [`OverlayWorkspace::prune_layers`]
/// that found, in a snapshot layer, a `*.jsonl` that is not byte for byte
/// the file the layer's commit tracks at that path. A layer is a git
/// snapshot, so such a file came from elsewhere: it may be a transcript,
/// and the layer is kept whole.
pub const TRANSCRIPT_IN_LAYER: &str = "TranscriptInLayer";

/// `statfs` magic of overlayfs.
const OVERLAYFS_SUPER_MAGIC: i64 = 0x794c_7630;

/// How long the workdir probe retries a busy or unknown answer: the last
/// mount of a stopped holder's namespace is released as it exits.
const WORK_LOCK_PATIENCE: std::time::Duration = std::time::Duration::from_secs(2);

/// Exit code of a launch refused because a teardown holds the workspace's
/// admission lease ([`OverlayWorkspace::launch_arguments`]).
pub const LAUNCH_REFUSED: i32 = 75;

/// Exit code of a launch whose recorded holder is no longer the process
/// that holds the overlay (stopped, or its pid reused).
pub const LAUNCH_STALE: i32 = 76;

/// Whether this host can mount an overlay without privileges: a user and
/// mount namespace, an overlay mount in it, a write that copies up, a
/// delete that leaves a whiteout the host can read, and the kernel's
/// workdir exclusion that teardown asks ([`probe_work_lock`]): a probe
/// overlay over the busy workdir must be refused with the busy error
/// (nothing else will do), and one over the same workdir, once free, must
/// be taken. Probed under [`std::env::temp_dir`].
pub fn overlay_supported() -> Result<(), String> {
    overlay_supported_in(&std::env::temp_dir())
}

/// [`overlay_supported`], probed in a new directory under `parent`.
/// Support depends on the filesystem under the upperdir (`user.*`
/// xattrs), so probe where the store keeps its state.
pub fn overlay_supported_in(parent: &Path) -> Result<(), String> {
    let probe = parent.join(format!(
        ".spawn-overlay-probe-{}-{}",
        std::process::id(),
        next_ordinal()
    ));
    let result = probe_in(&probe);
    let _ = remove_state(&probe);
    result
}

fn probe_in(probe: &Path) -> Result<(), String> {
    let failed = |what: &str, error: std::io::Error| format!("{what}: {error}");
    for name in [
        "lower",
        "upper",
        "work",
        "merged",
        "second/lower",
        "second/upper",
        "second/merged",
    ] {
        std::fs::create_dir_all(probe.join(name))
            .map_err(|error| failed(&format!("create {}", probe.display()), error))?;
    }
    std::fs::write(probe.join("lower/kept"), b"kept").map_err(|e| failed("write lower", e))?;
    std::fs::write(probe.join("lower/gone"), b"gone").map_err(|e| failed("write lower", e))?;
    let options = mount_options(
        &[probe.join("lower")],
        &probe.join("upper"),
        &probe.join("work"),
    )
    .map_err(|error| error.to_string())?;
    let busy = probe.join("second");
    let second =
        work_lock_options(&busy, &probe.join("work")).map_err(|error| error.to_string())?;
    // The probe of the busy workdir runs while the overlay is mounted; its
    // answer is written down and classified below, as teardown would.
    let script = r#"mount -t overlay overlay -o "$1" "$2" || exit 97
printf new > "$2/new" && rm "$2/gone" && cat "$2/kept" >/dev/null || exit 1
mount -t overlay overlay -o "$3" "$4/merged" 2>"$4/stderr"
echo $? > "$4/status""#;
    let output = git_command("unshare")
        .args(["--user", "--map-root-user", "--mount", "sh", "-c", script])
        .arg("spawn-overlay-probe")
        .arg(&options)
        .arg(probe.join("merged"))
        .arg(&second)
        .arg(&busy)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| failed("run unshare", e))?;
    if !output.status.success() {
        return Err(format!(
            "unprivileged overlay mount failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let new = std::fs::read(probe.join("upper/new")).map_err(|e| failed("read upper", e))?;
    if new != b"new" {
        return Err("a write through the overlay did not reach the upperdir".to_string());
    }
    let gone = std::fs::symlink_metadata(probe.join("upper/gone"))
        .map_err(|e| failed("read whiteout", e))?;
    if !is_whiteout(&gone) {
        return Err("a delete through the overlay left no 0:0 whiteout".to_string());
    }
    if std::fs::symlink_metadata(probe.join("upper/kept")).is_ok() {
        return Err("a read through the overlay copied the file up".to_string());
    }
    let status = std::fs::read_to_string(busy.join("status")).map_err(|e| failed("read", e))?;
    let stderr = std::fs::read_to_string(busy.join("stderr")).map_err(|e| failed("read", e))?;
    match classify(status.trim().parse().ok(), &stderr, &busy.join("merged")) {
        WorkLock::Busy(_) => {}
        other => {
            return Err(format!(
                "a probe overlay over a busy workdir was not refused as busy: {other:?}"
            ));
        }
    }
    match probe_work_lock(probe) {
        WorkLock::Free => Ok(()),
        other => Err(format!(
            "the workdir probe did not take the workdir once free: {other:?}"
        )),
    }
}

/// The `-o` argument of the overlay mount. A path holding a character the
/// option syntax reserves (`,` `:` `\`) or a newline is refused instead of
/// escaped.
fn mount_options(lowers: &[PathBuf], upper: &Path, work: &Path) -> Result<String, WorkspaceError> {
    let lowerdir = lowers
        .iter()
        .map(|path| option_path(path))
        .collect::<Result<Vec<_>, _>>()?
        .join(":");
    Ok(format!(
        "lowerdir={lowerdir},upperdir={},workdir={},{MOUNT_OPTIONS}",
        option_path(upper)?,
        option_path(work)?
    ))
}

/// The options of a probe overlay over `work`, with its own empty lower and
/// upper under `probe`. `index=on` makes the kernel refuse it (`EBUSY`)
/// while any overlay whose workdir is `work` is alive: overlayfs marks an
/// upperdir and a workdir in use for the whole life of the superblock,
/// which lasts while anything at all holds it (a mount in any namespace, a
/// working directory, an open file or mapping, a file descriptor in flight
/// in a unix socket, a process of any uid or one that is not dumpable).
fn work_lock_options(probe: &Path, work: &Path) -> Result<String, WorkspaceError> {
    Ok(format!(
        "lowerdir={},upperdir={},workdir={},userxattr,index=on",
        option_path(&probe.join("lower"))?,
        option_path(&probe.join("upper"))?,
        option_path(work)?
    ))
}

/// `path` as an overlay option value. A path holding a character the
/// option syntax reserves (`,` `:` `\`) or a newline is refused instead of
/// escaped.
fn option_path(path: &Path) -> Result<String, WorkspaceError> {
    let text = path
        .to_str()
        .ok_or_else(|| WorkspaceError::Unavailable(format!("{} is not UTF-8", path.display())))?;
    if text.contains([',', ':', '\\', '\n']) {
        return Err(WorkspaceError::Unavailable(format!(
            "{text} holds a character overlay options reserve"
        )));
    }
    Ok(text.to_string())
}

/// An overlayfs whiteout in its device form: a character device 0:0.
fn is_whiteout(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_char_device() && metadata.rdev() == 0
}

/// An overlayfs whiteout in its xattr form, as the kernel's lookup reads
/// it: an empty regular file carrying the file marker
/// `user.overlay.whiteout` (the namespace `userxattr` mounts read; this
/// store's mounts never read `trusted.*`). The directory's `opaque=x` mark
/// only makes readdir filter it; a lookup hides the lower file without it.
fn is_xattr_whiteout(path: &Path, metadata: &std::fs::Metadata) -> bool {
    metadata.is_file()
        && metadata.len() == 0
        && rustix::fs::lgetxattr(path, "user.overlay.whiteout", &mut [0u8; 64][..]).is_ok()
}

/// Whether a directory carries `user.overlay.opaque=y`: it hides everything
/// below it in the lower layers. (`x` marks one that holds xattr
/// whiteouts, and hides nothing itself.)
fn is_opaque(path: &Path) -> bool {
    let mut value = [0u8; 8];
    matches!(
        rustix::fs::lgetxattr(path, "user.overlay.opaque", &mut value[..]),
        Ok(length) if length > 0 && value[0] == b'y'
    )
}

/// Workspace store that serves each workspace through an unprivileged
/// overlay over a shared, read-only snapshot of a commit, backed by a bare
/// git repository for refs and commits.
#[derive(Debug)]
pub struct OverlayWorkspace {
    root: PathBuf,
    /// The repository's git ([`GitWorkspace`]): its commands, branch
    /// creation and resolution. It never makes a working copy here.
    git: GitWorkspace,
    /// Extra read-only layers stacked under the snapshot (pre-warmed
    /// dependencies, build output), highest first.
    lower_layers: Vec<PathBuf>,
    /// Directory names a checkpoint leaves out, at any depth.
    derived: Vec<OsString>,
    /// Holders this store started, by state directory.
    holders: Mutex<HashMap<PathBuf, Child>>,
    /// Whether the store root's filesystem passed [`overlay_supported_in`],
    /// asked once, before the first mount, recovery or teardown trusts the
    /// workdir probe there.
    supported: std::sync::OnceLock<Result<(), String>>,
    /// Code the unit tests run at named points of an operation, to put a
    /// racing process exactly where a check must catch it.
    #[cfg(test)]
    hooks: tests::Hooks,
}

/// A mount holder: its pid, the mount namespace it holds and its start
/// time. A pid is reused, and a freed namespace inode is handed to the next
/// namespace made: the start time (clock ticks since boot, in
/// `/proc/<pid>/stat`) is what a reused pid cannot share.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Holder {
    pid: u32,
    namespace: String,
    start: u64,
}

impl OverlayWorkspace {
    /// Store over the bare repository `repo`, keeping layers, state and
    /// mount points under `root`, resolved through its symlinks. `root`
    /// must be a root of its own, never a [`GitWorkspace`]'s.
    pub fn new(repo: impl AsRef<Path>, root: impl AsRef<Path>) -> Self {
        let root = crate::root::canonical_root(root.as_ref());
        Self {
            git: GitWorkspace::new(repo, &root),
            root,
            lower_layers: Vec::new(),
            derived: DEFAULT_DERIVED_DIRS.iter().map(OsString::from).collect(),
            holders: Mutex::new(HashMap::new()),
            supported: std::sync::OnceLock::new(),
            #[cfg(test)]
            hooks: tests::Hooks::default(),
        }
    }

    /// The same store with read-only layers stacked under every commit
    /// snapshot, highest first: a pre-warmed `target/` or `node_modules/`
    /// that every agent shares and none duplicates. Overlayfs never writes
    /// a lower layer. A write to one of its files copies the file up into
    /// that agent's upperdir.
    #[must_use]
    pub fn with_lower_layers(mut self, layers: impl IntoIterator<Item = PathBuf>) -> Self {
        self.lower_layers = layers.into_iter().collect();
        self
    }

    /// The same store leaving out these directory names, at any depth,
    /// instead of [`DEFAULT_DERIVED_DIRS`].
    #[must_use]
    pub fn with_derived_dirs<S: AsRef<OsStr>>(
        mut self,
        names: impl IntoIterator<Item = S>,
    ) -> Self {
        self.derived = names
            .into_iter()
            .map(|name| name.as_ref().to_os_string())
            .collect();
        self
    }

    /// The store root, resolved.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The shared snapshot layer of `commit`.
    pub fn layer_path(&self, commit: &str) -> PathBuf {
        self.root.join(".layers").join(commit)
    }

    /// The upperdir of `workspace`: exactly what its agent changed.
    pub fn upper_dir(&self, workspace: &WorkspaceId) -> Result<PathBuf, WorkspaceError> {
        Ok(self.state_dir(workspace)?.join("upper"))
    }

    /// The commit `workspace`'s snapshot layer holds: the base its upperdir
    /// is a diff against.
    pub fn lower_commit(&self, workspace: &WorkspaceId) -> Result<String, WorkspaceError> {
        read_line(&self.state_dir(workspace)?.join("lower"))
    }

    /// The pid of the process that holds `workspace`'s mount, when one is
    /// alive.
    pub fn holder_pid(&self, workspace: &WorkspaceId) -> Option<u32> {
        let state = self.state_dir(workspace).ok()?;
        self.live_holder(&state).map(|holder| holder.pid)
    }

    /// The merged view of `workspace` as the host reaches it: the mount
    /// point inside the holder's namespace, through `/proc/<pid>/root`.
    /// Reads and writes there go through the overlay as an agent's do. A
    /// process that stays there holds no lease: teardown stops the holder
    /// under it and then refuses ([`IN_USE`]) while it still holds the
    /// overlay, keeping its data.
    pub fn host_path(&self, workspace: &WorkspaceId) -> Result<PathBuf, WorkspaceError> {
        let state = self.state_dir(workspace)?;
        let holder = self
            .live_holder(&state)
            .ok_or_else(|| WorkspaceError::Unavailable(format!("{workspace} is not mounted")))?;
        let merged = self.merged_dir(workspace)?;
        let relative = merged.strip_prefix("/").unwrap_or(&merged);
        Ok(PathBuf::from(format!("/proc/{}/root", holder.pid)).join(relative))
    }

    /// A command that runs `program` inside `workspace` ([`Self::launch_arguments`]):
    /// in the holder's user and mount namespaces (uid 0 there, the caller's
    /// uid on the host), started in the merged directory, holding the
    /// workspace's admission lease while it lives. Its environment is the
    /// caller's: nothing is set for it.
    pub fn command(
        &self,
        workspace: &WorkspaceId,
        program: impl AsRef<OsStr>,
    ) -> Result<Command, WorkspaceError> {
        let mut arguments = self.launch_arguments(workspace)?.into_iter();
        let mut command = Command::new(arguments.next().unwrap_or_default());
        command.args(arguments).arg(program);
        Ok(command)
    }

    /// The whole launch prefix that enters `workspace`, program first:
    /// everything before the program and its arguments. A terminal host
    /// (tmux) starts its engine with this prefix, so the engine sees the
    /// merged view at the workspace's path, files its transcript outside
    /// the overlay at the same paths as on the host, and holds the lease.
    ///
    /// In order: `flock --shared --nonblock` takes the lease before anything
    /// else ([`LAUNCH_REFUSED`] while a teardown holds it exclusively) and
    /// stays as the supervisor of the program, holding the lease until the
    /// program exits, whatever descriptors the program closes. Only then
    /// does a shell read the holder record, open (pin) the holder's user
    /// and mount namespaces, and check, after opening them, that the
    /// recorded pid still runs with its recorded start time and that the
    /// pinned mount namespace is the recorded one ([`LAUNCH_STALE`] if
    /// not): a pid reused at any point is never entered. `nsenter` enters
    /// the pinned namespaces, never a pid, and a shell inside changes into
    /// the merged directory (`nsenter --wd` would open the empty host mount
    /// point instead), closes the pins and runs the program.
    pub fn launch_arguments(
        &self,
        workspace: &WorkspaceId,
    ) -> Result<Vec<OsString>, WorkspaceError> {
        let state = self.state_dir(workspace)?;
        self.live_holder(&state)
            .ok_or_else(|| WorkspaceError::Unavailable(format!("{workspace} is not mounted")))?;
        let merged = self.merged_dir(workspace)?;
        // `unshare -r` denies setgroups: `--preserve-credentials` keeps the
        // credentials, which already map to uid 0 in the namespace.
        let enter = format!(
            "stale() {{ echo 'spawn: the overlay holder of this workspace is gone' >&2; \
             exit {LAUNCH_STALE}; }}
read pid ns start < \"$1\" || stale
command exec 3<\"/proc/$pid/ns/user\" 4<\"/proc/$pid/ns/mnt\" 2>/dev/null || stale
[ \"$(readlink /proc/self/fd/4)\" = \"$ns\" ] || stale
[ \"$(sed 's/.*) //' \"/proc/$pid/stat\" 2>/dev/null | cut -d' ' -f20)\" = \"$start\" ] \
|| stale
dir=$2
shift 2
exec nsenter --user=/proc/self/fd/3 --mount=/proc/self/fd/4 --preserve-credentials -- \
sh -c 'cd \"$0\" || exit 1; exec 3<&- 4<&-; exec \"$@\"' \"$dir\" \"$@\""
        );
        Ok(vec![
            OsString::from("flock"),
            OsString::from("--shared"),
            OsString::from("--nonblock"),
            OsString::from(format!("--conflict-exit-code={LAUNCH_REFUSED}")),
            self.lease_path(&state).into_os_string(),
            OsString::from("sh"),
            OsString::from("-c"),
            OsString::from(enter),
            OsString::from("spawn-overlay-enter"),
            state.join("holder").into_os_string(),
            merged.into_os_string(),
        ])
    }

    /// Remove every snapshot layer no workspace's `lower` names, and what
    /// an interrupted build or prune left in `.layers`. Returns how many
    /// layers were removed.
    ///
    /// Holds the store lock exclusively, so no mount builds or picks a layer
    /// meanwhile. A state it cannot read refuses the whole prune
    /// ([`STATE_UNREADABLE`]) before anything is removed: it cannot tell
    /// which layer that workspace uses. A layer is first renamed aside in
    /// one step, so a layer half deleted is never found under its commit's
    /// name (and never mounted). A `*.jsonl` in a layer is deleted only when
    /// it is byte for byte the file the commit tracks at that path, so the
    /// commit still holds it. Any other one refuses ([`TRANSCRIPT_IN_LAYER`])
    /// and keeps that layer whole, set aside as `.kept-<commit>-…` (never
    /// pruned, for a person to look at), so the next mount builds the
    /// commit's own layer again. The other layers are still pruned.
    pub fn prune_layers(&self) -> Result<usize, WorkspaceError> {
        let _lock = self.lock_store(true)?;
        let used = collect_lowers(&self.root.join(".overlay"))?;
        let layers = self.root.join(".layers");
        let Ok(entries) = std::fs::read_dir(&layers) else {
            return Ok(0);
        };
        let failed = |path: &Path, error: std::io::Error| {
            WorkspaceError::Unavailable(format!("remove {}: {error}", path.display()))
        };
        let mut removed = 0;
        let mut refusals = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| failed(&layers, error))?;
            let path = entry.path();
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let kind = entry.file_type().map_err(|error| failed(&path, error))?;
            if !kind.is_dir() {
                // The index of an interrupted build.
                if name.starts_with(".staged-")
                    && Path::new(&name).extension() == Some(OsStr::new("index"))
                {
                    std::fs::remove_file(&path).map_err(|error| failed(&path, error))?;
                }
                continue;
            }
            // Leftovers are named `.staged-<commit>-…` and `.trash-<commit>-…`.
            let leftover = name
                .strip_prefix(".staged-")
                .or_else(|| name.strip_prefix(".trash-"));
            let commit = match leftover {
                Some(rest) => rest.split('-').next().unwrap_or_default().to_string(),
                None if name.starts_with('.') || used.contains(&name) => continue,
                None => name.clone(),
            };
            let verified = match self.layer_transcripts(&path, &commit) {
                Ok(verified) => verified,
                Err(error @ WorkspaceError::Refused { .. }) => {
                    refusals.push(error.to_string());
                    if leftover.is_none() {
                        // Not the commit's snapshot any more: kept whole,
                        // under a name no mount uses.
                        let kept = layers.join(format!(
                            ".kept-{commit}-{}-{}",
                            std::process::id(),
                            next_ordinal()
                        ));
                        std::fs::rename(&path, &kept).map_err(|error| failed(&path, error))?;
                        refusals.push(format!("set aside as {}", kept.display()));
                    }
                    continue;
                }
                Err(error) => return Err(error),
            };
            let trash = if leftover.is_some() {
                path
            } else {
                let trash = layers.join(format!(
                    ".trash-{commit}-{}-{}",
                    std::process::id(),
                    next_ordinal()
                ));
                std::fs::rename(&path, &trash).map_err(|error| failed(&path, error))?;
                removed += 1;
                trash
            };
            self.hook("prune:remove");
            let verified: std::collections::HashSet<PathBuf> = verified
                .into_iter()
                .map(|relative| trash.join(relative))
                .collect();
            if let Err(error) = remove_tree_allowing(&trash, &verified) {
                // Something appeared after the look: it stays in the trash,
                // under a name no mount uses, for the next prune to look at.
                refusals.push(failed(&trash, error).to_string());
            }
        }
        if refusals.is_empty() {
            Ok(removed)
        } else {
            Err(WorkspaceError::Refused {
                code: TRANSCRIPT_IN_LAYER.to_string(),
                message: format!("{removed} layer(s) removed; kept: {}", refusals.join("; ")),
            })
        }
    }

    /// The `*.jsonl` files ([`is_transcript`]) under the layer `layer`, by
    /// path relative to it, when each is byte for byte the blob (or
    /// symlink) `commit` tracks at that path. Any other one refuses
    /// ([`TRANSCRIPT_IN_LAYER`]), and so does a part of the layer that
    /// cannot be read.
    fn layer_transcripts(
        &self,
        layer: &Path,
        commit: &str,
    ) -> Result<Vec<PathBuf>, WorkspaceError> {
        let mut found = Vec::new();
        list_transcripts(layer, Path::new(""), &mut found).map_err(|error| {
            WorkspaceError::Refused {
                code: TRANSCRIPT_IN_LAYER.to_string(),
                message: format!(
                    "cannot read layer {} ({error}), so cannot tell it holds no transcript",
                    layer.display()
                ),
            }
        })?;
        let mut verified = Vec::new();
        for relative in found {
            let full = layer.join(&relative);
            let mut spec = OsString::from(format!("{commit}:"));
            spec.push(relative.as_os_str());
            let tracked = self
                .git()
                .args([
                    OsStr::new("rev-parse"),
                    OsStr::new("--verify"),
                    OsStr::new("--quiet"),
                    &spec,
                ])
                .output();
            let tracked = report(tracked, "rev-parse").unwrap_or_default();
            let metadata = std::fs::symlink_metadata(&full).ok();
            let actual = match metadata {
                Some(metadata) if metadata.is_symlink() => {
                    std::fs::read_link(&full).ok().and_then(|target| {
                        self.run(
                            &["hash-object", "--no-filters", "--stdin"],
                            Some(target.as_os_str().as_bytes()),
                        )
                        .ok()
                    })
                }
                Some(metadata) if metadata.is_file() => {
                    let output = self
                        .git()
                        .args(["hash-object", "--no-filters", "--"])
                        .arg(&full)
                        .output();
                    report(output, "hash-object").ok()
                }
                _ => None,
            };
            if tracked.is_empty() || actual.as_deref() != Some(tracked.as_str()) {
                return Err(WorkspaceError::Refused {
                    code: TRANSCRIPT_IN_LAYER.to_string(),
                    message: format!(
                        "{} is not the file commit {commit} tracks there: the layer is kept",
                        full.display()
                    ),
                });
            }
            verified.push(relative);
        }
        Ok(verified)
    }

    // ---- paths and locks -----------------------------------------------

    fn merged_dir(&self, workspace: &WorkspaceId) -> Result<PathBuf, WorkspaceError> {
        let branch = workspace.sandbox();
        let directory = PathBuf::from(workspace.path());
        if branch.is_empty() || branch.starts_with('.') || directory != self.root.join(branch) {
            return Err(WorkspaceError::Unavailable(format!(
                "{workspace} is not a workspace of {}",
                self.root.display()
            )));
        }
        Ok(directory)
    }

    /// `.overlay/<branch in hex>`: one flat name per branch, so the state
    /// of `a` never holds the state of `a/b`.
    fn state_dir(&self, workspace: &WorkspaceId) -> Result<PathBuf, WorkspaceError> {
        self.merged_dir(workspace)?;
        Ok(self.root.join(".overlay").join(hex(workspace.sandbox())))
    }

    /// The admission lease of the workspace whose state is `state`: shared
    /// by every client [`Self::command`] launches, for its whole life; tried
    /// exclusively by [`WorkspaceStore::unmount`] and by the store's drop
    /// before either stops the holder.
    fn lease_path(&self, state: &Path) -> PathBuf {
        let mut name = state.file_name().unwrap_or_default().to_os_string();
        name.push(".lease");
        self.root.join(".locks").join(name)
    }

    /// The store lock: shared by every mount (which builds and picks a
    /// layer), exclusive for [`Self::prune_layers`] (which deletes layers).
    /// Lock files live in `<root>/.locks`, empty, and stay: removing one
    /// another process may be about to lock would let two hold "the" lock.
    fn lock_store(&self, exclusive: bool) -> Result<std::fs::File, WorkspaceError> {
        let path = self.root.join(".locks").join("store");
        let file = lock_file(&path)?;
        let locked = if exclusive {
            file.lock()
        } else {
            file.lock_shared()
        };
        locked.map_err(|error| {
            WorkspaceError::Unavailable(format!("lock {}: {error}", path.display()))
        })?;
        Ok(file)
    }

    /// The lock of one workspace, held across a whole mount, commit or
    /// unmount of it, by every store over this root (as a checkpoint holds
    /// [`GitWorkspace`]'s branch lock): two of them never check, then
    /// mount, at once, nor delete an upperdir another is mounting.
    fn lock_workspace(&self, branch: &str) -> Result<std::fs::File, WorkspaceError> {
        let path = self.root.join(".locks").join(hex(branch));
        let file = lock_file(&path)?;
        file.lock().map_err(|error| {
            WorkspaceError::Unavailable(format!("lock {}: {error}", path.display()))
        })?;
        Ok(file)
    }

    /// Close the workspace of `state` to new clients: its lease, taken
    /// exclusively without waiting, until the returned file is dropped. A
    /// launched client that still holds it refuses ([`IN_USE`]).
    fn close_admission(&self, state: &Path) -> Result<std::fs::File, WorkspaceError> {
        let path = self.lease_path(state);
        let file = lock_file(&path)?;
        match file.try_lock() {
            Ok(()) => Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => Err(WorkspaceError::Refused {
                code: IN_USE.to_string(),
                message: format!(
                    "a client launched into the workspace still holds its lease {}; still mounted",
                    path.display()
                ),
            }),
            Err(std::fs::TryLockError::Error(error)) => Err(WorkspaceError::Unavailable(format!(
                "lock {}: {error}",
                path.display()
            ))),
        }
    }

    // ---- git -----------------------------------------------------------

    /// Every git this store runs ([`GitWorkspace`]'s), pathspecs literal.
    fn git(&self) -> Command {
        let mut command = self.git.command();
        command
            .env("GIT_LITERAL_PATHSPECS", "1")
            .stdin(Stdio::null());
        command
    }

    /// Run git with `arguments`, feeding `input`, and return its trimmed
    /// stdout.
    fn run<S: AsRef<OsStr>>(
        &self,
        arguments: &[S],
        input: Option<&[u8]>,
    ) -> Result<String, WorkspaceError> {
        self.run_with(&mut self.git(), arguments, input)
            .map(|stdout| String::from_utf8_lossy(&stdout).trim().to_string())
    }

    /// [`Self::run`] on a prepared `command`, stdout as bytes, untrimmed
    /// ([`report_listing`]).
    #[allow(clippy::unused_self)]
    fn run_with<S: AsRef<OsStr>>(
        &self,
        command: &mut Command,
        arguments: &[S],
        input: Option<&[u8]>,
    ) -> Result<Vec<u8>, WorkspaceError> {
        let word = arguments
            .first()
            .map(|word| word.as_ref().to_string_lossy().into_owned())
            .unwrap_or_default();
        command.args(arguments);
        let Some(input) = input else {
            return report_listing(command.output(), &word);
        };
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| WorkspaceError::Unavailable(format!("run {word}: {error}")))?;
        let stdin = child.stdin.take();
        // Written beside the read: a long answer must not block the input.
        std::thread::scope(|scope| {
            scope.spawn(|| stdin.map(|mut stdin| stdin.write_all(input)));
            report_listing(child.wait_with_output(), &word)
        })
    }

    /// The commit `reference` names, `None` when it names none. Only git's
    /// own "no such commit" (`rev-parse --verify --quiet`, exit 1, nothing
    /// printed) is `None`: any other failure (the repository unreadable,
    /// git not run) is an error, never taken for a missing branch.
    fn lookup(&self, reference: &str) -> Result<Option<String>, WorkspaceError> {
        let output = self
            .git()
            .args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("{reference}^{{commit}}"))
            .output();
        match output {
            Ok(output) if output.status.code() == Some(1) && output.stdout.is_empty() => Ok(None),
            output => report(output, "rev-parse").map(Some),
        }
    }

    fn commit_id(text: &str) -> Result<CommitId, WorkspaceError> {
        CommitId::new(text.to_string())
            .map_err(|error| WorkspaceError::Unavailable(error.to_string()))
    }

    /// Materialize `commit` as a shared snapshot layer, once: built beside
    /// it and renamed into place, so a layer that exists is whole.
    fn ensure_layer(&self, commit: &str) -> Result<PathBuf, WorkspaceError> {
        let layer = self.layer_path(commit);
        if layer.is_dir() {
            return Ok(layer);
        }
        let layers = self.root.join(".layers");
        let staged = layers.join(format!(
            ".staged-{commit}-{}-{}",
            std::process::id(),
            next_ordinal()
        ));
        std::fs::create_dir_all(&staged).map_err(|error| {
            WorkspaceError::Unavailable(format!("create {}: {error}", staged.display()))
        })?;
        let mut index = staged.clone().into_os_string();
        index.push(".index");
        let checkout = |arguments: &[&str]| {
            let mut git = self.git();
            git.env("GIT_INDEX_FILE", &index)
                .arg(format!("--work-tree={}", staged.display()));
            self.run_with(&mut git, arguments, None).map(drop)
        };
        let built = checkout(&["read-tree", commit])
            .and_then(|()| checkout(&["checkout-index", "--all", "--force"]));
        let _ = std::fs::remove_file(&index);
        if let Err(error) = built {
            let _ = remove_tree(&staged);
            return Err(error);
        }
        if std::fs::rename(&staged, &layer).is_err() {
            // Another mount built the same layer meanwhile: theirs is whole.
            let _ = remove_tree(&staged);
            if !layer.is_dir() {
                return Err(WorkspaceError::Unavailable(format!(
                    "could not put layer {} in place",
                    layer.display()
                )));
            }
        }
        Ok(layer)
    }

    /// Run the unit tests' hook at `point`, if one is set (nothing outside
    /// the unit tests).
    #[cfg_attr(not(test), allow(clippy::unused_self))]
    fn hook(&self, point: &str) {
        #[cfg(test)]
        self.hooks.run(point);
        #[cfg(not(test))]
        let _ = point;
    }

    // ---- holders -------------------------------------------------------

    /// The holder of the mount kept in `state`, when it is alive: the
    /// recorded pid still runs, started at the recorded time, in the
    /// recorded namespace. A record that does not parse names no live
    /// holder: it is neither trusted nor killed, and the kernel's workdir
    /// probe decides whether its overlay lives.
    fn live_holder(&self, state: &Path) -> Option<Holder> {
        let holder = recorded_holder(state)?;
        if let Some(child) = self.holders.lock().ok()?.get_mut(state)
            && child.id() == holder.pid
            && !matches!(child.try_wait(), Ok(None))
        {
            return None;
        }
        (runs_as(holder.pid, holder.start)
            && mount_namespace(holder.pid).as_deref() == Some(holder.namespace.as_str()))
        .then_some(holder)
    }

    /// Start a holder that mounts the overlay of `state` at `merged` and
    /// sleeps, and record it.
    fn start_holder(
        &self,
        state: &Path,
        lowers: &[PathBuf],
        merged: &Path,
    ) -> Result<Holder, WorkspaceError> {
        let options = mount_options(lowers, &state.join("upper"), &state.join("work"))?;
        let script = r#"mount -t overlay overlay -o "$1" "$2" || exit 97
echo ready
exec sleep infinity"#;
        let failed = |error: std::io::Error| {
            WorkspaceError::Unavailable(format!("start the overlay holder: {error}"))
        };
        let mut child = git_command("unshare")
            .args(["--user", "--map-root-user", "--mount", "sh", "-c", script])
            .arg("spawn-overlay-holder")
            .arg(&options)
            .arg(merged)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(failed)?;
        let mut line = String::new();
        let stdout = child.stdout.take().expect("piped stdout");
        BufReader::new(stdout)
            .read_line(&mut line)
            .map_err(failed)?;
        if line.trim() != "ready" {
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            let status = child.wait().map_err(failed)?;
            return Err(WorkspaceError::Unavailable(format!(
                "unprivileged overlay mount of {} failed ({status}): {}",
                merged.display(),
                stderr.trim()
            )));
        }
        drop(child.stderr.take());
        let pid = child.id();
        let root = PathBuf::from(format!("/proc/{pid}/root"))
            .join(merged.strip_prefix("/").unwrap_or(merged));
        let recorded = match (mount_namespace(pid), process_start(pid), on_overlay(&root)) {
            (Some(namespace), Some(start), Ok(true)) => Holder {
                pid,
                namespace,
                start,
            },
            (_, _, overlay) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(WorkspaceError::Unavailable(format!(
                    "the overlay holder {pid} shows no overlay at {} ({overlay:?})",
                    merged.display()
                )));
            }
        };
        let line = format!("{pid} {} {}\n", recorded.namespace, recorded.start);
        if let Err(error) = write_atomic(&state.join("holder"), line.as_bytes()) {
            // Unrecorded, it would hold the overlay with nothing naming it.
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        if let Ok(mut holders) = self.holders.lock()
            && let Some(mut old) = holders.insert(state.to_path_buf(), child)
        {
            let _ = old.kill();
            let _ = old.wait();
        }
        Ok(recorded)
    }

    /// Stop the holder of `state`, if any, and wait (a bounded while) until
    /// it has exited. One this store started is killed through its
    /// [`Child`]. A recovered one (another store's) is signalled through a
    /// pidfd opened first and checked against the record afterwards, so a
    /// pid reused in between is never signalled; one that does not match
    /// is left alone, and the workdir probe after the stop decides. The
    /// `holder` record stays.
    fn stop_holder(&self, state: &Path) {
        let child = self
            .holders
            .lock()
            .ok()
            .and_then(|mut holders| holders.remove(state));
        if let Some(mut child) = child {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }
        let Some(holder) = recorded_holder(state) else {
            return;
        };
        let Some(pid) = i32::try_from(holder.pid)
            .ok()
            .and_then(rustix::process::Pid::from_raw)
        else {
            return;
        };
        let Ok(pidfd) = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty())
        else {
            return;
        };
        if self.live_holder(state).as_ref() != Some(&holder) {
            return;
        }
        if rustix::process::pidfd_send_signal(&pidfd, rustix::process::Signal::KILL).is_err() {
            return;
        }
        // Not our child: it is reaped by its parent. It has left its
        // namespace once it no longer runs (or is a zombie).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while runs_as(holder.pid, holder.start) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    // ---- the checkpoint ------------------------------------------------

    /// The branch head this workspace last published, with a publication
    /// left in flight (`pending`: the old and new commit, written before the
    /// compare-and-swap) settled against the ref first: the ref at `new`
    /// means it was published, anything else that it was not. A ref that
    /// cannot be read is an error, and `pending` is kept for the next try.
    fn published_head(&self, state: &Path, reference: &str) -> Result<String, WorkspaceError> {
        let pending = state.join("pending");
        match std::fs::read_to_string(&pending) {
            Ok(text) => {
                let now = self.lookup(reference)?;
                if let Some(new) = text.split_whitespace().nth(1)
                    && now.as_deref() == Some(new)
                {
                    write_atomic(&state.join("head"), format!("{new}\n").as_bytes())?;
                }
                std::fs::remove_file(&pending).map_err(|error| {
                    WorkspaceError::Unavailable(format!("remove {}: {error}", pending.display()))
                })?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(WorkspaceError::Unavailable(format!(
                    "read {}: {error}",
                    pending.display()
                )));
            }
        }
        read_line(&state.join("head"))
    }

    /// What the upperdir under `upper` changes against the commit `lower`:
    /// paths the lower layers lose (whiteouts, opaque directories) and files
    /// it adds or replaces.
    fn walk_upper(&self, upper: &Path, lower: &str) -> Result<Diff, WorkspaceError> {
        let mut diff = Diff::default();
        self.walk(upper, &[], lower, &mut diff)?;
        Ok(diff)
    }

    /// Whether the commit `lower` tracks anything at `path`.
    fn tracks(&self, lower: &str, path: &[u8]) -> Result<bool, WorkspaceError> {
        let mut spec = OsString::from(format!("{lower}:"));
        spec.push(OsStr::from_bytes(path));
        let status = self
            .git()
            .args([OsStr::new("cat-file"), OsStr::new("-e"), &spec])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|error| WorkspaceError::Unavailable(format!("run git: {error}")))?;
        Ok(status.success())
    }

    fn walk(
        &self,
        directory: &Path,
        prefix: &[u8],
        lower: &str,
        diff: &mut Diff,
    ) -> Result<(), WorkspaceError> {
        let failed = |error: std::io::Error| {
            WorkspaceError::Unavailable(format!("read {}: {error}", directory.display()))
        };
        let mut entries: Vec<_> = std::fs::read_dir(directory)
            .map_err(failed)?
            .collect::<Result<_, _>>()
            .map_err(failed)?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let name = entry.file_name();
            let mut path = prefix.to_vec();
            if !path.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(name.as_bytes());
            let full = entry.path();
            let metadata = std::fs::symlink_metadata(&full).map_err(failed)?;
            let kind = metadata.file_type();
            // Only a directory is derived, and only one the lower commit
            // does not track: a tracked file or directory named `target`
            // is the agent's work like any other.
            if kind.is_dir() && self.derived.contains(&name) && !self.tracks(lower, &path)? {
                continue;
            }
            if is_whiteout(&metadata) || is_xattr_whiteout(&full, &metadata) {
                diff.removed.push(path);
            } else if kind.is_dir() {
                if is_opaque(&full) {
                    diff.removed.push(path.clone());
                }
                self.walk(&full, &path, lower, diff)?;
            } else if kind.is_symlink() {
                let target = std::fs::read_link(&full).map_err(failed)?;
                diff.added.push(Added {
                    path,
                    mode: "120000",
                    source: FileOrigin::Bytes(target.as_os_str().as_bytes().to_vec()),
                });
            } else if kind.is_file() {
                let mode = if metadata.permissions().mode() & 0o111 == 0 {
                    "100644"
                } else {
                    "100755"
                };
                diff.added.push(Added {
                    path,
                    mode,
                    source: FileOrigin::File(full),
                });
            }
            // Sockets, fifos and devices other than whiteouts are no
            // files of git's.
        }
        Ok(())
    }

    /// Store the blobs of `added` in the repository, in the order given.
    fn hash_blobs(&self, added: &[Added]) -> Result<Vec<String>, WorkspaceError> {
        let mut ids = vec![String::new(); added.len()];
        // Files whose path git can read one per line go through one git.
        let mut batch = Vec::new();
        let mut paths = Vec::new();
        for (index, item) in added.iter().enumerate() {
            match &item.source {
                FileOrigin::File(full) if !full.as_os_str().as_bytes().contains(&b'\n') => {
                    batch.push(index);
                    paths.extend_from_slice(full.as_os_str().as_bytes());
                    paths.push(b'\n');
                }
                FileOrigin::File(full) => {
                    ids[index] = self.run(
                        &[
                            OsStr::new("hash-object"),
                            OsStr::new("-w"),
                            OsStr::new("--no-filters"),
                            OsStr::new("--"),
                            full.as_os_str(),
                        ],
                        None,
                    )?;
                }
                FileOrigin::Bytes(bytes) => {
                    ids[index] = self.run(
                        &["hash-object", "-w", "--no-filters", "--stdin"],
                        Some(bytes),
                    )?;
                }
            }
        }
        if !batch.is_empty() {
            let out = self.run(
                &["hash-object", "-w", "--no-filters", "--stdin-paths"],
                Some(&paths),
            )?;
            let lines: Vec<&str> = out.lines().collect();
            if lines.len() != batch.len() {
                return Err(WorkspaceError::Unavailable(format!(
                    "git hashed {} files of {}",
                    lines.len(),
                    batch.len()
                )));
            }
            for (index, line) in batch.into_iter().zip(lines) {
                ids[index] = line.to_string();
            }
        }
        Ok(ids)
    }

    /// The tree of `base` with `diff` applied, written to the repository.
    /// The removed paths are listed in bounded batches
    /// ([`pathspec_batches`]), never all on one command line.
    fn write_tree(&self, state: &Path, base: &str, diff: &Diff) -> Result<String, WorkspaceError> {
        let index = state.join("index");
        let _ = std::fs::remove_file(&index);
        let git = || {
            let mut command = self.git();
            command.env("GIT_INDEX_FILE", &index);
            command
        };
        let result = (|| {
            self.run_with(&mut git(), &["read-tree", base], None)?;
            let mut info = Vec::new();
            let removed = diff
                .removed
                .iter()
                .map(|path| OsStr::from_bytes(path).to_os_string())
                .collect();
            for batch in pathspec_batches(removed) {
                let mut arguments: Vec<OsString> = ["ls-files", "-z", "--cached", "--"]
                    .map(OsString::from)
                    .to_vec();
                arguments.extend(batch);
                let listed = self.run_with(&mut git(), &arguments, None)?;
                for path in crate::tree_path::nul_fields(&listed) {
                    info.extend_from_slice(b"0 0000000000000000000000000000000000000000\t");
                    info.extend_from_slice(path);
                    info.push(0);
                }
            }
            let ids = self.hash_blobs(&diff.added)?;
            for (item, id) in diff.added.iter().zip(ids) {
                info.extend_from_slice(format!("{} {id}\t", item.mode).as_bytes());
                info.extend_from_slice(&item.path);
                info.push(0);
            }
            if !info.is_empty() {
                self.run_with(
                    &mut git(),
                    &["update-index", "--add", "--replace", "-z", "--index-info"],
                    Some(&info),
                )?;
            }
            let tree = self.run_with(&mut git(), &["write-tree"], None)?;
            Ok(String::from_utf8_lossy(&tree).trim().to_string())
        })();
        let _ = std::fs::remove_file(&index);
        result
    }

    // ---- teardown ------------------------------------------------------

    /// Whether overlays (and the workdir probe) work on the filesystem of
    /// the store root ([`overlay_supported_in`] under `.overlay`), probed
    /// once per store.
    fn support(&self) -> Result<(), String> {
        self.supported
            .get_or_init(|| {
                let overlay = self.root.join(".overlay");
                std::fs::create_dir_all(&overlay)
                    .map_err(|error| format!("create {}: {error}", overlay.display()))?;
                overlay_supported_in(&overlay)
            })
            .clone()
    }

    /// Ask the kernel whether the overlay of `state` is still alive, its
    /// holder gone or stopped ([`work_lock`] on the original workdir): busy
    /// refuses [`IN_USE`]; no answer, or a store root where the probe was
    /// not shown to work ([`Self::support`]), refuses [`IN_USE_UNPROVEN`];
    /// either way nothing is changed.
    fn refuse_while_alive(&self, state: &Path) -> Result<(), WorkspaceError> {
        let upper = state.join("upper");
        let answer = match self.support() {
            Ok(()) => work_lock(state),
            Err(why) => WorkLock::Unknown(format!("the store root is not supported: {why}")),
        };
        match answer {
            WorkLock::Free => Ok(()),
            WorkLock::Busy(why) => Err(WorkspaceError::Refused {
                code: IN_USE.to_string(),
                message: format!(
                    "the overlay over {} is still alive (workdir busy: {why}); kept",
                    upper.display()
                ),
            }),
            WorkLock::Unknown(why) => Err(WorkspaceError::Refused {
                code: IN_USE_UNPROVEN.to_string(),
                message: format!(
                    "could not ask the kernel whether the overlay over {} is alive ({why}); kept",
                    upper.display()
                ),
            }),
        }
    }

    /// A new state for `head` at `state`, built beside it and renamed into
    /// place: a state that exists always has its `lower`.
    fn create_state(state: &Path, head: &str) -> Result<(), WorkspaceError> {
        let name = state.file_name().unwrap_or_default().to_string_lossy();
        let staged = state.with_file_name(format!(
            ".staged-{name}-{}-{}",
            std::process::id(),
            next_ordinal()
        ));
        let made = (|| {
            for name in ["upper", "work"] {
                std::fs::create_dir_all(staged.join(name)).map_err(|error| {
                    WorkspaceError::Unavailable(format!("create {}: {error}", staged.display()))
                })?;
            }
            write_atomic(&staged.join("head"), format!("{head}\n").as_bytes())?;
            write_atomic(&staged.join("lower"), format!("{head}\n").as_bytes())?;
            std::fs::rename(&staged, state).map_err(|error| {
                WorkspaceError::Unavailable(format!("create {}: {error}", state.display()))
            })
        })();
        if made.is_err() {
            let _ = remove_tree(&staged);
        }
        made
    }
}

impl Drop for OverlayWorkspace {
    /// Holders this store started stop with it, each only once its
    /// workspace's admission is closed (its lease taken exclusively): a
    /// holder whose launched client still holds the lease is left running,
    /// so the client keeps its workspace, and the next store finds it as the
    /// live holder. Their upperdirs stay: the next mount mounts them again.
    fn drop(&mut self) {
        let Ok(mut holders) = self.holders.lock() else {
            return;
        };
        for (state, mut child) in holders.drain() {
            let Ok(_admission) = self.close_admission(&state) else {
                continue;
            };
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl WorkspaceStore for OverlayWorkspace {
    fn project(&self) -> &str {
        self.git.project()
    }

    /// [`GitWorkspace::ensure_branch`], refusing a name that starts with
    /// `.` (the store's own directories under its root).
    fn ensure_branch(
        &self,
        branch: &BranchName,
        from: Option<&str>,
    ) -> Result<Branch, WorkspaceError> {
        if branch.as_str().starts_with('.') {
            return Err(WorkspaceError::Unavailable(format!(
                "branch {branch} starts with '.'"
            )));
        }
        self.git.ensure_branch(branch, from)
    }

    /// Mount the branch's workspace: a state that does not exist yet is
    /// made for the branch head; one that exists is mounted again over its
    /// own lower commit, once the kernel shows the old overlay over its
    /// workdir gone ([`IN_USE`] while it lives, [`IN_USE_UNPROVEN`] when it
    /// cannot tell). A state that exists without its `lower` is refused
    /// ([`STATE_UNREADABLE`]), never started over.
    fn mount(&self, branch: &BranchName) -> Result<WorkspaceId, WorkspaceError> {
        let reference = format!("refs/heads/{branch}");
        let head = self
            .lookup(&reference)?
            .ok_or_else(|| WorkspaceError::Unavailable(format!("branch {branch} has no commit")))?;
        let merged = self.root.join(branch.as_str());
        let workspace = WorkspaceId::new(format!("{branch}:{}", merged.display()))
            .map_err(|error| WorkspaceError::Unavailable(error.to_string()))?;
        let state = self.state_dir(&workspace)?;
        self.support().map_err(|why| {
            WorkspaceError::Unavailable(format!(
                "no unprivileged overlay under {}: {why}",
                self.root.display()
            ))
        })?;
        let _store = self.lock_store(false)?;
        let _lock = self.lock_workspace(branch.as_str())?;
        let lower = match std::fs::read_to_string(state.join("lower")) {
            Ok(lower) => {
                self.published_head(&state, &reference)?;
                lower.trim().to_string()
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::symlink_metadata(&state) {
                    Err(missing) if missing.kind() == std::io::ErrorKind::NotFound => {
                        std::fs::create_dir_all(self.root.join(".overlay")).map_err(|error| {
                            WorkspaceError::Unavailable(format!("create .overlay: {error}"))
                        })?;
                        Self::create_state(&state, &head)?;
                        let layer = self.ensure_layer(&head)?;
                        return self.start(&state, layer, &merged).map(|()| workspace);
                    }
                    _ => {
                        return Err(WorkspaceError::Refused {
                            code: STATE_UNREADABLE.to_string(),
                            message: format!(
                                "{} exists without its lower record: kept, never started over",
                                state.display()
                            ),
                        });
                    }
                }
            }
            Err(error) => {
                return Err(WorkspaceError::Unavailable(format!(
                    "read {}: {error}",
                    state.join("lower").display()
                )));
            }
        };
        let layer = self.ensure_layer(&lower)?;
        let mut _admission = None;
        if let Some(holder) = self.live_holder(&state) {
            let relative = merged.strip_prefix("/").unwrap_or(&merged);
            let root = PathBuf::from(format!("/proc/{}/root", holder.pid)).join(relative);
            if on_overlay(&root).unwrap_or(false) {
                return Ok(workspace);
            }
            // A holder whose mount is gone (lazily unmounted from inside):
            // retired as teardown retires one, once no launched client
            // holds the lease ([`IN_USE`] while one does).
            _admission = Some(self.close_admission(&state)?);
            self.stop_holder(&state);
        }
        // The holder is gone, but what held its overlay may outlive it:
        // never a second overlay over one upperdir.
        self.refuse_while_alive(&state)?;
        self.start(&state, layer, &merged).map(|()| workspace)
    }

    /// The lower commit's tree with the upperdir applied, committed on the
    /// branch head this workspace last published (compare-and-swap). A
    /// branch moved by another writer refuses with [`WORKSPACE_STALE`], and
    /// the upperdir stays. A branch gone is made again
    /// ([`Commit::Recreated`]). The old and new commit are recorded
    /// (`pending`) before the ref moves, so a publication whose `head` was
    /// not written is settled on the next commit or mount.
    fn commit(&self, workspace: &WorkspaceId, message: &str) -> Result<Commit, WorkspaceError> {
        let state = self.state_dir(workspace)?;
        let branch = workspace.sandbox();
        let _lock = self.lock_workspace(branch)?;
        let reference = format!("refs/heads/{branch}");
        let lower = read_line(&state.join("lower"))?;
        let published = self.published_head(&state, &reference)?;
        let diff = self.walk_upper(&state.join("upper"), &lower)?;
        let tree = self.write_tree(&state, &lower, &diff)?;
        let head = self.lookup(&reference)?;
        if let Some(head) = &head {
            if *head != published {
                return Err(WorkspaceError::Refused {
                    code: WORKSPACE_STALE.to_string(),
                    message: format!(
                        "branch {branch} moved from {published} to {head} under the workspace"
                    ),
                });
            }
            if self.run(&["rev-parse", &format!("{head}^{{tree}}")], None)? == tree {
                return Ok(Commit::Unchanged);
            }
        }
        let parent = match &head {
            Some(head) => head.clone(),
            None if self.lookup(&published)?.is_some() => published.clone(),
            None => lower.clone(),
        };
        let commit = self.run(&["commit-tree", &tree, "-p", &parent, "-m", message], None)?;
        let old = head.clone().unwrap_or_default();
        write_atomic(
            &state.join("pending"),
            format!("{} {commit}\n", head.as_deref().unwrap_or("-")).as_bytes(),
        )?;
        self.hook("commit:update-ref");
        // Compare-and-swap: a writer that moved (or made) the branch since
        // it was read wins, and this commit is refused as stale.
        if let Err(error) = self.run(&["update-ref", &reference, &commit, &old], None) {
            let now = self.lookup(&reference)?;
            if now != head {
                return Err(WorkspaceError::Refused {
                    code: WORKSPACE_STALE.to_string(),
                    message: format!(
                        "branch {branch} moved from {} to {} under the commit",
                        head.as_deref().unwrap_or("nothing"),
                        now.as_deref().unwrap_or("nothing")
                    ),
                });
            }
            return Err(error);
        }
        self.hook("commit:published");
        write_atomic(&state.join("head"), format!("{commit}\n").as_bytes())?;
        let _ = std::fs::remove_file(state.join("pending"));
        if head.is_some() {
            Ok(Commit::Created(Self::commit_id(&commit)?))
        } else {
            Ok(Commit::Recreated {
                commit: Self::commit_id(&commit)?,
                parent: Some(Self::commit_id(&parent)?),
            })
        }
    }

    /// Tear the workspace down, all under the workspace lock:
    ///
    /// 1. the upperdir is read whole for transcripts ([`TRANSCRIPT_IN_UPPER`],
    ///    [`UPPER_UNREADABLE`]): a refusal leaves it mounted;
    /// 2. admission closes: the lease is taken exclusively, and a client
    ///    launched through [`Self::command`] that still holds it refuses
    ///    ([`IN_USE`]), still mounted; no new client gets in meanwhile;
    /// 3. the holder is stopped (only the verified one, see
    ///    `stop_holder`);
    /// 4. the kernel is asked about the original workdir: busy refuses
    ///    [`IN_USE`], no answer [`IN_USE_UNPROVEN`], with upper, work,
    ///    state and layers kept (a later mount refuses too while the old
    ///    overlay lives);
    /// 5. once it is free nothing can write the upperdir any more: it is
    ///    read for transcripts again, and the state renamed aside in one
    ///    step and deleted. Only the kernel's own `work/` subdirectories
    ///    get their permissions changed, and a `*.jsonl` is never deleted.
    fn unmount(&self, workspace: &WorkspaceId) -> Result<(), WorkspaceError> {
        let state = self.state_dir(workspace)?;
        let merged = self.merged_dir(workspace)?;
        let _lock = self.lock_workspace(workspace.sandbox())?;
        match std::fs::symlink_metadata(&state) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(WorkspaceError::Unavailable(format!(
                    "read {}: {error}",
                    state.display()
                )));
            }
        }
        let upper = state.join("upper");
        refuse_transcripts(&upper)?;
        let _admission = self.close_admission(&state)?;
        self.hook("unmount:stop");
        self.stop_holder(&state);
        self.refuse_while_alive(&state)?;
        refuse_transcripts(&upper)?;
        let trash = self.root.join(".overlay").join(format!(
            ".trash-{}-{}",
            std::process::id(),
            next_ordinal()
        ));
        std::fs::rename(&state, &trash).map_err(|error| {
            WorkspaceError::Unavailable(format!("retire {}: {error}", state.display()))
        })?;
        let _ = std::fs::remove_dir(&merged);
        remove_state(&trash).map_err(|error| {
            WorkspaceError::Unavailable(format!("remove {}: {error}", trash.display()))
        })
    }

    fn is_mounted(&self, workspace: &WorkspaceId) -> bool {
        let Ok(state) = self.state_dir(workspace) else {
            return false;
        };
        let Ok(merged) = self.merged_dir(workspace) else {
            return false;
        };
        if !state.join("lower").is_file() {
            return false;
        }
        let Some(holder) = self.live_holder(&state) else {
            return false;
        };
        let relative = merged.strip_prefix("/").unwrap_or(&merged);
        let root = PathBuf::from(format!("/proc/{}/root", holder.pid)).join(relative);
        on_overlay(&root).unwrap_or(false)
    }

    fn notices(&self, _workspace: &WorkspaceId) -> Vec<String> {
        Vec::new()
    }

    fn notices_told(&self, _workspace: &WorkspaceId, _notices: &[String]) {}

    fn branch_head(&self, branch: &BranchName) -> Result<Option<CommitId>, WorkspaceError> {
        self.lookup(&format!("refs/heads/{branch}"))?
            .map(|commit| Self::commit_id(&commit))
            .transpose()
    }
}

impl OverlayWorkspace {
    /// Mount the overlay of `state` over `layer` (and the pre-warmed
    /// layers) at `merged`.
    fn start(&self, state: &Path, layer: PathBuf, merged: &Path) -> Result<(), WorkspaceError> {
        std::fs::create_dir_all(merged).map_err(|error| {
            WorkspaceError::Unavailable(format!("prepare {}: {error}", merged.display()))
        })?;
        let mut lowers = vec![layer];
        lowers.extend(self.lower_layers.iter().cloned());
        self.start_holder(state, &lowers, merged).map(drop)
    }
}

/// What an upperdir changes against its lower layers.
#[derive(Debug, Default)]
struct Diff {
    /// Paths (files or whole directories) the lower layers lose.
    removed: Vec<Vec<u8>>,
    /// Files the upperdir adds or replaces, applied after the removals.
    added: Vec<Added>,
}

#[derive(Debug)]
struct Added {
    path: Vec<u8>,
    mode: &'static str,
    source: FileOrigin,
}

#[derive(Debug)]
enum FileOrigin {
    /// A regular file's content, read from the upperdir.
    File(PathBuf),
    /// A symlink's target.
    Bytes(Vec<u8>),
}

/// The lock file at `path`, made with its directory if missing. Each call
/// opens the file anew, so two stores, or two threads of one, exclude each
/// other as two processes do.
fn lock_file(path: &Path) -> Result<std::fs::File, WorkspaceError> {
    let failed = |error: std::io::Error| {
        WorkspaceError::Unavailable(format!("lock {}: {error}", path.display()))
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(failed)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(failed)
}

/// A number no other call in this process got: names a scratch path.
fn next_ordinal() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Whether `path` lies on an overlayfs.
#[allow(clippy::useless_conversion)] // `f_type`'s type differs by architecture
fn on_overlay(path: &Path) -> std::io::Result<bool> {
    let fs = rustix::fs::statfs(path)?;
    Ok(i64::from(fs.f_type) == OVERLAYFS_SUPER_MAGIC)
}

/// The mount namespace of `pid`, as `/proc` names it.
fn mount_namespace(pid: u32) -> Option<String> {
    let namespace = std::fs::read_link(format!("/proc/{pid}/ns/mnt")).ok()?;
    let own = std::fs::read_link("/proc/self/ns/mnt").ok()?;
    // A holder's namespace is its own, never ours.
    (namespace != own).then(|| namespace.to_string_lossy().into_owned())
}

fn read_line(path: &Path) -> Result<String, WorkspaceError> {
    std::fs::read_to_string(path)
        .map(|text| text.trim().to_string())
        .map_err(|error| WorkspaceError::Unavailable(format!("read {}: {error}", path.display())))
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), WorkspaceError> {
    let staged = path.with_extension("staged");
    std::fs::write(&staged, bytes)
        .and_then(|()| std::fs::rename(&staged, path))
        .map_err(|error| WorkspaceError::Unavailable(format!("write {}: {error}", path.display())))
}

/// The commit every workspace state under `overlay` names in its `lower`:
/// the layers in use. Fails closed ([`STATE_UNREADABLE`]): anything but
/// `overlay` itself or a `lower` missing is an error, never "unused". Only
/// the state directories themselves (`<hex branch>`) are read, never what
/// is inside their upperdirs. Names starting with `.` (an unmount's trash,
/// whose overlay was proven gone, or a state being made) are not
/// workspaces.
fn collect_lowers(overlay: &Path) -> Result<std::collections::HashSet<String>, WorkspaceError> {
    let unreadable = |path: &Path, error: std::io::Error| WorkspaceError::Refused {
        code: STATE_UNREADABLE.to_string(),
        message: format!(
            "cannot read {} ({error}), so cannot tell which layers are in use; nothing pruned",
            path.display()
        ),
    };
    let mut used = std::collections::HashSet::new();
    let entries = match std::fs::read_dir(overlay) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(used),
        Err(error) => return Err(unreadable(overlay, error)),
    };
    for entry in entries {
        let entry = entry.map_err(|error| unreadable(overlay, error))?;
        if entry.file_name().as_bytes().starts_with(b".") {
            continue;
        }
        let path = entry.path();
        if !entry
            .file_type()
            .map_err(|error| unreadable(&path, error))?
            .is_dir()
        {
            continue;
        }
        let lower = path.join("lower");
        let commit = std::fs::read_to_string(&lower).map_err(|error| unreadable(&lower, error))?;
        used.insert(commit.trim().to_string());
    }
    Ok(used)
}

/// `text`'s bytes in hex: a branch name as one flat file name.
fn hex(text: &str) -> String {
    use std::fmt::Write as _;
    text.bytes().fold(String::new(), |mut name, byte| {
        let _ = write!(name, "{byte:02x}");
        name
    })
}

/// The holder `state` records, alive or not: `<pid> <namespace> <start
/// time>`. A record that does not parse, or has another number of fields
/// (an older format), is none.
fn recorded_holder(state: &Path) -> Option<Holder> {
    let text = std::fs::read_to_string(state.join("holder")).ok()?;
    let fields: Vec<&str> = text.split_whitespace().collect();
    let [pid, namespace, start] = fields.as_slice() else {
        return None;
    };
    Some(Holder {
        pid: pid.parse().ok()?,
        namespace: (*namespace).to_string(),
        start: start.parse().ok()?,
    })
}

/// The fields of `/proc/<pid>/stat` after the command name (which may hold
/// spaces): the state first, the start time 20th.
fn stat_fields(pid: u32) -> Option<Vec<String>> {
    let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
    let after = stat.iter().rposition(|byte| *byte == b')')?;
    Some(
        std::str::from_utf8(&stat[after + 1..])
            .ok()?
            .split_whitespace()
            .map(str::to_string)
            .collect(),
    )
}

/// When `pid` started, in clock ticks since boot.
fn process_start(pid: u32) -> Option<u64> {
    stat_fields(pid)?.get(19)?.parse().ok()
}

/// Whether `pid` runs (not a zombie) and started at `start`.
fn runs_as(pid: u32, start: u64) -> bool {
    stat_fields(pid).is_some_and(|fields| {
        fields.get(19).and_then(|field| field.parse().ok()) == Some(start)
            && !matches!(fields.first().map(String::as_str), Some("Z" | "X"))
    })
}

/// Refuse ([`TRANSCRIPT_IN_UPPER`]) while `upper` holds a transcript, or
/// ([`UPPER_UNREADABLE`]) while it cannot be read whole.
fn refuse_transcripts(upper: &Path) -> Result<(), WorkspaceError> {
    let transcripts = count_jsonl(upper)?;
    if transcripts > 0 {
        return Err(WorkspaceError::Refused {
            code: TRANSCRIPT_IN_UPPER.to_string(),
            message: format!(
                "{} holds {transcripts} transcript file(s); kept",
                upper.display()
            ),
        });
    }
    Ok(())
}

/// What the kernel says about an overlay workdir ([`work_lock`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkLock {
    /// A probe overlay took it: no overlay over it is alive.
    Free,
    /// Refused as busy: an overlay over it is alive.
    Busy(String),
    /// The probe failed some other way: nothing is known.
    Unknown(String),
}

/// What `mount` said, in the C locale, of a probe mount at `target`:
/// success is [`WorkLock::Free`]. Exit 32 (a mount failure) is
/// [`WorkLock::Busy`] only when the first line is exactly `mount: <target>:
/// <message>` and the message is util-linux's wording of `EBUSY` ("overlay
/// already mounted on …", "… mount point busy", "target is busy", or the
/// system's "Device or resource busy"). The target's own path is cut off
/// before the message is read, so no path text can match. Anything else is
/// unknown.
fn classify(status: Option<i32>, stderr: &str, target: &Path) -> WorkLock {
    let stderr = stderr.trim();
    let message = stderr
        .lines()
        .next()
        .and_then(|line| line.strip_prefix(&format!("mount: {}: ", target.display())))
        .map(|message| message.trim_end_matches('.'));
    let busy = message.is_some_and(|message| {
        message.starts_with("overlay already mounted on ")
            || message == "overlay already mounted or mount point busy"
            || message == "target is busy"
            || message.ends_with("Device or resource busy")
    });
    match status {
        Some(0) => WorkLock::Free,
        Some(32) if busy => WorkLock::Busy(stderr.to_string()),
        status => WorkLock::Unknown(format!("mount exited {status:?}: {stderr}")),
    }
}

/// Ask the kernel whether an overlay whose workdir is `state/work` is still
/// alive ([`work_lock_options`]), retrying a busy or unknown answer for
/// [`WORK_LOCK_PATIENCE`]: the overlay of a holder just stopped goes as its
/// namespace does.
fn work_lock(state: &Path) -> WorkLock {
    let deadline = std::time::Instant::now() + WORK_LOCK_PATIENCE;
    loop {
        let lock = probe_work_lock(state);
        if lock == WorkLock::Free || std::time::Instant::now() >= deadline {
            return lock;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// One probe of [`work_lock`]: a throwaway overlay over the original
/// `state/work` (never renamed or replaced: the kernel marks that inode),
/// with an empty lower and upper of its own under `state/.probe-…`,
/// mounted in a namespace that ends with the probe. Only a mount that
/// succeeds says free.
fn probe_work_lock(state: &Path) -> WorkLock {
    let probe = state.join(format!(".probe-{}-{}", std::process::id(), next_ordinal()));
    let output = (|| {
        for name in ["lower", "upper", "merged"] {
            std::fs::create_dir_all(probe.join(name))
                .map_err(|error| format!("create {}: {error}", probe.display()))?;
        }
        let options =
            work_lock_options(&probe, &state.join("work")).map_err(|error| error.to_string())?;
        git_command("unshare")
            .args(["--user", "--map-root-user", "--mount"])
            .args(["mount", "-t", "overlay", "overlay", "-o"])
            .arg(&options)
            .arg(probe.join("merged"))
            .stdin(Stdio::null())
            .output()
            .map_err(|error| format!("run unshare: {error}"))
    })();
    let _ = remove_tree(&probe);
    match output {
        Err(why) => WorkLock::Unknown(why),
        Ok(output) => classify(
            output.status.code(),
            &String::from_utf8_lossy(&output.stderr),
            &probe.join("merged"),
        ),
    }
}

/// Whether the entry at `path` with `metadata` is a transcript: a name
/// ending in `.jsonl` (by its bytes, so a file named `.jsonl` is one too)
/// that is not a directory (a file, or a link or device standing for one)
/// and not an overlay whiteout of either form (which only marks a lower
/// file deleted, and holds nothing).
fn is_transcript(path: &Path, metadata: &std::fs::Metadata) -> bool {
    path.file_name()
        .is_some_and(|name| name.as_bytes().ends_with(b".jsonl"))
        && !metadata.is_dir()
        && !is_whiteout(metadata)
        && !is_xattr_whiteout(path, metadata)
}

/// How many transcripts ([`is_transcript`]) lie under `directory`, read
/// without following a symlink. Fails closed ([`UPPER_UNREADABLE`]): a
/// directory that cannot be listed (mode 000, say) or an entry that cannot
/// be stat'ed is an error, never zero, and no permission is changed to read
/// it. Only `directory` itself missing counts as empty.
fn count_jsonl(directory: &Path) -> Result<usize, WorkspaceError> {
    let unreadable = |error: std::io::Error| WorkspaceError::Refused {
        code: UPPER_UNREADABLE.to_string(),
        message: format!(
            "cannot read {} whole ({error}), so cannot tell it holds no transcript; kept",
            directory.display()
        ),
    };
    match std::fs::symlink_metadata(directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(unreadable(error)),
        // A symlink (or anything but a directory) is never followed.
        Ok(metadata) if !metadata.is_dir() => Err(unreadable(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "not a directory (a symlink is never followed)",
        ))),
        Ok(_) => {
            let mut found = Vec::new();
            list_transcripts(directory, Path::new(""), &mut found).map_err(unreadable)?;
            Ok(found.len())
        }
    }
}

/// Collect into `found` the path, relative to `root`, of every transcript
/// ([`is_transcript`]) under `root.join(relative)`. Fails on any directory
/// it cannot list or entry it cannot stat, naming it.
fn list_transcripts(root: &Path, relative: &Path, found: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let directory = root.join(relative);
    let named = |error: std::io::Error, path: &Path| {
        std::io::Error::new(error.kind(), format!("{}: {error}", path.display()))
    };
    for entry in std::fs::read_dir(&directory).map_err(|error| named(error, &directory))? {
        let entry = entry.map_err(|error| named(error, &directory))?;
        let path = relative.join(entry.file_name());
        let full = root.join(&path);
        let metadata = std::fs::symlink_metadata(&full).map_err(|error| named(error, &full))?;
        if metadata.is_dir() {
            list_transcripts(root, &path, found)?;
        } else if is_transcript(&full, &metadata) {
            found.push(path);
        }
    }
    Ok(())
}

/// Remove an overlay state (or probe) directory: the kernel's own
/// subdirectories of its workdir (`work/work`, `work/index`, which
/// overlayfs leaves mode 000) are made removable, and nothing else has its
/// permissions changed ([`remove_tree`]). Neither `work` nor those two is
/// followed when it is a symlink: only real directories are touched.
fn remove_state(state: &Path) -> std::io::Result<()> {
    let real_dir = |path: &Path| std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
    let work = state.join("work");
    if real_dir(&work) {
        for name in KERNEL_WORK_DIRS {
            let owned = work.join(name);
            if real_dir(&owned) {
                std::fs::set_permissions(&owned, std::fs::Permissions::from_mode(0o700))?;
            }
        }
    }
    remove_tree(state)
}

/// The directories overlayfs itself makes in a workdir.
const KERNEL_WORK_DIRS: [&str; 2] = ["work", "index"];

/// Remove `path` and everything under it. Symlinks are removed, never
/// followed, and no permission is changed: a directory that cannot be read
/// or emptied is an error. A transcript ([`is_transcript`]) is never
/// removed: meeting one is an error, whatever the caller checked before.
fn remove_tree(path: &Path) -> std::io::Result<()> {
    remove_tree_allowing(path, &std::collections::HashSet::new())
}

/// [`remove_tree`], except that the transcripts at the paths in `allowed`
/// (shown to be copies of a commit's files) are removed too.
fn remove_tree_allowing(
    path: &Path,
    allowed: &std::collections::HashSet<PathBuf>,
) -> std::io::Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() {
        if is_transcript(path, &metadata) && !allowed.contains(path) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("refusing to delete transcript {}", path.display()),
            ));
        }
        return std::fs::remove_file(path);
    }
    for entry in std::fs::read_dir(path)? {
        remove_tree_allowing(&entry?.path(), allowed)?;
    }
    std::fs::remove_dir(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    type Hook = Box<dyn FnOnce() + Send>;

    /// Code to run once at a named point of an operation
    /// ([`OverlayWorkspace::hook`]).
    #[derive(Default)]
    pub(super) struct Hooks(Mutex<Vec<(&'static str, Hook)>>);

    impl std::fmt::Debug for Hooks {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("Hooks")
        }
    }

    impl Hooks {
        fn set(&self, point: &'static str, hook: impl FnOnce() + Send + 'static) {
            self.0.lock().unwrap().push((point, Box::new(hook)));
        }

        pub(super) fn run(&self, point: &str) {
            let hook = {
                let mut hooks = self.0.lock().unwrap();
                hooks
                    .iter()
                    .position(|(at, _)| *at == point)
                    .map(|index| hooks.remove(index).1)
            };
            if let Some(hook) = hook {
                hook();
            }
        }
    }

    fn git(arguments: &[&str]) -> String {
        let output = crate::test_git_command().args(arguments).output().unwrap();
        assert!(
            output.status.success(),
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A scratch with a bare repository whose `main` holds `README`, and a
    /// store over it with branch `agent` mounted. `None` (the test skips)
    /// where no unprivileged overlay can be mounted.
    fn mounted(name: &str) -> Option<(crate::Scratch, PathBuf, OverlayWorkspace, WorkspaceId)> {
        let scratch = crate::Scratch::new(name);
        if let Err(why) = overlay_supported_in(&scratch) {
            eprintln!("skipped: no unprivileged overlayfs here: {why}");
            return None;
        }
        let checkout = scratch.join("seed");
        let repo = scratch.join("spawn.git");
        git(&["init", "--quiet", "-b", "main", checkout.to_str().unwrap()]);
        std::fs::write(checkout.join("README"), b"base\n").unwrap();
        let c = checkout.to_str().unwrap();
        git(&["-C", c, "add", "-A"]);
        git(&["-C", c, "commit", "--quiet", "-m", "seed"]);
        git(&["init", "--quiet", "--bare", repo.to_str().unwrap()]);
        git(&["-C", c, "push", "--quiet", repo.to_str().unwrap(), "main"]);
        let store = OverlayWorkspace::new(&repo, scratch.join("ws"));
        let agent = BranchName::new("agent".to_string()).unwrap();
        store.ensure_branch(&agent, None).unwrap();
        let workspace = store.mount(&agent).unwrap();
        Some((scratch, repo, store, workspace))
    }

    fn refused_code<T: std::fmt::Debug>(result: Result<T, WorkspaceError>) -> String {
        match result {
            Err(WorkspaceError::Refused { code, .. }) => code,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// A python3 process, its working directory `directory`, that makes
    /// itself not dumpable (`PR_SET_DUMPABLE` 0, as `spawn loop`, `serve`
    /// and `integrate` do), then waits for a line on stdin before appending
    /// `late` to `pending.txt`. `None` without python3.
    fn undumpable_in(directory: &Path) -> Option<Child> {
        let script = "import ctypes, sys\n\
                      assert ctypes.CDLL(None).prctl(4, 0, 0, 0, 0) == 0\n\
                      print('in', flush=True)\n\
                      sys.stdin.readline()\n\
                      open('pending.txt', 'a').write('late\\n')\n";
        let mut child = Command::new("python3")
            .args(["-I", "-c", script])
            .current_dir(directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .ok()?;
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line.trim(), "in");
        Some(child)
    }

    /// The owner decision of #556: the overlay's own kernel exclusion
    /// decides teardown, so a live non-dumpable client outliving the holder
    /// refuses `InUse` (not `InUseUnproven`), and the teardown completes
    /// once it is gone.
    #[test]
    fn a_live_non_dumpable_client_outliving_the_holder_refuses_in_use() {
        let Some((_scratch, _repo, store, workspace)) = mounted("overlay-user") else {
            return;
        };
        let merged = store.host_path(&workspace).unwrap();
        std::fs::write(merged.join("pending.txt"), b"pending\n").unwrap();
        let Some(mut child) = undumpable_in(&merged) else {
            eprintln!("skipped: no python3");
            return;
        };
        let state = store.state_dir(&workspace).unwrap();
        // The holder goes first: the client alone keeps the overlay alive.
        store.stop_holder(&state);
        assert_eq!(refused_code(store.unmount(&workspace)), IN_USE);
        assert!(state.join("upper/pending.txt").is_file());
        // ... and a second mount refuses too, never two overlays over one
        // upperdir.
        let agent = BranchName::new("agent".to_string()).unwrap();
        assert_eq!(refused_code(store.mount(&agent)), IN_USE);
        drop(child.stdin.take());
        assert!(
            child.wait().unwrap().success(),
            "the client lost its workspace"
        );
        assert_eq!(
            std::fs::read(state.join("upper/pending.txt")).unwrap(),
            b"pending\nlate\n"
        );
        assert_eq!(store.mount(&agent).unwrap(), workspace);
        store.unmount(&workspace).unwrap();
        assert!(!state.exists());
    }

    /// A mode-000 directory in the upperdir hiding a `*.jsonl` refuses the
    /// teardown, whatever the kernel says, and nothing is deleted, no
    /// permission is changed.
    #[test]
    fn a_hidden_transcript_refuses_the_teardown() {
        let Some((_scratch, _repo, store, workspace)) = mounted("overlay-hidden") else {
            return;
        };
        let upper = store.upper_dir(&workspace).unwrap();
        std::fs::create_dir_all(upper.join("hidden")).unwrap();
        std::fs::write(upper.join("hidden/session.jsonl"), b"{}\n").unwrap();
        let hidden = upper.join("hidden");
        std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o000)).unwrap();
        let readable = std::fs::read_dir(&hidden).is_ok();
        let refused = refused_code(store.unmount(&workspace));
        std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o700)).unwrap();
        if readable {
            // Root, or a filesystem that ignores the mode bits.
            assert_eq!(refused, TRANSCRIPT_IN_UPPER);
            std::fs::rename(hidden.join("session.jsonl"), hidden.join("session.kept")).unwrap();
            store.unmount(&workspace).unwrap();
            return;
        }
        assert_eq!(refused, UPPER_UNREADABLE);
        assert!(upper.join("hidden/session.jsonl").is_file());
        assert!(store.holder_pid(&workspace).is_some(), "still mounted");
    }

    /// #556, checkpoint bug 1: a publication that moved the ref but did not
    /// write `head` (here: the process stops between the two) is settled
    /// from `pending` on the next commit, not taken for a stale branch: the
    /// next commit, with nothing to add, is `Unchanged`.
    #[test]
    fn a_publication_without_its_head_is_settled_on_the_next_commit() {
        let Some((_scratch, repo, store, workspace)) = mounted("overlay-pending") else {
            return;
        };
        let merged = store.host_path(&workspace).unwrap();
        std::fs::write(merged.join("one.txt"), b"one\n").unwrap();
        let state = store.state_dir(&workspace).unwrap();
        let blocker = state.join("head.staged");
        let made = blocker.clone();
        // `head` cannot be written once the ref has moved.
        store.hooks.set("commit:published", move || {
            std::fs::create_dir(&made).unwrap();
        });
        assert!(store.commit(&workspace, "one").is_err());
        let published = git(&[
            &format!("--git-dir={}", repo.display()),
            "rev-parse",
            "agent",
        ]);
        assert!(state.join("pending").exists(), "nothing to settle");
        assert_ne!(read_line(&state.join("head")).unwrap(), published);
        std::fs::remove_dir(&blocker).unwrap();
        assert_eq!(
            store.commit(&workspace, "again").unwrap(),
            Commit::Unchanged
        );
        assert_eq!(read_line(&state.join("head")).unwrap(), published);
        assert!(!state.join("pending").exists());
        store.unmount(&workspace).unwrap();
    }

    /// #556, checkpoint bug 2: a state that holds work but lost its `lower`
    /// marker is never taken for a fresh workspace: refused
    /// ([`STATE_UNREADABLE`]) with everything kept.
    #[test]
    fn a_state_with_work_and_no_lower_is_never_a_fresh_start() {
        let Some((_scratch, _repo, store, workspace)) = mounted("overlay-no-lower") else {
            return;
        };
        let merged = store.host_path(&workspace).unwrap();
        std::fs::write(merged.join("pending.txt"), b"pending\n").unwrap();
        let state = store.state_dir(&workspace).unwrap();
        store.stop_holder(&state);
        std::fs::remove_file(state.join("lower")).unwrap();
        let agent = BranchName::new("agent".to_string()).unwrap();
        assert_eq!(refused_code(store.mount(&agent)), STATE_UNREADABLE);
        assert!(state.join("upper/pending.txt").is_file());
        assert!(!state.join("lower").exists());
    }

    /// Review of #556, finding 6: an empty file carrying the file marker
    /// `user.overlay.whiteout` is a whiteout as the kernel's lookup reads it,
    /// with or without its directory's `opaque=x` (which only filters
    /// readdir): the merged view no longer finds the lower file, and the
    /// checkpoint removes it. `trusted.*` markers are not this mount's. An
    /// xattr whiteout named `*.jsonl` is no transcript: it is removed with
    /// the state. A directory marked `opaque=y` drops the lower subtree.
    #[test]
    fn xattr_whiteouts_and_opaque_directories_are_read_as_removals() {
        let Some((_scratch, _repo, store, workspace)) = mounted("overlay-xattr") else {
            return;
        };
        let merged = store.host_path(&workspace).unwrap();
        std::fs::create_dir_all(merged.join("opaque")).unwrap();
        std::fs::write(merged.join("opaque/kept"), b"k\n").unwrap();
        let upper = store.upper_dir(&workspace).unwrap();
        store.stop_holder(&store.state_dir(&workspace).unwrap());
        let set = |path: &Path, name: &str, value: &[u8]| {
            rustix::fs::lsetxattr(path, name, value, rustix::fs::XattrFlags::empty())
        };
        if let Err(error) = set(&upper.join("opaque"), "user.overlay.opaque", b"y") {
            eprintln!("skipped: no user xattrs here: {error}");
            return;
        }
        // README: the file marker alone, no `x` on the directory.
        std::fs::write(upper.join("README"), b"").unwrap();
        set(&upper.join("README"), "user.overlay.whiteout", b"").unwrap();
        // A marked, empty `*.jsonl`: a whiteout, not a transcript.
        std::fs::write(upper.join("gone.jsonl"), b"").unwrap();
        set(&upper.join("gone.jsonl"), "user.overlay.whiteout", b"").unwrap();
        let lower = store.lower_commit(&workspace).unwrap();
        let diff = store.walk_upper(&upper, &lower).unwrap();
        for removed in [&b"README"[..], b"opaque", b"gone.jsonl"] {
            assert!(
                diff.removed.contains(&removed.to_vec()),
                "{:?}",
                diff.removed
            );
        }
        let added: Vec<&[u8]> = diff.added.iter().map(|item| item.path.as_slice()).collect();
        assert_eq!(added, [b"opaque/kept".as_slice()]);
        assert_eq!(count_jsonl(&upper).unwrap(), 0);
        // The kernel agrees: mounted again, the lower README is hidden.
        let agent = BranchName::new("agent".to_string()).unwrap();
        assert_eq!(store.mount(&agent).unwrap(), workspace);
        let merged = store.host_path(&workspace).unwrap();
        assert_eq!(
            std::fs::symlink_metadata(merged.join("README"))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound
        );
        store.unmount(&workspace).unwrap();
        assert!(!upper.exists());
    }

    /// The launch prefix takes the admission lease before it enters: a
    /// launch while a teardown has closed admission is refused
    /// ([`LAUNCH_REFUSED`]) and never gets in, and a launch whose recorded
    /// holder has stopped is refused ([`LAUNCH_STALE`]) before `nsenter`
    /// could reach a reused pid.
    #[test]
    fn a_launch_needs_the_lease_and_the_recorded_holder() {
        let Some((_scratch, _repo, store, workspace)) = mounted("overlay-lease") else {
            return;
        };
        let prefix = store.launch_arguments(&workspace).unwrap();
        let launch = move || {
            let mut arguments = prefix.iter();
            Command::new(arguments.next().unwrap())
                .args(arguments)
                .args(["sh", "-c", "pwd"])
                .stdin(Stdio::null())
                .output()
                .unwrap()
        };
        let inside = launch();
        assert!(inside.status.success(), "{inside:?}");
        assert_eq!(
            String::from_utf8_lossy(&inside.stdout).trim(),
            workspace.path()
        );
        let during = std::sync::Arc::new(Mutex::new(None));
        let (slot, racing) = (during.clone(), launch.clone());
        store.hooks.set("unmount:stop", move || {
            *slot.lock().unwrap() = Some(racing().status.code());
        });
        let state = store.state_dir(&workspace).unwrap();
        let holder = recorded_holder(&state).unwrap();
        store.unmount(&workspace).unwrap();
        assert_eq!(*during.lock().unwrap(), Some(Some(LAUNCH_REFUSED)));
        // The stopped holder's record, put back: a stale launch.
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(
            state.join("holder"),
            format!("{} {} {}\n", holder.pid, holder.namespace, holder.start),
        )
        .unwrap();
        assert_eq!(launch().status.code(), Some(LAUNCH_STALE));
    }

    /// Review 516 (a) R4 and R9: a process that comes to hold the overlay
    /// after the first look, while the holder is being stopped (here a host
    /// process that settles inside `host_path`), is caught by the look after
    /// the stop: the unmount refuses and deletes nothing. The `holder`
    /// record is kept, so the next mount refuses too
    /// instead of putting a second overlay over the upperdir.
    #[test]
    fn a_process_arriving_while_the_holder_stops_is_caught_after_the_stop() {
        let Some((_scratch, _repo, store, workspace)) = mounted("overlay-late-entrant") else {
            return;
        };
        let merged = store.host_path(&workspace).unwrap();
        std::fs::write(merged.join("pending.txt"), b"pending\n").unwrap();
        let late: std::sync::Arc<Mutex<Option<Child>>> = std::sync::Arc::default();
        let slot = late.clone();
        store.hooks.set("unmount:stop", move || {
            let mut child = Command::new("sh")
                .args(["-c", "echo in; read line; echo late >> pending.txt"])
                .current_dir(&merged)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut line = String::new();
            BufReader::new(child.stdout.take().unwrap())
                .read_line(&mut line)
                .unwrap();
            assert_eq!(line.trim(), "in");
            *slot.lock().unwrap() = Some(child);
        });
        let upper = store.upper_dir(&workspace).unwrap();
        assert_eq!(refused_code(store.unmount(&workspace)), IN_USE);
        assert!(upper.join("pending.txt").is_file());
        let agent = BranchName::new("agent".to_string()).unwrap();
        assert_eq!(refused_code(store.mount(&agent)), IN_USE);
        assert!(store.holder_pid(&workspace).is_none());

        let mut child = late.lock().unwrap().take().unwrap();
        drop(child.stdin.take());
        assert!(
            child.wait().unwrap().success(),
            "the late writer lost its workspace"
        );
        assert_eq!(store.mount(&agent).unwrap(), workspace);
        assert_eq!(
            std::fs::read(store.host_path(&workspace).unwrap().join("pending.txt")).unwrap(),
            b"pending\nlate\n"
        );
        store.unmount(&workspace).unwrap();
        assert!(!upper.exists());
    }

    /// Review 516 (a) R5: a transcript written after the first look, while
    /// the holder is being stopped, is caught by the count after the stop:
    /// refused, the upperdir kept where it was, and mountable again.
    #[test]
    fn a_transcript_written_while_the_holder_stops_is_kept() {
        let Some((_scratch, _repo, store, workspace)) = mounted("overlay-late-transcript") else {
            return;
        };
        let merged = store.host_path(&workspace).unwrap();
        store.hooks.set("unmount:stop", move || {
            std::fs::write(merged.join("late.jsonl"), b"{}\n").unwrap();
        });
        let upper = store.upper_dir(&workspace).unwrap();
        assert_eq!(refused_code(store.unmount(&workspace)), TRANSCRIPT_IN_UPPER);
        assert_eq!(std::fs::read(upper.join("late.jsonl")).unwrap(), b"{}\n");
        let agent = BranchName::new("agent".to_string()).unwrap();
        assert_eq!(store.mount(&agent).unwrap(), workspace);
        let merged = store.host_path(&workspace).unwrap();
        std::fs::rename(merged.join("late.jsonl"), merged.join("late.kept")).unwrap();
        store.unmount(&workspace).unwrap();
    }

    /// Review 516 (a) R17: a writer that moves the branch after the commit
    /// read it, and before the ref is updated, wins. The commit is refused
    /// as stale and never overwrites that writer's commit.
    #[test]
    fn a_branch_moved_during_the_commit_is_never_overwritten() {
        let Some((_scratch, repo, store, workspace)) = mounted("overlay-cas") else {
            return;
        };
        let merged = store.host_path(&workspace).unwrap();
        std::fs::write(merged.join("mine.txt"), b"mine\n").unwrap();
        let git_dir = format!("--git-dir={}", repo.display());
        let other = {
            let tree = git(&[&git_dir, "rev-parse", "main^{tree}"]);
            git(&[
                &git_dir,
                "commit-tree",
                &tree,
                "-p",
                "main",
                "-m",
                "other writer",
            ])
        };
        let (dir, target) = (git_dir.clone(), other.clone());
        store.hooks.set("commit:update-ref", move || {
            git(&[&dir, "update-ref", "refs/heads/agent", &target]);
        });
        assert_eq!(
            refused_code(store.commit(&workspace, "mine")),
            WORKSPACE_STALE
        );
        assert_eq!(git(&[&git_dir, "rev-parse", "agent"]), other);
        assert!(
            store
                .upper_dir(&workspace)
                .unwrap()
                .join("mine.txt")
                .is_file()
        );
        store.unmount(&workspace).unwrap();
    }

    /// Review 539 (a) N2: a recorded pid now running a stranger in the very
    /// namespace the record names (here the stranger's own, so no inode
    /// reuse is needed to make the case) is not the holder: its start time
    /// differs. It is neither trusted nor killed by the unmount.
    #[test]
    fn a_stranger_with_the_recorded_pid_and_namespace_is_not_the_holder() {
        let Some((_scratch, _repo, store, workspace)) = mounted("overlay-start-time") else {
            return;
        };
        let state = store.state_dir(&workspace).unwrap();
        let recorded = recorded_holder(&state).unwrap();
        store.stop_holder(&state);
        let mut stranger = Command::new("unshare")
            .args([
                "--user",
                "--map-root-user",
                "--mount",
                "sh",
                "-c",
                "echo in; read line",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(stranger.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line.trim(), "in");
        let namespace = mount_namespace(stranger.id()).unwrap();
        std::fs::write(
            state.join("holder"),
            format!("{} {namespace} {}\n", stranger.id(), recorded.start),
        )
        .unwrap();
        assert!(
            store.holder_pid(&workspace).is_none(),
            "the stranger was trusted"
        );
        assert!(store.command(&workspace, "true").is_err());
        store.unmount(&workspace).unwrap();
        assert!(
            matches!(stranger.try_wait(), Ok(None)),
            "the stranger was killed"
        );
        drop(stranger.stdin.take());
        stranger.wait().unwrap();
    }

    /// Review 539 (b) F2: prune renames a layer aside before deleting it, so
    /// a delete that fails half way (here a stray `*.jsonl` dropped into the
    /// layer after the look) never leaves a half layer under the commit's
    /// name. The next mount builds the commit's layer whole.
    #[test]
    fn a_prune_that_fails_half_way_leaves_no_half_layer_under_the_commit() {
        let Some((_scratch, repo, store, workspace)) = mounted("overlay-prune-aside") else {
            return;
        };
        let commit = store.lower_commit(&workspace).unwrap();
        // More entries, so the delete is half done when it meets the stray.
        let merged = store.host_path(&workspace).unwrap();
        for n in 0..40 {
            std::fs::write(merged.join(format!("f{n:02}")), b"x").unwrap();
        }
        let git_dir = format!("--git-dir={}", repo.display());
        let Commit::Created(id) = store.commit(&workspace, "files").unwrap() else {
            panic!("expected a commit");
        };
        store.unmount(&workspace).unwrap();
        assert_eq!(store.prune_layers().unwrap(), 1, "the old layer {commit}");
        let agent = BranchName::new("agent".to_string()).unwrap();
        let workspace = store.mount(&agent).unwrap();
        assert_eq!(store.lower_commit(&workspace).unwrap(), id.as_str());
        store.unmount(&workspace).unwrap();
        let layer = store.layer_path(id.as_str());
        let layers = store.root().join(".layers");
        let target = id.as_str().to_string();
        let (into, layer_for_hook) = (layers.clone(), layer.clone());
        store.hooks.set("prune:remove", move || {
            // Whatever is about to be deleted: the trash, or the layer.
            let doomed = std::fs::read_dir(&into)
                .unwrap()
                .flatten()
                .map(|entry| entry.path())
                .find(|path| {
                    path.file_name()
                        .and_then(OsStr::to_str)
                        .is_some_and(|name| name.starts_with(&format!(".trash-{target}")))
                })
                .unwrap_or(layer_for_hook);
            std::fs::write(doomed.join("zz-stray.jsonl"), b"{}\n").unwrap();
        });
        let refused = store.prune_layers();
        assert!(
            matches!(&refused, Err(WorkspaceError::Refused { code, .. }) if code == TRANSCRIPT_IN_LAYER),
            "{refused:?}"
        );
        assert!(
            !layer.exists(),
            "a half-deleted layer kept its commit's name"
        );
        let workspace = store.mount(&agent).unwrap();
        let tracked = git(&[&git_dir, "ls-tree", "-r", "--name-only", id.as_str()]);
        for path in tracked.lines() {
            assert!(
                layer.join(path).exists(),
                "{path} missing from the rebuilt layer"
            );
        }
        store.unmount(&workspace).unwrap();
    }

    /// A symlink in the upperdir is read, never followed: a link to `../../`
    /// adds a link (mode 120000) and reaches nothing outside.
    #[test]
    fn symlinks_are_read_never_followed() {
        let Some((_scratch, _repo, store, workspace)) = mounted("overlay-links") else {
            return;
        };
        let merged = store.host_path(&workspace).unwrap();
        std::os::unix::fs::symlink("../../etc/passwd", merged.join("escape")).unwrap();
        let upper = store.upper_dir(&workspace).unwrap();
        let lower = store.lower_commit(&workspace).unwrap();
        let diff = store.walk_upper(&upper, &lower).unwrap();
        let escape = diff
            .added
            .iter()
            .find(|item| item.path == b"escape")
            .expect("the link is a file of the diff");
        assert_eq!(escape.mode, "120000");
        assert!(matches!(&escape.source, FileOrigin::Bytes(bytes) if bytes == b"../../etc/passwd"));
    }

    #[test]
    fn a_missing_workdir_is_an_unknown_answer_not_free() {
        let scratch = crate::Scratch::new("overlay-nolock");
        assert!(matches!(probe_work_lock(&scratch), WorkLock::Unknown(_)));
    }

    /// Review of #556, finding 2: only util-linux's wording of `EBUSY`,
    /// after the probe's own target is cut off, is busy. A target whose path
    /// holds `busy` (or `already mounted`) and fails for another reason is
    /// unknown.
    #[test]
    fn mount_answers_are_classified_in_the_c_locale() {
        let target = Path::new("/x/busy/merged");
        assert_eq!(classify(Some(0), "", target), WorkLock::Free);
        for busy in [
            "mount: /x/busy/merged: overlay already mounted on /y.\n       dmesg(1) may …",
            "mount: /x/busy/merged: target is busy.",
            "mount: /x/busy/merged: fsconfig system call failed: Device or resource busy.",
        ] {
            assert!(
                matches!(classify(Some(32), busy, target), WorkLock::Busy(_)),
                "{busy}"
            );
        }
        for unknown in [
            "mount: /x/busy/merged: permission denied.",
            "mount: /x/busy/merged: overlay: no such device",
            "mount: /elsewhere: overlay already mounted on /y.",
            "mount: /x/busy/merged: wrong fs type, bad option, busy superblock?",
        ] {
            assert!(
                matches!(classify(Some(32), unknown, target), WorkLock::Unknown(_)),
                "{unknown}"
            );
        }
        let busy = "mount: /x/busy/merged: overlay already mounted on /y.";
        assert!(matches!(
            classify(Some(97), busy, target),
            WorkLock::Unknown(_)
        ));
        assert!(matches!(classify(None, "", target), WorkLock::Unknown(_)));
    }

    /// Review of #556, finding 2: a store root where the probe was not shown
    /// to work never trusts it: teardown refuses `InUseUnproven`, and nothing
    /// is deleted.
    #[test]
    fn an_unsupported_store_root_never_trusts_the_probe() {
        let Some((_scratch, _repo, store, workspace)) = mounted("overlay-unsupported") else {
            return;
        };
        let upper = store.upper_dir(&workspace).unwrap();
        let mut store = store;
        store.supported = std::sync::OnceLock::from(Err("pretend".to_string()));
        assert_eq!(refused_code(store.unmount(&workspace)), IN_USE_UNPROVEN);
        assert!(upper.is_dir());
        let agent = BranchName::new("agent".to_string()).unwrap();
        assert!(matches!(
            store.mount(&agent),
            Err(WorkspaceError::Unavailable(_))
        ));
    }

    /// Review of #556, finding 4: a transcript named exactly `.jsonl` is a
    /// transcript (`Path::extension` sees none); a symlinked upper root is
    /// never followed; cleanup changes the mode of the kernel's own `work/`
    /// directories only, and never through a symlinked `work`.
    #[test]
    fn transcripts_by_suffix_and_no_symlinked_roots() {
        let scratch = crate::Scratch::new("overlay-suffix");
        let tree = scratch.join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join(".jsonl"), b"{}\n").unwrap();
        assert_eq!(count_jsonl(&tree).unwrap(), 1);
        assert!(remove_tree(&tree).is_err());
        assert_eq!(std::fs::read(tree.join(".jsonl")).unwrap(), b"{}\n");
        let link = scratch.join("upper");
        std::os::unix::fs::symlink(&tree, &link).unwrap();
        assert_eq!(refused_code(refuse_transcripts(&link)), UPPER_UNREADABLE);
        // A state whose `work` is a symlink to a mode-000 directory: the
        // target keeps its mode, and only the link goes.
        let outside = scratch.join("outside");
        std::fs::create_dir_all(outside.join("work")).unwrap();
        std::fs::set_permissions(outside.join("work"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        let state = scratch.join("state");
        std::fs::create_dir_all(&state).unwrap();
        std::os::unix::fs::symlink(&outside, state.join("work")).unwrap();
        remove_state(&state).unwrap();
        let mode = std::fs::symlink_metadata(outside.join("work"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0, "a mode was changed through a symlink");
        // A real `work` with a user's mode-000 directory beside the kernel's:
        // only `work/work` and `work/index` are opened up.
        let state = scratch.join("state2");
        for name in ["work/work", "work/index", "work/mine"] {
            std::fs::create_dir_all(state.join(name)).unwrap();
            std::fs::set_permissions(state.join(name), std::fs::Permissions::from_mode(0o000))
                .unwrap();
        }
        let readable = std::fs::read_dir(state.join("work/mine")).is_ok();
        assert!(remove_state(&state).is_err() || readable);
        let mode = |name: &str| {
            std::fs::symlink_metadata(state.join(name))
                .map(|m| m.permissions().mode() & 0o777)
                .ok()
        };
        if !readable {
            assert_eq!(mode("work/mine"), Some(0), "a user directory was chmod'ed");
        }
        std::fs::set_permissions(outside.join("work"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        if state.join("work/mine").exists() {
            std::fs::set_permissions(
                state.join("work/mine"),
                std::fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
    }

    /// Review of #556, finding 5: a ref lookup that fails (here the
    /// repository cannot be read) is an error, not "no branch": the
    /// publication in flight stays in `pending`, and is settled once the
    /// repository reads again.
    #[test]
    fn a_failed_ref_lookup_keeps_the_publication_in_flight() {
        let Some((_scratch, repo, store, workspace)) = mounted("overlay-lookup") else {
            return;
        };
        let merged = store.host_path(&workspace).unwrap();
        std::fs::write(merged.join("one.txt"), b"one\n").unwrap();
        let state = store.state_dir(&workspace).unwrap();
        let blocker = state.join("head.staged");
        let made = blocker.clone();
        store.hooks.set("commit:published", move || {
            std::fs::create_dir(&made).unwrap();
        });
        assert!(store.commit(&workspace, "one").is_err());
        std::fs::remove_dir(&blocker).unwrap();
        let published = git(&[
            &format!("--git-dir={}", repo.display()),
            "rev-parse",
            "agent",
        ]);
        let hidden = repo.with_extension("hidden");
        std::fs::rename(&repo, &hidden).unwrap();
        let failed = store.commit(&workspace, "again");
        std::fs::rename(&hidden, &repo).unwrap();
        assert!(
            matches!(failed, Err(WorkspaceError::Unavailable(_))),
            "{failed:?}"
        );
        assert!(state.join("pending").exists(), "pending was dropped");
        assert_eq!(
            store.commit(&workspace, "again").unwrap(),
            Commit::Unchanged
        );
        assert_eq!(read_line(&state.join("head")).unwrap(), published);
        store.unmount(&workspace).unwrap();
    }

    #[test]
    fn remove_tree_never_deletes_a_transcript() {
        let scratch = crate::Scratch::new("overlay-remove-tree");
        let tree = scratch.join("tree");
        std::fs::create_dir_all(tree.join("deep/er")).unwrap();
        std::fs::write(tree.join("deep/er/session.jsonl"), b"{}\n").unwrap();
        std::fs::write(tree.join("deep/other.txt"), b"x").unwrap();
        let error = remove_tree(&tree).unwrap_err();
        assert!(error.to_string().contains("session.jsonl"), "{error}");
        assert_eq!(
            std::fs::read(tree.join("deep/er/session.jsonl")).unwrap(),
            b"{}\n"
        );
        std::fs::rename(
            tree.join("deep/er/session.jsonl"),
            scratch.join("session.kept"),
        )
        .unwrap();
        remove_tree(&tree).unwrap();
        assert!(!tree.exists());
    }

    #[test]
    fn count_jsonl_fails_closed_on_an_unreadable_directory() {
        let scratch = crate::Scratch::new("overlay-count-jsonl");
        let upper = scratch.join("upper");
        std::fs::create_dir_all(upper.join("hidden")).unwrap();
        std::fs::write(upper.join("hidden/session.jsonl"), b"{}\n").unwrap();
        assert_eq!(count_jsonl(&upper).unwrap(), 1);
        assert_eq!(count_jsonl(&scratch.join("missing")).unwrap(), 0);
        let hidden = upper.join("hidden");
        std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o000)).unwrap();
        let readable = std::fs::read_dir(&hidden).is_ok();
        let counted = count_jsonl(&upper);
        let mode = std::fs::symlink_metadata(&hidden)
            .unwrap()
            .permissions()
            .mode();
        std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(mode & 0o777, 0, "the count changed a permission");
        if !readable {
            assert!(
                matches!(&counted, Err(WorkspaceError::Refused { code, .. }) if code == UPPER_UNREADABLE),
                "{counted:?}"
            );
        }
    }

    /// The fail-closed rule for the holder record: an older record (two
    /// fields) names no holder, and a stranger's pid with the recorded
    /// start time is caught by the start time.
    #[test]
    fn a_holder_record_is_trusted_only_whole_and_by_its_start_time() {
        let scratch = crate::Scratch::new("overlay-holder-record");
        std::fs::write(scratch.join("holder"), b"12 pid:[1] 0:45 99\n").unwrap();
        assert!(recorded_holder(&scratch).is_none(), "an old record");
        std::fs::write(scratch.join("holder"), b"12 pid:[1]\n").unwrap();
        assert!(recorded_holder(&scratch).is_none());
        let me = std::process::id();
        let start = process_start(me).unwrap();
        std::fs::write(scratch.join("holder"), format!("{me} pid:[1] {start}\n")).unwrap();
        assert_eq!(
            recorded_holder(&scratch),
            Some(Holder {
                pid: me,
                namespace: "pid:[1]".to_string(),
                start
            })
        );
        assert!(runs_as(me, start));
        assert!(!runs_as(me, start + 1), "a reused pid is another process");
    }
}
