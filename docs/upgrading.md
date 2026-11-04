# Upgrading a running Spawn

The loop is meant to run without stopping. This document says which parts can
be replaced while it runs and which need the engine to stop.

## Processes

| Process | Started by | Lifetime | In-memory state |
| --- | --- | --- | --- |
| engine | `spawn run` in a tmux pane | one execution | native session context |
| runner | Bridge `launch` | parent of the engine; writes its exit file | none |
| Bridge | `spawn tick` or `spawn loop` | one tick or the loop | session map, log cursor, transcript read offsets, last screen reading |
| Observer | same process as the Bridge | same | log cursor, per-session tracking |
| dispatcher and liveness rule | `spawn loop` | the loop | none: idempotency keys and digest markers are in the commands they submitted |
| integrator | `spawn integrate` | the process | none |
| UI server | `spawn serve` | the process | none |

## Hot: replace and restart

These hold no state that is not rebuildable.

- **Bridge and Observer.** `recover()` replays `spawn_events` and rebuilds the
  cursor and the session map. A running execution keeps running in its tmux
  pane while the Bridge is down; the next tick reads its transcript from the
  start (event keys make a second read a no-op) and its exit file, if the
  engine ended meanwhile.
- **Dispatcher and liveness rule.** They decide from the log on every pass;
  their idempotency keys derive from the facts, so a restarted loop does not
  submit the same command twice.
- **Integrator.** Its state is the control stream of the event log (task and
  mission rows are folds of it). It records a verdict before it moves git, so
  a killed integrator resumes from the recorded state.
- **`spawn hook`.** A no-op that reads and discards standard input. Spawn
  configures no hooks; an engine whose settings name it calls it by absolute
  path, and its events come from the transcript.
- **UI server.** Read-only. No writes exist in `spawn serve`.

## Cold: needs the execution to end

- **The engine.** A process does not change its binary. Killing it ends the
  execution; `CommandAction::Resume` starts it again with the recorded native
  session id. The transcript survives, the turn in flight does not.
- **The runner.** It is the engine's parent process. Killing it kills the
  engine.

## Version-coupled boundaries

These are not about process lifetime. They break when two versions meet.

1. **`EventKind` is a closed enum.** A new event name written by an upgraded
   Bridge or Observer, read by an older one, produces `StoreError::Corrupt`.
   The next tick exits 1. Every process that reads the log must therefore
   carry the same enum.
2. **The schema.** `migrate()` runs `create or replace` on every connect. An
   additive column or view is safe for an older reader. A removed or renamed
   column is not. `spawn loop` holds one connection, so a migrated schema takes
   effect on its next connection, not on the running one.
3. **The SQL functions.** `spawn_append`, `spawn_submit`, `spawn_claim` and
   `spawn_finish` keep the ordering guarantees. A running transaction sees one
   version of a function, so replacing them mid-transaction is safe; a change
   to their meaning is not.
4. **The runner's files.** A running runner is the binary it was started
   with. It writes the exit code to `SPAWN_EXIT_FILE` (`.spawn/exit-<execution>`
   in the workspace) and, for Codex, a copy of the output to
   `SPAWN_ENGINE_OUTPUT`; the Bridge reads both, so their paths and format
   must stay the same across versions. An execution with no exit file is
   ended as `Lost` once its pane is gone.
5. **The Claude transcript.** Its format is the engine's. `claude_line` skips
   lines it does not know, so an engine release that adds line types causes
   no failure, but a renamed field silently drops the events it carried.
6. **The prompt contract.** Delivery writes the task prompt and the
   `.spawn/report.md` convention. The engine reads them as plain text. A
   change here is hot for the dispatcher and cold for the engine only in that
   the running turn keeps the old instruction.

## What the loop may improve by itself

- Integrator and dispatcher logic.
- The Observer, its judge and its thresholds.
- The UI and the documentation.

## What a candidate must not change

A candidate that changes any of these cannot be judged while the loop runs,
because the change breaks the reader that judges it:

- `EventKind` variants (adding one is safe only once every reader carries
  it).
- Destructive changes to `crates/spawn-postgres/sql/schema.sql`.
- The screen-reader markers the installed engine prints. Extending the
  marker list is safe; replacing it is not.

`scripts/rsi/gate.sh` builds the base and the candidate in separate trees and
runs every check on the candidate, so a candidate that fails to build or
breaks a scenario is rejected before `main` moves. The gate has no check
aimed at the boundaries above: the e2e and black-box scenarios catch a break
only where they exercise it.

## Deploying

`deploy/deploy.sh` replaces the binary of the live fleet on this host: a
backup, a release build, the binary swapped by rename (`spawn.prev` kept),
then, with `spawn-integrate` stopped, the schema migrated and the missions
repository's `main` moved forward, and finally `spawn-loop`,
`spawn-integrate` and `spawn-serve` restarted. Details in
[deploy/systemd/README.md](../deploy/systemd/README.md#deploying).
