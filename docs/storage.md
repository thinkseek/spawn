# Storage: one place, by kind of object

<<<<<<< 0b37b9d90137f1764ea3b9def673aa5e0c8c3ff7
Status: design (2026-11-04). Phase 0 and phase 2a (fork, `main` merged into
idle branches, branches pushed to the source, resume on another host) are in
the code; phases 1, 2b and 3 are the plan.
=======
Status: design (2026-11-04). Phases 0 and 1 are in the code; phases 2–3 are the plan.
>>>>>>> b1d3a6531158f966a00d42b42cfb0982f35d20ca

The goal: everything Spawn knows lives in one store that any machine with the
right credentials can reach, and work in it branches and merges the way
LayerFS models it (a branch per agent, a checkpoint per tool call, fork from
any commit, merge back automatically) — but durably. LayerFS itself is not the
store: its Store "runs in its Disposable profile only, with no crash or
power-loss durability" (its README), it is one SQLite file, and v3 dropped it
because rejected branches could not be deleted.

Git is not the store either. It versions text; it stores every version of a
file whole. Measured on this host (git 2.56, btrfs):

| Case | Cost per checkpoint |
| --- | --- |
| This repository's tree (5.8k files, ignored build output), index rebuilt every time (before phase 0) | 7–25 ms |
| Same, index kept between checkpoints (phase 0) | 2–3 ms |
| A 200 MiB binary with 1 MiB rewritten between checkpoints | **3.1 s** on the Bridge's loop, **+211 MiB** in the repository each time; repacking three versions still takes 622 MiB |

So each kind of object goes where its shape fits.

## Kinds of object

| Kind | Examples | Shape | Branch / merge | Store |
| --- | --- | --- | --- | --- |
| Facts | events, commands, missions, tasks, gate verdicts, landings | small rows, append-only | no branches: one truth, folded | PostgreSQL (`spawn_events` and folds), as today |
| Source text | code, docs, configs, scenarios, backlog | small text files | branch per agent; three-way merge with the source's `.gitattributes` rules (read from `main`) | git: the mission's source repository |
| Workspace snapshots | the whole tree an agent works in, at every tool call | many files, few change per checkpoint | a branch is a git commit (text) plus a large-file manifest it names; fork is O(1) | the agent's git branch + the chunk store |
| Large and binary files | weights, datasets, images, archives, anything over 8 MiB | MB–GB, opaque, small edits | no three-way merge: per-path rule, newest wins or conflict | content-defined chunks (FastCDC), zstd, by hash, in the chunk store |
| Streams | transcripts, engine output, gate logs | append-only, read in order | none | compressed segments in the chunk store, indexed by execution in PostgreSQL |
| Derived caches | `target/`, `node_modules/`, the kache store | rebuildable, large, hot | none; never authoritative | local to each host (kache may share a remote) |
| Secrets | `spawn.env`, test URLs, keys | tiny | none | never stored centrally |

The chunk store is content-addressed: an object is named by the hash of its
bytes, written once, never changed. A local directory first, any S3-compatible
bucket (R2, S3, MinIO) later. Identical chunks across files, versions,
branches and agents are stored once, so a 1 MiB edit of a 200 MiB file costs
about 1 MiB, not 200.

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
  trailer. Cost is proportional to what changed.
- **Branch**: `refs/heads/<agent>`, moved by compare-and-swap on the old
  commit, as today.
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
  local one stays). Chunks it lacks come with phase 1 and 3.
- **Central**: every branch that moved (a checkpoint commit, a merge, a new
  branch) is pushed to the mission's source as `spawn/<agent>` without force
  (two writers of one branch are caught: the push is refused and warned, not
  overwritten), from a background pusher off the Bridge's loop: at most one
  push per branch every 5 s, carrying the head as it is then, failures
  retried with backoff, and what still waits pushed when the store drops (a
  one-shot `spawn tick`). Chunks in R2 (phase 3).
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
  with reason `base_merged`. A conflict in files' lines is made resolvable
  the way `git merge` does it: the merge tree with conflict markers is written
  into the idle workspace by the same safe procedure (a directory that differs
  from the branch refuses, changing nothing), the branch stays where it is,
  and `main` waits as the pending second parent under the store root
  (`.merge/<agent>`, written atomically). The agent hears of the conflict at
  landing, as today, and finds the markers in its files. Checkpoints keep
  single parents while any conflicted file still holds a marker (a line that
  starts with `<<<<<<< ` or `>>>>>>> `, or is `=======`); the first one
  without commits with parents (head, pending `main`) and clears it, after
  which the branch lands clean. No newer `main` is merged on top of
  unresolved markers. A conflict markers cannot show (modify/delete, renames,
  binary files) changes nothing; the tick-report warning says so. Large files
  are not merged: the branch keeps its own (a merge commit carries the head's
  `Spawn-Large` trailer). The pending parent is host-local: an agent resumed
  on another host before resolving commits the resolution with one parent,
  and `main` is merged again there.
- **Landing**: unchanged and text only. The gate judges the git candidate; the
  integrator commits host-side and moves `main` by compare-and-swap. Large
  files are workspace state, not source, and never land.

## Phases

0. **Done.** The checkpoint index is kept between checkpoints (stat cache),
<<<<<<< 0b37b9d90137f1764ea3b9def673aa5e0c8c3ff7
   and files over 8 MiB stay out of git commits (a warning names them once;
   until phase 1 they are not on the branch and are lost at unmount).
1. **Chunk store; large files kept.** A chunk-store port with a local
   directory adapter (content-defined chunks, BLAKE3, zstd, verified on read);
   the large-file manifest and the `Spawn-Large` trailer; mount materializes
   the large files again.
2. **Branches in the store.** (a) **Done.** Fork from any commit (`start
   {from}`), resume on any host, branches pushed to `spawn/<agent>`, `main`
   merged into idle branches automatically. (b) Backlog, scenarios and gate
   versioned with the source; the integrator judges `spawn/<agent>` from the
   source, so work done on another host lands too.
=======
   and files over 8 MiB stay out of git commits.
1. **Done.** **Chunk store; large files kept.** A chunk-store port
   (`spawn_core::port::ChunkStore`) with a local directory adapter
   (`spawn_host::chunks`: gear-hash FastCDC chunks of 128 KiB–2 MiB, about
   512 KiB on average, named by BLAKE3, zstd-compressed, written by temporary
   file and link so an object is never rewritten, verified on read); the
   large-file manifest is a chunk-store object and the checkpoint commit names
   it in a `Spawn-Large` trailer; only large files whose size, mtime or mode
   changed are chunked again (a stat cache in `<root>/.large/<branch>`); mount
   materializes the large files again, each atomically, and fails rather than
   write bytes that do not verify. The store is the directory
   `SPAWN_CHUNK_STORE` names, else `<root>/.chunks`. A branch head that names
   a manifest lands as a commit of the same tree without the trailer, so
   large files never reach `main`.
2. **Branches in the store.** Fork from any commit (`start {from}`), resume on
   any host, branches pushed to `spawn/<agent>`, `main` merged into idle
   branches automatically.
>>>>>>> b1d3a6531158f966a00d42b42cfb0982f35d20ca
3. **Streams and reach.** Transcripts and gate logs as segments; the chunk
   store in R2; PostgreSQL reachable from other hosts through a Cloudflare
   Tunnel; `pg_dump` backups to R2.

## Where state lives today

| State | Where | Rebuildable? |
| --- | --- | --- |
| Event log | PostgreSQL, one container on this host, bound to 127.0.0.1 | It *is* the source |
| Event log backups | daily encrypted `pg_dump` in R2 bucket `spawn-store`: `postgres/daily/` (15 d), `weekly/` (60 d), `monthly/` (400 d), bucket-locked against deletion, expired by R2 lifecycle; newest also in `~/.local/share/spawn-rsi/backups/` ([deploy/backup](../deploy/backup/README.md)) | They are the copy; the key is kept off-host |
| What an engine did | read by the Bridge from transcript and pane into the log | Yes: in the log |
| Full transcript text | `~/.claude/projects/...` on the compute host | **No**: phase 3 |
| Code: mission sources | bare repos (`missions/*.git`) or remote URLs | Remote URLs: yes |
<<<<<<< 0b37b9d90137f1764ea3b9def673aa5e0c8c3ff7
| Code: agents' work | per-agent branches in host-local mirrors (`ws/.sources/*.git`), pushed to the source as `spawn/<agent>` | Yes for missions with a source; no for scratch missions and the default repository |
| Large files in workspaces | the workspace directory only | **No**: phase 1 |
| Backlog, scenarios, gate | files in the controller's checkout, by absolute path | **No** on another host: versioned with the source in phase 2b |
=======
| Code: agents' work | per-agent branches in host-local mirrors (`ws/.sources/*.git`) | **No**: phase 2 |
| Large files in workspaces | the chunk store (`SPAWN_CHUNK_STORE`, else `<root>/.chunks`), named by the branch head's `Spawn-Large` trailer | **No**: host-local until R2 (phase 3) |
| Backlog, scenarios, gate | files in the controller's checkout, by absolute path | **No** on another host: versioned with the source in phase 2 |
>>>>>>> b1d3a6531158f966a00d42b42cfb0982f35d20ca
| Secrets | `spawn.env`, `~/.config/spawn/*`, mode 600 | Deliberately not stored |

## Where the one store runs

Decided (2026-11-04): **this host + Cloudflare R2.** PostgreSQL stays here and is reached from other hosts through a Cloudflare Tunnel + Access; chunks and `pg_dump` backups go to an R2 bucket. Considered:

| Option | What it takes | Trade-off |
| --- | --- | --- |
| Private network (Tailscale/WireGuard) to this host | install it on each host | no TLS work; this host stays the single point of failure |
| Managed PostgreSQL + S3-compatible bucket (e.g. Neon/Supabase + R2) | TLS client; move the data once | off-host durability and backups; a bill and a provider |
| Cloudflare Tunnel + Access in front of this host, R2 for chunks | `cloudflared` on each client | reachable from anywhere without an open port |
