# Upgrading a running Spawn

The loop is meant to run without stopping. This document says which parts can
be replaced while it runs and which need the engine to stop.

## Processes

| Process | Started by | Lifetime | In-memory state |
| --- | --- | --- | --- |
| engine | `spawn run` in a Herdr terminal (`spawn-hc` / legacy `herdr`) | one execution | native session context |
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
  cursor and the session map. A running execution keeps running in its
  detached Herdr (`spawn-hc` / `herdr`) terminal while the Bridge is down; the next tick reads its transcript from the
  start (event keys make a second read a no-op) and its exit file, if the
  engine ended meanwhile.
- **Dispatcher and liveness rule.** They decide from the log on every pass;
  their idempotency keys derive from the facts, so a restarted loop does not
  submit the same command twice.
- **Integrator.** Its state is the control stream of the event log (task and
  mission rows are folds of it). It records a verdict before it moves git, so
  a killed integrator resumes from the recorded state.
- **UI server.** Holds no durable in-memory state (`spawn serve`, or split
  `spawn-core` + `spawn-node-edge`). Human writes execute directly as
  idempotent SQL calls (`spawn_human_*`), and SSE streams resume cleanly via
  `Last-Event-ID`.
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
   ended as `Lost` once no runner the Bridge launched runs it: its terminal
   is gone, dead, or does not hold the launch (issue #15). `HerdrCoreHost` and
   `HerdrHost` verify their own launch via the daemon's terminal list,
   private host owner records, and the launch environment file name
   (`spawn-launch-<terminal>-...`; historically introduced alongside the
   legacy launcher since `6f677b4` on 2026-11-05). A runner launched before
   launch-file attribution carries no such name and is ended as lost (its
   terminal killed, its final checkpoint taken) at the first tick: stop such
   executions before the deploy, or let the normal paths resume them (the
   liveness rule for the supervisor and residents, the dispatcher's one resume
   of a lost worker).
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
9. **Herdr (`spawn-hc` / legacy `herdr`) and agy** (issue #91 P2 PR 2, single-Herdr PR #150).
   The default `SPAWN_HERDR_MODE=core` drives the in-repo embedded
   `herdr-core` daemon through sibling `spawn-hc` (expected OpenRPC generation
   `EXPECTED_GENERATION = 1`, `crates/spawn-host/src/herdr_core.rs`) and
   performs agent detection out of sandbox on the host (`herdr_witness`). Because
   `spawn-hc serve` / `spawn-hc hold` run in their own session independently
   of `spawn-loop`, restarting `spawn-loop` keeps running Agent PTYs alive;
   upgrading `spawn-hc` across incompatible generations requires stopping
   active executions first. External legacy `herdr 0.9.5` (opt-in via
   `SPAWN_HERDR_MODE=legacy`) speaks protocol 22 and refuses a server speaking
   another, naming both (`[update] version_check = false`,
   `manifest_check = false` for servers Spawn starts). The rules reading agy's
   screen stay at `2026.10.05.1` alongside `spawn-host`'s `herdr_core` /
   `herdr` tests. agy's transcript format is agy's, as Claude Code's is Claude
   Code's (boundary 5). `agy_line` skips what it does not know, and reads
   `transcript_full.jsonl`; a renamed field drops the events it carried.
   Nothing of this runs until `SPAWN_ENGINES` names `agy`, which is unset
   (`claude`) by default.

   On `SPAWN_HERDR_MODE=legacy`, every server is also checked against the
   pinned agent detection manifests in `MANIFESTS`
   (`crates/spawn-host/src/herdr.rs`): agy `2026.10.05.1` and claude
   `2026.10.11.1`. A server whose active version differs, or has none, is
   refused before anything starts. To move a legacy pin:
   1. run `herdr server update-agent-manifests`;
   2. re-run the fixtures and `spawn-host` `herdr` tests;
   3. edit `MANIFESTS`.

   `GET /api/capabilities` shows the active Herdr engine (`herdr-core` or legacy
   `herdr`), binary version, expected generation or protocol/manifests, and
   shard count (`SPAWN_HERDR_SHARDS`).
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

12. **The filesystem view of engines (`BwrapView` / `ContainerView`)** (H5,
    decision C2 N6, PR #126; docs/fleet-architecture.md §3.9). Engines without
    deny rules of their own (agy) start inside a `bwrap` view
    (`SPAWN_ENGINE_VIEW=required`, the default; Claude Code can also opt in
    with `SPAWN_CLAUDE_SANDBOX=1`). A bare host that enables agy needs a working
    `bwrap` (bubblewrap 0.9.0 was checked) on the loop's `PATH`; without one,
    every agy start is refused (`engine_unavailable:` in the start's outcome)
    while Claude Code runs on. Inside container runtimes, `SPAWN_ENGINE_VIEW=container`
    uses `ContainerView` (failing closed on bare hosts and never falling back
    to `ContainerView` when the `bwrap` binary is missing). Safe read-only
    host toolchain inheritance defaults to `auto` (`SPAWN_SANDBOX_INHERIT_HOST=auto`,
    or `[sandbox].inherit_host = "auto"` in `spawn.toml`, binding standard
    toolchains read-only while refusing secret paths; `off` disables).
    `SPAWN_ENGINE_VIEW=off` disables the view only on the owner's explicit
    choice. Inside `BwrapView`, `~/.gemini` holds only agy's home
    (`SPAWN_AGY_HOME`), agy's `TMPDIR` is `<agy home>/.spawn-tmp`, and masked
    paths (`~/.gemini` outside `SPAWN_AGY_HOME`, `~/.claude`, `/run/keep`, …)
    are refused if configured as writable or engine home. The view is resolved
    at each launch, so a running execution keeps its view until its next
    launch. A rollback to a Spawn without the view simply starts agy without
    one.

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

### Single-Herdr migration (`tmux` removal)

`tmux` support has been removed (PR #150); `herdr` is now the sole terminal
backend:

- Setting `SPAWN_TERMINAL_BACKEND` to `"tmux"` (env) or `[terminal].backend = "tmux"`
  in `spawn.toml` is **explicitly refused fail-closed** at startup (unset/empty
  or `"herdr"` is required).
- **Pre-upgrade steps**:
  1. Remove any legacy `tmux` configuration (`SPAWN_TERMINAL_BACKEND`,
     `SPAWN_TMUX_PROGRAM`, `SPAWN_TMUX_SOCKET`, `[terminal].backend = "tmux"`),
     and stop any pre-upgrade tmux-hosted executions cleanly before restarting
     `spawn-loop`.
  2. Ensure sibling `spawn-hc` is installed next to `spawn` (`cargo install --path crates/spawn-hc`;
     `deploy/deploy.sh` and `rsi/scenarios/build` build/install both `spawn` and `spawn-hc`).
  3. Default runtime uses embedded `herdr-core` via `spawn-hc` (`SPAWN_HERDR_MODE=core`,
     `SPAWN_HERDR_SHARDS=1..=64`); set `SPAWN_HERDR_MODE=legacy` (with
     `SPAWN_HERDR_PROGRAM=herdr`, protocol 22) only if an explicit external
     `herdr` fallback is needed.

### Phase 2 opt-in UDS + Go front cutover (`SPAWN_DEPLOY_EDGE_MODE=go-uds`) & rollback

`deploy/deploy.sh` defaults to `SPAWN_DEPLOY_EDGE_MODE=legacy` and keeps single-process
`spawn serve` / `spawn-node-gateway` / `spawn-jev-relay` unless opt-in `go-uds`
is requested (PR #159; see [deploy/systemd/README.md](../deploy/systemd/README.md#deploying)):

1. **Read-only preflight check (zero host mutation, zero secret echo)**:
   ```sh
   bash deploy/deploy.sh --check-phase2-cutover [<units-dir>]
   ```
   Verifies executable `spawn-node` (`~/.local/share/spawn-rsi/bin/spawn-node`),
   all four regular unit files (`spawn-core.service`, `spawn-node-edge.service`,
   `spawn-node-gw.service`, `spawn-node-out.service`), zero `spawn.env` /
   `*DATABASE_URL*` references in the 3 Go units and their drop-ins, and
   `LoadCredential=typesafe.api-key:/run/keep/typesafe.api-key` +
   `RestartPreventExitStatus=243 78` on `spawn-node-out.service`.
2. **Opt-in cutover**:
   ```sh
   SPAWN_DEPLOY_EDGE_MODE=go-uds bash deploy/deploy.sh <verdict-file>   # or --main / --landed
   ```
   Runs preflight first, applies units, starts `spawn-core.service`, and waits up to
   `SPAWN_PHASE2_CORE_SOCK_TIMEOUT_SECS` (default `15`, valid `1..=300`) for
   `${XDG_RUNTIME_DIR}/spawn/core.sock` (`-S`). If `core.sock` is not ready in
   time, it **aborts fail-closed without stopping any legacy unit**. Once ready,
   it stops legacy `spawn-serve`, `spawn-node-gateway`, `spawn-jev-relay.path`,
   and `spawn-jev-relay`, starts `spawn-node-{edge,gw,out}`, restarts
   `spawn-loop` / `spawn-integrate`, and verifies all 4 Phase 2 units are active.
3. **Post-cutover verification**:
   `systemctl --user status spawn-core spawn-node-edge spawn-node-gw spawn-node-out`,
   `curl -fsS --unix-socket "${XDG_RUNTIME_DIR}/spawn/core.sock" http://localhost/ipc/v1/hello`,
   and `scripts/keep/credential-health.sh`.
4. **One-step reversible rollback (`go-uds` -> `legacy`)**:
   ```sh
   SPAWN_DEPLOY_EDGE_MODE=legacy bash deploy/deploy.sh <verdict-file>   # or --main
   ```
   When `legacy` runs (or `SPAWN_DEPLOY_EDGE_MODE` is omitted), `deploy.sh`
   stops active Phase 2 units **before** restarting legacy `spawn-loop`,
   `spawn-integrate`, `spawn-serve`, `spawn-node-gateway`, and `spawn-jev-relay`
   (preventing `:8787` / `:18096` port conflicts). For an immediate manual
   unit rollback without rebuilding:
   ```sh
   systemctl --user stop spawn-node-edge spawn-node-gw spawn-node-out spawn-core
   systemctl --user restart spawn-loop spawn-integrate spawn-serve spawn-node-gateway spawn-jev-relay.path spawn-jev-relay
   ```

### Opt-in D6 workspace backend (`SPAWN_WORKSPACE_BACKEND=d6-shadow|d6`) & rollback

- **Default & `auto`**: `SPAWN_WORKSPACE_BACKEND` defaults to `git`.
  `SPAWN_WORKSPACE_BACKEND=auto` selects only `cow` (when CoW clones are supported)
  or `git`; it never auto-selects `overlay`, `d6-shadow`, or `d6`.
- **`d6-shadow` (reversible dual-track mode)**: Runs shadow `D6Workspace` verification
  over an authoritative `git` root (`<root>/.d6-shadow/`, plus optional
  `SPAWN_D6_CAS_DIR`, `SPAWN_D6_TMPFS_ROOT`, `SPAWN_D6_TMPFS_BUDGET_MB=2048`,
  `SPAWN_D6_STRICT_FAIL_CLOSED=1`). **Rollback to `git`**: unset
  `SPAWN_WORKSPACE_BACKEND` (or set `git`) and restart `spawn-loop` — the `git`
  root marker and branches remain intact.
- **Pure `d6` (`D6Workspace`)**: Requires a dedicated root (`.d6/`) or explicit
  cutover (`claim_root` refuses mixing `d6` with `git|cow|overlay|d6-shadow`
  fail-closed). **Rollback**: switch `SPAWN_WORKSPACE_ROOT` back to the `git`
  root or execute the reverse cutover documented in [d6-cas-v1.md](./d6-cas-v1.md).

A deploy that enables agy (`SPAWN_ENGINES=claude,agy`) also needs, in the
loop's environment, `SPAWN_AGY_MODEL` (Herdr is default via sibling `spawn-hc`,
or legacy `herdr 0.9.5` under `SPAWN_HERDR_MODE=legacy`; optionally set
`SPAWN_TERMINAL_BACKEND=herdr`, `SPAWN_AGY_PROGRAM`, `SPAWN_AGY_HOME`), and a
working `bwrap` (or `SPAWN_ENGINE_VIEW=container` inside a container runtime;
item 12 above), a logged-in agy, and Host/Herdr detection (`herdr_witness` for
`herdr-core`, or legacy `herdr 0.9.5` integration). Without them,
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

## AI-era smooth self-upgrade (`deploy/self-upgrade.sh` & external watchdog)

When promoting prebuilt immutable releases (`$SPAWN_RSI_DIR/releases/<tag>/`,
holding `spawn`, optional `spawn-node-gateway` / `spawn-hc` / `spawn-node`,
`web/dist/` or `web.tar.gz`, and `SHA256SUMS`), upgrades follow a four-layer
zero-downtime protocol:

1. **Immutable release & fail-closed verification (canary-ready)**:
   `deploy/self-upgrade.sh --role <control-plane|worker-node> --version <tag>`
   acquires `$SPAWN_RSI_DIR/self-upgrade.lock` (closing the lock FD before
   spawning hooks or units), validates strict multi-path `SHA256SUMS` coverage
   (rejecting path traversal, symlinks outside the release, unlisted entries in
   the release root — only `SHA256SUMS`-verified executables are linked into
   `bin/` — missing executable bits, setuid/setgid modes, non-regular/link
   `web.tar.gz` members, and pre-existing unlisted `web/` alongside `web.tar.gz`),
   and extracts `web.tar.gz` using `--no-same-owner --no-same-permissions` +
   `chmod u-s,g-s,u+rwX,go+rX,go-w` into `$SPAWN_RSI_DIR/web-extracted/<tag>`
   **before** touching `current`, `previous`, `bin/`, or `web`. Only currently
   active systemd units are targeted, owner-stopped units
   (`ActiveState=inactive`, `SubState=dead`, `Result=success`) are never started
   on either upgrade or rollback, and `systemctl restart --job-mode=ignore-requirements`
   is used so `Wants=` relationships (such as `spawn-integrate.service`
   declaring `Wants=spawn-loop.service`) never pull up an owner-stopped peer
   unit; zero active units on upgrade fail closed unless
   `--allow-no-active-units` is passed (which records `staged (0 active units;
   no units restarted or verified)` instead of `verified healthy`). On a first
   upgrade from a legacy non-symlink live layout (`bin/*` regular executables
   and `web/` directory without `current`/`previous`), the live binaries and
   `web/` are atomically snapshotted into `releases/pre-upgrade-<utc-ts>/`
   (with generated `SHA256SUMS`) before switching so rollback preserves every
   legacy binary and UI asset. URLs (`scheme://...`), `Authorization:` /
   `Cookie:` / `Set-Cookie:` headers, `Bearer`/`Basic` credentials,
   JSON/key-value/CLI-flag secrets (`password`, `passwd`, `passphrase`, `token`,
   `secret`, `api[_-]?key`, `access[_-]?token`, `refresh[_-]?token`,
   `private[_-]?key`, `client[_-]?secret`, `session`, `key`), and bare `sk-...`
   tokens are redacted across `upgrade-state.json`, `upgrade-audit.jsonl`,
   script stdout/stderr, and captured backup-hook, drain, and `init-db` output.
2. **DB-blind stateless Canary / remote node (`--role worker-node --db-blind`)**:
   Remote/edge node units that never run schema migrations (such as
   `spawn-node.service`) should upgrade first with `--db-blind`. Before touching
   any symlink, `--db-blind` inspects each target unit's `ExecStartPre` (via
   `systemctl show`, fragment unit files, and drop-ins) and fails closed if any
   unit invokes `init-db`.
3. **Control-plane (`--role control-plane`, 1C/1G)**:
   Runs any configured pre-upgrade backup hook first, writes sanitized metadata
   to `upgrade-state.json` (`upgrading` -> `active`), atomically flips symlinks
   with same-filesystem temporary links (`mv -Tf`), executes `spawn init-db`,
   restarts `spawn-node-gateway` then `spawn-serve`, and probes `ActiveState=active`,
   `SubState=running`, `NRestarts == 0`, plus HTTP readiness (`/` on serve; non-5xx
   / signed protocol codes on the gateway). Any failure triggers immediate atomic
   rollback (`current -> previous`) and unit recovery (exit `3`).
4. **Fleet-loop worker host (`--role worker-node`, Epoch Drain + `spawn init-db` via `spawn-loop.service`)**:
   Because `deploy/systemd/spawn-loop.service` configures
   `ExecStartPre=%h/.local/share/spawn-rsi/bin/spawn init-db`, restarting
   `spawn-loop` executes `spawn init-db` with the new binary. Fleet-loop worker
   hosts are **not** DB-blind and therefore roll out *after* control-plane R2
   backup and schema migration (or run their own `--pre-backup-cmd` /
   `--backup-unit`). Before switching symlinks, if `spawn-integrate.service` is
   active, `self-upgrade.sh` runs `spawn integrate --drain` so in-flight gates
   land cleanly; it **never** signals or kills `spawn-hc` process trees
   (`KillMode=process`; `Bridge::recover()` adopts live Agent turns across loop
   restarts), and enforces a post-restart stability window
   (`--worker-settle-secs`, default `5`) to catch crash-looping binaries before
   recording `status = "active"`.
5. **Independent external watchdog (`deploy/upgrade-watchdog.sh`)**:
   Installed standalone outside `current/` into `~/.local/share/spawn-rsi/ops/`
   alongside `spawn-upgrade-watchdog.{service,timer}` (`deploy/sync-units.sh`
   only updates already-installed units) and never executing `bin/spawn`:
   ```sh
   install -d -m 755 ~/.local/share/spawn-rsi/ops
   install -m 755 deploy/self-upgrade.sh deploy/upgrade-watchdog.sh ~/.local/share/spawn-rsi/ops/
   install -m 644 deploy/systemd/spawn-upgrade-watchdog.service deploy/systemd/spawn-upgrade-watchdog.timer ~/.config/systemd/user/
   systemctl --user daemon-reload
   systemctl --user enable --now spawn-upgrade-watchdog.timer
   ```
   While `status == "active"` within `watch_window_until`, each 30 s tick
   verifies `bin/` and `web` symlinks still point to the `self-upgrade`-managed
   release; if `deploy/deploy.sh install_build` or another deploy replaced them,
   the watchdog transitions state to `superseded` and exits `0` without
   overwriting `bin/` or `web/`. Owner-stopped units (`ActiveState=inactive`,
   `SubState=dead`, `Result=success`) are excluded from probe failures and are
   never started during rollback (if an owner `systemctl stop` exceeds
   `TimeoutStopSec` and terminates with `Result=timeout`, the watchdog treats
   it fail-safe as non-clean rather than a clean stop). Once consecutive probe
   failures for the remaining active/failed units reach `--failure-threshold`
   (default `3`), it executes `current -> previous`, restarts only
   non-cleanly-stopped units using `--job-mode=ignore-requirements`, appends a
   redacted entry to `upgrade-audit.jsonl`, and sets `status = "rolled_back"`
   to debounce against rollback loops.

### Expand-Contract forward and backward compatibility (`N-1 <-> N`)

Safe automated rollback between releases `N-1` and `N` requires every schema,
event-log, and node-protocol change to obey Expand-Contract across adjacent
versions (see also [Version-coupled boundaries](#version-coupled-boundaries)):

- **Expand first (`N` understands `N-1`)**: new SQL columns/tables are additive
  with defaults or nullable types; new JSON/serde fields use `#[serde(default)]`;
  new HTTP/IPC fields are ignored by `N-1` or gated behind explicit capability
  negotiation; both `N` and `N-1` tolerate all `EventKind` variants emitted
  during the watch window before irreversible cleanup.
- **Contract later (`N+1` or later)**: column drops, constraint tightening, or
  removing legacy wire paths happen only after release `N` is established fleet-wide
  beyond the rollback window.

