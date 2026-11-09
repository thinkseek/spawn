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
| model relay | `spawn-model-relay` (systemd) | the process | none: each request is relayed and forgotten; streams in flight |

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
- **UI server.** Read-only. No writes exist in `spawn serve`.
- **Model relay.** `scripts/model-relay.py` keeps nothing between requests.
  Install the new script over the old one
  (`install -m 755 scripts/model-relay.py ~/.local/share/spawn-rsi/bin/`)
  and `systemctl --user restart spawn-model-relay`. The restart cuts the
  model calls in flight: a running engine sees an API error for those turns
  and Claude Code retries them; engines that start while it is down run the
  default profile on Claude Code's own settings, and cheap starts fail and
  reach the supervisor (docs/fleet-architecture.md §3.3). Restart it when
  the engines are idle if a cut turn matters. Setting or changing `SPAWN_MODEL_RELAY`,
  `SPAWN_CHEAP_MODEL` or `SPAWN_SMALL_MODEL` takes a `spawn-loop` restart and
  applies to engines started after it.

## Cold: needs the execution to end

- **The engine.** A process does not change its binary. Killing it ends the
  execution; `CommandAction::Resume` starts it again with the recorded native
  session id. The transcript survives, the turn in flight does not.
- **The runner.** It is the engine's parent process. Killing it kills the
  engine.
- **The engine's models.** An engine keeps the base URL and model slots it
  started with. A new profile, a relay that came back, or new model names
  reach it only through a stop and a resume (delivery does this when a task
  needs another profile).

## Version-coupled boundaries

These are not about process lifetime. They break when two versions meet.

1. **`EventKind` is a closed enum.** An event name written by one version of the
   Bridge or Observer and read by a version without it produces `StoreError::Corrupt`.
   The next tick exits 1. Every process that reads the log must therefore
   carry the same enum. A writer is different: a record whose `source` a
   reader does not know (a source since removed, such as `layerfs` or
   `codex`) is skipped with one warning per name, and the timeline shows it
   as stored, so removing a writer stops no reader. An `engine` field is
   text, read whatever it names. A field added to a variant is optional
   (`serde(default)`, left out when unset), so a record from before it
   reads, and a reader from before it ignores it: `session.native_started`
   gained `engine_version` this way.
2. **The schema.** `migrate()` runs `create or replace` on every connect. An
   additive column or view is safe for a reader built before it. A removed or renamed
   column is not. `spawn loop` holds one connection, so a migrated schema takes
   effect on its next connection, not on the running one.
3. **The SQL functions.** `spawn_append`, `spawn_submit`, `spawn_claim` and
   `spawn_finish` keep the ordering guarantees. A running transaction sees one
   version of a function, so replacing them mid-transaction is safe; a change
   to their meaning is not. `spawn_append` refuses an `execution.started`
   while the session's latest execution has no Bridge-written end
   (`StoreError::Rejected`): a Bridge that handles the refusal fails that
   start; one built before it would fail the whole tick, but it never
   starts a second execution of a session it knows runs one, so it meets
   the refusal only when two Bridges race.
4. **The runner's files.** A running runner is the binary it was started
   with. It writes the exit code to `SPAWN_EXIT_FILE` (`.spawn/exit-<execution>`
   in the workspace); the Bridge reads it, so its path and format must stay
   the same across versions. An execution with no exit file is
   ended as `Lost` once no runner the Bridge launched runs it: its pane is
   gone, dead, or does not hold the launch (issue #15). The tmux host tells
   its own launch by the pane's start command, which names the launch's
   environment file (`spawn-launch-<terminal>-...`); every launch since
   6f677b4 (2026-11-05) has one. A runner launched by an older binary that
   is still running when this one is deployed carries no such name and is
   ended as lost (its terminal killed, its final checkpoint taken) at the
   first tick: stop such executions before the deploy, or let the normal
   paths resume them (the liveness rule for the supervisor and residents,
   the dispatcher's one resume of a lost worker).
5. **The Claude transcript.** Its format is the engine's. `claude_line` skips
   lines it does not know, so an engine release that adds line types causes
   no failure, but a renamed field silently drops the events it carried.
6. **Commands with a model profile.** `CommandAction` refuses unknown
   fields, so a binary from before `model` fails a `start` or `resume` that
   names one (delivery always does). `execution.started` carries `model` only
   for the cheap profile, and readers ignore fields they do not know.
7. **Hooks.** Spawn configures none (the Bridge's inline `--settings` has no
   `hooks`) and has no `spawn hook` command. An engine whose own settings
   still name `spawn hook` gets exit status 1 from it, which Claude Code
   reports as a non-blocking hook error; its events still come from the
   transcript.
8. **The prompt contract.** Delivery writes the task prompt and the
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

`deploy/deploy.sh <verdict-file>` replaces the binary of the live fleet on
this host. It refuses unless the verdict file is the gate's, accepted, for
exactly the checked-out HEAD (`"candidate"`) over the missions repository's
current `main` (`"base"`); the gate records both commits in every verdict.
Then: a backup (the password reaches `pg_dump` through `PGPASSWORD`), a
release build, the binary swapped by rename (`spawn.prev` kept), then, with
`spawn-integrate` stopped and `main` checked again, the schema migrated and
`main` moved forward, and finally `spawn-loop`, `spawn-integrate` and
`spawn-serve` restarted. A failure after the integrator stopped restarts the
services before the script exits. Details in
[deploy/systemd/README.md](../deploy/systemd/README.md#deploying).

Rolling back to `spawn.prev` is safe only until the deployed binary writes an
event kind the previous one lacks (boundary 1 above): from then on the
previous binary cannot decode the log. For example, a binary whose
`EventKind` lacks `execution.end_stalled`, `notice.typed` or
`notice.received` halts on the first one in the log (and one whose
`CommandAction` lacks `discard` fails that command); roll forward instead.
