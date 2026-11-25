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
  - `data:<ref>` for a data ref (PV3b).
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
