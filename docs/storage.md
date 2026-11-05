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

So each kind of object goes where its shape fits, and a workspace file
over a size cap stays out of git (below).

## Kinds of object

| Kind | Examples | Shape | Branch / merge | Store | Built |
| --- | --- | --- | --- | --- | --- |
| Facts | events, commands, missions, tasks, gate verdicts, landings | small rows, append-only | no branches: one truth, folded | PostgreSQL (`spawn_events` and folds) | yes |
| Source text | code, docs, configs, scenarios, backlog | small text files | branch per agent; three-way merge with the source's `.gitattributes` rules (read from `main`) | git: the mission's source repository | yes |
| Workspace snapshots | the whole tree an agent works in, at every tool call | many files, few change per checkpoint | a branch is a git commit; fork is O(1) | the agent's git branch | yes |
| Large and binary files | weights, datasets, images, archives | MB–GB, opaque | as git merges binary files | git blobs up to `SPAWN_MAX_FILE_MB` (default 100); a larger file stays out of the checkpoint | yes, up to the cap |
| Streams | transcripts, engine output, gate logs | append-only, read in order | none | the transcript archive beside the workspaces (below); gate logs in `task.gated` records | yes, host-local |
| Derived caches | `target/`, `node_modules/`, the kache store | rebuildable, large, hot | none; never authoritative | local to each host (kache may share a remote) | yes |
| Secrets | `spawn.env`, test URLs, keys | tiny | none | never stored centrally | yes |

## Branching and merging

A workspace snapshot is one git commit: its tree holds every file git would
commit, each up to the size cap ("Files over the cap", below). The branch
head alone is the whole workspace, so everything below is a git operation.

- **Checkpoint** (every tool call, turn end, exit): the Bridge commits the
  tree against the kept index (a stat cache: only changed files are hashed).
  Cost is proportional to what changed.
- **Branch**: `refs/heads/<agent>`, moved by compare-and-swap on the old
  commit. A checkpoint or merge removes git's own `<index>.lock` under the
  branch's lock before it runs git on the index: one that is there was
  left by a git that died mid-command, and would fail every later
  checkpoint and merge of the branch.
- **Files the agent wrote** are opened by the controller one way only
  (`spawn_host::agent_files`): parents walked through held descriptors
  without following a symlink, the file `O_NOFOLLOW | O_NONBLOCK`, its stat
  from the descriptor, anything but a regular file refused, reads bounded.
  The pending-merge marker check and the task report and delivery marks
  (`.spawn/report.md`, `.spawn/deliveries/`) read and write that way.
- **Fork from any commit**: a new branch at an existing commit. No data is
  copied; sub-agents fork recursively.
  `start {from}` names another agent of the same mission source (its branch
  head) or a full commit id in the mirror; anything else fails the start. An
  agent whose branch exists keeps it (the tick report warns that `from` was
  ignored). The start that creates a branch records where it began on
  `execution.started` (`fork: {from, commit}`; `from` is `main`, the start's
  `from`, or `spawn/<agent>`).
- **One host**: a branch lives in this host's mirror, and a start takes
  nothing from the source but `main` (the owner's decision: Spawn runs on
  one host).
- **Published for landing**: every branch that moved (a checkpoint commit, a
  merge, a new branch) is pushed to the mission's source as `spawn/<agent>`
  without force (a rewritten branch is refused and warned, not overwritten),
  from a background pusher off the Bridge's loop: at most one push per
  branch every 5 s, carrying the head as it is then, failures retried with
  backoff, and what still waits pushed when the store drops (a one-shot
  `spawn tick`, or `spawn loop` ending on SIGTERM after its pass). The
  queue is in memory, so a new Bridge queues again the branch of every
  agent with an open task: a push lost with a killed process is made after
  the restart (a branch the source already has is a no-op push). Only a source that is a directory on this host receives
  branches: a remote may be public, and every agent commit would leave the
  host.
- **Main at a start or resume**: the Bridge merges main into an agent's
  branch only when an execution starts or resumes, before its workspace is
  mounted, and never while a turn runs: nothing changes files under a
  working agent (two merges between turns, below, are not the Bridge's).
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
  supervisor and the integrator. `notice.typed` and `notice.received`
  records, written while main was merged after every turn, still decode and
  change nothing.
- **Main into a workspace**: a worker that never restarts (a resident) would
  never get main that way, and every piece of its work after main moved
  would be judged `conflict` with nothing to resolve. So main is also merged
  into the agent's mounted workspace (`merge_base`, routed and refreshed by
  `MissionWorkspaces`, holding the branch's lock so no checkpoint reads the
  directory halfway) at two moments when the agent is idle between turns
  (spawn-cli's `main_merge`):
  - the integrator's `conflict` verdict, before the verdict is recorded, so
    its note names the files that now carry markers;
  - the dispatcher's delivery of a task typed into a live worker, so the
    task starts from main and its text says what the merge did. (A start or
    resume mounts later; the Bridge's clean merge above covers it.) The
    text names the files of every `base_merged` record since the last
    delivery the worker was typed, not only this attempt's merge: a
    delivery refused before typing (a pane not ready) is retried, and the
    retry's merge finds main already in.

  A clean merge is a merge commit recorded as `artifact.committed`
  (`base_merged`, its files) and then pushed. A record whose append failed
  is written at the agent's next merge, read back from the branch head (a
  two-parent "spawn: merge main" commit; the record's key makes a repeat a
  no-op). A content conflict writes the merge
  with conflict markers into the directory and leaves `main` pending under
  `<root>/.merge/<agent>`: the checkpoint after the last marker is gone
  takes `main` as its second parent (a checkpoint with markers still in is a
  plain commit, and the merge stays pending), and is gated and landed like
  any other. Until then no newer `main` is merged: markers never stack. It
  is recorded as `merge.conflicted {files, marked: true}`. A conflict
  markers cannot show (a file deleted on one side and changed on the other,
  a rename, a binary file) changes nothing but a copy of `main`'s version of
  each such file under `.spawn/main/` in the workspace, and the note or the
  task text tells the worker, who cannot run git, to make each file exactly
  `main`'s version (or delete it where `main` has none); recorded as
  `merge.conflicted {files, marked: false}`. An agent with no workspace on
  the integrator's host gets no merge, and its note says so.
- **A merge that loses the branch race**: when another writer moved the
  branch between the merge's directory update and its compare-and-swap,
  the directory follows the head that won by the same two-tree update
  (merge tree to the winner's tree), so the next checkpoint does not commit
  the lost merge as edits. When that is refused (a file the two sides
  differ in changed in the workspace meanwhile) the workspace is marked
  **stale** (`<root>/.stale/<agent>`). It keeps its work: each checkpoint of
  a stale workspace commits its tree to
  `refs/spawn-stale/<agent>` (first parent the branch head, second the
  snapshot before it), never to the branch, and answers `WorkspaceStale`;
  merges of `main` are refused. A mount or an unmount commits that snapshot
  before it replaces or removes the directory, and fails, changing nothing,
  when it cannot. The Bridge ends an execution whose workspace answers
  `WorkspaceStale` (its state is kept, so the unmount loses nothing) instead
  of retrying; the next start mounts the branch head afresh. What the
  snapshot holds is not merged back: a person or the supervisor takes what
  they need from `refs/spawn-stale/<agent>` (it is never pushed or
  landed).
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
  rejected ("the worker has no branch to land"). So only a source that is a
  directory on this host has its agents' work landed. The
  rest is unchanged: a branch behind a moved `main` lands as the merge by
  `main`'s merge rules; one that conflicts with it is gated `conflict` after
  `main` is merged into its workspace (above); marker blocks in the files
  the branch changes are gated `conflict`, a pending merge of `main` (it lives with the workspace
  directory, on the Bridge's host) blocks judging when that directory is on
  the integrator's host, and its markers, once checkpointed and pushed, are
  caught by the marker check wherever the integrator runs. The gate judges
  the candidate; the integrator moves `main` by a compare-and-swap push. A
  branch head an earlier version wrote with a `Spawn-Large` trailer lands
  as a commit of the same tree without it.

## Files over the cap

- **The cap.** One file in a checkpoint is at most `SPAWN_MAX_FILE_MB` MiB
  (default 100; anything but a positive number is the default). git stores
  every version of a file whole, so the cap bounds what one checkpoint can
  add to the repository; under it, a large or binary file is an ordinary
  blob, kept across remounts and forks like any other.
- **Over it.** The checkpoint leaves the file out (excluded from `git add`
  before it is hashed, and from the index; one that grew past the cap
  between the size check and `add` is taken out after) and goes on with
  everything else: it never fails because of a large file. The file stays
  in the workspace, on no branch, and is lost when the workspace is
  unmounted. A file over the cap that `main` (or a pending merge's `main`)
  holds as a blob came from the source and stays a blob: leaving it out
  would delete it from `main` when the branch lands.
- **Special files.** A FIFO, a socket or a device in the workspace is left
  out of every checkpoint too, without a notice (`git add` refuses one at a
  tracked path, which would fail every checkpoint while it is there).
- **The agent is told once.** The checkpoint records the files it left out
  in `<root>/.oversized/<agent>` (NUL-separated: told or not, size, path);
  `WorkspaceStore::notices` returns each one not yet told, changing
  nothing. The Bridge logs each notice as a tick warning and puts it before
  the next prompt it types for the session; once that typing is recorded it
  marks them told (`WorkspaceStore::notices_told`). The record is on disk,
  not in the Bridge's memory, so a restart between the checkpoint and the
  prompt loses no notice (at worst one is told twice). A file that drops under the cap
  is forgotten, and told of again if it grows past it again.
- **Old checkpoints.** An earlier version kept files over 8 MiB in a chunk
  store and named them with a `Spawn-Large: <hash>` trailer on the
  checkpoint commit. This version has no chunk store, so a branch whose
  head carries that trailer is refused at mount (`FilesOutsideGit`, naming
  the trailer): its tree is not the whole workspace. The live fleet had no
  such commits when the store was removed.

## The transcript archive

Claude Code's transcript of a native session (one JSONL file under its
projects directory) is the agent's history: `--resume` reads it, and the
Bridge derives the session's events from it. It is not Spawn's to keep.
Claude Code deletes old transcripts (its `cleanupPeriodDays` sweep runs at
every engine start, over the projects directory the owner's own sessions
share), and the format is the engine's. So Spawn keeps its own copy
(`spawn_core::transcripts`, the `TranscriptArchive` port, a local directory
adapter in `spawn_host::transcripts`):

- **Where.** `<root>/.transcripts/<agent>/<native session>.jsonl`, beside
  the workspaces, on the host that ran the engine.
- **When.** After the checkpoint of each turn end, and at the end of each
  execution, after its terminal is killed and before its workspace is
  unmounted: the Bridge reads (through the terminal host) what the file
  gained since the latest copy and appends it,
  each append made durable (`fdatasync`, and the directory for a new file).
  A failure is a tick warning; the next copy takes what was missed.
- **Never overwritten.** The file is append-only. The Bridge compares the
  last 4 KiB it copied with the file at the same place: a file that shrank,
  or whose bytes there differ (rewritten, replaced), is copied whole as the
  next copy, `<native session>.<n>.jsonl`, and the earlier copies stay as
  they were. An append to a copy at any length but its own is refused.
- **Restore on resume.** Before a `resume` starts the engine (after the
  mount), a missing transcript is put back from the
  latest copy, created whole or not at all (a temporary file linked into
  place; never over a file that is there), and the tick warns that it was
  restored. A transcript that is gone with no copy, for a session whose
  events were read from one, fails the resume with that reason and
  unmounts: resuming would start the agent blank under its old id. A
  session that never wrote a transcript resumes as before.
- **The log.** No transcript content and no record of the copies: the
  archive is its own index, and the events already derived from the lines
  are in the log.
- **Not built.** The archive is host-local and is never pruned; moving it
  to R2 is the way off this host.

## Disk room

The host's disks are shared with other projects, so Spawn guards what it
writes (the gate's scratch in `TMPDIR`, workspaces and mirrors under the
workspace root, deploy dumps) so a full disk corrupts no
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
  the gate's scratch directories (exactly
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

- **Files over the cap**: they stay in the workspace only; nothing keeps
  them past an unmount.
- **Streams as segments**: transcripts and gate logs as compressed segments
  in R2, indexed by execution in PostgreSQL.
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
| Large files in workspaces | git blobs on the agent's branch, up to `SPAWN_MAX_FILE_MB`; larger ones only in the workspace | Up to the cap, as code; **no** over it |
| Backlog, scenarios, gate | files in the controller's checkout, by absolute path | **No** on another host (versioning them with the source is not built) |
| Secrets | `spawn.env`, `~/.config/spawn/*`, mode 600 | Deliberately not stored |

## Where the one store runs

This host plus Cloudflare R2. PostgreSQL stays here and its backups go to an
R2 bucket (built). Other hosts are meant to reach PostgreSQL through a
Cloudflare Tunnel + Access (not built). The ways to reach one store, and their trade-offs:

| Option | What it takes | Trade-off |
| --- | --- | --- |
| Private network (Tailscale/WireGuard) to this host | install it on each host | no TLS work; this host stays the single point of failure |
| Managed PostgreSQL + S3-compatible bucket (e.g. Neon/Supabase + R2) | TLS client; move the data once | off-host durability and backups; a bill and a provider |
| Cloudflare Tunnel + Access in front of this host (the one chosen) | `cloudflared` on each client | reachable from anywhere without an open port |
