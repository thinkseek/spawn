# Upgrading a running Spawn

The loop is meant to run without stopping. This document says which parts can
be replaced while it runs and which need the engine to stop.

## Processes

| Process | Started by | Lifetime | In-memory state |
| --- | --- | --- | --- |
| engine | `spawn run` in a tmux pane | one execution | native session context |
| runner | Bridge `launch` | parent of the engine | none |
| Bridge | `spawn tick` or `spawn loop` | one tick or the loop | session map, log cursor |
| Observer | same process as the Bridge | same | log cursor, per-session tracking |
| controller | `spawn evolve` | one step | none |
| hook | the engine, on every event | milliseconds | none |
| UI server | `spawn serve` | the process | none |

## Hot: replace and restart

These hold no state that is not rebuildable.

- **Bridge and Observer.** `recover()` replays `spawn_events` and rebuilds the
  cursor and the session map. A running execution keeps running in its tmux
  pane while the Bridge is down; the next tick folds the events that piled up.
- **Controller.** All of its state is in `rsi_state`, `rsi_iterations` and
  `rsi_attempts`. The advisory lock belongs to the connection, so a killed
  controller never blocks the next one.
- **Hook.** Each hook event starts a new process. The settings passed with
  `--settings` hold an absolute path, so replacing the binary changes the hook
  of an already running engine at its next event.
- **UI server.** Read-only. No writes exist in `spawn serve`.

## Cold: needs the execution to end

- **The engine.** A process does not change its binary. Killing it ends the
  execution; `CommandAction::Resume` starts it again with the recorded native
  session id. The transcript survives, the turn in flight does not.
- **The runner.** It is the engine's parent process. Killing it kills the
  engine.

## Version-coupled boundaries

These are not about process lifetime. They break when two versions meet.

1. **`EventKind` is a closed enum.** An upgraded hook that writes a new event
   name, read by an older Bridge, produces `StoreError::Corrupt`. The next tick
   exits 1. The hook and the Bridge must therefore carry the same enum. Today
   the hook writes only kinds that already exist.
2. **The schema.** `migrate()` runs `create or replace` on every connect. An
   additive column or view is safe for an older reader. A removed or renamed
   column is not. The resident loop holds one connection, so a new schema takes
   effect on its next connection, not on the running one.
3. **The SQL functions.** `spawn_append`, `spawn_submit`, `spawn_claim` and
   `spawn_finish` keep the ordering guarantees. A running transaction sees one
   version of a function, so replacing them mid-transaction is safe; a change
   to their meaning is not.
4. **The prompt contract.** The controller writes the task prompt and the
   `.spawn/done` marker convention. The engine reads them as plain text. A
   change here is hot for the controller and cold for the engine only in that
   the running turn keeps the old instruction.

## What the loop may improve by itself

- Controller logic and the gate scripts.
- Scenarios and backlog entries.
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

`scripts/rsi/judge-candidate.sh` builds both the baseline and the candidate
into separate trees, so a candidate that fails to build, or that changes a
coupled boundary, is rejected before the baseline moves.
