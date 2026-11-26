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
9. **Herdr and agy** (issue #91 P2 PR 2). The Herdr host speaks protocol 22
   (herdr 0.9.5) and refuses a server that speaks another, naming both. A
   herdr upgrade is therefore cold: stop the executions on it, upgrade herdr,
   then deploy a Spawn that speaks its protocol. The server Spawn starts reads
   no update and no new agent manifest (`[update] version_check = false`,
   `manifest_check = false`). The rules it reads agy's screen by stay at the
   ones checked (2026.10.05.1) until the owner moves them, together with the
   tests in `crates/spawn-host/tests/herdr.rs`. agy's transcript format is
   agy's, as Claude Code's is Claude Code's (boundary 5). `agy_line` skips
   what it does not know, and it reads `transcript_full.jsonl`; a renamed
   field drops the events it carried. Nothing of this runs until
   `SPAWN_ENGINES` names `agy`, which is unset (`claude`) by default. Landing
   the change enables nothing.

   The `[update]` pin holds only for a server Spawn started itself (Herdr PR
   3, carry list N8). So every server, whoever started it, is also checked
   against the agent detection manifests in `MANIFESTS`
   (`crates/spawn-host/src/herdr.rs`): agy `2026.10.05.1` and claude
   `2026.10.11.1`. A server whose active version of either differs, or that
   has none, is refused before anything starts, with a message naming the
   pinned and the actual version. To move a pin:
   1. run `herdr server update-agent-manifests`;
   2. re-run the fixtures and `SPAWN_REQUIRE_HERDR=1 cargo test -p spawn-host
      --test herdr`;
   3. edit `MANIFESTS`.

   `GET /api/capabilities` shows the running server's protocol and active
   manifests beside the pinned ones, and herdr's version.
10. **`model_label` on `start`** (Herdr PR 4). `CommandAction::Start` has an
    optional `model_label`, left out of the JSON when absent, so a Claude
    Code start is byte for byte what it was. A Bridge built before it
    refuses a command that carries it (`deny_unknown_fields`). Delivery
    writes one only for a task on an agy preset, and the dispatcher and the
    Bridge are one binary, so the exposure is a rollback with such a start
    queued. **Before rolling back** to a Spawn without `model_label`, drain
    the queued starts that carry it: let them finish, or cancel their tasks
    (`select id from spawn_commands where state <> 'finished' and action ?
    'model_label'` lists them). The Master Control wake
    (`spawn_human_master_message`) copies `model_label` from the session's
    newest start too, so the same rule holds for it.
11. **agy updates itself.** agy's version is shown in `GET
    /api/capabilities`. A `major.minor` Spawn was not checked against
    (`AGY_TESTED`, `crates/spawn-core/src/driver.rs`) raises the notice
    `agy_version_untested`, once per version, and refuses nothing: a screen
    Spawn cannot read reads `Unknown`, which types nothing. After checking
    its screens and transcript, add its `major.minor` to `AGY_TESTED`.

12. **The filesystem view of agy** (H5, decision C2 N6;
    docs/fleet-architecture.md §3.9). From this version an engine with no
    deny rules of its own (agy) starts only inside a `bwrap` view
    (`SPAWN_ENGINE_VIEW=required`, the default). A host that enables agy
    needs `bwrap` (bubblewrap 0.9.0 was checked) on the loop's `PATH`;
    without a working one every agy start is refused (`engine_unavailable:`
    in the start's outcome) and Claude Code runs on. `SPAWN_ENGINE_VIEW=off`
    restores the old behaviour, on the owner's explicit choice. Inside the
    view `~/.gemini` holds only agy's home (`SPAWN_AGY_HOME`): anything agy
    reads elsewhere below `~/.gemini` is gone for it. agy's `TMPDIR` is
    `<agy home>/.spawn-tmp`, not the loop's (`/var/tmp/spawn` is masked).
    A `SPAWN_AGY_HOME` that is, or holds, a masked path (`~/.gemini`
    itself, `~/.claude`, `/run/keep`, …) refuses every agy start. The view is resolved
    at each launch, so an agy already running keeps the view it started
    with; and a masked path missing at its launch that appears later is
    visible to it (`--ro-bind / /` is live) until its next launch. A
    rollback to a Spawn without the view simply starts agy without one.

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

A deploy that enables agy (`SPAWN_ENGINES=claude,agy`) also needs, in the
loop's environment, `SPAWN_TERMINAL_BACKEND=herdr` and `SPAWN_AGY_MODEL`
(optionally `SPAWN_AGY_PROGRAM`, `SPAWN_AGY_HOME`), and on the host herdr
0.9.5 with its antigravity-cli integration, a logged-in agy and a working
`bwrap` on the loop's `PATH` (agy runs only inside its filesystem view,
`SPAWN_ENGINE_VIEW=required` by default; item 12 above). Without them,
an agy start is refused before launch, saying what is missing. Claude Code
is unaffected. The loop probes the engines at its start and hourly
(`<program> --version`, `agy models`; bounded, no shell, login files only
`stat`ed; an engine `SPAWN_ENGINES` does not name is never run): an agy
whose probe fails, or a preset's or `SPAWN_AGY_MODEL`'s
label that `agy models` does not list, makes no start and raises
`engine_unavailable`, while Claude Code keeps running. `GET
/api/capabilities` shows what the probe found.

`deploy/deploy.sh <verdict-file>` replaces the binary of the live fleet on
this host. It refuses unless the verdict file is the gate's, accepted, for
exactly the checked-out HEAD (`"candidate"`) over the missions repository's
current `main` (`"base"`); the gate records both commits in every verdict.
Then: a backup (the password reaches `pg_dump` through `PGPASSWORD`), a
release build, the binary swapped by rename (`spawn.prev` kept), then, with
`spawn-integrate` stopped and `main` checked again, the schema migrated and
`main` moved forward, and finally `spawn-loop`, `spawn-integrate` and
`spawn-serve` restarted. A failure after the integrator stopped restarts the
services before the script exits. With `SPAWN_DEPLOY_MIRROR` set, a deploy
that succeeded then publishes that `main` to the mirror (a plain push, never
forced; a push that fails is said and is not the deploy's failure; a mirror
whose URL carries a credential is refused before anything is done: the
credential goes in a credential helper or a deploy key). Details in
[deploy/systemd/README.md](../deploy/systemd/README.md#deploying).

Rolling back to `spawn.prev` is safe only until the deployed binary writes an
event kind the previous one lacks (boundary 1 above): from then on the
previous binary cannot decode the log. For example, a binary whose
`EventKind` lacks `execution.end_stalled`, `notice.typed` or
`notice.received` halts on the first one in the log, as one that lacks
`message.typed` or `message.taken` (objective 045) does (and one whose
`CommandAction` lacks `discard` fails that command); roll forward instead.
Rolling back from a binary whose gates run in units (gate v2 step 2) to one
that runs them as children: stop the gates first (`systemctl --user stop
'spawn-gate-*'`) and remove `$SPAWN_WORKSPACE_ROOT/judgings/`, with
`spawn-integrate` stopped. The older integrator adopts no unit: it judges
the same tasks again beside them, and their judgings keep their controllers
from ever being removed.
