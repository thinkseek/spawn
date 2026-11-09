# Cross-host Agent session recovery

Status: proposal. This document does not describe a working recovery feature.

## 1. Objective and timing boundary

Recover one Agent's logical Session on a different, same-platform Linux host
that initially contains no Spawn installation, configuration, workspace, or
cache. After recovery, the Session must retain its event history, native
conversation, workspace, task state, and Observer history, and must accept new
work under the same `AgentId` and `SessionId`.

Four intervals are measured separately:

| Interval | Start | Stop | Objective |
| --- | --- | --- | --- |
| `bootstrap_ms` | An empty Linux VM or container starts | The pinned recovery image and generic credentials are ready | Measure; no 10 s promise |
| `restore_ms` | Immediately before the first Session-specific recovery operation | The restored engine is ready to accept input under the resumed native Session | **10,000 ms maximum, unverified** |
| `failover_ms` | The old host fails | The restored engine is ready | Measure detection, fencing, database promotion, bootstrap, and restore |
| `total_ms` | An empty Linux VM or container starts | The restored engine is ready | Measure directly with one monotonic clock, including every gap |

Bootstrap ends before Session-specific recovery starts. It may install the
generic runtime and credentials, but it may not create a recovery operation,
acquire Session ownership, select a manifest, prefetch checkpoint objects,
write recovery destinations, fold the event log, or start Spawn or an engine.
`restore_ms` starts immediately before the first durable-operation, database,
ownership, manifest, or object-store operation in the restore protocol. Only
an explicitly identified idle operator-scheduling gap may remain outside it.
`total_ms` includes every gap; each gap is also reported separately.

A literal empty machine has no container runtime, image, packages, or Spawn
data. Operating-system boot, runtime installation, image download, and data
transfer have no fixed 10-second bound. The 10-second objective therefore
applies after bootstrap. A claim that the whole `total_ms` is always at most 10
seconds would require a prebuilt machine image or warm infrastructure, which
is a different deployment assumption.

Docker is packaging, not process recovery. The proposed path uses the existing
native `--resume` mechanism. It does not restore process memory, a tmux pane,
an in-flight HTTP request, or an interrupted model turn. Those are different
objectives; CRIU or a virtual-machine snapshot would be needed to attempt
them, and would still not make external tool side effects exactly once.

"Complete" means the last complete, cross-store recovery checkpoint. It does
not mean the instant of an arbitrary crash. The recovery report states three
positions independently:

- **Event RPO:** zero through the manifest's confirmed `event_position`.
- **Workspace RPO:** the manifest's verified commit and overlay.
- **Transcript RPO:** the manifest's verified archive copies, lengths, hashes,
  and per-Execution cursors.

Any later affected event, control, or command facts must be replayable from
those artifacts or be classified as derived, read-only observations. Otherwise the result is a
degraded restore, not a complete one. An interrupted input is at-most-once:
when the log shows `typing` without `typed`, recovery must not blindly type it
again. External effects that may already have happened are listed as
uncertain and reconciled by their own idempotency keys or by a person.

### Required surviving services

Recovery from total host loss needs state outside that host. The design
assumes that these services and their credentials survive:

- a PostgreSQL database reachable over TLS from the replacement host;
- the private R2 bucket used for immutable recovery objects;
- a secret store holding the database, object-storage, engine, and judge
  credentials, plus the recovery encryption key;
- the OCI registry holding the pinned Spawn recovery image.

If all copies of the event log, artifacts, or encryption key are lost, no
recovery protocol can reconstruct them.

## 2. What exists today

Spawn already has local recovery and disaster-recovery primitives:

- PostgreSQL is the fact log. Bridge and Observer fold it again after a
  restart; the dispatcher and integrator derive their state from it
  (`docs/upgrading.md:19-35`).
- `Resume` keeps the logical Session and native session id, but starts a new
  Execution (`docs/design.md:126-140`). An interrupted engine turn is not
  resumed (`docs/upgrading.md:47-56`).
- A turn-end workspace checkpoint is committed before the turn-end event is
  recorded (`docs/storage.md:57-69`). Command records make retries idempotent,
  including the uncertain interval after typing input
  (`docs/design.md:161-167`).
- The transcript archive is append-only, checks for rewritten source files,
  is made durable with `fdatasync`, and restores a missing native transcript
  before `--resume` (`docs/storage.md:491-527`).
- `spawn backup dump` writes an encrypted PostgreSQL dump every hour.
  `spawn backup bundle` writes mission-source git bundles and the last 14 days
  of transcript archives every day. `spawn backup verify` restores the latest
  dump and checks its event count/watermark; it opens the newest bundle and
  checks its recorded contents, but does not restore and validate the complete
  full-plus-incremental bundle chain (`deploy/backup/README.md:3-22`,
  `docs/storage.md:648-694`, `crates/spawn-cli/src/backup.rs:2225-2235`,
  `crates/spawn-cli/src/backup.rs:2323-2328`).
- The local recovery loop classifies an interruption and chooses `resume`,
  `handoff`, `wait`, or `final` from recorded facts. Its idempotent commands
  recover executions on the existing host; they do not reconstruct a host or
  select a Session-consistent off-host checkpoint
  (`crates/spawn-cli/src/recover.rs:1-48`).

Those primitives do not yet provide host-loss recovery:

| State | Current durability gap |
| --- | --- |
| Event log | The off-host copy is an encrypted `pg_dump` scheduled hourly with up to five minutes of jitter. Delays, failed retries, and budget-skipped uploads leave no fixed RPO bound. Daily verification proves dump restoration, not RPO zero (`deploy/backup/README.md:3-22`, `deploy/systemd/spawn-backup.timer:5-9`, `deploy/systemd/spawn-backup.service:20-27`, `docs/storage.md:634-637`). |
| Full native transcript | The live archive is host-local. Daily bundles include only the last 14 days, asynchronously and independently of the database cutoff; they are not a synchronous or Session-consistent transcript checkpoint (`docs/storage.md:491-527`, `docs/storage.md:648-681`). |
| Agent branches | Remote-source, scratch, and default-repository work can exist only in host-local mirrors (`docs/storage.md:640-642`). |
| Workspace contents | Files over the checkpoint cap are not preserved; `.spawn/` and engine runtime files are outside git checkpoints (`docs/storage.md:35-39`, `docs/storage.md:400-489`). |
| Mission configuration | Backlog, scenarios, and gate paths can be absolute host paths and are not reconstructed on another host (`docs/storage.md:619-630`, `docs/storage.md:644-646`). |
| Transcript ingestion | `settle` can end a lost Execution without first draining its transcript to EOF; its final checkpoint precedes terminal termination and transcript archival. Archive rewrite detection compares only a 4 KiB overlap; the Execution cursor has no copy generation; and transcript restore accepts an existing file without proving that it is not a truncated archive prefix (`crates/spawn-core/src/bridge.rs:2261-2297`, `crates/spawn-core/src/bridge.rs:1838-1875`, `crates/spawn-core/src/transcripts.rs:24-142`). These correctness gaps must be fixed before a complete-restore claim. |
| Targeted recovery | Normal runtime folds the full log, republishes branches, and uses the fleet-wide command claim; Observer and the local recovery loop can also act (`crates/spawn-cli/src/runtime.rs:21-73`, `crates/spawn-cli/src/recover.rs:1-48`, `crates/spawn-postgres/src/command_queue.rs:15-18`). A Session restore needs an isolated mode. |
| Secrets | `spawn.env`, engine authentication, and `~/.config/spawn/*` deliberately have no central copy (`docs/storage.md:646`). |

Consequently, restoring today's hourly database dump and daily bundle on a
new host is neither a complete Session restore nor an RPO-zero procedure.

## 3. Recovery data contract

A recovery checkpoint is an immutable, encrypted object set. It is identified
by `(agent_id, session_id, checkpoint_seq)` and written under:

```text
recovery/<agent-id>/<session-id>/<checkpoint-seq>/
  manifest.json
  workspace.gitbundle
  config.json
recovery/objects/<sha256>
  # content-addressed overlay, Spawn-state, controller, dependency, and
  # transcript chunks
```

The manifest is written last. Its presence closes the checkpoint; a partial
prefix without a valid manifest is ignored. Objects are never overwritten.
Every manifest independently enumerates the complete object closure needed to
rebuild that checkpoint. Each logical file and transcript copy records either
one `{object, length, sha256}` descriptor or an ordered list of
`{object, offset, length, sha256}` chunks. Uploading only a new chunk never
changes an earlier object.

A durable publication pin protects every uploaded or deduplicated object before
the manifest commits. The pin is bound to the publication ownership lease.
Retention protects the union of objects referenced by retained manifests and
objects covered by valid in-flight pins. A pin is released only after the
manifest write and generation CAS succeed. A stale pin is collectible only
after its publisher lease is proven expired. An object that merely exists but
has neither protection is not durable. The same closure rules protect every immutable secret object and every
recovery-encryption key version needed by a retained or in-flight checkpoint.
Rotatable service credentials are different: the manifest records the version
used at publication for audit, while restore resolves the currently authorized
version of the same stable reference and records that version. Rotation must
not delete a required encryption key, and old service credentials are not
reactivated merely because a manifest names them. The first implementation may
use a complete git bundle per checkpoint. If it
later uses bundle bases, the manifest must list the complete bundle chain and
retention must protect that chain. R2 object locking and lifecycle rules
provide immutability and retention, as they do for database backups.

### 3.1 Manifest

The manifest contains at least:

```json
{
  "format": 1,
  "agent_id": "agent_...",
  "session_id": "session_...",
  "last_execution_id": "exec_...",
  "checkpoint_seq": 42,
  "workspace_id": "<agent>:/<original absolute workspace path>",
  "workspace_root": "<original absolute Spawn root>",
  "claude_projects_root": "<original absolute Claude projects root>",
  "spawn_commit": "<full git commit>",
  "schema_sha256": "<schema.sql digest>",
  "config": {
    "object": "recovery/<agent-id>/<session-id>/42/config.json",
    "length": 4096,
    "sha256": "<digest>"
  },
  "event_position": 123456,
  "target_session_seq": 789,
  "command_cutoff": {
    "event_position": 123456,
    "open": [
      {
        "command_id": 123,
        "principal": "human",
        "idempotency_key": "<caller key>",
        "action": {"type": "input", "text": "<prompt>", "tags": []},
        "state": "pending",
        "lease_worker": null,
        "lease_attempt": 0,
        "lease_until_ms": null
      }
    ]
  },
  "wal_lsn": "<confirmed LSN when physical replication is used>",
  "transcript_from": {
    "copy": 0,
    "offset": 12345
  },
  "engine": {
    "name": "claude",
    "version": "<recorded engine version>",
    "native_session_id": "<uuid>"
  },
  "workspace": {
    "route": {
      "kind": "Default | Scratch | MissionSource",
      "source": "<original source string or null>",
      "repo_path": "<original absolute bare repository path>",
      "mirror_path": "<original absolute mission mirror path or null>"
    },
    "branch": "<agent branch>",
    "branch_commit": "<normal Agent branch head>",
    "commit": "<selected normal or stale workspace commit>",
    "stale": null,
    "git_bundle": {
      "object": "recovery/<agent-id>/<session-id>/42/workspace.gitbundle",
      "length": 123456,
      "sha256": "<digest>"
    },
    "overlay_sha256": "<digest>",
    "overlay_chunks": ["<complete ordered chunk descriptors>"],
    "spawn_state_sha256": "<digest>",
    "spawn_state_chunks": ["<complete ordered chunk descriptors>"],
    "external_dependencies": [
      {
        "link": "node_modules/.pnpm",
        "resolved_root": "<original absolute dependency tree>",
        "lockfile_sha256": "<digest>",
        "object": "recovery/objects/<sha256>",
        "length": 345678,
        "sha256": "<digest>"
      }
    ]
  },
  "controllers": [
    {
      "name": "spawn",
      "roles": ["global_gate"],
      "path": "<original absolute logical controller path>",
      "resolved_commit": "<full controller git commit>",
      "object": "recovery/objects/<sha256>",
      "length": 234567,
      "sha256": "<digest>",
      "dependencies": [
        {
          "lockfile_sha256": "<digest>",
          "object": "recovery/objects/<sha256>",
          "length": 345678,
          "sha256": "<digest>",
          "destination": "<original absolute dependency tree>"
        }
      ]
    }
  ],
  "transcripts": [
    {
      "copy": 0,
      "local_name": "<uuid>.jsonl",
      "length": 987654,
      "sha256": "<digest>",
      "chunks": [
        {"object": "recovery/objects/<sha256>", "offset": 0, "length": 987654, "sha256": "<digest>"}
      ]
    }
  ],
  "secret_refs": [
    {
      "ref": "spawn/claude-settings",
      "version": "<secret version>",
      "destination": "<original absolute settings path>",
      "mode": "0600",
      "consumer": "claude"
    },
    {
      "ref": "spawn/model-relay-env",
      "version": "<secret version>",
      "destination": "<original absolute relay env path>",
      "mode": "0600",
      "consumer": "spawn-model-relay"
    }
  ]
}
```

All hashes cover the plaintext objects before encryption. The restore command
verifies the complete object closure before changing the destination. Archive
member paths are relative and cannot contain `..`, absolute paths, or
symlinks. Destination roots and secret paths are separate absolute fields;
they must exactly match the original recorded paths in the first
implementation. A root mismatch is a refused restore, not an implicit path
migration.

### 3.2 Event and control state

Use an off-host PostgreSQL primary, or a synchronous replica that has
acknowledged the manifest's `event_position`, for zero loss of persisted
events. With physical replication, `synchronous_commit=on` must wait for the
synchronous standby to flush the required WAL; `remote_write` alone is not the
same durability guarantee. Promotion and client reconnection count in
`failover_ms`. An hourly dump remains a disaster-recovery backup, not the
online recovery source.

`event_position` is the `spawn_events.id` application coordinate, not a WAL
LSN; identity gaps are valid. `target_session_seq` separately bounds the
Session's own facts. Physical replication uses the WAL LSN for its durability
acknowledgement.

The same global event position bounds `session_control`, whose records are also
stored in `spawn_events`. The manifest records every open target command's id,
principal, idempotency key, action, raw database state, and complete lease
fields at that cutoff. `pending` and `finished` are raw states; claim
availability is derived, not stored as an `available` state
(`crates/spawn-postgres/sql/schema.sql:368-388`). During preflight, Spawn derives the
cutoff's task-to-mission-to-source dependency set and examines every later
control record and target command transition. A post-cutoff record affecting
that dependency set must be represented by the same artifacts and reconciled,
or the restore fails closed. Unclassifiable dependencies also fail closed.
Recovery compares the affected live mission projections and locks the target
command rows; it does not run the global destructive control refold. Producers
stay disabled until those projections and the queue are consistent with the
accepted cutoff.

The command inventory separately classifies `spawn-liveness` commands whose
keys begin `recover-<old-execution>-<attempt>`. The current local recovery loop
uses these keys for Input, Resume, handoff Stop, and handoff Start, but a
command action binds only the Session, not the intended old Execution
(`crates/spawn-cli/src/recover.rs:1785-1932`,
`crates/spawn-cli/src/recover.rs:1953-1962`,
`crates/spawn-core/src/command.rs:41-69`). An old recovery command left queued
could therefore act on the restored Execution. Every such command must be
resolved by step 7 or fail restore closed; none may remain for ordinary
producers after handoff. A malformed key or ambiguous execution association
also fails closed.

Before restore, Spawn verifies:

- the expected schema digest is compatible with the pinned binary, and a
  read-only transaction verifies the actual catalog, SQL functions, and
  grants rather than only the schema file;
- the pinned decoder can read every known-source event through the recovery
  cutoff, so an unknown event cannot fail halfway through a destructive
  restore;
- the database contains every event through `event_position`;
- the Agent/Session/native-session bindings match the manifest at the cutoff;
- every open command's principal, idempotency key, action, raw state, and lease
  fields are unchanged;
- no newer valid recovery checkpoint exists, under the same ownership and CAS
  protocol used by the publisher and restorer.

The recovery command never runs an implicit `init-db` or migration. A required
schema migration is a separate controlled roll-forward completed before the
timed restore.

If PostgreSQL itself must be restored from a dump, that operation belongs to
`bootstrap_ms`; the 10-second Session objective does not claim a bounded
cluster restore. The current dump and restore use `--no-owner --no-acl`, so a
fresh cluster does not reproduce the required roles and grants
(`crates/spawn-cli/src/backup.rs:1135-1158`,
`crates/spawn-cli/src/backup.rs:2753-2779`). Bootstrap must apply separately
versioned role, grant, and database provisioning before the timed restore and
its catalog/function/grant preflight. The daily drill's event count/watermark
check does not prove that access contract.

### 3.3 Workspace and source state

`workspace.gitbundle` contains the mission's required source refs, the Agent
branch, and every object reachable from the checkpoint commit. This includes
scratch missions and remote-source missions; restore does not depend on the
old host's mirror or on a public remote accepting private Agent branches.

The manifest distinguishes `Default`, `Scratch`, and `MissionSource` routing,
including an Agent that ran before its first task and must continue using the
default repository. It records the default bare repository, mission mirror,
and local origin paths. A local directory origin is either restored as a
verified bare-repository artifact at the same absolute path or declared as an
independent surviving service. The first implementation does not rewrite
source routes or mission configuration in PostgreSQL.

The current source parser accepts URL userinfo, so an accepted source string
can contain a password or token (`crates/spawn-cli/src/sources.rs:103-155`).
Recovery publication refuses such credential-bearing source URLs and query
parameters with `credential_in_source`. Before using this recovery feature,
the owner must move those credentials to the restored Git helper or SSH
configuration and record an explicit mission-source change. Scrubbing only the
manifest would leave the historical event log exposed and would change the
source identity that restore must verify. Non-secret SSH usernames remain part
of the source string.

The git checkpoint alone is not a complete workspace. The overlay preserves
every durable regular file that the git checkpoint omits, including:

- pending `.spawn/` reports and delivery state;
- ignored files not declared as derived caches;
- files over `SPAWN_MAX_FILE_MB`;
- durable engine state such as scheduled tasks.

The overlay records relative path, type, mode, size, `mtime_ns`, and SHA-256
for each file. Nanosecond mtimes are restored because report collection
compares report and delivery-mark times (`crates/spawn-cli/src/deliver.rs:1653-1680`).
For the union of the git tree and overlay, restore verifies path, type, mode,
and content hash. Original `mtime_ns` is required only for overlay entries;
git does not preserve tracked-file mtimes, so they are not a fidelity claim.
Cache directories are excluded only by an explicit versioned list.
Unknown special files fail checkpoint publication; they are not silently
dropped. Ephemeral locks, sockets, FIFOs, device nodes, build output, and
package caches are not restored. A project that requires such a file for
correct execution must declare how to recreate it. Materialized dependency
trees referenced by a workspace or controller are not treated as disposable
caches; their exact closure is defined below.

The Spawn-state object covers required state beside the workspace: kept git
indices, source mirrors, `.merge`, `.main-copies`, `.stale`, `.oversized`,
and `refs/spawn-stale/<agent>`. Each item is either restored or explicitly
rebuilt and validated. Mount inputs such as pending-merge and main-copy
records are installed before any mount. R2 is only the transfer and durability
layer. Before Spawn starts,
these objects are materialized onto a local filesystem that supports the
POSIX locks, hard links, atomic renames, directory syncs, and `fsync`
semantics used by the existing host adapters; object storage is not mounted
as a substitute.

For a normal workspace, `commit` equals `branch_commit` and is the checkpoint
recorded on the Agent branch. For a stale workspace, `stale` is
`{ref, commit, base_branch_commit}`, the selected `commit` is the stale snapshot,
and the bundle includes both that snapshot and the unchanged normal branch.
Automatic recovery verifies this closure but stops before Resume with
`stale_workspace_requires_reconciliation`. An ordinary mount would export the
normal branch and clear the stale marker, so it cannot preserve this state
(`crates/spawn-host/src/git.rs:1750-1865`). A
person or an explicit reconciliation command must resolve the stale snapshot,
publish a new non-stale checkpoint, and only then resume. Continuation must
never advance the normal Agent branch with stale work.

The `controllers` set carries every independently deployed tree needed by the
mission: its scenarios and backlog, the global gate script, and other declared
non-git dependencies. These can be different controllers at different commits
(`deploy/systemd/README.md:190-205`). Each entry records its name, roles,
logical absolute path, resolved immutable
commit, object, and dependency references. Existing absolute mission paths are
restored exactly. Path remapping requires an explicit new control record and is
outside the first implementation.

Every materialized external dependency reachable through a controller's
`.deps` or a workspace link such as `node_modules/.pnpm` is recorded with its
resolved root, lockfile hash, and exact object closure
(`deploy/controller.sh:217-259`, `deploy/systemd/README.md:206-231`). Restore
recreates the link at the captured lockfile hash, not the controller's newer
`current` tree.
A dependency tree may be rebuilt only when its resulting content digest matches
the manifest; otherwise the exact tree is an object in the closure. Generic
package managers may be installed during bootstrap, but Session-specific
dependency fetch, reconstruction, and verification are inside `restore_ms`.

This changes the meaning of "complete" from "the current git checkpoint" to
"all durable files, with explicit omissions". Until the overlay exists, the
implementation must report large or ignored non-cache files as recovery
coverage failures.

### 3.4 Native conversation and engine configuration

Every transcript archive copy is represented by its complete ordered chunk
list, total length, and hash. Before publication, archiving compares the full
existing prefix of the current native file with the latest archive copy, in
chunks or with a verified rolling digest. A shorter prefix or any differing
byte starts a new copy; the existing 4 KiB overlap check is insufficient.
Multiple copies are retained because a rewritten native transcript must never
overwrite an earlier copy. In Spawn's archive, copy 0 is named
`<uuid>.jsonl` and copy N above zero is named `<uuid>.<N>.jsonl`. All copies
are verified in a temporary directory and installed atomically. The copy
selected for Resume, normally the latest complete copy at the accepted cutoff,
is installed at the engine's canonical `<uuid>.jsonl` path regardless of its
archive copy number. Earlier copies remain in Spawn's archive for ingestion
and audit; Claude is never resumed from copy 0 merely because later copies
have suffixed archive names.

`transcript_from` records both the copy generation and byte offset at which the
old Execution started reading. Archive indexing records every later copy
transition. Settle drains the starting copy from its recorded offset to EOF,
then each subsequent copy from offset zero through the selected copy, using
native `source_event_id` for idempotent ingestion. A repeated source id must
decode to the identical event; a conflicting payload fails closed instead of
being silently discarded as a duplicate. An unknown or incomplete copy chain
refuses completion before `execution.ended`. The new Resume records its
selected copy generation and starting length for the next recovery.

The replacement reproduces the recorded absolute paths on the new VM or inside
its container, using bind mounts where necessary. This includes the workspace,
Claude projects, source, controller, and dependency roots. An existing host-path
Execution therefore does not need a pre-existing container-specific workspace
path. The restore does not rewrite Claude's escaped project path or the native
transcript. Migrating an Execution to a different canonical root requires a
separate path-mapping implementation; it is not assumed here. Spawn restores
the selected archive both to its own transcript directory and to the Claude
projects path derived from the recorded working directory. It then uses the recorded native session id with the
existing `Resume` path. It must not start a blank Session if the transcript is
absent or invalid. On a repeated restore, an existing native file is checked
against the archived length and digest; a strict archive prefix is completed,
and unrelated or rewritten content is refused.

The image pins the Spawn commit and Claude Code version. `config.json` holds
non-secret model names, relay settings, source settings, and path settings.
The secret inventory is generated from the effective service configuration,
not from the example above. It covers the database, R2/bootstrap, recovery
encryption, Claude settings, TypeSafe/Jev, model-relay upstream and proxy,
fencing-provider credentials, and every private Git source's SSH or HTTPS
credentials. Non-secret credential-helper configuration and SSH `known_hosts`
or pinned host keys are included in `config.json`. A remote source may instead
declare an independently surviving authentication service installed by
bootstrap. Every reference records its version, destination, mode, and
consuming service. Authentication is never embedded in the image, manifest,
event log, command line, or journal. Both default and cheap profiles require a
continuation check; restoring Claude settings alone does not restore an
independently configured relay environment.

## 4. Publication protocol

Publication is incremental, but every manifest describes a complete immutable
snapshot. A restore must not construct a backup after the old host has failed.

At a turn-stopped-idle boundary:

1. Acquire the publication side of Session recovery ownership and allocate its
   publication operation id and fencing epoch. In the same database critical section, block new target
   commands, target event appends, and control writes affecting the target's
   mission, except writes carrying the publisher epoch. Hold the
   workspace/branch lock and lock the target command rows. Quiesce the target's
   delivery, report, acceptance, gate, and landing work before choosing the
   cutoff. An already running target gate or landing must finish before the
   freeze, or publication aborts; a manifest cannot split its external source
   effects from its control records. Every submission, claim, append,
   integration write, and control-write function must enforce this admission
   fence; an application-level pause alone is insufficient.
2. Pause engine scheduling and freeze the target Execution's complete writable
   process group or cgroup, including scheduled and background children. The
   workspace ownership boundary must exclude outside writers; otherwise use an
   atomic filesystem snapshot. If neither guarantee exists, abort publication.
   Acquire every required controller lock in deterministic name order. For each
   controller, resolve its symlink once, validate `.controller`, and capture only
   that immutable commit tree. Record each controller's dependency reference and
   every restored workspace's resolved external dependency links and lockfile
   hashes; capture those exact trees rather than a newer `current` dependency.
3. Finish the existing workspace checkpoint. Compare the full transcript
   prefix, archive through EOF with `fdatasync`, and ingest the suffix. From
   the frozen state, record the normal branch head, any stale ref and snapshot,
   the complete overlay tree, Spawn-state, controller and dependency objects,
   transcript copy chain/cursors, secret versions, global `spawn_events.id`, target `session_seq`, every open
   command's principal, idempotency key, action, raw state and lease fields,
   and the affected control dependency set. Record the WAL LSN
   separately when synchronous replication is used.
4. Create the durable publication pin before uploading or deduplicating any
   object. Upload content-addressed chunks that are not already protected,
   plus the git bundle, hashed `config.json`, and other objects named by the
   manifest. The manifest still lists the complete closure, including old
   chunks, pinned immutable encryption-key versions, and audited service-secret
   versions.
5. Verify uploaded sizes, plaintext hashes, and the database's confirmed
   recovery barrier. Revalidate the file tree, transcript length, event and
   command cutoffs, controller commits, dependency digests, required
   encryption-key versions, and current service-credential accessibility. Any
   change to the frozen snapshot aborts publication instead of producing an
   incomplete checkpoint.
6. Write `manifest.json` last and compare-and-swap the Session's latest
   recovery generation. Release the publication pin only after that commit,
   then release the controller locks, process, workspace, and admission locks.

A checkpoint is recoverable only after step 6. Publication is all-or-nothing:
the existing archive warning alone is not a durable recovery guarantee. Any
upload, sync, or verification failure leaves the previous manifest as the
latest complete checkpoint and records the failed publication. That previous
checkpoint is usable for a complete restore only if the later event, control,
and command tail is represented or reconciled; publication failure never
licenses discarding execution-affecting facts.

### Manifest tail and recovery cutoff

The manifest's event position is a recovery cutoff for target-Session events,
related control records, and command-queue transitions. Isolated recovery folds
only through that cutoff and quarantines later affected facts; it does not fold
the database's newest head onto older files. Later events for unrelated
Sessions and missions remain outside the target restore. The restorer
establishes the database-enforced admission fence before recording these high
watermarks and holds it through handoff, so the tail cannot grow unnoticed
between preflight and activation.

Before input is accepted, each affected tail item must be handled in one of
four ways:

- it is already represented by the restored commit, native binding, or
  transcript bytes, so the cutoff can advance after verification;
- it is a completion fact for an uncertainty inside the cutoff: a `typed` for
  the same command and old Execution after its recorded `typing`, or the
  corresponding `message.received`. Isolated recovery verifies the command
  identity and event sequence, journals the reconciliation, and advances the
  accepted cutoff. This exception cannot admit a new action, skip an
  intervening execution-affecting fact, or resolve an unrelated command;
- it is a derived or read-only observation that can be regenerated without
  changing execution state;
- it affects execution, commands, source routing, native bindings, branches,
  or workspace state and is not represented by the artifacts. A complete
  restore refuses this tail and asks for a newer checkpoint or an explicit
  reconciliation. It never starts a blank native Session or guesses.

The current recovery operation's own committed journal is folded separately.
Its settle, Resume, checkpoint, mount, and native-binding facts are folded
through the operation's durable applied cutoff on retry. The immutable
manifest cutoff never changes. Any execution-affecting tail outside the
explicit reconciliation rules above still fails closed.

Unrecorded-commit rederivation is disabled until the restored branch head,
native binding, Execution, source route, and transcript match the cutoff. An
older restored branch must never be appended as a new checkpoint merely
because PostgreSQL contains newer facts.

A host crash during a turn loses that turn's unpublished work, as the current
cold engine restart already does. Publishing at tool checkpoints can reduce
that window later. No new `EventKind` is required for the manifest index; if
one is added later, every log reader must understand it before any writer
emits it (`docs/upgrading.md:63-70`).

## 5. Fencing and restore protocol

A database guard against two open Executions is necessary but insufficient:
a partitioned old host can continue running tools even when it cannot write
PostgreSQL. Automatic failover therefore requires persistent infrastructure
fencing and a database-enforced fencing epoch.

Every Session ownership acquisition allocates a durable, monotonically
increasing fencing epoch. Recovery-generation CAS checks manifest currency; it
does not fence a stale restorer. While a publication or recovery fence is
active, every command submission/claim/step, event or control append, Execution
activation, journal update, and producer enablement for the affected scope must
match `(operation_id, fencing_epoch, phase)` in the same database transaction.
A stale epoch fails before the write or launch. Before a new
restorer can take over, it must fence the previous replacement compute or
explicitly adopt its live Execution. Adoption is allowed only when the durable
journal proves that operation id, manifest digest, recovery generation,
ExecutionId, workspace head, native binding, and transcript copy/cursor all
match the selected restore. The takeover then binds that same Execution to the
new fencing epoch in one transaction. A different manifest, generation,
Execution, or incomplete journal always requires fencing the prior replacement;
it never permits cross-checkpoint adoption. A lost lease alone never authorizes a
second engine.

The restore operation is durable and keyed by `(session_id, manifest_digest)`.
It records the operation id, fencing epoch, recovery generation, phase,
designated Resume command id, new Execution id, applied cutoff, owned event
positions, checkpoint head, transcript copy/cursor, uncertainty inventory, and
continuation correlation ids. Each recovery event append and its journal
position/cutoff advance commit in one transaction. A retry first folds the
operation's committed journal and resumes from its recorded phase with the same
Resume command. It never treats those own writes as an unreconciled tail or
rewrites a workspace that has already become active. A `launching` phase is
committed before engine launch; after an uncertain launch, the successor adopts
or fences that compute before trying again.

Restore proceeds as follows:

1. Start `restore_ms`. Load or create the durable operation, allocate the
   fencing epoch, acquire Session recovery ownership, select the manifest under
   recovery-generation CAS, and lock the target command rows. In that same
   database critical section, reject new target submissions and affected event/
   control writes with `recovery_in_progress`, except this operation's writes.
   Hold this admission fence through handoff.
2. Resolve and read every selected secret reference before fencing or any
   destination write. Use retained immutable versions for encryption material
   and currently authorized versions for rotatable service credentials. This
   includes recovery-object decryption/access, database, fencing, engine,
   relay/upstream, judge, and private Git source credentials. Verify non-secret Git helper and host-key configuration. Missing or unreadable
   material returns `missing_recovery_credential`. Stage secrets only in a
   mode-700 temporary directory.
3. Verify the complete manifest closure, including `config.json`, actual
   database compatibility, event/control/command cutoffs, and all artifact
   hashes in a temporary directory. Classify the affected tail read-only under
   the admission fence, including the explicitly correlated completion
   reconciliation below; refuse any other unrepresented execution-affecting
   tail here. Require the original canonical workspace root, Claude projects
   root, source paths, controller paths, and external dependency roots. Build
   a persistent uncertainty inventory from events and transcript bytes through
   the cutoff. If the selected workspace is stale, return
   `stale_workspace_requires_reconciliation` after verification and before any
   provider fence, Resume, mount, marker removal, or active destination change.
   A corrupt or incomplete checkpoint therefore cannot disable a healthy old
   host.
4. Establish a persistent provider fence for the old compute and for any prior
   replacement that is not explicitly adopted under the identity checks below.
   Disable instance start/resume, isolate network and storage, revoke old
   credentials, and terminate old database sessions. A one-time powered-off
   observation is not a fence. Maintain these controls until the old instance
   is destroyed or wiped. Recheck the selected generation, cutoff, and
   admission fence after the provider fence. If fencing cannot be established,
   allow `--verify-only` but do not start an engine.
5. Restore mount inputs first: source refs, default/local origin repositories,
   the Agent branch, Spawn-state records, controller trees, exact external
   dependency trees and links, non-secret configuration, and secret files.
   Secret files are mode 600; destination directories are mode 700. Start each
   configured host-local model relay, judge relay, and outbound proxy from the
   pinned image with its restored environment, and require its health check.
   An independently surviving remote endpoint needs only its connectivity and
   authentication check. These service starts and checks are inside
   `restore_ms`; restoring relay environment files alone cannot make a
   loopback `SPAWN_MODEL_RELAY` or judge URL usable
   (`deploy/systemd/spawn-model-relay.service:5-17`,
   `crates/spawn-cli/src/runtime.rs:282-297`). Do not start ordinary Spawn
   producers. Use a recovery-specific mount at the original workspace path with
   `merge_main_at_start` disabled, apply the verified overlay, and restore the
   native transcript. Verify path, type, mode, and content hash for the git and
   overlay union, original mtimes for overlay entries, and every transcript
   copy before any engine starts. Keep the verified objects staged for the
   later Resume mount.
6. Start isolated, target-Session recovery. Fold through the manifest cutoff
   and this operation's applied cutoff; quarantine every other affected tail.
   Disable branch republish, orphan unmount, unrecorded-commit rederivation,
   merge, unrelated gate/land work, Observer action, and every unrelated
   command claim. Revoke old command leases atomically under the new epoch instead of waiting for their
   two-minute TTL, and claim only the operation's designated recovery command
   by id under the epoch guard. This direct claim does not finish, reorder, or
   type an older queued command.
7. Drain the old Execution through its recorded transcript copy/offset chain,
   ingest each suffix idempotently under the original Execution id, and update
   the uncertainty inventory after each ingest. A later `typed` or corresponding
   `message.received` can resolve uncertain input. A tool completion, verified
   idempotency-key result, or recorded human reconciliation can resolve an
   external effect. Persist each event, cursor, checkpoint head, and phase in
   the recovery journal, then finish the final checkpoint, transcript archive,
   and `execution.ended`. Resolve every old Execution's unfinished Input from
   its complete command-step and message fold, not from `waiting` alone. With
   no `typing`, finish it failed as untyped. With `typed`, or a corresponding
   `message.received` after `typing`, finish it succeeded under the existing
   `Typing::Done` rule. With `typing` but neither resolution, finish it failed
   with `input_outcome_unknown`, retain the uncertainty, and stop before a new
   Resume or engine launch; never record that Input as untyped or retry it
   blindly (`crates/spawn-core/src/bridge.rs:591-611`,
   `crates/spawn-core/src/bridge.rs:3219-3225`). `waiting` does not erase a
   later `typing` or `typed` step (`crates/spawn-core/src/bridge.rs:535-559`).
   Journal each operation-owned command outcome.

   Resolve every old execution-scoped `spawn-liveness`/`recover-*` command
   before the new Resume. If its full fold proves no Input typing, Execution
   start, stop, or other execution effect, finish it failed with
   `superseded_by_session_restore`; this includes an unclaimed recovery Input,
   Resume, handoff Stop, or handoff Start. If it already started an Execution,
   treat that compute as a prior replacement under the exact-match adoption or
   fencing rule and drain it before finishing the command. Input uncertainty
   follows the rule above. Other recorded effects use the existing
   command-specific idempotent outcome only after their Execution is drained;
   an uncertain stop or start effect blocks Resume. No old recovery action may
   remain queued for the restored Execution. Other unclaimed target commands
   stay queued and are
   never typed during isolated recovery. Any unresolved command effect or other
   uncertainty leaves the operation degraded and stops it before Resume.
8. Submit the operation's one recorded `Resume`. Recovery Resume mounts the
   final checkpoint head without merging a newer main, then reapplies the
   verified overlay **after that mount and before transcript validation,
   `execution.started`, or engine launch**. This hook is required because the
   ordinary mount replaces the whole workspace
   (`crates/spawn-core/src/bridge.rs:2360-2384`,
   `crates/spawn-host/src/git.rs:1214-1252`). Recheck the git/overlay content
   and modes, overlay mtimes, and all native transcript bytes after the overlay
   is applied. Record the new Execution's transcript copy and starting length.
   Defer the normal main merge until continuation has succeeded. Never reuse
   the old `ExecutionId` or tmux name.
9. The no-input Resume path must handle Claude's workspace-trust dialog without
   relying on prompt typing. Reuse the existing two-frame dialog detection and
   confirmation from `type_prompt`, but stage and submit no prompt
   (`crates/spawn-core/src/bridge.rs:3091-3169`). Require a live runner for the
   new Execution and two consecutive identical Ready observations of that
   Execution's pane. An empty-prompt Resume command finishing is not sufficient
   evidence of readiness (`crates/spawn-core/src/bridge.rs:2639-2663`). Stop
   `restore_ms` at the second Ready observation. Keep all ordinary producers,
   command claims, and the deferred main merge disabled.
10. Run one operation-scoped continuation Input with a unique command id,
    idempotency key, and correlation marker. Claim that command directly by id;
    older unclaimed commands remain queued and do not block it. No other command
    may be claimed or typed. Require its corresponding new `turn.stopped`, then
    run the existing report collection, acceptance, and applicable gate/land
    logic in a target-scoped continuation
    driver. Every such write carries the operation's epoch and phase; unrelated
    tasks, reports, gates, and landings remain disabled. The driver must use the
    existing source/workspace locks and must not bypass the database admission
    fence or race an ordinary integrator for the target. Require the accepted
    report revision and gate/land result when applicable. Apart from the old
    Execution's unfinished Inputs and old recovery commands resolved in step 7,
    existing target commands, reports, and pending merges remain unchanged.
    Report
    `continuation_ms` and enforce `continuation_timeout_ms`; model latency is
    not part of `restore_ms`.
11. After continuation succeeds and the uncertainty inventory is empty, perform
    the deferred main merge through the scoped control path. Recheck the fencing
    epoch, generation, and non-operation tail, then exit isolated mode, release
    the admission fence, and enable the dispatcher, integrator, Observer, and
    other control producers. Only now may unrelated queued commands run.

If the infrastructure fence later fails, record `fencing_lost`, stop new input
and the new engine, and reconcile uncertain effects. Detection cannot undo an
external write or prove that no overlap occurred during an actual fence
failure; the design does not claim an absolute two-writer guarantee in that
case. Automatic failover is allowed only while the preventive fence is in
force.

`spawn recover restore --verify-only` performs credential resolution, artifact
verification, and read-only database/cutoff checks without creating an
execution operation or provider fence. It changes no destination and starts
nothing.

A refusal before step 5 journals the refused phase, removes temporary secret
and artifact staging, leaves the active target filesystem unchanged, and
releases the Session ownership and database admission fence. Any persistent
old-host fence already established remains in force. After such a fence,
ordinary producers stay disabled until an explicit retry or operator decision;
if verification failed before fencing, the old host's existing producers and
credentials remain untouched. A failure in or
after step 5 is instead a resumable operation failure: it retains the durable
operation, ownership, admission fence, and verified destination staging for
retry. It must not be reported as a clean refusal with no destination change.

## 6. The 10-second budget is not yet supported by measurements

The current soak requires work to resume within 20 seconds and sets a 15-second
p90 bound (`rsi/scenarios/032.sh:60-78`). Its recorded same-host, fake-engine
measurements on an earlier commit include 2.6–3.1 seconds after an engine kill,
12.6–20.3 seconds after database recovery, and one 23.2-second memory-limit
path (`rsi/scenarios/032.sh:102-126`). These numbers are not evidence of a
real-Claude, cross-host restore, but they are the relevant baseline.

The proposed restore path also contains work whose cost is not constant:

- Bridge and Observer currently replay the full event log when the runtime is
  constructed (`crates/spawn-cli/src/runtime.rs:16-20`,
  `crates/spawn-cli/src/runtime.rs:57-73`).
- Workspace mount exports a git tree rather than attaching an already
  materialized workspace, and transcript verification scales with transcript
  bytes.
- The default Ready wait permits 40 attempts separated by 250 ms, or about
  9.75 seconds of sleeping before terminal-capture time is included
  (`crates/spawn-core/src/bridge.rs:221-288`,
  `crates/spawn-core/src/bridge.rs:3091-3115`).

Meeting 10 seconds therefore requires, as applicable, Bridge/Observer
snapshots plus tail replay, a pre-materialized workspace and mirror,
incremental hash verification, a compatible pre-migrated database, and a
resume readiness path with its own measured deadline. Failure detection and any
database promotion may precede Session-specific restore, but remain inside
`failover_ms`. Operation creation, credential preflight, fencing, old-Execution
settle, Resume, and Ready are inside both `restore_ms` and `failover_ms`; none
can be reclassified as a scheduling gap to make the number pass.

There is no data-size-independent 10-second guarantee. The drill publishes
input bytes, object counts, event count, transcript bytes, and per-stage
timings beside the result. The SLO applies only to the declared test envelope;
an oversized or slower run is a failed run, not a successful average hidden
by other runs.

## 7. Suggested implementation slices

Each slice must preserve current single-host behavior.

1. **Correctness prerequisites.** Drain old-Execution transcript output through
   its copy/cursor chain before settle/end; compare the full archived prefix;
   validate existing native transcripts against the archive. Add rewrite,
   truncation, unread-suffix, large-backlog, command-retry, and concurrent
   recovery tests. A missing branch or required artifact must fail closed.
2. **Coverage inventory and failing drill.** Add a restore test that captures
   today's missing transcript, scratch branch, pending report, ignored file,
   oversized file, and auxiliary Spawn state. It must fail until those classes
   are covered.
3. **Immutable artifact publisher.** Reuse the R2 request signing,
   encryption/decryption, hashing, and verification code in
   `crates/spawn-cli/src/backup.rs`, extracting shared helpers where needed,
   and the bucket lock/lifecycle configuration in `deploy/backup/`. Extract the
   existing git-bundle creation/head enumeration, incremental ref-delta,
   transcript append-slice, and bundle-manifest mechanics as well
   (`crates/spawn-cli/src/backup.rs:1645-1972`,
   `crates/spawn-cli/src/backup.rs:2000-2046`). Parameterize them for the
   checkpoint's exact refs and Session cutoff, not the daily 14-day selection
   policy. Deploy and verify separate `recovery/` object-lock rules and a manifest/pin-aware
   retention worker; the fixed 14-day `bundles/` lifecycle must not expire
   recovery objects. Add the database admission fence, process/controller
   freeze, publication and encryption-key pins, event/control/command cutoffs,
   and complete content-addressed closure. Reject credential-bearing source
   URLs with `credential_in_source`; test that rejection before any object
   upload. Write and CAS the manifest last.
4. **Workspace, controller, and dependency state.** Add the explicit cache
   exclusions and complete file manifests. Capture every required controller,
   each workspace's resolved external dependency links and exact dependency
   trees, and normal/stale branch heads separately. Refuse automatic Resume of
   a stale workspace until reconciliation publishes a non-stale checkpoint.
   Store large file chunks by content hash so repeated checkpoints do not
   upload unchanged data.
5. **Isolated native restore command.** Add `spawn recover restore` and
   `--verify-only`, the durable operation journal, monotonic fencing epochs,
   recovery-generation CAS, targeted cutoff fold, and tail quarantine. Resolve
   old execution-scoped recovery commands before Resume so none can act on the
   restored Execution. Reuse the existing mount and `Resume` path with a
   post-mount overlay/transcript hook and no-input trust-dialog handling. Add
   the target-scoped continuation
   driver for report collection, acceptance, and applicable gate/land. Keep
   ordinary producers disabled until that continuation and uncertainty
   reconciliation succeed.
6. **Pinned recovery image.** Include the Spawn binary, Claude Code, git,
   tmux, PostgreSQL client tools, relay scripts, and runtime dependencies.
   Start and health-check configured host-local relays and proxies in the
   isolated restore path, without starting ordinary Spawn producers.
   The runtime currently selects one `claude_program`, so the image must pin a
   version compatible with the recorded native Session. Configuration and
   secrets remain external.
7. **Fenced failover.** Add persistent provider power/network/storage/credential
   fencing and database-session termination before automatic restore. Fence or
   adopt a previous replacement before takeover. Until then, recovery is
   operator-initiated and refuses to start when the old host may still be alive.

A CRIU/Docker checkpoint prototype is not part of this path. It can be
evaluated later for process-memory continuity, but it does not replace the
portable event, workspace, transcript, secret, and fencing contracts above.

## 8. Acceptance drill

Pin and publish the reference environment: VM SKU, CPU architecture, disk type,
OS/kernel, container runtime, image digest, Spawn and engine versions, region,
network RTT and bandwidth, and the health of PostgreSQL, R2, model, judge, and
fencing services. Run the reference positive drill on at least 20 new VMs.
Each run starts with no Spawn packages, image cache, configuration, local
recovery objects, or local data. Run every additional positive and negative case at
least three times with its expected outcome.

### Reference setup

1. Run a cheap ephemeral worker with no source repository.
2. Produce a completed turn containing a normal source file, an ignored
   non-cache file, a file over the checkpoint cap, a pending `.spawn/` report,
   and a native transcript long enough to exercise incremental upload. Use a
   declared production-scale global event count, workspace size, and transcript
   size. Include one pending new report and one older-than-delivery report.
3. Publish at least two consecutive checkpoints and retain the final manifest.
4. In an isolated drill bucket or storage proxy, permit reads only for the
   final manifest and its enumerated closure. Deny and log every request for an
   earlier manifest, unlisted chunk, or omitted bundle base.
5. Power off the source VM without a graceful Spawn stop and start
   `failover_ms`.
6. Start an empty replacement VM and restore the final checkpoint with the old
   host absent and all earlier unlisted objects inaccessible. The restore
   protocol establishes the persistent fence inside `restore_ms`.

### Additional positive matrix

A route or mission type is not covered until its own drill passes:

- default and cheap model profiles, including relay and judge continuation;
- configured host-local model/judge relays and outbound proxies start from the
  pinned image and restored environment, pass health checks before Resume, and
  never expose their credentials in the journal or process arguments;
- an old host left alive and, separately, an old host isolated by a network
  partition: persistent fencing succeeds, restore proceeds, and the old host's
  tool, network, storage, and database writes are synchronously rejected;
- empty-prompt Resume with valid settings/credentials but no workspace-trust
  entry in `~/.claude.json`, proving the no-input trust-dialog path reaches
  Ready;
- a pre-first-task resident, scratch mission, remote-source mission, and
  local-directory-source mission;
- a private authenticated Git remote that completes a real fetch;
- a separate local-directory-source gated mission that completes a real landing
  push; remote-source gated landing is outside this proposal because the
  current integrator does not support it;
- pending and marker-less merges, an unannounced oversized file, local engine
  settings, and a durable schedule;
- a scheduled/background write attempted during publication, which is either
  included in the same cutoff or aborts that publication;
- two independently deployed mission/global-gate controllers at different
  commits, with either controller switching during publication; publication
  waits or retries without mixing gate, scenario, backlog, or dependency trees;
- a workspace linked to dependency hash A after the controller has advanced to
  hash B, proving that restore recreates the A link and exact A content;
- a transcript over 4 KiB whose early byte changes while length and final 4 KiB
  stay fixed, producing a new copy that restores the modified bytes;
- a rewritten shorter copy with an unread suffix, proving per-copy cursors and
  source-event-id ingestion before the old `execution.ended`;
- a separate unread suffix below 16 MiB whose unique native events are absent
  from the manifest cutoff and are ingested exactly once before the old final
  checkpoint and `execution.ended`;
- a gated objective task that completes the scoped report, accept, gate, and
  land continuation;
- a queued command for another Session and an unclaimed target Input, report,
  and pending merge, all unchanged until scoped continuation and handoff finish;
- old Execution Inputs interrupted after `waiting`, after `typing`, and after
  `typed` but before `command.finished`: the first fails untyped exactly once,
  the second fails `input_outcome_unknown` and blocks Resume pending
  reconciliation, and the third finishes succeeded without being typed again;
  none can block the operation-scoped continuation after its required outcome
  and reconciliation;
- a cutoff containing `typing` without `typed`, followed in the database tail
  by the same old-Execution command's `typed`: the correlated completion is
  journaled, the accepted cutoff advances, and the Input finishes succeeded
  without another submission; an unrelated execution-affecting tail still
  refuses restore;
- pending old-Execution recovery Input, Resume, handoff Stop, and handoff Start
  commands finish `superseded_by_session_restore` exactly once and never type,
  stop, resume, or start the restored Execution; an old recovery command that
  already started an Execution instead follows the exact-match adopt-or-fence
  path;
- rotation of database/engine/relay credentials after publication, restoring
  with currently authorized service credentials while the retained encryption
  key version still decrypts the old checkpoint. Deletion of that required key
  version must be denied.

### Measure and verify

1. Record `bootstrap_ms`, `restore_ms`, `failover_ms`, `total_ms`, scheduling
   gaps, `continuation_ms`, per-stage timings, and input sizes in a
   machine-readable result. `restore_ms` includes the first durable-operation
   and ownership action.
2. Require `restore_ms <= 10000` for every reference run; report failures
   rather than hiding them behind an average. Detection and promotion remain
   visible in `failover_ms`. Enforce an explicit `continuation_timeout_ms`.
3. Assert identical `AgentId`, `SessionId`, and native session id, and a new
   `ExecutionId`. Only one concurrent restore may start an engine. Assert no
   object read outside the final manifest closure.
4. Before `execution.started` or engine launch, assert transcript-copy lengths
   and SHA-256 digests; path, type, mode, and content hash for the git/overlay
   union; original mtimes for overlay entries; and every event, related control
   record, and command transition through the accepted cutoff. Repeat all file
   and transcript assertions after the Resume mount and overlay application.
   After continuation, validate new archive append/copy semantics instead of
   requiring the native file to equal the old checkpoint forever.
5. Assert no duplicate typed input and no second live Execution. List every
   uncertain external effect. Verify an idempotency key and recorded result
   when one exists; otherwise require a recorded human-reconciliation outcome
   and prohibit automatic retry. Do not claim arbitrary effects were exactly
   once.
6. Bind the continuation `turn.stopped`, report revision, and applicable
   gate/land to the operation's unique command. Assert that an old Input with
   no `typing` fails untyped exactly once, `typing` without resolution fails as
   unknown and blocks Resume, and `typed` finishes succeeded without another
   submission. Old recovery commands are superseded or reconciled as required,
   and none runs against the new Execution. All other pre-existing work stays
   unchanged until continuation and handoff finish, and queued work then
   resumes through normal producers.
7. Inspect restored permissions and logs for leaked credentials.

### Refusal cases

Run each pre-activation case at least three times. Assert the stable error shown
below, no engine start, no active target-filesystem change, and removal of
temporary staging. A durable recovery operation may record the refusal.

| Case | Required error |
| --- | --- |
| Old host alive or network-partitioned while persistent fencing cannot be established, verified, or maintained | `old_host_not_fenced` |
| Database behind `event_position` | `database_behind_manifest` |
| Actual catalog, functions, or grants incompatible with the decoder | `incompatible_database` |
| Missing/corrupt git object, overlay chunk, Spawn state, controller file, `config.json`, or prior-closure object | `invalid_recovery_closure` |
| Missing, truncated, corrupted, or replaced transcript copy | `invalid_transcript_archive` |
| A transcript copy repeats a `source_event_id` with a different event payload | `invalid_transcript_archive` |
| More than 16 MiB of unread transcript backlog | `transcript_backlog_exceeded` |
| Post-manifest native binding, execution event, related control record, or target command transition that is neither represented nor explicitly reconciled | `unreconciled_execution_tail` |
| Requested root differs from the recorded canonical root | `workspace_root_mismatch` |
| Selected workspace has a stale marker/ref | `stale_workspace_requires_reconciliation` |
| Unsupported schema, Spawn commit, engine version, or CPU architecture | `unsupported_recovery_version` |
| Missing encryption, engine, relay/upstream, judge, fencing, or private Git credential | `missing_recovery_credential` |

Every clean refusal also asserts that the recovery command lease and row locks
are released and a later non-operation submission is accepted rather than
returning `recovery_in_progress`; it remains queued while producers are
disabled. For invalid closure, incompatible version, missing credential, and
stale-workspace refusals before provider fencing, assert that the old compute,
its credentials, and its database sessions remain untouched.

For the stale case, also verify the staged normal-branch and stale-snapshot
hashes, no Resume or continuation, and no stale-marker/ref removal. Recovery
becomes eligible only after explicit reconciliation publishes a non-stale
checkpoint.

### Interruption, fencing, and idempotency cases

Run each case at least three times and assert its specific invariant:

- Put `typing` without `typed`, and a tool effect without completion, both
  inside the cutoff and in the unread suffix. Record the uncertainty and stop
  before Resume until the specified evidence or human reconciliation resolves
  it. Never repeat the input or effect blindly.
- Interrupt workspace checkpoint and every publication phase. With no
  unrepresented execution-affecting tail, restore the previous complete
  checkpoint and start one engine. With such a tail, return
  `unreconciled_execution_tail`, start nothing, and change no target filesystem.
- Run retention after chunk upload, after verification, and before manifest CAS.
  The committed closure remains restorable. Clean a crashed publisher's pin only
  after its lease expires. Verify that the recovery writer cannot overwrite or
  delete a locked object and that the existing fixed-age bundle lifecycle does
  not match `recovery/` keys.
- Interrupt the restorer at every journal and destructive phase, including after
  `execution.ended`, `execution.started`, and engine launch. Retry adopts or
  fences the prior compute and continues the same operation without duplicate
  input, Execution, event, or post-activation workspace rewrite.
- Pause restorer A after its claim or phase guard, let B take ownership, then
  heal A. A cannot append, launch an engine, or remain an active tool writer;
  B must adopt or fence A's compute before continuing.
- Start A from manifest M1, let A lose ownership, publish M2, and select M2 in
  B while A's M1 Execution is still live. B must not adopt that Execution: it
  fences A and starts at most one M2 Execution. Exact-match adoption instead
  preserves the journaled Execution and binds it to B's new epoch without
  another Resume.
- Repeat a completed restore. Return its original operation and Execution
  without rewriting the active workspace or starting another engine.
- Race concurrent restores and a publisher. At most one recovery generation
  wins and at most one operation starts an engine; losers return the winner's
  result or a generation conflict.
- Submit target Input/Resume and related control writes at every restore phase.
  Return `recovery_in_progress` for non-operation writes and never dispatch them
  ahead of the designated Resume or scoped continuation.
- Replace or truncate the native transcript after initial verification but
  during Resume preparation. Return `invalid_transcript_archive`, with no
  `execution.started`, engine launch, or complete-restore report. The earlier
  destination changes may remain for the durable retry.
- Force the old host to power on or resume after the new Resume while provider
  isolation remains active. Its tool, network, storage, and database writes are
  rejected synchronously. Separately inject actual fencing loss: report
  `fencing_lost`, stop new input/engine, and reconcile uncertain effects without
  claiming that no overlap occurred.

The feature is not called complete until the applicable positive drills and
these failure cases pass without manual edits to the restored Session.
