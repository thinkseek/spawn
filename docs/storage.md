# Storage: one place, by kind of object

The goal: everything Spawn knows lives in one store that any machine with the
right credentials can reach, and work in it branches and merges the way
LayerFS models it (a branch per agent, a checkpoint per tool call, fork from
any commit, merge back automatically), but durably. LayerFS itself is not the
store: its Store "runs in its Disposable profile only, with no crash or
power-loss durability" (its README), it is one SQLite file, and its rejected
branches cannot be deleted.

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
| Streams | transcripts, engine output, gate logs | append-only, read in order | none | compressed segments in the chunk store, indexed by execution in PostgreSQL | no: transcripts stay on the compute host, gate logs are in `task.gated` records |
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
- **Main into branches automatically**: right after the checkpoint of a turn
  end, and on an idle live execution (no turn open, nothing read that asks for
  a checkpoint, the screen last read ready) at most every 30 s by the Bridge's
  clock, so an agent already idle when the gate reports a conflict finds the
  markers within about 30 s; never while a turn runs or a prompt is typed.
  When the source's `main` moved (the mirror's
  `main` is fetched at most every 30 s for this), it is merged into the
  agent's branch (`merge-tree --attr-source=main`, so the rules come from
  `main`, which only the gate moves; the integrator lands with the same
  rules). A clean merge updates the idle workspace in place first (`read-tree
  -m -u` against a temporary index, which refuses if a file the merge changes
  differs from the branch), then moves the branch by compare-and-swap to a
  merge commit (parents: the head, `main`), recorded as `artifact.committed`
  with reason `base_merged` and the files it changed. A merge changes files
  under the agent, and an agent that rewrites one from stale context would
  make its branch revert `main`; so the Bridge tells it, with the next prompt
  it types for the session (an `Input` or a `Resume` prompt; never
  unprompted): "Spawn merged main into your workspace since your last turn:
  <files>. Re-read them before editing." (for conflicts: the files carry
  conflict markers; edit the marked regions, do not rewrite the files from
  memory). The notice is folded from the log, each file with the position
  of the last merge event that named it. When the Bridge types a prompt that
  carries it, it records `notice.typed {through}` (the last merge event the
  notice told of); the notice clears only when that prompt reaches the
  engine: the transcript line of a prompt that opens with the notice's
  fixed opening gives `notice.received`. Only the files told of
  are cleared, so a merge that lands between the typing and the receipt is
  told next time. A prompt the Bridge did not type (someone at the pane, the
  engine going on by itself) clears nothing, and neither does a restart: a
  notice typed and not yet received is typed again with the next prompt. A
  conflict in files' lines is made resolvable
  the way `git merge` does it: the merge tree with conflict markers is written
  into the idle workspace by the same safe procedure (a directory that differs
  from the branch refuses, changing nothing), the branch stays where it is,
  and `main` waits as the pending second parent under the store root
  (`.merge/<agent>`, written atomically, with the branch head the merge was
  written over), recorded once per conflict as
  `merge.conflicted {files, marked}`. The agent hears of the conflict at
  landing and with its next prompt, and finds the markers in its files.
  Checkpoints keep single parents while any conflicted file the merge listed
  still holds a whole marker block (a line starting `<<<<<<< `, later one
  that is exactly `=======`, later one starting `>>>>>>> `; a lone `=======`
  heading underline is not one); the first one without commits with parents
  (head, pending `main`) and clears it, after which the branch lands clean.
  The integrator does not judge a branch with a pending merge, or with marker
  blocks in the files it changes against `main`: it is gated `conflict` with
  a note to resolve the markers first. Mount and unmount keep a pending
  merge only when the branch carries it: a checkpoint was taken since it was
  written (the head moved on from the head it was written over), so the
  committed tree holds the markers and `main`'s other changes and the next
  export shows them, and `main` is not yet an ancestor of the branch. The
  checkpoint that resolves the markers, in that execution or a later one,
  then takes `main` as its second parent, and the next merge of `main` does
  not merge the markers again. Otherwise the pending merge goes: with the
  head still the pre-merge one, a fresh export holds none of `main`'s
  changes, so `main` must not become its parent (the next idle merge writes
  the merge again). Either way main is never recorded as a parent over a
  tree without its changes. A merge holds the branch's index lock, as a
  checkpoint does. No later
  `main` is merged on top of unresolved markers. A conflict markers cannot
  show (modify/delete, renames, binary files) changes nothing; the
  tick-report warning says so. Large files are not merged: the branch keeps
  its own (a merge commit carries the head's `Spawn-Large` trailer), so a
  merged tree must have room for them: a `main` that puts a file or a
  symlink at a large file's path or at one of its parent directories makes
  the merge a conflict markers cannot show (`marked: false`, the large
  files' paths), and nothing moves.
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
  checkpoint commit names it in a `Spawn-Large` trailer. Only large files
  whose size, mtime or mode changed are chunked again (a stat cache in
  `<root>/.large/<branch>`). A checkpoint that takes chunk names from that
  cache touches each one (makes it young again) and chunks the file again
  when the store no longer holds one, so it never names a collected chunk.
- **Mount.** Mount materializes the large files again, each atomically, and
  fails rather than write bytes that do not verify, or write outside the
  workspace. A manifest whose large file the head's tree has no room for (a
  tracked entry at its path, a file or a symlink at a parent directory)
  fails the mount. Parent directories are walked one component at a time,
  each opened with `O_NOFOLLOW` through the open descriptor of the one above
  (`/proc/self/fd`) and made with `mkdir` when missing, so a symlink or a
  file where the manifest has a directory fails the write; the temporary
  file is created exclusively without following a symlink and renamed
  inside the directory that was verified.
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
  - *Grace*: an object (a file named by 64 hex digits under
    `<store>/<2 hex>/`) goes only when it is not live and older than the
    grace period (default 60 minutes), its time read again just before it
    is removed. Chunks are written before the commit that names them, and a
    `put` or a stat-cache reuse makes an object young again, so a checkpoint
    in flight keeps what it reuses as long as it commits within the grace
    period.
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
| Full transcript text | `~/.claude/projects/...` on the compute host | **No** (streams as segments are not built) |
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
