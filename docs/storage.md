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
commit, each up to the size cap ("Files over the cap", below), except build
output, `.spawn/` and the engine's own runtime files ("The engine's runtime
files", below). The branch head alone is the whole workspace, so everything
below is a git operation.

Every git Spawn runs, the store's and the controller's, runs in the C
locale (`LC_ALL=C`, no `LANGUAGE`; `spawn_host::git_command`). Spawn reads
what git answers (a push's summary, a merge's conflict kinds, listings, the
error it quotes in a record or a note), and a host whose locale is
translated (`LANG=zh_CN.UTF-8`) would otherwise get git's messages in its
language. tmux is given no locale: it has no translations, and one given to
the command that starts its server would reach every pane.

Spawn's own commits never sign, whatever the host's git configuration says:
a checkpoint is `commit-tree`, which ignores `commit.gpgSign`, and a landing
passes `--no-gpg-sign` (a test forces signing and makes both). A test's own
git (a seed's `init`, `add` and `commit`) runs through
`spawn_host::test_git_command` (feature `testing`), without the user's or
the system's git configuration, so a host whose `~/.gitconfig` signs every
commit with a key no agent holds cannot hang or fail a test.

- **Checkpoint** (every tool call, turn end, exit): the Bridge commits the
  tree against the kept index (a stat cache: only changed files are hashed).
  Cost is proportional to what changed. The same tree is built for the
  workspace a set-aside keeps and for a stale workspace's snapshot.
- **A turn's checkpoint is part of the turn's end.** When the Bridge reads a
  turn's end from the transcript, it commits the workspace and records the
  `artifact.committed` first, then the `turn.stopped` (or `turn.failed`),
  in the same tick. A commit that fails holds the line back: the end is
  recorded only once the commit works. A turn that changed nothing has no
  new commit and no `artifact.committed`: its checkpoint is the branch head
  as it stands (the agent's latest `artifact.committed`). So whoever reads a
  turn's end from the log, wherever it runs, also finds the checkpoint of
  what the turn wrote.
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
  `execution.started` (`fork: {from, commit}`; `from` is `main` or the
  start's `from`; only records from before the cross-host cut say
  `spawn/<agent>`).
- **One host**: a branch lives in this host's mirror, and a start takes
  nothing from the source but `main` (the owner's decision: Spawn runs on
  one host).
- **Published for landing**: every branch that moved (a checkpoint commit, a
  merge, a new branch) is pushed to the mission's source as `spawn/<agent>`
  without force (a rewritten branch is refused and warned, not overwritten),
  from a background pusher off the Bridge's loop: at most one push per
  branch every 5 s, carrying the head as it is then, failures retried with
  backoff, and what still waits pushed when the store drops (a one-shot
  `spawn tick`, or `spawn loop` ending on SIGTERM after its pass). A process
  has one pusher per source, for its whole life, and every store it builds
  pushes through it: the Bridge's, and the one-shot stores the dispatcher
  and the integrator build for a merge of main or a set-aside. So a source
  has one queue and one pusher thread per process, however many merges it
  sees. What a one-shot store changed is pushed at once, on the caller's
  thread, never beside a push of the same branch; a push that fails there
  stays queued and the pusher's thread retries it. The queue is in memory,
  so a new Bridge queues again the branch of every agent with an open task,
  and the integrator, when the pushed branch lacks the agent's latest
  recorded checkpoint and this host's branch head is that checkpoint,
  pushes it again before it judges the task `stale`: a push lost with a
  killed process is made after the restart (a branch the source already
  has is a no-op push). Only a source that is a directory on this host receives
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
  (spawn-cli's `main_merge`). Both moments are chosen on facts read
  earlier, so the store asks again, under the branch's lock, after the
  fetch of main and right before it writes anything (`merge_base_when`), on
  the dispatcher's facts read then: no command in flight in the session, no
  stop under way, and a live worker stopped at its prompt, judged on its
  latest fact (the rule a revision is typed in by). The Bridge types into
  an engine under the same lock (`WorkspaceStore::hold`), so a turn typed in
  meanwhile is seen. A worker that is not idle keeps its workspace as it is
  and the merge waits: the integrator's `conflict` note says main is merged
  when the revision is typed in (the dispatcher's merge below), and a
  delivery whose merge finds the worker busy is not sent that pass and is
  tried again on the next:
  - the integrator's `conflict` verdict, before the verdict is recorded, so
    its note names the files that now carry markers;
  - the dispatcher's delivery of a task typed into a live worker, so the
    task starts from main and its text says what the merge did. (A start or
    resume mounts later; the Bridge's clean merge above covers it.) The
    text names the files of every `base_merged` record since the last
    delivery the worker was typed, not only this attempt's merge: a
    delivery refused before typing (a pane not ready) is retried, and the
    retry's merge finds main already in;
  - the dispatcher's fork of a re-assigned objective's ephemeral task from
    the work its predecessor left (docs/fleet-architecture.md §10.4), before
    its Start, when nothing of the agent runs: the branch is created at that
    work's last checkpoint and main merged into it cleanly, or, on a
    conflict, into the workspace mounted for it, the markers committed (a
    plain commit, recorded `base_merged` with no file, so the merge stays
    pending and the Start's mount keeps it) and the workspace unmounted.
    The Start's prompt says what the merge did.

  A clean merge is a merge commit recorded as `artifact.committed`
  (`base_merged`, its files) and then pushed at once (a push that fails
  stays queued and is retried, above). A record whose append failed
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
  `merge.conflicted {files, marked: false}`. `.spawn/` is never committed and
  a mount exports the workspace afresh, so the store records the copies
  (`main`'s commit and the files) under `<root>/.main-copies/<agent>`, and
  every mount (a resume after an engine was lost, a host restart) puts them
  back until the branch contains that `main`. An agent with no workspace on
  the integrator's host gets no merge, and its note says so.
- **Work set aside**: a resident keeps its branch from task to task, so the
  work of a task cancelled while it was delivered (by `spawn task cancel`,
  the supervisor, the integrator's abort over budget, or the closure rule)
  would otherwise stay in its workspace, the next task would start on top
  of it, and in a gated mission the next task's candidate would carry it
  into a landing under another objective's name. Before the resident gets
  anything else, the dispatcher (spawn-cli's `set_aside`) puts a new commit
  on top of its branch whose tree is the checkpoint the task started from:
  the agent's latest `artifact.committed` before the task's first delivery
  was accepted, the merge of `main` typed just before it included (and a
  start's or resume's `base_merged`, recorded before `task.delivered`). It
  runs only while nothing of the agent writes the branch, after the
  dispatcher's Stop took the final checkpoint, under the branch's lock
  (`set_aside`, routed like `merge_base`): a mounted directory holding
  what no checkpoint took is committed first, the directory moves to the
  start's tree the two-tree way, then the branch moves by compare-and-swap.
  Nothing is rewound (pushes are never forced, and the integrator requires
  the published branch to descend from the latest checkpoint), and nothing
  is lost: the work is the set-aside commit's parent, and the worker's
  notice names both commits. The set-aside is recorded as
  `artifact.committed` (`set_aside`, key `set-aside:<task>`, so it is made
  once) and pushed like any checkpoint. A merge pending its markers is set
  aside with the work.
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
- **When an execution has ended** (issue #15): its runner wrote its exit
  status (`exited`), or no runner the Bridge started runs it any more
  (`lost`): its terminal is gone, or is there and hosts no runner. A
  terminal of the execution's name can outlive its runner: a pane kept dead
  by `remain-on-exit`, or a session a terminal manager (tmux-resurrect,
  continuum) restored after a restart at a shell prompt. tmux's
  `has-session` says yes to both, and the Bridge used to call such an
  execution running forever, while the log's guard refused every start of
  its session. Now the tmux host answers `TerminalHost::runner` from
  `list-panes`: the pane alive (`pane_dead` 0) and started by this host's
  launch (`pane_start_command` names the launch's environment file,
  `spawn-launch-<terminal>-`; every launch has one). Anything else is
  `lost`, and its `execution.ended` carries `evidence`: what the host saw
  (the dead pane's status, or what the pane was started with and runs),
  and which command found it. The end is the one above: final checkpoint,
  terminal killed (the restored shell too: no untracked engine is ever
  adopted), unmount, record. A start, a resume and an input settle such an
  execution before they act (`Bridge::settle`), instead of failing on it
  until a later tick: a start or resume then goes on, an input fails once
  (it needs a running engine) so the normal resume path applies; a stop
  ends it as `stopped`, with the same evidence. The Bridge's view of what
  runs changes only by folding the log (an end it wrote is read back
  before the execution leaves its view), and when that view runs nothing
  for a session, a command asks the log for the session's open execution
  (`EventLog::open_execution`, the guard's own rule: the latest
  `execution.started` with no Bridge-written `execution.ended` after it)
  and follows it again, with a warning: a view that missed a record cannot
  answer "runs no execution" while the log refuses every start. At a
  restart, a recorded mount whose start the view did not read is no
  orphan when the log holds that execution open: it is kept and the
  execution followed again.
- **Workspace identity**: a workspace has one name, the one its engine
  sees. tmux starts the pane in the workspace (`-c`), the kernel resolves
  every symlink on the way, and Claude Code files its transcript under the
  resolved working directory (every character but ASCII letters and digits
  replaced by `-`). So the workspace root is resolved through its symlinks
  once, where it is configured (`spawn_host::canonical_root`, from
  spawn-cli's `workspace_root` and in `GitWorkspace::new`, the store that
  mints workspace ids), and the mount, the pane's working directory, the
  workspace id, the runner's exit file (`.spawn/exit-<execution>`) and the
  transcript path all derive from that one id. A root reached through a
  symlink (the reporter's `/home` -> `/data00/home`; on macOS `/tmp` and
  `/var` are links to `/private/...`) used to give each workspace two
  names, and the Bridge watched a transcript no engine wrote: no turn read,
  no report collected. Everything else that names a workspace by path
  (main merged into a workspace, set-aside, report collection, the
  delivery marks, the mirrors under `.sources/`) builds it from the same
  configured root, so the names agree. **Records from before**: an
  `execution.started` or a mount step written by an earlier version names
  the workspace under the root as configured. On a host whose root is no
  link the two are the same string and nothing changes. On a host whose
  root is reached through a link, `WorkspaceStore::canonical` maps such an
  id onto the resolved root (its old root, the id's path minus the branch,
  resolves to the store's root), the Bridge reads every execution's
  workspace through it (so an execution started before the upgrade is
  followed where its engine writes, its transcript read at the first tick
  after the restart), and the git store's path check accepts it for a
  checkpoint, a merge or an unmount.
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
  rejected ("the worker has no branch to land"). **A report is judged
  against the checkpoint of the turn that produced it.** The dispatcher
  collects a report only once the session's latest turn event in the log is
  a turn's end (`turn.stopped` or `turn.failed`, not a newer
  `message.received`; a ready screen alone is not enough), with the worker
  live, no command open and its latest judgement `TurnStoppedIdle`. A
  judgement with no model answer (no model judge, or the judge unavailable)
  may be older than the turn's end, since the Observer writes it again only
  when its verdict changes; a model's answer is written for every new
  situation, so a stale one waits for the model's judgement of the turn. A
  report that is still a draft is not taken at all (a template placeholder,
  a line that is only `(pending)`, `TBD` or `TODO`, an Outcome that says
  draft or in progress): the file stays, the task stays delivered, and the
  worker is told once per content (`task.report_unfinished`,
  [fleet-architecture.md](fleet-architecture.md) §10.1). Its `task.reported`
  record names in `commit` the agent's latest checkpoint read with that
  turn's end (`spawn_task_reported(task, report, commit)`; null only when
  the agent has no checkpoint at all, and from the two-argument form). The
  pushed head must also be that commit or a descendant of it, or the task
  is gated `stale` as above; a report that names a commit is never judged
  for lack of a checkpoint (the branch pushed at the mount is only the
  task's base). A report with a null commit is judged by the rule above
  alone. So only a source that is a
  directory on this host has its agents' work landed: a gate on any other
  source is refused when it is configured, and a mission gated on one
  before that is halted by the integrator. The
  rest is unchanged: a branch behind a moved `main` lands as the merge by
  `main`'s merge rules; one that conflicts with it is gated `conflict` after
  `main` is merged into its workspace (above); one whose merge is clean as
  text but fails the gate, while the head alone passes it against its
  merge-base with `main`, is gated `incompatible` and sent back the same
  way, not counted as a rejection (fleet-architecture.md §10.3); one whose
  check-tests.sh failures are all the host's (connection errors to the test
  database, or database errors while the test database's log,
  `SPAWN_TEST_DATABASE_LOG`, shows a crash during the gate) is gated
  `host_fault`: no verdict on the work, never counted, the task left where it
  was and judged again after a backoff from the record's `due_ms`, two in a
  row escalated to the supervisor; or one whose own base disagreed with
  itself (a scenario of it exited 1 and then 0, before any candidate code
  ran) is gated `host_fault` with the evidence `base_disagrees`, never
  counted, judged again after the same run's backoff and alerted at three in
  a row in the record itself (fleet-architecture.md §10.3); marker blocks in the files
  the branch changes are gated `conflict`, a pending merge of `main` (it lives with the workspace
  directory, on the Bridge's host) blocks judging when that directory is on
  the integrator's host, and its markers, once checkpointed and pushed, are
  caught by the marker check wherever the integrator runs. The gate judges
  the candidate; the integrator moves `main` by a compare-and-swap push. A
  branch head an earlier version wrote with a `Spawn-Large` trailer lands
  without it: the landing commit's message is the integrator's.

  **A landing is one commit per task on `main`'s first-parent line.** Every
  landing, also of a branch that already contains `main`, moves `main` by
  exactly one new merge commit C, never by a fast-forward to a worker's
  commit: first parent the `main` the gate judged against (the task's base),
  second parent the pushed head that was judged. C's tree is the head's when
  `main` is its ancestor, and the merge by `main`'s merge rules otherwise.
  C is built before the gate runs: the gate judges it, `task.gated` names it
  in `candidate`, the push moves `main` to exactly it, and `task.landed`
  names it in `to`. Its message is the integrator's, written from the record
  alone (never from a workspace), and says what landed: the first line is
  the objective's title (the brief's first line) with the objective and the
  task, `Greet the harbour (objective 7, task 42)`, or, for a task without
  an objective, the brief's first non-empty line and `(task <id>)`; after a
  blank line, the report's `## What changed` section (the whole report when
  it has none), at most 3000 characters, with any line of it that could
  read as a `Spawn-` trailer indented; last, the trailers `Spawn-Task`,
  `Spawn-Objective` and `Spawn-Head` (the judged head). Author and committer
  are `spawn-integrate`, unsigned. So `git log --first-parent main` reads one
  line per landed task, `git bisect start --first-parent` (git 2.29+) visits
  only landings, each one gated (a plain bisect may stop on an ungated
  checkpoint: the price of keeping the history), and `git revert -m 1 <C>`
  undoes exactly one task. The worker's checkpoints stay reachable from
  `main` through the second parents, off that line, so a reverted objective
  comes back as a new task from the current `main` (or by reverting the
  revert), never by merging the same branch again. It is a merge and not a
  squash so that no other ref has to keep the history, and so that the
  worker's head is in `main`: the next merge of `main` into its branch is
  clean, where after a squash every later merge would see the same change
  from both sides.

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

## The engine's runtime files

The engine writes files for itself in its working directory, the
workspace: Claude Code keeps its session's scheduler lock in
`.claude/scheduled_tasks.lock`, its worktrees under `.claude/worktrees/`,
and more. They are the engine's state, not the project's, and a checkpoint
that took them landed them in `main` (Supply PA's `main` got
`.claude/scheduled_tasks.lock` from `spawn checkpoint exec_212`, 5a386e2).

- **The list** is the engine's own (`ENGINE_RUNTIME_FILES` in
  `spawn_host::git`): the paths Claude Code (2.1.318) writes to a
  checkout's `.git/info/exclude` under `# claude-code-runtime`, at any
  depth: `.claude/scheduled_tasks.lock`, `.claude/scheduled_tasks.json`,
  `.claude/routines/.state/`, `.claude/worktrees/`, `.claude/checkpoints/`,
  `.claude/mailbox/`, `.claude/agent-registry.json`,
  `.claude/agent-memory-local`, `.claude/first-run`,
  `.claude/assistant-daemon-state.json`; and `.claude/settings.local.json`,
  the local permissions Claude Code writes when a person allows a tool,
  which a project never commits. A workspace has no `.git`, so
  Claude Code writes those rules nowhere there. The live workspaces held
  only the lock when the rule came in. A new engine version that writes
  another file means a new entry. `.claude/scheduled_tasks.json` holds the
  engine's durable schedule (`CronCreate` with `durable`), so such a
  schedule lasts only as long as the workspace stays mounted.
- **Never added.** Every staging (`git add -A`) excludes them by pathspec,
  so no project rule decides it: a project with no `.gitignore` entry for
  them, or one that re-includes one, commits none. Nothing is written into
  the mirror or the workspace for it, so a workspace mounted before this
  rule is covered at its next checkpoint, its kept index included (the
  index's entries for them are reset from the checkpoint's parent, never
  trusted). The rest of `.claude/` is the project's and is committed as
  usual: `.claude/settings.json`, agents, commands, skills, and a file
  that only looks like one (`.claude/worktrees.md`). The files stay in
  the workspace (the engine holds them) and go with it at the unmount.
- **A runtime file the project tracks**, one the branch and `main` both
  hold, is committed like any tracked file, as git treats a tracked file
  that an ignore rule names: the engine's edits to it, and its deletion.
  Spawn never removes one from `main`: the owner does (`git rm --cached`
  and an ignore rule, as for Supply PA, 37641e9). While a merge of `main`
  waits on its markers, the branch counts with what that `main` changed
  since the merge base, so a runtime file `main` added in the merge stays
  and one it removed stays out.
- **One only the branch holds** leaves the branch at its next checkpoint:
  one a checkpoint from before this rule committed (`main` never had it),
  or one the owner has since removed from `main`. The landing then changes
  nothing in `main` for it and does not conflict on it (the owner's
  removal against the engine's edits would be a modify/delete conflict).
  The engine's file stays in the workspace.
- **Cost.** One more git call per checkpoint: a listing
  (`ls-files --with-tree=<parent>`) that finds neither the parent nor the
  index holding a runtime file, after which nothing else runs (the reset
  and the rest run only when one is held, or a merge waits on markers);
  and 16 exclude pathspecs on the staging and the size check. Measured on
  this repository's tree (321 files, release build, the store's own
  `commit`): 17–18 ms per checkpoint instead of 13–15 ms with nothing
  changed, 21–22 ms instead of 17–18 ms with one file edited.

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
  exits 5 with a no-room fact before it makes any scratch
  (`scripts/disk.sh`). The integrator records it as `no_room`, which is
  never a verdict on the candidate and never counts toward a halt. If no
  room is left even for the fact, the integrator measures `TMPDIR` itself.
  It tries again with a backoff, and after 2 hours it tells the supervisor
  once (fleet-architecture.md §10.2, §10.3). The integrator passes the variable
  through. The gate's scratch is
  `$TMPDIR/spawn-gate.XXXXXX`, so cleanup can tell it from everyone else's.
  The integrator runs one gate at a time that uses the shared test server
  (`--gates`, more only with a test server per gate), and beside it the
  gates of missions marked `no-test-database`; each gate checks the floor
  at its own start. Measured
  2026-11-06, a Spawn gate's scratch peaks near 1.7 GiB, two at once
  3.4 GiB, well inside the default floor. Routine cleanup removes only scratch untouched for a day, which no
  running gate (its wall clock, `SPAWN_GATE_WALL_CLOCK_MINUTES`, 120 minutes by default, at most) is.
- **The loop watches free space.** Once a minute `spawn loop` reads the free
  space of the workspace root and `TMPDIR` (`df`). A path that falls under
  `SPAWN_DISK_WARN_GB` (default 20) GiB is recorded once as a control record
  `host.disk {label, path, state: "low", free_bytes, warn_bytes,
  floor_bytes}`, and once as `state: "recovered"` when it is back 1 GiB above
  the threshold (so a path at the threshold does not flap). The latest
  record per path is read back from the log on start, so a restart records
  nothing twice. A path's latest record reaches the supervisor's digest
  once: a `low` one ("disk space on this host") is a condition, kept in the
  dispatcher's plan (and the attention queue) as told and unresolved until
  the path's `recovered` record replaces it, however long that takes; a
  `recovered` one of the last day ("disk space back on this host") is news,
  resolved by being told. The fleet view's health carries `disk`:
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

## Memory room

Memory is the other resource every execution shares, and on this host it is
bounded the same way disk is (objective 029; design.md §11, "Resources"):
nothing an agent's build, test or tool does may take the host's memory, its
swap (zram, that is RAM) or its CPU from the rest of the fleet or from the
owner's work.

- **Every execution and every gate runs in a systemd user scope of its own**,
  directly inside Spawn's slice (`SPAWN_SLICE`, `spawn.slice`): an execution
  in `<slice stem>-<execution id>.scope`, a gate in
  `<slice stem>-gate-<task id>-<n>.scope`. The engine, its children and
  everything they start are in that cgroup; the scope carries
  `SPAWN_EXECUTION_MEMORY_HIGH` / `_MAX` / `_SWAP_MAX` (10G, 12G and 4G by
  default; a gate the same with `SPAWN_GATE_*`) and `OOMPolicy=stop`, so a
  process killed at the hard limit ends that scope and nothing else. The
  slice carries `SPAWN_SLICE_MEMORY_HIGH` / `_MAX` / `_CPU_WEIGHT` (16G, 20G
  and 50: Spawn yields to the host's other jobs, weight 100, under
  contention and uses an idle CPU otherwise), so Spawn's memory is at most
  20 GiB and throttled above 16 GiB whatever it runs. Each envelope caps its
  swap: a cap without one is no cap on a host whose swap is compressed RAM.
- **A kill by the envelope costs one execution.** A memory-limit kill
  (`memory.events` `oom_kill`, or the unit's `Result` `oom-kill`, read at the
  end, never an exit status alone) ends that execution only: its final
  checkpoint and its end run as for any end, no checkpointed work is lost,
  and the record is `execution.interrupted` with cause `memory_limit`, path
  `resume` (objective 028's rules; the session is resumed, not replaced). A
  scope leaves no unit behind once its end is recorded.
- **Admission waits for room.** An execution or a gate starts only when its
  envelope fits under the slice (the slice's `anon` + `shmem` at most its
  MemoryMax minus the envelope's) and on the host (`MemAvailable` at least
  the envelope's MemoryMax). Otherwise it waits, recorded once as
  `admission.waiting` with the measure and the threshold, and starts when it
  fits (`admission.admitted`, with `waited_ms`); nothing fails and nothing is
  lost. With the default numbers that is 8 GiB of the slice's room and
  12 GiB of the host's per execution or gate.
- **The loop records the envelope once** (`host.envelope`, the slice and
  every setting in bytes) and again only when a setting changes; a host
  without a reachable systemd user manager (the gate's sandbox is one) runs
  as before and records state `unavailable`. The fleet view shows each live
  execution's scope — its memory now, its peak, its limits, its swap and how
  often it was throttled — and the slice's.

Not guarded: the page cache an execution's build leaves behind (reclaimed
under the limit, and not counted by admission), the owner's own processes,
and the rest of the host outside Spawn's slice.

Not guarded: the kache compiler cache (`~/.cache/kache`, a derived cache
outside Spawn's cleanup), the mirrors under `<root>/.sources`, which hold
branches, and the transcript archive under `<root>/.transcripts`, which
holds the agents' history; none of them is pruned automatically.

## Alerts

What went wrong is recorded, not printed into a pane
([fleet-architecture.md](fleet-architecture.md) §3.4). Each alert transition
is a control record in the stream, in the same shape as everything else:

- `alert.fired {key, kind, subject, evidence, text, notice?}` — one
  occurrence. `key` names it (a task's state change, a halt's record, a
  crash line's stamp and pid, a reading's time, a backup's record, and so
  on); `evidence` holds the record's `event_id` or the reading's measure and
  threshold. A notice (`notice: true`) is news and never clears.
- `alert.cleared {key, kind, subject, evidence}` — what cleared it, with the
  clearing record's `event_id` (or the log line, or the reading).

The records are written by the loop through `spawn_alert_record` (owner
only) and folded into `spawn_alerts`, one row per key: `key` (primary key),
`kind`, `subject`, `state` (`open`, `notice` or `cleared`), `notice`,
`text`, `evidence`, `fired_event_id`, `fired_at_ms`, and `cleared_event_id`,
`cleared_at_ms`, `cleared_evidence`. The fold — and the writer — refuses a
second `alert.fired` for a key, an `alert.cleared` without its `alert.fired`
and a second `alert.cleared`, so an alert fires once per occurrence and
clears at most once, across restarts and two loops at once. The loop reads
the keys back on start and fires nothing twice; `spawn alerts --once`,
`spawn alerts --follow` and `/api/alerts` read the table, so the fleet, the
supervisor and the dashboard see the same alerts (`spawn_reader` and the
supervisor's login may read it; only the owner writes it).

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
| Event log backups | `spawn backup dump`, every hour: an encrypted `pg_dump` in R2 bucket `spawn-store` under `postgres/hourly/` (6 h), plus the day's first under `postgres/daily/` (7 d) and the ISO week's first under `postgres/weekly/` (28 d), bucket-locked against deletion and expired by R2 lifecycle; a copy the plausibility check doubts goes to `postgres/suspect/` (30 d) ([deploy/backup](../deploy/backup/README.md)) | They are the copy; the key is kept off-host |
| What an engine did | read by the Bridge from transcript and pane into the log | Yes: in the log |
| Full transcript text | `~/.claude/projects/...` on the compute host, copied to `<root>/.transcripts/<agent>/` at turn ends and execution ends; the last 14 days leave the host in the daily bundle below | From the archive, on the same host (restored before a resume when missing); from `bundles/` on another host |
| Code: mission sources | bare repos (`missions/*.git`) or remote URLs | Remote URLs: yes |
| Code: agents' work | per-agent branches in host-local mirrors (`ws/.sources/*.git`), pushed as `spawn/<agent>` to a source that is a directory on this host (a remote source receives none) | Yes for missions with a directory source; no for remote sources, scratch missions and the default repository |
| Work a task lands | the source's `spawn/<agent>`, fetched by the integrator into its mirror's `refs/spawn-integrate/<agent>`; judged only when it contains the agent's latest checkpoint (its `artifact.committed` in the event log) | Yes: the source holds it; nothing on the host that ran the agent is needed |
| Large files in workspaces | git blobs on the agent's branch, up to `SPAWN_MAX_FILE_MB`; larger ones only in the workspace | Up to the cap, as code; **no** over it |
| A mission's settings (gate, accept, budget, objectives) | `mission_missions`, the fold of its latest `mission.configured`, which holds the whole configuration: `spawn mission configure` merges what it is given into the row read under the append lock and records the result, clearing only on its `--no-<setting>` switches; clearing the gate, or moving accept away from gate, is refused while a task of the mission is `landing` (nothing would land it: GitHub issue #9) ([fleet-architecture.md](fleet-architecture.md) §10.3) | Yes: from the log |
| Backlog, scenarios, gate | files in the controller's checkout, by absolute path | **No** on another host (versioning them with the source is not built) |
| Secrets | `spawn.env`, `~/.config/spawn/*`, mode 600 | Deliberately not stored |

### What leaves the host

`spawn backup` (crates/spawn-cli/src/backup.rs) is the binary's part; the
timers, the bucket's lifecycle and lock rules and the README are the owner's
(deploy/backup). Each run records what it did, through the owner-only
`spawn_backup_record`:

- `backup.dumped {keys, tiers, skipped, sha256, bytes, stored_bytes, events,
  max_event_id, held_bytes, written_bytes, alert_bytes, limit_bytes,
  over_alert, suspect?, dump_ms, upload_ms}` — one `pg_dump -Fc` of the
  database `spawn backup` records into, its `spawn_events` rows and highest
  id counted in the archive itself, encrypted (AES256) and decrypted back to
  compare its sha256. A dump holding fewer events than the newest good one,
  or under half its size, is suspect and goes to `postgres/suspect/` only
  (exit 3). The tiers come from the earlier records: hourly always, daily
  the UTC day's first good dump, weekly the ISO week's first.
- `backup.bundled {kind, base_event_id, keys, tiers, skipped, sha256, bytes,
  stored_bytes, repos, missing, transcripts, held_bytes, written_bytes,
  alert_bytes, limit_bytes, over_alert, duration_ms}` — one encrypted tar
  holding `manifest.json`, a git bundle of every mission source's branches
  and tags (whole, or `--not` its base's heads; a source with none is
  recorded `empty`, an increment records its `delta` (added, moved, removed
  refs) and the `bundled` refs its bundle holds, and one that needs no bytes
  has no file) and the transcript archive's files of the last
  `SPAWN_BACKUP_TRANSCRIPT_DAYS` (14) days, whole or the bytes appended since
  its base's recorded sizes.
- `backup.verified {ok, reason, key, sha256, dumped_event_id, expected,
  events, max_event_id, database, server, bundle, duration_ms}` — the restore
  drill: the newest good dump fetched, decrypted, restored into a database of
  its own on `SPAWN_BACKUP_DRILL_DATABASE_URL` (never the source), its events
  and watermark compared with that record's (never with the live database),
  the database dropped, and the newest bundle fetched, unpacked and compared
  with its own record (`bundle`: `checked`, the key, kind, repos, verified
  alone, transcripts). Exit 0 both pass; 1 either does not; 2 cannot run.

Nothing is written past `SPAWN_BACKUP_R2_LIMIT_MB` (10,000,000,000 bytes,
the free tier in decimal GB): every run lists the whole bucket under
`<root>/.backup.lock` and skips any copy that would pass it, the hourly one
included (a dump whose hourly copy was skipped exits 4, so its timer unit
fails). `SPAWN_BACKUP_R2_ALERT_MB` (8,000,000,000) is the owner's alert: the
record says `over_alert` and the run says it on its output; objective 030's
`backup_budget` alert is raised from that record. The passphrase
(`SPAWN_BACKUP_KEY_FILE`) and the R2 credentials (`SPAWN_R2_ENV_FILE`) are
mode-600 files read by path and on stdin, never on a command line, in the
output or in a record. A host restored from these: the log from a dump, the
sources and the transcript window from the newest full bundle and the
increments after it.

## Where the one store runs

This host plus Cloudflare R2. PostgreSQL stays here and its backups go to an
R2 bucket (built). Other hosts are meant to reach PostgreSQL through a
Cloudflare Tunnel + Access (not built). The ways to reach one store, and their trade-offs:

| Option | What it takes | Trade-off |
| --- | --- | --- |
| Private network (Tailscale/WireGuard) to this host | install it on each host | no TLS work; this host stays the single point of failure |
| Managed PostgreSQL + S3-compatible bucket (e.g. Neon/Supabase + R2) | TLS client; move the data once | off-host durability and backups; a bill and a provider |
| Cloudflare Tunnel + Access in front of this host (the one chosen) | `cloudflared` on each client | reachable from anywhere without an open port |
