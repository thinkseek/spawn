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
| integrator | `spawn integrate` | the process | none |
| UI server | `spawn serve` | the process | none |

## Hot: replace and restart

These hold no state that is not rebuildable.

- **Bridge and Observer.** `recover()` replays `spawn_events` and rebuilds the
  cursor and the session map. A running execution keeps running in its tmux
  pane while the Bridge is down; the next tick reads its transcript from the
  start (event keys make a second read a no-op) and its exit file, if the
  engine ended meanwhile.
- **Integrator.** Its state is the control stream of the event log (task and
  mission rows are folds of it). It records a verdict before it moves git, so
  a killed integrator resumes from the recorded state.
- **`spawn hook`.** A no-op. Engines started by a build that still installed
  hooks call it by absolute path on every event; the replaced binary accepts
  and ignores the payload, and their events come from the transcript.
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
   column is not. The resident loop holds one connection, so a new schema takes
   effect on its next connection, not on the running one.
3. **The SQL functions.** `spawn_append`, `spawn_submit`, `spawn_claim` and
   `spawn_finish` keep the ordering guarantees. A running transaction sees one
   version of a function, so replacing them mid-transaction is safe; a change
   to their meaning is not.
4. **The runner's files.** A running runner is the binary it was started
   with. It writes the exit code to `SPAWN_EXIT_FILE` (`.spawn/exit-<execution>`
   in the workspace) and, for Codex, a copy of the output to
   `SPAWN_ENGINE_OUTPUT`; the Bridge reads both, so their paths and format
   must stay the same across versions. An engine started before the transcript
   reader has no exit file (its runner wrote through the retired
   `spawn_terminal` login); the Bridge ends it as `Lost` once its pane is gone.
5. **The Claude transcript.** Its format is the engine's. `claude_line` skips
   lines it does not know, so a new engine release adds no failure, but a
   renamed field silently drops the events it carried.
6. **The prompt contract.** Delivery writes the task prompt and the
   `.spawn/report.md` convention. The engine reads them as plain text. A
   change here is hot for the dispatcher and cold for the engine only in that
   the running turn keeps the old instruction.

## What the loop may improve by itself

- Integrator and dispatcher logic.
- The Observer, its judge and its thresholds.
- The UI and the documentation.

## What the gate must reject

A candidate that changes any of these cannot be judged while the loop runs,
because the change breaks the reader that judges it:

- `EventKind` variants (adding one is safe only after every writer is
  upgraded).
- Destructive changes to `crates/spawn-postgres/sql/schema.sql`.
- The screen-reader markers, when the installed engine version still prints
  the old ones. Extending the marker list is safe; replacing it is not.

`scripts/rsi/gate.sh` (formerly `judge-candidate.sh`) builds both the baseline and the candidate
into separate trees, so a candidate that fails to build, or that changes a
coupled boundary, is rejected before the baseline moves.
