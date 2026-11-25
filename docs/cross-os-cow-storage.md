# Cross-OS copy-on-write storage

Spawn runs many agents on one checkout. Each one needs a workspace of its
own, and each one builds, so the same `target/` or `node_modules/` gets
written over and over again. This document covers the storage layer
that makes those workspaces and build outputs cheap. It needs no VM, no
FUSE daemon, no compiler wrapper and no change to any agent, compiler or
IDE.

Code:

- `crates/spawn-host/src/cow.rs`: the clone primitives (`reflink_file`,
  `clone_or_copy`, `cow_clone_supported`) and `CowWorkspace`, a
  `spawn_core::port::WorkspaceStore`.
- `crates/spawn-host/src/cas_dedup.rs`: `dedup_dirs` / `prune_unused`, the
  content-addressed deduplicator for build artifacts.
- `crates/spawn-host/tests/cow.rs`: the tests and an `#[ignore]`d benchmark
  (`cargo test -p spawn-host --test cow -- --ignored --nocapture`).

`GitWorkspace` (`git.rs`) is still the default store. `CowWorkspace` is an
opt-in alternative and changes nothing for existing deployments.

## Why plain POSIX paths

Every tool an agent runs (`cargo`, `rustc`, `npm`, `tsc`, `pytest`,
language servers, editors, `git` itself) already understands one thing:
a directory full of ordinary files. So the storage layer adds nothing
on top of that:

- A workspace is a normal directory at `<root>/<branch>`. You can
  `stat`, `mmap`, `inotify`, `flock`, `rename` and `chmod` it like any
  other directory, and every syscall acts on real files on the host's own
  file system. Nothing is proxied through a userspace daemon.
- The sharing lives below the VFS, in the file system's extent tree. Two
  workspaces cloned from the same baseline share their data blocks until
  one of them writes. That write is an ordinary write, and the kernel
  splits the extent (copy-on-write).
- Deduplicated build artifacts keep their path, mode and mtime. Freshness
  checks in `cargo`, `make` and `npm` therefore see nothing change, and
  nothing rebuilds. The file does get a new inode and ctime, and loses
  any extended attributes or ACLs it had (the clone is a new file): a tool
  that keys on the inode, or on an xattr, sees a different file.

Overlay mounts, FUSE, NFS-backed or VM-backed workspaces each break some
part of this. Common failures are inotify that does not fire, mmap
coherency, `rename(2)` across layers, slow metadata, and root needed to
mount. Clones break none of it.

## Architecture per OS

| OS | Clone primitive | Change feed | Notes |
| --- | --- | --- | --- |
| Linux | `ioctl(FICLONE)` / `FICLONERANGE` (btrfs, XFS `reflink=1`, bcachefs) | inotify (`watch.rs`) | Implemented. `overlayfs` remains an option for a read-only Layer 0 when the backing fs cannot clone |
| macOS | `clonefile(2)` on APFS | `FSEvents` (future) | `fclonefileat` by descriptor through `rustix` (already locked), with a descriptor-based copy fallback. `FSKit` is a future path to a native lazily-materialised workspace |
| Windows | Block cloning (`FSCTL_DUPLICATE_EXTENTS_TO_FILE`) on ReFS / Dev Drive | `ReadDirectoryChangesW`; `ProjFS` for lazy materialisation | Design only: not built (Spawn's host adapters are Unix-only today) |

### Linux

Every clone goes through one primitive, `clone_at`, between an open
source and a directory descriptor. On Linux it creates the target
(`O_EXCL | O_NOFOLLOW`) and calls `FICLONE`, which is `FICLONERANGE` over
the whole file, through `rustix`. The workspace sets `unsafe_code =
"forbid"`, so no raw `libc` FFI is used. A file system that cannot clone
(ext4, tmpfs, or a clone that would cross file systems) answers
`EOPNOTSUPP`/`EXDEV`/`EINVAL`. `reflink_file` maps that to
`io::ErrorKind::Unsupported` and removes the half-made target;
`clone_or_copy` (and a mount) copy the bytes from the open source into
the exclusively made target instead, so the store works everywhere, just
more slowly. `cow_clone_supported(dir)` asks
the file system directly: it clones a 16 KiB probe file, which is above
btrfs's inline-extent limit.

You can verify the result with `filefrag -v`. The tests compare physical
extents and check the `shared` flag.

An `overlayfs` lower layer is a valid alternative for Layer 0 on file
systems without reflinks. It needs a mount (root or a user namespace),
and an unprivileged agent cannot rely on that. So the MVP copies instead.

### macOS

`clonefile(2)` makes an APFS clone in O(1) metadata work, with no data
written. `clone_at` calls `rustix::fs::fclonefileat` (safe, Apple-only,
already in `Cargo.lock` as `rustix 1.1.6`) with the already open source
and the target's directory descriptor, `CLONE_NOFOLLOW`:

- `EEXIST` is returned as is: a target is never overwritten, even one
  created concurrently (there is no separate existence check).
- `ENOTSUP`, `EOPNOTSUPP`, `EXDEV` or `ENOSYS` (a volume that cannot
  clone, another device) mean "cannot clone": `reflink_file` answers
  `Unsupported` and leaves no target; `clone_or_copy` and a mount fall
  back to an exclusive create (`O_EXCL | O_NOFOLLOW`) by descriptor and a
  copy from the open source. A copy is counted as `Copied`, never as a
  clone.
- A finished clone copies the source's mode, so a 0444 CAS blob makes a
  0444 clone. The clone is reopened read-only; its mode and times are
  then set through that descriptor (its owner may), never by a writable
  open.

Not verified here: the Apple branches are not compiled or run by this
repository's checks (no Apple target on the gate host). A native macOS
run of `cargo test -p spawn-host --test cow` on APFS covers the existing
target (`reflink_refuses_an_existing_target`), the 0444 source
(`a_read_only_source_clones_and_takes_new_metadata`) and clone versus
copy accounting (`workspaces_are_isolated_cow_clones_committed_by_manifest`).

Future work: an `FSEvents` stream could feed the dirty-path list. The
manifest diff below is already cheap, but on very large trees FSEvents
would remove even the `lstat` walk. `FSKit` (macOS 15+) could serve a
workspace whose files materialise on first access.

### Windows (design)

Dev Drive volumes are ReFS, and ReFS supports block cloning
(`FSCTL_DUPLICATE_EXTENTS_TO_FILE`), the same model as `FICLONERANGE`.
`ProjFS` (the Projected File System, which VFS for Git uses) can project
the baseline and hydrate files on demand. The workspace is still an
ordinary NTFS/ReFS path, so tools keep working. Neither is implemented
yet.

## `CowWorkspace`

Layout under the store's root:

```text
<root>/<branch>/                        the workspace (plain directory)
<root>/.cow/baselines/<commit>/tree/    Layer 0: read-only files of <commit>
<root>/.cow/baselines/<commit>/entries  `git ls-tree -r -z <commit>`
<root>/.cow/state/<branch>              head + mounted generation + manifest
<root>/.cow/state/<branch>.installing   a mount's state, before its rename
<root>/.cow/locks/<branch>.lock         the branch's flock
<root>/.cow/locks/<branch>.admission    the workspace's admission lock
<root>/.cow/swaps/<branch>              both trees' places while a mount swaps
<root>/.cow/kept/<label>@<pid>-<n>/     what was kept instead of deleted
```

- **Tree checks.** Before a byte of a commit is written, its listing
  (`git ls-tree -r`) is checked; a tree that fails is refused as
  `UnsafeTree`:
  - no empty, `.` or `..` component and no NUL;
  - no `.git` component in any spelling git refuses: any case, trailing
    dots or spaces, `git~1`, or with HFS+ ignorable code points;
  - only the modes `100644`, `100755`, `120000`, `160000`;
  - no duplicate path, and no path below another entry (a symlink, file
    or gitlink as a leading directory), compared case-folded.

  Every baseline and workspace file is then created relative to a
  directory descriptor, one component at a time, with `O_NOFOLLOW` and
  `O_EXCL`, so even a tree that got past the checks could not write
  through a link.
- **Locking.** Mount, commit and unmount of a branch hold its `flock`
  (`.cow/locks/<branch>.lock`, the Git store's own branch-lock idiom), so
  two stores or processes on one root take turns. Building and cloning a
  baseline holds the baselines' shared lock; `prune_baselines()` takes it
  exclusive, and refuses to prune on a state it cannot read.
- **Git.** All git plumbing, branch creation (`ensure_branch`), branch
  resolution and `branch_head` are the Git store's (`GitWorkspace` on the
  same repository); only the baseline reader and the manifest are this
  store's.
- **Ownership.** The state records the mounted generation: the
  `(device, inode)` of the directory the mount installed. Commit,
  replacement, unmount and `offline` refuse (`WorkspaceNotOwned`, nothing
  changed) a directory without a state or that is not that generation:
  a Git store's workspace of the same name, or a directory put in its
  place, is never deleted. Keep the store's root separate from the Git
  store's live root. Only `NotFound` counts as "absent"; a state or
  directory that cannot be read is an error.
- **Workspace lease (`WorkspaceLease`).** Each workspace has an admission lock
  (`.cow/locks/<branch>.admission`). The lock is found from the workspace
  (`<parent>/.cow/locks/<name>.admission` for `<parent>/<name>`), never
  named by the caller, and the guard records the workspace directory's
  generation when it was taken. A managed worker takes the shared
  side (`CowWorkspace::lease`) before it starts and holds it for its
  whole life. Replacement, unmount and `CowWorkspace::offline` (for
  `cas_dedup`) take the exclusive side without waiting and hold it
  throughout; while a lease is held they are refused (`WorkspaceBusy`),
  and meanwhile no worker is admitted. A lock that only looks held
  because a child inherited its descriptor between `fork` and `exec` is
  retried for up to a second. Users outside the managed lifecycle (an
  IDE, a shell) must be stopped first; the process-table look below only
  adds a snapshot of them. Wiring the lease into the launcher is not part
  of this change. `spawn-cli` runs this store when asked
  (`SPAWN_WORKSPACE_BACKEND=cow`, or `auto` where the file system clones:
  docs/storage.md, "Workspace backends").

- **Layer 0.** The first mount of a commit writes its blobs once, with
  their exact bytes (`git cat-file --batch`, no checkout filters), as
  read-only files. Each blob is streamed into its new file and its byte
  count and trailing newline are checked. On any failure the reader is
  closed and `git cat-file` is killed and reaped before its input thread
  is joined, so a full pipe can never hang the mount. It builds them beside the final name and renames them
  into place, so concurrent mounts of the same commit share one
  baseline. `prune_baselines()` removes the baselines that no mounted
  workspace is at.
- **Mount.** Each workspace is cloned from Layer 0 file by file
  (`clone_or_copy`), with modes taken from git (`0644`/`0755`) and
  symlinks recreated; a submodule gitlink becomes an empty directory and
  is carried over unchanged by every commit while that directory is
  there (files inside it are the submodule's, never committed here). The
  clone goes into `.cow/stage/` and is renamed into place; a failed rename
  removes the staging directory. If a workspace already sits at the
  branch head with nothing uncommitted, it is reused as it is. If it
  holds uncommitted work, the mount is refused (`WorkspaceDirty`) and the
  directory is left alone. A clean workspace at an older head is replaced
  only under its exclusive admission lock, while no process is seen in it
  (`WorkspaceBusy`, below) and after the strict transcript preflight: it is
  renamed out of the way, checked clean and scanned again, its
  `.transcripts/` and `.spawn/` move to the new tree, and the new tree is
  renamed in. Any failing step undoes the earlier ones, so the old
  workspace and its transcripts are back where they were (or the error
  says where they are). The new state is written as
  `<branch>.installing` before the final rename and moved into place
  after it; if that last move fails the mount still succeeds, says so in
  `CowWorkspace::diagnostics()`, and the next operation adopts the record
  whose generation is the directory now there (a record whose rename never
  happened is dropped). Before either tree moves, a swap record
  (`.cow/swaps/<branch>`) names both places: the staged new tree and where
  the old workspace goes. A mount interrupted at any point is settled by
  the next operation on the branch: after the final rename the new tree's
  state is moved into place and the old tree retired (no admission lock is
  needed, so a leased worker's commit goes on); before it, under the
  exclusive admission lock, the carried `.transcripts/` and `.spawn/` go
  back into the old workspace, which goes back in place, and the staged
  tree is removed. Removing the retired tree after a successful swap
  never fails the mount either: what cannot be removed stays under
  `.cow/kept/` or `.cow/stage/` and is named in the diagnostics. Untracked files in skipped
  directories (`target/`, a nested `.git/`) do not count as uncommitted
  work and are deleted by such a replacement, unless they include a
  `*.jsonl` file (then the old tree is kept under `.cow/kept/`).
- **Manifest.** For each committed path the manifest records its git mode,
  its blob id, and the `lstat` stamp the file had when that blob id was
  last known to be right: size, inode, mtime, ctime.
- **Commit.** What a commit takes is decided by the Git store's rules
  (`GitWorkspace::checkpoint_paths`, shared with its own checkpoint):
  `git ls-files --cached --others --exclude-standard` against the head
  (so `.gitignore`, `info/exclude` and the build-output excludes apply,
  and an ignored file such as `.env` or a `dist/` bundle is never added),
  never an engine runtime file (`ENGINE_RUNTIME_FILES`, the same exclude
  pathspecs) unless both the head and `main` track it, and no file over
  the size cap (`SPAWN_MAX_FILE_MB`) unless `main` holds it as a blob. A
  file left out for its size is recorded and told to the agent once,
  through the Git store's own notices. On top of git's rules, no untracked
  file under `target/`, `.spawn/`, `.transcripts/` or `.git/` (at any
  depth) is added; a path the branch already tracks there (a committed
  `target/config`) is checkpointed like any tracked path, edits and
  deletion included. Each listed path is then read with `lstat` only: a
  file whose stamp is unchanged reuses its blob id unread, and only files
  whose stamp changed are hashed, in batches (`git hash-object -w
  --no-filters`). The tree is built in a throwaway index (`update-index
  --index-info`, `write-tree`).
  - **Racy-clean rule.** A stamp is trusted only if it is older than a
    *fence*: a timestamp read from the file system's own clock (a probe
    file touched in `.cow/tmp`) before the walk. A change after the walk
    therefore always moves the stamp, however coarse the clock. A mount
    waits until the clock has passed its newest clone before it hands the
    tree out, so the first commit trusts every untouched file.
- **Compare-and-swap.** The branch moves with `git update-ref <ref> <new>
  <old>`.
  - A branch someone else moved meanwhile is refused as `WorkspaceStale`.
    The workspace's state is kept first on
    `refs/spawn-cow/stale/<branch>/<commit>`: one ref per kept state,
    never overwritten. If that ref cannot be made, the error says so
    instead of claiming the state was kept.
  - The move is written to the state (`next`) before `update-ref` runs, so
    a state that cannot be saved after a successful move does not make
    the next commit report a false `WorkspaceStale`.
  - A branch deleted under the workspace is recreated from it, with an
    empty old value (`Commit::Recreated`).
- **Unmount.** Before deleting anything, unmount applies these guards,
  all under the branch lock and the exclusive admission lock:
  - It refuses (`WorkspaceNotOwned`) a directory that is not the
    generation this store mounted.
  - It refuses (`WorkspaceBusy`) while a worker holds the lease, or while
    any process is seen with its working directory, root, an open file or
    a mapped file inside the workspace. On Linux this reads `/proc` thread
    by thread; processes of other users are not visible, and a process of
    this user whose links the kernel withholds is reported in
    `CowWorkspace::diagnostics()` (and in a refusal), not taken as absent.
    On macOS it runs `lsof +D`; a failure, or an exit 1 with a diagnostic,
    refuses with `lsof`'s message.
  - It refuses, with nothing deleted, while the workspace holds any
    `*.jsonl` outside its `.transcripts/` (the "0 `*.jsonl`" guard).
  - It fails closed: a directory or entry it cannot read (a mode-000
    directory included) refuses the unmount (`WorkspaceUnreadable`) with
    nothing deleted.
  - The workspace is retired in one rename first. Only then is its
    `.transcripts/` moved under `.cow/kept/`, never deleted, and the rest
    removed; a `*.jsonl` that appeared after the preflight keeps the whole
    retired tree under `.cow/kept/`.
  - Branch names are limited to `[A-Za-z0-9_-]`. The external transcript
    archive `<root>/.transcripts` therefore can never be named as a
    workspace, and it is never touched.
  - `remove_dir_all` never follows a symlink out of the tree.

Not implemented in this MVP (the trait's defaults answer): merging `main`
into a mounted workspace, and set-aside.

Benchmark on the Linux 7.2 btrfs dev VM, 2,005-file tree: a CoW mount
takes 70–190 ms and an unchanged commit about 25 ms.

## `cas_dedup`

**Contract: offline artifact roots only.** A replacement is a rename over
the file. A process that holds the file open (a log appender, a linker,
an `mmap` writer) would keep writing to the old, unlinked inode, and its
writes would vanish from the path; no stat, age threshold or advisory
lock of the pass can see such a writer. So `dedup_dirs` takes only
`ArtifactRoot`s:

- a plain directory named `target` or `node_modules` (disposable build
  output), never a source tree or an arbitrary root (`NotArtifact`);
- borrowing the exclusive side of its owner's admission lock for the
  whole pass (`CowWorkspace::offline(&workspace)`, or
  `WorkspaceLease::exclusive(workspace)` for a caller's own workspaces); a
  worker's lease is refused (`NotOffline`). No managed worker runs in the
  root or starts during the pass;
- bound to that workspace: the root must resolve, through every link, to
  `<workspace>/<name>` of the workspace the guard holds, be a plain
  directory (not a link), and that workspace must still be the generation
  the guard was taken for (`NotArtifact`, `NotOffline`). The guard of A
  never admits B's artifacts, through a link or a linked ancestor; the
  root and its workspace are checked again when the walk starts.

A root in which any process (the caller included) is seen with its
working directory, an open descriptor or a mapped file is still refused
whole with `DedupError::Live { directory, pids }`. Rename replaces the
inode and carries only mode and times, so files of another user and files
with extended attributes (an ACL included) are left alone, and the staged
clone is given the file's group and refused unless it then has the file's
owner, group and no extended attribute (a directory's default ACL).
A file that is gone, or that became a link, since it was listed is left
alone; one that cannot be opened or read for another reason (`EACCES`,
`EIO`) is an error and marks its root incomplete.

`dedup_dirs(&DedupConfig, &[ArtifactRoot])` walks the roots (no
symlinks followed) and hashes every eligible regular file with SHA-256,
using the `sha2` crate the workspace already links (no new crate, so it
still builds offline). It then does one of two things:

- **First file of its content.** The file is cloned to
  `<cas_root>/tmp/…`. The clone is re-hashed to prove it is what was
  hashed, made read-only, and published as
  `<cas_root>/<hh>/<sha256>` with a hard link, which never replaces an
  existing blob. A concurrent pass that publishes first wins, and this
  file is then treated as a duplicate. The source file already shares the
  blob's extents.
- **Duplicate.** The blob is opened with `O_NOFOLLOW` and must be a
  regular file with one link and the file's size. It is cloned beside the
  file as `.<name>.cas-<pid>-<n>`, and the clone is hashed: a blob that
  does not hold the content its name says (tampered or planted) is
  refused and reported in `errors`, and the file is left as it was. The
  clone is given the file's mode, atime and mtime, then renamed over the
  file, atomically, but only if the file's stamp is still the one that
  was hashed.

The pass skips:

- symlinks and special files, and a file swapped for either after it was
  listed (it is opened `O_NOFOLLOW | O_NONBLOCK`, so a FIFO never blocks
  the pass);
- files under `min_size` (64 KiB by default);
- `*.jsonl` files and anything under `.transcripts/`;
- files with more than one hard link;
- files changed within `min_age` (2 s by default);
- files another process holds an `flock` on (cargo's lock files, for
  example). The pass holds its own `flock` while it works;
- files of another user, and files with extended attributes.

A directory or entry the walk cannot read is reported in `errors`, and its
root is listed in `incomplete`; nothing is skipped silently.

A file system without clones changes nothing (`cow_supported: false`). A
hard link would share later writes, and a copy saves no space.

A CAS root carries the marker file `.spawn-cas`, made by the first pass.
A non-empty directory without it is refused as a CAS root
(`DedupError::NotCas`).

`<cas_root>/index` records each file made a clone (path, dev, inode,
size, mtime, ctime, digest), under an `flock`. A second pass skips those
files unread. A pass holds `<cas_root>/publish.lock` shared from its first
blob to its last index entry; a prune holds it exclusively, so it never
removes a blob or temporary between its creation and its index entry.
Both publish the index whole: written beside it, synced, and renamed over
it under `index.lock`, so a reader never sees a torn row. An index that
cannot be read, or holds a line that does not parse, is an error. Prune
drops an entry only when its file is gone or rewritten; one whose file
cannot be looked at is kept (with its blob) and reported. `prune_unused(cas_root, grace)` does three things:

- drops the entries whose file is gone or rewritten;
- removes the blobs no live entry references, once they are older than
  `grace`;
- clears stale temporaries.

It refuses a root without the marker, and removes only names a pass
makes: `<hh>/<hash>` with a 64-digit lowercase hex hash inside its own
shard, and `tmp/<hash>.<pid>-<n>`. Anything else, a `*.jsonl` file
included, stays.

Removing a blob is always safe. The file system refcounts shared extents,
so a pruned blob only stops being a clone source, and no workspace file
changes.

Measured on btrfs: 4 workers × 24 identical 1 MiB artifacts converge to
24 blobs in ~300 ms. That pass reclaims 72 MiB (3 × 24 MiB). A second
pass takes ~160 ms, nearly all of it the thread-by-thread `/proc` look
(on a host with ~160 processes of this user). The test `cas_dedup_converges_three_workers_on_one_extent`
checks with `filefrag` that three workers' copies and the blob have the
same physical extents.

`reclaimed_bytes` is the sum of the sizes of the replaced files: an upper
bound. A file whose extents were already shared (for example one a CoW
mount cloned) frees less than its size.

It fails closed on a directory it cannot list, and reports a blob or
temporary it could not remove in `errors`.

Limitations: the process-table look runs once, before the pass; a process
outside the managed lifecycle that starts working in the root during the
pass is not seen, which is why such users must be stopped (managed workers
are kept out by the admission lock). POSIX `fcntl` record locks are not
seen. For roots that stay in use, an in-place, kernel-verified range
dedup (`FIDEDUPERANGE`) would be the alternative to replacement; it is
not implemented.
