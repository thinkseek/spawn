# LayerFS v2, Track A: overlayfs workspaces (MVP)

Status: opt-in MVP (task #508, 2026-11-23; in-use and prune fixes from
reviews #512, 516 and 539, tasks 516, 539 and 546; minimized in task 556).
`GitWorkspace` is still the default store. `OverlayWorkspace`
(`crates/spawn-host/src/overlay.rs`) implements the same
`spawn_core::port::WorkspaceStore` port over a root of its own and is not
yet wired into `spawn-cli`. It is compiled on Linux only
(`#[cfg(target_os = "linux")]` on the module, its re-export and its
integration test): pidfds and overlayfs exist nowhere else, and
`spawn-host` builds on macOS without it.

**Teardown asks the kernel, not `/proc` (owner decision 2026-11-23, task
556).** The global `/proc` scan of task 546 refused every `unmount()` on a
fleet host, because unrelated non-dumpable processes (`spawn loop`, `serve`,
`integrate`) and a non-initial pid namespace made it unprovable. It is
gone. A per-workspace admission lease keeps launched clients attached, and
after the holder stops the overlay's own workdir exclusion decides. See
"The in-use proof" below.

## Why

With `GitWorkspace`, every agent gets a full working copy of its branch, so
N agents cost N copies of the tree on disk and N exports at mount time.
Most agents change a few files. Track A gives every agent a copy-on-write
view over one shared, read-only snapshot. Each agent then pays only for
the files it changes, and a checkpoint reads only those files.

## Architecture

```text
             bare repo (spawn.git)   <- durable: refs, commits (never in an overlay)
                   |  checkout-index once per commit
                   v
<root>/.layers/<commit>/             Layer 0: shared, read-only snapshot
[+ extra read-only lowers]           pre-warmed target/, node_modules/, deps
                   |
     +-------------+-------------+-------------+
     v             v             v             v
 .overlay/61/upper .overlay/62/upper  ...              Layer 1: one upperdir per agent (branch name in hex)
     |             |
 <root>/a        <root>/b         merged mount points (the workspace ids)

<transcripts dir>/*.jsonl           <- durable: outside the store root, never in an overlay
```

- **Layer 0: shared immutable lowers.** `mount()` materializes the branch
  head once per commit with `git read-tree` + `checkout-index` into a
  staged directory, then renames it into `.layers/<commit>`. A layer that
  exists is therefore complete. Every agent forked from that commit mounts
  the same layer, so no file is duplicated per agent. Extra layers
  (`with_lower_layers`) are stacked below it, for example a pre-warmed
  `target/` or `node_modules/` that every agent reads and none copies.
  `prune_layers()` removes layers that no workspace references. It holds
  the store lock exclusively (every `mount()` holds it shared). It reads
  every workspace's `lower` first, and a state it cannot read (`.overlay`,
  a state directory, a `lower` it may not open) refuses the whole prune
  with `StateUnreadable` before anything is removed. It renames a layer
  aside (`.layers/.trash-<commit>-…`) before deleting it, so a layer half
  deleted is never found under its commit's name, and also clears what an
  interrupted build or prune left there. A `*.jsonl` in a layer is deleted
  only when it is byte for byte the blob (or symlink) the commit tracks at
  that path; any other one refuses with `TranscriptInLayer` and keeps that
  layer whole, set aside as `.layers/.kept-<commit>-…` (never pruned, for a
  person to look at), so the next mount builds the commit's own layer
  again rather than serve one that is no longer the commit (the other
  layers are still pruned).
- **Layer 1: per-agent upperdir.** `mount()` starts a *holder*:
  `unshare --user --map-root-user --mount sh -c 'mount -t overlay overlay -o
  lowerdir=…,upperdir=…,workdir=…,userxattr,redirect_dir=nofollow,metacopy=off,index=off <merged>; exec sleep infinity'`.
  The holder's pid, mount-namespace inode and start time (clock ticks
  since boot, field 22 of `/proc/<pid>/stat`) are recorded in
  `.overlay/<hex branch>/holder`. A pid is reused and a freed namespace
  inode is handed to the next namespace made, so a live holder is one
  whose pid runs, started at the recorded time, in the recorded namespace.
  A record with another number of fields (an older format) names no live
  holder: it is neither trusted nor killed. The state directory is the
  branch name in hex, so the state of `a` never holds the state of `a/b`.
  A new state is built beside it and renamed into place, so a state that
  exists always has its `lower`.
  - Processes enter the workspace with `OverlayWorkspace::command(ws,
    program)`, or by prefixing a terminal launch (tmux, when wired) with
    the whole `launch_arguments(ws)`:
    `flock --shared --nonblock <root>/.locks/<hex>.lease sh -c <enter>
    <state>/holder <merged> <program>`. `flock` takes the lease first and
    stays as the program's supervisor, holding it until the program exits,
    whatever descriptors the program closes. Only then does `<enter>` read
    the holder record, open (pin) `/proc/<pid>/ns/user` and `ns/mnt`, and
    check, after opening them, that the pid still runs with its recorded
    start time and that the pinned mount namespace is the recorded one;
    `nsenter --user=/proc/self/fd/3 --mount=/proc/self/fd/4
    --preserve-credentials` then enters the pinned namespaces, never a pid,
    and a shell inside changes into `<merged>`, closes the pins and runs
    the program. A launch while a teardown holds the lease exits 75; one
    whose holder is gone or reused exits 76. The store sets nothing in the
    client's environment (no `CARGO_INCREMENTAL`): build settings are the
    caller's.
  - The host reads and writes the merged view through
    `host_path(ws)` = `/proc/<holder>/root/<merged>`. It takes no lease: a
    host process whose working directory, open file or mapping is there
    holds the overlay, and `unmount()` then retires the holder under it and
    refuses with `InUse`, keeping every byte (`mount()` refuses too until
    it lets go).
- **Upperdirs outlive holders.** Dropping the store, or restarting the host,
  stops the holders. The store's drop honors admission: it stops a holder
  only once it has taken that workspace's lease exclusively, and leaves a
  holder whose launched client still holds the lease running (the next
  store finds it as the live holder and mounts nothing). The upperdir
  stays on disk, and the next `mount()` mounts it again over the same
  lower commit (recorded in `lower`), so no edit is lost. A live holder
  whose namespace no longer shows the overlay (a client lazily unmounted
  it) is retired by the next `mount()` as teardown retires one: once the
  lease is free, then through the kernel's gate.

## `commit()` in O(|diff|)

`commit()` never reads the merged view or the lower layers. It walks only
the upperdir:

| upperdir entry                                        | effect on the tree            |
|-------------------------------------------------------|-------------------------------|
| regular file                                          | blob, mode 100644 / 100755    |
| symlink                                               | blob of the target, 120000    |
| char device 0:0 (whiteout)                            | path removed (file or subtree)|
| empty regular file carrying `user.overlay.whiteout` (xattr whiteout, as the kernel's lookup reads it; the directory's `opaque=x` only filters readdir) | path removed |
| dir with `user.overlay.opaque=y`                      | lower subtree removed, then the dir's own entries added |

Only `user.overlay.*` is read: the store mounts with `userxattr`, so the
kernel never reads `trusted.overlay.*` in its layers.
| directory named like a derived dir (`target`, `node_modules`, `.spawn`, configurable) that the lower commit does not track | skipped at any depth |
| socket / fifo / other device                          | skipped                       |

Steps:
1. `read-tree <lower>` into a temporary index.
2. `ls-files` lists the removed prefixes, in bounded batches of pathspecs
   (`git.rs`'s `pathspec_batches`, never one unbounded argv), and those
   entries are removed.
3. Blobs are written with one `hash-object --stdin-paths`.
4. One `update-index --add --replace -z --index-info` applies every change.
5. `write-tree` writes the tree.

Only a directory is derived: a file named `target`, or a directory of that
name the lower commit tracks, is committed like any other path.

The commit's parent is the branch head this workspace last published. The
ref moves by compare-and-swap (`update-ref <ref> <new> <old>`), with three
outcomes:
- **Branch moved by another writer:** refused with `WorkspaceStale`. The
  upperdir is kept. That holds for a branch moved before the commit read
  it and for one moved between that read and the ref update (the
  `update-ref` compare fails, and is reported as `WorkspaceStale`).
- **Branch deleted:** recreated (`Commit::Recreated`).
- **Same tree as the head:** `Commit::Unchanged`.

Before the ref moves, the old and new commit are written to `pending`. A
publication whose `head` was not written afterwards (a crash, a full disk)
is settled on the next `commit()` or `mount()`: the ref at the new commit
means published (`head` is brought up to it), anything else that it was
not. A retry is then never refused as stale against its own commit. Every
ref lookup is fallible: only git's own "no such commit" (`rev-parse
--verify --quiet`, exit 1, nothing printed) means absent; any other
failure (the repository unreadable, git not run) is an error, and
`pending` is kept for the next try.

Git runs through `GitWorkspace`'s command (C locale, the store's
identity) and its output helpers; branch creation and `from` resolution
are `GitWorkspace::ensure_branch` itself.
The mount options keep the upperdir easy to read: no redirects
(`redirect_dir=nofollow`), no metadata-only copy-ups (`metacopy=off`) and
no index (`index=off`).

`mount()`, `commit()` and `unmount()` of one workspace hold its lock
(`flock` on `<root>/.locks/<hex of the branch>`; the store lock is
`<root>/.locks/store`; lock files are empty and stay) for the whole call, so
two stores over one root never check, then mount, at once. This is the
same `flock` idiom as `GitWorkspace`'s branch lock.

## `unmount()` GC

All under the workspace lock. Every refusal below changes nothing: no file
is deleted and no permission is touched.

1. **Preflight.** Count transcripts (names ending in `.jsonl` by their
   bytes, `.jsonl` itself included, that are not directories or whiteouts
   of either form) in the upperdir, never following a symlink: an upper
   root that is a symlink, or no directory, refuses. Refuse with
   `TranscriptInUpper` if there are any. The count fails closed: a
   directory it cannot list (mode 000, say) or an entry it cannot stat
   refuses with `UpperUnreadable`. It never chmods anything to look. The
   workspace stays mounted.
2. **Close admission.** Take the workspace's lease exclusively, without
   waiting. A client launched through `command()`/`launch_arguments()`
   still holding it refuses `InUse`, still mounted. Held from here to the
   end, so no new client enters meanwhile.
3. **Retire the holder.** Only the verified one: a holder this store
   started is killed through its `Child`; a recovered one (another store's)
   through a pidfd opened first and checked against the record (pid, start
   time, namespace) afterwards, so a pid reused in between is never
   signalled.
4. **Ask the kernel** about the original workdir (below). Busy refuses
   `InUse`, no answer refuses `InUseUnproven` (this workspace only); either
   way `upper`, `work`, the state and its layers are kept, and the next
   `mount()` refuses too while the old overlay lives. Nothing that held
   the overlay loses a byte: only the host-path endpoint is gone.
5. **Free:** nothing can write the upperdir any more. Count transcripts
   again, then rename `.overlay/<hex branch>` to a trash name in one step,
   so the workspace is gone at once (O(1)), then delete it. Only the
   kernel's own `work/work` and `work/index` (mode 000) get their
   permissions changed, and only when they and `work` are real
   directories, never symlinks; anything else unreadable makes the delete
   fail and stay in the trash. As a second guard, the delete itself
   refuses to unlink any transcript.

`mount()` over an existing upperdir whose holder is gone (recovery) asks
the kernel the same question first: `InUse` while the old overlay lives,
`InUseUnproven` when it cannot tell. It never mounts a second overlay over
the same `upper`/`work`. A state that exists without its `lower` (holding
work, or a holder record) is refused with `StateUnreadable`, never taken
for a fresh start; a `lower` that cannot be read is an error. Only a
state that does not exist is created for the branch head.

## The in-use proof

**The kernel's workdir exclusion.** Overlayfs marks an overlay's upperdir
and workdir in use for the whole life of its superblock, and with
`index=on` a second overlay over a busy workdir is refused with `EBUSY`.
The store mounts a throwaway overlay (`lowerdir` and `upperdir` empty,
under `.overlay/<hex>/.probe-…`, `workdir` the original `work/`, never
renamed or replaced, since the kernel marks that inode,
`userxattr,index=on`) in a namespace that ends with the probe. Only a
mount that succeeds says "free". `mount` runs in the C locale. Exit 32 is
busy only when the first line is exactly `mount: <probe target>: <message>`
and the message, with the probe's own path cut off, is util-linux's
wording of `EBUSY` ("overlay already mounted on …", "… or mount point
busy", "target is busy", "Device or resource busy"): no text of any path
can match. Any other failure (the probe cannot run, the kernel refuses
`index=on`, a permission denied, an unrecognized message) is unknown.
Busy and unknown answers are retried for 2 s, since the last mount of a
stopped holder goes as its namespace does.

The superblock lives while anything at all holds it, so this sees,
without reading any process: a mount in any namespace (an inherited or
cloned one too), a working directory, root, open file or mapping, a file
descriptor in flight in a unix socket (`SCM_RIGHTS`), a process of another
uid, a process that is not dumpable, a process in any pid namespace. It
never sees an unrelated process: one that does not hold this overlay
cannot keep its workdir busy, whatever `/proc` would let us read of it.

`overlay_supported()` requires, on the same configuration, the busy
answer (not merely a failure) while an overlay is mounted over the
workdir, and the free answer once it is gone. Where it does not get both,
the store counts as unsupported. Each store asks it once on its own root
(under `.overlay`) before the first `mount()`, recovery or teardown trusts
the probe there: on a root that fails it, `mount()` is unavailable and
`unmount()` refuses `InUseUnproven`, with nothing changed.

**The admission lease.** Holder pid, start time and namespace identify the
holder only. Clients the store launches hold the lease (through their
`flock` supervisor) for their whole life, so a teardown, or the store's
drop, refuses while one lives instead of stopping the holder under it.
What a lease cannot cover (a host process in `host_path()`, a descriptor
handed elsewhere, a detached child that outlives the launched program) is
caught by the kernel after the stop, with the data kept. A
caller that promises uninterrupted external tooling must give that tooling
a lease too. Raw backing directories (`.overlay/…/upper`) are the store's:
a process writing there directly holds no overlay reference at all.

Git commits are in the bare repository and transcripts are outside the
root, so neither is touched.

## Verified kernel findings (Linux 7.2, btrfs root, this VM)

- With `userxattr,index=on`, a second overlay whose workdir is that of a
  live overlay is refused with `EBUSY`, in a user namespace too, while a
  non-dumpable process keeps the first alive through its working directory
  (the holder's namespace gone). Once that process exits, the probe mount
  succeeds. The probe wrote nothing into the workdir but `work/work`.

- `unshare -r -m` followed by `mount -t overlay` works without root.
- `userxattr` stores markers as `user.overlay.*`, which the host reads with
  `getxattr`. Opaque directories carry `user.overlay.opaque=y`.
- Whiteouts appear on the host as `c 0,0` owned by the caller's uid.
- **`redirect_dir=off` is refused inside a user namespace**
  (`wrong fs type, bad option`). Use `redirect_dir=nofollow`.
  `metacopy=off` and `index=off` are accepted.
- `nsenter` needs `--preserve-credentials`, because `unshare -r` writes
  `deny` to `setgroups` and plain `nsenter --user` fails on `setgroups`.
- **`nsenter --wd=DIR` opens DIR before entering the mount namespace**, so
  the process lands in the empty host mount point. Change directory after
  entering instead (`env --chdir`).
- `/proc/<holder>/root/<path>` lets a host process of the same uid read and
  write inside the holder's mount namespace.
- **userns visibility boundary.** The mount exists only inside the holder's
  namespace. Host tools (an IDE, `git status` run directly) see an empty
  directory at `<root>/<branch>`. They must use `host_path()` or
  `nsenter`. Track B (a `/dev/fuse` CAS filesystem) would not have this
  boundary, because a FUSE mount is visible host-wide, but every read would
  pay a userspace round trip.
- `btrfs` `FICLONERANGE` (`cp --reflink=always`) works without root as well.
  It is the fallback for hosts without unprivileged overlayfs: reflinked
  full copies share extents, but the tree walk stays O(|tree|). For build
  output on btrfs, `chattr +C` (nodatacow) on `target/` avoids copy-on-write
  fragmentation for files that are rewritten in place. Verified without
  root: `chattr +C` on a new directory is inherited by new files, and a
  reflink between two NOCOW files still works. nodatacow data has no
  checksums and no compression, so use it only for derived dirs.
- Incremental compilation writes thousands of small, short-lived files,
  and in an upperdir every one of them becomes a copy-up. A caller may
  set `CARGO_INCREMENTAL=0` for the clients it launches; the store's
  launch API sets nothing.
- The workdir probe runs in a nested user and pid namespace too
  (`unshare --user --map-current-user --pid --fork --mount-proc`, then the
  holder's own `unshare -r -m`): mount, recovery and teardown work there
  unchanged.
- In an upperdir, an empty file carrying `user.overlay.whiteout` is a
  whiteout to the kernel's lookup with or without its directory's
  `user.overlay.opaque=x`: the merged view no longer finds the lower file
  (readdir still lists the name without the `x`).
- `nsenter --user=/proc/self/fd/3 --mount=/proc/self/fd/4
  --preserve-credentials` enters namespaces pinned as open descriptors.

## Benchmark

`cargo test -p spawn-host --test overlay -- --ignored --nocapture`
(`SPAWN_OVERLAY_BENCH_FILES` sets the tree size). Measured on this VM with
4001 files of 4 KiB, one edited file per agent:

| setup                            | bytes on disk                          | wall time |
|----------------------------------|----------------------------------------|-----------|
| 5 full working copies            | 81,920,040                             | 1.89 s    |
| 5 overlays over 1 shared layer   | 16,384,046 (16,384,006 layer + 40 upper)| 1.16 s   |
| 5 overlay commits                | n/a                                    | 0.77 s    |
| 5 overlay unmounts               | n/a                                    | 1.82 s    |

Overlay disk use is one tree plus the diffs, instead of N trees. The mount
time includes building the one shared layer. Unmount time was measured with
a one-file upperdir: most of it is outside the store's own work (stopping
the holder and tearing down its namespace). Profiling it is left open.

## Not in the MVP

- **Moving base.** `merge_main_clean` / `merge_base` answer `Current`. An
  upperdir is a diff against a fixed lower commit, so main reaching a
  mounted overlay needs a rebase of the upperdir onto a new layer.
- **`set_aside`.** Uses the port's default.
- **Large-file cap and notices.** None yet.
- **Wiring into `spawn-cli` and the tmux launch.** This needs the terminal
  host to prefix launches with the whole `launch_arguments` (which takes
  the lease first), and a store root of its own.
- **Track B: FUSE CAS.** Out of scope.

## Tests

`crates/spawn-host/tests/overlay.rs` is compiled on Linux only, and skips,
with a reason, only on hosts that cannot mount an unprivileged overlay
with the kernel's workdir exclusion. Everywhere else every step runs: an
unexpected refusal, `InUseUnproven` included, fails the test. It covers:
- the probe;
- five concurrent agents over one layer: 0 copy-ups from reads, isolated
  conflicting edits, whiteouts, an opaque directory, symlink and mode
  changes, derived dirs left out, the exact git tree per agent, nothing
  set in a client's environment, and 100% of upperdir bytes reclaimed
  while commits and an external `.transcripts/` file stay;
- unmount refused on a `*.jsonl` in the upperdir, and on a `chmod 000`
  directory hiding one (refused, mode and file kept);
- **an unrelated non-dumpable host process never blocks** an unmount, a
  recovery mount after a dropped store, or a fresh lifecycle;
- **the whole lifecycle inside a nested pid namespace** (the test runs
  itself again under `unshare --pid`): mount, commit, recovery, a refused
  and a clean unmount;
- unmount refused (`InUse`, by the lease) while a launched client is
  inside the namespace, or in a nested namespace that inherited the mount
  or lazily unmounted it, or is a thread group whose leader exited, or
  has closed every descriptor it inherited (the supervisor holds the
  lease);
- a launched client keeps its holder across a dropped store, and the next
  store mounts nothing over it; a holder whose overlay a client lazily
  unmounted is retired by the next mount only once the lease is free;
- unmount and mount refused (`InUse`, by the kernel) by a process holding
  the overlay by reference: a host process inside `host_path()`, dumpable
  or not (by `prctl`, and by an execute-only binary), a file held open or
  mapped through it, a file from a pre-warmed layer on another
  filesystem, a process that lazily unmounted it after a dropped store,
  and a descriptor in flight in a unix socket held by no process; each
  time the late write lands and the workspace mounts again;
- a holder record in an older format is neither trusted nor killed;
- an unreadable `lower` is never taken for a fresh start;
- two stores mounting one branch at once make one overlay;
- a recorded holder pid reused by a stranger is neither trusted nor
  killed;
- `prune_layers` never leaves a half-deleted layer, sets aside a layer
  with a `*.jsonl` the commit does not hold, and refuses on a state it
  cannot read;
- nested branch names (`a`, `a/b`) never share state;
- derived names skip only untracked directories; a whiteout named
  `*.jsonl` is no transcript; a directory replaced by a file and back;
- unit tests (`overlay::tests`): **a live non-dumpable client outliving
  the holder refuses `InUse`** (and the next mount too) and the teardown
  completes once it is gone; a mode-000 directory hiding a `*.jsonl`
  refuses the teardown; a launch is refused while a teardown holds the
  lease, and when the recorded holder is gone; a publication whose `head`
  was not written is settled from `pending`, and kept through a failed
  ref lookup; a state with work and no `lower` is never a fresh start;
  xattr whiteouts by the file marker alone, checked against the merged
  view, and never taken for transcripts; opaque markers; symlinks read,
  never followed; the C-locale mount answers classified with a probe
  path holding `busy` (only `EBUSY`'s wording counts); a store root that
  failed the probe never trusts it; a transcript named `.jsonl`, a
  symlinked upper or `work` root, and a user's mode-000 directory in
  `work/` (never opened up); the holder record trusted only whole and by
  its start time;
- remount after the holder is gone; stale and recreated branches; shared
  pre-warmed lower layers.
