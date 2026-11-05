# Storage: one place, by kind of object

The goal: everything Spawn knows lives in one store that any machine with the
right credentials can reach, and work in it branches and merges (a branch per
agent, a checkpoint per tool call, fork from any commit, merge back
automatically), durably: a crash or a power loss loses no committed state,
and a rejected branch can be deleted.

Git is not the store either. It versions text; it stores every version of a
file whole. Measured on this host (git 2.56, btrfs):

| Case | Cost per checkpoint |
| --- | --- |
| This repository's tree (5.8k files, ignored build output), index rebuilt for every checkpoint | 7–25 ms |
| Same, index kept between checkpoints (what the Bridge does) | 2–3 ms |
| A 200 MiB binary committed to git, with 1 MiB rewritten between checkpoints | **3.1 s** on the Bridge's loop, **+211 MiB** in the repository each time; repacking three versions still takes 622 MiB |

So each kind of object goes where its shape fits.

## Kinds of object

| Kind | Examples | Shape | Branch / merge | Store | Built |
| --- | --- | --- | --- | --- | --- |
| Facts | events, commands, missions, tasks, gate verdicts, landings | small rows, append-only | no branches: one truth, folded | PostgreSQL (`spawn_events` and folds) | yes |
| Source text | code, docs, configs, scenarios, backlog | small text files | branch per agent; three-way merge with the source's `.gitattributes` rules (read from `main`) | git: the mission's source repository | yes |
| Workspace snapshots | the whole tree an agent works in, at every tool call | many files, few change per checkpoint | a branch is a git commit (text) plus a large-file manifest it names; fork is O(1) | the agent's git branch + the chunk store | yes |
| Large and binary files | weights, datasets, images, archives, anything over 8 MiB | MB–GB, opaque, small edits | not merged: a branch keeps its own | content-defined chunks (FastCDC), zstd, by hash, in the chunk store | yes, host-local |
| Streams | transcripts, engine output, gate logs | append-only, read in order | none | compressed segments in the chunk store, indexed by execution in PostgreSQL | no: transcripts are copied to the transcript archive beside the workspaces (below), gate logs are in `task.gated` records |
| Derived caches | `target/`, `node_modules/`, the kache store | rebuildable, large, hot | none; never authoritative | local to each host (kache may share a remote) | yes |
| Secrets | `spawn.env`, test URLs, keys | tiny | none | never stored centrally | yes |

The chunk store is content-addressed: an object is named by the hash of its
bytes, written once, never changed (a corrupt copy is replaced, which is a
repair, not a change). Identical chunks across files, versions,
branches and agents are stored once, so a 1 MiB edit of a 200 MiB file costs
about 1 MiB, not 200. Its adapter is a local directory; an S3-compatible
adapter (R2) is not built.

## Branching and merging

A workspace snapshot is a pair, carried by one git commit: the commit's tree
holds the text (every file git would commit, up to 8 MiB), and a trailer in
its message, `Spawn-Large: <manifest hash>`, names the manifest of the large
files (path, mode, size, chunk list) in the chunk store. The branch head alone
is the whole workspace, so everything below is a git operation plus, for large
files, a chunk fetch.

- **Checkpoint** (every tool call, turn end, exit): the Bridge commits the
  text against the kept index (a stat cache: only changed files are hashed)
  and chunks only the large files whose size or mtime changed. A checkpoint
  where only a large file changed is a commit with the same tree and a new
  trailer. Cost is proportional to what changed. A large file git already
  holds as a blob in the branch head, in `main` or in a pending merge's
  `main` (main may get one from outside Spawn, and a merge brings it in)
  stays a blob: leaving it out of the tree would delete it from the branch,
  and from `main` when the branch lands.
- **Branch**: `refs/heads/<agent>`, moved by compare-and-swap on the old
  commit.
- **Fork from any commit**: a new branch at an existing commit. No data is
  copied; the large files' chunks are shared; sub-agents fork recursively.
  `start {from}` names another agent of the same mission source (its branch
  head) or a full commit id in the mirror; anything else fails the start. An
  agent whose branch exists keeps it (the tick report warns that `from` was
  ignored). The start that creates a branch records where it began on
  `execution.started` (`fork: {from, commit}`; `from` is `main`, the start's
  `from`, or `spawn/<agent>`).
- **Resume anywhere**: a host that starts an agent whose branch its mirror
  lacks takes `spawn/<agent>` from the source when it is there; a local
  branch another host moved on since is fast-forwarded (diverged: warned, the
  local one stays). The chunk store is host-local, so a branch with large
  files fails to mount on a host without its chunks (see "Not built").
- **Central**: every branch that moved (a checkpoint commit, a merge, a new
  branch) is pushed to the mission's source as `spawn/<agent>` without force
  (two writers of one branch are caught: the push is refused and warned, not
  overwritten), from a background pusher off the Bridge's loop: at most one
  push per branch every 5 s, carrying the head as it is then, failures
  retried with backoff, and what still waits pushed when the store drops (a
  one-shot `spawn tick`). Only a source that is a directory on this host (the
  central store) shares branches by default, both ways (pushes and adopting
  `spawn/<agent>`): a remote may be public, and whoever can push `spawn/*`
  there would choose an agent's starting tree. A remote URL shares branches
  only when `SPAWN_PUSH_SOURCES` (comma-separated) names it exactly. Chunks
  are not shared between hosts (see "Not built").
- **Main at a start or resume**: main reaches an agent's branch only when an
  execution starts or resumes, before its workspace is mounted, and never
  while an execution runs: nothing changes files under a running agent.
  When the source's `main` moved (the mirror's `main` is fetched at most
  every 30 s for this), it is merged into the branch if, and only if, the
  merge is clean (`merge-tree --attr-source=main`, so the rules come from
  `main`, which only the gate moves; the integrator lands with the same
  rules). The branch then moves by compare-and-swap to a merge commit
  (parents: the head, `main`), recorded as `artifact.committed` with reason
  `base_merged` and the files it changed, and the mount exports it. Any
  conflict leaves the branch as it is: the execution starts on it, and the
  conflict is recorded as `merge.conflicted {files, marked: false}` (keyed by
  the branch head and the files, so a retried start records it once) for the
  supervisor and the integrator; the integrator's landing reports it as
  before. Large files are not merged: the branch keeps its own (the merge
  commit carries the head's `Spawn-Large` trailer), and a `main` that puts a
  file or a symlink where a large file or its directory is counts as a
  conflict. `notice.typed`, `notice.received` and `merge.conflicted` records
  with `marked: true`, written while main was merged into running workspaces,
  still decode and change nothing.
- **A merge that loses the branch race**: when another writer moved the
  branch between the merge's directory update and its compare-and-swap,
  the directory follows the head that won: its text by the same two-tree
  update (merge tree to the winner's tree), and its large files to the
  winner's manifest (each file the two manifests differ in is written from
  the chunk store, or removed, with its stat-cache entry), so the next
  checkpoint neither commits the lost merge as edits nor drops the other
  writer's large files. When either is refused (a file the two sides differ
  in changed in the workspace meanwhile, a large one included, or the
  winner's tree has no room for its large files) the workspace is marked
  **stale** (`<root>/.stale/<agent>`). It keeps its work: each checkpoint of
  a stale workspace commits its tree (large files and all) to
  `refs/spawn-stale/<agent>` (first parent the branch head, second the
  snapshot before it), never to the branch, and answers `WorkspaceStale`;
  merges of `main` are refused. A mount or an unmount commits that snapshot
  before it replaces or removes the directory, and fails, changing nothing,
  when it cannot. The Bridge ends an execution whose workspace answers
  `WorkspaceStale` (its state is kept, so the unmount loses nothing) instead
  of retrying; the next start mounts the branch head afresh. What the
  snapshot holds is not merged back: a person or the supervisor takes what
  they need from `refs/spawn-stale/<agent>` (it is never pushed or landed;
  chunk collection keeps what it names, like every ref).
- **Ending an execution**: final checkpoint, terminal killed, workspace
  unmounted, then `execution.ended`. The workspace holds the only copy of
  what the execution did not commit, so it is unmounted only after the final
  checkpoint commits, or on a `discard` command. A final checkpoint that
  fails keeps the execution and its workspace and is tried again each tick;
  after 20 failures (`END_ATTEMPTS`) the end is stalled: the terminal is
  killed, `execution.end_stalled {cause, exit_code, failures, error}` is
  recorded and warned once, the Observer marks the session BLOCKED with
  attention `end_stalled` (the fleet view and the supervisor's digest show
  it), and the checkpoint is retried every 30 s (`END_RETRY_MS`) by the
  Bridge's clock, also after a restart, until it commits. A plain `stop`
  does not change that. `discard` (`{"type":"discard"}`, a person's or the
  supervisor's decision) tries the checkpoint once more and unmounts even if
  it fails, losing what was not committed.
- **Landing**: text only, from what the agent published. The candidate is
  the source's `refs/heads/spawn/<agent>`, the branch the pusher publishes,
  fetched into the integrator's mirror as `refs/spawn-integrate/<agent>`,
  never into `refs/heads/<agent>` (on the Bridge's host that branch is the
  Bridge's, and the integrator never moves it). The integrator never commits
  a mounted workspace, so it works the same on a host that never ran the
  agent, and a file written after the last checkpoint, never pushed, does
  not land. **Stale pushes do not land**: the pushed head must be the commit
  of the agent's latest `artifact.committed` event (its last checkpoint or
  merge of `main`) or a descendant of it. A push that lags (behind it,
  diverged from it, or no `spawn/<agent>` at all while the agent has a
  checkpoint) is fetched again for up to 12 s (the pusher pushes a branch at
  most every 5 s), then gated `stale`: the gate does not run, the task goes
  back to `reported` (`accepted` under review), no rejection is counted,
  nothing reaches the worker, and a later pass judges again once the pusher
  has caught up. An agent with neither a checkpoint nor a pushed branch is
  rejected ("the worker has no branch to land"). So a remote source must
  share branches (`SPAWN_PUSH_SOURCES`) for its agents' work to land. The
  rest is unchanged: a branch behind a moved `main` lands as the merge by
  `main`'s merge rules, marker blocks in the files the branch changes are
  gated `conflict`, a pending merge of `main` (it lives with the workspace
  directory, on the Bridge's host) blocks judging when that directory is on
  the integrator's host, and its markers, once checkpointed and pushed, are
  caught by the marker check wherever the integrator runs. The gate judges
  the candidate; the integrator moves `main` by a compare-and-swap push.
  Large files are workspace state, not source, and never land (one main
  already holds as a blob is source: it stays a blob and lands as one).

## The chunk store

- **Checkpoint index.** The checkpoint index is kept between checkpoints (a
  stat cache), and files over 8 MiB stay out of git commits.
- **Port and adapter.** A chunk-store port (`spawn_core::port::ChunkStore`)
  with a local directory adapter (`spawn_host::chunks`): gear-hash FastCDC
  chunks of 128 KiB–2 MiB, about 512 KiB on average, named by BLAKE3,
  zstd-compressed, written by temporary file and link so a sound object is
  never rewritten, verified on read. A `put` of content whose object exists
  verifies that object first (once per process): a corrupt one (a torn
  write, a disk error, a copy that died) is replaced by a rename over it,
  which repairs every manifest that names it. The object file and its
  fan-out directory are fsynced, and the store root when a fan-out
  directory is created. A `put` of content the store already holds makes
  that object young again (its modification time is set to now). Objects
  are removed only by a collection (below).
- **Manifest.** The large-file manifest is a chunk-store object, and the
  checkpoint commit names it in a `Spawn-Large` trailer. A path that is not
  UTF-8 is written as `path_hex`, so any name round-trips. Only large files
  whose size, mtime, ctime, inode or mode changed are chunked again (a stat
  cache in `<root>/.large/<branch>`); an entry whose file changed within
  3 s of its stat ("racily clean", as git says) is hashed again before its
  chunks are trusted, so a same-size rewrite in the same timestamp tick, or
  one whose mtime was put back, is never committed with the old chunks. A
  checkpoint that takes chunk names from that
  cache touches each one (makes it young again) and chunks the file again
  when the store no longer holds one, so it never names a collected chunk.
- **Mount.** Mount materializes the large files again, each atomically, and
  fails rather than write bytes that do not verify, or write outside the
  workspace. The whole new tree, large files included, is built in
  `<root>/.mount/` first; only when every chunk was fetched and verified is
  the workspace renamed aside and the new tree renamed into its place, so
  a mount that fails leaves the workspace, uncommitted files and all, as it
  was. A manifest whose large file the head's tree has no room for (a
  tracked entry at its path, a file or a symlink at a parent directory)
  fails the mount. Parent directories are walked one component at a time,
  each opened with `O_NOFOLLOW` through the open descriptor of the one above
  (`openat`, made with `mkdirat` when missing; no `/proc`, the same on Linux
  and macOS), so a symlink or a file where the manifest has a directory
  fails the write, and one swapped in after the walk is never followed; the
  temporary file is created exclusively without following a symlink and
  renamed (`renameat`) inside the directory that was opened.
- **Where.** The store is the directory `SPAWN_CHUNK_STORE` names, else
  `<root>/.chunks`. It is host-local: a branch with large files resumed on a
  host without its chunks fails to mount rather than lose them.
- **Collection.** `spawn chunks gc [--grace <minutes>] [--root <dir>]
  [--repo <path>]` removes objects no reachable commit names.
  - *Roots*: the store is `SPAWN_CHUNK_STORE`, else `<root>/.chunks`, with
    the root from `--root`, else `SPAWN_WORKSPACE_ROOT`. The repositories
    are the default repository (`--repo`, else `SPAWN_GIT_REPO`) and every
    bare repository directly under `<root>/.sources/` (the mission mirrors).
  - *Live*: every commit reachable from any ref of those repositories, not
    only branch heads (an older commit of a branch still restores its
    files). Each `Spawn-Large` trailer names a manifest; the manifest and
    every chunk it lists are live.
  - *Another host's manifests*: a manifest missing from this store that
    only refs outside `refs/heads/` name (`refs/spawn-integrate/*`,
    `refs/remotes/*`: another host's pushed checkpoints) lives in that
    host's store; it is skipped with a warning naming it, so collection
    still runs on a host that shares branches. One a local branch reaches
    must be here, and stops the collection.
  - *Grace*: an object (a file named by 64 hex digits under
    `<store>/<2 hex>/`) goes only when it is not live and older than the
    grace period (default 60 minutes, at least 10: a shorter one is
    refused), its time read again just before it is removed. Chunks are
    written before the commit that names them, and a `put` or a stat-cache
    reuse makes an object young again, so a checkpoint in flight keeps what
    it reuses as long as it commits within the grace period. The age check
    and the unlink happen under the store's lock (a `flock` on the store
    directory) held exclusively, which every `put` and `touch` holds shared: a refresh
    between the listing and the removal keeps the object, and a writer that
    waited finds it gone and writes it again.
  - *An empty default repository is refused*: with `--repo` (or
    `SPAWN_GIT_REPO`) naming a repository without any ref, nothing would be
    live, so nothing is deleted and the exit is non-zero.
  - *Read errors delete nothing*: everything is decided before anything is
    deleted. A repository that cannot be read (named by its path), or a
    manifest a reachable commit names that is missing, unreadable or does
    not verify (named by its 64-hex name; never treated as unreferenced),
    stops the collection with a non-zero exit.
  - It prints one line, `chunks gc: kept N objects, removed M objects, freed
    B bytes`; a second collection with nothing new removes nothing.
    Listing (each object with its age and size), touching and the
    age-checked removal are part of the chunk-store port, so a bucket
    adapter is collected the same way.
- **Landing.** A branch head that names a manifest lands as a commit of the
  same tree without the trailer, so large files never reach `main`.

## The transcript archive

Claude Code's transcript of a native session (one JSONL file under its
projects directory) is the agent's history: `--resume` reads it, and the
Bridge derives the session's events from it. It is not Spawn's to keep.
Claude Code deletes old transcripts (its `cleanupPeriodDays` sweep runs at
every engine start, over the projects directory the owner's own sessions
share), a container's copy goes with the container at unmount, and the
format is the engine's. So Spawn keeps its own copy
(`spawn_core::transcripts`, the `TranscriptArchive` port, a local directory
adapter in `spawn_host::transcripts`):

- **Where.** `<root>/.transcripts/<agent>/<native session>.jsonl`, beside
  the workspaces and the chunk store, on the host that ran the engine.
- **When.** After the checkpoint of each turn end, and at the end of each
  execution, after its terminal is killed and before its workspace is
  unmounted: the Bridge reads (through the terminal host, so inside a
  container too) what the file gained since the latest copy and appends it,
  each append made durable (`fdatasync`, and the directory for a new file).
  A failure is a tick warning; the next copy takes what was missed.
- **Never overwritten.** The file is append-only. The Bridge compares the
  last 4 KiB it copied with the file at the same place: a file that shrank,
  or whose bytes there differ (rewritten, replaced), is copied whole as the
  next copy, `<native session>.<n>.jsonl`, and the earlier copies stay as
  they were. An append to a copy at any length but its own is refused.
- **Restore on resume.** Before a `resume` starts the engine (after the
  mount, so a container exists), a missing transcript is put back from the
  latest copy, created whole or not at all (a temporary file linked into
  place; never over a file that is there), and the tick warns that it was
  restored. A transcript that is gone with no copy, for a session whose
  events were read from one, fails the resume with that reason and
  unmounts: resuming would start the agent blank under its old id. A
  session that never wrote a transcript resumes as before.
- **The log.** No transcript content and no record of the copies: the
  archive is its own index, and the events already derived from the lines
  are in the log.
- **Not built.** The archive is host-local (like the chunk store) and is
  never pruned; moving it to R2 with the chunks is the way off this host.

## Disk room

The host's disks are shared with other projects, so Spawn guards what it
writes (the gate's scratch in `TMPDIR`, workspaces and mirrors under the
workspace root, the chunk store, deploy dumps) so a full disk corrupts no
state and wedges nothing:

- **The gate refuses to start without room.** With less than
  `SPAWN_GATE_MIN_FREE_GB` (default 10) GiB free in `TMPDIR`, `gate.sh`
  exits 3 before it makes any scratch (`scripts/disk.sh`): an abort, which
  the integrator records and retries, never a verdict on the candidate. The
  integrator passes the variable through. The gate's scratch is
  `$TMPDIR/spawn-gate.XXXXXX`, so cleanup can tell it from everyone else's.
- **The loop watches free space.** Once a minute `spawn loop` reads the free
  space of the workspace root and `TMPDIR` (`df`). A path that falls under
  `SPAWN_DISK_WARN_GB` (default 20) GiB is recorded once as a control record
  `host.disk {label, path, state: "low", free_bytes, warn_bytes,
  floor_bytes}`, and once as `state: "recovered"` when it is back 1 GiB above
  the threshold (so a path at the threshold does not flap). The latest
  record per path is read back from the log on start, so a restart records
  nothing twice. Each record of the last day reaches the supervisor's digest
  once ("disk space on this host"); the fleet view's health carries `disk`:
  per path its free space now (read by `spawn serve`), the thresholds and
  its latest record, and the health strip shows it. A failure to read or
  record is a line in the loop's log, never a failed pass.
- **Under the floor no new execution starts.** While any of the paths was
  under `SPAWN_DISK_FLOOR_GB` (default 2) GiB at the latest reading, the
  dispatcher holds its `start` and `resume` orders (decided again every
  pass, so nothing is lost; the loop logs `dispatch: held N new
  execution(s)`) and residents are not revived. Running executions go on,
  typed input, stops and digests still go, and the supervisor is still kept
  alive so someone can act.
- **Routine cleanup**, daily, by `spawn-maintenance.timer`
  (`scripts/maintenance.sh`, [install](../deploy/systemd/README.md#disk-room)):
  `spawn chunks gc --grace 120`; the gate's scratch directories (exactly
  `spawn-gate.` plus six characters, a directory, owned by this user, not
  modified for a day: a gate removes its own on exit and is killed after 45
  minutes, so these are left by a killed gate); and deploy.sh's
  `spawn-YYYYMMDD-HHMMSS.dump` files in the backups directory beyond the
  newest 5. `pg-backup.sh` rotates its own `spawn-auto-*.dump`; every other
  file is left alone. A dry run (`SPAWN_MAINTENANCE_DRY_RUN=1`) prints what
  would go.

Not guarded: the kache compiler cache (`~/.cache/kache`, a derived cache
outside Spawn's cleanup), the mirrors under `<root>/.sources`, which hold
branches, and the transcript archive under `<root>/.transcripts`, which
holds the agents' history; none of them is pruned automatically.

## Not built

- **Large files and chunks in R2**: an S3-compatible chunk-store adapter, so
  that a branch with large files resumes on any host.
- **Streams as segments**: transcripts and gate logs as compressed segments
  in the chunk store, indexed by execution in PostgreSQL.
- **Backlog, scenarios and gate versioned with the source**.
- **PostgreSQL reachable from other hosts** through a Cloudflare Tunnel +
  Access.

## Where state lives

| State | Where | Rebuildable? |
| --- | --- | --- |
| Event log | PostgreSQL, one container on this host, bound to 127.0.0.1 | It *is* the source |
| Event log backups | daily encrypted `pg_dump` in R2 bucket `spawn-store`: `postgres/daily/` (15 d), `weekly/` (60 d), `monthly/` (400 d), bucket-locked against deletion, expired by R2 lifecycle; newest also in `~/.local/share/spawn-rsi/backups/` ([deploy/backup](../deploy/backup/README.md)) | They are the copy; the key is kept off-host |
| What an engine did | read by the Bridge from transcript and pane into the log | Yes: in the log |
| Full transcript text | `~/.claude/projects/...` on the compute host, copied to `<root>/.transcripts/<agent>/` at turn ends and execution ends | From the archive, on the same host (restored before a resume when missing); **no** on another host |
| Code: mission sources | bare repos (`missions/*.git`) or remote URLs | Remote URLs: yes |
| Code: agents' work | per-agent branches in host-local mirrors (`ws/.sources/*.git`), pushed to the source as `spawn/<agent>` | Yes for missions with a source; no for scratch missions and the default repository |
| Work a task lands | the source's `spawn/<agent>`, fetched by the integrator into its mirror's `refs/spawn-integrate/<agent>`; judged only when it contains the agent's latest checkpoint (its `artifact.committed` in the event log) | Yes: the source holds it; nothing on the host that ran the agent is needed |
| Large files in workspaces | the chunk store (`SPAWN_CHUNK_STORE`, else `<root>/.chunks`), named by the branch head's `Spawn-Large` trailer | **No**: host-local (chunks in R2 are not built) |
| Backlog, scenarios, gate | files in the controller's checkout, by absolute path | **No** on another host (versioning them with the source is not built) |
| Secrets | `spawn.env`, `~/.config/spawn/*`, mode 600 | Deliberately not stored |

## Where the one store runs

This host plus Cloudflare R2. PostgreSQL stays here and its backups go to an
R2 bucket (built). Chunks are meant to go to the same bucket, and other hosts
are meant to reach PostgreSQL through a Cloudflare Tunnel + Access (neither
is built). The ways to reach one store, and their trade-offs:

| Option | What it takes | Trade-off |
| --- | --- | --- |
| Private network (Tailscale/WireGuard) to this host | install it on each host | no TLS work; this host stays the single point of failure |
| Managed PostgreSQL + S3-compatible bucket (e.g. Neon/Supabase + R2) | TLS client; move the data once | off-host durability and backups; a bill and a provider |
| Cloudflare Tunnel + Access in front of this host, R2 for chunks (the one chosen) | `cloudflared` on each client | reachable from anywhere without an open port |
