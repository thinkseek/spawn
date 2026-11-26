# Universal file provenance

Which change made each file as it is, who made it (agent, execution,
task, preset, model) and what it used. Spawn's Git history already holds
most of it: every checkpoint is a commit (`spawn checkpoint
<execution_id>`), and every landing is a merge commit on `main`'s
first-parent line whose second parent is the agent's branch, so every
checkpoint stays reachable from `main`. What was missing is an index:
three append-only tables, filled from git and the event log.

- PV1 (landed): the tables `prov_changes`, `prov_entries`, `prov_edges`,
  their writer `spawn_prov_record_change` (owner only) and their reads
  ([`crates/spawn-postgres/src/provenance.rs`](../crates/spawn-postgres/src/provenance.rs),
  `sql/schema.sql`). Nothing is ever updated or deleted.
- PV2 (this page): `spawn prov index`, `spawn prov who`, `spawn prov show`
  ([`crates/spawn-cli/src/provenance.rs`](../crates/spawn-cli/src/provenance.rs),
  the walker in
  [`provenance/index.rs`](../crates/spawn-cli/src/provenance/index.rs)),
  and two read-only routes, `GET /api/prov/who` and
  `GET /api/prov/change/<id>` ([`serve/prov.rs`](../crates/spawn-cli/src/serve/prov.rs),
  [docs/api.md](./api.md)).
- PV3a (this page): the landing writes its `land` Change itself, right
  after the push, and a `Spawn-Change` trailer; `spawn prov blast`; and
  three planning commands, `revert --plan`, `split --plan --paths`,
  `compose --plan` ([`provenance/plan.rs`](../crates/spawn-cli/src/provenance/plan.rs)),
  with one more read-only route, `GET /api/prov/blast`. The hook's rows
  are the ones the indexer writes; the indexer stays the truth.
- PV3b (this page): the data domain, directories that are not in Git
  (datasets, shards, model outputs): `spawn prov snapshot`, `spawn prov
  refs`, `spawn prov rollback [--plan]`, and `blast` across data
  snapshots ([`provenance/data.rs`](../crates/spawn-cli/src/provenance/data.rs)).
  The bytes go to spawn-cas: manifest-v1 and the snapshot pins are
  [`crates/spawn-cas/src/manifest.rs`](../crates/spawn-cas/src/manifest.rs).
  No table, column or event is added: a data ref is a domain of the same
  three tables.

## The model

- **Object**: immutable bytes named by a prefixed digest. In the Git
  domain `git1:<oid>`, a git object id (a blob, a tree, a commit).
- **Snapshot**: a tree at a boundary. In the Git domain a commit,
  `git1:<commit>`.
- **Change** (`prov_changes`): one capture, `(change_id, rev)`: its kind,
  its attribution, its `base` and `result` snapshots, its boundary and
  its time. A Change evolves by revisions: each checkpoint of a task's
  branch is one more `rev` of the same `change_id`.
- **Entry** (`prov_entries`): a path the Change touched, its content
  digest `before` and `after` (null: absent), its size, its class and,
  for a landing, its `origin`: the revision whose content it holds.
- **Edge** (`prov_edges`): a typed relation from a Change to a
  `change_id` or a digest. PV2 writes `includes`: a landing includes the
  `edit` Changes of the checkpoints it brought in, `{"select": "all"}`.

| Kind | Written by PV2 for | Boundary | Attribution |
| --- | --- | --- | --- |
| `edit` | a `spawn checkpoint` commit: brought in by a merge's second parents, on `main`'s first-parent line, or on an unlanded `spawn/*` branch | `checkpoint` | `host` or `inferred` |
| `land` | a first-parent commit of `main` with a `Spawn-Task` trailer | `land` | the task's (`host`), `inferred` when no task row exists |
| `import` | any other first-parent commit of `main` | `backfill` | `imported` (agent `imported`) |

## Identity (decisions B2, binding)

These rules are shared by the indexer and the landing hook (PV3a), so
both write the same rows (the hook runs the indexer's own walk).

- **`change_id` is derived, never random**: `c` and the first 26
  characters of the lowercase base32 (RFC 4648, no padding) of
  `sha256("spawn-change-v1" ‖ 0x00 ‖ domain ‖ 0x00 ‖ key)`. The domain is
  `git:<source>`, the source as missions name it (`spawn prov index
  --source`). The keys:
  - `edit:task:<task id>` for a task's branch;
  - `edit:branch:<ref>:<merge base>` for a branch with no task: an
    unlanded `spawn/<agent>` and its merge base with `main`; a merge with
    no `Spawn-Task`, `spawn/<agent>` (or `unknown`) and the merge base of
    its parents; a checkpoint on the first-parent line, `main` and its
    parent;
  - `land:<task id>:<Spawn-Head>` for a landing (the trailer as written;
    the merge's second parent when it has none);
  - `import:<commit>` for an imported commit (PV2's own: B2 names no
    key for it);
  - `data:<ref>` for a data ref (PV3b), in the domain `data:<ref>`.
- **`rev`** is `max(rev) + 1` of the Change for each commit not yet
  recorded, in the walk's order. The idempotency key is
  `(change_id, result)`; the indexer is stricter: a commit already
  recorded anywhere in the domain is never recorded again, whatever key
  it would get now.
- **Attribution** comes from the event log, never from text an agent
  wrote: `host` when the execution the checkpoint's subject names has an
  `execution.started` in the log (the agent is the log's), `inferred` when
  only the subject names it (the agent is the landing task's, the
  branch's `spawn/<agent>`, else `unknown`), `imported` for a first-parent
  commit with no `Spawn-Task`. A checkpoint's task is the landing's
  (`Spawn-Task`); off a landing, the agent's latest task created at or
  before the commit's time. `preset_id` is the task's and `model_label`
  that preset's, copied by value. A landing's trailers are the
  integrator's own: `landing_message` indents every line of a report that
  could read as one, and only a message's last paragraph is read as
  trailers.
- **Entries** come from `git diff-tree -r -z --no-renames` from the first
  parent (the empty tree for a root commit). A checkpoint with more than
  one parent (a merge of main that resolved a conflict) keeps only the
  paths where it differs from every other parent: what came from `main`
  is not its. A mode-only change is `mechanical` (the one entry whose
  `before` equals its `after`). `Cargo.lock` (at any depth) and
  `docs/feature-map/registry.json` are `generated`; `git check-attr
  spawn-prov` overrides the class (`content`, `mechanical`,
  `generated`), read when the commit's tree has a `.gitattributes` at its
  top. Over 10,000 entries in one Change, the paths under a top-level
  directory fold into one `tree` row per directory, its digests the
  directory's trees. A path that is not UTF-8, holds a control character
  or begins with `"` is stored quoted as git quotes it (`"\377"`).
- **Origin**: a landing's entry whose `after` equals the `after` of one of
  the checkpoints it brought in, for the same path, names the last such
  checkpoint's revision. A path whose content is none of them (a merge
  resolution) has no origin: it is the landing's own.
- **A provenance write never fails or delays a landing.** The indexer is
  the truth; the landing hook only makes it immediate.
- Every recorded commit id is read through `spawn_commit_now`: the
  results the domain holds (so a commit the history rewrite renamed is
  recorded already), and `show`'s commit, both ways.

## The walk

`spawn prov index --source <source> [--repo <git dir>]` reads the
repository (`--repo`, else the source) with git alone, oldest first:

1. every first-parent commit of `refs/heads/main`. A commit already
   recorded is skipped with everything under it. For a merge, every
   `spawn checkpoint` commit its second parents bring in (`git rev-list
   <P2> ^<P1>`, parents first) is recorded first, then the merge's own
   `land` or `import` Change with its `includes` edges and origins;
2. every `refs/heads/spawn/*` branch, for its checkpoints `main` does not
   hold.

Other commits of a branch (`spawn: merge main ...`, a set-aside) are no
Change of their own. Each Change is recorded atomically, and a landing
only after its checkpoints, so a run killed at any point is completed by
the next run, and a second run records nothing (it prints
`"recorded": 0`). One run at a time: a concurrent run that takes the same
revision is refused by the store ("recorded already with different
content") and a new run completes. Comparing two databases, compare an
entry's `origin` by the `change_id` and `rev` it names: row ids differ,
and a killed run leaves gaps in them.

It prints one JSON line: the domain, how many Changes it recorded (`edit`,
`land`, `import`) and how many first-parent commits were recorded before.

## Reading it back

- `spawn prov who <path>[:<line>] --source <source> [--repo <git dir>]`:
  with a line, `git blame` of `main` names the commit that made it, and
  the answer is the Change recorded for that commit (a checkpoint's
  `edit` revision, a landing's `land` for a merge resolution, an
  `import`), with its agent, execution, task and attribution. Without a
  line, the last first-parent commit of `main` that touched the path, its
  Change, its entry and the revision the entry's content came from
  (`origin`): the attribution is that revision's. `"indexed": false` when
  the commit has no Change yet.
- `spawn prov show <change id|task|commit>`: every revision of a Change,
  every Change of a task, or the Change recorded for a commit (an
  abbreviated id needs `--repo` or `--source`), each with its entries
  (an origin also by `origin_change_id` and `origin_rev`) and edges.
- `GET /api/prov/who?path=<path>[&source=<source>]` and
  `GET /api/prov/change/<id>` answer the same from the database alone, on
  the reader login (decision D8: the reader selects the three tables). A
  line's blame needs the repository: the route refuses `line` (400).

## At landing (PV3a)

- **`Spawn-Change` (decision D1).** Every landing whose branch brings in
  at least one `spawn checkpoint` commit ends with `Spawn-Change:
  <change_id>`, its task's `edit` Change (`edit:task:<task id>` in
  `git:<source>`), the last trailer line, after `Spawn-Task`,
  `Spawn-Objective` and `Spawn-Head`. The integrator writes it where the
  message becomes the commit (`landing_candidate` in
  [`integrate/landing.rs`](../crates/spawn-cli/src/integrate/landing.rs)):
  `landing_message` is given no source, and the mirror knows it (it was
  cloned from it: `remote.origin.url`). A message without `Spawn-Task`,
  a branch without a checkpoint or a mirror without an origin carries
  none. It is the anchor that survives a squash or an export: a commit no
  Change records is read through its trailers by `who`.
- **The hook.** Right after the push moved main and `task.landed` is
  recorded, `land()` calls `record_land`: the indexer's own walk, on a
  throwaway view of the mirror (its objects by alternates, its only branch
  `main` at the landing, removed afterwards). With every earlier commit of
  main recorded, the only new first-parent commit is the landing: its
  checkpoints are recorded first, then its `land` Change with its
  `includes` edges and origins, with the rows `spawn prov index` writes
  (tested against an indexer run in a second database). A landing whose
  first parent is in no Change of the domain is left to the indexer
  (main's history is not indexed yet), and so is one recorded already.
- **It never fails or delays a landing.** It runs after the push and the
  record; an error, or even a panic, is logged (`integrate: task N:
  provenance not recorded ...`) and the landing stands; `spawn prov index`
  records the same rows later (tested with the database unreachable at
  the hook).

## Blast radius

`spawn prov blast <change id|task|commit>` (and `GET
/api/prov/blast?target=<id>`, the same JSON, from the database alone on
the reader login):

- `changes`: the Changes the target names;
- `later`: every later Change (by time, then id) with an entry on one of
  their paths (a `tree` entry of its top-level directory counts), each
  with those paths; a landing that `includes` one of them is listed in
  `landed_by` instead;
- `radius`: every Change that depends on them, by an edge (to its
  `change_id`, its result or an entry's content) or an entry's `origin`,
  nearest first, each with the link it was reached by and one shortest
  path (`change_id#rev`);
- `used`: the part of the radius reached by `used` edges alone (read along
  the shortest path). The Git domain writes none; data snapshots (PV3b) do.

## Plans

`spawn prov revert --plan <target>`, `spawn prov split --plan <target>
--paths <path>[,<path>...]`, `spawn prov compose --plan <target>...`,
each with `--source <source> [--repo <git dir>]`, and `--onto <commit>`
(main by default) for a revert or a compose. Each prints the exact tree
its result has and the commands that make it, and **executes nothing**:
no ref is written, and no object either (the merges are tried in a
throwaway view, removed with it). A plan is carried out as an ordinary
candidate branch: a task's workspace runs the commands (the
`test "$(git write-tree)" = <tree>` among them checks the tree), the task
is reported, and the gate judges and lands it. There is no second
landing path.

- **The Change a target names**: a commit's Change; a task's last `land`;
  a Change's last revision, or, for an `edit` Change, the last landing
  that includes it. Its result commit and the result's first parent (its
  base) are applied, `-m 1` for a merge.
- **revert**: the tree `git revert --no-commit [-m 1] <result>` leaves on
  `onto`: the merge of `onto` and the base from the result, by `onto`'s
  merge rules. A conflict names its paths, with no tree.
- **split**: the Change in two. The first part is its base with the named
  paths (a file, or every path the Change touched under a directory) as
  its result holds them (`git checkout --no-overlay <result> -- <paths>`);
  the second is its result (`git checkout --no-overlay <result> -- :/`).
- **compose**: the Changes applied on `onto` in the order given, as
  successive `git cherry-pick --no-commit [-m 1]`; a conflict names the
  Change and its paths.
- **Granularity: the file.** A part of one file (`<path>:<line>` or
  `<path>:<from>-<to>`) is refused with `"granularity": "file"` (exit 1),
  for a binary file always and, in this version, for a text file too
  (decisions B4, B7); each refusal is logged on stderr, so the requests
  can be counted. A compose takes whole Changes (split one first); a
  revert takes no `--paths`.

## Data snapshots (PV3b)

A data ref (`--ref <name>`, `[A-Za-z0-9_][A-Za-z0-9._-]{0,127}`, never
64 hex digits) is a named directory whose successive states are recorded
like a branch's checkpoints: one Change, `change_id` derived from the
domain and key `data:<ref>` (B2), one revision (`max(rev) + 1`) per state.
Its **head** is the newest revision of the domain `data:<ref>`
(`PostgresStore::prov_head`): the database is the pointer, so moving it is
one atomic insert.

### manifest-v1 and the snapshot's identity

manifest-v1 **is** the `tree_sha256_v1` line list of spawn-node v1
(section 7, [docs/node-protocol-v1.md](./node-protocol-v1.md)): one line
`<64 hex SHA-256 of the file>  <path>\n` per regular file under the root,
sorted bytewise by path. Symbolic links are never followed (one inside the
root is left out, one that points outside it is refused), and other
special files are left out. The snapshot is `sha256:<tree_sha256_v1>`, the
SHA-256 of those bytes, algorithm-prefixed (D2): an unprefixed digest is
refused everywhere (`--input`, `rollback`, `blast`, `show`). File sizes are
not part of the identity; they go in the entries (`prov_entries.size`).
spawn-node v1's shared fixture
`crates/spawn-node-proto/testdata/node-protocol-v1/tree/` is
`sha256:191e8f57890fd35474a61e33b8876130b98c63388e00d05e7b4a36f4eb4e4707`
(tested in both crates). The empty directory is `sha256:e3b0c442...`, the
SHA-256 of nothing: the `base` of a ref's first revision.

### `spawn prov snapshot`

`spawn prov snapshot <dir> --ref <name> [--cas <dir>] [--input <ref or
digest>]... [--code <commit>]... [--config <file>]... [--task N]
[--digest-only]`:

1. **The root is checked first (D5)** and nothing is written when it is
   refused: a root inside the workspace root (`SPAWN_WORKSPACE_ROOT`, or
   any directory above it that holds the transcript archive
   `.transcripts`), a root that holds the workspace root, a root holding a
   transcript directory (`.transcripts`, `.transcripts-progress`,
   `.transcripts-sources`) at its top or anywhere below it, so a `*.jsonl`
   transcript is never copied into a store, a root outside
   `SPAWN_DATA_ROOT` when that is set, a store (`--cas`) inside the root,
   and a symbolic link that leaves the root.
2. Its manifest is built and its objects stored in spawn-cas, namespace
   `data` (`NS_DATA`), each verified on the way in (a file that changed
   meanwhile fails the run: snapshot again). `--digest-only` stores the
   manifest alone. The store is `--cas`, else `<SPAWN_WORKSPACE_ROOT>/.cas`.
3. When the snapshot differs from the head, one revision is recorded:
   `derive` when an `--input` is given, else `import`; boundary `job`;
   `base` the head (or the empty snapshot), `result` the snapshot; entries
   the paths that differ from the head's manifest (`before`/`after`
   `sha256:` digests, `size` the file's), folded past 10,000 entries into
   one `tree` row per top-level directory (its digests the directory's own
   manifest-v1 digest; a top-level file stays a row) as in the Git domain;
   and one `used` edge per `--input` (a ref: its head snapshot, the detail
   naming the ref, its `change_id` and `rev`; a digest: itself), `--code`
   (`git1:<commit>`, a full id) and `--config` (the file's `sha256:`, the
   detail its path). With `--task N` the revision is the task's agent,
   preset and model, `inferred` (the flag is the caller's word); without,
   an import is `imported` and anything else `unknown`.
4. **A directory equal to the head records nothing** (`"recorded":
   false`), whatever its flags: the idempotency of `(change_id, result)`
   is held against the head. A result equal to an older revision is a new
   revision (that is what a rollback, or an edit undone, is).
5. `pins/prov/<ref>` is rewritten to keep the **3 newest distinct
   snapshots** of the ref (`KEEP_SNAPSHOTS`): for each, a `# snapshot
   <digest>` line, its manifest's digest and its files'. The objects of a
   snapshot that leaves the pin, which no pin names any more and which no
   namespace but `data` records, are removed at once (`"dropped"`).

It prints one JSON object: the ref, its domain and `change_id`, the
snapshot, files and bytes, whether a revision was recorded (with its
`rev`, `kind`, entry count and whether they folded), the `used` edges,
and the snapshots kept and dropped.

**Class B, crash-only.** Nothing is fsynced. An object is renamed into
place whole after it was verified, a pin file is replaced whole (temporary
file and `rename`) under the pin/GC lock, and the head is one atomic
database insert recorded after the bytes are stored and before the pin is
rewritten. A run killed at any instant (SIGKILL included, tested) leaves
whole objects, the old pin or the new one, and the old head or the new
one; running it again completes it (the head equals the snapshot, so it
records nothing and rewrites the pin). At worst a killed run leaves
objects no pin names, which a `data` quota collects (`Store::gc`).
One snapshot of a ref at a time: a concurrent one that takes the same
revision is refused by the store ("recorded already with different
content"), and running it again records it.

### `spawn prov refs` and `spawn prov rollback`

- `spawn prov refs [--cas <dir>]`: every `data:<ref>` domain, its head
  (`snapshot`, `rev`, `kind`, `at_ms`), its revision count and, with a
  store, the snapshots its pin keeps.
- `spawn prov rollback --plan <ref> <snapshot> [--cas <dir>]` prints the
  rollback (from, to, the revision it would record and its entries) and
  **writes nothing**; a rollback that would be refused prints why and
  exits 1.
- `spawn prov rollback <ref> <snapshot> [--cas <dir>] [--task N]` moves
  the head: a `revert` revision of the ref's Change, `base` the head,
  `result` the snapshot, its entries the paths that differ, each with
  `origin` the newest revision that held the snapshot; then the pin keeps
  it as newest. The directory itself is not touched. It is refused, with a
  plain message and nothing recorded, when the snapshot was never one of
  the ref's, or when its bytes are no longer pinned and present (a snapshot
  past the 3 newest, whose objects were dropped, or a `--digest-only`
  one). A rollback to the head records nothing.

### Blast across data snapshots

`spawn prov blast` takes two more targets: `data:<ref>` (its Change, every
revision) and a snapshot `sha256:<hex>` (the revisions whose result it
is). A commit or a snapshot is also followed through the edges that name
it directly, so `blast <commit>` reaches every data snapshot made with
`--code <commit>`, whether or not the commit is indexed in a Git domain.
Through `used` edges the walk goes from a snapshot to what read it (its
result, or the content of one of its entries, `--input sha256:<file>`),
and on. The answer gains `snapshots`: the data snapshots among the
targets and their radius, nearest first, each with its ref, snapshot,
`change_id`, `rev`, kind, depth and link. With the merge of a pipeline
recording one ref per output, each with the digests of the shards it read
as inputs, a corrupted shard's snapshot names exactly the outputs that
read it (`crates/spawn-cli/tests/prov_data.rs`).

### Why no chunk store (decision D7)

Spawn once had a large-file chunk store, `Spawn-Large`:

- `9e39dd8c` (2026-11-04, docs/storage.md) made a workspace snapshot "a
  git commit plus the large-file manifest its trailer names": files over
  a size went to FastCDC chunks (zstd objects under `<root>`), and the
  checkpoint commit carried a `Spawn-Large` trailer naming the manifest
  that put them back; landing stayed text-only.
- `d4e58273` (storage phase 2, the last change to it) kept the head's
  `Spawn-Large` trailer across merges of main into an agent's branch.
- `198cf5de` (2026-11-05) removed it, the owner's decision (CUTS.md): it
  was local only (the chunks lived on the disk of the host that made
  them) and unused live (no `Spawn-Large` commit existed), and a commit
  whose files were partly outside Git was not a whole workspace by
  itself. With it went
  `spawn_host::chunks`, chunk GC and `spawn chunks gc`, the stat cache, the
  `ChunkStore` port, `SPAWN_CHUNK_STORE` and the blake3, zstd and rustix
  dependencies. A large file is now an ordinary git blob up to
  `SPAWN_MAX_FILE_MB`; a bigger one is left out of the checkpoint and
  reported, and a branch whose head still names a `Spawn-Large` manifest
  is refused at mount (`FilesOutsideGit`).

What that teaches PV3b, and how it is held: **a Git commit stays
self-contained.** Data snapshots never put a trailer, a manifest or any
other pointer into a checkpoint or a landing, and nothing in a commit
needs a store to be the whole tree; a commit's tree is the code alone, as
`main` holds it. The link goes the other way: a data snapshot names the
commit it was made with (a `used` edge to `git1:<commit>`), in the
provenance tables. The bytes live in spawn-cas, the one store already
shared with spawn-node, as content-addressed objects with explicit pins
and no separate chunk format, GC or port of their own; and a data root is
never inside a workspace, so no checkpoint ever holds, or leaves out,
data. Losing the store loses data bytes, never a commit.

## Limits

- The domain is the `--source` text: index each source under the name its
  missions use, or its Changes go to another domain.
- A landing recorded already is skipped without walking its branch: a
  landing hook that wrote the landing but not every checkpoint before it
  would leave those checkpoints out until the landing is recorded
  afresh. The hook writes the checkpoints first (it is the indexer's walk).
- The `Spawn-Change` trailer names the task's `edit` Change. A checkpoint
  the indexer met earlier on an unlanded `spawn/*` branch with no task at
  its time keeps the `edit:branch:...` Change it was recorded under: the
  landing's `includes` edge names that one.
- The owner's own landings (`spawn mission landing`) carry no `Spawn-Task`
  and so no `Spawn-Change`, and the hook runs for the integrator's
  landings only: the indexer records the others.
- `GET /api/prov/who` counts `land` and `import` Changes only: a
  checkpoint that reached `main`'s first-parent line by a fast-forward is
  found by `spawn prov who`, which reads git.
- Large files left out of a checkpoint (`SPAWN_MAX_FILE_MB`) are in no
  commit and so in no entry (decision D6, PV4).
- Data snapshots: no route answers `refs` yet (the commands do, and `GET
  /api/prov/blast` and `/api/prov/change/<id>` read data Changes like
  any other). A rollback moves the head and does not rewrite the
  directory. Paths with a newline cannot be in a manifest-v1 (one line per
  file) and are refused. A blast follows a snapshot's own result and its
  entries' contents: an output recorded as one ref for many inputs is
  affected as a whole, so record one ref per output to get the exact ones.
  The 3 kept snapshots are per ref; older bytes are gone once dropped.
